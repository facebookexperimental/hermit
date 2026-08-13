#!/usr/bin/env rust-script
//! Keep Hermit's compatibility scorecard derived from the E2E manifest and
//! verify that a validate run produced a fresh passing row for every selected
//! regression cell.
//!
//! ```cargo
//! [dependencies]
//! serde = { version = "1", features = ["derive"] }
//! serde_json = "1"
//! ```

#[path = "../../scripts/lib/rust_script_prelude.rs"]
mod rust_script_prelude;

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::ExitCode;
use std::time::SystemTime;

use serde::Deserialize;
use serde::Serialize;

const SCORECARD: &str = "SCORECARD.md";
const CELLS: &str = "ci/compat-envelope/cells.json";
const EXPECTED_PLAN: &str = "ci/expected-e2e-plan.json";
const SCHEMA: u64 = 2;

const USAGE: &str = r#"Usage: ci/compat-envelope/scorecard.rs COMMAND [OPTIONS]

Commands:
  show
      Print the derived compatibility table.
  check
      Refuse if SCORECARD.md or ci/compat-envelope/cells.json is stale.
  update [--allow-green-removal] [--allow-cell-removal]
      Rewrite the two tracked files. Green regressions and cell deletion are
      refused unless the matching explicit flag is present.
  verify-results --results DIR [--lanes portable,privileged]
      Check the tracked files, then require a fresh PASS row at HEAD for every
      selected regression cell in the named lanes. The default is both lanes.
  self-test
      Exercise accepting and refusing result sets without running a guest.
  --help
      Show this text.

Green means that the cell is selected by ci/expected-e2e-plan.json and is not a
chaos-mode race-exposure check. Everything else in the manifest is red until it
is measured, promoted into the selected plan, and passes validate.
"#;

#[derive(Clone, Debug, Deserialize)]
struct ManifestRow {
    backend: String,
    bucket: String,
    ci: bool,
    enabled: bool,
    lane: String,
    mode: String,
    test: String,
}

#[derive(Clone, Debug, Deserialize)]
struct ExpectedPlan {
    cells: Vec<CellId>,
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
struct CellId {
    lane: String,
    category: String,
    test: String,
    mode: String,
    backend: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct TrackedCells {
    schema: u64,
    cells: Vec<TrackedCell>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct TrackedCell {
    #[serde(flatten)]
    id: CellId,
    #[serde(default)]
    enabled: bool,
    status: CellStatus,
    /// Filled only by the periodic all-red pressure test. Ordinary validate
    /// never changes this array.
    observations: Vec<serde_json::Value>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
enum CellStatus {
    Green,
    Red,
}

#[derive(Clone, Debug, Deserialize)]
struct ResultRow {
    schema: u64,
    hermit_sha: String,
    source_tree_dirty: bool,
    test: String,
    category: String,
    lane: String,
    mode: String,
    backend: Option<String>,
    classification: String,
    outcome: String,
}

impl ResultRow {
    fn id(&self) -> Option<CellId> {
        let backend = match self.backend.as_deref() {
            Some(backend) => backend.to_string(),
            None if self.mode == "naked" => "native".to_string(),
            None => return None,
        };
        Some(CellId {
            lane: self.lane.clone(),
            category: self.category.clone(),
            test: self.test.clone(),
            mode: self.mode.clone(),
            backend,
        })
    }
}

struct Derived {
    population: BTreeSet<CellId>,
    enabled: BTreeSet<CellId>,
    selected: BTreeSet<CellId>,
    green: BTreeSet<CellId>,
}

struct ResultCandidate {
    modified: SystemTime,
    path: PathBuf,
    row: ResultRow,
}

fn main() -> ExitCode {
    rust_script_prelude::init();
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("compatibility scorecard: {message}");
            ExitCode::from(2)
        }
    }
}

fn run() -> Result<(), String> {
    let mut args = env::args().skip(1);
    let Some(command) = args.next() else {
        return Err(format!("missing command\n\n{USAGE}"));
    };
    if matches!(command.as_str(), "-h" | "--help" | "help") {
        print!("{USAGE}");
        return Ok(());
    }
    let root = repo_root()?;
    match command.as_str() {
        "show" => {
            no_more(&mut args)?;
            let derived = derive(&root)?;
            print!("{}", render_scorecard(&derived));
        }
        "check" => {
            no_more(&mut args)?;
            check_tracked(&root)?;
            println!(
                "compatibility scorecard: tracked table and {} cells are current",
                derive(&root)?.population.len()
            );
        }
        "update" => {
            let mut allow_green_removal = false;
            let mut allow_cell_removal = false;
            for arg in args {
                match arg.as_str() {
                    "--allow-green-removal" => allow_green_removal = true,
                    "--allow-cell-removal" => allow_cell_removal = true,
                    _ => return Err(format!("unknown update option `{arg}`\n\n{USAGE}")),
                }
            }
            update_tracked(&root, allow_green_removal, allow_cell_removal)?;
        }
        "verify-results" => {
            let mut result_root = None;
            let mut lanes = BTreeSet::from(["portable".to_string(), "privileged".to_string()]);
            while let Some(arg) = args.next() {
                match arg.as_str() {
                    "--results" => {
                        result_root = Some(PathBuf::from(
                            args.next().ok_or("--results requires a directory")?,
                        ));
                    }
                    "--lanes" => {
                        lanes = args
                            .next()
                            .ok_or("--lanes requires a comma-separated value")?
                            .split(',')
                            .filter(|lane| !lane.is_empty())
                            .map(str::to_string)
                            .collect();
                        if lanes.is_empty()
                            || lanes
                                .iter()
                                .any(|lane| lane != "portable" && lane != "privileged")
                        {
                            return Err("--lanes accepts portable, privileged, or both".into());
                        }
                    }
                    _ => return Err(format!("unknown verify-results option `{arg}`\n\n{USAGE}")),
                }
            }
            let result_root = result_root.ok_or("verify-results requires --results DIR")?;
            check_tracked(&root)?;
            verify_results(&root, &result_root, &lanes)?;
        }
        "self-test" => {
            no_more(&mut args)?;
            self_test()?;
        }
        _ => return Err(format!("unknown command `{command}`\n\n{USAGE}")),
    }
    Ok(())
}

fn no_more(args: &mut impl Iterator<Item = String>) -> Result<(), String> {
    match args.next() {
        Some(arg) => Err(format!("unexpected argument `{arg}`\n\n{USAGE}")),
        None => Ok(()),
    }
}

fn repo_root() -> Result<PathBuf, String> {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .map_err(|e| format!("cannot run git rev-parse: {e}"))?;
    if !output.status.success() {
        return Err("not inside a Git checkout".into());
    }
    let root = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
    if !root.join(EXPECTED_PLAN).is_file() {
        return Err(format!("{} is not the Hermit checkout", root.display()));
    }
    Ok(root)
}

fn derive(root: &Path) -> Result<Derived, String> {
    let output = Command::new("cargo")
        .args([
            "run",
            "--quiet",
            "-p",
            "hermit-manifest-plan",
            "--",
            "--format",
            "matrix-json",
        ])
        .current_dir(root)
        .output()
        .map_err(|e| format!("cannot run hermit-manifest-plan: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "hermit-manifest-plan failed:\n{}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let rows: Vec<ManifestRow> = serde_json::from_slice(&output.stdout)
        .map_err(|e| format!("manifest-plan emitted invalid JSON: {e}"))?;
    let expected: ExpectedPlan = read_json(&root.join(EXPECTED_PLAN))?;

    let mut population = BTreeSet::new();
    let mut enabled = BTreeSet::new();
    let mut ci_enabled = BTreeSet::new();
    for row in rows {
        let id = CellId {
            lane: row.lane,
            category: row.bucket,
            test: row.test,
            mode: row.mode,
            backend: row.backend,
        };
        if !population.insert(id.clone()) {
            return Err(format!(
                "manifest-plan emitted duplicate cell {}",
                display_id(&id)
            ));
        }
        if row.enabled {
            enabled.insert(id.clone());
            if row.ci {
                ci_enabled.insert(id);
            }
        }
    }
    let selected: BTreeSet<CellId> = expected.cells.into_iter().collect();
    if selected.len() == 0 {
        return Err("expected E2E plan is empty".into());
    }
    for id in &selected {
        if !population.contains(id) {
            return Err(format!(
                "expected plan names absent cell {}",
                display_id(id)
            ));
        }
        if !ci_enabled.contains(id) {
            return Err(format!(
                "expected plan names a cell not enabled for ordinary CI: {}",
                display_id(id)
            ));
        }
    }
    let green = selected
        .iter()
        .filter(|id| id.mode != "chaos")
        .cloned()
        .collect();
    Ok(Derived {
        population,
        enabled,
        selected,
        green,
    })
}

fn read_json<T: for<'a> Deserialize<'a>>(path: &Path) -> Result<T, String> {
    let bytes = fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    serde_json::from_slice(&bytes).map_err(|e| format!("invalid JSON in {}: {e}", path.display()))
}

fn render_scorecard(derived: &Derived) -> String {
    let mut backends: BTreeSet<&str> = derived
        .population
        .iter()
        .map(|id| id.backend.as_str())
        .collect();
    let preferred = ["ptrace", "dbt", "kvm", "sabre", "liteinst", "native"];
    let mut ordered = Vec::new();
    for backend in preferred {
        if backends.remove(backend) {
            ordered.push(backend);
        }
    }
    ordered.extend(backends);

    let mut out = String::from(
        "# Compatibility scorecard\n\n\
This table is derived from the manifest, not from a separately maintained parent-workspace CSV. \
`./ci/compat-envelope/scorecard.rs check` verifies it.\n\n\
**Green** means the cell is in `ci/expected-e2e-plan.json`, is not a chaos-mode \
race-exposure check, and is therefore required to pass by ordinary validation. **Red** is every \
other test/mode/backend cell: measured failure, unavailable, or not yet run all remain red until \
the cell is promoted into the regression plan and passes. Manifest-disabled combinations are red, \
not omitted: a cell that cannot run is not green.\n\n\
These are the current pre-basic-sanity contracts. In particular, bare `--verify` uses the \
Stripped comparator and this table does not relabel it as strict INFO-log parity.\n\n\
| Backend | Green | Red | Total |\n\
| --- | ---: | ---: | ---: |\n",
    );
    let mut green_total = 0usize;
    let mut total = 0usize;
    for backend in &ordered {
        let backend_total = derived
            .population
            .iter()
            .filter(|id| id.backend == *backend)
            .count();
        let backend_green = derived
            .green
            .iter()
            .filter(|id| id.backend == *backend)
            .count();
        green_total += backend_green;
        total += backend_total;
        out.push_str(&format!(
            "| `{backend}` | {backend_green} | {} | {backend_total} |\n",
            backend_total - backend_green
        ));
    }
    out.push_str(&format!(
        "| **Total** | **{green_total}** | **{}** | **{total}** |\n\n",
        total - green_total
    ));
    out.push_str(
        "The mode view makes the current order of work explicit: expand `verify` first, then \
`replay`, then `chaos`. Each backend cell is `green / total`; an em dash means that mode does \
not exist for that backend.\n\n| Mode",
    );
    for backend in &ordered {
        out.push_str(&format!(" | `{backend}`"));
    }
    out.push_str(" | Green | Red | Total |\n| ---");
    for _ in &ordered {
        out.push_str(" | ---:");
    }
    out.push_str(" | ---: | ---: | ---: |\n");
    for mode in ["verify", "replay", "chaos", "custom", "naked"] {
        let mode_total = derived
            .population
            .iter()
            .filter(|id| id.mode == mode)
            .count();
        let mode_green = derived.green.iter().filter(|id| id.mode == mode).count();
        out.push_str(&format!("| `{mode}`"));
        for backend in &ordered {
            let cell_total = derived
                .population
                .iter()
                .filter(|id| id.mode == mode && id.backend == *backend)
                .count();
            if cell_total == 0 {
                out.push_str(" | —");
            } else {
                let cell_green = derived
                    .green
                    .iter()
                    .filter(|id| id.mode == mode && id.backend == *backend)
                    .count();
                out.push_str(&format!(" | {cell_green} / {cell_total}"));
            }
        }
        out.push_str(&format!(
            " | {mode_green} | {} | {mode_total} |\n",
            mode_total - mode_green
        ));
    }
    out.push_str(&format!(
        "| **Total** | | | | | | | **{green_total}** | **{}** | **{total}** |\n\n",
        total - green_total
    ));
    let chaos = derived
        .selected
        .iter()
        .filter(|id| id.mode == "chaos")
        .count();
    out.push_str(&format!(
        "Ordinary full validation executes {} selected regression cells: the {green_total} green \
compatibility cells above plus {chaos} chaos-mode race-exposure checks. A passing validate must \
produce a fresh result for all of them; a failing green cell is a regression, not permission to \
move it to red.\n",
        derived.selected.len()
    ));
    out
}

fn tracked_from(
    derived: &Derived,
    existing: Option<TrackedCells>,
    allow_green_removal: bool,
    allow_cell_removal: bool,
) -> Result<TrackedCells, String> {
    let mut previous = BTreeMap::new();
    if let Some(existing) = existing {
        if existing.schema != 1 && existing.schema != SCHEMA {
            return Err(format!(
                "unsupported tracked cell schema {}",
                existing.schema
            ));
        }
        for cell in existing.cells {
            if previous.insert(cell.id.clone(), cell).is_some() {
                return Err("tracked cell file contains a duplicate identity".into());
            }
        }
    }

    let removed: Vec<_> = previous
        .keys()
        .filter(|id| !derived.population.contains(*id))
        .cloned()
        .collect();
    if !removed.is_empty() && !allow_cell_removal {
        return Err(format!(
            "refusing to delete {} tracked cell(s); first is {}. Re-run update with \
             --allow-cell-removal only for an intentional reviewed denominator change",
            removed.len(),
            display_id(&removed[0])
        ));
    }
    let regressed: Vec<_> = previous
        .values()
        .filter(|cell| cell.status == CellStatus::Green && !derived.green.contains(&cell.id))
        .map(|cell| cell.id.clone())
        .collect();
    if !regressed.is_empty() && !allow_green_removal {
        return Err(format!(
            "refusing to move {} green cell(s) to red; first is {}. Fix the regression, or use \
             --allow-green-removal only at an explicit compatibility-standard transition",
            regressed.len(),
            display_id(&regressed[0])
        ));
    }

    let cells = derived
        .population
        .iter()
        .cloned()
        .map(|id| {
            let observations = previous
                .get(&id)
                .map(|cell| cell.observations.clone())
                .unwrap_or_default();
            let status = if derived.green.contains(&id) {
                CellStatus::Green
            } else {
                CellStatus::Red
            };
            let enabled = derived.enabled.contains(&id);
            TrackedCell {
                id,
                enabled,
                status,
                observations,
            }
        })
        .collect();
    Ok(TrackedCells {
        schema: SCHEMA,
        cells,
    })
}

fn load_existing(root: &Path) -> Result<Option<TrackedCells>, String> {
    let path = root.join(CELLS);
    if !path.exists() {
        return Ok(None);
    }
    read_json(&path).map(Some)
}

fn encoded_cells(cells: &TrackedCells) -> Result<String, String> {
    let mut text = serde_json::to_string_pretty(cells)
        .map_err(|e| format!("cannot serialize tracked cells: {e}"))?;
    text.push('\n');
    Ok(text)
}

fn check_tracked(root: &Path) -> Result<(), String> {
    let derived = derive(root)?;
    let expected_scorecard = render_scorecard(&derived);
    compare_file(&root.join(SCORECARD), &expected_scorecard)?;
    let cells = tracked_from(&derived, load_existing(root)?, false, false)?;
    compare_file(&root.join(CELLS), &encoded_cells(&cells)?)?;
    Ok(())
}

fn compare_file(path: &Path, expected: &str) -> Result<(), String> {
    let actual = fs::read_to_string(path).map_err(|e| {
        format!(
            "cannot read tracked {}: {e}; run `./ci/compat-envelope/scorecard.rs update`",
            path.display()
        )
    })?;
    if actual != expected {
        return Err(format!(
            "{} is stale; run `./ci/compat-envelope/scorecard.rs update` and review the diff",
            path.display()
        ));
    }
    Ok(())
}

fn update_tracked(
    root: &Path,
    allow_green_removal: bool,
    allow_cell_removal: bool,
) -> Result<(), String> {
    let derived = derive(root)?;
    let cells = tracked_from(
        &derived,
        load_existing(root)?,
        allow_green_removal,
        allow_cell_removal,
    )?;
    fs::write(root.join(SCORECARD), render_scorecard(&derived))
        .map_err(|e| format!("cannot write {SCORECARD}: {e}"))?;
    fs::write(root.join(CELLS), encoded_cells(&cells)?)
        .map_err(|e| format!("cannot write {CELLS}: {e}"))?;
    println!(
        "compatibility scorecard: wrote {} green / {} red / {} total",
        derived.green.len(),
        derived.population.len() - derived.green.len(),
        derived.population.len()
    );
    Ok(())
}

fn verify_results(root: &Path, result_root: &Path, lanes: &BTreeSet<String>) -> Result<(), String> {
    let derived = derive(root)?;
    let head = git_head(root)?;
    let expected: BTreeSet<_> = derived
        .selected
        .iter()
        .filter(|id| lanes.contains(&id.lane))
        .cloned()
        .collect();
    if expected.is_empty() {
        return Err("selected lanes contain no regression cells".into());
    }
    let candidates = read_result_candidates(result_root, &head)?;
    verify_candidate_set(&expected, candidates)?;

    print!("{}", render_scorecard(&derived));
    let green_checked = expected
        .iter()
        .filter(|id| derived.green.contains(*id))
        .count();
    let chaos_checked = expected.iter().filter(|id| id.mode == "chaos").count();
    println!();
    println!(
        "Fresh result check: {}/{} selected cells passed at {} ({} compatibility green, {} chaos).",
        expected.len(),
        expected.len(),
        head,
        green_checked,
        chaos_checked
    );
    println!("Result directory: {}", result_root.display());
    Ok(())
}

fn git_head(root: &Path) -> Result<String, String> {
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(root)
        .output()
        .map_err(|e| format!("cannot read HEAD: {e}"))?;
    if !output.status.success() {
        return Err("git rev-parse HEAD failed".into());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn read_result_candidates(
    root: &Path,
    head: &str,
) -> Result<BTreeMap<CellId, Vec<ResultCandidate>>, String> {
    if !root.is_dir() {
        return Err(format!(
            "result directory does not exist: {}",
            root.display()
        ));
    }
    let mut files = Vec::new();
    find_result_files(root, &mut files)?;
    if files.is_empty() {
        return Err(format!("no results.jsonl files under {}", root.display()));
    }
    let mut out: BTreeMap<CellId, Vec<ResultCandidate>> = BTreeMap::new();
    for path in files {
        let modified = fs::metadata(&path)
            .and_then(|m| m.modified())
            .map_err(|e| format!("cannot read timestamp for {}: {e}", path.display()))?;
        let text = fs::read_to_string(&path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        for (index, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let row: ResultRow = serde_json::from_str(line)
                .map_err(|e| format!("invalid JSON at {}:{}: {e}", path.display(), index + 1))?;
            if row.schema != 3 {
                return Err(format!(
                    "{}:{} has result schema {}, expected 3",
                    path.display(),
                    index + 1,
                    row.schema
                ));
            }
            if row.hermit_sha != head || row.source_tree_dirty {
                return Err(format!(
                    "{}:{} is not a clean result for HEAD {} (sha={}, dirty={})",
                    path.display(),
                    index + 1,
                    head,
                    row.hermit_sha,
                    row.source_tree_dirty
                ));
            }
            if row.classification != "required" {
                continue;
            }
            let id = row
                .id()
                .ok_or_else(|| format!("{}:{} has no backend", path.display(), index + 1))?;
            out.entry(id).or_default().push(ResultCandidate {
                modified,
                path: path.clone(),
                row,
            });
        }
    }
    Ok(out)
}

fn find_result_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
    for entry in fs::read_dir(dir).map_err(|e| format!("cannot list {}: {e}", dir.display()))? {
        let entry = entry.map_err(|e| format!("cannot read entry under {}: {e}", dir.display()))?;
        let path = entry.path();
        let kind = entry
            .file_type()
            .map_err(|e| format!("cannot stat {}: {e}", path.display()))?;
        if kind.is_dir() {
            find_result_files(&path, out)?;
        } else if kind.is_file() && entry.file_name() == "results.jsonl" {
            out.push(path);
        }
    }
    Ok(())
}

fn verify_candidate_set(
    expected: &BTreeSet<CellId>,
    mut candidates: BTreeMap<CellId, Vec<ResultCandidate>>,
) -> Result<(), String> {
    let mut missing = Vec::new();
    let mut failed = Vec::new();
    for id in expected {
        let Some(rows) = candidates.get_mut(id) else {
            missing.push(display_id(id));
            continue;
        };
        rows.sort_by(|a, b| {
            a.modified
                .cmp(&b.modified)
                .then_with(|| a.path.cmp(&b.path))
        });
        let latest = rows.last().expect("nonempty candidate list");
        if rows.len() >= 2 && rows[rows.len() - 2].modified == latest.modified {
            return Err(format!(
                "ambiguous equally-new results for {} in {} and {}",
                display_id(id),
                rows[rows.len() - 2].path.display(),
                latest.path.display()
            ));
        }
        if latest.row.outcome != "PASS" {
            failed.push(format!(
                "{}={} ({})",
                display_id(id),
                latest.row.outcome,
                latest.path.display()
            ));
        }
    }
    if !missing.is_empty() || !failed.is_empty() {
        let mut message = format!(
            "fresh result set refused: {} missing, {} non-passing",
            missing.len(),
            failed.len()
        );
        for item in missing.iter().take(8) {
            message.push_str(&format!("\n  missing: {item}"));
        }
        for item in failed.iter().take(8) {
            message.push_str(&format!("\n  non-passing: {item}"));
        }
        return Err(message);
    }
    Ok(())
}

fn display_id(id: &CellId) -> String {
    format!(
        "{}/{}/{}/{}@{}",
        id.lane, id.category, id.test, id.mode, id.backend
    )
}

fn self_test() -> Result<(), String> {
    let id = CellId {
        lane: "portable".into(),
        category: "fixture".into(),
        test: "fixture/pass".into(),
        mode: "verify".into(),
        backend: "ptrace".into(),
    };
    let expected = BTreeSet::from([id.clone()]);
    let candidate = |outcome: &str| ResultCandidate {
        modified: SystemTime::UNIX_EPOCH,
        path: PathBuf::from("fixture/results.jsonl"),
        row: ResultRow {
            schema: 3,
            hermit_sha: "fixture".into(),
            source_tree_dirty: false,
            test: id.test.clone(),
            category: id.category.clone(),
            lane: id.lane.clone(),
            mode: id.mode.clone(),
            backend: Some(id.backend.clone()),
            classification: "required".into(),
            outcome: outcome.into(),
        },
    };
    verify_candidate_set(
        &expected,
        BTreeMap::from([(id.clone(), vec![candidate("PASS")])]),
    )
    .map_err(|e| format!("positive result bracket failed: {e}"))?;
    if verify_candidate_set(&expected, BTreeMap::new()).is_ok() {
        return Err("negative result bracket accepted a missing row".into());
    }
    if verify_candidate_set(
        &expected,
        BTreeMap::from([(id.clone(), vec![candidate("FAIL")])]),
    )
    .is_ok()
    {
        return Err("negative result bracket accepted a failing row".into());
    }
    let old_green = TrackedCells {
        schema: SCHEMA,
        cells: vec![TrackedCell {
            id: id.clone(),
            enabled: true,
            status: CellStatus::Green,
            observations: Vec::new(),
        }],
    };
    let regressed = Derived {
        population: BTreeSet::from([id.clone()]),
        enabled: BTreeSet::from([id.clone()]),
        selected: BTreeSet::new(),
        green: BTreeSet::new(),
    };
    if tracked_from(&regressed, Some(old_green), false, false).is_ok() {
        return Err("negative ratchet bracket accepted green-to-red movement".into());
    }
    let intentional = TrackedCells {
        schema: SCHEMA,
        cells: vec![TrackedCell {
            id,
            enabled: true,
            status: CellStatus::Green,
            observations: Vec::new(),
        }],
    };
    tracked_from(&regressed, Some(intentional), true, false)
        .map_err(|e| format!("explicit compatibility-transition bracket failed: {e}"))?;

    let native = ResultRow {
        schema: 3,
        hermit_sha: "fixture".into(),
        source_tree_dirty: false,
        test: "fixture/native".into(),
        category: "fixture".into(),
        lane: "portable".into(),
        mode: "naked".into(),
        backend: None,
        classification: "required".into(),
        outcome: "PASS".into(),
    };
    if native.id().map(|id| id.backend) != Some("native".into()) {
        return Err("native result identity did not map a null backend to `native`".into());
    }
    let mut malformed = native;
    malformed.mode = "verify".into();
    if malformed.id().is_some() {
        return Err("non-native result without a backend was accepted".into());
    }
    println!("compatibility scorecard self-test: 3 accepted cases, 4 refused cases");
    Ok(())
}
