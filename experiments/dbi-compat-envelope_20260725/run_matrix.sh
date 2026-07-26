#!/bin/bash
# DBI compat matrix harness (ephemeral, scratch). Compares --backend dbi guest
# stdout+exit vs native for a corpus, and probes L2 (--strict --verify).
set -u
HERMIT="${HERMIT:-/home/newton/work/dev-hermit/worktrees/slot211/hermit/target/release/hermit}"
OUT=/home/newton/work/dev-hermit/scratch/dbi-compat-20260725
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
printf "seed input\n" > "$WORK/in.txt"
printf "b\na\nc\na\n" > "$WORK/lines.txt"
TSV="$OUT/results.tsv"
printf "program\tnative_exit\tdbi_exit\tstdout_match\tverdict\tnote\n" > "$TSV"

run_case() {
  local name="$1"; shift
  local native_out native_exit dbi_out dbi_exit match verdict note=""
  native_out=$( "$@" 2>/dev/null ); native_exit=$?
  dbi_out=$( timeout 120 "$HERMIT" run --backend dbi -- "$@" 2>"$WORK/err" ); dbi_exit=$?
  if [[ $dbi_exit -eq 124 ]]; then verdict=TIMEOUT; note="120s timeout"
  elif [[ "$native_out" == "$dbi_out" && $native_exit -eq $dbi_exit ]]; then match=Y; verdict=PASS
  else match=N; verdict=FAIL
    note="nexit=$native_exit dexit=$dbi_exit; $(grep -iE 'error|panic|unsupported|ENOSYS' "$WORK/err" | head -1 | cut -c1-90)"
  fi
  [[ -z "${match:-}" ]] && match=-
  printf "%s\t%s\t%s\t%s\t%s\t%s\n" "$name" "$native_exit" "$dbi_exit" "${match:-}" "$verdict" "$note" | tee -a "$TSV"
  unset match
}

# Tier 1: trivial coreutils
run_case echo /bin/echo hello
run_case true /bin/true
run_case false /bin/false
run_case printf /usr/bin/printf 'a=%s\n' x
run_case pwd /bin/pwd
run_case whoami /usr/bin/whoami
run_case id /usr/bin/id
run_case hostname /bin/hostname
run_case uname /bin/uname -a
run_case arch /usr/bin/arch
run_case nproc /usr/bin/nproc
run_case seq /usr/bin/seq 5
# Tier 2: file/text (single input file)
run_case cat /bin/cat "$WORK/in.txt"
run_case head /usr/bin/head -1 "$WORK/in.txt"
run_case tail /usr/bin/tail -1 "$WORK/in.txt"
run_case wc /usr/bin/wc -w "$WORK/in.txt"
run_case sort /usr/bin/sort "$WORK/lines.txt"
run_case uniq /usr/bin/uniq "$WORK/lines.txt"
run_case cut /usr/bin/cut -c1 "$WORK/in.txt"
run_case tr /usr/bin/tr a-z A-Z "$WORK/in.txt"
run_case rev /usr/bin/rev "$WORK/in.txt"
run_case base64 /usr/bin/base64 "$WORK/in.txt"
run_case sha256sum /usr/bin/sha256sum "$WORK/in.txt"
run_case cksum /usr/bin/cksum "$WORK/in.txt"
run_case od /usr/bin/od -c "$WORK/in.txt"
run_case nl /usr/bin/nl "$WORK/in.txt"
run_case tac /usr/bin/tac "$WORK/lines.txt"
# Tier 3: time/random (known-hazard)
run_case date /bin/date -u +%Y
run_case sleep /bin/sleep 0
# Tier 4: shells & pipelines
run_case sh_echo /bin/sh -c 'echo shellhi'
run_case bash_echo /bin/bash -c 'echo bashhi'
run_case bash_pipe /bin/bash -c 'echo a b c | wc -w'
run_case bash_loop /bin/bash -c 'for i in 1 2 3; do echo $i; done'
# Tier 5: interpreters
run_case python3 /usr/bin/python3 -c 'print(2+2)'
run_case perl /usr/bin/perl -e 'print "perlhi\n"'
