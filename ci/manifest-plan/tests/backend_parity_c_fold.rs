/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Pin the fold of the backend-parity-c bucket into c-programs.
//!
//! Slice S6 of <https://github.com/rrnewton/hermit/issues/3301> deleted
//! `tests/e2e/manifests/backend-parity-c.yaml`, moved its 104 tests into
//! `tests/e2e/manifests/c-programs.yaml` and its fixtures into `tests/c`, and
//! recorded every old id in `tests/e2e/manifests/inventory/retired-ids.json`.
//! The fold is a rename, not a change of coverage. This file checks that from
//! four independent sides:
//!
//! 1. The retired-id map renames exactly the documented ids: every id is the
//!    bucket-prefix rename except the one collision, and it is a bijection onto
//!    live ids.
//! 2. No committed CI plan cell keeps the retired bucket: every folded test's
//!    cells are c-programs cells, and every documented later lane move is in
//!    its destination lane.
//! 3. No committed compatibility cell-table row keeps the retired bucket or a
//!    `backend-parity-c/` test id.
//! 4. The command the c-programs nodes run refuses a selection of zero cells,
//!    so folding more tests into that node cannot turn it into a vacuous pass.
//!
//! Two further checks pin slice S13's DAG edits: every manifest node that
//! selects a DBT cell and runs in one dagrun with check.dbt_runtime_abi orders
//! after it, as the retired test.dbt_parity node did, and the privileged
//! c-programs descriptions name the cells those nodes select.
//!
//! The fold's parent on main is 3f66a249b30fada86b81e722b8e5439ac0789f8e, the
//! last commit that declared backend-parity-c. Sides 2 and 3 used to pin the
//! plan's and the cell table's per-bucket counts to the pre-fold numbers plus
//! every later selection, so each cell flip had to edit this file. The counts
//! are now recorded only in the generated files themselves, which the manifest
//! gate holds to the manifests, and a flip is reviewed as a diff of
//! `ci/expected-e2e-plan.json` (<https://github.com/rrnewton/hermit/issues/3606>).

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

use hermit_manifest_plan::retired_ids::RetiredIds;
use serde_json::Value as JsonValue;
use serde_yaml::Value;

const RETIRED_BUCKET: &str = "backend-parity-c";
const SUCCESSOR_BUCKET: &str = "c-programs";
const LAST_LIVE_COMMIT: &str = "3f66a249b30fada86b81e722b8e5439ac0789f8e";
const FOLDED_TESTS: usize = 104;

/// Every successor that is not the plain prefix rename, and why. c-programs
/// already declared `c-programs/pidfd-open-self` for `tests/c/pidfd_open_self.c`,
/// a different program from the backend-parity-c fixture of the same name, so
/// the moved test and its fixture took a `-pair` suffix instead of overwriting.
const COLLISION_RENAMES: &[(&str, &str)] = &[(
    "backend-parity-c/pidfd-open-self",
    "c-programs/pidfd-open-self-pair",
)];

/// Cells that later changes moved between lanes after the fold, as
/// (test, backend, mode, from lane, to lane). Each move keeps the cell and only
/// changes which lane runs it.
///
/// - `system-utils/sysfs-sanitized-prefixes` needs host hwmon sensors, which
///   GitHub-hosted runners lack; hosted run
///   <https://github.com/rrnewton/hermit/actions/runs/36485831200> failed it
///   with "hwmon has no readable sanitized leaf". Its two verify cells moved
///   from the portable lane to the privileged lane.
const LATER_LANE_MOVES: &[(&str, &str, &str, &str, &str)] = &[
    (
        "system-utils/sysfs-sanitized-prefixes",
        "kvm",
        "verify",
        "portable",
        "privileged",
    ),
    (
        "system-utils/sysfs-sanitized-prefixes",
        "ptrace",
        "verify",
        "portable",
        "privileged",
    ),
];

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("ci/manifest-plan must sit two levels below the repository root")
        .to_path_buf()
}

fn read_json(path: &str) -> JsonValue {
    let path = repo_root().join(path);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()));
    serde_json::from_str(&text)
        .unwrap_or_else(|error| panic!("{} is not JSON: {error}", path.display()))
}

fn field<'a>(row: &'a JsonValue, name: &str) -> &'a str {
    row.get(name)
        .and_then(JsonValue::as_str)
        .unwrap_or_else(|| panic!("row has no string {name}: {row}"))
}

/// Every manifest bucket and, per test id, its program, read from the YAML
/// the harness reads.
struct Manifests {
    buckets: BTreeSet<String>,
    programs: BTreeMap<String, Option<String>>,
}

fn manifests() -> Manifests {
    let dir = repo_root().join("tests/e2e/manifests");
    let mut buckets = BTreeSet::new();
    let mut programs = BTreeMap::new();
    let mut entries = std::fs::read_dir(&dir)
        .unwrap_or_else(|error| panic!("cannot list {}: {error}", dir.display()))
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "yaml"))
        .collect::<Vec<_>>();
    entries.sort();
    for path in entries {
        // A corpus section expands into ordinary recipes, as the harness reads it.
        let document = hermit_manifest_plan::manifest_corpus::expand_corpus(
            serde_yaml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap(),
        )
        .unwrap();
        let Some(bucket) = document.get("bucket").and_then(Value::as_str) else {
            continue;
        };
        assert!(buckets.insert(bucket.to_string()), "bucket {bucket} twice");
        for test in document
            .get("test")
            .and_then(Value::as_sequence)
            .unwrap_or_else(|| panic!("{} has no test list", path.display()))
        {
            let id = test.get("id").and_then(Value::as_str).unwrap().to_string();
            let program = test
                .get("program")
                .and_then(Value::as_str)
                .map(str::to_string);
            assert!(programs.insert(id.clone(), program).is_none(), "{id} twice");
        }
    }
    assert!(!buckets.is_empty() && !programs.is_empty());
    Manifests { buckets, programs }
}

fn retired_ids() -> RetiredIds {
    RetiredIds::load(&repo_root()).unwrap()
}

#[test]
fn the_retired_id_map_is_the_prefix_rename_plus_the_documented_collision() {
    let map = retired_ids();
    assert_eq!(map.retirements.len(), 1, "only backend-parity-c is retired");
    let retirement = map.retirement(RETIRED_BUCKET).unwrap();
    assert_eq!(retirement.successor_bucket, SUCCESSOR_BUCKET);
    assert_eq!(retirement.last_live_commit, LAST_LIVE_COMMIT);
    assert_eq!(retirement.ids.len(), FOLDED_TESTS);
    let collisions = COLLISION_RENAMES
        .iter()
        .map(|(old, new)| (old.to_string(), new.to_string()))
        .collect::<BTreeMap<_, _>>();
    let mut renamed = BTreeMap::new();
    for (old, new) in &retirement.ids {
        let name = old
            .strip_prefix("backend-parity-c/")
            .unwrap_or_else(|| panic!("{old} is not a backend-parity-c id"));
        if format!("c-programs/{name}") != *new {
            renamed.insert(old.clone(), new.clone());
        }
    }
    assert_eq!(
        renamed, collisions,
        "only the documented collision may differ from the prefix rename"
    );
    for (old, new) in COLLISION_RENAMES {
        assert!(
            retirement.reason.contains(old) && retirement.reason.contains(new),
            "the retirement reason must document {old} -> {new}"
        );
        assert!(
            retirement.collisions[*old].contains(new),
            "the listed collision {old} must name its successor {new}"
        );
    }
    assert_eq!(
        retirement.collisions.keys().collect::<BTreeSet<_>>(),
        collisions.keys().collect::<BTreeSet<_>>(),
        "the production map lists exactly the documented collisions"
    );
}

/// The rename rule is enforced by the production parser, not only by the test
/// above: a retired id pointed at an unrelated live test is refused even though
/// that test is live and no other retired id maps to it, and the one real
/// collision is refused if it is not listed.
#[test]
fn the_production_parser_refuses_a_retired_id_mapped_onto_an_unrelated_live_test() {
    let path = repo_root().join(hermit_manifest_plan::retired_ids::RETIRED_IDS_FILE);
    let committed = std::fs::read_to_string(&path).unwrap();
    RetiredIds::parse(&committed).unwrap();

    let manifests = manifests();
    let unrelated = "c-programs/add-key-enosys";
    assert!(
        manifests.programs.contains_key(unrelated),
        "{unrelated} is live"
    );
    assert!(
        retired_ids()
            .retirement(RETIRED_BUCKET)
            .unwrap()
            .ids
            .values()
            .all(|new| new != unrelated),
        "{unrelated} is not already a successor, so only the rename rule can refuse it"
    );
    let rename = "\"backend-parity-c/aio-refusal\": \"c-programs/aio-refusal\"";
    assert_eq!(committed.matches(rename).count(), 1);
    let misdirected = committed.replace(
        rename,
        &format!("\"backend-parity-c/aio-refusal\": \"{unrelated}\""),
    );
    let error = RetiredIds::parse(&misdirected).unwrap_err();
    assert!(
        error.contains(
            "successor \"c-programs/add-key-enosys\" of \"backend-parity-c/aio-refusal\" is neither the prefix rename c-programs/aio-refusal nor a listed collision"
        ),
        "{error}"
    );

    let mut unlisted: JsonValue = serde_json::from_str(&committed).unwrap();
    let removed = unlisted["retirements"][0]["collisions"]
        .as_object_mut()
        .unwrap()
        .remove(COLLISION_RENAMES[0].0);
    assert!(removed.is_some());
    let error = RetiredIds::parse(&unlisted.to_string()).unwrap_err();
    assert!(
        error.contains(
            "is neither the prefix rename c-programs/pidfd-open-self nor a listed collision"
        ),
        "{error}"
    );
}

#[test]
fn the_retired_id_map_is_a_bijection_onto_live_ids() {
    let map = retired_ids();
    let manifests = manifests();
    let live = manifests.programs.keys().cloned().collect::<BTreeSet<_>>();
    map.check_live(&live, &manifests.buckets).unwrap();
    let retirement = map.retirement(RETIRED_BUCKET).unwrap();
    let successors = retirement.ids.values().collect::<BTreeSet<_>>();
    assert_eq!(successors.len(), FOLDED_TESTS, "no successor is shared");
    assert!(!manifests.buckets.contains(RETIRED_BUCKET));
    assert!(
        !repo_root()
            .join("tests/e2e/manifests/backend-parity-c.yaml")
            .exists()
    );
    assert!(
        live.iter().all(|id| !id.starts_with("backend-parity-c/")),
        "a backend-parity-c id is still declared"
    );
    // Every successor is a live c-programs test whose program sits in tests/c.
    // The collision kept both programs: the pre-existing test still names its
    // own source, and the moved one names the renamed copy.
    for new in &successors {
        let program = manifests.programs[new.as_str()]
            .as_deref()
            .unwrap_or_else(|| panic!("{new} has no program"));
        assert!(program.starts_with("tests/c/"), "{new} runs {program}");
        assert!(repo_root().join(program).is_file(), "{program} is missing");
    }
    assert_eq!(
        manifests.programs["c-programs/pidfd-open-self"].as_deref(),
        Some("tests/c/pidfd_open_self.c")
    );
    assert_eq!(
        manifests.programs["c-programs/pidfd-open-self-pair"].as_deref(),
        Some("tests/c/pidfd_open_self_pair.c")
    );
    // No two tests of the merged bucket may name one program: the fold must
    // not have pointed a moved test at an unrelated c-programs source.
    let mut owners = BTreeMap::<&str, Vec<&str>>::new();
    for (id, program) in &manifests.programs {
        if let (true, Some(program)) = (id.starts_with("c-programs/"), program) {
            owners.entry(program).or_default().push(id);
        }
    }
    for new in &successors {
        let program = manifests.programs[new.as_str()].as_deref().unwrap();
        assert_eq!(owners[program], vec![new.as_str()], "{program} is shared");
    }
}

#[test]
fn the_committed_plan_keeps_the_fold() {
    let plan = read_json("ci/expected-e2e-plan.json");
    let cells = plan["cells"].as_array().unwrap();
    // Every documented move is present in the committed plan exactly once, in
    // its destination lane, and absent from its source lane.
    for &(test, backend, mode, from, to) in LATER_LANE_MOVES {
        let matching = |lane_name: &str| {
            cells
                .iter()
                .filter(|c| {
                    field(c, "test") == test
                        && field(c, "backend") == backend
                        && field(c, "mode") == mode
                        && field(c, "lane") == lane_name
                })
                .count()
        };
        assert_eq!(
            (matching(from), matching(to)),
            (0, 1),
            "{test} {mode}/{backend}: {from} -> {to}"
        );
    }
    // The folded cells now belong to c-programs.
    let retirement = retired_ids();
    let successors = retirement.successors_of(RETIRED_BUCKET).unwrap();
    let mut folded = 0;
    for cell in cells {
        let category = field(cell, "category");
        assert_ne!(category, RETIRED_BUCKET, "{cell}");
        if successors.contains(field(cell, "test")) {
            assert_eq!(category, SUCCESSOR_BUCKET, "{cell}");
            folded += 1;
        }
    }
    assert!(folded > 0);
}

#[test]
fn the_committed_cell_table_keeps_no_retired_row() {
    let table = read_json("ci/compat-envelope/cells.json");
    let rows = table["cells"].as_array().unwrap();
    assert!(!rows.is_empty());
    for row in rows {
        assert_ne!(field(row, "category"), RETIRED_BUCKET, "{row}");
        assert!(
            !field(row, "test").starts_with("backend-parity-c/"),
            "{row}"
        );
    }
}

/// Run `test-harness` from the repository root and return `(code, stderr)`.
fn harness(args: &[&str]) -> (i32, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_test-harness"))
        .current_dir(repo_root())
        .args(args)
        .output()
        .expect("failed to run test-harness");
    let code = out
        .status
        .code()
        .unwrap_or_else(|| panic!("test-harness died on a signal for {args:?}"));
    (code, String::from_utf8_lossy(&out.stderr).into_owned())
}

#[test]
fn the_c_programs_selector_refuses_zero_cells() {
    // The flags are the ones the DAG generator writes into both c-programs
    // nodes, so this is the node's own selector, not a hand-copied spelling.
    let flags = hermit_manifest_plan::validation_dag::manifest_selector_flags(SUCCESSOR_BUCKET);
    assert!(!flags.contains("--allow-empty"), "{flags}");
    let results = std::env::temp_dir().join(format!(
        "backend-parity-c-fold-empty-{}.jsonl",
        std::process::id()
    ));
    let results = results.to_str().unwrap();
    // A portable test asked for in the privileged lane selects nothing.
    let mut args = vec![
        "run",
        "--lane",
        "privileged",
        "--category",
        SUCCESSOR_BUCKET,
        "--test",
        "c-programs/fork-exec-pipeline",
    ];
    args.extend(flags.split_whitespace());
    args.extend(["--results", results]);
    let (code, stderr) = harness(&args);
    assert_eq!(code, 2, "{stderr}");
    assert!(
        stderr.contains("filters selected no cells"),
        "the refusal must be the empty-selection one: {stderr}"
    );
    assert!(!Path::new(results).exists(), "a refused run wrote results");
}

/// `(lane, category)` of every committed CI-selected DBT plan cell.
fn dbt_plan_buckets() -> BTreeSet<(String, String)> {
    let plan = read_json("ci/expected-e2e-plan.json");
    plan["cells"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|cell| field(cell, "backend") == "dbt")
        .map(|cell| (field(cell, "lane").into(), field(cell, "category").into()))
        .collect()
}

/// The value after `flag` in a node command, if the flag appears once.
fn flag_value<'a>(cmd: &'a str, flag: &str) -> Option<&'a str> {
    let mut words = cmd.split_whitespace();
    words.position(|word| word == flag)?;
    words.next()
}

/// The retired test.dbt_parity node depended on check.dbt_runtime_abi so the
/// ABI check ran first and named a missing DBT runtime callback before
/// eager-exit could cancel it. Its cases now run in manifest nodes, so every
/// manifest node that selects a DBT cell and shares a dagrun profile with the
/// check has to carry the same ordering, and no other node needs it.
///
/// `hosted-portable` is not such a profile: the hosted workflow runs each E2E
/// node as its own job through ci/run-node.sh, which omits dependencies outside
/// the job's selection, and the ABI check runs in a separate release-shard job
/// that an E2E failure cannot cancel. ci/check-shard-coverage.sh refuses an E2E
/// edge to a node that no earlier hosted job supplies, so the _on_host twins
/// must not carry it.
#[test]
fn every_manifest_node_with_a_dbt_cell_orders_after_the_dbt_runtime_abi_check() {
    const ABI: &str = "check.dbt_runtime_abi";
    const HOSTED_PORTABLE: &str = "hosted-portable";
    let dag = read_json("ci/dag/validate.json");
    let steps = dag["steps"].as_array().unwrap();
    let tag = |step: &JsonValue| format!("{}.{}", field(step, "group"), field(step, "job"));
    let labels = |step: &JsonValue| {
        step["labels"]
            .as_array()
            .unwrap()
            .iter()
            .map(|label| label.as_str().unwrap().to_string())
            .collect::<BTreeSet<_>>()
    };
    let abi = steps
        .iter()
        .find(|step| tag(step) == ABI)
        .unwrap_or_else(|| panic!("{ABI} is missing from ci/dag/validate.json"));
    // Since the one-build change of 2026-09-30 the local check runs in the
    // pinned root, so hosted-portable runs its own host twin, which the hosted
    // E2E twins must still not depend on (see above).
    let hosted_abi = format!("{ABI}_on_host");
    let hosted = steps
        .iter()
        .find(|step| tag(step) == hosted_abi)
        .unwrap_or_else(|| panic!("{hosted_abi} is missing from ci/dag/validate.json"));
    assert_eq!(
        labels(hosted),
        BTreeSet::from([HOSTED_PORTABLE.to_string()]),
        "{hosted_abi} left the hosted-portable profile; revisit the _on_host twins"
    );
    let abi_labels = labels(abi);
    assert!(
        !abi_labels.contains(HOSTED_PORTABLE),
        "{ABI} is selected by hosted-portable besides its host twin {hosted_abi}"
    );
    assert!(!abi_labels.is_empty(), "{ABI} is in no dagrun profile");
    assert!(
        !steps.iter().any(|step| {
            step["deps"]
                .as_array()
                .unwrap()
                .iter()
                .any(|dep| dep.as_str() == Some(hosted_abi.as_str()))
        }),
        "a node depends on {hosted_abi}; the hosted E2E twins must not"
    );
    let dbt = dbt_plan_buckets();
    assert!(!dbt.is_empty(), "the committed plan selects no DBT cell");
    let mut needs = BTreeSet::new();
    let mut has = BTreeSet::new();
    for step in steps {
        let cmd = field(step, "cmd");
        if step["deps"]
            .as_array()
            .unwrap()
            .iter()
            .any(|dep| dep.as_str() == Some(ABI))
        {
            has.insert(tag(step));
        }
        if !cmd.contains("--ci-only") {
            continue;
        }
        let (Some(lane), Some(category)) =
            (flag_value(cmd, "--lane"), flag_value(cmd, "--category"))
        else {
            continue;
        };
        if dbt.contains(&(lane.to_string(), category.to_string()))
            && !labels(step).is_disjoint(&abi_labels)
        {
            needs.insert(tag(step));
        }
    }
    assert_eq!(
        needs,
        BTreeSet::from([
            "e2e.manifest_applications".to_string(),
            "e2e.manifest_bin_c".to_string(),
            "e2e.manifest_c_programs".to_string(),
            "e2e.manifest_debugger_c".to_string(),
            "e2e.manifest_determinism_stress_c".to_string(),
            "e2e.manifest_language_runtimes".to_string(),
            "e2e.manifest_shared_futex_c".to_string(),
            "e2e.manifest_system_utils".to_string(),
            "e2e.manifest_util_c".to_string(),
            "privileged-e2e.manifest_c_programs".to_string(),
        ])
    );
    // This counted integration target runs real DBT cases outside the
    // manifest buckets, so it must also wait for the same ABI check.
    let mut expected_dependents = needs;
    expected_dependents.insert("test.inherited_terminal_read".to_string());
    assert_eq!(has, expected_dependents);
}

/// The privileged c-programs nodes describe their selection by count and
/// backend; a description that still says "three" after S13 added the DBT
/// cell misstates what the node runs.
#[test]
fn the_privileged_c_programs_description_names_every_selected_cell() {
    let plan = read_json("ci/expected-e2e-plan.json");
    let backends = plan["cells"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|cell| {
            field(cell, "lane") == "privileged" && field(cell, "category") == SUCCESSOR_BUCKET
        })
        .map(|cell| {
            assert_eq!(field(cell, "test"), "c-programs/cpuid-probe", "{cell}");
            field(cell, "backend").to_string()
        })
        .collect::<Vec<_>>();
    let count = ["zero", "one", "two", "three", "four", "five", "six"][backends.len()];
    let dag = read_json("ci/dag/validate.json");
    let mut seen = 0;
    for step in dag["steps"].as_array().unwrap() {
        let cmd = field(step, "cmd");
        if !cmd.contains("--ci-only")
            || flag_value(cmd, "--lane") != Some("privileged")
            || flag_value(cmd, "--category") != Some(SUCCESSOR_BUCKET)
        {
            continue;
        }
        seen += 1;
        let description = field(step, "description");
        assert!(
            description.starts_with(&format!(
                "The selected cells are the {count} cpuid-probe verify cells ("
            )),
            "{}.{}: {description}",
            field(step, "group"),
            field(step, "job")
        );
        for backend in &backends {
            assert!(description.contains(backend), "{backend}: {description}");
        }
    }
    // 4 with privileged-e2e.manifest_c_programs_buck, the full-buck-e2e import
    // twin, which runs its counterpart's arguments and leads with its description.
    assert_eq!(seen, 4, "privileged c-programs nodes");
}
