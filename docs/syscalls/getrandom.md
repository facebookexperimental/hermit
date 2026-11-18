---
schema: syscall-pages/v3
syscall: getrandom
nr_x86_64: 318
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+getrandom&body=Discussion+for+the+%60getrandom%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fgetrandom.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60getrandom%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 403e350e9e82ee540529f097daccae6a60832d07
  detcore_class: determinized
  dispatch: "typed"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
# prose: human/agent-authored, machine-checked (syscall-pages.rs check); v3 only
prose:
  status: checked
  reviewed_at: 2026-11-16
  based_on: 7ac736d54e14d3eb8e190ae1849481e5ff41ea1a
---

## In short

Natively, getrandom returns fresh kernel entropy, different on every call. Hermit fills it from the caller's seeded deterministic stream instead, so the same seed and schedule give the same bytes. Known gaps: 3.

## Determinism strategy

**What is nondeterministic natively.** `getrandom` exists to return unpredictable bytes: on Linux the output buffer contents differ on every call by design, drawn from the kernel entropy pool, and early after boot a blocking call can also wait for pool initialization unless the caller passes `GRND_NONBLOCK` (then it can fail with `EAGAIN`) or `GRND_INSECURE` (then it returns possibly-uninitialized entropy).[^linux] The nondeterministic channels are therefore the bytes written, the returned length in fault and edge cases, and whether the call blocks.[^linux]

**What Hermit does.** Detcore determinizes `getrandom`: the call is intercepted by typed dispatch and filled from the calling thread's own seeded deterministic PRNG stream, without consulting the host kernel's entropy at all.[^c1][^c2][^c3] The root thread's stream is constructed from Hermit's configured RNG seed,[^c4] so the bytes a guest receives are a pure function of that seed and of the sequence of draws earlier in the serialized, repeatable schedule.

**Interface fidelity.** The three Linux flags are all accepted but *coalesced*: `GRND_NONBLOCK`, `GRND_RANDOM`, and `GRND_INSECURE` are served by the same always-ready stream, while any other flag bit, or `GRND_RANDOM` with `GRND_INSECURE` together, is rejected with `EINVAL` before guest memory is touched.[^c5] Requests are clamped to Linux's `MAX_RW_COUNT` (INT_MAX rounded down to a page), as the kernel's own buffer import does.[^c6][^c7] A zero-length request returns zero without touching the buffer; a null buffer with nonzero length fails with `EFAULT`; if a write faults after some bytes were delivered, the short count is returned instead of an error, matching the kernel's partial-copy behaviour.[^c8][^c10] The attempted chunk is drawn from the stream in full even when the write falls short, so the stream position after a faulting call depends on the fault point — repeatable under the same schedule and memory state, and worth knowing when comparing streams across runs.[^c10] The handler never sleeps, so there is no `EAGAIN` and no `EINTR` path.[^c8]

**Record & replay.** `getrandom` is subscribed in both run and record/replay modes (code facts in the Appendix). No bytes need to be recorded to reproduce a run: a replay that reaches the same call in the same schedule draws the same bytes from the same per-thread stream state.[^c8] The exec boundary preserves this — a loader that serves `getrandom` before post-exec hands its fills and stream state over instead of reseeding, and the initial `AT_RANDOM` bytes are drawn from the same stream, with a failed write leaving the consumed stream state in place exactly as the ordinary path does.[^c8][^c9]

## Known gaps

- G1 (by design): the `/dev/random` versus `/dev/urandom` distinction and pool-readiness blocking are not modelled; a guest probing entropy readiness with `GRND_NONBLOCK` always sees a ready pool.[^c5]
- G2 (by design): the stream is a PCG-family generator, not the kernel's ChaCha-based one, so bytes are repeatable across Hermit runs with the same seed but never equal to a native run's bytes.[^c4]
- G3 (by design): because the call never blocks, signal-delivery timing around a would-be-blocking native `getrandom` is not reproduced.[^c8]

## Discussion

[Start a discussion about `getrandom`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+getrandom&body=Discussion+for+the+%60getrandom%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fgetrandom.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60getrandom%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Appendix

### Code facts

Detcore classifies `getrandom` (318) as determinized via typed dispatch; deterministic refusal: false; run subscription: true; record/replay subscription: true.

- Reviewed list: `detcore/src/syscall_classification.rs:1721`
- Dispatch: `detcore/src/lib.rs:8328`

### Tests

Flag acceptance and rejection plus the length clamp: `detcore/src/syscalls/misc.rs` (`getrandom_accepts_linux_flags`, `getrandom_rejects_invalid_flags`, `getrandom_caps_requests_at_linux_max_rw_count`) — interface fidelity. Stream seeding, exec handoff, and fault behaviour: `detcore/src/random.rs` (`shared_random_preserves_auxv_and_fault_semantics`, `eight_byte_random_copy_distinguishes_faults_from_backend_errors`) — what Hermit does, interface fidelity, and record and replay.

### Citations

[^linux]: Linux `getrandom(2)`:
  https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/drivers/char/random.c?h=v7.1.3
[^c1]: detcore/src/syscall_classification.rs:1721 sha256:c54a525905843c1d
  https://github.com/facebookexperimental/hermit/blob/main/detcore/src/syscall_classification.rs#L1721
[^c2]: detcore/src/lib.rs:8328 sha256:93780f6568a9ccb6
  https://github.com/facebookexperimental/hermit/blob/main/detcore/src/lib.rs#L8328
[^c3]: detcore/src/syscalls/misc.rs:995-1006 sha256:da781258bb76f39c
  https://github.com/facebookexperimental/hermit/blob/main/detcore/src/syscalls/misc.rs#L995-L1006
[^c4]: detcore/src/random.rs:85-89 sha256:80070102c8955726; detcore/src/tool_local.rs:8064 sha256:77f5b8fa2750c93f
[^c5]: detcore/src/random.rs:353-367 sha256:457f6f1d8c825499
[^c6]: detcore/src/random.rs:369-372 sha256:1214b3548eb1bffa
[^c7]: detcore/src/page.rs:33-36 sha256:f905def3a3370b21
[^c8]: detcore/src/random.rs:498-531 sha256:04119e41e82c2eb6
[^c9]: detcore/src/random.rs:531-545 sha256:ecba227f019bd3b1
[^c10]: detcore/src/random.rs:422-465 sha256:f70450ad8a392d48

## Status

- 2026-11-15 · rev 54bd03547df0 · facts-only page generated from Detcore code facts at this revision.
- 2026-11-16 · rev 7ac736d54e14 · facts refreshed from Detcore at this revision.
- 2026-11-16 · rev 7ac736d54e14 · checked determinism-strategy prose added (rubric v3); citations verified at this revision.
- 2026-11-16 · rev 8e48c1c6605b · summary-first layout (In short, Known gaps, Appendix); claims unchanged.
- 2026-11-18 · rev 403e350e9e82 · facts refreshed from Detcore at this revision.
