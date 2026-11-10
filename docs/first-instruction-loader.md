# First-instruction loader integration

`hermit run --in-guest-exec-stop` explicitly selects the packaged singleton
first-image launcher on `in-guest-trap`. With the `exec-stop-launcher` and
`liteinst` features, a private runtime package and supported options, the CLI
admits direct runs and verification through the existing container and mapped
execution owner. Unsupported combinations or a missing package retain the typed
`exec-stop-runtime-not-ready` refusal. A requested verification report is stamped
with this invocation's NO-RESULT record before fallible admission.

Omitting the flag preserves the selected backend's ordinary launch path; ptrace
remains the default backend. Ordinary in-guest preload and native-exec selection
and diagnostics remain unchanged. This CLI adapter does not change public
library `Command` working-directory, stdio or `pre_exec` behavior, or the existing
`try_into_std` refusals.

## Launch ownership

The `exec-stop-launcher` Cargo feature carries the shared launcher. It prepares
an explicit executable, argv, environment, pinned working directory and
borrowed bootstrap descriptors; retains the child's original pidfd/generation;
reaches the kernel's exec stop and drains only the kernel exec return. On the
admitted path, `RunOpts::run` opens the retained runtime image and
`run_mapped_in_container` hands it to `mapped_first_admission::run_mapped`
(`mapped_run.rs`), which maps the private runtime, initializes Detcore before
any guest instruction, and then releases the guest through `handover_status`.
Cleanup confirms the child's reap before any borrowed resource is released,
including after a normal guest exit. The launcher smoke test in the ordinary
Hermit unit suite stops short of that: it captures entry state and cancels
with an observed terminal wait, releasing no guest instruction.

Reverie's separate `stage1a-probe` feature keeps the earlier mapping and
test-runtime handoff probe. Passing it, including its static glibc smoke, is
not evidence about the production path.

## Singleton verification

The explicit loader supports `run --verify` and a saved `--verify-run` pair under
the same verification owner and comparators as other supported runs. Verification
requires the normal namespace owner. A saved pair must supply one explicit epoch
and the existing `verify-inputs --in-guest-exec-stop` holder. That holder seals the
shared inputs and transfers the original read-only runtime ELF and descriptor
file descriptions to both consumers. Inline verification opens the package once
and retains the same image owner through both executions.

After capture, resource path discovery is not an input to either run. Moving the
package's parent directory does not select another image. Retained file identity
and bytes are checked before execution and after owned finalization, including
error outcomes; changing the original inode remains a refusal. Frozen stdin and
`ld.so.cache`, complete stdout/stderr/status, forwarded INFO records, loss
latches and the common comparison/reporting checks remain required. The strict
comparator retains its canonical host-address treatment; it does not claim raw
host-address equality.

The adapter uses the original stopped-child generation, mapping, initializer,
physical wait/cancellation and typed finalization owners. Captured output is not
published as a completed run merely because its pipes reached EOF. A package
descriptor describes bytes; it does not supply source qualification, mapping
ownership or readiness on its own.

## Qualification status

`mapped_run.rs` calls this path an intermediate opt-in whose full landing
qualification remains separate. The description of 059e069bfae reports what
was measured when it was activated: a private musl runtime owns its libc, TLS,
allocator and reserved Tool memory, so initialization does not run the guest
libc; and at Hermit 7b9f16cd with Reverie 1c5188a3 a static hello matched the
ptrace backend at all 15 syscall-entry boundaries and a dynamic hello at all
36, with matching INFO and memory records.

Full qualification is still the bar written before activation, and that
commit reports only the two hello images, not the whole of it:

- a **production ToolRegion reservation and authenticated adoption**: the
  runtime and loader image windows and guards permanently reserved in the real
  allocator, ownership authenticated before adoption, and the resulting runtime
  qualified with M1. An address or a record magic value alone is not ownership
  evidence, and no existing guest mapping may be replaced to make adoption
  succeed;
- release only through a genuine readiness transition; a fixture READY token
  cannot grant it;
- acceptance across static no-libc and glibc-static programs, dynamic startup,
  threads and exec re-entry, with full syscall, instruction-count,
  heap/stack-hash and INFO/DETLOG parity. Threads and exec re-entry are still
  refused today.

This singleton slice does not admit fork-family or native-successor verification;
`--native-exec-inputs` with the loader remains refused. It does not change the
public-library launch route or make the loader the default. Thread, fork and
exec-reentry support, broader static/dynamic program coverage, and full runtime
parity require their own implementation and evidence. This document describes
source behavior; it asserts no new runtime pass or corpus coverage at this head.
