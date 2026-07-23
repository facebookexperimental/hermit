<!--
Copyright (c) Meta Platforms, Inc. and affiliates.
All rights reserved.

This source code is licensed under the BSD-style license found in the
LICENSE file in the root directory of this source tree.
-->

# LevelDB concurrent-determinism experiment

Shows that a concurrent [LevelDB](https://github.com/google/leveldb) workload is
**nondeterministic natively** but **deterministic under `hermit run`**. LevelDB
is a widely-used C++ storage engine, so it is a good real-world showcase.

## What the workload does

`leveldb_concurrent.cc` opens one LevelDB database and starts `N` worker threads
(default 8). Every thread performs the *same* small, fixed sequence of
`Put`/`Get` operations (default 50 per thread), so the value each thread
computes is identical on every run. When a thread finishes it appends one
summary line to a mutex-guarded vector:

```
thread 003 done ops=50 acc=38745
```

The line *content* is fixed; only the *order* of the lines depends on the order
threads reach the mutex, which is governed by thread scheduling. A final
`total_keys=400` line is scheduling-independent and confirms the store was
actually exercised (it also proves the DB survived intact).

The dataset is intentionally tiny (8 threads x 50 ops -> 400 keys) to keep CI
fast; the effect does not depend on scale. A small `write_buffer_size` (4 KiB)
forces frequent SST flushes so LevelDB's background compaction thread runs too.

## Running

```bash
./run.sh
```

The script is self-contained and idempotent. It:

1. downloads LevelDB (via `with-proxy`; set `NO_PROXY_FETCH=1` for a bare curl),
   verifies its SHA-256, and builds `libleveldb.a` under `.build/` (git-ignored);
2. compiles `leveldb_concurrent.cc`;
3. runs the workload `NRUNS` times **natively** and asserts the outputs DIFFER;
4. runs it `HERMIT_RUNS` times under **`hermit run`** and asserts the outputs are
   IDENTICAL;
5. writes `results.csv` and exits non-zero if either assertion fails.

Knobs (env): `HERMIT`, `NTHREADS`, `OPS`, `NRUNS`, `HERMIT_RUNS`,
`HERMIT_TIMEOUT`, `LEVELDB_VERSION`, `LEVELDB_SHA256`.

`HERMIT` defaults to `../../target/release/hermit`; build it first with
`cargo build --release -p hermit`.

## Result on this host

Both assertions pass (see `results.csv`):

| phase  | runs | distinct outputs | verdict            |
|--------|------|------------------|--------------------|
| native | 6    | 6                | nondeterministic   |
| hermit | 4    | 1                | deterministic      |

Natively the completion order varies every run:

```
thread 002 done ops=50 acc=38695
thread 005 done ops=50 acc=38845
thread 001 done ops=50 acc=38645
...
```

Under `hermit run` it is identical every time (threads always complete in id
order):

```
thread 000 done ops=50 acc=38595
thread 001 done ops=50 acc=38645
thread 002 done ops=50 acc=38695
...
total_keys=400
```

## Hermit finding (worked around here)

Under `hermit run`, LevelDB fails to open a database whose directory does **not
already exist** when that directory is a freshly-created *nested* path
(e.g. `mktemp -d`'s dir + `/db`):

```
failed to open <dir>/db: NotFound: <dir>/db/LOCK: No such file or directory
```

LevelDB's creation of the brand-new subdirectory does not take effect, so the
subsequent `LOCK` file open fails. Opening an **existing empty directory** works
correctly both natively and under hermit. The harness therefore hands LevelDB an
already-created empty `mktemp -d` directory. This looks like a hermit filesystem
(mkdir visibility) quirk worth a follow-up; it is orthogonal to the determinism
result demonstrated here.
