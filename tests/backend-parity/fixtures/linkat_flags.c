/* Backend-parity fixture: linkat/unlinkat AT-flag semantics.
 *
 * Covers the *at variants and their flags, distinct from the file-metadata
 * row's basic hard/symlink checks:
 *   1. linkat(..., 0) creates a plain hard link; nlink becomes 2.
 *   2. unlinkat(..., 0) removes the link; nlink returns to 1.
 *   3. unlinkat(..., AT_REMOVEDIR) removes an empty directory.
 *   4. unlinkat on a missing name fails with ENOENT.
 *   5. linkat(..., AT_SYMLINK_FOLLOW) hard-links the symlink *target*, so the
 *      new name is a regular file rather than a symlink.
 *
 * All values are self-created and content-derived, so output is "linkat ok=5"
 * on every run regardless of host. All three backends pass (triple parity).
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/stat.h>
#include <unistd.h>

int main(void) {
    char root[] = "/tmp/linkatXXXXXX";
    if (!mkdtemp(root)) {
        perror("mkdtemp");
        return 1;
    }
    char a[128], b[128], c[128], s[128], d[128];
    snprintf(a, sizeof a, "%s/a", root);
    snprintf(b, sizeof b, "%s/b", root);
    snprintf(c, sizeof c, "%s/c", root);
    snprintf(s, sizeof s, "%s/s", root);
    snprintf(d, sizeof d, "%s/d", root);

    int ok = 0;
    struct stat st;

    int fd = open(a, O_CREAT | O_WRONLY, 0644);
    if (fd >= 0) {
        (void)!write(fd, "hi", 2);
        close(fd);
    }

    if (linkat(AT_FDCWD, a, AT_FDCWD, b, 0) == 0 && lstat(b, &st) == 0 &&
        S_ISREG(st.st_mode) && st.st_nlink == 2) {
        ok++;
    }
    if (unlinkat(AT_FDCWD, b, 0) == 0 && lstat(a, &st) == 0 &&
        st.st_nlink == 1) {
        ok++;
    }
    if (mkdir(d, 0755) == 0 && unlinkat(AT_FDCWD, d, AT_REMOVEDIR) == 0 &&
        lstat(d, &st) == -1 && errno == ENOENT) {
        ok++;
    }
    errno = 0;
    if (unlinkat(AT_FDCWD, b, 0) == -1 && errno == ENOENT) {
        ok++;
    }
    if (symlink(a, s) == 0 &&
        linkat(AT_FDCWD, s, AT_FDCWD, c, AT_SYMLINK_FOLLOW) == 0 &&
        lstat(c, &st) == 0 && S_ISREG(st.st_mode)) {
        ok++;
    }

    printf("linkat ok=%d\n", ok);

    unlink(a);
    unlink(c);
    unlink(s);
    unlink(b);
    rmdir(root);
    return 0;
}
