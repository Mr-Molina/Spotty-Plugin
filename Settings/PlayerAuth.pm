package Plugins::Spotty::Settings::PlayerAuth;

use strict;
use warnings;

use File::Basename qw(dirname);
use File::Spec;
use JSON::PP qw(encode_json);

BEGIN {
	eval {
		require Slim::Web::Settings;
		our @ISA = qw(Slim::Web::Settings);
		1;
	};
	eval { require Plugins::Spotty::SpottyBridge; 1 }
		or require File::Spec->catfile(dirname(dirname(File::Spec->rel2abs(__FILE__))), 'SpottyBridge.pm');
}

sub new {
	my ($class) = @_;
	if (eval { require Slim::Web::Pages; 1 }) {
		Slim::Web::Pages->addPageFunction($class->page, $class);
		Slim::Web::Pages->addRawFunction('plugins/Spotty/settings/hasPlayerCredentials', \&checkCredentials);
	}
}

sub name {
	return eval { require Slim::Web::HTTP::CSRF; 1 }
		? Slim::Web::HTTP::CSRF->protectName('PLUGIN_SPOTTY')
		: 'PLUGIN_SPOTTY';
}

sub page {
	return eval { require Slim::Web::HTTP::CSRF; 1 }
		? Slim::Web::HTTP::CSRF->protectURI('plugins/Spotty/settings/player-auth.html')
		: 'plugins/Spotty/settings/player-auth.html';
}

sub checkCredentials {
	my ($httpClient, $response) = @_;
	my $res = eval {
		Plugins::Spotty::SpottyBridge->call('settings.player_auth_check', { action => 'check' });
	};
	my $has_creds = (ref($res) eq 'HASH' && $res->{has_player_credentials})
		? JSON::PP::true
		: JSON::PP::false;
	if ($response && $response->can('header')) {
		$response->header('Content-Type' => 'application/json');
	}
	return encode_json({ hasCredentials => $has_creds });
}

sub handler {
	my ($class, $client, $paramRef, $callback, $httpClient, $response) = @_;
	my $payload = {
		action      => 'page',
		device_name => ($client && $client->can('name') ? $client->name : 'Spotty'),
	};

	if ($callback) {
		return Plugins::Spotty::SpottyBridge->call_async('settings.player_auth_check', $payload, sub {
			my ($res, $err) = @_;
			if (ref($res) eq 'HASH' && ($res->{outcome} || '') =~ /RedirectBasic/i) {
				if ($response && $response->can('code')) {
					$response->code(302);
					$response->header('Location' => 'basic.html');
				}
			}
			return $callback->($client, $paramRef, $httpClient, $response);
		});
	}

	return Plugins::Spotty::SpottyBridge->call('settings.player_auth_check', $payload);
}

1;
