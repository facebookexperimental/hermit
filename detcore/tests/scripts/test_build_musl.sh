#!/bin/bash
# (c) Meta Platforms, Inc. and affiliates. Confidential and proprietary.

set -Eeou pipefail

echo "Invoking script test: $0 $*"

MUSL_TARBALL="$1"
shift
RUNNER="$*"

MUSL_VER=1.2.1
MUSL_SOURCE_WORK_DIR=$(mktemp -d)

TIME='/bin/time -o /dev/stdout'

trap cleanup EXIT

function cleanup {
    echo "Cleanup .." && rm -fr "${MUSL_SOURCE_WORK_DIR}"
}

echo "Building Musl $MUSL_VER as $(whoami) on $(hostname)"

cd "$MUSL_SOURCE_WORK_DIR"
echo; echo "Unpacking sources..."
$TIME /bin/tar -xf "$MUSL_TARBALL"
echo "Done unpacking into $MUSL_SOURCE_WORK_DIR"
mkdir build
cd build

echo; echo "Configure step:"
$TIME ../musl-${MUSL_VER}/configure --prefix=""

echo; echo "Parallel build step (stdout to file):"
echo "Build prefixed with: $RUNNER"
# Here we explicitly want $RUNNER to split into multiple words:
# shellcheck disable=SC2086
/bin/time --verbose ${RUNNER} -- make -j > ./build_output.txt

echo
echo "Successfully finished build. Lines of stdout: $(wc -l ./build_output.txt | awk '{ print $1 }' )"
