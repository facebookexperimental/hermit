# Strict-green authority

`strict-green-authority` is the sole writer of Hermit strict-green records. A
test status, a manifest flag, or two caller-supplied digests cannot mint a
strict green. The authority opens the referenced artifacts, requires each one
to contain bytes, compares the reference and observed bytes itself, and emits
the computed SHA-256 only after the complete claim passes.

The input shape is [`strict-green-authority.schema.json`](strict-green-authority.schema.json).
The Rust verifier enforces the cross-field rules that JSON Schema cannot:

- the claim SHA is an exact 40-hex commit and equals `--expected-sha`; dirty
  source trees are refused;
- `ci_enabled=false`, `exercise=not-exercised`, zero claims, zero executions,
  zero control points, and any empty required artifact are refused;
- `short` means stdout, INFO log, stack, and heap on every run, with
  `memory_cadence=1`;
- `large` means stdout and INFO log on every run, with stack and heap together
  on run 1 and then exactly every declared `memory_cadence` runs. Large cadence
  is at least 2, so it is recorded as the spot-check tier it claims to be;
- the only comparison boundary is `guest-logical-control-v1` at syscall exit.
  Handler-interior state is outside the domain because a backend running its
  handler in the guest legitimately has different memory and instructions;
- every artifact path is relative to one evidence root, cannot escape it, and
  the reference and observed sides must be distinct regular files (not the
  same file, symlink target, or hard link). An artifact also cannot be reused
  as a different surface, run, or cell. Stack, heap, and register artifacts
  must contain their corresponding Hermit observation markers; register lines
  must also name `control_point=syscall-exit` and the declared cadence tier.

Run it with:

```sh
cargo run -p hermit-manifest-plan --bin strict-green-authority -- \
  --claims claims.json \
  --evidence-root ignored/strict-green/RUN_ID \
  --expected-sha "$(git rev-parse HEAD)" \
  --accepted ignored/strict-green/RUN_ID/accepted.json
```

The accepted output is cleared before validation begins. A refused invocation
therefore cannot leave an older green in place. The output records the tier,
exact SHA, execution count, control-point count, cadence, and a byte count plus
authority-computed digest for every compared surface.

## Register-coverage boundary

Register coverage is mandatory metadata but is not silently implied by a
strict green. `not-included` says exactly that no register claim was made.
`bounded-gpr-control-v1` requires nonempty, bitwise-equal register evidence on
every run and records its sampling cadence. Its landed implementation samples
at syscall-exit guest-logical-control points and covers 19 x86-64 integer and
control fields: the guest-observable GPR subset, RIP, RSP, RFLAGS, ORIG_RAX,
FS_BASE, and GS_BASE. It deliberately excludes RCX/R11 and segment selectors.
It does **not** cover FP, SIMD/vector, debug/control registers, non-x86-64
register files, or non-syscall guest-logical-control points. Therefore
`bounded-gpr-control-v1` must never be described as complete register-file
certification.

## Initial ratchet

The landed starting baseline is **0 strict greens**. That is a definition
change, not a backend regression: earlier green artifacts lack this authority's
exact-SHA, nonempty, tier, and complete-surface binding. A cell increments the
ratchet only when this verifier emits its accepted record. In particular, a
`ci=true` edit without an executed evidence set remains not running and earns
nothing.
