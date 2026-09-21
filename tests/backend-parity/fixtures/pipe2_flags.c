// Backend-parity fixture: pipe2(2) descriptor-flag semantics.
//
// Creates pipes with pipe2(2) under each flag combination and inspects the
// resulting descriptor flags with fcntl(F_GETFD)/fcntl(F_GETFL): O_CLOEXEC maps
// to the descriptor's FD_CLOEXEC bit, O_NONBLOCK maps to the open-file
// description's O_NONBLOCK status flag, and the two are independent. A final
// F_SETFL clears O_NONBLOCK and confirms FD_CLOEXEC is unaffected. These flag
// bits are a deterministic property of the syscall arguments, not of host
// timing, so ptrace, DBT, and KVM must agree. The fixture performs no read or
// write on the pipe: an empty-pipe blocking read is a scheduler-gated operation
// and out of scope for this flag-semantics contract.
//
// _GNU_SOURCE is supplied by the harness compile flags (see run_matrix.py).
#define _GNU_SOURCE
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

// Number of behavioural checks this fixture must complete. A lower count is a
// failure, not a smaller success.
#define EXPECTED_CHECKS 4

// Close both ends of a pair and RESET them, so the array never carries a stale
// descriptor into the next block.
//
// WHY THIS EXISTS. The array used to be declared uninitialised and closed
// unconditionally after every block. Both failure paths that creates are
// undefined and neither is theoretical:
//   - if the FIRST pipe2 fails, close() is handed two indeterminate values;
//   - if a LATER one fails, the array still holds the PREVIOUS pair, which was
//     already closed, so it double-closes.
// Either can close a descriptor this process legitimately holds, and which one
// depends on the descriptor table -- host state, which is the single thing a
// determinism fixture must not depend on. A run that happens to work today is
// exactly what breaks under a different interleaving.
static void close_pair(int fds[2]) {
  for (int end = 0; end < 2; ++end) {
    if (fds[end] >= 0) {
      close(fds[end]);
      fds[end] = -1;
    }
  }
}

int main(void) {
  int ok = 0;
  // Initialised, so a failed pipe2 leaves a value close_pair refuses rather
  // than an indeterminate one. pipe2 does not write fds on failure.
  int fds[2] = {-1, -1};

  // pipe2(0): neither flag set on either end.
  if (pipe2(fds, 0) == 0 && fd_cloexec(fds[0]) == 0 && fd_cloexec(fds[1]) == 0 &&
      fd_nonblock(fds[0]) == 0 && fd_nonblock(fds[1]) == 0) {
    ok++;
  }
  close_pair(fds);

  // O_CLOEXEC: FD_CLOEXEC set on both ends, O_NONBLOCK untouched.
  if (pipe2(fds, O_CLOEXEC) == 0 && fd_cloexec(fds[0]) == 1 &&
      fd_cloexec(fds[1]) == 1 && fd_nonblock(fds[0]) == 0) {
    ok++;
  }
  close_pair(fds);

  // O_NONBLOCK: status flag set on both ends, FD_CLOEXEC untouched.
  if (pipe2(fds, O_NONBLOCK) == 0 && fd_nonblock(fds[0]) == 1 &&
      fd_nonblock(fds[1]) == 1 && fd_cloexec(fds[0]) == 0) {
    ok++;
  }
  close_pair(fds);

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
  close_pair(fds);

  printf("pipe2_flags ok=%d\n", ok);

  // Route a behavioural failure into the exit status. Without this the guest
  // exits 0 whatever `ok` reached, so a flag that stopped being honoured only
  // lowered the printed number -- and under --verify both runs lower it
  // identically, so the comparison still matches and the cell stays green.
  // The four checks above are unchanged; this only requires all of them.
  if (ok != EXPECTED_CHECKS) {
    fprintf(stderr, "pipe2_flags completed %d of %d checks\n", ok,
            EXPECTED_CHECKS);
    return 1;
  }
  return 0;
}
