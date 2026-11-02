// A signal pending while a poll runs ends it with ERESTART_RESTARTBLOCK even
// when the poll has a zero timeout and cannot wait: Linux checks for a signal
// before it honours the timeout. With no handler to run, the kernel restarts
// the call, again with a zero timeout. Under a tracer a default-ignored
// SIGCHLD is still queued, so children that exit while the parent polls
// produce exactly that. The parent polls an empty pipe 2000 times with a zero
// timeout and forks a child that exits at once before every tenth poll; every
// poll returns 0.
#define _GNU_SOURCE
#include <poll.h>
#include <stdio.h>
#include <sys/wait.h>
#include <unistd.h>

#define POLLS 2000

int main(void) {
  int pipe_fds[2];
  if (pipe(pipe_fds) != 0) {
    perror("pipe");
    return 2;
  }
  struct pollfd fds = {pipe_fds[0], POLLIN, 0};
  int ready = 0;
  int failed = 0;
  for (int i = 0; i < POLLS; i++) {
    if (i % 10 == 0 && fork() == 0) {
      _exit(0);
    }
    int r = poll(&fds, 1, 0);
    if (r > 0) {
      ready++;
    } else if (r < 0) {
      failed++;
    }
  }
  int children = 0;
  int status;
  while (wait(&status) > 0) {
    if (WIFEXITED(status) && WEXITSTATUS(status) == 0) {
      children++;
    }
  }
  printf("%d zero-timeout polls: %d ready, %d failed, %d children\n", POLLS,
         ready, failed, children);
  return 0;
}
