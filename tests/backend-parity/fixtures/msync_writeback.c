// Cross-backend parity contract: msync(2) write-through visibility on a
// MAP_SHARED file mapping.
//
// The existing file_metadata row maps a file MAP_SHARED, writes into the
// mapping, and checks only that msync(MS_SYNC) returns 0. This contract goes
// further: after msync it re-reads the file through pread(2) and requires the
// bytes written into the mapping to be visible in the file, i.e. that MS_SYNC
// actually flushed the shared mapping back to the backing file. It repeats the
// write/msync/pread cycle with a second pattern and checks the misaligned-address
// error path. This directly exercises whether a backend's file-mapping model
// performs real MAP_SHARED write-back (KVM keeps an in-memory mapping model).
//
// Everything is a property of the guest's own writes: the patterns are fixed
// literals and pread reads them straight back, so no host state enters any
// check. There is no data transfer to another endpoint and no blocking wait,
// and the temp file is unlinked before printing for --verify idempotency.
#define _GNU_SOURCE
#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>

/* Number of behavioural checks this fixture must complete; a lower count is a
   failure, not a smaller success. */
#define EXPECTED_CHECKS 5

int main(void) {
    int ok = 0;
    char tmpl[] = "/tmp/msync_writeback_XXXXXX";
    int fd = mkstemp(tmpl);
    if (fd < 0) {
        printf("msync SETUP_FAIL [mkstemp]\n");
        return 1;
    }
    if (ftruncate(fd, 4096) != 0) {
        unlink(tmpl);
        close(fd);
        printf("msync SETUP_FAIL [ftruncate]\n");
        return 1;
    }

    void *map = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if (map == MAP_FAILED) {
        unlink(tmpl);
        close(fd);
        printf("msync SETUP_FAIL [mmap]\n");
        return 1;
    }

    char buf[8];

    // (1)-(2) write first pattern, MS_SYNC, and read it back through the fd.
    memcpy(map, "SYNCDAT1", 8);
    if (msync(map, 4096, MS_SYNC) == 0) ok++;
    memset(buf, 0, sizeof(buf));
    if (pread(fd, buf, 8, 0) == 8 && memcmp(buf, "SYNCDAT1", 8) == 0) ok++;

    // (3)-(4) overwrite with a second pattern, MS_SYNC, and re-read.
    memcpy(map, "SYNCDAT2", 8);
    if (msync(map, 4096, MS_SYNC) == 0) ok++;
    memset(buf, 0, sizeof(buf));
    if (pread(fd, buf, 8, 0) == 8 && memcmp(buf, "SYNCDAT2", 8) == 0) ok++;

    // (5) msync on a misaligned address fails deterministically with EINVAL.
    errno = 0;
    if (msync((char *)map + 1, 4096, MS_SYNC) == -1 && errno == EINVAL) ok++;

    munmap(map, 4096);
    unlink(tmpl);
    close(fd);
    printf("msync ok=%d\n", ok);

    /* Route a behavioural failure into the exit status. Without this the guest
       exits 0 whatever `ok` reached, so a writeback that stopped being visible
       through the fd only lowered the printed number -- and under --verify both
       runs lower it identically, so the comparison still matches and the cell
       stays green. The five checks above are unchanged; this only requires all
       of them. The setup paths above now say SETUP_FAIL rather than ok=0,
       because ok=0 is also what a total behavioural failure prints and the two
       must not be confusable. */
    if (ok != EXPECTED_CHECKS) {
        fprintf(stderr, "msync completed %d of %d checks\n", ok,
                EXPECTED_CHECKS);
        return 1;
    }
    return 0;
}
