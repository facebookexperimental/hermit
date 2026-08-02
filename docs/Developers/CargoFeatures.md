# Cargo Features and Backend Build Boundaries

This reference audits first-party Cargo features and source-level
`cfg(feature = "...")` uses in Hermit and Reverie. It distinguishes Cargo's
release defaults from the developer build entry point. Dependency features
requested on third-party crates are outside this inventory.

Audited revisions:

- Hermit [`065980ea`](https://github.com/rrnewton/hermit/tree/065980ea661f9d5e84b4fbaa0c69f4a4f69a81a9)
- Reverie [`37f04b76`](https://github.com/rrnewton/reverie/tree/37f04b7661a4f77955ba2fce7d3c9e8f1886631d)

## Hermit Features

All Hermit-local features belong to the `hermit` package and are declared in
[`hermit-cli/Cargo.toml`](https://github.com/rrnewton/hermit/blob/065980ea661f9d5e84b4fbaa0c69f4a4f69a81a9/hermit-cli/Cargo.toml).

| Feature | Default | What it gates |
| --- | --- | --- |
| `default` | On, empty | Enables no optional backend. |
| `dbi` | Off | Optional `detcore-dbi` and `reverie-dbi` dependencies plus DBI dispatch, runtime callbacks, tests, and imports. |
| `sabre` | Off | SaBRe runtime availability and its enabled-path test. The external loader and plugin are staged separately. |
| `e9patch` | Off | e9patch runtime availability. The preprocessing module remains compiled because it shares parser and instruction-map machinery with core code. |
| `third-party-backends` | Off | Aggregate enabling `dbi`, `sabre`, and `e9patch`; it has no direct source cfg. |

The workspace's
[`default-members`](https://github.com/rrnewton/hermit/blob/065980ea661f9d5e84b4fbaa0c69f4a4f69a81a9/Cargo.toml)
exclude `detcore-dbi`, `detcore-sabre`, and `hermit-install`. The default Cargo
build therefore remains the lean release configuration.

## Reverie Features

| Crate | Feature | Default | What it gates |
| --- | --- | --- | --- |
| [`reverie-dbi`](https://github.com/rrnewton/reverie/blob/37f04b7661a4f77955ba2fce7d3c9e8f1886631d/reverie-dbi/Cargo.toml) | `prototype-runtime` | On | The bundled prototype runtime and its exported callbacks. Hermit disables this default and supplies Detcore's runtime. |
| [`reverie-e9patch`](https://github.com/rrnewton/reverie/blob/37f04b7661a4f77955ba2fce7d3c9e8f1886631d/reverie-e9patch/Cargo.toml) | `preload-constructor` | On | Automatic runtime installation from the shared library constructor. |
| [`reverie-liteinst`](https://github.com/rrnewton/reverie/blob/37f04b7661a4f77955ba2fce7d3c9e8f1886631d/reverie-liteinst/Cargo.toml) | `preload-constructor` | On | Automatic LiteInst runtime installation. Hermit disables this default for its host-hybrid integration. |
| [`reverie-preload`](https://github.com/rrnewton/reverie/blob/37f04b7661a4f77955ba2fce7d3c9e8f1886631d/reverie-preload/Cargo.toml) | `preload-constructor` | On | Automatic preload runtime installation. |
| `reverie-preload` | `coordinator-rpc` | Off | The synchronous coordinator RPC client and its optional serialization/Reverie dependencies. |
| [`reverie-process`](https://github.com/rrnewton/reverie/blob/37f04b7661a4f77955ba2fce7d3c9e8f1886631d/reverie-process/Cargo.toml) | `nightly` | Off | Nightly-only process-container code paths. Its `default` feature set is empty. |
| [`safeptrace`](https://github.com/rrnewton/reverie/blob/37f04b7661a4f77955ba2fce7d3c9e8f1886631d/safeptrace/Cargo.toml) | `memory` | Off | The optional `reverie-memory` module/dependency. |
| `safeptrace` | `notifier` | Off | Pidfd/event notification state and wait handling. Its `default` feature set is empty. |

The three `preload-constructor` features are independent, crate-local switches;
the repeated name is intentional rather than a shared feature.

## Backend Matrix

| CLI selection | Compiled in core release | Feature boundary | Runtime dependency |
| --- | --- | --- | --- |
| `ptrace` | Yes | None | Host ptrace, namespaces, seccomp, and PMU when preemption is enabled. |
| `kvm` | Yes | None | `/dev/kvm` and the KVM guest ABI. |
| `liteinst` | Yes | None | Staged `libreverie_liteinst.so`; Hermit uses `reverie-liteinst` with its constructor default disabled. |
| `dbi` | No | `dbi` | Staged DynamoRIO, native client, and `libdetcore_dbi.so`. |
| `sabre` | No | `sabre` | Staged SaBRe loader and `libdetcore_sabre.so`. |
| `e9patch` | No | `e9patch` | Staged `e9tool` and `e9patch`; execution remains ptrace-backed preprocessing rather than a separate Detcore runtime. |

The intended split is a core Cargo release containing ptrace, KVM, and LiteInst,
and a developer build that explicitly enables and stages all third-party
backends. Draft PR
[#1433](https://github.com/rrnewton/hermit/pull/1433) makes plain `make` the
all-backend developer build while keeping `make release-core` feature-free.

This matches the release plan only if "single static core binary" means one
Hermit executable with no third-party backend features. It is not currently a
literal static, single-file distribution: the executable dynamically links
host libc/libunwind, and LiteInst needs the separately staged
`libreverie_liteinst.so`. A clean `release-core` build compiles the LiteInst
selection but does not by itself make that selection runnable. The release
contract must either call this a lean core executable, ship the LiteInst runtime
beside it, or change LiteInst to an embedded runtime.

## Conditional Compilation Audit

| Repository | Feature cfg occurrences | Result |
| --- | --- | --- |
| Hermit | `dbi`: 78; `sabre`: 3; `e9patch`: 2; aggregate: 0 | Every cfg names a declared leaf feature. All occurrences are confined to `hermit-cli`; no dead or contradictory feature name was found. |
| Reverie | `prototype-runtime`: 23; `preload-constructor`: 3; `coordinator-rpc`: 1; `nightly`: 2; `memory`: 1; `notifier`: 37 | Every cfg names a feature declared by its containing crate. Positive/negative pairs provide explicit enabled and disabled behavior where required. |

Hermit's gates are in
[`hermit-cli/src/lib.rs`](https://github.com/rrnewton/hermit/blob/065980ea661f9d5e84b4fbaa0c69f4a4f69a81a9/hermit-cli/src/lib.rs)
and
[`hermit-cli/src/bin/hermit/backends.rs`](https://github.com/rrnewton/hermit/blob/065980ea661f9d5e84b4fbaa0c69f4a4f69a81a9/hermit-cli/src/bin/hermit/backends.rs).
Reverie's gates are confined to the six feature-declaring crates listed above;
the largest groups are
[`reverie-dbi/src/lib.rs`](https://github.com/rrnewton/reverie/blob/37f04b7661a4f77955ba2fce7d3c9e8f1886631d/reverie-dbi/src/lib.rs)
and
[`safeptrace/src`](https://github.com/rrnewton/reverie/tree/37f04b7661a4f77955ba2fce7d3c9e8f1886631d/safeptrace/src).

Each Hermit leaf feature, the empty feature set, and the aggregate compile
independently. `third-party-backends` is deliberately only a manifest aggregate;
adding direct cfg uses of the aggregate would make individual leaf builds
behave differently.

## Recommendations

1. Resolve the release-contract ambiguity: "single static binary" is not true
   for a runnable LiteInst configuration today. Specify whether the deliverable
   is the lean executable alone or an executable-plus-LiteInst-runtime package.
2. Keep `hermit`'s Cargo `default` feature set empty. Express the all-backend
   developer policy in `make`, not by changing the published crate defaults.
3. Keep both no-feature and `third-party-backends` builds in CI. Also retain
   independent leaf-feature checks so accidental coupling is detected.
4. Continue setting `default-features = false` on Hermit's `reverie-dbi` and
   `reverie-liteinst` dependencies; Hermit supplies the Detcore DBI runtime and
   controls LiteInst activation itself.
5. Treat `sabre` and `e9patch` as availability gates, not complete module
   elimination. Gate more code only after measuring binary-size benefit and
   separating parser/instruction-map code shared with the core build.
6. Document build commands in terms of `make` and `make release-core`; a bare
   release Cargo command neither expresses nor stages the all-backend developer
   package.
