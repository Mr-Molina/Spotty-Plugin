package Plugins::Spotty::Plugin;

use strict;
use warnings;

use Config;
use File::Basename qw(dirname);
use File::Spec;

BEGIN {
	eval {
		require Slim::Plugin::OPMLBased;
		our @ISA = qw(Slim::Plugin::OPMLBased);
		1;
	};
	eval { require Plugins::Spotty::SpottyBridge; 1 }
		or require File::Spec->catfile(dirname(File::Spec->rel2abs(__FILE__)), 'SpottyBridge.pm');
}

sub initPlugin {
	my ($class, @args) = @_;

	my $helper_bin = eval { Plugins::Spotty::SpottyBridge->resolve_binary() };
	Plugins::Spotty::SpottyBridge->call('plugin.init', {
		arch       => $Config{archname} || '',
		os         => $^O,
		(defined $helper_bin ? (helper_bin => $helper_bin) : ()),
	});

	if (eval { require Slim::Web::Pages; 1 }) {
		for my $mod (
			'Plugins::Spotty::Settings',
			'Plugins::Spotty::Settings::Auth',
			'Plugins::Spotty::Settings::Player',
			'Plugins::Spotty::Settings::PlayerAuth',
			'Plugins::Spotty::Settings::PlaylistFolders',
		) {
			if (eval "require $mod; 1") {
				$mod->new() if $mod->can('new');
			}
		}
	}

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

	my $menu = (ref($params) eq 'HASH' && defined $params->{menu}) ? $params->{menu}
		: (ref($args) eq 'HASH' && defined $args->{menu}) ? $args->{menu}
		: undef;
	my $query = (ref($params) eq 'HASH' && defined $params->{search}) ? $params->{search}
		: (ref($params) eq 'HASH' && defined $params->{query}) ? $params->{query}
		: (ref($args) eq 'HASH' && defined $args->{search}) ? $args->{search}
		: undef;
	my $account = (ref($params) eq 'HASH' && defined $params->{account}) ? $params->{account}
		: (ref($args) eq 'HASH' && defined $args->{account}) ? $args->{account}
		: undef;

	Plugins::Spotty::SpottyBridge->call_async('opml.feed', {
		(defined $menu    ? (menu    => $menu)    : ()),
		(defined $query   ? (query   => $query)   : ()),
		(defined $account ? (account => $account) : ()),
	}, sub {
		my ($res, $err) = @_;
		return $cb->($res || { items => [] });
	});
}

1;
