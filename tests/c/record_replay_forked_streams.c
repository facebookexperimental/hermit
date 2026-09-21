#include <stdio.h>
#include <stdlib.h>
#include <sys/wait.h>
#include <unistd.h>

static void run_child(const char *message) {
  if (getpid() <= 0) {
    _exit(2);
  }
  if (dprintf(STDOUT_FILENO, "%s\n", message) < 0) {
    _exit(3);
  }
  _exit(0);
}

static void fork_and_wait(const char *message) {
  pid_t child = fork();
  if (child < 0) {
    perror("fork");
    exit(1);
  }
  if (child == 0) {
    run_child(message);
  }

  int status = 0;
  if (waitpid(child, &status, 0) != child || !WIFEXITED(status) || WEXITSTATUS(status) != 0) {
    fprintf(stderr, "child failed: status=%d\n", status);
    exit(1);
  }
}

int main(void) {
  fork_and_wait("first-child");
  fork_and_wait("second-child");
  puts("parent");
  return 0;
}
