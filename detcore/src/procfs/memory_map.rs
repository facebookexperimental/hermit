/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! A process's `maps` rendered from the backend's memory map
//! (`reverie::Guest::memory_map`), where the backend gives one: each listed
//! mapping as one line, as Linux prints it (`show_map_vma` and
//! `show_vma_header_prefix` in `fs/proc/task_mmu.c`), in the order listed.
//!
//! The backend's list is exact for each mapping it lists, but it need not be
//! complete (`reverie::MemoryMap::complete`). On ptrace it is the kernel's own
//! text, so the rendered file is the one the kernel prints. On KVM it holds the
//! stack window's mappings only, `[stack]` and the parts a split left of it:
//! the image, the interpreter, the vDSO, the heap and every mapping `mmap(2)`
//! made are missing from the rendered file, a named difference of the KVM
//! provider (<https://github.com/rrnewton/reverie/issues/1211>). Nothing here
//! infers from a missing line that no mapping is there, and an incomplete list
//! that lists nothing is no answer: KVM's, once LinuxEmu has lost track of the
//! stack window (an `mprotect(2)` across a hole in it, which LinuxEmu refuses
//! whole where Linux changes the part before the hole, or a change a `vfork`
//! partner made). The file is then read as it was before this rendering.
//!
//! One line the kernel prints is left out, and only here: `[uprobes]`, the
//! page the kernel adds to a process the first time a uprobe fires in it
//! (`xol_add_vma` in `kernel/events/uprobes.c`). Whether it is there depends
//! on what other processes on the host do (a profiler installing a uprobe on a
//! file the guest runs), not on the guest, so it can be in one run of a guest
//! and not in the next one on the same host. Lines that depend on the host or
//! on the backend but stay the same from run to run on one host are kept:
//! `[vsyscall]` (the kernel's `vsyscall=` mode), `[vvar_vclock]` (the kernel's
//! version), the sizes of `[vdso]` and `[vvar]`, and the pages a backend maps
//! into the guest for itself, which only `super::private_maps` removes, for
//! the backends that report them.
//!
//! The page is left out of the file this module renders, and nowhere else: it
//! still shows in `smaps`, in another process's `maps`, and in
//! a `maps` that is read from the kernel's file (by another process that
//! inherited the descriptor, or after an `exec`). Nor is the page's effect on
//! the rest of the map hidden: the kernel asks for it one page below the top
//! of the address space, which on a host with 4-level paging is inside the
//! stack, so there it takes a page a later mapping could have had (read from
//! the source, not measured: this host has 5-level paging, where that address
//! is free). x86's `[uprobes-trampoline]` page, which the kernel maps near a
//! probed instruction it optimizes (`create_uprobe_trampoline` in
//! `arch/x86/kernel/uprobes.c`), is not left out.

pub(crate) mod smaps;

use detcore_model::futex::MmId;
use reverie::MappingBacking;
use reverie::MappingFact;
use reverie::MemoryMap;
use serde::Deserialize;
use serde::Serialize;

use super::BoundTask;
use super::TaskIncarnation;
use super::listing::ProcfsTaskAnswer;

/// The column `seq_pad` fills a header to before a name, with
/// `seq_setwidth(m, 25 + sizeof(void *) * 6 - 1)` on a 64-bit kernel.
const NAME_COLUMN: usize = 25 + 8 * 6 - 1;

/// The name the kernel gives the page uprobes execute their copied
/// instructions from (`xol_add_vma`).
const UPROBES: &[u8] = b"[uprobes]";

/// The bytes of a `maps` file listing `map`'s mappings, but for any
/// `[uprobes]` page. `None` where `map` is an incomplete list that lists
/// nothing once that page is left out, or where a mapping's backing is one
/// this renderer does not know, so that the caller reads the file another way
/// rather than print a file Linux would not.
pub(crate) fn render(map: &MemoryMap) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    for mapping in &map.mappings {
        if matches!(&mapping.backing, MappingBacking::Special(name) if name == UPROBES) {
            continue;
        }
        line(mapping, &mut out)?;
    }
    if !map.complete && out.is_empty() {
        return None;
    }
    Some(out)
}

/// Append `mapping`'s line, newline included.
fn line(mapping: &MappingFact, out: &mut Vec<u8>) -> Option<()> {
    let (device, inode, name): (u64, u64, Option<Vec<u8>>) = match &mapping.backing {
        MappingBacking::Anonymous => (0, 0, None),
        MappingBacking::Stack => (0, 0, Some(b"[stack]".to_vec())),
        MappingBacking::Heap => (0, 0, Some(b"[heap]".to_vec())),
        MappingBacking::Vdso => (0, 0, Some(b"[vdso]".to_vec())),
        MappingBacking::Vvar => (0, 0, Some(b"[vvar]".to_vec())),
        MappingBacking::NamedAnonymous(name) => (0, 0, Some([b"[anon:", &name[..], b"]"].concat())),
        MappingBacking::Special(name) => (0, 0, Some(name.clone())),
        MappingBacking::File {
            raw_device,
            raw_inode,
            path,
        } => (*raw_device, *raw_inode, Some(path.clone())),
        MappingBacking::NamedSharedAnonymous {
            raw_device,
            raw_inode,
            name,
        } => (
            *raw_device,
            *raw_inode,
            Some([b"[anon_shmem:", &name[..], b"]"].concat()),
        ),
        _ => return None,
    };
    let prot = |bit: libc::c_int, set: u8| {
        if mapping.prot & bit as u32 != 0 {
            set
        } else {
            b'-'
        }
    };
    let start = out.len();
    out.extend_from_slice(
        format!(
            "{:08x}-{:08x} {}{}{}{} {:08x} {:02x}:{:02x} {} ",
            mapping.start,
            mapping.end,
            prot(libc::PROT_READ, b'r') as char,
            prot(libc::PROT_WRITE, b'w') as char,
            prot(libc::PROT_EXEC, b'x') as char,
            if mapping.shared { 's' } else { 'p' },
            mapping.offset,
            libc::major(device),
            libc::minor(device),
            inode,
        )
        .as_bytes(),
    );
    if let Some(name) = name {
        let header = out.len() - start;
        out.extend(std::iter::repeat_n(
            b' ',
            NAME_COLUMN.saturating_sub(header) + 1,
        ));
        out.extend_from_slice(&name);
    }
    out.push(b'\n');
    Some(())
}

/// What a process's `maps`, opened by a task of the process its path names,
/// is bound to where Detcore may render it (`Detcore::maps_from_memory_map`):
/// the task its path named, which of the tasks to hold that task's numbers it
/// was, and the address space it held at the open, the opener's.
///
/// Linux's open takes the task's address space (`proc_maps_open`), and each
/// read first finds the task again by the pid its inode holds (`m_start`):
/// ESRCH once that task is reaped, whichever task holds its number by then.
/// Detcore gives a reaped task's number out again
/// (`Scheduler::forget_previous_process_exit`), and an address space's
/// identity ([`MmId`]) is a task's number and an exec count that a fork
/// starts again at 0. So a task holding a reused number may hold an address
/// space with the bound one's identity, and only the number's incarnation
/// ([`TaskIncarnation`]) tells the two tasks apart.
///
/// That incarnation is the bound task's: [`Binding::held_by`] compares the
/// reader's address space by its identity alone, which a reused number can
/// repeat while the bound task lives. On ptrace, once the kernel's pid space
/// wraps, a task that shares a reaped task's address space (a `CLONE_VM`
/// child that is not a thread) can open its maps and then fork a child that
/// gets the reaped task's number: that child's address space has the bound
/// one's identity, so it reads its own map through the inherited descriptor,
/// where Linux prints the opener's. Telling the two apart needs the
/// incarnation of the address space's maker in [`MmId`], which also keys
/// futexes. LinuxEmu never reuses a task's number.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct Binding {
    task: BoundTask,
    incarnation: TaskIncarnation,
    mm: MmId,
}

impl Binding {
    /// The binding of a `maps` whose path named `task`, a task of the
    /// opener's process, where the process table answered `answer` about that
    /// task at the open and the opener's address space is `opener`. `None`
    /// where the task holds no address space, having exited (Linux's file
    /// then reads empty, as the kernel's does), or another one.
    pub(crate) fn at_open(
        task: BoundTask,
        answer: &ProcfsTaskAnswer,
        opener: MmId,
    ) -> Option<Self> {
        (answer.exists && !answer.without_mm && answer.mm == Some(opener)).then_some(Self {
            task,
            incarnation: answer.incarnation(),
            mm: opener,
        })
    }

    /// The task the path named, which a read asks the process table about.
    pub(crate) fn task(&self) -> BoundTask {
        self.task
    }

    /// Whether a reader whose address space is `reader` holds the bound one:
    /// not another process that inherited the descriptor, nor the opener's
    /// process after an `exec`. The reader is then one of the address space's
    /// users, so Linux's read finds it still in use.
    pub(crate) fn held_by(&self, reader: MmId) -> bool {
        self.mm == reader
    }

    /// Whether `answer`, the process table's answer about the bound task at a
    /// read, is about that very task, which Linux's read finds: it still
    /// exists (a zombie too) and no other task has taken its numbers.
    pub(crate) fn finds_its_task(&self, answer: &ProcfsTaskAnswer) -> bool {
        answer.still_names(self.incarnation)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A map as Linux prints it (`show_map_vma`), with a line of each kind:
    /// files, `[heap]`, unnamed and named anonymous memory, write-only and
    /// shared anonymous memory, named shared anonymous memory, shared memory
    /// Linux backs with `/dev/zero (deleted)`, a path that holds a newline
    /// (`\012`), a file with no path, the vDSO's pages, a device with a
    /// three-digit major and an inode past the name column, a header longer
    /// than the name column (a nine-digit offset, the widest device and
    /// inode), a stack `mprotect(2)` split, and the gate area. The padding
    /// rule it follows reproduced each of the 46 lines of a process's own
    /// map on Linux 7.1.3.
    const LINUX: &[u8] = b"\
        00400000-00401000 r--p 00000000 00:2a 2952102844                         /tmp/a.out\n\
        00401000-00402000 r-xp 00001000 00:2a 2952102844                         /tmp/a.out\n\
        00405000-00426000 rw-p 00000000 00:00 0                                  [heap]\n\
        7ffff7c00000-7ffff7c29000 r--p 00000000 00:20 73572743                   /usr/lib64/libc.so.6\n\
        7ffff7dfe000-7ffff7e0b000 rw-p 00000000 00:00 0 \n\
        7ffff7e0b000-7ffff7e0c000 -w-p 00000000 00:00 0 \n\
        7ffff7e0c000-7ffff7e0d000 --xs 00000000 00:00 0 \n\
        7ffff7faf000-7ffff7fb2000 rw-p 00000000 00:00 0                          [anon:probe]\n\
        7ffff7fb2000-7ffff7fb3000 rw-p 00000000 00:00 0                          [anon:]\n\
        7ffff7fb3000-7ffff7fb4000 rw-s 00002000 00:01 1036                       [anon_shmem:shared]\n\
        7ffff7fb4000-7ffff7fb5000 rw-s 00000000 00:01 1037                       /dev/zero (deleted)\n\
        7ffff7fb5000-7ffff7fb6000 r--p 00000000 00:2a 17                         /tmp/new\\012line\n\
        7ffff7fb6000-7ffff7fb7000 r--s 00000000 00:0f 1057                       anon_inode:[perf_event]\n\
        7ffff7fbc000-7ffff7fc0000 r--p 00000000 00:00 0                          [vvar]\n\
        7ffff7fc0000-7ffff7fc2000 r--p 00000000 00:00 0                          [vvar_vclock]\n\
        7ffff7fc2000-7ffff7fc4000 r-xp 00000000 00:00 0                          [vdso]\n\
        7ffff7fc4000-7ffff7fc6000 r--p 00000000 103:02 1234567890123             /usr/lib64/ld-linux-x86-64.so.2\n\
        7ffff7fc6000-7ffff7fc7000 r--p 1000000000 fff:fffff 18446744073709551615  /f\n\
        7ffffffde000-7fffffff6000 rw-p 00000000 00:00 0 \n\
        7fffffff6000-7fffffff7000 r--p 00000000 00:00 0 \n\
        7fffffff7000-7ffffffff000 rw-p 00000000 00:00 0                          [stack]\n\
        ffffffffff600000-ffffffffff601000 --xp 00000000 00:00 0                  [vsyscall]\n";

    /// The memory map a backend reports for Linux's text renders as that
    /// text, byte for byte.
    #[test]
    fn a_rendered_map_is_the_text_linux_prints() {
        let map = MemoryMap::from_maps(LINUX).unwrap();
        assert_eq!(map.mappings.len(), 22);
        assert_eq!(
            String::from_utf8_lossy(&render(&map).unwrap()),
            String::from_utf8_lossy(LINUX)
        );
    }

    /// The map Linux prints for this test's own process renders as the same
    /// text, but for a `[uprobes]` line where the host gave the process one.
    /// Another thread of the process may change its mappings while the
    /// file is read, which can give text out of order, so a text that does
    /// not parse is read again.
    #[test]
    fn a_rendered_map_is_the_text_linux_prints_for_this_process() {
        let (text, map) = (0..100)
            .find_map(|_| {
                let text = std::fs::read("/proc/self/maps").unwrap();
                let map = MemoryMap::from_maps(&text).ok()?;
                Some((text, map))
            })
            .expect("a map that parses");
        assert!(map.mappings.len() > 3, "{map:?}");
        let expected: Vec<u8> = text
            .split_inclusive(|byte| *byte == b'\n')
            .filter(|line| !line.ends_with(b" [uprobes]\n"))
            .flatten()
            .copied()
            .collect();
        assert_eq!(
            String::from_utf8_lossy(&render(&map).unwrap()),
            String::from_utf8_lossy(&expected)
        );
    }

    /// A `[uprobes]` page is left out, and every other line is as Linux
    /// prints it. An incomplete list whose only mapping is that page lists
    /// nothing once it is left out, which is no answer; a complete one is an
    /// empty file.
    #[test]
    fn a_rendered_map_hides_the_uprobes_page() {
        // The line a process on a Linux 7.1.3 host with five-level paging
        // listed once a uprobe installed on the host fired in it: the page
        // below the top of its address space, executable only.
        let uprobes =
            b"ffffffffffe000-fffffffffff000 --xp 00000000 00:00 0                      [uprobes]\n";
        let at = LINUX.len()
            - b"ffffffffff600000-ffffffffff601000 --xp 00000000 00:00 0                  [vsyscall]\n"
                .len();
        let text = [&LINUX[..at], uprobes, &LINUX[at..]].concat();
        let map = MemoryMap::from_maps(&text).unwrap();
        assert_eq!(map.mappings.len(), 23);
        assert_eq!(
            map.mappings[21].backing,
            MappingBacking::Special(UPROBES.to_vec())
        );
        assert_eq!(
            String::from_utf8_lossy(&render(&map).unwrap()),
            String::from_utf8_lossy(LINUX)
        );
        let only_uprobes =
            |complete| MemoryMap::new(vec![map.mappings[21].clone()], complete).unwrap();
        assert_eq!(render(&only_uprobes(false)), None);
        assert_eq!(render(&only_uprobes(true)).unwrap(), b"");
    }

    /// A complete list that lists nothing, the map of a task that has no
    /// address space left, is an empty file; an incomplete list that lists
    /// nothing is no answer (KVM's, once LinuxEmu has lost track of the stack
    /// window); an incomplete list renders only what it lists, here the stack
    /// window a guard zone split.
    #[test]
    fn a_rendered_map_lists_only_what_the_backend_lists() {
        assert_eq!(
            render(&MemoryMap::new(Vec::new(), true).unwrap()).unwrap(),
            b""
        );
        assert_eq!(render(&MemoryMap::new(Vec::new(), false).unwrap()), None);
        let read_write = (libc::PROT_READ | libc::PROT_WRITE) as u32;
        let window = MemoryMap::new(
            vec![
                MappingFact::new(
                    0x7fff_ff80_0000,
                    0x7fff_fffe_b000,
                    read_write,
                    false,
                    0,
                    MappingBacking::Anonymous,
                )
                .unwrap(),
                MappingFact::new(
                    0x7fff_fffe_b000,
                    0x7fff_fffe_f000,
                    0,
                    false,
                    0,
                    MappingBacking::Anonymous,
                )
                .unwrap(),
                MappingFact::new(
                    0x7fff_fffe_f000,
                    0x7fff_ffff_f000,
                    read_write,
                    false,
                    0,
                    MappingBacking::Stack,
                )
                .unwrap(),
            ],
            false,
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(render(&window).unwrap()).unwrap(),
            "\
            7fffff800000-7ffffffeb000 rw-p 00000000 00:00 0 \n\
            7ffffffeb000-7ffffffef000 ---p 00000000 00:00 0 \n\
            7ffffffef000-7ffffffff000 rw-p 00000000 00:00 0                          [stack]\n"
        );
    }

    /// The process table's answer about a task that holds the numbers
    /// `incarnation` names and the address space `mm`, or none.
    fn answer(incarnation: u32, mm: Option<MmId>) -> ProcfsTaskAnswer {
        ProcfsTaskAnswer {
            exists: true,
            without_mm: mm.is_none(),
            process_incarnation: incarnation,
            thread_incarnation: None,
            mm,
            runtime_private_memory: None,
        }
    }

    /// A `maps` binds the task its path named where that task holds the
    /// opener's address space at the open, and only there; a read of it is the
    /// reader's map while the reader holds that address space and the task is
    /// still there, a zombie too.
    #[test]
    fn a_maps_binding_takes_the_task_and_the_address_space_of_the_open() {
        let process = detcore_model::pid::DetTid::from_raw(5);
        let task = BoundTask {
            process: 5,
            thread: None,
        };
        let opener = MmId::initial(process);
        assert_eq!(Binding::at_open(task, &answer(1, None), opener), None);
        assert_eq!(
            Binding::at_open(task, &answer(1, Some(opener.for_exec(process))), opener),
            None
        );
        let gone = ProcfsTaskAnswer::default();
        assert_eq!(Binding::at_open(task, &gone, opener), None);
        // Whatever address space an answer names, a task that is not there,
        // or has released its address space, binds nothing.
        for (exists, without_mm) in [(false, false), (true, true)] {
            let inconsistent = ProcfsTaskAnswer {
                exists,
                without_mm,
                ..answer(1, Some(opener))
            };
            assert_eq!(Binding::at_open(task, &inconsistent, opener), None);
        }
        let binding = Binding::at_open(task, &answer(1, Some(opener)), opener).unwrap();
        assert_eq!(binding.task(), task);
        assert!(binding.held_by(opener));
        assert!(binding.finds_its_task(&answer(1, Some(opener))));
        // Its leader a zombie, whose address space Linux has released, while
        // another thread of the process still holds it.
        assert!(binding.finds_its_task(&answer(1, None)));
        // Reaped, with no task on its number.
        assert!(!binding.finds_its_task(&gone));
        // Another process that inherited the descriptor, and the process
        // after an exec.
        assert!(!binding.held_by(MmId::initial(detcore_model::pid::DetTid::from_raw(6))));
        assert!(!binding.held_by(opener.for_exec(process)));
    }

    /// Process 5 opens its `maps`, forks process 6 and exits, and is reaped;
    /// 6 forks a child that Detcore gives the number 5 again, which inherits
    /// the descriptor. That child's address space has the identity of the
    /// opener's (a fork's exec count starts again at 0), so the address
    /// space alone would render the child's own map where Linux's read
    /// answers ESRCH for the reaped task: the number's incarnation refuses
    /// it.
    #[test]
    fn a_maps_binding_refuses_a_task_on_a_reused_number() {
        let reused = detcore_model::pid::DetTid::from_raw(5);
        let task = BoundTask {
            process: 5,
            thread: None,
        };
        let opener = MmId::initial(reused);
        let binding = Binding::at_open(task, &answer(1, Some(opener)), opener).unwrap();
        let child = MmId::for_clone(
            MmId::initial(detcore_model::pid::DetTid::from_raw(6)),
            reused,
            false,
        );
        assert_eq!(child, opener);
        assert!(binding.held_by(child));
        assert!(!binding.finds_its_task(&answer(2, Some(child))));
    }
}
