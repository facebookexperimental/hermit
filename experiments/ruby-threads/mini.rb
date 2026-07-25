# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

n = (ARGV[0] || 2).to_i
ts = (0...n).map { |i| Thread.new { $stdout.puts "thread #{i}" } }
ts.each(&:join)
$stdout.puts "main done"
