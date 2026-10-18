/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The record/replay event format and the support code that both Hermit's
//! recording tool (the `hermit-record` crate) and the `hermit` library use:
//! the recorded events and their stream, the recording's file names, the
//! directory-relative path helpers, guest file-descriptor helpers, ELF
//! interpreter discovery and shebang parsing. Nothing here is generic over a
//! backend or a tool, and nothing here models Linux syscall semantics.

pub mod consts;
pub mod event;
pub mod event_stream;
pub mod fd;
pub mod interp;
pub mod record_replay_path;
pub mod script;
pub mod syscall_args;

pub use script::Shebang;
