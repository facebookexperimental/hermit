use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs::File;
use std::fs::OpenOptions;
use std::fs::{self};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::os::unix::process::ExitStatusExt;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::ExitStatus;
use std::process::Stdio;
use std::thread;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use detcore_model::summary::PathEvidence;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value as JsonValue;
use sha2::Digest;
use sha2::Sha256;

use crate::canonical_verdict::InfrastructureError;
use crate::canonical_verdict::Verdict;
pub use crate::canonical_verdict::VerificationReport;
pub use crate::canonical_verdict::VerificationRuntime;
use crate::ci_selection::CiDisabledReasonSpec;
use crate::ci_selection::CiSelection;
use crate::ci_selection::CiSelectionSpec;
use crate::environmental_block::EnvBlockClass;
use crate::environmental_block::environmental_block_observation;
use crate::host_capability::probe_host_capabilities;
use crate::stress_series::HostCapabilities;
use crate::stress_series::HostCapability;
#[cfg(test)]
use crate::stress_series::HostCapabilityVerdict;
#[cfg(test)]
use crate::timeouts::CALIBRATED_CI_CELL_COUNT;
#[cfg(test)]
use crate::timeouts::DEFAULT_TEST_CPU_TIMEOUT_SECONDS;
#[cfg(test)]
use crate::timeouts::DEFAULT_TEST_WALL_TIMEOUT_SECONDS;
use crate::timeouts::DEFAULTS_FILE;
#[cfg(test)]
use crate::timeouts::EXPLICIT_TIMEOUT_CALIBRATIONS;
#[cfg(test)]
use crate::timeouts::KVM_NEXT40_QUALIFIED_CI_CELL_COUNT;
#[cfg(test)]
use crate::timeouts::KVM_PINNED_IMAGE_QUALIFIED_CI_CELL_COUNT;
#[cfg(test)]
use crate::timeouts::KVM_RATCHET_CI_CELL_COUNT;
#[cfg(test)]
use crate::timeouts::KVM_RATCHET_TIMEOUT_CALIBRATIONS;
#[cfg(test)]
use crate::timeouts::KVM_RUN_1709_CI_REMOVAL_COUNT;
use crate::timeouts::MANIFEST_SCHEMA;
#[cfg(test)]
use crate::timeouts::NON_CI_CELL_COUNT;
use crate::timeouts::ResolvedTestTimeouts;
use crate::timeouts::TimeoutMultipliers;
use crate::timeouts::resolve_test_timeouts;
use crate::timeouts::resolve_timeout_seconds;
use crate::timeouts::timeout_multipliers_from_env;
use crate::timeouts::validate_timeout_seconds;

const BACKENDS: [&str; 5] = ["ptrace", "dbt", "kvm", "sabre", "liteinst"];
const MODES: [&str; 5] = ["verify", "chaos", "replay", "naked", "custom"];
const CELL_CPU_POLL_INTERVAL: Duration = Duration::from_millis(100);
const CELL_CPU_ACCOUNTING_GRACE: Duration = Duration::from_secs(1);
pub const CELL_RESULT_SCHEMA: u64 = 4;
pub const E2E_MACHINE_SHORTNAME_ENV: &str = "E2E_MACHINE_SHORTNAME";
pub const E2E_KERNEL_VERSION_ENV: &str = "E2E_KERNEL_VERSION";
pub const E2E_RUN_INDEX_ENV: &str = "E2E_RUN_INDEX";

fn first_attempt() -> u64 {
    1
}

/// Closed vocabulary for manifest `requires` tokens.
///
/// The optional value is the host capability whose proven absence may withhold
/// a cell. A token mapped to `None` is still validated, but can never suppress
/// execution. Keeping the mapping here gives the manifest validator and the
/// executable harness one authority.
pub const REQUIRES_VOCABULARY: &[(&str, Option<HostCapability>)] = &[
    ("ar", None),
    ("bash", None),
    ("cc", None),
    ("cpuid", Some(HostCapability::CpuidFaulting)),
    ("cxx", None),
    ("date", None),
    ("du", None),
    ("find", None),
    ("gawk", None),
    ("git", None),
    ("hexdump", None),
    ("jq", None),
    ("kvm", None),
    ("linux", None),
    ("lua5.4", None),
    ("m4", None),
    ("node", None),
    ("openssl", None),
    ("perl", None),
    ("ptrace", None),
    ("python3", None),
    ("ruby", None),
    ("rustc", None),
    ("sqlite3", None),
    ("tclsh", None),
    ("userns", None),
    ("x86_64", None),
    ("zstd", None),
];

pub fn requires_capability(token: &str) -> Result<Option<HostCapability>, String> {
    REQUIRES_VOCABULARY
        .iter()
        .find(|(name, _)| *name == token)
        .map(|(_, capability)| *capability)
        .ok_or_else(|| {
            format!(
                "unknown `requires` token `{token}`; the vocabulary is closed and an unrecognized \
                 name is refused rather than treated as a reason to omit a cell"
            )
        })
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestDefaults {
    pub schema: u64,
    pub timeout_seconds: u64,
    pub cpu_timeout_seconds: u64,
    #[serde(default)]
    pub nextest: Vec<NextestTimeout>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NextestTimeout {
    pub filter: String,
    pub timeout_seconds: u64,
    pub slow_reason: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestDocument {
    pub schema: u64,
    pub bucket: String,
    pub timeout_seconds: Option<u64>,
    pub slow_reason: Option<String>,
    pub test: Vec<TestRecipe>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TestRecipe {
    pub id: String,
    pub description: String,
    pub lane: String,
    #[serde(default)]
    pub requires: Vec<String>,
    pub occasional: bool,
    pub program: Option<String>,
    pub direct: Option<DirectCommand>,
    pub observation: Observation,
    pub build: Option<BuildRecipe>,
    pub modes: BTreeMap<String, ModeRecipe>,
    #[serde(default)]
    pub preprocessors: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
pub enum DirectCommand {
    Shell(String),
    Argv(Vec<String>),
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuildRecipe {
    #[serde(default)]
    pub cflags: Vec<String>,
    #[serde(default)]
    pub rustflags: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Observation {
    pub status: bool,
    pub stdout: bool,
    pub stderr: bool,
    #[serde(default)]
    pub artifacts: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModeRecipe {
    pub ci: CiSelectionSpec,
    pub ci_disabled_reason: Option<CiDisabledReasonSpec>,
    #[serde(default)]
    pub backends_enabled: Vec<String>,
    #[serde(default)]
    pub backends_disabled: BTreeMap<String, String>,
    #[serde(default)]
    pub guest_args: BTreeMap<String, Vec<String>>,
    pub workdir: Option<String>,
    pub runs: Option<u64>,
    pub seeds: Option<Vec<i64>>,
    pub assert: Option<Assertions>,
    pub outcome_classes: Option<u64>,
    #[serde(default)]
    pub args: Vec<String>,
    pub compare_io_buffers: Option<bool>,
    pub compare_io_buffers_disabled_reason: Option<String>,
    pub rcb_time: Option<bool>,
    pub rcb_time_disabled_reason: Option<String>,
    #[serde(default)]
    pub timeout_seconds: BTreeMap<String, u64>,
    #[serde(default)]
    pub cpu_timeout_seconds: BTreeMap<String, u64>,
    #[serde(default)]
    pub slow_reason: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Assertions {
    pub bitwise_parity: Option<bool>,
    pub min_distinct: Option<u64>,
    pub min_passes: Option<u64>,
    pub min_failures: Option<u64>,
    pub min_normalized_entropy: Option<f64>,
    pub runs: Option<u64>,
    pub repeat_identical: Option<bool>,
}

#[derive(Clone, Debug)]
pub struct ManifestSet {
    pub documents: Vec<ManifestDocument>,
    tests: BTreeMap<String, (String, u64, u64, TestRecipe)>,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct CellId {
    pub test: String,
    pub mode: String,
    pub backend: Option<String>,
}

const SCHEDULED_JOBS_ENV: &str = "HERMIT_E2E_SCHEDULED_JOBS";
const ISOLATED_WORKDIR_ENV: &str = "HERMIT_E2E_EMPTY_WORKDIR";
const HERMETIC_TEST_WORKDIR: &str = "/test";
const FIXED_GUEST_WORKDIR: &str = "/tmp/test";
const FIXED_WORKDIR_SOURCE_DIR: &str = "workdir";

fn fixed_workdir_source_for_attempt(cell_dir: &Path, attempt: &str) -> Result<PathBuf, String> {
    let mut components = Path::new(attempt).components();
    let is_one_normal_component = matches!(
        (components.next(), components.next()),
        (Some(Component::Normal(component)), None) if component == OsStr::new(attempt)
    );
    if !is_one_normal_component {
        return Err(format!(
            "invalid attempt label {attempt:?}: expected exactly one normal path component"
        ));
    }
    Ok(cell_dir.join(FIXED_WORKDIR_SOURCE_DIR).join(attempt))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Population {
    Required,
    Enabled,
    Disabled,
}

#[derive(Clone, Debug, Default)]
pub struct Selection {
    pub population: Option<Population>,
    pub lane: Option<String>,
    pub category: Option<String>,
    pub test: Option<String>,
    pub mode: Option<String>,
    pub backend: Option<String>,
    pub include_occasional: bool,
    pub include_manual: bool,
}

#[derive(Clone, Debug)]
pub struct SelectedCell {
    pub category: String,
    pub test: TestRecipe,
    pub id: CellId,
    pub enabled: bool,
    pub timeout_seconds: u64,
    pub cpu_timeout_seconds: u64,
}

/// Named manifest bytes captured once for shared parsing and provenance.
/// Keeping these inputs together prevents a consumer from hashing one revision
/// while independently parsing another revision at the same paths.
pub(crate) struct ManifestInputs {
    sources: Vec<(PathBuf, String)>,
}

impl ManifestInputs {
    pub(crate) fn read(root: &Path) -> Result<Self, String> {
        let dir = root.join("tests/e2e/manifests");
        let defaults_path = dir.join(DEFAULTS_FILE);
        let defaults_source = fs::read_to_string(&defaults_path)
            .map_err(|e| format!("cannot read {}: {e}", defaults_path.display()))?;
        let mut paths = fs::read_dir(&dir)
            .map_err(|e| format!("cannot read {}: {e}", dir.display()))?
            .map(|entry| {
                entry
                    .map(|entry| entry.path())
                    .map_err(|e| format!("cannot read an entry in {}: {e}", dir.display()))
            })
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .filter(|path| {
                path.extension().is_some_and(|ext| ext == "yaml")
                    && path.file_name().is_some_and(|name| name != DEFAULTS_FILE)
            })
            .collect::<Vec<_>>();
        paths.sort();
        if paths.is_empty() {
            return Err(format!("no YAML manifests found in {}", dir.display()));
        }
        let mut sources = vec![(defaults_path, defaults_source)];
        for path in paths {
            let source = fs::read_to_string(&path)
                .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
            sources.push((path, source));
        }
        sources.sort_by(|left, right| left.0.cmp(&right.0));
        Ok(Self { sources })
    }

    pub(crate) fn named_sources(&self) -> impl Iterator<Item = (&Path, &str)> {
        self.sources
            .iter()
            .map(|(path, source)| (path.as_path(), source.as_str()))
    }
}

impl ManifestSet {
    pub fn load(root: &Path) -> Result<Self, String> {
        Self::from_inputs(root, &ManifestInputs::read(root)?)
    }

    pub(crate) fn from_inputs(root: &Path, inputs: &ManifestInputs) -> Result<Self, String> {
        let (defaults_path, defaults_source) = inputs
            .named_sources()
            .find(|(path, _)| path.file_name().is_some_and(|name| name == DEFAULTS_FILE))
            .ok_or_else(|| "captured manifest inputs lack defaults.yaml".to_string())?;
        let defaults: ManifestDefaults = serde_yaml::from_str(defaults_source)
            .map_err(|e| format!("{}: invalid YAML: {e}", defaults_path.display()))?;
        if defaults.schema != MANIFEST_SCHEMA {
            return Err(format!(
                "{}: schema must be {MANIFEST_SCHEMA}",
                defaults_path.display()
            ));
        }
        let global_timeout_seconds =
            validate_timeout_seconds(defaults.timeout_seconds, "global default")?;
        let global_cpu_timeout_seconds =
            validate_timeout_seconds(defaults.cpu_timeout_seconds, "global CPU default")?;
        let mut nextest_filters = BTreeSet::new();
        for timeout in &defaults.nextest {
            if timeout.filter.trim().is_empty() {
                return Err("nextest timeout filter must not be empty".into());
            }
            validate_timeout_seconds(timeout.timeout_seconds, &timeout.filter)?;
            if timeout.timeout_seconds == global_timeout_seconds {
                return Err(format!(
                    "nextest timeout {} redundantly repeats the global default",
                    timeout.filter
                ));
            }
            if timeout.slow_reason.trim().is_empty() {
                return Err(format!(
                    "nextest timeout {} requires a non-empty slow_reason",
                    timeout.filter
                ));
            }
            if !nextest_filters.insert(timeout.filter.as_str()) {
                return Err(format!(
                    "duplicate nextest timeout filter: {}",
                    timeout.filter
                ));
            }
        }
        let mut documents = Vec::new();
        let mut tests = BTreeMap::new();
        for (path, source) in inputs
            .named_sources()
            .filter(|(path, _)| path.file_name().is_some_and(|name| name != DEFAULTS_FILE))
        {
            let document: ManifestDocument = serde_yaml::from_str(source)
                .map_err(|e| format!("{}: invalid YAML: {e}", path.display()))?;
            let stem = path.file_stem().and_then(OsStr::to_str).unwrap_or_default();
            validate_document_with_cpu(
                &document,
                stem,
                root,
                global_timeout_seconds,
                global_cpu_timeout_seconds,
            )?;
            let bucket_timeout_seconds =
                resolve_timeout_seconds(global_timeout_seconds, document.timeout_seconds, None);
            for test in &document.test {
                if tests
                    .insert(
                        test.id.clone(),
                        (
                            document.bucket.clone(),
                            bucket_timeout_seconds,
                            global_cpu_timeout_seconds,
                            test.clone(),
                        ),
                    )
                    .is_some()
                {
                    return Err(format!("duplicate test id: {}", test.id));
                }
            }
            documents.push(document);
        }
        Ok(Self { documents, tests })
    }

    /// Is `id` a test this manifest set has ever heard of?
    ///
    /// ⚠️ THIS EXISTS TO SEPARATE "NO CELLS" FROM "NO SUCH TEST". `select` returns an
    /// empty vector for both, and they mean opposite things: an unfiltered population
    /// that is legitimately empty ("no gaps") versus a filter naming something that
    /// does not exist (a typo). A caller that cannot tell them apart must either
    /// refuse a good answer or accept a meaningless one.
    pub fn knows_test(&self, id: &str) -> bool {
        self.tests.contains_key(id)
    }

    pub fn select(&self, selection: &Selection) -> Result<Vec<SelectedCell>, String> {
        let population = selection.population.unwrap_or(Population::Enabled);
        let mut cells = Vec::new();
        for (id, (category, bucket_timeout_seconds, default_cpu_timeout_seconds, test)) in
            &self.tests
        {
            if selection
                .lane
                .as_deref()
                .is_some_and(|lane| lane != test.lane)
                || selection
                    .category
                    .as_deref()
                    .is_some_and(|value| value != category)
                || selection.test.as_deref().is_some_and(|value| value != id)
                || (!selection.include_occasional && test.occasional)
            {
                continue;
            }
            for (mode, recipe) in &test.modes {
                if selection.mode.as_deref().is_some_and(|value| value != mode) {
                    continue;
                }
                if mode == "naked" {
                    let enabled = recipe
                        .backends_enabled
                        .iter()
                        .any(|value| value == "native");
                    let ci = ci_selection(recipe)?.selected("native");
                    let accepted = match population {
                        Population::Required => {
                            enabled
                                && (ci
                                    || selection.mode.as_deref() == Some("naked")
                                    || selection.include_manual)
                        }
                        other => population_accepts(other, ci, enabled),
                    };
                    if selection
                        .backend
                        .as_deref()
                        .is_some_and(|backend| backend != "native")
                        || !accepted
                    {
                        continue;
                    }
                    cells.push(SelectedCell {
                        category: category.clone(),
                        test: test.clone(),
                        id: CellId {
                            test: id.clone(),
                            mode: mode.clone(),
                            backend: None,
                        },
                        enabled,
                        timeout_seconds: resolve_timeout_seconds(
                            *bucket_timeout_seconds,
                            None,
                            recipe.timeout_seconds.get("native").copied(),
                        ),
                        cpu_timeout_seconds: recipe
                            .cpu_timeout_seconds
                            .get("native")
                            .copied()
                            .unwrap_or(*default_cpu_timeout_seconds),
                    });
                    continue;
                }
                let backends: Vec<_> = match population {
                    Population::Disabled => recipe.backends_disabled.keys().cloned().collect(),
                    _ => recipe.backends_enabled.clone(),
                };
                let ci = ci_selection(recipe)?;
                for backend in backends {
                    if selection
                        .backend
                        .as_deref()
                        .is_some_and(|value| value != backend)
                    {
                        continue;
                    }
                    let enabled = recipe.backends_enabled.contains(&backend);
                    if population == Population::Required
                        && !selection.include_manual
                        && !ci.selected(&backend)
                    {
                        continue;
                    }
                    if !population_accepts(population, ci.selected(&backend), enabled) {
                        continue;
                    }
                    let timeout_seconds = resolve_timeout_seconds(
                        *bucket_timeout_seconds,
                        None,
                        recipe.timeout_seconds.get(&backend).copied(),
                    );
                    let cpu_timeout_seconds = recipe
                        .cpu_timeout_seconds
                        .get(&backend)
                        .copied()
                        .unwrap_or(*default_cpu_timeout_seconds);
                    cells.push(SelectedCell {
                        category: category.clone(),
                        test: test.clone(),
                        id: CellId {
                            test: id.clone(),
                            mode: mode.clone(),
                            backend: Some(backend),
                        },
                        enabled,
                        timeout_seconds,
                        cpu_timeout_seconds,
                    });
                }
            }
        }
        cells.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(cells)
    }

    pub fn all_tests(&self) -> impl Iterator<Item = (&str, u64, u64, &TestRecipe)> {
        self.tests
            .values()
            .map(|(category, timeout_seconds, cpu_timeout_seconds, test)| {
                (
                    category.as_str(),
                    *timeout_seconds,
                    *cpu_timeout_seconds,
                    test,
                )
            })
    }
}

fn population_accepts(population: Population, ci: bool, enabled: bool) -> bool {
    match population {
        Population::Required => ci && enabled,
        Population::Enabled => enabled,
        Population::Disabled => !enabled,
    }
}

#[cfg(test)]
fn validate_document(
    document: &ManifestDocument,
    stem: &str,
    root: &Path,
    global_timeout_seconds: u64,
) -> Result<(), String> {
    validate_document_with_cpu(
        document,
        stem,
        root,
        global_timeout_seconds,
        DEFAULT_TEST_CPU_TIMEOUT_SECONDS,
    )
}

fn validate_document_with_cpu(
    document: &ManifestDocument,
    stem: &str,
    root: &Path,
    global_timeout_seconds: u64,
    global_cpu_timeout_seconds: u64,
) -> Result<(), String> {
    if document.schema != MANIFEST_SCHEMA {
        return Err(format!("{stem}: schema must be {MANIFEST_SCHEMA}"));
    }
    if document.bucket != stem {
        return Err(format!(
            "{stem}: bucket `{}` must equal file stem",
            document.bucket
        ));
    }
    if document.test.is_empty() {
        return Err(format!("{stem}: test list must not be empty"));
    }
    match (document.timeout_seconds, document.slow_reason.as_deref()) {
        (Some(timeout), Some(reason)) if !reason.trim().is_empty() => {
            validate_timeout_seconds(timeout, &format!("{stem} bucket"))?;
            if timeout == global_timeout_seconds {
                return Err(format!(
                    "{stem}: bucket timeout_seconds redundantly repeats the global default"
                ));
            }
        }
        (Some(_), _) => {
            return Err(format!(
                "{stem}: bucket timeout_seconds requires a non-empty slow_reason"
            ));
        }
        (None, Some(_)) => {
            return Err(format!("{stem}: bucket slow_reason has no timeout_seconds"));
        }
        (None, None) => {}
    }
    let bucket_timeout_seconds =
        resolve_timeout_seconds(global_timeout_seconds, document.timeout_seconds, None);
    for test in &document.test {
        if !test.id.starts_with(&format!("{stem}/")) {
            return Err(format!("{}: id must start with {stem}/", test.id));
        }
        if !matches!(test.lane.as_str(), "portable" | "privileged") {
            return Err(format!("{}: invalid lane `{}`", test.id, test.lane));
        }
        for token in &test.requires {
            requires_capability(token).map_err(|error| format!("{}.requires: {error}", test.id))?;
        }
        match (&test.program, &test.direct) {
            (Some(program), None) => {
                if !program.starts_with("tests/") || !root.join(program).exists() {
                    return Err(format!("{}: missing program {program}", test.id));
                }
            }
            (None, Some(DirectCommand::Shell(command))) if !command.trim().is_empty() => {}
            (None, Some(DirectCommand::Argv(argv))) if !argv.is_empty() => {}
            (Some(_), Some(_)) => return Err(format!("{}: set only program or direct", test.id)),
            _ => return Err(format!("{}: missing executable program/direct", test.id)),
        }
        let actual: BTreeSet<_> = test.modes.keys().map(String::as_str).collect();
        let expected: BTreeSet<_> = MODES.into_iter().collect();
        if actual != expected {
            return Err(format!("{}: modes must be exactly {expected:?}", test.id));
        }
        for (mode, recipe) in &test.modes {
            validate_mode_with_cpu(
                &test.id,
                mode,
                recipe,
                bucket_timeout_seconds,
                global_cpu_timeout_seconds,
            )?;
        }
    }
    Ok(())
}

#[cfg(test)]
fn validate_mode(
    id: &str,
    mode: &str,
    recipe: &ModeRecipe,
    bucket_timeout_seconds: u64,
) -> Result<(), String> {
    validate_mode_with_cpu(
        id,
        mode,
        recipe,
        bucket_timeout_seconds,
        DEFAULT_TEST_CPU_TIMEOUT_SECONDS,
    )
}

fn validate_mode_with_cpu(
    id: &str,
    mode: &str,
    recipe: &ModeRecipe,
    bucket_timeout_seconds: u64,
    default_cpu_timeout_seconds: u64,
) -> Result<(), String> {
    validate_mode_workdir(
        id,
        mode,
        recipe.workdir.as_deref(),
        &recipe.backends_enabled,
    )?;
    let expected: BTreeSet<&str> = if mode == "naked" {
        ["native"].into_iter().collect()
    } else {
        BACKENDS.into_iter().collect()
    };
    let enabled: BTreeSet<_> = recipe.backends_enabled.iter().map(String::as_str).collect();
    let disabled: BTreeSet<_> = recipe
        .backends_disabled
        .keys()
        .map(String::as_str)
        .collect();
    if enabled.len() != recipe.backends_enabled.len()
        || !enabled.is_disjoint(&disabled)
        || enabled.union(&disabled).copied().collect::<BTreeSet<_>>() != expected
    {
        return Err(format!("{id}: {mode} does not partition {expected:?}"));
    }
    if recipe
        .backends_disabled
        .values()
        .any(|reason| reason.trim().is_empty())
    {
        return Err(format!("{id}: {mode} has an empty backend-disabled reason"));
    }
    let wall_timeout_keys: BTreeSet<_> =
        recipe.timeout_seconds.keys().map(String::as_str).collect();
    let cpu_timeout_keys: BTreeSet<_> = recipe
        .cpu_timeout_seconds
        .keys()
        .map(String::as_str)
        .collect();
    let reason_keys: BTreeSet<_> = recipe.slow_reason.keys().map(String::as_str).collect();
    if wall_timeout_keys != cpu_timeout_keys || wall_timeout_keys != reason_keys {
        return Err(format!(
            "{id}: {mode} timeout_seconds, cpu_timeout_seconds, and slow_reason must name the same backends"
        ));
    }
    for (backend, timeout) in &recipe.timeout_seconds {
        if !enabled.contains(backend.as_str()) {
            return Err(format!(
                "{id}: {mode} timeout_seconds names disabled backend {backend}"
            ));
        }
        validate_timeout_seconds(*timeout, &format!("{id}: {mode}/{backend}"))?;
        let reason = recipe.slow_reason.get(backend).ok_or_else(|| {
            format!("{id}: {mode}/{backend} timeout_seconds requires slow_reason")
        })?;
        if reason.trim().is_empty() {
            return Err(format!(
                "{id}: {mode}/{backend} slow_reason must be non-empty"
            ));
        }
    }
    for (backend, timeout) in &recipe.cpu_timeout_seconds {
        if !enabled.contains(backend.as_str()) {
            return Err(format!(
                "{id}: {mode} cpu_timeout_seconds names disabled backend {backend}"
            ));
        }
        validate_timeout_seconds(*timeout, &format!("{id}: {mode}/{backend} CPU"))?;
        let reason = recipe.slow_reason.get(backend).ok_or_else(|| {
            format!("{id}: {mode}/{backend} cpu_timeout_seconds requires slow_reason")
        })?;
        if reason.trim().is_empty() {
            return Err(format!(
                "{id}: {mode}/{backend} slow_reason must be non-empty"
            ));
        }
    }
    for backend in &wall_timeout_keys {
        if recipe.timeout_seconds[*backend] == bucket_timeout_seconds
            && recipe.cpu_timeout_seconds[*backend] == default_cpu_timeout_seconds
        {
            return Err(format!(
                "{id}: {mode}/{backend} timeout pair redundantly repeats its inherited values"
            ));
        }
    }
    let ci = CiSelection::validate(
        &enabled
            .iter()
            .map(|backend| (*backend).to_string())
            .collect(),
        &disabled
            .iter()
            .map(|backend| (*backend).to_string())
            .collect(),
        &recipe.ci,
        recipe.ci_disabled_reason.as_ref(),
    )
    .map_err(|error| format!("{id}: {mode} {error}"))?;
    if mode == "naked" && ci.any_selected() {
        return Err(format!(
            "{id}: naked is opt-in meta-CI and must set ci=false"
        ));
    }
    for (backend, args) in &recipe.guest_args {
        if !enabled.contains(backend.as_str()) && !disabled.contains(backend.as_str()) {
            return Err(format!(
                "{id}: {mode} guest_args names backend {backend} outside backends_enabled/backends_disabled"
            ));
        }
        if args.iter().any(|argument| argument.contains('\0')) {
            return Err(format!(
                "{id}: {mode} guest_args.{backend} contains a NUL byte, which Linux argv cannot represent"
            ));
        }
    }
    match (
        mode,
        recipe.compare_io_buffers,
        recipe.compare_io_buffers_disabled_reason.as_deref(),
    ) {
        ("verify", None | Some(true), None) | ("verify", Some(false), Some(_)) => {}
        ("verify", Some(false), None) => {
            return Err(format!(
                "{id}: verify compare_io_buffers=false requires compare_io_buffers_disabled_reason"
            ));
        }
        ("verify", None | Some(true), Some(_)) => {
            return Err(format!(
                "{id}: verify comparison reason is stale while I/O-buffer comparison is enabled"
            ));
        }
        (_, None, None) => {}
        _ => {
            return Err(format!(
                "{id}: compare_io_buffers is supported only by verify mode"
            ));
        }
    }
    if recipe
        .compare_io_buffers_disabled_reason
        .as_deref()
        .is_some_and(|reason| reason.trim().is_empty())
    {
        return Err(format!(
            "{id}: compare_io_buffers_disabled_reason must be substantive"
        ));
    }
    match (
        mode,
        recipe.rcb_time,
        recipe.rcb_time_disabled_reason.as_deref(),
    ) {
        ("verify", None | Some(true), None) | ("verify", Some(false), Some(_)) => {}
        ("verify", Some(false), None) => {
            return Err(format!(
                "{id}: verify rcb_time=false requires rcb_time_disabled_reason"
            ));
        }
        ("verify", None | Some(true), Some(_)) => {
            return Err(format!(
                "{id}: verify RCB-time reason is stale while RCB time is enabled"
            ));
        }
        (_, None, None) => {}
        _ => {
            return Err(format!("{id}: rcb_time is supported only by verify mode"));
        }
    }
    if recipe
        .rcb_time_disabled_reason
        .as_deref()
        .is_some_and(|reason| reason.trim().is_empty())
    {
        return Err(format!(
            "{id}: rcb_time_disabled_reason must be substantive"
        ));
    }
    Ok(())
}

pub fn validate_mode_workdir(
    id: &str,
    mode: &str,
    workdir: Option<&str>,
    _backends_enabled: &[String],
) -> Result<(), String> {
    let Some(workdir) = workdir else {
        return Ok(());
    };
    if !matches!(mode, "verify" | "chaos" | "custom") {
        return Err(format!(
            "{id}: {mode} workdir is supported only by Hermit run modes"
        ));
    }
    if !Path::new(workdir).is_absolute() {
        return Err(format!("{id}: {mode} workdir must be an absolute path"));
    }
    Ok(())
}

fn supports_test_workdir(mode: &str, _backend: &str) -> bool {
    matches!(mode, "verify" | "replay" | "chaos" | "custom")
}

fn cell_relaxations(cell: &SelectedCell) -> Vec<String> {
    let recipe = &cell.test.modes[&cell.id.mode];
    let mut relaxations = Vec::new();
    if recipe.compare_io_buffers == Some(false) {
        relaxations.push(format!(
            "--no-detlog-io-buffers: {}",
            recipe
                .compare_io_buffers_disabled_reason
                .as_deref()
                .unwrap_or("reason missing")
        ));
    }
    if recipe.rcb_time == Some(false) {
        relaxations.push(format!(
            "--no-rcb-time: {}",
            recipe
                .rcb_time_disabled_reason
                .as_deref()
                .unwrap_or("reason missing")
        ));
    }
    relaxations
}

fn ci_selection(recipe: &ModeRecipe) -> Result<CiSelection, String> {
    CiSelection::validate(
        &recipe.backends_enabled.iter().cloned().collect(),
        &recipe.backends_disabled.keys().cloned().collect(),
        &recipe.ci,
        recipe.ci_disabled_reason.as_ref(),
    )
}

#[derive(Clone, Debug, Serialize)]
pub struct CellRunSpec {
    pub id: CellId,
    pub lane: String,
    pub category: String,
    pub cwd: PathBuf,
    pub env: BTreeMap<String, String>,
    pub argv: Vec<String>,
    pub guest_argv: Vec<String>,
    pub timeout_seconds: u64,
    pub verdict_path: Option<PathBuf>,
    pub verification_log_dir: Option<PathBuf>,
    pub sabre_path_evidence: Option<PathBuf>,
    pub cell_dir: PathBuf,
    #[serde(skip)]
    attempt: String,
    #[serde(skip)]
    fixed_workdir_source: PathBuf,
}

/// The existing pressure-test result vocabulary for one executed cell.
///
/// This is separate from [`FailureClass`]: two product failures can have
/// different results (`determinism-failure` and `crash-error`), while both are
/// still product failures. Keeping both values on the framework-written row
/// preserves that per-attempt distinction without asking pressure-test to
/// reconstruct it from process status and retained report files.
#[derive(
    Clone,
    Copy,
    Debug,
    Deserialize,
    Eq,
    Ord,
    PartialEq,
    PartialOrd,
    Serialize
)]
#[serde(rename_all = "kebab-case")]
pub enum ObservedResult {
    Pass,
    DeterminismFailure,
    ParityFailure,
    ReplayFailure,
    CrashError,
    Timeout,
    Oom,
    SandboxDenied,
    InfrastructureError,
}

impl ObservedResult {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "pass" => Ok(Self::Pass),
            "determinism-failure" => Ok(Self::DeterminismFailure),
            "parity-failure" => Ok(Self::ParityFailure),
            "replay-failure" => Ok(Self::ReplayFailure),
            "crash-error" => Ok(Self::CrashError),
            "timeout" => Ok(Self::Timeout),
            "oom" => Ok(Self::Oom),
            "sandbox-denied" => Err(
                "pressure summary contains a sandbox-denied operation; refusing to store it as product behavior"
                    .into(),
            ),
            "infrastructure-error" => Err(
                "pressure summary contains an infrastructure error; refusing to store it as product behavior"
                    .into(),
            ),
            other => Err(format!("unknown pressure result `{other}`")),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::DeterminismFailure => "determinism-failure",
            Self::ParityFailure => "parity-failure",
            Self::ReplayFailure => "replay-failure",
            Self::CrashError => "crash-error",
            Self::Timeout => "timeout",
            Self::Oom => "oom",
            Self::SandboxDenied => "sandbox-denied",
            Self::InfrastructureError => "infrastructure-error",
        }
    }

    pub fn carries_divergence_position(self) -> bool {
        matches!(
            self,
            Self::DeterminismFailure | Self::ParityFailure | Self::ReplayFailure
        )
    }

    pub fn failure_class(self) -> Option<FailureClass> {
        match self {
            Self::Pass => None,
            Self::DeterminismFailure
            | Self::ParityFailure
            | Self::ReplayFailure
            | Self::CrashError => Some(FailureClass::ProductFailure),
            Self::Timeout | Self::Oom => Some(FailureClass::NoResult),
            Self::SandboxDenied | Self::InfrastructureError => {
                Some(FailureClass::UnderstoodInfrastructureFailure)
            }
        }
    }
}

/// Attribution for a non-passing cell result.
///
/// These are the owner's four existing failure classes. `None` is reserved for
/// a pass or a retained row written before this field existed; every current
/// non-pass written by the framework carries one of these values.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureClass {
    ProductFailure,
    UnderstoodInfrastructureFailure,
    UnderstoodPrerequisiteFailure,
    NoResult,
}

impl FailureClass {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ProductFailure => "product_failure",
            Self::UnderstoodInfrastructureFailure => "understood_infrastructure_failure",
            Self::UnderstoodPrerequisiteFailure => "understood_prerequisite_failure",
            Self::NoResult => "no_result",
        }
    }

    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "product_failure" => Ok(Self::ProductFailure),
            "understood_infrastructure_failure" => Ok(Self::UnderstoodInfrastructureFailure),
            "understood_prerequisite_failure" => Ok(Self::UnderstoodPrerequisiteFailure),
            "no_result" => Ok(Self::NoResult),
            other => Err(format!("unknown failure_class `{other}`")),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AttemptResult {
    pub index: String,
    pub outcome: String,
    pub error_kind: Option<String>,
    pub status: Option<i32>,
    pub signal: Option<i32>,
    pub timed_out: bool,
    #[serde(default)]
    pub duration_ms: u128,
    /// CPU consumed by the launched process group.
    ///
    /// Completed commands use `wait4`; a CPU timeout retains the last live
    /// process-group observation when that is larger. It is not inferred from
    /// wall time and remains attributable when cells execute concurrently or
    /// move their work outside the enclosing DAG cgroup.
    #[serde(default)]
    pub cpu_usage_usec: Option<u64>,
    pub observation_sha256: Option<String>,
    pub argv: Vec<String>,
    pub guest_argv: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub cwd: String,
    pub shell_command: String,
    #[serde(default)]
    pub stdout: String,
    #[serde(default)]
    pub stderr: String,
    pub verification_report: Option<String>,
    pub verification_report_sha256: Option<String>,
    /// Runtime totals for the two executions compared by this attempt.
    pub runtime: Option<VerificationRuntime>,
    /// The divergence position, LIFTED OUT of `verification_report` so it is a
    /// field rather than a substring.
    ///
    /// `verification_report` already contained these numbers, but as a
    /// JSON-ENCODED STRING, so every consumer had to parse the row and then
    /// parse a string inside it. That is why no cell has ever reported where
    /// its divergence began: the value was present and unreachable. One
    /// attempt is one observation, so this is the level the position belongs
    /// at -- a cell with three attempts has three of them.
    pub first_divergent_scheduler_turn: Option<u64>,
    /// Companion to the field above, in virtual-nanosecond units.
    pub first_divergent_virtual_nanoseconds: Option<u64>,
    /// The coordinate that LOCATES the divergence rather than bounding it.
    pub first_divergent_record: Option<u64>,
    /// Syscalls the guest completed before diverging. A different keyspace from
    /// every other coordinate here; see the note in canonical_verdict.
    pub first_divergent_syscall: Option<u64>,
    /// First differing compared message from the left execution, with only the
    /// separately recorded syscall number, scheduler turn, and committed time
    /// removed.
    pub first_divergent_left_message: Option<String>,
    /// Corresponding first differing compared message from the right execution.
    pub first_divergent_right_message: Option<String>,
    pub sabre_path_evidence: Option<String>,
    pub sabre_path_evidence_sha256: Option<String>,
    pub reason: Option<String>,
}

/// One test-harness cell observation written to `results.jsonl`.
///
/// This is not [`crate::ledger::CellResult`]. That type is the validation
/// ledger's compact cell verdict; this type is the test framework's complete
/// execution result, including invocation, attempts, timing, and retained
/// evidence.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CellResult {
    pub schema: u64,
    pub run_id: String,
    /// Short machine name measured with this row. A hostname alone is not a
    /// stable host class, but it is the existing per-machine key used by the
    /// parent store.
    #[serde(default)]
    pub machine_shortname: String,
    /// `uname -r` for the kernel under which this cell ran.
    #[serde(default)]
    pub kernel_version: String,
    /// The complete closed set of capability verdicts that determined which
    /// cells could execute on this host.
    #[serde(default)]
    pub host_capabilities: HostCapabilities,
    /// The cell attempt that produced this observation. A retry is a second
    /// observation, so it receives the next positive ordinal instead of
    /// replacing the earlier row.
    #[serde(default = "first_attempt")]
    pub attempt: u64,
    /// Pressure-test repetition number supplied to the test framework.
    /// Ordinary validate rows leave this absent and use `attempt` as their
    /// series run index.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_index: Option<u64>,
    /// HEAD of the CHECKOUT the harness ran in. NOT the provenance of the
    /// binary that produced this measurement, despite the name.
    ///
    /// The harness cannot learn a binary's provenance from a path, and
    /// `hermit_bin` defaults to `target/debug/hermit` -- whatever file happens
    /// to be there. During any rebase-and-rerun loop the checkout moves and the
    /// binary does not, so this names a commit that never built the artifact.
    /// Read [`CellResult::binary_build_sha`] for that; read this only as "where
    /// the harness was standing".
    pub hermit_sha: String,
    /// Dirtiness of that same CHECKOUT at run time -- again not a statement
    /// about the binary, which may have been built from a different tree in a
    /// different state.
    pub source_tree_dirty: bool,
    pub binary_sha256: Option<String>,
    /// What the BINARY says about its own origin: the commit it was compiled
    /// from, with a `-dirty` suffix when that tree was dirty.
    ///
    /// `binary_sha256` proves WHICH file ran; this says WHERE THAT FILE CAME
    /// FROM. Together they are a complete attribution, and neither substitutes
    /// for the other. `None` means the binary could not be asked or did not
    /// answer recognisably -- deliberately distinct from a value, so "not
    /// established" never reads as agreement.
    pub binary_build_sha: Option<String>,
    #[serde(default)]
    pub test_sha256: String,
    pub test: String,
    pub category: String,
    pub lane: String,
    pub mode: String,
    pub backend: Option<String>,
    pub classification: String,
    pub outcome: String,
    /// What this cell attempt observed, using the same closed vocabulary that
    /// pressure summaries and the scorecard consume. `None` means either an
    /// infrastructure result or a retained row written before this field.
    #[serde(default)]
    pub result: Option<ObservedResult>,
    /// Who the non-pass is attributed to. This is deliberately separate from
    /// `error_kind`, which retains the more specific mechanism such as
    /// `backend-unavailable` or `incomplete-verification-evidence`.
    #[serde(default)]
    pub failure_class: Option<FailureClass>,
    pub error_kind: Option<String>,
    /// The cell wall-clock bound recorded by schema 4. It remains the fixture
    /// preparation and inner-invocation wall bound; the additive fields below
    /// describe the post-preparation policy without changing this field's
    /// meaning for existing readers.
    #[serde(default)]
    pub timeout_seconds: u64,
    /// Aggregate post-preparation CPU budget across attempts and seeds.
    /// Absent only on rows written before this policy became explicit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_cpu_timeout_seconds: Option<u64>,
    /// Outer post-preparation wall backstop for a near-zero-CPU wedge.
    /// Absent only on rows written before this policy became explicit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_wall_timeout_seconds: Option<u64>,
    /// Measured wall time for a cell that reached execution. Absent when the
    /// cell never ran; a measured zero remains a valid sub-millisecond result.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u128>,
    /// CPU consumed by this cell's preparation and launched attempts.
    ///
    /// Null when the cell never reached execution or the runner could not
    /// obtain a complete measurement. A measured zero remains a real value.
    #[serde(default)]
    pub cpu_usage_usec: Option<u64>,
    /// Runtime totals from the first attempt that produced them.
    pub runtime: Option<VerificationRuntime>,
    pub log_level: Option<String>,
    #[serde(default)]
    pub effective_args: Vec<String>,
    pub argv: Vec<String>,
    pub guest_argv: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub cwd: String,
    pub shell_command: String,
    #[serde(default)]
    pub relaxations: Vec<String>,
    pub execution_path: Option<JsonValue>,
    pub diversity: Option<JsonValue>,
    pub attempts: Vec<AttemptResult>,
    /// Cell-level divergence position: the FIRST attempt that located one.
    ///
    /// This mirrors `reason` exactly, which is also
    /// `attempts.iter().find_map(...)`. Two other rules were available and both
    /// are worse here: the LAST attempt would let a passing retry erase the
    /// position a failing attempt found, and a min across attempts would be a
    /// one-run aggregate that reads like the cross-run `ObservedRange`
    /// earliest/latest without having a sample behind it. The per-attempt
    /// fields are still present, so a consumer that wants a different rule can
    /// apply it rather than being stuck with this one.
    pub first_divergent_scheduler_turn: Option<u64>,
    /// Companion to the field above, selected by the same first-attempt rule.
    /// Selected INDEPENDENTLY, so a report carrying only some of the three
    /// coordinates still contributes the ones it has.
    pub first_divergent_virtual_nanoseconds: Option<u64>,
    /// The coordinate that LOCATES the divergence. Same first-attempt rule.
    pub first_divergent_record: Option<u64>,
    /// Syscalls completed before diverging. Same first-attempt rule.
    pub first_divergent_syscall: Option<u64>,
    /// First differing compared message from the first attempt that recorded
    /// message content.
    pub first_divergent_left_message: Option<String>,
    /// Corresponding first differing compared message from that same attempt.
    pub first_divergent_right_message: Option<String>,
    pub reason: Option<String>,
    /// Where this cell's retained evidence lives.
    ///
    /// Carried on the result rather than recomputed by readers: the directory
    /// is `<result_root>/runs/<run_id>/<slug>` for attempt 1 and uses an
    /// `-attempt-N` suffix for a retry. The slug is built in exactly one place
    /// (`cell_artifact_dir`). A consumer that rebuilds it from `test`, `mode`,
    /// `backend`, and `attempt` becomes a second definition that goes stale
    /// silently, and a path that points at nothing is worse than no path at all.
    pub artifact_dir: String,
}

impl CellResult {
    /// Validate the additive timeout fields while keeping earlier schema-4 rows
    /// readable. `timeout_seconds` retains its original wall-bound meaning;
    /// only rows carrying both new fields claim the execution CPU/backstop
    /// policy.
    pub fn validate_timeout_policy(&self) -> Result<(), String> {
        match (
            self.execution_cpu_timeout_seconds,
            self.execution_wall_timeout_seconds,
        ) {
            (None, None) => Ok(()),
            (Some(cpu), Some(wall)) if cpu > 0 && wall == self.timeout_seconds && wall > cpu => {
                Ok(())
            }
            (cpu, wall) => Err(format!(
                "timeout policy disagrees: timeout_seconds={} execution_cpu_timeout_seconds={cpu:?} execution_wall_timeout_seconds={wall:?}",
                self.timeout_seconds
            )),
        }
    }

    /// Require evidence emitted by the current runner rather than a readable
    /// schema-4 row written before the additive timeout fields existed.
    pub fn require_current_timeout_policy(&self) -> Result<(), String> {
        self.validate_timeout_policy()?;
        if self.execution_cpu_timeout_seconds.is_none()
            || self.execution_wall_timeout_seconds.is_none()
        {
            return Err("current result omitted explicit execution timeout bounds".into());
        }
        Ok(())
    }

    /// Check the classification fields written by the current framework.
    ///
    /// Both fields remain optional in the deserializer because schema 4 also
    /// names retained rows written before they existed. Current publication is
    /// stricter: every result is recorded, and every non-pass is attributed.
    pub fn require_current_classification(&self) -> Result<(), String> {
        match self.outcome.as_str() {
            "PASS" => {
                if self.result != Some(ObservedResult::Pass) || self.failure_class.is_some() {
                    return Err(format!(
                        "PASS result must carry result=pass and no failure_class, got result={:?} failure_class={:?}",
                        self.result, self.failure_class
                    ));
                }
            }
            "HOST-INAPPLICABLE" => {
                if self.result.is_some()
                    || self.failure_class != Some(FailureClass::UnderstoodPrerequisiteFailure)
                {
                    return Err(format!(
                        "HOST-INAPPLICABLE result must carry understood_prerequisite_failure and no observed result, got result={:?} failure_class={:?}",
                        self.result, self.failure_class
                    ));
                }
            }
            "FAIL" => {
                let failure_class = self.failure_class.ok_or_else(|| {
                    format!(
                        "{} result has no failure_class; current non-passes must be attributed",
                        self.outcome
                    )
                })?;
                let result = self.result.ok_or_else(|| {
                    "FAIL result has no observed result; the failure kind was lost".to_string()
                })?;
                let expected = result.failure_class().ok_or_else(|| {
                    format!("FAIL result cannot carry the passing result {result:?}")
                })?;
                if failure_class != expected {
                    return Err(format!(
                        "observed result {} requires failure_class {:?}, got {:?}",
                        result.as_str(),
                        expected,
                        failure_class
                    ));
                }
            }
            "ERROR" => match (self.result, self.failure_class) {
                (
                    Some(ObservedResult::SandboxDenied | ObservedResult::InfrastructureError),
                    Some(FailureClass::UnderstoodInfrastructureFailure),
                ) => Ok(()),
                (
                    None,
                    Some(
                        FailureClass::UnderstoodInfrastructureFailure
                        | FailureClass::UnderstoodPrerequisiteFailure
                        | FailureClass::NoResult,
                    ),
                )
                | (
                    Some(ObservedResult::Timeout | ObservedResult::Oom),
                    Some(FailureClass::NoResult),
                ) => {}
                other => {
                    return Err(format!(
                        "ERROR result must carry a non-product observation/classification, got {other:?}"
                    ));
                }
            },
            other => return Err(format!("unknown cell outcome {other:?}")),
        }
        Ok(())
    }

    /// Retained schema-4 rows predate these fields. Accept that exact legacy
    /// absence, but validate any row that carries either half of the contract.
    pub fn validate_recorded_classification(&self) -> Result<(), String> {
        if self.result.is_none() && self.failure_class.is_none() {
            return Ok(());
        }
        self.require_current_classification()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ScheduledWorkerCapacity(usize);

impl ScheduledWorkerCapacity {
    pub fn new(configured: usize) -> Self {
        assert!(configured > 0, "scheduled worker capacity must be positive");
        Self(configured)
    }

    pub fn configured(self) -> usize {
        self.0
    }

    pub fn workers_for(self, count: usize) -> usize {
        self.0.min(count)
    }
}

#[derive(Clone)]
pub struct RunContext {
    pub root: PathBuf,
    pub hermit_bin: PathBuf,
    pub result_root: PathBuf,
    pub build_root: PathBuf,
    pub run_id: String,
    pub machine_shortname: String,
    pub kernel_version: String,
    pub host_capabilities: HostCapabilities,
    pub attempt: u64,
    pub run_index: Option<u64>,
    pub source_sha: String,
    pub source_dirty: bool,
    /// Provenance the hermit binary reports about itself, probed once per run.
    /// See [`CellResult::binary_build_sha`].
    pub binary_build_sha: Option<String>,
    pub prebuilt: bool,
    pub keep_logs: bool,
    pub run_verify_strict: bool,
    pub record_verify_strict: bool,
    /// Per-machine scaling for individual test CPU and wall bounds. This is execution policy,
    /// not part of the canonical manifest.
    pub timeout_multipliers: TimeoutMultipliers,
    pub scheduled_worker_capacity: ScheduledWorkerCapacity,
    pub isolated_workdir: Option<PathBuf>,
}

impl RunContext {
    /// A copy of this context whose result row and retained artifacts use the
    /// given cell-attempt ordinal.
    pub fn with_attempt(&self, attempt: u64) -> Self {
        Self {
            attempt,
            ..self.clone()
        }
    }

    pub fn from_env(root: PathBuf, prebuilt: bool) -> Result<Self, String> {
        let source_sha = git(&root, &["rev-parse", "HEAD"])?;
        let source_dirty =
            !git(&root, &["status", "--porcelain", "--untracked-files=no"])?.is_empty();
        let result_root = std::env::var_os("E2E_RESULT_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|| root.join("ignored/e2e"));
        let run_id = std::env::var("E2E_RUN_ID").unwrap_or_else(|_| {
            format!(
                "local-{}-{}",
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs(),
                std::process::id()
            )
        });
        let run_index = std::env::var(E2E_RUN_INDEX_ENV)
            .ok()
            .map(|value| {
                value.parse::<u64>().map_err(|_| {
                    format!("{E2E_RUN_INDEX_ENV} must be a non-negative integer, got {value:?}")
                })
            })
            .transpose()?;
        let machine_name = match std::env::var(E2E_MACHINE_SHORTNAME_ENV) {
            Ok(value) if !value.is_empty() => value,
            _ => command_text("hostname", &["-s"])
                .or_else(|_| command_text("hostname", &[]))
                .map_err(|error| format!("cannot establish machine_shortname: {error}"))?,
        };
        let machine_shortname = machine_name
            .split('.')
            .next()
            .unwrap_or_default()
            .to_string();
        if machine_shortname.is_empty() || machine_shortname.contains('/') {
            return Err(format!(
                "machine_shortname must be one nonempty path segment, got {machine_shortname:?}"
            ));
        }
        let kernel_version = match std::env::var(E2E_KERNEL_VERSION_ENV) {
            Ok(value) if !value.trim().is_empty() => value,
            _ => command_text("uname", &["-r"])
                .map_err(|error| format!("cannot establish kernel_version: {error}"))?,
        };
        let host_capabilities = probe_host_capabilities();
        let build_root = std::env::var_os("E2E_BUILD_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|| result_root.join("build").join(&source_sha));
        let hermit_bin = std::env::var_os("HERMIT_BIN")
            .map(PathBuf::from)
            .unwrap_or_else(|| root.join("target/debug/hermit"));
        // Ask the binary where it came from, the same way this function already
        // asks it what flags it supports. `source_sha` above describes the
        // checkout; only the binary can describe the binary.
        let binary_build_sha = probe_binary_build_sha(&hermit_bin);
        // Published main still exposes the legacy `--verify-strict` spelling;
        // the canonical-only cutover removes it and makes bare `--verify`
        // canonical.  Detect the running binary rather than keying behavior to
        // a source SHA.  Whichever spelling executes is retained verbatim in
        // the result row, so this bridge cannot hide the comparison policy.
        let run_verify_strict =
            command_help_contains(&hermit_bin, &["run", "--help"], "--verify-strict");
        let isolated_workdir = match std::env::var_os(ISOLATED_WORKDIR_ENV) {
            None => None,
            Some(value) if value == HERMETIC_TEST_WORKDIR => {
                Some(PathBuf::from(HERMETIC_TEST_WORKDIR))
            }
            Some(value) => {
                return Err(format!(
                    "{ISOLATED_WORKDIR_ENV} must be {HERMETIC_TEST_WORKDIR}, got {}",
                    PathBuf::from(value).display()
                ));
            }
        };
        let record_verify_strict = command_help_contains(
            &hermit_bin,
            &["record", "start", "--help"],
            "--verify-strict",
        );
        Ok(Self {
            root,
            hermit_bin,
            result_root,
            build_root,
            run_id,
            machine_shortname,
            kernel_version,
            host_capabilities,
            attempt: 1,
            run_index,
            source_sha,
            source_dirty,
            binary_build_sha,
            prebuilt,
            keep_logs: std::env::var("E2E_KEEP_VERIFY_LOGS").as_deref() == Ok("1"),
            run_verify_strict,
            record_verify_strict,
            timeout_multipliers: timeout_multipliers_from_env()?,
            scheduled_worker_capacity: ScheduledWorkerCapacity::new(1),
            isolated_workdir,
        })
    }

    pub fn with_scheduled_worker_capacity(
        mut self,
        scheduled_worker_capacity: ScheduledWorkerCapacity,
    ) -> Self {
        self.scheduled_worker_capacity = scheduled_worker_capacity;
        self
    }
}

/// One initial execution plus one retry, scoped to one selected cell.
pub const MAX_ATTEMPTS_PER_CELL: u64 = 2;

/// Select the outcome that the framework reports for one cell's complete
/// attempt history.
///
/// A passing retry makes the cell pass, while the appended rows still retain
/// the failure it recovered from. If no attempt passes, any product failure
/// keeps the cell failed even when a later attempt ends in an infrastructure
/// error. A history containing only infrastructure errors remains an error.
/// Host-inapplicable is terminal and cannot be mixed with executed attempts.
pub fn outcome_after_retries<'a>(
    attempts: impl IntoIterator<Item = (u64, &'a str)>,
) -> Result<&'static str, String> {
    let mut expected_attempt = 1;
    let mut saw_pass = false;
    let mut saw_failure = false;
    let mut saw_error = false;
    let mut saw_host_inapplicable = false;

    for (attempt, outcome) in attempts {
        if attempt > MAX_ATTEMPTS_PER_CELL {
            return Err(format!(
                "cell result attempt {attempt} exceeds the shared maximum of {MAX_ATTEMPTS_PER_CELL}"
            ));
        }
        if attempt != expected_attempt {
            return Err(format!(
                "cell result attempt {attempt} does not follow the preceding attempts; expected {expected_attempt}"
            ));
        }
        if saw_pass || saw_host_inapplicable {
            return Err(format!(
                "cell result attempt {attempt} follows terminal outcome {}",
                if saw_pass {
                    "PASS"
                } else {
                    "HOST-INAPPLICABLE"
                }
            ));
        }
        match outcome {
            "PASS" => saw_pass = true,
            "FAIL" => saw_failure = true,
            "ERROR" => saw_error = true,
            "HOST-INAPPLICABLE" => saw_host_inapplicable = true,
            other => {
                return Err(format!(
                    "cell result attempt {attempt} has unknown outcome {other:?}"
                ));
            }
        }
        expected_attempt += 1;
    }

    if expected_attempt == 1 {
        return Err("cell result has no attempts".into());
    }
    if saw_host_inapplicable && (saw_failure || saw_error) {
        return Err("HOST-INAPPLICABLE cannot be mixed with executed cell attempts".into());
    }
    Ok(if saw_pass {
        "PASS"
    } else if saw_failure {
        "FAIL"
    } else if saw_error {
        "ERROR"
    } else {
        "HOST-INAPPLICABLE"
    })
}

/// Return the framework result row that carries the outcome selected by
/// [`outcome_after_retries`]. All attempt rows remain in `results.jsonl`; this
/// row is the one used for the cell's JUnit and summary entry.
pub fn cell_result_after_retries(results: &[CellResult]) -> Result<&CellResult, String> {
    let outcome = outcome_after_retries(
        results
            .iter()
            .map(|result| (result.attempt, result.outcome.as_str())),
    )?;
    results
        .iter()
        .rev()
        .find(|result| result.outcome == outcome)
        .ok_or_else(|| format!("cell result history selected {outcome} without a matching row"))
}

/// Return both the selected result row and the number of attempts in its
/// validated history. The selected row keeps its own ordinal and artifact;
/// the separate count is what structured summaries report.
pub fn cell_result_and_attempts_after_retries(
    results: &[CellResult],
) -> Result<(&CellResult, u64), String> {
    let selected = cell_result_after_retries(results)?;
    let attempts = results
        .last()
        .map(|result| result.attempt)
        .ok_or_else(|| "cell result has no attempts".to_string())?;
    Ok((selected, attempts))
}

fn command_text(program: &str, args: &[&str]) -> Result<String, String> {
    let output = Command::new(program)
        .args(args)
        .output()
        .map_err(|error| format!("cannot execute {program}: {error}"))?;
    if !output.status.success() {
        return Err(format!("{program} exited {}", output.status));
    }
    let value = String::from_utf8(output.stdout)
        .map_err(|error| format!("{program} output is not UTF-8: {error}"))?
        .trim()
        .to_string();
    if value.is_empty() {
        return Err(format!("{program} returned an empty value"));
    }
    Ok(value)
}

pub fn prepare_test(
    context: &RunContext,
    cell: &SelectedCell,
    dir: &Path,
) -> Result<Vec<String>, String> {
    let timeouts = cell_timeouts(context, cell)?;
    prepare_test_until(
        context,
        cell,
        dir,
        execution_deadline_after_preparation(Instant::now(), timeouts.wall_seconds)?,
        timeouts.wall_seconds,
    )
    .map(|(guest, _cpu_usage_usec)| guest)
}

fn prepare_test_until(
    context: &RunContext,
    cell: &SelectedCell,
    dir: &Path,
    deadline: Instant,
    wall_timeout_seconds: u64,
) -> Result<(Vec<String>, u64), String> {
    prepare_dirs(&context.root, dir)?;
    let mut cpu_usage_usec = 0u64;
    if context.prebuilt && cell.test.program.is_some() {
        let source = context
            .build_root
            .join(cell.test.id.replace('/', "-"))
            .join("fixtures");
        if !source.is_dir() {
            return Err(format!("prebuilt fixture is missing: {}", source.display()));
        }
        let destination = dir.join("fixtures");
        if destination.exists() {
            fs::remove_dir_all(&destination)
                .map_err(|e| format!("cannot clear {}: {e}", destination.display()))?;
        }
        copy_tree(&source, &destination)?;
    }
    let backend = cell.id.backend.as_deref().unwrap_or("native");
    let mode = cell.test.modes.get(&cell.id.mode).unwrap();
    let guest_args = mode.guest_args.get(backend).cloned().unwrap_or_default();
    let mut guest = match (&cell.test.program, &cell.test.direct) {
        (Some(program), None) if program.ends_with(".c") => {
            let output = dir.join("fixtures/program");
            if !context.prebuilt {
                let _ = fs::remove_file(&output);
                let mut args = vec!["-std=c11", "-O2", "-g", "-Wall", "-Wextra", "-Werror"]
                    .into_iter()
                    .map(str::to_string)
                    .collect::<Vec<_>>();
                args.extend(
                    cell.test
                        .build
                        .as_ref()
                        .map(|b| b.cflags.clone())
                        .unwrap_or_default(),
                );
                args.push(context.root.join(program).to_string_lossy().into_owned());
                args.push("-o".into());
                args.push(output.to_string_lossy().into_owned());
                cpu_usage_usec = cpu_usage_usec
                    .checked_add(run_preparation(
                        context,
                        dir,
                        "cc",
                        &args,
                        deadline,
                        wall_timeout_seconds,
                    )?)
                    .ok_or_else(|| "cell CPU usage overflowed u64".to_string())?;
            }
            require_executable_program(&output, &dir.join("captures"))?;
            vec![output.to_string_lossy().into_owned()]
        }
        (Some(program), None) if program.ends_with(".rs") => {
            let output = dir.join("fixtures/program");
            if !context.prebuilt {
                let _ = fs::remove_file(&output);
                let mut args = vec!["-O".to_string()];
                args.extend(
                    cell.test
                        .build
                        .as_ref()
                        .map(|b| b.rustflags.clone())
                        .unwrap_or_default(),
                );
                args.push(context.root.join(program).to_string_lossy().into_owned());
                args.push("-o".into());
                args.push(output.to_string_lossy().into_owned());
                cpu_usage_usec = cpu_usage_usec
                    .checked_add(run_preparation(
                        context,
                        dir,
                        "rustc",
                        &args,
                        deadline,
                        wall_timeout_seconds,
                    )?)
                    .ok_or_else(|| "cell CPU usage overflowed u64".to_string())?;
            }
            require_executable_program(&output, &dir.join("captures"))?;
            vec![output.to_string_lossy().into_owned()]
        }
        (Some(program), None) if program.ends_with(".sh") => {
            let path = context.root.join(program).to_string_lossy().into_owned();
            if !context.prebuilt {
                cpu_usage_usec = cpu_usage_usec
                    .checked_add(run_preparation(
                        context,
                        dir,
                        &path,
                        &["--prepare".into()],
                        deadline,
                        wall_timeout_seconds,
                    )?)
                    .ok_or_else(|| "cell CPU usage overflowed u64".to_string())?;
            }
            vec![path, "--run".into()]
        }
        (None, Some(DirectCommand::Shell(command))) => {
            let mut argv = vec!["bash".into(), "-c".into(), command.clone()];
            if !guest_args.is_empty() {
                argv.push("--".into());
            }
            argv
        }
        (None, Some(DirectCommand::Argv(argv))) => argv.clone(),
        _ => return Err(format!("{} has unsupported program kind", cell.test.id)),
    };
    guest.extend(guest_args);
    if context.isolated_workdir.is_some()
        || (backend != "dbt" && supports_test_workdir(&cell.id.mode, backend))
    {
        resolve_repo_guest_args(&context.root, &mut guest);
    }
    Ok((guest, cpu_usage_usec))
}

fn resolve_repo_guest_args(root: &Path, argv: &mut [String]) {
    for arg in argv {
        let path = Path::new(arg);
        if path.is_absolute() || arg == "." || arg == ".." {
            continue;
        }
        let resolved = root.join(path);
        let looks_like_repo_path = arg.starts_with("./") || arg.contains('/') || resolved.is_file();
        if looks_like_repo_path && resolved.exists() {
            *arg = resolved.to_string_lossy().into_owned();
        }
    }
}

/// What the preparation child actually said, stderr first.
///
/// The child's output is already captured to `prepare.stderr` / `prepare.stdout`;
/// this is the only thing that carries it back to the caller. It matters beyond
/// readability: validate classifies an environmentally-blocked gate by reading
/// the FAILING NODE's own output region and nothing else, deliberately, so that
/// an unrelated concurrent host denial cannot excuse a real red. A build step
/// that swallows its compiler's stderr therefore hides the very evidence the
/// classifier is looking for, and a host denial gets recorded as a product
/// failure. Measured 2026-08-17 in one cold run: `build.manifest_guests`
/// reported three guests as "missing or not executable" with no diagnostic while
/// the jailer had denied their `cc`/`ld`, and `build.runtime_release` in the SAME
/// run propagated its banner, was classified `bpfjailer-banner`, and was retried.
fn preparation_diagnostic(captures: &Path) -> String {
    fs::read_to_string(captures.join("prepare.stderr"))
        .ok()
        .filter(|text| !text.trim().is_empty())
        .or_else(|| {
            fs::read_to_string(captures.join("prepare.stdout"))
                .ok()
                .filter(|text| !text.trim().is_empty())
        })
        .unwrap_or_default()
}

/// Append a captured diagnostic to `message`, or say plainly that there was none.
///
/// The empty case is spelled out rather than left blank: "no output was captured"
/// is a different and much more suspicious fact than a compiler error, and a
/// reader who cannot tell them apart cannot tell a denial from a broken guest.
fn with_diagnostic(message: String, captures: &Path) -> String {
    let diagnostic = preparation_diagnostic(captures);
    if diagnostic.is_empty() {
        format!(
            "{message}\n  (no output was captured in {}; the preparation command produced none)",
            captures.display()
        )
    } else {
        format!("{message}\n{diagnostic}")
    }
}

fn remaining_cell_time(deadline: Instant) -> Duration {
    remaining_cell_time_at(deadline, Instant::now())
}

fn remaining_cell_seconds(deadline: Instant) -> u64 {
    let remaining = remaining_cell_time(deadline);
    if remaining.is_zero() {
        return 1;
    }
    remaining
        .as_secs()
        .saturating_add(u64::from(remaining.subsec_nanos() != 0))
}

fn remaining_cell_time_at(deadline: Instant, now: Instant) -> Duration {
    deadline.saturating_duration_since(now)
}

pub fn resolved_cell_timeouts(
    cell: &SelectedCell,
    timeout_multipliers: TimeoutMultipliers,
) -> Result<ResolvedTestTimeouts, String> {
    resolve_test_timeouts(
        cell.cpu_timeout_seconds,
        cell.timeout_seconds,
        timeout_multipliers,
    )
}

fn cell_timeouts(
    context: &RunContext,
    cell: &SelectedCell,
) -> Result<ResolvedTestTimeouts, String> {
    resolved_cell_timeouts(cell, context.timeout_multipliers)
}

fn execution_deadline_after_preparation(
    prepared_at: Instant,
    wall_timeout_seconds: u64,
) -> Result<Instant, String> {
    prepared_at
        .checked_add(Duration::from_secs(wall_timeout_seconds))
        .ok_or_else(|| format!("wall timeout {wall_timeout_seconds}s exceeds clock range"))
}

fn require_executable_program(path: &Path, captures: &Path) -> Result<(), String> {
    let executable = path
        .metadata()
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0);
    if executable {
        return Ok(());
    }
    Err(with_diagnostic(
        format!(
            "compiled guest is missing or not executable: {}",
            path.display()
        ),
        captures,
    ))
}

fn run_preparation(
    context: &RunContext,
    dir: &Path,
    program: &str,
    args: &[String],
    deadline: Instant,
    cell_timeout_seconds: u64,
) -> Result<u64, String> {
    let captures = dir.join("captures");
    if remaining_cell_time(deadline).is_zero() {
        return Err(with_diagnostic(
            format!("cell exceeded {cell_timeout_seconds} s during fixture preparation"),
            &captures,
        ));
    }
    let output = execute_process(
        &context.root,
        program,
        args,
        &preparation_env(dir),
        &captures.join("prepare.stdout"),
        &captures.join("prepare.stderr"),
        (deadline, None),
    )?;
    if output.timeout.is_some() || !output.status.success() {
        // Carry the child's own words back. This used to return the bare sentence
        // and drop `prepare.stderr` on the floor, which turned every denied or
        // broken compile into the same uninformative line.
        let how = if output.timeout.is_some() {
            format!("cell exceeded {cell_timeout_seconds} s during fixture preparation")
        } else {
            match output.status.code() {
                Some(code) => format!("exited {code}"),
                None => "was killed by a signal".to_string(),
            }
        };
        return Err(with_diagnostic(
            format!("fixture preparation failed for {program}: {how}"),
            &captures,
        ));
    }
    Ok(output.cpu_usage_usec)
}

pub fn build_spec(
    context: &RunContext,
    cell: &SelectedCell,
    dir: PathBuf,
    guest_argv: Vec<String>,
    attempt: &str,
    seed: Option<i64>,
    timeout_seconds: u64,
) -> Result<CellRunSpec, String> {
    // The label is used in verdict, log, evidence, and capture paths. Reject
    // traversal and multi-component values before any of those paths are
    // created. The ordinary host-bound workdir uses the same guard; the
    // hermetic tmpfs path does not remove this path-safety property.
    let fixed_workdir_source = fixed_workdir_source_for_attempt(&dir, attempt)?;
    let backend = cell.id.backend.as_deref().unwrap_or("native");
    let mode_recipe = &cell.test.modes[&cell.id.mode];
    let mut env = execution_cell_env(context, &dir, cell.id.mode != "naked");
    let sabre_path_evidence = (backend == "sabre").then(|| {
        dir.join("captures")
            .join(format!("{}-{attempt}.sabre-path.jsonl", cell.id.mode))
    });
    if let Some(path) = &sabre_path_evidence {
        fs::create_dir_all(path.parent().expect("evidence path has a parent"))
            .map_err(|e| e.to_string())?;
        fs::write(path, b"").map_err(|e| e.to_string())?;
        env.insert(
            "HERMIT_SABRE_PATH_EVIDENCE".into(),
            path.to_string_lossy().into_owned(),
        );
    }
    // The explicit hermetic path is available only where the launched backend
    // can honour a guest working directory. Replay is included there because
    // `record start` accepts the same mount and workdir arguments as `run`.
    let supports_test_workdir = supports_test_workdir(&cell.id.mode, backend);
    if context.isolated_workdir.is_some() && !supports_test_workdir {
        return Err(format!(
            "hermetic validation cannot isolate a {} cell on the {backend} backend at /test",
            cell.id.mode
        ));
    }
    let bound_workdir_source = (matches!(cell.id.mode.as_str(), "verify" | "chaos" | "custom")
        && backend != "dbt")
        .then_some(fixed_workdir_source.as_path());
    let isolated = context.isolated_workdir.is_some();
    let verdict = dir.join(format!("verify-{attempt}.json"));
    let mut verification_log_dir = None;
    let (argv, verdict_path) = match cell.id.mode.as_str() {
        "naked" => (guest_argv.clone(), None),
        "verify" => {
            let mut argv = vec![
                context.hermit_bin.to_string_lossy().into_owned(),
                "--log".into(),
                "info".into(),
                "run".into(),
                "--base-env=minimal".into(),
                "--backend".into(),
                backend.into(),
                "--strict".into(),
            ];
            if context.run_verify_strict {
                argv.push("--verify-strict".into());
            }
            if mode_recipe.compare_io_buffers == Some(false) {
                argv.push("--no-detlog-io-buffers".into());
            }
            if mode_recipe.rcb_time == Some(false) {
                argv.push("--no-rcb-time".into());
            }
            argv.extend([
                "--verify".into(),
                "--verify-json".into(),
                verdict.to_string_lossy().into_owned(),
            ]);
            if context.keep_logs {
                let logs = dir.join(format!("verify-logs/verify-{attempt}"));
                fs::create_dir_all(&logs).map_err(|e| e.to_string())?;
                argv.extend([
                    "--keep-logs".into(),
                    "--verify-log-dir".into(),
                    logs.to_string_lossy().into_owned(),
                ]);
                verification_log_dir = Some(logs);
            }
            append_execution_root_args(
                &mut argv,
                backend,
                context.isolated_workdir.as_deref(),
                mode_recipe.workdir.as_deref(),
                bound_workdir_source,
            );
            append_guest_env_args(&mut argv, &env, isolated);
            argv.push("--".into());
            argv.extend(guest_argv.clone());
            (argv, Some(verdict))
        }
        "replay" => {
            let mut argv = vec![
                context.hermit_bin.to_string_lossy().into_owned(),
                "--log".into(),
                "info".into(),
                "--backend".into(),
                backend.into(),
                "record".into(),
                "start".into(),
                "--strict".into(),
            ];
            if isolated {
                argv.push("--base-env=minimal".into());
            }
            if context.record_verify_strict {
                argv.push("--verify-strict".into());
            }
            argv.extend([
                "--verify".into(),
                "--verify-json".into(),
                verdict.to_string_lossy().into_owned(),
                "--data-dir".into(),
                dir.join("recording").to_string_lossy().into_owned(),
                "--record-timeout".into(),
                timeout_seconds.to_string(),
            ]);
            append_execution_root_args(
                &mut argv,
                backend,
                context.isolated_workdir.as_deref(),
                mode_recipe.workdir.as_deref(),
                bound_workdir_source,
            );
            append_guest_env_args(&mut argv, &env, isolated);
            argv.push("--".into());
            argv.extend(guest_argv.clone());
            (argv, Some(verdict))
        }
        "chaos" => {
            let seed = seed.ok_or_else(|| "chaos attempt requires a seed".to_string())?;
            let mut argv = vec![
                context.hermit_bin.to_string_lossy().into_owned(),
                "--log".into(),
                "info".into(),
                "run".into(),
                "--base-env=minimal".into(),
                "--backend".into(),
                backend.into(),
                "--strict".into(),
            ];
            if context.run_verify_strict {
                argv.push("--verify-strict".into());
            }
            argv.extend([
                "--verify".into(),
                "--verify-allow=both".into(),
                "--verify-json".into(),
                verdict.to_string_lossy().into_owned(),
                "--chaos".into(),
                "--sched-heuristic=random".into(),
                format!("--seed={seed}"),
            ]);
            append_execution_root_args(
                &mut argv,
                backend,
                context.isolated_workdir.as_deref(),
                mode_recipe.workdir.as_deref(),
                bound_workdir_source,
            );
            append_guest_env_args(&mut argv, &env, isolated);
            argv.push("--".into());
            argv.extend(guest_argv.clone());
            (argv, Some(verdict))
        }
        "custom" => {
            let mut argv = vec![
                context.hermit_bin.to_string_lossy().into_owned(),
                "--log".into(),
                "info".into(),
                "run".into(),
                "--backend".into(),
                backend.into(),
            ];
            argv.extend(cell.test.modes["custom"].args.clone());
            if isolated {
                require_minimal_base_env(&mut argv)?;
                append_guest_env_args(&mut argv, &env, true);
            } else {
                append_scheduled_jobs_env_arg(&mut argv, &env);
            }
            append_execution_root_args(
                &mut argv,
                backend,
                context.isolated_workdir.as_deref(),
                mode_recipe.workdir.as_deref(),
                bound_workdir_source,
            );
            argv.push("--".into());
            argv.extend(guest_argv.clone());
            (argv, None)
        }
        mode => return Err(format!("unsupported mode {mode}")),
    };
    Ok(CellRunSpec {
        id: cell.id.clone(),
        lane: cell.test.lane.clone(),
        category: cell.category.clone(),
        cwd: context.root.clone(),
        env,
        argv,
        guest_argv,
        timeout_seconds,
        verdict_path,
        verification_log_dir,
        sabre_path_evidence,
        cell_dir: dir,
        attempt: attempt.into(),
        fixed_workdir_source,
    })
}

pub fn execute_spec(spec: &CellRunSpec) -> Result<AttemptResult, String> {
    execute_spec_until(
        spec,
        execution_deadline_after_preparation(Instant::now(), spec.timeout_seconds)?,
        spec.timeout_seconds,
        spec.timeout_seconds,
        None,
    )
}

fn execute_spec_until(
    spec: &CellRunSpec,
    deadline: Instant,
    cpu_timeout_seconds: u64,
    wall_timeout_seconds: u64,
    remaining_cpu_usec: Option<u64>,
) -> Result<AttemptResult, String> {
    // The attempt label comes from the spec rather than a parallel parameter.
    // `build_spec` already stored it, and every caller passed the same value to
    // both, so a separate argument was a second definition that could go stale
    // against the workdir and capture paths built from the first.
    let index = spec.attempt.as_str();
    if spec.argv.is_empty() {
        return Err("empty cell argv".into());
    }
    if let Some(verdict) = &spec.verdict_path {
        let _ = fs::remove_file(verdict);
    }
    let tmp = spec.cell_dir.join("tmp");
    if tmp.exists() {
        fs::remove_dir_all(&tmp).map_err(|e| format!("cannot reset {}: {e}", tmp.display()))?;
    }
    fs::create_dir_all(&tmp).map_err(|e| format!("cannot create {}: {e}", tmp.display()))?;
    // The ordinary-host fallback binds this exact per-attempt directory at
    // /tmp/test. The explicit hermetic path mounts a fresh tmpfs at /test and
    // ignores the directory, but keeping the reset unconditional preserves the
    // fallback and makes switching paths a pure precedence decision.
    let workdir = &spec.fixed_workdir_source;
    if workdir.exists() {
        fs::remove_dir_all(workdir)
            .map_err(|e| format!("cannot reset {}: {e}", workdir.display()))?;
    }
    fs::create_dir_all(workdir).map_err(|e| format!("cannot create {}: {e}", workdir.display()))?;
    let captures = spec.cell_dir.join("captures");
    fs::create_dir_all(&captures).map_err(|e| e.to_string())?;
    let stdout_path = captures.join(format!("{}-{index}.stdout", spec.id.mode));
    let stderr_path = captures.join(format!("{}-{index}.stderr", spec.id.mode));
    let started = Instant::now();
    let remaining = remaining_cell_time(deadline);
    let exhausted = if remaining.is_zero() {
        Some(ProcessTimeout::Wall)
    } else if remaining_cpu_usec == Some(0) {
        Some(ProcessTimeout::Cpu)
    } else {
        None
    };
    if let Some(timeout) = exhausted {
        fs::write(&stdout_path, b"").map_err(|e| e.to_string())?;
        fs::write(&stderr_path, b"").map_err(|e| e.to_string())?;
        return Ok(cell_timeout_attempt(
            spec,
            cpu_timeout_seconds,
            wall_timeout_seconds,
            started.elapsed(),
            timeout,
        ));
    }
    let output = execute_process(
        &spec.cwd,
        &spec.argv[0],
        &spec.argv[1..],
        &spec.env,
        &stdout_path,
        &stderr_path,
        (deadline, remaining_cpu_usec),
    )?;
    if spec.id.mode == "verify" && spec.id.backend.as_deref() == Some("ptrace") {
        if let Some(directory) = &spec.verification_log_dir {
            normalize_ptrace_golden(&spec.argv[0], directory)?;
        }
    }
    let stdout = fs::read_to_string(&stdout_path).unwrap_or_default();
    let stderr = fs::read_to_string(&stderr_path).unwrap_or_default();
    let mut outcome = if output.timeout.is_some() || !output.status.success() {
        "FAIL"
    } else {
        "PASS"
    }
    .to_string();
    let mut reason = output
        .timeout
        .map(|kind| kind.reason(cpu_timeout_seconds, wall_timeout_seconds));
    let mut error_kind = None;
    let failure_class_line = stderr.lines().next().unwrap_or_default();
    let launch_refusal = spec.id.mode != "naked"
        && !output.status.success()
        && stdout.is_empty()
        && matches!(
            failure_class_line,
            "HERMIT_INTERNAL_FAILURE class=guest-program-not-found"
                | "HERMIT_INTERNAL_FAILURE class=guest-program-not-executable"
        );
    if launch_refusal {
        outcome = "ERROR".into();
        error_kind = Some("guest-launch-refused".into());
        reason = Some(format!(
            "guest launch refused before execution: {}",
            stderr
                .lines()
                .find_map(|line| line.strip_prefix("Error: "))
                .unwrap_or("unknown launch refusal")
        ));
    }
    // A runner that cannot start the requested backend and a backend that ran
    // but produced no canonical comparison are different failures. Keep both
    // visible as ERROR, but give the pre-guest availability refusal its own
    // machine-readable kind so sweeps cannot count it as a product failure.
    // The first line is the producer-owned class emitted before human prose.
    // Matching a broader nonzero exit would hide real regressions.
    // KVM currently bypasses ensure_available, so its availability failures do
    // not enter this class.
    let unavailable_class = spec.id.backend.as_deref().map(|backend| {
        format!("HERMIT_INTERNAL_FAILURE class=backend-unavailable backend={backend}")
    });
    let backend_unavailable = spec.id.mode != "naked"
        && !launch_refusal
        && output.timeout.is_none()
        && !output.status.success()
        && stdout.is_empty()
        && unavailable_class
            .as_deref()
            .is_some_and(|class| failure_class_line == class);
    if backend_unavailable {
        outcome = "ERROR".into();
        error_kind = Some("backend-unavailable".into());
        reason = Some(format!(
            "backend unavailable on this runner, so nothing was measured: {}",
            stderr
                .lines()
                .find_map(|line| line.strip_prefix("Error: "))
                .unwrap_or("unknown backend unavailability")
        ));
    }
    // A producer class that cannot satisfy the requested backend or execution
    // shape is unavailable evidence, not a product crash. In particular, the
    // human error line must never override a mismatched class into FAIL.
    let invalid_backend_evidence = output.timeout.is_none()
        && !output.status.success()
        && !backend_unavailable
        && failure_class_line
            .starts_with("HERMIT_INTERNAL_FAILURE class=backend-unavailable backend=");
    if invalid_backend_evidence {
        outcome = "ERROR".into();
        error_kind = Some("invalid-backend-evidence".into());
        reason = Some(format!(
            "backend availability evidence does not match this attempt: {failure_class_line}"
        ));
    }
    // The producer did write a class, but it did not establish a more specific
    // result. Keep that absence as no-result instead of letting the following
    // English line manufacture a product failure.
    let unclassified_internal_failure = output.timeout.is_none()
        && !output.status.success()
        && !launch_refusal
        && !backend_unavailable
        && !invalid_backend_evidence
        && failure_class_line == "HERMIT_INTERNAL_FAILURE class=cli-error";
    if unclassified_internal_failure {
        outcome = "ERROR".into();
        error_kind = Some("incomplete-verification-evidence".into());
        reason = Some("Hermit reported cli-error without a more specific result".into());
    }
    let producer_failure_classified = launch_refusal
        || backend_unavailable
        || invalid_backend_evidence
        || unclassified_internal_failure;
    let mut report_json = None;
    let mut report_sha = None;
    let mut runtime = None;
    let mut first_divergent_scheduler_turn = None;
    let mut first_divergent_virtual_nanoseconds = None;
    let mut first_divergent_record = None;
    let mut first_divergent_syscall = None;
    let mut first_divergent_left_message = None;
    let mut first_divergent_right_message = None;
    let (sabre_path_evidence, sabre_path_evidence_sha256) = spec
        .sabre_path_evidence
        .as_ref()
        .and_then(|path| fs::read(path).ok())
        .map(|bytes| {
            (
                Some(String::from_utf8_lossy(&bytes).into_owned()),
                Some(hex_digest(&bytes)),
            )
        })
        .unwrap_or((None, None));
    if let Some(path) = &spec.verdict_path {
        match fs::read(path) {
            Ok(bytes) => {
                report_sha = Some(hex_digest(&bytes));
                report_json = Some(String::from_utf8_lossy(&bytes).into_owned());
                match VerificationReport::from_json_slice(&bytes) {
                    Ok(report) => {
                        runtime = report.runtime.clone();
                        // Recorded BEFORE the classification chain below,
                        // because where the divergence began is a fact about
                        // the report rather than a consequence of how the
                        // attempt is classified -- a FAIL and an ERROR can both
                        // carry a located divergence.
                        //
                        // Guarded on `launch_refusal` for the same reason the
                        // arm below refuses to reclassify: no guest was ever
                        // created, so any report present cannot describe an
                        // executed guest's divergence position.
                        if !launch_refusal && !backend_unavailable {
                            first_divergent_scheduler_turn = report.first_divergent_scheduler_turn;
                            first_divergent_virtual_nanoseconds =
                                report.first_divergent_virtual_nanoseconds;
                            first_divergent_record = report.first_divergent_record;
                            first_divergent_syscall = report.first_divergent_syscall;
                            first_divergent_left_message =
                                report.first_divergent_left_message.clone();
                            first_divergent_right_message =
                                report.first_divergent_right_message.clone();
                        }
                        if producer_failure_classified {
                            // The producer's classified failure, or a
                            // contradiction in that evidence, cannot be
                            // superseded by a pre-stamped or unrelated report.
                        } else if report.verdict == Verdict::InfrastructureError {
                            let comparison_error = report
                                .comparison
                                .as_ref()
                                .and_then(|_| report.require_canonical_comparison().err());
                            if let Some(error) = comparison_error {
                                outcome = "ERROR".into();
                                error_kind = Some("incomplete-verification-evidence".into());
                                reason = Some(error);
                            } else {
                                outcome = "ERROR".into();
                                error_kind = Some("infrastructure".into());
                                reason = Some(match report.infrastructure_error.as_ref() {
                                    Some(InfrastructureError::SkidOvershoot { count }) => format!(
                                        "verification recorded {count} HERMIT_SKID_OVERSHOOT report(s)"
                                    ),
                                    None => unreachable!(
                                        "typed report parser requires an infrastructure error"
                                    ),
                                });
                            }
                        } else if report.verdict == Verdict::NoResult
                            && matches!(
                                report.no_result_reason,
                                Some(
                                    crate::canonical_verdict::NoResultReason::FirstRunRejected { .. }
                                ) | None
                            )
                            && output.timeout.is_none()
                            && output.status.code().is_some_and(|code| code != 0)
                        {
                            // The producer correctly has no comparison to
                            // report, but an ordinary nonzero process exit is
                            // still a completed failure rather than unknown
                            // infrastructure state.
                            outcome = "FAIL".into();
                            error_kind = None;
                            reason = Some(format!(
                                "{} exited with status {} before producing a terminal comparison",
                                spec.id.mode,
                                output.status.code().unwrap()
                            ));
                        } else if let Err(error) = report.require_canonical_comparison() {
                            outcome = "ERROR".into();
                            error_kind = Some("incomplete-verification-evidence".into());
                            reason = Some(error);
                        } else if let Err(error) = report.require_canonical_match() {
                            outcome = "FAIL".into();
                            reason = Some(error);
                        } else if output.timeout.is_none()
                            && (output.status.success() || spec.id.mode == "chaos")
                        {
                            // Chaos deliberately admits a reproduced nonzero
                            // guest class. Verify and replay still require the
                            // Hermit process itself to succeed, and no receipt
                            // may erase a timeout.
                            outcome = "PASS".into();
                            reason = None;
                        } else if reason.is_none() {
                            reason = Some(format!(
                                "{} exited with status {}",
                                spec.id.mode,
                                output.status.code().unwrap_or(128)
                            ));
                        }
                    }
                    Err(error) if !producer_failure_classified => {
                        outcome = "ERROR".into();
                        error_kind = Some("incomplete-verification-evidence".into());
                        reason = Some(format!("verification report is unreadable: {error}"));
                    }
                    Err(_) => {}
                }
            }
            Err(_error) if producer_failure_classified => {}
            Err(error) => {
                outcome = "ERROR".into();
                error_kind = Some("incomplete-verification-evidence".into());
                reason = Some(format!("verification report is missing: {error}"));
            }
        }
    }
    if let Some(timeout) = output.timeout {
        error_kind = Some(timeout.error_kind().into());
        reason = Some(timeout.reason(cpu_timeout_seconds, wall_timeout_seconds));
    }
    Ok(AttemptResult {
        index: index.into(),
        outcome,
        error_kind,
        status: output.status.code(),
        signal: std::os::unix::process::ExitStatusExt::signal(&output.status),
        timed_out: output.timeout.is_some(),
        duration_ms: started.elapsed().as_millis(),
        cpu_usage_usec: Some(output.cpu_usage_usec),
        observation_sha256: None,
        argv: spec.argv.clone(),
        guest_argv: spec.guest_argv.clone(),
        env: spec.env.clone(),
        cwd: spec.cwd.to_string_lossy().into_owned(),
        shell_command: shell_command(&spec.cwd.to_string_lossy(), &spec.env, &spec.argv),
        stdout,
        stderr,
        verification_report: report_json,
        verification_report_sha256: report_sha,
        runtime,
        first_divergent_scheduler_turn,
        first_divergent_virtual_nanoseconds,
        first_divergent_record,
        first_divergent_syscall,
        first_divergent_left_message,
        first_divergent_right_message,
        sabre_path_evidence,
        sabre_path_evidence_sha256,
        reason,
    })
}

fn normalize_ptrace_golden(hermit: &str, directory: &Path) -> Result<(), String> {
    let mut run1 = fs::read_dir(directory)
        .map_err(|e| format!("cannot read {}: {e}", directory.display()))?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(OsStr::to_str)
                .is_some_and(|name| name.starts_with("run1_log_"))
        })
        .collect::<Vec<_>>();
    run1.sort();
    let normalized = directory.join("normalized-ptrace-golden.log");
    let status_path = directory.join("normalized-ptrace-golden.status");
    let status = if run1.len() == 1 && run1[0].metadata().is_ok_and(|metadata| metadata.len() > 0) {
        let output = Command::new(hermit)
            .arg("log-diff")
            .arg(&run1[0])
            .output()
            .map_err(|e| format!("cannot normalize {}: {e}", run1[0].display()))?;
        if output.status.success() {
            fs::write(&normalized, output.stdout).map_err(|e| e.to_string())?;
        } else {
            fs::write(&normalized, b"").map_err(|e| e.to_string())?;
        }
        output.status.code().unwrap_or(1)
    } else {
        fs::write(&normalized, b"").map_err(|e| e.to_string())?;
        2
    };
    fs::write(status_path, format!("{status}\n")).map_err(|e| e.to_string())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProcessTimeout {
    Wall,
    Cpu,
}

impl ProcessTimeout {
    fn error_kind(self) -> &'static str {
        match self {
            Self::Wall => "wall-timeout",
            Self::Cpu => "cpu-timeout",
        }
    }

    fn reason(self, cpu_seconds: u64, wall_seconds: u64) -> String {
        match self {
            Self::Wall => {
                format!("cell exceeded {wall_seconds} wall s backstop ({cpu_seconds} s CPU budget)")
            }
            Self::Cpu => format!("cell exceeded {cpu_seconds} CPU s"),
        }
    }
}

struct ProcessOutput {
    status: ExitStatus,
    timeout: Option<ProcessTimeout>,
    cpu_usage_usec: u64,
}

struct ProcessLimits {
    deadline: Instant,
    cpu_budget_usec: Option<u64>,
    cpu_poll_interval: Duration,
}

/// Add two complete CPU measurements, refusing missing or overflowing input.
pub fn checked_add_cpu_usage(total: Option<u64>, usage: Option<u64>) -> Option<u64> {
    total.and_then(|total| usage.and_then(|usage| total.checked_add(usage)))
}

fn rusage_cpu_usage_usec(usage: &libc::rusage) -> Result<u64, String> {
    fn timeval_usec(value: libc::timeval) -> Option<u64> {
        let seconds = u64::try_from(value.tv_sec).ok()?;
        let microseconds = u64::try_from(value.tv_usec).ok()?;
        (microseconds < 1_000_000)
            .then_some(seconds.checked_mul(1_000_000)?.checked_add(microseconds)?)
    }

    let user = timeval_usec(usage.ru_utime)
        .ok_or_else(|| "wait4 returned an invalid user CPU duration".to_string())?;
    let system = timeval_usec(usage.ru_stime)
        .ok_or_else(|| "wait4 returned an invalid system CPU duration".to_string())?;
    user.checked_add(system)
        .ok_or_else(|| "wait4 CPU usage overflowed u64".to_string())
}

fn wait4_process(pid: u32, options: libc::c_int) -> Result<Option<(ExitStatus, u64)>, String> {
    loop {
        let mut status = 0;
        let mut usage = unsafe { std::mem::zeroed::<libc::rusage>() };
        let waited = unsafe { libc::wait4(pid as libc::pid_t, &mut status, options, &mut usage) };
        if waited == 0 {
            return Ok(None);
        }
        if waited == pid as libc::pid_t {
            return Ok(Some((
                ExitStatus::from_raw(status),
                rusage_cpu_usage_usec(&usage)?,
            )));
        }
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::Interrupted {
            continue;
        }
        return Err(format!("wait4({pid}) failed: {error}"));
    }
}

/// Live user-plus-system CPU consumed by one process group, including CPU from
/// children already reaped by a still-live member.
///
/// `wait4` is the authoritative final measurement, but it becomes available
/// only after the process exits and therefore cannot enforce a budget. Reuse
/// dagrun's process-group reader: its one short-lived snapshot is shared by all
/// concurrent cells, rather than making every cell enumerate the host's entire
/// `/proc` tree on every poll.
fn process_group_cpu_usage_usec(pgid: u32) -> Result<Option<u64>, String> {
    dagrun::proccpu::subtree_cpu_seconds(pgid)
        .map(|seconds| {
            let usec = seconds * 1_000_000.0;
            if !usec.is_finite() || usec.is_sign_negative() || usec > u64::MAX as f64 {
                return Err(format!(
                    "process group {pgid} returned invalid live CPU seconds {seconds}"
                ));
            }
            Ok(usec as u64)
        })
        .transpose()
}

fn stop_process_group(pid: u32) -> Result<(ExitStatus, u64), String> {
    unsafe {
        libc::kill(-(pid as i32), libc::SIGTERM);
    }
    let grace = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(result) = wait4_process(pid, libc::WNOHANG)? {
            return Ok(result);
        }
        if Instant::now() >= grace {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
    wait4_process(pid, 0)?.ok_or_else(|| format!("blocking wait4({pid}) returned no child"))
}

fn execute_process(
    cwd: &Path,
    program: &str,
    args: &[String],
    env: &BTreeMap<String, String>,
    stdout: &Path,
    stderr: &Path,
    limits: (Instant, Option<u64>),
) -> Result<ProcessOutput, String> {
    execute_process_with_cpu_poll_interval(
        cwd,
        program,
        args,
        env,
        stdout,
        stderr,
        ProcessLimits {
            deadline: limits.0,
            cpu_budget_usec: limits.1,
            cpu_poll_interval: CELL_CPU_POLL_INTERVAL,
        },
    )
}

fn execute_process_with_cpu_poll_interval(
    cwd: &Path,
    program: &str,
    args: &[String],
    env: &BTreeMap<String, String>,
    stdout: &Path,
    stderr: &Path,
    limits: ProcessLimits,
) -> Result<ProcessOutput, String> {
    let ProcessLimits {
        deadline,
        cpu_budget_usec,
        cpu_poll_interval,
    } = limits;
    let stdout_file = File::create(stdout).map_err(|e| format!("{}: {e}", stdout.display()))?;
    let stderr_file = File::create(stderr).map_err(|e| format!("{}: {e}", stderr.display()))?;
    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(stdout_file)
        .stderr(stderr_file);
    command.envs(env.iter());
    unsafe {
        command.pre_exec(|| {
            if libc::setpgid(0, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = command
        .spawn()
        .map_err(|e| format!("cannot execute {program}: {e}"))?;
    let pid = child.id();
    let mut next_cpu_poll = Instant::now() + cpu_poll_interval;
    let mut cpu_accounting_missing_since = None;
    loop {
        if let Some((status, cpu_usage_usec)) = wait4_process(pid, libc::WNOHANG)? {
            let timeout = cpu_budget_usec
                .filter(|limit| cpu_usage_usec >= *limit)
                .map(|_| ProcessTimeout::Cpu);
            return Ok(ProcessOutput {
                status,
                timeout,
                cpu_usage_usec,
            });
        }
        let now = Instant::now();
        let (timeout, observed_cpu_usec) = if now >= deadline {
            (Some(ProcessTimeout::Wall), None)
        } else if let Some(limit) = cpu_budget_usec.filter(|_| now >= next_cpu_poll) {
            next_cpu_poll = now + cpu_poll_interval;
            match process_group_cpu_usage_usec(pid)? {
                Some(used) => {
                    cpu_accounting_missing_since = None;
                    ((used >= limit).then_some(ProcessTimeout::Cpu), Some(used))
                }
                None => {
                    let missing_since = cpu_accounting_missing_since.get_or_insert(now);
                    if now.duration_since(*missing_since) >= CELL_CPU_ACCOUNTING_GRACE {
                        let _ = stop_process_group(pid);
                        return Err(format!(
                            "cannot measure live CPU for process group {pid}; stopped it rather than silently disabling its CPU budget"
                        ));
                    }
                    (None, None)
                }
            }
        } else {
            (None, None)
        };
        if let Some(timeout) = timeout {
            let (status, final_cpu_usage_usec) = stop_process_group(pid)?;
            let cpu_usage_usec = observed_cpu_usec.map_or(final_cpu_usage_usec, |observed| {
                observed.max(final_cpu_usage_usec)
            });
            return Ok(ProcessOutput {
                status,
                timeout: Some(timeout),
                cpu_usage_usec,
            });
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn cell_timeout_attempt(
    spec: &CellRunSpec,
    cpu_timeout_seconds: u64,
    wall_timeout_seconds: u64,
    duration: Duration,
    timeout: ProcessTimeout,
) -> AttemptResult {
    let index = spec.attempt.as_str();
    // A post-preparation bound was already exhausted before this attempt could
    // start, so no process exists from which to recover an exit status or signal.
    // Record that fact with the
    // same typed NotRun report Hermit uses before its first guest run.  A
    // synthetic exit code would make a scheduler decision look like a process
    // result, while a bare FAIL makes the retained attempt unreadable to every
    // typed consumer.
    let report = serde_json::to_string(&VerificationReport::no_result())
        .expect("the typed NotRun report must serialize");
    let report_sha256 = hex_digest(report.as_bytes());
    AttemptResult {
        index: index.into(),
        outcome: "ERROR".into(),
        error_kind: Some(timeout.error_kind().into()),
        status: None,
        signal: None,
        timed_out: true,
        duration_ms: duration.as_millis(),
        cpu_usage_usec: None,
        observation_sha256: None,
        argv: spec.argv.clone(),
        guest_argv: spec.guest_argv.clone(),
        env: spec.env.clone(),
        cwd: spec.cwd.to_string_lossy().into_owned(),
        shell_command: shell_command(&spec.cwd.to_string_lossy(), &spec.env, &spec.argv),
        stdout: String::new(),
        stderr: String::new(),
        verification_report: Some(report),
        verification_report_sha256: Some(report_sha256),
        runtime: None,
        first_divergent_scheduler_turn: None,
        first_divergent_virtual_nanoseconds: None,
        first_divergent_record: None,
        first_divergent_syscall: None,
        first_divergent_left_message: None,
        first_divergent_right_message: None,
        sabre_path_evidence: None,
        sabre_path_evidence_sha256: None,
        reason: Some(format!(
            "{} before attempt {index} started",
            timeout.reason(cpu_timeout_seconds, wall_timeout_seconds)
        )),
    }
}

/// The one definition of where a cell's retained evidence lives.
///
/// Was duplicated verbatim in `run_cell` and `infrastructure_error_result`.
/// Two copies of a path formula is how a log line ends up pointing at a
/// directory that does not exist, so surfacing the path made collapsing them a
/// prerequisite rather than a tidy-up.
fn cell_artifact_dir(context: &RunContext, cell: &SelectedCell) -> PathBuf {
    let base_slug = format!(
        "{}-{}-{}",
        cell.id.test.replace('/', "-"),
        cell.id.mode,
        cell.id.backend.as_deref().unwrap_or("none")
    );
    let slug = if context.attempt == 1 {
        base_slug
    } else {
        format!("{base_slug}-attempt-{}", context.attempt)
    };
    context
        .result_root
        .join("runs")
        .join(&context.run_id)
        .join(slug)
}

fn verification_verdict(attempt: &AttemptResult) -> Option<Verdict> {
    let report = attempt.verification_report.as_deref()?;
    VerificationReport::from_json_slice(report.as_bytes())
        .ok()
        .map(|report| report.verdict)
}

fn observed_result(
    mode: &str,
    outcome: &str,
    attempts: &[AttemptResult],
    error_kind: Option<&str>,
) -> Option<ObservedResult> {
    if outcome == "PASS" {
        return Some(ObservedResult::Pass);
    }
    if let Some(class) = attempts.iter().find_map(|attempt| {
        let output = format!("{}\n{}", attempt.stdout, attempt.stderr);
        environmental_block_observation(&output).block_class()
    }) {
        return Some(if class == EnvBlockClass::BpfjailerBanner {
            ObservedResult::SandboxDenied
        } else {
            ObservedResult::InfrastructureError
        });
    }
    // A later framework or evidence failure decides whether this cell produced
    // a usable product result. An earlier attempt can still retain a located
    // divergence in `attempts`, but it must not make a terminal infrastructure
    // or no-result outcome look product-attributed.
    if non_product_failure_class(error_kind).is_some() {
        return None;
    }
    if attempts.iter().any(|attempt| attempt.timed_out) {
        return Some(ObservedResult::Timeout);
    }
    if mode == "verify"
        && attempts
            .iter()
            .any(|attempt| verification_verdict(attempt) == Some(Verdict::Diverged))
    {
        return Some(ObservedResult::DeterminismFailure);
    }
    if mode == "replay"
        && attempts
            .iter()
            .any(|attempt| verification_verdict(attempt) == Some(Verdict::Diverged))
    {
        return Some(ObservedResult::ReplayFailure);
    }
    (outcome == "FAIL").then_some(ObservedResult::CrashError)
}

fn non_product_failure_class(error_kind: Option<&str>) -> Option<FailureClass> {
    match error_kind {
        Some("guest-launch-refused" | "backend-unavailable") => {
            Some(FailureClass::UnderstoodPrerequisiteFailure)
        }
        Some("infrastructure" | "result-publication") => {
            Some(FailureClass::UnderstoodInfrastructureFailure)
        }
        Some("incomplete-verification-evidence" | "invalid-backend-evidence") => {
            Some(FailureClass::NoResult)
        }
        _ => None,
    }
}

fn failure_class(
    outcome: &str,
    result: Option<ObservedResult>,
    error_kind: Option<&str>,
) -> Option<FailureClass> {
    if outcome == "PASS" {
        return None;
    }
    if matches!(
        result,
        Some(ObservedResult::SandboxDenied | ObservedResult::InfrastructureError)
    ) {
        return Some(FailureClass::UnderstoodInfrastructureFailure);
    }
    if let Some(failure_class) = non_product_failure_class(error_kind) {
        return Some(failure_class);
    }
    if let Some(result) = result {
        if let Some(failure_class) = result.failure_class() {
            return Some(failure_class);
        }
    }
    if outcome == "HOST-INAPPLICABLE" {
        Some(FailureClass::UnderstoodPrerequisiteFailure)
    } else {
        Some(FailureClass::NoResult)
    }
}

/// Apply a failed chaos population assertion without erasing an attempt-level
/// infrastructure result. A timeout or unavailable verification report means
/// the population was not fully measured; it is not a product failure merely
/// because the incomplete sample also missed `min_passes`.
fn apply_failed_chaos_assertion(
    outcome: &mut String,
    reason: &mut Option<String>,
    assertion_reason: String,
) {
    if outcome != "ERROR" {
        *outcome = "FAIL".into();
        *reason = Some(assertion_reason);
    }
}

pub fn run_cell(context: &RunContext, cell: &SelectedCell) -> Result<CellResult, String> {
    let dir = cell_artifact_dir(context, cell);
    let started = Instant::now();
    let timeouts = cell_timeouts(context, cell)?;
    let preparation_deadline =
        execution_deadline_after_preparation(started, timeouts.wall_seconds)?;
    let binary_before = fs::read(&context.hermit_bin)
        .ok()
        .map(|bytes| hex_digest(&bytes));
    let (guest, preparation_cpu_usage_usec) = prepare_test_until(
        context,
        cell,
        &dir,
        preparation_deadline,
        timeouts.wall_seconds,
    )?;
    // Fixture preparation keeps its scaled wall-clock guard above. Post-preparation
    // execution uses its separately measured bound as an aggregate CPU-second budget
    // across all attempts/seeds, so time spent descheduled by a busy host cannot
    // turn a valid run into no_result. The wider wall deadline is retained only
    // for a wedged process that consumes no CPU and therefore cannot reach the
    // primary bound.
    let deadline = execution_deadline_after_preparation(Instant::now(), timeouts.wall_seconds)?;
    let execution_cpu_budget_usec = timeouts.cpu_seconds.saturating_mul(1_000_000);
    let mut execution_cpu_usage_usec = 0u64;
    let mode = cell.test.modes.get(&cell.id.mode).unwrap();
    let mut attempts = Vec::new();
    match cell.id.mode.as_str() {
        "naked" => {
            for index in 1..=mode.runs.unwrap_or(3) {
                let spec = build_spec(
                    context,
                    cell,
                    dir.clone(),
                    guest.clone(),
                    &index.to_string(),
                    None,
                    remaining_cell_seconds(deadline),
                )?;
                attempts.push(execute_observed_until(
                    &spec,
                    &cell.test.observation,
                    &dir,
                    deadline,
                    timeouts.cpu_seconds,
                    timeouts.wall_seconds,
                    Some(execution_cpu_budget_usec.saturating_sub(execution_cpu_usage_usec)),
                )?);
                execution_cpu_usage_usec = execution_cpu_usage_usec
                    .checked_add(
                        attempts
                            .last()
                            .and_then(|attempt| attempt.cpu_usage_usec)
                            .unwrap_or(0),
                    )
                    .ok_or_else(|| "cell execution CPU usage overflowed u64".to_string())?;
                if attempts.last().is_some_and(|attempt| attempt.timed_out) {
                    break;
                }
            }
        }
        "chaos" => {
            let seeds = mode
                .seeds
                .as_ref()
                .ok_or_else(|| format!("{} chaos has no seeds", cell.id.test))?;
            if seeds.is_empty() {
                return Err(format!("{} chaos has no seeds", cell.id.test));
            }
            for seed in seeds {
                let index = format!("seed-{seed}");
                let spec = build_spec(
                    context,
                    cell,
                    dir.clone(),
                    guest.clone(),
                    &index,
                    Some(*seed),
                    remaining_cell_seconds(deadline),
                )?;
                attempts.push(execute_observed_until(
                    &spec,
                    &cell.test.observation,
                    &dir,
                    deadline,
                    timeouts.cpu_seconds,
                    timeouts.wall_seconds,
                    Some(execution_cpu_budget_usec.saturating_sub(execution_cpu_usage_usec)),
                )?);
                execution_cpu_usage_usec = execution_cpu_usage_usec
                    .checked_add(
                        attempts
                            .last()
                            .and_then(|attempt| attempt.cpu_usage_usec)
                            .unwrap_or(0),
                    )
                    .ok_or_else(|| "cell execution CPU usage overflowed u64".to_string())?;
                if attempts.last().is_some_and(|attempt| attempt.timed_out) {
                    break;
                }
            }
        }
        "custom" => {
            for index in 1..=mode.assert.as_ref().and_then(|a| a.runs).unwrap_or(1) {
                let spec = build_spec(
                    context,
                    cell,
                    dir.clone(),
                    guest.clone(),
                    &index.to_string(),
                    None,
                    remaining_cell_seconds(deadline),
                )?;
                attempts.push(execute_observed_until(
                    &spec,
                    &cell.test.observation,
                    &dir,
                    deadline,
                    timeouts.cpu_seconds,
                    timeouts.wall_seconds,
                    Some(execution_cpu_budget_usec.saturating_sub(execution_cpu_usage_usec)),
                )?);
                execution_cpu_usage_usec = execution_cpu_usage_usec
                    .checked_add(
                        attempts
                            .last()
                            .and_then(|attempt| attempt.cpu_usage_usec)
                            .unwrap_or(0),
                    )
                    .ok_or_else(|| "cell execution CPU usage overflowed u64".to_string())?;
                if attempts.last().is_some_and(|attempt| attempt.timed_out) {
                    break;
                }
            }
        }
        _ => {
            let spec = build_spec(
                context,
                cell,
                dir.clone(),
                guest.clone(),
                "1",
                None,
                remaining_cell_seconds(deadline),
            )?;
            attempts.push(execute_observed_until(
                &spec,
                &cell.test.observation,
                &dir,
                deadline,
                timeouts.cpu_seconds,
                timeouts.wall_seconds,
                Some(execution_cpu_budget_usec),
            )?);
        }
    }
    let hashes = attempts
        .iter()
        .filter_map(|attempt| attempt.observation_sha256.clone())
        .collect::<Vec<_>>();
    let distinct = hashes.iter().collect::<BTreeSet<_>>().len() as u64;
    let mut outcome = if attempts.iter().all(|attempt| attempt.outcome == "PASS") {
        "PASS"
    } else if attempts.iter().any(|attempt| attempt.outcome == "ERROR") {
        "ERROR"
    } else {
        "FAIL"
    }
    .to_string();
    let mut reason = attempts.iter().find_map(|attempt| attempt.reason.clone());
    let position = cell_divergence_position(&attempts);
    let first_divergent_scheduler_turn = position.scheduler_turn;
    let first_divergent_virtual_nanoseconds = position.virtual_nanoseconds;
    let first_divergent_record = position.record;
    let first_divergent_syscall = position.syscall;
    let first_divergent_left_message = position.left_message;
    let first_divergent_right_message = position.right_message;
    let mut error_kind = attempts
        .iter()
        .find_map(|attempt| attempt.error_kind.clone());
    if cell.id.mode == "naked" {
        let minimum = mode
            .assert
            .as_ref()
            .and_then(|a| a.min_distinct)
            .unwrap_or(2);
        if distinct < minimum {
            outcome = "FAIL".into();
            reason = Some(format!(
                "naked observed {distinct} distinct outcomes, need {minimum}"
            ));
        }
    }
    if cell.id.mode == "custom"
        && mode.assert.as_ref().and_then(|a| a.repeat_identical) == Some(true)
        && distinct != 1
    {
        outcome = "FAIL".into();
        reason = Some(format!("custom observed {distinct} distinct outcomes"));
    }
    if cell.id.mode == "chaos" {
        let assert = mode.assert.as_ref().cloned().unwrap_or_default();
        let pass_count = attempts
            .iter()
            .filter(|attempt| attempt.status == Some(0))
            .count() as u64;
        let failure_count = attempts.len() as u64 - pass_count;
        let diversity = diversity_evidence(
            &hashes,
            mode.outcome_classes,
            assert.min_distinct.unwrap_or(2),
            assert.min_normalized_entropy,
        );
        let normalized_entropy = diversity["normalized_entropy"].as_f64().unwrap_or(0.0);
        if distinct < assert.min_distinct.unwrap_or(2)
            || pass_count < assert.min_passes.unwrap_or(0)
            || failure_count < assert.min_failures.unwrap_or(0)
            || assert
                .min_normalized_entropy
                .is_some_and(|minimum| normalized_entropy < minimum)
        {
            apply_failed_chaos_assertion(
                &mut outcome,
                &mut reason,
                format!(
                    "chaos distinct={distinct} passes={pass_count} failures={failure_count} normalized_entropy={normalized_entropy:.4}"
                ),
            );
        }
    }
    let execution_path = match summarize_sabre_path_evidence(&attempts) {
        Ok(value) => value,
        Err(error) => {
            outcome = "ERROR".into();
            error_kind = Some("invalid-backend-evidence".into());
            reason = Some(error);
            None
        }
    };
    if outcome == "PASS"
        && execution_path
            .as_ref()
            .is_some_and(|evidence| evidence["eligible"] != true)
    {
        outcome = "FAIL".into();
        reason = Some("SaBRe execution path is incomplete or used fallback/native sites".into());
    }
    let literal_argv = attempts
        .first()
        .map(|attempt| attempt.argv.clone())
        .unwrap_or_default();
    let literal_guest_argv = attempts
        .first()
        .map(|attempt| attempt.guest_argv.clone())
        .unwrap_or_else(|| guest.clone());
    let literal_env = attempts
        .first()
        .map(|attempt| attempt.env.clone())
        .unwrap_or_else(|| execution_cell_env(context, &dir, cell.id.mode != "naked"));
    let literal_cwd = attempts
        .first()
        .map(|attempt| attempt.cwd.clone())
        .unwrap_or_else(|| context.root.to_string_lossy().into_owned());
    let literal_shell_command = attempts
        .first()
        .map(|attempt| attempt.shell_command.clone())
        .unwrap_or_default();
    let test_sha = test_digest(&context.root, &cell.test)?;
    let binary_sha = fs::read(&context.hermit_bin)
        .ok()
        .map(|bytes| hex_digest(&bytes));
    if binary_before.is_some() && binary_before != binary_sha {
        outcome = "ERROR".into();
        error_kind = Some("infrastructure".into());
        reason = Some("Hermit binary changed while the cell was executing".into());
    }
    let result = observed_result(&cell.id.mode, &outcome, &attempts, error_kind.as_deref());
    let failure_class = failure_class(&outcome, result, error_kind.as_deref());
    let cpu_usage_usec = attempts
        .iter()
        .try_fold(preparation_cpu_usage_usec, |total, attempt| {
            checked_add_cpu_usage(Some(total), attempt.cpu_usage_usec)
        });
    Ok(CellResult {
        artifact_dir: dir.display().to_string(),
        schema: CELL_RESULT_SCHEMA,
        run_id: context.run_id.clone(),
        machine_shortname: context.machine_shortname.clone(),
        kernel_version: context.kernel_version.clone(),
        host_capabilities: context.host_capabilities.clone(),
        attempt: context.attempt,
        run_index: context.run_index,
        hermit_sha: context.source_sha.clone(),
        binary_build_sha: context.binary_build_sha.clone(),
        source_tree_dirty: context.source_dirty,
        binary_sha256: binary_sha,
        test_sha256: test_sha,
        test: cell.id.test.clone(),
        category: cell.category.clone(),
        lane: cell.test.lane.clone(),
        mode: cell.id.mode.clone(),
        backend: cell.id.backend.clone(),
        // `required` records that the selected backend is an executable cell.
        // `ci` controls ordinary validate selection, not whether a manual red
        // measurement is real. Pressure re-runs enabled ci=false cells and
        // must be able to admit their evidence under the same identity.
        classification: if cell.enabled { "required" } else { "disabled" }.into(),
        outcome,
        result,
        failure_class,
        error_kind,
        timeout_seconds: timeouts.wall_seconds,
        execution_cpu_timeout_seconds: Some(timeouts.cpu_seconds),
        execution_wall_timeout_seconds: Some(timeouts.wall_seconds),
        duration_ms: Some(started.elapsed().as_millis()),
        cpu_usage_usec,
        runtime: attempts.iter().find_map(|attempt| attempt.runtime.clone()),
        log_level: (cell.id.mode != "naked").then(|| "info".into()),
        effective_args: literal_argv.iter().skip(1).cloned().collect(),
        argv: literal_argv,
        guest_argv: literal_guest_argv,
        env: literal_env,
        cwd: literal_cwd,
        shell_command: literal_shell_command,
        relaxations: cell_relaxations(cell),
        execution_path,
        diversity: (cell.id.mode == "chaos").then(|| {
            diversity_evidence(
                &hashes,
                mode.outcome_classes,
                mode.assert
                    .as_ref()
                    .and_then(|assert| assert.min_distinct)
                    .unwrap_or(2),
                mode.assert
                    .as_ref()
                    .and_then(|assert| assert.min_normalized_entropy),
            )
        }),
        attempts,
        first_divergent_scheduler_turn,
        first_divergent_virtual_nanoseconds,
        first_divergent_record,
        first_divergent_syscall,
        first_divergent_left_message,
        first_divergent_right_message,
        reason,
    })
}

/// Publish a durable third-outcome row when a cell could not reach execution.
///
/// Infrastructure failures are neither product failures nor admissible green
/// evidence.  Keeping an explicit row prevents preparation/spawn failures from
/// silently disappearing while leaving literal argv empty rather than
/// inventing a command that was never executed.
pub fn infrastructure_error_result(
    context: &RunContext,
    cell: &SelectedCell,
    reason: String,
) -> CellResult {
    let dir = cell_artifact_dir(context, cell);
    let timeouts = cell_timeouts(context, cell).ok();
    CellResult {
        artifact_dir: dir.display().to_string(),
        schema: CELL_RESULT_SCHEMA,
        run_id: context.run_id.clone(),
        machine_shortname: context.machine_shortname.clone(),
        kernel_version: context.kernel_version.clone(),
        host_capabilities: context.host_capabilities.clone(),
        attempt: context.attempt,
        run_index: context.run_index,
        hermit_sha: context.source_sha.clone(),
        binary_build_sha: context.binary_build_sha.clone(),
        source_tree_dirty: context.source_dirty,
        binary_sha256: fs::read(&context.hermit_bin)
            .ok()
            .map(|bytes| hex_digest(&bytes)),
        test_sha256: test_digest(&context.root, &cell.test).unwrap_or_default(),
        test: cell.id.test.clone(),
        category: cell.category.clone(),
        lane: cell.test.lane.clone(),
        mode: cell.id.mode.clone(),
        backend: cell.id.backend.clone(),
        classification: if cell.enabled { "required" } else { "disabled" }.into(),
        outcome: "ERROR".into(),
        result: None,
        failure_class: Some(FailureClass::UnderstoodInfrastructureFailure),
        error_kind: Some("infrastructure".into()),
        timeout_seconds: timeouts
            .map(|policy| policy.wall_seconds)
            .unwrap_or(cell.timeout_seconds),
        execution_cpu_timeout_seconds: timeouts.map(|policy| policy.cpu_seconds),
        execution_wall_timeout_seconds: timeouts.map(|policy| policy.wall_seconds),
        duration_ms: None,
        cpu_usage_usec: None,
        runtime: None,
        log_level: (cell.id.mode != "naked").then(|| "info".into()),
        effective_args: Vec::new(),
        argv: Vec::new(),
        guest_argv: Vec::new(),
        env: execution_cell_env(context, &dir, cell.id.mode != "naked"),
        cwd: context.root.to_string_lossy().into_owned(),
        shell_command: String::new(),
        relaxations: cell_relaxations(cell),
        execution_path: None,
        diversity: None,
        attempts: Vec::new(),
        // No attempt ran, so there is no divergence position to report. `None`
        // here means "never measured", which is the same value a clean run
        // produces -- see the note in the observation fold about those two
        // being indistinguishable in this field alone. The surrounding row is
        // what distinguishes them: this one carries an infrastructure error
        // kind and an empty `attempts`.
        first_divergent_scheduler_turn: None,
        first_divergent_virtual_nanoseconds: None,
        first_divergent_record: None,
        first_divergent_syscall: None,
        first_divergent_left_message: None,
        first_divergent_right_message: None,
        reason: Some(reason),
    }
}

/// Publish a typed result for a cell that provably cannot execute on this host.
///
/// No argv, environment, binary identity, or attempt is invented: the cell did
/// not run. Its normal identity and source digest remain present so the row
/// stays in the selected denominator and downstream completeness checks can
/// distinguish explicit inapplicability from missing output.
pub fn host_inapplicable_result(
    context: &RunContext,
    cell: &SelectedCell,
    reason: String,
) -> CellResult {
    let dir = cell_artifact_dir(context, cell);
    let timeouts = cell_timeouts(context, cell).ok();
    CellResult {
        artifact_dir: dir.display().to_string(),
        schema: CELL_RESULT_SCHEMA,
        run_id: context.run_id.clone(),
        machine_shortname: context.machine_shortname.clone(),
        kernel_version: context.kernel_version.clone(),
        host_capabilities: context.host_capabilities.clone(),
        attempt: context.attempt,
        run_index: context.run_index,
        hermit_sha: context.source_sha.clone(),
        binary_build_sha: context.binary_build_sha.clone(),
        source_tree_dirty: context.source_dirty,
        binary_sha256: None,
        test_sha256: test_digest(&context.root, &cell.test).unwrap_or_default(),
        test: cell.id.test.clone(),
        category: cell.category.clone(),
        lane: cell.test.lane.clone(),
        mode: cell.id.mode.clone(),
        backend: cell.id.backend.clone(),
        classification: if cell.enabled { "required" } else { "disabled" }.into(),
        outcome: "HOST-INAPPLICABLE".into(),
        result: None,
        failure_class: Some(FailureClass::UnderstoodPrerequisiteFailure),
        error_kind: None,
        timeout_seconds: timeouts
            .map(|policy| policy.wall_seconds)
            .unwrap_or(cell.timeout_seconds),
        execution_cpu_timeout_seconds: timeouts.map(|policy| policy.cpu_seconds),
        execution_wall_timeout_seconds: timeouts.map(|policy| policy.wall_seconds),
        duration_ms: None,
        cpu_usage_usec: None,
        runtime: None,
        log_level: None,
        effective_args: Vec::new(),
        argv: Vec::new(),
        guest_argv: Vec::new(),
        env: BTreeMap::new(),
        cwd: context.root.to_string_lossy().into_owned(),
        shell_command: String::new(),
        relaxations: cell_relaxations(cell),
        execution_path: None,
        diversity: None,
        attempts: Vec::new(),
        first_divergent_scheduler_turn: None,
        first_divergent_virtual_nanoseconds: None,
        first_divergent_record: None,
        first_divergent_syscall: None,
        first_divergent_left_message: None,
        first_divergent_right_message: None,
        reason: Some(reason),
    }
}

/// Reduce per-attempt divergence evidence to the cell-level fields.
///
/// Same first-attempt rule as `reason`, and resolved PER COORDINATE rather than
/// per attempt: a report that located a turn but no virtual nanosecond still
/// contributes the turn. Picking one attempt and taking both of its values
/// would silently drop the other coordinate.
///
/// Deliberately NOT a min across attempts. A min over the attempts of one run
/// is a one-run aggregate that would read like the cross-run `ObservedRange`
/// earliest/latest in ci/compat-envelope/scorecard.rs without having a sample
/// behind it, and this project has repeatedly been bitten by numbers that look
/// like a measurement and are not.
/// The divergence coordinates and compared messages of one cell.
///
/// A named struct rather than a tuple, deliberately: the four numeric values
/// are otherwise indistinguishable, and this project has already had a bare
/// ordinal read against the wrong axis. Each numeric field is a different
/// keyspace -- measured on one real divergence, the same event was record 98,
/// syscall 37 and scheduler turn 4. The messages retain the compared event
/// content without treating record position as identity.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DivergencePosition {
    pub scheduler_turn: Option<u64>,
    pub virtual_nanoseconds: Option<u64>,
    pub record: Option<u64>,
    pub syscall: Option<u64>,
    pub left_message: Option<String>,
    pub right_message: Option<String>,
}

fn cell_divergence_position(attempts: &[AttemptResult]) -> DivergencePosition {
    let messages = attempts.iter().find(|attempt| {
        attempt.first_divergent_left_message.is_some()
            || attempt.first_divergent_right_message.is_some()
    });
    DivergencePosition {
        scheduler_turn: attempts
            .iter()
            .find_map(|attempt| attempt.first_divergent_scheduler_turn),
        virtual_nanoseconds: attempts
            .iter()
            .find_map(|attempt| attempt.first_divergent_virtual_nanoseconds),
        record: attempts
            .iter()
            .find_map(|attempt| attempt.first_divergent_record),
        syscall: attempts
            .iter()
            .find_map(|attempt| attempt.first_divergent_syscall),
        left_message: messages.and_then(|attempt| attempt.first_divergent_left_message.clone()),
        right_message: messages.and_then(|attempt| attempt.first_divergent_right_message.clone()),
    }
}

fn summarize_sabre_path_evidence(attempts: &[AttemptResult]) -> Result<Option<JsonValue>, String> {
    let is_sabre = attempts.iter().any(|attempt| {
        attempt
            .argv
            .windows(2)
            .any(|window| window[0] == "--backend" && window[1] == "sabre")
    });
    if !is_sabre {
        return Ok(None);
    }
    let mut executions = Vec::<PathEvidence>::new();
    for line in attempts
        .iter()
        .flat_map(|attempt| attempt.sabre_path_evidence.as_deref().unwrap_or("").lines())
    {
        let value = serde_json::from_str::<JsonValue>(line)
            .map_err(|error| format!("invalid SaBRe path-evidence record: {error}"))?;
        if value.get("schema").and_then(JsonValue::as_u64) != Some(u64::from(PathEvidence::SCHEMA))
        {
            return Err(format!(
                "invalid SaBRe path-evidence record: schema must be {}",
                PathEvidence::SCHEMA
            ));
        }
        executions.push(
            serde_json::from_value::<PathEvidence>(value)
                .map_err(|error| format!("invalid SaBRe path-evidence record: {error}"))?,
        );
    }
    let expected = attempts
        .iter()
        .map(|attempt| {
            if attempt.argv.iter().any(|arg| arg == "--verify") {
                2
            } else {
                1
            }
        })
        .sum::<usize>();
    let complete = executions.len() == expected;
    let mut guest_rpc_observed = complete;
    let mut ptrace_fallback_sites = 0usize;
    let mut trusted_shared_object_sites = 0usize;
    let mut trusted_shared_objects = BTreeSet::new();
    for row in &executions {
        // Keep this pattern exhaustive. Adding a producer field must stop this
        // consumer at compile time until its meaning is handled here.
        let PathEvidence {
            schema,
            guest_rpc_observed: observed,
            ptrace_fallback_sites: fallback_sites,
            trusted_shared_object_sites: shared_object_sites,
            trusted_shared_objects: shared_objects,
        } = row;
        if *schema != PathEvidence::SCHEMA {
            return Err(format!(
                "invalid SaBRe path-evidence record: schema must be {}",
                PathEvidence::SCHEMA
            ));
        }
        guest_rpc_observed &= *observed;
        ptrace_fallback_sites += *fallback_sites;
        trusted_shared_object_sites += *shared_object_sites;
        trusted_shared_objects.extend(shared_objects.iter().cloned());
    }
    let eligible = complete
        && guest_rpc_observed
        && ptrace_fallback_sites == 0
        && trusted_shared_object_sites == 0;
    Ok(Some(serde_json::json!({
        "schema": 1,
        "expected_execution_count": expected,
        "complete": complete,
        "execution_count": executions.len(),
        "guest_rpc_observed": guest_rpc_observed,
        "ptrace_fallback_sites": ptrace_fallback_sites,
        "trusted_shared_object_sites": trusted_shared_object_sites,
        "trusted_shared_objects": trusted_shared_objects,
        "eligible": eligible,
        "executions": executions
    })))
}

fn diversity_evidence(
    hashes: &[String],
    outcome_classes: Option<u64>,
    min_distinct: u64,
    min_normalized_entropy: Option<f64>,
) -> JsonValue {
    let mut counts = BTreeMap::<&str, u64>::new();
    for hash in hashes {
        *counts.entry(hash).or_default() += 1;
    }
    let total = hashes.len() as f64;
    let entropy_bits = counts.values().fold(0.0, |entropy, count| {
        let share = *count as f64 / total.max(1.0);
        if share > 0.0 {
            entropy - share * share.log2()
        } else {
            entropy
        }
    });
    let normalized_entropy = outcome_classes
        .filter(|classes| *classes >= 2)
        .map(|classes| entropy_bits / (classes as f64).log2())
        .unwrap_or(0.0);
    let minority_share = counts
        .values()
        .min()
        .map(|count| *count as f64 / total.max(1.0))
        .unwrap_or(0.0);
    let histogram = counts
        .iter()
        .map(|(hash, count)| format!("{}:{count}", &hash[..hash.len().min(12)]))
        .collect::<Vec<_>>();
    serde_json::json!({
        "distinct": counts.len(), "outcome_classes": outcome_classes, "seeds": hashes.len(),
        "entropy_bits": entropy_bits, "normalized_entropy": normalized_entropy,
        "minority_share": minority_share,
        "oracle_saturated": outcome_classes.is_some_and(|classes| min_distinct >= classes),
        "class_histogram": histogram,
        "min_normalized_entropy": min_normalized_entropy,
    })
}

pub fn prepare_result_path(path: &Path) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map(|_| ())
        .map_err(|e| e.to_string())
}

pub fn append_result(path: &Path, result: &CellResult) -> Result<(), String> {
    result.require_current_classification()?;
    result.require_current_timeout_policy()?;
    // A missing prerequisite means the cell did not execute. Keep the typed
    // value for the harness summary and JUnit skip, but do not publish a cell
    // row that downstream readers could count as an observation. The validate
    // record names the withheld node and prerequisite separately.
    if result.outcome == "HOST-INAPPLICABLE" {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| e.to_string())?;
    serde_json::to_writer(&mut file, result).map_err(|e| e.to_string())?;
    file.write_all(b"\n").map_err(|e| e.to_string())?;
    // The bucket runner publishes each completed cell before printing its
    // PASS/FAIL/ERROR line. Flush the row now rather than waiting for the
    // bucket's JUnit/summary epilogue, which an outer node timeout may kill.
    file.flush().map_err(|e| e.to_string())
}

pub fn write_junit(path: &Path, results: &[CellResult]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let failures = results
        .iter()
        .filter(|result| result.outcome == "FAIL")
        .count();
    let errors = results
        .iter()
        .filter(|result| result.outcome == "ERROR")
        .count();
    let skipped = results
        .iter()
        .filter(|result| result.outcome == "HOST-INAPPLICABLE")
        .count();
    let mut out = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<testsuite name=\"hermit-e2e\" tests=\"{}\" failures=\"{failures}\" errors=\"{errors}\" skipped=\"{skipped}\">\n",
        results.len()
    );
    for result in results {
        let time = result
            .duration_ms
            .map(|duration_ms| format!(" time=\"{:.3}\"", duration_ms as f64 / 1000.0))
            .unwrap_or_default();
        out.push_str(&format!(
            "  <testcase classname=\"{}\" name=\"{}/{}/{}\"{}>",
            xml(&result.category),
            xml(&result.test),
            xml(&result.mode),
            xml(result.backend.as_deref().unwrap_or("none")),
            time,
        ));
        if result.outcome == "FAIL" {
            out.push_str(&format!(
                "<failure>{}</failure>",
                xml(result.reason.as_deref().unwrap_or("failed"))
            ));
        }
        if result.outcome == "ERROR" {
            out.push_str(&format!(
                "<error>{}</error>",
                xml(result.reason.as_deref().unwrap_or("error"))
            ));
        }
        if result.outcome == "HOST-INAPPLICABLE" {
            out.push_str(&format!(
                "<skipped message=\"{}\"/>",
                xml(result.reason.as_deref().unwrap_or("host-inapplicable"))
            ));
        }
        out.push_str("</testcase>\n");
    }
    out.push_str("</testsuite>\n");
    fs::write(path, out).map_err(|e| e.to_string())
}

fn prepare_dirs(root: &Path, dir: &Path) -> Result<(), String> {
    for child in [
        "home",
        "xdg-config",
        "tmp",
        "fixtures",
        "recording",
        "captures",
    ] {
        fs::create_dir_all(dir.join(child)).map_err(|e| e.to_string())?;
    }
    let xdg = root.join("tests/e2e/xdg-config");
    if xdg.is_dir() {
        copy_tree(&xdg, &dir.join("xdg-config"))?;
    }
    Ok(())
}

fn copy_tree(source: &Path, destination: &Path) -> Result<(), String> {
    fs::create_dir_all(destination).map_err(|e| e.to_string())?;
    for entry in fs::read_dir(source).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let target = destination.join(entry.file_name());
        if entry.file_type().map_err(|e| e.to_string())?.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), target).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

fn cell_env(dir: &Path, verified: bool) -> BTreeMap<String, String> {
    BTreeMap::from([
        ("LC_ALL".into(), "C".into()),
        ("TZ".into(), "UTC".into()),
        (
            "HOME".into(),
            dir.join("home").to_string_lossy().into_owned(),
        ),
        (
            "XDG_CONFIG_HOME".into(),
            dir.join("xdg-config").to_string_lossy().into_owned(),
        ),
        (
            "E2E_TMPDIR".into(),
            if verified {
                "/tmp/hermit-e2e".into()
            } else {
                dir.join("tmp").to_string_lossy().into_owned()
            },
        ),
        (
            "E2E_FIXTURE_DIR".into(),
            dir.join("fixtures").to_string_lossy().into_owned(),
        ),
    ])
}

fn execution_cell_env(
    context: &RunContext,
    dir: &Path,
    verified: bool,
) -> BTreeMap<String, String> {
    let mut env = cell_env(dir, verified);
    env.insert(
        SCHEDULED_JOBS_ENV.into(),
        context.scheduled_worker_capacity.configured().to_string(),
    );
    env
}

fn preparation_env(dir: &Path) -> BTreeMap<String, String> {
    let mut env = cell_env(dir, false);
    let ambient_home = std::env::var_os("HOME");
    let rustup_home = std::env::var_os("RUSTUP_HOME");
    let cargo_home = std::env::var_os("CARGO_HOME");
    add_toolchain_homes(
        &mut env,
        rustup_home.as_deref(),
        cargo_home.as_deref(),
        ambient_home.as_deref(),
    );
    env
}

fn add_toolchain_homes(
    env: &mut BTreeMap<String, String>,
    rustup_home: Option<&OsStr>,
    cargo_home: Option<&OsStr>,
    ambient_home: Option<&OsStr>,
) {
    // Guest preparation is a build-time context, not guest execution. Preserve
    // the invoking toolchain state even though the cell's HOME is isolated;
    // these values are deliberately absent from `cell_env` and never become
    // Hermit `--env` arguments.
    for (name, explicit, fallback) in [
        ("RUSTUP_HOME", rustup_home, ".rustup"),
        ("CARGO_HOME", cargo_home, ".cargo"),
    ] {
        let value = explicit
            .map(PathBuf::from)
            .or_else(|| ambient_home.map(|home| PathBuf::from(home).join(fallback)));
        if let Some(value) = value {
            env.insert(name.into(), value.to_string_lossy().into_owned());
        }
    }
}

fn shell_command(cwd: &str, env: &BTreeMap<String, String>, argv: &[String]) -> String {
    let mut words = vec!["cd".into(), shell_quote(cwd), "&&".into(), "env".into()];
    words.extend(
        env.iter()
            .map(|(name, value)| shell_quote(&format!("{name}={value}"))),
    );
    words.extend(argv.iter().map(|arg| shell_quote(arg)));
    words.join(" ")
}

fn shell_quote(value: &str) -> String {
    if !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_@%+=:,./-".contains(&byte))
    {
        value.into()
    } else {
        format!("'{}'", value.replace('\'', "'\"'\"'"))
    }
}

fn append_guest_env_args(
    argv: &mut Vec<String>,
    env: &BTreeMap<String, String>,
    uses_test_workdir: bool,
) {
    // Every forwarded value is harness-authored, never inherited ambient state.
    // PWD and OLDPWD remain absent; E2E_TMPDIR joins the fresh /test mount while
    // HOME/XDG retain their unique per-cell directories and seeded config.
    for name in [
        "LC_ALL",
        "TZ",
        "HOME",
        "XDG_CONFIG_HOME",
        "E2E_TMPDIR",
        "E2E_FIXTURE_DIR",
        SCHEDULED_JOBS_ENV,
    ] {
        let value = if uses_test_workdir && name == "E2E_TMPDIR" {
            HERMETIC_TEST_WORKDIR
        } else {
            env.get(name)
                .expect("cell environment contains every forwarded guest value")
        };
        argv.push("--env".into());
        argv.push(format!("{name}={value}"));
    }
}

fn append_scheduled_jobs_env_arg(argv: &mut Vec<String>, env: &BTreeMap<String, String>) {
    let value = env
        .get(SCHEDULED_JOBS_ENV)
        .expect("cell environment contains scheduled worker capacity");
    argv.push("--env".into());
    argv.push(format!("{SCHEDULED_JOBS_ENV}={value}"));
}

fn require_minimal_base_env(argv: &mut Vec<String>) -> Result<(), String> {
    let mut values = Vec::new();
    let mut index = 0;
    while index < argv.len() {
        if let Some(value) = argv[index].strip_prefix("--base-env=") {
            values.push(value.to_owned());
        } else if argv[index] == "--base-env" {
            index += 1;
            let value = argv.get(index).ok_or_else(|| {
                "hermetic validation received --base-env without a value".to_string()
            })?;
            values.push(value.clone());
        }
        index += 1;
    }
    match values.as_slice() {
        [] => argv.push("--base-env=minimal".into()),
        [value] if value == "minimal" => {}
        [value] => {
            return Err(format!(
                "hermetic validation requires --base-env=minimal, got {value}"
            ));
        }
        _ => return Err("hermetic validation received repeated --base-env flags".into()),
    }
    Ok(())
}

/// Choose the guest's working directory. Three sources, in this order, and the
/// ORDER IS THE POINT.
///
///   1. `isolated_workdir` — the hermetic lane's fresh tmpfs at `/test`, enabled by
///      `HERMIT_E2E_EMPTY_WORKDIR` from `ci/hermetic/run-split-validate.sh`.
///   2. `requested_workdir` — a workdir the manifest names for itself.
///   3. `fixed_workdir_source` — the ordinary-host fallback binds a fresh
///      per-attempt directory at `/tmp/test`.
fn append_execution_root_args(
    argv: &mut Vec<String>,
    backend: &str,
    isolated_workdir: Option<&Path>,
    requested_workdir: Option<&str>,
    fixed_workdir_source: Option<&Path>,
) {
    if let Some(workdir) = isolated_workdir {
        // The marked DBT adapter establishes a fresh namespace and /test tmpfs
        // for each physical execution before constructing its runtime. It still
        // refuses the general --mount CLI surface, so pass only the workdir.
        // Other backends create their per-invocation tmpfs from this argument.
        if backend != "dbt" {
            argv.push(format!("--mount=type=tmpfs,target={}", workdir.display()));
        }
        argv.extend(["--workdir".into(), workdir.to_string_lossy().into_owned()]);
    } else if let Some(workdir) = requested_workdir {
        // Outside the explicit hermetic path, a manifest that names its own
        // workdir keeps it and outranks the ordinary-host fallback.
        argv.extend(["--workdir".into(), workdir.into()]);
    } else if let Some(source) = fixed_workdir_source {
        argv.push(format!(
            "--bind={}:{FIXED_GUEST_WORKDIR}",
            source.to_string_lossy()
        ));
        argv.extend(["--workdir".into(), FIXED_GUEST_WORKDIR.into()]);
    }
}

fn observation_hash(observation: &Observation, attempt: &AttemptResult, dir: &Path) -> String {
    let mut digest = Sha256::new();
    if observation.status {
        digest.update(attempt.status.unwrap_or(-1).to_le_bytes());
        digest.update(attempt.signal.unwrap_or(0).to_le_bytes());
    }
    if observation.stdout {
        digest.update(attempt.stdout.as_bytes());
    }
    if observation.stderr {
        digest.update(attempt.stderr.as_bytes());
    }
    for artifact in &observation.artifacts {
        if let Ok(bytes) = fs::read(dir.join("tmp").join(artifact)) {
            digest.update(bytes);
        }
    }
    format!("{:x}", digest.finalize())
}

fn test_digest(root: &Path, test: &TestRecipe) -> Result<String, String> {
    let bytes = if let Some(program) = &test.program {
        fs::read(root.join(program)).map_err(|e| e.to_string())?
    } else {
        serde_json::to_vec(&test.direct.as_ref().map(|direct| match direct {
            DirectCommand::Shell(s) => vec![s.clone()],
            DirectCommand::Argv(v) => v.clone(),
        }))
        .map_err(|e| e.to_string())?
    };
    Ok(hex_digest(&bytes))
}

fn hex_digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
fn git(root: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Ask the hermit binary which commit it was built from.
///
/// The producer-owned `version --json` record is the authority. Human-readable
/// `--version` output is presentation and must not be parsed into provenance.
/// `None` means the binary could not be run, refused the current schema, or
/// returned an invalid revision; it is deliberately distinct from any value.
fn probe_binary_build_sha(program: &Path) -> Option<String> {
    let output = Command::new(program)
        .args(["version", "--json"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_binary_build_info(&output.stdout)
}

fn parse_binary_build_info(bytes: &[u8]) -> Option<String> {
    let report = serde_json::from_slice::<detcore_model::build_info::BuildInfo>(bytes).ok()?;
    if report.schema != detcore_model::build_info::BuildInfo::SCHEMA {
        return None;
    }
    let sha = report.git_sha;
    let hex = sha.strip_suffix("-dirty").unwrap_or(&sha);
    let recognised =
        hex == "unknown" || (hex.len() >= 7 && hex.chars().all(|c| c.is_ascii_hexdigit()));
    recognised.then_some(sha)
}

fn command_help_contains(program: &Path, args: &[&str], needle: &str) -> bool {
    Command::new(program)
        .args(args)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .is_some_and(|output| {
            String::from_utf8_lossy(&output.stdout).contains(needle)
                || String::from_utf8_lossy(&output.stderr).contains(needle)
        })
}

fn execute_observed_until(
    spec: &CellRunSpec,
    observation: &Observation,
    dir: &Path,
    deadline: Instant,
    cpu_timeout_seconds: u64,
    wall_timeout_seconds: u64,
    remaining_cpu_usec: Option<u64>,
) -> Result<AttemptResult, String> {
    let mut attempt = execute_spec_until(
        spec,
        deadline,
        cpu_timeout_seconds,
        wall_timeout_seconds,
        remaining_cpu_usec,
    )?;
    attempt.observation_sha256 = Some(observation_hash(observation, &attempt, dir));
    Ok(attempt)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ci_selection::BackendCiDisabledReason;

    #[test]
    fn failure_class_schema_matches_serialized_enum() {
        let schema: serde_json::Value =
            serde_json::from_str(include_str!("../failure-class-schema.json")).unwrap();
        assert_eq!(schema["schema"], 1);
        let classes = [
            FailureClass::ProductFailure,
            FailureClass::UnderstoodInfrastructureFailure,
            FailureClass::UnderstoodPrerequisiteFailure,
            FailureClass::NoResult,
        ];
        let serialized = classes
            .iter()
            .map(|class| serde_json::to_value(class).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(schema["values"], serde_json::Value::Array(serialized));
        for class in classes {
            assert_eq!(FailureClass::parse(class.as_str()), Ok(class));
        }
        assert_eq!(
            FailureClass::parse("future_failure"),
            Err("unknown failure_class `future_failure`".into())
        );
    }
    use crate::ci_selection::CiDisabledResult;

    #[test]
    fn retry_outcome_preserves_recovery_and_product_failure() {
        assert_eq!(
            outcome_after_retries([(1, "FAIL"), (2, "PASS")]).unwrap(),
            "PASS"
        );
        assert_eq!(
            outcome_after_retries([(1, "ERROR"), (2, "PASS")]).unwrap(),
            "PASS"
        );
        assert_eq!(
            outcome_after_retries([(1, "FAIL"), (2, "ERROR")]).unwrap(),
            "FAIL"
        );
        assert_eq!(
            outcome_after_retries([(1, "ERROR"), (2, "ERROR")]).unwrap(),
            "ERROR"
        );
        assert_eq!(
            outcome_after_retries([(1, "HOST-INAPPLICABLE")]).unwrap(),
            "HOST-INAPPLICABLE"
        );
    }

    #[test]
    fn retry_outcome_refuses_malformed_history_by_name() {
        for (attempts, named) in [
            (vec![], "no attempts"),
            (vec![(2, "FAIL")], "expected 1"),
            (
                vec![(1, "PASS"), (2, "FAIL")],
                "follows terminal outcome PASS",
            ),
            (
                vec![(1, "HOST-INAPPLICABLE"), (2, "ERROR")],
                "follows terminal outcome HOST-INAPPLICABLE",
            ),
            (vec![(1, "UNKNOWN")], "unknown outcome \"UNKNOWN\""),
            (
                vec![(1, "FAIL"), (2, "ERROR"), (3, "PASS")],
                "exceeds the shared maximum of 2",
            ),
        ] {
            let error = outcome_after_retries(attempts).unwrap_err();
            assert!(error.contains(named), "{error:?} did not name {named:?}");
        }
    }

    #[test]
    fn selected_retry_row_keeps_its_ordinal_while_summary_keeps_history_count() {
        let mut first = cell_result_that_located_nothing();
        first.outcome = "FAIL".into();
        first.attempt = 1;

        let mut infrastructure = first.clone();
        infrastructure.outcome = "ERROR".into();
        infrastructure.attempt = 2;
        let product_then_infrastructure = [first.clone(), infrastructure];
        let (selected, attempts) =
            cell_result_and_attempts_after_retries(&product_then_infrastructure).unwrap();
        assert_eq!(
            (selected.outcome.as_str(), selected.attempt, attempts),
            ("FAIL", 1, 2)
        );

        let mut recovered = first.clone();
        recovered.outcome = "PASS".into();
        recovered.attempt = 2;
        let recovered_history = [first, recovered];
        let (selected, attempts) =
            cell_result_and_attempts_after_retries(&recovered_history).unwrap();
        assert_eq!(
            (selected.outcome.as_str(), selected.attempt, attempts),
            ("PASS", 2, 2)
        );

        let peer = cell_result_that_located_nothing();
        let (selected, attempts) =
            cell_result_and_attempts_after_retries(std::slice::from_ref(&peer)).unwrap();
        assert_eq!(
            (selected.outcome.as_str(), selected.attempt, attempts),
            ("PASS", 1, 1)
        );
    }

    fn recipe(ci: bool) -> TestRecipe {
        let disabled = BTreeMap::from([
            ("dbt".into(), "not qualified".into()),
            ("kvm".into(), "not qualified".into()),
            ("sabre".into(), "not qualified".into()),
            ("liteinst".into(), "not qualified".into()),
        ]);
        let mode = ModeRecipe {
            ci: CiSelectionSpec::Uniform(ci),
            ci_disabled_reason: (!ci)
                // A real sentence, because the shared-string form now carries the
                // same substance rule as a per-backend entry. "not selected yet"
                // is one of the placeholder phrases that rule bans outright.
                .then(|| {
                    CiDisabledReasonSpec::Uniform(
                        "fixture cell: ptrace only, other backends unmeasured here".into(),
                    )
                }),
            backends_enabled: vec!["ptrace".into()],
            backends_disabled: disabled,
            ..ModeRecipe::default()
        };
        TestRecipe {
            id: "fixture/test".into(),
            description: "fixture".into(),
            lane: "portable".into(),
            requires: Vec::new(),
            occasional: false,
            program: None,
            direct: Some(DirectCommand::Argv(vec!["/bin/true".into()])),
            observation: Observation {
                status: true,
                stdout: true,
                stderr: true,
                artifacts: Vec::new(),
            },
            build: None,
            modes: BTreeMap::from([("verify".into(), mode)]),
            preprocessors: Vec::new(),
        }
    }

    /// ⚠️ "NO CELLS" AND "NO SUCH TEST" ARE DIFFERENT ANSWERS, AND `select` GIVES
    /// THE SAME EMPTY VECTOR FOR BOTH. Measured 2026-08-26 before this existed:
    /// `test-harness plan --lane portable --test no-such-test-xyz` printed nothing and
    /// exited 0 -- and so did the same command with a REAL id, so the exit code
    /// carried no information either way and anything bisecting off `plan` read a typo
    /// as "nothing failed here".
    ///
    /// The fix could NOT be `cells.is_empty()`, which is why this predicate exists:
    /// `print_plan` also serves `audit-gaps`, where empty legitimately means NO GAPS.
    /// Measured on the same head, a real id filtered to a lane it has no cells in
    /// (`--lane privileged --test applications/c-toolchain-workflow`) prints `[]` and
    /// exits 0 -- a correct, well-formed query that an emptiness guard would refuse.
    #[test]
    fn knows_test_separates_an_absent_id_from_an_empty_population() {
        let test = recipe(false);
        let id = test.id.clone();
        let set = ManifestSet {
            documents: Vec::new(),
            tests: BTreeMap::from([(
                id.clone(),
                ("fixture".into(), 15, DEFAULT_TEST_CPU_TIMEOUT_SECONDS, test),
            )]),
        };
        assert!(set.knows_test(&id), "a declared test must be known");
        assert!(
            !set.knows_test("no-such-test-xyz"),
            "an id in no manifest must NOT be known -- this is the typo a bisect would \
             otherwise read as a pass"
        );
        // The control that makes the pair meaningful: the KNOWN id still selects zero
        // cells in the Required population, so emptiness and unknown-ness genuinely
        // come apart here rather than only in principle.
        assert!(
            set.select(&Selection {
                population: Some(Population::Required),
                ..Selection::default()
            })
            .unwrap()
            .is_empty(),
            "fixture must be empty in Required, or this test proves nothing"
        );
    }

    fn guest_env_args(argv: &[String]) -> Vec<String> {
        argv.windows(2)
            .filter(|window| window[0] == "--env")
            .map(|window| window[1].clone())
            .collect()
    }

    fn assert_minimal_guest_env(argv: &[String], dir: &str, tmp: &str, jobs: &str) {
        let mut base_env = Vec::new();
        let mut index = 0;
        while index < argv.len() {
            if let Some(value) = argv[index].strip_prefix("--base-env=") {
                base_env.push(value.to_string());
            } else if argv[index] == "--base-env" {
                index += 1;
                base_env.push(argv[index].clone());
            }
            index += 1;
        }
        assert_eq!(
            base_env,
            ["minimal"],
            "every hermetic Hermit cell must select the minimal base environment exactly once: {argv:?}"
        );
        assert_eq!(
            guest_env_args(argv),
            vec![
                "LC_ALL=C".to_string(),
                "TZ=UTC".to_string(),
                format!("HOME={dir}/home"),
                format!("XDG_CONFIG_HOME={dir}/xdg-config"),
                format!("E2E_TMPDIR={tmp}"),
                format!("E2E_FIXTURE_DIR={dir}/fixtures"),
                format!("HERMIT_E2E_SCHEDULED_JOBS={jobs}"),
            ],
            "the guest environment is an exact allowlist; an added, removed, or inherited name is a regression"
        );
    }

    fn ptrace_cell(mode: &str) -> SelectedCell {
        let mut test = recipe(true);
        if mode != "verify" {
            let mode_recipe = test.modes.remove("verify").unwrap();
            test.modes.insert(mode.into(), mode_recipe);
        }
        SelectedCell {
            category: "fixture".into(),
            id: CellId {
                test: test.id.clone(),
                mode: mode.into(),
                backend: Some("ptrace".into()),
            },
            test,
            enabled: true,
            timeout_seconds: 600,
            cpu_timeout_seconds: 22,
        }
    }

    fn fixture_host_capabilities() -> HostCapabilities {
        BTreeMap::from([
            (
                HostCapability::CpuidFaulting,
                HostCapabilityVerdict {
                    present: true,
                    evidence: "fixture cpuid probe".into(),
                },
            ),
            (
                HostCapability::Kvm,
                HostCapabilityVerdict {
                    present: false,
                    evidence: "fixture kvm probe".into(),
                },
            ),
        ])
    }

    fn run_context(root: &Path) -> RunContext {
        RunContext {
            root: root.into(),
            hermit_bin: root.join("hermit"),
            result_root: root.join("results"),
            build_root: root.join("build"),
            run_id: "fixture".into(),
            machine_shortname: "fixture-host".into(),
            kernel_version: "7.1.3-fixture".into(),
            host_capabilities: fixture_host_capabilities(),
            attempt: 1,
            run_index: None,
            source_sha: "0".repeat(40),
            binary_build_sha: None,
            source_dirty: false,
            prebuilt: false,
            keep_logs: false,
            run_verify_strict: false,
            record_verify_strict: false,
            timeout_multipliers: TimeoutMultipliers::default(),
            scheduled_worker_capacity: ScheduledWorkerCapacity::new(1),
            isolated_workdir: None,
        }
    }

    #[test]
    fn cell_retry_attempts_use_distinct_artifact_directories() {
        let root = PathBuf::from("/tmp/hermit-cell-retry-artifact-fixture");
        let cell = ptrace_cell("verify");
        let first = run_context(&root);
        let second = first.with_attempt(2);

        let first_dir = cell_artifact_dir(&first, &cell);
        let second_dir = cell_artifact_dir(&second, &cell);
        assert_ne!(first_dir, second_dir);
        assert!(
            second_dir
                .file_name()
                .is_some_and(|name| name.to_string_lossy().ends_with("-attempt-2"))
        );
    }

    fn bound_workdir_source(spec: &CellRunSpec) -> PathBuf {
        let bind = spec
            .argv
            .iter()
            .find_map(|arg| {
                arg.strip_prefix("--bind=")
                    .and_then(|arg| arg.strip_suffix(":/tmp/test"))
            })
            .expect("default workdir bind is present");
        PathBuf::from(bind)
    }

    #[test]
    fn required_and_enabled_are_distinct_populations() {
        let test = recipe(false);
        let set = ManifestSet {
            documents: Vec::new(),
            tests: BTreeMap::from([(
                test.id.clone(),
                ("fixture".into(), 15, DEFAULT_TEST_CPU_TIMEOUT_SECONDS, test),
            )]),
        };
        assert!(
            set.select(&Selection {
                population: Some(Population::Required),
                ..Selection::default()
            })
            .unwrap()
            .is_empty()
        );
        assert_eq!(
            set.select(&Selection {
                population: Some(Population::Enabled),
                ..Selection::default()
            })
            .unwrap()
            .len(),
            1
        );
        let cell = set
            .select(&Selection {
                population: Some(Population::Enabled),
                ..Selection::default()
            })
            .unwrap()
            .into_iter()
            .next()
            .unwrap();
        let root = std::env::temp_dir().join(format!(
            "hermit-runner-classification-bracket-{}",
            std::process::id()
        ));
        let context = RunContext {
            root: root.clone(),
            hermit_bin: root.join("hermit"),
            result_root: root.join("results"),
            build_root: root.join("build"),
            run_id: "fixture".into(),
            machine_shortname: "fixture-host".into(),
            kernel_version: "7.1.3-fixture".into(),
            host_capabilities: fixture_host_capabilities(),
            attempt: 1,
            run_index: None,
            source_sha: "0".repeat(40),
            binary_build_sha: None,
            source_dirty: false,
            prebuilt: false,
            keep_logs: false,
            run_verify_strict: false,
            record_verify_strict: false,
            timeout_multipliers: TimeoutMultipliers::default(),
            scheduled_worker_capacity: ScheduledWorkerCapacity::new(1),
            isolated_workdir: None,
        };
        assert_eq!(
            infrastructure_error_result(&context, &cell, "fixture".into()).classification,
            "required"
        );
    }

    #[test]
    fn disabled_population_uses_only_its_explicit_guest_arguments() {
        let mut test = recipe(true);
        let mode = test.modes.get_mut("verify").unwrap();
        mode.guest_args.insert(
            "ptrace".into(),
            vec!["ptrace-scenario".into(), "ptrace-detail".into()],
        );
        mode.guest_args
            .insert("kvm".into(), vec!["kvm-scenario".into()]);
        validate_mode("fixture/test", "verify", mode, 60).unwrap();

        let set = ManifestSet {
            documents: Vec::new(),
            tests: BTreeMap::from([(
                test.id.clone(),
                ("fixture".into(), 60, DEFAULT_TEST_CPU_TIMEOUT_SECONDS, test),
            )]),
        };
        let cells = set
            .select(&Selection {
                population: Some(Population::Disabled),
                test: Some("fixture/test".into()),
                mode: Some("verify".into()),
                backend: Some("kvm".into()),
                ..Selection::default()
            })
            .unwrap();
        assert_eq!(cells.len(), 1);
        assert!(!cells[0].enabled);

        let root = std::env::temp_dir().join(format!(
            "hermit-runner-disabled-guest-args-bracket-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let context = run_context(&root);
        let guest = prepare_test(&context, &cells[0], &root.join("results/cell")).unwrap();
        assert_eq!(guest, ["/bin/true", "kvm-scenario"]);
        assert!(
            !guest.iter().any(|arg| arg == "ptrace-scenario"),
            "an unselected backend must not inherit a selected sibling's arguments"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn accepts_explicit_empty_guest_arguments() {
        let mut mode = recipe(true).modes.remove("verify").unwrap();
        mode.guest_args.insert("kvm".into(), Vec::new());
        validate_mode("fixture/test", "verify", &mode, 60).unwrap();
    }

    #[test]
    fn rejects_nul_in_guest_arguments() {
        let mut mode = recipe(true).modes.remove("verify").unwrap();
        mode.guest_args
            .insert("kvm".into(), vec!["contains\0nul".into()]);
        assert_eq!(
            validate_mode("fixture/test", "verify", &mode, 60).unwrap_err(),
            "fixture/test: verify guest_args.kvm contains a NUL byte, which Linux argv cannot represent"
        );
    }

    #[test]
    fn exact_cell_timeout_overrides_the_bucket_and_global_defaults() {
        let mut test = recipe(true);
        let mode = test.modes.get_mut("verify").unwrap();
        mode.backends_enabled = vec!["ptrace".into(), "liteinst".into()];
        mode.backends_disabled.remove("liteinst");
        mode.timeout_seconds.insert("ptrace".into(), 30);
        mode.cpu_timeout_seconds.insert("ptrace".into(), 25);
        mode.slow_reason.insert(
            "ptrace".into(),
            "three complete validation runs measured this cell above the inherited limit".into(),
        );
        validate_mode("fixture/test", "verify", mode, 20).unwrap();
        let set = ManifestSet {
            documents: Vec::new(),
            tests: BTreeMap::from([(
                test.id.clone(),
                ("fixture".into(), 20, DEFAULT_TEST_CPU_TIMEOUT_SECONDS, test),
            )]),
        };
        let cells = set
            .select(&Selection {
                population: Some(Population::Enabled),
                ..Selection::default()
            })
            .unwrap();
        assert_eq!(cells.len(), 2);
        assert_eq!(
            cells
                .iter()
                .find(|cell| cell.id.backend.as_deref() == Some("ptrace"))
                .unwrap()
                .timeout_seconds,
            30
        );
        assert_eq!(
            cells
                .iter()
                .find(|cell| cell.id.backend.as_deref() == Some("liteinst"))
                .unwrap()
                .timeout_seconds,
            20
        );
    }

    #[test]
    fn enabled_kvm_python_examples_keeps_its_measured_timeout() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let defaults: ManifestDefaults = serde_yaml::from_str(
            &fs::read_to_string(root.join("tests/e2e/manifests/defaults.yaml")).unwrap(),
        )
        .unwrap();
        assert_eq!(defaults.timeout_seconds, DEFAULT_TEST_WALL_TIMEOUT_SECONDS);
        assert_eq!(
            defaults.cpu_timeout_seconds,
            DEFAULT_TEST_CPU_TIMEOUT_SECONDS
        );

        let cells = ManifestSet::load(&root)
            .unwrap()
            .select(&Selection {
                population: Some(Population::Enabled),
                test: Some("applications/kvm-python-examples".into()),
                mode: Some("verify".into()),
                backend: Some("kvm".into()),
                ..Selection::default()
            })
            .unwrap();
        assert_eq!(cells.len(), 1);
        assert_eq!(cells[0].timeout_seconds, 74);
        assert_eq!(cells[0].cpu_timeout_seconds, 25);
    }

    #[test]
    fn shipped_readdir_order_identity_retains_its_resolved_timeouts() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let cells = ManifestSet::load(&root)
            .unwrap()
            .select(&Selection {
                population: Some(Population::Required),
                test: Some("backend-parity-c/readdir-order-identity".into()),
                mode: Some("verify".into()),
                backend: Some("ptrace".into()),
                ..Selection::default()
            })
            .unwrap();
        assert_eq!(cells.len(), 1);
        let cell = &cells[0];
        assert_eq!(cell.timeout_seconds, DEFAULT_TEST_WALL_TIMEOUT_SECONDS);
        assert_eq!(cell.cpu_timeout_seconds, DEFAULT_TEST_CPU_TIMEOUT_SECONDS);

        let multipliers = TimeoutMultipliers {
            cpu: 1.25,
            wall: 1.5,
        };
        let resolved = resolved_cell_timeouts(cell, multipliers).unwrap();
        assert_eq!(resolved.cpu_seconds, 28);
        assert_eq!(resolved.wall_seconds, 86);
        let mut context = run_context(&root);
        context.timeout_multipliers = multipliers;
        let retained = infrastructure_error_result(&context, cell, "fixture".into());
        assert_eq!(retained.timeout_seconds, resolved.wall_seconds);
        assert_eq!(
            retained.execution_cpu_timeout_seconds,
            Some(resolved.cpu_seconds)
        );
        assert_eq!(
            retained.execution_wall_timeout_seconds,
            Some(resolved.wall_seconds)
        );
    }

    #[test]
    fn shipped_plan_matches_the_frozen_timeout_calibration_partition() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let manifests = ManifestSet::load(&root).unwrap();
        let required = manifests
            .select(&Selection {
                population: Some(Population::Required),
                ..Selection::default()
            })
            .unwrap();
        let enabled = manifests
            .select(&Selection {
                population: Some(Population::Enabled),
                ..Selection::default()
            })
            .unwrap();
        // https://github.com/rrnewton/hermit/pull/2425 adds these two qualified
        // cells after the frozen calibration. Keep their identities explicit
        // without changing the historical population or timeout overrides.
        let regular_sink = required
            .iter()
            .filter(|cell| cell.id.test == "c-programs/record-replay-file-state-regular-sink")
            .map(|cell| (cell.id.mode.as_str(), cell.id.backend.as_deref()))
            .collect::<BTreeSet<_>>();
        assert_eq!(
            regular_sink,
            BTreeSet::from([("verify", Some("ptrace")), ("verify", Some("sabre"))])
        );
        assert_eq!(
            required.len(),
            CALIBRATED_CI_CELL_COUNT + KVM_RATCHET_CI_CELL_COUNT - KVM_RUN_1709_CI_REMOVAL_COUNT
                + KVM_PINNED_IMAGE_QUALIFIED_CI_CELL_COUNT
                + KVM_NEXT40_QUALIFIED_CI_CELL_COUNT
                + 2
        );
        assert_eq!(
            enabled.len() - required.len(),
            NON_CI_CELL_COUNT,
            "the current manifest census records every enabled ci:false cell"
        );

        let observed = enabled
            .iter()
            .filter(|cell| {
                cell.cpu_timeout_seconds != DEFAULT_TEST_CPU_TIMEOUT_SECONDS
                    || cell.timeout_seconds != DEFAULT_TEST_WALL_TIMEOUT_SECONDS
            })
            .map(|cell| {
                (
                    (
                        cell.id.test.as_str(),
                        cell.id.mode.as_str(),
                        cell.id.backend.as_deref().unwrap_or("native"),
                    ),
                    (cell.cpu_timeout_seconds, cell.timeout_seconds),
                )
            })
            .collect::<BTreeMap<_, _>>();
        let expected = EXPLICIT_TIMEOUT_CALIBRATIONS
            .iter()
            .chain(&KVM_RATCHET_TIMEOUT_CALIBRATIONS)
            .map(|calibration| {
                (
                    (calibration.test, calibration.mode, calibration.backend),
                    (
                        calibration.configured_cpu_seconds,
                        calibration.configured_wall_seconds,
                    ),
                )
            })
            .collect::<BTreeMap<_, _>>();
        assert_eq!(observed, expected);
    }

    #[test]
    fn bucket_timeout_requires_a_reason_and_must_change_the_global_default() {
        let mut document = ManifestDocument {
            schema: MANIFEST_SCHEMA,
            bucket: "fixture".into(),
            timeout_seconds: Some(20),
            slow_reason: Some("measured bucket-wide work needs more than 15 seconds".into()),
            test: vec![recipe(true)],
        };
        document.slow_reason = None;
        assert_eq!(
            validate_document(&document, "fixture", Path::new("/"), 15).unwrap_err(),
            "fixture: bucket timeout_seconds requires a non-empty slow_reason"
        );
        document.slow_reason = Some("measured bucket-wide work needs more than 15 seconds".into());
        document.timeout_seconds = Some(15);
        assert_eq!(
            validate_document(&document, "fixture", Path::new("/"), 15).unwrap_err(),
            "fixture: bucket timeout_seconds redundantly repeats the global default"
        );
    }

    #[test]
    fn cell_timeout_requires_the_same_named_reason() {
        let mut mode = recipe(true).modes.remove("verify").unwrap();
        mode.timeout_seconds.insert("ptrace".into(), 30);
        assert_eq!(
            validate_mode("fixture/test", "verify", &mode, 15).unwrap_err(),
            "fixture/test: verify timeout_seconds, cpu_timeout_seconds, and slow_reason must name the same backends"
        );
        mode.cpu_timeout_seconds.insert("ptrace".into(), 25);
        mode.slow_reason
            .insert("ptrace".into(), "measured above the inherited limit".into());
        assert!(validate_mode("fixture/test", "verify", &mode, 15).is_ok());
    }

    #[test]
    fn execution_wall_backstop_is_wider_than_the_cpu_budget() {
        let started = Instant::now();
        let deadline = execution_deadline_after_preparation(started, 57).unwrap();
        assert_eq!(
            remaining_cell_time_at(deadline, started),
            Duration::from_secs(57)
        );
        assert_eq!(
            remaining_cell_time_at(deadline, started + Duration::from_secs(57)),
            Duration::ZERO
        );
    }

    #[test]
    fn slow_preparation_does_not_reduce_the_execution_backstop() {
        let preparation_started = Instant::now();
        let prepared_at = preparation_started + Duration::from_millis(47_770);
        let execution_deadline = execution_deadline_after_preparation(prepared_at, 57).unwrap();
        assert_eq!(
            remaining_cell_time_at(execution_deadline, prepared_at),
            Duration::from_secs(57),
            "preparation must not consume the fresh explicit wall backstop"
        );
    }

    #[test]
    fn an_expired_wall_backstop_emits_typed_not_run_without_a_process() {
        let root = std::env::temp_dir().join(format!(
            "hermit-runner-prelaunch-timeout-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let spec = CellRunSpec {
            id: CellId {
                test: "fixture/prelaunch-timeout".into(),
                mode: "verify".into(),
                backend: Some("ptrace".into()),
            },
            lane: "portable".into(),
            category: "fixture".into(),
            cwd: root.clone(),
            env: BTreeMap::new(),
            argv: vec!["/bin/false".into()],
            guest_argv: vec!["fixture".into()],
            timeout_seconds: 15,
            verdict_path: Some(root.join("verdict.json")),
            verification_log_dir: None,
            sabre_path_evidence: None,
            cell_dir: root.clone(),
            attempt: "1".into(),
            fixed_workdir_source: root.join("workdir/1"),
        };

        // This is the exact boundary reached when the aggregate execution
        // backstop is gone before a later attempt starts: no child process can
        // exist or have an exit status.
        let attempt = execute_spec_until(&spec, Instant::now(), 15, 57, None).unwrap();
        assert_eq!(attempt.outcome, "ERROR");
        assert_eq!(attempt.error_kind.as_deref(), Some("wall-timeout"));
        assert!(attempt.timed_out);
        assert_eq!((attempt.status, attempt.signal), (None, None));
        let report = attempt
            .verification_report
            .as_deref()
            .expect("pre-launch timeout must carry typed NotRun evidence");
        let report_sha256 = hex_digest(report.as_bytes());
        assert_eq!(
            attempt.verification_report_sha256.as_deref(),
            Some(report_sha256.as_str())
        );
        let report = VerificationReport::from_json_slice(report.as_bytes()).unwrap();
        assert_eq!(report.verdict, Verdict::NoResult);
        assert_eq!(
            report.no_result_reason,
            Some(crate::canonical_verdict::NoResultReason::NotRun)
        );
        assert_eq!(
            failure_class(
                &attempt.outcome,
                observed_result(
                    "verify",
                    &attempt.outcome,
                    std::slice::from_ref(&attempt),
                    attempt.error_kind.as_deref(),
                ),
                attempt.error_kind.as_deref(),
            ),
            Some(FailureClass::NoResult)
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn an_exhausted_cpu_budget_is_typed_before_the_next_process_starts() {
        let root = std::env::temp_dir().join(format!(
            "hermit-runner-prelaunch-cpu-timeout-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let spec = bounded_spec(&root, "prelaunch-cpu-timeout", "/bin/false");
        let attempt = execute_spec_until(
            &spec,
            Instant::now() + Duration::from_secs(5),
            1,
            5,
            Some(0),
        )
        .unwrap();
        assert_eq!(attempt.outcome, "ERROR");
        assert_eq!(attempt.error_kind.as_deref(), Some("cpu-timeout"));
        assert!(attempt.timed_out);
        assert_eq!((attempt.status, attempt.signal), (None, None));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn wait4_cpu_usage_moves_with_descendant_work() {
        let root = std::env::temp_dir().join(format!(
            "hermit-runner-cpu-usage-bracket-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let measure = |label: &str, script: &str| {
            execute_process(
                &root,
                "/bin/sh",
                &["-c".into(), script.into()],
                &BTreeMap::new(),
                &root.join(format!("{label}.stdout")),
                &root.join(format!("{label}.stderr")),
                (Instant::now() + Duration::from_secs(10), None),
            )
            .unwrap()
        };
        let low = measure("low", ":");
        let high = measure("high", "head -c 134217728 /dev/zero | sha256sum >/dev/null");
        assert!(low.status.success());
        assert!(high.status.success());
        assert!(
            high.cpu_usage_usec > low.cpu_usage_usec.saturating_add(10_000),
            "adding 128 MiB of descendant hashing did not move CPU usage: low={} high={}",
            low.cpu_usage_usec,
            high.cpu_usage_usec
        );
        fs::remove_dir_all(root).unwrap();
    }

    fn bounded_process(
        root: &Path,
        label: &str,
        script: &str,
        wall: Duration,
        cpu_budget_usec: u64,
    ) -> ProcessOutput {
        execute_process(
            root,
            "/bin/sh",
            &["-c".into(), script.into()],
            &BTreeMap::new(),
            &root.join(format!("{label}.stdout")),
            &root.join(format!("{label}.stderr")),
            (Instant::now() + wall, Some(cpu_budget_usec)),
        )
        .unwrap()
    }

    #[test]
    fn live_cpu_accounting_excludes_an_unrelated_process_group() {
        let root = std::env::temp_dir().join(format!(
            "hermit-runner-owned-cpu-accounting-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let mut command = Command::new("/bin/sleep");
        command.arg("3");
        unsafe {
            command.pre_exec(|| {
                if libc::setpgid(0, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn().unwrap();
        let pid = child.id();
        // `stop_process_group` below owns the matching wait4; discard only the
        // std handle so the test exercises the production reaper.
        drop(child);
        let unrelated = bounded_process(
            &root,
            "unrelated",
            "while :; do :; done",
            Duration::from_millis(650),
            5_000_000,
        );
        assert_eq!(unrelated.timeout, Some(ProcessTimeout::Wall));
        let used = process_group_cpu_usage_usec(pid)
            .unwrap()
            .expect("the launched sleeper must remain measurable");
        assert!(
            used < 100_000,
            "an idle process group unexpectedly included unrelated CPU: {used} usec"
        );
        stop_process_group(pid).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    fn bounded_spec(root: &Path, label: &str, script: &str) -> CellRunSpec {
        CellRunSpec {
            id: CellId {
                test: format!("fixture/{label}"),
                mode: "naked".into(),
                backend: None,
            },
            lane: "portable".into(),
            category: "fixture".into(),
            cwd: root.to_owned(),
            env: BTreeMap::new(),
            argv: vec!["/bin/sh".into(), "-c".into(), script.into()],
            guest_argv: vec![script.into()],
            timeout_seconds: 1,
            verdict_path: None,
            verification_log_dir: None,
            sabre_path_evidence: None,
            cell_dir: root.join(label),
            attempt: "1".into(),
            fixed_workdir_source: root.join(label).join("workdir/1"),
        }
    }

    #[test]
    fn cpu_burner_is_stopped_by_the_cpu_budget() {
        let root = std::env::temp_dir().join(format!(
            "hermit-runner-live-cpu-budget-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let spec = bounded_spec(&root, "burner", "while :; do :; done");
        let attempt = execute_spec_until(
            &spec,
            Instant::now() + Duration::from_secs(5),
            1,
            5,
            Some(1_000_000),
        )
        .unwrap();
        assert!(attempt.timed_out);
        assert_eq!(attempt.error_kind.as_deref(), Some("cpu-timeout"));
        assert_eq!(attempt.reason.as_deref(), Some("cell exceeded 1 CPU s"));
        assert!(attempt.cpu_usage_usec.unwrap() >= 1_000_000);
        assert_eq!(
            serde_json::to_value(&attempt).unwrap()["error_kind"],
            "cpu-timeout",
            "the retained evidence must distinguish a CPU timeout from wall timeout"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn completed_process_is_checked_against_cpu_budget_between_polls() {
        let root = std::env::temp_dir().join(format!(
            "hermit-runner-completed-cpu-budget-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let output = execute_process_with_cpu_poll_interval(
            &root,
            "/bin/sh",
            &[
                "-c".into(),
                "head -c 8388608 /dev/zero | sha256sum >/dev/null".into(),
            ],
            &BTreeMap::new(),
            &root.join("completed.stdout"),
            &root.join("completed.stderr"),
            ProcessLimits {
                deadline: Instant::now() + Duration::from_secs(5),
                cpu_budget_usec: Some(1),
                cpu_poll_interval: Duration::from_secs(5),
            },
        )
        .unwrap();
        assert!(output.status.success());
        assert_eq!(output.timeout, Some(ProcessTimeout::Cpu));
        assert!(output.cpu_usage_usec >= 1);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn sleeping_process_is_stopped_by_the_wall_backstop() {
        let root = std::env::temp_dir().join(format!(
            "hermit-runner-live-wall-budget-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let spec = bounded_spec(&root, "sleeper", "sleep 5");
        let attempt = execute_spec_until(
            &spec,
            Instant::now() + Duration::from_secs(1),
            1,
            1,
            Some(5_000_000),
        )
        .unwrap();
        assert!(attempt.timed_out);
        assert_eq!(attempt.error_kind.as_deref(), Some("wall-timeout"));
        assert_eq!(
            attempt.reason.as_deref(),
            Some("cell exceeded 1 wall s backstop (1 s CPU budget)")
        );
        assert!(attempt.cpu_usage_usec.unwrap() < 5_000_000);
        assert_eq!(
            serde_json::to_value(&attempt).unwrap()["error_kind"],
            "wall-timeout",
            "the retained evidence must distinguish a wall timeout from CPU timeout"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn descheduled_process_may_exceed_the_old_wall_bound_and_pass() {
        let root = std::env::temp_dir().join(format!(
            "hermit-runner-descheduled-process-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let started = Instant::now();
        let output = bounded_process(
            &root,
            "stopped",
            "(sleep 0.3; kill -CONT $$) & kill -STOP $$; printf resumed",
            Duration::from_secs(2),
            200_000,
        );
        assert!(output.status.success());
        assert_eq!(output.timeout, None);
        assert!(
            started.elapsed() >= Duration::from_millis(250),
            "the process did not remain descheduled long enough to exercise wall-vs-CPU"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn live_cpu_budget_counts_descendant_work() {
        let root = std::env::temp_dir().join(format!(
            "hermit-runner-descendant-cpu-budget-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let output = bounded_process(
            &root,
            "descendant",
            "sh -c 'while :; do :; done' & wait",
            Duration::from_secs(5),
            100_000,
        );
        assert_eq!(output.timeout, Some(ProcessTimeout::Cpu));
        assert!(output.cpu_usage_usec >= 100_000);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn repeated_processes_share_one_aggregate_cpu_budget() {
        let root = std::env::temp_dir().join(format!(
            "hermit-runner-aggregate-cpu-budget-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let limit = 300_000u64;
        let first = bounded_process(
            &root,
            "first",
            "head -c 8388608 /dev/zero | sha256sum >/dev/null",
            Duration::from_secs(5),
            limit,
        );
        assert!(first.status.success());
        assert_eq!(first.timeout, None);
        assert!(first.cpu_usage_usec < limit);
        let second = bounded_process(
            &root,
            "second",
            "while :; do :; done",
            Duration::from_secs(5),
            limit - first.cpu_usage_usec,
        );
        assert_eq!(second.timeout, Some(ProcessTimeout::Cpu));
        assert!(
            first.cpu_usage_usec.saturating_add(second.cpu_usage_usec) >= limit,
            "second process did not consume the remainder of the shared CPU budget"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cpu_usage_aggregation_refuses_missing_or_overflowing_measurements() {
        assert_eq!(checked_add_cpu_usage(Some(2), Some(3)), Some(5));
        assert_eq!(checked_add_cpu_usage(Some(2), None), None);
        assert_eq!(checked_add_cpu_usage(None, Some(3)), None);
        assert_eq!(checked_add_cpu_usage(Some(u64::MAX), Some(1)), None);
    }

    #[test]
    fn sleeping_repeated_invocations_do_not_consume_the_cpu_budget() {
        let root = std::env::temp_dir().join(format!(
            "hermit-runner-cell-deadline-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();

        let mut test = recipe(true);
        let counter = root.join("counter");
        test.direct = Some(DirectCommand::Argv(vec![
            "/bin/sh".into(),
            "-c".into(),
            format!(
                "count=$(cat '{}' 2>/dev/null || printf 0); count=$((count + 1)); printf '%s' \"$count\" > '{}'; sleep 0.7; printf '%s\\n' \"$count\"",
                counter.display(),
                counter.display()
            ),
        ]));
        let mut mode = test.modes.remove("verify").unwrap();
        mode.runs = Some(3);
        test.modes.insert("naked".into(), mode);
        let cell = SelectedCell {
            category: "fixture".into(),
            id: CellId {
                test: test.id.clone(),
                mode: "naked".into(),
                backend: None,
            },
            test,
            enabled: true,
            timeout_seconds: 3,
            cpu_timeout_seconds: 1,
        };
        let context = RunContext {
            root: root.clone(),
            hermit_bin: root.join("missing-hermit"),
            result_root: root.join("results"),
            build_root: root.join("build"),
            run_id: "fixture".into(),
            machine_shortname: "fixture-host".into(),
            kernel_version: "7.1.3-fixture".into(),
            host_capabilities: fixture_host_capabilities(),
            attempt: 1,
            run_index: None,
            source_sha: "0".repeat(40),
            binary_build_sha: None,
            source_dirty: false,
            prebuilt: false,
            keep_logs: false,
            run_verify_strict: false,
            record_verify_strict: false,
            timeout_multipliers: TimeoutMultipliers::default(),
            scheduled_worker_capacity: ScheduledWorkerCapacity::new(1),
            isolated_workdir: None,
        };

        let result = run_cell(&context, &cell).unwrap();
        assert_eq!(result.attempts.len(), 3);
        assert!(result.attempts.iter().all(|attempt| !attempt.timed_out));
        assert_eq!(result.result, Some(ObservedResult::Pass));
        assert_eq!(result.failure_class, None);
        let duration_ms = result
            .duration_ms
            .expect("a cell that executed must report measured wall time");
        assert!(
            (2_000..3_000).contains(&duration_ms),
            "three sleeping attempts should pass despite exceeding the old one-second wall cap: {duration_ms}ms"
        );
        let attempt_cpu_usage_usec = result.attempts.iter().try_fold(0u64, |total, attempt| {
            checked_add_cpu_usage(Some(total), attempt.cpu_usage_usec)
        });
        assert_eq!(
            result.cpu_usage_usec, attempt_cpu_usage_usec,
            "the cell CPU figure must sum every process-owned attempt measurement"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn host_inapplicable_is_a_typed_nonpass_and_a_junit_skip() {
        let root = std::env::temp_dir().join(format!(
            "hermit-runner-host-inapplicable-bracket-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let mut test = recipe(true);
        test.requires = vec!["cpuid".into()];
        let cell = SelectedCell {
            category: "fixture".into(),
            id: CellId {
                test: test.id.clone(),
                mode: "verify".into(),
                backend: Some("ptrace".into()),
            },
            test,
            enabled: true,
            timeout_seconds: 15,
            cpu_timeout_seconds: 10,
        };
        let context = RunContext {
            root: root.clone(),
            hermit_bin: root.join("hermit"),
            result_root: root.join("results"),
            build_root: root.join("build"),
            run_id: "fixture".into(),
            machine_shortname: "fixture-host".into(),
            kernel_version: "7.1.3-fixture".into(),
            host_capabilities: fixture_host_capabilities(),
            attempt: 1,
            run_index: None,
            source_sha: "0".repeat(40),
            binary_build_sha: None,
            source_dirty: false,
            prebuilt: false,
            keep_logs: false,
            run_verify_strict: false,
            record_verify_strict: false,
            timeout_multipliers: TimeoutMultipliers::default(),
            scheduled_worker_capacity: ScheduledWorkerCapacity::new(1),
            isolated_workdir: None,
        };
        let result = host_inapplicable_result(
            &context,
            &cell,
            "NOT RUN, NOT a pass, no coverage: planted absence".into(),
        );
        assert_eq!(result.outcome, "HOST-INAPPLICABLE");
        assert_eq!(result.result, None);
        assert_eq!(
            result.failure_class,
            Some(FailureClass::UnderstoodPrerequisiteFailure)
        );
        assert!(result.attempts.is_empty());
        assert!(result.binary_sha256.is_none());
        assert_eq!(result.error_kind, None);
        assert_eq!(result.duration_ms, None);
        assert_eq!(result.cpu_usage_usec, None);
        let results = root.join("results.jsonl");
        append_result(&results, &result).unwrap();
        assert!(
            !results.exists(),
            "a cell withheld for an unmet prerequisite must not publish a cell row"
        );
        let row = serde_json::to_value(&result).unwrap();
        assert!(
            row.get("duration_ms").is_none(),
            "a cell that never executed must not publish a measured zero wall time"
        );
        assert!(
            row["cpu_usage_usec"].is_null(),
            "a cell that never executed must publish null rather than a measured zero CPU time"
        );
        let mut measured_zero = result.clone();
        measured_zero.duration_ms = Some(0);
        assert_eq!(
            serde_json::to_value(measured_zero).unwrap()["duration_ms"],
            0,
            "a measured sub-millisecond duration must remain zero"
        );

        let junit = root.join("junit.xml");
        write_junit(&junit, &[result]).unwrap();
        let xml = fs::read_to_string(&junit).unwrap();
        assert!(xml.contains("tests=\"1\" failures=\"0\" errors=\"0\" skipped=\"1\""));
        assert!(
            xml.contains(
                "<skipped message=\"NOT RUN, NOT a pass, no coverage: planted absence\"/>"
            )
        );
        assert!(!xml.contains(" time=\"0.000\""));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn retry_preserves_a_divergence_when_the_later_row_passes() {
        let root = std::env::temp_dir().join(format!(
            "hermit-runner-durable-row-bracket-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let test = recipe(true);
        let cell = SelectedCell {
            category: "fixture".into(),
            id: CellId {
                test: test.id.clone(),
                mode: "verify".into(),
                backend: Some("ptrace".into()),
            },
            test,
            enabled: true,
            timeout_seconds: 15,
            cpu_timeout_seconds: 10,
        };
        let mut context = RunContext {
            root: root.clone(),
            hermit_bin: root.join("hermit"),
            result_root: root.join("results"),
            build_root: root.join("build"),
            run_id: "fixture".into(),
            machine_shortname: "fixture-host".into(),
            kernel_version: "7.1.3-fixture".into(),
            host_capabilities: fixture_host_capabilities(),
            attempt: 1,
            run_index: None,
            source_sha: "0".repeat(40),
            binary_build_sha: None,
            source_dirty: false,
            prebuilt: false,
            keep_logs: false,
            run_verify_strict: false,
            record_verify_strict: false,
            timeout_multipliers: TimeoutMultipliers::default(),
            scheduled_worker_capacity: ScheduledWorkerCapacity::new(1),
            isolated_workdir: None,
        };
        let path = root.join("results.jsonl");
        prepare_result_path(&path).unwrap();
        let mut first = infrastructure_error_result(&context, &cell, "forced failure".into());
        first.outcome = "FAIL".into();
        first.result = Some(ObservedResult::DeterminismFailure);
        first.failure_class = Some(FailureClass::ProductFailure);
        first.error_kind = None;
        assert_eq!(first.duration_ms, None);
        first.duration_ms = Some(111);
        first.first_divergent_record = Some(93);
        first.first_divergent_syscall = Some(37);
        first.first_divergent_scheduler_turn = Some(68);
        first.first_divergent_virtual_nanoseconds = Some(7);
        first.first_divergent_left_message = Some("INFO detcore: left event".into());
        first.first_divergent_right_message = Some("INFO detcore: right event".into());
        append_result(&path, &first).unwrap();

        // A validate retry starts a fresh harness process and prepares the same
        // path again. This call is the negative-control boundary: replacing it
        // with the former fs::write(path, b"") drops the first observation.
        prepare_result_path(&path).unwrap();
        context.attempt = 2;
        let mut second = infrastructure_error_result(&context, &cell, "forced retry".into());
        second.outcome = "PASS".into();
        second.result = Some(ObservedResult::Pass);
        second.failure_class = None;
        second.error_kind = None;
        second.duration_ms = Some(222);
        append_result(&path, &second).unwrap();

        let rows = fs::read_to_string(&path).unwrap();
        let rows = rows
            .lines()
            .map(|line| serde_json::from_str::<JsonValue>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["attempt"], 1);
        assert_eq!(rows[0]["duration_ms"], 111);
        assert_eq!(rows[0]["timeout_seconds"], 15);
        assert_eq!(rows[0]["outcome"], "FAIL");
        assert_eq!(rows[0]["result"], "determinism-failure");
        assert_eq!(rows[0]["failure_class"], "product_failure");
        assert_eq!(rows[0]["first_divergent_record"], 93);
        assert_eq!(rows[0]["first_divergent_syscall"], 37);
        assert_eq!(rows[0]["first_divergent_scheduler_turn"], 68);
        assert_eq!(rows[0]["first_divergent_virtual_nanoseconds"], 7);
        assert_eq!(
            rows[0]["first_divergent_left_message"],
            "INFO detcore: left event"
        );
        assert_eq!(
            rows[0]["first_divergent_right_message"],
            "INFO detcore: right event"
        );
        assert_eq!(rows[1]["attempt"], 2);
        assert_eq!(rows[1]["duration_ms"], 222);
        assert_eq!(rows[1]["timeout_seconds"], 15);
        assert_eq!(rows[1]["outcome"], "PASS");
        assert_eq!(rows[1]["result"], "pass");
        assert_eq!(rows[1]["failure_class"], JsonValue::Null);
        assert!(rows[1]["first_divergent_left_message"].is_null());
        assert!(rows[1]["first_divergent_right_message"].is_null());
        assert_ne!(rows[0]["artifact_dir"], rows[1]["artifact_dir"]);
        assert!(
            rows[1]["artifact_dir"]
                .as_str()
                .unwrap()
                .ends_with("-attempt-2")
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn retry_preserves_changed_product_result_and_attribution() {
        let root = std::env::temp_dir().join(format!(
            "hermit-runner-retry-result-class-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let path = root.join("results.jsonl");
        prepare_result_path(&path).unwrap();

        let mut first = cell_result_that_located_nothing();
        first.attempt = 1;
        first.outcome = "FAIL".into();
        first.result = Some(ObservedResult::DeterminismFailure);
        first.failure_class = Some(FailureClass::ProductFailure);
        first.error_kind = None;
        first.artifact_dir = "/repo/artifacts-attempt-1".into();
        append_result(&path, &first).unwrap();

        let mut second = first.clone();
        second.attempt = 2;
        second.result = Some(ObservedResult::CrashError);
        second.artifact_dir = "/repo/artifacts-attempt-2".into();
        append_result(&path, &second).unwrap();

        let rows = fs::read_to_string(&path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<CellResult>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].result, Some(ObservedResult::DeterminismFailure));
        assert_eq!(rows[1].result, Some(ObservedResult::CrashError));
        assert_eq!(rows[0].failure_class, Some(FailureClass::ProductFailure));
        assert_eq!(rows[1].failure_class, Some(FailureClass::ProductFailure));
        assert_eq!(rows[0].error_kind, None);
        assert_eq!(rows[1].error_kind, None);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn required_selection_is_per_backend_while_enabled_selection_keeps_the_failure_visible() {
        let mut test = recipe(false);
        let mode = test.modes.get_mut("verify").unwrap();
        mode.backends_enabled = vec!["ptrace".into(), "liteinst".into()];
        mode.backends_disabled.remove("liteinst");
        mode.ci = CiSelectionSpec::PerBackend(BTreeMap::from([
            ("ptrace".into(), true),
            ("liteinst".into(), false),
        ]));
        mode.ci_disabled_reason = Some(CiDisabledReasonSpec::PerBackend(BTreeMap::from([(
            "liteinst".into(),
            BackendCiDisabledReason {
                result: CiDisabledResult::DeterminismFailure,
                evidence: "ignored/results/liteinst.jsonl".into(),
                reason: "canonical comparison diverged at scheduler turn 10".into(),
            },
        )])));
        validate_mode("fixture/test", "verify", mode, 15).unwrap();
        let set = ManifestSet {
            documents: Vec::new(),
            tests: BTreeMap::from([(
                test.id.clone(),
                ("fixture".into(), 15, DEFAULT_TEST_CPU_TIMEOUT_SECONDS, test),
            )]),
        };
        let required = set
            .select(&Selection {
                population: Some(Population::Required),
                ..Selection::default()
            })
            .unwrap();
        assert_eq!(required.len(), 1);
        assert_eq!(required[0].id.backend.as_deref(), Some("ptrace"));
        let enabled = set
            .select(&Selection {
                population: Some(Population::Enabled),
                ..Selection::default()
            })
            .unwrap();
        assert_eq!(enabled.len(), 2);
        assert!(
            enabled
                .iter()
                .any(|cell| cell.id.backend.as_deref() == Some("liteinst"))
        );
    }

    #[test]
    fn yaml_parses_structured_per_backend_selection() {
        let mode: ModeRecipe = serde_yaml::from_str(
            r#"
ci:
  ptrace: true
  liteinst: false
workdir: /tmp
ci_disabled_reason:
  liteinst:
    result: determinism-failure
    evidence: ignored/results/liteinst.jsonl
    reason: canonical comparison diverged at scheduler turn 10
backends_enabled: [ptrace, liteinst]
backends_disabled:
  dbt: unsupported
  kvm: unsupported
  sabre: unsupported
"#,
        )
        .unwrap();
        validate_mode("fixture/test", "verify", &mode, 15).unwrap();
        assert_eq!(mode.workdir.as_deref(), Some("/tmp"));
        let policy = ci_selection(&mode).unwrap();
        assert!(policy.selected("ptrace"));
        assert!(!policy.selected("liteinst"));
        assert_eq!(
            policy.reason("liteinst").unwrap().result,
            Some(CiDisabledResult::DeterminismFailure)
        );
    }

    #[test]
    fn rejects_relative_run_workdir() {
        let mut mode = recipe(true).modes.remove("verify").unwrap();
        mode.workdir = Some("tmp".into());
        assert_eq!(
            validate_mode("fixture/test", "verify", &mode, 15).unwrap_err(),
            "fixture/test: verify workdir must be an absolute path"
        );
    }

    #[test]
    fn rejects_workdir_for_non_run_modes() {
        for mode in ["replay", "naked"] {
            assert_eq!(
                validate_mode_workdir("fixture/test", mode, Some("/tmp"), &[]).unwrap_err(),
                format!("fixture/test: {mode} workdir is supported only by Hermit run modes")
            );
        }
    }

    #[test]
    fn workdir_accepts_every_hermit_run_backend() {
        let ptrace = vec!["ptrace".into()];
        assert_eq!(
            validate_mode_workdir("fixture/test", "verify", Some("/tmp"), &ptrace),
            Ok(())
        );

        for mode in ["verify", "chaos", "custom"] {
            for backends in [vec!["dbt".into()], vec!["ptrace".into(), "dbt".into()]] {
                assert_eq!(
                    validate_mode_workdir("fixture/test", mode, Some("/tmp"), &backends),
                    Ok(())
                );
            }
        }
    }

    #[test]
    fn run_workdir_precedes_the_guest_separator() {
        let mut test = recipe(true);
        test.modes.get_mut("verify").unwrap().workdir = Some("/tmp".into());
        let cell = SelectedCell {
            category: "fixture".into(),
            id: CellId {
                test: test.id.clone(),
                mode: "verify".into(),
                backend: Some("ptrace".into()),
            },
            test,
            enabled: true,
            timeout_seconds: 15,
            cpu_timeout_seconds: 10,
        };
        let context = RunContext {
            root: PathBuf::from("/repo"),
            hermit_bin: PathBuf::from("/repo/hermit"),
            result_root: PathBuf::from("/repo/results"),
            build_root: PathBuf::from("/repo/build"),
            run_id: "fixture".into(),
            machine_shortname: "fixture-host".into(),
            kernel_version: "7.1.3-fixture".into(),
            host_capabilities: fixture_host_capabilities(),
            attempt: 1,
            run_index: None,
            source_sha: "0".repeat(40),
            binary_build_sha: None,
            source_dirty: false,
            prebuilt: false,
            keep_logs: false,
            run_verify_strict: true,
            record_verify_strict: true,
            timeout_multipliers: TimeoutMultipliers::default(),
            scheduled_worker_capacity: ScheduledWorkerCapacity::new(1),
            isolated_workdir: None,
        };
        let spec = build_spec(
            &context,
            &cell,
            PathBuf::from("/repo/results/cell"),
            vec!["/bin/true".into()],
            "1",
            None,
            cell.timeout_seconds,
        )
        .unwrap();
        let separator = spec.argv.iter().position(|arg| arg == "--").unwrap();
        let workdir = spec
            .argv
            .windows(2)
            .position(|args| args == ["--workdir", "/tmp"])
            .unwrap();
        assert!(workdir < separator);
    }

    #[test]
    fn hermetic_spec_mounts_a_fresh_test_root_and_limits_guest_environment() {
        let test = recipe(true);
        let cell = SelectedCell {
            category: "fixture".into(),
            id: CellId {
                test: test.id.clone(),
                mode: "verify".into(),
                backend: Some("ptrace".into()),
            },
            test,
            enabled: true,
            timeout_seconds: 15,
            cpu_timeout_seconds: 10,
        };
        let context = RunContext {
            root: PathBuf::from("/repo"),
            hermit_bin: PathBuf::from("/repo/hermit"),
            result_root: PathBuf::from("/repo/results"),
            build_root: PathBuf::from("/repo/build"),
            run_id: "fixture".into(),
            machine_shortname: "fixture-host".into(),
            kernel_version: "7.1.3-fixture".into(),
            host_capabilities: fixture_host_capabilities(),
            attempt: 1,
            run_index: None,
            source_sha: "0".repeat(40),
            binary_build_sha: None,
            source_dirty: false,
            prebuilt: false,
            keep_logs: false,
            run_verify_strict: true,
            record_verify_strict: true,
            timeout_multipliers: TimeoutMultipliers::default(),
            scheduled_worker_capacity: ScheduledWorkerCapacity::new(7),
            isolated_workdir: Some(PathBuf::from("/test")),
        };
        let spec = build_spec(
            &context,
            &cell,
            PathBuf::from("/repo/results/cell"),
            vec!["/bin/true".into()],
            "1",
            None,
            cell.timeout_seconds,
        )
        .unwrap();
        let separator = spec.argv.iter().position(|arg| arg == "--").unwrap();
        let mount = spec
            .argv
            .iter()
            .position(|arg| arg == "--mount=type=tmpfs,target=/test")
            .unwrap();
        let workdir = spec
            .argv
            .windows(2)
            .position(|args| args == ["--workdir", "/test"])
            .unwrap();
        assert!(mount < separator && workdir < separator);
        assert_minimal_guest_env(&spec.argv, "/repo/results/cell", "/test", "7");

        let mut replay_test = recipe(true);
        let replay_mode = replay_test.modes.remove("verify").unwrap();
        replay_test.modes.insert("replay".into(), replay_mode);
        let replay_cell = SelectedCell {
            category: "fixture".into(),
            id: CellId {
                test: replay_test.id.clone(),
                mode: "replay".into(),
                backend: Some("ptrace".into()),
            },
            test: replay_test,
            enabled: true,
            timeout_seconds: 15,
            cpu_timeout_seconds: 10,
        };
        let replay = build_spec(
            &context,
            &replay_cell,
            PathBuf::from("/repo/results/replay"),
            vec!["/bin/true".into()],
            "1",
            None,
            7,
        )
        .unwrap();
        assert!(
            replay
                .argv
                .windows(2)
                .any(|window| { window[0] == "--record-timeout" && window[1] == "7" })
        );
        assert_eq!(replay.timeout_seconds, 7);
        assert!(replay.argv.iter().any(|arg| arg == "--base-env=minimal"));
        assert!(
            replay
                .argv
                .iter()
                .any(|arg| arg == "--mount=type=tmpfs,target=/test")
        );
        assert_minimal_guest_env(&replay.argv, "/repo/results/replay", "/test", "7");

        let mut custom_test = recipe(true);
        let mut custom_mode = custom_test.modes.remove("verify").unwrap();
        custom_mode.args = vec!["--base-env".into(), "minimal".into()];
        custom_test.modes.insert("custom".into(), custom_mode);
        let custom_cell = SelectedCell {
            category: "fixture".into(),
            id: CellId {
                test: custom_test.id.clone(),
                mode: "custom".into(),
                backend: Some("ptrace".into()),
            },
            test: custom_test,
            enabled: true,
            timeout_seconds: 15,
            cpu_timeout_seconds: 10,
        };
        let custom = build_spec(
            &context,
            &custom_cell,
            PathBuf::from("/repo/results/custom"),
            vec!["/bin/true".into()],
            "1",
            None,
            custom_cell.timeout_seconds,
        )
        .unwrap();
        assert!(
            custom
                .argv
                .windows(2)
                .any(|args| args == ["--base-env", "minimal"])
        );
        assert!(
            custom
                .argv
                .iter()
                .any(|arg| arg == "--mount=type=tmpfs,target=/test")
        );
        assert_minimal_guest_env(&custom.argv, "/repo/results/custom", "/test", "7");
        let mut naked_test = recipe(true);
        let naked_mode = naked_test.modes.remove("verify").unwrap();
        naked_test.modes.insert("naked".into(), naked_mode);
        let naked_cell = SelectedCell {
            category: "fixture".into(),
            id: CellId {
                test: naked_test.id.clone(),
                mode: "naked".into(),
                backend: None,
            },
            test: naked_test,
            enabled: true,
            timeout_seconds: 15,
            cpu_timeout_seconds: 10,
        };
        let naked_error = build_spec(
            &context,
            &naked_cell,
            PathBuf::from("/repo/results/naked"),
            vec!["/bin/true".into()],
            "1",
            None,
            naked_cell.timeout_seconds,
        )
        .unwrap_err();
        assert_eq!(
            naked_error,
            "hermetic validation cannot isolate a naked cell on the native backend at /test"
        );
    }

    #[test]
    fn hermetic_guest_arguments_resolve_repo_inputs_but_keep_dot_local() {
        let root = std::env::temp_dir().join(format!(
            "hermit-runner-repo-args-bracket-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("tests/e2e")).unwrap();
        fs::write(root.join("README.md"), b"fixture").unwrap();
        let mut argv = vec![
            "tool".into(),
            "tests/e2e".into(),
            "README.md".into(),
            ".".into(),
            "missing/path".into(),
        ];
        resolve_repo_guest_args(&root, &mut argv);
        assert_eq!(argv[0], "tool");
        assert_eq!(argv[1], root.join("tests/e2e").to_string_lossy());
        assert_eq!(argv[2], root.join("README.md").to_string_lossy());
        assert_eq!(argv[3], ".");
        assert_eq!(argv[4], "missing/path");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn ordinary_dbt_arguments_remain_literal_until_isolation_is_requested() {
        let root = std::env::temp_dir().join(format!(
            "hermit-runner-dbt-literal-args-{}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        fs::write(root.as_path().join("literal.txt"), b"fixture").unwrap();
        let mut cell = ptrace_cell("verify");
        cell.id.backend = Some("dbt".into());
        cell.test.program = None;
        let original = vec![
            "/bin/echo".to_owned(),
            "literal.txt".to_owned(),
            "./literal.txt".to_owned(),
            ".".to_owned(),
            "missing/path".to_owned(),
        ];
        cell.test.direct = Some(DirectCommand::Argv(original.clone()));
        let mut context = run_context(root.as_path());
        let dir = root.as_path().join("results/cell");
        assert_eq!(prepare_test(&context, &cell, &dir).unwrap(), original);

        context.isolated_workdir = Some(PathBuf::from("/test"));
        let isolated = prepare_test(&context, &cell, &dir).unwrap();
        assert_eq!(
            isolated,
            vec![
                original[0].clone(),
                root.as_path()
                    .join("literal.txt")
                    .to_string_lossy()
                    .into_owned(),
                root.as_path()
                    .join("./literal.txt")
                    .to_string_lossy()
                    .into_owned(),
                original[3].clone(),
                original[4].clone(),
            ]
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn ordinary_fixed_workdir_resolves_repo_inputs_before_chdir() {
        let root = std::env::temp_dir().join(format!(
            "hermit-runner-ordinary-repo-args-bracket-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("fixtures")).unwrap();
        fs::write(root.join("fixtures/input.txt"), b"fixture").unwrap();

        let mut cell = ptrace_cell("verify");
        cell.test.direct = Some(DirectCommand::Argv(vec![
            "/bin/true".into(),
            "fixtures/input.txt".into(),
            ".".into(),
        ]));
        let context = run_context(&root);
        let cell_dir = root.join("results/cell");
        let guest = prepare_test(&context, &cell, &cell_dir).unwrap();
        assert_eq!(guest[1], root.join("fixtures/input.txt").to_string_lossy());
        assert_eq!(guest[2], ".");

        let spec = build_spec(
            &context,
            &cell,
            cell_dir.clone(),
            guest,
            "1",
            None,
            cell.timeout_seconds,
        )
        .unwrap();
        assert_eq!(
            bound_workdir_source(&spec),
            cell_dir.join("workdir/1"),
            "the ordinary supported path must retain hermit-132's per-attempt bind"
        );
        assert_eq!(
            spec.guest_argv[1],
            root.join("fixtures/input.txt").to_string_lossy(),
            "repo input must be absolute before the guest changes to /tmp/test"
        );
        assert_eq!(spec.guest_argv[2], ".");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn verify_relaxations_are_explicit_and_recorded() {
        let mut test = recipe(true);
        let mode = test.modes.get_mut("verify").unwrap();
        mode.compare_io_buffers = Some(false);
        mode.compare_io_buffers_disabled_reason = Some(
            "guest assertions validate sanitizer-specific invariants while whole files may vary"
                .into(),
        );
        mode.rcb_time = Some(false);
        mode.rcb_time_disabled_reason =
            Some("data-dependent assertion work must not perturb virtual time".into());
        validate_mode("fixture/test", "verify", mode, 15).unwrap();
        let cell = SelectedCell {
            category: "fixture".into(),
            id: CellId {
                test: test.id.clone(),
                mode: "verify".into(),
                backend: Some("ptrace".into()),
            },
            test,
            enabled: true,
            timeout_seconds: 15,
            cpu_timeout_seconds: 10,
        };
        let context = RunContext {
            root: PathBuf::from("/repo"),
            hermit_bin: PathBuf::from("/repo/hermit"),
            result_root: PathBuf::from("/repo/results"),
            build_root: PathBuf::from("/repo/build"),
            run_id: "fixture".into(),
            machine_shortname: "fixture-host".into(),
            kernel_version: "7.1.3-fixture".into(),
            host_capabilities: fixture_host_capabilities(),
            attempt: 1,
            run_index: None,
            source_sha: "0".repeat(40),
            binary_build_sha: None,
            source_dirty: false,
            prebuilt: false,
            keep_logs: false,
            run_verify_strict: true,
            record_verify_strict: true,
            timeout_multipliers: TimeoutMultipliers::default(),
            scheduled_worker_capacity: ScheduledWorkerCapacity::new(1),
            isolated_workdir: None,
        };
        let spec = build_spec(
            &context,
            &cell,
            PathBuf::from("/repo/results/cell"),
            vec!["/bin/true".into()],
            "1",
            None,
            cell.timeout_seconds,
        )
        .unwrap();
        let separator = spec.argv.iter().position(|arg| arg == "--").unwrap();
        let relaxation = spec
            .argv
            .iter()
            .position(|arg| arg == "--no-detlog-io-buffers")
            .unwrap();
        assert!(relaxation < separator);
        let no_rcb_time = spec
            .argv
            .iter()
            .position(|arg| arg == "--no-rcb-time")
            .unwrap();
        assert!(no_rcb_time < separator);
        assert_eq!(
            cell_relaxations(&cell),
            vec![
                String::from(
                    "--no-detlog-io-buffers: guest assertions validate sanitizer-specific invariants while whole files may vary",
                ),
                String::from(
                    "--no-rcb-time: data-dependent assertion work must not perturb virtual time",
                ),
            ]
        );

        let mut missing_reason = recipe(true).modes.remove("verify").unwrap();
        missing_reason.compare_io_buffers = Some(false);
        assert!(
            validate_mode("fixture/test", "verify", &missing_reason, 15)
                .unwrap_err()
                .contains("requires compare_io_buffers_disabled_reason")
        );

        let mut missing_rcb_reason = recipe(true).modes.remove("verify").unwrap();
        missing_rcb_reason.rcb_time = Some(false);
        assert!(
            validate_mode("fixture/test", "verify", &missing_rcb_reason, 15)
                .unwrap_err()
                .contains("requires rcb_time_disabled_reason")
        );
    }

    #[test]
    fn run_spec_records_correct_policy_without_hidden_portable_flags() {
        let test = recipe(true);
        let cell = SelectedCell {
            category: "fixture".into(),
            id: CellId {
                test: test.id.clone(),
                mode: "verify".into(),
                backend: Some("ptrace".into()),
            },
            test,
            enabled: true,
            timeout_seconds: 15,
            cpu_timeout_seconds: 10,
        };
        let context = RunContext {
            root: PathBuf::from("/repo"),
            hermit_bin: PathBuf::from("/repo/hermit"),
            result_root: PathBuf::from("/repo/results"),
            build_root: PathBuf::from("/repo/build"),
            run_id: "fixture".into(),
            machine_shortname: "fixture-host".into(),
            kernel_version: "7.1.3-fixture".into(),
            host_capabilities: fixture_host_capabilities(),
            attempt: 1,
            run_index: None,
            source_sha: "0".repeat(40),
            binary_build_sha: None,
            source_dirty: false,
            prebuilt: false,
            keep_logs: false,
            run_verify_strict: true,
            record_verify_strict: true,
            timeout_multipliers: TimeoutMultipliers::default(),
            scheduled_worker_capacity: ScheduledWorkerCapacity::new(7),
            isolated_workdir: None,
        };
        let spec = build_spec(
            &context,
            &cell,
            PathBuf::from("/repo/results/cell"),
            vec!["/bin/true".into()],
            "1",
            None,
            cell.timeout_seconds,
        )
        .unwrap();
        assert!(spec.argv.iter().any(|arg| arg == "--verify-strict"));
        assert!(spec.argv.iter().any(|arg| arg == "--base-env=minimal"));
        assert_eq!(
            spec.env.get(SCHEDULED_JOBS_ENV).map(String::as_str),
            Some("7")
        );
        assert_minimal_guest_env(&spec.argv, "/repo/results/cell", "/tmp/hermit-e2e", "7");
        assert!(!spec.argv.iter().any(|arg| arg == "--no-virtualize-cpuid"));
        assert!(
            !spec
                .argv
                .iter()
                .any(|arg| arg == "--max-timeslice=disabled")
        );
        for name in ["RUSTUP_HOME", "CARGO_HOME"] {
            assert!(!spec.env.contains_key(name));
            assert!(
                !spec
                    .argv
                    .iter()
                    .any(|arg| arg.starts_with(&format!("{name}=")))
            );
        }

        let mut replay_test = recipe(true);
        let replay_mode = replay_test.modes.remove("verify").unwrap();
        replay_test.modes.insert("replay".into(), replay_mode);
        let replay_cell = SelectedCell {
            category: "fixture".into(),
            id: CellId {
                test: replay_test.id.clone(),
                mode: "replay".into(),
                backend: Some("ptrace".into()),
            },
            test: replay_test,
            enabled: true,
            timeout_seconds: 15,
            cpu_timeout_seconds: 10,
        };
        let replay = build_spec(
            &context,
            &replay_cell,
            PathBuf::from("/repo/results/replay-cell"),
            vec!["/bin/true".into()],
            "1",
            None,
            replay_cell.timeout_seconds,
        )
        .unwrap();
        assert!(replay.argv.iter().any(|arg| arg == "--verify-strict"));
        assert!(replay.argv.windows(2).any(|window| {
            window[0] == "--verify-json" && window[1] == "/repo/results/replay-cell/verify-1.json"
        }));
        assert!(!replay.argv.iter().any(|arg| arg.starts_with("--base-env")));
        assert_eq!(
            guest_env_args(&replay.argv),
            vec![
                "LC_ALL=C".to_string(),
                "TZ=UTC".to_string(),
                "HOME=/repo/results/replay-cell/home".to_string(),
                "XDG_CONFIG_HOME=/repo/results/replay-cell/xdg-config".to_string(),
                "E2E_TMPDIR=/tmp/hermit-e2e".to_string(),
                "E2E_FIXTURE_DIR=/repo/results/replay-cell/fixtures".to_string(),
                "HERMIT_E2E_SCHEDULED_JOBS=7".to_string(),
            ]
        );

        let mut custom_test = recipe(true);
        let mut custom_mode = custom_test.modes.remove("verify").unwrap();
        custom_mode.args = vec!["--base-env=minimal".into()];
        custom_test.modes.insert("custom".into(), custom_mode);
        let custom_cell = SelectedCell {
            category: "fixture".into(),
            id: CellId {
                test: custom_test.id.clone(),
                mode: "custom".into(),
                backend: Some("ptrace".into()),
            },
            test: custom_test,
            enabled: true,
            timeout_seconds: 15,
            cpu_timeout_seconds: 10,
        };
        let custom = build_spec(
            &context,
            &custom_cell,
            PathBuf::from("/repo/results/custom-cell"),
            vec!["/bin/true".into()],
            "1",
            None,
            custom_cell.timeout_seconds,
        )
        .unwrap();
        assert_eq!(
            guest_env_args(&custom.argv),
            vec!["HERMIT_E2E_SCHEDULED_JOBS=7".to_string()]
        );
    }

    #[test]
    fn preparation_preserves_toolchain_homes_outside_the_guest_environment() {
        let mut derived = cell_env(Path::new("/cell"), false);
        add_toolchain_homes(
            &mut derived,
            None,
            None,
            Some(OsStr::new("/example/toolchain-root")),
        );
        assert_eq!(derived["RUSTUP_HOME"], "/example/toolchain-root/.rustup");
        assert_eq!(derived["CARGO_HOME"], "/example/toolchain-root/.cargo");

        let mut explicit = cell_env(Path::new("/cell"), false);
        add_toolchain_homes(
            &mut explicit,
            Some(OsStr::new("/toolchains/rustup")),
            Some(OsStr::new("/toolchains/cargo")),
            Some(OsStr::new("/example/toolchain-root")),
        );
        assert_eq!(explicit["RUSTUP_HOME"], "/toolchains/rustup");
        assert_eq!(explicit["CARGO_HOME"], "/toolchains/cargo");

        let guest = cell_env(Path::new("/cell"), false);
        assert!(!guest.contains_key("RUSTUP_HOME"));
        assert!(!guest.contains_key("CARGO_HOME"));
    }

    #[test]
    fn recorded_shell_command_replays_literal_environment_and_argv() {
        let root = std::env::temp_dir().join(format!(
            "hermit-recorded-command-bracket-{}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        let env = BTreeMap::from([("E2E_VALUE".into(), "space and ' quote".into())]);
        let argv = vec![
            "/bin/sh".into(),
            "-c".into(),
            "test \"$E2E_VALUE\" = \"space and ' quote\" && test \"$1\" = \"arg with spaces\""
                .into(),
            "recorded-command".into(),
            "arg with spaces".into(),
        ];
        let literal = shell_command(&root.to_string_lossy(), &env, &argv);
        let status = Command::new("/bin/sh")
            .args(["-c", &literal])
            .status()
            .unwrap();
        assert!(status.success(), "recorded command failed: {literal}");
        fs::remove_dir_all(root).unwrap();
    }

    fn attempt_with_divergence(
        turn: Option<u64>,
        nanos: Option<u64>,
        record: Option<u64>,
        syscall: Option<u64>,
    ) -> AttemptResult {
        let mut attempt = attempt_with_sabre_evidence("");
        attempt.first_divergent_scheduler_turn = turn;
        attempt.first_divergent_virtual_nanoseconds = nanos;
        attempt.first_divergent_record = record;
        attempt.first_divergent_syscall = syscall;
        attempt
    }

    /// The cell-level position is the FIRST attempt that located one, matching
    /// how `reason` is chosen, and NOT the last -- otherwise a passing retry
    /// would erase the position the failing attempt found.
    #[test]
    fn cell_divergence_takes_the_first_attempt_that_located_one() {
        let mut attempts = vec![
            attempt_with_divergence(None, None, None, None),
            attempt_with_divergence(Some(4), Some(400), Some(40), Some(14)),
            attempt_with_divergence(Some(1), Some(100), Some(10), Some(2)),
        ];
        attempts[1].first_divergent_left_message = Some("left event A".into());
        attempts[1].first_divergent_right_message = Some("right event A".into());
        attempts[2].first_divergent_left_message = Some("left event B".into());
        attempts[2].first_divergent_right_message = Some("right event B".into());
        assert_eq!(
            cell_divergence_position(&attempts),
            DivergencePosition {
                scheduler_turn: Some(4),
                virtual_nanoseconds: Some(400),
                record: Some(40),
                syscall: Some(14),
                left_message: Some("left event A".into()),
                right_message: Some("right event A".into()),
            },
            "the first located position wins; a later, earlier-diverging attempt \
             must not silently replace it, and this is NOT a min across attempts"
        );
    }

    /// Resolved per coordinate, so a report that located only some of the four
    /// still contributes the ones it has. Each coordinate here comes from a
    /// DIFFERENT attempt, which a per-attempt rule could not produce.
    #[test]
    fn cell_divergence_resolves_each_coordinate_independently() {
        let attempts = vec![
            attempt_with_divergence(None, Some(900), None, None),
            attempt_with_divergence(Some(7), None, None, None),
            attempt_with_divergence(None, None, Some(3), None),
            attempt_with_divergence(None, None, None, Some(5)),
        ];
        assert_eq!(
            cell_divergence_position(&attempts),
            DivergencePosition {
                scheduler_turn: Some(7),
                virtual_nanoseconds: Some(900),
                record: Some(3),
                syscall: Some(5),
                left_message: None,
                right_message: None,
            }
        );
    }

    /// A located record with NO syscall is a real state, not a gap: a run can
    /// diverge before any syscall has completed. Measured on a real log --
    /// diverging at record 12 reported syscall `None`, because no
    /// `finish syscall #N` had been written yet.
    #[test]
    fn a_divergence_before_any_syscall_completes_reports_no_syscall() {
        let attempts = vec![attempt_with_divergence(Some(1), None, Some(12), None)];
        let position = cell_divergence_position(&attempts);
        assert_eq!(position.record, Some(12));
        assert_eq!(position.syscall, None);
    }

    /// A clean cell reports no position at all.
    #[test]
    fn cell_divergence_is_absent_when_no_attempt_located_one() {
        let attempts = vec![attempt_with_divergence(None, None, None, None)];
        assert_eq!(
            cell_divergence_position(&attempts),
            DivergencePosition::default()
        );
        assert_eq!(cell_divergence_position(&[]), DivergencePosition::default());
    }

    /// ⚠️ ABSENCE OF THIS KEY IS RESERVED FOR "WRITTEN BEFORE THE FIELD
    /// EXISTED", AND NOTHING ELSE. A row that ran, compared, and located no
    /// divergence must carry the key with an explicit null.
    ///
    /// WHY IT NEEDS A TEST RATHER THAN BEING OBVIOUS. `CELL_RESULT_SCHEMA` was
    /// NOT bumped when these coordinates were added, so the schema number
    /// cannot separate the two meanings. Within schema 4 the only thing that
    /// distinguishes "this predates the field" from "this ran and found nothing"
    /// is whether the key is present at all.
    ///
    /// ⚠️ THE STRUCTURAL CLAIM IS THE DURABLE ONE; THE COUNTS ARE NOT. Schema 4
    /// contains BOTH rows with the key and rows without it, while schemas 1, 2
    /// and 3 never carry it -- that is what makes the schema number useless as a
    /// discriminator, and agent(hermit-106) reproduced it independently across
    /// 1478 rows. The row COUNTS behind it move: this file measured 28899 without
    /// and 17540 with over 6215 retained `results.jsonl`, and a sample of 1200 of
    /// those files gave a different ratio. Both are honest. The corpus lives in
    /// gitignored run directories that agents create and delete, so it is not a
    /// fixed population and a count taken from it is true of one moment. Do not
    /// treat these numbers as a baseline to compare against.
    ///
    /// That distinction currently holds only because no
    /// `skip_serializing_if = "Option::is_none"` is attached to these
    /// fields -- an entirely natural tidy-up for a field that is null on the
    /// large majority of rows. Adding one would silently convert every future
    /// "ran and found nothing" row into a row indistinguishable from a
    /// pre-field one, and no existing test would notice. This is that test.
    ///
    /// It asserts PRESENCE and NULLNESS separately from any value, so it fails
    /// on the skip attribute rather than on a changed position.
    /// ⚠️ THE ROW, NOT ONLY THE ATTEMPT. The measurement in the docstring above is
    /// about `results.jsonl` ROWS, and a row is a `CellResult` with its attempts
    /// nested inside it. An earlier version of this test guarded `AttemptResult`
    /// alone, which left the level the evidence actually describes uncovered:
    /// agent(hermit-106) put `skip_serializing_if = "Option::is_none"` on all four
    /// `CellResult` coordinates and the whole package stayed GREEN, 126 tests, 0
    /// failures, INCLUDING this test. A check that stays green while the hazard it
    /// names is open is the failure this file is full of warnings about.
    fn cell_result_that_located_nothing() -> CellResult {
        CellResult {
            schema: CELL_RESULT_SCHEMA,
            run_id: "fixture".into(),
            machine_shortname: "fixture-host".into(),
            kernel_version: "7.1.3-fixture".into(),
            host_capabilities: fixture_host_capabilities(),
            attempt: 1,
            run_index: None,
            hermit_sha: "sha".into(),
            binary_build_sha: None,
            source_tree_dirty: false,
            binary_sha256: None,
            test_sha256: "digest".into(),
            test: "fixture/test".into(),
            category: "fixture".into(),
            lane: "portable".into(),
            mode: "verify".into(),
            backend: Some("ptrace".into()),
            classification: "required".into(),
            outcome: "PASS".into(),
            result: Some(ObservedResult::Pass),
            failure_class: None,
            error_kind: None,
            timeout_seconds: 3,
            execution_cpu_timeout_seconds: Some(1),
            execution_wall_timeout_seconds: Some(3),
            duration_ms: Some(1),
            cpu_usage_usec: Some(1),
            runtime: None,
            log_level: None,
            effective_args: Vec::new(),
            argv: vec!["hermit".into()],
            guest_argv: vec!["guest".into()],
            env: BTreeMap::new(),
            cwd: "/repo".into(),
            shell_command: "cd /repo && hermit".into(),
            relaxations: Vec::new(),
            execution_path: None,
            diversity: None,
            attempts: vec![attempt_with_sabre_evidence("evidence")],
            first_divergent_scheduler_turn: None,
            first_divergent_virtual_nanoseconds: None,
            first_divergent_record: None,
            first_divergent_syscall: None,
            first_divergent_left_message: None,
            first_divergent_right_message: None,
            reason: None,
            artifact_dir: "/repo/artifacts".into(),
        }
    }

    #[test]
    fn a_located_nothing_row_still_carries_all_divergence_fields() {
        let attempt = attempt_with_sabre_evidence("evidence");
        let rendered = serde_json::to_value(&attempt).expect("attempt serializes");
        let object = rendered.as_object().expect("attempt is a JSON object");
        for key in [
            "first_divergent_record",
            "first_divergent_syscall",
            "first_divergent_scheduler_turn",
            "first_divergent_virtual_nanoseconds",
            "first_divergent_left_message",
            "first_divergent_right_message",
        ] {
            let found = object.get(key).unwrap_or_else(|| {
                panic!(
                    "{key} is absent from a serialized attempt that located nothing. \
                     Absence is how a consumer recognises a row written before this \
                     field existed; emitting it for a row that simply found nothing \
                     merges two different facts that no later reader can separate."
                )
            });
            assert!(
                found.is_null(),
                "{key} must be an explicit null when nothing was located, got {found}"
            );
        }

        // THE ROW LEVEL, which is what a `results.jsonl` line actually is and what
        // every count in the docstring above was measured over.
        let row = cell_result_that_located_nothing();
        let rendered = serde_json::to_value(&row).expect("row serializes");
        let object = rendered.as_object().expect("row is a JSON object");
        assert!(
            !object.contains_key("run_index"),
            "ordinary validate rows must not invent a pressure repetition"
        );
        assert_eq!(object["machine_shortname"], "fixture-host");
        assert_eq!(object["kernel_version"], "7.1.3-fixture");
        assert_eq!(
            object["host_capabilities"]["cpuid-faulting"]["present"],
            true
        );
        assert_eq!(object["host_capabilities"]["kvm"]["present"], false);
        for key in [
            "first_divergent_record",
            "first_divergent_syscall",
            "first_divergent_scheduler_turn",
            "first_divergent_virtual_nanoseconds",
            "first_divergent_left_message",
            "first_divergent_right_message",
        ] {
            let found = object.get(key).unwrap_or_else(|| {
                panic!(
                    "{key} is absent from a serialized RESULT ROW that located nothing. \
                     A results.jsonl line is a CellResult, so this is the level every \
                     count in this test's docstring was measured over; absence here is \
                     how a reader recognises a row written before the field existed."
                )
            });
            assert!(
                found.is_null(),
                "{key} must be an explicit null on the row when nothing was located, got {found}"
            );
        }

        let decoded: CellResult =
            serde_json::from_value(rendered).expect("the producer-owned result type reads its row");
        assert_eq!(decoded.attempt, 1);
        assert_eq!(decoded.run_index, None);
        assert_eq!(decoded.attempts.len(), 1);

        let mut pressure_row = row;
        pressure_row.run_index = Some(4);
        let rendered = serde_json::to_value(&pressure_row).expect("pressure row serializes");
        assert_eq!(rendered["run_index"], 4);
        let decoded: CellResult = serde_json::from_value(rendered)
            .expect("the producer-owned result type reads a pressure row");
        assert_eq!(decoded.run_index, Some(4));
    }

    #[test]
    fn schema4_keeps_its_wall_field_and_adds_explicit_execution_bounds() {
        let row = cell_result_that_located_nothing();
        let mut rendered = serde_json::to_value(&row).expect("cell result serializes");
        assert_eq!(rendered["timeout_seconds"], 3);
        assert_eq!(rendered["execution_cpu_timeout_seconds"], 1);
        assert_eq!(rendered["execution_wall_timeout_seconds"], 3);
        row.require_current_timeout_policy().unwrap();

        let mut half_present = row.clone();
        half_present.execution_wall_timeout_seconds = None;
        assert!(half_present.validate_timeout_policy().is_err());
        let publication = std::env::temp_dir().join(format!(
            "hermit-runner-timeout-policy-publication-{}",
            std::process::id()
        ));
        let _ = fs::remove_file(&publication);
        assert!(append_result(&publication, &half_present).is_err());
        assert!(
            !publication.exists(),
            "a malformed current timeout policy reached the results file"
        );
        let mut wrong_cpu = row.clone();
        wrong_cpu.execution_cpu_timeout_seconds = Some(3);
        assert!(wrong_cpu.validate_timeout_policy().is_err());
        let mut wrong_wall = row.clone();
        wrong_wall.execution_wall_timeout_seconds = Some(2);
        assert!(wrong_wall.validate_timeout_policy().is_err());

        let object = rendered.as_object_mut().unwrap();
        object.remove("execution_cpu_timeout_seconds");
        object.remove("execution_wall_timeout_seconds");
        let retained: CellResult = serde_json::from_value(rendered)
            .expect("a schema-4 row written before the additive fields remains readable");
        assert_eq!(retained.timeout_seconds, 3);
        assert_eq!(retained.execution_cpu_timeout_seconds, None);
        assert_eq!(retained.execution_wall_timeout_seconds, None);
        retained.validate_timeout_policy().unwrap();
        assert!(retained.require_current_timeout_policy().is_err());
    }

    fn attempt_with_sabre_evidence(evidence: &str) -> AttemptResult {
        AttemptResult {
            first_divergent_scheduler_turn: None,
            first_divergent_virtual_nanoseconds: None,
            first_divergent_record: None,
            first_divergent_syscall: None,
            first_divergent_left_message: None,
            first_divergent_right_message: None,
            index: "1".into(),
            outcome: "PASS".into(),
            error_kind: None,
            status: Some(0),
            signal: None,
            timed_out: false,
            duration_ms: 1,
            cpu_usage_usec: Some(1),
            observation_sha256: Some("a".into()),
            argv: vec![
                "hermit".into(),
                "--backend".into(),
                "sabre".into(),
                "--verify".into(),
            ],
            guest_argv: vec!["guest".into()],
            env: BTreeMap::new(),
            cwd: "/repo".into(),
            shell_command: "cd /repo && env hermit --backend sabre --verify".into(),
            stdout: String::new(),
            stderr: String::new(),
            verification_report: None,
            verification_report_sha256: None,
            runtime: None,
            sabre_path_evidence: Some(evidence.into()),
            sabre_path_evidence_sha256: Some("b".into()),
            reason: None,
        }
    }

    #[test]
    fn current_cell_result_carries_result_and_failure_class() {
        let passed = serde_json::to_value(cell_result_that_located_nothing()).unwrap();
        assert_eq!(passed["result"], "pass");
        assert_eq!(passed["failure_class"], JsonValue::Null);

        let mut failed = cell_result_that_located_nothing();
        failed.outcome = "FAIL".into();
        failed.result = Some(ObservedResult::DeterminismFailure);
        failed.failure_class = Some(FailureClass::ProductFailure);
        let failed = serde_json::to_value(failed).unwrap();
        assert_eq!(failed["result"], "determinism-failure");
        assert_eq!(failed["failure_class"], "product_failure");

        let mut missing = cell_result_that_located_nothing();
        missing.outcome = "FAIL".into();
        missing.result = Some(ObservedResult::DeterminismFailure);
        missing.failure_class = None;
        assert_eq!(
            missing.require_current_classification().unwrap_err(),
            "FAIL result has no failure_class; current non-passes must be attributed"
        );

        let mut legacy = cell_result_that_located_nothing();
        legacy.outcome = "FAIL".into();
        legacy.result = None;
        legacy.failure_class = None;
        legacy
            .validate_recorded_classification()
            .expect("retained pre-field schema-4 row remains readable");
        assert!(legacy.require_current_classification().is_err());

        let mut error_with_product_result = cell_result_that_located_nothing();
        error_with_product_result.outcome = "ERROR".into();
        error_with_product_result.result = Some(ObservedResult::CrashError);
        error_with_product_result.failure_class = Some(FailureClass::ProductFailure);
        assert_eq!(
            error_with_product_result
                .require_current_classification()
                .unwrap_err(),
            "ERROR result must carry a non-product observation/classification, got (Some(CrashError), Some(ProductFailure))"
        );

        let mut timeout = cell_result_that_located_nothing();
        timeout.outcome = "ERROR".into();
        timeout.result = Some(ObservedResult::Timeout);
        timeout.failure_class = Some(FailureClass::NoResult);
        timeout.require_current_classification().unwrap();

        let mut sandbox_denied = cell_result_that_located_nothing();
        sandbox_denied.outcome = "ERROR".into();
        sandbox_denied.result = Some(ObservedResult::SandboxDenied);
        sandbox_denied.failure_class = Some(FailureClass::UnderstoodInfrastructureFailure);
        sandbox_denied
            .require_current_classification()
            .expect("typed sandbox denial is a current non-product result");
    }

    #[test]
    fn framework_classifies_divergence_and_crash_before_pressure_reads_them() {
        let mut divergence = attempt_with_sabre_evidence("");
        divergence.outcome = "FAIL".into();
        divergence.verification_report = Some(
            r#"{"verified":false,"bitwise_parity":false,"verdict":"diverged","comparison":{"strictness":"canonical","compare_logs":true,"record_envelope":"all_records_v1"},"compared_log_messages":{"left":1,"right":1},"first_divergent_scheduler_turn":4,"first_divergent_virtual_nanoseconds":7,"first_divergent_record":9,"first_divergent_syscall":2,"first_divergent_left_message":"left","first_divergent_right_message":"right"}"#
                .into(),
        );
        let divergence_result = observed_result(
            "verify",
            &divergence.outcome,
            std::slice::from_ref(&divergence),
            divergence.error_kind.as_deref(),
        );
        assert_eq!(divergence_result, Some(ObservedResult::DeterminismFailure));
        assert_eq!(
            failure_class(
                &divergence.outcome,
                divergence_result,
                divergence.error_kind.as_deref()
            ),
            Some(FailureClass::ProductFailure)
        );

        let mut crash = attempt_with_sabre_evidence("");
        crash.outcome = "FAIL".into();
        crash.status = Some(1);
        let crash_result = observed_result(
            "verify",
            &crash.outcome,
            std::slice::from_ref(&crash),
            crash.error_kind.as_deref(),
        );
        assert_eq!(crash_result, Some(ObservedResult::CrashError));
        assert_eq!(
            failure_class(&crash.outcome, crash_result, crash.error_kind.as_deref()),
            Some(FailureClass::ProductFailure)
        );

        let mut invalidated = divergence.clone();
        invalidated.outcome = "ERROR".into();
        invalidated.error_kind = Some("infrastructure".into());
        let invalidated_result = observed_result(
            "verify",
            &invalidated.outcome,
            std::slice::from_ref(&invalidated),
            invalidated.error_kind.as_deref(),
        );
        assert_eq!(
            invalidated_result, None,
            "a later infrastructure failure must outrank an earlier product observation"
        );
        assert_eq!(
            failure_class(
                &invalidated.outcome,
                invalidated_result,
                invalidated.error_kind.as_deref()
            ),
            Some(FailureClass::UnderstoodInfrastructureFailure)
        );

        let mut invalid_evidence = divergence;
        invalid_evidence.outcome = "ERROR".into();
        invalid_evidence.error_kind = Some("invalid-backend-evidence".into());
        let invalid_evidence_result = observed_result(
            "verify",
            &invalid_evidence.outcome,
            std::slice::from_ref(&invalid_evidence),
            invalid_evidence.error_kind.as_deref(),
        );
        assert_eq!(invalid_evidence_result, None);
        assert_eq!(
            failure_class(
                &invalid_evidence.outcome,
                invalid_evidence_result,
                invalid_evidence.error_kind.as_deref()
            ),
            Some(FailureClass::NoResult)
        );
    }

    #[test]
    fn framework_serializes_environmental_results_from_captured_output() {
        let mut sandbox_denied = attempt_with_sabre_evidence("");
        sandbox_denied.outcome = "ERROR".into();
        sandbox_denied.error_kind = Some("incomplete-verification-evidence".into());
        sandbox_denied.stderr =
            "An action was blocked on this server based on a security policy!\n\
             Enforcer: FS, Reason: FILE_OPEN\n"
                .into();
        let sandbox_result = observed_result(
            "verify",
            &sandbox_denied.outcome,
            std::slice::from_ref(&sandbox_denied),
            sandbox_denied.error_kind.as_deref(),
        );
        assert_eq!(sandbox_result, Some(ObservedResult::SandboxDenied));
        assert_eq!(
            failure_class(
                &sandbox_denied.outcome,
                sandbox_result,
                sandbox_denied.error_kind.as_deref()
            ),
            Some(FailureClass::UnderstoodInfrastructureFailure)
        );

        let mut proxy_denied = sandbox_denied.clone();
        proxy_denied.stderr = "fatal: unable to access repository: Could not resolve proxy".into();
        let infrastructure_result = observed_result(
            "verify",
            &proxy_denied.outcome,
            std::slice::from_ref(&proxy_denied),
            proxy_denied.error_kind.as_deref(),
        );
        assert_eq!(
            infrastructure_result,
            Some(ObservedResult::InfrastructureError)
        );
        assert_eq!(
            failure_class(
                &proxy_denied.outcome,
                infrastructure_result,
                proxy_denied.error_kind.as_deref()
            ),
            Some(FailureClass::UnderstoodInfrastructureFailure)
        );

        let mut ordinary = sandbox_denied;
        ordinary.outcome = "FAIL".into();
        ordinary.error_kind = None;
        ordinary.stderr = "ordinary guest assertion failed".into();
        let ordinary_result = observed_result(
            "verify",
            &ordinary.outcome,
            std::slice::from_ref(&ordinary),
            ordinary.error_kind.as_deref(),
        );
        assert_eq!(ordinary_result, Some(ObservedResult::CrashError));
        assert_eq!(
            failure_class(
                &ordinary.outcome,
                ordinary_result,
                ordinary.error_kind.as_deref()
            ),
            Some(FailureClass::ProductFailure)
        );
    }

    #[test]
    fn sabre_path_evidence_requires_every_execution_and_no_fallback() {
        let clean = r#"{"schema":1,"guest_rpc_observed":true,"ptrace_fallback_sites":0,"trusted_shared_object_sites":0,"trusted_shared_objects":[]}"#;
        let complete = attempt_with_sabre_evidence(&format!("{clean}\n{clean}\n"));
        assert_eq!(
            summarize_sabre_path_evidence(&[complete]).unwrap().unwrap()["eligible"],
            true
        );

        let short = attempt_with_sabre_evidence(&format!("{clean}\n"));
        let summary = summarize_sabre_path_evidence(&[short]).unwrap().unwrap();
        assert_eq!(summary["complete"], false);
        assert_eq!(summary["eligible"], false);

        let malformed = attempt_with_sabre_evidence(
            r#"{"schema":1,"guest_rpc_observed":true,"ptrace_fallback_sites":"0","trusted_shared_object_sites":"0","trusted_shared_objects":"none"}
{"schema":1,"guest_rpc_observed":true,"ptrace_fallback_sites":"0","trusted_shared_object_sites":"0","trusted_shared_objects":"none"}
"#,
        );
        assert!(summarize_sabre_path_evidence(&[malformed]).is_err());

        let unknown = attempt_with_sabre_evidence(
            r#"{"schema":1,"guest_rpc_observed":true,"ptrace_fallback_sites":0,"trusted_shared_object_sites":0,"trusted_shared_objects":[],"unexpected":0}
{"schema":1,"guest_rpc_observed":true,"ptrace_fallback_sites":0,"trusted_shared_object_sites":0,"trusted_shared_objects":[]}
"#,
        );
        let error = summarize_sabre_path_evidence(&[unknown]).unwrap_err();
        assert!(error.contains("unknown field `unexpected`"), "{error}");

        let missing = attempt_with_sabre_evidence(
            r#"{"schema":1,"ptrace_fallback_sites":0,"trusted_shared_object_sites":0,"trusted_shared_objects":[]}
{"schema":1,"guest_rpc_observed":true,"ptrace_fallback_sites":0,"trusted_shared_object_sites":0,"trusted_shared_objects":[]}
"#,
        );
        let error = summarize_sabre_path_evidence(&[missing]).unwrap_err();
        assert!(
            error.contains("missing field `guest_rpc_observed`"),
            "{error}"
        );

        let future = attempt_with_sabre_evidence(
            r#"{"schema":2,"guest_rpc_observed":true,"ptrace_fallback_sites":0,"trusted_shared_object_sites":0,"trusted_shared_objects":[],"future_field":"value"}
{"schema":1,"guest_rpc_observed":true,"ptrace_fallback_sites":0,"trusted_shared_object_sites":0,"trusted_shared_objects":[]}
"#,
        );
        assert_eq!(
            summarize_sabre_path_evidence(&[future]).unwrap_err(),
            "invalid SaBRe path-evidence record: schema must be 1"
        );
    }

    #[test]
    fn normalized_entropy_detects_a_narrowed_two_class_sweep() {
        let balanced = diversity_evidence(
            &["a".into(), "a".into(), "b".into(), "b".into()],
            Some(2),
            2,
            Some(0.8),
        );
        let narrowed = diversity_evidence(
            &["a".into(), "a".into(), "a".into(), "b".into()],
            Some(2),
            2,
            Some(0.8),
        );
        assert!(balanced["normalized_entropy"].as_f64().unwrap() > 0.99);
        assert!(narrowed["normalized_entropy"].as_f64().unwrap() < 0.82);
    }

    fn canonical_verification_report() -> VerificationReport {
        VerificationReport {
            verified: true,
            bitwise_parity: true,
            verdict: Verdict::Matched,
            no_result_reason: None,
            infrastructure_error: None,
            comparison: Some(crate::canonical_verdict::ComparisonReport {
                strictness: crate::canonical_verdict::LogCompareStrictness::Canonical,
                display_name: Some("BitwiseInfoV1".into()),
                compare_logs: true,
                compare_io_buffers: Some(true),
                log_scope: Some(crate::canonical_verdict::ComparedLogScope::Info),
                record_envelope: crate::canonical_verdict::RecordEnvelopeReport::AllRecordsV1,
                virtualize_time: Some(true),
                strip_lines: Some(false),
                canonicalize_addresses: Some(true),
                full_trace: Some(true),
                exact_remainder: Some(true),
                stripped_prefixes: Some(vec!["real-wall-clock-prefix/v1".into()]),
                canonicalizations: Some(vec!["host-address-to-first-appearance-ordinal/v1".into()]),
                ignore_lines: Some(false),
                skip_commit: Some(false),
                skip_detlog: Some(false),
            }),
            compared_log_messages: Some(crate::canonical_verdict::ComparedLogMessages {
                left: 1,
                right: 1,
            }),
            dbt_counted_branches: None,
            runtime: None,
            guest_exit_code: Some(7),
            guest_signal: None,
            first_divergent_scheduler_turn: None,
            first_divergent_virtual_nanoseconds: None,
            first_divergent_record: None,
            first_divergent_syscall: None,
            first_divergent_left_message: None,
            first_divergent_right_message: None,
        }
    }

    fn nonzero_with_canonical_receipt(mode: &str) -> AttemptResult {
        let dir = std::env::temp_dir().join(format!(
            "hermit-runner-status-bracket-{}-{mode}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let verdict = dir.join("verdict.json");
        let report = serde_json::to_string(&canonical_verification_report()).unwrap();
        let spec = CellRunSpec {
            id: CellId {
                test: "fixture/status".into(),
                mode: mode.into(),
                backend: Some("ptrace".into()),
            },
            lane: "portable".into(),
            category: "fixture".into(),
            cwd: dir.clone(),
            env: BTreeMap::new(),
            argv: vec![
                "/bin/sh".into(),
                "-c".into(),
                "printf %s \"$1\" > \"$2\"; exit 7".into(),
                "sh".into(),
                report,
                verdict.to_string_lossy().into_owned(),
            ],
            guest_argv: vec!["fixture".into()],
            timeout_seconds: 5,
            verdict_path: Some(verdict),
            verification_log_dir: None,
            sabre_path_evidence: None,
            cell_dir: dir.clone(),
            attempt: "1".into(),
            fixed_workdir_source: dir.join("workdir/1"),
        };
        let result = execute_spec(&spec).unwrap();
        fs::remove_dir_all(dir).unwrap();
        result
    }

    /// Build one attempt from a real subprocess, so this exercises the same
    /// classification path used by an actual manifest sweep.
    fn attempt_from_script(backend: &str, script: &str, report: Option<&str>) -> AttemptResult {
        let dir = std::env::temp_dir().join(format!(
            "hermit-runner-backend-availability-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let verdict = dir.join("verdict.json");
        let mut argv = vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            script.to_string(),
            "sh".to_string(),
        ];
        argv.push(report.unwrap_or("").to_string());
        argv.push(verdict.to_string_lossy().into_owned());
        let spec = CellRunSpec {
            id: CellId {
                test: "fixture/backend-availability".into(),
                mode: "verify".into(),
                backend: Some(backend.into()),
            },
            lane: "portable".into(),
            category: "fixture".into(),
            cwd: dir.clone(),
            env: BTreeMap::new(),
            argv,
            guest_argv: vec!["fixture".into()],
            timeout_seconds: 10,
            verdict_path: Some(verdict),
            verification_log_dir: None,
            sabre_path_evidence: None,
            cell_dir: dir.clone(),
            attempt: "1".into(),
            fixed_workdir_source: dir.join("workdir/1"),
        };
        let result = execute_spec(&spec).unwrap();
        fs::remove_dir_all(dir).unwrap();
        result
    }

    #[test]
    fn skid_overshoot_receipt_is_an_infrastructure_error_with_comparison_evidence() {
        let mut report = canonical_verification_report();
        report.verified = false;
        report.bitwise_parity = false;
        report.verdict = Verdict::InfrastructureError;
        report.infrastructure_error = Some(InfrastructureError::SkidOvershoot { count: 2 });
        let report = serde_json::to_string(&report).unwrap();

        let result = attempt_from_script(
            "ptrace",
            "printf %s \"$1\" > \"$2\"; exit 122",
            Some(&report),
        );

        assert_eq!(result.outcome, "ERROR");
        assert_eq!(result.error_kind.as_deref(), Some("infrastructure"));
        assert_eq!(result.status, Some(122));
        assert!(
            result
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("2 HERMIT_SKID_OVERSHOOT")),
            "infrastructure error must retain the recorded count: {result:?}"
        );
        assert!(
            result
                .verification_report
                .as_deref()
                .is_some_and(|json| json.contains("\"comparison\":{")),
            "infrastructure error must retain the completed comparison: {result:?}"
        );

        let mut before_comparison = canonical_verification_report();
        before_comparison.verified = false;
        before_comparison.bitwise_parity = false;
        before_comparison.verdict = Verdict::InfrastructureError;
        before_comparison.infrastructure_error =
            Some(InfrastructureError::SkidOvershoot { count: 1 });
        before_comparison.comparison = None;
        before_comparison.compared_log_messages = None;
        let before_comparison = serde_json::to_string(&before_comparison).unwrap();
        let result = attempt_from_script(
            "ptrace",
            "printf %s \"$1\" > \"$2\"; exit 122",
            Some(&before_comparison),
        );
        assert_eq!(result.outcome, "ERROR");
        assert_eq!(result.error_kind.as_deref(), Some("infrastructure"));
        assert!(
            result
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("1 HERMIT_SKID_OVERSHOOT")),
            "comparison-null infrastructure error must retain its cause: {result:?}"
        );
    }

    /// A backend this runner could not start and a backend that ran but recorded
    /// no comparison must stay visible while carrying different error kinds.
    ///
    /// The old shared incomplete-verification-evidence kind turned an unstaged
    /// backend into an apparent product defect. Classifying every nonzero exit
    /// as unavailable would be the same bug with the sign flipped, so this
    /// bracket exercises both directions through real subprocesses.
    #[test]
    fn an_unavailable_backend_is_not_reported_as_a_silent_one() {
        let no_result = r#"{"verified":false,"bitwise_parity":false,"verdict":"no_result","comparison":null,"compared_log_messages":null}"#;
        let unavailable = attempt_from_script(
            "sabre",
            "printf %s \"$1\" > \"$2\"; \
             printf '%s\\n' 'HERMIT_INTERNAL_FAILURE class=backend-unavailable backend=sabre' \
             'Error: backend \x60sabre\x60 is unavailable: HERMIT_SABRE_BINARY=/nonexistent/sabre is not an executable file' >&2; exit 1",
            Some(no_result),
        );
        let silent = attempt_from_script(
            "sabre",
            "printf %s \"$1\" > \"$2\"; exit 0",
            Some(no_result),
        );

        assert_eq!(unavailable.outcome, "ERROR");
        assert_eq!(silent.outcome, "ERROR");
        assert_eq!(
            failure_class(
                &unavailable.outcome,
                observed_result(
                    "verify",
                    &unavailable.outcome,
                    std::slice::from_ref(&unavailable),
                    unavailable.error_kind.as_deref(),
                ),
                unavailable.error_kind.as_deref()
            ),
            Some(FailureClass::UnderstoodPrerequisiteFailure)
        );
        assert_eq!(
            failure_class(
                &silent.outcome,
                observed_result(
                    "verify",
                    &silent.outcome,
                    std::slice::from_ref(&silent),
                    silent.error_kind.as_deref(),
                ),
                silent.error_kind.as_deref()
            ),
            Some(FailureClass::NoResult)
        );
        assert_eq!(
            unavailable.error_kind.as_deref(),
            Some("backend-unavailable"),
            "an unrunnable backend must carry its own kind: {:?}",
            unavailable.reason
        );
        assert_eq!(
            silent.error_kind.as_deref(),
            Some("incomplete-verification-evidence"),
            "a backend that ran and recorded nothing keeps the evidence kind: {:?}",
            silent.reason
        );
        assert_ne!(unavailable.error_kind, silent.error_kind);

        let unavailable_reason = unavailable.reason.clone().expect("reason");
        let silent_reason = silent.reason.clone().expect("reason");
        assert!(
            unavailable_reason.contains("backend unavailable on this runner")
                && unavailable_reason.contains("sabre"),
            "unavailable reason must name the backend and environment: {unavailable_reason}"
        );
        assert!(
            !unavailable_reason.contains("verification report is missing"),
            "the old wording pointed at a missing file, not the backend: {unavailable_reason}"
        );
        assert!(
            silent_reason.contains("no_result"),
            "silent reason must still name the producer state: {silent_reason}"
        );
        assert_ne!(unavailable_reason, silent_reason);
    }

    #[test]
    fn backend_unavailable_requires_the_requested_backend_and_empty_stdout() {
        let no_result = r#"{"verified":false,"bitwise_parity":false,"verdict":"no_result","comparison":null,"compared_log_messages":null}"#;
        let wrong_backend = attempt_from_script(
            "sabre",
            "printf %s \"$1\" > \"$2\"; \
             printf '%s\\n' 'HERMIT_INTERNAL_FAILURE class=backend-unavailable backend=dbt' \
             'Error: backend \x60dbt\x60 is unavailable: no SDK' >&2; exit 7",
            Some(no_result),
        );
        let guest_output = attempt_from_script(
            "sabre",
            "printf %s \"$1\" > \"$2\"; printf 'guest-started\\n'; \
             printf '%s\\n' 'HERMIT_INTERNAL_FAILURE class=backend-unavailable backend=sabre' \
             'Error: backend \x60sabre\x60 is unavailable: spoofed' >&2; exit 8",
            Some(no_result),
        );

        for result in [wrong_backend, guest_output] {
            assert_eq!(result.outcome, "ERROR", "unexpected result: {result:?}");
            assert_eq!(
                observed_result(
                    "verify",
                    &result.outcome,
                    std::slice::from_ref(&result),
                    result.error_kind.as_deref(),
                ),
                None,
                "mismatched producer evidence must not manufacture a product result: {result:?}"
            );
            assert_eq!(
                failure_class(&result.outcome, None, result.error_kind.as_deref()),
                Some(FailureClass::NoResult),
                "mismatched producer evidence must remain no-result: {result:?}"
            );
            assert_eq!(
                result.error_kind.as_deref(),
                Some("invalid-backend-evidence"),
                "unexpected result: {result:?}"
            );
            assert!(
                result
                    .reason
                    .as_deref()
                    .is_some_and(|reason| reason.contains("does not match this attempt")),
                "mismatched producer evidence must name the mismatch: {result:?}"
            );
        }

        let ordinary_failure = attempt_from_script(
            "sabre",
            "printf %s \"$1\" > \"$2\"; printf 'guest-started\\n'; exit 8",
            Some(no_result),
        );
        assert_eq!(ordinary_failure.outcome, "FAIL");
        let observed = observed_result(
            "verify",
            &ordinary_failure.outcome,
            std::slice::from_ref(&ordinary_failure),
            ordinary_failure.error_kind.as_deref(),
        );
        assert_eq!(observed, Some(ObservedResult::CrashError));
        assert_eq!(
            failure_class(
                &ordinary_failure.outcome,
                observed,
                ordinary_failure.error_kind.as_deref(),
            ),
            Some(FailureClass::ProductFailure),
            "an ordinary process failure without contradictory producer evidence remains product-attributed"
        );
    }

    #[test]
    fn backend_unavailable_survives_an_unreadable_current_report() {
        let unavailable = attempt_from_script(
            "sabre",
            "printf %s \"$1\" > \"$2\"; \
             printf '%s\\n' 'HERMIT_INTERNAL_FAILURE class=backend-unavailable backend=sabre' \
             'Error: backend \x60sabre\x60 is unavailable: no staged runtime' >&2; exit 1",
            Some("{"),
        );

        assert_eq!(unavailable.outcome, "ERROR");
        assert_eq!(
            unavailable.error_kind.as_deref(),
            Some("backend-unavailable")
        );
        assert!(
            unavailable
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("no staged runtime")),
            "unreadable evidence must not overwrite the pre-guest refusal: {unavailable:?}"
        );
    }

    #[test]
    fn launch_refusal_requires_the_producer_class_line() {
        let no_result = r#"{"verified":false,"bitwise_parity":false,"verdict":"no_result","comparison":null,"compared_log_messages":null}"#;
        let typed = attempt_from_script(
            "ptrace",
            "printf %s \"$1\" > \"$2\"; printf '%s\\n' \
             'HERMIT_INTERNAL_FAILURE class=guest-program-not-found' \
             'Error: Program /missing does not exist' >&2; exit 127",
            Some(no_result),
        );
        let prose_only = attempt_from_script(
            "ptrace",
            "printf %s \"$1\" > \"$2\"; printf '%s\\n' \
             'HERMIT_INTERNAL_FAILURE class=cli-error' \
             'Error: Program /missing does not exist' >&2; exit 127",
            Some(no_result),
        );

        assert_eq!(typed.error_kind.as_deref(), Some("guest-launch-refused"));
        assert_eq!(typed.outcome, "ERROR");
        assert_eq!(
            prose_only.error_kind.as_deref(),
            Some("incomplete-verification-evidence")
        );
        assert_eq!(prose_only.outcome, "ERROR");
        assert_eq!(
            failure_class(
                &prose_only.outcome,
                observed_result(
                    "verify",
                    &prose_only.outcome,
                    std::slice::from_ref(&prose_only),
                    prose_only.error_kind.as_deref(),
                ),
                prose_only.error_kind.as_deref(),
            ),
            Some(FailureClass::NoResult),
            "English launch prose without the producer class must remain no-result"
        );
    }

    #[test]
    fn canonical_receipt_does_not_erase_process_failure_except_for_chaos() {
        let verify = nonzero_with_canonical_receipt("verify");
        assert_eq!(verify.outcome, "FAIL");
        assert_eq!(verify.status, Some(7));

        let replay = nonzero_with_canonical_receipt("replay");
        assert_eq!(replay.outcome, "FAIL");
        assert_eq!(replay.status, Some(7));

        let chaos = nonzero_with_canonical_receipt("chaos");
        assert_eq!(chaos.outcome, "PASS");
        assert_eq!(chaos.status, Some(7));
    }

    #[test]
    fn chaos_assertion_does_not_relabel_an_incomplete_population_as_a_product_failure() {
        let mut outcome = "ERROR".to_string();
        let mut reason = Some("seed-31 timed out before producing a comparison".to_string());
        apply_failed_chaos_assertion(
            &mut outcome,
            &mut reason,
            "chaos distinct=8 passes=31 failures=1 normalized_entropy=0.9180".into(),
        );
        assert_eq!(outcome, "ERROR");
        assert_eq!(
            reason.as_deref(),
            Some("seed-31 timed out before producing a comparison")
        );

        let mut outcome = "PASS".to_string();
        let mut reason = None;
        apply_failed_chaos_assertion(
            &mut outcome,
            &mut reason,
            "chaos distinct=6 passes=32 failures=0 normalized_entropy=0.7000".into(),
        );
        assert_eq!(outcome, "FAIL");
        assert_eq!(
            reason.as_deref(),
            Some("chaos distinct=6 passes=32 failures=0 normalized_entropy=0.7000")
        );
    }

    fn no_result_with_exit_status(
        status: i32,
        reason: crate::canonical_verdict::NoResultReason,
    ) -> AttemptResult {
        let dir = std::env::temp_dir().join(format!(
            "hermit-runner-no-result-bracket-{}-{status}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let verdict = dir.join("verdict.json");
        let mut report = VerificationReport::no_result();
        report.no_result_reason = Some(reason);
        let report = serde_json::to_string(&report).unwrap();
        let spec = CellRunSpec {
            id: CellId {
                test: "fixture/no-result".into(),
                mode: "verify".into(),
                backend: Some("ptrace".into()),
            },
            lane: "portable".into(),
            category: "fixture".into(),
            cwd: dir.clone(),
            env: BTreeMap::new(),
            argv: vec![
                "/bin/sh".into(),
                "-c".into(),
                r#"printf %s "$1" > "$2"; exit "$3""#.into(),
                "sh".into(),
                report,
                verdict.to_string_lossy().into_owned(),
                status.to_string(),
            ],
            guest_argv: vec!["fixture".into()],
            timeout_seconds: 5,
            verdict_path: Some(verdict),
            verification_log_dir: None,
            sabre_path_evidence: None,
            cell_dir: dir.clone(),
            attempt: "1".into(),
            fixed_workdir_source: dir.join("workdir/1"),
        };
        let result = execute_spec(&spec).unwrap();
        fs::remove_dir_all(dir).unwrap();
        result
    }

    #[test]
    fn no_result_preserves_the_process_outcome_distinction() {
        let failed = no_result_with_exit_status(
            7,
            crate::canonical_verdict::NoResultReason::FirstRunRejected {
                exit_code: Some(7),
                signal: None,
                stdout_bytes: 0,
                stderr_bytes: 0,
            },
        );
        assert_eq!(failed.outcome, "FAIL");
        assert_eq!(
            observed_result(
                "verify",
                &failed.outcome,
                std::slice::from_ref(&failed),
                failed.error_kind.as_deref(),
            ),
            Some(ObservedResult::CrashError)
        );
        assert_eq!(
            failure_class(
                &failed.outcome,
                Some(ObservedResult::CrashError),
                failed.error_kind.as_deref()
            ),
            Some(FailureClass::ProductFailure)
        );
        assert_eq!(failed.error_kind, None);
        assert_eq!(failed.status, Some(7));
        assert!(
            failed
                .reason
                .as_deref()
                .unwrap()
                .contains("before producing a terminal comparison")
        );

        let not_run =
            no_result_with_exit_status(127, crate::canonical_verdict::NoResultReason::NotRun);
        assert_eq!(not_run.outcome, "ERROR");
        assert_eq!(
            failure_class(&not_run.outcome, None, not_run.error_kind.as_deref()),
            Some(FailureClass::NoResult)
        );
        assert_eq!(
            not_run.error_kind.as_deref(),
            Some("incomplete-verification-evidence")
        );
        assert_eq!(not_run.status, Some(127));

        let unknown =
            no_result_with_exit_status(0, crate::canonical_verdict::NoResultReason::NotRun);
        assert_eq!(unknown.outcome, "ERROR");
        assert_eq!(
            failure_class(&unknown.outcome, None, unknown.error_kind.as_deref()),
            Some(FailureClass::NoResult)
        );
        assert_eq!(
            unknown.error_kind.as_deref(),
            Some("incomplete-verification-evidence")
        );
        assert_eq!(unknown.status, Some(0));
    }

    #[test]
    fn failed_preparation_surfaces_its_own_diagnostic() {
        let root = std::env::temp_dir().join(format!(
            "hermit-runner-preparation-diagnostic-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let cell_dir = root.join("cell");
        fs::create_dir_all(cell_dir.join("captures")).unwrap();
        let context = RunContext {
            root: root.clone(),
            hermit_bin: root.join("hermit"),
            result_root: root.join("results"),
            build_root: root.join("build"),
            run_id: "fixture".into(),
            machine_shortname: "fixture-host".into(),
            kernel_version: "7.1.3-fixture".into(),
            host_capabilities: fixture_host_capabilities(),
            attempt: 1,
            run_index: None,
            source_sha: "0".repeat(40),
            binary_build_sha: None,
            source_dirty: false,
            prebuilt: false,
            keep_logs: false,
            run_verify_strict: false,
            record_verify_strict: false,
            timeout_multipliers: TimeoutMultipliers::default(),
            scheduled_worker_capacity: ScheduledWorkerCapacity::new(1),
            isolated_workdir: None,
        };
        let error = run_preparation(
            &context,
            &cell_dir,
            "/bin/sh",
            &[
                "-c".into(),
                "printf 'error: failed to write fixture: Permission denied\\n' >&2; exit 17".into(),
            ],
            Instant::now() + Duration::from_secs(5),
            5,
        )
        .unwrap_err();
        assert!(error.contains("fixture preparation failed for /bin/sh: exited 17"));
        assert!(error.contains("error: failed to write fixture: Permission denied"));
        assert!(!error.contains("no output was captured"));

        fs::remove_file(cell_dir.join("captures/prepare.stderr")).unwrap();
        let empty = with_diagnostic(
            "fixture preparation failed".into(),
            &cell_dir.join("captures"),
        );
        assert!(empty.contains("no output was captured"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn preparation_uses_the_named_cells_wall_deadline() {
        let root = std::env::temp_dir().join(format!(
            "hermit-runner-preparation-timeout-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let cell_dir = root.join("cell");
        fs::create_dir_all(cell_dir.join("captures")).unwrap();
        let context = RunContext {
            root: root.clone(),
            hermit_bin: root.join("hermit"),
            result_root: root.join("results"),
            build_root: root.join("build"),
            run_id: "fixture".into(),
            machine_shortname: "fixture-host".into(),
            kernel_version: "7.1.3-fixture".into(),
            host_capabilities: fixture_host_capabilities(),
            attempt: 1,
            run_index: None,
            source_sha: "0".repeat(40),
            binary_build_sha: None,
            source_dirty: false,
            prebuilt: false,
            keep_logs: false,
            run_verify_strict: false,
            record_verify_strict: false,
            timeout_multipliers: TimeoutMultipliers::default(),
            scheduled_worker_capacity: ScheduledWorkerCapacity::new(1),
            isolated_workdir: None,
        };
        let test = recipe(true);
        let cell = SelectedCell {
            category: "fixture".into(),
            id: CellId {
                test: test.id.clone(),
                mode: "verify".into(),
                backend: Some("ptrace".into()),
            },
            test,
            enabled: true,
            timeout_seconds: 1,
            cpu_timeout_seconds: 1,
        };

        let error = run_preparation(
            &context,
            &cell_dir,
            "/bin/sh",
            &["-c".into(), "sleep 60".into()],
            Instant::now() + Duration::from_secs(1),
            cell.timeout_seconds,
        )
        .unwrap_err();
        assert!(
            error.contains("cell exceeded 1 s during fixture preparation"),
            "{error}"
        );
        let result = infrastructure_error_result(&context, &cell, error);
        assert_eq!(result.test, "fixture/test");
        assert_eq!(result.error_kind.as_deref(), Some("infrastructure"));
        assert!(
            result
                .reason
                .as_deref()
                .unwrap()
                .contains("cell exceeded 1 s during fixture preparation")
        );

        run_preparation(
            &context,
            &cell_dir,
            "/bin/sh",
            &["-c".into(), "true".into()],
            Instant::now() + Duration::from_secs(1),
            cell.timeout_seconds,
        )
        .expect("healthy fixture preparation must finish silently under the same bound");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn preparation_does_not_consume_the_guest_execution_budget() {
        let root = std::env::temp_dir().join(format!(
            "hermit-runner-separate-preparation-budget-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let program = root.join("fixture.sh");
        fs::write(
            &program,
            "#!/bin/sh\ncase \"$1\" in\n  --prepare) sleep 1.1 ;;\n  --run) sleep 1.1; printf 'complete\\n' ;;\n  *) exit 64 ;;\nesac\n",
        )
        .unwrap();
        let mut permissions = fs::metadata(&program).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&program, permissions).unwrap();

        let mut test = recipe(true);
        test.id = "fixture/separate-preparation-budget".into();
        test.program = Some("fixture.sh".into());
        test.direct = None;
        let mut mode = test.modes.remove("verify").unwrap();
        mode.runs = Some(1);
        mode.assert = Some(Assertions {
            min_distinct: Some(1),
            ..Assertions::default()
        });
        test.modes.insert("naked".into(), mode);
        let cell = SelectedCell {
            category: "fixture".into(),
            id: CellId {
                test: test.id.clone(),
                mode: "naked".into(),
                backend: None,
            },
            test,
            enabled: true,
            timeout_seconds: 2,
            cpu_timeout_seconds: 1,
        };
        let context = run_context(&root);

        let result = run_cell(&context, &cell).unwrap();
        assert_eq!(result.outcome, "PASS", "unexpected result: {result:?}");
        assert_eq!(result.result, Some(ObservedResult::Pass));
        assert_eq!(result.attempts.len(), 1);
        assert!(!result.attempts[0].timed_out);
        assert_eq!(result.attempts[0].stdout, "complete\n");
        assert!(
            result.duration_ms.is_some_and(|duration| duration >= 2_000),
            "the fixture did not exercise separate preparation and execution time: {result:?}"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn prebuilt_copy_cannot_inherit_or_accept_a_nonexecutable_guest() {
        let root = std::env::temp_dir().join(format!(
            "hermit-runner-prebuilt-bracket-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let build_root = root.join("build");
        let source = build_root.join("fixture-test/fixtures");
        let cell_dir = root.join("cell");
        fs::create_dir_all(&source).unwrap();

        let mut test = recipe(true);
        test.program = Some("tests/fixture.c".into());
        test.direct = None;
        let cell = SelectedCell {
            category: "fixture".into(),
            id: CellId {
                test: test.id.clone(),
                mode: "verify".into(),
                backend: Some("ptrace".into()),
            },
            test,
            enabled: true,
            timeout_seconds: 15,
            cpu_timeout_seconds: 10,
        };
        let context = RunContext {
            root: root.clone(),
            hermit_bin: root.join("hermit"),
            result_root: root.join("results"),
            build_root,
            run_id: "fixture".into(),
            machine_shortname: "fixture-host".into(),
            kernel_version: "7.1.3-fixture".into(),
            host_capabilities: fixture_host_capabilities(),
            attempt: 1,
            run_index: None,
            source_sha: "0".repeat(40),
            binary_build_sha: None,
            source_dirty: false,
            prebuilt: true,
            keep_logs: false,
            run_verify_strict: false,
            record_verify_strict: false,
            timeout_multipliers: TimeoutMultipliers::default(),
            scheduled_worker_capacity: ScheduledWorkerCapacity::new(1),
            isolated_workdir: None,
        };

        fs::create_dir_all(cell_dir.join("fixtures")).unwrap();
        fs::write(cell_dir.join("fixtures/program"), b"stale").unwrap();
        fs::set_permissions(
            cell_dir.join("fixtures/program"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        assert!(prepare_test(&context, &cell, &cell_dir).is_err());
        assert!(!cell_dir.join("fixtures/program").exists());

        fs::write(source.join("program"), b"new").unwrap();
        fs::set_permissions(source.join("program"), fs::Permissions::from_mode(0o644)).unwrap();
        assert!(prepare_test(&context, &cell, &cell_dir).is_err());

        fs::set_permissions(source.join("program"), fs::Permissions::from_mode(0o755)).unwrap();
        assert!(prepare_test(&context, &cell, &cell_dir).is_ok());
        fs::remove_dir_all(root).unwrap();
    }

    fn build_info_json(git_sha: &str) -> Vec<u8> {
        serde_json::to_vec(&detcore_model::build_info::BuildInfo {
            schema: detcore_model::build_info::BuildInfo::SCHEMA,
            version: "0.2.0".into(),
            build_date: Some("2026-08-25".into()),
            git_sha: git_sha.into(),
            features: detcore_model::build_info::BuildFeatures {
                dbt: false,
                e9patch: false,
                sabre: false,
            },
        })
        .unwrap()
    }

    /// The binary's own revision is read from its typed record, including the
    /// dirty marker, which the checkout HEAD cannot supply because it describes
    /// a different tree at a different time.
    #[test]
    fn binary_build_sha_is_read_from_the_typed_build_record() {
        assert_eq!(
            parse_binary_build_info(&build_info_json("351cd3603f7e-dirty")).as_deref(),
            Some("351cd3603f7e-dirty"),
            "a dirty build must keep saying so; that is the fact the checkout cannot supply"
        );
        assert_eq!(
            parse_binary_build_info(&build_info_json("3d85028b3bca")).as_deref(),
            Some("3d85028b3bca")
        );
        // 40-hex is equally acceptable; the width is not the contract.
        assert_eq!(
            parse_binary_build_info(&build_info_json("351cd3603f7e537297067e07a20c5ccf7a23c0e0"))
                .as_deref(),
            Some("351cd3603f7e537297067e07a20c5ccf7a23c0e0")
        );
    }

    /// "I do not know" is a value the binary can state, and it must survive as
    /// one rather than collapsing into "I could not ask".
    #[test]
    fn an_unknown_revision_is_preserved_not_dropped() {
        assert_eq!(
            parse_binary_build_info(&build_info_json("unknown")).as_deref(),
            Some("unknown")
        );
    }

    /// Nothing recognisable must yield `None`, never a guess. A provenance that
    /// could not be established must not be reported as one that matched.
    #[test]
    fn malformed_or_unsupported_build_records_establish_nothing() {
        for bytes in [
            b"".as_slice(),
            br#"{"schema":1}"#,
            br#"{"schema":2,"version":"0.2.0","build_date":null,"git_sha":"351cd3603f7e","features":{"dbt":false,"e9patch":false,"sabre":false}}"#,
            &build_info_json("zzzz"),
        ] {
            assert_eq!(
                parse_binary_build_info(bytes),
                None,
                "must not invent provenance from {}",
                String::from_utf8_lossy(bytes)
            );
        }
    }

    /// Mutating the producer's typed value must move the consumer's value.
    #[test]
    fn binary_build_sha_follows_the_typed_field() {
        assert_eq!(
            parse_binary_build_info(&build_info_json("351cd3603f7e")),
            Some("351cd3603f7e".into())
        );
        assert_eq!(
            parse_binary_build_info(&build_info_json("aaaaaaaaaaaa")),
            Some("aaaaaaaaaaaa".into())
        );
    }

    /// The two fields answer different questions and must be independently
    /// settable, because in practice they disagree: the checkout moves during a
    /// rebase-and-rerun loop while the binary on disk does not.
    #[test]
    fn checkout_sha_and_binary_provenance_are_separate_facts() {
        let checkout = "affda5d9840baeb60c5f5aa9c7b0ff5560e81ef3";
        let built_from = parse_binary_build_info(&build_info_json("351cd3603f7e-dirty"))
            .expect("typed build record parses");
        assert_ne!(
            checkout,
            built_from.trim_end_matches("-dirty"),
            "the case this field exists for: the harness stood at one commit and ran a \
             binary built from another"
        );
        assert!(
            !checkout.contains("-dirty") && built_from.ends_with("-dirty"),
            "and the checkout cannot express the build tree's dirtiness at all"
        );
    }

    /// REGRESSION CHECK FOR THE ORDINARY-HOST FALLBACK.
    ///
    /// Without the default the guest inherits the HOST's cwd verbatim. Measured on
    /// 2026-08-26 at main f4de43461a, 200 runs rotating across four checkouts:
    ///   default off -> 4 distinct guest `pwd` values
    ///   default on  -> 1, `/tmp/test`, and the directory empty on all 200
    /// The explicit hermetic path below supersedes this with a tmpfs at `/test`.
    /// This fallback remains for direct non-hermetic harness runs.
    #[test]
    fn fixed_workdir_is_bound_by_default_and_withheld_where_it_would_lie() {
        let source = Path::new("/cells/x/workdir/attempt-7");
        let mut argv: Vec<String> = Vec::new();
        append_execution_root_args(&mut argv, "ptrace", None, None, Some(source));
        assert_eq!(
            argv,
            vec![
                "--bind=/cells/x/workdir/attempt-7:/tmp/test".to_string(),
                "--workdir".to_string(),
                "/tmp/test".to_string(),
            ],
            "the fallback must bind a fresh per-attempt directory AND chdir into it"
        );

        let mut argv: Vec<String> = Vec::new();
        append_execution_root_args(&mut argv, "ptrace", None, Some("/tmp"), Some(source));
        assert_eq!(argv, vec!["--workdir".to_string(), "/tmp".to_string()]);

        let mut argv: Vec<String> = Vec::new();
        append_execution_root_args(&mut argv, "ptrace", None, None, None);
        assert!(
            argv.is_empty(),
            "a cell that cannot honour a workdir must be given none, got {argv:?}"
        );
    }

    #[test]
    fn the_hermetic_lane_outranks_manifest_and_fallback_workdirs() {
        let mut argv: Vec<String> = Vec::new();
        append_execution_root_args(
            &mut argv,
            "ptrace",
            Some(Path::new(HERMETIC_TEST_WORKDIR)),
            Some("/tmp"),
            Some(Path::new("/cells/x/workdir/attempt-7")),
        );
        assert_eq!(
            argv,
            vec![
                format!("--mount=type=tmpfs,target={HERMETIC_TEST_WORKDIR}"),
                "--workdir".to_string(),
                HERMETIC_TEST_WORKDIR.to_string(),
            ],
            "the explicit hermetic request must suppress both the manifest override and fallback"
        );
        assert!(
            !argv.iter().any(|arg| arg.starts_with("--bind")),
            "the ordinary-host bind leaked into the /test path: {argv:?}"
        );

        let mut dbt_argv = Vec::new();
        append_execution_root_args(
            &mut dbt_argv,
            "dbt",
            Some(Path::new(HERMETIC_TEST_WORKDIR)),
            None,
            None,
        );
        assert_eq!(
            dbt_argv,
            vec!["--workdir".to_string(), HERMETIC_TEST_WORKDIR.to_string()],
            "DBT's marked physical adapter supplies /test without accepting the generic mount option"
        );
    }

    #[test]
    fn test_workdir_support_matches_the_modes_that_can_honour_it() {
        for mode in ["verify", "replay", "chaos", "custom"] {
            assert!(supports_test_workdir(mode, "ptrace"), "mode={mode}");
            assert!(supports_test_workdir(mode, "dbt"), "mode={mode}");
        }
        assert!(!supports_test_workdir("naked", "native"));
    }

    #[test]
    fn selected_supported_cells_do_not_override_the_hermetic_test_workdir() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let manifests = ManifestSet::load(&root).unwrap();
        let mut overrides = Vec::new();
        for document in &manifests.documents {
            for test in &document.test {
                for (mode, recipe) in &test.modes {
                    let selection = ci_selection(recipe).unwrap();
                    for backend in &recipe.backends_enabled {
                        if selection.selected(backend)
                            && supports_test_workdir(mode, backend)
                            && recipe.workdir.is_some()
                        {
                            overrides.push(format!("{}/{mode}/{backend}", test.id));
                        }
                    }
                }
            }
        }
        assert!(
            overrides.is_empty(),
            "scheduled supported cells must all use the hermetic /test workdir: {overrides:?}"
        );
    }

    #[test]
    fn every_selected_cell_can_honour_the_hermetic_test_workdir() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let expected: JsonValue =
            serde_json::from_slice(&fs::read(root.join("ci/expected-e2e-plan.json")).unwrap())
                .unwrap();
        let unsupported = expected["cells"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|cell| {
                let mode = cell["mode"].as_str().unwrap();
                let backend = cell["backend"].as_str().unwrap();
                (!supports_test_workdir(mode, backend))
                    .then(|| format!("{}/{mode}/{backend}", cell["test"].as_str().unwrap()))
            })
            .collect::<Vec<_>>();
        assert!(
            unsupported.is_empty(),
            "selected cells that cannot honour the hermetic /test gate: {unsupported:?}"
        );
    }

    #[test]
    fn attempt_labels_must_be_one_normal_path_component() {
        for attempt in [
            "",
            ".",
            "..",
            "/absolute",
            "../parent",
            "parent/child",
            "normal/",
        ] {
            assert_eq!(
                fixed_workdir_source_for_attempt(Path::new("/cell"), attempt),
                Err(format!(
                    "invalid attempt label {attempt:?}: expected exactly one normal path component"
                ))
            );
        }
        for attempt in ["1", "attempt-7", "seed--42"] {
            assert_eq!(
                fixed_workdir_source_for_attempt(Path::new("/cell"), attempt),
                Ok(PathBuf::from(format!("/cell/workdir/{attempt}")))
            );
        }
    }

    /// The label is rejected BEFORE anything is created on disk.
    ///
    /// `attempt_labels_must_be_one_normal_path_component` covers the return value
    /// against a path that is never touched, so it cannot see an ordering defect.
    /// If the guard ever moves below the first `create_dir_all`, a traversal label
    /// gets to create a directory outside the cell before being refused, and the
    /// return value alone still looks correct.
    #[test]
    fn an_invalid_attempt_label_is_refused_before_any_filesystem_side_effect() {
        let root = std::env::temp_dir().join(format!(
            "hermit-runner-attempt-label-ordering-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let cell = ptrace_cell("verify");
        let context = run_context(&root);
        let cell_dir = root.join("cell");

        for attempt in ["..", "/absolute", "parent/child"] {
            let error = build_spec(
                &context,
                &cell,
                cell_dir.clone(),
                vec!["/bin/true".into()],
                attempt,
                None,
                cell.timeout_seconds,
            )
            .unwrap_err();
            assert_eq!(
                error,
                format!(
                    "invalid attempt label {attempt:?}: expected exactly one normal path component"
                )
            );
            assert!(
                !cell_dir.exists(),
                "an invalid attempt label must be rejected before filesystem operations"
            );
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn fixed_workdir_bind_is_distinct_for_concurrent_attempts() {
        let root = std::env::temp_dir().join(format!(
            "hermit-runner-concurrent-workdir-bracket-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let cell_dir = root.join("cell");
        fs::create_dir_all(&cell_dir).unwrap();
        let cell = ptrace_cell("verify");
        let context = run_context(&root);
        let source_for = |attempt: &str| {
            let spec = build_spec(
                &context,
                &cell,
                cell_dir.clone(),
                vec!["/bin/true".into()],
                attempt,
                None,
                cell.timeout_seconds,
            )
            .unwrap();
            bound_workdir_source(&spec)
        };
        let sources = [source_for("1"), source_for("2")];
        assert_eq!(sources[0], cell_dir.join("workdir/1"));
        assert_eq!(sources[1], cell_dir.join("workdir/2"));
        assert_ne!(sources[0], sources[1]);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn execute_spec_resets_the_exact_bound_workdir() {
        let root = std::env::temp_dir().join(format!(
            "hermit-runner-workdir-reset-bracket-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let cell_dir = root.join("cell");
        let cell = ptrace_cell("custom");
        let context = run_context(&root);
        let mut spec = build_spec(
            &context,
            &cell,
            cell_dir.clone(),
            vec!["/bin/true".into()],
            "attempt-7",
            None,
            cell.timeout_seconds,
        )
        .unwrap();
        let bound = bound_workdir_source(&spec);
        assert_eq!(bound, cell_dir.join("workdir/attempt-7"));
        assert_eq!(bound, spec.fixed_workdir_source);

        fs::create_dir_all(&bound).unwrap();
        fs::write(bound.join("stale"), b"left by an earlier run").unwrap();
        spec.argv = vec![
            "/bin/sh".into(),
            "-c".into(),
            r#"test -d "$1" && test ! -e "$1/stale" && : > "$1/myfile.txt""#.into(),
            "sh".into(),
            bound.to_string_lossy().into_owned(),
        ];

        let result = execute_spec(&spec).unwrap();
        assert_eq!(result.index, "attempt-7");
        assert_eq!(result.outcome, "PASS", "{}", result.stderr);
        assert!(bound.join("myfile.txt").is_file());
        assert!(!bound.join("stale").exists());
        fs::remove_dir_all(root).unwrap();
    }
}
