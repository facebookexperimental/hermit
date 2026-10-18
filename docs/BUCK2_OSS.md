# Reproducing the OSS Buck2 build

The OSS Buck2 project is generated from Hermit's authoritative root
`Cargo.toml` and tracked `Cargo.lock`. Reverie remains a separately pinned
source cell, but both cells resolve third-party Rust crates through Hermit's
single generated graph. Generated Rust `BUCK` files and vendored crate sources
remain ignored because they can be regenerated.

## Prerequisites

- Git, and enough network access to reach `github.com`, `index.crates.io`,
  `static.rust-lang.org`, and the Buck2 GitHub release assets.
- `rustup`. The build compiler is pinned by `rust-toolchain.toml`
  (`nightly-2026-07-12`, rustc 1.99.0-nightly `be8e82435`); rustup installs it
  on first use. `components = ["clippy", "rustfmt"]` is part of that pin
  because rustup does not add them to a freshly installed dated toolchain
  otherwise.
- `flock` (util-linux). The first Reindeer invocation on a cache builds
  under a lock.
- `rust-script` 0.36.0. Dependency regeneration runs one checked-in
  rust-script postprocessor that materializes Reverie DBT's native source tree;
  install the pinned version with
  `cargo install rust-script --version 0.36.0 --locked`.
- The open-source [DotSlash](https://dotslash-cli.com) launcher. **On a Meta
  host, see "On a Meta host" below — the `dotslash` already on `PATH` is a
  different program and will not work.**

## Steps

```sh
git clone --recursive https://github.com/rrnewton/hermit.git
cd hermit
./bootstrap/regenerate-rust-deps
./bootstrap/buck2 build \
    reverie//:reverie-ptrace \
    reverie//:reverie-rpc-transport \
    reverie//:reverie-liteinst \
    reverie//:reverie-kvm
./bootstrap/buck2 build --keep-going shim//third-party/rust/...
./bootstrap/buck2 build //hermit-cli:hermit
```

If you already have a checkout, `git submodule update --init --recursive`
replaces the `--recursive` in the clone.

`//hermit-cli:hermit` is the green gate. The complete generated third-party
target pattern is a diagnostic rather than a gate: it includes optional and
non-host platform targets that the default Hermit binary does not use.

## Shadow release parity

The feature-complete release target is shadow-only: Cargo remains the
authoritative binary and install/resource producer. First build the Cargo
release with the optional backends and the revision stamped; the wrapper
requires the Cargo binary to name the 12-character `HEAD` revision, and only a
stamped build embeds one:

```sh
HERMIT_STAMP_GIT_SHA=1 cargo build --release --workspace --features hermit/third-party-backends
```

Then invoke the guarded wrapper with explicit absolute paths:

```sh
./scripts/build-buck-release.rs \
  --dotslash /absolute/path/to/public/dotslash \
  --cargo-binary "$PWD/target/release/hermit" \
  --install-bundle "$PWD/target/install_pkg" \
  --safehermit /absolute/path/to/dev-hermit/bin/safehermit
```

The wrapper always regenerates dependencies, refuses missing release metadata
inside Rust compilation, resolves Buck through the supplied public DotSlash
launcher, and records hashes for DotSlash, the Buck descriptor, resolved Buck
executable, generated graph, binaries, resources, the `liblzma.so.5` link
input, and the exact libunwind closure. On minimal CI hosts the driver resolves
and validates the runtime SONAME without requiring the unversioned development-package
symlink. It copies the canonical ELF bytes into a content-addressed package
under `ignored/buck2-link-inputs/`, supplies that package as an explicit Buck
dependency, and retains the source path, hash, and target label. Before
building it also snapshots the canonical `libunwind-ptrace.a`,
`libunwind-x86_64.so.8`, and transitive `libunwind.so.8` as one
content-addressed declared Buck package. Their archive/ELF type, SONAME,
DT_NEEDED closure, hashes, copied bytes, package text, and exact no-extra-file
population are reverified. Exactly two DT_NEEDED profiles are accepted, and
both libraries must match the same one: the narrow profile (libc and the
loader beside the libunwind pair, as on CentOS) and the lzma profile, which
adds `liblzma.so.5` to each library for Ubuntu's MiniDebugInfo support. Like
libc, `liblzma.so.5` is a host base library rather than part of the copied
closure; the declared link input above binds that SONAME's build-time bytes,
not the copy the loader resolves at run time. The package text records which
profile matched. The release executable carries only the reviewed
two-component relative DT_RPATH for direct `target/{ci,release}` use and the
published E2E layout; DT_RUNPATH, absolute, empty, and ambient components
refuse. Both shared libraries are immutable regular files under
`rsrcs/hermit-runtime`, are covered by the resource manifest, and are bound in
the typed receipt. Before DotSlash's first probe it snapshots the launcher
into the exclusive evidence tree with stable before/copy/after hashes, uses
only that snapshot, and re-verifies it before the final receipt. Deleting or
changing the caller path cannot change the run. The wrapper likewise snapshots
the DotSlash descriptor and resolved Buck executable, then invokes only the
snapshotted Buck binary for version, build, and log decoding. Before any Cargo
probe it snapshots the caller's binary and complete install bundle into the
exclusive evidence directory. Every subsequent probe, comparison, and receipt
fact uses that tested snapshot, never the mutable caller paths. The
Buck candidate's bundle is published separately from the caller's pre-overlay
install tree, and the two complete resource manifests must be byte-identical,
so any change to that tree between the two publications is refused. Before the first probe the
wrapper also snapshots the
reviewed `ROOT/bin/safehermit` plus its required
`ROOT/scripts/bounded-run-space` companion into the evidence tree, executes
only that copy, and re-verifies both tool hashes before the final receipt. The
named Cargo and Buck version, help, and host-capability outputs are retained;
receipt recomputation typed-decodes both version/host reports, compares the
help bytes exactly, and typed-decodes and compares both candidates' ptrace run
and record reports again. It also replays retained Buck event/stdout/shell-exit
semantics, re-runs the snapshotted Buck log summary, and requires exactly the
ten named candidate invocations with applied wall/cgroup safehermit reports.
The shadow runs no DBT guest. It used to run the official DBT parity matrix
under both binaries and require equal result rows with the timing column
removed; slice S13 of https://github.com/rrnewton/hermit/issues/3301 removed
that leg. Nothing automated ran it: the nightly workflow runs only the
driver's tests and its flag modes, and the validation DAG runs only
`--validate-dag-build` and `--validate-dag-install`. Of the matrix's 28 cases,
26 are CI-enabled DBT verify cells of the c-programs and system-utils
manifests. `io_uring_fallback` is a CI-enabled DBT custom cell (three
`--strict` runs that must repeat identically) while
https://github.com/rrnewton/reverie/issues/764 keeps its DBT verify cell out
of CI. `pthread_lifecycle` stays DBT-disabled; the matrix recorded it as a gap
and never passed it either. The `./scripts/validate.rs --buck-release` opt-in
(see [Validate DAG opt-in](#validate-dag-opt-in)) runs every E2E cell, those
DBT cells included, against the Buck release binary. The final receipt schema
is `hermit-buck-shadow-parity/v3`; a v2 receipt, which carried the matrix
facts, is refused.
The generated Buck graph has stable hashes immediately before/after the build
and at receipt time. The canonical host liblzma input remains guarded the same
way, while the link action consumes the separately verified content-addressed
copy as a declared source input. The final receipt has a closed
typed schema: unknown, duplicate, missing, mutated, or non-recomputable semantic
facts refuse. It publishes separate Cargo and Buck bundles only under
`ignored/buck2-phase1/`; it never writes the authoritative
`target/ci/hermit-strict` or E2E artifact pointer. A pass requires equal build
records and backend inventories, compatible ELF contracts, and canonical
ptrace strict-verify and record/replay evidence through `safehermit`. Buck
event evidence is retained as `.json-lines.gz` and must decode through
`buck2 log summary` before the wrapper can pass.

### Virtual-time scope

The fixed-epoch clock trajectory gate establishes Cargo-versus-Buck parity for
the ptrace backend only. It runs `tests/c/clock_exec_continuity.c` under
`--backend ptrace` with a pinned `--epoch` and timeslice, and requires
byte-identical trajectories and exactly equal strict reports, virtual time
included. It is not evidence of virtual-time parity between backends, and it
says nothing about DBT or KVM virtual time:

- The shadow runs no DBT guest. In validation, the DBT verify cell
  `system-utils/clock-determinism` runs the simpler
  `tests/c/clock_determinism.c` fixture as a repeatability contract within the
  DBT backend.
- The shadow runs no KVM guest. In validation, the KVM verify cell
  `system-utils/proc-uptime` reads `/proc/uptime` and checks repeatability. It
  does not exercise `sysinfo(2)`, exec or thread continuity, or an exact
  trajectory.
- The `system-utils/clock-exec-continuity` manifest cell enables verify mode
  on ptrace only. DBT and KVM stay disabled until their post-exec clock paths
  are qualified against the ptrace baseline.

A cross-backend virtual-time claim needs that coverage first: the same pinned
trajectory run on each backend, with a comparator that defines what must be
equal between backends.

## Public nightly evidence and the full-parity boundary

`.github/workflows/buck2-oss-nightly.yml` is schedule/manual only and has
read-only repository permission. The original 120-minute `oss-buck2` job still
owns dependency regeneration, the four Reverie targets, the third-party
diagnostic, and `//hermit-cli:hermit`. A separate independent supplemental job
installs `rust-script` into an isolated `CARGO_HOME`, binds `PATH` to that exact
executable, runs the release driver's unit/refusal tests, and builds
`//hermit-cli:hermit-release` with explicit version, date, 12-character Hermit
SHA, and 40-character Reverie pin metadata.
The step retains a recognized `.json-lines.gz` Buck event log, decodes its
summary, and reconciles it with the retained `--show-output` stdout. The
reviewed success contract is: shell exit zero; exactly one completed CommandEnd;
exactly one Result with zero errors and the canonical release target; and
exactly one executable output path named by stdout whose canonical realpath is
under the checkout's `buck-out/`, ends in the measured configured-target layout
`hermit-cli/___hermit-release__/hermit`, and is an executable x86_64 ELF.
Every Buck build these tools run passes `--no-remote-cache`, and the checkout
configures no remote execution. The reconciler also counts each executed
action's `execution_kind` from the event log. It accepts only the local kinds
(`local`, `simple`, `deferred`, `local_dep_file`, `local_worker`,
`local_action_cache`) and refuses any remote, remote-cache, or unknown kind.
The per-kind counts are recorded in the receipt, so a daemon that reused
every result (no actions) is distinguishable from a local rebuild.
The census is corroboration, not the guarantee. A result the daemon reuses from
memory emits no action event at all. A `local_dep_file` or `local_action_cache`
hit can serve an output whose cached entry records it as produced remotely
(`was_produced_locally = false`). "Built locally" therefore rests on
`--no-remote-cache` and on the absence of any remote-execution configuration.
The event census confirms that nothing it can see contradicts this.
Measured at Hermit commit 10906a288 with the pinned Buck2, every build passing
`--no-remote-cache`:

| Build | Actions executed | Wall time |
| --- | --- | --- |
| Slot checkout, first build at a new stamp value | 2,213 (535 `local`, 373 `local_action_cache`, 1,305 `simple`) | 202 s |
| Same checkout, repeated twice | 0 each | about 0.1 s each |
| Same checkout, two concurrent builds after a stamp change | 12 and 0 | 68 s each |
| Fresh second checkout of the same tree | 2,220 (909 `local`, 1,311 `simple`) | 229 s |

No remote kind appeared in any build. Local reuse works within one checkout,
including between concurrent builds. Nothing is reused across checkouts,
because each checkout has its own `buck-out/` and daemon.
Before upload, a separate 10-second process-group-bounded CLI-only probe decodes
that candidate's typed `version --json` and requires the exact metadata and
`dbt`/`e9patch`/`sabre` feature facts. This rejects an executable decoy without
making a guest-execution or behavioral-parity claim.
Measured pinned-Buck logs serialize `CommandEnd.is_success=false` even for
successful builds: 21 retained builds that completed with zero errors and 1
that failed with one error all carried `false`. The field is retained and
explicitly labeled `advisory-known-inconsistent`; it is neither hidden nor used
to override the canonical Result/shell/output evidence. That reading is bound
to the measured Buck2 descriptor (`bootstrap/buck2` SHA-256
`40e4842f407f589acf80a40267764bea914dc4067b2faf330cc1c377080db35e`): under any
other descriptor a `false` flag is refused until the flag is re-measured. One portable Rust operation examines
every prospective regular artifact file through one stable read that returns
the raw bytes, filesystem identity, and digest. Event logs are decompressed
from those same captured raw bytes; the receipt and `SHA256SUMS` are derived
from that same map, and a deterministic pinned `tar`/`flate2` writer creates
the archive directly from the captured bytes. The operation decodes that
in-memory archive, requires exact member bytes and checksum semantics,
re-enumerates the source population, stable-reverifies every original
identity/hash, and only then atomically publishes the read-only archive and
sidecar receipt. There is no scanner-to-packager process boundary and no source
payload reread during packaging. Symlinks, special files, unsafe/non-UTF-8
names, credential markers, coherent manifest rewrites, and late files all
refuse. Non-green build/event evidence is still scanned and
packaged before the supplemental step propagates failure; scanner refusal
suppresses upload. `safe-to-upload` means secret-scan and artifact safety only,
never parity or pass; the typed evidence verdict controls step success. Shell
steps explicitly blank known token carriers, while
upload actions retain their Actions runtime authentication. The legacy
third-party artifact name and downloaded top-level `buck2-third-party.log`
payload are preserved. A second combined mode stable-reads and scans the log,
then materializes the exact historical two-file staging population (captured
`buck2-third-party.log` plus `SHA256SUMS`) directly from those bytes, verifies
the staged digest internally, and re-verifies the original identity before
success. No shell or second process reads a mutable scanner receipt/digest.
Only after that check may the legacy step print its final 200 diagnostic lines,
preserving the historical log-output contract without printing unscanned content. The
immediate upload is a trusted Actions-step handoff: it assumes no malicious
same-user process rewrites read-only runner files between the final verification
and `upload-artifact`; the helper does not claim immutable consumption across
that process boundary.
The workflow checker exact-binds reviewed critical blocks. Its FNV hashes are
drift tripwires only; semantic validation and exhaustive mutation tests remain
the enforcement logic.
This supplemental job is release-build evidence only, not Cargo/Buck
behavioral-parity evidence and not a speedup claim.

A separate public full-parity job may call `build-buck-release.rs` only when
all of the following are available without repository secrets:

- the authoritative same-SHA Cargo release binary and complete
  `target/install_pkg` bundle;
- the public pinned DotSlash launcher and a source-controlled or
  content-addressed, independently reviewable public `safehermit` plus
  `bounded-run-space` execution bundle;
- delegated cgroup v2 controls needed by `safehermit` and `dagrun`, a real
  btrfs filesystem for the isolation contract, and accessible PMU retired-
  branch counters required by strict Hermit execution; and
- read-only repository permissions, public-network-only dependencies, a
  credential-marker scan before artifact publication, and no authoritative
  pointer or `target/ci/hermit-strict` writes.

That job is not currently enabled. There is no reviewed public
`safehermit`/`bounded-run-space` bundle; both currently live only in the private
parent workspace. GitHub-hosted Ubuntu runners also do not promise the required
btrfs mount, delegated cgroup controls, or PMU access. Copying private launcher
code into this repository, adding a PAT, silently substituting `timeout`, or
skipping hardware/isolation checks would weaken the validation contract.

Once an owner-reviewed public execution bundle is published with immutable
hashes, the supplemental job is mechanically runnable on a public self-hosted
runner labeled for x86_64 Linux, PMU access, btrfs quotas, and delegated cgroup
v2. It should use only `workflow_dispatch`/the reviewed schedule, retain
`permissions: contents: read`, set `persist-credentials: false`, verify the
bundle hashes before use, build the authoritative same-SHA Cargo artifacts,
and invoke the wrapper with absolute paths. Its first step must fail closed
with a retained prerequisite report unless PMU, btrfs quota/readback, cgroup
delegation, and passwordless bounded-run-space cleanup all work. Its last step
must run the same single-process captured-byte scan-and-package operation before
upload. Absence of a
runner or public bundle is `blocked`, never parity-green.

## Validate DAG opt-in

Ordinary validation keeps Cargo as its default release builder. Phase two adds
one explicit opt-in for the complete `full`, `portable-only`, and
`hosted-portable` plans:

```sh
./scripts/validate.rs portable-only \
  --buck-release /absolute/path/to/public/dotslash
```

The normal dev-hermit admission boundary still applies; this example documents
driver arguments rather than bypassing `ci-hub`. Buck mode is never inferred
from ambient environment.

A Buck-mode run tests a different binary. In Cargo mode every E2E cell runs
the debug `target/debug/hermit`, which has debug assertions and overflow
checks. In Buck mode the cells run the release `target/ci/hermit-strict`,
built with `-Cdebug-assertions=no -Coverflow-checks=no -Copt-level=3`. The
ledger row records this as `release_builder` (`cargo` or `buck`) and
`e2e_payload`, which gives the path, profile, and both check settings.

Every consumer reads `release_builder` and `e2e_payload` together. A row that
carries both keys names a builder only when the builder is `cargo` or `buck`
and `e2e_payload` equals exactly that builder's identity. A row that carries
neither key is a Cargo run only when it predates the pair: its
`schema_version` is an integer in 1 through 7 or 10, and it is either a
Reverie row (`repo` is `reverie` or `rrnewton/reverie`), which needs no date,
or a Hermit row (`repo` is `hermit`, `rrnewton/hermit`, absent or null) whose
`finished_at` is a valid `YYYY-MM-DDTHH:MM:SSZ` instant before
`2026-09-25T20:42:29Z`. Every other row, including one that carries only one
of the two keys or a null in either, names no builder, so it counts as neither
Cargo nor Buck evidence. `e2e_payload` is
still a label, not a measurement of the binary: it is a constant per builder,
and unit tests tie it to the checked-in sources that select the payload (the
release flag list in `shim/BUCK`, no check-class setting in `hermit-cli/BUCK`
or anywhere in the root `Cargo.toml`, no repository `.cargo/config`, and no
`RUSTFLAGS` or `CARGO_PROFILE_` in the validate DAG). It cannot see `RUSTFLAGS`,
`CARGO_ENCODED_RUSTFLAGS` or `CARGO_PROFILE_DEV_*` set in the environment, or
a Cargo config in an ancestor directory or `CARGO_HOME`.

A Buck row is supplemental evidence only:

- It is never a cache hit for a Cargo request.
- A Buck request is never answered from the cache.
- Hermit's own receipt publication refuses it.
- Hermit neither appends its cells to the parent's compatibility series nor
  writes them back to the scorecard locally. When `ci-hub` launched the run,
  the parent's cell-ledger mirror refuses the finalized publication before
  any scorecard work, so the cells are not projected into the published
  scorecard either. They remain in the run's ledger row and retained
  artifacts.
- The parent reads the shared ledger through builder-aware consumers: the
  qualifying-receipt predicate requires the row to read as `cargo` under that
  same rule;
  failure obligations let a Buck red latch but never let Buck clean runs
  discharge one; the compatibility website excludes Buck rows under a named
  reason; timing baselines skip them. A parent without these would count a
  Buck `full` row as a full green, so the parent change lands before this
  one.

A red Buck row on the same tree still blocks Cargo cache reuse, because a
failure is a failure whichever builder produced the binary.
A missing launcher, malformed provenance, failed regeneration or build,
unreadable event log, unexpected target/output, non-x86_64 executable, or hash
mismatch refuses the run. None falls back to Cargo.

The network-capable host producer builds `//hermit-cli:hermit-release`, retains
its event/stdout/stderr and typed reconciliation under
`ignored/buck2-phase2/`, and publishes one content-addressed binary there. Both
the host and network-disabled pinned-root release nodes install that exact
verified binary at `target/ci/hermit-strict` and `target/release/hermit`, plus
the verified libunwind runtime closure at
`target/install_pkg/rsrcs/hermit-runtime`. Direct and published-bundle loader
probes explicitly remove `LD_LIBRARY_PATH`; the executable must resolve the
same content-bound libraries through its relative DT_RPATH. Direct release
consumers use the strict path; E2E consumers receive the same bytes and runtime
closure through the existing verified binary-plus-resource publisher. Cargo
still supplies the remaining complete backend resource bundle in both modes.
A runtime-only bundle's `install/` holds just that closure, so
`ci/run-with-hermit-e2e-artifact.sh --require-install` admits only a verified
`complete` kind rather than any bundle with an `install/` directory. The
publisher and verifier inventory every entry of a runtime closure, including
directories and special files, and refuse any special file in a complete
bundle, which its regular-file manifest could not bind.

### Buck as the E2E runner

A second, independent opt-in changes who runs the E2E cells, not which binary
they test:

```sh
./scripts/validate.rs full --e2e-runner buck-hybrid \
  --buck2 ~/.config/hermit/buck2.dotslash
```

`--e2e-runner` takes `cargo` (the default), `buck-local` (every cell runs on
this host) or `buck-hybrid` (cells routed to remote execution run there, the
rest locally). A Buck runner is accepted only for the complete `full` level,
never with `--only`, `--selected` or `--buck-release`, and only with `--buck2`
naming an absolute, executable, non-symlink file. That file is the host's own
Buck2 launcher. Remote execution needs an internal Buck2 build, whose DotSlash
descriptor is **never committed** to this repository: keep it outside the
checkout (for example `~/.config/hermit/buck2.dotslash`) and pass its path.
Nothing is inferred from ambient environment, and nothing falls back to Cargo.

The plan is the committed `full` plan with the `full-buck-e2e` label swapped
in (`buck_e2e_selection` in `ci/manifest-plan/src/validation_dag.rs`): the 16
Cargo E2E bucket nodes, the compatibility scorecard and the five nodes that
only fed them leave the plan, and `e2e.buck_stage`, `e2e.buck_cells`, one
`<bucket>_buck` import twin per bucket and `full-scorecard.compatibility_buck`
take their place. `e2e.buck_stage` (`ci/buck-e2e/validate-node --stage-only`)
builds the inputs Buck does not build yet with Cargo
(`ci/buck-e2e/stage --from-cargo`). It depends only on `pre.reverie_pin`, so it
runs beside `build.rust_scripts` and `setup.manifest_plan` instead of after
them; each uses its own Cargo profile directory. `e2e.buck_cells`
(`ci/buck-e2e/validate-node --cells-only`) waits for it, refuses before any
step unless `ci/buck-e2e/staged/SOURCE_SHA` names the checkout's `HEAD`,
regenerates the third-party rules, stages the remote-execution inputs for
`buck-hybrid`, and runs every cell with `-c hermit_e2e.hermit=staged`. Run
without an argument, `ci/buck-e2e/validate-node` does both in one process, as
before. Each twin then judges its bucket's rows with the same
`test-harness run` verdict as the Cargo bucket, reading them through
`E2E_IMPORT_RESULTS`.

One ordering difference follows. In the Cargo plan, eight nodes run before the
E2E buckets, directly or through `compatprep.fixtures`: `check.dbt_runtime_abi`,
`doc.doctests`, `doc.rustdoc`, `lint.clippy`, `test.detcore_unit`,
`test.hermit_unit`, `test.regular_crates` and `test.rr_suite_contract`. In the
Buck plan nothing waits for them. They still run, and a failure still fails the
validation, but the Buck cells no longer wait on them, so a failure among them
no longer stops the cells early.

The cells therefore test the Cargo-built validate-profile
`target/validate/hermit`: `ci/buck-e2e/stage --from-cargo` builds it on the
host in the checkout's `target/stage-hermit/`, beside its other Cargo builds,
and copies the binary to `target/validate/hermit`. It has the same features,
debug assertions and overflow checks as a Cargo-runner build; it differs only
in the DynamoRIO build paths `reverie-dbt` records as a fallback, since Hermit
finds that runtime through `install_pkg`. The ledger row records `release_builder: cargo`,
the Cargo `e2e_payload` identity unchanged, and `e2e_runner` (`cargo`,
`buck-local` or `buck-hybrid`). A Buck-runner request is never answered from the
tree cache, and a Buck-runner row never answers a cargo request: the cache reads
`e2e_runner` as well as the payload. The host prerequisites below apply, and
`HERMIT_GIT_DEP_MIRRORS` must be set in the validation's environment when the
proxy refuses GitHub to Reindeer.

## Host prerequisites for Buck validation

The Buck E2E flow (`shim/modes/stage-re-inputs`, `ci/buck-e2e/stage`, then
`ci/buck-e2e/run`) copies some host libraries and tools into its inputs rather
than building them. On CentOS Stream 9 or Fedora:

```sh
sudo dnf install -y libunwind-devel xz-devel cmake patchelf binutils podman
```

| Package | Needed by | For |
|---|---|---|
| `libunwind-devel` | `stage-re-inputs`, `stage` | the libunwind link inputs remote actions link against, and the runtime closure shipped beside the staged `hermit` |
| `xz-devel` | `stage-re-inputs` | `liblzma`, which `//hermit-cli:hermit-release` links on remote execution |
| `cmake` | `stage` | the host Cargo build of `reverie-dbt`, which builds DynamoRIO (remote actions use the pinned cmake wheel `stage-re-inputs` stages instead) |
| `patchelf` | `stage` | the `DT_RPATH` through which the staged `hermit` finds the libunwind closure beside it |
| `binutils` | `stage` | `strip` for the staged harness, and `readelf`, with which `ci/publish-hermit-e2e-artifact.sh` checks the staged `hermit`'s runtime closure |
| `podman` | `ci/buck-e2e/cell.sh` | privileged-lane, pinned-root-only and local DBT cells, which run inside `ci/hermetic/run-in-pinned-root.sh` exactly as the Cargo flow runs them |

Those cells also need the pinned root image: build it once with
`ci/hermetic/build-image.sh`. Without it they fail with `pinned-root image
unavailable`; they never fall back to running on the host. A missing package
stops `stage-re-inputs` or `stage` before anything is built, with a message
naming the package; nothing downloads a substitute. `stage-re-inputs`
downloads its pinned cmake wheel from `files.pythonhosted.org`, so on a Meta
host run it under `with-proxy`.

## On a Meta host

A Meta devserver needs the following, which the steps above do not mention. All
are host facts rather than repository defects; a machine with direct internet
access and no internal `dotslash` needs none of them.

**Every network-touching command needs `with-proxy`** — the clone, the
crates.io index fetch, the Buck2 release download, and any rustup toolchain
install.

**`/usr/bin/dotslash` is the internal DotSlash2 and cannot read this
descriptor.** `./bootstrap/buck2` fails with:

```
dotslash error: problem with .../bootstrap/buck2
caused by: failed to parse DotSlash file
caused by: missing field `scheme`
```

That is a launcher-dialect difference, not a defect in the pin. Internal
descriptors carry a per-platform `scheme` field (for example `"scheme": "cas"`)
and slash-form platform keys (`linux/x86_64`); the public schema has neither,
using `providers[].url` and hyphen-form keys (`linux-x86_64`). Fetch a public
launcher and invoke it explicitly rather than putting it on `PATH`, so nothing
internal is shadowed:

Unpack it **outside** the checkout, so it does not show up as untracked files:

```sh
mkdir -p ~/.local/dotslash && cd ~/.local/dotslash
with-proxy curl -sSL -o ds.tgz \
  https://github.com/facebook/dotslash/releases/download/v0.5.9/dotslash-linux-musl.x86_64.v0.5.9.tar.gz
tar xzf ds.tgz && rm ds.tgz     # yields ~/.local/dotslash/dotslash
cd -
with-proxy ~/.local/dotslash/dotslash ./bootstrap/buck2 build reverie//:reverie-ptrace
```

**`regenerate-rust-deps` needs two Cargo environment variables.** Without the
first, the pinned Reindeer's bundled libcurl does not find the system CA bundle
and fails with `[60] SSL peer certificate ... unable to get local issuer
certificate`, even though system `curl` reaches `index.crates.io` normally.
Without the second it fails with `[7] CONNECT tunnel failed, response 407`,
because the host's `~/.cargo/config.toml` sets `proxy = "fwdproxy:8080"` with
no URL scheme:

```sh
CARGO_HTTP_CAINFO=/etc/pki/tls/certs/ca-bundle.crt \
CARGO_HTTP_PROXY=http://fwdproxy:8080 \
  ./bootstrap/regenerate-rust-deps
```

**A proxy that refuses `github.com` to the build needs local git mirrors.**
`Cargo.lock` locks some crates to git commits (`rust-shed`, `liteinst2` and
`reverie` today), and Reindeer's Cargo fetches them from GitHub even when the
host's `~/.cargo/git` already holds them. If the proxy refuses that fetch
(`CONNECT tunnel failed, response 403`), mirror each source with whatever
route does reach GitHub, then point `HERMIT_GIT_DEP_MIRRORS` at the directory:

```sh
mirrors=~/.cache/hermit-git-mirrors
mkdir -p "$mirrors"
for url in $(sed -n 's/^source = "git+\([^?#"]*\).*/\1/p' Cargo.lock | sort -u); do
  name=${url##*/}
  with-proxy git clone --mirror "$url" "$mirrors/${name%.git}.git"
done
HERMIT_GIT_DEP_MIRRORS=$mirrors \
CARGO_HTTP_CAINFO=/etc/pki/tls/certs/ca-bundle.crt \
CARGO_HTTP_PROXY=http://fwdproxy:8080 \
  ./bootstrap/regenerate-rust-deps
```

Each mirror is named after its URL's last component without `.git`, plus
`.git`. Once the variable is set, every git source must come from it:
`regenerate-rust-deps` refuses a missing mirror, or one that lacks a commit
`Cargo.lock` pins, before Reindeer runs, and prints the `git clone` or `git
fetch` that fixes it. Because each source is pinned to a commit, a mirror
changes only where the objects come from, never what is built. After a
`Cargo.lock` change, `git -C <mirror> fetch` brings a mirror up to date.

## Pinned versions

The wrappers use immutable versions rather than live branch tips:

- Buck2 release `2026-08-01`, through Buck2's upstream DotSlash descriptor with
  a BLAKE3 digest and size for each supported platform. The descriptor's size
  and digest describe the compressed `.zst` artifact, not the decompressed
  binary in the cache — those two numbers differing is expected.
- Reindeer `e3d72748131d3a70378055f091e0647c1edad85e`
- Reindeer's own Rust toolchain `nightly-2026-05-22`
- The build compiler, `nightly-2026-07-12` in `rust-toolchain.toml`

The compiler pin matters as much as the others. The shim uses
`system_rust_toolchain`, which runs whatever `rustc` is on `PATH`; under rustup
that resolves through `rust-toolchain.toml`. While that file said `nightly`, a
reviewer building on a different day got a different compiler, which left every
other pin here without effect.

Note that `reverie/rust-toolchain.toml` may select a different compiler for
standalone Cargo work. Under Buck2 this is inert — actions run from the outer
project root, so the Hermit pin governs the whole build.

The first Reindeer invocation downloads the pinned source revision, installs the
pinned Rust toolchain if needed, and compiles Reindeer into the user cache
(about 1m25s cold). Set `HERMIT_BUCK2_TOOL_CACHE` to place that cache
elsewhere. Each build is named by the tool revision, the toolchain and the
build recipe in `bootstrap/run-pinned-tool`: its source checkout, target
directory and lock all carry that name. Concurrent first invocations of one
build compile it once under the lock, and a checkout left by an interrupted
build is started again. The build ignores the caller's `RUSTFLAGS`,
`RUSTC_WRAPPER` and `CARGO_PROFILE_*` settings, which are not part of the name.
`HERMIT_BUCK2_SHARED_TOOL_CACHE` names a cache shared with other checkouts,
such as one per validation host, and takes precedence over
`HERMIT_BUCK2_TOOL_CACHE`; older checkouts, which lack the lock, ignore it. A
shared cache trusts every checkout that writes to it: a hit runs the cached
binary without checking it. Nothing is evicted; each build takes about 1 GB.

DotSlash downloads and verifies the platform-specific Buck2 release
binary; `DOTSLASH_CACHE` relocates its cache.

`regenerate-rust-deps` starts without generated dependency output, vendors the
versions in the tracked root `Cargo.lock`, generates
`shim/third-party/rust/BUCK` twice, and refuses the result if two consecutive
outputs differ. It also refuses changes to the lockfile. Both Hermit and
Reverie consume this graph. The repository-root `.gitignore` excludes generated
paths. Those patterns must not move into `shim/.gitignore`: pinned Reindeer
reads ignore files through the shim cell root and would otherwise generate
empty crates.

What Reindeer generates (the vendored crates, its BUCK before the
postprocessing, and the vendored-sources `.cargo/config.toml`) is kept in a
cache entry named by everything Reindeer reads. `bootstrap/rust-deps-cache-key.rs`
computes the name and lists what it covers: `Cargo.lock` (which pins git
dependencies by commit), every `Cargo.toml` Git lists and each workspace member
manifest `cargo metadata` names, ignored or not, the toolchain file and cargo
version, the compiler Reindeer queries (`RUSTC`, else `rustc`) with its
`-vV` output and its `--print=cfg` output for each platform target in
`reindeer.toml`, every file under `shim/third-party/rust` whether Git ignores it
or not and following links (apart from the generated `vendor`, `BUCK` and
`.cargo` and Cargo's `registry`, `git`, `target` and `.package-cache`), the
ignore files on the way to them, and the Reindeer pin and scripts. It reads
`reindeer.toml` and the `cargo metadata` output with TOML and JSON parsers, so
quoting and escapes do not hide a setting or a path from it. A `reindeer.toml`
that makes Reindeer read something the key does not cover (`fixups_dir`,
`cargo.cargo`, `cargo.rustc`, `vendor.gitignore_checksum_exclude`, or a key the
helper does not know) or that defines no platform table (Reindeer then uses
built-in platforms) is not cached: the run warns and vendors. So is a checkout
whose key cannot be computed, because a command fails, an input cannot be read
or something does not parse: the run warns, vendors and stores nothing.

A checkout whose inputs match an earlier run's restores the entry instead of
vendoring, about 2.5 s instead of 26 s with local mirrors (the key itself takes
about 0.3 s), and then runs the postprocessing and its comparison as a
generated checkout does. The two-run comparison of Reindeer's own output ran
when the entry was generated. Output is stored only when `Cargo.lock` is
unchanged after Reindeer ran and the key computed again then matches the one
computed before, so a run that rewrites the lock or whose inputs change while
it runs stores nothing, and so does one whose inputs can no longer be read. An
entry is built in a staging directory, published and evicted by renaming it in
and out of place under the cache lock, and a restore requires its raw BUCK, so
an interrupted publish or eviction leaves nothing a restore accepts. A publish
holds a lock on its staging directory until it is renamed in, and a later
publish removes only staging whose lock nobody holds. The cache
lives at `rust-deps/` under the Buck2 tool cache root above, so
`HERMIT_BUCK2_SHARED_TOOL_CACHE` shares it across a validation host's
checkouts; it trusts its writers as that cache does. Each entry takes about
400 MB and the newest `HERMIT_RUST_DEPS_CACHE_KEEP` (default 3) by last use are
kept. `HERMIT_RUST_DEPS_CACHE=off` vendors every time; the release build sets
it. Cargo's download caches under `shim/third-party/rust/.cargo` are removed
after vendoring, so a generated and a restored checkout hold the same files.

## What a reproduction should produce

Measured 2026-09-20 on x86_64 Linux with warm tool and crate caches:

| Step | Result |
|---|---|
| `regenerate-rust-deps` | exit 0, 300 crates; `Cargo.lock` SHA-256 `02e7643d7a5e8339bad982f9443d1289a02197bc0a8b130d2a8c5b37d686b0e2`; generated `BUCK` SHA-256 `4a73386cc01c33b308f8b915a2757b57dfd37912251895da50749f9fbbe31b7d` |
| build Reverie's ptrace, RPC transport, LiteInst, and KVM libraries together | exit 0 |
| `build //hermit-cli:hermit` | exit 0 |

The two cells do not compile separate copies of third-party crates.
`.buckconfig` maps the `reverie_shim` cell onto the Hermit shim cell, so
Reverie's BUCK files resolve their third-party crates through the same generated
targets as Hermit. `shared-cell-aliases.txt` provides the unversioned names used
by Reverie's hand-written BUCK targets.

No shared action-cache performance measurement has yet been made. A local
successful build proves target compatibility only, not the vision's claimed
cross-worktree benefit.
