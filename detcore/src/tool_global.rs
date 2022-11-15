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
use std::collections::btree_map::Entry;
use std::collections::HashMap;
use std::collections::HashSet;
use std::fmt::Debug;
use std::fmt::Write;
use std::fs;
use std::fs::File;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU16;
use std::sync::atomic::Ordering::SeqCst;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::SystemTime;

use chrono::DateTime;
use chrono::Utc;
use nix::sys::signal::Signal;
use reverie::syscalls::AddrMut;
use reverie::syscalls::CloneFlags;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::Sysno;
use reverie::GlobalRPC;
use reverie::GlobalTool;
use reverie::Guest;
use reverie::Tid;
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
use crate::resources::Permission;
use crate::resources::ResourceID;
use crate::resources::Resources;
use crate::scheduler::entropy_to_priority;
use crate::scheduler::runqueue::is_ordinary_priority;
use crate::scheduler::runqueue::REPLAY_DEFERRED_PRIORITY;
use crate::scheduler::runqueue::REPLAY_FOREGROUND_PRIORITY;
use crate::scheduler::sched_loop;
use crate::scheduler::ConsumeResult;
use crate::scheduler::MaybePrintStack;
use crate::scheduler::Priority;
use crate::scheduler::SchedResponse;
use crate::scheduler::SchedValue;
use crate::scheduler::Scheduler;
use crate::scheduler::Seconds;
use crate::scheduler::ThreadNextTurn;
use crate::scheduler::DEFAULT_PRIORITY;
use crate::tool_local::Detcore;
use crate::types::*;
use crate::util::truncated;

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

/// Global state associated with the detcore tool.
///
/// This is a singleton, and the one object of this type lives inside a central
/// address space, generally the "tracer" in a Reverie backend.
#[derive(Debug)]
pub struct GlobalState {
    sched: Arc<Mutex<Scheduler>>,

    inodes: Arc<Mutex<InodePool>>,

    // next port to use if input port is 0
    next_port: AtomicU16,

    // used ports
    used_ports: Mutex<HashSet<u16>>,

    // fd to port
    fd_to_port: Mutex<HashMap<i32, u16>>,

    port_start_range: AtomicU16,
    port_end_range: AtomicU16,

    // False initially after fork, and true when we begin executing the guest binary.
    past_first_execve: AtomicBool,

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
        info!("detcore shut down, destroying global state");
    }
}

impl GlobalState {
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

    /// Shut down anything running, in particular wait on the scheduler.
    ///
    /// This is basically the destructor for the global state, but is here rather than in the
    /// Drop instance so that it can be async, and is more explicitly sequenced in the program.
    ///
    /// Print a summary of the execution, typically called when it is complete.
    ///
    /// If the boolean argument is true, print to stderr, otherwise only print the summary
    /// to the log.
    pub async fn clean_up(mut self, to_stderr: bool) {
        if let Some(handle) = self.sched_handle.take() {
            debug!("Global state cleanup, confirming scheduler has shut down...");
            handle.await.expect("Global scheduler clean shutdown");
            debug!("Global state cleanup, continuing...");
        }
        let mut buf: String = String::new();
        writeln!(
            buf,
            "  ------------------------------ hermit run report ------------------------------"
        )
        .unwrap();

        let flush = |det: bool, buf: &mut String| {
            if to_stderr {
                // In this case, print summary irrespective of logging level.
                // TODO: output summary in machine-readable, JSON form.
                eprint!("{}", buf);
            } else if det {
                info!("{}", buf);
            } else {
                debug!("{}", buf);
            }
            buf.clear();
        };

        // Thread report:
        let mut sched = self.sched.lock().unwrap();
        buf.push_str(&sched.thread_tree.final_report());
        flush(true, &mut buf);

        writeln!(
            buf,
            "Internally, the hermit scheduler ran {} turns, recorded {} events, replayed {} events.",
            sched.turn, sched.recorded_event_count, sched.traced_event_count
        )
        .unwrap();

        if let Some(pw) = sched.preemption_writer.take() {
            writeln!(
                buf,
                "Record of {} preemption and reprioritization events:",
                pw.len()
            )
            .unwrap();
            if let Some(path) = &self.cfg.record_preemptions_to {
                writeln!(buf, "  (Writing to file {:?})", path).unwrap();
                if let Err(str) = pw.flush() {
                    warn!("{}", str);
                }
            } else {
                writeln!(buf, "{}", truncated(200, pw.into_string())).unwrap();
            }
        }

        // Real time report:
        // N.B.: We don't have a job-level exit hook atm (T76248597), so we use the
        // CURRENT time -- that we are calling summarize -- as the end time:
        if let Ok(realtime_elapsed) = self.realtime_start.elapsed() {
            writeln!(
                buf,
                "Nondeterministic realtime elapsed: {:?}",
                realtime_elapsed
            )
            .unwrap();
        }
        flush(false, &mut buf);

        // Virtual time report:
        if self.cfg.virtualize_time {
            let final_time = self.global_time.lock().unwrap();
            let final_time_ns = final_time.as_nanos();
            writeln!(buf, "Final virtual global (cpu) time: {}", final_time_ns).unwrap();
            let epoch_ns = LogicalTime::from_nanos(self.cfg.epoch.timestamp_nanos() as u64);
            if final_time_ns.as_nanos() >= epoch_ns.as_nanos() {
                writeln!(
                    buf,
                    "Elapsed virtual global (cpu) time: {}",
                    final_time_ns - epoch_ns
                )
                .unwrap();
            } else {
                error!(
                    "Internal invariant violated! Global time is before epoch start {}",
                    epoch_ns
                );
            }
        }
        flush(true, &mut buf);
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
    type Request = (DetTime, GlobalRequest);

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
        let sched = Arc::new(Mutex::new(Scheduler::new(cfg)));
        let global_time = Arc::new(Mutex::new(GlobalTime::new(cfg)));
        let handle = if cfg.sequentialize_threads {
            Some(tokio::spawn(sched_loop(sched.clone(), global_time.clone())))
        } else {
            None
        };

        let preemptions_to_replay: Option<PreemptionReader> = cfg
            .replay_preemptions_from
            .as_ref()
            .map(|path| PreemptionReader::new(path));

        let range = GlobalState::read_port_range();

        GlobalState {
            sched,
            next_port: AtomicU16::new(range[0]),
            used_ports: Mutex::new(HashSet::new()),
            port_start_range: AtomicU16::new(range[0]),
            port_end_range: AtomicU16::new(range[1]),
            fd_to_port: Mutex::new(HashMap::new()),
            past_first_execve: AtomicBool::new(false),
            inodes: Arc::new(Mutex::new(InodePool::new())),
            sched_handle: handle,
            cfg: cfg.clone(),
            realtime_start: SystemTime::now(),
            global_time,
            preemptions_to_replay,
        }
    }

    async fn receive_rpc(&self, from: Tid, gr: Self::Request) -> Self::Response {
        type R = GlobalResponse;
        let dtid = DetTid::from_raw(from.into()); // TODO(T78538674): FIXME
        let time_from_guest = gr.0.as_nanos();

        // This portion of the time updates "asynchronously", and we can tick it on every rpc:
        // TODO: eventually the vector clock should be in shared memory, and
        // the local clocks should update truly asynchrously.  Therefore it
        // SHOULD be safe to always push through this update on any rpc,
        // removing the conditional above.
        self.global_time
            .lock()
            .unwrap()
            .update_global_time(dtid, time_from_guest);

        // RPC boilerplate. (Hard to generate systematically now though, because of the
        // time payload piggy-backing on each rpc. Maybe eventually once ticking a
        // threads' own clock happens through shared memory.)
        #[allow(clippy::unit_arg)]
        let resp = match gr.1 {
            GlobalRequest::RequestResources(rs, pid) => {
                R::RequestResources(self.recv_request_resources(from, pid, rs).await)
            }
            GlobalRequest::ReleaseResources(rs) => {
                R::ReleaseResources(self.recv_release_resources(from, rs).await)
            }
            GlobalRequest::ReleaseAllResources => {
                R::ReleaseAllResources(self.recv_release_all_resources(from).await)
            }
            // Requested by the parent thread:
            GlobalRequest::CreateChildThread(dettid, parent_detpid, ctid, flags, priority) => {
                R::CreateChildThread(
                    self.recv_create_child_thread(
                        from,
                        parent_detpid,
                        dettid,
                        ctid,
                        flags,
                        priority,
                    )
                    .await,
                )
            }
            // Requested by the child thread itself:
            GlobalRequest::StartNewThread(dettid, detpid) => {
                R::StartNewThread(self.recv_start_new_thread(from, dettid, detpid).await)
            }
            GlobalRequest::DeregisterThread(dettid, detpid) => {
                R::DeregisterThread(self.recv_deregister_thread(from, dettid, detpid).await)
            }
            GlobalRequest::FutexAction(dettid, action, futexid, init_read, mask) => R::FutexAction(
                self.recv_futex_action(from, dettid, action, futexid, init_read, mask)
                    .await,
            ),
            GlobalRequest::DeterminizeInode(ino) => {
                R::DeterminizeInode(self.recv_determinize_inode(from, ino).await)
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
                let print_backtrace = self.recv_trace_schedevent(ev, detpid).await;
                R::TraceSchedEvent(print_backtrace)
            }
            GlobalRequest::RegisterAlarm(dpid, dtid, secs, sig) => {
                let remaining = self.recv_register_alarm(dpid, dtid, secs, sig).await;
                R::RegisterAlarm(remaining)
            }
            GlobalRequest::UnrecoverableShutdown => {
                self.force_shutdown_with_error();
                R::UnrecoverableShutdown(())
            }
            GlobalRequest::RequestPort(sock_fd) => {
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
                    let mut mut_fd_to_port = self.fd_to_port.lock().unwrap();
                    (*mut_fd_to_port).insert(sock_fd, self.next_port.load(SeqCst));
                    R::RequestPort(self.next_port.load(SeqCst))
                }
            }
            GlobalRequest::AddUsedPort(port, sock_fd) => {
                let mut mut_used_ports = self.used_ports.lock().unwrap();
                (*mut_used_ports).insert(port);
                let mut mut_fd_to_port = self.fd_to_port.lock().unwrap();
                (*mut_fd_to_port).insert(sock_fd, self.next_port.load(SeqCst));
                R::AddUsedPort
            }
            GlobalRequest::FreePort(port) => {
                let mut mut_used_ports = self.used_ports.lock().unwrap();
                (*mut_used_ports).remove(&port);
                R::FreePort
            }
            GlobalRequest::FreePortByFd(sock_fd) => {
                let mut mut_fd_to_port = self.fd_to_port.lock().unwrap();
                let port = (*mut_fd_to_port).remove(&sock_fd);
                if let Some(x) = port {
                    let mut mut_used_ports = self.used_ports.lock().unwrap();
                    (*mut_used_ports).remove(&x);
                }
                R::FreePort
            }
        };

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
    ) -> ResumeStatus {
        let dettid = DetTid::from_raw(from.into()); // TODO(T78538674): FIXME

        let resp2 = {
            let mut sched = self.sched.lock().unwrap();
            let nextturn = sched.next_turns.get(&dettid).unwrap_or_else(|| {
                panic!("Detcore internal error: no entry for dettid {} in next_turns during resource request.", dettid)
            }).clone();
            trace!(
                "[detcore, dtid {}] ResourceRequest, filling request into {}",
                &dettid,
                &nextturn.req
            );
            sched.request_put(&nextturn.req, rs.clone(), &self.global_time);
            nextturn.resp
        };
        trace!(
            "[detcore, dtid {}] waiting on {} for resources: {:?}",
            dettid,
            &resp2,
            rs
        );
        let answer = resp2.get().await; // Block on the scheduler allowing our guest to proceed.
        if rs.resources.contains_key(&ResourceID::Exit(true)) {
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
                for tid in sched.thread_tree.my_thread_group(&dettid) {
                    // We don't need to do anything extra for our own thread. That can use the
                    // same mechanics as a normal exit:
                    if tid != dettid {
                        sched.logically_kill_thread(&tid, &dettid);
                    }
                }
            }
        }

        match answer {
            SchedResponse::Go(_) => {
                trace!(
                    "[detcore, dtid {}] resources granted, resuming normally: {:?}",
                    dettid,
                    rs
                );
                ResumeStatus::Normal
            }
            SchedResponse::Signaled() => {
                trace!(
                    "[detcore, dtid {}] resources granted but interrupted by signal",
                    dettid,
                );
                ResumeStatus::Signaled
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

    /// Global portion of parent-forking-child protocol.  Called by the parent thread.
    async fn recv_create_child_thread(
        &self,
        from_parent: Tid,
        parent_detpid: DetPid,
        child_dettid: DetTid,
        ctid: usize,
        flags: Option<CloneFlags>,
        maybe_priority: Option<Priority>,
    ) {
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
                let parent_dettid = DetTid::from_raw(from_parent.into()); // TODO(T78538674)
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
                if sched.priorities.get(&child_dettid).is_none() {
                    assert_eq!(parent_detpid, ROOT_DETPID);
                    let _ = sched.priorities.insert(child_dettid, initial_priority);
                }
            }

            if let Some(pr) = &mut sched.preemption_writer {
                pr.register_thread(child_dettid, initial_priority);
            }

            let pos = sched.runqueue_push_back(child_dettid);
            debug!(
                "[detcore] CreateChildThread with dtid {}: Added child to back of queue, position {}.",
                child_dettid, pos,
            );
            sched.started_up.try_put(());
        }
        // Parent thread yields so child can run (if it is higher priority).
        if self.cfg.sequentialize_threads {
            let mut rs = Resources::new(parent_detpid);
            rs.insert(ResourceID::ParentContinue(), Permission::W);
            self.recv_request_resources(from_parent, parent_detpid, rs)
                .await;
        }
    }

    /// Called by the child thread upon startup.
    /// Returns a thread-preemption history for the new guest thread (if --replay-preemptions-from
    /// is used).
    async fn recv_start_new_thread(
        &self,
        from: Tid,
        dettid: DetTid,
        detpid: DetPid,
    ) -> Option<ThreadHistory> {
        let mut tries: u64 = 0;
        // TODO: eliminate this loop. Could instead signal with an ivar.
        let response_ivar = loop {
            tokio::task::yield_now().await;
            let detpid = detpid;
            let mut sched = self.sched.lock().unwrap();
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
                        from,
                        tries
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
            let old_prio = self
                .sched
                .lock()
                .unwrap()
                .priorities
                .insert(dettid, history.initial_priority());
            debug!(
                "[replay-preemption] Enqueing new thread at priority {:?} (changed from {:?})",
                history.initial_priority(),
                old_prio,
            );
            Some(history)
        } else {
            None
        }
    }

    /// Warning: this happens completely asynchronously, whenever the guest exit hook fires.
    /// Its timing is not coordinated by the scheduler.
    async fn recv_deregister_thread(&self, _from: Tid, dettid: DetTid, detpid: DetPid) {
        // Invariant: will only be called when sequentialize-threads is on.
        assert!(self.cfg.sequentialize_threads);
        self.sched
            .lock()
            .unwrap()
            .logically_kill_thread(&dettid, &detpid);
        trace!(
            "[detcore, dtid {}] thread deregistered, removed from sched structures.",
            dettid
        );
    }

    async fn recv_futex_action(
        &self,
        _from: Tid,
        dettid: DetTid,
        action: FutexAction,
        futexid: FutexID,
        _init_read: i32,
        _mask: i32,
    ) -> Option<SchedValue> {
        trace!("[detcore, dtid {}] Futex action: {:?}", &dettid, action);
        let response_iv = {
            let mut sched = self.sched.lock().unwrap();
            let resp_iv = sched
                .next_turns
                .get(&dettid)
                .expect("Missing next_turns entry")
                .resp
                .clone();
            match action {
                FutexAction::WaitRequest(maybe_timeout) => {
                    sched.sleep_futex_waiter(&dettid, futexid, maybe_timeout);
                    // block on ivar, below
                }
                FutexAction::WaitFinished => {
                    return None;
                }
                FutexAction::WakeRequest(num_threads) => {
                    let num = sched.wake_futex_waiters(dettid, futexid, num_threads);
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
                    &dettid,
                    &response_iv
                );
                answer
            }
            SchedResponse::Signaled() => Some(SchedValue::Value(nix::errno::Errno::EINTR as u64)),
        }
    }

    async fn recv_determinize_inode(&self, from: Tid, ino: RawInode) -> (DetInode, LogicalTime) {
        // Here we establish a policy that when we first see a file its mtime is epoch.
        let (dino, ns) = self.inodes.lock().unwrap().add_inode(
            ino,
            LogicalTime::from_nanos(self.cfg.epoch.timestamp_nanos() as u64),
        );
        trace!(
            "[detcore, dtid {}] resolved (raw) inode {:?} to {:?}, mtime {}",
            from,
            ino,
            dino,
            ns
        );
        (dino, ns)
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
            LogicalTime::from_nanos(dt.timestamp_nanos() as u64)
        };
        trace!(
            "[dtid {}] bumping mtime on file (rawinode {:?}) to {}",
            from,
            ino,
            mtime,
        );
        let mut mg = self.inodes.lock().unwrap();
        let dino = if let Some(d) = mg.inodes.get(&ino) {
            *d
        } else {
            // Otherwise we haven't seen this inode yet (e.g. because there hasnt been a
            // stat on it), so we just-in-time add it.
            let (d, _) = mg.add_inode(
                ino,
                LogicalTime::from_nanos(self.cfg.epoch.timestamp_nanos() as u64),
            );
            d
        };
        let mut info = mg
            .detinodes_info
            .get_mut(&dino)
            // TODO(T87258449): remove this `expect`:
            .expect("Invariant violation: det inode missing from map.");
        info.mtime = mtime;
    }

    /// The return value indicates whether the backtrace of this event should be printed, and if so,
    /// whether it should be printed to a file.
    async fn recv_trace_schedevent(
        &self,
        ev: SchedEvent,
        detpid: DetPid,
    ) -> Option<Option<PathBuf>> {
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
        if self.cfg.replay_schedule_from.is_some() {
            let ConsumeResult {
                keep_running,
                print_stack,
                event_ix: _,
            } = self.sched.lock().unwrap().consume_schedevent(&ev);
            let print_stack2 = self.record_event(&ev);
            if !keep_running {
                trace!(
                    "[detcore, dtid {}] Thread yielding to follow replay schedule",
                    &ev.dettid,
                );
                let tid = reverie::Tid::from(ev.dettid.as_raw()); // TODO(T78538674): virtualize pid/tid:
                let mut rsrcs = Resources::new(ev.dettid);
                rsrcs.insert(ResourceID::TraceReplay, Permission::RW);
                self.recv_request_resources(tid, detpid, rsrcs).await;
                trace!(
                    "[detcore, dtid {}] Thread reactivated after yielding for replay schedule",
                    &ev.dettid,
                );
            }
            print_stack.or(print_stack2)
        } else {
            self.record_event(&ev)
        }
    }

    // Return whether we should print the stacktrace after recording this event.
    fn record_event(&self, ev: &SchedEvent) -> MaybePrintStack {
        if self.cfg.record_preemptions {
            self.sched.lock().unwrap().record_event(ev)
        } else {
            None
        }
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

    /// Register an alarm (delayed signal delivery) with the global scheduler.
    pub async fn recv_register_alarm(
        &self,
        detpid: DetPid,
        dettid: DetTid,
        seconds: Seconds,
        sig: SigWrapper,
    ) -> Seconds {
        self.sched
            .lock()
            .unwrap()
            .register_alarm(detpid, dettid, seconds, sig.0)
    }
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

    /// The parent is adding a child-thread to the round-robin pool.  Contains the dettid
    /// of the new child and it's starting scheduler priority IF it is available to the caller.
    /// The only scenario where the Priority will be missing is when we're replaying preemptions.
    /// In that case it is the global state that holds the information regarding the new thread's
    /// initial priority.
    CreateChildThread(DetTid, DetPid, usize, Option<CloneFlags>, Option<Priority>),

    /// New thread is alive and waiting to run its first instruction.  Contains the dettid
    /// and detpid of the new child.
    StartNewThread(DetTid, DetPid),

    /// Remove thread from scheduler data structure, guaranteeing it will consume no
    /// further turns.
    DeregisterThread(DetTid, DetPid),

    /// Notify scheduler before/after futex action.
    /// The last two arguments are the initial contents of the memory word, and the mask.
    FutexAction(DetTid, FutexAction, FutexID, i32, i32),

    /// Translate nondeterministic to deterministic inode.
    DeterminizeInode(RawInode),

    /// unlink an inode
    UnlinkInode(DetInode),

    /// Bump mtime
    TouchFile(RawInode),

    /// Retrieve global time.
    GlobalTimeLowerBound,

    /// Record scheduling event in a total order.
    TraceSchedEvent(SchedEvent, DetPid),

    /// Basically performs an alarm syscall, takes seconds.
    RegisterAlarm(DetPid, DetTid, Seconds, SigWrapper),

    /// The container is shutting down.  Exit the scheduler "thread".
    UnrecoverableShutdown,

    // Request a port
    RequestPort(i32),

    // Add port to used ports list
    AddUsedPort(u16, i32),

    // FreePort
    FreePort(u16),

    FreePortByFd(i32),
}

/// Responses from the global object
#[allow(missing_docs, clippy::unit_arg)]
#[derive(PartialEq, Debug, Eq, Clone, Serialize, Deserialize)]
pub enum GlobalResponse {
    RequestResources(ResumeStatus),
    ReleaseResources(()),
    ReleaseAllResources(()),
    CreateChildThread(()),
    /// Includes optional preemption points for the new thread.
    StartNewThread(Option<ThreadHistory>),
    DeregisterThread(()),
    FutexAction(Option<SchedValue>),
    /// Return the mtime as well:
    DeterminizeInode((DetInode, LogicalTime)),
    UnlinkInode(()),
    TouchFile(()),
    GlobalTimeLowerBound(LogicalTime),
    TraceSchedEvent(MaybePrintStack),
    RegisterAlarm(Seconds),
    // TODO: use void_send_rpc, and remove this bogus response:
    UnrecoverableShutdown(()),

    RequestPort(u16),
    AddUsedPort,
    FreePort,
    PortFull,
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
    let resp = guest.send_rpc((mytime, request)).await;
    // For now, time updates to the guest are disabled.  The guest ONLY tracks
    // its own work, not global time.
    assert_eq!(resp.0, None);
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
            &dettid,
            r
        );
        let resp =
            send_and_update_time(guest, GlobalRequest::RequestResources(r.clone(), detpid)).await;
        match resp.1 {
            GlobalResponse::RequestResources(x) => {
                trace!(
                    "[detcore, dtid {}] UNBLOCKED, acquired resources: {:?}",
                    &dettid,
                    r
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
) where
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
        Some(entropy_to_priority(
            guest
                .thread_state_mut()
                .chaos_prng_next_u64("child_priority"),
        ))
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

/// Remove the thread from the scheduler.
///
/// Nonblocking: the future may return immediately, not guaranteeing the changes to the
/// scheduler have been completed.
pub async fn deregister_thread<R>(
    dettid: DetTid,
    threads_time: DetTime,
    cfg: &Config,
    reverie: &R,
    detpid: DetPid,
) where
    // Note, this is called from a context where we DON'T have a full, operable `Guest`.
    R: GlobalRPC<GlobalState>,
{
    if cfg.sequentialize_threads {
        // TODO: void_send_rpc
        let resp = reverie
            .send_rpc((
                threads_time,
                GlobalRequest::DeregisterThread(dettid, detpid),
            ))
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
    mask: i32,
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

/// Helper function just for printing backtrace to a given file (or stderr)
fn print_backtrace<G, T>(guest: &mut G, maybe_path: &Option<PathBuf>)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let mut file_writer: Box<dyn std::io::Write> = match &maybe_path {
        Some(path) => {
            Box::new(File::create(path).expect("Failed to open preemption stacktrace log file"))
        }
        None => Box::new(std::io::stderr()),
    };
    let ts = guest.thread_state();
    writeln!(
        file_writer,
        ":: Guest tid {}, at thread time {}, has the below backtrace.",
        ts.dettid,
        ts.thread_logical_time.as_nanos(),
    )
    .unwrap();
    if let Some(backtrace) = guest.backtrace() {
        if let Ok(pbt) = backtrace.pretty() {
            writeln!(file_writer, "{}", pbt).unwrap();
        } else {
            writeln!(file_writer, "{}", backtrace).unwrap();
        }
    } else {
        warn!("Could not read backtrace!");
    }
}

/// Record an event in the schedule trace, OR check the event on replay.
/// This also prints the backtrace of the schedevent, if indicated.
///
/// Arguments:
/// - tag_end_rip: read the current guest registers to fill in the `end_rip` on the event with the
///   current instruction pointer.
///
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
            rip,
            rip_contents
        );
    }

    let detpid = guest.thread_state().detpid.expect("detpid unset");
    let resp = send_and_update_time(guest, GlobalRequest::TraceSchedEvent(ev, detpid)).await;
    let do_backtrace = match resp.1 {
        GlobalResponse::TraceSchedEvent(x) => x,
        _ => unreachable!(),
    };
    if let Some(x) = do_backtrace {
        print_backtrace(guest, &x);
    }
}

/// Register an alarm (delayed signal delivery) with the global scheduler.
/// Returns the number of seconds remaining until any previously scheduled alarm.
pub async fn register_alarm<G, T>(guest: &mut G, seconds: Seconds, sig: Signal) -> Seconds
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let dettid = guest.thread_state().dettid;
    let detpid = guest.thread_state().detpid.expect("detpid unset");
    let resp = send_and_update_time(
        guest,
        GlobalRequest::RegisterAlarm(detpid, dettid, seconds, SigWrapper(sig)),
    )
    .await;
    match resp.1 {
        GlobalResponse::RegisterAlarm(x) => x,
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
        // TODO: void_send_rpc
        let _ = guest
            .send_rpc((mytime, GlobalRequest::UnrecoverableShutdown))
            .await;
    }

    // In this scenario a backtrace doesn't really help us.
    std::process::exit(1);
}
