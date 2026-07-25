# Manual C regression programs

These programs are focused reproducers that are not part of the generated
Cargo test manifest. Build their executables outside the source tree or remove
them before committing.

## Robust futex owner death

`robust_futex_test.c` checks the Linux robust-list contract for a waiter that is
already blocked when a mutex owner exits:

1. Thread A explicitly re-registers glibc's robust-list head with
   `set_robust_list`, then locks a `PTHREAD_MUTEX_ROBUST` mutex.
2. Thread B enters `pthread_mutex_lock` and sets the mutex's `FUTEX_WAITERS`
   bit.
3. Thread A exits without unlocking.
4. Thread B must wake and receive `EOWNERDEAD`, mark the mutex consistent, and
   unlock it.

The `FUTEX_WAITERS` check is important. Without it, Thread B could start after
Thread A exits and observe `EOWNERDEAD` from the mutex word without exercising
the owner-death wakeup.

Build the reproducer from the repository root:

```bash
cc -O2 -Wall -Wextra -Werror -pthread \
  tests/bin/robust_futex_test.c -o robust_futex_test
```

### Native control

On x86_64 Linux with glibc, the control exits 0:

```text
$ timeout 10s ./robust_futex_test
PASS: robust mutex waiter received EOWNERDEAD
```

### Current strict result

At fork `main` commit `46836669bd6c2f7151fbe65c55f4ea5bd1440897`, the
requested strict run does not reach L1 (ptrace backend, default log level,
relaxations: none):

```text
$ timeout 10s target/release/hermit run --strict -- ./robust_futex_test
thread 'main' (1) panicked at detcore/src/scheduler.rs:1631:17:
Deadlock detected: thread(s) waiting on futex, but no runnable threads left.
```

The host timeout exits 124 because the scheduler panic does not terminate all
stopped tracees. A DEBUG capture makes the missing bridge explicit:

```bash
timeout 20s target/release/hermit --log debug run --strict -- \
  ./robust_futex_test 2>/tmp/robust-futex-owner-death-debug.log
```

The waiter (`dtid 7`) blocks on the robust mutex word:

```text
inbound syscall: futex(0x404100, 0, -2147483643, NULL, NULL, 4210976) = ?
```

The owner (`dtid 5`) then exits. Detcore logs only its modeled
`CLONE_CHILD_CLEARTID` wake, at a different futex address. The scheduler's
deadlock dump still contains `dtid 7` in `futex_waiters` at address `4210944`
(`0x404100`). Linux kernel robust-list cleanup updated the mutex and issued an
internal kernel wake, but that wake could not reach Detcore's emulated waiter
queue.

### Polling-mode diagnostic

Polling mode observes the kernel's owner-death word update instead of relying
on Detcore's precise waiter queue. It reaches L2 (ptrace backend, ERROR log
level, `--debug-futex-mode polling`, no determinism relaxations):

```text
$ timeout 20s target/release/hermit --log error run --strict --verify \
    --debug-futex-mode polling -- ./robust_futex_test
:: Run1...
:: Run2...
Logs contain 531 | 531 messages total
Logs contain 294 | 294 DETLOG & scheduler COMMIT messages
Done processing logs, no substantive differences found.
:: Success: deterministic. Determinism verified.
```

This control confirms that kernel robust-list cleanup updates the mutex word
correctly. It does not fix the default precise mode, where the kernel wake and
Detcore's emulated waiter queue remain disconnected.

After the robust-list bridge is implemented, the strict command above should
exit 0 and print the same PASS line as the native control.
