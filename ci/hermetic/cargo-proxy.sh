#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# The FETCH phase's Cargo proxy default, sourced by run-split-validate.sh and by
# its test (ci/hermetic/tests/test-cargo-proxy.sh), which sources THIS file.
#
# WHY THIS EXISTS. Cargo picks its HTTP proxy in this order: its own http.proxy
# (the CARGO_HTTP_PROXY environment variable, then its config files), then
# GIT's http.proxy, and only then libcurl's https_proxy/HTTPS_PROXY. A Git proxy
# set for git's sake, such as an mTLS forward-proxy port, therefore captures
# Cargo's crates.io downloads even when the environment names the right proxy,
# and the download fails with `[60] SSL peer certificate ... unable to get local
# issuer certificate`. Warm Cargo homes never notice, because they download
# nothing; a cold one fails every attempt.
#
# The default this file applies: when Cargo itself names no proxy and the
# environment does, export CARGO_HTTP_PROXY from the environment, so Cargo uses
# the proxy the caller chose instead of Git's. Cargo's own choice always wins:
#   * CARGO_HTTP_PROXY already set, even to the empty string, is left alone;
#   * a Cargo config file that sets http.proxy is named and left alone;
#   * a Cargo config file this check cannot read exactly is named and left
#     alone too: whatever it sets, Cargo keeps.
#
# The config files are the ones Cargo reads, by Cargo's own rule: in the query
# directory's .cargo and in each parent's, then in CARGO_HOME (default ~/.cargo;
# a relative CARGO_HOME is taken from the current directory, as Cargo takes it),
# the extensionless `config` whenever it can be opened, and config.toml only
# otherwise. So with both present Cargo reads only `config`, and an unreadable
# `config` alone is skipped.
#
# Each file is read by a small TOML lexer. It follows multi-line strings, and
# arrays and inline tables that span lines, so a `[...]` or `proxy = ...` line
# inside a value is never read as a header or a key; it strips a leading
# byte-order mark, as Cargo does. It finds http.proxy as a key under an [http]
# table, as a dotted http.proxy key, and inside an inline `http = { ... }`
# table, on one line or several (there, any mention of proxy counts). Quotes,
# whitespace and comments are handled as TOML handles them, including the one
# or two quotes a multi-line string may end with before its closer
# (`"""x""""` holds x"). A construct it does
# not interpret means "applying nothing": an include (Cargo reads the included
# files too), an escape sequence in a quoted key or in an inline http table,
# and anything it cannot parse.
#
# This replaced a check that asked `cargo -Zunstable-options config get`, which
# only a nightly Cargo answers. Once the pinned toolchain became stable, that
# check could never run, and every cold fetch on a host with a Git http.proxy
# failed.

# cargo_config_proxy_file [DIR]: for a Cargo started in DIR (default: the
# current directory), look at each Cargo config file in Cargo's discovery order
# and stop at the first that decides:
#   0: it sets http.proxy; prints its path.
#   3: it may set http.proxy in a form this check does not interpret; prints its
#      path, then on a second line what it has (for example "an include key").
#   2: the answer is unknown: DIR cannot be entered, the file Cargo would read
#      is not a readable regular file (Cargo itself would fail on it), or the
#      lexer failed; prints the path.
# Returns 1 when no file sets http.proxy.
cargo_config_proxy_file() {
    local dir file reason status
    local -a files=()
    if ! dir=$(CDPATH='' cd -- "${1:-.}" 2>/dev/null && pwd -P); then
        printf '%s\n' "${1:-.}"
        return 2
    fi
    while :; do
        cargo_config_file_in "$dir/.cargo"
        [[ $dir == / ]] && break
        dir=$(dirname -- "$dir")
    done
    cargo_config_file_in "${CARGO_HOME:-$HOME/.cargo}"
    for file in "${files[@]}"; do
        if [[ ! -f $file || ! -r $file ]]; then
            printf '%s\n' "$file"
            return 2
        fi
        reason=$(cargo_config_file_sets_proxy "$file") && status=0 || status=$?
        case $status in
            1) ;;
            3)
                printf '%s\n%s\n' "$file" "$reason"
                return 3
                ;;
            *)
                printf '%s\n' "$file"
                return "$status"
                ;;
        esac
    done
    return 1
}

# cargo_config_file_in DIR: append to the caller's `files` array the config
# file Cargo reads in DIR, by Cargo's rule (its get_file_path): `config` when it
# can be opened, else config.toml when it exists, else nothing.
cargo_config_file_in() {
    if [[ -r $1/config ]]; then
        files+=("$1/config")
    elif [[ -e $1/config.toml ]]; then
        files+=("$1/config.toml")
    fi
}

# cargo_config_file_sets_proxy FILE: 0 when FILE sets http.proxy, 1 when it
# does not, 3 when it has a construct this check does not interpret (printed),
# and 2 when the lexer itself fails. The lexer must print exactly one verdict
# line and exit 0; anything else is a failure, never a "does not set".
cargo_config_file_sets_proxy() {
    local verdict
    # shellcheck disable=SC2016 # an awk program, not shell
    verdict=$(LC_ALL=C awk '
        function decide(v) { print v; decided = 1; exit 0 }
        function blank(c) { return c == " " || c == "\t" }
        # lex_key(s, i): read the dotted key at s[i]; KEY gets its parts joined
        # by SUBSEP, so a quoted part holding a dot stays one part. Returns the
        # index after the key and any blanks, or 0.
        function lex_key(s, i,    n, c, j, part, parts) {
            n = length(s); KEY = ""; parts = 0
            for (;;) {
                while (i <= n && blank(substr(s, i, 1))) i++
                c = substr(s, i, 1)
                if (c == "\"" || c == "\047") {
                    j = index(substr(s, i + 1), c)
                    if (j == 0) return 0
                    part = substr(s, i + 1, j - 1)
                    if (c == "\"" && index(part, "\\")) decide("uncertain an escape sequence in a quoted key")
                    i += j + 1
                } else {
                    j = i
                    while (i <= n && substr(s, i, 1) ~ /[A-Za-z0-9_-]/) i++
                    if (i == j) return 0
                    part = substr(s, j, i - j)
                }
                KEY = parts++ ? KEY SUBSEP part : part
                while (i <= n && blank(substr(s, i, 1))) i++
                if (substr(s, i, 1) != ".") return i
                i++
            }
        }
        # lex_value(s, i): scan value text from s[i] to a comment or the end of
        # the line, tracking ML (1 inside """, 2 inside \047\047\047) and DEPTH
        # (open arrays and inline tables) across lines. Returns 0 when the text
        # is not a TOML value (an unclosed one-line string, a stray closer).
        function lex_value(s, i,    n, c, j) {
            n = length(s)
            while (i <= n) {
                if (ML == 1) {
                    while (i <= n && ML) {
                        if (substr(s, i, 1) == "\\") i += 2
                        else if (substr(s, i, 3) == "\"\"\"") {
                            # A closer may carry up to two more quotes,
                            # which belong to the string: """x"""" is x".
                            i += 3; ML = 0
                            if (substr(s, i, 1) == "\"") i++
                            if (substr(s, i, 1) == "\"") i++
                        }
                        else i++
                    }
                    continue
                }
                if (ML == 2) {
                    j = index(substr(s, i), "\047\047\047")
                    if (j == 0) return 1
                    i += j + 2; ML = 0
                    if (substr(s, i, 1) == "\047") i++
                    if (substr(s, i, 1) == "\047") i++
                    continue
                }
                c = substr(s, i, 1)
                if (c == "#") return 1
                if (c == "\"") {
                    if (substr(s, i, 3) == "\"\"\"") { ML = 1; i += 3; continue }
                    for (i++; i <= n && substr(s, i, 1) != "\""; i++)
                        if (substr(s, i, 1) == "\\") i++
                    if (i > n) return 0
                    i++
                    continue
                }
                if (c == "\047") {
                    if (substr(s, i, 3) == "\047\047\047") { ML = 2; i += 3; continue }
                    j = index(substr(s, i + 1), "\047")
                    if (j == 0) return 0
                    i += j + 1
                    continue
                }
                if (c == "[" || c == "{") DEPTH++
                else if ((c == "]" || c == "}") && --DEPTH < 0) return 0
                i++
            }
            return 1
        }
        # under(path, prefix): path is prefix or lies below it.
        function under(path, prefix) { return path == prefix || index(path, prefix SUBSEP) == 1 }
        # look(text): text inside an inline http table.
        function look(text) {
            if (index(text, "proxy")) decide("set")
            if (index(text, "\\")) decide("uncertain an escape sequence in an inline http table")
        }
        function value(s, i) {
            if (!lex_value(s, i)) decide("uncertain a value it cannot parse")
            if (!ML && DEPTH == 0) INHTTP = 0
        }
        NR == 1 && substr($0, 1, 3) == "\357\273\277" { $0 = substr($0, 4) }
        {
            s = $0
            sub(/\r$/, "", s)
            if (ML || DEPTH) {
                if (INHTTP) look(s)
                value(s, 1)
                next
            }
            i = 1
            while (blank(substr(s, i, 1))) i++
            c = substr(s, i, 1)
            if (c == "" || c == "#") next
            if (c == "[") {
                i += (substr(s, i, 2) == "[[") ? 2 : 1
                if (!(i = lex_key(s, i)) || substr(s, i, 1) != "]") decide("uncertain a table header it cannot parse")
                TABLE = KEY
                if (under(TABLE, "include")) decide("uncertain an include table")
                if (under(TABLE, "http" SUBSEP "proxy")) decide("set")
                next
            }
            if (!(i = lex_key(s, i)) || substr(s, i, 1) != "=") decide("uncertain a line it cannot parse")
            full = (TABLE == "") ? KEY : TABLE SUBSEP KEY
            if (TABLE == "" && under(KEY, "include")) decide("uncertain an include key")
            if (under(full, "http" SUBSEP "proxy")) decide("set")
            if (full == "http") { INHTTP = 1; look(substr(s, i + 1)) }
            value(s, i + 1)
        }
        END {
            if (decided) exit 0
            v = (ML || DEPTH) ? "uncertain a value that never closes" : "unset"
            print v
        }
    ' "$1") || return 2
    case $verdict in
        set) return 0 ;;
        unset) return 1 ;;
        "uncertain "*)
            printf '%s\n' "${verdict#uncertain }"
            return 3
            ;;
        *) return 2 ;;
    esac
}

# cargo_proxy_default [DIR]: apply the default described above for a Cargo
# started in DIR (default: the current directory), and say on stderr which
# branch it took. Exports CARGO_HTTP_PROXY only in the "exported" branch.
# Returns 2, exporting nothing, when a Cargo config file cannot be read: the
# caller must not start Cargo then. Run it in the shell (or subshell) that then
# starts Cargo.
cargo_proxy_default() {
    local proxy setter status
    proxy=${https_proxy:-${HTTPS_PROXY:-}}
    if [[ -n ${CARGO_HTTP_PROXY+x} ]]; then
        echo 'cargo-proxy: CARGO_HTTP_PROXY is already set in the environment; Cargo uses it unchanged.' >&2
        return 0
    fi
    if [[ -z $proxy ]]; then
        echo 'cargo-proxy: no https_proxy or HTTPS_PROXY in the environment; applying nothing, so Cargo uses its own or Git'"'"'s http.proxy, or none.' >&2
        return 0
    fi
    setter=$(cargo_config_proxy_file "${1:-.}") && status=0 || status=$?
    case $status in
        0)
            echo "cargo-proxy: $setter sets Cargo's http.proxy; applying nothing." >&2
            ;;
        1)
            export CARGO_HTTP_PROXY=$proxy
            echo 'cargo-proxy: no Cargo setting names a proxy; exported CARGO_HTTP_PROXY from the environment'"'"'s https proxy, so Git'"'"'s http.proxy is not used.' >&2
            ;;
        3)
            echo "cargo-proxy: ${setter%%$'\n'*} may set Cargo's http.proxy: it has ${setter#*$'\n'}, which this check does not interpret; applying nothing, so whatever it sets, Cargo keeps." >&2
            ;;
        *)
            echo "cargo-proxy: cannot read $setter, so whether Cargo has its own http.proxy is unknown; refusing to start Cargo." >&2
            return 2
            ;;
    esac
}
