//! Concrete first-domain Detcore native exec transaction. No ordinary entry
//! installs this driver. The native initializer retains actual launch authority.

use std::sync::Arc;
use std::sync::Mutex;

use detcore::Detcore;
use detcore::GlobalState;
use detcore::ThreadState;
use detcore::native_exec::NativeExecCutBinding;
use detcore::native_exec::NativeExecPreparationReceipt;
use detcore::native_exec_control::GlobalRequest as Q;
use detcore::native_exec_control::GlobalResponse as R;
use detcore_model::liteinst_native_exec_input as input;
use detcore_model::liteinst_native_exec_state as state_frame;
use reverie::GlobalRPC;
use reverie::Pid;
use reverie::UnsupportedOperation;
use reverie::UnsupportedRefusal;
use reverie_inguest::guest::exec_private::IssuedNativeExecLeaf;
use reverie_inguest::guest::native_exec::ExecCut;
use reverie_inguest::guest::native_exec::NativeExecDriver;
use reverie_inguest::guest::native_exec::NativeExecFailure;
use reverie_inguest::guest::native_exec::NativeExecSession;
use reverie_inguest::guest::native_exec::PreparedNativeExec;
use reverie_inguest::guest::native_exec::ReturnedNativeExec;
use reverie_inguest::guest::native_exec::TerminalExecFailure;
use reverie_inguest::guest::native_exec_arguments::ArgumentImage;
use reverie_inguest::guest::native_exec_arguments::ArgumentQuiescence;
use reverie_inguest::guest::native_exec_arguments::ArgumentRange;
use reverie_inguest::guest::native_exec_arguments::OriginalExecArguments;
use reverie_inguest::guest::native_exec_arguments::ReadCapture;
use reverie_inguest::guest::native_exec_fds::BootstrapFdLease;
use reverie_inguest::guest::native_exec_fds::NativeExecSocketBinding;
use reverie_inguest::guest::native_exec_fds::NativeExecSocketBindings;
use reverie_inguest::guest::native_exec_fds::NativeSocket;
use reverie_inguest::guest::native_exec_fds::socket_fd;
use reverie_inguest::guest::native_exec_mappings::ExecutableMappings;
use reverie_inguest::guest::rpc::CoordinatorRpc;
use reverie_liteinst::native_exec_state::NativeExecStatsV1;
use reverie_rpc_transport::RequestEnvelope;
use sha2::Digest;
use sha2::Sha256;

use super::provider;
use super::runtime_packet;
use crate::exec_initial::DispatchSequenceCut;
use crate::exec_initial::{self};

type Rpc = CoordinatorRpc<GlobalState>;
const MAX_SNAPSHOT: usize = detcore::native_exec_control::MAX_STATE_BYTES
    - state_frame::HEADER_BYTES
    - runtime_packet::PACKET_BYTES;
const MAX_CONTROL: usize = 16384;
/// One admitted path and its independently retained sealed image roles.
/// Paths are caller-declared catalog entries, never inferred from argv or a test.
pub(crate) struct Target {
    pub(crate) path: Box<[u8]>,
    pub(crate) digest: [u8; 32],
    pub(crate) bootstrap: [u8; 32],
    // Actual stock loader (198 and 200), runner 199, and native config 202.
    pub(crate) objects: [provider::Object; 3],
}

/// Captured before the first guest instruction from the genuinely admitted
/// sealed namespace. No value from a subsequent SCM_RIGHTS reply populates it.
pub(crate) struct Authority {
    pub(crate) config: [u8; 32],
    pub(crate) source: [u8; 32],
    pub(crate) schema: [u8; 16],
    pub(crate) targets: Box<[Target]>,
    /// Complete actual protected storage, retained by this installed owner.
    pub(crate) protected: Box<[ArgumentRange]>,
    pub(crate) image_epoch: u64,
    pub(crate) argument_frontend: ReadCapture,
    /// Independently admitted objects survive descriptor relocation. Current
    /// descriptor numbers belong to each quiescent Session, not initialization.
    pub(crate) detlog_object: (u64, u64),
    pub(crate) observer_object: (u64, u64),
    pub(crate) forward_policy: String,
    pub(crate) provider_fd: i32,
    /// Actual sealed-object executable extents and qualified runtime ranges.
    /// Installed before the stock loader may perform its first mapping call.
    pub(crate) executable_mappings: ExecutableMappings,
}

pub(crate) struct Driver {
    authority: Arc<Authority>,
}
impl Driver {
    /// # Safety
    /// Only the covered native entry may construct this. Its real pidfd-backed
    /// admission must qualify immutable namespace/file roles and full compiled
    /// source identity, exact inherited filter, original guest target and owned
    /// stack/FS. All three objects and distinct protected sockets were queried
    /// from the actual admitted root before any guest work. The original target
    /// path cannot be rebound, replaced or reached through a mutable ancestor.
    /// Config must be the same independently authenticated first-domain Config.
    /// The catalog must come from the actual admitted sealed common root and
    /// cover every executable/interpreter/dependency under its original path.
    /// `protected` must cover all actual Tool/control/bootstrap/continuation
    /// storage. The owner must exclude all external, asynchronous and guest
    /// writers from original argument capture until kernel handoff/cancellation;
    /// the argument lease documents this same real closure, not a new promise
    /// inferred from observing private anonymous mappings. `argument_frontend`
    /// must describe genuine saved rights of that installed frontend.
    pub(crate) unsafe fn new(authority: Authority) -> Self {
        Self {
            authority: Arc::new(authority),
        }
    }
}

fn refuse(message: &'static str) -> UnsupportedRefusal {
    UnsupportedRefusal::new(
        UnsupportedOperation::NativeExec,
        reverie::Errno::EOPNOTSUPP,
        message,
    )
}
fn terminal_failure(message: &'static str) -> TerminalExecFailure {
    // SAFETY: the private native driver's transaction retains its installed
    // gate, Tool stack and protected output owner while Guest is excluded.
    // Report before an armed local transaction can drop and terminate.
    unsafe { reverie_inguest::guest::exec_private::report_native_terminal_reason(message) };
    TerminalExecFailure {
        diagnostic: message,
    }
}
fn terminal(message: &'static str) -> NativeExecFailure {
    NativeExecFailure::Terminal(terminal_failure(message))
}
fn refused(message: &'static str) -> NativeExecFailure {
    NativeExecFailure::Refused(refuse(message))
}

fn original_frontend(cut: &ExecCut) -> Result<(), UnsupportedRefusal> {
    if !cut.is_fallback() || cut.resume_mask_receipt().is_none() {
        return Err(refuse(
            "native exec requires the genuine fallback resume frame",
        ));
    }
    Ok(())
}

struct Session<'owner> {
    authority: Arc<Authority>,
    binding: NativeExecCutBinding,
    receipt: NativeExecPreparationReceipt,
    cut: ExecCut,
    selected: usize,
    sockets: NativeExecSocketBindings,
    detlog_identity: String,
    observer_identity: String,
    // This mutex owns only preallocated argument inspection storage. It never
    // reacquires ToolHost or real ThreadState locks and cannot await work.
    arguments: Mutex<OriginalExecArguments<'owner>>,
}

impl Session<'_> {
    fn target(&self) -> &Target {
        &self.authority.targets[self.selected]
    }

    fn check_arguments(&self, cut: &ExecCut) -> Result<(), NativeExecFailure> {
        original_frontend(cut).map_err(NativeExecFailure::Refused)?;
        let mut arguments = self
            .arguments
            .try_lock()
            .map_err(|_| terminal("native exec argument receipt is busy or poisoned"))?;
        arguments
            .recheck(cut.original())
            .map_err(|_| terminal("native exec original argument mappings or bytes changed"))?;
        if arguments.path() != self.target().path.as_ref()
            || arguments.image().mm_id
                != (
                    self.binding.old_mm.creator().as_raw(),
                    self.binding.old_mm.generation(),
                )
            || arguments.image().image_epoch != self.authority.image_epoch
        {
            return Err(terminal(
                "native exec admitted target or original image changed",
            ));
        }
        Ok(())
    }
}

// SAFETY: installed only by the unsafe covered native initializer above. The
// concrete capture inspects genuine live data without starting any transaction.
unsafe impl NativeExecDriver<Detcore> for Driver {
    fn bind_initial_state(
        &self,
        _tool: &Detcore,
        state: &mut ThreadState<()>,
        physical_tid: i32,
    ) -> std::io::Result<()> {
        let tid = unsafe { reverie_inguest::trap::raw_syscall6(libc::SYS_gettid, [0; 6]) };
        let pid = unsafe { reverie_inguest::trap::raw_syscall6(libc::SYS_getpid, [0; 6]) };
        if tid != i64::from(physical_tid) {
            return Err(std::io::Error::other(
                "native initial lifecycle TID differs",
            ));
        }
        // SAFETY: the caller holds the genuine same-task InitialImage permit;
        // these two IDs are read through its actual current raw gates above.
        unsafe {
            detcore::native_exec::bind_native_initial_physical_identity(
                state, pid as i32, tid as i32,
            )
        }
        .map_err(std::io::Error::other)
    }

    fn bind_post_exec_state(
        &self,
        state: &mut ThreadState<()>,
        mode: reverie::BackendSignalControlMode,
        actual: Option<reverie::SignalTaskIdentity>,
    ) -> std::io::Result<()> {
        detcore::native_exec::bind_native_fresh_signal_identity(state, mode, actual)
            .map_err(std::io::Error::other)
    }

    fn check_mapping(&self, number: i64, args: [u64; 6]) -> Result<(), UnsupportedRefusal> {
        self.authority
            .executable_mappings
            .check(number, args)
            .map_err(refuse)
    }

    fn capture_old<'owner>(
        &'owner self,
        tool: &Detcore,
        state: &ThreadState<()>,
        cut: &ExecCut,
        rpc: &Rpc,
    ) -> Result<Box<dyn NativeExecSession<Detcore> + 'owner>, UnsupportedRefusal> {
        detcore::native_exec::validate_native_exec_guest_configuration(rpc.config())
            .map_err(|_| refuse("native exec guest Config contains unsupported live fields"))?;
        original_frontend(cut)?;
        let image = ArgumentImage {
            physical_pid: cut.native().native_tid,
            physical_tid: cut.native().native_tid,
            mm_id: (state.mm_id.creator().as_raw(), state.mm_id.generation()),
            image_epoch: self.authority.image_epoch,
        };
        // SAFETY: Driver::new requires the genuine retained installation and
        // complete protected ranges/writer closure. Capture preserves that
        // owner's lifetime; no decoded packet or observation creates it.
        let lease = unsafe {
            ArgumentQuiescence::new(
                image,
                &self.authority.protected,
                self.authority.argument_frontend,
                self.authority.as_ref(),
            )
        }
        .map_err(|_| refuse("native exec argument owner or read rights differ"))?;
        let arguments = OriginalExecArguments::capture(cut.original(), lease)
            .map_err(|_| refuse("native exec original argument memory is unsupported"))?;
        let selected = self
            .authority
            .targets
            .iter()
            .position(|target| target.path.as_ref() == arguments.path())
            .ok_or_else(|| {
                refuse("native exec original path is outside the declared immutable catalog")
            })?;
        // This first loader domain preserves all ordinary environment bytes,
        // but excludes loader redirection/tunables that could escape the held
        // dependency graph. No environment variable is removed or rewritten.
        if arguments.envp().any(|entry| {
            let name = entry.split(|byte| *byte == b'=').next().unwrap_or(entry);
            name.starts_with(b"LD_") || name == b"GLIBC_TUNABLES"
        }) {
            return Err(refuse(
                "native exec loader-sensitive environment is outside the admitted domain",
            ));
        }
        if state.dettid.as_raw() != cut.native().native_tid {
            return Err(refuse(
                "native exec first domain excludes translated thread identities",
            ));
        }
        if detcore::native_exec::native_exec_config_digest(rpc.config()).ok()
            != Some(self.authority.config)
            || socket_fd(NativeSocket::Provider) != Some(self.authority.provider_fd)
        {
            return Err(refuse(
                "native exec admitted Config or protected provider differs",
            ));
        }
        let old_files = state
            .file_metadata
            .try_lock()
            .map_err(|_| refuse("native exec old descriptor table is busy"))?
            .native_exec_files_id();
        let binding = NativeExecCutBinding {
            dettid: state.dettid,
            detpid: state
                .detpid
                .ok_or_else(|| refuse("native exec process identity is absent"))?,
            physical_tid: cut.native().native_tid,
            old_mm: state.mm_id,
            old_files,
            config_digest: self.authority.config,
        };
        let receipt = detcore::native_exec::validate_old_native_exec_cut(tool, state, &binding)
            .map_err(|_| refuse("native exec old Tool or descriptor state is unsupported"))?;
        // Capture the current protected owners at this cut. A prior image or
        // guest dup may have moved the same independently admitted objects.
        let sockets = NativeExecSocketBindings::capture(
            self.authority.detlog_object,
            self.authority.observer_object,
        )
        .map_err(|_| refuse("native exec current protected socket bindings differ"))?;
        let detlog_identity = socket_identity(sockets.detlog());
        let observer_identity = socket_identity(sockets.observer());
        Ok(Box::new(Session {
            authority: Arc::clone(&self.authority),
            binding,
            receipt,
            cut: *cut,
            selected,
            sockets,
            detlog_identity,
            observer_identity,
            arguments: Mutex::new(arguments),
        }))
    }
}

// A successful Prepare response arms this owner before any subsequent fallible
// work. Loss of its exact cancellation response can never resume old Guest.
struct TicketGuard {
    armed: bool,
}
impl Drop for TicketGuard {
    fn drop(&mut self) {
        if self.armed {
            exec_initial::fail();
        }
    }
}

struct Transaction<'a, 'owner> {
    guard: TicketGuard,
    session: &'a Session<'owner>,
    lease: BootstrapFdLease,
    snapshot: Vec<u8>,
    stats: Option<NativeExecStatsV1>,
    sequence: Option<DispatchSequenceCut>,
    runtime: [u8; runtime_packet::PACKET_BYTES],
    seal: Vec<u8>,
    seal_len: usize,
    seal_expected: [u8; 128],
    seal_expected_len: usize,
    issue: [u8; 1024],
    issue_len: usize,
    issue_expected: [u8; 128],
    issue_expected_len: usize,
    cancel: [u8; 1024],
    cancel_len: usize,
    cancel_expected: [u8; 128],
    cancel_expected_len: usize,
    response: [u8; 4096],
    input: [u8; input::MAX_PACKET_LEN],
    input_len: usize,
    input_read: [u8; input::MAX_PACKET_LEN],
    scratch: [u8; 16384],
    provider_request: [u8; 48],
    provider_expected: [u8; 80],
    issued: bool,
}

fn encode<T: serde::Serialize>(value: &T, into: &mut [u8]) -> Result<usize, NativeExecFailure> {
    bincode::serde::encode_into_slice(value, into, bincode::config::legacy())
        .map_err(|_| terminal("native exec preallocated control encoding failed"))
}

fn envelope(
    state: &ThreadState<()>,
    request: Q,
) -> RequestEnvelope<<GlobalState as reverie::GlobalTool>::Request> {
    RequestEnvelope {
        from: Pid::from_raw(state.dettid.as_raw()),
        request: (
            state.thread_logical_time.clone(),
            state.mm_id,
            Q::WithForwardedCount(0, Box::new(request)),
        ),
    }
}

fn neutral_binding(binding: NativeExecCutBinding) -> state_frame::CutBinding {
    state_frame::CutBinding {
        dettid: binding.dettid,
        detpid: binding.detpid,
        physical_tid: binding.physical_tid,
        old_mm: binding.old_mm,
        old_files: binding.old_files,
        prepared_mm: binding.prepared_mm(),
        config_digest: binding.config_digest,
    }
}

fn socket_identity(socket: NativeExecSocketBinding) -> String {
    let object = socket.identity();
    format!(
        "{}:{:016x}:{:016x}",
        socket.fd(),
        object.device,
        object.inode
    )
}

fn socket_binding(
    socket: NativeExecSocketBinding,
) -> detcore::native_exec_control::NativeExecSocketBinding {
    let object = socket.identity();
    detcore::native_exec_control::NativeExecSocketBinding {
        fd: socket.fd(),
        device: object.device,
        inode: object.inode,
    }
}

fn composite_digest(
    binding: state_frame::CutBinding,
    snapshot: &[u8],
    runtime: &[u8],
) -> Result<[u8; 32], NativeExecFailure> {
    let mut header = [0; state_frame::HEADER_BYTES];
    state_frame::encode_header(binding, snapshot.len(), runtime.len(), &mut header)
        .map_err(|_| terminal("native exec neutral cut or composite bounds differ"))?;
    let mut sha = Sha256::new();
    sha.update(header);
    sha.update(snapshot);
    sha.update(runtime);
    Ok(sha.finalize().into())
}

// Exact equality to an independently serialized typed successful response is
// a complete no-allocation decode: it admits one variant, no clock and no tail.
fn exchange(
    rpc: &Rpc,
    request: &[u8],
    expected: &[u8],
    buffer: &mut [u8],
) -> Result<(), NativeExecFailure> {
    let reply = unsafe { rpc.exchange_native_frame(request, buffer) }
        .map_err(|_| terminal("native exec raw coordinator exchange failed"))?;
    if &buffer[..reply.length()] != expected {
        return Err(terminal(
            "native exec coordinator response differs from exact typed success",
        ));
    }
    unsafe {
        reply.accept();
    }
    Ok(())
}

// SAFETY: the session retains the real old table and actual original event.
// Every later phase rechecks live prepared state; no callback lock is reacquired.
unsafe impl NativeExecSession<Detcore> for Session<'_> {
    fn prepare<'a>(
        &'a mut self,
        tool: &Detcore,
        state: &ThreadState<()>,
        cut: &ExecCut,
        rpc: &Rpc,
    ) -> Result<Box<dyn PreparedNativeExec<Detcore> + 'a>, NativeExecFailure> {
        if cut != &self.cut {
            return Err(refused("native exec original cut changed"));
        }
        self.check_arguments(cut)?;
        let snapshot_ref = detcore::native_exec::capture_prepared_native_exec(
            tool,
            state,
            &self.binding,
            &self.receipt,
        )
        .map_err(|_| refused("native exec prepared state differs from the actual old receipt"))?;
        let snapshot =
            reverie_rpc_transport::codec::encode_bounded::<_, MAX_SNAPSHOT>(&snapshot_ref)
                .map_err(|_| refused("native exec Tool snapshot exceeds bounded state"))?;
        self.sockets
            .revalidate_original()
            .map_err(|_| refused("native exec protected sockets changed before reservation"))?;
        let mut lease = unsafe { BootstrapFdLease::reserve() }
            .map_err(|_| refused("native exec bootstrap descriptors cannot be reserved"))?;
        // This existing lease owns exact restoration on cancellation or a real
        // returned exec error. A failed mutation/readback remains armed/fatal.
        lease
            .prepare_socket_inheritance(self.sockets)
            .map_err(|_| terminal("native exec protected socket inheritance failed"))?;
        let mut transaction = Box::new(Transaction {
            guard: TicketGuard { armed: false },
            session: self,
            lease,
            snapshot,
            stats: None,
            sequence: None,
            runtime: [0; runtime_packet::PACKET_BYTES],
            seal: vec![0; MAX_CONTROL],
            seal_len: 0,
            seal_expected: [0; 128],
            seal_expected_len: 0,
            issue: [0; 1024],
            issue_len: 0,
            issue_expected: [0; 128],
            issue_expected_len: 0,
            cancel: [0; 1024],
            cancel_len: 0,
            cancel_expected: [0; 128],
            cancel_expected_len: 0,
            response: [0; 4096],
            input: [0; input::MAX_PACKET_LEN],
            input_len: 0,
            input_read: [0; input::MAX_PACKET_LEN],
            scratch: [0; 16384],
            provider_request: [0; 48],
            provider_expected: [0; 80],
            issued: false,
        });
        let request = Q::PrepareNativeExec(detcore::native_exec_control::PrepareNativeExec {
            sockets: detcore::native_exec_control::NativeExecSocketBindings {
                detlog: socket_binding(self.sockets.detlog()),
                observer: socket_binding(self.sockets.observer()),
            },
            cut: neutral_binding(self.binding),
            prepared_mm: state.mm_id,
            config_digest: self.authority.config,
            target_digest: self.target().digest,
            bootstrap_digest: self.target().bootstrap,
            resume_mask: cut
                .resume_mask_receipt()
                .ok_or_else(|| terminal("native exec resume receipt disappeared"))?
                .signal_mask(),
            snapshot_digest: Sha256::digest(&transaction.snapshot).into(),
            snapshot: transaction.snapshot.clone(),
        });
        let count = detcore::detlog::take_forwarded_since_request();
        let response = rpc.send_blocking_unless_in_flight((
            state.thread_logical_time.clone(),
            state.mm_id,
            Q::WithForwardedCount(count, Box::new(request)),
        ));
        let ticket = match response {
            Ok((None, R::PrepareNativeExec(Ok(ticket)))) if ticket.0 != [0; 32] => ticket,
            Ok((None, R::PrepareNativeExec(Err(_)))) => {
                transaction
                    .lease
                    .cancel()
                    .map_err(|_| terminal("native exec reservation cancellation failed"))?;
                return Err(refused(
                    "native exec coordinator refused actual prepared state",
                ));
            }
            _ => {
                return Err(terminal(
                    "native exec Prepare response or connection differs",
                ));
            }
        };
        transaction.guard.armed = true;
        // Allocate all owned request envelopes and runtime storage before the
        // actual collector capture. After this point only bounded slice encodes.
        let mut seal = envelope(
            state,
            Q::SealNativeExecStats {
                ticket,
                runtime_digest: [0; 32],
                runtime: vec![0; runtime_packet::PACKET_BYTES],
            },
        );
        let mut issue = envelope(
            state,
            Q::IssueNativeExec {
                ticket,
                state_digest: [0; 32],
            },
        );
        let cancel = envelope(state, Q::CancelNativeExec { ticket });
        transaction.cancel_len = encode(&cancel, &mut transaction.cancel)?;
        transaction.cancel_expected_len = encode(
            &(
                <Option<detcore::types::LogicalTime>>::None,
                R::CancelNativeExec(Ok(())),
            ),
            &mut transaction.cancel_expected,
        )?;
        transaction.issue_expected_len = encode(
            &(
                <Option<detcore::types::LogicalTime>>::None,
                R::IssueNativeExec(Ok(())),
            ),
            &mut transaction.issue_expected,
        )?;
        detcore::detlog::check_empty_forwarding_cut()
            .map_err(|_| terminal("native exec counted Prepare left nonempty forwarding"))?;
        let stats = unsafe { NativeExecStatsV1::capture() }
            .map_err(|_| terminal("native exec actual runtime cannot be captured"))?;
        let sequence = unsafe { exec_initial::capture_dispatch_sequence() }
            .map_err(|_| terminal("native exec observer sequence cannot be captured"))?;
        let mut stats_bytes = [0; runtime_packet::STATS_BYTES];
        stats.encode_into(&mut stats_bytes);
        runtime_packet::RuntimePacket {
            sequence: sequence.last(),
            stats: &stats_bytes,
        }
        .encode(&mut transaction.runtime)
        .map_err(|_| terminal("native exec runtime envelope is invalid"))?;
        let digest = composite_digest(
            neutral_binding(self.binding),
            &transaction.snapshot,
            &transaction.runtime,
        )?;
        if let Q::WithForwardedCount(_, inner) = &mut seal.request.2
            && let Q::SealNativeExecStats {
                runtime_digest,
                runtime,
                ..
            } = &mut **inner
        {
            *runtime_digest = Sha256::digest(transaction.runtime).into();
            runtime.copy_from_slice(&transaction.runtime);
        } else {
            return Err(terminal("native exec internal Seal encoding is invalid"));
        }
        if let Q::WithForwardedCount(_, inner) = &mut issue.request.2
            && let Q::IssueNativeExec { state_digest, .. } = &mut **inner
        {
            *state_digest = digest;
        } else {
            return Err(terminal("native exec internal Issue encoding is invalid"));
        }
        transaction.seal_len = encode(&seal, &mut transaction.seal)?;
        transaction.issue_len = encode(&issue, &mut transaction.issue)?;
        transaction.seal_expected_len = encode(
            &(
                <Option<detcore::types::LogicalTime>>::None,
                R::SealNativeExecStats(Ok(digest)),
            ),
            &mut transaction.seal_expected,
        )?;
        transaction.input_len = input::encode_into(
            input::Packet {
                config_digest: &self.authority.config,
                config_schema: &self.authority.schema,
                native_source_digest: &self.authority.source,
                target_digest: &self.target().digest,
                bootstrap_digest: &self.target().bootstrap,
                image: input::Image::Successor {
                    ticket: &ticket.0,
                    composite_digest: &digest,
                },
                coordinator: b"/run/rpc.sock",
                detlog_identity: &self.detlog_identity,
                forward_policy: &self.authority.forward_policy,
                observer_identity: &self.observer_identity,
            },
            &mut transaction.input,
        )
        .map_err(|_| terminal("native exec exact successor input cannot be encoded"))?;
        transaction.provider_request = provider::request(&ticket.0);
        transaction.provider_expected = provider::expected_response(&ticket.0, &digest);
        transaction.stats = Some(stats);
        transaction.sequence = Some(sequence);
        transaction.final_check(tool, state, cut)?;
        exchange(
            rpc,
            &transaction.seal[..transaction.seal_len],
            &transaction.seal_expected[..transaction.seal_expected_len],
            &mut transaction.response,
        )?;
        let received = unsafe {
            provider::exchange(
                self.authority.provider_fd,
                &transaction.provider_request,
                &transaction.provider_expected,
            )
        }
        .map_err(|_| terminal("native exec one-use provider failed"))?;
        let fds = received
            .fds()
            .map_err(|_| terminal("native exec provider has wrong descriptor roles"))?;
        for (role, index) in [(0, 0), (1, 1), (0, 2), (2, 4)] {
            self.target().objects[role]
                .verify(fds[index], &mut transaction.scratch)
                .map_err(|_| terminal("native exec provider object identity or content differs"))?;
        }
        provider::verify_input(
            fds[3],
            &transaction.input[..transaction.input_len],
            &mut transaction.input_read,
        )
        .map_err(|_| terminal("native exec provider input differs from independent encoding"))?;
        unsafe { transaction.lease.install(fds) }
            .map_err(|_| terminal("native exec bootstrap installation failed"))?;
        received
            .close()
            .map_err(|_| terminal("native exec received descriptor cleanup failed"))?;
        transaction.final_check(tool, state, cut)?;
        Ok(transaction)
    }
}

impl Transaction<'_, '_> {
    fn final_check(
        &self,
        tool: &Detcore,
        state: &ThreadState<()>,
        cut: &ExecCut,
    ) -> Result<(), NativeExecFailure> {
        if cut != &self.session.cut
            || socket_fd(NativeSocket::Provider) != Some(self.session.authority.provider_fd)
        {
            return Err(terminal(
                "native exec final event or protected provider changed",
            ));
        }
        self.session.check_arguments(cut)?;
        self.lease
            .revalidate()
            .map_err(|_| terminal("native exec final bootstrap ownership changed"))?;
        // All actual kernel queries precede these final local exact checks.
        detcore::detlog::check_empty_forwarding_cut()
            .map_err(|_| terminal("native exec forwarding changed after Seal"))?;
        detcore::native_exec::check_prepared_native_exec_bytes(
            tool,
            state,
            &self.session.binding,
            &self.session.receipt,
            &self.snapshot,
        )
        .map_err(|_| terminal("native exec final exact Tool snapshot changed"))?;
        self.stats
            .as_ref()
            .ok_or_else(|| terminal("native exec runtime receipt is absent"))?
            .check_frozen_safe()?;
        unsafe {
            exec_initial::check_dispatch_sequence(
                self.sequence
                    .ok_or_else(|| terminal("native exec sequence receipt is absent"))?,
            )
        }
        .map_err(|_| terminal("native exec final dispatch sequence changed"))?;
        Ok(())
    }
}

trait FrozenStats {
    fn check_frozen_safe(&self) -> Result<(), NativeExecFailure>;
}
impl FrozenStats for NativeExecStatsV1 {
    fn check_frozen_safe(&self) -> Result<(), NativeExecFailure> {
        // SAFETY: only this transaction's owned, still-quiescent actual cut.
        unsafe { self.check_frozen() }
            .map_err(|_| terminal("native exec final runtime counters changed"))
    }
}

// SAFETY: all buffers/leases are owned before Seal; each actual Issue response
// is matched exactly and followed by the unchanged physical leaf's rechecks.
unsafe impl PreparedNativeExec<Detcore> for Transaction<'_, '_> {
    fn revalidate(
        &mut self,
        tool: &Detcore,
        state: &ThreadState<()>,
        cut: &ExecCut,
        _: &Rpc,
    ) -> Result<(), NativeExecFailure> {
        self.final_check(tool, state, cut)
    }
    unsafe fn issue_and_call(
        &mut self,
        tool: &Detcore,
        state: &ThreadState<()>,
        cut: &ExecCut,
        rpc: &Rpc,
    ) -> Result<ReturnedNativeExec, NativeExecFailure> {
        if core::mem::replace(&mut self.issued, true) {
            return Err(terminal("native exec Issue attempted twice"));
        }
        self.final_check(tool, state, cut)?;
        exchange(
            rpc,
            &self.issue[..self.issue_len],
            &self.issue_expected[..self.issue_expected_len],
            &mut self.response,
        )?;
        let leaf = unsafe { IssuedNativeExecLeaf::after_issue(cut)? };
        unsafe { leaf.call(|| self.final_check(tool, state, cut)) }
    }
    fn cancel(mut self: Box<Self>, rpc: &Rpc) -> Result<(), TerminalExecFailure> {
        self.lease
            .cancel()
            .map_err(|_| terminal_failure("native exec final descriptor cancellation failed"))?;
        exchange(
            rpc,
            &self.cancel[..self.cancel_len],
            &self.cancel_expected[..self.cancel_expected_len],
            &mut self.response,
        )
        .map_err(|_| terminal_failure("native exec exact cancellation response failed"))?;
        self.guard.armed = false;
        Ok(())
    }
}
