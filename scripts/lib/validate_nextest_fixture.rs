//! Authentic producer/runner/ledger coverage. The fixture's evidence (bounds,
//! per-case logs, results and ledger rows) remains in the script harness's
//! retained directory, outside product inventory. Its Cargo build does not:
//! see [`FixtureTree`].

use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::time::Instant;

use super::super::DagConfig;
use super::super::LedgerCtx;
use super::super::Plan;
use super::super::dag_to_json;
use super::super::env_u64;
use super::super::finish_committed_selection;
use super::super::ledger_gate_attempts;
use super::super::libtest_counts;
use super::super::monotonic_now_ns;
use super::super::nextest_test_observations;
use super::super::run_lane_once;
use super::super::step_with_caps;
use super::super::test_id_summary;
use super::super::utc_now;
use super::super::validate_evidence;
use super::super::validate_test_results;
use super::super::write_ledger;
use super::*;

/// Reads the checkout this file belongs to, which is named by directory. An
/// inherited repository-location variable would redirect the read.
fn git_text(source: &Path, args: &[&str]) -> String {
    let output = crate::scratch_git()
        .args(args)
        .current_dir(source)
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?}: {output:?}");
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn json(path: &Path) -> serde_json::Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

fn quoted(path: &Path) -> String {
    super::super::validate_plan::shell_quote(path.to_str().unwrap())
}

const FIXTURE_WALL_SECONDS: u64 = 1200;

/// Stable name of every fixture tree inside the harness log directory.
const FIXTURE_ROOT_PREFIX: &str = "real-nextest-results-";

/// Stable name of every fixture's Cargo build directory under `$TMPDIR`.
const FIXTURE_BUILD_PREFIX: &str = "real-nextest-build-";

/// One run's fixture tree, `<DAGRUN_LOG_DIR>/real-nextest-results-XXXXXX`.
///
/// The harness retains its log directory on purpose, so the small evidence
/// files stay. The generated crate and its Cargo target directory are only
/// inputs to the five cases, and the target directory is a complete Cargo
/// build of about 1.7 GiB. Kept in the retained directory, it leaked: one host
/// held 70 of them using 43.5e9 bytes
/// (https://github.com/rrnewton/hermit/issues/3622), and after the first fix
/// every failing run still kept one
/// (https://github.com/rrnewton/hermit/issues/3867).
///
/// So the target directory is not in this tree at all. It lives under
/// `$TMPDIR` (`real-nextest-build-XXXXXX/target`) and is removed whether the
/// cases pass or fail. A local validation run's `$TMPDIR` is its per-run
/// runtime root, which validation's cleanup removes after the run's checkout
/// and Cargo home (or its sweep does later), so even a killed test leaves no
/// build in retained evidence. Elsewhere `$TMPDIR` is usually `/tmp` (a tmpfs
/// on hosted runners), where a killed test's build is left to that
/// filesystem's own cleanup.
///
/// [`FixtureTree::finish`] also removes the generated crate when every case
/// has passed. A failing case unwinds past `finish`; the crate and the
/// evidence are then kept for diagnosis and the path printed.
struct FixtureTree {
    root: PathBuf,
    build: Option<tempfile::TempDir>,
    finished: bool,
}

impl FixtureTree {
    fn create(parent: &Path) -> Self {
        Self::create_with_build_parent(parent, &std::env::temp_dir())
    }

    fn create_with_build_parent(parent: &Path, build_parent: &Path) -> Self {
        let root = tempfile::Builder::new()
            .prefix(FIXTURE_ROOT_PREFIX)
            .tempdir_in(parent)
            .unwrap()
            .keep();
        let build = tempfile::Builder::new()
            .prefix(FIXTURE_BUILD_PREFIX)
            .tempdir_in(build_parent)
            .unwrap();
        Self {
            root,
            build: Some(build),
            finished: false,
        }
    }

    fn root(&self) -> &Path {
        &self.root
    }

    fn crate_dir(&self) -> PathBuf {
        self.root.join("crate")
    }

    fn target_dir(&self) -> PathBuf {
        self.build
            .as_ref()
            .expect("the build directory lives until the tree is dropped")
            .path()
            .join("target")
    }

    /// Called after the last assertion. A removal failure fails the test: a
    /// silent one is exactly the leak this type exists to prevent.
    fn finish(mut self) {
        self.finished = true;
        let build = self.build.take().unwrap();
        let build_path = build.path().to_path_buf();
        if let Err(error) = build.close() {
            panic!(
                "cannot remove fixture build {}: {error}",
                build_path.display()
            );
        }
        let crate_dir = self.crate_dir();
        match fs::remove_dir_all(&crate_dir) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => panic!("cannot remove fixture {}: {error}", crate_dir.display()),
        }
        for directory in [&build_path, &crate_dir] {
            assert!(
                !directory.exists(),
                "{} survived removal",
                directory.display()
            );
        }
    }
}

impl Drop for FixtureTree {
    fn drop(&mut self) {
        if let Some(build) = self.build.take() {
            let build_path = build.path().to_path_buf();
            if let Err(error) = build.close() {
                eprintln!(
                    "cannot remove the fixture's Cargo build {}: {error}",
                    build_path.display()
                );
            }
        }
        if !self.finished {
            eprintln!(
                "authentic Nextest fixture failed; its crate and evidence are kept for \
                 diagnosis: {root}. Its Cargo build was removed; rebuild it with \
                 `cargo build --manifest-path {root}/crate/Cargo.toml --target-dir <dir>`.",
                root = self.root.display()
            );
        }
    }
}

#[test]
fn fixture_tree_never_keeps_its_build_and_keeps_its_crate_only_on_failure() {
    /// Fills the tree as the fixture does and returns its root and target.
    fn populate(tree: &FixtureTree) -> (PathBuf, PathBuf) {
        let root = tree.root().to_path_buf();
        let target = tree.target_dir();
        fs::create_dir_all(target.join("debug/deps")).unwrap();
        fs::write(target.join("debug/deps/fixture-binary"), b"x").unwrap();
        fs::create_dir_all(tree.crate_dir().join("src")).unwrap();
        fs::write(tree.crate_dir().join("Cargo.toml"), b"[package]\n").unwrap();
        fs::write(root.join("bounds.json"), b"{}").unwrap();
        fs::create_dir_all(root.join("writable-pass")).unwrap();
        fs::write(root.join("writable-pass/producer.stderr"), b"log").unwrap();
        (root, target)
    }
    /// No directory below `root` holds the build, at any depth.
    fn holds_no_build(root: &Path) -> bool {
        walkdir_names(root)
            .iter()
            .all(|name| name != "target" && name != "fixture-binary")
    }
    fn walkdir_names(root: &Path) -> Vec<String> {
        let mut names = Vec::new();
        for entry in fs::read_dir(root).unwrap() {
            let entry = entry.unwrap();
            names.push(entry.file_name().into_string().unwrap());
            if entry.file_type().unwrap().is_dir() {
                names.extend(walkdir_names(&entry.path()));
            }
        }
        names
    }
    let parent = tempfile::tempdir().unwrap();
    let build_parent = tempfile::tempdir().unwrap();

    let passed = FixtureTree::create_with_build_parent(parent.path(), build_parent.path());
    let (root, target) = populate(&passed);
    assert_eq!(root.parent(), Some(parent.path()));
    let name = root.file_name().unwrap().to_str().unwrap();
    assert!(name.starts_with(FIXTURE_ROOT_PREFIX), "{name}");
    assert!(
        target.starts_with(build_parent.path()) && !target.starts_with(parent.path()),
        "the Cargo build {} must live outside the retained directory",
        target.display()
    );
    assert!(holds_no_build(&root), "the retained tree holds a build");
    passed.finish();
    assert!(
        !target.exists(),
        "a passing run must not keep its Cargo build"
    );
    assert!(!root.join("crate").exists());
    assert!(root.join("bounds.json").is_file(), "evidence is retained");
    assert!(root.join("writable-pass/producer.stderr").is_file());

    let failed = FixtureTree::create_with_build_parent(parent.path(), build_parent.path());
    let (root, target) = populate(&failed);
    let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        let _failed = failed;
        panic!("simulated fixture assertion failure");
    }));
    assert!(unwound.is_err());
    assert!(
        !target.exists(),
        "a failing run must not keep its Cargo build either"
    );
    assert!(holds_no_build(&root), "the retained tree holds a build");
    assert!(
        root.join("crate/Cargo.toml").is_file(),
        "the crate is kept for diagnosis"
    );
    assert!(root.join("bounds.json").is_file());
    assert_eq!(
        fs::read_dir(build_parent.path()).unwrap().count(),
        0,
        "a fixture build directory was left under the build parent"
    );
}

#[test]
fn the_fixture_builds_under_tmpdir_which_validation_deletes() {
    // Stands in for DAGRUN_LOG_DIR. It is itself under $TMPDIR here, so the
    // check that matters is that the build is not under it: neither beside
    // the evidence nor inside the retained root.
    let parent = tempfile::tempdir().unwrap();
    let tree = FixtureTree::create(parent.path());
    let target = tree.target_dir();
    assert!(
        !target.starts_with(parent.path()),
        "{} is under the retained log directory {}",
        target.display(),
        parent.path().display()
    );
    assert_eq!(
        target.parent().unwrap().parent(),
        Some(std::env::temp_dir().as_path()),
        "{} is not directly in $TMPDIR",
        target.display()
    );
    let build = target
        .parent()
        .unwrap()
        .file_name()
        .unwrap()
        .to_str()
        .unwrap();
    assert!(build.starts_with(FIXTURE_BUILD_PREFIX), "{build}");
    tree.finish();
}

/// This fixture owns a local clock, not a replacement scheduler epoch. A real
/// enclosing absolute deadline can only shorten it. An epoch alone does not
/// tell us the parent's allowance; the parent still enforces its own wall cap.
fn fixture_deadline(
    now_ns: u64,
    claimed_nested: bool,
    step_started_ns: Option<u64>,
    inherited_deadline_ns: Option<u64>,
) -> Result<u64, String> {
    if claimed_nested && step_started_ns.is_none() {
        return Err("nested fixture lacks its scheduler-owned start epoch".into());
    }
    if step_started_ns.is_some_and(|start| start > now_ns) {
        return Err("fixture inherited a scheduler epoch in the future".into());
    }
    let local = now_ns
        .checked_add(FIXTURE_WALL_SECONDS * 1_000_000_000)
        .ok_or("fixture deadline overflows the monotonic clock")?;
    Ok(inherited_deadline_ns.map_or(local, |inherited| local.min(inherited)))
}

#[test]
fn fixture_clocks_preserve_standalone_and_inherited_bounds() {
    let now = 2_000_000_000_000;
    let local = now + FIXTURE_WALL_SECONDS * 1_000_000_000;
    assert_eq!(fixture_deadline(now, false, None, None).unwrap(), local);
    assert_eq!(fixture_deadline(now, true, Some(1), None).unwrap(), local);
    for inherited in [now - 1, now, now + 1, local - 1, local, local + 1] {
        assert_eq!(
            fixture_deadline(now, false, None, Some(inherited)).unwrap(),
            local.min(inherited),
        );
        assert_eq!(
            fixture_deadline(now, true, Some(now - 1), Some(inherited)).unwrap(),
            local.min(inherited),
        );
    }
    assert!(fixture_deadline(now, true, None, Some(local)).is_err());
    assert!(fixture_deadline(now, false, Some(now + 1), None).is_err());
    assert!(fixture_deadline(u64::MAX, false, None, None).is_err());
}

/// Direct script-test entrypoints can start without a prepared manifest. Pay
/// that cost through the unchanged producer, under its existing declaration.
/// Official consumers only check their prerequisite; they never compile here.
fn ensure_prepared_helpers(source: &Path, root: &Path, deadline: u64) {
    let bootstrap = root.join("prepare");
    fs::create_dir_all(bootstrap.join("tmp")).unwrap();
    let prebuilt_required =
        std::env::var("HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED").as_deref() == Ok("1");
    let producer_command = if prebuilt_required {
        "./ci/prepare-rust-scripts.sh --check"
    } else {
        "./ci/prepare-rust-scripts.sh && ./ci/prepare-rust-scripts.sh --check"
    };
    // This is a private fixture step, not a mutation of the product's producer
    // declaration. Preserve its wall900/CPU7200/6GiB limits and writer flock.
    let mut step = super::super::rust_script_producer_step();
    step.cmd = format!(
        "cd {} || exit $?; export TMPDIR={}; set +e; {{ {producer_command}; }} >{} 2>{}; \
         prepare_status=$?; cat {}; cat {} >&2; exit \"$prepare_status\"",
        quoted(source),
        quoted(&bootstrap.join("tmp")),
        quoted(&bootstrap.join("producer.stdout")),
        quoted(&bootstrap.join("producer.stderr")),
        quoted(&bootstrap.join("producer.stdout")),
        quoted(&bootstrap.join("producer.stderr")),
    );
    fs::write(
        bootstrap.join("mode.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "prebuilt_required":prebuilt_required,"command":producer_command,
            "deadline_monotonic_ns":deadline,"cargo_home_policy":"inherited/default",
            "product_producer_changed":false,"per_step_cgroup_binding":null,
        }))
        .unwrap(),
    )
    .unwrap();
    run_preparation_step(&bootstrap, step, deadline);
}

/// Wall-clock cap of [`prepare_cpu_wrapper`]'s one Cargo build, in seconds.
const CPU_WRAPPER_WALL_SECONDS: i64 = 600;
/// CPU cap of the same build.
const CPU_WRAPPER_CPU_SECONDS: i64 = 900;
/// Cargo width of the same build. The cases still run at one job; this build
/// only prepares the wrapper they check for freshness. Cold, under an 8-core
/// quota, at a25b3fcd3805 on 2026-10-06: 136 s wall at one job, 48 s at four
/// and 33 s at eight, for 134 to 146 CPU-seconds and a 2.37 to 2.62 GB cgroup
/// memory peak (page cache included) in all three. At one job it was about
/// 128 of the 185 seconds this fixture took inside the full script-test run.
const CPU_WRAPPER_CARGO_JOBS: u32 = 8;

/// Build the standalone Nextest CPU wrapper once, before the timed cases, with
/// the cases' own environment and private Cargo target directory.
///
/// Every case still runs `ci/run-nextest-counted.sh`, and its standalone path
/// still invokes this same `build-cpu-wrapper` operation inside the case's
/// 180-second cap. After this step that invocation is a Cargo freshness check
/// of an up-to-date target. Without it, the first case compiled the
/// `hermit-manifest-plan` package and its roughly 90 dependency crates from
/// scratch at one job inside that cap. A GitHub-hosted runner was still
/// compiling dependencies when the cap expired, in
/// https://github.com/rrnewton/hermit/actions/runs/37134916002 and
/// https://github.com/rrnewton/hermit/actions/runs/37126714210. That build is
/// not the behaviour the cases measure, so it gets its own declared caps here,
/// still inside the fixture deadline, instead of a longer case cap.
fn prepare_cpu_wrapper(source: &Path, root: &Path, target: &Path, deadline: u64) {
    use std::os::unix::fs::PermissionsExt;

    let prepare = root.join("cpu-wrapper");
    fs::create_dir_all(prepare.join("tmp")).unwrap();
    let command = format!(
        "cd {} || exit $?; export PATH={}:\"$PATH\"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT={}; \
         export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1 CARGO_NET_OFFLINE=true CARGO_BUILD_JOBS={CPU_WRAPPER_CARGO_JOBS}; \
         export CARGO_TARGET_DIR={} TMPDIR={}; unset HERMIT_PREPARED_NEXTEST_REQUIRED; set +e; \
         {} --force {} build-cpu-wrapper >{} 2>{}; \
         prepare_status=$?; cat {}; cat {} >&2; exit \"$prepare_status\"",
        quoted(source),
        quoted(&source.join("ci/rust-script-bin")),
        quoted(&source.join("target/ci/rust-scripts")),
        quoted(target),
        quoted(&prepare.join("tmp")),
        quoted(&source.join("ci/rust-script-bin/rust-script")),
        quoted(&source.join("ci/nextest-binaries.rs")),
        quoted(&prepare.join("producer.stdout")),
        quoted(&prepare.join("producer.stderr")),
        quoted(&prepare.join("producer.stdout")),
        quoted(&prepare.join("producer.stderr")),
    );
    let step = step_with_caps(
        "fixture",
        "cpu-wrapper",
        "standalone Nextest CPU wrapper for the real Nextest fixture cases",
        command,
        Vec::new(),
        CPU_WRAPPER_WALL_SECONDS,
        CPU_WRAPPER_CPU_SECONDS,
        2 * 1024 * 1024 * 1024,
    );
    run_preparation_step(&prepare, step, deadline);
    // The operation prints the wrapper it built. It must be an executable file
    // in the fixture's private target, the directory every case builds into.
    let stdout = fs::read_to_string(prepare.join("producer.stdout")).unwrap();
    let lines = stdout.lines().collect::<Vec<_>>();
    assert_eq!(lines.len(), 1, "one CPU wrapper path: {stdout:?}");
    let wrapper = Path::new(lines[0]).canonicalize().unwrap();
    let metadata = fs::metadata(&wrapper).unwrap();
    assert!(
        metadata.is_file() && metadata.permissions().mode() & 0o111 != 0,
        "CPU wrapper {} is not an executable file",
        wrapper.display()
    );
    assert!(
        wrapper.starts_with(target.canonicalize().unwrap()),
        "CPU wrapper {} is outside the fixture target {}",
        wrapper.display(),
        target.display()
    );
}

/// Run one fixture preparation step alone, retain its graph and result in
/// `dir`, and require a single clean, successful attempt.
fn run_preparation_step(dir: &Path, step: dagrun::model::Step, deadline: u64) {
    let cfg = DagConfig {
        steps: vec![step],
        ..Default::default()
    };
    fs::write(dir.join("dag.json"), dag_to_json(&cfg)).unwrap();
    let result = run_lane_once(
        &cfg,
        1,
        true,
        0,
        None,
        &dir.join("driver.log"),
        Some(deadline),
        false,
    );
    fs::write(
        dir.join("result.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "complete":result.complete,"ok":result.ok,"run_timed_out":result.run_timed_out,
            "attempts":result.attempts.len(),
            "outcomes":result.outcomes.iter().map(|outcome| serde_json::json!({
                "tag":outcome.tag,"ok":outcome.ok,"returncode":outcome.returncode,
                "duration_s":outcome.duration_s,"aborted":outcome.aborted,
                "timed_out":outcome.timed_out,"cpu_timed_out":outcome.cpu_timed_out,
                "oomed":outcome.oomed,"oom_kills":outcome.oom_kills,
            })).collect::<Vec<_>>(),
        }))
        .unwrap(),
    )
    .unwrap();
    assert!(result.complete && result.ok && !result.run_timed_out);
    assert_eq!(result.outcomes.len(), 1);
    assert_eq!(result.attempts.len(), 1, "no preparation retry");
    let outcome = &result.outcomes[0];
    assert!(outcome.ok && outcome.returncode == Some(0));
    assert!(!outcome.aborted && !outcome.timed_out && !outcome.cpu_timed_out && !outcome.oomed);
    assert_eq!(outcome.oom_kills, 0);
}

fn context(source: &Path, outcomes: &[StepOutcome], run_id: &str) -> LedgerCtx {
    let (executed_tests, passed_tests, filtered_tests) = libtest_counts(outcomes);
    // An exact clean source is required for cumulative artifact binding. The
    // fixture remains nonqualifying even when its sole passing control passes.
    assert!(
        git_text(
            source,
            &["status", "--porcelain", "--untracked-files=normal"]
        )
        .is_empty()
    );
    LedgerCtx {
        run_id: Some(run_id.to_string()),
        admission_floor_evidence: None,
        admission_provenance_error: None,
        log_identity: None,
        base_observation: serde_json::Value::Null,
        main_observation: serde_json::Value::Null,
        started_at: utc_now(),
        host: "real-nextest-fixture".into(),
        toolchain: "fixture; actual tool versions retained separately".into(),
        slot: "private-script-test-fixture".into(),
        cwd: source.display().to_string(),
        profile: "full".into(),
        selection_mode: "only".into(),
        release_builder: super::super::RELEASE_BUILDER_CARGO,
        e2e_runner: super::super::E2E_RUNNER_CARGO,
        cache_state: "fixture-owned".into(),
        commit: git_text(source, &["rev-parse", "HEAD"]),
        tree: git_text(source, &["rev-parse", "HEAD^{tree}"]),
        git_depth: Some(0),
        git_ahead: Some(0),
        git_behind: Some(0),
        commit_anchored: false,
        tree_dirty: false,
        dag_jobs: 1,
        admission: None,
        base_sha: serde_json::Value::Null,
        base_tree: serde_json::Value::Null,
        reverie_base_sha: serde_json::Value::Null,
        reverie_base_tree: serde_json::Value::Null,
        reverie_pin_current: false,
        concurrent_validates: None,
        concurrency_proof: None,
        interruption: None,
        // These serializer fixture fields are not a whole-run CPU measurement.
        // Real per-attempt CPU records are retained and checked below.
        cpu_user: 0.0,
        cpu_sys: 0.0,
        retry_rounds: 0,
        executed_tests,
        passed_tests,
        filtered_tests,
    }
}

#[test]
fn actual_nextest_results_and_publication_failures() {
    let started_ns = monotonic_now_ns().expect("fixture requires CLOCK_MONOTONIC");
    let step_started_ns = env_u64(dagrun::scheduler::STEP_STARTED_MONOTONIC_NS_ENV).unwrap();
    let inherited_deadline_ns = env_u64(super::super::OWN_SCOPE_DEADLINE_ENV).unwrap();
    let claimed_nested = ["DAGRUN_OUTER_RUN", "DAGRUN_STEP"]
        .iter()
        .any(|name| std::env::var_os(name).is_some_and(|value| !value.is_empty()));
    let deadline = fixture_deadline(
        started_ns,
        claimed_nested,
        step_started_ns,
        inherited_deadline_ns,
    )
    .unwrap();
    let source = Path::new(file!())
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .canonicalize()
        .unwrap();
    let parent = PathBuf::from(
        std::env::var_os("DAGRUN_LOG_DIR")
            .expect("run this test through ci/rust-script-bin/run-test-harness"),
    );
    let tree = FixtureTree::create(&parent);
    let root = tree.root().to_path_buf();
    eprintln!("authentic Nextest fixture evidence: {}", root.display());
    assert!(
        git_text(
            &source,
            &["status", "--porcelain", "--untracked-files=normal"]
        )
        .is_empty(),
        "commit the candidate before running the exact-source fixture"
    );
    let original_head = git_text(&source, &["rev-parse", "HEAD"]);
    let fixture = tree.crate_dir();
    fs::create_dir_all(fixture.join("src")).unwrap();
    fs::write(fixture.join("Cargo.toml"), concat!(
        "[package]\nname = \"hermit\"\nversion = \"0.0.0\"\nedition = \"2024\"\npublish = false\n",
        "[lib]\nname = \"hermit_structured_results_fixture\"\n[workspace]\n",
    )).unwrap();
    fs::write(fixture.join("src/lib.rs"), concat!(
        "#[test]\nfn passes() { assert_eq!(2 + 2, 4); }\n",
        "#[test]\nfn fails() { assert_eq!(2 + 2, 5, \"intentional structured-result fixture assertion\"); }\n",
    )).unwrap();
    // The canonical .config/nextest.toml names the `cli` test binary, and
    // nextest refuses a config whose binary() matcher names no binary in the
    // workspace. An empty `cli` target keeps the copied config loadable; it has
    // no tests, so nothing it adds is selected or counted.
    fs::create_dir_all(fixture.join("tests")).unwrap();
    fs::write(fixture.join("tests/cli.rs"), b"").unwrap();
    let target = tree.target_dir();
    // Cargo's standard explicit/default home remains in force. Only the target
    // and temp directories below are fixture-owned; a test must not silently
    // copy or claim private ownership of the developer's registry/configuration.
    let cargo_home = std::env::var_os("CARGO_HOME").map(PathBuf::from);
    fs::write(root.join("bounds.json"), serde_json::to_vec_pretty(&serde_json::json!({
        "fixture_wall_seconds":FIXTURE_WALL_SECONDS,"fixture_started_ns":started_ns,
        "deadline_monotonic_ns":deadline,"inherited_step_started_ns":step_started_ns,
        "inherited_deadline_ns":inherited_deadline_ns,"claimed_nested":claimed_nested,
        "per_case_wall_seconds":180,"per_case_cpu_seconds":300,
        "cpu_wrapper_wall_seconds":CPU_WRAPPER_WALL_SECONDS,
        "cpu_wrapper_cpu_seconds":CPU_WRAPPER_CPU_SECONDS,
        "cpu_wrapper_cargo_jobs":CPU_WRAPPER_CARGO_JOBS,
        "declared_memory_bytes":2_u64*1024*1024*1024,"dag_width":1,"cargo_jobs":1,"nextest_retries":0,
        "per_case_cgroup_binding":null,"cargo_home_env":cargo_home,
        "cargo_home_policy":"conventional explicit/default Cargo home; ownership not inferred",
        "source":source,"source_head":original_head,"private_target":target,
        "metadata_root":source.join("target/ci/nextest-binaries"),
        "evidence":root,"diagnostic_only":true,"admission":null,
    })).unwrap()).unwrap();
    ensure_prepared_helpers(&source, &root, deadline);
    prepare_cpu_wrapper(&source, &root, &target, deadline);
    for (index, (name, failing, deny_publish, mismatch, expected_class)) in [
        (
            "writable-pass",
            false,
            false,
            false,
            NodeClassification::Pass,
        ),
        (
            "writable-failure",
            true,
            false,
            false,
            NodeClassification::ProductFailure,
        ),
        (
            "publish-after-pass",
            false,
            true,
            false,
            NodeClassification::NoResult,
        ),
        (
            "publish-after-failure",
            true,
            true,
            false,
            NodeClassification::ProductFailure,
        ),
        (
            "count-before-publish",
            false,
            true,
            true,
            NodeClassification::ProductFailure,
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let case = root.join(name);
        fs::create_dir_all(case.join("tmp")).unwrap();
        let names: BTreeSet<String> = if failing {
            ["passes", "fails"].into_iter().map(str::to_owned).collect()
        } else {
            BTreeSet::from(["passes".into()])
        };
        let filter = if failing {
            "test(=passes) | test(=fails)"
        } else {
            "test(=passes)"
        };
        let expected = if failing || mismatch { 2 } else { 1 };
        let lock = if index == 0 {
            format!(
                "cargo generate-lockfile --offline --manifest-path {} || exit $?; cargo --version; cargo nextest --version; rustc --version; ",
                quoted(&fixture.join("Cargo.toml")),
            )
        } else {
            String::new()
        };
        let fault = if deny_publish {
            "mkdir -- \"$DAGRUN_TEST_COUNTS_PATH\" || exit $?; "
        } else {
            ""
        };
        let command = format!(
            "cd {} || exit $?; export PATH={}:\"$PATH\"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT={}; \
             export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1 CARGO_NET_OFFLINE=true CARGO_BUILD_JOBS=1; \
             export CARGO_TARGET_DIR={} TMPDIR={}; unset HERMIT_PREPARED_NEXTEST_REQUIRED; \
             printf '%s\\n' \"$DAGRUN_TEST_COUNTS_PATH\" > {}; {lock}{fault}set +e; \
             NEXTEST_EXPECTED_EXECUTED={expected} HERMIT_NEXTEST_CPU_REPORT_PATH={} \
             {} --manifest-path {} --locked --offline \
             --profile ci --lib -j 1 --retries 0 --no-tests fail -E {} >{} 2>{}; \
             fixture_status=$?; cat {}; cat {} >&2; exit \"$fixture_status\"",
            quoted(&source),
            quoted(&source.join("ci/rust-script-bin")),
            quoted(&source.join("target/ci/rust-scripts")),
            quoted(&target),
            quoted(&case.join("tmp")),
            quoted(&case.join("result-path.txt")),
            quoted(&case.join("cpu.json")),
            quoted(&source.join("ci/run-nextest-counted.sh")),
            quoted(&fixture.join("Cargo.toml")),
            super::super::validate_plan::shell_quote(filter),
            quoted(&case.join("producer.stdout")),
            quoted(&case.join("producer.stderr")),
            quoted(&case.join("producer.stdout")),
            quoted(&case.join("producer.stderr")),
        );
        let mut step = step_with_caps(
            "fixture",
            name,
            "real Nextest structured-results fixture",
            command,
            Vec::new(),
            180,
            300,
            2 * 1024 * 1024 * 1024,
        );
        step.result_manifests = Some(vec![dagrun::model::ResultManifest::StructuredTestResults(
            dagrun::model::StructuredTestResultsManifest::current(step.tag()),
        )]);
        let cfg = DagConfig {
            steps: vec![step],
            ..Default::default()
        };
        fs::create_dir_all(case.join("ci/dag")).unwrap();
        let dag = dag_to_json(&cfg);
        let dag_path = case.join("ci/dag/validate.json");
        fs::write(&dag_path, &dag).unwrap();
        fs::write(
            case.join("ci/expected-e2e-plan.json"),
            "{\"schema\":1,\"cells\":[]}",
        )
        .unwrap();
        let plan = finish_committed_selection(
            Plan {
                cfg,
                profile: "full".into(),
                ..Plan::default()
            },
            dag_path,
            dag.into_bytes(),
        );
        let run_id = format!("{}-{name}", root.file_name().unwrap().to_str().unwrap());
        let prepared = validate_evidence::SelectedEvidence::capture(&case, &plan)
            .unwrap()
            .publish(&case, &plan, &run_id, &original_head)
            .unwrap();
        let started = Instant::now();
        let result = run_lane_once(
            &plan.cfg,
            1,
            true,
            0,
            None,
            &case.join("driver.log"),
            Some(deadline),
            false,
        );
        assert!(
            result.complete && !result.run_timed_out,
            "{name}: incomplete fixture; evidence {}",
            case.display()
        );
        assert_eq!(result.outcomes.len(), 1);
        assert_eq!(result.attempts.len(), 1, "no outer retry");
        let outcome = &result.outcomes[0];
        assert!(!outcome.aborted && !outcome.timed_out && !outcome.cpu_timed_out && !outcome.oomed);
        assert_eq!(outcome.oom_kills, 0);
        let nextest_status = if failing { 100 } else { 0 };
        let writer_status = if mismatch {
            2
        } else if deny_publish {
            75
        } else {
            0
        };
        let wrapper_status = if failing {
            nextest_status
        } else {
            writer_status
        };
        assert_eq!(outcome.returncode, Some(wrapper_status));
        let actual: BTreeMap<String, bool> = names
            .iter()
            .map(|name| (format!("hermit${name}"), name == "passes"))
            .collect();
        assert_eq!(
            node_classification(outcome, &result.attempts),
            expected_class
        );
        if deny_publish {
            let path = PathBuf::from(
                fs::read_to_string(case.join("result-path.txt"))
                    .unwrap()
                    .trim(),
            );
            assert!(
                path.is_dir(),
                "the actual scheduler-injected destination was made a directory"
            );
            assert_eq!(
                outcome.test_results_error_kind,
                Some(dagrun::TestResultsErrorKind::ReadIo)
            );
            let stderr = fs::read_to_string(case.join("producer.stderr")).unwrap();
            if mismatch {
                assert!(
                    outcome.test_results.is_none()
                        && outcome.executed_tests.is_none()
                        && outcome.filtered_tests.is_none()
                );
                assert!(stderr.contains(
                    "expected 2 tests to execute, saw 1, of which 1 passed and 0 failed"
                ));
                assert!(!stderr.contains("structured-test-results-publish"));
            } else {
                assert!(stderr.contains("structured-test-results-publish"));
                let rows: BTreeMap<String, bool> = outcome
                    .test_results
                    .as_ref()
                    .unwrap()
                    .iter()
                    .map(|row| (row.id.clone(), row.passed))
                    .collect();
                assert_eq!(rows, actual);
                assert_eq!(outcome.executed_tests, Some(names.len() as u64));
            }
            assert!(
                !dagrun::structured_test_results_recovery_path(&path).exists(),
                "scheduler must consume recovery, and count refusal must not create it"
            );
            assert!(
                !case.join("cpu.json").exists(),
                "primary report refusal still precedes aggregate CPU publication"
            );
            fs::remove_dir(path).unwrap(); // Only our own empty fault fixture, after import.
        } else {
            assert!(
                outcome.test_results_error.is_none() && outcome.test_results_error_kind.is_none()
            );
            let rows: BTreeMap<String, bool> = outcome
                .test_results
                .as_ref()
                .unwrap()
                .iter()
                .map(|row| (row.id.clone(), row.passed))
                .collect();
            assert_eq!(rows, actual);
            assert_eq!(outcome.executed_tests, Some(names.len() as u64));
            assert_eq!(
                json(&case.join("cpu.json"))["attempts"]
                    .as_array()
                    .unwrap()
                    .len(),
                names.len()
            );
        }
        let ctx = context(&source, &result.outcomes, &run_id);
        let retained =
            prepared.retain(&case, &case, &ctx, &result.outcomes, &result.attempts, None);
        let retained = if mismatch {
            assert!(
                retained.is_err(),
                "unknown test rows must not acquire a cumulative artifact"
            );
            None
        } else {
            Some(retained.unwrap())
        };
        let ledger = case.join("fixture-ledger.jsonl");
        let planned = result
            .outcomes
            .iter()
            .map(|outcome| outcome.tag.clone())
            .collect();
        let exit = match expected_class {
            NodeClassification::Pass => 0,
            NodeClassification::NoResult => 75,
            _ => 1,
        };
        write_ledger(
            &ledger,
            &ctx,
            &result.outcomes,
            &result.attempts,
            &[],
            &[],
            &planned,
            started.elapsed().as_secs_f64(),
            exit,
            case.join("producer.stderr").to_str().unwrap(),
            result.complete,
            serde_json::json!({
                "run_id":run_id,
                "planned_test_nodes":1,
                "executed_test_nodes":1,
                "zero_executed_nodes":[],
                "absent_nodes":[],
            }),
            None,
            retained.as_ref(),
        );
        let text = fs::read_to_string(&ledger).unwrap();
        assert_eq!(text.lines().count(), 1);
        let row: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(row["run_id"], run_id);
        assert_eq!(row["result"], expected_class.result());
        assert_eq!(row["selection_mode"], "only");
        assert_eq!(row["commit_anchored"], false);
        assert!(row["admission"].is_null());
        let fixture_node = BTreeSet::from([outcome.tag.clone()]);
        let (observations, observation_errors) =
            nextest_test_observations(&result.attempts, &fixture_node);
        if mismatch {
            assert_eq!(observation_errors.len(), 1);
        } else {
            assert!(observation_errors.is_empty(), "{observation_errors:?}");
        }
        if name == "publish-after-failure" {
            let summary = test_id_summary(observations, &result.attempts, &fixture_node);
            assert_eq!(summary.failed.len(), 1);
            assert_eq!(summary.failed[0].id, "hermit$fails");
            assert!(summary.failed_nodes_without_test_ids.is_empty());
        }
        if let Some(retained) = retained {
            retained
                .verify_record(&serde_json::from_value(row.clone()).unwrap())
                .unwrap();
            let artifact: hermit_manifest_plan::ledger::TestResultsEvidenceV9 =
                serde_json::from_value(row["test_results"].clone()).unwrap();
            let bytes = fs::read(case.join(&artifact.artifact.path)).unwrap();
            let rows = validate_test_results::verify_artifact(&artifact, &bytes).unwrap();
            let observed: BTreeMap<String, bool> = rows
                .into_iter()
                .map(|item| {
                    assert_eq!(item.run_id, run_id);
                    assert_eq!(item.hermit_sha, original_head);
                    (
                        item.id,
                        item.result == hermit_manifest_plan::ledger::TestResultVerdict::Pass,
                    )
                })
                .collect();
            assert_eq!(
                observed, actual,
                "authentic failing ID must reach the retained ledger artifact"
            );
        } else {
            assert!(row["executed_tests"].is_null() && row["passed_tests"].is_null());
            assert!(row.get("test_results").is_none());
            assert_eq!(row["gates"][0]["test_results_error_kind"], "read_io");
            // The written gate may omit an attempt list it states itself;
            // the history read back is the same either way.
            let attempts = ledger_gate_attempts(&row["gates"][0]).unwrap();
            assert_eq!(attempts.len(), 1, "{attempts:?}");
            assert_eq!(attempts[0]["test_results_error_kind"], "read_io");
        }
        fs::write(
            case.join("checked.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "actual_nextest_status":nextest_status,"actual_writer_status":writer_status,
                "actual_wrapper_status":wrapper_status,"classification":expected_class.as_str(),
                "test_ids_verified_from_scheduler_rows":actual,
                "wall_seconds":started.elapsed().as_secs_f64(),
            }))
            .unwrap(),
        )
        .unwrap();
    }
    assert_eq!(git_text(&source, &["rev-parse", "HEAD"]), original_head);
    assert!(
        git_text(
            &source,
            &["status", "--porcelain", "--untracked-files=normal"]
        )
        .is_empty()
    );
    tree.finish();
}
