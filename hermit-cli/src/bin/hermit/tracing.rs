/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::fs::File;
use std::io;
use std::io::stderr;
use std::io::IsTerminal;

use tracing::metadata::LevelFilter;
use tracing::Subscriber;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

const DEFAULT_TRACE_LEVEL: LevelFilter = LevelFilter::WARN;

/// Returns a non-blocking subscriber for logging to a file.
///
/// NOTE: Writes to `f` are unbuffered, so this may be slow.
fn file_subscriber(level: LevelFilter, f: File) -> (impl Subscriber, impl Drop) {
    let filter = EnvFilter::from_default_env()
        .add_directive("tokio=debug".parse().expect("correct directive"))
        .add_directive(level.into());

    let (writer, guard) = tracing_appender::non_blocking(f);

    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(writer)
        .with_ansi(false)
        .finish();

    (subscriber, guard)
}

/// Initializes tracing to the given file `f`.
///
/// NOTE: Writes to `f` are unbuffered, so this may be slow.
#[must_use = "This function returns a guard that should not be immediately dropped"]
pub fn init_file_tracing(level: Option<LevelFilter>, f: File) -> impl Drop {
    let level = level.unwrap_or(DEFAULT_TRACE_LEVEL);

    let (subscriber, guard) = file_subscriber(level, f);

    subscriber
        .try_init()
        .expect("global tracing subscriber to install");

    guard
}

/// Returns a tracing subscriber that logs to `stderr`.
///
/// NOTE: Writes to stderr are unbuffered, so this may be slow.
pub fn stderr_subscriber(level: Option<LevelFilter>) -> impl Subscriber {
    let level = level.unwrap_or(DEFAULT_TRACE_LEVEL);

    let filter = EnvFilter::from_default_env()
        .add_directive("tokio=debug".parse().expect("correct directive"))
        .add_directive(level.into());
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(io::stderr)
        .with_ansi(stderr().is_terminal())
        .finish()
}

/// Initializes tracing to `stderr`.
///
/// NOTE: Writes to stderr are unbuffered, so this may be slow.
pub fn init_stderr_tracing(level: Option<LevelFilter>) {
    // Create an extra, pointless thread just so that our thread number starts at the same DetTid
    // "3" that the `init_file_tracing` option does.
    std::thread::spawn(|| {}).join().unwrap();

    stderr_subscriber(level)
        .try_init()
        .expect("global tracing subscriber to install")
}
