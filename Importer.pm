package Plugins::Spotty::Importer;

use strict;
use warnings;

BEGIN {
	eval {
		require Slim::Plugin::OnlineLibraryBase;
		our @ISA = qw(Slim::Plugin::OnlineLibraryBase);
	};
}

use Plugins::Spotty::SpottyBridge;

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
	my $res = Plugins::Spotty::SpottyBridge->call('importer.needs_update', {});
	return $cb ? $cb->($res) : $res;
}

1;
