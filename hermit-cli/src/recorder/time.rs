/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use reverie::Errno;
use reverie::Guest;
use reverie::syscalls::ClockGettime;
use reverie::syscalls::Gettimeofday;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::Time;
use reverie::syscalls::Timespec;

use super::Recorder;
use crate::event::SyscallEvent;
use crate::event::TimespecEvent;

impl Recorder {
    pub(super) async fn handle_clock_gettime<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: ClockGettime,
    ) -> Result<i64, Errno> {
        let result = guest.inject(syscall).await;

        let event = result.and_then(|ret| {
            debug_assert_eq!(ret, 0);
            let timespec: Timespec = guest
                .memory()
                .read_value(syscall.tp().ok_or(Errno::EFAULT)?)?;
            Ok(SyscallEvent::Timespec(TimespecEvent { timespec }))
        });

        self.record_event(guest, event);

        result
    }

    pub(super) async fn handle_time<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Time,
    ) -> Result<i64, Errno> {
        // No need to record the pointer value since it is the same as the
        // return value. Upon replay, we have to set the pointer as well.
        let result = guest.inject(syscall).await;
        self.record_event(guest, result.map(SyscallEvent::Return));
        result
    }

    pub(super) async fn handle_gettimeofday<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Gettimeofday,
    ) -> Result<i64, Errno> {
        let result = guest.inject(syscall).await;
        self.record_event(
            guest,
            result.and_then(|ret| {
                debug_assert_eq!(ret, 0);

                // NOTE: Either `tv` or `tz` can be NULL and not return EFAULT.
                let tv = match syscall.tv() {
                    Some(addr) => guest.memory().read_value(addr)?,
                    None => Default::default(),
                };

                let tz = match syscall.tz() {
                    Some(addr) => guest.memory().read_value(addr)?,
                    None => Default::default(),
                };

                Ok(SyscallEvent::Timeofday((tv, tz)))
            }),
        );
        result
    }
}
