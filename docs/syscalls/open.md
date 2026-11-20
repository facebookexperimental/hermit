---
schema: syscall-pages/v3
syscall: open
nr_x86_64: 2
related:
  - https://github.com/facebookexperimental/hermit/blob/main/docs/syscalls/openat.md
  - https://github.com/facebookexperimental/hermit/blob/main/docs/syscalls/creat.md
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+open&body=Discussion+for+the+%60open%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fopen.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60open%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 403e350e9e82ee540529f097daccae6a60832d07
  detcore_class: determinized
  dispatch: "typed"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
# prose: human/agent-authored, machine-checked (syscall-pages.rs check); v3 only
prose:
  status: checked
  reviewed_at: 2026-11-20
  based_on: 403e350e9e82ee540529f097daccae6a60832d07
---

## In short

Natively, `open` asks the host kernel to resolve a path on the host filesystem, so whether the call succeeds, which descriptor it returns, and what that descriptor reaches all follow host files, host permissions, and the order in which threads happen to run. Outside a DBT copied child, which runs it natively, Hermit keeps that kernel lookup for ordinary files but runs it inside its serialized schedule, answers the files it renders itself from its own model, and makes a blocking open of a guest-made FIFO wait in guest turns where it can see the guest descriptors. The sibling calls `openat` and `creat` share the same handler and keep their own pages; they are cross-linked here and are not checked on this page. Known gaps: 15.

## Determinism strategy

**What is nondeterministic natively.** `open` resolves a pathname against the caller's working directory, validates the flags, and either reaches an existing file or creates one, returning the lowest free descriptor or an errno.[^linux] Every part of that answer can move with the host: a file can appear, vanish, or change owner between two runs, a create can win or lose a race with another creator, and a blocking open of a FIFO waits until another opener arrives, so thread timing decides when and whether it returns.[^linux]

**What Hermit does.** Outside a DBT copied child, Detcore determinizes `open` by handling it with the shared typed handler for the open family: classification places the legacy entry with the determinized calls, and typed dispatch converts it to the same `openat` form its siblings use before any file work begins.[^c1][^c2] A copied child on that backend runs without a Detcore tool and runs this call natively, with none of the handling below (G8).[^c17] The handler reads the path from guest memory, failing with `EFAULT` when there is no path to read, resolves relative spellings against the guest's own root and working directory, and takes a read grant on the path resource before the open, so path resolution runs one guest turn at a time in schedule order; the working-directory and link injections and the procfs task queries that prepare the spelling run before that grant.[^c3][^c4] Files Detcore itself renders are answered from its model with no kernel open of the named file: the guest gets a placeholder descriptor, made from a sealed anonymous file (left unsealed on KVM, where seals are refused) and reopened through its descriptor link with the guest's flags, whose contents Detcore serves. The descriptor directories it serves take a different placeholder: an `O_RDONLY|O_DIRECTORY` (or `O_PATH`) open of the kernel's `/proc` on ptrace, DBT, SaBRe and LiteInst, so the kernel does open that named directory there, or of hermit-cli's placeholder directory on KVM. Every other name is handed to the kernel through the record-or-replay forwarding path, so the kernel still decides existence, permission, and creation for ordinary host files.[^c5][^c9][^c16][^c24][^c33] Which files count as rendered depends on the backend: a descriptor's info file is rendered only where the guest's procfs is not the kernel's, and the other rendered and directory answers likewise follow what the backend lets the tool name.[^c18] After a kernel open succeeds the handler records the descriptor with its flags and kind, taking the stat it needs to classify what was reached while metadata is virtualized, and a random device is known by its device number alone, the character devices numbered `1:8` and `1:9` by whatever name reached them, so later reads draw on Detcore's stream rather than host entropy; the stream name counts only the guest's own opens, so it does not move with what a backend's loader opens.[^c12][^c13][^c25] Detcore also answers without a fresh kernel answer for the name as spelled in the following five cases: a hidden placeholder and a procfs name outside the run's level are hidden with `ENOENT`, a sysfs object the presented machine does not have gets its absent answer, a read-only respelling answers a timer-slack witness the host mount refused, and a classification stat that fails closes the descriptor the kernel just opened and fails the open with that error.[^c26]

**Interface fidelity.** For ordinary files the flags and the creation mode reach the kernel unchanged, the mode under the kernel's own mask, so the access modes and the create, exclusive, truncate, append, directory, no-follow, close-on-exec, nonblocking, path-only, synchronous, direct, no-access-time, no-controlling-terminal, large-file, signal-driven, and temporary-file forms are honored exactly as the kernel honors them, including the kernel's own refusals for the directory-with-create and temporary-file combinations and the temporary-file success path.[^c9][^linux] Detcore mirrors that same flag refusal on the files it renders itself, answering `EINVAL` for the combinations the kernel refuses before any lookup.[^c14] The order of the checks differs from the kernel's, stated as G12: the path is read before the flags are judged and before a descriptor is taken, so a missing path answers `EFAULT` where the kernel answers its flag or table-full error first.[^c3][^linux] Beyond the forwarded answer, Detcore answers or refuses in its own right, each stated in the gaps: a backend-owned procfs directory the guest must not reach is refused with `EACCES` before injection, a writable open of a rendered file is answered from the model or refused with no injected call, an undetermined alias that may name a rendered name file is refused by name, an exe link the kernel would name as the launcher is refused by name, a live property file outside its frozen copy is refused by name, and in a process that handles the alarm signal an open that may wait is refused, all as the gaps state.[^c6][^c5][^c7][^c19][^c20][^c21] In a plain run, on a backend whose tool can name guest descriptors (so not KVM), a scheduler-managed pipe reopened by path gets the managed kind and an internal physical nonblocking flag the guest cannot see, so its waits stay in guest turns; under record or replay, and on KVM, that treatment does not apply (G15).[^c27][^c32] A blocking open of a guest-made FIFO for reading or writing, without the nonblocking or path-only flags and not read-write, does not block in the kernel where the poll applies: the handler injects a nonblocking try and waits in polling turns until a guest would open the other end, restoring the guest's blocking flags once the open completes.[^c8][^c10] A signal that ends that wait surfaces as the interrupted-open result the scheduler models; where the poll does not apply the open waits in the kernel holding its turn, which deadlocks when a guest would open the other end (G2, G3).[^c10]

**Record & replay.** The call is subscribed in both run and record/replay modes (code facts in the Appendix).[^c2] In a plain run the forwarding path injects the kernel open directly; under record the kernel answer is recorded, and under replay the recorded descriptor result and errno are returned without reaching the host filesystem again.[^c9][^c16] Replay therefore repeats the recorded existence and permission answers even if the host files have since changed, while the rendered-file answers never fire in that mode because Detcore renders no procfs file there.[^c15] The guest-made FIFO poll is also off under record and replay: under replay the open follows the recording, and while recording a blocking FIFO open with a guest at the other end waits in the kernel holding its turn, as in G3.[^c8]

## Known gaps

- G1 (by design): an ordinary file is still the host's file: existence, ownership, permission, contents, and the outcome of a racing create are whatever the host filesystem does when the forwarded open runs, so a filesystem that changes under the run, or differs between hosts, changes the result, and Hermit makes this call repeatable only so far as the filesystem itself repeats.[^c9]
- G2 (unimplemented): the polling wait for a guest-made FIFO runs only on the ptrace backend and on in-guest LiteInst, where the tool can see guest descriptors through host tasks; on KVM, DBT, and SaBRe, and wherever a shared descriptor table or a guest whose threads are not host tasks prevents that view, the blocking open waits in the kernel holding its turn, and another guest thread that would open the other end may never run.[^c8][^c11]
- G3 (unimplemented): a FIFO no guest made is opened with no polling and waits in the kernel holding its turn: where the other end belongs to a host process the host still makes progress on its own, but where a guest would open the other end that guest never runs and the run deadlocks, as with a FIFO made before the run that guest threads then open from both ends; while recording the poll is likewise off, so the same wait holds the turn there too.[^c8][^c28]
- G4 (unimplemented): in a process that handles the alarm signal, an open that may wait is refused with `EOPNOTSUPP` after its grant, where Linux would wait or complete: a FIFO without the nonblocking flag, and every character or block device with or without that flag, including the null, zero, and random devices and terminals, and any open whose lookup fails for a reason the open would not share, such as a permission-denied lookup; the only fallback inside the call is the path-only form, or not handling that signal, since the nonblocking form is refused for devices as well.[^c7][^c23]
- G5 (unimplemented): on KVM only, a name that reaches a backend-owned procfs directory is refused with `EACCES` before injection where Linux would open the directory the name reaches, only when this caller gets no placeholder, as with no namespace, a library caller, a user mount over the placeholder, or a caller in a nested pid namespace, and never under record or replay or on DBT or SaBRe; the model-served directories on the other paths are the only fallback, and a guest that must list the backend directory cannot.[^c6][^c29]
- G6 (by design): under record and replay the answer is the recording's, not a fresh lookup: replay returns the recorded success or errno even when the host file has been created, removed, or re-permissioned since, and neither the rendered-file answer nor the FIFO poll of a plain run applies in that mode.[^c15][^c8]
- G7 (unmeasured): which pooled device number a reached filesystem reports in later metadata answers follows the order in which distinct devices are first observed in the run, prefilled in snapshot row order where a snapshot is read; that order can differ with what each backend observes before the guest's own opens, and no measurement here pins the resulting guest-visible numbers across backends. A random device itself is known by its device number alone, as stated above, and takes no part in this ordering.[^c22]
- G8 (unimplemented): in a DBT copied child, which runs without a Detcore tool in the window between the vfork and the exec of the child's next program, this call and its sibling forms run natively in the kernel with no serialized lookup, no rendered files, no FIFO poll, no alarm-signal refusal, and no random-device handling, as the shell redirections and spawn file actions that run there do; a guest whose opens in that window must be handled must avoid that child path, and there is no fallback inside the call.[^c17][^c1]
- G9 (unimplemented): a writable open of a spelling with a parent climb that Detcore cannot resolve, which may name the rendered name file of a task, is refused by name with `EOPNOTSUPP`, and stops a fail-closed run, only where the name's source is Detcore's record, which is KVM, and never under record or replay, where Linux would open the file the spelling reaches; a guest in that case must open through a spelling Detcore resolves, and there is no fallback for the unresolved spelling.[^c19]
- G10 (unimplemented): an open of an exe link whose kernel answer would name the launcher, where Detcore holds no record of the image or the image is no longer at its path, is refused by name with `EOPNOTSUPP`, and stops a fail-closed run, only where a launcher is declared for the run, which is DBT and SaBRe; the link itself is otherwise served from Detcore's record. A guest in that case has no fallback inside the call and must open the image by its path.[^c20]
- G11 (unimplemented): with a frozen copy of the property area, pure reads are redirected to the copy; a readable open that reaches the live area, or that cannot be shown not to have reached it, including a name the copy does not hold that was made after it was taken, is refused by name with `EOPNOTSUPP`, before the open runs where the copy holds the object and after the live descriptor is closed where it cannot be shown, where Linux would read the live file; a fail-closed run (the default) stops, and `EOPNOTSUPP` is given only with `--allow-unsupported-syscalls`; a path-only open reads nothing and stays usable, and a guest in that case must read from the frozen copy, with no fallback to the live file inside the call.[^c21][^c34]
- G12 (unimplemented): the checks run in the other order from the kernel's: the path is read first, so a call with no readable path answers `EFAULT` where the kernel, which copies the name first but reports a copy failure only after judging the flags and taking a descriptor, answers `EINVAL` for a refused flag combination or `EMFILE` when no descriptor is free, and the alarm and directory refusals above likewise come before the kernel's flag answer.[^c3][^linux]
- G13 (unimplemented): a spelling with a parent climb that names a file Detcore renders, other than the `comm`, `/proc/locks` and fdinfo cases Detcore walks itself, reaches the kernel's own file instead of the rendered one, since Detcore's names drop the climb the kernel walks; a guest in that case must open through a direct spelling, and there is no fallback for the climbed spelling.[^c30]
- G14 (unimplemented): on KVM in a plain run, a writable open of a rendered `comm` file, by its path or a descriptor link (`O_RDWR`, `O_WRONLY`, or `O_RDONLY|O_TRUNC`), is refused with `EACCES` and nothing injected, where Linux opens the file: both `comm` entries are owner-writable and writing one renames any thread in the caller's own thread group, so a library call that opens another thread's `comm` with `O_RDWR` to rename it, as `pthread_setname_np` does, fails; the fallback is `prctl(PR_SET_NAME)` for the caller's own thread, and there is none for another thread's name.[^c31][^c5]
- G15 (unimplemented): a scheduler-managed pipe reopened by path keeps a physically blocking regular descriptor outside the scoped case above, under record or replay and on KVM where the tool cannot name guest descriptors, so a read that waits for a writer blocks in the kernel while holding the scheduler turn and the writer never runs, deadlocking the run; a guest in those cases has no fallback inside the call.[^c27][^c32]

## Discussion

[Start a discussion about `open`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+open&body=Discussion+for+the+%60open%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fopen.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60open%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Appendix

### Code facts

Detcore classifies `open` (2) as determinized via typed dispatch; deterministic refusal: false; run subscription: true; record/replay subscription: true.

- Reviewed list: `detcore/src/syscall_classification.rs:1810`
- Dispatch: `detcore/src/lib.rs:8052`

### Tests

That a writable open of a rendered alias is refused with nothing injected (G14), and that an undetermined alias is refused by name while a climbed spelling of another file is forwarded unchanged: `detcore/src/tool_global/tests/comm_alias_open_tests.rs` (`a_writable_open_of_a_valid_comm_alias_is_refused_with_nothing_injected`, `a_writable_open_of_an_undetermined_comm_alias_is_refused_by_name`, `a_dotdot_spelling_of_another_file_is_forwarded_unchanged`) — what Hermit does, and interface fidelity; the climbed spelling there, `/proc/self/task/../stat`, names `/proc/self/stat`, which Detcore renders by every spelling, so that test pins the limitation stated as G13 rather than an ordinary-file case. That guest descriptors are visible only under a host-task tracer and a FIFO is seen only there: `detcore/src/fifo_peer.rs` (`guest_descriptors_are_visible_only_under_a_host_task_tracer`, `a_fifo_is_seen_only_where_guest_descriptors_are_visible`) — what Hermit does. Guest programs fifo_peer, openat_flags, and o_tmpfile_anon under tests/c exercise blocking FIFO opens in both orders, flag semantics, and temporary-file opens end to end — what Hermit does, and interface fidelity. Guest programs are evidence as text; no machine-verified coverage is claimed for them.

### Citations

[^linux]: Linux `open(2)`:
  syscall entry: https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/fs/open.c?h=v7.1.3#n1374
  flags validated before lookup: https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/fs/open.c?h=v7.1.3#n1179
  the open itself: https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/fs/open.c?h=v7.1.3#n1355
  pathname walk: https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/fs/namei.c?h=v7.1.3#n4877
  walk steps: https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/fs/namei.c?h=v7.1.3#n4838
[^c1]: detcore/src/syscall_classification.rs:766 sha256:11064ced3a8269e6
[^c2]: detcore/src/lib.rs:8050-8054 sha256:498331a9fc74a519
  https://github.com/facebookexperimental/hermit/blob/main/detcore/src/lib.rs#L8050-L8054
[^c3]: detcore/src/syscalls/files.rs:6590-6605 sha256:2b57e43be131c6ef
[^c4]: detcore/src/syscalls/files.rs:6634-6637 sha256:cab077df08e60079
[^c5]: detcore/src/syscalls/files.rs:6663-6674 sha256:026daa72fe5689d1
[^c6]: detcore/src/syscalls/files.rs:6725-6740 sha256:3677f776c71a0b27
[^c7]: detcore/src/syscalls/files.rs:6755-6773 sha256:2f40fd47228be4b5
[^c8]: detcore/src/syscalls/files.rs:6793-6818 sha256:afc904b4402e8ee6
[^c9]: detcore/src/syscalls/files.rs:6864-6875 sha256:f59a46ef6b11b91a
[^c10]: detcore/src/syscalls/files.rs:4059-4060 sha256:8c53a8b5c1b5964d; detcore/src/syscalls/files.rs:4082-4082 sha256:a7fb59abc06b6f4d; detcore/src/syscalls/files.rs:4098-4120 sha256:d444ee72c137bedd; detcore/src/syscalls/files.rs:4152-4164 sha256:64e3d093de18590a; detcore/src/syscalls/files.rs:4209-4209 sha256:adf66458a0e41720
[^c11]: detcore/src/fifo_peer.rs:400-406 sha256:7a5204d91971aeb3
[^c12]: detcore/src/syscalls/files.rs:4993-5005 sha256:2b9c166bd3032c89
[^c13]: detcore/src/syscalls/files.rs:8992-9002 sha256:17eab00afa7ca923
[^c14]: detcore/src/syscalls/files/rendered_procfs.rs:2239-2252 sha256:6494edddf02ed53e
[^c15]: detcore/src/syscalls/files/rendered_procfs.rs:2575-2581 sha256:8e38dc8b4bf7b529
[^c16]: detcore/src/tool_local.rs:890-933 sha256:461000d6aa72b4fc
[^c17]: detcore/src/lib.rs:664-675 sha256:8983b700e3b58e5c
[^c18]: detcore/src/syscalls/files/rendered_procfs.rs:2757-2762 sha256:1be4bd69f84d6914
[^c19]: detcore/src/syscalls/files/rendered_procfs.rs:4975-5001 sha256:f67e88a3c25bb149; detcore/src/syscalls/files/rendered_procfs.rs:3339-3358 sha256:6b92f5fb51f9bb65
[^c20]: detcore/src/syscalls/files.rs:6883-6891 sha256:3a11482a98d9e4e8; detcore/src/syscalls/files.rs:6741-6753 sha256:e61890d7d397c6ee; detcore/src/syscalls/namespace.rs:1436-1453 sha256:e49bc7f6d9b8d967
[^c21]: detcore/src/syscalls/files.rs:6832-6857 sha256:1fa4343e6393a4b6; detcore/src/syscalls/files.rs:6977-7029 sha256:b40968805d6e0d1e
[^c22]: detcore/src/tool_global.rs:912-941 sha256:493a58227a8607ab
[^c23]: detcore/src/sigalrm_phase1.rs:421-433 sha256:f61a3fab0036c1d8
[^c24]: detcore/src/syscalls/files/rendered_procfs.rs:4481-4545 sha256:1795fc5fb29c9ce2
[^c25]: detcore/src/fd.rs:80-104 sha256:617e5374f7807ca6
[^c26]: detcore/src/syscalls/files.rs:7093-7109 sha256:6fb253fb6e7c25f6; detcore/src/syscalls/files.rs:7133-7168 sha256:13ac783d72237bf7; detcore/src/syscalls/files.rs:6867-6873 sha256:821568e39f5dc13e; detcore/src/syscalls/files.rs:5003-5020 sha256:34626b294485efaf
[^c27]: detcore/src/syscalls/files.rs:6943-6973 sha256:07ed3bcd0f933b18
[^c28]: detcore/src/fifo_peer.rs:13-16 sha256:20b87c7db2507ed4; detcore/src/syscalls/files.rs:4041-4046 sha256:96df80939f315aa0
[^c29]: detcore/src/syscalls/files/descriptor_directory.rs:2016-2047 sha256:4788c472b7ad61ae; detcore/src/syscalls/files/descriptor_directory.rs:1474-1480 sha256:6fcccbf3661b3a39
[^c30]: detcore/src/syscalls/files/rendered_procfs.rs:2714-2725 sha256:f2b7f28e6feafd57
[^c31]: detcore/src/syscalls/files/rendered_procfs.rs:3387-3393 sha256:401b55cbb13d46a6
[^c32]: detcore/src/syscalls/files.rs:1428-1432 sha256:0e921e5d2e620837
[^c33]: detcore/src/syscalls/files/descriptor_directory.rs:185-190 sha256:a76f0c48593801c7; detcore/src/syscalls/files/descriptor_directory.rs:2180-2207 sha256:5b5fca705f1080de
[^c34]: detcore/src/syscalls/files.rs:6519-6527 sha256:a44a4b45240ff819; detcore/src/syscalls/files.rs:5986-5992 sha256:c8b440f84e868e15

## Status

- 2026-11-15 · rev 54bd03547df0 · facts-only page generated from Detcore code facts at this revision.
- 2026-11-16 · rev 7ac736d54e14 · facts refreshed from Detcore at this revision.
- 2026-11-18 · rev 403e350e9e82 · facts refreshed from Detcore at this revision.
- 2026-11-20 · rev 403e350e9e82 · checked determinism-strategy prose added (rubric v3); citations verified at this revision.
