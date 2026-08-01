#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
#
# End-to-end m4 (macro processor) determinism fixture.
#
# m4 is the macro language underpinning autoconf/automake. Its `mkstemp`
# builtin creates a temporary file from an `XXXXXX` template, and the random
# suffix is drawn by glibc mkstemp(3) from pid/clock/kernel entropy, so it
# varies every run natively. The rest of the program is pure, deterministic
# macro expansion -- integer `eval`, recursive `ifelse` factorial,
# `translit`, and `len` -- so under Hermit --strict the entropy behind the
# temp-name suffix is determinized and the entire expansion is
# bitwise-identical across runs. The deterministic macro results cross-check
# that m4's expansion semantics are preserved under syscall interception.
set -euo pipefail

case ${1:-} in
    --prepare)
        command -v m4 >/dev/null 2>&1 || {
            echo "m4 not found" >&2
            exit 1
        }
        exit 0
        ;;
    --run)
        # Hermit gives the guest a fresh isolated /tmp per repeat; create the
        # working directory before writing the m4 source.
        work="${E2E_TMPDIR:-/tmp}/hermit-m4-macro"
        rm -rf "$work"
        mkdir -p "$work"
        src="${work}/in.m4"

        # `esyscmd`/file reads are avoided; TMPL is passed via -D so the
        # mkstemp template lives in the guest-visible working directory.
        cat >"$src" <<'M4'
define(`SQR', `eval($1 * $1)')dnl
define(`FACT', `ifelse($1, 0, 1, `eval($1 * FACT(decr($1)))')')dnl
tmp: mkstemp(TMPL)
sqr: SQR(7) SQR(12) SQR(25)
fact: FACT(6)
upper: translit(`hermit determinism', `a-z', `A-Z')
len: len(`abcdefghij')
M4

        exec m4 -DTMPL="${work}/hermit-m4-XXXXXX" "$src"
        ;;
    *)
        echo "usage: $0 --prepare|--run" >&2
        exit 2
        ;;
esac
