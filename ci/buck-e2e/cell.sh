#!/bin/bash
# Run ONE hermit e2e manifest cell through test-harness and return its evidence as
# flat Tpx test-result artifacts (Tpx collects top-level files only).
#
# usage: cell.sh TEST MODE BACKEND [extra test-harness run flags...]
# env:   HERMIT_E2E_BUNDLE         bundle dir (hermit/, bin/, src/, build/, run-state/,
#                                  SOURCE_SHA, and from the :bundle target BUNDLE_RECORD.json):
#                                  the :bundle target, or a prebuilt bundle
#        HERMIT_E2E_BUNDLE_SHA256  optional: BUNDLE.sha256 a prebuilt bundle must match
#        HERMIT_E2E_ROUTE[_REASON] where the generator routed this cell, and why
#        HERMIT_E2E_CONTAINER[_REASON]
#                                  "pinned-root": run the harness inside the bundle
#                                  source's ci/hermetic/run-in-pinned-root.sh, the
#                                  privileged podman container the cargo flow runs every
#                                  e2e node in (local cells only); empty: on this host
#        CELL_DEADLINE_S           optional cap on the harness wall time
#                                  (default: Tpx timeout - 30 s; Tpx returns no
#                                  artifacts at all from an RE action it kills)
#        HERMIT_E2E_KVM_SLOTS      how many local KVM cells may run at once on this host
#                                  (default 16; see acquire_kvm_slot)
#        HERMIT_E2E_KVM_SLOT_DIR   where their slot files live
#                                  (default /tmp/hermit-e2e-kvm-slots-UID)
#        HERMIT_E2E_DIGEST_CACHE_DIR
#                                  where cells on this host share verified file digests
#                                  (default /tmp/hermit-e2e-digests-UID; host cells only)
#        HERMIT_E2E_SESSION_CENSUS the file cells on this host share the harness's process
#                                  session census through (default
#                                  /tmp/hermit-e2e-session-census-UID; host cells only)
# stdout: Tpx HPHP-JSON (`type = "json"`): the harness's test_done for the cell, turned
#         into a failure if its evidence is incomplete, then all_done. Always exits 0.
#         A cell the harness withholds as HOST-INAPPLICABLE on a local route is reported
#         skipped, with the harness's reason; on RE it stays failed, so Tpx retries it on
#         another worker (see ci/buck-e2e/run).
set -u
TEST=$1 MODE=$2 BACKEND=$3
shift 3
CELL="$TEST/$MODE@$BACKEND"
t0=$(date +%s%N)

emit_fatal() {
    python3 -c 'import json,sys; print(json.dumps({"op":"test_done","test":sys.argv[1],"status":"failed","details":json.dumps({"outcome":"ERROR","reason":sys.argv[2]})})); print(json.dumps({"op":"all_done"}))' "$CELL" "$1"
    exit 0
}

[[ -n ${TEST_RESULT_ARTIFACTS_DIR:-} && -n ${TEST_RESULT_ARTIFACT_ANNOTATIONS_DIR:-} ]] ||
    emit_fatal "Tpx artifact dirs are not set; the target needs the tpx-enable-artifact-reporting label"
mkdir -p "$TEST_RESULT_ARTIFACTS_DIR" "$TEST_RESULT_ARTIFACT_ANNOTATIONS_DIR"
# On RE these are relative to /re_cwd; absolutize before anything changes directory.
A=$(realpath "$TEST_RESULT_ARTIFACTS_DIR")
N=$(realpath "$TEST_RESULT_ARTIFACT_ANNOTATIONS_DIR")
# The pid and the clock do not make the id unique: every RE action runs in a PID
# namespace of its own, so $$ is 2 on every worker, and a validate of c977ef11 saw
# two cells on one worker draw the same nanosecond as well. ingest.py refuses a run
# whose id names two executions, so 64 random bits keep two executions apart.
run_id="buck-$(hostname -s)-$$-$(date +%s%N)-$(od -An -N8 -tx8 /dev/urandom | tr -d ' \n')"
[[ $run_id =~ -[0-9a-f]{16}$ ]] || emit_fatal "cannot read 8 random bytes for the run id"
# Named by the run id, so the testx listing shows which run an execution was
# without fetching anything (ingest.py ties local artifact copies to it).
printf '%s\n' "$run_id" >"$A/run_id.$run_id"
B=$(realpath "${HERMIT_E2E_BUNDLE:?}")
bundle_sha=$(cat "$B/BUNDLE.sha256" 2>/dev/null || true)
if [[ -n ${HERMIT_E2E_BUNDLE_SHA256:-} && $bundle_sha != "$HERMIT_E2E_BUNDLE_SHA256" ]]; then
    emit_fatal "bundle $B is ${bundle_sha:-unreadable}, but the targets were generated from $HERMIT_E2E_BUNDLE_SHA256"
fi
if [[ -f $B/SOURCE_SHA ]]; then
    SHA=$(cat "$B/SOURCE_SHA")
else
    SHA=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["source_sha"])' "$B/PROVENANCE.json") ||
        emit_fatal "bundle has neither SOURCE_SHA nor a readable PROVENANCE.json"
fi

# Scratch next to the artifacts dir: not a declared output (never uploaded), and not
# under /tmp, which hermit hides from guests with a private tmpfs.
W=$(mktemp -d "$(dirname "$A")/hermit-cell.XXXXXX") || emit_fatal "cannot create a scratch directory"
mkdir -p "$W/run-state"
cp -a "$B/run-state/." "$W/run-state/" || emit_fatal "cannot copy the compat run state"
slug=$(printf '%s' "$TEST" | tr / -)-$MODE-$BACKEND
tpx_timeout=${TPX_TIMEOUT_SEC:-600}
deadline=${CELL_DEADLINE_S:-$((tpx_timeout - 30))}
((deadline > 10)) || deadline=10

# Every cell outside the pinned root uses the harness's default workdir: a fresh,
# empty per-attempt directory bound at /tmp/test. RE workers cannot mount at /test, and
# backend parity compares a candidate's log with a ptrace reference that may have run on
# RE, so a local cell must see the same workdir or the two guests' inputs differ (a local
# /test tmpfs made every kvm comparison inputs-not-equalized). The pinned-root container
# keeps its fresh tmpfs at /test, and a guest that asserts /test
# (c-programs/environment-and-workdir) runs there for it (defs.bzl PINNED_ROOT_ONLY).
container=${HERMIT_E2E_CONTAINER:-}
workdir=
[[ $container == pinned-root ]] && workdir=HERMIT_E2E_EMPTY_WORKDIR=/test
# env's -u options must precede its assignments (cpu_scan_env may be either).
# The harness measures each budgeted invocation's live CPU from a cgroup of its own and
# refuses the invocation when it cannot create one. An RE worker runs the test inside a
# root-owned cgroup that cell.sh cannot write (cgroup.procs: EACCES), so an RE cell is a
# run without cgroups on purpose and sets HERMIT_E2E_ALLOW_PROCESS_GROUP_CPU_SCAN=1. The
# harness still tries a cgroup first and only then uses the agent-utils process-group
# scan, whose rows name that source. A local cell runs in a cgroup it can use, so it
# never inherits a caller's marker; neither does the pinned-root container.
if [[ ${HERMIT_E2E_ROUTE:-} == re ]]; then
    cpu_scan_env=(HERMIT_E2E_ALLOW_PROCESS_GROUP_CPU_SCAN=1)
else
    cpu_scan_env=(-u HERMIT_E2E_ALLOW_PROCESS_GROUP_CPU_SCAN)
fi
# The bundle rule's BUNDLE_RECORD.json carries the Hermit digest and the manifest
# planner's verdict on src/, computed once for every cell (bundle-record). The harness
# still checks the binary against it, through a digest it hashes once per process and,
# for a cell on this host, a per-host cache keyed by the file's identity (inode, size,
# mtime, ctime); ci/manifest-plan/src/bundle_record.rs. A prebuilt bundle without a
# record runs as before. Both are unset first, so a caller's values never leak in.
bundle_env=()
[[ -f $B/BUNDLE_RECORD.json ]] && bundle_env+=(HERMIT_E2E_BUNDLE_RECORD="$B/BUNDLE_RECORD.json")
extra=()
# The compat bucket node runs with --diagnostic-results; so does its Buck cell.
[[ $TEST == compat/* ]] && extra+=(--diagnostic-results)

# A local KVM cell runs only while it holds one of HERMIT_E2E_KVM_SLOTS (default 16) slots
# shared by every cell on the host. reverie-kvm makes about seven ioctls per guest exit,
# and on this fleet every ioctl passes a host BPF LSM program on security_file_ioctl that
# takes one global ring-buffer spin lock (83% of the cycles in a perf profile of 48 cells),
# so a KVM cell's CPU time grows with the number of KVM cells running beside it. The
# applications/timed-progress-bar verify cell measured 2.4 s of CPU alone, 3.6 s with 8
# running at once, 5.3 s with 16 and 16.6 s with 48, against its 24 s budget; Buck starts
# every runnable local cell at once, while the cargo flow runs at most 8. Measured again
# on 2026-10-17 (on the host named for this default in docs/TESTING_ENVIRONMENTS.md,
# "Named measurement hosts"; 48 copies through test-harness as this script's host path
# runs them), the same cell's harness CPU was 1.39 s at 8, 1.72 s at 16, 3.25 s at 24 and
# 5.50 s at 48 (max 5.73 s), and 48 c-programs KVM verify cells ran 5.25 cells/s at 8 and
# 9.16 at 16 at about 1.3 s of CPU each: the lock costs about a third of what it did.
# With 557 local KVM cells in a full validation, 8 slots bounded e2e.buck_cells, so the
# default is 16. A slot is an
# flock on a file in HERMIT_E2E_KVM_SLOT_DIR (default /tmp/hermit-e2e-kvm-slots-UID),
# held on fd 9 by this script alone (the commands it runs get 9>&-), so it is released
# when this script exits, however it exits. A cell that finds no free slot within half
# its deadline runs without one; result.json records the slot and the wait either way.
kvm_slot='' kvm_slot_wait_ms=0
acquire_kvm_slot() {
    [[ $BACKEND == kvm && ${HERMIT_E2E_ROUTE:-} == local ]] || return 0
    local slots=${HERMIT_E2E_KVM_SLOTS:-16} dir=${HERMIT_E2E_KVM_SLOT_DIR:-/tmp/hermit-e2e-kvm-slots-$(id -u)}
    local fds=() fd i t
    [[ $slots =~ ^[1-9][0-9]*$ ]] || emit_fatal "HERMIT_E2E_KVM_SLOTS must be a positive integer, not '$slots'"
    mkdir -p "$dir" || emit_fatal "cannot create the KVM slot directory $dir"
    for ((i = 0; i < slots; i++)); do
        exec {fd}>>"$dir/slot.$i" || emit_fatal "cannot open the KVM slot file $dir/slot.$i"
        fds+=("$fd")
    done
    t=$(date +%s%N)
    # flock(2) locks the open file description, which this shell shares with the poller,
    # so a slot the poller locks stays locked after it exits. It polls in-process: one
    # flock command per try would fork hundreds of processes a second across the waiters.
    kvm_slot=$(python3 - "$((deadline / 2))" "${fds[@]}" <<'PY'
import fcntl, os, random, sys, time
give_up = time.monotonic() + int(sys.argv[1])
fds = [int(fd) for fd in sys.argv[2:]]
while True:
    for i in random.sample(range(len(fds)), len(fds)):
        try:
            fcntl.flock(fds[i], fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            continue
        os.utime(fds[i])  # the host's daily tmp cleaner deletes files untouched for 4 days
        print(i)
        sys.exit(0)
    if time.monotonic() >= give_up:
        print("none")
        sys.exit(0)
    time.sleep(random.uniform(0.1, 0.4))
PY
    )
    kvm_slot_wait_ms=$((($(date +%s%N) - t) / 1000000))
    if [[ $kvm_slot =~ ^[0-9]+$ ]] && ((kvm_slot < slots)); then
        exec 9>&"${fds[$kvm_slot]}"
    else
        kvm_slot=none
    fi
    for fd in "${fds[@]}"; do exec {fd}>&-; done
}

out=$W/out
case $container in
"")
    acquire_kvm_slot
    # What is left of the deadline after the setup above and any wait for a KVM slot.
    deadline=$((deadline - ($(date +%s%N) - t0) / 1000000000))
    ((deadline > 10)) || deadline=10
    # A host cell also starts the harness's process-group census from the one the last
    # host cell left (runner.rs process_group_members): its first scan then reads only
    # the PIDs created since, not every PID on the host.
    if [[ ${HERMIT_E2E_ROUTE:-} == local ]]; then
        bundle_env+=(HERMIT_E2E_DIGEST_CACHE_DIR="${HERMIT_E2E_DIGEST_CACHE_DIR:-/tmp/hermit-e2e-digests-$UID}"
            HERMIT_E2E_SESSION_CENSUS="${HERMIT_E2E_SESSION_CENSUS:-/tmp/hermit-e2e-session-census-$UID}")
    fi
    # The harness asks git whether an enclosing checkout tracks the bundle's src/ (a local
    # bundle sits in the checkout's buck-out; runner.rs validate_source_snapshot). Where
    # git is the telemetry wrapper of a Meta host, which runs four more git commands and a
    # logger per call, it asks the git behind the wrapper.
    [[ -x /usr/local/bin/git.meta.real ]] && bundle_env+=(HERMIT_E2E_SNAPSHOT_GIT=/usr/local/bin/git.meta.real)
    env -u HERMIT_E2E_EMPTY_WORKDIR -u HERMIT_E2E_BUNDLE_RECORD -u HERMIT_E2E_DIGEST_CACHE_DIR \
        -u HERMIT_E2E_SESSION_CENSUS -u HERMIT_E2E_SNAPSHOT_GIT \
        "${cpu_scan_env[@]}" ${workdir:+"$workdir"} VALIDATE_RUN_STATE="$W/run-state" \
        E2E_RESULT_ROOT="$W/results" E2E_BUILD_ROOT="$B/build" E2E_RUN_ID="$run_id" \
        E2E_KEEP_VERIFY_LOGS=1 E2E_PARITY_POST_PASS=0 \
        HERMIT_BIN="$B/hermit/hermit" HERMIT_INSTALL_DIR="$B/hermit/install" "${bundle_env[@]}" \
        timeout --kill-after=10 "$deadline" \
        "$B/bin/test-harness" run --repo-root "$B/src" --source-sha "$SHA" \
        --test "$TEST" --mode "$MODE" --backend "$BACKEND" --prebuilt --no-retry \
        --results "$out/results.jsonl" --junit "$out/junit.xml" --tpx-json "$out/tpx.jsonl" \
        "${extra[@]}" "$@" >"$W/harness.stdout" 2>"$W/harness.stderr" 9>&-
    harness_rc=$?
    ;;
pinned-root)
    # Cells that need CAP_SYS_ADMIN (DBT's mount namespace), the privileged lane or
    # the container's identity (hostname, /results). The wrapper and the image digest it
    # pins come from the bundle's own source snapshot, as in the cargo flow.
    [[ ${HERMIT_E2E_ROUTE:-} == local ]] ||
        emit_fatal "HERMIT_E2E_CONTAINER=pinned-root needs a local route, not '${HERMIT_E2E_ROUTE:-}'"
    wrapper=$B/src/ci/hermetic/run-in-pinned-root.sh
    [[ -x $wrapper ]] || emit_fatal "the bundle source has no executable ci/hermetic/run-in-pinned-root.sh"
    "$wrapper" --check-image >"$W/check-image.log" 2>&1 ||
        emit_fatal "pinned-root image unavailable: $(tr '\n' ' ' <"$W/check-image.log")(build it with ci/hermetic/build-image.sh)"
    # /src is mounted read-write, as in the cargo flow (--src-rw): hermit writes its
    # private verify and engagement summaries into its working directory, the bundle's
    # src/, when no enclosing git checkout ignores `ignored/`. /src/bundle is therefore
    # a private copy (a reflink on btrfs, about 1 s), never hard links, so nothing in
    # buck-out is written. The mountpoints the wrapper adds under /src are created first.
    R=$W/root
    mkdir -p "$R/target" "$R/agent-utils/rs/target" "$R/agent-utils/rs/.agent-utils-locks" \
        "$R/agent-utils/rs/.agent-utils-snapshots" "$W/results/buck-cell-out" "$W/pinned" ||
        emit_fatal "cannot create the pinned-root scratch tree"
    cp -a --reflink=auto "$B" "$R/bundle" ||
        emit_fatal "cannot place the bundle in the pinned-root scratch tree"
    # DynamoRIO's private loader loads the DBT client's own libc, libm and libgcc_s. It
    # searches the client's RPATH and directory, LD_LIBRARY_PATH and /lib, /lib64,
    # /usr/lib64..., not the paths the image's nix loader knows, so in the container it
    # finds only ld.so and the guest dies before the client connects ("DBT evidence
    # received no image START"). The cargo flow builds the client in the container and
    # does not hit this. So, in the private copy only, the client's directory gets a link
    # to every library the image's own loader resolves for the client outside it,
    # interpreter included: libc and ld.so come from one glibc, and a library the bundle
    # ships is never replaced. The staged bundle and RE cells are unchanged. Only a DBT
    # cell loads the client, so only a DBT cell gets the links.
    link_dbt_runtime='set -eu
r=$1
shift
link() { [ -e "$r/$2" ] || [ -L "$r/$2" ] || ln -s "$1" "$r/$2"; }
deps=$(ldd "$r/libreverie_dbt_client.so" "$r/libdetcore_dbt.so")
printf "%s\n" "$deps" | while read -r name arrow path rest; do
    case "$arrow $path" in
    "=> not"*) echo "cell.sh: the pinned-root image has no $name, which the DBT client needs" >&2; exit 125 ;;
    "=> $r/"*) ;;
    "=> /"*) link "$path" "${name##*/}" ;;
    "("*) case $name in /*) link "$name" "${name##*/}" ;; esac ;;
    esac
done
exec "$@"'
    prologue=()
    [[ $BACKEND != dbt ]] ||
        prologue=(sh -c "$link_dbt_runtime" link-dbt-runtime /src/bundle/hermit/install/rsrcs)
    # Outputs go through the wrapper's /results mount (E2E_RESULT_ROOT).
    out=$W/results/buck-cell-out
    o=/results/buck-cell-out
    # Tpx kills the cell at deadline + 30 s. The wrapper gets what is left of the deadline
    # after the copy above, and the harness that less 15 s for starting and removing the
    # container, so a hung container is stopped here, with its artifacts, before Tpx.
    # A KVM cell's wait for a slot comes out of the same deadline.
    acquire_kvm_slot
    outer=$((deadline - ($(date +%s%N) - t0) / 1000000000))
    ((outer > 25)) || outer=25
    deadline=$((outer - 15))
    # Inside the container the record is the private copy's (paths under /src/bundle).
    inner_bundle_env=()
    ((${#bundle_env[@]})) && inner_bundle_env=(HERMIT_E2E_BUNDLE_RECORD=/src/bundle/BUNDLE_RECORD.json)
    env -u HERMIT_E2E_EMPTY_WORKDIR -u HERMIT_E2E_BUNDLE_RECORD -u HERMIT_E2E_DIGEST_CACHE_DIR \
        -u HERMIT_E2E_SESSION_CENSUS -u HERMIT_E2E_SNAPSHOT_GIT \
        "${cpu_scan_env[@]}" ${workdir:+"$workdir"} VALIDATE_RUN_STATE="$W/run-state" \
        E2E_RESULT_ROOT="$W/results" E2E_RUN_ID="$run_id" \
        E2E_KEEP_VERIFY_LOGS=1 E2E_PARITY_POST_PASS=0 \
        timeout --kill-after=10 "$outer" \
        "$wrapper" --src "$R" --out "$W/pinned" --src-rw \
        --env HERMIT_E2E_EMPTY_WORKDIR --env VALIDATE_RUN_STATE --env E2E_RESULT_ROOT \
        --env E2E_RUN_ID --env E2E_KEEP_VERIFY_LOGS --env E2E_PARITY_POST_PASS \
        --env HERMIT_EPOCH -- \
        "${prologue[@]}" env E2E_BUILD_ROOT=/src/bundle/build \
        HERMIT_BIN=/src/bundle/hermit/hermit HERMIT_INSTALL_DIR=/src/bundle/hermit/install "${inner_bundle_env[@]}" \
        timeout --kill-after=10 "$deadline" \
        /src/bundle/bin/test-harness run --repo-root /src/bundle/src --source-sha "$SHA" \
        --test "$TEST" --mode "$MODE" --backend "$BACKEND" --prebuilt --no-retry \
        --results "$o/results.jsonl" --junit "$o/junit.xml" --tpx-json "$o/tpx.jsonl" \
        "${extra[@]}" "$@" >"$W/harness.stdout" 2>"$W/harness.stderr" 9>&-
    harness_rc=$?
    ;;
*) emit_fatal "unknown HERMIT_E2E_CONTAINER '$container' (expected pinned-root or empty)" ;;
esac
t1=$(date +%s%N)

copy_errors=0
put() { # put SRC NAME: logs over 256 KiB are zstd'd
    local src=$1 name=$2
    if [[ $(stat -c %s "$src") -gt 262144 ]]; then
        zstd -q -3 "$src" -o "$A/$name.zst" || copy_errors=$((copy_errors + 1))
    else
        cp "$src" "$A/$name" || copy_errors=$((copy_errors + 1))
    fi
}
for f in harness.stdout harness.stderr; do put "$W/$f" "$f"; done
for f in results.jsonl junit.xml tpx.jsonl summary.json; do
    [[ -f $out/$f ]] && put "$out/$f" "$f"
done
celldir="$W/results/runs/$run_id/$slug"
if [[ -d $celldir ]]; then
    while IFS= read -r -d '' f; do
        rel=${f#"$celldir/"}
        case $rel in fixtures/* | home/* | xdg-config/* | tmp/* | workdir/* | recording/*) continue ;; esac
        put "$f" "cell__${rel//\//__}"
    done < <(find "$celldir" -type f -print0)
fi

# Evidence policy: a passing verify cell must return its verdict JSON and the DETLOGs
# hermit keeps: after a matched verify only run 1's log (the golden copy; hermit deletes
# run 2's), otherwise both.
outcome=$(python3 -c 'import json,sys
rows=[json.loads(l) for l in open(sys.argv[1]) if l.strip()]
print(rows[-1]["outcome"] if rows else "")' "$out/results.jsonl" 2>/dev/null)
# The same rows as the verdict normalizer will judge them: how many, and the last one's outcome.
rows_seen=$(python3 -c 'import json,os,sys
p=sys.argv[1]
rows=[json.loads(l) for l in open(p) if l.strip()] if os.path.exists(p) else []
assert all(isinstance(r, dict) for r in rows)
print(json.dumps({"count": len(rows), "outcome": rows[-1].get("outcome") if rows else None}))' "$out/results.jsonl" 2>/dev/null) ||
    rows_seen=unreadable
n_run1=$(find "$A" -maxdepth 1 -name 'cell__verify-logs__*run1_log_*' | wc -l)
n_run2=$(find "$A" -maxdepth 1 -name 'cell__verify-logs__*run2_log_*' | wc -l)
n_detlogs=$((n_run1 + n_run2))
verdict=$(python3 -c 'import json,sys
print(json.load(open(sys.argv[1])).get("verdict") or "")' "$A/cell__verify-1.json" 2>/dev/null) || verdict=
evidence_complete=true
missing=()
[[ -s $out/tpx.jsonl ]] || { evidence_complete=false; missing+=(tpx.jsonl); }
if [[ $outcome == PASS && $MODE == verify ]]; then
    # hermit always writes a verdict; a file without one is not a verify report.
    [[ -s $A/cell__verify-1.json && -n $verdict ]] || { evidence_complete=false; missing+=(verify-1.json); }
    if [[ $verdict == matched ]]; then
        ((n_run1 == 1 && n_run2 == 0)) || { evidence_complete=false; missing+=(detlogs); }
    else
        ((n_run1 >= 1 && n_run2 >= 1)) || { evidence_complete=false; missing+=(detlogs); }
    fi
fi
((copy_errors == 0)) || { evidence_complete=false; missing+=("copy_errors=$copy_errors"); }

python3 - "$A" "$B" <<PY
import hashlib, json, os, sys
art, bundle = sys.argv[1:3]
json.dump({
    "schema": 1, "cell": "$CELL", "test": "$TEST", "mode": "$MODE", "backend": "$BACKEND",
    "route": "${HERMIT_E2E_ROUTE:-}", "route_reason": "${HERMIT_E2E_ROUTE_REASON:-}",
    "container": os.environ.get("HERMIT_E2E_CONTAINER", ""),
    "container_reason": os.environ.get("HERMIT_E2E_CONTAINER_REASON", ""),
    "empty_workdir": "$workdir",
    "kvm_slot": "$kvm_slot", "kvm_slot_wait_ms": $kvm_slot_wait_ms,
    "harness_rc": $harness_rc, "outcome": "$outcome", "deadline_s": $deadline,
    "tpx_timeout_s": $tpx_timeout, "wall_ms": $(((t1 - t0) / 1000000)),
    "evidence_complete": "$evidence_complete" == "true", "missing": "${missing[*]}".split(),
    "detlogs": $n_detlogs, "host": "$(hostname)", "re_platform": "${RE_PLATFORM:-local}",
    "nproc": $(nproc), "run_id": "$run_id", "bundle_sha256": "$bundle_sha",
    "source_sha": "$SHA",
    "provenance": json.load(open(os.path.join(bundle, "PROVENANCE.json"))) if os.path.exists(os.path.join(bundle, "PROVENANCE.json")) else None,
    # sha256 of every other artifact as written here, so a fetch can be checked byte-exact.
    "artifact_sha256": {n: hashlib.sha256(open(os.path.join(art, n), "rb").read()).hexdigest()
                        for n in sorted(os.listdir(art)) if n != "result.json"},
}, open(os.path.join(art, "result.json"), "w"), indent=1, sort_keys=True)
PY
for f in result.json tpx.jsonl summary.json results.jsonl harness.stderr; do
    [[ -f $A/$f ]] && printf '{"type": {"generic_text_log": {}}, "description": "%s"}\n' "$f" >"$N/$f.annotation"
done
rm -rf "${W:?}"

# Exactly one record, named for this cell, whatever the harness wrote, and the verdict on
# the harness's output recorded for ingest.py as result.json's producer_completion. An
# unreadable, malformed, contradictory or ambiguous payload is a failed cell and an
# "invalid" completion: never a cell with no record (Tpx would count only the target's
# passing "unmanaged" twin) and never a pass. The normalizer only prints: its record,
# all_done and the completion it proposes. Only once it has exited 0 with exactly those
# three lines does cell.sh publish the completion, by renaming a finished copy of
# result.json over the original, so a normalizer or publication that fails at any point
# leaves result.json without producer_completion, which ingest.py refuses.
verdict_out=$(python3 - "$A/tpx.jsonl" "$CELL" "$evidence_complete" "$harness_rc" "${missing[*]}" "${HERMIT_E2E_ROUTE:-}" "$rows_seen" <<'PY'
import json, os, re, sys
path, cell, complete, rc, missing, route, rows_seen = sys.argv[1:8]

class Invalid(Exception):
    """The harness output breaks its contract with this cell; the reason says how."""

def failed(reason):
    details = {"outcome": "ERROR", "reason": reason}
    return {"op": "test_done", "test": cell, "status": "failed", "details": json.dumps(details)}

def details_of(record):
    """The record's details as a dict, or None unless they are a JSON object in a string."""
    raw = record.get("details")
    if not isinstance(raw, str):
        return None
    try:
        value = json.loads(raw)
    except ValueError:
        return None
    return value if isinstance(value, dict) else None

def read_records():
    """Every nonblank line of the payload, each a JSON object."""
    if not os.path.exists(path):
        return []
    try:
        with open(path) as f:
            records = [json.loads(l) for l in f if l.strip()]
    except (OSError, UnicodeDecodeError, ValueError) as error:
        raise Invalid(f"unreadable tpx.jsonl: {error}")
    if not all(isinstance(r, dict) for r in records):
        raise Invalid("unreadable tpx.jsonl: a record is not a JSON object")
    return records

def read_rows():
    """(how many rows the harness wrote, the last row's outcome or None)."""
    try:
        seen = json.loads(rows_seen)
    except ValueError:
        raise Invalid(f"harness rc={rc} wrote an unreadable results.jsonl")
    if seen["count"] > 1:
        raise Invalid(f"harness rc={rc} wrote {seen['count']} rows; a cell runs the harness once, which writes one")
    return seen["count"], seen["outcome"]

def withheld_pair(done, rows):
    """The cell's own record when the harness withheld this one cell as HOST-INAPPLICABLE
    and failed the run only because it executed nothing; else None. A withheld cell runs
    nothing, so it writes no row. This is valid output on any route; withheld_here decides
    what the route makes of it."""
    if rows != 0 or len(done) != 2:
        return None
    own = [r for r in done if r.get("test") == cell]
    run = [r for r in done if r.get("test") == "test-harness/run-verdict"]
    if (len(own) != 1 or len(run) != 1 or own[0].get("status") != "skipped"
            or run[0].get("status") != "failed"):
        return None
    details, run_details = details_of(own[0]), details_of(run[0])
    if details is None or run_details is None:
        return None
    failures = run_details.get("run_failures")
    vacuous = r"every one of the 1 selected cell\(s\) was host-inapplicable; .*"
    if (details.get("outcome") != "HOST-INAPPLICABLE" or not isinstance(failures, list)
            or len(failures) != 1 or not isinstance(failures[0], str)
            or not re.fullmatch(vacuous, failures[0])):
        return None
    return own[0], details

def withheld_here(pair):
    """The withheld cell's record, as a skip, on a local route; else None.

    A local host that lacks a capability lacks it on a retry too, so the cell is
    reported skipped with the harness's reason, never as a pass. Only a local route
    qualifies: an RE worker that lacks the capability is retried on another worker."""
    if route != "local" or pair is None:
        return None
    record, details = pair
    reason = details.get("reason")
    details["reason"] = "SKIP: " + (reason if isinstance(reason, str) and reason else "host-inapplicable")
    return dict(record, details=json.dumps(details))

def no_result_pair(done, rows):
    """The cell's own record when the harness ran this one cell and got no result back:
    it reports the cell failed with a null outcome and fails the run for exactly that
    reason, with a nonzero exit and no row; else None."""
    if rows != 0 or rc == "0" or len(done) != 2:
        return None
    own = [r for r in done if r.get("test") == cell]
    run = [r for r in done if r.get("test") == "test-harness/run-verdict"]
    if (len(own) != 1 or len(run) != 1 or own[0].get("status") != "failed"
            or run[0].get("status") != "failed"):
        return None
    details, run_details = details_of(own[0]), details_of(run[0])
    if details is None or run_details is None or "outcome" not in details or details["outcome"] is not None:
        return None
    if run_details.get("run_failures") != ["only 0 of 1 selected cells returned a result"]:
        return None
    return own[0]

def verdict():
    """The one record for Tpx; raises Invalid when the harness output is invalid."""
    rows, row_outcome = read_rows()
    done = [r for r in read_records() if r.get("op") == "test_done"]
    pair = withheld_pair(done, rows)
    skip = withheld_here(pair)
    if skip:
        return skip
    if pair:
        # The existing RE policy: a withheld cell fails there, so Tpx retries it.
        return failed(f"harness rc={rc} produced {len(done)} test_done records")
    lost = no_result_pair(done, rows)
    if lost:
        # A failure like any other, which ingest.py keeps as the attempt it was, so a Tpx
        # retry imports with that history.
        return lost
    if not done and rc != "0" and (rows == 0 or row_outcome not in ("PASS", None)):
        # The harness died before its record (a deadline kill, a crash): a failure like any
        # other, which ingest.py keeps as the attempt it was. Its exit status or a passing
        # row would contradict that, so those stay invalid.
        return failed(f"harness rc={rc} produced 0 test_done records")
    if len(done) != 1:
        raise Invalid(f"harness rc={rc} produced {len(done)} test_done records")
    record = done[0]
    if record.get("test") != cell:
        raise Invalid(f"harness rc={rc} reported test {record.get('test')!r}, not {cell}")
    status, details = record.get("status"), details_of(record)
    if status not in ("passed", "failed", "skipped"):
        raise Invalid(f"harness rc={rc} reported status {status!r}")
    if details is None:
        raise Invalid(f"harness rc={rc} reported {status} without JSON-object details")
    outcome = details.get("outcome")
    # The harness writes `passed` exactly for a PASS outcome; for one selected cell, a
    # lone `skipped` only for a FAIL it excuses as a diagnostic (a withheld cell is the
    # pair above); and `failed` otherwise. A null outcome comes only with the run
    # failure no_result_pair recognizes, never alone. The row carries the record's outcome.
    if status == "passed" and outcome != "PASS":
        raise Invalid(f"harness rc={rc} reported a passing record with outcome {outcome!r}")
    if status == "skipped" and outcome != "FAIL":
        raise Invalid(f"harness rc={rc} reported a skipped record with outcome {outcome!r}")
    if status == "failed" and not (isinstance(outcome, str) and outcome
                                   and outcome not in ("PASS", "HOST-INAPPLICABLE")):
        raise Invalid(f"harness rc={rc} reported a failed record with outcome {outcome!r}")
    if outcome != (row_outcome if rows else None):
        raise Invalid(f"harness rc={rc} reported outcome {outcome!r} but its rows say "
                      f"{row_outcome if rows else 'nothing'!r}")
    if status == "passed" and rc != "0":
        raise Invalid(f"harness rc={rc} after a passing record")
    if status == "passed" and complete != "true":
        details.update(outcome="ERROR", reason=f"evidence incomplete: {missing}")
        return dict(record, status="failed", details=json.dumps(details))
    return record

try:
    record, problem = verdict(), None
except Invalid as error:
    record, problem = failed(str(error)), str(error)
except Exception as error:  # a normalizer fault is the cell's failure, not a missing record
    problem = f"harness rc={rc}: verdict normalizer error: {type(error).__name__}: {error}"
    record = failed(problem)
print(json.dumps(record))
print(json.dumps({"op": "all_done"}))
print(json.dumps({"version": 1, "status": "invalid" if problem else "valid", "reason": problem}))
PY
)
normalizer_rc=$?
mapfile -t verdict_lines <<<"$verdict_out"
if [[ $normalizer_rc -ne 0 || ${#verdict_lines[@]} -ne 3 || ${verdict_lines[1]} != '{"op": "all_done"}' ]]; then
    emit_fatal "verdict normalizer exited $normalizer_rc without exactly one record (harness rc=$harness_rc)"
fi
candidate=$(mktemp "$(dirname "$A")/hermit-cell-result.XXXXXX") ||
    emit_fatal "cannot create a scratch result.json (harness rc=$harness_rc)"
if ! python3 - "$A/result.json" "$candidate" "$CELL" "${verdict_lines[0]}" "${verdict_lines[2]}" <<'PY' ||
import json, os, sys
result_path, candidate, cell, record, completion = sys.argv[1:6]
record, completion = json.loads(record), json.loads(completion)
assert record["op"] == "test_done" and record["test"] == cell, record
assert record["status"] in ("passed", "failed", "skipped"), record
assert set(completion) == {"version", "status", "reason"} and type(completion["version"]) is int, completion
assert completion["version"] == 1, completion
assert ((completion["status"] == "valid" and completion["reason"] is None)
        or (completion["status"] == "invalid" and isinstance(completion["reason"], str))), completion
assert completion["status"] == "valid" or record["status"] == "failed", (record, completion)
with open(result_path) as f:
    result = json.load(f)
assert "producer_completion" not in result, result_path
result["producer_completion"] = completion
with open(candidate, "w") as f:
    json.dump(result, f, indent=1, sort_keys=True)
    f.flush()
    os.fsync(f.fileno())
os.chmod(candidate, os.stat(result_path).st_mode & 0o777)
PY
    ! mv -f -- "$candidate" "$A/result.json"; then
    rm -f -- "$candidate"
    emit_fatal "cannot publish the producer completion in result.json (harness rc=$harness_rc)"
fi
printf '%s\n' "${verdict_lines[0]}" "${verdict_lines[1]}"
exit 0
