//! The generated partition of the committed validation DAG, shared by
//! `scripts/validate.rs` (its `--write-generated-plan` export and the plans it
//! builds) and the `hermit-manifest-plan` library, which builds the partition
//! in-process for `generate-validation-dag` and for the freshness test
//! `validation_dag::tests::full_generator_refuses_static_artifact_mutations`.
//!
//! The library used to run `scripts/validate.rs --write-generated-plan`
//! through rust-script, which compiles the script in release mode inside the
//! test's 57 s budget and behind the host-wide rust-script cargo build lock.
//! Sharing this source lets both callers build the same nodes from the same
//! code without compiling a script.
//!
//! This file must compile in both crates: it names only `std`, `dagrun` and
//! its sibling modules `crate::validate_corpus`, `crate::validate_plan` and
//! `crate::validate_super`, which both crates include from `scripts/lib/`.

use std::collections::BTreeMap;
use std::path::Path;

use dagrun::model::CmdType;
use dagrun::model::DagConfig;
use dagrun::model::Step;

use crate::validate_corpus;
use crate::validate_plan;
use crate::validate_plan::CompatMode;
use crate::validate_super;

pub(crate) const RUST_SCRIPT_COMMAND_PREFIX: &str = "export PATH=\"$PWD/ci/rust-script-bin:$PATH\"; \
    export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT=\"$PWD/target/ci/rust-scripts\"; \
    export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; ";

/// Heavy compatibility preparation is the innermost bound in the validation ladder:
///
/// `420 prep < 480 gate clamp < 600 whole run < 660 local scope < 720 node < 900 job`.
///
/// A 3600s preparation allowance inside a 900s job was unreachable by
/// construction. This bound fires while the scheduler can still name the node
/// and flush its profile row.
pub(crate) const COMPAT_DIAGNOSTIC_WALL_S: i64 = 420;

/// What tests/compat/prepare_real_compat_fixtures.sh writes, for the paragraphs
/// of the nodes that run it.
pub(crate) const REAL_COMPAT_FIXTURE_CONTENTS: &str = "a copy of README.md, compiled binutils, gprof, gcov and lsof inputs, a loopback HTTP server, df's mount fixture, and cargo/rustc links into the active toolchain";

/// The nodes of the mechanically derived partition of the committed DAG.
///
/// Static nodes are authored in the private `validation_dag_static` module.
/// Compatibility rows come from the checked-in corpus, while stress repetitions
/// come from the typed probe definitions in `validate_super`; `repetitions` is
/// the stress repetition count (`validate_super::repetitions()` for a validate
/// run, the generator's fixed `SUPER_REPETITIONS` for the committed DAG). The
/// maintenance generator combines those independent inputs into the committed
/// `ci/dag/validate.json`.
pub(crate) fn generated_partition_steps(
    root: &Path,
    tmp: &Path,
    repetitions: i64,
) -> Result<Vec<Step>, String> {
    // These nodes exist only to satisfy dependency closure while the typed
    // compat/stress builders emit their generator-owned partitions. They are
    // discarded before the committed DAG is written; authoritative definitions
    // live in hermit-manifest-plan's private static source.
    let anchor_tags = [
        "build.e2e_artifact",
        "build.host_hermit_link",
        "compatprep.hermit_release",
        "gate.manifest",
        "setup.nextest",
        "doc.doctests",
        "doc.rustdoc",
        "lint.clippy",
        "test.detcore_unit",
        "test.hermit_unit",
        "test.regular_crates",
        "test.rr_suite_contract",
        "super.build_release_hermit",
        "super.build_workspace",
    ];
    let mut steps = anchor_tags
        .iter()
        .map(|tag| {
            let (group, job) = tag
                .split_once('.')
                .ok_or_else(|| format!("invalid generator dependency anchor {tag}"))?;
            let mut step = step_with_caps(
                group,
                job,
                "Generator dependency anchor",
                "true".into(),
                Vec::new(),
                1,
                1,
                1,
            );
            step.labels = vec!["generator-dependency-anchor".into()];
            Ok(step)
        })
        .collect::<Result<Vec<_>, String>>()?;

    // The portable strict corpus itself is the manifest bucket
    // e2e.manifest_compat (tests/e2e/manifests/compat.yaml); this node only
    // prepares the files its rows read.
    let portable_root = tmp.join("strict-compat");
    let portable_fixtures = portable_root.join("real-compat-fixtures");
    let mut portable_prep = prepare_fixtures_node("compatprep.fixtures", &portable_fixtures);
    portable_prep.deps = [
        "build.e2e_artifact",
        "doc.doctests",
        "doc.rustdoc",
        "lint.clippy",
        "test.detcore_unit",
        "test.hermit_unit",
        "test.regular_crates",
        "test.rr_suite_contract",
    ]
    .into_iter()
    .map(str::to_string)
    .collect();
    portable_prep.desc =
        "Prepare the fixture files the portable strict compatibility corpus reads".into();
    portable_prep.description = format!(
        "Runs tests/compat/prepare_real_compat_fixtures.sh into {}: {REAL_COMPAT_FIXTURE_CONTENTS}, \
         so the corpus rows of e2e.manifest_compat read run-owned files instead of the \
         checkout. It starts after every non-guest Cargo node so the corpus's shell-build \
         row cannot observe a concurrent target or cache mutation.",
        portable_fixtures.display()
    );
    portable_prep.labels = vec!["full".into(), "portable".into()];
    steps.push(portable_prep);

    // The SaBRe run type's rows are the compat.yaml cells labelled
    // sabre-compat-only, run by the static node sabrecompat.manifest_compat
    // against the validation's one Hermit build; this node prepares the files
    // those rows read, at the path the rows name.
    let mut sabre_prep = prepare_fixtures_node_dep(
        "sabrecompatprep.fixtures",
        &portable_fixtures,
        "build.host_hermit_link",
    );
    sabre_prep.group = "sabrecompatprep".into();
    sabre_prep.desc = "Prepare the fixture files the SaBRe run type's compat cells read".into();
    sabre_prep.description = format!(
        "Runs tests/compat/prepare_real_compat_fixtures.sh into {} on the host, once build.host_hermit_link has linked the validation's one Hermit build: {REAL_COMPAT_FIXTURE_CONTENTS}, the run-owned files the compat.yaml rows of sabrecompat.manifest_compat read. A fixture that fails to build or a missing host tool stops the run type before the bucket runs.",
        portable_fixtures.display()
    );
    sabre_prep.labels = vec!["sabre-compat-only".into()];
    steps.push(sabre_prep);

    // The rr run type's rows are the compat.yaml replay cells labelled
    // rr-compat-only, run the same way by rrcompat.manifest_compat.
    let mut rr_prep = prepare_fixtures_node_dep(
        "rrcompatprep.fixtures",
        &portable_fixtures,
        "build.host_hermit_link",
    );
    rr_prep.group = "rrcompatprep".into();
    rr_prep.desc = "Prepare the fixture files the rr run type's compat cells read".into();
    rr_prep.description = format!(
        "Runs tests/compat/prepare_real_compat_fixtures.sh into {} on the host, once build.host_hermit_link has linked the validation's one Hermit build: {REAL_COMPAT_FIXTURE_CONTENTS}, the run-owned files the compat.yaml rows of rrcompat.manifest_compat read. A fixture that fails to build or a missing host tool stops the run type before the bucket runs.",
        portable_fixtures.display()
    );
    rr_prep.labels = vec!["rr-compat-only".into()];
    steps.push(rr_prep);

    // The corpus-only run type's rows are tests/e2e/manifests/compat.yaml,
    // run by the static bucket node portablecompat.manifest_compat against the
    // lane's pinned-root release build; this node prepares the files they
    // read, at the path the rows name.
    let mut portable_focused_prep = prepare_fixtures_node_dep(
        "portablecompatprep.fixtures",
        &portable_fixtures,
        "compatprep.hermit_release",
    );
    portable_focused_prep.group = "portablecompatprep".into();
    portable_focused_prep.labels = vec!["portable-strict-compat-only".into()];
    portable_focused_prep.desc =
        "Prepare the fixture files the corpus-only lane's compat bucket reads".into();
    portable_focused_prep.description = format!(
        "Runs tests/compat/prepare_real_compat_fixtures.sh into {} on the host, after the lane's pinned-root release build: {REAL_COMPAT_FIXTURE_CONTENTS}, the run-owned files the compat.yaml rows of portablecompat.manifest_compat read. A fixture that fails to build or a missing host tool stops the lane before the bucket runs.",
        portable_fixtures.display()
    );
    steps.push(portable_focused_prep);
    steps.extend(generated_focused_compat_partition(
        root,
        tmp,
        CompatMode::E9patch,
        "e9patchcompat",
        "e9patch-compat-only",
    )?);

    // The super run type's compat rows are the compat.yaml heavy rows' ptrace
    // cells, labelled super and run by the static node
    // supercompat.manifest_compat against super.build_release_hermit's
    // release Hermit; this node prepares the files those rows read, at the
    // path the rows name.
    let mut super_prep = prepare_fixtures_node_dep(
        "super-compatprep.fixtures",
        &portable_fixtures,
        "super.build_release_hermit",
    );
    super_prep.group = "super-compatprep".into();
    super_prep.labels = vec!["super".into()];
    super_prep.desc = "Prepare the fixture files the super run type's compat cells read".into();
    super_prep.description = format!(
        "Runs tests/compat/prepare_real_compat_fixtures.sh into {} on the host, after super.build_release_hermit: {REAL_COMPAT_FIXTURE_CONTENTS}, the run-owned files the compat.yaml heavy rows (rustc, javac, java and node) of supercompat.manifest_compat read; rustc calls the toolchain link it makes because --base-env=minimal keeps the user's rustup directory off PATH. A fixture that fails to build or a missing host tool stops the run type before the bucket runs.",
        portable_fixtures.display()
    );
    steps.push(super_prep);

    let mut stress = validate_super::stress_nodes(
        &root.join("target/release/hermit").to_string_lossy(),
        &root.join("target/debug/hermit").to_string_lossy(),
        tmp,
        repetitions,
        "super.build_release_hermit",
        "super.build_workspace",
    );
    for step in &mut stress {
        step.labels = vec!["super".into()];
    }
    steps.extend(stress);

    Ok(steps)
}

/// The generated partition as the library's generator consumes it, so that
/// `ci/dag/validate.json` comes out as validate's `--write-generated-plan`
/// export made it before this was shared: the nodes of
/// [`generated_partition_steps`], each command prefixed with
/// [`RUST_SCRIPT_COMMAND_PREFIX`] as validate's prebuilt rust-script pass
/// (`configure_prebuilt_rust_scripts`) did, and every inherited CPU budget made
/// explicit ([`materialize_source_cpu_timeouts`]) as the export did.
///
/// validate's other pre-export passes also touched these nodes, but not the
/// committed DAG: it assigned fail-fast families and a VALIDATE_VERBOSITY
/// variable, and the library's own finalization gives the same families and
/// drops that variable. Measured on hermit main 53e6393e by exporting before
/// all of those passes, the committed DAG differed only in the prefix, on these
/// 193 nodes.
///
/// This function, not the script's export, is now what the committed DAG and
/// its freshness test (`full_generator_refuses_static_artifact_mutations`) are
/// built from. A pass added later to validate's export alone would change what
/// `--write-generated-plan` writes but not the committed DAG, and the freshness
/// test would not notice; runtime validation reads the committed DAG.
pub(crate) fn committed_generated_partition(
    root: &Path,
    tmp: &Path,
    repetitions: i64,
) -> Result<DagConfig, String> {
    let steps = generated_partition_steps(root, tmp, repetitions)?;
    let mut cfg = validate_plan::config_from(steps, "generated validation DAG partition");
    for step in &mut cfg.steps {
        if !step.cmd.starts_with(RUST_SCRIPT_COMMAND_PREFIX) {
            step.cmd = format!("{RUST_SCRIPT_COMMAND_PREFIX}{}", step.cmd);
        }
    }
    materialize_source_cpu_timeouts(&mut cfg)?;
    Ok(cfg)
}

pub(crate) fn generated_focused_compat_partition(
    root: &Path,
    tmp: &Path,
    mode: CompatMode,
    namespace: &str,
    label: &str,
) -> Result<Vec<Step>, String> {
    let run_root = tmp.join(namespace);
    let fixtures = run_root.join("real-compat-fixtures");
    let shell_build = run_root.join("shell-build");
    let nsswitch = run_root.join("nsswitch.conf");
    let paths = validate_corpus::CorpusPaths {
        root_dir: &root.to_string_lossy(),
        real_compat_fixtures: &fixtures.to_string_lossy(),
        validation_tmp_dir: &run_root.to_string_lossy(),
        shell_build_dir: &shell_build.to_string_lossy(),
    };
    let prep_tag = format!("{namespace}prep.fixtures");
    let mut prep = prepare_fixtures_node_dep(&prep_tag, &fixtures, "compatprep.hermit_release");
    prep.group = format!("{namespace}prep");
    prep.labels = vec![label.into()];
    if mode == CompatMode::E9patch {
        prep.description = format!(
            "Runs tests/compat/prepare_real_compat_fixtures.sh into {} on the host, after compatprep.hermit_release and the files-only NSS fixture: {REAL_COMPAT_FIXTURE_CONTENTS}. Every {namespace} probe depends on it, and the rows that run tests/compat/real_compat_workload.sh read these run-owned files through REAL_COMPAT_FIXTURES instead of the checkout. A fixture that fails to build or a missing host tool stops the e9patch-compat-only profile before any probe runs.",
            fixtures.display()
        );
    }

    let mut steps = Vec::new();
    if mode == CompatMode::E9patch {
        // The e9patch cells run the host release Hermit built by
        // compatprep.hermit_release, which resolves its packaged resources
        // (the e9patch and e9tool binaries among them) through the host's
        // target/install_pkg. The retired host build.runtime_release staged that
        // tree as a side effect until 2026-09-30; this lane-local node stages it
        // for the same release profile, so the lane keeps its own host release
        // build and does not pull in the validation's pinned-root build.
        let mut install = step_with_caps(
            &format!("{namespace}prep"),
            "release_resources",
            "Stage the release backend resources for the host e9patch cells",
            "./ci/run-with-reverie-dbt-budget.sh cargo build --release --locked -p detcore-dbt && ./ci/run-with-reverie-dbt-budget.sh cargo build --release --locked -p hermit --features third-party-backends -p detcore-dbt -p detcore-sabre -p hermit-install && test -x target/install_pkg/rsrcs/e9patch && test -x target/install_pkg/rsrcs/e9tool".into(),
            vec!["compatprep.hermit_release".into()],
            1200,
            7200,
            9 * 1024 * 1024 * 1024,
        );
        install.labels = vec![label.into()];
        install.description = format!(
            "Runs `cargo build --release --locked -p detcore-dbt`, then a release build of hermit with third-party-backends together with detcore-dbt, detcore-sabre and hermit-install, both through ci/run-with-reverie-dbt-budget.sh (which sets Cargo's job count from the calibrated DBT build budget), and finally `test -x` on target/install_pkg/rsrcs/e9patch and e9tool. The {namespace} probes run the host release Hermit built by compatprep.hermit_release, which finds its packaged resources, the e9patch and e9tool binaries among them, through target/install_pkg; this node stages that tree for the e9patch-compat-only profile. If hermit-install stops staging e9tool, the final `test -x` fails here instead of in every probe."
        );
        let mut nss = nsswitch_fixture_node(&nsswitch);
        nss.group = format!("{namespace}prep");
        nss.labels = vec![label.into()];
        nss.description = format!(
            "Writes a files-only nsswitch.conf (every database, aliases through shadow, set to \"files\") to {} with mkdir and printf. The six probes whoami, groups, pinky, logname, tar and chown bind it read-only over /etc/nsswitch.conf, because they look up user and group names that the host may resolve through an identity daemon; pinning them to files keeps that host race out of the e9patch compatibility measurement. It runs after release_resources, and the lane's fixtures node waits for it. It fails only if the run-state directory cannot be created or written, which shows as a mkdir or printf error.",
            nsswitch.display()
        );
        nss.deps = vec![install.tag()];
        prep.deps.push(nss.tag());
        steps.push(install);
        steps.push(nss);
    }
    steps.push(prep);
    let mut probes = validate_plan::compat_nodes(
        root,
        mode,
        &root.join("target/release/hermit").to_string_lossy(),
        &nsswitch.to_string_lossy(),
        &paths,
        Some(&prep_tag),
    )?;
    for step in &mut probes {
        step.group = namespace.into();
        step.labels = vec![label.into()];
    }
    steps.extend(probes);
    Ok(steps)
}

/// Make every inherited CPU budget explicit before a source plan crosses the
/// DAG document boundary. `dag_to_json` intentionally does not serialize the
/// execution-only default, so leaving zeroes here would turn validate's 7200s
/// fallback into dagrun's 10s undeclared-node forcing function on reload.
pub(crate) fn materialize_source_cpu_timeouts(cfg: &mut DagConfig) -> Result<(), String> {
    if cfg.default_step_cpu_timeout <= 0 {
        return Err(format!(
            "source plan has no positive default CPU timeout (got {})",
            cfg.default_step_cpu_timeout
        ));
    }
    for step in &mut cfg.steps {
        if step.cpu_timeout <= 0 {
            step.cpu_timeout = cfg.default_step_cpu_timeout;
        }
    }
    Ok(())
}

pub(crate) fn prepare_fixtures_node(_tag: &str, fixtures: &Path) -> dagrun::model::Step {
    prepare_fixtures_node_dep(_tag, fixtures, "compatprep.hermit_release")
}

/// The functional-fixture prep node, with an explicit predecessor.
///
/// The `super` suite already builds a release Hermit under its own tag, so it
/// hangs the fixtures off THAT node instead of adding a second identical build.
pub(crate) fn prepare_fixtures_node_dep(
    _tag: &str,
    fixtures: &Path,
    dep: &str,
) -> dagrun::model::Step {
    step_with_caps(
        "compatprep",
        "fixtures",
        "Functional compatibility fixtures",
        format!(
            "./tests/compat/prepare_real_compat_fixtures.sh {}",
            validate_plan::shell_quote(&fixtures.to_string_lossy())
        ),
        vec![dep.to_string()],
        COMPAT_DIAGNOSTIC_WALL_S,
        COMPAT_DIAGNOSTIC_WALL_S,
        4 * 1024 * 1024 * 1024,
    )
}

/// `require_e9patch_artifacts`' files-only NSS fixture (validate.sh:4095): keeps
/// host identity-daemon races out of the e9patch compatibility measurement.
pub(crate) fn nsswitch_fixture_node(path: &Path) -> dagrun::model::Step {
    let entries = [
        "aliases",
        "automount",
        "ethers",
        "group",
        "gshadow",
        "hosts",
        "initgroups",
        "netgroup",
        "netmasks",
        "networks",
        "passwd",
        "protocols",
        "publickey",
        "rpc",
        "services",
        "shadow",
    ]
    .iter()
    .map(|k| format!("{k}: files"))
    .collect::<Vec<_>>()
    .join("\\n");
    step_with_caps(
        "compatprep",
        "nsswitch",
        "e9patch files-only NSS fixture",
        format!(
            "mkdir -p $(dirname {p}) && printf '{entries}\\n' > {p}",
            p = validate_plan::shell_quote(&path.to_string_lossy())
        ),
        vec![],
        60,
        30,
        512 * 1024 * 1024,
    )
}

pub(crate) fn step_with_caps(
    group: &str,
    job: &str,
    desc: &str,
    cmd: String,
    deps: Vec<String>,
    timeout: i64,
    cpu_timeout: i64,
    mem: i64,
) -> dagrun::model::Step {
    dagrun::model::Step {
        group: group.into(),
        job: job.into(),
        desc: desc.into(),
        description: String::new(),
        cmd,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        result_manifests: None,
        labels: Vec::new(),
        deps,
        env: BTreeMap::new(),
        hint: dagrun::model::ResourceHint {
            rss_baseline_bytes: Some(mem),
            hard_mem_max_bytes: Some(mem),
            ..Default::default()
        },
        networkonly: false,
        engine_only: false,
        delegated_children: false,
        timeout,
        cpu_timeout,
        jobs_flag: None,
        jobs_env: None,
        skip_reason: None,
        // Undeclared, as these nodes were before the runner grew the fields. See
        // validate_plan::node for why this is not `Some(vec![])`.
        write_domains: None,
        write_domain_guarantee: None,
        explains: Vec::new(),
        fail_fast_family: None,
    }
}
