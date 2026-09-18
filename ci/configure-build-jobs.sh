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
    # independently measured higher width in the DAG manifest" -- build.workspace and
    # build.runtime_release both declare preferred_inner_jobs=32. That declaration has
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
if [[ ${REVERIE_DBT_BUDGET_BOUND_PIN:-} != f97b7be1de4e2ef10ecc24cee5d8cc47f2fd254f ]]; then
    echo "configure-build-jobs.sh: DECLINED (no_result, exit 75): DBT budget is not bound to Reverie f97b7be1de4e2ef10ecc24cee5d8cc47f2fd254f (bound pin: ${REVERIE_DBT_BUDGET_BOUND_PIN:-<unset>})" >&2
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
# registers the root thread again. Before it, every `hermit run --backend kvm`
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
