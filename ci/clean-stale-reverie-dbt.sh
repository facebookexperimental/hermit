#!/bin/bash
# Clean reverie-dbt only when Cargo would trust a DynamoRIO install that is gone.
#
# usage: ci/clean-stale-reverie-dbt.sh PROFILE
#        ci/clean-stale-reverie-dbt.sh --self-test
#
# reverie-dbt's build script bakes the absolute path of its DynamoRIO install
# into the crate (REVERIE_DBT_DYNAMORIO_HOME, _CMAKE, _DRRUN), and stores the
# install apart from Cargo's own state, in reverie-dbt-native-cache. A target
# whose Cargo state was restored without that directory (hosted CI's cache,
# 3af003724) still looks fresh, so the build script never reruns to recreate
# the install. build.workspace_compile_in_pinned_root used to clean
# reverie-dbt on every run for that reason, which also rebuilt detcore-dbt,
# hermit and the cdylibs above it.
#
# This keeps the protection and drops the cost: for every recorded run of
# reverie-dbt's build script under PROFILE, it checks the install that run
# names against the install's own attestation, the same evidence the build
# script checks before it reuses a cache entry:
#   - .reverie-dbt-install.provenance exists;
#   - every "sha256:HASH PATH" line of .reverie-dbt-install.sha256 matches the
#     file's content, and every "symlink TARGET PATH" line its link;
#   - the install holds nothing the manifest does not list.
# It cleans reverie-dbt if any install fails, or if a recorded run names no
# install. A target with no recorded run needs no clean: the build script runs.
set -uo pipefail

ATTESTATION=.reverie-dbt-install.sha256
PROVENANCE=.reverie-dbt-install.provenance

install_intact() { # install_intact DIR: whether DIR matches its attestation
    local dir=$1 kind first rest listed=0 actual
    [[ -d $dir && -f $dir/$ATTESTATION && -f $dir/$PROVENANCE ]] || return 1
    while IFS=' ' read -r first rest; do
        [[ -n $first ]] || continue
        listed=$((listed + 1))
        case $first in
            sha256:*)
                [[ -f $dir/$rest && ! -L $dir/$rest ]] || return 1
                actual=$(sha256sum -- "$dir/$rest" | cut -d' ' -f1) || return 1
                [[ $actual == "${first#sha256:}" ]] || return 1
                ;;
            symlink)
                # "symlink TARGET PATH": neither contains a space in DynamoRIO's install.
                kind=${rest%% *}
                [[ -L $dir/${rest#* } && $(readlink -- "$dir/${rest#* }") == "$kind" ]] || return 1
                ;;
            *) return 1 ;;
        esac
    done <"$dir/$ATTESTATION"
    actual=$(find "$dir" \( -type f -o -type l \) ! -name "$ATTESTATION" ! -name "$PROVENANCE" | wc -l)
    [[ $actual -eq $listed ]]
}

stale_runs() { # stale_runs TARGET PROFILE: print each recorded run whose install is not intact
    local out home
    shopt -s nullglob
    for out in "$1/$2"/build/reverie-dbt-*/output; do
        home=$(sed -n 's/^cargo:rustc-env=REVERIE_DBT_DYNAMORIO_HOME=//p' "$out" | tail -1)
        if [[ -z $home ]]; then
            printf '%s names no DynamoRIO install\n' "$out"
        elif ! install_intact "$home"; then
            printf '%s names %s, which does not match its attestation\n' "$out" "$home"
        fi
    done
}

main() {
    local profile=$1 target=${CARGO_TARGET_DIR:-target} stale
    stale=$(stale_runs "$target" "$profile")
    if [[ -z $stale ]]; then
        echo "clean-stale-reverie-dbt: every recorded DynamoRIO install under $target/$profile matches its attestation; reverie-dbt kept"
        return 0
    fi
    printf 'clean-stale-reverie-dbt: %s\n' "$stale"
    echo "clean-stale-reverie-dbt: cleaning reverie-dbt so its build script recreates the install"
    cargo clean --profile "$profile" -p reverie-dbt
}

self_test() {
    local dir
    dir=$(mktemp -d "${TMPDIR:-/tmp}/clean-stale-reverie-dbt-test.XXXXXX") || return 2
    trap 'rm -rf -- "$dir"' RETURN
    local install=$dir/t/validate/reverie-dbt-native-cache/key/install
    mkdir -p "$install/bin64" "$dir/t/validate/build/reverie-dbt-1"
    printf 'drrun' >"$install/bin64/drrun"
    ln -s drrun "$install/bin64/drrun-link"
    printf 'sha256:%s bin64/drrun\nsymlink drrun bin64/drrun-link\n' \
        "$(sha256sum "$install/bin64/drrun" | cut -d' ' -f1)" >"$install/$ATTESTATION"
    printf 'schema=x\n' >"$install/$PROVENANCE"
    printf 'cargo:rustc-env=REVERIE_DBT_DYNAMORIO_HOME=%s\n' "$install" >"$dir/t/validate/build/reverie-dbt-1/output"
    check() { # check NAME WANT_STALE
        local got
        got=$(stale_runs "$dir/t" validate)
        if { [[ -n $got ]] && [[ $2 == stale ]]; } || { [[ -z $got ]] && [[ $2 == intact ]]; }; then
            return 0
        fi
        echo "clean-stale-reverie-dbt self-test FAILED: $1: got '${got}' (want $2)" >&2
        return 1
    }
    check "an intact install is kept" intact || return 1
    printf 'changed' >"$install/bin64/drrun"
    check "a changed file is stale" stale || return 1
    printf 'drrun' >"$install/bin64/drrun"
    printf 'extra' >"$install/bin64/extra"
    check "an unlisted file is stale" stale || return 1
    rm -f "$install/bin64/extra"
    ln -sfn other "$install/bin64/drrun-link"
    check "a retargeted link is stale" stale || return 1
    ln -sfn drrun "$install/bin64/drrun-link"
    check "the restored install is intact again" intact || return 1
    rm -f "$install/$PROVENANCE"
    check "a missing provenance is stale" stale || return 1
    printf 'schema=x\n' >"$install/$PROVENANCE"
    rm -rf "$dir/t/validate/reverie-dbt-native-cache"
    check "a missing install is stale" stale || return 1
    printf 'cargo:rustc-link-lib=x\n' >"$dir/t/validate/build/reverie-dbt-1/output"
    check "a run that names no install is stale" stale || return 1
    rm -rf "$dir/t/validate/build"
    check "no recorded run needs no clean" intact || return 1
    echo "clean-stale-reverie-dbt: self-test PASS (intact, changed, unlisted, retargeted, restored, no provenance, missing, unnamed, none recorded)"
}

case ${1:-} in
    --self-test) self_test; exit $? ;;
    "") echo "usage: ci/clean-stale-reverie-dbt.sh PROFILE | --self-test" >&2; exit 2 ;;
    *) main "$1"; exit $? ;;
esac
