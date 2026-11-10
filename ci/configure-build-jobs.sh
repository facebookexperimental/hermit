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
# This file has two explicit source modes. `launcher` sets the shared Cargo
# widths before the DAG runner starts. `core-build-child` accepts the child-local
# width selected by that runner. Both scrub retired portable DBT-budget values
# so ambient settings cannot become core build authority.

CI_DAG_BUILD_JOBS=${CI_DAG_BUILD_JOBS:-8}
if [[ ! $CI_DAG_BUILD_JOBS =~ ^[1-9][0-9]*$ ]]; then
    echo "configure-build-jobs.sh: CI_DAG_BUILD_JOBS must be a positive integer" >&2
    return 2
fi

build_job_context=${1:-}
if [[ $build_job_context == launcher ]]; then
    # Remove even planted ambient values from the retired portable DBT budget
    # so the privileged runner keeps its established environment contract.
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

# First-party children use the runner-admitted Cargo pool. Extension SDK
# calibration belongs to the extension producer.
if [[ $build_job_context == core-build-child ]]; then
    if [[ -n ${CARGO_BUILD_JOBS:-} ]]; then
        CORE_BUILD_RAW_BUILD_JOBS=$CARGO_BUILD_JOBS
        if [[ ${DAGRUN_IN_SCOPE:-} == 1 ]]; then
            CORE_BUILD_JOBS_SOURCE=runner-child-cargo-build-jobs
        else
            CORE_BUILD_JOBS_SOURCE=inherited-launch-cargo-build-jobs
        fi
    else
        CORE_BUILD_RAW_BUILD_JOBS=$CI_DAG_BUILD_JOBS
        CORE_BUILD_JOBS_SOURCE=ci-dag-build-jobs-fallback
    fi
    if [[ ! $CORE_BUILD_RAW_BUILD_JOBS =~ ^[1-9][0-9]*$ ]]; then
        echo "configure-build-jobs.sh: selected raw core build width must be a positive integer" >&2
        return 2
    fi
    if ! CORE_BUILD_EFFECTIVE_CPUS=$(nproc); then
        echo "configure-build-jobs.sh: child nproc observation failed" >&2
        return 2
    fi
    if [[ ! $CORE_BUILD_EFFECTIVE_CPUS =~ ^[1-9][0-9]*$ ]]; then
        echo "configure-build-jobs.sh: child nproc must return a positive integer" >&2
        return 2
    fi
    # No foreign SDK threshold may become first-party build authority.
    unset REVERIE_DBT_BUDGET_BOUND_PIN REVERIE_DBT_BUILD_JOBS_SOURCE
    unset REVERIE_DBT_RAW_BUILD_JOBS REVERIE_DBT_EFFECTIVE_CPUS_SOURCE
    unset REVERIE_DBT_EFFECTIVE_CPUS REVERIE_DBT_MAX_PARALLEL_JOBS
    unset REVERIE_DBT_EFFECTIVE_BUILD_JOBS
    unset REVERIE_DBT_MAX_BUILD_EFFECTIVE_JOB_SECONDS REVERIE_DBT_MAX_BUILD_SECONDS
    export CARGO_BUILD_JOBS=$CORE_BUILD_RAW_BUILD_JOBS
    export THIRD_PARTY_BUILD_JOBS=$CORE_BUILD_RAW_BUILD_JOBS
    export CORE_BUILD_RAW_BUILD_JOBS CORE_BUILD_JOBS_SOURCE CORE_BUILD_EFFECTIVE_CPUS
    return 0
fi

echo "configure-build-jobs.sh: expected source mode launcher or core-build-child (received: ${build_job_context:-<unset>})" >&2
return 2
