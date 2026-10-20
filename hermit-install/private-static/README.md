# Private static first-image runtime

This producer is separate from the ordinary GNU LiteInst DSO finalizer. It
builds the private musl image used only by Hermit compiled with
`exec-stop-launcher,liteinst` and launched with `--in-guest-exec-stop` on
`in-guest-trap`. The flag is off by default. Its first domain is a direct run
with a real log file, without the verify, debugger, namespace-only or network
trace routes. Other admission checks, including disabled timer/RCB-time
requirements, remain in their existing owners.

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
