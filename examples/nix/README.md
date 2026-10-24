# Building Nix derivations under Hermit

These two files run a Nix build inside `hermit run`, so a builder that reads the
clock, `/dev/urandom`, `$RANDOM` or a thread schedule still produces the same
output bytes on every build. Neither one patches Nix or nixpkgs.

## Why

Nix fixes a build's inputs, but not what the builder does at run time. A build
that embeds a timestamp, a random identifier or the order in which parallel
jobs finished gives a different output on every run, which breaks binary-cache
verification (`nix build --rebuild`, `nix store verify`) and early cutoff for
content-addressed derivations. Today each such package is patched by hand.
Hermit removes those sources of nondeterminism for the whole process tree:
time and randomness come from a seeded virtual source, and threads and
processes run one at a time in a deterministic order.

## How it works

There are two ways to attach Hermit. Use the first one.

### 1. `external-builders` (recommended): `hermit-external-builder.sh`

Nix 2.35 has an experimental `external-builders` setting. For each build of a
matching system, Nix creates the build directory and then runs

    <program> <args...> <path to a JSON build description>

instead of the builder. The description names the builder, its arguments and
environment, `tmpDir` (the host build directory) and `tmpDirInSandbox` (the path
the builder expects, `/build`). The program prints a line holding only `\2` on
stderr to tell Nix the build has started, then runs the builder however it
likes.

`hermit-external-builder.sh` runs

    env -i PATH=/usr/bin:/bin HOME=/homeless-shelter TMPDIR=/tmp \
      HERMIT <hermit run args> --bind <tmpDir>:/tmp/build --workdir /tmp/build -- \
      /usr/bin/env -i TZ=UTC <the derivation's environment> <builder> <args>

The derivation is unchanged, so the output path is the same store path a
native build would produce, and a Hermit build can be checked against the
binary cache or against a native build directly.

Three details matter for reproducibility:

- **The build directory has a random name.** Nix 2.35 names it
  `/nix/var/nix/builds/nix-<pid>-<random u32>`, and that path appears in `PWD`,
  `TMPDIR` and `NIX_BUILD_TOP`. Its length alone changes how many branches the
  builder executes and therefore Hermit's virtual clock. The script bind-mounts
  it at `/tmp/build` on Hermit's private `/tmp` and rewrites every environment
  value under `/build` to `/tmp/build`, so the guest sees one path on every
  build. Hermit must therefore run WITHOUT `--tmp=/tmp`.
- **Nix starts the program inside that directory.** The script changes to `/`
  before starting Hermit so that the random name does not reach Hermit through
  its own working directory.
- **The guest sees the host's `/etc`.** glibc would apply the host's
  `/etc/localtime`, while Nix's sandbox has none and gives UTC. The script sets
  `TZ=UTC` ahead of the derivation's environment, so a derivation that sets
  `TZ` itself keeps its value. A program that reads other files under `/etc`
  still sees the host's (https://github.com/rrnewton/hermit/issues/3649).

### 2. `realBuilder` override: `hermit-wrap.nix`

`hermit-wrap.nix` overrides a derivation's `realBuilder` with a small script
that re-executes the original builder under Hermit. It works with any Nix
version, and `hermitize`, `hermitizeIfNeeded` and `overlayFor` let you opt in
one package at a time. The override is part of the derivation, so the output
path changes: compare a wrapped build with another wrapped build, never with
the native one. It does not set `TZ`, so a wrapped build applies the host's
`/etc/localtime`, unlike the external builder.

## Usage

Both methods need `sandbox = false`, because the builder runs Hermit from the
host filesystem, and a host with a usable PMU (Hermit counts retired
conditional branches for deterministic preemption).

A stdenv build with a fixupPhase also needs
https://github.com/rrnewton/hermit/pull/3554, still open
(https://github.com/rrnewton/hermit/pull/3534, the other half, is merged).
nixpkgs' `audit-tmpdir.sh` reads from named FIFOs and process substitutions,
and without both changes the build hangs there
(https://github.com/rrnewton/hermit/issues/2203).

With `external-builders`:

```bash
nix-build \
  --extra-experimental-features external-builders \
  --option sandbox false \
  --option external-builders '[{
    "systems": ["x86_64-linux"],
    "program": "/abs/path/examples/nix/hermit-external-builder.sh",
    "args": ["--native-fixed-output", "/abs/path/hermit", "run", "--epoch=2026-01-01T00:00:00Z"]
  }]' \
  '<nixpkgs>' -A hello
```

- Pin `--epoch`: without it `hermit run` takes one host wall-clock sample per
  run, and every timestamp differs.
- `--native-fixed-output` builds fixed-output derivations (source downloads)
  without Hermit. Nix checks their hash anyway, and they need the network.
  Untested: every source in the runs below was already in the store, so no
  download has gone through this path, and whether Nix's build description
  carries a derivation's impure variables (proxy settings, certificate file)
  has not been checked.
- `--launcher PROGRAM` runs `PROGRAM HERMIT ...` instead of `HERMIT ...`, for a
  site wrapper that bounds the run.
- The script needs bash 4.4 or later, and `jq` on `PATH` or named by `$JQ`.

To check a build, rebuild it and compare: `nix-build --check ...` reports a
differing output.

What has been tested, on one x86_64 host with Nix 2.35.1 and the ptrace
backend. Each number below names the version of this script that produced
it.

An earlier version of this script (no option parsing, the host's `HOME`, no
`TZ`), with Hermit including both pull requests above: four small derivations that write
the clock, `/dev/urandom`, `$RANDOM` and a UUID into their output, one of them
a full stdenv build including fixupPhase. Through `external-builders`, each
gave one output hash in 20 builds with `run` and with `run --strict`, and in
10 builds with `--no-rcb-time`. The same derivations built natively gave a
different hash on every build.

Real nixpkgs packages, built through this script as it was before `TZ=UTC`
was added, with `--native-fixed-output --no-rcb-time --max-timeslice=disabled`
and Hermit commit 2e59e12ce8cb, which combines
https://github.com/rrnewton/hermit/pull/3554,
https://github.com/rrnewton/hermit/pull/3566 and
https://github.com/rrnewton/hermit/pull/3570: `hello` and `duktape` gave the
same output as a native build, byte for byte. Five packages whose two
sandboxed native builds differ (chibi, sagittarius-scheme, aichat, rav1e,
gdbHostCpuOnly) each gave one output in two Hermit builds, but not the native
output. Two causes seen in those runs are fixed since: the host timezone
(sagittarius-scheme's manual showed `-0800`), now set to UTC by this script,
and a `SOURCE_DATE_EPOCH` that moved to `--epoch`
(https://github.com/rrnewton/hermit/issues/3639), fixed on main. With the
current script and the same Hermit, `hello` again gave the native output.
Known reasons a Hermit output still differs from a native one:
- The build directory is `/tmp/build`, not `/build`, and a package that
  records it (in `__FILE__` strings, for example) keeps that path.
- Hermit sorts every `getdents64` batch by name so that directory listings are
  reproducible, while a native build sees the filesystem's order. A package
  that records a listing without sorting it (chibi's `.chibi.meta`) differs
  from native but not between Hermit builds.

With `hermit-wrap.nix`:

```nix
let
  pkgs = import <nixpkgs> { };
  hw = import ./hermit-wrap.nix { inherit pkgs; hermit = "/abs/path/hermit"; };
in
hw.hermitize pkgs.hello
```

## Limitations

- Hermit runs the build one thread at a time, so a parallel build becomes a
  serial one and a build that spawns many short processes is slower.
- Hermit does not make a changing filesystem or the network deterministic. A
  builder that reads host state outside the store and the build directory can
  still differ between builds.
- The script rewrites an environment value that is exactly `/build` or starts
  with `/build/`. A value with `/build` elsewhere in it, such as
  `x:/build/y`, is passed unchanged and names a path the guest does not have.
- `external-builders` is experimental in Nix and its interface may change.
