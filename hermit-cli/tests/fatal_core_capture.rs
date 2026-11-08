/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! `hermit run --fatal-core-dir`: the ptrace backend keeps a capped,
//! compressed core for each guest process a core-dumping signal kills, without
//! changing what the run reports.

#[path = "common/hermit_binary.rs"]
mod hermit_test;

use std::fs;
use std::io::Read;
use std::ops::Range;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;

const MARKER: &[u8] = b"HERMIT-FATAL-CORE-MARKER";
/// What the guest's faulting thread holds in r12 in `threads` mode.
const THREAD_MARKER: u64 = 0x4845_524d_4954_3132;
/// In `parked` mode, parked thread `i` holds `PARK_REG_MARKER + i` in r12 and
/// `PARK_FP_MARKER + i` in xmm7.
const PARK_REG_MARKER: u64 = 0x4845_524d_4954_5200;
const PARK_FP_MARKER: u64 = 0x4845_524d_4954_4600;
const NT_PRSTATUS: u32 = 1;
const NT_PRFPREG: u32 = 2;
const NT_PRPSINFO: u32 = 3;
const NT_AUXV: u32 = 6;
const NT_X86_XSTATE: u32 = 0x202;
const NT_FILE: u32 = 0x4649_4c45;
const MIB: u64 = 1 << 20;
/// Reverie's private page, where its ptrace backend runs the syscalls it
/// injects for a guest thread (reverie-ptrace `cp::PRIVATE_PAGE_OFFSET`, one
/// page). No guest code runs there.
const REVERIE_PRIVATE_PAGE: Range<u64> = 0x7100_0000..0x7100_1000;

/// Builds the guest once per test into its own directory and returns it with
/// that directory.
fn guest(test: &str) -> (PathBuf, PathBuf) {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository");
    let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("fatal-core-capture-{}", std::process::id()))
        .join(test);
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).expect("failed to create the test directory");
    let guest = root.join("fatal_core_capture");
    let output = Command::new("cc")
        .args([
            "-O1",
            "-std=gnu11",
            "-pthread",
            "-Wall",
            "-Wextra",
            "-Werror",
        ])
        .arg(repository.join("tests/c/fatal_core_capture.c"))
        .arg("-o")
        .arg(&guest)
        .output()
        .expect("failed to start cc");
    assert!(
        output.status.success(),
        "guest compilation failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    (guest, root)
}

/// Runs `hermit <global> run <run_args> -- <guest> <guest_args>`.
fn hermit(run_args: &[&str], guest: &Path, guest_args: &[&str]) -> Output {
    let mut command = Command::new("timeout");
    command
        .args(["--kill-after", "5s", "120s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args(["run", "--base-env=minimal"])
        .args(run_args)
        .arg("--")
        .arg(guest)
        .args(guest_args);
    hermit_test::hermit_output(&format!("{command:?}"), &mut command)
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// The files in `dir`, by name, with their sizes. An absent directory has none.
fn files(dir: &Path) -> Vec<(String, u64)> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<_> = entries
        .map(|entry| {
            let entry = entry.expect("failed to read the core directory");
            let name = entry.file_name().to_string_lossy().into_owned();
            (name, entry.metadata().expect("failed to stat a core").len())
        })
        .collect();
    files.sort();
    files
}

/// Decompresses a kept core, a sequence of zstd frames, and checks that it is
/// an ELF core file.
fn decode_core(path: &Path) -> Vec<u8> {
    let stored = fs::read(path).expect("failed to read a kept core");
    let mut rest = stored.as_slice();
    let mut core = Vec::new();
    while !rest.is_empty() {
        let mut frame = ruzstd::decoding::StreamingDecoder::new(&mut rest)
            .unwrap_or_else(|error| panic!("{} is not zstd: {error}", path.display()));
        frame
            .read_to_end(&mut core)
            .unwrap_or_else(|error| panic!("{} is not zstd: {error}", path.display()));
    }
    assert_eq!(&core[..4], b"\x7fELF", "{} is not ELF", path.display());
    assert_eq!(
        u16::from_le_bytes([core[16], core[17]]),
        4,
        "{} is not an ET_CORE file",
        path.display()
    );
    core
}

/// The bytes of memory a decompressed core's `PT_LOAD` segments store.
fn stored_memory(core: &[u8]) -> u64 {
    let at = |offset: usize, len: usize| {
        let mut bytes = [0u8; 8];
        bytes[..len].copy_from_slice(&core[offset..offset + len]);
        u64::from_le_bytes(bytes)
    };
    let (phoff, phentsize, phnum) = (at(32, 8) as usize, at(54, 2) as usize, at(56, 2));
    (0..phnum as usize)
        .map(|i| phoff + i * phentsize)
        .filter(|&phdr| at(phdr, 4) == 1) // PT_LOAD
        .map(|phdr| at(phdr + 32, 8)) // p_filesz
        .sum()
}

#[test]
fn a_segfault_keeps_one_core_and_reports_what_it_would_without_one() {
    let (guest, root) = guest("segfault");
    let cores = root.join("cores");
    let dir = cores.to_str().unwrap();

    let without = hermit(&[], &guest, &["segv"]);
    let with = hermit(&["--fatal-core-dir", dir], &guest, &["segv"]);

    assert!(
        !without.status.success(),
        "the guest must die:\n{}",
        stderr(&without)
    );
    assert_eq!(
        with.status,
        without.status,
        "capture changed the exit status\nwith:\n{}\nwithout:\n{}",
        stderr(&with),
        stderr(&without)
    );
    assert_eq!(with.stdout, without.stdout);
    assert!(
        !stderr(&without).contains("fatal core"),
        "a run without --fatal-core-dir must keep nothing:\n{}",
        stderr(&without)
    );

    let kept = files(&cores);
    assert_eq!(kept.len(), 1, "expected one core, found {kept:?}");
    let (name, _) = &kept[0];
    assert!(
        name.starts_with("hermit-") && name.ends_with(".SIGSEGV.zst"),
        "unexpected core name {name}"
    );
    assert!(
        stderr(&with)
            .lines()
            .any(|line| line.starts_with("hermit: fatal core for thread ")
                && line.ends_with(&format!(" (SIGSEGV): kept as {name}"))),
        "hermit did not report the core by name alone (a size or tier depends on \
         host timing and would differ between two --verify runs):\n{}",
        stderr(&with)
    );
    let core = decode_core(&cores.join(name));
    assert!(
        core.windows(MARKER.len()).any(|window| window == MARKER),
        "the core does not hold the guest's heap"
    );
}

/// r12 in a decompressed core's first `NT_PRSTATUS` note, which is written
/// first, right after the program headers.
fn prstatus_r12(core: &[u8]) -> u64 {
    let u64_at = |offset: usize| u64::from_le_bytes(core[offset..offset + 8].try_into().unwrap());
    let u32_at = |offset: usize| u32::from_le_bytes(core[offset..offset + 4].try_into().unwrap());
    let notes = u64_at(u64_at(32) as usize + 8) as usize; // PT_NOTE p_offset
    assert_eq!(u32_at(notes + 8), 1, "the first note is not NT_PRSTATUS");
    // 12-byte note header, "CORE\0" padded to 8, then elf_prstatus, whose
    // registers start at 112; r12 is the fourth.
    u64_at(notes + 20 + 112 + 3 * 8)
}

#[test]
fn memory_excluded_with_madv_dontdump_stays_out_of_the_core() {
    let (guest, root) = guest("dontdump");
    let secret = b"HERMIT-DONTDUMP-SECRET";
    let executable = fs::read(&guest).expect("failed to read the guest executable");
    assert!(
        !executable
            .windows(secret.len())
            .any(|window| window == secret),
        "the guest ELF must not hold an independent plaintext copy of the secret"
    );
    let cores = root.join("cores");
    let output = hermit(
        &["--fatal-core-dir", cores.to_str().unwrap()],
        &guest,
        &["dontdump"],
    );
    assert!(
        !output.status.success(),
        "the guest must die:\n{}",
        stderr(&output)
    );
    let kept = files(&cores);
    assert_eq!(kept.len(), 1, "expected one core, found {kept:?}");
    let core = decode_core(&cores.join(&kept[0].0));
    let holds = |needle: &[u8]| core.windows(needle.len()).any(|window| window == needle);
    assert!(holds(MARKER), "the core does not hold the guest's heap");
    assert!(
        !holds(b"HERMIT-DONTDUMP-SECRET"),
        "the core holds memory the guest excluded with MADV_DONTDUMP"
    );

    // The same generated page must be present when only MADV_DONTDUMP is
    // omitted, so a missing page or failed search cannot satisfy the test.
    let control_cores = root.join("control-cores");
    let control = hermit(
        &["--fatal-core-dir", control_cores.to_str().unwrap()],
        &guest,
        &["dumpable-secret"],
    );
    assert_eq!(
        control.status,
        output.status,
        "the control guest must die the same way:\n{}",
        stderr(&control)
    );
    let kept = files(&control_cores);
    assert_eq!(kept.len(), 1, "expected one control core, found {kept:?}");
    let core = decode_core(&control_cores.join(&kept[0].0));
    let holds = |needle: &[u8]| core.windows(needle.len()).any(|window| window == needle);
    assert!(
        holds(MARKER),
        "the control core does not hold the guest's heap"
    );
    assert!(
        holds(secret),
        "the control core does not hold the page without MADV_DONTDUMP"
    );
}

#[test]
fn files_from_other_invocations_neither_block_a_core_nor_get_removed() {
    let (guest, root) = guest("others");
    let cores = root.join("cores");
    fs::create_dir_all(&cores).unwrap();
    // Names an older scheme or another invocation could have used for the
    // same guest process; the guest's first process is PID 3 in its namespace.
    let others = [
        "hermit-1-core.3.3.SIGSEGV.zst",
        "hermit-1-0000000000000000-core.3.3.SIGSEGV.zst",
    ];
    for name in others {
        fs::write(cores.join(name), b"not ours").unwrap();
    }
    // A temporary file exists only while its writer holds the directory lock,
    // so one found by a capture holding the lock belongs to a dead writer.
    let orphan = ".hermit-1-0000000000000000-core.3.3.SIGSEGV.zst.tmp";
    fs::write(cores.join(orphan), b"a dead writer's").unwrap();
    // Last written long before the capture's time limit, as a dead writer's is.
    fs::File::options()
        .write(true)
        .open(cores.join(orphan))
        .unwrap()
        .set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(3600))
        .unwrap();
    let dir = cores.to_str().unwrap();
    let crashed = hermit(&["--fatal-core-dir", dir], &guest, &["segv"]);
    assert!(
        stderr(&crashed).contains(": kept as hermit-"),
        "an existing file stopped the core:\n{}",
        stderr(&crashed)
    );
    assert!(
        !cores.join(orphan).exists(),
        "the capture kept a dead writer's temporary file"
    );
    let green = hermit(&["--fatal-core-dir", dir], &guest, &["child-segv-ok"]);
    assert!(green.status.success(), "{}", stderr(&green));
    let left: Vec<String> = files(&cores).into_iter().map(|(name, _)| name).collect();
    for name in others {
        assert!(
            left.iter().any(|kept| kept == name),
            "{name} was removed: {left:?}"
        );
    }
    assert_eq!(
        left.len(),
        others.len() + 1,
        "the crashed run's core must survive the later green run: {left:?}"
    );
}

#[test]
fn a_multithreaded_crash_keeps_the_faulting_threads_registers() {
    let (guest, root) = guest("threads");
    let cores = root.join("cores");
    let output = hermit(
        &["--fatal-core-dir", cores.to_str().unwrap()],
        &guest,
        &["threads"],
    );
    assert!(
        !output.status.success(),
        "the guest must die:\n{}",
        stderr(&output)
    );
    let kept = files(&cores);
    assert_eq!(kept.len(), 1, "expected one core, found {kept:?}");
    assert_eq!(
        stderr(&output).matches("fatal core").count(),
        1,
        "expected one report:\n{}",
        stderr(&output)
    );
    let r12 = prstatus_r12(&decode_core(&cores.join(&kept[0].0)));
    assert_eq!(
        r12, THREAD_MARKER,
        "the core holds another thread's registers (r12 {r12:#x})"
    );
}

/// A thread's fault ends the whole process even while its main thread waits on
/// a condition variable nothing signals, whose futex wait no thread's exit
/// wakes. The run ends as the native run does, by SIGSEGV (shell status 139);
/// before, Hermit saw the leader still waiting after the other threads were
/// reaped and refused the run as a deadlock (exit 125). With a deadlock under
/// Hermit's model reported as its own outcome, that same race would read as
/// exit 121 and `HERMIT_MODEL_DEADLOCK`, so neither may appear.
#[test]
fn a_fault_ends_the_run_while_the_leader_waits_on_a_condition_variable() {
    use std::os::unix::process::ExitStatusExt as _;

    let (guest, _root) = guest("leader-waits");
    let mut native = Command::new("timeout");
    native
        .args(["--kill-after", "5s", "120s"])
        .arg(&guest)
        .arg("leader-waits");
    let native = native.output().expect("failed to run the guest natively");
    assert_eq!(
        native.status.signal(),
        Some(libc::SIGSEGV),
        "the native run must end by SIGSEGV: {:?}",
        native.status
    );
    let output = hermit(&[], &guest, &["leader-waits"]);
    assert!(
        !stderr(&output).contains("Deadlock detected"),
        "hermit refused the run as a deadlock:\n{}",
        stderr(&output)
    );
    assert!(
        !stderr(&output).contains(detcore_model::HERMIT_MODEL_DEADLOCK_LINE)
            && output.status.code() != Some(detcore_model::HERMIT_GUEST_DEADLOCK_EXIT),
        "a signal death read as a deadlock under Hermit's model:\n{}",
        stderr(&output)
    );
    assert_eq!(
        output.status,
        native.status,
        "hermit's exit status differs from the native run's:\n{}",
        stderr(&output)
    );
}

/// The TIDs `parked` mode prints: main, the threads parked in futex,
/// nanosleep and read (parked threads 1 to 3), and the one that aborts.
struct ParkedTids {
    main: i32,
    parked: [i32; 3],
    aborted: i32,
}

impl ParkedTids {
    fn of(output: &Output) -> Self {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let line = stdout
            .lines()
            .find_map(|line| line.strip_prefix("parked "))
            .unwrap_or_else(|| panic!("the guest printed no TIDs:\n{stdout}\n{}", stderr(output)));
        let tids: Vec<i32> = line
            .split(' ')
            .map(|field| field.split_once('=').unwrap().1.parse().unwrap())
            .collect();
        Self {
            main: tids[0],
            parked: [tids[1], tids[2], tids[3]],
            aborted: tids[4],
        }
    }

    /// The order a core lists them in: the dumping thread, then the others
    /// by TID.
    fn in_core_order(&self) -> Vec<i32> {
        let mut others = vec![self.main];
        others.extend(self.parked);
        others.sort();
        [vec![self.aborted], others].concat()
    }
}

/// The `(name, type, contents)` of each note in a decompressed core's
/// `PT_NOTE` segment.
fn notes(core: &[u8]) -> Vec<(String, u32, Vec<u8>)> {
    let u32_at = |offset: usize| u32::from_le_bytes(core[offset..offset + 4].try_into().unwrap());
    let u64_at = |offset: usize| u64::from_le_bytes(core[offset..offset + 8].try_into().unwrap());
    let phdr = u64_at(32) as usize; // the PT_NOTE program header comes first
    assert_eq!(u32_at(phdr), 4, "the first program header is not PT_NOTE");
    let (mut at, end) = (
        u64_at(phdr + 8) as usize,
        (u64_at(phdr + 8) + u64_at(phdr + 32)) as usize,
    );
    let mut notes = Vec::new();
    while at < end {
        let (namesz, descsz) = (u32_at(at) as usize, u32_at(at + 4) as usize);
        let name = String::from_utf8_lossy(&core[at + 12..at + 12 + namesz - 1]).into_owned();
        let desc = at + 12 + namesz.next_multiple_of(4);
        notes.push((name, u32_at(at + 8), core[desc..desc + descsz].to_vec()));
        at = desc + descsz.next_multiple_of(4);
    }
    notes
}

/// One thread's notes in a core.
struct CoreThread<'a> {
    /// The `(name, type)` of each note from its `NT_PRSTATUS` to the next
    /// thread's.
    layout: Vec<(&'a str, u32)>,
    /// Its `NT_PRSTATUS` contents.
    status: &'a [u8],
    /// Its `NT_PRFPREG` contents.
    fp: Option<&'a [u8]>,
}

/// A core's notes grouped by thread, each group starting at an
/// `NT_PRSTATUS`.
fn threads(notes: &[(String, u32, Vec<u8>)]) -> Vec<CoreThread<'_>> {
    let mut threads: Vec<CoreThread<'_>> = Vec::new();
    for (name, kind, desc) in notes {
        if *kind == NT_PRSTATUS {
            threads.push(CoreThread {
                layout: Vec::new(),
                status: desc,
                fp: None,
            });
        }
        let thread = threads
            .last_mut()
            .expect("a note before the first NT_PRSTATUS");
        thread.layout.push((name, *kind));
        if *kind == NT_PRFPREG {
            thread.fp = Some(desc);
        }
    }
    threads
}

fn u64_at(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
}

fn i32_at(bytes: &[u8], offset: usize) -> i32 {
    i32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

/// `(p_flags, p_vaddr, p_filesz, p_memsz)` of each `PT_LOAD` of a core.
fn loads(core: &[u8]) -> Vec<(u32, u64, u64, u64)> {
    let phnum = u16::from_le_bytes([core[56], core[57]]) as usize;
    let phoff = u64_at(core, 32) as usize;
    (0..phnum)
        .map(|i| phoff + i * 56)
        .filter(|&phdr| i32_at(core, phdr) == 1)
        .map(|phdr| {
            (
                i32_at(core, phdr + 4) as u32,
                u64_at(core, phdr + 16),
                u64_at(core, phdr + 32),
                u64_at(core, phdr + 40),
            )
        })
        .collect()
}

/// Where `guest`'s `PT_GNU_RELRO` segment, which holds its `.dynamic`, was
/// loaded: its address in the file, moved by the distance between the
/// `AT_PHDR` the core's `NT_AUXV` records and the file's `PT_PHDR`.
fn relro_address(guest: &Path, notes: &[(String, u32, Vec<u8>)]) -> u64 {
    let elf = fs::read(guest).expect("failed to read the guest executable");
    let (phoff, phnum) = (
        u64_at(&elf, 32) as usize,
        u16::from_le_bytes([elf[56], elf[57]]) as usize,
    );
    let vaddr_of = |kind: u32| {
        (0..phnum)
            .map(|i| phoff + i * 56)
            .find(|&phdr| i32_at(&elf, phdr) as u32 == kind)
            .map(|phdr| u64_at(&elf, phdr + 16))
            .unwrap_or_else(|| panic!("the guest has no program header of type {kind:#x}"))
    };
    let (_, _, auxv) = notes.iter().find(|(_, kind, _)| *kind == NT_AUXV).unwrap();
    let at_phdr = auxv
        .as_chunks::<16>()
        .0
        .iter()
        .find(|entry| u64_at(&entry[..], 0) == 3) // AT_PHDR
        .map(|entry| u64_at(entry, 8))
        .expect("NT_AUXV has no AT_PHDR");
    at_phdr - vaddr_of(6) + vaddr_of(0x6474_e552) // PT_PHDR, PT_GNU_RELRO
}

/// The address ranges of `guest`'s code in a core: each mapping `NT_FILE`
/// lists under the guest's file name that an executable `PT_LOAD` covers.
fn guest_code(guest: &Path, core: &[u8], notes: &[(String, u32, Vec<u8>)]) -> Vec<Range<u64>> {
    let (_, _, files) = notes
        .iter()
        .find(|(_, kind, _)| *kind == NT_FILE)
        .expect("the core has no NT_FILE note");
    // The count and the page size, a (start, end, page offset) triple per
    // file, then the names, each ending in a NUL.
    let count = u64_at(files, 0) as usize;
    let names = files[16 + count * 24..].split(|&byte| byte == 0);
    let name = guest.file_name().unwrap().as_bytes();
    let loads = loads(core);
    (0..count)
        .zip(names)
        .filter(|(_, path)| path.rsplit(|&byte| byte == b'/').next() == Some(name))
        .map(|(i, _)| u64_at(files, 16 + i * 24)..u64_at(files, 16 + i * 24 + 8))
        .filter(|range| {
            loads.iter().any(|&(flags, vaddr, _, memsz)| {
                flags & 1 != 0 && vaddr <= range.start && range.end <= vaddr + memsz
            })
        })
        .collect()
}

/// Checks that `core` holds each thread's own general registers, not those
/// of a syscall the backend ran for it on Reverie's private page: no thread's
/// rip, nor its rcx, which a `syscall` instruction sets to the address after
/// it, lies in [`REVERIE_PRIVATE_PAGE`], and each parked thread's rip lies in
/// the guest executable's code, where its `park_forever` loop issues its
/// syscall. Unlike a comparison of two cores, this fails on one wrong core
/// whichever thread the backend offered first.
fn assert_threads_hold_their_own_registers(guest: &Path, tids: &ParkedTids, core: &[u8]) {
    let notes = notes(core);
    let code = guest_code(guest, core, &notes);
    assert!(
        !code.is_empty(),
        "the core maps no code of {}",
        guest.display()
    );
    for CoreThread { status, .. } in &threads(&notes) {
        let tid = i32_at(status, 32);
        // elf_prstatus's registers start at 112, in user_regs_struct's order:
        // rcx is the twelfth and rip the seventeenth.
        let (rcx, rip) = (u64_at(status, 112 + 11 * 8), u64_at(status, 112 + 16 * 8));
        for (name, value) in [("rip", rip), ("rcx", rcx)] {
            assert!(
                !REVERIE_PRIVATE_PAGE.contains(&value),
                "thread {tid}'s {name} {value:#x} is in Reverie's private page: the core holds \
                 the registers of a syscall the backend ran for the thread, not its own"
            );
        }
        if tids.parked.contains(&tid) {
            assert!(
                code.iter().any(|range| range.contains(&rip)),
                "parked thread {tid}'s rip {rip:#x} is not in the guest's code {code:x?}"
            );
        }
    }
}

/// Runs `guest` in `parked` mode with `--fatal-core-dir=<cores>` and returns
/// its TIDs and its one decompressed core, after checking that the core holds
/// every thread's own registers ([`assert_threads_hold_their_own_registers`]).
fn parked_core(guest: &Path, cores: &Path) -> (ParkedTids, Vec<u8>) {
    let output = hermit(
        &["--fatal-core-dir", cores.to_str().unwrap()],
        guest,
        &["parked"],
    );
    assert!(
        !output.status.success(),
        "the guest must die:\n{}",
        stderr(&output)
    );
    let tids = ParkedTids::of(&output);
    let kept = files(cores);
    assert_eq!(kept.len(), 1, "expected one core, found {kept:?}");
    let report = format!(
        "hermit: fatal core for thread {} (SIGABRT): kept as {}",
        tids.aborted, kept[0].0
    );
    assert_eq!(
        stderr(&output).matches("fatal core").count(),
        1,
        "expected one report:\n{}",
        stderr(&output)
    );
    assert!(
        stderr(&output).contains(&report),
        "expected {report:?}:\n{}",
        stderr(&output)
    );
    let core = decode_core(&cores.join(&kept[0].0));
    assert_threads_hold_their_own_registers(guest, &tids, &core);
    (tids, core)
}

/// A process a core-dumping signal ends keeps one core holding every thread,
/// laid out as the kernel's own dump: the thread that took the signal first
/// and the others by TID, the process-wide notes after the first
/// `NT_PRSTATUS`, and each thread's FP notes after its own. Each parked
/// thread's general and FP registers are its own, and every thread's stack
/// pointer and instruction pointer lie in its memory.
#[test]
fn a_crash_keeps_every_thread_with_its_own_registers() {
    let (guest, root) = guest("parked");
    let (tids, core) = parked_core(&guest, &root.join("cores"));
    let notes = notes(&core);
    let threads = threads(&notes);
    let pids: Vec<i32> = threads
        .iter()
        .map(|thread| i32_at(thread.status, 32))
        .collect();
    assert_eq!(
        pids,
        tids.in_core_order(),
        "the core holds other threads, or lists them out of order"
    );
    let first = &threads[0].layout;
    assert_eq!(
        first[..4],
        [
            ("CORE", NT_PRSTATUS),
            ("CORE", NT_PRPSINFO),
            ("CORE", NT_AUXV),
            ("CORE", NT_FILE)
        ],
        "the process-wide notes do not follow the first NT_PRSTATUS"
    );
    let regsets = &first[4..];
    assert_eq!(
        regsets,
        [("CORE", NT_PRFPREG), ("LINUX", NT_X86_XSTATE)],
        "the dumping thread's register notes differ from the kernel's"
    );
    let (_, _, psinfo) = notes
        .iter()
        .find(|(_, kind, _)| *kind == NT_PRPSINFO)
        .unwrap();
    assert_eq!(
        i32_at(psinfo, 24),
        tids.main,
        "NT_PRPSINFO's pr_pid is not the TGID"
    );
    assert_eq!(
        (psinfo[0], psinfo[1]),
        (2, b'D'),
        "the leader, waiting in pthread_join, is not in the dump's wait"
    );
    for CoreThread { layout, status, fp } in &threads {
        assert_eq!(i32_at(status, 12) as i16, libc::SIGABRT as i16, "pr_cursig");
        assert_eq!(i32_at(status, 328), 1, "pr_fpvalid");
        assert!(fp.is_some_and(|fp| fp.len() == 512), "{layout:?}");
        assert_eq!(
            layout[layout.len() - regsets.len()..],
            *regsets,
            "a thread's register notes differ from the dumping thread's"
        );
    }

    for (index, tid) in tids.parked.iter().enumerate() {
        let marker = index as u64 + 1;
        let CoreThread { status, fp, .. } = threads
            .iter()
            .find(|thread| i32_at(thread.status, 32) == *tid)
            .unwrap();
        // elf_prstatus's registers start at 112; r12 is the fourth.
        assert_eq!(
            u64_at(status, 112 + 3 * 8),
            PARK_REG_MARKER + marker,
            "r12 of {tid}"
        );
        // fxsave's xmm registers start at 160, 16 bytes each.
        let xmm7 = u64_at(fp.unwrap(), 160 + 7 * 16);
        assert_eq!(xmm7, PARK_FP_MARKER + marker, "xmm7 of {tid}");
    }
    let loads = loads(&core);
    let within = |addr: u64, executable: bool| {
        loads.iter().any(|&(flags, vaddr, filesz, memsz)| {
            let end = vaddr + if executable { memsz } else { filesz };
            (vaddr..end).contains(&addr) && (!executable || flags & 1 != 0)
        })
    };
    let relro = relro_address(&guest, &notes);
    assert!(
        loads
            .iter()
            .any(|&(_, vaddr, filesz, _)| (vaddr..vaddr + filesz).contains(&relro)),
        "the core leaves out the executable's relocated read-only page {relro:#x}"
    );
    for CoreThread { status, .. } in &threads {
        let (rsp, rip) = (u64_at(status, 112 + 19 * 8), u64_at(status, 112 + 16 * 8));
        let tid = i32_at(status, 32);
        assert!(
            within(rsp, false),
            "thread {tid}'s rsp {rsp:#x} is not in stored memory"
        );
        assert!(
            within(rip, true),
            "thread {tid}'s rip {rip:#x} is not in executable memory"
        );
    }
}

/// gdb, where installed, reads the core as a native one: every thread, each
/// stopped in the function it parked in, and the libc frames of the threads
/// in libc, which gdb finds only through the relocated `.dynamic`. Without a
/// gdb this skips loudly, and fails instead where `HERMIT_REQUIRE_DAP_GDB`
/// promises one, as the hermit-dap tests do. The validation node that runs
/// this binary does not set it, but its pinned image ships a gdb.
#[test]
fn gdb_shows_every_thread_where_it_stopped() {
    const TEST: &str = "gdb_shows_every_thread_where_it_stopped";
    let gdb = ["/usr/bin/gdb".into()]
        .into_iter()
        .chain(std::env::var_os("PATH").into_iter().flat_map(|path| {
            std::env::split_paths(&path)
                .map(|directory| directory.join("gdb"))
                .collect::<Vec<_>>()
        }))
        .find(|gdb: &PathBuf| gdb.is_file());
    let Some(gdb) = gdb else {
        assert!(
            std::env::var_os("HERMIT_REQUIRE_DAP_GDB").is_none(),
            "HERMIT_REQUIRE_DAP_GDB is set, but there is no gdb at /usr/bin/gdb or on PATH, \
             so {TEST} cannot read the core with it"
        );
        eprintln!("skipping {TEST}: no gdb at /usr/bin/gdb or on PATH");
        return;
    };
    let (guest, root) = guest("parked-gdb");
    let (tids, core) = parked_core(&guest, &root.join("cores"));
    let path = root.join("core");
    fs::write(&path, &core).unwrap();
    let output = Command::new(&gdb)
        .env_remove("DEBUGINFOD_URLS")
        .args([
            "--batch",
            "-nx",
            "-ex",
            "info threads",
            "-ex",
            "thread apply all bt",
        ])
        .arg(&guest)
        .arg(&path)
        .output()
        .expect("failed to run gdb");
    let text = String::from_utf8_lossy(&output.stdout);
    // `info threads` rows: `* 1    LWP 11  ...`, or `Thread 0x... (LWP 11)`
    // where gdb finds libthread_db; the current thread's row starts with `*`.
    let rows: Vec<(bool, i32)> = text
        .lines()
        .filter_map(|line| {
            let row = line.trim_start_matches('*').trim_start();
            row.split_whitespace().next()?.parse::<u32>().ok()?;
            let (_, lwp) = row.split_once("LWP ")?;
            let lwp = lwp.split(|c: char| !c.is_ascii_digit()).next()?;
            Some((line.starts_with('*'), lwp.parse().ok()?))
        })
        .collect();
    let mut lwps: Vec<i32> = rows.iter().map(|&(_, lwp)| lwp).collect();
    lwps.sort();
    assert_eq!(
        lwps,
        {
            let mut all = tids.in_core_order();
            all.sort();
            all
        },
        "gdb lists other threads:\n{text}"
    );
    assert_eq!(
        rows.iter()
            .find(|&&(current, _)| current)
            .map(|&(_, lwp)| lwp),
        Some(tids.aborted),
        "the current thread is not the one that aborted:\n{text}"
    );
    // A frame's function is followed by " (": `abort` may print as
    // `__GI_abort`, but `main` must not match `__libc_start_main`.
    for frame in [
        "park_in_futex (",
        "park_in_nanosleep (",
        "park_in_read (",
        "abort (",
        " main (",
    ] {
        assert!(text.contains(frame), "no thread is in {frame:?}:\n{text}");
    }
}

/// Two runs of one crashing multithreaded guest keep the same core, byte for
/// byte once decompressed: its threads' registers and memory follow the
/// guest's schedule, and the core lists the threads by TID, not in the host
/// order in which they reached their exit stops.
#[test]
fn two_runs_keep_the_same_core() {
    let (guest, root) = guest("parked-twice");
    let (first_tids, first) = parked_core(&guest, &root.join("cores-1"));
    let (second_tids, second) = parked_core(&guest, &root.join("cores-2"));
    assert_eq!(first_tids.in_core_order(), second_tids.in_core_order());
    assert!(
        first == second,
        "the cores differ ({} and {} bytes), first at byte {:?}",
        first.len(),
        second.len(),
        first.iter().zip(&second).position(|(a, b)| a != b)
    );
}

#[test]
fn cores_stay_within_the_per_core_and_total_caps() {
    let (guest, root) = guest("caps");
    let cores = root.join("cores");
    let max_core = 9 * MIB;
    let max_total = 20 * MIB;
    // 36 MiB of incompressible memory: the 12 MiB child cannot fit the
    // per-core cap, and the four together cannot fit the total.
    let children = ["12", "8", "8", "8"];
    let mut guest_args = vec!["children"];
    guest_args.extend(children);

    let output = hermit(
        &[
            "--fatal-core-dir",
            cores.to_str().unwrap(),
            "--fatal-core-max-bytes",
            &max_core.to_string(),
            "--fatal-core-total-max-bytes",
            &max_total.to_string(),
        ],
        &guest,
        &guest_args,
    );

    // EXIT-CLASS: guest. `children` mode exits 1 after its children crash.
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    let kept = files(&cores);
    assert_eq!(
        kept.len(),
        children.len(),
        "every child should keep a core, if only a smaller one: {kept:?}\n{}",
        stderr(&output)
    );
    let mut stored = Vec::new();
    for (name, bytes) in &kept {
        assert!(*bytes <= max_core, "{name} is {bytes} bytes");
        stored.push(stored_memory(&decode_core(&cores.join(name))));
    }
    let total: u64 = kept.iter().map(|(_, bytes)| bytes).sum();
    assert!(total <= max_total, "the cores take {total} bytes: {kept:?}");
    assert!(
        stored.iter().any(|&bytes| bytes >= 8 * MIB),
        "no core kept a whole 8 MiB child's memory, so the caps were never \
         approached from a full core: {stored:?}\n{}",
        stderr(&output)
    );
    assert!(!stderr(&output).contains("not kept"), "{}", stderr(&output));
}

#[test]
fn verify_parity_is_unchanged_with_capture_on() {
    let (guest, root) = guest("verify");
    let cores = root.join("cores");
    let mut reports = Vec::new();
    for capture in [false, true] {
        let json = root.join(format!("verify-{capture}.json"));
        let mut args = vec![
            "--strict",
            "--verify",
            "--verify-strict",
            "--verify-json",
            json.to_str().unwrap(),
        ];
        if capture {
            args.extend(["--fatal-core-dir", cores.to_str().unwrap()]);
        }
        let output = hermit(&args, &guest, &["child-segv-ok"]);
        assert!(output.status.success(), "{}", stderr(&output));
        if capture {
            for run in ["run1", "run2"] {
                assert!(
                    stderr(&output).contains(&format!("-{run}-core.")),
                    "{run} kept no core, so this compares nothing:\n{}",
                    stderr(&output)
                );
            }
        }
        let report: serde_json::Value =
            serde_json::from_slice(&fs::read(&json).expect("no --verify-json report"))
                .expect("--verify-json is not JSON");
        assert_eq!(report["bitwise_parity"], true, "{report:#}");
        assert_eq!(report["verdict"], "matched", "{report:#}");
        reports.push(report);
    }
    for field in ["compared_log_messages", "compared_outputs"] {
        assert_eq!(
            reports[0][field], reports[1][field],
            "capture changed {field}"
        );
    }
    assert_eq!(
        files(&cores),
        Vec::new(),
        "a matched --verify keeps nothing"
    );
}

#[test]
fn a_green_run_keeps_nothing() {
    let (guest, root) = guest("green");
    let cores = root.join("cores");
    let output = hermit(
        &["--fatal-core-dir", cores.to_str().unwrap()],
        &guest,
        &["child-segv-ok"],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("kept as") && stderr(&output).contains("were removed"),
        "the child's core was never written, so nothing was tested:\n{}",
        stderr(&output)
    );
    assert_eq!(files(&cores), Vec::new());
}

#[test]
fn backends_without_the_ptrace_exit_stop_are_refused() {
    let (guest, root) = guest("refused");
    let cores = root.join("cores");
    let output = hermit(
        &[
            "--namespace-only",
            "--fatal-core-dir",
            cores.to_str().unwrap(),
        ],
        &guest,
        &["child-segv-ok"],
    );
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("--fatal-core-dir"),
        "{}",
        stderr(&output)
    );
    assert!(!cores.exists(), "a refused run created the core directory");
}
