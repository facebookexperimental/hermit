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
if [[ ${REVERIE_DBT_BUDGET_BOUND_PIN:-} != c261050cfd41bec67e31bfd0cf6f56be008d0ebb ]]; then
    echo "configure-build-jobs.sh: DBT budget is not bound to calibrated Reverie c261050cfd41bec67e31bfd0cf6f56be008d0ebb" >&2
    return 2
fi

if [[ -n ${CARGO_BUILD_JOBS:-} ]]; then
    REVERIE_DBT_RAW_BUILD_JOBS=$CARGO_BUILD_JOBS
    if [[ ${SAFE_CI_IN_SCOPE:-} == 1 ]]; then
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
