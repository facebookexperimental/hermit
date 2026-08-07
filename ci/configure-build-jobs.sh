#!/usr/bin/env bash
# Shared inner-build width for every Hermit CI DAG launch path.
#
# The outer safe-ci cpu.max is a containment ceiling, not a request for Cargo to
# use every granted core. On the 316-CPU validation host that inference produced
# NUM_JOBS=284 and raced the native linker. K=8 is measurement-backed: on
# 2026-08-04 the pre-collapse build.dbi_release and rr_suite_contract nodes both
# completed at j8 under their cgroup-recorded memory caps. The collapsed fat-build
# nodes declare their independently measured higher width in the DAG manifest.
#
# This file has two explicit source modes. `launcher` preserves the historical
# shared Cargo widths and strips every portable DBI-budget variable before the
# DAG runner starts. `reverie-dbi-budget-child` is called only by the portable
# DBI wrapper, after safe-ci has entered the child and selected any child-local
# Cargo width.

CI_DAG_BUILD_JOBS=${CI_DAG_BUILD_JOBS:-8}
if [[ ! $CI_DAG_BUILD_JOBS =~ ^[1-9][0-9]*$ ]]; then
    echo "configure-build-jobs.sh: CI_DAG_BUILD_JOBS must be a positive integer" >&2
    return 2
fi

build_job_context=${1:-}
if [[ $build_job_context == launcher ]]; then
    # These variables are meaningful only in the two portable DBI build
    # children. Remove even planted ambient values so the privileged runner's
    # environment remains identical to the pre-budget launcher contract.
    unset REVERIE_DBI_BUDGET_BOUND_PIN
    unset REVERIE_DBI_BUILD_JOBS_SOURCE
    unset REVERIE_DBI_RAW_BUILD_JOBS
    unset REVERIE_DBI_EFFECTIVE_CPUS_SOURCE
    unset REVERIE_DBI_EFFECTIVE_CPUS
    unset REVERIE_DBI_MAX_PARALLEL_JOBS
    unset REVERIE_DBI_EFFECTIVE_BUILD_JOBS
    unset REVERIE_DBI_MAX_BUILD_EFFECTIVE_JOB_SECONDS
    unset REVERIE_DBI_MAX_BUILD_SECONDS

    # Retire the previous launcher-carried derivation names fail-closed too.
    unset CI_DAG_LAUNCH_WIDTH_BOUND
    unset CI_DAG_LAUNCH_BUILD_JOBS_SOURCE
    unset CI_DAG_LAUNCH_RAW_BUILD_JOBS
    unset CI_DAG_EFFECTIVE_CPUS
    unset CI_DAG_REVERIE_DBI_MAX_PARALLEL_JOBS
    unset CI_DAG_REVERIE_DBI_MAX_BUILD_JOB_SECONDS
    unset CI_DAG_REVERIE_DBI_MAX_BUILD_EFFECTIVE_JOB_SECONDS
    unset REVERIE_DBI_PINNED_MAX_PARALLEL_JOBS
    unset REVERIE_DBI_BUDGET_CHILD

    # Cargo converts this explicit pool width into build-script NUM_JOBS. Keep
    # the nested native-build knob identical so validate.sh cannot widen it.
    export CARGO_BUILD_JOBS=$CI_DAG_BUILD_JOBS
    export THIRD_PARTY_BUILD_JOBS=$CI_DAG_BUILD_JOBS
    return 0
fi

if [[ $build_job_context != reverie-dbi-budget-child ]]; then
    echo "configure-build-jobs.sh: expected source mode launcher or reverie-dbi-budget-child" >&2
    return 2
fi

# fc97 briefly exported this unconditioned threshold before the budget was
# normalized to effective-job-seconds. A direct wrapper invocation must not
# carry that retired authority into Cargo; normal launchers scrub it above.
if [[ -v CI_DAG_REVERIE_DBI_MAX_BUILD_JOB_SECONDS ]]; then
    echo "configure-build-jobs.sh: retired CI_DAG_REVERIE_DBI_MAX_BUILD_JOB_SECONDS is not accepted in a DBI budget child" >&2
    return 2
fi

# The calibration below is valid only for Reverie 0ae0c01. The portable wrapper
# obtains the repository's recorded pin through the canonical checker and
# carries it here; a pin bump cannot silently retain the old clamp or threshold.
if [[ ${REVERIE_DBI_BUDGET_BOUND_PIN:-} != 0ae0c01b5e4c9fbf85c97adc66c2740f280727df ]]; then
    echo "configure-build-jobs.sh: DBI budget is not bound to calibrated Reverie 0ae0c01b5e4c9fbf85c97adc66c2740f280727df" >&2
    return 2
fi

if [[ -n ${CARGO_BUILD_JOBS:-} ]]; then
    REVERIE_DBI_RAW_BUILD_JOBS=$CARGO_BUILD_JOBS
    if [[ ${SAFE_CI_IN_SCOPE:-} == 1 ]]; then
        REVERIE_DBI_BUILD_JOBS_SOURCE=runner-child-cargo-build-jobs
    else
        REVERIE_DBI_BUILD_JOBS_SOURCE=inherited-launch-cargo-build-jobs
    fi
else
    REVERIE_DBI_RAW_BUILD_JOBS=$CI_DAG_BUILD_JOBS
    REVERIE_DBI_BUILD_JOBS_SOURCE=ci-dag-build-jobs-fallback
fi
if [[ ! $REVERIE_DBI_RAW_BUILD_JOBS =~ ^[1-9][0-9]*$ ]]; then
    echo "configure-build-jobs.sh: selected raw build width must be a positive integer" >&2
    return 2
fi

# Observe affinity/cpuset visibility in this child, after safe-ci has applied
# its containment. A launcher observation would be only a correlated proxy for
# the CPUs available to the native build.
if ! REVERIE_DBI_EFFECTIVE_CPUS=$(nproc); then
    echo "configure-build-jobs.sh: child nproc observation failed" >&2
    return 2
fi
REVERIE_DBI_EFFECTIVE_CPUS_SOURCE=child-nproc
if [[ ! $REVERIE_DBI_EFFECTIVE_CPUS =~ ^[1-9][0-9]*$ ]]; then
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
# governs exactly one quantity: the elapsed time reverie-dbi/build.rs reports
# for a DynamoRIO content-key MISS. That build's inputs are hashed by
# source_recipe_key() over {reverie-dbi/vendor/dynamorio, reverie-dbi/build.rs,
# $CMAKE, $CMAKE_GENERATOR} -- host-invariant while CMAKE/CMAKE_GENERATOR are
# unset -- and six cold builds (three per pin, interleaved on one host,
# taskset 4 CPUs, CARGO_BUILD_JOBS=4) all printed the SAME recipe key
# sha256:19123c88d87a4cd9e8b0efdda7265c7682e8907fe6bbf8e0bd6fcb92fbfa85e4.
# Elapsed at 9470712: 39.80s / 39.23s / 39.52s (159.20 / 156.92 / 158.08
# effective-job-seconds); at 025d378: 38.10s / 39.58s / 41.01s (152.40 /
# 158.32 / 164.04). The new pin's slowest sample is 3% faster than the old
# pin's slowest and the whole set spans 7.1%, so the pin move causes no
# throughput change. Corroborating Git evidence: 025d378..9470712 touches only
# reverie-ptrace/src/{error,task,tracer}.rs; the reverie-dbi subtree
# (c38c979057f9fe3e4d46772c1fddd05a71db4bf9) and third-party/
# (fb49c0ba7a9abd48a4ea662bf20e08246c81fc5a) are identical at both pins, and
# MAX_PARALLEL_JOBS is still 16.
#
# CARRY TO e159d6c (2026-08-06). The only 9470712..e159d6c change is a
# hostname-neutral wording edit in reverie-dbi/build.rs. The vendored
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
# reverie-dbi/build.rs, its vendored DynamoRIO tree, build commands, and the
# MAX_PARALLEL_JOBS=16 clamp are byte-identical, so source_recipe_key() remains
# sha256:76403e8e76b128119be4a7192893b7ec3084aeb85f4bd0377198a538d94b2a1d.
# CI_MAX_BUILD_JOB_SECONDS=572 and the measured hosted-runner budget therefore
# carry without changing the derivation.
#
# CARRY TO dd3c178 (2026-08-06). The only 6a6b4ec..dd3c178 change adds
# reverie-kvm sendmsg/recvmsg ancillary-data translation and KVM tests.
# reverie-dbi/build.rs, its vendored DynamoRIO tree, build commands, and the
# MAX_PARALLEL_JOBS=16 clamp remain byte-identical. The DBI recipe identity
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
# The DBI inputs are byte-identical by git object identity at both pins --
# reverie-dbi/build.rs 9e35e1b699b7, reverie-dbi/vendor/dynamorio de352475846e,
# third-party fb49c0ba7a9a, and the whole reverie-dbi subtree eb284556d2df --
# so source_recipe_key() is unchanged at
# sha256:76403e8e76b128119be4a7192893b7ec3084aeb85f4bd0377198a538d94b2a1d and
# the MAX_PARALLEL_JOBS=16 clamp still applies. The hosted-runner budget
# therefore carries without re-derivation. This carry is evidenced by tree
# identity rather than by a fresh timing run, exactly as the 6a6b4ec and
# dd3c178 carries above: no DBI build input changed, so there is nothing for a
# new timing sample to measure.
#
# Those 2026-08-05 samples deliberately do NOT replace 1050. They come from a
# development host whose cores finish the identical work ~3.3x faster than the
# GitHub portable runner this budget governs; 2x their slowest would give 319
# effective-job-seconds and would fail the portable lane on its first genuine
# cold miss. The replacement bar stated above -- >=5 clean Hermit-lane samples
# -- is unchanged and still unmet.
REVERIE_DBI_MAX_PARALLEL_JOBS=16
REVERIE_DBI_MAX_BUILD_EFFECTIVE_JOB_SECONDS=1050
REVERIE_DBI_EFFECTIVE_BUILD_JOBS=$REVERIE_DBI_RAW_BUILD_JOBS
if ((REVERIE_DBI_EFFECTIVE_CPUS < REVERIE_DBI_EFFECTIVE_BUILD_JOBS)); then
    REVERIE_DBI_EFFECTIVE_BUILD_JOBS=$REVERIE_DBI_EFFECTIVE_CPUS
fi
if ((REVERIE_DBI_MAX_PARALLEL_JOBS < REVERIE_DBI_EFFECTIVE_BUILD_JOBS)); then
    REVERIE_DBI_EFFECTIVE_BUILD_JOBS=$REVERIE_DBI_MAX_PARALLEL_JOBS
fi
REVERIE_DBI_MAX_BUILD_SECONDS=$((
    (REVERIE_DBI_MAX_BUILD_EFFECTIVE_JOB_SECONDS +
        REVERIE_DBI_EFFECTIVE_BUILD_JOBS - 1) /
        REVERIE_DBI_EFFECTIVE_BUILD_JOBS
))

export CARGO_BUILD_JOBS=$REVERIE_DBI_RAW_BUILD_JOBS
export THIRD_PARTY_BUILD_JOBS=$REVERIE_DBI_RAW_BUILD_JOBS
export REVERIE_DBI_BUDGET_BOUND_PIN
export REVERIE_DBI_BUILD_JOBS_SOURCE
export REVERIE_DBI_RAW_BUILD_JOBS
export REVERIE_DBI_EFFECTIVE_CPUS_SOURCE
export REVERIE_DBI_EFFECTIVE_CPUS
export REVERIE_DBI_MAX_PARALLEL_JOBS
export REVERIE_DBI_EFFECTIVE_BUILD_JOBS
export REVERIE_DBI_MAX_BUILD_EFFECTIVE_JOB_SECONDS
export REVERIE_DBI_MAX_BUILD_SECONDS
