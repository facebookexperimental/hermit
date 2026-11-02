#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
#
# Compile a small Rust program with rustc and run it:
# `rustc hello.rs -o hello && ./hello`, which prints "hello from rustc".
#
# rustc starts its linker driver (cc) with the Rust standard library's
# Command. Because rustc changes PATH for that child, the library forks
# rather than calling posix_spawn, and the child reports a failed exec over a
# socketpair(AF_UNIX, SOCK_SEQPACKET). cc then runs rustc's bundled linker,
# rust-lld, which maps its output file with mmap(MAP_SHARED | MAP_NORESERVE).
# KVM refused both until Reverie admitted SOCK_SEQPACKET socketpairs and
# MAP_NORESERVE file views.
#
# The link is single-threaded (lld --threads=1) on every backend. rust-lld
# maps its output after it starts its worker threads, and on KVM a shared
# file view still needs one vCPU in its address space
# (https://github.com/rrnewton/reverie/issues/1108). The CLI test
# run_kvm_default_rustc_link_is_a_named_refusal_where_ptrace_links in
# hermit-cli/tests/cli.rs requires KVM's refusal of the default link by name,
# and fails once KVM runs it, which is when this flag should go.
#
# The guest's PATH is Hermit's minimal one, so --prepare records the
# toolchain's own rustc binary for --run: a rustup proxy on PATH would need
# the caller's HOME, which the cell isolates.
set -euo pipefail

case ${1:-} in
    --prepare)
        command -v rustc >/dev/null 2>&1 || {
            echo "rustc not found" >&2
            exit 1
        }
        : "${E2E_FIXTURE_DIR:?E2E_FIXTURE_DIR must be set}"
        mkdir -p -- "$E2E_FIXTURE_DIR"
        sysroot=$(rustc --print sysroot)
        [[ -x $sysroot/bin/rustc ]] || {
            echo "no rustc binary in $sysroot/bin" >&2
            exit 1
        }
        printf '%s\n' "$sysroot/bin/rustc" >"$E2E_FIXTURE_DIR/rustc-path"
        ;;
    --run)
        : "${E2E_FIXTURE_DIR:?E2E_FIXTURE_DIR must be set}"
        rustc=$(<"$E2E_FIXTURE_DIR/rustc-path")
        work="${E2E_TMPDIR:-/tmp}/hermit-rustc-hello"
        mkdir -p -- "$work"
        cat >"$work/hello.rs" <<'EOF'
fn main() {
    println!("hello from rustc");
}
EOF
        "$rustc" -C link-arg=-Wl,--threads=1 "$work/hello.rs" -o "$work/hello"
        "$work/hello"
        ;;
    *)
        echo "usage: $0 --prepare|--run" >&2
        exit 2
        ;;
esac
