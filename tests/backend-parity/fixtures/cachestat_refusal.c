/*
 * cachestat_refusal: cross-backend all-refuse contract for cachestat(2).
 *
 * cachestat(2) (Linux 6.5+) reports page-cache residency for a file
 * descriptor: how many pages are cached, dirty, in writeback, evicted, and
 * recently evicted. Those counters are pure host page-cache state — they depend
 * on what the host kernel happens to have cached at that instant and vary run to
 * run and host to host. Exposing them to a guest would be an uncontrolled
 * nondeterminism channel, so Hermit refuses the syscall with a deterministic
 * ENOSYS on every backend, exactly as it does for io_uring, listmount,
 * copy_file_range, and kernel AIO.
 *
 * This is a determinization *choice*, not a host limitation: native Linux on
 * the probe host executes cachestat successfully (rc == 0) and populates the
 * struct. All three Hermit backends instead return -1/ENOSYS and leave the
 * caller's struct untouched, so a guest cannot read even a stale cache count.
 *
 * The fixture probes two descriptor kinds (a directory and a freshly created
 * regular file) and, for each, requires both the ENOSYS refusal and that the
 * sentinel-filled result struct is left completely unmodified. golden ok=2.
 * Native returns ok=0 (both calls succeed and overwrite the sentinel).
 *
 * Uses raw syscall(); the harness supplies -D_GNU_SOURCE.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>

#ifndef SYS_cachestat
#define SYS_cachestat 451
#endif

struct cachestat_range_k {
    uint64_t off;
    uint64_t len;
};

struct cachestat_k {
    uint64_t nr_cache;
    uint64_t nr_dirty;
    uint64_t nr_writeback;
    uint64_t nr_evicted;
    uint64_t nr_recently_evicted;
};

/*
 * Require cachestat(fd) to fail with ENOSYS and to leave the result struct
 * exactly as the caller pre-filled it (a refusal must copy no host state).
 */
static int refused_and_untouched(int fd) {
    const uint64_t sentinel = 0xEEEEEEEEEEEEEEEEULL;
    struct cachestat_range_k range = {0, 0};
    struct cachestat_k stat;
    memset(&stat, 0xEE, sizeof(stat));

    errno = 0;
    long rc = syscall(SYS_cachestat, (unsigned)fd, &range, &stat, 0u);
    if (!(rc == -1 && errno == ENOSYS)) {
        return 0;
    }
    if (stat.nr_cache != sentinel || stat.nr_dirty != sentinel ||
        stat.nr_writeback != sentinel || stat.nr_evicted != sentinel ||
        stat.nr_recently_evicted != sentinel) {
        return 0;
    }
    return 1;
}

int main(void) {
    int ok = 0;

    int dir_fd = open("/tmp", O_RDONLY);
    if (dir_fd >= 0) {
        if (refused_and_untouched(dir_fd)) {
            ok += 1;
        }
        close(dir_fd);
    }

    char path[] = "/tmp/cachestat_refusal.XXXXXX";
    int file_fd = mkstemp(path);
    if (file_fd >= 0) {
        if (write(file_fd, "hello\n", 6) == 6 && refused_and_untouched(file_fd)) {
            ok += 1;
        }
        close(file_fd);
        unlink(path);
    }

    printf("cachestat ok=%d\n", ok);
    return 0;
}
