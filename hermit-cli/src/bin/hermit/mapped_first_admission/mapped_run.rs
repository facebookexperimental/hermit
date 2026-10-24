//! Intermediate opt-in execution through the one retained physical root.
//! Full landing qualification remains separate. No supplied image CLI option.
use std::ffi::OsString;
use std::fs::File;
use std::io;
use std::os::fd::AsFd;
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileExt;
use std::os::unix::net::UnixStream;
use std::panic::AssertUnwindSafe;
use std::panic::catch_unwind;
use std::panic::resume_unwind;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use detcore::detlog::NativePhysicalWatchIdentity;
use reverie::ExitStatus;
use reverie::GlobalTool;
use reverie::process::Command;
use reverie_inguest::launcher::exec_stop::CleanupReceipt;
use reverie_inguest::launcher::exec_stop::CopiedInputRequest;
use reverie_inguest::launcher::exec_stop::ExecStop;
use reverie_inguest::launcher::exec_stop::PreparedExec;
use reverie_inguest::launcher::exec_stop::physical::BootstrapEntryCoordinates;
use reverie_inguest::launcher::exec_stop::physical::Preparation;
use reverie_inguest::launcher::exec_stop::register_words;

use super::packet;
use super::runtime_binding::RetainedImage;
use super::runtime_support::init_trial::Trial;
use super::runtime_support::init_trial::hello_consumer::HelloConsumer;
use super::runtime_support::*;

/// # Safety
/// This is the intermediate bounded valid-path experiment, not a landing
/// qualifier. The internally bound image must implement the real phase4
/// runtime and reviewed entry/handover contracts. Original ptracer affinity,
/// stable same-host XState domain, inherited leases and external whole-tree
/// containment remain mandatory. No arbitrary image data can construct that
/// execution permission. The caller keeps the retained image through return.
pub(crate) unsafe fn run(
    command: Command,
    config: detcore::Config,
    retained: &RetainedImage,
    records: detcore::detlog::ForwardedRecordSink,
    print_summary: bool,
    summary_json: &Option<std::path::PathBuf>,
) -> anyhow::Result<ExitStatus> {
    // SAFETY: this wrapper preserves the existing caller's exact contract.
    unsafe {
        run_inner(
            command,
            config,
            retained,
            records,
            print_summary,
            summary_json,
            #[cfg(all(test, feature = "mm-cancellation-test-support"))]
            None,
        )
    }
}

unsafe fn run_inner(
    command: Command,
    config: detcore::Config,
    retained: &RetainedImage,
    records: detcore::detlog::ForwardedRecordSink,
    print_summary: bool,
    summary_json: &Option<std::path::PathBuf>,
    #[cfg(all(test, feature = "mm-cancellation-test-support"))] mut cancellation: Option<
        &mut mm_cancel::Cancellation,
    >,
) -> anyhow::Result<ExitStatus> {
    // Resolve with the ordinary Command's current directory and exact PATH.
    let program = command.find_program()?;
    let arguments: Vec<OsString> = std::iter::once(command.get_arg0().to_owned())
        .chain(command.get_args().map(ToOwned::to_owned))
        .collect();
    let environment: Vec<_> = command.get_captured_envs().into_iter().collect();
    let cwd = command.get_current_dir().map(ToOwned::to_owned);
    need(
        packet::config_violation(&config).is_none(),
        "mapped Config outside existing ordinary domain",
    )?;
    let observed = retained.observed();
    let image_bytes = retained.bytes();
    let (image, descriptor) = checked_image(image_bytes, observed).map_err(rpc)?;
    let placement = retained.placement()?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let global = Arc::new(runtime.block_on(
        <detcore::GlobalState as GlobalTool>::init_global_state(&config),
    ));
    let directory = tempfile::tempdir()?;
    let directory_fd = File::open(directory.path())?;
    // A tempdir inherits the process umask. Set the endpoint's required mode
    // on our retained directory descriptor before starting either listener.
    {
        use std::os::unix::fs::PermissionsExt;
        directory_fd.set_permissions(std::fs::Permissions::from_mode(0o700))?;
    }
    {
        use std::os::unix::fs::MetadataExt;
        let m = std::fs::symlink_metadata(directory.path())?;
        need(
            m.is_dir() && m.uid() == unsafe { libc::geteuid() } && m.mode() & 0o777 == 0o700,
            &format!(
                "actual run directory type={} mode={:o} uid={} euid={}",
                m.is_dir(),
                m.mode(),
                m.uid(),
                unsafe { libc::geteuid() }
            ),
        )?;
    }
    let rpc_path = std::path::PathBuf::from(format!(
        "/proc/{}/fd/{}/rpc.sock",
        std::process::id(),
        directory_fd.as_raw_fd()
    ));
    let lease = super::packet_lease::PendingPacketLease::new()?;
    let runtime_file = memfd_bytes(image_bytes, true)?;
    let handoff_file = memfd_bytes(&[], false)?;
    let (sender, receiver) = crate::run::RunOpts::open_forwarded_detlog_channel()?
        .ok_or_else(|| rpc("actual DETLOG channel absent"))?;
    let sender = UnixStream::from(sender);
    let receiver = UnixStream::from(receiver);
    receiver.set_nonblocking(true)?;
    let mut trial = Trial::register_with_sink(receiver.as_fd().try_clone_to_owned()?, records);
    // Independent SEQPACKET observer; the original DETLOG collector never
    // receives binary diagnostic frames. Both actual sender OFDs are retained.
    let (observer_sender, observer_receiver) =
        crate::run::RunOpts::open_forwarded_detlog_channel()?
            .ok_or_else(|| rpc("actual observer channel absent"))?;
    let observer_sender = UnixStream::from(observer_sender);
    let mut observer_receiver = Some(observer_receiver);
    let mut observer = None;
    let mut observer_task = None;
    let mut observer_joined = false;
    let mut hello = None;
    let mut admissions = Vec::new();
    // Bundle precedes borrowing physical holders. None of these owns an async
    // task/future; every exit/unwind reaches the existing guarded retirement.
    let mut bundle = None;
    let mut observed_pid = None;
    let mut physical = RetainedPhysical::empty(CleanupReceipt::default());
    let mut unused = RetainedPhysical::empty(CleanupReceipt::default());
    let mut panic_payload = None;
    let setup = catch_unwind(AssertUnwindSafe(|| -> io::Result<_> {
        let request = PreparedExec::new(
            &program,
            &arguments,
            &environment,
            cwd.as_deref(),
            vec![
                lease.inherited_fd(),
                sender.as_fd(),
                observer_sender.as_fd(),
                runtime_file.as_fd(),
                handoff_file.as_fd(),
            ],
        )
        .map_err(rpc)?
        .with_ptrace_launch_facts();
        physical.retain_spawn(ExecStop::spawn(request, physical.receipt.clone()));
        let stop = physical.initial()?;
        observed_pid = Some(stop.pid().as_raw());
        #[cfg(all(test, feature = "mm-cancellation-test-support"))]
        if let Some(cancel) = cancellation.as_mut() {
            cancel.arm(&global, stop.pid().as_raw()).map_err(rpc)?;
        }
        private_resource_diagnostic(stop.pid().as_raw(), "exec-stop");
        let captured = stop.capture_initial_inputs().map_err(rpc)?;
        let admission = runtime.block_on(admit_with_observer(
            &mut admissions,
            &global,
            &config,
            &rpc_path,
            stop,
            &sender,
            &observer_sender,
        ))?;
        observer = Some(crate::native_exec_observer::Observer::new(
            observer_receiver
                .take()
                .ok_or_else(|| rpc("observer receiver already consumed"))?,
            admission.observer_watch(),
        )?);
        let sealed = lease.fill(stop, &admission)?;
        exact_storage(&sealed, &admission.initial_packet()?)?;
        sealed.verify(stop)?;
        let control = placement.roles[4]
            .start
            .checked_add(PAGE)
            .ok_or_else(|| rpc("control overflow"))?;
        let relay = aligned(
            control
                .checked_add(descriptor[6])
                .ok_or_else(|| rpc("relay overflow"))?,
            descriptor[9],
        )
        .map_err(rpc)?;
        let address = placement.roles[3]
            .start
            .checked_add(PAGE)
            .ok_or_else(|| rpc("handoff address"))?;
        let copied_address = address
            .checked_add(2 * PAGE)
            .ok_or_else(|| rpc("copied address"))?;
        let copied_capacity = usize::try_from(
            placement.roles[3]
                .end
                .checked_sub(PAGE)
                .and_then(|end| end.checked_sub(copied_address))
                .ok_or_else(|| rpc("copied capacity"))?,
        )
        .map_err(rpc)?;
        let copied = stop
            .prepare_copied_inputs(
                &captured,
                &image,
                &CopiedInputRequest {
                    roles: placement.roles,
                    free_band: placement.free_band,
                    layout_address: observed.layout_address,
                    copied_address,
                    copied_capacity,
                    control_address: control,
                    relay_address: relay,
                    filter_scratch: placement.filter_scratch,
                },
            )
            .map_err(rpc)?;
        // SAFETY: actual stopped ptrace XState on the same stable host domain.
        let pkru = unsafe { stop.entry().xstate().pkru() }.map_err(rpc)?;
        let wire = PacketMappingWire::build(
            &image,
            WireInputs {
                runtime: image_bytes,
                observed,
                roles: &placement.roles,
                descriptor: &descriptor,
                reference: observed.reference_address,
                copied: &copied,
                pkru,
                free_band: placement.free_band,
                packet: &sealed,
            },
            Context {
                pid: stop.pid().as_raw() as u64,
                regs: register_words(stop.entry().regs()),
                mask: stop.entry().signal_mask(),
                input_fd: sealed.fd().as_raw_fd() as u64,
            },
        )
        .map_err(rpc)?;
        handoff_file.write_all_at(&wire.bytes, 0)?;
        handoff_file.set_len(wire.bytes.len() as u64)?;
        handoff_file.sync_all()?;
        seal_memfd(&handoff_file)?;
        Ok((wire, sealed, admission))
    }));
    let mut primary = match setup {
        Ok(Ok(value)) => {
            bundle = Some(value);
            Ok(None)
        }
        Ok(Err(error)) => Err(error),
        Err(payload) => {
            panic_payload = Some(payload);
            Err(rpc("mapped setup panicked; retained cleanup follows"))
        }
    };
    if let Some((wire, sealed, admission)) = bundle.as_ref() {
        let outcome = catch_unwind(AssertUnwindSafe(|| -> io::Result<ExitStatus> {
            // SAFETY: same original held birth and complete retained leases.
            unsafe {
                physical.prepare(Preparation {
                    runtime_fd: runtime_file.as_fd(),
                    runtime_bytes: image_bytes,
                    layout_address: observed.layout_address,
                    handoff_fd: handoff_file.as_fd(),
                    handoff_bytes: &wire.bytes,
                    handoff_address: wire.address,
                    input_fd: lease.inherited_fd(),
                    free_band: placement.free_band,
                    filter_scratch: placement.filter_scratch,
                })
            }?;
            sealed.verify_prepared(physical.mapped()?)?;
            if let Some(pid) = observed_pid {
                private_resource_diagnostic(pid, "mapped-before-init");
            }
            exact_storage(sealed, &admission.initial_packet()?)?;
            need(
                !admission.failed.load(Ordering::Acquire)
                    && !admission.initial_used.load(Ordering::Acquire),
                "mapped admission failed or consumed before initialization",
            )?;
            runtime.block_on(async {
                observer_task = Some(tokio::spawn(
                    observer
                        .as_ref()
                        .ok_or_else(|| rpc("actual observer absent"))?
                        .clone()
                        .serve(),
                ));
                trial.start(
                    admissions
                        .first_mut()
                        .ok_or_else(|| rpc("actual admission absent"))?,
                    &directory_fd,
                )?;
                // Both real listeners serve before the first initializer instruction.
                hello = Some(HelloConsumer::bind(
                    &trial,
                    &rpc_path,
                    directory.path(),
                    &directory_fd,
                )?);
                let mapped = physical
                    .mapped
                    .as_mut()
                    .ok_or_else(|| rpc("actual mapping absent"))?;
                let coordinates = BootstrapEntryCoordinates {
                    entry: retained.entry(),
                    terminal_int3: retained.terminal_int3(),
                };
                let execution = async {
                    // SAFETY: exact internally retained intermediate source/image
                    // binding, same owner, actual signal relay and genuine RPC.
                    #[cfg(all(test, feature = "mm-cancellation-test-support"))]
                    if let Some(cancel) = cancellation.as_mut() {
                        // The observer owns only a borrow. Dropping this select's
                        // losing future retains the actual physical root outside it.
                        let observed = unsafe { trial.observe_at(mapped, &config, coordinates) };
                        tokio::pin!(observed);
                        tokio::select! {
                            pause = cancel.reached() => {
                                cancel.retain_pause(pause.map_err(rpc)?);
                                return Err(io::Error::other(mm_cancel::CancellationRequested));
                            }
                            result = &mut observed => {
                                return Err(rpc(format!("initializer ended before MM checkpoint: {result:?}")));
                            }
                            _ = tokio::time::sleep(WAIT) => {
                                return Err(rpc("MM checkpoint not reached before bound"));
                            }
                        }
                    }
                    unsafe { trial.observe_at(mapped, &config, coordinates).await }?;
                    if let Some(pid) = observed_pid {
                        private_resource_diagnostic(pid, "initialized-before-guest");
                    }
                    // No owner, source, FD lease or backing is moved into this future.
                    // Explicit diagnostic build only; logging cannot select a
                    // counter or replace normal detached production handover.
                    #[cfg(feature = "loader-rcb-observation")]
                    {
                        let rcb_coordinates = retained.rcb_observation_coordinates();
                        // SAFETY: the retained, qualified package supplies exact
                        // same-image continuation DATA; this finite experiment additionally
                        // requires one-thread hello and no asynchronous handlers.
                        let result = unsafe {
                            hello.as_mut()
                                .ok_or_else(|| rpc("actual hello services absent"))?
                                .handover_rcb_status(&trial, mapped, &rcb_coordinates)
                                .await
                        };
                        // Keep partial data even on failure. Diagnostic stderr is
                        // separate from raw guest stdout and strict INFO. Never
                        // replace the physical error with a receipt I/O failure.
                        if let Some(observation) = mapped.rcb_observation() {
                            use std::io::Write as _;
                            let _ = writeln!(std::io::stderr().lock(),
                                "RCB_OBSERVATION_DATA={observation:#?}");
                        }
                        result?.terminal.ok_or_else(|| rpc("RCB observer lacks actual terminal status"))
                    }
                    #[cfg(not(feature = "loader-rcb-observation"))]
                    {
                        unsafe {
                            hello.as_mut()
                                .ok_or_else(|| rpc("actual hello services absent"))?
                                .handover_status(&trial, mapped)
                                .await
                        }
                    }
                };
                tokio::pin!(execution);
                let task = observer_task
                    .as_mut()
                    .ok_or_else(|| rpc("observer task absent"))?;
                tokio::select! {
                    result = &mut execution => result,
                    result = task => {
                        observer_joined = true;
                        result.map_err(rpc)?.map_err(rpc)?;
                        // EOF can race the actual terminal wait. It never
                        // authorizes completion; await that same physical owner.
                        execution.await
                    }
                }
            })
        }));
        primary = match outcome {
            Ok(status) => status.map(Some),
            Err(payload) => {
                panic_payload = Some(payload);
                Err(rpc(
                    "mapped observation/handover panicked; retained cleanup follows",
                ))
            }
        };
    }
    // Existing original-owner cleanup runs even after normal Guest exit. Its
    // actual receipt confirms reap before any borrowed resources are released.
    cleanup_all(&mut physical, &mut unused, &mut panic_payload);
    let mut errors = Vec::new();
    if primary.is_err()
        && physical.receipt.snapshot().confirmed
        && let Some(admission) = admissions.first()
    {
        // Every backing, FD, admission, physical receipt and (test) Pause
        // still lives. Error cleanup never resumes a reply to a dead peer.
        let cancelled = retain_unwind(&mut panic_payload, || {
            // Intent is synchronous: no watcher/RPC task can be driven by
            // re-entering this current-thread runtime before it is visible.
            if trial.cancel_reaped_requests(&physical.receipt, &admission.owner)? {
                runtime
                    .block_on(trial.join_reaped_requests(&physical.receipt, &admission.owner))?;
            }
            Ok::<(), io::Error>(())
        });
        match cancelled {
            Some(Ok(_)) => {}
            Some(Err(error)) => errors.push(format!("actual owned RPC cancellation: {error}")),
            None => errors.push("actual owned RPC cancellation panicked".to_owned()),
        }
    }
    #[cfg(all(test, feature = "mm-cancellation-test-support"))]
    if let Some(cancel) = cancellation.as_mut() {
        // Every owner/backing/lease still lives. Even a missing checkpoint or
        // assertion/error path retires the child before unblocking the handler.
        cancel.after_reap(&runtime, &physical.receipt, &admissions, &global, &trial);
    }
    if primary.is_ok() && !physical.receipt.snapshot().confirmed {
        errors.push("physical reap unconfirmed".to_owned());
    }
    drop(physical);
    drop(unused);
    drop(bundle);
    drop(sender);
    drop(observer_sender);
    let mut dispatch = None;
    let closed = retain_unwind(&mut panic_payload, || {
        runtime.block_on(async {
            if let Some(observer) = observer.as_ref() {
                if primary.is_ok() {
                    let result = async {
                        let admission = &admissions
                            .first()
                            .ok_or_else(|| rpc("observer admission absent"))?
                            .owner;
                        let completion = admission.observer_completion().await?;
                        observer.finish(&completion)
                    }
                    .await;
                    if let Err(error) = result {
                        errors.push(format!("actual observer completion: {error}"));
                    }
                }
                // Preserve every authenticated frame even on an independent
                // execution/close error. These are diagnostics, never INFO.
                match observer.observed_bytes() {
                    Ok(bytes) => {
                        use sha2::Digest as _;
                        let (frames, partial) = bytes.as_chunks::<96>();
                        if !partial.is_empty() {
                            errors.push("actual observer evidence has a partial frame".to_owned());
                        }
                        tracing::debug!(
                            "MAPPED_OBSERVER_EVIDENCE count={} bytes={} sha256={:x}",
                            frames.len(),
                            bytes.len(),
                            sha2::Sha256::digest(&bytes)
                        );
                        for frame in frames {
                            let hex: String =
                                frame.iter().map(|byte| format!("{byte:02x}")).collect();
                            tracing::debug!("MAPPED_OBSERVER_FRAME {hex}");
                        }
                    }
                    Err(error) => errors.push(format!("actual observer evidence: {error}")),
                }
            }
            if let Some(task) = observer_task.take() {
                // A completed task was already observed by the select branch;
                // polling it again would be invalid. Otherwise abort and join.
                if !observer_joined {
                    task.abort();
                    match task.await {
                        Err(error) if error.is_cancelled() => {}
                        Err(error) => errors.push(format!("observer join: {error}")),
                        Ok(Err(error)) => errors.push(format!("observer receive: {error}")),
                        Ok(Ok(())) => {}
                    }
                }
            }
            if let Some(hello) = hello.as_mut() {
                match hello.close(&mut trial, primary.is_ok()).await {
                    Ok(reports) => {
                        if primary.is_ok() && reports.detlog.is_empty() {
                            errors.push("actual complete DETLOG empty".to_owned());
                        }
                        if let Some(slot) = admissions
                            .iter_mut()
                            .find(|slot| Arc::ptr_eq(&slot.owner, &reports.drained))
                        {
                            slot.drained = true;
                        } else {
                            errors.push("closed RPC lacks retained actual admission".to_owned());
                        }
                        // A killed initializer has no successful Guest report.
                        // Service/collector errors above remain fatal on both
                        // paths; only normal completion may publish statistics.
                        // Failure keeps its original error and dispatch=None.
                        if primary.is_ok() {
                            match hermit::native_trap_dispatch_stats(&reports.source) {
                                Ok(record) => dispatch = record,
                                Err(error) => {
                                    errors.push(format!("actual dispatcher statistics: {error}"))
                                }
                            }
                        }
                    }
                    Err(error) => errors.push(format!("actual hello services: {error}")),
                }
            } else {
                // Setup/start failure still unregisters/drains the actual source.
                if let Err(error) = trial.close().await {
                    errors.push(format!("actual setup services: {error}"));
                }
            }
            for slot in &mut admissions {
                if primary.is_err() {
                    slot.owner.fail(&"actual mapped execution failed");
                }
                if !slot.drained {
                    match slot.owner.exit_admission().drain(WAIT).await {
                        Ok(()) => slot.drained = true,
                        Err(error) => errors.push(format!("actual watch drain: {error:?}")),
                    }
                }
                if slot.drained
                    && slot.owner.exit_admission().reported_exits()
                        != [slot.owner.watched_process().process_id()]
                {
                    errors.push("watch did not report actual child exactly once".to_owned());
                }
                if slot
                    .issues
                    .as_ref()
                    .is_some_and(|issues| !issues.snapshot().is_empty())
                {
                    errors.push("actual RPC issues retained".to_owned());
                }
            }
        })
    });
    if closed.is_none() {
        errors.push("actual service cleanup panicked".to_owned());
    }
    // Last sender is gone only after physical borrows ended. This is the real
    // inherited socket, not a byte-count substitute for EOF.
    let eof = retain_unwind(&mut panic_payload, || {
        let _enter = runtime.enter();
        let mut receiver = tokio::net::UnixStream::from_std(receiver)?;
        runtime.block_on(bounded(async {
            use tokio::io::AsyncReadExt;
            let mut byte = [0];
            need(
                receiver.read(&mut byte).await? == 0,
                "actual DETLOG not EOF",
            )
        }))
    });
    match eof {
        Some(Ok(())) => {}
        Some(Err(error)) => errors.push(format!("actual DETLOG EOF: {error}")),
        None => errors.push("DETLOG drain panicked".to_owned()),
    }
    #[cfg(all(test, feature = "mm-cancellation-test-support"))]
    if let Some(cancel) = cancellation.as_mut() {
        cancel.capture_state(&runtime, &trial, &admissions, &global);
    }
    drop(observer);
    drop(hello);
    drop(trial);
    drop(admissions);
    if let Err(error) = retained.verify() {
        errors.push(format!("actual runtime source closure: {error}"));
    }
    // Release all server/admission Arcs before the one ordinary GlobalState
    // cleanup writes real INFO/host inputs. No synthetic summary is created.
    let finalization = retain_unwind(&mut panic_payload, || -> anyhow::Result<()> {
        let mut global = Arc::try_unwrap(global)
            .map_err(|_| anyhow::anyhow!("mapped GlobalState still owned after drain"))?;
        runtime.block_on(async {
            if primary.is_err() || !errors.is_empty() {
                global.force_shutdown_with_error();
                global.cancel_internal_scheduler().await;
            }
            global
                .clean_up_with_dispatch_stats(print_summary, summary_json, dispatch)
                .await
        })
    });
    // Keep the typed error out of the string-only cleanup diagnostics. The
    // latter remain the original custody/cleanup verdict when already present.
    let finalization = finalization.unwrap_or_else(|| {
        Err(anyhow::anyhow!(
            "global finalization panicked; original panic retained"
        ))
    });
    #[cfg(all(test, feature = "mm-cancellation-test-support"))]
    let finalization_message = finalization
        .as_ref()
        .err()
        .map(|e| format!("actual global finalization: {e:#}"));
    #[cfg(all(test, feature = "mm-cancellation-test-support"))]
    if let Some(cancel) = cancellation.as_mut() {
        // After retained verification, Arc unwrap and finalization: no later
        // cleanup Result may escape the control's complete error custody.
        let mut complete_errors = errors.clone();
        if let Some(message) = &finalization_message {
            complete_errors.push(message.clone());
        }
        cancel.finish_outcome(&primary, &complete_errors);
    }
    if let Some(payload) = panic_payload {
        resume_unwind(payload);
    }
    if !errors.is_empty() {
        return detcore::combine_finalization(
            Err(anyhow::anyhow!(
                "mapped cleanup={errors:?}; execution={primary:?}"
            )),
            finalization,
        );
    }
    let primary = primary.map_err(anyhow::Error::from).and_then(|status| {
        status.ok_or_else(|| anyhow::anyhow!("no actual mapped terminal status"))
    });
    detcore::combine_finalization(primary, finalization)
}

// Read-only intermediate evidence while the original physical owner retains
// the stopped child. No diagnostic result changes admission or guest state.
// This must not read environ or authorize reuse of a numeric PID.
fn private_resource_diagnostic(pid: i32, phase: &str) {
    use std::io::Read;
    let captured = (|| -> io::Result<(String, Vec<(u32, std::path::PathBuf)>)> {
        let root = std::path::PathBuf::from(format!("/proc/{pid}"));
        let mut maps = String::new();
        File::open(root.join("maps"))?
            .take(1024 * 1024 + 1)
            .read_to_string(&mut maps)?;
        if maps.len() > 1024 * 1024 {
            return Err(io::Error::other("diagnostic maps bound exceeded"));
        }
        let mut fds = Vec::new();
        for entry in std::fs::read_dir(root.join("fd"))? {
            if fds.len() == 1024 {
                return Err(io::Error::other("diagnostic descriptor bound exceeded"));
            }
            let entry = entry?;
            let fd = entry
                .file_name()
                .to_str()
                .and_then(|s| s.parse::<u32>().ok())
                .ok_or_else(|| io::Error::other("diagnostic descriptor name"))?;
            fds.push((fd, std::fs::read_link(entry.path())?));
        }
        fds.sort_by_key(|(fd, _)| *fd);
        Ok((maps, fds))
    })();
    tracing::debug!(phase, pid, resources = ?captured, "MAPPED_PRIVATE_RESOURCE_EVIDENCE");
}

#[cfg(all(test, feature = "mm-cancellation-test-support"))]
pub(super) mod mm_cancel;
