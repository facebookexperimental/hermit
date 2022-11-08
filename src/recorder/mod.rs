// (c) Meta Platforms, Inc. and affiliates. Confidential and proprietary.

mod fs;
mod mmap;
mod network;
mod random;
mod time;

use std::path::PathBuf;

use reverie::syscalls::Syscall;
use reverie::syscalls::Sysno;
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

use crate::event::Event;
use crate::event::SyscallEvent;
use crate::event_stream::DebugEvent;
use crate::event_stream::EventWriter;

/// A Reverie tool that records syscalls. Note that only syscalls that cannot be
/// made deterministic are forwarded to this tool.
#[derive(Default, Serialize, Deserialize)]
pub struct Recorder {
    // TODO: We'll need to keep track of file descriptors here in order to
    // determine if a file descriptor should be fully recorded or simply cached
    // with a reflink. We can use `fstatfs` to figure out if the target file
    // system supports reflinks or not. All other file systems will need their
    // file interactions to be recorded on the syscall level.

    // Keep track of the data directory. Each thread uses this path to open its
    // event stream.
    data: PathBuf,
}

#[reverie::tool]
impl Tool for Recorder {
    type GlobalState = detcore::GlobalState;
    type ThreadState = EventWriter;

    fn new(_pid: Pid, cfg: &<Self::GlobalState as GlobalTool>::Config) -> Self {
        Self {
            data: cfg.replay_data.as_ref().unwrap().clone(),
        }
    }

    fn init_thread_state(
        &self,
        child: Tid,
        _parent: Option<(Tid, &Self::ThreadState)>,
    ) -> Self::ThreadState {
        // We have to unwrap because there is no way to handle errors here.
        EventWriter::create(&self.data, child).unwrap_or_else(|err| {
            panic!(
                "Failed to create {:?} for thread {}: {}",
                self.data, child, err
            )
        })
    }

    fn subscriptions(_config: &<Self::GlobalState as GlobalTool>::Config) -> Subscription {
        let mut subscription = Subscription::none();
        subscription.rdtsc().cpuid().syscalls([
            Sysno::execve,
            Sysno::execveat,
            //Sysno::brk,
            Sysno::mprotect,
            //Sysno::arch_prctl,
            Sysno::read,
            Sysno::pread64,
            Sysno::recvfrom,
            Sysno::write,
            Sysno::pwrite64,
            Sysno::writev,
            Sysno::pwritev,
            Sysno::pwritev2,
            Sysno::access,
            Sysno::lseek,
            Sysno::stat,
            Sysno::fstat,
            Sysno::lstat,
            Sysno::newfstatat,
            Sysno::statx,
            Sysno::getdents,
            Sysno::getdents64,
            Sysno::mmap,
            //Sysno::munmap,
            Sysno::open,
            Sysno::openat,
            Sysno::close,
            Sysno::fadvise64,
            Sysno::dup,
            Sysno::dup2,
            Sysno::dup3,
            Sysno::ioctl,
            Sysno::socket,
            Sysno::clock_gettime,
            Sysno::gettimeofday,
            Sysno::settimeofday,
            Sysno::time,
            Sysno::setsockopt,
            Sysno::fcntl,
            Sysno::connect,
            Sysno::sendto,
            Sysno::sendmsg,
            Sysno::poll,
            Sysno::getsockopt,
            Sysno::getpeername,
            Sysno::getsockname,
            Sysno::getrandom,
            Sysno::readlink,
            Sysno::mkdir,
            Sysno::unlink,
            Sysno::unlinkat,
        ]);

        subscription
    }

    async fn handle_syscall_event<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
    ) -> Result<i64, Error> {
        self.record_raw_syscall(guest, syscall);

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
            unsupported => return Ok(guest.inject(unsupported).await?),
        }?)
    }

    async fn handle_rdtsc_event<G: Guest<Self>>(
        &self,
        guest: &mut G,
        request: Rdtsc,
    ) -> Result<RdtscResult, Errno> {
        let result = RdtscResult::new(request);
        self.record_event(guest, Ok(SyscallEvent::Rdtsc(result)));
        Ok(result)
    }
}

impl Recorder {
    fn record_raw_syscall<G: Guest<Self>>(&self, guest: &mut G, syscall: Syscall) {
        let debug_event = DebugEvent::new(syscall, &guest.memory());
        guest
            .thread_state_mut()
            .push_debug_event(debug_event)
            .unwrap();
    }

    fn record_event<G: Guest<Self>>(&self, guest: &mut G, event: Result<SyscallEvent, Errno>) {
        // Record the event.
        guest
            .thread_state_mut()
            .push_event(Event { event })
            // TODO: Log errors instead of panicking.
            .unwrap();
    }

    /// Called for syscalls to explicitly let through. This should only be called
    /// for syscalls that cannot be recorded and are necessary for the program to
    /// function correctly. Examples of syscalls that fall into this category are
    /// ones that help with memory management (e.g., `brk`, `mprotect`, `mmap`,
    /// or `munmap`) or process management (e.g., `fork`, `vfork`, `clone`).
    ///
    /// For these syscalls, we don't really need to record anything, but we
    /// record their arguments to detect any desynchronization.
    async fn let_through<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
    ) -> Result<i64, Errno> {
        guest.inject(syscall).await
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
        syscall: Syscall,
    ) -> Result<i64, Errno> {
        let result = guest.inject(syscall).await;

        self.record_event(guest, result.map(SyscallEvent::Return));

        result
    }
}
