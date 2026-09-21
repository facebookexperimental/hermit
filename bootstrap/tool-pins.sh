#!/usr/bin/env bash

# Source revision used when the public Buck2 build ladder was established.
# Keep a full commit ID here: branch names and live tips are not reproducible.
REINDEER_REPOSITORY=https://github.com/facebookincubator/reindeer.git
REINDEER_REVISION=e3d72748131d3a70378055f091e0647c1edad85e

# The pinned Reindeer source revision selects this toolchain upstream.
REINDEER_RUST_TOOLCHAIN=nightly-2026-05-22
