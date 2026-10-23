#!/usr/bin/env bash
# Cached, bounded `apt-get install` for the hosted portable workflow.
#
# Called only by .github/actions/apt-install/action.yml, which every
# `Dependencies` step of .github/workflows/ci-portable.yml uses
# (https://github.com/rrnewton/hermit/issues/3487).
#
# WHY THIS EXISTS. Each hosted job used to run a bare `sudo apt-get update` and
# one `sudo apt-get install -y` of up to ~70 packages, about 203-208 MB, from
# the image's default mirror, with no cache, no timeout and no retry. The mirror
# is fast for most runners (`Fetched 203 MB in 4s`) but some individual runners
# get 68-216 kB/s from it. A slow connection that keeps delivering bytes is not
# an apt error, so nothing retried it and it ran until GitHub's job timeout
# cancelled the job before any test started: 9 jobs in 3 runs on 2026-10-01,
# for example https://github.com/rrnewton/hermit/actions/runs/36890056138.
#
# WHAT IT DOES. Two subcommands, run by the composite action around an
# actions/cache restore and save of one archive directory:
#
#   prepare  Create the runner-owned archive directory and print the cache key:
#            the runner image (ImageOS, ImageVersion), the dpkg architecture,
#            a hash of the sorted, de-duplicated package list, and a hash of
#            this script (see cache_key).
#   install  CACHE HIT: install exactly the restored .deb files with
#            `--no-download`, then confirm every requested package is
#            installed. No mirror is contacted at all, not even for
#            `apt-get update`. A hit whose archive is empty because the image
#            already had every requested package installs nothing. If the hit
#            cannot be used for any reason, warn with the remedy (see
#            unusable_hit_warning) and fall back to the miss path below rather
#            than failing the job.
#            CACHE MISS: `apt-get update`, then `apt-get install --download-only`
#            into the archive directory, each under its own per-attempt timeout.
#            If either fails, the attempt is retried from `update`, so a
#            download that failed against a stale or half-fetched index gets a
#            fresh one. Then the real install from the downloaded files with
#            `--no-download`. Finally leave the directory in a state the runner
#            user can archive (see seal_archive).
#
# ⚠️ WHAT THIS DELIBERATELY DOES NOT DO.
#   * It never puts a timeout on, or retries, the step that runs dpkg. Killing
#     dpkg mid-install leaves "dpkg was interrupted" and a half-configured
#     system, which is worse than a slow step. Only the network phases
#     (`update`, `--download-only`) are bounded and retried; the install phase
#     reads local files and runs once.
#   * It never turns a failure into success. A final failed attempt returns that
#     attempt's own exit status (124 when the timeout killed it), so the step
#     and the job still fail.
#   * It does not capture apt's output. apt's own lines stay in the log exactly
#     as before; this script only adds `apt-cache:` lines around them.
#   * It does not change what is installed on a miss: the same package names,
#     the same mirror, recommends left at the image default, as the former
#     inline `sudo apt-get install -y` did.
set -uo pipefail

# Mirror attempts INCLUDING the first, so 3 means at most two retries.
: "${HERMIT_APT_ATTEMPTS:=3}"
# Linear backoff: attempt N waits N * this many seconds before attempt N+1.
: "${HERMIT_APT_BACKOFF_SECONDS:=5}"
# Per-attempt limits. On a healthy runner `apt-get update` takes seconds and the
# largest list (~75-80 MB once the JDK left it) downloads in seconds too (the
# issue's healthy jobs show `Fetched 203 MB in 4s` for the old list), so these
# end only attempts that are far outside that range. Cutting a slow attempt
# loses little: completed .deb files persist between attempts and apt resumes
# files left in partial/, so a retry continues on a fresh connection instead of
# restarting from zero.
#
# WORST CASE on a miss whose mirror never recovers, before the install phase:
# 3 * (60 + 10 + 150 + 10) + 5 + 10 = 705 s, about 12 minutes. That is shorter
# than the 15- to 35-minute timeout-minutes of the jobs with the long lists, so
# such a runner usually ends with a labelled step failure naming the phase and
# the timeout rather than a bare job cancellation. The cache, not these
# bounds, is what removes the exposure on later runs.
: "${HERMIT_APT_UPDATE_TIMEOUT_SECONDS:=60}"
: "${HERMIT_APT_DOWNLOAD_TIMEOUT_SECONDS:=150}"
# Grace between the timeout's SIGTERM and its SIGKILL.
: "${HERMIT_APT_KILL_AFTER_SECONDS:=10}"
# Root of the cached directory. Deliberately NOT under the runner's home or the
# workspace: apt fetches as the unprivileged `_apt` user, which must be able to
# traverse every parent directory, and the workspace is wiped by checkout.
: "${HERMIT_APT_CACHE_ROOT:=/var/cache/hermit-ci-apt}"
# Bump to invalidate every saved archive at once, for example to evict a
# broken entry without deleting it by hand. Editing this script for any other
# reason (apt-get options, archive layout) already changes every key, because
# cache_key hashes the script itself.
readonly KEY_VERSION=v1

die() {
    echo "apt-install-cached: $*" >&2
    exit 2
}

# A zero timeout would mean NO timeout to timeout(1), so the limits must be >= 1.
check_numbers() {
    local var
    for var in HERMIT_APT_ATTEMPTS HERMIT_APT_UPDATE_TIMEOUT_SECONDS \
        HERMIT_APT_DOWNLOAD_TIMEOUT_SECONDS HERMIT_APT_KILL_AFTER_SECONDS; do
        [[ ${!var} =~ ^[1-9][0-9]*$ ]] || die "$var must be a positive integer, got '${!var}'"
    done
    [[ $HERMIT_APT_BACKOFF_SECONDS =~ ^[0-9]+$ ]] ||
        die "HERMIT_APT_BACKOFF_SECONDS must be a non-negative integer, got '$HERMIT_APT_BACKOFF_SECONDS'"
}

archive_dir() {
    [[ $HERMIT_APT_CACHE_ROOT == /?* && $HERMIT_APT_CACHE_ROOT != *..* ]] ||
        die "HERMIT_APT_CACHE_ROOT must be an absolute path without '..', got '$HERMIT_APT_CACHE_ROOT'"
    printf '%s/archives\n' "${HERMIT_APT_CACHE_ROOT%/}"
}

# Read APT_PACKAGES (whitespace separated) into the global array PACKAGES,
# sorted and de-duplicated. Every element must be a plain Debian package name:
# no version pins, no options, nothing apt could read as a flag.
read_packages() {
    local name
    PACKAGES=()
    while IFS= read -r name; do
        [[ -n $name ]] || continue
        [[ $name =~ ^[a-z0-9][a-z0-9+.-]+$ ]] ||
            die "'$name' is not a plain Debian package name"
        PACKAGES+=("$name")
    done < <(printf '%s\n' "${APT_PACKAGES:-}" | tr -s '[:space:]' '\n' | LC_ALL=C sort -u)
    (( ${#PACKAGES[@]} > 0 )) || die "APT_PACKAGES is empty"
}

# The key names everything that decides which .deb files a miss downloads: the
# runner image, the architecture, the package list, and this script, whose
# apt-get options and archive layout shape the saved closure. Hashing the script
# means an edit to those options starts fresh entries instead of letting hit
# jobs install a closure built under the old options while miss jobs build a
# different one.
cache_key() {
    local image_os=${ImageOS:-} image_version=${ImageVersion:-unknown} arch list_hash script_hash
    if [[ -z $image_os ]]; then
        # GitHub's hosted images export ImageOS and ImageVersion. Elsewhere
        # (the self-test) fall back to os-release so the key still names an OS.
        # shellcheck source=/dev/null
        image_os=$(. /etc/os-release 2>/dev/null && printf '%s%s' "${ID:-unknown}" "${VERSION_ID:-}")
        [[ -n $image_os ]] || image_os=unknown
    fi
    arch=$(dpkg --print-architecture 2>/dev/null) || arch=$(uname -m)
    list_hash=$(printf '%s\n' "${PACKAGES[@]}" | sha256sum | cut -c1-16) || return 1
    script_hash=$(sha256sum <"${BASH_SOURCE[0]}" | cut -c1-8) || return 1
    [[ $script_hash =~ ^[0-9a-f]{8}$ ]] || return 1
    printf 'apt-debs-%s-%s-%s-%s-%s-s%s\n' "$KEY_VERSION" "$image_os" "$image_version" "$arch" \
        "$list_hash" "$script_hash"
}

emit_output() {
    if [[ -n ${GITHUB_OUTPUT:-} ]]; then
        printf '%s=%s\n' "$1" "$2" >>"$GITHUB_OUTPUT"
    fi
    echo "apt-cache: $1=$2"
}

prepare_main() {
    local dir key
    read_packages
    dir=$(archive_dir) || exit 2
    # Runner-owned so actions/cache (which runs as the runner user) can extract
    # into it on restore and archive it on save.
    sudo install -d -m 0755 -o "$(id -u)" -g "$(id -g)" \
        "${HERMIT_APT_CACHE_ROOT%/}" "$dir" "$dir/partial" || return
    key=$(cache_key) || return
    echo "apt-cache: ${#PACKAGES[@]} requested packages: ${PACKAGES[*]}"
    emit_output key "$key"
    emit_output dir "$dir"
}

# deb_stats <dir> -> "<count> <bytes>"
deb_stats() {
    find "$1" -maxdepth 1 -type f -name '*.deb' -printf '%s\n' |
        awk '{ n += 1; b += $1 } END { printf "%d %d\n", n, b }'
}

mib() {
    awk -v b="$1" 'BEGIN { printf "%.1f", b / 1048576 }'
}

# apt_bounded <label> <per-attempt seconds> <apt-get args...>
#
# One attempt under the per-attempt timeout. Returns apt-get's OWN exit status,
# never a synthetic one; 124 (or 137 after SIGKILL) means the timeout ended it.
# On failure it leaves a description in FAILED_LABEL and FAILED_WHY.
apt_bounded() {
    local label=$1 limit=$2 rc=0
    shift 2
    # `sudo timeout`, not `timeout sudo`: timeout must run as root to be
    # able to signal apt-get and the `_apt` fetch helpers it starts.
    # Without --foreground, timeout signals its whole process group.
    # NOT `if cmd; then ...; fi; rc=$?`: after an `if` with no else, `$?` is
    # 0 when the condition failed. ci/hermetic/retry-fetch.sh documents the
    # same trap; keep the `||` form.
    sudo timeout --kill-after="$HERMIT_APT_KILL_AFTER_SECONDS" "$limit" apt-get "$@" || rc=$?
    if (( rc != 0 )); then
        FAILED_LABEL=$label
        FAILED_WHY="apt-get $label exit $rc"
        if (( rc == 124 || rc == 137 )); then
            FAILED_WHY+=", ended by the ${limit}s per-attempt timeout"
        fi
    fi
    return "$rc"
}

# Missing requested packages, one per line; empty when all are installed.
missing_packages() {
    local name status
    for name in "${PACKAGES[@]}"; do
        status=$(dpkg-query -W -f='${Status}' "$name" 2>/dev/null) || status=
        [[ $status == "install ok installed" ]] || printf '%s\n' "$name"
    done
}

# unusable_hit_warning <title> <what went wrong>
#
# An exact hit that cannot be used is never replaced by itself: the save step
# skips hits, and an entry is immutable for its key and branch scope. Every
# later job on the key would repeat the fallback, and pay the mirror again,
# until the runner image rotates. So the warning names the key and the two ways
# to clear it.
unusable_hit_warning() {
    local key
    key=$(cache_key) || key='(key unavailable)'
    echo "::warning title=$1::$2; falling back to the mirror. Cache entries are immutable, so every job on key $key will repeat this fallback until the key changes: delete the entry (gh cache delete $key${GITHUB_REPOSITORY:+ --repo $GITHUB_REPOSITORY}) or bump KEY_VERSION in ci/apt-install-cached.sh."
}

# Cache hit: install exactly the restored files. Never contacts a mirror.
install_from_archive() {
    local dir=$1 rc missing
    local -a debs=()
    mapfile -t debs < <(find "$dir" -maxdepth 1 -type f -name '*.deb' | LC_ALL=C sort)
    (( ${#debs[@]} > 0 )) || return 1
    rc=0
    sudo apt-get install -y --no-download -o "Dir::Cache::Archives=$dir/" "${debs[@]}" || rc=$?
    if (( rc != 0 )); then
        unusable_hit_warning "apt cache not usable" \
            "installing the ${#debs[@]} restored .deb files failed (exit $rc; apt's output is above)"
        return "$rc"
    fi
    missing=$(missing_packages)
    if [[ -n $missing ]]; then
        unusable_hit_warning "apt cache incomplete" \
            "the restored .deb files did not install: $(tr '\n' ' ' <<<"$missing")"
        return 1
    fi
}

# Cache miss (or unusable hit): bounded, retried network phases, then one
# unbounded install from the downloaded files.
install_from_mirror() {
    local dir=$1 t0 rc attempt=1 sleep_for
    while :; do
        rc=0
        t0=$SECONDS
        apt_bounded update "$HERMIT_APT_UPDATE_TIMEOUT_SECONDS" update || rc=$?
        UPDATE_SECONDS=$(( UPDATE_SECONDS + SECONDS - t0 ))
        if (( rc == 0 )); then
            t0=$SECONDS
            apt_bounded download "$HERMIT_APT_DOWNLOAD_TIMEOUT_SECONDS" \
                install -y --download-only -o "Dir::Cache::Archives=$dir/" "${PACKAGES[@]}" || rc=$?
            DOWNLOAD_SECONDS=$(( DOWNLOAD_SECONDS + SECONDS - t0 ))
        fi
        if (( rc == 0 )); then
            if (( attempt > 1 )); then
                echo "apt-cache: mirror update and download succeeded on attempt $attempt of $HERMIT_APT_ATTEMPTS"
            fi
            break
        fi
        if (( attempt >= HERMIT_APT_ATTEMPTS )); then
            echo "::error title=apt-get $FAILED_LABEL failed::attempt $attempt of $HERMIT_APT_ATTEMPTS failed ($FAILED_WHY). apt's own output above is the cause; this line only says the retry bound was reached."
            return "$rc"
        fi
        sleep_for=$(( HERMIT_APT_BACKOFF_SECONDS * attempt ))
        echo "::warning title=apt-get $FAILED_LABEL retried::attempt $attempt of $HERMIT_APT_ATTEMPTS failed ($FAILED_WHY); retrying from apt-get update in ${sleep_for}s. apt's output is above."
        sleep "$sleep_for"
        attempt=$(( attempt + 1 ))
    done
    t0=$SECONDS
    rc=0
    sudo apt-get install -y --no-download -o "Dir::Cache::Archives=$dir/" "${PACKAGES[@]}" || rc=$?
    INSTALL_SECONDS=$(( SECONDS - t0 ))
    if (( rc != 0 )); then
        echo "::error title=apt-get install failed::installing from the downloaded files failed (exit $rc). apt's output above is the cause."
    fi
    return "$rc"
}

# As root, apt leaves a root-owned `lock` file in the archive directory and
# chowns partial/ to `_apt` with mode 0700. actions/cache archives the
# directory as the runner user, so both would make the save fail or be partial.
# Drop the lock, discard any incomplete download, and give the tree back to the
# runner user. The path is the fixed one archive_dir() computed, never input.
seal_archive() {
    local dir=$1
    sudo rm -f "$dir/lock" || return
    if [[ -d $dir/partial ]]; then
        sudo find "$dir/partial" -mindepth 1 -delete || return
    fi
    sudo chown -R "$(id -u):$(id -g)" "$dir"
}

install_main() {
    local dir started t0 rc=0 path missing before_n before_b after_n after_b
    check_numbers
    read_packages
    dir=$(archive_dir) || exit 2
    [[ -d $dir ]] || die "$dir does not exist; run '$0 prepare' first"
    started=$SECONDS
    UPDATE_SECONDS=0 DOWNLOAD_SECONDS=0 INSTALL_SECONDS=0
    read -r before_n before_b < <(deb_stats "$dir")
    echo "apt-cache: restored $before_n .deb files, $(mib "$before_b") MiB (cache-hit=${APT_CACHE_HIT:-false})"

    if [[ ${APT_CACHE_HIT:-} == true ]] && (( before_n > 0 )); then
        t0=$SECONDS
        if install_from_archive "$dir"; then
            path=cache
            INSTALL_SECONDS=$(( SECONDS - t0 ))
        else
            path=mirror-after-unusable-cache
            install_from_mirror "$dir" || rc=$?
        fi
    elif [[ ${APT_CACHE_HIT:-} == true ]]; then
        # An empty archive under an exact key: the job that saved it downloaded
        # nothing, which on the same image means every requested package was
        # already installed at its candidate version. When that holds here too
        # (checked, not assumed) there is nothing to install and no reason to
        # contact the mirror. When it does not, the entry is broken in a way
        # the save step will never repair, so say so and take the miss path.
        missing=$(missing_packages)
        if [[ -z $missing ]]; then
            path=cache-preinstalled
        else
            unusable_hit_warning "apt cache empty" \
                "the restored archive has no .deb files but these packages are not installed: $(tr '\n' ' ' <<<"$missing")"
            path=mirror-after-unusable-cache
            install_from_mirror "$dir" || rc=$?
        fi
    else
        path=mirror
        install_from_mirror "$dir" || rc=$?
    fi

    if (( rc == 0 )); then
        seal_archive "$dir" || rc=$?
    fi
    read -r after_n after_b < <(deb_stats "$dir")
    echo "apt-cache: path=$path rc=$rc downloaded $(( after_n - before_n )) .deb files, $(mib "$(( after_b - before_b ))") MiB"
    echo "apt-cache: seconds update=$UPDATE_SECONDS download=$DOWNLOAD_SECONDS install=$INSTALL_SECONDS total=$(( SECONDS - started ))"
    return "$rc"
}

case "${1:-}" in
    prepare) prepare_main ;;
    install) install_main ;;
    *) die "usage: $0 prepare|install (packages in APT_PACKAGES)" ;;
esac
