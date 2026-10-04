/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use reverie::Errno;
use reverie::Guest;
use reverie::syscalls::ClockGetres;
use reverie::syscalls::ClockGettime;
use reverie::syscalls::Gettimeofday;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::Time;

use super::Replayer;

impl Replayer {
    pub(super) async fn handle_clock_gettime<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: ClockGettime,
    ) -> Result<i64, Errno> {
        // Consume the recorded event before validating `tp`: the recorder logs
        // an `Err(EFAULT)` event for a NULL `tp`, and returning before reading
        // it would hand that error to the next syscall that reads an event.
        let event = next_event!(guest, Timespec)?;
        let addr = syscall.tp().ok_or(Errno::EFAULT)?;
        guest.memory().write_value(addr, &event.timespec)?;

        // clock_gettime always returns 0 on success.
        Ok(0)
    }

    pub(super) async fn handle_clock_getres<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: ClockGetres,
    ) -> Result<i64, Errno> {
        match syscall.res() {
            None => next_event!(guest, Return),
            Some(addr) => next_event!(guest, Timespec).and_then(|event| {
                guest.memory().write_value(addr, &event.timespec)?;
                Ok(0)
            }),
        }
    }

    pub(super) async fn handle_time<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Time,
    ) -> Result<i64, Errno> {
        let time = next_event!(guest, Return)?;

        if let Some(addr) = syscall.tloc() {
            guest.memory().write_value(addr, &time)?;
        }

        Ok(time)
    }

    pub(super) async fn handle_gettimeofday<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Gettimeofday,
    ) -> Result<i64, Errno> {
        let (tv, tz) = next_event!(guest, Timeofday)?;

        if let Some(addr) = syscall.tv() {
            guest.memory().write_value(addr, &tv)?;
        }

        if let Some(addr) = syscall.tz() {
            guest.memory().write_value(addr, &tz)?;
        }

        Ok(0)
    }
}
