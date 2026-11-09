// A poll with a positive timeout that a signal ends in the kernel returns
// ERESTART_RESTARTBLOCK. With no handler to run, Linux restarts it through
// restart_syscall until the end time it kept; with a handler it returns EINTR.
// Under a tracer a default-ignored SIGCHLD is still queued, so children that
// exit while their parent polls end the poll in the kernel.
//
// "chld": five 100 ms polls of an empty pipe, each with a child that exits at
// once, then one poll that a child's write ends after three more children
// exit, which can end and restart it more than once.
// "storm": a child sends 40 SIGWINCH, which is ignored by default and which
// ptrace still queues, 20 ms apart while the parent polls an empty pipe for
// 100 ms: the poll must time out long before the storm ends. The child exits
// only after the poll has returned.
// "alrm": five 100 ms polls that a 30 ms ITIMER_REAL interrupts; SIGALRM has a
// handler.
#define _GNU_SOURCE
#include <errno.h>
#include <poll.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/time.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static volatile sig_atomic_t alarms;

static void on_alarm(int sig) {
  (void)sig;
  alarms++;
}

static int reap(void) {
  int children = 0;
  int status;
  while (wait(&status) > 0) {
    if (WIFEXITED(status) && WEXITSTATUS(status) == 0) {
      children++;
    }
  }
  return children;
}

static void tally(int r, int *zero, int *ready, int *eintr, int *other) {
  if (r == 0) {
    (*zero)++;
  } else if (r > 0) {
    (*ready)++;
  } else if (errno == EINTR) {
    (*eintr)++;
  } else {
    (*other)++;
  }
}

int main(int argc, char **argv) {
  int pipe_fds[2];
  if (pipe(pipe_fds) != 0) {
    perror("pipe");
    return 2;
  }
  struct pollfd fds = {pipe_fds[0], POLLIN, 0};
  int zero = 0, ready = 0, eintr = 0, other = 0;
  if (argc > 1 && strcmp(argv[1], "alrm") == 0) {
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_handler = on_alarm;
    sigaction(SIGALRM, &sa, NULL);
    for (int i = 0; i < 5; i++) {
      struct itimerval once = {{0, 0}, {0, 30 * 1000}};
      setitimer(ITIMER_REAL, &once, NULL);
      tally(poll(&fds, 1, 100), &zero, &ready, &eintr, &other);
    }
    printf("alrm: %d timed out, %d ready, %d EINTR, %d other, %d alarms\n",
           zero, ready, eintr, other, (int)alarms);
    return 0;
  }
  if (argc > 1 && strcmp(argv[1], "storm") == 0) {
    int done[2], release[2];
    if (pipe(done) != 0 || pipe(release) != 0) {
      perror("pipe");
      return 2;
    }
    pid_t parent = getpid();
    if (fork() == 0) {
      struct timespec gap = {0, 20 * 1000 * 1000};
      for (int i = 0; i < 40; i++) {
        nanosleep(&gap, NULL);
        kill(parent, SIGWINCH);
      }
      char c;
      // The sender exits only once the parent's poll has returned, so its
      // SIGCHLD never reaches the poll.
      int ok = write(done[1], "x", 1) == 1 && read(release[0], &c, 1) == 1;
      _exit(ok ? 0 : 1);
    }
    int r = poll(&fds, 1, 100);
    struct pollfd storm_done = {done[0], POLLIN, 0};
    int over = poll(&storm_done, 1, 0);
    if (write(release[1], "x", 1) != 1) {
      perror("write");
      return 2;
    }
    printf("storm: poll %d; the storm was still running when it returned: %s; "
           "%d children\n",
           r, over == 0 ? "yes" : "no", reap());
    return 0;
  }
  for (int i = 0; i < 5; i++) {
    if (fork() == 0) {
      _exit(0);
    }
    tally(poll(&fds, 1, 100), &zero, &ready, &eintr, &other);
  }
  for (int i = 0; i < 3; i++) {
    if (fork() == 0) {
      _exit(0);
    }
  }
  if (fork() == 0) {
    struct timespec delay = {0, 30 * 1000 * 1000};
    nanosleep(&delay, NULL);
    _exit(write(pipe_fds[1], "x", 1) == 1 ? 0 : 1);
  }
  int late = poll(&fds, 1, 1000);
  printf("chld: %d timed out, %d ready, %d EINTR, %d other; late data: %d "
         "revents %#x; %d children\n",
         zero, ready, eintr, other, late, fds.revents, reap());
  return 0;
}
