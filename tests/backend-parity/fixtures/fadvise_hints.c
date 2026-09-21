/*
 * File-advice parity fixture (posix_fadvise / fadvise64).
 *
 * The file-advice analog of the memory_advice (madvise) row: it issues the five
 * standard POSIX_FADV_* hints against a temporary file and requires each to be
 * deterministically accepted. posix_fadvise returns 0 on success or a positive
 * errno directly (it does not set errno). Advice is a pure hint with no
 * observable side effect on file contents, so acceptance must be identical
 * across ptrace, DBT, and KVM. The fixture deliberately avoids the
 * invalid-advice edge case, whose refusal semantics are a backend modeling
 * choice rather than a cross-backend contract.
 */
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

int main(void) {
    char path[] = "/tmp/fadviseXXXXXX";
    int fd = mkstemp(path);
    int ok = 0;

    if (fd < 0) {
        /* Setup failed, so no check ran. Exiting 0 here would report success
           for a run that observed nothing. */
        printf("fadvise ok=-1 mkstemp\n");
        return 1;
    }

    char buf[8192];
    memset(buf, 'A', sizeof buf);
    if (write(fd, buf, sizeof buf) != (ssize_t)sizeof buf) {
        printf("fadvise ok=-1 write\n");
        close(fd);
        unlink(path);
        return 1;
    }

    /* Each hint is reported separately: summing five independent
     * posix_fadvise contracts into one scalar meant two backends rejecting
     * DIFFERENT hints compared equal. The advice values are guest-chosen
     * constants and the returns are booleans, so there is no host-independent
     * value to print and this fixture is de-aliased rather than value-printing. */
    int normal = posix_fadvise(fd, 0, 0, POSIX_FADV_NORMAL) == 0;
    int sequential = posix_fadvise(fd, 0, 0, POSIX_FADV_SEQUENTIAL) == 0;
    int willneed = posix_fadvise(fd, 0, 4096, POSIX_FADV_WILLNEED) == 0;
    int dontneed = posix_fadvise(fd, 0, 4096, POSIX_FADV_DONTNEED) == 0;
    int random_hint = posix_fadvise(fd, 0, 0, POSIX_FADV_RANDOM) == 0;
    ok = normal + sequential + willneed + dontneed + random_hint;

    close(fd);
    unlink(path);
    printf("fadvise ok=%d normal=%d sequential=%d willneed=%d dontneed=%d "
           "random=%d\n",
           ok, normal, sequential, willneed, dontneed, random_hint);
    return ok == 5 ? 0 : 1;
}
