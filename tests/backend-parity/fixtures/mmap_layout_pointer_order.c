/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Memory-layout ORDER and SPACING contract -- a pointer-COMPARISON probe.
 *
 * WHY THIS CANNOT BE SOLVED BY NORMALIZING OUTPUT. Absolute addresses may
 * legitimately differ between backends: a rewriting backend loads the guest
 * elsewhere, so every address-valued field shifts and a comparator is right to
 * ordinalize them. But a guest that COMPARES TWO POINTERS observes something a
 * normalizer can never reach. `if (p1 < p2)` yields a bool, the guest branches
 * on it, and the branch changes what the program DOES. Rewriting the printed
 * address does not change the branch; it only hides that the branch differed.
 *
 * So this fixture deliberately prints NO absolute address. It prints only:
 *   - the OUTCOMES of pointer comparisons (a bit string), and
 *   - RELATIVE spacing in pages (differences between addresses),
 * both of which are relocation-invariant and must hold on every backend. A
 * backend that lays memory out differently fails here even if every syscall
 * succeeded and every printed address was normalized to an ordinal.
 *
 * The comparison outcomes are additionally folded into a decision word through
 * REAL CONTROL FLOW (a branch per comparison, mixed with position), so the
 * signal survives even if a future comparator learns to normalize the bit
 * string itself.
 *
 * COVERAGE, one probe per layout decision Detcore must make identically:
 *   A  successive anonymous mmaps  -- allocation direction and relative spacing
 *   B  file-backed mapping         -- placed consistently against the anon runs
 *   C  MAP_FIXED vs kernel-chosen  -- an explicit address is honoured exactly,
 *                                     and a kernel-chosen one is placed
 *                                     consistently relative to it
 *   D  mremap(MREMAP_MAYMOVE)      -- whether the mapping MOVES, and if so which
 *                                     side of the original it lands on
 *   E  brk/heap origin             -- the heap's position relative to mmap space
 *
 * Sizes are deliberately small and there are no long loops: this guest runs
 * under --verify, which executes it twice and diffs logs, so runtime is part of
 * the contract's cost.
 */

#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>

enum { PAGE = 4096, NANON = 4 };

/* Fold a branch OUTCOME (not a value) into the decision word, mixing in the
 * position so the same multiset of outcomes in a different order cannot
 * collide. This is the part a normalizer cannot reach. */
static unsigned long long fold(unsigned long long w, int taken, int pos) {
    if (taken) {
        w = w * 31 + 0x9E37;
    } else {
        w = w * 37 + 0x7C15;
    }
    return w ^ ((unsigned long long)pos * 0x100000001B3ULL);
}

/* Signed page distance between two mappings. Relative, hence relocation-invariant. */
static long page_delta(const void *a, const void *b) {
    return (long)(((const char *)b - (const char *)a) / PAGE);
}

int main(void) {
    unsigned long long word = 1469598103934665603ULL;
    int pos = 0;
    char cmp[64];
    int nc = 0;

    /* --- A: successive anonymous mmaps ------------------------------------ */
    void *a[NANON];
    for (int i = 0; i < NANON; i++) {
        a[i] = mmap(NULL, (size_t)(i + 1) * PAGE, PROT_READ | PROT_WRITE,
                    MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if (a[i] == MAP_FAILED) { perror("mmap anon"); return 2; }
    }
    /* Every ordered pair, so the full ordering is pinned, not just adjacency. */
    for (int i = 0; i < NANON; i++) {
        for (int j = i + 1; j < NANON; j++) {
            int lt = a[i] < a[j];            /* THE BRANCH */
            cmp[nc++] = lt ? '1' : '0';
            word = fold(word, lt, pos++);
        }
    }
    /* Relative spacing of the successive anon mappings. */
    long d01 = page_delta(a[0], a[1]);
    long d12 = page_delta(a[1], a[2]);
    long d23 = page_delta(a[2], a[3]);

    /* --- B: file-backed mapping ------------------------------------------- */
    char tmpl[] = "/tmp/mmap_layout_XXXXXX";
    int fd = mkstemp(tmpl);
    if (fd < 0) { perror("mkstemp"); return 2; }
    if (ftruncate(fd, PAGE) != 0) { perror("ftruncate"); return 2; }
    void *fb = mmap(NULL, PAGE, PROT_READ, MAP_PRIVATE, fd, 0);
    if (fb == MAP_FAILED) { perror("mmap file"); return 2; }
    int fb_below_anon0 = fb < a[0];          /* THE BRANCH */
    cmp[nc++] = fb_below_anon0 ? '1' : '0';
    word = fold(word, fb_below_anon0, pos++);

    /* --- C: MAP_FIXED vs kernel-chosen ------------------------------------ */
    /* Reserve a 2-page window, then re-map its SECOND page MAP_FIXED. A correct
     * implementation honours the exact address, so fixed_ok must be 1 on every
     * backend; the interesting part is where the NEXT kernel-chosen mapping goes
     * relative to it. */
    void *win = mmap(NULL, 2 * PAGE, PROT_READ | PROT_WRITE,
                     MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (win == MAP_FAILED) { perror("mmap window"); return 2; }
    void *want = (char *)win + PAGE;
    void *got = mmap(want, PAGE, PROT_READ | PROT_WRITE,
                     MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, -1, 0);
    if (got == MAP_FAILED) { perror("mmap fixed"); return 2; }
    int fixed_ok = (got == want);            /* THE BRANCH */
    cmp[nc++] = fixed_ok ? '1' : '0';
    word = fold(word, fixed_ok, pos++);

    void *chosen = mmap(NULL, PAGE, PROT_READ | PROT_WRITE,
                        MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (chosen == MAP_FAILED) { perror("mmap chosen"); return 2; }
    int chosen_below_fixed = chosen < got;   /* THE BRANCH */
    cmp[nc++] = chosen_below_fixed ? '1' : '0';
    word = fold(word, chosen_below_fixed, pos++);

    /* --- D: mremap -------------------------------------------------------- */
    void *grow = mmap(NULL, PAGE, PROT_READ | PROT_WRITE,
                      MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (grow == MAP_FAILED) { perror("mmap grow"); return 2; }
    void *moved = mremap(grow, PAGE, 8 * PAGE, MREMAP_MAYMOVE);
    if (moved == MAP_FAILED) { perror("mremap"); return 2; }
    int did_move = (moved != grow);          /* THE BRANCH */
    int moved_up = (moved > grow);           /* THE BRANCH: which side it landed */
    cmp[nc++] = did_move ? '1' : '0';
    word = fold(word, did_move, pos++);
    cmp[nc++] = moved_up ? '1' : '0';
    word = fold(word, moved_up, pos++);

    /* --- E: brk / heap origin --------------------------------------------- */
    void *brk0 = sbrk(0);
    if (brk0 == (void *)-1) { perror("sbrk"); return 2; }
    int heap_below_anon = brk0 < a[0];       /* THE BRANCH */
    cmp[nc++] = heap_below_anon ? '1' : '0';
    word = fold(word, heap_below_anon, pos++);

    cmp[nc] = '\0';
    /* Deliberately no absolute addresses: only comparison outcomes, relative
     * page spacing, and the control-flow-derived decision word. */
    printf("cmp=%s d01=%ld d12=%ld d23=%ld word=%016llx\n",
           cmp, d01, d12, d23, word);

    close(fd);
    unlink(tmpl);
    return 0;
}
