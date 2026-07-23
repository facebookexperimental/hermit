# frozen_string_literal: true
#
# Thread-scheduling nondeterminism probe.
#
# Spawns N threads that each do a small, equal amount of CPU work and then print
# their id. The order in which the lines appear depends on how the Ruby VM's
# scheduler (GVL hand-off + OS thread scheduling) interleaves the threads, which
# varies run-to-run natively. Under `hermit run --strict` the schedule is
# deterministic, so the order is identical on every run.
n = (ARGV[0] || 24).to_i
threads = (0...n).map do |i|
  Thread.new do
    acc = 0
    2_000.times { |k| acc += (k ^ i) }
    Thread.pass
    $stdout.puts("thread #{i} done acc=#{acc}")
  end
end
threads.each(&:join)
