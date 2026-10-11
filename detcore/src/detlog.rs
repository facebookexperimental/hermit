/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Module contains macroses that help tracing DETLOG entires for the purpose of verifiying determinism
//! ['detlog'] can be used to write a deterministic log entry at INFO level
//! ['detlog_debug] can be use to write a deterministic log entry at DEBUG level

use std::fmt;
use std::os::fd::AsRawFd;
use std::os::fd::OwnedFd;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::AtomicI32;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use serde::Deserialize;
use serde::Serialize;

/// Delimits the machine-readable record appended to a human DETLOG message.
///
/// The human text remains available to people and historical readers. Current
/// verification consumes the JSON after this delimiter for event class and
/// position rather than recovering those facts from prose.
pub const RECORD_SEPARATOR: &str = " DETLOG_RECORD=";

/// Current schema written beside each structured DETLOG event.
pub const RECORD_SCHEMA: u32 = 1;

/// Producer-owned facts needed by log comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum DetLogEvent {
    /// A deterministic record outside the syscall-specific classes below.
    Other,
    /// A syscall entered but not yet completed.
    Syscall,
    /// A completed syscall, carrying Detcore's own counter.
    SyscallResult {
        /// Number of syscalls completed by guest threads at this point.
        finished_syscall_number: u64,
    },
    /// A scheduler turn committed to the guest.
    SchedulerCommit {
        /// Detcore's scheduler turn number.
        scheduler_turn: u64,
        /// Committed virtual time at this turn, in nanoseconds.
        virtual_nanoseconds: u64,
        /// Whether this turn is host-timing-sensitive internal I/O polling.
        internal_io_poll: bool,
        /// Whether this turn reads the guest runtime's `/proc/self/maps`.
        runtime_maps_read: bool,
    },
    /// Per-turn committed-time bookkeeping excluded from deterministic comparison.
    SchedulerCommittedTime,
    /// The scheduler found no runnable thread and took its established kick path.
    SchedulerEmptyQueueKick,
}

/// Versioned serialized form appended to the human log record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DetLogRecord {
    /// Serialized record schema.
    pub schema: u32,
    /// Closed event payload.
    pub event: DetLogEvent,
}

impl DetLogRecord {
    /// Construct a record in the current schema.
    pub fn new(event: DetLogEvent) -> Self {
        Self {
            schema: RECORD_SCHEMA,
            event,
        }
    }

    /// Split a human record from its producer-owned structured suffix.
    ///
    /// An absent suffix is the historical format. Once the delimiter is
    /// present, malformed JSON or another schema is an error rather than an
    /// invitation to fall back to the human text.
    pub fn split(message: &str) -> Result<(&str, Option<Self>), String> {
        let Some((human, encoded)) = message.rsplit_once(RECORD_SEPARATOR) else {
            return Ok((message, None));
        };
        let record: Self = serde_json::from_str(encoded)
            .map_err(|error| format!("malformed DETLOG record: {error}"))?;
        if record.schema != RECORD_SCHEMA {
            return Err(format!(
                "unsupported DETLOG record schema {}; expected {}",
                record.schema, RECORD_SCHEMA
            ));
        }
        Ok((human, Some(record)))
    }
}

/// Serialize one event for appending to its human log message.
#[doc(hidden)]
pub fn record_suffix(event: DetLogEvent) -> String {
    let encoded = serde_json::to_string(&DetLogRecord::new(event))
        .expect("DETLOG record serialization cannot fail");
    format!("{RECORD_SEPARATOR}{encoded}")
}

/// A process-local sink for deterministic INFO records: the emitting module's path (the
/// target `tracing` would give the record), the record suffix, the record's index in its
/// thread's current counted interval (1 for the first record after the thread's last
/// counted request; see [`take_forwarded_since_request`]), and the message.
pub type DetlogForwarder = for<'a> fn(&str, &str, u64, fmt::Arguments<'a>);

/// Which emitting modules a forwarded process may send DETLOG records from.
///
/// WHY THIS IS NEEDED. In-process, `detlog!` asks `tracing` whether INFO is
/// enabled at its own callsite, whose target is the emitting module's path, so
/// a target-scoped filter such as `RUST_LOG=warn,detcore::random=info` keeps
/// the `detcore::random` records and drops the rest. A tool running in another
/// process (SaBRe's guest plugin) has no subscriber to ask, so the coordinator
/// must hand it the same per-target answer. A single yes/no for the whole
/// `detcore` target either drops the scoped records or emits records the
/// coordinator's filter suppresses; both make the backends' logs disagree.
///
/// The policy holds the coordinator's answer at a default target that no
/// directive names and at every target a directive names. Its answer for a
/// record is the entry for the longest named target that is a string prefix of
/// the record's target, else the default. That is `tracing-subscriber`'s rule
/// for target directives: the most specific matching directive decides, and
/// any target a directive could match through is itself an entry here.
///
/// Limit: directives that select by span (`[syscall.intercept]=info`, or a
/// field filter with a value, which matches span fields) decide per record
/// from the spans the emitting thread has entered. Those spans exist only in
/// the process where the coordinator's subscriber runs (under ptrace,
/// reverie-ptrace enters `syscall.intercept` around each Detcore callback); a
/// forwarded process has none, so this per-target answer, taken once at
/// launch, cannot reproduce them. Target directives and field-presence
/// filters are reproduced exactly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForwardPolicy {
    default: bool,
    targets: Vec<(String, bool)>,
}

impl ForwardPolicy {
    /// Forward every record. This is the policy the historical encoding `1`
    /// denoted, and the one a coordinator logging all of Detcore at INFO sends.
    pub fn all() -> Self {
        Self {
            default: true,
            targets: Vec::new(),
        }
    }

    /// Ask the current `tracing` subscriber which targets it takes INFO
    /// records from.
    ///
    /// `directives` is the filter text the subscriber was built from (for the
    /// CLI, `RUST_LOG`). Only the target names are read from it; whether each is
    /// enabled is asked of the subscriber, so level overrides added after the
    /// text (`--log`) are honoured. Unparseable or extra names are harmless:
    /// probing a target no directive names returns that target's true answer.
    pub fn from_current_subscriber(directives: &str) -> Self {
        Self::from_probe(directives, info_enabled_for_target)
    }

    /// Build a policy from `directives`' target names and an INFO probe.
    pub fn from_probe(directives: &str, info_enabled: impl Fn(&str) -> bool) -> Self {
        let mut targets: Vec<(String, bool)> = Vec::new();
        for directive in directives.split(',') {
            let target = directive
                .split(['[', '='])
                .next()
                .unwrap_or_default()
                .trim();
            if target.is_empty() || targets.iter().any(|(named, _)| named == target) {
                continue;
            }
            targets.push((target.to_owned(), info_enabled(target)));
        }
        Self {
            // No directive target is empty, so only directives that name no
            // target (the default level) can match this one.
            default: info_enabled(""),
            targets,
        }
    }

    /// Whether any record could be forwarded under this policy.
    pub fn forwards_any(&self) -> bool {
        self.default || self.targets.iter().any(|(_, enabled)| *enabled)
    }

    /// Whether a record emitted by module `target` is forwarded.
    pub fn forwards(&self, target: &str) -> bool {
        self.targets
            .iter()
            .filter(|(named, _)| target.starts_with(named.as_str()))
            .max_by_key(|(named, _)| named.len())
            .map_or(self.default, |(_, enabled)| *enabled)
    }

    /// Encode for an environment variable: the default (`1` or `0`) followed by
    /// `,target=1` or `,target=0` per named target. Directive targets cannot
    /// contain `,` or `=`. `1` alone is the historical forward-everything value.
    pub fn encode(&self) -> String {
        let mut encoded = String::from(if self.default { "1" } else { "0" });
        for (target, enabled) in &self.targets {
            encoded.push(',');
            encoded.push_str(target);
            encoded.push_str(if *enabled { "=1" } else { "=0" });
        }
        encoded
    }

    /// Decode [`ForwardPolicy::encode`]'s output, refusing anything else.
    pub fn decode(encoded: &str) -> Result<Self, String> {
        fn flag(text: &str) -> Option<bool> {
            match text {
                "1" => Some(true),
                "0" => Some(false),
                _ => None,
            }
        }
        let mut entries = encoded.split(',');
        let default = entries
            .next()
            .and_then(flag)
            .ok_or_else(|| format!("DETLOG forwarding policy {encoded:?} lacks a 0/1 default"))?;
        let mut targets = Vec::new();
        for entry in entries {
            let parsed = entry
                .split_once('=')
                .filter(|(target, _)| !target.is_empty())
                .and_then(|(target, enabled)| Some((target.to_owned(), flag(enabled)?)));
            let Some(parsed) = parsed else {
                return Err(format!(
                    "DETLOG forwarding policy {encoded:?} has malformed entry {entry:?}"
                ));
            };
            targets.push(parsed);
        }
        Ok(Self { default, targets })
    }
}

/// Whether the current subscriber would log a `detlog!` record emitted in the
/// module `target`. In-process that record passes two checks: `detlog!`'s own
/// `tracing::enabled!(INFO)`, a hint with no fields, and then the
/// `tracing::info!` event itself, which carries one field, the message. A
/// directive with a field filter (`detcore::random[{name}]=warn`) applies to
/// the hint whatever its fields but to the event only when the event has that
/// field, so the two checks can disagree; the record is logged only when both
/// pass, and so this answers true only when both probes do.
pub fn info_enabled_for_target(target: &str) -> bool {
    use tracing::Metadata;
    use tracing::callsite::DefaultCallsite;
    use tracing::field::FieldSet;
    use tracing::level_filters::LevelFilter;
    use tracing::metadata::Kind;

    // The hint `tracing::enabled!` builds: no fields.
    static HINT_CALLSITE: DefaultCallsite = DefaultCallsite::new(&HINT_METADATA);
    static HINT_METADATA: Metadata<'static> = Metadata::new(
        "detlog forwarding hint probe",
        "detcore::detlog",
        tracing::Level::INFO,
        None,
        None,
        None,
        FieldSet::new(&[], tracing::callsite::Identifier(&HINT_CALLSITE)),
        Kind::HINT,
    );
    // `detlog!` events carry exactly one field, the formatted message.
    static PROBE_CALLSITE: DefaultCallsite = DefaultCallsite::new(&PROBE_METADATA);
    static PROBE_METADATA: Metadata<'static> = Metadata::new(
        "detlog forwarding probe",
        "detcore::detlog",
        tracing::Level::INFO,
        None,
        None,
        None,
        FieldSet::new(&["message"], tracing::callsite::Identifier(&PROBE_CALLSITE)),
        Kind::EVENT,
    );

    if tracing::level_filters::STATIC_MAX_LEVEL < LevelFilter::INFO
        || LevelFilter::current() < LevelFilter::INFO
    {
        return false;
    }
    let hint = Metadata::new(
        "detlog forwarding hint probe",
        target,
        tracing::Level::INFO,
        None,
        None,
        Some(target),
        FieldSet::new(&[], tracing::callsite::Identifier(&HINT_CALLSITE)),
        Kind::HINT,
    );
    let event = Metadata::new(
        "detlog forwarding probe",
        target,
        tracing::Level::INFO,
        None,
        None,
        Some(target),
        FieldSet::new(&["message"], tracing::callsite::Identifier(&PROBE_CALLSITE)),
        Kind::EVENT,
    );
    tracing::dispatcher::get_default(|dispatch| dispatch.enabled(&hint) && dispatch.enabled(&event))
}

static FORWARDER: OnceLock<(DetlogForwarder, ForwardPolicy)> = OnceLock::new();

/// Installs a process-local sink for deterministic INFO records.
///
/// Backends whose tool runs in another process can use this to transport the
/// same records that are normally observed through the coordinator's tracing
/// subscriber. `policy` is the coordinator's per-target answer; records from
/// other modules are not forwarded. Only the first sink installed in a process
/// is retained.
pub fn set_forwarder(
    forwarder: DetlogForwarder,
    policy: ForwardPolicy,
) -> Result<(), (DetlogForwarder, ForwardPolicy)> {
    FORWARDER.set((forwarder, policy))
}

/// The line a forwarded record travels as: the record as the coordinator's tracing
/// subscriber would print it, naming the module that emitted it (a forwarded
/// `detcore::tool_local` record must not read as `detcore` and differ from the same
/// record logged through tracing), then a newline.
#[doc(hidden)]
pub fn forwarded_line(target: &str, record_suffix: &str, message: fmt::Arguments<'_>) -> Vec<u8> {
    format!("INFO {target}: DETLOG {message}{record_suffix}\n").into_bytes()
}

/// Calls `operation` and restores the calling thread's `errno` afterwards. An
/// in-guest tool runs on the guest's own thread, so a failed forwarding write must
/// not change what the guest reads there.
fn preserving_errno(operation: impl FnOnce()) {
    // SAFETY: `__errno_location` returns the calling thread's live errno slot.
    let errno = unsafe { libc::__errno_location() };
    // SAFETY: as above; the slot is valid for the life of the thread.
    let saved = unsafe { *errno };
    operation();
    // SAFETY: as above.
    unsafe { *errno = saved };
}

/// A [`DetlogForwarder`] for a tool that runs inside the guest process and shares the
/// guest's standard error: it writes each record as one [`forwarded_line`] there, where
/// Hermit's verification separates the records from the guest's own output by their
/// text.
///
/// It uses one raw `write` per record, so records from several guest threads cannot
/// interleave, and it avoids `tracing`'s thread-local dispatcher, which libc's final
/// `exit_group` can reach after Rust thread-local destruction has begun. A write error
/// drops the record. The calling thread's `errno` is preserved.
pub fn forward_to_stderr(
    target: &str,
    record_suffix: &str,
    _index: u64,
    message: fmt::Arguments<'_>,
) {
    preserving_errno(|| {
        let line = forwarded_line(target, record_suffix, message);
        let mut rest = line.as_slice();
        while !rest.is_empty() {
            // SAFETY: `rest` is a live, initialized byte slice of the given length.
            let written = unsafe {
                libc::write(
                    libc::STDERR_FILENO,
                    rest.as_ptr().cast::<libc::c_void>(),
                    rest.len(),
                )
            };
            if written > 0 {
                rest = &rest[written as usize..];
            } else if written == 0
                || std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted
            {
                return;
            }
        }
    });
}

/// Starts the line [`send_forwarded_record`] sends in place of a record it could not
/// deliver, followed by the record's length and the error number. Its presence means
/// the forwarded records are incomplete, so verification must not compare them.
pub const FORWARDING_LOSS_NOTICE: &str = "HERMIT_DETLOG_RECORD_LOST";

/// The message an in-guest runtime sends on the forwarding socket when it has to
/// give the socket up (reverie-liteinst's `reserve_tool_output_fd` retirement
/// message): a [`FORWARDING_LOSS_NOTICE`], since no later record can arrive.
pub const FORWARDING_RETIRED_NOTICE: &[u8] = b"HERMIT_DETLOG_RECORD_LOST 0 0 socket retired\n";

/// Losses whose notice could not be sent either; the next message that gets through
/// is preceded by a notice for them, so a later record cannot hide them.
static UNREPORTED_LOSSES: AtomicU64 = AtomicU64::new(0);

/// Sends one record as one [`forwarded_line`] message on `socket`, a message-oriented
/// socket (such as one end of a `SOCK_SEQPACKET` pair) that carries nothing but records,
/// from which Hermit's verification moves them into the run's log.
///
/// It uses one raw `send` with `MSG_NOSIGNAL`, so a socket whose reader has gone never
/// raises SIGPIPE in the guest. A record that cannot be sent (too large for the socket,
/// or any other error) is replaced by a short [`FORWARDING_LOSS_NOTICE`] message, so the
/// loss is visible rather than silent; if that notice cannot be sent either, a notice
/// precedes the next message that can. The calling thread's `errno` is preserved.
pub fn send_forwarded_record(
    socket: libc::c_int,
    target: &str,
    record_suffix: &str,
    index: u64,
    message: fmt::Arguments<'_>,
) {
    let send = |bytes: &[u8]| loop {
        // SAFETY: `bytes` is a live, initialized byte slice of the given length.
        let sent = unsafe {
            libc::send(
                socket,
                bytes.as_ptr().cast::<libc::c_void>(),
                bytes.len(),
                libc::MSG_NOSIGNAL,
            )
        };
        if sent >= 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(error);
        }
    };
    let notice = |length: usize, errno: i32| format!("{FORWARDING_LOSS_NOTICE} {length} {errno}\n");
    preserving_errno(|| {
        let unreported = UNREPORTED_LOSSES.swap(0, Ordering::AcqRel);
        if unreported > 0 && send(notice(0, 0).as_bytes()).is_err() {
            UNREPORTED_LOSSES.fetch_add(unreported, Ordering::AcqRel);
        }
        let line = tagged_forwarded_message(
            current_tid(),
            index,
            &forwarded_line(target, record_suffix, message),
        );
        if let Err(error) = send(&line) {
            let errno = error.raw_os_error().unwrap_or(0);
            if send(notice(line.len(), errno).as_bytes()).is_err() {
                UNREPORTED_LOSSES.fetch_add(1, Ordering::AcqRel);
            }
        }
    });
}

/// The socket `fd` names, as (`st_dev`, `st_ino`); `None` when it names no
/// socket.
pub fn socket_identity(fd: libc::c_int) -> Option<(u64, u64)> {
    // SAFETY: fstat writes only into `metadata`.
    // The raw syscall: this runs inside the guest under in-guest LiteInst
    // (see crate::util::raw_syscall).
    let mut metadata: libc::stat = unsafe { std::mem::zeroed() };
    let status = unsafe {
        crate::util::raw_syscall(
            libc::SYS_fstat,
            [fd as u64, (&raw mut metadata) as u64, 0, 0, 0, 0],
        )
    };
    (status == 0 && metadata.st_mode & libc::S_IFMT == libc::S_IFSOCK)
        .then_some((metadata.st_dev, metadata.st_ino))
}

/// The value Hermit passes an in-guest Tool for its output socket `fd`
/// (`reverie_sabre::TOOL_OUTPUT_ENV`, `detcore_liteinst::DETLOG_FORWARD_ENV`):
/// `<fd>:<st_dev>:<st_ino>`, the identity in 16 hexadecimal digits each. Guest
/// code can run before the Tool adopts the descriptor (an executable's
/// `.preinit_array`) and close it or put a socket of its own at that number;
/// the Tool adopts it only if it is still this socket
/// ([`tool_output_env_target`]), so its records never reach the guest's socket.
/// The fixed width keeps the value's length, which a scrubbed environment block
/// keeps, independent of the host.
pub fn tool_output_env_value(fd: libc::c_int) -> std::io::Result<String> {
    let (dev, ino) = socket_identity(fd).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "a Tool output descriptor must be a socket",
        )
    })?;
    Ok(format!("{fd}:{dev:016x}:{ino:016x}"))
}

/// The descriptor and socket identity a [`tool_output_env_value`] names.
pub fn parse_tool_output_env_value(value: &str) -> Option<(libc::c_int, (u64, u64))> {
    let mut fields = value.split(':');
    let fd = fields.next()?.parse::<libc::c_int>().ok()?;
    let hex = |field: Option<&str>| {
        field
            .filter(|field| field.len() == 16)
            .and_then(|field| u64::from_str_radix(field, 16).ok())
    };
    let identity = (hex(fields.next())?, hex(fields.next())?);
    fields.next().is_none().then_some((fd, identity))
}

/// The descriptor a [`tool_output_env_value`] names, if it still names that
/// socket in this process.
pub fn tool_output_env_target(value: &str) -> Option<libc::c_int> {
    let (fd, identity) = parse_tool_output_env_value(value)?;
    (socket_identity(fd) == Some(identity)).then_some(fd)
}

/// The calling thread's kernel id, for [`tagged_forwarded_message`].
fn current_tid() -> i32 {
    // SAFETY: gettid has no arguments and cannot fail.
    unsafe { libc::syscall(libc::SYS_gettid) as i32 }
}

/// Prefixes a forwarded record with the id of the guest thread that sent it and
/// the record's index in that thread's counted interval, `T<tid>.<index> `, so
/// the coordinator can write each thread's records at that thread's own
/// scheduler turn ([`write_forwarded_for`]) and check that it holds exactly the
/// records the thread produced, in order ([`check_forwarded_count`]). The
/// coordinator strips the tag before the record reaches the log.
pub fn tagged_forwarded_message(tid: i32, index: u64, line: &[u8]) -> Vec<u8> {
    let mut message = format!("T{tid}.{index} ").into_bytes();
    message.extend_from_slice(line);
    message
}

/// The sender's thread id, the record's index and the record, for a message
/// tagged by [`tagged_forwarded_message`]; `None` for an untagged message (a
/// [`FORWARDING_LOSS_NOTICE`] or [`FORWARDING_RETIRED_NOTICE`] sent raw, or a
/// record from a runtime older than the tag).
pub fn split_forwarded_message(message: &[u8]) -> Option<(i32, u64, &[u8])> {
    let rest = message.strip_prefix(b"T")?;
    let space = rest.iter().position(|&byte| byte == b' ')?;
    let tag = std::str::from_utf8(&rest[..space]).ok()?;
    let (tid, index) = tag.split_once('.')?;
    let decimal = |digits: &str| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit());
    if !decimal(tid) || !decimal(index) {
        return None;
    }
    Some((tid.parse().ok()?, index.parse().ok()?, &rest[space + 1..]))
}

/// Receives each record the coordinator writes from a forwarding socket: a
/// [`forwarded_line`] (its sender tag removed), or a [`FORWARDING_LOSS_NOTICE`].
pub type ForwardedRecordSink = Box<dyn FnMut(&[u8]) + Send>;

/// Where a thread's forwarded records may be written at its request
/// ([`ForwardedOrder::at_request`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Runner {
    /// No turn has been committed yet and no request has arrived.
    NoTurnYet,
    /// No turn has been committed yet; this thread sent the first request, so it
    /// is the root.
    Root(i32),
    /// The thread of the last ordinary COMMIT, whose request ends that turn.
    Exclusive(i32),
    /// The last COMMIT was a BACKGROUND one: no request is a fixed point.
    None,
}

/// The order rule's state ([`set_forwarded_source`]): the forwarded records taken
/// from the socket but not yet written, by sending thread, and which thread's
/// request is a point the schedule fixes. It decides what to write; the caller
/// writes it.
#[derive(Debug)]
pub(crate) struct ForwardedOrder {
    pending: std::collections::BTreeMap<i32, Vec<Vec<u8>>>,
    runner: Runner,
    /// A second root asked before any turn ([`ForwardedOrder::at_request`]).
    ambiguous: bool,
    /// An untagged ordinary record arrived ([`ForwardedOrder::untagged`]).
    legacy: bool,
    /// Records received from each thread since its last counted request
    /// ([`ForwardedOrder::received_since_check`]).
    received: std::collections::BTreeMap<i32, ReceivedInterval>,
}

/// What arrived from one thread since its last counted request.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ReceivedInterval {
    /// How many records arrived.
    pub(crate) count: u64,
    /// The first record whose index was not the next one: `(expected, sent)`.
    pub(crate) out_of_sequence: Option<(u64, u64)>,
}

/// The [`FORWARDING_LOSS_NOTICE`] the coordinator writes when a forwarded record
/// arrives without a sender tag (a guest runtime older than the tag), so its place
/// in the log would be the host's choice.
pub const FORWARDING_UNTAGGED_RECORD_NOTICE: &[u8] =
    b"HERMIT_DETLOG_RECORD_LOST 0 0 untagged forwarded record from an older runtime\n";

/// The [`FORWARDING_LOSS_NOTICE`] the coordinator writes when more than one guest
/// thread sends a request before the first turn, so the forwarded records' order
/// would depend on the host.
pub const FORWARDING_AMBIGUOUS_ORDER_NOTICE: &[u8] =
    b"HERMIT_DETLOG_RECORD_LOST 0 0 forwarded order ambiguous: more than one root\n";

impl ForwardedOrder {
    pub(crate) fn new() -> Self {
        Self {
            pending: Default::default(),
            runner: Runner::NoTurnYet,
            ambiguous: false,
            legacy: false,
            received: Default::default(),
        }
    }

    /// Queues a record thread `tid` sent.
    pub(crate) fn queue(&mut self, tid: i32, record: Vec<u8>) {
        self.pending.entry(tid).or_default().push(record);
    }

    /// Notes that the record with `index` in thread `tid`'s counted interval
    /// arrived. A thread sends its records in order on one message socket, so
    /// they arrive as 1, 2, 3, ...; the first index that is not the next one is
    /// kept.
    pub(crate) fn arrived(&mut self, tid: i32, index: u64) {
        let interval = self.received.entry(tid).or_default();
        interval.count += 1;
        if index != interval.count && interval.out_of_sequence.is_none() {
            interval.out_of_sequence = Some((interval.count, index));
        }
    }

    /// What arrived from thread `tid` since its last counted request, and
    /// starts its next interval.
    pub(crate) fn received_since_check(&mut self, tid: i32) -> ReceivedInterval {
        self.received.remove(&tid).unwrap_or_default()
    }

    /// The records to write when a request from `tid` arrives: `tid`'s queue if its
    /// request is a point the schedule fixes, otherwise none.
    pub(crate) fn at_request(&mut self, tid: i32) -> Vec<Vec<u8>> {
        match self.runner {
            Runner::NoTurnYet => {
                self.runner = Runner::Root(tid);
                self.pending.remove(&tid).unwrap_or_default()
            }
            Runner::Root(root) | Runner::Exclusive(root) if root == tid => {
                self.pending.remove(&tid).unwrap_or_default()
            }
            Runner::Root(_) if !self.ambiguous => {
                // A second thread asks before any turn: there is more than one root
                // (for example a preload constructor that forked before the runtime
                // was installed), so which root's records come first is decided by
                // the host. Say so instead of writing a host-timed order: the loss
                // notice makes verification refuse to compare the records.
                self.ambiguous = true;
                vec![FORWARDING_AMBIGUOUS_ORDER_NOTICE.to_vec()]
            }
            _ => Vec::new(),
        }
    }

    /// The records to write immediately before the scheduler commits a turn of
    /// `tid` (all of `tid`'s queue); `exclusive` is false for a BACKGROUND turn.
    pub(crate) fn at_commit(&mut self, tid: i32, exclusive: bool) -> Vec<Vec<u8>> {
        self.runner = if exclusive {
            Runner::Exclusive(tid)
        } else {
            Runner::None
        };
        self.pending.remove(&tid).unwrap_or_default()
    }

    /// What to write for a message without a sender tag, at once: a loss notice the
    /// runtime sent as is; for an ordinary record (from a runtime older than the
    /// tag), the record preceded, the first time, by
    /// [`FORWARDING_UNTAGGED_RECORD_NOTICE`], so verification refuses to compare
    /// records whose place the host chose.
    pub(crate) fn untagged(&mut self, message: &[u8]) -> Vec<Vec<u8>> {
        if message.starts_with(FORWARDING_LOSS_NOTICE.as_bytes()) {
            return vec![message.to_vec()];
        }
        let mut out = Vec::new();
        if !self.legacy {
            self.legacy = true;
            out.push(FORWARDING_UNTAGGED_RECORD_NOTICE.to_vec());
        }
        out.push(message.to_vec());
        out
    }

    /// Everything still queued, thread by thread in thread-id order.
    pub(crate) fn finish(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.pending)
            .into_values()
            .flatten()
            .collect()
    }
}

/// The receiving end of the socket an in-guest Tool sends its records on, where
/// the coordinator writes them, and the order rule's state ([`set_forwarded_source`]).
struct ForwardedSource {
    socket: OwnedFd,
    sink: ForwardedRecordSink,
    order: ForwardedOrder,
}

static FORWARDED_SOURCE: Mutex<Option<ForwardedSource>> = Mutex::new(None);

/// The target of the evidence record that reports this process's
/// [`determinism_loss`] where the verifier cannot read it: under the DBT
/// backend Detcore runs in the guest's DynamoRIO client, so the client writes
/// the reason into its protected evidence stream at process exit, and hermit's
/// DBT adapter refuses the comparison when a run's evidence holds one.
pub const DETERMINISM_LOSS_RECORD_TARGET: &str = "detcore::determinism_loss";

/// Counts [`set_forwarded_source`] and [`clear_forwarded_source`] calls, so the
/// receiver knows when the source it serves was replaced or cleared.
static FORWARDED_GENERATION: AtomicU64 = AtomicU64::new(0);
/// The generation whose receiver has been started ([`start_forwarded_receiver`]).
static RECEIVER_GENERATION: AtomicU64 = AtomicU64::new(0);
/// Stops the running receiver task: sent (or dropped) when its source is
/// replaced or cleared.
static RECEIVER_STOP: Mutex<Option<tokio::sync::oneshot::Sender<()>>> = Mutex::new(None);

/// The first determinism loss recorded in this process, never cleared: a run
/// that recorded one must not be compared, whatever reached its log (see
/// [`determinism_loss`]).
static DETERMINISM_LOSS: Mutex<Option<String>> = Mutex::new(None);

/// Records that this run's records can no longer be trusted to compare: a
/// record was lost, or an event happened at a moment the host chose. The first
/// reason is kept. Verification refuses to compare a run whose
/// [`determinism_loss`] is set, independently of whether the corresponding
/// loss notice reached the log.
pub fn record_determinism_loss(reason: &str) {
    let mut loss = DETERMINISM_LOSS.lock().unwrap();
    if loss.is_none() {
        *loss = Some(reason.to_owned());
    }
}

static REPLAY_REFUSAL: Mutex<Option<String>> = Mutex::new(None);

/// Records that a recording made by this process cannot be replayed
/// faithfully, for the recorder to persist in its metadata (`replay_refused`),
/// so that replay refuses before it starts the guest. The first reason is
/// kept. It also records a determinism loss.
pub fn record_replay_refusal(reason: &str) {
    write_loss_notice(reason);
    let mut refusal = REPLAY_REFUSAL.lock().unwrap();
    if refusal.is_none() {
        *refusal = Some(reason.to_owned());
    }
}

/// The first replay refusal [`record_replay_refusal`] recorded in this
/// process, if any.
pub fn replay_refusal() -> Option<String> {
    REPLAY_REFUSAL.lock().unwrap().clone()
}

/// The first determinism loss [`record_determinism_loss`] recorded in this
/// process, if any.
pub fn determinism_loss() -> Option<String> {
    DETERMINISM_LOSS.lock().unwrap().clone()
}

/// Writes a [`FORWARDING_LOSS_NOTICE`] line with `reason` through the
/// registered forwarding sink, the unfiltered raw path, and records the
/// determinism loss. Without a registered source the loss is still recorded.
pub fn write_loss_notice(reason: &str) {
    let line = format!("{FORWARDING_LOSS_NOTICE} 0 0 {reason}\n");
    record_determinism_loss(reason);
    if let Some(source) = FORWARDED_SOURCE.lock().unwrap().as_mut() {
        (source.sink)(line.as_bytes());
    }
}

/// Writes one forwarded record through `source`'s sink, recording a
/// determinism loss when the record is a loss notice.
fn emit(source: &mut ForwardedSource, record: &[u8]) {
    if record.starts_with(FORWARDING_LOSS_NOTICE.as_bytes()) {
        record_determinism_loss(&String::from_utf8_lossy(record));
    }
    (source.sink)(record);
}

/// Registers the receiving end of the socket an in-guest Tool sends its records on
/// ([`send_forwarded_record`]) and the sink that writes them to the run's log.
///
/// THE ORDER RULE, one for every backend whose Tool runs in the guest. Each record
/// carries its sending thread ([`tagged_forwarded_message`]). Before it handles any
/// guest request the coordinator takes the records waiting on the socket into a
/// queue per sending thread ([`drain_forwarded_at_request`] at the top of the
/// request handler). A thread's queued records are written into the log:
/// - when that thread's own request arrives, if it is the thread the scheduler last
///   committed an ordinary turn to, or, before any turn, the thread whose request
///   arrives first (the root; every other thread is created during some turn). That
///   thread's request is the event that ends the turn the schedule gave it, so the
///   request arrives at a point the schedule fixes, and its records land where a
///   single-process (ptrace) run writes them, ahead of everything handling that
///   request writes. (A thread still in background IO from an earlier BACKGROUND
///   turn may run at the same time; its records wait for its own COMMIT, by the next
///   rule. Coordinator lines its continuation writes below INFO verbosity, such as
///   TRACE, are not ordered by this rule; the INFO DETLOG and COMMIT lines that
///   verification compares are.)
/// - otherwise immediately before the scheduler commits that thread's next turn
///   ([`write_forwarded_for`], at both kinds of COMMIT): a thread that runs without
///   holding the turn (a forked child doing its setup, a thread in background IO)
///   sends its records while other threads' turns are being handled, so only its
///   own next COMMIT is a point the schedule fixes;
/// - and, for any still queued when the run ends, then, thread by thread in
///   thread-id order.
///
/// If a second thread sends a request before the first turn, there is more than one
/// root (a preload constructor that forked before the runtime was installed, say),
/// and which root's records come first would be the host's choice. The coordinator
/// then writes [`FORWARDING_AMBIGUOUS_ORDER_NOTICE`], so verification refuses to
/// compare the records rather than compare a host-timed order.
///
/// A `send` on a message socket queues the record on the receiver before it returns,
/// so a guest thread's records sent before a request are queued by the time that
/// request is handled, and so before the turn it leads to is committed. Writing every
/// thread's records at any request's arrival instead put a record wherever the host
/// happened to deliver it: a forked child's records, sent while its parent's next
/// request was being handled, landed before or after the scheduler's records for that
/// request depending on timing.
///
/// The records are TAKEN off the socket continuously, by a receiver task that
/// queues each one for its thread as soon as it arrives, so a guest never
/// blocks on a full socket buffer waiting for a request the coordinator would
/// drain at: a single `writev` of 1024 buffers sends 1024 records before its
/// thread's next request. Taking a record only queues it; where it is written
/// is still decided by the rule above. (A message without a sender tag is
/// written as it is taken, which verification refuses to compare anyway: see
/// `ForwardedOrder::untagged`.) The receiver is a task on the coordinator's
/// tokio runtime, started at the first request (`start_forwarded_receiver`),
/// not a thread of its own: inside Hermit's PID namespace a coordinator thread
/// takes an ID from the same allocator as the guest's tasks, and one more
/// thread shifted every guest ID by one (the root became DetPid 4).
pub fn set_forwarded_source(socket: OwnedFd, sink: ForwardedRecordSink) {
    let mut source = FORWARDED_SOURCE.lock().unwrap();
    FORWARDED_GENERATION.fetch_add(1, Ordering::SeqCst);
    stop_forwarded_receiver();
    *source = Some(ForwardedSource {
        socket,
        sink,
        order: ForwardedOrder::new(),
    });
}

/// Starts the receiver task of the registered source on the current tokio
/// runtime, once per [`set_forwarded_source`]. Called from the coordinator's
/// request handling, which runs on that runtime; outside one it does nothing,
/// and the request-time drains still take every record.
fn start_forwarded_receiver() {
    let generation = FORWARDED_GENERATION.load(Ordering::SeqCst);
    if RECEIVER_GENERATION.load(Ordering::SeqCst) == generation {
        return;
    }
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        return;
    };
    // The task watches its own duplicate of the socket, so closing the
    // source's descriptor (and the number's reuse) never touches the task's
    // registration.
    let watched = match FORWARDED_SOURCE.lock().unwrap().as_ref() {
        Some(source) => match source.socket.try_clone() {
            Ok(watched) => watched,
            Err(_) => return,
        },
        None => return,
    };
    if RECEIVER_GENERATION.swap(generation, Ordering::SeqCst) == generation {
        return;
    }
    let (stop, stopped) = tokio::sync::oneshot::channel();
    stop_forwarded_receiver();
    *RECEIVER_STOP.lock().unwrap() = Some(stop);
    runtime.spawn(receive_forwarded(watched, generation, stopped));
}

/// Stops the running receiver task, if any.
fn stop_forwarded_receiver() {
    if let Some(stop) = RECEIVER_STOP.lock().unwrap().take() {
        let _ = stop.send(());
    }
}

/// The receiver task of [`start_forwarded_receiver`]: waits until `watched`
/// (its own duplicate of the registered socket) is readable and takes the
/// socket's messages into their threads' queues, until the source of
/// `generation` is replaced or cleared, which also sends `stopped`. Readiness
/// is only a hint; the source lock and the generation decide whether the
/// registered socket is still the one it serves.
async fn receive_forwarded(
    watched: OwnedFd,
    generation: u64,
    mut stopped: tokio::sync::oneshot::Receiver<()>,
) {
    let Ok(readiness) =
        tokio::io::unix::AsyncFd::with_interest(watched, tokio::io::Interest::READABLE)
    else {
        // The next request starts it again; until then request-time drains
        // still take every record.
        let _ =
            RECEIVER_GENERATION.compare_exchange(generation, 0, Ordering::SeqCst, Ordering::SeqCst);
        return;
    };
    loop {
        let ready = tokio::select! {
            ready = readiness.readable() => ready,
            _ = &mut stopped => return,
        };
        let Ok(mut ready) = ready else {
            let _ = RECEIVER_GENERATION.compare_exchange(
                generation,
                0,
                Ordering::SeqCst,
                Ordering::SeqCst,
            );
            return;
        };
        {
            let mut source = FORWARDED_SOURCE.lock().unwrap();
            if FORWARDED_GENERATION.load(Ordering::SeqCst) != generation {
                return;
            }
            match source.as_mut() {
                Some(source) => collect_forwarded(source),
                None => return,
            }
        }
        // collect_forwarded read until the socket had nothing waiting.
        ready.clear_ready();
    }
}

/// Unregisters the source set with [`set_forwarded_source`], after taking the last
/// waiting records and writing every queued record, thread by thread in thread-id
/// order, and closes its socket.
pub fn clear_forwarded_source() {
    let mut source = FORWARDED_SOURCE.lock().unwrap();
    FORWARDED_GENERATION.fetch_add(1, Ordering::SeqCst);
    stop_forwarded_receiver();
    if let Some(source) = source.as_mut() {
        collect_forwarded(source);
        let records = source.order.finish();
        write_records(source, records);
    }
    source.take();
}

/// Takes every message waiting on the registered forwarding socket into the queue of
/// its sending thread, without blocking and without writing it. An untagged message is
/// written at once as `ForwardedOrder::untagged` says. Without a registered source it
/// does nothing.
pub fn drain_forwarded() {
    if let Some(source) = FORWARDED_SOURCE.lock().unwrap().as_mut() {
        collect_forwarded(source);
    }
}

/// Takes waiting messages as [`drain_forwarded`] does, then writes every queued record
/// thread `tid` sent, in the order it sent them. The scheduler calls this immediately
/// before it commits a turn of `tid`; `exclusive` is false for a BACKGROUND turn,
/// which runs alongside later turns, and true for an ordinary one.
pub fn write_forwarded_for(tid: i32, exclusive: bool) {
    start_forwarded_receiver();
    if let Some(source) = FORWARDED_SOURCE.lock().unwrap().as_mut() {
        collect_forwarded(source);
        let records = source.order.at_commit(tid, exclusive);
        write_records(source, records);
    }
}

/// Takes waiting messages as [`drain_forwarded`] does at the top of the handler of
/// a request from thread `tid`, and writes `tid`'s queued records now when its
/// request is a point the schedule fixes (the order rule at [`set_forwarded_source`]).
pub fn drain_forwarded_at_request(tid: i32) {
    start_forwarded_receiver();
    if let Some(source) = FORWARDED_SOURCE.lock().unwrap().as_mut() {
        collect_forwarded(source);
        let records = source.order.at_request(tid);
        write_records(source, records);
    }
}

/// The COMPLETENESS RULE. A counted request from guest thread `tid` says it
/// produced `produced` records for forwarding since its previous counted request
/// ([`take_forwarded_since_request`]); it sent them before this request, and a
/// `send` on a message socket queues a record on the receiver before it returns,
/// so all of them have been received by now, as indexes 1 to `produced` in
/// order. Anything else means the records the coordinator holds for that
/// thread are not the ones it produced: fewer, or a skipped index, were lost on
/// the way (the guest disturbed the socket, the Tool could not adopt it, or a
/// stale descriptor was refused); more, or a repeated or earlier index, came
/// from something other than the Tool. Either way the coordinator writes a loss
/// notice and records the determinism loss, so verification refuses to compare
/// the run. [`UNCOUNTED`] (a thread could not be counted) is refused the same
/// way.
/// Without a registered source it does nothing.
pub fn check_forwarded_count(tid: i32, produced: u64) {
    if let Some(source) = FORWARDED_SOURCE.lock().unwrap().as_mut() {
        collect_forwarded(source);
        let ReceivedInterval {
            count: received,
            out_of_sequence,
        } = source.order.received_since_check(tid);
        if produced == UNCOUNTED || out_of_sequence.is_some() || received != produced {
            let reason = if produced == UNCOUNTED {
                format!(
                    "forwarded records uncounted: a guest thread's records could not be counted (reported by thread {tid})"
                )
            } else if let Some((expected, sent)) = out_of_sequence {
                if sent > expected {
                    format!(
                        "forwarded records lost: thread {tid} sent record {sent} of its interval where {expected} was next"
                    )
                } else {
                    format!(
                        "forwarded records not the Tool's: thread {tid} sent record {sent} of its interval where {expected} was next"
                    )
                }
            } else if received < produced {
                format!(
                    "forwarded records lost: thread {tid} produced {produced} since its last counted request and {received} arrived"
                )
            } else {
                format!(
                    "forwarded records not the Tool's: thread {tid} produced {produced} since its last counted request and {received} arrived"
                )
            };
            record_determinism_loss(&reason);
            let line = format!("{FORWARDING_LOSS_NOTICE} 0 0 {reason}\n");
            (source.sink)(line.as_bytes());
        }
    }
}

fn write_records(source: &mut ForwardedSource, records: Vec<Vec<u8>>) {
    for record in records {
        emit(source, &record);
    }
}

fn collect_forwarded(source: &mut ForwardedSource) {
    let fd = source.socket.as_raw_fd();
    let mut buffer = Vec::new();
    loop {
        // The size of the next message, without taking it (a message socket reports
        // the whole message's length with MSG_TRUNC).
        // SAFETY: a zero-length peek writes nothing.
        let size = unsafe {
            libc::recv(
                fd,
                std::ptr::null_mut(),
                0,
                libc::MSG_PEEK | libc::MSG_TRUNC | libc::MSG_DONTWAIT,
            )
        };
        if size < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
            continue;
        }
        if size <= 0 {
            // Nothing waiting (EAGAIN), the senders are gone (0), or an error: the
            // next drain tries again, and the end of the run drains the rest.
            return;
        }
        buffer.resize(size as usize, 0);
        // SAFETY: `buffer` has room for `size` bytes.
        let received = unsafe {
            libc::recv(
                fd,
                buffer.as_mut_ptr().cast::<libc::c_void>(),
                buffer.len(),
                libc::MSG_DONTWAIT,
            )
        };
        if received <= 0 {
            return;
        }
        let message = &buffer[..received as usize];
        match split_forwarded_message(message) {
            Some((tid, index, record)) => {
                source.order.arrived(tid, index);
                source.order.queue(tid, record.to_vec());
            }
            None => {
                for record in source.order.untagged(message) {
                    emit(source, &record);
                }
            }
        }
    }
}

/// Returns whether a record emitted by module `target` goes to the
/// process-local deterministic-record sink.
#[doc(hidden)]
pub fn forwards_target(target: &str) -> bool {
    FORWARDER
        .get()
        .is_some_and(|(_, policy)| policy.forwards(target))
}

/// Emits one deterministic record through tracing and the process-local sink. `target` is
/// the emitting module's path, so a forwarded record names the same module a record observed
/// through tracing does.
#[doc(hidden)]
pub fn emit_forwarded(target: &str, record_suffix: &str, message: fmt::Arguments<'_>) {
    tracing::info!("DETLOG {}{}", message, record_suffix);
    let index = count_forwarded();
    FORWARDER.get().expect("forwarder disappeared").0(target, record_suffix, index, message);
}

/// One guest thread's count of records it produced for forwarding since its
/// last counted request ([`take_forwarded_since_request`]).
struct ForwardedCount {
    tid: AtomicI32,
    /// The process the slot's thread belongs to. A forked child, whose memory
    /// is a copy, sees its parent's slots under another process id: stale.
    pid: AtomicI32,
    count: AtomicU64,
}

/// The per-thread counts, keyed by kernel thread id. Atomics rather than a
/// thread-local: the in-guest Tool runs inside loaders (SaBRe) that do not
/// promise the Tool working thread-local storage. A thread releases its slot
/// when it deregisters ([`release_forwarded_count`]), and a forked child
/// reclaims the slots its parent's threads held. A thread that still finds no
/// slot is uncounted, and the next counted request of every thread reports
/// [`UNCOUNTED`], so verification refuses the run instead of checking less.
static FORWARDED_COUNTS: [ForwardedCount; 512] = [const {
    ForwardedCount {
        tid: AtomicI32::new(0),
        pid: AtomicI32::new(0),
        count: AtomicU64::new(0),
    }
}; 512];

/// Set once a thread could not be counted ([`FORWARDED_COUNTS`]).
static FORWARDING_UNCOUNTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// The count a request reports when some thread's records could not be
/// counted.
pub const UNCOUNTED: u64 = u64::MAX;

fn current_pid() -> i32 {
    // SAFETY: getpid has no arguments and cannot fail. The raw syscall, not
    // libc's interposable getpid: this runs inside the guest under in-guest
    // LiteInst (see crate::util::raw_syscall).
    unsafe { crate::util::raw_syscall(libc::SYS_getpid, [0; 6]) as i32 }
}

fn forwarded_count_slot(tid: i32) -> Option<&'static ForwardedCount> {
    let pid = current_pid();
    let start = tid.unsigned_abs() as usize % FORWARDED_COUNTS.len();
    let probe = || {
        (0..FORWARDED_COUNTS.len())
            .map(move |offset| &FORWARDED_COUNTS[(start + offset) % FORWARDED_COUNTS.len()])
    };
    let claim = |slot: &'static ForwardedCount, owner: i32| {
        if slot
            .tid
            .compare_exchange(owner, tid, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            slot.pid.store(pid, Ordering::Release);
            slot.count.store(0, Ordering::Release);
            true
        } else {
            false
        }
    };
    // The thread's own slot first, wherever it is: a slot released before it
    // on its probe path leaves a hole it must not claim a second slot in.
    if let Some(slot) = FORWARDED_COUNTS.iter().find(|slot| {
        slot.tid.load(Ordering::Acquire) == tid && slot.pid.load(Ordering::Acquire) == pid
    }) {
        return Some(slot);
    }
    for slot in probe() {
        if slot.tid.load(Ordering::Acquire) == 0 && claim(slot, 0) {
            return Some(slot);
        }
    }
    // No free slot: take one only a private copy of this memory holds (a
    // parent's thread before a fork, or a process that has exited). A process
    // that shares this memory (a CLONE_VM child, such as a vfork child) still
    // counts in it.
    for slot in probe() {
        let owner = slot.tid.load(Ordering::Acquire);
        let owner_pid = slot.pid.load(Ordering::Acquire);
        if owner != 0 && owner_pid != pid && private_copy(owner_pid, pid) && claim(slot, owner) {
            return Some(slot);
        }
    }
    None
}

/// Whether process `owner`'s slots in this memory are a private copy, so this
/// process may reclaim them: `owner` has exited, or `kcmp` says it does not
/// share this memory. An unanswerable question (kcmp unavailable or refused)
/// is answered "no", so the slot is kept and a full table makes counts
/// [`UNCOUNTED`] instead of erasing a live count.
fn private_copy(owner: i32, pid: i32) -> bool {
    const KCMP_VM: libc::c_int = 1;
    // SAFETY: kcmp only compares the two processes' kernel resources.
    let answer = unsafe { libc::syscall(libc::SYS_kcmp, owner, pid, KCMP_VM, 0, 0) };
    answer > 0
        || (answer < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH))
}

/// Counts one record for this thread and returns its index in the thread's
/// counted interval (0 when the thread could not be counted, which makes every
/// later count [`UNCOUNTED`]).
fn count_forwarded() -> u64 {
    match forwarded_count_slot(current_tid()) {
        Some(slot) => slot.count.fetch_add(1, Ordering::AcqRel) + 1,
        None => {
            FORWARDING_UNCOUNTED.store(true, Ordering::Release);
            0
        }
    }
}

/// How many records this guest thread produced for forwarding since its last
/// counted request, and starts its next count. The in-guest Tool sends it with
/// that request ([`check_forwarded_count`]), on its RPC connection, which does
/// not depend on the forwarding socket. A forked child starts from zero: its
/// thread id is new. [`UNCOUNTED`] once any thread could not be counted.
pub fn take_forwarded_since_request() -> u64 {
    if FORWARDING_UNCOUNTED.load(Ordering::Acquire) {
        return UNCOUNTED;
    }
    forwarded_count_slot(current_tid())
        .map(|slot| slot.count.swap(0, Ordering::AcqRel))
        .unwrap_or(0)
}

/// Called by a Tool image whose coordinator requires in-guest forwarding by
/// the policy `expected` encodes (`Config::in_guest_detlog_forward_policy`).
/// Without a forwarder in this image, or with one that forwards by another
/// policy (records its policy drops are never counted), the records cannot be
/// accounted for, so every later count is [`UNCOUNTED`] and the coordinator
/// refuses the run ([`check_forwarded_count`]).
pub fn require_forwarding(expected: &str) {
    if FORWARDER
        .get()
        .is_none_or(|(_, policy)| policy.encode() != expected)
    {
        FORWARDING_UNCOUNTED.store(true, Ordering::Release);
    }
}

/// Releases this thread's count slot, at its deregistration, after its last
/// count was taken.
pub fn release_forwarded_count() {
    release_count_slot(current_tid());
}

fn release_count_slot(tid: i32) {
    let pid = current_pid();
    for slot in &FORWARDED_COUNTS {
        if slot.tid.load(Ordering::Acquire) == tid && slot.pid.load(Ordering::Acquire) == pid {
            slot.count.store(0, Ordering::Release);
            let _ = slot
                .tid
                .compare_exchange(tid, 0, Ordering::AcqRel, Ordering::Acquire);
        }
    }
}

/// Macro used to encapsulate tracing should-be-deterministic information.
/// This is currently at the INFO log level.
#[macro_export]
macro_rules! detlog {
    (event = $event:expr; $($arg:tt)+) => {{
        if $crate::detlog::forwards_target(::core::module_path!())
            || ::tracing::enabled!(::tracing::Level::INFO)
        {
            let record_suffix = $crate::detlog::record_suffix($event);
            if $crate::detlog::forwards_target(::core::module_path!()) {
                $crate::detlog::emit_forwarded(
                    ::core::module_path!(),
                    &record_suffix,
                    format_args!($($arg)+),
                );
            } else {
                ::tracing::info!("DETLOG {}{}", format_args!($($arg)+), record_suffix);
            }
        }
    }};
    ($($arg:tt)+) => {{
        $crate::detlog!(event = $crate::detlog::DetLogEvent::Other; $($arg)+);
    }};
}

/// Whether a [`detlog!`] record emitted at this point would reach anything.
///
/// WHY THIS IS NEEDED, and why it is a macro rather than a function.
///
/// `detlog!` routes to the process-local forwarder when one is installed and its
/// policy takes the calling module's records, and to `tracing::info!` otherwise. `tracing` does not evaluate a macro's value
/// expressions when the level is disabled, so work done *inside* a `detlog!`
/// argument is already free when nothing observes the record. Work done
/// *before* the macro is not, and callers that must prepare something expensive
/// to pass in have no way to know they can skip it.
///
/// `tracing`'s level check is per-callsite and keyed on the *calling module's*
/// target, so this has to expand at the caller rather than resolve inside
/// `detcore::detlog`; otherwise a target-scoped filter could enable one and
/// disable the other.
///
/// Use it only to skip preparatory work. It is not a substitute for `detlog!`'s
/// own gating, and a caller that guards a record with it must still emit that
/// record through `detlog!`.
#[macro_export]
macro_rules! detlog_observed {
    () => {
        $crate::detlog::forwards_target(::core::module_path!())
            || ::tracing::enabled!(::tracing::Level::INFO)
    };
}

/// Macro used to encapsulate tracing should-be-deterministic information.
/// This variant is at a higher log level and requires that logging verbosity is
/// set to DEBUG.
#[macro_export]
macro_rules! detlog_debug {
    (event = $event:expr; $($arg:tt)+) => {{
        if ::tracing::enabled!(::tracing::Level::DEBUG) {
            let record_suffix = $crate::detlog::record_suffix($event);
            ::tracing::debug!("DETLOG {}{}", format_args!($($arg)+), record_suffix);
        }
    }};
    ($($arg:tt)+) => {{
        $crate::detlog_debug!(event = $crate::detlog::DetLogEvent::Other; $($arg)+);
    }};
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use tracing::Metadata;
    use tracing::span;
    use tracing::subscriber::Interest;

    use super::DetLogEvent;
    use super::DetLogRecord;
    use super::ForwardPolicy;
    use super::RECORD_SEPARATOR;
    use super::record_suffix;

    /// A thread that sends more records than the socket buffer holds before its
    /// next request (a single writev's records, say) is not blocked: the
    /// receiver task takes them off the socket as they arrive, queued for their
    /// thread, and they are written only at the points the order rule names
    /// (here the end of the run), in the order they were sent.
    /// Serializes the tests that use the process-wide forwarding source, so
    /// one's notices never land in another's sink.
    fn global_source_test() -> std::sync::MutexGuard<'static, ()> {
        static GLOBAL_SOURCE: std::sync::Mutex<()> = std::sync::Mutex::new(());
        GLOBAL_SOURCE
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Each thread counts the records it produced for forwarding since its
    /// last counted request; taking the count starts the next one.
    #[test]
    fn forwarded_records_are_counted_per_thread_until_taken() {
        std::thread::spawn(|| {
            assert_eq!(super::take_forwarded_since_request(), 0);
            super::count_forwarded();
            super::count_forwarded();
            assert_eq!(super::take_forwarded_since_request(), 2);
            assert_eq!(super::take_forwarded_since_request(), 0);
            super::count_forwarded();
            // Another thread's records are its own.
            let other = std::thread::spawn(|| {
                super::count_forwarded();
                super::take_forwarded_since_request()
            });
            assert_eq!(other.join().unwrap(), 1);
            assert_eq!(super::take_forwarded_since_request(), 1);
        })
        .join()
        .unwrap();
    }

    /// A thread that deregistered frees its slot, so sequential thread churn
    /// never exhausts the table; a thread that finds no slot at all makes every
    /// later count [`super::UNCOUNTED`] (checked in a forked child, since that
    /// latch is process-wide and a fork inherits the table).
    #[test]
    fn count_slots_are_released_and_an_uncounted_thread_is_reported() {
        let _source = global_source_test();
        for _ in 0..(super::FORWARDED_COUNTS.len() * 2) {
            std::thread::spawn(|| {
                super::count_forwarded();
                assert_eq!(super::take_forwarded_since_request(), 1);
                super::release_forwarded_count();
            })
            .join()
            .unwrap();
        }
        assert!(!super::FORWARDING_UNCOUNTED.load(Ordering::Acquire));

        let child = unsafe { libc::fork() };
        assert!(child >= 0);
        if child == 0 {
            unsafe { libc::alarm(20) };
            // Threads that never release fill the table; the next is uncounted.
            for _ in 0..=super::FORWARDED_COUNTS.len() {
                std::thread::spawn(super::count_forwarded).join().unwrap();
            }
            let reported = super::take_forwarded_since_request();
            unsafe { libc::_exit(if reported == super::UNCOUNTED { 0 } else { 1 }) };
        }
        let mut status = 0;
        assert_eq!(unsafe { libc::waitpid(child, &mut status, 0) }, child);
        assert!(
            libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
            "{status:#x}"
        );
    }

    /// A thread whose probe path had a slot released before its own finds its
    /// own slot again instead of claiming the hole (tids chosen to collide).
    /// Another process that shares this memory keeps its slots; a forked copy
    /// and an exited process do not.
    #[test]
    fn a_released_slot_on_the_probe_path_is_not_claimed_twice() {
        let _source = global_source_test();
        let buckets = super::FORWARDED_COUNTS.len() as i32;
        let base = 0x3f00_0000 - 0x3f00_0000 % buckets + 7;
        let (first, second, third) = (base, base + buckets, base + 2 * buckets);
        let slot = |tid| super::forwarded_count_slot(tid).map(|slot| slot as *const _);
        let first_slot = slot(first).unwrap();
        let second_slot = slot(second).unwrap();
        let third_slot = slot(third).unwrap();
        assert_ne!(second_slot, third_slot);
        super::release_count_slot(second);
        assert_eq!(
            slot(third),
            Some(third_slot),
            "the hole is not a second slot"
        );
        assert_eq!(slot(first), Some(first_slot));
        for tid in [first, third] {
            super::release_count_slot(tid);
        }

        let me = super::current_pid();
        assert!(!super::private_copy(me, me));
        let child = unsafe { libc::fork() };
        assert!(child >= 0);
        if child == 0 {
            unsafe { libc::_exit(0) };
        }
        assert!(
            super::private_copy(child, me),
            "a forked child's memory is its own"
        );
        let mut status = 0;
        assert_eq!(unsafe { libc::waitpid(child, &mut status, 0) }, child);
        assert!(
            super::private_copy(child, me),
            "an exited process holds nothing"
        );
        // A process that shares this memory (CLONE_VM, its own descriptor
        // table, as a vfork child), on a stack of its own.
        extern "C" fn shared_child(parent: *mut libc::c_void) -> libc::c_int {
            let own = unsafe { libc::syscall(libc::SYS_getpid) } as i32;
            i32::from(super::private_copy(parent as usize as i32, own))
        }
        let mut stack = vec![0u8; 256 * 1024];
        let top = (stack.as_mut_ptr() as usize + stack.len()) & !15;
        let shared = unsafe {
            libc::clone(
                shared_child,
                top as *mut libc::c_void,
                libc::CLONE_VM | libc::SIGCHLD,
                me as usize as *mut libc::c_void,
            )
        };
        assert!(shared > 0, "{}", std::io::Error::last_os_error());
        assert_eq!(unsafe { libc::waitpid(shared, &mut status, 0) }, shared);
        drop(stack);
        assert!(
            libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
            "a CLONE_VM child shares this memory: {status:#x}"
        );
    }

    /// A Tool image the coordinator requires to forward by a policy reports
    /// every count as UNCOUNTED when it has no forwarder, or one with another
    /// policy (records that policy drops would never be counted); with exactly
    /// that policy it counts normally. Each case in a forked child: the latch
    /// and the forwarder are process-wide.
    #[test]
    fn a_required_forwarder_that_is_missing_or_narrowed_makes_counts_uncounted() {
        let _source = global_source_test();
        let in_child = |install: Option<&str>, expected: &str| -> bool {
            let child = unsafe { libc::fork() };
            assert!(child >= 0);
            if child == 0 {
                unsafe { libc::alarm(20) };
                // The parent has none (the test process installs no
                // forwarder), so the child's is the one installed here.
                let ok = super::FORWARDER.get().is_none()
                    && install.is_none_or(|policy| {
                        let policy = ForwardPolicy::decode(policy).unwrap();
                        super::set_forwarder(|_, _, _, _| {}, policy).is_ok()
                    });
                super::require_forwarding(expected);
                let uncounted = super::take_forwarded_since_request() == super::UNCOUNTED;
                unsafe {
                    libc::_exit(if !ok {
                        2
                    } else if uncounted {
                        1
                    } else {
                        0
                    })
                };
            }
            let mut status = 0;
            assert_eq!(unsafe { libc::waitpid(child, &mut status, 0) }, child);
            assert!(
                libc::WIFEXITED(status) && libc::WEXITSTATUS(status) != 2,
                "{status:#x}"
            );
            libc::WEXITSTATUS(status) == 1
        };
        let required = "1,detcore::tool_local=1";
        assert!(in_child(None, required), "no forwarder: uncounted");
        assert!(
            in_child(Some("1,detcore::tool_local=0"), required),
            "a narrowed policy: uncounted"
        );
        assert!(
            !in_child(Some(required), required),
            "the required policy: counted"
        );
    }

    /// THE COMPLETENESS RULE: a counted request that says its thread produced
    /// more records than arrived from it writes a loss notice and records the
    /// determinism loss, so verification refuses the run; so does one that
    /// says fewer (records that were not the Tool's) or [`super::UNCOUNTED`].
    #[test]
    fn a_counted_request_with_missing_records_is_a_loss() {
        use std::os::fd::FromRawFd;
        use std::os::fd::OwnedFd;
        use std::sync::Arc;
        use std::sync::Mutex;

        let _source = global_source_test();
        let mut fds = [0; 2];
        assert_eq!(
            unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_SEQPACKET, 0, fds.as_mut_ptr()) },
            0
        );
        let written = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
        let sink = Arc::clone(&written);
        super::set_forwarded_source(
            unsafe { OwnedFd::from_raw_fd(fds[0]) },
            Box::new(move |record| sink.lock().unwrap().push(record.to_vec())),
        );
        let send = |tid: i32, index: u64, line: &str| {
            let message = super::tagged_forwarded_message(tid, index, line.as_bytes());
            let sent = unsafe { libc::send(fds[1], message.as_ptr().cast(), message.len(), 0) };
            assert_eq!(sent, message.len() as isize);
        };
        // This thread's notices only: another test may write a global loss
        // notice into the registered sink meanwhile.
        let notices = |written: &Arc<Mutex<Vec<Vec<u8>>>>| -> Vec<String> {
            written
                .lock()
                .unwrap()
                .iter()
                .filter(|record| record.starts_with(super::FORWARDING_LOSS_NOTICE.as_bytes()))
                .map(|record| String::from_utf8_lossy(record).into_owned())
                .filter(|notice| notice.contains("thread 41 ") || notice.contains("thread 41)"))
                .collect()
        };
        send(41, 1, "a\n");
        send(41, 2, "b\n");
        super::check_forwarded_count(41, 2);
        assert_eq!(notices(&written).len(), 0, "as many records as produced");
        send(41, 1, "c\n");
        super::check_forwarded_count(41, 3);
        assert_eq!(notices(&written).len(), 1, "a deficit is a loss");
        assert!(
            notices(&written)[0].contains(
                "lost: thread 41 produced 3 since its last counted request and 1 arrived"
            ),
            "{:?}",
            notices(&written)
        );
        // More than produced: records that were not the Tool's (a guest that
        // wrote to the socket around the Tool) cannot stand in for lost ones.
        send(41, 1, "d\n");
        send(41, 2, "e\n");
        super::check_forwarded_count(41, 1);
        assert_eq!(notices(&written).len(), 2, "a surplus is refused too");
        assert!(
            notices(&written)[1].contains("not the Tool's: thread 41 produced 1"),
            "{:?}",
            notices(&written)
        );
        super::check_forwarded_count(41, super::UNCOUNTED);
        assert_eq!(notices(&written).len(), 3, "an uncounted thread is refused");
        assert!(
            notices(&written)[2].contains("uncounted"),
            "{:?}",
            notices(&written)
        );
        // As many records as produced, but one index skipped: a record was lost
        // and another stood in for it.
        send(41, 1, "f\n");
        send(41, 3, "g\n");
        super::check_forwarded_count(41, 2);
        assert_eq!(notices(&written).len(), 4, "a skipped index is a loss");
        assert!(
            notices(&written)[3]
                .contains("lost: thread 41 sent record 3 of its interval where 2 was next"),
            "{:?}",
            notices(&written)
        );
        // As many records as produced, but one index repeated: a record that
        // was not the Tool's stood in for a lost one.
        send(41, 1, "h\n");
        send(41, 1, "padding\n");
        super::check_forwarded_count(41, 2);
        assert_eq!(notices(&written).len(), 5, "a repeated index is refused");
        assert!(
            notices(&written)[4].contains(
                "not the Tool's: thread 41 sent record 1 of its interval where 2 was next"
            ),
            "{:?}",
            notices(&written)
        );
        // The interval restarts after each counted request.
        send(41, 1, "i\n");
        super::check_forwarded_count(41, 1);
        assert_eq!(notices(&written).len(), 5, "a new interval starts at 1");
        assert!(super::determinism_loss().is_some());
        super::clear_forwarded_source();
        unsafe { libc::close(fds[1]) };
    }

    #[test]
    fn a_full_socket_buffer_does_not_wait_for_a_request() {
        let _source = global_source_test();
        use std::os::fd::FromRawFd;
        use std::os::fd::OwnedFd;
        use std::sync::Arc;
        use std::sync::Mutex;
        use std::time::Duration;
        use std::time::Instant;

        const RECORDS: usize = 2000;
        let mut fds = [0; 2];
        assert_eq!(
            unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_SEQPACKET, 0, fds.as_mut_ptr()) },
            0
        );
        let small: libc::c_int = 4096;
        for fd in fds {
            for option in [libc::SO_SNDBUF, libc::SO_RCVBUF] {
                unsafe {
                    libc::setsockopt(
                        fd,
                        libc::SOL_SOCKET,
                        option,
                        (&raw const small).cast(),
                        std::mem::size_of_val(&small) as libc::socklen_t,
                    )
                };
            }
        }
        let written = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
        let sink = Arc::clone(&written);
        super::set_forwarded_source(
            unsafe { OwnedFd::from_raw_fd(fds[0]) },
            Box::new(move |record| sink.lock().unwrap().push(record.to_vec())),
        );
        // The coordinator's runtime, and its first request: from the root,
        // thread 1, so thread 7's records stay queued until the end.
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async { super::drain_forwarded_at_request(1) });
        let sender = fds[1];
        let started = Instant::now();
        let sending = std::thread::spawn(move || {
            for index in 0..RECORDS {
                let message = super::tagged_forwarded_message(
                    7,
                    index as u64 + 1,
                    format!("record {index:04}\n").as_bytes(),
                );
                let sent = unsafe {
                    libc::send(
                        sender,
                        message.as_ptr().cast(),
                        message.len(),
                        libc::MSG_NOSIGNAL,
                    )
                };
                assert_eq!(sent, message.len() as isize);
            }
        });
        while !sending.is_finished() {
            assert!(
                started.elapsed() < Duration::from_secs(20),
                "the sender stayed blocked on a full socket buffer"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        sending.join().unwrap();
        // This test's records only: another test may write a global loss
        // notice into the registered sink meanwhile.
        let own = |written: &Arc<Mutex<Vec<Vec<u8>>>>| -> Vec<Vec<u8>> {
            written
                .lock()
                .unwrap()
                .iter()
                .filter(|record| record.starts_with(b"record "))
                .cloned()
                .collect()
        };
        assert!(
            own(&written).is_empty(),
            "records were written before their point"
        );
        super::clear_forwarded_source();
        let written = own(&written);
        assert_eq!(written.len(), RECORDS);
        for (index, record) in written.iter().enumerate() {
            assert_eq!(record, format!("record {index:04}\n").as_bytes());
        }
        unsafe { libc::close(sender) };
    }

    #[test]
    fn forwarded_line_names_the_emitting_module() {
        assert_eq!(
            super::forwarded_line(
                "detcore::tool_local",
                " DETLOG_RECORD={}",
                format_args!("USER RAND: seeding PRNG for root thread with seed {}", 0)
            ),
            b"INFO detcore::tool_local: DETLOG USER RAND: seeding PRNG for root thread with seed 0 DETLOG_RECORD={}\n"
                .to_vec()
        );
    }

    /// Forwarding failures are never silent, and never change the guest's errno.
    /// One sequential test, because the unreported-loss count is process-wide:
    /// - a record sent on an invalid descriptor fails, and so does its notice,
    ///   and errno is left as it was;
    /// - the next record that gets through is preceded by a loss notice for it;
    /// - a record too large for the socket arrives as a loss notice, and the next
    ///   small record intact.
    #[test]
    fn forwarding_failures_become_loss_notices() {
        let mut pair = [-1; 2];
        // SAFETY: socketpair writes two descriptors on success.
        let created = unsafe {
            libc::socketpair(
                libc::AF_UNIX,
                libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
                0,
                pair.as_mut_ptr(),
            )
        };
        assert_eq!(created, 0);
        let [sender, receiver] = pair;
        let mut buffer = vec![0u8; 2 << 20];
        let mut receive = || {
            // SAFETY: `buffer` is a live, writable byte slice.
            let received = unsafe {
                libc::recv(
                    receiver,
                    buffer.as_mut_ptr().cast(),
                    buffer.len(),
                    libc::MSG_DONTWAIT,
                )
            };
            assert!(received > 0, "{}", std::io::Error::last_os_error());
            String::from_utf8_lossy(&buffer[..received as usize]).into_owned()
        };
        let is_notice =
            |text: &str| text.starts_with(&format!("{} ", super::FORWARDING_LOSS_NOTICE));

        // SAFETY: errno is this thread's own.
        unsafe {
            *libc::__errno_location() = libc::EAGAIN;
            super::send_forwarded_record(-1, "detcore", "", 1, format_args!("lost"));
            assert_eq!(*libc::__errno_location(), libc::EAGAIN);
        }
        let tag = |index: u64| format!("T{}.{index} ", super::current_tid());
        super::send_forwarded_record(sender, "detcore", "", 2, format_args!("after"));
        let notice = receive();
        assert!(is_notice(&notice), "{notice}");
        assert_eq!(receive(), format!("{}INFO detcore: DETLOG after\n", tag(2)));

        let small: libc::c_int = 4096;
        // SAFETY: a valid socket and an int option value.
        unsafe {
            libc::setsockopt(
                sender,
                libc::SOL_SOCKET,
                libc::SO_SNDBUF,
                (&raw const small).cast(),
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            )
        };
        let huge = "x".repeat(1 << 20);
        super::send_forwarded_record(sender, "detcore", "", 1, format_args!("{huge}"));
        super::send_forwarded_record(sender, "detcore", "", 2, format_args!("small"));
        let notice = receive();
        assert!(is_notice(&notice), "{notice}");
        assert_eq!(receive(), format!("{}INFO detcore: DETLOG small\n", tag(2)));
        // SAFETY: closing the two descriptors this test created.
        unsafe {
            libc::close(sender);
            libc::close(receiver);
        }
    }

    /// The order rule at `set_forwarded_source`, on its own state (no process-wide
    /// source, so no other test's scheduler commits can interfere): records are
    /// written at the sender's own request only before any turn or when it is the
    /// thread of the last ordinary COMMIT, otherwise at its own next COMMIT, and the
    /// rest at the end in thread-id order.
    #[test]
    fn forwarded_records_are_written_at_points_the_schedule_fixes() {
        let mut order = super::ForwardedOrder::new();
        let text = |records: Vec<Vec<u8>>| -> Vec<String> {
            records
                .into_iter()
                .map(|record| String::from_utf8(record).unwrap())
                .collect()
        };
        let none: Vec<String> = Vec::new();

        // Before any turn only the root thread exists: its records go out at its
        // request. Its turn 0 makes it the exclusive runner.
        order.queue(3, b"root-1".to_vec());
        assert_eq!(text(order.at_request(3)), ["root-1"]);
        assert_eq!(text(order.at_commit(3, true)), none);
        // A forked child that does not hold the turn: queued until its own COMMIT,
        // even when its request arrives first.
        order.queue(9, b"child-1".to_vec());
        assert_eq!(text(order.at_request(9)), none);
        assert_eq!(text(order.at_commit(9, true)), ["child-1"]);
        // Thread 9 now holds the turn: its records go out at its request; thread 3's
        // records sent meanwhile wait for 3's COMMIT.
        order.queue(3, b"root-2".to_vec());
        order.queue(9, b"child-2".to_vec());
        assert_eq!(text(order.at_request(9)), ["child-2"]);
        assert_eq!(text(order.at_request(3)), none);
        // A BACKGROUND commit writes its thread's records but leaves no exclusive
        // runner, so a later request writes nothing.
        assert_eq!(text(order.at_commit(3, false)), ["root-2"]);
        order.queue(3, b"root-3".to_vec());
        assert_eq!(text(order.at_request(3)), none);
        // At the end, every queued record, thread by thread in thread-id order.
        order.queue(12, b"late-12".to_vec());
        order.queue(5, b"late-5".to_vec());
        assert_eq!(text(order.finish()), ["root-3", "late-5", "late-12"]);
        assert_eq!(text(order.finish()), none);

        // Two roots before any turn: the second request yields one ambiguity notice
        // (and only one), and the second root's records wait for its COMMIT.
        let mut order = super::ForwardedOrder::new();
        order.queue(3, b"first-root".to_vec());
        order.queue(4, b"second-root".to_vec());
        assert_eq!(text(order.at_request(3)), ["first-root"]);
        let notice = String::from_utf8(super::FORWARDING_AMBIGUOUS_ORDER_NOTICE.to_vec()).unwrap();
        assert!(notice.starts_with(&format!("{} ", super::FORWARDING_LOSS_NOTICE)));
        assert_eq!(text(order.at_request(4)), [notice]);
        assert_eq!(text(order.at_request(4)), none);
        assert_eq!(text(order.at_commit(4, true)), ["second-root"]);

        // Untagged messages: a loss notice passes as is; an ordinary record from an
        // older runtime is written with one untagged-record notice before it.
        let mut order = super::ForwardedOrder::new();
        let retired = String::from_utf8(super::FORWARDING_RETIRED_NOTICE.to_vec()).unwrap();
        assert_eq!(
            text(order.untagged(super::FORWARDING_RETIRED_NOTICE)),
            [retired]
        );
        let untagged =
            String::from_utf8(super::FORWARDING_UNTAGGED_RECORD_NOTICE.to_vec()).unwrap();
        assert!(untagged.starts_with(&format!("{} ", super::FORWARDING_LOSS_NOTICE)));
        assert_eq!(
            text(order.untagged(b"INFO detcore: DETLOG legacy-1\n")),
            [untagged, "INFO detcore: DETLOG legacy-1\n".to_string()]
        );
        assert_eq!(
            text(order.untagged(b"INFO detcore: DETLOG legacy-2\n")),
            ["INFO detcore: DETLOG legacy-2\n"]
        );
    }

    #[test]
    fn a_loss_notice_records_the_determinism_loss_latch() {
        let _source = global_source_test();
        // The latch is the verification safeguard, independent of whether the
        // notice line reaches a log: it is set even with no registered source.
        super::write_loss_notice("test: an unscheduled exit");
        let loss = super::determinism_loss().expect("the latch must be set");
        assert!(!loss.is_empty());
        // It keeps the first reason and is never cleared by later ones.
        super::record_determinism_loss("test: a later reason");
        assert_eq!(super::determinism_loss(), Some(loss));
    }

    #[test]
    fn split_forwarded_message_reads_the_sender_tag() {
        assert_eq!(
            super::split_forwarded_message(b"T42.7 INFO detcore: x\n"),
            Some((42, 7, &b"INFO detcore: x\n"[..]))
        );
        // A tag without an index is a runtime older than the index.
        assert_eq!(
            super::split_forwarded_message(b"T42 INFO detcore: x\n"),
            None
        );
        assert_eq!(super::split_forwarded_message(b"T42. INFO"), None);
        assert_eq!(super::split_forwarded_message(b"T42.x INFO"), None);
        assert_eq!(
            super::split_forwarded_message(super::FORWARDING_RETIRED_NOTICE),
            None
        );
        assert_eq!(super::split_forwarded_message(b"T INFO"), None);
        assert_eq!(super::split_forwarded_message(b"Tx1 INFO"), None);
        assert_eq!(super::split_forwarded_message(b"INFO detcore"), None);
    }

    #[test]
    fn test_detlog() {
        detlog!("Hello : {}. From {:?}", "World", 31337);
    }

    #[test]
    fn structured_record_round_trips_and_refuses_an_incomplete_current_shape() {
        let suffix = record_suffix(DetLogEvent::SyscallResult {
            finished_syscall_number: 37,
        });
        let line = format!("INFO detcore: DETLOG finish syscall #999{suffix}");
        let (human, record) = DetLogRecord::split(&line).unwrap();
        assert_eq!(human, "INFO detcore: DETLOG finish syscall #999");
        assert_eq!(
            record.unwrap().event,
            DetLogEvent::SyscallResult {
                finished_syscall_number: 37
            }
        );

        let missing_number = format!(
            "INFO detcore: DETLOG finish syscall #999{RECORD_SEPARATOR}{{\"schema\":1,\"event\":{{\"kind\":\"syscall_result\"}}}}"
        );
        assert!(
            DetLogRecord::split(&missing_number)
                .unwrap_err()
                .contains("finished_syscall_number"),
            "an incomplete current record must fail by field name"
        );
    }

    /// Minimal subscriber that reports every callsite as enabled.
    ///
    /// `register_callsite` deliberately answers `sometimes()` rather than
    /// letting the default derive `always()`: an `always`/`never` answer is
    /// cached per callsite for the life of the process, which would leak
    /// between tests.
    struct AlwaysEnabled;

    impl tracing::Subscriber for AlwaysEnabled {
        fn register_callsite(&self, _: &'static Metadata<'static>) -> Interest {
            Interest::sometimes()
        }
        fn enabled(&self, _: &Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _: &span::Attributes<'_>) -> span::Id {
            span::Id::from_u64(1)
        }
        fn record(&self, _: &span::Id, _: &span::Record<'_>) {}
        fn record_follows_from(&self, _: &span::Id, _: &span::Id) {}
        fn event(&self, _: &tracing::Event<'_>) {}
        fn enter(&self, _: &span::Id) {}
        fn exit(&self, _: &span::Id) {}
    }

    /// `detlog_observed!` must answer true when a subscriber would take the
    /// record. Callers use it to decide whether to prepare data for a
    /// `detlog!`, so a false negative silently drops determinism evidence.
    #[test]
    fn detlog_observed_is_true_when_a_subscriber_is_listening() {
        tracing::subscriber::with_default(AlwaysEnabled, || {
            assert!(detlog_observed!());
        });
    }

    /// ...and false when nothing is listening, which is the whole point: it is
    /// what lets `detlog_memory_maps` skip enumerating `/proc/<pid>/maps` on
    /// every syscall of a run that writes no log. Measured before this gate
    /// existed, on a QEMU/Linux boot with `RUST_LOG` unset (123 bytes of log
    /// produced): `--detlog-stack` cost 4.36x and `--detlog-heap` 4.76x the
    /// no-flag baseline.
    ///
    /// Uses a distinct callsite from the enabled test above on purpose --
    /// `tracing` caches per-callsite interest, so sharing one callsite between
    /// the two cases would make them order-dependent.
    #[test]
    fn detlog_observed_is_false_when_nothing_is_listening() {
        tracing::subscriber::with_default(tracing::subscriber::NoSubscriber::default(), || {
            assert!(!detlog_observed!());
        });
    }

    /// Takes WARN from everywhere and INFO only from `detcore::random` and its
    /// submodules, as `RUST_LOG=warn,detcore::random=info` does.
    struct RandomAtInfo;

    impl tracing::Subscriber for RandomAtInfo {
        fn register_callsite(&self, _: &'static Metadata<'static>) -> Interest {
            Interest::sometimes()
        }
        fn enabled(&self, metadata: &Metadata<'_>) -> bool {
            *metadata.level() <= tracing::Level::WARN
                || (*metadata.level() <= tracing::Level::INFO
                    && metadata.target().starts_with("detcore::random"))
        }
        fn max_level_hint(&self) -> Option<tracing::level_filters::LevelFilter> {
            Some(tracing::level_filters::LevelFilter::INFO)
        }
        fn new_span(&self, _: &span::Attributes<'_>) -> span::Id {
            span::Id::from_u64(1)
        }
        fn record(&self, _: &span::Id, _: &span::Record<'_>) {}
        fn record_follows_from(&self, _: &span::Id, _: &span::Id) {}
        fn event(&self, _: &tracing::Event<'_>) {}
        fn enter(&self, _: &span::Id) {}
        fn exit(&self, _: &span::Id) {}
    }

    /// A forwarded process must emit exactly the records the coordinator's
    /// subscriber would take in-process. A target-scoped filter used to yield
    /// no forwarding at all, because the coordinator asked only about the
    /// generic `detcore` target, so SaBRe silently dropped the scoped records.
    #[test]
    fn forward_policy_answers_per_target_as_the_subscriber_does() {
        let policy = tracing::subscriber::with_default(RandomAtInfo, || {
            ForwardPolicy::from_current_subscriber("warn,detcore::random=info")
        });
        assert!(policy.forwards_any());
        assert!(policy.forwards("detcore::random"));
        assert!(policy.forwards("detcore::random::inner"));
        assert!(!policy.forwards("detcore"));
        assert!(!policy.forwards("detcore::tool_local"));
        assert!(!policy.forwards("detcore::scheduler::runqueue"));
        assert_eq!(policy.encode(), "0,warn=0,detcore::random=1");

        let everything = tracing::subscriber::with_default(AlwaysEnabled, || {
            ForwardPolicy::from_current_subscriber("")
        });
        assert_eq!(everything, ForwardPolicy::all());

        let nothing =
            tracing::subscriber::with_default(tracing::subscriber::NoSubscriber::default(), || {
                ForwardPolicy::from_current_subscriber("warn,detcore::random=info")
            });
        assert!(!nothing.forwards_any());
    }

    /// The longest named target that prefixes the record's target decides, by
    /// string prefix as `tracing-subscriber` matches directives.
    #[test]
    fn forward_policy_most_specific_target_decides() {
        let policy = ForwardPolicy::decode("1,detcore=0,detcore::random=1").unwrap();
        assert!(policy.forwards("hermit"));
        assert!(!policy.forwards("detcore"));
        assert!(!policy.forwards("detcore::tool_local"));
        assert!(policy.forwards("detcore::random"));
        assert!(policy.forwards("detcore::randomness"));
    }

    #[test]
    fn forward_policy_encoding_round_trips_and_refuses_malformed_values() {
        assert_eq!(ForwardPolicy::decode("1").unwrap(), ForwardPolicy::all());
        let policy = ForwardPolicy::decode("0,warn=0,detcore::random=1").unwrap();
        assert_eq!(ForwardPolicy::decode(&policy.encode()).unwrap(), policy);
        for malformed in ["", "2", "1,detcore", "1,=1", "1,detcore=2", "1,detcore=1=0"] {
            assert!(
                ForwardPolicy::decode(malformed).is_err(),
                "{malformed:?} must be refused"
            );
        }
    }
}
