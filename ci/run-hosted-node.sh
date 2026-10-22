#!/usr/bin/env bash
# Run a committed hosted validation node inside the user namespace that gives
# its nested per-physical-run mount namespace CAP_SYS_ADMIN. GitHub's runner
# account can create a user namespace, but cannot unshare CLONE_NEWNS directly;
# merely probing the combined user+mount operation in an earlier step does not
# carry that authority into this process.

set -euo pipefail

if [[ ${GITHUB_ACTIONS:-} != true ]]; then
    echo "run-hosted-node.sh: this wrapper is only for GitHub-hosted validation" >&2
    exit 2
fi

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)"
export PATH="$ROOT_DIR/ci/rust-script-bin:$PATH"
export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$ROOT_DIR/target/ci/rust-scripts"
export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1

# Transported test binaries retain Cargo's compile-time CARGO_TARGET_TMPDIR.
# The artifact deliberately omits empty directories, so recreate that runtime
# contract before entering the network-isolated namespace.
mkdir -p "$ROOT_DIR/target/tmp"

if [[ ! -f $HERMIT_RUST_SCRIPT_ARTIFACT_ROOT/manifest.tsv ]]; then
    echo "run-hosted-node.sh: missing prepared rust-script manifest: $HERMIT_RUST_SCRIPT_ARTIFACT_ROOT/manifest.tsv" >&2
    exit 2
fi

# Budgeted Nextest attempts create per-attempt cgroups below this process's
# cgroup. Delegate that subtree before entering the user namespace: sudo does
# not work inside it, and the namespace inherits the delegated cgroup.
"$ROOT_DIR/ci/delegate-hosted-cgroup.sh" "$$"

echo "HOSTED-ISOLATION: entering a per-job user/mount/network namespace; local validate still uses its pinned-root and cgroup policy" >&2
exec unshare --user --map-root-user --uts --net --mount \
    bash -c '
        set -euo pipefail
        # Keep outbound networking absent while making loopback usable by the
        # local client/server unit tests. A new network namespace starts with
        # lo down, which is a setup failure rather than product isolation.
        ip link set lo up
        # Give tempfile users a root-owned path inside the UID mapping. The
        # runner checkout lives below host-owned /home, whose ancestors map to
        # the overflow UID and correctly fail security-sensitive path checks.
        # A fresh tmpfs also gives repeated strict runs the same inode baseline.
        #
        # Mount it without nosuid/nodev, like the local validation /tmp and the
        # runner'"'"'s own /tmp. Hermit nests a user namespace of its own, where
        # the kernel locks any nosuid/nodev on this mount, so every read-only
        # bind Hermit makes of a TMPDIR file must learn and restate them via
        # statfs. That turns the flags into a different test environment, not
        # extra isolation: this tmpfs is owned by the wrapper'"'"'s namespace and
        # nothing in it is set-uid or a device. The locked-flag case has its
        # own explicit test (image_container_accepts_a_rootfs_on_a_nosuid_nodev_filesystem).
        # See https://github.com/rrnewton/hermit/issues/3334.
        mount -t tmpfs -o mode=1777 tmpfs /tmp
        export TMPDIR=/tmp
        # Exercise the exact nested mount capability the per-physical-run /test
        # helper needs, without leaving the probe mount visible to validation.
        unshare --mount bash -c \
            "mount --make-rprivate / && mount -t tmpfs -o nosuid,nodev,mode=1777 tmpfs /test && umount /test"
        exec "$@"
    ' \
    bash ./ci/run-node.sh "$@"
