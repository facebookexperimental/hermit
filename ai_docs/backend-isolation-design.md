# Shared backend container contract

Freeze is established by exact-digest adversarial approval recorded in
`backend-isolation-review.md`; implementation requires that approval.
Source baseline: `5a3fdbb3c80a6f1673d8a51f617c660ffa2e6ca3`.
The companion [source audit](backend-isolation-audit.md) binds source citations
and will receive exact-baseline live evidence. The owner requires the same
namespace, filesystem and network isolation for every backend. Known DBT gaps:
https://github.com/rrnewton/hermit/issues/4009 and
https://github.com/rrnewton/hermit/issues/4002.

## One physical-run boundary

Resolve CLI/environment/config-file precedence once into a backend-independent
`ContainerPlan`: namespace mode, network mode, declared input root, scratch and
workdir mounts, ordered user mounts/binds, identity inputs, affinity and stdio.
Materialize this plan with one setup implementation in
`hermit-cli/src/bin/hermit/container.rs`. `run`, each physical verify run,
record, and replay consume it through `owned_container::run`. Backend selection
happens inside the completed container. No backend may apply a weaker helper
recipe, silently inherit an unsupported root, or select a fallback backend.
The SaBRe `strace` prototype also launches a guest and must consume this common
isolation boundary, while remaining explicitly classified as a tracing tool
rather than full Detcore execution.

The default contract creates private **user, PID, UTS, mount, network, IPC and
cgroup namespaces**. Namespace identity equality is a relation within one run;
independent runs necessarily have different kernel inode numbers. The common
init is real PID1; native guest tasks and syscall carriers belong to its private
PID namespace. Detcore's root `getpid` and aliasing semantics stay unchanged.
Backend worker counts may change raw task numbers; they may not expose a host
PID tree, confuse guest/process aliases, or skip the private PID namespace.

Use the existing parent-owned lifecycle: clone before creating Tokio/tracing
workers, retain guards until proven child retirement, arm PID1 parent-death and
stop-signal guards, preserve owned result transport and failure classification,
and join backend workers before finalization. Do not change Detcore scheduling,
virtual time, strict comparison or unsupported-syscall policy to obtain parity.
Changing init/runtime concurrency is subject to the scheduling review even if
the change appears to be launch plumbing.

### Namespace invariants

| Namespace | Default invariant | Explicit relaxation |
| --- | --- | --- |
| User | New namespace for privileged and unprivileged callers; map guest root to caller effective UID/GID; deny supplementary group remapping. | `--no-namespace` inherits credentials and namespace. |
| PID | New namespace; common init PID1; procfs mounted from that namespace; host sentinel process identity inaccessible; real task/carrier membership independently observed. | `--no-namespace` shares process tree; existing replay restrictions remain. |
| UTS | New namespace; nodename `hermetic-container.local`, domainname `local`; kernel hostname reads cannot reveal caller identity. | `--no-namespace` inherits kernel UTS; existing deterministic uname emulation is labeled separately. |
| Mount | New namespace; recursively private propagation before attaching mounts; all mount targets rebased to declared root before chroot. | `--no-namespace` applies no Hermit mounts or chroot. |
| Network | New namespace with only `lo`, brought up; sysfs interface view agrees; no caller port or abstract Unix-socket reachability. | Explicit `--network host` shares only network namespace. `--no-namespace` forces this choice. |
| IPC | New namespace; private POSIX shared-memory filesystem; host SysV IPC sentinel unreachable. | `--no-namespace` inherits both IPC namespace and shared-memory view. |
| Cgroup | New namespace rooted at caller's current cgroup; empty read-only tmpfs masks `/sys/fs/cgroup` with `mode=0555,size=4096` and `nosuid,nodev,noexec,ro`; no guest ability to move host tasks or adjust outer limits. | `--no-namespace` inherits the caller view; no resource-limit promise is implied. |

Linux user-namespace, IPC, cgroup and mount support must be admitted before a
guest starts. Unsupported kernels or denied operations produce an explicit
refusal with a no-result record, never a shared-namespace fallback. Cgroup
namespaces hide ancestry; resource containment stays owned by the outer runner.
The pinned-root runner's privilege cannot bypass user namespace creation.
The cgroup mask is a uniform sealed view, not a writable controller mount.
Materialize root-relative sysfs first: initially read-only for Local, or the
configured recursive bind for Host. Attach the read-only cgroup mask next; apply
Host's recursive read-only seal last. No later sysfs overmount may hide the mask.
Namespace creation or masking failure is a refused setup, not an alternate
absent/writable cgroup view.

## Filesystem contract

Root selection is a declared input. Host-root and the digest-pinned outer
runner are separate configuration cells; equality is required across backends
within each cell. Host-root intentionally exposes the declared caller root and
its input files: it is not a snapshot or general filesystem security boundary.
The isolation assertions below apply to scratch, process/device/control views
and undeclared inherited capabilities. The CLI image prototype and replay's
materialized root are additional root policies, not aliases for host-root.

| View | Shared policy |
| --- | --- |
| Root | One declared root with a common write policy and common ordered explicit input binds. Image/cache root is read-only; host-root input mutability is stated. Resolve and pin sources before overmounting; construct targets before installing guest-controlled symlinks. |
| `/tmp` | Fresh backing directory per physical run by default, mounted with one common recipe; explicit `--tmp` is the same bind on every backend and intentionally persists its supplied contents. Record must adopt default private scratch rather than keep host `/tmp`. |
| Working directory | Capture default logical caller cwd before namespace setup. When strictly below default private `/tmp`, preserve it as one declared input bind into the new scratch tree, so the guest still executes there. Exact `/tmp` re-enters fresh scratch itself, never the entire host tmp tree. Explicit `--workdir`, image workdir and recorded replay cwd resolve in their completed root; genuinely absent selected targets refuse uniformly. Explicit user mount/bind/tmp intent wins. |
| `/proc` | Fresh private-PID procfs in the declared root; require PID1/self/namespace/fd/mount relationships and absence of host PID access. Kernel read-only fallback is a reported common mount option, not silently different per backend. |
| `/sys` | Local network: fresh sysfs mounted initially `MS_RDONLY|MS_NOSUID|MS_NODEV|MS_NOEXEC` in its owned network namespace. Explicit host network: recursive bind of configured caller sysfs, preserving inherited locked submounts, then recursive `mount_setattr` sets `MOUNT_ATTR_RDONLY|MOUNT_ATTR_NOSUID|MOUNT_ATTR_NODEV|MOUNT_ATTR_NOEXEC`, clearing no attributes. Both receive the identical empty read-only cgroup mask at root-relative targets; interface listing agrees with selected networking. |
| `/dev` | One private tmpfs view with source-bound null/zero/full/random/urandom, private devpts, `/dev/fd -> /proc/self/fd` and matching stdin/stdout/stderr aliases, and private `/dev/shm`. Add `/dev/kvm` read/write for every backend iff the configured runner supplies it; preserve source device identity/permissions and locked flags, never fabricate a device or promote access. KVM explicitly refuses unavailable/inaccessible devices. No inherited host devpts, controlling-terminal path or extra device tree; declared stdio may still carry a terminal. |
| Runtime material | No backend-specific visible mount is exempt from comparison. Prefer inherited controlled descriptors and files in a common reserved runtime location. If a backend requires a visible mount/file/descriptor, define that exact surface for all backends or report the cell as a real remaining gap. |

Whole mount tables are compared, preserving root/source relationships, filesystem
types, flags, propagation and all rows. The inherited outer runner mounts are
declared inputs and must match across backend runs in the same runner cell.
Temporarily allocated backing paths and kernel mount IDs may be injectively
renamed while preserving identities, parent links, order and aliases. Contents,
permission failures, mount options, absent rows and backend resource mounts may
not be discarded.

Fresh sysfs with a new user namespace but the caller's host network namespace
can be denied by Linux ownership checks. The explicit host-network bind policy
avoids depending on that privilege and deliberately exposes the declared host
network/hardware view. Less-privileged mount namespaces can reject a
non-recursive bind that omits locked inherited submounts. Preserve every copied
mount row, including a masked underlying cgroup2 row. The recursive read-only
operation requires kernel `mount_setattr` support and must refuse explicitly if
unavailable. This is one preselected policy across backends, not a setup-error
fallback. Both recipes and all seven namespaces were measured successfully in
the native setup probe; this establishes feasibility, not product conformance.

The default cwd strictly below `/tmp` is a required executing cell, not an
unsupported allowance. Before mounts, open/pin its source directory with
`O_PATH|O_DIRECTORY|O_CLOEXEC`; create the logical target inside the private tmp
backing tree; bind from `/proc/self/fd/<source>` before the final `/tmp` bind.
That final bind explicitly uses `Mount::recursive()` so it carries staged
submounts; private propagation is retained. `Mount::rshared()` is not a substitute
for recursive binding and must not reconnect propagation to the caller.
Keep its source as `Option<File>` in `owned_container::run`'s guard value. The
universal child work wrapper takes and drops that file immediately after
container setup/init guards and before any runtime, logger or backend work.
The parent's fork copy remains owned until child reaping. Existing `Mount::bind`
and guard transfer support this without a new core API; no raw close or
double-close scheme is used. Allocate default scratch outside the preserved cwd
to avoid self-referential binds when a caller's temp directory is below cwd.

Resolve/pin declared executable and interpreter inputs before overmounting.
Preserve program inputs hidden by default `/tmp` in the same common declared
input policy, retaining logical pathname/argv0 and user-mount precedence. This
keeps relative/default launches working during DBT convergence. Other explicit
guest file inputs continue to require the caller's normal root/bind policy.
Retain all input-bind rows and source aliases in conformance observations.
Conformance's normal cwd is the same fresh writable input per backend; the
cwd-under-tmp cell separately checks getcwd, relative open and PID1/guest
cwd/root aliases. Explicit user-selected cwd that genuinely cannot resolve can
refuse. Replay resolves recorded cwd inside its materialized root; its
controller uses the declared root rather than an unreachable inherited cwd.

The e9patch executable overlay is preprocessing with ptrace, not a new runtime.
Its extra mount is a real filesystem observation: the implementation must use
the common declared executable-artifact policy or report the exact unresolved
topology difference. The full required comparator keeps that row.

## Descriptor contract

Inherited directory/file/socket descriptors can bypass pathname overmounts.
Default CLI entry sanitization closes unsolicited inherited descriptors before
opening log/result/resource handles or starting workers; preserve stdio and
explicitly documented internal helper handshakes. Respect startup stdin's
captured identity and closed-stdio semantics. Do not close an arbitrary
`fd >= 3` range after Tokio/logging has created its transports.

One owned descriptor inventory follows deliberate handles into PID1, carriers
and guest startup. Guest inherited capabilities are stdio plus any explicit
declared input policy, identical across backends. Runtime handles are recorded
individually with type, flags, target and alias relationships. Deliberate owned
log/result/bootstrap/RPC/statistics handles are permitted only under that
declared inventory; their visibility and capabilities remain observations, not
a hidden-handle claim. Unsolicited host input directories, parent procfs and
caller control sockets must not survive through direct guest fd operations or
proc aliases. Enumerate `/proc/1/fd`, `/proc/1/task/<tid>/fd`, `/proc/self/fd`
and corresponding fdinfo/cwd/root aliases; thread-specific aliases cannot be
ignored. A blanket exemption for high fds or all runtime transports is
prohibited. This is the same capability policy across backends, not a new
general-purpose security sandbox.

Marking an unsolicited descriptor CLOEXEC only at guest exec is insufficient:
PID1 may keep it reachable. Closing unrelated descriptors belongs at the
single-threaded entry boundary; deliberate guard/result/log descriptors remain
owned and protected through the existing completion boundary. Descriptor APIs
that lack this policy remain explicit lower-level library contracts.

## DBT and KVM obligations

Remove DBT's early isolation exception. Each DBT physical execution enters the
shared owned container first, then creates its two-worker coordinator runtime
and the namespace-local RPC/abstract evidence listeners. Run1 and run2 each get
new namespace and scratch resources, including stdin replay and their own
statistics lifetime. Parent-owned input/log/verdict/statistics resources survive
the namespace transition without binding arbitrary host `/tmp` directories back
into the guest. Preserve the existing verified Detcore event stream, retained
log names, typed verdict, terminal-input policy, process-group cleanup, and
resource-release order. The test-only workdir helper can retain its independent
test API; it no longer supplies DBT CLI isolation.

KVM's host syscall carriers use the same completed plan, and its guest-visible
proc/namespace interface must expose that isolation with faithful documented
relations. Synthetic fixed namespace names, unsupported `/proc/1`, or unavailable
namespace metadata do not satisfy conformance. Both the native carrier observer
and the guest report are required. Necessary guest-view fixes must use actual
carrier-owned proc/namespace provenance, receive review, and preserve all
required assertions. A required missing cell is a blocker to an all-backend
completion claim. Shared launch and DBT convergence can be handed back
independently as landable partial repairs while KVM guest cells remain
explicitly failing. If a paired Reverie change is needed, keep it in the slot's
initialized submodule and hand back its exact base and commit for coordinator
publication; do not silently pin an unreachable dependency revision.

## Modes, precedence and unsupported cells

`--no-namespace` means no Hermit-created namespaces, scratch mounts or chroot on
every supported backend, including DBT with test-workdir environment markers.
Keep its usage conflicts and schedule/preemption replay refusals; issue the same
warning before dispatch. It is measured as a deliberate shared-host control,
not an isolated pass. The seven namespace observations must agree with that
policy even when Detcore emulates uname, PIDs or namespace symlink text.
The deliberate descriptor policy still applies: no-namespace shares its stated
host views, but does not implicitly authorize arbitrary inherited descriptors.

Networking precedence is resolved once for local/host, analysis, capture/replay
and debugging. The existing TCP GDB transport requires explicitly selected
`--network host` for run, record and GDB-served replay; the replay CLI must expose
that same option. A local-network GDB request is refused before launching a
guest with an actionable explanation. Debugger selection cannot silently
override recorded/configured networking. Existing strict host-network refusal
and typed verification behavior remain; a private debugger transport is a
separate change.

Record/replay supported engines stay ptrace, including e9patch preprocessing on
record. Every unsupported backend/mode combination is retained as an explicit
refused cell and must never fall back to ptrace. Replay consumes recording args
unchanged and shares physical namespace/scratch policy, while its materialized
root and recorded filesystem contents remain the replay input contract.
Replayed report bytes alone cannot prove current physical isolation.

## Conformance and review freeze

Use one guest source with explicitly named dynamic/static builds where runtime
support requires them. It reports getpid/parent relations, PID1, hostname, all
seven namespace inode identities and aliases, interface list and loopback,
complete mount table, root/tmp/proc/sys/dev/cwd observations, and descriptors.
Unavailable observations retain exact syscall/error results and fail a required
cell; they do not disappear from the matrix. A native launcher captures a host
baseline, injects sentinel PID/files/IPC/socket/directory fd, and independently
observes only the bounded CLI descendant tree through completion.

Required matrix: ptrace, DBT, SaBRe, LiteInst, in-guest-trap, KVM, and e9patch with
ptrace; host-root and pinned-root runners; ordinary and verified physical runs;
default isolation, explicit host network and no-namespace controls; supported
record, verified record and standalone autopilot/GDB replay paths; explicit
refusals for unsupported backend/root/mode combinations. Mount/bind/tmp/cwd
options receive shared-policy cells. PID1 and guest/carrier descriptors are
required, not guest-only. Record/replay physical observations use the independent
observer rather than captured report contents.
Include the executing default cwd-under-`/tmp` cell, explicit missing-workdir
refusals, hidden program-input retention, and each thread-specific PID1 procfd
alias. The default cwd preservation rule cannot be changed to a refusal to make
a backend mismatch disappear.
Retain a separate tracing-tool cell for the SaBRe `strace` prototype and a
namespace-only smoke cell; neither is counted as full Detcore backend parity.

Assertions require all seven default namespace identities differ from the
immediate host, same-run guest/PID1/carrier namespace relationships, no host PID
visibility, unchanged deterministic getpid semantics, canonical hostname,
private loopback, identical full configured filesystem and descriptor policy,
and inability to access sentinels through paths, inherited fds or PID1 procfds.
The PID sentinel denotes an actual host process, not a globally unique numeric
PID. Its number may be reused inside the new namespace, especially when the
immediate host is itself a container. Retain raw namespace, NSpid and process
identity evidence, and assert that the guest cannot identify or control that
host process. A same-number guest process is a different object; absence of
every same-number proc pathname is not a valid Linux isolation assertion.
ID relabeling preserves structure; it does not mask unsupported capabilities or
different visible views. The baseline DBT default must fail real PID/UTS and
scratch-isolation assertions. Shared IPC/cgroup and other discovered defects
must also remain visible in baseline results.

Raw kernel measurements use labeled `--no-virtualize-metadata` relaxation and
are isolation evidence, not L2. Namespace readlink targets remain explicitly
labeled emulated even with this flag; physical identity and aliases come from
raw stat/fstat plus the independent observer. Never require the emulated link's
embedded inode number to equal the raw kernel stat inode. Separate normal
strict verify checks retain
Hermit's existing `BitwiseInfoV1` comparison, exact stdout/stderr/exit behavior
and nonzero INFO counts. No new stripping of numbers, virtual time, syscall
payloads, mount rows, aliases or required cells is permitted.

The design freezes only after deterministic scheduling and code reviewers read
the exact document digest and relevant complete launch/runtime code, record
blocking findings, and approve the corrected digest. **Goalpost-moving rule:**
the required cells, comparators and assertions are fixed by this reviewed
contract before product/test implementation. A failing cell cannot become a
skip, unsupported allowance, weaker comparator or synthetic isolation claim to
make the patch green. New evidence can change the design only through an
explicit reviewed amendment that records the original requirement, evidence
and owner's authorization where scope changes. Pending or refused required
cells prevent an all-backend completion claim.

Implementation should land in coherent local commits after freeze: contract and
conformance baseline, shared launch/DBT migration, common namespace/filesystem/fd
repairs, then remaining mode-specific repairs. Focused regression checks precede
format, Clippy with warnings denied, rustdoc with warnings denied, DAG/sync and
pinned DBT inventory checks. Full validation and publication stay with the
coordinator.
