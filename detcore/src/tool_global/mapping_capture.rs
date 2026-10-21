/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! One contained observation under the caller's existing serial grant.
//! The controller is in another MM. Only the backend's synchronous operation
//! excludes private executors; these controller checks do not stop a runtime.

use std::fs::File;
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::unix::fs::MetadataExt;

use reverie::ControllerCaptureFact;
use reverie::Error;
use reverie::MappingCaptureIdentity;
use reverie::MappingCaptureToken;
use serde::Deserialize;
use serde::Serialize;

use super::*;
use crate::procfs::MapsTarget;

const MAX_VIEW_BYTES: usize = 1024 * 1024;

/// Opaque control request for one authenticated mapping observation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request(Operation);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum Operation {
    Prepare(MapsTarget, ControllerCaptureFact),
    Read(MappingCaptureToken),
    Complete(MappingCaptureToken, Option<String>),
}

/// Controller reply for the bounded shared mapping capture protocol.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Reply {
    /// The exact target and launcher containment have been authenticated.
    Prepared(MappingCaptureToken),
    /// Complete raw views of the same target during backend exclusion.
    Captured {
        /// Echo of the prepared identity.
        token: MappingCaptureToken,
        /// Complete Maps extents.
        maps: Vec<u8>,
        /// Complete NUMA fields, unchanged.
        numa: Vec<u8>,
    },
    /// Normal restoration was acknowledged.
    Completed,
    /// The observation could not be established.
    Refused(String),
}

#[derive(Debug)]
pub(super) struct Pending {
    token: MappingCaptureToken,
    target: MapsTarget,
    fact: ControllerCaptureFact,
    read: bool,
}

impl GlobalState {
    fn mapping_target_is_current(
        &self,
        sched: &Scheduler,
        from: Tid,
        mm: MmId,
        target: &MapsTarget,
        fact: ControllerCaptureFact,
    ) -> Option<MappingCaptureIdentity> {
        let caller = DetTid::from_raw(from.as_raw());
        if !self.cfg.sequentialize_threads
            || self.cfg.recordreplay_modes
            || target.mm != mm
            || target.task.process != caller.as_raw()
            || target
                .task
                .thread
                .is_some_and(|thread| thread != caller.as_raw())
        {
            return None;
        }
        let (process, thread) = sched.controller_capture_task(caller, mm, fact)?;
        let existence = sched.procfs_task(caller, target.task.thread.map(DetTid::from_raw));
        let answer = self.inodes.lock().unwrap().procfs_task_answer(
            caller,
            target.task.thread.map(DetTid::from_raw),
            existence,
        );
        if !existence.0
            || existence.1
            || answer.incarnation() != target.incarnation
            || sched.runtime_private_memory(mm)
                != Some(reverie::RuntimePrivateMemory::Ready(target.private.clone()))
        {
            return None;
        }
        Some(MappingCaptureIdentity {
            process,
            thread,
            address_space: [mm.creator().as_raw() as u64, mm.generation()],
            private_owner: target.private.owner,
            private_version: target.private.version,
        })
    }

    pub(super) fn mapping_capture_request(&self, from: Tid, mm: MmId, request: &Request) -> Reply {
        let Some(run) = &self.mapping_capture_run else {
            return Reply::Refused("enabled launcher containment is unavailable".into());
        };
        let refuse_preparation = |reason: &str| {
            run.reject_preparation(reason);
            Reply::Refused(reason.into())
        };
        if !run.containment_enabled() {
            return refuse_preparation("enabled launcher containment is unavailable");
        }
        // All operations use the same lock order, with no async yield. A Read
        // retains the scheduler grant/pidfd through closing both proc files.
        let sched = self.sched.lock().unwrap();
        let mut pending = self.mapping_capture_pending.lock().unwrap();
        match &request.0 {
            Operation::Prepare(target, fact) => {
                if pending.is_some() {
                    return refuse_preparation("another mapping capture is pending");
                }
                let Some(identity) =
                    self.mapping_target_is_current(&sched, from, mm, target, *fact)
                else {
                    return refuse_preparation(
                        "opened mapping target is not the current serial caller",
                    );
                };
                let Some(token) = run.prepare(identity) else {
                    return refuse_preparation(
                        "mapping capture requires the contained launch root",
                    );
                };
                *pending = Some(Pending {
                    token,
                    target: target.clone(),
                    fact: *fact,
                    read: false,
                });
                Reply::Prepared(token)
            }
            Operation::Read(token) => {
                let Some(view) = pending
                    .as_mut()
                    .filter(|view| view.token == *token && !view.read)
                else {
                    return Reply::Refused(
                        "mapping capture request is absent, stale or repeated".into(),
                    );
                };
                view.read = true;
                if !run.begin_observation(*token)
                    || self.mapping_target_is_current(&sched, from, mm, &view.target, view.fact)
                        != Some(token.identity)
                {
                    return Reply::Refused("mapping capture authority changed before read".into());
                }
                match read_views(token.identity.thread, view.fact) {
                    Ok((maps, numa))
                        if self.mapping_target_is_current(
                            &sched,
                            from,
                            mm,
                            &view.target,
                            view.fact,
                        ) == Some(token.identity) =>
                    {
                        Reply::Captured {
                            token: *token,
                            maps,
                            numa,
                        }
                    }
                    Ok(_) => Reply::Refused("mapping capture authority changed after read".into()),
                    Err(reason) => Reply::Refused(reason.into()),
                }
            }
            Operation::Complete(token, rejection) => {
                let valid = pending.as_ref().is_some_and(|view| {
                    view.token == *token
                        && view.read
                        && self.mapping_target_is_current(&sched, from, mm, &view.target, view.fact)
                            == Some(token.identity)
                });
                let completed = if !valid {
                    run.complete(*token, false)
                } else if let Some(reason) = rejection {
                    run.reject_restored(*token, reason)
                } else {
                    run.complete(*token, true)
                };
                if completed {
                    *pending = None;
                    Reply::Completed
                } else {
                    Reply::Refused("mapping capture restoration was not completed".into())
                }
            }
        }
    }
}

const READER_CONTEXT_UNAVAILABLE: &str =
    "mapping reader root or mount namespace proof is unavailable";
const READER_CONTEXT_DIFFERS: &str = "mapping reader root or mount namespace differs";

fn open_at(directory: i32, path: &std::ffi::CStr, flags: i32) -> std::io::Result<File> {
    // SAFETY: the directory and terminated path remain live. A successful
    // descriptor transfers immediately to File, including on later failures.
    let raw = unsafe { libc::openat(directory, path.as_ptr(), flags | libc::O_CLOEXEC) };
    if raw < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(unsafe { File::from_raw_fd(raw) })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ReaderContextIdentity {
    root_device: u64,
    root_inode: u64,
    root_mount: u64,
    namespace_device: u64,
    namespace_inode: u64,
}

/// Descriptor pins prevent object/mount identity recycling during the reads.
/// Reopening the current context afterwards checks changes; merely re-statting
/// these retained descriptors would only check the old objects.
struct ReaderContext {
    _root: File,
    _namespace: File,
    identity: ReaderContextIdentity,
}

impl ReaderContext {
    fn open(task: &File, reader: bool) -> Result<Self, &'static str> {
        let root = if reader {
            open_at(libc::AT_FDCWD, c"/", libc::O_PATH | libc::O_DIRECTORY)
        } else {
            open_at(task.as_raw_fd(), c"root", libc::O_PATH | libc::O_DIRECTORY)
        }
        .map_err(|_| READER_CONTEXT_UNAVAILABLE)?;
        let namespace = open_at(task.as_raw_fd(), c"ns/mnt", libc::O_RDONLY)
            .map_err(|_| READER_CONTEXT_UNAVAILABLE)?;
        let namespace_metadata = namespace
            .metadata()
            .map_err(|_| READER_CONTEXT_UNAVAILABLE)?;
        let mut filesystem = std::mem::MaybeUninit::<libc::statfs>::uninit();
        // SAFETY: namespace and the complete output record remain live.
        if unsafe { libc::fstatfs(namespace.as_raw_fd(), filesystem.as_mut_ptr()) } != 0
            || unsafe { filesystem.assume_init() }.f_type as u64 != 0x6e736673
        // NSFS_MAGIC
        {
            return Err(READER_CONTEXT_UNAVAILABLE);
        }
        use reverie::syscalls::linux_abi;
        let mut raw = std::mem::MaybeUninit::<linux_abi::Statx>::zeroed();
        let required = linux_abi::STATX_INO | linux_abi::STATX_MNT_ID;
        // SAFETY: empty-path statx inspects the pinned root and writes exactly
        // the declared complete output record. No missing mount-ID fallback.
        if unsafe {
            linux_abi::statx(
                root.as_raw_fd(),
                c"".as_ptr(),
                libc::AT_EMPTY_PATH | libc::AT_NO_AUTOMOUNT,
                required,
                raw.as_mut_ptr(),
            )
        } != 0
        {
            return Err(READER_CONTEXT_UNAVAILABLE);
        }
        let found = unsafe { raw.assume_init() };
        if found.stx_mask & required != required {
            return Err(READER_CONTEXT_UNAVAILABLE);
        }
        Ok(Self {
            _root: root,
            _namespace: namespace,
            identity: ReaderContextIdentity {
                root_device: libc::makedev(found.stx_dev_major, found.stx_dev_minor),
                root_inode: found.stx_ino,
                root_mount: found.stx_mnt_id,
                namespace_device: namespace_metadata.dev(),
                namespace_inode: namespace_metadata.ino(),
            },
        })
    }
}

fn read_views(tid: i32, fact: ControllerCaptureFact) -> Result<(Vec<u8>, Vec<u8>), &'static str> {
    let root = File::open("/proc").map_err(|_| "controller procfs open failed")?;
    let metadata = root
        .metadata()
        .map_err(|_| "controller procfs identity failed")?;
    let mut filesystem = std::mem::MaybeUninit::<libc::statfs>::uninit();
    if !metadata.is_dir() || metadata.dev() != fact.procfs_device || metadata.ino() != fact.procfs_inode
        // SAFETY: root and the complete output record remain live.
        || unsafe { libc::fstatfs(root.as_raw_fd(), filesystem.as_mut_ptr()) } != 0
        || unsafe { filesystem.assume_init() }.f_type as u64
            != reverie::syscalls::linux_abi::PROC_SUPER_MAGIC as u64
    {
        return Err("controller procfs identity changed");
    }
    let target_path = std::ffi::CString::new(tid.to_string()).unwrap();
    let target_task = open_at(
        root.as_raw_fd(),
        &target_path,
        libc::O_PATH | libc::O_DIRECTORY,
    )
    .map_err(|_| "controller mapping target directory open failed")?;
    // thread-self, not self: an OS thread can have its own mount namespace.
    let reader_task = open_at(
        root.as_raw_fd(),
        c"thread-self",
        libc::O_PATH | libc::O_DIRECTORY,
    )
    .map_err(|_| READER_CONTEXT_UNAVAILABLE)?;
    let target_before = ReaderContext::open(&target_task, false)?;
    let reader_before = ReaderContext::open(&reader_task, true)?;
    if target_before.identity != reader_before.identity {
        return Err(READER_CONTEXT_DIFFERS);
    }
    // Linux renders NUMA file= paths relative to the reading task's root.
    // The normal Hermit controller completes container setup before entering
    // its current-thread runtime; its subsequent workers do not chroot/setns
    // or mutate a shared fs_struct. This synchronous, no-yield request retains
    // the grant/pidfd while native exclusion parks the target's only FS sharer.
    // That stability is a required controller contract, not an ABA guarantee
    // inferred from endpoint equality for arbitrary embedding callbacks.
    let read = |leaf| {
        let file = open_at(target_task.as_raw_fd(), leaf, libc::O_RDONLY)
            .map_err(|_| "controller mapping file open failed")?;
        let mut bytes = Vec::new();
        file.take((MAX_VIEW_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|_| "controller mapping file read failed")?;
        if bytes.len() > MAX_VIEW_BYTES {
            return Err("controller mapping file exceeds fixed capacity");
        }
        Ok(bytes)
    };
    let maps = read(c"maps")?;
    let numa = read(c"numa_maps")?;
    let target_after = ReaderContext::open(&target_task, false)?;
    let reader_after = ReaderContext::open(&reader_task, true)?;
    if target_after.identity != target_before.identity
        || reader_after.identity != reader_before.identity
    {
        return Err(READER_CONTEXT_DIFFERS);
    }
    Ok((maps, numa))
}

/// Validate the controller echo and project raw NUMA bytes only after the
/// backend has restored its executors. Completion/rejection is owned by capture.
fn project_captured_reply(
    token: MappingCaptureToken,
    response: (Option<LogicalTime>, GlobalResponse),
    regions: &[reverie::RuntimePrivateMemoryRegion],
) -> Result<Vec<u8>, String> {
    match response {
        (
            None,
            GlobalResponse::MappingCapture(Reply::Captured {
                token: echoed,
                maps,
                numa,
            }),
        ) if echoed == token => {
            crate::procfs::private_maps::project_numa(&maps, &numa, regions).map_err(str::to_owned)
        }
        (None, GlobalResponse::MappingCapture(Reply::Refused(reason))) => Err(reason),
        _ => Err("controller reply identity or framing mismatched".into()),
    }
}

pub(crate) fn capture<G, T>(guest: &G, target: &MapsTarget) -> Result<Vec<u8>, Error>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let fail = |reason| {
        Error::Tool(anyhow::anyhow!(
            "runtime-private numa_maps view unavailable: {reason}"
        ))
    };
    let fact = guest
        .controller_capture_fact()
        .ok_or_else(|| fail("controller authority is unavailable".to_owned()))?;
    let state = guest.thread_state();
    let time = state.thread_logical_time.clone();
    let mm = state.mm_id;
    let request = |operation| {
        (
            time.clone(),
            mm,
            GlobalRequest::MappingCapture(Request(operation)),
        )
    };
    let token = match guest.try_send_rpc_sync(request(Operation::Prepare(target.clone(), fact))) {
        Ok((None, GlobalResponse::MappingCapture(Reply::Prepared(token)))) => token,
        Ok((None, GlobalResponse::MappingCapture(Reply::Refused(reason)))) => {
            return Err(fail(reason));
        }
        _ => return Err(fail("controller preparation failed".into())),
    };
    let prepared = reverie::PreparedMappingCapture::new(
        token,
        target.private.clone(),
        request(Operation::Read(token)),
    )
    .map_err(|error| fail(error.to_string()))?;
    let response = guest
        .try_mapping_capture(prepared)
        .map_err(|error| fail(error.to_string()))?;
    let projected = project_captured_reply(token, response, &target.private.regions).map_err(fail);
    // Returning from try_mapping_capture establishes restoration independently
    // of whether the controller view/projector subsequently accepts the bytes.
    // A rejected view still poisons completion, with its actual reason.
    let rejection = projected.as_ref().err().map(ToString::to_string);
    let completed = guest.try_send_rpc_sync(request(Operation::Complete(token, rejection)));
    let projected = projected?;
    if !matches!(
        completed,
        Ok((None, GlobalResponse::MappingCapture(Reply::Completed)))
    ) {
        return Err(fail(
            "controller did not accept completed restoration".into(),
        ));
    }
    Ok(projected)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAPS: &[u8] = b"1000-2000 r--p 00000000 00:00 0 /guest\n\
        2000-3000 rw-p 00000000 00:00 0 /runtime\n";
    const GUEST_NUMA: &[u8] = b"1000 default file=/guest unknown=kept N0=7\n";
    const PRIVATE_NUMA: &[u8] = b"2000 default file=/runtime N0=9\n";
    const REGIONS: &[reverie::RuntimePrivateMemoryRegion] =
        &[reverie::RuntimePrivateMemoryRegion {
            start: 0x2000,
            end: 0x3000,
        }];
    const MISMATCH: &str = "controller reply identity or framing mismatched";

    fn token() -> MappingCaptureToken {
        MappingCaptureToken {
            nonce: 17,
            identity: MappingCaptureIdentity {
                process: 3,
                thread: 3,
                address_space: [4, 5],
                private_owner: [11, 12, 13, 14],
                private_version: 15,
            },
        }
    }

    fn captured(token: MappingCaptureToken) -> GlobalResponse {
        GlobalResponse::MappingCapture(Reply::Captured {
            token,
            maps: MAPS.to_vec(),
            numa: [GUEST_NUMA, PRIVATE_NUMA].concat(),
        })
    }

    #[test]
    fn exact_capture_echo_projects_raw_numa_without_sanitizing() {
        let token = token();
        assert_eq!(
            project_captured_reply(token, (None, captured(token)), REGIONS).unwrap(),
            GUEST_NUMA,
            "only the wholly private row disappears; raw guest fields remain"
        );
    }

    #[test]
    fn every_capture_token_field_is_required_before_projection() {
        let token = token();
        type TokenMutation = (&'static str, fn(&mut MappingCaptureToken));
        let mutations: [TokenMutation; 10] = [
            ("nonce", |token| token.nonce += 1),
            ("process", |token| token.identity.process += 1),
            ("thread", |token| token.identity.thread += 1),
            ("address_space[0]", |token| {
                token.identity.address_space[0] += 1
            }),
            ("address_space[1]", |token| {
                token.identity.address_space[1] += 1
            }),
            ("private_owner[0]", |token| {
                token.identity.private_owner[0] += 1
            }),
            ("private_owner[1]", |token| {
                token.identity.private_owner[1] += 1
            }),
            ("private_owner[2]", |token| {
                token.identity.private_owner[2] += 1
            }),
            ("private_owner[3]", |token| {
                token.identity.private_owner[3] += 1
            }),
            ("private_version", |token| {
                token.identity.private_version += 1
            }),
        ];
        for (field, mutate) in mutations {
            let mut echoed = token;
            mutate(&mut echoed);
            assert_ne!(echoed, token, "ineffective token mutation: {field}");
            assert_eq!(
                project_captured_reply(token, (None, captured(echoed)), REGIONS).unwrap_err(),
                MISMATCH,
                "changed controller token field accepted: {field}"
            );
        }
    }

    #[test]
    fn capture_reply_rejects_clocks_and_unexpected_response_variants() {
        let token = token();
        let replies = [
            (Some(LogicalTime::ZERO), captured(token)),
            (Some(LogicalTime::from_nanos(1)), captured(token)),
            (None, GlobalResponse::MappingCapture(Reply::Prepared(token))),
            (None, GlobalResponse::MappingCapture(Reply::Completed)),
            (None, GlobalResponse::ThreadExited),
            (None, GlobalResponse::Granted(Box::new(captured(token)))),
            (
                None,
                GlobalResponse::WithDeadline(Duration::ZERO, Box::new(captured(token))),
            ),
            (
                Some(LogicalTime::ZERO),
                GlobalResponse::MappingCapture(Reply::Refused(
                    "must not bypass clock guard".into(),
                )),
            ),
        ];
        for response in replies {
            assert_eq!(
                project_captured_reply(token, response, REGIONS).unwrap_err(),
                MISMATCH
            );
        }
    }

    #[test]
    fn capture_reply_preserves_controller_and_projector_refusal_reasons() {
        let token = token();
        let reason = "mapping reader root or mount namespace differs";
        assert_eq!(
            project_captured_reply(
                token,
                (
                    None,
                    GlobalResponse::MappingCapture(Reply::Refused(reason.into()))
                ),
                REGIONS,
            )
            .unwrap_err(),
            reason
        );
        let response = GlobalResponse::MappingCapture(Reply::Captured {
            token,
            maps: MAPS[..MAPS.len() - 1].to_vec(),
            numa: [GUEST_NUMA, PRIVATE_NUMA].concat(),
        });
        assert_eq!(
            project_captured_reply(token, (None, response), REGIONS).unwrap_err(),
            "truncated maps snapshot"
        );
    }
}
