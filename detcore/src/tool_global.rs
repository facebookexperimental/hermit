/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Detcore tool global state, and centralized methods corresponding to the centralized portion of
//! the Detcore tool.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::btree_map::Entry;
use std::fmt::Debug;
use std::fs;
use std::fs::File;
use std::io::Write;
use std::num::NonZeroUsize;
use std::os::fd::FromRawFd;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU16;
use std::sync::atomic::Ordering::SeqCst;
use std::task::Poll;
use std::time::SystemTime;

use anyhow::bail;
use chrono::DateTime;
use chrono::Utc;
use detcore_model::summary::RunSummary;
use detcore_model::summary::TimesliceStats;
use nix::sys::signal;
use nix::sys::signal::Signal;
use nix::unistd::Pid;
use reverie::GlobalRPC;
use reverie::GlobalTool;
use reverie::Guest;
use reverie::Tid;
use reverie::syscalls::AddrMut;
use reverie::syscalls::CloneFlags;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::Sysno;
use serde::Deserialize;
use serde::Serialize;
use tracing::debug;
use tracing::info;
use tracing::trace;
use tracing::warn;

use crate::config::Config;
use crate::consts::ROOT_DETPID;
use crate::ivar::Ivar;
use crate::preemptions::PreemptionReader;
use crate::preemptions::ThreadHistory;
use crate::record_or_replay::RecordOrReplay;
use crate::resources::ChaosEpochTransition;
use crate::resources::Permission;
use crate::resources::ResourceID;
use crate::resources::Resources;
use crate::scheduler::ConsumeResult;
use crate::scheduler::DEFAULT_PRIORITY;
use crate::scheduler::ExecReconnect;
use crate::scheduler::MaybePrintStack;
use crate::scheduler::Priority;
use crate::scheduler::SchedResponse;
use crate::scheduler::SchedValue;
use crate::scheduler::Scheduler;
use crate::scheduler::ThreadNextTurn;
use crate::scheduler::entropy_to_priority;
use crate::scheduler::runqueue::FIRST_PRIORITY;
use crate::scheduler::runqueue::LAST_PRIORITY;
use crate::scheduler::runqueue::REPLAY_DEFERRED_PRIORITY;
use crate::scheduler::runqueue::REPLAY_FOREGROUND_PRIORITY;
use crate::scheduler::runqueue::is_ordinary_priority;
use crate::scheduler::sched_loop;
use crate::scheduler::sched_loop_external;
use crate::tool_local::Detcore;
use crate::tool_local::ExecFdBlockingOverrides;
use crate::types::*;

async fn yield_once() {
    let mut yielded = false;
    std::future::poll_fn(|context| {
        if yielded {
            Poll::Ready(())
        } else {
            yielded = true;
            context.waker().wake_by_ref();
            Poll::Pending
        }
    })
    .await;
}

#[derive(Debug)]
struct InodePool {
    // TODO(T87258449): merge these two maps:
    inodes: HashMap<RawInode, DetInode>,
    detinodes_info: HashMap<DetInode, DetInodeInfo>,
    next_inode: RawInode,
}

/// Everything we know (globally) about a DetInode.
#[derive(Debug)]
struct DetInodeInfo {
    raw: RawInode,
    mtime: LogicalTime,
}

/// Everything the global scheduler needs to register a new child thread. A
/// normal clone is registered by the parent; a `CLONE_VFORK` child registers
/// itself (with `parent_is_kernel_blocked` set) because its parent is blocked
/// inside the kernel until the child execs or exits.
struct ChildRegistration {
    parent_dettid: DetTid,
    parent_detpid: DetPid,
    child_dettid: DetTid,
    child_tid_addr: usize,
    flags: Option<CloneFlags>,
    maybe_priority: Option<Priority>,
    parent_is_kernel_blocked: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PendingExecState {
    caller: DetTid,
    process: DetPid,
    mm: MmId,
    fd_blocking: ExecFdBlockingOverrides,
}

#[derive(Clone, Copy)]
struct RpcIncarnation {
    dettid: DetTid,
    mm: MmId,
}

impl Default for InodePool {
    fn default() -> Self {
        InodePool::new()
    }
}

impl InodePool {
    fn new() -> Self {
        InodePool {
            inodes: HashMap::new(),
            detinodes_info: HashMap::new(),
            next_inode: 1,
        }
    }

    // Allocate the next deterministic inode.  This takes the raw-inode and
    // can return an existing mapping or extend the mapping by creating a
    // new deterministic inode. The returned inode is strictly increasing
    // to avoid inode re-use issue in some filesystem like ext4.
    fn add_inode(&mut self, raw_inode: RawInode, mtime: LogicalTime) -> (DetInode, LogicalTime) {
        match self.inodes.get(&raw_inode) {
            None => {
                let new = self.next_inode;
                self.next_inode += 1;
                assert!(self.inodes.insert(raw_inode, new).is_none());
                let prev = self.detinodes_info.insert(
                    new,
                    DetInodeInfo {
                        raw: raw_inode,
                        mtime,
                    },
                );
                assert!(prev.is_none()); // Should not have been previously used.
                (new, mtime)
            }
            Some(dino) => {
                let info = self
                    .detinodes_info
                    .get(dino)
                    .expect("Internal invariant broken, det_ino missing entry");
                (*dino, info.mtime)
            }
        }
    }

    // remove a det inode
    fn remove_inode(&mut self, det_inode: DetInode) {
        if let Some(info) = self.detinodes_info.remove(&det_inode) {
            self.inodes.remove(&info.raw);
        }
    }
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-1056): Deterministic remapping of device numbers (st_dev).
/// Deterministic remapping of device numbers (`st_dev`).
///
/// The kernel assigns anonymous block-device numbers to filesystems without a
/// backing block device (procfs, sysfs, tmpfs, devpts) from a global,
/// host-wide counter (`get_anon_bdev`). The raw `st_dev` a guest observes for
/// such a filesystem therefore drifts between otherwise-identical runs — and
/// even between the two runs of `--verify`, because the first run's mounts are
/// still live when the second run mounts fresh copies, so the second run's
/// procfs gets a different anonymous device number. That leaked host state into
/// a guest-visible `stat`/`statx` field.
///
/// We replace each distinct raw device number with a strictly-increasing
/// synthetic id assigned in first-observation order. Because the guest's
/// sequence of `stat` calls is fixed by Detcore's deterministic schedule, the
/// order in which distinct devices are first seen is deterministic, so the
/// synthetic ids are stable across runs. The remapping preserves device
/// distinctness (distinct raw devices map to distinct ids) and consistency
/// (the same raw device always maps to the same id), so `find -xdev`, `du -x`,
/// and hardlink `(st_dev, st_ino)` identity checks still behave correctly.
#[derive(Debug)]
struct DevicePool {
    devices: HashMap<u64, u64>,
    next_device: u64,
}

impl Default for DevicePool {
    fn default() -> Self {
        DevicePool::new()
    }
}

impl DevicePool {
    fn new() -> Self {
        // Start at 1 so no file reports st_dev == 0, which some tools treat as
        // "no device".
        DevicePool {
            devices: HashMap::new(),
            next_device: 1,
        }
    }

    /// Return the deterministic device id for `raw_device`, allocating a new one
    /// (in first-observation order) the first time a raw device is seen.
    fn determinize(&mut self, raw_device: u64) -> u64 {
        match self.devices.get(&raw_device) {
            Some(dev) => *dev,
            None => {
                let new = self.next_device;
                self.next_device += 1;
                self.devices.insert(raw_device, new);
                new
            }
        }
    }
}

/// Global state associated with the detcore tool.
///
/// This is a singleton, and the one object of this type lives inside a central
/// address space, generally the "tracer" in a Reverie backend.
#[derive(Debug)]
pub struct GlobalState {
    sched: Arc<Mutex<Scheduler>>,

    inodes: Arc<Mutex<InodePool>>,

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1056): Deterministic st_dev remapping state.
    devices: Arc<Mutex<DevicePool>>,

    // next port to use if input port is 0
    next_port: AtomicU16,

    // used ports
    used_ports: Mutex<HashSet<u16>>,

    // Unsupported syscall names observed across every process in this run.
    unsupported_syscalls: Mutex<BTreeSet<String>>,

    // Optional append-only sink shared by DBI fork descendants.
    unsupported_syscall_report_fd: Option<Mutex<File>>,

    // Open file description to bound port.
    open_file_to_port: Mutex<HashMap<OpenFileId, u16>>,

    port_start_range: AtomicU16,
    port_end_range: AtomicU16,

    // False initially after fork, and true when we begin executing the guest binary.
    past_first_execve: AtomicBool,

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1154): Review the SaBRe exec descriptor-status handoff.
    /// Pre-exec identity and descriptor state awaiting a SaBRe exec reload.
    // TODO-HUMAN-REVIEW(PR-1173): Review SaBRe exec incarnation fencing.
    pending_exec_states: Mutex<BTreeMap<DetPid, PendingExecState>>,

    /// Descriptor state retained after the one-shot scheduler transition is consumed.
    post_exec_fd_blocking: Mutex<BTreeMap<DetTid, ExecFdBlockingOverrides>>,

    sched_handle: Option<tokio::task::JoinHandle<()>>,

    /// Global time is a *volatile* vector clock of individual thread progress. Each
    /// thread can independently update its own progress, even (potentially) asynchronously.
    ///
    /// LockOrdering: this lock can be acquired while holding the sched lock (but not vice
    /// versa).
    //
    // TODO: it would be more future-proof to provide a non-blocking way to retrieve a
    // (nondeterministic) monotonic lower bound on global time.
    global_time: Arc<Mutex<GlobalTime>>,

    /// Just cache the config so we can access it from everywhere.
    cfg: Config,

    /// Storage for the preemption record read from `replay_preemptions_from`.
    preemptions_to_replay: Option<PreemptionReader>,

    /// The start is when we construct the global state.  Close enough.
    realtime_start: SystemTime,
}

impl Default for GlobalState {
    fn default() -> Self {
        // TODO(T77816673): eventually we want to remove this requirement.
        // In the meantime... just don't call this.
        panic!("Detcore GlobalState Default impl should not be called");
    }
}

impl Drop for GlobalState {
    fn drop(&mut self) {
        // TODO-HUMAN-REVIEW(PR-643): Review shutdown-time aggregate warning delivery.
        if let Some(message) =
            format_unsupported_syscall_warning(&self.unsupported_syscalls.lock().unwrap())
        {
            warn!("{}", message);
        }
        info!("detcore shut down, destroying global state");
    }
}

impl GlobalState {
    fn initialize(cfg: &Config, spawn_scheduler: bool) -> Self {
        let sched = Arc::new(Mutex::new(Scheduler::new(cfg)));
        let global_time = Arc::new(Mutex::new(GlobalTime::new(cfg)));
        let handle = if cfg.sequentialize_threads && spawn_scheduler {
            Some(tokio::spawn(sched_loop(sched.clone(), global_time.clone())))
        } else {
            None
        };

        let preemptions_to_replay: Option<PreemptionReader> = cfg
            .replay_preemptions_from
            .as_ref()
            .map(|path| PreemptionReader::new(path));
        let range = Self::read_port_range();

        let unsupported_syscall_report_fd = cfg.unsupported_syscall_report_fd.and_then(|fd| {
            // This writer is internal controller state. In an in-process DBI
            // runtime it must not leak into the next guest image across exec
            // (hence F_DUPFD_CLOEXEC), and it must not perturb the descriptor
            // namespace the *current* guest observes. The backend places the
            // report fd itself high, out of the guest's working range (e.g. 199
            // for the DBI backend). Duplicating with a min hint of `fd` keeps
            // this private copy up in that same reserved band instead of
            // grabbing the lowest free descriptor (fd 3), which would shift
            // every fd the guest subsequently opens and diverge from the golden
            // ptrace reference (where this fd is unset and no dup happens).
            let duplicate = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, fd) };
            if duplicate == -1 {
                warn!(
                    "failed to duplicate unsupported-syscall report fd {fd}: {}",
                    std::io::Error::last_os_error()
                );
                None
            } else {
                // SAFETY: dup returned a new owned descriptor.
                Some(Mutex::new(unsafe { File::from_raw_fd(duplicate) }))
            }
        });

        Self {
            sched,
            next_port: AtomicU16::new(range[0]),
            used_ports: Mutex::new(HashSet::new()),
            unsupported_syscalls: Mutex::new(BTreeSet::new()),
            unsupported_syscall_report_fd,
            port_start_range: AtomicU16::new(range[0]),
            port_end_range: AtomicU16::new(range[1]),
            open_file_to_port: Mutex::new(HashMap::new()),
            past_first_execve: AtomicBool::new(false),
            pending_exec_states: Mutex::new(BTreeMap::new()),
            post_exec_fd_blocking: Mutex::new(BTreeMap::new()),
            inodes: Arc::new(Mutex::new(InodePool::new())),
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-1056): Deterministic st_dev remapping state.
            devices: Arc::new(Mutex::new(DevicePool::new())),
            sched_handle: handle,
            cfg: cfg.clone(),
            realtime_start: SystemTime::now(),
            global_time,
            preemptions_to_replay,
        }
    }

    /// Initializes global state whose sequential scheduler is driven by an
    /// external backend executor.
    pub fn init_for_external_scheduler(cfg: &Config) -> Self {
        assert!(
            cfg.sequentialize_threads,
            "an external scheduler is only meaningful when threads are sequentialized"
        );
        Self::initialize(cfg, false)
    }

    /// Runs the sequential scheduler on a backend-owned executor.
    pub async fn run_external_scheduler(&self, observer: Arc<dyn Fn(&'static str) + Send + Sync>) {
        sched_loop_external(self.sched.clone(), self.global_time.clone(), observer).await;
    }

    /// Reports that a backend supervisor received a process's final kernel exit status.
    ///
    /// This only records a barrier observation when the backend advertises physical-exit
    /// reporting; it is therefore a no-op for ptrace, DBI, KVM, and LiteInst execution. The
    /// exact process's barrier is released at this physical-waitability boundary.
    pub fn complete_physical_process_exit(&self, raw_pid: i32) {
        let detpid = DetPid::from_raw(raw_pid);
        self.pending_exec_states.lock().unwrap().remove(&detpid);
        self.post_exec_fd_blocking.lock().unwrap().remove(&detpid);
        if self
            .sched
            .lock()
            .unwrap()
            .complete_physical_process_exit(detpid)
        {
            trace!(
                "[detcore, dpid {}] backend completed final physical process exit",
                detpid
            );
        }
    }

    /// Releases all physical-process-exit barriers after a backend supervisor has drained every
    /// tracee and no guest thread can race another lifecycle event.
    pub fn release_all_physical_process_exits(&self) {
        self.pending_exec_states.lock().unwrap().clear();
        self.post_exec_fd_blocking.lock().unwrap().clear();
        let released = self
            .sched
            .lock()
            .unwrap()
            .release_all_physical_process_exits();
        if released != 0 {
            trace!("released {released} final physical process-exit barrier(s)");
        }
    }

    /// Unrecoverable fatal erorr. Bring things to a close cleanly, but as quickly as
    /// possible.
    pub fn force_shutdown_with_error(&self) {
        let start = std::time::Instant::now();
        let sched = loop {
            if start.elapsed().as_millis() > 1000 {
                eprintln!(
                    "Could not acquire scheduler lock during forced shutdown (timeout)... proceeding anyway."
                );
                return;
            }
            match self.sched.try_lock() {
                Ok(guard) => {
                    break guard;
                }
                Err(std::sync::TryLockError::WouldBlock) => {
                    std::thread::yield_now();
                    continue;
                }
                Err(e) => {
                    eprintln!(
                        "Could not acquire scheduler lock during forced shutdown ({})... proceeding anyway.",
                        e
                    );
                    return;
                }
            }
        };
        info!("Scheduler state at exit:\n{}", sched.full_summary());
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-744): Review explicit abnormal-backend scheduler cancellation.
    /// Cancels the internally spawned scheduler task after a backend guest exits abnormally.
    ///
    /// External-scheduler states do not own a task and are left unchanged.
    pub async fn cancel_internal_scheduler(&mut self) {
        if let Some(handle) = self.sched_handle.take() {
            handle.abort();
            match handle.await {
                Ok(()) => {}
                Err(error) if error.is_cancelled() => {}
                Err(error) => panic!("cancelled scheduler task panicked: {error}"),
            }
        }
    }

    /// Shut down anything running, in particular wait on the scheduler.
    ///
    /// This is basically the destructor for the global state, but is here rather than in the
    /// Drop instance so that it can be async, and is more explicitly sequenced in the program.
    ///
    /// Print a summary of the execution, typically called when it is complete.
    ///
    /// If the boolean argument is true, print to stderr, otherwise only print the summary
    /// to the log.
    pub async fn clean_up(mut self, to_stderr: bool, print_summary_to_json_file: &Option<PathBuf>) {
        if let Some(handle) = self.sched_handle.take() {
            debug!("Global state cleanup, confirming scheduler has shut down...");
            handle.await.expect("Global scheduler clean shutdown");
            debug!("Global state cleanup, continuing...");
        }
        let banner =
            "  ------------------------------ hermit run report ------------------------------";
        let mut summary = self.into_run_summary().unwrap();

        // Print machine-readable summary:
        if let Some(path) = print_summary_to_json_file {
            let json = serde_json::to_string_pretty(&summary).unwrap();
            fs::write(path, json + "\n").unwrap();
        }

        // Print human-readable summary:
        if to_stderr {
            // In this case, print summary irrespective of logging level.
            // TODO: output summary in machine-readable, JSON form.
            eprint!("{}\n{}", banner, summary);
        } else {
            // Separate out the nondeterministic bits and print them at debug level:
            let rt = summary.realtime_elapsed.take();
            info!("\n{}\n{}", banner, summary);
            if let Some(x) = rt {
                debug!("Nondeterministic realtime elapsed: {:?}", x);
            }
        }
    }

    fn into_run_summary(self) -> anyhow::Result<RunSummary> {
        // First, the scheduler can generate part of the summary
        let mut summary = {
            let mut sched = self.sched.lock().unwrap();
            sched.generate_partial_run_summary(self.cfg.record_preemptions_to.as_ref())?
        };
        // Second, we fill in the rest based on global state.
        //
        // Real time report:
        // N.B.: We don't have a job-level exit hook atm (T76248597), so we use the
        // CURRENT time -- that we are calling summarize -- as the end time:
        summary.realtime_elapsed = Some(self.realtime_start.elapsed()?);

        if self.cfg.virtualize_time {
            let final_time = self.global_time.lock().unwrap();
            let final_time_ns = final_time.as_nanos();
            let nanos = self
                .cfg
                .epoch
                .timestamp_nanos_opt()
                .expect("epoch cannot be represented in a timestamp with nanosecond precision")
                as u64;
            let epoch_ns = LogicalTime::from_nanos(nanos);
            summary.virttime_final = final_time_ns.as_nanos();
            summary.virttime_elapsed = if final_time_ns.as_nanos() >= epoch_ns.as_nanos() {
                (final_time_ns - epoch_ns).as_nanos()
            } else {
                bail!(
                    "Internal invariant violated! Global time is before epoch start {}",
                    epoch_ns
                );
            }
        }

        Ok(summary)
    }
}

#[reverie::global_tool]
impl GlobalTool for GlobalState {
    type Config = Config;

    /// A request asks the scheduler to perform an RPC, which includes multiple kinds of
    /// actions, and, most importantly, permission to acquire resources and run the guest thread.
    ///
    /// Irrespective of which method we execute, we can "tick" our local component of the
    /// global time in the process.
    type Request = (DetTime, MmId, GlobalRequest);

    /// Response from the global portion of the Detcore instrumentation tool.
    /// The exact form of the response depends on which method was executed.
    ///
    /// Irrespective of which method was called, the global handling may have consumed
    /// logical time, in which case the scheduler can send a new thread-local time back to
    /// the caller.  Unfortunately, information is lost as this is collapsed to a flat
    /// scalar instead of a rich `DetTime`.
    type Response = (Option<LogicalTime>, GlobalResponse);

    /// Called once during startup.
    async fn init_global_state(cfg: &Config) -> GlobalState {
        GlobalState::initialize(cfg, true)
    }

    async fn receive_rpc(&self, from: Tid, gr: Self::Request) -> Self::Response {
        type R = GlobalResponse;
        let dtid = DetTid::from_raw(from.into()); // TODO(T78538674): FIXME
        let (guest_time, request_mm, request) = gr;
        let time_from_guest = guest_time.as_nanos();
        let is_deregister = matches!(&request, GlobalRequest::DeregisterThread(_));

        let (exec_reconnect, is_exec_caller_after_local_mm_swap) = {
            let pending = self.pending_exec_states.lock().unwrap();
            let reconnect = match &request {
                GlobalRequest::CreateChildThread(child, process, _, None, _)
                    if *child == dtid && *child == *process =>
                {
                    pending.get(process).cloned()
                }
                _ => None,
            };
            let is_exec_caller_after_local_mm_swap = pending.values().any(|state| {
                state.caller == dtid && state.mm.for_exec(state.process) == request_mm
            });
            (reconnect, is_exec_caller_after_local_mm_swap)
        };

        // Tombstones reject raw Linux TID reuse except for the kernel-defined leader-TID takeover
        // recorded by a successful non-leader exec. Hold the scheduler admission lock through
        // clock accounting so logical teardown cannot linearize between the two.
        let mut tombstoned_deregistration = None;
        {
            let sched = self.sched.lock().unwrap();
            if exec_reconnect.is_none()
                && !is_exec_caller_after_local_mm_swap
                && !sched.rpc_incarnation_matches(dtid, request_mm)
            {
                debug!(
                    "[detcore, dtid {}] rejecting {:?} RPC from retired exec incarnation {:?}",
                    dtid, request, request_mm,
                );
                return if is_deregister {
                    (None, R::DeregisterThread(()))
                } else {
                    (None, R::ThreadExited)
                };
            }
            let child = match &request {
                GlobalRequest::CreateChildThread(child, ..)
                | GlobalRequest::CreateVforkChildThread(_, _, child, ..) => Some(*child),
                _ => None,
            };
            if sched.thread_is_logically_killed(dtid) && exec_reconnect.is_none() {
                trace!(
                    "[detcore, dtid {}] rejecting RPC after permanent logical-thread removal",
                    dtid
                );
                if let GlobalRequest::DeregisterThread(deregistration) = &request {
                    tombstoned_deregistration = Some(deregistration.clone());
                } else {
                    return (None, R::ThreadExited);
                }
            }
            if child.is_some_and(|child| sched.thread_is_logically_killed(child))
                && exec_reconnect.is_none()
            {
                trace!(
                    "[detcore, dtid {}] rejecting registration that reuses a tombstoned child TID",
                    dtid
                );
                return (None, R::ThreadExited);
            }

            let is_thread_reconnect = matches!(
                &request,
                GlobalRequest::StartNewThread(child_dettid, _) if *child_dettid == dtid
            ) && self.global_time.lock().unwrap().contains_thread(dtid);

            // This portion of the time updates "asynchronously", and we can tick it on every rpc:
            // TODO: eventually the vector clock should be in shared memory, and
            // the local clocks should update truly asynchrously.  Therefore it
            // SHOULD be safe to always push through this update on any rpc.
            if tombstoned_deregistration.is_none()
                && exec_reconnect.is_none()
                && !is_thread_reconnect
            {
                self.global_time
                    .lock()
                    .unwrap()
                    .update_global_time(dtid, time_from_guest);
            }
        }
        if let Some(deregistration) = tombstoned_deregistration {
            self.recv_deregister_thread(from, deregistration).await;
            return (None, R::DeregisterThread(()));
        }

        // RPC boilerplate. (Hard to generate systematically now though, because of the
        // time payload piggy-backing on each rpc. Maybe eventually once ticking a
        // threads' own clock happens through shared memory.)
        #[allow(clippy::unit_arg)]
        let resp = match request {
            GlobalRequest::RequestResources(rs, pid) => {
                let (response, _endtime) = self
                    .recv_request_resources(from, pid, rs, Some(request_mm))
                    .await;
                match response {
                    SchedulerRpcResult::Continue(response) => R::RequestResources(response),
                    SchedulerRpcResult::ThreadExited => R::ThreadExited,
                }
            }
            GlobalRequest::ReleaseResources(rs) => {
                R::ReleaseResources(self.recv_release_resources(from, rs).await)
            }
            GlobalRequest::ReleaseAllResources => {
                R::ReleaseAllResources(self.recv_release_all_resources(from).await)
            }
            // TODO-HUMAN-REVIEW(PR-643): Review run-wide unsupported-syscall aggregation.
            GlobalRequest::ReportUnsupportedSyscall(name) => {
                let inserted = self
                    .unsupported_syscalls
                    .lock()
                    .unwrap()
                    .insert(name.clone());
                if inserted
                    && let Some(report) = &self.unsupported_syscall_report_fd
                    && let Err(error) = writeln!(report.lock().unwrap(), "{name}")
                {
                    warn!("failed to append unsupported-syscall report: {error}");
                }
                R::ReportUnsupportedSyscall(())
            }
            GlobalRequest::PrepareExec(process, mm, fd_blocking) => {
                if mm != request_mm {
                    return (None, R::ThreadExited);
                }
                trace!(
                    "[detcore, dtid {}] preparing exec for process {} with mm {:?} and logically blocking descriptors {:?}",
                    dtid, process, mm, fd_blocking,
                );
                self.pending_exec_states.lock().unwrap().insert(
                    process,
                    PendingExecState {
                        caller: dtid,
                        process,
                        mm,
                        fd_blocking,
                    },
                );
                R::PrepareExec(())
            }
            GlobalRequest::CancelExec(process) => {
                let mut pending = self.pending_exec_states.lock().unwrap();
                if pending
                    .get(&process)
                    .is_some_and(|state| state.caller == dtid)
                {
                    pending.remove(&process);
                }
                R::CancelExec(())
            }
            GlobalRequest::MarkPastFirstExecve => {
                self.past_first_execve.store(true, SeqCst);
                let overrides = self
                    .post_exec_fd_blocking
                    .lock()
                    .unwrap()
                    .remove(&dtid)
                    .unwrap_or_default();
                trace!(
                    "[detcore, dtid {}] restoring logically blocking descriptors after exec: {:?}",
                    dtid, overrides,
                );
                R::MarkPastFirstExecve(overrides)
            }
            // Requested by the parent thread:
            GlobalRequest::CreateChildThread(dettid, parent_detpid, ctid, flags, priority) => {
                if let Some(prepared) = &exec_reconnect {
                    let (pending, post_exec_mm) = {
                        let mut states = self.pending_exec_states.lock().unwrap();
                        let Some(pending) = states.remove(&parent_detpid) else {
                            return (None, R::ThreadExited);
                        };
                        assert_eq!(&pending, prepared);
                        let post_exec_mm = pending.mm.for_exec(pending.process);
                        (pending, post_exec_mm)
                    };
                    assert_eq!(pending.process, parent_detpid);
                    let retired = self
                        .sched
                        .lock()
                        .unwrap()
                        .reconnect_after_exec(ExecReconnect {
                            caller: pending.caller,
                            new_leader: dettid,
                            detpid: parent_detpid,
                            pre_exec_mm: pending.mm,
                            post_exec_mm,
                            child_tid_addr: ctid,
                            reconnect_priority: priority,
                        });
                    if pending.caller != dettid {
                        self.global_time
                            .lock()
                            .unwrap()
                            .reassign_thread(pending.caller, dettid);
                    }
                    if !pending.fd_blocking.is_empty() {
                        self.post_exec_fd_blocking
                            .lock()
                            .unwrap()
                            .insert(dettid, pending.fd_blocking);
                    }
                    debug!(
                        "[detcore, dtid {}] reconciled successful exec from caller {}; retired prior identities {:?}",
                        dtid, pending.caller, retired
                    );
                    R::CreateChildThread(Some(post_exec_mm))
                } else {
                    match self
                        .recv_create_child_thread(
                            from,
                            request_mm,
                            ChildRegistration {
                                parent_dettid: DetTid::from_raw(from.into()),
                                parent_detpid,
                                child_dettid: dettid,
                                child_tid_addr: ctid,
                                flags,
                                maybe_priority: priority,
                                parent_is_kernel_blocked: false,
                            },
                        )
                        .await
                    {
                        SchedulerRpcResult::Continue(()) => R::CreateChildThread(None),
                        SchedulerRpcResult::ThreadExited => R::ThreadExited,
                    }
                }
            }
            // Requested by the vfork child on behalf of its kernel-blocked parent:
            GlobalRequest::CreateVforkChildThread(
                parent_dettid,
                parent_detpid,
                child_dettid,
                ctid,
                flags,
                priority,
            ) => match self
                .recv_create_child_thread(
                    from,
                    request_mm,
                    ChildRegistration {
                        parent_dettid,
                        parent_detpid,
                        child_dettid,
                        child_tid_addr: ctid,
                        flags: Some(flags),
                        maybe_priority: priority,
                        parent_is_kernel_blocked: true,
                    },
                )
                .await
            {
                SchedulerRpcResult::Continue(()) => R::CreateChildThread(None),
                SchedulerRpcResult::ThreadExited => R::ThreadExited,
            },
            // Requested by the child thread itself:
            GlobalRequest::StartNewThread(dettid, detpid) => {
                match self
                    .recv_start_new_thread(from, dettid, detpid, request_mm)
                    .await
                {
                    SchedulerRpcResult::Continue(history) => R::StartNewThread(history),
                    SchedulerRpcResult::ThreadExited => R::ThreadExited,
                }
            }
            GlobalRequest::DeregisterThread(deregistration) => {
                R::DeregisterThread(self.recv_deregister_thread(from, deregistration).await)
            }
            GlobalRequest::FutexAction(dettid, action, futexid, init_read, mask) => R::FutexAction(
                self.recv_futex_action(
                    RpcIncarnation {
                        dettid,
                        mm: request_mm,
                    },
                    action,
                    futexid,
                    init_read,
                    mask,
                )
                .await,
            ),
            GlobalRequest::DeterminizeInode(ino) => {
                R::DeterminizeInode(self.recv_determinize_inode(from, ino).await)
            }
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-1056): Deterministic st_dev remapping RPC.
            GlobalRequest::DeterminizeDevice(dev) => {
                R::DeterminizeDevice(self.recv_determinize_device(from, dev).await)
            }
            GlobalRequest::UnlinkInode(d_ino) => {
                R::UnlinkInode(self.recv_unlink_inode(from, d_ino).await)
            }
            GlobalRequest::TouchFile(ino) => R::TouchFile(self.recv_touch_file(from, ino).await),
            GlobalRequest::GlobalTimeLowerBound => {
                let ns = self.global_time.lock().unwrap().as_nanos();
                R::GlobalTimeLowerBound(ns)
            }
            GlobalRequest::TraceSchedEvent(ev, detpid) => {
                match self.recv_trace_schedevent(ev, detpid, request_mm).await {
                    SchedulerRpcResult::Continue(response) => R::TraceSchedEvent(response),
                    SchedulerRpcResult::ThreadExited => R::ThreadExited,
                }
            }
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(#663)
            // TODO-HUMAN-REVIEW(#869)
            GlobalRequest::RegisterAlarm(dpid, dtid, duration, interval, sig) => {
                let now = self.global_time.lock().unwrap().as_nanos();
                match self
                    .recv_register_alarm(
                        dpid,
                        RpcIncarnation {
                            dettid: dtid,
                            mm: request_mm,
                        },
                        now,
                        duration,
                        interval,
                        sig,
                    )
                    .await
                {
                    SchedulerRpcResult::Continue(remaining) => R::RegisterAlarm(remaining),
                    SchedulerRpcResult::ThreadExited => R::ThreadExited,
                }
            }
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-841): Review logical alarm query RPC.
            GlobalRequest::AlarmRemaining(dpid) => {
                let now = self.global_time.lock().unwrap().as_nanos();
                R::AlarmRemaining(self.sched.lock().unwrap().alarm_remaining(dpid, now))
            }
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(#869)
            GlobalRequest::RegisterPosixTimer(dpid, dtid, timer_id, deadline, interval, sig) => {
                match self
                    .recv_register_posix_timer(
                        dpid,
                        RpcIncarnation {
                            dettid: dtid,
                            mm: request_mm,
                        },
                        timer_id,
                        deadline,
                        interval,
                        sig,
                    )
                    .await
                {
                    SchedulerRpcResult::Continue(()) => R::RegisterPosixTimer(()),
                    SchedulerRpcResult::ThreadExited => R::ThreadExited,
                }
            }
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(#663)
            GlobalRequest::ResolveKillTargets(dpid) => {
                R::ResolveKillTargets(self.sched.lock().unwrap().process_signal_targets(dpid))
            }
            GlobalRequest::UnrecoverableShutdown => {
                self.force_shutdown_with_error();
                R::UnrecoverableShutdown(())
            }
            GlobalRequest::RequestPort(open_file_id) => {
                let mut mut_used_ports = self.used_ports.lock().unwrap();
                self.update_port_range();
                let total_available =
                    self.port_end_range.load(SeqCst) - self.port_start_range.load(SeqCst);
                let mut index = 0;
                while (*mut_used_ports).contains(&self.next_port.load(SeqCst))
                    && index < total_available
                {
                    self.next_port.fetch_add(1, SeqCst);
                    if self.next_port.load(SeqCst) > self.port_end_range.load(SeqCst) {
                        self.next_port
                            .store(self.port_start_range.load(SeqCst), SeqCst);
                    }
                    index += 1;
                }
                if index == total_available {
                    R::PortFull
                } else {
                    (*mut_used_ports).insert(self.next_port.load(SeqCst));
                    let mut open_file_to_port = self.open_file_to_port.lock().unwrap();
                    open_file_to_port.insert(open_file_id, self.next_port.load(SeqCst));
                    R::RequestPort(self.next_port.load(SeqCst))
                }
            }
            GlobalRequest::AddUsedPort(port, open_file_id) => {
                let mut used_ports = self.used_ports.lock().unwrap();
                used_ports.insert(port);
                let mut open_file_to_port = self.open_file_to_port.lock().unwrap();
                open_file_to_port.insert(open_file_id, port);
                R::AddUsedPort
            }
            GlobalRequest::ReleasePort(open_file_id) => {
                let mut used_ports = self.used_ports.lock().unwrap();
                let mut open_file_to_port = self.open_file_to_port.lock().unwrap();
                let port = open_file_to_port.remove(&open_file_id);
                if let Some(port) = port {
                    used_ports.remove(&port);
                }
                R::ReleasePort(port)
            }
        };

        // Awaited scheduler operations may have raced logical teardown. Never return their
        // operation-specific response after the sender acquired a permanent tombstone.
        let sender_became_terminal =
            if is_deregister || exec_reconnect.is_some() || is_exec_caller_after_local_mm_swap {
                false
            } else {
                let sched = self.sched.lock().unwrap();
                sched.thread_is_logically_killed(dtid)
                    || !sched.rpc_incarnation_matches(dtid, request_mm)
            };
        if resp == R::ThreadExited || sender_became_terminal {
            return (None, R::ThreadExited);
        }

        let time_from_sched = self.global_time.lock().unwrap().threads_time(dtid);
        let time_update = match time_from_sched.cmp(&time_from_guest) {
            Ordering::Equal => None,
            Ordering::Less => {
                panic!(
                    "internal error: thread time should never go down, only monotonically up: time in sched {}, thread local time was {}",
                    time_from_sched, time_from_guest
                )
            }
            Ordering::Greater => Some(time_from_sched),
        };
        (time_update, resp)
    }
}

impl GlobalState {
    async fn recv_request_resources(
        &self,
        from: Tid,
        detpid: DetPid,
        rs: Resources,
        request_mm: Option<MmId>,
    ) -> (SchedulerRpcResult<ResumeStatus>, Option<LogicalTime>) {
        let dettid = DetTid::from_raw(from.into()); // TODO(T78538674): FIXME

        let resp2 = {
            let mut sched = self.sched.lock().unwrap();
            if sched.thread_is_logically_killed(dettid)
                || request_mm.is_some_and(|mm| !sched.rpc_incarnation_matches(dettid, mm))
            {
                return (SchedulerRpcResult::ThreadExited, None);
            }
            let Some(nextturn) = sched.next_turns.get(&dettid).cloned() else {
                panic!(
                    "Detcore internal error: no entry for dettid {} in next_turns during resource request.",
                    dettid
                );
            };
            trace!(
                "[detcore, dtid {}] ResourceRequest, filling request into {}",
                &dettid, &nextturn.req
            );
            sched.request_put(&nextturn.req, rs.clone(), &self.global_time);
            nextturn.resp
        };
        trace!(
            "[detcore, dtid {}] waiting on {} for resources: {:?}",
            dettid, &resp2, rs
        );
        let answer = resp2.get().await; // Block on the scheduler allowing our guest to proceed.
        let request_became_stale = {
            let sched = self.sched.lock().unwrap();
            sched.thread_is_logically_killed(dettid)
                || request_mm.is_some_and(|mm| !sched.rpc_incarnation_matches(dettid, mm))
        };
        if request_became_stale {
            // `logically_kill_thread` wakes an already-pending request with a
            // signal response.  Treat that wake-up as terminal: otherwise a
            // caller that ignores `ResumeStatus::Signaled` can inject the
            // original syscall after the thread was logically removed.
            // TODO-HUMAN-REVIEW(PR-1023): Review pending SaBRe resource-request cancellation.
            trace!(
                "[detcore, dtid {}] terminating pending request after logical removal",
                dettid
            );
            return (SchedulerRpcResult::ThreadExited, None);
        }
        if let Some((true, process, mm)) = rs.exit_identity() {
            info!(
                "Scheduler authorized an exit-group scenario, from dettid {} / detpid {}",
                dettid, detpid
            );
            // Before allowing an `exit_group` to physically proceed, we
            // deregister the other threads in the thread group to reflect the
            // fact that they will not receive further logical turns.
            //
            // We trust the kernel to physically kill them irrespective of what they're
            // blocked on, including us having blocked them in the `futex_waiters` list.
            {
                let mut sched = self.sched.lock().unwrap();
                if sched.thread_is_logically_killed(dettid)
                    || request_mm.is_some_and(|mm| !sched.rpc_incarnation_matches(dettid, mm))
                {
                    return (SchedulerRpcResult::ThreadExited, None);
                }
                for tid in sched.thread_tree.my_thread_group(&dettid) {
                    // We don't need to do anything extra for our own thread. That can use the
                    // same mechanics as a normal exit:
                    if tid != dettid {
                        sched.logically_kill_thread(&tid, &process, mm);
                    }
                }
            }
        }

        match answer {
            // In this context, SchedValue
            SchedResponse::Go(Some(schedval)) => {
                trace!(
                    "[dtid {}] resources granted, resuming normally: {:?}",
                    dettid, rs
                );

                let endtime_update = match schedval {
                    // Only syscalls timeout, and they don't need to update guest timeslice end.
                    SchedValue::TimeOut => None,
                    SchedValue::Value(timeslice) => Some(LogicalTime::from_nanos(timeslice)),
                };
                (
                    SchedulerRpcResult::Continue(ResumeStatus::Normal),
                    endtime_update,
                )
            }
            SchedResponse::Go(None) => {
                trace!(
                    "[dtid {}] resources granted but no timeslice specified",
                    dettid,
                );
                (SchedulerRpcResult::Continue(ResumeStatus::Normal), None)
            }
            SchedResponse::Signaled() => {
                trace!(
                    "[dtid {}] resources granted but interrupted by signal",
                    dettid,
                );
                (SchedulerRpcResult::Continue(ResumeStatus::Signaled), None)
            }
        }
    }

    async fn recv_release_resources(&self, from: Tid, rs: Resources) {
        // TODO(T78627377): add real resource-locking when we enable backgrounding actions.
        trace!("[detcore] Resources released to pid {}: {:?}", from, rs);
    }

    async fn recv_release_all_resources(&self, from: Tid) {
        // TODO(T78627377): add real resource-locking when we enable backgrounding actions.
        trace!("[detcore] All resources held by pid {} released", from);
    }

    /// Global portion of parent-forking-child protocol.  Called by the parent
    /// thread for an ordinary clone, or by the child itself for a vfork whose
    /// parent is blocked inside the kernel (`parent_is_kernel_blocked`).
    async fn recv_create_child_thread(
        &self,
        rpc_sender: Tid,
        request_mm: MmId,
        registration: ChildRegistration,
    ) -> SchedulerRpcResult<()> {
        let ChildRegistration {
            parent_dettid,
            parent_detpid,
            child_dettid,
            child_tid_addr: ctid,
            flags,
            maybe_priority,
            parent_is_kernel_blocked,
        } = registration;
        let initial_priority = if let Some(pr) = &self.preemptions_to_replay {
            assert!(maybe_priority.is_none());
            let prio = pr
                .thread_initial_priority(&child_dettid)
                .unwrap_or_else(|| {
                    warn!(
                        "Child thread {} not found in preemption history to replay",
                        child_dettid
                    );
                    DEFAULT_PRIORITY
                });
            if !is_ordinary_priority(prio) {
                panic!(
                    "Read a bad initial_prority from file: {}\nFull file: {}",
                    prio,
                    pr.load_all(),
                );
            }
            prio
        } else {
            let prio = maybe_priority.expect(
                "create_child_thread must take an initial priority unless replaying preemptions",
            );
            if !is_ordinary_priority(prio) {
                panic!(
                    "recv_create_child_thread received a bad prority argument : {}",
                    prio,
                );
            }
            prio
        };

        {
            let mut sched = self.sched.lock().unwrap();
            let sender = DetTid::from_raw(rpc_sender.into());
            if sched.thread_is_logically_killed(sender)
                || !sched.rpc_incarnation_matches(sender, request_mm)
                || sched.thread_is_logically_killed(child_dettid)
            {
                return SchedulerRpcResult::ThreadExited;
            }

            if parent_is_kernel_blocked && self.cfg.sequentialize_threads {
                sched.complete_vfork_registration(parent_dettid, child_dettid);
            }

            // Don't fill in the request, as the child will do it:
            let _entry = sched
                .next_turns
                .entry(child_dettid)
                .or_insert_with(|| ThreadNextTurn {
                    dettid: child_dettid,
                    child_tid_addr: ctid,
                    req: Ivar::new(),
                    resp: Ivar::new(),
                });

            {
                let is_group_leader = if let Some(f) = flags {
                    !f.contains(CloneFlags::CLONE_THREAD)
                } else {
                    true // root thread
                };
                sched
                    .thread_tree
                    .add_child(parent_dettid, child_dettid, is_group_leader);
            }

            if self.cfg.replay_schedule_from.is_none() {
                // Give the thread an initial priority
                let old_prio = sched.priorities.insert(child_dettid, initial_priority);
                assert!(old_prio.is_none());
            } else {
                // In replay mode, the context switch point will already have initialized the priority.
                // UNLESS this is the root thread, in which case we need to fill it in:
                if let std::collections::btree_map::Entry::Vacant(entry) =
                    sched.priorities.entry(child_dettid)
                {
                    assert_eq!(parent_detpid, ROOT_DETPID);
                    entry.insert(initial_priority);
                }
            }

            if let Some(pr) = &mut sched.preemption_writer {
                pr.register_thread(child_dettid, initial_priority);
            }

            let child_first = self.cfg.sequentialize_threads
                && !parent_is_kernel_blocked
                && sched.child_runs_first_post_fork(self.cfg.runs_post_fork);
            let pos = if child_first {
                sched.runqueue_push_front(child_dettid)
            } else {
                sched.runqueue_push_back(child_dettid)
            };
            debug!(
                "[detcore] CreateChildThread with dtid {}: Added child to {} of priority band, position {}.",
                child_dettid,
                if child_first { "front" } else { "back" },
                pos,
            );
            sched.started_up.try_put(());
        }
        // The child queue position above determines which equal-priority side
        // gets the first turn when the parent requests ParentContinue.
        // A vfork parent is already blocked by the kernel and is not in the run
        // queue, so it must not issue a ParentContinue request here.
        if self.cfg.sequentialize_threads && !parent_is_kernel_blocked {
            let mut rs = Resources::new(parent_detpid);
            rs.insert(
                ResourceID::ParentContinue {
                    parent: parent_dettid,
                    child: child_dettid,
                },
                Permission::W,
            );
            if matches!(
                self.recv_request_resources(rpc_sender, parent_detpid, rs, Some(request_mm))
                    .await
                    .0,
                SchedulerRpcResult::ThreadExited
            ) {
                return SchedulerRpcResult::ThreadExited;
            }
        }
        SchedulerRpcResult::Continue(())
    }

    /// Called by the child thread upon startup.
    /// Returns a thread-preemption history for the new guest thread (if --replay-preemptions-from
    /// is used).
    async fn recv_start_new_thread(
        &self,
        from: Tid,
        dettid: DetTid,
        detpid: DetPid,
        request_mm: MmId,
    ) -> SchedulerRpcResult<Option<ThreadHistory>> {
        let mut tries: u64 = 0;
        // TODO: eliminate this loop. Could instead signal with an ivar.
        let response_ivar = loop {
            yield_once().await;
            let mut sched = self.sched.lock().unwrap();
            if sched.thread_is_logically_killed(dettid)
                || !sched.rpc_incarnation_matches(dettid, request_mm)
            {
                return SchedulerRpcResult::ThreadExited;
            }
            // The resources that must be held for the fresh thread to run:
            let rsrcs = {
                let mut s = HashMap::new();
                s.insert(ResourceID::MemAddrSpace(detpid), Permission::RW); // TODO(T78055411): track mem aliasing.
                Resources {
                    tid: dettid,
                    resources: s,
                    poll_attempt: 0,
                    fyi: String::new(),
                }
            };
            let nextturn = match sched.next_turns.entry(dettid) {
                Entry::Vacant(_entry) => {
                    // CreateChildThread on the parent hasn't run yet.

                    // TODO: We could try to populate the entry since we get here
                    // first, but currently we lack the information right here to
                    // populate the child_tid_addr field.
                    if tries == 0 {
                        trace!(
                            "[detcore, dtid {}] thread showed up early, no queue entry yet.  Waiting...",
                            dettid
                        );
                    }
                    tries += 1;
                    continue;
                }
                Entry::Occupied(entry) => {
                    trace!(
                        "[detcore, dtid {}] handling StartNewThread rpc.  Found next_turns entry (after {} tries)",
                        from, tries
                    );
                    entry.get().clone()
                }
            };
            sched.request_put(&nextturn.req, rsrcs, &self.global_time);
            break nextturn.resp;
        };
        debug!(
            "[detcore, dtid {}] New thread will now wait for response on {}...",
            &dettid, &response_ivar
        );
        let _answer = response_ivar.get().await;
        let request_became_stale = {
            let sched = self.sched.lock().unwrap();
            sched.thread_is_logically_killed(dettid)
                || !sched.rpc_incarnation_matches(dettid, request_mm)
        };
        if request_became_stale {
            return SchedulerRpcResult::ThreadExited;
        }
        info!(
            "[detcore, dtid {}] New thread given go-ahead to proceed via {}",
            &dettid, &response_ivar
        );
        if let Some(pr) = &self.preemptions_to_replay {
            let history = pr.extract_thread_record(&dettid).unwrap_or_else(|| {
                warn!(
                    "Replaying preemptions, but no record found for thread {}",
                    dettid
                );
                ThreadHistory::new()
            });
            let old_prio = {
                let mut sched = self.sched.lock().unwrap();
                if sched.thread_is_logically_killed(dettid)
                    || !sched.rpc_incarnation_matches(dettid, request_mm)
                {
                    return SchedulerRpcResult::ThreadExited;
                }
                sched.priorities.insert(dettid, history.initial_priority())
            };
            debug!(
                "[replay-preemption] Enqueing new thread at priority {:?} (changed from {:?})",
                history.initial_priority(),
                old_prio,
            );
            SchedulerRpcResult::Continue(Some(history))
        } else {
            SchedulerRpcResult::Continue(None)
        }
    }

    /// Warning: this happens completely asynchronously, whenever the guest exit hook fires.
    /// Its timing is not coordinated by the scheduler.
    async fn recv_deregister_thread(&self, _from: Tid, deregistration: ThreadDeregistration) {
        let ThreadDeregistration {
            dettid,
            detpid,
            mm,
            timeslice_stats,
            chaos_epochs,
        } = deregistration;
        // A fatal signal can tear down the caller after its local state has advanced to the
        // candidate exec image but before the successful reconnect (or failed-exec cancel).
        // Retire that one-shot preparation before scheduler-incarnation admission rejects the
        // candidate image's final cleanup RPC.
        let mut pending = self.pending_exec_states.lock().unwrap();
        if pending.get(&detpid).is_some_and(|state| {
            state.caller == dettid && (mm == state.mm || mm == state.mm.for_exec(state.process))
        }) {
            pending.remove(&detpid);
        }
        drop(pending);

        // Invariant: will only be called when sequentialize-threads is on.
        assert!(self.cfg.sequentialize_threads);
        let mut sched = self.sched.lock().unwrap();
        if !sched.rpc_incarnation_matches(dettid, mm) {
            debug!(
                "[detcore, dtid {}] ignoring deregistration from retired exec incarnation {:?}",
                dettid, mm,
            );
            return;
        }
        self.post_exec_fd_blocking.lock().unwrap().remove(&dettid);
        if !sched.note_deregistration_accounted(dettid) {
            trace!(
                "[detcore, dtid {}] acknowledging already-accounted deregistration",
                dettid
            );
            return;
        }
        if let Some(writer) = &mut sched.preemption_writer {
            for transition in chaos_epochs {
                writer.insert_chaos_epoch(dettid, transition);
            }
        }
        sched.record_timeslice_stats(dettid, timeslice_stats);
        if !sched.thread_is_logically_killed(dettid) {
            sched.logically_kill_thread(&dettid, &detpid, mm);
        }
        drop(sched);
        trace!(
            "[detcore, dtid {}] thread deregistered, removed from sched structures.",
            dettid
        );
    }

    async fn recv_futex_action(
        &self,
        caller: RpcIncarnation,
        action: FutexAction,
        futexid: FutexID,
        init_read: i32,
        mask: u32,
    ) -> Option<SchedValue> {
        let RpcIncarnation { dettid, mm } = caller;
        trace!("[detcore, dtid {}] Futex action: {:?}", &dettid, action);
        let response_iv = {
            let mut sched = self.sched.lock().unwrap();
            if sched.thread_is_logically_killed(dettid)
                || !sched.rpc_incarnation_matches(dettid, mm)
            {
                return Some(SchedValue::Value(nix::errno::Errno::EINTR as u64));
            }
            let Some(resp_iv) = sched
                .next_turns
                .get(&dettid)
                .map(|nextturn| nextturn.resp.clone())
            else {
                // AUTONOMOUS-BOT-IMPLEMENTED
                // TODO-HUMAN-REVIEW(PR-845): Review late RPCs from exit-group siblings.
                trace!(
                    "[detcore, dtid {}] ignoring futex action after logical thread removal",
                    dettid
                );
                return Some(SchedValue::Value(nix::errno::Errno::EINTR as u64));
            };
            match action {
                FutexAction::WaitRequest(maybe_timeout) => {
                    if sched.child_tid_was_cleared(futexid, init_read) {
                        trace!(
                            "[detcore, dtid {}] late wait on cleared child-TID futex {:?}",
                            dettid, futexid
                        );
                        return Some(SchedValue::Value(0));
                    }
                    sched.sleep_futex_waiter(&dettid, futexid, maybe_timeout, mask);
                    // block on ivar, below
                }
                FutexAction::WaitFinished => {
                    return None;
                }
                FutexAction::WakeRequest(num_threads) => {
                    let num = sched.wake_futex_waiters(dettid, futexid, num_threads, mask);
                    return Some(SchedValue::Value(num));
                }
                FutexAction::WakeFinished(_num_threads) => {
                    return None;
                }
            }
            // Blocking on the FUTEX_WAIT here, remove ourselves:
            assert!(sched.run_queue.remove_tid(dettid));
            resp_iv
        };
        // Wait for wake+scheduler response.
        match response_iv.get().await {
            SchedResponse::Go(answer) => {
                trace!(
                    "[detcore, dtid {}] Unblocked from futex_wait! ({})",
                    &dettid, &response_iv
                );
                answer
            }
            SchedResponse::Signaled() => Some(SchedValue::Value(nix::errno::Errno::EINTR as u64)),
        }
    }

    async fn recv_determinize_inode(&self, from: Tid, ino: RawInode) -> (DetInode, LogicalTime) {
        // Here we establish a policy that when we first see a file its mtime is epoch.
        let nanos = self
            .cfg
            .epoch
            .timestamp_nanos_opt()
            .expect("epoch cannot be represented in a timestamp with nanosecond precision")
            as u64;
        let (dino, ns) = self
            .inodes
            .lock()
            .unwrap()
            .add_inode(ino, LogicalTime::from_nanos(nanos));
        trace!(
            "[detcore, dtid {}] resolved (raw) inode {:?} to {:?}, mtime {}",
            from, ino, dino, ns
        );
        (dino, ns)
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1056): Deterministic st_dev remapping RPC.
    async fn recv_determinize_device(&self, from: Tid, raw_device: u64) -> u64 {
        let det_device = self.devices.lock().unwrap().determinize(raw_device);
        trace!(
            "[detcore, dtid {}] resolved (raw) device {} to {}",
            from, raw_device, det_device
        );
        det_device
    }

    async fn recv_unlink_inode(&self, from: Tid, d_ino: DetInode) {
        trace!("[detcore, dtid {}] unlink (det) inode {:?}", from, d_ino);
        self.inodes.lock().unwrap().remove_inode(d_ino);
    }

    async fn recv_touch_file(&self, from: Tid, ino: RawInode) {
        let mtime = if self.cfg.virtualize_time {
            self.global_time.lock().unwrap().as_nanos()
        } else {
            // In this scenario, virtualize_metadata is set and virtualize_time isn't.
            // We virtualize initial mtimes, but update using realtime.
            let dt: DateTime<Utc> = Utc::now();
            let nanos = dt.timestamp_nanos_opt().expect(
                "current time cannot be represented in a timestamp with nanosecond precision",
            ) as u64;
            LogicalTime::from_nanos(nanos)
        };
        trace!(
            "[dtid {}] bumping mtime on file (rawinode {:?}) to {}",
            from, ino, mtime,
        );
        let mut mg = self.inodes.lock().unwrap();
        let dino =
            if let Some(d) = mg.inodes.get(&ino) {
                *d
            } else {
                // Otherwise we haven't seen this inode yet (e.g. because there hasnt been a
                // stat on it), so we just-in-time add it.
                let nanos =
                    self.cfg.epoch.timestamp_nanos_opt().expect(
                        "epoch cannot be represented in a timestamp with nanosecond precision",
                    ) as u64;
                let (d, _) = mg.add_inode(ino, LogicalTime::from_nanos(nanos));
                d
            };
        let info = mg
            .detinodes_info
            .get_mut(&dino)
            // TODO(T87258449): remove this `expect`:
            .expect("Invariant violation: det inode missing from map.");
        info.mtime = mtime;
    }

    async fn recv_trace_schedevent(
        &self,
        ev: SchedEvent,
        detpid: DetPid,
        request_mm: MmId,
    ) -> SchedulerRpcResult<TraceSchedEventResponse> {
        if !self
            .sched
            .lock()
            .unwrap()
            .rpc_incarnation_matches(ev.dettid, request_mm)
        {
            return SchedulerRpcResult::ThreadExited;
        }
        // TODO(T124316762): debug address randomization in the tracer and get rid of this hack:
        let ev = {
            if self.past_first_execve.load(SeqCst) {
                ev
            } else {
                info!("Warning: erasing rip of pre-execve sched event! {:?}", ev);
                SchedEvent {
                    end_rip: None,
                    start_rip: None,
                    ..ev
                }
            }
        };
        // Future trace_schedevent calls will retain their rip values.
        if ev.op == Op::Syscall(Sysno::execve, SyscallPhase::Prehook) {
            self.past_first_execve.store(true, SeqCst);
        }

        // Yield this guest thread if needed to follow schedule.
        let result = if self.cfg.replay_schedule_from.is_some() {
            let (consumed, print_stack2) = {
                let mut sched = self.sched.lock().unwrap();
                if sched.thread_is_logically_killed(ev.dettid)
                    || !sched.rpc_incarnation_matches(ev.dettid, request_mm)
                {
                    return SchedulerRpcResult::ThreadExited;
                }
                let consumed = sched.consume_schedevent(&ev);
                let print_stack2 = if self.cfg.record_preemptions {
                    sched.record_event(&ev)
                } else {
                    None
                };
                (consumed, print_stack2)
            };
            let ConsumeResult {
                keep_running,
                print_stack,
                event_ix: _,
                timeslice_remaining: mut end_of_timeslice,
            } = consumed;
            trace!(
                "keep_running :{}, end_of_timeslice: {:?}",
                keep_running, end_of_timeslice
            );

            if !keep_running {
                trace!(
                    "[detcore, dtid {}] Thread yielding to follow replay schedule",
                    &ev.dettid,
                );
                let tid = reverie::Tid::from(ev.dettid.as_raw()); // TODO(T78538674): virtualize pid/tid:
                let mut rsrcs = Resources::new(ev.dettid);
                rsrcs.insert(ResourceID::TraceReplay, Permission::RW);
                let (response, timeslice) = self
                    .recv_request_resources(tid, detpid, rsrcs, Some(request_mm))
                    .await;
                if response == SchedulerRpcResult::ThreadExited {
                    return SchedulerRpcResult::ThreadExited;
                }
                end_of_timeslice = timeslice;
                trace!(
                    "[detcore, dtid {}] Thread reactivated after yielding for replay schedule",
                    &ev.dettid,
                );
            }

            TraceSchedEventResponse {
                print_stack_strace: print_stack.or(print_stack2),
                timeslice: end_of_timeslice,
            }
        } else {
            let print_stack_strace = {
                let mut sched = self.sched.lock().unwrap();
                if sched.thread_is_logically_killed(ev.dettid)
                    || !sched.rpc_incarnation_matches(ev.dettid, request_mm)
                {
                    return SchedulerRpcResult::ThreadExited;
                }
                if self.cfg.record_preemptions {
                    sched.record_event(&ev)
                } else {
                    None
                }
            };
            TraceSchedEventResponse {
                print_stack_strace,
                timeslice: None,
            }
        };

        if result.print_stack_strace.is_some()
            && let Some(sig) = &self.cfg.stacktrace_signal
        {
            trace!(
                "[dtid {}] signaling thread with {} at the point of stack trace printing.",
                ev.dettid, sig.0
            );
            let tid = Pid::from_raw(ev.dettid.as_raw());
            // TODO(T78538674): virtualize pid/tid:
            // We send a raw signal here and let the guest pick it up WHENEVER it resumes.
            // We don't use the "signal_guest" method because we don't necessarily respect that
            // protocol here.
            signal::kill(tid, sig.0).unwrap();
        }

        SchedulerRpcResult::Continue(result)
    }

    // Ephemeral port range is in file /proc/sys/net/ipv4/ip_local_port_range"
    // This function reads from the file and returns the range
    // Start of range is at index 0, end of range is at index 1.
    fn read_port_range() -> Vec<u16> {
        let contents = fs::read_to_string("/proc/sys/net/ipv4/ip_local_port_range")
            .expect("File should be present");
        let range: Vec<u16> = contents
            .split_whitespace()
            .filter_map(|number| number.parse().ok())
            .collect();
        range
    }

    // Reflect ephemeral port range updated outside of the tracer program internally.
    fn update_port_range(&self) {
        let range = Self::read_port_range();
        self.port_start_range.store(range[0], SeqCst);
        self.port_end_range.store(range[1], SeqCst);
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#663)
    // TODO-HUMAN-REVIEW(#869)
    /// Register an alarm (delayed signal delivery) with the global scheduler.
    async fn recv_register_alarm(
        &self,
        detpid: DetPid,
        caller: RpcIncarnation,
        now: LogicalTime,
        duration: LogicalTime,
        interval: LogicalTime,
        sig: SigWrapper,
    ) -> SchedulerRpcResult<(LogicalTime, LogicalTime)> {
        let RpcIncarnation { dettid, mm } = caller;
        let mut sched = self.sched.lock().unwrap();
        if sched.thread_is_logically_killed(dettid) || !sched.rpc_incarnation_matches(dettid, mm) {
            return SchedulerRpcResult::ThreadExited;
        }
        SchedulerRpcResult::Continue(
            sched.register_alarm(detpid, dettid, now, duration, interval, sig.0),
        )
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#869)
    /// Register, re-arm, or disarm a POSIX timer in the global scheduler.
    async fn recv_register_posix_timer(
        &self,
        detpid: DetPid,
        caller: RpcIncarnation,
        timer_id: i32,
        deadline: Option<LogicalTime>,
        interval: LogicalTime,
        sig: SigWrapper,
    ) -> SchedulerRpcResult<()> {
        let RpcIncarnation { dettid, mm } = caller;
        let mut sched = self.sched.lock().unwrap();
        if sched.thread_is_logically_killed(dettid) || !sched.rpc_incarnation_matches(dettid, mm) {
            return SchedulerRpcResult::ThreadExited;
        }
        sched.register_posix_timer(detpid, dettid, timer_id, deadline, interval, sig.0);
        SchedulerRpcResult::Continue(())
    }
}

/// Identity and final accounting for an asynchronous scheduler deregistration.
#[derive(PartialEq, Debug, Eq, Clone, Serialize, Deserialize)]
pub struct ThreadDeregistration {
    pub(crate) dettid: DetTid,
    pub(crate) detpid: DetPid,
    pub(crate) mm: MmId,
    pub(crate) timeslice_stats: TimesliceStats,
    pub(crate) chaos_epochs: Vec<ChaosEpochTransition>,
}

/// Messages to the global object.
///
/// This is public only so it can be used in the `GlobalTool` trait.
/// It should NOT be used by any client outside of this file.
#[derive(PartialEq, Debug, Eq, Clone, Serialize, Deserialize)]
#[allow(clippy::enum_variant_names)]
pub enum GlobalRequest {
    /// Lock the resources
    /// Also contains the `DetPid` of the process containing the thread requesting resources.
    RequestResources(Resources, DetPid),
    /// Release the locks
    ReleaseResources(Resources),
    /// For convenience, release all the resources held by the current TID.
    ReleaseAllResources,

    // TODO-HUMAN-REVIEW(PR-643): Review this new Detcore global RPC request.
    /// Add a syscall to the run-wide unsupported-use summary.
    ReportUnsupportedSyscall(String),

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1154): Review the SaBRe exec descriptor-status handoff.
    /// Save the caller, address-space identity, and logically blocking descriptors before a
    /// backend reloads its tool across exec.
    PrepareExec(DetPid, MmId, ExecFdBlockingOverrides),

    /// Clear the saved transition after an exec attempt returns with an error.
    CancelExec(DetPid),

    /// Mark the initial image transition complete for backends that begin post-exec.
    MarkPastFirstExecve,

    /// The parent is adding a child-thread to the round-robin pool.  Contains the dettid
    /// of the new child and it's starting scheduler priority IF it is available to the caller.
    /// The only scenario where the Priority will be missing is when we're replaying preemptions.
    /// In that case it is the global state that holds the information regarding the new thread's
    /// initial priority.
    CreateChildThread(DetTid, DetPid, usize, Option<CloneFlags>, Option<Priority>),

    /// A vfork child registering itself while its parent is blocked inside the
    /// kernel. Contains the (real) parent dettid and detpid, the child dettid,
    /// the child TID address, the clone flags, and the starting priority (absent
    /// only when replaying preemptions).
    CreateVforkChildThread(DetTid, DetPid, DetTid, usize, CloneFlags, Option<Priority>),

    /// New thread is alive and waiting to run its first instruction.  Contains the dettid
    /// and detpid of the new child.
    StartNewThread(DetTid, DetPid),

    /// Remove a thread from scheduler data structures, guaranteeing that it will
    /// consume no further turns. Carries its final timeslice distribution and any
    /// chaos-epoch transitions not yet flushed by a priority-change commit.
    DeregisterThread(ThreadDeregistration),

    /// Notify scheduler before/after futex action.
    /// The last two arguments are the initial contents of the memory word, and the mask.
    FutexAction(DetTid, FutexAction, FutexID, i32, u32),

    /// Translate nondeterministic to deterministic inode.
    DeterminizeInode(RawInode),

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1056): Deterministic st_dev remapping RPC.
    /// Translate a nondeterministic (host-assigned) device number to a
    /// deterministic one.
    DeterminizeDevice(u64),

    /// unlink an inode
    UnlinkInode(DetInode),

    /// Bump mtime
    TouchFile(RawInode),

    /// Retrieve global time.
    GlobalTimeLowerBound,

    /// Record scheduling event in a total order.
    TraceSchedEvent(SchedEvent, DetPid),

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#663)
    // TODO-HUMAN-REVIEW(#869)
    /// Basically performs an alarm syscall, takes a logical duration.
    RegisterAlarm(DetPid, DetTid, LogicalTime, LogicalTime, SigWrapper),

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#869)
    /// Register, re-arm, or disarm one POSIX timer.
    RegisterPosixTimer(
        DetPid,
        DetTid,
        i32,
        Option<LogicalTime>,
        LogicalTime,
        SigWrapper,
    ),

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-841): Review logical alarm query RPC.
    /// Return the logical time remaining on a process's one-shot alarm.
    AlarmRemaining(DetPid),

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#663)
    /// Query live threads before translating process-directed signal delivery.
    ResolveKillTargets(DetPid),

    /// The container is shutting down.  Exit the scheduler "thread".
    UnrecoverableShutdown,

    // Request a port for an open file description.
    RequestPort(OpenFileId),

    // Add a port to the used-port list for an open file description.
    AddUsedPort(u16, OpenFileId),

    // Release the port when the last alias of its open file description closes.
    ReleasePort(OpenFileId),
}

/// Responses from the global object
#[allow(missing_docs, clippy::unit_arg)]
#[derive(PartialEq, Debug, Eq, Clone, Serialize, Deserialize)]
pub enum GlobalResponse {
    /// The scheduler permanently removed this raw TID. Guest-side RPC handling consumes this by
    /// tail-injecting a thread exit before any per-operation caller can resume.
    ThreadExited,
    RequestResources(ResumeStatus),
    ReleaseResources(()),
    ReleaseAllResources(()),
    // TODO-HUMAN-REVIEW(PR-643): Review this new Detcore global RPC response.
    ReportUnsupportedSyscall(()),
    PrepareExec(()),
    CancelExec(()),
    MarkPastFirstExecve(ExecFdBlockingOverrides),
    CreateChildThread(Option<MmId>),
    /// Includes optional preemption points for the new thread.
    StartNewThread(Option<ThreadHistory>),
    DeregisterThread(()),
    FutexAction(Option<SchedValue>),
    /// Return the mtime as well:
    DeterminizeInode((DetInode, LogicalTime)),
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1056): Deterministic st_dev remapping RPC.
    DeterminizeDevice(u64),
    UnlinkInode(()),
    TouchFile(()),
    GlobalTimeLowerBound(LogicalTime),
    TraceSchedEvent(TraceSchedEventResponse),
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#663)
    // TODO-HUMAN-REVIEW(#869)
    RegisterAlarm((LogicalTime, LogicalTime)),
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#869)
    RegisterPosixTimer(()),
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-841): Review logical alarm query RPC.
    AlarmRemaining(LogicalTime),
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#663)
    ResolveKillTargets(Vec<DetTid>),
    // TODO: use void_send_rpc, and remove this bogus response:
    UnrecoverableShutdown(()),

    RequestPort(u16),
    AddUsedPort,
    ReleasePort(Option<u16>),
    PortFull,
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-644): Review the shared warning formatter API.
/// Formats one deterministic warning for a set of unsupported syscall names.
pub fn format_unsupported_syscall_warning(syscalls: &BTreeSet<String>) -> Option<String> {
    if syscalls.is_empty() {
        None
    } else {
        Some(format!(
            "syscalls {} used but not yet supported",
            syscalls.iter().cloned().collect::<Vec<_>>().join(",")
        ))
    }
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-1154): Review the SaBRe exec descriptor-status handoff.
pub async fn prepare_exec<G, T>(guest: &mut G, mm: MmId, fd_blocking: ExecFdBlockingOverrides)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let detpid = guest.thread_state().detpid.expect("detpid unset");
    let (_, response) =
        send_and_update_time(guest, GlobalRequest::PrepareExec(detpid, mm, fd_blocking)).await;
    assert_eq!(response, GlobalResponse::PrepareExec(()));
}

pub async fn cancel_exec<G, T>(guest: &mut G)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let detpid = guest.thread_state().detpid.expect("detpid unset");
    let (_, response) = send_and_update_time(guest, GlobalRequest::CancelExec(detpid)).await;
    assert_eq!(response, GlobalResponse::CancelExec(()));
}

pub async fn mark_past_first_execve<G, T>(guest: &mut G)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let (_, response) = send_and_update_time(guest, GlobalRequest::MarkPastFirstExecve).await;
    let overrides = match response {
        GlobalResponse::MarkPastFirstExecve(overrides) => overrides,
        _ => unreachable!(),
    };
    if !overrides.is_empty() {
        let dettid = guest.thread_state().dettid;
        let metadata = Arc::clone(&guest.thread_state().file_metadata);
        metadata
            .lock()
            .unwrap()
            .apply_exec_blocking_overrides(dettid, overrides);
    }
}

// TODO-HUMAN-REVIEW(PR-643): Review the guest-to-global unsupported-syscall report path.
pub async fn report_unsupported_syscall<G, T>(guest: &mut G, sysno: Sysno)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let (_, response) = send_and_update_time(
        guest,
        GlobalRequest::ReportUnsupportedSyscall(sysno.to_string()),
    )
    .await;
    assert_eq!(response, GlobalResponse::ReportUnsupportedSyscall(()));
}

pub async fn send_and_update_time<G, T>(
    guest: &mut G,
    request: GlobalRequest,
) -> (Option<LogicalTime>, GlobalResponse)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let mytime = guest.thread_state().thread_logical_time.clone();
    let mm = guest.thread_state().mm_id;
    let resp = guest.send_rpc((mytime, mm, request)).await;
    if resp.1 == GlobalResponse::ThreadExited {
        let dettid = guest.thread_state().dettid;
        trace!(
            "[detcore, dtid {}] exiting after terminal scheduler cancellation",
            dettid
        );
        // The terminal response must never return to an operation-specific RPC caller. Reverie
        // SaBRe runs exactly-once Tool cleanup for this non-original thread exit, then executes the
        // raw exit without restoring the callback's guest frame.
        guest.tail_inject(reverie::syscalls::Exit::default()).await
    }
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-845): Review applying the coordinator clock after exec reload.
    if let Some(time) = resp.0 {
        guest
            .thread_state_mut()
            .thread_logical_time
            .advance_to(time);
    }
    resp
}

/// When the thread resumes after a potentially-blocking scheduler request, is it a normal
/// continuation of execution, or is it because the thread will now execute a signal handler.
/// If the latter, that interrupts logically blocking syscalls that were in progress.
#[derive(PartialEq, Debug, Eq, Clone, Serialize, Deserialize)]
pub enum ResumeStatus {
    Normal,
    Signaled,
}

/// Internal result of a scheduler operation. Terminal results become
/// [`GlobalResponse::ThreadExited`] before returning to the guest-side RPC helper.
#[derive(PartialEq, Debug, Eq, Clone)]
enum SchedulerRpcResult<T> {
    Continue(T),
    ThreadExited,
}

/// Global method RPC to request to control a resource.
///
/// Blocking: future returns only when resources are fully acquired.
pub async fn resource_request<G, T>(guest: &mut G, r: Resources) -> ResumeStatus
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    if guest.config().sequentialize_threads {
        let dettid = guest.thread_state().dettid;
        let detpid = guest.thread_state().detpid.expect("detpid unset");
        trace!(
            "[detcore, dtid {}] BLOCKING on resource_request rpc... {:?}",
            &dettid, r
        );
        let resp =
            send_and_update_time(guest, GlobalRequest::RequestResources(r.clone(), detpid)).await;
        match resp.1 {
            GlobalResponse::RequestResources(x) => {
                trace!(
                    "[detcore, dtid {}] UNBLOCKED, acquired resources: {:?}",
                    &dettid, r
                );
                x
            }
            _ => unreachable!(),
        }
    } else {
        ResumeStatus::Normal
    }
}

/// Global method RPC to release all held resources.
///
/// Nonblocking: future may return immediately before the central global object has
/// processed the resource release.
pub async fn resource_release_all<G, T>(guest: &mut G)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    if guest.config().sequentialize_threads {
        let resp = send_and_update_time(guest, GlobalRequest::ReleaseAllResources).await;
        match resp.1 {
            GlobalResponse::ReleaseAllResources(x) => x,
            _ => unreachable!(),
        }
    }
}

/// Global method RPC to allow a new thread to begin execution, called from the child thread.
///
/// Blocking: future returns only when the thread execution is truly ready to proceed.
///
/// Returns: a history of the thread preemptions, for it to play back when --replay-preemptions-from
/// is used.
pub async fn thread_start_request<G, T>(
    cfg: &Config,
    guest: &mut G,
    detpid: DetPid,
) -> Option<ThreadHistory>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let dettid = guest.thread_state().dettid;
    if cfg.sequentialize_threads {
        trace!("[detcore, dtid {}] new thread BLOCKING on rpc...", &dettid);
        let resp = send_and_update_time(guest, GlobalRequest::StartNewThread(dettid, detpid)).await;
        match resp.1 {
            GlobalResponse::StartNewThread(preempts) => {
                trace!("[detcore, dtid {}] new thread UNBLOCKED (post-rpc)", dettid);
                preempts
            }
            _ => unreachable!(),
        }
    } else {
        None
    }
}

/// Global method RPC for the parent to add a child-thread to the round-robin pool.
///
/// Nonblocking: future returning does not guarantee anything about the central scheduler,
/// except that it will eventually give a slot to the child.  Then the protocol is that
/// child will subsequently make a `thread_start_request` to gate the start of its execution.
pub async fn create_child_thread<G, T>(
    guest: &mut G,
    child_dettid: DetTid,
    ctid: usize,
    flags: Option<CloneFlags>,
) -> Option<MmId>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    // Random (or replayed) starting priority in chaos mode, constant priority otherwise.
    let starting_priority = if guest.config().replay_preemptions_from.is_some() {
        // In preemption replay mode, the initial priority is set on the other
        // side of the rpc, in recv_create_child_thread.
        None
    } else if guest.config().replay_schedule_from.is_some() {
        // FIXME!  Find a cleaner way to make the root thread start off high-priority:
        if child_dettid <= DetTid::from_raw(3) {
            Some(REPLAY_FOREGROUND_PRIORITY)
        } else {
            Some(REPLAY_DEFERRED_PRIORITY)
        }
    } else if guest.config().chaos {
        let entropy = guest
            .thread_state_mut()
            .chaos_prng_next_u64("child_priority");
        if guest.config().chaos_target_races {
            // Targeted chaos: bias a freshly created child to an extreme priority
            // so it either runs before the parent resumes or strictly after it,
            // instead of landing at a uniformly random priority. This maximizes
            // parent/child ordering divergence to surface fork/exec races.
            // Reproducible under `--fuzz-seed`/`--sched-seed`.
            if entropy.is_multiple_of(2) {
                Some(FIRST_PRIORITY)
            } else {
                Some(LAST_PRIORITY)
            }
        } else {
            Some(entropy_to_priority(entropy))
        }
    } else {
        Some(DEFAULT_PRIORITY)
    };

    let detpid = guest.thread_state().detpid.expect("detpid unset");

    let resp = send_and_update_time(
        guest,
        GlobalRequest::CreateChildThread(child_dettid, detpid, ctid, flags, starting_priority),
    )
    .await;
    match resp.1 {
        GlobalResponse::CreateChildThread(x) => x,
        _ => unreachable!(),
    }
}

/// Register a vfork child while its parent is blocked inside `clone(2)`.
///
/// Unlike an ordinary clone, the parent cannot perform this registration
/// because the kernel has blocked it until the child execs or exits. The child
/// therefore registers itself, carrying the inherited parent identity, flags,
/// and starting priority. The starting priority is derived the same way as an
/// ordinary clone so that chaos and replay scheduling stay deterministic.
pub async fn create_vfork_child_thread<G, T>(
    guest: &mut G,
    child_dettid: DetTid,
    vfork: crate::tool_local::PendingVfork,
) where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let starting_priority = if guest.config().replay_preemptions_from.is_some() {
        None
    } else if guest.config().replay_schedule_from.is_some() {
        Some(if child_dettid <= DetTid::from_raw(3) {
            REPLAY_FOREGROUND_PRIORITY
        } else {
            REPLAY_DEFERRED_PRIORITY
        })
    } else if guest.config().chaos {
        Some(entropy_to_priority(vfork.child_priority_entropy.expect(
            "vfork child priority entropy missing in chaos mode",
        )))
    } else {
        // POSIX vfork suspends the parent until the child execs or _exits. Give
        // the vfork child a strictly higher priority (lower number) than the
        // parent's DEFAULT_PRIORITY so the deterministic scheduler always runs
        // the child first, rather than round-robining the parent and child at
        // equal priority (which leaves fork/exec ordering nondeterministic).
        Some(DEFAULT_PRIORITY - 1)
    };

    let resp = send_and_update_time(
        guest,
        GlobalRequest::CreateVforkChildThread(
            vfork.parent_dettid,
            vfork.parent_detpid,
            child_dettid,
            vfork.child_tid_addr,
            vfork.flags,
            starting_priority,
        ),
    )
    .await;
    match resp.1 {
        GlobalResponse::CreateChildThread(_) => (),
        _ => unreachable!(),
    }
}

/// Remove the thread from the scheduler.
///
/// Nonblocking: the future may return immediately, not guaranteeing the changes to the
/// scheduler have been completed.
pub(crate) async fn deregister_thread<R>(
    threads_time: DetTime,
    cfg: &Config,
    reverie: &R,
    thread: ThreadDeregistration,
) where
    // Note, this is called from a context where we DON'T have a full, operable `Guest`.
    R: GlobalRPC<GlobalState>,
{
    if cfg.sequentialize_threads {
        let mm = thread.mm;
        // TODO: void_send_rpc
        let resp = reverie
            .send_rpc((threads_time, mm, GlobalRequest::DeregisterThread(thread)))
            .await;
        // We can't update the thread time here.  But it's dead anyway!
        match resp.1 {
            GlobalResponse::DeregisterThread(x) => x,
            _ => unreachable!(),
        }
    }
}

/// Which actions we can take before/after a futex system call.
#[derive(PartialEq, Debug, Eq, Clone, Copy, Serialize, Deserialize)]
pub enum FutexAction {
    /// Check in before a FUTEX_WAIT, including an optional timeout.
    WaitRequest(Option<LogicalTime>),
    /// Check in after a FUTEX_WAIT
    WaitFinished,
    /// Check in before a FUTEX_WAKE, parameterized by the number of threads woken.
    WakeRequest(i32),
    /// Check in after a FUTEX_WAKE, parameterized by the number of threads woken.
    WakeFinished(i32),
}

/// Ask scheduler for permission to proceed before/after futex operation.
/// Returns true if the operation completed normally, and false if it timed out.
pub async fn futex_action<G, T>(
    guest: &mut G,
    futex_action: FutexAction,
    futexid: &FutexID,
    init_read: i32,
    mask: u32,
) -> Option<SchedValue>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    assert!(guest.config().sequentialize_threads);
    let dettid = guest.thread_state().dettid;
    let req = GlobalRequest::FutexAction(dettid, futex_action, *futexid, init_read, mask);
    trace!(
        "BLOCKING on futex_action: sending request to scheduler: {:?}",
        req
    );
    // Update local time from potentially blocking operation:
    let resp = send_and_update_time(guest, req.clone()).await;
    match resp.1 {
        GlobalResponse::FutexAction(answer) => {
            trace!("UNBLOCKING after futex_action. Request was: {:?}", req);
            answer
        }
        _ => unreachable!(),
    }
}

/// track a (possibly new) inode, by returning a deterministic inode.
/// Also return the logical mtime for the inode, though this is only
/// used if `virtualize_metadata` is set.
pub async fn determinize_inode<G, T>(guest: &mut G, inode: RawInode) -> (DetInode, LogicalTime)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let resp = send_and_update_time(guest, GlobalRequest::DeterminizeInode(inode)).await;
    match resp.1 {
        GlobalResponse::DeterminizeInode(x) => x,
        _ => unreachable!(),
    }
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-1056): Deterministic st_dev remapping RPC.
/// Translate a host-assigned device number (`st_dev`) to a deterministic one.
pub async fn determinize_device<G, T>(guest: &mut G, raw_device: u64) -> u64
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let resp = send_and_update_time(guest, GlobalRequest::DeterminizeDevice(raw_device)).await;
    match resp.1 {
        GlobalResponse::DeterminizeDevice(x) => x,
        _ => unreachable!(),
    }
}

/// unlink a detfd, i.e. When `unlink` a file
#[allow(unused)]
pub async fn unlink_inode<G, T>(guest: &mut G, d_ino: DetInode)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let resp = send_and_update_time(guest, GlobalRequest::UnlinkInode(d_ino)).await;
    match resp.1 {
        GlobalResponse::UnlinkInode(x) => x,
        _ => unreachable!(),
    }
}

/// Update the modification time for a file, using its inode.
/// This will set the mtime to a coherent global-time value.
pub async fn touch_file<G, T>(guest: &mut G, inode: RawInode)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let resp = send_and_update_time(guest, GlobalRequest::TouchFile(inode)).await;
    match resp.1 {
        GlobalResponse::TouchFile(x) => x,
        _ => unreachable!(),
    }
}

/// Read the global clock, or at least a deterministic lower bound on it.
pub async fn global_time_lower_bound<G, T>(guest: &mut G) -> LogicalTime
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let resp = send_and_update_time(guest, GlobalRequest::GlobalTimeLowerBound).await;
    match resp.1 {
        GlobalResponse::GlobalTimeLowerBound(x) => x,
        _ => unreachable!(),
    }
}

/// Take a time observation from the current thread. This extra indirection
/// helps abstract over whether or not we need to use local or global
/// information for this.
pub async fn thread_observe_time<G, T>(guest: &mut G) -> LogicalTime
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    global_time_lower_bound(guest).await
}

/// Writes a structured json backtrace to a given file
fn write_backtrace<G, T>(guest: &mut G, m_path: Option<&PathBuf>)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    if let Some(backtrace) = guest.backtrace() {
        if let Some(path) = m_path {
            let file = File::create(path).expect("Failed to open preemption stacktrace log file");
            serde_json::to_writer(file, &backtrace.force_pretty()).unwrap();
        } else {
            eprintln!("{}", backtrace.force_pretty());
        }
    } else {
        warn!("Could not read backtrace!");
    }
}

/// Additional instructions to a guest after shed events is consumed by global tool
#[derive(PartialEq, Debug, Eq, Clone, Serialize, Deserialize)]
pub struct TraceSchedEventResponse {
    print_stack_strace: MaybePrintStack,
    timeslice: Option<LogicalTime>,
}

/// Record an event in the schedule trace, OR check the event on replay.
/// This also prints the backtrace of the schedevent, if indicated.
///
/// Arguments:
/// - tag_end_rip: read the current guest registers to fill in the `end_rip` on the event with the
///   current instruction pointer.
pub async fn trace_schedevent<G, T>(guest: &mut G, ev: SchedEvent, tag_end_rip: bool)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    assert!(guest.config().sequentialize_threads);

    // trace_schedevent is called AFTER the event is complete, and the rip is resting just after it.
    let ev = if tag_end_rip {
        let end_rip = if let Some(r) = ev.end_rip {
            r
        } else {
            let regs = guest.regs().await;
            NonZeroUsize::new(regs.rip.try_into().unwrap()).unwrap()
        };
        SchedEvent {
            end_rip: Some(end_rip),
            ..ev
        }
    } else {
        ev
    };

    if let Some(rip) = ev.end_rip {
        let rip_addr = AddrMut::<u16>::from_raw(rip.into()).unwrap();
        let rip_contents: u16 = guest
            .memory()
            .read_value(rip_addr)
            .expect("memory read succeeds");
        trace!(
            "Tracing sched event, after which rip is {}, next two instruction bytes {:#06x}",
            rip, rip_contents
        );
    }

    let detpid = guest.thread_state().detpid.expect("detpid unset");
    let resp = send_and_update_time(guest, GlobalRequest::TraceSchedEvent(ev, detpid)).await;

    trace!("trace_schedevent result: {:?}", resp);
    match resp {
        (
            _,
            GlobalResponse::TraceSchedEvent(TraceSchedEventResponse {
                print_stack_strace,
                timeslice,
            }),
        ) => {
            if let Some(m_path) = print_stack_strace {
                trace!("[trace_schedevent] writing stacktrace via Reverie...");
                write_backtrace(guest, m_path.as_ref());
            }

            if let Some(timeslice) = timeslice
                && guest.thread_state().past_global_first_execve
            {
                let end_of_timeslice =
                    guest.thread_state().thread_logical_time.as_nanos() + timeslice;
                trace!(
                    "[detcore][dettid {}] setting end_of_timeslice to {:?} as instructed by replayer",
                    guest.thread_state().dettid,
                    end_of_timeslice
                );
                guest.thread_state_mut().end_of_timeslice = Some(end_of_timeslice);
                if guest.config().max_timeslice.is_some() {
                    guest.thread_state_mut().max_timeslice_end = Some(end_of_timeslice);
                }
            }
        }
        _ => {
            unreachable!()
        }
    }
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(#663)
// TODO-HUMAN-REVIEW(#869)
/// Register an alarm (delayed signal delivery) with the global scheduler.
/// Returns the logical duration remaining until any previously scheduled alarm.
pub async fn register_alarm<G, T>(
    guest: &mut G,
    duration: LogicalTime,
    interval: LogicalTime,
    sig: Signal,
) -> (LogicalTime, LogicalTime)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let dettid = guest.thread_state().dettid;
    let detpid = guest.thread_state().detpid.expect("detpid unset");
    let resp = send_and_update_time(
        guest,
        GlobalRequest::RegisterAlarm(detpid, dettid, duration, interval, SigWrapper(sig)),
    )
    .await;
    match resp.1 {
        GlobalResponse::RegisterAlarm(x) => x,
        _ => unreachable!(),
    }
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-841): Review logical alarm query API.
/// Return the logical duration remaining on the process's one-shot alarm.
pub async fn alarm_remaining<G, T>(guest: &mut G) -> LogicalTime
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let detpid = guest.thread_state().detpid.expect("detpid unset");
    let resp = send_and_update_time(guest, GlobalRequest::AlarmRemaining(detpid)).await;
    match resp.1 {
        GlobalResponse::AlarmRemaining(remaining) => remaining,
        _ => unreachable!(),
    }
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(#869)
/// Register, re-arm, or disarm a POSIX timer with the global scheduler.
pub async fn register_posix_timer<G, T>(
    guest: &mut G,
    timer_id: i32,
    deadline: Option<LogicalTime>,
    interval: LogicalTime,
    sig: Signal,
) where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let dettid = guest.thread_state().dettid;
    let detpid = guest.thread_state().detpid.expect("detpid unset");
    let resp = send_and_update_time(
        guest,
        GlobalRequest::RegisterPosixTimer(
            detpid,
            dettid,
            timer_id,
            deadline,
            interval,
            SigWrapper(sig),
        ),
    )
    .await;
    match resp.1 {
        GlobalResponse::RegisterPosixTimer(()) => {}
        _ => unreachable!(),
    }
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(#663)
/// Return the scheduler's live threads for a positive process ID.
pub async fn resolve_kill_targets<G, T>(guest: &mut G, detpid: DetPid) -> Vec<DetTid>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let response = send_and_update_time(guest, GlobalRequest::ResolveKillTargets(detpid)).await;
    match response.1 {
        GlobalResponse::ResolveKillTargets(targets) => targets,
        _ => unreachable!(),
    }
}

/// Signal an unrecoverable error that exits the entire container.
/// Such exits are not determinizable (see "quasi-determinism").
pub async fn unrecoverable_shutdown<G, T>(guest: &G) -> !
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    if cfg!(debug_assertions) {
        let mytime = guest.thread_state().thread_logical_time.clone();
        let mm = guest.thread_state().mm_id;
        // TODO: void_send_rpc
        let _ = guest
            .send_rpc((mytime, mm, GlobalRequest::UnrecoverableShutdown))
            .await;
    }

    // In this scenario a backtrace doesn't really help us.
    std::process::exit(1);
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::os::fd::AsRawFd;
    use std::os::fd::FromRawFd;
    use std::os::fd::OwnedFd;
    use std::time::Duration;

    use nix::sys::signal::Signal;
    use reverie::GlobalTool;

    use super::FutexAction;
    use super::GlobalRequest;
    use super::GlobalResponse;
    use super::GlobalState;
    use super::PendingExecState;
    use super::RpcIncarnation;
    use super::SchedulerRpcResult;
    use super::SigWrapper;
    use super::ThreadDeregistration;
    use super::TimesliceStats;
    use super::format_unsupported_syscall_warning;
    use crate::config::Config;
    use crate::ivar::Ivar;
    use crate::preemptions::PreemptionRecord;
    use crate::resources::Resources;
    use crate::scheduler::DEFAULT_PRIORITY;
    use crate::scheduler::SchedRequest;
    use crate::scheduler::SchedResponse;
    use crate::scheduler::SchedValue;
    use crate::scheduler::ThreadNextTurn;
    use crate::tool_local::ExecFdBlockingOverrides;
    use crate::types::DetPid;
    use crate::types::DetTid;
    use crate::types::DetTime;
    use crate::types::FutexID;
    use crate::types::LogicalTime;
    use crate::types::MmId;
    use crate::types::Op;
    use crate::types::SchedEvent;

    fn cancellation_test_state() -> (Config, GlobalState, DetTid, DetPid) {
        let config = Config {
            sequentialize_threads: true,
            cancel_killed_thread_rpcs: true,
            ..Config::default()
        };
        let state = GlobalState::initialize(&config, false);
        let dettid = DetTid::from_raw(17);
        let detpid = DetPid::from_raw(17);
        state
            .sched
            .lock()
            .unwrap()
            .thread_tree
            .add_child(dettid, dettid, true);
        (config, state, dettid, detpid)
    }

    fn install_test_registration(state: &GlobalState, dettid: DetTid, request: Ivar<SchedRequest>) {
        let mut scheduler = state.sched.lock().unwrap();
        assert!(!scheduler.thread_is_logically_killed(dettid));
        scheduler.next_turns.insert(
            dettid,
            ThreadNextTurn {
                dettid,
                child_tid_addr: 0,
                req: request,
                resp: Ivar::new(),
            },
        );
        scheduler.priorities.insert(dettid, DEFAULT_PRIORITY);
        scheduler.runqueue_push_back(dettid);
    }

    #[test]
    fn live_registration_without_next_turn_is_not_terminal() {
        let (_, state, dettid, detpid) = cancellation_test_state();
        install_test_registration(&state, dettid, Ivar::new());
        state.sched.lock().unwrap().next_turns.remove(&dettid);

        assert!(
            !state
                .sched
                .lock()
                .unwrap()
                .thread_is_logically_killed(dettid),
            "transient next-turn absence must not imply logical death"
        );

        state
            .sched
            .lock()
            .unwrap()
            .logically_kill_thread(&dettid, &detpid, MmId::initial(detpid));
        assert!(
            state
                .sched
                .lock()
                .unwrap()
                .thread_is_logically_killed(dettid),
            "explicit logical death must install a permanent TID tombstone"
        );
    }

    #[tokio::test]
    async fn exec_reconnect_retires_siblings_and_reuses_live_scheduler_and_clock_state() {
        let (config, state, dettid, detpid) = cancellation_test_state();
        let old_mm = MmId::initial(detpid).for_exec(detpid);
        install_test_registration(&state, dettid, Ivar::new());
        let sibling = DetTid::from_raw(dettid.as_raw() + 1);
        let sibling_request = Ivar::new();
        state
            .sched
            .lock()
            .unwrap()
            .thread_tree
            .add_child(dettid, sibling, false);
        install_test_registration(&state, sibling, sibling_request.clone());
        {
            let mut scheduler = state.sched.lock().unwrap();
            scheduler.next_turns.get_mut(&dettid).unwrap().resp =
                Ivar::full(SchedResponse::Go(None));
            scheduler
                .next_turns
                .get_mut(&sibling)
                .unwrap()
                .child_tid_addr = 0x1234;
        }
        let mut existing_time = DetTime::new(&config);
        existing_time.add_syscall();
        existing_time.add_syscall();
        state
            .global_time
            .lock()
            .unwrap()
            .update_global_time(dettid, existing_time.as_nanos());
        let (global_before, thread_before) = {
            let global_time = state.global_time.lock().unwrap();
            (global_time.as_nanos(), global_time.threads_time(dettid))
        };
        let fresh_local_time = DetTime::new(&config);
        state.pending_exec_states.lock().unwrap().insert(
            detpid,
            PendingExecState {
                caller: dettid,
                process: detpid,
                mm: old_mm,
                fd_blocking: Default::default(),
            },
        );
        state
            .sched
            .lock()
            .unwrap()
            .install_test_exec_incarnation(dettid, old_mm);
        let in_flight_exec_response = state
            .receive_rpc(
                reverie::Tid::from_raw(dettid.as_raw()),
                (
                    existing_time.clone(),
                    old_mm.for_exec(detpid),
                    GlobalRequest::ReportUnsupportedSyscall("exec-in-flight".to_owned()),
                ),
            )
            .await;
        assert_eq!(
            in_flight_exec_response.1,
            GlobalResponse::ReportUnsupportedSyscall(())
        );

        let create_response = state
            .receive_rpc(
                reverie::Tid::from_raw(dettid.as_raw()),
                (
                    fresh_local_time.clone(),
                    MmId::initial(dettid),
                    GlobalRequest::CreateChildThread(
                        dettid,
                        detpid,
                        0,
                        None,
                        Some(DEFAULT_PRIORITY),
                    ),
                ),
            )
            .await;
        assert_eq!(
            create_response,
            (
                Some(thread_before),
                GlobalResponse::CreateChildThread(Some(old_mm.for_exec(detpid)))
            )
        );

        let start_response = state
            .receive_rpc(
                reverie::Tid::from_raw(dettid.as_raw()),
                (
                    fresh_local_time,
                    old_mm.for_exec(detpid),
                    GlobalRequest::StartNewThread(dettid, detpid),
                ),
            )
            .await;
        assert_eq!(
            start_response,
            (Some(thread_before), GlobalResponse::StartNewThread(None))
        );

        let scheduler = state.sched.lock().unwrap();
        assert!(!scheduler.thread_is_logically_killed(dettid));
        assert!(scheduler.thread_is_logically_killed(sibling));
        assert_eq!(scheduler.next_turns.len(), 1);
        assert!(matches!(sibling_request.try_read(), Some(Err(_))));
        assert!(
            scheduler.child_tid_was_cleared(FutexID::private(old_mm, 0x1234), sibling.as_raw())
        );
        drop(scheduler);
        let global_time = state.global_time.lock().unwrap();
        assert_eq!(global_time.as_nanos(), global_before);
        assert_eq!(global_time.threads_time(dettid), thread_before);
    }

    #[tokio::test]
    async fn nonleader_exec_rebinds_caller_to_leader_and_preserves_its_clock() {
        let (config, state, leader, detpid) = cancellation_test_state();
        let worker = DetTid::from_raw(leader.as_raw() + 1);
        let sibling = DetTid::from_raw(leader.as_raw() + 2);
        let old_mm = MmId::initial(detpid).for_exec(detpid);
        let leader_request = Ivar::new();
        let worker_request = Ivar::new();
        let sibling_request = Ivar::new();
        install_test_registration(&state, leader, leader_request.clone());
        {
            let mut scheduler = state.sched.lock().unwrap();
            scheduler.thread_tree.add_child(leader, worker, false);
            scheduler.thread_tree.add_child(leader, sibling, false);
        }
        install_test_registration(&state, worker, worker_request.clone());
        install_test_registration(&state, sibling, sibling_request.clone());
        {
            let mut scheduler = state.sched.lock().unwrap();
            scheduler
                .next_turns
                .get_mut(&sibling)
                .unwrap()
                .child_tid_addr = 0x5678;
            scheduler
                .timeslices
                .insert(leader, Some(LogicalTime::from_nanos(99)));
            scheduler.install_test_vfork_barrier(leader, sibling);
        }

        let mut leader_clock = DetTime::new(&config);
        leader_clock.add_syscall();
        let mut worker_clock = DetTime::new(&config);
        worker_clock.add_syscall();
        worker_clock.add_syscall();
        worker_clock.add_syscall();
        {
            let mut global_time = state.global_time.lock().unwrap();
            global_time.update_global_time(leader, leader_clock.as_nanos());
            global_time.update_global_time(worker, worker_clock.as_nanos());
        }
        let total_before = state.global_time.lock().unwrap().as_nanos();
        let fd_blocking: ExecFdBlockingOverrides = [42].into_iter().collect();
        state.pending_exec_states.lock().unwrap().insert(
            detpid,
            PendingExecState {
                caller: worker,
                process: detpid,
                mm: old_mm,
                fd_blocking: fd_blocking.clone(),
            },
        );
        let fresh_local_time = DetTime::new(&config);

        let create_response = state
            .receive_rpc(
                reverie::Tid::from_raw(leader.as_raw()),
                (
                    fresh_local_time.clone(),
                    MmId::initial(leader),
                    GlobalRequest::CreateChildThread(
                        leader,
                        detpid,
                        0,
                        None,
                        Some(DEFAULT_PRIORITY),
                    ),
                ),
            )
            .await;
        assert_eq!(
            create_response,
            (
                Some(worker_clock.as_nanos()),
                GlobalResponse::CreateChildThread(Some(old_mm.for_exec(detpid)))
            )
        );
        assert!(state.pending_exec_states.lock().unwrap().is_empty());
        assert_eq!(
            state.post_exec_fd_blocking.lock().unwrap().get(&leader),
            Some(&fd_blocking)
        );

        let late_old_request = state
            .receive_rpc(
                reverie::Tid::from_raw(leader.as_raw()),
                (
                    leader_clock.clone(),
                    old_mm,
                    GlobalRequest::RequestResources(Resources::new(leader), detpid),
                ),
            )
            .await;
        assert_eq!(late_old_request, (None, GlobalResponse::ThreadExited));
        let admitted_before_fence = state
            .recv_request_resources(
                reverie::Tid::from_raw(leader.as_raw()),
                detpid,
                Resources::new(leader),
                Some(old_mm),
            )
            .await;
        assert_eq!(
            admitted_before_fence,
            (SchedulerRpcResult::ThreadExited, None)
        );
        let late_old_deregister = state
            .receive_rpc(
                reverie::Tid::from_raw(leader.as_raw()),
                (
                    leader_clock.clone(),
                    old_mm,
                    GlobalRequest::DeregisterThread(ThreadDeregistration {
                        dettid: leader,
                        detpid,
                        mm: old_mm,
                        timeslice_stats: TimesliceStats::default(),
                        chaos_epochs: Vec::new(),
                    }),
                ),
            )
            .await;
        assert_eq!(
            late_old_deregister,
            (None, GlobalResponse::DeregisterThread(()))
        );
        let duplicate_create = state
            .receive_rpc(
                reverie::Tid::from_raw(leader.as_raw()),
                (
                    fresh_local_time.clone(),
                    MmId::initial(leader),
                    GlobalRequest::CreateChildThread(
                        leader,
                        detpid,
                        0,
                        None,
                        Some(DEFAULT_PRIORITY),
                    ),
                ),
            )
            .await;
        assert_eq!(duplicate_create, (None, GlobalResponse::ThreadExited));

        {
            let mut scheduler = state.sched.lock().unwrap();
            assert!(!scheduler.thread_is_logically_killed(leader));
            assert!(scheduler.thread_is_logically_killed(worker));
            assert!(scheduler.thread_is_logically_killed(sibling));
            assert_eq!(scheduler.next_turns.len(), 1);
            assert!(scheduler.next_turns.contains_key(&leader));
            assert!(!scheduler.timeslices.contains_key(&leader));
            assert!(!scheduler.vfork_barrier_mentions(leader));
            assert!(!scheduler.vfork_barrier_mentions(sibling));
            assert!(matches!(leader_request.try_read(), Some(Err(_))));
            assert!(matches!(worker_request.try_read(), Some(Err(_))));
            assert!(matches!(sibling_request.try_read(), Some(Err(_))));
            assert!(
                scheduler
                    .child_tid_was_cleared(FutexID::private(old_mm, 0x5678), sibling.as_raw(),)
            );
            scheduler.next_turns.get_mut(&leader).unwrap().resp =
                Ivar::full(SchedResponse::Go(None));
        }

        let start_response = state
            .receive_rpc(
                reverie::Tid::from_raw(leader.as_raw()),
                (
                    fresh_local_time,
                    old_mm.for_exec(detpid),
                    GlobalRequest::StartNewThread(leader, detpid),
                ),
            )
            .await;
        assert_eq!(
            start_response,
            (
                Some(worker_clock.as_nanos()),
                GlobalResponse::StartNewThread(None)
            )
        );
        {
            let global_time = state.global_time.lock().unwrap();
            assert_eq!(global_time.as_nanos(), total_before);
            assert_eq!(global_time.threads_time(leader), worker_clock.as_nanos());
            assert!(!global_time.contains_thread(worker));
        }

        let mark_response = state
            .receive_rpc(
                reverie::Tid::from_raw(leader.as_raw()),
                (
                    worker_clock,
                    old_mm.for_exec(detpid),
                    GlobalRequest::MarkPastFirstExecve,
                ),
            )
            .await;
        assert_eq!(
            mark_response.1,
            GlobalResponse::MarkPastFirstExecve(fd_blocking)
        );
        assert!(state.post_exec_fd_blocking.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn failed_exec_clears_prepared_state_without_retiring_siblings() {
        let (config, state, leader, detpid) = cancellation_test_state();
        let sibling = DetTid::from_raw(leader.as_raw() + 1);
        install_test_registration(&state, leader, Ivar::new());
        state
            .sched
            .lock()
            .unwrap()
            .thread_tree
            .add_child(leader, sibling, false);
        install_test_registration(&state, sibling, Ivar::new());
        let clock = DetTime::new(&config);

        let prepared = state
            .receive_rpc(
                reverie::Tid::from_raw(leader.as_raw()),
                (
                    clock.clone(),
                    MmId::initial(leader),
                    GlobalRequest::PrepareExec(detpid, MmId::initial(detpid), Default::default()),
                ),
            )
            .await;
        assert_eq!(prepared.1, GlobalResponse::PrepareExec(()));
        assert!(
            state
                .pending_exec_states
                .lock()
                .unwrap()
                .contains_key(&detpid)
        );

        let cancelled = state
            .receive_rpc(
                reverie::Tid::from_raw(leader.as_raw()),
                (
                    clock,
                    MmId::initial(leader),
                    GlobalRequest::CancelExec(detpid),
                ),
            )
            .await;
        assert_eq!(cancelled.1, GlobalResponse::CancelExec(()));
        assert!(state.pending_exec_states.lock().unwrap().is_empty());
        {
            let scheduler = state.sched.lock().unwrap();
            assert!(!scheduler.thread_is_logically_killed(leader));
            assert!(!scheduler.thread_is_logically_killed(sibling));
            assert_eq!(scheduler.next_turns.len(), 2);
        }

        state.pending_exec_states.lock().unwrap().insert(
            detpid,
            PendingExecState {
                caller: leader,
                process: detpid,
                mm: MmId::initial(detpid),
                fd_blocking: Default::default(),
            },
        );
        state
            .post_exec_fd_blocking
            .lock()
            .unwrap()
            .insert(leader, [42].into_iter().collect());
        state
            .recv_deregister_thread(
                reverie::Tid::from_raw(leader.as_raw()),
                ThreadDeregistration {
                    dettid: leader,
                    detpid,
                    mm: MmId::initial(detpid),
                    timeslice_stats: TimesliceStats::default(),
                    chaos_epochs: Vec::new(),
                },
            )
            .await;
        assert!(state.pending_exec_states.lock().unwrap().is_empty());
        assert!(state.post_exec_fd_blocking.lock().unwrap().is_empty());

        state.pending_exec_states.lock().unwrap().insert(
            detpid,
            PendingExecState {
                caller: leader,
                process: detpid,
                mm: MmId::initial(detpid),
                fd_blocking: Default::default(),
            },
        );
        state
            .recv_deregister_thread(
                reverie::Tid::from_raw(leader.as_raw()),
                ThreadDeregistration {
                    dettid: leader,
                    detpid,
                    mm: MmId::initial(detpid).for_exec(detpid),
                    timeslice_stats: TimesliceStats::default(),
                    chaos_epochs: Vec::new(),
                },
            )
            .await;
        assert!(state.pending_exec_states.lock().unwrap().is_empty());

        state.pending_exec_states.lock().unwrap().insert(
            detpid,
            PendingExecState {
                caller: leader,
                process: detpid,
                mm: MmId::initial(detpid),
                fd_blocking: Default::default(),
            },
        );
        state
            .post_exec_fd_blocking
            .lock()
            .unwrap()
            .insert(leader, [42].into_iter().collect());
        state.complete_physical_process_exit(detpid.as_raw());
        assert!(state.pending_exec_states.lock().unwrap().is_empty());
        assert!(state.post_exec_fd_blocking.lock().unwrap().is_empty());
    }

    #[test]
    fn unsupported_syscall_report_duplicate_is_close_on_exec() {
        let mut descriptors = [-1; 2];
        assert_eq!(
            unsafe { libc::pipe2(descriptors.as_mut_ptr(), libc::O_CLOEXEC) },
            0
        );
        // SAFETY: pipe2 initialized both descriptors and transfers ownership.
        let _reader = unsafe { OwnedFd::from_raw_fd(descriptors[0]) };
        let writer = unsafe { OwnedFd::from_raw_fd(descriptors[1]) };
        let config = Config {
            unsupported_syscall_report_fd: Some(writer.as_raw_fd()),
            ..Config::default()
        };

        let state = GlobalState::initialize(&config, false);
        let duplicate = state
            .unsupported_syscall_report_fd
            .as_ref()
            .expect("report writer should be duplicated")
            .lock()
            .unwrap();
        let flags = unsafe { libc::fcntl(duplicate.as_raw_fd(), libc::F_GETFD) };
        assert_ne!(flags, -1);
        assert_ne!(flags & libc::FD_CLOEXEC, 0);
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1056): Deterministic st_dev remapping test.
    #[test]
    fn device_pool_remaps_deterministically() {
        use super::DevicePool;

        // Raw device numbers a guest might observe; the procfs/tmpfs ones
        // (large anon-bdev values) are exactly what drifts between runs.
        let raw_root = 0x20; // e.g. a real block device
        let raw_proc_run1 = 3_145_792; // anon bdev in run 1
        let raw_proc_run2 = 3_145_788; // same procfs, different number in run 2

        // Run 1: observe root then proc.
        let mut pool1 = DevicePool::new();
        let root1 = pool1.determinize(raw_root);
        let proc1 = pool1.determinize(raw_proc_run1);
        // Run 2: same observation order, different raw proc number.
        let mut pool2 = DevicePool::new();
        let root2 = pool2.determinize(raw_root);
        let proc2 = pool2.determinize(raw_proc_run2);

        // The synthetic ids depend only on first-observation order, so they are
        // identical across the two runs despite the raw proc number differing.
        assert_eq!(root1, root2);
        assert_eq!(proc1, proc2);

        // Distinct raw devices get distinct ids; ids start at 1 (never 0).
        assert_ne!(root1, proc1);
        assert_eq!(root1, 1);
        assert_eq!(proc1, 2);

        // Re-observing a raw device is stable within a run.
        assert_eq!(pool1.determinize(raw_root), root1);
        assert_eq!(pool1.determinize(raw_proc_run1), proc1);
    }

    #[tokio::test]
    async fn late_futex_rpc_after_thread_removal_returns_eintr() {
        let config = Config {
            sequentialize_threads: true,
            ..Config::default()
        };
        let state = GlobalState::initialize(&config, false);
        let dettid = DetTid::from_raw(17);
        let detpid = DetPid::from_raw(17);
        let response = state
            .recv_futex_action(
                RpcIncarnation {
                    dettid,
                    mm: MmId::initial(detpid),
                },
                FutexAction::WaitRequest(None),
                FutexID::private(MmId::initial(detpid), 0x1000),
                0,
                u32::MAX,
            )
            .await;

        assert!(matches!(
            response,
            Some(SchedValue::Value(value)) if value == nix::errno::Errno::EINTR as u64
        ));
    }

    #[tokio::test]
    async fn late_child_tid_wait_after_exit_returns_spurious_wake() {
        let config = Config {
            sequentialize_threads: true,
            ..Config::default()
        };
        let state = GlobalState::initialize(&config, false);
        let detpid = DetPid::from_raw(17);
        let child = DetTid::from_raw(18);
        let futex = FutexID::private(MmId::initial(detpid), 0x1000);
        state.sched.lock().unwrap().next_turns.insert(
            detpid,
            ThreadNextTurn {
                dettid: detpid,
                child_tid_addr: 0,
                req: Ivar::new(),
                resp: Ivar::new(),
            },
        );
        state
            .sched
            .lock()
            .unwrap()
            .wake_futex_child_cleartid(futex, child);
        assert!(
            state
                .sched
                .lock()
                .unwrap()
                .child_tid_was_cleared(futex, child.as_raw())
        );
        assert!(
            !state
                .sched
                .lock()
                .unwrap()
                .child_tid_was_cleared(futex, child.as_raw() + 1)
        );

        let response = state
            .recv_futex_action(
                RpcIncarnation {
                    dettid: detpid,
                    mm: MmId::initial(detpid),
                },
                FutexAction::WaitRequest(None),
                futex,
                child.as_raw(),
                u32::MAX,
            )
            .await;

        assert!(matches!(response, Some(SchedValue::Value(0))));
        assert!(state.sched.lock().unwrap().blocked.futex_waiters.is_empty());
    }

    #[tokio::test]
    async fn late_resource_request_after_logical_kill_is_cancelled() {
        let (config, state, dettid, detpid) = cancellation_test_state();
        install_test_registration(&state, dettid, Ivar::new());
        let mut current_time = DetTime::new(&config);
        current_time.add_syscall();
        state
            .global_time
            .lock()
            .unwrap()
            .update_global_time(dettid, current_time.as_nanos());
        state
            .sched
            .lock()
            .unwrap()
            .logically_kill_thread(&dettid, &detpid, MmId::initial(detpid));
        let (global_before, thread_before) = {
            let global_time = state.global_time.lock().unwrap();
            (global_time.as_nanos(), global_time.threads_time(dettid))
        };
        let mut late_time = current_time;
        late_time.add_syscall();

        let response = state
            .receive_rpc(
                reverie::Tid::from_raw(dettid.as_raw()),
                (
                    late_time,
                    MmId::initial(dettid),
                    GlobalRequest::RequestResources(Resources::new(dettid), detpid),
                ),
            )
            .await;

        assert_eq!(response, (None, GlobalResponse::ThreadExited));
        assert!(!state.sched.lock().unwrap().next_turns.contains_key(&dettid));
        let global_time = state.global_time.lock().unwrap();
        assert_eq!(global_time.as_nanos(), global_before);
        assert_eq!(global_time.threads_time(dettid), thread_before);
    }

    #[tokio::test]
    async fn duplicate_deregistration_is_acknowledged_without_clock_or_scheduler_mutation() {
        let (config, state, dettid, detpid) = cancellation_test_state();
        install_test_registration(&state, dettid, Ivar::new());
        let mut current_time = DetTime::new(&config);
        current_time.add_syscall();
        state
            .global_time
            .lock()
            .unwrap()
            .update_global_time(dettid, current_time.as_nanos());
        state
            .sched
            .lock()
            .unwrap()
            .logically_kill_thread(&dettid, &detpid, MmId::initial(detpid));
        let (global_before, thread_before) = {
            let global_time = state.global_time.lock().unwrap();
            (global_time.as_nanos(), global_time.threads_time(dettid))
        };
        let mut late_time = current_time;
        late_time.add_syscall();
        let mut final_stats = TimesliceStats::default();
        final_stats.record(7);

        let first_response = state
            .receive_rpc(
                reverie::Tid::from_raw(dettid.as_raw()),
                (
                    late_time.clone(),
                    MmId::initial(dettid),
                    GlobalRequest::DeregisterThread(ThreadDeregistration {
                        dettid,
                        detpid,
                        mm: MmId::initial(detpid),
                        timeslice_stats: final_stats,
                        chaos_epochs: Vec::new(),
                    }),
                ),
            )
            .await;
        assert_eq!(first_response, (None, GlobalResponse::DeregisterThread(())));
        assert_eq!(
            state
                .sched
                .lock()
                .unwrap()
                .per_thread_timeslice
                .get(&dettid),
            Some(&final_stats)
        );

        late_time.add_syscall();
        let duplicate_response = state
            .receive_rpc(
                reverie::Tid::from_raw(dettid.as_raw()),
                (
                    late_time,
                    MmId::initial(dettid),
                    GlobalRequest::DeregisterThread(ThreadDeregistration {
                        dettid,
                        detpid,
                        mm: MmId::initial(detpid),
                        timeslice_stats: final_stats,
                        chaos_epochs: Vec::new(),
                    }),
                ),
            )
            .await;
        assert_eq!(
            duplicate_response,
            (None, GlobalResponse::DeregisterThread(()))
        );
        assert_eq!(
            state
                .sched
                .lock()
                .unwrap()
                .per_thread_timeslice
                .get(&dettid),
            Some(&final_stats)
        );
        assert!(!state.sched.lock().unwrap().next_turns.contains_key(&dettid));
        let global_time = state.global_time.lock().unwrap();
        assert_eq!(global_time.as_nanos(), global_before);
        assert_eq!(global_time.threads_time(dettid), thread_before);
    }

    #[tokio::test]
    async fn child_registration_fails_closed_for_a_tombstoned_tid() {
        let (config, state, parent, detpid) = cancellation_test_state();
        install_test_registration(&state, parent, Ivar::new());
        let child = DetTid::from_raw(18);
        state
            .sched
            .lock()
            .unwrap()
            .thread_tree
            .add_child(parent, child, false);
        install_test_registration(&state, child, Ivar::new());
        state
            .sched
            .lock()
            .unwrap()
            .logically_kill_thread(&child, &detpid, MmId::initial(detpid));

        let response = state
            .receive_rpc(
                reverie::Tid::from_raw(parent.as_raw()),
                (
                    DetTime::new(&config),
                    MmId::initial(parent),
                    GlobalRequest::CreateChildThread(
                        child,
                        detpid,
                        0,
                        None,
                        Some(DEFAULT_PRIORITY),
                    ),
                ),
            )
            .await;

        assert_eq!(response, (None, GlobalResponse::ThreadExited));
        let scheduler = state.sched.lock().unwrap();
        assert!(scheduler.thread_is_logically_killed(child));
        assert!(!scheduler.next_turns.contains_key(&child));
        assert!(!scheduler.priorities.contains_key(&child));
        assert!(!state.global_time.lock().unwrap().contains_thread(parent));
    }

    #[tokio::test]
    async fn pending_resource_request_woken_by_logical_kill_is_terminal() {
        let (_, state, dettid, detpid) = cancellation_test_state();
        let request_seen = Ivar::new();
        install_test_registration(&state, dettid, request_seen.clone());

        let request = state.recv_request_resources(
            reverie::Tid::from_raw(dettid.as_raw()),
            detpid,
            Resources::new(dettid),
            None,
        );
        let kill_after_request = async {
            while request_seen.try_read().is_none() {
                tokio::task::yield_now().await;
            }
            state.sched.lock().unwrap().logically_kill_thread(
                &dettid,
                &detpid,
                MmId::initial(detpid),
            );
        };

        let (response, ()) = tokio::join!(request, kill_after_request);
        assert_eq!(response, (SchedulerRpcResult::ThreadExited, None));
    }

    #[tokio::test]
    async fn trace_replay_yield_propagates_terminal_scheduler_cancellation() {
        let dettid = DetTid::from_raw(17);
        let detpid = DetPid::from_raw(17);
        let next_tid = DetTid::from_raw(18);
        let event = SchedEvent {
            dettid,
            op: Op::OtherInstructions,
            count: 1,
            start_rip: None,
            end_rip: None,
            end_time: Some(LogicalTime::from_nanos(1)),
        };
        let next_event = SchedEvent {
            dettid: next_tid,
            end_time: Some(LogicalTime::from_nanos(2)),
            ..event.clone()
        };
        let trace_file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            trace_file.path(),
            PreemptionRecord::from_sched_events(vec![event.clone(), next_event]).to_string(),
        )
        .unwrap();
        let config = Config {
            sequentialize_threads: true,
            cancel_killed_thread_rpcs: true,
            replay_schedule_from: Some(trace_file.path().to_path_buf()),
            ..Config::default()
        };
        let state = GlobalState::initialize(&config, false);
        state
            .sched
            .lock()
            .unwrap()
            .thread_tree
            .add_child(dettid, dettid, true);
        state
            .sched
            .lock()
            .unwrap()
            .thread_tree
            .add_child(dettid, next_tid, false);
        let request_seen = Ivar::new();
        install_test_registration(&state, dettid, request_seen.clone());
        install_test_registration(&state, next_tid, Ivar::new());

        let replay = state.recv_trace_schedevent(event, detpid, MmId::initial(detpid));
        let kill_after_replay_yield = async {
            while request_seen.try_read().is_none() {
                tokio::task::yield_now().await;
            }
            state.sched.lock().unwrap().logically_kill_thread(
                &dettid,
                &detpid,
                MmId::initial(detpid),
            );
        };
        let (response, ()) = tokio::time::timeout(Duration::from_secs(1), async {
            tokio::join!(replay, kill_after_replay_yield)
        })
        .await
        .expect("trace replay cancellation did not terminate the pending scheduler RPC");

        assert_eq!(response, SchedulerRpcResult::ThreadExited);
    }

    #[tokio::test]
    async fn pending_start_request_woken_by_logical_kill_is_terminal() {
        let (config, state, dettid, detpid) = cancellation_test_state();
        let request_seen = Ivar::new();
        install_test_registration(&state, dettid, request_seen.clone());
        let request = state.receive_rpc(
            reverie::Tid::from_raw(dettid.as_raw()),
            (
                DetTime::new(&config),
                MmId::initial(dettid),
                GlobalRequest::StartNewThread(dettid, detpid),
            ),
        );
        let kill_after_request = async {
            while request_seen.try_read().is_none() {
                tokio::task::yield_now().await;
            }
            state.sched.lock().unwrap().logically_kill_thread(
                &dettid,
                &detpid,
                MmId::initial(detpid),
            );
        };

        let (response, ()) = tokio::join!(request, kill_after_request);
        assert_eq!(response, (None, GlobalResponse::ThreadExited));
        assert!(!state.sched.lock().unwrap().priorities.contains_key(&dettid));
    }

    #[tokio::test]
    async fn tombstoned_timer_registration_cannot_mutate_scheduler_state() {
        let (_, state, dettid, detpid) = cancellation_test_state();
        install_test_registration(&state, dettid, Ivar::new());
        state
            .sched
            .lock()
            .unwrap()
            .logically_kill_thread(&dettid, &detpid, MmId::initial(detpid));
        let now = LogicalTime::from_nanos(100);

        let alarm = state
            .recv_register_alarm(
                detpid,
                RpcIncarnation {
                    dettid,
                    mm: MmId::initial(detpid),
                },
                now,
                LogicalTime::from_nanos(10),
                LogicalTime::ZERO,
                SigWrapper(Signal::SIGALRM),
            )
            .await;
        assert_eq!(alarm, SchedulerRpcResult::ThreadExited);

        let posix = state
            .recv_register_posix_timer(
                detpid,
                RpcIncarnation {
                    dettid,
                    mm: MmId::initial(detpid),
                },
                1,
                Some(now + LogicalTime::from_nanos(10)),
                LogicalTime::ZERO,
                SigWrapper(Signal::SIGALRM),
            )
            .await;
        assert_eq!(posix, SchedulerRpcResult::ThreadExited);
        assert!(state.sched.lock().unwrap().blocked.timed_waiters.is_empty());
    }

    #[tokio::test]
    async fn pending_futex_request_woken_by_logical_kill_is_terminal() {
        let (config, state, dettid, detpid) = cancellation_test_state();
        install_test_registration(&state, dettid, Ivar::new());
        let request = state.receive_rpc(
            reverie::Tid::from_raw(dettid.as_raw()),
            (
                DetTime::new(&config),
                MmId::initial(dettid),
                GlobalRequest::FutexAction(
                    dettid,
                    FutexAction::WaitRequest(None),
                    FutexID::private(MmId::initial(detpid), 0x1000),
                    0,
                    u32::MAX,
                ),
            ),
        );
        let kill_after_wait = async {
            while state.sched.lock().unwrap().blocked.futex_waiters.is_empty() {
                tokio::task::yield_now().await;
            }
            state.sched.lock().unwrap().logically_kill_thread(
                &dettid,
                &detpid,
                MmId::initial(detpid),
            );
        };

        let (response, ()) = tokio::time::timeout(Duration::from_secs(1), async {
            tokio::join!(request, kill_after_wait)
        })
        .await
        .expect("futex teardown did not wake the blocked RPC");
        assert_eq!(response, (None, GlobalResponse::ThreadExited));
    }

    #[tokio::test]
    async fn parent_continue_propagates_terminal_scheduler_cancellation() {
        let (config, state, parent, detpid) = cancellation_test_state();
        let parent_request = Ivar::new();
        install_test_registration(&state, parent, parent_request.clone());
        let child = DetTid::from_raw(18);
        let request = state.receive_rpc(
            reverie::Tid::from_raw(parent.as_raw()),
            (
                DetTime::new(&config),
                MmId::initial(parent),
                GlobalRequest::CreateChildThread(child, detpid, 0, None, Some(DEFAULT_PRIORITY)),
            ),
        );
        let kill_after_parent_parks = async {
            while parent_request.try_read().is_none() {
                tokio::task::yield_now().await;
            }
            state.sched.lock().unwrap().logically_kill_thread(
                &parent,
                &detpid,
                MmId::initial(detpid),
            );
        };

        let (response, ()) = tokio::join!(request, kill_after_parent_parks);
        assert_eq!(response, (None, GlobalResponse::ThreadExited));
    }

    #[test]
    fn unsupported_syscall_warning_is_sorted_and_aggregated() {
        let syscalls = BTreeSet::from([
            "vmsplice".to_owned(),
            "getppid".to_owned(),
            "getppid".to_owned(),
        ]);

        assert_eq!(
            format_unsupported_syscall_warning(&syscalls).as_deref(),
            Some("syscalls getppid,vmsplice used but not yet supported")
        );
        assert_eq!(format_unsupported_syscall_warning(&BTreeSet::new()), None);
    }

    #[tokio::test]
    async fn abnormal_cleanup_cancels_an_unstarted_scheduler() {
        let config = Config {
            sequentialize_threads: true,
            ..Config::default()
        };
        let mut state = GlobalState::initialize(&config, true);
        state.cancel_internal_scheduler().await;
        let summary_path = None;
        let cleanup = state.clean_up(false, &summary_path);

        assert!(
            tokio::time::timeout(Duration::from_millis(100), cleanup)
                .await
                .is_ok(),
            "cleanup waited for a scheduler whose guest never registered"
        );
    }

    #[tokio::test]
    async fn abnormal_cleanup_cancels_a_registered_scheduler() {
        let config = Config {
            sequentialize_threads: true,
            ..Config::default()
        };
        let mut state = GlobalState::initialize(&config, true);
        let dettid = DetTid::from_raw(1);
        {
            let mut scheduler = state.sched.lock().unwrap();
            scheduler.priorities.insert(dettid, DEFAULT_PRIORITY);
            scheduler.next_turns.insert(
                dettid,
                ThreadNextTurn {
                    dettid,
                    child_tid_addr: 0,
                    req: Ivar::new(),
                    resp: Ivar::new(),
                },
            );
            scheduler.runqueue_push_back(dettid);
            scheduler.started_up.put(());
        }
        tokio::task::yield_now().await;

        state.cancel_internal_scheduler().await;
        let summary_path = None;
        assert!(
            tokio::time::timeout(
                Duration::from_millis(100),
                state.clean_up(false, &summary_path),
            )
            .await
            .is_ok(),
            "cleanup waited after cancelling a registered scheduler"
        );
    }
}
