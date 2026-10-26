/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The few places Detcore reads a guest's registers by name.
//!
//! `libc::user_regs_struct` has different fields on each architecture, so every
//! read of a named register goes through here.

/// `fstatat`, which x86_64 numbers as `newfstatat`.
#[cfg(target_arch = "x86_64")]
pub(crate) const FSTATAT: reverie::syscalls::Sysno = reverie::syscalls::Sysno::newfstatat;
/// `fstatat`, which x86_64 numbers as `newfstatat`.
#[cfg(target_arch = "aarch64")]
pub(crate) const FSTATAT: reverie::syscalls::Sysno = reverie::syscalls::Sysno::fstatat;

/// The bytes below the stack pointer that a leaf function may use without
/// moving it: the x86-64 System V red zone, which Linux also skips when it
/// builds a signal frame (`get_sigframe` in `arch/x86/kernel/signal.c`).
#[cfg(target_arch = "x86_64")]
pub(crate) const STACK_RED_ZONE: usize = 128;
/// The bytes below the stack pointer that a leaf function may use without
/// moving it. The Linux AArch64 ABI has no red zone: the kernel builds a
/// signal frame directly below the interrupted stack pointer
/// (`get_sigframe` in `arch/arm64/kernel/signal.c`).
#[cfg(target_arch = "aarch64")]
pub(crate) const STACK_RED_ZONE: usize = 0;

/// The offset from the stack pointer at `rt_sigreturn` of the `uc_sigmask`
/// the kernel restores. x86-64 finds `frame->uc` at `sp` (`frame = sp -
/// sizeof(long)`), and `uc_sigmask` follows uc_flags (8), uc_link (8),
/// uc_stack (24) and uc_mcontext (256).
#[cfg(target_arch = "x86_64")]
pub(crate) const RT_SIGRETURN_SIGMASK_OFFSET: usize = 8 + 8 + 24 + 256;
/// The offset from the stack pointer at `rt_sigreturn` of the `uc_sigmask`
/// the kernel restores. arm64's frame at `sp` is `struct rt_sigframe { struct
/// siginfo info; struct ucontext uc; }`: `uc` follows the 128-byte siginfo,
/// and `uc_sigmask` follows uc_flags (8), uc_link (8) and uc_stack (24).
#[cfg(target_arch = "aarch64")]
pub(crate) const RT_SIGRETURN_SIGMASK_OFFSET: usize = 128 + 8 + 8 + 24;

/// The guest's program counter.
pub(crate) fn instruction_pointer(regs: &libc::user_regs_struct) -> u64 {
    #[cfg(target_arch = "x86_64")]
    return regs.rip;
    #[cfg(target_arch = "aarch64")]
    return regs.pc;
}

/// The guest's stack pointer.
pub(crate) fn stack_pointer(regs: &libc::user_regs_struct) -> u64 {
    #[cfg(target_arch = "x86_64")]
    return regs.rsp;
    #[cfg(target_arch = "aarch64")]
    return regs.sp;
}

/// The six syscall argument registers, in argument order.
pub(crate) fn syscall_arguments(regs: &libc::user_regs_struct) -> [u64; 6] {
    #[cfg(target_arch = "x86_64")]
    return [regs.rdi, regs.rsi, regs.rdx, regs.r10, regs.r8, regs.r9];
    #[cfg(target_arch = "aarch64")]
    return [
        regs.regs[0],
        regs.regs[1],
        regs.regs[2],
        regs.regs[3],
        regs.regs[4],
        regs.regs[5],
    ];
}

/// The syscall number of the syscall the guest is stopped in: `orig_rax` on
/// x86_64, and `x8` on aarch64, which `svc` leaves intact.
pub(crate) fn syscall_number(regs: &libc::user_regs_struct) -> u64 {
    #[cfg(target_arch = "x86_64")]
    return regs.orig_rax;
    #[cfg(target_arch = "aarch64")]
    return regs.regs[8];
}

/// `fstatat` as a [`reverie::syscalls::Syscall`]: x86_64 names its variant
/// `Newfstatat`.
pub(crate) fn fstatat_syscall(call: reverie::syscalls::Fstatat) -> reverie::syscalls::Syscall {
    #[cfg(target_arch = "x86_64")]
    return reverie::syscalls::Syscall::Newfstatat(call);
    #[cfg(target_arch = "aarch64")]
    return reverie::syscalls::Syscall::Fstatat(call);
}

/// The register words the `[registers]` detlog digests at a syscall's exit.
///
/// x86_64 leaves out `%rcx` and `%r11`, which the `syscall` instruction
/// overwrites with the return address and the flags.
#[cfg(target_arch = "x86_64")]
pub(crate) fn detlog_register_words(regs: &libc::user_regs_struct) -> Vec<u64> {
    vec![
        regs.rax,
        regs.rbx,
        regs.rdx,
        regs.rsi,
        regs.rdi,
        regs.rbp,
        regs.rsp,
        regs.r8,
        regs.r9,
        regs.r10,
        regs.r12,
        regs.r13,
        regs.r14,
        regs.r15,
        regs.rip,
        regs.eflags,
        regs.orig_rax,
        regs.fs_base,
        regs.gs_base,
    ]
}

/// The register words the `[registers]` detlog digests at a syscall's exit.
///
/// arm64 leaves out `x7`. At a ptrace syscall stop the kernel overwrites it
/// with 0 or 1 to mark entry or exit and restores it on resume
/// (`report_syscall`, arch/arm64/kernel/ptrace.c at v6.16), so the tracer reads
/// a stop marker there, not guest state, and a syscall that the kernel never
/// ran shows none.
#[cfg(target_arch = "aarch64")]
pub(crate) fn detlog_register_words(regs: &libc::user_regs_struct) -> Vec<u64> {
    const PTRACE_SYSCALL_MARKER: usize = 7;
    regs.regs
        .iter()
        .enumerate()
        .filter(|&(number, _)| number != PTRACE_SYSCALL_MARKER)
        .map(|(_, &word)| word)
        .chain([regs.sp, regs.pc, regs.pstate])
        .collect()
}

/// The registers a syscall's completion leaves, computed from `entry`, the
/// context at its entry (the program counter at the syscall instruction).
///
/// On x86_64, `rax` holds the result, `rip` follows the two-byte `syscall`
/// instruction, and that instruction's own clobbers hold `rcx` = the new `rip`
/// and `r11` = `rflags`.
#[cfg(target_arch = "x86_64")]
pub(crate) fn completed_syscall_projection(
    entry: libc::user_regs_struct,
    result: i64,
) -> libc::user_regs_struct {
    const SYSCALL_INSTRUCTION_LENGTH: u64 = 2;
    let mut completed = entry;
    completed.rax = result as u64;
    completed.rip = entry.rip.wrapping_add(SYSCALL_INSTRUCTION_LENGTH);
    completed.rcx = completed.rip;
    completed.r11 = entry.eflags;
    completed
}

/// The registers a syscall's completion leaves, computed from `entry`, the
/// context at its entry (the program counter at the syscall instruction).
///
/// On aarch64, `x0` holds the result and `pc` follows the four-byte `svc`
/// instruction, which clobbers nothing else.
#[cfg(target_arch = "aarch64")]
pub(crate) fn completed_syscall_projection(
    entry: libc::user_regs_struct,
    result: i64,
) -> libc::user_regs_struct {
    const SVC_INSTRUCTION_LENGTH: u64 = 4;
    let mut completed = entry;
    completed.regs[0] = result as u64;
    completed.pc = entry.pc.wrapping_add(SVC_INSTRUCTION_LENGTH);
    completed
}

/// Test fixtures' register writes, the inverses of the reads above: set the
/// guest's program counter.
#[cfg(test)]
pub(crate) fn set_instruction_pointer(regs: &mut libc::user_regs_struct, value: u64) {
    #[cfg(target_arch = "x86_64")]
    {
        regs.rip = value;
    }
    #[cfg(target_arch = "aarch64")]
    {
        regs.pc = value;
    }
}

/// Test fixtures' register writes: set the guest's stack pointer.
#[cfg(test)]
pub(crate) fn set_stack_pointer(regs: &mut libc::user_regs_struct, value: u64) {
    #[cfg(target_arch = "x86_64")]
    {
        regs.rsp = value;
    }
    #[cfg(target_arch = "aarch64")]
    {
        regs.sp = value;
    }
}

/// Test fixtures' register writes: set the number
/// [`syscall_number`] reads.
#[cfg(test)]
pub(crate) fn set_syscall_number(regs: &mut libc::user_regs_struct, value: u64) {
    #[cfg(target_arch = "x86_64")]
    {
        regs.orig_rax = value;
    }
    #[cfg(target_arch = "aarch64")]
    {
        regs.regs[8] = value;
    }
}

/// Test fixtures' register writes: set the leading syscall argument
/// registers [`syscall_arguments`] reads, in argument order.
#[cfg(test)]
pub(crate) fn set_syscall_arguments(regs: &mut libc::user_regs_struct, arguments: &[u64]) {
    assert!(
        arguments.len() <= 6,
        "a syscall takes at most six arguments"
    );
    for (index, &value) in arguments.iter().enumerate() {
        #[cfg(target_arch = "x86_64")]
        match index {
            0 => regs.rdi = value,
            1 => regs.rsi = value,
            2 => regs.rdx = value,
            3 => regs.r10 = value,
            4 => regs.r8 = value,
            _ => regs.r9 = value,
        }
        #[cfg(target_arch = "aarch64")]
        {
            regs.regs[index] = value;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fixtures' register writes land where Detcore's reads look.
    #[test]
    fn register_writes_are_read_back_by_name() {
        // SAFETY: user_regs_struct is plain integers; all zeroes is valid.
        let mut regs: libc::user_regs_struct = unsafe { std::mem::zeroed() };
        set_instruction_pointer(&mut regs, 0x40_1000);
        set_stack_pointer(&mut regs, 0x7ffd_0000);
        set_syscall_number(&mut regs, 56);
        set_syscall_arguments(&mut regs, &[1, 2, 3, 4, 5, 6]);
        assert_eq!(instruction_pointer(&regs), 0x40_1000);
        assert_eq!(stack_pointer(&regs), 0x7ffd_0000);
        assert_eq!(syscall_number(&regs), 56);
        assert_eq!(syscall_arguments(&regs), [1, 2, 3, 4, 5, 6]);
    }

    /// Signal phase 1: the signal pipeline at a syscall's completion reads
    /// the result in rax, rip after the two-byte syscall instruction, and the
    /// instruction's clobbers; every other register is the entry's.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn a_completed_syscall_shows_its_result_and_return_address() {
        let mut entry: libc::user_regs_struct = unsafe { std::mem::zeroed() };
        entry.rip = 0x40_1000;
        entry.rax = libc::SYS_pause as u64;
        entry.rcx = 0x1111;
        entry.r11 = 0x2222;
        entry.eflags = 0x246;
        entry.rdi = 7;
        entry.rsp = 0x7ffd_0000;
        let completed = completed_syscall_projection(entry, -i64::from(libc::EINTR));
        assert_eq!(completed.rax, (-i64::from(libc::EINTR)) as u64);
        assert_eq!(completed.rip, 0x40_1002);
        assert_eq!(completed.rcx, 0x40_1002);
        assert_eq!(completed.r11, 0x246);
        assert_eq!(completed.rdi, 7);
        assert_eq!(completed.rsp, 0x7ffd_0000);
        assert_eq!(completed.eflags, 0x246);
    }

    /// The aarch64 projection: the result in x0 and pc after the four-byte
    /// `svc`; every other register, x8's syscall number included, is the
    /// entry's.
    #[cfg(target_arch = "aarch64")]
    #[test]
    fn a_completed_syscall_shows_its_result_and_return_address() {
        let mut entry: libc::user_regs_struct = unsafe { std::mem::zeroed() };
        entry.pc = 0x40_1000;
        entry.regs[0] = 7;
        entry.regs[1] = 9;
        entry.regs[8] = libc::SYS_ppoll as u64;
        entry.sp = 0x7ffd_0000;
        entry.pstate = 0x6000_0000;
        let completed = completed_syscall_projection(entry, -i64::from(libc::EINTR));
        assert_eq!(completed.regs[0], (-i64::from(libc::EINTR)) as u64);
        assert_eq!(completed.pc, 0x40_1004);
        assert_eq!(completed.regs[1], 9);
        assert_eq!(completed.regs[8], libc::SYS_ppoll as u64);
        assert_eq!(completed.sp, 0x7ffd_0000);
        assert_eq!(completed.pstate, 0x6000_0000);
    }
}
