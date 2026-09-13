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
#                                [--src-rw] [--cargo-home DIR] [--env NAME]... -- CMD...
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

set -euo pipefail

HERE=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
DIGEST_FILE="$HERE/image.digest"

src=""; out=""; digest=""; src_mode="ro=true"; cargo_home=""
pass_env=()
while [[ $# -gt 0 ]]; do
    case "$1" in
        --src) src=$2; shift 2 ;;
        --out) out=$2; shift 2 ;;
        --digest) digest=$2; shift 2 ;;
        --src-rw) src_mode="ro=false"; shift ;;
        --cargo-home) cargo_home=$2; shift 2 ;;
        --env) pass_env+=("$2"); shift 2 ;;
        --) shift; break ;;
        *) echo "run-in-pinned-root: unexpected argument '$1'" >&2; exit 2 ;;
    esac
done

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

if [[ -z "$digest" ]]; then
    [[ -f "$DIGEST_FILE" ]] || {
        echo "run-in-pinned-root: no --digest and no $DIGEST_FILE." >&2
        echo "  Build the image first: ci/hermetic/build-image.sh" >&2
        exit 2
    }
    digest=$(tr -d '[:space:]' < "$DIGEST_FILE")
fi

# FAIL CLOSED ON A MISSING IMAGE. Falling back to a tag, or to the host, would
# silently produce a run that is not hermetic while still reporting success --
# which is worse than not running at all, because the receipt would claim a
# pinned root it did not use.
if ! podman image exists "$digest"; then
    echo "run-in-pinned-root: image $digest is not present locally." >&2
    echo "  This path does not fall back to a tag or to the host: a run that is" >&2
    echo "  not in the pinned root must not be recorded as if it were." >&2
    echo "  Rebuild it from the committed lock: ci/hermetic/build-image.sh" >&2
    exit 1
fi

mkdir -p "$out/target" "$out/home"

cargo_mount=(); cargo_home_in=/build/.cargo
git_mounts=()
if [[ -f "$src/.git" ]]; then
    git_common_dir=$(git -C "$src" rev-parse --path-format=absolute --git-common-dir)
    git_mounts+=(--mount "type=bind,source=$git_common_dir,destination=$git_common_dir,ro=true")

fi
if [[ -e "$src/.git" ]]; then
    # Relocate each submodule's Git metadata without changing the shared host
    # configuration. Recreating its old worktree path makes Git report that
    # alias instead of /src/<path>, which the strict submodule verifier refuses.
    # Keep objects and indexes read-only; overlay only private config copies.
    git_config_root=""
    submodule_paths=$(git -C "$src" submodule foreach --quiet --recursive 'printf "%s\n" "$displaypath"')
    while IFS= read -r submodule_path; do
        [[ -n $submodule_path ]] || continue
        submodule_root="$src/$submodule_path"
        [[ -f "$submodule_root/.git" ]] || continue
        submodule_git_dir=$(git -C "$submodule_root" rev-parse --path-format=absolute --git-dir)
        raw_git_dir=$(sed -n "s/^gitdir: //p" "$submodule_root/.git")
        if [[ $raw_git_dir == /* ]]; then
            guest_git_dir=$raw_git_dir
        else
            guest_git_dir=$(realpath -m "/src/$submodule_path/$raw_git_dir")
        fi
        git_mounts+=(--mount "type=bind,source=$submodule_git_dir,destination=$guest_git_dir,ro=true")
        if [[ -z $git_config_root ]]; then
            git_config_root=$(mktemp -d "$out/git-configs.XXXXXX")
        fi
        mkdir -p "$git_config_root/$submodule_path"
        for config_name in config config.worktree; do
            [[ -f "$submodule_git_dir/$config_name" ]] || continue
            config_copy="$git_config_root/$submodule_path/$config_name"
            cp -- "$submodule_git_dir/$config_name" "$config_copy"
            git config --file "$config_copy" core.worktree "/src/$submodule_path"
            git_mounts+=(--mount "type=bind,source=$config_copy,destination=$guest_git_dir/$config_name,ro=true")
        done
    done <<< "$submodule_paths"
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
    mkdir -p "$out/home/.cargo"
    for cache in registry git; do
        if [[ -d "$cargo_home/$cache" ]]; then
            mkdir -p "$out/home/.cargo/$cache"
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
        *) env_args+=(--env "$name") ;;
    esac
done

# `--network=none` is the point, not a precaution: if the run can reach the
# network it can pick up something the lock does not describe, and the rebuild
# guarantee is void. CARGO_NET_OFFLINE in the image makes that fail loudly.
exec podman run --rm \
    --cgroups=disabled \
    --privileged \
    --hostname=hermetic-container.local \
    "${device_args[@]}" \
    --network=none \
    --http-proxy=false \
    --tmpfs /test:rw,nosuid,nodev,mode=1777 \
    --mount "type=bind,source=$src,destination=/src,$src_mode" \
    --mount "type=bind,source=$out/target,destination=/src/target" \
    --mount "type=bind,source=$out/home,destination=/build" \
    "${cargo_mount[@]}" \
    "${git_mounts[@]}" \
    "${extra_mounts[@]}" \
    "${env_args[@]}" \
    -e HOME=/build \
    -e CARGO_HOME="$cargo_home_in" \
    -e CARGO_TARGET_DIR=/src/target \
    -w /src \
    "$digest" \
    "$@"
