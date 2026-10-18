#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# Snapshot mutable Cargo outputs into one verified content-addressed artifact.
set -euo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
VERIFY="$ROOT_DIR/ci/verify-hermit-e2e-artifact.sh"

function fail {
    echo "publish-hermit-e2e-artifact.sh: $*" >&2
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
    for path in libdetcore_dbt.so libdetcore_sabre.so libdetcore_liteinst.so libreverie_dbt_client.so libreverie_liteinst.so; do
        [[ -f $install/rsrcs/$path && -s $install/rsrcs/$path ]] ||
            fail "resource bundle is missing or empty: $install/rsrcs/$path"
    done
    for path in dynamorio/bin64/drrun sabre e9patch e9tool; do
        [[ -f $install/rsrcs/$path && -s $install/rsrcs/$path && -x $install/rsrcs/$path ]] ||
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

[[ $# == 3 || $# == 4 || $# == 5 || $# == 6 ]] ||
    fail "usage: $0 SOURCE-BINARY BUNDLE-ROOT POINTER [SOURCE-INSTALL-DIR | --runtime-only SOURCE-INSTALL-DIR | SOURCE-INSTALL-DIR --runtime-overlay RUNTIME-INSTALL-DIR]"
source_binary=$1
bundle_root=$2
pointer=$3
source_install=""
runtime_overlay=""
kind="binary-only"
if [[ $# == 4 ]]; then
    source_install=$4
    kind=complete
elif [[ $# == 5 ]]; then
    [[ $4 == --runtime-only ]] || fail "five-argument form requires --runtime-only"
    source_install=$5
    kind=runtime
elif [[ $# == 6 ]]; then
    [[ $5 == --runtime-overlay ]] || fail "six-argument form requires --runtime-overlay"
    source_install=$4
    runtime_overlay=$6
    kind=complete
fi

[[ -f $source_binary && ! -L $source_binary && -s $source_binary && -x $source_binary ]] ||
    fail "source Hermit is missing, empty, or non-executable: $source_binary"
mkdir -p "$bundle_root" "$(dirname "$pointer")"
bundle_root=$(cd "$bundle_root" && pwd -P)
pointer_dir=$(cd "$(dirname "$pointer")" && pwd -P)
pointer="$pointer_dir/$(basename "$pointer")"
stage="$bundle_root/.tmp-$$"
pointer_tmp="$pointer.tmp-$$"
before_manifest=$(mktemp)
after_manifest=$(mktemp)
overlay_before_manifest=$(mktemp)
overlay_after_manifest=$(mktemp)
function cleanup {
    rm -rf "$stage"
    rm -f "$pointer_tmp" "$before_manifest" "$after_manifest" "$overlay_before_manifest" "$overlay_after_manifest"
}
trap cleanup EXIT
[[ ! -e $stage ]] || fail "staging path already exists: $stage"
mkdir -p "$stage"

binary_hash_before=$(sha256sum "$source_binary" | cut -d' ' -f1)
install -m 755 "$source_binary" "$stage/hermit"
binary_hash_after=$(sha256sum "$source_binary" | cut -d' ' -f1)
published_binary_hash=$(sha256sum "$stage/hermit" | cut -d' ' -f1)
[[ -f $source_binary && ! -L $source_binary && -s $source_binary && -x $source_binary ]] ||
    fail "source Hermit changed type, size, or mode during publication: $source_binary"
[[ $binary_hash_before == "$binary_hash_after" && $binary_hash_before == "$published_binary_hash" ]] ||
    fail "source Hermit changed bytes during publication: before=$binary_hash_before after=$binary_hash_after copy=$published_binary_hash"
printf '%s\n' "$published_binary_hash" >"$stage/hermit.sha256"
printf '%s\n' "$kind" >"$stage/kind"

resource_hash=none
runtime_contract=none
if [[ $kind == complete ]]; then
    runtime_required=false
    source_runtime_required=false
    if [[ -n $runtime_overlay ]]; then
        # The six-argument form is an explicit publisher contract used by the
        # Cargo/Buck shadow parity comparison. Its signed marker requires the
        # same copied closure without claiming the Cargo ELF itself needs it.
        runtime_required=true
        runtime_contract=explicit-runtime-overlay-v1
    elif binary_declares_runtime_resources "$source_binary"; then
        runtime_required=true
        source_runtime_required=true
        runtime_contract=elf-rpath-v1
    fi
    require_complete_resources "$source_install" "$source_runtime_required"
    tree_manifest "$source_install" >"$before_manifest"
    [[ -s $before_manifest ]] || fail "source install bundle contains no regular files: $source_install"
    mkdir -p "$stage/install"
    cp -aL "$source_install/." "$stage/install/"
    tree_manifest "$source_install" >"$after_manifest"
    cmp -s "$before_manifest" "$after_manifest" || fail "source install bundle changed during publication: $source_install"
    if [[ -n $runtime_overlay ]]; then
        require_runtime_resources "$runtime_overlay"
        tree_manifest "$runtime_overlay" >"$overlay_before_manifest"
        mkdir -p "$stage/install/rsrcs"
        cp -aL "$runtime_overlay/rsrcs/hermit-runtime" "$stage/install/rsrcs/"
        tree_manifest "$runtime_overlay" >"$overlay_after_manifest"
        cmp -s "$overlay_before_manifest" "$overlay_after_manifest" || fail "runtime overlay changed during publication: $runtime_overlay"
    fi
    tree_manifest "$stage/install" >"$stage/resources.sha256"
    if [[ -z $runtime_overlay ]]; then
        cmp -s "$before_manifest" "$stage/resources.sha256" || fail "published resource bytes do not match source bundle: $source_install"
    else
        # The stage is exactly the source bundle plus the overlay closure: the
        # source carries no hermit-runtime (require_complete_resources refused
        # one above), so each part is compared to its own origin, not trusted
        # to cp.
        cmp -s "$before_manifest" <(awk '$2 !~ /^rsrcs\/hermit-runtime\//' "$stage/resources.sha256") ||
            fail "published resource bytes outside the runtime overlay do not match source bundle: $source_install"
        cmp -s "$overlay_before_manifest" <(awk '$2 ~ /^rsrcs\/hermit-runtime\//' "$stage/resources.sha256") ||
            fail "published runtime overlay bytes do not match the overlay: $runtime_overlay"
    fi
    require_complete_resources "$stage/install" "$runtime_required"
    [[ -z $(find "$stage/install" -type l -print -quit) ]] ||
        fail "published resource bundle retained a symlink instead of an immutable copy: $stage/install"
    special=$(find "$stage/install" -mindepth 1 ! -type f ! -type d -print -quit)
    [[ -z $special ]] || fail "resource bundle contains a non-regular entry outside its manifest: $special"
    resource_hash=$(sha256sum "$stage/resources.sha256" | cut -d' ' -f1)
elif [[ $kind == runtime ]]; then
    runtime_contract="runtime-only-v1"
    require_runtime_resources "$source_install"
    tree_manifest "$source_install" >"$before_manifest"
    mkdir -p "$stage/install"
    cp -aL "$source_install/." "$stage/install/"
    tree_manifest "$source_install" >"$after_manifest"
    cmp -s "$before_manifest" "$after_manifest" || fail "source runtime bundle changed during publication: $source_install"
    tree_manifest "$stage/install" >"$stage/resources.sha256"
    cmp -s "$before_manifest" "$stage/resources.sha256" || fail "published runtime bytes do not match source bundle: $source_install"
    require_runtime_resources "$stage/install"
    [[ -z $(find "$stage/install" -type l -print -quit) ]] ||
        fail "published runtime bundle retained a symlink instead of an immutable copy: $stage/install"
    resource_hash=$(sha256sum "$stage/resources.sha256" | cut -d' ' -f1)
fi

if [[ $runtime_contract != none ]]; then
    printf '%s\n' "$runtime_contract" >"$stage/runtime-contract"
fi
identity=$(printf '%s\n%s\n%s\n%s\n' "$kind" "$published_binary_hash" "$resource_hash" "$runtime_contract" | sha256sum | cut -d' ' -f1)
published="$bundle_root/$identity"
if [[ -e $published ]]; then
    "$VERIFY" "$published" >/dev/null
    rm -rf "$stage"
else
    mv "$stage" "$published"
    "$VERIFY" "$published" >/dev/null
fi
printf '%s\n' "$published" >"$pointer_tmp"
mv -f "$pointer_tmp" "$pointer"
resolved=$("$VERIFY" "$pointer")
[[ $resolved == "$published" ]] || fail "published pointer resolved to $resolved, expected $published"
printf 'published Hermit E2E artifact kind=%s identity=%s path=%s\n' "$kind" "$identity" "$published"
