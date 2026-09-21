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
 * ptrace and DBT complete all four steps. KVM's ElfExecutor personality does
 * not implement the inotify family, so it is a documented gap.
 *
 * EACH LIFECYCLE STEP IS REPORTED SEPARATELY and the fixture fails closed.
 * "ino ok=4" summed four steps into one scalar, which is especially lossy for a
 * documented-gap row: a backend missing inotify entirely and a backend that
 * merely failed to remove the watch produced different totals, but any two
 * backends losing the SAME COUNT of different steps compared EQUAL. The
 * existing exit-status guard catches the lower total; naming the steps makes a
 * partial implementation legible as a partial implementation.
 *
 * The watch descriptor number is not printed: it is kernel-allocated state the
 * guest inherits rather than a value it chooses, so it is withheld under the
 * same rule that withholds raw file descriptors elsewhere in this family.
 */

#include <stdio.h>
#include <sys/inotify.h>
#include <unistd.h>

int main(void) {
    enum { EXPECTED_CHECKS = 4 };

    int fd = inotify_init1(IN_NONBLOCK | IN_CLOEXEC);
    int init_ok = fd >= 0;

    int wd = inotify_add_watch(fd, "/tmp", IN_CREATE | IN_DELETE);
    int watch_added = wd >= 0;

    int watch_removed = inotify_rm_watch(fd, wd) == 0;

    int closed = fd >= 0 && close(fd) == 0;

    int ok = init_ok + watch_added + watch_removed + closed;
    printf(
        "ino ok=%d init_ok=%d watch_added=%d watch_removed=%d closed=%d\n",
        ok,
        init_ok,
        watch_added,
        watch_removed,
        closed);
    return ok == EXPECTED_CHECKS ? 0 : 1;
}
