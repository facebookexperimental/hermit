#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
#
# End-to-end SQLite database determinism fixture -- the first e2e coverage of a
# real database engine.
#
# SQLite seeds its PRNG once per process from OS entropy (the unix VFS reads
# /dev/urandom), so random()/randomblob() vary every run natively. strftime()
# with 'now' reads the clock, which also varies natively. Under Hermit --strict
# both channels are determinized, so an otherwise deterministic relational
# workload -- table build, on-disk persistence, reopen, aggregates, and ordered
# projections -- produces bitwise-identical query output across runs. The
# aggregate and ordered-row lines are deterministic by construction and act as a
# cross-check that is stable natively and under Hermit.
set -euo pipefail

case ${1:-} in
    --prepare)
        command -v sqlite3 >/dev/null 2>&1 || {
            echo "sqlite3 not found" >&2
            exit 1
        }
        exit 0
        ;;
    --run)
        # Hermit gives the guest a fresh isolated /tmp per repeat; create the
        # working directory before writing the database file.
        work="${E2E_TMPDIR:-/tmp}/hermit-sqlite-query-determinism"
        rm -rf "$work"
        mkdir -p "$work"
        db="$work/data.db"

        # Build a small relational dataset on disk (deterministic by
        # construction: explicit rowids, fixed values).
        sqlite3 "$db" <<'SQL'
CREATE TABLE items(id INTEGER PRIMARY KEY, name TEXT NOT NULL, val INTEGER NOT NULL);
INSERT INTO items(name, val) VALUES
  ('alpha', 10), ('beta', 20), ('gamma', 30),
  ('delta', 40), ('epsilon', 50), ('zeta', 60);
CREATE INDEX idx_items_val ON items(val);
SQL

        # Reopen the on-disk database and query it. The 'agg' and 'row' lines are
        # deterministic; the 'rand' line draws from SQLite's /dev/urandom-seeded
        # PRNG and the 'time' line reads the clock -- both host-nondeterministic
        # natively and determinized by Hermit.
        out=$(sqlite3 "$db" <<'SQL'
.mode list
.separator |
SELECT 'agg', COUNT(*), SUM(val), MIN(name), MAX(name) FROM items;
SELECT 'row', id, name, val FROM items ORDER BY val DESC;
SELECT 'rand', abs(random()) % 1000000, lower(hex(randomblob(8)));
SELECT 'time', strftime('%s', 'now');
SQL
)

        printf '%s\n' "$out"
        printf 'SQLITE sha=%s\n' "$(printf '%s' "$out" | sha256sum | cut -d' ' -f1)"
        ;;
    *)
        echo "usage: $0 --prepare|--run" >&2
        exit 2
        ;;
esac
