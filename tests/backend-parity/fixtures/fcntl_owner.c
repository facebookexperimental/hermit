#include <fcntl.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/types.h>
#include <unistd.h>

/*
 * Asynchronous-I/O owner round-trip via fcntl F_SETOWN/F_GETOWN and the signal
 * companions F_SETSIG/F_GETSIG and F_SETOWN_EX/F_GETOWN_EX. This is a distinct
 * fcntl family from the descriptor-flag row (F_SETFD/F_GETFD), the status-flag
 * row (F_SETFL/F_GETFL), the pipe-capacity row (F_SETPIPE_SZ/F_GETPIPE_SZ), and
 * the record-lock row (F_SETLK/F_GETLK): it configures which process receives
 * the SIGIO/SIGURG data-ready notification for a descriptor.
 *
 * No signal is ever delivered: the contract only registers and reads back owner
 * and signal state on a pipe descriptor, so it is pure process-local fcntl
 * bookkeeping with no scheduling or delivery dependence. It asserts only
 * relational round-trips (the value read back equals the value just set), so the
 * golden stdout is portable:
 *   1. F_SETOWN to our own pid succeeds.
 *   2. F_GETOWN reads back our pid.
 *   3. F_SETSIG to SIGUSR1 succeeds.
 *   4. F_GETSIG reads back SIGUSR1.
 *   5. F_SETOWN_EX with F_OWNER_PID succeeds.
 *   6. F_GETOWN_EX reads back F_OWNER_PID and our pid.
 */
int main(void) {
    enum { EXPECTED_CHECKS = 6 };
    int ok = 0;
    int fds[2];
    if (pipe(fds) != 0) {
        printf("fowner ok=0\n");
        return EXIT_FAILURE;
    }
    pid_t me = getpid();

    if (fcntl(fds[0], F_SETOWN, me) == 0) {
        ok++;
    }
    if (fcntl(fds[0], F_GETOWN) == me) {
        ok++;
    }
    if (fcntl(fds[0], F_SETSIG, SIGUSR1) == 0) {
        ok++;
    }
    if (fcntl(fds[0], F_GETSIG) == SIGUSR1) {
        ok++;
    }
    struct f_owner_ex set_ex = {.type = F_OWNER_PID, .pid = me};
    if (fcntl(fds[0], F_SETOWN_EX, &set_ex) == 0) {
        ok++;
    }
    struct f_owner_ex got_ex = {0};
    if (fcntl(fds[0], F_GETOWN_EX, &got_ex) == 0 &&
        got_ex.type == F_OWNER_PID && got_ex.pid == me) {
        ok++;
    }

    close(fds[0]);
    close(fds[1]);
#ifdef HERMIT_TEST_ORACLE_NEGATIVE
    ok--; /* plant one failed contract check to bracket the exit oracle */
#endif
    printf("fowner ok=%d\n", ok);
    return ok == EXPECTED_CHECKS ? EXIT_SUCCESS : EXIT_FAILURE;
}
