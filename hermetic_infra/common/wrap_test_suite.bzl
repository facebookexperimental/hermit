# Open-source placeholder for the fbsource-internal
# //hermetic_infra/common:wrap_test_suite.bzl.
#
# Internally, wrap_test_suite wraps the upstream rr syscall test programs
# (RR_TEST_TARGETS) as Tpx tests under `hermit run`. Open-source builds do
# not carry that runner: the same rr programs are covered here by the Cargo
# harness in hermit-cli/tests/rr_suite.rs (see docs/rr-test-suite.md). So in
# the OSS tree no targets are defined, mirroring the shim's lit_unittest.
# This file lives at hermit/hermetic_infra/common/ inside the repository, a
# different path from the internal fbcode//hermetic_infra/common/, and must
# never replace the internal definitions.

RR_TEST_TARGETS = []

def wrap_test_suite(**_kwargs):
    pass
