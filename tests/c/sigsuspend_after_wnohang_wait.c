// dash's `wait` for a background job (jobs.c waitproc): catch SIGCHLD, try
// wait4(WNOHANG), and if the child is still running, block every signal and
// sigsuspend under the old mask until the handler has run, then try again.
//
// Under Hermit the child exits between the parent's wait4 and its
// rt_sigsuspend, so the child-exit SIGCHLD reaches the parent while it blocks
// every signal and is still stopped at its rt_sigsuspend request. The kernel
// holds the signal pending and delivers it as soon as rt_sigsuspend installs
// the old mask. Exits 0 only once the child is reaped with status 0.
#include <fcntl.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

static volatile sig_atomic_t got_sigchld;

static void on_sigchld(int sig) {
  (void)sig;
  got_sigchld = 1;
}

static void put(const char* text) {
  size_t length = strlen(text);
  if (write(1, text, length) != (ssize_t)length)
    _exit(3);
}

int main(void) {
  struct sigaction action;
  memset(&action, 0, sizeof(action));
  action.sa_handler = on_sigchld;
  if (sigaction(SIGCHLD, &action, NULL) != 0) {
    perror("sigaction");
    return 1;
  }

  pid_t child = fork();
  if (child < 0) {
    perror("fork");
    return 1;
  }
  if (child == 0) {
    // Like dash, point the background job's stdin at /dev/null. The extra
    // turn places the child's exit after the parent's wait4(WNOHANG).
    int null_fd = open("/dev/null", O_RDONLY);
    if (null_fd < 0 || dup2(null_fd, 0) < 0)
      _exit(2);
    close(null_fd);
    put("child\n");
    _exit(0);
  }

  put("parent\n");
  for (;;) {
    int status;
    got_sigchld = 0;
    pid_t reaped = waitpid(-1, &status, WNOHANG);
    if (reaped == child) {
      if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) {
        fprintf(stderr, "child status %#x\n", status);
        return 1;
      }
      return 0;
    }
    if (reaped != 0) {
      perror("waitpid");
      return 1;
    }
    sigset_t all, old;
    sigfillset(&all);
    sigprocmask(SIG_SETMASK, &all, &old);
    while (!got_sigchld)
      sigsuspend(&old);
    sigprocmask(SIG_SETMASK, &old, NULL);
  }
}
