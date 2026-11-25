# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# Where the Buck-only run's buckconfig comes from (ci/buck-e2e/validate-node, buck-stock):
# only this commit's tracked buckconfig files, the buckconfig bundled into the pinned buck2,
# and ci/buck-e2e/stock-runner.buckconfig. No machine-local buckconfig, no settings file,
# and no untracked or linked project buckconfig such as a shared-cache .buckconfig.local.
# Sourced by validate-node and by ci/buck-e2e/test-buckconfig-sources.sh.
# shellcheck shell=bash
# shellcheck disable=SC2154 # buck2, root and step come from the sourcing script
#
# Uses the caller's root (the checkout), buck2 and step. Each refusal prints why and exits 2.

stock_config_rel=ci/buck-e2e/stock-runner.buckconfig

# Export the two buck2 knobs every buck2 of a Buck-only run gets, the daemon included:
# BUCK2_TEST_SKIP_DEFAULT_EXTERNAL_CONFIG=1 skips /etc/buckconfig.d/*, /etc/buckconfig,
# ~/.buckconfig.d/* and ~/.buckconfig.local; BUCK2_TEST_EXTRA_EXTERNAL_CONFIG adds the run's
# own buckconfig file, which no other mode reads. Other buck2 test knobs
# reach buck2 outside any buckconfig, so they refuse.
export_stock_buckconfig_env() {
    local name
    for name in $(compgen -e); do
        case $name in
            BUCK2_TEST_SKIP_DEFAULT_EXTERNAL_CONFIG | BUCK2_TEST_EXTRA_EXTERNAL_CONFIG) ;;
            BUCK2_TEST_*)
                echo "validate-node: buck-stock refuses $name in its environment: it changes buck2 outside the checkout's buckconfig; unset it" >&2
                exit 2
                ;;
        esac
    done
    export BUCK2_TEST_SKIP_DEFAULT_EXTERNAL_CONFIG=1
    export BUCK2_TEST_EXTRA_EXTERNAL_CONFIG="$root/$stock_config_rel"
}

# One line per machine-local file buck2 would otherwise read (the knob above skips the
# buckconfigs; refuse_untracked_buck_config refuses the settings files): path, sha256, bytes;
# or path, absent.
record_external_buckconfigs() {
    local file
    for file in /etc/buckconfig.d/* /etc/buckconfig "$HOME"/.buckconfig.d/* "$HOME/.buckconfig.local" \
        "$HOME/.bucksettings.toml" "$HOME/.bucksettings.local.toml"; do
        if [[ -f $file && -r $file ]]; then
            printf '%s\t%s\t%s\n' "$file" "$(sha256sum <"$file" | cut -c1-64)" "$(stat -c %s -- "$file")"
        elif [[ -e $file ]]; then
            printf '%s\tunreadable\n' "$file"
        elif [[ $file != *'*' ]]; then
            printf '%s\tabsent\n' "$file"
        fi
    done
}

# Every read of HEAD here ignores refs/replace, which could make HEAD name other objects.
head_git() { git --no-replace-objects "$@"; }

# head_repo TOP: 0 if the git top level TOP is the checkout itself, or a submodule whose
# superproject records it as a gitlink at HEAD, checked out at exactly that commit, up to the
# checkout. A nested repository that is not such a submodule, or a submodule moved off its
# gitlink, would judge a file against a HEAD this commit does not name, so it refuses.
head_repo() {
    local top=$1 super entry mode type recorded
    [[ $top == "$(realpath -e -- "$root")" ]] && return 0
    super=$(head_git -C "$top" rev-parse --show-superproject-working-tree 2>/dev/null) || return 1
    [[ -n $super && $top == "$super"/* ]] || return 1
    entry=$(head_git -C "$super" ls-tree HEAD -- "${top#"$super"/}" 2>/dev/null) || return 1
    read -r mode type recorded _ <<<"$entry"
    [[ $mode == 160000 && $type == commit ]] || return 1
    [[ $(head_git -C "$top" rev-parse --verify -q HEAD 2>/dev/null) == "$recorded" ]] || return 1
    head_repo "$super"
}

# head_file REL: 0 if REL, relative to $root, is a regular file of HEAD in the repository that
# holds it (a submodule's HEAD for a path inside one), with HEAD's bytes, reached through no
# symlink. The bytes are hashed raw (--no-filters): a clean filter or autocrlf setting could
# otherwise make other bytes hash to HEAD's blob.
head_file() {
    local rel=$1 abs dir base top entry mode type blob name
    [[ $rel != /* && /$rel/ != */../* && /$rel/ != */./* ]] || return 1
    abs=$root/$rel
    [[ -f $abs ]] || return 1
    [[ $(realpath -e -- "$abs" 2>/dev/null) == "$(realpath -e -- "$root")/$rel" ]] || return 1
    dir=$(dirname -- "$abs") base=$(basename -- "$abs")
    top=$(head_git -C "$dir" rev-parse --show-toplevel 2>/dev/null) || return 1
    head_repo "$top" || return 1
    entry=$(head_git -C "$dir" ls-tree HEAD -- "$base" 2>/dev/null) || return 1
    read -r mode type blob name <<<"$entry"
    [[ ($mode == 100644 || $mode == 100755) && $type == blob && $name == "$base" ]] || return 1
    [[ $(head_git -C "$dir" hash-object --no-filters -- "$base" 2>/dev/null) == "$blob" ]]
}

# The cell roots that HEAD's .buckconfig marks `bundled` in [external_cells]: buck2 serves
# their buckconfig from its own binary, so a location under one is the pinned buck2's.
bundled_cell_roots() {
    head_git -C "$root" show HEAD:.buckconfig 2>/dev/null | awk '
        /^[[:space:]]*\[/ { section = $0; gsub(/[][[:space:]]/, "", section); next }
        /^[[:space:]]*[#;]/ || !/=/ { next }
        {
            key = $0; sub(/=.*/, "", key); gsub(/[[:space:]]/, "", key)
            value = $0; sub(/^[^=]*=/, "", value); gsub(/[[:space:]]/, "", value)
            if (section == "cells") cell[key] = value
            else if (section == "external_cells" && value == "bundled") bundled[key] = 1
        }
        END { for (key in bundled) if (key in cell) print cell[key] }'
}

# A location under a bundled cell root that the checkout does not have, so that buck2 can
# only have read it from its binary.
bundled_location() {
    local rel=$1 cell
    while read -r cell; do
        [[ -n $cell && $rel == "$cell"/* && ! -e $root/$cell ]] && return 0
    done < <(bundled_cell_roots)
    return 1
}

# A line for the records: the path, what it resolves to, and the sha256 of what buck2 read.
describe_config_file() {
    local path=$1 target sum
    target=$(realpath -e -- "$path" 2>/dev/null) || target=missing
    if [[ -f $path && -r $path ]]; then sum=$(sha256sum <"$path" | cut -c1-64); else sum=unreadable; fi
    if [[ -L $path ]]; then target="symlink to $target"; else target="resolves to $target"; fi
    printf '%s\t%s\t%s\n' "$path" "$sum" "$target"
}

# Right after the knobs are exported, in every form of the node and before any buck2 runs:
# buck2 settings files and the checkout's own untracked or linked buckconfig (the
# shared-cache .buckconfig.local link is the usual one), named with what to do.
refuse_untracked_buck_config() {
    local file found=()
    for file in "$HOME/.bucksettings.toml" "$HOME/.bucksettings.local.toml"; do
        [[ ! -e $file ]] || found+=("$file (a buck2 settings file, which no buckconfig check sees)")
    done
    for file in .bucksettings.toml .bucksettings.local.toml .buckconfig.local; do
        [[ -e $root/$file || -L $root/$file ]] || continue
        head_file "$file" && continue
        if [[ -L $root/$file ]]; then
            found+=("$root/$file -> $(readlink -- "$root/$file")")
        else
            found+=("$root/$file")
        fi
    done
    if [[ -e $root/.buckconfig.d && -z $(head_git -C "$root" ls-tree HEAD -- .buckconfig.d) ]]; then
        found+=("$root/.buckconfig.d (not in this commit)")
    fi
    ((${#found[@]} == 0)) && return 0
    echo "validate-node: buck-stock reads no buck2 configuration that is not this commit's, and this checkout or host has: ${found[*]}. A .buckconfig.local link from the shared action cache setup (docs/BUCK2_OSS.md) points builds at the host's cache: remove it from this checkout for a Buck-only run (rm .buckconfig.local). Nothing ran" >&2
    exit 2
}

# After the daemon starts: dump its buckconfig for every cell, with where each value is
# defined, to $1, append each refused file's line to $2, and refuse unless the dump has values,
# every location is a HEAD file, a bundled cell's, or the stock settings file, and both stock
# keys are defined in that file.
check_buckconfig_sources() {
    local dump=$1 records=$2 stock=$root/$stock_config_rel values parsed loc bad=() line key want got
    if ! timeout --foreground -k 5 300 "$buck2" audit config --client-metadata id=hermit-buckconfig-sources \
        --all-cells --location direct >"$dump"; then
        echo "validate-node: \`buck2 audit config\` failed or ran past 300 s (above), so this run cannot show where its buckconfig comes from; nothing ran" >&2
        exit 2
    fi
    values=$(grep -c '^  (defined at ' "$dump") || true
    if ((values == 0)); then
        echo "validate-node: \`buck2 audit config --location direct\` printed no value with its location ($dump), so this run cannot show where its buckconfig comes from; nothing ran" >&2
        exit 2
    fi
    # The same pattern as the location parse below: a location line it cannot parse would
    # otherwise skip the per-location rule unseen.
    parsed=$(grep -c '^  (defined at .*:[0-9]*)$' "$dump") || true
    if ((parsed != values)); then
        echo "validate-node: \`buck2 audit config --location direct\` printed $values location lines, but only $parsed have the form '(defined at PATH:LINE)' ($dump), so the others cannot be checked; nothing ran" >&2
        exit 2
    fi
    while read -r loc; do
        if [[ $loc == "$stock" ]]; then
            head_file "$stock_config_rel" && continue
        elif [[ $loc != /* ]]; then
            head_file "$loc" && continue
            bundled_location "$loc" && continue
            loc=$root/$loc
        fi
        bad+=("$loc")
        describe_config_file "$loc" >>"$records"
    done < <(sed -n 's/^  (defined at \(.*\):[0-9]*)$/\1/p' "$dump" | sort -u)
    if ((${#bad[@]})); then
        echo "validate-node: the buck2 daemon's buckconfig has values from files that are not this commit's (${bad[*]}; see $dump and $records): buck-stock reads only the checkout's tracked buckconfig, the buckconfig bundled into buck2 and $stock_config_rel; nothing ran" >&2
        exit 2
    fi
    for line in "buck2.sqlite_dep_file_state=false" "buck2_resource_control.status=if_available"; do
        key=${line%%=*} want=${line#*=}
        got=$(awk -v key="$key" '
            /^\[/ { section = substr($0, 2, length($0) - 2); next }
            /^    [^ ]/ { kv = substr($0, 5); next }
            /^  \(defined at / {
                loc = $0; sub(/^  \(defined at /, "", loc); sub(/:[0-9]+\)$/, "", loc)
                name = kv; sub(/ = .*/, "", name); value = kv; sub(/^[^=]* = /, "", value)
                if (section "." name == key) print value "@" loc
            }' "$dump" | sort -u)
        if [[ $got != "$want@$stock" ]]; then
            echo "validate-node: the buck2 daemon's $key is '${got:-unset}', not $want from $stock_config_rel; nothing ran" >&2
            exit 2
        fi
    done
    step "buck2 reads only this commit's buckconfig: $values values, every one from a tracked file, the bundled prelude or $stock_config_rel ($dump)"
}
