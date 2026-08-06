// utimensat(2) file-timestamp determinization parity probe.
//
// A file's access and modification times are host-derived state: outside Hermit
// they reflect whatever a program sets (or the real wall clock via UTIME_NOW).
// Hermit's determinize_stat normalizes the timestamp fields that stat/fstat
// report to a single deterministic value, so a guest cannot observe the true
// stored times. utimensat itself is accepted (it must not spuriously fail), but
// the value read back is the determinized constant, not the caller's request.
//
// The checks are epoch-agnostic and relational: they never hard-code Hermit's
// internal time base. They assert that the two timestamp fields collapse to a
// single value and that the value the program requested was overridden. Under
// Hermit all five checks pass (ok=5); native passes only the two acceptance
// checks (ok=2) because it faithfully echoes the requested atime/mtime.

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/stat.h>
#include <unistd.h>
#include <time.h>

int main(void) {
    char dir[] = "/tmp/utimensat_determinism.XXXXXX";
    if (!mkdtemp(dir)) {
        printf("utimensat MKDTEMP_FAIL\n");
        return 1;
    }
    char path[256];
    snprintf(path, sizeof(path), "%s/f", dir);
    int fd = open(path, O_CREAT | O_RDWR, 0600);
    if (fd < 0) {
        printf("utimensat OPEN_FAIL\n");
        return 1;
    }

    int ok = 0;

    // Request two distinct explicit timestamps.
    struct timespec ts[2] = {{1111111111, 0}, {2222222222, 0}};
    // (1) utimensat is accepted (native and all backends).
    if (utimensat(AT_FDCWD, path, ts, 0) == 0) ok++;

    struct stat st;
    fstat(fd, &st);
    long a = st.st_atim.tv_sec;
    long m = st.st_mtim.tv_sec;
    // (2) Determinized: both timestamp fields collapse to one value.
    if (a == m) ok++;
    // (3) Determinized: the requested mtime was overridden (native keeps it).
    if (m != 2222222222L) ok++;

    // Omit atime, request a fresh distinct mtime.
    struct timespec ts2[2] = {{0, UTIME_OMIT}, {3333333333, 0}};
    // (4) utimensat with UTIME_OMIT is accepted.
    if (utimensat(AT_FDCWD, path, ts2, 0) == 0) ok++;

    struct stat st2;
    fstat(fd, &st2);
    long a2 = st2.st_atim.tv_sec;
    long m2 = st2.st_mtim.tv_sec;
    // (5) Determinized: fields still collapse and the new mtime was overridden.
    if (a2 == m2 && m2 != 3333333333L) ok++;

    close(fd);
    unlink(path);
    rmdir(dir);
    printf("utimensat ok=%d\n", ok);
    return 0;
}
