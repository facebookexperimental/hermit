# Hermetic validate - pinned root

The canonical validate driver stays on the host and runs its build and test DAG
nodes in the pinned root. This directory contains that runner, its locked fetch
phase, and the v3 per-cell isolation contract. The older whole-split invocation
remains available as an explicit diagnostic path.

## What this is

A nix-built OCI image that pins the toolchain, the exact `rust-script 0.36.0`
and `cargo-nextest 0.9.100` developer tools used by portable CI, and every
system executable a required portable manifest cell runs as a hermit guest,
plus a runner that executes a command inside it. The guest-tool inventory was
audited from the selected portable population, including commands reached
through shell fixtures rather than only top-level `program` entries.
The existing outer `systemd-run`, `validate-lock`, DAG identities, accounting,
and cgroup policy stay on the host. Each wrapped build or test node runs in a
privileged podman container pinned by digest, with `/dev/kvm` passed through
when present, no runtime network, and source at `/src`. The wrapper defaults
that source bind to read-only; validation nodes explicitly make it writable.
Output and target volumes are separate and writable. `--cgroups=disabled` keeps
the container and its monitor inside the caller's resource group, so the outer
node accounts for and terminates their descendants. The wrapper does not add
an independent container resource policy.

```
ci/hermetic/build-image.sh                     # build from the lock, load, record the digest
ci/hermetic/run-in-pinned-root.sh --check-image # read-only exact local image check
ci/hermetic/run-in-pinned-root.sh --src DIR --out DIR -- CMD...
ci/hermetic/run-split-validate.sh --fetch-only # canonical driver's locked fetch node
ci/hermetic/run-split-validate.sh              # explicit whole-split diagnostic path
ci/hermetic/assert-no-network.sh               # the boundary check, with a negative control
ci/hermetic/assert-build-dependencies.sh       # the executable build-dependency check
```

Before executing a selected plan that consumes the pinned root, the driver
checks the committed exact image reference in the caller's Podman store. This
happens after cache selection: plan-only, cache-hit and host-only selections do
not inspect images. A missing image or unavailable inspection refuses admission
as `COULD_NOT_RUN`, with zero executed nodes/tests and no passing receipt. The
raw probe status and stderr remain visible; store errors are distinct from
Podman's absent-image status 1. Inspection has a 10-second bound and a 2-second
forced-stop grace. Admission neither builds nor loads an image.

`setup.pinned_root_fetch` fetches Cargo dependencies; it does not prepare the OCI
image. Prepare that image explicitly with `build-image.sh` when needed. Each
node repeats that exact-image check immediately before execution, including the
10-second query bound and 2-second forced-stop grace. Both checks require GNU
`timeout` with `--verbose` support. A timed-out query returns status 124 (or 137
when forced to stop); an unavailable inspection tool retains its raw status,
such as 127 for a missing command. These late failures remain ordinary recorded
node failures. An early presence observation does not reserve the image or
explain later image loss.

Before each canonical pinned-root DAG node starts, the wrapper checks the
network boundary and four different dependency populations rather than folding
them into one tool list: 18 executable build
dependencies, four native library packages with their development headers, 24
commands selected portable cells run as hermit guests, and 11 literal FHS paths
those cells name. Some commands occur in both executable sets because they are
used in both roles. `xxd` occurs only in the build set: e9patch runs `xxd -i`
while generating two C sources, but no selected portable cell runs it as a
guest. The assertion also checks the exact headers and libraries consumed from
`nativeLibs`, so their absence is named before compilation begins.

## V3 per-cell execution contract

Marked namespace-capable Hermit runs use a fresh private `tmpfs` at `/test`
and start the guest there. The marker is exactly
`HERMIT_E2E_EMPTY_WORKDIR=/test`; another value refuses before guest launch.
The outer Podman root supplies the mountpoint. Ptrace/KVM run, verify, replay,
chaos and custom command builders pass their existing mount/workdir options.
DBT accepts the working directory but continues to reject general mount/bind
options: its physical-run adapter creates a scoped host thread, makes a new
private mount namespace and `/test`, and only then creates that run's runtime.
Two executions inside one verify command therefore get separate filesystems.
The marked in-process Detcore test helper uses the same thread boundary before
constructing its tracer. Setup errors remain errors, and no user-namespace
fallback hides a missing mount capability.

These boundaries keep relative scratch names separate between runs. `/src`
stays writable and shared for explicit build products and repository fixtures.
Manifest repository inputs are resolved to absolute paths before the marked
cwd changes; ordinary unmarked DBT arguments retain their literal spelling.
The make reproducibility test exposes its two distinct project directories at
their original absolute guest paths and uses `make -C`; it does not replace both
build-path stimuli with `/test` or populate the empty execution root.

Naked manifest cells still refuse this marker. Tests whose subject is
`--no-namespace` retain that subject and use the outer node's `/test`; they do
not establish per-physical-run filesystem isolation. The scoped DBT adapter
joins its runtime workers and consumes normal global cleanup before returning.
The existing Reverie stdin pump is detached: a blocked source can retain the
ended namespace until input unblocks or the CLI exits. Prompt teardown of the
existing unsupported-syscall case does not prove every such pump has joined.

The same path uses Hermit's existing `--base-env=minimal` semantics rather than
defining another environment. On top of that base the harness supplies
deterministic `LC_ALL` and `TZ`, keeps `HOME` and `XDG_CONFIG_HOME` in unique
per-cell directories, points `E2E_TMPDIR` at the private `/test`, and forwards
the explicit fixture and scheduler inputs. No value is inherited from the
launching shell. A raw guest reading `getenv("PWD")` or `getenv("OLDPWD")`
receives null; a shell derives `PWD=/test` from `getcwd(2)`. No cell requires
inherited `PWD`. Record and replay now accept the same base-environment, mount,
and working-directory controls as `hermit run`, so replay cells obey the
identical contract.

The mount mechanism has a standalone control measurement, not an integrated
Hermit-harness result. Two hundred live
children produced 200 distinct mount namespaces and 200 tmpfs mounts. The median
metadata cost attributable to tmpfs was 278,528 bytes total (1.36 KiB per cell);
writing 64 KiB in every mount charged approximately 12.5 MiB of file memory.
After killing the run and waiting for the children, no guest process or mount
survived and file memory returned to within 28 KiB of baseline. The integrated
single-run and 200-run evidence is still to be collected by the canonical
validate path; this control does not substitute for it.

## Local and hosted validation

The committed graph owns both filesystem choices. Local portable tests and
focused portable compatibility nodes use the image; hosted shard variants keep
their original host commands, environment and fixture dependencies. Public
selectors such as `test.regular_crates` still resolve in the hosted lane, and
the compatibility selector retains its fixture producer. Prepared Nextest/Cargo
metadata must match both the declared Cargo selection and the consumer's root;
a host preparation cannot satisfy an image consumer.

`test.isolated_dbt_workdir` selects the four strict verification pairs and the
existing blocked-input refusal control. `test.isolated_detcore_workdir` selects
the marked in-process helper control. Their exact terminal test counts are two
and one, respectively; they retain the ordinary per-test CPU/wall policy and
add an inherited 64 MiB per-file limit. Missing, truncated, noncanonical or
unequal INFO evidence and every cap/deadline failure remain failures. Dagrun
passes their admitted worker count through `NEXTEST_TEST_THREADS`, which the
wrapper forwards to the prepared Nextest consumer. This keeps the existing
ignored-test arguments after `--` without appending a misplaced worker flag.
Other wrapped commands retain the scheduler's appended arguments as literal
arguments to their original command.

The wrapper imports only Cargo's registry and Git caches from `--cargo-home`.
The image supplies executables and configuration; host Cargo configuration and
rustup proxies do not cross that boundary. `VALIDATE_RUN_STATE` and structured
result paths are mounted explicitly so the original producers and consumers
refer to the same evidence without sharing unrelated output ownership.

## The network boundary — what is and is not claimed

This is the claim to read carefully, because it is easy to overstate and the
earlier version of this file did.

Validate has **two phases with a network boundary between them**:

| phase | where | network | what it does |
|---|---|---|---|
| **fetch** | the host | **yes** | `cargo fetch --locked` into a `CARGO_HOME`. Downloads only; produces no build output. |
| **build and test** | the pinned root | **no** | each host-scheduled DAG node runs against that cache and the pinned toolchain. |

The network window is deliberately a **pure download**. That matters because
`cargo fetch --locked` cannot introduce variance: every byte it writes is
checked against `Cargo.lock`, which pins exact versions **and content
checksums** for registry crates and exact revisions for git dependencies.
Nothing can enter the build from the network except bytes that already matched
a hash. A phase whose entire output is checksum-verified is a far smaller trust
surface than a build phase that merely happens to have network available.

So, precisely:

- **Claimed, and enforced:** the build and the tests cannot reach the network.
  `--network=none` and `--http-proxy=false`, asserted from *inside* each container by
  `assert-no-network.sh` before its payload, aborting before the node runs.
- **Claimed:** the compiler is pinned (the offline phase runs in the nix root)
  and the crates are pinned (`Cargo.lock` versions + checksums).
- **Not claimed:** that the fetch phase needs no upstreams. It needs
  `github.com` and `crates.io`. It simply cannot lie about what it got.

The assertion carries its own **negative control**, and this is the part that
makes it evidence rather than decoration: run
`assert-no-network.sh --expect-network` on the host and it must report a
reachable network. A probe that only ever runs where the answer is "no network"
cannot tell an isolated environment from a broken probe. Measured on a validate
host, the host run correctly refuses to certify isolation (exit 1) while the
same script in the pinned root reports no route, no DNS and no raw TCP (exit 0).

**The useful failure mode.** If the fetch phase did not populate the cache, the
offline phase fails immediately and loudly — `cargo metadata` exits 101 naming
the pinned `reverie` git dependency — instead of quietly reaching out. Measured:
unpopulated cache → exit 101; after fetch → `cargo metadata` exit 0 and a real
compile of that same git dependency in 8.78s with no network.

### Where the phase node sets come from

They are **not invented here.** `ci/portable-shards.json` partitions the
constructed portable plan, and GitHub CI runs that partition as separate jobs. The
split script reads the same keys with the same `jq` expressions as
`.github/workflows/ci-portable.yml`, including the `e2e.manifest_*` and final
result nodes. On a default full run it counts the selected portable cell
population from `ci/expected-e2e-plan.json` at runtime. It exact-compares the
combined build/test selection with every node in validate's constructed plan
and rejects missing, extra or duplicate identities. `--shards` remains an
explicit partial/debug mode and skips the manifest population.

Two things worth knowing before touching this:

- **The partition is the shard map, not the `group` field.**
  The stable `test.strict_compat` selection alias has group `test`, expands to
  direct `compat.*` nodes, and runs after its other test-node predecessors in a
  separate hosted job; `setup.manifest_plan`,
  `setup.nextest`, `gate.manifest` and `e2e.audit_compile_backend_parity_c` are
  not group `build` but execute before the remaining test side. Partitioning on
  `group` therefore does not reproduce the hosted grouping.
- **GitHub's split is for wall clock, not for network.** Its shard jobs have
  full network and restore a cargo cache with `Swatinem/rust-cache`; the
  prebuilt tarball carries only binaries, not `target/debug/deps`, so the shards
  really do compile. GitHub enforces no network boundary anywhere. This path
  mirrors GitHub's node sets and their order, then adds a boundary GitHub does
  not have.

## Why nix and not just an OCI digest

An OCI digest pins the **artifact**. If the registry loses the blob, a validate
run from a month ago cannot be reproduced. A `flake.lock` pins the **inputs**, so
the image can be rebuilt from source at that lock even after third-party
upgrades. A receipt should carry both: the digest says what ran, the lock says
how to rebuild it.

## What a month-old rebuild of **the image** depends on — measured, not asserted

Scope note: this section is about rebuilding the **pinned root image** from
`flake.lock`. It says nothing about the crate dependency graph, which is pinned
separately by `Cargo.lock` and fetched in the fetch phase described above. The
two are different guarantees with different upstreams and it is a mistake to
read one as covering the other.

Even for the image the guarantee is **not** self-contained today, and a
reproducibility claim that quietly depends on an upstream staying up is worth
nothing.

`flake.lock` pins each input by revision **and `narHash`**, so content is fixed —
a moved tag or a force-push cannot silently change what you get. But rebuilding
still has to *fetch* those sources. A rebuild needs:

| dependency | what breaks without it | risk |
|---|---|---|
| `github.com/NixOS/nixpkgs` @ `56c02bc00adc` | cannot evaluate at all | low — commits are durable, but GitHub availability is assumed |
| `github.com/oxalica/rust-overlay` @ `ab450d47a3f9` | no Rust toolchain | low, same caveat |
| `static.rust-lang.org` | **no dated nightly** | **the real one — see below** |
| `cache.nixos.org` | still works, but builds from source | high cost, not correctness: minutes become hours |

**The dated nightly is the weak link.** `rust-overlay` does not vendor Rust; it
downloads the `nightly-2026-07-29` artifacts from `static.rust-lang.org` at build
time. Old nightlies are not guaranteed to be retained indefinitely. If that
artifact is pruned, this lock stops rebuilding even though every hash in it is
still correct — and it will fail loudly on a hash mismatch rather than quietly
substituting a different compiler, which is the right failure but still a
failure.

### What would make the guarantee real

Two options, neither adopted here because both are storage decisions the owner
should make rather than something to slip in:

1. **Vendor the sources.** A pinned nixpkgs tarball is ~44 MB. (The sha256
   recorded here previously was for the superseded `b134951a4c9f` revision and
   has been dropped rather than left to mislead; re-measure it against the
   current pin if this option is adopted.) The Rust
   artifacts would need the same treatment, and they are the ones that matter.
2. **Export the closure.** `nix copy --to file:///archive` writes the entire
   built closure — **0.63 GB** for this image — to a local store. That is
   self-contained by construction: no GitHub, no `static.rust-lang.org`, no
   `cache.nixos.org`. At 0.63 GB per pinned root this is cheap enough to keep
   one per landed toolchain bump.

Option 2 is the honest way to say "reproducible in a month". Option 1 alone still
leaves the Rust download in the path.

## Toolchain bumps and rollback

A bump is **one reviewed change**: edit `flake.nix`, run `build-image.sh`, and
commit `flake.nix`, `flake.lock` and `image.digest` together. The script writes
the previous digest to `image.digest.prev` automatically, so rollback is putting
that value back or passing `--digest` to the runner. Confirmed working during
this change: adding `/tmp` to the image moved the digest from
`sha256:22c0f945cb4c…` to `sha256:d1301b3ae1eb…` and preserved the prior one.

## Failure mode, deliberately chosen

`run-in-pinned-root.sh` **fails closed** if the pinned image is not present. It
does not fall back to a tag and it does not fall back to the host. A run that is
not in the pinned root must not be recorded as if it were — that would be a
receipt claiming hermeticity it did not have.

## Canonical driver and the whole-split diagnostic path

The canonical path keeps `scripts/validate.rs`, dagrun, the validation lock,
receipts, and the scheduler-owned cgroup on the host. It wraps each build and
test node individually, so the container supplies the pinned filesystem and
network boundary without asking an inner DAG runner to create another cgroup.

The older whole-split diagnostic path still drives `ci/run-node.sh` from inside
one container. That inner runner reaches its engine and then **fails closed**
unless the explicit diagnostic invocation permits the missing inner cgroup:

```
[safe-ci] ERROR: systemd --user scope is unavailable; refusing advisory-only containment.
<runner>: ERROR: cgroup boxing could not be established ...
```

The `<runner>` prefix is the DAG runner's own program name, not a fixed string:
it comes from `PROG` in the runner's CLI, so it tracks whatever the tool is
currently called. Quoting it literally here would rot the moment the tool is
renamed — which it was, mid-flight, while this branch was open. Grep for the
`cgroup boxing could not be established` half, which is stable.

That refusal is correct: resource boxing is the runner's primary purpose and it
declines to pretend. The canonical path avoids this topology rather than
weakening the host-owned cgroup. Strict compatibility is expanded into direct
outer-DAG probes, so it neither starts an inner scheduler nor needs an inner
cgroup opt-out.

Existing evidence proves the boundary and toolchain components end to end:
`assert-no-network.sh` passes inside the container, `cargo metadata` and a real
compile of the pinned `reverie` git dependency succeed offline, and `cargo
fetch` inside the phase is refused. The split driver now selects the complete
portable DAG and its selected portable cell population, with both counts derived
from canonical repository data at runtime. The canonical full selection and
its required single-run and 200-run evidence remain unexecuted in this change.

## Notes from building this

- **The proxy authorizes per identity, and a container is not an authorized
  client.** Measured on a validate host: `api.github.com` returns 200 from the host
  through `with-proxy` and fails from inside a container even with
  `--network=host`. This is why the image is built with nix **on the host** and
  consumed by podman offline, rather than running nix inside a container — which
  is what blocked stage 1.
- **A nix-built root is minimal and has no FHS scratch directories.** Without an
  explicit `/tmp` the backend-abstraction gate failed at `mktemp`, and it failed
  in its *negative control*, so it reported itself untrustworthy rather than
  passing vacuously. Good failure mode; the directory still has to exist.
- The image pins `rustc 1.99.0-nightly (26ae60a9e 2026-07-28)`, which matches the
  commit stage 1 observed on the host, so the pin reproduces the toolchain that
  was already in use rather than silently changing it.

## Pin freshness — the policy, and the check that is missing

**A pin must be current when it is set, and refreshed deliberately.** Pinning by
revision + `narHash` and being up to date are two different properties, and this
directory has already demonstrated that satisfying the first says nothing about
the second: the original `flake.lock` pinned `nixpkgs` at a **2024-12-30**
revision while `rust-overlay` was pinned at that day's tip. The stale half was
invisible because the Rust toolchain comes from the overlay, so nobody looking at
"is the toolchain right?" would ever look at nixpkgs. At that revision
`pkgs.rustc` was **1.77.2**, which cannot build this repository at all — 30 of
its crates are `edition = "2024"`, which requires 1.85 or newer. That silently
made "could we drop `rust-overlay` and use nixpkgs' own Rust?" look answered in
the negative when it was not.

Do **not** fix this by switching to a branch name. A branch is not reproducible.
Re-pin to a fresh revision as a reviewed change, exactly as a toolchain bump is.

### Proposed check (not built here)

Nothing detected the staleness. It sat 20 months out of date, landed on `main`,
and was found only because someone was reading the lock for an unrelated reason.
A check is cheap because the answer is already in the file: every node in
`flake.lock` carries `locked.lastModified` as a Unix timestamp.

- **What it does.** Read `ci/hermetic/flake.lock`, compute `now - lastModified`
  for **every** input, print each age, warn past ~90 days and fail past ~180.
- **Every input, not just the interesting one.** The failure here was one input
  current and one rotten; a check that only looked at "the input we care about"
  would have missed it precisely.
- **Where it lives.** `ci/hermetic/check-pin-age.rs`, a rust-script following the
  repository script convention and co-located with the data it audits per
  `docs/DIRECTORY_ACTIONS.md`, wired into the portable lane alongside the other
  lints.
- **Why it can run anywhere.** It needs **no network** — `lastModified` is
  already in the lock — so it works in the portable lane and inside a hermetic
  root, and it cannot itself become a fetch dependency.
- **Print the age even when green,** so a receipt records how old the pin was on
  the day it ran rather than only whether it crossed a threshold.
