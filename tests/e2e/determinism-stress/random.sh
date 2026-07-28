#!/usr/bin/env bash

set -euo pipefail
# shellcheck source=tests/e2e/determinism-stress/common.sh
source "$(dirname -- "${BASH_SOURCE[0]}")/common.sh"

guest=$(compile_c tests/c/random_sources.c random-sources)
show_native_variation "getrandom plus /dev/random and /dev/urandom" "$guest"
failures=0
if ! verify_guest "random sources" "$guest"; then
  failures=$((failures + 1))
fi

python=${PYTHON:-/usr/bin/python3}
[[ -x $python ]] || fail "Python not found: $python"
python_program='import os, random, secrets; print(random.getrandbits(128)); print(random.SystemRandom().getrandbits(128)); print(secrets.token_hex(16)); print(os.urandom(16).hex())'
show_native_variation "Python PRNGs" "$python" -c "$python_program"
if ! verify_guest "Python PRNGs" "$python" -c "$python_program"; then
  failures=$((failures + 1))
fi

((failures == 0)) || fail "$failures random source(s) failed strict L2"
stress_success "random number generation"
