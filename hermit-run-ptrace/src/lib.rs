/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Hermit's plain (neither recording nor replaying) ptrace run, and the one
//! place its Detcore instantiation is rooted.
//!
//! `detcore::Detcore` under `reverie_ptrace`'s `TracedTask` is a large
//! instantiation. In its own crate it builds in a separate rustc process,
//! beside the other backends' instantiations. The entry point,
//! [`spawn_tracer`], is not generic and returns a boxed future, so its callers
//! instantiate nothing of it (see `hermit-record` for why it is boxed).

use std::future::Future;
use std::pin::Pin;

use reverie::BackendStatsRequest;
use reverie::process::Command;

/// The plain ptrace tracer: `detcore::Detcore` under `reverie_ptrace`.
pub type Tracer = reverie_ptrace::Tracer<detcore::GlobalState>;

/// The future [`spawn_tracer`] returns.
pub type SpawnTracer = Pin<Box<dyn Future<Output = Result<Tracer, reverie::Error>>>>;

/// Spawns COMMAND under Detcore with CONFIG on the ptrace backend, collecting
/// backend statistics as STATS requests. When GDBSERVER names a port, a gdb
/// remote is served there; SEQUENTIALIZED_GUEST then tells gdbserver that
/// Detcore already sequentializes the guest's threads.
///
/// Without that, gdbserver freezes the other threads around a breakpoint stop
/// and waits for each to report, but under Detcore they are parked in its
/// scheduler rather than blocked in the kernel, so they cannot answer until
/// the scheduler runs them, which it cannot while the stopped thread holds its
/// turn. The replay path does the same.
///
/// The future is boxed so the async body, and everything it instantiates, is
/// compiled in this crate rather than in its caller.
pub fn spawn_tracer(
    command: Command,
    config: detcore::Config,
    gdbserver: Option<u16>,
    sequentialized_guest: bool,
    stats: BackendStatsRequest,
) -> SpawnTracer {
    Box::pin(async move {
        let mut builder =
            reverie_ptrace::TracerBuilder::<detcore::Detcore>::new(command).config(config);
        if let Some(port) = gdbserver {
            builder = builder.gdbserver(port);
            if sequentialized_guest {
                builder = builder.sequentialized_guest();
            }
        }
        builder.backend_stats(stats).spawn().await
    })
}
