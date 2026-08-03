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
#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>

int main(void) {
    int ok = 0;
    char tmpl[] = "/tmp/msync_writeback_XXXXXX";
    int fd = mkstemp(tmpl);
    if (fd < 0) {
        printf("msync ok=0 [mkstemp fail]\n");
        return 0;
    }
    if (ftruncate(fd, 4096) != 0) {
        unlink(tmpl);
        close(fd);
        printf("msync ok=0 [ftruncate fail]\n");
        return 0;
    }

    void *map = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if (map == MAP_FAILED) {
        unlink(tmpl);
        close(fd);
        printf("msync ok=0 [mmap fail]\n");
        return 0;
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
    return 0;
}
