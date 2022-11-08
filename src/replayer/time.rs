// (c) Meta Platforms, Inc. and affiliates. Confidential and proprietary.

use reverie::syscalls::ClockGettime;
use reverie::syscalls::Gettimeofday;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::Time;
use reverie::Errno;
use reverie::Guest;

use super::Replayer;

impl Replayer {
    pub(super) async fn handle_clock_gettime<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: ClockGettime,
    ) -> Result<i64, Errno> {
        let addr = syscall.tp().ok_or(Errno::EFAULT)?;

        next_event!(guest, Timespec).and_then(|event| {
            guest.memory().write_value(addr, &event.timespec)?;

            // clock_gettime always returns 0 on success.
            Ok(0)
        })
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
