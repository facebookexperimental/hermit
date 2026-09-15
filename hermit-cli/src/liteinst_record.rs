//! Bounded complete records using stock tracing field formatting.
//!
//! The event envelope matches tracing-subscriber's default Full format with
//! ANSI disabled. Its public DefaultFields visitor handles fields unchanged.
//! This Layer owns per-call event storage and bounded span caches, avoiding the
//! fmt Layer's destructible TLS output buffer and unbounded initial span cache.
//! A failed update preserves the previous cache; errors poison later records.
//! A sink accepts one complete record or returns an error and is never retried.
//!
//! This does not make Registry or EnvFilter teardown-safe: creating/entering
//! spans or using dynamic span filters after their TLS teardown remains subject
//! to those dependencies' lifetime requirements. A contextual event after
//! Registry TLS destruction can lose its current span without a local error;
//! the public Context API cannot distinguish this from having no entered span.
//! Such emissions must not be treated as complete-log evidence. This module
//! provides neither capture completion nor deterministic record ordering.
//! This preparation is not wired into backend verification.

use std::cell::Cell;
use std::fmt;
use std::io;
use std::io::Write;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;

use tracing::Event;
use tracing::Subscriber;
use tracing::span::Attributes;
use tracing::span::Id;
use tracing::span::Record;
use tracing_log::NormalizeEvent;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::Layer;
use tracing_subscriber::field::RecordFields;
use tracing_subscriber::fmt::FormattedFields;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::fmt::format::DefaultFields;
use tracing_subscriber::fmt::format::FormatFields;
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::time::FormatTime;
use tracing_subscriber::fmt::time::SystemTime;
use tracing_subscriber::layer::Context;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::registry::LookupSpan;

/// Provisional per-event payload budget, not derived from measured workloads.
/// Measure complete INFO record sizes before selecting production limits or
/// wiring this preparation into backend verification.
pub const EVENT_BYTES: usize = 1024 * 1024;
/// Provisional per-span payload budget, not derived from measured workloads.
/// Each successfully formatted span retains the full reserved capacity until
/// its cache is replaced or the span closes, even when its fields are short.
/// Measure field sizes and live-span memory costs before production wiring.
pub const SPAN_BYTES: usize = 64 * 1024;

/// Formatter payload budgets, not a bound on callback allocations, registry or
/// filter bookkeeping, allocator overhead, live spans or concurrent emitters.
/// Each formatting call reserves its full event or span budget fallibly. Span
/// caches retain that allocation, including unused capacity; an update briefly
/// holds both the old cache and its replacement. An allocator may reserve more
/// than requested, but only the configured payload limit may be written.
#[derive(Clone, Copy, Debug)]
pub struct FormatterLimits {
    event_bytes: usize,
    span_bytes: usize,
}

impl FormatterLimits {
    pub fn new(event_bytes: usize, span_bytes: usize) -> io::Result<Self> {
        let envelope = span_bytes
            .checked_mul(2)
            .and_then(|spans| spans.checked_add(event_bytes))
            .and_then(|bytes| bytes.checked_add(8));
        if event_bytes == 0
            || span_bytes == 0
            || envelope.is_none_or(|bytes| bytes > isize::MAX as usize)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid formatter limits",
            ));
        }
        Ok(Self {
            event_bytes,
            span_bytes,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BufferKind {
    Event,
    Span,
}

/// A sticky failure, not a capture-completion or deterministic-order verdict.
#[derive(Debug)]
pub enum RecordFailure {
    Formatting,
    Sink(io::Error),
    Reentrant,
    Unwinding,
    Size { buffer: BufferKind, limit: usize },
    Allocation,
}

impl fmt::Display for RecordFailure {
    fn fmt(&self, writer: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Formatting => writer.write_str("host record formatting failed"),
            Self::Sink(error) => write!(writer, "host record sink failed: {error}"),
            Self::Reentrant => writer.write_str("reentrant host record publication"),
            Self::Unwinding => writer.write_str("host record formatting or publication unwound"),
            Self::Size { buffer, limit } => {
                write!(writer, "{buffer:?} formatter exceeded {limit} bytes")
            }
            Self::Allocation => writer.write_str("formatter storage allocation failed"),
        }
    }
}

impl std::error::Error for RecordFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Sink(error) => Some(error),
            _ => None,
        }
    }
}

/// Retain this independently of the subscriber, including through its Drop.
///
/// This reports formatter/publication failures, not Registry or EnvFilter
/// lifecycle completeness. In particular, losing Registry's current-span TLS
/// can omit context without setting this status. `None` cannot certify a
/// complete log; the caller must establish those dependency lifetimes too.
/// Sink calls already in flight may finish after another emitter fails. This
/// status is not a publication barrier: quiesce all emitters before checking
/// the final result, and refuse comparison if any failure was recorded.
#[derive(Clone, Default)]
pub struct RecordStatus(
    Arc<Mutex<Option<Arc<RecordFailure>>>>,
    Arc<OnceLock<Box<dyn Fn() + Send + Sync>>>,
);

impl RecordStatus {
    pub fn failure(&self) -> Option<Arc<RecordFailure>> {
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    fn fail(&self, failure: RecordFailure) {
        let mut stored = self.0.lock().unwrap_or_else(|error| error.into_inner());
        if stored.is_none() {
            *stored = Some(Arc::new(failure));
            drop(stored);
            if let Some(notify) = self.1.get() {
                notify();
            }
        } else {
            drop(stored);
            drop(failure);
        }
    }

    fn formatting(&self, action: impl FnOnce() -> fmt::Result) -> fmt::Result {
        let attempt = Attempt(Some(self));
        let result = action();
        attempt.complete();
        if result.is_err() {
            self.fail(RecordFailure::Formatting);
        }
        result
    }
}

struct Attempt<'status>(Option<&'status RecordStatus>);

impl Attempt<'_> {
    fn complete(mut self) {
        // A successful callback may run from Drop during an unrelated panic.
        // Only unwinding out of this callback is a formatter or sink failure.
        self.0 = None;
    }
}

impl Drop for Attempt<'_> {
    fn drop(&mut self) {
        if let Some(status) = self.0
            && std::thread::panicking()
        {
            status.fail(RecordFailure::Unwinding);
        }
    }
}

struct BoundedBuffer<'status> {
    bytes: Vec<u8>,
    limit: usize,
    failed: bool,
    kind: BufferKind,
    status: &'status RecordStatus,
}

fn reserve_payload(
    bytes: &mut Vec<u8>,
    limit: usize,
) -> Result<(), std::collections::TryReserveError> {
    #[cfg(test)]
    tests::before_payload_allocation()?;
    bytes.try_reserve_exact(limit)
}

impl<'status> BoundedBuffer<'status> {
    fn new(
        limit: usize,
        kind: BufferKind,
        status: &'status RecordStatus,
    ) -> Result<Self, fmt::Error> {
        let mut bytes = Vec::new();
        if reserve_payload(&mut bytes, limit).is_err() {
            status.fail(RecordFailure::Allocation);
            return Err(fmt::Error);
        }
        Ok(Self {
            bytes,
            limit,
            failed: false,
            kind,
            status,
        })
    }

    fn text(&self) -> &str {
        std::str::from_utf8(&self.bytes).expect("only complete UTF-8 strings were copied")
    }

    fn into_string(self) -> String {
        String::from_utf8(self.bytes).expect("only complete UTF-8 strings were copied")
    }
}

impl fmt::Write for BoundedBuffer<'_> {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        if self.failed {
            return Err(fmt::Error);
        }
        if self
            .bytes
            .len()
            .checked_add(text.len())
            .is_none_or(|end| end > self.limit)
        {
            self.failed = true;
            self.status.fail(RecordFailure::Size {
                buffer: self.kind,
                limit: self.limit,
            });
            return Err(fmt::Error);
        }
        // The full payload budget was reserved fallibly before formatting;
        // appending within that limit cannot allocate or touch unused bytes.
        self.bytes.extend_from_slice(text.as_bytes());
        Ok(())
    }
}

// Cell has no destructor. Each list entry belongs to a live call's stack,
// rather than a Vec whose TLS destructor can run before late logging.
thread_local! {
    static ACTIVE: Cell<*const Active> = const { Cell::new(std::ptr::null()) };
}

struct Active {
    identity: usize,
    previous: *const Active,
}

struct RestoreActive(*const Active);

impl Drop for RestoreActive {
    fn drop(&mut self) {
        ACTIVE.set(self.0);
    }
}

fn with_active<T>(
    status: &RecordStatus,
    action: impl FnOnce() -> Result<T, fmt::Error>,
) -> Result<T, fmt::Error> {
    if status.failure().is_some() {
        return Err(fmt::Error);
    }
    let identity = Arc::as_ptr(&status.0) as usize;
    let previous = ACTIVE.get();
    let mut current = previous;
    while !current.is_null() {
        // SAFETY: ACTIVE contains only entries in enclosing with_active calls
        // on this thread. Each entry stays in its stack slot until action has
        // returned or unwound; RestoreActive unlinks it before that slot dies.
        let active = unsafe { &*current };
        if active.identity == identity {
            status.fail(RecordFailure::Reentrant);
            return Err(fmt::Error);
        }
        current = active.previous;
    }
    let active = Active { identity, previous };
    ACTIVE.set(&raw const active);
    let restore = RestoreActive(previous);
    let result = action();
    drop(restore);
    result
}

// Keep the standard cache wrapper; only this Layer writes its payload.
struct CheckedFields;

// Extensions are keyed by Rust type, so two RecordLayers cannot each insert a
// FormattedFields<CheckedFields>. Retain a separate entry for each layer instead.
// The Arc stays in the cache as well as the layer: replacing a layer while a
// span lives cannot reuse an old allocation identity and inherit stale fields.
#[derive(Default)]
struct SpanCaches(Vec<(Arc<()>, FormattedFields<CheckedFields>)>);

impl SpanCaches {
    fn get(&self, identity: &Arc<()>) -> Option<&FormattedFields<CheckedFields>> {
        self.0
            .iter()
            .find(|(owner, _)| Arc::ptr_eq(owner, identity))
            .map(|(_, fields)| fields)
    }

    fn get_or_insert(&mut self, identity: &Arc<()>) -> (&mut FormattedFields<CheckedFields>, bool) {
        if let Some(index) = self
            .0
            .iter()
            .position(|(owner, _)| Arc::ptr_eq(owner, identity))
        {
            return (&mut self.0[index].1, true);
        }
        self.0.push((
            identity.clone(),
            FormattedFields::<CheckedFields>::new(String::new()),
        ));
        (&mut self.0.last_mut().unwrap().1, false)
    }
}

struct RecordLayer<Timer, Sink> {
    identity: Arc<()>,
    timer: Timer,
    timestamp: bool,
    limits: FormatterLimits,
    status: RecordStatus,
    writer: RecordWriterFactory<Sink>,
}

impl<Timer, Sink> RecordLayer<Timer, Sink> {
    fn format_span<Fields: RecordFields>(
        &self,
        current: &mut FormattedFields<CheckedFields>,
        fields: Fields,
        append: bool,
    ) -> fmt::Result {
        use fmt::Write as _;

        let mut buffer =
            BoundedBuffer::new(self.limits.span_bytes, BufferKind::Span, &self.status)?;
        if append {
            buffer.write_str(&current.fields)?;
            if !current.fields.is_empty() {
                buffer.write_char(' ')?;
            }
        }
        self.status
            .formatting(|| DefaultFields::new().format_fields(Writer::new(&mut buffer), fields))?;
        if self.status.failure().is_some() {
            return Err(fmt::Error);
        }
        current.fields = buffer.into_string();
        Ok(())
    }
}

impl<Registry, Timer, Sink> Layer<Registry> for RecordLayer<Timer, Sink>
where
    Registry: Subscriber + for<'lookup> LookupSpan<'lookup>,
    Timer: FormatTime + 'static,
    Sink: Fn(&[u8]) -> io::Result<()> + 'static,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, context: Context<'_, Registry>) {
        let span = context.span(id).expect("new span is registered");
        {
            let mut extensions = span.extensions_mut();
            if extensions.get_mut::<SpanCaches>().is_none() {
                extensions.insert(SpanCaches::default());
            }
            extensions
                .get_mut::<SpanCaches>()
                .unwrap()
                .get_or_insert(&self.identity);
        }
        let _ = with_active(&self.status, || {
            let mut extensions = span.extensions_mut();
            let (current, _) = extensions
                .get_mut::<SpanCaches>()
                .unwrap()
                .get_or_insert(&self.identity);
            self.format_span(current, attrs, false)
        });
    }

    fn on_record(&self, id: &Id, fields: &Record<'_>, context: Context<'_, Registry>) {
        let _ = with_active(&self.status, || {
            let span = context.span(id).expect("recorded span is registered");
            let mut extensions = span.extensions_mut();
            if extensions.get_mut::<SpanCaches>().is_none() {
                extensions.insert(SpanCaches::default());
            }
            let (current, append) = extensions
                .get_mut::<SpanCaches>()
                .unwrap()
                .get_or_insert(&self.identity);
            self.format_span(current, fields, append)
        });
    }

    fn on_event(&self, event: &Event<'_>, context: Context<'_, Registry>) {
        let record = with_active(&self.status, || {
            let mut buffer =
                BoundedBuffer::new(self.limits.event_bytes, BufferKind::Event, &self.status)?;
            self.status.formatting(|| {
                let mut writer = Writer::new(&mut buffer);
                // Match stock Full's non-ANSI timestamp fallback and padding.
                if self.timestamp {
                    if self.timer.format_time(&mut writer).is_err() {
                        writer.write_str("<unknown time>")?;
                    }
                    writer.write_char(' ')?;
                }
                let normalized = event.normalized_metadata();
                let metadata = normalized.as_ref().unwrap_or_else(|| event.metadata());
                write!(writer, "{:>5} ", metadata.level())?;
                if let Some(scope) = context.event_scope(event) {
                    let mut seen = false;
                    for span in scope.from_root() {
                        writer.write_str(span.metadata().name())?;
                        let extensions = span.extensions();
                        if let Some(fields) = extensions
                            .get::<SpanCaches>()
                            .and_then(|caches| caches.get(&self.identity))
                            && !fields.is_empty()
                        {
                            write!(writer, "{{{fields}}}")?;
                        }
                        writer.write_char(':')?;
                        seen = true;
                    }
                    if seen {
                        writer.write_char(' ')?;
                    }
                }
                write!(writer, "{}: ", metadata.target())?;
                DefaultFields::new().format_fields(writer.by_ref(), event)?;
                writeln!(writer)
            })?;
            if self.status.failure().is_some() {
                return Err(fmt::Error);
            }
            Ok(buffer)
        });
        if let Ok(record) = record {
            let _ = self
                .writer
                .write_record(record.text().as_bytes(), self.limits.event_bytes);
        }
    }
}

struct RecordWriter<'sink, Sink> {
    sink: &'sink Sink,
    status: &'sink RecordStatus,
}

impl<Sink: Fn(&[u8]) -> io::Result<()>> Write for RecordWriter<'_, Sink> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.write_all(bytes).map(|()| bytes.len())
    }

    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        with_active(self.status, || {
            let attempt = Attempt(Some(self.status));
            let result = (self.sink)(bytes);
            attempt.complete();
            match result {
                Ok(()) => Ok(()),
                Err(error) => {
                    self.status.fail(RecordFailure::Sink(error));
                    Err(fmt::Error)
                }
            }
        })
        .map_err(|_| io::Error::other("complete record publication failed"))
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct RecordWriterFactory<Sink> {
    sink: Sink,
    status: RecordStatus,
}

impl<Sink: Fn(&[u8]) -> io::Result<()>> RecordWriterFactory<Sink> {
    fn write_record(&self, bytes: &[u8], limit: usize) -> io::Result<()> {
        if bytes.len() > limit {
            self.status.fail(RecordFailure::Size {
                buffer: BufferKind::Event,
                limit,
            });
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "oversized formatted record",
            ));
        }
        self.make_writer().write_all(bytes)
    }
}

impl<'sink, Sink: Fn(&[u8]) -> io::Result<()> + 'sink> MakeWriter<'sink>
    for RecordWriterFactory<Sink>
{
    type Writer = RecordWriter<'sink, Sink>;

    fn make_writer(&'sink self) -> Self::Writer {
        RecordWriter {
            sink: &self.sink,
            status: &self.status,
        }
    }
}

/// Prepare without installing a subscriber or opening a destination.
///
/// `filter` must already be resolved by the caller. Records use the standard
/// Full format with timestamps and generated ANSI disabled. Stock sanitizes
/// the `message` field and error values; arbitrary named Debug fields preserve
/// their original bytes, including escape sequences.
/// Inspect the retained status after emitter quiescence, including teardown;
/// absence of a local failure alone is not a complete-log or parity verdict.
/// The callback is invoked once on the first local failure, after storing it.
///
/// The emitted byte stream is not self-describing on failure: it can be an
/// incomplete prefix with no marker. A consumer feeding it to `detcore::logdiff`
/// must ensure any failure refuses comparison, either through retained failure
/// state or an in-band marker recognized by the comparator as truncation. The
/// bytes alone cannot qualify a comparison. Transport wiring must enforce this
/// before this preparation can be used for backend verification.
pub fn record_subscriber_with_failure<Sink>(
    filter: EnvFilter,
    limits: FormatterLimits,
    sink: Sink,
    failure: impl Fn() + Send + Sync + 'static,
) -> (impl Subscriber + Send + Sync, RecordStatus)
where
    Sink: Fn(&[u8]) -> io::Result<()> + Send + Sync + 'static,
{
    let status = RecordStatus::default();
    let _ = status.1.set(Box::new(failure));
    subscriber_with_status(filter, limits, sink, SystemTime, true, status)
}

/// Prepare a complete-record layer without installing it or selecting events.
///
/// Apply [`Layer::with_filter`] to each layer independently: the public logger
/// can use its resolved [`EnvFilter`] while private evidence uses a fixed INFO
/// selector. A global public filter would also suppress private evidence.
/// Each layer owns its formatter limits, span fields and retained failure state.
/// The completion, teardown and transport requirements of
/// [`record_subscriber_with_failure`] apply equally to this constructor.
pub fn record_layer_with_failure<Registry, Sink>(
    limits: FormatterLimits,
    sink: Sink,
    failure: impl Fn() + Send + Sync + 'static,
) -> (impl Layer<Registry> + Send + Sync, RecordStatus)
where
    Registry: Subscriber + for<'lookup> LookupSpan<'lookup>,
    Sink: Fn(&[u8]) -> io::Result<()> + Send + Sync + 'static,
{
    let status = RecordStatus::default();
    let _ = status.1.set(Box::new(failure));
    (
        layer_with_status(limits, sink, SystemTime, true, status.clone()),
        status,
    )
}

fn subscriber_with_status<Sink, Timer>(
    filter: EnvFilter,
    limits: FormatterLimits,
    sink: Sink,
    timer: Timer,
    timestamp: bool,
    status: RecordStatus,
) -> (impl Subscriber + Send + Sync, RecordStatus)
where
    Sink: Fn(&[u8]) -> io::Result<()> + Send + Sync + 'static,
    Timer: FormatTime + Send + Sync + 'static,
{
    let layer = layer_with_status(limits, sink, timer, timestamp, status.clone());
    (
        tracing_subscriber::registry().with(layer).with(filter),
        status,
    )
}

fn layer_with_status<Sink, Timer>(
    limits: FormatterLimits,
    sink: Sink,
    timer: Timer,
    timestamp: bool,
    status: RecordStatus,
) -> RecordLayer<Timer, Sink> {
    RecordLayer {
        identity: Arc::new(()),
        timer,
        timestamp,
        limits,
        status: status.clone(),
        writer: RecordWriterFactory {
            sink,
            status: status.clone(),
        },
    }
}

#[cfg(test)]
mod tests;
