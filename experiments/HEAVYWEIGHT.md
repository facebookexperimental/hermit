# Heavyweight Manual Experiments

Standard `validate.sh` coverage uses bounded workloads that should finish in
less than 60 seconds per test. The experiments below download or build large
applications, run for more than a minute, or both. They are intentionally not
invoked by any validation profile.

Run these commands from the repository root on x86-64 Linux. Use the same host,
toolchain, environment, and input sizes when comparing branches.

## SQLite `veryquick`

This builds pinned SQLite 3.51.2 and runs its upstream `veryquick` suite twice
until Hermit's known `lock4.test` compatibility boundary:

```sh
cargo build --release -p hermit --bin hermit
HERMIT_BIN=target/release/hermit with-proxy env \
SQLITE_VERYQUICK_TIMEOUT_SECONDS=7200 \
  ./experiments/sqlite-veryquick/run.sh
```

The download, build, raw output, and normalized evidence remain under
`target/sqlite-veryquick/`. See
[`sqlite-veryquick/README.md`](sqlite-veryquick/README.md) for prerequisites,
the pinned archive hash, and the expected result.

## LULESH Large

LULESH is not checked into this repository. The following commands pin LULESH
2.0.3 at commit `46c2a1d6db9171f9637d79f407212e0f176e8194`, build its non-MPI
OpenMP binary, and run a 45-cubed mesh for 100 iterations twice under strict
Hermit. Each run has a one-hour manual-experiment timeout.

```sh
set -euo pipefail
mkdir -p target/lulesh-large
with-proxy git clone --branch 2.0.3 https://github.com/LLNL/LULESH.git \
  target/lulesh-large/source
git -C target/lulesh-large/source checkout \
  46c2a1d6db9171f9637d79f407212e0f176e8194
make -C target/lulesh-large/source clean
make -C target/lulesh-large/source -j4 'CXX=g++ -DUSE_MPI=0'
cargo build --release -p hermit --bin hermit

for run in 1 2; do
  timeout --kill-after=10s 3600s \
    target/release/hermit --log=error run --strict --base-env=minimal \
      --env=LC_ALL=C --env=OMP_NUM_THREADS=4 --env=OMP_DYNAMIC=false \
      --tmp="$PWD/target/lulesh-large/source" -- \
      /tmp/lulesh2.0 -s 45 -i 100 \
    >"target/lulesh-large/run-${run}.stdout" \
    2>"target/lulesh-large/run-${run}.stderr"
done

cmp target/lulesh-large/run-1.stdout target/lulesh-large/run-2.stdout
cmp target/lulesh-large/run-1.stderr target/lulesh-large/run-2.stderr
sha256sum target/lulesh-large/run-*.stdout target/lulesh-large/run-*.stderr
```

This comparison covers complete output and exit success for a manually scaled
workload. It does not replace full-state instrumentation when numerical state,
rather than guest-visible output, is the experiment's subject.

## Other Long Application Suites

The pinned Redis source-build suite runs two extended server workloads and a
memory test:

```sh
cargo build --release -p hermit --bin hermit
HERMIT_BIN=target/release/hermit with-proxy ./experiments/redis-strict/run.sh
```

The LevelDB experiment downloads and builds LevelDB, then compares native and
Hermit scheduling across repeated concurrent workloads:

```sh
cargo build --release -p hermit --bin hermit
HERMIT=target/release/hermit with-proxy ./experiments/leveldb-determinism/run.sh
```

These commands are manual experiments, not standard validation gates. Their
individual READMEs document additional prerequisites and tuning variables.
