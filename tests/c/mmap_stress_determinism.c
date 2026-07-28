/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// mmap-heavy determinism guest. Exercises many anonymous maps of varied
// sizes with deterministic page-touch, mprotect toggling, mremap grow/shrink,
// a MAP_SHARED anonymous region with msync, and a final FNV-1a checksum of all
// touched bytes. Under Hermit the address layout and byte content must be
// identical across runs, so the printed checksum is a determinism witness.
// Prints "mmap_stress success" as the final line on success.
#include <stdio.h>
#include <stdint.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>

static uint64_t fnv1a(const uint8_t *p, size_t n, uint64_t h) {
    for (size_t i = 0; i < n; i++) {
        h ^= p[i];
        h *= 1099511628211ULL;
    }
    return h;
}

int main(void) {
    long pg = sysconf(_SC_PAGESIZE);
    uint64_t h = 1469598103934665603ULL;
    size_t total_pages = 0;

    // A: 64 anonymous maps of growing size; touch every page deterministically.
    for (int i = 1; i <= 64; i++) {
        size_t len = (size_t)i * (size_t)pg;
        uint8_t *m = mmap(NULL, len, PROT_READ | PROT_WRITE,
                          MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if (m == MAP_FAILED) {
            perror("mmap");
            return 1;
        }
        for (size_t off = 0; off < len; off += pg) {
            m[off] = (uint8_t)((i * 31 + off) & 0xff);
            m[off + pg - 1] = (uint8_t)((i * 17 + off) & 0xff);
            total_pages++;
        }
        if (mprotect(m, len, PROT_READ) != 0) {
            perror("mprotect-ro");
            return 1;
        }
        h = fnv1a(m, len, h);
        if (mprotect(m, len, PROT_READ | PROT_WRITE) != 0) {
            perror("mprotect-rw");
            return 1;
        }
        munmap(m, len);
    }

    // B: mremap grow then shrink, preserving content.
    size_t base = 4 * (size_t)pg;
    uint8_t *r = mmap(NULL, base, PROT_READ | PROT_WRITE,
                      MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (r == MAP_FAILED) {
        perror("mmap-r");
        return 1;
    }
    memset(r, 0xab, base);
    uint8_t *r2 = mremap(r, base, base * 4, MREMAP_MAYMOVE);
    if (r2 == MAP_FAILED) {
        perror("mremap-grow");
        return 1;
    }
    memset(r2 + base, 0xcd, base * 3);
    h = fnv1a(r2, base * 4, h);
    uint8_t *r3 = mremap(r2, base * 4, base, MREMAP_MAYMOVE);
    if (r3 == MAP_FAILED) {
        perror("mremap-shrink");
        return 1;
    }
    h = fnv1a(r3, base, h);
    munmap(r3, base);

    // C: MAP_SHARED anonymous region + msync.
    size_t slen = 16 * (size_t)pg;
    uint8_t *s = mmap(NULL, slen, PROT_READ | PROT_WRITE,
                      MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    if (s == MAP_FAILED) {
        perror("mmap-shared");
        return 1;
    }
    for (size_t off = 0; off < slen; off++) {
        s[off] = (uint8_t)(off * 7 + 3);
    }
    if (msync(s, slen, MS_SYNC) != 0) {
        perror("msync");
        return 1;
    }
    h = fnv1a(s, slen, h);
    munmap(s, slen);

    printf("total_pages=%zu\n", total_pages);
    printf("checksum=%016llx\n", (unsigned long long)h);
    printf("mmap_stress success\n");
    return 0;
}
