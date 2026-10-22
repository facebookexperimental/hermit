#!/usr/bin/env bash
# Self-test for ci/apt-install-cached.sh, the cached and bounded apt install
# used by every `Dependencies` step of .github/workflows/ci-portable.yml
# (https://github.com/rrnewton/hermit/issues/3487).
#
# WHY THIS EXISTS. The script's claims are about control flow: which apt phases
# are retried and which never are, that a final failure keeps its own exit
# status, that a cache hit contacts no mirror, and that an unusable cache falls
# back instead of failing. Those claims can be checked without root, without a
# mirror and without installing anything, by putting stub `sudo`, `apt-get`,
# `dpkg` and `dpkg-query` commands first on PATH. The real coreutils `timeout`
# stays in use, so the hang case exercises the real per-attempt bound.
#
# Each assertion is written so that it fails if the property is broken, and
# several are checked both ways (a retried phase succeeding on attempt 2 AND a
# persistent failure returning its own status), in the style of
# ci/hermetic/tests/test-retry-fetch.sh.
#
# ⚠️ WHAT THIS DOES NOT PROVE. It does not run the real apt. The behaviour of
# real apt against a restored archive directory (offline install with empty
# package lists, a broken archive failing before dpkg runs, the root-owned
# `lock` and `_apt`-owned `partial/` that seal_archive removes) was checked by
# hand in an ubuntu:24.04 container and is described in the pull request that
# added this file.
set -uo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
script="$here/apt-install-cached.sh"

failures=0
check() {
    if [[ $2 -eq 0 ]]; then
        echo "ok   - $1"
    else
        echo "FAIL - $1"
        failures=$((failures + 1))
    fi
}
is() { if [[ $1 == "$2" ]]; then echo 0; else echo 1; fi; }
has() { if grep -q -- "$2" "$1"; then echo 0; else echo 1; fi; }
hasf() { if grep -qF -- "$2" "$1"; then echo 0; else echo 1; fi; }
lacks() { if grep -q -- "$2" "$1"; then echo 1; else echo 0; fi; }

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
mkdir -p "$work/bin"

# sudo: run the command as the current user.
cat >"$work/bin/sudo" <<'EOF'
#!/usr/bin/env bash
exec "$@"
EOF

cat >"$work/bin/dpkg" <<'EOF'
#!/usr/bin/env bash
if [[ ${1:-} == --print-architecture ]]; then echo amd64; exit 0; fi
echo "stub dpkg: unexpected arguments: $*" >&2
exit 99
EOF

# dpkg-query -W -f='${Status}' NAME: installed when NAME is in $STUB_STATE/installed.
cat >"$work/bin/dpkg-query" <<'EOF'
#!/usr/bin/env bash
name=${!#}
if grep -qx -- "$name" "$STUB_STATE/installed" 2>/dev/null; then
    printf 'install ok installed'
    exit 0
fi
exit 1
EOF

# apt-get: classify the call into one phase, log it, then behave as the phase's
# STUB_<PHASE> variable says:
#   ok           succeed
#   fail:N       always exit N
#   fail-once:N  exit N on the first call of this phase, succeed afterwards
#   hang         sleep far past any per-attempt timeout the test sets
cat >"$work/bin/apt-get" <<'EOF'
#!/usr/bin/env bash
set -u
dir= names=() debs=() download=0 nodownload=0 phase=
args=("$@")
for (( i = 0; i < ${#args[@]}; i++ )); do
    a=${args[i]}
    case $a in
        -o) i=$(( i + 1 )); o=${args[i]}
            [[ $o == Dir::Cache::Archives=* ]] && dir=${o#Dir::Cache::Archives=} ;;
        --download-only) download=1 ;;
        --no-download) nodownload=1 ;;
        -*|install|update) ;;
        *.deb) debs+=("$a") ;;
        *) names+=("$a") ;;
    esac
done
if [[ ${1:-} == update ]]; then phase=update
elif (( download )); then phase=download
elif (( nodownload )) && (( ${#debs[@]} )); then phase=cachefile
elif (( nodownload )); then phase=install
else echo "stub apt-get: unclassified call: $*" >&2; exit 98
fi
echo "$phase $*" >>"$STUB_STATE/calls"
count_file="$STUB_STATE/count.$phase"
n=$(( $(cat "$count_file" 2>/dev/null || echo 0) + 1 ))
echo "$n" >"$count_file"
var=STUB_${phase^^}
behaviour=${!var:-ok}
case $behaviour in
    ok) ;;
    fail:*) exit "${behaviour#fail:}" ;;
    fail-once:*) (( n == 1 )) && exit "${behaviour#fail-once:}" ;;
    hang) sleep 60; exit 0 ;;
    *) echo "stub apt-get: bad behaviour '$behaviour'" >&2; exit 97 ;;
esac
case $phase in
    download)
        # What real apt leaves behind as root: the files, a lock, and a
        # leftover partial download.
        for name in "${names[@]}"; do
            printf 'deb %s\n' "$name" >"${dir%/}/${name}_1.0_amd64.deb"
        done
        : >"${dir%/}/lock"
        mkdir -p "${dir%/}/partial"
        printf 'half\n' >"${dir%/}/partial/leftover_1.0_amd64.deb"
        ;;
    install)
        printf '%s\n' "${names[@]}" >>"$STUB_STATE/installed" ;;
    cachefile)
        for deb in "${debs[@]}"; do
            name=$(basename "$deb"); name=${name%%_*}
            [[ $name == "${STUB_CACHEFILE_SKIP:-}" ]] && continue
            printf '%s\n' "$name" >>"$STUB_STATE/installed"
        done
        : >"${dir%/}/lock"
        ;;
esac
exit 0
EOF
chmod +x "$work/bin/"*

# fresh: reset all per-case state and the environment the script reads.
fresh() {
    rm -rf "$work/state" "$work/cache" "$work/out"
    mkdir -p "$work/state"
    : >"$work/out"
    export STUB_STATE="$work/state"
    export HERMIT_APT_CACHE_ROOT="$work/cache"
    export GITHUB_OUTPUT="$work/out"
    export HERMIT_APT_BACKOFF_SECONDS=0
    export HERMIT_APT_ATTEMPTS=3
    export ImageOS=ubuntu24 ImageVersion=20260928.1
    unset STUB_UPDATE STUB_DOWNLOAD STUB_INSTALL STUB_CACHEFILE STUB_CACHEFILE_SKIP \
        APT_CACHE_HIT HERMIT_APT_UPDATE_TIMEOUT_SECONDS \
        HERMIT_APT_DOWNLOAD_TIMEOUT_SECONDS HERMIT_APT_KILL_AFTER_SECONDS
}
run() { PATH="$work/bin:$PATH" "$script" "$@"; }
count() { cat "$work/state/count.$1" 2>/dev/null || echo 0; }
key_of() {
    fresh
    APT_PACKAGES=$1 run prepare >/dev/null 2>&1 || { echo "prepare-failed"; return; }
    sed -n 's/^key=//p' "$work/out"
}
archive="$work/cache/archives"

# --- cache key -------------------------------------------------------------
k1=$(key_of "jq zstd bc")
k2=$(key_of "  zstd
bc   jq jq ")
check "key is independent of package order, duplicates and whitespace" "$(is "$k1" "$k2")"
check "key has the documented shape" \
    "$([[ $k1 =~ ^apt-debs-v1-ubuntu24-20260928\.1-amd64-[0-9a-f]{16}-s[0-9a-f]{8}$ ]] && echo 0 || echo 1)"
k3=$(key_of "jq zstd bc gdb")
check "key changes when the package list changes" "$([[ $k1 != "$k3" ]] && echo 0 || echo 1)"
fresh; export ImageVersion=20261005.2
APT_PACKAGES="jq zstd bc" run prepare >/dev/null 2>&1
k4=$(sed -n 's/^key=//p' "$work/out")
check "key changes when the runner image version changes" "$([[ -n $k4 && $k1 != "$k4" ]] && echo 0 || echo 1)"
# An edit to the script (say, a new apt-get option) must start fresh entries.
cp "$script" "$work/edited-apt-install-cached.sh"
printf '# an edit that changes nothing but the bytes\n' >>"$work/edited-apt-install-cached.sh"
fresh
APT_PACKAGES="jq zstd bc" PATH="$work/bin:$PATH" "$work/edited-apt-install-cached.sh" prepare >/dev/null 2>&1
k5=$(sed -n 's/^key=//p' "$work/out")
check "key changes when ci/apt-install-cached.sh changes" "$([[ -n $k5 && $k1 != "$k5" ]] && echo 0 || echo 1)"
check "  ...and only in its script-hash part" "$(is "${k5%-s*}" "${k1%-s*}")"
fresh
APT_PACKAGES="jq" run prepare >/dev/null 2>&1
check "prepare creates the archive directory and partial/" \
    "$([[ -d $archive/partial ]] && echo 0 || echo 1)"
check "prepare writes dir= to GITHUB_OUTPUT" "$(has "$work/out" "^dir=$archive$")"

# --- input validation --------------------------------------------------------
for bad in "-y" "foo=1" "Foo" "jq;ls"; do
    fresh; rc=0
    APT_PACKAGES="jq $bad" run prepare >/dev/null 2>&1 || rc=$?
    check "refuses package argument '$bad' (exit 2)" "$(is "$rc" 2)"
done
fresh; rc=0; APT_PACKAGES="   " run prepare >/dev/null 2>&1 || rc=$?
check "refuses an empty package list (exit 2)" "$(is "$rc" 2)"
fresh; rc=0; APT_PACKAGES=jq run frobnicate >/dev/null 2>&1 || rc=$?
check "refuses an unknown subcommand (exit 2)" "$(is "$rc" 2)"
fresh; rc=0; HERMIT_APT_CACHE_ROOT=relative/dir APT_PACKAGES=jq run prepare >/dev/null 2>&1 || rc=$?
check "refuses a relative cache root (exit 2)" "$(is "$rc" 2)"
fresh; APT_PACKAGES=jq run prepare >/dev/null 2>&1; rc=0
HERMIT_APT_UPDATE_TIMEOUT_SECONDS=0 APT_PACKAGES=jq run install >/dev/null 2>&1 || rc=$?
check "refuses a zero timeout, which timeout(1) reads as no timeout (exit 2)" "$(is "$rc" 2)"
check "  ...and calls no apt-get at all" "$([[ ! -e $work/state/calls ]] && echo 0 || echo 1)"

# --- cache miss: success -----------------------------------------------------
fresh; APT_PACKAGES="jq zstd" run prepare >/dev/null 2>&1; rc=0
APT_PACKAGES="jq zstd" run install >"$work/log" 2>&1 || rc=$?
check "miss: succeeds" "$(is "$rc" 0)"
check "miss: phases run in order update, download, install" \
    "$(is "$(cut -d' ' -f1 "$work/state/calls" | tr '\n' ' ')" "update download install ")"
check "miss: download targets the cached archive directory" \
    "$(has "$work/state/calls" "^download .*Dir::Cache::Archives=$archive/")"
check "miss: the install phase never downloads" \
    "$(has "$work/state/calls" "^install install -y --no-download ")"
check "miss: reports path=mirror" "$(has "$work/log" "path=mirror rc=0")"
check "miss: seal removed apt's lock file" "$([[ ! -e $archive/lock ]] && echo 0 || echo 1)"
check "miss: seal emptied partial/" \
    "$([[ -d $archive/partial && -z $(ls -A "$archive/partial") ]] && echo 0 || echo 1)"
check "miss: the downloaded .deb files stay for the cache save" \
    "$([[ -f $archive/jq_1.0_amd64.deb && -f $archive/zstd_1.0_amd64.deb ]] && echo 0 || echo 1)"
check "miss: a retried phase is not announced when it succeeded first time" \
    "$(lacks "$work/log" "succeeded on attempt")"

# --- retries: transient failure is survived ----------------------------------
fresh; APT_PACKAGES=jq run prepare >/dev/null 2>&1; rc=0
STUB_UPDATE=fail-once:100 APT_PACKAGES=jq run install >"$work/log" 2>&1 || rc=$?
check "transient update failure: succeeds" "$(is "$rc" 0)"
check "transient update failure: update ran twice" "$(is "$(count update)" 2)"
check "transient update failure: emits a retry warning" "$(has "$work/log" "::warning title=apt-get update retried::")"
check "transient update failure: says it succeeded on attempt 2" "$(has "$work/log" "succeeded on attempt 2 of 3")"

fresh; APT_PACKAGES=jq run prepare >/dev/null 2>&1; rc=0
STUB_DOWNLOAD=fail-once:100 APT_PACKAGES=jq run install >"$work/log" 2>&1 || rc=$?
check "transient download failure: succeeds" "$(is "$rc" 0)"
check "transient download failure: download ran twice" "$(is "$(count download)" 2)"
check "transient download failure: the retry starts again from update" \
    "$(is "$(cut -d' ' -f1 "$work/state/calls" | tr '\n' ' ')" "update download update download install ")"
check "transient download failure: emits a retry warning naming the phase" "$(has "$work/log" "::warning title=apt-get download retried::attempt 1 of 3 failed (apt-get download exit 100)")"

# --- retries: persistent failure keeps its own status ------------------------
fresh; APT_PACKAGES=jq run prepare >/dev/null 2>&1; rc=0
STUB_DOWNLOAD=fail:100 APT_PACKAGES=jq run install >"$work/log" 2>&1 || rc=$?
check "persistent download failure: fails with apt's own status 100" "$(is "$rc" 100)"
check "persistent download failure: exactly HERMIT_APT_ATTEMPTS (3) attempts" "$(is "$(count update):$(count download)" "3:3")"
check "persistent download failure: never runs the install phase" "$(is "$(count install)" 0)"
check "persistent download failure: emits an error annotation" "$(has "$work/log" "::error title=apt-get download failed::attempt 3 of 3 failed (apt-get download exit 100)")"
check "persistent download failure: reports the failing rc" "$(has "$work/log" "path=mirror rc=100")"

fresh; APT_PACKAGES=jq run prepare >/dev/null 2>&1; rc=0
HERMIT_APT_ATTEMPTS=2 STUB_UPDATE=fail:7 APT_PACKAGES=jq run install >"$work/log" 2>&1 || rc=$?
check "persistent update failure: fails with its own status 7" "$(is "$rc" 7)"
check "persistent update failure: honours HERMIT_APT_ATTEMPTS=2" "$(is "$(count update)" 2)"
check "persistent update failure: never downloads" "$(is "$(count download)" 0)"
check "persistent update failure: the error names the update phase" "$(has "$work/log" "::error title=apt-get update failed::attempt 2 of 2 failed (apt-get update exit 7)")"

# --- timeout: a hung attempt is ended by the real timeout(1) -----------------
fresh; APT_PACKAGES=jq run prepare >/dev/null 2>&1; rc=0
t0=$SECONDS
HERMIT_APT_ATTEMPTS=2 HERMIT_APT_DOWNLOAD_TIMEOUT_SECONDS=1 HERMIT_APT_KILL_AFTER_SECONDS=1 \
    STUB_DOWNLOAD=hang APT_PACKAGES=jq run install >"$work/log" 2>&1 || rc=$?
elapsed=$(( SECONDS - t0 ))
check "hung download: fails with timeout's status 124" "$(is "$rc" 124)"
check "hung download: both attempts ran" "$(is "$(count download)" 2)"
check "hung download: bounded (took ${elapsed}s against a 60s hang)" "$([[ $elapsed -lt 20 ]] && echo 0 || echo 1)"
check "hung download: the annotation names the timeout" "$(has "$work/log" "ended by the 1s per-attempt timeout")"

# --- the install phase is never retried --------------------------------------
fresh; APT_PACKAGES=jq run prepare >/dev/null 2>&1; rc=0
STUB_INSTALL=fail-once:100 APT_PACKAGES=jq run install >"$work/log" 2>&1 || rc=$?
check "install failure: fails even though a retry would have succeeded" "$(is "$rc" 100)"
check "install failure: install ran exactly once" "$(is "$(count install)" 1)"
check "install failure: emits an error annotation" "$(has "$work/log" "::error title=apt-get install failed::")"
check "install failure: archive is not sealed for saving" "$([[ -e $archive/lock ]] && echo 0 || echo 1)"

# --- cache hit ---------------------------------------------------------------
# seed: one successful miss leaves the archive a restore would provide.
seed() {
    fresh; APT_PACKAGES="jq zstd" run prepare >/dev/null 2>&1
    APT_PACKAGES="jq zstd" run install >/dev/null 2>&1
    rm -f "$work/state/calls" "$work/state/count."* "$work/state/installed"
}

seed; rc=0
APT_CACHE_HIT=true APT_PACKAGES="jq zstd" run install >"$work/log" 2>&1 || rc=$?
check "hit: succeeds" "$(is "$rc" 0)"
check "hit: contacts no mirror (no update, no download)" \
    "$(is "$(count update)+$(count download)" "0+0")"
check "hit: installs the restored files once" "$(is "$(count cachefile)" 1)"
check "hit: installs every restored file by path" \
    "$(has "$work/state/calls" "^cachefile install -y --no-download -o Dir::Cache::Archives=$archive/ $archive/jq_1.0_amd64.deb $archive/zstd_1.0_amd64.deb$")"
check "hit: reports path=cache" "$(has "$work/log" "path=cache rc=0")"

seed; rc=0; seeded_key=$(sed -n 's/^key=//p' "$work/out")
STUB_CACHEFILE=fail:100 APT_CACHE_HIT=true APT_PACKAGES="jq zstd" run install >"$work/log" 2>&1 || rc=$?
check "unusable hit: falls back and succeeds" "$(is "$rc" 0)"
check "unusable hit: falls back to update, download, install" \
    "$(is "$(cut -d' ' -f1 "$work/state/calls" | tr '\n' ' ')" "cachefile update download install ")"
check "unusable hit: warns that the cache was not usable" "$(has "$work/log" "::warning title=apt cache not usable::")"
check "unusable hit: the warning names the restored key" "$(hasf "$work/log" "every job on key $seeded_key will repeat")"
check "unusable hit: the warning names both remedies" \
    "$([[ $(hasf "$work/log" "gh cache delete $seeded_key") == 0 && $(hasf "$work/log" "bump KEY_VERSION in ci/apt-install-cached.sh") == 0 ]] && echo 0 || echo 1)"
check "unusable hit: reports path=mirror-after-unusable-cache" "$(has "$work/log" "path=mirror-after-unusable-cache rc=0")"

seed; rc=0
STUB_CACHEFILE_SKIP=zstd APT_CACHE_HIT=true APT_PACKAGES="jq zstd" run install >"$work/log" 2>&1 || rc=$?
check "incomplete hit: a package still missing after the cache install falls back" \
    "$(has "$work/log" "::warning title=apt cache incomplete::the restored .deb files did not install: zstd")"
check "incomplete hit: the warning names a remedy" "$(hasf "$work/log" "gh cache delete apt-debs-v1-")"
check "incomplete hit: the fallback succeeds" "$(is "$rc:$(count install)" "0:1")"

seed; rc=0
STUB_CACHEFILE=fail:100 STUB_UPDATE=fail:100 HERMIT_APT_ATTEMPTS=1 APT_CACHE_HIT=true \
    APT_PACKAGES="jq zstd" run install >"$work/log" 2>&1 || rc=$?
check "unusable hit whose fallback also fails: the step fails" "$(is "$rc" 100)"

# An empty archive under an exact key is what a list the image already fully
# provides saves. With every package installed it needs nothing at all; with
# one missing it is a broken entry that only a remedy can clear.
fresh; APT_PACKAGES=jq run prepare >/dev/null 2>&1; rc=0
echo jq >"$work/state/installed"
APT_CACHE_HIT=true APT_PACKAGES=jq run install >"$work/log" 2>&1 || rc=$?
check "empty hit, all preinstalled: succeeds without calling apt-get at all" \
    "$([[ $rc == 0 && ! -e $work/state/calls ]] && echo 0 || echo 1)"
check "empty hit, all preinstalled: reports path=cache-preinstalled" "$(has "$work/log" "path=cache-preinstalled rc=0")"

fresh; APT_PACKAGES=jq run prepare >/dev/null 2>&1; rc=0
APT_CACHE_HIT=true APT_PACKAGES=jq run install >"$work/log" 2>&1 || rc=$?
check "empty hit, package missing: takes the miss path" "$(is "$rc:$(count cachefile):$(count update):$(count install)" "0:0:1:1")"
check "empty hit, package missing: warns and names a remedy" \
    "$([[ $(has "$work/log" "::warning title=apt cache empty::") == 0 && $(hasf "$work/log" "gh cache delete apt-debs-v1-") == 0 ]] && echo 0 || echo 1)"
check "empty hit, package missing: reports path=mirror-after-unusable-cache" \
    "$(has "$work/log" "path=mirror-after-unusable-cache rc=0")"

fresh; APT_PACKAGES=jq run prepare >/dev/null 2>&1; rc=0
APT_PACKAGES=jq run install >"$work/log" 2>&1 || rc=$?
check "miss with nothing restored: no cache warning" "$(lacks "$work/log" "::warning title=apt cache")"

if (( failures )); then echo "apt-install-cached-test: $failures FAILED"; exit 1; fi
echo "apt-install-cached-test: all checks passed"
