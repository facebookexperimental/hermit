/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 * This source code is licensed under the BSD-style license found in LICENSE.
 */

//! One host admission owner for the shared in-guest native chain. It retains
//! actual filesystem, artifact, socket and physical-watch objects; incoming
//! frames supply lookup keys, never those authorities. No Tool is decoded or
//! run on this side of the RPC connection.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::io;
use std::os::fd::AsRawFd;
use std::os::fd::BorrowedFd;
use std::os::fd::OwnedFd;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::Weak;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;

use detcore::detlog::NativeForwardedSourceReceipt;
use detcore::detlog::NativePhysicalWatchIdentity;
use detcore::native_exec_control::ExecTicket;
use detcore::native_exec_control::NativeExecBinding;
use detcore::native_exec_control::NativeExecProof;
use detcore::native_exec_control::NativeExecRegistry;
use detcore::native_exec_control::PrepareNativeExec;
use detcore::types::DetTid;
use detcore_model::liteinst_native_exec_runtime as runtime_packet;
use hermit::in_guest_exits::InGuestExitAdmission;
use hermit::in_guest_exits::NativeWatchedProcess;
use hermit::physical_exit_watch;
use reverie::GlobalTool;
use reverie_inguest::launcher::native::NativeRootObservation;
use reverie_inguest::launcher::native::NativeStartAdmission;
use reverie_rpc_transport::Admitted;
use reverie_rpc_transport::ConnectionAdmission;
use reverie_rpc_transport::ExitReporter;
use reverie_rpc_transport::initial_image::InitialImageAdmission;
use reverie_rpc_transport::initial_image::config_digest;
use reverie_rpc_transport::successor::NativeImageConnection;
use reverie_rpc_transport::successor::SuccessorAdmission;
use reverie_rpc_transport::successor::SuccessorRequest;

use super::native_exec_artifacts::QualifiedNativeArtifacts;
use super::native_exec_inputs::GuestPath;
use super::native_exec_inputs::HeldDynamicInputs;
use super::native_exec_inputs::InputRole;
use super::native_exec_maps::Image;
use super::native_exec_maps::RootIdentity;
use super::native_exec_maps::WatchedRoot;
use super::native_exec_observer::Entry;
use super::native_exec_observer::Observer;
use super::native_exec_provider::Provider;
use super::native_exec_provider::ProviderBindings;

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}
fn error(error: impl std::fmt::Display) -> io::Error {
    invalid(error.to_string())
}
fn alive(fd: BorrowedFd<'_>) -> io::Result<()> {
    let mut poll = libc::pollfd {
        fd: fd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    if unsafe { libc::poll(&mut poll, 1, 0) } != 0 || poll.revents != 0 {
        return Err(invalid("native watched birth is no longer live"));
    }
    Ok(())
}
fn entropy() -> io::Result<[u8; 32]> {
    let mut bytes = [0; 32];
    let mut done = 0;
    while done < bytes.len() {
        let n =
            unsafe { libc::getrandom(bytes[done..].as_mut_ptr().cast(), bytes.len() - done, 0) };
        if n < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if n == 0 {
            return Err(invalid("native launch nonce source returned EOF"));
        }
        done += n as usize;
    }
    if bytes == [0; 32] {
        return Err(invalid("native launch nonce was empty"));
    }
    Ok(bytes)
}

/// Exact raw-kernel masks at the two real native image phases. Initial and
/// successor RPC redemption precede installation. Installation unblocks the
/// actual Tool's reserved signals; signals 32/33 retain their raw kernel bits.
#[derive(Clone, Copy)]
struct NativeSignalMasks {
    installed: u64,
}
impl NativeSignalMasks {
    const BOOTSTRAP: u64 = !((1u64 << (libc::SIGSYS - 1))
        | (1u64 << (libc::SIGKILL - 1))
        | (1u64 << (libc::SIGSTOP - 1)));

    fn from_config(config: &detcore::Config) -> Self {
        Self::from_subscriptions(&<detcore::Detcore as reverie::Tool>::subscriptions(config))
    }
    fn from_subscriptions(subscriptions: &reverie::Subscription) -> Self {
        let instructions = subscriptions.has_cpuid() || subscriptions.has_rdtsc();
        let reserved = (1u64 << (libc::SIGSYS - 1))
            | if instructions {
                1u64 << (libc::SIGSEGV - 1)
            } else {
                0
            };
        Self {
            installed: Self::BOOTSTRAP & !reserved,
        }
    }
    fn require_bootstrap(&self, image: &Image) -> io::Result<()> {
        if image.observed_mask != Self::BOOTSTRAP {
            return Err(invalid(format!(
                "native redemption lacks the exact bootstrap signal mask: expected={:016x} observed={:016x} changed={:016x}",
                Self::BOOTSTRAP,
                image.observed_mask,
                Self::BOOTSTRAP ^ image.observed_mask,
            )));
        }
        Ok(())
    }
    fn require_completion(&self, redeemed: &Image, current: &Image) -> io::Result<()> {
        self.require_bootstrap(redeemed)?;
        let mut expected = redeemed.clone();
        expected.observed_mask = self.installed;
        if current != &expected {
            return Err(invalid(
                "native completion image/root or installed signal mask changed",
            ));
        }
        Ok(())
    }
}

struct Redeemed {
    peer: OwnedFd,
    image: Image,
    binding: NativeExecBinding,
}
#[derive(Default)]
struct ProofState {
    old_entry: Option<Entry>,
    redeemed: BTreeMap<ExecTicket, Redeemed>,
    completed: BTreeSet<ExecTicket>,
}
struct Proof {
    watch: Arc<NativeWatchedProcess>,
    root: Arc<WatchedRoot>,
    observer: Arc<Observer>,
    inputs: Arc<HeldDynamicInputs>,
    artifacts: Arc<QualifiedNativeArtifacts>,
    initial_used: Arc<AtomicBool>,
    failed: Arc<AtomicBool>,
    state: Mutex<ProofState>,
    signal_masks: NativeSignalMasks,
    socket_objects: [(u64, u64); 2],
}
impl std::fmt::Debug for Proof {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeWatchedProof")
            .field("pid", &self.watch.process_id())
            .field("birth", &self.watch.birth_identity())
            .finish_non_exhaustive()
    }
}
impl Proof {
    fn peer(&self, peer: &OwnedFd) -> io::Result<()> {
        use std::os::fd::AsFd;
        alive(self.watch.retained_pidfd())?;
        alive(peer.as_fd())?;
        if self.failed.load(Ordering::Acquire)
            || !self.watch.watch_is_healthy()
            || physical_exit_watch::birth_identity(peer) != Some(self.watch.birth_identity())
        {
            return Err(invalid(
                "native peer/watch/source belongs to a different or failed birth",
            ));
        }
        Ok(())
    }
    fn selected_original_target(&self, entry: &Entry) -> io::Result<[u8; 32]> {
        self.root.verify()?;
        let pointer = match entry.number {
            libc::SYS_execve => entry.arguments[0],
            libc::SYS_execveat
                if entry.arguments[0] == libc::AT_FDCWD as i64 as u64
                    && entry.arguments[4] == 0 =>
            {
                entry.arguments[1]
            }
            _ => {
                return Err(invalid(
                    "native original exec operation is outside the declared domain",
                ));
            }
        };
        let path = read_original_path(self.watch.process_id(), pointer)?;
        let path = GuestPath::new(path).map_err(error)?;
        let resolved = self.inputs.resolve_path(&path).map_err(error)?;
        let target = self
            .inputs
            .files()
            .iter()
            .find(|file| file.role == InputRole::Executable && file.resolved_path == resolved)
            .ok_or_else(|| invalid("original exec target was not declared"))?;
        self.inputs.recheck().map_err(error)?;
        self.root.verify()?;
        Ok(target.identity.digest)
    }
    fn complete_mapping(&self, ticket: ExecTicket, binding: NativeExecBinding) -> io::Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| invalid("native proof state poisoned"))?;
        if state.completed.contains(&ticket) {
            return Err(invalid("native completion repeated"));
        }
        let redeemed = state
            .redeemed
            .get(&ticket)
            .ok_or_else(|| invalid("native completion lacks an accepted successor"))?;
        if redeemed.binding != binding {
            return Err(invalid("native completion binding changed"));
        }
        self.peer(&redeemed.peer)?;
        let current = self.root.image(&redeemed.peer, binding.target_digest)?;
        self.signal_masks
            .require_completion(&redeemed.image, &current)?;
        // The qualified consumer calls this only after the real post_exec and
        // frontend restoration, before publishing ready. Maps alone never
        // establish that ordering or attest a kernel exec.
        state.completed.insert(ticket);
        Ok(())
    }
}
// SAFETY: only observe_start below constructs Proof, from the actual common
// root, retained sealed compiled roles and genuine registered child pidfd.
// Config/bootstrap/driver/consumer are one qualified native chain. Initial
// redemption checks the exact bootstrap mask/mappings before Tool installation.
// The guest independently verifies the installed frontend before readiness.
// The shared driver protects the original operand lease, ticket and one native
// kernel leaf; successor redemption occurs in its fresh bootstrap,
// with real post_exec and Complete before the original stock loader resumes.
// This is not authority from a packet, digest, PID, auxv change or token.
unsafe impl NativeExecProof for Proof {
    fn owner(&self) -> DetTid {
        DetTid::from_raw(self.watch.process_id())
    }
    fn physical_ids(&self) -> Option<(i32, i32)> {
        self.is_live()
            .then_some((self.watch.process_id(), self.watch.process_id()))
    }
    fn is_live(&self) -> bool {
        !self.failed.load(Ordering::Acquire)
            && self.watch.watch_is_healthy()
            && alive(self.watch.retained_pidfd()).is_ok()
    }
    fn matches_peer(&self, peer: &OwnedFd) -> bool {
        self.peer(peer).is_ok()
    }
    fn qualify_target(&self, request: &PrepareNativeExec) -> bool {
        let result = (|| {
            if !self.initial_used.load(Ordering::Acquire)
                || !self.is_live()
                || request.bootstrap_digest != self.artifacts.bootstrap_identity().digest
                || request.resume_mask != self.root.initial_guest_mask()
                || request.sockets.validate().is_err()
                || [
                    request.sockets.detlog.object(),
                    request.sockets.observer.object(),
                ] != self.socket_objects
            {
                return Err(invalid(
                    "native target lacks initial admission or exact launch mask/bootstrap",
                ));
            }
            let entry = self.observer.original_exec_cut()?;
            if self.selected_original_target(&entry)? != request.target_digest {
                return Err(invalid(
                    "native original operand selects a different retained target",
                ));
            }
            let mut state = self
                .state
                .lock()
                .map_err(|_| invalid("native proof poisoned"))?;
            if state.redeemed.len() != state.completed.len() {
                return Err(invalid("native predecessor has an incomplete successor"));
            }
            state.old_entry = Some(entry);
            Ok(())
        })();
        result.is_ok()
    }
    fn qualify_runtime(&self, bytes: &[u8]) -> bool {
        let Ok(packet) = runtime_packet::decode(bytes) else {
            return false;
        };
        if reverie_liteinst::native_exec_state::NativeExecStatsV1::decode(packet.stats).is_err() {
            return false;
        }
        let mut canonical = [0; runtime_packet::PACKET_BYTES];
        if packet.encode(&mut canonical).is_err() || canonical.as_slice() != bytes {
            return false;
        }
        let Ok(current) = self.observer.original_exec_cut() else {
            return false;
        };
        let Ok(state) = self.state.lock() else {
            return false;
        };
        state.old_entry.as_ref() == Some(&current)
            && current.sequence == packet.sequence
            && self.is_live()
    }
    fn qualify_completion(&self, ticket: ExecTicket, binding: NativeExecBinding) -> bool {
        self.complete_mapping(ticket, binding).is_ok()
    }
}

struct Active {
    proof: Arc<Proof>,
    provider: Arc<Provider>,
    forwarded: Mutex<Option<NativeForwardedSourceReceipt>>,
}

/// Installed once per physical run, with the same immutable input capture used
/// by the other verify run. Runtime sockets/watch/nonce are fresh per run.
pub(super) struct NativeRunOwner {
    global: Arc<detcore::GlobalState>,
    config: detcore::Config,
    inputs: Arc<HeldDynamicInputs>,
    artifacts: Arc<QualifiedNativeArtifacts>,
    root: Arc<RootIdentity>,
    exits: Arc<InGuestExitAdmission>,
    initial_target: [u8; 32],
    nonce: [u8; 32],
    registry_nonce: [u8; 32],
    digest: [u8; 32],
    schema: [u8; 16],
    detlog_identity: String,
    observer_identity: String,
    observer: Mutex<Option<OwnedFd>>,
    initial_used: Arc<AtomicBool>,
    failed: Arc<AtomicBool>,
    active: OnceLock<Active>,
    active_ready: tokio::sync::Notify,
}
impl NativeRunOwner {
    pub(super) fn prepare(
        global: Arc<detcore::GlobalState>,
        inputs: Arc<HeldDynamicInputs>,
        artifacts: Arc<QualifiedNativeArtifacts>,
        root: Arc<RootIdentity>,
        initial_program: &GuestPath,
        detlog_sender: BorrowedFd<'_>,
        observer: (OwnedFd, BorrowedFd<'_>),
    ) -> io::Result<Arc<Self>> {
        let (observer_read, observer_sender) = observer;
        // Preserve the actual scheduler owner's skipped host sink fields as well
        // as the serialized guest Config. A caller cannot substitute a second
        // Config whose wire digest happens to agree.
        let config = global.native_exec_host_config().clone();
        detcore::native_exec::validate_native_exec_host_configuration(&config)
            .map_err(|e| invalid(format!("native Config refused: {e:?}")))?;
        if !Arc::ptr_eq(&inputs, artifacts.inputs()) {
            return Err(invalid("native artifact/input owners differ"));
        }
        let target = inputs
            .require_initial(initial_program)
            .map_err(error)?
            .identity
            .digest;
        if !artifacts
            .catalog()
            .iter()
            .any(|row| row.target_digest == target)
        {
            return Err(invalid(
                "native initial program lacks compiled role catalog",
            ));
        }
        let digest = config_digest(&reverie_rpc_transport::codec::encode(&config).map_err(error)?);
        let schema = detcore::config_wire_fingerprint()
            .as_bytes()
            .try_into()
            .map_err(|_| invalid("native Config schema width"))?;
        let exits = Arc::new(InGuestExitAdmission::default());
        exits.attach_exit_reporter(Arc::new(NativeReporter {
            global: Arc::downgrade(&global),
        }));
        let owner = Arc::new(Self {
            global,
            config,
            inputs,
            artifacts,
            root,
            exits,
            initial_target: target,
            nonce: entropy()?,
            registry_nonce: entropy()?,
            digest,
            schema,
            detlog_identity: inherited_socket_identity(detlog_sender, 204)?,
            observer_identity: inherited_socket_identity(observer_sender, 203)?,
            observer: Mutex::new(Some(observer_read)),
            initial_used: Arc::new(AtomicBool::new(false)),
            failed: Arc::new(AtomicBool::new(false)),
            active: OnceLock::new(),
            active_ready: tokio::sync::Notify::new(),
        });
        if owner.detlog_identity.split_once(':').map(|(_, s)| s)
            == owner.observer_identity.split_once(':').map(|(_, s)| s)
        {
            return Err(invalid(
                "native observer and DETLOG are the same socket object",
            ));
        }
        Ok(owner)
    }
    pub(super) fn initial_packet(&self) -> io::Result<Vec<u8>> {
        use detcore_model::liteinst_native_exec_input as input;
        let policy = self
            .config
            .in_guest_detlog_forward_policy
            .as_deref()
            .ok_or_else(|| invalid("native strict run lacks forwarding policy"))?;
        input::encode(input::Packet {
            config_digest: &self.digest,
            config_schema: &self.schema,
            native_source_digest: &self.artifacts.source_digest(),
            target_digest: &self.initial_target,
            bootstrap_digest: &self.artifacts.bootstrap_identity().digest,
            image: input::Image::Initial { nonce: &self.nonce },
            coordinator: b"/run/rpc.sock",
            detlog_identity: &self.detlog_identity,
            forward_policy: policy,
            observer_identity: &self.observer_identity,
        })
        .map_err(|e| invalid(format!("native initial packet: {e:?}")))
    }
    pub(super) fn configure_server(
        self: &Arc<Self>,
        server: &mut reverie_rpc_transport::RpcServer<detcore::GlobalState>,
    ) -> io::Result<()> {
        if !Arc::ptr_eq(&self.global, &server.global()) {
            return Err(invalid("native server has another GlobalState"));
        }
        server.retain_connection_issues();
        server.set_connection_admission(self.clone());
        server
            .set_native_image_admission(self.clone(), self.clone())
            .map_err(error)
    }
    pub(super) fn statistics_admission(self: &Arc<Self>) -> Arc<StatisticsAdmission> {
        Arc::new(StatisticsAdmission {
            owner: Arc::downgrade(self),
            accepted: AtomicU64::new(0),
        })
    }
    fn active(&self) -> io::Result<&Active> {
        self.active
            .get()
            .ok_or_else(|| invalid("native actual child was not admitted"))
    }
    pub(super) fn fail(&self, ticket: Option<ExecTicket>, reason: impl std::fmt::Display) {
        self.failed.store(true, Ordering::Release);
        self.active_ready.notify_waiters();
        if let Some(store) = self.global.native_exec_registry() {
            let mut registry = store.lock().unwrap();
            if let Some(ticket) = ticket {
                let _ = registry.failed(ticket);
            } else {
                registry.provision_transport_failed();
            }
        }
        self.exits.fail_launch(&reason);
    }
    pub(super) async fn wait_until_started(&self) -> io::Result<()> {
        loop {
            let notified = self.active_ready.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.failed.load(Ordering::Acquire) {
                return Err(invalid("native startup failed before service admission"));
            }
            if self.active.get().is_some() {
                return Ok(());
            }
            notified.await;
        }
    }
    pub(super) fn provider(&self) -> io::Result<Arc<Provider>> {
        Ok(self.active()?.provider.clone())
    }
    pub(super) fn observer(&self) -> io::Result<Arc<Observer>> {
        Ok(self.active()?.proof.observer.clone())
    }
    pub(super) async fn finish_physical(&self) -> io::Result<()> {
        let active = self.active()?;
        let watch = &active.proof.watch;
        let completion = self
            .exits
            .drain_native_completion(
                Duration::from_secs(10),
                watch.retained_pidfd().try_clone_to_owned()?,
                watch.process_id(),
                watch.birth_identity(),
            )
            .await?;
        for pid in self.exits.reported_exits() {
            self.global.complete_physical_process_exit(pid);
        }
        self.global.release_all_physical_process_exits();
        if self.failed.load(Ordering::Acquire) || !self.initial_used.load(Ordering::Acquire) {
            return Err(invalid("native run lacks successful initial admission"));
        }
        let receipt = active
            .forwarded
            .lock()
            .unwrap()
            .take()
            .ok_or_else(|| invalid("native source final receipt already consumed"))?;
        detcore::detlog::finish_native_forwarded_source_after_exit(&receipt, &completion)
            .map_err(|e| invalid(format!("native final DETLOG drain: {e:?}")))?;
        active.proof.observer.finish(&completion)?;
        let state = active.proof.state.lock().unwrap();
        if state.redeemed.len() != state.completed.len() {
            return Err(invalid(
                "native physical death interrupted successor lifecycle",
            ));
        }
        Ok(())
    }
}
impl NativeStartAdmission for NativeRunOwner {
    fn observe_start(
        &self,
        peer: OwnedFd,
        root: OwnedFd,
        observation: NativeRootObservation,
    ) -> io::Result<()> {
        let result = (|| {
            if self.active.get().is_some() || self.failed.load(Ordering::Acquire) {
                return Err(invalid("native startup repeated or poisoned"));
            }
            let admitted = self.exits.admit(peer.try_clone()?)?;
            if admitted.process_id != observation.pid || observation.pid != observation.tid {
                return Err(invalid("native startup does not name actual leader"));
            }
            let watch = Arc::new(self.exits.retain_native_process(peer)?);
            let root = self.root.bind_startup(root, observation, watch.clone())?;
            let forwarded = detcore::detlog::retain_native_forwarded_source(watch.as_ref())
                .map_err(|e| invalid(format!("native source admission: {e:?}")))?;
            let read = self
                .observer
                .lock()
                .unwrap()
                .take()
                .ok_or_else(|| invalid("native observer startup repeated"))?;
            let observer = Observer::new(read, watch.clone())?;
            let proof = Arc::new(Proof {
                watch: watch.clone(),
                root,
                observer,
                inputs: self.inputs.clone(),
                artifacts: self.artifacts.clone(),
                initial_used: self.initial_used.clone(),
                failed: self.failed.clone(),
                state: Mutex::new(ProofState::default()),
                signal_masks: NativeSignalMasks::from_config(&self.config),
                socket_objects: [
                    detcore::detlog::parse_tool_output_env_value(&self.detlog_identity)
                        .ok_or_else(|| invalid("native held DETLOG identity malformed"))?
                        .1,
                    detcore::detlog::parse_tool_output_env_value(&self.observer_identity)
                        .ok_or_else(|| invalid("native held observer identity malformed"))?
                        .1,
                ],
            });
            self.global
                .install_native_exec_registry(
                    NativeExecRegistry::new(proof.clone(), self.registry_nonce)
                        .map_err(|e| invalid(format!("native registry setup: {e:?}")))?,
                )
                .map_err(|e| invalid(format!("native registry install: {e:?}")))?;
            // SAFETY: provider authority joins the same private compiled roles,
            // closed root, actual watch and existing GlobalState/registry. Its
            // catalog is independent of every provider request and both verify
            // runs retain the same immutable captured input objects.
            let provider = Arc::new(unsafe {
                Provider::install(
                    self.global.clone(),
                    watch,
                    self.inputs.clone(),
                    self.artifacts.clone(),
                    ProviderBindings {
                        config_digest: self.digest,
                        config_schema: self.schema,
                        native_source_digest: self.artifacts.source_digest(),
                        coordinator: b"/run/rpc.sock".to_vec(),
                        detlog_identity: self.detlog_identity.clone(),
                        forward_policy: self
                            .config
                            .in_guest_detlog_forward_policy
                            .clone()
                            .ok_or_else(|| invalid("native forwarding policy missing"))?,
                        observer_identity: self.observer_identity.clone(),
                    },
                )?
            });
            self.active
                .set(Active {
                    proof,
                    provider,
                    forwarded: Mutex::new(Some(forwarded)),
                })
                .map_err(|_| invalid("native active owner installed twice"))?;
            self.active_ready.notify_waiters();
            Ok(())
        })();
        if let Err(error) = &result {
            self.fail(None, error);
        }
        result
    }
    fn startup_failed(&self) {
        self.fail(None, "native common startup failed");
    }
}
impl ConnectionAdmission for NativeRunOwner {
    fn admit(&self, peer: OwnedFd) -> io::Result<Admitted> {
        let result = (|| {
            let active = self.active()?;
            active.proof.peer(&peer)?;
            let admitted = self.exits.admit(peer)?;
            if admitted.process_id != active.proof.watch.process_id() {
                return Err(invalid("native RPC watch returned another leader"));
            }
            Ok(admitted)
        })();
        if let Err(error) = &result {
            self.fail(None, error);
        }
        result
    }
    fn admission_failed(&self, error: &io::Error) {
        self.fail(None, error);
    }
}
impl InitialImageAdmission for NativeRunOwner {
    fn redeem(
        &self,
        peer: OwnedFd,
        admitted: Admitted,
        nonce: &[u8],
        digest: [u8; 32],
    ) -> io::Result<()> {
        let result = (|| {
            let active = self.active()?;
            active.proof.peer(&peer)?;
            detcore::native_exec::validate_native_exec_host_configuration(&self.config)
                .map_err(|e| invalid(format!("native initial host Config: {e:?}")))?;
            if admitted.process_id != active.proof.watch.process_id()
                || nonce != self.nonce
                || digest != self.digest
                || self.initial_used.swap(true, Ordering::AcqRel)
            {
                return Err(invalid(
                    "native InitialImage owner/nonce/Config/replay refused",
                ));
            }
            let image = active.proof.root.image(&peer, self.initial_target)?;
            active.proof.signal_masks.require_bootstrap(&image)?;
            Ok(())
        })();
        if let Err(error) = &result {
            self.fail(None, error);
        }
        result
    }
}
impl SuccessorAdmission for NativeRunOwner {
    fn redeem(
        &self,
        peer: OwnedFd,
        admitted: Admitted,
        request: SuccessorRequest<'_>,
    ) -> io::Result<Vec<u8>> {
        let ticket = request
            .ticket
            .try_into()
            .map(ExecTicket)
            .map_err(|_| invalid("native successor exact ticket width"))?;
        let result = (|| {
            let active = self.active()?;
            active.proof.peer(&peer)?;
            if !self.initial_used.load(Ordering::Acquire)
                || admitted.process_id != active.proof.watch.process_id()
                || request.expected.config != self.digest
            {
                return Err(invalid("native successor independent Config/owner refused"));
            }
            let store = self
                .global
                .native_exec_registry()
                .ok_or_else(|| invalid("native registry absent"))?;
            let binding = store
                .lock()
                .unwrap()
                .issued_binding(&peer, ticket)
                .map_err(|e| invalid(format!("native issued binding: {e:?}")))?;
            let image = active.proof.root.image(&peer, binding.target_digest)?;
            active.proof.signal_masks.require_bootstrap(&image)?;
            {
                let state = active.proof.state.lock().unwrap();
                if state.redeemed.contains_key(&ticket) {
                    return Err(invalid("native accepted image replay"));
                }
            }
            let bytes = store
                .lock()
                .unwrap()
                .redeem(
                    &peer,
                    ticket,
                    request.expected.config,
                    request.expected.state,
                )
                .map_err(|e| invalid(format!("native successor consumption: {e:?}")))?;
            // Never acquire registry while holding proof.state: registry's
            // Complete callback takes proof.state in the opposite direction.
            let mut state = active.proof.state.lock().unwrap();
            if state
                .redeemed
                .insert(
                    ticket,
                    Redeemed {
                        peer,
                        image,
                        binding,
                    },
                )
                .is_some()
            {
                return Err(invalid("native successor consumed concurrently"));
            }
            Ok(bytes)
        })();
        if let Err(error) = &result {
            self.fail(Some(ticket), error);
        }
        result
    }
    fn connection_closed(
        &self,
        admitted: Admitted,
        image: &NativeImageConnection,
    ) -> io::Result<()> {
        let active = self.active()?;
        let watch = &active.proof.watch;
        if admitted.process_id != watch.process_id()
            || self.failed.load(Ordering::Acquire)
            || !watch.watch_is_healthy()
        {
            return Err(invalid(
                "native stream closed outside its retained healthy owner",
            ));
        }
        let predecessor = match image {
            NativeImageConnection::Initial => None,
            NativeImageConnection::Successor { ticket } => Some(ExecTicket(
                ticket
                    .as_slice()
                    .try_into()
                    .map_err(|_| invalid("native EOF successor ticket shape"))?,
            )),
        };
        let mut poll = libc::pollfd {
            fd: watch.retained_pidfd().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut poll, 1, 0) };
        if ready < 0 || poll.revents & (libc::POLLERR | libc::POLLNVAL) != 0 {
            return Err(invalid("native EOF physical watch query failed"));
        }
        // Actual death or the real scheduler's granted exit interval may close
        // its final stream. Neither is the final completion receipt: all watch,
        // DETLOG, observer, RPC and lifecycle drains still run afterward.
        if ready == 1 && poll.revents & (libc::POLLIN | libc::POLLHUP) != 0 {
            return Ok(());
        }
        if self
            .global
            .backend_pending_process_exits()
            .contains(&watch.process_id())
        {
            return Ok(());
        }
        let peer = watch.retained_pidfd().try_clone_to_owned()?;
        let store = self
            .global
            .native_exec_registry()
            .ok_or_else(|| invalid("native EOF registry absent"))?;
        let expected = store
            .lock()
            .unwrap()
            .permits_connection_close(&peer, predecessor)
            .map_err(|e| invalid(format!("native per-image EOF history: {e:?}")))?;
        if !expected {
            return Err(invalid("unexpected live native image disconnect"));
        }
        Ok(())
    }
}
/// The real typed statistics stream is independently same-birth admitted.
pub(super) struct StatisticsAdmission {
    owner: Weak<NativeRunOwner>,
    accepted: AtomicU64,
}
impl StatisticsAdmission {
    pub(super) fn accepted(&self) -> u64 {
        self.accepted.load(Ordering::Acquire)
    }
}
impl ConnectionAdmission for StatisticsAdmission {
    fn admit(&self, peer: OwnedFd) -> io::Result<Admitted> {
        let owner = self
            .owner
            .upgrade()
            .ok_or_else(|| invalid("native statistics owner is gone"))?;
        if !owner.initial_used.load(Ordering::Acquire) {
            return Err(invalid("native statistics preceded InitialImage admission"));
        }
        let admitted = owner.admit(peer)?;
        self.accepted
            .try_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_add(1))
            .map_err(|_| invalid("native statistics admission counter overflow"))?;
        Ok(admitted)
    }
    fn admission_failed(&self, error: &io::Error) {
        if let Some(owner) = self.owner.upgrade() {
            owner.fail(None, error);
        }
    }
}
struct NativeReporter {
    global: Weak<detcore::GlobalState>,
}
impl ExitReporter for NativeReporter {
    fn process_exited(&self, pid: i32) {
        if let Some(global) = self.global.upgrade() {
            if let Some(store) = global.native_exec_registry() {
                store.lock().unwrap().died();
            }
            global.on_backend_process_exited(pid);
        }
    }
    fn pending_process_exits(&self) -> Vec<i32> {
        self.global
            .upgrade()
            .map(|g| g.backend_pending_process_exits())
            .unwrap_or_default()
    }
    fn backend_failed(&self, failure: reverie::BackendFailure) {
        if let Some(global) = self.global.upgrade() {
            if let Some(store) = global.native_exec_registry() {
                store.lock().unwrap().provision_transport_failed();
            }
            global.report_backend_failure(failure);
        }
    }
}
fn inherited_socket_identity(fd: BorrowedFd<'_>, destination: i32) -> io::Result<String> {
    let encoded = detcore::detlog::tool_output_env_value(fd.as_raw_fd())?;
    let (_, (device, inode)) = detcore::detlog::parse_tool_output_env_value(&encoded)
        .ok_or_else(|| invalid("native endpoint identity malformed"))?;
    Ok(format!("{destination}:{device:016x}:{inode:016x}"))
}
fn read_original_path(pid: i32, pointer: u64) -> io::Result<Vec<u8>> {
    if pid <= 0 || pointer == 0 || pointer >= 0x0000_8000_0000_0000 {
        return Err(invalid("native original path pointer domain"));
    }
    let mut path = Vec::new();
    while path.len() < 4096 {
        let address = pointer
            .checked_add(path.len() as u64)
            .ok_or_else(|| invalid("native path address overflow"))?;
        let length = (4096 - (address as usize & 4095)).min(4096 - path.len());
        let mut bytes = [0u8; 4096];
        let local = libc::iovec {
            iov_base: bytes.as_mut_ptr().cast(),
            iov_len: length,
        };
        let remote = libc::iovec {
            iov_base: address as usize as *mut libc::c_void,
            iov_len: length,
        };
        let n = unsafe { libc::process_vm_readv(pid, &local, 1, &remote, 1, 0) };
        if n <= 0 {
            return Err(invalid(
                "native original path is unreadable at the actual cut",
            ));
        }
        let bytes = &bytes[..n as usize];
        if let Some(end) = bytes.iter().position(|b| *b == 0) {
            path.extend_from_slice(&bytes[..end]);
            return Ok(path);
        }
        path.extend_from_slice(bytes);
    }
    Err(invalid("native original path exceeds finite bound"))
}

/// Observes the real watched Initial peer without replacing any admission
/// decision. The preopened report descriptor and thread-local lease survive
/// the common container fork; the descriptor is CLOEXEC in the actual guest.
#[cfg(test)]
pub(super) mod bootstrap_mask_observation {
    use std::cell::RefCell;
    use std::fs::File;
    use std::io::Read;
    use std::io::Write;
    use std::marker::PhantomData;
    use std::rc::Rc;

    use super::*;

    struct Probe {
        report: Mutex<File>,
        delay: Duration,
    }
    thread_local! {
        static PROBE: RefCell<Option<Arc<Probe>>> = const { RefCell::new(None) };
    }
    pub(crate) struct Lease(PhantomData<Rc<()>>);
    impl Lease {
        pub(crate) fn arm(report: File, delay: Duration) -> io::Result<Self> {
            if delay > Duration::from_millis(50) {
                return Err(invalid("bootstrap observation delay exceeds test bound"));
            }
            PROBE.with(|slot| {
                let mut slot = slot.borrow_mut();
                if slot.is_some() {
                    return Err(invalid("bootstrap observation lease already active"));
                }
                *slot = Some(Arc::new(Probe {
                    report: Mutex::new(report),
                    delay,
                }));
                Ok(Self(PhantomData))
            })
        }
    }
    impl Drop for Lease {
        fn drop(&mut self) {
            PROBE.with(|slot| {
                slot.borrow_mut().take();
            });
        }
    }
    struct ObservedAdmission {
        owner: Arc<NativeRunOwner>,
        probe: Arc<Probe>,
    }
    fn raw_mask(proof: &Proof, peer: &OwnedFd) -> io::Result<u64> {
        proof.peer(peer)?;
        let path = format!("/proc/{}/status", proof.watch.process_id());
        let mut bytes = Vec::new();
        File::open(path)?
            .take(16 * 1024 + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() > 16 * 1024 {
            return Err(invalid("bootstrap status observation exceeds bound"));
        }
        let status = std::str::from_utf8(&bytes).map_err(error)?;
        let masks: Vec<_> = status
            .lines()
            .filter_map(|line| line.strip_prefix("SigBlk:"))
            .collect();
        if masks.len() != 1 || masks[0].trim().len() != 16 {
            return Err(invalid("bootstrap status lacks exact raw SigBlk"));
        }
        let mask = u64::from_str_radix(masks[0].trim(), 16).map_err(error)?;
        proof.peer(peer)?;
        Ok(mask)
    }
    impl InitialImageAdmission for ObservedAdmission {
        fn redeem(
            &self,
            peer: OwnedFd,
            admitted: Admitted,
            nonce: &[u8],
            digest: [u8; 32],
        ) -> io::Result<()> {
            let result = (|| {
                let active = self.owner.active()?;
                // First observation immediately follows complete request decode.
                // No guest-side barrier or mask change is introduced.
                let immediate = raw_mask(&active.proof, &peer)?;
                if !self.probe.delay.is_zero() {
                    std::thread::sleep(self.probe.delay);
                }
                let delayed = raw_mask(&active.proof, &peer)?;
                let image = active.proof.root.image(&peer, self.owner.initial_target)?;
                let pid = active.proof.watch.process_id();
                let birth = active.proof.watch.birth_identity();
                // This is the original one-use authority, with the original
                // peer, nonce and Config. Only its result can permit the ACK.
                let delegated = InitialImageAdmission::redeem(
                    self.owner.as_ref(),
                    peer,
                    admitted,
                    nonce,
                    digest,
                );
                let record = serde_json::json!({
                    "kind": "initial_observation",
                    "pid": pid, "birth": birth,
                    "delay_millis": self.probe.delay.as_millis() as u64,
                    "immediate_mask": immediate, "delayed_mask": delayed,
                    "image_mask": image.observed_mask,
                    "expected_mask": NativeSignalMasks::BOOTSTRAP,
                    "image": format!("{image:?}"),
                    "owner_redeemed": delegated.is_ok(),
                    "owner_error": delegated.as_ref().err().map(ToString::to_string),
                });
                let mut report = self.probe.report.lock().map_err(error)?;
                serde_json::to_writer(&mut *report, &record).map_err(error)?;
                report.write_all(b"\n")?;
                delegated
            })();
            if let Err(error) = &result {
                self.owner.fail(None, error);
            }
            result
        }
    }
    pub(crate) fn configure(
        owner: &Arc<NativeRunOwner>,
        server: &mut reverie_rpc_transport::RpcServer<detcore::GlobalState>,
    ) -> io::Result<()> {
        let probe = PROBE.with(|slot| slot.borrow().clone());
        if let Some(probe) = probe {
            // Retain the genuine successor owner; the initial-only setter would
            // disable successor support. The server and GlobalState stay put.
            server
                .set_native_image_admission(
                    Arc::new(ObservedAdmission {
                        owner: owner.clone(),
                        probe,
                    }),
                    owner.clone(),
                )
                .map_err(error)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Model observations exercise the production phase comparator. They do not
    // establish a physical watch, native exec, signal action or Tool readiness.
    fn bootstrap_image() -> Image {
        Image {
            target_digest: [1; 32],
            role_bases: BTreeMap::from([("target".to_owned(), vec![0x1000])]),
            auxv: BTreeMap::from([(7, 0), (9, 0x1000)]),
            namespace: (7, 9),
            observed_mask: NativeSignalMasks::BOOTSTRAP,
            native_resume: [3; 256],
        }
    }
    fn installed_image(masks: NativeSignalMasks, bootstrap: &Image) -> Image {
        let mut image = bootstrap.clone();
        image.observed_mask = masks.installed;
        image
    }
    fn instruction_masks() -> NativeSignalMasks {
        let mut subscriptions = reverie::Subscription::none();
        subscriptions.rdtsc();
        NativeSignalMasks::from_subscriptions(&subscriptions)
    }

    #[test]
    fn native_masks_follow_real_detcore_subscriptions_without_a_timer() {
        for virtualize_cpuid in [false, true] {
            let config = detcore::Config {
                passthru_opt: false,
                virtualize_cpuid,
                max_timeslice: None,
                ..detcore::Config::default()
            };
            let subscriptions = <detcore::Detcore as reverie::Tool>::subscriptions(&config);
            assert!(subscriptions.has_rdtsc());
            assert_eq!(subscriptions.has_cpuid(), config.virtualize_cpuid);
            let masks = NativeSignalMasks::from_config(&config);
            assert_eq!(masks.installed, 0xffffffffbffbfaff);
            let bootstrap = bootstrap_image();
            let installed = installed_image(masks, &bootstrap);
            assert!(masks.require_bootstrap(&bootstrap).is_ok());
            assert!(masks.require_completion(&bootstrap, &installed).is_ok());
        }
    }

    #[test]
    fn native_masks_require_the_exact_phase_at_redemption_and_completion() {
        let masks = instruction_masks();
        let bootstrap = bootstrap_image();
        let installed = installed_image(masks, &bootstrap);
        assert_eq!(bootstrap.observed_mask, 0xffffffffbffbfeff);
        assert!(masks.require_bootstrap(&bootstrap).is_ok());
        // Initial and successor RPC both precede the real installation.
        assert!(masks.require_bootstrap(&installed).is_err());
        assert!(masks.require_completion(&installed, &installed).is_err());
        assert!(masks.require_completion(&bootstrap, &bootstrap).is_err());
        assert!(masks.require_completion(&bootstrap, &installed).is_ok());
    }

    #[test]
    fn native_masks_reject_every_missing_or_additional_signal_bit() {
        let masks = instruction_masks();
        let bootstrap = bootstrap_image();
        let installed = installed_image(masks, &bootstrap);
        for bit in 0..64 {
            let mut wrong_bootstrap = bootstrap.clone();
            wrong_bootstrap.observed_mask ^= 1u64 << bit;
            assert!(
                masks.require_bootstrap(&wrong_bootstrap).is_err(),
                "bit {bit}"
            );
            assert!(
                masks
                    .require_completion(&wrong_bootstrap, &installed)
                    .is_err(),
                "bit {bit}"
            );
            let mut wrong_installed = installed.clone();
            wrong_installed.observed_mask ^= 1u64 << bit;
            assert!(
                masks
                    .require_completion(&bootstrap, &wrong_installed)
                    .is_err(),
                "bit {bit}"
            );
        }
        // These are raw kernel bits; libc's internal realtime signals stay blocked.
        for signal in [32, 33] {
            let bit = 1u64 << (signal - 1);
            assert_ne!(bootstrap.observed_mask & bit, 0);
            assert_ne!(installed.observed_mask & bit, 0);
        }
    }

    #[test]
    fn native_completion_preserves_all_other_image_fields_and_full_resume() {
        let masks = instruction_masks();
        let bootstrap = bootstrap_image();
        let installed = installed_image(masks, &bootstrap);
        let mut changed = installed.clone();
        changed.target_digest[0] ^= 1;
        assert!(masks.require_completion(&bootstrap, &changed).is_err());
        changed = installed.clone();
        changed.role_bases.get_mut("target").unwrap()[0] += 1;
        assert!(masks.require_completion(&bootstrap, &changed).is_err());
        changed = installed.clone();
        changed.auxv.insert(7, 1);
        assert!(masks.require_completion(&bootstrap, &changed).is_err());
        for field in 0..2 {
            changed = installed.clone();
            if field == 0 {
                changed.namespace.0 += 1;
            } else {
                changed.namespace.1 += 1;
            }
            assert!(masks.require_completion(&bootstrap, &changed).is_err());
        }
        for byte in 0..256 {
            changed = installed.clone();
            changed.native_resume[byte] ^= 1;
            assert!(
                masks.require_completion(&bootstrap, &changed).is_err(),
                "resume byte {byte}"
            );
        }
    }

    #[test]
    fn native_masks_subscription_shapes_keep_exact_equality() {
        // The no-instruction shape is a comparator control, not a native-domain admission.
        for cpuid in [false, true] {
            for rdtsc in [false, true] {
                let mut subscriptions = reverie::Subscription::none();
                if cpuid {
                    subscriptions.cpuid();
                }
                if rdtsc {
                    subscriptions.rdtsc();
                }
                let masks = NativeSignalMasks::from_subscriptions(&subscriptions);
                let bootstrap = bootstrap_image();
                let installed = installed_image(masks, &bootstrap);
                let expected = if cpuid || rdtsc {
                    NativeSignalMasks::BOOTSTRAP & !(1u64 << (libc::SIGSEGV - 1))
                } else {
                    NativeSignalMasks::BOOTSTRAP
                };
                assert_eq!(masks.installed, expected);
                assert!(masks.require_completion(&bootstrap, &installed).is_ok());
                for bit in 0..64 {
                    let mut changed = installed.clone();
                    changed.observed_mask ^= 1u64 << bit;
                    assert!(
                        masks.require_completion(&bootstrap, &changed).is_err(),
                        "cpuid={cpuid}, rdtsc={rdtsc}, bit={bit}"
                    );
                }
            }
        }
    }
}
