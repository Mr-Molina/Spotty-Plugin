package Plugins::Spotty::Settings::Auth;

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
	eval { require Plugins::Spotty::Settings::Callback; 1 }
		or require File::Spec->catfile(dirname(File::Spec->rel2abs(__FILE__)), 'Callback.pm');
}

sub new {
	my ($class) = @_;
	if (eval { require Slim::Web::Pages; 1 }) {
		Slim::Web::Pages->addPageFunction($class->page, $class);
		Slim::Web::Pages->addRawFunction('plugins/Spotty/settings/hasCredentials', \&checkCredentials);
	}
	Plugins::Spotty::Settings::Callback->init();
}

sub name {
	return eval { require Slim::Web::HTTP::CSRF; 1 }
		? Slim::Web::HTTP::CSRF->protectName('PLUGIN_SPOTTY')
		: 'PLUGIN_SPOTTY';
}

sub page {
	return eval { require Slim::Web::HTTP::CSRF; 1 }
		? Slim::Web::HTTP::CSRF->protectURI('plugins/Spotty/settings/authentication.html')
		: 'plugins/Spotty/settings/authentication.html';
}

sub checkCredentials {
	my ($httpClient, $response) = @_;
	my $res = eval {
		Plugins::Spotty::SpottyBridge->call('settings.auth_check', { promote => JSON::PP::true });
	};
	my $has_creds = (ref($res) eq 'HASH' && ($res->{promoted} || (ref($res->{status}) eq 'HASH' && $res->{status}{has_credentials})))
		? JSON::PP::true
		: JSON::PP::false;
	if ($response && $response->can('header')) {
		$response->header('Content-Type' => 'application/json');
	}
	return encode_json({ hasCredentials => $has_creds });
}

sub handler {
	my ($class, $client, $paramRef, $callback, $httpClient, $response) = @_;
	my $redirect_uri = Plugins::Spotty::Settings::Callback->getRedirectUri();
	my $ua = ($httpClient && $httpClient->can('request') && $httpClient->request)
		? ($httpClient->request->header('User-Agent') || 'LyrionMusicServer')
		: 'LyrionMusicServer';

	my $prep = eval {
		Plugins::Spotty::SpottyBridge->call('settings.oauth_prepare', {
			lms_redirect_uri => $redirect_uri,
			user_agent       => "$ua",
		});
	};
	if (ref($prep) eq 'HASH' && $prep->{authorize_url}) {
		$paramRef->{authUrl}     = $prep->{authorize_url};
		$paramRef->{callbackUrl} = $redirect_uri;
	}

	if ($callback) {
		return $callback->($client, $paramRef, $httpClient, $response);
	}
	return $class->can('SUPER::handler')
		? $class->SUPER::handler($client, $paramRef, $callback, $httpClient, $response)
		: $paramRef;
}

1;
