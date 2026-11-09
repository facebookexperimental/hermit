//! Canonical native log framing used by the actual shared engine channel.

use tracing::Event;
use tracing::field::Field;
use tracing::field::Visit;

const CANONICAL_PREFIX: &str = "1970-01-01T00:00:00.000000Z ";

pub(super) fn level(code: i32) -> Option<tracing::Level> {
    match code {
        1 => Some(tracing::Level::ERROR),
        2 => Some(tracing::Level::WARN),
        3 => Some(tracing::Level::INFO),
        4 => Some(tracing::Level::DEBUG),
        5 => Some(tracing::Level::TRACE),
        _ => None,
    }
}

pub(super) fn escaped(output: &mut String, value: &str) {
    for character in value.chars() {
        match character {
            '\\' => output.push_str("\\\\"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            _ => output.push(character),
        }
    }
}

#[derive(Default)]
struct Fields(String);

impl Fields {
    fn push(&mut self, field: &Field, value: String) {
        if !self.0.is_empty() {
            self.0.push(' ');
        }
        if field.name() != "message" {
            self.0.push_str(field.name());
            self.0.push('=');
        }
        escaped(&mut self.0, &value);
    }
}

impl Visit for Fields {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.push(field, format!("{value:?}"));
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        self.push(field, value.to_owned());
    }
}

pub(super) fn format_log_record(
    level: &str,
    target: &str,
    fields: &str,
    canonical: bool,
) -> String {
    let prefix = if canonical { CANONICAL_PREFIX } else { "" };
    format!("{prefix}{level} {target}: {fields}\n")
}

pub(super) fn format_event(event: &Event<'_>, canonical: bool) -> String {
    let mut fields = Fields::default();
    event.record(&mut fields);
    let metadata = event.metadata();
    format_log_record(
        metadata.level().as_str(),
        metadata.target(),
        &fields.0,
        canonical,
    )
}

pub(super) fn protected_evidence_capture_ready(protected_level: i32, tracing_active: bool) -> bool {
    protected_level == 0 || tracing_active
}

pub(super) fn emit_determinism_loss_record(
    emit: unsafe extern "C" fn(*const u8, usize),
    loss: Option<String>,
) {
    let Some(reason) = loss else {
        return;
    };
    let mut fields = String::new();
    escaped(&mut fields, &reason);
    let record = format_log_record(
        tracing::Level::ERROR.as_str(),
        detcore::detlog::DETERMINISM_LOSS_RECORD_TARGET,
        &fields,
        true,
    );
    unsafe { emit(record.as_ptr(), record.len()) };
}

#[cfg(test)]
mod tests {
    use detcore::GlobalState;

    use super::*;

    static LOSS_RECORDS: std::sync::Mutex<Vec<u8>> = std::sync::Mutex::new(Vec::new());

    unsafe extern "C" fn capture_loss_record(bytes: *const u8, length: usize) {
        let bytes = unsafe { std::slice::from_raw_parts(bytes, length) };
        LOSS_RECORDS.lock().unwrap().extend_from_slice(bytes);
    }

    #[test]
    fn a_recorded_determinism_loss_becomes_an_evidence_record() {
        LOSS_RECORDS.lock().unwrap().clear();
        emit_determinism_loss_record(capture_loss_record, None);
        assert!(LOSS_RECORDS.lock().unwrap().is_empty());

        emit_determinism_loss_record(
            capture_loss_record,
            Some("thread 3 of process 3 has a pending guest-handled SIGALRM\nbut".to_owned()),
        );
        assert_eq!(
            String::from_utf8(LOSS_RECORDS.lock().unwrap().clone()).unwrap(),
            format!(
                "1970-01-01T00:00:00.000000Z ERROR {}: thread 3 of process 3 has a pending \
                 guest-handled SIGALRM\\nbut\n",
                detcore::detlog::DETERMINISM_LOSS_RECORD_TARGET
            )
        );
    }

    #[test]
    fn protected_evidence_records_have_deterministic_prefix_and_escaped_fields() {
        let mut fields = String::new();
        escaped(&mut fields, "line one\nline two\\tail\r");
        assert_eq!(fields, "line one\\nline two\\\\tail\\r");
        assert_eq!(
            format_log_record(
                tracing::Level::INFO.as_str(),
                "detcore::scheduler",
                &fields,
                true,
            ),
            "1970-01-01T00:00:00.000000Z INFO detcore::scheduler: line one\\nline two\\\\tail\\r\n"
        );
        // The `canonical` flag arrived on main after this test was written
        // (a811f33684). Pin BOTH branches: the deterministic prefix is what a
        // verification record needs, and its ABSENCE is what keeps an ordinary
        // diagnostic run from claiming a fixed timestamp it never had. Asserting
        // only the true branch would let a change that always prefixes pass.
        assert_eq!(
            format_log_record(
                tracing::Level::INFO.as_str(),
                "detcore::scheduler",
                &fields,
                false,
            ),
            "INFO detcore::scheduler: line one\\nline two\\\\tail\\r\n"
        );
    }

    #[test]
    fn canonical_dynamorio_log_records_have_timestamp_and_one_line() {
        let mut fields = String::new();
        escaped(&mut fields, "first\\second\nthird\rfourth");
        assert_eq!(fields, "first\\\\second\\nthird\\rfourth");
        assert_eq!(
            format_log_record("INFO", "detcore::scheduler", &fields, true),
            "1970-01-01T00:00:00.000000Z INFO detcore::scheduler: \
             first\\\\second\\nthird\\rfourth\n"
        );
        assert_eq!(
            format_log_record("INFO", "detcore::scheduler", "ready", false),
            "INFO detcore::scheduler: ready\n"
        );
    }

    #[test]
    fn protected_evidence_level_selects_the_expected_filter() {
        // Through the subscriber DYNAMORIO installs (detcore::guest_log), with its
        // own channel at the INFO evidence level: ERROR, WARN and INFO are
        // written, in order, in canonical form; DEBUG and TRACE are not; and no
        // event the channel takes goes to the coordinator.
        static WRITTEN: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
        fn write(event: &tracing::Event<'_>) {
            WRITTEN.lock().unwrap().push(format_event(event, true));
        }
        fn call(
            _request: <GlobalState as reverie::GlobalTool>::Request,
        ) -> std::io::Result<<GlobalState as reverie::GlobalTool>::Response> {
            panic!("an event DYNAMORIO's own channel takes is not sent to the coordinator");
        }
        let info = level(3).expect("INFO code");
        let own = detcore::guest_log::OwnChannel {
            max_level: info,
            write,
        };
        tracing::subscriber::with_default(detcore::guest_log::subscriber(call, Some(own)), || {
            tracing::error!(target: "detcore::scheduler", "e");
            tracing::warn!(target: "detcore::scheduler", "w");
            tracing::info!(target: "detcore::scheduler", "DETLOG i");
            tracing::debug!(target: "detcore::scheduler", "d");
            tracing::trace!(target: "detcore::scheduler", "t");
        });
        assert_eq!(
            *WRITTEN.lock().unwrap(),
            [
                "1970-01-01T00:00:00.000000Z ERROR detcore::scheduler: e\n",
                "1970-01-01T00:00:00.000000Z WARN detcore::scheduler: w\n",
                "1970-01-01T00:00:00.000000Z INFO detcore::scheduler: DETLOG i\n",
            ]
        );
        let levels = (1..=5).map(level).collect::<Vec<_>>();
        assert_eq!(
            levels,
            [
                Some(tracing::Level::ERROR),
                Some(tracing::Level::WARN),
                Some(tracing::Level::INFO),
                Some(tracing::Level::DEBUG),
                Some(tracing::Level::TRACE),
            ]
        );
        assert!(level(0).is_none());
        assert!(level(6).is_none());
    }

    #[test]
    fn requested_protected_evidence_requires_an_installed_subscriber() {
        assert!(protected_evidence_capture_ready(0, false));
        assert!(protected_evidence_capture_ready(3, true));
        assert!(!protected_evidence_capture_ready(3, false));
    }
}
