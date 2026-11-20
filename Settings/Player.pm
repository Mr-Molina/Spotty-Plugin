package Plugins::Spotty::Settings::Player;

use strict;
use warnings;

use File::Basename qw(dirname);
use File::Spec;

BEGIN {
	eval {
		require Slim::Web::Settings;
		our @ISA = qw(Slim::Web::Settings);
		1;
	};
	eval { require Plugins::Spotty::SpottyBridge; 1 }
		or require File::Spec->catfile(dirname(dirname(File::Spec->rel2abs(__FILE__))), 'SpottyBridge.pm');
}

sub name {
	return eval { require Slim::Web::HTTP::CSRF; 1 }
		? Slim::Web::HTTP::CSRF->protectName('PLUGIN_SPOTTY')
		: 'PLUGIN_SPOTTY';
}

sub needsClient { 1 }

sub page {
	return eval { require Slim::Web::HTTP::CSRF; 1 }
		? Slim::Web::HTTP::CSRF->protectURI('plugins/Spotty/settings/player.html')
		: 'plugins/Spotty/settings/player.html';
}

sub _scalar_params {
	my ($paramRef) = @_;
	my %clean;
	return \%clean unless ref($paramRef) eq 'HASH';
	for my $k (keys %$paramRef) {
		my $v = $paramRef->{$k};
		next unless defined $v && !ref($v);
		$clean{$k} = "$v";
	}
	return \%clean;
}

sub handler {
	my ($class, $client, $params, $callback, $httpClient, $response) = @_;
	my $action = (ref($params) eq 'HASH' && $params->{saveSettings}) ? 'save' : 'load';
	my $payload = {
		player_id => ($client ? $client->id : 'default'),
		action    => $action,
		params    => _scalar_params($params),
	};

	if ($callback) {
		return Plugins::Spotty::SpottyBridge->call_async('settings.player', $payload, sub {
			my ($res, $err) = @_;
			if (ref($res) eq 'HASH') {
				for my $k (keys %$res) {
					$params->{$k} = $res->{$k};
				}
			}
			return $callback->($client, $params, $httpClient, $response);
		});
	}

	my $res = Plugins::Spotty::SpottyBridge->call('settings.player', $payload);
	if (ref($res) eq 'HASH') {
		for my $k (keys %$res) {
			$params->{$k} = $res->{$k};
		}
	}
	return $params;
}

1;
