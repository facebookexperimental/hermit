use std::cell::RefCell;
use std::panic::AssertUnwindSafe;
use std::sync::Barrier;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use super::*;

type Records = Arc<Mutex<Vec<Vec<u8>>>>;

fn baseline_limits() -> FormatterLimits {
    FormatterLimits::new(4096, 1024).unwrap()
}

fn record_subscriber<Sink>(
    filter: EnvFilter,
    ansi: bool,
    sink: Sink,
) -> (impl Subscriber + Send + Sync, RecordStatus)
where
    Sink: Fn(&[u8]) -> io::Result<()> + Send + Sync + 'static,
{
    assert!(!ansi, "complete records have canonical non-ANSI formatting");
    super::record_subscriber_with_failure(filter, baseline_limits(), sink, || {})
}

struct TestFormat<Timer = SystemTime> {
    timer: Timer,
    timestamp: bool,
}
fn format() -> TestFormat {
    TestFormat {
        timer: SystemTime,
        timestamp: true,
    }
}
impl<Timer> TestFormat<Timer> {
    fn with_timer<T>(self, timer: T) -> TestFormat<T> {
        TestFormat {
            timer,
            timestamp: self.timestamp,
        }
    }
    fn without_time(self) -> TestFormat<()> {
        TestFormat {
            timer: (),
            timestamp: false,
        }
    }
}

fn subscriber_with_format<Sink, Timer>(
    filter: EnvFilter,
    ansi: bool,
    sink: Sink,
    formatter: TestFormat<Timer>,
) -> (impl Subscriber + Send + Sync, RecordStatus)
where
    Sink: Fn(&[u8]) -> io::Result<()> + Send + Sync + 'static,
    Timer: FormatTime + Send + Sync + 'static,
{
    assert!(!ansi);
    super::subscriber_with_status(
        filter,
        baseline_limits(),
        sink,
        formatter.timer,
        formatter.timestamp,
        RecordStatus::default(),
    )
}

fn filter() -> EnvFilter {
    EnvFilter::new("host_record=info").add_directive(tracing::metadata::LevelFilter::OFF.into())
}

#[test]
fn failed_complete_record_notifies_transport_once_before_returning() {
    let records = Records::default();
    let notifications = Arc::new(AtomicUsize::new(0));
    let seen = notifications.clone();
    let (subscriber, status) = super::record_subscriber_with_failure(
        filter(),
        baseline_limits(),
        capture(&records),
        move || {
            seen.fetch_add(1, Ordering::SeqCst);
        },
    );
    tracing::subscriber::with_default(subscriber, || {
        tracing::info!(target: "host_record", broken = ?BrokenFormat);
        assert_eq!(notifications.load(Ordering::SeqCst), 1);
        tracing::info!(target: "host_record", "refused-after-failure");
    });
    assert!(status.failure().is_some());
    assert_eq!(notifications.load(Ordering::SeqCst), 1);
    assert!(records.lock().unwrap().is_empty());
}

fn capture(records: &Records) -> impl Fn(&[u8]) -> io::Result<()> + Send + Sync + 'static {
    let records = records.clone();
    move |bytes| {
        records.lock().unwrap().push(bytes.to_vec());
        Ok(())
    }
}

#[derive(Debug, Clone, Copy)]
struct FixedTime;

impl tracing_subscriber::fmt::time::FormatTime for FixedTime {
    fn format_time(&self, writer: &mut Writer<'_>) -> fmt::Result {
        writer.write_str("unchanged-time")
    }
}

struct Chunks;

impl fmt::Debug for Chunks {
    fn fmt(&self, writer: &mut fmt::Formatter<'_>) -> fmt::Result {
        writer.write_str("first")?;
        writer.write_str("\nsecond")?;
        writer.write_str("\x1b[31mthird")
    }
}

fn structured_events() {
    let span =
        tracing::info_span!(target: "host_record", "work", task = 7, later = tracing::field::Empty);
    let _entered = span.enter();
    tracing::info!(target: "host_record", chunks = ?Chunks, number = 1.0, "multi\nline");
    span.record("later", 9);
    tracing::info!(target: "host_record", "after span update");
    tracing::debug!(target: "host_record", "must remain filtered");
    tracing::warn!(target: "other_record", "must remain filtered too");
}

#[test]
fn public_preparation_is_scoped_and_drop_does_not_publish() {
    let records = Records::default();
    let (subscriber, status) = record_subscriber(filter(), false, capture(&records));
    assert!(records.lock().unwrap().is_empty());
    tracing::subscriber::with_default(subscriber, || {
        tracing::info!(target: "host_record", "public host record");
    });
    let records = records.lock().unwrap();
    assert_eq!(records.len(), 1);
    assert!(
        String::from_utf8_lossy(&records[0]).ends_with("INFO host_record: public host record\n")
    );
    assert!(status.failure().is_none());
}

struct BrokenFormat;

impl fmt::Debug for BrokenFormat {
    fn fmt(&self, writer: &mut fmt::Formatter<'_>) -> fmt::Result {
        writer.write_str("partial format must not commit")?;
        Err(fmt::Error)
    }
}

#[test]
fn formatting_error_rejects_partial_and_later_records() {
    let records = Records::default();
    let (subscriber, status) = record_subscriber(filter(), false, capture(&records));
    tracing::subscriber::with_default(subscriber, || {
        tracing::info!(target: "host_record", broken = ?BrokenFormat);
        tracing::info!(target: "host_record", "not silently recovered");
    });
    assert!(matches!(
        status.failure().as_deref(),
        Some(RecordFailure::Formatting)
    ));
    assert!(records.lock().unwrap().is_empty());
}

#[test]
fn span_creation_and_update_errors_are_inspectable() {
    for update in [false, true] {
        let records = Records::default();
        let (subscriber, status) = record_subscriber(filter(), false, capture(&records));
        tracing::subscriber::with_default(subscriber, || {
            if update {
                let span = tracing::info_span!(target: "host_record", "broken", value = tracing::field::Empty);
                span.record("value", tracing::field::debug(BrokenFormat));
                let _entered = span.enter();
                tracing::info!(target: "host_record", "in malformed span");
            } else {
                let span =
                    tracing::info_span!(target: "host_record", "broken", value = ?BrokenFormat);
                let _entered = span.enter();
                tracing::info!(target: "host_record", "in malformed span");
            }
        });
        assert!(matches!(
            status.failure().as_deref(),
            Some(RecordFailure::Formatting)
        ));
        assert!(records.lock().unwrap().is_empty());
    }
}

#[test]
fn partial_sink_error_is_sticky_and_never_retried() {
    struct Partial(Vec<u8>);
    impl Write for Partial {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if self.0.is_empty() {
                self.0.extend_from_slice(&bytes[..3]);
                Ok(3)
            } else {
                Err(io::Error::new(io::ErrorKind::BrokenPipe, "after prefix"))
            }
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    let partial = Arc::new(Mutex::new(Partial(Vec::new())));
    let attempted = Records::default();
    let attempts = Arc::new(AtomicUsize::new(0));
    let sink = partial.clone();
    let calls = attempts.clone();
    let attempted_record = attempted.clone();
    let (subscriber, status) = record_subscriber(filter(), false, move |bytes| {
        calls.fetch_add(1, Ordering::Relaxed);
        attempted_record.lock().unwrap().push(bytes.to_vec());
        sink.lock().unwrap().write_all(bytes)
    });
    tracing::subscriber::with_default(subscriber, || {
        tracing::info!(target: "host_record", "partially accepted");
        tracing::info!(target: "host_record", "no automatic replay");
    });
    assert_eq!(attempts.load(Ordering::Relaxed), 1);
    assert_eq!(partial.lock().unwrap().0.len(), 3);
    assert_eq!(attempted.lock().unwrap().len(), 1);
    assert_eq!(partial.lock().unwrap().0, attempted.lock().unwrap()[0][..3]);
    let failure = status.failure().unwrap();
    let RecordFailure::Sink(error) = failure.as_ref() else {
        panic!("missing sink cause")
    };
    assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    assert_eq!(error.to_string(), "after prefix");
}

#[test]
fn interrupted_record_commit_is_not_retried_like_a_byte_write() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let calls = attempts.clone();
    let (subscriber, status) = record_subscriber(filter(), false, move |_| {
        calls.fetch_add(1, Ordering::Relaxed);
        Err(io::Error::from(io::ErrorKind::Interrupted))
    });
    tracing::subscriber::with_default(subscriber, || {
        tracing::info!(target: "host_record", "uncertain commit");
    });
    assert_eq!(attempts.load(Ordering::Relaxed), 1);
    assert!(
        matches!(status.failure().as_deref(), Some(RecordFailure::Sink(error)) if error.kind() == io::ErrorKind::Interrupted)
    );
}

#[test]
fn unused_writer_flush_and_drop_do_not_commit_or_finalize() {
    let records = Records::default();
    let factory = RecordWriterFactory {
        sink: capture(&records),
        status: RecordStatus::default(),
    };
    {
        let mut writer = factory.make_writer();
        writer.flush().unwrap();
    }
    assert!(records.lock().unwrap().is_empty());
    {
        let mut writer = factory.make_writer();
        writer.write_all(b"complete\nmultiline\n").unwrap();
        writer.flush().unwrap();
    }
    assert_eq!(
        *records.lock().unwrap(),
        [b"complete\nmultiline\n".to_vec()]
    );
}

#[test]
fn concurrent_records_are_distinct_not_a_determinism_claim() {
    let records = Records::default();
    let (subscriber, status) =
        subscriber_with_format(filter(), false, capture(&records), format().without_time());
    let dispatch = tracing::Dispatch::new(subscriber);
    let barrier = Arc::new(Barrier::new(4));
    std::thread::scope(|scope| {
        for producer in 0..4 {
            let barrier = barrier.clone();
            let dispatch = dispatch.clone();
            scope.spawn(move || {
                tracing::dispatcher::with_default(&dispatch, || {
                    barrier.wait();
                    for sequence in 0..16 {
                        tracing::info!(target: "host_record", producer, sequence, "distinct");
                    }
                });
            });
        }
    });
    let records = records.lock().unwrap();
    assert_eq!(records.len(), 64);
    for producer in 0..4 {
        for sequence in 0..16 {
            let expected =
                format!(" INFO host_record: distinct producer={producer} sequence={sequence}\n");
            assert_eq!(
                records
                    .iter()
                    .filter(|record| **record == expected.as_bytes())
                    .count(),
                1
            );
        }
    }
    assert!(status.failure().is_none());
}

#[test]
fn formatting_unwind_is_preserved_and_cannot_publish_stale_buffer() {
    struct Panics;
    impl fmt::Debug for Panics {
        fn fmt(&self, writer: &mut fmt::Formatter<'_>) -> fmt::Result {
            writer.write_str("unfinished")?;
            panic!("format panic")
        }
    }
    let records = Records::default();
    let (subscriber, status) = record_subscriber(filter(), false, capture(&records));
    tracing::subscriber::with_default(subscriber, || {
        assert!(
            std::panic::catch_unwind(|| tracing::info!(target: "host_record", value = ?Panics))
                .is_err()
        );
        tracing::info!(target: "host_record", "must not inherit stale formatting");
    });
    assert!(matches!(
        status.failure().as_deref(),
        Some(RecordFailure::Unwinding)
    ));
    assert!(records.lock().unwrap().is_empty());
}

#[test]
fn sink_unwind_is_preserved_and_status_outlives_subscriber() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let calls = attempts.clone();
    let (subscriber, status) = record_subscriber(filter(), false, move |_| {
        calls.fetch_add(1, Ordering::Relaxed);
        panic!("sink panic")
    });
    assert!(
        std::panic::catch_unwind(AssertUnwindSafe(|| {
            tracing::subscriber::with_default(
                subscriber,
                || tracing::info!(target: "host_record", "panics"),
            );
        }))
        .is_err()
    );
    assert_eq!(attempts.load(Ordering::Relaxed), 1);
    assert!(matches!(
        status.failure().as_deref(),
        Some(RecordFailure::Unwinding)
    ));
}

#[test]
fn reentry_is_rejected_without_holding_a_sink_lock() {
    let status = RecordStatus::default();
    let records = Records::default();
    let sink = capture(&records);
    let nested_status = status.clone();
    let factory = RecordWriterFactory {
        status: status.clone(),
        sink: move |_: &[u8]| {
            let nested = RecordWriterFactory {
                sink: &sink,
                status: nested_status.clone(),
            };
            assert!(nested.make_writer().write_all(b"recursive").is_err());
            Ok(())
        },
    };
    factory.make_writer().write_all(b"outer").unwrap();
    assert!(matches!(
        status.failure().as_deref(),
        Some(RecordFailure::Reentrant)
    ));
    assert!(records.lock().unwrap().is_empty());
    assert!(with_active(&RecordStatus::default(), || Ok(())).is_ok());
}

#[test]
fn real_formatting_reentry_poison_prevents_outer_commit() {
    struct Reenters(RecordStatus);
    impl fmt::Debug for Reenters {
        fn fmt(&self, writer: &mut fmt::Formatter<'_>) -> fmt::Result {
            let nested = RecordWriterFactory {
                status: self.0.clone(),
                sink: |_: &[u8]| panic!("reentrant sink must not be reached"),
            };
            assert!(nested.make_writer().write_all(b"recursive").is_err());
            writer.write_str("outer formatting continued")
        }
    }
    let records = Records::default();
    let (subscriber, status) = record_subscriber(filter(), false, capture(&records));
    tracing::subscriber::with_default(subscriber, || {
        tracing::info!(target: "host_record", value = ?Reenters(status.clone()));
    });
    assert!(matches!(
        status.failure().as_deref(),
        Some(RecordFailure::Reentrant)
    ));
    assert!(records.lock().unwrap().is_empty());
}

#[test]
fn later_sink_error_is_dropped_without_holding_the_status_lock() {
    struct ObserveDrop {
        status: RecordStatus,
        observed: Arc<AtomicUsize>,
    }
    impl fmt::Debug for ObserveDrop {
        fn fmt(&self, writer: &mut fmt::Formatter<'_>) -> fmt::Result {
            writer.write_str("later error")
        }
    }
    impl fmt::Display for ObserveDrop {
        fn fmt(&self, writer: &mut fmt::Formatter<'_>) -> fmt::Result {
            fmt::Debug::fmt(self, writer)
        }
    }
    impl std::error::Error for ObserveDrop {}
    impl Drop for ObserveDrop {
        fn drop(&mut self) {
            assert!(self.status.failure().is_some());
            self.observed.fetch_add(1, Ordering::Relaxed);
        }
    }
    let status = RecordStatus::default();
    let observed = Arc::new(AtomicUsize::new(0));
    status.fail(RecordFailure::Formatting);
    status.fail(RecordFailure::Sink(io::Error::other(ObserveDrop {
        status: status.clone(),
        observed: observed.clone(),
    })));
    assert_eq!(observed.load(Ordering::Relaxed), 1);
    assert!(matches!(
        status.failure().as_deref(),
        Some(RecordFailure::Formatting)
    ));
}

fn bounded_subscriber(
    records: &Records,
    event_bytes: usize,
    span_bytes: usize,
) -> (impl Subscriber + Send + Sync, RecordStatus) {
    super::subscriber_with_status(
        filter(),
        FormatterLimits::new(event_bytes, span_bytes).unwrap(),
        capture(records),
        (),
        false,
        RecordStatus::default(),
    )
}

fn cached_span(span: &tracing::Span) -> (String, usize) {
    span.with_subscriber(|(identity, dispatch)| {
        let registry = dispatch
            .downcast_ref::<tracing_subscriber::Registry>()
            .unwrap();
        let span = registry.span(identity).unwrap();
        let extensions = span.extensions();
        let caches = extensions.get::<SpanCaches>().unwrap();
        assert_eq!(
            caches.0.len(),
            1,
            "this helper inspects a single-layer fixture"
        );
        let state = caches.0[0].1.0.lock().unwrap();
        (state.fields.fields.clone(), state.fields.fields.capacity())
    })
    .unwrap()
}

#[test]
fn formatter_limits_refuse_zero_and_overflow_without_allocating() {
    for (event, span) in [
        (0, 1),
        (1, 0),
        (usize::MAX, 1),
        (1, usize::MAX),
        (isize::MAX as usize, 1),
    ] {
        assert_eq!(
            FormatterLimits::new(event, span).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }
    assert!(FormatterLimits::new(1, 1).is_ok());
}

#[test]
fn fixed_storage_rejects_before_copy_and_stays_poisoned() {
    use std::fmt::Write as _;

    let status = RecordStatus::default();
    let mut buffer = BoundedBuffer::new(3, BufferKind::Event, &status).unwrap();
    assert_eq!(buffer.bytes.len(), 0);
    assert_eq!(buffer.bytes.capacity(), 3);
    let pointer = buffer.bytes.as_ptr();
    buffer.write_str("é").unwrap();
    assert_eq!(buffer.text(), "é");
    assert!(buffer.write_str("é").is_err());
    assert_eq!(buffer.text(), "é");
    assert_eq!(buffer.bytes.len(), 2);
    assert!(buffer.write_str("x").is_err());
    assert_eq!(buffer.bytes, "é".as_bytes());
    assert_eq!(buffer.bytes.capacity(), 3);
    assert_eq!(buffer.bytes.as_ptr(), pointer);
    assert!(matches!(
        status.failure().as_deref(),
        Some(RecordFailure::Size {
            buffer: BufferKind::Event,
            limit: 3
        })
    ));
}

#[test]
fn fixed_storage_moves_to_string_without_capacity_growth() {
    use std::fmt::Write as _;

    for capacity in [1, 3, 8, 31, 1024] {
        let status = RecordStatus::default();
        let mut buffer = BoundedBuffer::new(capacity, BufferKind::Span, &status).unwrap();
        buffer.write_str("x").unwrap();
        let text = buffer.into_string();
        assert_eq!(text, "x");
        assert_eq!(text.capacity(), capacity);
        assert!(status.failure().is_none());
    }
}

#[test]
fn real_event_exact_boundary_and_one_byte_overflow() {
    let expected = b" INFO host_record: exact\n";
    for limit in [expected.len(), expected.len() - 1] {
        let records = Records::default();
        let (subscriber, status) = bounded_subscriber(&records, limit, 32);
        tracing::subscriber::with_default(
            subscriber,
            || tracing::info!(target: "host_record", "exact"),
        );
        if limit == expected.len() {
            assert_eq!(*records.lock().unwrap(), [expected.to_vec()]);
            assert!(status.failure().is_none());
        } else {
            assert!(records.lock().unwrap().is_empty());
            assert!(
                matches!(status.failure().as_deref(), Some(RecordFailure::Size { buffer: BufferKind::Event, limit: found }) if *found == limit)
            );
        }
    }
}

#[test]
fn oversized_single_field_refuses_without_partial_sink_record() {
    let records = Records::default();
    let (subscriber, status) = bounded_subscriber(&records, 32, 32);
    tracing::subscriber::with_default(subscriber, || {
        tracing::info!(target: "host_record", "{}", "x".repeat(256));
        tracing::info!(target: "host_record", "later");
    });
    assert!(records.lock().unwrap().is_empty());
    assert!(matches!(
        status.failure().as_deref(),
        Some(RecordFailure::Size {
            buffer: BufferKind::Event,
            limit: 32
        })
    ));
}

#[test]
fn ignored_chunk_errors_cannot_turn_oversize_into_success() {
    struct Ignores(Arc<AtomicUsize>);
    impl fmt::Debug for Ignores {
        fn fmt(&self, writer: &mut fmt::Formatter<'_>) -> fmt::Result {
            for _ in 0..16 {
                self.0.fetch_add(1, Ordering::Relaxed);
                let _ = writer.write_str("xxxxxxxxxxxxxxxx");
            }
            Ok(())
        }
    }
    let calls = Arc::new(AtomicUsize::new(0));
    let records = Records::default();
    let (subscriber, status) = bounded_subscriber(&records, 64, 32);
    tracing::subscriber::with_default(subscriber, || {
        tracing::info!(target: "host_record", value = ?Ignores(calls.clone()));
    });
    assert_eq!(calls.load(Ordering::Relaxed), 16);
    assert!(records.lock().unwrap().is_empty());
    assert!(matches!(
        status.failure().as_deref(),
        Some(RecordFailure::Size {
            buffer: BufferKind::Event,
            limit: 64
        })
    ));
}

#[test]
fn span_cache_exact_total_bound_and_failed_update_is_unchanged() {
    let records = Records::default();
    let (subscriber, status) = bounded_subscriber(&records, 128, 7);
    tracing::subscriber::with_default(subscriber, || {
        let span = tracing::info_span!(target: "host_record", "small", v = 1);
        let initial = cached_span(&span);
        assert_eq!(initial.0, "v=1");
        assert!(initial.1 <= 8);
        span.record("v", 2);
        assert_eq!(cached_span(&span), ("v=1 v=2".to_owned(), 7));
        let _entered = span.enter();
        tracing::info!(target: "host_record", "at boundary");
        span.record("v", 3);
        assert_eq!(cached_span(&span), ("v=1 v=2".to_owned(), 7));
        tracing::info!(target: "host_record", "must not publish");
    });
    assert_eq!(
        *records.lock().unwrap(),
        [b" INFO small{v=1 v=2}: host_record: at boundary\n".to_vec()]
    );
    assert!(matches!(
        status.failure().as_deref(),
        Some(RecordFailure::Size {
            buffer: BufferKind::Span,
            limit: 7
        })
    ));
}

#[test]
fn span_update_counts_separator_before_appending() {
    let records = Records::default();
    let (subscriber, status) = bounded_subscriber(&records, 128, 3);
    tracing::subscriber::with_default(subscriber, || {
        let span = tracing::info_span!(target: "host_record", "small", v = 1);
        let before = cached_span(&span);
        span.record("v", 2);
        assert_eq!(cached_span(&span), before);
    });
    assert!(matches!(
        status.failure().as_deref(),
        Some(RecordFailure::Size {
            buffer: BufferKind::Span,
            limit: 3
        })
    ));
    assert!(records.lock().unwrap().is_empty());
}

#[test]
fn oversized_initial_span_and_updates_never_keep_partial_bytes() {
    for update in [false, true] {
        let records = Records::default();
        let (subscriber, status) = bounded_subscriber(&records, 128, 8);
        tracing::subscriber::with_default(subscriber, || {
            if update {
                let span = tracing::info_span!(target: "host_record", "small", value = tracing::field::Empty);
                let before = cached_span(&span);
                span.record("value", "far too large for the span cache");
                assert_eq!(cached_span(&span), before);
            } else {
                let span = tracing::info_span!(target: "host_record", "small", value = "far too large for the span cache");
                assert_eq!(cached_span(&span), (String::new(), 0));
            }
            tracing::info!(target: "host_record", "refused");
        });
        assert!(matches!(
            status.failure().as_deref(),
            Some(RecordFailure::Size {
                buffer: BufferKind::Span,
                limit: 8
            })
        ));
        assert!(records.lock().unwrap().is_empty());
    }
}

#[test]
fn utf8_span_exact_boundary_preserves_whole_value() {
    let records = Records::default();
    let (subscriber, status) = bounded_subscriber(&records, 128, 6);
    tracing::subscriber::with_default(subscriber, || {
        let span = tracing::info_span!(target: "host_record", "small", v = "é");
        assert_eq!(cached_span(&span).0, "v=\"é\"");
        let _entered = span.enter();
        tracing::info!(target: "host_record", "UTF-8");
    });
    assert!(status.failure().is_none());
    assert_eq!(
        *records.lock().unwrap(),
        [" INFO small{v=\"é\"}: host_record: UTF-8\n"
            .as_bytes()
            .to_vec()]
    );
}

#[test]
fn formatting_occurs_once_per_new_value_not_a_sizing_pass() {
    struct Once(Arc<AtomicUsize>);
    impl fmt::Debug for Once {
        fn fmt(&self, writer: &mut fmt::Formatter<'_>) -> fmt::Result {
            let observed = self.0.fetch_add(1, Ordering::Relaxed);
            write!(writer, "value-{observed}")
        }
    }
    let calls = Arc::new(AtomicUsize::new(0));
    let records = Records::default();
    let (subscriber, status) = bounded_subscriber(&records, 256, 128);
    tracing::subscriber::with_default(subscriber, || {
        let span = tracing::info_span!(target: "host_record", "once", value = ?Once(calls.clone()));
        span.record("value", tracing::field::debug(Once(calls.clone())));
        let _entered = span.enter();
        tracing::info!(target: "host_record", value = ?Once(calls.clone()), "once");
    });
    assert_eq!(calls.load(Ordering::Relaxed), 3);
    assert!(status.failure().is_none());
    assert_eq!(
        *records.lock().unwrap(),
        [b" INFO once{value=value-0 value=value-1}: host_record: once value=value-2\n".to_vec()]
    );
}

#[test]
fn commit_adapter_also_refuses_oversize_before_sink() {
    let records = Records::default();
    let factory = RecordWriterFactory {
        sink: capture(&records),
        status: RecordStatus::default(),
    };
    assert!(factory.write_record(b"too large", 3).is_err());
    assert!(records.lock().unwrap().is_empty());
    assert!(matches!(
        factory.status.failure().as_deref(),
        Some(RecordFailure::Size {
            buffer: BufferKind::Event,
            limit: 3
        })
    ));
}

#[test]
fn event_fields_are_not_subject_to_the_smaller_span_budget() {
    let records = Records::default();
    let (subscriber, status) = bounded_subscriber(&records, 128, 1);
    tracing::subscriber::with_default(subscriber, || {
        tracing::info!(target: "host_record", value = 123456, "event only");
    });
    assert!(status.failure().is_none());
    assert_eq!(
        *records.lock().unwrap(),
        [b" INFO host_record: event only value=123456\n".to_vec()]
    );
}

#[test]
fn cached_span_prefixes_are_included_in_the_event_budget() {
    let records = Records::default();
    let (subscriber, status) = bounded_subscriber(&records, 32, 16);
    tracing::subscriber::with_default(subscriber, || {
        let outer = tracing::info_span!(target: "host_record", "outer", value = 1);
        let _outer = outer.enter();
        let inner = tracing::info_span!(target: "host_record", "inner", value = 2);
        let _inner = inner.enter();
        assert_eq!(cached_span(&outer).0, "value=1");
        assert_eq!(cached_span(&inner).0, "value=2");
        tracing::info!(target: "host_record", "too much combined output");
    });
    assert!(records.lock().unwrap().is_empty());
    assert!(matches!(
        status.failure().as_deref(),
        Some(RecordFailure::Size {
            buffer: BufferKind::Event,
            limit: 32
        })
    ));
}

#[test]
fn ignored_span_errors_cannot_commit_partial_cache() {
    struct Ignores;
    impl fmt::Debug for Ignores {
        fn fmt(&self, writer: &mut fmt::Formatter<'_>) -> fmt::Result {
            for _ in 0..8 {
                let _ = writer.write_str("xxxxxxxx");
            }
            Ok(())
        }
    }
    for update in [false, true] {
        let records = Records::default();
        let (subscriber, status) = bounded_subscriber(&records, 128, 16);
        tracing::subscriber::with_default(subscriber, || {
            if update {
                let span = tracing::info_span!(target: "host_record", "small", value = 1);
                let before = cached_span(&span);
                span.record("value", tracing::field::debug(Ignores));
                assert_eq!(cached_span(&span), before);
            } else {
                let span = tracing::info_span!(target: "host_record", "small", value = ?Ignores);
                assert_eq!(cached_span(&span), (String::new(), 0));
            }
        });
        assert!(records.lock().unwrap().is_empty());
        assert!(matches!(
            status.failure().as_deref(),
            Some(RecordFailure::Size {
                buffer: BufferKind::Span,
                limit: 16
            })
        ));
    }
}

#[test]
fn panicking_span_update_preserves_cache_without_poisoning_registry() {
    let expected_poisoning = match std::env::var("HERMIT_FORMATTER_TEST_REGISTRY_LOCK") {
        Ok(lock) if lock == "std" => Some(true),
        Ok(lock) if lock == "parking_lot" => Some(false),
        Err(std::env::VarError::NotPresent) => None,
        other => panic!("invalid formatter test registry lock contract: {other:?}"),
    };
    assert_span_update_panic_contract(expected_poisoning);
}

fn assert_span_update_panic_contract(expected_poisoning: Option<bool>) {
    struct Panics;
    impl fmt::Debug for Panics {
        fn fmt(&self, writer: &mut fmt::Formatter<'_>) -> fmt::Result {
            writer.write_str("partial")?;
            panic!("span update panic")
        }
    }
    fn exercise(check_cache: bool) -> (String, bool) {
        let span = tracing::info_span!(target: "host_record", "small", value = 1);
        let before = check_cache.then(|| cached_span(&span));
        let panic = std::panic::catch_unwind(AssertUnwindSafe(|| {
            span.record("value", tracing::field::debug(Panics));
        }))
        .unwrap_err();
        let message = panic.downcast_ref::<&str>().unwrap().to_string();
        let poisoned = std::panic::catch_unwind(AssertUnwindSafe(|| {
            span.with_subscriber(|(identity, dispatch)| {
                let registry = dispatch
                    .downcast_ref::<tracing_subscriber::Registry>()
                    .unwrap();
                let span = registry.span(identity).unwrap();
                let _extensions = span.extensions();
            });
        }))
        .is_err();
        if let Some(before) = before {
            assert_eq!(cached_span(&span), before);
        }
        (message, poisoned)
    }

    let original = tracing_subscriber::fmt()
        .with_env_filter(filter())
        .with_writer(io::sink)
        .finish();
    let baseline = tracing::subscriber::with_default(original, || exercise(false));
    assert_eq!(baseline.0, "span update panic");
    if let Some(expected) = expected_poisoning {
        assert_eq!(baseline.1, expected);
    }
    let records = Records::default();
    let (subscriber, status) = bounded_subscriber(&records, 128, 32);
    tracing::subscriber::with_default(subscriber, || {
        let observed = exercise(true);
        assert_eq!(observed.0, baseline.0);
        assert!(
            !observed.1,
            "user formatting must not poison Registry locks"
        );
        tracing::info!(target: "host_record", "not a recovered record");
    });
    assert!(records.lock().unwrap().is_empty());
    assert!(matches!(
        status.failure().as_deref(),
        Some(RecordFailure::Unwinding)
    ));
}

struct AllocationControl {
    attempts: usize,
    refuse: Option<usize>,
    before: Option<Box<dyn FnOnce()>>,
}

thread_local! {
    static PAYLOAD_ALLOCATIONS: RefCell<Option<AllocationControl>> = const { RefCell::new(None) };
}

struct PayloadAllocations;

impl PayloadAllocations {
    fn new(refuse: Option<usize>) -> Self {
        PAYLOAD_ALLOCATIONS.with(|control| {
            assert!(control.borrow().is_none());
            *control.borrow_mut() = Some(AllocationControl {
                attempts: 0,
                refuse,
                before: None,
            });
        });
        Self
    }

    fn attempts(&self) -> usize {
        PAYLOAD_ALLOCATIONS.with(|control| control.borrow().as_ref().unwrap().attempts)
    }
}

impl Drop for PayloadAllocations {
    fn drop(&mut self) {
        PAYLOAD_ALLOCATIONS.with(|control| *control.borrow_mut() = None);
    }
}

pub(super) fn before_payload_allocation() -> Result<(), std::collections::TryReserveError> {
    let before = PAYLOAD_ALLOCATIONS
        .try_with(|control| {
            control
                .borrow_mut()
                .as_mut()
                .and_then(|control| control.before.take())
        })
        .ok()
        .flatten();
    if let Some(before) = before {
        before();
    }
    let refused = PAYLOAD_ALLOCATIONS
        .try_with(|control| {
            let mut control = control.borrow_mut();
            let Some(control) = control.as_mut() else {
                return false;
            };
            control.attempts += 1;
            control.refuse == Some(control.attempts)
        })
        .unwrap_or(false);
    if refused {
        Vec::<u8>::new().try_reserve_exact(usize::MAX)
    } else {
        Ok(())
    }
}

fn remove_span_cache(span: &tracing::Span) {
    span.with_subscriber(|(identity, dispatch)| {
        let registry = dispatch
            .downcast_ref::<tracing_subscriber::Registry>()
            .unwrap();
        let span = registry.span(identity).unwrap();
        let caches = span.extensions_mut().remove::<SpanCaches>().unwrap();
        assert_eq!(
            caches.0.len(),
            1,
            "this helper removes a single-layer cache"
        );
    })
    .unwrap();
}

fn allocation_failure(status: &RecordStatus) -> Arc<RecordFailure> {
    let failure = status.failure().unwrap();
    assert!(matches!(&*failure, RecordFailure::Allocation));
    failure
}

#[test]
fn event_scratch_allocation_refusal_is_sticky_and_never_publishes() {
    let records = Records::default();
    let (subscriber, status) = bounded_subscriber(&records, 128, 32);
    let allocations = PayloadAllocations::new(Some(1));
    tracing::subscriber::with_default(subscriber, || {
        tracing::info!(target: "host_record", "refused before formatting");
        let first = allocation_failure(&status);
        tracing::info!(target: "host_record", "subsequent refusal");
        assert!(Arc::ptr_eq(&first, &allocation_failure(&status)));
    });
    assert_eq!(allocations.attempts(), 1);
    allocation_failure(&status);
    assert!(records.lock().unwrap().is_empty());
}

#[test]
fn initial_cache_backing_allocation_refusal_keeps_empty_fields_and_status() {
    struct Counted<'count>(&'count AtomicUsize);
    impl fmt::Debug for Counted<'_> {
        fn fmt(&self, writer: &mut fmt::Formatter<'_>) -> fmt::Result {
            self.0.fetch_add(1, Ordering::SeqCst);
            writer.write_str("value")
        }
    }
    let calls = AtomicUsize::new(0);
    let records = Records::default();
    let (subscriber, status) = bounded_subscriber(&records, 128, 32);
    let allocations = PayloadAllocations::new(Some(1));
    tracing::subscriber::with_default(subscriber, || {
        let span = tracing::info_span!(target: "host_record", "small", value = ?Counted(&calls));
        assert_eq!(cached_span(&span), (String::new(), 0));
        let first = allocation_failure(&status);
        span.record("value", tracing::field::debug(Counted(&calls)));
        assert_eq!(cached_span(&span), (String::new(), 0));
        let later = tracing::info_span!(target: "host_record", "later", value = ?Counted(&calls));
        assert_eq!(cached_span(&later), (String::new(), 0));
        tracing::info!(target: "host_record", "cannot publish");
        assert!(Arc::ptr_eq(&first, &allocation_failure(&status)));
    });
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(allocations.attempts(), 1);
    allocation_failure(&status);
    assert!(records.lock().unwrap().is_empty());
}

#[test]
fn update_scratch_allocation_refusal_preserves_complete_previous_cache() {
    let records = Records::default();
    let (subscriber, status) = bounded_subscriber(&records, 128, 32);
    let allocations = PayloadAllocations::new(Some(2));
    tracing::subscriber::with_default(subscriber, || {
        let span = tracing::info_span!(target: "host_record", "small", value = 1);
        let before = cached_span(&span);
        assert_eq!(before, ("value=1".to_owned(), 32));
        assert!(status.failure().is_none());
        span.record("value", 2);
        assert_eq!(cached_span(&span), before);
        let first = allocation_failure(&status);
        span.record("value", 3);
        assert_eq!(cached_span(&span), before);
        tracing::info!(target: "host_record", "cannot publish");
        assert!(Arc::ptr_eq(&first, &allocation_failure(&status)));
    });
    assert_eq!(allocations.attempts(), 2);
    allocation_failure(&status);
    assert!(records.lock().unwrap().is_empty());
}

#[test]
fn missing_cache_on_record_uses_the_same_fallible_initial_hook() {
    let records = Records::default();
    let (subscriber, status) = bounded_subscriber(&records, 128, 32);
    let allocations = PayloadAllocations::new(Some(2));
    tracing::subscriber::with_default(subscriber, || {
        let span = tracing::info_span!(target: "host_record", "small", value = 1);
        remove_span_cache(&span);
        span.record("value", 2);
        assert_eq!(cached_span(&span), (String::new(), 0));
        let first = allocation_failure(&status);
        span.record("value", 3);
        assert_eq!(cached_span(&span), (String::new(), 0));
        tracing::info!(target: "host_record", "cannot publish");
        assert!(Arc::ptr_eq(&first, &allocation_failure(&status)));
    });
    assert_eq!(allocations.attempts(), 2);
    allocation_failure(&status);
    assert!(records.lock().unwrap().is_empty());
}

#[test]
fn initial_cache_has_exact_capacity_and_no_second_payload_allocation() {
    for limit in [3, 7, 8, 31, 1024] {
        let records = Records::default();
        let (subscriber, status) = bounded_subscriber(&records, 128, limit);
        let allocations = PayloadAllocations::new(Some(2));
        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!(target: "host_record", "small", v = 1);
            assert_eq!(cached_span(&span), ("v=1".to_owned(), limit));
        });
        assert_eq!(allocations.attempts(), 1);
        assert!(status.failure().is_none());
        assert!(records.lock().unwrap().is_empty());
    }
}

#[test]
fn owned_scratch_transfer_preserves_allocation_identity_and_utf8() {
    use std::fmt::Write as _;

    let status = RecordStatus::default();
    let allocations = PayloadAllocations::new(Some(2));
    let mut buffer = BoundedBuffer::new(7, BufferKind::Span, &status).unwrap();
    buffer.write_str("é").unwrap();
    let pointer = buffer.bytes.as_ptr();
    let text = buffer.into_string();
    assert_eq!(text.as_ptr(), pointer);
    assert_eq!(text, "é");
    assert_eq!(text.capacity(), 7);
    assert_eq!(allocations.attempts(), 1);
    assert!(status.failure().is_none());
}

#[derive(Clone, Default)]
struct Output(Arc<Mutex<Vec<u8>>>);

impl Write for Output {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn compare_with_stock(filter_text: &str, emit: impl Fn()) -> Vec<Vec<u8>> {
    let expected = Output::default();
    let writer = expected.clone();
    let stock = tracing_subscriber::fmt()
        .with_timer(FixedTime)
        .with_env_filter(EnvFilter::new(filter_text))
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();
    tracing::subscriber::with_default(stock, &emit);
    let records = Records::default();
    let (subscriber, status) = subscriber_with_format(
        EnvFilter::new(filter_text),
        false,
        capture(&records),
        format().with_timer(FixedTime),
    );
    tracing::subscriber::with_default(subscriber, emit);
    let records = records.lock().unwrap().clone();
    assert_eq!(records.concat(), *expected.0.lock().unwrap());
    assert!(status.failure().is_none());
    records
}

#[test]
fn complete_records_during_unrelated_outer_unwind_match_stock() {
    struct EmitOnDrop;

    impl Drop for EmitOnDrop {
        fn drop(&mut self) {
            tracing::info!(target: "host_record", "outer panic event");
            let span = tracing::info_span!(target: "host_record", "during_drop", value = 1);
            let _entered = span.enter();
            span.record("value", 2);
            tracing::info!(target: "host_record", "updated span event");
        }
    }

    let records = compare_with_stock("info", || {
        let panic = std::panic::catch_unwind(|| {
            let _emit = EmitOnDrop;
            panic!("unrelated outer panic");
        });
        assert_eq!(
            panic.unwrap_err().downcast_ref::<&str>(),
            Some(&"unrelated outer panic")
        );
        tracing::info!(target: "host_record", "after catch_unwind");
    });
    assert_eq!(
        records,
        [
            b"unchanged-time  INFO host_record: outer panic event\n".to_vec(),
            b"unchanged-time  INFO during_drop{value=1 value=2}: host_record: updated span event\n"
                .to_vec(),
            b"unchanged-time  INFO host_record: after catch_unwind\n".to_vec(),
        ]
    );
}

#[test]
fn complete_bytes_match_stock_fields_spans_filter_and_sanitization() {
    let records = compare_with_stock("off,host_record=info", structured_events);
    assert_eq!(records.len(), 2);
    assert_eq!(records[0], b"unchanged-time  INFO work{task=7}: host_record: multi\nline chunks=first\nsecond\x1b[31mthird number=1.0\n");
    assert_eq!(
        records[1],
        b"unchanged-time  INFO work{task=7 later=9}: host_record: after span update\n"
    );
}

#[test]
fn complete_bytes_match_all_levels_types_errors_and_raw_names() {
    #[derive(Debug)]
    struct Root;
    impl fmt::Display for Root {
        fn fmt(&self, writer: &mut fmt::Formatter<'_>) -> fmt::Result {
            writer.write_str("source\x1b[32m")
        }
    }
    impl std::error::Error for Root {}
    #[derive(Debug)]
    struct Failure(Root);
    impl fmt::Display for Failure {
        fn fmt(&self, writer: &mut fmt::Formatter<'_>) -> fmt::Result {
            writer.write_str("failure\x1b[31m")
        }
    }
    impl std::error::Error for Failure {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            Some(&self.0)
        }
    }
    let records = compare_with_stock("trace", || {
        tracing::trace!(target: "host_record", "trace");
        tracing::debug!(target: "host_record", "debug");
        tracing::info!(target: "host_record", signed = -2i64, unsigned = 3u64, large = i128::MAX,
            float = 1.25f64, boolean = true, r#type = "é", bytes = ?[0u8, 255], "types");
        tracing::warn!(target: "host_record", "message\x1b[31m");
        let failure = Failure(Root);
        tracing::error!(target: "host_record", error = &failure as &(dyn std::error::Error + 'static), "failed");
    });
    assert_eq!(records.len(), 5);
    assert_eq!(records[0], b"unchanged-time TRACE host_record: trace\n");
    assert_eq!(records[1], b"unchanged-time DEBUG host_record: debug\n");
    assert_eq!(records[2], "unchanged-time  INFO host_record: types signed=-2 unsigned=3 large=170141183460469231731687303715884105727 float=1.25 boolean=true type=\"é\" bytes=[0, 255]\n".as_bytes());
    assert!(!records[3].contains(&0x1b));
    assert!(!records[4].contains(&0x1b));
    assert!(String::from_utf8_lossy(&records[4]).contains("error.sources="));
}

#[test]
fn nested_explicit_and_root_parents_match_stock() {
    let records = compare_with_stock("info", || {
        let root = tracing::info_span!(target: "host_record", "root", id = 1);
        let _root = root.enter();
        let child = tracing::info_span!(target: "host_record", "child", id = 2);
        let _child = child.enter();
        tracing::info!(target: "host_record", "context");
        tracing::info!(target: "host_record", parent: &root, "explicit");
        tracing::info!(target: "host_record", parent: None, "root event");
        root.record("id", 3);
        tracing::info!(target: "host_record", "updated ancestor");
    });
    assert_eq!(
        records,
        [
            b"unchanged-time  INFO root{id=1}:child{id=2}: host_record: context\n".to_vec(),
            b"unchanged-time  INFO root{id=1}: host_record: explicit\n".to_vec(),
            b"unchanged-time  INFO host_record: root event\n".to_vec(),
            b"unchanged-time  INFO root{id=1 id=3}:child{id=2}: host_record: updated ancestor\n"
                .to_vec(),
        ]
    );
}

#[test]
fn dynamic_filter_matching_and_span_updates_match_stock() {
    let records = compare_with_stock("off,host_record[work{task=7}]=info", || {
        tracing::info!(target: "host_record", "outside");
        let rejected = tracing::info_span!(target: "host_record", "work", task = 8);
        rejected.in_scope(|| tracing::info!(target: "host_record", "wrong value"));
        let selected =
            tracing::info_span!(target: "host_record", "work", task = tracing::field::Empty);
        selected.record("task", 7);
        selected.in_scope(|| {
            tracing::info!(target: "host_record", "selected");
            tracing::debug!(target: "host_record", "wrong level");
        });
    });
    assert_eq!(
        records,
        [b"unchanged-time  INFO work{task=7}: host_record: selected\n".to_vec()]
    );
}

#[test]
fn log_bridge_metadata_is_normalized_without_reclassifying_tracing_events() {
    let records = compare_with_stock("trace", || {
        // The public Log implementation creates the real bridge callsite, not a
        // tracing event heuristically identified by its target or field names.
        let record = tracing_log::log::Record::builder()
            .args(format_args!("bridged"))
            .level(tracing_log::log::Level::Info)
            .target("actual_log_target")
            .module_path(Some("source_module"))
            .file(Some("source.rs"))
            .line(Some(27))
            .build();
        tracing_log::log::Log::log(&tracing_log::LogTracer::new(), &record);
        tracing::event!(target: "log", tracing::Level::INFO, { message = "ordinary tracing", log.target = "must_not_replace_target" });
    });
    assert_eq!(
        records,
        [
            b"unchanged-time  INFO actual_log_target: bridged\n".to_vec(),
            b"unchanged-time  INFO log: ordinary tracing\n".to_vec(),
        ]
    );
}

fn stock_layer_output<F>(filter: F, emit: impl FnOnce()) -> Vec<u8>
where
    F: tracing_subscriber::layer::Filter<tracing_subscriber::Registry> + Send + Sync + 'static,
{
    let output = Output::default();
    let writer = output.clone();
    let layer = tracing_subscriber::fmt::layer()
        .with_timer(FixedTime)
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .with_filter(filter);
    tracing::subscriber::with_default(tracing_subscriber::registry().with(layer), emit);
    output.0.lock().unwrap().clone()
}

fn bounded_record_callback_child(test: &str, case: &str) {
    use std::process::Command;
    use std::process::Stdio;
    use std::time::Duration;
    use std::time::Instant;

    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture", "--test-threads=1"])
        .env("HERMIT_RECORD_LOCK_CASE", case)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            child.kill().unwrap();
            let output = child.wait_with_output().unwrap();
            panic!("{case}: callback did not return: {output:?}");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "{case}: {output:?}");
    assert!(output.stderr.is_empty(), "{case}: {output:?}");
}

// Publish the actual registered ID using a public Layer callback. The writer
// below uses Dispatch::record; it does not manufacture a Span before creation.
struct ObserveInitializingSpan {
    span: std::sync::mpsc::Sender<(Id, &'static tracing::Metadata<'static>)>,
    event: Option<std::sync::mpsc::Sender<()>>,
}

impl<S: Subscriber> Layer<S> for ObserveInitializingSpan {
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, _: Context<'_, S>) {
        self.span.send((id.clone(), attrs.metadata())).unwrap();
    }

    fn on_event(&self, _: &Event<'_>, _: Context<'_, S>) {
        if let Some(event) = &self.event {
            event.send(()).unwrap();
        }
    }
}

fn record_initializing_span(
    dispatch: &tracing::Dispatch,
    id: &Id,
    metadata: &'static tracing::Metadata<'static>,
    value: Option<u64>,
) {
    let field = metadata.fields().field("v").unwrap();
    let empty = tracing::field::Empty;
    let value: &dyn tracing::field::Value = match &value {
        Some(value) => value,
        None => &empty,
    };
    let values = [(&field, Some(value))];
    dispatch.record(id, &Record::new(&metadata.fields().value_set(&values)));
}

thread_local! {
    static BEFORE_SPAN_INITIALIZATION: RefCell<Option<Box<dyn FnOnce()>>> =
        const { RefCell::new(None) };
}

pub(super) fn before_span_initialization() {
    let before = BEFORE_SPAN_INITIALIZATION.with(|control| control.borrow_mut().take());
    if let Some(before) = before {
        before();
    }
}

#[test]
fn span_initialization_keeps_early_updates_and_exact_stock_empty_spaces() {
    use std::sync::mpsc;
    use std::time::Duration;

    const TEST: &str = "liteinst_record::tests::span_initialization_keeps_early_updates_and_exact_stock_empty_spaces";
    if std::env::var("HERMIT_RECORD_LOCK_CASE").as_deref() != Ok("initial-updates") {
        bounded_record_callback_child(TEST, "initial-updates");
        return;
    }
    for initial in [None, Some(1)] {
        for updates in [
            vec![None, None],
            vec![None, None, Some(2), None, Some(3)],
            vec![None; 1030],
        ] {
            let expected_overflow = initial.is_some() && updates.len() == 1030;
            let expected_output = Output::default();
            let writer = expected_output.clone();
            let stock = tracing_subscriber::fmt()
                .without_time()
                .with_ansi(false)
                .with_writer(move || writer.clone())
                .finish();
            tracing::subscriber::with_default(stock, || {
                let span = tracing::info_span!(target: "host_record", "work", v = initial);
                for update in &updates {
                    match update {
                        Some(value) => {
                            span.record("v", value);
                        }
                        None => {
                            span.record("v", tracing::field::Empty);
                        }
                    }
                }
                tracing::info!(target: "host_record", parent: &span, "initialized");
            });
            let expected = expected_output.0.lock().unwrap().clone();
            let records = Records::default();
            let status = RecordStatus::default();
            let (id_tx, id_rx) = mpsc::channel();
            let (started_tx, started_rx) = mpsc::channel();
            let (done_tx, done_rx) = mpsc::channel();
            let registry = tracing_subscriber::registry().with(ObserveInitializingSpan {
                span: id_tx,
                event: None,
            });
            let layer = super::layer_with_status(
                baseline_limits(),
                capture(&records),
                (),
                false,
                status.clone(),
            );
            let dispatch = tracing::Dispatch::new(registry.with(layer));
            let writer_dispatch = dispatch.clone();
            let writer = std::thread::spawn(move || {
                let (id, metadata) = id_rx.recv_timeout(Duration::from_secs(2)).unwrap();
                started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
                for value in updates {
                    record_initializing_span(&writer_dispatch, &id, metadata, value);
                }
                done_tx.send(()).unwrap();
            });
            tracing::dispatcher::with_default(&dispatch, || {
                let allocations = PayloadAllocations::new(None);
                // Pausing before initial allocation also covers a truly empty
                // initial field set, which has no user Debug callback to pause.
                PAYLOAD_ALLOCATIONS.with(|control| {
                    control.borrow_mut().as_mut().unwrap().before = Some(Box::new(move || {
                        started_tx.send(()).unwrap();
                        done_rx.recv_timeout(Duration::from_secs(2)).unwrap();
                    }));
                });
                let span = tracing::info_span!(target: "host_record", "work", v = initial);
                assert_eq!(allocations.attempts(), 1);
                drop(allocations);
                writer.join().unwrap();
                tracing::info!(target: "host_record", parent: &span, "initialized");
            });
            if expected_overflow {
                assert!(matches!(
                    status.failure().as_deref(),
                    Some(RecordFailure::Size {
                        buffer: BufferKind::Span,
                        limit: 1024,
                    })
                ));
                assert!(records.lock().unwrap().is_empty());
            } else {
                assert!(status.failure().is_none());
                assert_eq!(
                    *records.lock().unwrap(),
                    vec![expected],
                    "initial {initial:?}"
                );
            }
        }
    }
}

#[test]
fn early_span_update_returns_before_initial_debug_and_formats_each_value_once() {
    use std::sync::mpsc;
    use std::time::Duration;

    const TEST: &str = "liteinst_record::tests::early_span_update_returns_before_initial_debug_and_formats_each_value_once";
    if std::env::var("HERMIT_RECORD_LOCK_CASE").as_deref() != Ok("initial-debug") {
        bounded_record_callback_child(TEST, "initial-debug");
        return;
    }
    struct Initial {
        started: mpsc::Sender<()>,
        done: Mutex<mpsc::Receiver<()>>,
        calls: Arc<AtomicUsize>,
    }
    impl fmt::Debug for Initial {
        fn fmt(&self, writer: &mut fmt::Formatter<'_>) -> fmt::Result {
            assert_eq!(self.calls.fetch_add(1, Ordering::SeqCst), 0);
            self.started.send(()).unwrap();
            self.done
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(2))
                .unwrap();
            writer.write_str("1")
        }
    }
    let records = Records::default();
    let status = RecordStatus::default();
    let (id_tx, id_rx) = mpsc::channel();
    let (started_tx, started_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();
    let registry = tracing_subscriber::registry().with(ObserveInitializingSpan {
        span: id_tx,
        event: None,
    });
    let layer = super::layer_with_status(
        baseline_limits(),
        capture(&records),
        (),
        false,
        status.clone(),
    );
    let dispatch = tracing::Dispatch::new(registry.with(layer));
    struct Update(Arc<AtomicUsize>);
    impl fmt::Debug for Update {
        fn fmt(&self, writer: &mut fmt::Formatter<'_>) -> fmt::Result {
            self.0.fetch_add(1, Ordering::SeqCst);
            writer.write_str("2")
        }
    }
    let update_calls = Arc::new(AtomicUsize::new(0));
    let worker_calls = update_calls.clone();
    let worker_dispatch = dispatch.clone();
    let writer = std::thread::spawn(move || {
        let (id, metadata) = id_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let field = metadata.fields().field("v").unwrap();
        let value = tracing::field::debug(Update(worker_calls));
        let values = [(&field, Some(&value as &dyn tracing::field::Value))];
        worker_dispatch.record(&id, &Record::new(&metadata.fields().value_set(&values)));
        done_tx.send(()).unwrap();
    });
    let calls = Arc::new(AtomicUsize::new(0));
    tracing::dispatcher::with_default(&dispatch, || {
        let span = tracing::info_span!(target: "host_record", "work", v = ?Initial {
            started: started_tx, done: Mutex::new(done_rx), calls: calls.clone(),
        });
        writer.join().unwrap();
        tracing::info!(target: "host_record", parent: &span, "initialized");
    });
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(update_calls.load(Ordering::SeqCst), 1);
    assert!(status.failure().is_none());
    assert_eq!(
        *records.lock().unwrap(),
        vec![b" INFO work{v=1 v=2}: host_record: initialized\n".to_vec()]
    );
}

#[test]
fn event_waits_for_span_initialization_and_wakes_before_failure_notification() {
    use std::sync::mpsc;
    use std::time::Duration;

    const TEST: &str = "liteinst_record::tests::event_waits_for_span_initialization_and_wakes_before_failure_notification";
    let cases = [
        "initial-event",
        "initial-publication",
        "initial-error",
        "initial-panic",
        "initial-overflow",
        "initial-allocation",
    ];
    let Ok(case) = std::env::var("HERMIT_RECORD_LOCK_CASE") else {
        for case in cases {
            bounded_record_callback_child(TEST, case);
        }
        return;
    };
    if !cases.contains(&case.as_str()) {
        return;
    }
    std::panic::set_hook(Box::new(|_| {}));
    struct Initial {
        started: mpsc::Sender<()>,
        release: Mutex<mpsc::Receiver<()>>,
        calls: Arc<AtomicUsize>,
        case: String,
    }
    impl fmt::Debug for Initial {
        fn fmt(&self, writer: &mut fmt::Formatter<'_>) -> fmt::Result {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.started.send(()).unwrap();
            self.release
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(2))
                .unwrap();
            match self.case.as_str() {
                "initial-error" => Err(fmt::Error),
                "initial-panic" => panic!("initial field panic retained"),
                _ => writer.write_str("1"),
            }
        }
    }
    let records = Records::default();
    let status = RecordStatus::default();
    let notifications = Arc::new(AtomicUsize::new(0));
    let notified = notifications.clone();
    let (id_tx, id_rx) = mpsc::channel();
    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let (event_tx, event_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();
    let done_rx = Arc::new(Mutex::new(done_rx));
    let callback_done = done_rx.clone();
    assert!(
        status
            .1
            .set(Box::new(move || {
                notified.fetch_add(1, Ordering::SeqCst);
                callback_done
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(2))
                    .unwrap();
            }))
            .is_ok()
    );
    let registry = tracing_subscriber::registry().with(ObserveInitializingSpan {
        span: id_tx,
        event: Some(event_tx),
    });
    let layer = super::layer_with_status(
        FormatterLimits::new(4096, if case == "initial-overflow" { 3 } else { 128 }).unwrap(),
        capture(&records),
        (),
        false,
        status.clone(),
    );
    let dispatch = tracing::Dispatch::new(registry.with(layer));
    let initializer_dispatch = dispatch.clone();
    let calls = Arc::new(AtomicUsize::new(0));
    let initial_calls = calls.clone();
    let initial_case = case.clone();
    let initializer = std::thread::spawn(move || {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            tracing::dispatcher::with_default(&initializer_dispatch, || {
                if initial_case == "initial-publication" {
                    // Pause immediately before marking the cache as initializing,
                    // while the real Registry extension guard still protects its
                    // publication. The event must not observe an empty ready cache.
                    BEFORE_SPAN_INITIALIZATION.with(|control| {
                        *control.borrow_mut() = Some(Box::new(move || {
                            started_tx.send(()).unwrap();
                            release_rx.recv_timeout(Duration::from_secs(2)).unwrap();
                        }));
                    });
                    return tracing::info_span!(target: "host_record", "work", v = 1);
                }
                let allocation = (initial_case == "initial-allocation")
                    .then(|| PayloadAllocations::new(Some(1)));
                if allocation.is_some() {
                    PAYLOAD_ALLOCATIONS.with(|control| {
                        let started = started_tx.clone();
                        control.borrow_mut().as_mut().unwrap().before = Some(Box::new(move || {
                            started.send(()).unwrap();
                            release_rx.recv_timeout(Duration::from_secs(2)).unwrap();
                        }));
                    });
                    tracing::info_span!(target: "host_record", "work", v = 1)
                } else {
                    tracing::info_span!(target: "host_record", "work", v = ?Initial {
                        started: started_tx, release: Mutex::new(release_rx),
                        calls: initial_calls, case: initial_case,
                    })
                }
            })
        }))
    });
    let (id, metadata) = id_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    if case == "initial-overflow" {
        record_initializing_span(&dispatch, &id, metadata, Some(2));
    }
    let inspect_id = id.clone();
    let event_dispatch = dispatch.clone();
    let event = std::thread::spawn(move || {
        tracing::dispatcher::with_default(&event_dispatch, || {
            tracing::info!(target: "host_record", parent: &id, "during initialization");
        });
        done_tx.send(()).unwrap();
    });
    event_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    let before_initial = done_rx
        .lock()
        .unwrap()
        .recv_timeout(Duration::from_millis(30));
    let records_before_initial = records.lock().unwrap().clone();
    release_tx.send(()).unwrap();
    let created = initializer.join().unwrap();
    event.join().unwrap();
    assert!(matches!(
        before_initial,
        Err(mpsc::RecvTimeoutError::Timeout)
    ));
    assert!(records_before_initial.is_empty());
    if matches!(case.as_str(), "initial-event" | "initial-publication") {
        done_rx
            .lock()
            .unwrap()
            .recv_timeout(Duration::from_secs(2))
            .unwrap();
        assert!(created.is_ok());
        assert_eq!(
            calls.load(Ordering::SeqCst),
            usize::from(case == "initial-event")
        );
        assert!(status.failure().is_none());
        assert_eq!(notifications.load(Ordering::SeqCst), 0);
        assert_eq!(
            *records.lock().unwrap(),
            vec![b" INFO work{v=1}: host_record: during initialization\n".to_vec()]
        );
    } else {
        assert_eq!(notifications.load(Ordering::SeqCst), 1);
        assert!(records.lock().unwrap().is_empty());
        assert_eq!(
            calls.load(Ordering::SeqCst),
            usize::from(case != "initial-allocation")
        );
        let registry = dispatch
            .downcast_ref::<tracing_subscriber::Registry>()
            .unwrap();
        let span = registry.span(&inspect_id).unwrap();
        let extensions = span.extensions();
        let caches = extensions.get::<SpanCaches>().unwrap();
        let cache = caches.0[0].1.0.lock().unwrap();
        assert!(cache.pending.is_none());
        assert_eq!(cache.fields.fields, "");
        assert_eq!(cache.fields.fields.capacity(), 0);
        drop(cache);
        drop(extensions);
        match case.as_str() {
            "initial-error" => assert!(matches!(
                status.failure().as_deref(),
                Some(RecordFailure::Formatting)
            )),
            "initial-panic" => {
                let panic = created.expect_err("original panic propagated");
                assert_eq!(
                    panic.downcast_ref::<&str>(),
                    Some(&"initial field panic retained")
                );
                assert!(matches!(
                    status.failure().as_deref(),
                    Some(RecordFailure::Unwinding)
                ));
            }
            "initial-allocation" => assert!(matches!(
                status.failure().as_deref(),
                Some(RecordFailure::Allocation)
            )),
            "initial-overflow" => assert!(matches!(
                status.failure().as_deref(),
                Some(RecordFailure::Size {
                    buffer: BufferKind::Span,
                    limit: 3
                })
            )),
            _ => unreachable!(),
        }
    }
}

#[test]
fn span_field_logging_with_two_layers_returns_and_retains_reentry_failure() {
    const TEST: &str = "liteinst_record::tests::span_field_logging_with_two_layers_returns_and_retains_reentry_failure";
    if std::env::var("HERMIT_RECORD_LOCK_CASE").as_deref() != Ok("record") {
        bounded_record_callback_child(TEST, "record");
        return;
    }
    struct Emits(tracing::Span, Arc<AtomicUsize>);
    impl fmt::Debug for Emits {
        fn fmt(&self, writer: &mut fmt::Formatter<'_>) -> fmt::Result {
            self.1.fetch_add(1, Ordering::SeqCst);
            tracing::info!(target: "host_record", parent: &self.0, "nested");
            writer.write_str("updated")
        }
    }
    let first = Records::default();
    let second = Records::default();
    let first_status = RecordStatus::default();
    let second_status = RecordStatus::default();
    let notifications = Arc::new(AtomicUsize::new(0));
    for status in [&first_status, &second_status] {
        let notifications = notifications.clone();
        assert!(
            status
                .1
                .set(Box::new(move || {
                    notifications.fetch_add(1, Ordering::SeqCst);
                }))
                .is_ok()
        );
    }
    let first_layer = layer_with_status(
        baseline_limits(),
        capture(&first),
        (),
        false,
        first_status.clone(),
    );
    let second_layer = layer_with_status(
        baseline_limits(),
        capture(&second),
        (),
        false,
        second_status.clone(),
    );
    let subscriber = tracing_subscriber::registry()
        .with(first_layer.with_filter(tracing::level_filters::LevelFilter::INFO))
        .with(second_layer.with_filter(tracing::level_filters::LevelFilter::INFO));
    let calls = Arc::new(AtomicUsize::new(0));
    tracing::subscriber::with_default(subscriber, || {
        let span = tracing::info_span!(target: "host_record", "work", value = 1);
        span.record(
            "value",
            tracing::field::debug(Emits(span.clone(), calls.clone())),
        );
        span.with_subscriber(|(id, dispatch)| {
            let registry = dispatch
                .downcast_ref::<tracing_subscriber::Registry>()
                .unwrap();
            let span = registry.span(id).unwrap();
            let extensions = span.extensions();
            let caches = extensions.get::<SpanCaches>().unwrap();
            assert_eq!(caches.0.len(), 2);
            assert!(
                caches
                    .0
                    .iter()
                    .all(|(_, cache)| cache.0.lock().unwrap().fields.fields == "value=1")
            );
        })
        .unwrap();
    });
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(notifications.load(Ordering::SeqCst), 2);
    for status in [&first_status, &second_status] {
        assert!(matches!(
            status.failure().as_deref(),
            Some(RecordFailure::Reentrant)
        ));
    }
    assert!(first.lock().unwrap().is_empty());
    assert_eq!(
        *second.lock().unwrap(),
        [b" INFO work{value=1}: host_record: nested\n".to_vec()]
    );
}

#[test]
fn new_span_formatter_can_read_its_registry_extensions() {
    const TEST: &str =
        "liteinst_record::tests::new_span_formatter_can_read_its_registry_extensions";
    if std::env::var("HERMIT_RECORD_LOCK_CASE").as_deref() != Ok("new") {
        bounded_record_callback_child(TEST, "new");
        return;
    }
    struct Observe(Arc<Mutex<Option<Id>>>);
    impl<S: Subscriber> Layer<S> for Observe {
        fn on_new_span(&self, _: &Attributes<'_>, id: &Id, _: Context<'_, S>) {
            *self.0.lock().unwrap() = Some(id.clone());
        }
    }
    struct Reads {
        dispatch: tracing::Dispatch,
        id: Arc<Mutex<Option<Id>>>,
        calls: Arc<AtomicUsize>,
    }
    impl fmt::Debug for Reads {
        fn fmt(&self, writer: &mut fmt::Formatter<'_>) -> fmt::Result {
            let id = self.id.lock().unwrap().clone().unwrap();
            let registry = self
                .dispatch
                .downcast_ref::<tracing_subscriber::Registry>()
                .unwrap();
            let span = registry.span(&id).unwrap();
            let _extensions = span.extensions();
            self.calls.fetch_add(1, Ordering::SeqCst);
            writer.write_str("readable")
        }
    }
    let id = Arc::new(Mutex::new(None));
    let records = Records::default();
    let status = RecordStatus::default();
    let layer = layer_with_status(
        baseline_limits(),
        capture(&records),
        (),
        false,
        status.clone(),
    );
    let dispatch = tracing::Dispatch::new(
        tracing_subscriber::registry()
            .with(Observe(id.clone()))
            .with(layer),
    );
    let calls = Arc::new(AtomicUsize::new(0));
    tracing::dispatcher::with_default(&dispatch, || {
        let span = tracing::info_span!(target: "host_record", "work", value = ?Reads {
            dispatch: dispatch.clone(), id, calls: calls.clone(),
        });
        tracing::info!(target: "host_record", parent: &span, "complete");
    });
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(status.failure().is_none());
    assert_eq!(
        *records.lock().unwrap(),
        [b" INFO work{value=readable}: host_record: complete\n".to_vec()]
    );
}

#[test]
fn concurrent_span_updates_merge_latest_fields_without_reformatting() {
    const TEST: &str =
        "liteinst_record::tests::concurrent_span_updates_merge_latest_fields_without_reformatting";
    if std::env::var("HERMIT_RECORD_LOCK_CASE").as_deref() != Ok("concurrent") {
        bounded_record_callback_child(TEST, "concurrent");
        return;
    }
    struct Paused {
        calls: Arc<AtomicUsize>,
        entered: std::sync::mpsc::SyncSender<()>,
        release: Mutex<std::sync::mpsc::Receiver<()>>,
    }
    impl fmt::Debug for Paused {
        fn fmt(&self, writer: &mut fmt::Formatter<'_>) -> fmt::Result {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                self.entered.send(()).unwrap();
                self.release.lock().unwrap().recv().unwrap();
            }
            writer.write_str("1")
        }
    }
    struct Counted(Arc<AtomicUsize>);
    impl fmt::Debug for Counted {
        fn fmt(&self, writer: &mut fmt::Formatter<'_>) -> fmt::Result {
            self.0.fetch_add(1, Ordering::SeqCst);
            writer.write_str("2")
        }
    }
    let first = Records::default();
    let second = Records::default();
    let first_status = RecordStatus::default();
    let second_status = RecordStatus::default();
    let first_layer = layer_with_status(
        baseline_limits(),
        capture(&first),
        (),
        false,
        first_status.clone(),
    );
    let second_layer = layer_with_status(
        baseline_limits(),
        capture(&second),
        (),
        false,
        second_status.clone(),
    );
    let subscriber = tracing_subscriber::registry()
        .with(first_layer)
        .with(second_layer);
    let first_calls = Arc::new(AtomicUsize::new(0));
    let second_calls = Arc::new(AtomicUsize::new(0));
    tracing::subscriber::with_default(subscriber, || {
        let span = tracing::info_span!(target: "host_record", "work", v = 0);
        let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(0);
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(0);
        let first_span = span.clone();
        let calls = first_calls.clone();
        let first = std::thread::spawn(move || {
            first_span.record(
                "v",
                tracing::field::debug(Paused {
                    calls,
                    entered: entered_tx,
                    release: Mutex::new(release_rx),
                }),
            );
        });
        entered_rx.recv().unwrap();
        let second_span = span.clone();
        let calls = second_calls.clone();
        std::thread::spawn(move || {
            second_span.record("v", tracing::field::debug(Counted(calls)));
        })
        .join()
        .unwrap();
        release_tx.send(()).unwrap();
        first.join().unwrap();
        tracing::info!(target: "host_record", parent: &span, "all updates");
    });
    assert_eq!(first_calls.load(Ordering::SeqCst), 2);
    assert_eq!(second_calls.load(Ordering::SeqCst), 2);
    assert!(first_status.failure().is_none());
    assert!(second_status.failure().is_none());
    let expected = [b" INFO work{v=0 v=2 v=1}: host_record: all updates\n".to_vec()];
    assert_eq!(*first.lock().unwrap(), expected);
    assert_eq!(*second.lock().unwrap(), expected);
}

#[test]
fn span_merge_overflow_keeps_previous_cache_after_formatting_fragment_once() {
    struct Counted<'a>(&'a AtomicUsize);
    impl fmt::Debug for Counted<'_> {
        fn fmt(&self, writer: &mut fmt::Formatter<'_>) -> fmt::Result {
            self.0.fetch_add(1, Ordering::SeqCst);
            writer.write_str("2")
        }
    }
    let calls = AtomicUsize::new(0);
    let records = Records::default();
    let (subscriber, status) = bounded_subscriber(&records, 128, 3);
    let allocations = PayloadAllocations::new(Some(3));
    tracing::subscriber::with_default(subscriber, || {
        let span = tracing::info_span!(target: "host_record", "work", v = 1);
        let before = cached_span(&span);
        span.record("v", tracing::field::debug(Counted(&calls)));
        assert_eq!(cached_span(&span), before);
    });
    // Formatting is separate from the serialized merge: unlike stock's
    // prefix-first writer, this invokes the new value even if the prior cache
    // already fills the budget. It never invokes it a second time to retry.
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(allocations.attempts(), 2);
    assert!(matches!(
        status.failure().as_deref(),
        Some(RecordFailure::Size {
            buffer: BufferKind::Span,
            limit: 3,
        })
    ));
    assert!(records.lock().unwrap().is_empty());
}

#[test]
fn span_and_event_failure_notifications_can_update_another_layer() {
    const TEST: &str =
        "liteinst_record::tests::span_and_event_failure_notifications_can_update_another_layer";
    let case = std::env::var("HERMIT_RECORD_LOCK_CASE");
    let case = match case.as_deref() {
        Ok("span-notify") => BufferKind::Span,
        Ok("event-notify") => BufferKind::Event,
        _ => {
            for case in ["span-notify", "event-notify"] {
                bounded_record_callback_child(TEST, case);
            }
            return;
        }
    };
    let parent = Arc::new(Mutex::new(None::<tracing::Span>));
    let notifications = Arc::new(AtomicUsize::new(0));
    let failed_status = RecordStatus::default();
    let complete_status = RecordStatus::default();
    let notify_parent = parent.clone();
    let notify_count = notifications.clone();
    assert!(
        failed_status
            .1
            .set(Box::new(move || {
                notify_count.fetch_add(1, Ordering::SeqCst);
                let parent = notify_parent.lock().unwrap().clone().unwrap();
                parent.record("v", 3);
            }))
            .is_ok()
    );
    let failed = Records::default();
    let complete = Records::default();
    let limits = match case {
        BufferKind::Span => FormatterLimits::new(128, 3).unwrap(),
        BufferKind::Event => FormatterLimits::new(14, 32).unwrap(),
    };
    let failed_layer =
        layer_with_status(limits, capture(&failed), (), false, failed_status.clone());
    let complete_layer = layer_with_status(
        baseline_limits(),
        capture(&complete),
        (),
        false,
        complete_status.clone(),
    );
    let subscriber = tracing_subscriber::registry()
        .with(failed_layer)
        .with(complete_layer);
    tracing::subscriber::with_default(subscriber, || {
        let span = tracing::info_span!(target: "host_record", "work", v = 1);
        *parent.lock().unwrap() = Some(span.clone());
        if case == BufferKind::Span {
            span.record("v", 2);
        }
        tracing::info!(target: "host_record", parent: &span, "complete");
        *parent.lock().unwrap() = None;
    });
    assert_eq!(notifications.load(Ordering::SeqCst), 1);
    assert!(
        matches!(failed_status.failure().as_deref(), Some(RecordFailure::Size { buffer, .. }) if *buffer == case)
    );
    assert!(complete_status.failure().is_none());
    assert!(failed.lock().unwrap().is_empty());
    let expected: &[u8] = match case {
        BufferKind::Span => b" INFO work{v=1 v=3 v=2}: host_record: complete\n",
        BufferKind::Event => b" INFO work{v=1 v=3}: host_record: complete\n",
    };
    assert_eq!(*complete.lock().unwrap(), [expected.to_vec()]);
}

#[test]
fn empty_span_update_fragments_preserve_stock_separator_bytes() {
    let records = compare_with_stock("trace", || {
        let span = tracing::info_span!(target: "host_record", "work", v = 1);
        span.record("v", tracing::field::Empty);
        span.record("v", tracing::field::Empty);
        span.record("v", 2);
        tracing::info!(target: "host_record", parent: &span, "spaces");
    });
    assert_eq!(
        records,
        [b"unchanged-time  INFO work{v=1   v=2}: host_record: spaces\n".to_vec()]
    );
}

fn dual_layer_events() {
    tracing::info!(target: "host_record", "outside private INFO");
    let root = tracing::info_span!(target: "host_record", "root", owner = 1);
    let _root = root.enter();
    let work = tracing::info_span!(target: "host_record", "work", task = tracing::field::Empty, later = tracing::field::Empty);
    work.record("task", 7);
    work.in_scope(|| {
        tracing::trace!(target: "host_record", "public TRACE");
        tracing::info!(target: "host_record", "both current");
        let detail = tracing::debug_span!(target: "host_record", "detail", step = 2);
        detail.in_scope(|| tracing::info!(target: "host_record", "different ancestor selection"));
        root.record("owner", 3);
        work.record("later", 9);
        tracing::info!(target: "host_record", "after updates");
        tracing::info!(target: "host_record", parent: &root, "explicit root");
        tracing::info!(target: "host_record", parent: None, "no parent");
        let record = tracing_log::log::Record::builder()
            .args(format_args!("bridged in span"))
            .level(tracing_log::log::Level::Info)
            .target("actual_log_target")
            .module_path(Some("source_module"))
            .file(Some("source.rs"))
            .line(Some(27))
            .build();
        tracing_log::log::Log::log(&tracing_log::LogTracer::new(), &record);
    });
    tracing::info!(target: "host_record", "after private INFO");
}

#[test]
fn complete_layers_keep_independent_stock_bytes_and_filters() {
    const PUBLIC: &str = "off,host_record[work{task=7}]=trace,actual_log_target=info";
    // Independent stock subscribers avoid sharing stock's own per-field-type
    // span cache between the two oracles. Each output defines one layer's bytes.
    let expected_public = stock_layer_output(EnvFilter::new(PUBLIC), dual_layer_events);
    let expected_private =
        stock_layer_output(tracing::metadata::LevelFilter::INFO, dual_layer_events);
    let public = Records::default();
    let private = Records::default();
    let public_status = RecordStatus::default();
    let private_status = RecordStatus::default();
    let public_layer = layer_with_status(
        baseline_limits(),
        capture(&public),
        FixedTime,
        true,
        public_status.clone(),
    )
    .with_filter(EnvFilter::new(PUBLIC));
    let private_layer = layer_with_status(
        baseline_limits(),
        capture(&private),
        FixedTime,
        true,
        private_status.clone(),
    )
    .with_filter(tracing::metadata::LevelFilter::INFO);
    tracing::subscriber::with_default(
        tracing_subscriber::registry()
            .with(public_layer)
            .with(private_layer),
        dual_layer_events,
    );
    let public = public.lock().unwrap();
    let private = private.lock().unwrap();
    assert_eq!(public.concat(), expected_public);
    assert_eq!(private.concat(), expected_private);
    assert!(public_status.failure().is_none());
    assert!(private_status.failure().is_none());
    assert_eq!(public.len(), 7);
    assert_eq!(private.len(), 8);
    assert_eq!(
        public[0],
        b"unchanged-time TRACE work{task=7}: host_record: public TRACE\n"
    );
    assert_eq!(
        private[0],
        b"unchanged-time  INFO host_record: outside private INFO\n"
    );
    assert_eq!(
        public[3],
        b"unchanged-time  INFO work{task=7 later=9}: host_record: after updates\n"
    );
    assert_eq!(
        private[3],
        b"unchanged-time  INFO root{owner=1 owner=3}:work{task=7 later=9}: host_record: after updates\n"
    );
    assert_eq!(
        public[6],
        b"unchanged-time  INFO work{task=7 later=9}: actual_log_target: bridged in span\n"
    );
}

#[test]
fn public_layer_constructor_retains_only_its_own_sink_failure() {
    let private = Records::default();
    let attempts = Arc::new(AtomicUsize::new(0));
    let public_notifications = Arc::new(AtomicUsize::new(0));
    let private_notifications = Arc::new(AtomicUsize::new(0));
    let calls = attempts.clone();
    let public_failed = public_notifications.clone();
    let private_failed = private_notifications.clone();
    let (public_layer, public_status) = super::record_layer_with_failure(
        baseline_limits(),
        move |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            Err(io::Error::from_raw_os_error(libc::EPIPE))
        },
        move || {
            public_failed.fetch_add(1, Ordering::SeqCst);
        },
    );
    let (private_layer, private_status) =
        super::record_layer_with_failure(baseline_limits(), capture(&private), move || {
            private_failed.fetch_add(1, Ordering::SeqCst);
        });
    tracing::subscriber::with_default(
        tracing_subscriber::registry()
            .with(public_layer.with_filter(EnvFilter::new("off,host_record=info")))
            .with(private_layer.with_filter(tracing::metadata::LevelFilter::INFO)),
        || {
            let span = tracing::info_span!(target: "host_record", "work", v = 1);
            let _entered = span.enter();
            tracing::info!(target: "host_record", "first");
            span.record("v", 2);
            tracing::info!(target: "host_record", "second");
        },
    );
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    assert_eq!(public_notifications.load(Ordering::SeqCst), 1);
    assert_eq!(private_notifications.load(Ordering::SeqCst), 0);
    assert!(matches!(
        public_status.failure().as_deref(),
        Some(RecordFailure::Sink(error)) if error.raw_os_error() == Some(libc::EPIPE)
    ));
    assert!(private_status.failure().is_none());
    let private = private.lock().unwrap();
    assert_eq!(private.len(), 2);
    // The public constructor uses real timestamps. Full bytes with a fixed
    // input timer are checked by the independent stock-oracle test above.
    assert!(private[0].ends_with(b" INFO work{v=1}: host_record: first\n"));
    assert!(private[1].ends_with(b" INFO work{v=1 v=2}: host_record: second\n"));
}

#[test]
fn span_update_failure_keeps_the_other_layer_complete_in_either_order() {
    let expected_first = b"unchanged-time  INFO work{v=1}: host_record: before\n";
    let expected_second = b"unchanged-time  INFO work{v=1 v=2}: host_record: after\n";
    for small_first in [false, true] {
        let first = Records::default();
        let second = Records::default();
        let first_status = RecordStatus::default();
        let second_status = RecordStatus::default();
        let first_layer = layer_with_status(
            FormatterLimits::new(256, if small_first { 3 } else { 64 }).unwrap(),
            capture(&first),
            FixedTime,
            true,
            first_status.clone(),
        );
        let second_layer = layer_with_status(
            FormatterLimits::new(256, if small_first { 64 } else { 3 }).unwrap(),
            capture(&second),
            FixedTime,
            true,
            second_status.clone(),
        );
        tracing::subscriber::with_default(
            tracing_subscriber::registry()
                .with(first_layer.with_filter(tracing::metadata::LevelFilter::INFO))
                .with(second_layer.with_filter(tracing::metadata::LevelFilter::INFO)),
            || {
                let span = tracing::info_span!(target: "host_record", "work", v = 1);
                let _entered = span.enter();
                tracing::info!(target: "host_record", "before");
                span.record("v", 2);
                tracing::info!(target: "host_record", "after");
            },
        );
        let (failed_records, failed_status, complete_records, complete_status) = if small_first {
            (&first, &first_status, &second, &second_status)
        } else {
            (&second, &second_status, &first, &first_status)
        };
        assert_eq!(*failed_records.lock().unwrap(), [expected_first.to_vec()]);
        assert!(matches!(
            failed_status.failure().as_deref(),
            Some(RecordFailure::Size {
                buffer: BufferKind::Span,
                limit: 3,
            })
        ));
        assert_eq!(
            *complete_records.lock().unwrap(),
            [expected_first.to_vec(), expected_second.to_vec()]
        );
        assert!(complete_status.failure().is_none());
    }
}

#[test]
fn timer_failure_preserves_stock_fallback_but_cannot_hide_overflow() {
    struct FailingTime;
    impl FormatTime for FailingTime {
        fn format_time(&self, writer: &mut Writer<'_>) -> fmt::Result {
            writer.write_str("prefix")?;
            Err(fmt::Error)
        }
    }
    for limit in [128, 6] {
        let records = Records::default();
        let (subscriber, status) = super::subscriber_with_status(
            filter(),
            FormatterLimits::new(limit, 8).unwrap(),
            capture(&records),
            FailingTime,
            true,
            RecordStatus::default(),
        );
        tracing::subscriber::with_default(
            subscriber,
            || tracing::info!(target: "host_record", "time"),
        );
        if limit == 128 {
            assert_eq!(
                *records.lock().unwrap(),
                [b"prefix<unknown time>  INFO host_record: time\n".to_vec()]
            );
            assert!(status.failure().is_none());
        } else {
            assert!(records.lock().unwrap().is_empty());
            assert!(matches!(
                status.failure().as_deref(),
                Some(RecordFailure::Size {
                    buffer: BufferKind::Event,
                    limit: 6
                })
            ));
        }
    }
}

const TEARDOWN_CASE: &str = "HERMIT_RECORD_TEARDOWN_CASE";
const TEARDOWN_OUTPUT: &str = "HERMIT_RECORD_TEARDOWN_OUTPUT";
const TEARDOWN_RECORD: &[u8] =
    b" INFO watched{owner=7}: host_record: retained value=11 answer=\"stable\"\n";

fn teardown_event(parent: &tracing::Span) {
    tracing::info!(target: "host_record", parent: parent, value = 11, answer = "stable", "retained");
}

struct ExitRecord {
    parent: tracing::Span,
    create_span: bool,
    contextual: bool,
}

impl Drop for ExitRecord {
    fn drop(&mut self) {
        if self.create_span {
            let _span = tracing::info_span!(target: "host_record", parent: &self.parent, "late", value = 11);
        } else if self.contextual {
            tracing::info!(target: "host_record", value = 11, answer = "stable", "retained");
        } else {
            teardown_event(&self.parent);
        }
    }
}

thread_local! {
    static EXIT_RECORD: std::cell::OnceCell<ExitRecord> = const { std::cell::OnceCell::new() };
}
static PROCESS_PARENT: OnceLock<tracing::Span> = OnceLock::new();

extern "C" fn process_exit_record() {
    teardown_event(PROCESS_PARENT.get().expect("retained process parent"));
}

// This is a normal libtest target: discovery does not run a producer. Each
// selected subprocess installs one global subscriber, avoiding scoped-dispatch
// TLS in the very control intended to test logging after TLS destruction.
#[test]
fn teardown_child() {
    let Ok(case) = std::env::var(TEARDOWN_CASE) else {
        return;
    };
    let limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_CORE, &limit) }, 0);
    let path = std::env::var_os(TEARDOWN_OUTPUT).expect("child output path");
    let file = std::fs::File::create(path).unwrap();
    let dynamic = case == "dynamic-filter";
    let filter = EnvFilter::new(if dynamic {
        "off,[watched]=info"
    } else {
        "info"
    });
    let status = if case.starts_with("stock-") {
        let writer = Arc::new(file);
        tracing::subscriber::set_global_default(
            tracing_subscriber::fmt()
                .without_time()
                .with_ansi(false)
                .with_env_filter(filter)
                .with_writer(writer)
                .finish(),
        )
        .unwrap();
        None
    } else {
        let writer = Mutex::new(file);
        let (subscriber, status) = super::subscriber_with_status(
            filter,
            baseline_limits(),
            move |record| writer.lock().unwrap().write_all(record),
            (),
            false,
            RecordStatus::default(),
        );
        tracing::subscriber::set_global_default(subscriber).unwrap();
        Some(status)
    };
    if case.ends_with("process") {
        let parent = tracing::info_span!(target: "host_record", "watched", owner = 7);
        parent.in_scope(|| teardown_event(&parent));
        assert!(PROCESS_PARENT.set(parent).is_ok());
        assert_eq!(unsafe { libc::atexit(process_exit_record) }, 0);
        // exit runs current-thread TLS destruction before the atexit callback.
        unsafe { libc::exit(0) }
    }
    std::thread::spawn(move || {
        let after_registry = case != "thread" && case != "stock-thread";
        if after_registry {
            EXIT_RECORD.with(|_| ());
        }
        let parent = tracing::info_span!(target: "host_record", "watched", owner = 7);
        let entered = parent.enter();
        EXIT_RECORD.with(|slot| {
            assert!(
                slot.set(ExitRecord {
                    parent: parent.clone(),
                    create_span: case == "new-span",
                    contextual: case == "contextual",
                })
                .is_ok()
            );
        });
        teardown_event(&parent);
        if case == "contextual" {
            // Keep an entered context until TLS destruction. This separate
            // dependency reproducer must not be called complete-log evidence.
            std::mem::forget(entered);
        }
    })
    .join()
    .unwrap();
    if let Some(status) = status {
        assert!(status.failure().is_none());
    }
}

fn teardown_subprocess(case: &str) -> (std::process::Output, Vec<u8>) {
    use std::process::Command;
    use std::process::Stdio;
    use std::time::Duration;
    use std::time::Instant;

    let directory = tempfile::tempdir().unwrap();
    let output = directory.path().join("records");
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "liteinst_record::tests::teardown_child",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(TEARDOWN_CASE, case)
        .env(TEARDOWN_OUTPUT, &output)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            let result = child.wait_with_output().unwrap();
            panic!(
                "teardown subprocess {case} exceeded 10 seconds: {}",
                String::from_utf8_lossy(&result.stderr)
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let result = child.wait_with_output().unwrap();
    let bytes = std::fs::read(output).unwrap();
    (result, bytes)
}

#[test]
fn complete_records_survive_thread_and_process_formatter_tls_teardown() {
    for case in ["thread", "after-registry", "process"] {
        let (result, bytes) = teardown_subprocess(case);
        assert!(
            result.status.success(),
            "{case}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(bytes, TEARDOWN_RECORD.repeat(2), "{case}");
        assert!(result.stderr.is_empty(), "{case}: {:?}", result.stderr);
    }
}

#[test]
fn teardown_control_reproduces_stock_formatter_tls_failure() {
    for case in ["stock-thread", "stock-process"] {
        let (result, bytes) = teardown_subprocess(case);
        assert!(
            !result.status.success(),
            "{case}: control did not exercise destroyed fmt TLS"
        );
        assert!(
            String::from_utf8_lossy(&result.stderr)
                .contains("cannot access a Thread Local Storage value"),
            "{case}: {:?}",
            result.stderr
        );
        assert_eq!(
            bytes, TEARDOWN_RECORD,
            "{case}: only the original warmup record survives"
        );
    }
}

// These controls characterize unresolved dependency defects, not supported
// logging cases. When the lifetime defects are fixed, require the complete
// records here too; never relax the positive 142-byte teardown comparison.
#[test]
fn known_teardown_defects_abort_on_new_span_and_dynamic_filter() {
    use std::os::unix::process::ExitStatusExt;

    for (case, diagnostic) in [
        (
            "new-span",
            // sharded-slab 0.1.7, shard.rs:295. Reassess this known defect
            // after a dependency update; complete records remain the goal.
            "Thread count overflowed the configured max count.",
        ),
        (
            "dynamic-filter",
            "cannot access a Thread Local Storage value during or after destruction: AccessError",
        ),
    ] {
        let (result, bytes) = teardown_subprocess(case);
        assert_eq!(
            result.status.signal(),
            Some(libc::SIGABRT),
            "{case}: expected the known dependency abort, not complete logging"
        );
        let stderr = String::from_utf8_lossy(&result.stderr);
        assert!(stderr.contains(diagnostic), "{case}: {stderr}");
        assert!(
            stderr.contains("thread local panicked on drop, aborting"),
            "{case}: {stderr}"
        );
        assert_eq!(
            bytes, TEARDOWN_RECORD,
            "{case}: the known defect preserves only the 71-byte warmup"
        );
    }
}

#[test]
fn known_teardown_defect_loses_context_without_reporting_a_local_failure() {
    let (result, bytes) = teardown_subprocess("contextual");
    // The child also requires RecordStatus to remain clear. Exit success and
    // a clear local status are insufficient: the entered context was lost.
    assert!(result.status.success(), "{:?}", result.stderr);
    assert!(result.stderr.is_empty(), "{:?}", result.stderr);
    let missing_context = b" INFO host_record: retained value=11 answer=\"stable\"\n";
    assert_eq!(bytes, [TEARDOWN_RECORD, missing_context].concat());
    assert_eq!(bytes.len(), 124);
    assert_ne!(bytes, TEARDOWN_RECORD.repeat(2));
}
