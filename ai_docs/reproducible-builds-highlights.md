# Bit-for-bit reproducible Debian package builds under Hermit

This note summarizes one experiment: building real Debian packages twice, from
two different directories, natively and under Hermit, and comparing the
resulting `.deb` files byte for byte.

## Headline

58 packages from Debian 7 (Wheezy). Built twice natively, each time from a
different root directory, **all 58** produced different `.deb` files. Built
twice under `hermit run --strict --no-rcb-time`, again from two different root
directories, **52 of 58** produced byte-identical `.deb` files and **none**
differed. The other 6 did not finish a build under Hermit (see "Packages with
no result").

| How the two builds ran | Identical | Different | No result |
| --- | --- | --- | --- |
| Natively (the control) | 0 | 58 | 0 |
| `hermit run --strict` | 46 | 6 | 6 |
| `hermit run --strict --no-rcb-time` | **52** | **0** | 6 |

A package counts as reproduced only if its own two native builds differed, so
every package is its own control. All 58 native pairs differed.

## An earlier run on the 8,688-package target

The 58-package run does not replace an earlier, still unfinished run against a
larger target: the 8,688 Wheezy packages that the ASPLOS 2020 study found were
not reproducible natively but were made reproducible by DetTrace, Hermit's
predecessor. Through its third batch of 2026-07-31, that run attempted 46 of
those packages, each with two independent build roots under
`hermit run --strict`. Each package has exactly one outcome:

| Outcome of the two builds | Packages |
| --- | --- |
| Byte-identical `.deb` files | 37 |
| Only `.deb` archive timestamps differed | 5 |
| Shipped bytes differed (`389-adminutil`) | 1 |
| Hermit crashed (`3depict`, `7kaa`) | 2 |
| Skipped by the harness (`a56`) | 1 |
| Attempted | 46 |

The five timestamp-only packages are `a52dec`, `bdfresize`, `bible-kjv`,
`binfmtc`, and `bison++`. In each, every file's contents were identical, and
one or two modification times in the `.deb` file's tar headers differed by 1
second. `389-adminutil` builds three `.deb` files. Two differed in that same
way, by 1 to 2 seconds. The third, `libadminutil-dev`, shipped different
bytes: its static library archives record a modification time for each member,
and those times differed. The package is counted once, in its own row. The two
crashes were traced to hardware performance-counter skid under host load: the
counter had already passed the point where Hermit meant to start
single-stepping, by 230 and 325 retired conditional branches. `a56` was skipped
because its build gives files to user 0, which the unprivileged build cannot
do; it is not a result about Hermit. The 58-package run's zero differences
therefore hold for that sample, not for Hermit in general.

Two Hermit builds were used, both release builds that include the same Reverie
fix, <https://github.com/rrnewton/reverie/pull/287>. `hello` and `hostname`
were built with one made from <https://github.com/rrnewton/hermit/pull/1160>,
and the other 44 packages with one made from commit `f3b29a1f1c3c`, the head of
<https://github.com/rrnewton/hermit/pull/1168>; `hostname` was built again with
the second and gave the same `.deb`. These counts are transcribed from the
experiment's results file, `experiments/debian_reproducible_builds_2026/PILOT_RESULTS.md`
in the team's private dev-hermit workspace repository (section "Cumulative
(through 2026-07-31 batch 3)", last changed in commit `05ed0bc90d83` on
2026-07-31). Not all build outputs were kept. A recount on 2026-08-18 of the
two-root outputs then retained found 23 packages with at least one `.deb` on
each side, 19 of them identical and 4 different, and excluded one more,
`3depict`, which had no `.deb` on either side. Those outputs were kept outside
version control and are no longer in the experiment's working directory, so
neither count can be recomputed from them now.

## Terms

- **Reproducible build**: building the same source twice gives bit-for-bit the
  same output. It lets anyone check that a published binary really came from
  the published source.
- **Root directory**: the host directory that holds the unpacked Debian system
  the build runs in. Each build used a separate copy in a different directory.
- **`--strict`**: Hermit's mode that runs all of the build's threads and
  processes one at a time, in a deterministic order, with virtual time,
  randomness, and process IDs.
- **`--no-rcb-time`**: tells Hermit not to use the CPU's hardware performance
  counters for its virtual clock (explained below).

## Method

- **Packages**: 58 source packages from the Debian 7 (Wheezy) archive as of
  snapshot.debian.org `20190301T000000Z`. They are a convenience sample of
  small, quick builds from the package set studied in the ASPLOS 2020 paper
  *Reproducible Containers* (Navarro Leija et al.,
  <https://doi.org/10.1145/3373376.3378519>).
- **Builds**: for each package, one prepared source tree was copied into six
  root directories. Two were built natively, two under `hermit run --strict`,
  and two under `hermit run --strict --no-rcb-time`: one build per root, six
  builds per package. Each build switched its root directory to the copied
  Debian tree and ran `dpkg-buildpackage -uc -us -b` in `/work/build` inside
  it.
- **Hermit invocation** for the headline arm:

  ```bash
  hermit run --strict --no-rcb-time --max-timeslice=disabled --base-env=minimal --network=local --bind=<root>:/tmp/drb-root -- <build script>
  ```

  `--bind` makes the copied tree visible inside Hermit's container. The
  `--strict` arm used Hermit's default clock, which counts branches with the
  performance counters.
- **Comparison**: one SHA-256 per build, taken over all the `.deb` files the
  build produced, in name order. Two builds are identical when those hashes are
  equal.
- **Setup**: Hermit commit `1fadc03779f2` (release build, `ptrace` backend,
  binary SHA-256 `96a29c8d74548dc9de0faa60e697e85a10e7973e74b445237f5426bbcf5dfe87`),
  on an AMD EPYC 9D85 host running Linux 6.16.1 with glibc 2.34, without
  `CPUID` interception. All 324 builds ran between 2026-08-07 03:16:55 and
  08:18:56 UTC and produced 226 distinct hashes.
- **Cost**: a small package that built natively in about 5 seconds took about
  50 seconds under Hermit.

## Why `--no-rcb-time`

By default Hermit's virtual clock, and its decisions about when to switch
threads, are driven by the number of retired conditional branches the program
has executed, as counted by the CPU's performance counters. On this host the
counters failed Hermit's own reliability check, so the plain `--strict` arm was
driven by an unreliable count. With `--no-rcb-time --max-timeslice=disabled`,
Hermit switches threads only at system calls and advances time without the
counters. That arm reproduced the six packages that had differed under plain
`--strict` (figlet, grep, indent, nano, time, and wdiff) and every other
package that finished.

## What Hermit removes, and what it keeps

The experiment confirmed two behaviours, each in both directions:

- **A path that only disturbs the build is neutralized.** A different build
  location can change memory layout, directory listing order, and timing. In
  28 packages that were checked, none wrote the host root directory into its
  output, so the native differences came from such disturbances and from
  ordinary sources like timestamps; under Hermit they disappeared.
- **A path written into the output is kept.** In a separate arm, the path the
  build sees inside the container was changed as well. `hostname`, `tree`, and
  `zip` then differed under Hermit too (for `hostname` the difference was 19
  bytes, in the ELF build ID), while a control with the same inside path came
  out identical. `groff` and `hdparm` also write the build path into their
  output. Hermit does not hide a real difference in the inputs; it removes
  only the variation the build could not have controlled.

## Packages with no result

- **ack-grep**: the build stopped with
  `make[1]: /work/build/0: Command not found` under Hermit. This is the one
  unexplained failure on Hermit's side.
- **bsdmainutils**: its install step runs `chown root:tty`, which fails with
  `EINVAL` because Hermit's container maps a single group ID. That is how a
  user namespace with one mapped group is supposed to behave.
- **flex, groff, lftp, splint**: the build did not finish within the
  experiment's run.

## Limits

- **Speed.** `--strict` runs a build's threads one at a time. Large parallel
  builds are slow; an `nftables` build ran for more than 23 minutes without
  finishing on a 316-core host
  (<https://github.com/rrnewton/hermit/issues/1798>).
- **Embedded paths and names.** Hermit deliberately keeps differences that a
  build writes into its output, such as its own path or host name.
- **Other ecosystems.** Of 13 nixpkgs packages that could be measured, none
  built reproducibly under Hermit yet.
- **Sample.** The 58 packages were picked because they build quickly; they are
  not a random sample. For scale, the ASPLOS 2020 study built 17,145 Debian
  packages, of which 11,958 were not reproducible on their own and 8,688 were
  made reproducible by DetTrace, Hermit's predecessor. The 52 here are not a
  percentage of either number. The earlier run on the 8,688-package target
  (above) found timestamp differences and one shipped-byte difference that this
  sample did not.
- **Age.** These results are from Hermit `1fadc03779f2` (2026-08-07) and have
  not been re-run since.

## Per-package results

"Native, two roots" compares the two native builds; the other two columns
compare the two builds under each Hermit mode.

| Package | Version | Native, two roots | `--strict` | `--strict --no-rcb-time` |
|---|---|---|---|---|
| ack-grep | 1.96-2 | differ | no result | no result |
| bridge-utils | 1.5-6 | differ | identical | identical |
| bsdiff | 4.3-14 | differ | identical | identical |
| bsdmainutils | 9.0.3 | differ | no result | no result |
| bzip2 | 1.0.6-4 | differ | identical | identical |
| cabextract | 1.4-3 | differ | identical | identical |
| cflow | 1:1.4+dfsg1-2 | differ | identical | identical |
| cgdb | 0.6.6-2 | differ | identical | identical |
| cmatrix | 1.2a-4 | differ | identical | identical |
| cscope | 15.7a-3.6 | differ | identical | identical |
| dialog | 1.1-20120215-2 | differ | identical | identical |
| dos2unix | 6.0-1 | differ | identical | identical |
| ed | 1.6-2 | differ | identical | identical |
| ethtool | 1:3.4.2-1 | differ | identical | identical |
| figlet | 2.2.5-2 | differ | differ | identical |
| file | 5.11-2+deb7u8 | differ | identical | identical |
| flex | 2.5.35-10.1 | differ | no result | no result |
| gperf | 3.0.3-1 | differ | identical | identical |
| grep | 2.12-2 | differ | differ | identical |
| groff | 1.21-9 | differ | no result | no result |
| hdparm | 9.39-1 | differ | identical | identical |
| hostname | 3.11 | differ | identical | identical |
| httping | 1.5.3-1 | differ | identical | identical |
| indent | 2.2.11-2 | differ | differ | identical |
| lftp | 4.3.6-1+deb7u2 | differ | no result | no result |
| ltrace | 0.5.3-2.1 | differ | identical | identical |
| lzop | 1.03-3 | differ | identical | identical |
| moreutils | 0.47 | differ | identical | identical |
| mtools | 4.0.17-1 | differ | identical | identical |
| nano | 2.2.6-1 | differ | differ | identical |
| ncompress | 4.2.4.4-5 | differ | identical | identical |
| netcat-openbsd | 1.105-7 | differ | identical | identical |
| ngrep | 1.45.ds2-12 | differ | identical | identical |
| numactl | 2.0.8~rc4-1 | differ | identical | identical |
| pax | 1:20120606-2 | differ | identical | identical |
| pbzip2 | 1.1.8-1 | differ | identical | identical |
| pmount | 0.9.23-2 | differ | identical | identical |
| psmisc | 22.19-1+deb7u1 | differ | identical | identical |
| pv | 1.2.0-1 | differ | identical | identical |
| sdparm | 1.07-1 | differ | identical | identical |
| sipcalc | 1.1.5-1 | differ | identical | identical |
| splint | 3.1.2.dfsg1-2 | differ | no result | no result |
| strace | 4.5.20-2.3 | differ | identical | identical |
| stress | 1.0.1-1 | differ | identical | identical |
| sysfsutils | 2.1.0+repack-2 | differ | identical | identical |
| sysstat | 10.0.5-1 | differ | identical | identical |
| time | 1.7-24 | differ | differ | identical |
| tofrodos | 1.7.9.debian.1-1 | differ | identical | identical |
| toilet | 0.3-1 | differ | identical | identical |
| tree | 1.6.0-1 | differ | identical | identical |
| tty-clock | 1.1-1 | differ | identical | identical |
| uncrustify | 0.59-2 | differ | identical | identical |
| units | 1.88-1 | differ | identical | identical |
| vlan | 1.9-3 | differ | identical | identical |
| wdiff | 1.1.2-1 | differ | differ | identical |
| whois | 5.1.1~deb7u1 | differ | identical | identical |
| xdelta | 1.1.3-9 | differ | identical | identical |
| zip | 3.0-6 | differ | identical | identical |
