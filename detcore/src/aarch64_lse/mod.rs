/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Rewriting aarch64 load/store-exclusive retry loops to FEAT_LSE atomics.
//!
//! On aarch64 an `LDXR`/`STXR` pair retries whenever the local exclusive
//! monitor is cleared between the two, which any exception does: a timer tick,
//! an IPI, a page fault, a PMU overflow, a ptrace stop. Each retry retires the
//! loop's branches again, so the retired-branch count of identical guest work
//! depends on host interrupt timing, and Hermit's branch-counted schedule
//! cannot be replayed exactly. A single LSE atomic cannot retry, so a program
//! whose exclusive loops have been rewritten retires a fixed number of
//! branches for the same work.
//!
//! [`insn`] holds the decoder and the encoders; [`rewrite`] holds the idioms
//! and the in-place replacement; [`elf`] applies them to the executable
//! sections of an ELF file's bytes. All three are pure functions of their
//! input and compile on every architecture, so their tests run everywhere.
//! `hermit-cli` writes rewritten files with them; Detcore applies the same
//! rewrite to a guest's mapped pages.

pub mod elf;
pub mod insn;
pub mod rewrite;
