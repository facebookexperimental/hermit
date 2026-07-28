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

readonly EXPECTED_ROWS_SHA256=3500e4318c539abc8887178622adfaa8746704ed40b3ada35d29ec5d78d5c247

function run_sqlite_workload {
    local work_dir=$1
    local database rows metadata rows_hash metadata_hash integrity

    database="$work_dir/application.db"
    rows="$work_dir/rows.txt"
    metadata="$work_dir/metadata.txt"
    rm -rf -- "$work_dir"
    mkdir -p -- "$work_dir"

    sqlite3 -batch "$database" >/dev/null <<'SQL'
PRAGMA journal_mode=DELETE;
CREATE TABLE records (id INTEGER PRIMARY KEY, label TEXT NOT NULL, value INTEGER NOT NULL);
INSERT INTO records VALUES (1, 'alpha', 11), (2, 'beta', 29), (3, 'gamma', 47);
CREATE TABLE run_metadata (observed_at TEXT NOT NULL, nonce TEXT NOT NULL);
INSERT INTO run_metadata
VALUES (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), lower(hex(randomblob(16))));
SQL

    [[ -s $database ]]
    integrity=$(sqlite3 -batch -noheader "$database" 'PRAGMA integrity_check;')
    [[ $integrity == ok ]]

    sqlite3 -batch -noheader -separator '|' "$database" \
        'SELECT id, label, value FROM records ORDER BY id;' >"$rows"
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

# shellcheck source=tests/e2e/applications/common.sh
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
