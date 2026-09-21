# Per-test Hermit code coverage

`scripts/hermit-code-coverage.rs` measures which Hermit and Detcore source
paths one guest test executes. It is intended for test power-to-weight work:
compare an original test with a smaller candidate and require the candidate to
preserve the original covered-line and covered-region sets.

This is coverage of the **Hermit implementation**, not coverage of the guest
program. The instrumented executable runs the normal Detcore tool through the
selected Reverie backend.

## Prerequisites

Install the coverage driver and the LLVM tools matching the repository's Rust
toolchain:

```sh
rustup component add llvm-tools-preview
cargo install cargo-llvm-cov --locked
```

On a Meta development host, run internet-facing installation commands through
the forward proxy:

```sh
with-proxy cargo install cargo-llvm-cov --locked
with-proxy rustup component add llvm-tools-preview
```

The first collection builds an instrumented debug Hermit. Later collections
reuse that build incrementally. A ptrace collection must run in an environment
that permits same-UID parent/child ptrace, just like an ordinary Hermit ptrace
run.

## Collect two named cases

Run from the Hermit repository root. Everything after `--` is passed to the
instrumented Hermit binary:

```sh
scripts/hermit-code-coverage.rs collect --name echo-original -- \
  --backend ptrace run --strict -- /bin/echo coverage

scripts/hermit-code-coverage.rs collect --name true-shrunk --no-build -- \
  --backend ptrace run --strict -- /bin/true
```

Omit `--no-build` when Hermit source may have changed. The build is
incremental, so this is the safe default. `--no-build` is useful for a batch of
guest-only variants at one exact Hermit revision.

For a shell or test wrapper that launches Hermit itself, use `--command`:

```sh
scripts/hermit-code-coverage.rs collect --name wrapped-case --command -- \
  ./path/to/test-wrapper.sh
```

The wrapper receives both `HERMIT_BIN` and `HERMIT_COVERAGE_BIN`, pointing to
the instrumented executable. Every instrumented Hermit process launched by the
wrapper contributes to the named report.

Each name is immutable: collection refuses to overwrite an existing report
directory. This prevents an accidental rerun from replacing evidence. Choose a
new name for another run.

## Compare original and shrunk coverage

```sh
scripts/hermit-code-coverage.rs diff \
  --baseline echo-original \
  --candidate true-shrunk
```

The diff reports:

- official LLVM covered/total line, region, and function counts;
- the exact covered source lines lost or gained by file;
- the exact covered source regions lost or gained by source coordinates;
- baseline coverage-preservation percentages.

LLVM's official totals may count multiple generic instantiations of one source
location. The normalized diff intentionally deduplicates source coordinates,
so its covered-set size can be smaller than the official covered total.

Use `--fail-on-loss` to make a lost baseline line or region return exit status
1. Additions do not fail the comparison.

```sh
scripts/hermit-code-coverage.rs diff \
  --baseline echo-original \
  --candidate echo-shrunk \
  --fail-on-loss
```

Coverage percentages alone are not the shrink gate. A candidate can keep the
same percentage while exchanging important code paths. The normalized set
diff is the authoritative power-to-weight comparison.

## Report format

Reports are ignored build artifacts under
`target/hermit-code-coverage/<name>/`:

| Path | Contents |
| --- | --- |
| `summary.md` | Bounded human report and per-file line/region totals. |
| `summary.json` | `hermit-code-coverage/v1`, metadata, official totals, and normalized covered-line/region sets. |
| `coverage.json` | Full `llvm-cov export` JSON, including functions and regions. |
| `coverage.lcov` | Full LCOV line report. |
| `coverage.profdata` | Merged LLVM profile. |
| `raw/*.profraw` | Isolated raw profile data for the named run. |
| `run.stdout.log` | Guest/wrapper standard output. |
| `run.stderr.log` | Guest/wrapper standard error. |

Diffs are written to
`target/hermit-code-coverage/diffs/<baseline>-vs-<candidate>.{md,json}`. The
JSON contains the complete lost/gained sets; the Markdown report bounds long
region listings and points to the JSON.

The default source scope is the in-repository implementation roots `common`,
`detcore*`, `hermit-cli`, `hermit-install`, `hermit-resources`, and
`hermit-verify`. Dependency and guest source files remain in the raw LLVM
export but are excluded from the normalized Hermit report.

## Why continuous profiles are required

The ptrace tracer/Detcore execution can terminate through a path that does not
run the ordinary LLVM profile flush. A normal `cargo llvm-cov run` therefore
records the CLI process but can incorrectly report zero coverage for Detcore.

The harness builds with LLVM runtime counter relocation and runs with the `%c`
continuous-profile filename specifier. Counters are backed by the profile file
while the tracer executes, so Detcore coverage survives non-flushing process
termination. Do not replace this with a plain `cargo llvm-cov run` unless the
result has been shown to contain nonzero `detcore/src/` coverage.

Coverage instrumentation adds substantial overhead and uses an unoptimized
debug binary. Use ordinary non-instrumented runs for runtime measurements; use
this harness only to compare exercised implementation paths.
