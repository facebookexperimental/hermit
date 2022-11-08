/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::io::Write;
use std::net::IpAddr;
use std::net::Ipv4Addr;
use std::net::Ipv6Addr;
use std::net::SocketAddr;
use std::path::PathBuf;

use clap::Parser;
use hermit::Error;
use once_cell::sync::OnceCell;
use reverie::process::Command;
use reverie::syscalls;
use reverie::syscalls::Addr;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::Syscall;
use reverie::syscalls::Sysno;
use reverie::Errno;
use reverie::Error as TraceError;
use reverie::ExitStatus;
use reverie::GlobalTool;
use reverie::Guest;
use reverie::Pid;
use reverie::Subscription;
use reverie::Tool;
use serde::Deserialize;
use serde::Serialize;

use super::container::default_container;
use super::container::with_container;
use super::global_opts::GlobalOpts;

/// A tool to trace system calls.
#[derive(Parser, Debug)]
pub struct BnzOpts {
    /// Program to run.
    #[clap(value_name = "PROGRAM")]
    program: PathBuf,

    /// Arguments for the program.
    #[clap(value_name = "ARGS")]
    args: Vec<String>,

    /// Write to output (json) when non-zero bind is detected.
    #[clap(long, short = 'o')]
    output: Option<PathBuf>,

    #[clap(flatten)]
    config: BnzConfig,
}

static LOGGER: OnceCell<std::fs::File> = OnceCell::new();

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct BnzResult {
    pub pid: Pid,
    pub comm: String,
    pub sock: SocketAddr,
    pub errno: Option<Errno>,
    pub desc: String,
}

impl BnzResult {
    fn new(pid: Pid, sockaddr: SocketAddr, errno: Option<Errno>) -> Self {
        let proc_pid_comm = format!("/proc/{}/comm", pid);
        let comm = std::fs::read_to_string(proc_pid_comm).unwrap_or_else(|_| String::from(""));
        BnzResult {
            pid,
            comm: comm.split('\n').next().unwrap_or("<unknown>").to_string(),
            sock: sockaddr,
            errno,
            desc: String::new(),
        }
    }
}

fn write_bnz_result(bnz: &BnzResult) -> Result<(), Error> {
    if let Some(mut logger) = LOGGER.get() {
        let json_str = serde_json::to_string(bnz)?;
        writeln!(logger, "{}", json_str)?;
    }
    Ok(())
}

#[derive(Debug, Serialize, Deserialize, Default, Clone, Parser)]
pub struct BnzConfig {
    /// Assert when a non-zero bind is called
    #[clap(long)]
    assert_on_non_zero_bind: bool,

    /// Assert when a non-zero bind returns -EADDRINUSE.
    #[clap(long)]
    assert_on_eaddrinuse: bool,
}

#[derive(Debug, Serialize, Deserialize, Default, Clone)]
pub struct BnzGlobal;

#[derive(Debug, Serialize, Deserialize, Default, Clone)]
pub struct BnzTool;

#[reverie::global_tool]
impl GlobalTool for BnzGlobal {
    type Request = ();
    type Response = ();
    type Config = BnzConfig;

    async fn receive_rpc(&self, _pid: Pid, _req: Self::Request) {}
}

impl BnzTool {
    async fn inject_fatal_error<T: Guest<Self>>(guest: &mut T) {
        let _ = guest
            .inject(
                syscalls::Tkill::new()
                    .with_tid(guest.tid().into())
                    .with_sig(libc::SIGBUS),
            )
            .await;
    }

    async fn handle_bind<T: Guest<Self>>(
        &self,
        guest: &mut T,
        bind: syscalls::Bind,
    ) -> Result<i64, Errno> {
        let res = guest.inject(bind).await;

        let memory = guest.memory();
        if let Some(sockaddr) = bind.umyaddr() {
            let sa = if bind.addrlen() as usize == std::mem::size_of::<libc::sockaddr_in>() {
                let sock_v4_addr: Addr<libc::sockaddr_in> = sockaddr.cast().into();
                let sock_v4 = memory.read_value(sock_v4_addr)?;
                let v4: IpAddr = Ipv4Addr::from(u32::from_be(sock_v4.sin_addr.s_addr)).into();
                Some(SocketAddr::new(v4, u16::from_be(sock_v4.sin_port)))
            } else if bind.addrlen() as usize == std::mem::size_of::<libc::sockaddr_in6>() {
                let sock_v6_addr: Addr<libc::sockaddr_in6> = sockaddr.cast().into();
                let sock_v6 = memory.read_value(sock_v6_addr)?;
                let v6: IpAddr = Ipv6Addr::from(sock_v6.sin6_addr.s6_addr).into();
                Some(SocketAddr::new(v6, u16::from_be(sock_v6.sin6_port)))
            } else {
                None
            };
            if let Some(sa) = sa {
                if sa.port() != 0 {
                    let config = guest.config().clone();
                    let errno = res.map_or_else(Some, |_| None);
                    let _ = write_bnz_result(&BnzResult::new(guest.pid(), sa, errno));
                    if config.assert_on_non_zero_bind {
                        BnzTool::inject_fatal_error(guest).await;
                    } else if let Some(eaddrinuse) = errno {
                        if eaddrinuse == Errno::EADDRINUSE && config.assert_on_eaddrinuse {
                            BnzTool::inject_fatal_error(guest).await;
                        }
                    }
                }
            }
        }
        res
    }
}
/// Here we use the same dummy type for both our local and global trait
/// implementations.
#[reverie::tool]
impl Tool for BnzTool {
    type GlobalState = BnzGlobal;

    fn subscriptions(_cfg: &BnzConfig) -> Subscription {
        let mut subs = Subscription::none();
        subs.syscalls(vec![Sysno::bind]);
        subs
    }

    async fn handle_syscall_event<T: Guest<Self>>(
        &self,
        guest: &mut T,
        syscall: Syscall,
    ) -> Result<i64, TraceError> {
        match syscall {
            Syscall::Bind(bind) => Ok(self.handle_bind(guest, bind).await?),
            _ => unreachable!(),
        }
    }

    type ThreadState = ();
}

// NOTE: A single-threaded executor is used here so that the tokio threads
// themselves wouldn't contribute non-determinism to the PID namespace. This
// could also be changed to a specific number of threads and that would be
// deterministic, but it shouldn't be based on the number of cores. When the
// thread count is based off of the number of cores in the machine, then two
// runs on different machines with a different number of cores will not be the
// same.
#[tokio::main(flavor = "current_thread")]
/// Run the given command as deterministically as possible.
pub async fn run(command: Command, config: BnzConfig) -> Result<ExitStatus, Error> {
    let builder = reverie_ptrace::TracerBuilder::<BnzTool>::new(command).config(config);
    let (exit_status, _) = builder.spawn().await?.wait().await?;
    Ok(exit_status)
}

impl BnzOpts {
    fn run_in_container(&self, command: Command, global: &GlobalOpts) -> Result<ExitStatus, Error> {
        let _guard = global.init_tracing();
        let config = self.config.clone();
        run(command, config)
    }
    pub fn main(&self, global: &GlobalOpts) -> Result<ExitStatus, Error> {
        let mut container = default_container(false);

        if let Some(output) = self.output.as_ref() {
            let file = std::fs::File::create(output)?;
            LOGGER
                .set(file)
                .unwrap_or_else(|_| panic!("Unable to open output: {:?}", self.output));
        }

        with_container(&mut container, || {
            let mut command = Command::new(&self.program);
            command.args(&self.args);
            self.run_in_container(command, global)
        })
    }
}
