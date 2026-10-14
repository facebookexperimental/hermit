/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The probe's measured windows, written in assembly
//! (<https://github.com/rrnewton/hermit/issues/4243>).
//!
//! Each arm runs from "every register of every enabled component holds a known
//! value" through "the case's initializing step" to "XSAVE of every enabled
//! component" inside one assembly function. No compiler-generated code runs in
//! that window, so nothing Rust emits (a `memcpy` using SSE, say) can change the
//! state being measured. The Rust driver prepares the buffers before a call and
//! reads the certificates (interrupt counter, `getrusage`, schedstat) outside it.

use std::arch::global_asm;

/// What an arm does after its initializing step and before its XSAVE.
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Nothing: the reference.
    Reference = 0,
    /// Hand off to the peer on the same CPU and wait for its reply, so the
    /// kernel saves this thread's FPU state, runs the peer's, and reloads ours.
    Reload = 1,
    /// Execute CPUID, which exits to a hypervisor when there is one.
    ForcedExit = 2,
}

/// The initializing step of a probe case. The numbering is shared with the
/// assembly dispatch below.
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Standard-form XRSTOR of one component with its header bit clear.
    XrstorInit = 0,
    /// Standard-form XRSTOR of one component with its header bit set and
    /// initial values in the image.
    XrstorHeaderSet = 1,
    /// WRPKRU 0.
    WrpkruZero = 2,
    /// WRPKRU 0, then a signal to this thread, whose return restores the frame.
    Sigreturn = 3,
    /// VZEROUPPER with XMM live.
    Vzeroupper = 4,
    /// Legacy PXOR of XMM0-15 with the YMM upper halves live.
    LegacyPxor = 5,
    /// VEX-128 VPXOR of XMM0-15 with Hi16_ZMM live.
    Vex128Zero = 6,
    /// VPXORD of ZMM16-31 alone.
    Hi16Zero = 7,
    /// PXOR of XMM0-15, then VZEROUPPER, then ZMM16-31 zeroed.
    PxorVzeroupperHi16 = 8,
    /// FNINIT, FLDZ ×8, FNINIT: every ST register zero.
    X87FullInit = 9,
    /// Two more pushes first, then the same full x87 initialization.
    X87DirtyFullInit = 10,
    /// Compacted-form XRSTOR of one component with its header bit clear
    /// (glibc's lazy-binding `_dl_runtime_resolve_xsavec` path).
    CompactedXrstorInit = 11,
    /// FXRSTOR of initial x87 and SSE values.
    FxrstorInit = 12,
    /// KXORQ of k0-7 with the vectors live.
    OpmaskZero = 13,
    /// VZEROALL, plus ZMM16-31 zeroed where AVX-512 is enabled.
    VzeroallAll = 14,
}

/// The state of one arm call, shared with the assembly. Field offsets are fixed
/// by the assembly below; the test `arm_ctx_layout_matches_the_assembly` checks them.
#[repr(C)]
#[derive(Debug)]
pub struct ArmCtx {
    pub kind: u32,          // 0
    pub component: u32,     // 4
    pub mxcsr: u32,         // 8
    pub mode: u32,          // 12
    pub xcr0: u64,          // 16
    pub save_area: *mut u8, // 24: XSAVE destination, 64-byte aligned
    pub image: *const u8,   // 32: XRSTOR/FXRSTOR source, 64-byte aligned
    pub fd_to_peer: i32,    // 40
    pub fd_from_peer: i32,  // 44
    pub tgid: i32,          // 48
    pub tid: i32,           // 52
    pub ones: *const u8,    // 56: 64 nonzero bytes, 64-byte aligned
    pub pkru_out: u32,      // 64
    pub avx512: u32,        // 68: whether ZMM16-31 and k0-7 exist
    pub cpuid_cycles: u64,  // 72: the forced exit's CPUID round trip
    pub seq: u8,            // 80: handshake byte sent
    pub reply: u8,          // 81: handshake byte received
    pub _pad: [u8; 2],      // 82
    pub status: u32,        // 84: an `ArmStatus`
    pub saved_pkru: u32,    // 88: scratch
    pub saved_mxcsr: u32,   // 92: scratch
}

/// How an arm call ended. Anything but `Ok` voids the trial.
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArmStatus {
    Ok = 0,
    WriteFailed = 1,
    ReadFailed = 2,
    WrongReply = 3,
    TgkillFailed = 4,
}

impl ArmStatus {
    pub fn from_raw(raw: u32) -> Option<Self> {
        Some(match raw {
            0 => ArmStatus::Ok,
            1 => ArmStatus::WriteFailed,
            2 => ArmStatus::ReadFailed,
            3 => ArmStatus::WrongReply,
            4 => ArmStatus::TgkillFailed,
            _ => return None,
        })
    }
}

/// The state the peer leaves on the shared CPU before it replies.
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PeerState {
    /// Every register dirty, PKRU `0x55555550` (differs from the probe's).
    DirtyPkruDiffers = 0,
    /// Every register dirty, PKRU 0 by WRPKRU.
    DirtyWrpkruZero = 1,
    /// Every component initialized by a zero-header XRSTOR (PKRU 0 included).
    XrstorInitAll = 2,
    /// Every register dirty, PKRU 0 by a header-loaded XRSTOR (bit 9 set).
    DirtyPkruHeaderLoaded = 3,
    /// Every register dirty, PKRU 0, then a signal and its return.
    DirtyPkruSigreturn = 4,
    /// Initial values with every header bit set.
    InitValuesAllHeaderBits = 5,
}

/// The peer thread's state, shared with the assembly; offsets as noted.
#[repr(C)]
#[derive(Debug)]
pub struct PeerCtx {
    pub state: u32,        // 0: a `PeerState`, set before each handshake
    pub fd_in: i32,        // 4
    pub fd_out: i32,       // 8
    pub avx512: u32,       // 12
    pub image: *const u8,  // 16: the XRSTOR source for states 2, 3 and 5
    pub xcr0: u64,         // 24
    pub ones: *const u8,   // 32
    pub tgid: i32,         // 40
    pub tid: i32,          // 44
    pub expected_cpu: u32, // 48
    pub wrong_cpu: u32,    // 52: rounds the peer ran on another CPU
    pub status: u32,       // 56: why the loop ended early, as `ArmStatus`
    pub rounds: u32,       // 60
    pub byte: u8,          // 64: received
    pub reply: u8,         // 65: sent
    pub _pad: [u8; 2],     // 66
    pub cpu_out: u32,      // 68: getcpu result
    pub saved_pkru: u32,   // 72
    pub saved_mxcsr: u32,  // 76
}

/// The handshake byte that ends the peer loop.
pub const PEER_EXIT: u8 = 0xff;

unsafe extern "C" {
    /// Runs one arm and returns its `ArmStatus`. The save area receives the
    /// XSAVE image and `pkru_out` the RDPKRU value.
    pub fn hermit_xinuse_arm(ctx: *mut ArmCtx) -> u32;
    /// Serves handshakes until it reads [`PEER_EXIT`] or a call fails.
    pub fn hermit_xinuse_peer_loop(ctx: *mut PeerCtx) -> u32;
    /// The TSC cycles of one plain XSAVE plus standard XRSTOR of `xcr0`.
    pub fn hermit_xinuse_save_restore_cycles(area: *mut u8, xcr0: u64) -> u64;
    /// The TSC cycles of one CPUID leaf 0.
    pub fn hermit_xinuse_cpuid_cycles() -> u64;
    /// The TSC cycles of one `getppid` round trip.
    pub fn hermit_xinuse_syscall_cycles() -> u64;
}

// Register-state macros. DIRTY sets every register of every enabled component to
// a known nonzero value and leaves the x87 stack with eight entries.
global_asm!(
    r#"
.macro XINUSE_DIRTY ctxreg, avxoff, onesoff
    mov rsi, [\ctxreg + \onesoff]
    cmp dword ptr [\ctxreg + \avxoff], 0
    je 2f
    .irp r,0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31
    vmovdqu32 zmm\r, [rsi]
    .endr
    .irp k,1,2,3,4,5,6,7
    kxnorq k\k, k\k, k\k
    .endr
    kxorq k0, k0, k0
    jmp 3f
2:
    .irp r,0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15
    vmovdqu ymm\r, [rsi]
    .endr
3:
    fninit
    .rept 6
    fld1
    .endr
    fldpi
    fldl2e
.endm

.macro XINUSE_WRPKRU value
    mov eax, \value
    xor ecx, ecx
    xor edx, edx
    wrpkru
.endm

.macro XINUSE_XRSTOR_MASK ctxreg, imgoff, maskreg
    mov rax, \maskreg
    mov rdx, rax
    shr rdx, 32
    mov r8, [\ctxreg + \imgoff]
    xrstor [r8]
.endm
"#
);

global_asm!(
    r#"
    .text
    .globl hermit_xinuse_arm
    .type hermit_xinuse_arm,@function
hermit_xinuse_arm:
    push rbx
    push r12
    push r13
    mov r12, rdi
    xor ecx, ecx
    rdpkru
    mov [r12 + 88], eax
    stmxcsr dword ptr [r12 + 92]
    XINUSE_WRPKRU 0x55555554
    XINUSE_DIRTY r12, 68, 56
    ldmxcsr dword ptr [r12 + 8]

    mov eax, [r12 + 0]
    cmp eax, 0
    je .Larm_xrstor
    cmp eax, 1
    je .Larm_xrstor
    cmp eax, 11
    je .Larm_xrstor
    cmp eax, 2
    je .Larm_wrpkru
    cmp eax, 3
    je .Larm_sigreturn
    cmp eax, 4
    je .Larm_vzeroupper
    cmp eax, 5
    je .Larm_legacy_pxor
    cmp eax, 6
    je .Larm_vex128
    cmp eax, 7
    je .Larm_hi16
    cmp eax, 8
    je .Larm_mix
    cmp eax, 9
    je .Larm_x87_full
    cmp eax, 10
    je .Larm_x87_dirty_full
    cmp eax, 12
    je .Larm_fxrstor
    cmp eax, 13
    je .Larm_opmask
    cmp eax, 14
    je .Larm_vzeroall
    jmp .Larm_mode

.Larm_xrstor:
    mov ecx, [r12 + 4]
    mov r13, 1
    shl r13, cl
    XINUSE_XRSTOR_MASK r12, 32, r13
    jmp .Larm_mode
.Larm_wrpkru:
    XINUSE_WRPKRU 0
    jmp .Larm_mode
.Larm_sigreturn:
    XINUSE_WRPKRU 0
    mov eax, 234
    movsxd rdi, dword ptr [r12 + 48]
    movsxd rsi, dword ptr [r12 + 52]
    mov edx, 10
    syscall
    test rax, rax
    jz .Larm_mode
    mov dword ptr [r12 + 84], 4
    jmp .Larm_finish
.Larm_vzeroupper:
    vzeroupper
    jmp .Larm_mode
.Larm_legacy_pxor:
    .irp r,0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15
    pxor xmm\r, xmm\r
    .endr
    jmp .Larm_mode
.Larm_vex128:
    .irp r,0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15
    vpxor xmm\r, xmm\r, xmm\r
    .endr
    jmp .Larm_mode
.Larm_hi16:
    .irp r,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31
    vpxord zmm\r, zmm\r, zmm\r
    .endr
    jmp .Larm_mode
.Larm_mix:
    .irp r,0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15
    pxor xmm\r, xmm\r
    .endr
    vzeroupper
    .irp r,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31
    vpxord zmm\r, zmm\r, zmm\r
    .endr
    jmp .Larm_mode
.Larm_x87_dirty_full:
    fld1
    fldpi
.Larm_x87_full:
    fninit
    .rept 8
    fldz
    .endr
    fninit
    jmp .Larm_mode
.Larm_fxrstor:
    mov r8, [r12 + 32]
    fxrstor [r8]
    jmp .Larm_mode
.Larm_opmask:
    .irp k,0,1,2,3,4,5,6,7
    kxorq k\k, k\k, k\k
    .endr
    jmp .Larm_mode
.Larm_vzeroall:
    vzeroall
    cmp dword ptr [r12 + 68], 0
    je .Larm_mode
    .irp r,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31
    vpxord zmm\r, zmm\r, zmm\r
    .endr

.Larm_mode:
    mov eax, [r12 + 12]
    cmp eax, 1
    je .Larm_reload
    cmp eax, 2
    je .Larm_exit
    jmp .Larm_save
.Larm_reload:
    mov eax, 1
    movsxd rdi, dword ptr [r12 + 40]
    lea rsi, [r12 + 80]
    mov edx, 1
    syscall
    cmp rax, 1
    je 4f
    mov dword ptr [r12 + 84], 1
    jmp .Larm_finish
4:
    xor eax, eax
    movsxd rdi, dword ptr [r12 + 44]
    lea rsi, [r12 + 81]
    mov edx, 1
    syscall
    cmp rax, 1
    je 5f
    mov dword ptr [r12 + 84], 2
    jmp .Larm_finish
5:
    movzx eax, byte ptr [r12 + 80]
    inc al
    cmp al, byte ptr [r12 + 81]
    je .Larm_save
    mov dword ptr [r12 + 84], 3
    jmp .Larm_finish
.Larm_exit:
    rdtscp
    shl rdx, 32
    or rax, rdx
    mov r13, rax
    xor eax, eax
    xor ecx, ecx
    cpuid
    rdtscp
    shl rdx, 32
    or rax, rdx
    sub rax, r13
    mov [r12 + 72], rax

.Larm_save:
    mov rax, [r12 + 16]
    mov rdx, rax
    shr rdx, 32
    mov r8, [r12 + 24]
    xsave [r8]
    xor ecx, ecx
    rdpkru
    mov [r12 + 64], eax

.Larm_finish:
    mov eax, [r12 + 88]
    xor ecx, ecx
    xor edx, edx
    wrpkru
    ldmxcsr dword ptr [r12 + 92]
    fninit
    vzeroupper
    mov eax, [r12 + 84]
    pop r13
    pop r12
    pop rbx
    ret
    .size hermit_xinuse_arm, .-hermit_xinuse_arm

    .globl hermit_xinuse_peer_loop
    .type hermit_xinuse_peer_loop,@function
hermit_xinuse_peer_loop:
    push rbx
    push r12
    push r13
    mov r12, rdi
    xor ecx, ecx
    rdpkru
    mov [r12 + 72], eax
    stmxcsr dword ptr [r12 + 76]
.Lpeer_round:
    xor eax, eax
    movsxd rdi, dword ptr [r12 + 4]
    lea rsi, [r12 + 64]
    mov edx, 1
    syscall
    cmp rax, 1
    je 6f
    mov dword ptr [r12 + 56], 2
    jmp .Lpeer_finish
6:
    cmp byte ptr [r12 + 64], 0xff
    je .Lpeer_finish

    mov eax, [r12 + 0]
    cmp eax, 0
    je .Lpeer_dirty_differs
    cmp eax, 1
    je .Lpeer_dirty_wrpkru
    cmp eax, 2
    je .Lpeer_xrstor_all
    cmp eax, 3
    je .Lpeer_header_loaded
    cmp eax, 4
    je .Lpeer_sigreturn
    jmp .Lpeer_init_values

.Lpeer_dirty_differs:
    XINUSE_DIRTY r12, 12, 32
    XINUSE_WRPKRU 0x55555550
    jmp .Lpeer_reply
.Lpeer_dirty_wrpkru:
    XINUSE_DIRTY r12, 12, 32
    XINUSE_WRPKRU 0
    jmp .Lpeer_reply
.Lpeer_xrstor_all:
    XINUSE_WRPKRU 0x55555550
    mov r13, [r12 + 24]
    XINUSE_XRSTOR_MASK r12, 16, r13
    jmp .Lpeer_reply
.Lpeer_header_loaded:
    XINUSE_DIRTY r12, 12, 32
    mov r13, 0x200
    XINUSE_XRSTOR_MASK r12, 16, r13
    jmp .Lpeer_reply
.Lpeer_sigreturn:
    XINUSE_DIRTY r12, 12, 32
    XINUSE_WRPKRU 0
    mov eax, 234
    movsxd rdi, dword ptr [r12 + 40]
    movsxd rsi, dword ptr [r12 + 44]
    mov edx, 10
    syscall
    test rax, rax
    jz .Lpeer_reply
    mov dword ptr [r12 + 56], 4
    jmp .Lpeer_finish
.Lpeer_init_values:
    mov r13, [r12 + 24]
    XINUSE_XRSTOR_MASK r12, 16, r13

.Lpeer_reply:
    mov eax, 309
    lea rdi, [r12 + 68]
    xor esi, esi
    xor edx, edx
    syscall
    mov eax, [r12 + 68]
    cmp eax, [r12 + 48]
    je 7f
    inc dword ptr [r12 + 52]
7:
    inc dword ptr [r12 + 60]
    movzx eax, byte ptr [r12 + 64]
    inc al
    mov [r12 + 65], al
    mov eax, 1
    movsxd rdi, dword ptr [r12 + 8]
    lea rsi, [r12 + 65]
    mov edx, 1
    syscall
    cmp rax, 1
    je .Lpeer_round
    mov dword ptr [r12 + 56], 1

.Lpeer_finish:
    mov eax, [r12 + 72]
    xor ecx, ecx
    xor edx, edx
    wrpkru
    ldmxcsr dword ptr [r12 + 76]
    fninit
    vzeroupper
    mov eax, [r12 + 56]
    pop r13
    pop r12
    pop rbx
    ret
    .size hermit_xinuse_peer_loop, .-hermit_xinuse_peer_loop

    .globl hermit_xinuse_save_restore_cycles
    .type hermit_xinuse_save_restore_cycles,@function
hermit_xinuse_save_restore_cycles:
    push rbx
    mov r8, rdi
    mov r9, rsi
    rdtscp
    shl rdx, 32
    or rax, rdx
    mov r10, rax
    mov rax, r9
    mov rdx, r9
    shr rdx, 32
    xsave [r8]
    mov rax, r9
    mov rdx, r9
    shr rdx, 32
    xrstor [r8]
    rdtscp
    shl rdx, 32
    or rax, rdx
    sub rax, r10
    vzeroupper
    pop rbx
    ret
    .size hermit_xinuse_save_restore_cycles, .-hermit_xinuse_save_restore_cycles

    .globl hermit_xinuse_cpuid_cycles
    .type hermit_xinuse_cpuid_cycles,@function
hermit_xinuse_cpuid_cycles:
    push rbx
    rdtscp
    shl rdx, 32
    or rax, rdx
    mov r8, rax
    xor eax, eax
    xor ecx, ecx
    cpuid
    rdtscp
    shl rdx, 32
    or rax, rdx
    sub rax, r8
    pop rbx
    ret
    .size hermit_xinuse_cpuid_cycles, .-hermit_xinuse_cpuid_cycles

    .globl hermit_xinuse_syscall_cycles
    .type hermit_xinuse_syscall_cycles,@function
hermit_xinuse_syscall_cycles:
    rdtscp
    shl rdx, 32
    or rax, rdx
    mov r8, rax
    mov eax, 110
    syscall
    rdtscp
    shl rdx, 32
    or rax, rdx
    sub rax, r8
    ret
    .size hermit_xinuse_syscall_cycles, .-hermit_xinuse_syscall_cycles
"#
);

#[cfg(test)]
mod tests {
    use std::mem::offset_of;

    use super::*;

    #[test]
    fn arm_ctx_layout_matches_the_assembly() {
        assert_eq!(offset_of!(ArmCtx, kind), 0);
        assert_eq!(offset_of!(ArmCtx, component), 4);
        assert_eq!(offset_of!(ArmCtx, mxcsr), 8);
        assert_eq!(offset_of!(ArmCtx, mode), 12);
        assert_eq!(offset_of!(ArmCtx, xcr0), 16);
        assert_eq!(offset_of!(ArmCtx, save_area), 24);
        assert_eq!(offset_of!(ArmCtx, image), 32);
        assert_eq!(offset_of!(ArmCtx, fd_to_peer), 40);
        assert_eq!(offset_of!(ArmCtx, fd_from_peer), 44);
        assert_eq!(offset_of!(ArmCtx, tgid), 48);
        assert_eq!(offset_of!(ArmCtx, tid), 52);
        assert_eq!(offset_of!(ArmCtx, ones), 56);
        assert_eq!(offset_of!(ArmCtx, pkru_out), 64);
        assert_eq!(offset_of!(ArmCtx, avx512), 68);
        assert_eq!(offset_of!(ArmCtx, cpuid_cycles), 72);
        assert_eq!(offset_of!(ArmCtx, seq), 80);
        assert_eq!(offset_of!(ArmCtx, reply), 81);
        assert_eq!(offset_of!(ArmCtx, status), 84);
        assert_eq!(offset_of!(ArmCtx, saved_pkru), 88);
        assert_eq!(offset_of!(ArmCtx, saved_mxcsr), 92);
    }

    #[test]
    fn peer_ctx_layout_matches_the_assembly() {
        assert_eq!(offset_of!(PeerCtx, state), 0);
        assert_eq!(offset_of!(PeerCtx, fd_in), 4);
        assert_eq!(offset_of!(PeerCtx, fd_out), 8);
        assert_eq!(offset_of!(PeerCtx, avx512), 12);
        assert_eq!(offset_of!(PeerCtx, image), 16);
        assert_eq!(offset_of!(PeerCtx, xcr0), 24);
        assert_eq!(offset_of!(PeerCtx, ones), 32);
        assert_eq!(offset_of!(PeerCtx, tgid), 40);
        assert_eq!(offset_of!(PeerCtx, tid), 44);
        assert_eq!(offset_of!(PeerCtx, expected_cpu), 48);
        assert_eq!(offset_of!(PeerCtx, wrong_cpu), 52);
        assert_eq!(offset_of!(PeerCtx, status), 56);
        assert_eq!(offset_of!(PeerCtx, rounds), 60);
        assert_eq!(offset_of!(PeerCtx, byte), 64);
        assert_eq!(offset_of!(PeerCtx, reply), 65);
        assert_eq!(offset_of!(PeerCtx, cpu_out), 68);
        assert_eq!(offset_of!(PeerCtx, saved_pkru), 72);
        assert_eq!(offset_of!(PeerCtx, saved_mxcsr), 76);
    }
}
