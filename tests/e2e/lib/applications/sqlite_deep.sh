#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# Deep on-disk SQLite workload: exercises multi-table joins, transactions,
# secondary indexes, ANALYZE, and aggregation over tens of thousands of rows,
# well beyond the small sqlite_on_disk.sh baseline. The deterministic
# analytical query results are pinned by SHA-256; a run_metadata table seeded
# from strftime('now') and randomblob() is intentionally nondeterministic
# natively so that Hermit's determinization is a real, observable signal.

set -euo pipefail
export LC_ALL=C
export PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
export TZ=UTC

readonly EXPECTED_ROWS_SHA256=d81388e6d1244c450fa88150e68dd67ba5f3bd86c74b99b5319b35857b41bb32

function run_deep_workload {
    local work_dir=$1
    local database rows metadata rows_hash metadata_hash integrity

    database="$work_dir/deep.db"
    rows="$work_dir/rows.txt"
    metadata="$work_dir/metadata.txt"
    rm -rf -- "$work_dir"
    mkdir -p -- "$work_dir"

    # Build the schema and populate deterministically via recursive CTEs (no
    # random data). A rolled-back transaction exercises the write/undo path
    # without altering the committed state. run_metadata is the only
    # natively-nondeterministic content.
    sqlite3 -batch "$database" >/dev/null <<'SQL'
PRAGMA journal_mode=DELETE;
PRAGMA foreign_keys=ON;

CREATE TABLE regions(id INTEGER PRIMARY KEY, name TEXT NOT NULL);
CREATE TABLE users(
  id INTEGER PRIMARY KEY, name TEXT NOT NULL, region_id INTEGER NOT NULL,
  FOREIGN KEY(region_id) REFERENCES regions(id));
CREATE TABLE products(id INTEGER PRIMARY KEY, name TEXT NOT NULL, price INTEGER NOT NULL);
CREATE TABLE orders(
  id INTEGER PRIMARY KEY, user_id INTEGER NOT NULL, amount INTEGER NOT NULL, ts INTEGER NOT NULL,
  FOREIGN KEY(user_id) REFERENCES users(id));
CREATE TABLE order_items(
  order_id INTEGER NOT NULL, product_id INTEGER NOT NULL, qty INTEGER NOT NULL,
  FOREIGN KEY(order_id) REFERENCES orders(id),
  FOREIGN KEY(product_id) REFERENCES products(id));

BEGIN;
INSERT INTO regions(id,name)
  WITH RECURSIVE r(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM r WHERE i<8)
  SELECT i,'region_'||i FROM r;
INSERT INTO users(id,name,region_id)
  WITH RECURSIVE u(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM u WHERE i<2000)
  SELECT i,'user_'||i,(i%8)+1 FROM u;
INSERT INTO products(id,name,price)
  WITH RECURSIVE p(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM p WHERE i<150)
  SELECT i,'product_'||i,((i*7)%1000)+1 FROM p;
INSERT INTO orders(id,user_id,amount,ts)
  WITH RECURSIVE o(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM o WHERE i<8000)
  SELECT i,(i%2000)+1,(i*13)%5000,1600000000+i FROM o;
INSERT INTO order_items(order_id,product_id,qty)
  WITH RECURSIVE oi(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM oi WHERE i<24000)
  SELECT (i%8000)+1,(i%150)+1,(i%9)+1 FROM oi;
COMMIT;

CREATE INDEX idx_orders_user   ON orders(user_id);
CREATE INDEX idx_orders_ts     ON orders(ts);
CREATE INDEX idx_items_order   ON order_items(order_id);
CREATE INDEX idx_items_product ON order_items(product_id);
CREATE INDEX idx_users_region  ON users(region_id);

-- Exercise a write transaction that is rolled back; committed state is unchanged.
BEGIN;
UPDATE orders SET amount=amount+1 WHERE user_id<50;
ROLLBACK;

ANALYZE;

CREATE TABLE run_metadata (observed_at TEXT NOT NULL, nonce TEXT NOT NULL);
INSERT INTO run_metadata
VALUES (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), lower(hex(randomblob(16))));
SQL

    [[ -s $database ]]
    integrity=$(sqlite3 -batch -noheader "$database" 'PRAGMA integrity_check;')
    [[ $integrity == ok ]]

    # Deterministic analytical queries: multi-table joins, aggregation,
    # ordering, and row counts. Their combined output is pinned by SHA-256.
    sqlite3 -batch -noheader -separator '|' "$database" >"$rows" <<'SQL'
SELECT r.name, COUNT(DISTINCT o.id), SUM(o.amount)
  FROM regions r JOIN users u ON u.region_id=r.id JOIN orders o ON o.user_id=u.id
  GROUP BY r.id ORDER BY SUM(o.amount) DESC, r.id ASC;
SELECT p.name, SUM(oi.qty*p.price), COUNT(*)
  FROM order_items oi JOIN products p ON p.id=oi.product_id
  GROUP BY p.id ORDER BY SUM(oi.qty*p.price) DESC, p.id ASC LIMIT 10;
SELECT (o.ts%100), COUNT(*), AVG(o.amount)
  FROM orders o GROUP BY (o.ts%100) ORDER BY (o.ts%100);
SELECT COUNT(*) FROM users;
SELECT COUNT(*) FROM orders;
SELECT COUNT(*) FROM order_items;
SQL

    sqlite3 -batch -noheader -separator '|' "$database" \
        'SELECT observed_at, nonce FROM run_metadata;' >"$metadata"

    rows_hash=$(sha256sum "$rows" | cut -d' ' -f1)
    metadata_hash=$(sha256sum "$metadata" | cut -d' ' -f1)
    [[ $rows_hash == "$EXPECTED_ROWS_SHA256" ]]
    printf 'sqlite-deep:%s:%s\n' "$rows_hash" "$metadata_hash"
}

if [[ ${1:-} == --guest ]]; then
    run_deep_workload "$2"
    exit
fi

# shellcheck source=tests/e2e/lib/applications/common.sh
source "$(dirname -- "$0")/common.sh"
require_commands sqlite3 sha256sum timeout

work_root=$(mktemp -d "${TMPDIR:-/tmp}/hermit-sqlite-deep-e2e.XXXXXX")
trap 'rm -rf -- "$work_root"' EXIT

native_first=$(run_deep_workload "$work_root/native")
native_second=$(run_deep_workload "$work_root/native")
assert_native_nondeterminism 'SQLite deep workload' "$native_first" "$native_second"

run_hermit_verify 'SQLite deep workload' \
    /bin/bash "$0" --guest "$work_root/verified" >/dev/null
printf 'sqlite-deep:verified:%s\n' "$EXPECTED_ROWS_SHA256"
