# Private static first-image runtime

This producer is separate from the ordinary GNU LiteInst DSO finalizer. It
builds the private musl image used only by Hermit compiled with
`exec-stop-launcher,liteinst` and launched with `--in-guest-exec-stop` on
`in-guest-trap`. The flag is off by default. Its first domain is a direct run
with a real log file, or an explicit singleton verification run. Debugger,
namespace-only and network trace routes remain refused. Other admission checks,
including disabled timer/RCB-time requirements, remain in their existing owners.

`owned/pipeline.py prepare` requires an explicit config and its SHA256, the
exact producer-source manifest SHA256, a fresh evidence name and a fresh
publication directory, normally a fresh `rsrcs` child of the chosen installation
root. Config supplies full clean source revisions, independent
checker sources, the private compiler/sysroot and source-authority inputs,
inspection tools and finite symbolic DATA expectations. No prebuilt runtime or
report can substitute for the actual source-owned archive and one-link
transaction. Run the emitted recipe through `run_pipeline.py`, which adapts it
to the existing stock dagrun engine and the unchanged PID helper. It does not
implement another scheduler or change the producer's qualification checks.

The pipeline retains the original archive, section preparation, native,
ordinary-gate, terminal, optional-use, source/member, mutation and cleanup
controls. It publishes `detcore_private_runtime.elf` first and
`detcore_private_runtime.package.json` last, into the existing Hermit resource
lookup. The consumer retains both opened files and rechecks their identity and
bytes through namespace entry and cleanup. Missing, stale or partial packages
refuse; there is no fallback to the ordinary DSO.

The package descriptor is bounded DATA, not a Ready token or a proof of
executable semantics. Distributing a package also requires the exact source,
ABI and emitted pre-FS entry/relay argument demanded by the unsafe launcher
contract, actual static and dynamic guest checks and cancellation closure.
The owned-link receipt deliberately retains its NOT_PROVEN scope markers.
Post-TLS Tool code is allowed to use its initialized private libc and allocator;
this does not claim to prove arbitrary post-TLS library code.

The separate `loader-rcb-observation` feature retains the observer for exact
retired-conditional-branch comparisons using the shared ptrace PMU event. The
normal opt-in build detaches at handover and does not enable that observer.
Observational count parity does not supply runtime preemption or a product
clock. Threads, successor exec and general startup-signal handling are outside
this first-image cut and keep their named refusals.

Producer DATA controls can be run with:

```sh
python3 -m unittest discover -s hermit-install/private-static/owned -p 'test_*.py'
```

These controls are not a replacement for the actual fresh source-owned build,
link, emitted-code inspection and physical guest/cancellation checks.

Validation runs both control suites in the `check.lint_checks` node, through
the `lint-check-private-static-owned` and `lint-check-private-static-pipeline`
make targets, next to the Reverie pin-currency check.

### Runtime source rows and Reverie pin bumps

`owned/runtime_cohort.py` admits the 30 Hermit and Reverie runtime source
files by rows it reads from Git objects: Hermit rows from the configured H
head, Reverie rows from the commit that head's `reverie` gitlink records. Every
Cargo.lock reverie revision in that head must equal the gitlink, and the
`reverie` submodule must be checked out at it with no tracked changes. The
working tree is then compared with those rows; it is never read to derive
them. A missing object, a disagreeing pin or a dirty or moved submodule is
refused by name. The literal cohorts in that file are historical fixtures.

`reverie-inguest/src/trap.rs` is also bound by shape. Every `global_asm!`
block that pushes `.text.reverie_inguest_trusted_syscall` is hashed: exactly
one `x86_64` and one `aarch64` block, each defining
`reverie_inguest_trusted_syscall`, and nothing else may name that section. The
`x86_64` block's digest is the reviewed guard; the `aarch64` digest is
recorded. Reverie's `ordinary_gate.rs` compiles the other half: the
DedicatedFunction cohort accepts only a 44-byte input section of that name.

Because a re-pin now changes the admitted rows without an edit here, every
Hermit pull request that moves the Reverie pin pastes the pin-bump report
into its description:

```sh
python3 -B hermit-install/private-static/pin_bump_report.py OLD_REVERIE_REV NEW_REVERIE_REV
```

It lists each derived Reverie row whose content changed, with both digests and
a diff stat, and the trap.rs gate-block digests when trap.rs changed. No
`R ` lines means no reviewed runtime source changed.


## Bounded producer launch

First prepare the current config and pipeline binding using the commands above.
The config's `paths.runner` and `digests.runner` identify the canonical
`run_pipeline.py` file and its independently measured SHA256. The adapter and
the stock dagrun binary have separate identities.

Invoke the adapter with flags and actual observed paths and digests:

```sh
python3 H/hermit-install/private-static/run_pipeline.py \
  --name private-package --recipe RECIPE --recipe-sha256 RECIPE_SHA \
  --cpu-seconds 3600 --wall-seconds 3600 --source H --source-head H_HEAD \
  --engine STOCK_DAGRUN --engine-sha256 ENGINE_SHA \
  --engine-provenance STOCK_STAMP --engine-provenance-sha256 STAMP_SHA \
  --pid-helper PID_HELPER \
  --pid-helper-sha256 3c87bfa780d378b9e8313c904ae6fa91ae3e2e57de12275b340122a41c8cccbb \
  --tmpdir PRIVATE_TMP --xdg-cache-home PRIVATE_CACHE --output-root EVIDENCE
```

`H` is the canonical final checkout named by the config. `STOCK_DAGRUN` is an
already built stock binary selected by H's pinned `agent-utils`; passing its
building launcher is not supported. `STOCK_STAMP` is that binary's normal
`.provenance` file, with the stock `source_fp` and `binary_sha256` values. The
adapter checks it against the existing source fingerprint helper and the
pinned checkout. It neither creates a build attestation nor changes an old
binary's expected checksum. `PID_HELPER` is the canonical existing
`pid-bound-v2.py` source with the exact digest shown. The original stays
unchanged; its identical snapshot runs beside fresh `a6-pids.json` evidence.
No helper or private machine paths are vendored into this repository.

`PRIVATE_TMP`, `PRIVATE_CACHE` and `EVIDENCE` must already exist. The run creates
a fresh `EVIDENCE/private-package` directory. Cargo home and the sole canonical
`H/target` come from the hashed config; they cannot be redirected by inherited
build variables. The recipe supplies its existing argv, cwd, env and unset
entries; there is no free-form extra-argument or environment flag.

The one full-DAG step retains separate **3,600 CPU seconds and 3,600 wall
seconds**, eight reserved CPUs, 16 GiB memory, zero swap and 512 tasks. The
unchanged helper checks the live scope, reservation, affinity and memory
events. The outer engine has the original 3,720-second reporting/teardown
budget. `reservation_gib=24` is a projected **disk** allowance: before launch,
available bytes minus 25,769,803,776 must be at least 537,000,000,000. During
execution the same floor and 16 MiB engine-console cap are checked every two
seconds. The disk observations use the canonical `H/target` filesystem, even
when evidence is retained elsewhere. A breach sends TERM and then KILL after
15 seconds. The emitted step keeps its explicit empty job flag and environment
channels, so stock inner-width rendering preserves the payload arguments.
Existing producer 64 MiB stream checks remain in their current owners.

`launch.json` and `receipt.json` record source, adapter, engine/stamp, helper,
recipe/binding/config and cache identities separately. Actual native
exit/signal, guard failures, payload stdout/stderr, PID evidence and stock
profile files are retained even when a run fails. Missing measurements remain
missing. A successful host recipe is not a Ready token, a guest test result or
a FULL validation receipt; DATA and runtime qualification retain their existing
producer and consumer checks.

Focused adapter controls use short native host commands, mocked disk observations
and the pinned AU pure Python model (Python 3.10 or newer). They do not invoke
stock dagrun, its PID helper or the private producer:

```sh
python3 -B -m unittest discover -s hermit-install/private-static -p test_run_pipeline.py
```

## Explicit singleton verification

With `exec-stop-launcher,liteinst` compiled, `--in-guest-exec-stop` on
`in-guest-trap` also admits `--verify` and `--verify-strict`. Both use the
existing verification owner and comparator. Complete stdout, stderr and exit
status are captured; INFO uses the existing bounded sink and sticky loss
latches. There is no success from an incomplete output or failed final flush.
The finite initializer/restoration bounds remain; detached guest execution
uses the verifier's ordinary execution lifetime.

A live pair opens the qualified runtime package once and retains it through
both runs. The existing `verify-inputs` owner also accepts
`--in-guest-exec-stop`; it retains the original image and descriptor files for
both saved `--verify-run` processes. Those runs require a shared manifest and
one explicit epoch. The official pair runner selects this route only when the
run itself explicitly supplies the loader flag. Package identities and bytes
are rechecked at adoption, launch and finalization. Missing, changed, stale or
partial input refuses without substituting another package or launch path.

Omitting the loader flag preserves the selected backend's existing launch path,
selection message and host-admission order. Ptrace remains the default backend;
ordinary in-guest startup continues to use the preload, with the separate native
exec opt-in unchanged. This change does not alter
Detcore, its RPC layout or accounting, ptrace's injected signal-mask
transaction, or the ordinary producer and warm Buck stage. The RCB observer's
feature dependencies are unchanged. Runtime production remains the explicit
pipeline documented above; this is not automatic package provisioning.

Fork-family verification, native successor verification, the library custody
prerequisite, and the eventual default switch are separate changes. Existing
runtime refusals remain; this singleton slice does not establish support for
those domains or change any selected corpus cell. Public `Command` behavior for
working directories, stdio and `pre_exec` remains unchanged, as do `try_into_std`
refusals for configuration that cannot be represented by a standard command.
