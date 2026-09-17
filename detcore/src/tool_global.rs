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
use detcore_model::procfs::mount_ids_are_ordered_subset;
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
use tracing::error;
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
use crate::scheduler::AdmitIntent;
use crate::scheduler::AdmitSide;
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
use crate::tool_local::RobustListWake;
use crate::types::*;

pub(crate) async fn yield_once() {
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
    /// Counter backing the minted [`DetInode`]s. Deliberately a plain integer:
    /// it is the *source* of deterministic inodes, not one itself, and typing
    /// it `RawInode` previously blurred that distinction.
    next_inode: u64,
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
    exit_signal: libc::c_int,
    physical_ids: Option<(i32, i32)>,
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

/// Separate terminal cleanup outcomes; neither replaces the backend failure.
pub struct BackendFailureCleanup {
    /// Natural scheduler completion, retaining a task panic or cancellation.
    pub scheduler: Result<(), tokio::task::JoinError>,
    /// The requested partial recording's write result; no destination is success.
    pub preemption_recording: Result<(), String>,
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
                // THE determinization boundary: the single place a host inode
                // is deliberately mapped to a deterministic one. The value is
                // minted from a monotonic counter, never derived from the host
                // inode's bits.
                let new = DetInode::mint(self.next_inode);
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
/// synthetic id assigned in first-observation order. Both `stat`/`statx` and a
/// virtualized mountinfo snapshot use this pool. A mountinfo read intentionally
/// pre-populates it in that snapshot's row order, and later metadata syscalls
/// reuse those assignments.
///
/// This guarantees identity consistency within one Detcore run. It is not an
/// unconditional cross-machine guarantee: hosts with different filesystem
/// layouts can expose different device equivalence classes or first-observation
/// order. The remapping still preserves distinctness and equality within the
/// run, so `find -xdev`, `du -x`, and `(st_dev, st_ino)` checks behave
/// consistently with the mountinfo device column.
#[derive(Debug)]
struct DevicePool {
    devices: HashMap<u64, u64>,
    next_device: u64,
}

/// Run-global identities for fdinfo mount IDs that are not present in the
/// namespace's mountinfo table.
///
/// Linux gives pseudo filesystems such as pipefs, sockfs, anon_inodefs, nsfs,
/// and pidfs their own mount IDs without listing those mounts in
/// `/proc/*/mountinfo`. The raw numbers are host-assigned. Preserve equality
/// and distinctness by keying on the raw mount ID. IDs present in mountinfo are
/// assigned in canonical row/parent order; unlisted IDs are assigned afterward
/// in deterministic guest observation order.
#[derive(Debug)]
enum MountIdPool {
    Uninitialized,
    Invalid,
    Ready {
        mount_ids: BTreeMap<u64, u64>,
        mountinfo_order: Vec<u64>,
        allow_visible_subsets: bool,
        unlisted_order: Vec<u64>,
        next_mount_id: u64,
    },
}

/// The raw identity order observed by the producer and required to reconstruct
/// the same guest-visible mount IDs during replay.
pub struct MountIdentityProvenance {
    pub mountinfo_order: Vec<u64>,
    pub unlisted_order: Vec<u64>,
}

impl MountIdPool {
    fn from_config(mount_ids: &[u64], captured: bool, unlisted_ids: &[u64]) -> Self {
        if !captured {
            if !mount_ids.is_empty() || !unlisted_ids.is_empty() {
                return Self::Invalid;
            }
            return Self::Uninitialized;
        }
        Self::from_orders(mount_ids, unlisted_ids, true).unwrap_or(Self::Invalid)
    }

    fn from_orders(
        mount_ids: &[u64],
        unlisted_ids: &[u64],
        allow_visible_subsets: bool,
    ) -> Option<Self> {
        let mut seen = BTreeSet::new();
        if !mount_ids.iter().all(|raw| seen.insert(*raw))
            || !unlisted_ids
                .iter()
                .all(|raw| *raw != 0 && seen.insert(*raw))
        {
            return None;
        }

        let mut mappings = BTreeMap::new();
        for (index, raw) in mount_ids.iter().chain(unlisted_ids).enumerate() {
            mappings.insert(*raw, u64::try_from(index).ok()?.checked_add(1)?);
        }
        let next_mount_id = u64::try_from(mappings.len()).ok()?.checked_add(1)?;
        Some(Self::Ready {
            mount_ids: mappings,
            mountinfo_order: mount_ids.to_vec(),
            allow_visible_subsets,
            unlisted_order: unlisted_ids.to_vec(),
            next_mount_id,
        })
    }

    fn validate_mountinfo_order(&mut self, mountinfo_order: &[u64]) -> bool {
        if matches!(self, Self::Uninitialized) {
            *self = Self::from_orders(mountinfo_order, &[], false).unwrap_or(Self::Invalid);
        }
        let Self::Ready {
            mountinfo_order: expected,
            allow_visible_subsets,
            ..
        } = self
        else {
            return false;
        };
        if *allow_visible_subsets {
            mount_ids_are_ordered_subset(mountinfo_order, expected)
        } else {
            mountinfo_order == expected
        }
    }

    fn determinize(&mut self, raw_mount_id: u64, mountinfo_order: Option<&[u64]>) -> Option<u64> {
        // Linux uses zero for anonymous objects such as memfd. It is one
        // equivalence class regardless of Detcore's descriptor classification.
        if raw_mount_id == 0 {
            return Some(0);
        }
        if let Some(order) = mountinfo_order {
            if !self.validate_mountinfo_order(order) {
                return None;
            }
        } else if matches!(self, Self::Uninitialized) {
            return None;
        }
        let Self::Ready {
            mount_ids,
            unlisted_order,
            next_mount_id,
            ..
        } = self
        else {
            return None;
        };

        if let Some(virtual_mount_id) = mount_ids.get(&raw_mount_id) {
            return Some(*virtual_mount_id);
        }
        let virtual_mount_id = *next_mount_id;
        *next_mount_id = next_mount_id.checked_add(1)?;
        mount_ids.insert(raw_mount_id, virtual_mount_id);
        unlisted_order.push(raw_mount_id);
        Some(virtual_mount_id)
    }

    fn provenance(&self) -> Result<Option<MountIdentityProvenance>, &'static str> {
        match self {
            Self::Uninitialized => Ok(None),
            Self::Invalid => Err("mount identity provenance is invalid"),
            Self::Ready {
                mountinfo_order,
                unlisted_order,
                ..
            } => Ok(Some(MountIdentityProvenance {
                mountinfo_order: mountinfo_order.clone(),
                unlisted_order: unlisted_order.clone(),
            })),
        }
    }
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

    /// Shared fdinfo mount-ID equivalence classes for this Detcore run.
    mount_ids: Mutex<MountIdPool>,

    // next port to use if input port is 0
    next_port: AtomicU16,

    // used ports
    used_ports: Mutex<HashSet<u16>>,

    // Unsupported syscall names observed across every process in this run.
    unsupported_syscalls: Mutex<BTreeSet<String>>,

    // Optional append-only sink shared by DBT fork descendants.
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
    /// Ordinary RPC mutation must linearize before terminal publication under
    /// the same mutex as scheduler grants. A losing callback stays pending for
    /// the backend's failure subscription to drop; no normal reply is invented.
    /// Consuming exit RPCs retain their existing validation/accounting path.
    async fn lock_rpc_scheduler(
        &self,
        consuming_cleanup: bool,
    ) -> std::sync::MutexGuard<'_, Scheduler> {
        std::future::poll_fn(|_| {
            let sched = self.sched.lock().unwrap();
            if !consuming_cleanup && sched.backend_failed() {
                Poll::Pending
            } else {
                Poll::Ready(sched)
            }
        })
        .await
    }

    /// Return the producer-observed mount identity order after a run.
    ///
    /// The first vector is the exact mountinfo row/parent order. The second is
    /// the first-observation order of raw fdinfo IDs absent from that table.
    pub fn mount_identity_provenance(
        &self,
    ) -> Result<Option<MountIdentityProvenance>, &'static str> {
        self.mount_ids.lock().unwrap().provenance()
    }

    fn initialize(cfg: &Config, spawn_scheduler: bool) -> Self {
        let sched = Arc::new(Mutex::new(Scheduler::new(cfg)));
        let global_time = Arc::new(Mutex::new(GlobalTime::new(cfg)));
        let handle = if cfg.sequentialize_threads && spawn_scheduler {
            // Announce before spawning, not from inside the spawned task. The
            // task's first poll is unordered with respect to the rest of this
            // bootstrap, so emitting there raced the root thread's seeding
            // lines and produced a nondeterministic INFO stream. Emitting here
            // sequences it: `Scheduler::new`'s SCHEDRAND line above, then this,
            // then the root `ThreadState::new` seeding lines.
            info!("[scheduler] daemon task starting up, waiting for guest thread start..");
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
            // This writer is internal controller state. In an in-process DBT
            // runtime it must not leak into the next guest image across exec
            // (hence F_DUPFD_CLOEXEC), and it must not perturb the descriptor
            // namespace the *current* guest observes. The backend places the
            // report fd itself high, out of the guest's working range (e.g. 199
            // for the DBT backend). Duplicating with a min hint of `fd` keeps
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
            mount_ids: Mutex::new(MountIdPool::from_config(
                &cfg.mountinfo_mount_ids,
                cfg.mountinfo_mount_ids_captured,
                &cfg.fdinfo_unlisted_mount_ids,
            )),
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
        // Emitted at the call site for the same reason as the spawned path in
        // `initialize`, so both ways of starting the daemon place this line at
        // a deterministic point in the caller's program order.
        info!("[scheduler] daemon task starting up, waiting for guest thread start..");
        sched_loop_external(self.sched.clone(), self.global_time.clone(), observer).await;
    }

    /// Reports that a backend supervisor received a process's final kernel exit status.
    ///
    /// This only records a barrier observation when the backend advertises physical-exit
    /// reporting; it is therefore a no-op for ptrace, DBT, KVM, and LiteInst execution. The
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

    /// Consume failed-run state after the scheduler has naturally finished.
    ///
    /// A failed run has no successful run summary. Preserve the scheduler's
    /// join error and any requested partial preemption recording's write error
    /// for the caller, without allowing either to replace the backend failure.
    pub async fn clean_up_after_backend_failure(mut self) -> BackendFailureCleanup {
        let scheduler = if let Some(handle) = self.sched_handle.take() {
            handle.await
        } else {
            Ok(())
        };
        // A scheduler panic can poison this mutex. Its JoinError is returned
        // below; recovering only to finish output must not replace that error.
        let writer = self
            .sched
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .preemption_writer
            .take();
        let preemption_recording = if self.cfg.record_preemptions_to.is_some() {
            writer.map_or(Ok(()), |writer| writer.flush())
        } else {
            // The writer's destination is fixed from this same configuration.
            // In-memory-only recordings have no Drop write to finish.
            drop(writer);
            Ok(())
        };
        BackendFailureCleanup {
            scheduler,
            preemption_recording,
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
            //
            // NOT `eprint!`: a guest that set O_NONBLOCK on the inherited fd 2
            // makes `write_all` fail with EAGAIN here, which panics the print
            // macro and loses both the summary and the panic message. See
            // `crate::util::RetryingStderr`.
            {
                use std::io::Write;
                let _ = write!(crate::util::RetryingStderr, "{}\n{}", banner, summary);
            }
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

    fn report_backend_failure(&self, event: reverie::BackendFailure) {
        let wake = self.sched.lock().unwrap().report_backend_failure(event);
        if let Some(wake) = wake {
            // No waiter can begin consuming cleanup until the scheduler has
            // closed its selected transaction under the grant/commit mutex.
            let _ = wake.send(());
        }
    }

    async fn wait_for_backend_failure(&self) {
        let wake = self.sched.lock().unwrap().backend_failure_waiter();
        wake.await
            .expect("GlobalState owns the failure sender until publication");
    }

    async fn receive_rpc(&self, from: Tid, gr: Self::Request) -> Self::Response {
        type R = GlobalResponse;
        let dtid = DetTid::from_raw(from.into()); // TODO(T78538674): FIXME
        let (guest_time, request_mm, request) = gr;
        let time_from_guest = guest_time.as_nanos();
        let is_deregister = matches!(&request, GlobalRequest::DeregisterThread(_));
        let consuming_cleanup =
            is_deregister || matches!(&request, GlobalRequest::RobustListWakes(_));

        let (exec_reconnect, is_exec_caller_after_local_mm_swap) = {
            let pending = self.pending_exec_states.lock().unwrap();
            let reconnect = match &request {
                GlobalRequest::CreateChildThread(child, process, _, None, _, _, _)
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
            let sched = self.lock_rpc_scheduler(consuming_cleanup).await;
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
            if let GlobalRequest::DeregisterThread(owner) = &request {
                assert_eq!(
                    owner.dettid, dtid,
                    "deregistration must belong to its sender"
                );
                assert_eq!(owner.mm, request_mm, "deregistration must retain its MmId");
                // DBT can reject StartNewThread before parent registration.
                // Its tombstone still needs the existing final accounting path.
                if !sched.thread_was_registered(dtid) && !sched.thread_is_logically_killed(dtid) {
                    assert!(
                        sched.backend_failed() || !owner.thread_start_entered,
                        "a started thread must have a scheduler registration before deregistration"
                    );
                    // The backend still consumes this constructed ThreadState,
                    // but no guest start/registration happened. Acknowledge its
                    // cleanup without creating a clock, tree entry or admission.
                    return (None, R::DeregisterThread(()));
                }
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
                GlobalRequest::StartNewThread(child_dettid, ..) if *child_dettid == dtid
            ) && self.global_time.lock().unwrap().contains_thread(dtid);

            // This portion of the time updates "asynchronously", and we can tick it on every rpc:
            // TODO: eventually the vector clock should be in shared memory, and
            // the local clocks should update truly asynchrously.  Therefore it
            // SHOULD be safe to always push through this update on any rpc.
            if tombstoned_deregistration.is_none()
                && exec_reconnect.is_none()
                && !is_thread_reconnect
            {
                self.global_time.lock().unwrap().update_global_time(
                    dtid,
                    time_from_guest,
                    guest_time.inherited_nanos(),
                );
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
                let _sched = self.lock_rpc_scheduler(false).await;
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
                let _sched = self.lock_rpc_scheduler(false).await;
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
                let _sched = self.lock_rpc_scheduler(false).await;
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
                let _sched = self.lock_rpc_scheduler(false).await;
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
            GlobalRequest::CreateChildThread(
                dettid,
                parent_detpid,
                ctid,
                flags,
                exit_signal,
                physical_ids,
                priority,
            ) => {
                if let Some(prepared) = &exec_reconnect {
                    let mut sched = self.lock_rpc_scheduler(false).await;
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
                    if let Some((physical_pid, physical_tid)) = physical_ids
                        && let Err(open_error) = sched.register_physical_thread(
                            dettid,
                            post_exec_mm,
                            physical_pid,
                            physical_tid,
                        )
                    {
                        error!(
                            "[detcore, dtid {}] failed to register post-exec host process {} thread {}: {}",
                            dettid, physical_pid, physical_tid, open_error,
                        );
                        return (None, R::ThreadExited);
                    }
                    let retired = sched.reconnect_after_exec(ExecReconnect {
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
                                exit_signal,
                                physical_ids,
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
                exit_signal,
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
                        exit_signal,
                        physical_ids: None,
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
            GlobalRequest::StartNewThread(dettid, detpid, physical_ids) => {
                match self
                    .recv_start_new_thread(from, dettid, detpid, request_mm, physical_ids)
                    .await
                {
                    SchedulerRpcResult::Continue(history) => R::StartNewThread(history),
                    SchedulerRpcResult::ThreadExited => R::ThreadExited,
                }
            }
            GlobalRequest::DeregisterThread(deregistration) => {
                R::DeregisterThread(self.recv_deregister_thread(from, deregistration).await)
            }
            GlobalRequest::SetChildTidAddress(address) => {
                let updated = self
                    .lock_rpc_scheduler(false)
                    .await
                    .set_child_tid_address(dtid, address);
                if updated {
                    R::SetChildTidAddress(())
                } else {
                    R::ThreadExited
                }
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
            GlobalRequest::RobustListWakes(wakes) => {
                R::RobustListWakes(self.recv_robust_list_wakes(wakes))
            }
            GlobalRequest::DeterminizeInode(ino) => {
                R::DeterminizeInode(self.recv_determinize_inode(from, ino).await)
            }
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-1056): Deterministic st_dev remapping RPC.
            GlobalRequest::DeterminizeDevice(dev) => {
                R::DeterminizeDevice(self.recv_determinize_device(from, dev).await)
            }
            GlobalRequest::DeterminizeMountId(raw_mount_id, fallback_order) => {
                R::DeterminizeMountId(
                    self.recv_determinize_mount_id(from, raw_mount_id, fallback_order.as_deref())
                        .await,
                )
            }
            GlobalRequest::ValidateMountIdOrder(mountinfo_order) => R::ValidateMountIdOrder(
                self.recv_validate_mount_id_order(from, &mountinfo_order)
                    .await,
            ),
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
                R::AlarmRemaining(
                    self.lock_rpc_scheduler(false)
                        .await
                        .alarm_remaining(dpid, now),
                )
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
            GlobalRequest::ResolveKillTargets(dpid) => R::ResolveKillTargets(
                self.lock_rpc_scheduler(false)
                    .await
                    .process_signal_targets(dpid),
            ),
            GlobalRequest::NotifySignalPending(dettid, SigWrapper(signal), target_process) => {
                let mut scheduler = self.lock_rpc_scheduler(false).await;
                scheduler.notify_signal_pending(dettid, SigWrapper(signal));
                if signal == libc::SIGKILL
                    && let Some(detpid) = target_process
                {
                    scheduler.note_process_sigkill(dettid, detpid);
                }
                R::NotifySignalPending(())
            }
            GlobalRequest::ThreadIsLive(dtid) => {
                R::ThreadIsLive(self.lock_rpc_scheduler(false).await.thread_is_live(dtid))
            }
            GlobalRequest::ExactChildWaitState(parent, child) => R::ExactChildWaitState(
                self.lock_rpc_scheduler(false)
                    .await
                    .exact_child_wait_state(parent, child),
            ),
            GlobalRequest::ReadyChildWait(parent, selector) => {
                let sched = self.lock_rpc_scheduler(false).await;
                R::ReadyChildWait((
                    sched.ready_child_wait(parent, selector),
                    sched.has_child_wait_target(parent, selector),
                ))
            }
            GlobalRequest::ConsumeChildWait(parent, child) => R::ConsumeChildWait(
                self.lock_rpc_scheduler(false)
                    .await
                    .consume_child_wait(parent, child),
            ),
            GlobalRequest::ProcessGroup(process) => R::ProcessGroup(
                self.lock_rpc_scheduler(false)
                    .await
                    .thread_tree
                    .process_group(process),
            ),
            GlobalRequest::SetProcessGroup(process, group) => R::SetProcessGroup(
                self.lock_rpc_scheduler(false)
                    .await
                    .thread_tree
                    .set_process_group(process, group),
            ),
            GlobalRequest::CreateSession(process) => R::CreateSession(
                self.lock_rpc_scheduler(false)
                    .await
                    .thread_tree
                    .create_session(process),
            ),
            GlobalRequest::UnrecoverableShutdown => {
                self.force_shutdown_with_error();
                R::UnrecoverableShutdown(())
            }
            GlobalRequest::RequestPort(open_file_id) => {
                let _sched = self.lock_rpc_scheduler(false).await;
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
                let _sched = self.lock_rpc_scheduler(false).await;
                let mut used_ports = self.used_ports.lock().unwrap();
                used_ports.insert(port);
                let mut open_file_to_port = self.open_file_to_port.lock().unwrap();
                open_file_to_port.insert(open_file_id, port);
                R::AddUsedPort
            }
            GlobalRequest::ReleasePort(open_file_id) => {
                let _sched = self.lock_rpc_scheduler(false).await;
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
                let sched = self.lock_rpc_scheduler(consuming_cleanup).await;
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
            let mut sched = self.lock_rpc_scheduler(false).await;
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
            let sched = self.lock_rpc_scheduler(false).await;
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
                let mut sched = self.lock_rpc_scheduler(false).await;
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
            SchedResponse::Signaled(signal) => {
                trace!(
                    "[dtid {}] resources granted but interrupted by signal",
                    dettid,
                );
                (
                    SchedulerRpcResult::Continue(ResumeStatus::Signaled(signal)),
                    None,
                )
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
            exit_signal,
            physical_ids,
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
            let mut sched = self.lock_rpc_scheduler(false).await;
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
                sched.thread_tree.add_child_with_wait_metadata(
                    parent_dettid,
                    child_dettid,
                    is_group_leader,
                    flags.is_some_and(|flags| flags.contains(CloneFlags::CLONE_PARENT)),
                    exit_signal,
                );
            }

            if let Some((physical_pid, physical_tid)) = physical_ids {
                let child_is_thread =
                    flags.is_some_and(|flags| flags.contains(CloneFlags::CLONE_THREAD));
                let child_detpid = if child_is_thread {
                    parent_detpid
                } else {
                    child_dettid
                };
                let child_mm = MmId::for_clone(
                    request_mm,
                    child_dettid,
                    flags.is_some_and(|flags| flags.contains(CloneFlags::CLONE_VM)),
                );
                if let Err(open_error) = sched.register_physical_thread(
                    child_dettid,
                    child_mm,
                    physical_pid,
                    physical_tid,
                ) {
                    error!(
                        "[detcore, dtid {}] cannot bind host process {} thread {} during child registration: {}",
                        child_dettid, physical_pid, physical_tid, open_error,
                    );
                    sched.logically_kill_thread(&child_dettid, &child_detpid, child_mm);
                    return SchedulerRpcResult::ThreadExited;
                }
            }

            // Record this thread in deterministic creation order so a
            // happens-before anchor addressed by `spawn_ordinal` resolves to it.
            sched.hb_note_spawn(child_dettid);

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

            // Describe *how* the admission side is chosen, but do not resolve it
            // (and in particular do not draw the post-fork PRNG) here: this
            // handler runs on whichever backend worker fielded the RPC, so on an
            // asynchronous backend (e.g. DBT, where the child self-registers
            // outside a scheduler turn) resolving the side now would consume the
            // PRNG draw in host RPC order. `admit_to_run_queue` resolves the
            // intent at the step2 drain -- which under ptrace is post-commit, in
            // schedule order, unchanged. Read its "What this does and does not
            // make deterministic" section before relying on that word: the drain
            // is a deterministic *point* and resolution within one drain is
            // `DetTid`-ordered, but on an asynchronous backend which drain a
            // given admission lands in is not itself schedule-determined unless
            // that admission is anchored (ordinary clone is, via the parent's
            // `ParentContinue`; vfork is, via `step2a`'s barrier). When threads
            // are not sequentialized, or the
            // parent is already kernel-blocked (vfork), the child takes the tail
            // and no PRNG is consumed.
            let intent = if self.cfg.sequentialize_threads && !parent_is_kernel_blocked {
                AdmitIntent::PostFork(self.cfg.runs_post_fork)
            } else {
                AdmitIntent::Fixed(AdmitSide::Back)
            };
            sched.admit_to_run_queue(child_dettid, intent);
            debug!(
                "[detcore] CreateChildThread with dtid {}: admit child via {:?}.",
                child_dettid, intent,
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
        physical_ids: Option<(i32, i32)>,
    ) -> SchedulerRpcResult<Option<ThreadHistory>> {
        let mut tries: u64 = 0;
        // TODO: eliminate this loop. Could instead signal with an ivar.
        let response_ivar = loop {
            yield_once().await;
            let mut sched = self.lock_rpc_scheduler(false).await;
            if sched.thread_is_logically_killed(dettid)
                || !sched.rpc_incarnation_matches(dettid, request_mm)
            {
                return SchedulerRpcResult::ThreadExited;
            }
            if self.cfg.backend_requires_thread_directed_process_signals && physical_ids.is_none() {
                error!(
                    "[detcore, dtid {}] backend requires a host thread ID at StartNewThread",
                    dettid,
                );
                sched.logically_kill_thread(&dettid, &detpid, request_mm);
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
                    signal_interrupt_errno: None,
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
            if let Some((physical_pid, physical_tid)) = physical_ids
                && let Err(open_error) =
                    sched.register_physical_thread(dettid, request_mm, physical_pid, physical_tid)
            {
                error!(
                    "[detcore, dtid {}] cannot bind host process {} thread {} for exact signal delivery: {}",
                    dettid, physical_pid, physical_tid, open_error,
                );
                sched.logically_kill_thread(&dettid, &detpid, request_mm);
                return SchedulerRpcResult::ThreadExited;
            }
            sched.request_put(&nextturn.req, rsrcs, &self.global_time);
            break nextturn.resp;
        };
        debug!(
            "[detcore, dtid {}] New thread will now wait for response on {}...",
            &dettid, &response_ivar
        );
        let _answer = response_ivar.get().await;
        let request_became_stale = {
            let sched = self.lock_rpc_scheduler(false).await;
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
            let (history, old_prio) = {
                let mut sched = self.lock_rpc_scheduler(false).await;
                if sched.thread_is_logically_killed(dettid)
                    || !sched.rpc_incarnation_matches(dettid, request_mm)
                {
                    return SchedulerRpcResult::ThreadExited;
                }
                let history = pr.extract_thread_record(&dettid).unwrap_or_else(|| {
                    warn!(
                        "Replaying preemptions, but no record found for thread {}",
                        dettid
                    );
                    ThreadHistory::new()
                });
                let old_prio = sched.priorities.insert(dettid, history.initial_priority());
                (history, old_prio)
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
            thread_start_entered: _,
            timeslice_stats,
            syscall_count,
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
        sched.record_syscall_count(dettid, syscall_count);
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
            let mut sched = self.lock_rpc_scheduler(false).await;
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
            SchedResponse::Signaled(_) => Some(SchedValue::Value(nix::errno::Errno::EINTR as u64)),
        }
    }

    fn recv_robust_list_wakes(&self, wakes: Vec<(DetTid, FutexID)>) -> Vec<u64> {
        let mut sched = self.sched.lock().unwrap();
        sched.wake_futex_waiters_after_exit(&wakes)
    }

    async fn recv_determinize_inode(&self, from: Tid, ino: RawInode) -> (DetInode, LogicalTime) {
        let _sched = self.lock_rpc_scheduler(false).await;
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
        let _sched = self.lock_rpc_scheduler(false).await;
        let det_device = self.devices.lock().unwrap().determinize(raw_device);
        trace!(
            "[detcore, dtid {}] resolved (raw) device {} to {}",
            from, raw_device, det_device
        );
        det_device
    }

    async fn recv_determinize_mount_id(
        &self,
        from: Tid,
        raw_mount_id: u64,
        mountinfo_order: Option<&[u64]>,
    ) -> Option<u64> {
        let _sched = self.lock_rpc_scheduler(false).await;
        let virtual_mount_id = self
            .mount_ids
            .lock()
            .unwrap()
            .determinize(raw_mount_id, mountinfo_order);
        trace!(
            "[detcore, dtid {}] resolved fdinfo mount ID {} to {:?}",
            from, raw_mount_id, virtual_mount_id
        );
        virtual_mount_id
    }

    async fn recv_validate_mount_id_order(&self, from: Tid, mountinfo_order: &[u64]) -> bool {
        let _sched = self.lock_rpc_scheduler(false).await;
        let valid = self
            .mount_ids
            .lock()
            .unwrap()
            .validate_mountinfo_order(mountinfo_order);
        trace!(
            "[detcore, dtid {}] validated mountinfo identity order: {}",
            from, valid
        );
        valid
    }

    async fn recv_unlink_inode(&self, from: Tid, d_ino: DetInode) {
        let _sched = self.lock_rpc_scheduler(false).await;
        trace!("[detcore, dtid {}] unlink (det) inode {:?}", from, d_ino);
        self.inodes.lock().unwrap().remove_inode(d_ino);
    }

    async fn recv_touch_file(&self, from: Tid, ino: RawInode) {
        let _sched = self.lock_rpc_scheduler(false).await;
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
        let ev = {
            let sched = self.lock_rpc_scheduler(false).await;
            if !sched.rpc_incarnation_matches(ev.dettid, request_mm) {
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
            ev
        };

        // Yield this guest thread if needed to follow schedule.
        let result = if self.cfg.replay_schedule_from.is_some() {
            let (consumed, print_stack2) = {
                let mut sched = self.lock_rpc_scheduler(false).await;
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
                let mut sched = self.lock_rpc_scheduler(false).await;
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
            let _sched = self.lock_rpc_scheduler(false).await;
            trace!(
                "[dtid {}] signaling thread with {} at the point of stack trace printing.",
                ev.dettid, sig.0
            );
            let tid = Pid::from_raw(ev.dettid.as_raw());
            // TODO(T78538674): virtualize pid/tid:
            // We send a raw signal here and let the guest pick it up WHENEVER it resumes.
            // We don't use the "signal_guest" method because we don't necessarily respect that
            // protocol here.
            // Alarm/timer signals are guest-chosen and may be realtime, which
            // `nix` cannot name; fall through to the raw syscall for those.
            match sig.signal() {
                Some(named) => signal::kill(tid, named).unwrap(),
                None => {
                    // SAFETY: `tid` is a live thread this scheduler owns and
                    // `sig.raw()` is a signal number the guest already supplied.
                    let rc = unsafe { libc::kill(tid.as_raw(), sig.raw()) };
                    assert_eq!(rc, 0, "raw kill of signal {} failed", sig.raw());
                }
            }
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
        let mut sched = self.lock_rpc_scheduler(false).await;
        if sched.thread_is_logically_killed(dettid) || !sched.rpc_incarnation_matches(dettid, mm) {
            return SchedulerRpcResult::ThreadExited;
        }
        SchedulerRpcResult::Continue(sched.register_alarm(
            detpid,
            dettid,
            now,
            duration,
            interval,
            alarm_signal(sig),
        ))
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
        let mut sched = self.lock_rpc_scheduler(false).await;
        if sched.thread_is_logically_killed(dettid) || !sched.rpc_incarnation_matches(dettid, mm) {
            return SchedulerRpcResult::ThreadExited;
        }
        sched.register_posix_timer(
            detpid,
            dettid,
            timer_id,
            deadline,
            interval,
            alarm_signal(sig),
        );
        SchedulerRpcResult::Continue(())
    }
}

/// Identity and final accounting for an asynchronous scheduler deregistration.
#[derive(PartialEq, Debug, Eq, Clone, Serialize, Deserialize)]
pub struct ThreadDeregistration {
    pub(crate) dettid: DetTid,
    pub(crate) detpid: DetPid,
    pub(crate) mm: MmId,
    /// Carried by the consuming ThreadState owner, independently of detpid's
    /// delayed initialization and the parent's scheduler registration RPC.
    pub(crate) thread_start_entered: bool,
    pub(crate) timeslice_stats: TimesliceStats,
    pub(crate) syscall_count: u64,
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
    CreateChildThread(
        DetTid,
        DetPid,
        usize,
        Option<CloneFlags>,
        libc::c_int,
        Option<(i32, i32)>,
        Option<Priority>,
    ),

    /// A vfork child registering itself while its parent is blocked inside the
    /// kernel. Contains the (real) parent dettid and detpid, the child dettid,
    /// the child TID address, the clone flags, and the starting priority (absent
    /// only when replaying preemptions).
    CreateVforkChildThread(
        DetTid,
        DetPid,
        DetTid,
        usize,
        CloneFlags,
        libc::c_int,
        Option<Priority>,
    ),

    /// New thread is alive and waiting to run its first instruction.  Contains the dettid
    /// and detpid of the new child.
    StartNewThread(DetTid, DetPid, Option<(i32, i32)>),

    /// Remove a thread from scheduler data structures, guaranteeing that it will
    /// consume no further turns. Carries its final timeslice distribution and any
    /// chaos-epoch transitions not yet flushed by a priority-change commit.
    DeregisterThread(ThreadDeregistration),

    /// Replace the address cleared and woken when the calling thread exits.
    /// A zero address disables the exit-time store and wake.
    SetChildTidAddress(usize),

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

    /// Translate a host-assigned fdinfo mount ID to a run-local identity. The
    /// fallback order is populated only by low-level callers without captured
    /// namespace provenance.
    DeterminizeMountId(u64, Option<Vec<u64>>),

    /// Seed or validate the exact mountinfo row/parent identity order.
    ValidateMountIdOrder(Vec<u64>),

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
    /// Deterministically select a logically exited matching child.
    ReadyChildWait(DetPid, ChildWaitSpec),
    /// Retire a consumed terminal child wait status.
    ConsumeChildWait(DetPid, DetPid),
    /// Query scheduler-owned process-group membership.
    ProcessGroup(DetPid),
    /// Apply a successful setpgid transition.
    SetProcessGroup(DetPid, DetPid),
    /// Apply a successful setsid transition.
    CreateSession(DetPid),
    /// Query live threads before translating process-directed signal delivery.
    ResolveKillTargets(DetPid),
    /// A successful kill(2) queued a physical signal for this sole target.
    NotifySignalPending(DetTid, SigWrapper, Option<DetPid>),
    /// Liveness of one tid, leader or not; see [`thread_is_live`].
    ThreadIsLive(DetTid),
    /// Scheduler-owned lifecycle state for a direct child process.
    ExactChildWaitState(DetPid, DetPid),

    /// The container is shutting down.  Exit the scheduler "thread".
    UnrecoverableShutdown,

    // Request a port for an open file description.
    RequestPort(OpenFileId),

    // Add a port to the used-port list for an open file description.
    AddUsedPort(u16, OpenFileId),

    // Release the port when the last alias of its open file description closes.
    ReleasePort(OpenFileId),

    /// Deliver robust-futex wakes collected before exit after the backend has
    /// confirmed that Linux's physical task cleanup completed.
    RobustListWakes(Vec<(DetTid, FutexID)>),
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
    SetChildTidAddress(()),
    FutexAction(Option<SchedValue>),
    /// Return the mtime as well:
    DeterminizeInode((DetInode, LogicalTime)),
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1056): Deterministic st_dev remapping RPC.
    DeterminizeDevice(u64),
    DeterminizeMountId(Option<u64>),
    ValidateMountIdOrder(bool),
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
    ReadyChildWait((Option<DetPid>, bool)),
    ConsumeChildWait(bool),
    ProcessGroup(Option<DetPid>),
    SetProcessGroup(bool),
    CreateSession(bool),
    AlarmRemaining(LogicalTime),
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#663)
    ResolveKillTargets(Vec<DetTid>),
    NotifySignalPending(()),
    ThreadIsLive(bool),
    ExactChildWaitState(ExactChildWaitState),
    // TODO: use void_send_rpc, and remove this bogus response:
    UnrecoverableShutdown(()),

    RequestPort(u16),
    AddUsedPort,
    ReleasePort(Option<u16>),
    PortFull,
    RobustListWakes(Vec<u64>),
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
/// Notifies the coordinator that `guest` is about to `execve`, recording the
/// pre-exec address space `mm` and any file-descriptor blocking overrides. A
/// backend that handles `execve` outside Detcore's syscall handler must call
/// this before the native syscall so the next image reconnects to the existing
/// scheduler identity and logical clock.
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

/// Mirrors a successful `set_tid_address(2)` into scheduler-owned exit state.
pub(crate) async fn set_child_tid_address<G, T>(guest: &mut G, address: usize)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let (_, response) =
        send_and_update_time(guest, GlobalRequest::SetChildTidAddress(address)).await;
    assert_eq!(response, GlobalResponse::SetChildTidAddress(()));
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
    Signaled(Option<Vec<SigWrapper>>),
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
        let physical_ids = guest
            .thread_state()
            .physical_tid
            .map(|tid| (guest.pid().as_raw(), tid));
        let resp = send_and_update_time(
            guest,
            GlobalRequest::StartNewThread(dettid, detpid, physical_ids),
        )
        .await;
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

/// Keep only the clone pointer that requests an exit-time clear and wake.
/// SETTID alone requests a birth-time store and must not register an exit wake.
pub(crate) fn child_tid_clear_address(flags: CloneFlags, address: usize) -> usize {
    if flags.contains(CloneFlags::CLONE_CHILD_CLEARTID) {
        address
    } else {
        0
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
    exit_signal: libc::c_int,
    physical_ids: Option<(i32, i32)>,
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
        GlobalRequest::CreateChildThread(
            child_dettid,
            detpid,
            ctid,
            flags,
            exit_signal,
            physical_ids,
            starting_priority,
        ),
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
            vfork.exit_signal,
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

/// Account this physical-exit callback's own clock before it joins a staged
/// robust-list batch. The empty wake request has no scheduler effects, but its
/// ordinary RPC header is admitted and accounted before it is acknowledged.
/// A retired incarnation must not contribute to the completed-exit barrier.
pub(crate) async fn acknowledge_robust_list_exit_time<R>(
    threads_time: DetTime,
    reverie: &R,
    mm: MmId,
) -> bool
where
    R: GlobalRPC<GlobalState>,
{
    let response = reverie
        .send_rpc((threads_time, mm, GlobalRequest::RobustListWakes(Vec::new())))
        .await;
    match response.1 {
        GlobalResponse::RobustListWakes(counts) => {
            assert!(
                counts.is_empty(),
                "an empty exit-clock acknowledgement woke a waiter"
            );
            true
        }
        GlobalResponse::ThreadExited => false,
        _ => unreachable!(),
    }
}

/// Deliver owner-death wakes from an exit callback that no longer has guest
/// memory access. The callback runs only after ptrace has observed physical
/// exit, so Linux's atomic robust-list word update is already complete.
pub(crate) async fn robust_list_wakes_after_exit<R>(
    threads_time: DetTime,
    reverie: &R,
    mm: MmId,
    wakes: Vec<(DetTid, RobustListWake)>,
) -> Vec<u64>
where
    R: GlobalRPC<GlobalState>,
{
    if wakes.is_empty() {
        return Vec::new();
    }
    let response = reverie
        .send_rpc((
            threads_time,
            mm,
            GlobalRequest::RobustListWakes(
                wakes
                    .into_iter()
                    .map(|(owner, wake)| (owner, wake.futex))
                    .collect(),
            ),
        ))
        .await;
    match response.1 {
        GlobalResponse::RobustListWakes(counts) => counts,
        _ => unreachable!(),
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

/// Translate an observed fdinfo mount ID to the shared run-local identity.
pub async fn determinize_mount_id<G, T>(
    guest: &mut G,
    raw_mount_id: u64,
    mountinfo_order: Option<Vec<u64>>,
) -> Option<u64>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let resp = send_and_update_time(
        guest,
        GlobalRequest::DeterminizeMountId(raw_mount_id, mountinfo_order),
    )
    .await;
    match resp.1 {
        GlobalResponse::DeterminizeMountId(value) => value,
        _ => unreachable!(),
    }
}

/// Seed or validate the run-global mount-ID pool against one mountinfo snapshot.
pub async fn validate_mountinfo_identity_order<G, T>(
    guest: &mut G,
    mountinfo_order: Vec<u64>,
) -> bool
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let resp =
        send_and_update_time(guest, GlobalRequest::ValidateMountIdOrder(mountinfo_order)).await;
    match resp.1 {
        GlobalResponse::ValidateMountIdOrder(valid) => valid,
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
        GlobalRequest::RegisterAlarm(detpid, dettid, duration, interval, SigWrapper::from(sig)),
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
            SigWrapper::from(sig),
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
/// Does a live thread with this tid exist, leader or not?
///
/// Distinct from [`resolve_kill_targets`], which models `kill(2)` and therefore
/// only recognises thread-group leaders. Syscalls that resolve a task through
/// `find_task_by_vpid` -- `sched_setattr` among them -- must use this instead,
/// or a non-leader thread reports ESRCH while it is plainly running.
pub async fn thread_is_live<G, T>(guest: &mut G, dettid: DetTid) -> bool
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let response = send_and_update_time(guest, GlobalRequest::ThreadIsLive(dettid)).await;
    match response.1 {
        GlobalResponse::ThreadIsLive(live) => live,
        _ => unreachable!(),
    }
}

/// Return scheduler-owned lifecycle state for an exact child-process wait.
pub async fn exact_child_wait_state<G, T>(guest: &mut G, child: DetPid) -> ExactChildWaitState
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let parent = guest.thread_state().detpid.expect("detpid unset");
    let response =
        send_and_update_time(guest, GlobalRequest::ExactChildWaitState(parent, child)).await;
    match response.1 {
        GlobalResponse::ExactChildWaitState(state) => state,
        _ => unreachable!(),
    }
}

/// Wait without requesting a scheduler turn for a backend's physical-exit report.
pub async fn await_exact_child_physical_exit<G, T>(
    guest: &mut G,
    child: DetPid,
) -> ExactChildWaitState
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let mut state = exact_child_wait_state(guest, child).await;
    if matches!(
        state,
        ExactChildWaitState::PhysicalExitPending | ExactChildWaitState::PhysicallyExited
    ) {
        let dettid = guest.thread_state().dettid;
        let mut resources = Resources::new(dettid);
        resources.insert(ResourceID::WaitPhysicalChild(child), Permission::R);
        resources.fyi("wait-child-physical-exit");
        let _ = resource_request(guest, resources).await;
        state = exact_child_wait_state(guest, child).await;
    }
    state
}

/// Park until an exact or any-child process wait has a logical exit to reap.
pub async fn wait_for_child_lifecycle<G, T>(guest: &mut G, spec: ChildWaitSpec) -> ResumeStatus
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let dettid = guest.thread_state().dettid;
    let parent = guest.thread_state().detpid.expect("detpid unset");
    let mut resources = Resources::new(dettid);
    resources.insert(ResourceID::WaitChild { parent, spec }, Permission::R);
    resources.fyi("wait-child-lifecycle");
    resource_request(guest, resources).await
}

pub async fn ready_child_wait<G, T>(guest: &mut G, spec: ChildWaitSpec) -> (Option<DetPid>, bool)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let parent = guest.thread_state().detpid.expect("detpid unset");
    let response = send_and_update_time(guest, GlobalRequest::ReadyChildWait(parent, spec)).await;
    match response.1 {
        GlobalResponse::ReadyChildWait(snapshot) => snapshot,
        _ => unreachable!(),
    }
}

pub async fn process_group<G, T>(guest: &mut G, process: DetPid) -> Option<DetPid>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let response = send_and_update_time(guest, GlobalRequest::ProcessGroup(process)).await;
    match response.1 {
        GlobalResponse::ProcessGroup(group) => group,
        _ => unreachable!(),
    }
}

pub async fn set_process_group<G, T>(guest: &mut G, process: DetPid, group: DetPid) -> bool
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let response =
        send_and_update_time(guest, GlobalRequest::SetProcessGroup(process, group)).await;
    match response.1 {
        GlobalResponse::SetProcessGroup(updated) => updated,
        _ => unreachable!(),
    }
}

pub async fn create_session<G, T>(guest: &mut G, process: DetPid) -> bool
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let response = send_and_update_time(guest, GlobalRequest::CreateSession(process)).await;
    match response.1 {
        GlobalResponse::CreateSession(updated) => updated,
        _ => unreachable!(),
    }
}

pub async fn consume_child_wait<G, T>(guest: &mut G, child: DetPid) -> bool
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let parent = guest.thread_state().detpid.expect("detpid unset");
    let response =
        send_and_update_time(guest, GlobalRequest::ConsumeChildWait(parent, child)).await;
    match response.1 {
        GlobalResponse::ConsumeChildWait(consumed) => consumed,
        _ => unreachable!(),
    }
}

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

/// Tell the scheduler that a successful kill(2) left a physical
/// signal pending for a sole, known target.
/// The `nix` signal a timer will raise.
///
/// Timer registration still models a named signal: `setitimer`/`timer_create`
/// reach here with one, and the timer wheel stores it. Widening that is a
/// separate axis from the notification defect this change fixes, so the
/// conversion is asserted here rather than silently widened — a realtime timer
/// signal would be a NEW capability, not a regression of an existing one.
fn alarm_signal(sig: SigWrapper) -> Signal {
    sig.signal().unwrap_or_else(|| {
        panic!(
            "timer registration received unnameable signal {}",
            sig.raw()
        )
    })
}

pub async fn notify_signal_pending<G, T>(
    guest: &mut G,
    dettid: DetTid,
    signal: SigWrapper,
    target_process: Option<DetPid>,
) where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let response = send_and_update_time(
        guest,
        GlobalRequest::NotifySignalPending(dettid, signal, target_process),
    )
    .await;
    match response.1 {
        GlobalResponse::NotifySignalPending(()) => {}
        _ => unreachable!(),
    }
}

/// Signal an unrecoverable error that exits the entire container.
/// Such exits are not determinizable (see "quasi-determinism").
///
/// ⚠️ `status` IS NOT A DEFAULTABLE PARAMETER, AND THAT IS THE POINT. This
/// function has four callers and they do not all mean the same thing: three are
/// fail-closed policy refusals (`HERMIT_POLICY_REFUSAL_EXIT`) and one is an
/// operator interrupt (`HERMIT_SIGINT_DEATH_EXIT`). While the status was baked in
/// here, every caller inherited whatever the last edit chose, so the SIGINT path
/// reported "hermit refused this run" for a run hermit did not refuse. Making it
/// an argument turns the meaning into a visible claim at each call site, which is
/// the same rule `scripts/check-exit-status-class.rs` enforces on the test side:
/// a status you can read is worth less than a status that says which channel
/// produced it.
///
/// Adding a caller means choosing; there is deliberately no default to fall into.
pub async fn unrecoverable_shutdown<G, T>(guest: &G, status: i32) -> !
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
    //
    // ⚠️ THE STATUS IS THE ONLY THING THAT CROSSES THIS BOUNDARY, SO IT HAS TO
    // CARRY THE MEANING. This is a DELIBERATE, policy-driven shutdown: the run
    // hit an operation it cannot service under a fail-closed configuration and
    // hermit chose to stop. The parent sees only a container child that exited,
    // and its classifier treats an exit it cannot account for as unchosen — so
    // exiting 1 here made `classify_container_result` report
    // `class=container-child-exit` and 125, i.e. "hermit broke", for a shutdown
    // that worked exactly as designed. `HERMIT_POLICY_REFUSAL_EXIT` is the
    // agreed value for "hermit refused"; see its doc for why it is not 1.
    //
    // ⚠️ AND WHICH VALUE IS THE CALLER'S TO SAY, NOT THIS FUNCTION'S. Baking one
    // in here is what made an operator's Ctrl-C report as a policy refusal: the
    // condition differs per caller and only the caller knows it.
    //
    // The RPC above is `cfg!(debug_assertions)`-only, so it cannot be the
    // signal — in a release build the parent would learn nothing.
    std::process::exit(status);
}

#[cfg(test)]
mod tests {
    mod backend_failure_tests;
    use std::collections::BTreeSet;
    use std::os::fd::AsRawFd;
    use std::os::fd::FromRawFd;
    use std::os::fd::OwnedFd;
    use std::sync::Mutex;
    use std::time::Duration;

    use nix::sys::signal::Signal;
    use reverie::GlobalRPC;
    use reverie::GlobalTool;
    use reverie::Guest;
    use reverie::Tid;
    use reverie::syscalls::CloneFlags;

    use super::FutexAction;
    use super::GlobalRequest;
    use super::GlobalResponse;
    use super::GlobalState;
    use super::MountIdPool;
    use super::PendingExecState;
    use super::ResumeStatus;
    use super::RpcIncarnation;
    use super::SchedulerRpcResult;
    use super::SigWrapper;
    use super::ThreadDeregistration;
    use super::TimesliceStats;
    use super::format_unsupported_syscall_warning;
    use crate::Detcore;
    use crate::config::Config;
    use crate::config::RunsPostFork;
    use crate::ivar::Ivar;
    use crate::preemptions::PreemptionRecord;
    use crate::resources::ExternalOpId;
    use crate::resources::Permission;
    use crate::resources::ResourceID;
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

    #[test]
    fn fdinfo_mount_ids_preserve_raw_equivalence_and_distinctness() {
        let mut pool = MountIdPool::from_config(&[10, 20, 30], true, &[]);
        assert_eq!(pool.determinize(20, None), Some(2));

        // Unlisted nsfs, anon_inodefs, and pidfs IDs are distinct even when
        // their descriptors have the same broad Detcore FdType.
        assert_eq!(pool.determinize(700, None), Some(4));
        assert_eq!(pool.determinize(701, None), Some(5));
        assert_eq!(pool.determinize(702, None), Some(6));
        assert_eq!(pool.determinize(701, None), Some(5));
    }

    #[test]
    fn fdinfo_mount_ids_seed_from_low_level_snapshot_and_refuse_drift() {
        let mut pool = MountIdPool::from_config(&[], false, &[]);
        assert_eq!(pool.determinize(20, Some(&[10, 20, 30])), Some(2));
        assert_eq!(pool.determinize(700, Some(&[10, 20, 30])), Some(4));
        assert_eq!(pool.determinize(20, Some(&[10, 20, 30])), Some(2));
        assert_eq!(pool.determinize(20, Some(&[10, 99, 30])), None);
        assert_eq!(pool.determinize(20, Some(&[10, 20])), None);
        assert_eq!(pool.determinize(20, Some(&[10, 20, 30, 40])), None);
    }

    #[test]
    fn mountinfo_reads_seed_once_and_refuse_later_table_changes() {
        let mut pool = MountIdPool::from_config(&[], false, &[]);
        assert!(pool.validate_mountinfo_order(&[10, 20, 30]));
        assert!(pool.validate_mountinfo_order(&[10, 20, 30]));
        assert!(!pool.validate_mountinfo_order(&[10, 99, 30]));

        let mut empty = MountIdPool::from_config(&[], false, &[]);
        assert!(empty.validate_mountinfo_order(&[]));
        assert!(!empty.validate_mountinfo_order(&[10]));
    }

    #[test]
    fn configured_mountinfo_order_accepts_only_ordered_known_subsets() {
        let mut pool = MountIdPool::from_config(&[10, 20, 30, 40], true, &[]);
        assert!(pool.validate_mountinfo_order(&[10, 30, 40]));
        assert_eq!(pool.determinize(30, None), Some(3));
        assert!(pool.validate_mountinfo_order(&[20, 40]));
        assert!(!pool.validate_mountinfo_order(&[30, 20]));
        assert!(!pool.validate_mountinfo_order(&[10, 10]));
    }

    #[test]
    fn configured_mountinfo_order_refuses_new_namespace_ids() {
        let mut pool = MountIdPool::from_config(&[10, 20, 30, 40], true, &[]);
        assert!(pool.validate_mountinfo_order(&[10, 30, 40]));
        assert!(
            !pool.validate_mountinfo_order(&[10, 99, 40]),
            "a namespace-local mount ID absent from captured provenance must fail closed"
        );
    }

    #[test]
    fn fdinfo_mount_ids_refuse_malformed_configured_provenance() {
        let mut pool = MountIdPool::from_config(&[10, 10], true, &[]);
        assert_eq!(pool.determinize(10, None), None);
    }

    #[test]
    fn raw_zero_is_one_mount_identity_without_descriptor_type_partitioning() {
        let mut pool = MountIdPool::from_config(&[10, 0], true, &[]);
        // The pool API intentionally accepts no descriptor type: memfd and
        // every other Linux descriptor reporting raw mnt_id 0 share this one
        // equivalence class, even when mountinfo uses zero as an outside parent
        // ID for its root row.
        assert_eq!(pool.determinize(0, None), Some(0));
        assert_eq!(pool.determinize(20, None), Some(3));
        assert_eq!(pool.determinize(0, None), Some(0));
    }

    #[test]
    fn recorded_unlisted_mount_order_rebuilds_the_same_mapping() {
        let mut recording = MountIdPool::from_config(&[], false, &[]);
        assert!(recording.validate_mountinfo_order(&[10, 20]));
        assert_eq!(recording.determinize(700, None), Some(3));
        assert_eq!(recording.determinize(701, None), Some(4));
        let provenance = recording.provenance().unwrap().unwrap();

        let mut replay = MountIdPool::from_config(
            &provenance.mountinfo_order,
            true,
            &provenance.unlisted_order,
        );
        assert_eq!(replay.determinize(701, None), Some(4));
        assert_eq!(replay.determinize(700, None), Some(3));
    }

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

    // Exercise the real external registration method and global RPC without a
    // backend or a guest process. Any unexpected guest operation fails the test.
    struct ExternalRegistrationGuest<'a> {
        global: &'a GlobalState,
        config: &'a Config,
        thread: crate::ThreadState<()>,
        requests: Mutex<Vec<GlobalRequest>>,
    }

    struct ExternalRegistrationStack;
    struct ExternalRegistrationStackGuard;

    impl Drop for ExternalRegistrationStackGuard {
        fn drop(&mut self) {}
    }

    impl reverie::Stack for ExternalRegistrationStack {
        type StackGuard = ExternalRegistrationStackGuard;

        fn size(&self) -> usize {
            panic!("external registration must not use a guest stack")
        }

        fn capacity(&self) -> usize {
            panic!("external registration must not use a guest stack")
        }

        fn push<'stack, T>(&mut self, _value: T) -> reverie::syscalls::Addr<'stack, T> {
            panic!("external registration must not use a guest stack")
        }

        fn reserve<'stack, T>(&mut self) -> reverie::syscalls::AddrMut<'stack, T> {
            panic!("external registration must not use a guest stack")
        }

        fn commit(self) -> Result<Self::StackGuard, reverie::syscalls::Errno> {
            panic!("external registration must not use a guest stack")
        }
    }

    #[reverie::tool]
    impl GlobalRPC<GlobalState> for ExternalRegistrationGuest<'_> {
        async fn send_rpc(
            &self,
            message: <GlobalState as GlobalTool>::Request,
        ) -> <GlobalState as GlobalTool>::Response {
            self.requests.lock().unwrap().push(message.2.clone());
            self.global
                .receive_rpc(Tid::from_raw(self.thread.dettid.as_raw()), message)
                .await
        }

        fn config(&self) -> &Config {
            self.config
        }
    }

    #[reverie::tool]
    impl Guest<Detcore> for ExternalRegistrationGuest<'_> {
        type Memory = reverie::syscalls::LocalMemory;
        type Stack = ExternalRegistrationStack;

        fn tid(&self) -> reverie::Pid {
            reverie::Pid::from_raw(self.thread.dettid.as_raw())
        }

        fn pid(&self) -> reverie::Pid {
            reverie::Pid::from_raw(self.thread.detpid.unwrap().as_raw())
        }

        fn ppid(&self) -> Option<reverie::Pid> {
            None
        }

        fn memory(&self) -> Self::Memory {
            panic!("external registration must not access guest memory")
        }

        fn thread_state_mut(&mut self) -> &mut crate::ThreadState<()> {
            &mut self.thread
        }

        fn thread_state(&self) -> &crate::ThreadState<()> {
            &self.thread
        }

        async fn regs(&mut self) -> libc::user_regs_struct {
            panic!("external registration must not read guest registers")
        }

        async fn stack(&mut self) -> Self::Stack {
            panic!("external registration must not use a guest stack")
        }

        async fn daemonize(&mut self) {
            panic!("external registration must not daemonize")
        }

        async fn inject<S: reverie::syscalls::SyscallInfo>(
            &mut self,
            _syscall: S,
        ) -> Result<i64, reverie::syscalls::Errno> {
            panic!("external registration must not inject a syscall")
        }

        async fn tail_inject<S: reverie::syscalls::SyscallInfo>(
            &mut self,
            _syscall: S,
        ) -> reverie::Never {
            panic!("external registration unexpectedly retired its live parent")
        }

        fn set_timer(&mut self, _schedule: reverie::TimerSchedule) -> Result<(), reverie::Error> {
            panic!("external registration must not set a timer")
        }

        fn set_timer_precise(
            &mut self,
            _schedule: reverie::TimerSchedule,
        ) -> Result<(), reverie::Error> {
            panic!("external registration must not set a timer")
        }

        fn read_clock(&mut self) -> Result<u64, reverie::Error> {
            panic!("external registration must not read a host clock")
        }
    }

    async fn check_external_child_tid_registration(
        flags: CloneFlags,
        supplied_address: usize,
        expected_address: usize,
    ) {
        use reverie::Tool;

        let config = Config {
            sequentialize_threads: true,
            cancel_killed_thread_rpcs: true,
            // This control observes registration before the child starts; use
            // the supported parent-first order for the one turn it drives.
            runs_post_fork: crate::RunsPostFork::Parent,
            ..Config::default()
        };
        let state = GlobalState::initialize(&config, false);
        let parent = DetTid::from_raw(17);
        let parent_pid = DetPid::from_raw(17);
        state
            .sched
            .lock()
            .unwrap()
            .thread_tree
            .add_child(parent, parent, true);
        install_test_registration(&state, parent, Ivar::new());
        let tool = Detcore::new(reverie::Pid::from_raw(parent.as_raw()), &config);
        let mut thread = tool.init_thread_state(Tid::from_raw(parent.as_raw()), None);
        thread.detpid = Some(parent_pid);
        let mut guest = ExternalRegistrationGuest {
            global: &state,
            config: &config,
            thread,
            requests: Mutex::new(Vec::new()),
        };
        let child = DetTid::from_raw(18);
        let exit_signal = if flags.contains(CloneFlags::CLONE_THREAD) {
            0
        } else {
            libc::SIGCHLD
        };
        tokio::time::timeout(Duration::from_secs(2), async {
            let mut registration = std::pin::pin!(tool.register_external_child(
                &mut guest,
                Tid::from_raw(child.as_raw()),
                supplied_address,
                flags,
                exit_signal,
                None,
            ));
            assert!(futures::poll!(registration.as_mut()).is_pending());
            // The production RPC parks the parent on ParentContinue. Drive
            // that actual scheduler turn instead of pre-filling a response or
            // disabling sequentialization to make registration return.
            let committed = crate::scheduler::do_a_turn_blocking(
                state.sched.clone(),
                state.global_time.clone(),
                &Err(crate::scheduler::SkipTurn),
            )
            .await
            .expect("the parent continuation must commit");
            assert_eq!(committed.tid, parent);
            assert_eq!(
                committed.resources,
                std::collections::HashMap::from([(
                    crate::resources::ResourceID::ParentContinue { parent, child },
                    crate::resources::Permission::W,
                )]),
            );
            registration.await;
        })
        .await
        .expect("external child registration must return without running a guest");

        assert_eq!(guest.thread.clone_flags, None);
        assert_eq!(guest.thread.dettid, parent);
        let mut scheduler = state.sched.lock().unwrap();
        assert_eq!(scheduler.next_turns.len(), 2);
        assert!(scheduler.next_turns.contains_key(&parent));
        assert_eq!(
            scheduler.next_turns[&child].child_tid_addr,
            expected_address
        );
        assert_eq!(
            *guest.requests.lock().unwrap(),
            vec![GlobalRequest::CreateChildThread(
                child,
                parent_pid,
                expected_address,
                Some(flags),
                exit_signal,
                None,
                Some(DEFAULT_PRIORITY),
            )],
            "external registration must send one correctly gated real RPC"
        );
        let child_pid = if flags.contains(CloneFlags::CLONE_THREAD) {
            parent_pid
        } else {
            child
        };
        let child_mm = MmId::for_clone(
            MmId::initial(parent_pid),
            child,
            flags.contains(CloneFlags::CLONE_VM),
        );
        scheduler.logically_kill_thread(&child, &child_pid, child_mm);
        assert!(!scheduler.next_turns.contains_key(&child));
        assert!(scheduler.next_turns.contains_key(&parent));
        assert_eq!(
            scheduler.child_tid_was_cleared(
                FutexID::private(child_mm, supplied_address),
                child.as_raw(),
            ),
            expected_address != 0,
            "exit must use only the registered child-TID address"
        );
        assert!(!scheduler.child_tid_was_cleared(FutexID::private(child_mm, 0), child.as_raw(),));
        assert!(!scheduler.child_tid_was_cleared(
            FutexID::private(child_mm, supplied_address),
            parent.as_raw(),
        ));
    }

    #[tokio::test]
    async fn external_registration_without_child_cleartid_disables_exit_wake() {
        for kind in [
            CloneFlags::empty(),
            CloneFlags::CLONE_THREAD | CloneFlags::CLONE_VM | CloneFlags::CLONE_SIGHAND,
        ] {
            for registration in [CloneFlags::empty(), CloneFlags::CLONE_CHILD_SETTID] {
                check_external_child_tid_registration(kind | registration, 0x1234, 0).await;
            }
        }
    }

    #[tokio::test]
    async fn external_registration_with_child_cleartid_preserves_exact_exit_wake() {
        for kind in [
            CloneFlags::empty(),
            CloneFlags::CLONE_THREAD | CloneFlags::CLONE_VM | CloneFlags::CLONE_SIGHAND,
        ] {
            for registration in [
                CloneFlags::CLONE_CHILD_CLEARTID,
                CloneFlags::CLONE_CHILD_CLEARTID | CloneFlags::CLONE_CHILD_SETTID,
            ] {
                check_external_child_tid_registration(kind | registration, 0x1234, 0x1234).await;
                check_external_child_tid_registration(kind | registration, 0, 0).await;
            }
        }
    }

    #[tokio::test]
    async fn set_child_tid_address_rpc_updates_and_resets_the_registration() {
        let (config, state, dettid, detpid) = cancellation_test_state();
        install_test_registration(&state, dettid, Ivar::new());
        state
            .sched
            .lock()
            .unwrap()
            .next_turns
            .get_mut(&dettid)
            .unwrap()
            .child_tid_addr = 0x1234;

        let response = state
            .receive_rpc(
                reverie::Tid::from_raw(dettid.as_raw()),
                (
                    DetTime::new(&config),
                    MmId::initial(detpid),
                    GlobalRequest::SetChildTidAddress(0),
                ),
            )
            .await;

        assert_eq!(response.1, GlobalResponse::SetChildTidAddress(()));
        assert_eq!(
            state
                .sched
                .lock()
                .unwrap()
                .next_turns
                .get(&dettid)
                .unwrap()
                .child_tid_addr,
            0
        );
    }

    async fn child_start_clock_trajectory(
        start_before_selection: bool,
        child_first: bool,
    ) -> (Vec<LogicalTime>, Vec<DetTid>, bool) {
        let config = Config {
            sequentialize_threads: true,
            runs_post_fork: if child_first {
                RunsPostFork::Child
            } else {
                RunsPostFork::Parent
            },
            ..Config::default()
        };
        let state = GlobalState::initialize(&config, false);
        let parent = DetTid::from_raw(17);
        let parent_pid = parent;
        state
            .sched
            .lock()
            .unwrap()
            .thread_tree
            .add_child(parent, parent, true);
        let child = DetTid::from_raw(parent.as_raw() + 1);
        let parent_mm = MmId::initial(parent_pid);
        let child_mm = MmId::for_clone(parent_mm, child, false);
        install_test_registration(&state, parent, Ivar::new());
        let epoch = DetTime::new(&config).as_nanos();
        let mut parent_clock = DetTime::new(&config);
        parent_clock.advance_to(epoch + LogicalTime::from_nanos(1_001));
        let child_clock = parent_clock.clone_for_child();

        // Polling the real parent RPC publishes both admission and
        // ParentContinue before it waits, just as the clone handler does.
        let mut registration = Box::pin(state.receive_rpc(
            Tid::from_raw(parent.as_raw()),
            (
                parent_clock.clone(),
                parent_mm,
                GlobalRequest::CreateChildThread(
                    child,
                    parent_pid,
                    0,
                    Some(CloneFlags::empty()),
                    libc::SIGCHLD,
                    None,
                    Some(DEFAULT_PRIORITY),
                ),
            ),
        ));
        assert!(futures::poll!(&mut registration).is_pending());
        let child_request = state.sched.lock().unwrap().next_turns[&child].req.clone();
        let mut startup = Box::pin(state.receive_rpc(
            Tid::from_raw(child.as_raw()),
            (
                child_clock.clone(),
                child_mm,
                GlobalRequest::StartNewThread(child, child, None),
            ),
        ));
        if start_before_selection {
            // recv_start_new_thread yields once before filling its request.
            assert!(futures::poll!(&mut startup).is_pending());
            assert!(futures::poll!(&mut startup).is_pending());
            assert!(child_request.try_read().is_some());
        }

        let skipped = Err(crate::scheduler::SkipTurn);
        let mut turn = Box::pin(crate::scheduler::do_a_turn_blocking(
            state.sched.clone(),
            state.global_time.clone(),
            &skipped,
        ));
        if !start_before_selection && child_first {
            assert!(futures::poll!(&mut turn).is_pending());
            assert_eq!(child_request.to_string(), "<ivar HasWaiter>");
            assert!(futures::poll!(&mut startup).is_pending());
            assert!(futures::poll!(&mut startup).is_pending());
            assert!(child_request.try_read().is_some());
        }
        let first = turn.await.expect("first post-fork turn must commit");
        if !start_before_selection && !child_first {
            assert!(child_request.try_read().is_none());
            assert!(futures::poll!(&mut startup).is_pending());
            assert!(futures::poll!(&mut startup).is_pending());
            assert!(child_request.try_read().is_some());
        }
        let child_resource = ResourceID::MemAddrSpace(child);
        let parent_resource = ResourceID::ParentContinue { parent, child };
        let (first_tid, first_resource, first_permission, second_resource, second_permission) =
            if child_first {
                (
                    child,
                    &child_resource,
                    Permission::RW,
                    &parent_resource,
                    Permission::W,
                )
            } else {
                (
                    parent,
                    &parent_resource,
                    Permission::W,
                    &child_resource,
                    Permission::RW,
                )
            };
        assert_eq!(first.tid, first_tid);
        assert_eq!(first.resources.len(), 1);
        assert_eq!(first.resources.get(first_resource), Some(&first_permission));
        if child_first {
            assert_eq!(
                startup.as_mut().await,
                (None, GlobalResponse::StartNewThread(None))
            );
        } else {
            assert_eq!(
                registration.as_mut().await,
                (None, GlobalResponse::CreateChildThread(None))
            );
        }
        let first_time = state.sched.lock().unwrap().committed_time;
        assert_eq!(first_time, epoch + LogicalTime::from_nanos(1_001));
        assert_eq!(
            state.global_time.lock().unwrap().threads_time(child),
            child_clock.as_nanos()
        );

        // The selected thread's first new nanosecond and the ordinary scheduler
        // increment must both remain observable on the other thread's turn.
        let (mut first_clock, first_mm) = if child_first {
            (child_clock, child_mm)
        } else {
            (parent_clock, parent_mm)
        };
        first_clock.advance_to(first_clock.as_nanos() + LogicalTime::from_nanos(1));
        let mut resources = Resources::new(first_tid);
        resources.insert(ResourceID::MemAddrSpace(first_tid), Permission::RW);
        let mut work = Box::pin(state.receive_rpc(
            Tid::from_raw(first_tid.as_raw()),
            (
                first_clock,
                first_mm,
                GlobalRequest::RequestResources(resources, first_tid),
            ),
        ));
        assert!(futures::poll!(&mut work).is_pending());
        let second = crate::scheduler::do_a_turn_blocking(
            state.sched.clone(),
            state.global_time.clone(),
            &Ok(first),
        )
        .await
        .expect("other thread's continuation must commit");
        assert_eq!(second.resources.len(), 1);
        assert_eq!(
            second.resources.get(second_resource),
            Some(&second_permission)
        );
        if child_first {
            assert_eq!(
                registration.await,
                (None, GlobalResponse::CreateChildThread(None))
            );
        } else {
            assert_eq!(startup.await, (None, GlobalResponse::StartNewThread(None)));
        }
        let mut scheduler = state.sched.lock().unwrap();
        let next_time = scheduler.committed_time;
        assert_eq!(next_time, epoch + LogicalTime::from_nanos(501_002));
        let queue = scheduler.run_queue.tids().copied().collect();
        let next_random = scheduler.child_runs_first_post_fork(RunsPostFork::Random);
        (vec![first_time, next_time], queue, next_random)
    }

    #[tokio::test]
    async fn child_start_clock_is_independent_of_first_rpc_arrival() {
        for child_first in [true, false] {
            assert_eq!(
                child_start_clock_trajectory(true, child_first).await,
                child_start_clock_trajectory(false, child_first).await,
            );
        }
    }

    #[tokio::test]
    async fn vfork_registration_does_not_charge_inherited_work_before_startup() {
        let (config, state, parent, parent_pid) = cancellation_test_state();
        let child = DetTid::from_raw(parent.as_raw() + 1);
        let mm = MmId::initial(parent_pid);
        install_test_registration(&state, parent, Ivar::new());
        let mut parent_clock = DetTime::new(&config);
        let epoch = parent_clock.as_nanos();
        parent_clock.advance_to(epoch + LogicalTime::from_nanos(1_001));
        let child_clock = parent_clock.clone_for_child();
        let mut resources = Resources::new(parent);
        resources.insert(
            ResourceID::BlockingVfork(ExternalOpId::new(parent, 1)),
            Permission::RW,
        );
        let mut blocking = Box::pin(state.receive_rpc(
            Tid::from_raw(parent.as_raw()),
            (
                parent_clock,
                mm,
                GlobalRequest::RequestResources(resources, parent_pid),
            ),
        ));
        assert!(futures::poll!(&mut blocking).is_pending());
        let background = crate::scheduler::do_a_turn_blocking(
            state.sched.clone(),
            state.global_time.clone(),
            &Err(crate::scheduler::SkipTurn),
        )
        .await;
        assert!(background.is_err());
        assert_eq!(
            blocking.await,
            (None, GlobalResponse::RequestResources(ResumeStatus::Normal))
        );
        let before_child = state.global_time.lock().unwrap().as_nanos();
        assert_eq!(before_child, epoch + LogicalTime::from_nanos(1_001));

        // Unlike ordinary clone, this first RPC is sent by the child itself.
        let created = state
            .receive_rpc(
                Tid::from_raw(child.as_raw()),
                (
                    child_clock.clone(),
                    mm,
                    GlobalRequest::CreateVforkChildThread(
                        parent,
                        parent_pid,
                        child,
                        0,
                        CloneFlags::CLONE_VFORK | CloneFlags::CLONE_VM,
                        libc::SIGCHLD,
                        Some(DEFAULT_PRIORITY - 1),
                    ),
                ),
            )
            .await;
        assert_eq!(created, (None, GlobalResponse::CreateChildThread(None)));
        assert_eq!(state.global_time.lock().unwrap().as_nanos(), before_child);
        assert_eq!(
            state.global_time.lock().unwrap().threads_time(child),
            child_clock.as_nanos()
        );

        let mut startup = Box::pin(state.receive_rpc(
            Tid::from_raw(child.as_raw()),
            (
                child_clock,
                mm,
                GlobalRequest::StartNewThread(child, child, None),
            ),
        ));
        assert!(futures::poll!(&mut startup).is_pending());
        assert!(futures::poll!(&mut startup).is_pending());
        let first = crate::scheduler::do_a_turn_blocking(
            state.sched.clone(),
            state.global_time.clone(),
            &background,
        )
        .await
        .expect("vfork child must receive its first turn");
        assert_eq!(first.tid, child);
        assert_eq!(startup.await, (None, GlobalResponse::StartNewThread(None)));
        assert_eq!(state.global_time.lock().unwrap().as_nanos(), before_child);
    }

    #[tokio::test]
    async fn first_nonstartup_rpc_counts_only_new_child_work() {
        let config = Config {
            sequentialize_threads: false,
            ..Config::default()
        };
        let state = GlobalState::initialize(&config, false);
        let parent = DetTid::from_raw(17);
        let child = DetTid::from_raw(18);
        let mut parent_clock = DetTime::new(&config);
        let epoch = parent_clock.as_nanos();
        parent_clock.advance_to(epoch + LogicalTime::from_nanos(101));
        let mut child_clock = parent_clock.clone_for_child();
        let _ = state
            .receive_rpc(
                Tid::from_raw(parent.as_raw()),
                (
                    parent_clock,
                    MmId::initial(parent),
                    GlobalRequest::GlobalTimeLowerBound,
                ),
            )
            .await;
        child_clock.advance_to(child_clock.as_nanos() + LogicalTime::from_nanos(1));
        let observed = state
            .receive_rpc(
                Tid::from_raw(child.as_raw()),
                (
                    child_clock,
                    MmId::initial(child),
                    GlobalRequest::GlobalTimeLowerBound,
                ),
            )
            .await;
        assert_eq!(
            observed,
            (
                None,
                GlobalResponse::GlobalTimeLowerBound(epoch + LogicalTime::from_nanos(102))
            )
        );
    }

    #[tokio::test]
    async fn exec_reconnect_retains_inherited_work_accounting_across_local_reload() {
        let (config, state, leader, detpid) = cancellation_test_state();
        let ancestor = DetTid::from_raw(leader.as_raw() - 1);
        let worker = DetTid::from_raw(leader.as_raw() + 1);
        let old_mm = MmId::initial(detpid);
        install_test_registration(&state, leader, Ivar::new());
        state
            .sched
            .lock()
            .unwrap()
            .thread_tree
            .add_child(leader, worker, false);
        install_test_registration(&state, worker, Ivar::new());

        let mut ancestor_clock = DetTime::new(&config);
        let epoch = ancestor_clock.as_nanos();
        ancestor_clock.advance_to(epoch + LogicalTime::from_nanos(1_000));
        let mut leader_clock = ancestor_clock.clone_for_child();
        leader_clock.advance_to(epoch + LogicalTime::from_nanos(1_100));
        let mut worker_clock = leader_clock.clone_for_child();
        worker_clock.advance_to(epoch + LogicalTime::from_nanos(1_350));
        for (tid, clock) in [
            (ancestor, ancestor_clock),
            (leader, leader_clock),
            (worker, worker_clock.clone()),
        ] {
            let _ = state
                .receive_rpc(
                    Tid::from_raw(tid.as_raw()),
                    (clock, old_mm, GlobalRequest::GlobalTimeLowerBound),
                )
                .await;
        }
        let total = state.global_time.lock().unwrap().as_nanos();
        assert_eq!(total, epoch + LogicalTime::from_nanos(1_350));
        // A failed exec cancels its pending transfer without changing either
        // inherited component. The next successful attempt must use the same
        // clocks, not charge either component's inherited work again.
        let _ = state
            .receive_rpc(
                Tid::from_raw(worker.as_raw()),
                (
                    worker_clock.clone(),
                    old_mm,
                    GlobalRequest::PrepareExec(detpid, old_mm, Default::default()),
                ),
            )
            .await;
        let cancelled = state
            .receive_rpc(
                Tid::from_raw(worker.as_raw()),
                (
                    worker_clock.clone(),
                    old_mm,
                    GlobalRequest::CancelExec(detpid),
                ),
            )
            .await;
        assert_eq!(cancelled, (None, GlobalResponse::CancelExec(())));
        assert!(state.pending_exec_states.lock().unwrap().is_empty());
        assert_eq!(state.global_time.lock().unwrap().as_nanos(), total);
        assert_eq!(
            state.global_time.lock().unwrap().threads_time(worker),
            worker_clock.as_nanos()
        );

        let prepared = state
            .receive_rpc(
                Tid::from_raw(worker.as_raw()),
                (
                    worker_clock.clone(),
                    old_mm,
                    GlobalRequest::PrepareExec(detpid, old_mm, Default::default()),
                ),
            )
            .await;
        assert_eq!(prepared, (None, GlobalResponse::PrepareExec(())));

        let mut fresh = DetTime::new(&config);
        let recreated = state
            .receive_rpc(
                Tid::from_raw(leader.as_raw()),
                (
                    fresh.clone(),
                    MmId::initial(leader),
                    GlobalRequest::CreateChildThread(
                        leader,
                        detpid,
                        0,
                        None,
                        libc::SIGCHLD,
                        None,
                        Some(DEFAULT_PRIORITY),
                    ),
                ),
            )
            .await;
        assert_eq!(
            recreated,
            (
                Some(worker_clock.as_nanos()),
                GlobalResponse::CreateChildThread(Some(old_mm.for_exec(detpid)))
            )
        );
        // A delayed request from the destroyed image must be rejected before
        // its absolute clock or its inherited metadata reaches accounting.
        let stale = state
            .receive_rpc(
                Tid::from_raw(leader.as_raw()),
                (
                    worker_clock.clone(),
                    old_mm,
                    GlobalRequest::RequestResources(Resources::new(leader), detpid),
                ),
            )
            .await;
        assert_eq!(stale, (None, GlobalResponse::ThreadExited));
        assert_eq!(state.global_time.lock().unwrap().as_nanos(), total);

        fresh.advance_to(recreated.0.unwrap());
        assert_eq!(fresh.inherited_nanos(), LogicalTime::ZERO);
        let mut startup = Box::pin(state.receive_rpc(
            Tid::from_raw(leader.as_raw()),
            (
                fresh.clone(),
                old_mm.for_exec(detpid),
                GlobalRequest::StartNewThread(leader, detpid, None),
            ),
        ));
        assert!(futures::poll!(&mut startup).is_pending());
        assert!(futures::poll!(&mut startup).is_pending());
        let first = crate::scheduler::do_a_turn_blocking(
            state.sched.clone(),
            state.global_time.clone(),
            &Err(crate::scheduler::SkipTurn),
        )
        .await
        .expect("replacement leader must run");
        assert_eq!(first.tid, leader);
        assert_eq!(startup.await, (None, GlobalResponse::StartNewThread(None)));
        assert_eq!(state.global_time.lock().unwrap().as_nanos(), total);

        fresh.advance_to(fresh.as_nanos() + LogicalTime::from_nanos(1));
        let observed = state
            .receive_rpc(
                Tid::from_raw(leader.as_raw()),
                (
                    fresh.clone(),
                    old_mm.for_exec(detpid),
                    GlobalRequest::GlobalTimeLowerBound,
                ),
            )
            .await;
        assert_eq!(
            observed,
            (
                None,
                GlobalResponse::GlobalTimeLowerBound(total + LogicalTime::from_nanos(1))
            )
        );
        let global = state.global_time.lock().unwrap();
        assert_eq!(global.threads_time(leader), fresh.as_nanos());
        assert!(!global.contains_thread(worker));
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
        state.global_time.lock().unwrap().update_global_time(
            dettid,
            existing_time.as_nanos(),
            LogicalTime::ZERO,
        );
        let (global_before, thread_before) = {
            let global_time = state.global_time.lock().unwrap();
            (global_time.as_nanos(), global_time.threads_time(dettid))
        };
        let fresh_local_time = DetTime::new(&config);
        let physical_pid = std::process::id() as i32;
        let physical_tid = unsafe { libc::syscall(libc::SYS_gettid) as i32 };
        let physical_ids = Some((physical_pid, physical_tid));
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
                        libc::SIGCHLD,
                        physical_ids,
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
        assert_eq!(
            state.sched.lock().unwrap().physical_thread_identity(dettid),
            Some((old_mm.for_exec(detpid), physical_pid, physical_tid)),
            "post-exec host identity must be installed by CreateChildThread before admission"
        );

        let start_response = state
            .receive_rpc(
                reverie::Tid::from_raw(dettid.as_raw()),
                (
                    fresh_local_time,
                    old_mm.for_exec(detpid),
                    GlobalRequest::StartNewThread(dettid, detpid, physical_ids),
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
            global_time.update_global_time(leader, leader_clock.as_nanos(), LogicalTime::ZERO);
            global_time.update_global_time(worker, worker_clock.as_nanos(), LogicalTime::ZERO);
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
                        libc::SIGCHLD,
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
                        thread_start_entered: true,
                        dettid: leader,
                        detpid,
                        mm: old_mm,
                        timeslice_stats: TimesliceStats::default(),
                        syscall_count: 0,
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
                        libc::SIGCHLD,
                        None,
                        Some(DEFAULT_PRIORITY),
                    ),
                ),
            )
            .await;
        assert_eq!(duplicate_create, (None, GlobalResponse::ThreadExited));

        {
            let scheduler = state.sched.lock().unwrap();
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
        }

        // Drive the real daemon through step2 rather than manually pre-filling
        // the replacement's response. This drains the destroyed leader's
        // physical removal and the same-raw-TID replacement admission before
        // StartNewThread supplies the fresh image's first request.
        let turn_sched = state.sched.clone();
        let turn_time = state.global_time.clone();
        let turn = tokio::spawn(async move {
            let last: Result<Resources, crate::scheduler::SkipTurn> =
                Err(crate::scheduler::SkipTurn);
            crate::scheduler::do_a_turn_blocking(turn_sched, turn_time, &last).await
        });
        let start_response = state
            .receive_rpc(
                reverie::Tid::from_raw(leader.as_raw()),
                (
                    fresh_local_time,
                    old_mm.for_exec(detpid),
                    GlobalRequest::StartNewThread(leader, detpid, None),
                ),
            )
            .await;
        assert!(
            turn.await
                .expect("replacement scheduler turn panicked")
                .is_ok(),
            "replacement leader did not survive the first step2 drain"
        );
        assert_eq!(
            start_response,
            (
                Some(worker_clock.as_nanos()),
                GlobalResponse::StartNewThread(None)
            )
        );
        {
            let scheduler = state.sched.lock().unwrap();
            assert_eq!(
                scheduler
                    .run_queue
                    .tids()
                    .filter(|dettid| **dettid == leader)
                    .count(),
                1
            );
            assert!(!scheduler.run_queue.contains_tid(worker));
            assert!(!scheduler.run_queue.contains_tid(sibling));
        }
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
                    thread_start_entered: true,
                    dettid: leader,
                    detpid,
                    mm: MmId::initial(detpid),
                    timeslice_stats: TimesliceStats::default(),
                    syscall_count: 0,
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
                    thread_start_entered: true,
                    dettid: leader,
                    detpid,
                    mm: MmId::initial(detpid).for_exec(detpid),
                    timeslice_stats: TimesliceStats::default(),
                    syscall_count: 0,
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

    #[test]
    fn mountinfo_prepopulation_is_reused_by_later_stat_observations() {
        use super::DevicePool;

        let mountinfo_devices = [libc::makedev(8, 1), libc::makedev(0, 44)];
        let mut pool = DevicePool::new();
        let rendered = mountinfo_devices
            .into_iter()
            .map(|raw| pool.determinize(raw))
            .collect::<Vec<_>>();

        assert_eq!(rendered, [1, 2]);
        assert_eq!(pool.determinize(libc::makedev(0, 44)), rendered[1]);
        assert_eq!(pool.determinize(libc::makedev(8, 1)), rendered[0]);
        assert_eq!(pool.determinize(libc::makedev(259, 7)), 3);
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
        state.global_time.lock().unwrap().update_global_time(
            dettid,
            current_time.as_nanos(),
            LogicalTime::ZERO,
        );
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
        state.global_time.lock().unwrap().update_global_time(
            dettid,
            current_time.as_nanos(),
            LogicalTime::ZERO,
        );
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
                        thread_start_entered: true,
                        dettid,
                        detpid,
                        mm: MmId::initial(detpid),
                        timeslice_stats: final_stats,
                        syscall_count: 17,
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
        assert_eq!(
            state.sched.lock().unwrap().per_thread_syscalls.get(&dettid),
            Some(&17)
        );

        late_time.add_syscall();
        let duplicate_response = state
            .receive_rpc(
                reverie::Tid::from_raw(dettid.as_raw()),
                (
                    late_time,
                    MmId::initial(dettid),
                    GlobalRequest::DeregisterThread(ThreadDeregistration {
                        thread_start_entered: true,
                        dettid,
                        detpid,
                        mm: MmId::initial(detpid),
                        timeslice_stats: final_stats,
                        syscall_count: 99,
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
        assert_eq!(
            state.sched.lock().unwrap().per_thread_syscalls.get(&dettid),
            Some(&17),
            "a duplicate deregistration must not double-count or replace final accounting"
        );
        let summary = state
            .sched
            .lock()
            .unwrap()
            .generate_partial_run_summary(None)
            .unwrap();
        assert_eq!(summary.syscalls, Some(17));
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
                        libc::SIGCHLD,
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
                GlobalRequest::StartNewThread(dettid, detpid, None),
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
    async fn required_physical_thread_id_missing_is_terminal() {
        let config = Config {
            sequentialize_threads: true,
            cancel_killed_thread_rpcs: true,
            backend_requires_thread_directed_process_signals: true,
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
        install_test_registration(&state, dettid, Ivar::new());

        let response = state
            .receive_rpc(
                reverie::Tid::from_raw(dettid.as_raw()),
                (
                    DetTime::new(&config),
                    MmId::initial(detpid),
                    GlobalRequest::StartNewThread(dettid, detpid, None),
                ),
            )
            .await;

        assert_eq!(response, (None, GlobalResponse::ThreadExited));
        let scheduler = state.sched.lock().unwrap();
        assert!(!scheduler.next_turns.contains_key(&dettid));
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
                SigWrapper::from(Signal::SIGALRM),
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
                SigWrapper::from(Signal::SIGALRM),
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
        let physical_pid = std::process::id() as i32;
        let physical_tid = unsafe { libc::syscall(libc::SYS_gettid) as i32 };
        let request = state.receive_rpc(
            reverie::Tid::from_raw(parent.as_raw()),
            (
                DetTime::new(&config),
                MmId::initial(parent),
                GlobalRequest::CreateChildThread(
                    child,
                    detpid,
                    0,
                    None,
                    libc::SIGCHLD,
                    Some((physical_pid, physical_tid)),
                    Some(DEFAULT_PRIORITY),
                ),
            ),
        );
        let kill_after_parent_parks = async {
            while parent_request.try_read().is_none() {
                tokio::task::yield_now().await;
            }
            let mut scheduler = state.sched.lock().unwrap();
            let (registered_mm, registered_pid, registered_tid) = scheduler
                .physical_thread_identity(child)
                .expect("parent registration must install the child pidfd before continuing");
            assert_eq!(
                registered_mm,
                MmId::for_clone(MmId::initial(parent), child, false)
            );
            assert_eq!(
                (registered_pid, registered_tid),
                (physical_pid, physical_tid)
            );
            scheduler.logically_kill_thread(&parent, &detpid, MmId::initial(detpid));
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

    /// A deterministic inode must be minted from the monotonic counter, never
    /// derived from the host inode's bits. This is the behavioural half of the
    /// guarantee whose static half is `DetInode` being a newtype: even for a
    /// large, realistic host inode the det value stays small and dense, so a
    /// leaked host inode is distinguishable from a genuine det one.
    #[test]
    fn det_inodes_are_minted_not_passed_through() {
        use crate::types::DetInode;

        let mut pool = super::InodePool::new();
        let t = LogicalTime::from_nanos(0);

        let host_a = 221_742_951; // the value observed leaking into FileContents
        let host_b = 998_877_665;
        let (a, _) = pool.add_inode(host_a, t);
        let (b, _) = pool.add_inode(host_b, t);

        assert_ne!(a.as_raw(), host_a, "det inode must not be the host inode");
        assert_ne!(b.as_raw(), host_b, "det inode must not be the host inode");
        assert_eq!(a, DetInode::mint(1), "minting starts at 1");
        assert_eq!(b, DetInode::mint(2), "minting is monotonic");

        // Re-determinizing the same host inode is stable, not a fresh mint.
        let (a_again, _) = pool.add_inode(host_a, t);
        assert_eq!(a, a_again, "mapping must be stable per host inode");
    }
}

#[cfg(test)]
mod robust_exit_clock_tests {
    use std::sync::Mutex;
    use std::task::Poll;

    use nix::sys::signal::Signal;
    use reverie::ExitStatus;
    use reverie::GlobalRPC;
    use reverie::GlobalTool;
    use reverie::Tid;
    use reverie::Tool;

    use super::GlobalRequest;
    use super::GlobalResponse;
    use super::GlobalState;
    use crate::Detcore;
    use crate::ThreadState;
    use crate::config::Config;
    use crate::ivar::Ivar;
    use crate::resources::Resources;
    use crate::scheduler::DEFAULT_PRIORITY;
    use crate::scheduler::SkipTurn;
    use crate::scheduler::ThreadNextTurn;
    use crate::tool_local::RobustListExit;
    use crate::tool_local::RobustListWake;
    use crate::types::*;

    #[derive(Debug, PartialEq)]
    struct WakeObservation {
        wakes: Vec<(DetTid, FutexID)>,
        counts: Vec<u64>,
        clocks: serde_json::Value,
        turn: u64,
    }

    #[derive(Debug)]
    struct RpcObservation {
        sender: DetTid,
        kind: &'static str,
        accepted: bool,
        clocks: serde_json::Value,
        queued: Vec<DetTid>,
        waiters: usize,
        turn: u64,
    }

    // This is only a sender-bound transport, as in Reverie's WrappedFrom.
    // Replies, clock accounting, wakes and deregistration come from GlobalState.
    struct ExitRpc<'a> {
        state: &'a GlobalState,
        sender: DetTid,
        observations: &'a Mutex<Vec<WakeObservation>>,
        rpc_observations: &'a Mutex<Vec<RpcObservation>>,
    }

    #[reverie::tool]
    impl GlobalRPC<GlobalState> for ExitRpc<'_> {
        async fn send_rpc(
            &self,
            request: <GlobalState as GlobalTool>::Request,
        ) -> <GlobalState as GlobalTool>::Response {
            let kind = match &request.2 {
                GlobalRequest::RobustListWakes(wakes) if wakes.is_empty() => "empty-wake",
                GlobalRequest::RobustListWakes(_) => "wake",
                GlobalRequest::DeregisterThread(_) => "deregister",
                _ => panic!("unexpected exit RPC: {:?}", request.2),
            };
            let wakes = match &request.2 {
                GlobalRequest::RobustListWakes(wakes) if !wakes.is_empty() => Some(wakes.clone()),
                _ => None,
            };
            let response = self
                .state
                .receive_rpc(Tid::from_raw(self.sender.as_raw()), request)
                .await;
            let clocks = serde_json::to_value(&*self.state.global_time.lock().unwrap()).unwrap();
            {
                let sched = self.state.sched.lock().unwrap();
                self.rpc_observations.lock().unwrap().push(RpcObservation {
                    sender: self.sender,
                    kind,
                    accepted: !matches!(response.1, GlobalResponse::ThreadExited),
                    clocks: clocks.clone(),
                    queued: sched.run_queue.tids().copied().collect(),
                    waiters: sched
                        .blocked
                        .futex_waiters
                        .values()
                        .map(Vec::len)
                        .sum::<usize>(),
                    turn: sched.turn,
                });
            }
            if let Some(wakes) = wakes {
                let GlobalResponse::RobustListWakes(counts) = &response.1 else {
                    panic!("a complete admitted exit batch was refused: {response:?}");
                };
                let clocks =
                    serde_json::to_value(&*self.state.global_time.lock().unwrap()).unwrap();
                let turn = self.state.sched.lock().unwrap().turn;
                self.observations.lock().unwrap().push(WakeObservation {
                    wakes,
                    counts: counts.clone(),
                    clocks,
                    turn,
                });
            }
            response
        }

        fn config(&self) -> &Config {
            &self.state.cfg
        }
    }

    struct Fixture {
        state: GlobalState,
        tool: Detcore,
        owners: [ThreadState<()>; 2],
        initial_clocks: [DetTime; 2],
        waiters: [DetTid; 2],
        peer: DetTid,
        futexes: [FutexID; 2],
        observations: Mutex<Vec<WakeObservation>>,
        rpc_observations: Mutex<Vec<RpcObservation>>,
    }

    impl Fixture {
        fn new(
            reason: RobustListExit,
            equal_clocks: bool,
            empty_owner: Option<usize>,
            cancel_killed_thread_rpcs: bool,
        ) -> Self {
            let config = Config {
                sequentialize_threads: true,
                cancel_killed_thread_rpcs,
                ..Config::default()
            };
            let state = GlobalState::initialize(&config, false);
            let parent = DetTid::from_raw(1);
            let leader = DetTid::from_raw(17);
            let worker = DetTid::from_raw(18);
            let waiters = [DetTid::from_raw(21), DetTid::from_raw(23)];
            let peer = DetTid::from_raw(25);
            let mm = MmId::initial(leader);
            let mut first = ThreadState::new(leader, &config, ());
            first.detpid = Some(leader);
            let mut second = first.clone();
            second.dettid = worker;
            let mut inherited = DetTime::new(&config);
            inherited.add_syscall_with_cost(1_000);
            let first_initial = inherited.clone_for_child();
            inherited.add_syscall_with_cost(370);
            let second_initial = inherited.clone_for_child();
            first.thread_logical_time = first_initial.clone();
            second.thread_logical_time = second_initial.clone();
            first
                .thread_logical_time
                .add_syscall_with_cost(if equal_clocks { 407 } else { 37 });
            second.thread_logical_time.add_syscall_with_cost(37);
            first.record_robust_list_head(Some(0x404100));
            second.record_robust_list_head(Some(0x404200));
            let object = SharedMemoryObjectId::Anonymous {
                origin: MmId::initial(parent),
                sequence: 1,
            };
            let futexes = [FutexID::shared(object, 0), FutexID::shared(object, 8)];
            first.stage_robust_list_wakes(
                reason,
                vec![
                    (
                        worker,
                        if empty_owner == Some(1) {
                            Vec::new()
                        } else {
                            vec![RobustListWake { futex: futexes[1] }]
                        },
                    ),
                    (
                        leader,
                        if empty_owner == Some(0) {
                            Vec::new()
                        } else {
                            vec![RobustListWake { futex: futexes[0] }]
                        },
                    ),
                ],
            );
            {
                let mut sched = state.sched.lock().unwrap();
                sched.thread_tree.add_child(parent, parent, true);
                sched.thread_tree.add_child(parent, leader, true);
                sched.thread_tree.add_child(leader, worker, false);
                for tid in [waiters[0], waiters[1], peer] {
                    sched.thread_tree.add_child(parent, tid, true);
                }
                for tid in [leader, worker, waiters[0], waiters[1], peer] {
                    sched.priorities.insert(tid, DEFAULT_PRIORITY);
                    sched.next_turns.insert(
                        tid,
                        ThreadNextTurn {
                            dettid: tid,
                            child_tid_addr: 0,
                            req: Ivar::new(),
                            resp: Ivar::new(),
                        },
                    );
                    sched.install_test_exec_incarnation(
                        tid,
                        if tid == leader || tid == worker {
                            mm
                        } else {
                            MmId::initial(tid)
                        },
                    );
                }
                // Both owners have returned from their last guest turn and
                // retain its empty next request until their exit callbacks.
                // The independent peer is ready, but cannot pass quiescence.
                sched.runqueue_push_back(leader);
                sched.runqueue_push_back(worker);
                sched.runqueue_push_back(peer);
                sched.next_turns[&peer].req.put(Ok(Resources::new(peer)));
                for (waiter, futex) in waiters.into_iter().zip(futexes) {
                    sched.sleep_futex_waiter(&waiter, futex, None, u32::MAX);
                }
            }
            {
                let mut time = state.global_time.lock().unwrap();
                for (tid, clock) in [(leader, &first_initial), (worker, &second_initial)] {
                    time.update_global_time(tid, clock.as_nanos(), clock.inherited_nanos());
                }
            }
            let tool = Detcore::new(Tid::from_raw(leader.as_raw()), &config);
            Self {
                state,
                tool,
                owners: [first, second],
                initial_clocks: [first_initial, second_initial],
                waiters,
                peer,
                futexes,
                observations: Mutex::new(Vec::new()),
                rpc_observations: Mutex::new(Vec::new()),
            }
        }

        fn assert_clocks(&self, completed: &[usize]) -> serde_json::Value {
            let time = self.state.global_time.lock().unwrap();
            let snapshot = serde_json::to_value(&*time).unwrap();
            let epoch = DetTime::new(&self.state.cfg).as_nanos();
            let mut expected = epoch;
            for (index, owner) in self.owners.iter().enumerate() {
                let clock = if completed.contains(&index) {
                    &owner.thread_logical_time
                } else {
                    &self.initial_clocks[index]
                };
                assert_eq!(time.threads_time(owner.dettid), clock.as_nanos());
                assert_eq!(
                    snapshot["inherited_time"][owner.dettid.as_raw().to_string()],
                    serde_json::to_value(clock.inherited_nanos()).unwrap()
                );
                expected = expected + (clock.as_nanos() - epoch - clock.inherited_nanos());
            }
            assert_eq!(
                time.as_nanos(),
                expected,
                "only each owner's own uninherited work contributes"
            );
            snapshot
        }

        async fn exit(&self, index: usize, status: ExitStatus) {
            self.exit_thread(self.owners[index].clone(), status).await;
        }

        async fn exit_thread(&self, thread: ThreadState<()>, status: ExitStatus) {
            let rpc = ExitRpc {
                state: &self.state,
                sender: thread.dettid,
                observations: &self.observations,
                rpc_observations: &self.rpc_observations,
            };
            let exit =
                self.tool
                    .on_exit_thread(Tid::from_raw(rpc.sender.as_raw()), &rpc, thread, status);
            let mut exit = std::pin::pin!(exit);
            // On the CLI's current-thread ptrace route, Ready on the first
            // poll excludes a new queued-observer window inside this callback.
            // Multi-thread embeddings still admit concurrent scheduler reads;
            // the RPC observations separately check the actual global actions.
            assert!(
                matches!(futures::poll!(exit.as_mut()), Poll::Ready(Ok(()))),
                "exit callback yielded before its accounting and cleanup completed"
            );
        }

        async fn nonmember_exit(&self, raw_tid: i32) {
            let mut thread = self.owners[0].clone();
            thread.dettid = DetTid::from_raw(raw_tid);
            thread.thread_logical_time = DetTime::new(&self.state.cfg);
            let tid = thread.dettid;
            {
                let mut sched = self.state.sched.lock().unwrap();
                sched
                    .thread_tree
                    .add_child(self.owners[0].dettid, tid, false);
                sched.priorities.insert(tid, DEFAULT_PRIORITY);
                sched.next_turns.insert(
                    tid,
                    ThreadNextTurn {
                        dettid: tid,
                        child_tid_addr: 0,
                        req: Ivar::new(),
                        resp: Ivar::new(),
                    },
                );
                sched.install_test_exec_incarnation(tid, thread.mm_id);
                sched.runqueue_push_back(tid);
            }
            let before = self.rpc_observations.lock().unwrap().len();
            self.exit_thread(thread, ExitStatus::Exited(0)).await;
            let observations = self.rpc_observations.lock().unwrap();
            assert_eq!(
                observations.len(),
                before + 1,
                "a nonmember sent an acknowledgement or wake RPC"
            );
            assert_eq!(observations[before].sender, tid);
            assert_eq!(observations[before].kind, "deregister");
            assert!(observations[before].accepted);
        }
    }

    #[tokio::test]
    async fn robust_exit_acknowledgements_preserve_global_action_order_and_eligibility() {
        for order in [[0, 1], [1, 0]] {
            for empty_owner in [0, 1] {
                let f = Fixture::new(RobustListExit::ExitGroup, false, Some(empty_owner), false);
                f.nonmember_exit(30).await;
                assert!(f.observations.lock().unwrap().is_empty());
                let queue_before_first: Vec<_> = f
                    .state
                    .sched
                    .lock()
                    .unwrap()
                    .run_queue
                    .tids()
                    .copied()
                    .collect();
                let first_start = f.rpc_observations.lock().unwrap().len();
                f.exit(order[0], ExitStatus::Exited(0)).await;
                let first_clocks = f.assert_clocks(&[order[0]]);
                {
                    let observations = f.rpc_observations.lock().unwrap();
                    let actions = &observations[first_start..];
                    assert_eq!(
                        actions.iter().map(|a| a.kind).collect::<Vec<_>>(),
                        ["empty-wake", "deregister"]
                    );
                    assert!(actions.iter().all(|a| a.accepted
                        && a.sender == f.owners[order[0]].dettid
                        && a.clocks == first_clocks
                        && a.waiters == 2
                        && a.turn == 0));
                    assert_eq!(
                        actions[0].queued, queue_before_first,
                        "empty acknowledgement changed scheduler eligibility"
                    );
                }
                f.nonmember_exit(31).await;
                f.assert_clocks(&[order[0]]);
                assert!(
                    f.observations.lock().unwrap().is_empty(),
                    "a nonmember completed the physical-exit barrier"
                );
                let queue_before_last: Vec<_> = f
                    .state
                    .sched
                    .lock()
                    .unwrap()
                    .run_queue
                    .tids()
                    .copied()
                    .collect();
                let last_start = f.rpc_observations.lock().unwrap().len();
                f.exit(order[1], ExitStatus::Exited(0)).await;
                let final_clocks = f.assert_clocks(&[0, 1]);
                {
                    let observations = f.rpc_observations.lock().unwrap();
                    let actions = &observations[last_start..];
                    assert_eq!(
                        actions.iter().map(|a| a.kind).collect::<Vec<_>>(),
                        ["empty-wake", "wake", "deregister"]
                    );
                    assert!(actions.iter().all(|a| a.accepted
                        && a.sender == f.owners[order[1]].dettid
                        && a.clocks == final_clocks
                        && a.turn == 0));
                    assert_eq!(actions[0].waiters, 2);
                    assert_eq!(actions[1].waiters, 1);
                    assert_eq!(actions[0].queued, queue_before_last);
                    assert_eq!(
                        actions[1].queued, queue_before_last,
                        "wake bypassed deferred admission"
                    );
                }
                f.nonmember_exit(32).await;
                f.assert_clocks(&[0, 1]);
                assert_eq!(f.observations.lock().unwrap().len(), 1);
                let observations = f.rpc_observations.lock().unwrap();
                for owner in &f.owners {
                    assert_eq!(
                        observations
                            .iter()
                            .filter(|a| a.sender == owner.dettid && a.kind == "empty-wake")
                            .count(),
                        1,
                        "each unique matching owner, including an empty-wake owner, must acknowledge before the batch clears"
                    );
                }
            }
        }
    }

    #[tokio::test]
    async fn robust_exit_callbacks_preserve_owner_clocks_across_arrival_orders() {
        for reason in [
            RobustListExit::ExitGroup,
            RobustListExit::Signal(libc::SIGTERM),
        ] {
            for equal_clocks in [false, true] {
                for empty_owner in [None, Some(0), Some(1)] {
                    let mut results = Vec::new();
                    for order in [[0, 1], [1, 0]] {
                        let f = Fixture::new(reason, equal_clocks, empty_owner, false);
                        let status = match reason {
                            RobustListExit::ExitGroup => ExitStatus::Exited(0),
                            RobustListExit::Signal(_) => {
                                ExitStatus::Signaled(Signal::SIGTERM, false)
                            }
                        };
                        f.exit(order[0], status).await;
                        f.assert_clocks(&[order[0]]);
                        assert!(f.observations.lock().unwrap().is_empty());
                        {
                            let sched = f.state.sched.lock().unwrap();
                            assert_eq!(
                                sched
                                    .blocked
                                    .futex_waiters
                                    .values()
                                    .map(Vec::len)
                                    .sum::<usize>(),
                                2
                            );
                            assert_eq!(sched.turn, 0);
                        }
                        // An actually runnable peer must still wait for the
                        // remaining owner's outstanding next request.
                        {
                            let last = Err(SkipTurn);
                            let turn = crate::scheduler::do_a_turn_blocking(
                                f.state.sched.clone(),
                                f.state.global_time.clone(),
                                &last,
                            );
                            let mut turn = std::pin::pin!(turn);
                            assert!(matches!(futures::poll!(turn.as_mut()), Poll::Pending));
                        }
                        f.exit(order[0], status).await;
                        f.assert_clocks(&[order[0]]);
                        assert!(
                            f.observations.lock().unwrap().is_empty(),
                            "duplicate physical exit released an incomplete group"
                        );
                        f.exit(order[1], status).await;
                        let clocks = f.assert_clocks(&[0, 1]);
                        {
                            let observations = f.observations.lock().unwrap();
                            assert_eq!(observations.len(), 1);
                            let expected: Vec<_> = (0..2)
                                .filter(|i| Some(*i) != empty_owner)
                                .map(|i| (f.owners[i].dettid, f.futexes[i]))
                                .collect();
                            assert_eq!(observations[0].wakes, expected);
                            assert_eq!(observations[0].counts, vec![1; expected.len()]);
                            assert_eq!(
                                observations[0].clocks, clocks,
                                "all owner clocks must be accounted before wake admission"
                            );
                            assert_eq!(observations[0].turn, 0);
                        }
                        f.exit(order[1], status).await;
                        assert_eq!(f.assert_clocks(&[0, 1]), clocks);
                        assert_eq!(
                            f.observations.lock().unwrap().len(),
                            1,
                            "repeated cleanup emitted a second batch"
                        );
                        {
                            let sched = f.state.sched.lock().unwrap();
                            assert!(!sched.run_queue.contains_tid(f.waiters[0]));
                            assert!(!sched.run_queue.contains_tid(f.waiters[1]));
                        }
                        // Drive the real step1/step2 drain and one peer turn;
                        // no test-only scheduler implementation or reply shim.
                        let result = crate::scheduler::do_a_turn_blocking(
                            f.state.sched.clone(),
                            f.state.global_time.clone(),
                            &Err(SkipTurn),
                        )
                        .await;
                        assert!(result.is_ok());
                        let sched = f.state.sched.lock().unwrap();
                        assert_eq!(sched.turn, 1);
                        let queued: Vec<_> = sched.run_queue.tids().copied().collect();
                        for (i, waiter) in f.waiters.iter().enumerate() {
                            assert_eq!(queued.contains(waiter), Some(i) != empty_owner);
                        }
                        assert!(queued.contains(&f.peer));
                        results.push((
                            clocks,
                            queued,
                            std::mem::take(&mut *f.observations.lock().unwrap()),
                        ));
                    }
                    assert_eq!(
                        results[0], results[1],
                        "callback order changed final clocks, typed wake observations or the actual scheduler drain"
                    );
                }
            }
        }
    }

    #[tokio::test]
    async fn robust_exit_callbacks_keep_rejected_or_mismatched_batches_incomplete() {
        for rejection in ["old-mm", "tombstone", "wrong-signal", "normal-exit"] {
            let f = Fixture::new(
                RobustListExit::Signal(libc::SIGTERM),
                false,
                None,
                rejection == "tombstone",
            );
            let rejected = f.owners[0].dettid;
            let mm = f.owners[0].mm_id;
            f.exit(1, ExitStatus::Signaled(Signal::SIGTERM, false))
                .await;
            let replacement_request = Ivar::new();
            if rejection == "old-mm" {
                let mut sched = f.state.sched.lock().unwrap();
                sched.install_test_exec_incarnation(rejected, mm.for_exec(rejected));
                sched.next_turns.insert(
                    rejected,
                    ThreadNextTurn {
                        dettid: rejected,
                        child_tid_addr: 0,
                        req: replacement_request.clone(),
                        resp: Ivar::new(),
                    },
                );
                let mut replacement_time = f.owners[0].thread_logical_time.clone();
                replacement_time.add_syscall_with_cost(500);
                f.state.global_time.lock().unwrap().update_global_time(
                    rejected,
                    replacement_time.as_nanos(),
                    replacement_time.inherited_nanos(),
                );
            } else if rejection == "tombstone" {
                // Use the real cancelling backend gate; keep its matching Mm.
                let mut sched = f.state.sched.lock().unwrap();
                sched.logically_kill_thread(&rejected, &rejected, mm);
            }
            let before = serde_json::to_value(&*f.state.global_time.lock().unwrap()).unwrap();
            let status = match rejection {
                "wrong-signal" => ExitStatus::Signaled(Signal::SIGKILL, false),
                "normal-exit" => ExitStatus::Exited(0),
                _ => ExitStatus::Signaled(Signal::SIGTERM, false),
            };
            f.exit(0, status).await;
            assert!(
                f.observations.lock().unwrap().is_empty(),
                "{rejection} released a group"
            );
            if rejection == "old-mm" || rejection == "tombstone" {
                assert_eq!(
                    serde_json::to_value(&*f.state.global_time.lock().unwrap()).unwrap(),
                    before,
                    "rejected acknowledgement changed clock state"
                );
            }
            let sched = f.state.sched.lock().unwrap();
            assert_eq!(
                sched
                    .blocked
                    .futex_waiters
                    .values()
                    .map(Vec::len)
                    .sum::<usize>(),
                2,
                "rejected group lost a real waiter"
            );
            assert!(
                f.waiters
                    .iter()
                    .all(|tid| !sched.run_queue.contains_tid(*tid))
            );
            if rejection == "old-mm" {
                assert!(sched.rpc_incarnation_matches(rejected, mm.for_exec(rejected)));
                assert_eq!(
                    sched.next_turns[&rejected].req, replacement_request,
                    "old cleanup destroyed replacement registration"
                );
            }
        }
    }

    #[tokio::test]
    #[should_panic(expected = "Attempted to update tid 17 time")]
    async fn robust_exit_clock_ack_still_refuses_a_backwards_owner_sample() {
        let f = Fixture::new(RobustListExit::ExitGroup, false, None, false);
        let owner = &f.owners[0];
        let mut later = owner.thread_logical_time.clone();
        later.add_syscall_with_cost(1);
        f.state.global_time.lock().unwrap().update_global_time(
            owner.dettid,
            later.as_nanos(),
            later.inherited_nanos(),
        );
        f.exit(0, ExitStatus::Exited(0)).await;
    }

    #[tokio::test]
    async fn backend_failure_preserves_consuming_robust_exit_clock_accounting() {
        for order in [[0, 1], [1, 0]] {
            let f = Fixture::new(RobustListExit::ExitGroup, false, None, false);
            let selected = {
                let mut sched = f.state.sched.lock().unwrap();
                sched.next_turns.get_mut(&f.peer).unwrap().req = Ivar::new();
                sched.select_test_turn().unwrap()
            };
            let responses = {
                let sched = f.state.sched.lock().unwrap();
                f.waiters.map(|tid| sched.next_turns[&tid].resp.clone())
            };
            let mut turn = std::pin::pin!(crate::scheduler::finish_selected_turn(
                f.state.sched.clone(),
                f.state.global_time.clone(),
                selected.0,
                selected.1,
                selected.2,
            ));
            assert!(futures::poll!(turn.as_mut()).is_pending());
            f.state.report_backend_failure(reverie::BackendFailure {
                pid: Tid::from_raw(17),
                tid: Tid::from_raw(18),
                phase: "native robust cleanup control",
            });
            for index in order {
                f.exit(index, ExitStatus::Exited(0)).await;
            }
            assert!(matches!(futures::poll!(turn.as_mut()), Poll::Ready(Err(_))));
            f.assert_clocks(&[0, 1]);
            let observations = f.observations.lock().unwrap();
            assert_eq!(observations.len(), 1, "one complete batch");
            assert_eq!(observations[0].counts, vec![1, 1]);
            assert!(
                responses
                    .iter()
                    .all(|response| response.try_read().is_none())
            );
            let mut sched = f.state.sched.lock().unwrap();
            assert_eq!(sched.turn, 0);
            for owner in &f.owners {
                assert!(!sched.next_turns.contains_key(&owner.dettid));
                assert!(!sched.note_deregistration_accounted(owner.dettid));
            }
        }
    }
}
