/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Hermit's ptrace replay tool, [`Replayer`], and the one place its Detcore
//! instantiation is rooted.
//!
//! `detcore::Detcore<Replayer>` drives `reverie_ptrace`'s `TracedTask`, and
//! that instantiation is large. In its own crate it builds in a separate
//! rustc process, beside the other backends' instantiations. The entry point,
//! [`spawn_tracer`], is not generic and returns a boxed future, so `hermit`
//! instantiates nothing of it (see `hermit-record` for why it is boxed).

mod desync;
mod replayer;

use std::future::Future;
use std::pin::Pin;

use hermit_record_format::consts;
use hermit_record_format::event;
use hermit_record_format::event_stream;
use hermit_record_format::fd;
use hermit_record_format::record_replay_path;
use hermit_record_format::syscall_args::read_iovecs;
use hermit_record_format::syscall_args::vectored_offset;
pub use replayer::ReplayMaterializationScope;
pub use replayer::Replayer;
pub use replayer::registered_materialized_count;
pub use replayer::remember_materialized_object;
use reverie::process::Command;

/// The replay tracer: `detcore::Detcore<Replayer>` under `reverie_ptrace`.
pub type Tracer = reverie_ptrace::Tracer<detcore::GlobalState>;

/// The future [`spawn_tracer`] returns.
pub type SpawnTracer = Pin<Box<dyn Future<Output = Result<Tracer, reverie::Error>>>>;

/// Spawns COMMAND under the replay tool with CONFIG, as `hermit replay` does:
/// serving a gdb remote on GDBSERVER when given, and telling gdbserver the
/// guest is already sequentialized when SEQUENTIALIZED_GUEST.
///
/// The future is boxed so the async body, and everything it instantiates, is
/// compiled in this crate rather than in its caller.
pub fn spawn_tracer(
    command: Command,
    config: detcore::Config,
    gdbserver: Option<u16>,
    sequentialized_guest: bool,
) -> SpawnTracer {
    Box::pin(async move {
        let mut builder = reverie_ptrace::TracerBuilder::<detcore::Detcore<Replayer>>::new(command)
            .config(config);
        if let Some(port) = gdbserver {
            builder = builder.gdbserver(port);
        }
        if sequentialized_guest {
            builder = builder.sequentialized_guest();
        }
        builder.spawn().await
    })
}
