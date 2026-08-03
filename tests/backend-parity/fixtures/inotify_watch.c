/*
 * inotify_watch: cross-backend parity contract for the inotify(2) watch
 * descriptor lifecycle.
 *
 * The contract sets up and tears down an inotify instance without ever reading
 * an event: inotify_init1, inotify_add_watch on /tmp, inotify_rm_watch, and
 * close. Reading an event would block until a filesystem change occurs, which
 * is a host-timing channel this matrix keeps out of the non-gated lane, so only
 * the descriptor lifecycle is asserted. The instance is created non-blocking
 * and close-on-exec.
 *
 * ptrace and DBI complete all four steps. KVM's ElfExecutor personality does
 * not implement the inotify family, so it is a documented gap.
 */

#include <stdio.h>
#include <sys/inotify.h>
#include <unistd.h>

int main(void) {
    int ok = 0;

    int fd = inotify_init1(IN_NONBLOCK | IN_CLOEXEC);
    if (fd >= 0) {
        ok++;
    }

    int wd = inotify_add_watch(fd, "/tmp", IN_CREATE | IN_DELETE);
    if (wd >= 0) {
        ok++;
    }

    if (inotify_rm_watch(fd, wd) == 0) {
        ok++;
    }

    if (fd >= 0 && close(fd) == 0) {
        ok++;
    }

    printf("ino ok=%d\n", ok);
    return 0;
}
