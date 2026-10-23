/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Hermit's ptrace recording tool, [`Recorder`], and the one place its
//! Detcore instantiation is rooted.
//!
//! `detcore::Detcore<Recorder>` drives `reverie_ptrace`'s `TracedTask`, and
//! that instantiation is large. Compiled inside the `hermit` library it shared
//! one LLVM codegen unit with every other backend's instantiation; in its own
//! crate it builds in a separate rustc process, beside the others. The entry
//! point, [`spawn_tracer`], is not generic, so `hermit` instantiates nothing
//! of it.

mod recorder;

use std::future::Future;
use std::pin::Pin;

use hermit_record_format::Shebang;
use hermit_record_format::consts;
use hermit_record_format::event;
use hermit_record_format::event_stream;
use hermit_record_format::fd;
use hermit_record_format::interp;
use hermit_record_format::record_replay_path;
use hermit_record_format::syscall_args::read_iovecs;
use hermit_record_format::syscall_args::vectored_offset;
pub use recorder::Recorder;
use reverie::process::Command;

/// The recording tracer: `detcore::Detcore<Recorder>` under `reverie_ptrace`.
pub type Tracer = reverie_ptrace::Tracer<detcore::GlobalState>;

/// The future [`spawn_tracer`] returns.
pub type SpawnTracer = Pin<Box<dyn Future<Output = Result<Tracer, reverie::Error>>>>;

/// Spawns COMMAND under the recording tool with CONFIG, as `hermit record`
/// does.
///
/// The future is boxed on purpose. rustc compiles an upstream crate's
/// coroutine (an `async fn` body) in the crate that polls it, so an
/// `async fn` here would make `hermit` instantiate the whole recording tool
/// again. Behind `dyn Future` the body, and everything it instantiates, is
/// compiled in this crate, and `hermit` only calls it through the vtable.
pub fn spawn_tracer(command: Command, config: detcore::Config) -> SpawnTracer {
    Box::pin(async move {
        reverie_ptrace::TracerBuilder::<detcore::Detcore<Recorder>>::new(command)
            .config(config)
            .spawn()
            .await
    })
}
