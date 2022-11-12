/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#[macro_use]
mod macros;

mod fs;
mod mmap;
mod network;
mod random;
mod time;

use std::path::PathBuf;

use reverie::syscalls::Syscall;
use reverie::Errno;
use reverie::Error;
use reverie::GlobalTool;
use reverie::Guest;
use reverie::Pid;
use reverie::Rdtsc;
use reverie::RdtscResult;
use reverie::Subscription;
use reverie::Tid;
use reverie::Tool;
use serde::Deserialize;
use serde::Serialize;

use crate::desync::DesyncError;
use crate::event_stream::DebugEvent;
use crate::event_stream::EventReader;

/// A Reverie tool that replays syscalls. Note that only syscalls that cannot be
/// made deterministic are forwarded to this tool.
#[derive(Default, Serialize, Deserialize)]
pub struct Replayer {
    // Keep track of the data directory. Each thread uses this path to open its
    // event stream.
    data: PathBuf,

    // Set to true if we should detect desynchronization.
    verify: bool,
}

#[reverie::tool]
impl Tool for Replayer {
    type GlobalState = detcore::GlobalState;
    type ThreadState = EventReader;

    fn new(_pid: Pid, cfg: &<Self::GlobalState as GlobalTool>::Config) -> Self {
        Self {
            data: cfg.replay_data.as_ref().unwrap().clone(),
            // TODO: Make this part of the configuration instead when global
            // state composition works.
            verify: std::env::var_os("HERMIT_VERIFY").is_some(),
        }
    }

    fn init_thread_state(
        &self,
        child: Tid,
        _parent: Option<(Tid, &Self::ThreadState)>,
    ) -> Self::ThreadState {
        // We have to unwrap because there is now way to handle errors here.
        EventReader::open(&self.data, child).unwrap_or_else(|err| {
            panic!(
                "Failed to open {:?} for thread {}: {}",
                self.data, child, err
            )
        })
    }

    fn subscriptions(config: &<Self::GlobalState as GlobalTool>::Config) -> Subscription {
        // Subscribe to the exact same events as the recorder does.
        crate::recorder::Recorder::subscriptions(config)
    }

    async fn handle_syscall_event<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
    ) -> Result<i64, Error> {
        if self.verify {
            self.expect_syscall(guest, syscall);
        }

        // NOTE: This match statement should be identical to the one in the
        // recorder. Otherwise, our recorder and replayer will disagree about
        // how syscalls should be handled.
        //
        // FIXME: Figure out a way to avoid duplicate code. (Merge record/replay
        // into a single tool?)
        Ok(match syscall {
            // We must let through execve without any modification. Recording
            // events for these is hard because execve only returns upon
            // failure.
            Syscall::Execve(_) => guest.inject(syscall).await,
            Syscall::Execveat(_) => guest.inject(syscall).await,
            Syscall::Brk(_) => self.let_through(guest, syscall).await,
            Syscall::Mprotect(_) => self.let_through(guest, syscall).await,
            Syscall::ArchPrctl(_) => {
                // To properly handle arch_prctl, we should prevent calls from
                // using ARCH_SET_CPUID since we already do that for the
                // tracees. However, it is rare for programs to use
                // ARCH_SET_CPUID. For all other arch_prctl subfunctions, we
                // should let it through.
                self.let_through(guest, syscall).await
            }
            Syscall::Read(syscall) => self.handle_read(guest, syscall).await,
            Syscall::Pread64(syscall) => self.handle_pread64(guest, syscall).await,
            Syscall::Recvfrom(syscall) => self.handle_recvfrom(guest, syscall).await,
            Syscall::Write(syscall) => self.handle_write_family(guest, syscall.into()).await,
            Syscall::Pwrite64(syscall) => self.handle_write_family(guest, syscall.into()).await,
            Syscall::Writev(syscall) => self.handle_write_family(guest, syscall.into()).await,
            Syscall::Pwritev(syscall) => self.handle_write_family(guest, syscall.into()).await,
            Syscall::Pwritev2(syscall) => self.handle_write_family(guest, syscall.into()).await,
            Syscall::Access(_) => self.handle_simple(guest, syscall).await,
            Syscall::Lseek(_) => self.handle_simple(guest, syscall).await,
            Syscall::Stat(syscall) => self.handle_stat_family(guest, syscall.into()).await,
            Syscall::Fstat(syscall) => self.handle_stat_family(guest, syscall.into()).await,
            Syscall::Lstat(syscall) => self.handle_stat_family(guest, syscall.into()).await,
            Syscall::Newfstatat(syscall) => self.handle_stat_family(guest, syscall.into()).await,
            Syscall::Statx(syscall) => self.handle_statx(guest, syscall).await,
            Syscall::Getdents(syscall) => self.handle_getdents(guest, syscall).await,
            Syscall::Getdents64(syscall) => self.handle_getdents64(guest, syscall).await,
            Syscall::Mmap(syscall) => self.handle_mmap(guest, syscall).await,
            Syscall::Munmap(_) => self.let_through(guest, syscall).await,
            Syscall::Open(_) => self.handle_simple(guest, syscall).await,
            Syscall::Openat(_) => self.handle_simple(guest, syscall).await,
            Syscall::Close(_) => self.handle_simple(guest, syscall).await,
            Syscall::Fadvise64(_) => self.handle_simple(guest, syscall).await,
            Syscall::Dup(_) => self.handle_simple(guest, syscall).await,
            Syscall::Dup2(_) => self.handle_simple(guest, syscall).await,
            Syscall::Dup3(_) => self.handle_simple(guest, syscall).await,
            Syscall::Ioctl(syscall) => self.handle_ioctl(guest, syscall).await,
            Syscall::Socket(_) => self.handle_simple(guest, syscall).await,
            Syscall::ClockGettime(syscall) => self.handle_clock_gettime(guest, syscall).await,
            Syscall::Gettimeofday(syscall) => self.handle_gettimeofday(guest, syscall).await,
            Syscall::Settimeofday(_) => self.handle_simple(guest, syscall).await,
            Syscall::Time(syscall) => self.handle_time(guest, syscall).await,
            Syscall::Setsockopt(_) => self.handle_simple(guest, syscall).await,
            // FIXME: Not all fcntl cases are simple.
            Syscall::Fcntl(_) => self.handle_simple(guest, syscall).await,
            Syscall::Connect(_) => self.handle_simple(guest, syscall).await,
            Syscall::Sendto(_) => self.handle_simple(guest, syscall).await,
            Syscall::Sendmsg(_) => self.handle_simple(guest, syscall).await,
            Syscall::Poll(syscall) => self.handle_poll(guest, syscall).await,
            Syscall::Getsockopt(syscall) => self.handle_sockopt_family(guest, syscall.into()).await,
            Syscall::Getpeername(syscall) => {
                self.handle_sockopt_family(guest, syscall.into()).await
            }
            Syscall::Getsockname(syscall) => {
                self.handle_sockopt_family(guest, syscall.into()).await
            }
            Syscall::Getrandom(syscall) => self.handle_getrandom(guest, syscall).await,
            Syscall::Readlink(syscall) => self.handle_readlink(guest, syscall).await,
            Syscall::Mkdir(_) => self.handle_simple(guest, syscall).await,
            Syscall::Unlink(_) => self.handle_simple(guest, syscall).await,
            Syscall::Unlinkat(_) => self.handle_simple(guest, syscall).await,
            unsupported => return Ok(guest.inject_with_retry(unsupported).await?),
        }?)
    }

    async fn handle_rdtsc_event<G: Guest<Self>>(
        &self,
        guest: &mut G,
        _request: Rdtsc,
    ) -> Result<RdtscResult, Errno> {
        next_event!(guest, Rdtsc)
    }
}

impl Replayer {
    // Check if we received the expected syscall or not.
    fn expect_syscall<G: Guest<Self>>(&self, guest: &mut G, syscall: Syscall) {
        let debug_event = guest.thread_state_mut().next_debug_event().unwrap();

        if debug_event.syscall() == syscall {
            return;
        }

        if guest.is_root_thread() {
            // execve and execveat for the root thread are special cases. Even
            // when ASLR is turned off, these can have different pointer values
            // than what we originally recorded because the pointers originate
            // outside of the current address space.
            match syscall {
                Syscall::Execve(_) | Syscall::Execveat(_) => return,
                _ => {}
            }
        }

        let error = DesyncError {
            thread: guest.tid(),
            count: guest.thread_state().count,
            actual: DebugEvent::new(syscall, &guest.memory()),
            expected: debug_event,
        };

        let report = error
            .generate_report(&self.data)
            .expect("Failed to generate desync error report");

        panic!(
            "{}\nSee the report generated at: {:?}",
            error.summary(&self.data, 16, 4),
            report
        );
    }

    /// Called for syscalls to explicitly let through. This should only be called
    /// for syscalls that cannot be recorded and are necessary for the program to
    /// function correctly. Examples of syscalls that fall into this category are
    /// ones that help with memory management (e.g., `brk`, `mprotect`, or
    /// `munmap`) or process management (e.g., `fork`, `vfork`, `clone`).
    ///
    /// For these syscalls, we just care about detecting dsynchronization and
    /// simply inject them to let them through.
    async fn let_through<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
    ) -> Result<i64, Errno> {
        // NOTE: Must use `inject_with_retry` here. Otherwise, we may end up
        // introducing non-determinism into the replay and popping multiple
        // syscall events.
        guest.inject_with_retry(syscall).await
    }

    /// Handles a syscall whose only value we care about is the return value
    /// (i.e., simple syscalls).
    ///
    /// For recording, this means we only record the return value of the syscall.
    /// For replay, this means we substitute the return value in lieu of actually
    /// performing the injection.
    ///
    /// The syscall must have two properties satisfied for this to be called:
    ///  1. The syscall must only have "input" arguments. That is, all arguments
    ///     must either be values or const pointers.
    ///  2. The execution of the program must not depend on anything else other
    ///     than the return value of the syscall. For example, `mmap` would violate
    ///     this rule since it affects later memory access.
    ///
    /// There are many syscalls who satisfy these two requirements.
    async fn handle_simple<G: Guest<Self>>(
        &self,
        guest: &mut G,
        _syscall: Syscall,
    ) -> Result<i64, Errno> {
        next_event!(guest, Return)
    }
}
