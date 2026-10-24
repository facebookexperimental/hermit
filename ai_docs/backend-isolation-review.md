# Independent backend isolation design review

Review target: Hermit base `5a3fdbb3c80a6f1673d8a51f617c660ffa2e6ca3`;
`backend-isolation-design.md` SHA256
`9c4e889debd7bc7192b049c6315167cb11b2ea9311d45911aa4c886b1f6bc7d8`.
Pinned Reverie: `255d2e0b8f5ba017bcc7233711c63583e4fe419e`.

Scope: the design before implementation. Product code and conformance
tests have not been implemented or reviewed; no assurance level or all-backend
completion is claimed. A document change requires a new review of its digest.

## Findings resolved before freeze

No unresolved blocker to this design freeze remains. The following corrections
were necessary; their tests and implementation remain required.

| Finding and evidence | Required correction in the frozen design |
| --- | --- |
| Native launch retains inherited cwd unless explicitly selected (`hermit-cli/src/bin/hermit/run.rs:7822`), while KVM canonicalizes it after mounts (`hermit-cli/src/lib.rs:2794`). Universally refusing a valid default cwd hidden by private `/tmp` would narrow native capability to match KVM's gap. | **That refusal was rejected.** The default cwd strictly beneath `/tmp` remains an executing cell: capture and pin the declared input, stage its bind, recursively expose scratch, enter the logical cwd and retire the child's source fd before runtime startup (`backend-isolation-design.md:105`, `:238`). Explicit missing selected workdirs retain genuine refusal cells. |
| The SaBRe tracing prototype uses a plain subprocess outside the container (`hermit-cli/src/bin/hermit/strace.rs:45`; `backends.rs:1887`). | It consumes the common boundary and retains its distinct tracing-tool classification and required cell (`backend-isolation-design.md:22`, `:242`). It does not become full Detcore backend evidence. |
| A non-recursive host-sysfs bind failed `EINVAL`; a local sysfs read-only remount failed `EPERM` in the native setup probe. | Local sysfs starts read-only. Explicit host networking uses a recursive clone preserving locked submounts and a recursive read-only seal that clears no attributes. Sysfs precedes the empty read-only cgroup mask; no later overmount may hide it. Every mount row remains compared (`backend-isolation-design.md:59`, `:82`, `:94`). |
| CLOEXEC at guest exec cannot remove a descriptor retained by PID1. Thread procfd aliases expose the same capability (`hermit-cli/src/bin/hermit/owned_container.rs:240`; pinned `reverie-process/src/container.rs:1372`). | Sanitize unsolicited inheritance at single-threaded CLI entry; retain explicit owned handles with their full inventory, ownership and capabilities. Include PID1 thread fd/fdinfo/cwd/root aliases (`backend-isolation-design.md:139`, `:146`). No blanket runtime-fd exemption. |
| Detcore namespace readlink identities are fixed even when membership differs (`detcore/src/syscalls/namespace.rs:49`, `:406`, `:575`). | Label them emulated; establish physical identity through raw stat/fstat and the independent observer. `--no-virtualize-metadata` does not disable this readlink policy (`backend-isolation-design.md:261`). |

## Descriptor comparison boundary

The full guest-observable surface remains an equality assertion: fd numbers and
closed states, object types, flags, access modes, targets, aliases and reachable
capabilities, including PID1/thread procfd/fdinfo/cwd/root views. Fd numbers are
guest API values, not namespace or mount identities to rename. A runtime purpose
does not exempt a visible socket or memfd row. Differing native runtime/worker
inventories remain raw evidence and require individual ownership/provenance and
proof that their differences are not guest-observable or reachable; otherwise
the required mismatch remains. Numerical reuse of fd42 does not establish a
leak, but the original unsolicited host directory object must be unreachable by
every direct operation and alias. An intended normalizer that drops visible
runtime rows would require a reviewed amendment and is not approved here.

## Determinism conclusion

The design preserves the shared Detcore scheduler, committed-turn accounting,
continuous virtual time, syscall policy and strict comparison. One resolved
configuration produces the same setup recipe before backend selection; DBT
creates its runtime and evidence listeners only after its owned physical
container exists (`backend-isolation-design.md:14`, `:34`, `:168`).

Implementation must preserve logical guest IDs, physical-exit barriers, signal
targets, result classification and worker joining. Moving async startup must not
introduce a fork of live workers or an extra scheduler turn. Fresh namespace
numbers and varying internal task numbers are raw kernel facts; structural
identity comparison in the isolation test does not authorize stripping them
from strict INFO verification or changing virtual time. Both verify runs need
fresh physical setup and retained evidence. Supported record/replay paths need
independent physical observation because replayed bytes are not live proof.

## Linux and POSIX conclusion

The design uses real namespace membership and private mount propagation.
The native feasibility probe established real PID1, all seven private namespaces
for local networking, and six private namespaces with an inherited network
namespace for explicitly selected host networking. The staged local sysfs view
listed only loopback; the host view listed the host interfaces. Both cgroup
masks were empty tmpfs mounts and refused creation with `EROFS`.

These measurements establish recipe feasibility on the measured kernel, not
product conformance or containment of resources. The host sysfs recipe retains
locked flags and copied submounts; missing `mount_setattr` support explicitly
refuses setup. Device permissions remain the caller's: optional `/dev/kvm` is
one configured input for every backend, not a backend-only escape. Stdio can
retain an explicitly inherited terminal. Missing devices are not fabricated.

PID values are namespace-relative. The host sentinel assertion binds its actual
process identity and namespace/NSpid relations; numeric reuse by an unrelated
inner task is permitted Linux behavior (`backend-isolation-design.md:250`).
Default declared cwd and hidden program inputs are preserved. Explicit user
mounts and selected roots remain authoritative. GDB needs explicitly selected
host networking; it cannot silently change a recorded or configured choice.

## Evidence and goalpost-moving conclusion

> GOALPOST-MOVING REVIEW RULE
>
> Adversarial reviewers must look explicitly for goalpost moving. We are extremely skeptical of any goalpost moving. YOU DO NOT CLEAR THE BAR BY SIMPLY LOWERING THE BAR.
>
> Treat each of these as an explicit review target:
> - weakening an assertion so a test passes
> - widening a tolerance · adding an exemption · skipping a case · relaxing a comparator
> - renaming or relabelling so a failure reads as a pass
> - deleting a check rather than satisfying it

- **Assertions weakened: no.** The default cwd-under-`/tmp` successful cell was
  preserved after rejecting the proposed universal refusal. DBT must fail the
  baseline PID/UTS/scratch assertions; other baseline defects remain visible.
- **Tolerance widened, exemption added, case skipped or comparator relaxed:
  no.** All required backends, configured roots, modes, mount rows, errors and
  fd aliases remain. Only injective kernel-identity/backing-path relabeling is
  allowed, retaining structure and raw facts. Strict `BitwiseInfoV1`, exact
  stdout/stderr/status and nonzero compared INFO counts remain unchanged.
- **Failure renamed or relabelled as a pass: no.** KVM guest-view failures remain
  required failures. The SaBRe tracing prototype and e9patch preprocessing keep
  their actual classifications. Raw metadata measurements are labeled isolation
  evidence with their relaxation and do not claim L2.
- **Check deleted instead of satisfied: no.** No product/test checks have been
  deleted. The frozen matrix and goalpost rule prohibit a later skip, allowance,
  missing-row normalization or synthetic-green claim (`backend-isolation-design.md:230`, `:271`).

## Verification and residual risks

Grounding was completed before reading the proposal: the complete 16-page
ASPLOS paper, DOI `10.1145/3373376.3378519`, including its artifact appendix;
all 16,609 lines of base `detcore/src/scheduler.rs`, including tests; then the
complete project vision and v2 roadmap. The author-hosted paper SHA256 is
`6346dfa0e5ea193fb975d5330da80d030f2a7641d95c7063069c8f8d7d875539`.
Complete relevant container/owned-run, DBT, backend dispatch and mode contexts
were read at the fixed base, with dependency evidence bound to the actual pin.
The final proposal was read completely and its digest checked independently.

The ignored native helper and both v2 receipts were inspected independently:
exit 0, empty stderr, real namespace stats, sysfs mount rows, empty cgroup masks
and `EROFS`. The earlier failed recipes remain retained. No Hermit run, build,
full validation, commit or publication was performed by this reviewer.

KVM still requires a real guest-visible proc/namespace view. Its pinned synthetic
proc policy omits namespace/PID1 observations and refuses broader procfs access
([policy](https://github.com/rrnewton/reverie/blob/255d2e0b8f5ba017bcc7233711c63583e4fe419e/reverie-kvm/src/executor.rs#L17164));
the carrier alone cannot establish that cell. A minimal repair can project
actual carrier-owned namespace/proc objects through the existing guest fd model,
with faithful self/PID1 aliases and independent carrier proof. It must not expose
arbitrary carrier fds or fabricate fixed metadata. Any paired dependency change
needs its exact base/commit handoff; an unreachable pin is not a deliverable.

e9patch's extra executable mount and every runtime file/descriptor surface also
remain required observations. Pinned-root recipes, all physical verify runs,
record/replay, debugger paths, input-binding retirement and signal/cleanup paths
still need exact-head implementation review and focused checks. Shared launch
and DBT repairs may be handed back as partial landable stages; missing required
cells prevent an all-backend completion claim.

## Verdict

**Approve the end-state design and freeze the exact digest above.** Product code,
tests and any new descriptor-projection strategy remain unreviewed.
