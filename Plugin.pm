package Plugins::Spotty::Plugin;

use strict;
use warnings;

use base qw(Slim::Plugin::OPMLBased);

use Plugins::Spotty::SpottyBridge;

sub initPlugin {
	my ($class, @args) = @_;
	Plugins::Spotty::SpottyBridge->call('plugin.init', { args => \@args });
	return $class->SUPER::initPlugin(
		feed   => \&handleFeed,
		tag    => 'spotty',
		menu   => 'music_services',
		weight => 35,
		is_app => 1,
	);
}

sub getDisplayName { 'PLUGIN_SPOTTY' }

sub handleFeed {
	my ($client, $cb, $args, $params) = @_;
	my $res = Plugins::Spotty::SpottyBridge->call('opml.feed', {
		client => ($client ? $client->id : undef),
		args   => $args,
		params => $params,
	});
	return $cb->($res);
}

1;
