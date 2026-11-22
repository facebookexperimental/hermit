#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# Tests for ci/hermetic/cargo-proxy.sh, the fetch phase's Cargo proxy default.
# Sources THAT file, not a copy, so a change to the helper is a change here.
#
#   ci/hermetic/tests/test-cargo-proxy.sh             offline checks (make lint-check-cargo-proxy)
#   ci/hermetic/tests/test-cargo-proxy.sh --network   also the networked red control
#
# The networked control needs crates.io through an https proxy in the
# environment (on Meta hosts: with-proxy ci/hermetic/tests/test-cargo-proxy.sh
# --network). In a throwaway HOME whose Git config names an unreachable
# http.proxy, with a cold CARGO_HOME, it shows that:
#   1. a plain `cargo fetch` (what the fetch phase did before this helper,
#      whenever its nightly-only config check could not run) fails, because
#      Cargo prefers Git's http.proxy to the environment's;
#   2. the same fetch after cargo_proxy_default succeeds;
#   3. a Cargo config that names a proxy is left alone, so that proxy (here the
#      same unreachable one) is the one Cargo uses, and the fetch fails.
set -uo pipefail

HERE=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=ci/hermetic/cargo-proxy.sh
source "$HERE/../cargo-proxy.sh"

network=0
case ${1:-} in
    "") ;;
    --network) network=1 ;;
    -h | --help) awk '!body && /^$/ { body = 1; next } body && !/^#/ { exit } body { sub(/^# ?/, ""); print }' "${BASH_SOURCE[0]}"; exit 0 ;;
    *) echo "test-cargo-proxy: unknown argument $1 (try --help)" >&2; exit 2 ;;
esac

failures=0
pass() { echo "ok   - $1"; }
fail() { echo "FAIL - $1"; failures=$((failures + 1)); }
skip() { echo "skip - $1"; }

work=
rc=
for sig in 1 2 3 13 15; do
    # shellcheck disable=SC2064
    trap "exit $((sig + 128))" "$sig"
done
trap 'rc=$?; set +e; [[ -z $work ]] || rm -rf -- "$work"; exit $rc' EXIT
work=$(mktemp -d "${TMPDIR:-/tmp}/test-cargo-proxy.XXXXXX") || exit 1

# decide DIR ENV...: run cargo_proxy_default for DIR in a clean child started
# in DIR (when it exists) with only ENV (plus PATH and HOME=$work/homedir), as
# host_fetch runs it, recording its status, stderr and the CARGO_HTTP_PROXY it
# leaves ("unset" when it leaves none).
decide() {
    local dir=$1
    shift
    # shellcheck disable=SC2016 # expanded by the child shell
    env -i PATH="$PATH" HOME="$work/homedir" "$@" bash -c '
        source "$1"
        if [[ -d $2 ]]; then cd "$2" || exit 1; fi
        cargo_proxy_default "$2" 2>"$3/stderr"
        status=$?
        printf "%s\n%s\n" "$status" "${CARGO_HTTP_PROXY-unset}" >"$3/result"
    ' decide "$HERE/../cargo-proxy.sh" "$dir" "$work"
    { read -r d_status; read -r d_proxy; } <"$work/result"
    d_stderr=$(cat "$work/stderr")
}

# expect NAME STATUS PROXY STDERR-SUBSTRING
expect() {
    if [[ $d_status == "$2" && $d_proxy == "$3" && $d_stderr == *"$4"* ]]; then
        pass "$1"
    else
        fail "$1: status $d_status (want $2), CARGO_HTTP_PROXY '$d_proxy' (want '$3'), stderr: $d_stderr"
    fi
}

# fresh: a new query directory and an empty CARGO_HOME, with no config files.
fresh() {
    rm -rf -- "${work:?}/tree" "${work:?}/homedir"
    mkdir -p "$work/tree/repo/sub" "$work/tree/cargo-home" "$work/homedir"
    q=$work/tree/repo/sub
    ch=$work/tree/cargo-home
}

ENV_PROXY=http://environment.invalid:8080

# --- the environment ----------------------------------------------------------
fresh
decide "$q" CARGO_HOME="$ch" CARGO_HTTP_PROXY=http://explicit.invalid https_proxy=$ENV_PROXY
expect "an already-set CARGO_HTTP_PROXY wins" 0 http://explicit.invalid "already set"
decide "$q" CARGO_HOME="$ch" CARGO_HTTP_PROXY= https_proxy=$ENV_PROXY
expect "an already-set empty CARGO_HTTP_PROXY wins" 0 "" "already set"
decide "$q" CARGO_HOME="$ch"
expect "no https proxy in the environment: nothing applied" 0 unset "no https_proxy"
decide "$q" CARGO_HOME="$ch" http_proxy=$ENV_PROXY HTTP_PROXY=$ENV_PROXY
expect "an HTTP-only proxy stays HTTP-only" 0 unset "no https_proxy"
decide "$q" CARGO_HOME="$ch" https_proxy=$ENV_PROXY
expect "cold tree, no Cargo setting: exported from https_proxy" 0 "$ENV_PROXY" "exported CARGO_HTTP_PROXY"
decide "$q" CARGO_HOME="$ch" HTTPS_PROXY=http://upper.invalid
expect "HTTPS_PROXY is used when https_proxy is unset" 0 http://upper.invalid "exported"
decide "$q" CARGO_HOME="$ch" https_proxy=http://lower.invalid HTTPS_PROXY=http://upper.invalid
expect "https_proxy wins over HTTPS_PROXY" 0 http://lower.invalid "exported"
decide "$q" CARGO_HOME="$ch" https_proxy= HTTPS_PROXY=http://upper.invalid
expect "an empty https_proxy falls through to HTTPS_PROXY" 0 http://upper.invalid "exported"

# --- Cargo config files that set http.proxy -----------------------------------
# where FILE-RELATIVE-TO-$work/tree, then the content.
setter_case() {
    local name=$1 where=$2 content=$3 file
    fresh
    file=$work/tree/$where
    mkdir -p "$(dirname -- "$file")"
    printf '%s\n' "$content" >"$file"
    decide "$q" CARGO_HOME="$ch" https_proxy=$ENV_PROXY
    expect "$name" 0 unset "$file sets Cargo's http.proxy; applying nothing"
}
setter_case "[http] table, proxy key" repo/sub/.cargo/config.toml $'[build]\njobs = 2\n\n[http]\ntimeout = 30\nproxy = "http://configured.invalid"'
setter_case "[http] table with spaces, quotes and a comment" repo/sub/.cargo/config.toml $'[ "http" ] # proxy settings\n  "proxy"   =   \'http://configured.invalid\''
setter_case "dotted http.proxy key at the top level" repo/sub/.cargo/config.toml 'http.proxy = "http://configured.invalid"'
setter_case "dotted key with spaces and quotes" repo/sub/.cargo/config.toml '"http" . "proxy"="http://configured.invalid"'
setter_case "inline http table" repo/sub/.cargo/config.toml 'http = { timeout = 30, proxy = "http://configured.invalid" }'
setter_case "an empty proxy value is Cargo's own choice too" repo/sub/.cargo/config.toml $'[http]\nproxy = ""'
setter_case "legacy extensionless .cargo/config" repo/sub/.cargo/config $'[http]\nproxy = "http://configured.invalid"'
setter_case "a parent directory's .cargo/config.toml" repo/.cargo/config.toml 'http.proxy = "http://configured.invalid"'
setter_case "CARGO_HOME/config.toml" cargo-home/config.toml $'[http]\nproxy = "http://configured.invalid"'
setter_case "CARGO_HOME/config (extensionless)" cargo-home/config 'http.proxy = "http://configured.invalid"'

fresh
mkdir -p "$work/homedir/.cargo"
printf 'http.proxy = "http://configured.invalid"\n' >"$work/homedir/.cargo/config.toml"
decide "$q" https_proxy=$ENV_PROXY
expect "CARGO_HOME unset: ~/.cargo/config.toml is read" 0 unset "$work/homedir/.cargo/config.toml sets"

fresh
printf 'http.proxy = "http://configured.invalid"\n' >"$work/tree/cargo-home/config.toml"
decide "$q" CARGO_HOME=../../cargo-home https_proxy=$ENV_PROXY
expect "a relative CARGO_HOME is taken from the current directory" 0 unset "sets Cargo's http.proxy"

# --- forms a line-by-line scan would misread, each checked against Cargo 1.97.1
setter_case "a leading byte-order mark before [http]" repo/sub/.cargo/config.toml $'\xEF\xBB\xBF[http]\nproxy = "http://configured.invalid"'
setter_case "a multi-line inline http table" repo/sub/.cargo/config.toml $'http = {\n  timeout = 30,\n  proxy = "http://configured.invalid",\n}'
setter_case "a multi-line inline http table with a nested table" repo/sub/.cargo/config.toml $'http = {\n  ssl-version = { min = "tlsv1.2" },\n  proxy = "http://configured.invalid",\n}'
setter_case "a multi-line string holding a [header] line, then proxy" repo/sub/.cargo/config.toml $'[http]\nuser-agent = """\n[net]\n"""\nproxy = "http://configured.invalid"'
setter_case "a multi-line literal string holding a [header] line, then proxy" repo/sub/.cargo/config.toml $'[http]\nuser-agent = \'\'\'\n[net]\n\'\'\'\nproxy = \'http://configured.invalid\''
setter_case "a top-level multi-line string, then a dotted http.proxy" repo/sub/.cargo/config.toml $'http.user-agent = """\n[x]\n"""\nhttp.proxy = "http://configured.invalid"'
setter_case "a multi-line string with escaped quotes, then proxy" repo/sub/.cargo/config.toml $'[http]\nuser-agent = """a\\"""\n[net]\n"""\nproxy = "http://configured.invalid"'
setter_case "a multi-line array whose element line starts with [, then proxy" repo/sub/.cargo/config.toml $'[http]\nx = [\n  [1],\n]\nproxy = "http://configured.invalid"'
setter_case "CRLF line endings with a blank line" repo/sub/.cargo/config.toml $'[http]\r\n\r\nproxy = "http://configured.invalid"\r'
setter_case "a four-quote closer and a quote-bracket comment around [http] (seat A1)" repo/sub/.cargo/config.toml $'a = """x"""" # " [\n[http]\nproxy = "http://configured.invalid"\nb = """y"""" # " ]'
setter_case "a four-apostrophe closer and an apostrophe-bracket comment around [http] (seat A2)" repo/sub/.cargo/config.toml $'a = \'\'\'x\'\'\'\' # \' [\n[http]\nproxy = \'http://configured.invalid\'\nb = \'\'\'y\'\'\'\' # \' ]'
setter_case "a four-quote closer and a comment holding a triple quote (seat A3)" repo/sub/.cargo/config.toml $'a = """x"""" # " """\n[http]\nproxy = "http://configured.invalid"\nb = 1 # """'
setter_case "a four-quote closer, then proxy (seat A4)" repo/sub/.cargo/config.toml $'[http]\nuser-agent = """x""""\nproxy = "http://configured.invalid"'
setter_case "a five-quote closer, then proxy (seat A5)" repo/sub/.cargo/config.toml $'[http]\nuser-agent = """x"""""\nproxy = "http://configured.invalid"'
setter_case "a four-quote closer and a comment with one quote (seat A6)" repo/sub/.cargo/config.toml $'[http]\nuser-agent = """x"""" # say "\nproxy = "http://configured.invalid"'

# --- constructs the check does not interpret: name the file, apply nothing ----
# uncertain_case NAME CONTENT WHAT: the query directory's config.toml has CONTENT.
uncertain_case() {
    local name=$1 content=$2 what=$3
    fresh
    mkdir -p "$q/.cargo"
    printf '%s\n' "$content" >"$q/.cargo/config.toml"
    printf '[http]\nproxy = "http://configured.invalid"\n' >"$q/.cargo/proxy.toml"
    decide "$q" CARGO_HOME="$ch" https_proxy=$ENV_PROXY
    expect "$name" 0 unset "$q/.cargo/config.toml may set Cargo's http.proxy: it has $what, which this check does not interpret; applying nothing"
}
uncertain_case "an include list (the included file sets proxy)" 'include = ["proxy.toml"]' "an include key"
uncertain_case "an include list of tables" 'include = [{ path = "proxy.toml" }]' "an include key"
uncertain_case "an [[include]] table" $'[[include]]\npath = "proxy.toml"' "an include table"
uncertain_case "an escape sequence in a quoted key" $'[http]\n"\\u0070roxy" = "http://configured.invalid"' "an escape sequence in a quoted key"
uncertain_case "an escape sequence in an inline http table" 'http = { "\u0070roxy" = "http://configured.invalid" }' "an escape sequence in an inline http table"
uncertain_case "a value that never closes" $'[build]\nrustflags = [' "a value that never closes"
uncertain_case "a line that is not TOML" $'[http]\nproxy' "a line it cannot parse"
uncertain_case "a stray closing bracket" $'[build]\njobs = ]' "a value it cannot parse"

# --- near misses, which must not stop the default -----------------------------
miss_case() {
    local name=$1 content=$2
    fresh
    mkdir -p "$q/.cargo"
    printf '%s\n' "$content" >"$q/.cargo/config.toml"
    decide "$q" CARGO_HOME="$ch" https_proxy=$ENV_PROXY
    expect "$name" 0 "$ENV_PROXY" "exported"
}
miss_case "a commented-out proxy" $'[http]\n# proxy = "http://configured.invalid"'
miss_case "proxy under another table" $'[target.http]\nproxy = "x"\n[profile.http]\nproxy = "x"'
miss_case "a longer key" $'[http]\nproxy-user = "x"\nproxyx = "x"\nhttp.proxyx = "x"'
miss_case "http.proxy after the [http] table ended" $'[http]\ntimeout = 3\n[net]\nproxy = "x"'
miss_case "an [[array]] table named http2" $'[[http2]]\nproxy = "x"'
miss_case "a quoted key that only looks dotted" '"http.proxy" = "x"'
miss_case "a proxy line inside a multi-line string" $'[http]\nuser-agent = """\nproxy = "x"\n"""'
miss_case "a multi-line inline http table without proxy" $'http = {\n  timeout = 30,\n}'
miss_case "a multi-line array, then another table" $'[build]\nrustflags = [\n  "-C", "link-arg=-s",\n]\n[net]\nretry = 2'
miss_case "a byte-order mark before another table" $'\xEF\xBB\xBF[build]\njobs = 2'
miss_case "CRLF line endings with a blank line" $'[build]\r\njobs = 2\r\n\r\n[net]\r\nretry = 2\r'

fresh
mkdir -p "$q/deeper/.cargo"
printf 'http.proxy = "x"\n' >"$q/deeper/.cargo/config.toml"
decide "$q" CARGO_HOME="$ch" https_proxy=$ENV_PROXY
expect "a config below the query directory is not Cargo's" 0 "$ENV_PROXY" "exported"

# --- Cargo's file choice in one directory: `config` when it opens ------------
fresh
mkdir -p "$q/.cargo"
printf '[build]\njobs = 2\n' >"$q/.cargo/config"
printf '[http]\nproxy = "x"\n' >"$q/.cargo/config.toml"
decide "$q" CARGO_HOME="$ch" https_proxy=$ENV_PROXY
expect "config and config.toml: Cargo reads only config, so config.toml's proxy is not Cargo's" 0 "$ENV_PROXY" "exported"
fresh
printf '[build]\njobs = 2\n' >"$ch/config"
printf '[http]\nproxy = "x"\n' >"$ch/config.toml"
decide "$q" CARGO_HOME="$ch" https_proxy=$ENV_PROXY
expect "the same choice in CARGO_HOME" 0 "$ENV_PROXY" "exported"
fresh
mkdir -p "$q/.cargo"
printf '[http]\nproxy = "x"\n' >"$q/.cargo/config"
printf '[build]\njobs = 2\n' >"$q/.cargo/config.toml"
decide "$q" CARGO_HOME="$ch" https_proxy=$ENV_PROXY
expect "config and config.toml: config's proxy is named" 0 unset "$q/.cargo/config sets Cargo's http.proxy"
if ((EUID == 0)); then
    skip "unreadable-file cases (root reads mode-000 files)"
else
    fresh
    mkdir -p "$q/.cargo"
    printf '[build]\n' >"$q/.cargo/config"
    chmod 000 "$q/.cargo/config"
    printf '[http]\nproxy = "x"\n' >"$q/.cargo/config.toml"
    decide "$q" CARGO_HOME="$ch" https_proxy=$ENV_PROXY
    expect "an unreadable config: Cargo reads config.toml, whose proxy is named" 0 unset "$q/.cargo/config.toml sets Cargo's http.proxy"
    fresh
    mkdir -p "$q/.cargo"
    printf '[http]\nproxy = "x"\n' >"$q/.cargo/config"
    chmod 000 "$q/.cargo/config"
    decide "$q" CARGO_HOME="$ch" https_proxy=$ENV_PROXY
    expect "an unreadable config alone: Cargo skips it" 0 "$ENV_PROXY" "exported"
    fresh
    mkdir -p "$q/.cargo"
    printf '[build]\n' >"$q/.cargo/config"
    printf '[http]\n' >"$q/.cargo/config.toml"
    chmod 000 "$q/.cargo/config.toml"
    decide "$q" CARGO_HOME="$ch" https_proxy=$ENV_PROXY
    expect "a readable config beside an unreadable config.toml: Cargo reads config" 0 "$ENV_PROXY" "exported"
fi

# --- unknown answers refuse ----------------------------------------------------
fresh
mkdir -p "$q/.cargo/config.toml"
decide "$q" CARGO_HOME="$ch" https_proxy=$ENV_PROXY
expect "a config path that is a directory refuses" 2 unset "cannot read $q/.cargo/config.toml"
fresh
decide "$work/tree/no-such-dir" CARGO_HOME="$ch" https_proxy=$ENV_PROXY
expect "a query directory that cannot be entered refuses" 2 unset "cannot read $work/tree/no-such-dir"
fresh
mkdir -p "$q/.cargo/config"
decide "$q" CARGO_HOME="$ch" https_proxy=$ENV_PROXY
expect "a config path that is a directory refuses even beside config.toml" 2 unset "cannot read $q/.cargo/config,"
if ((EUID == 0)); then
    skip "an unreadable regular config.toml (root reads mode-000 files)"
else
    fresh
    mkdir -p "$q/.cargo"
    printf '[build]\n' >"$q/.cargo/config.toml"
    chmod 000 "$q/.cargo/config.toml"
    decide "$q" CARGO_HOME="$ch" https_proxy=$ENV_PROXY
    expect "an unreadable regular config.toml refuses" 2 unset "cannot read $q/.cargo/config.toml"
fi

# A lexer that fails must refuse, never read as "no proxy": an awk that exits
# 2 (as on a fatal error), one that exits 1, one that exits 0 having printed no
# verdict, and one that prints "unset" but exits 2, each on a config that does
# set a proxy.
for stub in 'exit 2' 'exit 1' 'exit 0' 'echo unset; exit 2'; do
    fresh
    mkdir -p "$q/.cargo" "$work/tree/stub"
    printf '[http]\nproxy = "x"\n' >"$q/.cargo/config.toml"
    printf '#!/bin/sh\necho "stub awk: failing" >&2\n%s\n' "$stub" >"$work/tree/stub/awk"
    chmod +x "$work/tree/stub/awk"
    decide "$q" PATH="$work/tree/stub:$PATH" CARGO_HOME="$ch" https_proxy=$ENV_PROXY
    expect "a lexer that fails ($stub) refuses" 2 unset "cannot read $q/.cargo/config.toml"
done

# --- the networked red control -------------------------------------------------
if ((network)); then
    proxy=${https_proxy:-${HTTPS_PROXY:-}}
    if [[ -z $proxy ]]; then
        fail "--network needs an https proxy in the environment (with-proxy on Meta hosts)"
    else
        cargo_bin=$(command -v cargo)
        rustup_home=${RUSTUP_HOME:-}
        if [[ -z $rustup_home ]] && command -v rustup >/dev/null; then
            rustup_home=$(rustup show home)
        fi
        crate=$work/net/crate
        mkdir -p "$crate/src" "$work/net/homedir"
        printf '[package]\nname = "proxy-probe"\nversion = "0.0.0"\nedition = "2021"\n\n[dependencies]\ncfg-if = "=1.0.0"\n' >"$crate/Cargo.toml"
        : >"$crate/src/lib.rs"
        # Port 9 (discard) on loopback: nothing listens, so a fetch through it
        # fails fast and never leaves the host.
        printf '[http]\n\tproxy = http://127.0.0.1:9\n' >"$work/net/homedir/.gitconfig"
        net_fetch() { # net_fetch LABEL CARGO-HOME [prelude]
            local label=$1 home=$2 prelude=${3:-:}
            # shellcheck disable=SC2016 # expanded by the child shell
            env -i PATH="$PATH" HOME="$work/net/homedir" RUSTUP_HOME="$rustup_home" \
                CARGO_HOME="$home" https_proxy="$proxy" \
                bash -c 'source "$1"; cd "$2" || exit 1; '"$prelude"'; exec "$3" fetch' \
                net "$HERE/../cargo-proxy.sh" "$crate" "$cargo_bin" \
                >"$work/net/$label.log" 2>&1
        }
        rm -f "$crate/Cargo.lock"
        if net_fetch before "$work/net/cold-before"; then
            fail "network control 1: a plain fetch with Git's http.proxy unexpectedly passed, so this host does not reproduce the failure"
        else
            pass "network control 1: a plain fetch on a cold home fails through Git's http.proxy ($(grep -m1 -o -E 'proxy|Connection refused|SSL[^,]*' "$work/net/before.log" | head -1))"
        fi
        rm -f "$crate/Cargo.lock"
        if net_fetch after "$work/net/cold-after" 'cargo_proxy_default . || exit $?'; then
            pass "network control 2: after cargo_proxy_default the same cold fetch succeeds"
        else
            fail "network control 2: the fetch after cargo_proxy_default failed: $(tail -3 "$work/net/after.log")"
        fi
        rm -f "$crate/Cargo.lock"
        mkdir -p "$crate/.cargo"
        printf '[http]\nproxy = "http://127.0.0.1:9"\n' >"$crate/.cargo/config.toml"
        if net_fetch respected "$work/net/cold-respected" 'cargo_proxy_default . || exit $?'; then
            fail "network control 3: a Cargo config proxy was overridden (the fetch passed)"
        elif grep -q "sets Cargo's http.proxy; applying nothing" "$work/net/respected.log"; then
            pass "network control 3: a Cargo config http.proxy is named and used, not overridden"
        else
            fail "network control 3: no 'applying nothing' line: $(tail -3 "$work/net/respected.log")"
        fi
    fi
fi

if ((failures)); then
    echo "test-cargo-proxy: $failures FAILED"
    exit 1
fi
echo "test-cargo-proxy: all checks passed"
