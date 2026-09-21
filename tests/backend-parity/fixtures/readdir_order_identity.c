/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Directory-enumeration ORDER identity probe -- a CREATION-ORDER-INDEPENDENCE
 * oracle, deliberately not a double-run oracle.
 *
 * WHY NOT JUST COMPARE TWO RUNS. Enumeration order is host-filesystem state.
 * A double-run comparison (--verify, and L3 --detlog-heap/--detlog-stack) runs
 * both executions against the SAME on-disk directory layout, so a host-order
 * leak reproduces identically in both and the comparison is GREEN. That is a
 * structural blindness, not a tuning problem: no number of repeats can see it.
 * Detecting this class needs the host's ordering to VARY while the guest-visible
 * input stays fixed.
 *
 * So this fixture builds the SAME NAME SET TWICE, in two different CREATION
 * ORDERS, and compares the two enumerations WITHIN A SINGLE RUN. If enumeration
 * is determinized the two sequences must be identical, because the name set is
 * identical and creation order is not guest-visible input. If enumeration
 * inherits host/filesystem order they diverge -- and they diverge inside one
 * run, where no double-run oracle is needed.
 *
 * THE GUEST BRANCHES ON ORDER, IT DOES NOT PRINT IT. Printing a sequence can be
 * normalized, stripped, or sorted away by a comparator; control flow cannot. The
 * order is consumed by an adjacency walk that takes a different branch per
 * comparison and folds the branch taken into a decision word. Two different
 * orders produce different decision words even if every name is present in both.
 *
 * TWO SIZES, ON PURPOSE:
 *   SMALL - fits in one getdents64 result buffer.
 *   LARGE - deliberately exceeds glibc's 32 KiB readdir buffer (~1000 short
 *           names), so the stream spans MULTIPLE buffers.
 * The split matters because a per-BUFFER sort determinizes SMALL while leaving
 * LARGE host-ordered, and only the LARGE probe can tell those two apart. Both
 * results are printed so the contract records which sizes hold.
 *
 * Mixed entry types (regular files, subdirectories, symlinks) are included so
 * the contract covers d_type variation and not just names.
 *
 * MEASURED AT THE TIME OF WRITING (hermit ptrace, --strict, on a shared dev host):
 *   native   small order_identical=0 seams_rev=23   large order_identical=0 seams_rev=1999
 *   hermit   small order_identical=1 seams_rev=0    large order_identical=0 seams_rev=1
 * SMALL is fully determinized. LARGE is NOT, and seams_rev=1 is the precise
 * signature of a per-BUFFER sort: 2000 reverse-created names come back as TWO
 * sorted runs joined at ONE seam -- the 32 KiB getdents64 buffer boundary. The
 * stream is 99.95% sorted and still not sorted.
 *
 * THE LARGE LINE PINS A KNOWN GAP, NOT A DESIRED STATE. It is recorded rather
 * than hidden so the gap cannot be lost, and so that FIXING the sort (drain +
 * sort the whole stream at first getdents64, cache per open-file-description,
 * synthesize monotonic d_off) turns this test RED and forces a deliberate
 * update to order_identical=1 seams_rev=0. Do NOT resolve a future red here by
 * relaxing the assertion.
 */

#define _GNU_SOURCE

#include <dirent.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

/* NAME_CAP matches dirent d_name (256) so the copy cannot truncate; a truncating
 * copy would silently merge distinct names and weaken the comparison. */
enum { SMALL_N = 24, LARGE_N = 2000, NAME_CAP = 256, PATH_CAP = 512 };

/* Create entry i under dir, cycling through regular file / directory / symlink
 * so the enumeration carries mixed d_type values. */
static int make_entry(const char *dir, int i) {
    char path[PATH_CAP];
    snprintf(path, sizeof path, "%s/e%06d", dir, i);
    switch (i % 3) {
        case 0: {
            int fd = open(path, O_CREAT | O_WRONLY | O_TRUNC, 0644);
            if (fd < 0) return -1;
            close(fd);
            return 0;
        }
        case 1:
            return mkdir(path, 0755);
        default:
            return symlink("target-does-not-need-to-exist", path);
    }
}

/* Fold the ENUMERATION ORDER into a decision word by BRANCHING on each adjacent
 * pair, rather than hashing the concatenated names. The branch taken -- not the
 * names -- drives the accumulator, so two permutations of the same name set
 * yield different words. */
static unsigned long long order_decision(char names[][NAME_CAP], int n) {
    unsigned long long word = 1469598103934665603ULL;
    for (int i = 1; i < n; i++) {
        int c = strcmp(names[i - 1], names[i]);
        if (c < 0) {
            word = word * 31 + 1;          /* ascending step */
        } else if (c > 0) {
            word = word * 37 + 2;          /* DESCENDING step: an out-of-order seam */
        } else {
            word = word * 41 + 3;          /* duplicate: should never happen */
        }
        /* Position-sensitive: the same multiset of steps in a different place
         * must not collide. */
        word ^= (unsigned long long)i * 0x9E3779B97F4A7C15ULL;
    }
    return word;
}

/* Enumerate dir into names[]; returns count, or -1. Skips "." and "..". */
static int enumerate(const char *dir, char names[][NAME_CAP], int cap) {
    DIR *d = opendir(dir);
    if (!d) return -1;
    int n = 0;
    struct dirent *e;
    while ((e = readdir(d)) != NULL) {
        if (strcmp(e->d_name, ".") == 0 || strcmp(e->d_name, "..") == 0) continue;
        if (n >= cap) { closedir(d); return -1; }
        snprintf(names[n], NAME_CAP, "%s", e->d_name);
        n++;
    }
    closedir(d);
    return n;
}

/* Build two dirs holding the SAME name set in two different creation orders,
 * enumerate both, and report whether the two orders agree. */
/* Returns 1 when the two creation orders enumerated identically, 0 when they
 * diverged, and -1 on a setup error. */
static int probe(const char *root, const char *label, int count,
                 char names_a[][NAME_CAP], char names_b[][NAME_CAP]) {
    char da[PATH_CAP], db[PATH_CAP];
    snprintf(da, sizeof da, "%s/%s_fwd", root, label);
    snprintf(db, sizeof db, "%s/%s_rev", root, label);
    if (mkdir(da, 0755) != 0 || mkdir(db, 0755) != 0) return -1;

    /* forward insertion */
    for (int i = 0; i < count; i++) {
        if (make_entry(da, i) != 0) return -1;
    }
    /* reverse insertion -- same names, opposite creation order */
    for (int i = count - 1; i >= 0; i--) {
        if (make_entry(db, i) != 0) return -1;
    }

    int na = enumerate(da, names_a, count);
    int nb = enumerate(db, names_b, count);
    if (na != count || nb != count) return -1;

    unsigned long long wa = order_decision(names_a, na);
    unsigned long long wb = order_decision(names_b, nb);

    /* Where the two enumerations first disagree; -1 when identical. */
    int first_diff = -1;
    for (int i = 0; i < count; i++) {
        if (strcmp(names_a[i], names_b[i]) != 0) { first_diff = i; break; }
    }

    /* Is each enumeration globally sorted? A per-buffer sort is locally sorted
     * with seams, so this distinguishes "sorted stream" from "sorted buffers". */
    int seams_a = 0, seams_b = 0;
    for (int i = 1; i < na; i++) {
        if (strcmp(names_a[i - 1], names_a[i]) > 0) seams_a++;
        if (strcmp(names_b[i - 1], names_b[i]) > 0) seams_b++;
    }

    int identical = (first_diff == -1) && (wa == wb);
    printf("%s n=%d order_identical=%d first_diff=%d seams_fwd=%d seams_rev=%d words_equal=%d\n",
           label, count, first_diff == -1 ? 1 : 0, first_diff, seams_a, seams_b,
           wa == wb ? 1 : 0);
    /* 1 = the two creation orders enumerated identically, 0 = they did not.
     * Returned rather than only printed: a verdict a caller cannot branch on is
     * a verdict no test harness can fail on. */
    return identical ? 1 : 0;
}

/*
 * EXIT STATUS IS THE CONTRACT, and it is opt-in.
 *
 * With no arguments this only REPORTS, which is what the native premise needs:
 * native creation-order leakage is expected (small order_identical=0), so an
 * unconditional assertion would make the documented baseline "fail".
 *
 * With `--require-small-determinized` the SMALL probe becomes fatal. Small fits
 * in one getdents64 buffer and IS determinized today, so that is a contract we
 * hold and can regress. LARGE stays reported-only because it pins a KNOWN GAP
 * (per-buffer sort, seams_rev=1); making it fatal would land a red test.
 *
 * Why this matters: the harness's `verify` mode grades a cell on the EXIT STATUS
 * of `hermit --strict --verify` and never compares the observation hash against
 * a baseline (ci/test_harness.sh: the `else` branch of run_cell). A fixture that
 * only prints its verdict therefore pins nothing -- and `--verify` is itself
 * structurally blind to this bug class, as the header explains. Returning the
 * verdict through the exit status is what makes the cell able to fail at all.
 */
int main(int argc, char **argv) {
    int require_small = 0;
    for (int i = 1; i < argc; i++) {
        if (strcmp(argv[i], "--require-small-determinized") == 0) require_small = 1;
    }

    char root[] = "/tmp/readdir_order_identity_XXXXXX";
    if (!mkdtemp(root)) { perror("mkdtemp"); return 2; }

    static char small_a[SMALL_N][NAME_CAP], small_b[SMALL_N][NAME_CAP];
    int small_ok = probe(root, "small", SMALL_N, small_a, small_b);
    if (small_ok < 0) {
        fprintf(stderr, "small probe failed\n");
        return 2;
    }

    static char large_a[LARGE_N][NAME_CAP], large_b[LARGE_N][NAME_CAP];
    if (probe(root, "large", LARGE_N, large_a, large_b) < 0) {
        fprintf(stderr, "large probe failed\n");
        return 2;
    }

    if (require_small && !small_ok) {
        fprintf(stderr,
                "CONTRACT VIOLATED: small (single-buffer) enumeration is no longer "
                "creation-order independent; directory order is leaking guest-visible "
                "state. Do not resolve this by relaxing the assertion.\n");
        return 1;
    }
    return 0;
}
