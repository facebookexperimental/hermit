#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# Verify and resolve one content-addressed Hermit E2E artifact.
set -euo pipefail

function fail {
    echo "verify-hermit-e2e-artifact.sh: $*" >&2
    exit 2
}

function tree_manifest {
    local root=$1 relative hash
    while IFS= read -r -d '' relative; do
        hash=$(sha256sum "$root/$relative" | cut -d' ' -f1)
        printf '%s  %s\n' "$hash" "$relative"
    done < <(cd "$root" && find -L . -type f -printf '%P\0' | LC_ALL=C sort -z)
}

function binary_declares_runtime_resources {
    local binary=$1 dynamic magic rpath runpath needed
    # Historical Cargo/default fixtures may be executable scripts. They do not
    # declare an ELF runtime closure; the exact resource inventory below still
    # rejects any undeclared hermit-runtime directory beside them.
    magic=$(od -An -t x1 -N4 "$binary" | tr -d ' \n') ||
        fail "cannot inspect selected Hermit file type: $binary"
    if [[ $magic != 7f454c46 ]]; then
        return 1
    fi
    dynamic=$(readelf -d "$binary") || fail "cannot read selected Hermit dynamic contract: $binary"
    rpath=$(sed -n 's/.*(RPATH).*Library rpath: \[\(.*\)\].*/\1/p' <<<"$dynamic")
    runpath=$(sed -n 's/.*(RUNPATH).*Library runpath: \[\(.*\)\].*/\1/p' <<<"$dynamic")
    needed=$(sed -n 's/.*(NEEDED).*Shared library: \[\(.*\)\].*/\1/p' <<<"$dynamic")
    if [[ $rpath == '$ORIGIN/../install_pkg/rsrcs/hermit-runtime:$ORIGIN/install/rsrcs/hermit-runtime' ]]; then
        [[ -z $runpath && $(grep -Fxc 'libunwind-x86_64.so.8' <<<"$needed") == 1 ]] ||
            fail "selected Hermit has an incomplete unwind runtime contract: $binary"
        return 0
    fi
    if [[ $rpath == *hermit-runtime* || $runpath == *hermit-runtime* ]]; then
        fail "selected Hermit has an unsupported partial unwind runtime contract: $binary"
    fi
    return 1
}

function require_runtime_closure {
    local runtime=$1 path actual
    [[ -d $runtime && ! -L $runtime ]] ||
        fail "runtime bundle has no real hermit-runtime directory: $runtime"
    for path in libunwind-x86_64.so.8 libunwind.so.8; do
        [[ -f $runtime/$path && ! -L $runtime/$path && -s $runtime/$path ]] ||
            fail "runtime bundle is missing, empty, or linked: $runtime/$path"
    done
    # Every entry, not only files and links: an extra directory, FIFO or
    # socket is also outside the closure.
    actual=$(cd "$runtime" && find . -mindepth 1 | LC_ALL=C sort)
    [[ $actual == $'./libunwind-x86_64.so.8\n./libunwind.so.8' ]] ||
        fail "runtime bundle contains files outside the exact unwind closure: $runtime"
}

# The SaBRe plugin (https://github.com/rrnewton/hermit/issues/3652) and the
# in-guest LiteInst runtime (https://github.com/rrnewton/hermit/issues/3967) are
# guest preloads: each guest's own dynamic loader loads them, against the libc
# that guest already has. The bundle also ships Reverie's plain LiteInst preload
# (https://github.com/rrnewton/reverie/issues/980), which no Hermit run path has
# preloaded since 2d8bada7e9 (2026-10-04); Reverie's own strace tool and tests
# preload it, and it is checked the same way so that every shipped preload keeps
# the contract. Each may need only libraries every glibc provides, may name no
# build-root directory to find them in, and may require no glibc symbol version
# newer than the oldest supported host's: glibc 2.34, measured on the host
# recorded for GUEST_PRELOAD_GLIBC_MINOR_FLOOR in docs/TESTING_ENVIRONMENTS.md
# under "Named measurement hosts". detcore-sabre/build.rs,
# detcore-liteinst/build.rs, detcore-sabre/src/glibc_compat.rs and Reverie's
# reverie-liteinst/build.rs build them that way. ci/test_guest_preload_gate.py
# runs this gate on real ELF files and checks that both scripts apply it to all
# three.
GUEST_PRELOAD_GLIBC_MINOR_FLOOR=34

function require_portable_guest_preload {
    local label=$1 preload=$2 magic dynamic library versions version
    # Like the Hermit binary above, the publication fixtures use scripts in
    # place of ELF files; a script carries no loader contract to check.
    magic=$(od -An -t x1 -N4 "$preload" | tr -d ' \n') ||
        fail "cannot inspect $label file type: $preload"
    [[ $magic == 7f454c46 ]] || return 0
    dynamic=$(readelf -d "$preload") || fail "cannot read $label dynamic section: $preload"
    if grep -Eq '\((RPATH|RUNPATH)\)' <<<"$dynamic"; then
        fail "$label records a library search path, so a guest would load the build root's libraries: $preload"
    fi
    while IFS= read -r library; do
        case $library in
            ld-linux-x86-64.so.2 | libc.so.6 | libm.so.6 | libdl.so.2 | libpthread.so.0 | librt.so.1 | libutil.so.1) ;;
            *) fail "$label needs $library, which not every guest's glibc provides: $preload" ;;
        esac
    done < <(sed -n 's/.*(NEEDED).*Shared library: \[\(.*\)\].*/\1/p' <<<"$dynamic")
    versions=$(readelf -V --wide "$preload") || fail "cannot read $label symbol versions: $preload"
    while IFS= read -r version; do
        [[ $version =~ ^GLIBC_2\.([0-9]+)(\.[0-9]+)?$ ]] &&
            ((BASH_REMATCH[1] <= GUEST_PRELOAD_GLIBC_MINOR_FLOOR)) ||
            fail "$label requires symbol version $version; only glibc's up to GLIBC_2.$GUEST_PRELOAD_GLIBC_MINOR_FLOOR load in every guest: $preload"
    done < <(sed -n 's/.*Name: \([^ ]*\) *Flags:.*/\1/p' <<<"$versions")
}

function require_complete_resources {
    local install=$1 require_runtime=$2 path
    [[ -d $install/rsrcs ]] || fail "resource bundle has no rsrcs directory: $install"
    for path in \
        libdetcore_dbt.so \
        libdetcore_sabre.so \
        libdetcore_liteinst.so \
        libreverie_dbt_client.so \
        libreverie_liteinst.so; do
        [[ -f $install/rsrcs/$path && ! -L $install/rsrcs/$path && -s $install/rsrcs/$path ]] ||
            fail "resource bundle is missing or empty: $install/rsrcs/$path"
    done
    for path in dynamorio/bin64/drrun sabre e9patch e9tool; do
        [[ -f $install/rsrcs/$path && ! -L $install/rsrcs/$path && -s $install/rsrcs/$path && -x $install/rsrcs/$path ]] ||
            fail "resource bundle executable is missing, empty, or non-executable: $install/rsrcs/$path"
    done
    require_portable_guest_preload "SaBRe plugin" "$install/rsrcs/libdetcore_sabre.so"
    require_portable_guest_preload "In-guest LiteInst runtime" "$install/rsrcs/libdetcore_liteinst.so"
    require_portable_guest_preload "LiteInst preload" "$install/rsrcs/libreverie_liteinst.so"
    if [[ $require_runtime == true ]]; then
        require_runtime_closure "$install/rsrcs/hermit-runtime"
    elif [[ -e $install/rsrcs/hermit-runtime ]]; then
        fail "resource bundle carries an undeclared Hermit runtime closure: $install/rsrcs/hermit-runtime"
    fi
}

function require_runtime_resources {
    local install=$1 actual
    require_runtime_closure "$install/rsrcs/hermit-runtime"
    actual=$(cd "$install" && find . -mindepth 1 | LC_ALL=C sort)
    [[ $actual == $'./rsrcs\n./rsrcs/hermit-runtime\n./rsrcs/hermit-runtime/libunwind-x86_64.so.8\n./rsrcs/hermit-runtime/libunwind.so.8' ]] ||
        fail "runtime bundle contains files outside the exact unwind closure: $install"
}

[[ $# == 1 ]] || fail "usage: $0 BUNDLE-OR-POINTER"
input=$1
if [[ -d $input ]]; then
    bundle=$(cd "$input" && pwd -P)
else
    [[ -f $input && -s $input ]] || fail "artifact pointer is missing or empty: $input"
    IFS= read -r bundle <"$input"
    [[ -n $bundle && $bundle == /* ]] || fail "artifact pointer must contain one absolute path: $input"
    [[ $(wc -l <"$input") == 1 ]] || fail "artifact pointer must contain exactly one line: $input"
fi

[[ -d $bundle && ! -L $bundle ]] || fail "published artifact directory is missing or not a regular directory: $bundle"
[[ -f $bundle/kind && -s $bundle/kind ]] || fail "published artifact has no kind marker: $bundle"
kind=$(<"$bundle/kind")
[[ $kind == complete || $kind == runtime || $kind == binary-only ]] || fail "unknown artifact kind '$kind': $bundle"
[[ -f $bundle/hermit && ! -L $bundle/hermit && -s $bundle/hermit && -x $bundle/hermit ]] ||
    fail "published Hermit is missing, empty, or non-executable: $bundle/hermit"
[[ -f $bundle/hermit.sha256 && -s $bundle/hermit.sha256 ]] ||
    fail "published Hermit hash is missing: $bundle/hermit.sha256"
expected_binary_hash=$(<"$bundle/hermit.sha256")
actual_binary_hash=$(sha256sum "$bundle/hermit" | cut -d' ' -f1)
[[ $actual_binary_hash == "$expected_binary_hash" ]] ||
    fail "published Hermit hash mismatch: expected $expected_binary_hash, got $actual_binary_hash"

resource_hash=none
runtime_contract=none
if [[ -e $bundle/runtime-contract ]]; then
    [[ -f $bundle/runtime-contract && ! -L $bundle/runtime-contract && $(wc -l <"$bundle/runtime-contract") == 1 ]] ||
        fail "published runtime contract is malformed: $bundle/runtime-contract"
    runtime_contract=$(<"$bundle/runtime-contract")
fi
if [[ $kind == complete ]]; then
    declared=false
    if binary_declares_runtime_resources "$bundle/hermit"; then
        declared=true
    fi
    case $runtime_contract in
        elf-rpath-v1) [[ $declared == true ]] || fail "ELF runtime contract marker has no matching binary contract: $bundle" ;;
        # This signed marker is the explicit six-argument shadow-comparison
        # contract. It requires the exact closure but does not pretend that a
        # Cargo ELF without the Buck RPATH declares a loader dependency.
        explicit-runtime-overlay-v1) declared=true ;;
        none) [[ $declared == false ]] || fail "selected Hermit runtime contract marker is missing: $bundle" ;;
        *) fail "unknown complete-artifact runtime contract '$runtime_contract': $bundle" ;;
    esac
    require_complete_resources "$bundle/install" "$declared"
    # The manifest hashes regular files only, so anything else would ride along
    # unbound. The publisher copies without links; a special file never belongs.
    special=$(find "$bundle/install" -mindepth 1 ! -type f ! -type d -print -quit)
    [[ -z $special ]] || fail "complete artifact contains a non-regular entry outside its manifest: $special"
    [[ -f $bundle/resources.sha256 ]] || fail "complete artifact has no resource manifest: $bundle"
    generated=$(mktemp)
    trap 'rm -f "$generated"' EXIT
    tree_manifest "$bundle/install" >"$generated"
    cmp -s "$bundle/resources.sha256" "$generated" || fail "published resource hash manifest does not match: $bundle"
    resource_hash=$(sha256sum "$bundle/resources.sha256" | cut -d' ' -f1)
elif [[ $kind == runtime ]]; then
    [[ $runtime_contract == runtime-only-v1 ]] || fail "runtime artifact has the wrong runtime contract: $bundle"
    require_runtime_resources "$bundle/install"
    [[ -f $bundle/resources.sha256 ]] || fail "runtime artifact has no resource manifest: $bundle"
    generated=$(mktemp)
    trap 'rm -f "$generated"' EXIT
    tree_manifest "$bundle/install" >"$generated"
    cmp -s "$bundle/resources.sha256" "$generated" || fail "published runtime hash manifest does not match: $bundle"
    resource_hash=$(sha256sum "$bundle/resources.sha256" | cut -d' ' -f1)
elif [[ -e $bundle/install || -e $bundle/resources.sha256 ]]; then
    fail "binary-only artifact unexpectedly contains an unverified resource bundle: $bundle"
elif [[ $runtime_contract != none ]]; then
    fail "binary-only artifact unexpectedly declares runtime resources: $bundle"
fi

identity=$(printf '%s\n%s\n%s\n%s\n' "$kind" "$actual_binary_hash" "$resource_hash" "$runtime_contract" | sha256sum | cut -d' ' -f1)
[[ ${bundle##*/} == "$identity" ]] ||
    fail "content-addressed artifact identity mismatch: expected directory $identity, got ${bundle##*/}"
printf '%s\n' "$bundle"
