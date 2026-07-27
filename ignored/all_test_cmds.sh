# Unified Hermit end-to-end command inventory.
#
# Sources: hermit-cli/tests/*.rs, validate.sh, ci/dag/hosted.json,
# and ci/dag/hardware.json at base 6cd2b1d4716d165fed5c46bbeadeceebde7c9754.
# This file is intentionally non-executable; it is an audit/runbook, not a suite.
#
# Annotations:
#   [verify]       hermit run --verify (normally with --strict)
#   [record/replay] hermit record/replay, often record start --verify
#   [both]         the same validate.sh label is covered by both paths
#   [both: ...]    command-specific verify/record/replay arm of that label
#   [run]          repeated Hermit runs without built-in verification
#   [both/mixed]   a Cargo/CI driver contains more than one of those modes
#
# Runtime-created paths use descriptive shell variables such as $HOME_DIR,
# $RECORDING_DIR, $CARGO_TARGET_TMPDIR, and $HERMIT_LEVELDB_BUILD_DIR.
# validate.sh functional probes deliberately diverge: strict verification runs
# REAL_COMPAT_WORKLOAD, while selected R/R rows record the listed command.
# At this source snapshot, the rr control flow reaches 142/144 selected labels:
# tcl and dc are selected in RR_COMPAT_PASSING_LABELS but guarded to strict/SaBRe.

# System utilities
hermit --log=off run --strict --verify "--env=HOME=$HOME_DIR" "--env=XDG_CONFIG_HOME=$HOME_DIR/.config" -- /usr/bin/cat /etc/hostname # [verify] hermit-cli/tests/command_strict_verify.rs case=cat
hermit --log=off run --strict --verify "--env=HOME=$HOME_DIR" "--env=XDG_CONFIG_HOME=$HOME_DIR/.config" -- /usr/bin/wc -l /etc/passwd # [verify] hermit-cli/tests/command_strict_verify.rs case=wc
hermit --log=off run --strict --verify "--env=HOME=$HOME_DIR" "--env=XDG_CONFIG_HOME=$HOME_DIR/.config" -- /usr/bin/head -n 3 /etc/passwd # [verify] hermit-cli/tests/command_strict_verify.rs case=head
printf %b 'gamma\nalpha\nbeta\n' | hermit --log=off run --strict --verify "--env=HOME=$HOME_DIR" "--env=XDG_CONFIG_HOME=$HOME_DIR/.config" -- /usr/bin/sort # [verify] hermit-cli/tests/command_strict_verify.rs case=sort
hermit --log=off run --strict --verify "--env=HOME=$HOME_DIR" "--env=XDG_CONFIG_HOME=$HOME_DIR/.config" -- /usr/bin/env -i HERMIT_COMMAND_COMPAT=1 # [verify] hermit-cli/tests/command_strict_verify.rs case=env
hermit --log=off run --strict --verify "--env=HOME=$HOME_DIR" "--env=XDG_CONFIG_HOME=$HOME_DIR/.config" -- /usr/bin/date -u +%s # [verify] hermit-cli/tests/command_strict_verify.rs case=date
hermit --log=off run --strict --verify "--env=HOME=$HOME_DIR" "--env=XDG_CONFIG_HOME=$HOME_DIR/.config" -- /usr/bin/id -u # [verify] hermit-cli/tests/command_strict_verify.rs case=id
hermit --log=off run --strict --verify "--env=HOME=$HOME_DIR" "--env=XDG_CONFIG_HOME=$HOME_DIR/.config" -- /usr/bin/hostname # [verify] hermit-cli/tests/command_strict_verify.rs case=hostname
hermit --log=off run --strict --verify "--env=HOME=$HOME_DIR" "--env=XDG_CONFIG_HOME=$HOME_DIR/.config" -- /usr/bin/uname -a # [verify] hermit-cli/tests/command_strict_verify.rs case=uname
printf %b 'hello hermit\n' | hermit --log=off run --strict --verify "--env=HOME=$HOME_DIR" "--env=XDG_CONFIG_HOME=$HOME_DIR/.config" -- /usr/bin/tr a-z A-Z # [verify] hermit-cli/tests/command_strict_verify.rs case=tr
hermit --log=off run --strict --verify "--env=HOME=$HOME_DIR" "--env=XDG_CONFIG_HOME=$HOME_DIR/.config" -- /usr/bin/cut -d: -f1 /etc/passwd # [verify] hermit-cli/tests/command_strict_verify.rs case=cut
printf %b 'tee-through-hermit\n' | hermit --log=off run --strict --verify "--env=HOME=$HOME_DIR" "--env=XDG_CONFIG_HOME=$HOME_DIR/.config" -- /usr/bin/tee /dev/null # [verify] hermit-cli/tests/command_strict_verify.rs case=tee
hermit --log=off run --strict --verify "--env=HOME=$HOME_DIR" "--env=XDG_CONFIG_HOME=$HOME_DIR/.config" -- /usr/bin/diff /etc/hostname /etc/hostname # [verify] hermit-cli/tests/command_strict_verify.rs case=diff
hermit --log=off run --strict --verify "--env=HOME=$HOME_DIR" "--env=XDG_CONFIG_HOME=$HOME_DIR/.config" -- /usr/bin/grep -m 1 root /etc/passwd # [verify] hermit-cli/tests/command_strict_verify.rs case=grep
hermit --log=off run --strict --verify "--env=HOME=$HOME_DIR" "--env=XDG_CONFIG_HOME=$HOME_DIR/.config" -- /usr/bin/sed -n 1,3p /etc/passwd # [verify] hermit-cli/tests/command_strict_verify.rs case=sed
hermit --log=off run --strict --verify "--env=HOME=$HOME_DIR" "--env=XDG_CONFIG_HOME=$HOME_DIR/.config" -- /usr/bin/find /etc -maxdepth 1 -type f -name hostname -print # [verify] hermit-cli/tests/command_strict_verify.rs case=find
printf %b 'one two three\n' | hermit --log=off run --strict --verify "--env=HOME=$HOME_DIR" "--env=XDG_CONFIG_HOME=$HOME_DIR/.config" -- /usr/bin/xargs echo # [verify] hermit-cli/tests/command_strict_verify.rs case=xargs
hermit --log=off run --strict --verify "--env=HOME=$HOME_DIR" "--env=XDG_CONFIG_HOME=$HOME_DIR/.config" -- /usr/bin/basename /tmp/hermit-example.txt .txt # [verify] hermit-cli/tests/command_strict_verify.rs case=basename
hermit --log=off run --strict --verify "--env=HOME=$HOME_DIR" "--env=XDG_CONFIG_HOME=$HOME_DIR/.config" -- /usr/bin/dirname /tmp/hermit-example.txt # [verify] hermit-cli/tests/command_strict_verify.rs case=dirname
hermit --log=off run --strict --verify "--env=HOME=$HOME_DIR" "--env=XDG_CONFIG_HOME=$HOME_DIR/.config" -- /usr/bin/realpath /etc/../etc/passwd # [verify] hermit-cli/tests/command_strict_verify.rs case=realpath
hermit --log=off run --strict --verify "--env=HOME=$HOME_DIR" "--env=XDG_CONFIG_HOME=$HOME_DIR/.config" -- /usr/bin/readlink /proc/self/ns/mnt # [verify] hermit-cli/tests/command_strict_verify.rs case=readlink-mnt
hermit --log=off run --strict --verify "--env=HOME=$HOME_DIR" "--env=XDG_CONFIG_HOME=$HOME_DIR/.config" -- /usr/bin/readlink /proc/self/exe # [verify] hermit-cli/tests/command_strict_verify.rs case=readlink-exe
hermit --log=off run --strict --verify "--env=HOME=$HOME_DIR" "--env=XDG_CONFIG_HOME=$HOME_DIR/.config" -- /usr/bin/md5sum /etc/hostname # [verify] hermit-cli/tests/command_strict_verify.rs case=md5sum
hermit --log=off run --strict --verify "--env=HOME=$HOME_DIR" "--env=XDG_CONFIG_HOME=$HOME_DIR/.config" -- /usr/bin/sha256sum /etc/hostname # [verify] hermit-cli/tests/command_strict_verify.rs case=sha256sum
hermit --log=off run --strict --verify "--env=HOME=$HOME_DIR" "--env=XDG_CONFIG_HOME=$HOME_DIR/.config" -- /usr/bin/du -b /etc/hostname # [verify] hermit-cli/tests/command_strict_verify.rs case=du
hermit --log=off run --strict --verify "--env=HOME=$HOME_DIR" "--env=XDG_CONFIG_HOME=$HOME_DIR/.config" -- /usr/bin/awk 'BEGIN { for (i = 1; i <= 10; ++i) sum += i; print sum }' # [verify] hermit-cli/tests/command_strict_verify.rs case=awk
hermit --log=off run --strict --verify "--env=HOME=$HOME_DIR" "--env=XDG_CONFIG_HOME=$HOME_DIR/.config" -- /usr/bin/whoami # [verify] hermit-cli/tests/command_strict_verify.rs case=whoami
hermit --log=off run --strict --verify "--env=HOME=$HOME_DIR" "--env=XDG_CONFIG_HOME=$HOME_DIR/.config" -- /usr/bin/groups # [verify] hermit-cli/tests/command_strict_verify.rs case=groups
hermit --log=off run --strict --verify "--env=HOME=$HOME_DIR" "--env=XDG_CONFIG_HOME=$HOME_DIR/.config" -- /usr/bin/ps aux # [verify] hermit-cli/tests/command_strict_verify.rs case=ps
hermit --log=off run --strict --verify "--env=HOME=$HOME_DIR" "--env=XDG_CONFIG_HOME=$HOME_DIR/.config" -- /usr/bin/free -m # [verify] hermit-cli/tests/command_strict_verify.rs case=free
hermit --log=off run --strict --verify "--env=HOME=$HOME_DIR" "--env=XDG_CONFIG_HOME=$HOME_DIR/.config" -- /usr/bin/vmstat -s # [verify] hermit-cli/tests/command_strict_verify.rs case=vmstat
hermit --log=off run --strict --verify "--env=HOME=$HOME_DIR" "--env=XDG_CONFIG_HOME=$HOME_DIR/.config" -- /usr/bin/top -b -n 1 -p 1 -w 80 # [verify] hermit-cli/tests/command_strict_verify.rs case=top
hermit --log=off run --strict --verify "--env=HOME=$HOME_DIR" "--env=XDG_CONFIG_HOME=$HOME_DIR/.config" -- /usr/bin/iostat -d -x 1 1 # [verify] hermit-cli/tests/command_strict_verify.rs case=iostat
hermit --log=off run --strict --verify "--env=HOME=$HOME_DIR" "--env=XDG_CONFIG_HOME=$HOME_DIR/.config" -- /usr/bin/vmstat -d 1 2 # [verify] hermit-cli/tests/command_strict_verify.rs case=vmstat-disk
hermit --log=off run --strict --verify "--env=HOME=$HOME_DIR" "--env=XDG_CONFIG_HOME=$HOME_DIR/.config" -- /usr/bin/pidstat -d -p 1 1 1 # [verify] hermit-cli/tests/command_strict_verify.rs case=pidstat
hermit --log=off run --strict --verify "--env=HOME=$HOME_DIR" "--env=XDG_CONFIG_HOME=$HOME_DIR/.config" -- /usr/bin/findmnt --kernel --list --output TARGET,SOURCE,FSTYPE,OPTIONS # [verify] hermit-cli/tests/command_strict_verify.rs case=findmnt
hermit --log=off run --strict --verify "--env=HOME=$HOME_DIR" "--env=XDG_CONFIG_HOME=$HOME_DIR/.config" -- /usr/sbin/sysctl kernel.random.uuid # [verify] hermit-cli/tests/command_strict_verify.rs case=sysctl
hermit --log=off run --strict --verify "--env=HOME=$HOME_DIR" "--env=XDG_CONFIG_HOME=$HOME_DIR/.config" -- /usr/bin/sar -v 1 1 # [verify] hermit-cli/tests/command_strict_verify.rs case=sar
hermit --log=off run --strict --verify "--env=HOME=$HOME_DIR" "--env=XDG_CONFIG_HOME=$HOME_DIR/.config" -- /usr/bin/ionice -p 0 # [verify] hermit-cli/tests/command_strict_verify.rs case=ionice
hermit --log=off run --strict --verify "--env=HOME=$HOME_DIR" "--env=XDG_CONFIG_HOME=$HOME_DIR/.config" -- /usr/bin/lsirq --noheadings --output IRQ,TOTAL,NAME # [verify] hermit-cli/tests/command_strict_verify.rs case=lsirq
hermit --log=off run --strict --verify "--env=HOME=$HOME_DIR" "--env=XDG_CONFIG_HOME=$HOME_DIR/.config" -- /usr/bin/mpstat -I SCPU 1 1 # [verify] hermit-cli/tests/command_strict_verify.rs case=mpstat
hermit --log=off run --strict --verify "--env=HOME=$HOME_DIR" "--env=XDG_CONFIG_HOME=$HOME_DIR/.config" -- /usr/sbin/lsmod # [verify] hermit-cli/tests/command_strict_verify.rs case=lsmod
hermit --log=off run --strict --verify "--env=HOME=$HOME_DIR" "--env=XDG_CONFIG_HOME=$HOME_DIR/.config" -- /usr/bin/numastat # [verify] hermit-cli/tests/command_strict_verify.rs case=numastat
hermit --log=off run --strict --verify "--env=HOME=$HOME_DIR" "--env=XDG_CONFIG_HOME=$HOME_DIR/.config" -- /usr/bin/numactl --hardware # [verify] hermit-cli/tests/command_strict_verify.rs case=numactl
hermit --log=off run --strict --verify "--env=HOME=$HOME_DIR" "--env=XDG_CONFIG_HOME=$HOME_DIR/.config" -- /usr/bin/sensors --version # [verify] hermit-cli/tests/command_strict_verify.rs case=sensors
hermit run --backend liteinst --no-namespace --strict --verify -- /bin/true # [verify] validate.sh:1320 label=true
hermit run --backend liteinst --no-namespace --strict --verify -- /bin/echo hermit-compat # [verify] validate.sh:1321 label=echo
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/seq 10 # [verify] validate.sh:1322 label=seq
hermit run --backend liteinst --no-namespace --strict --verify -- /bin/cat README.md # [verify] validate.sh:1323 label=cat
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/wc -c README.md # [verify] validate.sh:1324 label=wc
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/head -n 3 README.md # [verify] validate.sh:1325 label=head
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/base64 README.md # [verify] validate.sh:1326 label=base64
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/id -u # [verify] validate.sh:1327 label=id
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/uname -sr # [verify] validate.sh:1328 label=uname
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/printf '%s=%d\n' hermit 42 # [verify] validate.sh:1329 label=printf
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/stat -c '%n %s %f' README.md # [verify] validate.sh:1330 label=stat
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/sha256sum README.md # [verify] validate.sh:1331 label=sha256sum
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/arch # [verify] validate.sh:1332 label=arch
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/factor 42 # [verify] validate.sh:1333 label=factor
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/expr 2 + 2 # [verify] validate.sh:1334 label=expr
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/hostname # [verify] validate.sh:1335 label=hostname
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/awk 'BEGIN { print 42 }' # [verify] validate.sh:1338 label=awk
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/sort README.md # [verify] validate.sh:1340 label=sort
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/file /bin/sh # [verify] validate.sh:1341 label=file
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/readlink -f README.md # [verify] validate.sh:1342 label=readlink
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/du -sk README.md # [verify] validate.sh:1343 label=du
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/nproc # [verify] validate.sh:1344 label=nproc
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/make --version # [verify] validate.sh:1347 label=make
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/basename /tmp/foo.txt .txt # [verify] validate.sh:1349 label=basename
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/dirname /tmp/foo.txt # [verify] validate.sh:1350 label=dirname
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/pwd # [verify] validate.sh:1351 label=pwd
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/realpath README.md # [verify] validate.sh:1352 label=realpath
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/md5sum README.md # [verify] validate.sh:1353 label=md5sum
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/sha1sum README.md # [verify] validate.sh:1354 label=sha1sum
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/cut -c 1-20 README.md # [verify] validate.sh:1355 label=cut
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/uniq README.md # [verify] validate.sh:1356 label=uniq
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/paste README.md README.md # [verify] validate.sh:1357 label=paste
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/nl -ba README.md # [verify] validate.sh:1358 label=nl
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/ls -ld README.md # [verify] validate.sh:1359 label=ls
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/date -u +%s # [verify] validate.sh:1360 label=date
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/grep -n Hermit README.md # [verify] validate.sh:1361 label=grep
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/sed -n 1,20p README.md # [verify] validate.sh:1362 label=sed
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/find hermit-cli -maxdepth 1 -type f -printf '%f\n' # [verify] validate.sh:1363 label=find
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/ldd --version # [verify] validate.sh:1368 label=ldd
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/lscpu # [verify] validate.sh:1369 label=lscpu
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/uptime -p # [verify] validate.sh:1370 label=uptime
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/base32 README.md # [verify] validate.sh:1371 label=base32
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/sha224sum README.md # [verify] validate.sh:1372 label=sha224sum
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/sha384sum README.md # [verify] validate.sh:1373 label=sha384sum
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/sha512sum README.md # [verify] validate.sh:1374 label=sha512sum
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/b2sum README.md # [verify] validate.sh:1375 label=b2sum
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/cksum README.md # [verify] validate.sh:1376 label=cksum
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/sum README.md # [verify] validate.sh:1377 label=sum
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/fold -w 40 README.md # [verify] validate.sh:1378 label=fold
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/fmt -w 60 README.md # [verify] validate.sh:1379 label=fmt
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/tac README.md # [verify] validate.sh:1380 label=tac
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/rev README.md # [verify] validate.sh:1381 label=rev
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/od -An -tx1 -N32 README.md # [verify] validate.sh:1382 label=od
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/xxd -l 32 README.md # [verify] validate.sh:1383 label=xxd
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/strings -n 8 /bin/true # [verify] validate.sh:1384 label=strings
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/nm -D /bin/true # [verify] validate.sh:1385 label=nm
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/objdump -f /bin/true # [verify] validate.sh:1386 label=objdump
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/readelf -h /bin/true # [verify] validate.sh:1387 label=readelf
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/size /bin/true # [verify] validate.sh:1388 label=size
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/addr2line -e /bin/true 0 # [verify] validate.sh:1389 label=addr2line
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/c++filt _Z3foov # [verify] validate.sh:1390 label=c++filt
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/expand -t 4 README.md # [verify] validate.sh:1391 label=expand
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/unexpand -a README.md # [verify] validate.sh:1392 label=unexpand
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/printenv PATH # [verify] validate.sh:1393 label=printenv
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/whoami # [verify] validate.sh:1394 label=whoami
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/groups --version # [verify] validate.sh:1395 label=groups
hermit run --backend liteinst --no-namespace --strict --verify -- /bin/sh -c 'printf "sh-ok\n"' # [verify] validate.sh:1397 label=sh
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/cmp README.md README.md # [verify] validate.sh:1398 label=cmp
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/diff README.md README.md # [verify] validate.sh:1399 label=diff
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/pr -t README.md # [verify] validate.sh:1400 label=pr
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/numfmt --to=iec 1048576 # [verify] validate.sh:1401 label=numfmt
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/test -f README.md # [verify] validate.sh:1402 label=test
hermit run --backend liteinst --no-namespace --strict --verify -- '/usr/bin/[' -f README.md ']' # [verify] validate.sh:1403 label=bracket
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/users # [verify] validate.sh:1404 label=users
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/pinky -l root # [verify] validate.sh:1405 label=pinky
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/ptx README.md # [verify] validate.sh:1406 label=ptx
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/tsort /dev/null # [verify] validate.sh:1407 label=tsort
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/column README.md # [verify] validate.sh:1408 label=column
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/hexdump -C -n 32 README.md # [verify] validate.sh:1409 label=hexdump
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/iconv -f UTF-8 -t UTF-8 README.md # [verify] validate.sh:1410 label=iconv
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/jq -n '{answer: 42}' # [verify] validate.sh:1411 label=jq
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/cal 1 2000 # [verify] validate.sh:1414 label=cal
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/sleep 0 # [verify] validate.sh:1415 label=sleep
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/systemd-repart --version # [verify] validate.sh:1416 label=systemd-repart-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/comm /dev/null /dev/null # [verify] validate.sh:1417 label=comm
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/join /dev/null /dev/null # [verify] validate.sh:1418 label=join
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/tee # [verify] validate.sh:1419 label=tee
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/tr a-z A-Z # [verify] validate.sh:1420 label=tr
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/xargs -r # [verify] validate.sh:1421 label=xargs
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/ar --version # [verify] validate.sh:1423 label=ar
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/as --version # [verify] validate.sh:1424 label=as
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/gprof --version # [verify] validate.sh:1427 label=gprof
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/ld --version # [verify] validate.sh:1428 label=ld
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/objcopy --version # [verify] validate.sh:1429 label=objcopy
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/ranlib --version # [verify] validate.sh:1430 label=ranlib
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/strip --version # [verify] validate.sh:1431 label=strip
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/elfedit --version # [verify] validate.sh:1432 label=elfedit
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/getopt --version # [verify] validate.sh:1433 label=getopt
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/dd --version # [verify] validate.sh:1434 label=dd
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/df -P README.md # [verify] validate.sh:1435 label=df
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/split --version # [verify] validate.sh:1436 label=split
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/csplit --version # [verify] validate.sh:1437 label=csplit
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/pathchk README.md # [verify] validate.sh:1438 label=pathchk
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/getconf ARG_MAX # [verify] validate.sh:1439 label=getconf
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/locale charmap # [verify] validate.sh:1440 label=locale
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/whereis sh # [verify] validate.sh:1441 label=whereis
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/namei README.md # [verify] validate.sh:1442 label=namei
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/tty --version # [verify] validate.sh:1443 label=tty
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/timeout --version # [verify] validate.sh:1444 label=timeout
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/flock --version # [verify] validate.sh:1445 label=flock
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/chrt --version # [verify] validate.sh:1446 label=chrt
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/ionice --version # [verify] validate.sh:1447 label=ionice
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/pgrep --version # [verify] validate.sh:1448 label=pgrep
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/pkill --version # [verify] validate.sh:1449 label=pkill
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/patch --version # [verify] validate.sh:1455 label=patch
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/kill --version # [verify] validate.sh:1462 label=kill
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/ps --version # [verify] validate.sh:1463 label=ps
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/top -v # [verify] validate.sh:1464 label=top
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/sbin/ip -Version # [verify] validate.sh:1465 label=ip
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/sbin/ss --version # [verify] validate.sh:1466 label=ss
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/taskset --version # [verify] validate.sh:1467 label=taskset
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/time --version # [verify] validate.sh:1468 label=time
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/yes --version # [verify] validate.sh:1469 label=yes
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/shuf --version # [verify] validate.sh:1470 label=shuf
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/cp --version # [verify] validate.sh:1471 label=cp
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/mv --version # [verify] validate.sh:1472 label=mv
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/rm --version # [verify] validate.sh:1473 label=rm
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/mkdir --version # [verify] validate.sh:1474 label=mkdir
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/rmdir --version # [verify] validate.sh:1475 label=rmdir
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/touch --version # [verify] validate.sh:1476 label=touch
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/chmod --version # [verify] validate.sh:1477 label=chmod
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/chown --version # [verify] validate.sh:1478 label=chown
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/ln --version # [verify] validate.sh:1479 label=ln
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/install --version # [verify] validate.sh:1480 label=install
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/mkfifo --version # [verify] validate.sh:1481 label=mkfifo
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/mktemp --version # [verify] validate.sh:1482 label=mktemp
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/link --version # [verify] validate.sh:1483 label=link
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/unlink --version # [verify] validate.sh:1484 label=unlink
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/sync --version # [verify] validate.sh:1485 label=sync
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/truncate --version # [verify] validate.sh:1486 label=truncate
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/who --version # [verify] validate.sh:1487 label=who
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/w --version # [verify] validate.sh:1488 label=w
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/last --version # [verify] validate.sh:1489 label=last
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/lastlog --help # [verify] validate.sh:1490 label=lastlog
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/wall --version # [verify] validate.sh:1491 label=wall
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/sbin/pivot_root --version # [verify] validate.sh:1492 label=pivot-root-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/shuf -i 1-1 -n 1 # [verify] validate.sh:1494 label=shuf-singleton
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/sync -f README.md # [verify] validate.sh:1495 label=sync-file
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/mountpoint -q / # [verify] validate.sh:1496 label=mountpoint
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/getent passwd root # [verify] validate.sh:1497 label=getent-root
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/sbin/ip -o link show lo # [verify] validate.sh:1498 label=ip-loopback
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/lastlog -u root # [verify] validate.sh:1499 label=lastlog-root
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/who # [verify] validate.sh:1501 label=who-live
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/last -n 1 # [verify] validate.sh:1502 label=last-live
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/taskset -pc 1 # [verify] validate.sh:1503 label=taskset-pid1
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/tail -n 3 README.md # [verify] validate.sh:1505 label=tail
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/hostid # [verify] validate.sh:1506 label=hostid
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/stty --version # [verify] validate.sh:1507 label=stty
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/dircolors --version # [verify] validate.sh:1508 label=dircolors
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/env --version # [verify] validate.sh:1509 label=env-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/nice --version # [verify] validate.sh:1510 label=nice-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/nohup --version # [verify] validate.sh:1511 label=nohup-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/stdbuf --version # [verify] validate.sh:1512 label=stdbuf-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/free --version # [verify] validate.sh:1513 label=free-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/findmnt -n -o TARGET / # [verify] validate.sh:1519 label=findmnt-root
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/systemd-escape --path /tmp/hermit-compat # [verify] validate.sh:1520 label=systemd-escape
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/sbin/sysctl -n kernel.ostype # [verify] validate.sh:1521 label=sysctl-ostype
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/php -v # [verify] validate.sh:1522 label=php-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/basenc --base64 README.md # [verify] validate.sh:1523 label=basenc
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/chcon --version # [verify] validate.sh:1524 label=chcon-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/runcon --version # [verify] validate.sh:1525 label=runcon-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/lsblk --version # [verify] validate.sh:1526 label=lsblk-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/lslocks --version # [verify] validate.sh:1527 label=lslocks-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/lsns --version # [verify] validate.sh:1528 label=lsns-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/prlimit --nofile # [verify] validate.sh:1529 label=prlimit-live
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/setpriv --dump # [verify] validate.sh:1530 label=setpriv-dump
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/nsenter --version # [verify] validate.sh:1531 label=nsenter-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/unshare --version # [verify] validate.sh:1532 label=unshare-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/choom -p 1 # [verify] validate.sh:1533 label=choom-pid1
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/rename --version # [verify] validate.sh:1534 label=rename-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/script --version # [verify] validate.sh:1535 label=script-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/scriptreplay --version # [verify] validate.sh:1536 label=scriptreplay-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/utmpdump --version # [verify] validate.sh:1537 label=utmpdump-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/uuidgen --sha1 --namespace @dns --name hermit # [verify] validate.sh:1538 label=uuidgen-name
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/systemctl --version # [verify] validate.sh:1539 label=systemctl-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/journalctl --version # [verify] validate.sh:1540 label=journalctl-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/busctl --version # [verify] validate.sh:1541 label=busctl-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/findmnt -n -o FSTYPE / # [verify] validate.sh:1545 label=findmnt-fstype
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/systemd-escape --unescape 'tmp-hermit\x2dcompat' # [verify] validate.sh:1546 label=systemd-unescape
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/systemd-detect-virt # [verify] validate.sh:1547 label=systemd-detect-virt
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/systemd-path temporary # [verify] validate.sh:1548 label=systemd-path
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/systemd-id128 machine-id # [verify] validate.sh:1549 label=systemd-id128
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/lsblk -dn -o NAME,TYPE # [verify] validate.sh:1550 label=lsblk-live
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/localectl --version # [verify] validate.sh:1551 label=localectl-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/loginctl --version # [verify] validate.sh:1552 label=loginctl-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/networkctl --version # [verify] validate.sh:1553 label=networkctl-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/hostnamectl --version # [verify] validate.sh:1554 label=hostnamectl-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/timedatectl --version # [verify] validate.sh:1555 label=timedatectl-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/resolvectl --version # [verify] validate.sh:1556 label=resolvectl-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/coredumpctl --version # [verify] validate.sh:1557 label=coredumpctl-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/udevadm --version # [verify] validate.sh:1558 label=udevadm-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/systemd-analyze --version # [verify] validate.sh:1559 label=systemd-analyze-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/systemd-cgls --version # [verify] validate.sh:1560 label=systemd-cgls-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/systemd-delta --version # [verify] validate.sh:1561 label=systemd-delta-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/systemd-notify --version # [verify] validate.sh:1562 label=systemd-notify-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/sbin/getcap README.md # [verify] validate.sh:1563 label=getcap-readme
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/sbin/setcap -h # [verify] validate.sh:1564 label=setcap-help
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/iostat -V # [verify] validate.sh:1565 label=iostat-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/sbin/getpcaps 1 # [verify] validate.sh:1566 label=getpcaps-pid1
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/sestatus # [verify] validate.sh:1567 label=sestatus
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/diff3 --version # [verify] validate.sh:1568 label=diff3-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/dir -d README.md # [verify] validate.sh:1569 label=dir
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/vdir -d README.md # [verify] validate.sh:1570 label=vdir
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/chgrp --version # [verify] validate.sh:1571 label=chgrp-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/envsubst --version # [verify] validate.sh:1572 label=envsubst-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/ctest --version # [verify] validate.sh:1573 label=ctest-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/cpack --version # [verify] validate.sh:1574 label=cpack-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/sbin/losetup --version # [verify] validate.sh:1575 label=losetup-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/sbin/blkid --version # [verify] validate.sh:1576 label=blkid-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/sbin/wipefs --version # [verify] validate.sh:1577 label=wipefs-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/sbin/partx --version # [verify] validate.sh:1578 label=partx-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/sbin/swapon --version # [verify] validate.sh:1579 label=swapon-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/dmesg --version # [verify] validate.sh:1580 label=dmesg-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/fallocate --version # [verify] validate.sh:1581 label=fallocate-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/uuidparse --version # [verify] validate.sh:1582 label=uuidparse-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/ipcmk --version # [verify] validate.sh:1583 label=ipcmk-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/ipcrm --version # [verify] validate.sh:1584 label=ipcrm-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/ipcs --version # [verify] validate.sh:1585 label=ipcs-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/lsmem --version # [verify] validate.sh:1586 label=lsmem-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/lsipc --version # [verify] validate.sh:1587 label=lsipc-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/lslogins --version # [verify] validate.sh:1588 label=lslogins-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/hardlink --version # [verify] validate.sh:1589 label=hardlink-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/wdctl --version # [verify] validate.sh:1590 label=wdctl-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/col --version # [verify] validate.sh:1591 label=col-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/colcrt --version # [verify] validate.sh:1592 label=colcrt-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/colrm --version # [verify] validate.sh:1593 label=colrm-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/look --version # [verify] validate.sh:1594 label=look-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/mcookie --version # [verify] validate.sh:1595 label=mcookie-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/more --version # [verify] validate.sh:1596 label=more-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/ul --version # [verify] validate.sh:1597 label=ul-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/setsid --version # [verify] validate.sh:1598 label=setsid-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/setarch --version # [verify] validate.sh:1599 label=setarch-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/sbin/readprofile --version # [verify] validate.sh:1600 label=readprofile-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/sbin/rtcwake --version # [verify] validate.sh:1601 label=rtcwake-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/sbin/agetty --version # [verify] validate.sh:1602 label=agetty-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/sbin/resizepart --version # [verify] validate.sh:1603 label=resizepart-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/fincore --version # [verify] validate.sh:1604 label=fincore-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/scriptlive --version # [verify] validate.sh:1605 label=scriptlive-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/lastb --version # [verify] validate.sh:1606 label=lastb-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/renice --version # [verify] validate.sh:1607 label=renice-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/sbin/blockdev --version # [verify] validate.sh:1608 label=blockdev-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/sbin/sfdisk --version # [verify] validate.sh:1609 label=sfdisk-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/sbin/fdisk --version # [verify] validate.sh:1610 label=fdisk-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/sbin/fsck --version # [verify] validate.sh:1611 label=fsck-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/sbin/mkfs --version # [verify] validate.sh:1612 label=mkfs-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/bootctl --version # [verify] validate.sh:1613 label=bootctl-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/kernel-install --version # [verify] validate.sh:1614 label=kernel-install-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/oomctl --version # [verify] validate.sh:1615 label=oomctl-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/portablectl --version # [verify] validate.sh:1616 label=portablectl-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/userdbctl --version # [verify] validate.sh:1617 label=userdbctl-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/systemd-cat --version # [verify] validate.sh:1618 label=systemd-cat-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/systemd-cgtop --version # [verify] validate.sh:1619 label=systemd-cgtop-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/systemd-mount --version # [verify] validate.sh:1620 label=systemd-mount-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/systemd-run --version # [verify] validate.sh:1621 label=systemd-run-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/systemd-socket-activate --version # [verify] validate.sh:1622 label=systemd-socket-activate-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/systemd-stdio-bridge --version # [verify] validate.sh:1623 label=systemd-stdio-bridge-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/systemd-sysusers --version # [verify] validate.sh:1624 label=systemd-sysusers-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/systemd-tmpfiles --version # [verify] validate.sh:1625 label=systemd-tmpfiles-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/systemd-tty-ask-password-agent --version # [verify] validate.sh:1626 label=systemd-tty-ask-password-agent-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/chmem --version # [verify] validate.sh:1627 label=chmem-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/eject --version # [verify] validate.sh:1628 label=eject-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/getfattr --version # [verify] validate.sh:1629 label=getfattr-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/setfattr --version # [verify] validate.sh:1630 label=setfattr-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/bison --version # [verify] validate.sh:1631 label=bison-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/flex --version # [verify] validate.sh:1632 label=flex-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/dot -V # [verify] validate.sh:1633 label=dot-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/bat --version # [verify] validate.sh:1634 label=bat-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/cscope --version # [verify] validate.sh:1635 label=cscope-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/sbin/lspci --version # [verify] validate.sh:1636 label=lspci-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/dos2unix --version # [verify] validate.sh:1637 label=dos2unix-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/fish --version # [verify] validate.sh:1638 label=fish-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/gawk --version # [verify] validate.sh:1639 label=gawk-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/eu-addr2line --version # [verify] validate.sh:1640 label=eu-addr2line-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/eu-ar --version # [verify] validate.sh:1641 label=eu-ar-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/eu-nm --version # [verify] validate.sh:1642 label=eu-nm-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/eu-readelf --version # [verify] validate.sh:1643 label=eu-readelf-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/eu-size --version # [verify] validate.sh:1644 label=eu-size-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/eu-strings --version # [verify] validate.sh:1645 label=eu-strings-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/ed --version # [verify] validate.sh:1646 label=ed-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/patch --version # [verify] validate.sh:1647 label=patch-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/vmstat --version # [verify] validate.sh:1648 label=vmstat-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/strace --version # [verify] validate.sh:1649 label=strace-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/perf --version # [verify] validate.sh:1650 label=perf-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/lsusb --version # [verify] validate.sh:1651 label=lsusb-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/sbin/ethtool --version # [verify] validate.sh:1652 label=ethtool-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/sbin/bridge -V # [verify] validate.sh:1653 label=bridge-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/sbin/tc -V # [verify] validate.sh:1654 label=tc-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/sbin/nft --version # [verify] validate.sh:1655 label=nft-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/mpstat -V # [verify] validate.sh:1656 label=mpstat-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/sar -V # [verify] validate.sh:1657 label=sar-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/pidstat -V # [verify] validate.sh:1658 label=pidstat-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/infocmp -V # [verify] validate.sh:1659 label=infocmp-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/tic -V # [verify] validate.sh:1660 label=tic-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/toe -V # [verify] validate.sh:1661 label=toe-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/tput -V # [verify] validate.sh:1662 label=tput-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/fribidi --version # [verify] validate.sh:1663 label=fribidi-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/fuse-overlayfs --version # [verify] validate.sh:1664 label=fuse-overlayfs-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/dwp --version # [verify] validate.sh:1665 label=dwp-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/rsync --version # [verify] validate.sh:1666 label=rsync-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/eu-findtextrel --version # [verify] validate.sh:1667 label=eu-findtextrel-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/dwz --version # [verify] validate.sh:1668 label=dwz-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/eu-elfclassify --version # [verify] validate.sh:1669 label=eu-elfclassify-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/eu-elfcmp --version # [verify] validate.sh:1670 label=eu-elfcmp-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/psql --version # [verify] validate.sh:1671 label=psql-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/pg_dump --version # [verify] validate.sh:1672 label=pg-dump-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/fc-cat --version # [verify] validate.sh:1675 label=fc-cat-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/fc-list --version # [verify] validate.sh:1676 label=fc-list-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/fc-match --version # [verify] validate.sh:1677 label=fc-match-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/fc-pattern --version # [verify] validate.sh:1678 label=fc-pattern-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/fc-query --version # [verify] validate.sh:1679 label=fc-query-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/fc-scan --version # [verify] validate.sh:1680 label=fc-scan-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/fc-validate --version # [verify] validate.sh:1681 label=fc-validate-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/circo -V # [verify] validate.sh:1682 label=circo-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/fdp -V # [verify] validate.sh:1683 label=fdp-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/neato -V # [verify] validate.sh:1684 label=neato-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/sfdp -V # [verify] validate.sh:1685 label=sfdp-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/twopi -V # [verify] validate.sh:1686 label=twopi-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/eu-objdump --version # [verify] validate.sh:1687 label=eu-objdump-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/eu-ranlib --version # [verify] validate.sh:1688 label=eu-ranlib-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/eu-strip --version # [verify] validate.sh:1689 label=eu-strip-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/eu-unstrip --version # [verify] validate.sh:1690 label=eu-unstrip-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/chronyc -v # [verify] validate.sh:1691 label=chronyc-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/cpupower --version # [verify] validate.sh:1692 label=cpupower-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/expect -v # [verify] validate.sh:1693 label=expect-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/kmod --version # [verify] validate.sh:1694 label=kmod-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/rpm --version # [verify] validate.sh:1695 label=rpm-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/ipmitool -V # [verify] validate.sh:1696 label=ipmitool-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/man --version # [verify] validate.sh:1697 label=man-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/systemd-hwdb --version # [verify] validate.sh:1698 label=systemd-hwdb-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/systemd-creds --version # [verify] validate.sh:1699 label=systemd-creds-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/systemd-ac-power --version # [verify] validate.sh:1700 label=systemd-ac-power-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/systemd-ask-password --version # [verify] validate.sh:1701 label=systemd-ask-password-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/systemd-cryptenroll --version # [verify] validate.sh:1702 label=systemd-cryptenroll-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/systemd-dissect --version # [verify] validate.sh:1703 label=systemd-dissect-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/systemd-firstboot --version # [verify] validate.sh:1704 label=systemd-firstboot-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/systemd-inhibit --version # [verify] validate.sh:1705 label=systemd-inhibit-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/systemd-machine-id-setup --version # [verify] validate.sh:1706 label=systemd-machine-id-setup-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/systemd-mute-console --version # [verify] validate.sh:1707 label=systemd-mute-console-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/systemd-nspawn --version # [verify] validate.sh:1708 label=systemd-nspawn-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/sbin/btrfs --version # [verify] validate.sh:1709 label=btrfs-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/systemd-sysext --version # [verify] validate.sh:1710 label=systemd-sysext-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/systemd-vmspawn --version # [verify] validate.sh:1711 label=systemd-vmspawn-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/systemd-vpick --version # [verify] validate.sh:1712 label=systemd-vpick-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/krb5-config --version # [verify] validate.sh:1713 label=krb5-config-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/pcre2-config --version # [verify] validate.sh:1714 label=pcre2-config-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/sbin/ausearch --version # [verify] validate.sh:1715 label=ausearch-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/sbin/aureport --version # [verify] validate.sh:1716 label=aureport-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/checkmodule -V # [verify] validate.sh:1717 label=checkmodule-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/checkpolicy -V # [verify] validate.sh:1718 label=checkpolicy-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/sbin/chronyd -v # [verify] validate.sh:1719 label=chronyd-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/sbin/smartctl --version # [verify] validate.sh:1720 label=smartctl-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/sbin/nvme version # [verify] validate.sh:1721 label=nvme-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/sbin/mdadm --version # [verify] validate.sh:1722 label=mdadm-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/sbin/xfs_db -V # [verify] validate.sh:1723 label=xfs-db-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/sbin/mkfs.xfs -V # [verify] validate.sh:1724 label=mkfs-xfs-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/FileCheck --version # [verify] validate.sh:1725 label=filecheck-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/clang++ --version # [verify] validate.sh:1726 label=clangxx-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llc --version # [verify] validate.sh:1730 label=llc-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-addr2line --version # [verify] validate.sh:1731 label=llvm-addr2line-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-ar --version # [verify] validate.sh:1732 label=llvm-ar-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-as --version # [verify] validate.sh:1733 label=llvm-as-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-bcanalyzer --version # [verify] validate.sh:1734 label=llvm-bcanalyzer-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-cov --version # [verify] validate.sh:1735 label=llvm-cov-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-cxxfilt --version # [verify] validate.sh:1736 label=llvm-cxxfilt-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-diff --version # [verify] validate.sh:1737 label=llvm-diff-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-dis --version # [verify] validate.sh:1738 label=llvm-dis-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-dwarfdump --version # [verify] validate.sh:1739 label=llvm-dwarfdump-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-dwp --version # [verify] validate.sh:1740 label=llvm-dwp-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-extract --version # [verify] validate.sh:1741 label=llvm-extract-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-link --version # [verify] validate.sh:1742 label=llvm-link-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-mc --version # [verify] validate.sh:1743 label=llvm-mc-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-mca --version # [verify] validate.sh:1744 label=llvm-mca-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-nm --version # [verify] validate.sh:1745 label=llvm-nm-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-objcopy --version # [verify] validate.sh:1746 label=llvm-objcopy-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-objdump --version # [verify] validate.sh:1747 label=llvm-objdump-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-profdata --version # [verify] validate.sh:1748 label=llvm-profdata-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/opt --version # [verify] validate.sh:1749 label=opt-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-ranlib --version # [verify] validate.sh:1750 label=llvm-ranlib-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-readelf --version # [verify] validate.sh:1751 label=llvm-readelf-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-readobj --version # [verify] validate.sh:1752 label=llvm-readobj-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-size --version # [verify] validate.sh:1753 label=llvm-size-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-strings --version # [verify] validate.sh:1754 label=llvm-strings-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-strip --version # [verify] validate.sh:1755 label=llvm-strip-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-symbolizer --version # [verify] validate.sh:1756 label=llvm-symbolizer-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-bitcode-strip --version # [verify] validate.sh:1757 label=llvm-bitcode-strip-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-cat --version # [verify] validate.sh:1758 label=llvm-cat-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-cfi-verify --version # [verify] validate.sh:1759 label=llvm-cfi-verify-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-cgdata --version # [verify] validate.sh:1760 label=llvm-cgdata-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-ctxprof-util --version # [verify] validate.sh:1761 label=llvm-ctxprof-util-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-cxxdump --version # [verify] validate.sh:1762 label=llvm-cxxdump-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-cxxmap --version # [verify] validate.sh:1763 label=llvm-cxxmap-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-debuginfo-analyzer --version # [verify] validate.sh:1764 label=llvm-debuginfo-analyzer-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-dwarfutil --version # [verify] validate.sh:1765 label=llvm-dwarfutil-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-exegesis --version # [verify] validate.sh:1766 label=llvm-exegesis-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-gsymutil --version # [verify] validate.sh:1767 label=llvm-gsymutil-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-ifs --version # [verify] validate.sh:1768 label=llvm-ifs-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-install-name-tool --version # [verify] validate.sh:1769 label=llvm-install-name-tool-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-ir2vec --version # [verify] validate.sh:1770 label=llvm-ir2vec-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-jitlink --version # [verify] validate.sh:1771 label=llvm-jitlink-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-lib --version # [verify] validate.sh:1772 label=llvm-lib-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-libtool-darwin --version # [verify] validate.sh:1773 label=llvm-libtool-darwin-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-lipo --version # [verify] validate.sh:1774 label=llvm-lipo-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-lto --version # [verify] validate.sh:1775 label=llvm-lto-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-lto2 --version # [verify] validate.sh:1776 label=llvm-lto2-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-ml --version # [verify] validate.sh:1777 label=llvm-ml-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-modextract --version # [verify] validate.sh:1778 label=llvm-modextract-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-offload-binary --version # [verify] validate.sh:1779 label=llvm-offload-binary-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-offload-wrapper --version # [verify] validate.sh:1780 label=llvm-offload-wrapper-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-opt-report --version # [verify] validate.sh:1781 label=llvm-opt-report-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-otool --version # [verify] validate.sh:1782 label=llvm-otool-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-pdbutil --version # [verify] validate.sh:1783 label=llvm-pdbutil-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-profgen --version # [verify] validate.sh:1784 label=llvm-profgen-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-readtapi --version # [verify] validate.sh:1785 label=llvm-readtapi-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-reduce --version # [verify] validate.sh:1786 label=llvm-reduce-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-remarkutil --version # [verify] validate.sh:1787 label=llvm-remarkutil-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-rtdyld --version # [verify] validate.sh:1788 label=llvm-rtdyld-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-sim --version # [verify] validate.sh:1789 label=llvm-sim-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-split --version # [verify] validate.sh:1790 label=llvm-split-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-stress --version # [verify] validate.sh:1791 label=llvm-stress-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-tblgen --version # [verify] validate.sh:1792 label=llvm-tblgen-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-undname --version # [verify] validate.sh:1793 label=llvm-undname-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-windres --version # [verify] validate.sh:1794 label=llvm-windres-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-xray --version # [verify] validate.sh:1795 label=llvm-xray-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/eu-elflint --version # [verify] validate.sh:1798 label=eu-elflint-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/eu-srcfiles --version # [verify] validate.sh:1799 label=eu-srcfiles-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/eu-stack --version # [verify] validate.sh:1800 label=eu-stack-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/ld.gold --version # [verify] validate.sh:1801 label=ld-gold-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/cc --version # [verify] validate.sh:1802 label=cc-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/c++ --version # [verify] validate.sh:1803 label=cxx-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/conmon --version # [verify] validate.sh:1804 label=conmon-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/rpmkeys --version # [verify] validate.sh:1805 label=rpmkeys-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/rpmdb --version # [verify] validate.sh:1806 label=rpmdb-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/rpmbuild --version # [verify] validate.sh:1807 label=rpmbuild-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/rpmspec --version # [verify] validate.sh:1808 label=rpmspec-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/gpgv --version # [verify] validate.sh:1809 label=gpgv-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/gpg-connect-agent --version # [verify] validate.sh:1810 label=gpg-connect-agent-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/sdiff --version # [verify] validate.sh:1811 label=sdiff-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/zipinfo -v # [verify] validate.sh:1812 label=zipinfo-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/zipcloak -v # [verify] validate.sh:1813 label=zipcloak-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/zipnote -v # [verify] validate.sh:1814 label=zipnote-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/zipsplit -v # [verify] validate.sh:1815 label=zipsplit-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/xsltproc --version # [verify] validate.sh:1816 label=xsltproc-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/clang++-22 --version # [verify] validate.sh:1819 label=clangxx-22-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-ranlib-22 --version # [verify] validate.sh:1823 label=llvm-ranlib-22-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-readelf-22 --version # [verify] validate.sh:1824 label=llvm-readelf-22-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-addr2line-22 --version # [verify] validate.sh:1825 label=llvm-addr2line-22-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-ar-22 --version # [verify] validate.sh:1826 label=llvm-ar-22-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-as-22 --version # [verify] validate.sh:1827 label=llvm-as-22-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-bcanalyzer-22 --version # [verify] validate.sh:1828 label=llvm-bcanalyzer-22-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-bitcode-strip-22 --version # [verify] validate.sh:1829 label=llvm-bitcode-strip-22-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-cat-22 --version # [verify] validate.sh:1830 label=llvm-cat-22-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-cfi-verify-22 --version # [verify] validate.sh:1831 label=llvm-cfi-verify-22-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-cgdata-22 --version # [verify] validate.sh:1832 label=llvm-cgdata-22-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-cov-22 --version # [verify] validate.sh:1833 label=llvm-cov-22-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-ctxprof-util-22 --version # [verify] validate.sh:1834 label=llvm-ctxprof-util-22-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-cxxdump-22 --version # [verify] validate.sh:1835 label=llvm-cxxdump-22-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-cxxfilt-22 --version # [verify] validate.sh:1836 label=llvm-cxxfilt-22-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-cxxmap-22 --version # [verify] validate.sh:1837 label=llvm-cxxmap-22-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-debuginfo-analyzer-22 --version # [verify] validate.sh:1838 label=llvm-debuginfo-analyzer-22-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-diff-22 --version # [verify] validate.sh:1839 label=llvm-diff-22-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-dis-22 --version # [verify] validate.sh:1840 label=llvm-dis-22-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-dwarfdump-22 --version # [verify] validate.sh:1841 label=llvm-dwarfdump-22-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-dwarfutil-22 --version # [verify] validate.sh:1842 label=llvm-dwarfutil-22-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-dwp-22 --version # [verify] validate.sh:1843 label=llvm-dwp-22-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-exegesis-22 --version # [verify] validate.sh:1844 label=llvm-exegesis-22-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-extract-22 --version # [verify] validate.sh:1845 label=llvm-extract-22-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-gsymutil-22 --version # [verify] validate.sh:1846 label=llvm-gsymutil-22-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-ifs-22 --version # [verify] validate.sh:1847 label=llvm-ifs-22-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-install-name-tool-22 --version # [verify] validate.sh:1848 label=llvm-install-name-tool-22-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-ir2vec-22 --version # [verify] validate.sh:1849 label=llvm-ir2vec-22-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-jitlink-22 --version # [verify] validate.sh:1850 label=llvm-jitlink-22-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-lib-22 --version # [verify] validate.sh:1851 label=llvm-lib-22-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-libtool-darwin-22 --version # [verify] validate.sh:1852 label=llvm-libtool-darwin-22-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-link-22 --version # [verify] validate.sh:1853 label=llvm-link-22-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-lipo-22 --version # [verify] validate.sh:1854 label=llvm-lipo-22-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-lto-22 --version # [verify] validate.sh:1855 label=llvm-lto-22-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-lto2-22 --version # [verify] validate.sh:1856 label=llvm-lto2-22-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-mc-22 --version # [verify] validate.sh:1857 label=llvm-mc-22-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-mca-22 --version # [verify] validate.sh:1858 label=llvm-mca-22-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-ml-22 --version # [verify] validate.sh:1859 label=llvm-ml-22-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-ml64-22 --version # [verify] validate.sh:1860 label=llvm-ml64-22-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-modextract-22 --version # [verify] validate.sh:1861 label=llvm-modextract-22-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-offload-binary-22 --version # [verify] validate.sh:1862 label=llvm-offload-binary-22-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-offload-wrapper-22 --version # [verify] validate.sh:1863 label=llvm-offload-wrapper-22-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-opt-report-22 --version # [verify] validate.sh:1864 label=llvm-opt-report-22-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-otool-22 --version # [verify] validate.sh:1865 label=llvm-otool-22-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-pdbutil-22 --version # [verify] validate.sh:1866 label=llvm-pdbutil-22-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-profdata-22 --version # [verify] validate.sh:1867 label=llvm-profdata-22-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-profgen-22 --version # [verify] validate.sh:1868 label=llvm-profgen-22-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-readobj-22 --version # [verify] validate.sh:1869 label=llvm-readobj-22-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-readtapi-22 --version # [verify] validate.sh:1870 label=llvm-readtapi-22-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-reduce-22 --version # [verify] validate.sh:1871 label=llvm-reduce-22-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-remarkutil-22 --version # [verify] validate.sh:1872 label=llvm-remarkutil-22-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-rtdyld-22 --version # [verify] validate.sh:1873 label=llvm-rtdyld-22-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-sim-22 --version # [verify] validate.sh:1874 label=llvm-sim-22-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/FileCheck-22 --version # [verify] validate.sh:1875 label=filecheck-22-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/bugpoint-22 --version # [verify] validate.sh:1876 label=bugpoint-22-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/dsymutil-22 --version # [verify] validate.sh:1877 label=dsymutil-22-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llc-22 --version # [verify] validate.sh:1878 label=llc-22-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/lli-22 --version # [verify] validate.sh:1879 label=lli-22-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-nm-22 --version # [verify] validate.sh:1880 label=llvm-nm-22-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-objcopy-22 --version # [verify] validate.sh:1881 label=llvm-objcopy-22-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-objdump-22 --version # [verify] validate.sh:1882 label=llvm-objdump-22-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-size-22 --version # [verify] validate.sh:1883 label=llvm-size-22-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-split-22 --version # [verify] validate.sh:1884 label=llvm-split-22-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-stress-22 --version # [verify] validate.sh:1885 label=llvm-stress-22-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-strings-22 --version # [verify] validate.sh:1886 label=llvm-strings-22-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-strip-22 --version # [verify] validate.sh:1887 label=llvm-strip-22-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-symbolizer-22 --version # [verify] validate.sh:1888 label=llvm-symbolizer-22-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-tblgen-22 --version # [verify] validate.sh:1889 label=llvm-tblgen-22-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-undname-22 --version # [verify] validate.sh:1890 label=llvm-undname-22-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-windres-22 --version # [verify] validate.sh:1891 label=llvm-windres-22-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-xray-22 --version # [verify] validate.sh:1892 label=llvm-xray-22-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/obj2yaml-22 --version # [verify] validate.sh:1893 label=obj2yaml-22-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/opt-22 --version # [verify] validate.sh:1894 label=opt-22-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/reduce-chunk-list-22 --version # [verify] validate.sh:1895 label=reduce-chunk-list-22-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/sancov-22 --version # [verify] validate.sh:1896 label=sancov-22-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/sanstats-22 --version # [verify] validate.sh:1897 label=sanstats-22-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/split-file-22 --version # [verify] validate.sh:1898 label=split-file-22-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/verify-uselistorder-22 --version # [verify] validate.sh:1899 label=verify-uselistorder-22-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/yaml2obj-22 --version # [verify] validate.sh:1900 label=yaml2obj-22-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/aarch64-linux-gnu-addr2line --version # [verify] validate.sh:1901 label=aarch64-linux-gnu-addr2line-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/aarch64-linux-gnu-ar --version # [verify] validate.sh:1902 label=aarch64-linux-gnu-ar-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/aarch64-linux-gnu-as --version # [verify] validate.sh:1903 label=aarch64-linux-gnu-as-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/aarch64-linux-gnu-c++ --version # [verify] validate.sh:1904 label=aarch64-linux-gnu-cxx-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/aarch64-linux-gnu-c++filt --version # [verify] validate.sh:1905 label=aarch64-linux-gnu-cxxfilt-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/aarch64-linux-gnu-elfedit --version # [verify] validate.sh:1907 label=aarch64-linux-gnu-elfedit-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/aarch64-linux-gnu-g++ --version # [verify] validate.sh:1908 label=aarch64-linux-gnu-gxx-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/aarch64-linux-gnu-gprof --version # [verify] validate.sh:1913 label=aarch64-linux-gnu-gprof-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/aarch64-linux-gnu-ld --version # [verify] validate.sh:1914 label=aarch64-linux-gnu-ld-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/aarch64-linux-gnu-ld.bfd --version # [verify] validate.sh:1915 label=aarch64-linux-gnu-ld-bfd-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/aarch64-linux-gnu-lto-dump --version # [verify] validate.sh:1916 label=aarch64-linux-gnu-lto-dump-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/aarch64-linux-gnu-nm --version # [verify] validate.sh:1917 label=aarch64-linux-gnu-nm-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/aarch64-linux-gnu-objcopy --version # [verify] validate.sh:1918 label=aarch64-linux-gnu-objcopy-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/aarch64-linux-gnu-objdump --version # [verify] validate.sh:1919 label=aarch64-linux-gnu-objdump-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/aarch64-linux-gnu-ranlib --version # [verify] validate.sh:1920 label=aarch64-linux-gnu-ranlib-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/aarch64-linux-gnu-readelf --version # [verify] validate.sh:1921 label=aarch64-linux-gnu-readelf-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/aarch64-linux-gnu-size --version # [verify] validate.sh:1922 label=aarch64-linux-gnu-size-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/aarch64-linux-gnu-strings --version # [verify] validate.sh:1923 label=aarch64-linux-gnu-strings-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/aarch64-linux-gnu-strip --version # [verify] validate.sh:1924 label=aarch64-linux-gnu-strip-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-cas-22 --help # [verify] validate.sh:1925 label=llvm-cas-22-help
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-debuginfod-22 --help # [verify] validate.sh:1926 label=llvm-debuginfod-22-help
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-debuginfod-find-22 --help # [verify] validate.sh:1927 label=llvm-debuginfod-find-22-help
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/llvm-tli-checker-22 --help # [verify] validate.sh:1928 label=llvm-tli-checker-22-help
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/x86_64-linux-gnu-addr2line --version # [verify] validate.sh:1929 label=x86-64-linux-gnu-addr2line-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/x86_64-linux-gnu-ar --version # [verify] validate.sh:1930 label=x86-64-linux-gnu-ar-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/x86_64-linux-gnu-as --version # [verify] validate.sh:1931 label=x86-64-linux-gnu-as-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/x86_64-linux-gnu-c++ --version # [verify] validate.sh:1932 label=x86-64-linux-gnu-cxx-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/x86_64-linux-gnu-c++filt --version # [verify] validate.sh:1933 label=x86-64-linux-gnu-cxxfilt-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/x86_64-linux-gnu-elfedit --version # [verify] validate.sh:1935 label=x86-64-linux-gnu-elfedit-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/x86_64-linux-gnu-g++ --version # [verify] validate.sh:1936 label=x86-64-linux-gnu-gxx-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/x86_64-linux-gnu-gprof --version # [verify] validate.sh:1941 label=x86-64-linux-gnu-gprof-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/x86_64-linux-gnu-ld --version # [verify] validate.sh:1942 label=x86-64-linux-gnu-ld-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/x86_64-linux-gnu-ld.bfd --version # [verify] validate.sh:1943 label=x86-64-linux-gnu-ld-bfd-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/x86_64-linux-gnu-lto-dump --version # [verify] validate.sh:1944 label=x86-64-linux-gnu-lto-dump-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/x86_64-linux-gnu-nm --version # [verify] validate.sh:1945 label=x86-64-linux-gnu-nm-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/x86_64-linux-gnu-objcopy --version # [verify] validate.sh:1946 label=x86-64-linux-gnu-objcopy-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/x86_64-linux-gnu-objdump --version # [verify] validate.sh:1947 label=x86-64-linux-gnu-objdump-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/x86_64-linux-gnu-ranlib --version # [verify] validate.sh:1948 label=x86-64-linux-gnu-ranlib-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/x86_64-linux-gnu-readelf --version # [verify] validate.sh:1949 label=x86-64-linux-gnu-readelf-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/x86_64-linux-gnu-size --version # [verify] validate.sh:1950 label=x86-64-linux-gnu-size-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/x86_64-linux-gnu-strings --version # [verify] validate.sh:1951 label=x86-64-linux-gnu-strings-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/x86_64-linux-gnu-strip --version # [verify] validate.sh:1952 label=x86-64-linux-gnu-strip-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/pg_checksums --version # [verify] validate.sh:1953 label=pg-checksums-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/pg_controldata --version # [verify] validate.sh:1954 label=pg-controldata-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/pg_ctl --version # [verify] validate.sh:1955 label=pg-ctl-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/pg_resetwal --version # [verify] validate.sh:1956 label=pg-resetwal-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/postmaster --version # [verify] validate.sh:1958 label=postmaster-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/x86_64-redhat-linux-c++ --version # [verify] validate.sh:1959 label=x86-64-redhat-linux-cxx-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/x86_64-redhat-linux-g++ --version # [verify] validate.sh:1960 label=x86-64-redhat-linux-gxx-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/acyclic '-?' # [verify] validate.sh:1963 label=acyclic-help
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/bcomps -V # [verify] validate.sh:1964 label=bcomps-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/ccomps -V # [verify] validate.sh:1965 label=ccomps-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/cluster -V # [verify] validate.sh:1966 label=cluster-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/dijkstra '-?' # [verify] validate.sh:1967 label=dijkstra-help
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/dot2gxl '-?' # [verify] validate.sh:1968 label=dot2gxl-help
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/edgepaint -V # [verify] validate.sh:1969 label=edgepaint-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/gc -V # [verify] validate.sh:1970 label=gc-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/gml2gv '-?' # [verify] validate.sh:1971 label=gml2gv-help
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/graphml2gv '-?' # [verify] validate.sh:1972 label=graphml2gv-help
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/gv2gml '-?' # [verify] validate.sh:1973 label=gv2gml-help
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/gv2gxl '-?' # [verify] validate.sh:1974 label=gv2gxl-help
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/gvcolor -V # [verify] validate.sh:1975 label=gvcolor-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/gvgen '-?' # [verify] validate.sh:1976 label=gvgen-help
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/gvmap -V # [verify] validate.sh:1977 label=gvmap-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/gvpack -V # [verify] validate.sh:1978 label=gvpack-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/gxl2dot '-?' # [verify] validate.sh:1979 label=gxl2dot-help
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/gxl2gv '-?' # [verify] validate.sh:1980 label=gxl2gv-help
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/mm2gv '-?' # [verify] validate.sh:1981 label=mm2gv-help
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/nop -V # [verify] validate.sh:1982 label=nop-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/osage -V # [verify] validate.sh:1983 label=osage-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/patchwork -V # [verify] validate.sh:1984 label=patchwork-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/prune '-?' # [verify] validate.sh:1985 label=prune-help
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/sccmap -V # [verify] validate.sh:1986 label=sccmap-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/tred -V # [verify] validate.sh:1987 label=tred-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/unflatten '-?' # [verify] validate.sh:1988 label=unflatten-help
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/dbus-broker --version # [verify] validate.sh:1989 label=dbus-broker-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/dbus-broker-launch --version # [verify] validate.sh:1990 label=dbus-broker-launch-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/dbus-monitor --help # [verify] validate.sh:1991 label=dbus-monitor-help
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/dbus-uuidgen --version # [verify] validate.sh:1992 label=dbus-uuidgen-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/ccmake --version # [verify] validate.sh:1993 label=ccmake-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/ccmake3 --version # [verify] validate.sh:1994 label=ccmake3-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/cmake3 --version # [verify] validate.sh:1995 label=cmake3-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/cpack3 --version # [verify] validate.sh:1996 label=cpack3-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/ctest3 --version # [verify] validate.sh:1997 label=ctest3-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/emacs --version # [verify] validate.sh:1998 label=emacs-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/emacs-30.1-pgtk --version # [verify] validate.sh:1999 label=emacs-30-pgtk-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/emacs-pgtk --version # [verify] validate.sh:2000 label=emacs-pgtk-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/emacsclient --version # [verify] validate.sh:2001 label=emacsclient-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/createrepo --version # [verify] validate.sh:2002 label=createrepo-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/createrepo_c --version # [verify] validate.sh:2003 label=createrepo-c-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/mergerepo --version # [verify] validate.sh:2004 label=mergerepo-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/mergerepo_c --version # [verify] validate.sh:2005 label=mergerepo-c-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/modifyrepo --version # [verify] validate.sh:2006 label=modifyrepo-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/modifyrepo_c --version # [verify] validate.sh:2007 label=modifyrepo-c-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/modulemd-validator --version # [verify] validate.sh:2008 label=modulemd-validator-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/rpm2extents --version # [verify] validate.sh:2009 label=rpm2extents-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/rpmquery --version # [verify] validate.sh:2010 label=rpmquery-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/rpmverify --version # [verify] validate.sh:2011 label=rpmverify-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/podman-compose --help # [verify] validate.sh:2012 label=podman-compose-help
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/rpm2extents_dump --help # [verify] validate.sh:2013 label=rpm2extents-dump-help
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/audit2allow --version # [verify] validate.sh:2014 label=audit2allow-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/audit2why --version # [verify] validate.sh:2015 label=audit2why-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/semodule_expand -V # [verify] validate.sh:2016 label=semodule-expand-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/semodule_link -V # [verify] validate.sh:2017 label=semodule-link-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/semodule_package --help # [verify] validate.sh:2018 label=semodule-package-help
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/host -V # [verify] validate.sh:2019 label=host-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/named-checkzone -v # [verify] validate.sh:2020 label=named-checkzone-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/named-compilezone -v # [verify] validate.sh:2021 label=named-compilezone-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/dirmngr --version # [verify] validate.sh:2022 label=dirmngr-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/dirmngr-client --version # [verify] validate.sh:2023 label=dirmngr-client-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/gpg-error --version # [verify] validate.sh:2024 label=gpg-error-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/gpg-wks-server --version # [verify] validate.sh:2025 label=gpg-wks-server-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/gpgme-json --version # [verify] validate.sh:2026 label=gpgme-json-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/gpgsplit --version # [verify] validate.sh:2027 label=gpgsplit-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/gpgv2 --version # [verify] validate.sh:2028 label=gpgv2-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/watchgnupg --version # [verify] validate.sh:2029 label=watchgnupg-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/btop --version # [verify] validate.sh:2030 label=btop-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/cvtsudoers -V # [verify] validate.sh:2031 label=cvtsudoers-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/debugedit --version # [verify] validate.sh:2032 label=debugedit-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/etags --version # [verify] validate.sh:2033 label=etags-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/evmctl --version # [verify] validate.sh:2034 label=evmctl-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/fish_indent --version # [verify] validate.sh:2035 label=fish-indent-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/fuse2fs -V # [verify] validate.sh:2036 label=fuse2fs-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/gdb --version # [verify] validate.sh:2037 label=gdb-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/getfacl --version # [verify] validate.sh:2038 label=getfacl-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/gettext --version # [verify] validate.sh:2039 label=gettext-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/gmake --version # [verify] validate.sh:2040 label=gmake-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/gpic --version # [verify] validate.sh:2041 label=gpic-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/grub2-fstest --version # [verify] validate.sh:2042 label=grub2-fstest-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/grub2-mkimage --version # [verify] validate.sh:2043 label=grub2-mkimage-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/grub2-mkrelpath --version # [verify] validate.sh:2044 label=grub2-mkrelpath-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/grub2-script-check --version # [verify] validate.sh:2045 label=grub2-script-check-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/htop --version # [verify] validate.sh:2046 label=htop-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/icuinfo --version # [verify] validate.sh:2047 label=icuinfo-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/ipcalc --version # [verify] validate.sh:2048 label=ipcalc-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/irssi --version # [verify] validate.sh:2049 label=irssi-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/less --version # [verify] validate.sh:2050 label=less-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/lesskey --version # [verify] validate.sh:2051 label=lesskey-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/lspci --version # [verify] validate.sh:2052 label=lspci-bin-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/lsscsi --version # [verify] validate.sh:2053 label=lsscsi-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/lz4 --version # [verify] validate.sh:2054 label=lz4-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/lzop --version # [verify] validate.sh:2055 label=lzop-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/linux32 --version # [verify] validate.sh:2056 label=linux32-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/linux64 --version # [verify] validate.sh:2057 label=linux64-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/luac -v # [verify] validate.sh:2058 label=luac-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/mosh --version # [verify] validate.sh:2059 label=mosh-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/msgfmt --version # [verify] validate.sh:2060 label=msgfmt-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/nano --version # [verify] validate.sh:2061 label=nano-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/ncat --version # [verify] validate.sh:2062 label=ncat-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/ncdu --version # [verify] validate.sh:2063 label=ncdu-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/ngettext --version # [verify] validate.sh:2064 label=ngettext-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/nmap --version # [verify] validate.sh:2065 label=nmap-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/nping --version # [verify] validate.sh:2066 label=nping-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/numactl --version # [verify] validate.sh:2067 label=numactl-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/ocamlc -version # [verify] validate.sh:2068 label=ocamlc-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/ocamlopt -version # [verify] validate.sh:2069 label=ocamlopt-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/ocamlrun -version # [verify] validate.sh:2070 label=ocamlrun-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/parallel --version # [verify] validate.sh:2071 label=parallel-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/passt --version # [verify] validate.sh:2072 label=passt-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/perl5.32.1 -v # [verify] validate.sh:2073 label=perl532-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/pigz --version # [verify] validate.sh:2074 label=pigz-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/pngquant --version # [verify] validate.sh:2075 label=pngquant-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/preconv --version # [verify] validate.sh:2076 label=preconv-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/pstree --version # [verify] validate.sh:2077 label=pstree-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/pv --version # [verify] validate.sh:2078 label=pv-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/pygmentize -V # [verify] validate.sh:2079 label=pygmentize-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/pzstd --version # [verify] validate.sh:2081 label=pzstd-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/ragel -v # [verify] validate.sh:2082 label=ragel-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/readtags --version # [verify] validate.sh:2083 label=readtags-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/rst2html --version # [verify] validate.sh:2085 label=rst2html-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/rst2man --version # [verify] validate.sh:2086 label=rst2man-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/rst2xml --version # [verify] validate.sh:2087 label=rst2xml-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/sensors --version # [verify] validate.sh:2088 label=sensors-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/sepdebugcrcfix --version # [verify] validate.sh:2089 label=sepdebugcrcfix-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/setfacl --version # [verify] validate.sh:2090 label=setfacl-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/slirp4netns --version # [verify] validate.sh:2091 label=slirp4netns-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/snmpbulkget -V # [verify] validate.sh:2092 label=snmpbulkget-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/snmpbulkwalk -V # [verify] validate.sh:2093 label=snmpbulkwalk-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/snmpget -V # [verify] validate.sh:2094 label=snmpget-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/snmpgetnext -V # [verify] validate.sh:2095 label=snmpgetnext-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/snmpset -V # [verify] validate.sh:2096 label=snmpset-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/snmpstatus -V # [verify] validate.sh:2097 label=snmpstatus-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/snmptable -V # [verify] validate.sh:2098 label=snmptable-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/snmptranslate -V # [verify] validate.sh:2099 label=snmptranslate-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/snmptrap -V # [verify] validate.sh:2100 label=snmptrap-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/snmpwalk -V # [verify] validate.sh:2101 label=snmpwalk-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/soelim --version # [verify] validate.sh:2102 label=soelim-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/source-highlight --version # [verify] validate.sh:2103 label=source-highlight-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/stress --version # [verify] validate.sh:2105 label=stress-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/sudoreplay -V # [verify] validate.sh:2106 label=sudoreplay-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/systemd-umount --version # [verify] validate.sh:2107 label=systemd-umount-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/tmux -V # [verify] validate.sh:2108 label=tmux-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/tqdm --version # [verify] validate.sh:2109 label=tqdm-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/tracepath -V # [verify] validate.sh:2110 label=tracepath-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/traceroute --version # [verify] validate.sh:2111 label=traceroute-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/tree --version # [verify] validate.sh:2112 label=tree-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/troff --version # [verify] validate.sh:2113 label=troff-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/unix2dos --version # [verify] validate.sh:2114 label=unix2dos-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/unix2mac --version # [verify] validate.sh:2115 label=unix2mac-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/unlz4 --version # [verify] validate.sh:2116 label=unlz4-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/unpigz --version # [verify] validate.sh:2117 label=unpigz-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/unzstd --version # [verify] validate.sh:2118 label=unzstd-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/varlinkctl --version # [verify] validate.sh:2119 label=varlinkctl-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/watch --version # [verify] validate.sh:2120 label=watch-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/which --version # [verify] validate.sh:2121 label=which-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/xgettext --version # [verify] validate.sh:2122 label=xgettext-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/xmlsec1 --version # [verify] validate.sh:2123 label=xmlsec1-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/xmlwf -v # [verify] validate.sh:2124 label=xmlwf-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/xzdec --version # [verify] validate.sh:2125 label=xzdec-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/yaml2obj --version # [verify] validate.sh:2126 label=yaml2obj-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/zsh --version # [verify] validate.sh:2127 label=zsh-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/zstdmt --version # [verify] validate.sh:2128 label=zstdmt-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/rst2html4 --version # [verify] validate.sh:2129 label=rst2html4-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/rst2html5 --version # [verify] validate.sh:2130 label=rst2html5-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/rst2latex --version # [verify] validate.sh:2131 label=rst2latex-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/rst2odt --version # [verify] validate.sh:2132 label=rst2odt-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/rst2pseudoxml --version # [verify] validate.sh:2133 label=rst2pseudoxml-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/rst2s5 --version # [verify] validate.sh:2134 label=rst2s5-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/rst2xetex --version # [verify] validate.sh:2135 label=rst2xetex-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/rstpep2html --version # [verify] validate.sh:2136 label=rstpep2html-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/snmpdelta -V # [verify] validate.sh:2137 label=snmpdelta-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/snmpdf -V # [verify] validate.sh:2138 label=snmpdf-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/snmpinform -V # [verify] validate.sh:2139 label=snmpinform-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/snmpnetstat -V # [verify] validate.sh:2140 label=snmpnetstat-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/snmpping -V # [verify] validate.sh:2141 label=snmpping-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/snmptest -V # [verify] validate.sh:2142 label=snmptest-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/snmptls -V # [verify] validate.sh:2143 label=snmptls-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/snmpusm -V # [verify] validate.sh:2144 label=snmpusm-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/snmpvacm -V # [verify] validate.sh:2145 label=snmpvacm-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/soelim.groff --version # [verify] validate.sh:2146 label=soelim-groff-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/tbl --version # [verify] validate.sh:2147 label=tbl-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/zsoelim --version # [verify] validate.sh:2148 label=zsoelim-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/gsoelim --version # [verify] validate.sh:2149 label=gsoelim-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/gtbl --version # [verify] validate.sh:2150 label=gtbl-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/geqn --version # [verify] validate.sh:2151 label=geqn-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/grops --version # [verify] validate.sh:2152 label=grops-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/grotty --version # [verify] validate.sh:2153 label=grotty-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/gtroff --version # [verify] validate.sh:2154 label=gtroff-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/zcat --version # [verify] validate.sh:2155 label=zcat-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/gunzip --version # [verify] validate.sh:2156 label=gunzip-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/zstdcat --version # [verify] validate.sh:2157 label=zstdcat-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/x86_64 --version # [verify] validate.sh:2158 label=x86-64-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/uname26 --version # [verify] validate.sh:2159 label=uname26-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/tcsh --version # [verify] validate.sh:2160 label=tcsh-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/sem --version # [verify] validate.sh:2161 label=sem-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/sql --version # [verify] validate.sh:2162 label=sql-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/sha1hmac --version # [verify] validate.sh:2163 label=sha1hmac-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/sha224hmac --version # [verify] validate.sh:2164 label=sha224hmac-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/sha256hmac --version # [verify] validate.sh:2165 label=sha256hmac-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/sha384hmac --version # [verify] validate.sh:2166 label=sha384hmac-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/sha512hmac --version # [verify] validate.sh:2167 label=sha512hmac-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/sm3hmac --version # [verify] validate.sh:2168 label=sm3hmac-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/shasum --version # [verify] validate.sh:2169 label=shasum-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/sdparm --version # [verify] validate.sh:2170 label=sdparm-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/secon --version # [verify] validate.sh:2171 label=secon-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/run0 --version # [verify] validate.sh:2172 label=run0-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/scl --version # [verify] validate.sh:2173 label=scl-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/my_print_defaults --version # [verify] validate.sh:2174 label=my-print-defaults-version
hermit run --strict --verify -- /bin/echo hermit-compat # [both] validate.sh:2462 label=echo
hermit record start --data-dir "$RECORDING_DIR" -- /bin/echo hermit-compat # [both: record] validate.sh:2462 label=echo
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2462 label=echo
hermit run --strict --verify -- /usr/bin/true # [both] validate.sh:2464 label=true
hermit record start --data-dir "$RECORDING_DIR" -- /usr/bin/true # [both: record] validate.sh:2464 label=true
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2464 label=true
hermit run --strict --verify -- /usr/bin/pwd # [both] validate.sh:2466 label=pwd
hermit record start --data-dir "$RECORDING_DIR" -- /usr/bin/pwd # [both: record] validate.sh:2466 label=pwd
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2466 label=pwd
hermit run --strict --verify -- env "REAL_COMPAT_FIXTURES=$REAL_COMPAT_FIXTURES" bash "$REAL_COMPAT_WORKLOAD" seq # [both: verify variant] validate.sh:2470 label=seq
hermit record start --data-dir "$RECORDING_DIR" -- /usr/bin/seq 10 # [both: record] validate.sh:2470 label=seq
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2470 label=seq
hermit run --strict --verify -- /bin/cat README.md # [both] validate.sh:2472 label=cat
hermit record start --data-dir "$RECORDING_DIR" -- /bin/cat README.md # [both: record] validate.sh:2472 label=cat
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2472 label=cat
hermit run --strict --verify -- /usr/bin/wc -c README.md # [both] validate.sh:2474 label=wc
hermit record start --data-dir "$RECORDING_DIR" -- /usr/bin/wc -c README.md # [both: record] validate.sh:2474 label=wc
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2474 label=wc
hermit run --strict --verify -- /usr/bin/head -n 3 README.md # [both] validate.sh:2476 label=head
hermit record start --data-dir "$RECORDING_DIR" -- /usr/bin/head -n 3 README.md # [both: record] validate.sh:2476 label=head
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2476 label=head
hermit run --strict --verify -- /usr/bin/base64 README.md # [both] validate.sh:2478 label=base64
hermit record start --data-dir "$RECORDING_DIR" -- /usr/bin/base64 README.md # [both: record] validate.sh:2478 label=base64
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2478 label=base64
hermit run --strict --verify -- /usr/bin/base32 README.md # [both] validate.sh:2480 label=base32
hermit record start --data-dir "$RECORDING_DIR" -- /usr/bin/base32 README.md # [both: record] validate.sh:2480 label=base32
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2480 label=base32
hermit run --strict --verify -- /usr/bin/id -u # [both] validate.sh:2482 label=id
hermit record start --data-dir "$RECORDING_DIR" -- /usr/bin/id -u # [both: record] validate.sh:2482 label=id
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2482 label=id
hermit run --strict --verify -- bash -c 'set -euo pipefail; printf "alpha 2\nbeta 3\nalpha 5\n" | awk "\$1 == \"alpha\" { sum += \$2 } END { print sum }" | diff -u <(printf "7\n") -; printf "awk-ok\n"' # [both] validate.sh:2529 label=awk
hermit record start --data-dir "$RECORDING_DIR" -- bash -c 'set -euo pipefail; printf "alpha 2\nbeta 3\nalpha 5\n" | awk "\$1 == \"alpha\" { sum += \$2 } END { print sum }" | diff -u <(printf "7\n") -; printf "awk-ok\n"' # [both: record] validate.sh:2529 label=awk
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2529 label=awk
hermit run --strict --verify -- env "REAL_COMPAT_FIXTURES=$REAL_COMPAT_FIXTURES" bash "$REAL_COMPAT_WORKLOAD" jq # [verify] validate.sh:2535 label=jq
hermit run --strict --verify -- /usr/bin/nc -h # [verify] validate.sh:2606 label=netcat
hermit run --strict --verify -- /usr/bin/socat -h # [verify] validate.sh:2611 label=socat
hermit run --strict --verify -- env "REAL_COMPAT_FIXTURES=$REAL_COMPAT_FIXTURES" bash "$REAL_COMPAT_WORKLOAD" pkg-config # [verify] validate.sh:2628 label=pkg-config
hermit run --strict --verify -- env "REAL_COMPAT_FIXTURES=$REAL_COMPAT_FIXTURES" bash "$REAL_COMPAT_WORKLOAD" make # [both: verify variant] validate.sh:2647 label=make
hermit record start --data-dir "$RECORDING_DIR" -- make --version # [both: record] validate.sh:2647 label=make
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2647 label=make
hermit run --strict --verify -- env "REAL_COMPAT_FIXTURES=$REAL_COMPAT_FIXTURES" bash "$REAL_COMPAT_WORKLOAD" ar # [both: verify variant] validate.sh:2649 label=ar
hermit record start --data-dir "$RECORDING_DIR" -- /usr/bin/ar --version # [both: record] validate.sh:2649 label=ar
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2649 label=ar
hermit run --strict --verify -- env "REAL_COMPAT_FIXTURES=$REAL_COMPAT_FIXTURES" bash "$REAL_COMPAT_WORKLOAD" as # [both: verify variant] validate.sh:2651 label=as
hermit record start --data-dir "$RECORDING_DIR" -- /usr/bin/as --version # [both: record] validate.sh:2651 label=as
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2651 label=as
hermit run --strict --verify -- env "REAL_COMPAT_FIXTURES=$REAL_COMPAT_FIXTURES" bash "$REAL_COMPAT_WORKLOAD" ld # [both: verify variant] validate.sh:2653 label=ld
hermit record start --data-dir "$RECORDING_DIR" -- /usr/bin/ld --version # [both: record] validate.sh:2653 label=ld
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2653 label=ld
hermit run --strict --verify -- env "REAL_COMPAT_FIXTURES=$REAL_COMPAT_FIXTURES" bash "$REAL_COMPAT_WORKLOAD" nm # [both: verify variant] validate.sh:2655 label=nm
hermit record start --data-dir "$RECORDING_DIR" -- /usr/bin/nm --version # [both: record] validate.sh:2655 label=nm
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2655 label=nm
hermit run --strict --verify -- env "REAL_COMPAT_FIXTURES=$REAL_COMPAT_FIXTURES" bash "$REAL_COMPAT_WORKLOAD" objcopy # [both: verify variant] validate.sh:2657 label=objcopy
hermit record start --data-dir "$RECORDING_DIR" -- /usr/bin/objcopy --version # [both: record] validate.sh:2657 label=objcopy
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2657 label=objcopy
hermit run --strict --verify -- env "REAL_COMPAT_FIXTURES=$REAL_COMPAT_FIXTURES" bash "$REAL_COMPAT_WORKLOAD" objdump # [both: verify variant] validate.sh:2659 label=objdump
hermit record start --data-dir "$RECORDING_DIR" -- /usr/bin/objdump --version # [both: record] validate.sh:2659 label=objdump
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2659 label=objdump
hermit run --strict --verify -- env "REAL_COMPAT_FIXTURES=$REAL_COMPAT_FIXTURES" bash "$REAL_COMPAT_WORKLOAD" ranlib # [both: verify variant] validate.sh:2661 label=ranlib
hermit record start --data-dir "$RECORDING_DIR" -- /usr/bin/ranlib --version # [both: record] validate.sh:2661 label=ranlib
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2661 label=ranlib
hermit run --strict --verify -- env "REAL_COMPAT_FIXTURES=$REAL_COMPAT_FIXTURES" bash "$REAL_COMPAT_WORKLOAD" readelf # [both: verify variant] validate.sh:2663 label=readelf
hermit record start --data-dir "$RECORDING_DIR" -- /usr/bin/readelf --version # [both: record] validate.sh:2663 label=readelf
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2663 label=readelf
hermit run --strict --verify -- env "REAL_COMPAT_FIXTURES=$REAL_COMPAT_FIXTURES" bash "$REAL_COMPAT_WORKLOAD" size # [both: verify variant] validate.sh:2665 label=size
hermit record start --data-dir "$RECORDING_DIR" -- /usr/bin/size --version # [both: record] validate.sh:2665 label=size
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2665 label=size
hermit run --strict --verify -- env "REAL_COMPAT_FIXTURES=$REAL_COMPAT_FIXTURES" bash "$REAL_COMPAT_WORKLOAD" strip # [both: verify variant] validate.sh:2667 label=strip
hermit record start --data-dir "$RECORDING_DIR" -- /usr/bin/strip --version # [both: record] validate.sh:2667 label=strip
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2667 label=strip
hermit run --strict --verify -- env "REAL_COMPAT_FIXTURES=$REAL_COMPAT_FIXTURES" bash "$REAL_COMPAT_WORKLOAD" addr2line # [both: verify variant] validate.sh:2669 label=addr2line
hermit record start --data-dir "$RECORDING_DIR" -- /usr/bin/addr2line --version # [both: record] validate.sh:2669 label=addr2line
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2669 label=addr2line
hermit run --strict --verify -- env "REAL_COMPAT_FIXTURES=$REAL_COMPAT_FIXTURES" bash "$REAL_COMPAT_WORKLOAD" c++filt # [both: verify variant] validate.sh:2671 label=c++filt
hermit record start --data-dir "$RECORDING_DIR" -- /usr/bin/c++filt --version # [both: record] validate.sh:2671 label=c++filt
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2671 label=c++filt
hermit run --strict --verify -- env "REAL_COMPAT_FIXTURES=$REAL_COMPAT_FIXTURES" bash "$REAL_COMPAT_WORKLOAD" elfedit # [both: verify variant] validate.sh:2673 label=elfedit
hermit record start --data-dir "$RECORDING_DIR" -- /usr/bin/elfedit --version # [both: record] validate.sh:2673 label=elfedit
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2673 label=elfedit
hermit run --strict --verify -- env "REAL_COMPAT_FIXTURES=$REAL_COMPAT_FIXTURES" bash "$REAL_COMPAT_WORKLOAD" gprof # [both: verify variant] validate.sh:2675 label=gprof
hermit record start --data-dir "$RECORDING_DIR" -- /usr/bin/gprof --version # [both: record] validate.sh:2675 label=gprof
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2675 label=gprof
hermit run --strict --verify -- bash -c 'printf "beta\nalpha\nalpha\n" | sort' # [both] validate.sh:2738 label=sort
hermit record start --data-dir "$RECORDING_DIR" -- bash -c 'printf "beta\nalpha\nalpha\n" | sort' # [both: record] validate.sh:2738 label=sort
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2738 label=sort
hermit run --strict --verify -- bash -c 'set -euo pipefail; printf "alpha\nalpha\nbeta\nbeta\ngamma\n" | uniq -d | diff -u <(printf "alpha\nbeta\n") -; printf "uniq-ok\n"' # [both] validate.sh:2741 label=uniq
hermit record start --data-dir "$RECORDING_DIR" -- bash -c 'set -euo pipefail; printf "alpha\nalpha\nbeta\nbeta\ngamma\n" | uniq -d | diff -u <(printf "alpha\nbeta\n") -; printf "uniq-ok\n"' # [both: record] validate.sh:2741 label=uniq
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2741 label=uniq
hermit run --strict --verify -- bash -c 'printf "Hermit\n" | tr "[:upper:]" "[:lower:]"' # [both] validate.sh:2744 label=tr
hermit record start --data-dir "$RECORDING_DIR" -- bash -c 'printf "Hermit\n" | tr "[:upper:]" "[:lower:]"' # [both: record] validate.sh:2744 label=tr
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2744 label=tr
hermit run --strict --verify -- bash -c 'set -euo pipefail; printf "one:two:three\nfour:five:six\n" | cut -d: -f2 | diff -u <(printf "two\nfive\n") -; printf "cut-ok\n"' # [both] validate.sh:2747 label=cut
hermit record start --data-dir "$RECORDING_DIR" -- bash -c 'set -euo pipefail; printf "one:two:three\nfour:five:six\n" | cut -d: -f2 | diff -u <(printf "two\nfive\n") -; printf "cut-ok\n"' # [both: record] validate.sh:2747 label=cut
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2747 label=cut
hermit run --strict --verify -- bash -c 'printf "tee-through-hermit\n" | tee /dev/null' # [both] validate.sh:2750 label=tee
hermit record start --data-dir "$RECORDING_DIR" -- bash -c 'printf "tee-through-hermit\n" | tee /dev/null' # [both: record] validate.sh:2750 label=tee
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2750 label=tee
hermit run --strict --verify -- bash -c 'set -euo pipefail; paste -d: <(printf "alpha\nbeta\n") <(printf "1\n2\n") | diff -u <(printf "alpha:1\nbeta:2\n") -; printf "paste-ok\n"' # [both] validate.sh:2753 label=paste
hermit record start --data-dir "$RECORDING_DIR" -- bash -c 'set -euo pipefail; paste -d: <(printf "alpha\nbeta\n") <(printf "1\n2\n") | diff -u <(printf "alpha:1\nbeta:2\n") -; printf "paste-ok\n"' # [both: record] validate.sh:2753 label=paste
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2753 label=paste
hermit run --strict --verify -- bash -c 'set -euo pipefail; comm -12 <(printf "alpha\nbeta\n") <(printf "beta\ngamma\n") | diff -u <(printf "beta\n") -; printf "comm-ok\n"' # [both] validate.sh:2756 label=comm
hermit record start --data-dir "$RECORDING_DIR" -- bash -c 'set -euo pipefail; comm -12 <(printf "alpha\nbeta\n") <(printf "beta\ngamma\n") | diff -u <(printf "beta\n") -; printf "comm-ok\n"' # [both: record] validate.sh:2756 label=comm
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2756 label=comm
hermit run --strict --verify -- bash -c 'set -euo pipefail; join <(printf "1 alpha\n2 beta\n") <(printf "1 one\n2 two\n") | diff -u <(printf "1 alpha one\n2 beta two\n") -; printf "join-ok\n"' # [both] validate.sh:2759 label=join
hermit record start --data-dir "$RECORDING_DIR" -- bash -c 'set -euo pipefail; join <(printf "1 alpha\n2 beta\n") <(printf "1 one\n2 two\n") | diff -u <(printf "1 alpha one\n2 beta two\n") -; printf "join-ok\n"' # [both: record] validate.sh:2759 label=join
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2759 label=join
hermit run --strict --verify -- env "REAL_COMPAT_FIXTURES=$REAL_COMPAT_FIXTURES" bash "$REAL_COMPAT_WORKLOAD" find # [both: verify variant] validate.sh:2762 label=find
hermit record start --data-dir "$RECORDING_DIR" -- find /etc -maxdepth 1 # [both: record] validate.sh:2762 label=find
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2762 label=find
hermit run --strict --verify -- stat -c '%n %s %f' /etc/hostname # [both] validate.sh:2765 label=stat
hermit record start --data-dir "$RECORDING_DIR" -- stat -c '%n %s %f' /etc/hostname # [both: record] validate.sh:2765 label=stat
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2765 label=stat
hermit run --strict --verify -- file /bin/sh # [both] validate.sh:2767 label=file
hermit record start --data-dir "$RECORDING_DIR" -- file /bin/sh # [both: record] validate.sh:2767 label=file
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2767 label=file
hermit run --strict --verify -- /usr/bin/basename /usr/local/bin/hermit # [both] validate.sh:2769 label=basename
hermit record start --data-dir "$RECORDING_DIR" -- /usr/bin/basename /usr/local/bin/hermit # [both: record] validate.sh:2769 label=basename
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2769 label=basename
hermit run --strict --verify -- /usr/bin/dirname /usr/local/bin/hermit # [both] validate.sh:2771 label=dirname
hermit record start --data-dir "$RECORDING_DIR" -- /usr/bin/dirname /usr/local/bin/hermit # [both: record] validate.sh:2771 label=dirname
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2771 label=dirname
hermit run --strict --verify -- env "REAL_COMPAT_FIXTURES=$REAL_COMPAT_FIXTURES" bash "$REAL_COMPAT_WORKLOAD" env # [both: verify variant] validate.sh:2773 label=env
hermit record start --data-dir "$RECORDING_DIR" -- /usr/bin/env -i HERMIT_COMPAT=env /usr/bin/env # [both: record] validate.sh:2773 label=env
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2773 label=env
hermit run --strict --verify -- /usr/bin/env -i HERMIT_COMPAT=printenv /usr/bin/printenv HERMIT_COMPAT # [both] validate.sh:2775 label=printenv
hermit record start --data-dir "$RECORDING_DIR" -- /usr/bin/env -i HERMIT_COMPAT=printenv /usr/bin/printenv HERMIT_COMPAT # [both: record] validate.sh:2775 label=printenv
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2775 label=printenv
hermit run --strict --verify -- /usr/bin/uname -sr # [both] validate.sh:2778 label=uname
hermit record start --data-dir "$RECORDING_DIR" -- /usr/bin/uname -sr # [both: record] validate.sh:2778 label=uname
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2778 label=uname
hermit run --strict --verify -- env "REAL_COMPAT_FIXTURES=$REAL_COMPAT_FIXTURES" bash "$REAL_COMPAT_WORKLOAD" factor # [both: verify variant] validate.sh:2780 label=factor
hermit record start --data-dir "$RECORDING_DIR" -- factor 42 # [both: record] validate.sh:2780 label=factor
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2780 label=factor
hermit run --strict --verify -- expr 2 + 2 # [both] validate.sh:2782 label=expr
hermit record start --data-dir "$RECORDING_DIR" -- expr 2 + 2 # [both: record] validate.sh:2782 label=expr
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2782 label=expr
hermit run --strict --verify -- bash -c 'printf "hermit-dd\n" | dd bs=1 count=10 status=none' # [both] validate.sh:2784 label=dd
hermit record start --data-dir "$RECORDING_DIR" -- bash -c 'printf "hermit-dd\n" | dd bs=1 count=10 status=none' # [both: record] validate.sh:2784 label=dd
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2784 label=dd
hermit run --strict --verify -- /usr/bin/df -P / # [both] validate.sh:2787 label=df
hermit record start --data-dir "$RECORDING_DIR" -- /usr/bin/df -P / # [both: record] validate.sh:2787 label=df
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2787 label=df
hermit run --strict --verify -- /usr/bin/du -sk README.md # [both] validate.sh:2789 label=du
hermit record start --data-dir "$RECORDING_DIR" -- /usr/bin/du -sk README.md # [both: record] validate.sh:2789 label=du
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2789 label=du
hermit run --strict --verify -- /usr/bin/hostname # [both] validate.sh:2791 label=hostname
hermit record start --data-dir "$RECORDING_DIR" -- /usr/bin/hostname # [both: record] validate.sh:2791 label=hostname
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2791 label=hostname
hermit run --strict --verify -- env "REAL_COMPAT_FIXTURES=$REAL_COMPAT_FIXTURES" bash "$REAL_COMPAT_WORKLOAD" ip # [verify] validate.sh:2794 label=ip
hermit run --strict --verify -- env "REAL_COMPAT_FIXTURES=$REAL_COMPAT_FIXTURES" bash "$REAL_COMPAT_WORKLOAD" ss # [verify] validate.sh:2796 label=ss
hermit run --strict --verify -- env "REAL_COMPAT_FIXTURES=$REAL_COMPAT_FIXTURES" bash "$REAL_COMPAT_WORKLOAD" lscpu # [verify] validate.sh:2798 label=lscpu
hermit run --strict --verify -- env "REAL_COMPAT_FIXTURES=$REAL_COMPAT_FIXTURES" bash "$REAL_COMPAT_WORKLOAD" lsof # [verify] validate.sh:2802 label=lsof
hermit run --strict --verify -- /usr/bin/whoami # [both] validate.sh:2805 label=whoami
hermit record start --data-dir "$RECORDING_DIR" -- /usr/bin/whoami # [both: record] validate.sh:2805 label=whoami
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2805 label=whoami
hermit run --strict --verify -- /usr/bin/groups # [both] validate.sh:2807 label=groups
hermit record start --data-dir "$RECORDING_DIR" -- /usr/bin/groups # [both: record] validate.sh:2807 label=groups
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2807 label=groups
hermit run --strict --verify -- bash -c 'output=$(tty 2>&1); status=$?; printf "%s\n" "$output"; test "$status" -eq 1' # [both] validate.sh:2812 label=tty
hermit record start --data-dir "$RECORDING_DIR" -- bash -c 'output=$(tty 2>&1); status=$?; printf "%s\n" "$output"; test "$status" -eq 1' # [both: record] validate.sh:2812 label=tty
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2812 label=tty
hermit run --strict --verify -- /usr/bin/nproc # [both] validate.sh:2815 label=nproc
hermit record start --data-dir "$RECORDING_DIR" -- /usr/bin/nproc # [both: record] validate.sh:2815 label=nproc
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2815 label=nproc
hermit run --strict --verify -- /usr/bin/arch # [both] validate.sh:2817 label=arch
hermit record start --data-dir "$RECORDING_DIR" -- /usr/bin/arch # [both: record] validate.sh:2817 label=arch
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2817 label=arch
hermit run --strict --verify -- /usr/bin/realpath README.md # [both] validate.sh:2819 label=realpath
hermit record start --data-dir "$RECORDING_DIR" -- /usr/bin/realpath README.md # [both: record] validate.sh:2819 label=realpath
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2819 label=realpath
hermit run --strict --verify -- /usr/bin/readlink -f README.md # [both] validate.sh:2821 label=readlink
hermit record start --data-dir "$RECORDING_DIR" -- /usr/bin/readlink -f README.md # [both: record] validate.sh:2821 label=readlink
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2821 label=readlink
hermit run --strict --verify -- bash -c 'set -euo pipefail; d=$(mktemp -d /tmp/hermit-compat.XXXXXX); test -d "$d"; rmdir "$d"; printf "mktemp-ok\n"' # [verify] validate.sh:2824 label=mktemp
hermit run --strict --verify -- /usr/bin/sha256sum README.md # [both] validate.sh:2827 label=sha256sum
hermit record start --data-dir "$RECORDING_DIR" -- /usr/bin/sha256sum README.md # [both: record] validate.sh:2827 label=sha256sum
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2827 label=sha256sum
hermit run --strict --verify -- /usr/bin/sha1sum README.md # [both] validate.sh:2829 label=sha1sum
hermit record start --data-dir "$RECORDING_DIR" -- /usr/bin/sha1sum README.md # [both: record] validate.sh:2829 label=sha1sum
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2829 label=sha1sum
hermit run --strict --verify -- /usr/bin/md5sum README.md # [both] validate.sh:2831 label=md5sum
hermit record start --data-dir "$RECORDING_DIR" -- /usr/bin/md5sum README.md # [both: record] validate.sh:2831 label=md5sum
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2831 label=md5sum
hermit run --strict --verify -- /usr/bin/sha224sum README.md # [both] validate.sh:2833 label=sha224sum
hermit record start --data-dir "$RECORDING_DIR" -- /usr/bin/sha224sum README.md # [both: record] validate.sh:2833 label=sha224sum
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2833 label=sha224sum
hermit run --strict --verify -- /usr/bin/sha384sum README.md # [both] validate.sh:2835 label=sha384sum
hermit record start --data-dir "$RECORDING_DIR" -- /usr/bin/sha384sum README.md # [both: record] validate.sh:2835 label=sha384sum
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2835 label=sha384sum
hermit run --strict --verify -- /usr/bin/sha512sum README.md # [both] validate.sh:2837 label=sha512sum
hermit record start --data-dir "$RECORDING_DIR" -- /usr/bin/sha512sum README.md # [both: record] validate.sh:2837 label=sha512sum
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2837 label=sha512sum
hermit run --strict --verify -- /usr/bin/wc -l README.md # [both] validate.sh:2839 label=wc-lines
hermit record start --data-dir "$RECORDING_DIR" -- /usr/bin/wc -l README.md # [both: record] validate.sh:2839 label=wc-lines
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2839 label=wc-lines
hermit run --strict --verify -- bash -c 'printf "alpha\nbeta\n" | nl -ba' # [both] validate.sh:2841 label=nl
hermit record start --data-dir "$RECORDING_DIR" -- bash -c 'printf "alpha\nbeta\n" | nl -ba' # [both: record] validate.sh:2841 label=nl
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2841 label=nl
hermit run --strict --verify -- bash -c 'printf "a\tb\n" | expand -t 4' # [both] validate.sh:2844 label=expand
hermit record start --data-dir "$RECORDING_DIR" -- bash -c 'printf "a\tb\n" | expand -t 4' # [both: record] validate.sh:2844 label=expand
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2844 label=expand
hermit run --strict --verify -- bash -c 'printf "a   b\n" | unexpand -a -t 4' # [both] validate.sh:2847 label=unexpand
hermit record start --data-dir "$RECORDING_DIR" -- bash -c 'printf "a   b\n" | unexpand -a -t 4' # [both: record] validate.sh:2847 label=unexpand
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2847 label=unexpand
hermit run --strict --verify -- /usr/bin/test 42 -eq 42 # [both] validate.sh:2850 label=test
hermit record start --data-dir "$RECORDING_DIR" -- /usr/bin/test 42 -eq 42 # [both: record] validate.sh:2850 label=test
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2850 label=test
hermit run --strict --verify -- '/usr/bin/[' 42 -eq 42 ']' # [both] validate.sh:2852 label=bracket
hermit record start --data-dir "$RECORDING_DIR" -- '/usr/bin/[' 42 -eq 42 ']' # [both: record] validate.sh:2852 label=bracket
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2852 label=bracket
hermit run --strict --verify -- /usr/bin/printf '%s=%d\n' hermit 42 # [both] validate.sh:2854 label=printf
hermit record start --data-dir "$RECORDING_DIR" -- /usr/bin/printf '%s=%d\n' hermit 42 # [both: record] validate.sh:2854 label=printf
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2854 label=printf
hermit run --strict --verify -- /usr/bin/pr -t README.md # [both] validate.sh:2856 label=pr
hermit record start --data-dir "$RECORDING_DIR" -- /usr/bin/pr -t README.md # [both: record] validate.sh:2856 label=pr
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2856 label=pr
hermit run --strict --verify -- /usr/bin/ls -1 README.md # [both] validate.sh:2858 label=ls
hermit record start --data-dir "$RECORDING_DIR" -- /usr/bin/ls -1 README.md # [both: record] validate.sh:2858 label=ls
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2858 label=ls
hermit run --strict --verify -- env "REAL_COMPAT_FIXTURES=$REAL_COMPAT_FIXTURES" bash "$REAL_COMPAT_WORKLOAD" xargs # [both: verify variant] validate.sh:2860 label=xargs
hermit record start --data-dir "$RECORDING_DIR" -- bash -c 'printf "one\ntwo\n" | /usr/bin/xargs -n1 /bin/echo' # [both: record] validate.sh:2860 label=xargs
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2860 label=xargs
hermit run --strict --verify -- env "REAL_COMPAT_FIXTURES=$REAL_COMPAT_FIXTURES" bash "$REAL_COMPAT_WORKLOAD" time # [verify] validate.sh:2864 label=time
hermit run --strict --verify -- bash -c 'printf "hermit\n" | /usr/bin/iconv -f UTF-8 -t UTF-8' # [both] validate.sh:2867 label=iconv
hermit record start --data-dir "$RECORDING_DIR" -- bash -c 'printf "hermit\n" | /usr/bin/iconv -f UTF-8 -t UTF-8' # [both: record] validate.sh:2867 label=iconv
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2867 label=iconv
hermit run --strict --verify -- /usr/bin/sleep 0 # [both] validate.sh:2870 label=sleep
hermit record start --data-dir "$RECORDING_DIR" -- /usr/bin/sleep 0 # [both: record] validate.sh:2870 label=sleep
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2870 label=sleep
hermit run --strict --verify -- /usr/bin/stdbuf -o0 /usr/bin/printf 'stdbuf-ok\n' # [both] validate.sh:2872 label=stdbuf
hermit record start --data-dir "$RECORDING_DIR" -- /usr/bin/stdbuf -o0 /usr/bin/printf 'stdbuf-ok\n' # [both: record] validate.sh:2872 label=stdbuf
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2872 label=stdbuf
hermit run --strict --verify -- /usr/bin/nohup /bin/echo nohup-ok # [both] validate.sh:2875 label=nohup
hermit record start --data-dir "$RECORDING_DIR" -- /usr/bin/nohup /bin/echo nohup-ok # [both: record] validate.sh:2875 label=nohup
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2875 label=nohup
hermit run --strict --verify -- /usr/bin/nice -n 1 /bin/echo nice-ok # [both] validate.sh:2877 label=nice
hermit record start --data-dir "$RECORDING_DIR" -- /usr/bin/nice -n 1 /bin/echo nice-ok # [both: record] validate.sh:2877 label=nice
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2877 label=nice
hermit run --strict --verify -- /usr/bin/ionice -c 3 /bin/echo ionice-ok # [both] validate.sh:2879 label=ionice
hermit record start --data-dir "$RECORDING_DIR" -- /usr/bin/ionice -c 3 /bin/echo ionice-ok # [both: record] validate.sh:2879 label=ionice
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2879 label=ionice
hermit run --strict --verify -- bash -c 'set -euo pipefail; taskset -p $$ >/dev/null; printf "taskset-ok\n"' # [both] validate.sh:2883 label=taskset
hermit record start --data-dir "$RECORDING_DIR" -- bash -c 'set -euo pipefail; taskset -p $$ >/dev/null; printf "taskset-ok\n"' # [both: record] validate.sh:2883 label=taskset
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2883 label=taskset
hermit run --strict --verify -- bash -c 'set -euo pipefail; chrt -p $$ >/dev/null; printf "chrt-ok\n"' # [both] validate.sh:2887 label=chrt
hermit record start --data-dir "$RECORDING_DIR" -- bash -c 'set -euo pipefail; chrt -p $$ >/dev/null; printf "chrt-ok\n"' # [both: record] validate.sh:2887 label=chrt
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2887 label=chrt
hermit run --strict --verify -- bash -c 'set -euo pipefail; f=$(mktemp); flock -x "$f" -c "printf \"flock-ok\\n\""; rm -f "$f"' # [both] validate.sh:2890 label=flock
hermit record start --data-dir "$RECORDING_DIR" -- bash -c 'set -euo pipefail; f=$(mktemp); flock -x "$f" -c "printf \"flock-ok\\n\""; rm -f "$f"' # [both: record] validate.sh:2890 label=flock
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2890 label=flock
hermit run --strict --verify -- bash -c 'set -euo pipefail; output=$(/usr/bin/logger --stderr --no-act -t hermit-compat logger-ok 2>&1); [[ $output == *"hermit-compat: logger-ok" ]]; printf "logger-ok\n"' # [both] validate.sh:2894 label=logger
hermit record start --data-dir "$RECORDING_DIR" -- bash -c 'set -euo pipefail; output=$(/usr/bin/logger --stderr --no-act -t hermit-compat logger-ok 2>&1); [[ $output == *"hermit-compat: logger-ok" ]]; printf "logger-ok\n"' # [both: record] validate.sh:2894 label=logger
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2894 label=logger
hermit run --strict --verify -- /usr/bin/getopt -o ab: -- -a -b value # [both] validate.sh:2897 label=getopt
hermit record start --data-dir "$RECORDING_DIR" -- /usr/bin/getopt -o ab: -- -a -b value # [both: record] validate.sh:2897 label=getopt
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2897 label=getopt
hermit run --strict --verify -- bash -c 'set -euo pipefail; printf "alpha:1\nbeta:22\n" | column -t -s :' # [both] validate.sh:2899 label=column
hermit record start --data-dir "$RECORDING_DIR" -- bash -c 'set -euo pipefail; printf "alpha:1\nbeta:22\n" | column -t -s :' # [both: record] validate.sh:2899 label=column
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2899 label=column
hermit run --strict --verify -- bash -c 'set -euo pipefail; printf "Hermit\n" | hexdump -C' # [both] validate.sh:2902 label=hexdump
hermit record start --data-dir "$RECORDING_DIR" -- bash -c 'set -euo pipefail; printf "Hermit\n" | hexdump -C' # [both: record] validate.sh:2902 label=hexdump
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2902 label=hexdump
hermit run --strict --verify -- bash -c 'set -euo pipefail; printf "Hermit\n" | xxd' # [both] validate.sh:2905 label=xxd
hermit record start --data-dir "$RECORDING_DIR" -- bash -c 'set -euo pipefail; printf "Hermit\n" | xxd' # [both: record] validate.sh:2905 label=xxd
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2905 label=xxd
hermit run --strict --verify -- bash -c 'set -euo pipefail; printf "\0Hermit\0" | strings -n 5' # [both] validate.sh:2908 label=strings
hermit record start --data-dir "$RECORDING_DIR" -- bash -c 'set -euo pipefail; printf "\0Hermit\0" | strings -n 5' # [both: record] validate.sh:2908 label=strings
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2908 label=strings
hermit run --strict --verify -- bash -c 'set -euo pipefail; printf "Hermit\n" | od -An -tx1' # [both] validate.sh:2911 label=od
hermit record start --data-dir "$RECORDING_DIR" -- bash -c 'set -euo pipefail; printf "Hermit\n" | od -An -tx1' # [both: record] validate.sh:2911 label=od
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2911 label=od
hermit run --strict --verify -- /usr/bin/sum README.md # [both] validate.sh:2914 label=sum
hermit record start --data-dir "$RECORDING_DIR" -- /usr/bin/sum README.md # [both: record] validate.sh:2914 label=sum
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2914 label=sum
hermit run --strict --verify -- /usr/bin/cksum README.md # [both] validate.sh:2916 label=cksum
hermit record start --data-dir "$RECORDING_DIR" -- /usr/bin/cksum README.md # [both: record] validate.sh:2916 label=cksum
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2916 label=cksum
hermit run --strict --verify -- /usr/bin/b2sum README.md # [both] validate.sh:2918 label=b2sum
hermit record start --data-dir "$RECORDING_DIR" -- /usr/bin/b2sum README.md # [both: record] validate.sh:2918 label=b2sum
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2918 label=b2sum
hermit run --strict --verify -- bash -c 'set -euo pipefail; printf "alpha beta\nbeta gamma\n" | tsort' # [both] validate.sh:2920 label=tsort
hermit record start --data-dir "$RECORDING_DIR" -- bash -c 'set -euo pipefail; printf "alpha beta\nbeta gamma\n" | tsort' # [both: record] validate.sh:2920 label=tsort
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2920 label=tsort
hermit run --strict --verify -- bash -c 'set -euo pipefail; printf "alpha beta\n" | ptx -f' # [both] validate.sh:2923 label=ptx
hermit record start --data-dir "$RECORDING_DIR" -- bash -c 'set -euo pipefail; printf "alpha beta\n" | ptx -f' # [both: record] validate.sh:2923 label=ptx
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2923 label=ptx
hermit run --strict --verify -- /usr/bin/pinky -l root # [both] validate.sh:2926 label=pinky
hermit record start --data-dir "$RECORDING_DIR" -- /usr/bin/pinky -l root # [both: record] validate.sh:2926 label=pinky
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2926 label=pinky
hermit run --strict --verify -- bash -c 'if output=$(/usr/bin/logname 2>/dev/null); then test -n "$output"; printf "logname:login-present\n"; else printf "logname:no-login-record\n"; fi' # [both] validate.sh:2929 label=logname
hermit record start --data-dir "$RECORDING_DIR" -- bash -c 'if output=$(/usr/bin/logname 2>/dev/null); then test -n "$output"; printf "logname:login-present\n"; else printf "logname:no-login-record\n"; fi' # [both: record] validate.sh:2929 label=logname
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2929 label=logname
hermit run --strict --verify -- /usr/bin/users # [both] validate.sh:2932 label=users
hermit record start --data-dir "$RECORDING_DIR" -- /usr/bin/users # [both: record] validate.sh:2932 label=users
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2932 label=users
hermit run --strict --verify -- /usr/bin/uptime -p # [both] validate.sh:2934 label=uptime
hermit record start --data-dir "$RECORDING_DIR" -- /usr/bin/uptime -p # [both: record] validate.sh:2934 label=uptime
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2934 label=uptime
hermit run --strict --verify -- /usr/bin/iostat -d -x 1 1 # [verify] validate.sh:2936 label=iostat
hermit run --strict --verify -- /usr/bin/vmstat -d 1 2 # [verify] validate.sh:2938 label=vmstat-disk
hermit run --strict --verify -- /usr/bin/pidstat -d -p 1 1 1 # [verify] validate.sh:2940 label=pidstat-disk
hermit run --strict --verify -- /usr/bin/findmnt --kernel --list --output TARGET,SOURCE,FSTYPE,OPTIONS # [verify] validate.sh:2942 label=findmnt
hermit run --strict --verify -- /usr/sbin/sysctl kernel.random.uuid # [verify] validate.sh:2945 label=sysctl-random-uuid
hermit run --strict --verify -- /usr/bin/sar -v 1 1 # [verify] validate.sh:2947 label=sar-resource-tables
hermit run --strict --verify -- /usr/bin/lsirq --noheadings --output IRQ,TOTAL,NAME # [verify] validate.sh:2949 label=lsirq
hermit run --strict --verify -- /usr/bin/mpstat -I SCPU 1 1 # [verify] validate.sh:2952 label=mpstat-softirqs
hermit run --strict --verify -- /usr/sbin/lsmod # [verify] validate.sh:2954 label=lsmod
hermit run --strict --verify -- /usr/bin/numastat # [verify] validate.sh:2956 label=numastat
hermit run --strict --verify -- /usr/bin/numactl --hardware # [verify] validate.sh:2958 label=numactl-hardware
hermit run --strict --verify -- /usr/bin/sensors --version # [verify] validate.sh:2961 label=sensors-version
hermit run --strict --verify -- /usr/bin/ps aux # [verify] validate.sh:2963 label=ps
hermit run --strict --verify -- /usr/bin/vmstat -s # [verify] validate.sh:2965 label=vmstat
hermit run --strict --verify -- /usr/bin/env "HOME=$top_home" "XDG_CONFIG_HOME=$top_config_home" /bin/bash -c 'set -euo pipefail; LC_ALL=C /usr/bin/top -b -n 1 -p $$ -w 80 >/dev/null; printf "top-ok\n"' # [verify] validate.sh:2971 label=top
hermit run --strict --verify -- /usr/bin/kill -0 1 # [verify] validate.sh:2977 label=kill
hermit run --strict --verify -- bash -c 'set -euo pipefail; /usr/bin/pgrep -x bash | /usr/bin/grep -qx "$$"; printf "pgrep-ok\n"' # [verify] validate.sh:2980 label=pgrep
hermit run --strict --verify -- bash -c 'set -euo pipefail; /usr/bin/pkill -0 -x bash; printf "pkill-ok\n"' # [verify] validate.sh:2983 label=pkill
hermit run --strict --verify -- /usr/bin/timeout 1 /usr/bin/true # [verify] validate.sh:2990 label=timeout
hermit run --strict --verify -- bash -c 'set -euo pipefail; f=$(mktemp); printf "Hermit\n" >"$f"; /usr/bin/truncate -s 4096 "$f"; test "$(stat -c %s "$f")" = 4096; /usr/bin/truncate -s 7 "$f"; test "$(stat -c %s "$f")" = 7; test "$(cat "$f")" = Hermit; rm -f "$f"; printf "truncate:4096-to-7-ok\n"' # [verify] validate.sh:2996 label=truncate
hermit run --strict --verify -- bash -c 'set -euo pipefail; d=$(mktemp -d); trap '"'"'rm -rf "$d"'"'"' EXIT; f="$d/file"; /usr/bin/fallocate -l 4096 "$f"; size=$(stat -c %s "$f"); test "$size" = 4096; printf "fallocate:size=%s\n" "$size"' # [verify] validate.sh:3003 label=fallocate
hermit run --strict --verify -- bash -c 'set -euo pipefail; f=$(mktemp); trap '"'"'rm -f "$f"'"'"' EXIT; printf payload >"$f"; /usr/bin/setfattr -n user.hermit.compat -v 42 "$f"; value=$(/usr/bin/getfattr --absolute-names --only-values -n user.hermit.compat "$f"); test "$value" = 42; /usr/bin/setfattr -x user.hermit.compat "$f"; ! /usr/bin/getfattr --absolute-names --only-values -n user.hermit.compat "$f" >/dev/null 2>&1; printf "setfattr:value=%s:removed\n" "$value"' # [verify] validate.sh:3006 label=setfattr
hermit run --strict --verify -- bash -c 'set -euo pipefail; f=$(mktemp); trap '"'"'rm -f "$f"'"'"' EXIT; chmod 600 "$f"; /usr/bin/setfacl -m u::rw,g::r,o::- "$f"; mode=$(stat -c %a "$f"); test "$mode" = 640; /usr/bin/getfacl --absolute-names -cp "$f" | /usr/bin/grep -Fxq "group::r--"; printf "setfacl:mode=%s\n" "$mode"' # [verify] validate.sh:3009 label=setfacl
hermit run --strict --verify -- bash -c 'set -euo pipefail; /usr/bin/mountpoint -q /; ! /usr/bin/mountpoint -q README.md; printf "mountpoint:root=yes:file=no\n"' # [verify] validate.sh:3012 label=mountpoint
hermit run --strict --verify -- bash -c 'set -euo pipefail; d=$(mktemp -d); trap '"'"'rm -rf "$d"'"'"' EXIT; printf "alpha\nbeta\n" >"$d/base"; printf "alpha\nbeta\nours\n" >"$d/ours"; printf "theirs\nalpha\nbeta\n" >"$d/theirs"; /usr/bin/diff3 -m "$d/ours" "$d/base" "$d/theirs" | /usr/bin/diff -u <(printf "theirs\nalpha\nbeta\nours\n") -; printf "diff3:clean-merge-ok\n"' # [verify] validate.sh:3015 label=diff3
hermit run --strict --verify -- bash -c 'set -euo pipefail; encoded=$(printf "Hermit\n" | /usr/bin/basenc --base64url); test "$encoded" = SGVybWl0Cg==; decoded=$(printf "%s\n" "$encoded" | /usr/bin/basenc --base64url -d); test "$decoded" = Hermit; printf "basenc:%s:roundtrip-ok\n" "$encoded"' # [verify] validate.sh:3022 label=basenc
hermit run --strict --verify -- bash -c 'set -euo pipefail; f=$(mktemp); trap '"'"'rm -f "$f"'"'"' EXIT; printf "alpha\r\nbeta\r\n" >"$f"; /usr/bin/dos2unix -q "$f"; /usr/bin/diff -u <(printf "alpha\nbeta\n") "$f"; printf "dos2unix:crlf-to-lf-ok\n"' # [verify] validate.sh:3025 label=dos2unix
hermit run --strict --verify -- bash -c 'set -euo pipefail; output=$(HERMIT_NAME=Hermit HERMIT_VALUE=42 /usr/bin/envsubst "\$HERMIT_NAME=\$HERMIT_VALUE" <<<"\$HERMIT_NAME=\$HERMIT_VALUE"); test "$output" = Hermit=42; printf "envsubst:%s\n" "$output"' # [verify] validate.sh:3028 label=envsubst
hermit run --strict --verify -- bash -c 'set -euo pipefail; output=$(printf "A\bB\n" | /usr/bin/col -b); test "$output" = B; printf "col:overstrike=%s\n" "$output"' # [verify] validate.sh:3031 label=col
hermit run --strict --verify -- bash -c 'set -euo pipefail; output=$(printf "abcdef\n" | /usr/bin/colrm 3 5); test "$output" = abf; printf "colrm:%s\n" "$output"' # [verify] validate.sh:3034 label=colrm
hermit run --strict --verify -- bash -c 'set -euo pipefail; f=$(mktemp); trap '"'"'rm -f "$f"'"'"' EXIT; printf "Hermit\n" >"$f"; sum=$(/usr/bin/crc32 "$f"); test "$sum" = 146f43bb; printf "crc32:%s\n" "$sum"' # [verify] validate.sh:3037 label=crc32
hermit run --strict --verify -- bash -c 'set -euo pipefail; value=$(/usr/bin/uuidgen --random); [[ $value =~ ^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$ ]]; printf "uuidgen:%s\n" "$value"' # [verify] validate.sh:3044 label=uuidgen
hermit run --strict --verify -- bash -c 'set -euo pipefail; f=$(mktemp); trap '"'"'rm -f "$f"'"'"' EXIT; printf seed >"$f"; /usr/bin/shred -n 1 -z -s 4096 "$f"; test "$(stat -c %s "$f")" = 4096; /usr/bin/cmp -n 4096 "$f" /dev/zero; printf "shred:4096-zeroed\n"' # [verify] validate.sh:3047 label=shred
hermit run --strict --verify -- bash -c 'set -euo pipefail; f=$(mktemp); trap '"'"'rm -f "$f"'"'"' EXIT; printf "sync-payload\n" >"$f"; /usr/bin/sync -f "$f"; test "$(cat "$f")" = sync-payload; printf "sync:file-ok\n"' # [verify] validate.sh:3050 label=sync
hermit run --strict --verify -- bash -c 'set -euo pipefail; /usr/bin/pathchk -p alpha/beta_42; component=$(printf "%015d" 0 | /usr/bin/tr 0 x); ! /usr/bin/pathchk -p "$component" >/dev/null 2>&1; printf "pathchk:portable-limit-ok\n"' # [verify] validate.sh:3053 label=pathchk
hermit run --strict --verify -- bash -c 'set -euo pipefail; d=$(mktemp -d); trap '"'"'rm -rf "$d"'"'"' EXIT; mkdir "$d/real"; touch "$d/real/file"; ln -s real "$d/link"; /usr/bin/namei -m "$d/link/file" >"$d/output"; /usr/bin/grep -Fxq " lrwxrwxrwx link -> real" "$d/output"; /usr/bin/grep -Fxq " -rw-r--r-- file" "$d/output"; printf "namei:symlink-path-ok\n"' # [verify] validate.sh:3056 label=namei
hermit run --strict --verify -- bash -c 'set -euo pipefail; page=$(/usr/bin/getconf PAGESIZE); bits=$(/usr/bin/getconf LONG_BIT); test "$page:$bits" = 4096:64; printf "getconf:page=%s:long=%s\n" "$page" "$bits"' # [verify] validate.sh:3059 label=getconf
hermit run --strict --verify -- bash -c 'set -euo pipefail; d=$(mktemp -d); trap '"'"'rm -rf "$d"'"'"' EXIT; printf "int compat_add(int a, int b) { return a + b; }\nint main(void) { return compat_add(20, 22) != 42; }\n" >"$d/fixture.c"; printf "fixture.c\n" >"$d/cscope.files"; (cd "$d" && /usr/bin/cscope -bq -i cscope.files); output=$(cd "$d" && /usr/bin/cscope -dL -1 compat_add); [[ $output == *"fixture.c compat_add 1"* ]]; printf "cscope:compat_add-found\n"' # [verify] validate.sh:3066 label=cscope
hermit run --strict --verify -- bash -c 'set -euo pipefail; d=$(mktemp -d); trap '"'"'rm -rf "$d"'"'"' EXIT; printf "%s\n" "%option prefix=\"compat\" noyywrap" "%%" "[0-9]+ return 1;" ".      ;" "%%" >"$d/scanner.l"; /usr/bin/flex -o "$d/scanner.c" "$d/scanner.l"; grep -q compatlex "$d/scanner.c"; printf "flex:compat-scanner-generated\n"' # [verify] validate.sh:3069 label=flex
hermit run --strict --verify -- bash -c 'set -euo pipefail; d=$(mktemp -d); trap '"'"'rm -rf "$d"'"'"' EXIT; printf "%s\n" "msgid \"\"" "msgstr \"Content-Type: text/plain; charset=UTF-8\\n\"" "" "msgid \"hello\"" "msgstr \"Hermit\"" >"$d/messages.po"; /usr/bin/msgfmt -o "$d/messages.mo" "$d/messages.po"; test -s "$d/messages.mo"; printf "msgfmt:catalog-compiled\n"' # [verify] validate.sh:3072 label=msgfmt
hermit run --strict --verify -- bash -c 'set -euo pipefail; d=$(mktemp -d); trap '"'"'rm -rf "$d"'"'"' EXIT; printf "%s\n" "msgid \"\"" "msgstr \"Content-Type: text/plain; charset=UTF-8\\n\"" "" "msgid \"hello\"" "msgstr \"Hermit\"" >"$d/messages.po"; /usr/bin/msgfmt -o "$d/messages.mo" "$d/messages.po"; /usr/bin/msgunfmt "$d/messages.mo" >"$d/roundtrip.po"; grep -Fq '"'"'msgstr "Hermit"'"'"' "$d/roundtrip.po"; printf "msgunfmt:catalog-roundtrip-ok\n"' # [verify] validate.sh:3075 label=msgunfmt
hermit run --strict --verify -- bash -c 'set -euo pipefail; d=$(mktemp -d); printf "alpha\nbeta\n" >"$d/a"; cp "$d/a" "$d/b"; diff -u "$d/a" "$d/b"; rm -rf "$d"; printf "diff-ok\n"' # [both] validate.sh:3085 label=diff
hermit record start --data-dir "$RECORDING_DIR" -- bash -c 'set -euo pipefail; d=$(mktemp -d); printf "alpha\nbeta\n" >"$d/a"; cp "$d/a" "$d/b"; diff -u "$d/a" "$d/b"; rm -rf "$d"; printf "diff-ok\n"' # [both: record] validate.sh:3085 label=diff
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:3085 label=diff
hermit run --strict --verify -- bash -c 'set -euo pipefail; d=$(mktemp -d); printf "old\n" >"$d/file"; printf "%s\n" "--- file" "+++ file" "@@ -1 +1 @@" "-old" "+new" | (cd "$d" && patch -s file); cat "$d/file"; rm -rf "$d"' # [verify] validate.sh:3088 label=patch
hermit run --strict --verify -- bash -c 'set -euo pipefail; printf "alpha\nbeta\ngamma\nalpha\n" | grep -nx alpha | diff -u <(printf "1:alpha\n4:alpha\n") -; printf "grep-ok\n"' # [both] validate.sh:3091 label=grep
hermit record start --data-dir "$RECORDING_DIR" -- bash -c 'set -euo pipefail; printf "alpha\nbeta\ngamma\nalpha\n" | grep -nx alpha | diff -u <(printf "1:alpha\n4:alpha\n") -; printf "grep-ok\n"' # [both: record] validate.sh:3091 label=grep
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:3091 label=grep
hermit run --strict --verify -- bash -c 'set -euo pipefail; printf "alpha\nbeta\ngamma\n" | egrep "^(alpha|gamma)$" | diff -u <(printf "alpha\ngamma\n") -; printf "egrep-ok\n"' # [both] validate.sh:3094 label=egrep
hermit record start --data-dir "$RECORDING_DIR" -- bash -c 'set -euo pipefail; printf "alpha\nbeta\ngamma\n" | egrep "^(alpha|gamma)$" | diff -u <(printf "alpha\ngamma\n") -; printf "egrep-ok\n"' # [both: record] validate.sh:3094 label=egrep
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:3094 label=egrep
hermit run --strict --verify -- bash -c 'set -euo pipefail; printf "alpha.beta\nalphaXbeta\n" | fgrep "alpha.beta" | diff -u <(printf "alpha.beta\n") -; printf "fgrep-ok\n"' # [both] validate.sh:3097 label=fgrep
hermit record start --data-dir "$RECORDING_DIR" -- bash -c 'set -euo pipefail; printf "alpha.beta\nalphaXbeta\n" | fgrep "alpha.beta" | diff -u <(printf "alpha.beta\n") -; printf "fgrep-ok\n"' # [both: record] validate.sh:3097 label=fgrep
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:3097 label=fgrep
hermit run --strict --verify -- bash -c 'set -euo pipefail; printf "alpha:12\nbeta:3\n" | sed -E "s/^([a-z]+):([0-9]+)$/\\2-\\1/" | diff -u <(printf "12-alpha\n3-beta\n") -; printf "sed-ok\n"' # [both] validate.sh:3100 label=sed
hermit record start --data-dir "$RECORDING_DIR" -- bash -c 'set -euo pipefail; printf "alpha:12\nbeta:3\n" | sed -E "s/^([a-z]+):([0-9]+)$/\\2-\\1/" | diff -u <(printf "12-alpha\n3-beta\n") -; printf "sed-ok\n"' # [both: record] validate.sh:3100 label=sed
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:3100 label=sed
hermit run --strict --verify -- bash -c 'set -euo pipefail; d=$(mktemp -d); printf "copy-data\n" >"$d/source"; cp "$d/source" "$d/copy"; cmp "$d/source" "$d/copy"; cat "$d/copy"; rm -rf "$d"' # [both] validate.sh:3106 label=cp
hermit record start --data-dir "$RECORDING_DIR" -- bash -c 'set -euo pipefail; d=$(mktemp -d); printf "copy-data\n" >"$d/source"; cp "$d/source" "$d/copy"; cmp "$d/source" "$d/copy"; cat "$d/copy"; rm -rf "$d"' # [both: record] validate.sh:3106 label=cp
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:3106 label=cp
hermit run --strict --verify -- bash -c 'set -euo pipefail; d=$(mktemp -d); printf "move-data\n" >"$d/source"; mv "$d/source" "$d/moved"; test ! -e "$d/source"; cat "$d/moved"; rm -rf "$d"' # [both] validate.sh:3109 label=mv
hermit record start --data-dir "$RECORDING_DIR" -- bash -c 'set -euo pipefail; d=$(mktemp -d); printf "move-data\n" >"$d/source"; mv "$d/source" "$d/moved"; test ! -e "$d/source"; cat "$d/moved"; rm -rf "$d"' # [both: record] validate.sh:3109 label=mv
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:3109 label=mv
hermit run --strict --verify -- bash -c 'set -euo pipefail; d=$(mktemp -d); printf "remove-data\n" >"$d/file"; rm "$d/file"; test ! -e "$d/file"; rmdir "$d"; printf "rm-ok\n"' # [both] validate.sh:3112 label=rm
hermit record start --data-dir "$RECORDING_DIR" -- bash -c 'set -euo pipefail; d=$(mktemp -d); printf "remove-data\n" >"$d/file"; rm "$d/file"; test ! -e "$d/file"; rmdir "$d"; printf "rm-ok\n"' # [both: record] validate.sh:3112 label=rm
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:3112 label=rm
hermit run --strict --verify -- bash -c 'set -euo pipefail; d=$(mktemp -d); mkdir -p "$d/a/b"; test -d "$d/a/b"; printf "mkdir-ok\n"; rm -rf "$d"' # [both] validate.sh:3115 label=mkdir
hermit record start --data-dir "$RECORDING_DIR" -- bash -c 'set -euo pipefail; d=$(mktemp -d); mkdir -p "$d/a/b"; test -d "$d/a/b"; printf "mkdir-ok\n"; rm -rf "$d"' # [both: record] validate.sh:3115 label=mkdir
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:3115 label=mkdir
hermit run --strict --verify -- bash -c 'set -euo pipefail; d=$(mktemp -d); rmdir "$d"; test ! -e "$d"; printf "rmdir-ok\n"' # [both] validate.sh:3118 label=rmdir
hermit record start --data-dir "$RECORDING_DIR" -- bash -c 'set -euo pipefail; d=$(mktemp -d); rmdir "$d"; test ! -e "$d"; printf "rmdir-ok\n"' # [both: record] validate.sh:3118 label=rmdir
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:3118 label=rmdir
hermit run --strict --verify -- bash -c 'set -euo pipefail; f=$(mktemp); touch -t 200001010000 "$f"; stat -c "%Y %s" "$f"; rm -f "$f"' # [both] validate.sh:3121 label=touch
hermit record start --data-dir "$RECORDING_DIR" -- bash -c 'set -euo pipefail; f=$(mktemp); touch -t 200001010000 "$f"; stat -c "%Y %s" "$f"; rm -f "$f"' # [both: record] validate.sh:3121 label=touch
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:3121 label=touch
hermit run --strict --verify -- bash -c 'set -euo pipefail; f=$(mktemp); printf "mode\n" >"$f"; chmod 640 "$f"; stat -c "%a" "$f"; rm -f "$f"' # [both] validate.sh:3124 label=chmod
hermit record start --data-dir "$RECORDING_DIR" -- bash -c 'set -euo pipefail; f=$(mktemp); printf "mode\n" >"$f"; chmod 640 "$f"; stat -c "%a" "$f"; rm -f "$f"' # [both: record] validate.sh:3124 label=chmod
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:3124 label=chmod
hermit run --strict --verify -- bash -c 'set -euo pipefail; f=$(mktemp); printf "owner\n" >"$f"; chown --reference=README.md "$f"; stat -c "%u:%g" "$f"; rm -f "$f"' # [verify] validate.sh:3127 label=chown
hermit run --strict --verify -- bash -c 'set -euo pipefail; d=$(mktemp -d); printf "link-data\n" >"$d/source"; ln "$d/source" "$d/hard"; ln -s source "$d/sym"; stat -c "%h" "$d/source"; cat "$d/hard" "$d/sym"; rm -rf "$d"' # [verify] validate.sh:3130 label=ln
hermit run --strict --verify -- /usr/bin/date -u +%Y-%m-%dT%H:%M:%SZ # [both] validate.sh:3133 label=date
hermit record start --data-dir "$RECORDING_DIR" -- /usr/bin/date -u +%Y-%m-%dT%H:%M:%SZ # [both: record] validate.sh:3133 label=date
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:3133 label=date
hermit run --strict --verify -- /usr/bin/cal 1 2000 # [both] validate.sh:3135 label=cal
hermit record start --data-dir "$RECORDING_DIR" -- /usr/bin/cal 1 2000 # [both: record] validate.sh:3135 label=cal
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:3135 label=cal
hermit run --strict --verify -- bash -c 'set -eu; yes hermit | head -n 3' # [both] validate.sh:3137 label=yes
hermit record start --data-dir "$RECORDING_DIR" -- bash -c 'set -eu; yes hermit | head -n 3' # [both: record] validate.sh:3137 label=yes
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:3137 label=yes
hermit run --strict --verify -- bash -c 'set -euo pipefail; printf "first\nsecond\nthird\n" | tac' # [both] validate.sh:3140 label=tac
hermit record start --data-dir "$RECORDING_DIR" -- bash -c 'set -euo pipefail; printf "first\nsecond\nthird\n" | tac' # [both: record] validate.sh:3140 label=tac
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:3140 label=tac
hermit run --strict --verify -- bash -c 'set -euo pipefail; printf "Hermit\ndeterminism\n" | rev' # [both] validate.sh:3143 label=rev
hermit record start --data-dir "$RECORDING_DIR" -- bash -c 'set -euo pipefail; printf "Hermit\ndeterminism\n" | rev' # [both: record] validate.sh:3143 label=rev
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:3143 label=rev
hermit run --strict --verify -- bash -c 'set -euo pipefail; printf "abcdefghijklmnopqrstuvwxyz\n" | fold -w 8' # [both] validate.sh:3146 label=fold
hermit record start --data-dir "$RECORDING_DIR" -- bash -c 'set -euo pipefail; printf "abcdefghijklmnopqrstuvwxyz\n" | fold -w 8' # [both: record] validate.sh:3146 label=fold
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:3146 label=fold
hermit run --strict --verify -- bash -c 'set -euo pipefail; printf "Hermit formats this deterministic paragraph into narrow lines for validation.\n" | fmt -w 24' # [both] validate.sh:3149 label=fmt
hermit record start --data-dir "$RECORDING_DIR" -- bash -c 'set -euo pipefail; printf "Hermit formats this deterministic paragraph into narrow lines for validation.\n" | fmt -w 24' # [both: record] validate.sh:3149 label=fmt
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:3149 label=fmt
hermit run --strict --verify -- env "REAL_COMPAT_FIXTURES=$REAL_COMPAT_FIXTURES" bash "$REAL_COMPAT_WORKLOAD" shuf # [both: verify variant] validate.sh:3152 label=shuf
hermit record start --data-dir "$RECORDING_DIR" -- bash -c 'set -euo pipefail; output=$(printf "alpha\nbeta\ngamma\ndelta\n" | shuf | sort); test "$output" = "$(printf "alpha\nbeta\ndelta\ngamma\n")"; printf "shuf-ok\n"' # [both: record] validate.sh:3152 label=shuf
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:3152 label=shuf
hermit run --strict --verify -- /usr/bin/numfmt --to=iec 1048576 # [both] validate.sh:3155 label=numfmt
hermit record start --data-dir "$RECORDING_DIR" -- /usr/bin/numfmt --to=iec 1048576 # [both: record] validate.sh:3155 label=numfmt
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:3155 label=numfmt
hermit run --strict --verify -- bash -c 'set -euo pipefail; d=$(mktemp -d); printf "alpha\nbeta\ngamma\n" >"$d/input"; (cd "$d" && csplit -s input "/^beta$/" && cat xx00 xx01); rm -rf "$d"' # [verify] validate.sh:3157 label=csplit
hermit run --strict --verify -- bash -c 'set -euo pipefail; d=$(mktemp -d); printf "one\ntwo\nthree\nfour\n" >"$d/input"; split -l 2 "$d/input" "$d/part-"; cat "$d"/part-*; rm -rf "$d"' # [both] validate.sh:3160 label=split
hermit record start --data-dir "$RECORDING_DIR" -- bash -c 'set -euo pipefail; d=$(mktemp -d); printf "one\ntwo\nthree\nfour\n" >"$d/input"; split -l 2 "$d/input" "$d/part-"; cat "$d"/part-*; rm -rf "$d"' # [both: record] validate.sh:3160 label=split
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:3160 label=split
hermit run --strict --verify -- bash -c 'set -euo pipefail; d=$(mktemp -d); install -m 640 README.md "$d/copied"; stat -c "%a %s" "$d/copied"; rm -rf "$d"' # [both] validate.sh:3163 label=install
hermit record start --data-dir "$RECORDING_DIR" -- bash -c 'set -euo pipefail; d=$(mktemp -d); install -m 640 README.md "$d/copied"; stat -c "%a %s" "$d/copied"; rm -rf "$d"' # [both: record] validate.sh:3163 label=install
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:3163 label=install
hermit run --strict --verify -- bash -c 'set -euo pipefail; p=$(mktemp -u); mkfifo "$p"; stat -c "%F" "$p"; rm -f "$p"' # [both] validate.sh:3166 label=mkfifo
hermit record start --data-dir "$RECORDING_DIR" -- bash -c 'set -euo pipefail; p=$(mktemp -u); mkfifo "$p"; stat -c "%F" "$p"; rm -f "$p"' # [both: record] validate.sh:3166 label=mkfifo
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:3166 label=mkfifo
hermit run --strict --verify -- bash -c 'set -euo pipefail; d=$(mktemp -d); printf "same\n" >"$d/a"; printf "same\n" >"$d/b"; cmp -s "$d/a" "$d/b"; printf "cmp-ok\n"; rm -rf "$d"' # [both] validate.sh:3170 label=cmp
hermit record start --data-dir "$RECORDING_DIR" -- bash -c 'set -euo pipefail; d=$(mktemp -d); printf "same\n" >"$d/a"; printf "same\n" >"$d/b"; cmp -s "$d/a" "$d/b"; printf "cmp-ok\n"; rm -rf "$d"' # [both: record] validate.sh:3170 label=cmp
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:3170 label=cmp
hermit run --strict --verify -- /usr/bin/free -m # [both] validate.sh:3173 label=free
hermit record start --data-dir "$RECORDING_DIR" -- /usr/bin/free -m # [both: record] validate.sh:3173 label=free
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:3173 label=free
hermit run --base-env=minimal --no-virtualize-cpuid --max-timeslice=disabled -- /bin/echo "$SMOKE_MARKER" # [run] validate.sh:710 hermit_echo
hermit run --base-env=minimal --no-virtualize-cpuid --max-timeslice=disabled --verify -- /bin/echo "$SMOKE_MARKER" # [verify] validate.sh:758 hermit_verify_smoke
hermit record start --data-dir "$RECORDING_DIR" -- /bin/echo "$SMOKE_MARKER" # [record/replay] validate.sh:772 hermit_record_replay_smoke record
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [record/replay] validate.sh:775 hermit_record_replay_smoke replay
hermit run --backend dbi -- /bin/true # [run] validate.sh:790 dbi_backend_available
hermit run --backend liteinst --no-namespace -- /bin/true # [run] validate.sh:796 liteinst_backend_available
hermit run --strict --verify -- /bin/echo "$SUPER_MARKER" # [verify] validate.sh:1027 ptrace-strict-verify
hermit run --strict --verify -- bash -c 'yes hermit | head -n 64 | sha256sum' # [verify] validate.sh:1031 ptrace-pipeline
hermit record start --verify --data-dir "$RECORDING_DIR" -- /bin/echo "$SUPER_RECORD_MARKER" # [record/replay] validate.sh:1037 ptrace-record-replay
hermit run --backend kvm --verify -- /bin/echo "$SUPER_KVM_MARKER" # [verify] validate.sh:1044 kvm-verify
hermit run --backend dbi --verify -- /bin/echo "$SUPER_DBI_MARKER" # [verify] validate.sh:1048 dbi-verify
hermit run --base-env=minimal --no-virtualize-cpuid --max-timeslice=disabled --strict -- /bin/true # [run] validate.sh:3508 envelope true L1
hermit run --base-env=minimal --no-virtualize-cpuid --max-timeslice=disabled --strict --verify -- /bin/true # [verify] validate.sh:3508 envelope true L2/L4
hermit run --base-env=minimal --no-virtualize-cpuid --max-timeslice=disabled --strict --verify --detlog-heap --detlog-stack -- /bin/true # [verify] validate.sh:3508 envelope true L3
hermit record start --verify -- /bin/true # [record/replay] validate.sh:3556/3772 envelope true rr
hermit run --base-env=minimal --no-virtualize-cpuid --max-timeslice=disabled --strict -- /bin/echo hermit-envelope # [run] validate.sh:3508 envelope echo L1
hermit run --base-env=minimal --no-virtualize-cpuid --max-timeslice=disabled --strict --verify -- /bin/echo hermit-envelope # [verify] validate.sh:3508 envelope echo L2/L4
hermit run --base-env=minimal --no-virtualize-cpuid --max-timeslice=disabled --strict --verify --detlog-heap --detlog-stack -- /bin/echo hermit-envelope # [verify] validate.sh:3508 envelope echo L3
hermit record start --verify -- /bin/echo hermit-envelope # [record/replay] validate.sh:3556/3772 envelope echo rr
hermit run --base-env=minimal --no-virtualize-cpuid --max-timeslice=disabled --strict -- /bin/date -u +%Y # [run] validate.sh:3508 envelope date L1
hermit run --base-env=minimal --no-virtualize-cpuid --max-timeslice=disabled --strict --verify -- /bin/date -u +%Y # [verify] validate.sh:3508 envelope date L2/L4
hermit run --base-env=minimal --no-virtualize-cpuid --max-timeslice=disabled --strict --verify --detlog-heap --detlog-stack -- /bin/date -u +%Y # [verify] validate.sh:3508 envelope date L3
hermit record start --verify -- /bin/date -u +%Y # [record/replay] validate.sh:3556/3772 envelope date rr
hermit record start --verify --record-timeout=30 "--data-dir=$RECORDING_DIR" -- /bin/echo hello # [record/replay] hermit-cli/tests/record_replay.rs explicit-1
hermit record start --verify --record-timeout=30 "--data-dir=$RECORDING_DIR" -- /bin/sh -c '/usr/bin/yes | /usr/bin/head -n 1' # [record/replay] hermit-cli/tests/record_replay.rs explicit-3
hermit record start --verify --record-timeout=30 "--data-dir=$RECORDING_DIR" -- /bin/sh -c 'printf '"'"'b\na\n'"'"' | /usr/bin/sort' # [record/replay] hermit-cli/tests/record_replay.rs explicit-4
hermit record start --verify --record-timeout=30 "--data-dir=$RECORDING_DIR" -- /usr/bin/head -c 262144 /dev/zero # [record/replay] hermit-cli/tests/record_replay.rs explicit-5
hermit record start --verify --record-timeout=30 "--data-dir=$RECORDING_DIR" -- /bin/sh -c 'while :; do :; done' # [record/replay] hermit-cli/tests/record_replay.rs explicit-9

# Compression/archive
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/tar --version # [verify] validate.sh:1366 label=tar
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/gzip --version # [verify] validate.sh:1367 label=gzip
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/bzip2 --version # [verify] validate.sh:1450 label=bzip2
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/zstd --version # [verify] validate.sh:1451 label=zstd
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/cpio --version # [verify] validate.sh:1452 label=cpio
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/zip -v # [verify] validate.sh:1453 label=zip
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/unzip -v # [verify] validate.sh:1454 label=unzip
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/bzip2 -c README.md # [verify] validate.sh:1500 label=bzip2-stream
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/gzip -cn README.md # [verify] validate.sh:1514 label=gzip-stream
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/tar -cf - README.md # [verify] validate.sh:1515 label=tar-stream
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/zip -q - README.md # [verify] validate.sh:1516 label=zip-stream
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/eu-elfcompress --version # [verify] validate.sh:1674 label=eu-elfcompress-version
hermit run --strict --verify -- bash -c 'bzip2 -c README.md | sha256sum' # [both] validate.sh:2681 label=bzip2
hermit record start --data-dir "$RECORDING_DIR" -- bash -c 'bzip2 -c README.md | sha256sum' # [both: record] validate.sh:2681 label=bzip2
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2681 label=bzip2
hermit run --strict --verify -- bash -c 'gzip -cn README.md | sha256sum' # [both] validate.sh:2684 label=gzip
hermit record start --data-dir "$RECORDING_DIR" -- bash -c 'gzip -cn README.md | sha256sum' # [both: record] validate.sh:2684 label=gzip
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2684 label=gzip
hermit run --strict --verify -- bash -c 'xz -c README.md | sha256sum' # [both] validate.sh:2687 label=xz
hermit record start --data-dir "$RECORDING_DIR" -- bash -c 'xz -c README.md | sha256sum' # [both: record] validate.sh:2687 label=xz
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2687 label=xz
hermit run --strict --verify -- bash -c 'zstd -q -c README.md | sha256sum' # [both] validate.sh:2690 label=zstd
hermit record start --data-dir "$RECORDING_DIR" -- bash -c 'zstd -q -c README.md | sha256sum' # [both: record] validate.sh:2690 label=zstd
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2690 label=zstd
hermit run --strict --verify -- env "REAL_COMPAT_FIXTURES=$REAL_COMPAT_FIXTURES" bash "$REAL_COMPAT_WORKLOAD" gzip-roundtrip # [verify] validate.sh:2699 label=gzip-roundtrip
hermit run --strict --verify -- env "REAL_COMPAT_FIXTURES=$REAL_COMPAT_FIXTURES" bash "$REAL_COMPAT_WORKLOAD" bzip2-roundtrip # [verify] validate.sh:2701 label=bzip2-roundtrip
hermit run --strict --verify -- env "REAL_COMPAT_FIXTURES=$REAL_COMPAT_FIXTURES" bash "$REAL_COMPAT_WORKLOAD" xz-roundtrip # [verify] validate.sh:2703 label=xz-roundtrip
hermit run --strict --verify -- env "REAL_COMPAT_FIXTURES=$REAL_COMPAT_FIXTURES" bash "$REAL_COMPAT_WORKLOAD" zstd-roundtrip # [verify] validate.sh:2705 label=zstd-roundtrip
hermit run --strict --verify -- env "REAL_COMPAT_FIXTURES=$REAL_COMPAT_FIXTURES" bash "$REAL_COMPAT_WORKLOAD" tar-roundtrip # [verify] validate.sh:2707 label=tar-roundtrip
hermit run --strict --verify -- env "REAL_COMPAT_FIXTURES=$REAL_COMPAT_FIXTURES" bash "$REAL_COMPAT_WORKLOAD" cpio-roundtrip # [verify] validate.sh:2709 label=cpio-roundtrip
hermit run --strict --verify -- env "REAL_COMPAT_FIXTURES=$REAL_COMPAT_FIXTURES" bash "$REAL_COMPAT_WORKLOAD" gzip-roundtrip # [verify] validate.sh:2716 label=gzip-roundtrip
hermit run --strict --verify -- env "REAL_COMPAT_FIXTURES=$REAL_COMPAT_FIXTURES" bash "$REAL_COMPAT_WORKLOAD" bzip2-roundtrip # [verify] validate.sh:2718 label=bzip2-roundtrip
hermit run --strict --verify -- env "REAL_COMPAT_FIXTURES=$REAL_COMPAT_FIXTURES" bash "$REAL_COMPAT_WORKLOAD" xz-roundtrip # [verify] validate.sh:2720 label=xz-roundtrip
hermit run --strict --verify -- env "REAL_COMPAT_FIXTURES=$REAL_COMPAT_FIXTURES" bash "$REAL_COMPAT_WORKLOAD" zstd-roundtrip # [verify] validate.sh:2722 label=zstd-roundtrip
hermit run --strict --verify -- env "REAL_COMPAT_FIXTURES=$REAL_COMPAT_FIXTURES" bash "$REAL_COMPAT_WORKLOAD" tar-roundtrip # [verify] validate.sh:2724 label=tar-roundtrip
hermit run --strict --verify -- env "REAL_COMPAT_FIXTURES=$REAL_COMPAT_FIXTURES" bash "$REAL_COMPAT_WORKLOAD" cpio-roundtrip # [verify] validate.sh:2726 label=cpio-roundtrip
hermit run --strict --verify -- bash -c 'set -euo pipefail; rm -rf /tmp/hermit-compat-zip; mkdir /tmp/hermit-compat-zip; printf "archive-data\n" >/tmp/hermit-compat-zip/input; touch -t 200001010000 /tmp/hermit-compat-zip/input; (cd /tmp/hermit-compat-zip && zip -q archive.zip input); unzip -Z1 /tmp/hermit-compat-zip/archive.zip; unzip -p /tmp/hermit-compat-zip/archive.zip input; rm -rf /tmp/hermit-compat-zip' # [verify] validate.sh:2733 label=zip-unzip
hermit run --strict --verify -- bash -c 'set -euo pipefail; d=$(mktemp -d); printf "archive-data\n" >"$d/input"; touch -t 200001010000 "$d/input"; tar -cf "$d/archive.tar" -C "$d" input; tar -tf "$d/archive.tar"; rm -rf "$d"' # [both] validate.sh:3103 label=tar
hermit record start --data-dir "$RECORDING_DIR" -- bash -c 'set -euo pipefail; d=$(mktemp -d); printf "archive-data\n" >"$d/input"; touch -t 200001010000 "$d/input"; tar -cf "$d/archive.tar" -C "$d" input; tar -tf "$d/archive.tar"; rm -rf "$d"' # [both: record] validate.sh:3103 label=tar
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:3103 label=tar

# Language runtimes
hermit --log=off run --strict --verify "--env=HOME=$HOME_DIR" "--env=XDG_CONFIG_HOME=$HOME_DIR/.config" -- /usr/bin/python3 -c 'import os; d=os.open('"'"'/'"'"', os.O_RDONLY); print(os.readlink('"'"'/proc/self/ns/pid'"'"', dir_fd=d))' # [verify] hermit-cli/tests/command_strict_verify.rs case=python-pid-ns
hermit --log=off run --strict --verify "--env=HOME=$HOME_DIR" "--env=XDG_CONFIG_HOME=$HOME_DIR/.config" -- /usr/bin/perl -e 'print readlink('"'"'/proc/self/ns/user'"'"'), qq(\n)' # [verify] hermit-cli/tests/command_strict_verify.rs case=perl-user-ns
hermit --log=off run --strict --verify "--env=HOME=$HOME_DIR" "--env=XDG_CONFIG_HOME=$HOME_DIR/.config" -- /usr/bin/perl -e 'print join('"'"','"'"', map { $_ * $_ } 1..5), qq(\n)' # [verify] hermit-cli/tests/command_strict_verify.rs case=perl-squares
hermit --log=off run --strict --verify "--env=HOME=$HOME_DIR" "--env=XDG_CONFIG_HOME=$HOME_DIR/.config" -- /usr/bin/python3 -c 'import resource; print(resource.getrlimit(resource.RLIMIT_NOFILE))' # [verify] hermit-cli/tests/command_strict_verify.rs case=python-prlimit
hermit --log=off run --strict --verify "--env=HOME=$HOME_DIR" "--env=XDG_CONFIG_HOME=$HOME_DIR/.config" -- /usr/bin/python3 -c 'import os; print(os.urandom(16).hex())' # [verify] hermit-cli/tests/command_strict_verify.rs case=python-getrandom
hermit run --strict --verify --no-virtualize-cpuid --max-timeslice=disabled -- /usr/bin/java -Xint -XX:+UseSerialGC -XX:ActiveProcessorCount=1 -version # [verify] hermit-cli/tests/app_strict_verify.rs java_version
hermit run --strict -- /usr/bin/ruby --disable-gems "$REPO/tests/runtime/random.rb" # [run] hermit-cli/tests/language_runtime_determinism.rs ruby
hermit run --strict -- /usr/bin/node "$REPO/tests/runtime/random.js" # [run] hermit-cli/tests/language_runtime_determinism.rs node
hermit run --strict -- /usr/bin/java -Xint -XX:ActiveProcessorCount=1 -cp "$RUNTIME_BUILD" RuntimeRandom # [run] hermit-cli/tests/language_runtime_determinism.rs JVM
hermit run --strict -- /usr/bin/python3 -S -I "$REPO/tests/runtime/random.py" # [run] hermit-cli/tests/language_runtime_determinism.rs CPython
hermit run --strict --verify --panic-on-unsupported-syscalls -- /usr/bin/ruby --disable-gems -e 'thread = Thread.new { Thread.current.name = "hermit-ruby"; 42 }; raise "bad" unless thread.value == 42' # [verify] hermit-cli/tests/language_runtime_determinism.rs Ruby threads
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/python3 -c 'print(42)' # [verify] validate.sh:1336 label=python3
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/perl -e 'print 42, chr(10)' # [verify] validate.sh:1337 label=perl
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/gcc --version # [verify] validate.sh:1345 label=gcc
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/g++ --version # [verify] validate.sh:1346 label=g++
hermit run --backend liteinst --no-namespace --strict --verify -- /bin/bash -c 'printf "bash-ok\n"' # [verify] validate.sh:1396 label=bash
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/lua -e 'print(42)' # [verify] validate.sh:1412 label=lua
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/dc -e '2 2 + p' # [verify] validate.sh:1413 label=dc
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/m4 --version # [verify] validate.sh:1422 label=m4
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/cpp --version # [verify] validate.sh:1425 label=cpp
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/gcov --version # [verify] validate.sh:1426 label=gcov
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/clang --version # [verify] validate.sh:1459 label=clang
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/bc --version # [verify] validate.sh:1460 label=bc
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/tclsh /dev/null # [verify] validate.sh:1461 label=tcl
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/clang -E -x c /dev/null # [verify] validate.sh:1493 label=clang-preprocess
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/clang-cl --version # [verify] validate.sh:1727 label=clang-cl-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/clang-cpp --version # [verify] validate.sh:1728 label=clang-cpp-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/clang-scan-deps --version # [verify] validate.sh:1729 label=clang-scan-deps-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/gcov-dump --version # [verify] validate.sh:1796 label=gcov-dump-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/gcov-tool --version # [verify] validate.sh:1797 label=gcov-tool-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/clang-22 --version # [verify] validate.sh:1818 label=clang-22-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/clang-cl-22 --version # [verify] validate.sh:1820 label=clang-cl-22-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/clang-cpp-22 --version # [verify] validate.sh:1821 label=clang-cpp-22-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/clang-scan-deps-22 --version # [verify] validate.sh:1822 label=clang-scan-deps-22-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/aarch64-linux-gnu-cpp --version # [verify] validate.sh:1906 label=aarch64-linux-gnu-cpp-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/aarch64-linux-gnu-gcc --version # [verify] validate.sh:1909 label=aarch64-linux-gnu-gcc-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/aarch64-linux-gnu-gcov --version # [verify] validate.sh:1910 label=aarch64-linux-gnu-gcov-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/aarch64-linux-gnu-gcov-dump --version # [verify] validate.sh:1911 label=aarch64-linux-gnu-gcov-dump-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/aarch64-linux-gnu-gcov-tool --version # [verify] validate.sh:1912 label=aarch64-linux-gnu-gcov-tool-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/x86_64-linux-gnu-cpp --version # [verify] validate.sh:1934 label=x86-64-linux-gnu-cpp-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/x86_64-linux-gnu-gcc --version # [verify] validate.sh:1937 label=x86-64-linux-gnu-gcc-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/x86_64-linux-gnu-gcov --version # [verify] validate.sh:1938 label=x86-64-linux-gnu-gcov-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/x86_64-linux-gnu-gcov-dump --version # [verify] validate.sh:1939 label=x86-64-linux-gnu-gcov-dump-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/x86_64-linux-gnu-gcov-tool --version # [verify] validate.sh:1940 label=x86-64-linux-gnu-gcov-tool-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/x86_64-redhat-linux-gcc --version # [verify] validate.sh:1961 label=x86-64-redhat-linux-gcc-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/x86_64-redhat-linux-gcc-11 --version # [verify] validate.sh:1962 label=x86-64-redhat-linux-gcc-11-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/python3.9 --version # [verify] validate.sh:2080 label=python39-version
hermit run --strict --verify -- bash -c 'set -euo pipefail; out=$("$1" -e "$2"); test "$out" = "$3"; printf "lua-fib=%s\n" "$out"' bash /usr/bin/lua 'local a,b=0,1; for i=1,30 do a,b=b,a+b end; print(a)' 832040 # [verify] validate.sh:2487 label=lua
hermit run --strict --verify -- bash -c 'set -euo pipefail; out=$("$1" -e "$2"); test "$out" = "$3"; printf "perl-prime-sum=%s\n" "$out"' bash /usr/bin/perl 'my $sum=0; OUTER: for my $n (2..100) { for my $d (2..int(sqrt($n))) { next OUTER if $n % $d == 0 } $sum += $n } print "$sum\n"' 1060 # [verify] validate.sh:2492 label=perl
hermit run --strict --verify -- bash -c 'set -euo pipefail; out=$(printf "%s\n" "$2" | BC_LINE_LENGTH=200 "$1" -q); test "$out" = "$3"; printf "bc-math=%s\n" "$out"' bash /usr/bin/bc 'define f(n) { auto r,i; r=1; for(i=2;i<=n;i++) r*=i; return(r) }; scale=50; print f(20), " ", sqrt(2), "\n"' '2432902008176640000 1.41421356237309504880168872420969807856967187537694' # [verify] validate.sh:2500 label=bc
hermit record start --data-dir "$RECORDING_DIR" -- lua -e 'print(42)' # [record/replay] validate.sh:2507 label=lua
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [record/replay] validate.sh:2507 label=lua
hermit record start --data-dir "$RECORDING_DIR" -- perl -e 'print 42, chr(10)' # [record/replay] validate.sh:2509 label=perl
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [record/replay] validate.sh:2509 label=perl
hermit record start --data-dir "$RECORDING_DIR" -- bash -c 'printf "6*7\n" | bc' # [record/replay] validate.sh:2511 label=bc
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [record/replay] validate.sh:2511 label=bc
hermit run --strict --verify -- bash -c 'set -euo pipefail; out=$(printf "%s\n" "$2" | "$1"); test "$out" = "$3"; printf "tcl-squares=%s\n" "$out"' bash /usr/bin/tclsh 'set sum 0; for {set i 1} {$i <= 100} {incr i} {set sum [expr {$sum + $i*$i}]}; puts $sum' 338350 # [verify] validate.sh:2517 label=tcl
hermit run --strict --verify -- bash -c 'set -euo pipefail; out=$(printf "%s\n" "$2" | "$1"); test "$out" = "$3"; printf "dc-math=%s\n" "$out"' bash /usr/bin/dc '2 100 ^ 1 - n [ ]P 4 13 497 | p' '1267650600228229401496703205375 445' # [verify] validate.sh:2522 label=dc
hermit run --strict --verify -- bash -c 'for i in 1 2 3; do echo "$i"; done' # [both] validate.sh:2544 label=bash
hermit record start --data-dir "$RECORDING_DIR" -- bash -c 'for i in 1 2 3; do echo "$i"; done' # [both: record] validate.sh:2544 label=bash
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2544 label=bash
hermit run --strict --verify -- bash "$COMPLEX_SHELL_WORKLOAD" "$shell_build_dir" # [verify] validate.sh:2556 label=shell-build
hermit run --strict --verify -- env "REAL_COMPAT_FIXTURES=$REAL_COMPAT_FIXTURES" bash "$REAL_COMPAT_WORKLOAD" cargo # [verify] validate.sh:2561 label=cargo
hermit run --strict --verify -- env "REAL_COMPAT_FIXTURES=$REAL_COMPAT_FIXTURES" bash "$REAL_COMPAT_WORKLOAD" rustc # [verify] validate.sh:2566 label=rustc
hermit run --strict --verify -- env "REAL_COMPAT_FIXTURES=$REAL_COMPAT_FIXTURES" bash "$REAL_COMPAT_WORKLOAD" clang # [verify] validate.sh:2570 label=clang
hermit run --strict --verify -- env "REAL_COMPAT_FIXTURES=$REAL_COMPAT_FIXTURES" bash "$REAL_COMPAT_WORKLOAD" javac # [verify] validate.sh:2575 label=javac
hermit run --strict --verify -- env "REAL_COMPAT_FIXTURES=$REAL_COMPAT_FIXTURES" bash "$REAL_COMPAT_WORKLOAD" java # [both: verify variant] validate.sh:2582 label=java
hermit record start --data-dir "$RECORDING_DIR" -- java -Xint -XX:+UseSerialGC -XX:ActiveProcessorCount=1 -version # [both: record] validate.sh:2582 label=java
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2582 label=java
hermit run --strict --verify -- /usr/bin/ruby --disable-gems -e 'values = (1..5).map { |value| value * value }; raise "unexpected squares" unless values == [1, 4, 9, 16, 25]; puts values.join(",")' # [both] validate.sh:2586 label=ruby
hermit record start --data-dir "$RECORDING_DIR" -- /usr/bin/ruby --disable-gems -e 'values = (1..5).map { |value| value * value }; raise "unexpected squares" unless values == [1, 4, 9, 16, 25]; puts values.join(",")' # [both: record] validate.sh:2586 label=ruby
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2586 label=ruby
hermit run --strict --verify -- /bin/node -e 'console.log(42)' # [both] validate.sh:2592 label=node
hermit record start --data-dir "$RECORDING_DIR" -- /bin/node -e 'console.log(42)' # [both: record] validate.sh:2592 label=node
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2592 label=node
hermit run --strict --verify -- /usr/bin/python3 -c 'print(42)' # [both] validate.sh:2596 label=python3
hermit record start --data-dir "$RECORDING_DIR" -- /usr/bin/python3 -c 'print(42)' # [both: record] validate.sh:2596 label=python3
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2596 label=python3
hermit run --strict --verify -- env "REAL_COMPAT_FIXTURES=$REAL_COMPAT_FIXTURES" bash "$REAL_COMPAT_WORKLOAD" m4 # [verify] validate.sh:2630 label=m4
hermit run --strict --verify -- env "REAL_COMPAT_FIXTURES=$REAL_COMPAT_FIXTURES" bash "$REAL_COMPAT_WORKLOAD" gcc # [verify] validate.sh:2635 label=gcc
hermit record start --data-dir "$RECORDING_DIR" -- gcc --version # [record/replay] validate.sh:2642 label=gcc
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [record/replay] validate.sh:2642 label=gcc
hermit run --strict --verify -- env "REAL_COMPAT_FIXTURES=$REAL_COMPAT_FIXTURES" bash "$REAL_COMPAT_WORKLOAD" g++ # [both: verify variant] validate.sh:2645 label=g++
hermit record start --data-dir "$RECORDING_DIR" -- g++ --version # [both: record] validate.sh:2645 label=g++
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2645 label=g++
hermit run --strict --verify -- env "REAL_COMPAT_FIXTURES=$REAL_COMPAT_FIXTURES" bash "$REAL_COMPAT_WORKLOAD" cpp # [both: verify variant] validate.sh:2677 label=cpp
hermit record start --data-dir "$RECORDING_DIR" -- /usr/bin/cpp --version # [both: record] validate.sh:2677 label=cpp
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2677 label=cpp
hermit run --strict --verify -- env "REAL_COMPAT_FIXTURES=$REAL_COMPAT_FIXTURES" bash "$REAL_COMPAT_WORKLOAD" gcov # [both: verify variant] validate.sh:2679 label=gcov
hermit record start --data-dir "$RECORDING_DIR" -- /usr/bin/gcov --version # [both: record] validate.sh:2679 label=gcov
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2679 label=gcov
hermit record start --verify --record-timeout=30 "--data-dir=$RECORDING_DIR" -- /bin/bash -c 'set -euo pipefail; root=/tmp/hermit-record-mkdir-side-effect; rm -rf "$root"; mkdir "$root"; rmdir "$root"' # [record/replay] hermit-cli/tests/record_replay.rs explicit-2
hermit record start --verify --record-timeout=30 "--data-dir=$RECORDING_DIR" -- /usr/bin/node -e 'console.log(42)' # [record/replay] hermit-cli/tests/record_replay.rs explicit-7

# Applications
hermit --log=off run --strict --verify "--env=HOME=$HOME_DIR" "--env=XDG_CONFIG_HOME=$HOME_DIR/.config" -- /usr/bin/sqlite3 :memory: 'CREATE TABLE t(v); INSERT INTO t VALUES(3),(1),(2); SELECT group_concat(v, '"'"','"'"') FROM (SELECT v FROM t ORDER BY v);' # [verify] hermit-cli/tests/command_strict_verify.rs case=sqlite3
hermit run --strict --verify --no-virtualize-cpuid --max-timeslice=disabled -- /usr/bin/curl --version # [verify] hermit-cli/tests/app_strict_verify.rs curl_version
hermit run --strict --verify --no-virtualize-cpuid --max-timeslice=disabled -- /usr/sbin/nginx -v # [verify] hermit-cli/tests/app_strict_verify.rs nginx_version
hermit run --strict --verify --no-virtualize-cpuid --max-timeslice=disabled -- /usr/bin/redis-server --version # [verify] hermit-cli/tests/app_strict_verify.rs redis_server_version
hermit --log off run -- /bin/sh "$REPO/hermit-cli/tests/fixtures/redis-strict/workload.sh" /usr/bin/redis-server /usr/bin/redis-cli "$MODE" "$INSTANCE" "$PORT" # [run] hermit-cli/tests/redis_strict.rs small/extended
hermit --log=off run --strict --base-env=minimal -- "$HERMIT_LEVELDB_BUILD_DIR/c_test" # [run] hermit-cli/tests/leveldb.rs c_test
hermit --log=off run --strict --base-env=minimal -- "$HERMIT_LEVELDB_BUILD_DIR/leveldb_tests" "--gtest_filter=$FOCUSED_FILTER" # [run] hermit-cli/tests/leveldb.rs focused suite
hermit --log off run -- /usr/bin/sqlite3 "$DATABASE" "$SQL" # [run] hermit-cli/tests/sqlite_veryquick.rs fast subset
hermit --log=off run --base-env=minimal --no-virtualize-cpuid --max-timeslice=disabled "--bind=$FIXTURE:/tmp/integration-matrix" -- "$PROGRAM" "$ARGS" # [run] hermit-cli/tests/integration_matrix.rs echo/ls/cat/sqlite/python/node/java/git/nginx
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/sqlite3 :memory: 'SELECT 1+1;' # [verify] validate.sh:1339 label=sqlite3
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/openssl dgst -sha256 /etc/hostname # [verify] validate.sh:1348 label=openssl
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/git --version # [verify] validate.sh:1364 label=git
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/cmake --version # [verify] validate.sh:1365 label=cmake
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/xmllint --version # [verify] validate.sh:1456 label=xmllint
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/curl --version # [verify] validate.sh:1457 label=curl
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/wget --version # [verify] validate.sh:1458 label=wget
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/pkgconf --version # [verify] validate.sh:1504 label=pkgconf
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/git hash-object README.md # [verify] validate.sh:1517 label=git-hash
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/cmake -E sha256sum README.md # [verify] validate.sh:1518 label=cmake-sha
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/cmake -E echo cmake-ok # [verify] validate.sh:1542 label=cmake-echo
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/pkgconf --modversion zlib # [verify] validate.sh:1543 label=pkgconf-zlib
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/git rev-parse --is-inside-work-tree # [verify] validate.sh:1544 label=git-inside
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/redis-cli --version # [verify] validate.sh:1673 label=redis-cli-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/redis-server --version # [verify] validate.sh:1817 label=redis-server-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/postgres --version # [verify] validate.sh:1957 label=postgres-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/redis-benchmark --version # [verify] validate.sh:2084 label=redis-benchmark-version
hermit run --backend liteinst --no-namespace --strict --verify -- /usr/bin/sqliterepo_c --version # [verify] validate.sh:2104 label=sqliterepo-version
hermit run --strict --verify -- env "REAL_COMPAT_FIXTURES=$REAL_COMPAT_FIXTURES" bash "$REAL_COMPAT_WORKLOAD" sqlite3 # [both: verify variant] validate.sh:2532 label=sqlite3
hermit record start --data-dir "$RECORDING_DIR" -- sqlite3 :memory: 'CREATE TABLE values_under_test(value INTEGER NOT NULL); WITH RECURSIVE sequence(value) AS (VALUES(1) UNION ALL SELECT value + 1 FROM sequence WHERE value < 100) INSERT INTO values_under_test SELECT value FROM sequence; SELECT count(*), sum(value) FROM values_under_test;' # [both: record] validate.sh:2532 label=sqlite3
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2532 label=sqlite3
hermit run --strict --verify -- env "REAL_COMPAT_FIXTURES=$REAL_COMPAT_FIXTURES" bash "$REAL_COMPAT_WORKLOAD" xmllint # [verify] validate.sh:2539 label=xmllint
hermit run --strict --verify -- /usr/bin/curl --fail --silent --show-error file:///etc/hostname # [verify] validate.sh:2598 label=curl
hermit run --strict --verify -- /usr/bin/wget --version # [verify] validate.sh:2604 label=wget
hermit run --strict --verify -- env "REAL_COMPAT_FIXTURES=$REAL_COMPAT_FIXTURES" bash "$REAL_COMPAT_WORKLOAD" git # [both: verify variant] validate.sh:2624 label=git
hermit record start --data-dir "$RECORDING_DIR" -- /usr/local/bin/git.meta.real --version # [both: record] validate.sh:2624 label=git
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2624 label=git
hermit run --strict --verify -- env "REAL_COMPAT_FIXTURES=$REAL_COMPAT_FIXTURES" bash "$REAL_COMPAT_WORKLOAD" cmake # [verify] validate.sh:2626 label=cmake
hermit run --strict --verify -- env "REAL_COMPAT_FIXTURES=$REAL_COMPAT_FIXTURES" bash "$REAL_COMPAT_WORKLOAD" wget-localhost # [verify] validate.sh:2711 label=wget-localhost
hermit run --strict --verify -- env "REAL_COMPAT_FIXTURES=$REAL_COMPAT_FIXTURES" bash "$REAL_COMPAT_WORKLOAD" curl-localhost # [verify] validate.sh:2713 label=curl-localhost
hermit run --strict --verify -- env "REAL_COMPAT_FIXTURES=$REAL_COMPAT_FIXTURES" bash "$REAL_COMPAT_WORKLOAD" wget-localhost # [verify] validate.sh:2728 label=wget-localhost
hermit run --strict --verify -- env "REAL_COMPAT_FIXTURES=$REAL_COMPAT_FIXTURES" bash "$REAL_COMPAT_WORKLOAD" curl-localhost # [verify] validate.sh:2730 label=curl-localhost
hermit run --strict --verify -- openssl dgst -sha256 /etc/hostname # [both] validate.sh:2736 label=openssl
hermit record start --data-dir "$RECORDING_DIR" -- openssl dgst -sha256 /etc/hostname # [both: record] validate.sh:2736 label=openssl
hermit replay --autopilot --data-dir "$RECORDING_DIR" # [both: replay] validate.sh:2736 label=openssl
hermit record start --verify --record-timeout=30 "--data-dir=$RECORDING_DIR" -- /usr/bin/curl --version # [record/replay] hermit-cli/tests/record_replay.rs explicit-6
hermit record start --verify --record-timeout=30 "--data-dir=$RECORDING_DIR" -- /usr/bin/sqlite3 :memory: 'SELECT 1+1;' # [record/replay] hermit-cli/tests/record_replay.rs explicit-8

# Regression tests
hermit record start --verify --record-timeout=30 "--data-dir=$RECORDING_DIR" -- "$CARGO_TARGET_TMPDIR/record-replay/c_getsockopt_null" # [record/replay] hermit-cli/tests/record_replay.rs workload=c_getsockopt_null
hermit record start --verify --record-timeout=30 "--data-dir=$RECORDING_DIR" -- "$CARGO_TARGET_TMPDIR/record-replay/c_setsockopt_replay" # [record/replay] hermit-cli/tests/record_replay.rs workload=c_setsockopt_replay
hermit record start --verify --record-timeout=30 "--data-dir=$RECORDING_DIR" -- "$CARGO_TARGET_TMPDIR/record-replay/c_record_replay_fd_close" # [record/replay] hermit-cli/tests/record_replay.rs workload=c_record_replay_fd_close
hermit record start --verify --record-timeout=30 "--data-dir=$RECORDING_DIR" -- "$CARGO_TARGET_TMPDIR/record-replay/c_sigpipe_siginfo" # [record/replay] hermit-cli/tests/record_replay.rs workload=c_sigpipe_siginfo
hermit record start --verify --record-timeout=30 "--data-dir=$RECORDING_DIR" -- "$CARGO_TARGET_TMPDIR/record-replay/c_pidfd_open_self" # [record/replay] hermit-cli/tests/record_replay.rs workload=c_pidfd_open_self
hermit record start --verify --record-timeout=30 "--data-dir=$RECORDING_DIR" -- "$CARGO_TARGET_TMPDIR/record-replay/c_pidfd_poll_self" # [record/replay] hermit-cli/tests/record_replay.rs workload=c_pidfd_poll_self
hermit record start --verify --record-timeout=30 "--data-dir=$RECORDING_DIR" -- "$CARGO_TARGET_TMPDIR/record-replay/rustbin_clock_total_order" # [record/replay] hermit-cli/tests/record_replay.rs workload=rustbin_clock_total_order
hermit record start --verify --record-timeout=30 "--data-dir=$RECORDING_DIR" -- "$CARGO_TARGET_TMPDIR/record-replay/rustbin_exit_group" # [record/replay] hermit-cli/tests/record_replay.rs workload=rustbin_exit_group
hermit record start --verify --record-timeout=30 "--data-dir=$RECORDING_DIR" -- "$CARGO_TARGET_TMPDIR/record-replay/rustbin_sched_yield" # [record/replay] hermit-cli/tests/record_replay.rs workload=rustbin_sched_yield
hermit record start --verify --record-timeout=30 "--data-dir=$RECORDING_DIR" -- "$CARGO_TARGET_TMPDIR/record-replay/rustbin_futex_timeout" # [record/replay] hermit-cli/tests/record_replay.rs workload=rustbin_futex_timeout
hermit record start --verify --record-timeout=30 "--data-dir=$RECORDING_DIR" -- "$CARGO_TARGET_TMPDIR/record-replay/rustbin_futex_wait_child" # [record/replay] hermit-cli/tests/record_replay.rs workload=rustbin_futex_wait_child
hermit record start --verify --record-timeout=30 "--data-dir=$RECORDING_DIR" -- "$CARGO_TARGET_TMPDIR/record-replay/rustbin_futex_wake_some" # [record/replay] hermit-cli/tests/record_replay.rs workload=rustbin_futex_wake_some
hermit record start --verify --record-timeout=30 "--data-dir=$RECORDING_DIR" -- "$CARGO_TARGET_TMPDIR/record-replay/rustbin_heap_ptrs" # [record/replay] hermit-cli/tests/record_replay.rs workload=rustbin_heap_ptrs
hermit record start --verify --record-timeout=30 "--data-dir=$RECORDING_DIR" -- "$CARGO_TARGET_TMPDIR/record-replay/rustbin_print_nanosleep_race" # [record/replay] hermit-cli/tests/record_replay.rs workload=rustbin_print_nanosleep_race
hermit record start --verify --record-timeout=30 "--data-dir=$RECORDING_DIR" -- "$CARGO_TARGET_TMPDIR/record-replay/rustbin_nanosleep" # [record/replay] hermit-cli/tests/record_replay.rs workload=rustbin_nanosleep
hermit record start --verify --record-timeout=30 "--data-dir=$RECORDING_DIR" -- "$CARGO_TARGET_TMPDIR/record-replay/rustbin_pipe_basics" # [record/replay] hermit-cli/tests/record_replay.rs workload=rustbin_pipe_basics
hermit record start --verify --record-timeout=30 "--data-dir=$RECORDING_DIR" -- "$CARGO_TARGET_TMPDIR/record-replay/rustbin_poll" # [record/replay] hermit-cli/tests/record_replay.rs workload=rustbin_poll
hermit record start --verify --record-timeout=30 "--data-dir=$RECORDING_DIR" -- "$CARGO_TARGET_TMPDIR/record-replay/rustbin_poll_spin" # [record/replay] hermit-cli/tests/record_replay.rs workload=rustbin_poll_spin
hermit record start --verify --record-timeout=30 "--data-dir=$RECORDING_DIR" -- "$CARGO_TARGET_TMPDIR/record-replay/rustbin_rdtsc" # [record/replay] hermit-cli/tests/record_replay.rs workload=rustbin_rdtsc
hermit record start --verify --record-timeout=30 "--data-dir=$RECORDING_DIR" -- "$CARGO_TARGET_TMPDIR/record-replay/rustbin_stack_ptr" # [record/replay] hermit-cli/tests/record_replay.rs workload=rustbin_stack_ptr
hermit record start --verify --record-timeout=30 "--data-dir=$RECORDING_DIR" -- "$CARGO_TARGET_TMPDIR/record-replay/rustbin_thread_random" # [record/replay] hermit-cli/tests/record_replay.rs workload=rustbin_thread_random
cargo test -p hermit --test aio_nr_determinism -- --include-ignored --test-threads=1 # [verify] all tests in hermit-cli/tests/aio_nr_determinism.rs
cargo test -p hermit --test aio_nr_determinism aio_nr_consumers_verify -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/aio_nr_determinism.rs::aio_nr_consumers_verify
cargo test -p hermit --test analyze -- --include-ignored --test-threads=1 # [run] all tests in hermit-cli/tests/analyze.rs
cargo test -p hermit --test analyze analyze_hello_race -- --exact --include-ignored --test-threads=1 # [run] hermit-cli/tests/analyze.rs::analyze_hello_race
cargo test -p hermit --test analyze analyze_racewrite_nostdlib -- --exact --include-ignored --test-threads=1 # [run] hermit-cli/tests/analyze.rs::analyze_racewrite_nostdlib
cargo test -p hermit --test analyze analyze_nanosleep_threads_rejects_indistinguishable_baseline -- --exact --include-ignored --test-threads=1 # [run] hermit-cli/tests/analyze.rs::analyze_nanosleep_threads_rejects_indistinguishable_baseline
cargo test -p hermit --test app_strict_verify -- --include-ignored --test-threads=1 # [verify] all tests in hermit-cli/tests/app_strict_verify.rs
cargo test -p hermit --test app_strict_verify curl_version_is_deterministic_under_strict_verify -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/app_strict_verify.rs::curl_version_is_deterministic_under_strict_verify
cargo test -p hermit --test app_strict_verify nginx_version_is_deterministic_under_strict_verify -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/app_strict_verify.rs::nginx_version_is_deterministic_under_strict_verify
cargo test -p hermit --test app_strict_verify redis_server_version_is_deterministic_under_strict_verify -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/app_strict_verify.rs::redis_server_version_is_deterministic_under_strict_verify
cargo test -p hermit --test app_strict_verify java_version_is_deterministic_under_strict_verify -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/app_strict_verify.rs::java_version_is_deterministic_under_strict_verify
cargo test -p hermit --test app_strict_verify go_hello_is_deterministic_under_strict_verify -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/app_strict_verify.rs::go_hello_is_deterministic_under_strict_verify
cargo test -p hermit --test app_strict_verify go_goroutines_are_deterministic_under_strict_verify -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/app_strict_verify.rs::go_goroutines_are_deterministic_under_strict_verify
cargo test -p hermit --test app_strict_verify java_hello_is_deterministic_under_strict_verify -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/app_strict_verify.rs::java_hello_is_deterministic_under_strict_verify
cargo test -p hermit --test app_strict_verify java_threads_are_deterministic_under_strict_verify -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/app_strict_verify.rs::java_threads_are_deterministic_under_strict_verify
cargo test -p hermit --test app_strict_verify go_version_is_l1_deterministic_under_strict -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/app_strict_verify.rs::go_version_is_l1_deterministic_under_strict
cargo test -p hermit --test app_strict_verify javac_is_l1_deterministic_under_strict -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/app_strict_verify.rs::javac_is_l1_deterministic_under_strict
cargo test -p hermit --test arbitrary_binaries -- --include-ignored --test-threads=1 # [both/mixed] all tests in hermit-cli/tests/arbitrary_binaries.rs
cargo test -p hermit --test arbitrary_binaries run_arbitrary_binary_matrix -- --exact --include-ignored --test-threads=1 # [both/mixed] hermit-cli/tests/arbitrary_binaries.rs::run_arbitrary_binary_matrix
cargo test -p hermit --test arbitrary_binaries record_replay_stable_arbitrary_binaries -- --exact --include-ignored --test-threads=1 # [both/mixed] hermit-cli/tests/arbitrary_binaries.rs::record_replay_stable_arbitrary_binaries
cargo test -p hermit --test arbitrary_binaries arbitrary_binary_commands_are_bounded -- --exact --include-ignored --test-threads=1 # [both/mixed] hermit-cli/tests/arbitrary_binaries.rs::arbitrary_binary_commands_are_bounded
cargo test -p hermit --test arbitrary_binaries arbitrary_binary_lists_are_curated_for_ci -- --exact --include-ignored --test-threads=1 # [both/mixed] hermit-cli/tests/arbitrary_binaries.rs::arbitrary_binary_lists_are_curated_for_ci
cargo test -p hermit --test arch_prctl -- --include-ignored --test-threads=1 # [both/mixed] all tests in hermit-cli/tests/arch_prctl.rs
cargo test -p hermit --test arch_prctl arch_prctl_controls_verify_in_run_and_record_modes -- --exact --include-ignored --test-threads=1 # [both/mixed] hermit-cli/tests/arch_prctl.rs::arch_prctl_controls_verify_in_run_and_record_modes
cargo test -p hermit --test arch_status_determinism -- --include-ignored --test-threads=1 # [verify] all tests in hermit-cli/tests/arch_status_determinism.rs
cargo test -p hermit --test arch_status_determinism arch_status_consumers_are_deterministic_under_strict_verify -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/arch_status_determinism.rs::arch_status_consumers_are_deterministic_under_strict_verify
cargo test -p hermit --test block_inflight_determinism -- --include-ignored --test-threads=1 # [verify] all tests in hermit-cli/tests/block_inflight_determinism.rs
cargo test -p hermit --test block_inflight_determinism block_inflight_consumers_are_deterministic_under_strict_verify -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/block_inflight_determinism.rs::block_inflight_consumers_are_deterministic_under_strict_verify
cargo test -p hermit --test btrfs_commit_determinism -- --include-ignored --test-threads=1 # [verify] all tests in hermit-cli/tests/btrfs_commit_determinism.rs
cargo test -p hermit --test btrfs_commit_determinism btrfs_commit_stats_consumers_are_deterministic_under_strict_verify -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/btrfs_commit_determinism.rs::btrfs_commit_stats_consumers_are_deterministic_under_strict_verify
cargo test -p hermit --test btrfs_pinned_bytes_determinism -- --include-ignored --test-threads=1 # [verify] all tests in hermit-cli/tests/btrfs_pinned_bytes_determinism.rs
cargo test -p hermit --test btrfs_pinned_bytes_determinism btrfs_pinned_space_consumers_are_deterministic_under_strict_verify -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/btrfs_pinned_bytes_determinism.rs::btrfs_pinned_space_consumers_are_deterministic_under_strict_verify
cargo test -p hermit --test btrfs_reservation_determinism -- --include-ignored --test-threads=1 # [verify] all tests in hermit-cli/tests/btrfs_reservation_determinism.rs
cargo test -p hermit --test btrfs_reservation_determinism btrfs_reservation_consumers_verify -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/btrfs_reservation_determinism.rs::btrfs_reservation_consumers_verify
cargo test -p hermit --test btrfs_reserved_bytes_determinism -- --include-ignored --test-threads=1 # [verify] all tests in hermit-cli/tests/btrfs_reserved_bytes_determinism.rs
cargo test -p hermit --test btrfs_reserved_bytes_determinism btrfs_reserved_bytes_consumers_are_deterministic_under_strict_verify -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/btrfs_reserved_bytes_determinism.rs::btrfs_reserved_bytes_consumers_are_deterministic_under_strict_verify
cargo test -p hermit --test buddyinfo_determinism -- --include-ignored --test-threads=1 # [verify] all tests in hermit-cli/tests/buddyinfo_determinism.rs
cargo test -p hermit --test buddyinfo_determinism buddyinfo_consumers_verify -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/buddyinfo_determinism.rs::buddyinfo_consumers_verify
cargo test -p hermit --test chaos_sched_yield_progress -- --include-ignored --test-threads=1 # [both/mixed] all tests in hermit-cli/tests/chaos_sched_yield_progress.rs
cargo test -p hermit --test chaos_sched_yield_progress chaos_sched_yield_makes_progress_without_timer_preemption -- --exact --include-ignored --test-threads=1 # [both/mixed] hermit-cli/tests/chaos_sched_yield_progress.rs::chaos_sched_yield_makes_progress_without_timer_preemption
cargo test -p hermit --test chaos_sched_yield_progress strict_sched_yield_is_deterministic -- --exact --include-ignored --test-threads=1 # [both/mixed] hermit-cli/tests/chaos_sched_yield_progress.rs::strict_sched_yield_is_deterministic
cargo test -p hermit --test chaos_sched_yield_progress strict_vfork_child_sched_yield_is_deterministic -- --exact --include-ignored --test-threads=1 # [both/mixed] hermit-cli/tests/chaos_sched_yield_progress.rs::strict_vfork_child_sched_yield_is_deterministic
cargo test -p hermit --test chaos_sched_yield_progress preemption_replay_preserves_vfork_sched_yield_progress -- --exact --include-ignored --test-threads=1 # [both/mixed] hermit-cli/tests/chaos_sched_yield_progress.rs::preemption_replay_preserves_vfork_sched_yield_progress
cargo test -p hermit --test chaos_stress_pmu_detection -- --include-ignored --test-threads=1 # [run] all tests in hermit-cli/tests/chaos_stress_pmu_detection.rs
cargo test -p hermit --test chaos_stress_pmu_detection detects_capable_host -- --exact --include-ignored --test-threads=1 # [run] hermit-cli/tests/chaos_stress_pmu_detection.rs::detects_capable_host
cargo test -p hermit --test chaos_stress_pmu_detection detects_legacy_hardware_labelled_host -- --exact --include-ignored --test-threads=1 # [run] hermit-cli/tests/chaos_stress_pmu_detection.rs::detects_legacy_hardware_labelled_host
cargo test -p hermit --test chaos_stress_pmu_detection rejects_not_supported_counter -- --exact --include-ignored --test-threads=1 # [run] hermit-cli/tests/chaos_stress_pmu_detection.rs::rejects_not_supported_counter
cargo test -p hermit --test chaos_stress_pmu_detection rejects_not_counted_counter -- --exact --include-ignored --test-threads=1 # [run] hermit-cli/tests/chaos_stress_pmu_detection.rs::rejects_not_counted_counter
cargo test -p hermit --test chaos_stress_pmu_detection rejects_perf_stat_failure -- --exact --include-ignored --test-threads=1 # [run] hermit-cli/tests/chaos_stress_pmu_detection.rs::rejects_perf_stat_failure
cargo test -p hermit --test chaos_stress_pmu_detection rejects_missing_perf_binary -- --exact --include-ignored --test-threads=1 # [run] hermit-cli/tests/chaos_stress_pmu_detection.rs::rejects_missing_perf_binary
cargo test -p hermit --test cli -- --include-ignored --test-threads=1 # [both/mixed] all tests in hermit-cli/tests/cli.rs
cargo test -p hermit --test cli top_level_help_lists_user_facing_commands -- --exact --include-ignored --test-threads=1 # [both/mixed] hermit-cli/tests/cli.rs::top_level_help_lists_user_facing_commands
cargo test -p hermit --test cli bisect_help_describes_schedule_endpoints -- --exact --include-ignored --test-threads=1 # [both/mixed] hermit-cli/tests/cli.rs::bisect_help_describes_schedule_endpoints
cargo test -p hermit --test cli replay_help_accepts_optional_recording_id -- --exact --include-ignored --test-threads=1 # [both/mixed] hermit-cli/tests/cli.rs::replay_help_accepts_optional_recording_id
cargo test -p hermit --test cli run_help_exposes_determinism_modes -- --exact --include-ignored --test-threads=1 # [both/mixed] hermit-cli/tests/cli.rs::run_help_exposes_determinism_modes
cargo test -p hermit --test cli run_strict_flag_is_accepted_and_runs -- --exact --include-ignored --test-threads=1 # [both/mixed] hermit-cli/tests/cli.rs::run_strict_flag_is_accepted_and_runs
cargo test -p hermit --test cli verify_verbose_requires_verify -- --exact --include-ignored --test-threads=1 # [both/mixed] hermit-cli/tests/cli.rs::verify_verbose_requires_verify
cargo test -p hermit --test cli run_rejects_unknown_backends_during_argument_parsing -- --exact --include-ignored --test-threads=1 # [both/mixed] hermit-cli/tests/cli.rs::run_rejects_unknown_backends_during_argument_parsing
cargo test -p hermit --test cli run_dbi_executes_integrated_backend -- --exact --include-ignored --test-threads=1 # [both/mixed] hermit-cli/tests/cli.rs::run_dbi_executes_integrated_backend
cargo test -p hermit --test cli run_ptrace_verify_reemits_unsupported_syscall_warning -- --exact --include-ignored --test-threads=1 # [both/mixed] hermit-cli/tests/cli.rs::run_ptrace_verify_reemits_unsupported_syscall_warning
cargo test -p hermit --test cli run_dbi_aggregates_unsupported_syscalls_and_strict_rejects_them -- --exact --include-ignored --test-threads=1 # [both/mixed] hermit-cli/tests/cli.rs::run_dbi_aggregates_unsupported_syscalls_and_strict_rejects_them
cargo test -p hermit --test cli run_dbi_strict_returns_with_blocked_stdin_source -- --exact --include-ignored --test-threads=1 # [both/mixed] hermit-cli/tests/cli.rs::run_dbi_strict_returns_with_blocked_stdin_source
cargo test -p hermit --test cli run_liteinst_verifies_detcore_backend -- --exact --include-ignored --test-threads=1 # [both/mixed] hermit-cli/tests/cli.rs::run_liteinst_verifies_detcore_backend
cargo test -p hermit --test cli run_dbi_keeps_diagnostics_out_of_guest_stderr -- --exact --include-ignored --test-threads=1 # [both/mixed] hermit-cli/tests/cli.rs::run_dbi_keeps_diagnostics_out_of_guest_stderr
cargo test -p hermit --test cli run_dbi_verifies_application_mmap -- --exact --include-ignored --test-threads=1 # [both/mixed] hermit-cli/tests/cli.rs::run_dbi_verifies_application_mmap
cargo test -p hermit --test cli run_dbi_verifies_process_wait_lifecycle -- --exact --include-ignored --test-threads=1 # [both/mixed] hermit-cli/tests/cli.rs::run_dbi_verifies_process_wait_lifecycle
cargo test -p hermit --test cli run_dbi_virtualizes_process_identities -- --exact --include-ignored --test-threads=1 # [both/mixed] hermit-cli/tests/cli.rs::run_dbi_virtualizes_process_identities
cargo test -p hermit --test cli run_dbi_verifies_shell_process_lifecycle -- --exact --include-ignored --test-threads=1 # [both/mixed] hermit-cli/tests/cli.rs::run_dbi_verifies_shell_process_lifecycle
cargo test -p hermit --test cli run_dbi_verifies_pipe_backpressure -- --exact --include-ignored --test-threads=1 # [both/mixed] hermit-cli/tests/cli.rs::run_dbi_verifies_pipe_backpressure
cargo test -p hermit --test cli run_dbi_recovers_after_failed_exec -- --exact --include-ignored --test-threads=1 # [both/mixed] hermit-cli/tests/cli.rs::run_dbi_recovers_after_failed_exec
cargo test -p hermit --test cli run_dbi_rejects_unfollowed_execveat -- --exact --include-ignored --test-threads=1 # [both/mixed] hermit-cli/tests/cli.rs::run_dbi_rejects_unfollowed_execveat
cargo test -p hermit --test cli run_kvm_executes_dynamic_guest -- --exact --include-ignored --test-threads=1 # [both/mixed] hermit-cli/tests/cli.rs::run_kvm_executes_dynamic_guest
cargo test -p hermit --test cli run_kvm_resolves_bare_program_from_guest_path -- --exact --include-ignored --test-threads=1 # [both/mixed] hermit-cli/tests/cli.rs::run_kvm_resolves_bare_program_from_guest_path
cargo test -p hermit --test cli run_kvm_propagates_explicit_environment -- --exact --include-ignored --test-threads=1 # [both/mixed] hermit-cli/tests/cli.rs::run_kvm_propagates_explicit_environment
cargo test -p hermit --test cli run_kvm_bash_process_substitution_is_deterministic -- --exact --include-ignored --test-threads=1 # [both/mixed] hermit-cli/tests/cli.rs::run_kvm_bash_process_substitution_is_deterministic
cargo test -p hermit --test cli run_kvm_cpuid_policy_is_deterministic -- --exact --include-ignored --test-threads=1 # [both/mixed] hermit-cli/tests/cli.rs::run_kvm_cpuid_policy_is_deterministic
cargo test -p hermit --test cli run_kvm_respects_workdir_for_relative_paths -- --exact --include-ignored --test-threads=1 # [both/mixed] hermit-cli/tests/cli.rs::run_kvm_respects_workdir_for_relative_paths
cargo test -p hermit --test cli run_kvm_lists_host_directory_metadata -- --exact --include-ignored --test-threads=1 # [both/mixed] hermit-cli/tests/cli.rs::run_kvm_lists_host_directory_metadata
cargo test -p hermit --test cli run_kvm_reads_host_file -- --exact --include-ignored --test-threads=1 # [both/mixed] hermit-cli/tests/cli.rs::run_kvm_reads_host_file
cargo test -p hermit --test cli run_kvm_reads_standard_input -- --exact --include-ignored --test-threads=1 # [both/mixed] hermit-cli/tests/cli.rs::run_kvm_reads_standard_input
cargo test -p hermit --test cli run_kvm_f_getfl_and_reads_standard_input -- --exact --include-ignored --test-threads=1 # [both/mixed] hermit-cli/tests/cli.rs::run_kvm_f_getfl_and_reads_standard_input
cargo test -p hermit --test cli run_kvm_verify_f_getfl_with_isolated_standard_input -- --exact --include-ignored --test-threads=1 # [both/mixed] hermit-cli/tests/cli.rs::run_kvm_verify_f_getfl_with_isolated_standard_input
cargo test -p hermit --test cli run_kvm_verify_isolates_standard_input -- --exact --include-ignored --test-threads=1 # [both/mixed] hermit-cli/tests/cli.rs::run_kvm_verify_isolates_standard_input
cargo test -p hermit --test cli run_kvm_preserves_closed_standard_input -- --exact --include-ignored --test-threads=1 # [both/mixed] hermit-cli/tests/cli.rs::run_kvm_preserves_closed_standard_input
cargo test -p hermit --test cli run_kvm_verify_does_not_write_to_standard_input -- --exact --include-ignored --test-threads=1 # [both/mixed] hermit-cli/tests/cli.rs::run_kvm_verify_does_not_write_to_standard_input
cargo test -p hermit --test cli run_kvm_counts_standard_input -- --exact --include-ignored --test-threads=1 # [both/mixed] hermit-cli/tests/cli.rs::run_kvm_counts_standard_input
cargo test -p hermit --test cli run_kvm_reports_hostname -- --exact --include-ignored --test-threads=1 # [both/mixed] hermit-cli/tests/cli.rs::run_kvm_reports_hostname
cargo test -p hermit --test cli run_kvm_pipe_pipe2_and_getgroups_round_trip -- --exact --include-ignored --test-threads=1 # [both/mixed] hermit-cli/tests/cli.rs::run_kvm_pipe_pipe2_and_getgroups_round_trip
cargo test -p hermit --test cli run_kvm_reports_fixed_supplementary_groups -- --exact --include-ignored --test-threads=1 # [both/mixed] hermit-cli/tests/cli.rs::run_kvm_reports_fixed_supplementary_groups
cargo test -p hermit --test cli namespace_only_rejects_every_explicit_backend -- --exact --include-ignored --test-threads=1 # [both/mixed] hermit-cli/tests/cli.rs::namespace_only_rejects_every_explicit_backend
cargo test -p hermit --test cli backend_accepted_in_global_position -- --exact --include-ignored --test-threads=1 # [both/mixed] hermit-cli/tests/cli.rs::backend_accepted_in_global_position
cargo test -p hermit --test cli sabre_backend_validation_honors_command_scope -- --exact --include-ignored --test-threads=1 # [both/mixed] hermit-cli/tests/cli.rs::sabre_backend_validation_honors_command_scope
cargo test -p hermit --test cli sabre_rpc_socket_is_hidden_from_proc_environ -- --exact --include-ignored --test-threads=1 # [both/mixed] hermit-cli/tests/cli.rs::sabre_rpc_socket_is_hidden_from_proc_environ
cargo test -p hermit --test cli global_position_rejects_unknown_backends -- --exact --include-ignored --test-threads=1 # [both/mixed] hermit-cli/tests/cli.rs::global_position_rejects_unknown_backends
cargo test -p hermit --test cli namespace_only_rejects_global_position_backend -- --exact --include-ignored --test-threads=1 # [both/mixed] hermit-cli/tests/cli.rs::namespace_only_rejects_global_position_backend
cargo test -p hermit --test cli incompatible_run_modes_fail_during_argument_parsing -- --exact --include-ignored --test-threads=1 # [both/mixed] hermit-cli/tests/cli.rs::incompatible_run_modes_fail_during_argument_parsing
cargo test -p hermit --test cli no_namespace_rejects_container_only_options -- --exact --include-ignored --test-threads=1 # [both/mixed] hermit-cli/tests/cli.rs::no_namespace_rejects_container_only_options
cargo test -p hermit --test cli no_namespace_runs_without_container_setup -- --exact --include-ignored --test-threads=1 # [both/mixed] hermit-cli/tests/cli.rs::no_namespace_runs_without_container_setup
cargo test -p hermit --test cli no_namespace_preserves_affinity_for_run_and_verify -- --exact --include-ignored --test-threads=1 # [both/mixed] hermit-cli/tests/cli.rs::no_namespace_preserves_affinity_for_run_and_verify
cargo test -p hermit --test cli record_help_lists_management_commands -- --exact --include-ignored --test-threads=1 # [both/mixed] hermit-cli/tests/cli.rs::record_help_lists_management_commands
cargo test -p hermit --test cli record_list_json_reports_an_empty_inventory -- --exact --include-ignored --test-threads=1 # [both/mixed] hermit-cli/tests/cli.rs::record_list_json_reports_an_empty_inventory
cargo test -p hermit --test cli run_rejects_invalid_programs_with_actionable_errors -- --exact --include-ignored --test-threads=1 # [both/mixed] hermit-cli/tests/cli.rs::run_rejects_invalid_programs_with_actionable_errors
cargo test -p hermit --test cli run_rejects_invalid_configuration_without_panicking -- --exact --include-ignored --test-threads=1 # [both/mixed] hermit-cli/tests/cli.rs::run_rejects_invalid_configuration_without_panicking
cargo test -p hermit --test cli run_rejects_a_missing_bind_source_before_mounting -- --exact --include-ignored --test-threads=1 # [both/mixed] hermit-cli/tests/cli.rs::run_rejects_a_missing_bind_source_before_mounting
cargo test -p hermit --test cli run_reports_denied_ptrace_and_seccomp_capabilities -- --exact --include-ignored --test-threads=1 # [both/mixed] hermit-cli/tests/cli.rs::run_reports_denied_ptrace_and_seccomp_capabilities
cargo test -p hermit --test clock_determinism -- --include-ignored --test-threads=1 # [run] all tests in hermit-cli/tests/clock_determinism.rs
cargo test -p hermit --test clock_determinism clock_apis_are_deterministic_across_five_runs -- --exact --include-ignored --test-threads=1 # [run] hermit-cli/tests/clock_determinism.rs::clock_apis_are_deterministic_across_five_runs
cargo test -p hermit --test clock_determinism strict_mode_eliminates_native_clock_nondeterminism -- --exact --include-ignored --test-threads=1 # [run] hermit-cli/tests/clock_determinism.rs::strict_mode_eliminates_native_clock_nondeterminism
cargo test -p hermit --test clock_discipline_determinism -- --include-ignored --test-threads=1 # [verify] all tests in hermit-cli/tests/clock_discipline_determinism.rs
cargo test -p hermit --test clock_discipline_determinism clock_discipline_and_kernel_log_are_host_independent -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/clock_discipline_determinism.rs::clock_discipline_and_kernel_log_are_host_independent
cargo test -p hermit --test command_strict_verify -- --include-ignored --test-threads=1 # [verify] all tests in hermit-cli/tests/command_strict_verify.rs
cargo test -p hermit --test command_strict_verify common_commands_are_deterministic_under_strict_verify -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/command_strict_verify.rs::common_commands_are_deterministic_under_strict_verify
cargo test -p hermit --test command_strict_verify identity_commands_are_deterministic_under_strict_verify -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/command_strict_verify.rs::identity_commands_are_deterministic_under_strict_verify
cargo test -p hermit --test command_strict_verify process_accounting_commands_are_deterministic_under_strict_verify -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/command_strict_verify.rs::process_accounting_commands_are_deterministic_under_strict_verify
cargo test -p hermit --test command_strict_verify io_accounting_commands_are_deterministic_under_strict_verify -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/command_strict_verify.rs::io_accounting_commands_are_deterministic_under_strict_verify
cargo test -p hermit --test command_strict_verify kernel_pseudofile_commands_are_deterministic_under_strict_verify -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/command_strict_verify.rs::kernel_pseudofile_commands_are_deterministic_under_strict_verify
cargo test -p hermit --test command_strict_verify ionice_query_is_deterministic_under_strict_verify -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/command_strict_verify.rs::ionice_query_is_deterministic_under_strict_verify
cargo test -p hermit --test command_strict_verify kernel_activity_commands_are_deterministic_under_strict_verify -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/command_strict_verify.rs::kernel_activity_commands_are_deterministic_under_strict_verify
cargo test -p hermit --test command_strict_verify hardware_accounting_commands_are_deterministic_under_strict_verify -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/command_strict_verify.rs::hardware_accounting_commands_are_deterministic_under_strict_verify
cargo test -p hermit --test command_strict_verify python_prlimit64_query_is_deterministic_under_strict_verify -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/command_strict_verify.rs::python_prlimit64_query_is_deterministic_under_strict_verify
cargo test -p hermit --test command_strict_verify python_getrandom_is_deterministic_under_strict_verify -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/command_strict_verify.rs::python_getrandom_is_deterministic_under_strict_verify
cargo test -p hermit --test compression -- --include-ignored --test-threads=1 # [run] all tests in hermit-cli/tests/compression.rs
cargo test -p hermit --test compression compression_tools_are_deterministic_under_strict_hermit -- --exact --include-ignored --test-threads=1 # [run] hermit-cli/tests/compression.rs::compression_tools_are_deterministic_under_strict_hermit
cargo test -p hermit --test copy_file_range_refusal -- --include-ignored --test-threads=1 # [verify] all tests in hermit-cli/tests/copy_file_range_refusal.rs
cargo test -p hermit --test copy_file_range_refusal copy_file_range_refusal_is_deterministic -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/copy_file_range_refusal.rs::copy_file_range_refusal_is_deterministic
cargo test -p hermit --test cppc_feedback_determinism -- --include-ignored --test-threads=1 # [verify] all tests in hermit-cli/tests/cppc_feedback_determinism.rs
cargo test -p hermit --test cppc_feedback_determinism cppc_feedback_consumers_verify -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/cppc_feedback_determinism.rs::cppc_feedback_consumers_verify
cargo test -p hermit --test cpufreq_avg_determinism -- --include-ignored --test-threads=1 # [verify] all tests in hermit-cli/tests/cpufreq_avg_determinism.rs
cargo test -p hermit --test cpufreq_avg_determinism cpufreq_average_consumers_verify -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/cpufreq_avg_determinism.rs::cpufreq_average_consumers_verify
cargo test -p hermit --test cpuidle_determinism -- --include-ignored --test-threads=1 # [verify] all tests in hermit-cli/tests/cpuidle_determinism.rs
cargo test -p hermit --test cpuidle_determinism cpuidle_counter_consumers_verify -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/cpuidle_determinism.rs::cpuidle_counter_consumers_verify
cargo test -p hermit --test dentry_state_determinism -- --include-ignored --test-threads=1 # [verify] all tests in hermit-cli/tests/dentry_state_determinism.rs
cargo test -p hermit --test dentry_state_determinism dentry_state_consumers_verify -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/dentry_state_determinism.rs::dentry_state_consumers_verify
cargo test -p hermit --test epoll_determinism -- --include-ignored --test-threads=1 # [verify] all tests in hermit-cli/tests/epoll_determinism.rs
cargo test -p hermit --test epoll_determinism multiple_ready_fds_have_deterministic_ordering -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/epoll_determinism.rs::multiple_ready_fds_have_deterministic_ordering
cargo test -p hermit --test epoll_determinism edge_triggered_delivery_is_deterministic -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/epoll_determinism.rs::edge_triggered_delivery_is_deterministic
cargo test -p hermit --test epoll_determinism oneshot_delivery_and_rearming_are_deterministic -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/epoll_determinism.rs::oneshot_delivery_and_rearming_are_deterministic
cargo test -p hermit --test epoll_determinism mixed_fd_readiness_is_deterministic -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/epoll_determinism.rs::mixed_fd_readiness_is_deterministic
cargo test -p hermit --test epoll_determinism nested_epoll_delivery_is_deterministic -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/epoll_determinism.rs::nested_epoll_delivery_is_deterministic
cargo test -p hermit --test epoll_determinism notification_control_syscalls_are_deterministic -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/epoll_determinism.rs::notification_control_syscalls_are_deterministic
cargo test -p hermit --test epoll_determinism notification_control_syscalls_reach_strict_verify_l2 -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/epoll_determinism.rs::notification_control_syscalls_reach_strict_verify_l2
cargo test -p hermit --test epoll_determinism epoll_fd_supports_descriptor_table_ops -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/epoll_determinism.rs::epoll_fd_supports_descriptor_table_ops
cargo test -p hermit --test file_nr_determinism -- --include-ignored --test-threads=1 # [verify] all tests in hermit-cli/tests/file_nr_determinism.rs
cargo test -p hermit --test file_nr_determinism file_nr_consumers_verify -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/file_nr_determinism.rs::file_nr_consumers_verify
cargo test -p hermit --test fp_reduction_determinism -- --include-ignored --test-threads=1 # [run] all tests in hermit-cli/tests/fp_reduction_determinism.rs
cargo test -p hermit --test fp_reduction_determinism native_parallel_fp_reduction_exposes_low_bit_variation -- --exact --include-ignored --test-threads=1 # [run] hermit-cli/tests/fp_reduction_determinism.rs::native_parallel_fp_reduction_exposes_low_bit_variation
cargo test -p hermit --test fp_reduction_determinism strict_parallel_fp_reduction_is_bit_identical -- --exact --include-ignored --test-threads=1 # [run] hermit-cli/tests/fp_reduction_determinism.rs::strict_parallel_fp_reduction_is_bit_identical
cargo test -p hermit --test futex2_refusal -- --include-ignored --test-threads=1 # [verify] all tests in hermit-cli/tests/futex2_refusal.rs
cargo test -p hermit --test futex2_refusal futex2_feature_probes_receive_deterministic_enosys -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/futex2_refusal.rs::futex2_feature_probes_receive_deterministic_enosys
cargo test -p hermit --test getitimer_determinism -- --include-ignored --test-threads=1 # [verify] all tests in hermit-cli/tests/getitimer_determinism.rs
cargo test -p hermit --test getitimer_determinism getitimer_tracks_logical_alarm_state -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/getitimer_determinism.rs::getitimer_tracks_logical_alarm_state
cargo test -p hermit --test hashseed_determinism -- --include-ignored --test-threads=1 # [run] all tests in hermit-cli/tests/hashseed_determinism.rs
cargo test -p hermit --test hashseed_determinism python_set_order_nondeterministic_natively_deterministic_under_hermit -- --exact --include-ignored --test-threads=1 # [run] hermit-cli/tests/hashseed_determinism.rs::python_set_order_nondeterministic_natively_deterministic_under_hermit
cargo test -p hermit --test hermit_modes -- --include-ignored --test-threads=1 # [verify] all tests in hermit-cli/tests/hermit_modes.rs
cargo test -p hermit --test hermit_modes default_mode_matrix -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/hermit_modes.rs::default_mode_matrix
cargo test -p hermit --test hermit_modes resource_syscalls_are_deterministic_across_five_runs -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/hermit_modes.rs::resource_syscalls_are_deterministic_across_five_runs
cargo test -p hermit --test hermit_modes default_cargo_bind_connect_race -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/hermit_modes.rs::default_cargo_bind_connect_race
cargo test -p hermit --test hermit_modes default_cargo_clock_total_order -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/hermit_modes.rs::default_cargo_clock_total_order
cargo test -p hermit --test hermit_modes default_minimal_hello -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/hermit_modes.rs::default_minimal_hello
cargo test -p hermit --test hermit_modes default_lit_networking -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/hermit_modes.rs::default_lit_networking
cargo test -p hermit --test hermit_modes default_exit_codes -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/hermit_modes.rs::default_exit_codes
cargo test -p hermit --test hermit_modes default_virtualized_uname -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/hermit_modes.rs::default_virtualized_uname
cargo test -p hermit --test hermit_modes default_cat_issue -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/hermit_modes.rs::default_cat_issue
cargo test -p hermit --test hermit_modes default_bind_mounts -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/hermit_modes.rs::default_bind_mounts
cargo test -p hermit --test hermit_modes default_preserved_tmpfs -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/hermit_modes.rs::default_preserved_tmpfs
cargo test -p hermit --test hermit_modes default_environment_selection -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/hermit_modes.rs::default_environment_selection
cargo test -p hermit --test hermit_modes no_hardware_minimal_hello_backtraces -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/hermit_modes.rs::no_hardware_minimal_hello_backtraces
cargo test -p hermit --test hermit_modes no_hardware_stacktrace_signal -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/hermit_modes.rs::no_hardware_stacktrace_signal
cargo test -p hermit --test hermit_modes strict_mode_matrix -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/hermit_modes.rs::strict_mode_matrix
cargo test -p hermit --test hermit_modes chaos_mode_matrix -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/hermit_modes.rs::chaos_mode_matrix
cargo test -p hermit --test hermit_modes verify_mode_matrix -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/hermit_modes.rs::verify_mode_matrix
cargo test -p hermit --test hermit_modes verify_captures_debug_logs_when_a_lower_level_is_requested -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/hermit_modes.rs::verify_captures_debug_logs_when_a_lower_level_is_requested
cargo test -p hermit --test hermit_modes verify_reports_stdout_divergence -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/hermit_modes.rs::verify_reports_stdout_divergence
cargo test -p hermit --test hermit_modes verify_reports_exit_status_divergence -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/hermit_modes.rs::verify_reports_exit_status_divergence
cargo test -p hermit --test hermit_modes verify_verbose_compares_the_full_trace -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/hermit_modes.rs::verify_verbose_compares_the_full_trace
cargo test -p hermit --test hermit_modes verify_honors_tmp_and_environment -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/hermit_modes.rs::verify_honors_tmp_and_environment
cargo test -p hermit --test hermit_modes hello_race_chaos_verify -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/hermit_modes.rs::hello_race_chaos_verify
cargo test -p hermit --test host_kernel_probes -- --include-ignored --test-threads=1 # [verify] all tests in hermit-cli/tests/host_kernel_probes.rs
cargo test -p hermit --test host_kernel_probes host_kernel_probes_fall_back_deterministically -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/host_kernel_probes.rs::host_kernel_probes_fall_back_deterministically
cargo test -p hermit --test host_security_identity -- --include-ignored --test-threads=1 # [verify] all tests in hermit-cli/tests/host_security_identity.rs
cargo test -p hermit --test host_security_identity host_security_identity_probes_fall_back_deterministically -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/host_security_identity.rs::host_security_identity_probes_fall_back_deterministically
cargo test -p hermit --test inode_nr_determinism -- --include-ignored --test-threads=1 # [verify] all tests in hermit-cli/tests/inode_nr_determinism.rs
cargo test -p hermit --test inode_nr_determinism inode_nr_consumers_verify -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/inode_nr_determinism.rs::inode_nr_consumers_verify
cargo test -p hermit --test integration_matrix -- --include-ignored --test-threads=1 # [run] all tests in hermit-cli/tests/integration_matrix.rs
cargo test -p hermit --test integration_matrix integration_matrix -- --exact --include-ignored --test-threads=1 # [run] hermit-cli/tests/integration_matrix.rs::integration_matrix
cargo test -p hermit --test ipc_determinism -- --include-ignored --test-threads=1 # [run] all tests in hermit-cli/tests/ipc_determinism.rs
cargo test -p hermit --test ipc_determinism ipc_patterns_are_deterministic_across_five_runs -- --exact --include-ignored --test-threads=1 # [run] hermit-cli/tests/ipc_determinism.rs::ipc_patterns_are_deterministic_across_five_runs
cargo test -p hermit --test irq_per_cpu_determinism -- --include-ignored --test-threads=1 # [verify] all tests in hermit-cli/tests/irq_per_cpu_determinism.rs
cargo test -p hermit --test irq_per_cpu_determinism irq_per_cpu_count_consumers_are_deterministic_under_strict_verify -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/irq_per_cpu_determinism.rs::irq_per_cpu_count_consumers_are_deterministic_under_strict_verify
cargo test -p hermit --test kernel_keyring -- --include-ignored --test-threads=1 # [verify] all tests in hermit-cli/tests/kernel_keyring.rs
cargo test -p hermit --test kernel_keyring kernel_keyring_is_deterministically_unavailable -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/kernel_keyring.rs::kernel_keyring_is_deterministically_unavailable
cargo test -p hermit --test kernel_keyring kernel_keyring_passes_through_in_non_strict_mode -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/kernel_keyring.rs::kernel_keyring_passes_through_in_non_strict_mode
cargo test -p hermit --test key_users_determinism -- --include-ignored --test-threads=1 # [verify] all tests in hermit-cli/tests/key_users_determinism.rs
cargo test -p hermit --test key_users_determinism key_user_consumers_are_deterministic_under_strict_verify -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/key_users_determinism.rs::key_user_consumers_are_deterministic_under_strict_verify
cargo test -p hermit --test language_runtime_determinism -- --include-ignored --test-threads=1 # [verify] all tests in hermit-cli/tests/language_runtime_determinism.rs
cargo test -p hermit --test language_runtime_determinism go_runtime_entropy_is_determinized -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/language_runtime_determinism.rs::go_runtime_entropy_is_determinized
cargo test -p hermit --test language_runtime_determinism ruby_runtime_entropy_is_determinized -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/language_runtime_determinism.rs::ruby_runtime_entropy_is_determinized
cargo test -p hermit --test language_runtime_determinism ruby_thread_prctls_are_supported_in_fail_closed_mode -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/language_runtime_determinism.rs::ruby_thread_prctls_are_supported_in_fail_closed_mode
cargo test -p hermit --test language_runtime_determinism node_runtime_entropy_is_determinized -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/language_runtime_determinism.rs::node_runtime_entropy_is_determinized
cargo test -p hermit --test language_runtime_determinism jvm_runtime_entropy_is_determinized -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/language_runtime_determinism.rs::jvm_runtime_entropy_is_determinized
cargo test -p hermit --test language_runtime_determinism ocaml_runtime_entropy_is_determinized -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/language_runtime_determinism.rs::ocaml_runtime_entropy_is_determinized
cargo test -p hermit --test language_runtime_determinism python_runtime_entropy_is_determinized -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/language_runtime_determinism.rs::python_runtime_entropy_is_determinized
cargo test -p hermit --test leveldb -- --include-ignored --test-threads=1 # [run] all tests in hermit-cli/tests/leveldb.rs
cargo test -p hermit --test leveldb focused_leveldb_tests_are_deterministic_under_strict -- --exact --include-ignored --test-threads=1 # [run] hermit-cli/tests/leveldb.rs::focused_leveldb_tests_are_deterministic_under_strict
cargo test -p hermit --test leveldb full_leveldb_suite_is_deterministic_under_strict -- --exact --include-ignored --test-threads=1 # [run] hermit-cli/tests/leveldb.rs::full_leveldb_suite_is_deterministic_under_strict
cargo test -p hermit --test leveldb leveldb_env_posix_is_deterministic_under_strict -- --exact --include-ignored --test-threads=1 # [run] hermit-cli/tests/leveldb.rs::leveldb_env_posix_is_deterministic_under_strict
cargo test -p hermit --test liteinst_advanced -- --include-ignored --test-threads=1 # [verify] all tests in hermit-cli/tests/liteinst_advanced.rs
cargo test -p hermit --test liteinst_advanced liteinst_detcore_strict_verify_micro_suite -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/liteinst_advanced.rs::liteinst_detcore_strict_verify_micro_suite
cargo test -p hermit --test liteinst_advanced liteinst_thread_clone_fails_closed_without_sigsys -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/liteinst_advanced.rs::liteinst_thread_clone_fails_closed_without_sigsys
cargo test -p hermit --test liteinst_advanced liteinst_fork_fails_closed_without_hanging -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/liteinst_advanced.rs::liteinst_fork_fails_closed_without_hanging
cargo test -p hermit --test liteinst_advanced liteinst_abnormal_exit_after_registration_does_not_hang -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/liteinst_advanced.rs::liteinst_abnormal_exit_after_registration_does_not_hang
cargo test -p hermit --test madvise -- --include-ignored --test-threads=1 # [both/mixed] all tests in hermit-cli/tests/madvise.rs
cargo test -p hermit --test madvise madvise_policy_verifies_in_run_record_and_kvm_modes -- --exact --include-ignored --test-threads=1 # [both/mixed] hermit-cli/tests/madvise.rs::madvise_policy_verifies_in_run_record_and_kvm_modes
cargo test -p hermit --test meminfo_determinism -- --include-ignored --test-threads=1 # [verify] all tests in hermit-cli/tests/meminfo_determinism.rs
cargo test -p hermit --test meminfo_determinism meminfo_fields_and_free_use_guest_memory -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/meminfo_determinism.rs::meminfo_fields_and_free_use_guest_memory
cargo test -p hermit --test mmap_determinism -- --include-ignored --test-threads=1 # [run] all tests in hermit-cli/tests/mmap_determinism.rs
cargo test -p hermit --test mmap_determinism multiple_mmap_addresses_are_deterministic -- --exact --include-ignored --test-threads=1 # [run] hermit-cli/tests/mmap_determinism.rs::multiple_mmap_addresses_are_deterministic
cargo test -p hermit --test mmap_determinism map_fixed_address_is_deterministic -- --exact --include-ignored --test-threads=1 # [run] hermit-cli/tests/mmap_determinism.rs::map_fixed_address_is_deterministic
cargo test -p hermit --test mmap_determinism brk_and_sbrk_addresses_are_deterministic -- --exact --include-ignored --test-threads=1 # [run] hermit-cli/tests/mmap_determinism.rs::brk_and_sbrk_addresses_are_deterministic
cargo test -p hermit --test mmap_determinism map_shared_address_is_deterministic -- --exact --include-ignored --test-threads=1 # [run] hermit-cli/tests/mmap_determinism.rs::map_shared_address_is_deterministic
cargo test -p hermit --test mmap_determinism mmap_reuses_unmapped_address_deterministically -- --exact --include-ignored --test-threads=1 # [run] hermit-cli/tests/mmap_determinism.rs::mmap_reuses_unmapped_address_deterministically
cargo test -p hermit --test mount_introspection -- --include-ignored --test-threads=1 # [verify] all tests in hermit-cli/tests/mount_introspection.rs
cargo test -p hermit --test mount_introspection mount_introspection_syscalls_fall_back_deterministically -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/mount_introspection.rs::mount_introspection_syscalls_fall_back_deterministically
cargo test -p hermit --test name_to_handle_refusal -- --include-ignored --test-threads=1 # [verify] all tests in hermit-cli/tests/name_to_handle_refusal.rs
cargo test -p hermit --test name_to_handle_refusal filesystem_handle_export_refusals_verify -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/name_to_handle_refusal.rs::filesystem_handle_export_refusals_verify
cargo test -p hermit --test netlink_table_determinism -- --include-ignored --test-threads=1 # [verify] all tests in hermit-cli/tests/netlink_table_determinism.rs
cargo test -p hermit --test netlink_table_determinism netlink_table_consumers_verify -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/netlink_table_determinism.rs::netlink_table_consumers_verify
cargo test -p hermit --test netns_cookie_determinism -- --include-ignored --test-threads=1 # [verify] all tests in hermit-cli/tests/netns_cookie_determinism.rs
cargo test -p hermit --test netns_cookie_determinism network_namespace_cookie_verifies_for_distinct_socket_programs -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/netns_cookie_determinism.rs::network_namespace_cookie_verifies_for_distinct_socket_programs
cargo test -p hermit --test node_vmstat_determinism -- --include-ignored --test-threads=1 # [verify] all tests in hermit-cli/tests/node_vmstat_determinism.rs
cargo test -p hermit --test node_vmstat_determinism node_vmstat_consumers_are_deterministic_under_strict_verify -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/node_vmstat_determinism.rs::node_vmstat_consumers_are_deterministic_under_strict_verify
cargo test -p hermit --test numa_maps_determinism -- --include-ignored --test-threads=1 # [verify] all tests in hermit-cli/tests/numa_maps_determinism.rs
cargo test -p hermit --test numa_maps_determinism numa_maps_consumers_are_deterministic_under_strict_verify -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/numa_maps_determinism.rs::numa_maps_consumers_are_deterministic_under_strict_verify
cargo test -p hermit --test optional_memory_features -- --include-ignored --test-threads=1 # [verify] all tests in hermit-cli/tests/optional_memory_features.rs
cargo test -p hermit --test optional_memory_features optional_memory_features_fall_back_deterministically -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/optional_memory_features.rs::optional_memory_features_fall_back_deterministically
cargo test -p hermit --test perf_event_refusal -- --include-ignored --test-threads=1 # [verify] all tests in hermit-cli/tests/perf_event_refusal.rs
cargo test -p hermit --test perf_event_refusal perf_event_refusals_verify -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/perf_event_refusal.rs::perf_event_refusals_verify
cargo test -p hermit --test pidfd_creation -- --include-ignored --test-threads=1 # [verify] all tests in hermit-cli/tests/pidfd_creation.rs
cargo test -p hermit --test pidfd_creation pidfd_creation_is_tracked_across_descriptor_operations -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/pidfd_creation.rs::pidfd_creation_is_tracked_across_descriptor_operations
cargo test -p hermit --test ppoll_simulation -- --include-ignored --test-threads=1 # [verify] all tests in hermit-cli/tests/ppoll_simulation.rs
cargo test -p hermit --test ppoll_simulation ppoll_waits_use_nonblocking_probes_and_verify -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/ppoll_simulation.rs::ppoll_waits_use_nonblocking_probes_and_verify
cargo test -p hermit --test prctl_dumpable_determinism -- --include-ignored --test-threads=1 # [verify] all tests in hermit-cli/tests/prctl_dumpable_determinism.rs
cargo test -p hermit --test prctl_dumpable_determinism dumpability_controls_are_deterministic -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/prctl_dumpable_determinism.rs::dumpability_controls_are_deterministic
cargo test -p hermit --test privileged_observation -- --include-ignored --test-threads=1 # [verify] all tests in hermit-cli/tests/privileged_observation.rs
cargo test -p hermit --test privileged_observation privileged_observation_is_refused_deterministically -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/privileged_observation.rs::privileged_observation_is_refused_deterministically
cargo test -p hermit --test proc_fd_link_determinism -- --include-ignored --test-threads=1 # [verify] all tests in hermit-cli/tests/proc_fd_link_determinism.rs
cargo test -p hermit --test proc_fd_link_determinism proc_fd_link_consumers_verify -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/proc_fd_link_determinism.rs::proc_fd_link_consumers_verify
cargo test -p hermit --test proc_fd_link_determinism proc_fd_link_aliases_and_truncation_verify -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/proc_fd_link_determinism.rs::proc_fd_link_aliases_and_truncation_verify
cargo test -p hermit --test proc_fdinfo_determinism -- --include-ignored --test-threads=1 # [verify] all tests in hermit-cli/tests/proc_fdinfo_determinism.rs
cargo test -p hermit --test proc_fdinfo_determinism proc_fdinfo_consumers_are_deterministic_under_strict_verify -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/proc_fdinfo_determinism.rs::proc_fdinfo_consumers_are_deterministic_under_strict_verify
cargo test -p hermit --test proc_locks_determinism -- --include-ignored --test-threads=1 # [verify] all tests in hermit-cli/tests/proc_locks_determinism.rs
cargo test -p hermit --test proc_locks_determinism proc_locks_consumers_are_deterministic_under_strict_verify -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/proc_locks_determinism.rs::proc_locks_consumers_are_deterministic_under_strict_verify
cargo test -p hermit --test process_isolation_refusals -- --include-ignored --test-threads=1 # [verify] all tests in hermit-cli/tests/process_isolation_refusals.rs
cargo test -p hermit --test process_isolation_refusals process_isolation_refusals_verify -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/process_isolation_refusals.rs::process_isolation_refusals_verify
cargo test -p hermit --test procfs_determinism -- --include-ignored --test-threads=1 # [run] all tests in hermit-cli/tests/procfs_determinism.rs
cargo test -p hermit --test procfs_determinism proc_self_maps_is_deterministic -- --exact --include-ignored --test-threads=1 # [run] hermit-cli/tests/procfs_determinism.rs::proc_self_maps_is_deterministic
cargo test -p hermit --test procfs_determinism proc_self_stat_is_deterministic -- --exact --include-ignored --test-threads=1 # [run] hermit-cli/tests/procfs_determinism.rs::proc_self_stat_is_deterministic
cargo test -p hermit --test procfs_determinism proc_self_status_is_deterministic -- --exact --include-ignored --test-threads=1 # [run] hermit-cli/tests/procfs_determinism.rs::proc_self_status_is_deterministic
cargo test -p hermit --test procfs_determinism proc_self_cmdline_is_deterministic -- --exact --include-ignored --test-threads=1 # [run] hermit-cli/tests/procfs_determinism.rs::proc_self_cmdline_is_deterministic
cargo test -p hermit --test procfs_determinism proc_system_cpu_accounting_is_deterministic -- --exact --include-ignored --test-threads=1 # [run] hermit-cli/tests/procfs_determinism.rs::proc_system_cpu_accounting_is_deterministic
cargo test -p hermit --test procfs_determinism proc_vm_accounting_is_deterministic -- --exact --include-ignored --test-threads=1 # [run] hermit-cli/tests/procfs_determinism.rs::proc_vm_accounting_is_deterministic
cargo test -p hermit --test procfs_determinism proc_pid_stat_accounting_is_deterministic -- --exact --include-ignored --test-threads=1 # [run] hermit-cli/tests/procfs_determinism.rs::proc_pid_stat_accounting_is_deterministic
cargo test -p hermit --test procfs_determinism proc_pid_statm_accounting_is_deterministic -- --exact --include-ignored --test-threads=1 # [run] hermit-cli/tests/procfs_determinism.rs::proc_pid_statm_accounting_is_deterministic
cargo test -p hermit --test procfs_determinism proc_pid_status_accounting_is_deterministic -- --exact --include-ignored --test-threads=1 # [run] hermit-cli/tests/procfs_determinism.rs::proc_pid_status_accounting_is_deterministic
cargo test -p hermit --test procfs_determinism proc_diskstats_uses_synthetic_counters -- --exact --include-ignored --test-threads=1 # [run] hermit-cli/tests/procfs_determinism.rs::proc_diskstats_uses_synthetic_counters
cargo test -p hermit --test procfs_determinism proc_pid_io_uses_zero_counters -- --exact --include-ignored --test-threads=1 # [run] hermit-cli/tests/procfs_determinism.rs::proc_pid_io_uses_zero_counters
cargo test -p hermit --test procfs_determinism proc_cpuinfo_is_deterministic -- --exact --include-ignored --test-threads=1 # [run] hermit-cli/tests/procfs_determinism.rs::proc_cpuinfo_is_deterministic
cargo test -p hermit --test procfs_determinism proc_loadavg_uses_virtual_values -- --exact --include-ignored --test-threads=1 # [run] hermit-cli/tests/procfs_determinism.rs::proc_loadavg_uses_virtual_values
cargo test -p hermit --test procfs_determinism proc_uptime_uses_virtual_time -- --exact --include-ignored --test-threads=1 # [run] hermit-cli/tests/procfs_determinism.rs::proc_uptime_uses_virtual_time
cargo test -p hermit --test procfs_determinism proc_entropy_available_is_deterministic -- --exact --include-ignored --test-threads=1 # [run] hermit-cli/tests/procfs_determinism.rs::proc_entropy_available_is_deterministic
cargo test -p hermit --test procfs_determinism proc_pressure_uses_virtual_zero_values -- --exact --include-ignored --test-threads=1 # [run] hermit-cli/tests/procfs_determinism.rs::proc_pressure_uses_virtual_zero_values
cargo test -p hermit --test procfs_determinism proc_interrupt_accounting_is_deterministic -- --exact --include-ignored --test-threads=1 # [run] hermit-cli/tests/procfs_determinism.rs::proc_interrupt_accounting_is_deterministic
cargo test -p hermit --test procfs_determinism proc_schedstat_uses_virtual_zero_values -- --exact --include-ignored --test-threads=1 # [run] hermit-cli/tests/procfs_determinism.rs::proc_schedstat_uses_virtual_zero_values
cargo test -p hermit --test procfs_determinism proc_zoneinfo_uses_virtual_zero_values -- --exact --include-ignored --test-threads=1 # [run] hermit-cli/tests/procfs_determinism.rs::proc_zoneinfo_uses_virtual_zero_values
cargo test -p hermit --test procfs_determinism proc_rtc_tracks_custom_epoch_and_virtual_time -- --exact --include-ignored --test-threads=1 # [run] hermit-cli/tests/procfs_determinism.rs::proc_rtc_tracks_custom_epoch_and_virtual_time
cargo test -p hermit --test procfs_determinism proc_self_mountinfo_hides_private_temp_roots -- --exact --include-ignored --test-threads=1 # [run] hermit-cli/tests/procfs_determinism.rs::proc_self_mountinfo_hides_private_temp_roots
cargo test -p hermit --test procfs_determinism proc_random_uuid_is_deterministic -- --exact --include-ignored --test-threads=1 # [run] hermit-cli/tests/procfs_determinism.rs::proc_random_uuid_is_deterministic
cargo test -p hermit --test procfs_determinism proc_modules_are_deterministic -- --exact --include-ignored --test-threads=1 # [run] hermit-cli/tests/procfs_determinism.rs::proc_modules_are_deterministic
cargo test -p hermit --test procfs_determinism sysfs_numa_accounting_is_deterministic -- --exact --include-ignored --test-threads=1 # [run] hermit-cli/tests/procfs_determinism.rs::sysfs_numa_accounting_is_deterministic
cargo test -p hermit --test procfs_determinism sysfs_hwmon_input_is_deterministic_when_available -- --exact --include-ignored --test-threads=1 # [run] hermit-cli/tests/procfs_determinism.rs::sysfs_hwmon_input_is_deterministic_when_available
cargo test -p hermit --test procfs_positioned_determinism -- --include-ignored --test-threads=1 # [verify] all tests in hermit-cli/tests/procfs_positioned_determinism.rs
cargo test -p hermit --test procfs_positioned_determinism procfs_positioned_reads_are_mediated_and_deterministic -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/procfs_positioned_determinism.rs::procfs_positioned_reads_are_mediated_and_deterministic
cargo test -p hermit --test protocols_determinism -- --include-ignored --test-threads=1 # [verify] all tests in hermit-cli/tests/protocols_determinism.rs
cargo test -p hermit --test protocols_determinism protocol_consumers_are_deterministic_under_strict_verify -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/protocols_determinism.rs::protocol_consumers_are_deterministic_under_strict_verify
cargo test -p hermit --test pselect6_simulation -- --include-ignored --test-threads=1 # [verify] all tests in hermit-cli/tests/pselect6_simulation.rs
cargo test -p hermit --test pselect6_simulation pselect6_preserves_kernel_abi_and_unblocks_scheduler -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/pselect6_simulation.rs::pselect6_preserves_kernel_abi_and_unblocks_scheduler
cargo test -p hermit --test ptrace_refusal -- --include-ignored --test-threads=1 # [verify] all tests in hermit-cli/tests/ptrace_refusal.rs
cargo test -p hermit --test ptrace_refusal guest_ptrace_refusals_verify -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/ptrace_refusal.rs::guest_ptrace_refusals_verify
cargo test -p hermit --test pty_nr_determinism -- --include-ignored --test-threads=1 # [verify] all tests in hermit-cli/tests/pty_nr_determinism.rs
cargo test -p hermit --test pty_nr_determinism pty_nr_consumers_verify -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/pty_nr_determinism.rs::pty_nr_consumers_verify
cargo test -p hermit --test python_stdlib -- --include-ignored --test-threads=1 # [run] all tests in hermit-cli/tests/python_stdlib.rs
cargo test -p hermit --test python_stdlib zero_case_module_is_rejected -- --exact --include-ignored --test-threads=1 # [run] hermit-cli/tests/python_stdlib.rs::zero_case_module_is_rejected
cargo test -p hermit --test python_stdlib strict_python_stdlib_is_deterministic -- --exact --include-ignored --test-threads=1 # [run] hermit-cli/tests/python_stdlib.rs::strict_python_stdlib_is_deterministic
cargo test -p hermit --test random_determinism -- --include-ignored --test-threads=1 # [verify] all tests in hermit-cli/tests/random_determinism.rs
cargo test -p hermit --test random_determinism random_sources_repeat_across_runs_and_change_with_seed -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/random_determinism.rs::random_sources_repeat_across_runs_and_change_with_seed
cargo test -p hermit --test random_determinism random_sources_are_deterministic_under_strict_verify -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/random_determinism.rs::random_sources_are_deterministic_under_strict_verify
cargo test -p hermit --test random_uuid_determinism -- --include-ignored --test-threads=1 # [verify] all tests in hermit-cli/tests/random_uuid_determinism.rs
cargo test -p hermit --test random_uuid_determinism random_uuid_consumers_verify -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/random_uuid_determinism.rs::random_uuid_consumers_verify
cargo test -p hermit --test rcx_canonicalization -- --include-ignored --test-threads=1 # [run] all tests in hermit-cli/tests/rcx_canonicalization.rs
cargo test -p hermit --test rcx_canonicalization rcx_r11_are_canonical_and_deterministic_under_strict -- --exact --include-ignored --test-threads=1 # [run] hermit-cli/tests/rcx_canonicalization.rs::rcx_r11_are_canonical_and_deterministic_under_strict
cargo test -p hermit --test record_replay -- --include-ignored --test-threads=1 # [both/mixed] all tests in hermit-cli/tests/record_replay.rs
cargo test -p hermit --test record_replay record_strict_direct_cli_records_and_replays_echo -- --exact --include-ignored --test-threads=1 # [both/mixed] hermit-cli/tests/record_replay.rs::record_strict_direct_cli_records_and_replays_echo
cargo test -p hermit --test record_replay record_replay_matrix -- --exact --include-ignored --test-threads=1 # [both/mixed] hermit-cli/tests/record_replay.rs::record_replay_matrix
cargo test -p hermit --test record_replay record_reopened_inherited_and_cloned_file_state -- --exact --include-ignored --test-threads=1 # [both/mixed] hermit-cli/tests/record_replay.rs::record_reopened_inherited_and_cloned_file_state
cargo test -p hermit --test record_replay record_find_directory_tree -- --exact --include-ignored --test-threads=1 # [both/mixed] hermit-cli/tests/record_replay.rs::record_find_directory_tree
cargo test -p hermit --test record_replay record_mkdir_and_rmdir_side_effects -- --exact --include-ignored --test-threads=1 # [both/mixed] hermit-cli/tests/record_replay.rs::record_mkdir_and_rmdir_side_effects
cargo test -p hermit --test record_replay record_nested_mkdir_side_effects -- --exact --include-ignored --test-threads=1 # [both/mixed] hermit-cli/tests/record_replay.rs::record_nested_mkdir_side_effects
cargo test -p hermit --test record_replay record_writable_filesystem_side_effects -- --exact --include-ignored --test-threads=1 # [both/mixed] hermit-cli/tests/record_replay.rs::record_writable_filesystem_side_effects
cargo test -p hermit --test record_replay record_mkfifo_in_replay_tmp -- --exact --include-ignored --test-threads=1 # [both/mixed] hermit-cli/tests/record_replay.rs::record_mkfifo_in_replay_tmp
cargo test -p hermit --test record_replay record_shell_forked_external_command -- --exact --include-ignored --test-threads=1 # [both/mixed] hermit-cli/tests/record_replay.rs::record_shell_forked_external_command
cargo test -p hermit --test record_replay record_shell_sigpipe_pipeline -- --exact --include-ignored --test-threads=1 # [both/mixed] hermit-cli/tests/record_replay.rs::record_shell_sigpipe_pipeline
cargo test -p hermit --test record_replay record_shell_pipeline_stdout_matches -- --exact --include-ignored --test-threads=1 # [both/mixed] hermit-cli/tests/record_replay.rs::record_shell_pipeline_stdout_matches
cargo test -p hermit --test record_replay record_large_captured_output_does_not_deadlock -- --exact --include-ignored --test-threads=1 # [both/mixed] hermit-cli/tests/record_replay.rs::record_large_captured_output_does_not_deadlock
cargo test -p hermit --test record_replay record_shell_command_substitution_stdout_matches -- --exact --include-ignored --test-threads=1 # [both/mixed] hermit-cli/tests/record_replay.rs::record_shell_command_substitution_stdout_matches
cargo test -p hermit --test record_replay record_shell_redirected_stdout_stays_hidden -- --exact --include-ignored --test-threads=1 # [both/mixed] hermit-cli/tests/record_replay.rs::record_shell_redirected_stdout_stays_hidden
cargo test -p hermit --test record_replay record_shell_original_output_aliases_and_swaps -- --exact --include-ignored --test-threads=1 # [both/mixed] hermit-cli/tests/record_replay.rs::record_shell_original_output_aliases_and_swaps
cargo test -p hermit --test record_replay record_curl_version -- --exact --include-ignored --test-threads=1 # [both/mixed] hermit-cli/tests/record_replay.rs::record_curl_version
cargo test -p hermit --test record_replay record_node_eventfd_epoll_sequence -- --exact --include-ignored --test-threads=1 # [both/mixed] hermit-cli/tests/record_replay.rs::record_node_eventfd_epoll_sequence
cargo test -p hermit --test record_replay record_sqlite_memory_query -- --exact --include-ignored --test-threads=1 # [both/mixed] hermit-cli/tests/record_replay.rs::record_sqlite_memory_query
cargo test -p hermit --test record_replay record_timeout_kills_guest_without_committing_partial_data -- --exact --include-ignored --test-threads=1 # [both/mixed] hermit-cli/tests/record_replay.rs::record_timeout_kills_guest_without_committing_partial_data
cargo test -p hermit --test record_replay record_timeout_fires_even_when_sigalrm_is_blocked -- --exact --include-ignored --test-threads=1 # [both/mixed] hermit-cli/tests/record_replay.rs::record_timeout_fires_even_when_sigalrm_is_blocked
cargo test -p hermit --test record_replay record_timeout_preserves_existing_last -- --exact --include-ignored --test-threads=1 # [both/mixed] hermit-cli/tests/record_replay.rs::record_timeout_preserves_existing_last
cargo test -p hermit --test record_replay record_timeout_terminates_descendant_processes -- --exact --include-ignored --test-threads=1 # [both/mixed] hermit-cli/tests/record_replay.rs::record_timeout_terminates_descendant_processes
cargo test -p hermit --test record_replay record_pidfd_open_modeled_descriptor_ops -- --exact --include-ignored --test-threads=1 # [both/mixed] hermit-cli/tests/record_replay.rs::record_pidfd_open_modeled_descriptor_ops
cargo test -p hermit --test redis_strict -- --include-ignored --test-threads=1 # [run] all tests in hermit-cli/tests/redis_strict.rs
cargo test -p hermit --test redis_strict redis_small_subset_is_deterministic_under_hermit -- --exact --include-ignored --test-threads=1 # [run] hermit-cli/tests/redis_strict.rs::redis_small_subset_is_deterministic_under_hermit
cargo test -p hermit --test redis_strict redis_persistence_restart_is_deterministic_under_hermit -- --exact --include-ignored --test-threads=1 # [run] hermit-cli/tests/redis_strict.rs::redis_persistence_restart_is_deterministic_under_hermit
cargo test -p hermit --test redis_strict redis_workload_refuses_to_control_a_preexisting_server -- --exact --include-ignored --test-threads=1 # [run] hermit-cli/tests/redis_strict.rs::redis_workload_refuses_to_control_a_preexisting_server
cargo test -p hermit --test redis_strict redis_source_build_and_extended_suite_under_strict_hermit -- --exact --include-ignored --test-threads=1 # [run] hermit-cli/tests/redis_strict.rs::redis_source_build_and_extended_suite_under_strict_hermit
cargo test -p hermit --test remap_file_pages_refusal -- --include-ignored --test-threads=1 # [verify] all tests in hermit-cli/tests/remap_file_pages_refusal.rs
cargo test -p hermit --test remap_file_pages_refusal remap_file_pages_refusals_verify -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/remap_file_pages_refusal.rs::remap_file_pages_refusals_verify
cargo test -p hermit --test robust_list_queries -- --include-ignored --test-threads=1 # [verify] all tests in hermit-cli/tests/robust_list_queries.rs
cargo test -p hermit --test robust_list_queries robust_list_queries_verify -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/robust_list_queries.rs::robust_list_queries_verify
cargo test -p hermit --test rr_suite -- --include-ignored --test-threads=1 # [run] all tests in hermit-cli/tests/rr_suite.rs
cargo test -p hermit --test rr_suite rr_scratch_directories_are_fresh_and_cleaned -- --exact --include-ignored --test-threads=1 # [run] hermit-cli/tests/rr_suite.rs::rr_scratch_directories_are_fresh_and_cleaned
cargo test -p hermit --test rr_suite rr_pause -- --exact --include-ignored --test-threads=1 # [run] hermit-cli/tests/rr_suite.rs::rr_pause
cargo test -p hermit --test sched_setattr_noop -- --include-ignored --test-threads=1 # [verify] all tests in hermit-cli/tests/sched_setattr_noop.rs
cargo test -p hermit --test sched_setattr_noop scheduler_policy_setters_verify -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/sched_setattr_noop.rs::scheduler_policy_setters_verify
cargo test -p hermit --test scheduler_policy_queries -- --include-ignored --test-threads=1 # [verify] all tests in hermit-cli/tests/scheduler_policy_queries.rs
cargo test -p hermit --test scheduler_policy_queries scheduler_policy_queries_are_deterministic -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/scheduler_policy_queries.rs::scheduler_policy_queries_are_deterministic
cargo test -p hermit --test self_sched_determinism -- --include-ignored --test-threads=1 # [verify] all tests in hermit-cli/tests/self_sched_determinism.rs
cargo test -p hermit --test self_sched_determinism self_sched_consumers_are_deterministic_under_strict_verify -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/self_sched_determinism.rs::self_sched_consumers_are_deterministic_under_strict_verify
cargo test -p hermit --test self_schedstat_determinism -- --include-ignored --test-threads=1 # [verify] all tests in hermit-cli/tests/self_schedstat_determinism.rs
cargo test -p hermit --test self_schedstat_determinism self_schedstat_consumers_are_deterministic_under_strict_verify -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/self_schedstat_determinism.rs::self_schedstat_consumers_are_deterministic_under_strict_verify
cargo test -p hermit --test signal_determinism -- --include-ignored --test-threads=1 # [run] all tests in hermit-cli/tests/signal_determinism.rs
cargo test -p hermit --test signal_determinism sigalrm_itimer_delivery_is_deterministic -- --exact --include-ignored --test-threads=1 # [run] hermit-cli/tests/signal_determinism.rs::sigalrm_itimer_delivery_is_deterministic
cargo test -p hermit --test signal_determinism armed_itimer_is_discarded_on_process_exit -- --exact --include-ignored --test-threads=1 # [run] hermit-cli/tests/signal_determinism.rs::armed_itimer_is_discarded_on_process_exit
cargo test -p hermit --test signal_determinism signal_interrupts_emulated_blocking_read -- --exact --include-ignored --test-threads=1 # [run] hermit-cli/tests/signal_determinism.rs::signal_interrupts_emulated_blocking_read
cargo test -p hermit --test signal_determinism signal_restarts_emulated_blocking_read -- --exact --include-ignored --test-threads=1 # [run] hermit-cli/tests/signal_determinism.rs::signal_restarts_emulated_blocking_read
cargo test -p hermit --test signal_determinism signal_interrupts_poll_despite_sa_restart -- --exact --include-ignored --test-threads=1 # [run] hermit-cli/tests/signal_determinism.rs::signal_interrupts_poll_despite_sa_restart
cargo test -p hermit --test signal_determinism signal_interrupts_epoll_wait_despite_sa_restart -- --exact --include-ignored --test-threads=1 # [run] hermit-cli/tests/signal_determinism.rs::signal_interrupts_epoll_wait_despite_sa_restart
cargo test -p hermit --test signal_determinism signal_interrupts_rt_sigtimedwait_despite_sa_restart -- --exact --include-ignored --test-threads=1 # [run] hermit-cli/tests/signal_determinism.rs::signal_interrupts_rt_sigtimedwait_despite_sa_restart
cargo test -p hermit --test signal_determinism blocking_sigsuspend_releases_the_scheduler -- --exact --include-ignored --test-threads=1 # [run] hermit-cli/tests/signal_determinism.rs::blocking_sigsuspend_releases_the_scheduler
cargo test -p hermit --test signal_determinism signal_masks_survive_fork_and_clone -- --exact --include-ignored --test-threads=1 # [run] hermit-cli/tests/signal_determinism.rs::signal_masks_survive_fork_and_clone
cargo test -p hermit --test signal_determinism signal_handler_reentrance_is_deterministic -- --exact --include-ignored --test-threads=1 # [run] hermit-cli/tests/signal_determinism.rs::signal_handler_reentrance_is_deterministic
cargo test -p hermit --test signal_determinism alternate_signal_stack_is_preserved -- --exact --include-ignored --test-threads=1 # [run] hermit-cli/tests/signal_determinism.rs::alternate_signal_stack_is_preserved
cargo test -p hermit --test signal_determinism pending_signal_and_mask_survive_exec -- --exact --include-ignored --test-threads=1 # [run] hermit-cli/tests/signal_determinism.rs::pending_signal_and_mask_survive_exec
cargo test -p hermit --test smaps_determinism -- --include-ignored --test-threads=1 # [verify] all tests in hermit-cli/tests/smaps_determinism.rs
cargo test -p hermit --test smaps_determinism smaps_consumers_are_deterministic_under_strict_verify -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/smaps_determinism.rs::smaps_consumers_are_deterministic_under_strict_verify
cargo test -p hermit --test smaps_rollup_determinism -- --include-ignored --test-threads=1 # [verify] all tests in hermit-cli/tests/smaps_rollup_determinism.rs
cargo test -p hermit --test smaps_rollup_determinism smaps_rollup_consumers_are_deterministic_under_strict_verify -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/smaps_rollup_determinism.rs::smaps_rollup_consumers_are_deterministic_under_strict_verify
cargo test -p hermit --test so_incoming_cpu -- --include-ignored --test-threads=1 # [verify] all tests in hermit-cli/tests/so_incoming_cpu.rs
cargo test -p hermit --test so_incoming_cpu incoming_cpu_is_the_virtual_cpu_under_strict_verify -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/so_incoming_cpu.rs::incoming_cpu_is_the_virtual_cpu_under_strict_verify
cargo test -p hermit --test socket_cookie_determinism -- --include-ignored --test-threads=1 # [verify] all tests in hermit-cli/tests/socket_cookie_determinism.rs
cargo test -p hermit --test socket_cookie_determinism socket_cookies_verify_for_distinct_socket_families -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/socket_cookie_determinism.rs::socket_cookies_verify_for_distinct_socket_families
cargo test -p hermit --test socket_ioctl_timestamp_determinism -- --include-ignored --test-threads=1 # [verify] all tests in hermit-cli/tests/socket_ioctl_timestamp_determinism.rs
cargo test -p hermit --test socket_ioctl_timestamp_determinism socket_timestamp_ioctls_use_logical_time -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/socket_ioctl_timestamp_determinism.rs::socket_timestamp_ioctls_use_logical_time
cargo test -p hermit --test socket_timestamp_determinism -- --include-ignored --test-threads=1 # [verify] all tests in hermit-cli/tests/socket_timestamp_determinism.rs
cargo test -p hermit --test socket_timestamp_determinism socket_receive_timestamps_use_logical_time -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/socket_timestamp_determinism.rs::socket_receive_timestamps_use_logical_time
cargo test -p hermit --test sockstat_determinism -- --include-ignored --test-threads=1 # [verify] all tests in hermit-cli/tests/sockstat_determinism.rs
cargo test -p hermit --test sockstat_determinism sockstat_consumers_are_deterministic_under_strict_verify -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/sockstat_determinism.rs::sockstat_consumers_are_deterministic_under_strict_verify
cargo test -p hermit --test softnet_stat_determinism -- --include-ignored --test-threads=1 # [verify] all tests in hermit-cli/tests/softnet_stat_determinism.rs
cargo test -p hermit --test softnet_stat_determinism softnet_stat_consumers_are_deterministic_under_strict_verify -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/softnet_stat_determinism.rs::softnet_stat_consumers_are_deterministic_under_strict_verify
cargo test -p hermit --test sqlite_veryquick -- --include-ignored --test-threads=1 # [run] all tests in hermit-cli/tests/sqlite_veryquick.rs
cargo test -p hermit --test sqlite_veryquick sqlite_fast_subset_is_deterministic_under_hermit -- --exact --include-ignored --test-threads=1 # [run] hermit-cli/tests/sqlite_veryquick.rs::sqlite_fast_subset_is_deterministic_under_hermit
cargo test -p hermit --test sqlite_veryquick sqlite_veryquick_is_deterministic_under_strict_hermit -- --exact --include-ignored --test-threads=1 # [run] hermit-cli/tests/sqlite_veryquick.rs::sqlite_veryquick_is_deterministic_under_strict_hermit
cargo test -p hermit --test stress_suite -- --include-ignored --test-threads=1 # [run] all tests in hermit-cli/tests/stress_suite.rs
cargo test -p hermit --test stress_suite chaos_finds_and_reproduces_order_violation -- --exact --include-ignored --test-threads=1 # [run] hermit-cli/tests/stress_suite.rs::chaos_finds_and_reproduces_order_violation
cargo test -p hermit --test stress_suite targeted_chaos_finds_order_violation_at_least_as_often -- --exact --include-ignored --test-threads=1 # [run] hermit-cli/tests/stress_suite.rs::targeted_chaos_finds_order_violation_at_least_as_often
cargo test -p hermit --test stress_suite fast_chaos_matrix -- --exact --include-ignored --test-threads=1 # [run] hermit-cli/tests/stress_suite.rs::fast_chaos_matrix
cargo test -p hermit --test stress_suite slow_race_matrix -- --exact --include-ignored --test-threads=1 # [run] hermit-cli/tests/stress_suite.rs::slow_race_matrix
cargo test -p hermit --test stress_suite schedule_bisect_localizes_publish_ordering_race -- --exact --include-ignored --test-threads=1 # [run] hermit-cli/tests/stress_suite.rs::schedule_bisect_localizes_publish_ordering_race
cargo test -p hermit --test stress_suite slow_cas_search_and_replay -- --exact --include-ignored --test-threads=1 # [run] hermit-cli/tests/stress_suite.rs::slow_cas_search_and_replay
cargo test -p hermit --test swaps_determinism -- --include-ignored --test-threads=1 # [verify] all tests in hermit-cli/tests/swaps_determinism.rs
cargo test -p hermit --test swaps_determinism swaps_consumers_are_deterministic_under_strict_verify -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/swaps_determinism.rs::swaps_consumers_are_deterministic_under_strict_verify
cargo test -p hermit --test syscall_file_io -- --include-ignored --test-threads=1 # [verify] all tests in hermit-cli/tests/syscall_file_io.rs
cargo test -p hermit --test syscall_file_io deterministic_file_io_syscalls_verify -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/syscall_file_io.rs::deterministic_file_io_syscalls_verify
cargo test -p hermit --test syscall_file_metadata -- --include-ignored --test-threads=1 # [verify] all tests in hermit-cli/tests/syscall_file_metadata.rs
cargo test -p hermit --test syscall_file_metadata deterministic_file_metadata_syscalls_verify -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/syscall_file_metadata.rs::deterministic_file_metadata_syscalls_verify
cargo test -p hermit --test syscall_quick_wins -- --include-ignored --test-threads=1 # [verify] all tests in hermit-cli/tests/syscall_quick_wins.rs
cargo test -p hermit --test syscall_quick_wins deterministic_passthrough_syscalls_verify -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/syscall_quick_wins.rs::deterministic_passthrough_syscalls_verify
cargo test -p hermit --test sysfs_rtc_determinism -- --include-ignored --test-threads=1 # [verify] all tests in hermit-cli/tests/sysfs_rtc_determinism.rs
cargo test -p hermit --test sysfs_rtc_determinism sysfs_rtc_consumers_verify -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/sysfs_rtc_determinism.rs::sysfs_rtc_consumers_verify
cargo test -p hermit --test sysv_legacy_fallbacks -- --include-ignored --test-threads=1 # [verify] all tests in hermit-cli/tests/sysv_legacy_fallbacks.rs
cargo test -p hermit --test sysv_legacy_fallbacks sysv_and_legacy_filesystem_features_fall_back_deterministically -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/sysv_legacy_fallbacks.rs::sysv_and_legacy_filesystem_features_fall_back_deterministically
cargo test -p hermit --test tcp_info_determinism -- --include-ignored --test-threads=1 # [verify] all tests in hermit-cli/tests/tcp_info_determinism.rs
cargo test -p hermit --test tcp_info_determinism tcp_info_hides_host_transport_counters_under_strict_verify -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/tcp_info_determinism.rs::tcp_info_hides_host_transport_counters_under_strict_verify
cargo test -p hermit --test thp_stats_determinism -- --include-ignored --test-threads=1 # [verify] all tests in hermit-cli/tests/thp_stats_determinism.rs
cargo test -p hermit --test thp_stats_determinism transparent_hugepage_stat_consumers_verify -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/thp_stats_determinism.rs::transparent_hugepage_stat_consumers_verify
cargo test -p hermit --test thread_scheduling_fairness -- --include-ignored --test-threads=1 # [run] all tests in hermit-cli/tests/thread_scheduling_fairness.rs
cargo test -p hermit --test thread_scheduling_fairness four_runnable_threads_receive_round_robin_progress -- --exact --include-ignored --test-threads=1 # [run] hermit-cli/tests/thread_scheduling_fairness.rs::four_runnable_threads_receive_round_robin_progress
cargo test -p hermit --test thread_scheduling_fairness bounded_buffer_producer_and_consumers_complete -- --exact --include-ignored --test-threads=1 # [run] hermit-cli/tests/thread_scheduling_fairness.rs::bounded_buffer_producer_and_consumers_complete
cargo test -p hermit --test thread_scheduling_fairness rwlock_writer_is_not_starved_by_readers -- --exact --include-ignored --test-threads=1 # [run] hermit-cli/tests/thread_scheduling_fairness.rs::rwlock_writer_is_not_starved_by_readers
cargo test -p hermit --test thread_self_procfs_determinism -- --include-ignored --test-threads=1 # [verify] all tests in hermit-cli/tests/thread_self_procfs_determinism.rs
cargo test -p hermit --test thread_self_procfs_determinism thread_self_stat_consumers_are_deterministic_under_strict_verify -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/thread_self_procfs_determinism.rs::thread_self_stat_consumers_are_deterministic_under_strict_verify
cargo test -p hermit --test thread_self_procfs_determinism thread_self_fd_keeps_the_opener_identity -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/thread_self_procfs_determinism.rs::thread_self_fd_keeps_the_opener_identity
cargo test -p hermit --test thread_sync_determinism -- --include-ignored --test-threads=1 # [run] all tests in hermit-cli/tests/thread_sync_determinism.rs
cargo test -p hermit --test thread_sync_determinism thread_sync_patterns_are_deterministic_across_five_runs -- --exact --include-ignored --test-threads=1 # [run] hermit-cli/tests/thread_sync_determinism.rs::thread_sync_patterns_are_deterministic_across_five_runs
cargo test -p hermit --test uevent_seqnum_determinism -- --include-ignored --test-threads=1 # [verify] all tests in hermit-cli/tests/uevent_seqnum_determinism.rs
cargo test -p hermit --test uevent_seqnum_determinism uevent_seqnum_consumers_are_deterministic_under_strict_verify -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/uevent_seqnum_determinism.rs::uevent_seqnum_consumers_are_deterministic_under_strict_verify
cargo test -p hermit --test unix_socket_table_determinism -- --include-ignored --test-threads=1 # [verify] all tests in hermit-cli/tests/unix_socket_table_determinism.rs
cargo test -p hermit --test unix_socket_table_determinism unix_socket_table_consumers_verify -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/unix_socket_table_determinism.rs::unix_socket_table_consumers_verify
cargo test -p hermit --test vmstat_determinism -- --include-ignored --test-threads=1 # [verify] all tests in hermit-cli/tests/vmstat_determinism.rs
cargo test -p hermit --test vmstat_determinism vmstat_consumers_are_deterministic_under_strict_verify -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/vmstat_determinism.rs::vmstat_consumers_are_deterministic_under_strict_verify
cargo test -p hermit --test writev_determinism -- --include-ignored --test-threads=1 # [both/mixed] all tests in hermit-cli/tests/writev_determinism.rs
cargo test -p hermit --test writev_determinism writev_uses_fd_aware_scheduling_and_verifies -- --exact --include-ignored --test-threads=1 # [both/mixed] hermit-cli/tests/writev_determinism.rs::writev_uses_fd_aware_scheduling_and_verifies
cargo test -p hermit --test zero_copy_pipe_fallback -- --include-ignored --test-threads=1 # [verify] all tests in hermit-cli/tests/zero_copy_pipe_fallback.rs
cargo test -p hermit --test zero_copy_pipe_fallback zero_copy_pipe_syscalls_fall_back_only_in_strict_mode -- --exact --include-ignored --test-threads=1 # [verify] hermit-cli/tests/zero_copy_pipe_fallback.rs::zero_copy_pipe_syscalls_fall_back_only_in_strict_mode
cargo test -p hermit --lib --bins -- --test-threads=1 # [both/mixed] ci/dag/hosted.json test/hermit_unit
cargo test -p hermit --test aio_nr_determinism --test arch_status_determinism --test chaos_sched_yield_progress --test chaos_stress_pmu_detection --test clock_determinism --test clock_discipline_determinism --test cpufreq_avg_determinism --test epoll_determinism --test file_nr_determinism --test fp_reduction_determinism --test futex2_refusal --test hashseed_determinism --test inode_nr_determinism --test kernel_keyring --test key_users_determinism --test mmap_determinism --test node_vmstat_determinism --test numa_maps_determinism --test perf_event_refusal --test pidfd_creation --test process_isolation_refusals --test proc_fdinfo_determinism --test proc_locks_determinism --test procfs_determinism --test procfs_positioned_determinism --test pty_nr_determinism --test python_stdlib --test self_sched_determinism --test self_schedstat_determinism --test signal_determinism --test smaps_determinism --test smaps_rollup_determinism --test softnet_stat_determinism --test sockstat_determinism --test swaps_determinism --test thp_stats_determinism --test zero_copy_pipe_fallback -- --test-threads=1 # [both/mixed] ci/dag/hosted.json test/hermit_integration
cargo test -p hermit --test arbitrary_binaries -- --skip record_replay_stable_arbitrary_binaries --test-threads=1 # [run] ci/dag/hosted.json test/arbitrary_binaries
cargo test -p hermit --test cli -- --skip run_kvm_ --skip backend_accepted_in_global_position --skip run_dbi_aggregates_unsupported_syscalls_and_strict_rejects_them --skip run_dbi_strict_returns_with_blocked_stdin_source --skip run_dbi_verifies_pipe_backpressure --skip run_dbi_keeps_diagnostics_out_of_guest_stderr --skip run_dbi_recovers_after_failed_exec --skip run_liteinst_rejects_non_fork_clone --skip run_liteinst_handles_inherited_ignored_sigchld --skip run_liteinst_verifies_forked_guest --skip run_liteinst_verifies_raw_fork_guest --test-threads=1 # [both/mixed] ci/dag/hosted.json test/cli
cargo test -p hermit --test hermit_modes -- --skip default_ --skip chaos_buck_ --skip hello_race_chaos_verify --test-threads=1 # [both/mixed] ci/dag/hosted.json test/hermit_modes
cargo test -p hermit --test app_strict_verify -- --ignored --skip java_ --skip javac_ --test-threads=1 # [verify] ci/dag/hosted.json test/app_strict_verify
cargo test -p hermit --test command_strict_verify -- --ignored --test-threads=1 # [verify] ci/dag/hosted.json test/command_strict_verify
cargo test -p hermit --test epoll_determinism --test rcx_canonicalization -- --ignored --test-threads=1 # [both/mixed] ci/dag/hosted.json test/ignored_syscall_regressions
cargo test -p hermit --test rr_suite rr_scratch_directories_are_fresh_and_cleaned -- --exact # [run] ci/dag/hosted.json test/rr_suite_contract
python3 tests/backend-parity/run_matrix.py --hermit target/release/hermit --backend dbi --require-backend # [both/mixed] ci/dag/hosted.json test/dbi_parity
set -e; HERMIT=target/debug/hermit; ARGS='run --base-env=minimal --no-virtualize-cpuid --max-timeslice=disabled'; REPS=${L4_REPS:-20}; run_probe() { local c="$1"; timeout 30s $HERMIT $ARGS --strict -- $c </dev/null; timeout 30s $HERMIT $ARGS --strict --verify -- $c </dev/null; timeout 30s $HERMIT $ARGS --strict --verify --detlog-heap --detlog-stack -- $c </dev/null; local i; for ((i=0;i<REPS;i++)); do timeout 30s $HERMIT $ARGS --strict --verify -- $c </dev/null; done; }; run_probe '/bin/true'; run_probe '/bin/echo hermit-envelope'; run_probe '/bin/date -u +%Y' # [verify] ci/dag/hosted.json test/envelope_levels
./validate.sh --hosted-strict-compat-only --no-label-pr --verbose # [verify] ci/dag/hosted.json test/strict_compat
cargo test -p hermit --test cli run_kvm_ -- --test-threads=1 # [both/mixed] ci/dag/hardware.json kvm/cli
cargo test -p hermit --test cli backend_accepted_in_global_position -- --exact --test-threads=1 # [both/mixed] ci/dag/hardware.json kvm/global_position
cargo test -p hermit --test arch_prctl --test compression --test madvise --test ppoll_simulation --test redis_strict --test sqlite_veryquick --test syscall_file_io --test syscall_file_metadata --test syscall_quick_wins --test thread_scheduling_fairness --test writev_determinism -- --test-threads=1 # [both/mixed] ci/dag/hardware.json hw/integration
cargo test -p hermit --test record_replay -- --skip record_replay_matrix --test-threads=1 # [record/replay] ci/dag/hardware.json rr/stable
cargo test -p hermit --test arbitrary_binaries record_replay_stable_arbitrary_binaries -- --exact --test-threads=1 # [record/replay] ci/dag/hardware.json rr/arbitrary
cargo test -p hermit --test random_determinism random_sources_are_deterministic_under_strict_verify -- --exact --ignored --test-threads=1 # [verify] ci/dag/hardware.json random/strict_verify
cargo test -p hermit --test analyze -- --ignored --skip analyze_hello_race --test-threads=1 # [both/mixed] ci/dag/hardware.json analyze/pmu
cargo test -p hermit --test language_runtime_determinism -- --ignored --test-threads=1 # [both/mixed] ci/dag/hardware.json runtime/entropy
cargo test -p hermit --test python_stdlib -- --ignored --test-threads=1 # [both/mixed] ci/dag/hardware.json python/stdlib
cargo test -p hermit --test stress_suite slow_cas_search_and_replay -- --exact --ignored --test-threads=1 # [both/mixed] ci/dag/hardware.json stress/search_replay
./hermit-cli/tests/prepare_leveldb.sh target/hermit-leveldb-ci target/hermit-leveldb-build-ci # [run] ci/dag/hardware.json leveldb/build_fixture
env HERMIT_LEVELDB_BUILD_DIR=target/hermit-leveldb-build-ci cargo test -p hermit --test leveldb focused_leveldb_tests_are_deterministic_under_strict -- --exact --test-threads=1 # [both/mixed] ci/dag/hardware.json leveldb/focused
env HERMIT_LEVELDB_BUILD_DIR=target/hermit-leveldb-build-ci cargo test -p hermit --test leveldb leveldb_env_posix_is_deterministic_under_strict -- --exact --ignored --test-threads=1 # [both/mixed] ci/dag/hardware.json leveldb/env_posix
cargo test -p hermit --test redis_strict -- --ignored --test-threads=1 # [both/mixed] ci/dag/hardware.json redis/extended
test -f third-party/rr/src/test/util.h || { echo 'FAIL: PMU rr syscall suite requires initialized third-party/rr' >&2; exit 1; }; cargo test -p hermit --test rr_suite -- --ignored --skip rr_ppoll --skip rr_rlimit --skip rr_sched_yield_to_lower_priority --test-threads=1 # [record/replay] ci/dag/hardware.json rr/suite
set -e; HERMIT=target/debug/hermit; for c in '/bin/true' '/bin/echo hermit-envelope' '/bin/date -u +%Y'; do timeout ${HERMIT_RR_TIMEOUT:-30s} $HERMIT record start --verify -- $c </dev/null; done # [record/replay] ci/dag/hardware.json rr/envelope
./validate.sh --rr-compat-only --no-label-pr # [record/replay] ci/dag/hardware.json rr/compat_baseline
./tests/debugger/run_debugger_tests.sh # [both/mixed] ci/dag/hardware.json debugger/integration
python3 tests/backend-parity/run_matrix.py --backend ptrace # [both/mixed] ci/dag/hardware.json ptrace/parity
