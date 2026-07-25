# SaBRe determinism gap analysis

Status: research snapshot, 2026-07-21

## Executive conclusion

The restored SaBRe backend is a useful low-overhead, in-process syscall
interceptor. It is not currently a deterministic execution backend equivalent
to `reverie-ptrace` plus detcore.

SaBRe can replace the result of a syscall that its loader successfully rewrote,
virtualize four selected vDSO entry points, and replace most statically located
`RDTSC` executions. With a deterministic tool implementation, this is enough
for useful syscall-boundary determinism on a constrained, single-threaded or
cooperatively threaded, dynamically linked x86-64 workload.

It cannot currently guarantee deterministic execution for general
multithreaded programs. Native guest threads run concurrently between
callbacks; physical signals arrive at host-selected instructions and threads;
there is no PMU timer or exact preemption; `CPUID` and `RDTSCP` execute natively;
and executable code outside the loader's scan set can issue unintercepted
syscalls. The current thread callbacks describe lifecycle events but do not
form a scheduler.

The shortest credible path is therefore:

1. Close interception and instruction-virtualization holes, and fail closed.
2. Build a blocking, host-coordinated scheduler behind the existing SaBRe API.
3. Add a child-start barrier and deterministic signal broker.
4. Only then add per-thread PMU preemption and exact single stepping.
5. Port and validate detcore's syscall models, rather than treating interception
   alone as deterministic semantics.

No shared Reverie core abstraction needs to change for this sequence. A
SaBRe-specific adapter can translate synchronous callbacks into blocking RPCs
to a host scheduler, at the cost of maintaining a backend-specific execution
path.

## Scope and source baseline

This analysis distinguishes three properties that are easy to conflate:

- **Interception reach:** every relevant event reaches the tool, or execution
  fails before the event can affect guest state.
- **Deterministic semantics:** the tool replaces or records every
  nondeterministic result and side effect.
- **Deterministic ordering:** the backend chooses which thread runs and the
  exact point where asynchronous events become visible.

SaBRe currently provides part of the first property and a mechanism on which to
implement part of the second. It does not provide the third. Ptrace provides
the stop/control primitives for all three, while detcore supplies policy and
models. Ptrace by itself is not a determinism guarantee.

Sources inspected:

- Hermit/detcore `592d5c6ccbced0d1240b6562ff87652cb706f142`.
- Restored Reverie SaBRe commit
  `dc5ac5bf5864aea5495b617515343fa76717da0f`.
- Reverie ptrace base `075d1eff799eb619282cedd303afe9fdacea02a5`.
- SaBRe upstream `05816ee066a7284bee8afd0e73eeb44455b254b4`.
- AMD PMU feature commit
  `9951ce7e0f7ca6610e62b5239a8961a7f63c20d7`.

SaBRe is more than `LD_PRELOAD`: it is a custom ELF loader that rewrites
machine instructions and injects an in-process plugin. It nevertheless has
the same fundamental isolation problem as preload interposition: control code
shares the guest address space and relies on coverage of code discovered by
the loader.

## Gap matrix

Effort is engineering effort including focused tests: S is 2-5 days, M is 1-3
weeks, L is 4-8 weeks, and XL is 8-16+ weeks. Estimates overlap and are not
additive.

| Area | Ptrace plus detcore | SaBRe today | Determinism consequence | Parity effort |
| --- | --- | --- | --- | --- |
| Syscall entry reach | Seccomp `RET_TRACE` produces ptrace stops for subscribed syscall numbers regardless of which mapping contains the instruction. Debug detcore subscribes to all calls. | Rewrites decoded `SYSCALL` instructions in selected ELF `.text` sections. No syscall-number subscription filter. | Calls from stripped/unscanned ELF, arbitrary DSOs, JIT/anonymous executable memory, or static binaries can escape. | L |
| Fail-closed behavior | A subscribed call cannot execute before the tracer handles the stop. | The inspected revision installs no seccomp filter, despite a stale rewriter comment claiming one. A missed instruction executes normally. | Coverage bugs can silently destroy determinism. | M-L |
| Syscall result control | Stopped registers, memory, `inject`, retry, and tail injection support suppression, replacement, and multi-syscall emulation. | Callback can return a replacement value or synchronously execute a direct syscall. It has local memory but no register, stack, remote injection, retry, or subscription API. | Simple emulation works; detcore's blocking and multi-step handlers do not port directly. | L |
| Syscall semantics | Detcore models a substantial explicit set, but unsupported calls may still pass through unless strict mode is enabled. | Default tool executes the host syscall. Backend-only special cases cover lifecycle, signal registration, `execve`, `readlink`, and protected FDs. | Interception is not result determinism. Each detcore model still needs a port or bridge. | XL |
| vDSO/time | Ptrace setup patches/disables vDSO paths and detcore virtualizes time syscalls and TSC when configured. | Routes `clock_gettime`, `getcpu`, `gettimeofday`, and `time` vDSO functions to callbacks. | Selected libc paths can be virtualized, but alternate code/mappings and instruction sources remain. | M |
| `RDTSC`/`RDTSCP` | `PR_SET_TSC(PR_TSC_SIGSEGV)` traps both anywhere in the process; ptrace decodes the operation and writes TSC plus `TSC_AUX`. | Static rewrite handles opcode `0f 31` only. The normal jump path returns a virtual value. No `RDTSCP` or AUX contract. | Native `RDTSCP` and unscanned `RDTSC` expose host time/CPU. | M |
| RDTSC slow path | Signal stop preserves registers for tracer emulation. | Rare UD2 fallback calls the plugin but discards its return value before advancing RIP. | A successfully located but non-relocatable `RDTSC` gets a wrong result. | S |
| `CPUID` | Per-tracee `ARCH_SET_CPUID(0)` turns the instruction into a pre-delivery fault on supported x86 Linux; detcore returns a stable table. | No callback or rewrite. SaBRe deliberately leaves `SIGSEGV` outside central mediation. | CPU model/topology/features remain host dependent. | M |
| PMU/preemption | Per-thread retired-conditional-branch counter interrupts a stopped tracee; precise timers single-step to the requested RCB/instruction boundary. | No timer, PMU, clock, or timer-event API. `SIGSTKFLT` is currently reserved for controlled exit. | A CPU-bound guest can run indefinitely without a tool boundary. | XL |
| Async signals | Ptrace signal-delivery stop occurs before guest delivery. Tool can suppress or replace the signal, and detcore can schedule the event. | Kernel first chooses delivery time and target. An in-process central handler queues notification and manually invokes the guest handler. Tool notification cannot suppress, replace, defer, or retarget it. | Signal observation is host-schedule dependent and cannot drive replay. | L-XL |
| Signal ABI | Kernel performs normal delivery after tracer decision, including masks, frame, alt stack, and context. | Masks, `SA_NODEFER`, `SA_RESETHAND`, alt stack, and original `ucontext_t` are not reproduced; queue is 64 entries; realtime ordering/payload is absent. | Even a fixed signal sequence can produce different guest-visible behavior. | L |
| Thread creation | Kernel ptrace clone/fork/vfork events stop the child before its first instruction. Reverie initializes parent-aware thread state before execution. | Rewritten wrappers preserve clone/vfork call mechanics. Tracking is lazy at a thread's first intercepted boundary and exposes only native TID start/exit callbacks. | Child code can race and mutate state before registration; no deterministic parent/child order. | L |
| Thread scheduling | Detcore resource requests and precise timers serialize runnable threads and preempt pure userspace loops. | Native kernel scheduler runs all guest threads. Signal exclusion only protects callbacks; it is not a run token. | Shared-memory races, lock acquisition, and syscall arrival order are nondeterministic. | XL |
| `exec` | Ptrace follows kernel `execve`/`execveat` events and reinitializes interception state. | `execve` is relaunched through SaBRe; `execveat` is unsupported. | `execveat` escapes the loader, and new-image coverage retains all rewrite limitations. | M |
| Binary/mapping coverage | Syscall and fault traps work for static, stripped, dynamically loaded, and generated code once tracing/filter state is installed. | Dynamic x86-64 only. Rewriter requires `.text`; initial known libraries are loader, libc, librt, libpthread, and libresolv. Arbitrary executable DSOs and JIT mappings are not generally scanned. | Cannot claim arbitrary-binary determinism. | L-XL |
| Isolation | Tracer state is in a separate process. | Plugin, RPC state, signal machinery, and guest share an address space and file table. | Guest corruption, reserved-FD collisions, or unsafe signal reentrancy can corrupt the controller. | L; architectural |

## Syscall coverage in detail

### What SaBRe intercepts

SaBRe does not choose syscalls by number. For every executable region that it
successfully scans, it decodes each `SYSCALL` instruction and redirects it to
the plugin. Therefore a rewritten call with any Linux syscall number reaches
`Tool::syscall`. Calls made while the recursion protector says execution is in
the loader/plugin bypass the tool intentionally.

The practical coverage is narrower than "all syscalls":

- `patch_syscalls` scans an ELF `.text` section, not all executable `PT_LOAD`
  segments. It returns without rewriting when section headers or `.text` are
  absent.
- Initial rewriting covers the client and dynamic loader. Runtime library
  handling recognizes libc, librt, libpthread, and libresolv by name.
- Arbitrary DSOs containing raw syscalls, anonymous executable mappings, and
  JIT-generated code have no general rewrite hook.
- Static binaries are explicitly unsupported by the loader.
- The source comment claiming missed syscalls are killed by seccomp is stale;
  no seccomp installation exists in the inspected tree.

Within the Reverie plugin, `clone`, `clone3`, `vfork`, `exit`, and `exit_group`
receive lifecycle wrappers. Direct execution additionally special-cases
`execve`, `readlink`, `rt_sigaction`, `rt_sigprocmask`, and protected file
descriptors. All other calls default to a raw host syscall. These special cases
are runtime integrity behavior, not detcore semantic parity.

### What release detcore asks ptrace to intercept

Release detcore has 60 unconditional syscall subscriptions:

`write`, `openat`, `open`, `creat`, `close`, `read`, `mmap`, `fcntl`, `futex`,
`clone`, `clone3`, `fork`, `vfork`, `wait4`, `setsid`, `uname`, `exit_group`,
`exit`, `dup`, `dup2`, `dup3`, `pipe`, `pipe2`, `getrandom`, `utime`, `utimes`,
`utimensat`, `futimesat`, `socket`, `socketpair`, `eventfd`, `eventfd2`,
`sched_getaffinity`, `sched_setaffinity`, `signalfd`, `signalfd4`,
`timerfd_create`, `memfd_create`, `userfaultfd`, `accept`, `accept4`,
`nanosleep`, `clock_nanosleep`, `sched_yield`, `poll`, `epoll_create`,
`epoll_create1`, `epoll_ctl`, `epoll_pwait`, `epoll_wait`, `epoll_wait_old`,
`epoll_ctl_old`, `recvfrom`, `rt_sigtimedwait`, `execve`, `execveat`, `getcpu`,
`rt_sigprocmask`, `rt_sigaction`, and `sysinfo`.

It also always adds `add_key`, `request_key`, and `keyctl` (currently
passthrough). Configuration adds these groups:

- Scheduling: `alarm`, `pause`, `connect`, and sometimes `bind`.
- Metadata virtualization: `getdents`, `getdents64`, `stat`, `lstat`, `fstat`,
  `newfstatat`, and `statx`.
- Time virtualization: `gettimeofday`, `time`, `clock_gettime`, and
  `clock_getres`, plus RDTSC subscription.
- CPU virtualization: CPUID subscription.
- Record/replay may contribute more subscriptions. Debug builds subscribe to
  every syscall and both instruction classes.

The semantic dispatch is broader than the release subscription list: it also
contains handlers for `recvmsg`, `sendto`, `sendmsg`, and `sendmmsg`, plus
explicit passthroughs for startup/memory/key calls. Conversely, subscription
does not imply complete modeling: deprecated epoll calls panic, `futimesat`
returns `ENOSYS`, and the catch-all passes through unless
`panic_on_unsupported_syscalls` is set. A parity claim must therefore measure
both callback reach and deterministic model coverage.

## Nondeterministic instructions

### RDTSC and RDTSCP

Ptrace uses a kernel-assisted faulting mode. This is stronger than static
rewriting because it catches execution from any mapping, recognizes both
`RDTSC` and `RDTSCP`, and gives the tracer stopped registers in which to place
RAX/RDX and RCX (`TSC_AUX`). Detcore derives the returned timestamp from logical
time.

SaBRe's common rewritten `RDTSC` path is usable: the callback's 64-bit return is
split into RAX/RDX. Four gaps prevent a guarantee:

1. There is no `RDTSCP` recognition or AUX result.
2. Only scanned `.text` is covered.
3. The UD2 fallback discards the callback return.
4. The default callback executes real `RDTSC`; determinism requires a tool to
   supply logical time.

The preferred fix is to use `PR_SET_TSC(PR_TSC_SIGSEGV)` inside the guest and
add a precise SIGSEGV/ucontext emulation path owned by the SaBRe runtime. Static
rewrite can remain the fast path. This also gives fail-closed coverage for
generated code, but it requires careful coexistence with genuine SIGSEGV.

### CPUID

SaBRe has no CPUID support. A compatible implementation does not require
ptrace, but it does require Linux/x86 CPUID faulting support:

1. Call `arch_prctl(ARCH_SET_CPUID, 0)` in every guest thread/process context.
2. Distinguish the two-byte CPUID instruction in the SIGSEGV handler.
3. Read EAX/ECX and write deterministic EAX/EBX/ECX/EDX into `ucontext_t`.
4. Advance RIP, preserving real SIGSEGV delivery for all other faults.
5. Reapply or validate the setting across clone and exec.

Detcore's stable CPUID table can be reused as data, but the synchronous SaBRe
API needs a CPUID callback. On hardware/kernel combinations without CPUID
faulting, exhaustive executable-region rewriting is the fallback and should be
declared unsupported unless coverage can fail closed.

## PMU preemption and deterministic scheduling

Ptrace's key advantage is not just access to `perf_event_open`; it owns a
stopped task. Reverie opens a per-thread retired conditional branch counter,
requests an interrupt before the target, suppresses the timer signal, and uses
ptrace single-step until the exact RCB/instruction target. Detcore uses that
event to end a timeslice even when guest code makes no syscall.

An in-process SaBRe implementation would need all of the following:

- Per-thread perf events and inheritance/cleanup tied to clone/exit.
- A signal reserved for PMU overflow and blocked from guest disposition.
- A scheduler run token so no other guest thread runs while a target is being
  resolved.
- An overflow handler that edits the interrupted `ucontext_t`, enables x86 Trap
  Flag, and owns subsequent SIGTRAP delivery.
- Exact RCB/instruction accounting across every step, including intervening
  synchronous faults and signals.
- A way to defer tool RPC and allocation out of async-signal context.
- A late-interrupt policy. Once an in-process thread executes past the desired
  boundary, it cannot single-step backward without checkpoint/rollback.

Skid margin should normally be a performance tuning parameter: an earlier stop
only creates more single steps. On AMD EPYC 9D85, the measured p99 skid is 384
RCBs but rare observations reach roughly 61K, so a 1K margin cannot be assumed
to bound delivery. The recent optimization intentionally chooses 1K for
performance. The inspected ptrace base still asserts if the initial counter is
already beyond the exact target. SaBRe parity needs an explicit, tested
long-tail policy rather than inheriting the numerical margin as a correctness
assumption.

Before PMU work, a syscall-boundary scheduler can provide a useful cooperative
mode: only the thread holding a host-granted token may return from a callback.
That mode still cannot preempt a busy loop or deterministically order a race that
occurs entirely between callbacks, so it must be labeled accordingly.

## Signals

Ptrace stops before signal delivery. Reverie's tool callback receives a stopped
guest and returns `Some(signal)` to deliver/replace it or `None` to suppress it.
Detcore turns the event into a scheduler resource request before resuming the
thread. This is the mechanism needed for deterministic placement, although
external signal arrival itself still must be recorded/replayed or excluded.

SaBRe's central handler improves runtime stability but is later in the causal
chain. The kernel has already selected a native target thread and interrupted
it at a host-dependent instruction. The tool receives only an integer
notification; it cannot choose delivery. The runtime's subsequent manual
handler invocation does not reproduce the kernel signal ABI completely.
Central mediation is limited to standard catchable signals: `SIGILL`,
`SIGSEGV`, `SIGKILL`, and `SIGSTOP` are excluded, while `SIGSTKFLT` is consumed
as a runtime exit-control signal. Realtime delivery and payload ordering are
not implemented.

A deterministic SaBRe signal design should:

1. Block asynchronous signals in every guest thread before guest execution.
2. Receive them in a dedicated broker (for example, `signalfd` or a host
   controller), recording order, payload, and external source.
3. Queue logical signals in the scheduler and select target plus delivery point
   at a deterministic safe point.
4. Construct kernel-equivalent masks, siginfo, alt-stack, reset/nodefer, and
   context behavior, or explicitly reject unsupported actions.
5. Keep synchronous faults on a separate path for TSC/CPUID/PMU emulation and
   genuine guest faults.
6. Define realtime queueing and overflow without coalescing.

Without the scheduler and broker, SaBRe can make handler execution safer but
cannot promise repeatable signal timing.

## Clone, fork, and exec reliability

The stabilized SaBRe wrappers correctly address several ABI hazards: clone
with a user stack, vfork child return, callback exact-once tracking, and
coordinated `exit_group`. The conformance gate covers repeated pthread
create/join and basic fork/signal behavior. These are runtime correctness wins,
not deterministic creation guarantees.

Ptrace receives kernel clone/fork/vfork events and stops a child before its
first instruction. Reverie's `init_thread_state` receives a snapshot of the
creating parent's state, and detcore can register the child with its scheduler
before release. SaBRe discovers a native child lazily when it reaches a rewritten
boundary. A child can execute arbitrary guest code, win locks, modify shared
memory, or receive a signal before the backend has a record for it.

Parity requires a creation handshake in the wrapper:

- Parent announces intended clone flags and deterministic parent identity.
- Child enters a runtime trampoline before returning to guest code.
- Child establishes TLS, CPUID/TSC/PMU state, signal masks, and RPC identity.
- Child blocks until the host scheduler registers it and grants a run token.
- Parent does not report clone completion to detcore until registration is
  coherent; vfork retains its required parent-blocking semantics.
- Forked runtime locks, queues, RPC connections, protected FDs, and perf events
  are reinitialized without allocation-unsafe atfork behavior.
- `execve` and `execveat` preserve deterministic identity while rebuilding all
  interception state.

Native PID/TID values also remain a guest-visible nondeterministic input.
Detcore itself contains TODOs for complete PID/TID virtualization, so this is a
shared policy gap rather than a SaBRe-only regression.

## Guarantees that are defensible today

For a dynamically linked x86-64 guest whose executable code is fully within
SaBRe's known scan set, the backend can guarantee that:

- A successfully rewritten syscall reaches the synchronous tool before the
  kernel executes it.
- The tool may replace the return value or execute a chosen syscall using local
  memory.
- Selected vDSO functions and the normal rewritten `RDTSC` path can return
  virtual values.
- Threads that reach callbacks can be observed and coordinated during normal
  exit.

A repeatability claim additionally needs all of these workload restrictions:

- Single-threaded, or threads cooperate only at rewritten boundaries with no
  racy shared-memory behavior between them.
- No asynchronous/realtime signals, or signals are excluded by the harness.
- No static, stripped-without-`.text`, arbitrary raw-syscall DSO, or generated
  executable code.
- No `RDTSCP`, `CPUID`, unrewritten `RDTSC`, or other host-dependent instruction.
- Every nondeterministic syscall/vDSO result is modeled by the tool; there is no
  external shared memory, device state, or unrecorded I/O.

Outside that profile, the correct description is "intercepted execution," not
"deterministic execution."

Ptrace plus detcore has a materially stronger mechanism, but its guarantee is
also configuration- and workload-dependent: release subscriptions omit some
calls, unsupported calls can pass through, external signals require an input
policy, complete PID/TID virtualization is unfinished, and precise PMU support
depends on validated CPU/kernel behavior.

## Prioritized roadmap

### P0: Coverage closure and fail-closed execution (L, 4-8 weeks)

- Scan executable `PT_LOAD` segments rather than requiring `.text`.
- Track every executable mapping, including arbitrary `dlopen` DSOs and W-to-X
  transitions, or combine rewriting with Syscall User Dispatch/seccomp fallback.
- Make a missed guest syscall terminate with a diagnostic.
- Support `execveat`; explicitly reject static/JIT workloads until covered.
- Add stripped ELF, custom DSO, anonymous executable, and self-modifying tests.

Exit criterion: no test can execute a guest syscall without either a callback
or a deliberate fail-closed termination.

### P1: Instruction/time closure (M, 2-4 weeks)

- Fix the RDTSC UD2 return path.
- Add `RDTSCP` plus TSC_AUX and complete mapping coverage.
- Add CPUID faulting/emulation with a deterministic table.
- Ensure all vDSO and direct time sources use the same logical clock.
- Separate genuine SIGSEGV/SIGILL from runtime instruction traps.

Exit criterion: adversarial instruction tests return identical values across
CPUs or fail with an explicit unsupported-platform error.

### P2: SaBRe detcore adapter and cooperative scheduler (L-XL, 6-12 weeks)

- Keep shared Reverie abstractions unchanged; implement a SaBRe-local adapter.
- Translate synchronous callbacks into blocking host scheduler/resource RPCs.
- Add typed per-thread state, deterministic identity, parent snapshots, and a
  child-start barrier.
- Port injection-dependent syscall models to direct local operations or a
  backend-specific syscall transaction API.
- Park/unpark threads without exposing controller futexes and FDs to the guest.

Exit criterion: syscall-boundary schedules and outputs replay identically for
threaded workloads, with busy-loop non-preemptibility documented.

### P3: Deterministic signals (L, 4-8 weeks after P2)

- Add the broker/queue/target policy described above.
- Reproduce or reject each signal ABI flag explicitly.
- Record/replay externally originated signals.
- Add realtime, process-directed, thread-directed, fork, exec, and overflow
  stress tests.

Exit criterion: a recorded signal trace replays with the same target thread,
logical point, siginfo, and handler-visible context.

### P4: Exact PMU preemption (XL, 6-12+ weeks after P2)

- Add per-thread PMU setup and precise timer callbacks.
- Implement safe TF/SIGTRAP stepping or choose broader DBI/basic-block
  instrumentation instead.
- Validate counter monotonicity, migration, virtualization, throttling, nested
  signals, and CPU model selection.
- Include AMD EPYC 9D85 long-tail skid tests; do not use p99 as a bound.

Exit criterion: CPU-bound multithreaded workloads replay at identical
RCB/instruction boundaries over long stress runs on every supported CPU class.

### P5: Semantic and operational parity (ongoing, L-XL)

- Port the 60-call detcore base, optional groups, and strict unsupported policy.
- Match blocking syscall, futex, poll/epoll, network, metadata, randomness,
  clone, and exec behavior.
- Harden in-process isolation, protected FDs, atfork state, and signal safety.
- Run the same backend-neutral conformance corpus and compare event logs.

A realistic first milestone is a deterministic single-thread/cooperative mode
in roughly 6-10 person-weeks after coverage work. General multithreaded parity
with signals and exact preemption is a multi-quarter effort, approximately
20-35 person-weeks before broad workload hardening.

## Recommendation

Use SaBRe now for low-overhead syscall tracing, experimentation, and explicitly
constrained deterministic workloads. Do not expose it as a transparent detcore
backend yet.

The next implementation project should be P0 plus P1, followed by a
syscall-boundary scheduler prototype. That ordering tests whether an in-process
controller can maintain coverage and scheduler integrity before investing in
the highest-risk PMU/signal work. Full ptrace parity should not be a release
gate for the prototype, but every omitted guarantee must fail closed or be
visible in the selected execution profile.
