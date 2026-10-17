/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Coordinator-owned native exec transactions. No Tool is decoded or run here.
//!
//! This inactive interface requires a trusted watched native launcher. A PID,
//! ticket or socket credential alone is not proof that a kernel exec occurred.
//! The launcher owns that unsafe contract, including the covered interpreter,
//! protected bootstrap descriptors and successor-before-loader initialization.

use std::collections::BTreeMap;
use std::fmt;
use std::os::fd::OwnedFd;
use std::sync::Arc;

use detcore_model::liteinst_native_exec_state as state_frame;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest as _;
use sha2::Sha256;
/// Neutral, admitted live-cut data outside opaque Tool bytes.
pub use state_frame::CutBinding as NativeExecNeutralBinding;

/// Existing full request wire type; native controls preserve its exact tags.
pub use super::GlobalRequest;
/// Existing full response wire type; neutral native responses carry no clock.
pub use super::GlobalResponse;
use crate::types::DetTid;
use crate::types::FilesId;
use crate::types::MmId;

/// Maximum complete state frame, including both Tool and runtime state.
pub const MAX_STATE_BYTES: usize = state_frame::MAX_BYTES;
/// Finite retained history for the first, bounded native exec implementation.
pub const MAX_TRANSITIONS: usize = 1024;
const STATE_HEADER: usize = state_frame::HEADER_BYTES;

/// A protected single-use token. It carries no guest-visible deterministic data.
#[derive(Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub struct ExecTicket(pub [u8; 32]);

impl fmt::Debug for ExecTicket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ExecTicket(<private>)")
    }
}

/// A native control refusal, never a replacement for a returned kernel errno.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ExecControlError {
    /// No watched native authority was installed.
    Unavailable,
    /// The actual sender, watched birth or image incarnation disagrees.
    Identity,
    /// The existing Detcore exec preparation or one-thread domain disagrees.
    Preparation,
    /// A strict actual-source forwarding check failed.
    Forwarding,
    /// Payload bounds or a supported wire version disagree.
    Bounds,
    /// Actual bytes disagree with an independently supplied digest.
    Digest,
    /// The requested edge is not legal from the retained state.
    Transition,
    /// A physical death or terminal transport failure has been observed.
    Dead,
    /// The bounded retained history cannot accept another transaction.
    Capacity,
    /// Native runtime state or target/bootstrap qualification was refused.
    Qualification,
}

/// Socket facts queried by the genuine protected guest owner at its live cut.
/// Numeric slots are handoff data, not independent host proof of a child FD.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct NativeExecSocketBinding {
    /// Current protected descriptor observed by the quiescent guest owner.
    pub fd: i32,
    /// Device identity of the independently held socket object.
    pub device: u64,
    /// Inode identity of the independently held socket object.
    pub inode: u64,
}
impl NativeExecSocketBinding {
    /// Socket identity without its image-specific descriptor number.
    pub fn object(self) -> (u64, u64) {
        (self.device, self.inode)
    }
}

/// The same independently admitted socket roles in their current handoff slots.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct NativeExecSocketBindings {
    /// Existing protected deterministic-log output socket.
    pub detlog: NativeExecSocketBinding,
    /// Existing protected native-exec observation socket.
    pub observer: NativeExecSocketBinding,
}
impl NativeExecSocketBindings {
    /// Structural checks only; the proof must independently check both objects,
    /// and the qualified guest owner must query/recheck actual protected slots.
    pub fn validate(self) -> Result<(), ExecControlError> {
        if [self.detlog.fd, self.observer.fd]
            .into_iter()
            .any(|fd| fd <= 2 || (198..=202).contains(&fd))
            || self.detlog.fd == self.observer.fd
            || self.detlog.object() == self.observer.object()
        {
            return Err(ExecControlError::Identity);
        }
        Ok(())
    }
}

/// Data submitted at the already-prepared, still-live Detcore injection cut.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PrepareNativeExec {
    /// Current protected socket slots, queried/rechecked by the genuine owner.
    pub sockets: NativeExecSocketBindings,
    /// Actual old live-table/cut receipt, checked independently before retention.
    pub cut: NativeExecNeutralBinding,
    /// Fresh incarnation produced by the existing same-leader exec operation.
    pub prepared_mm: MmId,
    /// Digest of the unchanged canonical Config.
    pub config_digest: [u8; 32],
    /// Independently held immutable target identity.
    pub target_digest: [u8; 32],
    /// Independently held bootstrap/interpreter object identity.
    pub bootstrap_digest: [u8; 32],
    /// Actual guest mask that the native operation must preserve.
    pub resume_mask: u64,
    /// Digest of the exact opaque typed Tool snapshot.
    pub snapshot_digest: [u8; 32],
    /// Bounded snapshot; the coordinator never deserializes its Tool.
    pub snapshot: Vec<u8>,
}

/// Scheduler facts read under its mutex before a neutral native control edge.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NativeExecContext {
    /// Authenticated deterministic main thread.
    pub owner: DetTid,
    /// The old incarnation retained by the ordinary PrepareExec path.
    pub old_mm: MmId,
    /// Exact new incarnation of that same leader.
    pub prepared_mm: MmId,
    /// Independently computed canonical Config digest.
    pub config_digest: [u8; 32],
}

/// Real watcher and immutable-object authority, retained for a single birth.
///
/// # Safety
/// Implementations must retain the genuine pidfd/watch registration, accept
/// only the exact same living birth and main thread, and recheck terminal
/// watcher failure. `matches_peer` must compare the actual accepted peer pidfd
/// to that retained birth. Qualification must independently bind the actual
/// captured resume mask and the one-main-thread, childless preparation cut,
/// the immutable admitted namespace, and the held target/bootstrap files.
/// The runtime proof must exactly decode all carried counters, observer
/// sequence and forwarding boundary, refusing unsupported patch state.
///
/// The installed proof and its admission adapter must protect the ticket and
/// provisioned objects, constrain Issue to the exact original native exec leaf,
/// and prevent redemption from the old image. They must establish the covered
/// fresh bootstrap and successor-before-loader transition, and accept Complete
/// only from that genuine successor after its real post-exec and frontend
/// proofs. A same-birth pidfd, token, seal or digest is not proof of kernel exec.
/// Socket qualification must compare against independently held launch objects;
/// actual numeric ownership requires the qualified guest owner's physical
/// queries and final rechecks, never object equality alone.
/// No request field is authority for any of these facts. These obligations
/// belong to the trusted native launch chain, not to this opaque storage.
pub unsafe trait NativeExecProof: fmt::Debug + Send + Sync {
    /// Deterministic owner established by actual admission.
    fn owner(&self) -> DetTid;
    /// Physical main-thread identity from the same retained watched birth.
    /// Missing proof refuses control RPCs. Implementations must never derive
    /// this identity from request IDs or unretained numerical PID equality.
    fn physical_ids(&self) -> Option<(i32, i32)> {
        None
    }
    /// The retained birth remains alive with no watcher failure.
    fn is_live(&self) -> bool;
    /// Actual accepted peer pidfd names precisely that retained live birth.
    fn matches_peer(&self, peer: &OwnedFd) -> bool;
    /// Validate held target/bootstrap objects and immutable namespace, and
    /// compare the requested mask with the independently captured actual cut.
    fn qualify_target(&self, request: &PrepareNativeExec) -> bool;
    /// Validate the real, versioned backend runtime state before sealing it.
    fn qualify_runtime(&self, bytes: &[u8]) -> bool;
    /// Revalidate the actual accepted successor's held mappings, namespace,
    /// birth and binding immediately before Complete. Refuses by default.
    /// Called under the registry mutex; must never acquire scheduler/pending
    /// locks or recurse into the registry. This augments, and never replaces,
    /// the unsafe genuine-kernel-exec/lifecycle/consumer obligations above.
    fn qualify_completion(&self, _ticket: ExecTicket, _binding: NativeExecBinding) -> bool {
        false
    }
}

/// Retained transaction stage. Terminal records cannot be removed or replayed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExecStage {
    /// Prepared Tool bytes; runtime state may still need sealing.
    Prepared,
    /// Provision succeeded and the final old-image issue was acknowledged.
    Issued,
    /// Consumed before sending successor ACK, Config or state bytes.
    Redeemed,
    /// Genuine post-exec and frontend completion were acknowledged.
    Completed,
    /// A pre-redemption failure cancelled the native transaction.
    Cancelled,
    /// Watched death invalidated the transaction.
    Dead,
    /// A terminal provision/transport/bootstrap failure invalidated it.
    Failed,
}

impl ExecStage {
    fn live(self) -> bool {
        matches!(self, Self::Prepared | Self::Issued | Self::Redeemed)
    }
}

/// Immutable target and state identities retained under the ticket.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NativeExecBinding {
    /// Admitted old-image to bootstrap slots; successor may reserve them again.
    pub sockets: NativeExecSocketBindings,
    /// Independently admitted neutral cut, emitted before opaque Tool bytes.
    pub cut: NativeExecNeutralBinding,
    /// Independently validated scheduler and Config identity.
    pub context: NativeExecContext,
    /// Held immutable target identity.
    pub target_digest: [u8; 32],
    /// Held bootstrap/interpreter identity.
    pub bootstrap_digest: [u8; 32],
    /// Actual guest mask preserved across the operation.
    pub resume_mask: u64,
    /// Exact prepared Tool bytes retained by this transaction.
    pub snapshot_digest: [u8; 32],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProvisionState {
    Fresh,
    Consumed,
    Delivered,
}

#[derive(Debug)]
struct Transition {
    binding: NativeExecBinding,
    stage: ExecStage,
    marked_past_first_exec: bool,
    provision: ProvisionState,
    tool: Vec<u8>,
    state: Vec<u8>,
    state_digest: Option<[u8; 32]>,
}

/// Serial retained store for one watched native process. The coordinator must
/// place this behind the same mutex for RPC, admission and watcher callbacks.
/// No socket EOF calls `died`: the birth watcher supplies actual death.
pub struct NativeExecRegistry {
    proof: Arc<dyn NativeExecProof>,
    nonce: [u8; 32],
    next_ticket: u64,
    current_mm: Option<MmId>,
    current_files: FilesId,
    initial_mm: Option<MmId>,
    forwarding_receipt: Option<crate::detlog::CoordinatorForwardingReceipt>,
    transitions: BTreeMap<ExecTicket, Transition>,
    dead: bool,
    terminal_failure: bool,
}

impl fmt::Debug for NativeExecRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NativeExecRegistry")
            .field("owner", &self.proof.owner())
            .field("retained_transitions", &self.transitions.len())
            .field("dead", &self.dead)
            .field("terminal_failure", &self.terminal_failure)
            .finish_non_exhaustive()
    }
}

impl NativeExecRegistry {
    /// Retain the launcher's proof and a private fresh CSPRNG nonce. The nonce
    /// is control-plane secrecy, never a draw from any guest PRNG or clock.
    pub fn new(proof: Arc<dyn NativeExecProof>, nonce: [u8; 32]) -> Result<Self, ExecControlError> {
        if proof.owner().as_raw() <= 0 || !proof.is_live() || nonce == [0; 32] {
            return Err(ExecControlError::Identity);
        }
        // Actual first-domain main initialization uses FileMetadata::new(pid),
        // whose table is FilesId::initial(pid). This is a model incarnation
        // anchor; the genuine guest receipt separately validates the live table.
        let current_files = FilesId::initial(proof.owner());
        Ok(Self {
            proof,
            nonce,
            next_ticket: 1,
            current_mm: None,
            current_files,
            initial_mm: None,
            forwarding_receipt: None,
            transitions: BTreeMap::new(),
            dead: false,
            terminal_failure: false,
        })
    }

    fn check_owner(&mut self, owner: DetTid) -> Result<(), ExecControlError> {
        if self.terminal_failure {
            return Err(ExecControlError::Dead);
        }
        if self.dead || !self.proof.is_live() {
            self.died();
            return Err(ExecControlError::Dead);
        }
        if owner != self.proof.owner() {
            return Err(ExecControlError::Identity);
        }
        Ok(())
    }

    // Called under sched and this registry's mutex before a control transition.
    // Every cut retains and checks the same real forwarding source; no reset or
    // synthetic count can establish a fresh boundary for a later native edge.
    fn check_forwarding_cut(
        &mut self,
        owner: DetTid,
        produced: u64,
        require_empty: bool,
    ) -> Result<(), ExecControlError> {
        self.check_owner(owner)?;
        let (pid, tid) = self
            .proof
            .physical_ids()
            .ok_or(ExecControlError::Identity)?;
        if pid <= 0 || pid != tid {
            return Err(ExecControlError::Identity);
        }
        if require_empty && produced != 0 {
            return Err(ExecControlError::Forwarding);
        }
        let receipt = crate::detlog::check_native_exec_forwarding_cut(
            tid,
            produced,
            self.forwarding_receipt.as_ref(),
        )
        .map_err(|_| ExecControlError::Forwarding)?;
        self.forwarding_receipt = Some(receipt);
        Ok(())
    }

    /// Retain exact Tool bytes only after existing exec preparation is proved.
    pub fn prepare(
        &mut self,
        context: NativeExecContext,
        request: PrepareNativeExec,
    ) -> Result<ExecTicket, ExecControlError> {
        self.check_owner(context.owner)?;
        request.sockets.validate()?;
        let (physical_pid, physical_tid) = self
            .proof
            .physical_ids()
            .ok_or(ExecControlError::Identity)?;
        request
            .cut
            .validate()
            .map_err(|_| ExecControlError::Identity)?;
        if physical_pid <= 0
            || physical_pid != physical_tid
            || request.cut.physical_tid != physical_tid
            || request.cut.dettid != context.owner
            || request.cut.detpid != context.owner
            || request.cut.old_mm != context.old_mm
            || request.cut.old_files != self.current_files
            || request.cut.prepared_mm != context.prepared_mm
            || request.cut.config_digest != context.config_digest
            || self.current_mm.is_some_and(|mm| mm != context.old_mm)
            || context.old_mm.generation().checked_add(1) != Some(context.prepared_mm.generation())
            || context.prepared_mm.creator() != context.owner
            || request.prepared_mm != context.prepared_mm
            || request.config_digest != context.config_digest
        {
            return Err(ExecControlError::Identity);
        }
        if self.transitions.values().any(|value| value.stage.live()) {
            return Err(ExecControlError::Transition);
        }
        if self.transitions.len() >= MAX_TRANSITIONS {
            return Err(ExecControlError::Capacity);
        }
        if request.snapshot.is_empty()
            || request.snapshot.len() > MAX_STATE_BYTES - STATE_HEADER - 1
        {
            return Err(ExecControlError::Bounds);
        }
        if <[u8; 32]>::from(Sha256::digest(&request.snapshot)) != request.snapshot_digest {
            return Err(ExecControlError::Digest);
        }
        if !self.proof.qualify_target(&request) {
            return Err(ExecControlError::Qualification);
        }
        // The secret run nonce plus a checked, never-reused ordinal makes
        // tickets distinct without touching deterministic execution state.
        let mut hash = Sha256::new();
        hash.update(b"Hermit native exec ticket v1\0");
        hash.update(self.nonce);
        hash.update(self.next_ticket.to_le_bytes());
        let ticket = ExecTicket(hash.finalize().into());
        self.next_ticket = self
            .next_ticket
            .checked_add(1)
            .ok_or(ExecControlError::Capacity)?;
        if self.transitions.contains_key(&ticket) {
            return Err(ExecControlError::Transition);
        }
        self.current_mm = Some(context.old_mm);
        self.initial_mm.get_or_insert(context.old_mm);
        self.transitions.insert(
            ticket,
            Transition {
                binding: NativeExecBinding {
                    sockets: request.sockets,
                    cut: request.cut,
                    context,
                    target_digest: request.target_digest,
                    bootstrap_digest: request.bootstrap_digest,
                    resume_mask: request.resume_mask,
                    snapshot_digest: request.snapshot_digest,
                },
                stage: ExecStage::Prepared,
                marked_past_first_exec: false,
                provision: ProvisionState::Fresh,
                tool: request.snapshot,
                state: Vec::new(),
                state_digest: None,
            },
        );
        Ok(ticket)
    }

    fn transaction(
        &mut self,
        owner: DetTid,
        ticket: ExecTicket,
        mm: MmId,
    ) -> Result<&mut Transition, ExecControlError> {
        self.check_owner(owner)?;
        let entry = self
            .transitions
            .get_mut(&ticket)
            .ok_or(ExecControlError::Identity)?;
        if entry.binding.context.owner != owner || entry.binding.context.prepared_mm != mm {
            return Err(ExecControlError::Identity);
        }
        Ok(entry)
    }

    // Only the successful same-leader ordinary MarkPastFirstExecve handler
    // calls this after its real scheduler reconnect. An alternate registration
    // or a global startup boolean cannot establish this per-ticket receipt.
    pub(super) fn mark_past_first_exec(
        &mut self,
        owner: DetTid,
        mm: MmId,
    ) -> Result<(), ExecControlError> {
        if !self
            .transitions
            .values()
            .any(|entry| entry.stage.live() && entry.binding.context.owner == owner)
        {
            return Ok(());
        }
        self.check_owner(owner)?;
        let entry = self
            .transitions
            .values_mut()
            .find(|entry| entry.stage.live() && entry.binding.context.owner == owner)
            .expect("one live transaction was checked under this mutex");
        if entry.stage != ExecStage::Redeemed || entry.binding.context.prepared_mm != mm {
            return Err(ExecControlError::Transition);
        }
        entry.marked_past_first_exec = true;
        Ok(())
    }

    /// Classify clean EOF for this exact accepted native connection incarnation.
    /// `None` denotes the admitted initial stream; a successor tag names only
    /// its own consumed ticket. Delayed EOF remains valid after the later
    /// transition completes. Prepared/cancelled attempts and another image's
    /// Issue cannot grant permission. Physical terminal closure is separately
    /// owned by the real watch; this method never turns EOF into death.
    pub fn permits_connection_close(
        &mut self,
        peer: &OwnedFd,
        predecessor: Option<ExecTicket>,
    ) -> Result<bool, ExecControlError> {
        self.check_owner(self.proof.owner())?;
        if !self.proof.matches_peer(peer) {
            return Err(ExecControlError::Identity);
        }
        let mm = match predecessor {
            None => match self.initial_mm {
                Some(mm) => mm,
                None => return Ok(false),
            },
            Some(ticket) => {
                let entry = self
                    .transitions
                    .get(&ticket)
                    .ok_or(ExecControlError::Identity)?;
                if !matches!(entry.stage, ExecStage::Redeemed | ExecStage::Completed) {
                    return Err(ExecControlError::Transition);
                }
                entry.binding.context.prepared_mm
            }
        };
        Ok(self.transitions.values().any(|entry| {
            entry.binding.context.old_mm == mm
                && matches!(
                    entry.stage,
                    ExecStage::Issued | ExecStage::Redeemed | ExecStage::Completed
                )
        }))
    }

    /// Read the exact retained binding for provisioning or successor map proof.
    /// This does not issue or redeem the transaction.
    pub fn binding(
        &mut self,
        owner: DetTid,
        ticket: ExecTicket,
        mm: MmId,
    ) -> Result<NativeExecBinding, ExecControlError> {
        let entry = self.transaction(owner, ticket, mm)?;
        if !entry.stage.live() {
            return Err(ExecControlError::Transition);
        }
        Ok(entry.binding)
    }

    /// Read the retained Issued binding for the actual authenticated successor
    /// peer without asking the packet to supply its owner or image incarnation.
    /// This neither consumes nor authorizes delivery; redeem still performs all
    /// independent Config/composite checks and consumes before sending bytes.
    pub fn issued_binding(
        &mut self,
        peer: &OwnedFd,
        ticket: ExecTicket,
    ) -> Result<NativeExecBinding, ExecControlError> {
        self.check_owner(self.proof.owner())?;
        if !self.proof.matches_peer(peer) {
            return Err(ExecControlError::Identity);
        }
        let entry = self
            .transitions
            .get(&ticket)
            .ok_or(ExecControlError::Identity)?;
        if entry.stage != ExecStage::Issued {
            return Err(ExecControlError::Transition);
        }
        Ok(entry.binding)
    }

    /// Consume one Prepared+sealed provision attempt before constructing or
    /// sending any packet or descriptor. The caller holds the very same mutex
    /// as RPC/admission/watch callbacks through this affine lease. No scheduler
    /// lock may be acquired while the lease is held.
    pub fn begin_provision(
        &mut self,
        peer: &OwnedFd,
        ticket: ExecTicket,
    ) -> Result<NativeExecProvision<'_>, ExecControlError> {
        self.check_owner(self.proof.owner())?;
        if !self.proof.matches_peer(peer) {
            return Err(ExecControlError::Identity);
        }
        let entry = self
            .transitions
            .get_mut(&ticket)
            .ok_or(ExecControlError::Identity)?;
        if entry.stage != ExecStage::Prepared || entry.provision != ProvisionState::Fresh {
            return Err(ExecControlError::Transition);
        }
        let digest = entry.state_digest.ok_or(ExecControlError::Transition)?;
        let binding = entry.binding;
        entry.provision = ProvisionState::Consumed;
        Ok(NativeExecProvision {
            registry: self,
            ticket,
            binding,
            digest,
            completed: false,
        })
    }

    /// Seal the actual runtime bytes exactly once before final input provision.
    /// The complete digest covers version, both lengths, Tool and runtime bytes.
    pub fn seal(
        &mut self,
        owner: DetTid,
        ticket: ExecTicket,
        mm: MmId,
        runtime_digest: [u8; 32],
        runtime: &[u8],
    ) -> Result<[u8; 32], ExecControlError> {
        self.check_owner(owner)?;
        if runtime.is_empty() || runtime.len() > MAX_STATE_BYTES - STATE_HEADER - 1 {
            return Err(ExecControlError::Bounds);
        }
        if <[u8; 32]>::from(Sha256::digest(runtime)) != runtime_digest {
            return Err(ExecControlError::Digest);
        }
        if !self.proof.qualify_runtime(runtime) {
            return Err(ExecControlError::Qualification);
        }
        let entry = self.transaction(owner, ticket, mm)?;
        if entry.stage != ExecStage::Prepared || entry.state_digest.is_some() {
            return Err(ExecControlError::Transition);
        }
        let length = STATE_HEADER
            .checked_add(entry.tool.len())
            .and_then(|n| n.checked_add(runtime.len()))
            .ok_or(ExecControlError::Bounds)?;
        if length > MAX_STATE_BYTES {
            return Err(ExecControlError::Bounds);
        }
        let mut state = Vec::with_capacity(length);
        let mut header = [0; STATE_HEADER];
        state_frame::encode_header(
            entry.binding.cut,
            entry.tool.len(),
            runtime.len(),
            &mut header,
        )
        .map_err(|_| ExecControlError::Bounds)?;
        state.extend_from_slice(&header);
        state.extend_from_slice(&entry.tool);
        state.extend_from_slice(runtime);
        let digest = Sha256::digest(&state).into();
        entry.tool = Vec::new();
        entry.state = state;
        entry.state_digest = Some(digest);
        Ok(digest)
    }

    /// Last old-image edge. Sealed runtime and completed provision are required.
    pub fn issue(
        &mut self,
        owner: DetTid,
        ticket: ExecTicket,
        mm: MmId,
        digest: [u8; 32],
    ) -> Result<(), ExecControlError> {
        let entry = self.transaction(owner, ticket, mm)?;
        if entry.stage != ExecStage::Prepared
            || entry.state_digest != Some(digest)
            || entry.provision != ProvisionState::Delivered
        {
            return Err(ExecControlError::Transition);
        }
        entry.stage = ExecStage::Issued;
        Ok(())
    }

    /// Redeem against the actual accepted socket's pidfd. Consume first: failed
    /// ACK/config/state delivery can never make this ticket redeemable again.
    /// The unsafe launcher contract, not these credentials, proves native exec.
    pub fn redeem(
        &mut self,
        peer: &OwnedFd,
        ticket: ExecTicket,
        config: [u8; 32],
        digest: [u8; 32],
    ) -> Result<Vec<u8>, ExecControlError> {
        let owner = self.proof.owner();
        self.check_owner(owner)?;
        if !self.proof.matches_peer(peer) {
            return Err(ExecControlError::Identity);
        }
        let entry = self
            .transitions
            .get_mut(&ticket)
            .ok_or(ExecControlError::Identity)?;
        if entry.stage != ExecStage::Issued {
            return Err(ExecControlError::Transition);
        }
        if entry.binding.context.config_digest != config || entry.state_digest != Some(digest) {
            return Err(ExecControlError::Digest);
        }
        entry.stage = ExecStage::Redeemed;
        Ok(std::mem::take(&mut entry.state))
    }

    /// Returned exec errno or an earlier refusal cancels only pre-redemption.
    /// Ordinary Detcore CancelExec still performs its own full rollback.
    pub fn cancel(
        &mut self,
        owner: DetTid,
        ticket: ExecTicket,
        mm: MmId,
    ) -> Result<(), ExecControlError> {
        let entry = self.transaction(owner, ticket, mm)?;
        if !matches!(entry.stage, ExecStage::Prepared | ExecStage::Issued) {
            return Err(ExecControlError::Transition);
        }
        entry.stage = ExecStage::Cancelled;
        entry.tool = Vec::new();
        entry.state = Vec::new();
        Ok(())
    }

    /// The trusted successor reports completed real lifecycle and frontend
    /// verification, before the acknowledgment can release the guest loader.
    pub fn complete(
        &mut self,
        owner: DetTid,
        ticket: ExecTicket,
        mm: MmId,
    ) -> Result<(), ExecControlError> {
        let entry = self.transaction(owner, ticket, mm)?;
        if entry.stage != ExecStage::Redeemed {
            return Err(ExecControlError::Transition);
        }
        let binding = entry.binding;
        if !self.proof.qualify_completion(ticket, binding) {
            self.failed(ticket)?;
            return Err(ExecControlError::Qualification);
        }
        // Reborrow only after the proof; no mutable entry reference crosses it.
        self.transitions
            .get_mut(&ticket)
            .expect("retained Redeemed record")
            .stage = ExecStage::Completed;
        self.current_mm = Some(mm);
        self.current_files = binding.cut.old_files.for_exec(owner);
        Ok(())
    }

    /// Permanently fail this birth's native launch chain, retaining the Failed
    /// tombstone. Only explicit pre-redemption cancellation permits a retry.
    pub fn failed(&mut self, ticket: ExecTicket) -> Result<(), ExecControlError> {
        let entry = self
            .transitions
            .get_mut(&ticket)
            .ok_or(ExecControlError::Identity)?;
        if !entry.stage.live() {
            return Err(ExecControlError::Transition);
        }
        entry.stage = ExecStage::Failed;
        entry.tool = Vec::new();
        entry.state = Vec::new();
        self.terminal_failure = true;
        Ok(())
    }

    /// A same-birth authenticated provision stream became ambiguous before a
    /// usable ticket could be consumed. Preserve terminal records, fail all live
    /// transactions and prohibit retries for this birth. Unauthenticated peers
    /// must never call this method.
    pub fn provision_transport_failed(&mut self) {
        self.terminal_failure = true;
        for entry in self
            .transitions
            .values_mut()
            .filter(|entry| entry.stage.live())
        {
            entry.stage = ExecStage::Failed;
            entry.tool = Vec::new();
            entry.state = Vec::new();
        }
    }

    /// Called only by actual birth-watch death/failure, never replacement EOF.
    pub fn died(&mut self) {
        self.dead = true;
        for entry in self
            .transitions
            .values_mut()
            .filter(|entry| entry.stage.live())
        {
            entry.stage = ExecStage::Dead;
            entry.tool = Vec::new();
            entry.state = Vec::new();
        }
    }

    /// Observe the retained stage without mutating a transaction or its bytes.
    pub fn stage(&self, ticket: ExecTicket) -> Option<ExecStage> {
        self.transitions.get(&ticket).map(|entry| entry.stage)
    }
}

/// Affine host-only provision access. Dropping incomplete access poisons the
/// birth and retains its Failed tombstone. It is neither Clone nor Copy.
#[must_use = "an incomplete provision poisons the native birth"]
pub struct NativeExecProvision<'a> {
    registry: &'a mut NativeExecRegistry,
    ticket: ExecTicket,
    binding: NativeExecBinding,
    digest: [u8; 32],
    completed: bool,
}

impl NativeExecProvision<'_> {
    /// Exact independently qualified binding retained by this transaction.
    pub fn binding(&self) -> NativeExecBinding {
        self.binding
    }
    /// Exact complete Tool/runtime composite digest retained by Seal.
    pub fn state_digest(&self) -> [u8; 32] {
        self.digest
    }

    /// Record successful one-shot transfer before allowing Issue.
    ///
    /// # Safety
    /// The trusted host provider must have sent exactly the final fully sealed
    /// HLNEXEC1 input and held immutable role descriptors to this same accepted
    /// peer, with one exact bounded SCM_RIGHTS send and no partial/error result.
    /// This is host transport evidence only; guest FD validation, final cuts
    /// and original native exec proof remain launch-chain obligations.
    pub unsafe fn complete(mut self) -> Result<(), ExecControlError> {
        self.registry.check_owner(self.binding.context.owner)?;
        let entry = self.registry.transaction(
            self.binding.context.owner,
            self.ticket,
            self.binding.context.prepared_mm,
        )?;
        if entry.stage != ExecStage::Prepared
            || entry.provision != ProvisionState::Consumed
            || entry.state_digest != Some(self.digest)
        {
            return Err(ExecControlError::Transition);
        }
        entry.provision = ProvisionState::Delivered;
        self.completed = true;
        Ok(())
    }
}

impl Drop for NativeExecProvision<'_> {
    fn drop(&mut self) {
        if !self.completed {
            // Do not relabel Dead/Cancelled tombstones. The exclusive lease
            // excludes concurrent cancellation; failed completion may mark Dead.
            let _ = self.registry.failed(self.ticket);
            self.registry.terminal_failure = true;
        }
    }
}

/// Split an already-authenticated complete state frame with exact lengths.
/// This checks its structural format; the caller must separately authenticate
/// the expected digest and perform bounded exact decoding of both payloads.
pub fn split_state(bytes: &[u8]) -> Result<(&[u8], &[u8]), ExecControlError> {
    let (_, tool, runtime) = split_state_binding(bytes)?;
    Ok((tool, runtime))
}

/// Decode the admitted neutral identity outside the opaque Tool bytes. The
/// transport caller must first authenticate the entire expected frame digest.
pub fn split_state_binding(
    bytes: &[u8],
) -> Result<(NativeExecNeutralBinding, &[u8], &[u8]), ExecControlError> {
    state_frame::decode(bytes).map_err(|_| ExecControlError::Bounds)
}

/// Identify a native leaf through any number of envelopes. Malformed envelopes
/// still reach the native refusal path before ordinary forwarding or clocks.
pub(super) fn is_native_exec_control(mut request: &super::GlobalRequest) -> bool {
    use super::GlobalRequest as Q;
    loop {
        match request {
            Q::WithForwardedCount(_, inner) => request = inner,
            Q::PrepareNativeExec(_)
            | Q::SealNativeExecStats { .. }
            | Q::IssueNativeExec { .. }
            | Q::CancelNativeExec { .. }
            | Q::CompleteNativeExec { .. } => return true,
            _ => return false,
        }
    }
}

#[derive(Clone, Copy)]
enum ControlCut {
    Prepared,
    EmptyPrepared,
    Completed,
}

impl super::GlobalState {
    /// The complete actual host configuration retained by this GlobalState,
    /// including legitimate host-only fields omitted from the Tool wire.
    /// Native admission uses this owner rather than a separate caller Config.
    pub fn native_exec_host_config(&self) -> &crate::Config {
        &self.cfg
    }

    /// Install the genuine watched authority once, before native controls.
    /// Ordinary runs leave this absent. No request can create this capability.
    pub fn install_native_exec_registry(
        &self,
        registry: NativeExecRegistry,
    ) -> Result<(), ExecControlError> {
        self.native_exec_registry
            .set(std::sync::Mutex::new(registry))
            .map_err(|_| ExecControlError::Transition)
    }

    /// The very same retained store for native admission and birth watchers.
    /// Callbacks must drop this guard before acquiring the scheduler mutex.
    pub fn native_exec_registry(&self) -> Option<&std::sync::Mutex<NativeExecRegistry>> {
        self.native_exec_registry.get()
    }

    fn native_exec_transition<R>(
        &self,
        owner: DetTid,
        request_mm: MmId,
        produced: Option<u64>,
        cut: ControlCut,
        ticket: Option<ExecTicket>,
        transition: impl FnOnce(
            &mut NativeExecRegistry,
            Option<NativeExecContext>,
        ) -> Result<R, ExecControlError>,
    ) -> Result<R, ExecControlError> {
        let produced = produced.ok_or(ExecControlError::Forwarding)?;
        let registry = self
            .native_exec_registry
            .get()
            .ok_or(ExecControlError::Unavailable)?;
        // This is a synchronous neutral check, not lock_rpc_scheduler's parked
        // backend-failure continuation. Hold sched through the strict cut and
        // registry transition. Lock order is sched -> pending -> registry.
        let sched = self.sched.lock().unwrap();
        if sched.backend_failed() {
            return Err(ExecControlError::Dead);
        }
        if !self.cfg.sequentialize_threads
            || sched.registered_process(owner) != Some(owner)
            || !sched.thread_is_live(owner)
            || sched.thread_is_logically_killed(owner)
        {
            return Err(ExecControlError::Identity);
        }
        let live_group: Vec<_> = sched
            .thread_tree
            .my_thread_group(&owner)
            .into_iter()
            .filter(|tid| sched.thread_is_live(*tid))
            .collect();
        if live_group != [owner]
            || !sched.thread_tree.unreaped_children_of(owner).is_empty()
            || !sched.holds_serial_grant(owner)
        {
            return Err(ExecControlError::Preparation);
        }
        let pending = self.pending_exec_states.lock().unwrap();
        let context = match cut {
            ControlCut::Completed => {
                if !self
                    .past_first_execve
                    .load(std::sync::atomic::Ordering::SeqCst)
                    || pending.contains_key(&owner)
                    || sched.thread_mm(owner) != Some(request_mm)
                    || !sched.rpc_incarnation_matches(owner, request_mm)
                {
                    return Err(ExecControlError::Preparation);
                }
                None
            }
            ControlCut::Prepared | ControlCut::EmptyPrepared => {
                let prepared = pending.get(&owner).ok_or(ExecControlError::Preparation)?;
                // Native Detcore already stages its old-mm wakes in ThreadState.
                // The DBT coordinator vector has a separate existing owner.
                if !prepared.fd_blocking.robust_wakes.is_empty() {
                    return Err(ExecControlError::Preparation);
                }
                if prepared.caller != owner || prepared.process != owner {
                    return Err(ExecControlError::Identity);
                }
                if sched.thread_mm(owner) != Some(prepared.mm)
                    || !sched.rpc_incarnation_matches(owner, prepared.mm)
                    || request_mm != prepared.mm.for_exec(owner)
                {
                    return Err(ExecControlError::Identity);
                }
                Some(NativeExecContext {
                    owner,
                    old_mm: prepared.mm,
                    prepared_mm: request_mm,
                    config_digest: crate::native_exec::native_exec_config_digest(&self.cfg)
                        .map_err(|_| ExecControlError::Digest)?,
                })
            }
        };
        let mut registry = registry.lock().unwrap();
        registry.check_owner(owner)?;
        if let Some(ticket) = ticket {
            if registry.forwarding_receipt.is_none() {
                return Err(ExecControlError::Forwarding);
            }
            let binding = registry.binding(owner, ticket, request_mm)?;
            if context.is_some_and(|context| binding.context != context) {
                return Err(ExecControlError::Identity);
            }
            if matches!(cut, ControlCut::Completed)
                && !registry
                    .transaction(owner, ticket, request_mm)?
                    .marked_past_first_exec
            {
                return Err(ExecControlError::Preparation);
            }
        }
        registry.check_forwarding_cut(owner, produced, matches!(cut, ControlCut::EmptyPrepared))?;
        transition(&mut registry, context)
    }

    pub(super) fn dispatch_native_exec_control(
        &self,
        from: reverie::Tid,
        request: <Self as reverie::GlobalTool>::Request,
    ) -> <Self as reverie::GlobalTool>::Response {
        use super::GlobalRequest as Q;
        use super::GlobalResponse as R;
        let (_, mm, mut request) = request;
        let mut produced = None;
        let mut envelopes = 0usize;
        loop {
            match request {
                Q::WithForwardedCount(count, inner) => {
                    envelopes = envelopes.saturating_add(1);
                    produced = Some(count);
                    request = *inner;
                }
                leaf => {
                    request = leaf;
                    break;
                }
            }
        }
        if envelopes != 1 {
            produced = None;
        }
        let owner = DetTid::from_raw(from.as_raw());
        let response = match request {
            Q::PrepareNativeExec(request) => R::PrepareNativeExec(self.native_exec_transition(
                owner,
                mm,
                produced,
                ControlCut::Prepared,
                None,
                |registry, context| {
                    registry.prepare(
                        context.expect("prepared native cut supplies context"),
                        request,
                    )
                },
            )),
            Q::SealNativeExecStats {
                ticket,
                runtime_digest,
                runtime,
            } => R::SealNativeExecStats(self.native_exec_transition(
                owner,
                mm,
                produced,
                ControlCut::EmptyPrepared,
                Some(ticket),
                |registry, _| registry.seal(owner, ticket, mm, runtime_digest, &runtime),
            )),
            Q::IssueNativeExec {
                ticket,
                state_digest,
            } => R::IssueNativeExec(self.native_exec_transition(
                owner,
                mm,
                produced,
                ControlCut::EmptyPrepared,
                Some(ticket),
                |registry, _| registry.issue(owner, ticket, mm, state_digest),
            )),
            Q::CancelNativeExec { ticket } => R::CancelNativeExec(self.native_exec_transition(
                owner,
                mm,
                produced,
                ControlCut::Prepared,
                Some(ticket),
                |registry, _| registry.cancel(owner, ticket, mm),
            )),
            Q::CompleteNativeExec { ticket } => R::CompleteNativeExec(self.native_exec_transition(
                owner,
                mm,
                produced,
                ControlCut::Completed,
                Some(ticket),
                |registry, _| registry.complete(owner, ticket, mm),
            )),
            _ => unreachable!("only native leaves are dispatched on the neutral path"),
        };
        (None, response)
    }
}

#[cfg(test)]
mod control_tests {
    use std::os::fd::AsRawFd;
    use std::os::fd::FromRawFd;
    use std::os::fd::OwnedFd;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::time::Duration;

    use reverie::GlobalTool;
    use reverie::Tid;
    use sha2::Sha256;

    use super::*;
    use crate::config::Config;
    use crate::ivar::Ivar;
    use crate::scheduler::DEFAULT_PRIORITY;
    use crate::scheduler::ThreadNextTurn;
    use crate::tool_global::GlobalRequest as Q;
    use crate::tool_global::GlobalResponse as R;
    use crate::tool_global::GlobalState;
    use crate::tool_global::PendingExecState;
    use crate::types::DetTime;
    use crate::types::LogicalTime;

    // Protocol-only trusted proof mock. These tests prove coordinator guards,
    // actual socket counting and neutral dispatch; they do not prove watched
    // birth admission, immutable target maps, runtime qualification or exec.
    #[derive(Debug)]
    struct ProtocolProof {
        owner: DetTid,
        physical: Option<(i32, i32)>,
    }

    // SAFETY: test-only model with no production installer. All qualification
    // answers are deliberately modelled; actual source records below are real.
    unsafe impl NativeExecProof for ProtocolProof {
        fn owner(&self) -> DetTid {
            self.owner
        }
        fn physical_ids(&self) -> Option<(i32, i32)> {
            self.physical
        }
        fn is_live(&self) -> bool {
            true
        }
        fn matches_peer(&self, _: &OwnedFd) -> bool {
            true
        }
        fn qualify_target(&self, request: &PrepareNativeExec) -> bool {
            request.target_digest == [2; 32]
                && request.bootstrap_digest == [3; 32]
                && request.sockets.detlog.object() == (1, 2)
                && request.sockets.observer.object() == (1, 3)
        }
        fn qualify_runtime(&self, bytes: &[u8]) -> bool {
            bytes == b"protocol-runtime"
        }
        fn qualify_completion(&self, _: ExecTicket, _: NativeExecBinding) -> bool {
            // Explicit registry model only; not watched native execution proof.
            true
        }
    }

    fn registry(owner: DetTid, physical: Option<(i32, i32)>) -> NativeExecRegistry {
        NativeExecRegistry::new(Arc::new(ProtocolProof { owner, physical }), [7; 32]).unwrap()
    }

    fn fixture(install: bool, physical: Option<(i32, i32)>) -> (GlobalState, DetTid, MmId, MmId) {
        let cfg = Config {
            sequentialize_threads: true,
            ..Config::default()
        };
        let state = GlobalState::initialize(&cfg, false);
        let owner = DetTid::from_raw(41);
        let old_mm = MmId::initial(owner).for_exec(owner);
        {
            let mut sched = state.sched.lock().unwrap();
            sched.thread_tree.add_child(owner, owner, true);
            sched.next_turns.insert(
                owner,
                ThreadNextTurn {
                    dettid: owner,
                    child_tid_addr: 0,
                    req: Ivar::new(),
                    resp: Ivar::new(),
                    protocol: Default::default(),
                },
            );
            sched.priorities.insert(owner, DEFAULT_PRIORITY);
            sched.record_thread_mm(owner, old_mm);
            sched.install_test_exec_incarnation(owner, old_mm);
            sched.set_running_for_test(owner);
            assert!(sched.prepare_exec_teardown(owner, owner, old_mm));
        }
        state.pending_exec_states.lock().unwrap().insert(
            owner,
            PendingExecState {
                caller: owner,
                process: owner,
                mm: old_mm,
                fd_blocking: Default::default(),
            },
        );
        let mut clock = DetTime::new(&cfg);
        clock.add_syscall_with_cost(1337);
        state.global_time.lock().unwrap().update_global_time(
            owner,
            clock.as_nanos(),
            clock.inherited_nanos(),
        );
        if install {
            state
                .install_native_exec_registry(registry(owner, physical))
                .unwrap();
        }
        (state, owner, old_mm, old_mm.for_exec(owner))
    }

    fn prepare(state: &GlobalState, mm: MmId) -> Q {
        let bytes = b"protocol-tool".to_vec();
        let owner = DetTid::from_raw(41);
        let old_mm = state.pending_exec_states.lock().unwrap()[&owner].mm;
        let config_digest = crate::native_exec::native_exec_config_digest(&state.cfg).unwrap();
        let physical_tid = state
            .native_exec_registry()
            .and_then(|mutex| mutex.lock().unwrap().proof.physical_ids())
            .map_or(41, |ids| ids.1);
        Q::PrepareNativeExec(PrepareNativeExec {
            sockets: NativeExecSocketBindings {
                detlog: NativeExecSocketBinding {
                    fd: 1025,
                    device: 1,
                    inode: 2,
                },
                observer: NativeExecSocketBinding {
                    fd: 1026,
                    device: 1,
                    inode: 3,
                },
            },
            cut: NativeExecNeutralBinding {
                dettid: owner,
                detpid: owner,
                physical_tid,
                old_mm,
                old_files: FilesId::initial(owner),
                prepared_mm: mm,
                config_digest,
            },
            prepared_mm: mm,
            config_digest: crate::native_exec::native_exec_config_digest(&state.cfg).unwrap(),
            target_digest: [2; 32],
            bootstrap_digest: [3; 32],
            resume_mask: 0x42,
            snapshot_digest: Sha256::digest(&bytes).into(),
            snapshot: bytes,
        })
    }

    fn counted(count: u64, request: Q) -> Q {
        Q::WithForwardedCount(count, Box::new(request))
    }

    fn encoded<T: serde::Serialize>(value: &T) -> Vec<u8> {
        bincode::serde::encode_to_vec(value, bincode::config::legacy()).unwrap()
    }

    async fn neutral(state: &GlobalState, owner: DetTid, mm: MmId, request: Q) -> R {
        // Preserve a real stored deadline hint. A native response cannot consume
        // it, recompute it or wrap the result with WithDeadline.
        let now = state.global_time.lock().unwrap().as_nanos();
        {
            let mut sched = state.sched.lock().unwrap();
            sched.register_alarm(
                owner,
                owner,
                now,
                LogicalTime::from_nanos(5000),
                LogicalTime::ZERO,
                nix::sys::signal::Signal::SIGALRM,
            );
            sched.note_arming_hint(owner, now);
        }
        let global_before = encoded(&*state.global_time.lock().unwrap());
        let sched_before = format!("{:?}", *state.sched.lock().unwrap());
        let pending_before = state.pending_exec_states.lock().unwrap().clone();
        let mut guest_time = DetTime::new(&state.cfg);
        guest_time.add_syscall_with_cost(987654);
        let (time, response) = state
            .receive_rpc(
                Tid::from_raw(owner.as_raw()),
                (guest_time.clone(), mm, request),
            )
            .await;
        assert_eq!(time, None);
        assert!(!matches!(response, R::WithDeadline(..)));
        assert_eq!(
            encoded(&*state.global_time.lock().unwrap()),
            global_before,
            "complete GlobalTime remains unchanged"
        );
        assert_eq!(
            format!("{:?}", *state.sched.lock().unwrap()),
            sched_before,
            "complete scheduler debug state, including PRNGs, grants and hint, is unchanged"
        );
        assert_eq!(*state.pending_exec_states.lock().unwrap(), pending_before);
        assert_eq!(
            state.sched.lock().unwrap().take_deadline_hint(owner),
            Some(Duration::from_nanos(5000))
        );
        response
    }

    struct Source {
        send: OwnedFd,
        written: Arc<Mutex<Vec<Vec<u8>>>>,
    }

    impl Source {
        fn new() -> Self {
            let mut fds = [-1; 2];
            // SAFETY: writable pair storage; success creates two owned sockets.
            assert_eq!(
                unsafe {
                    libc::socketpair(
                        libc::AF_UNIX,
                        libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
                        0,
                        fds.as_mut_ptr(),
                    )
                },
                0
            );
            let receive = unsafe { OwnedFd::from_raw_fd(fds[0]) };
            let send = unsafe { OwnedFd::from_raw_fd(fds[1]) };
            let written = Arc::new(Mutex::new(Vec::new()));
            let sink = Arc::clone(&written);
            crate::detlog::set_forwarded_source(
                receive,
                Box::new(move |bytes| sink.lock().unwrap().push(bytes.to_vec())),
            );
            Self { send, written }
        }
        fn send(&self, physical: i32, index: u64, bytes: &[u8]) {
            let record = crate::detlog::tagged_forwarded_message(physical, index, bytes);
            // SAFETY: owned socket and valid initialized message storage.
            assert_eq!(
                unsafe {
                    libc::send(
                        self.send.as_raw_fd(),
                        record.as_ptr().cast(),
                        record.len(),
                        libc::MSG_NOSIGNAL,
                    )
                },
                record.len() as isize
            );
        }
    }

    // The forwarding source and determinism-loss latch are process-global.
    // A child libtest process avoids racing unrelated tests without clearing a
    // real retained loss or changing the completeness checks to admit this test.
    fn isolated(name: &str, body: impl FnOnce()) {
        const KEY: &str = "HERMIT_NATIVE_CONTROL_PROTOCOL_CHILD";
        if std::env::var(KEY).as_deref() == Ok(name) {
            body();
            return;
        }
        let full_name = format!("tool_global::native_exec::control_tests::{name}");
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &full_name, "--nocapture", "--test-threads=1"])
            .env(KEY, name)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{full_name}: {:?}\n{}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("running 1 test") && stdout.contains("1 passed; 0 failed"),
            "the exact child test must actually execute: {stdout}"
        );
    }

    #[test]
    fn native_exec_coordinator_robust_wakes_refuse_without_consuming_preparation() {
        isolated(
            "native_exec_coordinator_robust_wakes_refuse_without_consuming_preparation",
            || {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(async {
                        // Real healthy forwarding source and actual process ID;
                        // authority remains the existing explicit registry model.
                        let _source = Source::new();
                        let physical = i32::try_from(std::process::id()).unwrap();
                        let (state, owner, old_mm, mm) = fixture(true, Some((physical, physical)));
                        let clean_request = prepare(&state, mm);
                        let clean_ticket =
                            match neutral(&state, owner, mm, counted(0, clean_request)).await {
                                R::PrepareNativeExec(Ok(ticket)) => ticket,
                                response => panic!("clean preparation must qualify: {response:?}"),
                            };
                        assert_eq!(
                            neutral(
                                &state,
                                owner,
                                mm,
                                counted(
                                    0,
                                    Q::CancelNativeExec {
                                        ticket: clean_ticket
                                    }
                                ),
                            )
                            .await,
                            R::CancelNativeExec(Ok(()))
                        );
                        state
                            .pending_exec_states
                            .lock()
                            .unwrap()
                            .get_mut(&owner)
                            .unwrap()
                            .fd_blocking
                            .robust_wakes
                            .push((owner, crate::types::FutexID::private(old_mm, 0x456000)));
                        let request = prepare(&state, mm);
                        assert_eq!(
                            neutral(&state, owner, mm, counted(0, request)).await,
                            R::PrepareNativeExec(Err(ExecControlError::Preparation))
                        );
                        assert_eq!(
                            state.pending_exec_states.lock().unwrap()[&owner]
                                .fd_blocking
                                .robust_wakes,
                            vec![(owner, crate::types::FutexID::private(old_mm, 0x456000))]
                        );
                    });
            },
        );
    }

    #[test]
    fn native_exec_control_absent_and_envelope_refusals_are_neutral() {
        isolated(
            "native_exec_control_absent_and_envelope_refusals_are_neutral",
            || {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(async {
                        let (state, owner, _, mm) = fixture(false, None);
                        let ticket = ExecTicket([8; 32]);
                        let requests = [
                            (
                                prepare(&state, mm),
                                R::PrepareNativeExec(Err(ExecControlError::Unavailable)),
                            ),
                            (
                                Q::SealNativeExecStats {
                                    ticket,
                                    runtime_digest: [0; 32],
                                    runtime: vec![1],
                                },
                                R::SealNativeExecStats(Err(ExecControlError::Unavailable)),
                            ),
                            (
                                Q::IssueNativeExec {
                                    ticket,
                                    state_digest: [0; 32],
                                },
                                R::IssueNativeExec(Err(ExecControlError::Unavailable)),
                            ),
                            (
                                Q::CancelNativeExec { ticket },
                                R::CancelNativeExec(Err(ExecControlError::Unavailable)),
                            ),
                            (
                                Q::CompleteNativeExec { ticket },
                                R::CompleteNativeExec(Err(ExecControlError::Unavailable)),
                            ),
                        ];
                        for (request, expected) in requests {
                            assert_eq!(
                                neutral(&state, owner, mm, counted(0, request)).await,
                                expected
                            );
                        }
                        assert_eq!(
                            neutral(&state, owner, mm, prepare(&state, mm)).await,
                            R::PrepareNativeExec(Err(ExecControlError::Forwarding))
                        );
                        assert_eq!(
                            neutral(
                                &state,
                                owner,
                                mm,
                                counted(0, counted(0, prepare(&state, mm)))
                            )
                            .await,
                            R::PrepareNativeExec(Err(ExecControlError::Forwarding))
                        );
                        // Direct dispatch has the same interception before ordinary drain.
                        let before = format!("{:?}", *state.sched.lock().unwrap());
                        let response = state
                            .dispatch_rpc(
                                Tid::from_raw(owner.as_raw()),
                                (DetTime::new(&state.cfg), mm, prepare(&state, mm)),
                            )
                            .await;
                        assert_eq!(
                            response,
                            (
                                None,
                                R::PrepareNativeExec(Err(ExecControlError::Forwarding))
                            )
                        );
                        assert_eq!(format!("{:?}", *state.sched.lock().unwrap()), before);
                    });
            },
        );
    }

    #[test]
    fn native_exec_control_protocol_counts_real_source_and_completes_after_lifecycle() {
        isolated(
            "native_exec_control_protocol_counts_real_source_and_completes_after_lifecycle",
            || {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(async {
                        // The model proof supplies this test process's actual physical
                        // main PID/TID, independent of deterministic owner 41.
                        let physical = unsafe { libc::getpid() };
                        let source = Source::new();
                        let (state, owner, old, mm) = fixture(true, Some((physical, physical)));
                        // Model the already-owned serial grant's genuine log
                        // order using the existing commit API. Counts still
                        // come only from the real socket records below.
                        crate::detlog::write_forwarded_for(physical, true);
                        assert_eq!(
                            state.install_native_exec_registry(registry(
                                owner,
                                Some((physical, physical))
                            )),
                            Err(ExecControlError::Transition)
                        );
                        source.send(physical, 1, b"before-cut\n");
                        let ticket =
                            match neutral(&state, owner, mm, counted(1, prepare(&state, mm))).await
                            {
                                R::PrepareNativeExec(Ok(ticket)) => ticket,
                                response => panic!("prepare: {response:?}"),
                            };
                        assert_eq!(
                            *source.written.lock().unwrap(),
                            vec![b"before-cut\n".to_vec()]
                        );
                        let runtime = b"protocol-runtime".to_vec();
                        let digest = match neutral(
                            &state,
                            owner,
                            mm,
                            counted(
                                0,
                                Q::SealNativeExecStats {
                                    ticket,
                                    runtime_digest: Sha256::digest(&runtime).into(),
                                    runtime,
                                },
                            ),
                        )
                        .await
                        {
                            R::SealNativeExecStats(Ok(digest)) => digest,
                            response => panic!("seal: {response:?}"),
                        };
                        // Protocol fixture models host transfer only. The
                        // real neutral RPC cut and all assertions remain intact.
                        {
                            let provision_peer: OwnedFd =
                                std::fs::File::open("/dev/null").unwrap().into();
                            let mut provision_registry =
                                state.native_exec_registry().unwrap().lock().unwrap();
                            // SAFETY: fixture qualification is model authority.
                            unsafe {
                                provision_registry
                                    .begin_provision(&provision_peer, ticket)
                                    .unwrap()
                                    .complete()
                            }
                            .unwrap();
                        }
                        assert_eq!(
                            neutral(
                                &state,
                                owner,
                                mm,
                                counted(
                                    0,
                                    Q::IssueNativeExec {
                                        ticket,
                                        state_digest: digest
                                    }
                                )
                            )
                            .await,
                            R::IssueNativeExec(Ok(()))
                        );
                        assert_eq!(
                            neutral(
                                &state,
                                owner,
                                mm,
                                counted(0, Q::CompleteNativeExec { ticket })
                            )
                            .await,
                            R::CompleteNativeExec(Err(ExecControlError::Preparation))
                        );
                        // Model redemption only: this does not assert any Linux exec or
                        // native birth/mapping/runtime qualification. The ordinary
                        // MarkPastFirstExecve RPC below supplies real lifecycle state.
                        let peer = std::fs::File::open("/dev/null").unwrap().into();
                        let bytes = state
                            .native_exec_registry()
                            .unwrap()
                            .lock()
                            .unwrap()
                            .redeem(
                                &peer,
                                ticket,
                                crate::native_exec::native_exec_config_digest(&state.cfg).unwrap(),
                                digest,
                            )
                            .unwrap();
                        assert_eq!(<[u8; 32]>::from(Sha256::digest(&bytes)), digest);
                        {
                            let mut registry =
                                state.native_exec_registry().unwrap().lock().unwrap();
                            registry
                                .mark_past_first_exec(DetTid::from_raw(99), mm)
                                .unwrap();
                            assert!(
                                !registry
                                    .transaction(owner, ticket, mm)
                                    .unwrap()
                                    .marked_past_first_exec
                            );
                            assert_eq!(
                                registry.mark_past_first_exec(owner, old),
                                Err(ExecControlError::Transition)
                            );
                            assert!(
                                !registry
                                    .transaction(owner, ticket, mm)
                                    .unwrap()
                                    .marked_past_first_exec
                            );
                        }
                        // Alternate registration and pending-map consumption
                        // install the right current mm but cannot replace the
                        // actual MarkPastFirstExecve receipt for this ticket.
                        let pending = state
                            .pending_exec_states
                            .lock()
                            .unwrap()
                            .remove(&owner)
                            .unwrap();
                        {
                            let mut sched = state.sched.lock().unwrap();
                            sched.reconnect_after_exec(crate::scheduler::ExecReconnect {
                                caller: owner,
                                new_leader: owner,
                                detpid: owner,
                                pre_exec_mm: old,
                                post_exec_mm: mm,
                                child_tid_addr: 0,
                                reconnect_priority: None,
                            });
                        }
                        state
                            .past_first_execve
                            .store(true, std::sync::atomic::Ordering::SeqCst);
                        assert_eq!(
                            neutral(
                                &state,
                                owner,
                                mm,
                                counted(0, Q::CompleteNativeExec { ticket })
                            )
                            .await,
                            R::CompleteNativeExec(Err(ExecControlError::Preparation))
                        );
                        assert!(
                            !state
                                .native_exec_registry()
                                .unwrap()
                                .lock()
                                .unwrap()
                                .transaction(owner, ticket, mm)
                                .unwrap()
                                .marked_past_first_exec
                        );
                        // Restore the old-image protocol fixture, then exercise
                        // the real ordinary Mark RPC rather than its registry hook.
                        state
                            .pending_exec_states
                            .lock()
                            .unwrap()
                            .insert(owner, pending);
                        {
                            let mut sched = state.sched.lock().unwrap();
                            sched.record_thread_mm(owner, old);
                            sched.install_test_exec_incarnation(owner, old);
                        }
                        state
                            .past_first_execve
                            .store(false, std::sync::atomic::Ordering::SeqCst);
                        let mut mark_time = DetTime::new(&state.cfg);
                        mark_time.add_syscall_with_cost(987654);
                        let mark = state
                            .receive_rpc(
                                Tid::from_raw(owner.as_raw()),
                                (mark_time, mm, Q::MarkPastFirstExecve(owner, None, None)),
                            )
                            .await;
                        let mark_response = match mark.1 {
                            R::WithDeadline(_, response) => *response,
                            response => response,
                        };
                        assert_eq!(mark_response, R::MarkPastFirstExecve(Default::default()));
                        assert_eq!(state.sched.lock().unwrap().thread_mm(owner), Some(mm));
                        assert!(state.pending_exec_states.lock().unwrap().is_empty());
                        assert!(
                            state
                                .past_first_execve
                                .load(std::sync::atomic::Ordering::SeqCst)
                        );
                        assert_ne!(mm, old);
                        assert_eq!(
                            neutral(
                                &state,
                                owner,
                                old,
                                counted(0, Q::CompleteNativeExec { ticket })
                            )
                            .await,
                            R::CompleteNativeExec(Err(ExecControlError::Preparation))
                        );
                        assert_eq!(
                            neutral(
                                &state,
                                owner,
                                mm,
                                counted(0, Q::CompleteNativeExec { ticket })
                            )
                            .await,
                            R::CompleteNativeExec(Ok(()))
                        );
                        assert_eq!(
                            state
                                .native_exec_registry()
                                .unwrap()
                                .lock()
                                .unwrap()
                                .stage(ticket),
                            Some(ExecStage::Completed)
                        );
                        assert_eq!(
                            neutral(
                                &state,
                                owner,
                                mm,
                                counted(0, Q::CompleteNativeExec { ticket })
                            )
                            .await,
                            R::CompleteNativeExec(Err(ExecControlError::Transition))
                        );
                        crate::detlog::clear_forwarded_source();
                    });
            },
        );
    }

    #[test]
    fn native_exec_control_completed_cut_counts_real_post_exec_record() {
        isolated(
            "native_exec_control_completed_cut_counts_real_post_exec_record",
            || {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(async {
                        // Existing protocol model, not a watched kernel exec.
                        // The source, interval, ordinary Mark and neutral host
                        // Complete dispatch below are actual production paths.
                        let physical = unsafe { libc::getpid() };
                        let source = Source::new();
                        let (state, owner, _, mm) = fixture(true, Some((physical, physical)));
                        crate::detlog::write_forwarded_for(physical, true);
                        let ticket =
                            match neutral(&state, owner, mm, counted(0, prepare(&state, mm))).await
                            {
                                R::PrepareNativeExec(Ok(ticket)) => ticket,
                                response => panic!("prepare: {response:?}"),
                            };
                        let runtime = b"protocol-runtime".to_vec();
                        let digest = match neutral(
                            &state,
                            owner,
                            mm,
                            counted(
                                0,
                                Q::SealNativeExecStats {
                                    ticket,
                                    runtime_digest: Sha256::digest(&runtime).into(),
                                    runtime,
                                },
                            ),
                        )
                        .await
                        {
                            R::SealNativeExecStats(Ok(digest)) => digest,
                            response => panic!("seal: {response:?}"),
                        };
                        {
                            let peer = std::fs::File::open("/dev/null").unwrap().into();
                            let mut registry =
                                state.native_exec_registry().unwrap().lock().unwrap();
                            // SAFETY: existing test-only provider qualification model.
                            unsafe { registry.begin_provision(&peer, ticket).unwrap().complete() }
                                .unwrap();
                        }
                        assert_eq!(
                            neutral(
                                &state,
                                owner,
                                mm,
                                counted(
                                    0,
                                    Q::IssueNativeExec {
                                        ticket,
                                        state_digest: digest
                                    }
                                )
                            )
                            .await,
                            R::IssueNativeExec(Ok(()))
                        );
                        let peer = std::fs::File::open("/dev/null").unwrap().into();
                        state
                            .native_exec_registry()
                            .unwrap()
                            .lock()
                            .unwrap()
                            .redeem(
                                &peer,
                                ticket,
                                crate::native_exec::native_exec_config_digest(&state.cfg).unwrap(),
                                digest,
                            )
                            .unwrap();
                        // Match the existing protocol fixture's ordinary Mark
                        // clock, after its retained initial time of 1337 ns.
                        let mut mark_time = DetTime::new(&state.cfg);
                        mark_time.add_syscall_with_cost(987654);
                        let mark = state
                            .receive_rpc(
                                Tid::from_raw(owner.as_raw()),
                                (mark_time, mm, Q::MarkPastFirstExecve(owner, None, None)),
                            )
                            .await;
                        let response = match mark.1 {
                            R::WithDeadline(_, response) => *response,
                            response => response,
                        };
                        assert_eq!(response, R::MarkPastFirstExecve(Default::default()));
                        assert!(state.pending_exec_states.lock().unwrap().is_empty());
                        assert_eq!(
                            state
                                .native_exec_registry()
                                .unwrap()
                                .lock()
                                .unwrap()
                                .stage(ticket),
                            Some(ExecStage::Redeemed)
                        );
                        source.send(physical, 1, b"successor-post-exec\n");
                        assert!(source.written.lock().unwrap().is_empty());
                        // Unlike Seal/Issue, Completed owns the actual nonzero
                        // interval. neutral() also checks full scheduler/clock
                        // equality and retained deadline/pending-map invariants.
                        assert_eq!(
                            neutral(
                                &state,
                                owner,
                                mm,
                                counted(1, Q::CompleteNativeExec { ticket })
                            )
                            .await,
                            R::CompleteNativeExec(Ok(()))
                        );
                        assert_eq!(
                            *source.written.lock().unwrap(),
                            vec![b"successor-post-exec\n".to_vec()]
                        );
                        assert_eq!(
                            state
                                .native_exec_registry()
                                .unwrap()
                                .lock()
                                .unwrap()
                                .stage(ticket),
                            Some(ExecStage::Completed)
                        );
                        assert_eq!(
                            neutral(
                                &state,
                                owner,
                                mm,
                                counted(0, Q::CompleteNativeExec { ticket })
                            )
                            .await,
                            R::CompleteNativeExec(Err(ExecControlError::Transition))
                        );
                        assert_eq!(
                            *source.written.lock().unwrap(),
                            vec![b"successor-post-exec\n".to_vec()]
                        );
                        crate::detlog::clear_forwarded_source();
                    });
            },
        );
    }

    #[test]
    fn native_exec_control_protocol_refuses_wrong_context_late_records_and_source_change() {
        isolated(
            "native_exec_control_protocol_refuses_wrong_context_late_records_and_source_change",
            || {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(async {
                        let physical = unsafe { libc::getpid() };
                        let source = Source::new();
                        let (missing, owner, _, mm) = fixture(true, None);
                        assert_eq!(
                            neutral(&missing, owner, mm, counted(0, prepare(&missing, mm))).await,
                            R::PrepareNativeExec(Err(ExecControlError::Identity))
                        );
                        let (wrong_proof, owner, _, mm) = fixture(false, None);
                        wrong_proof
                            .install_native_exec_registry(registry(
                                DetTid::from_raw(99),
                                Some((physical, physical)),
                            ))
                            .unwrap();
                        assert_eq!(
                            neutral(
                                &wrong_proof,
                                owner,
                                mm,
                                counted(0, prepare(&wrong_proof, mm))
                            )
                            .await,
                            R::PrepareNativeExec(Err(ExecControlError::Identity))
                        );
                        let (state, owner, old, mm) = fixture(true, Some((physical, physical)));
                        assert_eq!(
                            neutral(&state, owner, old, counted(0, prepare(&state, mm))).await,
                            R::PrepareNativeExec(Err(ExecControlError::Identity))
                        );
                        let missing_preparation_request = prepare(&state, mm);
                        let prepared = state
                            .pending_exec_states
                            .lock()
                            .unwrap()
                            .remove(&owner)
                            .unwrap();
                        assert_eq!(
                            neutral(&state, owner, mm, counted(0, missing_preparation_request))
                                .await,
                            R::PrepareNativeExec(Err(ExecControlError::Preparation))
                        );
                        state
                            .pending_exec_states
                            .lock()
                            .unwrap()
                            .insert(owner, prepared.clone());
                        state
                            .pending_exec_states
                            .lock()
                            .unwrap()
                            .get_mut(&owner)
                            .unwrap()
                            .caller = DetTid::from_raw(99);
                        assert_eq!(
                            neutral(&state, owner, mm, counted(0, prepare(&state, mm))).await,
                            R::PrepareNativeExec(Err(ExecControlError::Identity))
                        );
                        state
                            .pending_exec_states
                            .lock()
                            .unwrap()
                            .insert(owner, prepared);
                        let child = DetTid::from_raw(42);
                        state
                            .sched
                            .lock()
                            .unwrap()
                            .thread_tree
                            .add_child(owner, child, true);
                        assert_eq!(
                            neutral(&state, owner, mm, counted(0, prepare(&state, mm))).await,
                            R::PrepareNativeExec(Err(ExecControlError::Preparation))
                        );
                        assert_eq!(
                            state
                                .sched
                                .lock()
                                .unwrap()
                                .thread_tree
                                .unreaped_children_of(owner),
                            vec![child],
                            "an exited/unregistered but unreaped child still refuses"
                        );

                        let (siblings, owner, _, mm) = fixture(true, Some((physical, physical)));
                        {
                            let mut sched = siblings.sched.lock().unwrap();
                            sched.thread_tree.add_child(owner, child, false);
                            sched.next_turns.insert(
                                child,
                                ThreadNextTurn {
                                    dettid: child,
                                    child_tid_addr: 0,
                                    req: Ivar::new(),
                                    resp: Ivar::new(),
                                    protocol: Default::default(),
                                },
                            );
                            assert!(sched.holds_serial_grant(owner));
                            assert!(sched.thread_tree.unreaped_children_of(owner).is_empty());
                        }
                        assert_eq!(
                            neutral(&siblings, owner, mm, counted(0, prepare(&siblings, mm))).await,
                            R::PrepareNativeExec(Err(ExecControlError::Preparation))
                        );

                        let (failed, owner, _, mm) = fixture(true, Some((physical, physical)));
                        failed.report_backend_failure(reverie::BackendFailure {
                            pid: reverie::Pid::from_raw(owner.as_raw()),
                            tid: Tid::from_raw(owner.as_raw()),
                            phase: "native control protocol test",
                        });
                        assert_eq!(
                            neutral(&failed, owner, mm, counted(0, prepare(&failed, mm))).await,
                            R::PrepareNativeExec(Err(ExecControlError::Dead))
                        );

                        let (state, owner, _, mm) = fixture(true, Some((physical, physical)));
                        source.send(physical, 1, b"counted\n");
                        let ticket =
                            match neutral(&state, owner, mm, counted(1, prepare(&state, mm))).await
                            {
                                R::PrepareNativeExec(Ok(ticket)) => ticket,
                                response => panic!("prepare: {response:?}"),
                            };
                        let runtime = b"protocol-runtime".to_vec();
                        let digest = match neutral(
                            &state,
                            owner,
                            mm,
                            counted(
                                0,
                                Q::SealNativeExecStats {
                                    ticket,
                                    runtime_digest: Sha256::digest(&runtime).into(),
                                    runtime,
                                },
                            ),
                        )
                        .await
                        {
                            R::SealNativeExecStats(Ok(digest)) => digest,
                            response => panic!("seal: {response:?}"),
                        };
                        source.send(physical, 1, b"after-seal\n");
                        // A declared nonempty interval refuses before consuming it.
                        assert_eq!(
                            neutral(
                                &state,
                                owner,
                                mm,
                                counted(
                                    1,
                                    Q::IssueNativeExec {
                                        ticket,
                                        state_digest: digest
                                    }
                                )
                            )
                            .await,
                            R::IssueNativeExec(Err(ExecControlError::Forwarding))
                        );
                        assert_eq!(
                            state
                                .native_exec_registry()
                                .unwrap()
                                .lock()
                                .unwrap()
                                .stage(ticket),
                            Some(ExecStage::Prepared)
                        );
                        // Cancellation may account the actual retained nonempty interval.
                        assert_eq!(
                            neutral(
                                &state,
                                owner,
                                mm,
                                counted(1, Q::CancelNativeExec { ticket })
                            )
                            .await,
                            R::CancelNativeExec(Ok(()))
                        );
                        assert_eq!(
                            *source.written.lock().unwrap(),
                            vec![b"counted\n".to_vec(), b"after-seal\n".to_vec()]
                        );
                        assert!(
                            state
                                .pending_exec_states
                                .lock()
                                .unwrap()
                                .contains_key(&owner),
                            "native cancellation never replaces ordinary CancelExec"
                        );
                        let _replacement = Source::new();
                        assert_eq!(
                            neutral(&state, owner, mm, counted(0, prepare(&state, mm))).await,
                            R::PrepareNativeExec(Err(ExecControlError::Forwarding))
                        );
                        assert_eq!(
                            state
                                .native_exec_registry()
                                .unwrap()
                                .lock()
                                .unwrap()
                                .stage(ticket),
                            Some(ExecStage::Cancelled)
                        );
                        crate::detlog::clear_forwarded_source();
                    });
            },
        );
    }
    #[test]
    fn native_exec_control_protocol_refuses_zero_claim_with_actual_late_record() {
        isolated(
            "native_exec_control_protocol_refuses_zero_claim_with_actual_late_record",
            || {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(async {
                        let physical = unsafe { libc::getpid() };
                        let source = Source::new();
                        let (state, owner, _, mm) = fixture(true, Some((physical, physical)));
                        let ticket =
                            match neutral(&state, owner, mm, counted(0, prepare(&state, mm))).await
                            {
                                R::PrepareNativeExec(Ok(ticket)) => ticket,
                                response => panic!("prepare: {response:?}"),
                            };
                        let runtime = b"protocol-runtime".to_vec();
                        let digest = match neutral(
                            &state,
                            owner,
                            mm,
                            counted(
                                0,
                                Q::SealNativeExecStats {
                                    ticket,
                                    runtime_digest: Sha256::digest(&runtime).into(),
                                    runtime,
                                },
                            ),
                        )
                        .await
                        {
                            R::SealNativeExecStats(Ok(digest)) => digest,
                            response => panic!("seal: {response:?}"),
                        };
                        source.send(physical, 1, b"actual-late-record\n");
                        assert_eq!(
                            neutral(
                                &state,
                                owner,
                                mm,
                                counted(
                                    0,
                                    Q::IssueNativeExec {
                                        ticket,
                                        state_digest: digest
                                    }
                                )
                            )
                            .await,
                            R::IssueNativeExec(Err(ExecControlError::Forwarding))
                        );
                        assert!(crate::detlog::determinism_loss().is_some());
                        assert_eq!(
                            state
                                .native_exec_registry()
                                .unwrap()
                                .lock()
                                .unwrap()
                                .stage(ticket),
                            Some(ExecStage::Prepared)
                        );
                        assert_eq!(
                            neutral(
                                &state,
                                owner,
                                mm,
                                counted(1, Q::CancelNativeExec { ticket })
                            )
                            .await,
                            R::CancelNativeExec(Err(ExecControlError::Forwarding)),
                            "a persistent forwarding loss cannot be waived by cancellation"
                        );
                        assert_eq!(
                            state
                                .native_exec_registry()
                                .unwrap()
                                .lock()
                                .unwrap()
                                .stage(ticket),
                            Some(ExecStage::Prepared)
                        );
                        assert!(source.written.lock().unwrap().iter().any(|record| {
                            record.starts_with(crate::detlog::FORWARDING_LOSS_NOTICE.as_bytes())
                        }));
                        crate::detlog::clear_forwarded_source();
                        assert!(
                            crate::detlog::determinism_loss().is_some(),
                            "source cleanup never clears the retained failure"
                        );
                    });
            },
        );
    }

    #[test]
    fn native_exec_control_protocol_refuses_missing_actual_record() {
        isolated(
            "native_exec_control_protocol_refuses_missing_actual_record",
            || {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(async {
                        let physical = unsafe { libc::getpid() };
                        let source = Source::new();
                        let (state, owner, _, mm) = fixture(true, Some((physical, physical)));
                        // The actual registered socket has zero records. Declaring one
                        // is a negative completeness case, never a fabricated success.
                        assert_eq!(
                            neutral(&state, owner, mm, counted(1, prepare(&state, mm))).await,
                            R::PrepareNativeExec(Err(ExecControlError::Forwarding))
                        );
                        assert!(
                            state
                                .native_exec_registry()
                                .unwrap()
                                .lock()
                                .unwrap()
                                .transitions
                                .is_empty()
                        );
                        assert!(crate::detlog::determinism_loss().is_some());
                        assert!(source.written.lock().unwrap().iter().any(|record| {
                            record.starts_with(crate::detlog::FORWARDING_LOSS_NOTICE.as_bytes())
                        }));
                        assert_eq!(
                            neutral(&state, owner, mm, counted(0, prepare(&state, mm))).await,
                            R::PrepareNativeExec(Err(ExecControlError::Forwarding))
                        );
                        crate::detlog::clear_forwarded_source();
                        assert!(crate::detlog::determinism_loss().is_some());
                    });
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use std::os::fd::AsRawFd;
    use std::os::fd::FromRawFd;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;

    use super::*;

    // This proof deliberately models qualification. These tests establish
    // registry transitions, not immutable mappings, a watcher or native exec.
    #[derive(Debug)]
    struct Proof {
        live: AtomicBool,
        pidfd: OwnedFd,
    }

    fn same_file(left: &OwnedFd, right: &OwnedFd) -> bool {
        let mut a = std::mem::MaybeUninit::<libc::stat>::uninit();
        let mut b = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: valid owned descriptors and writable stat storage.
        unsafe {
            if libc::fstat(left.as_raw_fd(), a.as_mut_ptr()) != 0
                || libc::fstat(right.as_raw_fd(), b.as_mut_ptr()) != 0
            {
                return false;
            }
            let (a, b) = (a.assume_init(), b.assume_init());
            (a.st_dev, a.st_ino) == (b.st_dev, b.st_ino)
        }
    }

    // SAFETY: test-only authority; no installer/guest execution uses it. The
    // peer check compares genuine held pidfds, while qualification is modelled.
    unsafe impl NativeExecProof for Proof {
        fn owner(&self) -> DetTid {
            DetTid::from_raw(41)
        }
        fn physical_ids(&self) -> Option<(i32, i32)> {
            // Explicit registry model, not a native process admission.
            Some((41, 41))
        }
        fn is_live(&self) -> bool {
            self.live.load(Ordering::SeqCst)
        }
        fn matches_peer(&self, peer: &OwnedFd) -> bool {
            same_file(&self.pidfd, peer)
        }
        fn qualify_target(&self, request: &PrepareNativeExec) -> bool {
            request.target_digest == [2; 32]
                && request.bootstrap_digest == [3; 32]
                && request.resume_mask == 0x42
                && request.sockets.detlog.object() == (1, 2)
                && request.sockets.observer.object() == (1, 3)
        }
        fn qualify_runtime(&self, bytes: &[u8]) -> bool {
            bytes == b"actual-runtime-model"
        }
        fn qualify_completion(&self, _: ExecTicket, _: NativeExecBinding) -> bool {
            // Explicit registry model only; not watched native execution proof.
            true
        }
    }

    fn fixture() -> (
        NativeExecRegistry,
        Arc<Proof>,
        NativeExecContext,
        PrepareNativeExec,
    ) {
        // SAFETY: read-only process identity and a new owned pidfd on success.
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, libc::getpid(), 0) };
        assert!(raw >= 0, "pidfd_open: {}", std::io::Error::last_os_error());
        let proof = Arc::new(Proof {
            live: AtomicBool::new(true),
            pidfd: unsafe { OwnedFd::from_raw_fd(raw as i32) },
        });
        let owner = proof.owner();
        let old_mm = MmId::initial(owner).for_exec(owner);
        let context = NativeExecContext {
            owner,
            old_mm,
            prepared_mm: old_mm.for_exec(owner),
            config_digest: [1; 32],
        };
        let request = PrepareNativeExec {
            sockets: NativeExecSocketBindings {
                detlog: NativeExecSocketBinding {
                    fd: 1025,
                    device: 1,
                    inode: 2,
                },
                observer: NativeExecSocketBinding {
                    fd: 1026,
                    device: 1,
                    inode: 3,
                },
            },
            cut: NativeExecNeutralBinding {
                dettid: owner,
                detpid: owner,
                physical_tid: proof.physical_ids().unwrap().1,
                old_mm,
                old_files: FilesId::initial(owner),
                prepared_mm: context.prepared_mm,
                config_digest: context.config_digest,
            },
            prepared_mm: context.prepared_mm,
            config_digest: context.config_digest,
            target_digest: [2; 32],
            bootstrap_digest: [3; 32],
            resume_mask: 0x42,
            snapshot_digest: Sha256::digest(b"actual-tool-model").into(),
            snapshot: b"actual-tool-model".to_vec(),
        };
        (
            NativeExecRegistry::new(proof.clone(), [7; 32]).unwrap(),
            proof,
            context,
            request,
        )
    }

    fn seal(
        registry: &mut NativeExecRegistry,
        context: NativeExecContext,
        ticket: ExecTicket,
    ) -> [u8; 32] {
        let runtime = b"actual-runtime-model";
        registry
            .seal(
                context.owner,
                ticket,
                context.prepared_mm,
                Sha256::digest(runtime).into(),
                runtime,
            )
            .unwrap()
    }

    #[test]
    fn native_registry_retains_exact_binding_and_completes_once() {
        let (mut registry, proof, context, request) = fixture();
        let ticket = registry.prepare(context, request.clone()).unwrap();
        let binding = registry
            .binding(context.owner, ticket, context.prepared_mm)
            .unwrap();
        assert_eq!(binding.context, context);
        assert_eq!(binding.sockets, request.sockets);
        assert_eq!(binding.cut, request.cut);
        assert_eq!(binding.target_digest, request.target_digest);
        assert_eq!(binding.bootstrap_digest, request.bootstrap_digest);
        assert_eq!(binding.resume_mask, request.resume_mask);
        assert_eq!(binding.snapshot_digest, request.snapshot_digest);
        assert_eq!(
            registry.prepare(context, request.clone()),
            Err(ExecControlError::Transition)
        );
        assert_eq!(
            registry.issue(context.owner, ticket, context.prepared_mm, [0; 32]),
            Err(ExecControlError::Transition)
        );
        assert_eq!(
            registry.redeem(&proof.pidfd, ticket, context.config_digest, [0; 32]),
            Err(ExecControlError::Transition)
        );
        let digest = seal(&mut registry, context, ticket);
        assert_eq!(
            registry.seal(
                context.owner,
                ticket,
                context.prepared_mm,
                Sha256::digest(b"actual-runtime-model").into(),
                b"actual-runtime-model"
            ),
            Err(ExecControlError::Transition)
        );
        // Model transport completion setup; existing assertions unchanged.
        // SAFETY: fixture models transfer, not native execution.
        unsafe {
            registry
                .begin_provision(&proof.pidfd, ticket)
                .unwrap()
                .complete()
        }
        .unwrap();
        registry
            .issue(context.owner, ticket, context.prepared_mm, digest)
            .unwrap();
        assert_eq!(
            registry.issue(context.owner, ticket, context.prepared_mm, digest),
            Err(ExecControlError::Transition)
        );
        let state = registry
            .redeem(&proof.pidfd, ticket, context.config_digest, digest)
            .unwrap();
        assert_eq!(<[u8; 32]>::from(Sha256::digest(&state)), digest);
        assert_eq!(
            split_state(&state),
            Ok((
                request.snapshot.as_slice(),
                b"actual-runtime-model".as_slice()
            ))
        );
        assert_eq!(registry.stage(ticket), Some(ExecStage::Redeemed));
        assert_eq!(
            registry.redeem(&proof.pidfd, ticket, context.config_digest, digest),
            Err(ExecControlError::Transition)
        );
        assert_eq!(
            registry.cancel(context.owner, ticket, context.prepared_mm),
            Err(ExecControlError::Transition)
        );
        registry
            .complete(context.owner, ticket, context.prepared_mm)
            .unwrap();
        assert_eq!(
            registry.complete(context.owner, ticket, context.prepared_mm),
            Err(ExecControlError::Transition)
        );
        assert_eq!(registry.stage(ticket), Some(ExecStage::Completed));
        assert_eq!(
            registry.prepare(context, request.clone()),
            Err(ExecControlError::Identity)
        );
        let next_context = NativeExecContext {
            old_mm: context.prepared_mm,
            prepared_mm: context.prepared_mm.for_exec(context.owner),
            ..context
        };
        let next_request = PrepareNativeExec {
            cut: NativeExecNeutralBinding {
                old_mm: next_context.old_mm,
                old_files: request.cut.old_files.for_exec(context.owner),
                prepared_mm: next_context.prepared_mm,
                ..request.cut
            },
            prepared_mm: next_context.prepared_mm,
            ..request
        };
        let next = registry.prepare(next_context, next_request).unwrap();
        assert_ne!(next, ticket);
        assert_eq!(registry.stage(ticket), Some(ExecStage::Completed));
    }

    #[test]
    fn model_socket_bindings_refuse_invalid_slots_aliases_and_unheld_objects() {
        let (mut registry, _, context, request) = fixture();
        let original = request.sockets;
        let mut structural = Vec::new();
        for fd in [-1, 0, 1, 2, 198, 199, 200, 201, 202] {
            structural.push(NativeExecSocketBindings {
                detlog: NativeExecSocketBinding {
                    fd,
                    ..original.detlog
                },
                ..original
            });
            structural.push(NativeExecSocketBindings {
                observer: NativeExecSocketBinding {
                    fd,
                    ..original.observer
                },
                ..original
            });
        }
        structural.push(NativeExecSocketBindings {
            observer: NativeExecSocketBinding {
                fd: original.detlog.fd,
                ..original.observer
            },
            ..original
        });
        structural.push(NativeExecSocketBindings {
            observer: NativeExecSocketBinding {
                device: original.detlog.device,
                inode: original.detlog.inode,
                ..original.observer
            },
            ..original
        });
        for sockets in structural {
            assert_eq!(
                registry.prepare(
                    context,
                    PrepareNativeExec {
                        sockets,
                        ..request.clone()
                    }
                ),
                Err(ExecControlError::Identity),
                "invalid socket shape {sockets:?}"
            );
            assert!(registry.transitions.is_empty());
        }
        for sockets in [
            NativeExecSocketBindings {
                detlog: NativeExecSocketBinding {
                    device: 9,
                    ..original.detlog
                },
                ..original
            },
            NativeExecSocketBindings {
                detlog: NativeExecSocketBinding {
                    inode: 9,
                    ..original.detlog
                },
                ..original
            },
            NativeExecSocketBindings {
                observer: NativeExecSocketBinding {
                    device: 9,
                    ..original.observer
                },
                ..original
            },
            NativeExecSocketBindings {
                observer: NativeExecSocketBinding {
                    inode: 9,
                    ..original.observer
                },
                ..original
            },
            NativeExecSocketBindings {
                detlog: original.observer,
                observer: original.detlog,
            },
        ] {
            assert_eq!(
                registry.prepare(
                    context,
                    PrepareNativeExec {
                        sockets,
                        ..request.clone()
                    }
                ),
                Err(ExecControlError::Qualification),
                "unheld socket objects {sockets:?}"
            );
            assert!(registry.transitions.is_empty());
        }
        let ticket = registry.prepare(context, request).unwrap();
        assert_eq!(
            registry
                .binding(context.owner, ticket, context.prepared_mm)
                .unwrap()
                .sockets,
            original
        );
    }

    #[test]
    fn model_socket_bindings_are_per_transaction_and_cancelled_mapping_cannot_provision() {
        let (mut registry, proof, context, request) = fixture();
        let first = registry.prepare(context, request.clone()).unwrap();
        let digest = seal(&mut registry, context, first);
        // SAFETY: modeled transfer only, never native execution authority.
        unsafe {
            registry
                .begin_provision(&proof.pidfd, first)
                .unwrap()
                .complete()
        }
        .unwrap();
        registry
            .issue(context.owner, first, context.prepared_mm, digest)
            .unwrap();
        registry
            .redeem(&proof.pidfd, first, context.config_digest, digest)
            .unwrap();
        registry
            .complete(context.owner, first, context.prepared_mm)
            .unwrap();

        let next = NativeExecContext {
            old_mm: context.prepared_mm,
            prepared_mm: context.prepared_mm.for_exec(context.owner),
            ..context
        };
        let current = NativeExecSocketBindings {
            detlog: NativeExecSocketBinding {
                fd: 3025,
                ..request.sockets.detlog
            },
            observer: NativeExecSocketBinding {
                fd: 3026,
                ..request.sockets.observer
            },
        };
        let next_request = PrepareNativeExec {
            sockets: current,
            cut: NativeExecNeutralBinding {
                old_mm: next.old_mm,
                old_files: request.cut.old_files.for_exec(context.owner),
                prepared_mm: next.prepared_mm,
                ..request.cut
            },
            prepared_mm: next.prepared_mm,
            ..request.clone()
        };
        let cancelled = registry.prepare(next, next_request.clone()).unwrap();
        assert_eq!(
            registry
                .binding(next.owner, cancelled, next.prepared_mm)
                .unwrap()
                .sockets,
            current
        );
        registry
            .cancel(next.owner, cancelled, next.prepared_mm)
            .unwrap();
        assert_eq!(
            registry.begin_provision(&proof.pidfd, cancelled).err(),
            Some(ExecControlError::Transition)
        );
        assert_eq!(registry.stage(cancelled), Some(ExecStage::Cancelled));

        let fresh = NativeExecSocketBindings {
            detlog: NativeExecSocketBinding {
                fd: 4025,
                ..current.detlog
            },
            observer: NativeExecSocketBinding {
                fd: 4026,
                ..current.observer
            },
        };
        let third = registry
            .prepare(
                next,
                PrepareNativeExec {
                    sockets: fresh,
                    ..next_request
                },
            )
            .unwrap();
        assert_ne!(first, third);
        assert_ne!(cancelled, third);
        assert_eq!(
            registry
                .binding(next.owner, third, next.prepared_mm)
                .unwrap()
                .sockets,
            fresh
        );
        assert_eq!(
            registry.transitions[&first].binding.sockets,
            request.sockets
        );
        assert_eq!(registry.transitions[&cancelled].binding.sockets, current);
        assert_eq!(registry.stage(first), Some(ExecStage::Completed));
        assert_eq!(
            registry.begin_provision(&proof.pidfd, cancelled).err(),
            Some(ExecControlError::Transition)
        );
        assert_eq!(registry.stage(third), Some(ExecStage::Prepared));
        assert_eq!(
            registry.begin_provision(&proof.pidfd, first).err(),
            Some(ExecControlError::Transition)
        );
        let _digest = seal(&mut registry, next, third);
        let provision = registry.begin_provision(&proof.pidfd, third).unwrap();
        assert_eq!(provision.binding().sockets, fresh);
        // SAFETY: same modeled transport completion as the first transaction.
        unsafe { provision.complete() }.unwrap();
    }

    #[test]
    fn model_neutral_cut_rejects_owner_memory_table_physical_and_config_mismatch() {
        let (mut registry, _, context, request) = fixture();
        let other = DetTid::from_raw(42);
        for cut in [
            NativeExecNeutralBinding {
                dettid: other,
                ..request.cut
            },
            NativeExecNeutralBinding {
                detpid: other,
                ..request.cut
            },
            NativeExecNeutralBinding {
                physical_tid: 42,
                ..request.cut
            },
            NativeExecNeutralBinding {
                old_mm: context.prepared_mm,
                ..request.cut
            },
            NativeExecNeutralBinding {
                prepared_mm: context.old_mm,
                ..request.cut
            },
            NativeExecNeutralBinding {
                old_files: request.cut.old_files.for_exec(context.owner),
                ..request.cut
            },
            NativeExecNeutralBinding {
                config_digest: [9; 32],
                ..request.cut
            },
        ] {
            assert_eq!(
                registry.prepare(
                    context,
                    PrepareNativeExec {
                        cut,
                        ..request.clone()
                    }
                ),
                Err(ExecControlError::Identity)
            );
        }
        assert_eq!(registry.current_files, FilesId::initial(context.owner));
        let first = registry.prepare(context, request.clone()).unwrap();
        registry
            .cancel(context.owner, first, context.prepared_mm)
            .unwrap();
        assert_eq!(registry.current_files, FilesId::initial(context.owner));
        let retry = registry.prepare(context, request).unwrap();
        assert_ne!(first, retry);
    }

    #[test]
    fn native_registry_cancel_and_delivery_failure_never_reopen_a_ticket() {
        for issued in [false, true] {
            let (mut registry, proof, context, request) = fixture();
            let ticket = registry.prepare(context, request.clone()).unwrap();
            let digest = seal(&mut registry, context, ticket);
            if issued {
                // Model transport completion setup; existing assertions unchanged.
                // SAFETY: fixture models transfer, not native execution.
                unsafe {
                    registry
                        .begin_provision(&proof.pidfd, ticket)
                        .unwrap()
                        .complete()
                }
                .unwrap();
                registry
                    .issue(context.owner, ticket, context.prepared_mm, digest)
                    .unwrap();
            }
            registry
                .cancel(context.owner, ticket, context.prepared_mm)
                .unwrap();
            assert_eq!(registry.stage(ticket), Some(ExecStage::Cancelled));
            assert_eq!(
                registry.cancel(context.owner, ticket, context.prepared_mm),
                Err(ExecControlError::Transition)
            );
            assert_eq!(
                registry.redeem(&proof.pidfd, ticket, context.config_digest, digest),
                Err(ExecControlError::Transition)
            );
            assert_ne!(registry.prepare(context, request).unwrap(), ticket);
        }
        let (mut registry, proof, context, request) = fixture();
        let ticket = registry.prepare(context, request).unwrap();
        let digest = seal(&mut registry, context, ticket);
        // Model transport completion setup; existing assertions unchanged.
        // SAFETY: fixture models transfer, not native execution.
        unsafe {
            registry
                .begin_provision(&proof.pidfd, ticket)
                .unwrap()
                .complete()
        }
        .unwrap();
        registry
            .issue(context.owner, ticket, context.prepared_mm, digest)
            .unwrap();
        let _undelivered = registry
            .redeem(&proof.pidfd, ticket, context.config_digest, digest)
            .unwrap();
        registry.failed(ticket).unwrap();
        assert_eq!(registry.stage(ticket), Some(ExecStage::Failed));
        assert_eq!(
            registry.redeem(&proof.pidfd, ticket, context.config_digest, digest),
            Err(ExecControlError::Dead)
        );
        assert_eq!(
            registry.complete(context.owner, ticket, context.prepared_mm),
            Err(ExecControlError::Dead)
        );
    }

    #[test]
    fn native_registry_wrong_peer_owner_mm_and_digests_cannot_consume() {
        let (mut registry, proof, context, request) = fixture();
        let ticket = registry.prepare(context, request).unwrap();
        let digest = seal(&mut registry, context, ticket);
        let stranger = DetTid::from_raw(42);
        assert_eq!(
            registry.issue(stranger, ticket, context.prepared_mm, digest),
            Err(ExecControlError::Identity)
        );
        assert_eq!(
            registry.issue(context.owner, ticket, context.old_mm, digest),
            Err(ExecControlError::Identity)
        );
        // Model transport completion setup; existing assertions unchanged.
        // SAFETY: fixture models transfer, not native execution.
        unsafe {
            registry
                .begin_provision(&proof.pidfd, ticket)
                .unwrap()
                .complete()
        }
        .unwrap();
        registry
            .issue(context.owner, ticket, context.prepared_mm, digest)
            .unwrap();
        let wrong_peer: OwnedFd = std::fs::File::open("/dev/null").unwrap().into();
        assert_eq!(
            registry.redeem(&wrong_peer, ticket, context.config_digest, digest),
            Err(ExecControlError::Identity)
        );
        assert_eq!(
            registry.redeem(
                &proof.pidfd,
                ExecTicket([0; 32]),
                context.config_digest,
                digest
            ),
            Err(ExecControlError::Identity)
        );
        assert_eq!(
            registry.redeem(&proof.pidfd, ticket, [9; 32], digest),
            Err(ExecControlError::Digest)
        );
        assert_eq!(
            registry.redeem(&proof.pidfd, ticket, context.config_digest, [9; 32]),
            Err(ExecControlError::Digest)
        );
        assert_eq!(registry.stage(ticket), Some(ExecStage::Issued));
        assert!(
            !registry
                .redeem(&proof.pidfd, ticket, context.config_digest, digest)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn native_registry_terminal_failure_refuses_every_future_edge() {
        for edge in 0..4 {
            let (mut registry, proof, context, request) = fixture();
            let ticket = registry.prepare(context, request.clone()).unwrap();
            let digest = if edge > 0 {
                seal(&mut registry, context, ticket)
            } else {
                [0; 32]
            };
            if edge > 1 {
                // Model transport completion setup; existing assertions unchanged.
                // SAFETY: fixture models transfer, not native execution.
                unsafe {
                    registry
                        .begin_provision(&proof.pidfd, ticket)
                        .unwrap()
                        .complete()
                }
                .unwrap();
                registry
                    .issue(context.owner, ticket, context.prepared_mm, digest)
                    .unwrap();
            }
            if edge > 2 {
                registry
                    .redeem(&proof.pidfd, ticket, context.config_digest, digest)
                    .unwrap();
            }
            registry.failed(ticket).unwrap();
            assert_eq!(registry.stage(ticket), Some(ExecStage::Failed));
            assert_eq!(
                registry.prepare(context, request.clone()),
                Err(ExecControlError::Dead)
            );
            let next_context = NativeExecContext {
                old_mm: context.prepared_mm,
                prepared_mm: context.prepared_mm.for_exec(context.owner),
                ..context
            };
            assert_eq!(
                registry.prepare(
                    next_context,
                    PrepareNativeExec {
                        prepared_mm: next_context.prepared_mm,
                        ..request
                    }
                ),
                Err(ExecControlError::Dead)
            );
            assert_eq!(
                registry.seal(
                    context.owner,
                    ticket,
                    context.prepared_mm,
                    Sha256::digest(b"actual-runtime-model").into(),
                    b"actual-runtime-model"
                ),
                Err(ExecControlError::Dead)
            );
            assert_eq!(
                registry.issue(context.owner, ticket, context.prepared_mm, digest),
                Err(ExecControlError::Dead)
            );
            assert_eq!(
                registry.redeem(&proof.pidfd, ticket, context.config_digest, digest),
                Err(ExecControlError::Dead)
            );
            assert_eq!(
                registry.complete(context.owner, ticket, context.prepared_mm),
                Err(ExecControlError::Dead)
            );
            assert_eq!(
                registry.cancel(context.owner, ticket, context.prepared_mm),
                Err(ExecControlError::Dead)
            );
            assert_eq!(registry.stage(ticket), Some(ExecStage::Failed));
        }
    }

    #[test]
    fn native_registry_mask_must_match_the_independently_captured_cut() {
        let (mut registry, _, context, mut request) = fixture();
        request.resume_mask ^= 1;
        assert_eq!(
            registry.prepare(context, request),
            Err(ExecControlError::Qualification)
        );
        assert!(registry.transitions.is_empty());
        assert_eq!(registry.next_ticket, 1);
    }

    #[test]
    fn native_registry_death_at_each_live_edge_preserves_tombstones() {
        for edge in 0..4 {
            let (mut registry, proof, context, request) = fixture();
            let ticket = registry.prepare(context, request.clone()).unwrap();
            let digest = if edge > 0 {
                seal(&mut registry, context, ticket)
            } else {
                [0; 32]
            };
            if edge > 1 {
                // Model transport completion setup; existing assertions unchanged.
                // SAFETY: fixture models transfer, not native execution.
                unsafe {
                    registry
                        .begin_provision(&proof.pidfd, ticket)
                        .unwrap()
                        .complete()
                }
                .unwrap();
                registry
                    .issue(context.owner, ticket, context.prepared_mm, digest)
                    .unwrap();
            }
            if edge > 2 {
                registry
                    .redeem(&proof.pidfd, ticket, context.config_digest, digest)
                    .unwrap();
            }
            proof.live.store(false, Ordering::SeqCst);
            assert_eq!(
                registry.complete(context.owner, ticket, context.prepared_mm),
                Err(ExecControlError::Dead)
            );
            assert_eq!(registry.stage(ticket), Some(ExecStage::Dead));
            proof.live.store(true, Ordering::SeqCst);
            assert_eq!(
                registry.prepare(context, request),
                Err(ExecControlError::Dead)
            );
            assert_eq!(
                registry.redeem(&proof.pidfd, ticket, context.config_digest, digest),
                Err(ExecControlError::Dead)
            );
        }
    }

    #[test]
    fn native_registry_bounds_and_qualification_leave_no_partial_preparation() {
        for invalid in 0..7 {
            let (mut registry, _, context, mut request) = fixture();
            match invalid {
                0 => request.prepared_mm = context.old_mm,
                1 => request.config_digest = [9; 32],
                2 => request.snapshot.clear(),
                3 => request.snapshot.resize(MAX_STATE_BYTES, 0),
                4 => request.snapshot_digest = [9; 32],
                5 => request.target_digest = [9; 32],
                6 => request.bootstrap_digest = [9; 32],
                _ => unreachable!(),
            }
            assert!(registry.prepare(context, request).is_err());
            assert!(registry.transitions.is_empty());
            assert_eq!(registry.next_ticket, 1);
        }
        let (mut registry, _, context, request) = fixture();
        let ticket = registry.prepare(context, request).unwrap();
        for runtime in [b"".as_slice(), b"unsupported-runtime".as_slice()] {
            assert!(
                registry
                    .seal(
                        context.owner,
                        ticket,
                        context.prepared_mm,
                        Sha256::digest(runtime).into(),
                        runtime
                    )
                    .is_err()
            );
            assert_eq!(registry.stage(ticket), Some(ExecStage::Prepared));
            assert!(registry.transitions[&ticket].state_digest.is_none());
        }
        assert_eq!(
            registry.seal(
                context.owner,
                ticket,
                context.prepared_mm,
                [9; 32],
                b"actual-runtime-model"
            ),
            Err(ExecControlError::Digest)
        );
        seal(&mut registry, context, ticket);
    }

    #[test]
    fn native_registry_state_parser_rejects_all_truncations_and_wrong_lengths() {
        let (mut registry, proof, context, request) = fixture();
        let ticket = registry.prepare(context, request).unwrap();
        let digest = seal(&mut registry, context, ticket);
        // Model transport completion setup; existing assertions unchanged.
        // SAFETY: fixture models transfer, not native execution.
        unsafe {
            registry
                .begin_provision(&proof.pidfd, ticket)
                .unwrap()
                .complete()
        }
        .unwrap();
        registry
            .issue(context.owner, ticket, context.prepared_mm, digest)
            .unwrap();
        let bytes = registry
            .redeem(&proof.pidfd, ticket, context.config_digest, digest)
            .unwrap();
        for n in 0..bytes.len() {
            assert!(split_state(&bytes[..n]).is_err(), "length {n}");
        }
        for field in [0, 8, 10, 14] {
            let mut bad = bytes.clone();
            bad[field] ^= 1;
            assert!(split_state(&bad).is_err());
        }
        let mut extra = bytes.clone();
        extra.push(0);
        assert!(split_state(&extra).is_err());
        extra.resize(MAX_STATE_BYTES + 1, 0);
        assert!(split_state(&extra).is_err());
        assert!(split_state(&bytes).is_ok());
    }

    // These additional tests reuse the explicitly modelled qualification proof
    // above. They test provision state/actual pidfd identity, not native exec.
    #[test]
    fn native_provision_preseal_wrong_peer_and_never_provisioned_issue_refuse() {
        let (mut registry, proof, context, request) = fixture();
        let ticket = registry.prepare(context, request).unwrap();
        assert!(matches!(
            registry.begin_provision(&proof.pidfd, ticket),
            Err(ExecControlError::Transition)
        ));
        assert_eq!(
            registry.transitions[&ticket].provision,
            ProvisionState::Fresh
        );
        let digest = seal(&mut registry, context, ticket);
        let wrong: OwnedFd = std::fs::File::open("/dev/null").unwrap().into();
        assert!(matches!(
            registry.begin_provision(&wrong, ticket),
            Err(ExecControlError::Identity)
        ));
        assert_eq!(
            registry.transitions[&ticket].provision,
            ProvisionState::Fresh
        );
        assert_eq!(
            registry.issue(context.owner, ticket, context.prepared_mm, digest),
            Err(ExecControlError::Transition)
        );
        assert_eq!(registry.stage(ticket), Some(ExecStage::Prepared));
    }

    #[test]
    fn native_provision_pending_failure_is_terminal_and_cannot_issue() {
        let (mut registry, proof, context, request) = fixture();
        let ticket = registry.prepare(context, request.clone()).unwrap();
        let digest = seal(&mut registry, context, ticket);
        {
            let lease = registry.begin_provision(&proof.pidfd, ticket).unwrap();
            assert_eq!(lease.binding().context, context);
            assert_eq!(lease.state_digest(), digest);
            assert_eq!(
                lease.registry.transitions[&ticket].provision,
                ProvisionState::Consumed
            );
            assert_eq!(
                lease
                    .registry
                    .issue(context.owner, ticket, context.prepared_mm, digest),
                Err(ExecControlError::Transition)
            );
            // Model send error: Drop performs terminal failure, with no retry.
        }
        assert_eq!(registry.stage(ticket), Some(ExecStage::Failed));
        assert!(matches!(
            registry.begin_provision(&proof.pidfd, ticket),
            Err(ExecControlError::Dead)
        ));
        assert_eq!(
            registry.issue(context.owner, ticket, context.prepared_mm, digest),
            Err(ExecControlError::Dead)
        );
        assert_eq!(
            registry.prepare(context, request),
            Err(ExecControlError::Dead)
        );
    }

    #[test]
    fn native_provision_complete_once_then_issue_and_cancel_tombstone() {
        let (mut registry, proof, context, request) = fixture();
        let ticket = registry.prepare(context, request.clone()).unwrap();
        let digest = seal(&mut registry, context, ticket);
        // SAFETY: explicit transport model, not native execution authority.
        unsafe {
            registry
                .begin_provision(&proof.pidfd, ticket)
                .unwrap()
                .complete()
        }
        .unwrap();
        assert_eq!(
            registry.transitions[&ticket].provision,
            ProvisionState::Delivered
        );
        assert!(matches!(
            registry.begin_provision(&proof.pidfd, ticket),
            Err(ExecControlError::Transition)
        ));
        registry
            .issue(context.owner, ticket, context.prepared_mm, digest)
            .unwrap();
        registry
            .cancel(context.owner, ticket, context.prepared_mm)
            .unwrap();
        assert!(matches!(
            registry.begin_provision(&proof.pidfd, ticket),
            Err(ExecControlError::Transition)
        ));
        assert_eq!(registry.stage(ticket), Some(ExecStage::Cancelled));
        let next = registry.prepare(context, request).unwrap();
        assert_ne!(next, ticket);
        registry.provision_transport_failed();
        assert_eq!(registry.stage(ticket), Some(ExecStage::Cancelled));
        assert_eq!(registry.stage(next), Some(ExecStage::Failed));
    }

    #[test]
    fn native_provision_death_before_and_during_completion_preserves_dead() {
        for consumed in [false, true] {
            let (mut registry, proof, context, request) = fixture();
            let ticket = registry.prepare(context, request).unwrap();
            let digest = seal(&mut registry, context, ticket);
            if consumed {
                let lease = registry.begin_provision(&proof.pidfd, ticket).unwrap();
                proof.live.store(false, Ordering::SeqCst);
                // SAFETY: modelled transfer; actual live proof must still refuse.
                assert_eq!(unsafe { lease.complete() }, Err(ExecControlError::Dead));
            } else {
                proof.live.store(false, Ordering::SeqCst);
                assert!(matches!(
                    registry.begin_provision(&proof.pidfd, ticket),
                    Err(ExecControlError::Dead)
                ));
            }
            assert_eq!(registry.stage(ticket), Some(ExecStage::Dead));
            assert_eq!(
                registry.issue(context.owner, ticket, context.prepared_mm, digest),
                Err(ExecControlError::Dead)
            );
        }
    }
    #[derive(Debug)]
    struct RefusingCompletion(Arc<Proof>);
    // SAFETY: explicit registry model only; completion is deliberately left at
    // its default refusal. No runtime installer or native launch uses this mock.
    unsafe impl NativeExecProof for RefusingCompletion {
        fn owner(&self) -> DetTid {
            self.0.owner()
        }
        fn physical_ids(&self) -> Option<(i32, i32)> {
            self.0.physical_ids()
        }
        fn is_live(&self) -> bool {
            self.0.is_live()
        }
        fn matches_peer(&self, peer: &OwnedFd) -> bool {
            self.0.matches_peer(peer)
        }
        fn qualify_target(&self, request: &PrepareNativeExec) -> bool {
            self.0.qualify_target(request)
        }
        fn qualify_runtime(&self, bytes: &[u8]) -> bool {
            self.0.qualify_runtime(bytes)
        }
    }
    #[test]
    fn model_default_completion_refusal_is_terminal_without_retry() {
        let (_, proof, context, request) = fixture();
        let mut registry =
            NativeExecRegistry::new(Arc::new(RefusingCompletion(proof.clone())), [9; 32]).unwrap();
        let ticket = registry.prepare(context, request.clone()).unwrap();
        let digest = seal(&mut registry, context, ticket);
        // SAFETY: explicit model transfer only.
        unsafe {
            registry
                .begin_provision(&proof.pidfd, ticket)
                .unwrap()
                .complete()
        }
        .unwrap();
        registry
            .issue(context.owner, ticket, context.prepared_mm, digest)
            .unwrap();
        registry
            .redeem(&proof.pidfd, ticket, context.config_digest, digest)
            .unwrap();
        assert_eq!(
            registry.complete(context.owner, ticket, context.prepared_mm),
            Err(ExecControlError::Qualification)
        );
        assert_eq!(registry.stage(ticket), Some(ExecStage::Failed));
        assert_eq!(
            registry.complete(context.owner, ticket, context.prepared_mm),
            Err(ExecControlError::Dead)
        );
        assert_eq!(
            registry.prepare(context, request),
            Err(ExecControlError::Dead)
        );
        assert_eq!(
            registry.cancel(context.owner, ticket, context.prepared_mm),
            Err(ExecControlError::Dead)
        );
    }

    #[test]
    fn model_connection_close_is_incarnation_specific_and_survives_delayed_eof() {
        let (mut registry, proof, context, request) = fixture();
        assert_eq!(
            registry.permits_connection_close(&proof.pidfd, None),
            Ok(false)
        );
        let wrong: OwnedFd = std::fs::File::open("/dev/null").unwrap().into();
        assert_eq!(
            registry.permits_connection_close(&wrong, None),
            Err(ExecControlError::Identity)
        );
        let first = registry.prepare(context, request.clone()).unwrap();
        assert_eq!(
            registry.permits_connection_close(&proof.pidfd, None),
            Ok(false)
        );
        let digest = seal(&mut registry, context, first);
        // SAFETY: existing explicit registry model transfer, not native launch.
        unsafe {
            registry
                .begin_provision(&proof.pidfd, first)
                .unwrap()
                .complete()
        }
        .unwrap();
        registry
            .issue(context.owner, first, context.prepared_mm, digest)
            .unwrap();
        assert_eq!(
            registry.permits_connection_close(&proof.pidfd, None),
            Ok(true)
        );
        registry
            .redeem(&proof.pidfd, first, context.config_digest, digest)
            .unwrap();
        registry
            .complete(context.owner, first, context.prepared_mm)
            .unwrap();
        assert_eq!(
            registry.permits_connection_close(&proof.pidfd, None),
            Ok(true)
        );
        assert_eq!(
            registry.permits_connection_close(&proof.pidfd, Some(first)),
            Ok(false)
        );
        let next_context = NativeExecContext {
            old_mm: context.prepared_mm,
            prepared_mm: context.prepared_mm.for_exec(context.owner),
            ..context
        };
        let next_request = PrepareNativeExec {
            cut: NativeExecNeutralBinding {
                old_mm: next_context.old_mm,
                old_files: request.cut.old_files.for_exec(context.owner),
                prepared_mm: next_context.prepared_mm,
                ..request.cut
            },
            prepared_mm: next_context.prepared_mm,
            ..request
        };
        let second = registry.prepare(next_context, next_request).unwrap();
        assert_eq!(
            registry.permits_connection_close(&proof.pidfd, Some(first)),
            Ok(false)
        );
        let digest = seal(&mut registry, next_context, second);
        // SAFETY: existing explicit registry model transfer, not native launch.
        unsafe {
            registry
                .begin_provision(&proof.pidfd, second)
                .unwrap()
                .complete()
        }
        .unwrap();
        registry
            .issue(context.owner, second, next_context.prepared_mm, digest)
            .unwrap();
        registry
            .redeem(&proof.pidfd, second, context.config_digest, digest)
            .unwrap();
        registry
            .complete(context.owner, second, next_context.prepared_mm)
            .unwrap();
        assert_eq!(
            registry.permits_connection_close(&proof.pidfd, None),
            Ok(true)
        );
        assert_eq!(
            registry.permits_connection_close(&proof.pidfd, Some(first)),
            Ok(true)
        );
        assert_eq!(
            registry.permits_connection_close(&proof.pidfd, Some(second)),
            Ok(false)
        );
    }

    #[test]
    fn model_cancelled_issue_cannot_admit_connection_close() {
        let (mut registry, proof, context, request) = fixture();
        let ticket = registry.prepare(context, request).unwrap();
        let digest = seal(&mut registry, context, ticket);
        // SAFETY: existing explicit registry model transfer, not native launch.
        unsafe {
            registry
                .begin_provision(&proof.pidfd, ticket)
                .unwrap()
                .complete()
        }
        .unwrap();
        registry
            .issue(context.owner, ticket, context.prepared_mm, digest)
            .unwrap();
        registry
            .cancel(context.owner, ticket, context.prepared_mm)
            .unwrap();
        assert_eq!(
            registry.permits_connection_close(&proof.pidfd, None),
            Ok(false)
        );
        assert_eq!(
            registry.permits_connection_close(&proof.pidfd, Some(ticket)),
            Err(ExecControlError::Transition)
        );
    }
}
