# SQLite `veryquick` under strict Hermit

This experiment builds SQLite's upstream `testfixture` and runs the real
`test/veryquick.test` suite twice under `hermit run --strict`. Only two
root-only assertions in a fatal guard are omitted so execution can continue;
every other upstream outcome is retained. Current Hermit does not complete the
suite, so the runner records the reproducible compatibility boundary and
checks both runs against it.

## Pinned source

The runner uses SQLite 3.51.2, released 2026-01-09:

- archive: `sqlite-src-3510200.zip`
- URL: <https://www.sqlite.org/2026/sqlite-src-3510200.zip>
- SHA-256: `85110f762d5079414d99dd5d7917bc3ff7e05876e6ccbd13d8496a3817f20829`
- SQLite commit: `b270f8339eb13b504d0b2ba154ebca966b7dde08e40c3ed7d559749818cb2075`

SQLite's smaller `sqlite-autoconf` archive contains the amalgamation and CLI,
but not the test scripts or sources needed by `testfixture`. The official full
source archive is therefore required for this test.

SQLite 3.53.3 was evaluated first but its suite has a host-native failure in
`zipfile-25.0` (`cannot open file: x` versus `error in fread()`). Version 3.51.2
has a clean native baseline and avoids hiding an upstream failure behind a
Hermit-specific expectation.

Hermit maps the calling user to root inside its container namespace so it can
create mounts without host privileges. SQLite's `attach-6.2` and
`attach-6.2.2` expect denial after `chmod 0000`; upstream exits the entire
suite when that assumption is false. The checked-in
[`root-userns.patch`](root-userns.patch) records only those two assertions as
omitted so execution can continue. This is the sole source adaptation. The
runner applies it after verifying the pristine source archive and records its
hash in the metadata.

## Result

The native baseline completed 330,902 assertions with zero errors and no
leaks. It took 114.95 seconds on the recorded host.

Each strict Hermit run exposed the same 13 failures before the stall:
`backup2-6`, three `busy2` cases, six `delete-8` cases, `extension01-1.6`, and
the two `like-14` cases. The exact names are in [`metadata.txt`](metadata.txt).

Both runs then stopped making progress after `lock3.test`; the next upstream
case is `lock4.test`. The recorded guests remained asleep in `select` until
they were stopped after approximately 37 minutes.

Raw stdout differs in SQLite's per-file elapsed-time telemetry. The
canonicalizer removes only complete `Time:` lines and normalizes the embedded
elapsed milliseconds in the two `like-14` diagnostics; expected/actual test
results are unchanged. The canonical streams match with SHA-256
`c69dd97aacfdb56210db135cfc86708bb3af7bf75681fdfd2f05b7312a47ef78`,
and both stderr streams are empty. [`results.tsv`](results.tsv) retains both
raw hashes and the shared semantic hash.

## Prerequisites

The test requires x86-64 Linux, `curl`, `unzip`, `patch`, `make`, a C compiler,
Tcl 8.6 development headers, and an already-built Hermit binary. On
Fedora-family systems, install the build dependency with:

```sh
sudo dnf install tcl-devel
```

The configure flags `--disable-shared --enable-static` statically incorporate
SQLite and its test modules into `testfixture`. System Tcl, zlib, libc, and
other platform libraries remain dynamically linked because common distro
`tcl-devel` packages do not provide a static Tcl archive.

## Run

From the repository root:

```sh
cargo build --release -p hermit --bin hermit
HERMIT_BIN=target/release/hermit \
  ./experiments/sqlite-veryquick/run.sh
```

The 7,200-second hard limit applies separately to each Hermit execution. After
`lock3.test`, the watchdog waits 30 seconds for new output before classifying
and terminating the known `lock4.test` stall. The timeout, stall grace, and
build parallelism are configurable through the `SQLITE_VERYQUICK_*`
environment variables. Downloads, source, the build, and raw output are kept
below `target/sqlite-veryquick/`; set `SQLITE_VERYQUICK_ARTIFACT_ROOT` to
relocate them.

The ignored Cargo integration entry point runs the same script:

```sh
HERMIT_BIN=target/release/hermit \
  cargo test -p hermit --test sqlite_veryquick -- --ignored --nocapture
```

The complete suite uses SQLite's supported `--verbose=0` mode and is ignored
by default because it downloads and builds a complete source release and runs
until the known stall twice. A separate SQLite CLI workload exercises WAL,
transactions, indexing, and integrity checking twice in ordinary CI; it took
0.30 seconds locally.

## Validation contract

For each run, the harness requires all of the following:

- the watchdog identifies the no-progress boundary after `lock3.test`;
- the stopped run has status 143 and the exact 13-failure set;
- stdout matches after only documented timing-telemetry normalization;
- the complete stderr streams are byte-for-byte identical.
- raw stdout hashes are retained even though timing telemetry differs.

It records source, `testfixture`, Hermit, raw and semantic stdout, stderr,
outcome, and exit status in `results.tsv`. `metadata.txt` records the exact
hashes, commands, versions, host, and repository revision. Raw output remains
left in a timestamped directory below the ignored Cargo `target/` tree and is
not committed.

The checked-in [`results.tsv`](results.tsv) and
[`metadata.txt`](metadata.txt) summarize the validation performed for this
change. They are evidence for one pinned release on one host, not a claim that
Hermit supports every SQLite configuration or makes an externally changing
filesystem deterministic.
