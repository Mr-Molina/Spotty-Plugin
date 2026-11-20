package Plugins::Spotty::Importer;

use strict;
use warnings;

use File::Basename qw(dirname);
use File::Spec;

BEGIN {
	eval {
		require Slim::Plugin::OnlineLibraryBase;
		our @ISA = qw(Slim::Plugin::OnlineLibraryBase);
		1;
	};
	eval { require Plugins::Spotty::SpottyBridge; 1 }
		or require File::Spec->catfile(dirname(File::Spec->rel2abs(__FILE__)), 'SpottyBridge.pm');
}

sub initPlugin {
	my ($class, @args) = @_;
	return $class->SUPER::initPlugin(@args);
}

sub startScan {
	my ($class, @args) = @_;
	return Plugins::Spotty::SpottyBridge->run_scan(@args);
}

sub needsUpdate {
	my ($class, $cb) = @_;
	if ($cb) {
		return Plugins::Spotty::SpottyBridge->call_async('importer.needs_update', {}, sub {
			my ($res, $err) = @_;
			my $needs = ref($res) eq 'HASH' ? ($res->{needs_update} ? 1 : 0) : ($res ? 1 : 0);
			return $cb->($needs);
		});
	}
	my $res = Plugins::Spotty::SpottyBridge->call('importer.needs_update', {});
	return ref($res) eq 'HASH' ? ($res->{needs_update} ? 1 : 0) : ($res ? 1 : 0);
}

1;
