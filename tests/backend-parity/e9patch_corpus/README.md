# e9patch preprocessing parity corpus

Freestanding, statically linked, raw-`syscall` x86-64 guests used by
`../e9patch_corpus.py` to ratchet e9patch preprocessing parity against the
golden ptrace backend.

These guests are deliberately freestanding (`-nostdlib -static -ffreestanding`)
rather than ordinary libc programs. e9tool rewrites only the *main* executable,
so a dynamically linked libc binary exposes zero `SYSCALL` sites in its own ELF
(they live in `libc.so`) and e9patch preprocessing is a no-op
(`candidate_sites=0`). A freestanding guest emits its `syscall` instructions in
the main ELF, so e9patch actually rewrites it (`candidate_sites > 0`). Every
guest ends in `exit_group` (231); a bare `exit` (60) would exit only the calling
thread and hang the run.

| Guest | Exercises |
| --- | --- |
| `minimal_exit` | single site: `exit_group` only |
| `write_stdout` | `write(1, ...)` then exit |
| `getpid_check` | virtualized `getpid` |
| `clock_gettime` | `clock_gettime(CLOCK_MONOTONIC)` |
| `nanosleep` | `nanosleep` |
| `getrandom` | determinized `getrandom` stream |
| `multi_site` | three distinct `noinline` syscall sites (write/getpid/exit) |
| `loop_write` | one site invoked eight times in a loop |
| `mmap_anon` | anonymous `mmap`, touch, `munmap` |
| `uname` | `uname` |
| `sigmask` | `gettid` + `rt_sigprocmask` |
| `compute` | CPU-bound loop (RCB preemption) then exit |

Regenerate identical sources with the parent workspace generator at
`experiments/e9patch_ptrace_corpus_parity_20260731/src/gen_corpus.sh`.
