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
//! 2. The committed CI plan selects 1233 cells, with per-(lane, backend, mode)
//!    counts equal to the pre-fold plan's plus exactly the cells slice S13
//!    added (895 portable and 5 privileged), the 189 portable cells of the
//!    compatibility-corpus fold, the 2 portable select replay cells and the 6
//!    portable KVM verify selections, the three socket selections, and one
//!    poll-readiness, one epoll-pwait2, one fsync-durability, one fcntl-owner
//!    and one msync-writeback selection, and the 128 portable SaBRe verify
//!    cells promoted on 2026-10-03,
//!    after applying the later lane moves listed
//!    in `LATER_LANE_MOVES` (now 1226 portable and 7 privileged).
//! 3. The committed compatibility cell table has 14800 rows, with
//!    per-(backend, mode, status) counts equal to the pre-fold table's plus
//!    exactly the rows slice S13 added or reclassified and the 3024 rows of the
//!    compatibility-corpus fold, plus the 2 select replay and 6 KVM verify
//!    selection changes, plus three socket, one poll-readiness, one epoll-pwait2,
//!    one fsync-durability, one fcntl-owner and one msync-writeback selection,
//!    and the 128 SaBRe verify promotions of 2026-10-03,
//!    that keep the row total unchanged, plus the later SaBRe, strict and rr
//!    compatibility folds described below.
//! 4. The command the c-programs nodes run refuses a selection of zero cells,
//!    so folding more tests into that node cannot turn it into a vacuous pass.
//!
//! Two further checks pin slice S13's DAG edits: every manifest node that
//! selects a DBT cell and runs in one dagrun with check.dbt_runtime_abi orders
//! after it, as the retired test.dbt_parity node did, and the privileged
//! c-programs descriptions name the cells those nodes select.
//!
//! The pre-fold numbers are embedded, not recomputed: they were measured at
//! e8007f971a72c5fd92fcf3e17e41f38811c3cac3. The fold's parent on main is
//! 3f66a249b30fada86b81e722b8e5439ac0789f8e, the last commit that declared
//! backend-parity-c; the plan, the cell table and both manifests are
//! byte-identical at the two commits. A later change that moves a count has to
//! change this file and say why. Slice S13 of the same issue moved the retired
//! DBT backend-parity matrix onto manifest cells, and fold 1 of
//! <https://github.com/rrnewton/hermit/issues/3448> moved the strict
//! compatibility corpus into the manifest; their counts are listed separately
//! below so the pre-fold snapshot stays byte-for-byte what was measured.

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

/// Pre-fold `ci/expected-e2e-plan.json` cells per (lane, backend, mode).
const PLAN_COUNTS: &[(&str, &str, &str, usize)] = &[
    ("portable", "kvm", "verify", 242),
    ("portable", "liteinst", "custom", 1),
    ("portable", "liteinst", "verify", 145),
    ("portable", "ptrace", "chaos", 6),
    ("portable", "ptrace", "custom", 2),
    ("portable", "ptrace", "replay", 2),
    ("portable", "ptrace", "verify", 345),
    ("portable", "sabre", "verify", 112),
    ("privileged", "kvm", "verify", 2),
    ("privileged", "liteinst", "verify", 1),
    ("privileged", "ptrace", "verify", 1),
];

/// Plan cells slice S13 of <https://github.com/rrnewton/hermit/issues/3301>
/// added when it replaced `tests/backend-parity/run_matrix.py --backend dbt`
/// with manifest cells. Each of the 27 matrix cases that passed on DBT now has
/// a CI-selected DBT cell. 26 are verify cells: 25 portable, and
/// `c-programs/cpuid-probe`, whose test is in the privileged lane. The
/// remaining case, `c-programs/io-uring-fallback`, fails DBT verification
/// because of <https://github.com/rrnewton/reverie/issues/764>, so its DBT
/// verify cell stays enabled but unselected (red). A custom-mode cell carries
/// the matrix's plain `--strict` run on DBT instead, and the ptrace custom cell
/// that the matrix-symmetry rule requires comes with it. The 13 tests S13
/// declared for matrix cases that had no manifest test also carry a ptrace
/// verify cell against the same expected stdout. Nothing else moved.
const S13_PLAN_ADDITIONS: &[(&str, &str, &str, usize)] = &[
    ("portable", "dbt", "custom", 1),
    ("portable", "dbt", "verify", 25),
    ("portable", "ptrace", "custom", 1),
    ("portable", "ptrace", "verify", 13),
    ("privileged", "dbt", "verify", 1),
];

/// Fold 1 of <https://github.com/rrnewton/hermit/issues/3448> moved the
/// portable strict compatibility corpus from 189 generated validation nodes into
/// `tests/e2e/manifests/compat.yaml`. Each of its 189 programs adds one
/// selected portable ptrace verify cell to the plan. In the cell table each adds
/// the 16 rows every test has: its ptrace verify row is selected (green), and
/// its other 15 rows are not applicable, because it declares only that cell
/// enabled.
const COMPAT_FOLD_TESTS: usize = 189;
const COMPAT_FOLD_PLAN_ADDITIONS: &[(&str, &str, &str, usize)] =
    &[("portable", "ptrace", "verify", COMPAT_FOLD_TESTS)];

fn compat_fold_cell_deltas() -> Vec<(&'static str, &'static str, &'static str, isize)> {
    let tests = COMPAT_FOLD_TESTS as isize;
    let mut deltas = vec![
        ("ptrace", "verify", "green", tests),
        ("native", "naked", "not-applicable", tests),
    ];
    for backend in ["dbt", "kvm", "liteinst", "sabre"] {
        deltas.push((backend, "verify", "not-applicable", tests));
    }
    for backend in ["dbt", "kvm", "liteinst", "ptrace", "sabre"] {
        deltas.push((backend, "chaos", "not-applicable", tests));
        deltas.push((backend, "replay", "not-applicable", tests));
    }
    deltas
}

/// <https://github.com/rrnewton/hermit/pull/3580> taught the recorder and
/// replayer `select` and `pselect6` and selected the portable ptrace replay cell
/// of `c-programs/poll-readiness` and `c-programs/pselect6-simulation`. Both
/// tests already existed, so the cell table keeps its row count: each test's
/// ptrace replay row moves from not applicable to selected (green).
const SELECT_REPLAY_TESTS: usize = 2;
const SELECT_REPLAY_PLAN_ADDITIONS: &[(&str, &str, &str, usize)] =
    &[("portable", "ptrace", "replay", SELECT_REPLAY_TESTS)];
const SELECT_REPLAY_CELL_DELTAS: &[(&str, &str, &str, isize)] = &[
    ("ptrace", "replay", "green", SELECT_REPLAY_TESTS as isize),
    (
        "ptrace",
        "replay",
        "not-applicable",
        -(SELECT_REPLAY_TESTS as isize),
    ),
];

/// <https://github.com/rrnewton/reverie/issues/891> qualified six existing
/// portable KVM verify cells. Their selection adds six plan cells; the catalogue
/// moves six existing rows from not applicable to selected (green), without
/// adding rows or changing any guest assertion or bound.
const KVM_2026_10_03_PLAN_ADDITIONS: &[(&str, &str, &str, usize)] =
    &[("portable", "kvm", "verify", 6)];
const KVM_2026_10_03_CELL_DELTAS: &[(&str, &str, &str, isize)] = &[
    ("kvm", "verify", "green", 6),
    ("kvm", "verify", "not-applicable", -6),
];

/// Three further socket KVM verify selections from
/// <https://github.com/rrnewton/reverie/issues/891>; preserve the earlier six
/// and all frozen pre-fold populations as independent additive changes.
const KVM_SOCKET_PLAN_ADDITIONS: &[(&str, &str, &str, usize)] = &[("portable", "kvm", "verify", 3)];
const KVM_SOCKET_CELL_DELTAS: &[(&str, &str, &str, isize)] = &[
    ("kvm", "verify", "green", 3),
    ("kvm", "verify", "not-applicable", -3),
];

/// <https://github.com/rrnewton/reverie/issues/620> qualifies the unchanged
/// poll-readiness KVM verify fixture: one required cell, with every original
/// guest assertion, comparator and bound intact. Replay remains disabled.
const KVM_PSELECT_PLAN_ADDITIONS: &[(&str, &str, &str, usize)] =
    &[("portable", "kvm", "verify", 1)];
const KVM_PSELECT_CELL_DELTAS: &[(&str, &str, &str, isize)] = &[
    ("kvm", "verify", "green", 1),
    ("kvm", "verify", "not-applicable", -1),
];

/// Fold 3 of <https://github.com/rrnewton/hermit/issues/3448> moved the SaBRe
/// compatibility corpus from 212 generated validation nodes into the same
/// manifest, as SaBRe verify cells that only the sabre-compat-only run type
/// selects. 185 of its programs were already rows, so each of their SaBRe
/// verify rows went from not applicable to red (enabled, not selected by the
/// full validation). The other 27 became new rows whose ptrace and SaBRe verify
/// cells are both red for the same reason, with their other 14 rows not
/// applicable. Three more new rows, lua-direct, perl-direct and df-direct,
/// keep the SaBRe corpus's single-image argv for programs whose existing rows
/// run under a bash wrapper. The plan of the full validation is unchanged.
const SABRE_FOLD_EXISTING_ROWS: usize = 185;
const SABRE_FOLD_NEW_ROWS: usize = 27 + 3;

fn sabre_fold_cell_deltas() -> Vec<(&'static str, &'static str, &'static str, isize)> {
    let existing = SABRE_FOLD_EXISTING_ROWS as isize;
    let new = SABRE_FOLD_NEW_ROWS as isize;
    let mut deltas = vec![
        ("sabre", "verify", "not-applicable", -existing),
        ("sabre", "verify", "red", existing + new),
        ("ptrace", "verify", "red", new),
        ("native", "naked", "not-applicable", new),
    ];
    for backend in ["dbt", "kvm", "liteinst"] {
        deltas.push((backend, "verify", "not-applicable", new));
    }
    for backend in ["dbt", "kvm", "liteinst", "ptrace", "sabre"] {
        deltas.push((backend, "chaos", "not-applicable", new));
        deltas.push((backend, "replay", "not-applicable", new));
    }
    deltas
}

/// Fold 4 of the same issue moved the strict compatibility run type from 193
/// generated validation nodes into the manifest as variant tests
/// compat/strict-<row>, one per program of ci/compat/corpus-strict.json. Each
/// adds the 16 rows every test has: its ptrace verify row is red (enabled, and
/// only the strict-compat-only run type selects it), and its other 15 rows are
/// not applicable. The plan of the full validation is unchanged.
const STRICT_FOLD_TESTS: usize = 193;

fn strict_fold_cell_deltas() -> Vec<(&'static str, &'static str, &'static str, isize)> {
    let tests = STRICT_FOLD_TESTS as isize;
    let mut deltas = vec![
        ("ptrace", "verify", "red", tests),
        ("native", "naked", "not-applicable", tests),
    ];
    for backend in ["dbt", "kvm", "liteinst", "sabre"] {
        deltas.push((backend, "verify", "not-applicable", tests));
    }
    for backend in ["dbt", "kvm", "liteinst", "ptrace", "sabre"] {
        deltas.push((backend, "chaos", "not-applicable", tests));
        deltas.push((backend, "replay", "not-applicable", tests));
    }
    deltas
}

/// Fold 5 of the same issue moved the rr compatibility run type from 139
/// generated validation nodes into the manifest as replay variant tests
/// compat/rr-<row>, one per program the retired rr lane listed as passing.
/// Each adds the 16 rows every test has: its ptrace replay row is red
/// (enabled, and only the rr-compat-only run type selects it), and its other 15
/// rows, its ptrace verify row among them, are not applicable. The plan of the
/// full validation is unchanged.
const RR_FOLD_TESTS: usize = 139;

fn rr_fold_cell_deltas() -> Vec<(&'static str, &'static str, &'static str, isize)> {
    let tests = RR_FOLD_TESTS as isize;
    let mut deltas = vec![
        ("ptrace", "replay", "red", tests),
        ("native", "naked", "not-applicable", tests),
    ];
    for backend in ["dbt", "kvm", "liteinst", "ptrace", "sabre"] {
        deltas.push((backend, "chaos", "not-applicable", tests));
        deltas.push((backend, "verify", "not-applicable", tests));
    }
    for backend in ["dbt", "kvm", "liteinst", "sabre"] {
        deltas.push((backend, "replay", "not-applicable", tests));
    }
    deltas
}

/// <https://github.com/rrnewton/reverie/issues/905> qualifies the unchanged
/// epoll-pwait2 KVM verify fixture: one additional required cell with all seven
/// checks and the original bounds intact. Replay stays disabled.
const KVM_EPOLL_PWAIT2_PLAN_ADDITIONS: &[(&str, &str, &str, usize)] =
    &[("portable", "kvm", "verify", 1)];
const KVM_EPOLL_PWAIT2_CELL_DELTAS: &[(&str, &str, &str, isize)] = &[
    ("kvm", "verify", "green", 1),
    ("kvm", "verify", "not-applicable", -1),
];

/// <https://github.com/rrnewton/reverie/issues/838> adds ordinary syncfs
/// forwarding for the unchanged fsync-durability KVM verify fixture:
/// one required cell, all six checks and
/// original bounds retained. Replay stays disabled.
const KVM_FSYNC_PLAN_ADDITIONS: &[(&str, &str, &str, usize)] = &[("portable", "kvm", "verify", 1)];
const KVM_FSYNC_CELL_DELTAS: &[(&str, &str, &str, isize)] = &[
    ("kvm", "verify", "green", 1),
    ("kvm", "verify", "not-applicable", -1),
];

/// <https://github.com/rrnewton/reverie/pull/910> supplies the proven pipe
/// owner/signal configuration used by the unchanged fcntl-owner fixture.
/// One required KVM verify cell, all six checks and original bounds retained;
/// this selection does not claim asynchronous signal delivery. Replay stays disabled.
const KVM_FCNTL_OWNER_PLAN_ADDITIONS: &[(&str, &str, &str, usize)] =
    &[("portable", "kvm", "verify", 1)];
const KVM_FCNTL_OWNER_CELL_DELTAS: &[(&str, &str, &str, isize)] = &[
    ("kvm", "verify", "green", 1),
    ("kvm", "verify", "not-applicable", -1),
];

/// <https://github.com/rrnewton/reverie/issues/891> tracks the msync-writeback
/// KVM verify selection: one required cell with its five checks, exact writeback
/// assertions and original bounds retained. Replay stays disabled.
const KVM_MSYNC_PLAN_ADDITIONS: &[(&str, &str, &str, usize)] = &[("portable", "kvm", "verify", 1)];
const KVM_MSYNC_CELL_DELTAS: &[(&str, &str, &str, isize)] = &[
    ("kvm", "verify", "green", 1),
    ("kvm", "verify", "not-applicable", -1),
];

/// The 2026-10-03 SaBRe promotion (`SABRE_2026_10_03_*` in
/// `ci/manifest-plan/src/timeouts.rs`): 128 SaBRe verify cells that passed 10 of
/// 10 no-retry pressure runs become required, all portable. 120 of them were
/// declared not applicable and 8 were enabled but kept out of CI (red); 116 are
/// c-programs cells, 10 system-utils, 1 determinism-stress-c and 1
/// language-runtimes. The ordinary bounds are unchanged.
const SABRE_2026_10_03_PLAN_ADDITIONS: &[(&str, &str, &str, usize)] =
    &[("portable", "sabre", "verify", 128)];
const SABRE_2026_10_03_CELL_DELTAS: &[(&str, &str, &str, isize)] = &[
    ("sabre", "verify", "green", 128),
    ("sabre", "verify", "not-applicable", -120),
    ("sabre", "verify", "red", -8),
];

/// Cells that later changes moved between lanes after the fold, as
/// (test, backend, mode, from lane, to lane). Each move keeps the cell and only
/// changes which lane runs it, so the total and the per-(backend, mode) counts
/// stay those of the pre-fold plan plus `S13_PLAN_ADDITIONS`.
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

/// Pre-fold `ci/compat-envelope/cells.json` rows per (backend, mode, status).
const CELL_COUNTS: &[(&str, &str, &str, usize)] = &[
    ("dbt", "chaos", "not-applicable", 361),
    ("dbt", "replay", "not-applicable", 361),
    ("dbt", "verify", "not-applicable", 300),
    ("dbt", "verify", "red", 61),
    ("kvm", "chaos", "not-applicable", 361),
    ("kvm", "replay", "not-applicable", 361),
    ("kvm", "verify", "green", 244),
    ("kvm", "verify", "not-applicable", 109),
    ("kvm", "verify", "red", 8),
    ("liteinst", "chaos", "not-applicable", 361),
    ("liteinst", "replay", "not-applicable", 361),
    ("liteinst", "verify", "green", 146),
    ("liteinst", "verify", "not-applicable", 212),
    ("liteinst", "verify", "red", 3),
    ("native", "naked", "not-applicable", 328),
    ("native", "naked", "red", 33),
    ("ptrace", "chaos", "green", 6),
    ("ptrace", "chaos", "not-applicable", 354),
    ("ptrace", "chaos", "red", 1),
    ("ptrace", "replay", "green", 2),
    ("ptrace", "replay", "not-applicable", 359),
    ("ptrace", "verify", "green", 346),
    ("ptrace", "verify", "not-applicable", 3),
    ("ptrace", "verify", "red", 12),
    ("sabre", "chaos", "not-applicable", 361),
    ("sabre", "replay", "not-applicable", 361),
    ("sabre", "verify", "green", 112),
    ("sabre", "verify", "not-applicable", 217),
    ("sabre", "verify", "red", 32),
];

/// Rows slice S13 changed in `ci/compat-envelope/cells.json`, per (backend,
/// mode, status). The 13 tests it declared add 16 rows each (208). The DBT
/// verify rows are 26 newly selected cells (13 from the new tests, 10 existing
/// tests whose DBT verify was disabled and so not applicable, and 3 whose DBT
/// verify was enabled but not selected and so red), plus
/// `c-programs/io-uring-fallback`, whose DBT verify moved from disabled (not
/// applicable) to enabled but unselected (red). Each new test's ptrace verify
/// row is selected; its other rows are not applicable. The table has no rows
/// for custom mode, so the two io-uring-fallback custom cells do not appear.
const S13_CELL_DELTAS: &[(&str, &str, &str, isize)] = &[
    ("dbt", "chaos", "not-applicable", 13),
    ("dbt", "replay", "not-applicable", 13),
    ("dbt", "verify", "green", 26),
    ("dbt", "verify", "not-applicable", -11),
    ("dbt", "verify", "red", -2),
    ("kvm", "chaos", "not-applicable", 13),
    ("kvm", "replay", "not-applicable", 13),
    ("kvm", "verify", "not-applicable", 13),
    ("liteinst", "chaos", "not-applicable", 13),
    ("liteinst", "replay", "not-applicable", 13),
    ("liteinst", "verify", "not-applicable", 13),
    ("native", "naked", "not-applicable", 13),
    ("ptrace", "chaos", "not-applicable", 13),
    ("ptrace", "replay", "not-applicable", 13),
    ("ptrace", "verify", "green", 13),
    ("sabre", "chaos", "not-applicable", 13),
    ("sabre", "replay", "not-applicable", 13),
    ("sabre", "verify", "not-applicable", 13),
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
fn the_committed_plan_keeps_its_cell_counts() {
    let plan = read_json("ci/expected-e2e-plan.json");
    let cells = plan["cells"].as_array().unwrap();
    let lane = |name: &str| cells.iter().filter(|c| field(c, "lane") == name).count();
    let moved_out = |name: &str| {
        LATER_LANE_MOVES
            .iter()
            .filter(|&&(_, _, _, from, _)| from == name)
            .count()
    };
    let moved_in = |name: &str| {
        LATER_LANE_MOVES
            .iter()
            .filter(|&&(_, _, _, _, to)| to == name)
            .count()
    };
    // One zero-time poll-readiness KVM verify selection: https://github.com/rrnewton/reverie/issues/620.
    // One zero-time epoll-pwait2 KVM verify selection: https://github.com/rrnewton/reverie/issues/905.
    // One ordinary syncfs KVM selection: https://github.com/rrnewton/reverie/issues/838.
    // One pipe owner/signal KVM selection: https://github.com/rrnewton/reverie/pull/910.
    // One msync writeback KVM selection: https://github.com/rrnewton/reverie/issues/891.
    // The 128 portable SaBRe verify cells of `SABRE_2026_10_03_PLAN_ADDITIONS`.
    assert_eq!(
        (cells.len(), lane("portable"), lane("privileged")),
        (
            900 + COMPAT_FOLD_TESTS + SELECT_REPLAY_TESTS + 6 + 3 + 1 + 1 + 1 + 1 + 1 + 128,
            895 + COMPAT_FOLD_TESTS + SELECT_REPLAY_TESTS + 6 + 3 + 1 + 1 + 1 + 1 + 1 + 128
                - moved_out("portable")
                + moved_in("portable"),
            5 - moved_out("privileged") + moved_in("privileged"),
        )
    );
    assert_eq!(
        (lane("portable"), lane("privileged")),
        (
            893 + COMPAT_FOLD_TESTS + SELECT_REPLAY_TESTS + 6 + 3 + 1 + 1 + 1 + 1 + 1 + 128,
            7
        ),
        "the lane moves above are the only ones since the fold"
    );
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
    let mut counts = BTreeMap::<(String, String, String), usize>::new();
    for cell in cells {
        let key = (
            field(cell, "lane").to_string(),
            field(cell, "backend").to_string(),
            field(cell, "mode").to_string(),
        );
        *counts.entry(key).or_default() += 1;
    }
    let mut expected = PLAN_COUNTS
        .iter()
        .map(|&(lane, backend, mode, n)| ((lane.into(), backend.into(), mode.into()), n))
        .collect::<BTreeMap<(String, String, String), usize>>();
    for &(lane, backend, mode, n) in S13_PLAN_ADDITIONS
        .iter()
        .chain(COMPAT_FOLD_PLAN_ADDITIONS)
        .chain(SELECT_REPLAY_PLAN_ADDITIONS)
        .chain(KVM_2026_10_03_PLAN_ADDITIONS)
        .chain(KVM_SOCKET_PLAN_ADDITIONS)
        .chain(KVM_PSELECT_PLAN_ADDITIONS)
        .chain(KVM_EPOLL_PWAIT2_PLAN_ADDITIONS)
        .chain(KVM_FSYNC_PLAN_ADDITIONS)
        .chain(KVM_FCNTL_OWNER_PLAN_ADDITIONS)
        .chain(KVM_MSYNC_PLAN_ADDITIONS)
        .chain(SABRE_2026_10_03_PLAN_ADDITIONS)
    {
        *expected
            .entry((lane.into(), backend.into(), mode.into()))
            .or_default() += n;
    }
    for &(_, backend, mode, from, to) in LATER_LANE_MOVES {
        let source = expected
            .get_mut(&(from.into(), backend.into(), mode.into()))
            .unwrap_or_else(|| panic!("no pre-fold {from} {mode}/{backend} cell to move"));
        *source -= 1;
        *expected
            .entry((to.into(), backend.into(), mode.into()))
            .or_default() += 1;
    }
    expected.retain(|_, n| *n > 0);
    assert_eq!(counts, expected);
    // The folded cells now belong to c-programs: 437 portable c-programs cells
    // and 276 portable plus 3 privileged backend-parity-c cells before the fold,
    // plus the 29 portable and 1 privileged c-programs cells S13 added and the
    // two portable ptrace replay cells of `SELECT_REPLAY_PLAN_ADDITIONS` and
    // six portable KVM verify cells of `KVM_2026_10_03_PLAN_ADDITIONS` and
    // three socket KVM verify cells of `KVM_SOCKET_PLAN_ADDITIONS`, plus the one
    // poll-readiness cell of `KVM_PSELECT_PLAN_ADDITIONS` and one epoll-pwait2
    // cell of `KVM_EPOLL_PWAIT2_PLAN_ADDITIONS`, plus one fsync-durability
    // cell of `KVM_FSYNC_PLAN_ADDITIONS` and one fcntl-owner cell
    // of `KVM_FCNTL_OWNER_PLAN_ADDITIONS`, plus one msync-writeback cell
    // of `KVM_MSYNC_PLAN_ADDITIONS` above, and the 116 c-programs cells of
    // `SABRE_2026_10_03_PLAN_ADDITIONS`.
    let retirement = retired_ids();
    let successors = retirement.successors_of(RETIRED_BUCKET).unwrap();
    let mut by_bucket = BTreeMap::<(String, String), usize>::new();
    for cell in cells {
        let category = field(cell, "category");
        assert_ne!(category, RETIRED_BUCKET, "{cell}");
        if successors.contains(field(cell, "test")) {
            assert_eq!(category, SUCCESSOR_BUCKET, "{cell}");
        }
        if category == SUCCESSOR_BUCKET {
            *by_bucket
                .entry((field(cell, "lane").into(), category.into()))
                .or_default() += 1;
        }
    }
    assert_eq!(
        by_bucket,
        BTreeMap::from([
            (
                ("portable".into(), "c-programs".into()),
                437 + 276 + 29 + SELECT_REPLAY_TESTS + 6 + 3 + 1 + 1 + 1 + 1 + 1 + 116
            ),
            (("privileged".into(), "c-programs".into()), 3 + 1),
        ])
    );
}

#[test]
fn the_committed_cell_table_keeps_its_row_counts() {
    let table = read_json("ci/compat-envelope/cells.json");
    let rows = table["cells"].as_array().unwrap();
    assert_eq!(
        rows.len(),
        5776 + 208
            + 16 * COMPAT_FOLD_TESTS
            + 16 * SABRE_FOLD_NEW_ROWS
            + 16 * STRICT_FOLD_TESTS
            + 16 * RR_FOLD_TESTS
    );
    let mut counts = BTreeMap::<(String, String, String), usize>::new();
    for row in rows {
        assert_ne!(field(row, "category"), RETIRED_BUCKET, "{row}");
        assert!(
            !field(row, "test").starts_with("backend-parity-c/"),
            "{row}"
        );
        let key = (
            field(row, "backend").to_string(),
            field(row, "mode").to_string(),
            field(row, "status").to_string(),
        );
        *counts.entry(key).or_default() += 1;
    }
    let mut expected = CELL_COUNTS
        .iter()
        .map(|&(backend, mode, status, n)| ((backend.into(), mode.into(), status.into()), n))
        .collect::<BTreeMap<(String, String, String), usize>>();
    for (backend, mode, status, delta) in S13_CELL_DELTAS
        .iter()
        .copied()
        .chain(compat_fold_cell_deltas())
        .chain(SELECT_REPLAY_CELL_DELTAS.iter().copied())
        .chain(KVM_2026_10_03_CELL_DELTAS.iter().copied())
        .chain(KVM_SOCKET_CELL_DELTAS.iter().copied())
        .chain(KVM_PSELECT_CELL_DELTAS.iter().copied())
        .chain(sabre_fold_cell_deltas())
        .chain(strict_fold_cell_deltas())
        .chain(rr_fold_cell_deltas())
        .chain(KVM_EPOLL_PWAIT2_CELL_DELTAS.iter().copied())
        .chain(KVM_FSYNC_CELL_DELTAS.iter().copied())
        .chain(KVM_FCNTL_OWNER_CELL_DELTAS.iter().copied())
        .chain(KVM_MSYNC_CELL_DELTAS.iter().copied())
        .chain(SABRE_2026_10_03_CELL_DELTAS.iter().copied())
    {
        let count = expected
            .entry((backend.into(), mode.into(), status.into()))
            .or_default();
        *count = count
            .checked_add_signed(delta)
            .unwrap_or_else(|| panic!("delta {delta} underflows {backend}/{mode}/{status}"));
    }
    expected.retain(|_, count| *count != 0);
    assert_eq!(counts, expected);
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
            "e2e.manifest_c_programs".to_string(),
            "e2e.manifest_system_utils".to_string(),
            "privileged-e2e.manifest_c_programs".to_string(),
        ])
    );
    assert_eq!(has, needs);
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
    assert_eq!(seen, 3, "privileged c-programs nodes");
}
