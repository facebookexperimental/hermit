#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

set -euo pipefail
export LC_ALL=C
export PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
export TZ=UTC

readonly EXPECTED_ROWS_SHA256=b44b616cd6823208d1f28a458acea5ff742e1b030adf03b6b80e0cdaa01c2ede

function run_sqlite_workload {
    local work_dir=$1
    local database rows metadata plan rows_hash metadata_hash integrity

    database="$work_dir/application.db"
    rows="$work_dir/rows.txt"
    metadata="$work_dir/metadata.txt"
    plan="$work_dir/plan.txt"
    rm -rf -- "$work_dir"
    mkdir -p -- "$work_dir"

    sqlite3 -batch "$database" >/dev/null <<'SQL'
PRAGMA journal_mode=DELETE;
PRAGMA synchronous=FULL;
PRAGMA mmap_size=1048576;

CREATE TABLE records(
  id INTEGER PRIMARY KEY,
  category TEXT NOT NULL,
  value INTEGER NOT NULL,
  note TEXT NOT NULL
);

-- Keep this fixture small while exercising a committed multi-row transaction.
BEGIN IMMEDIATE;
WITH RECURSIVE sequence(id) AS (
  VALUES(1) UNION ALL SELECT id + 1 FROM sequence WHERE id < 48
)
INSERT INTO records(id, category, value, note)
SELECT id,
       CASE id % 3 WHEN 0 THEN 'alpha' WHEN 1 THEN 'beta' ELSE 'gamma' END,
       id * 7,
       printf('row-%02d', id)
FROM sequence;
UPDATE records SET value = value + 5 WHERE id % 10 = 0;
COMMIT;

CREATE INDEX idx_records_category_value ON records(category, value);

-- Exercise rollback and journal cleanup without changing the expected rows.
BEGIN;
INSERT INTO records VALUES(999, 'rolled-back', 999, 'must-not-persist');
DELETE FROM records WHERE id <= 3;
ROLLBACK;

CREATE TABLE run_metadata (observed_at TEXT NOT NULL, nonce TEXT NOT NULL);
INSERT INTO run_metadata
VALUES (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), lower(hex(randomblob(16))));
SQL

    [[ -s $database ]]
    integrity=$(sqlite3 -batch -noheader "$database" 'PRAGMA integrity_check;')
    [[ $integrity == ok ]]

    sqlite3 -batch -noheader "$database" >"$plan" <<'SQL'
EXPLAIN QUERY PLAN
SELECT id, value FROM records INDEXED BY idx_records_category_value
WHERE category = 'alpha' AND value >= 70 ORDER BY value;
SQL
    grep -Fq 'USING COVERING INDEX idx_records_category_value' "$plan"

    sqlite3 -batch -noheader -separator '|' "$database" >"$rows" <<'SQL'
PRAGMA mmap_size=1048576;
SELECT category, COUNT(*), SUM(value), MIN(value), MAX(value)
FROM records GROUP BY category ORDER BY category;
SELECT id, category, value
FROM records INDEXED BY idx_records_category_value
WHERE category = 'alpha' AND value >= 70 ORDER BY value, id LIMIT 8;
SELECT COUNT(*) FROM records WHERE category = 'rolled-back';
SQL
    sqlite3 -batch -noheader -separator '|' "$database" \
        'SELECT observed_at, nonce FROM run_metadata;' >"$metadata"

    rows_hash=$(sha256sum "$rows" | cut -d' ' -f1)
    metadata_hash=$(sha256sum "$metadata" | cut -d' ' -f1)
    [[ $rows_hash == "$EXPECTED_ROWS_SHA256" ]]
    printf 'sqlite-on-disk:%s:%s\n' "$rows_hash" "$metadata_hash"
}

if [[ ${1:-} == --guest ]]; then
    run_sqlite_workload "$2"
    exit
fi

# shellcheck source=tests/e2e/lib/applications/common.sh
source "$(dirname -- "$0")/common.sh"
require_commands sqlite3 sha256sum timeout

work_root=$(mktemp -d "${TMPDIR:-/tmp}/hermit-sqlite-e2e.XXXXXX")
trap 'rm -rf -- "$work_root"' EXIT

native_first=$(run_sqlite_workload "$work_root/native")
native_second=$(run_sqlite_workload "$work_root/native")
assert_native_nondeterminism 'SQLite on-disk workload' "$native_first" "$native_second"

run_hermit_verify 'SQLite on-disk workload' \
    /bin/bash "$0" --guest "$work_root/verified" >/dev/null
printf 'sqlite-on-disk:verified:%s\n' "$EXPECTED_ROWS_SHA256"
