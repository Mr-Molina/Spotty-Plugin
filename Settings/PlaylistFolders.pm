package Plugins::Spotty::Settings::PlaylistFolders;

use strict;
use warnings;

use File::Basename qw(basename dirname);
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

sub new {
	my ($class) = @_;
	if (eval { require Slim::Web::Pages; 1 }) {
		Slim::Web::Pages->addPageFunction($class->page, $class);
		Slim::Web::Pages->addRawFunction('plugins/spotty/uploadPlaylistFolderData', \&uploadPlaylistFolderData);
	}
}

sub name {
	return eval { require Slim::Web::HTTP::CSRF; 1 }
		? Slim::Web::HTTP::CSRF->protectName('PLUGIN_SPOTTY')
		: 'PLUGIN_SPOTTY';
}

sub page {
	return eval { require Slim::Web::HTTP::CSRF; 1 }
		? Slim::Web::HTTP::CSRF->protectURI('plugins/Spotty/settings/playlist-folders.html')
		: 'plugins/Spotty/settings/playlist-folders.html';
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

sub uploadPlaylistFolderData {
	my ($httpClient, $response, $params, $dir) = @_;
	my $res = eval {
		Plugins::Spotty::SpottyBridge->call('settings.playlist_folders', {
			action => 'load',
			params => _scalar_params($params),
		});
	};
	if ($response && $response->can('code')) {
		$response->code(302);
		$response->header('Location' => '/plugins/Spotty/settings/playlist-folders.html');
	}
	return ref($res) eq 'HASH' ? ($res->{view_model} || {}) : {};
}

sub handler {
	my ($class, $client, $paramRef, $callback, $httpClient, $response) = @_;
	my $action = (ref($paramRef) eq 'HASH' && $paramRef->{saveSettings}) ? 'delete' : 'load';
	my $payload = {
		action => $action,
		params => _scalar_params($paramRef),
	};

	my $apply_vm = sub {
		my ($res) = @_;
		if (ref($res) eq 'HASH' && ref($res->{view_model}) eq 'HASH') {
			my $vm = $res->{view_model};
			for my $k (keys %$vm) {
				$paramRef->{$k} = $vm->{$k};
			}
		}
	};

	if ($callback) {
		return Plugins::Spotty::SpottyBridge->call_async('settings.playlist_folders', $payload, sub {
			my ($res, $err) = @_;
			$apply_vm->($res);
			return $callback->($client, $paramRef, $httpClient, $response);
		});
	}

	my $res = Plugins::Spotty::SpottyBridge->call('settings.playlist_folders', $payload);
	$apply_vm->($res);
	return $paramRef;
}

1;
