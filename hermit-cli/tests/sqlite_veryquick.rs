/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;

fn sqlite3() -> PathBuf {
    ["/usr/bin/sqlite3", "/usr/local/bin/sqlite3"]
        .into_iter()
        .map(PathBuf::from)
        .find(|path| path.is_file())
        .expect("sqlite3 is required for the fast SQLite integration test")
}

fn run_fast_sqlite(hermit: &Path, sqlite: &Path, database: &Path) -> Output {
    const SQL: &str = "\
PRAGMA journal_mode=WAL;
CREATE TABLE accounts(id INTEGER PRIMARY KEY, balance INTEGER NOT NULL);
BEGIN IMMEDIATE;
INSERT INTO accounts VALUES (1, 40), (2, 2), (3, 13);
UPDATE accounts SET balance = balance + 1 WHERE id IN (1, 2);
COMMIT;
CREATE INDEX accounts_balance ON accounts(balance);
SELECT count(*), sum(balance) FROM accounts;
PRAGMA integrity_check;";

    Command::new(hermit)
        .args(["--log", "off", "run", "--strict", "--"])
        .arg(sqlite)
        .arg(database)
        .arg(SQL)
        .output()
        .expect("failed to run the fast SQLite workload under Hermit")
}

#[test]
fn sqlite_fast_subset_is_deterministic_under_strict_hermit() {
    let hermit = Path::new(env!("CARGO_BIN_EXE_hermit"));
    let sqlite = sqlite3();
    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("sqlite-fast");
    if root.exists() {
        fs::remove_dir_all(&root).expect("failed to remove stale fast SQLite directory");
    }
    fs::create_dir_all(&root).expect("failed to create fast SQLite directory");

    let first = run_fast_sqlite(hermit, &sqlite, &root.join("run-1.db"));
    let second = run_fast_sqlite(hermit, &sqlite, &root.join("run-2.db"));

    assert!(
        first.status.success() && second.status.success(),
        "fast SQLite runs failed:\nfirst={first:?}\nsecond={second:?}"
    );
    assert_eq!(first.stdout, second.stdout, "SQLite stdout differed");
    assert_eq!(first.stderr, second.stderr, "SQLite stderr differed");
    assert_eq!(
        String::from_utf8(first.stdout).expect("SQLite stdout was not UTF-8"),
        "wal\n3|57\nok\n"
    );
}

#[test]
#[ignore = "downloads/builds SQLite and runs the slow veryquick compatibility probe"]
fn sqlite_veryquick_is_deterministic_under_strict_hermit() {
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository root");
    let runner = repo_root.join("experiments/sqlite-veryquick/run.sh");
    let hermit =
        std::env::var_os("HERMIT_BIN").unwrap_or_else(|| env!("CARGO_BIN_EXE_hermit").into());
    let artifact_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("sqlite-veryquick");

    let output = Command::new(&runner)
        .env("HERMIT_BIN", hermit)
        .env("SQLITE_VERYQUICK_ARTIFACT_ROOT", artifact_root)
        .output()
        .unwrap_or_else(|error| panic!("failed to start {}: {error}", runner.display()));

    assert!(
        output.status.success(),
        "SQLite veryquick harness failed with {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains(
            "SQLite 3.51.2 veryquick: reproduced lock4 stall with 13 identical pre-stall failures."
        ),
        "SQLite veryquick harness did not report deterministic success:\n{}",
        String::from_utf8_lossy(&output.stdout),
    );
}
