/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Opt-in core files for guest threads killed by a core-dumping signal.
//!
//! The ptrace backend offers each thread of such a process at its exit stop
//! ([`reverie::GlobalTool::on_fatal_signal_exit`]), after the kernel's fatal
//! decision. [`capture`] keeps a core only for the one thread that ran the
//! kernel's core dump step (`exit.dumping`), so the registers in the core are
//! those of the thread that took the signal. A process the kernel would not
//! dump, for example one that is not dumpable, gets no core. It writes one ELF
//! core, compressed as a sequence of zstd frames, into
//! [`FatalCoreCapture::dir`]. It only reads the stopped guest, through procfs,
//! so the guest observes nothing and its exit status is already fixed.
//!
//! The thread's memory is normally still mapped at the exit stop, but nothing
//! pins it: it can be reaped before or during the capture. A page that cannot
//! be read, or reads short, is stored as zeros and the core is kept as a
//! partial one; when `/proc/<tid>/mem` cannot be opened at all, the core holds
//! only the notes and the mapping layout. Neither fails the capture.
//!
//! A core stores only what the kernel's own dump would (see [`dump_rule`]):
//! nothing the guest marked `MADV_DONTDUMP`, nothing of an I/O or raw-PFN
//! mapping, and only the kinds of mapping its `coredump_filter` selects, so
//! shared file mappings are left out by default.
//!
//! Every core gets a name no other core has: the caller's
//! [`FatalCoreCapture::file_prefix`] is unique to the invocation, and a
//! repeated guest process ID within a run gets a numbered name.
//!
//! The capture is best effort and bounded. One core never exceeds
//! `max_core_bytes`; the regular files in the directory never exceed
//! `max_total_bytes` together, counted under a directory lock that every
//! writer takes. It never runs past `time_limit`, nor past `exit.deadline`
//! when the run has already failed and the backend must finish cleaning up.
//! A core that would break either cap, or that takes too long, is retried
//! with less memory: first only the stack the thread was running on, then
//! only the notes (registers, signal, process identity, auxiliary vector and
//! mapped files) and the mapping layout. A core that cannot fit even then is
//! not written. Once a Full core runs out of time, later captures in the same
//! hermit process start at the Stack tier. The tracer waits for a capture only
//! until its end, even when a read or write blocks in the kernel (see
//! [`capture`]), and a capture deletes temporary files that dead writers left
//! before it sizes the remaining total. Every failure is returned for the
//! caller to log.

use std::fs;
use std::fs::File;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::mpsc::RecvTimeoutError;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;

use ::procfs::process::CoredumpFlags;
use ::procfs::process::MMPermissions;
use ::procfs::process::MMapPath;
use ::procfs::process::MemoryMap;
use ::procfs::process::Process;
use ::procfs::process::VmFlags;
use reverie::FatalSignalExit;
use ruzstd::encoding::CompressionLevel;

use crate::config::FatalCoreCapture;

/// Uncompressed bytes per zstd frame.
const CHUNK: usize = 1 << 20;
const PAGE: u64 = 4096;
static ZEROS: [u8; CHUNK] = [0; CHUNK];

/// What one [`capture`] did.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// A core was written to `path`.
    Written { path: PathBuf },
    /// The thread did not run the kernel's core dump step, so it gets no
    /// core: another thread of its process did, or none did.
    NotDumping,
    /// No tier fitted in `budget` bytes before the capture's end.
    NoRoom { budget: u64 },
}

/// How much memory a core holds. Every tier keeps only what the kernel's own
/// dump would: see [`dump_rule`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Tier {
    /// Every mapping the kernel would dump whole, and the first page of each
    /// file mapped from offset 0 (its ELF header).
    Full,
    /// Of those, only the mapping holding the stack pointer and the main
    /// stack, plus the ELF header pages.
    Stack,
    /// No memory: notes and the mapping layout only.
    NotesOnly,
}

/// Captures `exit` under `cfg` and reports the outcome on stderr. Never fails
/// the run.
///
/// The report goes to stderr, not to the tracing log, which a `--verify` run
/// compares. Hermit's stderr can still be the guest's stderr, which `--verify`
/// also compares, so the report names only the file: the core's size and tier
/// depend on host timing and memory pressure and would make two identical runs
/// differ.
pub(crate) fn capture_and_log(cfg: &FatalCoreCapture, exit: &FatalSignalExit) {
    let report = match capture(cfg, exit) {
        Ok(Outcome::Written { path }) => format!(
            "kept as {}",
            path.file_name().unwrap_or_default().to_string_lossy()
        ),
        Ok(Outcome::NotDumping) => return,
        Ok(Outcome::NoRoom { .. }) => {
            "not kept: no core fits within its size and time limits".to_string()
        }
        Err(error) => format!("not kept: {error:#}"),
    };
    let line = format!(
        "hermit: fatal core for thread {} ({}): {report}\n",
        exit.tid, exit.signal
    );
    crate::nonwaiting_write::write_without_waiting(libc::STDERR_FILENO, line.as_bytes());
}

/// Writes a core for the thread `exit` names, held at its exit stop, if it is
/// the thread that ran the kernel's core dump step.
///
/// The work runs on a helper thread that the tracer waits for only until the
/// capture's end, plus [`HELPER_SLACK`]. The capture checks its end between
/// frames, but one read of a file mapping on a hung network filesystem, or a
/// write into a core directory on one, can block in the kernel past any
/// check. The tracer then gives up, so the guest waits no longer, and the
/// helper deletes what it was writing if it ever returns.
pub(crate) fn capture(cfg: &FatalCoreCapture, exit: &FatalSignalExit) -> anyhow::Result<Outcome> {
    if !exit.dumping {
        return Ok(Outcome::NotDumping);
    }
    let end = capture_end(Instant::now(), cfg.time_limit, exit.deadline) + HELPER_SLACK;
    let (cfg, exit) = (cfg.clone(), *exit);
    run_within(end, move |abandoned| {
        // A memory that is already gone still leaves the notes worth keeping.
        let mem = File::open(format!("/proc/{}/mem", exit.tid.as_raw())).ok();
        capture_abandonable(&cfg, &exit, mem.as_ref(), &FULL_TIER_TIMED_OUT, abandoned)
    })
}

/// How long past a capture's own end the tracer still waits for it. A healthy
/// capture overruns its end by at most one frame: one read of up to 1 MiB and
/// its compression.
const HELPER_SLACK: Duration = Duration::from_millis(500);

/// Runs `work` on a helper thread and returns its result if it arrives by
/// `end`. Otherwise sets the flag `work` was given and returns an error: the
/// helper is left to finish on its own and must not keep anything once the
/// flag is set. A panic in `work` is an error too, not a tracer panic.
///
/// A helper that finishes in the instant between the timeout and the flag
/// can still keep its core while the report says it was not kept.
fn run_within(
    end: Instant,
    work: impl FnOnce(&AtomicBool) -> anyhow::Result<Outcome> + Send + 'static,
) -> anyhow::Result<Outcome> {
    let abandoned = Arc::new(AtomicBool::new(false));
    let (send, receive) = std::sync::mpsc::sync_channel(1);
    let flag = Arc::clone(&abandoned);
    std::thread::Builder::new()
        .name("fatal-core".to_string())
        .spawn(move || {
            let _ = send.send(work(&flag));
        })
        .map_err(|error| anyhow::anyhow!("cannot start the capture thread: {error}"))?;
    match receive.recv_timeout(end.saturating_duration_since(Instant::now())) {
        Ok(result) => result,
        Err(RecvTimeoutError::Timeout) => {
            abandoned.store(true, Ordering::SeqCst);
            match receive.try_recv() {
                Ok(result) => result,
                Err(_) => anyhow::bail!("gave up: a read or write blocked past the time limit"),
            }
        }
        Err(RecvTimeoutError::Disconnected) => anyhow::bail!("the capture thread panicked"),
    }
}

/// Set once a Full core has run out of time in this hermit process. Every
/// capture stops the whole guest, so later crashes then go straight to the
/// smaller tiers instead of each spending most of the time limit again.
static FULL_TIER_TIMED_OUT: AtomicBool = AtomicBool::new(false);

/// [`capture`] with the thread's memory, or `None` when it cannot be read at
/// all, in which case only the notes and the mapping layout are kept.
/// `full_timed_out` is [`FULL_TIER_TIMED_OUT`] outside tests. Tests call it
/// directly; [`capture`] goes through [`capture_abandonable`].
#[cfg(test)]
fn capture_from(
    cfg: &FatalCoreCapture,
    exit: &FatalSignalExit,
    mem: Option<&File>,
    full_timed_out: &AtomicBool,
) -> anyhow::Result<Outcome> {
    capture_abandonable(cfg, exit, mem, full_timed_out, &AtomicBool::new(false))
}

/// [`capture_from`] for a helper the tracer may give up on: once `abandoned`
/// is set, a finished core is deleted instead of kept.
fn capture_abandonable(
    cfg: &FatalCoreCapture,
    exit: &FatalSignalExit,
    mem: Option<&File>,
    full_timed_out: &AtomicBool,
    abandoned: &AtomicBool,
) -> anyhow::Result<Outcome> {
    let start = Instant::now();
    let end = capture_end(start, cfg.time_limit, exit.deadline);
    let tid = exit.tid.as_raw();
    let process = Process::new(tid)?;
    let snapshot = Snapshot::read(&process, exit)?;
    fs::create_dir_all(&cfg.dir)?;
    let _lock = DirLock::acquire(&cfg.dir, end)?;
    remove_orphaned_temps(&cfg.dir, cfg.time_limit * 2)?;
    let used = regular_file_bytes(&cfg.dir)?;
    let budget = cfg
        .max_core_bytes
        .min(cfg.max_total_bytes.saturating_sub(used));
    let stem = format!(
        "{}core.{}.{tid}.{}",
        cfg.file_prefix,
        snapshot.tgid,
        exit.signal.as_str()
    );
    let name = free_name(&cfg.dir, &stem)?;
    let path = cfg.dir.join(&name);
    let temp = cfg.dir.join(format!(".{name}.tmp"));
    let span = end.saturating_duration_since(start);
    let tiers: &[(Tier, u32)] = match mem {
        Some(_) if !full_timed_out.load(Ordering::Relaxed) => {
            &[(Tier::Full, 7), (Tier::Stack, 9), (Tier::NotesOnly, 10)]
        }
        Some(_) => &[(Tier::Stack, 9), (Tier::NotesOnly, 10)],
        None => &[(Tier::NotesOnly, 10)],
    };
    for &(tier, tenths) in tiers {
        let deadline = start + span * tenths / 10;
        let segments = snapshot.segments(tier);
        match write_core(&temp, budget, deadline, &snapshot, &segments, mem) {
            Ok(_) => {
                if abandoned.load(Ordering::SeqCst) {
                    let _ = fs::remove_file(&temp);
                    anyhow::bail!("the tracer gave up on this capture");
                }
                fs::rename(&temp, &path)?;
                return Ok(Outcome::Written { path });
            }
            Err(error) => {
                let _ = fs::remove_file(&temp);
                match error {
                    WriteError::Deadline if tier == Tier::Full => {
                        full_timed_out.store(true, Ordering::Relaxed);
                    }
                    WriteError::OverBudget | WriteError::Deadline => {}
                    WriteError::Io(error) => return Err(error.into()),
                }
            }
        }
    }
    Ok(Outcome::NoRoom { budget })
}

/// When a capture started at `start` must be over: after `time_limit`, or at
/// the backend's cleanup `deadline` if that comes first.
fn capture_end(start: Instant, time_limit: Duration, deadline: Option<Instant>) -> Instant {
    let limit = start + time_limit;
    deadline.map_or(limit, |deadline| deadline.min(limit))
}

/// An exclusive `flock` on the core directory itself, held for the whole
/// sizing and writing of one core, so two writers cannot both spend the same
/// remaining total. Locking the directory leaves no lock file behind.
struct DirLock {
    _held: File,
}

impl DirLock {
    fn acquire(dir: &Path, deadline: Instant) -> anyhow::Result<Self> {
        let file = File::open(dir)?;
        loop {
            // SAFETY: flock on a descriptor this function owns.
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                return Ok(Self { _held: file });
            }
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::EWOULDBLOCK) {
                return Err(error.into());
            }
            if Instant::now() >= deadline {
                anyhow::bail!("timed out waiting for the lock on {}", dir.display());
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

/// `<stem>.zst`, or `<stem>.<n>.zst` for the first `n` that names no file yet,
/// so a core never replaces another: a guest process ID can be reused within
/// a run. Called under [`DirLock`], so two writers cannot pick the same name.
fn free_name(dir: &Path, stem: &str) -> std::io::Result<String> {
    let mut name = format!("{stem}.zst");
    for n in 1u32.. {
        if !dir.join(&name).try_exists()? {
            break;
        }
        name = format!("{stem}.{n}.zst");
    }
    Ok(name)
}

/// Removes the temporary files of writers that died mid-core, so they stop
/// counting against `max_total_bytes`. Every writer creates its temp only while
/// it holds [`DirLock`], so one found while the caller holds the lock has no
/// live writer on this host. Only hermit's own core temp names
/// (`.hermit-*core.*.zst.tmp`) are touched, and only ones last written at
/// least `min_age` ago, which also covers a writer on another host whose lock
/// this host cannot see on some network filesystems. A temp that cannot be
/// removed, such as another user's in a sticky directory, is skipped: it keeps
/// counting, but it never stops this capture.
fn remove_orphaned_temps(dir: &Path, min_age: Duration) -> std::io::Result<()> {
    let now = SystemTime::now();
    // Listed without the C library's heap (util::find_in_directory): this
    // can run in the guest's process. As with std's read_dir, a directory
    // that cannot be opened is the caller's error, and a listing error ends
    // the cleanup without failing the capture.
    drop(crate::util::RawFd::open(
        dir,
        libc::O_RDONLY | libc::O_DIRECTORY,
    )?);
    let _ = crate::util::find_in_directory(dir, |name| {
        let text = name.to_string_lossy();
        if !(text.starts_with(".hermit-") && text.contains("core.") && text.ends_with(".zst.tmp")) {
            return None::<()>;
        }
        let path = dir.join(name);
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            return None;
        };
        let old = metadata
            .modified()
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age >= min_age);
        if metadata.is_file() && old {
            let _ = fs::remove_file(&path);
        }
        None
    });
    Ok(())
}

/// Bytes held by the regular files directly in `dir`.
fn regular_file_bytes(dir: &Path) -> std::io::Result<u64> {
    let mut total = 0u64;
    let failed =
        crate::util::find_in_directory(dir, |name| match fs::symlink_metadata(dir.join(name)) {
            Ok(metadata) => {
                if metadata.file_type().is_file() {
                    total = total.saturating_add(metadata.len());
                }
                None
            }
            Err(error) => Some(error),
        })?;
    match failed {
        Some(error) => Err(error),
        None => Ok(total),
    }
}

/// Everything a core needs except memory contents, read before writing.
struct Snapshot {
    tgid: i32,
    stack_pointer: u64,
    notes: Vec<u8>,
    maps: Vec<MemoryMap>,
    /// What may be stored of each of `maps`, by [`dump_rule`].
    rules: Vec<Dump>,
}

impl Snapshot {
    fn read(process: &Process, exit: &FatalSignalExit) -> anyhow::Result<Self> {
        let status = process.status()?;
        let stat = process.stat()?;
        // smaps carries the kernel's VmFlags, which say what a dump must skip;
        // without them, keep the layout and store no memory at all.
        let (maps, flags_known) = match process.smaps() {
            Ok(maps) => (maps.0, true),
            Err(_) => (process.maps()?.0, false),
        };
        let filter = coredump_filter(exit.tid.as_raw());
        let rules = maps
            .iter()
            .map(|map| {
                let flags = map.extension.vm_flags;
                dump_rule(
                    map,
                    (flags_known && !flags.is_empty()).then_some(flags),
                    filter,
                )
            })
            .collect();
        let cmdline = process.cmdline().unwrap_or_default().join(" ");
        let auxv = fs::read(format!("/proc/{}/auxv", exit.tid.as_raw())).ok();
        let identity = Identity {
            tid: exit.tid.as_raw(),
            tgid: status.tgid,
            ppid: stat.ppid,
            pgrp: stat.pgrp,
            sid: stat.session,
            uid: status.ruid,
            gid: status.rgid,
            sigpend: status.sigpnd | status.shdpnd,
            sighold: status.sigblk,
            state: stat.state,
            nice: stat.nice,
            flags: stat.flags,
            comm: stat.comm,
            cmdline,
        };
        let mut notes = Vec::new();
        note(
            &mut notes,
            NT_PRSTATUS,
            &arch::prstatus(exit.signal as i32, &identity, &exit.regs),
        );
        note(&mut notes, NT_PRPSINFO, &prpsinfo(&identity));
        if let Some(auxv) = auxv {
            note(&mut notes, NT_AUXV, &auxv);
        }
        note(&mut notes, NT_FILE, &nt_file(&maps));
        Ok(Self {
            tgid: status.tgid,
            stack_pointer: arch::stack_pointer(&exit.regs),
            notes,
            maps,
            rules,
        })
    }

    fn segments(&self, tier: Tier) -> Vec<Segment> {
        self.maps
            .iter()
            .zip(&self.rules)
            .filter(|(map, _)| dumpable(map))
            .map(|(map, rule)| {
                let (start, end) = map.address;
                let len = end - start;
                let all = rule.whole
                    && match tier {
                        Tier::Full => true,
                        Tier::Stack => {
                            (start..end).contains(&self.stack_pointer)
                                || matches!(map.pathname, MMapPath::Stack)
                        }
                        Tier::NotesOnly => false,
                    };
                let elf_header = tier != Tier::NotesOnly && rule.elf_header;
                let filesz = if all {
                    len
                } else if elf_header {
                    len.min(PAGE)
                } else {
                    0
                };
                Segment {
                    vaddr: start,
                    memsz: len,
                    filesz,
                    flags: segment_flags(map.perms),
                }
            })
            .collect()
    }
}

/// What a core may store of one mapping.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Dump {
    /// All of it.
    whole: bool,
    /// Its first page, the ELF header of a file mapped from offset 0.
    elf_header: bool,
}

/// What the kernel's own dump (`vma_dump_size` in fs/coredump.c) would keep of
/// `map`, given its VmFlags (`None` when unknown, which keeps nothing) and the
/// process's `coredump_filter`:
///
/// - nothing of a mapping the guest excluded with `MADV_DONTDUMP` (`dd`), or
///   of an I/O or raw-PFN mapping (`io`, `pf`), whose read could touch a
///   device;
/// - hugetlb and shared mappings only if the filter bit for their kind is set
///   (shared file mappings are off by default);
/// - anonymous memory, and a writable private file mapping, whose written
///   pages the kernel holds as anonymous, under the anonymous-private bit; a
///   read-only private file mapping under the file-private bit (off by
///   default);
/// - the first page of a file mapped from offset 0 under the ELF-headers bit.
fn dump_rule(map: &MemoryMap, flags: Option<VmFlags>, filter: CoredumpFlags) -> Dump {
    const NOTHING: Dump = Dump {
        whole: false,
        elf_header: false,
    };
    let Some(flags) = flags else {
        return NOTHING;
    };
    if flags.intersects(VmFlags::DD | VmFlags::IO | VmFlags::PF) {
        return NOTHING;
    }
    let file = matches!(map.pathname, MMapPath::Path(_));
    let shared = flags.contains(VmFlags::SH);
    let whole = if flags.contains(VmFlags::HT) {
        filter.contains(if shared {
            CoredumpFlags::SHARED_HUGEPAGES
        } else {
            CoredumpFlags::PROVATE_HUGEPAGES
        })
    } else if shared {
        filter.contains(if file {
            CoredumpFlags::FILEBACKED_SHARED_MAPPINGS
        } else {
            CoredumpFlags::ANONYMOUS_SHARED_MAPPINGS
        })
    } else if !file || map.perms.contains(MMPermissions::WRITE) {
        filter.contains(CoredumpFlags::ANONYMOUS_PRIVATE_MAPPINGS)
            || (file && filter.contains(CoredumpFlags::FILEBACKED_PRIVATE_MAPPINGS))
    } else {
        filter.contains(CoredumpFlags::FILEBACKED_PRIVATE_MAPPINGS)
    };
    Dump {
        whole,
        elf_header: file && map.offset == 0 && filter.contains(CoredumpFlags::ELF_HEADERS),
    }
}

/// The thread's `/proc/<tid>/coredump_filter`, or the kernel default (0x33:
/// anonymous private and shared, ELF headers, private hugetlb) when it cannot
/// be read. Unknown bits are ignored.
fn coredump_filter(tid: i32) -> CoredumpFlags {
    fs::read_to_string(format!("/proc/{tid}/coredump_filter"))
        .ok()
        .and_then(|text| u32::from_str_radix(text.trim(), 16).ok())
        .map_or(
            CoredumpFlags::from_bits_truncate(0x33),
            CoredumpFlags::from_bits_truncate,
        )
}

/// Whether a mapping can appear in a core: procfs cannot read the vsyscall
/// page or the vvar pages, and an unreadable mapping has nothing to read.
fn dumpable(map: &MemoryMap) -> bool {
    map.perms.contains(MMPermissions::READ)
        && !matches!(map.pathname, MMapPath::Vvar | MMapPath::Vsyscall)
        && !matches!(&map.pathname, MMapPath::Other(name) if name.starts_with("vvar"))
}

fn segment_flags(perms: MMPermissions) -> u32 {
    let mut flags = 0;
    if perms.contains(MMPermissions::EXECUTE) {
        flags |= PF_X;
    }
    if perms.contains(MMPermissions::WRITE) {
        flags |= PF_W;
    }
    if perms.contains(MMPermissions::READ) {
        flags |= PF_R;
    }
    flags
}

/// One `PT_LOAD`: `filesz` bytes from `vaddr` are stored, the rest of
/// `memsz` is described but not stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Segment {
    vaddr: u64,
    memsz: u64,
    filesz: u64,
    flags: u32,
}

const PT_LOAD: u32 = 1;
const PT_NOTE: u32 = 4;
const PF_X: u32 = 1;
const PF_W: u32 = 2;
const PF_R: u32 = 4;
const NT_PRSTATUS: u32 = 1;
const NT_PRPSINFO: u32 = 3;
const NT_AUXV: u32 = 6;
const NT_FILE: u32 = 0x4649_4c45;
const EHDR_SIZE: u64 = 64;
const PHDR_SIZE: u64 = 56;

/// The fields of the notes that do not come from registers.
struct Identity {
    tid: i32,
    tgid: i32,
    ppid: i32,
    pgrp: i32,
    sid: i32,
    uid: u32,
    gid: u32,
    sigpend: u64,
    sighold: u64,
    state: char,
    nice: i64,
    flags: u32,
    comm: String,
    cmdline: String,
}

fn note(out: &mut Vec<u8>, kind: u32, desc: &[u8]) {
    const NAME: &[u8] = b"CORE\0";
    out.extend_from_slice(&(NAME.len() as u32).to_le_bytes());
    out.extend_from_slice(&(desc.len() as u32).to_le_bytes());
    out.extend_from_slice(&kind.to_le_bytes());
    out.extend_from_slice(NAME);
    pad4(out);
    out.extend_from_slice(desc);
    pad4(out);
}

fn pad4(out: &mut Vec<u8>) {
    out.resize(out.len().next_multiple_of(4), 0);
}

fn put(out: &mut [u8], offset: usize, bytes: &[u8]) {
    out[offset..offset + bytes.len()].copy_from_slice(bytes);
}

/// `struct elf_prpsinfo` for a 64-bit process (136 bytes).
fn prpsinfo(id: &Identity) -> Vec<u8> {
    let mut out = vec![0u8; 136];
    let state = id.state as u8;
    out[0] = b"RSDTZW".iter().position(|&c| c == state).unwrap_or(0) as u8;
    out[1] = state;
    out[2] = u8::from(state == b'Z');
    out[3] = id.nice as i8 as u8;
    put(&mut out, 8, &u64::from(id.flags).to_le_bytes());
    put(&mut out, 16, &id.uid.to_le_bytes());
    put(&mut out, 20, &id.gid.to_le_bytes());
    put(&mut out, 24, &id.tgid.to_le_bytes());
    put(&mut out, 28, &id.ppid.to_le_bytes());
    put(&mut out, 32, &id.pgrp.to_le_bytes());
    put(&mut out, 36, &id.sid.to_le_bytes());
    let comm = id.comm.as_bytes();
    put(&mut out, 40, &comm[..comm.len().min(15)]);
    let args = id.cmdline.as_bytes();
    put(&mut out, 56, &args[..args.len().min(79)]);
    out
}

/// `NT_FILE`: the file-backed mappings, so a debugger can find the files.
fn nt_file(maps: &[MemoryMap]) -> Vec<u8> {
    let files: Vec<(&MemoryMap, &Path)> = maps
        .iter()
        .filter_map(|map| match &map.pathname {
            MMapPath::Path(path) => Some((map, path.as_path())),
            _ => None,
        })
        .collect();
    let mut out = Vec::new();
    out.extend_from_slice(&(files.len() as u64).to_le_bytes());
    out.extend_from_slice(&PAGE.to_le_bytes());
    for (map, _) in &files {
        out.extend_from_slice(&map.address.0.to_le_bytes());
        out.extend_from_slice(&map.address.1.to_le_bytes());
        out.extend_from_slice(&(map.offset / PAGE).to_le_bytes());
    }
    for (_, path) in &files {
        use std::os::unix::ffi::OsStrExt;
        out.extend_from_slice(path.as_os_str().as_bytes());
        out.push(0);
    }
    out
}

#[cfg(target_arch = "x86_64")]
mod arch {
    use super::Identity;
    use super::put;

    pub(super) const MACHINE: u16 = 62; // EM_X86_64

    pub(super) fn stack_pointer(regs: &libc::user_regs_struct) -> u64 {
        regs.rsp
    }

    /// `struct elf_prstatus` for x86-64 (336 bytes).
    pub(super) fn prstatus(signal: i32, id: &Identity, regs: &libc::user_regs_struct) -> Vec<u8> {
        const REGS: usize = std::mem::size_of::<libc::user_regs_struct>();
        const _: () = assert!(REGS == 216);
        let mut out = vec![0u8; 336];
        put(&mut out, 0, &signal.to_le_bytes()); // pr_info.si_signo
        put(&mut out, 12, &(signal as i16).to_le_bytes()); // pr_cursig
        put(&mut out, 16, &id.sigpend.to_le_bytes());
        put(&mut out, 24, &id.sighold.to_le_bytes());
        put(&mut out, 32, &id.tid.to_le_bytes());
        put(&mut out, 36, &id.ppid.to_le_bytes());
        put(&mut out, 40, &id.pgrp.to_le_bytes());
        put(&mut out, 44, &id.sid.to_le_bytes());
        // SAFETY: user_regs_struct is plain old data of exactly REGS bytes.
        let bytes = unsafe { std::slice::from_raw_parts(regs as *const _ as *const u8, REGS) };
        put(&mut out, 112, bytes);
        out
    }
}

#[cfg(not(target_arch = "x86_64"))]
mod arch {
    use super::Identity;

    pub(super) const MACHINE: u16 = 0;

    pub(super) fn stack_pointer(regs: &libc::user_regs_struct) -> u64 {
        regs.sp
    }

    pub(super) fn prstatus(_: i32, _: &Identity, _: &libc::user_regs_struct) -> Vec<u8> {
        Vec::new()
    }
}

#[derive(Debug)]
enum WriteError {
    OverBudget,
    Deadline,
    Io(std::io::Error),
}

impl From<std::io::Error> for WriteError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

/// Compresses what is pushed into zstd frames of [`CHUNK`] bytes and appends
/// them to a file, refusing any frame that would pass `budget` or start after
/// `deadline`.
struct Compressed {
    file: File,
    buf: Vec<u8>,
    written: u64,
    budget: u64,
    deadline: Instant,
}

impl Compressed {
    fn push(&mut self, mut data: &[u8]) -> Result<(), WriteError> {
        while !data.is_empty() {
            let take = data.len().min(CHUNK - self.buf.len());
            self.buf.extend_from_slice(&data[..take]);
            data = &data[take..];
            if self.buf.len() == CHUNK {
                self.flush()?;
            }
        }
        Ok(())
    }

    fn push_zeros(&mut self, mut len: u64) -> Result<(), WriteError> {
        while len > 0 {
            let take = len.min(CHUNK as u64) as usize;
            self.push(&ZEROS[..take])?;
            len -= take as u64;
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<(), WriteError> {
        if self.buf.is_empty() {
            return Ok(());
        }
        if Instant::now() >= self.deadline {
            return Err(WriteError::Deadline);
        }
        let frame = ruzstd::encoding::compress_to_vec(&self.buf[..], CompressionLevel::Fastest);
        self.buf.clear();
        if self.written + frame.len() as u64 > self.budget {
            return Err(WriteError::OverBudget);
        }
        self.file.write_all(&frame)?;
        self.written += frame.len() as u64;
        Ok(())
    }
}

/// The ELF header and program headers for `segments`, with their stored
/// bytes laid out page-aligned after `notes_len` bytes of notes.
fn headers(notes_len: u64, segments: &[Segment]) -> (Vec<u8>, u64) {
    let phnum = 1 + segments.len() as u64;
    let notes_offset = EHDR_SIZE + PHDR_SIZE * phnum;
    let data_offset = (notes_offset + notes_len).next_multiple_of(PAGE);
    let mut out = Vec::with_capacity(notes_offset as usize);
    out.extend_from_slice(b"\x7fELF\x02\x01\x01");
    out.resize(16, 0);
    out.extend_from_slice(&4u16.to_le_bytes()); // e_type: ET_CORE
    out.extend_from_slice(&arch::MACHINE.to_le_bytes());
    out.extend_from_slice(&1u32.to_le_bytes()); // e_version
    out.extend_from_slice(&0u64.to_le_bytes()); // e_entry
    out.extend_from_slice(&EHDR_SIZE.to_le_bytes()); // e_phoff
    out.extend_from_slice(&0u64.to_le_bytes()); // e_shoff
    out.extend_from_slice(&0u32.to_le_bytes()); // e_flags
    out.extend_from_slice(&(EHDR_SIZE as u16).to_le_bytes());
    out.extend_from_slice(&(PHDR_SIZE as u16).to_le_bytes());
    out.extend_from_slice(&(phnum as u16).to_le_bytes());
    out.extend_from_slice(&[0; 6]); // e_shentsize, e_shnum, e_shstrndx
    let mut phdr =
        |kind: u32, flags: u32, offset: u64, vaddr: u64, filesz: u64, memsz: u64, align: u64| {
            out.extend_from_slice(&kind.to_le_bytes());
            out.extend_from_slice(&flags.to_le_bytes());
            out.extend_from_slice(&offset.to_le_bytes());
            out.extend_from_slice(&vaddr.to_le_bytes());
            out.extend_from_slice(&0u64.to_le_bytes()); // p_paddr
            out.extend_from_slice(&filesz.to_le_bytes());
            out.extend_from_slice(&memsz.to_le_bytes());
            out.extend_from_slice(&align.to_le_bytes());
        };
    phdr(PT_NOTE, 0, notes_offset, 0, notes_len, 0, 4);
    let mut offset = data_offset;
    for segment in segments {
        phdr(
            PT_LOAD,
            segment.flags,
            offset,
            segment.vaddr,
            segment.filesz,
            segment.memsz,
            PAGE,
        );
        offset += segment.filesz.next_multiple_of(PAGE);
    }
    (out, data_offset)
}

fn write_core(
    temp: &Path,
    budget: u64,
    deadline: Instant,
    snapshot: &Snapshot,
    segments: &[Segment],
    mem: Option<&File>,
) -> Result<u64, WriteError> {
    let _ = fs::remove_file(temp);
    let file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(temp)?;
    let mut out = Compressed {
        file,
        buf: Vec::with_capacity(CHUNK),
        written: 0,
        budget,
        deadline,
    };
    let (headers, data_offset) = headers(snapshot.notes.len() as u64, segments);
    out.push(&headers)?;
    out.push(&snapshot.notes)?;
    out.push_zeros(data_offset - headers.len() as u64 - snapshot.notes.len() as u64)?;
    let mut page = vec![0u8; CHUNK];
    for segment in segments {
        match mem {
            Some(mem) => copy_memory(&mut out, mem, segment.vaddr, segment.filesz, &mut page)?,
            None => out.push_zeros(segment.filesz)?,
        }
        out.push_zeros(segment.filesz.next_multiple_of(PAGE) - segment.filesz)?;
    }
    out.flush()?;
    Ok(out.written)
}

/// Pushes `len` bytes of guest memory from `addr`; a page procfs cannot read
/// is stored as zeros.
fn copy_memory(
    out: &mut Compressed,
    mem: &File,
    mut addr: u64,
    len: u64,
    buf: &mut [u8],
) -> Result<(), WriteError> {
    let end = addr + len;
    while addr < end {
        let want = (end - addr).min(buf.len() as u64) as usize;
        match read_fully(mem, &mut buf[..want], addr) {
            Ok(()) => out.push(&buf[..want])?,
            Err(_) => {
                for page in buf[..want].chunks_mut(PAGE as usize) {
                    let page_addr = addr;
                    addr += page.len() as u64;
                    if read_fully(mem, page, page_addr).is_err() {
                        page.fill(0);
                    }
                }
                out.push(&buf[..want])?;
                continue;
            }
        }
        addr += want as u64;
    }
    Ok(())
}

fn read_fully(mem: &File, buf: &mut [u8], addr: u64) -> std::io::Result<()> {
    let mut done = 0;
    while done < buf.len() {
        match mem.read_at(&mut buf[done..], addr + done as u64)? {
            0 => return Err(std::io::ErrorKind::UnexpectedEof.into()),
            n => done += n,
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The number of names in `dir`, read with std as an independent oracle.
    #[allow(clippy::disallowed_methods)]
    fn entries(dir: &std::path::Path) -> usize {
        fs::read_dir(dir).unwrap().count()
    }

    #[test]
    fn headers_lay_out_notes_then_page_aligned_segments() {
        let segments = [
            Segment {
                vaddr: 0x1000,
                memsz: 0x3000,
                filesz: 0x3000,
                flags: PF_R | PF_W,
            },
            Segment {
                vaddr: 0x10000,
                memsz: 0x2000,
                filesz: 0,
                flags: PF_R,
            },
            Segment {
                vaddr: 0x20000,
                memsz: 0x5000,
                filesz: 0x1000,
                flags: PF_R | PF_X,
            },
        ];
        let (bytes, data_offset) = headers(100, &segments);
        assert_eq!(bytes.len() as u64, EHDR_SIZE + 4 * PHDR_SIZE);
        assert_eq!(data_offset, PAGE);
        assert_eq!(&bytes[..4], b"\x7fELF");
        let u16_at = |at: usize| u16::from_le_bytes(bytes[at..at + 2].try_into().unwrap());
        assert_eq!(u16_at(16), 4, "e_type is ET_CORE");
        assert_eq!(u16_at(56), 4, "e_phnum");
        let u64_at = |at: usize| u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap());
        let phdr = |i: u64| (EHDR_SIZE + i * PHDR_SIZE) as usize;
        // PT_NOTE right after the program headers.
        assert_eq!(u64_at(phdr(0) + 8), EHDR_SIZE + 4 * PHDR_SIZE);
        assert_eq!(u64_at(phdr(0) + 32), 100);
        // Stored segments follow each other at page granularity; an empty
        // one takes no file space.
        assert_eq!(u64_at(phdr(1) + 8), PAGE);
        assert_eq!(u64_at(phdr(2) + 8), PAGE + 0x3000);
        assert_eq!(u64_at(phdr(3) + 8), PAGE + 0x3000);
        assert_eq!(u64_at(phdr(3) + 32), 0x1000);
        assert_eq!(u64_at(phdr(3) + 40), 0x5000);
    }

    #[test]
    fn notes_are_four_byte_aligned() {
        let mut notes = Vec::new();
        note(&mut notes, NT_AUXV, &[1, 2, 3]);
        assert_eq!(notes.len(), 12 + 8 + 4);
        assert_eq!(&notes[12..17], b"CORE\0");
        assert_eq!(&notes[20..23], &[1, 2, 3]);
    }

    fn compressed(dir: &Path, budget: u64, deadline: Instant) -> Compressed {
        Compressed {
            file: File::create(dir.join("out")).unwrap(),
            buf: Vec::new(),
            written: 0,
            budget,
            deadline,
        }
    }

    #[test]
    fn frames_decode_back_to_the_input() {
        let dir = tempfile::tempdir().unwrap();
        let mut out = compressed(
            dir.path(),
            u64::MAX,
            Instant::now() + Duration::from_secs(60),
        );
        let input: Vec<u8> = (0..(CHUNK * 2 + 77)).map(|i| (i % 251) as u8).collect();
        out.push(&input).unwrap();
        out.flush().unwrap();
        let stored = fs::read(dir.path().join("out")).unwrap();
        assert_eq!(stored.len() as u64, out.written);
        // One frame per chunk: decode them all.
        let mut decoded = Vec::with_capacity(input.len());
        ruzstd::decoding::FrameDecoder::new()
            .decode_all_to_vec(&stored, &mut decoded)
            .unwrap();
        assert_eq!(decoded, input);
    }

    #[test]
    fn a_frame_past_the_budget_is_refused_and_not_written() {
        let dir = tempfile::tempdir().unwrap();
        let mut out = compressed(dir.path(), 64, Instant::now() + Duration::from_secs(60));
        let noise: Vec<u8> = (0..4096u32)
            .map(|i| (i.wrapping_mul(2654435761) >> 13) as u8)
            .collect();
        out.push(&noise).unwrap();
        assert!(matches!(out.flush(), Err(WriteError::OverBudget)));
        assert_eq!(fs::metadata(dir.path().join("out")).unwrap().len(), 0);
    }

    #[test]
    fn a_frame_after_the_deadline_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let mut out = compressed(dir.path(), u64::MAX, Instant::now());
        out.push(b"late").unwrap();
        assert!(matches!(out.flush(), Err(WriteError::Deadline)));
    }

    #[test]
    fn the_capture_ends_at_the_earlier_of_its_time_limit_and_the_cleanup_deadline() {
        let start = Instant::now();
        let limit = Duration::from_secs(10);
        assert_eq!(capture_end(start, limit, None), start + limit);
        let soon = start + Duration::from_secs(2);
        assert_eq!(capture_end(start, limit, Some(soon)), soon);
        let late = start + Duration::from_secs(60);
        assert_eq!(capture_end(start, limit, Some(late)), start + limit);
    }

    fn config(dir: &Path) -> FatalCoreCapture {
        FatalCoreCapture {
            dir: dir.to_path_buf(),
            file_prefix: "test-".to_string(),
            max_core_bytes: 64 << 20,
            max_total_bytes: 64 << 20,
            time_limit: Duration::from_secs(10),
        }
    }

    /// An exit for this test's own thread, which procfs can describe and read
    /// like a stopped guest's.
    fn own_exit(dumping: bool, deadline: Option<Instant>) -> FatalSignalExit {
        // SAFETY: gettid has no preconditions.
        let tid = reverie::Pid::from_raw(unsafe { libc::gettid() });
        // SAFETY: user_regs_struct is plain old data; zero is a valid value.
        let regs: libc::user_regs_struct = unsafe { std::mem::zeroed() };
        let status = reverie::ExitStatus::Signaled(reverie::Signal::SIGSEGV, false);
        FatalSignalExit::new(tid, status, dumping, deadline, regs).unwrap()
    }

    /// The decompressed core at `path` and the bytes of memory its `PT_LOAD`
    /// segments store.
    fn stored_memory(path: &Path) -> u64 {
        use std::io::Read;
        let stored = fs::read(path).unwrap();
        let mut rest = stored.as_slice();
        let mut core = Vec::new();
        while !rest.is_empty() {
            ruzstd::decoding::StreamingDecoder::new(&mut rest)
                .unwrap()
                .read_to_end(&mut core)
                .unwrap();
        }
        let u16_at = |at: usize| u16::from_le_bytes(core[at..at + 2].try_into().unwrap());
        let u32_at = |at: usize| u32::from_le_bytes(core[at..at + 4].try_into().unwrap());
        let u64_at = |at: usize| u64::from_le_bytes(core[at..at + 8].try_into().unwrap());
        assert_eq!(u16_at(16), 4, "not an ET_CORE file");
        (0..u64::from(u16_at(56)))
            .map(|i| (EHDR_SIZE + i * PHDR_SIZE) as usize)
            .filter(|&phdr| u32_at(phdr) == PT_LOAD)
            .map(|phdr| u64_at(phdr + 32))
            .sum()
    }

    fn map(pathname: MMapPath, perms: MMPermissions, offset: u64) -> MemoryMap {
        MemoryMap {
            address: (0x1000, 0x3000),
            perms,
            offset,
            dev: (0, 0),
            inode: u64::from(matches!(pathname, MMapPath::Path(_))),
            pathname,
            extension: Default::default(),
        }
    }

    #[test]
    fn the_dump_rule_keeps_what_the_kernel_would() {
        use MMPermissions as P;
        let default = CoredumpFlags::from_bits_truncate(0x33);
        let rd = VmFlags::RD | VmFlags::MR;
        let rw = rd | VmFlags::WR | VmFlags::MW;
        let lib = || MMapPath::Path("/lib/libc.so.6".into());
        let whole = |m: &MemoryMap, flags, filter| dump_rule(m, Some(flags), filter).whole;
        let heap = map(MMapPath::Heap, P::READ | P::WRITE | P::PRIVATE, 0);
        assert!(whole(&heap, rw, default), "anonymous private memory");
        assert!(!whole(&heap, rw | VmFlags::DD, default), "MADV_DONTDUMP");
        assert!(!whole(&heap, rw | VmFlags::IO, default), "an I/O mapping");
        assert!(
            !whole(&heap, rw | VmFlags::PF, default),
            "a raw-PFN mapping"
        );
        assert!(!whole(&heap, rw, CoredumpFlags::empty()), "filter 0");
        assert_eq!(
            dump_rule(&heap, None, default),
            dump_rule(&heap, Some(rw | VmFlags::DD), default)
        );
        let shared_file = map(lib(), P::READ | P::WRITE | P::SHARED, 0);
        assert!(
            !whole(&shared_file, rw | VmFlags::SH, default),
            "shared file, default"
        );
        assert!(whole(
            &shared_file,
            rw | VmFlags::SH,
            default | CoredumpFlags::FILEBACKED_SHARED_MAPPINGS
        ));
        let data = map(lib(), P::READ | P::WRITE | P::PRIVATE, 0x2000);
        assert!(whole(&data, rw, default), "a written private file mapping");
        let text = map(lib(), P::READ | P::EXECUTE | P::PRIVATE, 0);
        let rule = dump_rule(&text, Some(rd | VmFlags::EX), default);
        assert!(!rule.whole && rule.elf_header, "{rule:?}");
        let no_headers = default - CoredumpFlags::ELF_HEADERS;
        assert!(!dump_rule(&text, Some(rd | VmFlags::EX), no_headers).elf_header);
        assert!(!dump_rule(&text, Some(rd | VmFlags::DD), default).elf_header);
    }

    #[test]
    fn the_lock_wait_gives_up_at_the_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let held = DirLock::acquire(dir.path(), Instant::now()).unwrap();
        let start = Instant::now();
        let error = DirLock::acquire(dir.path(), start + Duration::from_millis(50))
            .err()
            .expect("a second writer took a held lock");
        assert!(error.to_string().contains("timed out"), "{error:#}");
        assert!(start.elapsed() >= Duration::from_millis(50));
        drop(held);
        DirLock::acquire(dir.path(), Instant::now()).unwrap();
    }

    #[test]
    fn a_repeated_process_id_gets_a_new_name_instead_of_being_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config(dir.path());
        let exit = own_exit(true, None);
        let fresh = AtomicBool::new(false);
        let mut paths = Vec::new();
        for _ in 0..2 {
            let Outcome::Written { path } = capture_from(&cfg, &exit, None, &fresh).unwrap() else {
                panic!("the second core of a repeated process ID was not kept");
            };
            paths.push(path);
        }
        assert_ne!(paths[0], paths[1]);
        assert!(paths.iter().all(|path| path.exists()));
        let name = paths[1].file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.ends_with(".SIGSEGV.1.zst"), "{name}");
    }

    #[test]
    fn after_a_full_core_runs_out_of_time_later_cores_start_smaller() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config(dir.path());
        let mem = File::open("/proc/self/mem").unwrap();
        let flag = AtomicBool::new(false);
        let late = own_exit(true, Some(Instant::now()));
        capture_from(&cfg, &late, Some(&mem), &flag).unwrap();
        assert!(
            flag.load(Ordering::Relaxed),
            "a timed-out Full core was not remembered"
        );
        let full = AtomicBool::new(false);
        let Outcome::Written { path: big } =
            capture_from(&cfg, &own_exit(true, None), Some(&mem), &full).unwrap()
        else {
            panic!("no Full core");
        };
        let Outcome::Written { path: small } =
            capture_from(&cfg, &own_exit(true, None), Some(&mem), &flag).unwrap()
        else {
            panic!("no core after the timeout");
        };
        assert!(
            stored_memory(&small) < stored_memory(&big),
            "the core after a timeout was not smaller"
        );
    }

    #[test]
    fn a_thread_that_did_not_dump_gets_no_core() {
        let dir = tempfile::tempdir().unwrap();
        let cores = dir.path().join("cores");
        let outcome = capture(&config(&cores), &own_exit(false, None)).unwrap();
        assert_eq!(outcome, Outcome::NotDumping);
        assert!(
            !cores.exists(),
            "a non-dumping thread created the directory"
        );
    }

    #[test]
    fn unreadable_memory_still_keeps_the_notes() {
        let dir = tempfile::tempdir().unwrap();
        let Outcome::Written { path } = capture_from(
            &config(dir.path()),
            &own_exit(true, None),
            None,
            &AtomicBool::new(false),
        )
        .unwrap() else {
            panic!("no core was kept without memory");
        };
        assert_eq!(
            stored_memory(&path),
            0,
            "memory was stored without a mem file"
        );
    }

    #[test]
    fn readable_memory_is_stored() {
        let dir = tempfile::tempdir().unwrap();
        let Outcome::Written { path } =
            capture(&config(dir.path()), &own_exit(true, None)).unwrap()
        else {
            panic!("no core was kept");
        };
        assert!(stored_memory(&path) > 0, "the core stores no memory");
    }

    #[test]
    fn a_passed_cleanup_deadline_keeps_nothing() {
        let dir = tempfile::tempdir().unwrap();
        // A local flag, so this timeout leaves the process-wide one alone.
        let mem = File::open("/proc/self/mem").unwrap();
        let flag = AtomicBool::new(false);
        let late = own_exit(true, Some(Instant::now()));
        let outcome = capture_from(&config(dir.path()), &late, Some(&mem), &flag).unwrap();
        assert!(matches!(outcome, Outcome::NoRoom { .. }), "{outcome:?}");
        assert_eq!(entries(dir.path()), 0);
    }

    #[test]
    fn a_dead_writers_temp_file_stops_counting_against_the_total() {
        let dir = tempfile::tempdir().unwrap();
        let orphan = dir
            .path()
            .join(".hermit-1-0000000000000000-core.3.3.SIGSEGV.zst.tmp");
        let unrelated = dir.path().join(".someone-else.tmp");
        let not_a_core = dir.path().join(".hermit-notes.tmp");
        let young = dir
            .path()
            .join(".hermit-2-0000000000000000-core.4.4.SIGSEGV.zst.tmp");
        fs::write(&orphan, vec![0u8; 1 << 20]).unwrap();
        for path in [&unrelated, &not_a_core, &young] {
            fs::write(path, b"x").unwrap();
        }
        // Written longer ago than twice the time limit, so certainly dead.
        let hour_ago = SystemTime::now() - Duration::from_secs(3600);
        for path in [&orphan, &unrelated, &not_a_core] {
            File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(hour_ago)
                .unwrap();
        }
        let cfg = FatalCoreCapture {
            max_total_bytes: 1 << 20,
            ..config(dir.path())
        };
        let outcome = capture_from(&cfg, &own_exit(true, None), None, &AtomicBool::new(false));
        assert!(
            matches!(outcome, Ok(Outcome::Written { .. })),
            "the orphan still took the whole total: {outcome:?}"
        );
        assert!(!orphan.exists(), "the orphaned temp file was kept");
        assert!(
            unrelated.exists(),
            "a file that is not hermit's was removed"
        );
        assert!(
            not_a_core.exists(),
            "a hermit file that is not a core temp was removed"
        );
        assert!(young.exists(), "a temp written just now was removed");
    }

    #[test]
    fn the_tracer_stops_waiting_at_the_end_and_the_helper_then_keeps_nothing() {
        let (tell, told) = std::sync::mpsc::channel();
        let start = Instant::now();
        let outcome = run_within(start + Duration::from_millis(100), move |abandoned| {
            // Stands in for a read blocked in the kernel.
            std::thread::sleep(Duration::from_millis(600));
            tell.send(abandoned.load(Ordering::SeqCst)).unwrap();
            Ok(Outcome::NotDumping)
        });
        let waited = start.elapsed();
        let error = outcome.expect_err("a blocked capture was reported as done");
        assert!(error.to_string().contains("gave up"), "{error:#}");
        assert!(
            waited < Duration::from_millis(500),
            "the tracer waited {waited:?}"
        );
        assert!(
            told.recv_timeout(Duration::from_secs(5)).unwrap(),
            "the helper was not told it was abandoned"
        );
    }

    #[test]
    fn a_capture_that_finishes_in_time_is_returned_and_a_panic_is_an_error() {
        let done = run_within(Instant::now() + Duration::from_secs(5), |_| {
            Ok(Outcome::NotDumping)
        });
        assert_eq!(done.unwrap(), Outcome::NotDumping);
        let panicked = run_within(Instant::now() + Duration::from_secs(5), |_| panic!("boom"));
        assert!(panicked.unwrap_err().to_string().contains("panicked"));
    }

    #[test]
    fn an_abandoned_capture_deletes_its_core_instead_of_keeping_it() {
        let dir = tempfile::tempdir().unwrap();
        let abandoned = AtomicBool::new(true);
        let outcome = capture_abandonable(
            &config(dir.path()),
            &own_exit(true, None),
            None,
            &AtomicBool::new(false),
            &abandoned,
        );
        assert!(outcome.is_err(), "{outcome:?}");
        assert_eq!(entries(dir.path()), 0, "an abandoned capture left a file");
    }

    #[test]
    fn regular_file_bytes_counts_only_regular_files() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a"), [0u8; 10]).unwrap();
        fs::write(dir.path().join("b"), [0u8; 5]).unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();
        fs::write(dir.path().join("sub/c"), [0u8; 100]).unwrap();
        assert_eq!(regular_file_bytes(dir.path()).unwrap(), 15);
    }
}
