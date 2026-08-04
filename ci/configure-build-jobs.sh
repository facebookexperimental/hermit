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

# Cargo converts this explicit pool width into build-script NUM_JOBS. Keep the
# nested native-build knob identical so validate.sh cannot widen the pool again.
export CARGO_BUILD_JOBS=$CI_DAG_BUILD_JOBS
export THIRD_PARTY_BUILD_JOBS=$CI_DAG_BUILD_JOBS
