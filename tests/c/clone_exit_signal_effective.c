// A child's effective exit signal, which decides both the parent's
// notification and which wait calls can reap the child (a child whose exit
// signal is not SIGCHLD is a "clone" child, reaped only with __WCLONE or
// __WALL):
//
// - `exec`: a clone(CLONE_VM | CLONE_VFORK | SIGUSR1) child that execs. A
//   successful exec resets its exit signal to SIGCHLD, so a plain waitpid
//   reaps it and the parent gets SIGCHLD.
// - `clone-parent`: a SIGCHLD child creates a
//   clone(CLONE_VM | CLONE_VFORK | CLONE_PARENT | SIGUSR1) grandchild.
//   CLONE_PARENT makes the grandchild inherit its creator's exit signal,
//   SIGCHLD, so the grandparent reaps it with a plain waitpid and gets SIGCHLD
//   for it.
//
// Both children are vfork children: a raw clone with a signal other than
// SIGCHLD and no CLONE_VFORK is reported by Hermit's ptrace backend as a
// thread of its creator, a separate defect this guest stays clear of.
//
// Prints the signals the parent got and exits 0 only if both children were
// reaped by plain waits and only SIGCHLD arrived.
#define _GNU_SOURCE
#include <errno.h>
#include <sched.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

static volatile sig_atomic_t sigchld_count;
static volatile sig_atomic_t sigusr1_count;

static void on_sigchld(int sig) {
  (void)sig;
  sigchld_count++;
}

static void on_sigusr1(int sig) {
  (void)sig;
  sigusr1_count++;
}

static char child_stack[64 * 1024] __attribute__((aligned(16)));
static char grandchild_stack[64 * 1024] __attribute__((aligned(16)));

static int exec_true(void* argument) {
  (void)argument;
  execl("/bin/true", "true", (char*)NULL);
  _exit(9);
}

static int exit_zero(void* argument) {
  (void)argument;
  _exit(0);
}

// Reap `pid` with a plain wait (no __WCLONE), retrying through EINTR.
static int reap_plainly(pid_t pid) {
  int status;
  pid_t reaped;
  do {
    reaped = waitpid(pid, &status, 0);
  } while (reaped < 0 && errno == EINTR);
  if (reaped != pid || !WIFEXITED(status) || WEXITSTATUS(status) != 0) {
    fprintf(stderr, "plain waitpid(%d) returned %d (errno %d)\n", (int)pid, (int)reaped, errno);
    return 1;
  }
  return 0;
}

int main(int argc, char** argv) {
  const char* mode = argc > 1 ? argv[1] : "exec";
  struct sigaction action;
  memset(&action, 0, sizeof(action));
  action.sa_handler = on_sigchld;
  if (sigaction(SIGCHLD, &action, NULL) != 0) {
    perror("sigaction");
    return 1;
  }
  action.sa_handler = on_sigusr1;
  if (sigaction(SIGUSR1, &action, NULL) != 0) {
    perror("sigaction");
    return 1;
  }
  if (strcmp(mode, "exec") == 0) {
    // The vfork returns once the child's exec has succeeded, so its exit
    // signal is SIGCHLD by the time the parent waits.
    pid_t child = clone(
        exec_true, child_stack + sizeof(child_stack), CLONE_VM | CLONE_VFORK | SIGUSR1, NULL);
    if (child < 0 || reap_plainly(child))
      return 1;
  } else {
    int pipe_fds[2];
    if (pipe(pipe_fds) != 0)
      return 1;
    pid_t creator = fork();
    if (creator < 0)
      return 1;
    if (creator == 0) {
      pid_t grandchild = clone(
          exit_zero,
          grandchild_stack + sizeof(grandchild_stack),
          CLONE_VM | CLONE_VFORK | CLONE_PARENT | SIGUSR1,
          NULL);
      if (write(pipe_fds[1], &grandchild, sizeof(grandchild)) != sizeof(grandchild))
        _exit(2);
      _exit(grandchild < 0 ? 3 : 0);
    }
    pid_t grandchild;
    ssize_t got;
    do {
      got = read(pipe_fds[0], &grandchild, sizeof(grandchild));
    } while (got < 0 && errno == EINTR);
    if (got != sizeof(grandchild))
      return 1;
    if (reap_plainly(creator) || reap_plainly(grandchild))
      return 1;
  }
  // Leave room for any late notification.
  for (int i = 0; i < 5; i++)
    usleep(1000);
  printf("%s sigusr1=%d sigchld_seen=%d\n", mode, (int)sigusr1_count, sigchld_count > 0);
  return sigusr1_count == 0 && sigchld_count > 0 ? 0 : 1;
}
