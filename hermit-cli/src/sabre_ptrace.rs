/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Ptrace safety net for syscall instructions missed by SaBRe rewriting.

use std::collections::HashMap;
use std::collections::HashSet;
use std::fs;
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::ExitStatus;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use anyhow::Context;
use anyhow::Error;
use anyhow::anyhow;
use nix::sys::ptrace;
use nix::sys::signal::Signal;
use nix::sys::wait::WaitPidFlag;
use nix::sys::wait::WaitStatus;
use nix::sys::wait::waitpid;
use nix::unistd::Pid;
use serde::Serialize;

const SYSCALL_INSN: [u8; 2] = [0x0f, 0x05];
// SaBRe's SIGILL handler recognizes this reserved two-byte instruction as a
// syscall site that could not be expanded to an out-of-line jump.
const SABRE_SYSCALL_MARKER: [u8; 2] = [0x0f, 0xff];

#[derive(Debug)]
pub struct Output {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub path_evidence: PathEvidence,
}

#[derive(Debug, Serialize)]
pub struct PathEvidence {
    pub schema: u8,
    pub guest_rpc_observed: bool,
    pub ptrace_fallback_sites: usize,
    pub trusted_shared_object_sites: usize,
    pub trusted_shared_objects: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct MappingClassification {
    trusted: bool,
    trusted_shared_object: Option<PathBuf>,
}

#[derive(Default)]
struct TraceeState {
    pending_patch: Option<PendingPatch>,
}

struct PendingPatch {
    site: usize,
    syscall: u64,
}

struct SignalDiagnostic {
    signal: Signal,
    si_code: Option<i32>,
    si_errno: Option<i32>,
    fault_address: Option<usize>,
    mapping: Option<MappingDiagnostic>,
    instruction_bytes: Option<[u8; 16]>,
    registers: libc::user_regs_struct,
}

struct MappingDiagnostic {
    line: String,
    relative_offset: usize,
    file_offset: usize,
}

fn should_replace_signal_diagnostic(
    existing: Option<(Signal, Option<i32>, bool)>,
    signal: Signal,
    si_code: Option<i32>,
) -> bool {
    // SaBRe handles its reserved SIGILL markers in-process. For an unknown hardware SIGILL it
    // restores SIG_DFL and raises SIGILL again, producing a second ptrace stop from SI_TKILL.
    // Preserve the original kernel fault context across that re-raise, but let a later hardware
    // fault replace it so a successfully handled marker cannot leave stale diagnostics behind.
    if let Some((Signal::SIGILL, Some(existing_code), existing_is_marker)) = existing
        && signal == Signal::SIGILL
        && existing_code > 0
        && !existing_is_marker
        && !si_code.is_some_and(|code| code > 0)
    {
        return false;
    }
    true
}

fn is_sabre_sigill_marker(instruction_bytes: Option<&[u8; 16]>) -> bool {
    instruction_bytes
        .is_some_and(|bytes| matches!(&bytes[..2], [0x0f, 0xff] | [0x0f, 0x0b] | [0x0f, 0x0c]))
}

fn final_physical_exit(status: &WaitStatus) -> Option<(Pid, ExitStatus)> {
    match *status {
        WaitStatus::Exited(pid, code) => Some((pid, ExitStatus::from_raw(code << 8))),
        WaitStatus::Signaled(pid, signal, core_dumped) => {
            let raw = signal as i32 | if core_dumped { 0x80 } else { 0 };
            Some((pid, ExitStatus::from_raw(raw)))
        }
        _ => None,
    }
}

/// Identity of one address space (`mm`). Assigned by this supervisor rather
/// than read from the kernel, which exposes no stable mm identifier.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, PartialOrd, Ord)]
struct AddressSpaceId(u64);

/// Page-classification cache, keyed by ADDRESS SPACE rather than by tracee.
///
/// The classification of a page is a property of the mm that maps it, not of
/// the thread that happened to fault on it. Keying by `Pid` was unsound: under
/// `CLONE_VM` every thread in a group shares one mm, so a mutation observed on
/// thread A evicted only A's entries and a sibling B kept serving the
/// pre-mutation verdict for the same page -- letting a raw syscall at a page
/// that is no longer trusted bypass both redirection and accounting, which is
/// exactly the invisible-site class this evidence exists to close.
struct MappingCache {
    entries: HashMap<(AddressSpaceId, usize), MappingClassification>,
    /// Which address space each tracee currently runs in.
    spaces: HashMap<Pid, AddressSpaceId>,
    next: u64,
}

impl MappingCache {
    fn new(root: Pid) -> Self {
        Self {
            entries: HashMap::new(),
            spaces: HashMap::from([(root, AddressSpaceId(0))]),
            next: 1,
        }
    }

    fn fresh(&mut self) -> AddressSpaceId {
        let id = AddressSpaceId(self.next);
        self.next += 1;
        id
    }

    /// The address space `pid` runs in. An unknown tracee gets a fresh space:
    /// we have never cached anything for it, so it can hold nothing stale.
    fn space_of(&mut self, pid: Pid) -> AddressSpaceId {
        if let Some(id) = self.spaces.get(&pid) {
            return *id;
        }
        let id = self.fresh();
        self.spaces.insert(pid, id);
        id
    }

    fn get(&mut self, pid: Pid, page: usize) -> Option<&MappingClassification> {
        let space = self.space_of(pid);
        self.entries.get(&(space, page))
    }

    fn insert(&mut self, pid: Pid, page: usize, classification: MappingClassification) {
        let space = self.space_of(pid);
        self.entries.insert((space, page), classification);
    }

    /// Drop every cached page of the address space `pid` runs in -- including
    /// entries cached while a SIBLING thread was executing.
    fn invalidate_address_space(&mut self, pid: Pid) {
        let space = self.space_of(pid);
        self.entries.retain(|(cached, _), _| *cached != space);
    }

    /// `child` shares `parent`'s address space (CLONE_VM / vfork).
    fn share_address_space(&mut self, parent: Pid, child: Pid) {
        let space = self.space_of(parent);
        self.spaces.insert(child, space);
    }

    /// `child` gets its own address space (fork).
    fn new_address_space(&mut self, child: Pid) {
        let space = self.fresh();
        self.spaces.insert(child, space);
    }

    /// `pid` exec'd: the old mm is gone, so discard it and start a fresh one.
    fn replace_address_space(&mut self, pid: Pid) {
        self.invalidate_address_space(pid);
        let space = self.fresh();
        self.spaces.insert(pid, space);
    }

    /// `pid` exited. Its cached pages stay valid for any sibling still sharing
    /// the mm, so they are dropped only once the last user is gone.
    fn forget(&mut self, pid: Pid) {
        let Some(space) = self.spaces.remove(&pid) else {
            return;
        };
        if !self.spaces.values().any(|other| *other == space) {
            self.entries.retain(|(cached, _), _| *cached != space);
        }
    }

    /// Place a newly announced `child` in the right address space.
    ///
    /// `shares_mm` is an OBSERVATION of the child that now exists -- see
    /// `mm_sharing` -- not a prediction from the clone arguments. An earlier
    /// revision read `CLONE_VM` out of the tracee's `clone_args` at the clone3
    /// syscall-entry stop, which is before the kernel copies that buffer: a
    /// concurrently runnable sibling thread could rewrite it in the window and
    /// the recorded intent then disagreed with the child the kernel actually
    /// made, in both directions. Demonstrated with a tracer that rewrites the
    /// buffer after the entry read; `kcmp(KCMP_VM)` at this stop reported the
    /// opposite of the entry value each time.
    fn admit_child(&mut self, parent: Pid, child: Pid, shares_mm: MmSharing) {
        match shares_mm {
            MmSharing::Shared => self.share_address_space(parent, child),
            MmSharing::Private => self.new_address_space(child),
        }
    }
}

struct Supervisor {
    root: Pid,
    tracees: HashSet<Pid>,
    states: HashMap<Pid, TraceeState>,
    mapping_cache: MappingCache,
    /// Identity (inode + canonical path) of the launched SaBRe loader and
    /// plugin. Resolved
    /// once at construction so the exemption binds to the objects this
    /// supervisor actually started, not to whatever a mapping calls itself.
    sabre_id: Option<FileId>,
    plugin_id: Option<FileId>,
    readiness: Arc<AtomicBool>,
    ready_observed: bool,
    patched_sites: HashSet<(Pid, usize)>,
    trusted_shared_object_sites: HashSet<(Pid, usize)>,
    trusted_shared_objects: HashSet<PathBuf>,
    signal_diagnostics: HashMap<Pid, SignalDiagnostic>,
    physical_exit_observer: Arc<detcore::GlobalState>,
}

impl Supervisor {
    fn new(
        root: Pid,
        sabre: PathBuf,
        plugin: PathBuf,
        readiness: Arc<AtomicBool>,
        physical_exit_observer: Arc<detcore::GlobalState>,
    ) -> Self {
        Self {
            root,
            tracees: HashSet::from([root]),
            states: HashMap::from([(root, TraceeState::default())]),
            mapping_cache: MappingCache::new(root),
            sabre_id: launched_file_id(&sabre),
            plugin_id: launched_file_id(&plugin),
            readiness,
            ready_observed: false,
            patched_sites: HashSet::new(),
            trusted_shared_object_sites: HashSet::new(),
            trusted_shared_objects: HashSet::new(),
            signal_diagnostics: HashMap::new(),
            physical_exit_observer,
        }
    }

    fn run(mut self) -> Result<(ExitStatus, PathEvidence), Error> {
        ptrace::attach(self.root).context("failed to attach SaBRe supervisor worker")?;
        match waitpid(self.root, Some(WaitPidFlag::__WALL))? {
            WaitStatus::Stopped(pid, Signal::SIGSTOP) if pid == self.root => {}
            status => {
                return Err(anyhow!(
                    "unexpected SaBRe supervisor attach stop: {status:?}"
                ));
            }
        }
        tracing::trace!(
            target: "hermit::sabre::fallback",
            tid = self.root.as_raw(),
            "received supervisor attach stop",
        );
        self.set_options(self.root)
            .context("failed to set options on the initial SaBRe tracee")?;
        ptrace::syscall(self.root, None)
            .context("failed to resume the initial SaBRe tracee with PTRACE_SYSCALL")?;

        let mut root_status = None;
        while !self.tracees.is_empty() {
            let status = match waitpid(Pid::from_raw(-1), Some(WaitPidFlag::__WALL)) {
                Ok(status) => status,
                Err(nix::errno::Errno::EINTR) => continue,
                Err(error) => return Err(error.into()),
            };
            tracing::trace!(
                target: "hermit::sabre::fallback",
                ?status,
                "received ptrace wait status",
            );
            if let Some((pid, exit_status)) = final_physical_exit(&status) {
                if let WaitStatus::Signaled(_, signal, _) = status
                    && let Some(diagnostic) = self
                        .signal_diagnostics
                        .get(&pid)
                        .filter(|diagnostic| diagnostic.signal == signal)
                {
                    let instruction_bytes = diagnostic.instruction_bytes.as_ref().map(|bytes| {
                        bytes
                            .iter()
                            .map(|byte| format!("{byte:02x}"))
                            .collect::<Vec<_>>()
                            .join(" ")
                    });
                    tracing::error!(
                        target: "hermit::sabre::fallback",
                        tid = pid.as_raw(),
                        ?signal,
                        si_code = diagnostic.si_code,
                        si_errno = diagnostic.si_errno,
                        rip = format!("{:#x}", diagnostic.registers.rip),
                        rsp = format!("{:#x}", diagnostic.registers.rsp),
                        fault_address = diagnostic
                            .fault_address
                            .map(|address| format!("{address:#x}")),
                        mapping = diagnostic
                            .mapping
                            .as_ref()
                            .map(|mapping| mapping.line.as_str()),
                        file_offset = diagnostic
                            .mapping
                            .as_ref()
                            .map(|mapping| format!("{:#x}", mapping.file_offset)),
                        relative_offset = diagnostic
                            .mapping
                            .as_ref()
                            .map(|mapping| format!("{:#x}", mapping.relative_offset)),
                        instruction_bytes,
                        rax = format!("{:#x}", diagnostic.registers.rax),
                        rbx = format!("{:#x}", diagnostic.registers.rbx),
                        rcx = format!("{:#x}", diagnostic.registers.rcx),
                        rdx = format!("{:#x}", diagnostic.registers.rdx),
                        rsi = format!("{:#x}", diagnostic.registers.rsi),
                        rdi = format!("{:#x}", diagnostic.registers.rdi),
                        rbp = format!("{:#x}", diagnostic.registers.rbp),
                        r8 = format!("{:#x}", diagnostic.registers.r8),
                        r9 = format!("{:#x}", diagnostic.registers.r9),
                        r10 = format!("{:#x}", diagnostic.registers.r10),
                        r11 = format!("{:#x}", diagnostic.registers.r11),
                        r12 = format!("{:#x}", diagnostic.registers.r12),
                        r13 = format!("{:#x}", diagnostic.registers.r13),
                        r14 = format!("{:#x}", diagnostic.registers.r14),
                        r15 = format!("{:#x}", diagnostic.registers.r15),
                        orig_rax = format!("{:#x}", diagnostic.registers.orig_rax),
                        eflags = format!("{:#x}", diagnostic.registers.eflags),
                        cs = format!("{:#x}", diagnostic.registers.cs),
                        ss = format!("{:#x}", diagnostic.registers.ss),
                        fs_base = format!("{:#x}", diagnostic.registers.fs_base),
                        gs_base = format!("{:#x}", diagnostic.registers.gs_base),
                        "SaBRe tracee terminated by a fatal signal",
                    );
                }
                self.remove_tracee(pid);
                self.physical_exit_observer
                    .complete_physical_process_exit(pid.as_raw());
                if pid == self.root {
                    root_status = Some(exit_status);
                }
                continue;
            }
            match status {
                WaitStatus::PtraceSyscall(pid) => self.handle_syscall_stop(pid)?,
                WaitStatus::PtraceEvent(pid, _, event) => self.handle_ptrace_event(pid, event)?,
                WaitStatus::Stopped(pid, signal) => {
                    if signal == Signal::SIGSTOP && pid != self.root {
                        self.states.entry(pid).or_default();
                        self.tracees.insert(pid);
                        self.set_options(pid)?;
                        self.resume(pid, None)?;
                    } else {
                        if !matches!(signal, Signal::SIGSTOP | Signal::SIGCHLD) {
                            let registers = ptrace::getregs(pid).ok();
                            let rip = registers.as_ref().map_or(0, |registers| registers.rip);
                            let captures_fault_context = matches!(
                                signal,
                                Signal::SIGSEGV
                                    | Signal::SIGILL
                                    | Signal::SIGBUS
                                    | Signal::SIGFPE
                                    | Signal::SIGABRT
                            );
                            if captures_fault_context {
                                let siginfo = ptrace::getsiginfo(pid).ok();
                                let si_code = siginfo.as_ref().map(|info| info.si_code);
                                let existing =
                                    self.signal_diagnostics.get(&pid).map(|diagnostic| {
                                        (
                                            diagnostic.signal,
                                            diagnostic.si_code,
                                            is_sabre_sigill_marker(
                                                diagnostic.instruction_bytes.as_ref(),
                                            ),
                                        )
                                    });
                                if should_replace_signal_diagnostic(existing, signal, si_code) {
                                    if let Some(registers) = registers {
                                        let fault_address = siginfo
                                            .as_ref()
                                            .filter(|info| info.si_code > 0)
                                            .map(|info| unsafe { info.si_addr() as usize });
                                        let mapping = fs::read_to_string(format!(
                                            "/proc/{}/maps",
                                            pid.as_raw()
                                        ))
                                        .ok()
                                        .and_then(|maps| mapping_diagnostic(&maps, rip as usize));
                                        let instruction_bytes =
                                            read_diagnostic_bytes(pid, rip as usize).ok();
                                        self.signal_diagnostics.insert(
                                            pid,
                                            SignalDiagnostic {
                                                signal,
                                                si_code,
                                                si_errno: siginfo
                                                    .as_ref()
                                                    .map(|info| info.si_errno),
                                                fault_address,
                                                mapping,
                                                instruction_bytes,
                                                registers,
                                            },
                                        );
                                    } else {
                                        self.signal_diagnostics.remove(&pid);
                                    }
                                }
                            }
                            tracing::debug!(
                                target: "hermit::sabre::fallback",
                                tid = pid.as_raw(),
                                ?signal,
                                rip = format!("{rip:#x}"),
                                "forwarding signal to tracee",
                            );
                        }
                        self.resume(pid, Some(signal))?;
                    }
                }
                WaitStatus::Exited(..) | WaitStatus::Signaled(..) => unreachable!(),
                WaitStatus::Continued(_) | WaitStatus::StillAlive => {}
            }
        }

        let status = root_status.ok_or_else(|| anyhow!("SaBRe root tracee disappeared"))?;
        let mut trusted_shared_objects = self
            .trusted_shared_objects
            .into_iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>();
        trusted_shared_objects.sort();
        Ok((
            status,
            PathEvidence {
                schema: 1,
                guest_rpc_observed: self.readiness.load(Ordering::Acquire),
                ptrace_fallback_sites: self.patched_sites.len(),
                trusted_shared_object_sites: self.trusted_shared_object_sites.len(),
                trusted_shared_objects,
            },
        ))
    }

    fn set_options(&self, pid: Pid) -> Result<(), Error> {
        ptrace::setoptions(
            pid,
            ptrace::Options::PTRACE_O_EXITKILL
                | ptrace::Options::PTRACE_O_TRACESYSGOOD
                | ptrace::Options::PTRACE_O_TRACECLONE
                | ptrace::Options::PTRACE_O_TRACEFORK
                | ptrace::Options::PTRACE_O_TRACEVFORK
                | ptrace::Options::PTRACE_O_TRACEEXEC
                | ptrace::Options::PTRACE_O_TRACEEXIT,
        )?;
        Ok(())
    }

    fn handle_syscall_stop(&mut self, pid: Pid) -> Result<(), Error> {
        let syscall_info = get_syscall_info(pid)?;
        tracing::trace!(
            target: "hermit::sabre::fallback",
            tid = pid.as_raw(),
            op = syscall_info.op,
            "decoded ptrace syscall stop",
        );
        match syscall_info.op {
            libc::PTRACE_SYSCALL_INFO_ENTRY => {
                let mut regs = ptrace::getregs(pid)?;
                let site = regs
                    .rip
                    .checked_sub(SYSCALL_INSN.len() as u64)
                    .ok_or_else(|| anyhow!("invalid syscall RIP {:#x} in tracee {pid}", regs.rip))?
                    as usize;
                let bytes = read_two_bytes(pid, site)?;
                let fallback_ready = self.fallback_ready()?;
                if pid != self.root {
                    tracing::trace!(
                        target: "hermit::sabre::fallback",
                        tid = pid.as_raw(),
                        nr = regs.orig_rax,
                        site = format!("{site:#x}"),
                        raw = bytes == SYSCALL_INSN,
                        "child syscall entry",
                    );
                }
                if bytes == SYSCALL_INSN && fallback_ready {
                    let mapping = self.classify_mapping(pid, site)?;
                    if let Some(path) = mapping.trusted_shared_object {
                        self.trusted_shared_object_sites.insert((pid, site));
                        self.trusted_shared_objects.insert(path);
                    }
                    if !mapping.trusted {
                        write_two_bytes(pid, site, SABRE_SYSCALL_MARKER)?;
                        let syscall = regs.orig_rax;
                        regs.orig_rax = u64::MAX;
                        ptrace::setregs(pid, regs)?;
                        self.states.entry(pid).or_default().pending_patch =
                            Some(PendingPatch { site, syscall });
                        self.patched_sites.insert((pid, site));
                        tracing::debug!(
                            target: "hermit::sabre::fallback",
                            tid = pid.as_raw(),
                            address = site,
                            "redirecting raw syscall instruction through the SaBRe handler",
                        );
                    }
                }
            }
            libc::PTRACE_SYSCALL_INFO_EXIT => {
                // A cached verdict describes the mapping that occupied that page
                // when it was classified. mmap/munmap/mremap/mprotect/brk can
                // replace or re-permission that page in-process, so a page
                // previously classified as trusted can come to host a
                // completely different raw-syscall site. Drop the whole
                // ADDRESS SPACE this tracee runs in whenever it mutates that
                // space; the next site there is reclassified against live
                // /proc/<pid>/maps. Whole-space is coarse but correct --
                // per-page would be an optimisation, not a correctness
                // requirement.
                let exit_regs = ptrace::getregs(pid)?;
                if mutates_address_space(exit_regs.orig_rax) {
                    // Evict the whole ADDRESS SPACE, not just this thread's
                    // entries: CLONE_VM siblings share one mm, so a sibling
                    // would otherwise keep serving a pre-mutation verdict for
                    // the very page this syscall just replaced.
                    self.mapping_cache.invalidate_address_space(pid);
                }
                if let Some(pending) = self.states.entry(pid).or_default().pending_patch.take() {
                    let mut regs = exit_regs;
                    regs.rax = pending.syscall;
                    regs.orig_rax = pending.syscall;
                    regs.rip = pending.site as u64;
                    ptrace::setregs(pid, regs)?;
                }
            }
            _ => {}
        }
        ptrace::syscall(pid, None)?;
        Ok(())
    }

    fn handle_ptrace_event(&mut self, pid: Pid, event: libc::c_int) -> Result<(), Error> {
        if matches!(
            event,
            libc::PTRACE_EVENT_CLONE | libc::PTRACE_EVENT_FORK | libc::PTRACE_EVENT_VFORK
        ) {
            let child = Pid::from_raw(ptrace::getevent(pid)? as i32);
            self.tracees.insert(child);
            self.states.entry(child).or_default();
            // Ask the kernel whether the child it just made shares this mm.
            // Both tasks are alive at THIS stop and nowhere later, which is the
            // only window in which the question can be answered by observation
            // rather than predicted from the clone arguments.
            let shares_mm = mm_sharing(pid, child);
            self.mapping_cache.admit_child(pid, child, shares_mm);
        } else if event == libc::PTRACE_EVENT_EXEC {
            // exec installs a brand-new mm and tears down the old one.
            self.mapping_cache.replace_address_space(pid);
            self.states.insert(pid, TraceeState::default());
            self.signal_diagnostics.remove(&pid);
        }
        self.resume(pid, None)
    }

    fn resume(&self, pid: Pid, signal: Option<Signal>) -> Result<(), Error> {
        ptrace::syscall(pid, signal)?;
        Ok(())
    }
    fn fallback_ready(&mut self) -> Result<bool, Error> {
        let ready = self.readiness.load(Ordering::Acquire);
        if ready && !self.ready_observed {
            self.ready_observed = true;
            tracing::debug!(
                target: "hermit::sabre::fallback",
                "SaBRe fallback readiness observed",
            );
        }
        Ok(ready)
    }

    fn classify_mapping(
        &mut self,
        pid: Pid,
        address: usize,
    ) -> Result<MappingClassification, Error> {
        let page = address & !4095usize;
        if let Some(classification) = self.mapping_cache.get(pid, page) {
            return Ok(classification.clone());
        }
        let maps = fs::read_to_string(format!("/proc/{}/maps", pid.as_raw()))?;
        let classification = mapping_entry(&maps, address).map_or(
            MappingClassification {
                trusted: false,
                trusted_shared_object: None,
            },
            |entry| classify_mapping(&entry, self.sabre_id.as_ref(), self.plugin_id.as_ref()),
        );
        self.mapping_cache.insert(pid, page, classification.clone());
        Ok(classification)
    }

    fn remove_tracee(&mut self, pid: Pid) {
        self.tracees.remove(&pid);
        self.states.remove(&pid);
        self.mapping_cache.forget(pid);
        self.signal_diagnostics.remove(&pid);
    }
}

// nix 0.30.1 passes a null `addr` to PTRACE_GET_SYSCALL_INFO, but Linux
// defines that argument as the size of the output buffer. Use the kernel ABI
// directly until the upstream wrapper supplies the required size.
fn get_syscall_info(pid: Pid) -> Result<libc::ptrace_syscall_info, Error> {
    let mut info = std::mem::MaybeUninit::<libc::ptrace_syscall_info>::zeroed();
    let size = std::mem::size_of::<libc::ptrace_syscall_info>();
    let written = unsafe {
        libc::ptrace(
            libc::PTRACE_GET_SYSCALL_INFO,
            pid.as_raw(),
            size as *mut libc::c_void,
            info.as_mut_ptr().cast::<libc::c_void>(),
        )
    };
    if written < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    if written == 0 {
        return Err(anyhow!(
            "PTRACE_GET_SYSCALL_INFO returned no data for {pid}"
        ));
    }
    Ok(unsafe { info.assume_init() })
}

fn mapping_line(maps: &str, address: usize) -> Option<&str> {
    maps.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        let range = fields.next()?;
        let mut limits = range.split('-');
        let start = usize::from_str_radix(limits.next()?, 16).ok()?;
        let end = usize::from_str_radix(limits.next()?, 16).ok()?;
        (start <= address && address < end).then_some(line)
    })
}

/// Identity of a mapped file: its `inode` plus the canonicalized path the
/// object resolves to. This names the object itself rather than a path anyone
/// can reproduce with the same basename.
///
/// `dev` is deliberately NOT part of the identity. It looks like the obvious
/// second component and it is the one that does not survive the comparison:
/// `stat(2)` reports the filesystem's `st_dev`, while `/proc/<pid>/maps` prints
/// the SUPERBLOCK device, and on btrfs those differ because every subvolume
/// gets its own anonymous `st_dev`. Measured on one file on a btrfs developer
/// box: `stat()` gives `st_dev = 46` (rendered `00:2e`) while the maps row for
/// the very same mapping of that very same inode prints `00:2d`. Comparing
/// them made the SaBRe loader unrecognisable as infrastructure, so the
/// supervisor patched a UD0 marker into the loader's own text and the guest
/// died with SIGSEGV. `inode` is the discriminating component and it agrees;
/// the canonicalized path is what rules out an inode collision across
/// filesystems, which is the only way two distinct files can share one.
#[derive(Clone, Debug, Eq, PartialEq)]
struct FileId {
    inode: u64,
    path: PathBuf,
}

/// One parsed `/proc/<pid>/maps` row: `range perms offset dev inode path`.
struct MappingEntry<'a> {
    inode: Option<u64>,
    path: &'a str,
}

impl MappingEntry<'_> {
    /// `None` for anonymous or pseudo mappings (inode 0), which have no file
    /// identity to bind to and therefore can never match a launched object.
    fn file_id(&self) -> Option<FileId> {
        match self.inode {
            Some(inode) if inode != 0 => {
                // The kernel already prints an absolute, resolved path here;
                // canonicalizing anyway keeps both sides of the comparison in
                // the same form. A path that no longer resolves (a deleted or
                // replaced file) yields `None`, which fails CLOSED: no identity
                // means no match and therefore no exemption.
                let path = fs::canonicalize(Path::new(self.path)).ok()?;
                Some(FileId { inode, path })
            }
            _ => None,
        }
    }
}

/// Identity of a path this supervisor launched. `None` when the file cannot be
/// stat'ed, which fails CLOSED: an unknown identity matches no mapping, so the
/// exemption is withheld rather than granted.
fn launched_file_id(path: &Path) -> Option<FileId> {
    use std::os::unix::fs::MetadataExt;
    let metadata = fs::metadata(path).ok()?;
    let canonical = fs::canonicalize(path).ok()?;
    Some(FileId {
        inode: metadata.ino(),
        path: canonical,
    })
}

fn mapping_entry(maps: &str, address: usize) -> Option<MappingEntry<'_>> {
    let mut fields = mapping_line(maps, address)?.split_whitespace();
    fields.next()?; // range
    fields.next()?; // perms
    fields.next()?; // offset
    // dev: the superblock device column is parsed past but deliberately NOT
    // stored — it is not part of file identity, because btrfs disagrees with
    // stat's `st_dev` for the same inode (see the `FileId` doc above).
    fields.next()?; // dev
    let inode = fields.next()?;
    Some(MappingEntry {
        inode: inode.parse::<u64>().ok(),
        path: fields.next().unwrap_or(""),
    })
}

fn mapping_diagnostic(maps: &str, address: usize) -> Option<MappingDiagnostic> {
    let line = mapping_line(maps, address)?;
    let mut fields = line.split_whitespace();
    let mut limits = fields.next()?.split('-');
    let start = usize::from_str_radix(limits.next()?, 16).ok()?;
    fields.next()?;
    let mapping_offset = usize::from_str_radix(fields.next()?, 16).ok()?;
    let relative_offset = address.checked_sub(start)?;
    Some(MappingDiagnostic {
        line: line.to_owned(),
        relative_offset,
        file_offset: mapping_offset.checked_add(relative_offset)?,
    })
}

/// Syscalls that can replace, move, unmap or re-permission a page in the
/// tracee's address space. A cached page classification is only valid until one
/// of these runs, so observing any of them invalidates that tracee's cache.
const ADDRESS_SPACE_MUTATORS: [u64; 6] = [
    libc::SYS_mmap as u64,
    libc::SYS_munmap as u64,
    libc::SYS_mremap as u64,
    libc::SYS_mprotect as u64,
    libc::SYS_brk as u64,
    libc::SYS_shmat as u64,
];

/// True when `nr` is an address-space mutator. On x86-64 the syscall number
/// remains in `orig_rax` at the syscall-exit stop, which is where the cache is
/// invalidated -- after the mapping change has actually taken effect.
fn mutates_address_space(nr: u64) -> bool {
    ADDRESS_SPACE_MUTATORS.contains(&nr)
}

/// Whether two tasks share one address space.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MmSharing {
    Shared,
    Private,
}

/// `kcmp(2)`'s "compare address spaces" selector. `KCMP_FILE` is 0 and
/// `KCMP_VM` is 1; getting that backwards silently compares file descriptor 0,
/// which two related tasks almost always share, so every answer comes back
/// "shared".
const KCMP_VM: libc::c_int = 1;

/// Ask the KERNEL whether `child` shares `parent`'s address space.
///
/// This is an observation of the tasks that now exist, taken at the
/// `PTRACE_EVENT_*` stop where both are alive. It deliberately replaces the
/// earlier approach of predicting the answer from clone arguments:
///
///   * `clone(2)` carries its flags in a register, which a sibling cannot
///     change while the caller is stopped -- that read was sound, and
///   * `clone3(2)` carries them in a USER BUFFER that the kernel copies AFTER
///     the syscall-entry stop. Sibling threads stay runnable across that stop,
///     so the buffer can change in the window and the prediction can be wrong
///     in either direction. Measured with a tracer that rewrites the buffer
///     after the entry read: pre-copy flags said SHARED where the child was
///     private, and said private where the child was SHARED.
///
/// Rather than keep two rules with different trust properties, every clone
/// shape is now answered the same way, by the kernel, after the fact.
///
/// `kcmp` needs both tasks alive: called after either exits it fails with
/// `ESRCH`. Any failure -- including a kernel built without
/// `CONFIG_CHECKPOINT_RESTORE`, where the syscall is `ENOSYS` -- falls back to
/// `Shared`, which over-evicts. Over-eviction costs one re-read of
/// `/proc/<pid>/maps`; under-eviction leaves a stale trusted verdict that a raw
/// syscall can hide behind.
fn mm_sharing(parent: Pid, child: Pid) -> MmSharing {
    // SAFETY: kcmp takes only scalars and returns a scalar.
    let result = unsafe {
        libc::syscall(
            libc::SYS_kcmp,
            parent.as_raw() as libc::c_long,
            child.as_raw() as libc::c_long,
            KCMP_VM as libc::c_long,
            0 as libc::c_long,
            0 as libc::c_long,
        )
    };
    interpret_kcmp_vm(result)
}

/// `kcmp` returns 0 when the two address spaces are the SAME, an ordering
/// value (1/2/3) when they differ, and -1 on error. Split out so the mapping --
/// including the fail-safe on error -- is testable without spawning tasks.
fn interpret_kcmp_vm(result: libc::c_long) -> MmSharing {
    match result {
        0 => MmSharing::Shared,
        r if r > 0 => MmSharing::Private,
        // Errors fail SAFE: assume sharing rather than risk a stale verdict.
        _ => MmSharing::Shared,
    }
}

/// Kernel-supplied mappings whose raw syscall sites are causally identified as
/// infrastructure the guest cannot have rewritten. `[vdso]` and `[vsyscall]`
/// are mapped by the kernel and are not writable, so SaBRe cannot expand a
/// syscall site inside them and the supervisor must not try to patch one.
///
/// They are exempt from REDIRECTION only — never from ACCOUNTING. A raw syscall
/// executed here did not traverse the measured in-guest SaBRe handler, so it is
/// still recorded as a trusted-native site and still makes the cell ineligible.
/// Exempting a mapping from both redirection and counting is the one
/// combination that makes a real raw syscall disappear from the evidence.
const CAUSAL_KERNEL_MAPPINGS: [&str; 2] = ["[vdso]", "[vsyscall]"];

fn classify_mapping(
    entry: &MappingEntry<'_>,
    sabre: Option<&FileId>,
    plugin: Option<&FileId>,
) -> MappingClassification {
    let path = entry.path;
    if path.starts_with('[') {
        // Only the causally identified kernel mappings above are exempt from
        // redirection, and even those are counted. Every other bracket-named
        // mapping -- `[stack]`, `[heap]`, or any future kernel name -- is NOT
        // infrastructure: fall through to `trusted: false` so the site is
        // redirected through the SaBRe handler and counted as a fallback site.
        if CAUSAL_KERNEL_MAPPINGS.contains(&path) {
            return MappingClassification {
                trusted: true,
                trusted_shared_object: Some(PathBuf::from(path)),
            };
        }
        return MappingClassification {
            trusted: false,
            trusted_shared_object: None,
        };
    }
    let path = Path::new(path.strip_suffix(" (deleted)").unwrap_or(path));
    // Bind the infrastructure exemption to the identity of the objects this
    // supervisor actually launched (inode plus canonical path, both resolved
    // from the same `/proc/<pid>/maps` line), not to their basenames. A
    // basename match would
    // exempt any unrelated mapping that happens to be called `sabre` or
    // `libdetcore_sabre.so`.
    let mapped = entry.file_id();
    let infrastructure = matches!((mapped.as_ref(), sabre), (Some(seen), Some(known)) if seen == known)
        || matches!((mapped.as_ref(), plugin), (Some(seen), Some(known)) if seen == known);
    if infrastructure {
        return MappingClassification {
            trusted: true,
            trusted_shared_object: None,
        };
    }
    // SaBRe owns shared-library rewriting. A raw syscall that still reaches the
    // supervisor from another shared object ran outside the measured in-guest
    // interception path. Keep trusting it for runtime safety, but report it so
    // compatibility accounting cannot credit the cell to SaBRe.
    let trusted_shared_object = path
        .file_name()
        .is_some_and(|name| name.to_string_lossy().contains(".so"))
        .then(|| path.to_path_buf());
    MappingClassification {
        trusted: trusted_shared_object.is_some(),
        trusted_shared_object,
    }
}

fn read_two_bytes(pid: Pid, address: usize) -> Result<[u8; 2], Error> {
    let word_size = std::mem::size_of::<libc::c_long>();
    let aligned = address & !(word_size - 1);
    let offset = address - aligned;
    let first = ptrace::read(pid, aligned as ptrace::AddressType)?.to_ne_bytes();
    if offset + 1 < word_size {
        Ok([first[offset], first[offset + 1]])
    } else {
        let second = ptrace::read(pid, (aligned + word_size) as ptrace::AddressType)?.to_ne_bytes();
        Ok([first[offset], second[0]])
    }
}

fn read_diagnostic_bytes(pid: Pid, address: usize) -> Result<[u8; 16], Error> {
    let mut bytes = [0; 16];
    let word_size = std::mem::size_of::<libc::c_long>();
    for (index, chunk) in bytes.chunks_mut(word_size).enumerate() {
        let word = ptrace::read(pid, (address + index * word_size) as ptrace::AddressType)?;
        chunk.copy_from_slice(&word.to_ne_bytes()[..chunk.len()]);
    }
    Ok(bytes)
}

fn write_two_bytes(pid: Pid, address: usize, bytes: [u8; 2]) -> Result<(), Error> {
    let word_size = std::mem::size_of::<libc::c_long>();
    let aligned = address & !(word_size - 1);
    let offset = address - aligned;
    let mut first = ptrace::read(pid, aligned as ptrace::AddressType)?.to_ne_bytes();
    first[offset] = bytes[0];
    if offset + 1 < word_size {
        first[offset + 1] = bytes[1];
        ptrace::write(
            pid,
            aligned as ptrace::AddressType,
            libc::c_long::from_ne_bytes(first),
        )?;
    } else {
        ptrace::write(
            pid,
            aligned as ptrace::AddressType,
            libc::c_long::from_ne_bytes(first),
        )?;
        let second_address = aligned + word_size;
        let mut second = ptrace::read(pid, second_address as ptrace::AddressType)?.to_ne_bytes();
        second[0] = bytes[1];
        ptrace::write(
            pid,
            second_address as ptrace::AddressType,
            libc::c_long::from_ne_bytes(second),
        )?;
    }
    Ok(())
}

pub async fn run(
    mut command: std::process::Command,
    sabre: PathBuf,
    plugin: PathBuf,
    readiness: Arc<AtomicBool>,
    physical_exit_observer: Arc<detcore::GlobalState>,
    capture_output: bool,
) -> Result<Output, Error> {
    if capture_output {
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
    }
    // Spawn before creating the blocking supervisor worker. A worker thread consumes a task ID
    // in the guest PID namespace; creating it first shifts the root guest from PID 3 to PID 4 and
    // makes otherwise identical ptrace and SaBRe programs observe different process identities.
    let child = spawn_tracee(command)?;
    let root = Pid::from_raw(child.id() as i32);
    match waitpid(root, Some(WaitPidFlag::__WALL))? {
        WaitStatus::Stopped(pid, Signal::SIGTRAP) if pid == root => {}
        status => return Err(anyhow!("unexpected initial SaBRe ptrace stop: {status:?}")),
    }
    // Ptrace ownership belongs to the individual tracer task, not its thread group. Leave the
    // tracee stopped while handing ownership from this async caller to the blocking supervisor.
    // Injecting SIGSTOP as part of detach prevents any guest instruction from running in between.
    ptrace::detach(root, Some(Signal::SIGSTOP))
        .context("failed to hand SaBRe tracee to supervisor worker")?;
    tokio::task::spawn_blocking(move || {
        run_blocking(child, sabre, plugin, readiness, physical_exit_observer)
    })
    .await
    .context("SaBRe ptrace supervisor task panicked")?
}

fn spawn_tracee(mut command: std::process::Command) -> Result<std::process::Child, Error> {
    // TODO-HUMAN-REVIEW(PR-845): Review SaBRe launch-time ASLR disabling.
    // PTRACE_TRACEME makes exec stop with SIGTRAP. A pre-exec SIGSTOP would
    // deadlock std::process::Command on its exec error pipe. personality(2)
    // is async-signal-safe and survives the SaBRe and guest execs.
    unsafe {
        command.pre_exec(|| {
            let current = libc::personality(0xffff_ffff);
            if current == -1 {
                return Err(std::io::Error::last_os_error());
            }
            let deterministic = current as libc::c_ulong | libc::ADDR_NO_RANDOMIZE as libc::c_ulong;
            if libc::personality(deterministic) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            ptrace::traceme().map_err(std::io::Error::from)
        });
    }

    command
        .spawn()
        .context("failed to spawn ptraced SaBRe guest")
}

fn run_blocking(
    mut child: std::process::Child,
    sabre: PathBuf,
    plugin: PathBuf,
    readiness: Arc<AtomicBool>,
    physical_exit_observer: Arc<detcore::GlobalState>,
) -> Result<Output, Error> {
    let root = Pid::from_raw(child.id() as i32);
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    drop(child);

    let stdout_thread = std::thread::spawn(move || read_pipe(stdout));
    let stderr_thread = std::thread::spawn(move || read_pipe(stderr));
    let supervised = Supervisor::new(root, sabre, plugin, readiness, physical_exit_observer).run();
    if supervised.is_err() {
        let _ = nix::sys::signal::kill(root, Signal::SIGKILL);
    }
    let stdout = stdout_thread
        .join()
        .map_err(|_| anyhow!("SaBRe stdout reader panicked"))??;
    let stderr = stderr_thread
        .join()
        .map_err(|_| anyhow!("SaBRe stderr reader panicked"))??;
    let (status, path_evidence) = supervised?;
    Ok(Output {
        status,
        stdout,
        stderr,
        path_evidence,
    })
}

fn read_pipe<R: Read>(pipe: Option<R>) -> Result<Vec<u8>, std::io::Error> {
    let mut bytes = Vec::new();
    if let Some(mut pipe) = pipe {
        pipe.read_to_end(&mut bytes)?;
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acknowledges_only_final_physical_exit_statuses() {
        let child = Pid::from_raw(17);
        let parent = Pid::from_raw(11);

        assert_eq!(
            final_physical_exit(&WaitStatus::Exited(child, 0)).map(|(pid, _)| pid),
            Some(child)
        );
        assert_eq!(
            final_physical_exit(&WaitStatus::Signaled(child, Signal::SIGKILL, false))
                .map(|(pid, _)| pid),
            Some(child)
        );
        assert!(
            final_physical_exit(&WaitStatus::PtraceEvent(
                child,
                Signal::SIGTRAP,
                libc::PTRACE_EVENT_EXIT,
            ))
            .is_none()
        );
        assert!(final_physical_exit(&WaitStatus::Stopped(parent, Signal::SIGCHLD)).is_none());
    }

    #[test]
    fn preserves_hardware_sigill_across_userspace_reraise() {
        const KERNEL_FAULT: i32 = 2;
        const USER_RERAISE: i32 = -6;
        let hardware = Some((Signal::SIGILL, Some(KERNEL_FAULT), false));

        assert!(!should_replace_signal_diagnostic(
            hardware,
            Signal::SIGILL,
            Some(USER_RERAISE),
        ));
        assert!(!should_replace_signal_diagnostic(
            hardware,
            Signal::SIGILL,
            None,
        ));
        assert!(should_replace_signal_diagnostic(
            hardware,
            Signal::SIGILL,
            Some(KERNEL_FAULT),
        ));
        assert!(should_replace_signal_diagnostic(
            hardware,
            Signal::SIGSEGV,
            Some(KERNEL_FAULT),
        ));
        assert!(should_replace_signal_diagnostic(
            Some((Signal::SIGILL, Some(KERNEL_FAULT), true)),
            Signal::SIGILL,
            Some(USER_RERAISE),
        ));
        assert!(should_replace_signal_diagnostic(
            Some((Signal::SIGILL, Some(USER_RERAISE), false)),
            Signal::SIGILL,
            Some(USER_RERAISE),
        ));
    }

    #[test]
    fn recognizes_sabre_sigill_markers() {
        for marker in [[0x0f, 0xff], [0x0f, 0x0b], [0x0f, 0x0c]] {
            let mut bytes = [0; 16];
            bytes[..2].copy_from_slice(&marker);
            assert!(is_sabre_sigill_marker(Some(&bytes)));
        }

        let mut unknown = [0; 16];
        unknown[..2].copy_from_slice(&[0x62, 0xf1]);
        assert!(!is_sabre_sigill_marker(Some(&unknown)));
        assert!(!is_sabre_sigill_marker(None));
    }

    #[test]
    fn finds_mapping_path() {
        let maps = concat!(
            "1000-2000 r-xp 00002000 00:00 0 /tmp/sabre\n",
            "3000-4000 rwxp 00000000 00:00 0 \n",
        );
        assert_eq!(
            mapping_entry(maps, 0x1234).map(|entry| entry.path),
            Some("/tmp/sabre")
        );
        assert_eq!(
            mapping_entry(maps, 0x3456).map(|entry| entry.path),
            Some("")
        );
        assert!(mapping_entry(maps, 0x2500).is_none());
        let diagnostic = mapping_diagnostic(maps, 0x1234).unwrap();
        assert_eq!(diagnostic.relative_offset, 0x234);
        assert_eq!(diagnostic.file_offset, 0x2234);
        assert_eq!(
            diagnostic.line,
            "1000-2000 r-xp 00002000 00:00 0 /tmp/sabre"
        );
    }

    // Rewritten for the identity-based contract. The previous version of this
    // test asserted the two defects codex found: it required a different-root
    // `libdetcore_sabre.so` and `[vdso]` to be `trusted` with NO counted site --
    // the one combination that makes a real raw syscall vanish from evidence.
    #[test]
    fn classifies_runtime_mapping_attribution() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (sabre_path, sabre) = real_file(dir.path(), "sabre");
        let (plugin_path, plugin) = real_file(dir.path(), "libdetcore_sabre.so");

        // The launched objects themselves: exempt and uncounted, because they
        // ARE the measured interception path. Real files, and the maps rows
        // carry a dev that does NOT match stat() -- see the regression test.
        for (inode, path) in [
            (sabre.inode, sabre_path.to_str().expect("utf8")),
            (plugin.inode, plugin_path.to_str().expect("utf8")),
        ] {
            let maps = maps_row("1000-2000", "00:2d", inode, path);
            let entry = mapping_entry(&maps, 0x1234).unwrap();
            assert_eq!(
                classify_mapping(&entry, Some(&sabre), Some(&plugin)),
                MappingClassification {
                    trusted: true,
                    trusted_shared_object: None
                },
                "{path} is a launched object and must be exempt"
            );
        }

        // A DIFFERENT object wearing the plugin's basename is refused, even
        // with the ` (deleted)` suffix the old test used.
        let maps = maps_row(
            "1000-2000",
            "fd:01",
            7777,
            "/different/root/libdetcore_sabre.so (deleted)",
        );
        let entry = mapping_entry(&maps, 0x1234).unwrap();
        assert_eq!(
            classify_mapping(&entry, Some(&sabre), Some(&plugin)),
            MappingClassification {
                trusted: true,
                trusted_shared_object: Some(PathBuf::from("/different/root/libdetcore_sabre.so")),
            },
            "a basename impostor must be counted as a trusted-native site, not exempted"
        );

        // Ordinary shared object: trusted for runtime safety, counted for
        // accounting. Unchanged contract.
        let maps = maps_row("1000-2000", "fd:01", 55, "/usr/lib/libc.so.6");
        let entry = mapping_entry(&maps, 0x1234).unwrap();
        assert_eq!(
            classify_mapping(&entry, Some(&sabre), Some(&plugin)),
            MappingClassification {
                trusted: true,
                trusted_shared_object: Some(PathBuf::from("/usr/lib/libc.so.6")),
            }
        );

        // Non-library and anonymous mappings are redirected and counted as
        // fallback sites. Unchanged contract.
        for (inode, path) in [(66, "/usr/bin/echo"), (0, "")] {
            let maps = maps_row("1000-2000", "fd:01", inode, path);
            let entry = mapping_entry(&maps, 0x1234).unwrap();
            assert_eq!(
                classify_mapping(&entry, Some(&sabre), Some(&plugin)),
                MappingClassification {
                    trusted: false,
                    trusted_shared_object: None
                }
            );
        }
    }

    /// One synthetic `/proc/<pid>/maps` row. Columns are exactly what the
    /// kernel prints: `range perms offset dev inode path`.
    fn maps_row(range: &str, dev: &str, inode: u64, path: &str) -> String {
        format!("{range} r-xp 00000000 {dev} {inode} {path}\n")
    }

    fn classify(maps: &str, address: usize, sabre: Option<&FileId>) -> MappingClassification {
        let entry = mapping_entry(maps, address).expect("mapping row must parse");
        classify_mapping(&entry, sabre, None)
    }

    /// A real file on disk, plus its identity. The synthetic-row tests below
    /// must use REAL paths: identity is now (inode, canonicalized path), and a
    /// fabricated path resolves to nothing and therefore matches nothing.
    fn real_file(dir: &std::path::Path, name: &str) -> (PathBuf, FileId) {
        let path = dir.join(name);
        fs::write(&path, b"object").expect("write fixture");
        let id = launched_file_id(&path).expect("fixture must have an identity");
        (path, id)
    }

    // FINDING 1 -- bracketed mappings.
    //
    // POSITIVE: a raw syscall in the kernel-supplied [vdso] is exempt from
    // REDIRECTION (it is not writable, so it cannot be patched) but is still
    // COUNTED, so the cell cannot be silently credited to SaBRe.
    #[test]
    fn vdso_raw_syscall_is_counted_not_silently_trusted() {
        let maps = maps_row("7ffff7fc9000-7ffff7fcb000", "00:00", 0, "[vdso]");
        let classification = classify(&maps, 0x7ffff7fc9010, None);
        assert!(
            classification.trusted,
            "[vdso] is not writable, so it must not be redirected"
        );
        assert_eq!(
            classification.trusted_shared_object,
            Some(PathBuf::from("[vdso]")),
            "a [vdso] raw syscall did not traverse the SaBRe handler and must be counted"
        );
    }

    // NEGATIVE: any other bracket-named mapping is NOT causally identified
    // infrastructure. An executable [stack] hosting a raw syscall must be
    // redirected and counted as a fallback site, never silently exempted.
    #[test]
    fn non_infrastructure_bracket_mapping_is_refused() {
        for name in ["[stack]", "[heap]", "[anon:jit]"] {
            let maps = maps_row("7ffffffde000-7ffffffff000", "00:00", 0, name);
            let classification = classify(&maps, 0x7ffffffde010, None);
            assert!(
                !classification.trusted,
                "{name} must be redirected through the SaBRe handler, not exempted"
            );
        }
    }

    // FINDING 2, THE REAL-MAPPING TEST. The reviewer's closing requirement was
    // "a test that would have failed today -- one exercising the real launched
    // loader rather than a synthetic maps row", and the reason is exact: every
    // other test here builds BOTH halves of the comparison itself, so dev and
    // inode agree by construction and the btrfs disagreement is invisible to
    // them by design. This one fabricates neither half. It mmaps a real file,
    // finds the kernel's own row for that mapping in /proc/self/maps, and
    // requires the identity derived from the maps row to equal the identity
    // derived from stat() on the same path.
    //
    // On btrfs at the previous head this FAILS: stat() reports the subvolume's
    // anonymous st_dev while maps prints the superblock's, so the two FileIds
    // differ in `dev` despite naming one object. It passes iff identity is
    // built from components that actually agree.
    #[test]
    fn a_real_mapping_and_stat_agree_on_the_same_object() {
        use std::io::Write;

        let mut file = tempfile::NamedTempFile::new().expect("tempfile");
        file.write_all(&[0u8; 4096]).expect("write");
        file.flush().expect("flush");
        let path = file.path().to_path_buf();

        // Map it, so the kernel prints a row for this exact object.
        let fd = fs::File::open(&path).expect("open");
        let mapping = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                4096,
                libc::PROT_READ,
                libc::MAP_PRIVATE,
                std::os::unix::io::AsRawFd::as_raw_fd(&fd),
                0,
            )
        };
        assert_ne!(mapping, libc::MAP_FAILED, "mmap of the fixture failed");
        let address = mapping as usize;

        let maps = fs::read_to_string("/proc/self/maps").expect("read own maps");
        let entry = mapping_entry(&maps, address).expect("our own mapping must have a row");
        let from_maps = entry
            .file_id()
            .expect("a file-backed mapping has an identity");
        let from_stat = launched_file_id(&path).expect("the fixture must stat");

        // SAFETY: `mapping` is the address mmap just returned for this length.
        unsafe { libc::munmap(mapping, 4096) };

        assert_eq!(
            from_maps, from_stat,
            "the identity of one object must not depend on which side of the \
             kernel it is observed from; a mismatch here is what patched the \
             SaBRe loader's own text and SIGSEGV'd the guest"
        );
    }

    // FINDING 2 -- infrastructure identity.
    //
    // POSITIVE, and this is the REGRESSION TEST: the launched loader is
    // recognised even when the maps `dev` column disagrees with `stat()`.
    // The row below deliberately carries a WRONG dev (`00:2d` against a
    // `stat()` that will report something else), which is exactly what btrfs
    // produces for a real file: same inode, different device, because each
    // subvolume gets its own anonymous `st_dev` while maps prints the
    // superblock's. Under the previous dev+inode identity this assertion
    // FAILS -- the loader is not recognised, its text is patched with UD0, and
    // the guest SIGSEGVs. That is the defect this test exists to catch.
    #[test]
    fn launched_sabre_object_is_recognised_despite_a_disagreeing_dev_column() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (path, id) = real_file(dir.path(), "sabre");
        let maps = maps_row(
            "400000-401000",
            "00:2d",
            id.inode,
            path.to_str().expect("utf8 path"),
        );
        let classification = classify(&maps, 0x400010, Some(&id));
        assert!(
            classification.trusted,
            "the launched loader must stay infrastructure even when the maps dev \
             column disagrees with stat() (btrfs subvolume anon dev)"
        );
        assert_eq!(classification.trusted_shared_object, None);
    }

    // NEGATIVE: an impostor with the SAME BASENAME but a different object
    // identity must NOT inherit the exemption. This is the planted bypass.
    #[test]
    fn same_basename_different_object_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (_, known) = real_file(dir.path(), "sabre");
        let attacker = dir.path().join("attacker");
        fs::create_dir(&attacker).expect("mkdir");
        let (impostor_path, impostor) = real_file(&attacker, "sabre");
        assert_ne!(
            known.inode, impostor.inode,
            "fixtures must be distinct files"
        );
        let maps = maps_row(
            "500000-501000",
            "fd:01",
            impostor.inode,
            impostor_path.to_str().expect("utf8 path"),
        );
        let classification = classify(&maps, 0x500010, Some(&known));
        assert!(
            !classification.trusted,
            "basename `sabre` must not exempt a different object"
        );
        assert_eq!(classification.trusted_shared_object, None);
    }

    // NEGATIVE: an unstat-able launched path yields no identity, and a missing
    // identity must withhold the exemption (fail closed) rather than grant it.
    #[test]
    fn unknown_launched_identity_grants_no_exemption() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (path, id) = real_file(dir.path(), "sabre");
        let maps = maps_row(
            "400000-401000",
            "fd:01",
            id.inode,
            path.to_str().expect("utf8 path"),
        );
        let classification = classify(&maps, 0x400010, None);
        assert!(
            !classification.trusted,
            "an unresolved launched identity must not exempt anything"
        );
    }

    // A shared object that is neither infrastructure nor kernel mapping stays
    // trusted-for-safety but counted, which is the pre-existing contract this
    // change must not weaken.
    #[test]
    fn other_shared_object_remains_counted() {
        let maps = maps_row(
            "7f0000000000-7f0000001000",
            "fd:01",
            77,
            "/usr/lib64/libc.so.6",
        );
        let dir = tempfile::tempdir().expect("tempdir");
        let (_, known) = real_file(dir.path(), "sabre");
        let classification = classify(&maps, 0x7f0000000010, Some(&known));
        assert!(classification.trusted);
        assert_eq!(
            classification.trusted_shared_object,
            Some(PathBuf::from("/usr/lib64/libc.so.6"))
        );
    }

    // FINDING 3 -- cache invalidation. Both sides of the predicate that decides
    // whether a tracee's cached page verdicts survive a syscall.
    fn trusted() -> MappingClassification {
        MappingClassification {
            trusted: true,
            trusted_shared_object: None,
        }
    }

    /// FINDING 2, the unsound case. Threads A and B share one address space
    /// (`CLONE_VM`). B caches a page as trusted; A then mutates the address
    /// space. B MUST NOT keep serving the pre-mutation verdict for that page --
    /// under the old `(Pid, page)` keying only A's entries were evicted, so B
    /// went on treating a page that may no longer be trusted as trusted, and a
    /// raw syscall there bypassed both redirection and accounting.
    #[test]
    fn sibling_thread_cannot_serve_a_stale_classification_after_a_shared_mm_mutation() {
        const PAGE: usize = 0x1000;
        let a = Pid::from_raw(100);
        let b = Pid::from_raw(101);
        let mut cache = MappingCache::new(a);
        cache.share_address_space(a, b);

        // B observes the page and caches it.
        cache.insert(b, PAGE, trusted());
        assert!(
            cache.get(b, PAGE).is_some(),
            "precondition: B has a cached verdict, so the eviction below is not vacuous"
        );

        // A mutates the SHARED address space.
        cache.invalidate_address_space(a);

        assert!(
            cache.get(b, PAGE).is_none(),
            "a sibling sharing the mm must lose its cached verdict when ANY thread \
             in that mm mutates the address space"
        );
    }

    /// The matching control: eviction must be scoped to the address space that
    /// actually changed, not global. A task in an UNRELATED mm keeps its cache,
    /// so the fix is a re-keying rather than a blanket flush.
    #[test]
    fn unrelated_address_space_is_not_over_evicted() {
        const PAGE: usize = 0x1000;
        let a = Pid::from_raw(100);
        let b = Pid::from_raw(101);
        let unrelated = Pid::from_raw(200);
        let mut cache = MappingCache::new(a);
        cache.share_address_space(a, b);
        cache.new_address_space(unrelated);

        cache.insert(b, PAGE, trusted());
        cache.insert(unrelated, PAGE, trusted());

        cache.invalidate_address_space(a);

        assert!(cache.get(b, PAGE).is_none(), "the mutated mm is evicted");
        assert!(
            cache.get(unrelated, PAGE).is_some(),
            "a different address space maps a different page at the same address \
             and must keep its verdict"
        );
    }

    /// Fork a child of the requested shape, run `check` while it is alive, then
    /// reap it. Real tasks, because the property under test is what the KERNEL
    /// did -- a hand-built pid pair would test nothing.
    fn with_child<F: FnOnce(Pid, Pid)>(share_mm: bool, check: F) {
        const STACK: usize = 1 << 20;
        let mut stack = vec![0u8; STACK];
        let child = if share_mm {
            // CLONE_VM|SIGCHLD: shares the mm AND is reported as
            // PTRACE_EVENT_FORK, the shape that defeats an event-kind rule.
            let top = unsafe { stack.as_mut_ptr().add(STACK) } as *mut libc::c_void;
            extern "C" fn sleeper(_: *mut libc::c_void) -> libc::c_int {
                unsafe { libc::pause() };
                0
            }
            unsafe {
                libc::clone(
                    sleeper,
                    top,
                    libc::CLONE_VM | libc::SIGCHLD,
                    std::ptr::null_mut(),
                )
            }
        } else {
            match unsafe { libc::fork() } {
                0 => unsafe {
                    libc::pause();
                    libc::_exit(0)
                },
                pid => pid,
            }
        };
        assert!(child > 0, "failed to create the {share_mm} child");
        let parent = Pid::from_raw(unsafe { libc::getpid() });
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            check(parent, Pid::from_raw(child))
        }));
        unsafe {
            libc::kill(child, libc::SIGKILL);
            libc::waitpid(child, std::ptr::null_mut(), libc::__WALL);
        }
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

    /// The oracle itself, against real tasks, both ways.
    ///
    /// This is the whole reason the clone arguments are no longer consulted: a
    /// `CLONE_VM` child is reported as `PTRACE_EVENT_FORK` when its exit signal
    /// is `SIGCHLD`, so the event kind cannot tell these two apart -- but the
    /// kernel can, and does.
    #[test]
    fn the_kernel_distinguishes_a_shared_mm_from_a_private_one() {
        with_child(true, |parent, child| {
            assert_eq!(
                mm_sharing(parent, child),
                MmSharing::Shared,
                "a CLONE_VM child shares the caller's address space"
            );
        });
        with_child(false, |parent, child| {
            assert_eq!(
                mm_sharing(parent, child),
                MmSharing::Private,
                "a forked child gets its own address space"
            );
        });
    }

    /// A `CLONE_VM` child must lose a cached verdict when the parent mutates the
    /// shared mm -- driven end to end from the real child through the real
    /// oracle into the real cache, with no hand-supplied sharing answer.
    #[test]
    fn a_clone_vm_child_cannot_serve_a_stale_classification() {
        const PAGE: usize = 0x5000;
        with_child(true, |parent, child| {
            let mut cache = MappingCache::new(parent);
            cache.admit_child(parent, child, mm_sharing(parent, child));

            cache.insert(child, PAGE, trusted());
            assert!(
                cache.get(child, PAGE).is_some(),
                "precondition: the child has a cached verdict, so the eviction is not vacuous"
            );

            cache.invalidate_address_space(parent);

            assert!(
                cache.get(child, PAGE).is_none(),
                "the child shares the mm the parent just mutated, so its cached \
                 verdict must be dropped"
            );
        });
    }

    /// The control on the same end-to-end path: a child with its OWN mm keeps
    /// its cache, so the oracle did not collapse into "always share".
    #[test]
    fn a_private_mm_child_keeps_its_own_address_space() {
        const PAGE: usize = 0x6000;
        with_child(false, |parent, child| {
            let mut cache = MappingCache::new(parent);
            cache.admit_child(parent, child, mm_sharing(parent, child));

            cache.insert(child, PAGE, trusted());
            cache.invalidate_address_space(parent);

            assert!(
                cache.get(child, PAGE).is_some(),
                "a private mm cannot be reached by the parent's mutation"
            );
        });
    }

    /// `clone3(2)` specifically, with the buffer CLOBBERED after the call --
    /// the regression test for the race that motivated this design.
    ///
    /// `clone3` passes its flags in a user buffer that the kernel copies after
    /// the syscall-entry ptrace stop, so any implementation that reads that
    /// buffer is reading something a concurrently runnable sibling can still
    /// change. Here the parent overwrites `flags` immediately after the call,
    /// which is what such a sibling would do: a buffer-reading implementation
    /// then sees `0` and concludes "private", while the child the kernel
    /// actually created shares the mm. The kernel oracle is unaffected because
    /// it inspects the child, not the request.
    #[test]
    fn clone3_sharing_is_read_from_the_child_not_from_a_clobberable_buffer() {
        #[repr(C)]
        #[derive(Default)]
        struct CloneArgs {
            flags: u64,
            pidfd: u64,
            child_tid: u64,
            parent_tid: u64,
            exit_signal: u64,
            stack: u64,
            stack_size: u64,
            tls: u64,
        }
        const STACK: usize = 1 << 20;
        let mut stack = vec![0u8; STACK];
        let mut args = CloneArgs {
            flags: libc::CLONE_VM as u64,
            exit_signal: libc::SIGCHLD as u64,
            stack: stack.as_mut_ptr() as u64,
            stack_size: STACK as u64,
            ..Default::default()
        };
        let child = unsafe {
            libc::syscall(
                libc::SYS_clone3,
                &mut args as *mut CloneArgs,
                std::mem::size_of::<CloneArgs>(),
            )
        };
        if child == 0 {
            // In the child, on the supplied stack: do nothing that could unwind.
            unsafe {
                libc::pause();
                libc::_exit(0)
            };
        }
        assert!(
            child > 0,
            "clone3 failed: {}",
            std::io::Error::last_os_error()
        );

        // The racing write. Any implementation that consults the request rather
        // than the child now has CLONE_VM erased under it.
        args.flags = 0;
        assert_eq!(
            args.flags & libc::CLONE_VM as u64,
            0,
            "the request now reads private"
        );

        let parent = Pid::from_raw(unsafe { libc::getpid() });
        let child = Pid::from_raw(child as i32);
        let observed = mm_sharing(parent, child);

        unsafe {
            libc::kill(child.as_raw(), libc::SIGKILL);
            libc::waitpid(child.as_raw(), std::ptr::null_mut(), libc::__WALL);
        }

        assert_eq!(
            observed,
            MmSharing::Shared,
            "the child shares the mm; reading the clobbered request would have said private"
        );
    }

    /// The kcmp result mapping, including the fail-safe. A dead task, a kernel
    /// without CONFIG_CHECKPOINT_RESTORE, or any other error must resolve to
    /// SHARED: over-eviction costs a `/proc/<pid>/maps` re-read, under-eviction
    /// leaves a stale verdict a raw syscall can hide behind.
    #[test]
    fn an_unanswerable_kcmp_fails_safe_to_shared() {
        assert_eq!(interpret_kcmp_vm(0), MmSharing::Shared, "0 means same mm");
        for ordered in [1, 2, 3] {
            assert_eq!(
                interpret_kcmp_vm(ordered),
                MmSharing::Private,
                "a nonzero ordering value means the address spaces differ"
            );
        }
        assert_eq!(
            interpret_kcmp_vm(-1),
            MmSharing::Shared,
            "an error must fail safe to sharing, never to private"
        );
    }

    /// And the same fail-safe through the live path: asking about a pid that
    /// cannot be compared must not report `Private`.
    #[test]
    fn mm_sharing_of_an_unavailable_task_fails_safe_to_shared() {
        // A pid that is not ours to inspect; kcmp refuses rather than answering.
        let parent = Pid::from_raw(unsafe { libc::getpid() });
        assert_eq!(
            mm_sharing(parent, Pid::from_raw(-1)),
            MmSharing::Shared,
            "an unanswerable comparison must over-evict, not under-evict"
        );
    }

    /// fork does NOT share the mm, so a forked child must not be evicted by its
    /// parent's mutations -- the other direction of the same scoping property.
    #[test]
    fn forked_child_gets_its_own_address_space() {
        const PAGE: usize = 0x2000;
        let parent = Pid::from_raw(100);
        let child = Pid::from_raw(101);
        let mut cache = MappingCache::new(parent);
        cache.new_address_space(child);

        cache.insert(child, PAGE, trusted());
        cache.invalidate_address_space(parent);

        assert!(
            cache.get(child, PAGE).is_some(),
            "fork installs a separate mm; the parent's mutation cannot reach it"
        );
    }

    /// exec replaces the mm outright, so nothing cached against the old one may
    /// survive for the exec'ing task.
    #[test]
    fn exec_discards_the_previous_address_space() {
        const PAGE: usize = 0x3000;
        let pid = Pid::from_raw(100);
        let mut cache = MappingCache::new(pid);
        cache.insert(pid, PAGE, trusted());

        cache.replace_address_space(pid);

        assert!(
            cache.get(pid, PAGE).is_none(),
            "exec installs a new mm; the old classification describes a dead address space"
        );
    }

    /// A thread exiting must not evict pages its siblings still rely on, but the
    /// last user leaving must free them.
    #[test]
    fn cached_pages_outlive_one_thread_but_not_the_whole_group() {
        const PAGE: usize = 0x4000;
        let a = Pid::from_raw(100);
        let b = Pid::from_raw(101);
        let mut cache = MappingCache::new(a);
        cache.share_address_space(a, b);
        cache.insert(a, PAGE, trusted());

        cache.forget(b);
        assert!(
            cache.get(a, PAGE).is_some(),
            "a sibling exiting does not invalidate the surviving thread's view"
        );

        cache.forget(a);
        assert!(
            cache.entries.is_empty(),
            "the last user of an address space releases its cached pages"
        );
    }

    #[test]
    fn address_space_mutators_invalidate_and_others_do_not() {
        for nr in [
            libc::SYS_mmap,
            libc::SYS_munmap,
            libc::SYS_mremap,
            libc::SYS_mprotect,
            libc::SYS_brk,
            libc::SYS_shmat,
        ] {
            assert!(
                mutates_address_space(nr as u64),
                "syscall {nr} changes the address space and must invalidate the cache"
            );
        }
        for nr in [libc::SYS_read, libc::SYS_write, libc::SYS_getpid] {
            assert!(
                !mutates_address_space(nr as u64),
                "syscall {nr} cannot change a mapping and must not invalidate the cache"
            );
        }
    }
}
