#!/bin/bash
# hermit-external-builder.sh: run Nix builds under `hermit run` through Nix's
# `external-builders` setting. See README.md in this directory.
#
# Nix runs  <program> <args...> <path of a JSON build description>  once the
# build directory exists and before the build starts. The description names the
# builder, its args and env, the outputs, tmpDir (the host build directory) and
# tmpDirInSandbox (the path the builder expects, "/build").
#
# args:  [--launcher PROGRAM] [--native-fixed-output] HERMIT [HERMIT RUN ARGS...]
#   --launcher PROGRAM     run `PROGRAM HERMIT ...` instead of `HERMIT ...`,
#                          for a site wrapper that bounds the run
#   --native-fixed-output  build fixed-output derivations (source downloads)
#                          without Hermit; Nix checks their output hash anyway,
#                          and they need the network
set -uo pipefail

launcher=()
native_fod=false
while [ "$#" -gt 1 ]; do
  case "$1" in
    --launcher) launcher=("$2"); shift 2 ;;
    --native-fixed-output) native_fod=true; shift ;;
    *) break ;;
  esac
done
if [ "$#" -lt 2 ]; then
  echo "usage: $0 [--launcher PROGRAM] [--native-fixed-output] HERMIT [HERMIT RUN ARGS...] DESCRIPTION" >&2
  exit 2
fi
desc="${@: -1}"
hermit_bin="$1"
hermit_args=("${@:2:$#-2}")
jq="${JQ:-jq}"
canonical=/tmp/build

tmp_dir="$("$jq" -r .tmpDir "$desc")" || exit 1
in_sandbox="$("$jq" -r .tmpDirInSandbox "$desc")" || exit 1
fixed_output="$("$jq" -r '.env | has("outputHash")' "$desc")" || exit 1
mapfile -d '' guest_argv < <("$jq" -j '.builder, (.args[]) | "\(.)\u0000"' "$desc")

if [ "$native_fod" = true ] && [ "$fixed_output" = true ]; then
  mapfile -d '' build_env < <("$jq" -j --arg from "$in_sandbox" --arg to "$tmp_dir" '
    .env | to_entries[] | .value as $v
    | (if $v == $from or ($v | startswith($from + "/")) then $to + $v[($from | length):] else $v end) as $w
    | "\(.key)=\($w)\u0000"' "$desc")
  cd "$tmp_dir" || exit 1
  # Nix treats the build as started once it reads a line holding only \2.
  printf '\2\n' >&2
  exec /usr/bin/env -i "${build_env[@]}" "${guest_argv[@]}"
fi

# The guest sees the build directory at /tmp/build on Hermit's private /tmp.
# The host directory name is random, and its length alone changes how many
# branches the builder executes and so Hermit's virtual clock.
mapfile -d '' guest_env < <("$jq" -j --arg from "$in_sandbox" --arg to "$canonical" '
  .env | to_entries[] | .value as $v
  | (if $v == $from or ($v | startswith($from + "/")) then $to + $v[($from | length):] else $v end) as $w
  | "\(.key)=\($w)\u0000"' "$desc")

# Nix starts this program in the host build directory, whose random name would
# reach Hermit through its own working directory.
cd / || exit 1
printf '\2\n' >&2
# Hermit itself starts with a fixed minimal environment, so the first guest
# process inherits nothing build-specific; `env -i` inside the guest installs
# exactly the derivation's environment.
# The guest sees the host's /etc, so glibc would apply the host's
# /etc/localtime; Nix's sandbox has none and gives UTC. TZ=UTC comes first so
# that a derivation which sets TZ itself still wins
# (https://github.com/rrnewton/hermit/issues/3649).
exec /usr/bin/env -i PATH=/usr/bin:/bin HOME=/homeless-shelter TMPDIR=/tmp \
  "${launcher[@]}" "$hermit_bin" "${hermit_args[@]}" \
  --bind "$tmp_dir:$canonical" --workdir "$canonical" -- \
  /usr/bin/env -i TZ=UTC "${guest_env[@]}" "${guest_argv[@]}"
