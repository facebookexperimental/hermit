// One SIGCHLD per child exit, with Linux's siginfo
// (https://github.com/rrnewton/hermit/issues/3895).
//
// The parent catches SIGCHLD with an SA_SIGINFO handler, forks one child and
// sleeps in sigsuspend until the handler has run. It then keeps SIGCHLD
// unblocked and sleeps until a SIGALRM one second later, so a second SIGCHLD
// for the same exit would be delivered too. It reaps the child and prints the
// number of SIGCHLDs and the first one's si_code, whether its si_pid is the
// child's, and its si_status.
//
// Argument `exit-group` (the default): the child calls _exit(7), which is
// exit_group. Argument `raw-exit`: the child's only thread calls the raw exit
// syscall. Linux sends one SIGCHLD either way: "sigchld=1 code=1 pid=child
// status=7". Exits 0 only for that.
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#define EXIT_CODE 7

static volatile sig_atomic_t sigchld_count;
static volatile sig_atomic_t first_code;
static volatile sig_atomic_t first_pid;
static volatile sig_atomic_t first_status;
static volatile sig_atomic_t alarmed;

static void on_sigchld(int sig, siginfo_t* info, void* context) {
  (void)sig;
  (void)context;
  if (sigchld_count++ == 0) {
    first_code = info->si_code;
    first_pid = info->si_pid;
    first_status = info->si_status;
  }
}

static void on_sigalrm(int sig) {
  (void)sig;
  alarmed = 1;
}

int main(int argc, char** argv) {
  int raw_exit = argc > 1 && strcmp(argv[1], "raw-exit") == 0;
  struct sigaction action;
  memset(&action, 0, sizeof(action));
  action.sa_sigaction = on_sigchld;
  action.sa_flags = SA_SIGINFO;
  if (sigaction(SIGCHLD, &action, NULL) != 0) {
    perror("sigaction");
    return 1;
  }
  memset(&action, 0, sizeof(action));
  action.sa_handler = on_sigalrm;
  if (sigaction(SIGALRM, &action, NULL) != 0) {
    perror("sigaction");
    return 1;
  }
  sigset_t blocked, unblocked;
  sigemptyset(&blocked);
  sigaddset(&blocked, SIGCHLD);
  sigaddset(&blocked, SIGALRM);
  sigprocmask(SIG_BLOCK, &blocked, &unblocked);

  pid_t child = fork();
  if (child < 0) {
    perror("fork");
    return 1;
  }
  if (child == 0) {
    if (raw_exit)
      syscall(SYS_exit, EXIT_CODE);
    _exit(EXIT_CODE);
  }

  while (!sigchld_count)
    sigsuspend(&unblocked);
  alarm(1);
  while (!alarmed)
    sigsuspend(&unblocked);

  int status;
  if (waitpid(child, &status, 0) != child || !WIFEXITED(status) ||
      WEXITSTATUS(status) != EXIT_CODE) {
    fprintf(stderr, "child did not exit with %d\n", EXIT_CODE);
    return 1;
  }
  printf(
      "sigchld=%d code=%d pid=%s status=%d\n",
      (int)sigchld_count,
      (int)first_code,
      first_pid == child ? "child" : "other",
      (int)first_status);
  return sigchld_count == 1 && first_code == CLD_EXITED && first_pid == child &&
          first_status == EXIT_CODE
      ? 0
      : 1;
}
