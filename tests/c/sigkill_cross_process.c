// A SIGKILL sent by one guest process to another
// (https://github.com/rrnewton/hermit/issues/3994).
//
// Mode "parent": process A forks C; C kills A, its parent, with SIGKILL and
// exits; main waits for A. Mode "bystander": the same, plus a bystander
// process B that takes one scheduler turn per sched_yield throughout, and C
// keeps yielding after the kill. Natively, and under Hermit, main prints how A
// ended. Before the fix the killed process left the scheduler only when its
// own deregistration arrived, at a moment the host chose, so the committed
// schedule (and Hermit's INFO log) differed between runs.
#define _GNU_SOURCE
#include <sched.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

#define YIELDS 200

int main(int argc, char **argv) {
  int bystander = argc > 1 && strcmp(argv[1], "bystander") == 0;
  pid_t b = -1;
  if (bystander) {
    b = fork();
    if (b < 0) {
      perror("fork");
      return 1;
    }
    if (b == 0) {
      for (int i = 0; i < YIELDS; i++) {
        sched_yield();
      }
      _exit(0);
    }
  }
  pid_t a = fork();
  if (a < 0) {
    perror("fork");
    return 1;
  }
  if (a == 0) {
    pid_t c = fork();
    if (c == 0) {
      kill(getppid(), SIGKILL);
      if (bystander) {
        for (int i = 0; i < YIELDS; i++) {
          sched_yield();
        }
      }
      _exit(0);
    }
    for (;;) {
      pause();
    }
  }
  int status = 0;
  if (waitpid(a, &status, 0) != a) {
    perror("waitpid A");
    return 1;
  }
  if (WIFSIGNALED(status)) {
    printf("A killed by signal %d\n", WTERMSIG(status));
  } else {
    printf("A exited %d\n", WEXITSTATUS(status));
  }
  if (bystander) {
    int bystander_status = 0;
    if (waitpid(b, &bystander_status, 0) != b) {
      perror("waitpid B");
      return 1;
    }
    printf("B exited %d\n", WEXITSTATUS(bystander_status));
  }
  /* C, A's orphan, goes to the container's init; main does not wait for it. */
  fflush(stdout);
  return 0;
}
