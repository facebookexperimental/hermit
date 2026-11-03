//! Authoritative native startup configuration over a consumed temporary FD.
//!
//! This is configuration ownership only. It does not publish a Tool, readiness,
//! a native thread, a private phase, a continuation or a descriptor fallback.

use std::ffi::OsStr;
use std::io;
use std::os::fd::FromRawFd;
use std::os::fd::IntoRawFd;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::path::PathBuf;

use detcore::Config;

/// The native launch owner supplies this explicit selection. A missing
/// coordinator never changes a Coordinated launch into a local one.
#[derive(Clone, Copy, Eq, PartialEq)]
#[repr(u32)]
pub(super) enum Mode {
    Standalone = 1,
    Coordinated = 2,
}

#[derive(Clone, Copy)]
pub(super) enum Source {
    Coordinator,
    StandaloneJson,
    StandaloneParseFallback,
    StandaloneDefault,
}

pub(super) struct Configuration {
    pub config: Config,
    pub coordinator: Option<PathBuf>,
    pub source: Source,
}

/// Preserve the maintained native launch selection, including its strict
/// standalone JSON fallback. `Some(empty)` is a present, invalid control and
/// must reach the socket validator; it is never treated as absent.
pub(super) fn select(
    mode: Mode,
    control: Option<&OsStr>,
    fingerprint: Option<&OsStr>,
    standalone_json: Option<&str>,
) -> io::Result<Configuration> {
    let (mut config, coordinator, source) = match control {
        Some(path) => {
            let fingerprint = fingerprint.ok_or_else(|| {
                io::Error::other(
                    "native coordinator announced no Detcore configuration fingerprint",
                )
            })?;
            (
                receive(Path::new(path), fingerprint)?,
                Some(PathBuf::from(path)),
                Source::Coordinator,
            )
        }
        None if mode == Mode::Coordinated => {
            return Err(io::Error::other(
                "coordinated native startup requires a coordinator socket",
            ));
        }
        None => {
            let (config, source) = match standalone_json {
                Some(value) if !value.is_empty() => {
                    match detcore::decode_backend_config_json(value) {
                        Ok(config) => (config, Source::StandaloneJson),
                        Err(_) => (strict_default(), Source::StandaloneParseFallback),
                    }
                }
                _ => (strict_default(), Source::StandaloneDefault),
            };
            (config, None, source)
        }
    };
    // The existing native executor owns the external sequential scheduler.
    // These are its original physical facts, not an alternate syscall policy.
    config.max_timeslice = None;
    config.sequentialize_threads = true;
    config.backend.supports_parked_write_signal_interruption = false;
    config.backend.promotes_open_files_before_native_lifecycle = true;
    Ok(Configuration {
        config,
        coordinator,
        source,
    })
}

fn strict_default() -> Config {
    Config {
        sequentialize_threads: true,
        deterministic_io: true,
        max_timeslice: None,
        ..Config::default()
    }
}

/// `fingerprint` is the actual coordinator's announcement. Check it BEFORE
/// decoding its positional Config or any Global RPC. A present control never
/// falls back to JSON, an ambient socket, empty config or another runtime.
fn receive(path: &Path, fingerprint: &OsStr) -> io::Result<Config> {
    if let Some(mismatch) = detcore::config_fingerprint_mismatch(fingerprint) {
        return Err(io::Error::other(format!(
            "this Detcore native engine was built from a different tree than the hermit \
             running it: {mismatch}"
        )));
    }
    read_initial(path)
}

/// The maintained initial native config protocol: validate before socket;
/// receive exactly the full bounded frame; consume the FD with ONE close
/// attempt before either result. EINTR does not grant a second close attempt.
fn read_initial(path: &Path) -> io::Result<Config> {
    let bytes = path.as_os_str().as_bytes();
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    if bytes.is_empty() || bytes.len() >= address.sun_path.len() || bytes.contains(&0) {
        return Err(io::Error::from_raw_os_error(libc::EINVAL));
    }
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (destination, source) in address.sun_path.iter_mut().zip(bytes) {
        *destination = *source as libc::c_char;
    }
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // Immediately adopt ownership so every exit consumes this exact FD.
    let owned = unsafe { OwnedFd::from_raw_fd(fd) };
    let mut stream = UnixStream::from(owned);
    let length =
        (std::mem::offset_of!(libc::sockaddr_un, sun_path) + bytes.len() + 1) as libc::socklen_t;
    let result = if unsafe {
        libc::connect(fd, (&address as *const libc::sockaddr_un).cast(), length)
    } < 0
    {
        Err(io::Error::last_os_error())
    } else {
        reverie_rpc_transport::read_config_frame(&mut stream)
            .and_then(|payload| reverie_rpc_transport::codec::decode_exact(&payload))
            .map_err(io::Error::other)
    };
    let fd = stream.into_raw_fd();
    let close = if unsafe { libc::close(fd) } < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    };
    // Preserve the receive failure even if close also failed. A successful
    // receive with failed close does not return a usable configuration.
    match (result, close) {
        (Ok(config), Ok(())) => Ok(config),
        (Err(receive), Ok(())) => Err(receive),
        (Ok(_), Err(close)) => Err(close),
        (Err(receive), Err(close)) => Err(io::Error::new(
            receive.kind(),
            format!("{receive}; temporary config close also failed: {close}"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    use super::Source as ConfigSource;
    use super::*;

    // These are fixture facts, not another native runtime or selector. The
    // implementation-reporting assertion remains with its physical owner.
    fn backend_capabilities() -> reverie::BackendCapabilities {
        reverie::BackendCapabilities::DBT
    }

    fn default_dynamorio_config() -> Config {
        super::strict_default()
    }

    fn dynamorio_config_from(value: Option<&str>) -> (Config, ConfigSource) {
        let selected = super::select(super::Mode::Standalone, None, None, value)
            .expect("the admitted standalone selection returns its actual config");
        assert!(
            selected.coordinator.is_none(),
            "standalone selection must not invent a coordinator"
        );
        (selected.config, selected.source)
    }

    fn initial_dynamorio_config(
        mode: super::Mode,
        control: Option<&OsStr>,
        fingerprint: Option<&OsStr>,
    ) -> std::io::Result<(Config, ConfigSource)> {
        super::select(mode, control, fingerprint, None)
            .map(|selected| (selected.config, selected.source))
    }

    fn hermit_run_dynamorio_detconfig() -> (Config, String) {
        let mut sent = default_dynamorio_config();
        sent.seed = 7;
        sent.chaos = true;
        sent.backend = backend_capabilities();
        let json = detcore::encode_backend_config_json(
            detcore::to_legacy_backend_json(&sent).unwrap(),
            sent.backend,
        )
        .unwrap();
        (sent, json)
    }

    #[test]
    fn a_rich_configuration_round_trips_under_every_descriptor_mask() {
        for mask in 0_u8..8 {
            let (mut sent, _) = hermit_run_dynamorio_detconfig();
            sent.rng_seed = Some(13);
            sent.sched_seed = Some(11);
            sent.clock_multiplier = Some(1.5);
            sent.record_preemptions_to = Some(std::path::PathBuf::from(
                "/tmp/a,\"detect_host_clock_futex_timeouts\":true,\"backend_tracks_process_children\":{",
            ));
            sent.stacktrace_event = vec![(3, Some(std::path::PathBuf::from("/s")))];
            sent.backend.tool_shares_guest_descriptor_table = mask & 1 != 0;
            sent.backend.rediscovers_descriptors_after_exec = mask & 2 != 0;
            sent.backend.loopback_pollers_yield_to_peers = mask & 4 != 0;
            let json = detcore::encode_backend_config_json(
                detcore::to_legacy_backend_json(&sent).unwrap(),
                sent.backend,
            )
            .unwrap();
            assert_eq!(
                detcore::from_legacy_backend_json(&json).is_ok(),
                mask == 0 || mask == 7,
                "only coupled facts fit the legacy form: mask {mask}"
            );
            let decoded = detcore::decode_backend_config_json(&json).unwrap();
            assert_eq!(
                serde_json::to_string(&decoded).unwrap(),
                serde_json::to_string(&sent).unwrap(),
                "mask {mask}"
            );
        }
    }

    #[test]
    fn versioned_config_rejects_unknown_or_malformed_envelopes() {
        let (config, _) = hermit_run_dynamorio_detconfig();
        let legacy = detcore::to_legacy_backend_json(&config).unwrap();
        let legacy_backend = detcore::from_legacy_backend_json(&legacy).unwrap().backend;
        let backend = serde_json::to_string(&config.backend).unwrap();
        let envelope = |version: &str| {
            format!(
                "{{\"dynamorio_config_version\":{version},\"config\":{legacy},\"backend\":{backend}}}"
            )
        };
        for version in ["0", "2", "-1", "null", "\"1\""] {
            assert!(
                detcore::decode_backend_config_json(&envelope(version)).is_err(),
                "version {version} must be refused"
            );
            let tagged_legacy = legacy.replacen(
                '{',
                &format!("{{\"dynamorio_config_version\":{version},"),
                1,
            );
            assert_eq!(
                detcore::decode_backend_config_json(&tagged_legacy)
                    .unwrap()
                    .backend,
                legacy_backend,
                "an unknown legacy key must remain ignored"
            );
        }
        let duplicate = format!(
            "{{\"dynamorio_config_version\":1,\"dynamorio_config_version\":1,\"config\":{legacy},\"backend\":{backend}}}"
        );
        assert!(detcore::decode_backend_config_json(&duplicate).is_err());
        let missing_backend = format!("{{\"dynamorio_config_version\":1,\"config\":{legacy}}}");
        assert!(detcore::decode_backend_config_json(&missing_backend).is_err());
        let missing_config = format!("{{\"dynamorio_config_version\":1,\"backend\":{backend}}}");
        assert!(detcore::decode_backend_config_json(&missing_config).is_err());
        for duplicate in [
            format!("\"config\":{legacy}"),
            format!("\"backend\":{backend}"),
            "\"unknown\":true".to_owned(),
        ] {
            let json = envelope("1").replacen('{', &format!("{{{duplicate},"), 1);
            assert!(detcore::decode_backend_config_json(&json).is_err());
        }
    }

    #[test]
    fn versioned_config_preserves_independent_descriptor_facts_in_the_runtime() {
        for mask in 0_u8..8 {
            let (mut sent, _) = hermit_run_dynamorio_detconfig();
            sent.backend.tool_shares_guest_descriptor_table = mask & 1 != 0;
            sent.backend.rediscovers_descriptors_after_exec = mask & 2 != 0;
            sent.backend.loopback_pollers_yield_to_peers = mask & 4 != 0;
            let json = detcore::encode_backend_config_json(
                detcore::to_legacy_backend_json(&sent).unwrap(),
                sent.backend,
            )
            .unwrap();
            let (received, source) = dynamorio_config_from(Some(&json));
            assert!(
                matches!(source, ConfigSource::StandaloneJson),
                "mask {mask}"
            );
            assert_eq!(received.backend, sent.backend, "mask {mask}");
            assert_eq!(received.max_timeslice, None);
            assert!(received.sequentialize_threads);
        }
    }

    #[test]
    fn a_rejected_versioned_config_keeps_the_existing_parse_fallback_policy() {
        let (sent, _) = hermit_run_dynamorio_detconfig();
        let legacy = detcore::to_legacy_backend_json(&sent).unwrap();
        let backend = serde_json::to_string(&sent.backend).unwrap();
        for json in [
            format!("{{\"dynamorio_config_version\":2,\"config\":{legacy},\"backend\":{backend}}}"),
            format!("{{\"dynamorio_config_version\":1,\"config\":{legacy}}}"),
        ] {
            assert!(detcore::decode_backend_config_json(&json).is_err());
            let (received, source) = dynamorio_config_from(Some(&json));
            assert!(matches!(source, ConfigSource::StandaloneParseFallback));
            let (expected, _) = dynamorio_config_from(None);
            // Defaults include a freshly sampled epoch; the fallback policy
            // is established by the source, capabilities and execution model.
            assert_eq!(received.backend, expected.backend);
            assert_eq!(received.max_timeslice, None);
            assert!(received.sequentialize_threads);
        }
    }

    #[test]
    fn a_dynamorio_run_without_a_cli_configuration_keeps_its_previous_backend_facts() {
        let (_, json) = hermit_run_dynamorio_detconfig();
        // A legacy key of the wrong type failed the parse when it was a
        // `Config` field, and still does.
        let mistyped = json.replacen("\"backend_is_kvm\":false", "\"backend_is_kvm\":0", 1);
        assert_ne!(mistyped, json);
        for (value, fallback) in [
            (None, false),
            (Some(""), false),
            (Some("not json"), true),
            (Some(mistyped.as_str()), true),
        ] {
            let (config, source) = dynamorio_config_from(value);
            if fallback {
                assert!(
                    matches!(source, ConfigSource::StandaloneParseFallback),
                    "{value:?}"
                );
            } else {
                assert!(
                    matches!(source, ConfigSource::StandaloneDefault),
                    "{value:?}"
                );
            }
            let backend = config.backend;
            assert!(!backend.supports_parked_write_signal_interruption);
            assert!(!backend.needs_killed_thread_rpc_cancellation);
            assert!(backend.tracks_process_children);
            assert!(backend.runs_exit_robust_list);
            assert!(!backend.requires_thread_directed_process_signals);
            // An unprepared configuration's capabilities: ptrace's, with the
            // facts only a host sets for its backend left off, and the
            // promotion this runtime performs whatever the source says.
            let mut expected = Config::default().backend;
            expected.supports_parked_write_signal_interruption = false;
            expected.promotes_open_files_before_native_lifecycle = true;
            assert_eq!(backend, expected);
            assert_ne!(backend, backend_capabilities());
            assert_eq!(config.max_timeslice, None);
            assert!(config.sequentialize_threads);
        }
    }

    #[test]
    fn a_standalone_configuration_round_trips_through_its_environment_encoding() {
        let (sent, json) = hermit_run_dynamorio_detconfig();
        assert!(detcore::from_legacy_backend_json(&json).is_err());
        let (received, source) = dynamorio_config_from(Some(&json));
        assert!(matches!(source, ConfigSource::StandaloneJson));
        assert_eq!(received.backend, backend_capabilities());
        assert_eq!(
            serde_json::to_string(&received).unwrap(),
            serde_json::to_string(&sent).unwrap()
        );
    }

    #[test]
    fn a_legacy_cli_configuration_keeps_the_backend_facts_it_carries() {
        let (mut sent, _) = hermit_run_dynamorio_detconfig();
        // Preserve the pre-discovery DYNAMORIO input: legacy keys, with no envelope
        // whose typed capabilities would take precedence over their projection.
        sent.backend.tool_shares_guest_descriptor_table = false;
        sent.backend.rediscovers_descriptors_after_exec = false;
        let json = detcore::to_legacy_backend_json(&sent).unwrap();
        let key = "\"backend_requires_thread_directed_process_signals\"";
        let edited = json.replacen(&format!("{key}:true"), &format!("{key}:false"), 1);
        assert_ne!(edited, json);
        let (config, source) = dynamorio_config_from(Some(&edited));
        assert!(matches!(source, ConfigSource::StandaloneJson));
        let mut expected = sent.backend;
        expected.requires_thread_directed_process_signals = false;
        assert_eq!(config.backend, expected);

        let key = "\"backend_supports_parked_write_signal_interruption\"";
        let edited = json.replacen(&format!("{key}:false"), &format!("{key}:true"), 1);
        assert_ne!(edited, json);
        let (config, _) = dynamorio_config_from(Some(&edited));
        assert_eq!(config.backend, sent.backend);

        let mut object: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(&json).unwrap();
        object.remove("backend_tracks_process_children").unwrap();
        let edited = serde_json::to_string(&object).unwrap();
        let (config, source) = dynamorio_config_from(Some(&edited));
        assert!(matches!(source, ConfigSource::StandaloneJson));
        let mut expected = sent.backend;
        expected.tracks_process_children = true;
        assert_eq!(config.backend, expected);
    }

    #[test]
    fn startup_authority_refuses_missing_or_invalid_control_without_fallback() {
        use super::Mode as DynamoRioStartupMode;

        assert!(initial_dynamorio_config(DynamoRioStartupMode::Coordinated, None, None).is_err());
        let ours = detcore::config_wire_fingerprint();
        for mode in [
            DynamoRioStartupMode::Standalone,
            DynamoRioStartupMode::Coordinated,
        ] {
            // A present control is authoritative even in standalone mode.
            // An invalid address must not become a successful JSON/default path.
            assert!(
                initial_dynamorio_config(mode, Some(OsStr::new("")), Some(OsStr::new(&ours)))
                    .is_err()
            );
        }
    }

    #[test]
    fn startup_refuses_another_trees_coordinator_before_reading_its_frame() {
        use super::Mode as DynamoRioStartupMode;

        // The same production selector checks the actual coordinator's
        // fingerprint before attempting its initial frame. This closure only
        // preserves the former string-error call shape for the old oracles.
        let initial_dynamorio_config = |mode, control, fingerprint| {
            super::select(mode, control, fingerprint, None)
                .map(|selected| (selected.config, selected.source))
                .map_err(|error| error.to_string())
        };
        let control = Some(OsStr::new("/nonexistent/coordinator.sock"));
        let ours = detcore::config_wire_fingerprint();
        for mode in [
            DynamoRioStartupMode::Standalone,
            DynamoRioStartupMode::Coordinated,
        ] {
            let refusal =
                initial_dynamorio_config(mode, control, Some(OsStr::new("0000000000000000")))
                    .err()
                    .expect("another tree's coordinator is refused");
            assert!(
                refusal.contains("built from a different tree than the hermit running it")
                    && refusal.contains(&format!(
                        "fingerprint is {ours}, Hermit's is 0000000000000000"
                    )),
                "{refusal}"
            );
            let unannounced = initial_dynamorio_config(mode, control, None)
                .err()
                .expect("a coordinator that announced nothing is refused");
            assert!(
                unannounced
                    .contains("native coordinator announced no Detcore configuration fingerprint"),
                "{unannounced}"
            );
            let error = initial_dynamorio_config(mode, control, Some(OsStr::new(&ours)))
                .err()
                .expect("nothing listens at the control");
            assert!(
                !error.contains("different tree") && !error.contains("announced no"),
                "{error}"
            );
        }
    }
}
