/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! SaBRe plugin that executes Hermit's Detcore tool inside each guest process.

use std::ffi::CString;
use std::ffi::OsStr;
use std::ffi::OsString;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use detcore::Detcore;
use reverie_memory::LocalMemory;
use reverie_memory::MemoryAccess;
use reverie_sabre as sabre;
use reverie_sabre::RemoteReverieAdapter;
use reverie_syscalls::Errno;
use reverie_syscalls::Syscall;
use reverie_syscalls::SyscallArgs;
use reverie_syscalls::Sysno;

/// Environment variable containing the coordinator's Unix-domain socket path.
// TODO-HUMAN-REVIEW(PR-745): Review the private SaBRe exec environment contract.
pub const RPC_SOCKET_ENV: &str = "REVERIE_SABRE_HERMIT_RPC_SOCKET";

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-771): Review fork-inherited SaBRe coordinator discovery.
static RPC_SOCKET: OnceLock<PathBuf> = OnceLock::new();

fn coordinator_socket() -> Option<PathBuf> {
    if let Some(socket) = RPC_SOCKET.get() {
        return Some(socket.clone());
    }

    // SAFETY: Plugin construction runs before SaBRe starts guest callbacks.
    let requested = unsafe { sabre::take_private_env(RPC_SOCKET_ENV) };
    remember_coordinator_socket(&RPC_SOCKET, requested.as_deref())
}

fn remember_coordinator_socket(
    slot: &OnceLock<PathBuf>,
    requested: Option<&OsStr>,
) -> Option<PathBuf> {
    slot.get().cloned().or_else(|| {
        let requested = requested.map(PathBuf::from)?;
        Some(slot.get_or_init(|| requested).clone())
    })
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-845): Review SaBRe guest comm-name restoration.
fn guest_comm_from_args(args: impl IntoIterator<Item = OsString>) -> Option<CString> {
    let program = args.into_iter().next()?;
    let name = Path::new(&program).file_name()?.as_bytes();
    CString::new(&name[..name.len().min(15)]).ok()
}

fn restore_guest_comm_name(thread_id: u32) {
    if thread_id != unsafe { libc::getpid() as u32 } {
        return;
    }
    let Some(name) = guest_comm_from_args(std::env::args_os()) else {
        return;
    };
    unsafe {
        libc::prctl(libc::PR_SET_NAME, name.as_ptr() as usize, 0, 0, 0);
    }
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-779): Review fail-closed SaBRe RDTSC errors.
fn require_virtual_rdtsc(result: Result<u64, Errno>) -> u64 {
    result.expect("SaBRe RDTSC virtualization failed")
}

fn is_post_load_bootstrap_random(syscall: &Syscall) -> bool {
    matches!(
        syscall,
        Syscall::Getrandom(call)
            if call.buflen() == 32 && call.flags() == libc::GRND_NONBLOCK as usize
    )
}

/// Returns the Detcore SaBRe plugin built beside the running Hermit binary.
// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-738): Review the Hermit-to-SaBRe plugin artifact boundary.
pub fn runtime_library_path() -> io::Result<PathBuf> {
    let executable = std::env::current_exe()?;
    let directory = executable.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "Hermit executable has no parent directory",
        )
    })?;
    [
        directory.join("libdetcore_sabre.so"),
        directory.join("deps/libdetcore_sabre.so"),
    ]
    .into_iter()
    .find(|path| path.is_file())
    .ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "libdetcore_sabre.so was not built beside {}",
                executable.display()
            ),
        )
    })
}

struct Plugin {
    adapter: RemoteReverieAdapter<Detcore>,
    // The SaBRe-injected runtime requests its hash seed on the first rewritten
    // syscall after post-load. Keep that tool-private draw out of Detcore's
    // guest-visible random stream.
    post_load_syscall_pending: AtomicBool,
}

impl Plugin {
    fn connect() -> Self {
        let socket = coordinator_socket().unwrap_or_else(|| panic!("{RPC_SOCKET_ENV} is not set"));

        let adapter = RemoteReverieAdapter::connect(socket)
            .expect("failed to connect Detcore SaBRe plugin to coordinator");

        Self {
            adapter,
            post_load_syscall_pending: AtomicBool::new(false),
        }
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1117): Review SaBRe bootstrap-random isolation.
    fn handle_post_load_syscall(&self, syscall: &Syscall) -> Option<Result<usize, Errno>> {
        if !self.post_load_syscall_pending.swap(false, Ordering::AcqRel) {
            return None;
        }

        if !is_post_load_bootstrap_random(syscall) {
            return None;
        }

        let Syscall::Getrandom(call) = syscall else {
            unreachable!("bootstrap-random classifier accepted a non-getrandom syscall")
        };
        let buffer = call.buf().ok_or(Errno::EFAULT);
        Some(buffer.and_then(|buffer| {
            let mut memory = LocalMemory::new();
            memory.write_exact(buffer, &[0; 32]).map(|()| 32)
        }))
    }

    fn handle_vdso(&self, sysno: Sysno, args: SyscallArgs) -> i32 {
        self.adapter
            .handle_syscall(Syscall::from_raw(sysno, args))
            .map_or_else(|errno| -errno.into_raw(), |result| result as i32)
    }
}

#[sabre::tool]
impl reverie_sabre::Tool for Plugin {
    type Client = ();

    fn new(_client: Self::Client) -> Self {
        Self::connect()
    }

    fn new_without_legacy_rpc() -> Option<Self> {
        Some(Self::connect())
    }

    fn syscall(&self, syscall: Syscall, _memory: &LocalMemory) -> Result<usize, Errno> {
        if let Some(result) = self.handle_post_load_syscall(&syscall) {
            return result;
        }
        self.adapter.handle_syscall(syscall)
    }

    fn syscall_with_inject<F>(
        &self,
        syscall: Syscall,
        _memory: &LocalMemory,
        inject: F,
    ) -> Result<usize, Errno>
    where
        F: FnMut() -> usize + Send + Sync,
    {
        self.adapter.handle_syscall_with_inject(syscall, inject)
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-755): Review SaBRe RDTSC virtualization.
    fn rdtsc(&self) -> u64 {
        require_virtual_rdtsc(self.adapter.handle_rdtsc())
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-755): Review SaBRe clock_gettime VDSO virtualization.
    fn vdso_clock_gettime(&self, clockid: libc::clockid_t, tp: *mut libc::timespec) -> i32 {
        self.handle_vdso(
            Sysno::clock_gettime,
            SyscallArgs::new(clockid as usize, tp as usize, 0, 0, 0, 0),
        )
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-755): Review SaBRe getcpu VDSO virtualization.
    fn vdso_getcpu(&self, cpu: *mut u32, node: *mut u32, unused: usize) -> i32 {
        self.handle_vdso(
            Sysno::getcpu,
            SyscallArgs::new(cpu as usize, node as usize, unused, 0, 0, 0),
        )
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-755): Review SaBRe gettimeofday VDSO virtualization.
    fn vdso_gettimeofday(&self, tv: *mut libc::timeval, tz: *mut libc::timezone) -> i32 {
        self.handle_vdso(
            Sysno::gettimeofday,
            SyscallArgs::new(tv as usize, tz as usize, 0, 0, 0, 0),
        )
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-755): Review SaBRe time VDSO virtualization.
    fn vdso_time(&self, tloc: *mut libc::time_t) -> i32 {
        self.handle_vdso(Sysno::time, SyscallArgs::new(tloc as usize, 0, 0, 0, 0, 0))
    }

    fn on_thread_start(&self, thread_id: u32) {
        restore_guest_comm_name(thread_id);
        self.adapter.handle_thread_start(thread_id);
    }

    fn on_post_load(&self) {
        self.adapter.handle_post_exec();
        self.post_load_syscall_pending
            .store(true, Ordering::Release);
    }

    fn on_thread_exit(&self, thread_id: u32) {
        self.adapter.handle_thread_exit(thread_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guest_comm_uses_target_basename_and_linux_limit() {
        assert_eq!(
            guest_comm_from_args(
                ["/usr/bin/bash", "-c", "exit 0"]
                    .into_iter()
                    .map(OsString::from)
            )
            .unwrap()
            .to_bytes(),
            b"bash"
        );
        assert_eq!(
            guest_comm_from_args(["abcdefghijklmnop"].into_iter().map(OsString::from))
                .unwrap()
                .to_bytes(),
            b"abcdefghijklmno"
        );
    }

    #[test]
    fn rpc_socket_uses_sabre_private_environment_namespace() {
        assert!(RPC_SOCKET_ENV.starts_with("REVERIE_SABRE_"));
    }

    #[test]
    fn rpc_socket_survives_plugin_reinitialization() {
        let socket = OnceLock::new();

        assert_eq!(
            remember_coordinator_socket(&socket, Some(OsStr::new("/tmp/coordinator.sock"))),
            Some(PathBuf::from("/tmp/coordinator.sock"))
        );
        assert_eq!(
            remember_coordinator_socket(&socket, None),
            Some(PathBuf::from("/tmp/coordinator.sock"))
        );
    }

    #[test]
    fn virtual_rdtsc_returns_coordinator_value() {
        assert_eq!(require_virtual_rdtsc(Ok(42)), 42);
    }

    #[test]
    fn recognizes_only_sabre_post_load_bootstrap_random_shape() {
        let buffer = 0x1234;
        let syscall = |length, flags| {
            Syscall::from_raw(
                Sysno::getrandom,
                SyscallArgs::new(buffer, length, flags, 0, 0, 0),
            )
        };

        assert!(is_post_load_bootstrap_random(&syscall(
            32,
            libc::GRND_NONBLOCK as usize
        )));
        assert!(!is_post_load_bootstrap_random(&syscall(
            31,
            libc::GRND_NONBLOCK as usize
        )));
        assert!(!is_post_load_bootstrap_random(&syscall(32, 0)));
        assert!(!is_post_load_bootstrap_random(&Syscall::from_raw(
            Sysno::getpid,
            SyscallArgs::new(0, 0, 0, 0, 0, 0),
        )));
    }

    #[test]
    #[should_panic(expected = "SaBRe RDTSC virtualization failed")]
    fn virtual_rdtsc_error_fails_closed() {
        require_virtual_rdtsc(Err(Errno::EIO));
    }
}
