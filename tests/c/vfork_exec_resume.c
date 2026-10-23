// A vfork parent resumes when its child execs or exits, as vfork(2) says and
// kernel/fork.c does (mm_release -> complete_vfork_done). Under Hermit it is
// then runnable, so when the exec'd child yields, the parent's next action
// runs before the child finishes, as natively.
//
// Case 1: the child execs this program in "work" mode, which yields five
// times and then writes "child done". The parent writes "parent resumed" as
// soon as it resumes. Natively, and under Hermit: "parent resumed" first.
// Case 2: the child _exits at once. The parent's wait4(WNOHANG) finds it
// exited: "after exit: exited".
#define _GNU_SOURCE
#include <sched.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

static void say(const char *msg) {
  if (write(STDOUT_FILENO, msg, strlen(msg)) < 0) _exit(2);
}

int main(int argc, char **argv) {
  if (argc == 2 && strcmp(argv[1], "work") == 0) {
    for (int i = 0; i < 5; i++) sched_yield();
    say("child done\n");
    return 5;
  }
  int st = 0;
  pid_t c = vfork();
  if (c < 0) return 1;
  if (c == 0) {
    execl(argv[0], argv[0], "work", (char *)NULL);
    _exit(127);
  }
  say("parent resumed\n");
  if (waitpid(c, &st, 0) != c) return 1;
  printf("exec child exit=%d\n", WIFEXITED(st) ? WEXITSTATUS(st) : -1);
  fflush(stdout);

  c = vfork();
  if (c < 0) return 1;
  if (c == 0) _exit(6);
  pid_t r = waitpid(c, &st, WNOHANG);
  printf("after exit: %s\n", r == c ? "exited" : r == 0 ? "running" : "error");
  if (r == 0 && waitpid(c, &st, 0) != c) return 1;
  printf("exit child exit=%d\n", WIFEXITED(st) ? WEXITSTATUS(st) : -1);
  return 0;
}
