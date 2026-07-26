/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! SaBRe plugin that executes Hermit's Detcore tool inside each guest process.

use std::ffi::OsStr;
use std::io;
use std::path::PathBuf;
use std::sync::OnceLock;

use detcore::Detcore;
use reverie_memory::LocalMemory;
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
// TODO-HUMAN-REVIEW(PR-779): Review fail-closed SaBRe RDTSC errors.
fn require_virtual_rdtsc(result: Result<u64, Errno>) -> u64 {
    result.expect("SaBRe RDTSC virtualization failed")
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
}

impl Plugin {
    fn connect() -> Self {
        let socket = coordinator_socket().unwrap_or_else(|| panic!("{RPC_SOCKET_ENV} is not set"));

        let adapter = RemoteReverieAdapter::connect(socket)
            .expect("failed to connect Detcore SaBRe plugin to coordinator");

        Self { adapter }
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
        self.adapter.handle_thread_start(thread_id);
    }

    fn on_thread_exit(&self, thread_id: u32) {
        self.adapter.handle_thread_exit(thread_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    #[should_panic(expected = "SaBRe RDTSC virtualization failed")]
    fn virtual_rdtsc_error_fails_closed() {
        require_virtual_rdtsc(Err(Errno::EIO));
    }
}
