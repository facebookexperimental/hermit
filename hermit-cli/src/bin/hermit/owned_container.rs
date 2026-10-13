/* Copyright (c) Meta Platforms, Inc. and affiliates. */

//! CLI-only original container ownership. Library failures never exit the process.
//! Initial workload/result draining is blocking. The two explicit settlement
//! attempts below bound only finalization; configured CLI alarms/outer runners
//! bound a stalled initial drain. The caller must satisfy Container's pre-thread
//! and no-competing-reaper requirements.

use std::any::Any;
use std::cell::RefCell;
use std::mem::ManuallyDrop;
use std::rc::Rc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use hermit::Error;
use hermit::SerializableError;
use reverie::process::ChildCleanupObservation;
use reverie::process::Container;
use reverie::process::ExitStatus;
use reverie::process::OwnedFinalization;
use reverie::process::OwnedFinalize;
use reverie::process::OwnedRunFailure;
use reverie::process::RunError;
use reverie::process::StartupError;
use reverie::process::StartupOwnedFailure;
use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde::de::Error as _;

/// How long the parent waits for the container child to EXIT after it has
/// published its result, and then waits again after cancelling it.
///
/// The run is already over at this point; only teardown remains (the child's
/// own exit and, as PID 1 of its namespace, reaping everything left in it).
/// Publication-to-reap latency was measured on the development host recorded
/// in docs/TESTING_ENVIRONMENTS.md (316 CPUs), for trace-level
/// `hermit run --record-preemptions-to` of rustbin_heap_ptrs on a debug build,
/// every run through safehermit:
///
/// | concurrent runs | samples | p50 | p99 | max |
/// |---|---|---|---|---|
/// | 1 | 30 | 2.4 ms | 2.9 ms | 2.9 ms |
/// | 64 | 640 | 7.6 ms | 63 ms | 64 ms |
/// | 158 | 948 | 9.5 ms | 84 ms | 161 ms |
/// | 316 | 1264 | 18 ms | 1105 ms | 1222 ms |
/// | 316, repeated | 1264 | 15 ms | 94 ms | 100 ms |
/// | 316, `--no-namespace` | 1264 | 179 ms | 747 ms | 1033 ms |
/// | 316, plus 316 CPU + 16 IO stress workers | 948 | 15 ms | 1386 ms | 1520 ms |
/// | 632 | 1896 | 65 ms | 1223 ms | 1409 ms |
/// | 1264 | 2528 | 20 ms | 893 ms | 1096 ms |
///
/// All 10,782 runs exited, and none took longer than 1.52 s, so the former
/// 2 s bound held in every measured condition. The tail's cause was not
/// identified: two identical 316-run sweeps differed 12x in their maximum, and
/// the `--no-namespace` runs were bimodal with a 10x higher median. What was
/// NOT measured is the failure itself: a fully parallel fbsource Buck run
/// (mostly replay and chaos-replay targets) exceeded 2 s in 49 tests, which were
/// then reported as cancelled although they had completed
/// (<https://github.com/rrnewton/hermit/issues/3414>). How far beyond 2 s that
/// teardown went is unknown.
///
/// 20 s is therefore a margin chosen without that measurement: 13x the largest
/// measured tail. It only costs time for a child that genuinely does not exit
/// after publishing. The same budget then applies after the SIGKILL, because
/// the same teardown (the child's exit and its namespace reaping) has to finish
/// again; that post-cancel wait was never exercised in the measurements. A
/// child that never exits is therefore reported after up to 2 x 20 s, where the
/// old bound took 2 x 2 s. `HERMIT_FINALIZE_BUDGET_MS` overrides both waits.
const FINALIZE_BUDGET: Duration = Duration::from_secs(20);
const FINALIZE_BUDGET_ENV: &str = "HERMIT_FINALIZE_BUDGET_MS";
/// Test-only: the container child sleeps this many milliseconds after
/// publishing its result, so the parent's finalize budget can be exercised.
/// Read in the child from the hermit process's environment, never from the
/// guest's; an unset or unparsable value means no stall.
const STALL_AFTER_PUBLICATION_ENV: &str = "HERMIT_INTERNAL_STALL_AFTER_PUBLICATION_MS";

/// The finalize budget for this invocation, refusing a malformed override
/// rather than silently falling back to the default.
fn finalize_budget() -> Result<Duration, Error> {
    let Some(raw) = std::env::var_os(FINALIZE_BUDGET_ENV) else {
        return Ok(FINALIZE_BUDGET);
    };
    let millis = raw
        .to_str()
        .filter(|text| !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit()))
        .and_then(|text| text.parse::<u64>().ok())
        .filter(|millis| *millis > 0)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "{FINALIZE_BUDGET_ENV}={} is not a positive number of milliseconds",
                raw.to_string_lossy()
            )
        })?;
    Ok(Duration::from_millis(millis))
}
const RETAINED_OWNER_REMEDY: &str = "Stop the affected run through its process supervisor, \
    confirm its entire child tree has stopped, then start a fresh Hermit process; \
    retrying in this process cannot release the retained owner.";
static RETAINED: AtomicBool = AtomicBool::new(false);
static NEXT_OWNER: AtomicU64 = AtomicU64::new(1);
thread_local! {
    // Deliberate CLI retention, including on origin-thread exit. These owners
    // require process supervision; there is no detached reaper or generic exit.
    static OWNERS: RefCell<ManuallyDrop<Vec<Box<dyn Any>>>> =
        const { RefCell::new(ManuallyDrop::new(Vec::new())) };
}

#[derive(Debug)]
pub(super) struct ParentCleanupUnconfirmed {
    key: u64,
    pid: i32,
    observation: ChildCleanupObservation,
    cause: Option<OwnedRunFailure>,
    reported_kind: Option<hermit::FailureKind>,
    resources: String,
    primary_display: Option<String>,
}

impl ParentCleanupUnconfirmed {
    fn context(mut self, primary: Error) -> Error {
        // The copied headline is display-only. Keep the original typed error
        // underneath this typed retention context for classification/downcasts.
        self.primary_display = Some(primary.to_string());
        // Not `eprintln!`, which panics when stderr's reader has gone (EPIPE)
        // and would replace the primary failure's status with 101.
        use std::io::Write as _;
        let _ = writeln!(std::io::stderr(), "HERMIT_CLEANUP_UNCONFIRMED: {self}");
        primary.context(self)
    }
}

impl std::fmt::Display for ParentCleanupUnconfirmed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(primary) = &self.primary_display {
            write!(f, "{primary}; ")?;
        }
        write!(
            f,
            "container cleanup unconfirmed (retained owner {}, child {}, observation {:?}, original owner failure {:?}, reported primary {:?}); keep backing resources: {}; {RETAINED_OWNER_REMEDY}",
            self.key, self.pid, self.observation, self.cause, self.reported_kind, self.resources
        )
    }
}
impl std::error::Error for ParentCleanupUnconfirmed {}

/// Only the CLI init owns this fallback. Reverie drops it after flushing and
/// closing the serialized error. _exit is a process/group exit; a raw clone
/// callback's ordinary return would exit only its current thread.
struct PublishedFailureExit {
    unresolved: bool,
    _alarm: Option<super::run_timeout::RunTimeoutFallback>,
}
impl Drop for PublishedFailureExit {
    fn drop(&mut self) {
        // Reverie drops this only after the result is flushed and closed, so a
        // stall here is a stall after publication.
        if let Some(millis) = std::env::var(STALL_AFTER_PUBLICATION_ENV)
            .ok()
            .and_then(|text| text.parse::<u64>().ok())
        {
            std::thread::sleep(Duration::from_millis(millis));
        }
        if self.unresolved {
            unsafe { libc::_exit(125) }
        }
    }
}

#[derive(Debug)]
struct RejectSuccess;
impl<'de> Deserialize<'de> for RejectSuccess {
    fn deserialize<D: Deserializer<'de>>(_: D) -> Result<Self, D::Error> {
        Err(D::Error::custom(
            "a failed child cannot publish a success value",
        ))
    }
}

/// Never deserialize T from a failed owner. Accept only this same-image error
/// envelope, completely consumed, with an explicit unresolved-owner diagnostic.
fn decode_failed_error(bytes: &[u8]) -> Option<SerializableError> {
    let (decoded, used) = bincode::serde::decode_from_slice::<
        Result<Result<RejectSuccess, SerializableError>, StartupError>,
        _,
    >(bytes, bincode::config::legacy())
    .ok()?;
    match decoded {
        Ok(Err(error)) if used == bytes.len() && error.cleanup_stage().is_some() => Some(error),
        _ => None,
    }
}

fn retain<T: 'static, F: 'static>(
    run: OwnedFinalization<T>,
    factory: F,
    resources: String,
    cause: OwnedRunFailure,
) -> ParentCleanupUnconfirmed {
    let pid = run.cleanup().child_pid().as_raw();
    let observation = run.cleanup().observation();
    retain_owned(run, factory, resources, pid, observation, Some(cause), None)
}

// Complete has already consumed its original successful wait. Retain its actual
// receipt/bytes or decode refusal with the same factory, without inventing a live
// wait owner. Failed/Pending retain their original OwnedFinalization instead.
fn retain_owned<T: 'static, F: 'static>(
    owner: T,
    factory: F,
    resources: String,
    pid: i32,
    observation: ChildCleanupObservation,
    cause: Option<OwnedRunFailure>,
    reported_kind: Option<hermit::FailureKind>,
) -> ParentCleanupUnconfirmed {
    let diagnostic = ParentCleanupUnconfirmed {
        key: NEXT_OWNER.fetch_add(1, Ordering::Relaxed),
        pid,
        observation,
        cause,
        reported_kind,
        resources,
        primary_display: None,
    };
    RETAINED.store(true, Ordering::Release);
    OWNERS.with(|owners| owners.borrow_mut().push(Box::new((owner, factory))));
    diagnostic
}

/// On success return the original parent guards, allowing record -> replay to
/// move them onward. On unresolved failure retain the factory, guards, original
/// wait and exact bytes together. `private_pid_namespace` is a caller-supplied
/// fact about the actual Container configuration, never inferred from PID == 1.
pub(super) fn run<G, F, T>(
    container: &mut Container,
    guards: G,
    resources: String,
    private_pid_namespace: bool,
    site: &'static str,
    timeout: Option<Duration>,
    work: F,
) -> Result<(T, G), Error>
where
    G: 'static,
    F: FnMut(&mut G) -> Result<T, Error> + 'static,
    T: Serialize + DeserializeOwned + 'static,
{
    let finalize_budget = finalize_budget()?;
    if RETAINED.load(Ordering::Acquire) {
        anyhow::bail!(
            "a prior CLI container owner is unresolved; backing resources remain retained; {RETAINED_OWNER_REMEDY}"
        );
    }
    let state = Rc::new(RefCell::new((guards, work)));
    let mut factory = {
        let state = Rc::clone(&state);
        move || {
            let mut alarm = None;
            let result = super::container::catch_child_panic_at(site, || {
                super::container::stall_container_init_before_arming_if_asked();
                super::container::arm_container_init_guards().map_err(Error::from)?;
                if let Some(limit) = timeout {
                    let after = limit
                        .checked_add(super::run_timeout::RUN_TIMEOUT_UNWIND_GRACE)
                        .ok_or_else(|| anyhow::anyhow!("--timeout grace overflow"))?;
                    alarm = Some(super::run_timeout::RunTimeoutFallback::arm(after)?);
                }
                let mut state = state.borrow_mut();
                let (guards, work) = &mut *state;
                work(guards)
            });
            let unresolved = result
                .as_ref()
                .err()
                .is_some_and(|e| e.cleanup_stage().is_some());
            if result
                .as_ref()
                .err()
                .is_some_and(|e| e.kind() == hermit::FailureKind::RunTimeout)
            {
                super::run_timeout::stall_the_unwind_if_asked();
            }
            (
                result,
                PublishedFailureExit {
                    unresolved,
                    _alarm: alarm,
                },
            )
        }
    };
    // The container init inherits this at clone, and it covers the setup Reverie
    // runs before the factory above can arm the init's own handlers.
    let stop_signal_bridge = super::container::ContainerInitStopSignalBridge::install()?;
    let started = container.run_with_deferred_drop_owned(&mut factory);
    drop(stop_signal_bridge);
    let (first, child_pid) = match started {
        Ok(run) => {
            let pid = run.cleanup().child_pid().as_raw();
            (run.finalize_until(Instant::now() + finalize_budget), pid)
        }
        Err(StartupOwnedFailure::BeforeClone { cause }) => return Err(Error::new(cause)),
        Err(StartupOwnedFailure::AfterClone { cause, run }) => {
            let pid = run.cleanup().child_pid().as_raw();
            (
                OwnedFinalize::Failed {
                    cause,
                    cleanup: run,
                },
                pid,
            )
        }
    };
    // At most ONE cancellation attempt. A successful initial wait never cancels;
    // failed acquisition and lingering workers preserve their original failure.
    let final_result = match first {
        OwnedFinalize::Complete(result) => OwnedFinalize::Complete(result),
        OwnedFinalize::Pending(run) => run.cancel_until(Instant::now() + finalize_budget),
        OwnedFinalize::Failed { cause, cleanup } => {
            if matches!(
                cleanup.cleanup().observation(),
                ChildCleanupObservation::Reaped(_)
                    | ChildCleanupObservation::ExitedWithoutWaitStatus
            ) {
                OwnedFinalize::Failed { cause, cleanup }
            } else {
                cleanup.retry_until(Instant::now() + finalize_budget)
            }
        }
    };
    match final_result {
        OwnedFinalize::Complete(result) => {
            let status = result.status();
            // Decoding consumes the receipt. Keep the exact frame when a
            // no-namespace error could require retention after that decode.
            let original_bytes = (!private_pid_namespace).then(|| result.encoded_bytes().to_vec());
            let decoded = match result.decode() {
                Ok(value) => value,
                Err(failure) => {
                    let cause = failure.cause();
                    let primary = anyhow::anyhow!(
                        "container result decode refused: {:?}: {:?}",
                        cause,
                        failure.detail()
                    );
                    if !private_pid_namespace {
                        let retained = retain_owned(
                            failure,
                            factory,
                            resources,
                            child_pid,
                            ChildCleanupObservation::Reaped(status),
                            Some(cause),
                            None,
                        );
                        return Err(retained.context(primary));
                    }
                    return Err(primary);
                }
            };
            // Actual successful wait is required before ordinary decoding, but
            // only a successful workload promises it settled its own workers.
            // A reported error is not proof of no-namespace tree retirement.
            let value = match decoded {
                Ok(value) => value,
                Err(reported) => {
                    let kind = reported.kind();
                    let primary =
                        super::container::classify_container_result::<()>(Ok(Err(reported)))
                            .expect_err("an error envelope remains an error");
                    if !private_pid_namespace {
                        let retained = retain_owned(
                            (status, original_bytes),
                            factory,
                            resources,
                            child_pid,
                            ChildCleanupObservation::Reaped(status),
                            None,
                            Some(kind),
                        );
                        return Err(retained.context(primary));
                    }
                    return Err(primary);
                }
            };
            drop(factory);
            let state = Rc::try_unwrap(state)
                .ok()
                .expect("the original factory was dropped");
            let (guards, _) = state.into_inner();
            Ok((value, guards))
        }
        OwnedFinalize::Pending(cleanup) => {
            let cause = cleanup.failure().unwrap_or(OwnedRunFailure::Cancelled);
            let retained = retain(cleanup, factory, resources, cause);
            eprintln!("HERMIT_CLEANUP_UNCONFIRMED: {retained}");
            Err(Error::new(retained))
        }
        OwnedFinalize::Failed { cause, cleanup } => {
            let observation = cleanup.cleanup().observation();
            let reported = if cleanup.result_eof()
                && observation == ChildCleanupObservation::Reaped(ExitStatus::Exited(125))
                && cause == OwnedRunFailure::ChildStatus(ExitStatus::Exited(125))
            {
                decode_failed_error(cleanup.provisional_bytes())
            } else {
                None
            };
            let actual_exit = matches!(observation, ChildCleanupObservation::Reaped(_));
            // A direct-child exit never proves arbitrary descendant retirement;
            // an absent/malformed frame cannot strengthen that evidence.
            let must_retain = !actual_exit || !private_pid_namespace;
            let primary = if let Some(error) = reported {
                eprintln!(
                    "HERMIT_CLEANUP_UNCONFIRMED: backend phase {:?}; namespace retirement observed={}; no backend/recorder cleanup success is claimed",
                    error.cleanup_stage(),
                    private_pid_namespace && actual_exit
                );
                super::container::classify_container_result::<()>(Ok(Err(error)))
                    .expect_err("an error envelope remains an error")
            } else {
                match observation {
                    ChildCleanupObservation::Reaped(status)
                        if matches!(
                            cause,
                            OwnedRunFailure::ChildStatus(_)
                                | OwnedRunFailure::Startup(StartupError::MissingResult)
                        ) && !status.success() =>
                    {
                        super::container::classify_container_result::<()>(Err(
                            RunError::ExitStatus(status),
                        ))
                        .expect_err("an abnormal child status remains an error")
                    }
                    // Name the budget: this child published its result and
                    // then did not exit in time, so hermit killed it. Without
                    // the budget in the message it reads as a guest SIGKILL.
                    _ if cause == OwnedRunFailure::Cancelled && cleanup.result_eof() => {
                        anyhow::anyhow!(
                            "container child published its result but did not exit within \
                             the {finalize_budget:?} finalize budget ({FINALIZE_BUDGET_ENV} \
                             overrides it), so hermit cancelled it and discarded the published \
                             result; this error reports a teardown stall and says nothing about \
                             the guest's own outcome; actual child: {observation:?}"
                        )
                    }
                    _ => anyhow::anyhow!(
                        "container result failed: {cause:?}; actual child: {observation:?}"
                    ),
                }
            };
            if must_retain {
                let retained = retain(cleanup, factory, resources, cause);
                // The original primary remains downcastable; the retained-owner
                // context cannot rename a timeout, signal, or child exit.
                Err(retained.context(primary))
            } else {
                drop(cleanup);
                drop(factory);
                Err(primary)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reported(
        kind: hermit::FailureKind,
        stage: Option<hermit::HermitCleanupStage>,
    ) -> SerializableError {
        // A wire-format seam, not a fabricated backend outcome/retirement.
        serde_json::from_value(serde_json::json!({
            "error": "original diagnostic", "context": ["later timeout or unsupported syscall prose"],
            "kind": kind, "cleanup_stage": stage,
        })).unwrap()
    }
    fn encode(value: Result<Result<u64, SerializableError>, StartupError>) -> Vec<u8> {
        bincode::serde::encode_to_vec(value, bincode::config::legacy()).unwrap()
    }

    #[test]
    fn failed_wire_accepts_only_complete_staged_error_and_never_success() {
        let error = reported(
            hermit::FailureKind::Error,
            Some(hermit::HermitCleanupStage::PtraceCleanup),
        );
        let frame = encode(Ok(Err(error.clone())));
        assert_eq!(decode_failed_error(&frame), Some(error));
        for end in 0..frame.len() {
            assert!(decode_failed_error(&frame[..end]).is_none(), "prefix {end}");
        }
        let mut trailing = frame;
        trailing.push(0);
        assert!(decode_failed_error(&trailing).is_none());
        assert!(decode_failed_error(&encode(Ok(Ok(73)))).is_none());
        assert!(decode_failed_error(&encode(Err(StartupError::Protocol))).is_none());
        assert!(
            decode_failed_error(&encode(Ok(Err(reported(hermit::FailureKind::Error, None)))))
                .is_none()
        );
        assert!(decode_failed_error(&[255; 64]).is_none());
    }

    #[test]
    fn fallback_error_reapplies_primary_cli_class_and_exit() {
        fn diagnostic(kind: Option<hermit::FailureKind>) -> ParentCleanupUnconfirmed {
            // Formatting/classification seam only: no fabricated cleanup result
            // is passed to a production owner or admitted as tree retirement.
            ParentCleanupUnconfirmed {
                key: 7,
                pid: 123,
                observation: ChildCleanupObservation::Reaped(ExitStatus::Exited(0)),
                cause: None,
                reported_kind: kind,
                resources: "test backing".to_owned(),
                primary_display: None,
            }
        }
        for (kind, code, marker) in [
            (
                hermit::FailureKind::Error,
                125,
                "HERMIT_INTERNAL_FAILURE class=cli-error",
            ),
            (
                hermit::FailureKind::Panic,
                125,
                "HERMIT_INTERNAL_FAILURE class=container-child-panic",
            ),
            (
                hermit::FailureKind::PolicyRefusal,
                122,
                "HERMIT_POLICY_REFUSAL class=policy-refusal",
            ),
            (
                hermit::FailureKind::SkidOvershoot { count: 7 },
                122,
                "HERMIT_POLICY_REFUSAL class=policy-refusal cause=skid-overshoot count=7",
            ),
            (
                hermit::FailureKind::RunTimeout,
                124,
                "HERMIT_RUN_TIMEOUT class=run-timeout",
            ),
        ] {
            let error = decode_failed_error(&encode(Ok(Err(reported(
                kind,
                Some(hermit::HermitCleanupStage::GlobalStateCleanup),
            )))))
            .unwrap();
            assert_eq!(error.kind(), kind);
            let error = super::super::container::classify_container_result::<()>(Ok(Err(error)))
                .unwrap_err();
            assert_eq!(crate::failure_exit_code(&error), code);
            assert_eq!(crate::classify_failure(&error), marker);
            assert!(format!("{error:#}").contains("original diagnostic"));
            let primary_display = error.to_string();
            let error = diagnostic(Some(kind)).context(error);
            assert_eq!(crate::failure_exit_code(&error), code);
            assert_eq!(crate::classify_failure(&error), marker);
            assert!(
                error
                    .to_string()
                    .starts_with(&format!("{primary_display}; "))
            );
            assert!(error.to_string().contains(RETAINED_OWNER_REMEDY));
            assert!(format!("{error:#}").contains("original diagnostic"));
            assert_eq!(
                error
                    .downcast_ref::<ParentCleanupUnconfirmed>()
                    .unwrap()
                    .reported_kind,
                Some(kind)
            );
        }
        assert!(diagnostic(None).to_string().contains(RETAINED_OWNER_REMEDY));
        let original = Error::new(std::io::Error::from_raw_os_error(libc::EIO));
        let address = original.downcast_ref::<std::io::Error>().unwrap() as *const _;
        let error = diagnostic(None).context(original);
        let preserved = error.downcast_ref::<std::io::Error>().unwrap();
        assert_eq!(preserved as *const _, address);
        assert_eq!(preserved.raw_os_error(), Some(libc::EIO));
    }

    #[test]
    fn invalid_125_frame_keeps_actual_child_failure_class() {
        assert!(decode_failed_error(&encode(Ok(Ok(73)))).is_none());
        let error = super::super::container::classify_container_result::<()>(Err(
            RunError::ExitStatus(ExitStatus::Exited(125)),
        ))
        .unwrap_err();
        assert_eq!(crate::failure_exit_code(&error), 125);
        assert_eq!(
            crate::classify_failure(&error),
            "HERMIT_INTERNAL_FAILURE class=container-child-exit status=Exited(125)"
        );
    }
}
