package Plugins::Spotty::SpottyBridge;

use strict;
use warnings;

use Config;
use File::Basename qw(dirname);
use File::Spec;
use IO::Socket::INET;
use JSON::PP qw(decode_json encode_json);
use Time::HiRes qw(sleep);

my $NEXT_ID = 0;

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

sub _connect {
	my ($addr) = @_;
	my ($host, $port) = $addr =~ m{\A([^:]+):(\d+)\z} or return undef;
	return IO::Socket::INET->new(
		PeerAddr => $host,
		PeerPort => $port,
		Proto    => 'tcp',
		Timeout  => 2,
	);
}

sub _spawn_daemon {
	my ($class, $cache_dir) = @_;
	my $bin = $class->resolve_binary();
	my @cmd = ($bin, 'daemon', '--cache-dir', $cache_dir);

	if ($^O eq 'MSWin32') {
		system(1, @cmd);
	} else {
		my $pid = fork();
		die "fork failed: $!" unless defined $pid;
		if ($pid == 0) {
			exec @cmd or POSIX::_exit(127);
		}
	}
}

sub ensure_daemon {
	my ($class, $cache_dir) = @_;
	$cache_dir ||= default_cache_dir();

	if (my $ep = $class->read_endpoint($cache_dir)) {
		if (my $sock = _connect($ep->{addr})) {
			return ($ep, $sock);
		}
	}

	$class->_spawn_daemon($cache_dir);

	for (1 .. 100) {
		if (my $ep = $class->read_endpoint($cache_dir)) {
			if (my $sock = _connect($ep->{addr})) {
				return ($ep, $sock);
			}
		}
		sleep(0.05);
	}

	die "Timed out waiting for lms-spotty daemon.endpoint at " . $class->endpoint_path($cache_dir) . "\n";
}

sub call {
	my ($class, $method, $params, %opts) = @_;
	my ($ep, $sock) = $class->ensure_daemon($opts{cache_dir});

	my $req = {
		jsonrpc => '2.0',
		id      => ++$NEXT_ID,
		token   => $ep->{token},
		method  => $method,
		params  => $params || {},
	};

	print {$sock} encode_json($req) . "\n";
	$sock->flush();

	my $line = <$sock>;
	close $sock;

	die "Empty JSON-RPC response for $method\n" unless defined $line && length $line;
	my $resp = decode_json($line);
	if (ref($resp) eq 'HASH' && $resp->{error}) {
		my $msg = ref($resp->{error}) eq 'HASH' ? ($resp->{error}{message} || 'RPC error') : $resp->{error};
		die "lms-spotty RPC $method failed: $msg\n";
	}
	return ref($resp) eq 'HASH' ? $resp->{result} : $resp;
}

sub run_scan {
	my ($class, @args) = @_;
	my $bin = $class->resolve_binary();
	return system($bin, 'scan', @args) == 0 ? 1 : 0;
}

1;
