package Plugins::Spotty::Settings::Callback;

use strict;
use warnings;

use File::Basename qw(dirname);
use File::Spec;

BEGIN {
	eval { require Plugins::Spotty::SpottyBridge; 1 }
		or require File::Spec->catfile(dirname(dirname(File::Spec->rel2abs(__FILE__))), 'SpottyBridge.pm');
}

use constant CALLBACK_PATH => 'plugins/Spotty/settings/callback';
use constant REDIRECT_PATH => 'plugins/Spotty/settings/redirect';

sub init {
	if (eval { require Slim::Web::Pages; 1 }) {
		my $redir = eval { require Slim::Web::HTTP::CSRF; 1 }
			? Slim::Web::HTTP::CSRF->protectURI(REDIRECT_PATH)
			: REDIRECT_PATH;
		Slim::Web::Pages->addPageFunction($redir, \&oauthRedirect);
		Slim::Web::Pages->addPageFunction(CALLBACK_PATH, \&oauthCallback);
	}
}

sub getRedirectUri {
	if (eval { require Slim::Utils::Network; 1 }) {
		return Slim::Utils::Network::serverURL() . '/' . CALLBACK_PATH;
	}
	return 'http://127.0.0.1:9000/' . CALLBACK_PATH;
}

sub oauthRedirect {
	my ($client, $params, $callback, $httpClient, $response) = @_;
	my $redirect_uri = getRedirectUri();
	my $ua = ($httpClient && $httpClient->can('request') && $httpClient->request)
		? ($httpClient->request->header('User-Agent') || 'LyrionMusicServer')
		: 'LyrionMusicServer';

	Plugins::Spotty::SpottyBridge->call_async('settings.oauth_prepare', {
		lms_redirect_uri => $redirect_uri,
		user_agent       => "$ua",
	}, sub {
		my ($res, $err) = @_;
		if (ref($res) eq 'HASH' && $res->{authorize_url} && $response && $response->can('code')) {
			$response->code(302);
			$response->header('Location' => $res->{authorize_url});
		}
		return $callback->($client, $params, $httpClient, $response) if $callback;
	});
}

sub oauthCallback {
	my ($client, $params, $callback, $httpClient, $response) = @_;
	my $code = ref($params) eq 'HASH' ? $params->{code} : undef;
	my $state = ref($params) eq 'HASH' ? $params->{state} : undef;
	my $oauth_error = ref($params) eq 'HASH' ? $params->{error} : undef;
	my $error_desc = ref($params) eq 'HASH' ? $params->{error_description} : undef;

	Plugins::Spotty::SpottyBridge->call_async('settings.oauth_callback', {
		(defined $code ? (code => "$code") : ()),
		(defined $state ? (state => "$state") : ()),
		(defined $oauth_error ? (error => "$oauth_error") : ()),
		(defined $error_desc ? (error_description => "$error_desc") : ()),
	}, sub {
		my ($res, $err) = @_;
		if ($err) {
			$params->{auth_error} = "$err";
		} elsif (ref($res) eq 'HASH') {
			for my $k (keys %$res) {
				$params->{$k} = $res->{$k};
			}
		}
		if ($callback) {
			return $callback->($client, $params, $httpClient, $response);
		}
		if (eval { require Slim::Web::HTTP; 1 }) {
			return Slim::Web::HTTP::filltemplatefile('plugins/Spotty/settings/callback.html', $params);
		}
		return $params;
	});
}

1;
