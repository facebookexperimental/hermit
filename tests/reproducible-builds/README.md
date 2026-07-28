# Reproducible Rust Build Showcase

This fixture uses [`build-time` 0.1.3](https://crates.io/crates/build-time/0.1.3),
a small proc-macro crate that expands `build_time_utc!()` by calling
`chrono::Utc::now()`. The exact dependency version is pinned in both
`Cargo.toml` and `Cargo.lock`.

The runner builds the timestamp-bearing leaf crate twice natively and twice
under Hermit. Native object files differ because they embed host wall time.
Hermit virtualizes that clock, so its two object files are byte-for-byte
identical. It also runs the compiler command with `--strict --verify` to check
the complete execution logs.

```bash
cargo build --release -p hermit --bin hermit
with-proxy ./tests/reproducible-builds/run.sh
```

Set `HERMIT_BIN` to test another Hermit binary. Generated objects remain under
the fixture's ignored `target/reproducible-builds/` directory for inspection.

## Scope

- Backend: ptrace (the default).
- Hermit mode: strict, with no determinism relaxations.
- Assurance: two independent L1 builds with a direct bitwise artifact check,
  plus an L2 `--strict --verify` run of the same compiler command.
- Artifact: an ELF object produced with `rustc --emit=obj`. Avoiding the linker
  keeps the example focused on the compile-time clock input.
- Compiler: Hermit's pinned nightly with `-Z threads=1`, which keeps `rustc`
  compatible with deterministic thread serialization.

The dependency graph is compiled natively once before measurement. The
prebuilt `build-time` proc-macro is then loaded by each measured `rustc`
process, so `build_time_utc!()` itself executes inside Hermit for the Hermit
builds. This separates the timestamp experiment from Cargo's parallel build
orchestration without precomputing or overriding the timestamp.
