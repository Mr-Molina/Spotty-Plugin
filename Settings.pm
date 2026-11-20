package Plugins::Spotty::Settings;

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
		or require File::Spec->catfile(dirname(File::Spec->rel2abs(__FILE__)), 'SpottyBridge.pm');
}

sub name { 'PLUGIN_SPOTTY' }

sub page { 'plugins/Spotty/settings/basic.html' }

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

sub _apply_outcome {
	my ($paramRef, $res, $response) = @_;
	return $paramRef unless ref($res) eq 'HASH';
	my $outcome = $res->{outcome};
	return $paramRef unless ref($outcome) eq 'HASH';

	my $kind = $outcome->{kind} || '';
	if (($kind eq 'redirect_auth' || $kind eq 'redirect_player_auth') && $outcome->{location}) {
		if ($response && $response->can('code') && $response->can('header')) {
			$response->code(302);
			$response->header('Location' => $outcome->{location});
		}
		$paramRef->{redirectLocation} = $outcome->{location};
		return $paramRef;
	}

	my $vm = $outcome->{view_model};
	if (ref($vm) eq 'HASH') {
		for my $k (keys %$vm) {
			$paramRef->{$k} = $vm->{$k};
		}
	}
	return $paramRef;
}

sub handler {
	my ($class, $client, $paramRef, $callback, $httpClient, $response) = @_;
	my $payload = { params => _scalar_params($paramRef) };

	if ($callback) {
		return Plugins::Spotty::SpottyBridge->call_async('settings.basic', $payload, sub {
			my ($res, $err) = @_;
			_apply_outcome($paramRef, $res, $response);
			return $callback->($client, $paramRef, $httpClient, $response);
		});
	}

	my $res = Plugins::Spotty::SpottyBridge->call('settings.basic', $payload);
	return _apply_outcome($paramRef, $res, $response);
}

1;
