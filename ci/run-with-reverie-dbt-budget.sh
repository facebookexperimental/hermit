#!/usr/bin/env bash
# CARRY TO ae40c7f1 (2026-10-16): 51ecc8fb..ae40c7f1 adds a facts-only
# Guest runtime-FD query and normalizes the existing RPC owner to physical PID.
# Native C/CMake/vendor/SDK inputs and seven SDK object IDs are unchanged.
# Existing CPU, memory and parallel-job limits carry without recalibration;
# fresh Rust consumers and runtime checks remain required.
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# Re-derive the Reverie DBT elapsed budget inside the safe-ci child. Under
# cgroup boxing the runner exports its cap-derived CARGO_BUILD_JOBS immediately
# before this wrapper; on an unboxed hosted runner the launch-time
# CI_DAG_BUILD_JOBS value remains the fallback. Keeping this wrapper immediately
# around Cargo prevents a launcher-side width from standing in for NUM_JOBS.

set -euo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

if (($# == 0)); then
    echo "usage: ci/run-with-reverie-dbt-budget.sh COMMAND [ARG...]" >&2
    exit 2
fi

# Bind the calibration to the exact local Reverie revision before applying it.
# --print-pin is deliberately offline: the separate latest-main gate owns the
# network authority, while this check prevents a pin bump from silently reusing
# an earlier revision's clamp and measured threshold.
# CALIBRATED FOR ad598995 BY DBT RECIPE IDENTITY. CARRY TO ad598995 (2026-08-26):
# 200439dc..ad598995 is exactly rrnewton/reverie#496 and changes only
# reverie-process/src/container.rs. The two Reverie repository inputs to
# source_recipe_key are byte-identical:
#     reverie-dbt/vendor/dynamorio  de352475846e -> de352475846e
#     reverie-dbt/build.rs          0ff8ae24b974 -> 0ff8ae24b974
# The pin does not alter the selected CMAKE or CMAKE_GENERATOR, so the complete
# recipe remains the measured install key 132d77130980c546c8867fc196d97e664bc4816b1dfa9ea9c18de4a94d109c4d.
# The 1050 effective-job-second budget and MAX_PARALLEL_JOBS=16 therefore carry
# unchanged. Fresh validation is still required because runtime behavior changed.
#
# CARRY TO 200439dc (2026-08-26):
# a16e3c46..200439dc changes only reverie-ptrace/src/gdbstub/server.rs.
# The two Reverie repository inputs to source_recipe_key are byte-identical:
#     reverie-dbt/vendor/dynamorio  de352475846e -> de352475846e
#     reverie-dbt/build.rs          0ff8ae24b974 -> 0ff8ae24b974
# The pin does not alter the selected CMAKE or CMAKE_GENERATOR, so the complete
# recipe remains the measured install key 132d77130980c546c8867fc196d97e664bc4816b1dfa9ea9c18de4a94d109c4d.
# The 1050 effective-job-second budget and MAX_PARALLEL_JOBS=16 therefore carry
# unchanged. This source comparison does not replace fresh validation, and no
# receipt from the earlier pin may be reused.
#
# CARRY TO a16e3c46 (2026-08-25):
# b0c3cfe4..a16e3c46 is rrnewton/reverie#490, a reverie-kvm-only change (KVM SIGCHLD
# auto-reap). `git diff b0c3cfe4 a16e3c46 -- reverie-dbt` is EMPTY, and all three
# recipe inputs are byte-identical by git object id:
#     reverie-dbt/vendor/dynamorio  de352475846e -> de352475846e
#     reverie-dbt/build.rs          0ff8ae24b974 -> 0ff8ae24b974
#     third-party/                  fb49c0ba7a9a -> fb49c0ba7a9a
# so the measured effective-job-seconds budget carries unchanged. This is the same
# no-argument-required shape as the b0c3cfe4 carry below, not a weaker one.
# The earlier b0c3cfe4 evidence below
# established the prior carry. CARRY TO b0c3cfe4 (2026-08-25): f4152f8f..b0c3cfe4
# changes only reverie-memory/src/local.rs. reverie-dbt/build.rs remains blob
# 0ff8ae24b974 and reverie-dbt/vendor/dynamorio remains de352475846e, so every
# repository input to source_recipe_key is byte-identical and the measured
# effective-job-seconds budget carries unchanged. The prior calibration
# has been carried on five times before. Between 13cf8bcb and f4152f8f the two
# repository inputs are BYTE-IDENTICAL by git object id:
#     reverie-dbt/vendor/dynamorio  de352475846e -> de352475846e
#     reverie-dbt/build.rs          0ff8ae24b974 -> 0ff8ae24b974
# source_recipe_key also hashes the selected CMAKE and CMAKE_GENERATOR.
# This carry is stronger than the previous four, which each had to argue that some
# reverie-dbt Rust change was not a recipe input. Here `git diff 13cf8bcb f4152f8f
# -- reverie-dbt` is EMPTY: the directory is unchanged in its entirety, so there is
# no such argument to make, and the empirical install-key check below confirms the
# complete selected recipe.
#
# AND IT WAS CONFIRMED EMPIRICALLY AT THE NEW PIN, not only by source comparison.
# A build at f4152f8f produces the DynamoRIO install key this budget was measured
# against, in both profiles:
#     target/{debug,release}/reverie-dbt-native-cache/dynamorio-install-132d7713...
# Reproduced from a genuinely cold state -- the native cache was deleted and
# reverie-dbt `cargo clean`ed first, and the rebuild landed on the same key.
# The recipe key is the thing the measurement is a property of, so an identical key
# means the measured work is identical.
#
# WHAT IS CALIBRATED IS 1050 EFFECTIVE JOB-SECONDS, not an elapsed wall time.
# ci/configure-build-jobs.sh derives the elapsed bound as
#     MAX_BUILD_SECONDS = ceil(MAX_BUILD_EFFECTIVE_JOB_SECONDS / EFFECTIVE_BUILD_JOBS)
# so the budget already scales with width and must not be "topped up" by hand. If
# it is ever too tight, re-measure the job-seconds; do not raise the elapsed bound.
# CARRY TO 86d9003a (2026-08-27):
# ad598995..86d9003a is eight Reverie commits touching reverie-sabre, reverie-kvm,
# reverie-process and reverie-ptrace. `git diff ad598995 86d9003a -- reverie-dbt`
# is EMPTY -- the directory is unchanged in its entirety -- and all three recipe
# inputs are byte-identical by git object id:
#     reverie-dbt/vendor/dynamorio  de352475846e -> de352475846e
#     reverie-dbt/build.rs          0ff8ae24b974 -> 0ff8ae24b974
#     third-party/                  fb49c0ba7a9a -> fb49c0ba7a9a
# so the measured effective-job-seconds budget carries unchanged. This is the
# no-argument-required shape, like the a16e3c46 and f4152f8f carries above.
#
# CONFIRMED EMPIRICALLY AT THE NEW PIN, from a genuinely cold state rather than by
# source comparison alone: `target/debug/reverie-dbt-native-cache` was deleted and
# reverie-dbt `cargo clean`ed, then rebuilt at this pin. The build reported cache
# MISS then PUBLISHED on
#     key=sha256:132d77130980c546c8867fc196d97e664bc4816b1dfa9ea9c18de4a94d109c4d
# which is the key this budget was measured against, so the complete selected
# recipe -- including CMAKE and CMAKE_GENERATOR -- is identical. DynamoRIO source
# build took 30.85s at jobs=16. `cargo check --workspace --all-targets --locked`
# is rc=0 at this pin.
#
# ⚠️ WHY THIS RECALIBRATION IS ITS OWN COMMIT AND NOT PART OF THE BUMP. The pin
# moved from ad598995 to 86d9003a in 164d10f54e and 26d0230beb without this file
# changing, and every node behind this wrapper DECLINED for that whole window --
# correctly, and not silently: the decline is exit 75, which validate propagates
# as `no_result` and reports as FINAL_VALIDATE_STATUS: COULD_NOT_RUN. No run could
# report PASSED while these nodes had no verdict. But the coverage was really gone,
# and nothing in the bump said so. A Reverie bump and this expected_pin are coupled
# and the coupling is invisible from the bump side; whoever moves the pin next
# should expect to move this too.
# CARRY TO 7137c5dd (2026-08-27):
# 86d9003a..7137c5dd changes no DBT build input. Verified by git object ID:
#     reverie-dbt/vendor/dynamorio  de352475846e -> de352475846e
#     reverie-dbt/build.rs          0ff8ae24b974 -> 0ff8ae24b974
#     third-party/                  fb49c0ba7a9a -> fb49c0ba7a9a
# so the measured effective-job-seconds budget carries unchanged.
#
# BOUND TO 49ae9401 (2026-08-27): this revision changes the vendored
# DynamoRIO source, so the earlier recipe identity does not carry. A cold
# `cargo check -p reverie-dbt --locked --offline` reported cache MISS then
# PUBLISHED for
#     key=sha256:c9c1ee55257cbb0635b56f494a75ee1dc6af839ca8e289231f533b0208340463
# and the native source build took 33.38s at jobs=16, or 534.08 effective
# job-seconds. The existing 1050 effective-job-second threshold remains above
# that one cold local measurement. It is retained conservatively; this sample
# does not replace the original n=3 hosted measurement or satisfy the >=5-sample
# replacement rule.
# CARRY TO 1645b64b (2026-08-27): the three source_recipe_key repository
# inputs are byte-identical to 49ae9401:
#     reverie-dbt/vendor/dynamorio  a3c41e5d3630 -> a3c41e5d3630
#     reverie-dbt/build.rs          0ff8ae24b974 -> 0ff8ae24b974
#     third-party/                  fb49c0ba7a9a -> fb49c0ba7a9a
# so the measured key and conservative threshold carry unchanged.
# CARRY TO 4f3fbd50 (2026-08-27): ab07a892..4f3fbd50 changes
# reverie-dbt/src/lib.rs but neither input to the DynamoRIO content-key miss
# whose elapsed time this wrapper bounds:
#     reverie-dbt/vendor/dynamorio  a3c41e5d3630 -> a3c41e5d3630
#     reverie-dbt/build.rs          0ff8ae24b974 -> 0ff8ae24b974
# CMAKE and CMAKE_GENERATOR are host inputs rather than pin contents, so the
# measured key and conservative threshold carry unchanged. The Rust source
# change remains build-relevant and requires fresh validation; this carry does
# not reuse a receipt.
# CARRY TO 1f226acd (2026-08-27): all three source_recipe_key repository
# inputs are byte-identical to 4f3fbd50:
#     reverie-dbt/vendor/dynamorio  a3c41e5d3630 -> a3c41e5d3630
#     reverie-dbt/build.rs          0ff8ae24b974 -> 0ff8ae24b974
#     third-party/                  fb49c0ba7a9a -> fb49c0ba7a9a
# so the measured key and conservative threshold carry unchanged.
# CARRY TO af42d9cf (2026-08-28): the same three inputs are byte-identical
# from 1f226acd, checked by tree object rather than by reading the diff:
#     reverie-dbt/vendor/dynamorio  a3c41e5d3630 -> a3c41e5d3630
#     reverie-dbt/build.rs          0ff8ae24b974 -> 0ff8ae24b974
#     third-party/                  fb49c0ba7a9a -> fb49c0ba7a9a
# The intervening Reverie commits are the LiteInst task-creation change (#447),
# which touches only reverie-ptrace/src/{task,tracer,error}.rs,
# reverie-liteinst/src/backend.rs, reverie-liteinst/tests/hybrid.rs and six C
# fixtures, and a documentation commit (#511). Neither can affect the elapsed
# time of a DynamoRIO content-key miss, so the measured key and conservative
# threshold carry unchanged and this is NOT a recalibration.
# CARRY TO bc106a19 (2026-08-28): both repository inputs to the DynamoRIO
# content-key miss are byte-identical to af42d9cf:
#     reverie-dbt/vendor/dynamorio  a3c41e5d3630 -> a3c41e5d3630
#     reverie-dbt/build.rs          0ff8ae24b974 -> 0ff8ae24b974
# The changed Reverie files are confined to reverie-ptrace timer recovery and
# tests. They are build-relevant to Hermit, so this pin still requires fresh
# validation; only the measured DBT build budget carries unchanged.
# CARRY TO c2e2c8fb (2026-09-02): both repository inputs to the DynamoRIO
# content-key miss are byte-identical to bc106a19 by git object id:
#     reverie-dbt/vendor/dynamorio  a3c41e5d3630 -> a3c41e5d3630
#     reverie-dbt/build.rs          0ff8ae24b974 -> 0ff8ae24b974
# The five intervening commits change validation evidence, exhaustive internal
# enum dispatch, and the common Backend output API, but no native DBT recipe
# input. The measured native-build budget therefore carries unchanged; the Rust
# API change still requires a fresh Hermit build and validation.
# CARRY TO 4b18ecf0 (2026-09-05): both repository inputs to the DynamoRIO
# content-key miss are byte-identical to 37e7b727 by git object id:
#     reverie-dbt/vendor/dynamorio  a3c41e5d3630 -> a3c41e5d3630
#     reverie-dbt/build.rs          0ff8ae24b974 -> 0ff8ae24b974
# The intervening commit changes DBT evidence and launcher behavior, not the
# native DBT build recipe. The measured native-build budget carries unchanged;
# fresh Hermit validation is still required for the evidence API change.
# CARRY TO 8c8c0a57 (2026-09-05): both repository inputs to the DynamoRIO
# content-key miss are byte-identical to 4b18ecf0 by git object id:
#     reverie-dbt/vendor/dynamorio  a3c41e5d3630 -> a3c41e5d3630
#     reverie-dbt/build.rs          0ff8ae24b974 -> 0ff8ae24b974
# The intervening commit changes only reverie-kvm runtime behavior and its
# static-ELF tests. The measured native DBT build budget carries unchanged;
# fresh Hermit validation is still required for the KVM runtime change.
# CARRY TO a158914e (2026-09-15): both repository inputs to the DynamoRIO
# content-key miss are byte-identical to 8c8c0a57 by Git object identity:
#     reverie-dbt/vendor/dynamorio  a3c41e5d3630 -> a3c41e5d3630
#     reverie-dbt/build.rs          0ff8ae24b974 -> 0ff8ae24b974
# CMAKE and CMAKE_GENERATOR selection is unchanged. The existing 16-job clamp
# and 1050 effective-job-second threshold carry at recipe key
# c9c1ee55257cbb0635b56f494a75ee1dc6af839ca8e289231f533b0208340463.
# This is source identity, not a new timing sample or a validation receipt;
# fresh Hermit validation is required for the runtime and API changes.
# CARRY TO a2cc1868 (2026-09-17): the private-loader fallback changes
# core/unix/loader.c; build.rs, CMake targets/options and MAX_PARALLEL_JOBS=16
# remain unchanged. The default native recipe key therefore changes to
# 0aa6d84239b5a04b7cda124ebed4c7e3adc8b62f5b4c96011a9b971e90d6b0a4.
# A genuine local content-key MISS built in 27.71s at jobs=4 (110.84 job-seconds)
# under 4 CPU / 8 GiB bounds. This supports retaining the existing 1050
# effective-job-second threshold and 16-job clamp; it is not a replacement
# hosted-runner calibration or a claim of unchanged recipe bytes.
# The same pin also carries stricter RPC partial-header EOF classification;
# it does not select the new mapped transport. Fresh Hermit build and original
# unit assertions remain required; native loader fixtures are not guest evidence.
# CARRY TO b3049e54 (2026-09-17): the landed SaBRe frame ABI repair and
# opt-in mapped-RPC ownership APIs leave the entire reverie-dbt subtree,
# build.rs, vendor tree, native key, and CMAKE/CMAKE_GENERATOR selection unchanged.
# Keep the existing 1050 effective-job-second threshold and 16-job clamp.
# This source-identity carry supplies no new timing or Hermit guest evidence.
# CARRY TO 596b9ade (2026-09-17): the complete b3049e54-to-landed
# comparison preserves reverie-dbt tree 6232257769144e8f63891a5efc8935abc3cd836b,
# build.rs blob 0ff8ae24b97464044735ba79ea74765ba4ac3ff0 and DynamoRIO vendor
# tree 42dd83f76cef3e730c39d2313c11fdc78d12ae35, including all build/config bytes.
# CMAKE remains the default cmake and CMAKE_GENERATOR remains unset.
# Keep the existing 1050 effective-job-second threshold and 16-job clamp.
# Source comparison 89b2eb0abe05008a21974668602c288452108270c85132ea48f87bb325d91ca2 is a carry decision,
# not a new timing sample or Hermit guest receipt.
# CARRY TO 94d97270 (2026-09-17): all eight commits after b3049e54
# (f918218c, 2fabda5b, 4866241e, 596b9ade, 545faab1, ca4e61a9,
# 78e5d73a, 94d97270) preserve the complete reverie-dbt and third-party
# trees and root Cargo.toml/rust-toolchain.toml Git objects. In particular:
#   reverie-dbt: 6232257769144e8f63891a5efc8935abc3cd836b
#   reverie-dbt/build.rs: 0ff8ae24b97464044735ba79ea74765ba4ac3ff0
#   reverie-dbt/vendor/dynamorio: 42dd83f76cef3e730c39d2313c11fdc78d12ae35
# CMAKE/CMAKE_GENERATOR selection and native build options are unchanged, so
# recipe key 0aa6d84239b5a04b7cda124ebed4c7e3adc8b62f5b4c96011a9b971e90d6b0a4,
# MAX_PARALLEL_JOBS=16 and 1050 effective-job-seconds carry unchanged. This is
# recipe identity evidence, not a new timing measurement or runtime receipt.
# The carried KVM, RPC/log-capture and SaBRe behavior is not claimed unchanged.
# CARRY TO c164a085 (2026-09-17): the ninth commit after b3049e54
# restores SaBRe's original PROT_* mapping protections and adds native controls.
# Its five-path delta leaves all DBT inputs unchanged. Across all nine commits,
# the complete reverie-dbt/third-party trees and root Cargo.toml/toolchain
# retain the exact Git objects recorded above. The native recipe key remains
# 0aa6d84239b5a04b7cda124ebed4c7e3adc8b62f5b4c96011a9b971e90d6b0a4;
# CMAKE/CMAKE_GENERATOR, native options, 16-job clamp and 1050 effective-job-
# second threshold are unchanged. This is a source-identity carry, not a new
# timing sample or Hermit guest result; the SaBRe behavior intentionally changes.
# CARRY TO 6ae69f57 (2026-09-17): the tenth commit after b3049e54
# intentionally changes reverie-dbt/native/CMakeLists.txt: GNU builds of the
# on-demand client now use -mtls-dialect=gnu, matching the installed client.
# The complete reverie-dbt subtree and client compiler flags are NOT identical.
# This is the sole DBT delta across the ten commits. DynamoRIO vendor source,
# build.rs, root Cargo/toolchain and CMAKE/CMAKE_GENERATOR selection are unchanged;
# native/CMakeLists.txt is not an input to the DynamoRIO SDK recipe key above.
# That SDK key, MAX_PARALLEL_JOBS=16 and 1050 effective-job-seconds therefore carry.
# Client preparation still reruns CMake and uses the new Cargo source directory.
# This carry is source evidence, not a new timing sample or Hermit guest result.
# CARRY TO 4db9ddb6 (2026-09-17): the host-hybrid LiteInst exec repair and
# intervening runtime changes preserve the DynamoRIO cache recipe inputs:
# vendor/dynamorio is 42dd83f76cef, and build.rs is 0ff8ae24b974.
# The GNU-only -mtls-dialect=gnu addition changes native/CMakeLists.txt for
# the on-demand client, not this DynamoRIO content-key miss. Rebuild that
# client at the new pin; this carry is not runtime validation.
# Preserve CMAKE/CMAKE_GENERATOR selection, the 1050 effective-job-second
# threshold and the 16-job clamp. No new calibration is claimed.
# CARRY TO 226c3e31 (2026-09-17): the four commits after 6ae69f57
# repair LiteInst exec reactivation/owned worker-exec waits, observe returned
# native PKRU, and declare SaBRe's zlib dependency. The complete reverie-dbt
# subtree, build.rs, DynamoRIO vendor, root Cargo/toolchain and CMake selection
# are byte-identical to 6ae69f57. The earlier GNU client TLS flag is preserved;
# it remains the sole DBT change across b3049e54..226c3e31, not an SDK key input.
# Keep SDK key 0aa6d84239b5a04b7cda124ebed4c7e3adc8b62f5b4c96011a9b971e90d6b0a4,
# the 16-job clamp and 1050 effective-job-seconds. No new timing or guest claim;
# the carried LiteInst/ptrace/preload and SaBRe behavior intentionally changes.
# CARRY TO 114b3094 (2026-09-17): the complete 4db9ddb6-to-landed
# comparison preserves the entire reverie-dbt tree
# df7f4e8c655698849f0356a2bb41121ddff15be8, build.rs blob
# 0ff8ae24b97464044735ba79ea74765ba4ac3ff0 and DynamoRIO vendor tree
# 42dd83f76cef3e730c39d2313c11fdc78d12ae35, including native build inputs.
# Preserve CMAKE/CMAKE_GENERATOR selection, the 1050 effective-job-second
# threshold and the 16-job clamp. This is a source-identity carry,
# not a new timing calibration or Hermit guest result.
# CARRY TO 526c21cf (2026-09-17): the complete 30fee360-to-landed
# comparison preserves the entire reverie-dbt tree
# df7f4e8c655698849f0356a2bb41121ddff15be8, build.rs blob
# 0ff8ae24b97464044735ba79ea74765ba4ac3ff0 and DynamoRIO vendor tree
# 42dd83f76cef3e730c39d2313c11fdc78d12ae35, including native build inputs.
# Preserve CMAKE/CMAKE_GENERATOR selection, the 1050 effective-job-second
# threshold and the 16-job clamp. This is a source-identity carry,
# not a new timing calibration or Hermit guest result.
# CARRY TO c8f4ca9d (2026-09-17): the landed KVM failure-notification repair
# https://github.com/rrnewton/reverie/pull/577 preserves the SDK recipe from
# 526c21cf: build.rs blob 0ff8ae24b97464044735ba79ea74765ba4ac3ff0 and
# DynamoRIO vendor tree 42dd83f76cef3e730c39d2313c11fdc78d12ae35 are identical.
# Root Cargo.toml, rust-toolchain.toml and the third-party gitlink also match.
# CMAKE remains the default cmake and CMAKE_GENERATOR remains unset, retaining
# SDK key 0aa6d84239b5a04b7cda124ebed4c7e3adc8b62f5b4c96011a9b971e90d6b0a4,
# the 16-job clamp and 1050 effective-job-second threshold. This is source
# evidence for carrying the build budget, not a new timing or runtime result.
# CARRY TO 7d863ab3 (2026-09-17): the landed KVM cleanup correction
# https://github.com/rrnewton/reverie/pull/578 changes only reverie-kvm/src/vm.rs.
# The build.rs blob 0ff8ae24b97464044735ba79ea74765ba4ac3ff0, DynamoRIO
# vendor 42dd83f76cef3e730c39d2313c11fdc78d12ae35, root Cargo/toolchain and
# third-party inputs match c8f4ca9d. Keep default cmake, unset CMAKE_GENERATOR,
# SDK key 0aa6d84239b5a04b7cda124ebed4c7e3adc8b62f5b4c96011a9b971e90d6b0a4,
# the 16-job clamp and 1050 effective-job-second threshold. This carries
# the unchanged SDK recipe; it is not a new timing or Hermit guest measurement.
# BOUNDED COLD SDK OBSERVATION AT 99d1e482 (2026-09-18):
# Incoming Reverie https://github.com/rrnewton/reverie/pull/467 changes the
# vendored DynamoRIO drreg.c, so the old 7d863ab3 recipe identity does not carry.
# At landed https://github.com/rrnewton/reverie/pull/579, the actual new build.rs
# reported MISS, native completion in 49.64s at jobs=2, and PUBLISHED for
#     key=sha256:b0247764df7fba083f90538e12d3afcc8ffad5150c65bd321e689da5e57b74ed
# Actual child nproc=316; min(2,316,16)=2 yields a 525s elapsed ratchet.
# The completed native sample is 99.28 effective-job-seconds, below 1050.
# Retain the conservative 1050 effective-job-second threshold and 16-job clamp.
# This one local sample does not replace the original n=3 hosted calibration
# or satisfy the >=5-sample replacement rule. The encompassing explicit
# external-package Cargo check returned 101 afterward: the counter2 example
# requires prototype-runtime, absent in Hermit's default-features=false graph.
# That Cargo failure remains a failure; only its completed cold SDK work is
# calibration evidence. The normal Hermit workspace/all-target check remains
# independently required. No DBT guest correctness or new replay claim follows.
# CARRY TO b5e2ab49 (2026-09-18): the KVM repairs through
# https://github.com/rrnewton/reverie/pull/587 preserve the entire reverie-dbt
# subtree from f97b7be1, including build.rs and the DynamoRIO gitlink. Root
# Cargo.toml, rust-toolchain.toml, third-party and .gitmodules also match.
# No CMAKE or CMAKE_GENERATOR selection changes here. Carry the existing
# b0247764df7f recipe, 1050 effective-job-second threshold and 16-job clamp.
# This is source identity evidence, not a new timing or Hermit guest result;
# the original single-sample and failed-enclosing-check limitations remain.
# CARRY TO e21e13c7 (2026-09-18): the ptrace clock-origin repair changes
# only reverie-ptrace source/tests. The complete reverie-dbt tree, build.rs,
# DynamoRIO Gitlink and root recipe/toolchain inputs are identical to b5e2ab49.
# Retain the 1050 effective-job-second budget and 16-job clamp with the same
# CMAKE selection. This source-identity carry is not a new timing measurement
# and does not transfer an earlier pin's runtime validation.
# CARRY TO bd398149 (2026-09-19): the landed native feature-build repair
# https://github.com/rrnewton/reverie/pull/590 changes only a KVM constructor
# call and equivalent test byte-array syntax. The complete reverie-dbt tree
# ad0ef5e0d8bd, build.rs, DynamoRIO tree, root Cargo.toml, toolchain,
# .gitmodules and third-party inputs are identical to e21e13c7.
# Keep default CMAKE, unset CMAKE_GENERATOR, the 1050 effective-job-second
# budget and 16-job clamp. This source-identity carry is not a new timing or
# guest measurement; all prior calibration and validation limitations remain.
# CARRY TO d3ababc9c1ac5704322c0feca3658fee11f20155: reviewed timer source preserves the exact
# DynamoRIO SDK build.rs/vendor and root manifest/toolchain recipe inputs.
# Keep the same CMAKE/CMAKE_GENERATOR selection, 1050 effective-job-seconds
# and 16-job clamp. This is source identity carry, not new calibration.
# CARRY TO 429962666ad7e9ef05877b74b585a571f57ef0c4: the landed capture/startup and memory
# ownership changes preserve the exact DynamoRIO SDK build.rs/vendor and root
# manifest/toolchain recipe inputs. Keep the same CMAKE/CMAKE_GENERATOR selection,
# 1050 effective-job-seconds and 16-job clamp. Source identity carry only;
# no new calibration.
# CARRY TO 91110d249ffd8957267d71fab8c83d9636105efe: the landed KVM memory and process
# startup changes preserve the exact DynamoRIO SDK build.rs/vendor and root
# manifest/toolchain recipe inputs. Keep the same CMAKE/CMAKE_GENERATOR selection,
# 1050 effective-job-seconds and 16-job clamp. Source identity carry only;
# no new calibration.
# CARRY TO 502bc21f897065766f1ef4c940ede1efe4743acd: all repository inputs
# to the DynamoRIO SDK recipe, including the complete reverie-dbt subtree,
# remain byte-identical to 91110d249ffd8957267d71fab8c83d9636105efe.
# Keep default CMAKE, unset CMAKE_GENERATOR, the 1050 effective-job-second
# threshold and 16-job clamp. The native KVM repair requires its own
# qualification; this is source-identity carry, not a new timing sample,
# SDK cache-key measurement, guest result or earlier receipt reuse.
# CARRY TO be09e5100bca6dad77aede0349de9a5c92990854 (2026-09-20): relative to
# f7bd85e11dd258112148ed2cba6531501a1a00d9, reverie-dbt/build.rs has no
# diff and the DynamoRIO vendor tree remains
# 117d54d744df23921c531d0fe08537249f5a510a. Preserve the existing CMAKE and
# CMAKE_GENERATOR selection, 16-job clamp, and 1050 effective-job-second
# threshold. This source-identity carry adds no timing sample, guest result,
# or runtime qualification; all prior calibration limitations remain.
# CARRY TO cf1e993517c94d05200ec2d4bd840a42fd2bd62c (2026-09-20): relative to
# be09e5100bca6dad77aede0349de9a5c92990854, build.rs remains blob
# 0ff8ae24b97464044735ba79ea74765ba4ac3ff0 and the DynamoRIO vendor tree
# remains 117d54d744df23921c531d0fe08537249f5a510a. This is the squash landing
# of reviewed head 1fdadb7940dc232d07c1e36494f0f102b74f3140 from
# https://github.com/rrnewton/reverie/pull/606; both have exact repository tree
# f676d5d8dcfb266e92e2e656661ff5e08f73f787. Preserve CMAKE/CMAKE_GENERATOR,
# the 16-job clamp, and the 1050 effective-job-second threshold. This is
# source-identity carry, not new calibration or runtime qualification.
# CARRY TO 123df7c4c0169006fbfe1f11a1553fe333eac937 (2026-09-20): relative to
# cf1e993517c94d05200ec2d4bd840a42fd2bd62c, build.rs remains blob
# 0ff8ae24b97464044735ba79ea74765ba4ac3ff0 and the DynamoRIO vendor tree
# remains 117d54d744df23921c531d0fe08537249f5a510a. PR607 changes only
# reverie-kvm address-publication paths. Preserve CMAKE/CMAKE_GENERATOR, the
# 16-job clamp, and the 1050 effective-job-second threshold. This is
# source-identity carry, not new calibration or runtime qualification.
# CARRY TO ae1d1da78d2d89a1fa0454ee2282cf135e2286ed (2026-09-22): relative to
# 123df7c4c0169006fbfe1f11a1553fe333eac937, build.rs remains blob
# 0ff8ae24b97464044735ba79ea74765ba4ac3ff0, the DynamoRIO vendor tree
# remains 117d54d744df23921c531d0fe08537249f5a510a, and third-party/
# remains tree fb49c0ba7a9abd48a4ea662bf20e08246c81fc5a. The forward
# pin includes SaBRe finalizer ownership repair PR617 and earlier main
# changes; none changes these build inputs. Preserve CMAKE/CMAKE_GENERATOR,
# the 16-job clamp, and the 1050 effective-job-second threshold. This is
# source-identity carry, not new calibration or runtime qualification.
# CARRY TO c444c4ff15b6f5985082317e7c93b370e67571c1 (2026-09-23): relative to
# ae1d1da78d2d89a1fa0454ee2282cf135e2286ed, build.rs remains blob
# 0ff8ae24b97464044735ba79ea74765ba4ac3ff0, the DynamoRIO vendor tree
# remains 117d54d744df23921c531d0fe08537249f5a510a, and third-party/
# remains tree fb49c0ba7a9abd48a4ea662bf20e08246c81fc5a. Reverie PR622
# changes KVM capture handling; PR621 consumes LiteInst2 state descriptors.
# Neither changes these build inputs. Preserve CMAKE/CMAKE_GENERATOR,
# the 16-job clamp, and the 1050 effective-job-second threshold. This is
# source-identity carry, not new calibration or runtime qualification.
# CARRY TO afc71332b51344e22ce082c64fbf7c534fca1986 (2026-09-25): relative to
# c444c4ff15b6f5985082317e7c93b370e67571c1, build.rs remains blob
# 0ff8ae24b97464044735ba79ea74765ba4ac3ff0, the DynamoRIO vendor tree
# remains 117d54d744df23921c531d0fe08537249f5a510a, and third-party/
# remains tree fb49c0ba7a9abd48a4ea662bf20e08246c81fc5a. Preserve
# CMAKE/CMAKE_GENERATOR, the 16-job clamp and 1050 effective-job-second
# threshold. This is source-identity carry, not a new calibration.
# CARRY TO 05400652a6fc1a7403a2b5cdec40bd4f4631eac3 (2026-09-25): relative to
# afc71332b51344e22ce082c64fbf7c534fca1986, the complete reverie-dbt tree remains
# a62d15302ee5e907667d1c02f6e629177ad87f61; build.rs remains blob
# 0ff8ae24b97464044735ba79ea74765ba4ac3ff0 and DynamoRIO remains tree
# 117d54d744df23921c531d0fe08537249f5a510a. The only new product paths are
# reverie-process owned-container source/tests. Preserve CMAKE/CMAKE_GENERATOR,
# the 16-job clamp and 1050 effective-job-second threshold. This is unchanged
# source-input carry, not fresh calibration or runtime qualification.
# CARRY TO 95bc2b1daf9e4d0cd9bd158c5846374244a08d40 (2026-09-25): relative to
# 05400652a6fc1a7403a2b5cdec40bd4f4631eac3, the complete reverie-dbt tree remains
# a62d15302ee5e907667d1c02f6e629177ad87f61; build.rs remains blob
# 0ff8ae24b97464044735ba79ea74765ba4ac3ff0 and DynamoRIO remains tree
# 117d54d744df23921c531d0fe08537249f5a510a. Preserve CMAKE/CMAKE_GENERATOR,
# the 16-job clamp and 1050 effective-job-second threshold. This is unchanged
# source-input carry, not fresh calibration or runtime qualification.
# CARRY TO efc671191bf5cc756c8703df6b5204cce63d0bbd (2026-09-26): from
# 95bc2b1daf9e4d0cd9bd158c5846374244a08d40, both native DBT recipe inputs retain
# their exact Git object identities: reverie-dbt/build.rs is
# 0ff8ae24b97464044735ba79ea74765ba4ac3ff0 and vendor/dynamorio is
# 117d54d744df23921c531d0fe08537249f5a510a. The complete reverie-dbt
# tree is unchanged, and this update changes neither CMAKE nor CMAKE_GENERATOR.
# MAX_PARALLEL_JOBS=16 and the 1050 effective-job-second budget carry unchanged.
# This source comparison is not a new timing sample or a runtime qualification.
# CARRY TO 424e5424c97696b92c3aad61701e540e7e8b92ec (2026-09-26): from
# efc671191bf5cc756c8703df6b5204cce63d0bbd, only reverie-e9patch/src/backend.rs
# changes. The complete reverie-dbt tree remains
# a62d15302ee5e907667d1c02f6e629177ad87f61; build.rs remains blob
# 0ff8ae24b97464044735ba79ea74765ba4ac3ff0 and DynamoRIO remains tree
# 117d54d744df23921c531d0fe08537249f5a510a. CMAKE/CMAKE_GENERATOR selection,
# MAX_PARALLEL_JOBS=16 and 1050 effective-job-seconds remain unchanged.
# This is source-identity carry, not a new timing sample or runtime qualification.
# CARRY TO b0ede531e00dd1e068d0e0b1f220edddcf96d4b5 (2026-09-27): from
# 424e5424c97696b92c3aad61701e540e7e8b92ec, the stdin terminal-read work and
# child-future storage repair leave the complete reverie-dbt tree unchanged at
# a62d15302ee5e907667d1c02f6e629177ad87f61. build.rs remains blob
# 0ff8ae24b97464044735ba79ea74765ba4ac3ff0; DynamoRIO remains tree
# 117d54d744df23921c531d0fe08537249f5a510a; third-party remains tree
# fb49c0ba7a9abd48a4ea662bf20e08246c81fc5a. CMAKE/CMAKE_GENERATOR selection,
# MAX_PARALLEL_JOBS=16 and 1050 effective-job-seconds carry unchanged.
# This is source-identity carry, not a new timing sample or runtime qualification.
# CARRY TO fba351a56c1bc6f6d713b4fa0d6e46979aa6b4e1 (2026-09-30): the two-commit
# range 2eeb704cefb166ef7411478a1bb37e2867cf3644..fba351a5 (09242068 and fba351a5,
# test-fixture path lookup and C fixture formatting in reverie-ptrace) does not
# touch reverie-dbt: its tree stays 0e92df9a7348861fab977a98165a616da6c4f41f.
# build.rs remains blob 0ff8ae24b97464044735ba79ea74765ba4ac3ff0; DynamoRIO remains
# tree 117d54d744df23921c531d0fe08537249f5a510a; third-party remains tree
# fb49c0ba7a9abd48a4ea662bf20e08246c81fc5a; root Cargo.toml remains blob
# 4168dea2771f18a00fb1afdfd2218efba415ecbb; rust-toolchain.toml remains blob
# b7ca9302bc65522b829aa2fe3b8783fc77fcb7b9. CMAKE/CMAKE_GENERATOR selection,
# MAX_PARALLEL_JOBS=16 and 1050 effective-job-seconds carry unchanged. This is
# source-identity carry, not a new timing sample or runtime qualification.
# CARRY TO e9f88def58060d1407461e04f93103fac0cd6f8f (2026-09-30): from
# fba351a56c1bc6f6d713b4fa0d6e46979aa6b4e1, the 24-commit range changes reverie-dbt
# only in src/lib.rs (tree 0e92df9a7348861fab977a98165a616da6c4f41f ->
# bf0f2c006c6547f67a4d8ffe753b14d7e803838a): six lines that give DbtGuest the new
# Guest::is_backend_runtime_bootstrap method, returning false. src/ is not a
# DynamoRIO SDK recipe input: build_dynamorio times only the CMake configure/build
# of vendor/dynamorio. build.rs remains blob 0ff8ae24b97464044735ba79ea74765ba4ac3ff0;
# DynamoRIO remains tree 117d54d744df23921c531d0fe08537249f5a510a; third-party
# remains tree fb49c0ba7a9abd48a4ea662bf20e08246c81fc5a; root Cargo.toml remains
# blob 4168dea2771f18a00fb1afdfd2218efba415ecbb; rust-toolchain.toml remains blob
# b7ca9302bc65522b829aa2fe3b8783fc77fcb7b9. CMAKE/CMAKE_GENERATOR selection,
# MAX_PARALLEL_JOBS=16 and 1050 effective-job-seconds carry unchanged. This is
# source-identity carry, not a new timing sample or runtime qualification.
# CARRY TO 5ef758608c92aca7a7bde71acc38ec184cc3d297 (2026-09-30): from
# e9f88def58060d1407461e04f93103fac0cd6f8f, the 1-commit range changes reverie-dbt
# only in src/evidence.rs (tree bf0f2c006c6547f67a4d8ffe753b14d7e803838a ->
# a58869784a49b28fe187f955585bf3cf0597455a): DbtEvidence records the stream
# position of each initialization record and gains the all_records accessor,
# which returns every authenticated record in arrival order. src/ is not a
# DynamoRIO SDK recipe input: build_dynamorio times only the CMake configure/build
# of vendor/dynamorio. build.rs remains blob 0ff8ae24b97464044735ba79ea74765ba4ac3ff0;
# DynamoRIO remains tree 117d54d744df23921c531d0fe08537249f5a510a; third-party
# remains tree fb49c0ba7a9abd48a4ea662bf20e08246c81fc5a; root Cargo.toml remains
# blob 4168dea2771f18a00fb1afdfd2218efba415ecbb; rust-toolchain.toml remains blob
# b7ca9302bc65522b829aa2fe3b8783fc77fcb7b9. CMAKE/CMAKE_GENERATOR selection,
# MAX_PARALLEL_JOBS=16 and 1050 effective-job-seconds carry unchanged. This is
# source-identity carry, not a new timing sample or runtime qualification.
# CARRY TO dbf2b5c8880bac5fd6a06fa1f296b3eec3590569 (2026-10-01): from
# 5ef758608c92aca7a7bde71acc38ec184cc3d297, the 26-commit range does not touch
# reverie-dbt: its tree stays a58869784a49b28fe187f955585bf3cf0597455a. The range
# changes reverie-ptrace, reverie-liteinst, reverie-kvm/src and the SaBRe loader.
# build.rs remains blob 0ff8ae24b97464044735ba79ea74765ba4ac3ff0; DynamoRIO remains
# tree 117d54d744df23921c531d0fe08537249f5a510a; third-party remains tree
# fb49c0ba7a9abd48a4ea662bf20e08246c81fc5a; root Cargo.toml remains blob
# 4168dea2771f18a00fb1afdfd2218efba415ecbb; rust-toolchain.toml remains blob
# b7ca9302bc65522b829aa2fe3b8783fc77fcb7b9. CMAKE/CMAKE_GENERATOR selection,
# MAX_PARALLEL_JOBS=16 and 1050 effective-job-seconds carry unchanged. This is
# source-identity carry, not a new timing sample or runtime qualification.
# CARRY TO 096cbcc8cd945cc26753d12f35ad0bae6d2ad277 (2026-10-01): from
# dbf2b5c8880bac5fd6a06fa1f296b3eec3590569, the 1-commit range is the 0.4.0 release version bump.
# It changes reverie-dbt only in Cargo.toml (blob 8da5d73a60b920b3077a57836e7fa5fb9c66edb7
# -> 0e24d047d544a3daae2d6350270b26ceb74139d1; tree a58869784a49b28fe187f955585bf3cf0597455a
# -> 0cbe39248f8b7ce6fce78f42963e6231b9ab7fc3): the package version and three
# first-party dependency version requirements go from 0.2.0 to 0.4.0. Cargo.toml is
# not a DynamoRIO SDK recipe input. build.rs remains blob
# 0ff8ae24b97464044735ba79ea74765ba4ac3ff0; DynamoRIO remains tree
# 117d54d744df23921c531d0fe08537249f5a510a; third-party remains tree
# fb49c0ba7a9abd48a4ea662bf20e08246c81fc5a; root Cargo.toml remains blob
# 4168dea2771f18a00fb1afdfd2218efba415ecbb; rust-toolchain.toml remains blob
# b7ca9302bc65522b829aa2fe3b8783fc77fcb7b9. CMAKE/CMAKE_GENERATOR selection,
# MAX_PARALLEL_JOBS=16 and 1050 effective-job-seconds carry unchanged. This is
# source-identity carry, not a new timing sample or runtime qualification.
# CARRY TO c2a7e9f2f17f64e8f0740fc76e0f6046ac13d3fb (2026-10-02): from
# 096cbcc8cd945cc26753d12f35ad0bae6d2ad277, the 47-commit range (ptrace, safeptrace,
# KVM, LiteInst and preload fixes, including the vfork fix chain) leaves the whole
# reverie-dbt tree byte-identical: it remains tree
# 0cbe39248f8b7ce6fce78f42963e6231b9ab7fc3, so Cargo.toml remains blob
# 0e24d047d544a3daae2d6350270b26ceb74139d1, build.rs remains blob
# 0ff8ae24b97464044735ba79ea74765ba4ac3ff0 and DynamoRIO remains tree
# 117d54d744df23921c531d0fe08537249f5a510a; third-party remains tree
# fb49c0ba7a9abd48a4ea662bf20e08246c81fc5a; root Cargo.toml remains blob
# 4168dea2771f18a00fb1afdfd2218efba415ecbb; rust-toolchain.toml remains blob
# b7ca9302bc65522b829aa2fe3b8783fc77fcb7b9. CMAKE/CMAKE_GENERATOR selection,
# MAX_PARALLEL_JOBS=16 and 1050 effective-job-seconds carry unchanged. This is
# source-identity carry, not a new timing sample or runtime qualification.
# CARRY TO b4aa7f80542a5edd8e0dc30aea7f9626c9c8a359 (2026-10-02): from
# c2a7e9f2f17f64e8f0740fc76e0f6046ac13d3fb, the 16-commit range (zero-count read,
# vDSO getrandom refusal, KVM, safeptrace and object 0.40 fixes) changes reverie-dbt only in
# native/client.c, scripts/test-example-tools.sh and a new
# tests/fixtures/vdso_getrandom.c (tree 0cbe39248f8b7ce6fce78f42963e6231b9ab7fc3
# -> 12c917f3efdfbc31f65bbcdba9b6aae3f5253f04). None is an input to the DynamoRIO
# SDK recipe key above, which hashes the vendored DynamoRIO source, build.rs,
# CMAKE and CMAKE_GENERATOR. Cargo.toml remains blob
# 0e24d047d544a3daae2d6350270b26ceb74139d1, build.rs remains blob
# 0ff8ae24b97464044735ba79ea74765ba4ac3ff0, native/CMakeLists.txt is unchanged and
# DynamoRIO remains tree 117d54d744df23921c531d0fe08537249f5a510a; third-party
# remains tree fb49c0ba7a9abd48a4ea662bf20e08246c81fc5a; root Cargo.toml remains
# blob 4168dea2771f18a00fb1afdfd2218efba415ecbb; rust-toolchain.toml remains blob
# b7ca9302bc65522b829aa2fe3b8783fc77fcb7b9. CMAKE/CMAKE_GENERATOR selection,
# MAX_PARALLEL_JOBS=16 and 1050 effective-job-seconds carry unchanged. Client
# preparation still rebuilds the on-demand client from the new client.c. This is
# source-identity carry, not a new timing sample or runtime qualification.
# CARRY TO f4a19322a925268f855b9b5cad1c05fb71817246 (2026-10-02): from
# b4aa7f80542a5edd8e0dc30aea7f9626c9c8a359, the 10-commit range (vDSO fail-closed
# patching, LiteInst newborn TIDs, safeptrace docs, and the family 19h model A0h
# skid margin) changes reverie-dbt only in native/client.c,
# scripts/test-example-tools.sh and a new tests/fixtures/vdso_fail_closed.c (tree
# 12c917f3efdfbc31f65bbcdba9b6aae3f5253f04 -> c513f8f99c52e047591e8526da6332a099dde509).
# None is an input to the DynamoRIO SDK recipe key above. Cargo.toml remains blob
# 0e24d047d544a3daae2d6350270b26ceb74139d1, build.rs remains blob
# 0ff8ae24b97464044735ba79ea74765ba4ac3ff0, native/CMakeLists.txt remains blob
# bcfb298a4f87ed190d7fdc52393e01d1245a8fe3 and DynamoRIO remains tree
# 117d54d744df23921c531d0fe08537249f5a510a; third-party remains tree
# fb49c0ba7a9abd48a4ea662bf20e08246c81fc5a; root Cargo.toml remains blob
# 4168dea2771f18a00fb1afdfd2218efba415ecbb; rust-toolchain.toml remains blob
# b7ca9302bc65522b829aa2fe3b8783fc77fcb7b9. CMAKE/CMAKE_GENERATOR selection,
# MAX_PARALLEL_JOBS=16 and 1050 effective-job-seconds carry unchanged. Client
# preparation still rebuilds the on-demand client from the new client.c. This is
# source-identity carry, not a new timing sample or runtime qualification.
# CARRY TO d766df20b7809d707e29bf7ceff24d3f52f0a3dc (2026-10-03): from
# f4a19322a925268f855b9b5cad1c05fb71817246, the 30-commit range (the shared dispatch-stats record,
# libc 0.2.190, the gdbstub T packet and breakpoint fixes, KVM descriptor fixes,
# alias tests, LiteInst fallback switches, and the reverie-sabre-stats Buck target)
# changes reverie-dbt only in native/client.c, a new native/reverie_vdso_symbols.h,
# src/backend_stats.rs and a new tests/vdso_symbol_header.rs (tree
# c513f8f99c52e047591e8526da6332a099dde509 -> ee627a7227998bdd91018dab1efbe6ab7964ec6b).
# None is an input to the DynamoRIO SDK recipe key above. Cargo.toml remains blob
# 0e24d047d544a3daae2d6350270b26ceb74139d1, build.rs remains blob
# 0ff8ae24b97464044735ba79ea74765ba4ac3ff0, native/CMakeLists.txt remains blob
# bcfb298a4f87ed190d7fdc52393e01d1245a8fe3 and DynamoRIO remains tree
# 117d54d744df23921c531d0fe08537249f5a510a; third-party remains tree
# fb49c0ba7a9abd48a4ea662bf20e08246c81fc5a; root Cargo.toml remains blob
# 4168dea2771f18a00fb1afdfd2218efba415ecbb; rust-toolchain.toml remains blob
# b7ca9302bc65522b829aa2fe3b8783fc77fcb7b9. CMAKE/CMAKE_GENERATOR selection,
# MAX_PARALLEL_JOBS=16 and 1050 effective-job-seconds carry unchanged. Client
# preparation still rebuilds the on-demand client from the new client.c. This is
# source-identity carry, not a new timing sample or runtime qualification.
# CARRY TO 9976e29c1151acf258609d9d32eba86f112054ac (2026-10-03): from
# d766df20b7809d707e29bf7ceff24d3f52f0a3dc, the DynamoRIO SDK inputs listed below
# are byte-identical. This is source-identity carry, not a new timing sample.
# reverie-dbt/Cargo.toml: 0e24d047d544a3daae2d6350270b26ceb74139d1
# reverie-dbt/build.rs: 0ff8ae24b97464044735ba79ea74765ba4ac3ff0
# reverie-dbt/native/CMakeLists.txt: bcfb298a4f87ed190d7fdc52393e01d1245a8fe3
# reverie-dbt/vendor/dynamorio: 117d54d744df23921c531d0fe08537249f5a510a
# third-party: fb49c0ba7a9abd48a4ea662bf20e08246c81fc5a
# Cargo.toml: 4168dea2771f18a00fb1afdfd2218efba415ecbb
# rust-toolchain.toml: b7ca9302bc65522b829aa2fe3b8783fc77fcb7b9
# CMAKE/CMAKE_GENERATOR policy, MAX_PARALLEL_JOBS=16 and the existing
# 1050 effective-job-seconds budget are unchanged.
# CARRY TO 3a196cfb9dbb775900a1acb02890b8ca10aedd0c (2026-10-03): from
# 9976e29c1151acf258609d9d32eba86f112054ac, the DynamoRIO SDK inputs listed below
# are byte-identical. The 2-commit range (the held-signal fix-forwards of
# https://github.com/rrnewton/reverie/pull/831) changes only reverie-ptrace and a
# reverie-liteinst test. This is source-identity carry, not a new timing sample.
# reverie-dbt/Cargo.toml: 0e24d047d544a3daae2d6350270b26ceb74139d1
# reverie-dbt/build.rs: 0ff8ae24b97464044735ba79ea74765ba4ac3ff0
# reverie-dbt/native/CMakeLists.txt: bcfb298a4f87ed190d7fdc52393e01d1245a8fe3
# reverie-dbt/vendor/dynamorio: 117d54d744df23921c531d0fe08537249f5a510a
# third-party: fb49c0ba7a9abd48a4ea662bf20e08246c81fc5a
# Cargo.toml: 4168dea2771f18a00fb1afdfd2218efba415ecbb
# rust-toolchain.toml: b7ca9302bc65522b829aa2fe3b8783fc77fcb7b9
# CMAKE/CMAKE_GENERATOR policy, MAX_PARALLEL_JOBS=16 and the existing
# 1050 effective-job-seconds budget are unchanged.
# CARRY TO d4971bca43fd65749b9d2a9c4fd5c7dee00ad505 (2026-10-03): diagnostic consumer of
# https://github.com/rrnewton/reverie/pull/903, from 3a196cfb9dbb775900a1acb02890b8ca10aedd0c.
# The KVM candidate and intervening ptrace changes leave all seven SDK
# recipe/provenance inputs below byte-identical. This is source-identity carry,
# not a new timing sample, canonical validation, or a manifest activation.
# reverie-dbt/Cargo.toml: 0e24d047d544a3daae2d6350270b26ceb74139d1
# reverie-dbt/build.rs: 0ff8ae24b97464044735ba79ea74765ba4ac3ff0
# reverie-dbt/native/CMakeLists.txt: bcfb298a4f87ed190d7fdc52393e01d1245a8fe3
# reverie-dbt/vendor/dynamorio: 117d54d744df23921c531d0fe08537249f5a510a
# third-party: fb49c0ba7a9abd48a4ea662bf20e08246c81fc5a
# Cargo.toml: 4168dea2771f18a00fb1afdfd2218efba415ecbb
# rust-toolchain.toml: b7ca9302bc65522b829aa2fe3b8783fc77fcb7b9
# CMAKE/CMAKE_GENERATOR policy, MAX_PARALLEL_JOBS=16 and the existing
# 1050 effective-job-seconds budget are unchanged.
# CARRY TO c3fe1dc3493f32b5d81a0a9babbae8ba758bf02d (2026-10-03): landed producer for
# https://github.com/rrnewton/reverie/pull/903. Relative to the diagnostic
# candidate d4971bca43fd65749b9d2a9c4fd5c7dee00ad505, all seven DBT recipe/provenance
# inputs below are byte-identical; the additional main commit changes only
# ptrace code and tests. This is source-identity carry, not a timing sample.
# reverie-dbt/Cargo.toml: 0e24d047d544a3daae2d6350270b26ceb74139d1
# reverie-dbt/build.rs: 0ff8ae24b97464044735ba79ea74765ba4ac3ff0
# reverie-dbt/native/CMakeLists.txt: bcfb298a4f87ed190d7fdc52393e01d1245a8fe3
# reverie-dbt/vendor/dynamorio: 117d54d744df23921c531d0fe08537249f5a510a
# third-party: fb49c0ba7a9abd48a4ea662bf20e08246c81fc5a
# Cargo.toml: 4168dea2771f18a00fb1afdfd2218efba415ecbb
# rust-toolchain.toml: b7ca9302bc65522b829aa2fe3b8783fc77fcb7b9
# CMAKE/CMAKE_GENERATOR policy, MAX_PARALLEL_JOBS=16 and the existing
# 1050 effective-job-seconds budget are unchanged.
# CARRY TO 5914bcb59a45038a12400ae30d03ebc0c1382b79 (2026-10-03): source-only candidate
# for https://github.com/rrnewton/reverie/issues/905. All seven DBT
# recipe/provenance objects are identical to c3fe1dc3493f32b5d81a0a9babbae8ba758bf02d.
# This carries the existing budget by source identity, not a new timing sample.
# reverie-dbt/Cargo.toml: 0e24d047d544a3daae2d6350270b26ceb74139d1
# reverie-dbt/build.rs: 0ff8ae24b97464044735ba79ea74765ba4ac3ff0
# reverie-dbt/native/CMakeLists.txt: bcfb298a4f87ed190d7fdc52393e01d1245a8fe3
# reverie-dbt/vendor/dynamorio: 117d54d744df23921c531d0fe08537249f5a510a
# third-party: fb49c0ba7a9abd48a4ea662bf20e08246c81fc5a
# Cargo.toml: 4168dea2771f18a00fb1afdfd2218efba415ecbb
# rust-toolchain.toml: b7ca9302bc65522b829aa2fe3b8783fc77fcb7b9
# CMAKE/CMAKE_GENERATOR policy, MAX_PARALLEL_JOBS=16 and the existing
# 1050 effective-job-seconds budget remain unchanged.
# CARRY TO 107734a16487797251d40b5efa7013d3436797bf: landed epoll_pwait2 producer for
# https://github.com/rrnewton/reverie/pull/907 . Seven DBT recipe/provenance
# inputs below equal the landing-base producer c3fe1dc3493f32b5d81a0a9babbae8ba758bf02d.
# reverie-dbt/Cargo.toml: 0e24d047d544a3daae2d6350270b26ceb74139d1
# reverie-dbt/build.rs: 0ff8ae24b97464044735ba79ea74765ba4ac3ff0
# reverie-dbt/native/CMakeLists.txt: bcfb298a4f87ed190d7fdc52393e01d1245a8fe3
# reverie-dbt/vendor/dynamorio: 117d54d744df23921c531d0fe08537249f5a510a
# third-party: fb49c0ba7a9abd48a4ea662bf20e08246c81fc5a
# Cargo.toml: 4168dea2771f18a00fb1afdfd2218efba415ecbb
# rust-toolchain.toml: b7ca9302bc65522b829aa2fe3b8783fc77fcb7b9
# Source identity carries the existing CMAKE/CMAKE_GENERATOR policy,
# MAX_PARALLEL_JOBS=16 and 1050 effective-job-seconds; no new timing claim.
# CARRY TO f49afefd16b72af4452475aa383a372d57a5d9de (2026-10-03): from
# 107734a16487797251d40b5efa7013d3436797bf, the DynamoRIO SDK inputs listed below
# are byte-identical. The 4-commit range (the held-signal seccomp
# fix-forwards of https://github.com/rrnewton/reverie/pull/831) changes only
# reverie-ptrace and touches no reverie-dbt input. This is source-identity
# carry, not a new timing sample.
# reverie-dbt/Cargo.toml: 0e24d047d544a3daae2d6350270b26ceb74139d1
# reverie-dbt/build.rs: 0ff8ae24b97464044735ba79ea74765ba4ac3ff0
# reverie-dbt/native/CMakeLists.txt: bcfb298a4f87ed190d7fdc52393e01d1245a8fe3
# reverie-dbt/vendor/dynamorio: 117d54d744df23921c531d0fe08537249f5a510a
# third-party: fb49c0ba7a9abd48a4ea662bf20e08246c81fc5a
# Cargo.toml: 4168dea2771f18a00fb1afdfd2218efba415ecbb
# rust-toolchain.toml: b7ca9302bc65522b829aa2fe3b8783fc77fcb7b9
# CMAKE/CMAKE_GENERATOR policy, MAX_PARALLEL_JOBS=16 and the existing
# 1050 effective-job-seconds budget are unchanged.
# CARRY TO b4ffc27d4376d5a0ee9d42170652e89c8501d2a6 (2026-10-03): from
# f49afefd16b72af4452475aa383a372d57a5d9de, the DynamoRIO SDK inputs listed below
# are byte-identical. The 3-commit range (fix-forward 13 of
# https://github.com/rrnewton/reverie/pull/831 in reverie-ptrace, and two
# reverie-kvm syncfs commits) touches no reverie-dbt input. This is
# source-identity carry, not a new timing sample.
# reverie-dbt/Cargo.toml: 0e24d047d544a3daae2d6350270b26ceb74139d1
# reverie-dbt/build.rs: 0ff8ae24b97464044735ba79ea74765ba4ac3ff0
# reverie-dbt/native/CMakeLists.txt: bcfb298a4f87ed190d7fdc52393e01d1245a8fe3
# reverie-dbt/vendor/dynamorio: 117d54d744df23921c531d0fe08537249f5a510a
# third-party: fb49c0ba7a9abd48a4ea662bf20e08246c81fc5a
# Cargo.toml: 4168dea2771f18a00fb1afdfd2218efba415ecbb
# rust-toolchain.toml: b7ca9302bc65522b829aa2fe3b8783fc77fcb7b9
# CMAKE/CMAKE_GENERATOR policy, MAX_PARALLEL_JOBS=16 and the existing
# 1050 effective-job-seconds budget are unchanged.
# CARRY TO 41550078fb631c9d07ea71a3c5a44126e5cb87ef (2026-10-03): from
# b4ffc27d4376d5a0ee9d42170652e89c8501d2a6, the DynamoRIO SDK inputs listed below
# are byte-identical. The 11-commit range (fix-forward 14 of
# https://github.com/rrnewton/reverie/pull/831 in reverie-ptrace, and ten
# reverie-kvm pipe-owner and shared-mapping commits) touches no reverie-dbt
# input. This is source-identity carry, not a new timing sample.
# reverie-dbt/Cargo.toml: 0e24d047d544a3daae2d6350270b26ceb74139d1
# reverie-dbt/build.rs: 0ff8ae24b97464044735ba79ea74765ba4ac3ff0
# reverie-dbt/native/CMakeLists.txt: bcfb298a4f87ed190d7fdc52393e01d1245a8fe3
# reverie-dbt/vendor/dynamorio: 117d54d744df23921c531d0fe08537249f5a510a
# third-party: fb49c0ba7a9abd48a4ea662bf20e08246c81fc5a
# Cargo.toml: 4168dea2771f18a00fb1afdfd2218efba415ecbb
# rust-toolchain.toml: b7ca9302bc65522b829aa2fe3b8783fc77fcb7b9
# CMAKE/CMAKE_GENERATOR policy, MAX_PARALLEL_JOBS=16 and the existing
# 1050 effective-job-seconds budget are unchanged.
# CARRY TO 6c79fbafa8e49e60ccc79de984b7cdca3c9b0c5c (2026-10-03): from
# 41550078fb631c9d07ea71a3c5a44126e5cb87ef; all seven recorded DynamoRIO build inputs
# below are byte-identical. This adopts the ordinary-file fork repair in
# https://github.com/rrnewton/reverie/pull/919 and preserves intervening main.
# Source identity carries the calibration; no new timing sample is claimed.
# reverie-dbt/Cargo.toml: 0e24d047d544a3daae2d6350270b26ceb74139d1
# reverie-dbt/build.rs: 0ff8ae24b97464044735ba79ea74765ba4ac3ff0
# reverie-dbt/native/CMakeLists.txt: bcfb298a4f87ed190d7fdc52393e01d1245a8fe3
# reverie-dbt/vendor/dynamorio: 117d54d744df23921c531d0fe08537249f5a510a
# third-party: fb49c0ba7a9abd48a4ea662bf20e08246c81fc5a
# Cargo.toml: 4168dea2771f18a00fb1afdfd2218efba415ecbb
# rust-toolchain.toml: b7ca9302bc65522b829aa2fe3b8783fc77fcb7b9
# CMAKE/CMAKE_GENERATOR policy, MAX_PARALLEL_JOBS=16 and the existing
# 1050 effective-job-seconds budget are unchanged.
# CARRY TO dbb8e5b6962312f4ee0e6046098ddae18574f818 (2026-10-04): from
# 6c79fbafa8e49e60ccc79de984b7cdca3c9b0c5c; all seven recorded DynamoRIO build inputs
# below are byte-identical. This adopts the launch lock of
# https://github.com/rrnewton/reverie/issues/912 (reverie-process and
# reverie-ptrace) and three reverie-ptrace test commits; none touches a
# reverie-dbt input. Source identity carries the calibration; no new timing
# sample is claimed.
# reverie-dbt/Cargo.toml: 0e24d047d544a3daae2d6350270b26ceb74139d1
# reverie-dbt/build.rs: 0ff8ae24b97464044735ba79ea74765ba4ac3ff0
# reverie-dbt/native/CMakeLists.txt: bcfb298a4f87ed190d7fdc52393e01d1245a8fe3
# reverie-dbt/vendor/dynamorio: 117d54d744df23921c531d0fe08537249f5a510a
# third-party: fb49c0ba7a9abd48a4ea662bf20e08246c81fc5a
# Cargo.toml: 4168dea2771f18a00fb1afdfd2218efba415ecbb
# rust-toolchain.toml: b7ca9302bc65522b829aa2fe3b8783fc77fcb7b9
# CMAKE/CMAKE_GENERATOR policy, MAX_PARALLEL_JOBS=16 and the existing
# 1050 effective-job-seconds budget are unchanged.
# CARRY TO 51186070ef15bb4471b4d8801ff77ee385c793f2 (2026-10-04): from
# dbb8e5b6962312f4ee0e6046098ddae18574f818; all seven recorded DynamoRIO build inputs
# below are byte-identical. The 1-commit range (the reverie-kvm host metadata
# timestamp commit of https://github.com/rrnewton/hermit/issues/3695) touches
# no reverie-dbt input.
# Source identity carries the calibration; no new timing sample is claimed.
# reverie-dbt/Cargo.toml: 0e24d047d544a3daae2d6350270b26ceb74139d1
# reverie-dbt/build.rs: 0ff8ae24b97464044735ba79ea74765ba4ac3ff0
# reverie-dbt/native/CMakeLists.txt: bcfb298a4f87ed190d7fdc52393e01d1245a8fe3
# reverie-dbt/vendor/dynamorio: 117d54d744df23921c531d0fe08537249f5a510a
# third-party: fb49c0ba7a9abd48a4ea662bf20e08246c81fc5a
# Cargo.toml: 4168dea2771f18a00fb1afdfd2218efba415ecbb
# rust-toolchain.toml: b7ca9302bc65522b829aa2fe3b8783fc77fcb7b9
# CMAKE/CMAKE_GENERATOR policy, MAX_PARALLEL_JOBS=16 and the existing
# 1050 effective-job-seconds budget are unchanged.
# CARRY TO d646498e4c5ec2fbdd48d9eb0cfc26dfb4468918 (2026-10-04): from
# 51186070ef15bb4471b4d8801ff77ee385c793f2; all seven recorded DynamoRIO build inputs
# below are byte-identical. The 1-commit range (the reverie-kvm synthetic carrier
# provenance commit of https://github.com/rrnewton/reverie/issues/933) touches
# no reverie-dbt input.
# Source identity carries the calibration; no new timing sample is claimed.
# reverie-dbt/Cargo.toml: 0e24d047d544a3daae2d6350270b26ceb74139d1
# reverie-dbt/build.rs: 0ff8ae24b97464044735ba79ea74765ba4ac3ff0
# reverie-dbt/native/CMakeLists.txt: bcfb298a4f87ed190d7fdc52393e01d1245a8fe3
# reverie-dbt/vendor/dynamorio: 117d54d744df23921c531d0fe08537249f5a510a
# third-party: fb49c0ba7a9abd48a4ea662bf20e08246c81fc5a
# Cargo.toml: 4168dea2771f18a00fb1afdfd2218efba415ecbb
# rust-toolchain.toml: b7ca9302bc65522b829aa2fe3b8783fc77fcb7b9
# CMAKE/CMAKE_GENERATOR policy, MAX_PARALLEL_JOBS=16 and the existing
# 1050 effective-job-seconds budget are unchanged.
# CARRY TO 6b037bcad73db1ad6f23597e4a3ee624994c37da (2026-10-04): from
# d646498e4c5ec2fbdd48d9eb0cfc26dfb4468918; all seven recorded DynamoRIO build inputs
# below are byte-identical. The 1-commit range (the reverie-kvm carrier
# timestamp-class fix-forward for https://github.com/rrnewton/reverie/issues/933) touches
# no reverie-dbt input.
# Source identity carries the calibration; no new timing sample is claimed.
# reverie-dbt/Cargo.toml: 0e24d047d544a3daae2d6350270b26ceb74139d1
# reverie-dbt/build.rs: 0ff8ae24b97464044735ba79ea74765ba4ac3ff0
# reverie-dbt/native/CMakeLists.txt: bcfb298a4f87ed190d7fdc52393e01d1245a8fe3
# reverie-dbt/vendor/dynamorio: 117d54d744df23921c531d0fe08537249f5a510a
# third-party: fb49c0ba7a9abd48a4ea662bf20e08246c81fc5a
# Cargo.toml: 4168dea2771f18a00fb1afdfd2218efba415ecbb
# rust-toolchain.toml: b7ca9302bc65522b829aa2fe3b8783fc77fcb7b9
# CMAKE/CMAKE_GENERATOR policy, MAX_PARALLEL_JOBS=16 and the existing
# 1050 effective-job-seconds budget are unchanged.
# CARRY TO 2b7ad37be26698367fc6a3990760f972e9816dcb (2026-10-04): from
# 6b037bcad73db1ad6f23597e4a3ee624994c37da; all seven recorded DynamoRIO build inputs
# below are byte-identical. The 1-commit range (the launch lock's descriptor
# allocation retry of https://github.com/rrnewton/reverie/issues/930) changes
# reverie-process and reverie-ptrace and touches no reverie-dbt input.
# Source identity carries the calibration; no new timing sample is claimed.
# reverie-dbt/Cargo.toml: 0e24d047d544a3daae2d6350270b26ceb74139d1
# reverie-dbt/build.rs: 0ff8ae24b97464044735ba79ea74765ba4ac3ff0
# reverie-dbt/native/CMakeLists.txt: bcfb298a4f87ed190d7fdc52393e01d1245a8fe3
# reverie-dbt/vendor/dynamorio: 117d54d744df23921c531d0fe08537249f5a510a
# third-party: fb49c0ba7a9abd48a4ea662bf20e08246c81fc5a
# Cargo.toml: 4168dea2771f18a00fb1afdfd2218efba415ecbb
# rust-toolchain.toml: b7ca9302bc65522b829aa2fe3b8783fc77fcb7b9
# CMAKE/CMAKE_GENERATOR policy, MAX_PARALLEL_JOBS=16 and the existing
# 1050 effective-job-seconds budget are unchanged.
# CARRY TO 034ebf294e3e863805d2971d85499a0041f67e8b (2026-10-04): from
# 2b7ad37be26698367fc6a3990760f972e9816dcb; all seven recorded DynamoRIO build inputs
# below are byte-identical. The 1-commit range (the reverie-kvm carrier
# timestamp-class fix-forward for https://github.com/rrnewton/reverie/issues/933) touches
# no reverie-dbt input.
# Source identity carries the calibration; no new timing sample is claimed.
# reverie-dbt/Cargo.toml: 0e24d047d544a3daae2d6350270b26ceb74139d1
# reverie-dbt/build.rs: 0ff8ae24b97464044735ba79ea74765ba4ac3ff0
# reverie-dbt/native/CMakeLists.txt: bcfb298a4f87ed190d7fdc52393e01d1245a8fe3
# reverie-dbt/vendor/dynamorio: 117d54d744df23921c531d0fe08537249f5a510a
# third-party: fb49c0ba7a9abd48a4ea662bf20e08246c81fc5a
# Cargo.toml: 4168dea2771f18a00fb1afdfd2218efba415ecbb
# rust-toolchain.toml: b7ca9302bc65522b829aa2fe3b8783fc77fcb7b9
# CMAKE/CMAKE_GENERATOR policy, MAX_PARALLEL_JOBS=16 and the existing
# 1050 effective-job-seconds budget are unchanged.
# CARRY TO b8ef6a829634d51e79ebc4c24e11c5dd5fac205c (2026-10-04): from 034ebf294e3e863805d2971d85499a0041f67e8b; all seven
# recorded DynamoRIO build inputs are byte-identical. The 0.4.1 stable-build,
# package-metadata and pidfd compatibility work touches no DBT build input.
# Source identity carries the existing calibration; no new timing sample is claimed.
# reverie-dbt/Cargo.toml: 0e24d047d544a3daae2d6350270b26ceb74139d1
# reverie-dbt/build.rs: 0ff8ae24b97464044735ba79ea74765ba4ac3ff0
# reverie-dbt/native/CMakeLists.txt: bcfb298a4f87ed190d7fdc52393e01d1245a8fe3
# reverie-dbt/vendor/dynamorio: 117d54d744df23921c531d0fe08537249f5a510a
# third-party: fb49c0ba7a9abd48a4ea662bf20e08246c81fc5a
# Cargo.toml: 4168dea2771f18a00fb1afdfd2218efba415ecbb
# rust-toolchain.toml: b7ca9302bc65522b829aa2fe3b8783fc77fcb7b9
# CMAKE/CMAKE_GENERATOR policy, MAX_PARALLEL_JOBS=16 and the existing
# 1050 effective-job-seconds budget are unchanged.
# BOUND TO c8181d43a59a6d1ac602f2fad7615189f4996b41 (2026-10-04): from
# b8ef6a829634d51e79ebc4c24e11c5dd5fac205c. reverie-dbt/build.rs CHANGED, so the
# DynamoRIO SDK recipe key changes; the other six recorded inputs are
# byte-identical:
# reverie-dbt/Cargo.toml: 0e24d047d544a3daae2d6350270b26ceb74139d1
# reverie-dbt/build.rs: 0ff8ae24b97464044735ba79ea74765ba4ac3ff0 -> e05db6238bf07c96d8a850c5635a8c48590f20b7
# reverie-dbt/native/CMakeLists.txt: bcfb298a4f87ed190d7fdc52393e01d1245a8fe3
# reverie-dbt/vendor/dynamorio: 117d54d744df23921c531d0fe08537249f5a510a
# third-party: fb49c0ba7a9abd48a4ea662bf20e08246c81fc5a
# Cargo.toml: 4168dea2771f18a00fb1afdfd2218efba415ecbb
# rust-toolchain.toml: b7ca9302bc65522b829aa2fe3b8783fc77fcb7b9
# The build.rs change (Reverie "Cap the DynamoRIO build job count at the
# available CPUs") makes the cmake job count
# min(clamp(NUM_JOBS, 1, 16), std::thread::available_parallelism()). It can
# change build time only where NUM_JOBS exceeds the CPUs the build may use.
# Under this wrapper NUM_JOBS is the raw Cargo job count and the elapsed bound
# uses min(raw, nproc, 16): where nproc equals available_parallelism (affinity
# limits), cmake runs exactly the effective jobs the bound assumes. Where a
# cgroup CPU quota makes available_parallelism smaller than nproc, cmake now
# runs fewer jobs, but the quota already capped the CPU time the extra jobs
# could get, so elapsed time is about unchanged. The compiled DynamoRIO tree
# and cmake configuration are unchanged.
# BOUNDED COLD SDK OBSERVATION AT c8181d43: a cold `cargo build -p reverie-dbt
# -j 16` of the Reverie checkout with CI=true under `taskset -c 0-3`, default
# cmake and CMAKE_GENERATOR unset, on a 316-CPU host at load average ~170,
# reported MISS, then "completed in 38.18s (jobs=4, 152.73 job-seconds;
# NUM_JOBS=16, available CPUs=4)", and PUBLISHED for
#     key=sha256:f85df40daa25eff544e316659d674515091948a66bb7a3861f5e613dc3465b21
# 152.73 effective-job-seconds is below 1050; at 4 effective jobs the elapsed
# bound is ceil(1050/4)=263s. A second cold sample through this wrapper
# (`CARGO_BUILD_JOBS=16 ci/run-with-reverie-dbt-budget.sh cargo check --locked
# -p detcore-dbt`, child nproc=316, so min(16,316,16)=16 and a 66s bound)
# reported the same key and "completed in 13.28s (jobs=16, 212.53
# job-seconds; NUM_JOBS=16, available CPUs=316)".
# Retain the conservative 1050 effective-job-second threshold and the 16-job
# clamp. These two local samples do not replace the original n=3 hosted
# calibration or satisfy the >=5-sample replacement rule, and they are not a
# Hermit guest or replay result; fresh validation is required.
# CARRY TO c7a1ed25bca339daa0e69241353fc9882da63d7a (2026-10-05): from
# c8181d43a59a6d1ac602f2fad7615189f4996b41; all seven recorded DynamoRIO build inputs
# are byte-identical. Adaptive wait-mode selection and retained controller
# delivery change SDK and ptrace sources, not the DynamoRIO build recipe.
# The available-CPU cap and f85df40daa25eff544e316659d674515091948a66bb7a3861f5e613dc3465b21
# recipe, CMAKE/CMAKE_GENERATOR policy, 16-job clamp and 1050 effective-job-
# second budget are unchanged. Source identity carries the calibration;
# no new timing sample or >=5-sample replacement claim is made.
# CARRY TO aa1cffcd7893307ce06da3d6d74434b8ea567119 (2026-10-05): from
# c7a1ed25bca339daa0e69241353fc9882da63d7a; all seven recorded DynamoRIO build inputs
# are byte-identical. The one-commit range changes only the in-guest LiteInst
# runtime (reverie-liteinst: CPUID/RDTSC at sites whose patch would cross a
# cache line), not the DynamoRIO build recipe.
# The available-CPU cap and f85df40daa25eff544e316659d674515091948a66bb7a3861f5e613dc3465b21
# recipe, CMAKE/CMAKE_GENERATOR policy, 16-job clamp and 1050 effective-job-
# second budget are unchanged. Source identity carries the calibration;
# no new timing sample or >=5-sample replacement claim is made.
# CARRY TO e86aba35925080d8a760ea787057b5ded95858b7 (2026-10-05): from
# aa1cffcd7893307ce06da3d6d74434b8ea567119; all seven recorded DynamoRIO build inputs
# are byte-identical. The three-commit range deletes the ptrace-hosted
# LiteInst hybrid (reverie-liteinst launchers and host runtime, its
# constructor heap, and the LiteInst-only reverie-ptrace modules), not the
# DynamoRIO build recipe.
# The available-CPU cap and f85df40daa25eff544e316659d674515091948a66bb7a3861f5e613dc3465b21
# recipe, CMAKE/CMAKE_GENERATOR policy, 16-job clamp and 1050 effective-job-
# second budget are unchanged. Source identity carries the calibration;
# no new timing sample or >=5-sample replacement claim is made.
# CARRY TO addd1a04e31601c01afaf1442580563e8213ad6c (2026-10-05): from
# e86aba35925080d8a760ea787057b5ded95858b7; all seven recorded DynamoRIO build inputs
# are byte-identical. The one-commit range adds Backend::capabilities to
# reverie-core and each backend crate (in reverie-dbt, only src/launcher.rs),
# not the DynamoRIO build recipe.
# The available-CPU cap and f85df40daa25eff544e316659d674515091948a66bb7a3861f5e613dc3465b21
# recipe, CMAKE/CMAKE_GENERATOR policy, 16-job clamp and 1050 effective-job-
# second budget are unchanged. Source identity carries the calibration;
# no new timing sample or >=5-sample replacement claim is made.
# CARRY TO 5cd0b28a878f41c49fafc69bee740807846e5ffa (2026-10-05): from
# addd1a04e31601c01afaf1442580563e8213ad6c; all seven recorded DynamoRIO build inputs
# are byte-identical. The two-commit range (d639265a, 5cd0b28a) changes only
# reverie-liteinst, not reverie-dbt or the DynamoRIO build recipe.
# The available-CPU cap and f85df40daa25eff544e316659d674515091948a66bb7a3861f5e613dc3465b21
# recipe, CMAKE/CMAKE_GENERATOR policy, 16-job clamp and 1050 effective-job-
# second budget are unchanged. Source identity carries the calibration;
# no new timing sample or >=5-sample replacement claim is made.
# CARRY TO 0395a68f29dfb5f069b8deeb4ef6adef2327cdba (2026-10-05): from
# 5cd0b28a878f41c49fafc69bee740807846e5ffa; all seven recorded DynamoRIO build inputs
# are byte-identical. The two-commit range (1b258366, 0395a68f) changes
# reverie-ptrace (the Zen SpecLockMap check and PMU validation result) and
# reverie-liteinst, not reverie-dbt or the DynamoRIO build recipe.
# The available-CPU cap and f85df40daa25eff544e316659d674515091948a66bb7a3861f5e613dc3465b21
# recipe, CMAKE/CMAKE_GENERATOR policy, 16-job clamp and 1050 effective-job-
# second budget are unchanged. Source identity carries the calibration;
# no new timing sample or >=5-sample replacement claim is made.
# CARRY TO d2257c061c5bc6523cd9dc9b3a75dd898fd62d34 (2026-10-05): from
# 0395a68f29dfb5f069b8deeb4ef6adef2327cdba. The four inputs the budget governs, the DynamoRIO content-key miss
# hashed over reverie-dbt/vendor/dynamorio, reverie-dbt/build.rs, $CMAKE and
# $CMAKE_GENERATOR, are unchanged: vendor/dynamorio and build.rs have the same
# git object ids at both revisions, as do reverie-dbt/Cargo.toml,
# reverie-dbt/native/CMakeLists.txt, third-party and rust-toolchain.toml. The
# root Cargo.toml differs by one line, the workspace member reverie-preload
# renamed to reverie-inguest; that cannot change the elapsed time of a
# DynamoRIO content-key miss. The six-commit range (e1fbd91c, b6be55a9,
# bffb408d, bb4cb136, 5399822f, d2257c06) renames reverie-preload to
# reverie-inguest and moves LiteInst's in-guest Tool host, fallback
# continuation and RCB clock into it; it does not touch reverie-dbt.
# The available-CPU cap and f85df40daa25eff544e316659d674515091948a66bb7a3861f5e613dc3465b21
# recipe, CMAKE/CMAKE_GENERATOR policy, 16-job clamp and 1050 effective-job-
# second budget are unchanged. Carry, not recalibration: no new timing sample
# or >=5-sample replacement claim is made.
# CARRY TO 7e0f57e9e1318d930f304f05fa9a9cfb257261d3 (2026-10-06): from
# d2257c061c5bc6523cd9dc9b3a75dd898fd62d34. The four inputs the budget governs, the DynamoRIO content-key miss
# hashed over reverie-dbt/vendor/dynamorio, reverie-dbt/build.rs, $CMAKE and
# $CMAKE_GENERATOR, are unchanged: vendor/dynamorio and build.rs have the same
# git object ids at both revisions, as do reverie-dbt/Cargo.toml,
# reverie-dbt/native/CMakeLists.txt, third-party, rust-toolchain.toml and the
# root Cargo.toml. The seven-commit range (1ef1f2a0, f7ec2823, 0f3d0b4d,
# bc7cd3b8, cffe8539, f7d8b07e, 7e0f57e9) moves LiteInst's trap path (runtime
# support, instruction control and fault handler, signal rules, descriptor
# protection, SIGSYS dispatcher) into reverie-inguest and makes trap-only mode
# emulate in-arena CPUID/RDTSC; it does not touch reverie-dbt.
# The available-CPU cap and f85df40daa25eff544e316659d674515091948a66bb7a3861f5e613dc3465b21
# recipe, CMAKE/CMAKE_GENERATOR policy, 16-job clamp and 1050 effective-job-
# second budget are unchanged. Carry, not recalibration: no new timing sample
# or >=5-sample replacement claim is made.
# CARRY TO eb4372dcabea6708374f9fe649256f92f2040497 (2026-10-06): from
# 7e0f57e9e1318d930f304f05fa9a9cfb257261d3. The four inputs the budget governs, the DynamoRIO content-key miss
# hashed over reverie-dbt/vendor/dynamorio, reverie-dbt/build.rs, $CMAKE and
# $CMAKE_GENERATOR, are unchanged: vendor/dynamorio and build.rs have the same
# git object ids at both revisions, as do reverie-dbt/Cargo.toml, third-party,
# rust-toolchain.toml and the root Cargo.toml. The two-commit range (f1353b48,
# eb4372dc) adds reverie-dbt's coordinator evidence append (reverie-dbt/src)
# and routes the client's rdtsc through the runtime (reverie-dbt/src and
# reverie-dbt/native/client.c, the client source, which is not part of the
# DynamoRIO content key); it does not touch the DynamoRIO build recipe.
# The available-CPU cap and f85df40daa25eff544e316659d674515091948a66bb7a3861f5e613dc3465b21
# recipe, CMAKE/CMAKE_GENERATOR policy, 16-job clamp and 1050 effective-job-
# second budget are unchanged. Carry, not recalibration: no new timing sample
# or >=5-sample replacement claim is made.
# CARRY TO 0c2d3db220a64da6aaf0a741b6d0ad225dd85e71 (2026-10-06): from
# eb4372dcabea6708374f9fe649256f92f2040497. The four inputs the budget governs, the DynamoRIO content-key miss
# hashed over reverie-dbt/vendor/dynamorio, reverie-dbt/build.rs, $CMAKE and
# $CMAKE_GENERATOR, are unchanged: the one-commit range changes only
# reverie-kvm (the KVM guest's stack placement and initial-stack layout); it
# touches nothing under reverie-dbt and not the DynamoRIO build recipe.
# The available-CPU cap and f85df40daa25eff544e316659d674515091948a66bb7a3861f5e613dc3465b21
# recipe, CMAKE/CMAKE_GENERATOR policy, 16-job clamp and 1050 effective-job-
# second budget are unchanged. Carry, not recalibration: no new timing sample
# or >=5-sample replacement claim is made.
# CARRY TO ffea36eee6d12c88f0d067837b76c6d0eb073866 (2026-10-06): from
# 0c2d3db220a64da6aaf0a741b6d0ad225dd85e71. The one commit changes only reverie/src and
# reverie-kvm (dequeue observation follows the installed signal control, and
# a run that requires the control is refused when the Tool declines it); no
# file under reverie-dbt, the root Cargo.toml, third-party or
# rust-toolchain.toml changes, so the four budget inputs are unchanged.
# Carry, not recalibration: no new timing sample or >=5-sample replacement
# claim is made.
# CARRY TO df4044f5a7a958201ac130dcf2aec2f7f7390dad (2026-10-06): from
# ffea36eee6d12c88f0d067837b76c6d0eb073866. The one commit changes only reverie-kvm (the
# KVM guest's ld.so and mmap placement); no file under reverie-dbt, the root
# Cargo.toml, third-party or rust-toolchain.toml changes, so the four budget
# inputs are unchanged. Carry, not recalibration: no new timing sample or
# >=5-sample replacement claim is made.
# CARRY TO d6eedef99c111cb8b7cd76c5e103d2b781be7890 (2026-10-06): from
# df4044f5a7a958201ac130dcf2aec2f7f7390dad. The two commits change only reverie-kvm (the
# guest's brk lower bound and heap observation) and reverie-ptrace (the last
# precise-timer margin); no file under reverie-dbt,
# the root Cargo.toml, third-party or rust-toolchain.toml changes, so the four
# budget inputs are unchanged. Carry, not recalibration: no new timing sample
# or >=5-sample replacement claim is made.
# CARRY TO 056c8caccf262087e8e815b514ff31da07d8bd54 (2026-10-06): from
# d6eedef99c111cb8b7cd76c5e103d2b781be7890. The one commit changes reverie/src,
# reverie-rpc-transport and reverie-liteinst (in-guest asynchronous exit
# completion); no file under reverie-dbt, the root Cargo.toml, third-party or
# rust-toolchain.toml changes, so the four budget inputs are unchanged. Carry,
# not recalibration: no new timing sample or >=5-sample replacement claim is
# made.
# CARRY TO a9f2ef666a83920146f2e9362487ec867c6ccced (2026-10-06): from
# 056c8caccf262087e8e815b514ff31da07d8bd54. The one commit changes reverie-kvm and adds
# reverie/src/task_ids.rs (guest task-ID numbering); no file under
# reverie-dbt, the root Cargo.toml, third-party or rust-toolchain.toml
# changes, so the four budget inputs are unchanged. Carry, not recalibration:
# no new timing sample or >=5-sample replacement claim is made.
# CARRY TO ba18ad41ef79d932927ba3fd890bd437e26233a4 (2026-10-06): from
# a9f2ef666a83920146f2e9362487ec867c6ccced. The one commit changes only reverie-kvm (a
# PIE main image's placement); no file under reverie-dbt, the root Cargo.toml,
# third-party or rust-toolchain.toml changes, so the four budget inputs are
# unchanged. Carry, not recalibration: no new timing sample or >=5-sample
# replacement claim is made.
# CARRY TO ace0cc59c18bac0cee5503a192868a2f82d95691 (2026-10-06): from
# ba18ad41ef79d932927ba3fd890bd437e26233a4. The one commit changes only reverie-kvm (which
# thread owns a guest's ppoll); no file under reverie-dbt, the root
# Cargo.toml, third-party or rust-toolchain.toml changes, so the four budget
# inputs are unchanged. Carry, not recalibration: no new timing sample or
# >=5-sample replacement claim is made.
# CARRY TO 30c8241117e8126a1802ccb9831e15047908c33a (2026-10-06): from
# ace0cc59c18bac0cee5503a192868a2f82d95691. The one commit changes only
# reverie-rpc-transport (a connection-admission failure hook); no file under
# reverie-dbt, the root Cargo.toml, third-party or rust-toolchain.toml
# changes, so the four budget inputs are unchanged. Carry, not recalibration:
# no new timing sample or >=5-sample replacement claim is made.
# CARRY TO ee1d28738595930689aa58d03e62285d0788bbdf (2026-10-06): from
# 30c8241117e8126a1802ccb9831e15047908c33a. The one commit changes reverie-
# inguest and reverie-liteinst (a guest close or close_range over the
# runtime's descriptors reaches the Tool); no file under reverie-dbt, the root
# Cargo.toml, third-party or rust-toolchain.toml changes, so the four budget
# inputs are unchanged. Carry, not recalibration: no new timing sample or
# >=5-sample replacement claim is made.
# CARRY TO c683c24288e462b2658444da52eaeadfea1802d7 (2026-10-06): from
# ee1d28738595930689aa58d03e62285d0788bbdf. The one commit changes reverie-
# inguest and reverie-liteinst (blocking_global_rpc, synchronous coordinator
# access for Tool code inside a callback; no caller yet); no file under
# reverie-dbt, the root Cargo.toml, third-party or rust-toolchain.toml
# changes, so the four budget inputs are unchanged. Carry, not recalibration:
# no new timing sample or >=5-sample replacement claim is made.
# CARRY TO c496dd8e91ec8c318633460a07353b9f83b36cb4 (2026-10-07): from
# c683c24288e462b2658444da52eaeadfea1802d7. The three commits change only reverie-kvm (madvise
# MADV_DONTNEED and its allocation guard, brk) and add one field to
# reverie/src/capabilities.rs; no file under reverie-dbt, the root Cargo.toml,
# third-party or rust-toolchain.toml changes, so the four budget inputs are
# unchanged. Carry, not recalibration: no new timing sample or >=5-sample
# replacement claim is made.
# CARRY TO 31acc5e3f344aff55ca011abf5c7cd1553e33855 (2026-10-07): from
# c496dd8e91ec8c318633460a07353b9f83b36cb4. The one commit on top (the same patch as
# 71791d21e36e99a570d379746c173e1793945b93, patch id 7d263929aee8728ce4948203aee3e3d5db868651)
# changes reverie-dbt only in native/client.c, src/backend_stats.rs,
# src/launcher.rs and src/lib.rs: a terminal exit sets a flag in the image's
# stats record, and a failed run keeps its captured output. The reverie-dbt
# tree moves 3573f362207f61db433c8db6ffb3df77c6502e63 -> f5e3bad9517ec8b41b8c2c0827b814d676d7df57.
# No changed file is an input to the DynamoRIO SDK recipe key above. Compared by
# git object id at both pins: reverie-dbt/Cargo.toml is blob
# 0e24d047d544a3daae2d6350270b26ceb74139d1, build.rs is blob
# e05db6238bf07c96d8a850c5635a8c48590f20b7, native/CMakeLists.txt is blob
# bcfb298a4f87ed190d7fdc52393e01d1245a8fe3, vendor/dynamorio is tree
# 117d54d744df23921c531d0fe08537249f5a510a, third-party is tree
# fb49c0ba7a9abd48a4ea662bf20e08246c81fc5a, the root Cargo.toml is blob
# 395eddb164895c7c59ff7db11d0a6105d0c69d30 and rust-toolchain.toml is blob
# b7ca9302bc65522b829aa2fe3b8783fc77fcb7b9. Client preparation still rebuilds
# the on-demand client from the new client.c. Carry, not recalibration: no new
# timing sample or >=5-sample replacement claim is made.
# CARRY TO 015d9c00e7e83890246db68a11cfbb2317fe02ee (2026-10-07): from
# 31acc5e3f344aff55ca011abf5c7cd1553e33855. The one commit changes reverie-dbt only in src/launcher.rs: an
# output reader stopped after a failed run first reads what is already in its
# pipe, bounded to 1 MiB, and its tests no longer sleep. The reverie-dbt tree
# moves f5e3bad9517ec8b41b8c2c0827b814d676d7df57 -> cc882d339affba476229fe5ff209d888d8fa0a38.
# No changed file is an input to the DynamoRIO SDK recipe key above. Compared by
# git object id at both pins: reverie-dbt/Cargo.toml is blob
# 0e24d047d544a3daae2d6350270b26ceb74139d1, build.rs is blob
# e05db6238bf07c96d8a850c5635a8c48590f20b7, native/CMakeLists.txt is blob
# bcfb298a4f87ed190d7fdc52393e01d1245a8fe3, vendor/dynamorio is tree
# 117d54d744df23921c531d0fe08537249f5a510a, third-party is tree
# fb49c0ba7a9abd48a4ea662bf20e08246c81fc5a, the root Cargo.toml is blob
# 395eddb164895c7c59ff7db11d0a6105d0c69d30 and rust-toolchain.toml is blob
# b7ca9302bc65522b829aa2fe3b8783fc77fcb7b9. The native client is unchanged.
# Carry, not recalibration: no new timing sample or >=5-sample replacement
# claim is made.
# CARRY TO 9cb6f7b7db6549e373ae57fa7035f09aca7d508f (2026-10-07): from
# 015d9c00e7e83890246db68a11cfbb2317fe02ee. The one commit changes no reverie-dbt file: the reverie-dbt
# tree is cc882d339affba476229fe5ff209d888d8fa0a38 at both pins, and
# third-party, rust-toolchain.toml and the root Cargo.toml are unchanged. It
# changes reverie-ptrace (a child-exit publication report) and
# reverie/src/capabilities.rs and tool.rs. Carry, not recalibration: no new
# timing sample or >=5-sample replacement claim is made.
# CARRY TO 0f7ffe7e3f82320c45d5d220b846b2a055b29501 (2026-10-07): from
# 9cb6f7b7db6549e373ae57fa7035f09aca7d508f. The two commits change only
# reverie-dbt/src/evidence.rs: the evidence collector writes each process
# image's initialization record immediately before that image's first
# comparable record, or at the end of the stream if it has none, instead of
# where the host happened to accept it, and charges a held record against its
# memory bound when it accepts it. The reverie-dbt tree moves
# cc882d339affba476229fe5ff209d888d8fa0a38 -> 18468c68a51117f33521dc2327d76c67423faa82.
# No changed file is an input to the DynamoRIO SDK recipe key above. Compared by
# git object id at both pins: reverie-dbt/Cargo.toml is blob
# 0e24d047d544a3daae2d6350270b26ceb74139d1, build.rs is blob
# e05db6238bf07c96d8a850c5635a8c48590f20b7, native/CMakeLists.txt is blob
# bcfb298a4f87ed190d7fdc52393e01d1245a8fe3, vendor/dynamorio is tree
# 117d54d744df23921c531d0fe08537249f5a510a, third-party is tree
# fb49c0ba7a9abd48a4ea662bf20e08246c81fc5a, the root Cargo.toml is blob
# 395eddb164895c7c59ff7db11d0a6105d0c69d30 and rust-toolchain.toml is blob
# b7ca9302bc65522b829aa2fe3b8783fc77fcb7b9. The native client is unchanged
# (reverie-dbt/native is tree 27a61eb170f7372b5120f0ef92a900e9e8d6071c at both).
# Carry, not recalibration: no new timing sample or >=5-sample replacement
# claim is made.
# CARRY TO d78f770f2fde06db9d2787e0fea0ca0984891763 (2026-10-07): from
# 0f7ffe7e3f82320c45d5d220b846b2a055b29501. The one commit changes reverie/src (typed
# unsupported-operation refusals and one removed BackendCapabilities field)
# and reverie-kvm; no file under reverie-dbt, the root Cargo.toml,
# third-party or rust-toolchain.toml changes, so the four budget inputs are
# unchanged. Carry, not recalibration: no new timing sample or >=5-sample
# replacement claim is made.
# CARRY TO 8bfb450ed8cbcebfbf474ea083e68cf332324e51 (2026-10-07): from
# d78f770f2fde06db9d2787e0fea0ca0984891763. The one commit changes reverie/src (one removed
# BackendCapabilities field and the Guest::storable_memory_ranges query) and
# reverie-kvm (time(2); time and gettimeofday stores only to writable pages,
# shared file mappings included, through a new UserMemory::store_user_u64; and
# the storable-range report); no file under
# reverie-dbt, the root Cargo.toml, third-party or rust-toolchain.toml
# changes, so the four budget inputs are unchanged. Carry, not recalibration:
# no new timing sample or >=5-sample replacement claim is made.
# CARRY TO 6f75cc4c50449ddde9334a311f7a2860c7217aee (2026-10-07): from
# 8bfb450ed8cbcebfbf474ea083e68cf332324e51. The one commit changes reverie/src (one removed
# BackendCapabilities field and the Guest::user_address_limit query) and
# reverie-kvm (its guest's user address limit report); no file under
# reverie-dbt, the root Cargo.toml, third-party or rust-toolchain.toml
# changes, so the four budget inputs are unchanged. Carry, not recalibration:
# no new timing sample or >=5-sample replacement claim is made.
# CARRY TO 689d7f0bafa7c315450eaa75518274e65b8a13d3 (2026-10-07): from
# 6f75cc4c50449ddde9334a311f7a2860c7217aee. The one commit changes two things:
# an optional in-guest process-creation hook API (B3b, inert until a hook is
# registered) and a LiteInst launch that ends the run when Detcore reports a
# backend failure; no file under reverie-dbt, the root Cargo.toml, third-party
# or rust-toolchain.toml changes, so the four budget inputs are unchanged.
# Carry, not recalibration: no new timing sample or >=5-sample replacement
# claim is made.
# CARRY TO 2c86fe0fa14d987d8bf8befb0c3a9ca297814202 (2026-10-07): from
# 689d7f0bafa7c315450eaa75518274e65b8a13d3. The one commit makes the KVM backend
# name the execve filename in AT_EXECFN (reverie-kvm only); no file under
# reverie-dbt, the root Cargo.toml, third-party or rust-toolchain.toml
# changes, so the four budget inputs are unchanged. Carry, not recalibration:
# no new timing sample or >=5-sample replacement claim is made.
# CARRY TO 1cc1918a4a5900963d7d0c682b642f25ea46bc1b (2026-10-07): from
# 2c86fe0fa14d987d8bf8befb0c3a9ca297814202. The one commit changes reverie/src/pmu.rs
# (an Emerald Rapids PMU profile) and reverie-ptrace (a typed refusal for a CPU
# with no profile, a recorded skid margin and host_pmu_profile); no file under
# reverie-dbt, the root Cargo.toml, third-party or rust-toolchain.toml
# changes, so the four budget inputs are unchanged. Carry, not recalibration:
# no new timing sample or >=5-sample replacement claim is made.
# CARRY TO 589ee25275ff6670b57da5f692fb2a6ceea63cba (2026-10-07): from
# 1cc1918a4a5900963d7d0c682b642f25ea46bc1b. The one commit changes reverie-inguest, reverie-liteinst and
# reverie-e9patch (signal phase 1 step I2: a private signal restorer and the
# Tool-mode rt_sigreturn rule); no file under reverie-dbt, the root
# Cargo.toml, third-party or rust-toolchain.toml changes, so the four budget
# inputs are unchanged. Carry, not recalibration: no new timing sample or
# >=5-sample replacement claim is made.
# CARRY TO 7142ff8c0a78b275c94e796bf10053fabde35748 (2026-10-08): from
# 589ee25275ff6670b57da5f692fb2a6ceea63cba. The five commits change reverie-inguest, reverie-liteinst,
# experimental/reverie-sabre (a protected output socket for its in-guest tool) and
# reverie/src/capabilities.rs (signal phase 1 steps I3a and I3b: guest SIGALRM
# handlers kept virtual behind an off-by-default admission, and the
# virtualizes_guest_sigalrm capability); no file under reverie-dbt, the root
# Cargo.toml, third-party or rust-toolchain.toml changes, so the four budget
# inputs are unchanged. Carry, not recalibration: no new timing sample or
# >=5-sample replacement claim is made.
# CARRY TO 762fa7cf2e861552811878c2ac11c0dbdaa940f1 (2026-10-08): from
# 7142ff8c0a78b275c94e796bf10053fabde35748. The two commits change reverie-inguest, reverie-liteinst and
# reverie/src/guest.rs (signal phase 1 step I4: the runtime delivers a guest
# SIGALRM handler's signal, at a syscall's completion or before it, through a
# new default-ENOSYS Guest method); no file under reverie-dbt, the root
# Cargo.toml, third-party or rust-toolchain.toml changes, so the four budget
# inputs are unchanged. Carry, not recalibration: no new timing sample or
# >=5-sample replacement claim is made.
# CARRY TO 684186e9e7dcbb242a5cc9507a4b3cd57ee561f1 (2026-10-08): from
# 762fa7cf2e861552811878c2ac11c0dbdaa940f1. The two commits change reverie-liteinst (ace025d1:
# read-only getters for the settings the runtime captured) and reverie,
# reverie-ptrace and safeptrace (684186e9: a Tool reads, replaces and filters
# the siginfo of the signals it sees, through new default methods); no file
# under reverie-dbt, the root Cargo.toml, third-party or rust-toolchain.toml
# changes, so the four budget inputs are unchanged. Carry, not recalibration:
# no new timing sample or >=5-sample replacement claim is made.
# CARRY TO 23ca5355678059c7acd598d3a989af7143885947 (2026-10-08): from
# 684186e9e7dcbb242a5cc9507a4b3cd57ee561f1. The three commits change reverie-ptrace (a reported
# signal's requeue count retired at every delivery dequeue, never at a group
# stop, and a test of it) and experimental/reverie-sabre (the vendored SaBRe
# loader's RDTSC entry points routed through the plugin boundary, and a
# re-entrant RDTSC refused); no file under reverie-dbt, the root Cargo.toml, third-party or rust-toolchain.toml
# changes, so the four budget inputs are unchanged. Carry, not recalibration:
# no new timing sample or >=5-sample replacement claim is made.
# CARRY TO 87e2b5c47febd685ed3ee746e47255bb6461958e (2026-10-08): from
# 23ca5355678059c7acd598d3a989af7143885947. The three commits change reverie-ptrace (core-limit
# handling for the fatal-signal tests and their plain-guest fixture) and
# experimental/reverie-sabre's vendored SaBRe loader (a pre-plugin
# resource-limit read forwarded through the bootstrap channel, a pre-plugin
# change refused, and a version 2 frame descriptor); no file under
# reverie-dbt, the root Cargo.toml, third-party or rust-toolchain.toml changes,
# so the four budget inputs are unchanged. Carry, not recalibration: no new
# timing sample or >=5-sample replacement claim is made.
# BOUND TO d9f0affc97430885ca79d02dffa7611419a07129 (2026-10-08): from
# 87e2b5c47febd685ed3ee746e47255bb6461958e. reverie-dbt/build.rs and the root
# Cargo.toml CHANGED, so the DynamoRIO SDK recipe key changes; the other five
# recorded inputs are byte-identical:
# reverie-dbt/Cargo.toml: 0e24d047d544a3daae2d6350270b26ceb74139d1
# reverie-dbt/build.rs: e05db6238bf07c96d8a850c5635a8c48590f20b7 -> ff1eeb32b140ae477894126af21296723c1ba582
# reverie-dbt/native/CMakeLists.txt: bcfb298a4f87ed190d7fdc52393e01d1245a8fe3
# reverie-dbt/vendor/dynamorio: 117d54d744df23921c531d0fe08537249f5a510a
# third-party: fb49c0ba7a9abd48a4ea662bf20e08246c81fc5a
# Cargo.toml: 395eddb164895c7c59ff7db11d0a6105d0c69d30 -> 7177e96f4575158230e2473b4cbfdb1ad902710d
# rust-toolchain.toml: b7ca9302bc65522b829aa2fe3b8783fc77fcb7b9
# The build.rs change (Reverie d712c0ec "build DynamoRIO reproducibly by
# mapping its build and source paths", and 7e3dda49, which tests the build
# commands' environment and refuses '$' paths) appends -ffile-prefix-map and
# -fdebug-prefix-map for the staging and source directories to CFLAGS and
# CXXFLAGS; the DynamoRIO source, cmake options and job-count rule are
# unchanged. The root Cargo.toml change only adds the workspace member
# reverie-elf-loader (d9f0affc), which nothing in the DBT build depends on.
# The other 18 commits in the range (safeptrace e9d00dc3 and a7a5028f, and
# the 16-commit in-guest heap series ending at 24d9649a) touch no recorded
# input.
# BOUNDED COLD SDK OBSERVATION AT d9f0affc: a cold `cargo build -p reverie-dbt
# -j 16` of a fresh Reverie checkout with CI=true under `taskset -c 0-3`,
# default cmake 3.31.8 and CMAKE/CMAKE_GENERATOR unset, on a 316-CPU host at
# load average ~230, reported MISS, then "completed in 37.95s (jobs=4, 151.80
# job-seconds; NUM_JOBS=16, available CPUs=4)", and PUBLISHED for
#     key=sha256:c941bff007f8dd7737312cc1c59824ca796cad1d4688a27f872a558c07fe7cae
# 151.80 effective-job-seconds is below 1050; at 4 effective jobs the elapsed
# bound is ceil(1050/4)=263s. A second cold sample through this wrapper, in a
# fresh Hermit checkout with no target directory
# (`CARGO_BUILD_JOBS=16 ci/run-with-reverie-dbt-budget.sh cargo check --locked
# -p detcore-dbt`, child nproc=316, so min(16,316,16)=16 and a 66s bound),
# reported the same key and "completed in 12.65s (jobs=16, 202.33
# job-seconds; NUM_JOBS=16, available CPUs=316)".
# Retain the conservative 1050 effective-job-second threshold and the 16-job
# clamp. These two local samples do not replace the original n=3 hosted
# calibration or satisfy the >=5-sample replacement rule, and they are not a
# Hermit guest or replay result; fresh validation is required.
# BOUND TO 54adc5eebf40c8f407399b02dcd9ca1f62b7f157 (2026-10-08): from
# d9f0affc97430885ca79d02dffa7611419a07129. The one commit (54adc5ee "reverie-dbt: hide
# DynamoRIO's and the runtime's private variables from the guest's
# environment") changes the vendored DynamoRIO source, so the DynamoRIO SDK
# recipe key changes; the other six recorded inputs are byte-identical:
# reverie-dbt/Cargo.toml: 0e24d047d544a3daae2d6350270b26ceb74139d1
# reverie-dbt/build.rs: ff1eeb32b140ae477894126af21296723c1ba582
# reverie-dbt/native/CMakeLists.txt: bcfb298a4f87ed190d7fdc52393e01d1245a8fe3
# reverie-dbt/vendor/dynamorio: 117d54d744df23921c531d0fe08537249f5a510a -> ec2cc0a9bca71c9a6fad3dc39e713f77a8cccf50
# third-party: fb49c0ba7a9abd48a4ea662bf20e08246c81fc5a
# Cargo.toml: 7177e96f4575158230e2473b4cbfdb1ad902710d
# rust-toolchain.toml: b7ca9302bc65522b829aa2fe3b8783fc77fcb7b9
# The vendored change is to core/unix/loader.c and core/unix/os.c (hide the
# injector's variables at early injection; pass them to injected children);
# the cmake options and job-count rule are unchanged.
# BOUNDED COLD SDK OBSERVATION AT 54adc5ee: a cold
# `cargo check -p reverie-dbt --offline` (CARGO_BUILD_JOBS=16 NUM_JOBS=16, a
# fresh target directory) on a 316-CPU host at load average ~240 reported
# MISS, then "completed in 12.60s (jobs=16, 201.61 job-seconds; NUM_JOBS=16,
# available CPUs=316)", and PUBLISHED for
#     key=sha256:75c3067d4692cd8aa168d6791e718f21e6535e672b957291142070a0edc27234
# 201.61 effective-job-seconds is below 1050. Retain the conservative 1050
# effective-job-second threshold and the 16-job clamp. This local sample does
# not replace the original n=3 hosted calibration or satisfy the >=5-sample
# replacement rule; fresh validation is required.
# CARRY TO ae8a078ea8f68dab5cc73007fa7d56935d62f11f (2026-10-09): from
# 54adc5eebf40c8f407399b02dcd9ca1f62b7f157. The one commit changes reverie-dbt's native client
# (native/client.c and native/virtual_identity.h: asynchronous-I/O owners set
# through fcntl F_SETOWN/F_SETOWN_EX and the socket ioctls FIOSETOWN/SIOCSPGRP
# are translated from guest to host IDs and the owner queries back,
# https://github.com/rrnewton/hermit/issues/3955) and adds reverie-dbt tests.
# None of the seven recorded inputs changes (reverie-dbt/Cargo.toml,
# reverie-dbt/build.rs, reverie-dbt/native/CMakeLists.txt,
# reverie-dbt/vendor/dynamorio, third-party, the root Cargo.toml and
# rust-toolchain.toml are byte-identical), so the DynamoRIO SDK recipe key is
# unchanged. Carry, not recalibration: no new timing sample or >=5-sample
# replacement claim is made.
# CARRY TO 7215adf9345a93e283338acfe7afb64ea4045d0d (2026-10-09): from
# ae8a078ea8f68dab5cc73007fa7d56935d62f11f. The three commits change
# reverie-inguest (its seccomp TSYNC test runs under the test harness, with
# the test's check in a child process without it; and functions that save
# and restore its RCB clock and restore a thread's state, which nothing
# calls yet), reverie-elf-loader (a step that prepares an exec before it
# commits, which nothing calls yet), reverie-ptrace (an injected syscall
# that a signal stopped before it ran is reported as a restart,
# ERESTARTSYS, rather than as what the result register held, and tests of
# it) and reverie/src/guest.rs (Guest::inject documents that result). No
# file under reverie-dbt changes (tree
# 9a3212a9b0f8070c4cce0ae1bf6957d067b5e2d8 at both), and third-party, the root
# Cargo.toml and rust-toolchain.toml are byte-identical, so none of the seven
# recorded inputs changes and the DynamoRIO SDK recipe key is unchanged; a
# reverie-dbt build at 7215adf9's tree computed
#     key=sha256:75c3067d4692cd8aa168d6791e718f21e6535e672b957291142070a0edc27234
# Carry, not recalibration: no new timing sample or >=5-sample replacement
# claim is made.
# CARRY TO 5ba26897e1db46071f2a00f9ef4bee10e29c94ad (2026-10-09): from
# 7215adf9345a93e283338acfe7afb64ea4045d0d. The frozen in-guest self-signal
# contract changes no SDK input recorded above. reverie-dbt, third-party,
# root Cargo.toml and rust-toolchain.toml are byte-identical by Git object.
# Carry the existing budget and job clamp; this is source evidence, not a
# new timing calibration. Validate the changed in-guest behavior separately.
# CARRY TO 255d2e0b8f5ba017bcc7233711c63583e4fe419e (2026-10-09): from
# 5ba26897e1db46071f2a00f9ef4bee10e29c94ad. Five commits, none in reverie-dbt:
# reverie-liteinst tests its self-signal cases with default dispositions
# (https://github.com/rrnewton/reverie/pull/988), and its guest preload links
# its unwinder statically, records no RUNPATH and finds glibc's
# _dl_find_object through reverie-core's new glibc_symbol module
# (https://github.com/rrnewton/reverie/pull/984). None of the seven recorded
# inputs changes (reverie-dbt/Cargo.toml, reverie-dbt/build.rs,
# reverie-dbt/native/CMakeLists.txt, reverie-dbt/vendor/dynamorio,
# third-party, the root Cargo.toml and rust-toolchain.toml are
# byte-identical), so the DynamoRIO SDK recipe key is unchanged. Carry, not
# recalibration: no new timing sample or >=5-sample replacement claim is made.
# CARRY TO 630259bfb5006c3b2b820aae3ddcf5b20a53eaef (2026-10-09): from
# 255d2e0b8f5ba017bcc7233711c63583e4fe419e. The owned-callback pkey_alloc repair
# changes reverie-inguest and LiteInst tests/docs; the two new main
# commits change only syscall-display and capability documentation. reverie-dbt,
# third-party, root Cargo.toml and rust-toolchain.toml are byte-identical
# by Git object, including all seven recorded SDK inputs. Carry the existing
# budget and job clamp; this is not a new timing calibration.
# CARRY TO 0352d5da9b9422fe690bbc87511479e80b0080fe (2026-10-10): from
# 630259bfb5006c3b2b820aae3ddcf5b20a53eaef. The allocator-owned preload leaf
# and checked pinned mounts change no native DynamoRIO SDK recipe input:
# reverie-dbt/Cargo.toml, build.rs, native/CMakeLists.txt, vendor/dynamorio,
# third-party and rust-toolchain.toml are byte-identical by Git object.
# Root Cargo.toml adds the preload workspace member; it is not hashed by
# source_recipe_key (vendor, build.rs, CMAKE, CMAKE_GENERATOR,
# SOURCE_DATE_EPOCH). With the same tooling and epoch, carry the existing
# budget and sixteen-job clamp; this is not a new timing calibration.
# NATIVE-RW REGRESSION CARRY TO 4203a25399da4e01fab49c0b85f8ee503c8869bc (2026-10-10):
# From 0352d5da9b9422fe690bbc87511479e80b0080fe, build.rs, Cargo.toml,
# vendor/dynamorio, third-party and rust-toolchain.toml are unchanged Git
# objects. The tracked reverie-dbt file population remains939. This carries
# the same SDK recipe budget, not an earlier runtime validation receipt.
# FIXED-STACK CARRY TO f2cb0dc267f6d502a3d27b95b01121d584de2265:
# reverie-dbt, third-party and rust-toolchain.toml are identical to 4203a253;
# the tracked DBT inventory remains 939. No timing budget was changed.
# CONSTRUCTOR-STACK CARRY TO b7ac1604f2c969f6e96dc3978c0df60b051fb0c1 (2026-10-10):
# From f2cb0dc267f6d502a3d27b95b01121d584de2265, the complete reverie-dbt and
# third-party trees and rust-toolchain.toml are Git-object identical.
# The tracked DBT inventory remains 939; timing budgets are unchanged.
# This is source identity, not a new runtime receipt or timing calibration.
# PUBLISHED ALLOCATOR/DBT-STARTUP CARRY TO bb656c7bf3f008af11d12866b3156f2905246f9f (2026-10-10):
# From b7ac1604f2c969f6e96dc3978c0df60b051fb0c1, the native DBT client and Rust
# startup protocol change; the complete reverie-dbt tree is NOT identical.
# The SDK source_recipe_key repository inputs, vendor/dynamorio and build.rs,
# are Git-object identical. Preserve CMAKE, CMAKE_GENERATOR and normalized
# SOURCE_DATE_EPOCH, the remaining key inputs, for the existing calibration.
# Native CMake topology, third-party trees and rust-toolchain.toml also match.
# Actual tracked DBT inventory remains 939. Keep the 16-job clamp, 1050
# effective-job-second wrapper budget and 572-job-second cold-build fallback.
# This SDK recipe carry is not native/runtime identity or a fresh timing sample.
# PUBLISHED EXEC-HELPER CARRY TO 6f7c2617e933cc4a9107e4a5d67aec1f146b5528 (2026-10-10):
# From bb656c7bf3f008af11d12866b3156f2905246f9f, helper retirement and clone
# classification change. SDK recipe inputs, native CMake topology, third-party
# trees and rust-toolchain.toml remain Git-object identical. Preserve CMAKE,
# CMAKE_GENERATOR and normalized SOURCE_DATE_EPOCH for the existing calibration.
# Two regression fixtures raise the tracked DBT inventory from 939 to 941.
# Keep the existing job clamp and timing budgets; this is not a new timing sample.
# STANDALONE CONSTRUCTOR CARRY TO 20b4d1f81d5b5a78cc6bfe02c20db1038b916a6d (2026-10-10):
# The published standalone constructor follows 6f7c2617. The DBT subtree,
# build.rs, vendor/dynamorio, native/CMakeLists.txt, DBT Cargo.toml, third-party
# and rust-toolchain.toml remain Git-object identical. Root Cargo.toml adds
# the host qualification member; it is not a source_recipe_key input.
# With unchanged CMAKE, CMAKE_GENERATOR and normalized SOURCE_DATE_EPOCH,
# retain SDK key 75c3067d4692cd8aa168d6791e718f21e6535e672b957291142070a0edc27234,
# inventory 941, the 16-job clamp, 1050-job-second wrapper and 572-job-second
# fallback. This carries the SDK budget, not a timing sample, runtime
# identity or earlier validation evidence.
# PIPE-SEMANTICS CARRY TO 15374907e7359d639ce62b7e67179736707af332 (2026-10-10):
# Relative to 20b4d1f81d5b5a78cc6bfe02c20db1038b916a6d, all seven SDK build
# inputs (build.rs, vendor/dynamorio, DBT Cargo.toml/native CMakeLists.txt,
# third-party, root Cargo.toml and rust-toolchain.toml) retain their Git objects.
# Native pipe/RPC behavior and two descriptor facts changed, so the full DBT
# source is not identical and requires fresh runtime checks. With unchanged
# CMAKE/CMAKE_GENERATOR and normalized SOURCE_DATE_EPOCH, carry the existing
# 16-job clamp and 1050/572-job-second SDK budgets only. This is not a new
# timing sample or a claim that earlier native/runtime evidence applies.
# NATIVE DISPATCH CARRY TO 1dae09ef151f88d0d6a0cb7e3c1f06e05f324181 (2026-10-10):
# From 15374907e7359d639ce62b7e67179736707af332, native constructor dispatch
# and test stream capture change. The seven measured SDK build inputs
# (build.rs, vendor/dynamorio, native/CMakeLists.txt, DBT Cargo.toml,
# third-party, root Cargo.toml and rust-toolchain.toml) are Git-object equal.
# Preserve CMAKE, CMAKE_GENERATOR and normalized SOURCE_DATE_EPOCH; the actual
# SDK key is 75c3067d4692cd8aa168d6791e718f21e6535e672b957291142070a0edc27234.
# Preserve inventory 941, the 16-job clamp, 1050-job-second wrapper and
# 572-job-second fallback. This carries the SDK budget only, not runtime
# validation evidence. Main's versioned DBT consumer behavior is retained.
# LOCAL FIXED DETCORE EMISSION PREPARATION (2026-10-11):
# Advance the private source pair from 1dae09ef151f88d0d6a0cb7e3c1f06e05f324181
# to 366dd0a42c9a6f456d7122fba8934a4b52258883. All seven recorded SDK inputs
# are identical by Git object: reverie-dbt build.rs, vendor/dynamorio,
# Cargo.toml and native/CMakeLists.txt; third-party; root Cargo.toml; toolchain.
# Preserve inventory 941, SDK key 75c3067d4692cd8aa168d6791e718f21e6535e672b957291142070a0edc27234,
# the 16-job clamp, 1050-job-second wrapper and 572-job-second fallback.
# This carries the SDK budget only; optimized emission and qualification are pending.
# COPIED-SOURCE QUALIFIER PIN CARRY (2026-10-11):
# Advance from 347e802e861094bbf827c4fc1bb067c18de7d8dd to
# a29aa1c85b08717fc0e2e181333acac42e5261ef. Only the host qualifier and its
# proof document change; all seven recorded SDK input Git objects are equal.
# Preserve SDK key, inventory 941 and the existing budgets. This carries no
# runtime qualification or test evidence.
# MAPPING, DIRECT-EVENT AND NORMAL RETURN-CONTRACT PIN CARRY (2026-10-11):
# Advance the consumer from a29aa1c85b08717fc0e2e181333acac42e5261ef to
# 0f7cdd3232d3216637bc076aaa7517a981d6fdec. Mapping guards, direct event dispatch
# and finite whole-body normal return qualification change. All seven recorded
# native SDK input Git objects are equal. Preserve SDK key, inventory 941,
# the 16-job clamp and 1050/572-job-second budgets. This carries the SDK budget
# only; runtime qualification, test and timing evidence must be measured anew.
# CANONICAL vDSO VERSION TABLES AND VALIDATION PIN CARRY (2026-10-12):
# Advance the consumer from 0f7cdd3232d3216637bc076aaa7517a981d6fdec to
# 19cdd8f59d2c69621f42b789169b0907e68f7144. The canonical vDSO gains the
# kernel's symbol-version tables (reverie 1002), realtime-signal and Tool-region
# work lands, and validate.sh's counted step is fixed (reverie 1006). All seven
# recorded native SDK input Git objects are equal (build.rs ff1eeb32, vendor/
# dynamorio ec2cc0a9, reverie-dbt Cargo.toml 0e24d047, native/CMakeLists.txt
# bcfb298a, third-party fb49c0ba, root Cargo.toml 29399255, toolchain b7ca9302).
# Preserve SDK key, inventory 941, the 16-job clamp and 1050/572-job-second
# budgets. This carries the SDK budget only; runtime qualification, test and
# timing evidence must be measured anew.
# BOUND TO ab8cd5171e502e0332a9b3b0a536c12438670995 (2026-10-12): from
# 19cdd8f59d2c69621f42b789169b0907e68f7144. Of the 14 intervening Reverie commits
# (11 to cdb8ed61), 47f8fe755 ("Route DBT signals through shared Tool
# admission") changes the vendored DynamoRIO tree, so the DynamoRIO SDK recipe
# key changes; the other six recorded inputs are byte-identical:
# reverie-dbt/Cargo.toml: 0e24d047d544a3daae2d6350270b26ceb74139d1
# reverie-dbt/build.rs: ff1eeb32b140ae477894126af21296723c1ba582
# reverie-dbt/native/CMakeLists.txt: bcfb298a4f87ed190d7fdc52393e01d1245a8fe3
# reverie-dbt/vendor/dynamorio: ec2cc0a9bca71c9a6fad3dc39e713f77a8cccf50 -> 3be3524be8355bacf65d4a5296be7cfe30fdd9ce
# third-party: fb49c0ba7a9abd48a4ea662bf20e08246c81fc5a
# Cargo.toml: 293992554d9966678cc161d8bcd65bc26b9faea7
# rust-toolchain.toml: b7ca9302bc65522b829aa2fe3b8783fc77fcb7b9
# 47f8fe755 also raises the native/runtime ABI 6 -> 7, which detcore-dbt
# implements (reverie_dbt_runtime_signal_event), and adds six
# signal_admission_state test files, so the reverie-dbt inventory is 947, not
# 941 (scripts/lib/reverie_dbt_inventory.rs); neither is a recipe input.
# The cmake options and job-count rule are unchanged. The other commits
# change Reverie's realtime-signal and vDSO handling, its validation script
# and reverie-ptrace's injected mask-swapping syscalls
# (https://github.com/rrnewton/reverie/pull/996), none a recipe input.
# ab8cd517 is cdb8ed61 plus the three commits of
# https://github.com/rrnewton/reverie/pull/1010 (reverie-ptrace and the Guest
# trait), none a recipe input: all seven inputs above are byte-identical at
# cdb8ed61 and ab8cd517 (git ls-tree), so the observation below carries.
# BOUNDED COLD SDK OBSERVATION AT cdb8ed61: two cold
# `cargo check -p reverie-dbt --offline` runs (CARGO_BUILD_JOBS=16
# NUM_JOBS=16, each in a fresh target directory) on a 316-CPU host at load
# averages ~103 and ~156 each reported MISS, then "completed in 15.93s
# (jobs=16, 254.87 job-seconds; NUM_JOBS=16, available CPUs=316)" and
# "completed in 19.42s (jobs=16, 310.73 job-seconds ...)", and PUBLISHED for
#     key=sha256:ff4576416a64218bcbccb4528cee86e5fc4da7119267d963751b37c39f68e542
# Both are below 1050. Retain the conservative 1050 effective-job-second
# threshold and the 16-job clamp. These local samples do not replace the
# original n=3 hosted calibration or satisfy the >=5-sample replacement rule;
# fresh validation is required.
# STDIN STREAMING SDK-BUDGET CARRY (2026-10-12):
# Advance from ab8cd517 to a7402b9d25fcf785bf0b5bbeffb3e5b3c35bde43.
# All seven recorded SDK recipe inputs retain their Git objects. The net
# source delta is a DEV qualifier script and the host launcher helper/test.
# Preserve CMAKE, CMAKE_GENERATOR, normalized SOURCE_DATE_EPOCH, inventory 947,
# the 16-job clamp and existing 1050/572-job-second budgets. This carries only
# the SDK budget; no new cold timing sample or runtime acceptance is claimed.
# KVM SDK-BUDGET CARRY (2026-10-13): advance from a7402b9d to e39c780fd2d48424f3f7389330d22689526856fa.
# The added commits change only reverie-kvm/; every SDK recipe input, build.rs
# and the vendored DynamoRIO tree are unchanged, so the budgets above carry.
# DBT IDENTITY SDK-BUDGET CARRY (2026-10-13): advance from e39c780f to 42adcb1d44f38477e14a073b135ceccecae0a7a5.
# The added commit changes only reverie-dbt/native/client.c,
# reverie-dbt/native/virtual_identity.h and a reverie-dbt test fixture, none an
# SDK recipe input: build.rs, the vendored DynamoRIO tree, third-party/, the
# root Cargo.toml and rust-toolchain.toml are unchanged, and no file is added
# (inventory 947), so the budgets above carry. No new timing sample is claimed.
# DBT SETTID AND LITEINST SDK-BUDGET CARRY (2026-10-13): advance from 42adcb1d to 48e78557804772b7e0d2f988504ca9ae2ab6115c.
# The added commits are c58538858a94 (native LiteInst initial image: reverie-inguest,
# reverie-liteinst, reverie-rpc-transport, reverie-ptrace/src/vdso.rs and lib.rs,
# scripts/m1_artifact.rs) and c18356c5b022 (reverie-dbt/native/client.c, the
# thread_start_live test and two new reverie-dbt test fixtures). None is an SDK
# recipe input: build.rs, the vendored DynamoRIO tree, third-party/, the root
# Cargo.toml and rust-toolchain.toml are unchanged. The two fixtures raise the
# reverie-dbt inventory from 947 to 949. No new timing sample is claimed.
# DBT ABI 8 SDK-BUDGET CARRY (2026-10-14): advance from 48e78557 to aad2ccd89d462d4e1bd9194e405d2adab1a728f3.
# The added commits are cb1fd8be05f2 (BACKENDS.md), 57b25878e8ea and
# bb510cd10a75 (native LiteInst kernel registrations and state payloads),
# c710d37a287e (shared preload launcher), 60a68e8bce06 (DBT guest
# compare-and-exchange: reverie-dbt/native/client.c, src/lib.rs, src/tools.rs,
# the thread_start_live test and one new fixture), 62ce9e128be0 (shared vDSO
# rewriting), efa88bd1a6ab (reverie-ptrace guest int3 delivery) and
# aad2ccd89d46 (scripts/m1_artifact.rs allocator TLS route). None is an SDK
# recipe input: build.rs, the vendored DynamoRIO tree, third-party/, the root
# Cargo.toml and rust-toolchain.toml are unchanged. The new fixture raises the
# reverie-dbt inventory from 949 to 950. No new timing sample is claimed.
# KVM CLOCK SDK-BUDGET CARRY (2026-10-14): advance from aad2ccd8 to b866896f688bb27db6e29efd6696451e60ca20be (reverie main 41d4e683 plus one commit).
# The added commits are 41d4e6839387 (scripts/m1_artifact.rs only) and the
# KVM clock commit, which changes only reverie-kvm/src/clock.rs and its tests; every
# SDK recipe input, build.rs and the vendored DynamoRIO tree are unchanged, and
# no reverie-dbt file is added (inventory 950), so the budgets above carry.
# KVM ENTRY-WAKE SDK-BUDGET CARRY (2026-10-14): advance from b866896f to 0b22d5b13b562c65122e6928d2ebbcefd5104f3f.
# The added commits are b87385e1f921 (reverie-ptrace test only) and the KVM
# entry-gate commit (reverie-kvm/src/entry.rs only). Every SDK recipe input,
# build.rs and the vendored DynamoRIO tree are unchanged, and no reverie-dbt
# file is added (inventory 950), so the budgets above carry.
# KVM SYNC-REGS SDK-BUDGET CARRY (2026-10-14): advance from 0b22d5b1 to b0e637a167859017a6869cb035e448ccf190fd62.
# The added commit changes only reverie-kvm (clock.rs, vm.rs and a test); every
# SDK recipe input, build.rs and the vendored DynamoRIO tree are unchanged, and
# no reverie-dbt file is added (inventory 950), so the budgets above carry.
# KVM SIGNAL-MASK SDK-BUDGET CARRY (2026-10-14): advance from b0e637a1 to 1d20e4f25ae32776482eae2083b66cc60c066133.
# The added commits change only reverie-kvm (entry.rs, entry/signal.rs, clock.rs,
# README and tests); every SDK recipe input, build.rs and the vendored DynamoRIO
# tree are unchanged, and no reverie-dbt file is added (inventory 950), so the
# budgets above carry.
# KVM ONE-FRAME-READ SDK-BUDGET CARRY (2026-10-14): advance from 1d20e4f2 to 9d2eaaf69a7dfa39824a8b9ec50b45b51a17a216.
# The added commits change only reverie-kvm (bootstrap.rs); every SDK recipe
# input, build.rs and the vendored DynamoRIO tree are unchanged, and no
# reverie-dbt file is added (inventory 950), so the budgets above carry.
# KVM GATE-FAST-PATH SDK-BUDGET CARRY (2026-10-14): advance from 9d2eaaf6 to fcba7b1969148d21953e3da63faeafb29ac5d52f.
# The added commits change only reverie-kvm (entry.rs); every SDK recipe
# input, build.rs and the vendored DynamoRIO tree are unchanged, and no
# reverie-dbt file is added (inventory 950), so the budgets above carry.
# KVM R3+R4 SDK-BUDGET CARRY (2026-10-14): advance from fcba7b19 to 796c1cb81d1658596c7e41c950cf5c9f46a6986c (R3 and R4
# re-parented by the coordinator onto PR 1029, b36d2030). The added commits change only
# reverie-kvm (entry.rs, executor.rs and the gate type); every SDK recipe input, build.rs and the
# vendored DynamoRIO tree are unchanged, and no reverie-dbt file is added (inventory 950), so the
# budgets above carry.
# SHARED RPC CLIENT SDK-BUDGET CARRY (2026-10-14):
# 796c1cb81d1658596c7e41c950cf5c9f46a6986c -> 049750ad5bfd93f3a315747cc259b5593f756d75.
# Only Rust RPC clients, shared codec code and their tests change. build.rs,
# native/ (including CMake), the DynamoRIO/vendor and third-party trees, root
# and DBT manifests, and toolchain are identical. Inventory rises 950 -> 951.
# Existing SDK budgets carry; no new build timing or runtime artifact is claimed.
# KVM R5 SDK-BUDGET CARRY (2026-10-14): advance from 049750ad to ea20a636c2999320d4d31979f3a6d04577273bae (R5 re-parented by
# the coordinator onto DBT D1). The added commit changes only reverie-kvm (guest-memory
# deferred-failure check); every SDK recipe input, build.rs and the vendored DynamoRIO tree are
# unchanged, and no reverie-dbt file is added, so the budgets above carry.
# KVM GROUP-EXIT-ORDER SDK-BUDGET CARRY (2026-10-14): advance from ea20a636 to 2777e813fcaa35431eb83b10ba57a964f2907c25.
# The added commits change only reverie-kvm (vm.rs, executor.rs and a test); every SDK recipe
# input, build.rs and the vendored DynamoRIO tree are unchanged, and no
# reverie-dbt file is added (inventory 950), so the budgets above carry.
# LITEINST SHARED-HANDLER + REVERIE BUILD-FIX SDK-BUDGET CARRY (2026-10-14): advance from 2777e813 to
# afa7e5e191d8561fd9e4fd0c2231d49d07eb45dc. The added commits change reverie-inguest, reverie-liteinst, scripts/m1_artifact.rs,
# reverie-process (tokio 1.53.3 lint fixes) and one line of reverie-dbt/src/launcher.rs; no SDK recipe
# input, build.rs or vendored DynamoRIO file changes and no reverie-dbt file is added, so the budgets carry.
# KVM R6 + 4a SDK-BUDGET CARRY (2026-10-14): advance from afa7e5e1 to 0c65d74c023f0d1c10413ade2d1029f802293cb9 (R6 and 4a re-parented by
# the coordinator onto the LiteInst shared-handler stack). The added commits change only reverie-kvm
# (file-table install skip, cached host tid); no SDK recipe input, build.rs or vendored DynamoRIO file
# changes and no reverie-dbt file is added, so the budgets above carry.
# KVM SLICE 3 SDK-BUDGET CARRY (2026-10-14): advance from 0c65d74c to 6dd1d62bf71ae171945feed82154a43aedf555bf.
# The added commits change only reverie-kvm (src/entry.rs, src/entry/signal.rs, src/clock.rs, README.md and tests); every SDK recipe
# input, build.rs and the vendored DynamoRIO tree are unchanged, and no
# reverie-dbt file is added (inventory 950), so the budgets above carry.
# DBT D3 SDK-BUDGET CARRY (2026-10-14): advance from 6dd1d62b to e1ecaeb31a0bb1f8b86f36e769296ab6a7de3782 (the DBT lane's inherited-input
# binding, re-parented by the coordinator onto KVM slice 3). It adds reverie/src/inherited_input.rs and a
# reverie-dbt launcher test (inventory 951 -> 952) and edits reverie-dbt/src/launcher.rs; no SDK recipe
# input, build.rs or vendored DynamoRIO file changes, so the budgets above carry.
# KVM RDPMC SDK-BUDGET CARRY (2026-10-14): advance from e1ecaeb3 to 16ffab7c558c47c78c43aa8e0e4242eb2deeeb3d (the KVM lane's rdpmc counter
# reads, re-parented by the coordinator onto DBT D3). The added commit changes only reverie-kvm; no SDK
# recipe input, build.rs or vendored DynamoRIO file changes and no reverie-dbt file is added.
# DBT SHARED-STATS SDK-BUDGET CARRY (2026-10-14): advance from 16ffab7c to dde3f9da33a0474d0fa9b273a25100fc0db4d751.
# Only Rust statistics ownership, launcher exports and capture tests change (inventory 952 -> 953).
# SDK recipe inputs, build.rs, native CMake and vendored DynamoRIO are unchanged, so these budgets
# carry by source equality. This does not reuse or qualify an old native client/runtime artifact.
# Re-parented onto Reverie main ccf6b58a as 9bff68ac24c4e0a41a39bab2c6e6f14f3d0acf40; the added commit changes only safeptrace.
# KVM K-DUP SDK-BUDGET CARRY (2026-10-14): advance from 9bff68ac to 193968de57cdc3aaf2f6eb63c75743503218a657 (KVM deletes its copies of the Detcore-rendered /proc files; changes only reverie-kvm, inventory unchanged), so the budgets above carry.
# KVM IMMEDIATE-EXIT SDK-BUDGET CARRY (2026-10-14): advance from 193968de to d60812b68751ef4b11ba8ae222c38781fda98cde (KVM entries stop with immediate_exit instead of per-entry signal syscalls; changes only reverie-kvm, inventory unchanged), so the budgets above carry.
# KVM CHEAP-COPY SDK-BUDGET CARRY (2026-10-14): advance from d60812b6 to 502bea229f32ce7bdb50d73c7b8d2572bdf2688a (cheaper KVM guest-memory copies and entry checks; changes only reverie-kvm, inventory unchanged), so the budgets above carry.
# ALLOCATOR-CHECKER CARRY (2026-10-14): advance from 502bea22 to 370eb86a01ec9fa316d90699bd64992906efade0 (LiteInst M1 checker admits the measured 0x128 Detcore TLS layout; changes only scripts/m1_artifact.rs, inventory unchanged), so the budgets above carry.
# KVM LAZY-WAKE SDK-BUDGET CARRY (2026-10-14): advance from 370eb86a to dfb68913256514f1550f67fb52271e5aba1e3655 (KVM stops subscribing and registering wakes on every guest syscall; changes only reverie-kvm, inventory unchanged), so the budgets above carry.
# KVM ENTRY-CHECK + SHARED-MOUNT SDK-BUDGET CARRY (2026-10-14): advance from dfb68913 to 9e0ce5768aae1e6c71697ef3dd7ce3d61a91a260 (4b633f7f: cheaper KVM per-syscall entry checks, adds one GlobalTool method skip_empty_signal_boundaries defaulting to false that Detcore does not opt into; 9e0ce576: LiteInst shared-mount setup fix for nested tmpfs targets and pinned bind restrictions in reverie-process; reverie-dbt sources and inventory unchanged), so the budgets above carry.
# KVM READ-QUERY-DEDUP SDK-BUDGET CARRY (2026-10-14): advance from 9e0ce576 to a1fb30867b020e9a8da1fc18296d306e0dbe91a6 (KVM read queries each descriptor's access mode and type once; changes only reverie-kvm, inventory unchanged), so the budgets above carry.
# KVM PUBLISH-CHANGED-FILES SDK-BUDGET CARRY (2026-10-14): advance from a1fb3086 to 69228e48decbb554ddc832086d4fdf3b28bb4fde (KVM republishes only changed descriptors to its shared file table; changes only reverie-kvm, inventory unchanged), so the budgets above carry.
# SIGPIPE RECEIPT SDK OBSERVATION (2026-10-14): pin c18f07cb614ca404b34e131a8a912f4a7b62813c
# changes the vendored returning-injection/signal path; source identity does not carry.
# The maintained build script built the new Release SDK key
# 7035b7a08e771618f7b2cf0692eb4584f10591c566577161548d23d4cda1f9f8
# in 13.02s at 16 jobs (208.31 job-seconds; NUM_JOBS=40, available CPUs=40).
# The rebase onto 69228e48 changes only KVM files, preserving these SDK inputs.
# Keep the existing budgets and 16-job clamp. This single SDK observation
# is not a full-profile calibration or whole-guest qualification.
# NON-LEADER-EXEC ROBUST-OWNER SDK-BUDGET CARRY (2026-10-15): advance from c18f07cb to 4bfe8b8f44bd9992f6ae3f8f4df0fdfb4b334bfd (https://github.com/rrnewton/reverie/pull/1053: reverie-dbt native/client.c robust-owner rewrites and its tests only; vendor/dynamorio, the SDK build inputs, is unchanged, so the observation above carries).
# KVM RPC-KEPT-FAILURE SDK-BUDGET CARRY (2026-10-15): advance from 4bfe8b8f to df6a4e5c30d7782141392e328e6a56bb368982ae (KVM keeps one RPC failure wait per thread instead of one per RPC; changes only reverie-kvm, inventory unchanged), so the budgets above carry.
# KVM SIGPIPE-PRIVATE-PIPE SDK-BUDGET CARRY (2026-10-15): advance from df6a4e5c to 1c514b58e8600d4bb003e5174b3d99e35b20d5dc (KVM skips the SIGPIPE check after a complete write to a guest-private pipe; changes only reverie-kvm, inventory unchanged), so the budgets above carry.
# KVM HYPERCALL-ADDRESS-MEMO SDK-BUDGET CARRY (2026-10-15): advance from 1c514b58 to a4d18df6d14a81ee6553ed4459f4172a92c7733a (KVM memoizes the syscall trampoline's hypercall offset per thread; changes only reverie-kvm, inventory unchanged), so the budgets above carry.
# KVM LOADAVG-COPY-DELETION SDK-BUDGET CARRY (2026-10-15): advance from a4d18df6 to b5ff4dce018027502490fa4d8990bec3b143460c (deletes reverie-kvm's copy of /proc/loadavg, which Detcore renders; changes only reverie-kvm, inventory unchanged), so the budgets above carry.
# KVM LAZY-ZERO + DESCRIPTOR-FACTS SDK-BUDGET CARRY (2026-10-15): advance from b5ff4dce to da4679f359d2397442c59453497cb2bf49bedac2 (542d8457 zeroes large new and unmapped guest ranges lazily; da4679f3 records each descriptor's access mode, file type and read capability once per entry; both change only reverie-kvm, inventory unchanged), so the budgets above carry.
# KVM LAZY-DONTNEED + ZERO-LAZILY-REFUSAL SDK-BUDGET CARRY (2026-10-15): advance from da4679f3 to 8f6e9b816ee502cd5a91b540439bbb674f95644e (reverie-kvm drops MADV_DONTNEED runs of 2 MiB or more lazily, and zero_lazily refuses retained shared views in every build; changes only reverie-kvm, inventory unchanged), so the budgets above carry.
# KVM MAP_FIXED_NOREPLACE SDK-BUDGET CARRY (2026-10-15): advance from 8f6e9b81 to 68e5db200dd63516fc779826069a086dd2b28b4b (KVM honours MAP_FIXED_NOREPLACE; changes only reverie-kvm, inventory unchanged), so the budgets above carry.
# KVM UPTIME-COPY-DELETION SDK-BUDGET CARRY (2026-10-15): advance from 68e5db20 to 361661a980278a363529b63246f173900d431fa6 (7858a5c3, the reverie-ptrace change ending the run on a guest syscall through the untraced private-page stub, changes only reverie-ptrace; 361661a9 deletes reverie-kvm's copies of /proc/uptime, which Detcore renders, and changes only reverie-kvm; inventory unchanged), so the budgets above carry.
# KVM FILE-MMAP-STREAMING SDK-BUDGET CARRY (2026-10-15): advance from 361661a9 to f07e52b10ebb2ad4393a4fa2999bac5440c703ad (reverie-kvm streams a regular file's mmap into guest memory through one 64 KiB buffer, and unmaps the whole range when the mmap fails after its reservation; changes only reverie-kvm, inventory unchanged), so the budgets above carry.
# DBT STATS-GUEST-PATH SDK-BUDGET CARRY (2026-10-15): advance from f07e52b10ebb2ad4393a4fa2999bac5440c703ad to 7b42bb0361cc5288e2319d8ce76cbf7c52bebcf7 (a KVM shared-file test and the additive DBT launcher configure_at utility with capture tests; neither changes native sources, build.rs, vendor SDK inputs or the native build recipe), so the budgets above carry unchanged.
# SHARED NATIVE EXEC SDK-BUDGET CARRY: advance 7b42bb03 to f9be99dc93ba503b1cf89faa8b95e39c8e1b3b91.
# This adds the accepted native exec pair after the DBT stats-path fix. The
# third-party SDK tree, root Cargo.toml and reverie-dbt/build.rs are unchanged;
# inventory remains 955. The existing SDK budget is carried, not remeasured.

# DBT COMPLETED-TOOL-RESULT SDK-BUDGET CARRY (2026-10-15): advance
# f9be99dc93ba503b1cf89faa8b95e39c8e1b3b91 to 14bae9ad147c97b4d29f0cb8d49cdf772eb5bfce.
# The interval contains a LiteInst glibc test correction, one DBT native
# post-Tool override deletion and its regression. build.rs and vendor/dynamorio
# are unchanged; the SDK budget carries. The added live test changes the
# tracked reverie-dbt inventory from 955 to 956. No budget is increased.
# ARCHITECTURE A9/A10 SDK-BUDGET CARRY (2026-10-15): advance
# 14bae9ad147c97b4d29f0cb8d49cdf772eb5bfce to 42d75dd6fdb656a174331244bea298d92e0e7b02.
# The interval deletes the unwired reverie-elf-loader crate, moves the LiteInst
# census from reverie-ptrace into reverie-liteinst, adds reverie-kvm's
# provider-neutral memory traits (U1), one reserved-descriptor rule for
# reverie-inguest, reverie-e9patch and SaBRe (A4 step 1), three
# BackendCapabilities fields, the in-guest typed thread-creation refusal and
# the M1 source-catalog qualification. No reverie-dbt source, build.rs, vendor SDK input
# or native recipe changes; the reverie-dbt inventory stays 956 and the budget
# carries.
# DBT FIXED-VMCODE-BASE SDK-BUDGET CARRY (2026-10-16): advance
# 42d75dd6fdb656a174331244bea298d92e0e7b02 to b88d6af03a3976b17519582ef991969ae0760b48.
# The interval is one commit that changes the reverie-dbt tree: launcher.rs
# alone passes DynamoRIO -vm_max_offset 0x0 at run time (a fixed base for its
# code region; https://github.com/rrnewton/hermit/issues/4478). No build.rs,
# vendor SDK input, native source or recipe changes, so no DynamoRIO build input
# moves and the budget carries; no test is added or removed, so the reverie-dbt
# inventory stays 956.
# M1 RELOCATION-WRITER SDK-BUDGET CARRY (2026-10-16): advance
# b88d6af03a3976b17519582ef991969ae0760b48 to 30a02a848a0e00afc4b6584fffe904208237094f.
# The fixed DBT code-region placement is retained exactly. The additional
# source-catalog identities and fully checked relocation-writer list change
# only the M1 artifact verifier. SDK recipe inputs build.rs ff1eeb32b140,
# vendor/dynamorio 56df1bfed0e7 and third-party tree fb49c0ba7a9a remain
# byte-identical; the tracked DBT inventory stays 956. SDK thresholds, width
# and CMAKE/CMAKE_GENERATOR/SOURCE_DATE_EPOCH selections carry unchanged,
# not remeasured. Fresh exact-pin runtime validation remains required.
# ARCHITECTURE A4/A10 SDK-BUDGET CARRY (2026-10-16): advance
# 30a02a848a0e00afc4b6584fffe904208237094f to ea5f55877a1c3750af04cd80d1a0ccad243231ee.
# Two commits change reverie-dbt: 2d0f66b3 reduces PrototypeTool to a
# forwarding tool (src/lib.rs, +7/-993 lines, and README.md); ea5f5587 makes
# the runtime's descriptors 197/198 not open to the guest and refuses a dup
# onto them (native/client.c, so the DynamoRIO client rebuilds, plus its
# identity-policy test and fixture). The rest of the interval is 89f4124c
# (KVM LinuxProvider), c661348c (A4 step 2a: a reserved descriptor reads
# EBADF on every in-process backend) and 9b299ebb (drops
# supports_guest_memory_compare_exchange). build.rs, vendor/dynamorio and the
# root Cargo.toml are unchanged, so the SDK build inputs and their budget
# carry; the client rebuild is not an SDK rebuild. No file is added or
# removed; the reverie-dbt inventory stays 956.
# A8 STEP 1 CPUID SDK-BUDGET CARRY (2026-10-16): advance
# ea5f55877a1c3750af04cd80d1a0ccad243231ee to d76232d15bd7a99d3947e49fca26f34cc0c324d3.
# 2b55657f refuses KVM flock by name and d8ce0a85 moves the Linux provider
# interface into reverie-linux-provider; neither touches a reverie-dbt file.
# e6ad44e0 moves DBT's CPUID answer into the runtime (native/runtime ABI 9):
# native/client.c loses its CPUID tables and calls reverie_dbt_runtime_cpuid,
# so the DynamoRIO client rebuilds; src/lib.rs, README.md,
# scripts/test-cpuid.sh and tests/cpuid_probe.c change with it. build.rs and
# vendor/dynamorio are unchanged (the root Cargo.toml only gains the
# reverie-linux-provider member), so the SDK build inputs and their budget
# carry; the client rebuild is not an SDK rebuild.
# d76232d1 refreshes the M1 source catalog for the new member and changes
# only scripts/m1_artifact.rs.
# No file is added or removed; the reverie-dbt inventory stays 956.
# SABRE LAUNCH SDK-BUDGET CARRY (2026-10-16): advance
# d76232d15bd7a99d3947e49fca26f34cc0c324d3 to 875a66c95de435f3bc4b56ea9c6020c55efc36f3.
# Three commits, none touching reverie-dbt/ (git diff is empty there):
# 6c60f1c9 gives SaBRe clients their own argv and environment, 7ffc6b97
# preserves signal metadata through in-guest deferral (reverie-liteinst and
# reverie-inguest), and 875a66c9 moves KVM types into the new reverie-kvm-core
# crate (the root Cargo.toml only adds that member). No DynamoRIO build input
# moves, so the budget carries; the reverie-dbt inventory is unchanged.
# A8 STEP 2 CLOCK/RLIMIT SDK-BUDGET CARRY (2026-10-16): advance
# 875a66c95de435f3bc4b56ea9c6020c55efc36f3 to 6109a170bd8833ab304edeb2e26a12aead413117.
# The interval is one commit. 6109a170 deletes the DBT client's own clock and
# resource-limit tables from native/client.c, so the DynamoRIO client
# rebuilds, and updates scripts/test-example-tools.sh and
# tests/fixtures/fork_clock_resource.c with it. build.rs, vendor/dynamorio and
# the root Cargo.toml are unchanged, so the SDK build inputs and their budget
# carry; the client rebuild is not an SDK rebuild. No reverie-dbt file is added
# or removed; the inventory stays 956.
# B3A-4C DBT CONTAINMENT SDK-BUDGET CARRY (2026-10-16): advance
# 6109a170bd8833ab304edeb2e26a12aead413117 to da86dfbe50e6c53845f5f68439e9f32cb6d74f0f.
# Two commits. f84f2f43 changes only reverie-sabre's vendored SaBRe loader
# (backend stats under a launch specification). da86dfbe changes
# reverie-dbt/native/client.c, src/launcher.rs and src/evidence.rs: the
# launcher contains a Hermit run by its PID namespace, so the DynamoRIO client
# rebuilds. build.rs, vendor/dynamorio and the root Cargo.toml are unchanged,
# so the SDK build inputs and their budget carry; the client rebuild is not an
# SDK rebuild. No reverie-dbt file is added or removed; the inventory stays 956.
# IN-GUEST BOOTSTRAP SDK-BUDGET CARRY (2026-10-16): advance
# da86dfbe50e6c53845f5f68439e9f32cb6d74f0f to 8ae79e16069d5db97847f5ee98dfa81652f60511.
# The interval is one commit, 8ae79e16, which sends the in-guest statistics
# coordinator in the sealed bootstrap (reverie-inguest, reverie-liteinst and the
# M1 catalog). git diff is empty for reverie-dbt/, so no DynamoRIO build input
# moves and the budget carries; the reverie-dbt inventory is unchanged.
# KVM CPUID SDK-BUDGET CARRY (2026-10-16): advance
# 8ae79e16069d5db97847f5ee98dfa81652f60511 to 0f4d8852c99b3091f04ca6b5b354ff1aff120027.
# c3082de7 changes only reverie-dbt/src/evidence.rs, a test that now restores
# the subreaper setting and reaps the adopted orphan; no native source changes.
# 0f4d8852 (KVM keeps no CPUID profile of its own) touches only reverie-kvm
# and reverie-core. build.rs, vendor/dynamorio, the native client and the root
# Cargo.toml are unchanged, so the SDK build inputs and their budget carry; no
# reverie-dbt file is added or removed and the inventory stays 956.
# A7 RAW-IDENTITY SDK-BUDGET CARRY (2026-10-16): advance
# 0f4d8852c99b3091f04ca6b5b354ff1aff120027 to 2a62681f03c45f6f402679e3e5a7322eea4209c8.
# The six published A7 commits remove synthetic native identity allocation and
# projection and order collector/input setup before guest execution. The native
# client changes, so a client rebuild is required. The subsequent shared Signal
# refusal variant/KVM use and ptrace signal-boundary test remain included.
# All seven SDK recipe Git inputs are unchanged across this full interval;
# the DynamoRIO SDK budget and every limit therefore carry unchanged.
# Three A7 fixtures increase the tracked DBT inventory from 956 to 959.
# This source carry is not a fresh runtime measurement or a whole-backend claim.
# B3A-4C TYPED PROCESS-GROUP REFUSAL SDK-BUDGET CARRY (2026-10-16): advance
# 2a62681f03c45f6f402679e3e5a7322eea4209c8 to cdf6e9d689e6dcbeafe1b1ed0ea8529a2d455414.
# One commit, cdf6e9d6, touching only reverie/src/unsupported.rs (a new
# UnsupportedOperation variant) and reverie-kvm/src/executor.rs; git diff is
# empty under reverie-dbt/. No DynamoRIO build input moves, so the budget
# carries; the reverie-dbt inventory is unchanged.
# IN-GUEST EXEC REFUSAL AND SITE-PATCHING SDK-BUDGET CARRY (2026-10-16):
# advance cdf6e9d689e6dcbeafe1b1ed0ea8529a2d455414 to caa1deb31e6fddd14d3ed764d2f1759f0feab3d0.
# Four commits: the reverie-ptrace commits 30f7925d and 4c20795a (Tool output
# writes and the Tool stack commit), then 2f10ec67 (in-guest exec refused with
# a typed refusal) and caa1deb3 (the site-patching selection in the sealed
# bootstrap), touching reverie-ptrace, reverie-core, reverie-inguest,
# reverie-liteinst and the M1 catalog. git diff is empty for reverie-dbt/, so
# no DynamoRIO build input moves and the budget carries; the reverie-dbt
# inventory is unchanged.
# VIRTUALIZES_CPUID REMOVAL SDK-BUDGET CARRY (2026-10-16): advance
# caa1deb31e6fddd14d3ed764d2f1759f0feab3d0 to a59dc37f56ddbada4390686cec95c00433d0f7e5.
# 21d91fa7 and 28f86f2d add reverie-examples (a cross-backend recorder) and
# touch no reverie-dbt file. 60138390 (dbt: leave group and session queries to
# Linux) changes reverie-dbt native/client.c and src/tools.rs, so the
# DynamoRIO client rebuilds, and adds tests/fixtures/group_session_getters.c
# and tests/group_session_getters_live.rs: the reverie-dbt inventory goes 959
# -> 961. a59dc37f (drop virtualizes_cpuid) touches only reverie-core.
# build.rs, vendor/dynamorio and the root Cargo.toml are unchanged, so the
# SDK budget carries.
# KVM PROC-STAT-COPY-DELETION SDK-BUDGET CARRY (2026-10-16): advance
# a59dc37f56ddbada4390686cec95c00433d0f7e5 to 47190e9fc8662f38699cccb0444ec524ffbc2b60.
# The one commit deletes reverie-kvm's copy of /proc/stat and changes only
# reverie-kvm (6 files). No reverie-dbt file or recorded SDK input changes;
# the reverie-dbt inventory stays 961 and the budget carries.
# REVERIE-LAYER E2E SDK-BUDGET CARRY (2026-10-16): advance
# 47190e9fc8662f38699cccb0444ec524ffbc2b60 to 75575dc018555cc08531b2b2204045a6fc9cfbbb.
# 98a8a5d2 and b56b90f9 give every reverie-examples tool one --backend runner
# and touch no reverie-dbt file. 75575dc0 (DBT hosts the shared example tools
# by name) changes reverie-dbt src/lib.rs and src/tools.rs and adds no file,
# so the reverie-dbt inventory stays 961. build.rs, vendor/dynamorio, native/
# and the root Cargo.toml are unchanged, so the SDK budget carries.
# REVERIE-LAYER E2E ON ALL BACKENDS SDK-BUDGET CARRY (2026-10-16): advance
# 75575dc018555cc08531b2b2204045a6fc9cfbbb to c4fb4d70aa6d5d61545a4759289f101529329afd.
# 14940a97 adds a test-only import to reverie-inguest/src/launcher/mod.rs (not
# in the LiteInst M1 source catalog, whose 18 entries are byte-identical at both
# pins). 682166d7 changes only reverie-examples/e2e. c4fb4d70 changes
# reverie-examples, experimental/reverie-host, experimental/reverie-sabre-strace
# and Reverie's Cargo.lock. git diff is empty under reverie-dbt/, so the
# reverie-dbt inventory stays 961, and build.rs, vendor/dynamorio and the root
# Cargo.toml are unchanged: the SDK budget carries.
# PROVIDER-INTO-CORE SDK-BUDGET CARRY (2026-10-16): advance
# c4fb4d70aa6d5d61545a4759289f101529329afd to 4d50f85b288f5ab973e51c29e1701c5bb0d380ef.
# The interval is one commit, 4d50f85b, which moves the Linux provider
# interface from reverie-linux-provider into reverie-core as reverie::provider
# and drops reverie-kvm's dependency on the old crate, which stays a workspace
# member. git diff is empty under reverie-dbt/ (tree 981db5f4a9cf at both
# pins), and build.rs, vendor/dynamorio, native/, third-party, the root
# Cargo.toml and rust-toolchain.toml are unchanged, so no DynamoRIO build input
# moves and the budget carries; the reverie-dbt inventory stays 961.
# CARRY TO aed891104de8d8882fa71f24a5c152393596b69e (2026-10-16): from
# 4d50f85b288f5ab973e51c29e1701c5bb0d380ef. The paired scheduler change
# removes only the sched_getscheduler native screen and extends existing live
# tests. All seven SDK inputs and inventory 961 remain unchanged. Match the
# native client and paired runtime to this source; identical tested artifacts
# may be reused. SDK budget, job width and resource limits carry unchanged.
# The preceding owner comments retain both 755..c4fb and c4fb..E1 carries.
# CARRY TO e9f043de2fa512630f749751d8f36a4306ebd0a2 (2026-10-16): from
# aed891104de8d8882fa71f24a5c152393596b69e. Managed copied failures publish the
# existing terminal stats record before root termination. Match native client
# and tests to this source; identical tested artifacts may be reused. All seven
# SDK inputs, inventory 961, calibrated budget, job width and limits are unchanged.
# CARRY TO 1e36924571bda44acd381e35a0d7c6f6f3a4894c (2026-10-16): from
# e9f043de2fa512630f749751d8f36a4306ebd0a2. The interval is one commit, 1e369245, which makes reverie-ptrace's
# timer reach precise targets with PTRACE_SINGLEBLOCK under a decoded bound
# (reverie-ptrace and safeptrace only). git diff is empty under reverie-dbt/
# (tree 17688ad5a2f6 at both pins), and the root Cargo.toml, Cargo.lock and
# rust-toolchain.toml are unchanged. All seven SDK inputs, inventory 961,
# calibrated budget, job width and limits are unchanged.
# CARRY TO 4021030b81acd342affafaab923b096636670df4 (2026-10-16): from
# 1e36924571bda44acd381e35a0d7c6f6f3a4894c. Three commits: 057c2f75 changes two
# reverie-inguest test-support files; b68cc8b9 deletes reverie-kvm's copy of
# /proc/cpuinfo (reverie-kvm/src/executor.rs only); 4021030b is vspeed's PMU
# skid-margin table entry for AMD family 1Ah model 11h, 1,000 -> 2,000 RCBs
# (reverie/src/pmu.rs and reverie-ptrace/src/timer.rs unit expectations).
# git diff is empty under reverie-dbt/ (tree 17688ad5a2f6 at both pins) and
# for the root Cargo.toml, Cargo.lock and rust-toolchain.toml, so all seven
# SDK inputs, inventory 961, the calibrated budget, job width and limits are
# unchanged.
# CARRY TO 51ecc8fb0c7b38ff03ce1596d765a9a0055e7d09 (2026-10-16): from
# 4021030b81acd342affafaab923b096636670df4 through the descriptor pair.
# The runtime waits for prototype bootstrap readiness and reserves the primary
# RPC socket through shared A4 operations. Its source catalog binds the moved
# policy and actual syscalls dependency. All seven SDK inputs, native client
# source and DBT inventory 961 remain unchanged. The calibrated SDK budget,
# job width and limits carry without increase; final runtime/source-closure
# checks must bind this composition rather than older receipts.
# CARRY TO f818f0b4fa105fbe1a5d9aad077a5ea5691bdc4d (2026-10-16): from
# 3137866aa09275237db3715eadeca3c756751c1c through kvm E2a (async LinuxProvider::execute) and liteinst's
# callback-RPC private syscall gate plus native-exec fixture alignment.
# The reverie-dbt tree (61bc6e93), root Cargo.toml, Cargo.lock and
# rust-toolchain.toml are the same objects at both pins, so the seven SDK
# inputs, inventory 961, budget, job width and limits carry unchanged.
# CARRY TO f818f0b4fa105fbe1a5d9aad077a5ea5691bdc4d (2026-10-16): from
# c313920fd73ceb63b52038b595846d608efa31b2 through test-unify's SaBRe-only pair (the plugin's partial
# /proc/self/exe readlink emulation deleted; the loader's mallopt moved
# ahead of read_launch_spec). reverie-dbt tree, root Cargo.toml, Cargo.lock
# and rust-toolchain.toml are unchanged, so the budget carries unchanged.
# CARRY TO 638ee64ae72b819ef12aa90321423e38117c50a1 (2026-10-16): from
# f818f0b4fa105fbe1a5d9aad077a5ea5691bdc4d. The interval is two commits, a70862eb and 638ee64a, which remove
# BackendCapabilities::virtualizes_capability_prctls and add
# promotes_open_files_before_native_lifecycle (reverie/src/capabilities.rs
# only). git diff is empty under reverie-dbt/, and the root Cargo.toml,
# Cargo.lock and rust-toolchain.toml are unchanged. All seven SDK inputs,
# inventory 961, calibrated budget, job width and limits are unchanged.
# CARRY TO bc301d042d8f871326c2b7d6168919aa99f0fa2d (2026-10-17): from
# 638ee64ae72b819ef12aa90321423e38117c50a1. Two commits, 0339779e (provider step E2b:
# execute takes the issuing task and a &mut per-call context, with a guard
# unit test) and 42555ccd (provider step E2c: cancelled reads and committed
# group exits become provider outcomes), touching only reverie-core,
# reverie-linux-provider and reverie-kvm. git diff is empty under reverie-dbt/
# (tree 61bc6e936c77 at both pins), and build.rs, vendor/dynamorio, native/,
# third-party, the root Cargo.toml, Cargo.lock and rust-toolchain.toml have the
# same object ids, so no DynamoRIO build input moves: all seven SDK inputs,
# native client source, DBT inventory 961, calibrated budget, job width and
# limits are unchanged.
# CARRY TO bc301d042d8f871326c2b7d6168919aa99f0fa2d (2026-10-17): from
# 42555ccdb136fe0984cee211a3932139ea32eede through signals' realtime UnreportedSignal (reverie-ptrace,
# reverie core) and isolation's Reverie-hosted e2e. reverie-dbt changes only
# src/lib.rs (two imports gated on prototype-runtime); no file is added
# (inventory 961), and build.rs, vendor/, native/, the root Cargo.toml,
# Cargo.lock and rust-toolchain.toml are unchanged, so the budget carries.
# CARRY TO 9a6c76c6e784b9e38e47485b3df0d32b8ea5c844 (2026-10-17): from
# bc301d042d8f871326c2b7d6168919aa99f0fa2d, replay only native getpriority target admission
# removal and its original/tail/returning regression cases. All seven SDK
# recipe inputs remain object-identical; native client source equals the
# reviewed73cc repair and is source-bound separately. Preserve incoming
# realtime-signal/e2e carries, SDK budget, parallel width and limits.
# The paired Hermit strict copied-child refusal remains required.
# CARRY TO 01e05bef246244466710d07d6e85442f64303f48 (2026-10-17): from
# 9a6c76c6e784b9e38e47485b3df0d32b8ea5c844, remove only native sched_getparam
# membership admission and extend existing original/tail/returning witnesses.
# All seven SDK recipe inputs are object-identical; native client.c changes
# outside the measured SDK region and requires fresh runtime validation.
# Preserve existing carries, budget values, parallel width and limits.
# The paired strict copied-child named refusal remains required.
# CARRY TO 586d929ed48607c751606552593241eabeca047e (2026-10-17): from
# 01e05bef246244466710d07d6e85442f64303f48. One commit, 586d929e (B3 step 2: LITEINST_IN_GUEST
# reports promotes_open_files_before_native_lifecycle, and the field's
# documentation names both ways a backend satisfies it), touching only
# reverie/src/capabilities.rs. git diff is empty under reverie-dbt/ (tree
# bb913edb at both pins), and the root Cargo.toml, Cargo.lock and
# rust-toolchain.toml are unchanged: all seven SDK inputs, DBT inventory 961,
# calibrated budget, job width and limits are unchanged.
# CARRY TO 3da25ef38b2b2ba1784fdbc6aface1610dc0c74a (2026-10-17): from
# 586d929ed48607c751606552593241eabeca047e. Shared callback memory copies,
# a test-only source transaction helper, and ptrace interrupt-latency headroom.
# The full reverie-dbt tree and all seven SDK recipe inputs are unchanged;
# inventory remains 961. Cargo.lock adds only the inguest-to-existing-syscalls
# edge. Preserve the new base's calibration, widths, bounds and prior carries.
# Fresh locked Rust consumers, fixtures and ptrace checks remain required.
# CARRY TO bb86c5d9801493eddd4089b8b34091b40603ef87 (2026-10-17): from
# 3da25ef38b2b2ba1784fdbc6aface1610dc0c74a. One commit, bb86c5d9 (provider step E2d:
# a successful call that stages a process action reports it as a provider
# outcome carrying a token; the action stays in the executor), touching only
# reverie-core, reverie-linux-provider and reverie-kvm. git diff is empty under
# reverie-dbt/ (tree bb913edb at both pins), and build.rs, vendor/dynamorio,
# native/, third-party, the root Cargo.toml, Cargo.lock and rust-toolchain.toml
# have the same object ids: all seven SDK inputs, DBT inventory 961, calibrated
# budget, job width and limits are unchanged.
# CARRY TO 5aa3284d0202a9c69e2db20dfcf3ac314b3014e1 (2026-10-17): from
# bb86c5d9801493eddd4089b8b34091b40603ef87 through liteinst's Tool-owned syscall gate (60bbf180) and A5 step
# 2's sigreturn report (Tool::observes_sigreturns, Tool::handle_sigreturn,
# BackendCapabilities::reports_sigreturns). reverie-dbt tree, root Cargo.toml,
# Cargo.lock and rust-toolchain.toml are unchanged, so the budget carries
# unchanged.
# CARRY TO d1f198a6c5fa466a259a22d49ed80c1d40ba4960 (2026-10-17): from
# 5aa3284d0202a9c69e2db20dfcf3ac314b3014e1 across the published2476 KVM A2 train. Remove only native sched_getaffinity
# membership admission and extend the existing standalone getter witnesses.
# All seven SDK recipe inputs and root Cargo.lock are unchanged; inventory961,
# calibrated job width, CPU/wall limits and prior carry intervals remain.
# Native client.c intentionally changes; fresh native and shared Tool evidence
# remains separate from this SDK build-budget carry.
# CARRY TO 25f8b656d468bf894147079b476b88cc92a65e68 (2026-10-17): from
# d1f198a6c5fa466a259a22d49ed80c1d40ba4960. Six commits: be8af50f (reverie-sabre close
# of stderr), cf83e82a (the ReservedSignal refusal variant), dc977455 (reverie-sabre
# uses the shared LocalMemory), 528bf5d5 (provider step E2d-1), 774e9fd9 (tests
# pinning the handler segment-base rule) and 25f8b656 (provider step E2e), touching
# only experimental/reverie-sabre, reverie/src, reverie-memory/src and
# reverie-kvm/src. git diff is empty under reverie-dbt/
# (tree c01780c7 at both pins), and build.rs, vendor/dynamorio, native/,
# third-party, the root Cargo.toml, Cargo.lock and rust-toolchain.toml have the
# same object ids: all seven SDK inputs, DBT inventory 961, calibrated budget,
# job width and limits are unchanged.
# CARRY TO e53cc6f4a20318730eacfb10804acf5fee80e077 (2026-10-17): from
# 25f8b656d468bf894147079b476b88cc92a65e68 for default-off SaBRe post-arm loader routing,
# the shared installed Tool entry, and the bounded native artifact qualifier.
# The complete reverie-dbt tree and root Cargo.toml, Cargo.lock and
# rust-toolchain.toml are byte-identical to 25f8b656. Inventory 961,
# calibrated job width and CPU/wall limits carry unchanged. Fresh runtime
# and native artifact qualification remain separate from this budget carry.
# CARRY TO 7230ef91248b2a42ad212c5b991cea75cfa29c26 (2026-10-17): from
# e53cc6f4a20318730eacfb10804acf5fee80e077. Four commits: fd45d650 and 0b583715 (reverie-sabre:
# a rewritten site's frame adapts to the shared saved-register entry; a
# prefixed SYSCALL's marker goes on its final two bytes; experimental/
# reverie-sabre only), 8b6991ef (provider step E3a-0: E2e review follow-ups,
# reverie-kvm tests and comments) and 7230ef91 (provider step E3a: ptrace's
# injections run through LinuxProvider; reverie-ptrace only). git diff is empty
# under reverie-dbt/ (tree c01780c7 at both pins), and build.rs,
# vendor/dynamorio, native/, third-party, the root Cargo.toml, Cargo.lock and
# rust-toolchain.toml have the same object ids: all seven SDK inputs, DBT
# inventory 961, calibrated budget, job width and limits are unchanged.
# CARRY TO 5a38b0f755652b59179a6e8d5ac9e590045d1301 (2026-10-17): from
# 7230ef91248b2a42ad212c5b991cea75cfa29c26. Seven commits: 329c7f66 (reverie-ptrace test
# reaping) and 5a38b0f7 (reverie-inguest records refused signal controls), and five commits: the sabre 5c prep chain
# (06cdc058, 3ea100d7, 42694c54, 4ef07bba; experimental/reverie-sabre loader only)
# and 9c6069d7 (Guest::initial_stack_pointer reports the image's initial stack
# pointer to a Tool; reverie, reverie-ptrace and reverie-kvm only; replayed from
# 60c43813). git diff is empty under reverie-dbt/ (tree c01780c7 at
# both pins), and build.rs, vendor/dynamorio, native/, third-party, the root
# Cargo.toml, Cargo.lock and rust-toolchain.toml have the same object ids: all
# seven SDK inputs, DBT inventory 961, calibrated budget, job width and limits
# are unchanged.
# CARRY TO 5396f3c2f4db48a53329bb56b5b52b16fa5d449e (2026-10-18): from
# 5a38b0f755652b59179a6e8d5ac9e590045d1301. Two commits: 3f648f4d (a doc comment on reverie-kvm's
# nice-inheritance test) and 5396f3c2 (provider step E3b: ProviderOutcome
# gains Interrupted; reverie-ptrace reports it and decides restartable
# injection from it; reverie-kvm treats it as an invariant error; replayed
# from 284cbecd), touching only reverie-core, reverie-linux-provider,
# reverie-kvm and reverie-ptrace.
# git diff is empty under reverie-dbt/ (tree c01780c7 at both pins), and
# build.rs, vendor/dynamorio, native/, third-party, the root Cargo.toml,
# Cargo.lock and rust-toolchain.toml have the same object ids: all seven SDK
# inputs, DBT inventory 961, calibrated budget, job width and limits are
# unchanged.
# CARRY TO ce6433f83adfc3e666d1ebed175b869edaea3574 (2026-10-18): from
# 5396f3c2f4db48a53329bb56b5b52b16fa5d449e. One commit, ce6433f8 (A5 step 2c: a reported
# rt_sigreturn requests a timer event it would cancel again; replayed from
# 9074ce43 with the same patch-id), touching only reverie-ptrace (timer.rs,
# task.rs, plain_guest_regression_tests.rs and its C fixture).
# git diff is empty under reverie-dbt/ (tree c01780c7 at both pins), and
# build.rs, vendor/dynamorio, native/, third-party, the root Cargo.toml,
# Cargo.lock and rust-toolchain.toml have the same object ids: all seven SDK
# inputs, DBT inventory 961, calibrated budget, job width and limits are
# unchanged.
# CARRY TO 0afd4fe6e9292853e253a9b0d1accf79ea6c25d2 (2026-10-18): from
# ce6433f83adfc3e666d1ebed175b869edaea3574. One commit, 0afd4fe6 (getter-only
# native get_robust_list admission forwards unrecorded targets to Linux;
# replayed from d54422db with the same patch-id), touching only reverie-dbt:
# native/client.c, native/task_membership.h, src/tools.rs and the
# group_session_getters/task_membership tests and fixtures. No file is added or
# removed (DBT inventory 961). reverie-dbt/vendor, third-party, build.rs, the root
# Cargo.toml, Cargo.lock and rust-toolchain.toml have the same object ids: all
# seven SDK inputs, calibrated budget, job width and limits are unchanged.
# CARRY TO 48e0d7f1fa9a89aa70f379c5ff6f6d24ee9dda6f (2026-10-18): from
# 0afd4fe6e9292853e253a9b0d1accf79ea6c25d2. Two commits: 38c87aed
# (DbtRunner::tool_answers_brk; replayed from 57406bba with the same patch-id)
# and 48e0d7f1 (Auxv::at_execfn, reverie/src only; replayed from cf46ad3e with
# the same patch-id). 38c87aed changes two vendored DynamoRIO sources,
# reverie-dbt/vendor/dynamorio/core/lib/globals_shared.h (+4 lines) and
# core/unix/loader.c (+12 lines), changes src/launcher.rs, and adds two files,
# tests/fixtures/tool_answers_brk.c and tests/tool_answers_brk_live.rs: DBT
# inventory 961 -> 963 (REVERIE_DBT_FILES in
# scripts/lib/reverie_dbt_inventory.rs). build.rs, native/, third-party, the
# root Cargo.toml, Cargo.lock and rust-toolchain.toml have the same object ids.
# Cold DynamoRIO builds, back to back, 16 jobs, load about 300 of 316 CPUs
# (host in docs/TESTING_ENVIRONMENTS.md, "Named measurement hosts"): unpatched
# (Reverie e53cc6f4, whose vendor/dynamorio is 0afd4fe6's) 13.06s (208.95
# job-seconds), patched (7e0cabd7, whose DynamoRIO sources differ from
# 48e0d7f1's only in a loader.c comment) 13.32s (213.16 job-seconds), against
# the 572 job-second ceiling. Calibrated job width and CPU/wall limits carry
# unchanged.
# CARRY TO 8214c7ee0d87b7a68eff6dbc1a02782035f42d54 (2026-10-18): from
# 48e0d7f1fa9a89aa70f379c5ff6f6d24ee9dda6f. Five commits. 4ebe6b40, 10774879,
# 6607d5ab and 368dd498 add Guest::set_blocked_signals (reverie/src/guest.rs,
# reverie-ptrace task.rs and its tests) and touch nothing under reverie-dbt/.
# 8214c7ee (DbtGuest::auxv reads the application's vector from its initial
# stack; replayed from 1ca2c374 with the same patch-id) touches only
# reverie-dbt/src/lib.rs. No file is added or removed (DBT
# inventory 963). reverie-dbt/vendor, native/, build.rs, third-party, the root
# Cargo.toml, Cargo.lock and rust-toolchain.toml have the same object ids: all
# seven SDK inputs, calibrated budget, job width and limits are unchanged.
# CARRY TO ed29a09d102ffa3bf33c3efce9401025e80b7e4e (2026-10-18): from
# 8214c7ee0d87b7a68eff6dbc1a02782035f42d54. One commit, ed29a09d (the LiteInst
# artifact qualifier moves inside its package for independent Reindeer
# vendoring; replayed from 73242102 with the same patch-id). No runtime code
# changes. git diff is empty under reverie-dbt/, and the root Cargo.toml,
# Cargo.lock and rust-toolchain.toml are unchanged. All seven SDK inputs,
# inventory 963, calibrated budget, job width and limits are unchanged.
# CARRY TO b1e81ac276ccebe31d1a41e4c1f648bbcbd5e16f (2026-10-18): from
# ed29a09d102ffa3bf33c3efce9401025e80b7e4e. Four commits, in-guest layering step 2
# (10cd4cd4, b240b619, bcf633d1, b1e81ac2; replayed from 585d1eb7, ceee5a6d,
# b7416bc5 and ed456a49 with the same patch-ids): reverie-inguest gains the
# Patcher seam, the shared Tool-mode syscall path and the shared typed
# installer, and LiteInst plugs in as a PatchHost. They touch only
# reverie-inguest/, reverie-liteinst/ and reverie-liteinst-artifact/src/lib.rs
# (its declared-content catalog). git diff is empty under reverie-dbt/, no
# reverie-dbt file is added or removed, and the root Cargo.toml, Cargo.lock and
# rust-toolchain.toml are unchanged. All seven SDK inputs, inventory 963,
# calibrated budget, job width and limits are unchanged.
# CARRY TO 6d76bb72178c34b3ded8ec2d12b26aca8f51eee0 (2026-10-18): from
# ed29a09d102ffa3bf33c3efce9401025e80b7e4e. Controller-capture certainty and inline RPC
# storage add one DBT test file (inventory 964), native callback ABI 10, and
# a small vendor export. Build recipes and calibrated limits stay unchanged;
# vendor source identity changes, so no unchanged-SDK carry is asserted.
# Actual R6d76 SDK MISS 8da5596da1403ff0b682f35f088ce01dda82e3d1bd64a4395c94d4d8bb6df94e
# built in 25.36 seconds with 7 native jobs (NUM_JOBS=8). The scoped R build
# peaked at 1,715,703,808 bytes and 163.077281 CPU seconds, with all caps clear.
# This is SDK-input evidence; successor native-client compilation is separate.
# CARRY TO 5ac38380bc85e49f5212a4d4518179595b016ce8 (2026-10-18): from
# 6d76bb72178c34b3ded8ec2d12b26aca8f51eee0. Correct native reservation reuse and atomic
# admission publication; the client and its existing component driver change.
# Vendor/SDK recipe inputs are byte-identical, so key 8da5596d above still
# names those inputs. Rebuild the native client separately; limits stay unchanged.
# CARRY TO 9451327184143f9046c80415f65eafbc0b25bd43 (2026-10-18): rebase the two
# controller-capture commits onto b1e81ac276ccebe31d1a41e4c1f648bbcbd5e16f.
# Incoming changes are disjoint from DBT/native/vendor/build inputs; the
# SDK and native component inputs equal 5ac above. All limits stay unchanged.
# CARRY TO 80fc77faa79da855612517666bd87134a915e2fc (2026-10-18): from
# 9451327184143f9046c80415f65eafbc0b25bd43. One commit, 80fc77fa (the SaBRe loader
# keeps address space free above a PIE client's image for its program break;
# replayed from 19f82dca with the same patch-id), touching only
# experimental/reverie-sabre (vendor/sabre/loader/elf_loading.c and a new test
# pair). git diff is empty under reverie-dbt/, so inventory 964, the SDK and
# native component inputs, calibrated budget, job width and limits are unchanged.
# CARRY TO aaed1ab669b079a7f0a3adf00cf92d274e34649b (2026-10-18): Reverie's revert of
# 6a5491c0 (AT_RANDOM startup seed). Its tree is exactly
# 80fc77faa79da855612517666bd87134a915e2fc's, so everything the 80fc77fa block above
# states holds unchanged (inventory 964, SDK and native inputs, limits).
# HISTORICAL MAPS BRANCH EVIDENCE (separate from the landed capture chain):
# Preserved maps head 8bb1b7d26ad8defab878d0dd3f4ee431888a936c on ed29a09d.
# PRIVATE MAPS f85bbe2aca52191d835e80f8600c9dfec880f81f: Guest ownership facts,
# ABI 10 and native reservation retirement change vendor/SDK inputs. No old
# SDK source-equivalence carry applies. The fresh SDK MISS at jobs=8 used key
# 01e4833a30c4dcfaa615fb8c402e91fb1b0836601781376713641ab7605e4adf
# and completed in 24.45 seconds. The scoped Reverie build/contract checks
# completed in 42.901049 seconds, 156.484799 CPU seconds, peak 1,583,882,240
# bytes and 86 tasks. The H ff17cc04 validate-profile consumer build completed
# in 224.095764 seconds, 654.418854 CPU seconds, peak 3,532,775,424 bytes and
# 345 tasks. These are capped local measurements with existing dependency
# caches, not a new cold-build calibration. Inventory is 965; the existing
# parallel-job, memory, CPU and elapsed limits remain unchanged.
# The restack preserves the changed SDK inputs byte-for-byte while adding
# landed Guest mask and DBT auxv support; full-runtime equality is not claimed.
# COMBINED MAPS/CAPTURE 3090b098a5efbe124815ca6babec849511b52558: ABI 11 retains
# controller capture and appends the maps provider. Native publication and
# vendor inputs differ from both historical branches. Fresh SDK key
# f33cd1a46e8c962dc6af2a0cced1024ac65500a8ac4934962551a6535ab6e5b3
# was built in the debug and validate caches, respectively 33.71 and 30.58
# seconds at 7 native jobs (NUM_JOBS=8). The H 776470bd validate consumer
# build took 356.790859 seconds, 1217.714040 CPU seconds, peak 4,224,962,560
# bytes and 337 tasks. Current native compile/link rows use Release/O3.
# These are capped local builds with existing dependencies, not a cold-build
# recalibration or equality to either historical SDK key. Inventory is 966;
# all existing job, memory, CPU and elapsed limits remain unchanged.
# CARRY TO c4c5b7374d4fe1aa3e04066744cb623e48b6695d (2026-10-18): the block above's
# 3090b098 (private memory facts beside controller capture), replayed onto
# aaed1ab669b079a7f0a3adf00cf92d274e34649b, whose tree equals 80fc77fa; the replayed
# tree is byte-identical to 3090b098's, so the block above holds unchanged.
# CARRY TO 38a30d7a4912d046487d4a517ffad092cfb5d4c0 (2026-10-18): from
# c4c5b7374d4fe1aa3e04066744cb623e48b6695d. Two commits: 8be792c1 (SaBRe's plugin ABI for
# the shared in-guest runtime, R1; experimental/reverie-sabre*) and 38a30d7a
# (Guest::deliver_signal and the signal-delivery check types, A5 step 3.1;
# reverie/src and reverie-ptrace). git diff is empty under reverie-dbt/, no
# reverie-dbt file is added or removed (inventory 966), and the root Cargo.toml,
# Cargo.lock and rust-toolchain.toml are unchanged. SDK and native inputs,
# calibrated budget, job width and limits are unchanged.
# CARRY TO 3d4461d7af7a2895130b020c36411fc3fc81a287 (2026-10-18):
# The JECXZ correction changes one vendored counter-branch predicate plus two
# regression files; the separate native exit-classification repair leaves SDK
# inputs unchanged. The a22f4adf restack has no DBT/vendor/SDK input delta.
# Fresh maintained SDK miss 37d51116a9327239d5599d8fb9f2efb538cbc42837c69598e51a4699db24b0b2
# measured 27.05s with 7 available jobs (NUM_JOBS=8), 189.38 job-seconds, below
# the default native ratchet572. The portable1050 job-second budget and all
# selectors/limits remain unchanged; this is one source-equivalent carry sample.
# Actual producer: R38-based JECXZ source43df98d4, jecxz-cargo-r2 under
# dbt-continuous-clock-main09ec59c4-20261018; restacked SDK inputs byte-identical.
# Tracked DBT inventory is 968 after the two new JECXZ regression files.
# CARRY TO c58a4137af163e3ebb35b41b3c48e478daeb0fcf (2026-10-18): from
# 3d4461d7af7a2895130b020c36411fc3fc81a287. Two commits: c57b7a90 (reverie-ptrace keeps the
# guest's SIGTRAP action and mask across Reverie's single-steps for a Tool that
# declares serialized execution; reverie/src/tool.rs gains the default-false
# enforces_serialized_execution) and c58a4137 (reverie-sabre gives the tool each
# signal's siginfo and honours its answer; BackendCapabilities gains
# reports_kernel_child_exit_signals). git diff is empty under reverie-dbt/ and for
# the root Cargo.toml, Cargo.lock and rust-toolchain.toml, so the inventory (968),
# SDK and native inputs, calibrated budget, job width and limits are unchanged.
# CARRY TO 8ebc1ae72bfeaa4ae127ece2d108156092f884fd (2026-10-18): from
# c58a4137af163e3ebb35b41b3c48e478daeb0fcf. One commit, 8ebc1ae7 (the in-guest
# backend's InGuest injections run through reverie::provider::LinuxProvider;
# replayed from 8b5cbb55, whose added and removed lines it keeps exactly). It
# changes reverie-inguest/src/guest/host.rs and native_exec_sources.rs, adds
# reverie-inguest/src/guest/host_linux.rs, updates the LiteInst qualifier
# catalog in reverie-liteinst-artifact/src/lib.rs, and edits documentation in
# reverie/src/lib.rs and provider.rs. git diff is empty under reverie-dbt/ (DBT
# inventory 968), and the root Cargo.toml, Cargo.lock and rust-toolchain.toml
# are unchanged. All seven SDK inputs, calibrated budget, job width and limits
# are unchanged.
# CARRY TO 4561503488d69d6d4350d5db090279c9cf8bb3e4 (2026-10-18): from
# 8ebc1ae72bfeaa4ae127ece2d108156092f884fd. One commit, a replay of the dbt lane's 51fed06f onto
# E4a's 8ebc1ae7 (E4a itself is outside reverie-dbt). The terminal-app-UBR
# translation repair changes vendored interp.c and adds five regression files
# (tracked DBT inventory 968 -> 973). It preserves true successor translations
# and original source extent; no clock policy changes. Fresh maintained SDK MISS
# 49724b2667ab56f2408fb932c1d7dc06aa45677336fda523df9b47cd804c1c6a measured 26.87s with 7 available jobs
# (NUM_JOBS=8), 188.10 job-seconds, below the default native ratchet572. The
# portable1050 effective-job-second limit and all job/selection limits remain
# unchanged, not recalibrated. One qualified sample from the dbt lane's
# terminal-translation-candidate-r1, not a five-sample calibration campaign.
# CARRY TO 6bdd0533a38f68a5742b361df656e32b2237d78a (2026-10-18): from
# 188cb62ab812068cd71ef80f2c43e5854572f70e. 6bdd0533 reverts 188cb62a (ptrace CPUID arming), so
# its tree equals 4561503488d69d6d4350d5db090279c9cf8bb3e4's exactly. git diff is empty under reverie-dbt/ and
# for the root Cargo.toml, Cargo.lock and rust-toolchain.toml in both steps, so
# the inventory (973), SDK and native inputs, calibrated budget, job width and
# limits are unchanged from the 45615034 carry.
# CARRY TO 50b0e681d2215a73c113e6c2200f589fa66db29f (2026-10-18): from
# 6bdd0533a38f68a5742b361df656e32b2237d78a. One commit (signals lane, SIGTRAP-keep fixes). It
# changes reverie-ptrace only (src/task.rs, its regression tests and their C
# fixture); git diff is empty under reverie-dbt/ and for the root Cargo.toml,
# Cargo.lock and rust-toolchain.toml, so the inventory (973), SDK and native
# inputs, calibrated budget, job width and limits are unchanged from the
# 45615034 carry.
# CARRY TO db0d0d92037af0a359589a301d41768eb26aa490 (2026-10-18): from
# 50b0e681d2215a73c113e6c2200f589fa66db29f. One commit (signals lane, the SIGTRAP keep without
# procfs on its hot paths). It changes reverie-ptrace/src/task.rs and
# safeptrace/src/lib.rs (a new peeksiginfo_any) only; git
# diff is empty under reverie-dbt/ and for the root Cargo.toml, Cargo.lock and
# rust-toolchain.toml, so the inventory (973), SDK and native inputs,
# calibrated budget, job width and limits are unchanged from the 45615034
# carry.
# CARRY TO 956522100b5f6dbd3bb5a105087aa450db8648aa (2026-10-19): from
# db0d0d92037af0a359589a301d41768eb26aa490. One commit, the liteinst lane's verified ptrace CPUID
# arming (its reviewed 1418122e) replayed onto the signals lane's SIGTRAP keep.
# It changes reverie-ptrace (src/task.rs, Cargo.toml) and adds tests; git diff
# is empty under reverie-dbt/ and for the root Cargo.toml, Cargo.lock and
# rust-toolchain.toml, so the inventory (973), SDK and native inputs,
# calibrated budget, job width and limits are unchanged from the 45615034
# carry.
# CARRY TO 9ded00ff4ab82e48368e0113f65e954d3533bc3f (2026-10-19): from
# 956522100b5f6dbd3bb5a105087aa450db8648aa. One commit, the liteinst lane's reviewed f962560b
# replayed unchanged. Only inguest/loader ownership and fixtures move.
# reverie-dbt/, root Cargo.toml and rust-toolchain.toml are byte-identical.
# Cargo.lock changes only reverie-inguest and reverie-loader dependency edges;
# neither is in the DBT dependency closure, whose locked packages are identical.
# Native SDK recipe/source inputs, inventory 973, budgets and job limits carry
# unchanged. This is not a claim that the whole workspace lock is unchanged.
# CARRY TO 63934937463458de93d43ed08038d256ae799c64 (2026-10-19): from
# 9ded00ff4ab82e48368e0113f65e954d3533bc3f. One commit, 63934937 (reverie-kvm deletes the
# backend's copy of the host mount table, proc_mounts.rs, now that Detcore
# renders every process's mount files from the container mount table; package
# K slice K3b). It changes only reverie-kvm/src/{elf,executor,lib}.rs,
# deletes reverie-kvm/src/proc_mounts.rs and edits reverie-kvm/tests/static_elf.rs.
# git diff is empty under reverie-dbt/ (DBT inventory 973), and the root
# Cargo.toml, Cargo.lock and rust-toolchain.toml are unchanged. All SDK
# inputs, calibrated budget, job width and limits are unchanged.
# CARRY TO 2df22d5eb00ebfcf5fdecee67bbebbb1239d7081 (2026-10-18): from
# 6bdd0533a38f68a5742b361df656e32b2237d78a. Default-off internal original-conditional facts
# append ABI12/80, checked task births, and native cache-abandonment/guarded-return
# support. Fourteen maintained source/test files are added (inventory973 -> 987).
# Vendor/native inputs changed; neither the old SDK37 nor SDK497 is reused as
# the candidate recipe. Fresh SDK MISS
# a15e54f33b6693b6796540d3179389e897c30d7ee370a6bbf158c93fc5e6bc93 measured36.65s with7
# available jobs (NUM_JOBS=8),256.52 job-seconds, below default native ratchet572.
# This is one qualified source07 sample, not a recalibration campaign. The
# existing portable1050 effective-job-second budget, jobs, limits and selections
# stay unchanged. Collection is default off; shared clock/timer policy is unchanged.
# CARRY TO 3a17e3d04777f8cfee445ddac285ae0815f0f9d9 (2026-10-19): replay the exact
# 2df22d5e owned change onto published db0d0d92037af0a359589a301d41768eb26aa490.
# The full reverie-dbt tree equals 2df22d5e (inventory987), including SDK/native
# inputs. The a15e sample above remains the original measured sample, not a new
# build measurement. Incoming ptrace/safeptrace changes do not alter that recipe.
# CARRY TO 64f05984c8da7f9de8c7433e18a8214d69277d9a (2026-10-19): replay the same owned fact
# change onto published 9ded00ff4ab82e48368e0113f65e954d3533bc3f. The DBT
# tree and SDK/native inputs equal 3a17e3d (inventory987). The incoming
# inguest/loader lock changes do not alter the locked DBT dependency closure.
# The a15e timing above remains its original sample, not a fresh measurement.
# CARRY TO 99def63a0dd7be7e994857709d4ecb403eb100d6 (2026-10-19): replay the same owned change
# from 64f05984c8da7f9de8c7433e18a8214d69277d9a onto the K3b stack rooted at
# published 63934937463458de93d43ed08038d256ae799c64. The owned DBT
# tree and SDK/native recipe inputs are identical (inventory987).
# Preserve the incoming K3b carry and the original a15e measurement above;
# this source equality is not a new build or runtime-package receipt.
# CARRY TO bfe01a465bd53a00eff185f3130e37344938982e (2026-10-18): from
# 2df22d5eb00ebfcf5fdecee67bbebbb1239d7081. Bind each task from its already
# admitted birth so an unrelated sibling reservation cannot leave PendingOwner.
# Two native regression-support paths are added (inventory987 -> 989).
# Vendor/build.rs and SDK recipe inputs are unchanged; the qualified a15e SDK
# sample above carries under the same CMake/generator/epoch settings. The native
# client changes and requires a new build receipt. ABI12/80, portable1050 budget,
# default ratchet572, job width, limits, selectors and clock policy stay unchanged.
# CARRY TO a8c3ba366f72155ab37d423f4241d40ffda729ff (2026-10-19): exact replay of
# bfe01a465bd53a00eff185f3130e37344938982e, following 3a17e3d04777f8cfee445ddac285ae0815f0f9d9.
# Full reverie-dbt source equality preserves inventory989 and the a15e SDK
# recipe. This carries the original measured sample; it is not a new runtime
# or build result. Native/client changes remain covered by their own receipts.
# CARRY TO 69efc09ebcc7bd3ea33bd033e9688500dc205191 (2026-10-19): replay the same owned
# change in order after 64f05984c8da7f9de8c7433e18a8214d69277d9a.
# Full DBT source equals a8c3ba366f72155ab37d423f4241d40ffda729ff (inventory989).
# SDK/native recipe inputs carry; the a15e sample keeps its original attribution.
# Incoming workspace dependency changes require fresh paired build/runtime evidence.
# CARRY TO 97c6424a743f423e57d9dfaae15662e1cbcf5a01 (2026-10-19): replay the same owned change
# from 69efc09ebcc7bd3ea33bd033e9688500dc205191 onto the K3b stack rooted at
# published 63934937463458de93d43ed08038d256ae799c64. The owned DBT
# tree and SDK/native recipe inputs are identical (inventory989).
# Preserve the incoming K3b carry and the original a15e measurement above;
# this source equality is not a new build or runtime-package receipt.
# CARRY TO 9e11cab435dde363c9f065d172da1754b8c25909 (2026-10-18): from
# bfe01a465bd53a00eff185f3130e37344938982e. Correct only the README description of local
# cache-context failure checks; no caller abort/recovery guarantee is claimed.
# Executable, native, test and build-recipe sources are unchanged. Inventory989,
# ABI12/80 and the qualified a15e SDK sample carry under the same CMake/generator/
# epoch settings. Documentation, Git pin and lock metadata change; runtime receipts
# remain bound to H1d2169da/Rbfe01a46, not this documentation successor. Portable1050,
# default ratchet572, job width, limits, selectors and clock policy stay unchanged.
# CARRY TO 00007ffab5bcec2f1635584aab98b8e934be4d71 (2026-10-19): exact replay of
# 9e11cab435dde363c9f065d172da1754b8c25909, following a8c3ba366f72155ab37d423f4241d40ffda729ff.
# Full reverie-dbt source equality preserves inventory989 and the a15e SDK
# recipe. This carries the original measured sample; it is not a new runtime
# or build result. Native/client changes remain covered by their own receipts.
# CARRY TO 0cc79d6c15cdb43134dafef39d97ee67847492cb (2026-10-19): replay the same owned
# change in order after 69efc09ebcc7bd3ea33bd033e9688500dc205191.
# Full DBT source equals 00007ffab5bcec2f1635584aab98b8e934be4d71 (inventory989).
# SDK/native recipe inputs carry; the a15e sample keeps its original attribution.
# Incoming workspace dependency changes require fresh paired build/runtime evidence.
# CARRY TO e7bf2d9b9d4f58c108b591b70ebf0e496de34f4b (2026-10-19): replay the same owned change
# from 0cc79d6c15cdb43134dafef39d97ee67847492cb onto the K3b stack rooted at
# published 63934937463458de93d43ed08038d256ae799c64. The owned DBT
# tree and SDK/native recipe inputs are identical (inventory989).
# Preserve the incoming K3b carry and the original a15e measurement above;
# this source equality is not a new build or runtime-package receipt.
# CARRY TO 9547c3594d3881dead7d0e7a04ea5f32df7a7745 (2026-10-19): from
# e7bf2d9b9d4f58c108b591b70ebf0e496de34f4b. One commit (signals lane, A5 step 3.0a:
# injected syscalls run under the guest's own mask on an unshared
# signal-action table). It changes reverie-ptrace/src/task.rs, sigtrap_state.rs
# and its plain-guest fixture and tests only; git diff is empty under
# reverie-dbt/ and for the root Cargo.toml, Cargo.lock and rust-toolchain.toml,
# so the inventory (989), SDK and native inputs, calibrated budget, job width
# and limits are unchanged from the e7bf2d9b carry.
# CARRY TO f950fb1e5a0bb2abbf8f2c47089d54e0564d36a0 (2026-10-19): from
# 9547c3594d3881dead7d0e7a04ea5f32df7a7745. One commit (signals lane, A5 step 3.0b:
# the SIGSEGV state a trapped rdtsc or cpuid resets is put back). It changes
# reverie-ptrace/src/task.rs, lib.rs, the new segv_state.rs, and its plain-guest
# fixture and tests only; git diff is empty under reverie-dbt/ and for the root
# Cargo.toml, Cargo.lock and rust-toolchain.toml, so the inventory (989), SDK and
# native inputs, calibrated budget, job width and limits are unchanged from the
# 9547c359 carry.
# CARRY TO d451e8dbc10461f875e78f65dbb3d5d28a718a42 (2026-10-19): from
# f950fb1e5a0bb2abbf8f2c47089d54e0564d36a0. One commit (dbt lane: admit DBT image leaders before their first instruction). It changes reverie-dbt/native/client.c
# only: build.rs, native/CMakeLists.txt and vendor/dynamorio stay
# byte-identical and no reverie-dbt file is added or removed, so the
# inventory (989), SDK recipe and resource budget carry without a cold SDK
# build. This is not runtime-correctness or binary/package identity evidence.
# CARRY TO 905b53dbdb37422fa5d1ea70c1e1df60e211cb18 (2026-10-19): from
# d451e8dbc10461f875e78f65dbb3d5d28a718a42. One commit (dbt lane: keep DBT runtime readiness outside the birth deadline). It changes reverie-dbt/native/client.c
# only: build.rs, native/CMakeLists.txt and vendor/dynamorio stay
# byte-identical and no reverie-dbt file is added or removed, so the
# inventory (989), SDK recipe and resource budget carry without a cold SDK
# build. This is not runtime-correctness or binary/package identity evidence.
# CARRY TO 5041073ee5204c976aa3fdba76b16035bbf7ef76 (2026-10-19): from
# 905b53dbdb37422fa5d1ea70c1e1df60e211cb18. One commit (signals lane, A5 step 3.2b part 2:
# signal report path, unreported-signal reasons, per-mask exemptions). It
# changes reverie/src (guest.rs, signal.rs, signal_delivery.rs, tool.rs),
# reverie-ptrace/src/task.rs and reverie-ptrace tests only; git diff is empty
# under reverie-dbt/ and for the root Cargo.toml, Cargo.lock and
# rust-toolchain.toml, so the inventory (989), SDK and native inputs,
# calibrated budget, job width and limits are unchanged from the 905b53db carry.
# CARRY TO 229e1fad57e19a378790ae1052b35fd70112754b (2026-10-20): from
# 5041073ee5204c976aa3fdba76b16035bbf7ef76. One documentation-only commit
# (signals lane, A5 step 3.2b part 4: how a Tool binds AfterFrame). It changes
# doc comments in reverie/src/signal_delivery.rs, reverie/src/tool.rs and
# reverie-ptrace/src/task.rs only; git diff is empty under reverie-dbt/ and
# for the root Cargo.toml, Cargo.lock and rust-toolchain.toml, so the
# inventory (989), SDK and native inputs, calibrated budget, job width and
# limits are unchanged from the 5041073e carry.
# CARRY TO a778f36cec8ba4b9f6ba4a811496a37d01dff8d4 (2026-10-20): from
# 229e1fad57e19a378790ae1052b35fd70112754b. One commit (main-red fix: the LiteInst
# artifact runner keeps a child's output that arrives after an empty read).
# It changes reverie-liteinst-artifact/src/lib.rs only; git diff is empty
# under reverie-dbt/ and for the root Cargo.toml, Cargo.lock and
# rust-toolchain.toml, so the inventory (989), SDK and native inputs,
# calibrated budget, job width and limits are unchanged from the 229e1fad
# carry.
# CARRY TO 9b015a747751c4ba83ca79745f21177ca0df9217 (2026-10-20): from
# 5041073ee5204c976aa3fdba76b16035bbf7ef76. The scoped pselect wrapper-validation
# purpose advances the callback ABI to 13; the table remains 80 bytes. Native
# client and three DynamoRIO inputs change; this is not an SDK byte-equality
# carry. No DBT path is added or removed (inventory 989), and root manifests,
# lockfile and build recipe are unchanged. One source-build sample for SDK
# fcc3a4ab3c7b2df4bb57c564dae8ce734ca1334338dd828030d63a3472d1b381
# took 26.44 seconds at 7 effective jobs (185.07 job-seconds, NUM_JOBS=8),
# below the existing 572 job-second default ratchet. Preserve the calibrated
# budgets, job width and resource limits; this sample does not recalibrate them
# or establish runtime-package identity. Fresh paired runtime evidence is required.
# REBASE TO 34386566313e8a71913435c60054d348afb6b5bb (2026-10-20): replay the unchanged
# API13 change on the documentation-only 229e1fad base above. The original
# measured build and its limits remain historical evidence, not a new result.
# Native/SDK afterimages match the original API13 change; fresh runtime evidence
# is required for the newly composed Hermit source.
# REBASE TO edb27ebed0310674972a7d7e63f24ad180441b76 (2026-10-20): replay the same scoped
# API13 change on published a778f36c, retaining its artifact-runner correction.
# Native and SDK inputs match the reviewed R72 afterimages; inventory stays989.
# The historical calibration is unchanged, not a new source-build measurement.
# This budget carry does not substitute for current source/runtime validation.
expected_pin=edb27ebed0310674972a7d7e63f24ad180441b76

# TAKE THE PIN, NOT WHATEVER ELSE THE PRODUCER PRINTED.
#
# This captured the whole of `--print-pin` and compared it. A later change made
# that command also emit a pin-uniformity report on stdout, so the capture became
# 941 characters over 8 lines and the comparison below COULD NEVER SUCCEED FOR
# ANY PIN, including a correctly calibrated one. Every node behind this wrapper
# then failed closed in about a second, and updating `expected_pin` could not fix
# it because the recorded side was never a sha. The producer is fixed too -- its
# report now goes to stderr -- but a value parsed out of a shared stream should
# be validated by the consumer rather than trusted to stay clean.
recorded_pin=$(
    "$ROOT_DIR/ci/run-reverie-pin-check.sh" --repo "$ROOT_DIR" --print-pin
)

# ⚠️ A REFUSAL EXITS 75 (EX_TEMPFAIL), NOT 2, AND THE DIFFERENCE IS THE WHOLE
# POINT OF THIS BLOCK.
#
# `scripts/validate.rs` defines NO_RESULT_EXIT_CODE = 75 as "the only nonzero
# code that is not a product failure" -- a completed node saying it COULD NOT
# DETERMINE ITS CONDITION. That is exactly what this wrapper is when it declines:
# it never invoked the command, so it has measured nothing and has no verdict to
# offer about the tree.
#
# Every layer above already distinguishes 75 and needs no change:
#     ledger_gate_result   75 -> "no_result", not "fail"
#     ledger_run_results   no_results>0 -> run result "no_result", NEVER "pass"
#     print_cost_table     renders "NO_RESULT", a distinct status from ok/FAIL
#
# THIS DOES NOT MAKE A REFUSAL QUIETER, and it cannot restore a false green.
# ci-hub/lib/qualifying_receipt.rs refuses a receipt on `result != "pass"` AND
# separately on `executed_tests == 0`; a declining wrapper trips both. no_result
# is strictly LESS green than fail, not more.
#
# WHAT IT STOPS BEING CONFUSED WITH, measured on this repository:
#     a genuine compile failure exits 101 -- cargo's code, passed through by the
#     `exec` below, verified: wrapping `exit 0|2|101` returns 0|2|101 unchanged
#     a refusal exited 2, a code nothing else on these 17 nodes produces
# Both were recorded as gate result "fail" with a bare `exit N` reason, so a node
# that compiled nothing was indistinguishable in the ledger from one that compiled
# and broke. The 2026-08-25 red at 323a87d1da5f was read as two failing builds by
# three separate reports; it was this wrapper declining, because that branch
# declared pin f4152f8f while its wrapper still expected 13cf8bcb.
#
# The `$# == 0` usage error above deliberately KEEPS exit 2: a caller that passed
# no command is a caller bug, not a node that declined, and it should stay loud.
DECLINED_EXIT_CODE=75

if [[ ! $recorded_pin =~ ^[0-9a-f]{40}$ ]]; then
    echo "run-with-reverie-dbt-budget.sh: --print-pin did not yield a 40-hex revision; got ${#recorded_pin} char(s): ${recorded_pin:0:80}" >&2
    echo "run-with-reverie-dbt-budget.sh: DECLINED (no_result, exit $DECLINED_EXIT_CODE): NOT RUNNING '$*' against an unidentified Reverie pin. Nothing was built or tested, so this node has no verdict about the tree." >&2
    exit "$DECLINED_EXIT_CODE"
fi
if [[ $recorded_pin != "$expected_pin" ]]; then
    echo "run-with-reverie-dbt-budget.sh: no calibrated budget for Reverie pin $recorded_pin (expected $expected_pin)" >&2
    echo "run-with-reverie-dbt-budget.sh: DECLINED (no_result, exit $DECLINED_EXIT_CODE): NOT RUNNING '$*'. Nothing was built or tested, so this node has no verdict about the tree -- it is NOT a build failure. To recalibrate, confirm reverie-dbt/vendor/dynamorio and reverie-dbt/build.rs are unchanged and CMAKE/CMAKE_GENERATOR select the same tooling between the pins, then update expected_pin here." >&2
    exit "$DECLINED_EXIT_CODE"
fi
REVERIE_DBT_BUDGET_BOUND_PIN=$recorded_pin
export REVERIE_DBT_BUDGET_BOUND_PIN

# shellcheck source=ci/configure-build-jobs.sh
source "$ROOT_DIR/ci/configure-build-jobs.sh" reverie-dbt-budget-child

echo "run-with-reverie-dbt-budget.sh: reverie-dbt-budget={pin:$REVERIE_DBT_BUDGET_BOUND_PIN,source:$REVERIE_DBT_BUILD_JOBS_SOURCE,raw-build-jobs:$REVERIE_DBT_RAW_BUILD_JOBS,effective-cpus-source:$REVERIE_DBT_EFFECTIVE_CPUS_SOURCE,effective-cpus:$REVERIE_DBT_EFFECTIVE_CPUS,reverie-max-jobs:$REVERIE_DBT_MAX_PARALLEL_JOBS,effective-native-jobs:$REVERIE_DBT_EFFECTIVE_BUILD_JOBS,effective-job-seconds:$REVERIE_DBT_MAX_BUILD_EFFECTIVE_JOB_SECONDS,max-elapsed-seconds:$REVERIE_DBT_MAX_BUILD_SECONDS,basis:existing-portable-budget,combined-sdk-key:a15e54f33b6693b6796540d3179389e897c30d7ee370a6bbf158c93fc5e6bc93,new-sdk-qualified-sample}" >&2

# A cargo build, check, clippy, doc, rustc or test command reruns once if the
# compiler itself crashed; every other command is exec'd unchanged
# (ci/retry-on-compiler-crash.sh).
# DIAGNOSTICS ONLY, 2026-10-18: HERMIT_CARGO_TIMINGS_DIR=<dir> asks a `cargo build` for
# Cargo's per-unit timing report (`--timings`, the HTML report under
# <target>/cargo-timings/) and copies the reports this invocation wrote into <dir>.
# build.workspace sets it to $E2E_RESULT_ROOT/cargo-timings, so every validation keeps
# the unit timeline of its validate-profile build at the width it really ran with.
# The exit status is the build's; a report that cannot be copied only warns. Unset
# or empty, nothing changes.
if [[ -n ${HERMIT_CARGO_TIMINGS_DIR:-} && ${1:-} == cargo && ${2:-} == build ]]; then
    timings_started=$(date +%s)
    timings_status=0
    "$ROOT_DIR/ci/retry-on-compiler-crash.sh" "$@" --timings || timings_status=$?
    timings_reports=${CARGO_TARGET_DIR:-$ROOT_DIR/target}/cargo-timings
    if ! mkdir -p -- "$HERMIT_CARGO_TIMINGS_DIR" ||
        ! find "$timings_reports" -maxdepth 1 -name 'cargo-timing-*.html' -newermt "@$timings_started" \
            -exec cp -- {} "$HERMIT_CARGO_TIMINGS_DIR/" \; ; then
        echo "run-with-reverie-dbt-budget.sh: could not keep the cargo --timings report in $HERMIT_CARGO_TIMINGS_DIR (diagnostics only)" >&2
    fi
    exit "$timings_status"
fi
exec "$ROOT_DIR/ci/retry-on-compiler-crash.sh" "$@"
