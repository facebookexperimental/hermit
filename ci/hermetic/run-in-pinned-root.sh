#!/usr/bin/env bash
# Run a command inside the nix-pinned validate root. The canonical host-side
# validate plan uses this wrapper for each build and test DAG node.
#
# The existing outer systemd-run, validate-lock, DAG scheduler, and cgroup
# policy stay on the host; this wrapper adds the pinned filesystem and network
# boundary without creating a second resource or cgroup layer. Each invocation
# is one privileged podman container pinned BY DIGEST, with /dev/kvm passed
# through, no runtime network, source at /src (read-only by default, explicitly
# writable for validation nodes), and separate writable output and target
# volumes.
#
# WHY BY DIGEST AND NOT BY TAG. A tag is mutable; a digest is the artifact that
# actually ran. The digest belongs in the receipt next to the flake.lock: the
# digest says what ran, the lock says how to rebuild it. See README.md.
#
#   usage: run-in-pinned-root.sh --src DIR --out DIR [--digest NAME@SHA]
#                                [--src-rw] [--cargo-home DIR] [--proc-locks-runtime] [--env NAME]... -- CMD...
#                                [--nextest-calibration] [--bundle-lower DIR]
#                                [--project-src DIR --project-out DIR --project-descriptor FILE]
#                                [--project-src-rw]
#          run-in-pinned-root.sh --check-image [--digest NAME@SHA]
#
# --check-image only reads the exact local image reference. It shares the late
# execution guard, uses the caller's Podman store, and creates no output paths.
#
# HERMIT_PINNED_IMAGE_PROBE_BOUND_S, when set, replaces the 120 s WALL bound on
# each of the exact-image probe's two attempts. It must be a positive integer
# number of seconds; any other value, an empty one included, is refused with
# status 2 before Podman runs. Production leaves it unset; the wrapper's own
# tests shorten it. It is an environment supplement rather than a flag because
# the late probe runs inside every pinned-root node's generated command line.
#
# --bundle-lower DIR mounts DIR READ-ONLY at /bundle-lower, for a command that
# mounts its own overlay with DIR as the lower layer (ci/buck-e2e/cell.sh shares
# one staged bundle across concurrent cells this way). A bind adds no link to
# DIR's files, so a guest's stat of one reports the same st_nlink however many
# cells share it; hard-linking DIR into --src did not (each live copy was one
# more link).
#
# --src-rw mounts the source WRITABLE. The default is read-only and stays that
# way, but a test phase legitimately writes into its own tree (target/ci,
# ignored/e2e/build), exactly as a GitHub shard job writes into its checkout.
# Read-only is the right default for a one-shot command; it is not a property
# the test phase can satisfy.
#
# --cargo-home mounts the registry and git caches from an already-populated
# CARGO_HOME. The test phase has no network BY DESIGN, so cargo must find those
# caches already present or it cannot even resolve the dependency graph. Host
# executables and configuration are deliberately not mounted: the image owns
# its toolchain and network policy. This is the local equivalent of the shard
# jobs' `Swatinem/rust-cache` restore, not a workaround: in both cases the cache
# is an input carried across the phase boundary.
#
# DAGRUN_TEST_COUNTS_PATH is scheduler-owned on the host. When requested through
# --env, its parent directory is mounted at /dagrun-test-counts and the child is
# given the translated path. This lets an in-container test producer atomically
# publish the same evidence file the host scheduler will read after podman exits.
#
# DAGRUN_ARTIFACT_READY_DIR is likewise scheduler-owned. When requested through
# --env, the directory is mounted at /dagrun-artifact-ready so an in-container
# producer can atomically publish the signal file the host scheduler polls
# while the producer continues running.

set -euo pipefail

HERE=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
DIGEST_FILE="$HERE/image.digest"

src=""; out=""; digest=""; src_mode="ro=true"; cargo_home=""; bundle_lower=""
project_src=""; project_out=""; project_descriptor=""; project_mode="ro=true"
proc_locks_runtime=false
check_image=false
nextest_calibration=false
pass_env=()
while [[ $# -gt 0 ]]; do
    case "$1" in
        --help|-h)
            printf '%s\n' 'usage: run-in-pinned-root.sh --src DIR --out DIR [--digest NAME@SHA] [--src-rw] [--cargo-home DIR] [--proc-locks-runtime] [--nextest-calibration] [--bundle-lower DIR] [--env NAME]... -- CMD...' \
                '--bundle-lower DIR mounts DIR read-only at /bundle-lower, as the lower layer of an overlay the command mounts itself.' \
                '       run-in-pinned-root.sh --check-image [--digest NAME@SHA]' \
                '--project-src/--project-out/--project-descriptor admit one distinct /project workspace and target; the original core stays at /src. --project-src-rw is separate from --src-rw.' \
                '--nextest-calibration captures the actual host envelope for the regular Nextest consumer using an existing prepared helper. Missing capture capability stays unqualified; bootstrap calls do not capture or compile.' \
                'HERMIT_PINNED_IMAGE_PROBE_BOUND_S=SECONDS replaces the 120 s WALL bound on each of the two exact-image probe attempts (a positive integer; unset: 120). Production leaves it unset.'
            exit 0 ;;
        --src) src=$2; shift 2 ;;
        --out) out=$2; shift 2 ;;
        --project-src) [[ -z $project_src ]] || { echo "duplicate --project-src" >&2; exit 2; }; project_src=$2; shift 2 ;;
        --project-out) [[ -z $project_out ]] || { echo "duplicate --project-out" >&2; exit 2; }; project_out=$2; shift 2 ;;
        --project-descriptor) [[ -z $project_descriptor ]] || { echo "duplicate --project-descriptor" >&2; exit 2; }; project_descriptor=$2; shift 2 ;;
        --project-src-rw) project_mode="ro=false"; shift ;;
        --digest) digest=$2; shift 2 ;;
        --check-image) check_image=true; shift ;;
        --src-rw) src_mode="ro=false"; shift ;;
        --cargo-home) cargo_home=$2; shift 2 ;;
        --proc-locks-runtime) proc_locks_runtime=true; shift ;;
        --nextest-calibration) nextest_calibration=true; shift ;;
        --bundle-lower) bundle_lower=$2; shift 2 ;;
        --env) pass_env+=("$2"); shift 2 ;;
        --) shift; break ;;
        *) echo "run-in-pinned-root: unexpected argument '$1'" >&2; exit 2 ;;
    esac
done

# $1 is "admission" for --check-image and "late" before a node's own run.
check_pinned_image() {
    local caller=$1
    if [[ -z "$digest" ]]; then
        [[ -f "$DIGEST_FILE" ]] || {
            echo "run-in-pinned-root: no --digest and no $DIGEST_FILE." >&2
            echo "  Build the image first: ci/hermetic/build-image.sh" >&2
            return 2
        }
        if ! digest=$(cat -- "$DIGEST_FILE"); then
            echo "run-in-pinned-root: cannot read exact image reference from $DIGEST_FILE." >&2
            return 2
        fi
    fi
    if [[ ! "$digest" =~ ^[^[:space:]@]+@sha256:[0-9a-f]{64}$ ]]; then
        echo "run-in-pinned-root: invalid exact image reference '$digest'; expected NAME@sha256:64-lowercase-hex." >&2
        return 2
    fi

    # Admission runs before any DAG node, so store inspection needs its own
    # bound. It is a WALL bound on a query that normally takes ~0.05 s, so it
    # is generous (120 s) and a timeout gets one bounded retry, logged: under
    # validate load a 10 s bound timed out (status 124) for hundreds of cells
    # whose images were present. Keep stderr and the actual status (including
    # timeout/tool failure) instead of converting every nonzero result into
    # 'image missing'. GNU timeout also signals its command group; the
    # forced-stop grace is finite.
    #
    # HERMIT_PINNED_IMAGE_PROBE_BOUND_S replaces the 120 s per-attempt bound, so
    # the wrapper's tests can time a hung and a TERM-ignoring probe through both
    # attempts in seconds. The default applies only when it is unset: an empty
    # value is refused, not read as 120.
    local bound=${HERMIT_PINNED_IMAGE_PROBE_BOUND_S-120} status attempt
    if [[ ! "$bound" =~ ^[1-9][0-9]*$ ]]; then
        echo "run-in-pinned-root: HERMIT_PINNED_IMAGE_PROBE_BOUND_S must be a positive integer number of seconds, got '$bound'." >&2
        echo "  Unset it for the default 120 s probe bound, or set a whole number of seconds such as 30." >&2
        return 2
    fi
    for attempt in 1 2; do
        if timeout --verbose --signal=TERM --kill-after=2s "${bound}s" podman image exists "$digest"; then
            return 0
        else
            status=$?
        fi
        if [[ ( "$status" == 124 || "$status" == 137 ) && "$attempt" == 1 ]]; then
            echo "run-in-pinned-root: inspection of $digest timed out (probe status $status; ${bound}s query bound); retrying once." >&2
            continue
        fi
        break
    done
    if [[ "$status" == 1 ]]; then
        echo "run-in-pinned-root: image $digest is not present locally (Podman status 1)." >&2
        echo "  This path does not fall back to a tag or to the host." >&2
        echo "  Rebuild it from the committed lock: ci/hermetic/build-image.sh" >&2
    elif [[ "$caller" == late && ( "$status" == 124 || "$status" == 137 ) ]]; then
        # A timed-out query says nothing about the image: on 2026-10-08, with
        # the host at load average 780, a query that normally takes 0.05 s
        # missed the bound for 25 nodes, which then failed as if their tests
        # had. Before a node's run, the run itself is the exact check: it uses
        # --pull=never, so a missing image still fails there and nothing is
        # fetched. Admission (--check-image) keeps refusing on a timeout.
        echo "run-in-pinned-root: inspection of $digest timed out (probe status $status; ${bound}s query bound); the run below uses --pull=never and fails if the image is absent." >&2
        return 0
    else
        echo "run-in-pinned-root: inspection unavailable for $digest (probe status $status; ${bound}s query bound, 2s forced-stop grace)." >&2
    fi
    return "$status"
}

if "$check_image"; then
    [[ -z "$project_src" && -z "$project_out" && -z "$project_descriptor" && "$project_mode" == ro=true && -z "$src" && -z "$out" && -z "$cargo_home" && "$src_mode" == ro=true && "$proc_locks_runtime" == false && "$nextest_calibration" == false && ${#pass_env[@]} == 0 && $# == 0 ]] || {
        echo "run-in-pinned-root: --check-image accepts only optional --digest." >&2
        exit 2
    }
    status=0
    check_pinned_image admission || status=$?
    exit "$status"
fi

[[ -n "$src" ]] || { echo "run-in-pinned-root: --src is required" >&2; exit 2; }
[[ -n "$out" ]] || { echo "run-in-pinned-root: --out is required" >&2; exit 2; }
[[ $# -gt 0 ]] || { echo "run-in-pinned-root: a command is required after --" >&2; exit 2; }

# Committed DAG commands are checkout-portable and therefore pass paths relative
# to the scheduler's repository working directory. Podman bind sources must be
# absolute; resolve them here at the execution boundary without rewriting the
# DAG in validate.
src=$(realpath -m -- "$src")
out=$(realpath -m -- "$out")
if [[ -n "$cargo_home" ]]; then
    cargo_home=$(realpath -m -- "$cargo_home")
fi
bundle_mount=()
if [[ -n "$bundle_lower" ]]; then
    bundle_lower=$(realpath -m -- "$bundle_lower")
    [[ -d "$bundle_lower" ]] || {
        echo "run-in-pinned-root: --bundle-lower $bundle_lower is not a directory" >&2
        exit 2
    }
    # Podman's --mount value is comma-framed.
    [[ "$bundle_lower" != *,* ]] || {
        echo "run-in-pinned-root: --bundle-lower path cannot contain a comma: $bundle_lower" >&2
        exit 2
    }
    bundle_mount=(--mount "type=bind,source=$bundle_lower,destination=/bundle-lower,ro=true")
fi

# A second workspace is one paired source/output, never an arbitrary mount.
# Keep the original core /src paths: prepared core records contain them.
project_mounts=(); workdir_in=/src; target_in=/src/target
if [[ -n $project_src || -n $project_out || -n $project_descriptor || $project_mode != ro=true ]]; then
    [[ -n $project_src && -n $project_out && -n $project_descriptor ]] || {
        echo "run-in-pinned-root: project source, output and descriptor are required together" >&2; exit 2;
    }
    project_src=$(realpath -e -- "$project_src")
    project_out=$(realpath -m -- "$project_out")
    project_descriptor=$(realpath -e -- "$project_descriptor")
    for mount_path in "$src" "$out" "$project_src" "$project_out" "$project_descriptor"; do
        [[ $mount_path != *','* && $mount_path != *$'\n'* && $mount_path != *$'\r'* && $mount_path != *$'\t'* ]] || {
            echo "run-in-pinned-root: project mount path is not exactly representable" >&2; exit 2;
        }
    done
    [[ $project_src != "$src" && $project_src != "$src/"* && $src != "$project_src/"* && $project_out != "$out" && $project_out != "$out/"* && $out != "$project_out/"* && $project_out != "$src" && $project_out != "$src/"* && $out != "$project_src" && $out != "$project_src/"* && $project_src != "$out/"* && $src != "$project_out/"* ]] || {
        echo "run-in-pinned-root: core and project sources/outputs must be distinct and non-overlapping" >&2; exit 2;
    }
    mkdir -p "$project_out/target"
    project_binding=$(mktemp "$project_out/source-binding.XXXXXX.json")
    python3 "$HERE/project-source.py" prepare "$src" "$project_src" "$project_descriptor" "$project_binding"
    project_mounts+=(--mount "type=bind,source=$project_src,destination=/project,$project_mode")
    project_mounts+=(--mount "type=bind,source=$project_out/target,destination=/project/target")
    project_mounts+=(--mount "type=bind,source=$project_binding,destination=/run/hermit-project-source.json,ro=true")
    workdir_in=/project; target_in=/project/target
fi

# FAIL CLOSED ON A MISSING IMAGE. Falling back to a tag, or to the host, would
# silently produce a run that is not hermetic while still reporting success --
# which is worse than not running at all, because the receipt would claim a
# pinned root it did not use.
check_pinned_image late || exit $?

pinned_home="$out/home"
# `agent-utils/rs/bin/*` deliberately keeps its Cargo target, provenance lock,
# and content-addressed launch snapshots beside the agent-utils checkout.  The
# source mount below is writable for build/test nodes, so without more-specific
# mounts a pinned-root invocation at `/src` and a host invocation at the real
# checkout path mutate one cache under two absolute source identities.  Cargo
# then invalidates the otherwise warm artifacts, and the shared launcher lock
# turns that rebuild into a cross-environment queue.  Keep all three writable
# state roots private to this pinned output, just like the repository target.
agent_utils_state="$out/agent-utils-rs"
mkdir -p \
    "$out/target" \
    "$pinned_home" \
    "$agent_utils_state/target" \
    "$agent_utils_state/locks" \
    "$agent_utils_state/snapshots"

# Only actual regular calibration consumers opt in. In particular, the script
# producer and other bootstrap calls must not depend on their own output.
calibration_mount=()
if "$nextest_calibration"; then
    host_scripts="$src/target/ci/rust-scripts"
    if [[ -e "$host_scripts/manifest.tsv" || -L "$host_scripts/manifest.tsv" ]]; then
        resolve_status=0
        capture_binary=$(cd -- "$src" && HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$host_scripts" \
            "$src/ci/rust-script-bin/rust-script" --resolve-optional --force "$src/ci/nextest-binaries.rs") || resolve_status=$?
        if ((resolve_status != 0 && resolve_status != 3)); then exit "$resolve_status"; fi
        capture_status=2
        if ((resolve_status == 0)); then
            capture_status=0
            capture_version=$("$capture_binary" capture-launch --probe) || capture_status=$?
        fi
        case $capture_status in
            0)
                [[ $capture_version == nextest-launch-observation-v2 ]] || { printf 'nextest budgets: malformed prepared capture capability; refresh build.rust_scripts\n' >&2; exit 2; }
                launch_dir=$(mktemp -d "$out/nextest-launch.XXXXXX")
                launch_file="$launch_dir/context.json"
                image_id=$(timeout --verbose --signal=TERM --kill-after=2s 10s \
                    podman image inspect --format '{{.Id}}' "$digest")
                image_id=${image_id#sha256:}
                "$capture_binary" capture-launch --pinned-image "$digest" --image-id "$image_id" --output "$launch_file"
                calibration_mount+=(--mount "type=bind,source=$launch_file,destination=/run/hermit-nextest-launch.json,ro=true")
                ;;
            2) printf 'nextest budgets: prepared host capture capability unavailable; pinned cohort remains unqualified\n' >&2 ;;
            *) exit "$capture_status" ;;
        esac
    else
        printf 'nextest budgets: prepared host capture unavailable; pinned cohort remains unqualified\n' >&2
    fi
fi

# Every git command below reads the repository named by --src or one of its
# submodules, or edits a private config copy named by --file. Git exports
# GIT_DIR to each `git rebase --exec` step, and GIT_DIR, GIT_WORK_TREE and
# GIT_INDEX_FILE to hooks, and each one overrides `git -C`: an inherited one
# made this wrapper mount another repository's metadata as the guest's, drop
# the submodule metadata mounts, or exit because `git submodule` found no
# working tree (https://github.com/rrnewton/hermit/issues/3362). Run them
# without the repository-location variables. GIT_CONFIG_COUNT and its
# companions are kept; they carry this host's url.insteadOf rewrites.
src_git() {
    (
        unset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE GIT_COMMON_DIR \
            GIT_OBJECT_DIRECTORY GIT_ALTERNATE_OBJECT_DIRECTORIES \
            GIT_NAMESPACE GIT_PREFIX
        exec git "$@"
    )
}

cargo_mount=(); cargo_home_in=/build/.cargo
git_mounts=()
append_source_git_mounts() {
    local src=$1 guest_source=$2 out=$3
if [[ -f "$src/.git" ]]; then
    git_common_dir=$(src_git -C "$src" rev-parse --path-format=absolute --git-common-dir)
    root_git_dir=$(src_git -C "$src" rev-parse --path-format=absolute --git-dir)
    raw_root_git_dir=$(sed -n "s/^gitdir: //p" "$src/.git")
    guest_root_git_dir=$(realpath -m "$guest_source/$raw_root_git_dir")
    if [[ $raw_root_git_dir == /* ]]; then
        guest_root_git_dir=$raw_root_git_dir
    fi
    guest_common_dir=$guest_root_git_dir
    if [[ -f "$root_git_dir/commondir" ]]; then
        raw_common_dir=$(cat "$root_git_dir/commondir")
        if [[ $raw_common_dir == /* ]]; then
            guest_common_dir=$raw_common_dir
        else
            guest_common_dir=$(realpath -m "$guest_root_git_dir/$raw_common_dir")
        fi
    fi
    # Keep the host-absolute common path for existing object/alternate references,
    # and also mount the paths that the unchanged gitfile/commondir resolve to
    # after the source is relocated to $guest_source. Mount ancestors before descendants.
    git_mounts+=(--mount "type=bind,source=$git_common_dir,destination=$git_common_dir,ro=true")
    if [[ $guest_common_dir != "$git_common_dir" ]]; then
        git_mounts+=(--mount "type=bind,source=$git_common_dir,destination=$guest_common_dir,ro=true")
    fi
    if [[ $root_git_dir != "$git_common_dir" || $guest_root_git_dir != "$guest_common_dir" ]]; then
        git_mounts+=(--mount "type=bind,source=$root_git_dir,destination=$guest_root_git_dir,ro=true")
    fi
    # A submodule root's core.worktree points back to its host parent. Override
    # only private config copies, never shared metadata or a global Git env var:
    # nested git -C commands must continue to discover their own repositories.
    #
    # The common config of a linked worktree is the shared checkout's config,
    # which every agent on the host rewrites by lock-and-rename. A rename over a
    # bind-mount destination detaches that mount in every mount namespace, so a
    # copy bound over it vanishes mid-run: the container's mount table changes
    # and its Git silently reads the host file again
    # (https://github.com/rrnewton/hermit/issues/3920). A linked worktree needs
    # that copy only when the common config sets core.worktree and
    # extensions.worktreeConfig applies it to every worktree; without the
    # extension Git applies it to the main worktree alone. Otherwise the common
    # config is left as it is and nothing is bound over it.
    common_override=true
    if [[ $root_git_dir != "$git_common_dir" ]]; then
        common_worktree_status=0
        src_git config --includes --file "$git_common_dir/config" --get core.worktree \
            >/dev/null || common_worktree_status=$?
        worktree_config_status=0
        worktree_config=$(src_git config --includes --file "$git_common_dir/config" \
            --type=bool --get extensions.worktreeConfig) || worktree_config_status=$?
        # --includes reads the config as Git does, following include.path.
        # Any status other than found (0) or absent (1), such as a malformed
        # boolean (exit 128, which also stops every other Git command in the
        # repository), keeps the private copy, as before this check.
        case "$common_worktree_status:$worktree_config_status" in
            1:0 | 1:1 | 0:1) common_override=false ;;
            0:0) [[ $worktree_config == true ]] || common_override=false ;;
        esac
    fi
    root_config_dir=$(mktemp -d "$out/git-root-configs.XXXXXX")
    for config_name in config config.worktree; do
        config_source="$git_common_dir/config"
        config_destination="$guest_common_dir/config"
        if [[ $config_name == config.worktree ]]; then
            config_source="$root_git_dir/config.worktree"
            config_destination="$guest_root_git_dir/config.worktree"
        elif [[ $common_override == false ]]; then
            continue
        fi
        [[ -f $config_source ]] || continue
        config_copy="$root_config_dir/$config_name"
        cp -- "$config_source" "$config_copy"
        src_git config --file "$config_copy" core.worktree "$guest_source"
        git_mounts+=(--mount "type=bind,source=$config_copy,destination=$config_destination,ro=true")
    done
fi
if [[ -e "$src/.git" ]]; then
    # Relocate each submodule's Git metadata without changing the shared host
    # configuration. Recreating its old worktree path makes Git report that
    # alias instead of $guest_source/<path>, which the strict submodule verifier refuses.
    # Keep objects and indexes read-only; overlay only private config copies.
    git_config_root=""
    submodule_paths=$(src_git -C "$src" submodule foreach --quiet --recursive 'printf "%s\n" "$displaypath"')
    while IFS= read -r submodule_path; do
        [[ -n $submodule_path ]] || continue
        submodule_root="$src/$submodule_path"
        [[ -f "$submodule_root/.git" ]] || continue
        submodule_git_dir=$(src_git -C "$submodule_root" rev-parse --path-format=absolute --git-dir)
        submodule_common_dir=$(src_git -C "$submodule_root" rev-parse --path-format=absolute --git-common-dir)
        raw_git_dir=$(sed -n "s/^gitdir: //p" "$submodule_root/.git")
        if [[ $raw_git_dir == /* ]]; then
            guest_git_dir=$raw_git_dir
        else
            guest_git_dir=$(realpath -m "$guest_source/$submodule_path/$raw_git_dir")
        fi
        guest_submodule_common_dir=$guest_git_dir
        if [[ -f "$submodule_git_dir/commondir" ]]; then
            raw_common_dir=$(cat "$submodule_git_dir/commondir")
            if [[ $raw_common_dir == /* ]]; then
                guest_submodule_common_dir=$raw_common_dir
            else
                guest_submodule_common_dir=$(realpath -m "$guest_git_dir/$raw_common_dir")
            fi
        fi
        # A linked submodule worktree can keep its common objects, refs and
        # config outside the root repository's metadata. Its gitdir alone is
        # not a repository: preserve the unchanged commondir resolution too.
        if [[ $submodule_common_dir != "$submodule_git_dir" ]]; then
            git_mounts+=(--mount "type=bind,source=$submodule_common_dir,destination=$submodule_common_dir,ro=true")
            if [[ $guest_submodule_common_dir != "$submodule_common_dir" ]]; then
                git_mounts+=(--mount "type=bind,source=$submodule_common_dir,destination=$guest_submodule_common_dir,ro=true")
            fi
        fi
        git_mounts+=(--mount "type=bind,source=$submodule_git_dir,destination=$guest_git_dir,ro=true")
        if [[ -z $git_config_root ]]; then
            git_config_root=$(mktemp -d "$out/git-configs.XXXXXX")
        fi
        mkdir -p "$git_config_root/$submodule_path"
        for config_name in config config.worktree; do
            config_source="$submodule_common_dir/config"
            config_destination="$guest_submodule_common_dir/config"
            if [[ $config_name == config.worktree ]]; then
                config_source="$submodule_git_dir/config.worktree"
                config_destination="$guest_git_dir/config.worktree"
            fi
            [[ -f $config_source ]] || continue
            config_copy="$git_config_root/$submodule_path/$config_name"
            cp -- "$config_source" "$config_copy"
            src_git config --file "$config_copy" core.worktree "$guest_source/$submodule_path"
            git_mounts+=(--mount "type=bind,source=$config_copy,destination=$config_destination,ro=true")
        done
    done <<< "$submodule_paths"
fi
}
append_source_git_mounts "$src" /src "$out"
if [[ -n $project_src ]]; then
    append_source_git_mounts "$project_src" /project "$project_out"
    # Two workspaces cannot overlay different private configs at one metadata
    # destination. Refuse such an alias instead of selecting one repository.
    declare -A metadata_sources=()
    for ((mount_index=1; mount_index<${#git_mounts[@]}; mount_index+=2)); do
        mount_value=${git_mounts[mount_index]}
        mount_source=${mount_value#type=bind,source=}; mount_source=${mount_source%%,destination=*}
        mount_destination=${mount_value#*,destination=}; mount_destination=${mount_destination%,ro=true}
        [[ ! -v metadata_sources[$mount_destination] || ${metadata_sources[$mount_destination]} == "$mount_source" ]] || {
            echo "run-in-pinned-root: conflicting project Git metadata mounts" >&2; exit 2;
        }
        metadata_sources[$mount_destination]=$mount_source
    done
    python3 "$HERE/project-source.py" check "$src" "$project_src" "$project_descriptor" "$project_binding"
fi
if [[ -n "$cargo_home" ]]; then
    [[ -d "$cargo_home" ]] || {
        echo "run-in-pinned-root: --cargo-home '$cargo_home' is not a directory." >&2
        echo "  It must be populated BEFORE this runs -- there is no network in here" >&2
        echo "  to populate it from. Run the build phase first." >&2
        exit 2
    }
    # A host Cargo home also contains installed executables and configuration.
    # Mounting it whole made `cargo clippy` select the host's rustup proxy and
    # try to update the moving `nightly` channel through the intentionally
    # disabled network, even though the image carries its own pinned clippy.
    # Import only dependency caches into a separate Cargo home so `/bin` remains
    # the sole source of Cargo subcommands in the pinned root.
    mkdir -p "$pinned_home/.cargo"
    for cache in registry git; do
        if [[ -d "$cargo_home/$cache" ]]; then
            mkdir -p "$pinned_home/.cargo/$cache"
            cargo_mount+=(--mount "type=bind,source=$cargo_home/$cache,destination=/build/.cargo/$cache")
        fi
    done
fi

env_args=()
extra_mounts=()
device_args=()
if [[ -e /dev/kvm ]]; then
    device_args+=(--device /dev/kvm)
fi
for name in "${pass_env[@]}"; do
    [[ $name =~ ^[A-Za-z_][A-Za-z0-9_]*$ ]] || {
        echo "run-in-pinned-root: invalid environment name '$name'." >&2
        exit 2
    }
    [[ -v $name ]] || continue
    case "$name" in
        E2E_RESULT_ROOT)
            mkdir -p "${!name}"
            extra_mounts+=(--mount "type=bind,source=${!name},destination=/results")
            env_args+=(-e E2E_RESULT_ROOT=/results)
            ;;
        E2E_BUILD_ROOT)
            env_args+=(-e E2E_BUILD_ROOT=/src/target/e2e-build)
            ;;
        VALIDATE_RUN_STATE)
            [[ ${!name} == /* ]] || {
                echo "run-in-pinned-root: VALIDATE_RUN_STATE must be absolute" >&2
                exit 2
            }
            mkdir -p "${!name}"
            extra_mounts+=(--mount "type=bind,source=${!name},destination=/validate-run-state")
            env_args+=(-e VALIDATE_RUN_STATE=/validate-run-state)
            ;;
        DAGRUN_TEST_COUNTS_PATH)
            [[ ${!name} == /* ]] || {
                echo "run-in-pinned-root: DAGRUN_TEST_COUNTS_PATH must be absolute" >&2
                exit 2
            }
            counts_dir=$(dirname -- "${!name}")
            counts_file=$(basename -- "${!name}")
            mkdir -p "$counts_dir"
            extra_mounts+=(--mount "type=bind,source=$counts_dir,destination=/dagrun-test-counts")
            env_args+=(-e "DAGRUN_TEST_COUNTS_PATH=/dagrun-test-counts/$counts_file")
            ;;
        HERMIT_CENSUS_BASE_DIR)
            # The stamped census base lives outside the source mount (the
            # queue adapter materializes it under the state root), so the
            # host path alone would name nothing inside the container.
            # Mount it read-only at a fixed guest path and rewrite the
            # variable; the file variables below rewrite against it.
            [[ ${!name} == /* ]] || {
                echo "run-in-pinned-root: HERMIT_CENSUS_BASE_DIR must be absolute" >&2
                exit 2
            }
            extra_mounts+=(--mount "type=bind,source=${!name},destination=/census-base,ro=true")
            env_args+=(-e HERMIT_CENSUS_BASE_DIR=/census-base)
            ;;
        HERMIT_CENSUS_BASE_LISTED_IDS|HERMIT_CENSUS_DROPPED_FILE)
            # Files inside the census base dir ride its /census-base
            # mount; a file elsewhere gets its own read-only mount.
            [[ ${!name} == /* ]] || {
                echo "run-in-pinned-root: $name must be absolute" >&2
                exit 2
            }
            if [[ -v HERMIT_CENSUS_BASE_DIR && ${!name} == "${HERMIT_CENSUS_BASE_DIR}/"* ]]; then
                env_args+=(-e "$name=/census-base/${!name#"${HERMIT_CENSUS_BASE_DIR}/"}")
            elif [[ $name == HERMIT_CENSUS_BASE_LISTED_IDS ]]; then
                extra_mounts+=(--mount "type=bind,source=$(dirname -- "${!name}"),destination=/census-listed,ro=true")
                env_args+=(-e "$name=/census-listed/$(basename -- "${!name}")")
            else
                extra_mounts+=(--mount "type=bind,source=$(dirname -- "${!name}"),destination=/census-dropped,ro=true")
                env_args+=(-e "$name=/census-dropped/$(basename -- "${!name}")")
            fi
            ;;
        DAGRUN_ARTIFACT_READY_DIR)
            [[ ${!name} == /* ]] || {
                echo "run-in-pinned-root: DAGRUN_ARTIFACT_READY_DIR must be absolute" >&2
                exit 2
            }
            mkdir -p "${!name}"
            extra_mounts+=(--mount "type=bind,source=${!name},destination=/dagrun-artifact-ready")
            env_args+=(-e DAGRUN_ARTIFACT_READY_DIR=/dagrun-artifact-ready)
            ;;
        *) env_args+=(--env "$name") ;;
    esac
done


# Only nodes selecting proc-locks fixtures opt in. Native tests and containers
# must flock this same host inode: a per-container lock cannot serialize OFD
# observations across PID namespaces. Never mount the rest of the host runtime.
if "$proc_locks_runtime"; then
    for name in "${pass_env[@]}"; do
        case "$name" in
            XDG_RUNTIME_DIR|HERMIT_PROC_LOCKS_LEASE_ID)
                echo "run-in-pinned-root: proc-locks runtime identity is wrapper-owned" >&2
                exit 2
                ;;
        esac
    done
    proc_locks_metadata=$(python3 - <<'PY'
import os
import stat

uid = os.geteuid()
configured = os.environ.get("XDG_RUNTIME_DIR", "")
runtime = configured if os.path.isabs(configured) else f"/run/user/{uid}"
# Podman's --mount value is comma-framed; our output is tab-framed. Refuse
# unrepresentable names rather than reinterpret them or choose another lease.
if any(character in runtime for character in ",\t\r\n"):
    raise SystemExit("proc-locks runtime path cannot be represented exactly")
directory_fd = os.open(runtime, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC)
try:
    directory = os.fstat(directory_fd)
    if not stat.S_ISDIR(directory.st_mode) or directory.st_uid != uid or directory.st_mode & 0o077:
        raise SystemExit("proc-locks runtime directory is not private and owned by the current user")
    name = "hermit-proc-locks-determinism.lock"
    lease_fd = os.open(name, os.O_CREAT | os.O_RDWR | os.O_CLOEXEC | os.O_NOFOLLOW | os.O_NONBLOCK,
                       0o600, dir_fd=directory_fd)
    try:
        lease = os.fstat(lease_fd)
        if not stat.S_ISREG(lease.st_mode) or lease.st_uid != uid or lease.st_mode & 0o077:
            raise SystemExit("proc-locks lease is not a private regular file owned by the current user")
        path = os.path.join(runtime, name)
        current = os.stat(path, follow_symlinks=False)
        if (current.st_dev, current.st_ino) != (lease.st_dev, lease.st_ino):
            raise SystemExit("proc-locks lease path changed while preparing the mount")
        print(f"{path}\t{lease.st_dev}:{lease.st_ino}")
    finally:
        os.close(lease_fd)
finally:
    os.close(directory_fd)
PY
    )
    IFS=$'\t' read -r proc_locks_file proc_locks_identity <<< "$proc_locks_metadata"
    [[ -n "$proc_locks_file" && $proc_locks_identity =~ ^[0-9]+:[0-9]+$ ]] || {
        echo "run-in-pinned-root: invalid proc-locks mount identity" >&2
        exit 2
    }
    extra_mounts+=(--tmpfs "/run/hermit-proc-locks:rw,nosuid,nodev,noexec,mode=0700")
    extra_mounts+=(--mount "type=bind,source=$proc_locks_file,destination=/run/hermit-proc-locks/hermit-proc-locks-determinism.lock")
    env_args+=(-e XDG_RUNTIME_DIR=/run/hermit-proc-locks)
    env_args+=(-e "HERMIT_PROC_LOCKS_LEASE_ID=$proc_locks_identity")
fi

# No optional Git locks. `git status` takes .git/index.lock to refresh the index
# opportunistically, even when it then writes nothing, so a read-only status
# creates and deletes a file in the repository it reads. Pinned-root nodes run
# side by side over the same Cargo git checkouts: the LiteInst tests resolve
# their compiled Reverie sources with `git status` (reverie/scripts/
# m1_artifact.rs) while source_copy::gitless_standalone_compiler_sources
# `cp -a`s that same checkout, and cp failed with "cannot stat
# '.git/index.lock'" (71e738ab's validation, 2026-10-15). Reproduced: 10 of 40
# copies failed beside a `git status` loop, 0 of 40 with GIT_OPTIONAL_LOCKS=0.
# ci/verify-submodules.sh already reads repositories this way.
env_args+=(-e GIT_OPTIONAL_LOCKS=0)

# Cargo decides a path crate is fresh by the checksums its dep-info records
# (-Zchecksum-freshness), not by mtimes. Every validation builds in a fresh
# checkout, whose files are all newer than the warm target's dep-info
# (ci-hub/validate/warm_target.py), so by mtime every workspace crate rebuilt
# on every run, changed or not. Set for EVERY pinned-root container, because
# they share /src/target: a build without it after one with it marks each
# unit dirty ("the prior compilation used checksum freshness and this one does
# not") and rebuilds everything. A seed built without it lacks the checksums,
# so its first run rebuilds in full; that is the fallback.
# Measured 2026-10-16 (host, validate profile, the
# build.workspace_compile_in_pinned_root cargo commands, 32 jobs): a warm build
# reproduced every rlib, shared object and executable of a cold build byte for
# byte (1,253 of 1,258 identical) for a pin bump (10a7b59d), a Detcore change
# (6b9c57af) and a CI-only change (548b2db5). The 5 others differ between two
# cold builds too: libmimalloc_sys's C objects and detcore-sabre, which this
# node cleans and rebuilds every run. The CI-only build took 207 s warm
# against 282 s cold.
env_args+=(-e CARGO_UNSTABLE_CHECKSUM_FRESHNESS=true)

# `--network=none` is the point, not a precaution: if the run can reach the
# network it can pick up something the lock does not describe, and the rebuild
# guarantee is void. CARGO_NET_OFFLINE in the image makes that fail loudly.
# --pull=never: the exact digest must already be in the local store; the run
# never fetches it, so a missing image fails here rather than being pulled.

# A container runtime can widen the task affinity even with cgroups disabled.
# Restore the actual caller's set inside the container before executing the
# command; util-linux/taskset is already in the pinned image. This preserves
# placement as well as the outer cgroup limits. Calibration still independently
# compares the resulting live affinity with its earlier host observation.
#
# Read with bash builtins from /proc/self/status (the kernel's
# Cpus_allowed_list, ascending ranges such as 0-3,8) and expanded to the
# explicit ascending list sched_getaffinity(2) gives. Not by starting python3:
# on a loaded host that interpreter start took 16.8 s, every second of it on a
# node's critical path, and validate_pinned_image's forced-stop test failed at
# 20.2 s against its 20 s bound when five validations ran at once (f9d81a69).
#
# Snapshot the file with one read(2) first. `read` on a seekable file reads
# 4096 bytes, then lseeks back to the end of the line it consumed, so a loop
# reading /proc/self/status line by line makes the kernel regenerate the file
# once per line. Fields before Cpus_allowed_list (VmRSS, RssAnon, ...) change
# length between regenerations, so a later line could be read at a shifted
# offset and arrive torn, which failed this check intermittently
# (lint-check-pinned-root-cache, three validates on 2026-10-21/22).
# $(<file) reads to EOF in one 4096-byte call for this ~1.4 KB file.
proc_status=$(< /proc/self/status)
caller_affinity=
while IFS=$'\t' read -r status_key status_value; do
    if [[ $status_key == Cpus_allowed_list: ]]; then
        IFS=, read -r -a affinity_ranges <<< "$status_value"
        for affinity_range in "${affinity_ranges[@]}"; do
            if [[ $affinity_range =~ ^([0-9]+)-([0-9]+)$ ]]; then
                for ((cpu = BASH_REMATCH[1]; cpu <= BASH_REMATCH[2]; cpu++)); do
                    caller_affinity+=${caller_affinity:+,}$cpu
                done
            elif [[ $affinity_range =~ ^[0-9]+$ ]]; then
                caller_affinity+=${caller_affinity:+,}$affinity_range
            else
                caller_affinity=
                break
            fi
        done
        break
    fi
done <<< "$proc_status"
[[ $caller_affinity =~ ^[0-9]+(,[0-9]+)*$ ]] || {
    echo "run-in-pinned-root: cannot read a nonempty caller CPU affinity" >&2
    exit 2
}

exec podman run --rm \
    --pull=never \
    --cgroups=disabled \
    --privileged \
    --hostname=hermetic-container.local \
    "${device_args[@]}" \
    --network=none \
    --http-proxy=false \
    --tmpfs /test:rw,nosuid,nodev,mode=1777 \
    --mount "type=bind,source=$src,destination=/src,$src_mode" \
    --mount "type=bind,source=$out/target,destination=/src/target" \
    --mount "type=bind,source=$agent_utils_state/target,destination=/src/agent-utils/rs/target" \
    --mount "type=bind,source=$agent_utils_state/locks,destination=/src/agent-utils/rs/.agent-utils-locks" \
    --mount "type=bind,source=$agent_utils_state/snapshots,destination=/src/agent-utils/rs/.agent-utils-snapshots" \
    --mount "type=bind,source=$pinned_home,destination=/build" \
    "${project_mounts[@]}" \
    "${cargo_mount[@]}" \
    "${git_mounts[@]}" \
    "${extra_mounts[@]}" \
    "${bundle_mount[@]}" \
    "${calibration_mount[@]}" \
    "${env_args[@]}" \
    -e HOME=/build \
    -e CARGO_HOME="$cargo_home_in" \
    -e "CARGO_TARGET_DIR=$target_in" \
    -w "$workdir_in" \
    "$digest" \
    /bin/taskset --cpu-list "$caller_affinity" \
    "$@"
