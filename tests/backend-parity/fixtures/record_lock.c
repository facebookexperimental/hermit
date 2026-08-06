/*
 * POSIX advisory record-lock (fcntl F_SETLK / F_GETLK) parity fixture.
 *
 * Exercises the byte-range advisory-locking fcntl command family on a single
 * open file description in one process. This is a distinct fcntl family from
 * the descriptor-flag (F_GETFD/F_SETFD), status-flag (F_GETFL/F_SETFL), and
 * pipe-capacity (F_GETPIPE_SZ/F_SETPIPE_SZ) namespaces, and distinct from
 * whole-file flock(2). Checks (six):
 *   1. ftruncate the temp file to a fixed size.
 *   2. F_SETLK acquires a write lock on bytes [0, 100).
 *   3. F_GETLK querying a disjoint range reports F_UNLCK -- a process never
 *      conflicts with its own locks (POSIX), so the query is deterministic.
 *   4. F_SETLK F_UNLCK releases the write lock.
 *   5. F_SETLK acquires a whole-file read lock (l_len == 0).
 *   6. F_SETLK F_UNLCK releases it.
 *
 * Every observable is process-local lock-table state on a self-created file
 * with no host-derived, timing, or cross-process contention input, so the
 * guest-visible byte stream ("reclock ok=6") is identical across repeated runs.
 * The absolute lock offsets/lengths are never printed, only the pass count.
 *
 * ptrace and DBI implement the whole family. KVM's ElfExecutor implements
 * F_SETLK (acquire/release/read-lock all succeed) but returns a deterministic
 * ENOSYS for F_GETLK, so it reports ok=5 and is recorded as an explicit gap.
 */
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

int main(void) {
    enum { EXPECTED_CHECKS = 6 };
    int ok = 0;
    char path[] = "/tmp/reclockXXXXXX";
    int fd = mkstemp(path);
    if (fd < 0) {
        printf("reclock ok=0\n");
        return EXIT_FAILURE;
    }
    if (ftruncate(fd, 4096) == 0) {
        ok++;
    }

    struct flock wl;
    memset(&wl, 0, sizeof wl);
    wl.l_type = F_WRLCK;
    wl.l_whence = SEEK_SET;
    wl.l_start = 0;
    wl.l_len = 100;
    if (fcntl(fd, F_SETLK, &wl) == 0) {
        ok++;
    }

    struct flock query;
    memset(&query, 0, sizeof query);
    query.l_type = F_WRLCK;
    query.l_whence = SEEK_SET;
    query.l_start = 200;
    query.l_len = 50;
    if (fcntl(fd, F_GETLK, &query) == 0 && query.l_type == F_UNLCK) {
        ok++;
    }

    struct flock unlock;
    memset(&unlock, 0, sizeof unlock);
    unlock.l_type = F_UNLCK;
    unlock.l_whence = SEEK_SET;
    unlock.l_start = 0;
    unlock.l_len = 100;
    if (fcntl(fd, F_SETLK, &unlock) == 0) {
        ok++;
    }

    struct flock rl;
    memset(&rl, 0, sizeof rl);
    rl.l_type = F_RDLCK;
    rl.l_whence = SEEK_SET;
    rl.l_start = 0;
    rl.l_len = 0; /* whole file */
    if (fcntl(fd, F_SETLK, &rl) == 0) {
        ok++;
    }

    struct flock whole_unlock;
    memset(&whole_unlock, 0, sizeof whole_unlock);
    whole_unlock.l_type = F_UNLCK;
    whole_unlock.l_whence = SEEK_SET;
    whole_unlock.l_start = 0;
    whole_unlock.l_len = 0; /* whole file, matching the read lock above */
    if (fcntl(fd, F_SETLK, &whole_unlock) == 0) {
        ok++;
    }

    close(fd);
    unlink(path);
#ifdef HERMIT_TEST_ORACLE_NEGATIVE
    ok--; /* plant one failed contract check to bracket the exit oracle */
#endif
    printf("reclock ok=%d\n", ok);
    return ok == EXPECTED_CHECKS ? EXIT_SUCCESS : EXIT_FAILURE;
}
