// Cross-backend parity contract: faccessat2(2) syscall 439 flag semantics.
//
// faccessat2 is the flag-bearing successor to access/faccessat (syscall 269):
// it adds a real flags argument (AT_EACCESS, AT_SYMLINK_NOFOLLOW) that the
// legacy glibc faccessat wrapper emulates in userspace. This fixture drives the
// raw syscall so every backend must resolve and validate the flags in-kernel.
// The guest creates the file it queries, so the accessibility answer is a pure
// function of the guest's own actions, independent of host user, umask, or real
// filesystem state, and every backend (and native) agrees.
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <unistd.h>

#ifndef SYS_faccessat2
#define SYS_faccessat2 439
#endif
#ifndef AT_EACCESS
#define AT_EACCESS 0x200
#endif

static int faccessat2_call(int dirfd, const char *path, int mode, int flags) {
    return (int)syscall(SYS_faccessat2, dirfd, path, mode, flags);
}

int main(void) {
    int ok = 0;
    char tmpl[] = "/tmp/faccessat2_XXXXXX";
    int fd = mkstemp(tmpl);
    if (fd < 0) {
        printf("faccessat2 ok=0 [mkstemp fail]\n");
        return 0;
    }
    write(fd, "hi\n", 3);

    // (1) F_OK existence check on the freshly created file.
    if (faccessat2_call(AT_FDCWD, tmpl, F_OK, 0) == 0) ok++;

    // (2) R_OK|W_OK on a file the guest owns.
    if (faccessat2_call(AT_FDCWD, tmpl, R_OK | W_OK, 0) == 0) ok++;

    // (3) AT_EACCESS flag is accepted and resolves the effective-id check.
    if (faccessat2_call(AT_FDCWD, tmpl, R_OK, AT_EACCESS) == 0) ok++;

    // (4) a missing path is rejected with ENOENT.
    errno = 0;
    if (faccessat2_call(AT_FDCWD, "/tmp/faccessat2_absent_marker", F_OK, 0) == -1
        && errno == ENOENT) {
        ok++;
    }

    // (5) invalid flag bits are rejected with EINVAL (in-kernel validation).
    errno = 0;
    if (faccessat2_call(AT_FDCWD, tmpl, F_OK, 0xFFFF) == -1 && errno == EINVAL) {
        ok++;
    }

    // (6) dirfd-relative resolution against an open directory.
    int dirfd = open("/tmp", O_RDONLY | O_DIRECTORY);
    const char *base = strrchr(tmpl, '/');
    base = base ? base + 1 : tmpl;
    if (dirfd >= 0 && faccessat2_call(dirfd, base, F_OK, 0) == 0) ok++;
    if (dirfd >= 0) close(dirfd);

    close(fd);
    unlink(tmpl);
    printf("faccessat2 ok=%d\n", ok);
    return 0;
}
