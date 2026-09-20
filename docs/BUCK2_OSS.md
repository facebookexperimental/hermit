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

## On a Meta host

A Meta devserver needs five things the steps above do not mention. All five are
host facts rather than repository defects; a machine with direct internet
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
elsewhere. DotSlash downloads and verifies the platform-specific Buck2 release
binary; `DOTSLASH_CACHE` relocates its cache.

`regenerate-rust-deps` starts without generated dependency output, vendors the
versions in the tracked root `Cargo.lock`, generates
`shim/third-party/rust/BUCK` twice, and refuses the result if two consecutive
outputs differ. It also refuses changes to the lockfile. Both Hermit and
Reverie consume this graph. The repository-root `.gitignore` excludes generated
paths. Those patterns must not move into `shim/.gitignore`: pinned Reindeer
reads ignore files through the shim cell root and would otherwise generate
empty crates.

## What a reproduction should produce

Measured 2026-09-20 on x86_64 Linux with warm tool and crate caches:

| Step | Result |
|---|---|
| `regenerate-rust-deps` | exit 0, 300 crates; `Cargo.lock` SHA-256 `fa94382c50130b061e0e4ed057485c83bdc8cc018fe057be35363e72d90a74f9`; generated `BUCK` SHA-256 `3607f57c700803956e60e25147b982e8e15cb5e91ec08fe3cf679941bcc383c4` |
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
