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

# Reverie 025d378's DynamoRIO source-build ratchet accepts an elapsed-seconds
# override, while Hermit's CI contract fixes the requested build width here.
# Carry the width with the threshold by storing job-seconds and deriving the
# elapsed limit with an explicit ceiling:
#
#   max elapsed seconds = ceil(job-second threshold / requested jobs)
#
# PROVENANCE (GitHub portable run 31008044311 at Hermit f21b22ed, requested
# jobs=8, runner affinity=4): three content-key misses measured 115.82s,
# 128.27s, and 131.21s -- one debug build and two concurrent release builds --
# i.e. 926.58, 1026.17, and 1049.65 requested-job-seconds. Reverie's original
# ratchet policy used 2x the slowest of n=3 clean observations; applying that
# same emergency-remediation policy and rounding up gives 2100 job-seconds.
# This narrow downstream threshold is not a capacity-independent estimate or a
# DAG cpu_timeout declaration; replace it when >=5 clean Hermit-lane samples
# support a topology-independent calibration.
CI_DAG_REVERIE_DBI_MAX_BUILD_JOB_SECONDS=${CI_DAG_REVERIE_DBI_MAX_BUILD_JOB_SECONDS:-2100}
if [[ ! $CI_DAG_REVERIE_DBI_MAX_BUILD_JOB_SECONDS =~ ^[1-9][0-9]*$ ]]; then
    echo "configure-build-jobs.sh: CI_DAG_REVERIE_DBI_MAX_BUILD_JOB_SECONDS must be a positive integer" >&2
    return 2
fi
REVERIE_DBI_MAX_BUILD_SECONDS=$((
    (CI_DAG_REVERIE_DBI_MAX_BUILD_JOB_SECONDS + CI_DAG_BUILD_JOBS - 1) /
        CI_DAG_BUILD_JOBS
))
if ((REVERIE_DBI_MAX_BUILD_SECONDS <= 0)); then
    echo "configure-build-jobs.sh: derived REVERIE_DBI_MAX_BUILD_SECONDS must be positive" >&2
    return 2
fi
CI_DAG_EFFECTIVE_CPUS=${CI_DAG_EFFECTIVE_CPUS:-$(nproc)}
if [[ ! $CI_DAG_EFFECTIVE_CPUS =~ ^[1-9][0-9]*$ ]]; then
    echo "configure-build-jobs.sh: CI_DAG_EFFECTIVE_CPUS must be a positive integer" >&2
    return 2
fi

# Cargo converts this explicit pool width into build-script NUM_JOBS. Keep the
# nested native-build knob identical so validate.sh cannot widen the pool again.
export CARGO_BUILD_JOBS=$CI_DAG_BUILD_JOBS
export THIRD_PARTY_BUILD_JOBS=$CI_DAG_BUILD_JOBS
export CI_DAG_EFFECTIVE_CPUS
export CI_DAG_REVERIE_DBI_MAX_BUILD_JOB_SECONDS
export REVERIE_DBI_MAX_BUILD_SECONDS
