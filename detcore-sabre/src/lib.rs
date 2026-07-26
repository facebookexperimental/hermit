/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! SaBRe plugin that executes Hermit's Detcore tool inside each guest process.

use std::io;
use std::path::PathBuf;

use detcore::Detcore;
use reverie_memory::LocalMemory;
use reverie_sabre as sabre;
use reverie_sabre::RemoteReverieAdapter;
use reverie_syscalls::Errno;
use reverie_syscalls::Syscall;

/// Environment variable containing the coordinator's Unix-domain socket path.
// TODO-HUMAN-REVIEW(PR-745): Review the private SaBRe exec environment contract.
pub const RPC_SOCKET_ENV: &str = "REVERIE_SABRE_HERMIT_RPC_SOCKET";

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
        // SAFETY: Plugin construction runs before SaBRe starts guest callbacks.
        let socket = unsafe { sabre::take_private_env(RPC_SOCKET_ENV) }
            .unwrap_or_else(|| panic!("{RPC_SOCKET_ENV} is not set"));

        let adapter = RemoteReverieAdapter::connect(socket)
            .expect("failed to connect Detcore SaBRe plugin to coordinator");

        Self { adapter }
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
}
