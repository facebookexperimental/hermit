/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Read-only observations for the explicit test-configured runtime cdylib.
//!
//! This module and its call sites are absent from non-test builds. The fixed
//! addresses belong to tests/fixtures/first_instruction, not to a runtime knob.

use std::ffi::c_void;

use reverie::syscalls::Addr;
use reverie::syscalls::MemoryAccess;
use reverie_dbt::RegisterReader;

pub(super) fn observe(event: &str, context: *mut c_void, reader: RegisterReader) {
    let memory = reverie_dbt::DbtMemory::new();
    let mut marker = [0_u8; 4];
    let marker_read = memory
        .read(Addr::from_raw(0x402000_usize).unwrap(), &mut marker)
        .map_err(|error| error.into_raw());
    let mut saved_sp = [0_u8; 8];
    let saved_sp_read = memory
        .read(Addr::from_raw(0x402008_usize).unwrap(), &mut saved_sp)
        .map_err(|error| error.into_raw());
    let mut saved_pc = [0_u8; 8];
    let saved_pc_read = memory
        .read(Addr::from_raw(0x402010_usize).unwrap(), &mut saved_pc)
        .map_err(|error| error.into_raw());
    let mut regs = std::mem::MaybeUninit::<libc::user_regs_struct>::zeroed();
    // The live callback owns this context and the writable observation buffer.
    let register_status = unsafe { reader(context as usize, regs.as_mut_ptr()) };
    let regs = unsafe { regs.assume_init() };
    let record = serde_json::json!({
        "event": event,
        "runtime": "Detcore",
        "marker_read": marker_read,
        "marker": u32::from_le_bytes(marker),
        "saved_sp_read": saved_sp_read,
        "saved_sp": u64::from_le_bytes(saved_sp),
        "saved_pc_read": saved_pc_read,
        "saved_pc": u64::from_le_bytes(saved_pc),
        "register_status": register_status,
        "registers": [regs.r15, regs.r14, regs.r13, regs.r12, regs.rbp,
            regs.rbx, regs.r11, regs.r10, regs.r9, regs.r8, regs.rax, regs.rcx,
            regs.rdx, regs.rsi, regs.rdi, regs.orig_rax, regs.rip, regs.cs,
            regs.eflags, regs.rsp, regs.ss, regs.fs_base, regs.gs_base,
            regs.ds, regs.es, regs.fs, regs.gs]
    });
    let line = format!("FIRST_INSTRUCTION_TEST {record}\n");
    // A missing channel means missing mandatory rows, never a stdout fallback.
    if let Some(emit) = super::DBT_DIAGNOSTIC_EMITTER.get() {
        let bytes = if line.len() < 4096 {
            line.as_bytes()
        } else {
            b"FIRST_INSTRUCTION_TEST oversized-record\n"
        };
        unsafe { emit(bytes.as_ptr(), bytes.len()) };
    }
}
