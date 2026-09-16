#!/usr/bin/env bash
# Run local validate as TWO PHASES separated by a network boundary:
#
#   FETCH phase   -- on the host, WITH network, and it does nothing but
#                    download. Locked fetches for every Cargo workspace and the
#                    generated rust-script workspace used by the offline phase
#                    populate one CARGO_HOME and produce no build output at all.
#
#   OFFLINE phase -- inside the nix-pinned root, with NO network. BUILD AND TEST
#                    BOTH RUN HERE, against the fetched cache and the pinned
#                    toolchain.
#
# WHY THIS SHAPE. The earlier version of this script put the boundary between
# build and test, which left the build with network. Shrinking the network
# window to a pure download is strictly better, and the reason is worth stating
# precisely: `cargo fetch --locked` cannot introduce variance, because every
# byte it writes is checked against the selected Cargo.lock -- exact versions
# AND content checksums for registry crates, exact revisions for git
# dependencies. A phase
# whose ENTIRE OUTPUT IS CHECKSUM-VERIFIED is a far smaller trust surface than a
# build phase that merely happens to have network available. Nothing can enter
# the build from the network except bytes that already matched a hash.
#
# So the honest claim is now stronger than it was:
#   * the COMPILER is pinned      -- the offline phase runs in the nix root
#   * the CRATES are pinned       -- Cargo.lock versions + checksums
#   * the BUILD cannot reach out  -- --network=none, asserted from inside
#   * the TESTS cannot reach out  -- same phase, same assertion
# What is NOT claimed: that the fetch phase needs no upstreams. It does. It just
# cannot lie to us about what it got.
#
# The canonical host-side validate plan calls `--fetch-only` once, then wraps
# each build/test DAG node in the pinned root. Invoking this script without
# `--fetch-only` remains the explicit whole-split diagnostic path, where one
# container drives the selected node sequence itself.
#
# ---------------------------------------------------------------------------
# WHERE THE NODE SETS COME FROM -- and why they are not invented here.
#
# The build/test partition already exists in ci/portable-shards.json, and GitHub
# CI already runs it as separate jobs: build jobs publish a prebuilt tree, then
# test, E2E, and final-result jobs consume it. This script reads THE SAME KEYS
# with THE SAME jq expressions as .github/workflows/ci-portable.yml. Completeness
# is checked against the committed hosted-portable selection, never a generated lane
# file, so plan-construction changes cannot silently fall out of this path.
#
# THE PARTITION IS THE SHARD MAP, NOT THE `group` FIELD. A naive implementation
# gets this wrong in both directions:
#   * the strict compatibility marker has group "test" but expands into direct
#     `compat.*` nodes after its other test-node predecessors in a separate
#     hosted job;
#   * preflight, check, setup, and E2E audit nodes are not group "build", but
#     they execute before the remaining test side.
# Read the map rather than reconstructing either partition from tag prefixes.
#
# HOW THIS DIFFERS FROM GITHUB, STATED PLAINLY. GitHub's split is for WALL CLOCK
# -- build once, fan out -- not for network. Its shard jobs have full network
# and restore a cargo cache with `Swatinem/rust-cache`; the prebuilt tarball
# carries only binaries, not target/debug/deps, so the shards genuinely do
# compile. GitHub enforces no network boundary anywhere. This script mirrors
# GitHub's node sets and their order, then adds a boundary GitHub does not have.
# Nothing here changes or weakens the GitHub lane.
#
# WHAT CROSSES THE BOUNDARY: two directories under <out>.
#   <out>/cargo    the fetched CARGO_HOME  (the fetch phase's only output)
#   <out>/target   build outputs           (CARGO_TARGET_DIR, written offline)
#
# THE FAILURE MODE THIS BUYS, which is the useful part: if the fetch phase did
# not populate correctly, the offline phase fails immediately and loudly at
# `cargo metadata` (exit 101 on the pinned reverie git dependency) instead of
# silently reaching out to the network. Loud and early beats quiet and wrong.
#
#   usage: run-split-validate.sh [options]
#     --lane LANE        portable (default) | privileged
#     --out DIR          phase-boundary directory (default ignored/hermetic/split)
#     --shards a,b,c     partial/debug run: only these test shard slugs; skips
#                        the full e2e.manifest_* population (default: all)
#     --fetch-only       run the fetch phase and stop
#     --offline-only     run the offline phase only (fetch must have run before)
#     --seed-cargo DIR   warm-start the CARGO_HOME by reflink from DIR (usually
#                        ~/.cargo) before fetching. An OPTIMISATION only: the
#                        fetch phase still runs and still reconciles against
#                        Cargo.lock, this just avoids redownloading what the host
#                        already has. ci-hub's validate does the same thing
#                        (ci-hub/validate/start_unit.py), so it is a pattern to
#                        copy rather than invent.
#     --dry-run          print the phases and node sets, run nothing

set -euo pipefail

HERE=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
ROOT=$(cd -- "$HERE/../.." && pwd)

lane=portable
out="$ROOT/ignored/hermetic/split"
shards=""
do_fetch=1; do_offline=1; dry=0; seed=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        --lane) lane=$2; shift 2 ;;
        --out) out=$2; shift 2 ;;
        --shards) shards=$2; shift 2 ;;
        --fetch-only) do_offline=0; shift ;;
        --offline-only) do_fetch=0; shift ;;
        --seed-cargo) seed=$2; shift 2 ;;
        --dry-run) dry=1; shift ;;
        *) echo "run-split-validate: unexpected argument '$1'" >&2; exit 2 ;;
    esac
done

if [[ $do_fetch -eq 0 && $do_offline -eq 0 ]]; then
    echo "run-split-validate: --fetch-only and --offline-only cannot be combined" >&2
    exit 2
fi

[[ "$lane" == portable ]] || {
    echo "run-split-validate: only the portable lane has a shard map today (got '$lane')." >&2
    echo "  ci/portable-shards.json is what defines the node sets; there is no" >&2
    echo "  privileged-shards.json, so a privileged split would have to invent one." >&2
    exit 2
}

MAP="$ROOT/ci/portable-shards.json"
EXPECTED_E2E_PLAN="$ROOT/ci/expected-e2e-plan.json"
FETCH_MANIFESTS=(
    Cargo.toml
    liteinst-runtime-build/Cargo.toml
    agent-utils/rs/Cargo.toml
)
[[ -f "$MAP" ]] || { echo "run-split-validate: missing $MAP" >&2; exit 2; }
[[ -f "$EXPECTED_E2E_PLAN" ]] || {
    echo "run-split-validate: missing $EXPECTED_E2E_PLAN" >&2
    exit 2
}

# Read the same assignment fields ci-portable.yml uses. This whole-split path
# groups the hosted preflight and check jobs with the build jobs before running
# the remaining test, E2E, and final steps.
build_nodes=$(jq -r '(
    .preflight_nodes
  + .check_nodes
  + .build_debug_nodes
  + .build_dbt_nodes
  + .build_aux_nodes
)|join(",")' "$MAP")

if [[ -n "$shards" ]]; then
    shard_nodes=$(jq -r --arg sel "$shards" \
        '($sel|split(",")) as $s
         | ([ (.debug_shards[], .release_shards[])
                | select(.slug as $x | $s | index($x))
                | .nodes[] ]
            + (if ($s | index("strict-compat")) == null
               then []
               else .strict_compat_nodes
               end))
         | join(",")' "$MAP")
    [[ -n "$shard_nodes" ]] || {
        echo "run-split-validate: no shard matched '$shards'. Known slugs:" >&2
        jq -r '(.debug_shards[], .release_shards[]).slug | "  " + .' "$MAP" >&2
        jq -r 'select((.strict_compat_nodes // []) | length > 0) | "  strict-compat"' "$MAP" >&2
        exit 2
    }
    test_nodes=$shard_nodes
else
    shard_nodes=$(jq -r '[ (.debug_shards[], .release_shards[]).nodes[] ]|join(",")' "$MAP")
    test_nodes=$(jq -r '[
        (.debug_shards[], .release_shards[]).nodes[],
        .strict_compat_nodes[],
        .e2e_nodes[],
        .final_nodes[]
    ]|join(",")' "$MAP")
fi
shard_node_count=$(tr ',' '\n' <<<"$shard_nodes" | wc -l)

# The constructed plan and the shard map both carry the manifest steps. Keep
# --shards useful for focused debugging by counting E2E only on the default full
# run.
e2e_nodes=""
e2e_node_count=0
strict_compat_node_count=0
final_node_count=0
e2e_cell_count=0
if [[ -z "$shards" ]]; then
    e2e_nodes=$(jq -er '.e2e_nodes | if length > 0 then join(",") else error("no e2e.manifest_* steps") end' "$MAP")
    e2e_node_count=$(tr ',' '\n' <<<"$e2e_nodes" | wc -l)
    strict_compat_node_count=$(jq -er '.strict_compat_nodes | if length > 0 then length else error("no strict compatibility steps") end' "$MAP")
    final_node_count=$(jq -er '.final_nodes | if length > 0 then length else error("no final steps") end' "$MAP")
    e2e_cell_count=$(jq -er '
        [.cells[] | select(.lane == "portable")] as $portable
        | if (.schema == 1 and ($portable | length) > 0)
          then $portable | length
          else error("invalid or empty portable E2E plan")
          end
    ' "$EXPECTED_E2E_PLAN")
fi

build_node_count=$(tr ',' '\n' <<<"$build_nodes" | wc -l)
test_node_count=$(tr ',' '\n' <<<"$test_nodes" | wc -l)
total_node_count=$((build_node_count + test_node_count))
if [[ -z "$shards" ]]; then
    plan_out=$(mktemp)
    ./scripts/validate.rs --hosted-portable-only --show-plan-json \
        --skip-inner-dirty-working-tree-and-rebase-freshness-checks >"$plan_out"
    plan_json=$(sed -n '1p' "$plan_out")
    rm -f "$plan_out"
    strict_alias_count=$(tr ',' '\n' <<<"$build_nodes,$test_nodes" |
        grep -Fxc 'test.strict_compat' || true)
    [[ $strict_alias_count -eq 1 ]] || {
        echo "run-split-validate: shard map has $strict_alias_count test.strict_compat aliases; expected exactly one." >&2
        exit 1
    }
    compat_expansion=$(jq -r '
        .dags[].steps[].tag
        | select(. == "compatprep.fixtures" or . == "compatprep.fixtures_on_host" or startswith("compat."))
    ' <<<"$plan_json")
    [[ -n "$compat_expansion" ]] || {
        echo "run-split-validate: constructed plan has no direct strict compatibility nodes." >&2
        exit 1
    }
    # The shard map deliberately retains the stable `test.strict_compat`
    # selection alias. Validate expands that alias at execution time; expand it
    # here too before comparing the partition with the constructed graph.
    selected_list=$(
        {
            # Match validate's exact-name-first hosted selector resolution.
            # Preserve unknown names and duplicates for the checks below.
            tr ',' '\n' <<<"$build_nodes,$test_nodes" | grep -Fvx 'test.strict_compat' |
                jq -Rr --argjson available "$(jq '[.dags[].steps[].tag]' <<<"$plan_json")" '
                    . as $tag | ($tag + "_on_host") as $hosted
                    | if ($available | index($tag)) == null and ($available | index($hosted)) != null
                      then $hosted else $tag end
                '
            printf '%s\n' "$compat_expansion"
        } | LC_ALL=C sort
    )
    duplicate_nodes=$(LC_ALL=C uniq -d <<<"$selected_list" || true)
    strict_compat_node_count=$(wc -l <<<"$compat_expansion")
    test_node_count=$((test_node_count - 1 + strict_compat_node_count))
    total_node_count=$((build_node_count + test_node_count))
    expected_list=$(jq -r '.dags[].steps[].tag' <<<"$plan_json" | LC_ALL=C sort)
    duplicate_dag_nodes=$(LC_ALL=C uniq -d <<<"$expected_list" || true)
    selected_unique=$(LC_ALL=C uniq <<<"$selected_list")
    expected_unique=$(LC_ALL=C uniq <<<"$expected_list")
    missing_nodes=$(LC_ALL=C comm -23 <(printf '%s\n' "$expected_unique") <(printf '%s\n' "$selected_unique") || true)
    extra_nodes=$(LC_ALL=C comm -13 <(printf '%s\n' "$expected_unique") <(printf '%s\n' "$selected_unique") || true)
    if [[ -n "$duplicate_nodes" || -n "$duplicate_dag_nodes" || -n "$missing_nodes" || -n "$extra_nodes" ]]; then
        echo "run-split-validate: full portable step selection does not exactly match validate's constructed plan." >&2
        [[ -z "$duplicate_nodes" ]] || printf '  duplicate selection: %s\n' $duplicate_nodes >&2
        [[ -z "$duplicate_dag_nodes" ]] || printf '  duplicate constructed step: %s\n' $duplicate_dag_nodes >&2
        [[ -z "$missing_nodes" ]] || printf '  missing: %s\n' $missing_nodes >&2
        [[ -z "$extra_nodes" ]] || printf '  extra: %s\n' $extra_nodes >&2
        exit 1
    fi
fi

cargo_home="$out/cargo"
target_dir="$out/target"

echo "== phase boundary: $out"
if [[ $do_fetch -eq 1 ]]; then
    echo "== FETCH phase   (host, WITH network): cargo fetch --locked, no build output"
fi
if [[ $do_offline -eq 1 ]]; then
    echo "== OFFLINE phase (pinned root, NO network): build then test, in one place"
    echo "     build-side: $build_node_count node(s)"
    if [[ -n "$shards" ]]; then
        echo "     test-side:  $test_node_count selected node(s)"
    else
        echo "     test-side:  $test_node_count node(s) ($shard_node_count shard + $strict_compat_node_count strict compatibility + $e2e_node_count manifest + $final_node_count final)"
    fi
    if [[ -n "$e2e_nodes" ]]; then
        echo "     e2e cells:  $e2e_cell_count selected portable cell(s)"
    fi
fi

if [[ $dry -eq 1 ]]; then
    if [[ $do_fetch -eq 1 ]]; then
        echo
        echo "-- fetch phase would run, on the host:"
        for manifest in "${FETCH_MANIFESTS[@]}"; do
            if [[ "$manifest" == agent-utils/rs/Cargo.toml ]]; then
                echo "   # Agent Utils: neutral cwd=/; absolute Cargo home; no consumer target overrides"
                echo "   CARGO_HOME=$cargo_home cargo fetch --locked --manifest-path $ROOT/$manifest"
            else
                echo "   CARGO_HOME=$cargo_home cargo fetch --locked --manifest-path $manifest"
            fi
        done
        echo "   CARGO_HOME=$cargo_home ./ci/prepare-rust-scripts.sh --fetch-only"
    fi
    if [[ $do_offline -eq 1 ]]; then
        echo
        echo "-- offline phase would run, inside the pinned root, --network=none:"
        echo "   ci/hermetic/assert-no-network.sh"
        echo "   verify pinned developer tools, build dependencies and required guest commands"
        echo "   ci/run-node.sh $lane $build_nodes"
        echo "   ci/run-node.sh $lane $test_nodes"
    fi
    exit 0
fi

mkdir -p "$cargo_home" "$target_dir"

if [[ $do_fetch -eq 1 ]]; then
    echo
    echo ":::: FETCH PHASE -- host, network ALLOWED, download only"
    # Asserted, not assumed. Without network the fetch silently produces an
    # incomplete cache and the offline phase fails later for a confusing reason.
    if ! "$HERE/assert-no-network.sh" --expect-network; then
        echo "run-split-validate: the fetch phase needs network -- github.com for the" >&2
        echo "  pinned reverie git dependency and crates.io for the registry -- and" >&2
        echo "  this host has none. Refusing to start a fetch that cannot complete." >&2
        exit 1
    fi

    # Warm-start is optional and never fatal. --reflink=auto, not =always: a
    # non-CoW filesystem must still work, just slower. Failing here would trade
    # a slow fetch for an outage, and the fetch phase has network anyway.
    if [[ -n "$seed" ]]; then
        for sub in registry git/db; do
            if [[ -d "$seed/$sub" && ! -e "$cargo_home/$sub" ]]; then
                mkdir -p "$(dirname "$cargo_home/$sub")"
                if cp -a --reflink=auto "$seed/$sub" "$cargo_home/$sub"; then
                    echo ":: warm-started CARGO_HOME/$sub from $seed"
                else
                    echo ":: could not warm-start $sub from $seed; fetching it instead" >&2
                fi
            fi
        done
    fi

    # --locked is the point of the phase: resolve to EXACTLY each workspace's
    # Cargo.lock or fail. LiteInst and Agent Utils have separate workspaces;
    # the root lock does not include every member of either workspace.
    (
        cd "$ROOT"
        for manifest in "${FETCH_MANIFESTS[@]}"; do
            if [[ "$manifest" == agent-utils/rs/Cargo.toml ]]; then
                # Match rs/bin/cargo-runner's Cargo configuration scope. Cargo
                # discovers config from its process cwd, not --manifest-path;
                # Hermit's target/toolchain config must not redirect this fetch.
                # Resolve paths before leaving the repository, retaining cargo's
                # executable name (it may be a rustup symlink).
                (
                    cargo_bin=$(command -v cargo)
                    [[ "$cargo_bin" == /* ]] || cargo_bin="$PWD/$cargo_bin"
                    absolute_cargo_home=$(realpath -- "$cargo_home")
                    cd /
                    env -u CARGO_BUILD_TARGET -u CARGO_TARGET_DIR \
                        CARGO_HOME="$absolute_cargo_home" "$cargo_bin" fetch --locked \
                        --manifest-path "$ROOT/$manifest"
                )
            else
                CARGO_HOME="$cargo_home" cargo fetch --locked --manifest-path "$manifest"
            fi
        done
        CARGO_HOME="$cargo_home" ./ci/prepare-rust-scripts.sh --fetch-only
    )
    echo ":::: FETCH PHASE complete -- every byte checked against its Cargo.lock"
fi

if [[ $do_offline -eq 1 ]]; then
    echo
    echo ":::: OFFLINE PHASE -- pinned root, network REFUSED, build AND test"
    [[ -d "$cargo_home/registry" ]] || {
        echo "run-split-validate: $cargo_home has no registry; the fetch phase has not run." >&2
        echo "  The offline phase has no network and cannot populate it itself." >&2
        exit 2
    }
    # The assertion runs INSIDE the container as its first act, and a reachable
    # network aborts the phase before anything is built or tested. Checking from
    # out here would prove nothing about in there.
    # Keep the two machine timeout settings independent across this boundary.
    # run-in-pinned-root omits an unset name and otherwise preserves its value;
    # the manifest runner remains the single parser and policy authority, so a
    # malformed setting is refused there rather than reinterpreted in shell.
    exec "$HERE/run-in-pinned-root.sh" \
        --src "$ROOT" --out "$out" --src-rw --cargo-home "$cargo_home" \
        --env HERMIT_TEST_CPU_TIMEOUT_MULTIPLIER \
        --env HERMIT_TEST_WALL_TIMEOUT_MULTIPLIER \
        -- bash -c '
            set -euo pipefail
            /src/ci/hermetic/assert-no-network.sh
            export HERMIT_E2E_EMPTY_WORKDIR=/test

            # Fail before a DAG node can report a misleading product failure.
            # Build dependencies and guest tools are different populations. The
            # first assertion names the 18 executables and four native libraries
            # used to compile and stage Hermit and its backend resources; the loop
            # below remains the audit of commands selected cells run as guests.
            /src/ci/hermetic/assert-build-dependencies.sh
            rust_script_actual=$(rust-script --version)
            [[ "$rust_script_actual" == *" ${HERMIT_RUST_SCRIPT_VERSION}"* ]] || {
                echo "run-split-validate: rust-script version mismatch: $rust_script_actual" >&2
                exit 2
            }
            nextest_actual=$(cargo-nextest --version)
            [[ "$nextest_actual" == *" ${HERMIT_CARGO_NEXTEST_VERSION}"* ]] || {
                echo "run-split-validate: cargo-nextest version mismatch: $nextest_actual" >&2
                exit 2
            }
            for tool in ar bash cc c++ du find gawk hexdump jq lua m4 mcookie node \
                        openssl perl ps python3 ruby rustc sqlite3 ssh-keygen tclsh \
                        uuidgen zstd; do
                command -v "$tool" >/dev/null || {
                    echo "run-split-validate: pinned root is missing required tool: $tool" >&2
                    exit 2
                }
            done
            for path in /usr/bin/bash /usr/bin/date /usr/bin/df /usr/bin/du \
                        /usr/bin/find /usr/bin/git /usr/bin/node /usr/bin/nodejs \
                        /usr/bin/nproc /usr/bin/python3 /usr/bin/sort \
                        /usr/bin/stat /usr/bin/tr; do
                [[ -x "$path" ]] || {
                    echo "run-split-validate: pinned root is missing required FHS path: $path" >&2
                    exit 2
                }
            done

            # These CLI test drivers are distinct from the guest-tool list.
            command -v gdb >/dev/null || {
                echo "run-split-validate: pinned root is missing required CLI test driver: gdb" >&2
                exit 2
            }
            gdb_python=$(timeout 10s gdb --batch --nx \
                -ex "python import sys; print(2694001)") || {
                echo "run-split-validate: pinned gdb cannot run the required Python fixture" >&2
                exit 2
            }
            [[ "$gdb_python" == "2694001" ]] || {
                echo "run-split-validate: pinned gdb Python fixture marker was not exact" >&2
                exit 2
            }

            echo ":: build-side nodes"
            /src/ci/run-node.sh '"$lane"' '"$build_nodes"'
            echo ":: test-side nodes ('"$shard_node_count"' shard + '"$e2e_node_count"' manifest + '"$final_node_count"' final; '"$e2e_cell_count"' selected portable cells)"
            exec /src/ci/run-node.sh '"$lane"' '"$test_nodes"'
        '
fi
