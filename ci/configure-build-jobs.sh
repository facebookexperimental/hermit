#!/usr/bin/env bash
# Shared inner-build width for every Hermit CI DAG launch path.
#
# The outer safe-ci cpu.max is a containment ceiling, not a request for Cargo to
# use every granted core. On the 316-CPU validation host that inference produced
# NUM_JOBS=284 and raced the native linker. K=8 is measurement-backed: on
# 2026-08-04 the pre-collapse build.dbt_release and rr_suite_contract nodes both
# completed at j8 under their cgroup-recorded memory caps. The collapsed fat-build
# nodes declare their independently measured higher width in the DAG manifest.
#
# This file has two explicit source modes. `launcher` preserves the historical
# shared Cargo widths and strips every portable DBT-budget variable before the
# DAG runner starts. `reverie-dbt-budget-child` is called only by the portable
# DBT wrapper, after safe-ci has entered the child and selected any child-local
# Cargo width.

CI_DAG_BUILD_JOBS=${CI_DAG_BUILD_JOBS:-8}
if [[ ! $CI_DAG_BUILD_JOBS =~ ^[1-9][0-9]*$ ]]; then
    echo "configure-build-jobs.sh: CI_DAG_BUILD_JOBS must be a positive integer" >&2
    return 2
fi

build_job_context=${1:-}
if [[ $build_job_context == launcher ]]; then
    # These variables are meaningful only in the two portable DBT build
    # children. Remove even planted ambient values so the privileged runner's
    # environment remains identical to the pre-budget launcher contract.
    unset REVERIE_DBT_BUDGET_BOUND_PIN
    unset REVERIE_DBT_BUILD_JOBS_SOURCE
    unset REVERIE_DBT_RAW_BUILD_JOBS
    unset REVERIE_DBT_EFFECTIVE_CPUS_SOURCE
    unset REVERIE_DBT_EFFECTIVE_CPUS
    unset REVERIE_DBT_MAX_PARALLEL_JOBS
    unset REVERIE_DBT_EFFECTIVE_BUILD_JOBS
    unset REVERIE_DBT_MAX_BUILD_EFFECTIVE_JOB_SECONDS
    unset REVERIE_DBT_MAX_BUILD_SECONDS

    # Retire the previous launcher-carried derivation names fail-closed too.
    unset CI_DAG_LAUNCH_WIDTH_BOUND
    unset CI_DAG_LAUNCH_BUILD_JOBS_SOURCE
    unset CI_DAG_LAUNCH_RAW_BUILD_JOBS
    unset CI_DAG_EFFECTIVE_CPUS
    unset CI_DAG_REVERIE_DBT_MAX_PARALLEL_JOBS
    unset CI_DAG_REVERIE_DBT_MAX_BUILD_JOB_SECONDS
    unset CI_DAG_REVERIE_DBT_MAX_BUILD_EFFECTIVE_JOB_SECONDS
    unset REVERIE_DBT_PINNED_MAX_PARALLEL_JOBS
    unset REVERIE_DBT_BUDGET_CHILD

    # Cargo converts this explicit pool width into build-script NUM_JOBS. Keep
    # the nested native-build knob identical so the Rust validator cannot widen it.
    export CARGO_BUILD_JOBS=$CI_DAG_BUILD_JOBS
    export THIRD_PARTY_BUILD_JOBS=$CI_DAG_BUILD_JOBS

    # AND LET A NODE'S OWN DECLARED WIDTH REACH CARGO, which until now it could not.
    #
    # The K=8 above is the FLOOR for a step that declares nothing. The comment at the
    # top of this file already says the collapsed fat-build nodes "declare their
    # independently measured higher width in the DAG manifest" -- build.workspace
    # declares preferred_inner_jobs=32 (as build.runtime_release did until the
    # one-build change of 2026-09-30 merged it away). That declaration had
    # never reached Cargo: every portable jobs_flag in ci/dag/validate.json is the empty string,
    # so the runner had no way to hand a step its width, and this line's ambient 8 was
    # the only value Cargo ever saw. Measured on the 2026-08-24 clean full run.
    #
    # $DAGRUN_JOBS_ENV names the environment variable through which the
    # runner delivers a step's width (agent-utils 3b9c272). The runner applies it as a
    # per-step overlay ON TOP of this ambient value, so a step that declares a width
    # gets it and a step that declares none still gets 8.
    #
    # DELIBERATELY NOT A NEW CONSTANT. 8 is not raised here and no width is invented:
    # the widths that now take effect are the ones already measured and recorded per
    # node in the DAG. Picking a fresh global number was rejected -- the historical 284
    # inference "raced the native linker" (see the header), and a sweep on a loaded box
    # is not evidence for a production default.
    export DAGRUN_JOBS_ENV=CARGO_BUILD_JOBS
    return 0
fi

if [[ $build_job_context != reverie-dbt-budget-child ]]; then
    echo "configure-build-jobs.sh: expected source mode launcher or reverie-dbt-budget-child" >&2
    return 2
fi

# fc97 briefly exported this unconditioned threshold before the budget was
# normalized to effective-job-seconds. A direct wrapper invocation must not
# carry that retired authority into Cargo; normal launchers scrub it above.
if [[ -v CI_DAG_REVERIE_DBT_MAX_BUILD_JOB_SECONDS ]]; then
    echo "configure-build-jobs.sh: retired CI_DAG_REVERIE_DBT_MAX_BUILD_JOB_SECONDS is not accepted in a DBT budget child" >&2
    return 2
fi

# The calibration below is valid only for Reverie 0384d673. The calibration
# itself is unchanged; see the carry chain below. The portable wrapper obtains
# the repository's recorded pin through the canonical checker and carries it
# here; a pin bump cannot silently retain the old clamp or threshold.
# CARRY TO ad598995 (2026-08-26): 200439dc..ad598995 is exactly
# rrnewton/reverie#496 and changes only reverie-process/src/container.rs.
# Both repository inputs to source_recipe_key are byte-identical by git object
# id: reverie-dbt/build.rs remains 0ff8ae24b974 and
# reverie-dbt/vendor/dynamorio remains de352475846e. The selected CMAKE and
# CMAKE_GENERATOR are unchanged, so the measured 1050 effective-job-second
# budget and MAX_PARALLEL_JOBS=16 carry unchanged. Fresh validation remains
# required because the runtime behavior changed.
#
# CARRY TO 200439dc (2026-08-26): a16e3c46..200439dc changes only
# reverie-ptrace/src/gdbstub/server.rs. reverie-dbt/build.rs remains blob
# 0ff8ae24b974 and reverie-dbt/vendor/dynamorio remains de352475846e. The pin
# does not alter the selected CMAKE or CMAKE_GENERATOR, so the complete recipe
# remains install key 132d77130980c546c8867fc196d97e664bc4816b1dfa9ea9c18de4a94d109c4d.
# The 1050 effective-job-second budget and MAX_PARALLEL_JOBS=16 carry unchanged.
# Fresh validation is still required, and an earlier pin's receipt is not valid
# for this pin.
#
# CARRY TO f4152f8f (2026-08-25), on the same recipe-identity evidence as the
# carries above, and stronger than any of them: `git diff 13cf8bcb f4152f8f --
# reverie-dbt` is EMPTY. Both repository inputs to source_recipe_key are
# byte-identical by git object id -- reverie-dbt/vendor/dynamorio de352475846e
# and reverie-dbt/build.rs 0ff8ae24b974. source_recipe_key also hashes the
# selected CMAKE and CMAKE_GENERATOR; the empirical install-key check below
# confirms the complete recipe identity. Unlike the previous carries, there is
# no reverie-dbt Rust change to argue about at all. MAX_PARALLEL_JOBS=16 is
# unchanged. Confirmed empirically: a build at f4152f8f produces DynamoRIO
# install key 132d7713..., the key this budget was measured against.
#
# CARRY TO b0c3cfe4 (2026-08-25): f4152f8f..b0c3cfe4 changes only
# reverie-memory/src/local.rs. reverie-dbt/build.rs remains blob
# 0ff8ae24b974 and reverie-dbt/vendor/dynamorio remains de352475846e, so
# every repository input to source_recipe_key is byte-identical. The measured
# effective-job-seconds budget and MAX_PARALLEL_JOBS=16 carry unchanged.
#
# ⚠️ THIS BINDING AND THE ONE IN ci/run-with-reverie-dbt-budget.sh MUST MOVE
# TOGETHER. They are two separate hard-coded revisions guarding one calibration,
# and updating only the wrapper leaves the whole budget child failing at
# `return 2` -- which looks identical to the refusal the wrapper was just fixed
# to stop emitting. ci/run-with-reverie-dbt-budget-test.sh exists because that is
# exactly what happened; it runs the wrapper end to end and so sees this layer.
# ⚠️ 75, NOT 2, AND IT MUST MOVE WITH THE WRAPPER. The comment above already
# records that updating only the wrapper leaves this child "failing at `return 2`
# -- which looks identical to the refusal the wrapper was just fixed to stop
# emitting". The same is true of the exit code: a wrapper that declines with 75
# while this layer declines with 2 reports the SAME condition as two different
# things depending on which guard fired first. Both are "could not determine",
# which is what EX_TEMPFAIL means to scripts/validate.rs.
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
# The intervening Reverie commits are the LiteInst task-creation change (#447)
# and a documentation commit (#511); neither can affect the elapsed time of a
# DynamoRIO content-key miss. Carry, not recalibration.
# CARRY TO bc106a19 (2026-08-28): both repository inputs to the DynamoRIO
# content-key miss are byte-identical to af42d9cf:
#     reverie-dbt/vendor/dynamorio  a3c41e5d3630 -> a3c41e5d3630
#     reverie-dbt/build.rs          0ff8ae24b974 -> 0ff8ae24b974
# The changed Reverie files are confined to reverie-ptrace timer recovery and
# tests. They require fresh Hermit validation but cannot change this measured
# native-build budget. Carry, not recalibration.
# CARRY TO c2e2c8fb (2026-09-02): both repository inputs to the DynamoRIO
# content-key miss are byte-identical to bc106a19 by git object id:
#     reverie-dbt/vendor/dynamorio  a3c41e5d3630 -> a3c41e5d3630
#     reverie-dbt/build.rs          0ff8ae24b974 -> 0ff8ae24b974
# The five intervening commits do not touch a native DBT recipe input. The
# measured native-build budget carries unchanged; fresh Rust build and validate
# evidence is still required for the Backend API change.
# CARRY TO 320412c5 (2026-09-04): both repository inputs to the DynamoRIO
# content-key miss are byte-identical to c2e2c8fb by git object id:
#     reverie-dbt/vendor/dynamorio  a3c41e5d3630 -> a3c41e5d3630
#     reverie-dbt/build.rs          0ff8ae24b974 -> 0ff8ae24b974
# The intervening Reverie changes include the KVM CPUID correction, but do not
# change the native DBT build recipe. The measured native-build budget carries
# unchanged; fresh Hermit validation is still required for the new pin.
# CARRY TO 37e7b727 (2026-09-04): every input to the DynamoRIO content-key miss
# is byte-identical to 320412c5 by git object id:
#     reverie-dbt/vendor/dynamorio  a3c41e5d3630 -> a3c41e5d3630
#     reverie-dbt/build.rs          0ff8ae24b974 -> 0ff8ae24b974
#     third-party/                  fb49c0ba7a9a -> fb49c0ba7a9a
# The two intervening commits change indexed CPUID behavior in the KVM and DBT
# runtime paths, not the native DBT build recipe. The measured native-build
# budget carries unchanged; fresh Hermit validation is still required.
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
# CARRY TO d87a03a3 (2026-09-16): the native-build inputs remain identical
# to a158914e by Git object identity:
#     reverie-dbt/vendor/dynamorio  a3c41e5d3630 -> a3c41e5d3630
#     reverie-dbt/build.rs          0ff8ae24b974 -> 0ff8ae24b974
#     reverie-dbt/Cargo.toml        8da5d73a60b9 -> 8da5d73a60b9
# The unchanged recipe hashes the full vendored tree, build.rs, CMAKE, and
# CMAKE_GENERATOR; this pin changes neither tool selection nor build options.
# The 16-job clamp and 1050 effective-job-second threshold carry unchanged
# at the existing default recipe key
# c9c1ee55257cbb0635b56f494a75ee1dc6af839ca8e289231f533b0208340463.
# The range also includes the paused-counter API change before the vFile fix.
# This is source identity, not a new timing sample or Hermit test receipt;
# fresh Rust build and unchanged selected CLI validation remain required.
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
# 30fee360: build.rs blob 0ff8ae24b97464044735ba79ea74765ba4ac3ff0 and
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
# BOUND TO 99d1e482 (2026-09-18): the SDK recipe changed to b0247764df7f.
# See "BOUNDED COLD SDK OBSERVATION AT 99d1e482" below for the native sample,
# failed enclosing Cargo check, and conservative 1050 effective-job-second
# threshold with a 16-job clamp. The single local sample does not replace
# the original hosted calibration.
# CARRY TO f97b7be1 (2026-09-18): the budget carries UNCHANGED, on the
# strongest form of the recipe-identity argument rather than an input-by-input
# one: `git diff 99d1e482..f97b7be1 -- reverie-dbt` is EMPTY, and the whole
# reverie-dbt subtree is one object, ad0ef5e0d8bd at both revisions. The two
# repository inputs to the DynamoRIO content-key miss this wrapper bounds are
# therefore identical by construction, and confirmed directly:
#
#   reverie-dbt/build.rs          0ff8ae24b974 -> 0ff8ae24b974  IDENTICAL
#   reverie-dbt/vendor/dynamorio  117d54d744df -> 117d54d744df  IDENTICAL
#
# Both resolved at both revisions, so this is measured identity and not the
# absent-reads-as-unchanged case the DBI->DBT path move can produce. The root
# Cargo.toml 4168dea2771f, rust-toolchain.toml fdd319e308cd and the
# third-party gitlink fb49c0ba7a9a are identical too. CMAKE and
# CMAKE_GENERATOR are host inputs rather than pin contents, so the measured
# SDK recipe, the 16-job clamp and the 1050 effective-job-second threshold all
# carry. Reverie f97b7be1 "Add process-pending alarm publication for KVM"
# touches only reverie-kvm/ and reverie/src/guest.rs; that runtime change is
# build-relevant and still requires fresh validation. This carry is source
# evidence for the build budget, not a new timing or Hermit guest measurement,
# and it does not reuse an earlier pin's receipt.
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
# CARRY TO 502bc21f897065766f1ef4c940ede1efe4743acd: the landed native KVM
# signal-setup repair https://github.com/rrnewton/reverie/pull/595 changes
# only failure_tests.rs and native_test_support.rs. Relative to sole parent
# 91110d249ffd8957267d71fab8c83d9636105efe, the complete reverie-dbt tree
# d12ba78b0f844791815db06eb41f07a818f99967, build.rs, DynamoRIO vendor tree,
# root Cargo/toolchain/.gitmodules and third-party inputs are identical.
# Keep default CMAKE, unset CMAKE_GENERATOR, the 1050 effective-job-second
# budget and 16-job clamp. This is source-identity carry, not new calibration
# or transferred runtime evidence; prior calibration limitations remain.
# CARRY TO f7bd85e11dd258112148ed2cba6531501a1a00d9 (2026-09-19): relative to
# 502bc21f897065766f1ef4c940ede1efe4743acd, the complete reverie-dbt tree
# remains d12ba78b0f844791815db06eb41f07a818f99967, including build.rs blob
# 0ff8ae24b97464044735ba79ea74765ba4ac3ff0 and DynamoRIO vendor tree
# 117d54d744df23921c531d0fe08537249f5a510a. Root Cargo/toolchain/.gitmodules
# and third-party inputs are unchanged. Preserve CMAKE/CMAKE_GENERATOR
# selection, the 16-job clamp and 1050 effective-job-second threshold.
# This source-identity carry adds no timing sample or runtime evidence;
# prior calibration limitations remain.
# CARRY TO be09e5100bca6dad77aede0349de9a5c92990854 (2026-09-20): relative to
# f7bd85e11dd258112148ed2cba6531501a1a00d9, reverie-dbt/build.rs has no
# diff and the DynamoRIO vendor tree remains
# 117d54d744df23921c531d0fe08537249f5a510a. Preserve CMAKE/CMAKE_GENERATOR,
# the 16-job clamp, and 1050 effective-job-second threshold. This is
# source-identity carry, not new calibration or runtime qualification;
# prior limitations remain.
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
# CARRY TO a1d07619c7c06d3a525a9db5b6f113bb52a4901b (2026-09-27): from
# b0ede531e00dd1e068d0e0b1f220edddcf96d4b5, KVM orphan handling, worker
# retirement and terminal WUNTRACED support leave the full reverie-dbt tree
# unchanged at a62d15302ee5e907667d1c02f6e629177ad87f61. build.rs remains
# blob 0ff8ae24b97464044735ba79ea74765ba4ac3ff0; DynamoRIO remains tree
# 117d54d744df23921c531d0fe08537249f5a510a; third-party remains tree
# fb49c0ba7a9abd48a4ea662bf20e08246c81fc5a. CMAKE/CMAKE_GENERATOR selection,
# MAX_PARALLEL_JOBS=16 and 1050 effective-job-seconds carry unchanged.
# This is source-identity carry, not a new timing sample or runtime qualification.
# CARRY TO 6297f7154299e30bed97e6aead6ae7f5e1fc45ed (2026-09-27): from
# a1d07619c7c06d3a525a9db5b6f113bb52a4901b, the KVM wait4 copyout permission
# repair leaves the complete reverie-dbt tree unchanged at
# a62d15302ee5e907667d1c02f6e629177ad87f61. build.rs remains blob
# 0ff8ae24b97464044735ba79ea74765ba4ac3ff0; DynamoRIO remains tree
# 117d54d744df23921c531d0fe08537249f5a510a. CMAKE/CMAKE_GENERATOR selection,
# MAX_PARALLEL_JOBS=16 and 1050 effective-job-seconds carry unchanged.
# This is source-identity carry, not a new timing sample or runtime qualification.
# CARRY TO 2eeb704cefb166ef7411478a1bb37e2867cf3644 (2026-09-29): from
# 6297f7154299e30bed97e6aead6ae7f5e1fc45ed, the 51-commit range changes reverie-dbt
# native/client.c, src/ and tests/ (tree a62d15302ee5e907667d1c02f6e629177ad87f61
# -> 0e92df9a7348861fab977a98165a616da6c4f41f), none of which is a DynamoRIO SDK
# recipe input: build_dynamorio times only the CMake configure/build of
# vendor/dynamorio. build.rs remains blob 0ff8ae24b97464044735ba79ea74765ba4ac3ff0;
# DynamoRIO remains tree 117d54d744df23921c531d0fe08537249f5a510a; third-party
# remains tree fb49c0ba7a9abd48a4ea662bf20e08246c81fc5a; root Cargo.toml remains
# blob 4168dea2771f18a00fb1afdfd2218efba415ecbb. rust-toolchain.toml changes only
# an @fb-only comment. CMAKE/CMAKE_GENERATOR selection, MAX_PARALLEL_JOBS=16 and
# 1050 effective-job-seconds carry unchanged. This is source-identity carry, not
# a new timing sample or runtime qualification.
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
if [[ ${REVERIE_DBT_BUDGET_BOUND_PIN:-} != 049750ad5bfd93f3a315747cc259b5593f756d75 ]]; then
    echo "configure-build-jobs.sh: DECLINED (no_result, exit 75): DBT budget is not bound to Reverie 049750ad5bfd93f3a315747cc259b5593f756d75 (bound pin: ${REVERIE_DBT_BUDGET_BOUND_PIN:-<unset>})" >&2
    return 75
fi

if [[ -n ${CARGO_BUILD_JOBS:-} ]]; then
    REVERIE_DBT_RAW_BUILD_JOBS=$CARGO_BUILD_JOBS
    if [[ ${DAGRUN_IN_SCOPE:-} == 1 ]]; then
        REVERIE_DBT_BUILD_JOBS_SOURCE=runner-child-cargo-build-jobs
    else
        REVERIE_DBT_BUILD_JOBS_SOURCE=inherited-launch-cargo-build-jobs
    fi
else
    REVERIE_DBT_RAW_BUILD_JOBS=$CI_DAG_BUILD_JOBS
    REVERIE_DBT_BUILD_JOBS_SOURCE=ci-dag-build-jobs-fallback
fi
if [[ ! $REVERIE_DBT_RAW_BUILD_JOBS =~ ^[1-9][0-9]*$ ]]; then
    echo "configure-build-jobs.sh: selected raw build width must be a positive integer" >&2
    return 2
fi

# Observe affinity/cpuset visibility in this child, after safe-ci has applied
# its containment. A launcher observation would be only a correlated proxy for
# the CPUs available to the native build.
if ! REVERIE_DBT_EFFECTIVE_CPUS=$(nproc); then
    echo "configure-build-jobs.sh: child nproc observation failed" >&2
    return 2
fi
REVERIE_DBT_EFFECTIVE_CPUS_SOURCE=child-nproc
if [[ ! $REVERIE_DBT_EFFECTIVE_CPUS =~ ^[1-9][0-9]*$ ]]; then
    echo "configure-build-jobs.sh: child nproc must return a positive integer" >&2
    return 2
fi

# Reverie 9470712's DynamoRIO build.rs clamps Cargo NUM_JOBS to 16 before
# passing it to `cmake --parallel`. Carry the calibrated threshold together with
# every condition used to convert it into elapsed seconds:
#
#   effective native jobs = min(requested jobs, child CPUs, Reverie clamp)
#   max elapsed seconds = ceil(effective-job-second threshold / effective jobs)
#
# PROVENANCE (GitHub portable run 31008044311 at Hermit f21b22ed, requested
# jobs=8, runner affinity=4): three content-key misses measured 115.82s,
# 128.27s, and 131.21s -- one debug build and two concurrent release builds --
# i.e. 463.28, 513.08, and 524.84 effective-job-seconds at min(8, 4, 16)=4.
# Reverie's original ratchet policy used 2x the slowest of n=3 clean
# observations; applying that policy and rounding up gives 1050
# effective-job-seconds. The concurrent release builds embody contention;
# replace this calibration when >=5 clean Hermit-lane samples support it.
#
# CARRY TO 9470712 (2026-08-05). The threshold above was measured at 025d378
# and is reused here, so the reuse is evidenced rather than assumed. The budget
# governs exactly one quantity: the elapsed time reverie-dbt/build.rs reports
# for a DynamoRIO content-key MISS. That build's inputs are hashed by
# source_recipe_key() over {reverie-dbt/vendor/dynamorio, reverie-dbt/build.rs,
# $CMAKE, $CMAKE_GENERATOR} -- host-invariant while CMAKE/CMAKE_GENERATOR are
# unset -- and six cold builds (three per pin, interleaved on one host,
# taskset 4 CPUs, CARGO_BUILD_JOBS=4) all printed the SAME recipe key
# sha256:19123c88d87a4cd9e8b0efdda7265c7682e8907fe6bbf8e0bd6fcb92fbfa85e4.
# Elapsed at 9470712: 39.80s / 39.23s / 39.52s (159.20 / 156.92 / 158.08
# effective-job-seconds); at 025d378: 38.10s / 39.58s / 41.01s (152.40 /
# 158.32 / 164.04). The new pin's slowest sample is 3% faster than the old
# pin's slowest and the whole set spans 7.1%, so the pin move causes no
# throughput change. Corroborating Git evidence: 025d378..9470712 touches only
# reverie-ptrace/src/{error,task,tracer}.rs; the reverie-dbt subtree
# (c38c979057f9fe3e4d46772c1fddd05a71db4bf9) and third-party/
# (fb49c0ba7a9abd48a4ea662bf20e08246c81fc5a) are identical at both pins, and
# MAX_PARALLEL_JOBS is still 16.
#
# CARRY TO e159d6c (2026-08-06). The only 9470712..e159d6c change is a
# hostname-neutral wording edit in reverie-dbt/build.rs. The vendored
# DynamoRIO tree, build commands, MAX_PARALLEL_JOBS=16 clamp, and
# CI_MAX_BUILD_JOB_SECONDS=572 remain identical. Because source_recipe_key()
# deliberately hashes the full build script, its default-tool identity changes
# to sha256:76403e8e76b128119be4a7192893b7ec3084aeb85f4bd0377198a538d94b2a1d.
# A cold local CARGO_BUILD_JOBS=4 check observed the new identity and completed
# its native build in 30.73s (122.92 effective-job-seconds). This confirms the
# identity transition but does not replace the slower GitHub-runner calibration.
#
# CARRY TO 6a6b4ec (2026-08-06). The e159d6c..6a6b4ec changes are confined to
# reverie-kvm task lifecycle, process-tree exit accounting, and KVM tests.
# reverie-dbt/build.rs, its vendored DynamoRIO tree, build commands, and the
# MAX_PARALLEL_JOBS=16 clamp are byte-identical, so source_recipe_key() remains
# sha256:76403e8e76b128119be4a7192893b7ec3084aeb85f4bd0377198a538d94b2a1d.
# CI_MAX_BUILD_JOB_SECONDS=572 and the measured hosted-runner budget therefore
# carry without changing the derivation.
#
# CARRY TO dd3c178 (2026-08-06). The only 6a6b4ec..dd3c178 change adds
# reverie-kvm sendmsg/recvmsg ancillary-data translation and KVM tests.
# reverie-dbt/build.rs, its vendored DynamoRIO tree, build commands, and the
# MAX_PARALLEL_JOBS=16 clamp remain byte-identical. The DBT recipe identity
# therefore remains sha256:76403e8e76b128119be4a7192893b7ec3084aeb85f4bd0377198a538d94b2a1d,
# and the hosted-runner budget carries unchanged.
#
# CARRY TO 0ae0c01 (2026-08-06). dd3c178..0ae0c01 is rrnewton/reverie#396,
# which revives the KVM backend: it stops answering the `Guest::ppid`
# traced-tree contract from the guest-visible getppid() value, so Detcore
# registers the root thread again. Before it, every `hermit --backend kvm run`
# hung before the first guest syscall, including /bin/true.
#
# `git diff --name-only dd3c178..0ae0c01` is exactly two files, both KVM:
#   reverie-kvm/src/elf.rs
#   reverie-kvm/src/executor.rs
# The DBT inputs are byte-identical by git object identity at both pins --
# reverie-dbt/build.rs 9e35e1b699b7, reverie-dbt/vendor/dynamorio de352475846e,
# third-party fb49c0ba7a9a, and the whole reverie-dbt subtree eb284556d2df --
# so source_recipe_key() is unchanged at
# sha256:76403e8e76b128119be4a7192893b7ec3084aeb85f4bd0377198a538d94b2a1d and
# the MAX_PARALLEL_JOBS=16 clamp still applies. The hosted-runner budget
# therefore carries without re-derivation. This carry is evidenced by tree
# identity rather than by a fresh timing run, exactly as the 6a6b4ec and
# dd3c178 carries above: no DBT build input changed, so there is nothing for a
# new timing sample to measure.
#
# CARRY TO 6144323 (2026-08-07). 0ae0c01..6144323 is exactly one commit,
# rrnewton/reverie#377 (HybridPtrace A-class lifecycle-owner for reverie-e9patch),
# touching 8 files: reverie-e9patch/{README.md,src/backend.rs,src/lib.rs,
# src/runtime.rs}, reverie-preload/{README.md,src/lifecycle.rs}, and
# reverie-ptrace/{src/tracer.rs,tests/stdio_drain.rs}. NONE is a DBT input.
#
# Verified by git object identity at both pins, not by inspection: build.rs
# 9e35e1b699b7, vendor/dynamorio de352475846e, third-party fb49c0ba7a9a, and the
# whole reverie-dbt subtree eb284556d2df are byte-identical at 0ae0c01 and at
# 6144323 -- the same four object ids this file already records for 0ae0c01, so
# the recorded evidence for the previous carry independently checks out too.
# source_recipe_key() is therefore unchanged at
# sha256:76403e8e76b128119be4a7192893b7ec3084aeb85f4bd0377198a538d94b2a1d and the
# MAX_PARALLEL_JOBS=16 clamp (reverie-dbt/build.rs:25) still applies, so the
# hosted-runner budget carries without re-derivation. Evidenced by tree identity
# rather than a fresh timing run, exactly as the 6a6b4ec, dd3c178 and 0ae0c01
# carries above: no DBT build input changed, so there is nothing to re-measure.
#
# CARRY TO 038e993 (2026-08-07). NOTE: unlike the 6a6b4ec/dd3c178/0ae0c01/6144323
# carries above, the whole reverie-dbt subtree is NOT identical this time, so the
# argument is narrower and is stated explicitly rather than reused.
#
# 6144323..038e993 touches reverie-dbt/native/client.c, two test fixtures
# (first_scrub_marker.c, stack_scrub_marker.c) and one test
# (stack_scrub_preserves_guest_data.rs).
#
# The budget governs exactly one quantity: the elapsed time build_dynamorio()
# reports on a DynamoRIO content-key MISS. source_recipe_key() is computed over
# (source_dir = reverie-dbt/vendor/dynamorio, reverie-dbt/build.rs, $CMAKE,
# $CMAKE_GENERATOR) -- see reverie-dbt/build.rs:75-80 -- and ALL FOUR are
# unchanged: vendor/dynamorio and build.rs are byte-identical at both pins.
# build_dynamorio() only cmake-configures and cmake-builds source_dir
# (build.rs:199-220); native/client.c is not referenced by build.rs at all and is
# compiled outside the timed region. So the recipe identity remains
# sha256:76403e8e76b128119be4a7192893b7ec3084aeb85f4bd0377198a538d94b2a1d, the
# MAX_PARALLEL_JOBS=16 clamp still applies, and the measured MISS cost is
# unaffected by a client.c edit.
#
# CARRY TO 108f9ab (2026-08-08). This is the WIDEST carry argument of the set,
# not the narrowest: 038e993..108f9ab is a SINGLE commit that touches exactly
# one file, AGENTS.md (+22/-0, documentation only). No Rust, no C, no build
# script, no vendored source. Evidenced by tree identity, not a timing run:
#
#   git diff --name-only 038e993..108f9ab            -> AGENTS.md
#   git rev-parse 038e993:reverie-dbt                -> 5c15596f739710b48aaafe6f90b9dc6f5f1a4b8a
#   git rev-parse 108f9ab:reverie-dbt                -> 5c15596f739710b48aaafe6f90b9dc6f5f1a4b8a
#   git rev-parse 038e993:reverie-dbt/vendor/dynamorio -> de352475846e385002c1e4e54604fa0a7647b2de
#   git rev-parse 108f9ab:reverie-dbt/vendor/dynamorio -> de352475846e385002c1e4e54604fa0a7647b2de
#   git rev-parse 038e993:reverie-dbt/build.rs       -> 9e35e1b699b76d8b9f8a6adacc21c7a095f4f8f7
#   git rev-parse 108f9ab:reverie-dbt/build.rs       -> 9e35e1b699b76d8b9f8a6adacc21c7a095f4f8f7
#
# The whole reverie-dbt subtree is byte-identical (same tree object), so unlike
# the 038e993 carry there is no client.c caveat to reason around. All four
# source_recipe_key() inputs are unchanged, the recipe identity remains
# sha256:76403e8e76b128119be4a7192893b7ec3084aeb85f4bd0377198a538d94b2a1d, the
# MAX_PARALLEL_JOBS=16 clamp still applies, and the measured MISS cost cannot
# have moved because no DBT build input exists that differs between the pins.
#
# Those 2026-08-05 samples deliberately do NOT replace 1050. They come from a
# development host whose cores finish the identical work ~3.3x faster than the
# GitHub portable runner this budget governs; 2x their slowest would give 319
# effective-job-seconds and would fail the portable lane on its first genuine
# cold miss. The replacement bar stated above -- >=5 clean Hermit-lane samples
# -- is unchanged and still unmet.
#
# CARRY TO 5bf9e0b (2026-08-08, second bump of the day). Narrower than the
# 108f9ab carry and evidenced the same way -- tree identity, not a timing run.
# 108f9ab..5bf9e0b is a SINGLE commit touching exactly two files, both in
# reverie-ptrace (timer.rs, vdso.rs: making two DEBUG log sites reproducible
# across identical runs). No C, no build script, no vendored source, and
# nothing under reverie-dbt at all:
#
#   git log --oneline 108f9ab..5bf9e0b   -> 5bf9e0b reverie-ptrace: make two
#                                           DEBUG log sites reproducible
#   git diff --name-only 108f9ab..5bf9e0b -> reverie-ptrace/src/timer.rs
#                                            reverie-ptrace/src/vdso.rs
#   git rev-parse 108f9ab:reverie-dbt                  -> 5c15596f739710b48aaafe6f90b9dc6f5f1a4b8a
#   git rev-parse 5bf9e0b:reverie-dbt                  -> 5c15596f739710b48aaafe6f90b9dc6f5f1a4b8a
#   git rev-parse 108f9ab:reverie-dbt/vendor/dynamorio -> de352475846e385002c1e4e54604fa0a7647b2de
#   git rev-parse 5bf9e0b:reverie-dbt/vendor/dynamorio -> de352475846e385002c1e4e54604fa0a7647b2de
#   git rev-parse 108f9ab:reverie-dbt/build.rs         -> 9e35e1b699b76d8b9f8a6adacc21c7a095f4f8f7
#   git rev-parse 5bf9e0b:reverie-dbt/build.rs         -> 9e35e1b699b76d8b9f8a6adacc21c7a095f4f8f7
#
# All four source_recipe_key() inputs are unchanged, so the recipe identity
# remains sha256:76403e8e76b128119be4a7192893b7ec3084aeb85f4bd0377198a538d94b2a1d,
# the MAX_PARALLEL_JOBS=16 clamp still applies, and the measured MISS cost
# cannot have moved because no DBT build input differs between the pins.
# Budget values (1050 effective-job-seconds, 263/66 max-elapsed-seconds) carry
# unchanged. The >=5-clean-Hermit-lane-samples replacement bar is still unmet.
#
# CARRY ACROSS THE DBT RENAME, AND THE RECIPE KEY DOES CHANGE HERE (2026-08-08).
# Unlike every carry above, this one is NOT key-preserving. The rename moves
# reverie-dbi/build.rs to reverie-dbt/build.rs and edits its DBT-facing
# environment-variable and diagnostic names. source_recipe_key() deliberately
# hashes the full build script, so the default-tool identity becomes
# sha256:019b79670b3572c1afc2690932dd3fbbf70bbc9d0d96b5086ea121422de4bbb9,
# observed by a sequential cold build at reverie 88363a5
# (CARGO_BUILD_JOBS=1 cargo build -p reverie-dbt -j 1, DynamoRIO source build
# 108.37s). That single development-host sample corroborates the identity
# transition; it does NOT replace the hosted-runner calibration or its
# >=5-sample replacement bar, and the budget values below are unchanged.
#
# AND THAT KEY SURVIVES THE PIN MOVE TO fb963d90. source_recipe_key() hashes
# exactly {vendor/dynamorio, build.rs, $CMAKE, $CMAKE_GENERATOR}. Measured
# 88363a5 -> fb963d90:
#   reverie-dbt/vendor/dynamorio -> de352475846e385002c1e4e54604fa0a7647b2de (identical)
#   reverie-dbt/build.rs         -> af2faa442335... (identical)
#   reverie-dbt (whole subtree)  -> 31ed9e93 -> 7cf124ac (DIFFERS)
# The subtree differs only because fb963d90 is "Finish DBT rename across
# rebased native client", i.e. native/client.c -- which is NOT a
# source_recipe_key() input. So 019b7967 is the correct key at this pin.
#
# CARRY TO ab44bbf7 (2026-08-08). THE CALIBRATION DECISION IS STATED, NOT
# DEFAULTED: the budget carries UNCHANGED, and this is the widest carry in the
# chain -- the entire reverie-dbt subtree is the SAME TREE OBJECT at both pins.
#
#   git log --oneline fb963d90..ab44bbf7  -> ab44bbf7 validate.sh: name the writer in every ledger row
#                                            7d87ba30 Use short host names in benchmark evidence
#                                            9f4fa6c0 Convert SysInfo to libc::sysinfo field-wise
#   git diff --name-only fb963d90..ab44bbf7 -> benchmarks/counter2-shootout/INITIAL_RESULTS.md
#                                              benchmarks/counter2-shootout/results/.../metadata.json
#                                              reverie-syscalls/src/args/sysinfo.rs
#                                              validate.sh          (reverie's own, not hermit's)
#   git rev-parse fb963d90:reverie-dbt                  -> 7cf124ac7a88...
#   git rev-parse ab44bbf7:reverie-dbt                  -> 7cf124ac7a88...  IDENTICAL (whole subtree)
#   git rev-parse ab44bbf7:reverie-dbt/vendor/dynamorio -> de352475846e385002c1e4e54604fa0a7647b2de
#   git rev-parse ab44bbf7:reverie-dbt/build.rs         -> af2faa442335...
#
# Nothing under reverie-dbt changed at all, so both source_recipe_key() file
# inputs are byte-identical, the recipe identity remains
# sha256:019b79670b3572c1afc2690932dd3fbbf70bbc9d0d96b5086ea121422de4bbb9, the
# MAX_PARALLEL_JOBS=16 clamp still applies, and the measured MISS cost cannot
# have moved. Budget values (1050 effective-job-seconds, 263/66 max-elapsed)
# carry unchanged. The >=5-clean-Hermit-lane-samples replacement bar is unmet.
#
# BUILD-RELEVANT ANYWAY, and that is a separate axis from the budget:
# 9f4fa6c0 edits reverie-syscalls/src/args/sysinfo.rs, and reverie-syscalls is
# one of the crates hermit compiles. So this bump requires REAL revalidation --
# a prior receipt cannot be reused even though the DBT budget is untouched.
#
# CARRY TO 0384d673 (2026-08-08). The calibration carries unchanged because
# neither input to source_recipe_key() changed across ab44bbf7..0384d673:
#
#   git diff --name-status ab44bbf7..0384d673 -- reverie-dbt -> no output
#   git rev-parse ab44bbf7:reverie-dbt/vendor/dynamorio -> de352475846e385002c1e4e54604fa0a7647b2de
#   git rev-parse 0384d673:reverie-dbt/vendor/dynamorio -> de352475846e385002c1e4e54604fa0a7647b2de
#   git rev-parse ab44bbf7:reverie-dbt/build.rs -> byte-identical to 0384d673
#
# The three intervening commits change LiteInst, ptrace, and RPC transport,
# none of which can affect the DynamoRIO content-key miss measured by this
# budget. They remain build-relevant and therefore require fresh validation;
# this carry does not authorize receipt reuse.
#
# CARRY TO 8f4eb9ef (2026-08-09). The calibration carries unchanged because
# neither input to source_recipe_key() changed across 0384d673..8f4eb9ef:
#
#   git diff --name-status 0384d673..8f4eb9ef -- reverie-dbt -> no output
#   git rev-parse 0384d673:reverie-dbt/vendor/dynamorio -> de352475846e385002c1e4e54604fa0a7647b2de
#   git rev-parse 8f4eb9ef:reverie-dbt/vendor/dynamorio -> de352475846e385002c1e4e54604fa0a7647b2de
#   git rev-parse 0384d673:reverie-dbt/build.rs -> af2faa442335c1914f24a633d9cf2aa12820034b
#   git rev-parse 8f4eb9ef:reverie-dbt/build.rs -> af2faa442335c1914f24a633d9cf2aa12820034b
#
# The 14 intervening commits are build-relevant but cannot affect the
# DynamoRIO content-key miss measured by this budget. MAX_PARALLEL_JOBS=16 and
# the 1050 effective-job-second threshold carry unchanged. Fresh validation is
# still required; this carry does not authorize receipt reuse.
#
# CARRY TO 99437f05 (2026-08-09). The calibration carries unchanged because
# neither input to source_recipe_key() changed across 8f4eb9ef..99437f05:
#
#   git diff --name-status 8f4eb9ef..99437f05 -- reverie-dbt -> no output
#   git rev-parse 8f4eb9ef:reverie-dbt/vendor/dynamorio -> de352475846e385002c1e4e54604fa0a7647b2de
#   git rev-parse 99437f05:reverie-dbt/vendor/dynamorio -> de352475846e385002c1e4e54604fa0a7647b2de
#   git rev-parse 8f4eb9ef:reverie-dbt/build.rs -> af2faa442335c1914f24a633d9cf2aa12820034b
#   git rev-parse 99437f05:reverie-dbt/build.rs -> af2faa442335c1914f24a633d9cf2aa12820034b
#
# The sole intervening commit changes only Reverie's validation entrypoint,
# outside the DynamoRIO content-key recipe. MAX_PARALLEL_JOBS=16 and the 1050
# effective-job-second threshold carry unchanged. Fresh validation is still
# required; this carry does not authorize receipt reuse.
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
REVERIE_DBT_MAX_PARALLEL_JOBS=16
REVERIE_DBT_MAX_BUILD_EFFECTIVE_JOB_SECONDS=1050
REVERIE_DBT_EFFECTIVE_BUILD_JOBS=$REVERIE_DBT_RAW_BUILD_JOBS
if ((REVERIE_DBT_EFFECTIVE_CPUS < REVERIE_DBT_EFFECTIVE_BUILD_JOBS)); then
    REVERIE_DBT_EFFECTIVE_BUILD_JOBS=$REVERIE_DBT_EFFECTIVE_CPUS
fi
if ((REVERIE_DBT_MAX_PARALLEL_JOBS < REVERIE_DBT_EFFECTIVE_BUILD_JOBS)); then
    REVERIE_DBT_EFFECTIVE_BUILD_JOBS=$REVERIE_DBT_MAX_PARALLEL_JOBS
fi
REVERIE_DBT_MAX_BUILD_SECONDS=$((
    (REVERIE_DBT_MAX_BUILD_EFFECTIVE_JOB_SECONDS +
        REVERIE_DBT_EFFECTIVE_BUILD_JOBS - 1) /
        REVERIE_DBT_EFFECTIVE_BUILD_JOBS
))

# CARRY TO 3494609 (2026-08-10). RECIPE IDENTITY MOVES; THE BUDGET CARRIES.
# This is the e159d6c case, not the ab44bbf7 case: reverie-dbt/build.rs CHANGED,
# so source_recipe_key() necessarily changes, but the work it keys has not.
#
#   git rev-parse 99437f05:reverie-dbt/vendor/dynamorio -> de352475846e385002c1e4e54604fa0a7647b2de
#   git rev-parse 3494609 :reverie-dbt/vendor/dynamorio -> de352475846e385002c1e4e54604fa0a7647b2de
#                                                          IDENTICAL -- the compiled source is the same tree.
#
# The five commits 99437f05..3494609 are DynamoRIO BUILD-CACHE MANAGEMENT:
#   5dffda1 Share DynamoRIO installs across Cargo fingerprints
#   1a227a9 Exercise concurrent DynamoRIO cache publication
#   3d9756a Reject incomplete DynamoRIO cache installs
#   4664b5e Bind DynamoRIO cache hits to build provenance
#   3494609 Handle both Cargo OUT_DIR cache layouts
# They relocate the install under a shared cache root, stage into a temporary
# directory, quarantine an install that fails a usability check, and rebuild.
# Every one of them changes whether a build is a HIT or a MISS. NONE changes
# what a MISS compiles: the vendored tree is byte-identical and the cmake
# invocation is unchanged. The budget governs exactly one quantity -- the
# elapsed time of a content-key MISS -- so its worst case is bounded by the same
# cold DynamoRIO compile as before. The staging copy/rename these commits add is
# negligible beside that compile, and the added quarantine path leads to the
# already-budgeted cold build.
#
# NEW RECIPE IDENTITY, DERIVED NOT GUESSED. source_recipe_key() was
# reimplemented from the build.rs at 3494609 (hash_tree/hash_file/hash_value/
# hash_name, usize::to_le_bytes framing) and FIRST VALIDATED AGAINST THE
# RECORDED VALUE: fed the vendored tree and build.rs at 99437f05 it reproduces
# sha256:019b79670b3572c1afc2690932dd3fbbf70bbc9d0d96b5086ea121422de4bbb9
# exactly -- the identity this chain already recorded. Only then was it used to
# derive the value at 3494609:
#   sha256:63e29544455c901f05e37224b52e7f9734480d7c05914083bdcbd335968e6429
# A key computed by a reimplementation that could not reproduce the known
# answer would be a number, not evidence; the positive control is what makes
# this one usable.
#
# CONFIRMED BY THE REAL BUILD, not only by the reimplementation. A cold
# `cargo build --workspace` at this pin ran the actual build.rs at 3494609 and
# printed its own content key:
#   cargo:warning=DynamoRIO build cache MISS key=sha256:63e29544455c901f05e37224b52e7f9734480d7c05914083bdcbd335968e6429
# identical to the derived value. The derivation and the running code agree.
# This is still NOT a substitute for the hosted-runner calibration, exactly as
# the e159d6c entry noted for its own identity transition.
#
# Budget values (MAX_PARALLEL_JOBS=16, 1050 effective-job-seconds, 263/66
# max-elapsed) carry unchanged. The >=5-clean-Hermit-lane-samples replacement
# bar is unmet, so nothing is recalibrated here.
#
# BUILD-RELEVANT ANYWAY: reverie-dbt/build.rs is compiled by hermit, so this
# bump requires REAL revalidation; no prior receipt may be reused.

# CARRY TO 0fd04fe (2026-08-11). The calibration carries unchanged because
# every versioned input to the DynamoRIO content-key miss is object-identical
# across 3494609..0fd04fe:
#
#   git diff --name-status 3494609..0fd04fe -- reverie-dbt -> no output
#   git rev-parse 3494609:reverie-dbt -> bffe51c6a6e47ebd64ab1e055eed5165f83237a6
#   git rev-parse 0fd04fe:reverie-dbt -> bffe51c6a6e47ebd64ab1e055eed5165f83237a6
#   git rev-parse 3494609:reverie-dbt/build.rs -> 209bca718ea9b6d026a26abf5cbd8accbd346068
#   git rev-parse 0fd04fe:reverie-dbt/build.rs -> 209bca718ea9b6d026a26abf5cbd8accbd346068
#   git rev-parse 3494609:reverie-dbt/vendor/dynamorio -> de352475846e385002c1e4e54604fa0a7647b2de
#   git rev-parse 0fd04fe:reverie-dbt/vendor/dynamorio -> de352475846e385002c1e4e54604fa0a7647b2de
#
# The two intervening commits modify only AGENTS.md. They do not change the
# vendored DynamoRIO source, the build recipe or commands, workspace/toolchain
# metadata, or the CI cache/build invocation. With CMAKE=cmake and
# CMAKE_GENERATOR unset, source_recipe_key() therefore remains
# sha256:63e29544455c901f05e37224b52e7f9734480d7c05914083bdcbd335968e6429.
# MAX_PARALLEL_JOBS=16 and the measured 1050 effective-job-second threshold
# (263s at 4 effective jobs; 66s at 16) carry unchanged. Fresh validation is
# still required; this carry does not authorize receipt reuse.

# CARRY TO 6b62f91 (2026-08-11). The calibration carries unchanged across
# 0fd04fe..6b62f91 because every input to the DynamoRIO content-key miss is
# object-identical:
#
#   git rev-parse 0fd04fe:reverie-dbt -> bffe51c6a6e47ebd64ab1e055eed5165f83237a6
#   git rev-parse 6b62f91:reverie-dbt -> bffe51c6a6e47ebd64ab1e055eed5165f83237a6
#   git rev-parse 0fd04fe:reverie-dbt/build.rs -> 209bca718ea9b6d026a26abf5cbd8accbd346068
#   git rev-parse 6b62f91:reverie-dbt/build.rs -> 209bca718ea9b6d026a26abf5cbd8accbd346068
#   git rev-parse 0fd04fe:reverie-dbt/vendor/dynamorio -> de352475846e385002c1e4e54604fa0a7647b2de
#   git rev-parse 6b62f91:reverie-dbt/vendor/dynamorio -> de352475846e385002c1e4e54604fa0a7647b2de
#
# The two intervening commits change only AGENTS.md and wording/test naming in
# reverie-kvm/tests/static_elf.rs. They do not change a crate manifest, runtime
# source, toolchain, DBT build recipe, or vendored DynamoRIO input. Therefore
# source_recipe_key(), MAX_PARALLEL_JOBS=16, and the measured 1050
# effective-job-second threshold (263s at 4 jobs; 66s at 16) carry unchanged.
# Fresh exact-head validation remains required.

#
# CARRY TO c261050 (2026-08-11, third bump of the day). RECIPE IDENTITY MOVES;
# THE BUDGET CARRIES. This is the e159d6c case, not the 108f9ab case:
# reverie-dbt/build.rs CHANGED, so source_recipe_key() necessarily changes, but
# the work it keys has not.
#
#   git rev-parse 5d42e32:reverie-dbt/vendor/dynamorio -> de352475846e385002c1e4e54604fa0a7647b2de
#   git rev-parse c261050:reverie-dbt/vendor/dynamorio -> de352475846e385002c1e4e54604fa0a7647b2de
#                                                         IDENTICAL -- the compiled source is the same tree.
#   git rev-parse 5d42e32:reverie-dbt/build.rs         -> 209bca718ea9b6d026a26abf5cbd8accbd346068
#   git rev-parse c261050:reverie-dbt/build.rs         -> 0ff8ae24b97464044735ba79ea74765ba4ac3ff0
#
# The two commits 5d42e32..c261050 are rrnewton/reverie#440 ("Make SaBRe CMake
# state relocatable" + "Keep the Reverie DBT cleanup lint-clean"). The only
# reverie-dbt change is a let-chain rewrite of StagingDirectory::drop's error
# path -- same control flow, same message, no build behaviour. build_dynamorio()
# still cmake-configures and cmake-builds only vendor/dynamorio, which is
# byte-identical, so the measured MISS cost cannot have moved.
#
# NEW RECIPE IDENTITY, DERIVED NOT GUESSED, exactly as the 3494609 entry above
# requires. source_recipe_key() was reimplemented from the build.rs at c261050
# (hash_tree/hash_file/hash_value/hash_name, usize::to_le_bytes framing, CMAKE
# defaulting to "cmake" and CMAKE_GENERATOR to "<unset>") and FIRST VALIDATED
# AGAINST THE RECORDED VALUE: fed the same on-disk vendored tree together with
# the build.rs at 209bca71 it reproduces
# sha256:63e29544455c901f05e37224b52e7f9734480d7c05914083bdcbd335968e6429
# exactly -- the identity this chain already records. Only then was it used to
# derive the value at c261050:
#   sha256:132d77130980c546c8867fc196d97e664bc4816b1dfa9ea9c18de4a94d109c4d
# A key computed by a reimplementation that could not reproduce the known answer
# would be a number, not evidence; the positive control is what makes this one
# usable. The negative direction was checked too: swapping only build.rs moves
# the key, so the derivation is not insensitive to the input that changed.
#
# NOT confirmed by a real cold build at this pin. The 3494609 entry additionally
# quoted `cargo:warning=DynamoRIO build cache MISS key=...` from an actual build;
# that has not been done here, so this identity rests on the validated
# reimplementation alone. Exact-head validation will exercise the real build.rs
# and is the check that would surface a disagreement.
#
# Budget values (MAX_PARALLEL_JOBS=16, 1050 effective-job-seconds, 263/66
# max-elapsed) carry unchanged. The >=5-clean-Hermit-lane-samples replacement bar
# is still unmet, so nothing is recalibrated here.
#
# CARRY TO bfbe3b14 (2026-08-23), ACROSS EIGHT PIN ADVANCES. The
# calibration carries unchanged and the RECIPE IDENTITY DOES NOT MOVE: this is
# the 0384d673 case, not the c261050 case, because neither source_recipe_key()
# file input differs at any pin between c261050 and bfbe3b14.
#
#   pin        reverie-dbt/vendor/dynamorio                  reverie-dbt/build.rs
#   c261050c   de352475846e385002c1e4e54604fa0a7647b2de      0ff8ae24b9746404...
#   986e17e0   de352475846e385002c1e4e54604fa0a7647b2de      0ff8ae24b9746404...
#   ee6716a6   de352475846e385002c1e4e54604fa0a7647b2de      0ff8ae24b9746404...
#   4f57671d   de352475846e385002c1e4e54604fa0a7647b2de      0ff8ae24b9746404...
#   efb7b08c   de352475846e385002c1e4e54604fa0a7647b2de      0ff8ae24b9746404...
#   268a25b6   de352475846e385002c1e4e54604fa0a7647b2de      0ff8ae24b9746404...
#   af82f1b9   de352475846e385002c1e4e54604fa0a7647b2de      0ff8ae24b9746404...
#   f2e9839e   de352475846e385002c1e4e54604fa0a7647b2de      0ff8ae24b9746404...
#   bfbe3b14   de352475846e385002c1e4e54604fa0a7647b2de      0ff8ae24b9746404...
#
# So the identity stays sha256:132d77130980c546c8867fc196d97e664bc4816b1dfa9ea9c18de4a94d109c4d
# and no derivation is needed; the entry above already validated that value.
#
# WHAT DID CHANGE UNDER reverie-dbt, and why it cannot move this budget:
# 268a25b6 adds +4189/-115 across eight files -- native/client.c (+1291), a new
# src/evidence.rs (+1589), src/launcher.rs, src/lib.rs, and four test fixtures.
# None is a source_recipe_key() input. build_dynamorio() cmake-configures and
# cmake-builds only vendor/dynamorio, which is byte-identical, so the DynamoRIO
# content-key MISS this budget measures cannot have moved. The Rust and C that
# did change is compiled by Cargo under the ordinary workspace budget.
#
# BUILD-RELEVANT ANYWAY, the separate axis the ab44bbf7 entry names: those eight
# reverie-dbt files plus reverie-ptrace/src/tracer.rs at af82f1b9 are compiled by
# Hermit, so this sequence requires REAL revalidation. This carry authorizes
# reusing the budget, never reusing a receipt.
#
# f2e9839e changes only .github/workflows/ci.yml and
# .github/workflows/merge-gate.yml. bfbe3b14 adds the external-scheduler
# protected-evidence FINAL in native/client.c plus its source audit in
# src/evidence.rs. Neither revision changes a recipe input; bfbe3b14 is
# build-relevant, so the real Hermit validation still runs below.
#
# CARRY TO 3798935e (2026-08-24). The budget carries UNCHANGED. Unlike the
# ab44bbf7 carry, this one is NOT "the whole subtree is identical" -- the
# reverie-dbt subtree genuinely differs -- so the argument is made on the two
# recipe inputs specifically, which is the narrower and honest claim:
#
#   git rev-parse bfbe3b14:reverie-dbt/build.rs         -> 0ff8ae24b974
#   git rev-parse 3798935e:reverie-dbt/build.rs         -> 0ff8ae24b974  IDENTICAL
#   git rev-parse bfbe3b14:reverie-dbt/vendor/dynamorio -> de352475846e385002c1e4e54604fa0a7647b2de
#   git rev-parse 3798935e:reverie-dbt/vendor/dynamorio -> de352475846e385002c1e4e54604fa0a7647b2de  IDENTICAL
#   git rev-parse bfbe3b14:reverie-dbt                  -> b693370f9a79f971  (differs)
#   git rev-parse 3798935e:reverie-dbt                  -> d4549166ea7b9ef3  (differs)
#
# The content key is computed at build.rs:472-481 over exactly four inputs:
# hash_tree(vendor/dynamorio), hash_file(build.rs), CMAKE, CMAKE_GENERATOR.
# Both file inputs are byte-identical above, and the two environment inputs
# are host state that a pin move cannot change, so the key is bit-identical at
# both pins. A miss therefore configures and builds the SAME DynamoRIO source
# with the SAME build script, and the measured MISS cost cannot have moved.
# Budget values carry unchanged; the >=5-clean-Hermit-lane-samples replacement
# bar is unmet.
#
# WHAT CHANGED UNDER reverie-dbt, AND WHY IT IS OUTSIDE THE MEASURED REGION:
# native/client.c (+80/-?), src/evidence.rs, src/lib.rs, src/tools.rs, and four
# new process-clone test fixtures/tests. build.rs does not reference client.c,
# and the timed region is bounded at build.rs:542-580 around the cmake
# configure/build/install of the vendored tree alone. So none of these appear
# in either the cache key or the elapsed-seconds measurement.
#
# BUILD-RELEVANT ANYWAY, on the separate axis: reverie-ptrace/src/task.rs
# changes (this is the guest-task panic fix, rrnewton/reverie#480) and
# reverie-dbt sources change, and Hermit compiles both crates. This carry
# authorizes reusing the BUDGET only; it does not authorize reusing a receipt,
# and a fresh exact-head Hermit validation runs for this bump.


export CARGO_BUILD_JOBS=$REVERIE_DBT_RAW_BUILD_JOBS
export THIRD_PARTY_BUILD_JOBS=$REVERIE_DBT_RAW_BUILD_JOBS
export REVERIE_DBT_BUDGET_BOUND_PIN
export REVERIE_DBT_BUILD_JOBS_SOURCE
export REVERIE_DBT_RAW_BUILD_JOBS
export REVERIE_DBT_EFFECTIVE_CPUS_SOURCE
export REVERIE_DBT_EFFECTIVE_CPUS
export REVERIE_DBT_MAX_PARALLEL_JOBS
export REVERIE_DBT_EFFECTIVE_BUILD_JOBS
export REVERIE_DBT_MAX_BUILD_EFFECTIVE_JOB_SECONDS
export REVERIE_DBT_MAX_BUILD_SECONDS
