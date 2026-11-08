#!/usr/bin/env -S rust-script --force
//! Prepare or consume source-bound Nextest executable metadata.
//! ```cargo
//! [dependencies]
//! hermit-manifest-plan = { path = "manifest-plan" }
//! serde_json = "1"
//! tempfile = "3"
//! ```

use std::path::PathBuf;
use std::process::ExitCode;

#[path = "../scripts/lib/rust_script_prelude.rs"]
mod rust_script_prelude;

fn run() -> Result<i32, String> {
    let root = std::env::current_dir().map_err(|e| e.to_string())?;
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("--print-executable") => {
            if args.next().is_some() { return Err("unexpected executable query argument".into()); }
            println!("{}", std::env::current_exe().map_err(|e| e.to_string())?.display());
            Ok(0)
        }
        Some("prepare") => {
            let profile = args.next().ok_or("prepare requires a committed graph profile")?;
            if profile == "--help" || profile == "-h" {
                println!("{PREPARE_USAGE}");
                return Ok(0);
            }
            let mut build_mode = "cargo";
            let mut cohort = hermit_manifest_plan::nextest_binaries::DEFAULT_COHORT;
            let mut rest = args.peekable();
            while let Some(arg) = rest.peek() {
                match arg.as_str() {
                    "--build-mode" => {
                        rest.next();
                        match rest.next().as_deref() {
                            Some("cargo") => build_mode = "cargo",
                            Some("buck") => build_mode = "buck",
                            _ => {
                                return Err("prepare --build-mode needs cargo or buck".into());
                            }
                        }
                    }
                    "--cohort" => {
                        rest.next();
                        let slug = rest.next().ok_or("prepare --cohort needs a cohort slug")?;
                        cohort =
                            hermit_manifest_plan::nextest_binaries::PreparedCohort::from_slug(
                                Some(&slug),
                            )?;
                    }
                    _ => break,
                }
            }
            if rest.next().is_some() { return Err("unexpected prepare argument".into()); }
            match hermit_manifest_plan::nextest_binaries::prepare(&root, &profile, build_mode, cohort) {
                Ok(()) => Ok(0),
                Err(error) => {
                    eprintln!("prepared-nextest: {error}");
                    Ok(i32::from(error.status))
                }
            }
        }
        Some("assert") => {
            let remaining = args.collect::<Vec<_>>();
            if remaining.len() == 1 && (remaining[0] == "--help" || remaining[0] == "-h") {
                println!("{ASSERT_USAGE}");
                return Ok(0);
            }
            let mut rest = remaining.iter();
            let Some(profile) = rest.next().filter(|value| !value.starts_with('-')) else {
                return Err("invalid assert arguments; use assert --help".into());
            };
            let mut producer = None;
            let mut cohort = hermit_manifest_plan::nextest_binaries::DEFAULT_COHORT;
            let mut cohort_seen = false;
            while let Some(flag) = rest.next() {
                match flag.as_str() {
                    "--prepared-by" if producer.is_none() => {
                        producer = Some(
                            rest.next()
                                .ok_or("invalid assert arguments; use assert --help")?
                                .as_str(),
                        );
                    }
                    "--cohort" if !cohort_seen => {
                        cohort_seen = true;
                        let slug = rest
                            .next()
                            .ok_or("invalid assert arguments; use assert --help")?;
                        cohort =
                            hermit_manifest_plan::nextest_binaries::PreparedCohort::from_slug(
                                Some(slug),
                            )?;
                    }
                    _ => return Err("invalid assert arguments; use assert --help".into()),
                }
            }
            hermit_manifest_plan::nextest_binaries::assert_profile(&root, profile, producer, cohort)?;
            Ok(0)
        }
        Some("executable") => {
            let package = args.next().ok_or("executable requires a Cargo package")?;
            let name = args.next().ok_or("executable requires a test target")?;
            if args.next().is_some() { return Err("unexpected executable argument".into()); }
            println!("{}", hermit_manifest_plan::nextest_binaries::executable(&root, &package, &name)?.display());
            Ok(0)
        }
        Some("budget-context") => {
            let mut remaining = args.collect::<Vec<_>>();
            if remaining == ["--help"] || remaining == ["-h"] {
                println!("usage: ci/nextest-binaries.rs budget-context [--launch-proof PATH] NEXTEST_ARGS...\n\nRead verified preparation and print regular calibration context. An optional retained launch proof binds actual execution domain and resources; absent/unavailable proof keeps unchanged defaults. Malformed proof or stale preparation refuses. Other selections print null. This command never builds artifacts.");
                return Ok(0);
            }
            let launch = take_path(&mut remaining, "--launch-proof")?;
            println!("{}", hermit_manifest_plan::nextest_binaries::budget_context(&root, &remaining, launch.as_deref())?);
            Ok(0)
        }
        Some("capture-launch") => {
            let remaining = args.collect::<Vec<_>>();
            if remaining == ["--help"] || remaining == ["-h"] {
                println!("usage: ci/nextest-binaries.rs capture-launch (--host | --pinned-image REFERENCE --image-id CONFIG_ID) --output NEW_ABSOLUTE_PATH\n\nAt the maintained native launch boundary, observe actual host cgroup ancestors and CPU affinity into a new exclusive read-only proof file. Pinned mode records the image inspected by the wrapper and actually passed to Podman. Unavailable observation is explicitly unqualified; malformed inputs refuse. --probe prints the supported observation version without reading source or resources. Never builds, changes limits, or runs a workload. Use budget-context --launch-proof to verify the actual execution boundary.");
                return Ok(0);
            }
            if remaining == ["--probe"] {
                println!("nextest-launch-observation-v2");
                return Ok(0);
            }
            let (domain, output) = match remaining.as_slice() {
                [host, output_flag, output] if host == "--host" && output_flag == "--output" =>
                    (hermit_manifest_plan::nextest_cpu::ExecutionDomain::Host, output),
                [image_flag, image, id_flag, image_id, output_flag, output]
                    if image_flag == "--pinned-image" && id_flag == "--image-id" && output_flag == "--output" =>
                    (hermit_manifest_plan::nextest_cpu::ExecutionDomain::PinnedRoot { image: image.clone(), image_id: image_id.clone() }, output),
                _ => return Err("invalid launch capture arguments; use capture-launch --help for the maintained launch forms".into()),
            };
            hermit_manifest_plan::nextest_cohort::capture(domain, &PathBuf::from(output))?;
            Ok(0)
        }
        Some(operation @ ("cpu-wrapper" | "build-cpu-wrapper")) => {
            if args.next().is_some() { return Err("unexpected CPU wrapper query argument".into()); }
            let path = if operation == "cpu-wrapper" {
                hermit_manifest_plan::nextest_binaries::cpu_wrapper(&root)?
            } else {
                match hermit_manifest_plan::nextest_binaries::build_cpu_wrapper(&root) {
                    Ok(path) => path,
                    Err(error) => {
                        eprintln!("prepared-nextest: {error}");
                        return Ok(i32::from(error.status));
                    }
                }
            };
            println!("{}", path.display());
            Ok(0)
        }
        Some(operation @ ("run" | "list")) => {
            let mut remaining = args.collect::<Vec<_>>();
            if remaining == ["--help"] || remaining == ["-h"] {
                println!("usage: ci/nextest-binaries.rs {operation} [--launch-proof PATH] [--budget-map PATH SHA256] [--config-file PATH] NEXTEST_ARGS...\n\nRun/list verified prepared artifacts without compilation. A run may bind the exact resolved map and its verified launch context; changed or malformed evidence refuses. Missing cohort proof stays unqualified. --budget-map is only valid for run.");
                return Ok(0);
            }
            let launch = take_path(&mut remaining, "--launch-proof")?;
            let budget = take_path(&mut remaining, "--budget-map")?;
            let digest = if budget.is_some() {
                if remaining.is_empty() { return Err("--budget-map requires its retained SHA256".into()); }
                Some(remaining.remove(0))
            } else { None };
            let config = if remaining.first().map(String::as_str) == Some("--config-file") {
                if remaining.len() < 2 { return Err("--config-file requires a path".into()); }
                let path = PathBuf::from(remaining.remove(1));
                remaining.remove(0);
                Some(path)
            } else { None };
            hermit_manifest_plan::nextest_binaries::run(&root, operation, config.as_deref(), &remaining, launch.as_deref(), budget.as_deref().zip(digest.as_deref()))
        }
        Some("--help" | "-h") => {
            println!("usage: ci/nextest-binaries.rs prepare PROFILE [--build-mode cargo|buck] [--cohort SLUG] | assert PROFILE [--prepared-by PRODUCER] [--cohort SLUG] | run|list [--launch-proof PATH] [--budget-map PATH SHA256] [--config-file PATH] NEXTEST_ARGS... | budget-context [--launch-proof PATH] NEXTEST_ARGS... | capture-launch --help\n\nPrepared run optionally rechecks the retained map and launch context under the artifact lock. Missing cohort evidence retains defaults; malformed evidence refuses. No consumer compiles artifacts. prepare --help and assert --help describe the prepared record and its Cargo-profile check.");
            Ok(0)
        }
        _ => Err("usage: ci/nextest-binaries.rs prepare PROFILE [--build-mode cargo|buck] [--cohort SLUG] | assert PROFILE [--prepared-by PRODUCER] [--cohort SLUG] | run|list [--config-file PATH] NEXTEST_ARGS... | budget-context NEXTEST_ARGS... | capture-launch --help".into()),
    }
}

const PREPARE_USAGE: &str = "usage: ci/nextest-binaries.rs prepare PROFILE [--build-mode cargo|buck] [--cohort SLUG]

Build PROFILE's committed Nextest selections in one prepared cohort and
publish one record of them, replacing that cohort's previous record. The
default cohort publishes target/ci/nextest-binaries/current.json; another
cohort (--cohort SLUG) publishes cohorts/SLUG.json in the same store, under
its own lock, from its own Cargo target directory. The full and
hosted-portable profiles build in Cargo profile validate; every other
profile, and a second Cargo project, builds in Cargo's default test profile,
recorded as dev. The record names PROFILE (prepared_for), that Cargo profile
(cargo_profile), the build mode (build_mode), and the selections' canonical
feature set (feature_set), which assert checks.";

const ASSERT_USAGE: &str = "usage: ci/nextest-binaries.rs assert PROFILE [--prepared-by PRODUCER] [--cohort SLUG]

Verify, without compiling, that the prepared record for the cohort (the
default cohort's target/ci/nextest-binaries/current.json, or cohorts/SLUG.json
with --cohort) holds every committed selection of PROFILE unchanged and was
built in the Cargo profile that `prepare PRODUCER` builds in (PRODUCER
defaults to PROFILE), in this run's build mode and the cohort's feature set.
Use --prepared-by when a lane checks another profile's selections in its own
producer's record, for example the full lane's `assert privileged
--prepared-by full`, which needs validate-profile executables although
`prepare privileged` builds dev ones.

Exit status: 0 when the record matches; 2 when it does not, naming the expected
and recorded Cargo profiles and the producer node to rerun. A record written
before record schema 7 names no build mode or feature set (before schema 6,
no Cargo profile) and is refused, not guessed.";

fn take_path(args: &mut Vec<String>, flag: &str) -> Result<Option<PathBuf>, String> {
    if args.first().map(String::as_str) != Some(flag) {
        return Ok(None);
    }
    if args.len() < 2 || args[1].starts_with('-') {
        return Err(format!("{flag} requires a path; use --help"));
    }
    args.remove(0);
    Ok(Some(PathBuf::from(args.remove(0))))
}

fn main() -> ExitCode {
    rust_script_prelude::init();
    match run() {
        Ok(status) => ExitCode::from(u8::try_from(status).unwrap_or(1)),
        Err(error) => {
            eprintln!("prepared-nextest: {error}");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use hermit_manifest_plan::nextest_binaries::check_prepared_cargo_profile;
    use hermit_manifest_plan::nextest_cohort::capture;
    use hermit_manifest_plan::nextest_cohort::cpu_set;
    use hermit_manifest_plan::nextest_cohort::verify;
    use hermit_manifest_plan::nextest_cpu::ExecutionDomain;
    use serde_json::json;

    fn producers() -> Vec<String> {
        vec!["build.workspace_in_pinned_root".into()]
    }

    // The 2026-11-03 sequence: the focused privileged lane's producer ran
    // `prepare privileged` (dev profile) last, and the full lane's barrier then
    // accepted its record because every privileged selection was present.
    #[test]
    fn a_dev_record_is_refused_where_the_full_lane_expects_validate() {
        let dev = json!({"schema": 7, "prepared_for": "privileged", "cargo_profile": "dev"});
        let error = check_prepared_cargo_profile(&dev, "privileged", "full", false, &producers())
            .unwrap_err();
        for expected in [
            "assert privileged --prepared-by full refused",
            "built in Cargo profile dev by `prepare privileged`",
            "the full lane expects Cargo profile validate",
            "Rerun the full lane's producer (build.workspace_in_pinned_root)",
            "`./ci/nextest-binaries.rs prepare full`",
        ] {
            assert!(error.contains(expected), "missing {expected:?} in: {error}");
        }
        for (selections, producer) in [("full", "full"), ("hosted-portable", "hosted-portable")] {
            let error =
                check_prepared_cargo_profile(&dev, selections, producer, false, &[]).unwrap_err();
            assert!(
                error.contains(&format!("assert {selections} refused")),
                "{error}"
            );
            assert!(error.contains("expects Cargo profile validate"), "{error}");
            assert!(
                error.contains(&format!(
                    "Rerun `./ci/nextest-binaries.rs prepare {producer}`"
                )),
                "{error}"
            );
        }
    }

    #[test]
    fn a_validate_record_passes_where_its_producer_builds_validate() {
        let validate = json!({"schema": 7, "prepared_for": "full", "cargo_profile": "validate"});
        for (selections, producer) in [
            ("privileged", "full"),
            ("full", "full"),
            ("hosted-portable", "hosted-portable"),
        ] {
            check_prepared_cargo_profile(&validate, selections, producer, false, &producers())
                .unwrap();
        }
        // The check is symmetric: a dev-profile consumer refuses validate
        // executables, and a second Cargo project always lists in dev.
        let error = check_prepared_cargo_profile(&validate, "privileged", "privileged", false, &[])
            .unwrap_err();
        assert!(
            error.contains("built in Cargo profile validate by `prepare full`"),
            "{error}"
        );
        assert!(
            error.contains("the privileged lane expects Cargo profile dev"),
            "{error}"
        );
        assert!(check_prepared_cargo_profile(&validate, "full", "full", true, &[]).is_err());
        let dev = json!({"schema": 7, "prepared_for": "privileged", "cargo_profile": "dev"});
        check_prepared_cargo_profile(&dev, "privileged", "privileged", false, &[]).unwrap();
        check_prepared_cargo_profile(&dev, "full", "full", true, &[]).unwrap();
    }

    // A record from before schema 7 names no build mode or feature set
    // (before schema 6, no profile either). It is refused, not guessed
    // from executable paths, with the same remedy.
    #[test]
    fn a_record_without_a_cargo_profile_is_refused_with_its_remedy() {
        for (record, found) in [
            (
                json!({"schema": 5}),
                "the prepared record is schema 5, not schema 7",
            ),
            (
                json!({"schema": 6, "prepared_for": "full", "cargo_profile": "validate"}),
                "the prepared record is schema 6, not schema 7",
            ),
            (json!({}), "the prepared record has no schema, not schema 7"),
        ] {
            for producer in ["full", "privileged"] {
                let error = check_prepared_cargo_profile(
                    &record,
                    "privileged",
                    producer,
                    false,
                    &producers(),
                )
                .unwrap_err();
                assert!(error.contains(found), "{error}");
                assert!(error.contains("does not name the Cargo profile"), "{error}");
                assert!(
                    error.contains("(build.workspace_in_pinned_root)"),
                    "{error}"
                );
            }
        }
        for record in [
            json!({"schema": 7, "prepared_for": "full"}),
            json!({"schema": 7, "prepared_for": "full", "cargo_profile": ""}),
            json!({"schema": 7, "cargo_profile": "validate"}),
        ] {
            let error = check_prepared_cargo_profile(&record, "full", "full", false, &producers())
                .unwrap_err();
            assert!(error.contains("the prepared record has no "), "{error}");
            assert!(error.contains("Rerun the full lane's producer"), "{error}");
        }
    }

    #[test]
    fn launch_capture_is_exclusive_and_each_invocation_has_its_own_proof() {
        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("first.json");
        let second = temp.path().join("second.json");
        capture(ExecutionDomain::Host, &first).unwrap();
        capture(ExecutionDomain::Host, &second).unwrap();
        let a = verify(&first).unwrap();
        let b = verify(&second).unwrap();
        assert_eq!(a.0, b.0);
        assert_ne!(a.1.as_ref().unwrap().sha256, b.1.as_ref().unwrap().sha256);
        let before = fs::read(&first).unwrap();
        assert!(capture(ExecutionDomain::Host, &first).is_err());
        assert_eq!(fs::read(&first).unwrap(), before);
        let link = temp.path().join("link.json");
        std::os::unix::fs::symlink(&first, &link).unwrap();
        assert!(capture(ExecutionDomain::Host, &link).is_err());
        assert!(verify(&link).is_err());
        assert!(
            capture(
                ExecutionDomain::PinnedRoot {
                    image: "unbound:tag".into(),
                    image_id: "".into()
                },
                &temp.path().join("invalid.json")
            )
            .is_err()
        );
        eprintln!("actual native observation complete: {}", a.0.is_some());
    }

    #[test]
    fn absent_and_explicit_unavailable_proofs_do_not_authorize_a_cohort() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("launch.json");
        assert_eq!(verify(&path).unwrap(), (None, None));
        let proof = json!({"schema":1, "invocation":"/fixture/unique", "observation":{"state":"unavailable", "reason":"host observer unavailable"}});
        fs::write(&path, serde_json::to_vec(&proof).unwrap()).unwrap();
        let observed = verify(&path).unwrap();
        assert!(observed.0.is_none());
        assert!(observed.1.is_some());
        for value in [
            json!({}),
            json!({"schema":3,"invocation":"/fixture/unique","observation":{"state":"unavailable","reason":"unsupported"}}),
            json!({"schema":1,"invocation":"/fixture/unique","observation":{"state":"unavailable","reason":""}}),
            json!({"schema":1,"invocation":"/fixture/unique","observation":{"state":"unavailable","reason":"missing","fabricated":true}}),
        ] {
            fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
            assert!(verify(&path).is_err(), "accepted {value}");
        }
        for bytes in [vec![], b"{\"schema\":1".to_vec(), vec![b' '; 262_145]] {
            fs::write(&path, bytes).unwrap();
            assert!(verify(&path).is_err());
        }
    }

    #[test]
    fn pinned_evidence_copy_cannot_claim_hidden_host_ancestors() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("forged.json");
        let proof = json!({"schema":2,"invocation":"/fixture/unique","observation":{
            "state":"complete","domain":{"kind":"pinned-root","image":format!("fixture@sha256:{}","a".repeat(64)),"image_id":"b".repeat(64)},
            "host":{"membership":"/actual-leaf","namespace":"cgroup:[123]","affinity":[0],"ancestors":[{"anchor":{"device":27,"inode":123},"controls":{"cpu":{"quota_usec":100000,"period_usec":100000},"cpuset":{"state":"observed","cpus":[0]},"cpuset_enabled_for_children":true,"memory":{"max":1024},"swap":{"max":0}}},{"anchor":{"device":27,"inode":1},"controls":{"cpu":null,"cpuset":{"state":"observed","cpus":[0]},"cpuset_enabled_for_children":true,"memory":"unlimited","swap":"unlimited"}}]}
        }});
        fs::write(&path, serde_json::to_vec(&proof).unwrap()).unwrap();
        assert!(verify(&path).unwrap_err().contains("fixed read-only mount"));
        let mut old_complete = proof.clone();
        old_complete["schema"] = json!(1);
        fs::write(&path, serde_json::to_vec(&old_complete).unwrap()).unwrap();
        assert!(
            verify(&path)
                .unwrap_err()
                .contains("current controller-enable evidence")
        );
        for duplicate in [false, true] {
            let mut malformed = proof.clone();
            let layers = malformed["observation"]["host"]["ancestors"]
                .as_array_mut()
                .unwrap();
            if duplicate {
                layers.push(layers.last().unwrap().clone());
            } else {
                layers.pop();
            }
            fs::write(&path, serde_json::to_vec(&malformed).unwrap()).unwrap();
            assert!(
                verify(&path)
                    .unwrap_err()
                    .contains("malformed complete launch")
            );
        }
        let mut missing = proof.clone();
        missing["observation"]["host"]["ancestors"][0]["controls"]
            .as_object_mut()
            .unwrap()
            .remove("cpu");
        fs::write(&path, serde_json::to_vec(&missing).unwrap()).unwrap();
        assert!(
            verify(&path)
                .unwrap_err()
                .contains("malformed launch proof")
        );
        for value in ["", "1-0", "0,0", "0-1048577", "max"] {
            assert!(cpu_set(value).is_err(), "accepted {value}");
        }
        assert_eq!(cpu_set("0-2,4,6-7").unwrap(), vec![0, 1, 2, 4, 6, 7]);
    }
}
