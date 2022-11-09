# Flaky-tests - patterns that are known to cause test flakiness

Known flaky tests patterns. These patterns are derived from fbcode real
tests, ideally the size of the folder should grow, while the flaky tests
in fbcode should shrink.

## Bind arbitrary/random number
The bind(2) syscall allow specify a port number, however, using arbitrary or
random number could cause bind fail with -EADDRINUSE, or flakiness for tests,
because it may not always fail.

One mitigation is use Linux network namespace, and mount a private network so
that the test with sockets could still work unless it uses external network.

However, the test should not rely on network namespace, instead of binding an
arbitrary port, the test could also bind with zero, which kernel will assign
next available port hence it would always success. The port number can be
retrieved by calling getsockname(2).

Same tests use a for loop for bind: always retry with next port when bind
returns -EADDRINUSE, this is less ideal, not only it is brute force, but also
it may cause TOCTOU issue.
