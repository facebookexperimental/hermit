---
name: backend-reality-reviewer
description: Audit Hermit backend completion claims against real CLI execution, Reverie Backend implementation, Detcore Tool integration, arbitrary-program support, and hermit-cli linkage. Use whenever a backend agent reports progress or completion, or when hermit-coord evaluates whether a backend claim is real.
---

# Backend Reality Reviewer

> **Don't break the demos.** A backend- or demo-touching change must be
> adversarially reviewed with a green-demo run (the demo still passes) before it
> lands — see the demo-touching-commit adversarial-review policy.

## Purpose
This skill is used by hermit-coord to audit backend claims. Every time a backend agent reports completion, run this checklist to determine how "real" the backend is.

## Post-facto human-review criteria

Apply `post-facto-human-review` exactly when a PR contains at least one of
these four triggers:

1. new syscall support, after verifying `AUTONOMOUS-BOT-IMPLEMENTED` at the
   new dispatch/classification entry and `TODO-HUMAN-REVIEW(PR-id)` at the
   implementation or determinization block;
2. a Reverie API/core-abstraction change to the `Tool`, `Guest`, `Backend`,
   or syscall-interception model;
3. a new determinization strategy; or
4. a core DetCore scheduling change affecting how programs are scheduled,
   especially race search. Trigger 4 is always labeled.

Routine backend parity toward the golden ptrace reference implementation is not
a trigger merely because it changes a non-ptrace backend. It is labeled only if
it also meets one of the four triggers.

Every PR description requires `Summary`, mandatory `Determinism` (why the
change is deterministic plus its logic or informal proof), `Linux Semantics`
(how it matches real Linux kernel behavior, or why a deviation is safe), and
`Validation`. KVM PRs also require `Relationship to gVisor`. A labeled PR
additionally requires `Human Review Required`, naming the specific numbered
trigger rather than vague prose such as "backend change". The syscall tags above
verify trigger 1; they are not blanket backend-change markers. Hermit
[PR #1151](https://github.com/rrnewton/hermit/pull/1151), which moved slowdown
into virtual-time/epoch scheduling, is the canonical good example for trigger 4.

For any time/clock/scheduling change, the `Determinism` argument must show that
virtual time stays **continuous and fine-grained**, and `Validation` must
demonstrate **continuous evolution** (repeated and cross-exec/cross-thread/
cross-backend reads), **not a single first-sample** match. A backend that
"achieves parity" by rounding, freezing, coarsening, or resetting time is faking
it — score it as unproven and see
[continuous-virtual-time-is-sacred](../continuous-virtual-time-is-sacred/SKILL.md).
Core determinism/time/scheduling changes (triggers 3 and 4, and any
time-virtualization change) must pass **dual independent adversarial review — one
`claude` agent and one `codex` agent — before landing**, per
[post-facto-review](../post-facto-review/SKILL.md).

## Milestone Completion Gate

**A milestone is NOT DONE until the code is on main.**

- Building on a feature branch = in progress.
- PR opened = in review.
- Merged to main + `hermit run --backend X` works = DONE.
- Never close a backend milestone task for work on an unlanded branch.

## Deep Code-Path Audit

Before assigning a backend score, trace and record the literal implementation path.

1. Trace `--backend X` from CLI parsing and dispatch to `Detcore<XxxGuest>`; identify any path that bypasses Detcore.
2. Inspect `run_kvm()` and `run_dbi()`. Each must instantiate `detcore::Config` and construct a real Detcore tool (or the exact shared Detcore construction used by ptrace).
3. Trace representative syscalls from backend interception into Detcore handlers and back to the guest; determine whether they are determinized or merely passed through to the host.
4. Capture INFO logs for the same program under ptrace and backend X, then compare whether both paths show equivalent syscall interception and Detcore handling.
5. **If backend X bypasses Detcore, its score is B0 regardless of test passes, program output, or Guest implementation completeness.**
6. **A backend milestone is not done until this code is on `main`; a feature branch is in progress and an open PR is in review.** Confirm the command on `main` before closing the milestone.

Record exact `file:line -> symbol -> symbol` paths, commands, and literal output. Do not infer integration from type names, crate names, or a successful process exit.

## The Test

A backend is REAL if and only if:

1. **`hermit run --backend X --strict --verify -- echo hello` exits 0 on main branch**
   - If --backend flag doesn't exist on main: NOT a real backend yet
   - If it exists but ignores the program (canned output): FAKE

2. **`impl Backend for XxxBackend` exists in the crate**
   - grep `impl Backend` in the relevant reverie-xxx crate
   - If missing: prototype, not a backend

3. **Detcore loads as Tool**: `Detcore<XxxGuest>` instantiation exists
   - grep `Detcore<` in hermit-cli or the backend crate
   - If missing: not integrated with hermit's determinism engine

4. **Arbitrary programs run**: test at least 3 real programs (echo, true, cat)
   - All must produce correct output
   - All must pass --verify (determinism check)

5. **hermit-cli links the backend**: check Cargo.toml dependencies
   - If hermit-cli doesn't depend on reverie-xxx: not wired in

## Scoring

| Score | Meaning |
|-------|---------|
| B0 | Crate exists, compiles |
| B1 | Guest trait partially implemented |
| B2 | Can run trivial programs through Detcore<XxxGuest> |
| B3 | Passes 50%+ of ptrace strict-verify corpus |
| B4 | Passes 100% of ptrace strict-verify corpus = DONE |

Detcore integration is a hard prerequisite. A backend that bypasses Detcore is
B0 even if its observed program tests would otherwise qualify for a higher level.

## Audit Procedure

Run these commands and record literal output:

```bash
# 1. Check --backend flag exists
target/release/hermit run --help 2>&1 | grep -i backend

# 2. Check Backend trait impl
grep -rn 'impl Backend' reverie/reverie-*/src/

# 3. Check Detcore integration
grep -rn 'Detcore<' hermit-cli/src/ detcore/src/

# 4. Check Cargo.toml linkage
grep -n 'reverie-' hermit-cli/Cargo.toml

# 5. Trace CLI dispatch and real Detcore construction
rg -n 'run_kvm|run_dbi|detcore::Config|Detcore<' hermit-cli/src/ detcore/src/

# 6. Trace syscall interception, determinization, and possible passthrough
rg -n 'syscall|intercept|passthrough|forward' hermit-cli/src/ detcore/src/ reverie-*/src/

# 7. Try running real programs (if --backend exists)
target/release/hermit run --backend X --strict --verify -- echo hello 2>&1
target/release/hermit run --backend X --strict --verify -- /bin/true 2>&1
target/release/hermit run --backend X --strict --verify -- cat /dev/null 2>&1

# 8. Capture and compare INFO-level syscall handling with ptrace
target/release/hermit --log info run --strict --verify -- echo hello > /tmp/hermit-ptrace.out 2> /tmp/hermit-ptrace.info
target/release/hermit --log info run --backend X --strict --verify -- echo hello > /tmp/hermit-backend-X.out 2> /tmp/hermit-backend-X.info
diff -u /tmp/hermit-ptrace.info /tmp/hermit-backend-X.info
```

## Report Format

After running the audit, produce:

```
BACKEND REALITY AUDIT: [name]
Score: B[0-4]
--backend flag on main: YES/NO
impl Backend: YES/NO
Detcore<XxxGuest>: YES/NO
CLI-to-Detcore code path: [file:line -> symbol -> symbol, or BYPASS]
detcore::Config instantiated by run_X: YES/NO
Syscalls intercepted and determinized: [evidence or PASSTHROUGH]
INFO log parity with ptrace: PASS/FAIL, with differences
Linked in hermit-cli: YES/NO
Programs tested: [list with PASS/FAIL]
Code present and tested on main: YES/NO
GAP TO REAL BACKEND: [numbered list of concrete steps]
```

## Gap Steps Template

The gap report must include SPECIFIC, ORDERED steps like:
1. Implement `impl Backend for XxxBackend` in reverie-xxx/src/backend.rs
2. Add reverie-xxx dependency to hermit-cli/Cargo.toml
3. Add --backend xxx variant to CLI arg parser
4. Implement Guest trait syscall handlers for: [list missing syscalls]
5. Test with ptrace strict-verify corpus
6. Each step = a PR

These steps become tasks in the task graph.
