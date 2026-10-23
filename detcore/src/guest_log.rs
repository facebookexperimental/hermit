/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Detcore's warnings from inside the guest, printed by the coordinator.
//!
//! WHY THIS IS NEEDED. Where Detcore runs in the guest's own process (DBT,
//! SaBRe, in-guest LiteInst and in-guest-trap), nothing printed its `tracing`
//! events: a warning that ptrace and KVM print at the default level was lost.
//!
//! One mechanism serves every such runtime. The runtime installs [`subscriber`]
//! with the one synchronous coordinator request it already has for Tool
//! callbacks (the one shared open file control uses). The subscriber sends each
//! WARN and ERROR event as one `GlobalRequest::GuestLog`, and the coordinator
//! emits it again with the event's own level and target (`emit`). So the
//! run's own filter decides, per target, exactly as it does for ptrace's
//! events, and the line reads as ptrace's does, without span context.
//!
//! WHY ONLY WARN AND ERROR. Verification compares every INFO record, and a
//! guest record printed when it arrives would land at a host-timed place
//! among the run's ordered records. Warnings and errors are not compared, and
//! they are what a default run prints. DETLOG records, which are INFO or
//! DEBUG, keep their own ordered and counted channels ([`crate::detlog`](mod@crate::detlog), or
//! the runtime's own, [`OwnChannel`]).
//!
//! A runtime with its own channel ([`OwnChannel`]) runs it at the run's own
//! level, so it sends nothing: its channel already has every event the run
//! prints.
//!
//! An event that cannot be sent (outside a Tool callback, while another
//! request of the process is in flight, or from inside the send itself) is
//! counted, per address space. The next request that gets through carries the
//! count, which the coordinator prints as a warning; Detcore also sends any
//! count still held when a thread exits, at `exit_group`, before an exec
//! (`prepare_exec`, every exec path), before an injected `exit_group` and at
//! an unrecoverable shutdown (`flush_unsent`), and last in each thread's exit
//! hook, through the hook's own connection (`flush_unsent_through`). A
//! process killed where its runtime runs no exit hook, or a warning after its
//! last such point, leaves a count that cannot be reported. (DBT with no coordinator prints its warnings on its own
//! diagnostic channel instead.) Nothing is written to
//! the guest's own descriptors, and formatting uses only Rust's allocator (see
//! `detcore/clippy.toml` for the C library calls that would use the guest's
//! heap).

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use serde::Deserialize;
use serde::Serialize;
use tracing::Event;
use tracing::Level;
use tracing::Metadata;
use tracing::Subscriber;
use tracing::callsite::Callsite;
use tracing::field::Field;
use tracing::field::FieldSet;
use tracing::field::Value;
use tracing::field::Visit;
use tracing::level_filters::LevelFilter;
use tracing::metadata::Kind;
use tracing::span;
use tracing::subscriber::Interest;

/// One Detcore event from inside the guest.
#[derive(PartialEq, Eq, Debug, Clone, Serialize, Deserialize)]
pub struct GuestLogRecord {
    /// The event's level, as [`level_code`] numbers it.
    pub level: u8,
    /// The event's target: the emitting module's path, unless it named one.
    pub target: String,
    /// The event's fields as the coordinator's formatter prints them: the
    /// message, then `name=value` for each other field.
    pub message: String,
}

/// An in-guest runtime's one synchronous request to Detcore's global state from
/// inside a Tool callback: it sends the request and waits for the answer, and
/// refuses with an error where it cannot send. The runtime's shared open file
/// transport sends through the same function.
pub type CoordinatorCall = fn(
    <crate::GlobalState as reverie::GlobalTool>::Request,
) -> io::Result<<crate::GlobalState as reverie::GlobalTool>::Response>;

/// A runtime's own channel for its events: DBT's, which carries its DETLOG
/// records through `tracing` (where other runtimes use [`crate::detlog`](mod@crate::detlog)'s
/// forwarder), to its protected verification evidence or to its diagnostic
/// output, at the level the run asked for. It takes every event up to
/// `max_level`, exactly as the runtime's own subscriber did. A subscriber with
/// an own channel sends nothing to the coordinator: events above the run's
/// level are not printed there either.
#[derive(Clone, Copy)]
pub struct OwnChannel {
    /// The most verbose level the channel takes.
    pub max_level: Level,
    /// Writes one event to the channel.
    pub write: fn(&Event<'_>),
}

/// The most verbose level sent to the coordinator.
const SENT_LEVEL: Level = Level::WARN;

/// Events of this address space that could not be sent since the last
/// request that was. A forked child, whose copy of memory holds its parent's
/// count, starts again at zero ([`starts_new_process`]).
static UNSENT: AtomicU64 = AtomicU64::new(0);
/// The address space [`UNSENT`] counts for, once a process's first thread has
/// started in it; a copy of memory that names another address space is a
/// forked child's.
static UNSENT_OWNER: Mutex<Option<crate::types::MmId>> = Mutex::new(None);
static NEXT_SPAN_ID: AtomicU64 = AtomicU64::new(1);

/// Whether a send is under way in this process, so an event the send itself
/// emits, or another thread's meanwhile, is counted instead of sent. (The
/// in-guest coordinator connection takes one request at a time, so another
/// thread's request would be refused as in flight anyway.) A process-wide flag,
/// not a thread-local: Detcore's in-guest thread-local layout is pinned by
/// LiteInst's allocator qualifier, and DynamoRIO's application threads cannot
/// run thread-local destructors (see `detcore/clippy.toml`).
static SENDING: AtomicBool = AtomicBool::new(false);

/// The number this module gives `level`: 1 for ERROR up to 5 for TRACE.
pub fn level_code(level: &Level) -> u8 {
    match *level {
        Level::ERROR => 1,
        Level::WARN => 2,
        Level::INFO => 3,
        Level::DEBUG => 4,
        Level::TRACE => 5,
    }
}

fn level_from_code(code: u8) -> Option<Level> {
    match code {
        1 => Some(Level::ERROR),
        2 => Some(Level::WARN),
        3 => Some(Level::INFO),
        4 => Some(Level::DEBUG),
        5 => Some(Level::TRACE),
        _ => None,
    }
}

/// The subscriber an in-guest runtime installs as its global default: it sends
/// WARN and ERROR events through `call`, and writes every event up to its
/// level to `own`, when the runtime has one.
pub fn subscriber(call: CoordinatorCall, own: Option<OwnChannel>) -> impl Subscriber {
    GuestLogSubscriber { call, own }
}

/// Installs [`subscriber`] as this process's global default. Returns false,
/// and changes nothing, when the process already has a global subscriber.
pub fn install(call: CoordinatorCall, own: Option<OwnChannel>) -> bool {
    tracing::subscriber::set_global_default(subscriber(call, own)).is_ok()
}

/// Notes that a process's first thread started in address space `mm`, its
/// final one (after an exec's). When the count in memory belongs to another
/// address space, this is a forked child holding its parent's count, which is
/// not its own, and it starts at zero; the root process's own count stays.
/// Detcore gives a vfork child its parent's MmId, so it keeps the count; the
/// in-guest runtimes copy a vfork child's memory, so that child's report can
/// repeat its parent's count (a duplicate, never a silent loss). An image an
/// exec-private-init successor starts runs no thread start and keeps the
/// count it inherited the same way. Detcore calls it when a process's first
/// thread starts; it acts only under an in-guest runtime's subscriber.
pub(crate) fn starts_new_process(mm: crate::types::MmId) {
    // Only an in-guest runtime's process counts unsent warnings.
    if !tracing::dispatcher::get_default(|dispatch| dispatch.is::<GuestLogSubscriber>()) {
        return;
    }
    let mut owner = UNSENT_OWNER
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    if owner.is_some_and(|owner| owner != mm) {
        UNSENT.store(0, Ordering::Release);
        // A parent's send under way at the fork is not this child's.
        SENDING.store(false, Ordering::Release);
    }
    *owner = Some(mm);
}

/// Sends this address space's unsent count, if any, through the installed
/// subscriber. Detcore calls it where a process may end or replace its image:
/// at a thread's exit, at `exit_group` and before `execve`. Without an in-guest
/// subscriber, or with nothing held, it does nothing.
pub(crate) fn flush_unsent() {
    if UNSENT.load(Ordering::Acquire) == 0 {
        return;
    }
    tracing::dispatcher::get_default(|dispatch| {
        if let Some(subscriber) = dispatch.downcast_ref::<GuestLogSubscriber>() {
            subscriber.send(None);
        }
    });
}

/// Sends this address space's unsent count, if any, through `rpc`: a Tool exit
/// callback's own connection to the global state. It serves the callback where
/// the warning path's call cannot (on LiteInst exit callbacks run outside the
/// runtime's Tool-callback mark), so the hook's own warnings, and every earlier
/// one still held, are reported. A count the coordinator does not take back is
/// kept. Without an in-guest subscriber nothing is ever held, so it sends
/// nothing.
pub(crate) async fn flush_unsent_through<R>(rpc: &R)
where
    R: reverie::GlobalRPC<crate::GlobalState>,
{
    let unsent = UNSENT.swap(0, Ordering::AcqRel);
    if unsent == 0 {
        return;
    }
    let answer = rpc
        .send_rpc(crate::tool_global::guest_log_request(None, unsent))
        .await;
    if crate::tool_global::guest_log_reply(answer).is_err() {
        UNSENT.fetch_add(unsent, Ordering::AcqRel);
    }
}

/// Clears [`SENDING`] when a send ends, unwinding included.
struct Sending;

impl Drop for Sending {
    fn drop(&mut self) {
        SENDING.store(false, Ordering::Release);
    }
}

struct GuestLogSubscriber {
    call: CoordinatorCall,
    own: Option<OwnChannel>,
}

impl GuestLogSubscriber {
    /// Sends one record, or only this process's unsent count, with that count,
    /// unless a send is already under way. A record that is not sent is
    /// counted.
    fn send(&self, record: Option<GuestLogRecord>) {
        let counts = u64::from(record.is_some());
        if SENDING.swap(true, Ordering::AcqRel) {
            UNSENT.fetch_add(counts, Ordering::AcqRel);
            return;
        }
        let _sending = Sending;
        let unsent = UNSENT.swap(0, Ordering::AcqRel);
        let sent = (self.call)(crate::tool_global::guest_log_request(record, unsent))
            .and_then(crate::tool_global::guest_log_reply);
        if sent.is_err() {
            UNSENT.fetch_add(unsent + counts, Ordering::AcqRel);
        }
    }

    fn own_takes(&self, level: &Level) -> Option<OwnChannel> {
        self.own.filter(|own| *level <= own.max_level)
    }

    /// Whether events of `level` are sent: WARN and ERROR, by a runtime with
    /// no own channel.
    fn sends(&self, level: &Level) -> bool {
        self.own.is_none() && *level <= SENT_LEVEL
    }
}

impl Subscriber for GuestLogSubscriber {
    fn max_level_hint(&self) -> Option<LevelFilter> {
        Some(match self.own {
            Some(own) => own.max_level.into(),
            None => SENT_LEVEL.into(),
        })
    }

    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        self.sends(metadata.level()) || self.own_takes(metadata.level()).is_some()
    }

    fn new_span(&self, _span: &span::Attributes<'_>) -> span::Id {
        span::Id::from_u64(NEXT_SPAN_ID.fetch_add(1, Ordering::Relaxed))
    }

    fn record(&self, _span: &span::Id, _values: &span::Record<'_>) {}

    fn record_follows_from(&self, _span: &span::Id, _follows: &span::Id) {}

    fn event(&self, event: &Event<'_>) {
        let metadata = event.metadata();
        if let Some(own) = self.own_takes(metadata.level()) {
            (own.write)(event);
            return;
        }
        if !self.sends(metadata.level()) {
            return;
        }
        let mut fields = Fields::default();
        event.record(&mut fields);
        self.send(Some(GuestLogRecord {
            level: level_code(metadata.level()),
            target: metadata.target().to_owned(),
            message: fields.0,
        }));
    }

    fn enter(&self, _span: &span::Id) {}

    fn exit(&self, _span: &span::Id) {}
}

/// An event's fields as `tracing_subscriber`'s default formatter prints them:
/// the message as written, every other field as `name=value` with the value's
/// `Debug` form, separated by spaces.
#[derive(Default)]
struct Fields(String);

impl Fields {
    fn separate(&mut self) {
        if !self.0.is_empty() {
            self.0.push(' ');
        }
    }
}

impl Visit for Fields {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.separate();
        if field.name() == "message" {
            self.0.push_str(value);
        } else {
            let _ = write!(self.0, "{}={value:?}", field.name());
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.separate();
        if field.name() == "message" {
            let _ = write!(self.0, "{value:?}");
        } else {
            let _ = write!(self.0, "{}={value:?}", field.name());
        }
    }
}

/// A callsite the coordinator makes for one level and target of guest records,
/// so its own subscriber filters and prints them as events of that target.
struct GuestCallsite {
    metadata: OnceLock<Metadata<'static>>,
}

impl Callsite for GuestCallsite {
    fn set_interest(&self, _interest: Interest) {}

    fn metadata(&self) -> &Metadata<'_> {
        self.metadata
            .get()
            .expect("a guest callsite's metadata is set before it is registered")
    }
}

/// The most distinct (level, target) callsites the coordinator makes; records
/// beyond them are printed under this module's own target, naming theirs.
const MAX_CALLSITES: usize = 1024;

/// The callsite for guest records of `level` and `target`. Each is made once and
/// lives for the run, as a callsite must.
fn callsite(level: Level, target: &str) -> Option<&'static GuestCallsite> {
    static CALLSITES: Mutex<BTreeMap<(u8, String), &'static GuestCallsite>> =
        Mutex::new(BTreeMap::new());
    let mut callsites = CALLSITES
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let key = (level_code(&level), target.to_owned());
    if let Some(callsite) = callsites.get(&key) {
        return Some(callsite);
    }
    if callsites.len() >= MAX_CALLSITES {
        return None;
    }
    let callsite: &'static GuestCallsite = Box::leak(Box::new(GuestCallsite {
        metadata: OnceLock::new(),
    }));
    let target: &'static str = Box::leak(target.to_owned().into_boxed_str());
    let _ = callsite.metadata.set(Metadata::new(
        "guest event",
        target,
        level,
        None,
        None,
        None,
        FieldSet::new(&["message"], tracing::callsite::Identifier(callsite)),
        Kind::EVENT,
    ));
    tracing::callsite::register(callsite);
    callsites.insert(key, callsite);
    Some(callsite)
}

/// Emits one guest record in the coordinator, at its own level and target, after
/// a warning naming the records of the sender's process that were lost before
/// it (`unsent_before`).
pub(crate) fn emit(record: Option<&GuestLogRecord>, unsent_before: u64) {
    if unsent_before > 0 {
        tracing::warn!(
            "{} Detcore warning(s) from inside the guest could not be sent to the coordinator and are missing",
            unsent_before
        );
    }
    let Some(record) = record else {
        return;
    };
    let Some(level) = level_from_code(record.level) else {
        return;
    };
    if level > LevelFilter::current() {
        return;
    }
    let Some(callsite) = callsite(level, &record.target) else {
        tracing::warn!(
            "(guest record of {} {}, past the callsite limit) {}",
            level,
            record.target,
            record.message
        );
        return;
    };
    let metadata = callsite.metadata();
    tracing::dispatcher::get_default(|dispatch| {
        if !dispatch.enabled(metadata) {
            return;
        }
        let fields = metadata.fields();
        let Some(message) = fields.field("message") else {
            return;
        };
        let text = format_args!("{}", record.message);
        let values = [(&message, Some(&text as &dyn Value))];
        dispatch.event(&Event::new(metadata, &fields.value_set(&values)));
    });
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::tool_global::GlobalRequest;
    use crate::tool_global::GlobalResponse;

    /// Each event a subscriber saw: its level, target and fields as
    /// [`Fields`] reads them.
    #[derive(Default)]
    struct Capture(Mutex<Vec<(Level, String, String)>>);

    impl Subscriber for Capture {
        fn enabled(&self, _metadata: &Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _span: &span::Attributes<'_>) -> span::Id {
            span::Id::from_u64(1)
        }
        fn record(&self, _span: &span::Id, _values: &span::Record<'_>) {}
        fn record_follows_from(&self, _span: &span::Id, _follows: &span::Id) {}
        fn event(&self, event: &Event<'_>) {
            let mut fields = Fields::default();
            event.record(&mut fields);
            let metadata = event.metadata();
            self.0.lock().unwrap().push((
                *metadata.level(),
                metadata.target().to_owned(),
                fields.0,
            ));
        }
        fn enter(&self, _span: &span::Id) {}
        fn exit(&self, _span: &span::Id) {}
    }

    #[test]
    fn level_codes_round_trip() {
        for level in [
            Level::ERROR,
            Level::WARN,
            Level::INFO,
            Level::DEBUG,
            Level::TRACE,
        ] {
            assert_eq!(level_from_code(level_code(&level)), Some(level));
        }
        assert_eq!(level_from_code(0), None);
    }

    /// The fields read as the default formatter prints them: the message as
    /// written, then each other field as `name=value` in `Debug` form.
    #[test]
    fn fields_read_as_the_default_formatter_prints_them() {
        let capture = Arc::new(Capture::default());
        tracing::subscriber::with_default(capture.clone(), || {
            tracing::warn!("cpuid leaf 0x{:x} not in the table", 0x4ffffff0);
            tracing::warn!(fd = 3, path = "a b", "cannot restore");
        });
        let fields: Vec<_> = capture
            .0
            .lock()
            .unwrap()
            .iter()
            .map(|event| event.2.clone())
            .collect();
        assert_eq!(
            fields,
            [
                "cpuid leaf 0x4ffffff0 not in the table",
                "cannot restore fd=3 path=\"a b\""
            ]
        );
    }

    /// The coordinator prints a guest record at its own level and target, so
    /// its filter treats it as that module's event, after a warning that
    /// counts the records of that process lost before it; a request with only
    /// a count prints only that warning.
    #[test]
    fn the_coordinator_emits_a_record_at_its_level_and_target_after_its_losses() {
        let capture = Arc::new(Capture::default());
        tracing::subscriber::with_default(capture.clone(), || {
            emit(
                Some(&GuestLogRecord {
                    level: 2,
                    target: "detcore::syscalls::files".to_owned(),
                    message: "cannot restore fd=3".to_owned(),
                }),
                2,
            );
            emit(None, 1);
            emit(None, 0);
        });
        let events = capture.0.lock().unwrap();
        assert_eq!(events.len(), 3, "{events:?}");
        let ours = module_path!().trim_end_matches("::tests");
        assert_eq!((events[0].0, events[0].1.as_str()), (Level::WARN, ours));
        assert!(
            events[0]
                .2
                .starts_with("2 Detcore warning(s) from inside the guest"),
            "{events:?}"
        );
        assert_eq!(
            events[1],
            (
                Level::WARN,
                "detcore::syscalls::files".to_owned(),
                "cannot restore fd=3".to_owned()
            )
        );
        assert!(
            events[2].2.starts_with("1 Detcore warning(s)"),
            "{events:?}"
        );
    }

    /// What the fake coordinator call received: each record and count, and
    /// whether it refused. It refuses its first call.
    static SENT: Mutex<Vec<(Option<GuestLogRecord>, u64, bool)>> = Mutex::new(Vec::new());

    fn fake_call(
        request: <crate::GlobalState as reverie::GlobalTool>::Request,
    ) -> io::Result<<crate::GlobalState as reverie::GlobalTool>::Response> {
        let GlobalRequest::GuestLog(record, unsent) = request.2 else {
            panic!("the guest log sends only GuestLog requests: {request:?}");
        };
        let mut sent = SENT.lock().unwrap();
        let refuse = sent.is_empty();
        sent.push((record, unsent, refuse));
        if refuse {
            Err(io::Error::other("refused"))
        } else {
            Ok((None, GlobalResponse::GuestLog(())))
        }
    }

    /// Held by each test that uses the process-wide count, so that tests run
    /// as threads of one process cannot interleave on it.
    static COUNT_TEST_LOCK: Mutex<()> = Mutex::new(());

    /// In the guest, through the subscriber a runtime installs: a warning that
    /// cannot be sent is counted and carried by the next request that gets
    /// through; INFO, DEBUG and TRACE events are never sent; and the
    /// exit_group flush sends a count still held.
    #[test]
    fn the_guest_sends_warnings_counts_what_it_could_not_send_and_flushes_at_exit() {
        let _count = COUNT_TEST_LOCK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        use crate::types::DetTid;
        use crate::types::MmId;
        let root = MmId::initial(DetTid::from_raw(3));
        let child = MmId::initial(DetTid::from_raw(7));
        // Outside an in-guest runtime's subscriber, a process start leaves the
        // count alone.
        UNSENT.store(5, Ordering::Release);
        starts_new_process(root);
        starts_new_process(child);
        assert_eq!(UNSENT.load(Ordering::Acquire), 5);
        let subscriber = subscriber(fake_call, None);
        tracing::subscriber::with_default(subscriber, || {
            // A count held before the root process's first thread starts is
            // kept, as is one when another process's thread starts in the same
            // memory (a vfork child); a forked child's copy starts at zero.
            *UNSENT_OWNER.lock().unwrap() = None;
            starts_new_process(root);
            starts_new_process(root);
            assert_eq!(UNSENT.load(Ordering::Acquire), 5);
            starts_new_process(child);
            assert_eq!(UNSENT.load(Ordering::Acquire), 0);
            starts_new_process(root);
            tracing::warn!("first");
            tracing::error!(target: "detcore::fifo_peer", "second");
            tracing::info!("not sent");
            tracing::debug!("not sent");
            tracing::trace!("not sent");
            assert!(!tracing::enabled!(Level::INFO));
            UNSENT.store(3, Ordering::Release);
            flush_unsent();
            flush_unsent();
        });
        let sent = SENT.lock().unwrap();
        let summary: Vec<_> = sent
            .iter()
            .map(|(record, unsent, refused)| {
                (
                    record
                        .as_ref()
                        .map(|r| (r.level, r.target.clone(), r.message.clone())),
                    *unsent,
                    *refused,
                )
            })
            .collect();
        assert_eq!(
            summary,
            [
                (
                    Some((2, module_path!().to_owned(), "first".to_owned())),
                    0,
                    true
                ),
                (
                    Some((1, "detcore::fifo_peer".to_owned(), "second".to_owned())),
                    1,
                    false
                ),
                (None, 3, false),
            ]
        );
        assert_eq!(UNSENT.load(Ordering::Acquire), 0);
    }

    /// What the runtime's own channel received.
    static OWN: Mutex<Vec<(Level, String)>> = Mutex::new(Vec::new());

    fn own_write(event: &Event<'_>) {
        let mut fields = Fields::default();
        event.record(&mut fields);
        OWN.lock()
            .unwrap()
            .push((*event.metadata().level(), fields.0));
    }

    fn refusing_call(
        _request: <crate::GlobalState as reverie::GlobalTool>::Request,
    ) -> io::Result<<crate::GlobalState as reverie::GlobalTool>::Response> {
        panic!("an event the runtime's own channel takes is not sent");
    }

    /// The runtime's own channel takes every event up to its level, DETLOG or
    /// not, in order, and nothing above it; and a subscriber with an own
    /// channel sends nothing, also above that level (at ERROR, a WARN).
    #[test]
    fn the_own_channel_takes_every_event_up_to_its_level_and_nothing_is_sent() {
        let own = OwnChannel {
            max_level: Level::INFO,
            write: own_write,
        };
        tracing::subscriber::with_default(subscriber(refusing_call, Some(own)), || {
            tracing::error!("an error");
            tracing::warn!("a warning");
            tracing::info!("DETLOG a record");
            tracing::info!("an info");
            tracing::debug!("a debug");
            tracing::trace!("a trace");
        });
        assert_eq!(
            *OWN.lock().unwrap(),
            [
                (Level::ERROR, "an error".to_owned()),
                (Level::WARN, "a warning".to_owned()),
                (Level::INFO, "DETLOG a record".to_owned()),
                (Level::INFO, "an info".to_owned()),
            ]
        );
        let errors_only = OwnChannel {
            max_level: Level::ERROR,
            write: own_write,
        };
        tracing::subscriber::with_default(subscriber(refusing_call, Some(errors_only)), || {
            tracing::warn!("above the run's level");
            assert!(!tracing::enabled!(Level::WARN));
        });
        assert_eq!(OWN.lock().unwrap().len(), 4);
    }

    /// A Tool exit callback's connection to the global state: what it was
    /// sent, and whether it answers as the coordinator does.
    struct ExitHookRpc {
        config: crate::Config,
        answers: bool,
        sent: Mutex<Vec<GlobalRequest>>,
    }

    #[reverie::tool]
    impl reverie::GlobalRPC<crate::GlobalState> for ExitHookRpc {
        async fn send_rpc(
            &self,
            message: <crate::GlobalState as reverie::GlobalTool>::Request,
        ) -> <crate::GlobalState as reverie::GlobalTool>::Response {
            self.sent.lock().unwrap().push(message.2);
            if self.answers {
                (None, GlobalResponse::GuestLog(()))
            } else {
                (None, GlobalResponse::Retired)
            }
        }

        fn config(&self) -> &crate::Config {
            &self.config
        }
    }

    /// The exit hook's flush sends a held count through the hook's own
    /// connection and clears it; a count the coordinator does not take back
    /// is kept; with nothing held, nothing is sent.
    #[test]
    fn the_exit_hook_sends_a_held_count_through_its_own_connection() {
        let _count = COUNT_TEST_LOCK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let rpc = |answers| ExitHookRpc {
            config: Default::default(),
            answers,
            sent: Mutex::new(Vec::new()),
        };
        UNSENT.store(0, Ordering::Release);
        let idle = rpc(true);
        futures::executor::block_on(flush_unsent_through(&idle));
        assert!(idle.sent.lock().unwrap().is_empty());

        UNSENT.store(4, Ordering::Release);
        let refusing = rpc(false);
        futures::executor::block_on(flush_unsent_through(&refusing));
        assert_eq!(
            *refusing.sent.lock().unwrap(),
            [GlobalRequest::GuestLog(None, 4)]
        );
        assert_eq!(UNSENT.load(Ordering::Acquire), 4);

        let answering = rpc(true);
        futures::executor::block_on(flush_unsent_through(&answering));
        assert_eq!(
            *answering.sent.lock().unwrap(),
            [GlobalRequest::GuestLog(None, 4)]
        );
        assert_eq!(UNSENT.load(Ordering::Acquire), 0);
    }
}
