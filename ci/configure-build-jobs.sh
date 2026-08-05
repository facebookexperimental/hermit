#!/usr/bin/env bash
# Shared inner-build width for every Hermit CI DAG launch path.
#
# The outer safe-ci cpu.max is a containment ceiling, not a request for Cargo to
# use every granted core. On the 316-CPU validation host that inference produced
# NUM_JOBS=284 and raced the native linker. K=8 is measurement-backed: on
# 2026-08-04 the pre-collapse build.dbi_release and rr_suite_contract nodes both
# completed at j8 under their cgroup-recorded memory caps. The collapsed fat-build
# nodes declare their independently measured higher width in the DAG manifest.

CI_DAG_BUILD_JOBS=${CI_DAG_BUILD_JOBS:-8}
if [[ ! $CI_DAG_BUILD_JOBS =~ ^[1-9][0-9]*$ ]]; then
    echo "configure-build-jobs.sh: CI_DAG_BUILD_JOBS must be a positive integer" >&2
    return 2
fi

# The safe DAG runner writes its cap-derived CARGO_BUILD_JOBS inside each boxed
# child, after the launch script has run. Preserve that post-wrapper value when
# present; CI_DAG_BUILD_JOBS is only the unboxed/ambient fallback. The DBI budget
# wrapper re-sources this file inside the child so the timeout and Cargo's actual
# NUM_JOBS input are bound to the same raw value.
if [[ -n ${CARGO_BUILD_JOBS:-} ]]; then
    REVERIE_DBI_BUILD_JOBS_SOURCE=cargo-build-jobs
    REVERIE_DBI_RAW_BUILD_JOBS=$CARGO_BUILD_JOBS
else
    REVERIE_DBI_BUILD_JOBS_SOURCE=ci-dag-build-jobs-fallback
    REVERIE_DBI_RAW_BUILD_JOBS=$CI_DAG_BUILD_JOBS
fi
if [[ ! $REVERIE_DBI_RAW_BUILD_JOBS =~ ^[1-9][0-9]*$ ]]; then
    echo "configure-build-jobs.sh: selected raw build width must be a positive integer" >&2
    return 2
fi

# Reverie 025d378's DynamoRIO source-build ratchet accepts an elapsed-seconds
# override. Its build.rs clamps Cargo's NUM_JOBS to 16 before passing it to
# `cmake --parallel`; the operating system cannot execute more jobs at once than
# the process affinity exposes. Carry every condition with the threshold by
# deriving the nominal effective native width first:
#
#   effective native jobs = min(requested jobs, effective CPUs, Reverie clamp)
#   max elapsed seconds = ceil(effective-job-second threshold / effective jobs)
#
# PROVENANCE (GitHub portable run 31008044311 at Hermit f21b22ed, requested
# jobs=8, runner affinity=4): three content-key misses measured 115.82s,
# 128.27s, and 131.21s -- one debug build and two concurrent release builds --
# i.e. 463.28, 513.08, and 524.84 effective-job-seconds at min(8, 4,
# 16)=4. Reverie's original ratchet policy used 2x the slowest of n=3 clean
# observations; applying that emergency-remediation policy and rounding up gives
# 1050 effective-job-seconds. The concurrent release builds embody contention;
# nproc reports affinity/cpuset visibility, not a guaranteed per-build CPU share.
# This is not a DAG cpu_timeout declaration or a topology-independent estimate;
# replace it when >=5 clean Hermit-lane samples support broader calibration.
CI_DAG_EFFECTIVE_CPUS=${CI_DAG_EFFECTIVE_CPUS:-$(nproc)}
if [[ ! $CI_DAG_EFFECTIVE_CPUS =~ ^[1-9][0-9]*$ ]]; then
    echo "configure-build-jobs.sh: CI_DAG_EFFECTIVE_CPUS must be a positive integer" >&2
    return 2
fi

# This duplicated condition is deliberately fail-closed against the exact
# Reverie 025d378 build.rs contract. A pin that changes MAX_PARALLEL_JOBS must
# update this binding and its brackets together.
REVERIE_DBI_PINNED_MAX_PARALLEL_JOBS=16
CI_DAG_REVERIE_DBI_MAX_PARALLEL_JOBS=${CI_DAG_REVERIE_DBI_MAX_PARALLEL_JOBS:-$REVERIE_DBI_PINNED_MAX_PARALLEL_JOBS}
if [[ ! $CI_DAG_REVERIE_DBI_MAX_PARALLEL_JOBS =~ ^[1-9][0-9]*$ ]] ||
    ((CI_DAG_REVERIE_DBI_MAX_PARALLEL_JOBS != REVERIE_DBI_PINNED_MAX_PARALLEL_JOBS)); then
    echo "configure-build-jobs.sh: CI_DAG_REVERIE_DBI_MAX_PARALLEL_JOBS must match pinned Reverie clamp 16" >&2
    return 2
fi

REVERIE_DBI_EFFECTIVE_BUILD_JOBS=$REVERIE_DBI_RAW_BUILD_JOBS
if ((CI_DAG_EFFECTIVE_CPUS < REVERIE_DBI_EFFECTIVE_BUILD_JOBS)); then
    REVERIE_DBI_EFFECTIVE_BUILD_JOBS=$CI_DAG_EFFECTIVE_CPUS
fi
if ((CI_DAG_REVERIE_DBI_MAX_PARALLEL_JOBS < REVERIE_DBI_EFFECTIVE_BUILD_JOBS)); then
    REVERIE_DBI_EFFECTIVE_BUILD_JOBS=$CI_DAG_REVERIE_DBI_MAX_PARALLEL_JOBS
fi

CI_DAG_REVERIE_DBI_MAX_BUILD_EFFECTIVE_JOB_SECONDS=${CI_DAG_REVERIE_DBI_MAX_BUILD_EFFECTIVE_JOB_SECONDS:-1050}
if [[ ! $CI_DAG_REVERIE_DBI_MAX_BUILD_EFFECTIVE_JOB_SECONDS =~ ^[1-9][0-9]*$ ]]; then
    echo "configure-build-jobs.sh: CI_DAG_REVERIE_DBI_MAX_BUILD_EFFECTIVE_JOB_SECONDS must be a positive integer" >&2
    return 2
fi
REVERIE_DBI_MAX_BUILD_SECONDS=$((
    (CI_DAG_REVERIE_DBI_MAX_BUILD_EFFECTIVE_JOB_SECONDS +
        REVERIE_DBI_EFFECTIVE_BUILD_JOBS - 1) /
        REVERIE_DBI_EFFECTIVE_BUILD_JOBS
))

# Cargo converts this explicit pool width into build-script NUM_JOBS. Keep the
# nested native-build knob identical so validate.sh cannot widen the pool again.
export CARGO_BUILD_JOBS=$REVERIE_DBI_RAW_BUILD_JOBS
export THIRD_PARTY_BUILD_JOBS=$REVERIE_DBI_RAW_BUILD_JOBS
export REVERIE_DBI_BUILD_JOBS_SOURCE
export REVERIE_DBI_RAW_BUILD_JOBS
export CI_DAG_EFFECTIVE_CPUS
export CI_DAG_REVERIE_DBI_MAX_PARALLEL_JOBS
export REVERIE_DBI_EFFECTIVE_BUILD_JOBS
export CI_DAG_REVERIE_DBI_MAX_BUILD_EFFECTIVE_JOB_SECONDS
export REVERIE_DBI_MAX_BUILD_SECONDS
