/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 * This source code is licensed under the BSD-style license found in LICENSE.
 */

//! Shared actual watched-peer and one-use InitialImage checks.
//! No packet, digest, PID, or this module's numerical test data grants launch,
//! mapping, source, stopped-generation, or runtime-ready authority.

use std::io;
use std::os::fd::AsFd;
use std::os::fd::AsRawFd;
use std::os::fd::BorrowedFd;
use std::os::fd::OwnedFd;
use std::sync::Weak;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use detcore::detlog::NativePhysicalWatchIdentity;
use hermit::in_guest_exits::NativeWatchedProcess;
use reverie::GlobalTool;
use reverie_rpc_transport::ExitReporter;

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

pub(super) fn alive(fd: BorrowedFd<'_>) -> io::Result<()> {
    let mut poll = libc::pollfd {
        fd: fd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    if unsafe { libc::poll(&mut poll, 1, 0) } != 0 || poll.revents != 0 {
        return Err(invalid("native watched birth is no longer live"));
    }
    Ok(())
}
pub(super) fn entropy() -> io::Result<[u8; 32]> {
    let mut bytes = [0; 32];
    let mut done = 0;
    while done < bytes.len() {
        let n =
            unsafe { libc::getrandom(bytes[done..].as_mut_ptr().cast(), bytes.len() - done, 0) };
        if n < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if n == 0 {
            return Err(invalid("native launch nonce source returned EOF"));
        }
        done += n as usize;
    }
    if bytes == [0; 32] {
        return Err(invalid("native launch nonce was empty"));
    }
    Ok(bytes)
}

pub(super) fn inherited_socket_identity(
    fd: BorrowedFd<'_>,
    destination: i32,
) -> io::Result<String> {
    let encoded = detcore::detlog::tool_output_env_value(fd.as_raw_fd())?;
    let (_, (device, inode)) = detcore::detlog::parse_tool_output_env_value(&encoded)
        .ok_or_else(|| invalid("native endpoint identity malformed"))?;
    Ok(format!("{destination}:{device:016x}:{inode:016x}"))
}

pub(super) struct NativeReporter {
    pub(super) global: Weak<detcore::GlobalState>,
}
impl ExitReporter for NativeReporter {
    fn process_exited(&self, pid: i32) {
        if let Some(global) = self.global.upgrade() {
            if let Some(store) = global.native_exec_registry() {
                store.lock().unwrap().died();
            }
            global.on_backend_process_exited(pid);
        }
    }
    fn pending_process_exits(&self) -> Vec<i32> {
        self.global
            .upgrade()
            .map(|g| g.backend_pending_process_exits())
            .unwrap_or_default()
    }
    fn backend_failed(&self, failure: reverie::BackendFailure) {
        if let Some(global) = self.global.upgrade() {
            if let Some(store) = global.native_exec_registry() {
                store.lock().unwrap().provision_transport_failed();
            }
            global.report_backend_failure(failure);
        }
    }
}

/// The peer descriptor comes from the accepted socket (SO_PEERPIDFD), not
/// from a PID in a request. The expected watch was retained before entry.
pub(super) fn require_watched_peer(
    watch: &NativeWatchedProcess,
    peer: &OwnedFd,
    failed: &AtomicBool,
) -> io::Result<()> {
    alive(watch.retained_pidfd())?;
    alive(peer.as_fd())?;
    if failed.load(Ordering::Acquire)
        || !watch.watch_is_healthy()
        || hermit::physical_exit_watch::birth_identity(peer) != Some(watch.birth_identity())
    {
        return Err(invalid(
            "native peer/watch/source belongs to a different or failed birth",
        ));
    }
    Ok(())
}

/// Values to compare only after the actual peer/watch has been authenticated.
/// This structure is deliberately not a physical authority or admission result.
pub(super) struct InitialRedemption<'a> {
    pub(super) process: i32,
    pub(super) nonce: &'a [u8],
    pub(super) digest: [u8; 32],
}

/// Preserve native's short-circuit order: mismatches do not consume the bit;
/// the caller poisons the entire launch on every error. A successful comparison
/// consumes before any further image checks or acknowledgement can occur.
pub(super) fn consume_initial_redemption(
    expected: InitialRedemption<'_>,
    actual: InitialRedemption<'_>,
    used: &AtomicBool,
) -> io::Result<()> {
    if actual.process != expected.process
        || actual.nonce != expected.nonce
        || actual.digest != expected.digest
        || used.swap(true, Ordering::AcqRel)
    {
        return Err(invalid(
            "native InitialImage owner/nonce/Config/replay refused",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_redemption_consumes_once_without_reset() {
        let used = AtomicBool::new(false);
        let expected = || InitialRedemption {
            process: 7,
            nonce: &[1; 32],
            digest: [2; 32],
        };
        assert!(consume_initial_redemption(expected(), expected(), &used).is_ok());
        assert!(used.load(Ordering::Acquire));
        assert!(consume_initial_redemption(expected(), expected(), &used).is_err());
        assert!(used.load(Ordering::Acquire));
    }

    #[test]
    fn every_initial_binding_mismatch_refuses_before_consumption() {
        // These are numerical comparator controls, never mocked pidfd/watch or
        // physical positive tests. The real caller additionally poisons errors.
        for (process, nonce, digest) in [
            (8, &[1; 32][..], [2; 32]),
            (7, &[3; 32][..], [2; 32]),
            (7, &[1; 32][..], [4; 32]),
            (7, &[1; 31][..], [2; 32]),
        ] {
            let used = AtomicBool::new(false);
            let result = consume_initial_redemption(
                InitialRedemption {
                    process: 7,
                    nonce: &[1; 32],
                    digest: [2; 32],
                },
                InitialRedemption {
                    process,
                    nonce,
                    digest,
                },
                &used,
            );
            assert!(result.is_err());
            assert!(!used.load(Ordering::Acquire));
        }
    }
}
