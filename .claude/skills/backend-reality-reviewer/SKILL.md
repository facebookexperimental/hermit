---
name: backend-reality-reviewer
description: Audit Hermit backend completion claims against real CLI execution, Reverie Backend implementation, Detcore Tool integration, arbitrary-program support, and hermit-cli linkage. Use whenever a backend agent reports progress or completion, or when hermit-coord evaluates whether a backend claim is real.
---

# Backend Reality Reviewer

## Purpose
This skill is used by hermit-coord to audit backend claims. Every time a backend agent reports completion, run this checklist to determine how "real" the backend is.

## Milestone Completion Gate

**A milestone is NOT DONE until the code is on main.**

- Building on a feature branch = in progress.
- PR opened = in review.
- Merged to main + `hermit run --backend X` works = DONE.
- Never close a backend milestone task for work on an unlanded branch.

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

# 5. Try running a real program (if --backend exists)
target/release/hermit run --backend X --strict --verify -- echo hello 2>&1
target/release/hermit run --backend X --strict --verify -- /bin/true 2>&1
target/release/hermit run --backend X --strict --verify -- cat /dev/null 2>&1
```

## Report Format

After running the audit, produce:

```
BACKEND REALITY AUDIT: [name]
Score: B[0-4]
--backend flag on main: YES/NO
impl Backend: YES/NO
Detcore<XxxGuest>: YES/NO
Linked in hermit-cli: YES/NO
Programs tested: [list with PASS/FAIL]
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
