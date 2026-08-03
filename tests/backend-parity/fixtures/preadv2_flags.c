/*
 * preadv2_flags: cross-backend contract for the flagged vectored I/O syscalls
 * preadv2(2) / pwritev2(2) (distinct syscall numbers 327/328 from the classic
 * preadv/pwritev exercised by vectored_file_io).
 *
 * These add a trailing RWF_* flags argument to positioned scatter/gather I/O.
 * The fixture drives:
 *   1. pwritev2(fd, iov, 1, off=0,  flags=0) writes 6 bytes -> rc == 6
 *   2. pwritev2(fd, iov, 1, off=6,  flags=0) writes 6 bytes -> rc == 6
 *   3. preadv2(fd,  iov, 1, off=0,  flags=0) reads back chunk 1 -> rc == 6, match
 *   4. preadv2(fd,  iov, 1, off=6,  flags=0) reads back chunk 2 -> rc == 6, match
 *   5. pwritev2(fd, iov, 1, off=-1, RWF_APPEND) appends at EOF -> rc == 6
 *   6. preadv2(fd,  iov, 1, off=12, flags=0) reads the appended chunk -> match
 *
 * golden ok=6 on native Linux and on the ptrace and DBI backends: positioned
 * vectored I/O with RWF flags targets an offset the caller supplies, so it does
 * not depend on host time, PID, the scheduler, or a shared file position — a
 * faithful, deterministic result on every backend that implements the syscalls.
 *
 * The KVM ElfExecutor personality does not implement preadv2/pwritev2 (it
 * returns ENOSYS, and EOPNOTSUPP for the RWF_APPEND form), exactly as it lacks
 * the classic pwritev/preadv, so KVM is an explicit gap for this row.
 *
 * Uses the glibc preadv2/pwritev2 wrappers (which split the 64-bit offset into
 * the kernel's pos_lo/pos_hi pair correctly); the harness supplies
 * -D_GNU_SOURCE for the wrapper prototypes and RWF_APPEND.
 */
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/uio.h>
#include <unistd.h>

int main(void) {
    int ok = 0;

    char path[] = "/tmp/preadv2_flags.XXXXXX";
    int fd = mkstemp(path);
    if (fd < 0) {
        printf("preadv2 ok=0\n");
        return 0;
    }

    char c0[6] = "AAAAA\n";
    char c1[6] = "BBBBB\n";
    char c2[6] = "CCCCC\n";
    char r[6];

    struct iovec w0 = {c0, sizeof(c0)};
    if (pwritev2(fd, &w0, 1, 0, 0) == (ssize_t)sizeof(c0)) {
        ok += 1;
    }
    struct iovec w1 = {c1, sizeof(c1)};
    if (pwritev2(fd, &w1, 1, (off_t)sizeof(c0), 0) == (ssize_t)sizeof(c1)) {
        ok += 1;
    }

    memset(r, 0, sizeof(r));
    struct iovec r0 = {r, sizeof(r)};
    if (preadv2(fd, &r0, 1, 0, 0) == (ssize_t)sizeof(r) &&
        memcmp(r, c0, sizeof(r)) == 0) {
        ok += 1;
    }
    memset(r, 0, sizeof(r));
    struct iovec r1 = {r, sizeof(r)};
    if (preadv2(fd, &r1, 1, (off_t)sizeof(c0), 0) == (ssize_t)sizeof(r) &&
        memcmp(r, c1, sizeof(r)) == 0) {
        ok += 1;
    }

    /* RWF_APPEND ignores the supplied offset and writes at end-of-file. */
    struct iovec w2 = {c2, sizeof(c2)};
    if (pwritev2(fd, &w2, 1, -1, RWF_APPEND) == (ssize_t)sizeof(c2)) {
        ok += 1;
    }
    memset(r, 0, sizeof(r));
    struct iovec r2 = {r, sizeof(r)};
    if (preadv2(fd, &r2, 1, (off_t)(2 * sizeof(c0)), 0) == (ssize_t)sizeof(r) &&
        memcmp(r, c2, sizeof(r)) == 0) {
        ok += 1;
    }

    close(fd);
    unlink(path);

    printf("preadv2 ok=%d\n", ok);
    return 0;
}
