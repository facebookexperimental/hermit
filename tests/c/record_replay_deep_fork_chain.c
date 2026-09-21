#include <stdio.h>
#include <stdlib.h>
#include <sys/wait.h>
#include <unistd.h>

enum { FORK_DEPTH = 125 };

int main(void) {
  for (int depth = 0; depth < FORK_DEPTH; ++depth) {
    pid_t child = fork();
    if (child < 0) {
      perror("fork");
      return 1;
    }
    if (child == 0) {
      continue;
    }

    int status = 0;
    if (waitpid(child, &status, 0) != child || !WIFEXITED(status) || WEXITSTATUS(status) != 0) {
      fprintf(stderr, "child at depth %d failed: status=%d\n", depth, status);
      return 1;
    }
    dprintf(STDOUT_FILENO, "parent-%d\n", depth);
    return 0;
  }

  dprintf(STDOUT_FILENO, "leaf-%d\n", FORK_DEPTH);
  return 0;
}
