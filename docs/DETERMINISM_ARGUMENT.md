# Determinism Argument for Syscall Changes

Every pull request that adds or materially changes syscall handling should
explain why the change preserves Hermit's deterministic contract. Semantic
compatibility with Linux is necessary, but it is not by itself a determinism
argument.

Use the rubric below during design and review, then copy the template into the
pull request description. Mark an item `N/A` only with a syscall-specific
reason. An unexplained `N/A` is an incomplete argument.

## Scope of the Claim

State the exact mode and environment covered by the claim. Unless narrowed in
the pull request, the expected claim is:

> Given the same executable, arguments, environment, immutable filesystem
> inputs, Hermit configuration, and supported host architecture, two
> `hermit run --strict` executions have the same guest-visible behavior.

Hermit does not make a changing filesystem or external network deterministic.
If the syscall exposes either one, identify it as an assumption or reject the
claim. Diagnostic modes such as `--strace-only`, `--namespace-only`, and
`--no-sequentialize-threads` are outside the strict determinism claim.

Guest-visible behavior includes more than the return register:

- return values and `errno`;
- bytes written to guest memory, including structure padding;
- file offsets, status flags, descriptors, and open-file-description state;
- PIDs, TIDs, addresses, inode values, ports, and other allocated identifiers;
- logical time, wakeup order, signal delivery, and thread progress;
- persistent kernel effects and subsequent syscall behavior; and
- output, exit status, and Detcore's deterministic log.

## Review Rubric

### 1. Syscall Semantics

Describe the Linux ABI rather than only the libc wrapper.

- List every argument, output buffer, return value, and relevant `errno`.
- State whether Linux may block, partially complete, restart, or mutate input
  structures.
- Identify shared kernel state: file descriptions, offsets, signal masks,
  mappings, process state, socket state, or child state.
- Explain interception and dispatch in both debug and optimized builds.
- Classify the implementation as emulation, injection plus sanitization,
  deterministic polling, record/replay, or deliberate passthrough.

The argument is incomplete if it covers only the success path, relies on libc
behavior where the raw ABI differs, or leaves release-build subscription
implicit.

### 2. Entropy Sources

Inventory every value or ordering decision that can come from hardware, the
kernel, another task, or the external environment. Check at least:

| Source | Examples |
| --- | --- |
| Time | wall and monotonic clocks, CPU time, timer expiry, timeout remainder |
| Kernel allocation | PID/TID, fd, port, inode, address, mapping placement |
| Concurrency | scheduler choice, readiness, wake order, futex or signal order |
| Hardware | PMU counts, RDTSC, CPUID, RDRAND/RDSEED, CPU feature state |
| Randomness | kernel RNG, randomized identifiers, hash iteration order |
| Filesystem | metadata, directory order, procfs, changing file contents |
| External I/O | network packets, host services, terminals, devices |
| ABI details | partial writes, padding, uninitialized bytes, unused registers |

For each source, label it **eliminated**, **normalized**, **ordered**,
**recorded**, **assumed stable**, or **unsupported**. A host-derived value with
no classification is a review blocker.

### 3. Mitigation Strategy

Connect every entropy source to a concrete mechanism and code path.

- **Emulate:** compute the result only from deterministic state.
- **Normalize:** inject the syscall, then replace nondeterministic fields.
- **Order:** serialize access through deterministic scheduler resources.
- **Poll:** transform a blocking operation into deterministic nonblocking
  probes with logical-time deadlines.
- **Record/replay:** capture every guest-visible result and validate replay.
- **Fail closed:** reject behavior that cannot be made deterministic.

Explain why the mechanism covers output memory and persistent side effects, not
just the integer result. Passthrough is not a mitigation unless all observable
inputs and effects are independently shown deterministic.

### 4. Resource Checkout

List every Detcore resource or ownership token acquired by the syscall. For
each one, document:

- resource ID and `Permission` (`R`, `W`, or `RW`);
- the state or kernel object protected, including aliases through `dup`,
  `fork`, shared mappings, or shared file descriptions;
- the deterministic key and linearization point;
- what happens while the request waits and whether another guest can progress;
- release or one-shot consumption on success, error, timeout, `EINTR`, signal,
  cancellation, and task exit; and
- ordering relative to output-memory writes and the returned result.

Write `None` only after showing that the operation is local to one stopped
guest and cannot race with shared state. Leaked ownership can deadlock;
premature release or per-fd ownership for an open-file-description operation
can reintroduce nondeterministic races.

### 5. Informal Proof

Give a short compositional proof, not just an implementation summary. One useful
form is induction over intercepted guest events:

1. **Assumptions:** enumerate stable inputs and enabled Hermit guarantees.
2. **Pre-state:** assume two strict runs have equivalent registers, guest
   memory, logical time, scheduler state, descriptor state, pending signals,
   and modeled kernel state before this syscall.
3. **Choice:** show that any resource grant, retry, wakeup, timeout, or signal
   decision is a deterministic function of that pre-state.
4. **Host boundary:** show that every host result is absent, normalized,
   deterministically ordered, recorded, or covered by an explicit assumption.
5. **Post-state:** show that return value, `errno`, output memory, persistent
   effects, logical time, resources, and next runnable task are equivalent.
6. **Composition:** explain why retries, interruption, and later syscalls cannot
   observe hidden host state introduced by this step.

If one step cannot be shown, narrow the claim or change the implementation.

### 6. Edge Cases

Cover all that apply:

- null, invalid, aliased, short, or page-boundary guest pointers;
- zero, negative, maximum, overflowing, and ABI-width-dependent values;
- invalid flags and unsupported operation variants;
- `EINTR`, restart behavior, pending signals, and signal-mask restoration;
- zero and infinite timeouts, timeout races, and remainder mutation;
- partial I/O, EOF, readiness changes, spurious wakeups, and closed peers;
- concurrent close, `dup`, `fork`, `exec`, task exit, and shared-object aliases;
- cleanup after every error and cancellation path; and
- debug/release subscription plus relevant run, record, and replay modes.

### 7. Test Evidence

Map tests to claims. Prefer the lowest useful layer, then add an end-to-end
strict-mode test for guest-visible behavior.

Required evidence for a completed argument normally includes:

- semantic tests for success, representative errors, and output memory;
- a concurrent or delayed-completion test when ordering is part of the proof;
- repeated `--strict` executions with exact output and status comparison;
- `hermit run --strict --verify -- ...` for an idempotent reproducer;
- a nondeterministic naked/diagnostic baseline when the test claims to prove
  that Hermit removes an entropy source; and
- record/replay coverage when the changed path claims record/replay support.

Report the command, repetition count, result, architecture, and whether PMU,
CPUID interception, or special kernel features were available. A single green
run, `cargo test --workspace` alone, or tests that assert only exit status do
not establish determinism. `--verify` is strong regression evidence, but finite
testing does not replace the proof above.

## Pull Request Template

Copy and complete this section in a syscall pull request:

```markdown
## Determinism Argument

### Claim and assumptions
- Strict-mode claim:
- Stable external inputs assumed:
- Unsupported or out-of-scope behavior:

### Syscall semantics
- Raw Linux ABI and observable effects:
- Blocking, restart, and partial-completion behavior:
- Shared kernel/model state:
- Hermit path (subscription -> handler -> emulate/inject/poll/record):

### Entropy inventory
| Source | Guest-visible effect | Classification | Mitigation/assumption |
| --- | --- | --- | --- |
| | | eliminated/normalized/ordered/recorded/stable/unsupported | |

### Resource checkout
| Resource and permission | Protects/keyed by | Acquire/linearize | Release on all paths |
| --- | --- | --- | --- |
| | | | |

Aliases (`dup`, `fork`, shared memory/file descriptions):

### Informal proof
Assume equivalent deterministic pre-state in two strict runs. ...

Deterministic choice/order: ...

Host-boundary handling: ...

Equivalent return value, output memory, persistent effects, logical time,
resource state, and next scheduler state: ...

### Edge cases
- [ ] invalid and boundary arguments/pointers
- [ ] errors, partial completion, and cleanup
- [ ] signals, `EINTR`, restart, and timeouts
- [ ] concurrent aliases, close, fork, exec, and exit
- [ ] debug/release subscription and applicable run/record/replay modes

### Test evidence
| Command/test | Runs | Mode/environment | Claim covered | Result |
| --- | ---: | --- | --- | --- |
| | | | | |

Remaining proof or coverage gaps:
```

## Review Outcome

Reviewers should record one of these outcomes:

- **Complete:** every entropy source and resource path is accounted for, the
  informal proof composes, and tests exercise the important claims.
- **Needs revision:** the intended strategy may be sound, but assumptions,
  cleanup, edge cases, or evidence are missing.
- **Not deterministic:** an uncontrolled host value or ordering decision is
  guest-visible. Fail closed, record it, or explicitly exclude the behavior.

A completed rubric supports review; it does not expand Hermit's documented
boundary around changing filesystems and external networks.
