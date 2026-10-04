#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

set -euo pipefail

case ${1:-} in
    --prepare)
        perl -MDigest::SHA -MFile::Path -MIPC::Open2 -MTime::HiRes -e 1
        test -x /usr/bin/tr
        ;;
    --run)
        exec perl - <<'PERL'
use strict;
use warnings;
use Digest::SHA qw(sha256_hex);
use File::Path qw(make_path remove_tree);
use IPC::Open2 qw(open2);
use Time::HiRes qw(CLOCK_MONOTONIC clock_gettime time);

my $root = ($ENV{E2E_TMPDIR} // "/tmp") . "/hermit-perl-interpreter-batch";
remove_tree($root);
make_path($root);

my $payload = "alpha\nbeta\ngamma\n";
my $input_path = "$root/input.txt";
my $output_path = "$root/output.txt";
open my $input_file, ">", $input_path or die "open $input_path: $!";
print {$input_file} $payload;
close $input_file or die "close $input_path: $!";

open my $saved_input, "<", $input_path or die "open $input_path: $!";
local $/;
my $input = <$saved_input>;
close $saved_input or die "close $input_path: $!";

my $pid = open2(my $child_output, my $child_input, "/usr/bin/tr", "a-z", "A-Z");
print {$child_input} $input;
close $child_input or die "close child stdin: $!";
my $converted = <$child_output>;
close $child_output or die "close child stdout: $!";
waitpid($pid, 0);
die "tr failed: $?" if $? != 0;

open my $output_file, ">", $output_path or die "open $output_path: $!";
print {$output_file} $converted;
close $output_file or die "close $output_path: $!";
open my $observed_file, "<", $output_path or die "open $output_path: $!";
my $observed = <$observed_file>;
close $observed_file or die "close $output_path: $!";

my $version = sprintf("%vd", $^V);
my $wall_ns = int(time() * 1_000_000_000);
my $monotonic_ns = int(clock_gettime(CLOCK_MONOTONIC) * 1_000_000_000);
printf "PERL version=%s bytes=%d sha256=%s child=0 wall_ns=%d monotonic_ns=%d\n",
    $version, length($observed), sha256_hex($observed), $wall_ns, $monotonic_ns;
PERL
        ;;
    *)
        echo "usage: $0 --prepare|--run" >&2
        exit 2
        ;;
esac
