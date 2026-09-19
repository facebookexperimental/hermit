#!/usr/bin/env bash
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
expected_pin=91110d249ffd8957267d71fab8c83d9636105efe

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

echo "run-with-reverie-dbt-budget.sh: reverie-dbt-budget={pin:$REVERIE_DBT_BUDGET_BOUND_PIN,source:$REVERIE_DBT_BUILD_JOBS_SOURCE,raw-build-jobs:$REVERIE_DBT_RAW_BUILD_JOBS,effective-cpus-source:$REVERIE_DBT_EFFECTIVE_CPUS_SOURCE,effective-cpus:$REVERIE_DBT_EFFECTIVE_CPUS,reverie-max-jobs:$REVERIE_DBT_MAX_PARALLEL_JOBS,effective-native-jobs:$REVERIE_DBT_EFFECTIVE_BUILD_JOBS,effective-job-seconds:$REVERIE_DBT_MAX_BUILD_EFFECTIVE_JOB_SECONDS,max-elapsed-seconds:$REVERIE_DBT_MAX_BUILD_SECONDS,basis:github-portable-cold-miss-n3-affinity4,carried-to-pin-on-dynamorio-recipe-key:b0247764df7fba083f90538e12d3afcc8ffad5150c65bd321e689da5e57b74ed}" >&2

exec "$@"
