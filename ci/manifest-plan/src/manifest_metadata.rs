//! Shared typed projection of the current manifest policy.
//!
//! This is a read-only projection. It deliberately contains no observation or
//! measurement state: those facts belong to the canonical ledgers. Current
//! reproducers use the exact-cell test-harness front door and never call the
//! filesystem-mutating `runner::build_spec` path.

#[cfg(test)]
use std::fs;
use std::path::Path;

use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;

use crate::ci_selection::CiDisabledReasonData;
use crate::ci_selection::CiSelection;
use crate::runner::DirectCommand;
use crate::runner::ManifestInputs;
use crate::runner::ManifestSet;
use crate::runner::ModeRecipe;
use crate::runner::Population;
use crate::runner::SelectedCell;
use crate::runner::Selection;
use crate::runner::TestRecipe;
use crate::timeouts::MANIFEST_SCHEMA;

// Schema 2 requires the independently resolved CPU timeout for every cell.
// Schema 3 exports every custom cell of both source populations, selected by
// full validation or not (see `ManifestMetadata::selected_by_full_custom_commands`).
const EXPORT_SCHEMA: u64 = 3;
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestMetadata {
    pub schema: u64,
    pub manifest_schema: u64,
    pub manifest_sha256: String,
    pub tests: Vec<TestMetadata>,
    pub cells: Vec<CellMetadata>,
    /// Every custom-mode cell in the manifest, from both the enabled and the
    /// disabled populations, each carrying its own `selected_by_full` and
    /// reason fields exactly as `cells` does. Through schema 2 this held only
    /// the custom cells that full validation selects, so a custom cell moved
    /// to `backends_disabled` vanished from the export and the compatibility
    /// site dropped its history. The field keeps its schema-2 name because the
    /// website builder decodes this type through a Cargo path dependency on
    /// whichever Hermit checkout sits beside it; a rename would stop that
    /// builder compiling against one of the two schemas.
    pub selected_by_full_custom_commands: Vec<CellMetadata>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TestMetadata {
    pub id: String,
    pub description: String,
    pub category: String,
    pub lane: String,
    pub requires: Vec<String>,
    pub occasional: bool,
    pub program: Option<String>,
    pub direct: Option<DirectMetadata>,
    pub build: Option<BuildMetadata>,
    pub observation: ObservationMetadata,
    pub preprocessors: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum DirectMetadata {
    Shell { command: String },
    Argv { argv: Vec<String> },
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BuildMetadata {
    pub cflags: Vec<String>,
    pub rustflags: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ObservationMetadata {
    pub status: bool,
    pub stdout: bool,
    pub stderr: bool,
    pub artifacts: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CellMetadata {
    pub test: String,
    pub category: String,
    pub lane: String,
    pub mode: String,
    pub backend: String,
    pub selected_by_full: bool,
    pub not_selected_by_full_reason: Option<CiDisabledReasonData>,
    pub not_applicable_reason: Option<String>,
    pub timeout_seconds: u64,
    pub cpu_timeout_seconds: u64,
    pub guest_args: Vec<String>,
    pub workdir: Option<String>,
    pub current_reproducer: Option<CurrentReproducer>,
    pub current_reproducer_unavailable_reason: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CurrentReproducer {
    pub argv: Vec<String>,
    pub shell_command: String,
}

pub fn build_export(root: &Path) -> Result<ManifestMetadata, String> {
    let inputs = ManifestInputs::read(root)?;
    build_export_from_inputs(root, inputs, || ManifestInputs::read(root))
}

fn build_export_from_inputs(
    root: &Path,
    inputs: ManifestInputs,
    read_current: impl FnOnce() -> Result<ManifestInputs, String>,
) -> Result<ManifestMetadata, String> {
    let manifest_sha256_before = manifest_sha256(&inputs)?;
    let manifests = ManifestSet::from_inputs(root, &inputs)?;
    let selected_by_full_cells = manifests.select(&Selection {
        population: Some(Population::Required),
        ..Selection::default()
    })?;
    let selected_by_full_ids = selected_by_full_cells
        .iter()
        .map(|cell| cell.id.clone())
        .collect::<std::collections::BTreeSet<_>>();
    let tests = manifests
        .all_tests()
        .map(|(category, _, _, test)| test_metadata(category, test))
        .collect();

    let mut cells = Vec::new();
    let mut selected_by_full_custom_commands = Vec::new();
    // Keep both source populations: selection by full validation is a separate
    // fact from whether a cell is present in the manifest. A cell only a
    // focused run type selects (by its label) is still in the manifest. The
    // same rule holds for custom cells, which are exported separately because
    // they are not part of the comparable matrix.
    for population in [Population::Enabled, Population::Disabled] {
        let selected = manifests.select(&Selection {
            population: Some(population),
            include_occasional: true,
            include_manual: true,
            all_run_types: true,
            ..Selection::default()
        })?;
        for cell in selected {
            let metadata = cell_metadata(&cell, selected_by_full_ids.contains(&cell.id))?;
            if cell.id.mode == "custom" {
                selected_by_full_custom_commands.push(metadata);
            } else {
                cells.push(metadata);
            }
        }
    }
    sort_and_require_unique_cells("comparable cells in the manifest", &mut cells)?;
    sort_and_require_unique_cells(
        "custom commands in the manifest",
        &mut selected_by_full_custom_commands,
    )?;
    // Every custom cell full validation selects must still be exported as
    // selected: schema 2 took them from that selection directly.
    for cell in selected_by_full_cells
        .iter()
        .filter(|cell| cell.id.mode == "custom")
    {
        let backend = cell.id.backend.as_deref().unwrap_or("native");
        if !selected_by_full_custom_commands.iter().any(|exported| {
            exported.selected_by_full
                && exported.test == cell.id.test
                && exported.mode == cell.id.mode
                && exported.backend == backend
        }) {
            return Err(format!(
                "{}/{}@{backend} is selected by full validation but absent from the exported custom commands",
                cell.id.test, cell.id.mode
            ));
        }
    }

    let manifest_sha256 =
        require_stable_manifest_sha(manifest_sha256_before, manifest_sha256(&read_current()?)?)?;

    Ok(ManifestMetadata {
        schema: EXPORT_SCHEMA,
        manifest_schema: MANIFEST_SCHEMA,
        manifest_sha256,
        tests,
        cells,
        selected_by_full_custom_commands,
    })
}

fn require_stable_manifest_sha(before: String, after: String) -> Result<String, String> {
    if before == after {
        Ok(before)
    } else {
        Err(format!(
            "manifest inputs changed while they were being read: before={before}, after={after}"
        ))
    }
}

fn test_metadata(category: &str, test: &TestRecipe) -> TestMetadata {
    let direct = test.direct.as_ref().map(|direct| match direct {
        DirectCommand::Shell(command) => DirectMetadata::Shell {
            command: command.clone(),
        },
        DirectCommand::Argv(argv) => DirectMetadata::Argv { argv: argv.clone() },
    });
    let build = test.build.as_ref().map(|build| BuildMetadata {
        cflags: build.cflags.clone(),
        rustflags: build.rustflags.clone(),
    });
    TestMetadata {
        id: test.id.clone(),
        description: test.description.clone(),
        category: category.to_string(),
        lane: test.lane.clone(),
        requires: test.requires.clone(),
        occasional: test.occasional,
        program: test.program.clone(),
        direct,
        build,
        observation: ObservationMetadata {
            status: test.observation.status,
            stdout: test.observation.stdout,
            stderr: test.observation.stderr,
            artifacts: test.observation.artifacts.clone(),
        },
        preprocessors: test.preprocessors.clone(),
    }
}

fn cell_metadata(cell: &SelectedCell, selected_by_full: bool) -> Result<CellMetadata, String> {
    let backend = cell.id.backend.as_deref().unwrap_or("native").to_string();
    let recipe = cell
        .test
        .modes
        .get(&cell.id.mode)
        .ok_or_else(|| format!("{} has no {} mode", cell.id.test, cell.id.mode))?;
    let configured_selection = configured_selection(recipe)
        .map_err(|error| format!("{}: {} {error}", cell.id.test, cell.id.mode))?;
    if selected_by_full && !cell.enabled {
        return Err(format!(
            "{}/{}@{} is selected by full validation but is not applicable",
            cell.id.test, cell.id.mode, backend
        ));
    }
    if selected_by_full && !configured_selection.selected(&backend) {
        return Err(format!(
            "{}/{}@{} is selected by full validation but its manifest selection is false",
            cell.id.test, cell.id.mode, backend
        ));
    }
    if selected_by_full && cell.test.occasional {
        return Err(format!(
            "{}/{}@{} is selected by full validation but its test is marked occasional",
            cell.id.test, cell.id.mode, backend
        ));
    }

    let (not_selected_by_full_reason, not_applicable_reason) = if cell.enabled {
        let reason = if selected_by_full {
            None
        } else if let Some(reason) = configured_selection.reason(&backend) {
            Some(reason.clone())
        } else if let Some(reason) =
            crate::runner::focused_run_type_reason(&cell.test, recipe, Some(&backend))
        {
            Some(CiDisabledReasonData {
                result: None,
                evidence: None,
                reason,
            })
        } else if cell.test.occasional {
            Some(CiDisabledReasonData {
                result: None,
                evidence: None,
                reason: "This test is marked occasional, and full validation does not select occasional tests."
                    .to_string(),
            })
        } else {
            return Err(format!(
                "{}/{}@{} is not selected by full validation without a reason",
                cell.id.test, cell.id.mode, backend
            ));
        };
        (reason, None)
    } else {
        (
            None,
            Some(
                recipe
                    .backends_disabled
                    .get(&backend)
                    .cloned()
                    .ok_or_else(|| {
                        format!(
                            "{}/{}@{} is not applicable without a reason",
                            cell.id.test, cell.id.mode, backend
                        )
                    })?,
            ),
        )
    };
    // The run types the harness's own selection gives this cell; a cell of
    // any but the default one is selected only with `--label`.
    let run_types: Vec<&str> =
        crate::runner::cell_labels(&cell.test, recipe, cell.id.backend.as_deref())
            .into_iter()
            .collect();
    let (current_reproducer, current_reproducer_unavailable_reason) = exact_cell_reproducer(
        &cell.id.test,
        &cell.id.mode,
        &backend,
        cell.enabled,
        &run_types,
    );

    Ok(CellMetadata {
        test: cell.id.test.clone(),
        category: cell.category.clone(),
        lane: cell.test.lane.clone(),
        mode: cell.id.mode.clone(),
        backend: backend.clone(),
        selected_by_full,
        not_selected_by_full_reason,
        not_applicable_reason,
        timeout_seconds: cell.timeout_seconds,
        cpu_timeout_seconds: cell.cpu_timeout_seconds,
        guest_args: crate::runner::cell_guest_args(cell, &backend),
        workdir: crate::runner::cell_workdir(cell),
        current_reproducer,
        current_reproducer_unavailable_reason,
    })
}

fn configured_selection(recipe: &ModeRecipe) -> Result<CiSelection, String> {
    CiSelection::validate(
        &recipe.backends_enabled.iter().cloned().collect(),
        &recipe.backends_disabled.keys().cloned().collect(),
        &recipe.ci,
        recipe.ci_disabled_reason.as_ref(),
    )
}

/// The exact-cell `test-harness run` command for one cell. `run_types` are the
/// cell's run types ([`crate::runner::cell_labels`]); a selection without
/// `--label` admits only [`crate::runner::DEFAULT_RUN_TYPE`] cells, so a cell
/// of any other run type adds `--label` naming its run types.
fn exact_cell_reproducer(
    test: &str,
    mode: &str,
    backend: &str,
    applicable: bool,
    run_types: &[&str],
) -> (Option<CurrentReproducer>, Option<String>) {
    if !applicable && mode == "naked" && backend == "native" {
        return (
            None,
            Some(
                "test-harness --probe-disabled requires --backend, but its backend selector does not accept native"
                    .to_string(),
            ),
        );
    }

    let mut argv = vec![
        "target/debug/test-harness".to_string(),
        "run".to_string(),
        if applicable {
            "--include-manual".to_string()
        } else {
            "--probe-disabled".to_string()
        },
        "--include-occasional".to_string(),
        "--test".to_string(),
        test.to_string(),
        "--mode".to_string(),
        mode.to_string(),
    ];
    if backend != "native" {
        argv.extend(["--backend".to_string(), backend.to_string()]);
    }
    if !run_types.contains(&crate::runner::DEFAULT_RUN_TYPE) && !run_types.is_empty() {
        argv.extend(["--label".to_string(), run_types.join(",")]);
    }
    let shell_command = argv
        .iter()
        .map(|argument| shell_quote(argument))
        .collect::<Vec<_>>()
        .join(" ");
    (
        Some(CurrentReproducer {
            argv,
            shell_command,
        }),
        None,
    )
}

fn shell_quote(value: &str) -> String {
    if !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_@%+=:,./-".contains(&byte))
    {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', "'\"'\"'"))
    }
}

fn sort_and_require_unique_cells(label: &str, cells: &mut [CellMetadata]) -> Result<(), String> {
    cells.sort_by(|left, right| cell_key(left).cmp(&cell_key(right)));
    for pair in cells.windows(2) {
        if cell_key(&pair[0]) == cell_key(&pair[1]) {
            return Err(format!(
                "{label} contains duplicate identity {}/{}/{}@{}",
                pair[0].category, pair[0].test, pair[0].mode, pair[0].backend
            ));
        }
    }
    Ok(())
}

fn cell_key(cell: &CellMetadata) -> (&str, &str, &str, &str, &str) {
    (
        &cell.lane,
        &cell.category,
        &cell.test,
        &cell.mode,
        &cell.backend,
    )
}

fn manifest_sha256(inputs: &ManifestInputs) -> Result<String, String> {
    let mut digest = Sha256::new();
    digest.update(b"hermit-manifest-metadata-v1\0");
    for (path, source) in inputs.named_sources() {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| format!("manifest name is not UTF-8: {}", path.display()))?;
        let contents = source.as_bytes();
        digest.update(
            u64::try_from(name.len())
                .map_err(|_| "manifest name length does not fit u64".to_string())?
                .to_le_bytes(),
        );
        digest.update(name.as_bytes());
        digest.update(
            u64::try_from(contents.len())
                .map_err(|_| "manifest length does not fit u64".to_string())?
                .to_le_bytes(),
        );
        digest.update(contents);
    }
    Ok(format!("{:x}", digest.finalize()))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::collections::BTreeSet;
    use std::path::PathBuf;

    use serde_json::Value;

    use super::*;

    fn root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .unwrap()
    }

    // Inspect absolute path references within field values, not substrings of
    // relative paths such as hermit-cli/src/bin/hermit/run.rs. Delimiters also
    // cover quoted prose/shell paths, flag assignments, linker comma arguments
    // and file:line citations. Recognize the finite compiler path options below
    // only when the option itself begins a token; a relative path containing
    // -I/src is not one. This does not parse arbitrary shell/compiler syntax.
    fn starts_path_token(before: &str) -> bool {
        before.chars().next_back().is_none_or(|c| {
            c.is_whitespace() || matches!(c, '\'' | '"' | '`' | '=' | ':' | ',' | '(' | '[' | '{')
        })
    }

    fn contains_absolute_root_reference(value: &Value, root: &str) -> bool {
        match value {
            Value::String(text) => text.match_indices(root).any(|(offset, _)| {
                let before = &text[..offset];
                let after = text[offset + root.len()..].chars().next();
                let starts_path = starts_path_token(before)
                    || [
                        "-I",
                        "-L",
                        "-B",
                        "-isystem",
                        "-iquote",
                        "-idirafter",
                        "-include",
                        "-imacros",
                        "-isysroot",
                        "-iprefix",
                        "-o",
                        "-MF",
                    ]
                    .iter()
                    .any(|&option| before.strip_suffix(option).is_some_and(starts_path_token));
                let ends_component = after.is_none_or(|c| {
                    c.is_whitespace()
                        || matches!(
                            c,
                            '/' | '\'' | '"' | '`' | '=' | ':' | ',' | ';' | ')' | ']' | '}'
                        )
                });
                starts_path && ends_component
            }),
            Value::Array(values) => values
                .iter()
                .any(|value| contains_absolute_root_reference(value, root)),
            Value::Object(fields) => fields
                .values()
                .any(|value| contains_absolute_root_reference(value, root)),
            _ => false,
        }
    }

    fn require_root_independent_exports(exports: &[String], roots: &[&Path]) -> Result<(), String> {
        let first = exports.first().ok_or("missing exported metadata")?;
        if exports.iter().any(|export| export != first) {
            return Err("exports differ across repository roots".into());
        }
        let value: Value = serde_json::from_str(first).unwrap();
        for root in roots {
            assert!(root.is_absolute());
            if contains_absolute_root_reference(&value, root.to_str().unwrap()) {
                return Err(format!(
                    "absolute checkout-root reference: {}",
                    root.display()
                ));
            }
        }
        Ok(())
    }

    #[test]
    fn nested_alias_sibling_remains_an_absolute_checkout_root_reference() {
        // Model TMPDIR inside the checkout regardless of this test process's
        // environment. A component-boundary sibling of one alias still lies
        // inside the checkout and must fail the complete-root oracle.
        let checkout = Path::new("/checkout");
        let first_alias = Path::new("/checkout/run-state/tmp/first-root");
        let second_alias = Path::new("/checkout/run-state/tmp/second-root");
        let checked_roots = [checkout, first_alias, second_alias];
        for field in [
            "/tests/1/build/cflags/0",
            "/tests/1/build/rustflags/0",
            "/tests/1/direct/argv/1",
            "/cells/0/current_reproducer/argv/1",
            "/cells/0/current_reproducer/shell_command",
        ] {
            let mut mapped = serde_json::to_value(contract_fixture()).unwrap();
            *mapped.pointer_mut(field).unwrap() = Value::String(format!(
                "-fdebug-prefix-map={}-sibling=/mapped",
                first_alias.display()
            ));
            let exports = vec![serde_json::to_string(&mapped).unwrap(); checked_roots.len()];
            require_root_independent_exports(&exports, &[first_alias]).unwrap();
            let error = require_root_independent_exports(&exports, &checked_roots).unwrap_err();
            assert_eq!(
                error, "absolute checkout-root reference: /checkout",
                "{field}"
            );
        }
    }

    fn identity(cell: &CellMetadata) -> (String, String, String, String, String) {
        (
            cell.lane.clone(),
            cell.category.clone(),
            cell.test.clone(),
            cell.mode.clone(),
            cell.backend.clone(),
        )
    }

    fn contract_fixture() -> ManifestMetadata {
        ManifestMetadata {
            schema: EXPORT_SCHEMA,
            manifest_schema: 3,
            manifest_sha256: "abc".into(),
            tests: vec![
                TestMetadata {
                    id: "shell".into(),
                    description: "shell command".into(),
                    category: "fixture".into(),
                    lane: "portable".into(),
                    requires: vec!["kvm".into()],
                    occasional: false,
                    program: None,
                    direct: Some(DirectMetadata::Shell {
                        command: "true".into(),
                    }),
                    build: None,
                    observation: ObservationMetadata {
                        status: true,
                        stdout: false,
                        stderr: true,
                        artifacts: vec!["result.txt".into()],
                    },
                    preprocessors: Vec::new(),
                },
                TestMetadata {
                    id: "argv".into(),
                    description: "argv command".into(),
                    category: "fixture".into(),
                    lane: "portable".into(),
                    requires: Vec::new(),
                    occasional: true,
                    program: Some("fixture-bin".into()),
                    direct: Some(DirectMetadata::Argv {
                        argv: vec!["fixture-bin".into(), "--flag".into()],
                    }),
                    build: Some(BuildMetadata {
                        cflags: vec!["-O2".into()],
                        rustflags: vec!["-Copt-level=2".into()],
                    }),
                    observation: ObservationMetadata {
                        status: false,
                        stdout: true,
                        stderr: false,
                        artifacts: Vec::new(),
                    },
                    preprocessors: vec!["e9patch".into()],
                },
            ],
            cells: vec![
                CellMetadata {
                    test: "shell".into(),
                    category: "fixture".into(),
                    lane: "portable".into(),
                    mode: "verify".into(),
                    backend: "ptrace".into(),
                    selected_by_full: false,
                    not_selected_by_full_reason: Some(CiDisabledReasonData {
                        result: Some(crate::ci_selection::CiDisabledResult::Unavailable),
                        evidence: Some("fixture-evidence".into()),
                        reason: "fixture reason".into(),
                    }),
                    not_applicable_reason: None,
                    timeout_seconds: 15,
                    cpu_timeout_seconds: 7,
                    guest_args: vec!["--guest".into(), String::new()],
                    workdir: Some("fixture-workdir".into()),
                    current_reproducer: Some(CurrentReproducer {
                        argv: vec!["test-harness".into(), "run".into()],
                        shell_command: "test-harness run".into(),
                    }),
                    current_reproducer_unavailable_reason: None,
                },
                CellMetadata {
                    test: "argv".into(),
                    category: "fixture".into(),
                    lane: "portable".into(),
                    mode: "naked".into(),
                    backend: "native".into(),
                    selected_by_full: false,
                    not_selected_by_full_reason: None,
                    not_applicable_reason: Some("not applicable".into()),
                    timeout_seconds: 30,
                    cpu_timeout_seconds: 11,
                    guest_args: Vec::new(),
                    workdir: None,
                    current_reproducer: None,
                    current_reproducer_unavailable_reason: Some("no reproducer".into()),
                },
            ],
            selected_by_full_custom_commands: Vec::new(),
        }
    }

    struct TemporaryManifestRoot(PathBuf);

    impl TemporaryManifestRoot {
        fn alias_of(root: &Path, name: &str) -> Self {
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let alias = std::env::temp_dir().join(format!(
                "hermit-manifest-metadata-{name}-{}-{nonce}",
                std::process::id()
            ));
            fs::create_dir(&alias).unwrap();
            let alias = Self(alias);
            std::os::unix::fs::symlink(root.join("tests"), alias.path().join("tests")).unwrap();
            alias
        }

        fn new(name: &str, manifest: &str) -> Self {
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = std::env::temp_dir().join(format!(
                "hermit-manifest-metadata-{name}-{}-{nonce}",
                std::process::id()
            ));
            let directory = root.join("tests/e2e/manifests");
            fs::create_dir_all(&directory).unwrap();
            fs::write(
                directory.join("defaults.yaml"),
                "schema: 3\ntimeout_seconds: 15\ncpu_timeout_seconds: 7\nnextest: []\n",
            )
            .unwrap();
            fs::write(directory.join(format!("{name}.yaml")), manifest).unwrap();
            Self(root)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TemporaryManifestRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    const OCCASIONAL_MANIFEST: &str = r#"schema: 3
bucket: occasional
test:
  - id: occasional/full-selection
    description: Fixture proving that full validation excludes occasional tests
    lane: portable
    requires: []
    occasional: true
    direct:
      - /bin/true
    observation:
      status: true
      stdout: false
      stderr: false
      artifacts: []
    modes:
      verify:
        ci: true
        backends_enabled: [ptrace]
        backends_disabled: {dbt: Not applicable in this fixture, kvm: Not applicable in this fixture, sabre: Not applicable in this fixture, liteinst: Not applicable in this fixture, in-guest-trap: Not applicable in this fixture}
      naked:
        ci: false
        backends_enabled: []
        backends_disabled: {native: Not applicable in this fixture}
      replay:
        ci: false
        backends_enabled: []
        backends_disabled: {ptrace: Not applicable in this fixture, dbt: Not applicable in this fixture, kvm: Not applicable in this fixture, sabre: Not applicable in this fixture, liteinst: Not applicable in this fixture, in-guest-trap: Not applicable in this fixture}
      chaos:
        ci: false
        backends_enabled: []
        backends_disabled: {ptrace: Not applicable in this fixture, dbt: Not applicable in this fixture, kvm: Not applicable in this fixture, sabre: Not applicable in this fixture, liteinst: Not applicable in this fixture, in-guest-trap: Not applicable in this fixture}
      custom:
        ci: true
        backends_enabled: [ptrace]
        backends_disabled: {dbt: Not applicable in this fixture, kvm: Not applicable in this fixture, sabre: Not applicable in this fixture, liteinst: Not applicable in this fixture, in-guest-trap: Not applicable in this fixture}
"#;

    #[test]
    fn occasional_cells_remain_in_the_manifest_but_full_does_not_select_them() {
        let fixture = TemporaryManifestRoot::new("occasional", OCCASIONAL_MANIFEST);
        let manifests = ManifestSet::load(fixture.path()).unwrap();
        let cells_in_manifest = manifests
            .select(&Selection {
                population: Some(Population::Enabled),
                include_occasional: true,
                include_manual: true,
                ..Selection::default()
            })
            .unwrap();
        assert!(cells_in_manifest.iter().any(|cell| {
            cell.id.mode == "verify" && cell.id.backend.as_deref() == Some("ptrace")
        }));
        assert!(cells_in_manifest.iter().any(|cell| {
            cell.id.mode == "custom" && cell.id.backend.as_deref() == Some("ptrace")
        }));
        assert!(
            manifests
                .select(&Selection {
                    population: Some(Population::Required),
                    ..Selection::default()
                })
                .unwrap()
                .is_empty()
        );

        let export = build_export(fixture.path()).unwrap();
        let verify = export
            .cells
            .iter()
            .find(|cell| {
                cell.test == "occasional/full-selection"
                    && cell.mode == "verify"
                    && cell.backend == "ptrace"
            })
            .unwrap();
        assert!(!verify.selected_by_full);
        assert_eq!(
            verify
                .not_selected_by_full_reason
                .as_ref()
                .map(|reason| reason.reason.as_str()),
            Some(
                "This test is marked occasional, and full validation does not select occasional tests."
            )
        );
        assert!(verify.not_applicable_reason.is_none());

        // Schema 3 exports a custom cell whether or not full validation
        // selects it (schema 2 exported only selected ones, so this list was
        // empty): the enabled ptrace cell with the occasional reason and the
        // five disabled backends with their not-applicable reasons.
        let custom = &export.selected_by_full_custom_commands;
        assert_eq!(custom.len(), 6);
        assert!(custom.iter().all(|cell| {
            cell.test == "occasional/full-selection"
                && cell.mode == "custom"
                && !cell.selected_by_full
        }));
        let enabled = custom.iter().find(|cell| cell.backend == "ptrace").unwrap();
        assert_eq!(
            enabled
                .not_selected_by_full_reason
                .as_ref()
                .map(|reason| reason.reason.as_str()),
            Some(
                "This test is marked occasional, and full validation does not select occasional tests."
            )
        );
        assert!(enabled.not_applicable_reason.is_none());
        for backend in ["dbt", "kvm", "sabre", "liteinst"] {
            let disabled = custom.iter().find(|cell| cell.backend == backend).unwrap();
            assert!(disabled.not_selected_by_full_reason.is_none());
            assert_eq!(
                disabled.not_applicable_reason.as_deref(),
                Some("Not applicable in this fixture")
            );
        }
    }

    #[test]
    fn current_reproducer_uses_only_the_exact_harness_front_door() {
        let (applicable, missing) =
            exact_cell_reproducer("bucket/test", "verify", "ptrace", true, &["full"]);
        assert!(missing.is_none());
        let applicable = applicable.unwrap();
        assert_eq!(applicable.argv[0], "target/debug/test-harness");
        assert!(applicable.argv.iter().any(|arg| arg == "--include-manual"));
        assert!(!applicable.argv.iter().any(|arg| arg == "--probe-disabled"));
        assert!(
            applicable
                .argv
                .windows(2)
                .any(|args| args == ["--test", "bucket/test"])
        );
        assert!(
            applicable
                .argv
                .windows(2)
                .any(|args| args == ["--mode", "verify"])
        );
        assert!(
            applicable
                .argv
                .windows(2)
                .any(|args| args == ["--backend", "ptrace"])
        );

        let (not_applicable, missing) =
            exact_cell_reproducer("bucket/test", "verify", "sabre", false, &["full"]);
        assert!(missing.is_none());
        let not_applicable = not_applicable.unwrap();
        assert!(
            not_applicable
                .argv
                .iter()
                .any(|arg| arg == "--probe-disabled")
        );
        assert!(
            !not_applicable
                .argv
                .iter()
                .any(|arg| arg == "--include-manual")
        );

        let (native, missing) =
            exact_cell_reproducer("bucket/test", "naked", "native", true, &["full"]);
        assert!(missing.is_none());
        assert!(!native.unwrap().argv.iter().any(|arg| arg == "--backend"));

        let (not_applicable_native, missing) =
            exact_cell_reproducer("bucket/test", "naked", "native", false, &["full"]);
        assert!(not_applicable_native.is_none());
        assert!(missing.unwrap().contains("does not accept native"));
    }

    #[test]
    fn shell_command_quotes_untrusted_arguments() {
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("one'word"), "'one'\"'\"'word'");
        let (reproducer, missing) = exact_cell_reproducer(
            "bucket/test with space",
            "verify",
            "ptrace",
            true,
            &["full"],
        );
        assert!(missing.is_none());
        assert!(
            reproducer
                .unwrap()
                .shell_command
                .contains("'bucket/test with space'")
        );
    }

    #[test]
    fn manifest_digest_must_bound_one_stable_read() {
        assert_eq!(
            require_stable_manifest_sha("same".into(), "same".into()).unwrap(),
            "same"
        );
        let error = require_stable_manifest_sha("before".into(), "after".into()).unwrap_err();
        assert!(error.contains("changed while they were being read"));
        assert!(error.contains("before=before"));
        assert!(error.contains("after=after"));
    }

    #[test]
    fn backend_env_values_affect_manifest_digest_with_guest_digest_scope_unchanged() {
        let source = OCCASIONAL_MANIFEST.replace(
            "      verify:\n        ci: true\n",
            "      verify:\n        ci: true\n        env_by_backend: {ptrace: {FIXTURE_CELL: 'one'}}\n",
        );
        let fixture = TemporaryManifestRoot::new("occasional", &source);
        let before = ManifestInputs::read(fixture.path()).unwrap();
        let old = ManifestSet::from_inputs(fixture.path(), &before).unwrap();
        fs::write(
            fixture.path().join("tests/e2e/manifests/occasional.yaml"),
            source.replace("FIXTURE_CELL: 'one'", "FIXTURE_CELL: 'two'"),
        )
        .unwrap();
        let after = ManifestInputs::read(fixture.path()).unwrap();
        let new = ManifestSet::from_inputs(fixture.path(), &after).unwrap();
        assert_ne!(
            manifest_sha256(&before).unwrap(),
            manifest_sha256(&after).unwrap()
        );
        assert_eq!(
            crate::runner::test_digest(fixture.path(), old.all_tests().next().unwrap().3).unwrap(),
            crate::runner::test_digest(fixture.path(), new.all_tests().next().unwrap().3).unwrap()
        );
    }

    #[test]
    fn metadata_and_digest_use_the_same_captured_inputs_during_an_aba_change() {
        let fixture = TemporaryManifestRoot::new("occasional", OCCASIONAL_MANIFEST);
        let path = fixture.path().join("tests/e2e/manifests/occasional.yaml");
        let inputs = ManifestInputs::read(fixture.path()).unwrap();
        let expected_digest = manifest_sha256(&inputs).unwrap();
        let changed = OCCASIONAL_MANIFEST.replace(
            "Fixture proving that full validation excludes occasional tests",
            "Different valid revision present during projection",
        );
        assert_ne!(changed, OCCASIONAL_MANIFEST);
        fs::write(&path, &changed).unwrap();
        // The final directory contents agree with the original capture. A
        // live parser between those two reads would nevertheless consume B.
        let export = build_export_from_inputs(fixture.path(), inputs, || {
            fs::write(&path, OCCASIONAL_MANIFEST).unwrap();
            ManifestInputs::read(fixture.path())
        })
        .unwrap();
        assert_eq!(export.manifest_sha256, expected_digest);
        assert_eq!(export.tests.len(), 1);
        assert_eq!(
            export.tests[0].description,
            "Fixture proving that full validation excludes occasional tests"
        );
        assert_eq!(fs::read_to_string(path).unwrap(), OCCASIONAL_MANIFEST);
    }

    #[test]
    fn captured_metadata_still_refuses_inputs_changed_at_the_final_read() {
        let fixture = TemporaryManifestRoot::new("occasional", OCCASIONAL_MANIFEST);
        let inputs = ManifestInputs::read(fixture.path()).unwrap();
        let path = fixture.path().join("tests/e2e/manifests/occasional.yaml");
        let changed = OCCASIONAL_MANIFEST.replace(
            "Fixture proving that full validation excludes occasional tests",
            "Different valid revision remaining at the final read",
        );
        assert_ne!(changed, OCCASIONAL_MANIFEST);
        let error = build_export_from_inputs(fixture.path(), inputs, || {
            fs::write(&path, &changed).unwrap();
            ManifestInputs::read(fixture.path())
        })
        .unwrap_err();
        assert!(error.contains("manifest inputs changed while they were being read"));
    }

    #[test]
    fn metadata_preserves_inherited_and_overridden_cpu_and_wall_limits() {
        let source = OCCASIONAL_MANIFEST
            .replace(
                "backends_enabled: [ptrace]",
                "backends_enabled: [ptrace, sabre]\n        timeout_seconds: {ptrace: 21}\n        cpu_timeout_seconds: {ptrace: 9}\n        slow_reason: {ptrace: Explicit fixture override}",
            )
            .replace(
                "backends_disabled: {dbt: Not applicable in this fixture, kvm: Not applicable in this fixture, sabre: Not applicable in this fixture, liteinst: Not applicable in this fixture, in-guest-trap: Not applicable in this fixture}",
                "backends_disabled: {dbt: Not applicable in this fixture, kvm: Not applicable in this fixture, liteinst: Not applicable in this fixture, in-guest-trap: Not applicable in this fixture}",
            );
        for inherited_wall in [15, 19] {
            let source = if inherited_wall == 15 {
                source.clone()
            } else {
                source.replace(
                    "bucket: occasional\n",
                    "bucket: occasional\ntimeout_seconds: 19\nslow_reason: Explicit bucket fixture override\n",
                )
            };
            let fixture = TemporaryManifestRoot::new("occasional", &source);
            let export = build_export(fixture.path()).unwrap();
            assert_eq!(export.schema, 3);
            let limits = export
                .cells
                .iter()
                .filter(|cell| cell.mode == "verify")
                .map(|cell| {
                    (
                        cell.backend.as_str(),
                        (cell.timeout_seconds, cell.cpu_timeout_seconds),
                    )
                })
                .collect::<BTreeMap<_, _>>();
            assert_eq!(
                limits,
                BTreeMap::from([
                    ("dbt", (inherited_wall, 7)),
                    ("in-guest-trap", (inherited_wall, 7)),
                    ("kvm", (inherited_wall, 7)),
                    ("liteinst", (inherited_wall, 7)),
                    ("ptrace", (21, 9)),
                    ("sabre", (inherited_wall, 7)),
                ])
            );
        }
    }

    #[test]
    fn metadata_contract_has_stable_json_and_round_trips() {
        let encoded = serde_json::to_string(&contract_fixture()).unwrap();
        let expected = concat!(
            r#"{"schema":3,"manifest_schema":3,"manifest_sha256":"abc","tests":["#,
            r#"{"id":"shell","description":"shell command","category":"fixture","lane":"portable","requires":["kvm"],"occasional":false,"program":null,"direct":{"kind":"shell","command":"true"},"build":null,"observation":{"status":true,"stdout":false,"stderr":true,"artifacts":["result.txt"]},"preprocessors":[]},"#,
            r#"{"id":"argv","description":"argv command","category":"fixture","lane":"portable","requires":[],"occasional":true,"program":"fixture-bin","direct":{"kind":"argv","argv":["fixture-bin","--flag"]},"build":{"cflags":["-O2"],"rustflags":["-Copt-level=2"]},"observation":{"status":false,"stdout":true,"stderr":false,"artifacts":[]},"preprocessors":["e9patch"]}],"#,
            r#""cells":[{"test":"shell","category":"fixture","lane":"portable","mode":"verify","backend":"ptrace","selected_by_full":false,"not_selected_by_full_reason":{"result":"unavailable","evidence":"fixture-evidence","reason":"fixture reason"},"not_applicable_reason":null,"timeout_seconds":15,"cpu_timeout_seconds":7,"guest_args":["--guest",""],"workdir":"fixture-workdir","current_reproducer":{"argv":["test-harness","run"],"shell_command":"test-harness run"},"current_reproducer_unavailable_reason":null},"#,
            r#"{"test":"argv","category":"fixture","lane":"portable","mode":"naked","backend":"native","selected_by_full":false,"not_selected_by_full_reason":null,"not_applicable_reason":"not applicable","timeout_seconds":30,"cpu_timeout_seconds":11,"guest_args":[],"workdir":null,"current_reproducer":null,"current_reproducer_unavailable_reason":"no reproducer"}],"#,
            r#""selected_by_full_custom_commands":[]}"#,
        );
        assert_eq!(encoded, expected);

        let decoded: ManifestMetadata = serde_json::from_str(&encoded).unwrap();
        assert_eq!(serde_json::to_string(&decoded).unwrap(), encoded);
    }

    #[test]
    fn metadata_deserialization_refuses_unknown_and_missing_fields() {
        let mut unknown = serde_json::to_value(contract_fixture()).unwrap();
        unknown
            .as_object_mut()
            .unwrap()
            .insert("unexpected".into(), Value::Bool(true));
        assert!(serde_json::from_value::<ManifestMetadata>(unknown).is_err());

        let mut missing = serde_json::to_value(contract_fixture()).unwrap();
        missing.as_object_mut().unwrap().remove("schema");
        assert!(serde_json::from_value::<ManifestMetadata>(missing).is_err());

        let mut nested_unknown = serde_json::to_value(contract_fixture()).unwrap();
        nested_unknown["tests"][0]
            .as_object_mut()
            .unwrap()
            .insert("unexpected".into(), Value::Bool(true));
        assert!(serde_json::from_value::<ManifestMetadata>(nested_unknown).is_err());

        let mut direct_unknown = serde_json::to_value(contract_fixture()).unwrap();
        direct_unknown["tests"][0]["direct"]
            .as_object_mut()
            .unwrap()
            .insert("unexpected".into(), Value::Bool(true));
        assert!(serde_json::from_value::<ManifestMetadata>(direct_unknown).is_err());

        let mut nested_missing = serde_json::to_value(contract_fixture()).unwrap();
        nested_missing["tests"][0]["observation"]
            .as_object_mut()
            .unwrap()
            .remove("status");
        assert!(serde_json::from_value::<ManifestMetadata>(nested_missing).is_err());

        for field in ["timeout_seconds", "cpu_timeout_seconds"] {
            let mut missing_limit = serde_json::to_value(contract_fixture()).unwrap();
            missing_limit["cells"][0]
                .as_object_mut()
                .unwrap()
                .remove(field);
            assert!(
                serde_json::from_value::<ManifestMetadata>(missing_limit).is_err(),
                "schema 2 must require {field}"
            );
        }
    }

    #[test]
    fn duplicate_cell_identity_is_refused() {
        let mut first = contract_fixture();
        let mut second = contract_fixture();
        let mut cells = vec![first.cells.remove(0), second.cells.remove(0)];
        let error = sort_and_require_unique_cells("fixture", &mut cells).unwrap_err();
        assert!(error.contains("duplicate identity fixture/shell/verify@ptrace"));
    }

    #[test]
    fn binary_is_a_thin_caller_without_duplicate_metadata_types() {
        let binary = include_str!("bin/manifest-metadata.rs");
        assert!(binary.contains("manifest_metadata::build_export"));
        for declaration in [
            "struct ManifestMetadata",
            "struct TestMetadata",
            "enum DirectMetadata",
            "struct BuildMetadata",
            "struct ObservationMetadata",
            "struct CellMetadata",
            "struct CurrentReproducer",
            "fn require_stable_manifest_sha",
            "fn test_metadata",
            "fn cell_metadata",
            "fn configured_selection",
            "fn exact_cell_reproducer",
            "fn shell_quote",
            "fn sort_and_require_unique_cells",
            "fn cell_key",
            "fn manifest_sha256",
            "mod tests",
        ] {
            assert!(!binary.contains(declaration), "duplicate {declaration}");
        }
    }

    #[test]
    fn shipped_export_is_deterministic_and_matches_the_comparable_projection() {
        let root = root();
        let first = build_export(&root).unwrap();
        let second = build_export(&root).unwrap();
        assert_eq!(
            serde_json::to_vec(&first).unwrap(),
            serde_json::to_vec(&second).unwrap()
        );

        let test_ids = first
            .tests
            .iter()
            .map(|test| test.id.as_str())
            .collect::<BTreeSet<_>>();
        assert_eq!(test_ids.len(), first.tests.len());

        let mut per_test_modes = BTreeMap::<&str, BTreeMap<&str, usize>>::new();
        for cell in &first.cells {
            *per_test_modes
                .entry(&cell.test)
                .or_default()
                .entry(&cell.mode)
                .or_default() += 1;
        }
        assert_eq!(per_test_modes.len(), first.tests.len());
        for (test, modes) in per_test_modes {
            assert_eq!(
                modes,
                BTreeMap::from([("chaos", 6), ("naked", 1), ("replay", 6), ("verify", 6),]),
                "wrong comparable matrix for {test}"
            );
        }

        let tracked: Value =
            serde_json::from_slice(&fs::read(root.join("ci/compat-envelope/cells.json")).unwrap())
                .unwrap();
        let tracked = tracked["cells"].as_array().unwrap();
        let tracked_identities = tracked
            .iter()
            .map(|cell| {
                (
                    cell["lane"].as_str().unwrap().to_string(),
                    cell["category"].as_str().unwrap().to_string(),
                    cell["test"].as_str().unwrap().to_string(),
                    cell["mode"].as_str().unwrap().to_string(),
                    cell["backend"].as_str().unwrap().to_string(),
                )
            })
            .collect::<BTreeSet<_>>();
        let exported_identities = first.cells.iter().map(identity).collect::<BTreeSet<_>>();
        assert_eq!(exported_identities, tracked_identities);

        let encoded = serde_json::to_string(&first).unwrap();
        // A relative authored citation can contain "/src", the pinned checkout
        // root. Preserve it while refusing genuine absolute checkout paths. No
        // shipped cell has to cite one (the last did until its cell was
        // switched on), so the oracle below also runs over the exports with one
        // authored into a cell's not-selected evidence, identically in each.
        let citation = "hermit-cli/src/bin/hermit/run.rs:2163";
        let first_alias = TemporaryManifestRoot::alias_of(&root, "first-root");
        let second_alias = TemporaryManifestRoot::alias_of(&root, "second-root");
        let roots = [root.as_path(), first_alias.path(), second_alias.path()];
        let mut exports = vec![encoded.clone()];
        for alias in [&first_alias, &second_alias] {
            let exported = build_export(alias.path()).unwrap();
            exports.push(serde_json::to_string(&exported).unwrap());
        }
        require_root_independent_exports(&exports, &roots).unwrap();
        let cited = exports
            .iter()
            .map(|export| {
                let mut value: Value = serde_json::from_str(export).unwrap();
                let evidence = value["cells"]
                    .as_array_mut()
                    .unwrap()
                    .iter_mut()
                    .find_map(|cell| {
                        cell.pointer_mut("/not_selected_by_full_reason/evidence")
                            .filter(|evidence| evidence.is_string())
                    })
                    .expect("a shipped cell carries not-selected evidence to cite in");
                *evidence = Value::String(citation.into());
                serde_json::to_string(&value).unwrap()
            })
            .collect::<Vec<_>>();
        assert!(cited.iter().all(|export| export.contains(citation)));
        require_root_independent_exports(&cited, &roots).unwrap();

        // Run the same oracle against a leak shared by every variant: alias
        // equality alone would miss a canonicalized or compile-time real root.
        // /src is also checked without exempting the authored relative citation.
        for leaked_root in roots.into_iter().chain([Path::new("/src")]) {
            let checked_roots = [
                root.as_path(),
                first_alias.path(),
                second_alias.path(),
                leaked_root,
            ];
            require_root_independent_exports(&exports, &checked_roots).unwrap();
            require_root_independent_exports(&cited, &checked_roots).unwrap();

            // Exercise each exported flag/argument surface through the same
            // oracle, including uniform leaks that root equality cannot catch.
            let mut option_fixture = serde_json::to_value(contract_fixture()).unwrap();
            option_fixture["cells"][0]["not_selected_by_full_reason"]["evidence"] =
                Value::String(citation.into());
            for field in [
                "/tests/1/build/cflags/0",
                "/tests/1/build/rustflags/0",
                "/tests/1/direct/argv/1",
                "/cells/0/current_reproducer/argv/1",
                "/cells/0/current_reproducer/shell_command",
            ] {
                for (option, suffix) in [
                    ("-I", "include"),
                    ("-L", "lib"),
                    ("-B", "bin"),
                    ("-isystem", "include"),
                    ("-iquote", "include"),
                    ("-idirafter", "include"),
                    ("-include", "header"),
                    ("-imacros", "header"),
                    ("-isysroot", "root"),
                    ("-iprefix", "include"),
                    ("-o", "output"),
                    ("-MF", "dependencies"),
                    ("-Wl,-rpath,", "lib"),
                ] {
                    for relative in [
                        format!("{option}hermit-cli/src/{suffix}"),
                        format!("{option}relative-I/src/{suffix}"),
                        format!("{option}/bin/{suffix}"),
                    ] {
                        let mut permitted = option_fixture.clone();
                        *permitted.pointer_mut(field).unwrap() = Value::String(relative);
                        let permitted = serde_json::to_string(&permitted).unwrap();
                        require_root_independent_exports(
                            &vec![permitted; exports.len()],
                            &checked_roots,
                        )
                        .unwrap();
                    }
                    let mut leaked = option_fixture.clone();
                    *leaked.pointer_mut(field).unwrap() =
                        Value::String(format!("{option}{}/{suffix}", leaked_root.display()));
                    let leaked = serde_json::to_string(&leaked).unwrap();
                    let error = require_root_independent_exports(
                        &vec![leaked; exports.len()],
                        &checked_roots,
                    )
                    .unwrap_err();
                    assert!(
                        error.contains("absolute checkout-root reference"),
                        "{field}: {option}: {error}"
                    );
                }
                // A path mapping can end at '=' instead of a slash. Preserve
                // relative operands and sibling paths sharing only the prefix.
                for permitted in [
                    "-fdebug-prefix-map=hermit-cli/src=/mapped".to_string(),
                    format!(
                        "-fdebug-prefix-map={}-sibling=/mapped",
                        leaked_root.display()
                    ),
                ] {
                    let relative_only = permitted.starts_with("-fdebug-prefix-map=hermit-cli/");
                    let mut mapped = option_fixture.clone();
                    *mapped.pointer_mut(field).unwrap() = Value::String(permitted);
                    let mapped = serde_json::to_string(&mapped).unwrap();
                    // The sibling spelling is relative only to leaked_root.
                    // Hosted validation deliberately puts TMPDIR below the
                    // checkout, so an alias sibling there still is an absolute
                    // descendant of the checkout root and must remain refused
                    // by that separate root. Test the component boundary
                    // against the root whose textual prefix this case varies.
                    let roots = if relative_only {
                        checked_roots.as_slice()
                    } else {
                        std::slice::from_ref(&leaked_root)
                    };
                    require_root_independent_exports(&vec![mapped; exports.len()], roots).unwrap();
                }
                let mut mapped = option_fixture.clone();
                *mapped.pointer_mut(field).unwrap() = Value::String(format!(
                    "-fdebug-prefix-map={}=/mapped",
                    leaked_root.display()
                ));
                let mapped = serde_json::to_string(&mapped).unwrap();
                let error =
                    require_root_independent_exports(&vec![mapped; exports.len()], &checked_roots)
                        .unwrap_err();
                assert!(
                    error.contains("absolute checkout-root reference"),
                    "{field}: {error}"
                );
            }

            let mut leaked: ManifestMetadata = serde_json::from_str(&encoded).unwrap();
            let reproducer = leaked
                .cells
                .iter_mut()
                .find_map(|cell| cell.current_reproducer.as_mut())
                .unwrap();
            reproducer.argv[0] = leaked_root
                .join(&reproducer.argv[0])
                .to_str()
                .unwrap()
                .to_string();
            let leaked = serde_json::to_string(&leaked).unwrap();
            let shared_leak = vec![leaked.clone(); exports.len()];
            let error = require_root_independent_exports(&shared_leak, &checked_roots).unwrap_err();
            assert!(
                error.contains("absolute checkout-root reference"),
                "{error}"
            );
            let mut one_root_leak = exports.clone();
            one_root_leak[1] = leaked;
            let error =
                require_root_independent_exports(&one_root_leak, &checked_roots).unwrap_err();
            assert!(
                error.contains("exports differ across repository roots"),
                "{error}"
            );
        }
        let encoded_value = serde_json::to_value(&first).unwrap();
        assert!(encoded_value.get("selected_custom_commands").is_none());
        assert!(
            encoded_value
                .get("selected_by_full_custom_commands")
                .is_some()
        );
        assert!(
            encoded_value["cells"]
                .as_array()
                .unwrap()
                .iter()
                .all(|cell| {
                    cell.get("measurement").is_none()
                        && cell.get("measured").is_none()
                        && cell.get("state").is_none()
                        && cell.get("enabled").is_none()
                        && cell.get("ci").is_none()
                        && cell.get("ci_disabled_reason").is_none()
                        && cell.get("selected_by_full").is_some()
                        && cell.get("not_selected_by_full_reason").is_some()
                })
        );

        let cells_in_manifest = first.cells.len();
        let cells_selected_by_full = first
            .cells
            .iter()
            .filter(|cell| cell.selected_by_full)
            .count();
        assert!(cells_selected_by_full > 0);
        assert!(cells_selected_by_full < cells_in_manifest);
        for cell in first
            .cells
            .iter()
            .chain(&first.selected_by_full_custom_commands)
        {
            if cell.selected_by_full {
                assert!(cell.not_selected_by_full_reason.is_none());
                assert!(cell.not_applicable_reason.is_none());
            } else {
                assert!(
                    cell.not_selected_by_full_reason.is_some()
                        || cell.not_applicable_reason.is_some()
                );
                assert!(
                    cell.not_selected_by_full_reason.is_none()
                        || cell.not_applicable_reason.is_none()
                );
            }
        }
        // Schema 3 semantic change: this list holds every custom cell in the
        // manifest, not only those full validation selects. Schema 2 asserted
        // here that every entry was selected by full; a custom cell moved to
        // backends_disabled (the LiteInst reset) is now exported unselected
        // with its not-applicable reason instead of disappearing.
        let custom = &first.selected_by_full_custom_commands;
        assert!(custom.iter().all(|cell| cell.mode == "custom"));
        assert!(first.cells.iter().all(|cell| cell.mode != "custom"));
        assert!(custom.iter().any(|cell| cell.selected_by_full));
        let liteinst = custom
            .iter()
            .find(|cell| {
                cell.test == "system-utils/clock-determinism" && cell.backend == "liteinst"
            })
            .expect("the disabled clock-determinism custom/liteinst cell is exported");
        assert!(!liteinst.selected_by_full);
        assert!(liteinst.not_selected_by_full_reason.is_none());
        assert!(
            liteinst
                .not_applicable_reason
                .as_deref()
                .is_some_and(|reason| !reason.trim().is_empty())
        );
    }

    /// A cell's published reproducer selects exactly that cell through the
    /// harness's own selection: a cell of a focused run type (compat/java
    /// verify on ptrace carries the test-level label sabre-compat-only) needs
    /// `--label`, and a `full` cell's reproducer keeps its exact form. The
    /// argv is parsed the way test-harness parses it.
    #[test]
    fn a_reproducer_selects_exactly_its_cell_including_focused_run_types() {
        let root = root();
        let export = build_export(&root).unwrap();
        let manifests = ManifestSet::load(&root).unwrap();
        let reproducer = |test: &str, mode: &str, backend: &str| {
            export
                .cells
                .iter()
                .find(|cell| cell.test == test && cell.mode == mode && cell.backend == backend)
                .unwrap_or_else(|| panic!("no exported cell {test}/{mode}@{backend}"))
                .current_reproducer
                .as_ref()
                .unwrap_or_else(|| panic!("{test}/{mode}@{backend} has no reproducer"))
        };
        let selected = |argv: &[String]| {
            assert_eq!(argv[..2], ["target/debug/test-harness", "run"]);
            let mut selection = Selection::default();
            let mut words = argv[2..].iter();
            while let Some(word) = words.next() {
                match word.as_str() {
                    "--include-manual" => selection.include_manual = true,
                    "--include-occasional" => selection.include_occasional = true,
                    "--probe-disabled" => selection.population = Some(Population::Disabled),
                    "--test" => selection.test = words.next().cloned(),
                    "--mode" => selection.mode = words.next().cloned(),
                    "--backend" => selection.backend = words.next().cloned(),
                    "--label" => selection
                        .labels
                        .extend(words.next().unwrap().split(',').map(str::to_string)),
                    other => panic!("unexpected reproducer word {other}"),
                }
            }
            if selection.population.is_none() {
                selection.population = Some(if selection.include_manual {
                    Population::Enabled
                } else {
                    Population::Required
                });
            }
            manifests
                .select(&selection)
                .unwrap()
                .into_iter()
                .map(|cell| {
                    (
                        cell.id.test,
                        cell.id.mode,
                        cell.id.backend.unwrap_or_else(|| "native".into()),
                    )
                })
                .collect::<Vec<_>>()
        };
        for (test, mode, backend) in [
            ("compat/java", "verify", "ptrace"),
            ("compat/comm", "verify", "sabre"),
            ("compat/strict-du", "verify", "sabre"),
            ("compat/comm", "verify", "ptrace"),
        ] {
            let argv = &reproducer(test, mode, backend).argv;
            assert_eq!(
                selected(argv),
                vec![(test.to_string(), mode.to_string(), backend.to_string())],
                "{argv:?}"
            );
        }
        // A cell of the default run type keeps its exact reproducer.
        let full = reproducer("compat/comm", "verify", "ptrace");
        assert_eq!(
            full.argv,
            [
                "target/debug/test-harness",
                "run",
                "--include-manual",
                "--include-occasional",
                "--test",
                "compat/comm",
                "--mode",
                "verify",
                "--backend",
                "ptrace",
            ]
        );
        assert_eq!(
            full.shell_command,
            "target/debug/test-harness run --include-manual --include-occasional --test compat/comm --mode verify --backend ptrace"
        );
    }
}
