// A process rewrites the mask another process passed to rt_sigsuspend, in a
// MAP_SHARED page, while that call waits for its turn under Hermit's
// scheduler.
//
// The parent blocks every signal, catches SIGCHLD and SIGUSR1, and waits in
// sigsuspend on a mask in the shared page that blocks neither, until SIGUSR1
// has arrived. Once the parent has said it is about to suspend, its child
// exits, and a peer process adds SIGCHLD to the shared mask and then sends the
// parent SIGUSR1. The yields place, under Hermit with the timeslice disabled,
// the child's exit and the peer's rewrite between the moment Detcore reads the
// mask for the parent's rt_sigsuspend and the moment the call runs.
//
// Linux copies the mask when rt_sigsuspend starts, so the first call wakes for
// the child's SIGCHLD; the second sleeps under the rewritten mask and wakes for
// the peer's SIGUSR1. Exits 0 only if each handler ran exactly once.
#include <sched.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <unistd.h>

struct shared {
  sigset_t mask;
  volatile int suspending;
};

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

static int reap(pid_t pid) {
  int status;
  if (waitpid(pid, &status, 0) != pid || !WIFEXITED(status) ||
      WEXITSTATUS(status) != 0) {
    fprintf(stderr, "child %d did not exit cleanly\n", (int)pid);
    return 1;
  }
  return 0;
}

int main(void) {
  struct shared* shared = mmap(
      NULL,
      sizeof(*shared),
      PROT_READ | PROT_WRITE,
      MAP_SHARED | MAP_ANONYMOUS,
      -1,
      0);
  if (shared == MAP_FAILED) {
    perror("mmap");
    return 1;
  }
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
  sigset_t all;
  sigfillset(&all);
  sigprocmask(SIG_SETMASK, &all, NULL);
  sigemptyset(&shared->mask);
  pid_t parent = getpid();

  pid_t peer = fork();
  if (peer < 0) {
    perror("fork");
    return 1;
  }
  if (peer == 0) {
    while (!shared->suspending)
      sched_yield();
    sched_yield();
    sigaddset(&shared->mask, SIGCHLD);
    for (int i = 0; i < 3; i++)
      sched_yield();
    kill(parent, SIGUSR1);
    _exit(0);
  }
  pid_t child = fork();
  if (child < 0) {
    perror("fork");
    return 1;
  }
  if (child == 0) {
    while (!shared->suspending)
      sched_yield();
    _exit(0);
  }

  shared->suspending = 1;
  sched_yield();
  while (!sigusr1_count)
    sigsuspend(&shared->mask);
  if (reap(peer) || reap(child))
    return 1;
  printf("sigchld=%d sigusr1=%d\n", (int)sigchld_count, (int)sigusr1_count);
  return sigchld_count == 1 && sigusr1_count == 1 ? 0 : 1;
}
