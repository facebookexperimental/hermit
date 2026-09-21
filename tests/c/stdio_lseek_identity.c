/*
 * Inherited stdout is a container output stream, even when the outer Hermit
 * process is redirected to a regular log file.  Its physical file offset also
 * includes Hermit's diagnostic writes and must not be guest-visible.  A file
 * that the guest itself installs on fd 1 remains an ordinary seekable file.
 */

#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>

static void report_seek(int report_fd, const char *label, int fd) {
    errno = 0;
    off_t offset = lseek(fd, 0, SEEK_CUR);
    dprintf(report_fd, "%s offset=%lld errno=%d\n", label,
            (long long)offset, errno);
}

int main(int argc, char **argv) {
    if (argc != 3) {
        return 2;
    }

    int report = open(argv[1], O_CREAT | O_TRUNC | O_WRONLY, 0600);
    if (report < 0) {
        return 3;
    }

    report_seek(report, "inherited-stdout", STDOUT_FILENO);
    report_seek(report, "inherited-stderr", STDERR_FILENO);

    int stdout_alias = dup(STDOUT_FILENO);
    int stderr_alias = dup(STDERR_FILENO);
    if (stdout_alias < 0 || stderr_alias < 0) {
        return 4;
    }
    report_seek(report, "stdout-alias", stdout_alias);
    report_seek(report, "stderr-alias", stderr_alias);
    close(stdout_alias);
    close(stderr_alias);

    int file = open(argv[2], O_CREAT | O_TRUNC | O_RDWR, 0600);
    if (file < 0 || dup2(file, STDOUT_FILENO) != STDOUT_FILENO ||
        dup2(file, STDERR_FILENO) != STDERR_FILENO) {
        return 5;
    }
    close(file);
    report_seek(report, "guest-file-stdout", STDOUT_FILENO);
    report_seek(report, "guest-file-stderr", STDERR_FILENO);
    close(report);
    return 0;
}
