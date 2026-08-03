// Backend-parity fixture: pipe2(2) descriptor-flag semantics.
//
// Creates pipes with pipe2(2) under each flag combination and inspects the
// resulting descriptor flags with fcntl(F_GETFD)/fcntl(F_GETFL): O_CLOEXEC maps
// to the descriptor's FD_CLOEXEC bit, O_NONBLOCK maps to the open-file
// description's O_NONBLOCK status flag, and the two are independent. A final
// F_SETFL clears O_NONBLOCK and confirms FD_CLOEXEC is unaffected. These flag
// bits are a deterministic property of the syscall arguments, not of host
// timing, so ptrace, DBI, and KVM must agree. The fixture performs no read or
// write on the pipe: an empty-pipe blocking read is a scheduler-gated operation
// and out of scope for this flag-semantics contract.
//
// _GNU_SOURCE is supplied by the harness compile flags (see run_matrix.py).
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <unistd.h>

// 1 if FD_CLOEXEC is set on the descriptor, 0 if clear, -1 on error.
static int fd_cloexec(int fd) {
  int flags = fcntl(fd, F_GETFD);
  return flags < 0 ? -1 : !!(flags & FD_CLOEXEC);
}

// 1 if O_NONBLOCK is set on the open file description, 0 if clear, -1 on error.
static int fd_nonblock(int fd) {
  int flags = fcntl(fd, F_GETFL);
  return flags < 0 ? -1 : !!(flags & O_NONBLOCK);
}

int main(void) {
  int ok = 0;
  int fds[2];

  // pipe2(0): neither flag set on either end.
  if (pipe2(fds, 0) == 0 && fd_cloexec(fds[0]) == 0 && fd_cloexec(fds[1]) == 0 &&
      fd_nonblock(fds[0]) == 0 && fd_nonblock(fds[1]) == 0) {
    ok++;
  }
  close(fds[0]);
  close(fds[1]);

  // O_CLOEXEC: FD_CLOEXEC set on both ends, O_NONBLOCK untouched.
  if (pipe2(fds, O_CLOEXEC) == 0 && fd_cloexec(fds[0]) == 1 &&
      fd_cloexec(fds[1]) == 1 && fd_nonblock(fds[0]) == 0) {
    ok++;
  }
  close(fds[0]);
  close(fds[1]);

  // O_NONBLOCK: status flag set on both ends, FD_CLOEXEC untouched.
  if (pipe2(fds, O_NONBLOCK) == 0 && fd_nonblock(fds[0]) == 1 &&
      fd_nonblock(fds[1]) == 1 && fd_cloexec(fds[0]) == 0) {
    ok++;
  }
  close(fds[0]);
  close(fds[1]);

  // O_CLOEXEC|O_NONBLOCK: both set; clearing O_NONBLOCK with F_SETFL leaves
  // FD_CLOEXEC intact because they live on different objects (descriptor vs
  // open file description).
  if (pipe2(fds, O_CLOEXEC | O_NONBLOCK) == 0 && fd_cloexec(fds[0]) == 1 &&
      fd_nonblock(fds[0]) == 1) {
    int flags = fcntl(fds[0], F_GETFL);
    if (flags >= 0 && fcntl(fds[0], F_SETFL, flags & ~O_NONBLOCK) == 0 &&
        fd_nonblock(fds[0]) == 0 && fd_cloexec(fds[0]) == 1) {
      ok++;
    }
  }
  close(fds[0]);
  close(fds[1]);

  printf("pipe2_flags ok=%d\n", ok);
  return 0;
}
