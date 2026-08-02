# RFC: Deterministic OCI Image Execution

Status: proposed, design only

## Decision Summary

Hermit should add a distinct OCI command family:

```text
hermit oci pull IMAGE
hermit oci inspect IMAGE
hermit oci run [RUN-OPTIONS] [-i] [-t] IMAGE [COMMAND [ARG...]]
```

The target interactive experience is:

```bash
hermit oci run -it ubuntu
```

The canonical form is `hermit oci run`, not an overloaded
`hermit run -it ubuntu`. Existing `hermit run PROGRAM` treats its first
positional argument as a host-visible program. Guessing whether that argument
is a program or an image would be ambiguous, and `-i` and `-t` must also remain
useful for host-visible programs. The prototype's explicit form,
`hermit run --image IMAGE -- PROGRAM`, may remain as a compatibility alias once
both paths use the same implementation.

Use these Rust crates:

- [`oci-distribution` 0.11](https://crates.io/crates/oci-distribution) for OCI
  registry references, bearer/basic authentication, manifest retrieval, image
  index selection, and blob streaming. Its active upstream is the ORAS
  [`rust-oci-client`](https://github.com/oras-project/rust-oci-client).
- [`oci-spec` 0.10](https://crates.io/crates/oci-spec) with
  `default-features = false, features = ["image"]` for typed OCI image
  manifests and configuration. It is maintained by the
  [`oci-spec-rs`](https://github.com/youki-dev/oci-spec-rs) project.
- A small Hermit-owned layer applier for ordered extraction, whiteouts,
  ownership, and path confinement. Registry transport and the OCI schema are
  reused; filesystem policy remains in Hermit, where its isolation and
  determinism requirements can be enforced.

Do not use Youki's [`libcontainer`](https://crates.io/crates/libcontainer) as
the runtime. It is a complete OCI runtime library: it owns namespaces, mounts,
cgroups, seccomp, process launch, and console setup. Reverie and Hermit already
own that lifecycle. Putting one runtime inside the other would create two
competing process supervisors and make backend attachment unclear.

This choice was verified against the published crates and upstream repositories
on 2026-08-02. `oci-distribution`, `oci-spec-rs`, and Youki were active and not
archived at that time.

## Goals

1. Pull a Linux OCI image, resolve it to an immutable manifest digest, unpack
   it safely, and run its declared command under Hermit.
2. Make the image filesystem, image configuration, and per-run writable state
   explicit inputs to Hermit's determinism claim.
3. Preserve the existing Detcore execution path. OCI support supplies a guest
   filesystem and process configuration; it is not a second determinism engine.
4. Support a usable `-it` shell with correct PTY, signal, foreground process
   group, and terminal restoration behavior.
5. Record interactive input so a session can be replayed without depending on
   human keystroke timing.
6. Keep image materialization backend-neutral while qualifying each backend's
   rootfs presentation path independently.
7. Remain rootless by default and fail closed when the host cannot provide the
   required user and mount namespace operations.

## Non-Goals

- Implementing the full Podman command surface, daemon, build system, pod
  model, networking stack, or volume manager.
- Replacing Reverie with an OCI runtime such as Youki or `runc`.
- Claiming that a mutable tag such as `ubuntu:latest` identifies the same bytes
  across separate invocations.
- Making external registry or host-network responses deterministic.
- Enabling every backend in the first implementation phase.
- Treating an image digest as publisher authenticity. Content integrity and
  signature policy are separate concerns.

## Existing Seams And Prior Art

Hermit's CLI already has a distinct `run` subcommand and a positional program
contract ([source](https://github.com/rrnewton/hermit/blob/8c6e3efe7f91713295db26b5413cd2bed8c686f8/hermit-cli/src/bin/hermit/main.rs#L193-L225)).
`RunOpts` already owns backend selection, mounts, network mode, `/tmp`,
environment, working directory, and two-run verification
([source](https://github.com/rrnewton/hermit/blob/8c6e3efe7f91713295db26b5413cd2bed8c686f8/hermit-cli/src/bin/hermit/run.rs#L84-L300)).
The common container path creates a root-mapped PID namespace and mounts
`/proc` before Reverie runs the child
([source](https://github.com/rrnewton/hermit/blob/8c6e3efe7f91713295db26b5413cd2bed8c686f8/hermit-cli/src/bin/hermit/container.rs#L104-L138)).

Detcore is deliberately backend-independent: a backend supplies event
mechanism while Detcore supplies deterministic policy
([architecture](https://github.com/rrnewton/hermit/blob/8c6e3efe7f91713295db26b5413cd2bed8c686f8/docs/ARCHITECTURE.md#L57-L64)).
The same split applies here. OCI materialization determines which files and
configuration the guest sees. Detcore determines how the resulting program is
scheduled and how nondeterministic events are handled.

[PR #1179](https://github.com/rrnewton/hermit/pull/1179) proved the basic path:
rootless image materialization, a rootfs chroot, image `Env` and `WorkingDir`,
and ptrace L2 execution. It also exposed the production requirements this RFC
addresses:

- remove the `buildah` subprocess dependency;
- key storage by the resolved manifest digest, not the input reference string;
- prevent a guest from mutating a shared cached rootfs;
- preserve `/tmp` and network isolation in the image path;
- route every supported backend through an explicit rootfs presentation seam;
- implement OCI command, user, and PATH behavior rather than requiring an
  absolute executable path;
- integrate a real interactive TTY path.

## User Experience

### Command forms

```bash
# Pull and print the resolved digest.
hermit oci pull ubuntu

# Run Config.Entrypoint + Config.Cmd.
hermit oci run ubuntu

# Retain Entrypoint and replace Cmd, matching container CLI behavior.
hermit oci run ubuntu echo hello

# Replace Entrypoint explicitly.
hermit oci run --entrypoint /bin/bash ubuntu -lc 'id; uname -a'

# Interactive terminal.
hermit oci run -it ubuntu

# Determinism verification from two fresh writable snapshots.
hermit oci run --strict --verify ubuntu /bin/sh -lc 'date; id'

# Existing backend selection remains global.
hermit --backend=ptrace oci run --strict ubuntu /bin/true
```

`ubuntu` normalizes to `docker.io/library/ubuntu:latest`. A tag is resolved
once, before sandbox creation, to a platform-specific image manifest digest.
Hermit prints and records that digest. Both executions of `--verify` use the
same resolved manifest and the same content-addressed blobs; Hermit never
resolves a tag between verification runs.

For reproducible automation, callers should use a digest:

```bash
hermit oci run \
  docker.io/library/ubuntu@sha256:0123456789abcdef... /bin/true
```

Accepting a tag is necessary for the drop-in experience, but a claim is always
reported as "deterministic for resolved digest D", never "deterministic for
latest". Add `--require-digest` for CI and `--offline` for digest-only cached
execution. `--pull=missing|always|never` controls registry access without
changing this rule.

### Image configuration

The initial implementation applies the OCI image's:

- `Entrypoint` and `Cmd`, with the override behavior above;
- `Env`, starting from an empty host environment;
- `WorkingDir`, creating it only when OCI/container semantics require that;
- `User`, after resolving names against the image's passwd/group databases;
- `StopSignal` for interactive shutdown.

User `-e`, `--workdir`, `--entrypoint`, and mount options override image
defaults. Host environment values pass only when explicitly requested. Images
with user or ownership requirements that cannot be represented by the
available subordinate UID/GID mapping fail before execution; Hermit must not
silently run them as another user.

Image `ExposedPorts`, annotations, and history are metadata only. Image
`Volumes` receive fresh per-run writable storage; they do not create implicit
host bind mounts.

### Pull authentication

Never accept a registry password as a normal command-line value. The first
phase supports anonymous pull plus a credential provider that reads standard
containers/Docker auth files or invokes a credential helper. The provider
returns `RegistryAuth` to `oci-distribution`; it owns neither blob parsing nor
cache paths. Logs redact authorization headers and resolved credentials.

## Architecture

### Data model

The implementation should expose four narrow values:

```text
ResolvedImage
  normalized reference, manifest digest, platform, typed manifest/config

ContentStore
  verified manifest/config/layer blobs addressed by digest

RootfsSnapshot
  immutable extracted lower tree plus a fresh per-run writable view

PreparedGuest
  rootfs view, argv, env, cwd, identity, mounts, network, terminal policy
```

No backend receives an image reference. A backend receives a `PreparedGuest`
whose registry traffic and unpacking have already completed.

### Pull and content store

1. Parse and normalize the reference with `oci-distribution`.
2. Resolve an image index using an exact `linux/amd64` match for Hermit's
   current supported host. Reject missing or ambiguous platform entries.
3. Fetch the raw manifest and calculate its digest locally. If the caller gave
   a digest, require an exact match even if the registry's
   `Docker-Content-Digest` header claims otherwise.
4. Parse the verified bytes with `oci-spec` image types.
5. Stream config and layer descriptors into temporary content-store files.
   Check declared size and digest while streaming, fsync, then atomically rename
   into `blobs/ALGORITHM/HEX`.
6. Parse the verified config and require its `rootfs.diff_ids` to match the
   ordered, uncompressed layer digests.

Use `oci-distribution`'s manifest and `pull_blob` APIs rather than its
high-level in-memory `pull` result. Container layers can be large, and Hermit
must retain descriptor identity, verify bytes itself, and apply layers in
manifest order. Blob downloads may run concurrently because the CAS is
content-addressed; layer application remains ordered.

The cache layout is based on actual content identity:

```text
$XDG_CACHE_HOME/hermit/oci/
  blobs/sha256/HEX
  manifests/sha256/HEX.json
  snapshots/sha256/HEX/linux-amd64/rootfs/
  locks/sha256-HEX.lock
```

Materialization uses a same-filesystem temporary directory and atomic rename.
A lock serializes construction of one digest. An interrupted pull or unpack is
never treated as complete.

### Secure layer application

The layer applier is security-sensitive. It must:

- support OCI and Docker gzip layers plus OCI zstd layers;
- verify the compressed descriptor digest and uncompressed `diff_id`;
- apply layers in manifest order;
- implement ordinary and opaque whiteouts exactly as specified by the
  [OCI image layer specification](https://github.com/opencontainers/image-spec/blob/main/layer.md#whiteouts);
- resolve extraction through a root directory FD using `openat2` beneath/in-root
  constraints, never by joining an untrusted archive path to a host path;
- reject `..`, absolute escape, magic links, symlink traversal, and hard links
  whose target escapes the root;
- bound expanded bytes, entry count, path length, and link depth;
- preserve supported modes, ownership, timestamps, xattrs, and hard links;
- fail with a named compatibility error for unsupported devices, capabilities,
  xattrs, or ID mappings rather than silently dropping them.

Extraction runs in a short-lived helper process with Landlock when available
and a dedicated user namespace. The target root FD is the only writable tree.
The helper performs no registry authentication and receives already verified
blob FDs, limiting both credential and filesystem exposure.

The small `oci-unpack` crate demonstrates useful dirfd, Landlock, and whiteout
techniques, but it is not selected as the public integration boundary: its
0.1 API combines anonymous-only transport with unpacking, skips unsupported
archive entry types, and ignores some ownership failures. Those behaviors are
too permissive for Hermit's fail-closed contract.

### Per-run rootfs and mounts

The extracted snapshot is never the guest's writable root. For each execution:

1. Create a private mount namespace inside Hermit's rootless user namespace.
2. Mount the content-addressed snapshot as a read-only lower directory.
3. Create fresh upper and work directories and mount a per-run overlay.
4. If rootless kernel overlay is unavailable, use a correctness-first full copy
   that preserves the validated metadata. Do not fall back to the shared lower
   tree.
5. Mount a fresh tmpfs for `/tmp` and `/run`, a new `/proc`, a minimal `/dev`,
   and a private `devpts` instance when `-t` is active.
6. Apply explicit bind mounts through root-FD-confined target resolution.
7. `pivot_root` into the merged view, detach the old root, close setup FDs, set
   identity/cwd/env, and exec the guest.

`pivot_root` is preferred to a bare `chroot`: the old host root must not remain
reachable through the process cwd, mount tree, or inherited file descriptors.
The default remains isolated loopback networking. `--network=host` and writable
host bind mounts are explicit nondeterministic inputs and are recorded in the
run summary.

This directly closes the risks tracked by `fix-1179-oci-isolation-risks`:
verification runs cannot poison each other, the image path cannot bypass
network or `/tmp` policy, and unsupported launch paths cannot omit the rootfs
transition.

### Backend composition

Image pull, blob verification, config parsing, and rootfs extraction are
backend-neutral. Rootfs presentation is a backend capability, not an
assumption:

| Backend path | Rootfs presentation | First qualification |
| --- | --- | --- |
| ptrace | Host mount namespace, overlay, `pivot_root`, then normal Reverie attach | Compatibility baseline |
| DBI/SaBRe | Same host namespace view, but instrumentation/runtime files must be opened before `pivot_root` or deliberately staged inside the image | Performance follow-up |
| LiteInst hybrid | Same host namespace view; preload DSO and post-exec bootstrap must be qualified inside the image | After its exec/lifecycle support |
| KVM | Export the prepared root through a block image, virtio-fs, or equivalent guest-kernel filesystem channel | Separate performance/isolation phase |

Define a backend rootfs capability such as `HostPivotRoot`, `GuestExport`, or
`Unsupported`. `hermit oci run --backend=X` fails before guest launch when the
backend has not passed its rootfs and runtime-artifact tests. There is no silent
fallback to ptrace and no silent use of the host root.

This preserves the backend-agnostic filesystem seam without making the false
claim that host `chroot` automatically configures KVM. Ptrace is the golden
compatibility baseline; DBI and KVM are performance paths qualified against
the same image, argv, environment, and filesystem fixture.

## Interactive TTY Design

`-i` and `-t` are independent:

- `-i` keeps an input stream open.
- `-t` allocates a PTY, makes its slave the controlling terminal, and connects
  guest stdin/stdout/stderr to it.
- `-it` does both.

The current output-capturing path intentionally does not snapshot terminal
stdin ([source](https://github.com/rrnewton/hermit/blob/8c6e3efe7f91713295db26b5413cd2bed8c686f8/hermit-cli/src/lib.rs#L183-L257)).
Therefore `-it` depends on `interactive-tty-shell-support`; it is not a parser
alias around existing stdin forwarding.

The terminal proxy must:

1. Save the host terminal mode and restore it on every normal, error, and signal
   exit path.
2. Allocate a private devpts PTY pair, create a guest session, set the
   controlling terminal, and put the guest process group in the foreground.
3. Proxy bytes without line-oriented rewriting.
4. Preserve terminal-driver behavior for Ctrl-C, Ctrl-Z, EOF, and job control;
   forward shutdown signals to the guest foreground process group.
5. Record the initial termios state and window size and handle resize events as
   explicit `SIGWINCH` inputs.
6. Continue draining output and reaping the process tree after the interactive
   shell exits.

Human arrival time is not a deterministic guest input. Interactive recording
stores an ordered transcript:

```text
header: image digest, argv/env/cwd, terminal modes, rows/columns
events: stdin bytes | EOF | signal | resize, each with a logical sequence
```

It does not store wall-clock delays as replay authority. While a live session
waits for external input, guest progress is stopped at a deterministic input
boundary. Replay injects the same ordered events at those boundaries.

Until transcript replay exists, `--verify -it` must fail with a useful message.
After it exists, verification records run one's input and automatically replays
it into a fresh PTY and fresh rootfs overlay for run two. A live interactive run
can be useful before then, but it is not an L2 result.

## Determinism Argument

For a resolved digest, platform, Hermit configuration, backend, and optional
input transcript:

1. The manifest, config, and layer bytes are selected and verified by content
   digest.
2. Ordered layer application produces one immutable lower rootfs.
3. Each execution begins with an empty upper layer, so run one cannot affect
   run two or a later invocation.
4. Argv, environment, cwd, identity, mounts, and terminal inputs are derived
   from the verified config plus explicit CLI inputs.
5. The selected Reverie backend loads the shared Detcore policy, which handles
   execution nondeterminism as for an ordinary Hermit run.

Thus OCI support closes the mutable host-filesystem input without adding a new
determinization strategy. Verification still compares guest output and Detcore
logs; it additionally requires equal image digest, sandbox plan, and transcript
digest.

The run summary must expose at least:

```text
input_image_reference
resolved_manifest_digest
platform
config_digest
rootfs_snapshot_digest
backend
network_mode
host_bind_mounts
interactive_transcript_digest (when present)
```

Residual inputs remain honest and visible: a tag may resolve differently on a
later invocation, registry availability affects pulling, host networking is
external, bind mounts can change, and backend hardware requirements can vary.
None of those is hidden behind an unqualified deterministic claim.

## Isolation And Trust

The first production release requires:

- rootless user, PID, mount, UTS, and network namespaces;
- no shared writable image cache in the guest mount tree;
- a detached old root after `pivot_root`;
- a minimal device tree and private devpts;
- confined archive and mount target resolution;
- no inherited registry credentials or host environment in the guest;
- no backend-specific bypass of the prepared sandbox;
- explicit failure when ownership, capabilities, or mount semantics cannot be
  represented safely.

Digest verification detects corruption but not a malicious publisher. Add an
optional Sigstore/cosign policy after the basic pull path, with policies such as
`--verify-signature`, trusted identity constraints, and a fail-closed CI mode.
Signature verification does not replace layer extraction confinement.

## Implementation Plan

### Phase 1: Native pull, inspect, and content store

- Add `hermit oci pull` and `hermit oci inspect`.
- Integrate `oci-distribution` and image-only `oci-spec`.
- Implement deterministic platform selection, descriptor verification, CAS
  locking, atomic writes, auth-provider abstraction, and structured digest
  output.
- Add a local-registry integration fixture covering anonymous/basic/bearer auth,
  tags, digests, indexes, wrong digest, truncated blob, and concurrent pull.

Exit gate: no `buildah`/Podman dependency; a pulled image is fully identified by
verified manifest/config/layer digests and can be reused offline.

### Phase 2: Secure unpack and ptrace execution

- Implement the confined layer helper and immutable snapshot cache.
- Add fresh overlay/copy views, pseudo-filesystem mounts, rootless ID handling,
  image command/env/cwd/user semantics, and common `PreparedGuest` plumbing.
- Land `hermit oci run` non-interactive on the ptrace backend.
- Keep `hermit run --image` as a thin compatibility alias, not a second path.

Exit gate: BusyBox and Ubuntu commands pass ptrace L2 with default log level and
no relaxations; run one writing `/etc` or `/tmp` cannot affect run two; network,
mount, old-root, symlink, whiteout, ownership, and cache-poison tests pass.

### Phase 3: Interactive shell and transcript replay

- Complete `interactive-tty-shell-support` first: PTY, controlling terminal,
  foreground process groups, signal forwarding, resize, and terminal restore.
- Add `-i`, `-t`, and transcript record/replay to OCI run.
- Make `--verify -it` record run one and replay run two from a fresh overlay.

Exit gate: an Ubuntu shell supports commands, Ctrl-C, Ctrl-Z/foreground resume,
resize, EOF, and clean exit; a recorded session reproduces at ptrace L2; Hermit
restores the host terminal after success, guest crash, and Hermit interruption.

### Phase 4: Backend qualification

- Move rootfs setup behind the explicit backend capability interface.
- Qualify DBI/SaBRe runtime artifacts and exact output against ptrace fixtures.
- Qualify LiteInst only after its image-replacement lifecycle is supported.
- Implement and qualify a KVM rootfs export mechanism; do not reuse the host
  `pivot_root` claim for KVM.

Exit gate: every enabled cell is bitwise-identical to ptrace for the same image
digest and transcript. Unsupported backends fail before guest launch.

### Phase 5: Policy and operations

- Credential-helper coverage and registry auth-file compatibility.
- Signature policy, offline policy, cache inspection, and garbage collection.
- Pull progress, cancellation, quotas, and decompression resource limits.
- Performance measurement of cold pull, warm snapshot, overlay startup, DBI,
  and KVM without weakening the correctness gates.

## Required Tests

The implementation is not complete without all of these classes:

- CLI parsing and ambiguity: host `hermit run bash` remains a program run;
  `hermit oci run ubuntu` is always an image run.
- OCI semantics: entrypoint/cmd overrides, PATH, env, cwd, user, volumes,
  platform indexes, gzip/zstd, and whiteouts.
- Supply-chain inputs: wrong manifest/config/layer digest and size, invalid
  media type, decompression limit, traversal, symlink and hardlink escape.
- Cache: interrupted pull, concurrent pull, tag movement, offline digest run,
  and stale/incomplete materialization.
- Isolation: rootfs write poisoning, fresh `/tmp`, explicit mounts, local/host
  networking, old-root reachability, inherited FDs, and credentials.
- Determinism: two fresh overlays at ptrace L2, then exact cross-backend output
  for each backend as it is enabled.
- TTY: interactive echo, binary input, EOF, Ctrl-C, Ctrl-Z, foreground process
  group, resize, terminal restoration, transcript replay, and `--verify -it`.

## Delivery Boundaries

Keep the change reviewable as separate PRs:

1. dependency decision plus pull/content store;
2. secure unpacker plus adversarial archive tests;
3. common sandbox/rootfs presentation plus ptrace run;
4. TTY and transcript support;
5. one PR per additional backend presentation path;
6. signature/auth/cache operations.

Do not merge a facade that accepts `-it` while terminal semantics still hang,
or a facade that accepts an image while silently running against host files.
The first user-visible `oci run` release must include the ptrace isolation and
fresh-state gates; `-it` becomes advertised only when its phase gate is green.
