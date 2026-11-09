/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! A set of the guest's virtual CPUs, as the kernel's `struct cpumask`
//! holds one: a task's affinity, or the CPUs a topology or cache file
//! names. Bit `n` is CPU `n`; Detcore's masks have
//! [`VIRTUAL_CPUSET_BYTES`] bytes, so CPUs 0 to 127.

use serde::Deserialize;
use serde::Serialize;

use crate::syscalls::VIRTUAL_CPUSET_BYTES;

/// A set of virtual CPUs: bit `n` of the 128-bit mask is CPU `n`.
///
/// The mask is held as its two 64-bit halves, least significant first,
/// rather than as a `u128`: a `u128` is 16-byte aligned, and a set held
/// across an `.await` would raise the alignment, and so the size, of every
/// syscall future that holds one (`syscall_hot_path_tests` bounds them). It
/// serializes, prints and hashes as the `u128` it is.
#[derive(Clone, Copy, Default, Eq, PartialEq)]
pub struct CpuSet([u64; 2]);

impl CpuSet {
    const fn from_bits(bits: u128) -> Self {
        Self([bits as u64, (bits >> 64) as u64])
    }

    const fn bits(self) -> u128 {
        ((self.0[1] as u128) << 64) | self.0[0] as u128
    }
}

impl std::fmt::Debug for CpuSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("CpuSet").field(&self.bits()).finish()
    }
}

impl std::hash::Hash for CpuSet {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.bits().hash(state)
    }
}

impl Serialize for CpuSet {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.bits().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for CpuSet {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        u128::deserialize(deserializer).map(Self::from_bits)
    }
}

const _: () = assert!(VIRTUAL_CPUSET_BYTES == std::mem::size_of::<u128>());
const _: () = assert!(detcore_model::config::MAX_VIRTUAL_CPUS as usize == VIRTUAL_CPUSET_BYTES * 8);

impl CpuSet {
    /// How many CPUs a set can name.
    const BITS: u32 = u128::BITS;

    /// CPUs `0..count`: every CPU of a machine with `count` CPUs.
    pub fn first(count: u32) -> Self {
        Self::from_bits(match count {
            0 => 0,
            count if count >= Self::BITS => u128::MAX,
            count => (1u128 << count) - 1,
        })
    }

    /// The one CPU `cpu`.
    pub fn only(cpu: u32) -> Self {
        Self::from_bits(1u128.checked_shl(cpu).unwrap_or(0))
    }

    /// The set a `sched_setaffinity` mask of `bytes` names, read as the
    /// kernel's `get_user_cpu_mask` reads it: bytes past the mask's size are
    /// ignored and a shorter mask leaves the CPUs past it out.
    pub fn from_mask_bytes(bytes: &[u8]) -> Self {
        let mut mask = [0u8; VIRTUAL_CPUSET_BYTES];
        let copied = bytes.len().min(VIRTUAL_CPUSET_BYTES);
        mask[..copied].copy_from_slice(&bytes[..copied]);
        Self::from_bits(u128::from_le_bytes(mask))
    }

    /// The set as the kernel lays out a `cpumask` in memory.
    pub fn to_mask_bytes(self) -> [u8; VIRTUAL_CPUSET_BYTES] {
        self.bits().to_le_bytes()
    }

    /// The CPUs in both sets.
    pub fn intersection(self, other: Self) -> Self {
        Self::from_bits(self.bits() & other.bits())
    }

    /// Whether `cpu` is in the set.
    pub fn contains(self, cpu: u32) -> bool {
        !self.intersection(Self::only(cpu)).is_empty()
    }

    /// Whether the set has no CPU.
    pub fn is_empty(self) -> bool {
        self.bits() == 0
    }

    /// How many CPUs the set has.
    pub fn len(self) -> u32 {
        self.bits().count_ones()
    }

    /// The set's CPUs, from the lowest.
    fn cpus(self) -> impl Iterator<Item = u32> {
        (0..Self::BITS).filter(move |cpu| self.contains(*cpu))
    }

    /// The set's `index`th CPU, counting from the lowest.
    pub fn nth(self, index: u32) -> Option<u32> {
        self.cpus().nth(index as usize)
    }

    /// The set as a kernel CPU list (`%*pbl`, as `online` and
    /// `Cpus_allowed_list` print it): ascending ranges, `0-3,5`.
    pub fn list(self) -> String {
        let mut ranges: Vec<(u32, u32)> = Vec::new();
        for cpu in self.cpus() {
            match ranges.last_mut() {
                Some((_, end)) if *end + 1 == cpu => *end = cpu,
                _ => ranges.push((cpu, cpu)),
            }
        }
        let ranges: Vec<String> = ranges
            .into_iter()
            .map(|(start, end)| match start == end {
                true => start.to_string(),
                false => format!("{start}-{end}"),
            })
            .collect();
        ranges.join(",")
    }

    /// The set as a kernel CPU mask of `bits` bits (`%*pb`, as `cpumap`,
    /// `thread_siblings` and `Cpus_allowed` print it, with `nr_cpu_ids`
    /// bits): hexadecimal groups of 32 bits, the most significant first,
    /// separated by commas, the first group only as wide as its bits need.
    pub fn mask(self, bits: u32) -> String {
        let bits = bits.clamp(1, Self::BITS);
        let mut groups = Vec::new();
        let mut low = bits.div_ceil(32) * 32;
        let mut width = match bits % 32 {
            0 => 32,
            partial => partial,
        };
        while low > 0 {
            low -= 32;
            let value = (self.bits() >> low) as u32 & (u32::MAX >> (32 - width));
            groups.push(format!(
                "{value:0digits$x}",
                digits = width.div_ceil(4) as usize
            ));
            width = 32;
        }
        groups.join(",")
    }
}

/// Where a task may run and where it is placed among those CPUs, as the
/// scheduler keeps them (`ThreadTree::cpu_affinities`, `ThreadTree::cpu_homes`).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct TaskCpus {
    /// The CPUs a `sched_setaffinity` left the task, or `None` where it may
    /// run on every virtual CPU, as every task starts.
    pub mask: Option<CpuSet>,
    /// The number of tasks the run created before it, so that tasks created
    /// one after another take the CPUs in turn.
    pub home: u32,
}

impl TaskCpus {
    /// The CPUs the task may run on, of a machine with `cpus` CPUs.
    pub fn allowed(self, cpus: u32) -> CpuSet {
        self.mask.unwrap_or(CpuSet::first(cpus))
    }

    /// The CPU the task runs on, of a machine with `cpus` CPUs, always one
    /// it may run on: CPU home mod `cpus` while it may run there, and
    /// otherwise its home's place among the CPUs it may run on. Linux moves
    /// a task only off a CPU its new mask leaves out (`affine_move_task`).
    pub fn current(self, cpus: u32) -> u32 {
        let allowed = self.allowed(cpus);
        let own = self.home % cpus.max(1);
        match allowed.contains(own) {
            true => own,
            false => allowed.nth(self.home % allowed.len().max(1)).unwrap_or(own),
        }
    }
}

/// A task's CPUs as the scheduler answers for them under `--virtual-cpus`
/// (`Scheduler::cpu_affinity`).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum TaskAffinity {
    /// No task Linux would find has the number asked for: ESRCH.
    NoSuchTask,
    /// A task: live, or a process that has exited and is not reaped.
    Task(TaskCpus),
}

impl TaskAffinity {
    /// The CPUs the task may run on, of a machine with `cpus` CPUs; ESRCH
    /// where there is no such task.
    pub fn cpus(self, cpus: u32) -> Result<CpuSet, reverie::Errno> {
        match self {
            Self::NoSuchTask => Err(reverie::Errno::ESRCH),
            Self::Task(task) => Ok(task.allowed(cpus)),
        }
    }

    /// The CPU the task runs on ([`TaskCpus::current`]); ESRCH where there
    /// is no such task.
    pub fn current_cpu(self, cpus: u32) -> Result<u32, reverie::Errno> {
        match self {
            Self::NoSuchTask => Err(reverie::Errno::ESRCH),
            Self::Task(task) => Ok(task.current(cpus)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_and_only_name_the_cpus_they_say() {
        assert_eq!(CpuSet::first(1), CpuSet::only(0));
        assert_eq!(CpuSet::first(4).len(), 4);
        assert_eq!(CpuSet::first(128).len(), 128);
        assert!(CpuSet::first(0).is_empty());
        assert!(CpuSet::only(128).is_empty());
    }

    /// Bytes past the mask are ignored and a short mask names no CPU past
    /// its end, as `get_user_cpu_mask` reads a user mask.
    #[test]
    fn a_mask_reads_as_the_kernel_copies_it() {
        assert_eq!(CpuSet::from_mask_bytes(&[0b1010]).list(), "1,3");
        assert!(CpuSet::from_mask_bytes(&[]).is_empty());
        let mut long = [0xffu8; 32];
        long[0] = 0b1;
        assert_eq!(CpuSet::from_mask_bytes(&long).len(), 121);
        assert_eq!(
            CpuSet::from_mask_bytes(&CpuSet::first(9).to_mask_bytes()),
            CpuSet::first(9)
        );
    }

    /// A task runs on CPU home mod N while its mask allows it, and
    /// otherwise on its home's place among the CPUs it may run on.
    #[test]
    fn a_task_runs_on_its_home_among_its_cpus() {
        let task = |mask, home| TaskAffinity::Task(TaskCpus { mask, home });
        assert_eq!(task(None, 0).current_cpu(4), Ok(0));
        assert_eq!(task(None, 6).current_cpu(4), Ok(2));
        let odd = CpuSet::from_mask_bytes(&[0b1010]);
        assert_eq!(task(Some(odd), 0).current_cpu(4), Ok(1));
        assert_eq!(task(Some(odd), 5).current_cpu(4), Ok(1));
        assert_eq!(task(Some(odd), 7).current_cpu(4), Ok(3));
        assert_eq!(task(Some(odd), 2).current_cpu(4), Ok(1));
        // A narrower mask that keeps the task's CPU leaves it there.
        let upper = CpuSet::from_mask_bytes(&[0b0110]);
        assert_eq!(task(Some(upper), 2).current_cpu(4), Ok(2));
        assert_eq!(task(Some(odd), 5).cpus(4), Ok(odd));
        assert_eq!(task(None, 9).current_cpu(1), Ok(0));
        assert_eq!(
            TaskAffinity::NoSuchTask.current_cpu(4),
            Err(reverie::Errno::ESRCH)
        );
    }

    #[test]
    fn nth_counts_from_the_lowest_cpu() {
        let set = CpuSet::from_mask_bytes(&[0b1011_0000]);
        assert_eq!(set.nth(0), Some(4));
        assert_eq!(set.nth(2), Some(7));
        assert_eq!(set.nth(3), None);
    }

    /// `%*pbl` and `%*pb` as lib/vsprintf.c's `bitmap_list_string` and
    /// `bitmap_string` print them.
    #[test]
    fn lists_and_masks_print_as_the_kernel_prints_them() {
        assert_eq!(CpuSet::first(1).list(), "0");
        assert_eq!(CpuSet::first(4).list(), "0-3");
        assert_eq!(CpuSet::from_mask_bytes(&[0b0110_1101]).list(), "0,2-3,5-6");
        assert_eq!(CpuSet::only(127).list(), "127");
        assert_eq!(CpuSet::first(1).mask(1), "1");
        assert_eq!(CpuSet::first(4).mask(4), "f");
        assert_eq!(CpuSet::only(2).mask(4), "4");
        assert_eq!(CpuSet::first(8).mask(8), "ff");
        assert_eq!(CpuSet::only(0).mask(40), "00,00000001");
        assert_eq!(CpuSet::first(32).mask(32), "ffffffff");
        assert_eq!(
            CpuSet::only(0).mask(128),
            "00000000,00000000,00000000,00000001"
        );
    }
}
