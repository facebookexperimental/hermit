//! Read-only admission for the exact image consumed by selected DAG commands.
//!
//! This runs after plan/cache returns. It does not prepare images or reserve
//! them: the wrapper repeats the same bounded check before every container.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;
use std::process::ExitStatus;

use dagrun::DagConfig;

use crate::COULD_NOT_RUN_EXIT_CODE;
use crate::RunSummary;
use crate::validate_admission::BoundExecutionPlan;

const WRAPPER: &str = "./ci/hermetic/run-in-pinned-root.sh";

#[derive(Debug)]
struct Probe {
    env: BTreeMap<String, String>,
    consumers: Vec<String>,
}

fn selected_probes(cfg: &DagConfig, second: Option<&DagConfig>) -> Result<Vec<Probe>, String> {
    let mut groups: BTreeMap<BTreeMap<String, String>, Vec<String>> = BTreeMap::new();
    for cfg in std::iter::once(cfg).chain(second) {
        for step in &cfg.steps {
            if step.skip_reason.is_some() {
                continue;
            }
            // The committed generator renders a direct wrapper invocation,
            // not an arbitrary shell program that might mention its pathname.
            // Inspect that executable, never the node name/label/payload text.
            // The driver separately authenticates the complete selected graph.
            let argv = match shell_words::split(&step.cmd) {
                Ok(argv) => argv,
                Err(error) if step.cmd.trim_start().starts_with(WRAPPER) => {
                    return Err(format!(
                        "{}: invalid pinned-root quoting: {error}",
                        step.tag()
                    ));
                }
                Err(_) => continue,
            };
            if argv.first().map(String::as_str) != Some(WRAPPER) {
                continue;
            }
            let boundary = argv
                .iter()
                .position(|arg| arg == "--")
                .ok_or_else(|| format!("{}: missing pinned-root payload boundary", step.tag()))?;
            let tail = &argv[boundary..];
            if tail.len() != 6
                || tail[1] != "bash"
                || tail[2] != "-c"
                || tail[4] != "bash"
                || tail[3] != hermit_manifest_plan::validation_dag::PINNED_ROOT_COMMAND_GUARD
                || argv[1..boundary]
                    .iter()
                    .any(|arg| arg == "--digest" || arg == "--check-image")
            {
                return Err(format!(
                    "{}: unrecognized canonical pinned-root invocation; cannot bind its image",
                    step.tag()
                ));
            }
            // Preserve every declared outer environment value, including any
            // future Podman storage setting. Only identical environments share
            // a probe. All other caller environment is inherited unchanged.
            groups.entry(step.env.clone()).or_default().push(step.tag());
        }
    }
    Ok(groups
        .into_iter()
        .map(|(env, consumers)| Probe { env, consumers })
        .collect())
}

fn probe_command(root: &Path, probe: &Probe) -> Command {
    let mut command = Command::new(root.join(WRAPPER));
    command
        .arg("--check-image")
        .current_dir(root)
        .envs(&probe.env);
    command
}

fn refusal(profile: &str, reason: String) -> Box<RunSummary> {
    let mut summary = RunSummary::refused(
        COULD_NOT_RUN_EXIT_CODE,
        profile,
        "selected pinned-root image",
        vec![
            reason,
            "No DAG node or product test was executed; no passing receipt was produced.".into(),
        ],
    );
    summary.executed_tests = Some(0);
    summary.passed_tests = Some(0);
    Box::new(summary)
}

fn admit_with(
    plan: &BoundExecutionPlan,
    profile: &str,
    mut inspect: impl FnMut(&Probe) -> std::io::Result<ExitStatus>,
) -> Result<(), Box<RunSummary>> {
    let probes = selected_probes(&plan.cfg, plan.second.as_ref())
        .map_err(|error| refusal(profile, error))?;
    for probe in probes {
        let status = inspect(&probe).map_err(|error| {
            refusal(
                profile,
                format!(
                    "Cannot start the read-only image probe for {}: {error}",
                    probe.consumers.join(", ")
                ),
            )
        })?;
        if !status.success() {
            return Err(refusal(
                profile,
                format!(
                    "Read-only exact-image probe returned {status} for selected consumers [{}]; its raw stdout/stderr are retained above. The wrapper distinguishes absence from unavailable inspection; no image preparation or fallback was attempted.",
                    probe.consumers.join(", ")
                ),
            ));
        }
    }
    Ok(())
}

pub(crate) fn admit(
    root: &Path,
    plan: &BoundExecutionPlan,
    profile: &str,
) -> Result<(), Box<RunSummary>> {
    // Inherit the raw streams rather than capturing pipes whose EOF could be
    // held by another process. The shared wrapper owns a 10s query bound plus
    // 2s forced-stop grace; status() waits for that bounded command.
    admit_with(plan, profile, |probe| probe_command(root, probe).status())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::process::ExitStatusExt;
    use std::process::Output;

    use super::*;

    fn root() -> &'static Path {
        Path::new(file!())
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .parent()
            .unwrap()
    }

    fn observed(case: &str, output: Output) -> Output {
        eprintln!(
            "PINNED_IMAGE_CONTROL {}",
            serde_json::json!({
                "case": case, "raw_exit": output.status.code(), "signal": output.status.signal(),
                "stdout_bytes": output.stdout, "stderr_bytes": output.stderr,
            })
        );
        output
    }

    struct Fixture {
        dir: tempfile::TempDir,
        path: std::ffi::OsString,
        reference: String,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let here = dir.path().join("ci/hermetic");
            std::fs::create_dir_all(&here).unwrap();
            for name in ["run-in-pinned-root.sh", "image.digest"] {
                std::fs::copy(root().join("ci/hermetic").join(name), here.join(name)).unwrap();
            }
            let bin = dir.path().join("bin");
            std::fs::create_dir(&bin).unwrap();
            let podman = bin.join("podman");
            std::fs::write(
                &podman,
                r#"#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >> "$PROBE_CALLS"
printf '%s\n' "${CONTAINERS_STORAGE_CONF:-inherited}" >> "$PROBE_STORES"
if [[ $# == 3 && $1 == image && $2 == exists ]]; then
    case ${PROBE_MODE:-present} in
        error) printf 'exact inspection error\n' >&2; exit "${PROBE_ERROR:-125}" ;;
        hang) cat "/proc/$$/stat" > "$PROBE_GENERATION"; exec sleep 300 ;;
        ignore-term) trap '' TERM; cat "/proc/$$/stat" > "$PROBE_GENERATION"; exec sleep 300 ;;
    esac
    [[ $3 == "$(cat "$PROBE_AVAILABLE")" ]] && exit 0
    exit 1
fi
if [[ ${1:-} == run ]]; then
    : > "$PROBE_PAYLOAD"
    exit "${PROBE_PAYLOAD_EXIT:-0}"
fi
echo 'unexpected mutating or unknown Podman command' >&2
exit 91
"#,
            )
            .unwrap();
            std::fs::set_permissions(&podman, std::fs::Permissions::from_mode(0o755)).unwrap();
            let reference = std::fs::read_to_string(here.join("image.digest"))
                .unwrap()
                .trim()
                .to_string();
            std::fs::write(dir.path().join("available"), &reference).unwrap();
            let mut paths = vec![bin];
            paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
            let path = std::env::join_paths(paths).unwrap();
            Self {
                dir,
                path,
                reference,
            }
        }

        fn command(&self) -> Command {
            let mut command = Command::new(self.dir.path().join(WRAPPER));
            self.configure(&mut command);
            command
        }

        fn configure(&self, command: &mut Command) {
            command
                .current_dir(self.dir.path())
                .env("PATH", &self.path)
                .env("PROBE_CALLS", self.dir.path().join("calls"))
                .env("PROBE_STORES", self.dir.path().join("stores"))
                .env("PROBE_AVAILABLE", self.dir.path().join("available"))
                .env("PROBE_PAYLOAD", self.dir.path().join("payload"))
                .env("PROBE_GENERATION", self.dir.path().join("generation"));
        }

        fn check(&self) -> Output {
            observed(
                "check-image",
                self.command().arg("--check-image").output().unwrap(),
            )
        }

        fn run(&self) -> Output {
            observed(
                "late-payload",
                self.command()
                    .args(["--src", ".", "--out", "out", "--", "true"])
                    .output()
                    .unwrap(),
            )
        }

        fn calls(&self) -> String {
            std::fs::read_to_string(self.dir.path().join("calls")).unwrap_or_default()
        }
    }

    #[test]
    fn exact_image_probe_is_read_only_and_late_guard_remains() {
        let f = Fixture::new();
        let checked = f.check();
        assert!(checked.status.success(), "{checked:?}");
        assert_eq!(f.calls(), format!("image exists {}\n", f.reference));
        assert!(!f.dir.path().join("out").exists());
        assert!(!f.dir.path().join("payload").exists());
        let ran = f.run();
        assert!(ran.status.success(), "{ran:?}");
        assert!(f.dir.path().join("payload").exists());
        let failed_payload = f
            .command()
            .args(["--src", ".", "--out", "out", "--", "true"])
            .env("PROBE_PAYLOAD_EXIT", "23")
            .output()
            .unwrap();
        assert_eq!(failed_payload.status.code(), Some(23));

        // The successful early observation is no image lease. Only a different
        // digest/tag remains now; the actual late wrapper must refuse it.
        std::fs::remove_file(f.dir.path().join("payload")).unwrap();
        std::fs::write(
            f.dir.path().join("available"),
            "localhost/hermit-hermetic-validate:latest",
        )
        .unwrap();
        let late = f.run();
        assert_eq!(late.status.code(), Some(1));
        assert!(String::from_utf8_lossy(&late.stderr).contains("not present locally"));
        assert!(!f.dir.path().join("payload").exists());
        assert_eq!(
            f.calls()
                .lines()
                .filter(|line| line.starts_with("run "))
                .count(),
            2
        );
    }

    #[test]
    fn absence_and_inspection_errors_keep_actual_status_and_stderr() {
        let f = Fixture::new();
        std::fs::write(
            f.dir.path().join("available"),
            "localhost/other@sha256:".to_owned() + &"0".repeat(64),
        )
        .unwrap();
        let absent = f.check();
        assert_eq!(absent.status.code(), Some(1));
        assert!(String::from_utf8_lossy(&absent.stderr).contains("Podman status 1"));
        for code in [125, 127] {
            let error = f
                .command()
                .arg("--check-image")
                .env("PROBE_MODE", "error")
                .env("PROBE_ERROR", code.to_string())
                .output()
                .unwrap();
            let error = observed(&format!("inspection-error-{code}"), error);
            assert_eq!(error.status.code(), Some(code));
            assert!(error.stderr.starts_with(b"exact inspection error\n"));
            let text = String::from_utf8_lossy(&error.stderr);
            assert!(text.contains("inspection unavailable"));
            assert!(!text.contains("not present locally"));
        }
        assert!(!f.dir.path().join("out").exists());
        assert!(!f.dir.path().join("payload").exists());
        assert!(
            f.calls()
                .lines()
                .all(|line| line == format!("image exists {}", f.reference))
        );
    }

    #[test]
    fn missing_or_malformed_pin_never_reaches_podman() {
        let f = Fixture::new();
        let pin = f.dir.path().join("ci/hermetic/image.digest");
        for value in [
            "".to_string(),
            "localhost/hermit-hermetic-validate:latest".to_string(),
            "name@sha256:bad".to_string(),
            format!("prefix {}", f.reference),
        ] {
            std::fs::write(&pin, value).unwrap();
            let output = f.check();
            assert_eq!(output.status.code(), Some(2));
            assert!(
                String::from_utf8_lossy(&output.stderr).contains("invalid exact image reference")
            );
            assert!(f.calls().is_empty());
        }
        std::fs::remove_file(pin).unwrap();
        let output = f.check();
        assert_eq!(output.status.code(), Some(2));
        assert!(String::from_utf8_lossy(&output.stderr).contains("no --digest"));
        assert!(f.calls().is_empty());
    }

    #[test]
    fn hanging_inspection_is_bounded_and_never_runs_payload() {
        let f = Fixture::new();
        let started = std::time::Instant::now();
        let output = f
            .command()
            .arg("--check-image")
            .env("PROBE_MODE", "hang")
            .output()
            .unwrap();
        let output = observed("hanging-query", output);
        assert_eq!(output.status.code(), Some(124), "{output:?}");
        let elapsed = started.elapsed();
        eprintln!(
            "PINNED_IMAGE_HANG_ELAPSED_SECONDS {}",
            elapsed.as_secs_f64()
        );
        assert!(elapsed >= std::time::Duration::from_secs(10), "{elapsed:?}");
        assert!(elapsed < std::time::Duration::from_secs(20), "{elapsed:?}");
        assert!(String::from_utf8_lossy(&output.stderr).contains("inspection unavailable"));
        let saved = std::fs::read_to_string(f.dir.path().join("generation")).unwrap();
        eprintln!("PINNED_IMAGE_HANG_GENERATION {}", saved.trim());
        let pid = saved.split_once(' ').unwrap().0;
        let saved_start = saved
            .rsplit_once(") ")
            .unwrap()
            .1
            .split_whitespace()
            .nth(19)
            .unwrap();
        match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Ok(current) => {
                assert_ne!(
                    current
                        .rsplit_once(") ")
                        .unwrap()
                        .1
                        .split_whitespace()
                        .nth(19)
                        .unwrap(),
                    saved_start
                );
            }
            Err(error)
                if error.kind() == std::io::ErrorKind::NotFound
                    || error.raw_os_error() == Some(libc::ESRCH) => {}
            Err(error) => panic!("cannot verify stopped probe generation: {error}"),
        }
        assert!(!f.dir.path().join("payload").exists());
        assert!(!f.dir.path().join("out").exists());
    }

    #[test]
    fn term_ignoring_inspection_is_forced_to_stop_without_payload() {
        let f = Fixture::new();
        let started = std::time::Instant::now();
        // Exercise the late caller too: even a forced-stop inspection error
        // must not proceed to the payload or create its output directory.
        let output = f
            .command()
            .args(["--src", ".", "--out", "out", "--", "true"])
            .env("PROBE_MODE", "ignore-term")
            .env("LC_ALL", "C")
            .output()
            .unwrap();
        let elapsed = started.elapsed();
        let output = observed("term-ignoring-late-query", output);
        eprintln!(
            "PINNED_IMAGE_FORCED_STOP_ELAPSED_SECONDS {}",
            elapsed.as_secs_f64()
        );
        assert_eq!(output.status.code(), Some(137), "{output:?}");
        assert!(elapsed >= std::time::Duration::from_secs(12), "{elapsed:?}");
        assert!(elapsed < std::time::Duration::from_secs(20), "{elapsed:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("sending signal TERM"), "{stderr}");
        assert!(stderr.contains("sending signal KILL"), "{stderr}");
        assert!(stderr.contains("inspection unavailable"), "{stderr}");
        assert!(!f.dir.path().join("payload").exists());
        assert!(!f.dir.path().join("out").exists());
        assert_eq!(f.calls(), format!("image exists {}\n", f.reference));

        let saved = std::fs::read_to_string(f.dir.path().join("generation")).unwrap();
        eprintln!("PINNED_IMAGE_FORCED_STOP_GENERATION {}", saved.trim());
        let pid = saved.split_once(' ').unwrap().0;
        let saved_start = saved
            .rsplit_once(") ")
            .unwrap()
            .1
            .split_whitespace()
            .nth(19)
            .unwrap();
        // timeout and its direct command receive KILL together. Allow only a
        // bounded observation interval for the original command to be reaped
        // after adoption; a zombie of that generation is still not gone.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
                Ok(current) => {
                    let current_start = current
                        .rsplit_once(") ")
                        .unwrap()
                        .1
                        .split_whitespace()
                        .nth(19)
                        .unwrap();
                    if current_start != saved_start {
                        break;
                    }
                    assert!(
                        std::time::Instant::now() < deadline,
                        "original forced-stop probe generation still present: {current}"
                    );
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(error)
                    if error.kind() == std::io::ErrorKind::NotFound
                        || error.raw_os_error() == Some(libc::ESRCH) =>
                {
                    break;
                }
                Err(error) => panic!("cannot verify forced-stop probe generation: {error}"),
            }
        }
        eprintln!("PINNED_IMAGE_FORCED_STOP_ORIGINAL_GENERATION_GONE");
    }

    #[test]
    fn committed_pinned_consumer_ids_are_all_recognized() {
        let (canonical, _, _) = crate::load_committed_validation_dag(root()).unwrap();
        // This conservative census is a test of the committed generator's
        // direct-invocation contract, not a wider production trigger. A future
        // prefix or text-only mention in that DAG requires explicit review.
        let expected: BTreeSet<_> = canonical
            .steps
            .iter()
            .filter(|step| step.skip_reason.is_none() && step.cmd.contains(WRAPPER))
            .map(|step| step.tag())
            .collect();
        let probes = selected_probes(&canonical, None).unwrap();
        let recognized: BTreeSet<_> = probes
            .iter()
            .flat_map(|probe| probe.consumers.iter().cloned())
            .collect();
        assert!(!expected.is_empty());
        assert_eq!(recognized, expected);
        eprintln!(
            "PINNED_IMAGE_COMMITTED_CONSUMERS {}",
            serde_json::json!({"expected":expected,"recognized":recognized})
        );
    }

    #[test]
    fn selected_executables_and_both_lanes_own_admission() {
        let (canonical, _, _) = crate::load_committed_validation_dag(root()).unwrap();
        let pinned = canonical
            .steps
            .iter()
            .find(|s| s.tag() == "test.regular_crates")
            .unwrap()
            .clone();
        let host = canonical
            .steps
            .iter()
            .find(|s| s.tag() == "test.regular_crates_on_host")
            .unwrap()
            .clone();
        let cfg = canonical.with_steps(vec![host.clone()]);
        let mut second_step = pinned.clone();
        second_step.group = "unrelated".into();
        second_step.job = "name_without_a_pinned_suffix".into();
        second_step.env.insert(
            "CONTAINERS_STORAGE_CONF".into(),
            "/fixture/second-store".into(),
        );
        let second = canonical.with_steps(vec![second_step]);
        let plan = BoundExecutionPlan::bind(&cfg, Some(&second), None).unwrap();
        let f = Fixture::new();
        admit_with(&plan, "fixture", |probe| {
            let mut command = probe_command(f.dir.path(), probe);
            f.configure(&mut command);
            let output = command.output()?;
            Ok(output.status)
        })
        .unwrap_or_else(|s| panic!("{:?}", s.detail));
        assert_eq!(f.calls().lines().count(), 1);
        assert_eq!(
            std::fs::read_to_string(f.dir.path().join("stores")).unwrap(),
            "/fixture/second-store\n"
        );

        let mut decoy = host;
        decoy.job = "decoy_in_pinned_root".into();
        decoy.cmd = format!("printf '%s' '{WRAPPER}'");
        let mut omitted = pinned;
        omitted.skip_reason = Some(dagrun::model::IntentionalSkipReason::EmptyManifestBucket);
        let cfg = canonical.with_steps(vec![decoy, omitted]);
        let plan = BoundExecutionPlan::bind(&cfg, None, None).unwrap();
        admit_with(&plan, "host", |_| {
            panic!("nonexecuting decoy/skip must not inspect images")
        })
        .unwrap_or_else(|s| panic!("{:?}", s.detail));

        let mut bad = second;
        bad.steps[0].cmd = bad.steps[0]
            .cmd
            .replacen(" --src ", " --digest wrong --src ", 1);
        assert!(selected_probes(&bad, None).is_err());
    }

    #[test]
    fn canonical_full_and_hosted_selections_preserve_image_boundary() {
        // The real hosted entrypoint requires its existing off-record policy;
        // image admission must not turn the original invalid form into a pass.
        assert!(crate::parse_argv(&["--hosted-portable-only".into()]).is_err());
        for (argv, needs_image) in [
            (vec!["full", "--all", "--ignore-cache"], true),
            (
                vec![
                    "--hosted-portable-only",
                    crate::ALLOW_LOCAL_OFF_THE_RECORD_RUN_OPTION,
                ],
                false,
            ),
            (vec!["--only", "portable", "test.regular_crates"], true),
            (
                vec![
                    "--only",
                    "hosted-portable",
                    "test.regular_crates",
                    crate::ALLOW_LOCAL_OFF_THE_RECORD_RUN_OPTION,
                ],
                false,
            ),
        ] {
            let argv = argv.into_iter().map(str::to_owned).collect::<Vec<_>>();
            let args = crate::parse_argv(&argv).unwrap();
            let scratch = tempfile::tempdir().unwrap();
            let plan = crate::build_plan(root(), &args, scratch.path()).unwrap();
            let executable =
                BoundExecutionPlan::bind(&plan.cfg, plan.second.as_ref(), None).unwrap();
            let probes = selected_probes(&executable.cfg, executable.second.as_ref()).unwrap();
            assert_eq!(!probes.is_empty(), needs_image, "{argv:?}");
            if needs_image {
                let mut calls = 0;
                let refused = admit_with(&executable, &plan.profile, |_| {
                    calls += 1;
                    Ok(ExitStatus::from_raw(1 << 8))
                })
                .expect_err("absent image must stop this selected plan");
                assert_eq!(calls, 1);
                assert_eq!(refused.nodes_executed, 0);
                assert_eq!(refused.executed_tests, Some(0));
            }
        }
    }

    #[test]
    fn image_refusal_uses_typed_zero_execution_service_result() {
        let f = Fixture::new();
        let (cfg, _, _) = crate::load_committed_validation_dag(root()).unwrap();
        let plan = BoundExecutionPlan::bind(&cfg, None, None).unwrap();
        for code in [1, 124, 125, 127, 137] {
            let mut summary = admit_with(&plan, "full", |_| Ok(ExitStatus::from_raw(code << 8)))
                .expect_err("nonzero probe must refuse");
            assert_eq!(summary.verdict, crate::Verdict::Refused);
            assert_eq!(summary.exit_code, COULD_NOT_RUN_EXIT_CODE);
            assert_eq!(summary.nodes_executed, 0);
            assert_eq!(summary.executed_tests, Some(0));
            assert_eq!(summary.passed_tests, Some(0));
            assert!(summary.ledger.is_none());
            summary.commit = "0123456789abcdef0123456789abcdef01234567".into();
            let path = f.dir.path().join(format!("refused-{code}.json"));
            crate::write_validation_service_result(&path, &summary).unwrap();
            let read =
                crate::ValidationServiceResult::from_json_slice(&std::fs::read(path).unwrap())
                    .unwrap();
            assert_eq!(
                read.final_validate_status,
                crate::FinalValidateStatus::CouldNotRun
            );
            assert_eq!(read.executed_nodes, 0);
            assert_eq!(read.executed_tests, Some(0));
            assert_eq!(read.passed_tests, Some(0));
            assert!(
                read.detail
                    .unwrap()
                    .iter()
                    .any(|s| s.contains(&format!("exit status: {code}")))
            );
        }
        std::fs::remove_file(f.dir.path().join(WRAPPER)).unwrap();
        let unavailable = admit(f.dir.path(), &plan, "full")
            .expect_err("an unstartable probe must refuse before graph execution");
        assert_eq!(unavailable.nodes_executed, 0);
        assert_eq!(unavailable.executed_tests, Some(0));
        assert!(
            unavailable
                .detail
                .iter()
                .any(|line| line.contains("Cannot start the read-only image probe"))
        );
        assert_eq!(crate::completed_verdict(1), crate::Verdict::Fail);
    }
}
