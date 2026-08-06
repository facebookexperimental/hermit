// mincore(2) determinization and error-path parity probe.
//
// Real page residency reflects host memory pressure and kernel reclaim
// decisions, so it is nondeterministic. Hermit determinizes mincore by
// injecting the real syscall (to preserve Linux pointer and mapping
// validation) and then reporting every mapped page as resident. The residency
// vector is only an advisory hint, so replacing those bits with a constant is
// bitwise-identical across runs while the argument-validation error paths stay
// faithful to Linux.
//
// Determinized checks (1-2) diverge from native, which reports true residency;
// faithful checks (3-6) match native exactly. Under Hermit all six pass
// (ok=6); native passes only the four faithful checks (ok=4).

#define _GNU_SOURCE
#include <errno.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>

int main(void) {
    long pg = sysconf(_SC_PAGESIZE);
    size_t np = 6;
    size_t len = (size_t)pg * np;
    int ok = 0;

    unsigned char *m =
        mmap(NULL, len, PROT_READ | PROT_WRITE,
             MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (m == MAP_FAILED) {
        printf("mincore MMAP_FAIL\n");
        return 1;
    }

    // (1) Determinized: a fresh, unfaulted mapping reports all pages resident.
    unsigned char v0[6];
    memset(v0, 0xEE, np);
    if (mincore(m, len, v0) == 0) {
        int all = 1;
        for (size_t i = 0; i < np; i++) all &= (v0[i] & 1);
        if (all) ok++;
    }

    // (2) Determinized: after touching only pages 0, 2, 4, mincore still
    //     reports every page resident (native would report 1,0,1,0,1,0).
    m[0 * pg] = 'a';
    m[2 * pg] = 'c';
    m[4 * pg] = 'e';
    unsigned char v1[6];
    memset(v1, 0xEE, np);
    if (mincore(m, len, v1) == 0) {
        int all = 1;
        for (size_t i = 0; i < np; i++) all &= (v1[i] & 1);
        if (all) ok++;
    }

    // (3) Faithful: a zero-length request succeeds with rc 0.
    unsigned char vz[1];
    if (mincore(m, 0, vz) == 0) ok++;

    // (4) Faithful: a NULL residency vector with nonzero length -> EFAULT.
    errno = 0;
    if (mincore(m, len, NULL) == -1 && errno == EFAULT) ok++;

    // (5) Faithful: an unmapped range -> ENOMEM.
    munmap(m, len);
    unsigned char v2[6];
    errno = 0;
    if (mincore(m, len, v2) == -1 && errno == ENOMEM) ok++;

    // (6) Faithful: a non-page-aligned start address -> EINVAL.
    unsigned char *m2 =
        mmap(NULL, (size_t)pg * 2, PROT_READ | PROT_WRITE,
             MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (m2 != MAP_FAILED) {
        unsigned char v3[2];
        errno = 0;
        if (mincore(m2 + 1, (size_t)pg, v3) == -1 && errno == EINVAL) ok++;
        munmap(m2, (size_t)pg * 2);
    }

    printf("mincore ok=%d\n", ok);
    return 0;
}
