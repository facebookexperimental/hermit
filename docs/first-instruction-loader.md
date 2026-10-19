# First-instruction loader integration

`hermit run --in-guest-exec-stop` reserves an opt-in for the shared first-image
launcher. It currently fails with the typed policy refusal
`exec-stop-runtime-not-ready`. This also applies to `hermit oci run` and to
every backend selection. Refusal precedes host admission, image-store access,
stdin capture, container creation and guest execution. A requested verification
file is first replaced with this invocation's NO-RESULT record. Omitting the
flag preserves the existing launch path.

The optional `exec-stop-launcher` Cargo feature exposes the shared launcher
dependency. The ordinary Hermit unit suite exercises it inside the existing
physical container. It prepares an explicit executable, argv, environment,
pinned working directory and borrowed bootstrap descriptors; it retains the
child's original pidfd/generation, reaches the kernel's exec stop, drains only
the kernel exec return, captures entry state and cancels with an observed
terminal wait. No guest instruction is released by this production interface.

The mapping and test-runtime handoff are available only in Reverie's separate
`stage1a-probe` feature. Passing that probe, including its static glibc smoke,
does not establish genuine Detcore readiness or production static support.

## Required before activation

The next integration must satisfy the named **production ToolRegion reservation
and authenticated adoption prerequisite**: permanently reserve the runtime and
loader image windows and guards in the real allocator, authenticate ownership
before adoption, and qualify the resulting runtime with M1. An address or a
record magic value alone is not ownership evidence. No existing guest mapping
may be replaced to make adoption succeed.

Real Detcore must initialize without executing guest libc or constructors and
arm interception before the original loader or static entry executes. Release
requires a genuine readiness transition; a fixture READY token cannot grant it.
Acceptance includes static no-libc and glibc-static programs, dynamic startup,
threads and exec re-entry, with full syscall, instruction-count, heap/stack-hash
and INFO/DETLOG parity. This commit does not waive or claim those gates.
