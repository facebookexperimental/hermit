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
transaction. Run the emitted recipe under the existing bounded runner; the
producer does not supply a second runner.

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
