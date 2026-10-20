#!/usr/bin/env python3
"""Rebuild the e2e result files of a Buck test run from its Tpx results.

usage: ingest.py --plan ci/expected-e2e-plan.json --out IMPORT_DIR [--work DIR]
                 [--local-artifacts BUCK_OUT_TEST_DIR] [--failed-verify-logs DIR]
                 [--withheld-on-re FILE] TEST_RUN_ID...

TEST_RUN_ID is what `buck2 test --write-test-id FILE` wrote (one per invocation; the
hybrid run makes two, one for RE cells and one for local cells). Every cell execution
uploads the rows test-harness wrote as its results.jsonl Tpx artifact; this fetches
them with testx (TESTX env, default `testx`) and writes one bucket per (lane, category):

    IMPORT_DIR/<lane>/manifest_<category>/results.jsonl

    IMPORT_DIR/<lane>/manifest_<category>/summary.json

IMPORT_DIR is input for `test-harness run` in import mode (E2E_IMPORT_RESULTS=IMPORT_DIR)
and for nothing else. Import mode checks every cell's history, staleness and evidence,
then publishes the result files the cargo flow's e2e nodes write. Read directly, these
files skip all of those checks, and summary.json's counts leave out every cell whose
final execution wrote no summary.json.
summary.json aggregates each cell's final execution: counts are summed and cell lists
joined, which is what one harness process over the bucket writes. A cell's final
execution is its last one that did not withhold the cell on an RE worker (below), when it
has one: such an execution is not an attempt, so its claim is not the cell's outcome.
summary.json's evidence_complete_executions lists each execution whose result.json
(written by cell.sh) records complete evidence, by its run id; import mode refuses a
PASS whose own execution is not listed. An execution's rows, result.json and run_id.
marker (see --local-artifacts) must name one run id, and no two executions may share one.
Tpx owns retries (cells run the harness with --no-retry). A cell's Nth execution in Tpx
order is attempt N, and its row (at most one, for its own cell) carries that number. An
execution that wrote no row still takes its number, so the next row shows the gap and
import mode refuses the history. "Passed only on rerun" therefore stays visible, as
attempt 1 failing before attempt 2 passes, even when attempt 1 died before writing a row.
The one exception is an execution that withheld its cell on an RE worker: it wrote no row,
its summary.json claims the cell host-inapplicable, and its result.json records route "re".
That harness started no hermit, because the worker it landed on cannot run the cell (a
worker without an exact branch counter, which the plan cannot route around: RE workers
differ in CPU model). It is not an attempt and takes no number; the cell's next execution,
on another worker, is attempt 1. Any other claim followed by a later execution still takes
its number.
--event-log FILE (repeatable) and --buck2 PATH: the invocations' Buck event logs and the
buck2 that reads them. When an execution wrote no row, ingest reads its own record from
them (`buck2 log show`): Tpx's status, details and duration for the target (every
TestResult within 15 s of the execution's TestRun end), and the command's status, exit
code and wall time (that TestRun span), matched by Buck target and nearest end time. Each
such execution gets one line, {schema, test, mode, backend, attempt, cause}, in
IMPORT_DIR/<lane>/manifest_<category>/no-row-executions.jsonl, which import mode quotes
when it refuses the cell's history. It is diagnostics only: the refusal is the same with
or without it, and a missing or unreadable event log only makes the cause say so.
--withheld-on-re FILE: write the Buck target name (the harness's slug) of each cell that
has no row and was withheld on RE by every one of its executions, one per line. Import
mode would judge such a cell's claim against the machine that imports it, which did not
make it, so ci/buck-e2e/run runs these cells again locally and ingests once more.
--withheld-retries FILE: likewise, each cell whose last execution withheld it on RE after
an earlier execution ran it: the retry Tpx gave a failed attempt never ran (on 2026-10-19
c-programs/sigreturn-preemption failed attempt 1 with a skid overshoot on RE and its retry
landed on a worker without an exact branch counter). ci/buck-e2e/run runs these once more
locally without a Tpx retry, so that execution is the cell's next attempt.
A cell with no rows is host-inapplicable only if it ran and every one of its executions
says so in its own summary.json. Any other plan cell with no rows is an error, and so is
a cell outside the plan. Executions of one cell that end in the same second are refused:
their order is unknown, and testx addresses an artifact as RUN.TEST.END. Prints one
JSON summary line.

Import mode's parity post-pass compares each verify cell's first-run log (the one hermit
keeps as the golden copy) with ptrace's. A row records the --verify-log-dir its cell ran
with, in scratch space that is gone; cell.sh returned that directory's files as
artifacts (cell__<path below the row's artifact_dir, / as __>__<file>, .zst when
compressed). Every run1_log_ file directly in it is checked byte-exact against the
sha256 its result.json recorded, decompressed, and written to

    IMPORT_DIR/retained-verify-logs/<run id>/<path below the row's artifact_dir>/

IMPORT_DIR/retained-verify-logs/index.jsonl maps each recorded directory to the
directory below IMPORT_DIR its logs were restored to, or to why none were (no such log
among the artifacts, a failed fetch, no recorded or a different sha256). A directory is
restored whole or not at all. A log that is not restored leaves only its parity cell
unmeasured: it never fails the ingest. Each entry also records where the execution that
recorded the directory ran, as its result.json recorded it: route (local or re),
container (pinned-root or empty) and re_platform, each null when it recorded none or
several executions recorded the directory. The post-pass gives a pair clean credit only
when both cells ran on the same route.

--failed-verify-logs DIR: keep the verify logs of every execution that did not pass, as
evidence that outlives IMPORT_DIR (validate passes a directory below its E2E result
root). An execution did not pass when its row is not PASS, when it wrote no row, or when
its result.json does not record complete evidence (cell.sh then reports it to Tpx as an
ERROR, whatever its row says). Each
of hermit's verify logs (run1_log_*, and run2_log_*, which hermit keeps only when the
two runs differ) that cell.sh returned from anywhere in the cell's directory is checked
byte-exact against the sha256 its result.json recorded, decompressed, and written to

    DIR/<run id>/<path below the cell's directory>/<log>

An execution that passed gets nothing here. A log is bounded the way the cargo lane's
retained logs are: hermit already stops a log at HERMIT_LOG_MAX_BYTES (1 GiB by default)
and appends its truncation marker, and this keeps at most 1 GiB + 1 MiB of a log, so a
log hermit bounded is kept whole and anything longer is cut there. DIR/index.jsonl has
one line per execution that did not pass: its cell, run id, outcome (null without a
row), route keys as above, and for each log the file written (relative to DIR) with its
size and whether it was cut, or why it was not kept. Each entry also keeps what Hermit
said: the last 64 KiB of the stderr the harness recorded for the execution's last attempt,
written to DIR/<run id>/hermit-stderr.tail (its "stderr" record gives the file, the full
size and whether it was cut, or why none was kept), and "internal_failure_class", the class
of its last HERMIT_INTERNAL_FAILURE line (null without one). Both are kept whether or not
any log was: a refusal before the guest ran, Hermit's exit 125, leaves none
(https://github.com/rrnewton/hermit/issues/3922). DIR must be absent, empty, or a
previous ingest's (it holds index.jsonl), and must neither contain nor lie inside IMPORT_DIR,
--work or --local-artifacts; it is replaced. The logs are kept before the cells' coverage is
checked, so a run that ingest refuses for a cell with no row (every attempt died) still
leaves them. A log that is not kept never fails the ingest.

--local-artifacts: Buck materializes each test's artifact directory locally
(buck-out/v2/test/execution/<cell>/<target hash>/<config hash>/default/artifacts_directory),
keeping the newest execution per target. cell.sh writes an artifact named run_id.<run id>,
which the testx listing shows without a fetch. An execution is read from the one local
directory holding its marker; every other execution, including one whose marker is in
no local directory or in several, is fetched with testx.
"""
import argparse, collections, concurrent.futures as cf, hashlib, json, os, re, shutil, stat, subprocess, sys, tempfile, time

TESTX = os.environ.get("TESTX", "testx")
MARKER = "run_id."  # cell.sh: an artifact named run_id.<run id>
VERIFY_LOG_DIR = "--verify-log-dir"  # the harness's flag naming where hermit --verify keeps its logs
RUN1_LOG = "run1_log_"  # hermit's first-run log; parity.rs RETAINED_LOG_PREFIX
LOGS_DIR = "retained-verify-logs"  # parity.rs IMPORTED_LOGS_DIR
LOGS_INDEX = "index.jsonl"  # parity.rs IMPORTED_LOGS_INDEX
LOGS_SCHEMA = 2  # parity.rs IMPORTED_LOGS_SCHEMA
ROUTE_KEYS = ("route", "container", "re_platform")  # cell.sh's result.json: where the execution ran
SAFE = re.compile(r"[A-Za-z0-9][A-Za-z0-9._-]*")  # a path component, never . or ..
RUN_LOGS = (RUN1_LOG, "run2_log_")  # hermit's two verify logs; it keeps run 2's only when the runs differ
CELL_ARTIFACT = "cell__"  # cell.sh: <cell dir>/A/B is the artifact cell__A__B
# hermit-cli tracing.rs DEFAULT_LOG_MAX_BYTES (1 GiB), the bound on every log hermit writes,
# plus room for the truncation marker it appends there.
FAILED_LOG_MAX_BYTES = (1 << 30) + (1 << 20)
FAILED_LOGS_INDEX = "index.jsonl"
FAILED_LOGS_SCHEMA = 2  # 2: each entry also carries the stderr tail and the internal failure class
# The tail of a failed execution's Hermit stderr that the index keeps. Measured 2026-10-10 over
# the retained cargo-lane results files on the validation host, counted twice over slightly
# different file sets (2,985 failed attempts in 6,499 files; 2,880 in the 6,372 at
# e2e/*/*/results.jsonl): their stderr was 629 bytes at the median, 9,477 or 998 at p90, 60,766 or
# 64,503 at p99 and 95,118 at most, with 23 or 27 attempts over 64 KiB; the text from Hermit's
# HERMIT_INTERNAL_FAILURE line to the end was at most 1,041 bytes. So 64 KiB keeps all of it for
# more than 99% of them, and that line and the message after it always.
STDERR_TAIL = "hermit-stderr.tail"
STDERR_TAIL_MAX_BYTES = 64 << 10
INTERNAL_FAILURE = "HERMIT_INTERNAL_FAILURE class="  # runner.rs HERMIT_INTERNAL_FAILURE_CLASS_PREFIX

def try_testx(*args, tries=4):
    """(testx's stdout, None), or (None, why it failed)."""
    for i in range(tries):
        p = subprocess.run([TESTX, *args], capture_output=True, text=True)
        if p.returncode == 0:
            return p.stdout, None
        time.sleep(5 * (i + 1))  # TestX answers 500 transiently
    return None, f"testx {' '.join(args[:3])} failed: {p.stderr[-500:]}"

def testx(*args, tries=4):
    stdout, error = try_testx(*args, tries=tries)
    if error is not None:
        sys.exit(f"ingest: {error}")
    return stdout

def rows_of(path):
    if path.endswith(".zst"):
        text = subprocess.run(["zstd", "-q", "-d", "-c", path], check=True, capture_output=True, text=True).stdout
    else:
        text = open(path).read()
    return [json.loads(l) for l in text.splitlines() if l.strip()]

def markers(names):
    return [n[len(MARKER):] for n in names if n.startswith(MARKER)]

def cell_of(entry):
    return "{}/{}@{}".format(entry.get("test"), entry.get("mode"), entry.get("backend") or "native")

def cell_slug(cell):
    """A plan cell's Buck target name (defs.bzl cell_slug)."""
    return "{}-{}-{}".format(cell["test"].replace("/", "-"), cell["mode"], cell["backend"])

# buck2_data.TestStatus (buck2/app/buck2_data/data.proto).
TPX_STATUS = {0: "NOT_SET", 1: "PASS", 2: "FAIL", 3: "SKIP", 4: "OMITTED", 5: "FATAL", 6: "TIMEOUT",
              7: "UNKNOWN", 8: "RERUN", 9: "LISTING_SUCCESS", 10: "LISTING_FAILED", 11: "INFRA_FAILURE"}
NO_ROW_FILE = "no-row-executions.jsonl"
NO_ROW_SCHEMA = "hermit-buck-e2e-no-row-execution/v1"
NO_ROW_MATCH_SECONDS = 15

def event_log_tests(buck2, paths):
    """(runs, results, errors) from the invocations' event logs: runs maps a Buck target
    name to [(end time, command record)] for each TestRun span end (the command's own
    status, exit code and wall time), results maps it to [(time, Tpx record)] for each
    TestResult (Tpx's status, details and duration). errors says why a log was not read."""
    runs, results, errors = collections.defaultdict(list), collections.defaultdict(list), []
    for path in paths:
        p = subprocess.run([buck2, "log", "show", path], capture_output=True, text=True)
        if p.returncode != 0:
            errors.append(f"`buck2 log show {path}` exited {p.returncode}: {p.stderr.strip()[-300:]}")
            continue
        for line in p.stdout.splitlines():
            if '"TestRun"' not in line and '"TestResult"' not in line:
                continue
            try:
                event = json.loads(line)["Event"]
                at = event["timestamp"][0] + event["timestamp"][1] / 1e9
            except (ValueError, KeyError, TypeError, IndexError):
                continue
            data = event.get("data") or {}
            run = ((data.get("SpanEnd") or {}).get("data") or {}).get("TestRun")
            if run:
                name = run["suite"]["target_label"]["label"]["name"]
                report = run.get("command_report") or {}
                details = report.get("details") or {}
                wall = (details.get("metadata") or {}).get("wall_time_us")
                runs[name].append((at, {"command_status": next(iter(report.get("status") or {}), None),
                                        "exit_code": details.get("signed_exit_code", details.get("exit_code")),
                                        "wall_s": None if wall is None else int(wall) / 1e6}))
            result = ((data.get("Instant") or {}).get("data") or {}).get("TestResult")
            if result:
                target, _, case = str(result.get("name", "")).partition(" - ")
                duration = result.get("duration_us")
                results[target.rsplit(":", 1)[-1]].append((at, {
                    "case": case, "status": TPX_STATUS.get(result.get("status"), result.get("status")),
                    "details": result.get("details"), "duration_s": None if duration is None else int(duration) / 1e6}))
    return runs, results, errors

def no_row_cause(slug, end, events):
    """Why the execution of Buck target slug that testx says ended at end wrote no row, in
    Tpx's and Buck's own words, or why that could not be read."""
    if events is None:
        return "ingest was given no Buck event log to read this execution's record from"
    runs, results, errors = events
    near = [(abs(at - end), at, record) for at, record in runs.get(slug, []) if abs(at - end) <= NO_ROW_MATCH_SECONDS]
    if not near:
        why = f"; event logs not read: {'; '.join(errors)}" if errors else ""
        return f"no TestRun of {slug} ended within {NO_ROW_MATCH_SECONDS} s of {end} in the event logs{why}"
    _, at, command = min(near, key=lambda x: x[0])
    tpx = [record for t, record in results.get(slug, []) if abs(t - at) <= NO_ROW_MATCH_SECONDS]
    def seconds(value):
        return "?" if value is None else f"{value:.1f} s"
    said = "; ".join(f"Tpx {r['status']} ({r['case'] or 'no case'}), details {r['details']!r}, after {seconds(r['duration_s'])}"
                     for r in tpx) or "no Tpx result for it"
    return (f"{said}; the command's status {command['command_status']}, exit code {command['exit_code']}, "
            f"after {seconds(command['wall_s'])} of wall time")

def claims_host_inapplicable(cell, summary):
    return any(cell_of(h) == cell for h in (summary or {}).get("host_inapplicable_cells", []))

def run1_log_name(name, prefix):
    """The name hermit gave the first-run log that cell.sh stored as artifact NAME,
    when NAME is PREFIX + that name (+ .zst for a log cell.sh compressed), else None."""
    if not name.startswith(prefix):
        return None
    base = name[len(prefix):]
    base = base[:-len(".zst")] if base.endswith(".zst") else base
    # A name with __ in it was in a subdirectory, which hermit never writes logs to.
    return base if base.startswith(RUN1_LOG) and "__" not in base and SAFE.fullmatch(base) else None

def verify_logs(rows, names):
    """{recorded verify-log directory: (its path below the row's artifact_dir, prefix,
    [artifact name of each first-run log in it])} for every row run with --verify-log-dir,
    or a reason in place of the tuple when its logs cannot be among the artifacts NAMES.
    cell.sh stores the file <artifact_dir>/A/B as the artifact cell__A__B."""
    found = {}
    for row in rows:
        argv = row.get("argv") or []
        flag = argv.index(VERIFY_LOG_DIR) if VERIFY_LOG_DIR in argv else None
        if flag is None or flag + 1 >= len(argv):
            continue
        recorded, adir = argv[flag + 1], row.get("artifact_dir") or ""
        if not adir or not recorded.startswith(adir.rstrip("/") + "/"):
            found[recorded] = f"it is not below the row's artifact directory {adir!r}, the only one cell.sh returns"
            continue
        parts = recorded[len(adir.rstrip("/")) + 1:].split("/")
        if not all(SAFE.fullmatch(part) and "__" not in part for part in parts):
            found[recorded] = f"its path below the artifact directory, {'/'.join(parts)!r}, has no cell.sh artifact name"
            continue
        prefix = "cell__" + "__".join(parts) + "__"
        found[recorded] = ("/".join(parts), prefix, sorted(n for n in names if run1_log_name(n, prefix)))
    return found

def run_logs(names):
    """{artifact name: (directory below the cell's directory, log name)} for each of
    hermit's verify logs among the artifacts NAMES, from any directory: cell.sh stores
    <cell dir>/A/B/run2_log_x as cell__A__B__run2_log_x (+ .zst when compressed)."""
    found = {}
    for name in names:
        base = name[:-len(".zst")] if name.endswith(".zst") else name
        if not base.startswith(CELL_ARTIFACT):
            continue
        parts = base[len(CELL_ARTIFACT):].split("__")
        if parts[-1].startswith(RUN_LOGS) and all(SAFE.fullmatch(part) for part in parts):
            found[name] = ("/".join(parts[:-1]), parts[-1])
    return found

def copy_log(path, sha256, target, cap):
    """Write the artifact at PATH to TARGET, decompressed when it is a .zst, after
    checking its bytes against SHA256, keeping at most CAP bytes. Returns (bytes written,
    whether the log was cut at CAP); raises ValueError(why) when it cannot be kept."""
    digest = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            digest.update(chunk)
    if sha256 != digest.hexdigest():
        raise ValueError("it does not have the sha256 its cell recorded")
    zstd = subprocess.Popen(["zstd", "-q", "-d", "-c", path], stdout=subprocess.PIPE, stderr=subprocess.PIPE) \
        if path.endswith(".zst") else None
    written, cut = 0, False
    with (zstd.stdout if zstd else open(path, "rb")) as source, open(target, "wb") as out:
        for chunk in iter(lambda: source.read(1 << 20), b""):
            out.write(chunk[:cap - written])
            if written + len(chunk) > cap:
                written, cut = cap, True
                break
            written += len(chunk)
    if zstd:
        if cut:
            zstd.kill()
        stderr = zstd.stderr.read()
        if zstd.wait() != 0 and not cut:
            raise ValueError(f"it cannot be decompressed: {stderr[-300:].decode(errors='replace')}")
    return written, cut

def last_attempt_stderr(rows):
    """The stderr the harness recorded for the last attempt of the execution's last row, or
    (None, why there is none)."""
    if not rows:
        return None, "the execution wrote no row"
    attempts = rows[-1].get("attempts")
    if not isinstance(attempts, list) or not attempts or not isinstance(attempts[-1], dict):
        return None, "the execution's row records no attempt"
    stderr = attempts[-1].get("stderr")
    if not isinstance(stderr, str):
        return None, "the execution's last attempt records no stderr"
    return stderr, None

def internal_failure_class(stderr):
    """The class of the last HERMIT_INTERNAL_FAILURE line in STDERR, or None."""
    for line in reversed(stderr.splitlines()):
        if line.startswith(INTERNAL_FAILURE):
            words = line[len(INTERNAL_FAILURE):].split()
            return words[0] if words else None
    return None

def keep_stderr(root, run_id, rows):
    """Write the last STDERR_TAIL_MAX_BYTES of the execution's Hermit stderr to
    ROOT/RUN_ID/STDERR_TAIL, and return (its FAILED_LOGS_INDEX "stderr" record, the
    internal failure class its stderr names)."""
    record = {"file": None, "bytes": None, "truncated": None, "reason": None}
    stderr, why = last_attempt_stderr(rows)
    if stderr is None:
        record["reason"] = why
        return record, None
    blob = stderr.encode(errors="replace")
    kept = os.path.join(run_id, STDERR_TAIL)
    try:
        os.makedirs(os.path.join(root, run_id), exist_ok=True)
        with open(os.path.join(root, kept), "wb") as out:
            out.write(blob[-STDERR_TAIL_MAX_BYTES:])
        record.update(file=kept, bytes=len(blob), truncated=len(blob) > STDERR_TAIL_MAX_BYTES)
    except OSError as error:
        record["reason"] = str(error)
    return record, internal_failure_class(stderr)

def keep_failed_logs(root, cell, run_id, rows, result, logs, log_dir, fetch_error, cap):
    """Copy the verify logs LOGS (see run_logs) of an execution that did not pass out of
    LOG_DIR into ROOT/RUN_ID/<directory below the cell's directory>/ (see copy_log), keep
    the tail of its Hermit stderr (see keep_stderr), and return its FAILED_LOGS_INDEX
    entry."""
    entry = {"schema": FAILED_LOGS_SCHEMA, "cell": cell, "run_id": run_id,
             "outcome": rows[-1].get("outcome") if rows else None, **route_of(result), "logs": [], "reason": None,
             "stderr": {"file": None, "bytes": None, "truncated": None, "reason": None},
             "internal_failure_class": None}
    hashes = (result or {}).get("artifact_sha256")
    # The stderr is kept whatever happens to the logs: a refusal before the guest ran, the
    # case it is for, often leaves no log at all.
    if isinstance(run_id, str) and SAFE.fullmatch(run_id):
        entry["stderr"], entry["internal_failure_class"] = keep_stderr(root, run_id, rows)
    else:
        entry["stderr"]["reason"] = f"the execution's run id {run_id!r} cannot name a directory"
    if not logs:
        entry["reason"] = "the execution's artifacts hold no run1_log_* or run2_log_* log"
    elif fetch_error is not None:
        entry["reason"] = f"its logs could not be fetched: {fetch_error}"
    elif not isinstance(run_id, str) or not SAFE.fullmatch(run_id):
        entry["reason"] = f"the execution's run id {run_id!r} cannot name a directory"
    elif not isinstance(hashes, dict):
        entry["reason"] = "the execution's result.json records no artifact_sha256, so its logs cannot be checked byte-exact"
    if entry["reason"] is not None:
        return entry
    written = set()
    for name, (rel, log) in sorted(logs.items()):
        kept = os.path.join(run_id, *rel.split("/"), log) if rel else os.path.join(run_id, log)
        one = {"artifact": name, "log": None, "bytes": None, "truncated": None, "reason": None}
        target = os.path.join(root, kept)
        try:
            if kept in written:
                raise ValueError(f"another artifact was already kept as {kept}")
            os.makedirs(os.path.dirname(target), exist_ok=True)
            size, cut = copy_log(os.path.join(log_dir, name), hashes.get(name), target, cap)
            one.update(log=kept, bytes=size, truncated=cut)
            written.add(kept)
        except (OSError, ValueError) as error:
            if kept not in written and os.path.exists(target):
                os.unlink(target)
            one["reason"] = str(error)
        entry["logs"].append(one)
    return entry

def route_of(result):
    """{key: the string the execution's RESULT records for it, else None} for each of
    ROUTE_KEYS."""
    result = result if isinstance(result, dict) else {}
    return {k: result[k] if isinstance(result.get(k), str) else None for k in ROUTE_KEYS}

def restore(out, run_id, result, logs, log_dir, fetch_error):
    """Copy each first-run log LOGS (see verify_logs) names out of LOG_DIR into
    OUT/LOGS_DIR/RUN_ID/<its path below the artifact_dir>, decompressed, after checking
    the bytes against the sha256 cell.sh recorded in the execution's RESULT. Returns
    {recorded verify-log directory: (restored directory relative to OUT, None) or
    (None, why none)}. A directory is restored whole or not at all."""
    hashes = (result or {}).get("artifact_sha256")
    entries = {}
    for recorded, found in sorted(logs.items()):
        if isinstance(found, str):
            entries[recorded] = (None, found)
            continue
        rel, prefix, names = found
        if not names:
            entries[recorded] = (None, f"the execution's artifacts hold no {RUN1_LOG}* log from it")
            continue
        if fetch_error is not None:
            entries[recorded] = (None, f"its logs could not be fetched: {fetch_error}")
            continue
        if not isinstance(run_id, str) or not SAFE.fullmatch(run_id):
            entries[recorded] = (None, f"the execution's run id {run_id!r} cannot name a directory")
            continue
        if not isinstance(hashes, dict):
            entries[recorded] = (None, "the execution's result.json records no artifact_sha256, "
                                       "so its logs cannot be checked byte-exact")
            continue
        restored, reason = {}, None
        for name in names:
            try:
                with open(os.path.join(log_dir, name), "rb") as f:
                    blob = f.read()
            except OSError as error:
                reason = f"{name} cannot be read: {error}"
                break
            if hashes.get(name) != hashlib.sha256(blob).hexdigest():
                reason = f"{name} does not have the sha256 its cell recorded"
                break
            if name.endswith(".zst"):
                try:
                    p = subprocess.run(["zstd", "-q", "-d", "-c"], input=blob, capture_output=True)
                    failure = p.stderr[-300:].decode(errors="replace") if p.returncode != 0 else None
                except OSError as error:
                    failure = str(error)
                if failure is not None:
                    reason = f"{name} cannot be decompressed: {failure}"
                    break
                blob = p.stdout
            log = run1_log_name(name, prefix)
            if log in restored:
                reason = f"two artifacts restore to {log}"
                break
            restored[log] = blob
        if reason is None:
            target = os.path.join(LOGS_DIR, run_id, *rel.split("/"))
            try:
                os.makedirs(os.path.join(out, target))
                for log, blob in sorted(restored.items()):
                    with open(os.path.join(out, target, log), "wb") as f:
                        f.write(blob)
            except OSError as error:
                reason = f"its logs cannot be written below {out}: {error}"
        entries[recorded] = (target, None) if reason is None else (None, reason)
    return entries

def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--plan", required=True)
    ap.add_argument("--out", required=True, help="IMPORT_DIR: input for test-harness run's import mode only")
    ap.add_argument("--work", help="where fetched artifacts go (default: a temporary directory)")
    ap.add_argument("-j", type=int, default=32)
    ap.add_argument("--local-artifacts", help="buck-out/v2/test/execution: read an execution from here when one directory holds its marker")
    ap.add_argument("--failed-verify-logs", metavar="DIR", help="keep the verify logs of every execution that did not pass here")
    ap.add_argument("--failed-log-max-bytes", type=int, default=FAILED_LOG_MAX_BYTES, help=argparse.SUPPRESS)  # tests only
    ap.add_argument("--withheld-on-re", help="write the target name of each cell withheld on RE by every execution here")
    ap.add_argument("--withheld-retries", help="write the target name of each cell whose retry after an attempt was withheld on RE here")
    ap.add_argument("--event-log", action="append", default=[], help="an invocation's Buck event log, read to name why an execution wrote no row")
    ap.add_argument("--buck2", default=os.environ.get("BUCK2", "buck2"), help="the buck2 that reads --event-log")
    ap.add_argument("run_ids", nargs="+")
    a = ap.parse_args()
    failed_root, failed_logs_error = a.failed_verify_logs, None
    if failed_root is not None and os.path.lexists(failed_root):
        def holds_index():
            # lstat, not isfile: isfile reads a permission error as "no index", which would
            # refuse a previous ingest's directory as someone else's.
            try:
                return stat.S_ISREG(os.lstat(os.path.join(failed_root, FAILED_LOGS_INDEX)).st_mode)
            except FileNotFoundError:
                return False
        try:
            foreign = (not os.path.isdir(failed_root) or os.path.islink(failed_root) or
                       (bool(os.listdir(failed_root)) and not holds_index()))
        except OSError as error:
            # A directory that cannot be inspected cannot be shown to be ours, so it is left
            # alone and no logs are kept; like any retention failure this never fails the ingest.
            failed_logs_error = f"cannot inspect {failed_root}: {error}"
            print(f"ingest: the failed cells' verify logs will not be kept: {failed_logs_error}", file=sys.stderr)
            failed_root, foreign = None, False
        if foreign:
            sys.exit(f"ingest: --failed-verify-logs {failed_root} is not absent, empty, or a previous ingest's "
                     f"(no {FAILED_LOGS_INDEX}); refusing to replace it")
    plan = json.load(open(a.plan))
    want = {"{}/{}@{}".format(c["test"], c["mode"], c["backend"]): c for c in plan["cells"]}
    work = a.work or tempfile.mkdtemp(prefix="buck-e2e-ingest-")
    if failed_root is not None:
        # Against the work directory in effect, including a default one below TMPDIR.
        def nested(a_path, b_path):
            a_path, b_path = os.path.realpath(a_path), os.path.realpath(b_path)
            return os.path.commonpath([a_path, b_path]) in (a_path, b_path)
        for flag, other in (("--out", a.out), ("--work", work), ("--local-artifacts", a.local_artifacts)):
            if other is not None and nested(failed_root, other):
                sys.exit(f"ingest: --failed-verify-logs {failed_root} overlaps {flag} {other}; replacing it would "
                         f"delete that directory's files")
    os.makedirs(work, exist_ok=True)  # testx needs the parent of --output-dir to exist
    executions = []  # (cell, end, test id, test run id, marker run id, result)
    def listed(rid):
        # TestX's listing, not the local work below, is most of the ingest: the local part of a
        # 2,314-cell hybrid run takes about 5 s, and its ingest took 76 s. The invocations'
        # listings are independent queries, so they overlap; the order of the rows below is
        # still the order of the run ids, and the time each took is printed.
        started = time.monotonic()
        results = json.loads(testx("--as-json", "results", "list", rid))["results"]["test_results"]
        print(f"ingest: testx listed {len(results)} results of test run {rid} in "
              f"{time.monotonic() - started:.1f} s", file=sys.stderr)
        return results
    with cf.ThreadPoolExecutor(len(a.run_ids)) as ex:
        listings = list(ex.map(listed, a.run_ids))
    for rid, results in zip(a.run_ids, listings):
        for t in results:
            name = t["test_details"]["name"]
            # Each target also reports an "unmanaged" twin of the same execution.
            if " - " in name and not name.endswith(" - unmanaged"):
                cell, end, tid = name.split(" - ", 1)[1], int(t["end_time"]), t["test_details"]["id"]
                found = markers(x["name"] for x in t.get("artifacts") or [])
                if len(found) > 1:
                    sys.exit(f"ingest: the execution of {cell} that ended at {end} has {len(found)} run id markers: {found[:5]}")
                executions.append((cell, end, tid, rid, found[0] if found else None, t))
    ends = collections.Counter((cell, end) for cell, end, *_ in executions)
    ties = sorted(f"{cell} at {end}" for (cell, end), n in ends.items() if n > 1)
    if ties:
        sys.exit(f"ingest: executions of one cell ended in the same second, so their order is unknown "
                 f"and testx (RUN.TEST.END) cannot tell their artifacts apart: {ties[:20]} ({len(ties)})")

    local = collections.defaultdict(list)  # run id -> local artifact directories holding its marker
    if a.local_artifacts:
        for dirpath, dirnames, filenames in os.walk(a.local_artifacts):
            if os.path.basename(dirpath) == "artifacts_directory":
                dirnames[:] = []
                found = markers(filenames)
                if len(found) == 1:
                    local[found[0]].append(dirpath)

    def fetch(item):
        cell, end, tid, rid, marker, t = item
        if marker is not None and len(local.get(marker, [])) == 1:
            source, d = "local", local[marker][0]
            names = os.listdir(d)
            rows_name = next((n for n in ("results.jsonl", "results.jsonl.zst") if n in names), None)
            wanted = [n for n in (rows_name, "summary.json", "result.json") if n in names]
        else:
            source = "testx"
            names = [x["name"] for x in t.get("artifacts") or []]
            rows_name = next((n for n in ("results.jsonl", "results.jsonl.zst") if n in names), None)
            wanted = [n for n in (rows_name, "summary.json", "result.json") if n in names]
            d = os.path.join(work, re.sub(r"[^A-Za-z0-9._-]+", "_", f"{cell}__{tid}__{end}"))
            if wanted and not all(os.path.exists(os.path.join(d, n)) for n in wanted):
                args = ["artifacts", "get", f"{rid}.{tid}.{end}", "--output-dir", d]
                for n in wanted:
                    args += ["--artifact-names", n]
                testx(*args)
        rows = rows_of(os.path.join(d, rows_name)) if rows_name else []
        summary = json.load(open(os.path.join(d, "summary.json"))) if "summary.json" in wanted else None
        result = json.load(open(os.path.join(d, "result.json"))) if "result.json" in wanted else None
        # The logs only feed the parity post-pass and the failure evidence, so failing to
        # fetch them never fails the ingest.
        logs, log_dir, fetch_error = verify_logs(rows, names), d, None
        log_names = sorted({n for found in logs.values() if not isinstance(found, str) for n in found[2]})
        # As cell.sh reports it to Tpx: a PASS row whose evidence is incomplete is an ERROR.
        passed = (bool(rows) and all(row.get("outcome") == "PASS" for row in rows)
                  and isinstance(result, dict) and result.get("evidence_complete") is True)
        failed = run_logs(names) if failed_root is not None and not passed else None
        def get(directory, wanted):
            if all(os.path.exists(os.path.join(directory, n)) for n in wanted):
                return None
            args = ["artifacts", "get", f"{rid}.{tid}.{end}", "--output-dir", directory]
            for n in wanted:
                args += ["--artifact-names", n]
            return try_testx(*args)[1]
        if source == "testx" and log_names:
            log_dir = d + "__verify-logs"
            fetch_error = get(log_dir, log_names)
        # Into a directory of its own, so this fetch can never overwrite or fail the parity post-pass's logs.
        failed_dir, failed_error = d, None
        if source == "testx" and failed:
            failed_dir = d + "__failed-verify-logs"
            failed_error = get(failed_dir, sorted(failed))
        return (cell, end, marker, source, rows, summary, result, (logs, log_dir, fetch_error),
                None if failed is None else (failed, failed_dir, failed_error))

    with cf.ThreadPoolExecutor(a.j) as ex:
        fetched = list(ex.map(fetch, executions))
    sources = collections.Counter(source for _, _, _, source, *_ in fetched)
    per_cell = collections.defaultdict(list)  # cell -> [(rows, summary)] in Tpx order
    per_cell_ends = collections.defaultdict(list)  # cell -> [testx end time] in the same order
    complete_runs = collections.defaultdict(list)  # cell -> run ids of evidence-complete executions
    run_cells = {}
    log_sources = []  # (run id, result, (logs, log_dir, fetch_error)) of each execution
    failed_sources = []  # (cell, run id, rows, result, failed logs, log_dir, fetch_error) of each that did not pass
    for cell, end, marker, source, rows, summary, result, logs, failed in sorted(fetched, key=lambda x: (x[0], x[1])):
        where = f"the execution of {cell} that ended at {end}"
        if result is not None and result.get("cell") != cell:
            sys.exit(f"ingest: {where} has a result.json for {result.get('cell')}")
        if len(rows) > 1 or any(cell_of(row) != cell for row in rows):
            sys.exit(f"ingest: {where} has {len(rows)} rows, for cells {sorted({cell_of(row) for row in rows})}; "
                     f"a cell runs the harness once (--no-retry), which writes one row, for that cell")
        ids = [row.get("run_id") for row in rows] + ([result.get("run_id")] if result is not None else [])
        ids += [marker] if marker is not None else []
        if len(set(ids)) > 1 or None in ids:
            sys.exit(f"ingest: {where} has run ids {sorted(set(map(str, ids)))}; "
                     f"its rows, its result.json and its {MARKER} marker must all name the same one")
        run_id = ids[0] if ids else None
        if run_id is not None:
            if run_id in run_cells:
                sys.exit(f"ingest: run {run_id} names two executions: {run_cells[run_id]} and {where}")
            run_cells[run_id] = where
        if result is not None and result.get("evidence_complete") is True:
            complete_runs[cell].append(run_id)
        per_cell[cell].append((rows, summary, result.get("route") if result is not None else None))
        per_cell_ends[cell].append(end)
        log_sources.append((run_id, result, logs))
        # An execution with neither a row nor a log (one that claimed host inapplicability) has nothing to keep.
        if failed is not None and (rows or failed[0]):
            failed_sources.append((cell, run_id, rows, result, *failed))

    # Kept before the coverage check below, which refuses a run with a cell that wrote no row:
    # the logs of the executions that died are the evidence that refusal needs.
    # Like every log kept here, the directory is evidence, not input: failing to write it is
    # reported in the summary line and never fails the ingest.
    failed_entries = []
    if failed_root is not None:
        try:
            shutil.rmtree(failed_root, ignore_errors=True)
            os.makedirs(failed_root)  # fails if a previous ingest's logs could not all be removed
        except OSError as error:
            failed_logs_error = f"cannot create {failed_root}: {error}"
        if failed_logs_error is None:
            with cf.ThreadPoolExecutor(a.j) as ex:
                failed_entries = list(ex.map(lambda s: keep_failed_logs(failed_root, *s, a.failed_log_max_bytes), failed_sources))
            try:
                with open(os.path.join(failed_root, FAILED_LOGS_INDEX), "w") as f:
                    for entry in sorted(failed_entries, key=lambda e: (e["cell"], str(e["run_id"]))):
                        f.write(json.dumps(entry, sort_keys=True) + "\n")
            except OSError as error:
                failed_logs_error = f"cannot write {FAILED_LOGS_INDEX} in {failed_root}: {error}"
        if failed_logs_error is not None:
            print(f"ingest: the failed cells' verify logs were not kept: {failed_logs_error}", file=sys.stderr)
    failed_logs = [one for entry in failed_entries for one in entry["logs"]]

    def covered(cell):
        runs = per_cell.get(cell, [])
        return any(rows for rows, _, _ in runs) or (runs and all(claims_host_inapplicable(cell, s) for _, s, _ in runs))

    def withheld_on_re(cell, rows, summary, route):
        return not rows and route == "re" and claims_host_inapplicable(cell, summary)

    def failed_attempt(cell, rows, summary, route):
        if rows:
            return rows[-1].get("outcome") != "PASS"
        return not claims_host_inapplicable(cell, summary)
    missing = sorted(c for c in want if not covered(c))
    extra = sorted(set(per_cell) - set(want))
    if missing or extra:
        sys.exit(f"ingest: cells with neither a row nor a host-inapplicable claim from every execution: "
                 f"{missing[:20]} ({len(missing)}); unexpected cells: {extra[:20]} ({len(extra)})")
    buckets = collections.defaultdict(list)
    summaries = collections.defaultdict(list)
    evidence_complete = collections.defaultdict(list)
    attempts = collections.Counter()
    final = collections.Counter()
    no_row = []  # "cell attempt N" for each execution that wrote no row and claimed no host inapplicability
    no_row_entries = collections.defaultdict(list)  # (lane, category) -> [(cell, attempt, testx end time)]
    withheld = 0  # executions that withheld their cell on an RE worker, which take no attempt number
    rerun_locally = []  # cells withheld on RE by every execution
    retry_locally = []  # cells whose retry after an attempt was withheld on RE
    for cell, runs in per_cell.items():
        lane, category = want[cell]["lane"], want[cell]["category"]
        # A withheld execution is not an attempt, so its claim is not the cell's
        # outcome when another execution ran the cell.
        ran = [run for run in runs if not withheld_on_re(cell, *run)]
        final_rows, final_summary, _ = (ran or runs)[-1]
        if final_summary:
            summaries[(lane, category)].append(final_summary)
        c = want[cell]
        for run_id in complete_runs[cell]:
            evidence_complete[(lane, category)].append(
                {"test": c["test"], "mode": c["mode"], "backend": None if c["backend"] == "native" else c["backend"],
                 "run_id": run_id})
        if not ran:
            rerun_locally.append(cell_slug(want[cell]))
        elif withheld_on_re(cell, *runs[-1]):
            retry_locally.append(cell_slug(want[cell]))
        elif len(ran) == 1 and len(runs) > 1 and ran[0][2] == "re" and failed_attempt(cell, *ran[0]):
            # Tpx's one retry went to an execution withheld on RE before the
            # cell's only attempt, which failed on RE: that attempt never got
            # the retry an executed run gives it. (A local rerun's attempt has
            # its own Tpx retry.) In the full validation of
            # 2d279d95, c-programs/sigreturn-preemption was withheld on one RE
            # worker, then failed attempt 1 with a skid overshoot on another,
            # and import mode refused it with no attempt 2 to judge.
            retry_locally.append(cell_slug(want[cell]))
        attempt = 0
        for (rows, summary, route), end in zip(runs, per_cell_ends[cell]):
            if withheld_on_re(cell, rows, summary, route):
                withheld += 1
                continue
            attempt += 1
            for row in rows:
                buckets[(lane, category)].append(dict(row, attempt=attempt))
                attempts[attempt] += 1
            if not rows and not claims_host_inapplicable(cell, summary):
                no_row.append(f"{cell} attempt {attempt}")
                no_row_entries[(lane, category)].append((cell, attempt, end))
        final[final_rows[-1]["outcome"] if final_rows else
              "HOST-INAPPLICABLE" if claims_host_inapplicable(cell, final_summary) else "NO-ROW"] += 1
    events = None
    if no_row and a.event_log:
        events = event_log_tests(a.buck2, a.event_log)
    for key in sorted(set(buckets) | set(summaries)):
        lane, category = key
        d = os.path.join(a.out, lane, "manifest_" + category.replace("-", "_"))
        os.makedirs(d, exist_ok=True)
        # An earlier ingest into the same IMPORT_DIR (ci/buck-e2e/run ingests again after a
        # local rerun) may have left one; this ingest's causes replace it.
        no_row_path = os.path.join(d, NO_ROW_FILE)
        if os.path.exists(no_row_path):
            os.remove(no_row_path)
        if no_row_entries[key]:
            with open(no_row_path, "w") as f:
                for cell, attempt, end in sorted(no_row_entries[key]):
                    c = want[cell]
                    f.write(json.dumps({"schema": NO_ROW_SCHEMA, "test": c["test"], "mode": c["mode"],
                                        "backend": None if c["backend"] == "native" else c["backend"],
                                        "attempt": attempt,
                                        "cause": no_row_cause(cell_slug(c), end, events)}, sort_keys=True) + "\n")
        with open(os.path.join(d, "results.jsonl"), "w") as f:
            for row in sorted(buckets[key], key=lambda r: ("{}/{}@{}".format(r["test"], r["mode"], r.get("backend")), r["attempt"])):
                f.write(json.dumps(row, sort_keys=True) + "\n")
        total = {}
        for one in summaries[key]:
            for k, v in one.items():
                if k == "schema":
                    total[k] = v
                elif isinstance(v, list):
                    total.setdefault(k, []).extend(v)
                elif isinstance(v, (int, float)):
                    total[k] = total.get(k, 0) + v
        total["evidence_complete_executions"] = sorted(
            evidence_complete[key], key=lambda c: (c["test"], c["mode"], c["backend"] or "", c["run_id"]))
        with open(os.path.join(d, "summary.json"), "w") as f:
            json.dump(total, f, indent=2, sort_keys=True)
            f.write("\n")
    if a.withheld_retries:
        with open(a.withheld_retries, "w") as f:
            f.writelines(slug + "\n" for slug in sorted(retry_locally))
    if a.withheld_on_re:
        with open(a.withheld_on_re, "w") as f:
            f.writelines(slug + "\n" for slug in sorted(rerun_locally))
    logs_root = os.path.join(a.out, LOGS_DIR)
    shutil.rmtree(logs_root, ignore_errors=True)
    os.makedirs(logs_root)  # fails if a previous ingest's logs could not all be removed
    recorded = collections.Counter(r for _, _, (logs, _, _) in log_sources for r in logs)
    index = {}  # recorded verify-log directory -> (restored directory, None) or (None, why none)
    routes = {}  # recorded verify-log directory -> route_of the execution that recorded it
    for run_id, result, (logs, log_dir, fetch_error) in log_sources:
        shared = {r for r in logs if recorded[r] > 1}
        index.update((r, (None, f"{recorded[r]} executions recorded it, so their logs cannot be told apart")) for r in shared)
        index.update(restore(a.out, run_id, result, {r: v for r, v in logs.items() if r not in shared}, log_dir, fetch_error))
        routes.update((r, route_of(None if r in shared else result)) for r in logs)
    with open(os.path.join(logs_root, LOGS_INDEX), "w") as f:
        for r, (restored, reason) in sorted(index.items()):
            f.write(json.dumps({"schema": LOGS_SCHEMA, "verify_log_dir": r, "restored": restored, "reason": reason,
                                **routes[r]}, sort_keys=True) + "\n")
    unrestored = sorted(f"{r}: {reason}" for r, (_, reason) in index.items() if reason is not None)
    print(json.dumps({"sources": dict(sources), "cells": len(per_cell), "buckets": len(set(buckets) | set(summaries)), "rows": sum(attempts.values()),
                      "attempts": dict(attempts), "final_outcomes": dict(final),
                      "no_row_executions": len(no_row), "no_row_examples": sorted(no_row)[:20],
                      "withheld_on_re_executions": withheld, "withheld_on_re_every_execution": len(rerun_locally),
                      "withheld_retries_on_re": len(retry_locally),
                      "evidence_complete_executions": sum(len(v) for v in evidence_complete.values()),
                      "verify_logs_restored": len(index) - len(unrestored), "verify_logs_unrestored": len(unrestored),
                      "verify_logs_unrestored_examples": unrestored[:5],
                      "failed_executions": len(failed_entries), "failed_verify_logs_error": failed_logs_error,
                      "failed_verify_logs_kept": sum(one["log"] is not None for one in failed_logs),
                      "failed_verify_logs_not_kept": sum(one["log"] is None for one in failed_logs),
                      "failed_executions_without_logs": sum(entry["reason"] is not None for entry in failed_entries)}))

if __name__ == "__main__":
    main()
