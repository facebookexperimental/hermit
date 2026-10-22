#!/usr/bin/env python3
"""Judge a Buck e2e run as the cargo flow judges its e2e nodes.

usage: verdict.py --plan ci/expected-e2e-plan.json --import-dir IMPORT_DIR --out DIR
                  --harness TEST_HARNESS --repo-root ROOT [--source-sha SHA]

IMPORT_DIR is what ingest.py wrote. For each (lane, category) bucket of the plan, this
runs TEST_HARNESS in import mode (E2E_IMPORT_RESULTS=IMPORT_DIR) with the selection that
bucket's e2e node uses in the cargo flow:

    TEST_HARNESS run --repo-root ROOT [--source-sha SHA] --lane LANE --category CATEGORY
        --ci-only --prebuilt [--diagnostic-results]
        --results DIR/LANE/manifest_CATEGORY/results.jsonl
        --junit DIR/LANE/manifest_CATEGORY/junit.xml

and keeps its output in DIR/LANE/manifest_CATEGORY/harness.log. Only a bucket listed in
DIAGNOSTIC_MANIFEST_BUCKETS (ci/manifest-plan/src/validation_dag.rs: compat) gets
--diagnostic-results, as only its node does; a plan that puts a diagnostic cell in any
other bucket fails that bucket, because its node refuses to select one.
The harness decides every cell's verdict as it does for the rows of an executed run:
which retries this run would have made (no compat cell is retried, so a second Tpx
execution of one is dropped), staleness, evidence, host inapplicability on this machine,
a cell with no result, and the diagnostic excuse. DIR receives the result files each node
would publish. The node flags that change nothing here are not passed: --allow-empty (a
plan bucket has cells), --jobs (nothing executes) and --exclude-backend kvm (the plan
includes KVM cells).

A bucket passes only when the harness exits 0 and its summary.json reports an import of
exactly the bucket's plan cells, with no ERROR and no FAIL beyond the excused diagnostic
ones. An excused diagnostic failure is printed as DIAGNOSTIC; as in the cargo flow, a
run that has one is not a qualifying receipt. Exit status: 0 when every bucket passes;
1 when any bucket does not (each failing cell, or why the bucket has no verdict, goes to
stderr); 2 when judging could not run: bad arguments, an unreadable plan, a DIR that
already holds files, or a harness that cannot be executed. Prints one JSON summary line.

The environment is passed on except DAGRUN_TEST_COUNTS_PATH: one file cannot hold the
counts of several harness runs, and this reports to no scheduler. The harness compares
each row's timeouts with its own, so HERMIT_TEST_CPU_TIMEOUT_MULTIPLIER and
HERMIT_TEST_WALL_TIMEOUT_MULTIPLIER must be as they were for the cells (cell.sh sets
neither); otherwise every row is an import-stale ERROR.
"""
import argparse, collections, json, os, subprocess, sys
import xml.etree.ElementTree as ET

COUNTS = ("cells", "passed", "failed", "errors", "diagnostic_failures", "host_inapplicable")
IMPORTED = ("missing_cells", "dropped_retries")
# DIAGNOSTIC_MANIFEST_BUCKETS in ci/manifest-plan/src/validation_dag.rs; a test pins the copy.
DIAGNOSTIC_BUCKETS = ("compat",)

def refuse(message):
    print(f"verdict: {message}", file=sys.stderr)
    sys.exit(2)

def bucket_dir(root, lane, category):
    # imported_results.rs (bucket_dir) and ingest.py name a bucket's directory this way.
    return os.path.join(root, lane, "manifest_" + category.replace("-", "_"))

def junit_name(cell):
    # runner.rs write_junit names a testcase TEST/MODE/BACKEND, "none" for no backend.
    return "{}/{}/{}".format(cell["test"], cell["mode"], cell.get("backend") or "none")

def first_line(text):
    return ((text or "").strip().splitlines() or [""])[0][:300]

def read_summary(path):
    try:
        with open(path) as f:
            return json.load(f)
    except FileNotFoundError:
        return None
    except (OSError, ValueError) as error:
        return {"unreadable": str(error)}

def no_verdict(rc, summary, planned):
    """Why a bucket whose harness exited RC and wrote SUMMARY (None: none) did not pass,
    or None when it passed."""
    if rc != 0:
        died = f"was killed by signal {-rc}" if rc < 0 else f"exited {rc}"
        return f"test-harness {died}" + ("" if summary else " and wrote no summary.json")
    if summary is None:
        return "test-harness exited 0 but wrote no summary.json"
    if "unreadable" in summary:
        return f"test-harness exited 0 but its summary.json is unreadable: {summary['unreadable']}"
    if "imported" not in summary:
        return ("test-harness exited 0 but its summary.json is not an import's, so it ignored "
                "E2E_IMPORT_RESULTS; judge with the harness staged from this checkout (ci/buck-e2e/stage)")
    imported = summary["imported"] if isinstance(summary["imported"], dict) else {}
    counts = {key: summary.get(key) for key in COUNTS}
    counts["missing_cells"] = imported.get("missing_cells")
    absent = [key for key, value in counts.items() if type(value) is not int]
    if absent:
        return (f"test-harness exited 0 but its summary.json has no integer {', '.join(absent)}; "
                "judge with the harness staged from this checkout (ci/buck-e2e/stage)")
    if summary["cells"] != planned:
        return (f"test-harness exited 0 but its summary.json has {summary.get('cells')} cells and the plan "
                f"{planned}; judge with the harness and plan of one commit (ci/buck-e2e/stage)")
    outcomes = summary["passed"] + summary["failed"] + summary["errors"] + summary["host_inapplicable"]
    if outcomes != summary["cells"]:
        return (f"test-harness exited 0 but its summary.json counts {outcomes} PASS, FAIL, ERROR and "
                f"HOST-INAPPLICABLE outcomes for {summary['cells']} cells")
    if counts["missing_cells"] != 0:
        return f"test-harness exited 0 but its summary.json counts {counts['missing_cells']} imported cells missing"
    if summary["errors"] != 0 or summary["failed"] != summary["diagnostic_failures"]:
        return (f"test-harness exited 0 but its summary.json counts {summary.get('failed')} FAIL "
                f"({summary.get('diagnostic_failures')} diagnostic) and {summary.get('errors')} ERROR")
    return None

def failing_cells(junit, excused):
    """(outcome, testcase name, first line of the reason) for each cell junit.xml reports
    as a FAIL or an ERROR, other than the excused diagnostic failures; None if unreadable."""
    try:
        cases = list(ET.parse(junit).getroot().iter("testcase"))
    except (OSError, ET.ParseError):
        return None
    found = []
    for case in cases:
        for tag, outcome in (("failure", "FAIL"), ("error", "ERROR")):
            node = case.find(tag)
            if node is not None and not (outcome == "FAIL" and case.get("name") in excused):
                found.append((outcome, case.get("name"), first_line(node.text)))
    return found

def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--plan", required=True, help="ci/expected-e2e-plan.json")
    ap.add_argument("--import-dir", required=True, help="IMPORT_DIR: what ingest.py wrote")
    ap.add_argument("--out", required=True, help="DIR: absent or empty; receives each bucket's result files")
    ap.add_argument("--harness", required=True, help="the test-harness to judge with (ci/buck-e2e/staged/bin/test-harness)")
    ap.add_argument("--repo-root", required=True, help="the checkout whose manifests and test sources the rows must match")
    ap.add_argument("--source-sha", help="ROOT is a `git archive` of commit SHA (test-harness --source-sha)")
    a = ap.parse_args()
    try:
        with open(a.plan) as f:
            cells = json.load(f)["cells"]
        buckets = collections.defaultdict(lambda: [0, []])  # (lane, category) -> [cells, diagnostic cells]
        for cell in cells:
            bucket = buckets[(cell["lane"], cell["category"])]
            bucket[0] += 1
            if cell.get("classification") == "diagnostic":
                bucket[1].append(junit_name(cell))
    except (OSError, ValueError, KeyError, TypeError) as error:
        refuse(f"cannot read the plan {a.plan}: {error!r}; pass ci/expected-e2e-plan.json")
    if not buckets:
        refuse(f"the plan {a.plan} has no cells; pass ci/expected-e2e-plan.json")
    out, harness = os.path.abspath(a.out), os.path.abspath(a.harness)
    if os.path.lexists(out) and not (os.path.isdir(out) and not os.listdir(out)):
        refuse(f"{out} already exists and is not an empty directory; test-harness appends to the "
               f"results.jsonl it is given, so remove {out} or pass a new --out")
    if not (os.path.isfile(harness) and os.access(harness, os.X_OK)):
        refuse(f"{harness} is not an executable file; stage the inputs (ci/buck-e2e/stage) or pass --harness")
    env = dict(os.environ, E2E_IMPORT_RESULTS=os.path.abspath(a.import_dir), E2E_RESULT_ROOT=out)
    env.pop("DAGRUN_TEST_COUNTS_PATH", None)
    totals = collections.Counter()
    failed = []
    for (lane, category), (planned, diagnostic) in sorted(buckets.items()):
        if diagnostic and category not in DIAGNOSTIC_BUCKETS:
            failed.append(f"{lane}/{category}")
            print(f"verdict: {lane}/{category} did not pass: the plan puts the diagnostic cells "
                  f"{', '.join(diagnostic)} in it, but only a {' or '.join(DIAGNOSTIC_BUCKETS)} bucket may "
                  "hold one (validation_dag.rs DIAGNOSTIC_MANIFEST_BUCKETS) and its node refuses to "
                  "select them; declare them required in their manifest, or make the category a "
                  "diagnostic bucket in validation_dag.rs and here", file=sys.stderr)
            continue
        d = bucket_dir(out, lane, category)
        os.makedirs(d)
        command = [harness, "run", "--repo-root", os.path.abspath(a.repo_root)]
        command += ["--source-sha", a.source_sha] if a.source_sha else []
        command += ["--lane", lane, "--category", category, "--ci-only", "--prebuilt"]
        command += ["--diagnostic-results"] if category in DIAGNOSTIC_BUCKETS else []
        command += ["--results", os.path.join(d, "results.jsonl"), "--junit", os.path.join(d, "junit.xml")]
        try:
            with open(os.path.join(d, "harness.log"), "w") as log:
                rc = subprocess.run(command, stdout=log, stderr=subprocess.STDOUT, env=env).returncode
        except OSError as error:
            refuse(f"cannot run {harness}: {error}; stage the inputs (ci/buck-e2e/stage) or pass --harness")
        summary = read_summary(os.path.join(d, "summary.json"))
        counted = summary if summary and "unreadable" not in summary else {}
        imported = counted.get("imported") if isinstance(counted.get("imported"), dict) else {}
        for key in COUNTS:
            totals[key] += counted.get(key) if type(counted.get(key)) is int else 0
        for key in IMPORTED:
            totals[key] += imported.get(key) if type(imported.get(key)) is int else 0
        excused = {junit_name(cell): cell for cell in counted.get("diagnostic_failure_cells") or []}
        for name, cell in sorted(excused.items()):
            print(f"verdict: DIAGNOSTIC {lane}/{category} {name}: a diagnostic cell's product failure "
                  f"does not fail the run: {first_line(cell.get('failure_reason'))}", file=sys.stderr)
        reason = no_verdict(rc, summary, planned)
        if reason is None:
            continue
        failed.append(f"{lane}/{category}")
        print(f"verdict: {lane}/{category} did not pass: {reason}", file=sys.stderr)
        cases = failing_cells(os.path.join(d, "junit.xml"), excused)
        for outcome, name, why in cases or []:
            print(f"verdict:   {outcome} {name}: {why}", file=sys.stderr)
        if not cases:
            try:
                with open(os.path.join(d, "harness.log"), errors="replace") as f:
                    tail = f.read().splitlines()[-20:]
            except OSError as error:
                tail = [f"(harness.log unreadable: {error})"]
            print(f"verdict:   no failing cell in junit.xml; the end of {d}/harness.log:", file=sys.stderr)
            for line in tail:
                print(f"verdict:   | {line}", file=sys.stderr)
    print(json.dumps({"buckets": len(buckets), "failed_buckets": failed, **{k: totals[k] for k in COUNTS + IMPORTED}}))
    sys.exit(1 if failed else 0)

if __name__ == "__main__":
    main()
