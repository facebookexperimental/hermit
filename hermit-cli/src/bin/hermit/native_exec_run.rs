/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 * This source code is licensed under the BSD-style license found in LICENSE.
 */

//! Composition of the common container, shared native launcher and one real
//! Detcore coordinator. The original CLI verification sink remains its owner.

use std::fs::File;
use std::fs::Permissions;
use std::io;
use std::os::fd::AsFd;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use anyhow::ensure;
use reverie::GlobalTool;
use reverie::process::Command;
use reverie::process::Output;
use reverie_inguest::launcher::InitialImageStatsServer;
use reverie_inguest::launcher::native::NativeRunIo;
use reverie_inguest::launcher::native::NativeRunOutput;
use reverie_inguest::launcher::native_frontend::NativeFrontend;
use reverie_rpc_transport::RpcServer;

use super::container::declared_inputs::DeclaredRootAdditions;
use super::container::declared_inputs::Endpoint;
use super::container::declared_inputs::PreparedDeclaredRoot;
use super::container::declared_inputs::RuntimeFile;
use super::native_exec_artifacts::QualifiedNativeArtifacts;
use super::native_exec_coordinator::NativeRunOwner;
use super::native_exec_inputs::GuestPath;
use super::native_exec_inputs::InputRole;
use super::native_exec_inputs::seal_bytes;
use super::native_exec_maps::RootIdentity;
use super::native_exec_setup;

fn packet_pair() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut pair = [-1; 2];
    if unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
            0,
            pair.as_mut_ptr(),
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { (OwnedFd::from_raw_fd(pair[0]), OwnedFd::from_raw_fd(pair[1])) })
}

fn packet_listener(path: &Path) -> io::Result<OwnedFd> {
    let bytes = path.as_os_str().as_bytes();
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    if bytes.contains(&0) || bytes.len() >= address.sun_path.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "native provision socket path bound",
        ));
    }
    address.sun_family = libc::AF_UNIX as _;
    for (to, from) in address.sun_path.iter_mut().zip(bytes) {
        *to = *from as libc::c_char;
    }
    let raw = unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            0,
        )
    };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    if unsafe {
        libc::bind(
            fd.as_raw_fd(),
            (&raw const address).cast(),
            std::mem::size_of_val(&address) as _,
        )
    } != 0
        || unsafe { libc::listen(fd.as_raw_fd(), 4) } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(fd)
}

/// Descriptor-free plan for one observer pair, opened after the outer clone.
struct ObserverPlan {
    opened: bool,
}

impl ObserverPlan {
    fn open_in_container(&mut self) -> io::Result<(OwnedFd, OwnedFd)> {
        if std::mem::replace(&mut self.opened, true) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "native observer plan already consumed",
            ));
        }
        packet_pair()
    }
}

/// Prebound genuine socket objects. Binding happens before outer container
/// setup; that common owner pins these same inodes into its declared root.
/// The short proc-fd path avoids changing Unix socket limits for long checkout
/// paths; its actual directory descriptor remains owned through server drain.
pub(super) struct NativeEndpoints {
    _directory: tempfile::TempDir,
    _directory_fd: File,
    rpc_path: PathBuf,
    stats_path: PathBuf,
    directory_path: PathBuf,
    rpc: UnixListener,
    stats: UnixListener,
    provision: OwnedFd,
    observer: ObserverPlan,
}
impl NativeEndpoints {
    pub(super) fn bind(staging_parent: &Path) -> Result<Self> {
        ensure!(
            staging_parent.is_absolute(),
            "native endpoint staging parent must be absolute"
        );
        let directory = tempfile::Builder::new()
            .prefix("native-sockets-")
            .permissions(Permissions::from_mode(0o700))
            .tempdir_in(staging_parent)?;
        let directory_fd = File::open(directory.path())?;
        let directory_path = directory.path().to_path_buf();
        let short_path = PathBuf::from(format!("/proc/self/fd/{}", directory_fd.as_raw_fd()));
        let rpc_path = short_path.join("rpc.sock");
        let stats_path =
            reverie_inguest::initial_image_stats::initial_image_stats_endpoint(&rpc_path)?;
        let rpc = UnixListener::bind(&rpc_path)?;
        let stats = UnixListener::bind(&stats_path)?;
        let provision = packet_listener(&short_path.join("provision.sock"))?;
        Ok(Self {
            _directory: directory,
            _directory_fd: directory_fd,
            rpc_path,
            stats_path,
            directory_path,
            rpc,
            stats,
            provision,
            observer: ObserverPlan { opened: false },
        })
    }
    pub(super) fn open_observer_in_container(&mut self) -> io::Result<(OwnedFd, OwnedFd)> {
        self.observer.open_in_container()
    }

    pub(super) fn add_to(&self, additions: &mut DeclaredRootAdditions) -> Result<()> {
        // Retain the short procfd spelling for AF_UNIX binding and server
        // drain, but reopen mount sources through the actual child namespace.
        // An inherited directory FD still references its parent vfsmount.
        let rpc_name = self
            .rpc_path
            .file_name()
            .context("native RPC endpoint lost name")?;
        let stats_name = self
            .stats_path
            .file_name()
            .context("native statistics endpoint lost name")?;
        additions.add_endpoint(Endpoint::Rpc, &self.directory_path.join(rpc_name))?;
        additions.add_endpoint(Endpoint::Statistics, &self.directory_path.join(stats_name))?;
        additions.add_endpoint(
            Endpoint::Provision,
            &self.directory_path.join("provision.sock"),
        )?;
        Ok(())
    }
}

/// Inputs for one physical execution. Both runs retain the same input/artifact
/// capture, while endpoints, actual child watch and transaction nonces are new.
pub(super) struct PhysicalRun {
    pub(super) command: Command,
    pub(super) config: detcore::Config,
    pub(super) root: PreparedDeclaredRoot,
    pub(super) runtime_files: Vec<RuntimeFile>,
    pub(super) artifacts: Arc<QualifiedNativeArtifacts>,
    pub(super) endpoints: NativeEndpoints,
    pub(super) detlog_sender: OwnedFd,
    pub(super) print_summary: bool,
    pub(super) summary_json: Option<PathBuf>,
}

// Use the same retained verification snapshot as the other output backends.
// This is a read-only regular file (or null), never a fabricated controller
// input pipe or a second capture of the original stdin.
fn replay_native_stdin(command: &mut Command) -> Result<()> {
    command.stdin(hermit::output_backend_stdin()?);
    Ok(())
}

// Own only output actually returned by the shared native launcher. The
// existing Result remains the sole source of success/failure and error priority.
#[derive(Default)]
struct CapturedNativeOutput {
    output: Option<NativeRunOutput>,
}

impl CapturedNativeOutput {
    fn capture(&mut self, result: Result<NativeRunOutput>) -> Result<()> {
        self.output = Some(result?);
        Ok(())
    }

    // Keep the Status-without-Output refusal at its original late check.
    fn require_output(&self) -> Result<()> {
        match self.output {
            Some(NativeRunOutput::Output(_)) => Ok(()),
            Some(NativeRunOutput::Status(_)) | None => {
                bail!("native capture returned status without output")
            }
        }
    }

    fn finish(self, result: Result<()>, diagnostic: &mut impl io::Write) -> Result<Output> {
        if let Err(error) = result {
            if let Some(NativeRunOutput::Output(output)) = self.output.as_ref() {
                // Existing conditional capped/uncapped diagnostic semantics.
                // A write failure never replaces the selected native error.
                let _ = diagnostic.write_all(&output.stderr);
            }
            return Err(error);
        }
        match self.output {
            Some(NativeRunOutput::Output(output)) => Ok(output),
            Some(NativeRunOutput::Status(_)) | None => {
                bail!("native capture returned status without output")
            }
        }
    }
}

/// The common outer namespace and root materialization must already be done.
/// Call from a fresh current-thread Tokio runtime; no worker exists before
/// setup reserves its fixed descriptor roles and preallocates the child entry.
pub(super) async fn run(physical: PhysicalRun) -> Result<Output> {
    let mut captured = CapturedNativeOutput::default();
    let result = run_captured(physical, &mut captured).await;
    captured.finish(result, &mut detcore::util::DroppingStderr)
}

async fn run_captured(physical: PhysicalRun, captured: &mut CapturedNativeOutput) -> Result<()> {
    let PhysicalRun {
        mut command,
        config,
        mut root,
        runtime_files,
        artifacts,
        mut endpoints,
        detlog_sender,
        print_summary,
        summary_json,
    } = physical;
    replay_native_stdin(&mut command)?;
    detcore::native_exec::validate_native_exec_host_configuration(&config)
        .map_err(|e| anyhow::anyhow!("native host Config refused: {e:?}"))?;
    let initial = GuestPath::new(command.get_program().as_bytes().to_vec())?;
    let cwd = match command.get_current_dir() {
        Some(cwd) => cwd.to_path_buf(),
        None => std::env::current_dir()?,
    };
    ensure!(cwd.is_absolute(), "native original cwd must be absolute");
    let root_identity = RootIdentity::capture(&root, &runtime_files, artifacts.clone())?;
    let guest_setup = root.prepare_guest_setup(&cwd)?;
    let (observer_read, observer_send) = endpoints.open_observer_in_container()?;
    let NativeEndpoints {
        _directory,
        _directory_fd,
        rpc_path,
        stats_path: _,
        directory_path,
        rpc,
        stats,
        provision,
        observer: _,
    } = endpoints;
    let global = Arc::new(detcore::GlobalState::init_global_state(&config).await);
    let result = async {
        let owner = NativeRunOwner::prepare(
            global.clone(),
            artifacts.inputs().clone(),
            artifacts.clone(),
            root_identity,
            &initial,
            detlog_sender.as_fd(),
            (observer_read, observer_send.as_fd()),
        )?;
        let packet = seal_bytes(&owner.initial_packet()?, InputRole::Data)?;
        let row = artifacts
            .catalog()
            .iter()
            .find(|row| {
                artifacts
                    .inputs()
                    .require_initial(&initial)
                    .is_ok_and(|target| target.identity.digest == row.target_digest)
            })
            .context("native initial catalog entry disappeared")?;
        let gate: Box<[u8; 4096]> = artifacts
            .gate_bytes()?
            .try_into()
            .map(Box::new)
            .map_err(|_| anyhow::anyhow!("native gate page length"))?;
        // SAFETY: this actual compiled artifact owner qualified the complete
        // gate and native ELF roles; root and initial target are retained here.
        let frontend = unsafe { NativeFrontend::new(gate)? };
        let setup = unsafe {
            native_exec_setup::prepare(
                guest_setup,
                frontend,
                [
                    row.stock_loader(),
                    row.runner(),
                    row.tool_loader(),
                    packet.as_fd(),
                    row.native_config(),
                ],
                observer_send.as_fd(),
                detlog_sender.as_fd(),
            )?
        };
        // Only Setup's cloned fixed roles may reach the guest. Retaining a
        // parent socket sender here would conceal EOF after physical death.
        drop(observer_send);
        drop(detlog_sender);
        let mut server = RpcServer::from_prebound(rpc, global.clone(), config.clone())?;
        owner.configure_server(&mut server)?;
        #[cfg(test)]
        super::native_exec_coordinator::bootstrap_mask_observation::configure(&owner, &mut server)?;
        let statistics_admission = owner.statistics_admission();
        let (stats_server, collector) = InitialImageStatsServer::from_prebound(
            stats,
            &rpc_path,
            &directory_path,
            statistics_admission.clone(),
        )?;
        let mut helpers = tokio::task::JoinSet::new();
        let provider_owner = owner.clone();
        helpers.spawn(async move {
            provider_owner.wait_until_started().await?;
            serve_provider(provision, provider_owner).await
        });
        let observer_owner = owner.clone();
        helpers.spawn(async move {
            observer_owner.wait_until_started().await?;
            observer_owner.observer()?.serve().await?;
            // EOF is checked by actual physical completion, never a success
            // result from a helper task racing the native root wait.
            std::future::pending::<io::Result<()>>().await
        });
        // SAFETY: same actual owner, root, source artifacts, child setup, typed
        // servers and fixed terminal entry. No guest target/vector substitution.
        let execution = unsafe {
            reverie_inguest::launcher::native::run_native(
                command,
                setup,
                reverie_inguest::launcher::native_frontend::terminal_execve,
                owner.clone(),
                server,
                NativeRunIo {
                    stats: Some(stats_server),
                    capture_output: true,
                },
                &global,
            )
        };
        tokio::pin!(execution);
        let mut result = tokio::select! {
            biased;
            stopped = helpers.join_next() => {
                let reason = format!("native provider/observer stopped: {stopped:?}");
                owner.fail(None, &reason);
                match captured.capture(execution.await.map_err(anyhow::Error::from)) {
                    Ok(()) => Err(anyhow::anyhow!(reason)),
                    Err(error) => Err(error.context(reason)),
                }
            }
            result = &mut execution => captured.capture(result.map_err(anyhow::Error::from)),
        };
        if result.is_err() {
            owner.fail(None, "native shared execution failed");
        }
        // Child reaped and RPC drained before final same-birth log completion.
        if let Err(error) = owner.finish_physical().await {
            owner.fail(None, &error);
            if result.is_ok() {
                result = Err(error.into());
            }
        }
        helpers.abort_all();
        while let Some(joined) = helpers.join_next().await {
            match joined {
                Err(error) if error.is_cancelled() => {}
                Ok(Err(error)) => {
                    if result.is_ok() {
                        result = Err(error.into());
                    }
                }
                Err(error) => {
                    if result.is_ok() {
                        result = Err(error.into());
                    }
                }
                Ok(Ok(())) => {
                    if result.is_ok() {
                        result = Err(anyhow::anyhow!("native helper ended unexpectedly"));
                    }
                }
            }
        }
        // Drain statistics even after an execution failure. Its missing report
        // must not replace the earlier physical/RPC error, but remains fatal
        // whenever execution otherwise succeeded.
        let source = collector.into_source().await;
        let statistics = source.map_err(anyhow::Error::from).and_then(|source| {
            ensure!(
                statistics_admission.accepted() > 0,
                "native guest submitted no authenticated statistics"
            );
            Ok(source)
        });
        result?;
        let source = statistics?;
        let dispatch = hermit::native_trap_dispatch_stats(&source)?;
        drop(owner);
        captured.require_output()?;
        Ok::<_, anyhow::Error>(dispatch)
    }
    .await;
    // All native helpers and server connections have released their GlobalState
    // Arc. The one ordinary cleanup writes the actual host-input log and INFO
    // report used by strict verification; neither is synthesized here.
    let mut global = Arc::try_unwrap(global)
        .map_err(|_| anyhow::anyhow!("native GlobalState still has live owners after drain"))?;
    if result.is_err() {
        global.force_shutdown_with_error();
        global.cancel_internal_scheduler().await;
    }
    let dispatch = result.as_ref().ok().and_then(|dispatch| dispatch.clone());
    let finalized = global
        .clean_up_with_dispatch_stats(print_summary, &summary_json, dispatch)
        .await;
    // Keep actual files/root/endpoint-directory pins through the cleanup above.
    drop((root, runtime_files, artifacts, _directory_fd, _directory));
    detcore::combine_finalization(result.map(|_| ()), finalized)
}

async fn serve_provider(listener: OwnedFd, owner: Arc<NativeRunOwner>) -> io::Result<()> {
    let listener = tokio::io::unix::AsyncFd::new(listener)?;
    loop {
        let socket = loop {
            let mut ready = listener.readable().await?;
            let raw = unsafe {
                libc::accept4(
                    listener.get_ref().as_raw_fd(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
                )
            };
            if raw >= 0 {
                break unsafe { OwnedFd::from_raw_fd(raw) };
            }
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::WouldBlock {
                ready.clear_ready();
                continue;
            }
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        };
        let socket = tokio::io::unix::AsyncFd::new(socket)?;
        // Keep readiness borrowed through the one-shot nonblocking transfer.
        // This accepted socket is discarded after serve; no retry clears it.
        let _ready = tokio::time::timeout(Duration::from_secs(5), socket.readable())
            .await
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    "native provision request timed out",
                )
            })??;
        owner.provider()?.serve(socket.get_ref())?;
    }
}

/// Test-only reuse of the existing real ELF/dependency discovery fixture.
#[cfg(test)]
pub(super) fn shared_input_declaration_fixture() -> Result<serde_json::Value> {
    tests::bootstrap_rpc_inputs()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Discover inputs only; production Capture still authenticates the full
    /// declared graph against the actual host files and compiled bundle.
    pub(super) fn bootstrap_rpc_inputs() -> Result<serde_json::Value> {
        use std::collections::BTreeMap;
        use std::collections::BTreeSet;
        use std::path::Component;

        fn normalize(path: &Path) -> Result<PathBuf> {
            ensure!(path.is_absolute(), "ELF input path must be absolute");
            let mut result = PathBuf::from("/");
            for component in path.components() {
                match component {
                    Component::RootDir | Component::CurDir => {}
                    Component::Normal(part) => result.push(part),
                    Component::ParentDir => {
                        ensure!(result.pop(), "input symlink escapes root");
                    }
                    Component::Prefix(_) => bail!("non-Linux input path"),
                }
            }
            Ok(result)
        }
        fn resolve(path: &Path, aliases: &mut BTreeMap<PathBuf, PathBuf>) -> Result<PathBuf> {
            let mut pending = normalize(path)?;
            for _ in 0..40 {
                let parts: Vec<_> = pending.components().skip(1).collect();
                let mut prefix = PathBuf::from("/");
                let mut redirect = None;
                for (index, part) in parts.iter().enumerate() {
                    prefix.push(part.as_os_str());
                    if std::fs::symlink_metadata(&prefix)?.file_type().is_symlink() {
                        let target = std::fs::read_link(&prefix)?;
                        if let Some(old) = aliases.insert(prefix.clone(), target.clone()) {
                            ensure!(old == target, "input alias changed during discovery");
                        }
                        let mut next = if target.is_absolute() {
                            target
                        } else {
                            prefix
                                .parent()
                                .context("input alias lacks parent")?
                                .join(target)
                        };
                        for suffix in &parts[index + 1..] {
                            next.push(suffix.as_os_str());
                        }
                        redirect = Some(normalize(&next)?);
                        break;
                    }
                }
                match redirect {
                    Some(next) => pending = next,
                    None => return Ok(pending),
                }
            }
            bail!("input symlink traversal exceeds discovery bound")
        }

        let executables = [PathBuf::from("/usr/bin/nice"), PathBuf::from("/bin/echo")];
        let mut aliases = BTreeMap::new();
        let mut images = Vec::new();
        let mut interpreters = BTreeSet::new();
        for path in &executables {
            resolve(path, &mut aliases)?;
            let bytes = std::fs::read(path)?;
            let elf = goblin::elf::Elf::parse(&bytes)?;
            interpreters.insert(PathBuf::from(
                elf.interpreter
                    .context("dynamic target lacks interpreter")?,
            ));
            images.push(bytes);
        }
        let (runner, runtime) =
            super::super::native_exec_artifacts::bootstrap_rpc_dependency_fixture()?;
        let runtime_name = super::super::native_exec_artifacts::RUNTIME_PATH
            .rsplit(|byte| *byte == b'/')
            .next()
            .unwrap();
        let runner_elf = goblin::elf::Elf::parse(&runner)?;
        ensure!(
            runner_elf
                .libraries
                .iter()
                .any(|name| name.as_bytes() == runtime_name),
            "actual runner lacks compiled runtime dependency"
        );
        interpreters.insert(PathBuf::from(
            runner_elf
                .interpreter
                .context("actual runner lacks interpreter")?,
        ));
        let mut directories: Vec<_> = interpreters
            .iter()
            .map(|path| path.parent().unwrap().to_path_buf())
            .collect();
        directories.extend(
            [
                "/lib64",
                "/usr/lib64",
                "/lib/x86_64-linux-gnu",
                "/usr/lib/x86_64-linux-gnu",
            ]
            .map(PathBuf::from),
        );
        let locate = |name: &str| -> Result<PathBuf> {
            ensure!(
                !name.as_bytes().contains(&b'/'),
                "path-based ELF dependency unsupported in this control"
            );
            directories
                .iter()
                .map(|directory| directory.join(name))
                .find(|path| path.is_file())
                .with_context(|| format!("actual ELF dependency unavailable: {name}"))
        };
        let mut pending: Vec<_> = interpreters.into_iter().collect();
        // Only the runner's strong compiled-runtime dependency is supplied by
        // the artifact owner. Every other dependency is explicitly discovered.
        pending.extend(
            runner_elf
                .libraries
                .iter()
                .filter(|name| name.as_bytes() != runtime_name)
                .map(|name| locate(name))
                .collect::<Result<Vec<_>>>()?,
        );
        images.push(runtime);
        for bytes in images {
            let elf = goblin::elf::Elf::parse(&bytes)?;
            pending.extend(
                elf.libraries
                    .iter()
                    .map(|name| locate(name))
                    .collect::<Result<Vec<_>>>()?,
            );
        }
        let mut loaders = BTreeMap::new();
        while let Some(path) = pending.pop() {
            ensure!(
                loaders.len() < 256,
                "actual loader graph exceeds discovery bound"
            );
            let resolved = resolve(&path, &mut aliases)?;
            if loaders.contains_key(&resolved) {
                continue;
            }
            let bytes = std::fs::read(&path)?;
            let elf = goblin::elf::Elf::parse(&bytes)?;
            pending.extend(
                elf.libraries
                    .iter()
                    .map(|name| locate(name))
                    .collect::<Result<Vec<_>>>()?,
            );
            loaders.insert(resolved, path);
        }
        Ok(serde_json::json!({
            "version": 1,
            "executables": executables.iter().map(|path| path.as_os_str().as_bytes().to_vec()).collect::<Vec<_>>(),
            "loader_files": loaders.values().map(|path| path.as_os_str().as_bytes().to_vec()).collect::<Vec<_>>(),
            "data_files": [],
            "aliases": aliases.iter().map(|(path, target)| serde_json::json!({
                "path": path.as_os_str().as_bytes(), "target": target.as_os_str().as_bytes(),
            })).collect::<Vec<_>>(),
        }))
    }

    #[test]
    fn initial_redemption_preserves_bootstrap_mask_immediate_and_delayed() {
        use std::io::Read;
        use std::io::Seek;
        use std::io::SeekFrom;
        use std::io::Write;

        use clap::CommandFactory;

        let cwd = std::env::current_dir().unwrap();
        assert!(
            !cwd.starts_with("/tmp"),
            "physical native Capture requires a caller-owned uncovered cwd"
        );
        let staging = tempfile::Builder::new()
            .prefix(".bootstrap-rpc-test-")
            .tempdir_in(&cwd)
            .unwrap();
        let declaration = staging.path().join("inputs.json");
        std::fs::write(
            &declaration,
            serde_json::to_vec(&bootstrap_rpc_inputs().unwrap()).unwrap(),
        )
        .unwrap();
        let mut failures = Vec::new();
        for delay_millis in [0, 30] {
            let output = staging.path().join(format!("delay-{delay_millis}"));
            std::fs::create_dir(&output).unwrap();
            let mut report = std::fs::OpenOptions::new()
                .create_new(true)
                .read(true)
                .append(true)
                .open(output.join("observations.jsonl"))
                .unwrap();
            let flags = unsafe { libc::fcntl(report.as_raw_fd(), libc::F_GETFD) };
            assert!(flags >= 0 && flags & libc::FD_CLOEXEC != 0);
            // Isolate the normal CLI's process-global log budget and observer
            // lease. This existing container fixture launches no replacement
            // target: Subcommand retains its own genuine common run owner.
            let isolation = reverie::process::Container::new().run(|| {
                let _lease =
                    super::super::native_exec_coordinator::bootstrap_mask_observation::Lease::arm(
                        report.try_clone().unwrap(),
                        Duration::from_millis(delay_millis),
                    )
                    .unwrap();
                let mut argv: Vec<std::ffi::OsString> = [
                    "hermit",
                    "--backend=in-guest-trap",
                    "--log=info",
                    "--max-log-bytes=16M",
                    "run",
                    "--base-env=minimal",
                    "--strict",
                    "--verify",
                    "--verify-strict",
                    "--max-timeslice=disabled",
                    "--keep-logs",
                    "--workdir=/tmp",
                    "--env=LC_ALL=C",
                    "--env=TZ=UTC",
                    "--env=HOME=/tmp/e2e/home",
                    "--env=XDG_CONFIG_HOME=/tmp/e2e/xdg-config",
                    "--env=E2E_TMPDIR=/tmp/hermit-e2e",
                    "--env=E2E_FIXTURE_DIR=/tmp/e2e/fixtures",
                    "--env=HERMIT_E2E_SCHEDULED_JOBS=1",
                    "--env=TMPDIR=/tmp",
                    "--epoch=2026-10-14T10:08:00.975312300+00:00",
                    "--native-exec-inputs",
                ]
                .map(std::ffi::OsString::from)
                .to_vec();
                argv.push(declaration.clone().into_os_string());
                for (option, path) in [
                    ("--save-config", output.join("config.yaml")),
                    ("--verify-json", output.join("verdict.json")),
                    ("--verify-log-dir", output.join("logs")),
                ] {
                    argv.push(option.into());
                    argv.push(path.into_os_string());
                }
                argv.extend(
                    ["--", "/usr/bin/nice", "-n", "1", "/bin/echo", "nice-ok"]
                        .map(std::ffi::OsString::from),
                );
                let matches = crate::Args::command().try_get_matches_from(argv).unwrap();
                let mut args = crate::args_from_matches_with_clock(&matches, || {
                    panic!("explicit epoch must not capture host time")
                })
                .unwrap();
                crate::tracing::enable_log_cap_exit_bound();
                args.global.prepare_log_budget().unwrap();
                crate::tracing::register_invocation_log_budget(args.global.log_budget());
                args.global.open_log_file().unwrap();
                let result = args.command.main(&args.global);
                // A later initialization/exec/verification error stays explicit.
                // This control asserts Initial redemption, never full parity.
                let record =
                    serde_json::json!({"kind": "command_result", "result": format!("{result:?}")});
                serde_json::to_writer(&report, &record).unwrap();
                (&report).write_all(b"\n").unwrap();
            });
            report.seek(SeekFrom::Start(0)).unwrap();
            let mut bytes = String::new();
            (&mut report)
                .take(1024 * 1024 + 1)
                .read_to_string(&mut bytes)
                .unwrap();
            assert!(
                bytes.len() <= 1024 * 1024,
                "physical observation report exceeds bound"
            );
            eprintln!("bootstrap RPC delay {delay_millis} ms, isolation {isolation:?}:\n{bytes}");
            let rows: Vec<serde_json::Value> = bytes
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            let samples: Vec<_> = rows
                .iter()
                .filter(|row| row["kind"] == "initial_observation")
                .collect();
            if isolation.is_err()
                || samples.is_empty()
                || rows
                    .iter()
                    .filter(|row| row["kind"] == "command_result")
                    .count()
                    != 1
            {
                failures.push(format!(
                    "delay {delay_millis}: real run/report did not reach Initial admission"
                ));
            }
            for sample in samples {
                let exact_mask = 0xffff_ffff_bffb_feffu64;
                if sample["pid"].as_i64().is_none_or(|pid| pid <= 0)
                    || sample["birth"].as_u64().is_none_or(|birth| birth == 0)
                    || sample["delay_millis"] != delay_millis
                    || sample["expected_mask"] != exact_mask
                    || sample["immediate_mask"] != exact_mask
                    || sample["delayed_mask"] != exact_mask
                    || sample["image_mask"] != exact_mask
                    || sample["owner_redeemed"] != true
                    || !sample["owner_error"].is_null()
                {
                    failures.push(format!("delay {delay_millis}: exact physical Bootstrap/owner redemption refused: {sample}"));
                }
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    #[test]
    fn native_endpoint_directory_meets_stats_policy_under_child_umasks() {
        use std::os::unix::fs::MetadataExt;

        struct RefusePeers;
        impl reverie_rpc_transport::ConnectionAdmission for RefusePeers {
            fn admit(&self, _peer: OwnedFd) -> io::Result<reverie_rpc_transport::Admitted> {
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "endpoint setup test admits no guest",
                ))
            }
        }

        for mask in [0o022, 0o000] {
            // Only this real child changes its umask. No test-runner thread or
            // later invocation inherits the deliberately permissive setting.
            reverie::process::Container::new()
                .run(|| {
                    unsafe { libc::umask(mask) };
                    let staging = tempfile::tempdir().unwrap();
                    let endpoints = NativeEndpoints::bind(staging.path()).unwrap();
                    let before = std::fs::metadata(&endpoints.stats_path).unwrap();
                    let directory = std::fs::symlink_metadata(&endpoints.directory_path).unwrap();
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .unwrap();
                    let _entered = runtime.enter();
                    let (_server, _collector) = InitialImageStatsServer::from_prebound(
                        endpoints.stats.try_clone().unwrap(),
                        &endpoints.rpc_path,
                        &endpoints.directory_path,
                        Arc::new(RefusePeers),
                    )
                    .unwrap_or_else(|error| {
                        panic!(
                            "native endpoint directory refused under umask {mask:o}, actual mode {:o}, uid {}, is_directory {}: {error}",
                            directory.mode() & 0o777,
                            directory.uid(),
                            directory.is_dir(),
                        )
                    });
                    assert!(directory.is_dir());
                    assert_eq!(directory.uid(), unsafe { libc::geteuid() });
                    assert_eq!(directory.mode() & 0o777, 0o700);
                    let after = std::fs::metadata(&endpoints.stats_path).unwrap();
                    assert_eq!((after.dev(), after.ino()), (before.dev(), before.ino()));
                })
                .unwrap();
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn native_command_replays_the_retained_pipe_snapshot_for_both_runs() {
        use std::io::Write;

        let mut fds = [-1; 2];
        assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
        // SAFETY: pipe2 returned two fresh owned descriptors.
        let reader = unsafe { File::from_raw_fd(fds[0]) };
        let mut writer = unsafe { File::from_raw_fd(fds[1]) };
        let input = b"native-verification-input\nsecond-line\n";
        writer.write_all(input).unwrap();
        drop(writer);
        hermit::reserve_output_stdin_snapshot(Some(reader)).unwrap();
        for run in 0..2 {
            // A real ordinary child checks the exact native command-setup
            // method. Native frontend/loader admission is a separate control.
            let mut command = Command::new("/bin/cat");
            command
                .stdout(reverie::process::Stdio::piped())
                .stderr(reverie::process::Stdio::piped());
            replay_native_stdin(&mut command).unwrap();
            let child = command.spawn().unwrap();
            let output = tokio::time::timeout(Duration::from_secs(5), child.wait_with_output())
                .await
                .unwrap()
                .unwrap();
            assert!(output.status.success(), "run {run}: {output:?}");
            assert_eq!(output.stdout, input, "run {run}");
            assert!(output.stderr.is_empty(), "run {run}: {output:?}");
        }
    }

    // These controls model only the returned-output/finalization boundary. They
    // use real ordinary child bytes, not a native/root admission or a copied
    // launcher. The production capture and finish methods are exercised here.
    async fn returned_child_output() -> Output {
        let mut command = Command::new("/bin/sh");
        command
            .args([
                "-c",
                r"printf 'actual child stdout\000\377\n'; printf 'actual child stderr\000\376\n' >&2; exit 37",
            ])
            .stdout(reverie::process::Stdio::piped())
            .stderr(reverie::process::Stdio::piped());
        let child = command.spawn().unwrap();
        let output = tokio::time::timeout(Duration::from_secs(5), child.wait_with_output())
            .await
            .expect("finite child output must complete within the test bound")
            .unwrap();
        assert_eq!(output.status.code(), Some(37));
        assert_eq!(output.stdout, b"actual child stdout\0\xff\n");
        assert_eq!(output.stderr, b"actual child stderr\0\xfe\n");
        output
    }

    #[derive(Debug)]
    struct SelectedFailure {
        label: &'static str,
        identity: Arc<()>,
    }

    impl std::fmt::Display for SelectedFailure {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str(self.label)
        }
    }

    impl std::error::Error for SelectedFailure {}

    fn selected_failure(label: &'static str) -> (anyhow::Error, Arc<()>) {
        let identity = Arc::new(());
        let error = anyhow::Error::new(SelectedFailure {
            label,
            identity: identity.clone(),
        })
        .context("original native failure context");
        (error, identity)
    }

    fn assert_selected_failure(error: &anyhow::Error, label: &str, identity: &Arc<()>) {
        assert_eq!(error.to_string(), "original native failure context");
        let cause = error.downcast_ref::<SelectedFailure>().unwrap();
        assert_eq!(cause.label, label);
        assert!(Arc::ptr_eq(&cause.identity, identity));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn captured_child_stderr_survives_native_final_failure() {
        // These are injected finalization results, not claims that each actual
        // native checker executed. Each shares the real production finish site.
        for label in [
            "helper stopped after physical output",
            "physical final admission refused",
            "helper drain failed",
            "statistics refused",
            "dispatch validation refused",
            "global owner unwrap refused",
        ] {
            let mut captured = CapturedNativeOutput::default();
            captured
                .capture(Ok(NativeRunOutput::Output(returned_child_output().await)))
                .unwrap();
            let (error, identity) = selected_failure(label);
            let mut diagnostic = Vec::new();
            let error = captured.finish(Err(error), &mut diagnostic).unwrap_err();
            assert_selected_failure(&error, label, &identity);
            assert_eq!(diagnostic, b"actual child stderr\0\xfe\n", "{label}");
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn captured_child_success_keeps_output_and_does_not_emit() {
        let output = returned_child_output().await;
        let status = output.status;
        let stdout_address = output.stdout.as_ptr();
        let stderr_address = output.stderr.as_ptr();
        let mut captured = CapturedNativeOutput::default();
        captured
            .capture(Ok(NativeRunOutput::Output(output)))
            .unwrap();
        captured.require_output().unwrap();
        let mut diagnostic = Vec::new();
        let output = captured.finish(Ok(()), &mut diagnostic).unwrap();
        assert_eq!(output.status, status);
        assert_eq!(output.stdout, b"actual child stdout\0\xff\n");
        assert_eq!(output.stderr, b"actual child stderr\0\xfe\n");
        assert_eq!(output.stdout.as_ptr(), stdout_address);
        assert_eq!(output.stderr.as_ptr(), stderr_address);
        assert!(diagnostic.is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn native_failure_without_returned_output_preserves_first_error() {
        let label = "execution failed before output was returned";
        let (error, identity) = selected_failure(label);
        let mut captured = CapturedNativeOutput::default();
        let selected_result = captured.capture(Err(error));
        let mut diagnostic = Vec::new();
        let error = captured
            .finish(selected_result, &mut diagnostic)
            .unwrap_err();
        assert_selected_failure(&error, label, &identity);
        assert!(diagnostic.is_empty());

        // Status-only remains a late refusal, not an invented empty Output.
        let status = returned_child_output().await.status;
        let mut captured = CapturedNativeOutput::default();
        captured
            .capture(Ok(NativeRunOutput::Status(status)))
            .unwrap();
        let late_result = captured.require_output();
        let error = captured.finish(late_result, &mut diagnostic).unwrap_err();
        assert_eq!(
            error.to_string(),
            "native capture returned status without output"
        );
        assert!(diagnostic.is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn native_failure_stderr_sink_errors_do_not_replace_original_error() {
        struct RefusingSink {
            partial_first_write: bool,
            error: io::ErrorKind,
            calls: usize,
            accepted: Vec<u8>,
        }

        impl io::Write for RefusingSink {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.calls += 1;
                if self.partial_first_write && self.calls == 1 {
                    let amount = 3.min(bytes.len());
                    self.accepted.extend_from_slice(&bytes[..amount]);
                    return Ok(amount);
                }
                Err(io::Error::new(
                    self.error,
                    "injected diagnostic sink refusal",
                ))
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        for (partial_first_write, kind) in [
            (false, io::ErrorKind::BrokenPipe),
            (false, io::ErrorKind::WouldBlock),
            (true, io::ErrorKind::BrokenPipe),
        ] {
            let mut captured = CapturedNativeOutput::default();
            captured
                .capture(Ok(NativeRunOutput::Output(returned_child_output().await)))
                .unwrap();
            let label = "selected failure survives diagnostic error";
            let (error, identity) = selected_failure(label);
            let mut sink = RefusingSink {
                partial_first_write,
                error: kind,
                calls: 0,
                accepted: Vec::new(),
            };
            let error = captured.finish(Err(error), &mut sink).unwrap_err();
            assert_selected_failure(&error, label, &identity);
            assert_eq!(sink.calls, if partial_first_write { 2 } else { 1 });
            let expected: &[u8] = if partial_first_write { b"act" } else { b"" };
            assert_eq!(sink.accepted, expected);
        }
    }
}
