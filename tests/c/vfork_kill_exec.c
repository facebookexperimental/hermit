// A vfork child that kills its parent and then execs
// (https://github.com/rrnewton/hermit/issues/3984).
//
// main forks A; A vforks C. C sends SIGKILL to A and execs this program again
// in "forkwait" mode: the new image forks G, waits for it, and reports through
// a pipe that main reads until end of file. A barrier left behind after C's
// exec would let only C run, so G could never run and C would wait forever.
// main prints how A ended, then C's report. Natively, and under Hermit:
// "A killed by signal 9" then "forkwait ok".
#define _GNU_SOURCE
#include <sched.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

static int forkwait(int fd) {
  pid_t g = fork();
  if (g < 0) return 1;
  if (g == 0) {
    for (int i = 0; i < 5; i++) sched_yield();
    _exit(7);
  }
  int st = 0;
  if (waitpid(g, &st, 0) != g) return 2;
  const char *msg = WIFEXITED(st) && WEXITSTATUS(st) == 7 ? "forkwait ok\n" : "forkwait bad\n";
  if (write(fd, msg, strlen(msg)) < 0) return 3;
  return 0;
}

int main(int argc, char **argv) {
  if (argc == 3 && strcmp(argv[1], "forkwait") == 0) return forkwait(atoi(argv[2]));
  int fds[2];
  if (pipe(fds)) return 1;
  pid_t a = fork();
  if (a < 0) return 1;
  if (a == 0) {
    pid_t c = vfork();
    if (c == 0) {
      char fd[16];
      snprintf(fd, sizeof fd, "%d", fds[1]);
      kill(getppid(), SIGKILL);
      execl(argv[0], argv[0], "forkwait", fd, (char *)NULL);
      _exit(127);
    }
    _exit(3); /* not reached: A dies while blocked in vfork */
  }
  close(fds[1]);
  int st = 0;
  if (waitpid(a, &st, 0) != a) return 1;
  if (WIFSIGNALED(st))
    printf("A killed by signal %d\n", WTERMSIG(st));
  else
    printf("A exited %d\n", WEXITSTATUS(st));
  char buf[64];
  ssize_t n, total = 0;
  while ((n = read(fds[0], buf + total, sizeof buf - 1 - total)) > 0) total += n;
  buf[total] = '\0';
  printf("%s", buf);
  fflush(stdout);
  return 0;
}
