---
schema: syscall-pages/v3
syscall: statx
nr_x86_64: 332
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+statx&body=Discussion+for+the+%60statx%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fstatx.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60statx%60%2C+or+in+what+the+page+says%3F%0A
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

Natively, this call copies a file's identity, device, size, link count and timestamps straight from the host file system, so the same program sees different metadata on different hosts and after unrelated host activity. Hermit still lets the kernel look the file up and decide every error, then rewrites the result before the guest sees it: the inode and device become run-local numbers, directory sizes and link counts are pinned, and times come from the run's epoch or its virtual clock rather than the host clock. Fields past the mount identity that Hermit does not model never reach the guest. Known gaps: 11.

## Determinism strategy

**What is nondeterministic natively.** The call asks the file system for a file's basic attributes and, for the fields the caller's mask requests, such extras as the birth time, the mount identity and direct-I/O alignments, and the kernel copies out whatever the host inode holds: host-assigned inode and device numbers, a directory size and link count that change as other processes create entries, timestamps written by host clocks, and mount numbers assigned by the host kernel.[^linux] Every one of those values is guest-visible, so a guest that prints, hashes or branches on metadata follows host state rather than its own schedule.[^linux]

**What Hermit does.** Detcore determinizes the call, when metadata virtualization is on, by intercepting it on the way through: typed dispatch reaches a handler that first checks the call's own argument shape, answers a file Detcore itself renders without asking the kernel, and otherwise injects the guest's call into the kernel — its path respelled where `/proc` numbers are translated[^c22] — so the kernel performs the lookup and decides every error.[^c1][^c2][^c3] The kernel's answer is then read back, converted into Detcore's own stat form, rewritten, and written to the guest's buffer in its place.[^c5][^c10] The rewrite reveals only a deterministic inode, numbered from the run's pool for the host file identity, and only a deterministic device, translated the same way, so the host's numbers never reach the guest.[^c6] A directory that is not a proved procfs directory reports one fixed block as its size with matching blocks — where a backend cannot prove the directory is not on procfs that size pin stands aside and the host size shows instead — because a host directory's size sums whatever entries anyone has created in it, and a directory with a positive host link count reports one link, with no such stand-aside, because counting file systems add a link per subdirectory; a deleted directory keeps its zero count, which is guest state rather than host churn.[^c7][^c8] The access, status-change and birth times are always the run's configured epoch, whatever the host inode holds and whatever earlier calls did to the host file, while the modification time is the file's virtual time from the same pool that numbers its inode.[^c9] That virtual modification time keeps an observed host time only when it is already one of the canonical archive values, exactly zero or one whole seconds with no nanosecond part; any real host timestamp becomes the epoch instead.[^c11] The value is settled once, at the first resolving stat unless an earlier write has left its bump pending, and afterwards moves only to a handled write's bump or to a time the guest explicitly sets.[^c20] The container's standard streams keep a fixed inode and report the epoch for every time, whatever backs them on the host; their device and mount identity still follow that backing (G11).[^c14][^c23]

**Interface fidelity.** The argument checks mirror the kernel's, before any file is looked at: a reserved bit in the request mask, unknown flags on a call that names a path, and both synchronization modes at once are invalid, while a call that names no path — the empty-path form that stats the descriptor itself — does not have its other flags examined, just as on Linux.[^c4][^linux] Where the target is a file Detcore renders, those checks are Detcore's to enforce and a failure is `EINVAL`; everywhere else the injected kernel call decides existence, permissions and every other error, and a missing result buffer is `EFAULT` once the call has run.[^c3][^c5] The result mask is handled field by field. The kernel's returned mask is carried through Detcore's stat form, dropping bits that form does not know, and the basic fields it names — mode, owners, device numbers for special files, block size and the attribute words, themselves truncated to the attribute bits Detcore knows — keep the kernel's values except where the rewrite above replaces inode, device, directory size, directory link count and the times.[^c10][^c12] The birth time is the sharp edge of that policy: when the kernel reports it, its mask bit survives but the value the guest reads is the epoch, not the host file's creation time (G2).[^c9][^c10] The times are written into the result whether or not their mask bits survive, where Linux zeroes its working result before filling it and then copies every filled field out whatever the returned mask says, so a guest can read an epoch or virtual time from a field the returned mask does not claim (G10).[^c9][^c10][^linux] A modification time is read from the host only when the kernel's mask says the field was filled, and then only to classify it as canonical or host-specific as above.[^c21][^c11] On this virtualized path the mount identity is never the host's number: a request for the unique mount identity is translated to a request for the classic one before the kernel is asked, the classic number is renumbered through the run's mount-identity pool — the same numbering the run's mount tables show, a file on a hidden mount showing its nearest shown ancestor — and a unique request is answered with that pooled number shifted above every classic number, under the unique bit alone.[^c13][^c15][^linux] Where no pooled number exists for the file's mount, the field is zero and both mount bits are cleared, so no guest reads a number that names nothing.[^c15] Everything past the mount identity — the direct-I/O alignments, the subvolume and the atomic-write limits — has no place in Detcore's stat form and is never carried back: on the path without metadata virtualization the same boundary is enforced by clearing every mask bit past the modeled set and zeroing the tail of the structure, as a kernel without those fields would answer for those fields, while the attribute words before the tail keep the host kernel's values, so an atomic-write attribute can still show beside cleared limits (G1).[^c10][^c12] The follow, automount and synchronization flags are the kernel's to honor on the injected call, since Detcore changes nothing about which file is found or how fresh its attributes are.[^c5][^linux]

**Record & replay.** The call is subscribed in both run and record/replay modes (code facts in the Appendix).[^c2] Record and replay always run without metadata virtualization, so that subscription serves the unvirtualized branch: the record-or-replay tool records the kernel's answer and replay returns it, host inode, device, mount identity and host times included, with the unmodeled tail canonicalized after the call in recording and replay alike, because a recording keeps only the modeled fields and the recorded run must not see host values its replay cannot reproduce (G5).[^c16] The virtualized rewrite carries no recording of its own: it injects the call directly rather than handing it to the record-or-replay tool, so no replay regenerates a pooled answer for this call.[^c17]

## Known gaps

- G1 (unimplemented): fields past the mount identity — the direct-I/O alignments, the subvolume and the atomic-write limits — are never faithfully reported: Detcore's stat form has no place for them, and the path without metadata virtualization clears their mask bits and zeroes them, so a guest that asks for them gets the answer of a kernel without those fields and has no fallback inside the call.[^c10][^c12]
- G2 (by design): the access, status-change and birth times are always the run's epoch, so a guest never sees a file's real creation time even when it asks for the birth time and the kernel reports one, and no guest action that would move the host access or status-change time is visible here either; where the host file system reports no birth time Linux leaves that field zero and Hermit reports the epoch, and where a file system keeps no access times Linux still copies the inode's stored access time with its mask bit clear while Hermit reports the epoch.[^c9][^c10]
- G3 (by design): a directory's reported size and positive link count are pinned rather than counted, except that where a backend cannot prove a directory is not on procfs the size pin stands aside for the host size (the link pin does not), a proved procfs directory keeps its kernel values, and a deleted directory keeps its zero link count; a guest that infers a directory's contents from its size or link count is therefore told nothing, on purpose.[^c7][^c8]
- G4 (unimplemented): the unique mount identity is derived from the pooled classic number, and classic numbers can be reused after an unmount where Linux never reuses a unique one, so a mount created on a freed classic number shows the earlier mount's unique identity here; a mount the pool cannot number reports no mount identity at all.[^c15]
- G5 (by design): without metadata virtualization nothing but the unmodeled tail is rewritten, so host inode numbers, devices, mount identity and timestamps reach the guest as the kernel wrote them: a plain run in that configuration makes no cross-run determinism claim for this call, while in record and replay — which always run that way — replay fidelity rests on the recording returning the recorded answer.[^c16]
- G6 (unimplemented): host inode reuse is contained, not handled: a file that never had a name, or one that disappears without the guest removing its name, is never retired, so a named file the host gives its freed inode inherits its pooled number and its virtual modification time, and whether the host reuses the inode differs between runs.[^c18]
- G7 (unimplemented): a procfs file Detcore cannot name — threads not sequentialized, the guest mount table unreadable, a file outside the guest procfs mounts, a failed reopen, or a result buffer over the guest path — is numbered by its raw host identity instead of a rendered name, and the run records a determinism loss for it.[^c19]
- G8 (by design): the settled virtual modification time is never re-read from the host: a file changed by a writer outside the run, or in any way the handled write and time-setting bumps do not cover, keeps its settled or last bumped time even though the fresh kernel result supplies its new size, so a guest comparing modification times alone can miss the change.[^c20][^c11]
- G9 (by design): the fields the rewrite leaves alone — mode, ownership, the device-file number, block size, blocks, and a non-directory's size — are the host file system's when the injected call runs: a file system that changes under the run, or differs between hosts, changes them, and Hermit makes this call's view of them repeatable only so far as the file system itself repeats.[^c5][^c10]
- G10 (unimplemented): the epoch times and the virtual modification time are written into the buffer even where the returned mask does not claim the field, so a guest reading an unclaimed field finds a time value where Linux's zeroed-then-filled result leaves whatever nothing filled as zero.[^c9][^c10]
- G11 (by design): a standard stream's device and mount identity follow what backs it on the host despite its fixed inode: its device is pooled from the backing's host device in first-observation order, so a pipe, a terminal or a redirected file gives the stream a different device and renumbers every device first seen after it, and its mount identity pools the host mount's number, or is zero with both mount bits cleared where no pooled number exists.[^c6][^c14][^c15][^c23]

## Discussion

[Start a discussion about `statx`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+statx&body=Discussion+for+the+%60statx%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fstatx.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60statx%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Appendix

### Code facts

Detcore classifies `statx` (332) as determinized via typed dispatch; deterministic refusal: false; run subscription: true; record/replay subscription: true.

- Reviewed list: `detcore/src/syscall_classification.rs:1939`
- Dispatch: `detcore/src/lib.rs:8191`

### Tests

Which fields a result keeps and the argument contract: `detcore/src/syscalls/files.rs` (`a_statx_result_keeps_only_the_fields_a_recording_keeps`, `statx_sync_modes_keep_linux_positive_and_negative_contract`) — what Hermit does, and interface fidelity. The epoch times and the directory link pin: `detcore/src/syscalls/files/determinize_stat_tests.rs` (`determinize_stat_reads_the_mode_and_epoch_it_is_configured_with`, `determinize_stat_reports_one_link_for_directories_only`) — what Hermit does. Device encoding shared by the stat forms: `detcore/src/stat.rs` (`stat_and_statx_use_the_same_device_encoding`) — what Hermit does. Guest programs file_timestamp_identity, through the C library's statx wrapper, and stat_metadata_identity, by raw syscall 332, under tests/c exercise timestamp and metadata identity end to end — what Hermit does, and interface fidelity.

### Citations

[^linux]: Linux `statx(2)`:
  argument checks and dispatch: https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/fs/stat.c?h=v7.1.3#n744
  basic attributes: https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/fs/stat.c?h=v7.1.3#n82
  mount identity: https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/fs/stat.c?h=v7.1.3#n296
  zeroing the working result: https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/fs/stat.c?h=v7.1.3#n181
  copying the result out: https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/fs/stat.c?h=v7.1.3#n700
[^c1]: detcore/src/syscall_classification.rs:1939 sha256:9eb2bef21340e388
[^c2]: detcore/src/lib.rs:8191 sha256:0cad7db463a75291
  https://github.com/facebookexperimental/hermit/blob/main/detcore/src/lib.rs#L8191
[^c3]: detcore/src/syscalls/files.rs:13871-13875 sha256:bb7f60a481f13583; detcore/src/syscalls/files.rs:13886-13891 sha256:0da31932013cf083
[^c4]: detcore/src/syscalls/files.rs:3103-3114 sha256:780e3cbbad97c408
[^c5]: detcore/src/syscalls/files.rs:13914-13916 sha256:42f2ee6ef6cd0924; detcore/src/syscalls/files.rs:13963-13970 sha256:641c8bfb4f161e03
[^c6]: detcore/src/syscalls/files.rs:13600-13610 sha256:78c99ba7f9e900a7
[^c7]: detcore/src/syscalls/files.rs:13649-13663 sha256:6664919e96f08b5d
[^c8]: detcore/src/syscalls/files.rs:13701-13707 sha256:bfa7e0e34a36312c
[^c9]: detcore/src/syscalls/files.rs:13709-13721 sha256:572b7c93469f9a04
[^c10]: detcore/src/stat.rs:196-216 sha256:f19ad4d6d9d571e4; detcore/src/stat.rs:226-250 sha256:45f7752e1b948d9b
[^c11]: detcore/src/tool_global.rs:305-330 sha256:f8d3222b7c0108f3
[^c12]: detcore/src/syscalls/files.rs:3035-3043 sha256:91a1597de763efc6; detcore/src/syscalls/files.rs:3053-3066 sha256:cf1d27fdd3accca1
[^c13]: detcore/src/syscalls/files.rs:3018-3033 sha256:311f066007405c98; detcore/src/syscalls/files.rs:13901-13908 sha256:c15dc296c003e54d
[^c14]: detcore/src/syscalls/files.rs:13528-13539 sha256:f5fb10672ad6821d
[^c15]: detcore/src/syscalls/files.rs:10617-10639 sha256:d810d6a7b85066a2
[^c16]: detcore/src/syscalls/files.rs:13984-14009 sha256:9695d027c4f1543b
[^c17]: detcore/src/syscalls/files/procfs_numbers.rs:525-534 sha256:2f66d64f35d4e433
[^c18]: detcore/src/tool_global.rs:611-613 sha256:f82534f1f5e06fc0; detcore/src/tool_global.rs:623-627 sha256:6aa1303484ad62a8
[^c19]: detcore/src/syscalls/files.rs:3463-3477 sha256:b994713baaf730e2; detcore/src/syscalls/files.rs:3486-3491 sha256:2826534f684f3c82; detcore/src/syscalls/files.rs:13591-13597 sha256:cfe44e3bca5b52b6; detcore/src/syscalls/files.rs:13480-13490 sha256:8b8bc26bbd638b82
[^c20]: detcore/src/tool_global.rs:711-726 sha256:d710614a575e8000; detcore/src/tool_global.rs:8093-8105 sha256:3e3443044cde2bf0; detcore/src/syscalls/files.rs:17288-17305 sha256:630073d24e9d58ea
[^c21]: detcore/src/syscalls/files.rs:13540-13545 sha256:b7fbcd5fea15d619
[^c22]: detcore/src/syscalls/files/procfs_numbers.rs:96-102 sha256:c97f91460b46b20e; detcore/src/syscalls/files/procfs_numbers.rs:562-565 sha256:483edb1fa08f2544
[^c23]: detcore/src/syscalls/files.rs:13917-13919 sha256:e83e551021f9cbbc; detcore/src/syscalls/files.rs:13958-13962 sha256:aab31a6f66c0e233; detcore/src/tool_global.rs:1169-1185 sha256:911eb680f2cbee9f

## Status

- 2026-11-15 · rev 54bd03547df0 · facts-only page generated from Detcore code facts at this revision.
- 2026-11-16 · rev 7ac736d54e14 · facts refreshed from Detcore at this revision.
- 2026-11-18 · rev 403e350e9e82 · facts refreshed from Detcore at this revision.
- 2026-11-20 · rev 403e350e9e82 · checked determinism-strategy prose added (rubric v3); citations verified at this revision.
- 2026-11-20 · rev 403e350e9e82 · checked prose corrected after review; citations re-verified at this revision.
