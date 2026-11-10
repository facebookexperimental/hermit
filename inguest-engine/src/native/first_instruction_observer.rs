/* SPDX-License-Identifier: BSD-3-Clause */
//! Explicit observer build for the real native first-instruction fixture.
//! Fixed addresses are fixture witnesses, never a runtime environment switch.
//! The common host supplies its actual admitted, read-only native callback.

use std::cell::Cell;

use reverie_inguest::guest::native_services::NativeEntryObservation;
use reverie_inguest::guest::native_services::NativeEntryStage;

std::thread_local! {
    // Diagnostic bookkeeping only, reset by the genuine thread-start boundary.
    static FIRST_SYSCALL_PENDING: Cell<bool> = const { Cell::new(false) };
}

fn read_result(result: Result<usize, i32>) -> String {
    match result {
        Ok(count) => format!("{{\"Ok\":{count}}}"),
        Err(error) => format!("{{\"Err\":{error}}}"),
    }
}

pub(super) fn observe(stage: NativeEntryStage, observation: NativeEntryObservation<'_, '_>) {
    if stage == NativeEntryStage::StartEnter {
        FIRST_SYSCALL_PENDING.with(|pending| pending.set(true));
    }
    if stage == NativeEntryStage::Syscall
        && !FIRST_SYSCALL_PENDING.with(|pending| pending.replace(false))
    {
        return;
    }
    let event = match stage {
        NativeEntryStage::StartEnter => "start-enter",
        NativeEntryStage::StartComplete => "start-complete",
        NativeEntryStage::PostExecEnter => "post-exec-enter",
        NativeEntryStage::PostExecComplete => "post-exec-complete",
        NativeEntryStage::Syscall => "first-syscall",
    };
    let mut marker = [0_u8; 4];
    let marker_read = observation
        .read_memory(0x402000, &mut marker)
        .map_err(|error| error.into_raw());
    let mut saved_sp = [0_u8; 8];
    let saved_sp_read = observation
        .read_memory(0x402008, &mut saved_sp)
        .map_err(|error| error.into_raw());
    let mut saved_pc = [0_u8; 8];
    let saved_pc_read = observation
        .read_memory(0x402010, &mut saved_pc)
        .map_err(|error| error.into_raw());
    // The record contains integers; the actual native reader supplies every field.
    let mut regs: libc::user_regs_struct = unsafe { std::mem::zeroed() };
    let register_status = observation.read_registers(&mut regs).unwrap_or(0);
    let registers = [
        regs.r15,
        regs.r14,
        regs.r13,
        regs.r12,
        regs.rbp,
        regs.rbx,
        regs.r11,
        regs.r10,
        regs.r9,
        regs.r8,
        regs.rax,
        regs.rcx,
        regs.rdx,
        regs.rsi,
        regs.rdi,
        regs.orig_rax,
        regs.rip,
        regs.cs,
        regs.eflags,
        regs.rsp,
        regs.ss,
        regs.fs_base,
        regs.gs_base,
        regs.ds,
        regs.es,
        regs.fs,
        regs.gs,
    ];
    // All strings here are fixed literals. Integer Debug arrays are JSON arrays.
    let line = format!(
        "FIRST_INSTRUCTION_TEST {{\"event\":\"{event}\",\"runtime\":\"Detcore\",\"marker_read\":{},\"marker\":{},\"saved_sp_read\":{},\"saved_sp\":{},\"saved_pc_read\":{},\"saved_pc\":{},\"register_status\":{register_status},\"registers\":{registers:?}}}\n",
        read_result(marker_read),
        u32::from_le_bytes(marker),
        read_result(saved_sp_read),
        u64::from_le_bytes(saved_sp),
        read_result(saved_pc_read),
        u64::from_le_bytes(saved_pc),
    );
    // Same actual protected diagnostic channel; missing rows fail the oracle.
    // No stdout fallback, unbounded record or fabricated successful read.
    if line.len() < 4096 {
        super::diagnostics::emit(line.as_bytes());
    } else {
        super::diagnostics::emit(b"FIRST_INSTRUCTION_TEST oversized-record\n");
    }
}
