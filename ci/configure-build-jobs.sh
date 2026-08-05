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

# The calibration below is valid only for Reverie 025d378. The portable wrapper
# obtains the repository's recorded pin through the canonical checker and
# carries it here; a pin bump cannot silently retain the old clamp or threshold.
if [[ ${REVERIE_DBI_BUDGET_BOUND_PIN:-} != 025d37800d347c32711038bd0a3889e8e4774c2b ]]; then
    echo "configure-build-jobs.sh: DBI budget is not bound to calibrated Reverie 025d37800d347c32711038bd0a3889e8e4774c2b" >&2
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

# Reverie 025d378's DynamoRIO build.rs clamps Cargo NUM_JOBS to 16 before
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
