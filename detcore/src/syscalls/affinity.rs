/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! CPU affinity under `--virtual-cpus`: each task's mask over the machine's
//! N virtual CPUs, kept by the scheduler (`ThreadTree::cpu_affinities`),
//! and the CPU a thread is told it runs on. Without the flag the guest has
//! Hermit's historical single CPU, answered in `threads.rs` without a mask.

use reverie::Error;
use reverie::Guest;
use reverie::syscalls;
use reverie::syscalls::Errno;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::Sysno;

use super::VIRTUAL_CPUSET_BYTES;
use crate::cpu_set::CpuSet;
use crate::record_or_replay::RecordOrReplay;
use crate::scheduler::pid_namespaces::GuestPidAnswer;
use crate::tool_global::cpu_affinity;
use crate::tool_global::set_cpu_affinity;
use crate::tool_local::Detcore;
use crate::types::DetTid;

/// Whether `sys_sched_getaffinity` accepts a buffer of `len` bytes on a
/// machine of `cpus` CPUs (kernel/sched/syscalls.c:1331-1334): its bits
/// must cover `nr_cpu_ids`, in the kernel's unsigned int arithmetic, and it
/// must be a whole number of `long`s.
fn getaffinity_length_is_valid(len: u32, cpus: u32) -> bool {
    len.wrapping_mul(8) >= cpus
        && (len as usize).is_multiple_of(std::mem::size_of::<libc::c_ulong>())
}

impl<T: RecordOrReplay> Detcore<T> {
    /// The task `pid` names for the affinity calls, as `find_process_by_pid`
    /// reads it: the caller for 0, no task (ESRCH) for a negative number or
    /// one no task of the caller's PID namespace has.
    async fn affinity_target<G: Guest<Self>>(
        &self,
        guest: &mut G,
        sysno: Sysno,
        pid: libc::pid_t,
    ) -> Result<DetTid, Error> {
        match pid {
            0 => Ok(guest.thread_state().dettid),
            pid if pid < 0 => Err(Errno::ESRCH.into()),
            pid if !guest.thread_state().nested_pid_namespace => Ok(DetTid::from_raw(pid)),
            pid => match crate::tool_global::guest_pid(guest, pid).await {
                GuestPidAnswer::Task { task, .. } => Ok(task),
                GuestPidAnswer::NoSuchPid => Err(Errno::ESRCH.into()),
                GuestPidAnswer::Untranslatable(reason) => {
                    Err(self.refuse_nested_pid(guest, sysno, &reason).await)
                }
            },
        }
    }

    /// `sched_getaffinity` on a machine of `cpus` virtual CPUs, as Linux's
    /// `sys_sched_getaffinity` (kernel/sched/syscalls.c) answers: EINVAL for
    /// a length whose bits do not cover `nr_cpu_ids` (`cpus`) or that is not
    /// a multiple of `sizeof(long)`; ESRCH for no such task; otherwise the
    /// task's mask, `min(len, cpumask_size())` bytes of it, and that count.
    pub(super) async fn virtual_sched_getaffinity<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::SchedGetaffinity,
        cpus: u32,
    ) -> Result<i64, Error> {
        let len = call.len();
        if !getaffinity_length_is_valid(len, cpus) {
            return Err(Errno::EINVAL.into());
        }
        let target = self
            .affinity_target(guest, Sysno::sched_getaffinity, call.pid())
            .await?;
        let affinity = cpu_affinity(guest, target).await.cpus(cpus)?;
        let copied = (len as usize).min(VIRTUAL_CPUSET_BYTES);
        let mask = call.mask().ok_or(Errno::EFAULT)?;
        guest
            .memory()
            .write_exact(mask.cast(), &affinity.to_mask_bytes()[..copied])?;
        Ok(copied as i64)
    }

    /// `sched_setaffinity` on a machine of `cpus` virtual CPUs, as Linux's
    /// `sys_sched_setaffinity` (kernel/sched/syscalls.c) does it: read the
    /// mask (`get_user_cpu_mask`: at most `cpumask_size()` bytes, a shorter
    /// mask naming no CPU past its end; EFAULT for an unreadable one), find
    /// the task (ESRCH), keep the requested CPUs the machine has
    /// (`__sched_setaffinity`'s `cpumask_and` with the possible CPUs), and
    /// refuse a mask left with none (EINVAL, `__set_cpus_allowed_ptr_locked`
    /// finding no `dest_cpu`). The task's mask is then the CPUs kept, which
    /// `sched_getaffinity` reads back.
    pub(super) async fn virtual_sched_setaffinity<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::SchedSetaffinity,
        cpus: u32,
    ) -> Result<i64, Error> {
        let len = (call.len() as usize).min(VIRTUAL_CPUSET_BYTES);
        let mut bytes = [0u8; VIRTUAL_CPUSET_BYTES];
        if len > 0 {
            let mask = call.mask().ok_or(Errno::EFAULT)?;
            guest.memory().read_exact(mask.cast(), &mut bytes[..len])?;
        }
        let target = self
            .affinity_target(guest, Sysno::sched_setaffinity, call.pid())
            .await?;
        let kept = CpuSet::from_mask_bytes(&bytes).intersection(CpuSet::first(cpus));
        if kept.is_empty() {
            // ESRCH before EINVAL: the kernel finds the task first.
            cpu_affinity(guest, target).await.cpus(cpus)?;
            return Err(Errno::EINVAL.into());
        }
        set_cpu_affinity(guest, target, kept).await.cpus(cpus)?;
        Ok(0)
    }

    /// The virtual CPU the calling thread runs on, of a machine with `cpus`
    /// CPUs: which `getcpu` reports and whose APIC IDs CPUID answers. It is
    /// placed by its home and its affinity mask (`TaskCpus::current`):
    /// always a CPU the thread may run on, changing only with that mask, and
    /// taken in turn by threads created one after another. It depends on no
    /// scheduling decision. The caller is a live task, which the scheduler
    /// always answers for; CPU 0 is a fallback that is logged.
    pub(crate) async fn virtual_cpu<G: Guest<Self>>(&self, guest: &mut G, cpus: u32) -> u32 {
        let me = guest.thread_state().dettid;
        cpu_affinity(guest, me)
            .await
            .current_cpu(cpus)
            .unwrap_or_else(|errno| {
                tracing::error!("[detcore, dtid {me}] no virtual CPU for a live task: {errno}");
                0
            })
    }
}

#[cfg(test)]
mod tests {
    use super::getaffinity_length_is_valid;

    /// `len * 8 < nr_cpu_ids` and a length that is no whole number of
    /// `long`s are EINVAL; the multiplication wraps as the kernel's
    /// unsigned int does.
    #[test]
    fn getaffinity_needs_a_length_covering_every_cpu() {
        assert!(getaffinity_length_is_valid(8, 64));
        assert!(!getaffinity_length_is_valid(8, 65));
        assert!(getaffinity_length_is_valid(16, 65));
        assert!(getaffinity_length_is_valid(16, 128));
        assert!(!getaffinity_length_is_valid(4, 1));
        assert!(!getaffinity_length_is_valid(0, 1));
        assert!(!getaffinity_length_is_valid(12, 4));
        assert!(!getaffinity_length_is_valid(0x2000_0000, 4));
        assert!(getaffinity_length_is_valid(0x2000_0008, 4));
    }
}
