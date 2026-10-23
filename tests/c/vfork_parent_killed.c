// A vfork parent killed while its vfork child runs
// (https://github.com/rrnewton/hermit/issues/3984).
//
// Process A forks from main and vforks child C. C sends SIGKILL to A, its
// parent, and _exits. main waits for A and reports how A ended. Before the fix
// the killed parent left its vfork barrier behind and Hermit stopped selecting
// any thread once C was gone; natively, and now under Hermit, main prints
// "A killed by signal 9".
#define _GNU_SOURCE
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/wait.h>
#include <unistd.h>

int main(void) {
  pid_t a = fork();
  if (a < 0) {
    perror("fork");
    return 1;
  }
  if (a == 0) {
    pid_t c = vfork();
    if (c == 0) {
      kill(getppid(), SIGKILL);
      _exit(0);
    }
    _exit(3); /* not reached: A dies while blocked in vfork */
  }
  int st = 0;
  if (waitpid(a, &st, 0) != a) {
    perror("waitpid");
    return 1;
  }
  if (WIFSIGNALED(st))
    printf("A killed by signal %d\n", WTERMSIG(st));
  else
    printf("A exited %d\n", WEXITSTATUS(st));
  fflush(stdout);
  return 0;
}
