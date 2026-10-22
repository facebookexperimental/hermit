# Backend isolation audit

Source baseline: `5a3fdbb3c80a6f1673d8a51f617c660ffa2e6ca3`.
Reverie dependency: `255d2e0b8f5ba017bcc7233711c63583e4fe419e`, as pinned in
`hermit-cli/Cargo.toml:67`. Source findings below are not live pass claims.
Exact-baseline host measurements are summarized below; other required mode/root
cells remain pending or explicitly refused. The installed default CLI predates
this baseline and was not used for these results.

The known DBT PID/UTS gap is tracked at
https://github.com/rrnewton/hermit/issues/4009. The earlier DBT network repair is
https://github.com/rrnewton/hermit/issues/4002.

## Namespace policy on ordinary `run`

`P` means a newly created namespace; `H` means the caller's namespace is
inherited. `C` is conditional and is spelled out below. Each cell cites a source
key expanded to precise file and line locations in the reference table. The
network column describes the default local mode; explicit host mode inherits
the caller's network namespace. Neither an outer pinned-root container nor a
synthetic guest identity establishes a fresh Hermit namespace.

| Execution path | User | PID | UTS | Mount | Network | IPC | Cgroup |
| --- | --- | --- | --- | --- | --- | --- | --- |
| ptrace | P [C,R] | P [C] | P [C,R] | P [C,R] | P [N,R] | H [C,R] | H [C,R] |
| SaBRe | P [C,L,R] | P [C,L] | P [C,L,R] | P [C,L,R] | P [N,L,R] | H [C,L,R] | H [C,L,R] |
| LiteInst | P [C,L,R] | P [C,L] | P [C,L,R] | P [C,L,R] | P [N,L,R] | H [C,L,R] | H [C,L,R] |
| in-guest-trap | P [C,L,R] | P [C,L] | P [C,L,R] | P [C,L,R] | P [N,L,R] | H [C,L,R] | H [C,L,R] |
| KVM syscall carriers | P [C,K,R] | P [C,K] | P [C,K,R] | P [C,K,R] | P [N,K,R] | H [C,K,R] | H [C,K,R] |
| e9patch preprocessing with ptrace | P [C,L,R] | P [C,L] | P [C,L,R] | P [C,L,R] | P [N,L,R] | H [C,L,R] | H [C,L,R] |
| DBT | C [D,U] | H [D,U] | H [D,U] | P in local mode [D,U] | P [D,U] | H [D,U] | H [D,U] |

DBT enters a new root-mapped user namespace only when isolation is requested
and the caller lacks effective `CAP_SYS_ADMIN`; a privileged caller retains its
user namespace. Isolation runs on a scoped thread with new mount and optional
network namespaces. The CLI calling process is moved into that user namespace
before starting workers; it is not a child owned by the common container [D,U].
DBT host networking without binds or the marked `/test` workdir creates no
namespace. These conditions are real policy differences, beyond PID and UTS.

KVM executes `Detcore` and sends host filesystem/socket operations through
carriers in the common container [K]. Its virtual CPUs are not Linux tasks
belonging to a guest PID or UTS namespace. KVM's synthetic `/proc` allowlist
includes mount snapshots and its own process state, refuses other procfs
operations, and does not provide enumeration of all PIDs [KP]. Namespace
membership of carriers and guest namespace capabilities are separate required
observations; a carrier-only pass does not establish guest parity.

## Filesystem, identity and descriptor views

This table concerns ordinary host-root runs. `Host root` means the caller's
root filesystem remains visible; a mount namespace does not copy or make that
root read-only. Working-directory changes alone do not make its contents
private. Each field has a source key; raw live results must accompany these
source predictions before an isolation result is claimed.

| Execution path | Root; `/tmp` | `/proc`; `/sys`; `/dev` | Working directory | Hostname; loopback | Inherited descriptors |
| --- | --- | --- | --- | --- | --- |
| ptrace | Host root; fresh directory bind over `/tmp`, or explicit `--tmp` [F] | New PID procfs, read-only fallback permitted [C]; fresh sysfs in local mode [R]; caller's `/dev` [F] | Caller cwd or `--workdir`; explicit mount/bind policy [F,W] | Canonical UTS identity [C]; only new loopback, brought up [R] | No common arbitrary-fd allowlist; stdio inherited/captured, container result channel retained until completion [FD,O] |
| SaBRe | Same root/tmp recipe; private RPC socket directory and optional neutral executable staging [F,S] | Same common mounts [C,R,F] | Same CLI cwd policy [W,L] | Same common UTS/loopback [C,R,L] | Common inherited descriptors plus backend bootstrap/RPC/evidence transports; reachability must be measured [FD,S] |
| LiteInst | Same common root/tmp recipe, preload/runtime staging [F,LI] | Same common mounts [C,R,F] | Same CLI cwd policy [W,L] | Same common UTS/loopback [C,R,L] | Common inheritance plus sealed bootstrap memfd intentionally survives exec, and RPC/evidence transports [FD,LI] |
| in-guest-trap | Same recipe and runtime as LiteInst; syscall site patching disabled [F,L] | Same common mounts [C,R,F] | Same CLI cwd policy [W,L] | Same common UTS/loopback [C,R,L] | Same bootstrap/RPC policy as LiteInst [FD,LI,L] |
| KVM | Carrier host root/tmp; ordinary guest paths use carrier cwd/root [F,K] | Synthetic proc process data and captured full mount table, other procfs refused [KP]; ordinary sys/dev paths use carrier filesystem, with selected synthetic device/CPU handling [KP] | Explicit canonical cwd tracked in guest execution context [K] | Detcore synthetic uname, not guest UTS membership [K]; network operations use carrier namespace [N,K] | Guest fd table models its own files/stdio, not arbitrary carrier fd numbers; carrier and PID1 tables still matter [KF,FD] |
| e9patch with ptrace | Common root/tmp plus read-only rewritten executable bind over original pathname [F,E] | Common proc/sys/dev [C,R,F] | Common CLI policy [W] | Common UTS/loopback [C,R] | ptrace inheritance, with preprocessing guards [FD,E] |
| DBT | Host root; host `/tmp` unless binds are nonempty, then fresh tmpfs with input and statistics-preservation binds; `--tmp` is not passed [D,U] | Caller procfs/PID view; fresh sysfs only for local networking; caller `/dev` [U] | `--workdir` or caller cwd; marked `/test` can be fresh tmpfs [D,U] | Kernel UTS inherited, uname rewritten by Detcore [D,H]; new loopback brought up for local mode [U] | Standard subprocess inheritance, stdio capture/inherit variants, coordinator and evidence transports; no common fd policy [DF,D] |

The different DBT temporary-filesystem policy exposes host scratch files on its
default run. Statistics-preservation binds and e9patch's executable overlay
also add real mount rows. Runtime files and descriptors cannot be removed from
the observation merely because a backend needs them.

Default native cwd is not re-entered after `/tmp` is mounted; an inherited cwd
inode can remain beneath that overmount
(`hermit-cli/src/bin/hermit/run.rs:7822`). KVM instead
canonicalizes its requested cwd after setup (`hermit-cli/src/lib.rs:2794`). A
caller cwd hidden beneath `/tmp` is therefore a required comparison cell. The
shared design preserves a default cwd strictly below `/tmp` as a declared input;
it cannot turn that executing case into a blanket refusal.
SaBRe's neutral-name executable staging is specifically in `/dev/shm`
(`hermit-cli/src/lib.rs:2058`), and KVM opens `/dev/kvm` after setup through the
pinned [constructor](https://github.com/rrnewton/reverie/blob/255d2e0b8f5ba017bcc7233711c63583e4fe419e/reverie-kvm/src/vm.rs#L1721); a new shared device view must
account for both requirements without backend-specific visible mounts.

## Other launch configurations and modes

| Required cell | Baseline source behavior |
| --- | --- |
| `run --verify` | Every non-DBT physical run constructs its own common container and temp guard [V]. DBT runs each observation with its scoped isolation worker, including its separate namespace/tmp policy [D]. The strict comparator and two retained logs are required, unchanged. |
| `run --no-namespace` | Common path uses `Container::new` without namespaces or private mounts; network is forced host and `--tmp` resolves to host `/tmp` [NN]. DBT returns before the common warning and is not told the flag; a marked `/test` can still request helper isolation [D,NN]. e9patch refuses this combination [E]. |
| `--network host`; GDB; networking capture | Network selection is separate from the common builder. Strict host networking is refused; GDB can change local mode to host with a warning; networking capture also changes mode [N]. DBT must share this resolved policy rather than reinterpret it. |
| `--namespace-only` | Separate direct spawn builds user/PID/UTS/mount and optional net namespaces; guest itself is PID1. No Detcore runtime; IPC/cgroup still shared; explicit backend and verification combinations are refused [NO]. |
| SaBRe `strace` prototype | Runs the narrow shared Reverie strace tool through a plain subprocess, bypassing the common container entirely; all seven namespaces, root/tmp/proc/sys/dev/cwd and descriptors are inherited. This is a tracing prototype, not the full Detcore SaBRe `run` path [ST]. |
| `record start`, `record start --verify` | ptrace recorder, optionally e9patch preprocessing. Common user/PID/UTS/mount/local-net setup, identity mounts and requested mounts, but no default private `/tmp`; cwd configured on container [REC]. Non-ptrace backend record/replay engines are not supplied by this CLI and are refused [SC]. |
| Standalone autopilot replay | Common container; private network only when metadata records local networking. Replay then enters a fresh materialized chroot with its own proc mount, `/dev/fd` alias and explicitly rebased mounts [REP]. Guest report bytes may come from the recording, not the replay's live kernel view. |
| GDB replay or serve-only replay | Host network chosen for debugger connectivity, including replay of a local-network recording; warning is conditional on metadata. GDB client runs outside the PID namespace [REP]. This is a configured-isolation exception needing an explicit contract, not parity evidence. |
| Host runner | Declared host root/cwd; generated private workdir binds and input paths vary by run mode [RUNNER]. A stable outer root does not prove a private inner namespace. |
| Pinned-root runner | Podman starts a digest-pinned outer root, no network, fresh `/test`, and explicit source/build/cache binds; backend then applies its inner policy. `--privileged` and disabled cgroup management make the DBT user-namespace condition significant [PIN]. Compare within this runner configuration, separately from host-root cells. |
| CLI `run --image` | ptrace-only prototype, not the pinned-root runner. Read-only materialized root, private tmp bind, root-relative proc/dev mounts, minimal devices and private devpts [IMG]. Common local-network setup mounts `/sys` before chroot at its literal path [R], so image `/sys` needs direct qualification. All other selected backends explicitly refuse this root configuration [NN]. |
| Public library and test helpers | Low-level execution APIs do not promise CLI container setup. Test helpers can independently enter `common/test-workdir` isolation before runtime [LIB]. These paths must be labeled rather than counted as common CLI parity. |

## Measurement obligations

For every supported backend/mode/root cell retain raw namespace stat identities
against its immediate host baseline, PID1 and guest process relations, complete
mountinfo and mounts, root/tmp/proc/sys/dev/cwd sentinel observations, uname and
network interface/loopback state, and full PID1/carrier/guest fd tables. A
non-CLOEXEC inherited host directory descriptor is an explicit sentinel.
Unsupported cells and capacity refusals remain rows with their exact reason.

Detcore substitutes fixed namespace readlink identities even when real
membership differs (`detcore/src/syscalls/namespace.rs:49` and `:391`). Readlink
canonicalization is unconditional (`:406`, `:575`, `:622`), including under
`--no-virtualize-metadata`; label these link targets emulated. Readlink alone is
not physical isolation evidence. Use an independent bounded descendant
observer plus raw metadata under the labeled `--no-virtualize-metadata`
relaxation. Namespace and mount identities from independent launches are
compared by preserved structural relations, not literal kernel numbers. Keep
the raw records; do not erase PID aliases, mount rows, options, errors or fd
capabilities to manufacture equality. Strict deterministic verification remains
a separate check under Hermit's existing canonical full-observation comparator.

## Exact-baseline host observations

These are raw isolation collections, **not conformance passes or L2 results**.
All seven host-root collections exited 0. They used the baseline above and a
frozen CLI with SHA256
`48cc6a9bfe252a487ad5913184b471a49d74bbbfbc5567b0e497666492877884`.
The native launcher, contained by `safehermit`, supplied the immediate-host
namespace stat identities and an inheritable directory at fd42 with only a
known marker. CLI arguments were:

```text
--log=warn --backend=BACKEND run --strict --no-virtualize-metadata
--max-timeslice=disabled --base-env=minimal --workdir=EMPTY_WORKDIR -- PROBE
```

The launcher appended `--host-ns NAME DEV INO` for user, pid, uts, mnt, net, ipc
and cgroup, in that order; the first seven output lines retain those raw host
identities. Raw metadata and disabled preemption are declared relaxations. The
fixed empty cwd was the same filesystem object for all seven collections. Probe
binary SHA256:
`52e529f739f755cb152ef1f10f6bcc6fa23a37b6099e2969734ba75017c10daf`;
source SHA256:
`c624126c1965badfa1911319dc17a385f20c8b4b9827ee34a6197aa0e5bb5455`.

`P` below means guest raw stat device/inode differed from its host baseline;
`H` means equal. `EACCES` is a required observation failure, never evidence of
separation. Returned namespace readlink targets remain emulated; KVM refused
those target observations with `EACCES`.

| Host-root `run` observation | User | PID | UTS | Mount | Network | IPC | Cgroup | Source policy |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| ptrace | P | P | P | P | P | H | H | C,R |
| DBT | P | H | H | P | P | H | H | D,U |
| SaBRe | P | P | P | P | P | H | H | C,L,R |
| LiteInst | P | P | P | P | P | H | H | C,L,R |
| in-guest-trap | P | P | P | P | P | H | H | C,L,R |
| KVM guest report | EACCES | EACCES | EACCES | EACCES | EACCES | EACCES | EACCES | K,KP |
| e9patch preprocessing with ptrace | P | P | P | P | P | H | H | C,L,R |

The raw native reports have private procfs with PID1 named `hermit` for ptrace,
SaBRe, LiteInst, in-guest-trap and e9patch with ptrace. DBT sees the caller's
procfs, with PID1 named `systemd` in this host cell. KVM refuses PID1, proc-root
stat, kernel-hostname and proc network files with `EACCES`; its sysfs interface
listing still reports only `lo`, flags `0x9` (up), as do the other six paths.
All seven report Detcore `getpid() == 3`; that value is emulated, not evidence of
physical PID membership. Native kernel hostname is `hermetic-container.local`
on common paths, while DBT retains the caller hostname despite emulated
`gethostname`. The actual caller name is deliberately omitted here.

| Host-root view | `/tmp` | Root and cwd | `/sys`; `/dev` | Complete mount rows | Source policy |
| --- | --- | --- | --- | --- | --- |
| ptrace | Fresh, mode 0755, empty | Caller root; selected empty cwd | Fresh writable sysfs; inherited devtmpfs/devpts/shm | 146 | F,C,R,W |
| DBT | Caller tmp, mode 1777, 69,820 entries at collection | Caller root; selected empty cwd | Fresh writable sysfs; inherited devtmpfs/devpts/shm | 142 | D,U,W |
| SaBRe | Fresh, mode 0755, one runtime entry | Caller root; selected empty cwd | Fresh writable sysfs; inherited devtmpfs/devpts/shm | 146 | F,S,R,W |
| LiteInst | Fresh, mode 0755, one runtime entry | Caller root; selected empty cwd | Fresh writable sysfs; inherited devtmpfs/devpts/shm | 146 | F,LI,R,W |
| in-guest-trap | Fresh, mode 0755, one runtime entry | Caller root; selected empty cwd | Fresh writable sysfs; inherited devtmpfs/devpts/shm | 146 | F,L,R,W |
| KVM | Fresh, mode 0755, empty | Carrier root/cwd; proc-root alias refused | Fresh writable sysfs; inherited devtmpfs/devpts/shm | 146 in mount snapshot | F,K,KP,R |
| e9patch with ptrace | Fresh, mode 0755, empty | Caller root; selected empty cwd | Fresh writable sysfs; inherited devtmpfs/devpts/shm | 146; no rewrite artifact for this probe | F,E,C,R,W |

Full rows, covered inherited mounts, propagation/master relationships and
backend runtime entries are retained. Row counts are collection descriptors,
not equality assertions. Source names and mutable caller tmp contents are not
reproduced in this public reference.
This e9patch-selected probe reported `candidate_sites=0`, `mapped_sites=0` and
`artifact_sha256=none`; it executed with ptrace but added no executable overlay.
An actual successful rewrite and its complete mount row remain required pending
observations, rather than being inferred from the selected CLI spelling.

| Descriptor observation | Guest fd42/direct marker | Self procfd marker | PID1 procfd marker | Other observed runtime handles | Source policy |
| --- | --- | --- | --- | --- | --- |
| ptrace | Absent from guest listing; direct access refused | Unreachable | **Reachable** | Stdio and scanner | FD,O |
| DBT | Listed; direct access virtual `EBADF` | **Reachable** | Caller PID1 view, not a private init | fd3 launcher file; fd4 socket; fd197 PID memfd; fd198 pipe; fd199–201 aliased pipes | DF,D,U |
| SaBRe | Listed; direct access **reachable** | **Reachable** | **Reachable** | fd3 launcher file; fd101 stats memfd; fd4 listed but virtual `EBADF` | FD,S |
| LiteInst | Listed; direct access virtual `EBADF` | **Reachable** | **Reachable** | fd3 launcher file; fd4 socket; fd5 perf event | FD,LI |
| in-guest-trap | Listed; direct access virtual `EBADF` | **Reachable** | **Reachable** | Same observed types as LiteInst | FD,LI,L |
| KVM | Enumeration `EACCES` | `ELOOP` | `ELOOP` | Required fd-view failure; no leak-proof claim | KF,KP,FD |
| e9patch with ptrace | Absent from guest listing; direct access refused | Unreachable | **Reachable** | Stdio and scanner | FD,E,O |

Virtual `EBADF` does not prove the physical fd was closed: procfd traversal
exposed the known marker in several such cells. PID1 retains it even when the
ptrace guest does not. This is a real isolation defect. The first independent
observer lacked complete thread/sibling traversal; guest raw stat and sentinel
results are retained as evidence, while independent physical corroboration
remains pending. No observer limitation changes the required assertions.

Raw outputs, stderr, exit status, resource reports and physical observations are
retained under `ignored/backend-isolation-measurement/host-BACKEND-run-default-v2.*`.
The original `default-matrix-v2.sh` producer bytes were overwritten during
observer iteration and are unavailable; its current contents are not the exact
driver of these retained collections. The retained launcher-v2 binary SHA256 is
`586f00d6d0906c9dd29ab6b66db81b434ad14a5729e3460f48f840c1a6cc8437`.
The DBT safehermit receipt binds invocation digest
`ef5c55238b7547084e7d6a9c01d1fe2f564f70ce655d15c363b4731586f42b05`.
The raw guest observations remain evidence; exact-driver reproduction awaits
the frozen replacement collection.
The corresponding stdout SHA256 values bind these collections:

| BACKEND | stdout SHA256 |
| --- | --- |
| ptrace | `682dc83ea71712c4e114d203d117a015604c3d7983dbd0e773c5f88fed07d0d5` |
| dbt | `846e16e4bbd8661d72a644f7d2280b5443b1f64878078fa50f11fe8f242064c7` |
| sabre | `d6fec1cd56a3113474c786d05affcab31f33cef69619a94671ed6bd086e78647` |
| liteinst | `590d18fd6c0eb56e2919ed1d03188aaf094baaacc06bec3c77bf94cfce0b6b7c` |
| in-guest-trap | `45ea43e94db40a2236083ed608ec8cdf6a8e47bb6a85bf6731cbdd8956b733d0` |
| kvm | `0043d71e62cd8e65c1efc8b5fcc6da1219508e89ab824f3e875cb21152c36025` |
| e9patch | `14774e4e4efcf07c47f70d58a2a420835b289abcdeb9aa17b6f9997384e2e26b` |

All seven pinned-root attempts were retained as loader refusals, exit 127,
missing `libunwind-x86_64.so.8`; these are pending executions, not unavailable
backend skips. Other required mode/option cells remain pending.

## Native mount feasibility

Both native setup recipes created the requested namespaces and real PID1. Local
mode mounted fresh sysfs with `MS_RDONLY|MS_NOSUID|MS_NODEV|MS_NOEXEC` at creation;
host-network mode used `MS_BIND|MS_REC`, then recursive `mount_setattr` setting
read-only/nosuid/nodev/noexec while clearing no attributes. Both overlaid
`/sys/fs/cgroup` with empty read-only tmpfs (`mode=0555,size=4096`), whose test
creation returned `EROFS`. Local mode had private network/loopback; explicit host
mode shared only network among the seven namespaces. Full mount topology keeps
the masked underlying controller row.

The initial local recipe failed a later plain read-only remount with `EPERM`;
the initial host recipe failed a non-recursive bind with `EINVAL`. Both failures
remain evidence and motivated the explicit successful recipes; neither became
a fallback. These probes establish setup feasibility, not backend conformance.
Native source SHA256:
`96cbcd32675d7a71f864ee5cf99543474e4f47f87e2988c6fa2e16384f75e56c`.

## Source references

Paths without a URL refer to the baseline Hermit tree. External links bind the
dependency revision above; a newer dependency checkout is not source evidence.

| Key | Exact sources |
| --- | --- |
| C | `hermit-cli/src/bin/hermit/container.rs:443`; `:446`; `:448`; `:450`. |
| L | `hermit-cli/src/bin/hermit/run.rs:5519`; `hermit-cli/src/lib.rs:3773`; `:3814`; `:3832`; `run.rs:5565`. |
| N | `hermit-cli/src/bin/hermit/run.rs:1481`; `:1497`; `:5552`; `:5963`; `:6000`; `:6016`. |
| R | Pinned Reverie [namespace flags](https://github.com/rrnewton/reverie/blob/255d2e0b8f5ba017bcc7233711c63583e4fe419e/reverie-process/src/namespace.rs#L21); [implicit user namespace](https://github.com/rrnewton/reverie/blob/255d2e0b8f5ba017bcc7233711c63583e4fe419e/reverie-process/src/container.rs#L475); [UTS](https://github.com/rrnewton/reverie/blob/255d2e0b8f5ba017bcc7233711c63583e4fe419e/reverie-process/src/container.rs#L526); [mount and network setup](https://github.com/rrnewton/reverie/blob/255d2e0b8f5ba017bcc7233711c63583e4fe419e/reverie-process/src/container.rs#L565); [mount/chroot/loopback order](https://github.com/rrnewton/reverie/blob/255d2e0b8f5ba017bcc7233711c63583e4fe419e/reverie-process/src/container.rs#L715). |
| F | `hermit-cli/src/bin/hermit/run.rs:6911`; `:7597`; `:7635`; `:7671`; `:7680`. |
| W | `hermit-cli/src/bin/hermit/run.rs:7807`; `:7822`; `:7824`; `:7836`. |
| D | `hermit-cli/src/bin/hermit/run.rs:5528`; `:5550`; `hermit-cli/src/bin/hermit/backends.rs:1013`; `:1028`; `:1153`; `:1483`; `:1508`; `:1526`; `:1538`; `:1565`. |
| U | `common/test-workdir/src/lib.rs:49`; `:64`; `:115`; `:160`; `:342`; `:346`; `:351`; `:372`; `:380`; `:383`; `:385`; `:399`. |
| H | `hermit-cli/src/bin/hermit/run.rs:5753`; `detcore/src/syscalls/namespace.rs:49`; `:391`. |
| S | `hermit-cli/src/lib.rs:2090`; `:2255`; `:2285`; `:2299`; `:2305`. |
| LI | `hermit-cli/src/lib.rs:3821`; `:3832`; pinned Reverie [sealed bootstrap descriptor](https://github.com/rrnewton/reverie/blob/255d2e0b8f5ba017bcc7233711c63583e4fe419e/reverie-liteinst/src/backend.rs#L203); [exec inheritance](https://github.com/rrnewton/reverie/blob/255d2e0b8f5ba017bcc7233711c63583e4fe419e/reverie-liteinst/src/backend.rs#L970). |
| K | `hermit-cli/src/lib.rs:2771`; `:2794`; `:2857`; `:2862`; `:2867`; `:2872`; `:2879`; `:2906`. |
| KP | Pinned Reverie [synthetic proc policy and allowlist](https://github.com/rrnewton/reverie/blob/255d2e0b8f5ba017bcc7233711c63583e4fe419e/reverie-kvm/src/executor.rs#L17164); [mount/process contents](https://github.com/rrnewton/reverie/blob/255d2e0b8f5ba017bcc7233711c63583e4fe419e/reverie-kvm/src/executor.rs#L17333); [ordinary filesystem/device dispatch](https://github.com/rrnewton/reverie/blob/255d2e0b8f5ba017bcc7233711c63583e4fe419e/reverie-kvm/src/executor.rs#L10457); [proc refusal](https://github.com/rrnewton/reverie/blob/255d2e0b8f5ba017bcc7233711c63583e4fe419e/reverie-kvm/src/executor.rs#L11019); [raw namespace mount capture](https://github.com/rrnewton/reverie/blob/255d2e0b8f5ba017bcc7233711c63583e4fe419e/reverie-kvm/src/proc_mounts.rs#L20). |
| KF | Pinned Reverie [guest fd table and inherited stdio](https://github.com/rrnewton/reverie/blob/255d2e0b8f5ba017bcc7233711c63583e4fe419e/reverie-kvm/src/executor.rs#L2710); [modeled output descriptor refusal](https://github.com/rrnewton/reverie/blob/255d2e0b8f5ba017bcc7233711c63583e4fe419e/reverie-kvm/src/executor.rs#L9937). |
| FD | Pinned Reverie [stdio setup](https://github.com/rrnewton/reverie/blob/255d2e0b8f5ba017bcc7233711c63583e4fe419e/reverie-process/src/container.rs#L655); [owned result channel](https://github.com/rrnewton/reverie/blob/255d2e0b8f5ba017bcc7233711c63583e4fe419e/reverie-process/src/container.rs#L1372); [spawn stdio](https://github.com/rrnewton/reverie/blob/255d2e0b8f5ba017bcc7233711c63583e4fe419e/reverie-process/src/spawn.rs#L109); `hermit-cli/src/bin/hermit/main.rs:62`. None is a common filter for unrelated inherited descriptors. |
| DF | Pinned Reverie [DBT stdio and child launch](https://github.com/rrnewton/reverie/blob/255d2e0b8f5ba017bcc7233711c63583e4fe419e/reverie-dbt/src/launcher.rs#L645); `hermit-cli/src/bin/hermit/backends.rs:1611`; `:1646`; `:1670`. |
| O | `hermit-cli/src/bin/hermit/owned_container.rs:240`; `:265`; `:298`; `:301`; `hermit-cli/src/bin/hermit/container.rs:623`; `:643`. |
| E | `hermit-cli/src/bin/hermit/run.rs:5416`; `:7652`; `:7811`; `hermit-cli/src/bin/hermit/record_start.rs:472`. |
| V | `hermit-cli/src/bin/hermit/run.rs:7707`; `:7743`; `:8289`; `hermit-cli/src/bin/hermit/backends.rs:1185`. |
| NN | `hermit-cli/src/bin/hermit/run.rs:1081`; `:5572`; `:5714`; `:5720`; `:5767`; `:6953`. |
| NO | `hermit-cli/src/bin/hermit/run.rs:5422`; `:7013`; `:7057`; `:7065`. |
| REC | `hermit-cli/src/bin/hermit/record_start.rs:428`; `:449`; `:460`; `:527`; `:647`; `:650`. |
| REP | `hermit-cli/src/bin/hermit/replay.rs:69`; `:79`; `:101`; `:134`; `:196`; `hermit-cli/src/replay.rs:100`; `:143`; `:146`; `:202`; `:253`; `:271`. |
| SC | `hermit-cli/src/bin/hermit/main.rs:400`; `:410`; `:426`; `:435`; `:445`; `:454`. |
| ST | `hermit-cli/src/bin/hermit/strace.rs:36`; `:45`; `:64`; `hermit-cli/src/bin/hermit/backends.rs:1875`; `:1887`; `:1892`. |
| RUNNER | `ci/manifest-plan/src/runner.rs:1282`; `:1315`; `:4606`; `:5084`. |
| PIN | `ci/hermetic/run-in-pinned-root.sh:456`; `:458`; `:459`; `:462`; `:464`; `:465`; `:479`; `ci/hermetic/README.md:124`. |
| IMG | `hermit-cli/src/bin/hermit/container.rs:475`; `:500`; `:507`; `:537`; `:551`; `:559`; `hermit-cli/src/bin/hermit/run.rs:7685`. |
| LIB | `hermit-cli/src/lib.rs:4315`; `:3304`; `:3773`; `detcore/tests/testutils/src/lib.rs:557`. |
