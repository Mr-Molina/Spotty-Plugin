package Plugins::Spotty::SpottyBridge;

use strict;
use warnings;

use Config;
use Errno qw(EAGAIN EWOULDBLOCK EINPROGRESS);
use Fcntl qw(:flock);
use File::Basename qw(dirname);
use File::Path qw(make_path);
use File::Spec;
use IO::Select;
use IO::Socket::INET;
use JSON::PP qw(decode_json encode_json);
use POSIX ();
use Time::HiRes qw(sleep time);

my $NEXT_ID = 0;
my $DEFAULT_TIMEOUT_SECS = 15;

sub plugin_dir {
	return dirname(File::Spec->rel2abs(__FILE__));
}

sub resolve_binary {
	my ($class) = @_;
	my $exe = ($^O eq 'MSWin32') ? 'lms-spotty.exe' : 'lms-spotty';
	my $base = plugin_dir();

	my @candidates = (
		File::Spec->catfile($base, 'Bin', $Config{archname}, $exe),
		File::Spec->catfile($base, 'Bin', $exe),
	);
	for my $path (@candidates) {
		return $path if -f $path && ($^O eq 'MSWin32' || -x $path);
	}

	my $sep = ($^O eq 'MSWin32') ? ';' : ':';
	for my $dir (split /\Q$sep\E/, ($ENV{PATH} || '')) {
		next unless length $dir;
		my $path = File::Spec->catfile($dir, $exe);
		return $path if -f $path && ($^O eq 'MSWin32' || -x $path);
	}

	die "Unable to locate $exe in Bin/$Config{archname}, Bin/, or PATH\n";
}

sub default_cache_dir {
	if (eval { require Slim::Utils::Prefs; 1 }) {
		my $dir = Slim::Utils::Prefs::preferences('server')->get('cachedir');
		return $dir if defined $dir && length $dir;
	}
	return $ENV{LMS_CACHE_DIR} if defined $ENV{LMS_CACHE_DIR} && length $ENV{LMS_CACHE_DIR};
	return File::Spec->tmpdir();
}

sub default_lms_db {
	my ($class, $cache_dir) = @_;
	$cache_dir ||= $class->default_cache_dir();
	if (defined $ENV{LMS_DB_PATH} && length $ENV{LMS_DB_PATH}) {
		return $ENV{LMS_DB_PATH};
	}
	if (eval { require Slim::Utils::Prefs; 1 }) {
		my $dbsource = Slim::Utils::Prefs::preferences('server')->get('dbsource');
		if (defined $dbsource && $dbsource =~ m{dbname=([^;]+)}) {
			return $1;
		}
	}
	return File::Spec->catfile($cache_dir, 'library.db');
}

sub spotty_dir {
	my ($class, $cache_dir) = @_;
	$cache_dir ||= $class->default_cache_dir();
	my $dir = File::Spec->catdir($cache_dir, 'spotty');
	if (!-d $dir) {
		make_path($dir, ($^O eq 'MSWin32' ? () : ({ mode => 0700 })));
	}
	chmod 0700, $dir if $^O ne 'MSWin32' && -d $dir;
	return $dir;
}

sub endpoint_path {
	my ($class, $cache_dir) = @_;
	$cache_dir ||= default_cache_dir();
	return File::Spec->catfile($cache_dir, 'spotty', 'daemon.endpoint');
}

sub read_endpoint {
	my ($class, $cache_dir) = @_;
	my $path = $class->endpoint_path($cache_dir);
	return undef unless -f $path;

	open my $fh, '<:raw', $path or return undef;
	local $/;
	my $raw = <$fh>;
	close $fh;

	my $data = eval { decode_json($raw) };
	return undef unless ref($data) eq 'HASH' && $data->{addr} && $data->{token};
	return $data;
}

sub _unlink_stale_endpoint {
	my ($class, $cache_dir) = @_;
	my $path = $class->endpoint_path($cache_dir);
	unlink $path if -e $path;
}

sub _connect {
	my ($addr, $timeout) = @_;
	$timeout ||= 2;
	my ($host, $port) = $addr =~ m{\A([^:]+):(\d+)\z} or return undef;
	return IO::Socket::INET->new(
		PeerAddr => $host,
		PeerPort => $port,
		Proto    => 'tcp',
		Timeout  => $timeout,
	);
}

sub _spawn_daemon {
	my ($class, $cache_dir) = @_;
	my $spotty_dir = $class->spotty_dir($cache_dir);
	my $lock_path  = File::Spec->catfile($spotty_dir, 'daemon.lock');

	open my $lock_fh, '>', $lock_path or die "Unable to open daemon lock $lock_path: $!\n";
	chmod 0600, $lock_path if $^O ne 'MSWin32';
	flock($lock_fh, LOCK_EX) or die "flock failed on $lock_path: $!\n";

	# Double-check under lock in case another process already started the daemon.
	if (my $ep = $class->read_endpoint($cache_dir)) {
		if (my $sock = _connect($ep->{addr}, 1)) {
			close $sock;
			flock($lock_fh, LOCK_UN);
			close $lock_fh;
			return;
		}
		$class->_unlink_stale_endpoint($cache_dir);
	}

	my $bin = $class->resolve_binary();
	my @cmd = ($bin, 'daemon', '--cache-dir', $cache_dir);

	if ($^O eq 'MSWin32') {
		system(1, @cmd);
	} else {
		my $pid = fork();
		die "fork failed: $!" unless defined $pid;
		if ($pid == 0) {
			POSIX::setsid();
			open STDIN,  '<',  '/dev/null';
			open STDOUT, '>',  '/dev/null';
			open STDERR, '>&', \*STDOUT;

			my $max_fd = eval { POSIX::sysconf(&POSIX::_SC_OPEN_MAX) } || 1024;
			$max_fd = 1024 if $max_fd > 4096;
			for my $fd (3 .. $max_fd - 1) {
				POSIX::close($fd);
			}

			my $grandchild = fork();
			POSIX::_exit(127) unless defined $grandchild;
			if ($grandchild == 0) {
				exec @cmd or POSIX::_exit(127);
			}
			POSIX::_exit(0);
		}
		waitpid($pid, 0);
	}

	flock($lock_fh, LOCK_UN);
	close $lock_fh;
}

sub ensure_daemon {
	my ($class, $cache_dir, %opts) = @_;
	$cache_dir ||= default_cache_dir();

	if (my $ep = $class->read_endpoint($cache_dir)) {
		if (my $sock = _connect($ep->{addr}, 2)) {
			return ($ep, $sock);
		}
		$class->_unlink_stale_endpoint($cache_dir);
	}

	$class->_spawn_daemon($cache_dir);

	my $deadline = time() + ($opts{timeout} || 5);
	while (time() < $deadline) {
		if (my $ep = $class->read_endpoint($cache_dir)) {
			if (my $sock = _connect($ep->{addr}, 1)) {
				return ($ep, $sock);
			}
		}
		sleep(0.05);
	}

	die "Timed out waiting for lms-spotty daemon.endpoint at " . $class->endpoint_path($cache_dir) . "\n";
}

sub ensure_daemon_async {
	my ($class, $cache_dir, $cb, %opts) = @_;
	$cache_dir ||= default_cache_dir();

	if (my $ep = $class->read_endpoint($cache_dir)) {
		if (my $sock = _connect($ep->{addr}, 1)) {
			return $cb->($ep, $sock, undef);
		}
		$class->_unlink_stale_endpoint($cache_dir);
	}

	eval { $class->_spawn_daemon($cache_dir); 1 } or do {
		return $cb->(undef, undef, $@);
	};

	my $deadline = time() + ($opts{timeout} || 5);
	if ($INC{'Slim/Utils/Timers.pm'}) {
		my $poll;
		$poll = sub {
			if (my $ep = $class->read_endpoint($cache_dir)) {
				if (my $sock = _connect($ep->{addr}, 1)) {
					return $cb->($ep, $sock, undef);
				}
			}
			if (time() >= $deadline) {
				return $cb->(undef, undef, "Timed out waiting for lms-spotty daemon.endpoint\n");
			}
			Slim::Utils::Timers::setTimer(undef, Time::HiRes::time() + 0.05, $poll);
		};
		Slim::Utils::Timers::setTimer(undef, Time::HiRes::time() + 0.05, $poll);
		return;
	}

	if ($INC{'AnyEvent.pm'}) {
		my $watcher;
		$watcher = AnyEvent->timer(
			after    => 0.05,
			interval => 0.05,
			cb       => sub {
				if (my $ep = $class->read_endpoint($cache_dir)) {
					if (my $sock = _connect($ep->{addr}, 1)) {
						undef $watcher;
						return $cb->($ep, $sock, undef);
					}
				}
				if (time() >= $deadline) {
					undef $watcher;
					return $cb->(undef, undef, "Timed out waiting for lms-spotty daemon.endpoint\n");
				}
			},
		);
		return;
	}

	my ($ep, $sock) = eval { $class->ensure_daemon($cache_dir, %opts) };
	return $cb->($ep, $sock, $@);
}

sub _build_request {
	my ($method, $params, $token) = @_;
	return encode_json({
		jsonrpc => '2.0',
		id      => ++$NEXT_ID,
		token   => $token,
		method  => $method,
		params  => $params || {},
	}) . "\n";
}

sub _decode_response {
	my ($method, $line) = @_;
	die "Empty JSON-RPC response for $method\n" unless defined $line && length $line;
	my $resp = decode_json($line);
	if (ref($resp) eq 'HASH' && $resp->{error}) {
		my $msg = ref($resp->{error}) eq 'HASH' ? ($resp->{error}{message} || 'RPC error') : $resp->{error};
		die "lms-spotty RPC $method failed: $msg\n";
	}
	return ref($resp) eq 'HASH' ? $resp->{result} : $resp;
}

sub call {
	my ($class, $method, $params, %opts) = @_;
	my $timeout = $opts{timeout} || $DEFAULT_TIMEOUT_SECS;
	my ($ep, $sock) = $class->ensure_daemon($opts{cache_dir}, timeout => $timeout);
	$sock->blocking(0);

	my $payload = _build_request($method, $params, $ep->{token});
	my $sel     = IO::Select->new($sock);
	my $deadline = time() + $timeout;

	my $written = 0;
	my $len     = length($payload);
	while ($written < $len) {
		my $rem = $deadline - time();
		if ($rem <= 0 || !$sel->can_write($rem)) {
			close $sock;
			die "Timed out writing JSON-RPC request for $method\n";
		}
		my $n = syswrite($sock, $payload, $len - $written, $written);
		if (!defined $n) {
			next if $! == EAGAIN || $! == EWOULDBLOCK;
			close $sock;
			die "Socket write failed for $method: $!\n";
		}
		$written += $n;
	}

	my $buf = '';
	while ($buf !~ /\n/) {
		my $rem = $deadline - time();
		if ($rem <= 0 || !$sel->can_read($rem)) {
			close $sock;
			die "Timed out reading JSON-RPC response for $method\n";
		}
		my $chunk = '';
		my $n = sysread($sock, $chunk, 65536);
		if (!defined $n) {
			next if $! == EAGAIN || $! == EWOULDBLOCK;
			close $sock;
			die "Socket read failed for $method: $!\n";
		}
		last if $n == 0;
		$buf .= $chunk;
		if (length($buf) > 4 * 1024 * 1024) {
			close $sock;
			die "JSON-RPC response exceeded maximum frame size for $method\n";
		}
	}

	close $sock;
	$buf =~ s/\r?\n.*\z//s;
	return _decode_response($method, $buf);
}

sub call_async {
	my ($class, $method, $params, $cb, %opts) = @_;
	my $timeout = $opts{timeout} || $DEFAULT_TIMEOUT_SECS;

	if (!$INC{'Slim/Networking/Select.pm'} && !$INC{'AnyEvent.pm'}) {
		my $res = eval { $class->call($method, $params, %opts) };
		return $cb->($res, $@);
	}

	$class->ensure_daemon_async($opts{cache_dir}, sub {
		my ($ep, $sock, $err) = @_;
		if ($err || !$sock) {
			return $cb->(undef, $err || "Failed to connect to lms-spotty daemon\n");
		}

		$sock->blocking(0);
		my $payload = _build_request($method, $params, $ep->{token});
		my $offset  = 0;
		my $buf     = '';
		my $done    = 0;

		my ($cleanup, $finish);
		if ($INC{'Slim/Networking/Select.pm'}) {
			my $timer_ref;
			$cleanup = sub {
				Slim::Networking::Select::removeRead($sock);
				Slim::Networking::Select::removeWrite($sock);
				Slim::Utils::Timers::killTimers(undef, $timer_ref) if $timer_ref && $INC{'Slim/Utils/Timers.pm'};
				close $sock;
			};
			$finish = sub {
				my ($res, $cb_err) = @_;
				return if $done++;
				$cleanup->();
				$cb->($res, $cb_err);
			};

			if ($INC{'Slim/Utils/Timers.pm'}) {
				$timer_ref = sub { $finish->(undef, "JSON-RPC timeout for $method\n") };
				Slim::Utils::Timers::setTimer(undef, Time::HiRes::time() + $timeout, $timer_ref);
			}

			my $on_read = sub {
				my $chunk = '';
				my $n = sysread($sock, $chunk, 65536);
				if (!defined $n) {
					return if $! == EAGAIN || $! == EWOULDBLOCK;
					return $finish->(undef, "Socket read failed for $method: $!\n");
				}
				if ($n == 0 && $buf !~ /\n/) {
					my $res = eval { _decode_response($method, $buf) };
					return $finish->($res, $@);
				}
				$buf .= $chunk;
				if ($buf =~ s/\r?\n.*\z//s) {
					my $res = eval { _decode_response($method, $buf) };
					return $finish->($res, $@);
				}
			};

			Slim::Networking::Select::addWrite($sock, sub {
				my $n = syswrite($sock, $payload, length($payload) - $offset, $offset);
				if (!defined $n) {
					return if $! == EAGAIN || $! == EWOULDBLOCK;
					return $finish->(undef, "Socket write failed for $method: $!\n");
				}
				$offset += $n;
				if ($offset >= length($payload)) {
					Slim::Networking::Select::removeWrite($sock);
					Slim::Networking::Select::addRead($sock, $on_read);
				}
			});
			return;
		}

		# AnyEvent non-blocking path
		my ($io_w, $timer_w);
		$finish = sub {
			my ($res, $cb_err) = @_;
			return if $done++;
			undef $io_w;
			undef $timer_w;
			close $sock;
			$cb->($res, $cb_err);
		};
		$timer_w = AnyEvent->timer(
			after => $timeout,
			cb    => sub { $finish->(undef, "JSON-RPC timeout for $method\n") },
		);
		$io_w = AnyEvent->io(
			fh   => $sock,
			poll => 'w',
			cb   => sub {
				my $n = syswrite($sock, $payload, length($payload) - $offset, $offset);
				if (!defined $n) {
					return if $! == EAGAIN || $! == EWOULDBLOCK;
					return $finish->(undef, "Socket write failed for $method: $!\n");
				}
				$offset += $n;
				if ($offset >= length($payload)) {
					$io_w = AnyEvent->io(
						fh   => $sock,
						poll => 'r',
						cb   => sub {
							my $chunk = '';
							my $rn = sysread($sock, $chunk, 65536);
							if (!defined $rn) {
								return if $! == EAGAIN || $! == EWOULDBLOCK;
								return $finish->(undef, "Socket read failed for $method: $!\n");
							}
							if ($rn == 0 && $buf !~ /\n/) {
								my $res = eval { _decode_response($method, $buf) };
								return $finish->($res, $@);
							}
							$buf .= $chunk;
							if ($buf =~ s/\r?\n.*\z//s) {
								my $res = eval { _decode_response($method, $buf) };
								return $finish->($res, $@);
							}
						},
					);
				}
			},
		);
	}, timeout => $timeout);
}

sub run_scan {
	my ($class, @args) = @_;
	my $bin = $class->resolve_binary();
	my $has_cache = grep { $_ eq '--cache-dir' || /^--cache-dir=/ } @args;
	my $has_db    = grep { $_ eq '--lms-db' || /^--lms-db=/ } @args;

	my $cache_dir = $class->default_cache_dir();
	my @full_args = @args;
	push @full_args, '--cache-dir', $cache_dir unless $has_cache;
	push @full_args, '--lms-db', $class->default_lms_db($cache_dir) unless $has_db;

	return system($bin, 'scan', @full_args) == 0 ? 1 : 0;
}

1;
