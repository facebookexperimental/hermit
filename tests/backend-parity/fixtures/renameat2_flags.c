/* Backend-parity fixture: renameat2 flag semantics.
 *
 * Extends the rename_ops row with the flag-carrying renameat2(2) syscall:
 *   1. RENAME_NOREPLACE onto an existing destination fails with EEXIST.
 *   2. RENAME_NOREPLACE onto a missing destination succeeds and moves content.
 *   3. RENAME_EXCHANGE atomically swaps two files' contents.
 *   4. renameat2(..., 0) behaves like plain rename.
 *
 * All content is self-created under a private mkdtemp root, so every observed
 * value is content-derived and output is "renameat2 ok=4" on every run. All
 * three backends pass (triple parity).
 *
 * Do not #define _GNU_SOURCE here: the harness compiles with -D_GNU_SOURCE and
 * an in-file define would trip -Werror=... redefinition.
 */
#include <errno.h>
#include <fcntl.h>
#include <linux/fs.h> /* RENAME_NOREPLACE / RENAME_EXCHANGE */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

static int put(const char *path, const char *text) {
    int fd = open(path, O_CREAT | O_WRONLY | O_TRUNC, 0644);
    if (fd < 0) {
        return -1;
    }
    size_t len = strlen(text);
    ssize_t n = write(fd, text, len);
    close(fd);
    return n == (ssize_t)len ? 0 : -1;
}

static int slurp(const char *path, char *buf, size_t cap) {
    int fd = open(path, O_RDONLY);
    if (fd < 0) {
        return -1;
    }
    ssize_t n = read(fd, buf, cap - 1);
    close(fd);
    if (n < 0) {
        return -1;
    }
    buf[n] = 0;
    return 0;
}

int main(void) {
    char root[] = "/tmp/rn2XXXXXX";
    if (!mkdtemp(root)) {
        perror("mkdtemp");
        return 1;
    }
    char a[128], b[128], buf[64];
    snprintf(a, sizeof a, "%s/a", root);
    snprintf(b, sizeof b, "%s/b", root);

    int ok = 0;

    put(a, "AAA");
    put(b, "BBB");
    errno = 0;
    if (renameat2(AT_FDCWD, a, AT_FDCWD, b, RENAME_NOREPLACE) == -1 &&
        errno == EEXIST) {
        ok++;
    }

    unlink(b);
    if (renameat2(AT_FDCWD, a, AT_FDCWD, b, RENAME_NOREPLACE) == 0 &&
        slurp(b, buf, sizeof buf) == 0 && strcmp(buf, "AAA") == 0) {
        ok++;
    }

    put(a, "XXX");
    if (renameat2(AT_FDCWD, a, AT_FDCWD, b, RENAME_EXCHANGE) == 0 &&
        slurp(a, buf, sizeof buf) == 0 && strcmp(buf, "AAA") == 0 &&
        slurp(b, buf, sizeof buf) == 0 && strcmp(buf, "XXX") == 0) {
        ok++;
    }

    if (renameat2(AT_FDCWD, a, AT_FDCWD, b, 0) == 0 &&
        slurp(b, buf, sizeof buf) == 0 && strcmp(buf, "AAA") == 0) {
        ok++;
    }

    printf("renameat2 ok=%d\n", ok);

    unlink(a);
    unlink(b);
    rmdir(root);
    return 0;
}
