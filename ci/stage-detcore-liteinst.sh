#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# Build the in-guest Detcore runtime (the detcore-liteinst cdylib,
# libdetcore_liteinst.so) beside a set of Cargo-built test executables, so
# the LiteInst and in-guest-trap cases in them find it. Callable on its own,
# from any directory, against any Hermit source tree that has the
# detcore-liteinst package (older trees included): it reads no
# validation-only environment and refuses by name when an input is missing.
#
# NOT hermit-install's stage-detcore-liteinst. That Rust tool
# (hermit-install/src/bin/stage-detcore-liteinst.rs, run by the workspace
# build and by `make install-deps`) finalizes the QUALIFIED packaged runtime:
# the liteinst-runtime profile with ThinLTO, qualification, and installation
# into an installation's rsrcs/ with a receipt. This script builds an
# UNQUALIFIED runtime in the test executables' own Cargo profile and leaves it
# in their deps directory, where only a test executable that finds no
# installation looks. It never installs it: hermit-install/build.rs forbids
# exposing such a library as an installed resource.
#
# Usage:
#   ci/stage-detcore-liteinst.sh --source SOURCE_TREE --target-dir TARGET_DIR --profile PROFILE [--offline]
#
# SOURCE_TREE  The Hermit checkout the test executables were built from: its
#              Cargo.toml is the workspace root and detcore-liteinst/ is a
#              member. Cargo runs from this directory, so the tree's own
#              rust-toolchain.toml selects the compiler.
# TARGET_DIR   The Cargo target directory the test executables were built
#              into (their CARGO_TARGET_DIR or --target-dir). Relative paths
#              resolve from the current directory.
# PROFILE      The Cargo profile they were built with: dev, release, or a
#              custom profile such as validate.
# --offline    Optional. Pass --offline to Cargo: Cargo then uses only
#              crates already downloaded into CARGO_HOME (or vendored by the
#              tree's Cargo configuration) and fails with its own status when
#              one is missing. A coordinator ruling (2026-11-22, 8:38 PM ET)
#              requires that fixture staging never reach the network; the
#              out-of-tree launcher that stages runtimes for older trees runs
#              every Cargo call this way, and the validation node that runs
#              this script (build.hermit_exec_stop_launcher_compile_in_pinned_root)
#              passes --offline explicitly. CARGO_NET_OFFLINE=true has the same
#              effect in Cargo, but the flag puts the guarantee on the command
#              line itself: it is printed in this script's failure message,
#              and it survives launchers that pass on only named environment
#              variables. Without it the Cargo command is unchanged.
# The options may come in any order; nothing else is accepted.
#
# Effect: runs, in SOURCE_TREE,
#   cargo build --locked [--offline] --message-format=json-render-diagnostics \
#       --manifest-path SOURCE_TREE/Cargo.toml -p detcore-liteinst --lib \
#       --profile PROFILE --target-dir TARGET_DIR
# and takes the runtime's path from Cargo's own report: the compiler-artifact
# record for target detcore_liteinst must name exactly one cdylib (.so) inside
# TARGET_DIR, built by this call (fresh or not). Its copy in the deps
# directory beside it is the one the test executables use, and must hold the
# same bytes. No output directory is predicted from the profile name, so a
# library left by an earlier build is never reported as staged, and a target
# triple layout (CARGO_BUILD_TARGET) is followed, not refused.
#
# Hermit's resolver (liteinst_tool_runtime_library_path in
# hermit-cli/src/lib.rs) takes HERMIT_LITEINST_TOOL_RUNTIME first, then an
# installation's rsrcs directory (HERMIT_INSTALL_DIR, or one found beside the
# executable), then a libdetcore_liteinst.so beside the executable or in its
# deps directory. This staging serves the last case, which is where the
# default validation build leaves the runtime (its workspace build compiles
# detcore-liteinst in the same target and profile as the tests), so leave the
# first two unset when running the executables.
#
# Features: Cargo resolves detcore-liteinst's features for this package
# alone. The default test node's runtime comes from the `--workspace
# --all-targets` build, whose feature unification also includes hermit-cli's
# dev-dependency reverie-inguest/exec-stop; a cohort's own host build may
# enable more (exec-stop-launcher enables reverie-inguest's exec-stop and
# loader-private-runtime). So the runtime staged here can be built with fewer
# features than the one the default test node uses; the consuming node's own
# run shows whether its cases pass with it. Cargo's own environment (CARGO,
# CARGO_HOME, CARGO_BUILD_JOBS, CARGO_BUILD_TARGET, CARGO_NET_OFFLINE) applies
# as usual; CARGO_TARGET_DIR is overridden by --target-dir.
#
# Exit status: 0 after staging (one line on stdout names the file and its
# SHA-256); 2, with "stage-detcore-liteinst: <reason>" on stderr, when the
# usage is wrong, an input is missing, or Cargo reports no single cdylib
# inside TARGET_DIR; Cargo's own status when the build fails.
set -euo pipefail

usage="usage: $0 --source SOURCE_TREE --target-dir TARGET_DIR --profile PROFILE [--offline]"
fail() { printf 'stage-detcore-liteinst: %s\n' "$*" >&2; exit 2; }

source_tree='' target_dir='' profile=''
offline=()
while (($#)); do
    case $1 in
        --offline)
            offline=(--offline)
            shift
            ;;
        --source|--target-dir|--profile)
            if (($# < 2)) || [[ -z $2 ]]; then fail "$1 needs a value; $usage"; fi
            case $1 in
                --source) source_tree=$2 ;;
                --target-dir) target_dir=$2 ;;
                --profile) profile=$2 ;;
            esac
            shift 2
            ;;
        *) fail "unexpected argument '$1'; $usage" ;;
    esac
done
[[ -n $source_tree ]] || fail "--source is required; $usage"
[[ -n $target_dir ]] || fail "--target-dir is required; $usage"
[[ -n $profile ]] || fail "--profile is required; $usage"
[[ $profile =~ ^[A-Za-z0-9_-]+$ ]] || fail "--profile '$profile' is not a Cargo profile name"

[[ -d $source_tree ]] || fail "the source tree $source_tree is not a directory"
source_tree=$(realpath -e -- "$source_tree")
[[ -f $source_tree/Cargo.toml ]] ||
    fail "the source tree $source_tree has no Cargo.toml; pass the Hermit checkout's root"
[[ -f $source_tree/detcore-liteinst/Cargo.toml ]] ||
    fail "the source tree $source_tree has no detcore-liteinst package; this tree has no in-guest Detcore runtime to stage"
target_dir=$(realpath -m -- "$target_dir")
command -v python3 >/dev/null 2>&1 || fail "python3 is required to read Cargo's artifact report"

status=0
messages=$(env -C "$source_tree" cargo build --locked "${offline[@]}" --message-format=json-render-diagnostics \
    --manifest-path "$source_tree/Cargo.toml" -p detcore-liteinst --lib --profile "$profile" \
    --target-dir "$target_dir") || status=$?
if ((status != 0)); then
    printf 'stage-detcore-liteinst: cargo build --locked%s --message-format=json-render-diagnostics --manifest-path %s -p detcore-liteinst --lib --profile %s --target-dir %s failed in %s (exit %d)\n' \
        "${offline[*]/#/ }" "$source_tree/Cargo.toml" "$profile" "$target_dir" "$source_tree" "$status" >&2
    exit "$status"
fi

# Every .so Cargo reports for target detcore_liteinst when it is built as a
# cdylib, one per line; nothing when this tree's library is no cdylib.
# shellcheck disable=SC2016  # Python source, not shell expansions.
report='
import json, sys
found = set()
for line in sys.stdin:
    try:
        message = json.loads(line)
    except ValueError:
        continue
    target = message.get("target") or {}
    kinds = target.get("crate_types") or target.get("kind") or []
    if message.get("reason") == "compiler-artifact" and target.get("name") == "detcore_liteinst" and "cdylib" in kinds:
        found.update(name for name in message.get("filenames") or [] if name.endswith(".so"))
print("\n".join(sorted(found)))
'
reported=$(python3 -I -c "$report" <<<"$messages") ||
    fail "cannot read Cargo's artifact report for detcore-liteinst"
mapfile -t cdylibs < <(printf '%s' "$reported" | sed '/^$/d')
((${#cdylibs[@]} == 1)) ||
    fail "Cargo built detcore-liteinst in $source_tree but reported ${#cdylibs[@]} cdylib file(s) for it (${cdylibs[*]:-none}); this tree's detcore-liteinst must build one cdylib, and a library left by an earlier build is never staged"
built=$(realpath -e -- "${cdylibs[0]}") || fail "Cargo reported ${cdylibs[0]}, which does not exist"
[[ $built == "$target_dir"/* ]] ||
    fail "Cargo reported $built, which is not inside the target directory $target_dir"
runtime=$(dirname -- "$built")/deps/$(basename -- "$built")
[[ -f $runtime && ! -L $runtime && -s $runtime ]] ||
    fail "Cargo reported $built, but its deps copy $runtime, the one the test executables use, is not a non-empty regular file"
digest=$(sha256sum -- "$runtime") || fail "cannot hash $runtime"
reported_digest=$(sha256sum -- "$built") || fail "cannot hash $built"
[[ ${digest%% *} == "${reported_digest%% *}" ]] ||
    fail "Cargo reported $built, but its deps copy $runtime holds other bytes"
printf 'stage-detcore-liteinst: staged %s (sha256 %s) for the %s test executables in %s\n' \
    "$runtime" "${digest%% *}" "$profile" "$(dirname -- "$runtime")"
