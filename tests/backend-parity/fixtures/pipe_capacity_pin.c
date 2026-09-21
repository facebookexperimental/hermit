#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>

/*
 * Detcore pins every pipe used by its deterministic scheduler before the guest
 * can observe either descriptor. Keep that environment contract explicit and
 * independent of the host's per-UID pipe-page pressure state.
 */
int main(void) {
  enum { EXPECTED_PIPE_CAPACITY = 8192 };
  int fds[2] = {-1, -1};

  if (pipe(fds) != 0) {
    perror("pipe");
    return EXIT_FAILURE;
  }

  int read_capacity = fcntl(fds[0], F_GETPIPE_SZ);
  int write_capacity = fcntl(fds[1], F_GETPIPE_SZ);
  printf("pipe-capacity-pin read=%d write=%d expected=%d\n", read_capacity,
         write_capacity, EXPECTED_PIPE_CAPACITY);

  close(fds[0]);
  close(fds[1]);

  return read_capacity == EXPECTED_PIPE_CAPACITY &&
                 write_capacity == EXPECTED_PIPE_CAPACITY
             ? EXIT_SUCCESS
             : EXIT_FAILURE;
}
