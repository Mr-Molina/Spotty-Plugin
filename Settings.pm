package Plugins::Spotty::Settings;

use strict;
use warnings;

use base qw(Slim::Web::Settings);

use Plugins::Spotty::SpottyBridge;

sub name { 'PLUGIN_SPOTTY' }

sub page { 'plugins/Spotty/settings/basic.html' }

sub handler {
	my ($class, $client, $paramRef, $callback, $httpClient, $response) = @_;
	my $res = Plugins::Spotty::SpottyBridge->call('settings.basic', {
		client => ($client ? $client->id : undef),
		params => $paramRef,
	});
	return $callback ? $callback->($client, $res || $paramRef, $httpClient, $response) : $res;
}

1;
