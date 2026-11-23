#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# Tests that the Makefile installs system packages only on request. The
# install-build-tools recipe runs the platform package manager through sudo, so
# it must run only when the caller passes INSTALL_BUILD_TOOLS=1: `make`,
# `make build` and `make install-deps` never escalate on their own.
#
# Every make below runs under `env -i` with a PATH that holds only stand-ins:
# `sudo` and `dnf` log their arguments and exit 1, `id` reports a non-root
# caller, and there is no cmake (and, unless a case adds them, no C or C++
# compiler), so the build-tools check always finds something missing. No real
# sudo or package manager is reachable from these runs, and none of them
# writes inside the checkout.
#
#   build -n   `make -n build` walks the real build chain (build -> install-deps
#              -> check-submodules -> checkout-all -> check-build-tools). -n
#              prints each recipe without running it, except a line that calls
#              $(MAKE), which is how check-build-tools runs here. Without the
#              opt-in it fails naming the missing tools, their packages and
#              INSTALL_BUILD_TOOLS=1, and the installer's recipe is never
#              reached; with the opt-in the same harness does reach it, so the
#              negative is not vacuous.
#   check      check-build-tools for real, with stand-in compilers: without the
#              opt-in nothing is called and only cmake is named; with
#              INSTALL_BUILD_TOOLS=1 the installer runs, through the stand-in
#              sudo, and the check still fails because nothing was installed.
#   direct     `make install-build-tools` without the opt-in refuses by name and
#              calls nothing.
#
# Exit status: 0 every case passed; 1 a case failed.

set -uo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)
# Invoked by absolute path, so the recipes' $(MAKE) needs no make on PATH.
MAKE_BIN=$(command -v make) || {
    echo "test-build-tools-opt-in: FAIL - make is not on PATH" >&2
    exit 1
}

TMP=$(mktemp -d)
[[ -n $TMP && -d $TMP ]] || {
    echo "test-build-tools-opt-in: FAIL - mktemp -d made no directory" >&2
    exit 1
}
trap 'rm -rf -- "$TMP"' EXIT
BARE=$TMP/bare
COMPILERS=$TMP/compilers
CALLS=$TMP/calls
mkdir -p "$BARE" "$COMPILERS"

# A stand-in that logs its name and arguments to $CALLS and exits 1.
stand_in() {
    local path=$1
    {
        printf '#!/bin/sh\n'
        printf 'echo "%s $*" >>"%s"\n' "${path##*/}" "$CALLS"
        printf 'exit 1\n'
    } >"$path"
    chmod +x "$path"
}
stand_in "$BARE/sudo"
stand_in "$BARE/dnf"
stand_in "$COMPILERS/cc"
stand_in "$COMPILERS/c++"
cat >"$BARE/id" <<'EOF'
#!/bin/sh
# A non-root caller, so the installer reaches for sudo.
[ "${1:-}" = -u ] && { echo 1000; exit 0; }
echo "uid=1000 gid=1000"
EOF
chmod +x "$BARE/id"

failures=0
passes=0
check() {
    if [[ $2 -eq 0 ]]; then
        echo "ok   - $1"
        passes=$((passes + 1))
    else
        echo "FAIL - $1"
        failures=$((failures + 1))
    fi
}
yes_if() { if "$@"; then echo 0; else echo 1; fi; }

# No case runs unless every `sudo` and `dnf` these runs can reach is a stand-in
# and cmake is unreachable.
resolved=$(env -i PATH="$BARE:$COMPILERS" /bin/sh -c \
    'command -v sudo; command -v dnf; command -v cmake || echo no-cmake')
if [[ $resolved != "$BARE/sudo"$'\n'"$BARE/dnf"$'\n'"no-cmake" ]]; then
    echo "test-build-tools-opt-in: FAIL - the stand-ins do not resolve first on PATH; refusing to run make" >&2
    exit 1
fi

# run_make NAME PATH MAKE_ARGS...: run the checkout's Makefile under env -i,
# keeping NAME.out, NAME.err and NAME.rc; the stand-in log starts empty.
run_make() {
    local name=$1 path=$2 rc=0
    shift 2
    : >"$CALLS"
    env -i PATH="$path" LC_ALL=C "$MAKE_BIN" -C "$ROOT" --no-print-directory "$@" \
        >"$TMP/$name.out" 2>"$TMP/$name.err" || rc=$?
    echo "$rc" >"$TMP/$name.rc"
}
rc_of() { cat "$TMP/$1.rc"; }
no_calls() { [[ ! -s $CALLS ]]; }
has() { grep -qF -- "$2" "$TMP/$1"; }
lacks() { ! grep -qF -- "$2" "$TMP/$1"; }
has_line() { grep -qE -- "$2" "$TMP/$1"; }
# A line only the installer's recipe has, printed by -n when it is reached.
INSTALLER_MARK='SUDO="sudo -n"'

# --- build -n --------------------------------------------------------------
run_make build-n "$BARE" -n build
check "make -n build without the opt-in fails" "$(yes_if [ "$(rc_of build-n)" != 0 ])"
check "it names every missing tool" \
    "$(yes_if has build-n.err 'not found on PATH: cmake C-compiler(cc/gcc/clang) C++-compiler(c++/g++/clang++)')"
check "it names their packages" \
    "$(yes_if has_line build-n.err '^    Debian/Ubuntu: cmake build-essential$')"
check "it names their Fedora packages" \
    "$(yes_if has_line build-n.err '^    Fedora/RHEL: +cmake gcc gcc-c\+\+$')"
check "it names the opt-in" "$(yes_if has build-n.err 'rerun with INSTALL_BUILD_TOOLS=1')"
check "it never reaches the installer" "$(yes_if lacks build-n.out "$INSTALLER_MARK")"
check "it calls no stand-in" "$(yes_if no_calls)"

run_make build-n-opt-in "$BARE" -n build INSTALL_BUILD_TOOLS=1
check "make -n build with the opt-in reaches the installer" \
    "$(yes_if has build-n-opt-in.out "$INSTALLER_MARK")"

# --- check -----------------------------------------------------------------
run_make check "$BARE:$COMPILERS" check-build-tools
check "check-build-tools without the opt-in fails" "$(yes_if [ "$(rc_of check)" != 0 ])"
check "it names only cmake" "$(yes_if has_line check.err 'not found on PATH: cmake$')"
check "it names only the cmake package" \
    "$(yes_if has_line check.err '^    Fedora/RHEL: +cmake$')"
check "it names the opt-in" "$(yes_if has check.err 'rerun with INSTALL_BUILD_TOOLS=1')"
check "it calls no stand-in" "$(yes_if no_calls)"

run_make check-opt-in "$BARE:$COMPILERS" check-build-tools INSTALL_BUILD_TOOLS=1
check "with INSTALL_BUILD_TOOLS=1 the installer runs through sudo" \
    "$(yes_if [ "$(cat "$CALLS")" = "sudo -n dnf install -y cmake gcc gcc-c++ make" ])"
check "and the check still fails when nothing was installed" \
    "$(yes_if [ "$(rc_of check-opt-in)" != 0 ])"
check "and says the installer ran" \
    "$(yes_if has check-opt-in.err 'INSTALL_BUILD_TOOLS=1 ran the installer')"

# --- direct ----------------------------------------------------------------
run_make direct "$BARE" install-build-tools
check "make install-build-tools without the opt-in refuses" \
    "$(yes_if [ "$(rc_of direct)" != 0 ])"
check "it names the opt-in" "$(yes_if has direct.err 'runs only when the caller passes INSTALL_BUILD_TOOLS=1')"
check "it calls no stand-in" "$(yes_if no_calls)"

if ((failures)); then
    echo "test-build-tools-opt-in: $failures FAILED, $passes passed"
    exit 1
fi
echo "test-build-tools-opt-in: all $passes checks passed"
