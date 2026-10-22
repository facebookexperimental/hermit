# Deterministic-scheduling review: guest mountinfo membership (procfs)

Base: `422f3f3a4e05353edd4f2449affc8df9241bdf51` (exact, fetched 2026-09-25).

## Grounding order (skill §0–§1)
- ASPLOS paper (Kaiser et al., external ACM link): unreachable from this host
  (no direct egress; pane allowlist excludes curl) — no design claim depends
  on it; normative sources below are local and primary.
- `detcore/src/scheduler.rs` at base (read: deterministic core, timed events,
  RCB/committed-time, run-queue/deadlock rules) + `DETERMINISM_ARGUMENT.md`
  (causality scope, entropy inventory) + `PROJECT_VISION.md` (deterministic
  record & replay as canonical interface) + v2 roadmap guest-semantics notes.
- Linux: `/proc/<pid>/mountinfo` (proc(5)): mount ID, parent ID, major:minor,
  root, mountpoint, options, optional shared/master/propagate_from peer
  groups, ` - ` separator, fstype, source, super options. Shared/slave
  propagation imports peer-group mounts into a namespace asynchronously.
- Full affected path read: `detcore/src/syscalls/files.rs`
  (`snapshot_procfs`/`initialize_procfs_snapshot`, fdinfo tracer snapshot),
  `detcore/src/procfs.rs` (`MountInfoSnapshot`, `sanitize_mountinfo`,
  `sanitize_fdinfo`, `syscall_time`), `detcore-model/src/procfs.rs` parsers,
  `hermit-cli/src/lib.rs` producer provenance capture.

## Frozen semantics / design selection
Guest mount namespace under Detcore is a launch-defined, immutable object:
files.rs already records "Mount/unshare/setns are refused once Detcore
starts". Membership of the deterministic guest view is therefore the launch
namespace minus one class of rows, excluded as a chosen determinism fidelity
trade (they are real propagated mounts, excluded for determinism scope):

**Excluded class** — `fuse.squashfuse_ll` mounts whose mount point's last
path component is a host seed name,
`<hex>-seed-<seed>-ns-<digits>`, where `<seed>` is a non-empty run of ASCII
letters, digits, `_` and `-` (normally shown at
`/mnt/xarfuse/uid-<uid>/<seed name>`): seed mounts created by host squashfuse
infrastructure, imported by shared propagation, lifetime bound to unrelated
host processes. The seed is either per-process
(`nspid<digits>_cgpid<digits>`, embedding host namespace and cgroup PIDs) or
names a host tool (`chef`, `fb-pcie-error-log`,
`devserver-cleanup_hg_cache`). Named seeds were observed living 4–5 s on
devbig030, and `chef` was remounted under a new mount ID; a named seed that
happens to live longer is excluded too, as part of the same fidelity trade.
The rule keys on the seed name, not the directory, so a long-lived
SquashFUSE mount such as `/mnt/xarfuse/stable-release` stays visible, and a
changed guest root that displays the seed as `/xarfuse/uid-<uid>/<seed name>`
still excludes the row the launch-time capture excluded. The one predicate is
`detcore_model::procfs::is_ephemeral_host_seed_mount`, applied through the one
filter `detcore_model::procfs::exclude_ephemeral_host_seed_mounts`. This is the PID-virtualization
analogue for mounts: other tenants' runtime state. Everything else stays:
kernel/system rows, shared filesystems (edenfs/manifold/btrfs), binds,
overlays, tmpfs, and every Hermit-configured mount (`--mount`, container
provenance, `/test`, `/tmpvol/.hermit/*`).

Alternatives rejected: launch-only full capture (does not fix cross-run
churn — the failure is between two invocations); synthesized fixed table
(hides legitimate guest-visible shared mounts); comparator/guest change
(weakening, forbidden).

Applied at every membership consumer: procfs capture (`initialize_procfs_
snapshot`), fdinfo tracer snapshot, and producer provenance
(`capture_mountinfo_identity_order`) — otherwise seed rows would still
shift run-global mount-ID assignment or captured identity order.

## Event path / lifecycle / backends
open/read of mountinfo → first-read capture (class-filtered) → snapshot
identities assigned over filtered membership → seqfile reads render from the
same filtered contents. dup/fork share the open-file snapshot (unchanged);
exec creates no new namespace under Detcore (refusals unchanged);
record/replay: capture is a host read at first use in both modes and the
filter is a pure function of contents, so replay renders identically.
Backends: ptrace measured end-to-end here; liteinst/sabre share Detcore
procfs code (claims limited to ptrace — no liteinst/sabre cell changes);
kvm path uses the same CLI provenance capture. Guest mount changes remain
visible exactly as before: Hermit launch/configured mounts and all
non-seed host rows (regression test asserts `/test`, edenfs, binds survive;
unit tests assert that a non-seed SquashFUSE row under `/mnt/xarfuse/` and a
seed-named row of another filesystem type survive).

## Evidence
- Unit (detcore-model): `ephemeral_host_seed_mount_class_is_the_seed_name`,
  `named_host_seeds_are_excluded` (the three named-seed rows observed in a
  20-minute mountinfo churn log, plus `/mnt/xarfuse/stable-release` and a
  tmpfs row carrying a seed path as negatives; it fails under the earlier
  `nspid<digits>_cgpid<digits>`-only grammar), and
  `seed_filter_drops_only_seed_rows_and_keeps_order`.
- Unit (detcore): `seed_churn_does_not_change_guest_mountinfo_membership`
  (old leak fails: churn variants differed before, identical after, through
  sanitize), `retained_row_with_seed_parent_still_snapshots`, and the source
  guard `snapshot_initializer_excludes_host_seed_mounts_at_both_captures`,
  which fails if either capture in `initialize_procfs_snapshot` stops calling
  the filter.
- Unit (hermit-cli): `identity_capture_excludes_host_seed_mounts_and_keeps_order`.
- E2E, 2026-09-25, under the original `/mnt/xarfuse/` prefix rule (not
  re-measured under the seed-name rule): guest `cat` view 101→86 rows, 0
  xarfuse, `/test` + edenfs retained. Under the seed-name rule a non-seed
  SquashFUSE row stays visible, so "0 xarfuse" holds only while every such
  row is seed-named.
- Cell: `system-utils/procfs-sanitized-paths` canonical verify, ptrace,
  under its manifest profile: **60/60 with the harness retry, first-attempt
  60/60** at ab7f0dd15120aeecea647940feca712741ec214d (2026-10-04 09:38:46Z
  to 09:41:42Z). The 60-repetition window contained no guest-visible host
  mount churn, so it measured neither the named-seed widening nor the
  residual `/run/user` failure rate. The cell stays required in CI with pinned evidence
  (`PROCFS_MOUNTINFO_2026_09_25_*`). A mountinfo monitor running alongside
  saw one row change, an excluded per-process seed mount added at 09:41:31Z
  during repetition 56, which matched. The earlier 180 repetitions at
  d547b64d3b9f9976232aa1bb71ea7773b7f73d1a (nspid-only grammar) were 180/180
  with the retry and first-attempt 177/180: two mountinfo-read divergences
  (one coinciding with a `/run/user/0` tmpfs unmount, one unattributed) and
  one unrelated `newfstatat` size change on a host directory. Guest-visible
  host mount churn outside the excluded class, such as `/run/user/<uid>`
  (<https://github.com/rrnewton/hermit/issues/1820>), can therefore still
  fail a first attempt, so the claim is "with the harness retry", not
  "every attempt". The real fix is one shared mount snapshot
  (<https://github.com/rrnewton/hermit/issues/3627>). That profile is not
  relaxation-free: the manifest sets `rcb_time: false`, so every run carries
  `--no-rcb-time`. That setting is inherited from main and was not added by
  this change; `compare_io_buffers: true` is kept. One seed event in one
  window does not by itself show the exclusion works; the unit tests above
  carry that.

## Mounts view
`/proc/<pid>/mounts` and `/proc/<pid>/mountstats` are not covered. Both stay
raw host passthrough, as on main (Detcore has no snapshot kind for either),
so both still list seed rows and disagree with the guest mountinfo view. Giving it
the same exclusion and mountinfo's mount-point prefix rewrites is
<https://github.com/rrnewton/hermit/issues/3719>. KVM reverie-kvm proc_mounts
capture is pre-existing and unmeasured.

The procfs snapshot capture writes raw kernel bytes into the guest's buffer
before sanitizing, so excluded rows can be visible in buffer bytes past the
returned length. That predates this change and affects every snapshotted
procfs file: <https://github.com/rrnewton/hermit/issues/3718>.

A retained row whose parent is an excluded seed still snapshots: the constructor's parent pass covers it and its parent id is rewritten deterministically (unit-tested, `retained_row_with_seed_parent_still_snapshots`).

Residual: a guest program that itself drives host squashfuse seeds would
not see its own post-launch seed mounts (they were never deterministic —
host-assigned hashes/PIDs); disclosed in the PR. Churn outside the class,
such as `/run/user/<uid>` tmpfs mounts, stays visible to the guest.
