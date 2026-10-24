# shellcheck shell=bash
#
# Collect every missing demo prerequisite and report them together.
#
# Each check records a miss instead of exiting, so `preflight_report` can name
# the complete set with a remedy for each item. A blocked download counts as a
# missing prerequisite just like an absent command: the QEMU demos need a
# kernel image that some networks cannot fetch.
#
# Usage:
#   source "$DEMOS_DIR/lib/preflight.sh"
#   preflight_require_command qemu-system-x86_64 "install QEMU"
#   preflight_require_file "$KERNEL" "set KERNEL_IMAGE=/path/to/bzImage"
#   preflight_require_url "$KERNEL_URL" "set KERNEL_IMAGE to a local copy"
#   preflight_report            # prints every miss, exits 1 if any

_PREFLIGHT_MISSING=()

_preflight_record() {
    _PREFLIGHT_MISSING+=("$1"$'\t'"$2"$'\t'"$3")
}

preflight_require_command() {
    local name=$1 remedy=${2:-"install $1"}
    command -v "$name" >/dev/null 2>&1 || _preflight_record "command" "$name" "$remedy"
}

preflight_require_file() {
    local path=$1 remedy=${2:-"create or point the demo at $1"}
    [ -f "$path" ] || _preflight_record "file" "$path" "$remedy"
}

preflight_require_executable() {
    local path=$1 remedy=${2:-"build or install $1"}
    [ -x "$path" ] || _preflight_record "executable" "$path" "$remedy"
}

# A download the network may refuse, checked with a HEAD request. An optional
# `with-proxy` helper on PATH is used when present, for networks that route
# outbound traffic through one. Skipped when curl itself is missing, because
# curl is then already reported as a missing command.
preflight_require_url() {
    local url=$1 remedy=${2:-"make $1 reachable, or set the corresponding *_IMAGE/*_PATH override to a local copy"}
    command -v curl >/dev/null 2>&1 || return 0
    local curl_cmd=(curl --fail --location --silent --show-error --head --max-time 20)
    if command -v with-proxy >/dev/null 2>&1; then
        curl_cmd=(with-proxy "${curl_cmd[@]}")
    fi
    "${curl_cmd[@]}" "$url" >/dev/null 2>&1 ||
        _preflight_record "download" "$url" "$remedy"
}

preflight_missing_count() { printf '%s\n' "${#_PREFLIGHT_MISSING[@]}"; }

# Print the complete set, then exit non-zero.
preflight_report() {
    local label=${1:-demo}
    if [ "${#_PREFLIGHT_MISSING[@]}" -eq 0 ]; then
        return 0
    fi
    {
        printf '\n=== %s: %d missing prerequisite(s) ===\n' \
            "$label" "${#_PREFLIGHT_MISSING[@]}"
        printf 'All of the following are missing (this is the complete list):\n\n'
        local entry kind what remedy
        for entry in "${_PREFLIGHT_MISSING[@]}"; do
            IFS=$'\t' read -r kind what remedy <<<"$entry"
            printf '  [%s] %s\n      -> %s\n' "$kind" "$what" "$remedy"
        done
        printf '\nFix all of them, then re-run. Nothing above was attempted.\n'
    } >&2
    exit 1
}
