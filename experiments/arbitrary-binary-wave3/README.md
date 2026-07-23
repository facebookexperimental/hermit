# Arbitrary Binary Compatibility: Wave 3

This experiment ran curl, Git, nginx, and Redis Server three times each under
Hermit's strict mode on July 22, 2026. The workloads use only loopback traffic
and temporary files so external network and changing filesystem state do not
affect the comparison.

## Results

| Program | Result | Runs | Repeatable output | Last reported phase |
| --- | --- | ---: | --- | --- |
| curl | Pass | 3/3 | Yes | `curl-wave3-ok` |
| Git | Timeout (exit 137) | 3/3 | Yes | `phase=git-init` |
| nginx | Timeout (exit 137) | 3/3 | Yes | `phase=nginx-start` |
| Redis Server | Timeout (exit 137) | 3/3 | Yes | `phase=redis-ping` |

"Repeatable output" means that the exit code, standard output hash, and
standard error hash were identical across the three runs. It does not turn a
timeout into a compatibility success and does not by itself prove deterministic
scheduling. Curl completed successfully; the other three programs did not
complete within the 20-second per-run limit.

The exact per-run results and hashes are in [results.tsv](results.tsv), with an
aggregate view in [summary.tsv](summary.tsv). [metadata.txt](metadata.txt)
records the Hermit and program binary hashes, package versions, host kernel,
CPU, base commit, and timeout.

## Workloads

- **curl:** Starts a Python HTTP server inside the same Hermit guest, fetches a
  fixed payload over loopback with curl, and checks curl's exit status.
- **Git:** Initializes a repository, configures a local identity, commits a
  fixed file with fixed author and committer timestamps, then prints porcelain
  status for fixed tracked and untracked changes.
- **nginx:** Configures an nginx master and worker with all runtime paths under
  the guest temporary directory, requests a fixed loopback response with curl,
  and requests a graceful shutdown.
- **Redis Server:** Starts a daemon bound to loopback, exercises PING, SET, GET,
  and BGSAVE, waits for the snapshot, and requests a clean shutdown.

The phase markers narrow the observed stalls without claiming a root cause.
Git did not return from `git init`; nginx did not return from its startup
command; Redis Server returned from daemon startup but its first `redis-cli`
PING did not complete. All three behaviors repeated in every run with empty
standard error. Independent strict-mode version probes succeeded for nginx and
Redis Server, while `git --version` also timed out.

## Reproduce

From the repository root on a supported x86_64 Linux host:

```bash
cargo build -p hermit
./experiments/arbitrary-binary-wave3/run_wave3.sh
```

The runner invokes each fixture as:

```text
timeout --signal=KILL 20s target/debug/hermit --log off run --strict -- /bin/sh FIXTURE
```

Set `HERMIT_BIN` to select another Hermit build or `CASE_TIMEOUT_SECONDS` to
change the hard timeout. `ARTIFACT_ROOT`, `RESULTS_FILE`, `SUMMARY_FILE`, and
`METADATA_FILE` can redirect outputs. Timestamped raw stdout and stderr files
are written under the ignored `artifacts/` directory; the checked-in TSV files
contain their SHA-256 hashes.

These results describe the binary builds and host recorded in `metadata.txt`.
Three runs are a compatibility smoke test, not a broad statistical study.
