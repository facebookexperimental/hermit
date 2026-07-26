#!/bin/bash
# L2 determinism sweep: hermit run --backend dbi --strict --verify (bitwise-identical repeat).
set -u
HERMIT="${HERMIT:-/home/newton/work/dev-hermit/worktrees/slot211/hermit/target/release/hermit}"
OUT=/home/newton/work/dev-hermit/scratch/dbi-compat-20260725
WORK=$(mktemp -d); trap 'rm -rf "$WORK"' EXIT
printf "seed input\n" > "$WORK/in.txt"; printf "b\na\nc\na\n" > "$WORK/lines.txt"
TSV="$OUT/l2_results.tsv"; printf "program\tdbi_verify_exit\tverdict\tnote\n" > "$TSV"
l2() {
  local name="$1"; shift
  local out exit note verdict
  out=$( timeout 150 "$HERMIT" run --backend dbi --strict --verify -- "$@" 2>&1 ); exit=$?
  if [[ $exit -eq 124 ]]; then verdict=TIMEOUT; note="150s"
  elif grep -qiE 'Determinism verified|DBI path confirmed|verification .*ok' <<<"$out"; then verdict=L2_PASS; note=""
  elif [[ $exit -eq 0 ]]; then verdict=RUN_OK_NOVERIFYMSG; note="$(echo "$out" | tail -1 | cut -c1-70)"
  else verdict=FAIL; note="$(grep -iE 'differ|error|panic|failed' <<<"$out" | head -1 | cut -c1-80)"
  fi
  printf "%s\t%s\t%s\t%s\n" "$name" "$exit" "$verdict" "$note" | tee -a "$TSV"
}
l2 echo /bin/echo hello
l2 true /bin/true
l2 printf /usr/bin/printf 'a=%s\n' x
l2 pwd /bin/pwd
l2 id /usr/bin/id
l2 arch /usr/bin/arch
l2 nproc /usr/bin/nproc
l2 seq /usr/bin/seq 5
l2 cat /bin/cat "$WORK/in.txt"
l2 head /usr/bin/head -1 "$WORK/in.txt"
l2 wc /usr/bin/wc -w "$WORK/in.txt"
l2 sort /usr/bin/sort "$WORK/lines.txt"
l2 base64 /usr/bin/base64 "$WORK/in.txt"
l2 sha256sum /usr/bin/sha256sum "$WORK/in.txt"
l2 od /usr/bin/od -c "$WORK/in.txt"
l2 uname /bin/uname -a
l2 date /bin/date -u +%Y
l2 sh_echo /bin/sh -c 'echo shellhi'
l2 bash_echo /bin/bash -c 'echo bashhi'
l2 bash_pipe /bin/bash -c 'echo a b c | wc -w'
l2 python3 /usr/bin/python3 -c 'print(2+2)'
l2 perl /usr/bin/perl -e 'print "perlhi\n"'
