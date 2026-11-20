---
schema: syscall-pages/v3
syscall: readlink
nr_x86_64: 89
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+readlink&body=Discussion+for+the+%60readlink%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Freadlink.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60readlink%60%2C+or+in+what+the+page+says%3F%0A
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

Natively, this call copies a symbolic link's target straight from the host kernel, and for the magic links under procfs that target names host processes, host inodes and whatever program the host actually launched. Hermit keeps the kernel's lookup and its errors for ordinary links, but answers the procfs links its model owns itself and rewrites the host identities in the targets it lets through, so a guest reads its own image, its own descriptors and run-local inode numbers instead of the host's. Where a launched run's kernel link would name the launcher rather than the guest's image and Hermit cannot prove the right target, the call is refused by name instead of answered wrongly. Known gaps: 5.

## Determinism strategy

**What is nondeterministic natively.** For an ordinary link the target is whatever the host file system holds, and for the procfs magic links — a task's executable, working-directory and root links, its descriptor links and its namespace links — the kernel renders the target from live host state: the file the host kernel exec'd, the host process and thread numbers in the path's own spelling, and the host inode numbers inside pipe, socket and namespace targets.[^linux] A guest that finds its own executable, inventories its descriptors or compares namespace identities therefore observes the host's process table and the launcher's path, none of which a repeated run reproduces.[^linux]

**What Hermit does.** Detcore determinizes the call in layers, leaving what it does not model to the kernel: typed dispatch reaches a handler that refuses a non-positive buffer size before reading the path, exactly where Linux does, then tries each modeled answer in turn and only then lets the kernel decide.[^c1][^c2][^c3] A link into a directory Detcore serves itself is answered from that model,[^c18] and one of the caller's own executable, working-directory or root links, spelled from the guest's own procfs, is answered from Detcore's knowledge of the caller's image and directories — the executable link only where the backend cannot name the guest's own descriptors, for where it can the kernel names the image exactly and its answer stands; the spelling itself first decides whose link it names — with a missing task and a followed link answered as Linux answers them.[^c5][^c6][^c14][^c17] In a launched run, where the host kernel exec'd a launcher and its executable link names that launcher, the kernel is still asked first so it decides whether the link is there and every error, and Detcore then substitutes the guest image's recorded path for the launcher's bytes, restoring whatever the kernel wrote past the shorter target.[^c7][^c8] Links no layer answers run in the kernel, and their results are canonicalized on the way back: a namespace link reports its canonical constant target, and a descriptor link to a pipe or socket reports the deterministic inode Detcore numbers that object by, in place of the host inode inside the kernel's target.[^c10] A spelling that names one of Detcore's own runtime descriptors is answered as absent rather than allowed to reveal it.[^c11]

**Interface fidelity.** The buffer-size check is Linux's, in Linux's place: the size is taken as a signed value and a size that is not positive fails with `EINVAL` before the path is read, and a missing path pointer is `EFAULT`.[^c3][^c4][^linux] Whether the named file is a link at all is otherwise the kernel's decision on every path Detcore does not answer itself, so a regular file or directory fails exactly as it does natively, and the same holds for existence and permission errors; on the modeled own-task path a followed executable link and a followed directory link keep their distinct native errors rather than collapsing into one.[^c5][^c6][^linux] Truncation follows Linux in every ordinary case, modeled or not: at most the buffer's worth of the target is copied, no terminator is added, and the return value is the number of bytes written, so a target that exactly fills the buffer is indistinguishable from one that was cut — and where Detcore substitutes a shorter image path for the kernel's longer answer, the bytes past it are restored to what the guest's buffer held, as a native call would have left them.[^c9][^c10][^linux] The empty-path form belongs to the descriptor-relative sibling, not to this call: here an empty pathname names no file and the kernel's lookup decides it, while the descriptor-relative sibling is handled separately and likewise lets the kernel read the descriptor's own link for its empty path, so neither form invents a target Detcore cannot prove.[^c12][^c19][^linux] Three refusals by name stand in for answers Detcore cannot prove, each with no fallback inside the call: a launched executable link with no record of the image or the image no longer at its recorded path; a spelling Detcore could not resolve whose kernel answer turns out to be the launcher's path; and another thread's working-directory or root link under backend link facts (G1, G3).[^c8][^c13][^c16]

**Record & replay.** The call is subscribed in both run and record/replay modes (code facts in the Appendix).[^c2] Kernel-answered links go through the record-or-replay path, so replay returns the recorded target instead of re-reading a host link that may have moved, and Detcore's own internal link reads through that path retry identically in both phases.[^c12][^c15] The modeled own-task answers are the deliberate exception: under record and replay they stand aside and the recorded kernel answer stands, because replay cannot re-derive the caller's directories from a recording the way a live run derives them (G2).[^c14]

## Known gaps

- G1 (unimplemented): in a launched run, an executable link whose image Detcore has no record of, or whose image is no longer at its recorded path, is refused by name instead of answered, because the kernel's link names the launcher and no other source proves the current target; the same refusal meets a spelling Detcore could not resolve — after a guest change of root, or through a descriptor for the link itself — whose kernel answer turns out to be the launcher's path; a guest has no fallback inside the call.[^c8][^c13]
- G2 (by design): the modeled own-task links are answered from Detcore's knowledge only outside record and replay, outside a nested PID namespace, before any guest change of root, below the kernel's path-length limit (`PATH_MAX`), and, for the executable link, only where the backend cannot name the guest's own descriptors; in those states the kernel's or the recording's answer stands, host spellings included, except that in a launched run an executable-link answer naming the launcher is replaced with the image's recorded path, or refused where nothing proves it (G1).[^c6][^c7][^c14]
- G3 (unimplemented): another thread's working-directory or root link, where Detcore reads link facts from the backend and cannot prove the threads share them, is refused by name rather than answered with the caller's directory; the same thread's links are served.[^c16]
- G4 (by design): an ordinary link Detcore does not model is the kernel's answer verbatim, so a host file system that changes between runs changes the target — Hermit fixes which identities a target may contain, not the contents of a changing file system.[^c12]
- G5 (unimplemented): at the top of the buffer-size range the canonicalized answer truncates by the full size register where Linux, and Detcore's own size check, take a signed int size, so a size at or above two to the thirty-second power whose low half is positive can write and return the whole target where Linux writes at most the low-half count.[^c20][^c4][^linux]

## Discussion

[Start a discussion about `readlink`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+readlink&body=Discussion+for+the+%60readlink%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Freadlink.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60readlink%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Appendix

### Code facts

Detcore classifies `readlink` (89) as determinized via typed dispatch; deterministic refusal: false; run subscription: true; record/replay subscription: true.

- Reviewed list: `detcore/src/syscall_classification.rs:1855`
- Dispatch: `detcore/src/lib.rs:8195`

### Tests

Which spellings name whose executable link: `detcore/src/syscalls/namespace.rs` (`names_whose_exe_link_a_spelling_reads`) — what Hermit does, and interface fidelity. The caller's own links under every spelling: `detcore/src/syscalls/namespace/own_links.rs` (`names_the_own_links_of_every_spelling`) — what Hermit does. Guest programs proc_link_targets, proc_own_links, proc_fd_link_aliases and procfs_links under tests/c exercise the procfs links end to end — what Hermit does, and interface fidelity.

### Citations

[^linux]: Linux `readlink(2)`:
  size check, lookup and the non-link error: https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/fs/stat.c?h=v7.1.3#n559
  truncating copy: https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/fs/namei.c?h=v7.1.3#n6229
  reading the link body: https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/fs/namei.c?h=v7.1.3#n6251
  the executable magic link: https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/fs/proc/base.c?h=v7.1.3#n1780
  rendering a procfs link target: https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/fs/proc/base.c?h=v7.1.3#n1822
[^c1]: detcore/src/syscall_classification.rs:1855 sha256:29ae9989ef760ea3
[^c2]: detcore/src/lib.rs:8195 sha256:f382cc8fe2548f91
  https://github.com/facebookexperimental/hermit/blob/main/detcore/src/lib.rs#L8195
[^c3]: detcore/src/syscalls/namespace.rs:2409-2411 sha256:ed6ef59e645b5ea6
[^c4]: detcore/src/syscalls/namespace.rs:407-413 sha256:997ab9bbbe14cdfb
[^c5]: detcore/src/syscalls/namespace.rs:2423-2433 sha256:135b88b08b91cd8e
[^c6]: detcore/src/syscalls/namespace/own_links.rs:329-342 sha256:078099d8f5642fcc; detcore/src/syscalls/namespace/own_links.rs:343-358 sha256:5ada796beb7aa809; detcore/src/syscalls/namespace/own_links.rs:167 sha256:9b454ce5e5fc5264
[^c7]: detcore/src/syscalls/namespace.rs:2434-2457 sha256:c15900b78ba18d3f; detcore/src/syscalls/namespace.rs:1447-1469 sha256:2ae463dccc4977ce
[^c8]: detcore/src/syscalls/namespace.rs:1610-1616 sha256:eb7678f90dd8d8af
[^c9]: detcore/src/syscalls/namespace.rs:1628-1647 sha256:6a264172d2e8028f
[^c10]: detcore/src/syscalls/namespace.rs:950-1000 sha256:def18c239c243a17
[^c11]: detcore/src/syscalls/namespace.rs:2486-2491 sha256:12429099265b9d06
[^c12]: detcore/src/syscalls/namespace.rs:1182-1192 sha256:2c62707ade04a13d; detcore/src/syscalls/namespace.rs:2498-2510 sha256:2ed46fbbb8e35b4d
[^c13]: detcore/src/syscalls/namespace.rs:1495-1510 sha256:020ba9970217aa0b; detcore/src/syscalls/namespace.rs:1512-1558 sha256:e6c2b2140ce87654
[^c14]: detcore/src/syscalls/namespace/own_links.rs:305-328 sha256:b9a881e1efc477a0
[^c15]: detcore/src/syscalls/files.rs:9234-9247 sha256:75318c64b415f695
[^c18]: detcore/src/syscalls/namespace.rs:2412-2422 sha256:00c0fd20d679094d
[^c19]: detcore/src/syscalls/namespace.rs:2559-2565 sha256:63df2dac7d233e91; detcore/src/syscalls/namespace.rs:2640-2641 sha256:56739b51a466c29b; detcore/src/syscalls/namespace.rs:2683-2685 sha256:84517538dd073aad
[^c20]: detcore/src/syscalls/namespace.rs:995 sha256:585bab16489cbf00
[^c16]: detcore/src/syscalls/namespace/own_links.rs:359-379 sha256:3a7cf9a4b1c8145d
[^c17]: detcore/src/syscalls/namespace.rs:617 sha256:4a3e64896152ea2e

## Status

- 2026-11-15 · rev 54bd03547df0 · facts-only page generated from Detcore code facts at this revision.
- 2026-11-16 · rev 7ac736d54e14 · facts refreshed from Detcore at this revision.
- 2026-11-18 · rev 403e350e9e82 · facts refreshed from Detcore at this revision.
- 2026-11-20 · rev 403e350e9e82 · checked determinism-strategy prose added (rubric v3); citations verified at this revision.
- 2026-11-20 · rev 403e350e9e82 · checked prose corrected after review; citations re-verified at this revision.
