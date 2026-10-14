/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Starting `hermit-dap`, the Debug Adapter Protocol adapter that is built
//! beside `hermit`, for `run --dap-listen` and `replay --dap-listen`.

use std::os::unix::process::CommandExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Child;
use std::process::Command;
use std::process::Stdio;
use std::time::Duration;
use std::time::Instant;

use hermit::Context;
use hermit::Error;

/// How long a finished run waits for its adapter to end on its own, as it does
/// once the DAP client has seen the program exit and disconnected, before
/// stopping it.
const ADAPTER_EXIT_GRACE: Duration = Duration::from_secs(5);

/// The `hermit-dap` beside this `hermit` executable.
pub fn hermit_dap_beside_hermit() -> Result<PathBuf, Error> {
    let adapter = std::env::current_exe()
        .context("Failed to locate hermit")?
        .with_file_name("hermit-dap");
    if !adapter.is_file() {
        anyhow::bail!(
            "--dap-listen needs hermit-dap beside hermit, and {} does not exist; \
             build it with `cargo build -p hermit --bin hermit --bin hermit-dap`",
            adapter.display()
        );
    }
    Ok(adapter)
}

/// The line that tells the user what a DAP client's attach request must name.
pub fn attach_hint(program: &Path, gdbserver_port: u16) -> String {
    format!(
        "its attach request needs \"program\": {:?} and \"target\": \"127.0.0.1:{gdbserver_port}\"",
        program.display().to_string()
    )
}

/// A `hermit-dap --listen` serving a `hermit run --gdbserver` in this
/// process. It runs outside the guest's namespaces, as the GDB client of a
/// plain `--gdbserver` run does, and is stopped when this value is dropped:
/// at once if no client ever connected, or after [`ADAPTER_EXIT_GRACE`] if its
/// session has not ended by then. It also dies with this process.
#[derive(Debug)]
pub struct ListeningAdapter {
    child: Child,
}

impl ListeningAdapter {
    /// Whether the adapter is still waiting for its client. `hermit-dap`
    /// becomes GDB, by `exec`, once a client connects.
    fn still_waiting_for_a_client(&self) -> bool {
        std::fs::read_to_string(format!("/proc/{}/comm", self.child.id()))
            .is_ok_and(|comm| comm.trim_end() == "hermit-dap")
    }
}

impl ListeningAdapter {
    pub fn spawn(address: &str) -> Result<Self, Error> {
        let adapter = hermit_dap_beside_hermit()?;
        let mut command = Command::new(&adapter);
        command
            .arg("--listen")
            .arg(address)
            .stdin(Stdio::null())
            .stdout(Stdio::null());
        // SAFETY: prctl is async-signal-safe and touches no memory of the
        // parent; it is the only work done between fork and exec.
        unsafe {
            command.pre_exec(|| {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command
            .spawn()
            .with_context(|| format!("Failed to start {}", adapter.display()))?;
        Ok(Self { child })
    }
}

impl Drop for ListeningAdapter {
    fn drop(&mut self) {
        let grace = if self.still_waiting_for_a_client() {
            Duration::ZERO
        } else {
            ADAPTER_EXIT_GRACE
        };
        let deadline = Instant::now() + grace;
        while Instant::now() < deadline {
            match self.child.try_wait() {
                Ok(Some(_)) | Err(_) => return,
                Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
