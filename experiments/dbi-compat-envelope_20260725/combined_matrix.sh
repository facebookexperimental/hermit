#!/bin/bash
# Authoritative combined DBI compat matrix: native vs ptrace vs dbi(run) vs dbi(--strict --verify L2).
set -u
HERMIT="${HERMIT:-/home/newton/work/dev-hermit/worktrees/slot211/hermit/target/release/hermit}"
OUT="${OUT:-/home/newton/work/dev-hermit/scratch/dbi-compat-20260725}"
WORK=$(mktemp -d); trap 'rm -rf "$WORK"' EXIT
printf "seed input\n" > "$WORK/in.txt"; printf "b\na\nc\na\n" > "$WORK/lines.txt"
CSV="$OUT/results.csv"
echo "program,category,dbi_run_exit,dbi_stdout_eq_native,dbi_eq_ptrace,dbi_L2_verify,note" > "$CSV"
cell() {
  local name="$1" cat="$2"; shift 2
  local nout nexit pout dout dexit l2 out veq peq note=""
  nout=$( "$@" 2>/dev/null ); nexit=$?
  pout=$( timeout --signal=KILL 90 "$HERMIT" run --backend ptrace -- "$@" 2>/dev/null )
  dout=$( timeout --signal=KILL 90 "$HERMIT" run --backend dbi -- "$@" 2>/dev/null ); dexit=$?
  [[ "$dout" == "$nout" ]] && veq=Y || veq=N
  [[ "$dout" == "$pout" ]] && peq=Y || peq=N
  out=$( timeout --signal=KILL 120 "$HERMIT" run --backend dbi --strict --verify -- "$@" 2>&1 )
  if grep -qiE 'Determinism verified' <<<"$out"; then l2=PASS; else l2=FAIL; note="verify:$(grep -iE 'differ|error|panic' <<<"$out"|head -1|cut -c1-50)"; fi
  echo "$name,$cat,$dexit,$veq,$peq,$l2,$note" | tee -a "$CSV"
}
cell echo      trivial     /bin/echo hello
cell true      trivial     /bin/true
cell printf    trivial     /usr/bin/printf 'a=%s\n' x
cell pwd       sysutil     /bin/pwd
cell id        sysutil     /usr/bin/id
cell arch      sysutil     /usr/bin/arch
cell nproc     sysutil     /usr/bin/nproc
cell uname     sysutil     /bin/uname -a
cell date      time        /bin/date -u +%Y
cell seq       textutil    /usr/bin/seq 5
cell cat       textutil    /bin/cat "$WORK/in.txt"
cell head      textutil    /usr/bin/head -1 "$WORK/in.txt"
cell wc        textutil    /usr/bin/wc -w "$WORK/in.txt"
cell sort      textutil    /usr/bin/sort "$WORK/lines.txt"
cell base64    textutil    /usr/bin/base64 "$WORK/in.txt"
cell sha256sum textutil    /usr/bin/sha256sum "$WORK/in.txt"
cell od        textutil    /usr/bin/od -c "$WORK/in.txt"
cell sh_exit   shell       /bin/sh -c 'exit 23'
cell bash_pipe shell       /bin/bash -c 'echo a b c | wc -w'
cell bash_loop shell       /bin/bash -c 'for i in 1 2 3; do echo $i; done'
cell python3   interpreter /usr/bin/python3 -c 'print(2+2)'
cell perl      interpreter /usr/bin/perl -e 'print "perlhi\n"'
