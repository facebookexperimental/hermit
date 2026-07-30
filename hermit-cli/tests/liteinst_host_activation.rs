/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#[path = "common/liteinst.rs"]
mod liteinst_runtime;

use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use reverie::Error;
use reverie::GlobalTool;
use reverie::Guest;
use reverie::Subscription;
use reverie::Tid;
use reverie::Tool;
use reverie::process::Command;
use reverie::syscalls::Syscall;
use reverie::syscalls::SyscallInfo;
use reverie::syscalls::Sysno;
use reverie_liteinst::LiteinstBackend;

#[derive(Debug, Default)]
struct ActivationCount(AtomicU64);

#[reverie::global_tool]
impl GlobalTool for ActivationCount {
    type Request = ();
    type Response = ();
    type Config = ();

    async fn receive_rpc(&self, _from: Tid, (): ()) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[derive(Default)]
struct GetpidTool;

#[reverie::tool]
impl Tool for GetpidTool {
    type GlobalState = ActivationCount;
    type ThreadState = ();

    fn subscriptions(_config: &()) -> Subscription {
        [Sysno::getpid].into_iter().collect()
    }

    async fn handle_syscall_event<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
    ) -> Result<i64, Error> {
        assert_eq!(syscall.number(), Sysno::getpid);
        guest.send_rpc(()).await;
        Ok(guest.inject(syscall).await?)
    }
}

fn activation_guest() -> (tempfile::TempDir, PathBuf) {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository");
    let directory = tempfile::tempdir().expect("create activation guest directory");
    let guest = directory.path().join("liteinst_host_activation");
    let output = std::process::Command::new("cc")
        .args(["-O0", "-fno-pie", "-no-pie", "-Wall", "-Wextra", "-Werror"])
        .arg(repository.join("tests/c/liteinst_host_activation.c"))
        .arg("-ldl")
        .arg("-o")
        .arg(&guest)
        .output()
        .expect("compile activation guest");
    assert!(output.status.success(), "{output:?}");
    (directory, guest)
}

#[tokio::test(flavor = "current_thread")]
async fn exact_staged_runtime_activates_with_a_minimal_tool() {
    liteinst_runtime::ensure_liteinst_runtime();
    let runtime = liteinst_runtime::liteinst_runtime_library();
    let (_directory, guest) = activation_guest();
    let native = std::process::Command::new(&guest)
        .output()
        .expect("run activation guest natively");
    assert_eq!(native.status.code(), Some(20), "{native:?}");
    let (output, global) = LiteinstBackend::run_host_with_output_and_preload::<GetpidTool>(
        Command::new(&guest),
        (),
        runtime,
    )
    .await
    .expect("run minimal LiteInst host activation");
    assert!(
        output.status.success(),
        "{output:?}; delivered={}",
        global.0.load(Ordering::SeqCst)
    );
    assert_eq!(output.stdout, b"calls=32 traps=1 hooks=31\n");
    assert_eq!(global.0.load(Ordering::SeqCst), 32);
}
