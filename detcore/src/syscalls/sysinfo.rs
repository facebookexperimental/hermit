/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use procfs::process::Process;
use reverie::Error;
use reverie::Guest;
use reverie::syscalls;
use reverie::syscalls::Errno;
use reverie::syscalls::MemoryAccess;

use crate::Detcore;
use crate::RecordOrReplay;
use crate::tool_global::thread_observe_time;
use crate::tool_local::ResourceLimit;

const MB: u64 = 1024 * 1024;
const CLOCK_TICKS_PER_SECOND: u64 = 100;
const NANOS_PER_CLOCK_TICK: u64 = 1_000_000_000 / CLOCK_TICKS_PER_SECOND;

fn logical_clock_ticks(
    now: crate::types::LogicalTime,
    boot: crate::types::LogicalTime,
    uptime_offset_seconds: u64,
) -> libc::clock_t {
    let elapsed_ticks = (now - boot).as_nanos() / NANOS_PER_CLOCK_TICK;
    let ticks = uptime_offset_seconds
        .saturating_mul(CLOCK_TICKS_PER_SECOND)
        .saturating_add(elapsed_ticks);
    libc::clock_t::try_from(ticks).unwrap_or(libc::clock_t::MAX)
}

impl<T: RecordOrReplay> Detcore<T> {
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#663)
    /// Return one deterministic process resource limit through the legacy ABI.
    pub async fn handle_getrlimit<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Getrlimit,
    ) -> Result<i64, Error> {
        let resource = u32::try_from(call.resource()).map_err(|_| Errno::EINVAL)?;
        let address = call.rlim().ok_or(Errno::EFAULT)?;
        let limit = guest
            .thread_state()
            .resource_limits
            .lock()
            .expect("resource limits mutex poisoned")
            .get(resource)
            .ok_or(Errno::EINVAL)?;
        let result = libc::rlimit {
            rlim_cur: limit.current,
            rlim_max: limit.maximum,
        };
        guest.memory().write_value(address, &result)?;
        Ok(0)
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#663)
    /// Update one virtual process resource limit through the legacy ABI.
    pub async fn handle_setrlimit<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Setrlimit,
    ) -> Result<i64, Error> {
        let resource = u32::try_from(call.resource()).map_err(|_| Errno::EINVAL)?;
        let address = call.rlim().ok_or(Errno::EFAULT)?;
        let requested: libc::rlimit = guest.memory().read_value(address)?;
        let requested = ResourceLimit {
            current: requested.rlim_cur,
            maximum: requested.rlim_max,
        };
        let resource_limits = guest.thread_state().resource_limits.clone();
        let mut limits = resource_limits
            .lock()
            .expect("resource limits mutex poisoned");
        let previous = limits.get(resource).ok_or(Errno::EINVAL)?;
        if requested.current > requested.maximum {
            return Err(Errno::EINVAL.into());
        }
        if resource != libc::RLIMIT_STACK && resource != libc::RLIMIT_NOFILE {
            return Err(Errno::EPERM.into());
        }
        if requested.maximum > previous.maximum {
            return Err(Errno::EPERM.into());
        }
        limits.set(resource, requested);
        Ok(0)
    }

    /// Virtualize `prlimit64(2)` for the current guest process.
    ///
    /// Queries return process-local deterministic values. Mutations are kept
    /// virtual and restricted to limits that do not grant access to host
    /// resources or affect host scheduling. Accepted mutations update only
    /// guest-observable compatibility state; they are not a sandbox boundary
    /// and do not ask the host kernel to enforce the virtual limit.
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#534)
    pub async fn handle_prlimit64<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Prlimit64,
    ) -> Result<i64, Error> {
        let resource = call.resource();
        let resource_limits = guest.thread_state().resource_limits.clone();
        if resource_limits
            .lock()
            .expect("resource limits mutex poisoned")
            .get(resource)
            .is_none()
        {
            return Err(Errno::EINVAL.into());
        }

        let requested = if let Some(address) = call.new_rlim() {
            let limit: libc::rlimit64 = guest.memory().read_value(address)?;
            Some(ResourceLimit {
                current: limit.rlim_cur,
                maximum: limit.rlim_max,
            })
        } else {
            None
        };

        let pid = call.pid();
        if pid != 0 && pid != guest.pid().as_raw() {
            return Err(Errno::EPERM.into());
        }

        let previous = {
            let mut limits = resource_limits
                .lock()
                .expect("resource limits mutex poisoned");
            let previous = limits
                .get(resource)
                .expect("resource validity changed while handling prlimit64");

            if let Some(requested) = requested {
                if requested.current > requested.maximum {
                    return Err(Errno::EINVAL.into());
                }
                if resource != libc::RLIMIT_STACK && resource != libc::RLIMIT_NOFILE {
                    return Err(Errno::EPERM.into());
                }
                if requested.maximum > previous.maximum {
                    return Err(Errno::EPERM.into());
                }
                limits.set(resource, requested);
            }

            previous
        };

        if let Some(address) = call.old_rlim() {
            let previous = libc::rlimit64 {
                rlim_cur: previous.current,
                rlim_max: previous.maximum,
            };
            guest.memory().write_value(address, &previous)?;
        }

        crate::detlog!(
            "prlimit64: pid={pid}, resource={resource}, mutation={}, old={}:{}",
            requested.is_some(),
            previous.current,
            previous.maximum
        );
        Ok(0)
    }
    /// Return a deterministic resource-usage snapshot. Host CPU times, page-fault counts, and
    /// context-switch counts depend on kernel scheduling, so report zero until Detcore models
    /// those counters using logical execution progress.
    ///
    /// `ru_maxrss` is the exception: it is populated with the guest's peak resident set size so
    /// that programs which require a positive maximum RSS (e.g. rr's `rusage` test) behave like
    /// they do on Linux. The value comes from the same procfs memory accounting that `sysinfo`'s
    /// free-memory reporting already relies on, which is deterministic across runs under Detcore's
    /// fixed schedule.
    pub async fn handle_getrusage<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Getrusage,
    ) -> Result<i64, Error> {
        let who = call.who();
        match who {
            libc::RUSAGE_SELF | libc::RUSAGE_CHILDREN | libc::RUSAGE_THREAD => {}
            _ => return Err(Errno::EINVAL.into()),
        }

        let usage_addr = call.usage().ok_or(Errno::EFAULT)?;

        // SAFETY: `libc::rusage` is a plain-old-data C struct that is valid when zero-initialized.
        let mut usage: libc::rusage = unsafe { std::mem::zeroed() };

        // RUSAGE_SELF/RUSAGE_THREAD report this process's peak RSS. RUSAGE_CHILDREN aggregates
        // terminated children only; with no such accounting we leave it zero, matching Linux when
        // no child has exited.
        if matches!(who, libc::RUSAGE_SELF | libc::RUSAGE_THREAD) {
            usage.ru_maxrss = self.guest_peak_rss_kb(guest) as libc::c_long;
        }

        guest.memory().write_value(usage_addr, &usage)?;
        Ok(0)
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#783): Review logical tick and zero-CPU accounting semantics.
    /// Return deterministic elapsed ticks and process CPU accounting for `times(2)`.
    ///
    /// Linux's host boot epoch and scheduler CPU counters are nondeterministic. Detcore instead
    /// derives the return value from its logical clock and reports zero CPU ticks, matching the
    /// policy used by `getrusage` until logical per-process CPU accounting is modeled.
    pub async fn handle_times<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Times,
    ) -> Result<i64, Error> {
        let now = thread_observe_time(guest).await;
        let boot = crate::types::DetTime::new(&self.cfg).as_nanos();
        let ticks = logical_clock_ticks(now, boot, self.cfg.sysinfo_uptime_offset);

        if let Some(address) = call.buf() {
            let usage = libc::tms {
                tms_utime: 0,
                tms_stime: 0,
                tms_cutime: 0,
                tms_cstime: 0,
            };
            guest.memory().write_value(address, &usage)?;
        }

        Ok(ticks as i64)
    }

    /// The guest's peak resident set size ("high water mark") in kibibytes, matching the units of
    /// Linux `getrusage`'s `ru_maxrss`. Reads procfs like [`Self::free_ram`]; always returns a
    /// positive value so guests can rely on a nonzero maximum RSS even if the read fails.
    fn guest_peak_rss_kb<G: Guest<Self>>(&self, guest: &G) -> u64 {
        Process::new(guest.pid().as_raw())
            .and_then(|process| process.status())
            .ok()
            .and_then(|status| status.vmhwm.or(status.vmrss))
            .unwrap_or(0)
            .max(1)
    }

    /// handle sysinfo syscall
    pub async fn handle_sysinfo<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Sysinfo,
    ) -> Result<i64, Error> {
        let sys_info = self.collect_sysinfo(guest).await?;
        let mut memory = guest.memory();

        if let Some(info_addr) = call.info() {
            memory.write_value(info_addr, &sys_info.into())?;
        }
        Ok(0)
    }

    pub(super) async fn calculate_uptime<G: Guest<Self>>(
        &self,
        guest: &mut G,
    ) -> Result<u64, Error> {
        let global_time = thread_observe_time(guest).await;
        Ok(self.cfg.sysinfo_uptime_offset + global_time.as_secs()
            - crate::types::DetTime::new(&self.cfg).as_nanos().as_secs())
    }

    async fn collect_sysinfo<G: Guest<Self>>(
        &self,
        guest: &mut G,
    ) -> Result<syscalls::SysInfo, Error> {
        Ok(syscalls::SysInfo {
            uptime: self.calculate_uptime(guest).await?,
            loads_1: 1,
            loads_5: 1,
            loads_15: 1,
            total_ram: self.cfg.memory,
            free_ram: self.free_ram(guest, self.cfg.memory)?,
            buffer_ram: MB,
            shared_ram: MB,
            total_swap: 0,
            free_swap: 0,
            procs: 1,
            total_high: 0,
            free_high: 0,
            mem_unit: 1,
        })
    }

    fn free_ram<G: Guest<Self>>(&self, guest: &mut G, total_ram: u64) -> anyhow::Result<u64> {
        let process = Process::new(guest.pid().as_raw())?;
        let page_size = procfs::page_size();
        let statm = process.statm()?;
        let used_memory = statm.resident * page_size;
        if used_memory > total_ram {
            return Ok(0);
        }
        Ok(total_ram - used_memory)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::LogicalTime;

    #[test]
    fn logical_clock_ticks_include_boot_offset_and_fractional_seconds() {
        let boot = LogicalTime::from_secs(1_000);
        let now = boot + LogicalTime::from_millis(25);

        assert_eq!(logical_clock_ticks(now, boot, 120), 12_002);
    }

    #[test]
    fn logical_clock_ticks_saturate_at_clock_t_max() {
        let ticks = logical_clock_ticks(LogicalTime::MAX, LogicalTime::ZERO, u64::MAX);

        assert_eq!(ticks, libc::clock_t::MAX);
    }
}
