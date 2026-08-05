// fchmodat2(2) (syscall 452) flags-argument parity probe.
//
// fchmodat2 is the flags-bearing successor to fchmodat(2): unlike the older
// glibc fchmodat wrapper (which ignores its flags argument), fchmodat2 accepts
// AT_SYMLINK_NOFOLLOW / AT_EMPTY_PATH and validates the flags word directly in
// the kernel. It carries no host-derived state in its result, so a correct
// implementation is deterministic by construction: identical mode changes and
// identical error classifications on every run.
//
// The ptrace and DBI backends forward syscall 452 to the host, so all five
// checks pass (ok=5) exactly as they do natively. The KVM backend's ElfExecutor
// does not route syscall 452 and returns ENOSYS for every fchmodat2 call, so it
// fails all five checks (ok=0); that is a documented KVM gap in matrix.tsv, not
// a determinism relaxation. Native and the two forwarding backends agree, which
// is the faithful-support shape.

#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <unistd.h>

static long fchmodat2(int dfd, const char *p, unsigned mode, unsigned flags) {
    return syscall(452, dfd, p, mode, flags);
}

static unsigned mode_of(const char *p) {
    struct stat st;
    if (stat(p, &st) != 0) return 0xFFFF;
    return st.st_mode & 07777;
}

int main(void) {
    char dir[] = "/tmp/fchmodat2_flags.XXXXXX";
    if (!mkdtemp(dir)) {
        printf("fchmodat2 MKDTEMP_FAIL\n");
        return 1;
    }
    char path[256];
    snprintf(path, sizeof(path), "%s/f", dir);
    int fd = open(path, O_CREAT | O_WRONLY, 0600);
    if (fd >= 0) close(fd);

    int ok = 0;

    // (1) Set mode 0640 with flags 0.
    if (fchmodat2(AT_FDCWD, path, 0640, 0) == 0 && mode_of(path) == 0640) ok++;

    // (2) Replace with 0600 (deterministic, repeatable state change).
    if (fchmodat2(AT_FDCWD, path, 0600, 0) == 0 && mode_of(path) == 0600) ok++;

    // (3) AT_SYMLINK_NOFOLLOW on a regular file succeeds -> 0644.
    if (fchmodat2(AT_FDCWD, path, 0644, AT_SYMLINK_NOFOLLOW) == 0 &&
        mode_of(path) == 0644)
        ok++;

    // (4) Faithful: a missing path yields ENOENT.
    char miss[256];
    snprintf(miss, sizeof(miss), "%s/nope", dir);
    errno = 0;
    if (fchmodat2(AT_FDCWD, miss, 0600, 0) == -1 && errno == ENOENT) ok++;

    // (5) Faithful: an invalid flags word yields EINVAL.
    errno = 0;
    if (fchmodat2(AT_FDCWD, path, 0600, 0xFFFFu) == -1 && errno == EINVAL) ok++;

    unlink(path);
    rmdir(dir);
    printf("fchmodat2 ok=%d\n", ok);
    return 0;
}
