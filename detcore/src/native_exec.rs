/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Inactive, backend-neutral validation of a prepared single-thread exec cut.
//!
//! These APIs inspect actual Detcore data. They do not authorize kernel exec,
//! observe a physical task, install a backend, or run a lifecycle callback.
//! The caller must own the quiescent dispatch and separately prove the admitted
//! one-main-thread/childless domain, physical identity and registrations.
//! In particular, probing an Arc's lock does not establish that no other task
//! owns it. No ordinary exec path calls this module.
//!
//! The transport consumer must bound the payload before allocation/decode,
//! authenticate its exact bytes and Config, and reject trailing bytes. It must
//! retain the same live cut through its final physical exec edge. Deserializing
//! serde-skipped capabilities cannot reconstruct them; the live producer must
//! validate them before preparing the replacement table.

use std::fmt;
use std::io;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;

use serde::Deserialize;
use serde::Serialize;
use sha2::Digest as _;
use sha2::Sha256;

use crate::Config;
use crate::Detcore;
use crate::FileMetadata;
use crate::ThreadState;
/// The concrete empty subtool supported by this first typed snapshot domain.
pub use crate::record_or_replay::NoopTool;
use crate::types::DetPid;
use crate::types::DetTid;
use crate::types::FilesId;
use crate::types::MmId;

/// Version of the seven-field Detcore prepared-exec payload.
pub const DETCORE_EXEC_SNAPSHOT_VERSION: u16 = 2;

/// Maximum encoded snapshot size enforced by the concrete transport consumer.
pub const MAX_SNAPSHOT_BYTES: usize = 1024 * 1024;

/// A data validation refusal, not a backend errno or admission capability.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeExecCutError {
    /// The supplied binding or a Tool/state/header identity disagrees.
    Identity(&'static str),
    /// The immutable configuration is outside the named snapshot domain.
    Configuration(&'static str),
    /// Actual embedded Config bytes disagree with the admitted digest.
    ConfigDigest,
    /// Existing Config serialization failed.
    ConfigEncoding,
    /// A skipped runtime capability or live operation remains.
    LiveCapability(&'static str),
    /// A supposedly quiescent model is locked or poisoned.
    LockUnavailable(&'static str),
    /// The replacement state was not prepared as the existing exec path does.
    PreparedState(&'static str),
    /// A table slot or its within-table alias identity is inconsistent.
    Descriptor(&'static str),
    /// The decoded payload version is unsupported.
    Version,
}

impl fmt::Display for NativeExecCutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (category, field) = match *self {
            Self::Identity(field) => ("identity mismatch", field),
            Self::Configuration(field) => ("unsupported configuration", field),
            Self::ConfigDigest => ("Config digest mismatch", "Config"),
            Self::ConfigEncoding => ("Config encoding failed", "Config"),
            Self::LiveCapability(field) => ("live runtime capability", field),
            Self::LockUnavailable(field) => ("model locked or poisoned", field),
            Self::PreparedState(field) => ("unprepared replacement state", field),
            Self::Descriptor(field) => ("descriptor mismatch", field),
            Self::Version => ("unsupported snapshot version", "version"),
        };
        write!(f, "native exec cut {category}: {field}")
    }
}

impl std::error::Error for NativeExecCutError {}

/// Bind the genuine initial physical leader before its first thread-start.
/// This does not initialize deterministic process identity or run a callback.
///
/// # Safety
/// The caller owns the actual authenticated InitialImage lifecycle and passes
/// PID/TID read from that same current kernel task through its qualified gate.
/// No guest work has run; this is the unique freshly constructed initial state.
pub unsafe fn bind_native_initial_physical_identity(
    thread: &mut ThreadState<()>,
    physical_pid: i32,
    physical_tid: i32,
) -> Result<(), NativeExecCutError> {
    if physical_tid <= 0
        || physical_pid != physical_tid
        || thread.dettid.as_raw() != physical_tid
        || thread.physical_tid.is_some()
        || thread.detpid.is_some()
        || thread.thread_start_entered
        || thread.mm_id != MmId::initial(thread.dettid)
        || try_native_exec_lock(&thread.file_metadata, "initial file_metadata")?
            .native_exec_files_id()
            != FilesId::initial(thread.dettid)
    {
        return Err(NativeExecCutError::Identity("initial physical state"));
    }
    if thread.signal_task_identity.is_some() {
        return Err(NativeExecCutError::LiveCapability("signal_task_identity"));
    }
    thread.physical_tid = Some(physical_tid);
    Ok(())
}

/// Rebind from the actual fresh Guest answer before native post-exec.
/// The first domain has no admitted signal-control capability; equal None is
/// supported data, never a fabricated task-lifetime identity.
pub fn bind_native_fresh_signal_identity(
    thread: &mut ThreadState<()>,
    mode: reverie::BackendSignalControlMode,
    actual: Option<reverie::SignalTaskIdentity>,
) -> Result<(), NativeExecCutError> {
    if mode != reverie::BackendSignalControlMode::Unchanged
        || thread.signal_task_identity.is_some()
        || actual.is_some()
    {
        return Err(NativeExecCutError::LiveCapability("signal_task_identity"));
    }
    thread.signal_task_identity = actual;
    Ok(())
}

/// Independently supplied identities of the existing prepared transaction.
///
/// This is data, not proof of watched birth, physical state, or childlessness.
/// The old mm/table identities must come from the live owned transaction,
/// never from the payload being validated.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NativeExecCutBinding {
    /// Deterministic main thread identity.
    pub dettid: DetTid,
    /// Continuing deterministic process identity; equal to dettid here.
    pub detpid: DetPid,
    /// Actual continuing physical main-thread ID proved by the backend.
    pub physical_tid: i32,
    /// Live old-image memory incarnation.
    pub old_mm: MmId,
    /// Live old-image descriptor-table incarnation.
    pub old_files: FilesId,
    /// Independently admitted SHA-256 of the unchanged legacy wire Config.
    pub config_digest: [u8; 32],
}

impl NativeExecCutBinding {
    fn validate(&self) -> Result<(), NativeExecCutError> {
        if self.dettid != self.detpid || self.detpid.as_raw() <= 0 {
            return Err(NativeExecCutError::Identity("main-thread IDs"));
        }
        if self.physical_tid <= 0 {
            return Err(NativeExecCutError::Identity("physical_tid"));
        }
        if self.old_mm.generation() == u64::MAX || self.old_files.generation() == u64::MAX {
            return Err(NativeExecCutError::Identity("incarnation overflow"));
        }
        Ok(())
    }

    /// Replacement mm produced by the existing same-leader exec transition.
    pub fn prepared_mm(&self) -> MmId {
        self.old_mm.for_exec(self.detpid)
    }

    /// Replacement descriptor-table identity produced by the existing path.
    pub fn prepared_files(&self) -> FilesId {
        self.old_files.for_exec(self.dettid)
    }
}

/// Dispatch-owned evidence that the actual old table was inspected.
///
/// This deliberately implements neither Serialize/Deserialize nor Clone.
/// Retaining the old table also lets the prepared cut recheck its skipped
/// capabilities, including descriptions whose last slot was CLOEXEC.
/// It is not a global admission or physical-exec authority.
#[derive(Debug)]
pub struct NativeExecPreparationReceipt {
    binding: NativeExecCutBinding,
    old_table: Arc<Mutex<FileMetadata>>,
}

/// A borrowed encoding of the actual already-prepared Tool and main state.
///
/// Field order and representation match the owned version. No new state is
/// constructed and no PRNG, accounting or clock observation is performed.
/// The receipt borrow keeps its old-table ownership alive during encoding.
/// The caller must still keep all interior model data quiescent.
#[derive(Debug, Serialize)]
pub struct DetcoreExecSnapshotRefV2<'a> {
    version: u16,
    dettid: DetTid,
    detpid: DetPid,
    prepared_mm: MmId,
    config_digest: [u8; 32],
    tool: &'a Detcore<NoopTool>,
    thread: &'a ThreadState<()>,
    #[serde(skip)]
    _receipt: &'a NativeExecPreparationReceipt,
}

/// Owned data decoded only after exact bounded transport authentication.
///
/// Deserialization restores the existing within-table OFD aliases. It cannot
/// prove that skipped producer capabilities were absent before serialization.
/// Use `into_validated_parts` before attaching this data to a replacement host.
#[derive(Debug, Serialize, Deserialize)]
pub struct DetcoreExecSnapshotV2 {
    version: u16,
    dettid: DetTid,
    detpid: DetPid,
    prepared_mm: MmId,
    config_digest: [u8; 32],
    tool: Detcore<NoopTool>,
    thread: ThreadState<()>,
}

impl DetcoreExecSnapshotV2 {
    /// Check the decoded header and actual state against independent admission.
    ///
    /// The expected Config is independently obtained from the admitted run.
    /// This leaves every field and capability untouched. Backend incarnation
    /// rebinding (including signal_task_identity) remains the host's explicit
    /// responsibility before any restored lifecycle use.
    pub fn validate(
        &self,
        binding: &NativeExecCutBinding,
        expected_config: &Config,
    ) -> Result<(), NativeExecCutError> {
        binding.validate()?;
        if self.version != DETCORE_EXEC_SNAPSHOT_VERSION {
            return Err(NativeExecCutError::Version);
        }
        if self.dettid != binding.dettid || self.detpid != binding.detpid {
            return Err(NativeExecCutError::Identity("snapshot IDs"));
        }
        if self.prepared_mm != binding.prepared_mm() {
            return Err(NativeExecCutError::Identity("snapshot prepared_mm"));
        }
        // The admitted host Config can retain host-only output paths. Its
        // omitted semantic fields must nevertheless be checked before wire
        // equality loses them.
        validate_native_exec_host_configuration(expected_config)?;
        if self.config_digest != binding.config_digest
            || native_exec_config_digest(expected_config)? != binding.config_digest
        {
            return Err(NativeExecCutError::ConfigDigest);
        }
        validate_tool_and_thread(&self.tool, &self.thread, binding)?;
        self.thread.validate_prepared_native_exec_cut(binding)
    }

    /// Consume this payload only after all neutral validation succeeds.
    pub fn into_validated_parts(
        self,
        binding: &NativeExecCutBinding,
        expected_config: &Config,
    ) -> Result<(Detcore<NoopTool>, ThreadState<()>), NativeExecCutError> {
        self.validate(binding, expected_config)?;
        Ok((self.tool, self.thread))
    }
}

/// Validate actual old state before `FileMetadata::for_exec` drops capabilities.
///
/// This is an explicit inactive API; it is not an added ordinary syscall hook.
pub fn validate_old_native_exec_cut(
    tool: &Detcore<NoopTool>,
    thread: &ThreadState<()>,
    binding: &NativeExecCutBinding,
) -> Result<NativeExecPreparationReceipt, NativeExecCutError> {
    binding.validate()?;
    validate_tool_and_thread(tool, thread, binding)?;
    if thread.mm_id != binding.old_mm {
        return Err(NativeExecCutError::Identity("old mm"));
    }
    {
        let table = try_native_exec_lock(&thread.file_metadata, "old file_metadata")?;
        table.validate_native_exec_cut(binding.old_files, false)?;
    }
    thread.validate_old_native_exec_robust_state()?;
    Ok(NativeExecPreparationReceipt {
        binding: *binding,
        old_table: Arc::clone(&thread.file_metadata),
    })
}

/// Borrow the actual prepared state, bound to its live old-table receipt.
///
/// Both old and prepared tables are checked without comparing their total Arc
/// counts: the existing rollback future legitimately owns old-table aliases.
pub fn capture_prepared_native_exec<'a>(
    tool: &'a Detcore<NoopTool>,
    thread: &'a ThreadState<()>,
    binding: &NativeExecCutBinding,
    receipt: &'a NativeExecPreparationReceipt,
) -> Result<DetcoreExecSnapshotRefV2<'a>, NativeExecCutError> {
    binding.validate()?;
    if receipt.binding != *binding {
        return Err(NativeExecCutError::Identity("preparation receipt"));
    }
    {
        let old = try_native_exec_lock(&receipt.old_table, "old file_metadata")?;
        old.validate_native_exec_cut(binding.old_files, false)?;
    }
    if Arc::ptr_eq(&receipt.old_table, &thread.file_metadata) {
        return Err(NativeExecCutError::PreparedState(
            "replacement table ownership",
        ));
    }
    validate_tool_and_thread(tool, thread, binding)?;
    thread.validate_prepared_native_exec_cut(binding)?;
    Ok(DetcoreExecSnapshotRefV2 {
        version: DETCORE_EXEC_SNAPSHOT_VERSION,
        dettid: binding.dettid,
        detpid: binding.detpid,
        prepared_mm: binding.prepared_mm(),
        config_digest: binding.config_digest,
        tool,
        thread,
        _receipt: receipt,
    })
}

/// Compare the final actual prepared state directly against already sealed
/// bytes, using the unchanged legacy serializer without a temporary encoding.
/// The caller retains the same exclusive dispatch and old-table receipt.
/// This checks the complete encoding, including exact length, and never resets
/// accounting, advances a PRNG, or constructs a replacement model.
pub fn check_prepared_native_exec_bytes(
    tool: &Detcore<NoopTool>,
    thread: &ThreadState<()>,
    binding: &NativeExecCutBinding,
    receipt: &NativeExecPreparationReceipt,
    expected: &[u8],
) -> Result<(), NativeExecCutError> {
    if expected.is_empty() || expected.len() > MAX_SNAPSHOT_BYTES {
        return Err(NativeExecCutError::PreparedState("sealed snapshot bounds"));
    }
    let live = capture_prepared_native_exec(tool, thread, binding, receipt)?;
    compare_encoding(&live, expected)
}

fn compare_encoding<T: Serialize + ?Sized>(
    live: &T,
    expected: &[u8],
) -> Result<(), NativeExecCutError> {
    struct CompareWriter<'a> {
        expected: &'a [u8],
        position: usize,
        failed: bool,
    }
    impl io::Write for CompareWriter<'_> {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            let end = self.position.checked_add(bytes.len());
            if self.failed
                || end.is_none_or(|end| self.expected.get(self.position..end) != Some(bytes))
            {
                self.failed = true;
                return Err(io::ErrorKind::InvalidData.into());
            }
            self.position = end.unwrap();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut writer = CompareWriter {
        expected,
        position: 0,
        failed: false,
    };
    let result =
        bincode::serde::encode_into_std_write(live, &mut writer, bincode::config::legacy());
    if result.is_err() || writer.failed || writer.position != expected.len() {
        return Err(NativeExecCutError::PreparedState("sealed snapshot changed"));
    }
    Ok(())
}

fn validate_tool_and_thread(
    tool: &Detcore<NoopTool>,
    thread: &ThreadState<()>,
    binding: &NativeExecCutBinding,
) -> Result<(), NativeExecCutError> {
    if tool.detpid != binding.detpid {
        return Err(NativeExecCutError::Identity("Tool detpid"));
    }
    validate_native_exec_guest_configuration(&tool.cfg)?;
    if native_exec_config_digest(&tool.cfg)? != binding.config_digest {
        return Err(NativeExecCutError::ConfigDigest);
    }
    thread.validate_native_exec_cut(binding, &tool.cfg)
}

/// SHA-256 of the actual unchanged Config in the existing legacy bincode form.
///
/// This is a neutral identity operation, not transport framing or admission.
/// It streams into the existing hash dependency instead of cloning Config.
pub fn native_exec_config_digest(config: &Config) -> Result<[u8; 32], NativeExecCutError> {
    struct HashWriter(Sha256);
    impl io::Write for HashWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.update(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut writer = HashWriter(Sha256::new());
    bincode::serde::encode_into_std_write(config, &mut writer, bincode::config::legacy())
        .map_err(|_| NativeExecCutError::ConfigEncoding)?;
    Ok(writer.0.finalize().into())
}

/// Check the actual host Config before granting native InitialImage or exec authority.
///
/// `host_input_log` and `fatal_core_capture` belong to the continuing host
/// GlobalState. This policy preserves them without copying them to the guest.
/// Their presence does not establish that the caller retained that owner; the
/// launcher must keep it alive through exec and the final physical drain.
/// Neither this check nor a Config digest establishes runtime admission.
pub fn validate_native_exec_host_configuration(config: &Config) -> Result<(), NativeExecCutError> {
    validate_native_exec_common_configuration(config)
}

/// Check the live guest Config before snapshotting or using a restored Tool.
///
/// Serde omits these fields, so a wire digest cannot establish their absence.
/// The continuing host owns output paths; the guest must not carry a discarded
/// copy. This function inspects the Config without clearing or rebinding it.
pub fn validate_native_exec_guest_configuration(config: &Config) -> Result<(), NativeExecCutError> {
    validate_native_exec_common_configuration(config)?;
    if config.host_input_log.is_some() {
        return Err(NativeExecCutError::Configuration("host_input_log"));
    }
    if config.fatal_core_capture.is_some() {
        return Err(NativeExecCutError::Configuration("fatal_core_capture"));
    }
    Ok(())
}

fn validate_native_exec_common_configuration(config: &Config) -> Result<(), NativeExecCutError> {
    // The guest branches on presence, including Some(empty). Do not silently
    // turn a resolved program into None by passing it through serde.
    if config.happens_before.is_some() {
        return Err(NativeExecCutError::Configuration("happens_before"));
    }
    // This is an actual image address, never a transferable Config constant.
    if config.untraced_code_range.is_some() {
        return Err(NativeExecCutError::Configuration("untraced_code_range"));
    }
    if config.use_rcb_time() || config.max_timeslice.is_some() || config.target_timeslice.is_some()
    {
        return Err(NativeExecCutError::Configuration("RCB/timeslices"));
    }
    if config.imprecise_timers
        || config.chaos
        || config.chaos_target_races
        || config.chaos_per_thread_slowdown
    {
        return Err(NativeExecCutError::Configuration("chaos/imprecise timers"));
    }
    if config.record_preemptions
        || config.record_preemptions_to.is_some()
        || config.replay_preemptions_from.is_some()
        || config.replay_schedule_from.is_some()
        || config.preemption_stacktrace
        || config.preemption_stacktrace_log_file.is_some()
        || !config.stacktrace_event.is_empty()
        || config.stacktrace_signal.is_some()
    {
        return Err(NativeExecCutError::Configuration(
            "preemption/replay/stacktrace",
        ));
    }
    if config.recordreplay_modes || config.replaying || config.replay_data.is_some() {
        return Err(NativeExecCutError::Configuration("record/replay"));
    }
    Ok(())
}

pub(crate) fn try_native_exec_lock<'a, T>(
    lock: &'a Mutex<T>,
    field: &'static str,
) -> Result<MutexGuard<'a, T>, NativeExecCutError> {
    lock.try_lock()
        .map_err(|_| NativeExecCutError::LockUnavailable(field))
}

#[cfg(test)]
pub(crate) mod tests {
    use std::time::Duration;

    use nix::fcntl::OFlag;
    use rand::RngExt as _;

    use super::*;
    use crate::fd::DetFd;
    use crate::fd::FdType;
    use crate::memory::MemoryMetadata;
    use crate::tool_local::ResourceLimit;
    use crate::tool_local::RobustListWake;
    use crate::types::FutexID;
    use crate::types::LogicalTime;
    use crate::types::OpenFileId;
    use crate::types::RcbTimeMultiplier;
    use crate::types::SharedMemoryObjectId;

    pub(crate) fn fixture() -> (Detcore<NoopTool>, ThreadState<()>, NativeExecCutBinding) {
        let owner = DetTid::from_raw(42);
        let cfg = Config {
            sequentialize_threads: true,
            no_rcb_time: true,
            max_timeslice: None,
            target_timeslice: None,
            rng_seed: Some(17),
            sched_seed: Some(31),
            clock_multiplier: Some(1.25),
            ..Config::default()
        };
        let tool = Detcore {
            detpid: owner,
            cfg,
            record_or_replay: NoopTool,
        };
        let mut thread = ThreadState::new(owner, &tool.cfg, ());
        thread.detpid = Some(owner);
        thread.physical_tid = Some(4242);
        thread.thread_start_entered = true;
        thread.past_global_first_execve = true;
        let mut files = FileMetadata::new(owner);
        for _ in 0..3 {
            thread.mm_id = thread.mm_id.for_exec(owner);
            files = files.for_exec(owner);
        }
        let old_files = files.files_id;
        thread.file_metadata = Arc::new(Mutex::new(files));
        let binding = NativeExecCutBinding {
            dettid: owner,
            detpid: owner,
            physical_tid: 4242,
            old_mm: thread.mm_id,
            old_files,
            config_digest: native_exec_config_digest(&tool.cfg).unwrap(),
        };
        (tool, thread, binding)
    }

    pub(crate) fn prepare(thread: &mut ThreadState<()>, binding: &NativeExecCutBinding) {
        let files = thread
            .file_metadata
            .lock()
            .unwrap()
            .for_exec(binding.dettid);
        thread.file_metadata = Arc::new(Mutex::new(files));
        thread.mm_id = binding.prepared_mm();
        thread.memory_metadata = Arc::new(Mutex::new(MemoryMetadata::new()));
        thread.take_robust_list_for_exec();
    }

    // JSON Value cannot represent the full-width u128 PCG state. Compare all
    // serialized fields in the actual wire format, without narrowing values.
    // Cross-decode byte comparisons below use fixtures with empty HashMaps;
    // nonempty descriptor maps are compared by entries in the alias test.
    pub(crate) fn state_bytes<T: Serialize + ?Sized>(state: &T) -> Vec<u8> {
        bincode::serde::encode_to_vec(state, bincode::config::legacy()).unwrap()
    }

    fn encoded(
        tool: &Detcore<NoopTool>,
        thread: &ThreadState<()>,
        binding: &NativeExecCutBinding,
        receipt: &NativeExecPreparationReceipt,
    ) -> Vec<u8> {
        bincode::serde::encode_to_vec(
            capture_prepared_native_exec(tool, thread, binding, receipt).unwrap(),
            bincode::config::legacy(),
        )
        .unwrap()
    }

    pub(crate) fn decoded(bytes: &[u8]) -> DetcoreExecSnapshotV2 {
        let (state, consumed) = bincode::serde::decode_from_slice(
            bytes,
            bincode::config::legacy().with_limit::<MAX_SNAPSHOT_BYTES>(),
        )
        .unwrap();
        assert_eq!(consumed, bytes.len());
        state
    }

    pub(crate) fn thread_for_test(snapshot: &mut DetcoreExecSnapshotV2) -> &mut ThreadState<()> {
        &mut snapshot.thread
    }

    #[test]
    fn native_exec_v2_refuses_v1_before_returning_owned_state() {
        let (tool, mut thread, binding) = fixture();
        let receipt = validate_old_native_exec_cut(&tool, &thread, &binding).unwrap();
        prepare(&mut thread, &binding);
        let mut payload = decoded(&encoded(&tool, &thread, &binding, &receipt));
        assert_eq!(payload.version, 2);
        payload.version = 1;
        assert_eq!(
            payload.validate(&binding, &tool.cfg),
            Err(NativeExecCutError::Version)
        );
        assert_eq!(
            payload
                .into_validated_parts(&binding, &tool.cfg)
                .unwrap_err(),
            NativeExecCutError::Version
        );
    }

    #[test]
    fn native_exec_final_exact_comparison_keeps_state_and_refuses_every_byte_change() {
        let (tool, mut thread, binding) = fixture();
        let receipt = validate_old_native_exec_cut(&tool, &thread, &binding).unwrap();
        prepare(&mut thread, &binding);
        let bytes = encoded(&tool, &thread, &binding, &receipt);
        let before = state_bytes(&thread);
        check_prepared_native_exec_bytes(&tool, &thread, &binding, &receipt, &bytes).unwrap();
        assert_eq!(state_bytes(&thread), before);
        for index in 0..bytes.len() {
            let mut bad = bytes.clone();
            bad[index] ^= 1;
            assert!(
                check_prepared_native_exec_bytes(&tool, &thread, &binding, &receipt, &bad).is_err(),
                "byte {index}"
            );
        }
        for end in 0..bytes.len() {
            assert!(
                check_prepared_native_exec_bytes(&tool, &thread, &binding, &receipt, &bytes[..end])
                    .is_err(),
                "truncation {end}"
            );
        }
        let mut extra = bytes.clone();
        extra.push(0);
        assert!(
            check_prepared_native_exec_bytes(&tool, &thread, &binding, &receipt, &extra).is_err()
        );
        assert_eq!(state_bytes(&thread), before);
        let _ = thread.prng.random::<u64>();
        assert!(
            check_prepared_native_exec_bytes(&tool, &thread, &binding, &receipt, &bytes).is_err()
        );
        let changed = encoded(&tool, &thread, &binding, &receipt);
        check_prepared_native_exec_bytes(&tool, &thread, &binding, &receipt, &changed).unwrap();
        thread.stats.count_syscall();
        assert!(
            check_prepared_native_exec_bytes(&tool, &thread, &binding, &receipt, &changed).is_err()
        );
    }

    #[test]
    fn native_exec_final_comparison_latches_ignored_serializer_errors() {
        struct Ignores;
        impl Serialize for Ignores {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                use serde::ser::SerializeTuple;
                let mut tuple = serializer.serialize_tuple(2)?;
                let _ = tuple.serialize_element(&99u64);
                let _ = tuple.serialize_element(&17u64);
                tuple.end()
            }
        }
        assert!(compare_encoding(&Ignores, &17u64.to_le_bytes()).is_err());
        let valid = state_bytes(&(99u64, 17u64));
        compare_encoding(&Ignores, &valid).unwrap();
    }

    #[test]
    fn native_exec_roundtrip_continues_full_noninitial_clock_accounting_and_prngs() {
        let (tool, mut thread, binding) = fixture();
        for _ in 0..13 {
            let _ = thread.prng.random::<u64>();
        }
        for _ in 0..19 {
            let _ = thread.chaos_prng.random::<u64>();
        }
        thread.pedigree = thread.pedigree.fork().1.fork().0;
        thread.stats.count_syscall();
        thread.stats.count_syscall();
        thread.stats.count_signal();
        thread.stats.timeslice_stats.record(173);
        thread.timer_slack_ns = 137;
        thread.default_timer_slack_ns = 91;
        thread.resource_limits.lock().unwrap().set(
            libc::RLIMIT_NOFILE,
            ResourceLimit {
                current: 97,
                maximum: 211,
            },
        );
        thread.thread_logical_time.add_syscall_with_cost(137);
        thread
            .thread_logical_time
            .add_rcbs_with_multiplier(7, RcbTimeMultiplier::from_f64(1.5));
        thread.thread_logical_time.add_cpuid();
        thread.thread_logical_time = thread.thread_logical_time.clone_for_child();
        thread.thread_cpu_start_user_time = thread.thread_logical_time.user_cpu_time();
        thread.thread_cpu_start_system_time = thread.thread_logical_time.system_cpu_time();
        thread.account_process_cpu_time();
        thread.thread_logical_time.add_syscall_with_cost(19); // deliberately unaccounted
        thread.thread_logical_time.add_rdtsc();
        thread.committed_clock_value = 7;
        let cut_time = thread.thread_logical_time.as_nanos() + Duration::from_nanos(43);
        thread.thread_logical_time.advance_to(cut_time);
        thread.observe_guest_clock(cut_time + Duration::from_nanos(23));
        thread.end_of_timeslice = Some(cut_time + Duration::from_nanos(101));
        thread.last_grant_deadline = Some(Duration::from_nanos(211));
        let receipt = validate_old_native_exec_cut(&tool, &thread, &binding).unwrap();
        prepare(&mut thread, &binding);
        let before = state_bytes(&thread);
        let bytes = encoded(&tool, &thread, &binding, &receipt);
        assert_eq!(
            encoded(&tool, &thread, &binding, &receipt),
            bytes,
            "validation must not advance or normalize the live state"
        );
        let payload = decoded(&bytes);
        payload.validate(&binding, &tool.cfg).unwrap();
        assert_eq!(state_bytes(&payload.thread), before);
        assert_ne!(
            payload.thread.thread_logical_time.inherited_nanos(),
            LogicalTime::ZERO
        );
        assert_eq!(payload.thread.thread_logical_time.as_nanos(), cut_time);
        assert_eq!(
            payload.thread.observe_guest_clock(LogicalTime::ZERO),
            cut_time + Duration::from_nanos(23)
        );
        // Decode a second independent oracle: Arc::clone would share its
        // accounting/floor models and conceal a reset or double charge.
        let mut oracle = decoded(&bytes).thread;
        let (_, mut restored) = payload.into_validated_parts(&binding, &tool.cfg).unwrap();
        for cost in [1, 7, 103, 29, 2, 71, 11, 43] {
            assert_eq!(restored.prng.random::<u64>(), oracle.prng.random::<u64>());
            assert_eq!(
                restored.chaos_prng.random::<u64>(),
                oracle.chaos_prng.random::<u64>()
            );
            let previous = restored.thread_logical_time.as_nanos();
            restored.thread_logical_time.add_syscall_with_cost(cost);
            oracle.thread_logical_time.add_syscall_with_cost(cost);
            restored.thread_logical_time.add_cpuid();
            oracle.thread_logical_time.add_cpuid();
            assert!(restored.thread_logical_time.as_nanos() > previous);
            assert_eq!(
                state_bytes(&restored.thread_logical_time),
                state_bytes(&oracle.thread_logical_time)
            );
            let now = restored.thread_logical_time.as_nanos();
            assert_eq!(
                restored.observe_guest_clock(now),
                oracle.observe_guest_clock(now)
            );
            restored.account_process_cpu_time();
            oracle.account_process_cpu_time();
            let accounted = state_bytes(&*restored.process_cpu_time.lock().unwrap());
            restored.account_process_cpu_time();
            assert_eq!(
                state_bytes(&*restored.process_cpu_time.lock().unwrap()),
                accounted
            );
            assert_eq!(
                accounted,
                state_bytes(&*oracle.process_cpu_time.lock().unwrap())
            );
            assert_eq!(state_bytes(&restored), state_bytes(&oracle));
        }
    }

    #[test]
    fn native_exec_prepared_aliases_cloexec_and_old_receipt_are_preserved() {
        let (tool, mut thread, binding) = fixture();
        thread.add_fd(30, OFlag::O_RDWR, FdType::Rng, None).unwrap();
        thread.dup_fd(30, 31, OFlag::empty()).unwrap();
        thread.dup_fd(30, 32, OFlag::O_CLOEXEC).unwrap();
        thread
            .add_fd(33, OFlag::O_CLOEXEC, FdType::Rng, None)
            .unwrap();
        thread
            .add_fd(34, OFlag::empty(), FdType::Rng, None)
            .unwrap();
        thread
            .with_detfd(31, |fd| fd.advance_random_device_offset(37))
            .unwrap();
        let receipt = validate_old_native_exec_cut(&tool, &thread, &binding).unwrap();
        prepare(&mut thread, &binding);
        // The receipt and actual rollback table retain additional Arc owners.
        assert!(
            receipt
                .old_table
                .lock()
                .unwrap()
                .file_handles
                .contains_key(&33)
        );
        let old_prepared = serde_json::to_value(&*thread.file_metadata.lock().unwrap()).unwrap();
        let payload = decoded(&encoded(&tool, &thread, &binding, &receipt));
        payload.validate(&binding, &tool.cfg).unwrap();
        let table = payload.thread.file_metadata.lock().unwrap();
        assert_eq!(serde_json::to_value(&*table).unwrap(), old_prepared);
        assert!(!table.file_handles.contains_key(&32));
        assert!(!table.file_handles.contains_key(&33));
        let first = table.file_handles[&30]
            .validate_native_exec_runtime_state()
            .unwrap();
        let alias = table.file_handles[&31]
            .validate_native_exec_runtime_state()
            .unwrap();
        let distinct = table.file_handles[&34]
            .validate_native_exec_runtime_state()
            .unwrap();
        assert_eq!(first, alias);
        assert_ne!(first.0, distinct.0);
        assert_ne!(first.1, distinct.1);
        table.file_handles[&31].advance_random_device_offset(11);
        assert_eq!(table.file_handles[&30].random_device_offset(), 48);
        assert_eq!(table.file_handles[&34].random_device_offset(), 0);
        drop(table);
        for (slot, kind) in [(40, FdType::Rng), (41, FdType::Socket), (42, FdType::Rng)] {
            thread.add_fd(slot, OFlag::empty(), kind, None).unwrap();
            payload
                .thread
                .add_fd(slot, OFlag::empty(), kind, None)
                .unwrap();
            assert_eq!(
                thread.with_detfd(slot, |fd| fd.open_file_id()).unwrap(),
                payload
                    .thread
                    .with_detfd(slot, |fd| fd.open_file_id())
                    .unwrap(),
                "both independent OFD allocators must continue their saved sequence"
            );
        }
        // A held lease on a description absent from the prepared table must
        // still be refused by the live old-table receipt.
        let lock = receipt.old_table.lock().unwrap().file_handles[&33].directory_lock();
        let lease = lock.try_lock().unwrap();
        assert!(matches!(
            capture_prepared_native_exec(&tool, &thread, &binding, &receipt),
            Err(NativeExecCutError::LiveCapability("directory lease"))
        ));
        drop(lease);
        capture_prepared_native_exec(&tool, &thread, &binding, &receipt).unwrap();
    }

    #[test]
    fn native_exec_live_alias_conflict_is_refused_before_interning() {
        let (tool, thread, binding) = fixture();
        let id = OpenFileId::new(binding.dettid, 7);
        let mut table = thread.file_metadata.lock().unwrap();
        table
            .file_handles
            .insert(3, DetFd::new(3, OFlag::empty(), FdType::Rng, id));
        table
            .file_handles
            .insert(4, DetFd::new(4, OFlag::empty(), FdType::Rng, id));
        drop(table);
        assert!(matches!(
            validate_old_native_exec_cut(&tool, &thread, &binding),
            Err(NativeExecCutError::Descriptor("OFD alias identity"))
        ));
    }

    #[test]
    fn native_exec_staged_robust_wakes_keep_old_keys_and_are_consumed_once() {
        let (tool, mut thread, binding) = fixture();
        thread.record_robust_list_head(Some(0x123000));
        let receipt = validate_old_native_exec_cut(&tool, &thread, &binding).unwrap();
        prepare(&mut thread, &binding);
        let shared = SharedMemoryObjectId::File {
            device: 7,
            inode: 11,
        };
        thread.stage_robust_list_wakes_for_exec(vec![(
            binding.dettid,
            vec![
                RobustListWake {
                    futex: FutexID::private(binding.old_mm, 0x456000),
                },
                RobustListWake {
                    futex: FutexID::shared(shared, 64),
                },
                RobustListWake {
                    futex: FutexID::private(binding.old_mm, 0x456008),
                },
            ],
        )]);
        let payload = decoded(&encoded(&tool, &thread, &binding, &receipt));
        payload.validate(&binding, &tool.cfg).unwrap();
        assert_eq!(state_bytes(&payload.thread), state_bytes(&thread));
        let actual = payload.thread.take_robust_list_wakes_for_exec();
        assert_eq!(actual, thread.take_robust_list_wakes_for_exec());
        assert_eq!(actual.len(), 3);
        assert!(payload.thread.take_robust_list_wakes_for_exec().is_empty());
        assert_ne!(
            FutexID::private(binding.old_mm, 0x456000),
            FutexID::private(binding.prepared_mm(), 0x456000)
        );
    }

    #[test]
    fn native_exec_binding_config_and_prepared_identity_mismatches_refuse() {
        let (tool, mut thread, binding) = fixture();
        let receipt = validate_old_native_exec_cut(&tool, &thread, &binding).unwrap();
        prepare(&mut thread, &binding);
        let bytes = encoded(&tool, &thread, &binding, &receipt);
        let mut changed_config = tool.cfg.clone();
        changed_config.rng_seed = Some(23);
        assert_eq!(
            decoded(&bytes).validate(&binding, &changed_config),
            Err(NativeExecCutError::ConfigDigest)
        );
        for field in 0..7 {
            let mut payload = decoded(&bytes);
            match field {
                0 => payload.version += 1,
                1 => payload.dettid = DetTid::from_raw(43),
                2 => payload.detpid = DetTid::from_raw(43),
                3 => payload.prepared_mm = binding.old_mm,
                4 => payload.config_digest[0] ^= 1,
                5 => payload.tool.detpid = DetTid::from_raw(43),
                6 => payload.thread.physical_tid = Some(4243),
                _ => unreachable!(),
            }
            assert!(
                payload.validate(&binding, &tool.cfg).is_err(),
                "field {field}"
            );
        }
        let mut other_binding = binding;
        other_binding.physical_tid += 1;
        assert!(capture_prepared_native_exec(&tool, &thread, &other_binding, &receipt).is_err());
        thread.mm_id = binding.old_mm;
        assert!(capture_prepared_native_exec(&tool, &thread, &binding, &receipt).is_err());
    }

    #[test]
    fn native_exec_guest_config_refuses_skipped_values_with_equal_wire_bytes() {
        for field in [
            "happens_before",
            "host_input_log",
            "fatal_core_capture",
            "untraced_code_range",
        ] {
            let (mut tool, mut thread, binding) = fixture();
            let receipt = validate_old_native_exec_cut(&tool, &thread, &binding).unwrap();
            prepare(&mut thread, &binding);
            let mut payload = decoded(&encoded(&tool, &thread, &binding, &receipt));
            let expected_config = tool.cfg.clone();
            let wire = state_bytes(&tool.cfg);
            set_skipped_config_field(&mut tool.cfg, field);
            set_skipped_config_field(&mut payload.tool.cfg, field);
            assert_eq!(state_bytes(&tool.cfg), wire, "field {field}");
            assert_eq!(state_bytes(&payload.tool.cfg), wire, "field {field}");
            assert_eq!(
                native_exec_config_digest(&tool.cfg).unwrap(),
                binding.config_digest,
                "the wire digest alone cannot establish this policy"
            );
            assert_eq!(
                capture_prepared_native_exec(&tool, &thread, &binding, &receipt).unwrap_err(),
                NativeExecCutError::Configuration(field),
                "producer must refuse {field} before encoding loses it"
            );
            assert_eq!(
                payload.validate(&binding, &expected_config),
                Err(NativeExecCutError::Configuration(field)),
                "consumer must refuse live skipped value {field}"
            );
        }
    }

    #[test]
    fn native_exec_expected_config_cannot_hide_unsupported_host_state() {
        for field in ["happens_before", "untraced_code_range"] {
            let (tool, mut thread, binding) = fixture();
            let receipt = validate_old_native_exec_cut(&tool, &thread, &binding).unwrap();
            prepare(&mut thread, &binding);
            let payload = decoded(&encoded(&tool, &thread, &binding, &receipt));
            let mut expected_config = tool.cfg.clone();
            set_skipped_config_field(&mut expected_config, field);
            assert_eq!(state_bytes(&expected_config), state_bytes(&tool.cfg));
            assert_eq!(
                payload.validate(&binding, &expected_config),
                Err(NativeExecCutError::Configuration(field)),
                "independent admission must not lose {field} through its digest"
            );
        }
    }

    #[test]
    fn native_exec_expected_config_keeps_host_only_verification_outputs() {
        let (mut tool, mut thread, mut binding) = fixture();
        tool.cfg.record_host_inputs = true;
        binding.config_digest = native_exec_config_digest(&tool.cfg).unwrap();
        let receipt = validate_old_native_exec_cut(&tool, &thread, &binding).unwrap();
        prepare(&mut thread, &binding);
        let payload = decoded(&encoded(&tool, &thread, &binding, &receipt));
        let mut expected_config = tool.cfg.clone();
        set_skipped_config_field(&mut expected_config, "host_input_log");
        set_skipped_config_field(&mut expected_config, "fatal_core_capture");
        let host_input_log = expected_config.host_input_log.clone();
        let fatal_core_capture = expected_config.fatal_core_capture.clone();
        assert_eq!(state_bytes(&expected_config), state_bytes(&tool.cfg));
        payload.validate(&binding, &expected_config).unwrap();
        assert_eq!(expected_config.host_input_log, host_input_log);
        assert_eq!(expected_config.fatal_core_capture, fatal_core_capture);
        assert!(payload.tool.cfg.host_input_log.is_none());
        assert!(payload.tool.cfg.fatal_core_capture.is_none());
        assert!(payload.tool.cfg.record_host_inputs);
    }

    #[test]
    fn native_exec_empty_happens_before_is_still_an_unsupported_skipped_value() {
        let (mut tool, thread, binding) = fixture();
        tool.cfg.happens_before = Some(
            detcore_model::happens_before::HappensBeforeSpec::from_dsl("")
                .unwrap()
                .normalize()
                .unwrap(),
        );
        assert_eq!(
            native_exec_config_digest(&tool.cfg).unwrap(),
            binding.config_digest
        );
        assert_eq!(
            validate_old_native_exec_cut(&tool, &thread, &binding).unwrap_err(),
            NativeExecCutError::Configuration("happens_before")
        );
    }

    fn set_skipped_config_field(config: &mut Config, field: &str) {
        match field {
            "happens_before" => {
                config.happens_before = Some(
                    detcore_model::happens_before::HappensBeforeSpec::from_dsl("42:sc=1 < 42:sc=2")
                        .unwrap()
                        .normalize()
                        .unwrap(),
                );
            }
            "host_input_log" => config.host_input_log = Some("native-inputs.jsonl".into()),
            "fatal_core_capture" => {
                config.fatal_core_capture = Some(detcore_model::config::FatalCoreCapture {
                    dir: "native-core-output".into(),
                    file_prefix: "native-exec-test-".into(),
                    max_core_bytes: 1024,
                    max_total_bytes: 4096,
                    time_limit: Duration::from_secs(1),
                });
            }
            "untraced_code_range" => config.untraced_code_range = Some((0x1000, 0x2000)),
            _ => panic!("unknown skipped Config field {field}"),
        }
    }

    #[test]
    fn native_exec_unsupported_configuration_remains_a_refusal() {
        for field in 0..10 {
            let (mut tool, thread, mut binding) = fixture();
            match field {
                0 => {
                    tool.cfg.no_rcb_time = false;
                    tool.cfg.max_timeslice = std::num::NonZeroU64::new(10);
                }
                1 => tool.cfg.max_timeslice = std::num::NonZeroU64::new(1),
                2 => tool.cfg.target_timeslice = std::num::NonZeroU64::new(1),
                3 => tool.cfg.chaos = true,
                4 => tool.cfg.imprecise_timers = true,
                5 => tool.cfg.record_preemptions = true,
                6 => tool.cfg.preemption_stacktrace = true,
                7 => tool.cfg.recordreplay_modes = true,
                8 => tool.cfg.replaying = true,
                9 => tool.cfg.replay_data = Some("unopened".into()),
                _ => unreachable!(),
            }
            binding.config_digest = native_exec_config_digest(&tool.cfg).unwrap();
            assert!(
                matches!(
                    validate_old_native_exec_cut(&tool, &thread, &binding),
                    Err(NativeExecCutError::Configuration(_))
                ),
                "field {field}"
            );
        }
    }

    #[test]
    fn native_initial_binding_keeps_lifecycle_unstarted_and_is_one_use() {
        // Model mechanics only: the production unsafe caller supplies the real
        // InitialImage permit and raw same-task IDs, neither simulated here.
        let owner = DetTid::from_raw(41);
        let mut thread = ThreadState::new(owner, &Config::default(), ());
        unsafe { bind_native_initial_physical_identity(&mut thread, 41, 41) }.unwrap();
        assert_eq!(thread.physical_tid, Some(41));
        assert_eq!(thread.detpid, None);
        assert!(!thread.thread_start_entered);
        assert_eq!(thread.mm_id, MmId::initial(owner));
        assert_eq!(
            thread.file_metadata.lock().unwrap().files_id,
            FilesId::initial(owner)
        );
        assert!(unsafe { bind_native_initial_physical_identity(&mut thread, 41, 41) }.is_err());
        for field in 0..6 {
            let mut thread = ThreadState::new(owner, &Config::default(), ());
            let (pid, tid) = match field {
                0 => (42, 41),
                1 => (42, 42),
                2 => (0, 0),
                3 => {
                    thread.detpid = Some(owner);
                    (41, 41)
                }
                4 => {
                    thread.thread_start_entered = true;
                    (41, 41)
                }
                5 => {
                    thread.mm_id = thread.mm_id.for_exec(owner);
                    (41, 41)
                }
                _ => unreachable!(),
            };
            assert!(
                unsafe { bind_native_initial_physical_identity(&mut thread, pid, tid) }.is_err(),
                "field {field}"
            );
            assert_eq!(thread.physical_tid, None);
        }
    }

    #[test]
    fn native_fresh_signal_binding_refuses_carried_or_actual_capability() {
        let owner = DetTid::from_raw(41);
        let identity = reverie::SignalTaskIdentity {
            process: reverie::SignalProcessId {
                tgid: reverie::Pid::from_raw(41),
                generation: 0,
            },
            tid: reverie::Pid::from_raw(41),
            task_generation: 0,
        };
        let mut thread = ThreadState::new(owner, &Config::default(), ());
        bind_native_fresh_signal_identity(
            &mut thread,
            reverie::BackendSignalControlMode::Unchanged,
            None,
        )
        .unwrap();
        assert_eq!(thread.signal_task_identity, None);
        assert!(
            bind_native_fresh_signal_identity(
                &mut thread,
                reverie::BackendSignalControlMode::Unchanged,
                Some(identity)
            )
            .is_err()
        );
        assert_eq!(thread.signal_task_identity, None);
        thread.signal_task_identity = Some(identity);
        assert!(
            bind_native_fresh_signal_identity(
                &mut thread,
                reverie::BackendSignalControlMode::Unchanged,
                None
            )
            .is_err()
        );
        assert_eq!(thread.signal_task_identity, Some(identity));
        thread.signal_task_identity = None;
        assert!(
            bind_native_fresh_signal_identity(
                &mut thread,
                reverie::BackendSignalControlMode::ToolControlled,
                None
            )
            .is_err()
        );
    }
}
