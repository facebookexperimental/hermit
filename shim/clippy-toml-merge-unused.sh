#!/bin/sh
# clippy_configuration's TOML merge tool (build_defs/clippy_configuration.bzl).
# The prelude runs it only when the Rust toolchain sets its own clippy.toml,
# and this shim's toolchain sets none, so reaching here is a configuration
# change that needs a real merge tool.
echo "shim: clippy_configuration reached its TOML merge tool, which this build does not provide" >&2
exit 1
