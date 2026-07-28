#!/usr/bin/env ruby
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

require "digest"
require "etc"
require "set"
require "thread"
require "time"

THREADS = 4
ITERATIONS = 64

def random_probe
  token = Random.bytes(16).unpack1("H*")
  value = Random.rand(1 << 63)
  hash_value = "hermit-runtime".hash
  hash_order = Set.new(%w[alpha bravo charlie delta echo foxtrot golf hotel]).to_a.join(",")
  puts "RANDOM token=#{token} prng=#{value} hash=#{hash_value} hash_order=#{hash_order}"
end

def time_probe
  ENV["TZ"] = "UTC"
  now = Time.now
  puts "TIME unix_ns=#{now.to_i * 1_000_000_000 + now.nsec} utc=#{now.utc.iso8601(9)} zone=#{now.zone}"
end

def thread_probe
  mutex = Mutex.new
  start = Queue.new
  schedule = []
  counter = 0
  workers = THREADS.times.map do |worker_id|
    Thread.new do
      start.pop
      ITERATIONS.times do
        mutex.synchronize do
          counter += 1
          schedule << worker_id
        end
        Thread.pass
      end
    end
  end
  THREADS.times { start << true }
  workers.each(&:join)

  expected = THREADS * ITERATIONS
  raise "thread counter mismatch: #{counter} != #{expected}" unless counter == expected && schedule.length == expected

  digest = Digest::SHA256.hexdigest(schedule.pack("C*"))
  puts "THREAD workers=#{THREADS} counter=#{counter} schedule_sha256=#{digest}"
end

def system_probe
  uname = Etc.uname
  sentinel = ENV.fetch("HERMIT_RUNTIME_SENTINEL")
  status = File.readlines("/proc/self/status", chomp: true)
    .select { |line| line.start_with?("Name:", "Threads:") }
    .map { |line| line.split(":", 2).map(&:strip) }
    .to_h
  proc_hostname = File.read("/proc/sys/kernel/hostname").strip
  puts "SYSTEM uname=#{uname[:sysname]}/#{uname[:machine]}/#{uname[:nodename]} " \
       "env=#{sentinel} proc=#{status['Name']}/#{status['Threads']}/#{proc_hostname}"
end

random_probe
time_probe
thread_probe
system_probe
