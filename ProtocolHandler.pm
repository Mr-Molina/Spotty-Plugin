package Plugins::Spotty::ProtocolHandler;

use strict;
use warnings;

use File::Basename qw(dirname);
use File::Spec;

BEGIN {
	eval {
		require Slim::Formats::RemoteStream;
		our @ISA = qw(Slim::Formats::RemoteStream);
		1;
	};
	eval { require Plugins::Spotty::SpottyBridge; 1 }
		or require File::Spec->catfile(dirname(File::Spec->rel2abs(__FILE__)), 'SpottyBridge.pm');
}

sub contentType { 'spt' }
sub canSeek { 1 }
sub canTranscodeSeek { 1 }

sub getMetadataFor {
	my ($class, $client, $url) = @_;
	my $params = ref($url) eq 'ARRAY'
		? { uris => $url }
		: { uri  => $url };
	return Plugins::Spotty::SpottyBridge->call('protocol.metadata', $params);
}

sub explodePlaylist {
	my ($class, $client, $url, $cb) = @_;
	my $params = { uri => $url };
	if ($cb) {
		return Plugins::Spotty::SpottyBridge->call_async('protocol.explode_playlist', $params, sub {
			my ($res, $err) = @_;
			return $cb->($res || []);
		});
	}
	return Plugins::Spotty::SpottyBridge->call('protocol.explode_playlist', $params);
}

sub trackGain {
	my ($class, $client, $url, $remote_gain) = @_;
	my $server_mode;
	if (eval { require Slim::Utils::Prefs; 1 }) {
		$server_mode = Slim::Utils::Prefs::preferences('server')->get('replayGainMode');
	}
	my $res = Plugins::Spotty::SpottyBridge->call('protocol.track_gain', {
		player_id => ($client ? $client->id : undef),
		(defined $server_mode ? (server_mode => "$server_mode") : ()),
		(defined $remote_gain ? (remote_replay_gain => 0 + $remote_gain) : ()),
	});
	return ref($res) eq 'HASH' ? $res->{gain} : $res;
}

1;
