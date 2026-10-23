// Copyright (c) Meta Platforms, Inc. and affiliates.
//
// This source code is licensed under the BSD-style license found in the
// LICENSE file in the root directory of this source tree.

//! The shared CLI logging filter policy and the tool identity, configuration
//! schema and effective logging directives carried
//! by the LiteInst bootstrap. The payload is schema-validated and its config
//! fingerprint is compared; it is not cryptographically authenticated.
//! The host resolves the environment once;
//! the guest consumes the same accepted directives without reading its own
//! environment or reformatting field matchers.

use serde::Deserialize;
use serde::Serialize;
use tracing::metadata::LevelFilter;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::filter::Directive;

const VERSION: u32 = 2;
const TOOL: &str = "hermit-detcore-liteinst-v1";

#[derive(Debug)]
pub struct EffectiveFilter {
    directives: String,
    filter: EnvFilter,
}

impl EffectiveFilter {
    /// Resolve `RUST_LOG` once, using the same missing/non-Unicode fallback as
    /// `EnvFilter::from_default_env`. All CLI file and stderr subscribers use
    /// this constructor so their policy also defines the bootstrap directives.
    pub fn from_default_env(level: LevelFilter) -> Self {
        let raw = std::env::var(EnvFilter::DEFAULT_ENV).unwrap_or_default();
        Self::from_directives_lossy(&raw, level)
    }

    /// Resolve directives using the CLI's existing lossy environment policy and
    /// its final Tokio and level overrides. Preserve the accepted source text:
    /// formatting an EnvFilter is not a lossless encoding of field matchers.
    pub fn from_directives_lossy(raw: &str, level: LevelFilter) -> Self {
        let filter = EnvFilter::new(raw)
            .add_directive("tokio=debug".parse().expect("correct directive"))
            .add_directive(level.into());
        let mut accepted: Vec<_> = raw
            .split(',')
            .filter(|directive| !directive.is_empty())
            .filter(|directive| directive.parse::<Directive>().is_ok())
            .collect();
        let level_text = level.to_string();
        accepted.push("tokio=debug");
        accepted.push(&level_text);
        Self {
            directives: accepted.join(","),
            filter,
        }
    }

    pub fn into_filter(self) -> EnvFilter {
        self.filter
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Payload {
    version: u32,
    tool: String,
    config_wire_fingerprint: String,
    log_filter: String,
}

pub fn encode(
    config_wire_fingerprint: &str,
    log_filter: &EffectiveFilter,
) -> Result<Vec<u8>, serde_json::Error> {
    serde_json::to_vec(&Payload {
        version: VERSION,
        tool: TOOL.to_owned(),
        config_wire_fingerprint: config_wire_fingerprint.to_owned(),
        log_filter: log_filter.directives.clone(),
    })
}

pub fn decode(bytes: &[u8], expected_fingerprint: &str) -> Result<EffectiveFilter, String> {
    let payload: Payload = serde_json::from_slice(bytes)
        .map_err(|error| format!("invalid bootstrap payload: {error}"))?;
    if payload.version != VERSION {
        return Err("unsupported bootstrap payload version".to_owned());
    }
    if payload.tool != TOOL {
        return Err("bootstrap tool mismatch".to_owned());
    }
    if payload.config_wire_fingerprint != expected_fingerprint {
        return Err("bootstrap config fingerprint mismatch".to_owned());
    }
    let filter = EnvFilter::try_new(&payload.log_filter)
        .map_err(|error| format!("invalid bootstrap log filter: {error}"))?;
    Ok(EffectiveFilter {
        directives: payload.log_filter,
        filter,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::Value;
    use serde_json::json;

    use super::*;

    const FINGERPRINT: &str = "fixture-config-fingerprint";

    fn payload() -> Value {
        json!({
            "version": VERSION,
            "tool": TOOL,
            "config_wire_fingerprint": FINGERPRINT,
            "log_filter": "info,tokio=debug,detcore[work{task=7}]=trace",
        })
    }

    #[test]
    fn schema_and_filter_round_trip() {
        let raw = payload()["log_filter"].as_str().unwrap().to_owned();
        let filter = EffectiveFilter::from_directives_lossy(&raw, LevelFilter::WARN);
        let encoded = encode(FINGERPRINT, &filter).unwrap();
        let mut expected = payload();
        expected["log_filter"] = json!(format!("{raw},tokio=debug,warn"));
        assert_eq!(serde_json::from_slice::<Value>(&encoded).unwrap(), expected);
        let decoded = decode(&encoded, FINGERPRINT).unwrap();
        assert_eq!(decoded.directives, filter.directives);
        assert_eq!(encode(FINGERPRINT, &decoded).unwrap(), encoded);
    }

    #[test]
    fn preserves_accepted_spelling_and_override_order() {
        let raw = "warn,,detcore[work{task=1.0}]=info,tokio=off,broken=bogus,detcore[work{task=-1.0}]=debug,detcore[work{task=1e0}]=trace";
        let filter = EffectiveFilter::from_directives_lossy(raw, LevelFilter::WARN);
        let expected = "warn,detcore[work{task=1.0}]=info,tokio=off,detcore[work{task=-1.0}]=debug,detcore[work{task=1e0}]=trace,tokio=debug,warn";
        let encoded = encode(FINGERPRINT, &filter).unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&encoded).unwrap()["log_filter"],
            expected
        );
        let decoded = decode(&encoded, FINGERPRINT).unwrap();
        assert_eq!(decoded.directives, expected);
        assert_eq!(encode(FINGERPRINT, &decoded).unwrap(), encoded);
    }

    use std::io;
    use std::io::Write;
    use std::sync::Arc;
    use std::sync::Mutex;

    #[derive(Clone)]
    struct Records(Arc<Mutex<Vec<u8>>>);

    impl Write for Records {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn capture(filter: EnvFilter) -> Vec<u8> {
        let records = Records(Arc::new(Mutex::new(Vec::new())));
        let writer = records.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .without_time()
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(target: "bootstrap_fixture", "outside info");
            tracing::debug!(target: "tokio", "Tokio override");
            for task in [1.0_f64, -1.0, 2.0] {
                let span = tracing::info_span!(target: "bootstrap_fixture", "work", task);
                let _entered = span.enter();
                tracing::debug!(target: "bootstrap_fixture", "inside debug");
                tracing::trace!(target: "bootstrap_fixture", "inside trace");
            }
            tracing::warn!(target: "bootstrap_fixture", "warning");
        });
        records.0.lock().unwrap().clone()
    }

    #[test]
    fn decoded_filter_selects_the_same_events_and_span_fields_as_the_host() {
        for raw in [
            "",
            "off,tokio=off",
            "broken=bogus,bootstrap_fixture=trace",
            "off,bootstrap_fixture[work{task=1.0}]=trace",
            "off,bootstrap_fixture[work{task=-1.0}]=debug,bootstrap_fixture[work{task=1e0}]=trace",
            "bootstrap_fixture=info,bootstrap_fixture=trace,bootstrap_fixture=warn",
        ] {
            let host = EffectiveFilter::from_directives_lossy(raw, LevelFilter::WARN);
            let payload = encode(FINGERPRINT, &host).unwrap();
            let guest = decode(&payload, FINGERPRINT).unwrap();
            // Keep the original CLI construction as an independent oracle.
            let original = EnvFilter::new(raw)
                .add_directive("tokio=debug".parse().unwrap())
                .add_directive(LevelFilter::WARN.into());
            let expected = capture(original);
            assert_eq!(capture(host.into_filter()), expected, "host: {raw}");
            assert!(
                expected
                    .windows(b"Tokio override".len())
                    .any(|part| part == b"Tokio override"),
                "the mandatory Tokio directive must select a real event: {raw}"
            );
            if raw.contains("work{task=") {
                assert!(
                    expected
                        .windows(b"inside trace".len())
                        .any(|part| part == b"inside trace"),
                    "the dynamic matcher must select an event in its span: {raw}"
                );
                assert!(
                    !expected
                        .windows(b"outside info".len())
                        .any(|part| part == b"outside info"),
                    "the dynamic matcher must not enable the same target outside its span: {raw}"
                );
            }
            assert_eq!(capture(guest.into_filter()), expected, "{raw}");
        }
    }

    // Run environment controls in child processes so these tests do not mutate
    // the process-global environment while other libtest threads are active.
    #[test]
    fn default_environment_filter_subprocess() {
        if std::env::var_os("HERMIT_BOOTSTRAP_FILTER_CHILD").is_none() {
            return;
        }
        let host = EffectiveFilter::from_default_env(LevelFilter::WARN);
        let encoded = encode(FINGERPRINT, &host).unwrap();
        let original = EnvFilter::from_default_env()
            .add_directive("tokio=debug".parse().unwrap())
            .add_directive(LevelFilter::WARN.into());
        let expected = capture(original);
        assert!(
            expected
                .windows(b"Tokio override".len())
                .any(|part| part == b"Tokio override")
        );
        assert_eq!(capture(host.into_filter()), expected);
        assert_eq!(
            capture(decode(&encoded, FINGERPRINT).unwrap().into_filter()),
            expected
        );
    }

    #[test]
    fn environment_policy_matches_original_cli_for_missing_invalid_and_valid_values() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;
        use std::process::Command;
        use std::process::Stdio;
        use std::time::Duration;
        use std::time::Instant;

        for value in [
            None,
            Some(OsString::new()),
            Some(OsString::from_vec(vec![0xff])),
            Some(OsString::from(
                "broken=bogus,bootstrap_fixture=trace,tokio=off",
            )),
            Some(OsString::from(
                "off,bootstrap_fixture[work{task=1.0}]=trace",
            )),
        ] {
            let mut child = Command::new(std::env::current_exe().unwrap());
            child.args([
                "--exact",
                "liteinst_bootstrap::tests::default_environment_filter_subprocess",
                "--nocapture",
            ]);
            child.env("HERMIT_BOOTSTRAP_FILTER_CHILD", "1");
            child.env_remove(EnvFilter::DEFAULT_ENV);
            if let Some(value) = &value {
                child.env(EnvFilter::DEFAULT_ENV, value);
            }
            child.stdout(Stdio::piped()).stderr(Stdio::piped());
            let mut child = child.spawn().unwrap();
            let deadline = Instant::now() + Duration::from_secs(10);
            while child.try_wait().unwrap().is_none() {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let output = child.wait_with_output().unwrap();
                    panic!("filter child exceeded ten seconds: {value:?}: {output:?}");
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            let output = child.wait_with_output().unwrap();
            assert!(output.status.success(), "{value:?}: {output:?}");
            assert!(
                String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed"),
                "{value:?}: {output:?}"
            );
        }
    }

    #[test]
    fn refuses_wrong_version_tool_fingerprint_and_filter() {
        for (field, value, expected) in [
            ("version", json!(1), "unsupported bootstrap payload version"),
            ("tool", json!("other-tool"), "bootstrap tool mismatch"),
            (
                "config_wire_fingerprint",
                json!("other-config"),
                "bootstrap config fingerprint mismatch",
            ),
            (
                "log_filter",
                json!("detcore=bogus"),
                "invalid bootstrap log filter:",
            ),
        ] {
            let mut invalid = payload();
            invalid[field] = value;
            let error = decode(&serde_json::to_vec(&invalid).unwrap(), FINGERPRINT).unwrap_err();
            assert!(error.starts_with(expected), "{field}: {error}");
        }
    }

    #[test]
    fn refuses_missing_unknown_and_wrong_typed_fields() {
        for field in ["version", "tool", "config_wire_fingerprint", "log_filter"] {
            let mut missing = payload();
            missing.as_object_mut().unwrap().remove(field);
            assert!(decode(&serde_json::to_vec(&missing).unwrap(), FINGERPRINT).is_err());
            let mut wrong_type = payload();
            wrong_type[field] = json!(false);
            assert!(decode(&serde_json::to_vec(&wrong_type).unwrap(), FINGERPRINT).is_err());
        }
        let mut unknown = payload();
        unknown["extra"] = json!(0);
        assert!(decode(&serde_json::to_vec(&unknown).unwrap(), FINGERPRINT).is_err());
    }

    #[test]
    fn refuses_old_payload_duplicate_fields_trailing_data_and_invalid_utf8() {
        let old = json!({"tool": TOOL, "config_wire_fingerprint": FINGERPRINT});
        assert!(decode(&serde_json::to_vec(&old).unwrap(), FINGERPRINT).is_err());
        let valid = serde_json::to_string(&payload()).unwrap();
        let duplicate = format!("{{\"version\":2,{}", &valid[1..]);
        assert!(decode(duplicate.as_bytes(), FINGERPRINT).is_err());
        assert!(decode(format!("{valid} null").as_bytes(), FINGERPRINT).is_err());
        assert!(decode(b"\xff", FINGERPRINT).is_err());
        assert!(decode(b"{", FINGERPRINT).is_err());
    }
}
