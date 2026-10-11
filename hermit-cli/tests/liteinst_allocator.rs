//! Required scope-free allocation matrices for both actual preload leaves.
//! Fixture exports never select an allocator. Missing qualification, bootstrap,
//! reproducible placement or exact Q/W comparisons fail; no cases are skipped.

#[allow(dead_code)]
#[path = "../../reverie/scripts/m1_artifact.rs"]
mod artifact;
#[allow(dead_code)]
#[path = "../../reverie/scripts/m1_contract.rs"]
mod contract;
#[allow(dead_code)]
#[path = "common/hermit_binary.rs"]
mod hermit_binary;
#[path = "common/m1_bundle.rs"]
mod m1_bundle;
#[path = "common/liteinst_source_copy.rs"]
mod source_copy;

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use artifact::CompiledSourceExpectation;
use artifact::CompiledSourceLayout;
use artifact::ConsumerExpectation;
use artifact::FileIdentity;
use artifact::FixedImageKind;
use artifact::PackageIdentity;
use artifact::Result;
use artifact::Runner;
use artifact::RuntimeReceipt;
use artifact::VerifiedCompiledSources;
use contract::CapturedRun;
use contract::HeldInputs;
use contract::Mode;
use contract::TrialKey;
use m1_bundle::Bundle;
use m1_bundle::Leaf;
use serde_json::json;

const EPOCH: &str = "2026-10-09T00:00:00Z";

fn require(ok: bool, message: &str) -> Result<()> {
    if ok { Ok(()) } else { Err(message.into()) }
}

fn required_bundle() -> Result<Bundle> {
    let pointer = std::env::var_os("HERMIT_M1_ALLOCATOR_FIXTURE_BUNDLE")
        .ok_or("required HERMIT_M1_ALLOCATOR_FIXTURE_BUNDLE pointer is absent")?;
    let pointer = artifact::real_file(Path::new(&pointer))?;
    let manifest = fs::read_to_string(pointer).map_err(|e| e.to_string())?;
    let path = artifact::real_file(Path::new(manifest.trim()))?;
    let bundle: Bundle = serde_json::from_slice(&fs::read(path).map_err(|e| e.to_string())?)
        .map_err(|e| format!("typed fixture bundle: {e}"))?;
    require(
        bundle.schema == 1 && bundle.executed_tests == 0 && !bundle.full_m1_pass_claimed,
        "fixture producer may not claim executed tests or full M1",
    )?;
    require(
        bundle.reverie_head == env!("HERMIT_REVERIE_PIN"),
        "fixture/binary Reverie pin differs",
    )?;
    require(
        bundle.hermit_lock_sha256 == artifact::sha256(include_bytes!("../../Cargo.lock"))
            && bundle.nested_lock_sha256
                == artifact::sha256(include_bytes!("../../liteinst-runtime-build/Cargo.lock")),
        "fixture locks differ from the compiled test inputs",
    )?;
    let oracles = BTreeMap::from([
        ("rust_exports".into(), artifact::FROZEN_RUST_ORACLE.into()),
        ("c_workload".into(), artifact::FROZEN_C_ORACLE.into()),
    ]);
    require(
        bundle.oracle_sha256 == oracles,
        "frozen export/workload oracle identities differ",
    )?;
    require(
        bundle.config_fingerprint == detcore_model::config::config_wire_fingerprint(),
        "Detcore config fingerprint differs",
    )?;
    require(
        bundle.producer_sha256
            == artifact::sha256(include_bytes!(
                "../../ci/build-liteinst-allocator-fixtures.rs"
            ))
            && artifact::file_sha256(&bundle.producer_source)? == bundle.producer_sha256,
        "fixture producer differs from the compiled source",
    )?;
    artifact::verify_guest_receipt(
        &bundle.guest_receipt,
        &bundle.guest,
        &Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .ok_or("H root absent")?
            .join("reverie/reverie-liteinst/tests/fixtures/m1_allocator_guest.c"),
        artifact::FROZEN_C_ORACLE,
    )?;
    Ok(bundle)
}

fn qualify(
    bundle: &Bundle,
    standalone: bool,
    evidence: &Path,
    phase: &str,
) -> Result<(Leaf, RuntimeReceipt)> {
    let leaf = if standalone {
        &bundle.standalone
    } else {
        &bundle.detcore
    };
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .ok_or("H root absent")?;
    let pin = env!("HERMIT_REVERIE_PIN");
    let source_manifest = if standalone {
        root.join("liteinst-runtime-build/Cargo.toml")
    } else {
        root.join("Cargo.toml")
    };
    let mut runner = Runner::new(
        evidence.join(format!("{phase}-compiled-sources-logs")),
        Duration::from_secs(60),
        64 * 1024 * 1024,
    )?;
    let compiled_sources = VerifiedCompiledSources::resolve(
        &CompiledSourceExpectation {
            layout: CompiledSourceLayout::HermitGit,
            image_kind: if standalone {
                FixedImageKind::Standalone
            } else {
                FixedImageKind::Detcore
            },
            workspace_root: root,
            manifest: &source_manifest,
            reverie_pin: pin,
        },
        &mut runner,
    )?;
    let receipt: RuntimeReceipt = serde_json::from_slice(
        &fs::read(artifact::real_file(&leaf.receipt)?).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    let source = format!("git+https://github.com/rrnewton/reverie.git?rev={pin}#{pin}");
    let (name, target, manifest, package_source, id, initializer) = if standalone {
        require(
            fs::read_to_string(format!("{}.revision", leaf.runtime.display()))
                .map_err(|e| e.to_string())?
                .trim()
                == pin,
            "standalone revision sidecar differs",
        )?;
        require(
            receipt.cargo_artifact.package.source.as_deref() == Some(source.as_str()),
            "standalone exact Git source differs",
        )?;
        let manifest = artifact::real_file(&compiled_sources.package().manifest)?;
        require(
            manifest.file_name().is_some_and(|n| n == "Cargo.toml")
                && manifest
                    .parent()
                    .and_then(Path::file_name)
                    .is_some_and(|n| n == "reverie-liteinst-preload"),
            "standalone selected package is not the real preload leaf",
        )?;
        (
            "reverie-liteinst-preload",
            "reverie_liteinst_preload",
            manifest,
            Some(source.clone()),
            format!(
                "git+https://github.com/rrnewton/reverie.git?rev={pin}#reverie-liteinst-preload@0.4.1"
            ),
            artifact::STANDALONE_ROOT,
        )
    } else {
        let manifest = artifact::real_file(&root.join("detcore-liteinst/Cargo.toml"))?;
        (
            "detcore-liteinst",
            "detcore_liteinst",
            manifest,
            None,
            format!(
                "path+file://{}#0.4.0",
                root.join("detcore-liteinst").display()
            ),
            "detcore_liteinst_initialize",
        )
    };
    let package = PackageIdentity {
        id,
        name: name.into(),
        manifest: manifest.clone(),
        source: package_source,
        target: target.into(),
        kind: vec!["cdylib".into()],
        crate_types: vec!["cdylib".into()],
        target_source: artifact::real_file(
            &manifest
                .parent()
                .ok_or("leaf directory absent")?
                .join("src/lib.rs"),
        )?,
    };
    require(
        compiled_sources.package() == &package,
        "fresh metadata package differs from the independent leaf expectations",
    )?;
    let expected_profile = json!({"opt_level":"3", "debuginfo":0, "debug_assertions":!standalone,
        "overflow_checks":!standalone, "test":false});
    let profile_name = if standalone { "release" } else { "validate" };
    require(
        leaf.profile_name == profile_name && leaf.cargo_profile == expected_profile,
        "leaf profile differs from required diagnostic producer",
    )?;
    let features = vec!["allocator-fixture".into()];
    let expected = ConsumerExpectation {
        package: &package,
        features: &features,
        profile_name,
        profile: &expected_profile,
        initializer,
        source_head: &bundle.hermit_head,
        source_tree: &bundle.hermit_tree,
        source_lock_sha256: if standalone {
            &bundle.nested_lock_sha256
        } else {
            &bundle.hermit_lock_sha256
        },
        oracle_sha256: &bundle.oracle_sha256,
        compiled_sources: Some(&compiled_sources),
    };
    let intact_receipt = FileIdentity::read(&leaf.receipt)?;
    let receipt = artifact::verify_runtime_receipt(&leaf.receipt, &leaf.runtime, &expected)?;
    // Receipt fields are evidence, not source authority. Start from the real
    // qualifying receipt and change exactly its recorded full pin, while
    // retaining the independently resolved expectation and actual runtime.
    let mut changed = serde_json::to_value(&receipt).map_err(|e| e.to_string())?;
    let actual_pin = changed["compiled_sources"]["reverie_pin"]
        .as_str()
        .ok_or("qualified receipt lacks its source pin")?;
    let mut wrong_pin = actual_pin.to_owned();
    wrong_pin.replace_range(
        ..1,
        if actual_pin.starts_with('0') {
            "1"
        } else {
            "0"
        },
    );
    changed["compiled_sources"]["reverie_pin"] = json!(wrong_pin);
    let changed_path = evidence.join(format!("{phase}-wrong-compiled-source-receipt.json"));
    artifact::write_new(
        &changed_path,
        &serde_json::to_vec_pretty(&changed).map_err(|e| e.to_string())?,
    )?;
    let error = artifact::verify_runtime_receipt(&changed_path, &leaf.runtime, &expected)
        .err()
        .ok_or("receipt-authored source pin unexpectedly authorized the runtime")?;
    require(
        error == "runtime independently resolved compiled source identities differ",
        &format!("tampered source receipt failed for an unrelated reason: {error}"),
    )?;
    intact_receipt.verify()?;
    compiled_sources.verify_unchanged(&mut runner)?;
    artifact::write_new(
        &evidence.join(format!("{phase}-compiled-sources.json")),
        &serde_json::to_vec_pretty(&json!({
            "compiled_sources": compiled_sources.record(),
            "commands": runner.commands,
        }))
        .map_err(|e| e.to_string())?,
    )?;
    Ok((leaf.clone(), receipt))
}

fn retain(directory: &Path, label: &str, run: &CapturedRun) -> Result<()> {
    artifact::write_new(&directory.join(format!("{label}.stdout")), &run.stdout)?;
    artifact::write_new(&directory.join(format!("{label}.stderr")), &run.stderr)?;
    artifact::write_new(&directory.join(format!("{label}.json")), &serde_json::to_vec_pretty(&json!({
        "stdin":run.input, "status_code":run.status.and_then(|status| status.code()),
        "status":format!("{:?}",run.status), "capture_end":format!("{:?}",run.end),
        "elapsed_ns":run.elapsed.as_nanos(), "output_truncated":run.output_truncated,
        "program":run.launch.program, "arguments":run.launch.arguments, "cwd":run.launch.current_dir,
        "held_environment":run.launch.held.environment, "held_policy":run.launch.held.policy,
        "execution_claim":"raw trial; candidate pair comparison is separate",
    })).map_err(|e| e.to_string())?)
}

// Producer identities are untrusted until independently matched to this clean
// checkout. Read Git with closed stdin and the same bounded capture machinery.
fn source_guard(bundle: &Bundle, root: &Path, evidence: &Path, phase: &str) -> Result<()> {
    let mut environment = vec![
        (
            "PATH".into(),
            std::env::var_os("PATH").ok_or("Git PATH absent")?,
        ),
        ("LC_ALL".into(), "C".into()),
        ("GIT_OPTIONAL_LOCKS".into(), "0".into()),
    ];
    // The pinned-root wrapper supplies its Git safe-directory configuration.
    // Repository-location variables and replacement-object controls are never
    // inherited; all repositories and revisions below are explicit.
    for name in ["HOME", "GIT_CONFIG_GLOBAL", "GIT_CONFIG_SYSTEM"] {
        if let Some(value) = std::env::var_os(name) {
            environment.push((name.into(), value));
        }
    }
    let held = HeldInputs {
        environment,
        policy: b"independent clean source identity; closed stdin".to_vec(),
    };
    let read = |repo: &Path, name: &str, args: &[&str]| -> Result<String> {
        let mut command = Command::new("git");
        command
            .arg("--no-replace-objects")
            .args(args)
            .current_dir(repo);
        let label = format!("{phase}-source-{name}");
        let run = contract::run_without_input(command, &held).map_err(|failure| {
            for (index, capture) in failure.captures.iter().enumerate() {
                let _ = retain(evidence, &format!("{label}-failure-{index}"), capture);
            }
            failure.to_string()
        })?;
        retain(evidence, &label, &run)?;
        require(
            run.input.is_none()
                && run.status.is_some_and(|status| status.success())
                && run.end == contract::CaptureEnd::Complete
                && !run.output_truncated,
            "independent Git source read failed",
        )?;
        String::from_utf8(run.stdout).map_err(|e| e.to_string())
    };
    let h = read(
        root,
        "hermit",
        &["rev-parse", "HEAD", "HEAD^{tree}", "HEAD:reverie"],
    )?;
    let values: Vec<_> = h.lines().collect();
    require(
        values
            == [
                bundle.hermit_head.as_str(),
                bundle.hermit_tree.as_str(),
                env!("HERMIT_REVERIE_PIN"),
            ],
        "current Hermit HEAD/tree/gitlink differs from qualified producer or compiled pin",
    )?;
    require(
        read(
            root,
            "hermit-clean",
            &["status", "--porcelain=v1", "--untracked-files=all"],
        )?
        .is_empty(),
        "Hermit source must remain clean during actual fixture execution",
    )?;
    let rv = root.join("reverie");
    let identity = read(&rv, "reverie", &["rev-parse", "HEAD", "HEAD^{tree}"])?;
    let values: Vec<_> = identity.lines().collect();
    require(
        bundle.reverie_head == env!("HERMIT_REVERIE_PIN")
            && values == [env!("HERMIT_REVERIE_PIN"), bundle.reverie_tree.as_str()],
        "initialized Reverie HEAD/tree differs from compiled pin or producer",
    )?;
    require(
        read(
            &rv,
            "reverie-clean",
            &["status", "--porcelain=v1", "--untracked-files=all"],
        )?
        .is_empty(),
        "initialized Reverie source must remain clean during actual fixture execution",
    )
}

fn matrix(standalone: bool) -> Result<()> {
    let bundle = required_bundle()?;
    let label = if standalone { "standalone" } else { "detcore" };
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .ok_or("H root absent")?;
    // This fixed, process-owned path is runnable inside Hermit's namespace;
    // no CARGO_TARGET_TMPDIR guest path and no compilation in the consumer.
    let parent = root.join("target/ci/m1-allocator-results/build");
    fs::create_dir_all(&parent).map_err(|e| e.to_string())?;
    let prefix = format!("{label}-{}-", std::process::id());
    let evidence = tempfile::Builder::new()
        .prefix(&prefix)
        .tempdir_in(&parent)
        .map_err(|e| e.to_string())?
        .keep();
    eprintln!("M1 {label} evidence {}", evidence.display());
    source_guard(&bundle, root, &evidence, "before")?;
    let (leaf, receipt) = qualify(&bundle, standalone, &evidence, "before")?;
    let guest = FileIdentity::read(&bundle.guest)?;
    let cwd = bundle.guest.parent().ok_or("guest directory absent")?;
    let mut base = Command::new(if standalone {
        &bundle.guest
    } else {
        hermit_binary::hermit_binary()
    });
    base.current_dir(cwd);
    let mut environment = BTreeMap::<OsString, OsString>::from([
        (
            "PATH".into(),
            std::env::var_os("PATH").ok_or("pinned PATH absent")?,
        ),
        ("LC_ALL".into(), "C".into()),
        ("LD_BIND_NOW".into(), "1".into()),
    ]);
    if standalone {
        require(
            std::env::var_os("LD_PRELOAD").is_none(),
            "test process may not inherit an unqualified preload",
        )?;
        require(
            std::env::var_os("REVERIE_LITEINST_PRELOAD").as_deref()
                == Some(leaf.runtime.as_os_str()),
            "actual standalone configure_command selector differs from qualified leaf",
        )?;
        reverie_liteinst::configure_command(&mut base, reverie_liteinst::PreloadTool::Strace)
            .map_err(|e| e.to_string())?;
        for (name, value) in base.get_envs() {
            environment.insert(
                name.to_owned(),
                value
                    .ok_or("launcher removed a required environment value")?
                    .to_owned(),
            );
        }
        require(
            environment.get(&OsString::from("LD_PRELOAD"))
                == Some(&leaf.runtime.as_os_str().to_owned()),
            "public standalone launcher did not select exactly the qualified DSO",
        )?;
    } else {
        environment.insert(
            "HERMIT_LITEINST_TOOL_RUNTIME".into(),
            leaf.runtime.as_os_str().to_owned(),
        );
        base.args([
            "--log=warn",
            "--backend=in-guest-trap",
            "run",
            &format!("--epoch={EPOCH}"),
            "--seed=0",
            "--sched-seed=0",
            "--max-timeslice=disabled",
            "--strict",
            "--base-env=minimal",
            "--env=LD_BIND_NOW=1",
        ])
        .arg(format!("--workdir={}", cwd.display()))
        .arg("--")
        .arg(&bundle.guest);
    }
    let program = base.get_program().to_owned();
    let arguments: Vec<_> = base.get_args().map(|a| a.to_owned()).collect();
    let executable = FileIdentity::read(Path::new(&program))?;
    let policy = serde_json::to_vec(&json!({"hermit_head":bundle.hermit_head,"reverie_head":bundle.reverie_head,
        "runtime_sha256":receipt.artifact.sha256,"guest_sha256":guest.sha256,"executable_sha256":executable.sha256,
        "epoch":EPOCH,"seed":0,"sched_seed":0,"personality":"ADDR_NO_RANDOMIZE",
        "placement":if standalone {"pre-exec personality; native loader"} else {"Hermit guest bootstrap"},
        "selectors":"fixed two-byte stdin only","full_memory_isolation_claimed":false})).map_err(|e| e.to_string())?;
    let held = HeldInputs {
        environment: environment.into_iter().collect(),
        policy,
    };
    let mut pairs = Vec::new();
    for case in contract::FIXED_CASES {
        let mut runs = Vec::new();
        for mode in [Mode::Quiet, Mode::Work] {
            let mut command = Command::new(&program);
            command.args(&arguments).current_dir(cwd);
            if standalone {
                // SAFETY: the single-threaded fork child performs only Linux
                // personality syscalls; it fails exec on refusal rather than
                // accepting random placement or retrying a comparison.
                unsafe {
                    command.pre_exec(|| {
                        let current = libc::personality(0xffff_ffff);
                        if current < 0 {
                            return Err(std::io::Error::last_os_error());
                        }
                        if libc::personality(
                            current as libc::c_ulong | libc::ADDR_NO_RANDOMIZE as libc::c_ulong,
                        ) < 0
                        {
                            return Err(std::io::Error::last_os_error());
                        }
                        Ok(())
                    });
                }
            }
            let key = TrialKey { mode, case };
            let name = format!("{}{}", mode.byte() as char, case.selector() as char);
            let run = contract::run_bounded(command, key.input(), &held).map_err(|failure| {
                for (index, capture) in failure.captures.iter().enumerate() {
                    let _ = retain(&evidence, &format!("{name}-failure-{index}"), capture);
                }
                format!("{failure}; evidence {}", evidence.display())
            })?;
            retain(&evidence, &name, &run)?;
            if !standalone {
                require(
                    run.stderr
                        .windows(b"hermit: [in-guest-trap] selected:".len())
                        .any(|w| w == b"hermit: [in-guest-trap] selected:"),
                    "actual Hermit did not report in-guest Detcore selection",
                )?;
            }
            runs.push(run);
        }
        pairs.push(
            contract::compare_pair(&runs[0], &runs[1], case, &leaf.runtime)
                .map_err(|failure| format!("{failure}; raw evidence {}", evidence.display()))?,
        );
    }
    let counts = contract::validate_matrix(&pairs).map_err(|e| e.to_string())?;
    guest.verify()?;
    executable.verify()?;
    receipt.artifact.verify()?;
    qualify(&bundle, standalone, &evidence, "after")?;
    source_guard(&bundle, root, &evidence, "after")?;
    artifact::write_new(&evidence.join("matrix.json"), &serde_json::to_vec_pretty(&json!({
        "quiet_controls":counts.quiet_controls,"completed_work_runs":counts.completed_work_runs,
        "completed_work_case_rows":counts.completed_work_case_rows,"observed_work_operations":counts.observed_work_operations,
        "literal_address_pairs":counts.literal_address_pairs,"rust_allocator_contract_passed":true,
        "full_memory_isolation_claimed":false,"libc_internal_allocation_claimed":false,
    })).map_err(|e|e.to_string())?)?;
    Ok(())
}

#[test]
fn genuine_standalone_leaf_keeps_scope_free_work_private() -> Result<()> {
    matrix(true)
}

#[test]
fn genuine_detcore_leaf_keeps_scope_free_work_private() -> Result<()> {
    matrix(false)
}
