// epoll_wait returns EINTR only for a signal that is delivered: Linux
// discards a signal the process ignores when it is generated, so it never
// ends the wait (sig_ignored, kernel/signal.c). Under a tracer the kernel
// queues even an ignored signal, and ep_poll then returns EINTR, which no
// restart hides. The poll family (poll, ppoll, select, pselect6) ends with a
// restart code instead: poll restarts through restart_syscall, the others run
// again in place.
//
// Each wait below is, in turn, an epoll_wait, an epoll_pwait with no mask and
// an epoll_pwait2 with no mask; with a second argument "poll", a poll, a
// ppoll, a select, and a pselect6 without a mask argument and with one that
// names no mask (as glibc's pselect passes it), on the same pipe. The select,
// and the late wait of "chld" and "sigign" (then a pselect6), watch a copy of
// the pipe at fd 100: more descriptors than Hermit emulates, so they wait in
// the kernel even in a sequentialized run. Their timeouts sit on a read-only
// page: Linux then cannot write back the time
// left, so an interrupted select, pselect6 or ppoll returns EINTR instead of
// restarting (poll_select_finish, fs/select.c), and a signal that ends one
// shows.
//
// "chld": five 100 ms waits on an empty pipe, each with a child that exits at
// once (SIGCHLD, ignored by default), then an epoll_wait of up to a second
// that a child's write ends after three more children exit and a SIGWINCH
// arrives.
// "storm": a child sends 40 SIGWINCH, ignored by default, 20 ms apart while
// the parent makes three 100 ms waits, and exits right after the last: each
// wait returns 0 long before the storm ends.
// "sigign": as "chld", with SIGCHLD set to SIG_IGN, so children are reaped by
// the kernel.
// "alrm": a 30 ms ITIMER_REAL with a SIGALRM handler interrupts each of five
// 100 ms waits: EINTR.
// "exit": a thread waits with no timeout (an epoll_wait, or a poll in the poll
// family) while main returns, so the process exits with the thread inside its
// wait.
//
// Every mode blocks SIGUSR2 first, and reports whether the thread's mask is
// still exactly that after the waits (and, in "alrm", inside the handler).
#define _GNU_SOURCE
#include <errno.h>
#include <poll.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/mman.h>
#include <sys/select.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#ifndef SYS_epoll_pwait2
#define SYS_epoll_pwait2 441
#endif

// The descriptor every wait watches (the pipe's read end), its epoll
// instance, and whether the waits are the poll family's.
static int read_fd, epfd, poll_family;
// The copy of `read_fd` that the poll family's select and late wait watch.
#define HIGH_FD 100
// The poll family's timeouts, on a page main makes read-only: [0] for the
// 100 ms waits, [1] for the late wait's second.
struct timeouts {
  struct timespec ts;
  struct timeval tv;
};
static struct timeouts *timeouts;
static volatile sig_atomic_t alarms;
static volatile sig_atomic_t handler_mask_changed;

// Whether the thread's mask is as main set it: SIGUSR2 blocked, and neither
// ignored signal.
static int mask_as_before(void) {
  sigset_t now;
  sigprocmask(SIG_BLOCK, NULL, &now);
  return sigismember(&now, SIGUSR2) && !sigismember(&now, SIGCHLD) &&
         !sigismember(&now, SIGWINCH);
}

static void on_alarm(int sig) {
  (void)sig;
  alarms++;
  if (!mask_as_before()) {
    handler_mask_changed = 1;
  }
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

struct tally {
  int zero, ready, eintr, other;
};

static void count(struct tally *t, int r) {
  if (r == 0) {
    t->zero++;
  } else if (r > 0) {
    t->ready++;
  } else if (errno == EINTR) {
    t->eintr++;
  } else {
    t->other++;
  }
}

// The i-th wait of up to `ms` on `read_fd`, with no mask: epoll_wait,
// epoll_pwait or epoll_pwait2, or in the poll family (100 ms only) poll,
// ppoll, select (on HIGH_FD) or pselect6 (twice).
static int wait_ms(int i, int ms) {
  struct epoll_event ev;
  struct timespec timeout = {ms / 1000, ms % 1000 * 1000 * 1000};
  if (!poll_family) {
    switch (i % 3) {
    case 0:
      return epoll_wait(epfd, &ev, 1, ms);
    case 1:
      return epoll_pwait(epfd, &ev, 1, ms, NULL);
    default:
      return (int)syscall(SYS_epoll_pwait2, epfd, &ev, 1, &timeout, NULL, 0);
    }
  }
  struct pollfd pfd = {read_fd, POLLIN, 0};
  fd_set set;
  FD_ZERO(&set);
  FD_SET(read_fd, &set);
  const struct timeouts *t = &timeouts[0];
  // pselect6's { sigmask, sigsetsize } argument, naming no mask.
  unsigned long no_mask[2] = {0, 8};
  switch (i % 5) {
  case 0:
    return poll(&pfd, 1, ms);
  case 1:
    return (int)syscall(SYS_ppoll, &pfd, 1, &t->ts, NULL, 8);
  case 2: {
    fd_set high;
    FD_ZERO(&high);
    FD_SET(HIGH_FD, &high);
    return (int)syscall(SYS_select, HIGH_FD + 1, &high, NULL, NULL, &t->tv);
  }
  case 3:
    return (int)syscall(SYS_pselect6, read_fd + 1, &set, NULL, NULL, &t->ts,
                        NULL);
  default:
    return (int)syscall(SYS_pselect6, read_fd + 1, &set, NULL, NULL, &t->ts,
                        no_mask);
  }
}

static void print_tally(const char *what, const struct tally *t) {
  printf("%s: %d timed out, %d ready, %d EINTR, %d other", what, t->zero,
         t->ready, t->eintr, t->other);
}

static int children_mode(int write_fd, const char *name) {
  struct tally t = {0};
  for (int i = 0; i < 5; i++) {
    if (fork() == 0) {
      _exit(0);
    }
    count(&t, wait_ms(i, 100));
  }
  for (int i = 0; i < 3; i++) {
    if (fork() == 0) {
      _exit(0);
    }
  }
  pid_t parent = getpid();
  if (fork() == 0) {
    // A SIGWINCH reaches the late wait before the data does.
    struct timespec delay = {0, 30 * 1000 * 1000};
    syscall(SYS_nanosleep, &delay, NULL);
    kill(parent, SIGWINCH);
    syscall(SYS_nanosleep, &delay, NULL);
    _exit(write(write_fd, "x", 1) == 1 ? 0 : 1);
  }
  int late;
  if (poll_family) {
    // A second, watching HIGH_FD.
    fd_set set;
    FD_ZERO(&set);
    FD_SET(HIGH_FD, &set);
    late = (int)syscall(SYS_pselect6, HIGH_FD + 1, &set, NULL, NULL,
                        &timeouts[1].ts, NULL);
  } else {
    late = wait_ms(0, 1000);
  }
  int late_errno = late < 0 ? errno : 0;
  print_tally(name, &t);
  printf("; late data: %d%s; mask as before: %s; %d children\n", late,
         late_errno == EINTR ? " (EINTR)" : "", mask_as_before() ? "yes" : "no",
         reap());
  return 0;
}

static int storm_mode(void) {
  int done[2];
  if (pipe(done) != 0) {
    perror("pipe");
    return 2;
  }
  pid_t parent = getpid();
  if (fork() == 0) {
    struct timespec gap = {0, 20 * 1000 * 1000};
    for (int i = 0; i < 40; i++) {
      syscall(SYS_nanosleep, &gap, NULL);
      kill(parent, SIGWINCH);
    }
    _exit(write(done[1], "x", 1) == 1 ? 0 : 1);
  }
  struct tally t = {0};
  for (int i = 0; i < 3; i++) {
    count(&t, wait_ms(i, 100));
  }
  fd_set set;
  FD_ZERO(&set);
  FD_SET(done[0], &set);
  struct timeval tv = {0, 0};
  int over = select(done[0] + 1, &set, NULL, NULL, &tv);
  print_tally("storm", &t);
  printf(
      "; mask as before: %s; the storm was still running when they returned: "
      "%s; %d children\n",
      mask_as_before() ? "yes" : "no", over == 0 ? "yes" : "no", reap());
  return 0;
}

// Written by the waiter just before its wait.
static int waiting[2];

static void *wait_forever(void *arg) {
  (void)arg;
  if (write(waiting[1], "x", 1) != 1) {
    return NULL;
  }
  if (poll_family) {
    struct pollfd pfd = {read_fd, POLLIN, 0};
    poll(&pfd, 1, -1);
  } else {
    struct epoll_event ev;
    epoll_wait(epfd, &ev, 1, -1);
  }
  return NULL;
}

static int exit_mode(void) {
  pthread_t waiter;
  char c;
  if (pipe(waiting) != 0 ||
      pthread_create(&waiter, NULL, wait_forever, NULL) != 0 ||
      read(waiting[0], &c, 1) != 1) {
    perror("exit mode");
    return 2;
  }
  // The waiter is about to wait; 50 ms lets it get there.
  struct timespec delay = {0, 50 * 1000 * 1000};
  syscall(SYS_nanosleep, &delay, NULL);
  printf("exit: a thread was still waiting\n");
  fflush(stdout);
  return 0;
}

static int alarm_mode(void) {
  struct sigaction sa;
  memset(&sa, 0, sizeof sa);
  sa.sa_handler = on_alarm;
  sigaction(SIGALRM, &sa, NULL);
  struct tally t = {0};
  for (int i = 0; i < 5; i++) {
    struct itimerval once = {{0, 0}, {0, 30 * 1000}};
    setitimer(ITIMER_REAL, &once, NULL);
    count(&t, wait_ms(poll_family ? i : 0, 100));
  }
  print_tally("alrm", &t);
  printf(", %d alarms; mask as before in the handler: %s, after: %s\n",
         (int)alarms, handler_mask_changed ? "no" : "yes",
         mask_as_before() ? "yes" : "no");
  return 0;
}

int main(int argc, char **argv) {
  const char *mode = argc > 1 ? argv[1] : "chld";
  poll_family = argc > 2 && strcmp(argv[2], "poll") == 0;
  // A signal the guest blocks itself, which every wait must leave blocked.
  sigset_t usr2;
  sigemptyset(&usr2);
  sigaddset(&usr2, SIGUSR2);
  sigprocmask(SIG_BLOCK, &usr2, NULL);
  int pipe_fds[2];
  if (pipe(pipe_fds) != 0) {
    perror("pipe");
    return 2;
  }
  read_fd = pipe_fds[0];
  if (dup2(read_fd, HIGH_FD) != HIGH_FD) {
    perror("dup2");
    return 2;
  }
  timeouts = mmap(NULL, 4096, PROT_READ | PROT_WRITE,
                  MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  if (timeouts == MAP_FAILED) {
    perror("mmap");
    return 2;
  }
  timeouts[0] = (struct timeouts){{0, 100 * 1000 * 1000}, {0, 100 * 1000}};
  timeouts[1] = (struct timeouts){{1, 0}, {1, 0}};
  if (mprotect(timeouts, 4096, PROT_READ) != 0) {
    perror("mprotect");
    return 2;
  }
  epfd = epoll_create1(0);
  struct epoll_event ev = {.events = EPOLLIN};
  if (epfd < 0 || epoll_ctl(epfd, EPOLL_CTL_ADD, pipe_fds[0], &ev) != 0) {
    perror("epoll");
    return 2;
  }
  if (strcmp(mode, "storm") == 0) {
    return storm_mode();
  }
  if (strcmp(mode, "alrm") == 0) {
    return alarm_mode();
  }
  if (strcmp(mode, "exit") == 0) {
    return exit_mode();
  }
  if (strcmp(mode, "sigign") == 0) {
    signal(SIGCHLD, SIG_IGN);
  }
  return children_mode(pipe_fds[1], mode);
}
