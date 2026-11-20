package Plugins::Spotty::ProtocolHandler;

use strict;
use warnings;

use base qw(Slim::Formats::RemoteStream);

use Plugins::Spotty::SpottyBridge;

sub contentType { 'spt' }
sub canSeek { 1 }
sub canTranscodeSeek { 1 }

sub getMetadataFor {
	my ($class, $client, $url) = @_;
	return Plugins::Spotty::SpottyBridge->call('protocol.metadata', {
		client => ($client ? $client->id : undef),
		url    => $url,
	});
}

sub explodePlaylist {
	my ($class, $client, $url, $cb) = @_;
	my $res = Plugins::Spotty::SpottyBridge->call('protocol.explode_playlist', {
		client => ($client ? $client->id : undef),
		url    => $url,
	});
	return $cb ? $cb->($res) : $res;
}

sub trackGain {
	my ($class, $client, $url) = @_;
	return Plugins::Spotty::SpottyBridge->call('protocol.track_gain', {
		client => ($client ? $client->id : undef),
		url    => $url,
	});
}

1;
