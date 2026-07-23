n = (ARGV[0] || 2).to_i
ts = (0...n).map { |i| Thread.new { $stdout.puts "thread #{i}" } }
ts.each(&:join)
$stdout.puts "main done"
