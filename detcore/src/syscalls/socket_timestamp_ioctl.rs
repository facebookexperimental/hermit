/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Deterministic socket receive timestamps returned through `ioctl(2)`.

use reverie::Error;
use reverie::Guest;
use reverie::syscalls;
use reverie::syscalls::AddrMut;
use reverie::syscalls::Errno;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::ioctl::Request;

use crate::record_or_replay::RecordOrReplay;
use crate::tool_global::thread_observe_time;
use crate::tool_local::Detcore;
use crate::types::LogicalTime;

const SIOCGSTAMPNS_OLD: usize = 0x8907;
const SIOCGSTAMP_NEW: usize = 0x8010_8906;
const SIOCGSTAMPNS_NEW: usize = 0x8010_8907;

#[derive(Clone, Copy)]
enum TimestampOutput {
    Timeval(usize),
    Timespec(usize),
}

fn timestamp_output(call: syscalls::Ioctl) -> Option<TimestampOutput> {
    match call.request() {
        Request::SIOCGSTAMP(address) => Some(TimestampOutput::Timeval(
            address.map_or(0, |address| address.as_raw()),
        )),
        Request::Other(SIOCGSTAMP_NEW, address) => Some(TimestampOutput::Timeval(address)),
        Request::Other(SIOCGSTAMPNS_OLD | SIOCGSTAMPNS_NEW, address) => {
            Some(TimestampOutput::Timespec(address))
        }
        _ => None,
    }
}

/// Returns whether `call` reads a socket receive timestamp.
pub(crate) fn is_socket_timestamp_ioctl(call: syscalls::Ioctl) -> bool {
    matches!(
        call.request(),
        Request::SIOCGSTAMP(_)
            | Request::Other(SIOCGSTAMP_NEW | SIOCGSTAMPNS_OLD | SIOCGSTAMPNS_NEW, _,)
    )
}

fn logical_timeval(now: LogicalTime) -> libc::timeval {
    libc::timeval {
        tv_sec: now.as_secs() as libc::time_t,
        tv_usec: (now.subsec_nanos() / 1_000) as libc::suseconds_t,
    }
}

fn logical_timespec(now: LogicalTime) -> libc::timespec {
    libc::timespec {
        tv_sec: now.as_secs() as libc::time_t,
        tv_nsec: now.subsec_nanos() as libc::c_long,
    }
}

impl<T: RecordOrReplay> Detcore<T> {
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-912)
    /// Preserve kernel validation while replacing host receive time with logical time.
    pub async fn handle_socket_timestamp_ioctl<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Ioctl,
    ) -> Result<i64, Error> {
        let output = timestamp_output(call).ok_or(Errno::EFAULT)?;
        // Legacy socket timestamp requests predate _IOC direction/size bits, so
        // Reverie's generic recorder cannot identify their output. On x86_64,
        // the time64 request layouts are ABI-identical and encode the 16-byte
        // output, allowing the existing record/replay path to preserve the
        // kernel's fd, socket-state, and pointer validation.
        let recorded_call = match output {
            TimestampOutput::Timeval(address) => {
                call.with_request(Request::Other(SIOCGSTAMP_NEW, address))
            }
            TimestampOutput::Timespec(address) => {
                call.with_request(Request::Other(SIOCGSTAMPNS_NEW, address))
            }
        };
        let result = self.record_or_replay(guest, recorded_call).await?;
        let now = thread_observe_time(guest).await;
        match output {
            TimestampOutput::Timeval(address) => {
                let address = AddrMut::from_raw(address).ok_or(Errno::EFAULT)?;
                guest.memory().write_value(address, &logical_timeval(now))?;
            }
            TimestampOutput::Timespec(address) => {
                let address = AddrMut::from_raw(address).ok_or(Errno::EFAULT)?;
                guest
                    .memory()
                    .write_value(address, &logical_timespec(now))?;
            }
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logical_socket_timestamps_use_linux_units() {
        let now = LogicalTime::from_nanos(2_345_678_901);
        let timeval = logical_timeval(now);
        assert_eq!(timeval.tv_sec, 2);
        assert_eq!(timeval.tv_usec, 345_678);
        let timespec = logical_timespec(now);
        assert_eq!(timespec.tv_sec, 2);
        assert_eq!(timespec.tv_nsec, 345_678_901);
    }
}
