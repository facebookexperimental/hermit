#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

/*
 * Anonymous temporary file lifecycle via open(O_TMPFILE). Distinct from the
 * memfd_create row (an open flag on a directory, not a dedicated syscall) and
 * from the file-mutation row (the file is unnamed and never linked into the
 * directory tree). It asserts only backend-invariant properties, so the golden
 * stdout is portable:
 *   1. open("/tmp", O_TMPFILE|O_RDWR) creates an unnamed regular file.
 *   2. a six-byte write succeeds.
 *   3. fstat reports size 6 and a regular-file type.
 *   4. the link count is zero (the file has no directory entry).
 *   5. pread returns the exact written bytes.
 *   6. ftruncate grows the file to ten bytes with the tail zero-filled.
 *
 * No path is materialized (no /proc, no linkat), so the contract depends only
 * on the anonymous inode and its own fd.
 */
int main(void) {
    enum { EXPECTED_CHECKS = 6 };
    int ok = 0;
    int fd = open("/tmp", O_TMPFILE | O_RDWR, 0600);
    if (fd >= 0) {
        ok++;
    }
    if (fd >= 0 && write(fd, "hello\n", 6) == 6) {
        ok++;
    }
    struct stat st = {0};
    int stat_ok = fd >= 0 && fstat(fd, &st) == 0;
    if (stat_ok && st.st_size == 6 && S_ISREG(st.st_mode)) {
        ok++;
    }
    if (stat_ok && st.st_nlink == 0) {
        ok++;
    }
    char buf[8] = {0};
    ssize_t pread_len = (fd >= 0) ? pread(fd, buf, 6, 0) : -1;
    if (pread_len == 6 && memcmp(buf, "hello\n", 6) == 0) {
        ok++;
    }
    off_t trunc_size = -1;
    if (fd >= 0 && ftruncate(fd, 10) == 0) {
        struct stat ts = {0};
        trunc_size = (fstat(fd, &ts) == 0) ? ts.st_size : -1;
        char tail[10];
        if (pread(fd, tail, 10, 0) == 10 && tail[6] == 0 && tail[7] == 0 &&
            tail[8] == 0 && tail[9] == 0) {
            ok++;
        }
    }
    if (fd >= 0) {
        close(fd);
    }
#ifdef HERMIT_TEST_ORACLE_NEGATIVE
    ok--; /* plant one failed contract check to bracket the exit oracle */
#endif
    /* Emit the OBSERVED VALUES. `ok=%d` is a SUM: two backends failing DIFFERENT
     * checks alias to the same total, and a sum cannot expose a wrong-but-accepted
     * value. Every field below is guest-determined (we wrote 6 bytes, truncated to
     * 10), so the byte stream stays host-independent. */
    printf("otmpfile ok=%d size=%lld nlink=%llu preadlen=%zd truncsize=%lld\n",
           ok, (long long)st.st_size, (unsigned long long)st.st_nlink,
           pread_len, (long long)trunc_size);
    return ok == EXPECTED_CHECKS ? EXIT_SUCCESS : EXIT_FAILURE;
}
