/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * CONTRACT FIXTURE: file metadata must not leak host state.
 *
 * Pins st_ino, st_dev, st_atime/st_mtime/st_ctime, statx's btime, st_nlink and
 * st_blocks across stat / fstat / lstat / statx, over regular files, directories,
 * symlinks, pipes and /proc entries.
 *
 * RELATIONSHIP TO THE DetInode NEWTYPE: that change makes a host-inode leak a
 * COMPILE error at one boundary. This fixture proves no OTHER path leaks one at
 * RUNTIME. They cover different failure modes -- a type cannot catch a leak that
 * arrives through a syscall return value the type never wraps -- so both are needed.
 * Neither subsumes the other.
 *
 * TWO ASSERTIONS ON TIME, AND THE SECOND IS THE POINT (#140):
 *   (1) IDENTICAL across a double-run. Equality alone, however, is satisfied by a
 *       frozen clock, which is a determinism bug dressed as a pass.
 *   (2) STILL ADVANCING within a single run. The guest stats a file, does work,
 *       touches it, stats again, and prints whether mtime STRICTLY INCREASED. A
 *       frozen or coarsely-rounded clock makes that comparison false and the
 *       fixture's output changes. So the degenerate "fix" -- freeze or round time
 *       until the double-run matches -- is caught by the same fixture that the
 *       equality check would have rewarded.
 *
 * A BRANCH ON AN INODE COMPARISON: the guest compares two inodes and takes a
 * different code path depending on the result, so an inode divergence propagates
 * into the syscall sequence rather than only into a printed number. A printed inode
 * could be normalised away; a taken branch could not.
 *
 * Values are printed RELATIVELY where an absolute would be meaningless. Raw st_dev
 * and raw st_ino ARE printed, deliberately: they are exactly the fields that must
 * not carry host identity, so masking them would defeat the fixture.
 */

#define _GNU_SOURCE
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <linux/stat.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <unistd.h>

#define P(...) do { printf("EV " __VA_ARGS__); putchar('\n'); fflush(stdout); } while (0)

static void dump(const char *tag, const struct stat *s) {
    P("%s ino=%llu dev=%llu nlink=%llu blocks=%lld mode=%o size=%lld",
      tag, (unsigned long long)s->st_ino, (unsigned long long)s->st_dev,
      (unsigned long long)s->st_nlink, (long long)s->st_blocks,
      (unsigned)(s->st_mode & 07777), (long long)s->st_size);
    P("%s atime=%lld mtime=%lld ctime=%lld", tag,
      (long long)s->st_atime, (long long)s->st_mtime, (long long)s->st_ctime);
}

int main(void) {
    const char *reg = "hermit-stat-fixture.tmp";
    const char *lnk = "hermit-stat-fixture.link";
    const char *dir = "hermit-stat-fixture.dir";
    unlink(reg); unlink(lnk); rmdir(dir);

    /* ---- regular file: stat / fstat ---- */
    int fd = open(reg, O_RDWR | O_CREAT | O_TRUNC, 0644);
    if (fd < 0) { perror("open"); return 1; }
    if (write(fd, "hello", 5) != 5) { perror("write"); return 1; }

    struct stat s1;
    if (stat(reg, &s1) != 0) { perror("stat"); return 1; }
    dump("stat_reg", &s1);

    struct stat sf;
    if (fstat(fd, &sf) != 0) { perror("fstat"); return 1; }
    /* fstat and stat name the same file: their inodes MUST agree. Printed as a
     * relation, so it holds regardless of what the (virtualised) value is. */
    P("fstat_matches_stat=%d", (sf.st_ino == s1.st_ino && sf.st_dev == s1.st_dev));

    /* ---- (2) TIME MUST STILL ADVANCE: touch the file, re-stat, compare ---- */
    volatile unsigned long spin = 0;
    for (int i = 0; i < 2 * 1000 * 1000; i++) spin += i;
    if (write(fd, "!", 1) != 1) { perror("write2"); return 1; }
    fsync(fd);
    struct stat s2;
    if (stat(reg, &s2) != 0) { perror("stat2"); return 1; }
    /* STRICT increase would over-constrain a 1-second-granularity st_mtime, so the
     * assertion is non-decreasing PLUS an explicit delta, which distinguishes
     * "advancing" from "frozen" without depending on how fast the guest ran. */
    P("mtime_nondecreasing=%d mtime_delta_sec=%lld",
      (long long)s2.st_mtime >= (long long)s1.st_mtime,
      (long long)s2.st_mtime - (long long)s1.st_mtime);
    /* NANOSECOND advance check. st_mtime has 1-second granularity, so the
     * second-level delta above is 0 on any fast host and CANNOT distinguish
     * "advancing" from "frozen" -- it would be an inert assertion. st_mtim.tv_nsec
     * is the field that actually discriminates, so the anti-freeze check (#140)
     * keys on the full nanosecond timestamp. */
    {
        long long n1 = (long long)s1.st_mtim.tv_sec * 1000000000LL + s1.st_mtim.tv_nsec;
        long long n2 = (long long)s2.st_mtim.tv_sec * 1000000000LL + s2.st_mtim.tv_nsec;
        P("mtime_nsec_advanced=%d", n2 > n1);
    }
    P("size_grew=%d", s2.st_size > s1.st_size);

    /* ---- directory ---- */
    if (mkdir(dir, 0755) != 0) { perror("mkdir"); return 1; }
    struct stat sd;
    if (stat(dir, &sd) != 0) { perror("stat dir"); return 1; }
    dump("stat_dir", &sd);
    P("dir_same_dev_as_file=%d", sd.st_dev == s1.st_dev);

    /* ---- symlink: lstat (the link) vs stat (the target) ---- */
    if (symlink(reg, lnk) != 0) { perror("symlink"); return 1; }
    struct stat sl, st;
    if (lstat(lnk, &sl) != 0) { perror("lstat"); return 1; }
    if (stat(lnk, &st) != 0) { perror("stat link"); return 1; }
    dump("lstat_link", &sl);
    P("lstat_is_symlink=%d", S_ISLNK(sl.st_mode) != 0);
    /* THE INODE BRANCH. lstat sees the link, stat follows to the target, so these
     * inodes must DIFFER. The comparison drives a branch so a divergence changes the
     * syscall sequence, not just a printed number. */
    if (sl.st_ino != st.st_ino) {
        P("branch=A_link_and_target_distinct");
        struct stat again;
        stat(reg, &again);                        /* extra syscall only on branch A */
        P("branch_A_target_ino_matches_file=%d", again.st_ino == s1.st_ino);
    } else {
        P("branch=B_link_and_target_SAME_inode");
        P("branch_B_unexpected_alias=1");
    }

    /* ---- pipe (fstat on a non-file object) ---- */
    int pf[2];
    if (pipe(pf) != 0) { perror("pipe"); return 1; }
    struct stat sp;
    if (fstat(pf[0], &sp) != 0) { perror("fstat pipe"); return 1; }
    P("pipe ino=%llu dev=%llu isfifo=%d",
      (unsigned long long)sp.st_ino, (unsigned long long)sp.st_dev, S_ISFIFO(sp.st_mode) != 0);
    P("pipe_dev_differs_from_file=%d", sp.st_dev != s1.st_dev);

    /* ---- /proc entry ---- */
    struct stat spr;
    if (stat("/proc/self/stat", &spr) == 0) {
        P("proc ino_nonzero=%d dev_nonzero=%d mode=%o",
          spr.st_ino != 0, spr.st_dev != 0, (unsigned)(spr.st_mode & 07777));
    } else {
        P("proc stat_failed=1");
    }

    /* ---- statx, including btime, via raw syscall (glibc may predate statx) ---- */
#if defined(__NR_statx)
    {
        struct statx sx;
        memset(&sx, 0, sizeof sx);
        long r = syscall(__NR_statx, AT_FDCWD, reg, 0, STATX_ALL, &sx);
        if (r == 0) {
            P("statx ino=%llu nlink=%u blocks=%llu mask_btime=%d",
              (unsigned long long)sx.stx_ino, (unsigned)sx.stx_nlink,
              (unsigned long long)sx.stx_blocks, (sx.stx_mask & STATX_BTIME) != 0);
            P("statx mtime=%lld btime=%lld statx_ino_matches_stat=%d",
              (long long)sx.stx_mtime.tv_sec, (long long)sx.stx_btime.tv_sec,
              (unsigned long long)sx.stx_ino == (unsigned long long)s2.st_ino);
        } else {
            P("statx unavailable errno_class=%d", r < 0);
        }
    }
#else
    P("statx not_compiled");
#endif

    close(pf[0]); close(pf[1]); close(fd);
    unlink(lnk); unlink(reg); rmdir(dir);
    P("done");
    return 0;
}
