/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * A signal one guest process sends another, in the states the KVM
 * cross-process signal design admits at stage X1 slice C, and in the states
 * it refuses by name until a later slice
 * (https://github.com/rrnewton/hermit/issues/4485; Reverie 1429).
 *
 * Usage: cross_process_signal_admission MODE [HOW]
 *
 * MODE is the child's state and the signal:
 *
 *   ignored        SIGUSR2 (SIG_IGN), SIGHUP (blocked), SIGWINCH (ignored by
 *                  default) and SIGCONT (default action, the child not
 *                  stopped) to a child in pause(). None of them ends the
 *                  pause; the child's own alarm does, a second later, and
 *                  SIGHUP is still pending then.
 *   runnable       A caught SIGUSR1 to a child that runs a loop of system
 *                  calls: the handler runs once, with the sender's tgid and
 *                  the code HOW's call gives.
 *   select         A caught SIGUSR1 to a child in select(2): EINTR.
 *   pselect        The same in pselect6(2) under a temporary mask that admits
 *                  it, the child's own mask blocking it: EINTR.
 *   sigsuspend     The same in sigsuspend(2): EINTR, after the handler.
 *   sigsuspend-two The same to a child with two threads, both in
 *                  sigsuspend(2) under that temporary mask, sent to the
 *                  process naming its first thread: Linux wakes that thread
 *                  alone (complete_signal), which runs the handler, and the
 *                  other thread is still in sigsuspend afterwards.
 *   select-raw     A caught SIGUSR1 to a child in the raw select system call
 *                  (syscall(SYS_select), no descriptors, no timeout; libc's
 *                  select() issues pselect6 instead): EINTR.
 *   select-raw-two The same to a child whose first thread is in that raw
 *                  select and whose other thread spins in user code, with
 *                  no system call, and SIGUSR1 unblocked: runnable, so the
 *                  first thread is not the only receiver Linux could pick.
 *                  Linux wakes the first thread alone (complete_signal),
 *                  which runs the handler, and the other thread keeps
 *                  running and sees that it ran. ptrace's wrong-thread
 *                  delivery (https://github.com/rrnewton/hermit/issues/5455)
 *                  needs a sibling that makes system calls, and these rows
 *                  do not cover it.
 *   select-raw-timed, select-raw-two-timed
 *                  The same two with a 5-second timeout instead of none.
 *   sigtimedwait   SIGUSR1, blocked, to a child waiting for it in
 *                  sigtimedwait(2): the call returns it, no handler runs.
 *   poll           A caught SIGUSR1 to a child in poll(2) on an empty pipe:
 *                  EINTR.
 *   fatal          SIGTERM, default action, to a runnable child: it dies.
 *   sigkill        SIGKILL to a runnable child: it dies.
 *   stop           SIGTSTP, default action, to a runnable child: it stops;
 *                  SIGCONT resumes it.
 *   pause          A caught SIGUSR1 to a child in pause(2): EINTR.
 *   futex          A caught SIGUSR1 to a child whose only thread waits on a
 *                  futex: EINTR.
 *   read           A caught SIGUSR1 to a child in read(2) of an empty pipe:
 *                  EINTR (no SA_RESTART).
 *   recv           The same in recv(2) on a socket pair.
 *   waitfatal      SIGTERM, default action and blocked, to a child waiting
 *                  for it in sigtimedwait(2): the call returns it, and the
 *                  child lives.
 *   newthread      A caught SIGUSR1 sent right after the child created a
 *                  thread that does not block it, its first thread blocking
 *                  it: the handler runs in the new thread.
 *   exec           A caught SIGUSR1 the child blocks, queued before it
 *                  execs itself: after the exec the handler is gone, and
 *                  when the new image unblocks it, its default action ends
 *                  the child.
 *
 * HOW is the sending call (default kill): kill, tgkill, tkill, sigqueue
 * (rt_sigqueueinfo with SI_QUEUE), tgsigqueue (rt_tgsigqueueinfo with
 * SI_QUEUE), pidfd (pidfd_send_signal to a process pidfd, no siginfo) or
 * pidfd-thread (the same with PIDFD_SIGNAL_THREAD).
 *
 * Every line printed is one fact the sender or the child observed, and none
 * names a pid, so the output does not depend on the pid namespace. A send
 * that fails prints its errno, so a run that lets a refused send return
 * ENOSYS (--allow-unsupported-syscalls) shows that the child saw nothing.
 */

#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <linux/futex.h>
#include <poll.h>
#include <pthread.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/select.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#ifndef PIDFD_SIGNAL_THREAD
#define PIDFD_SIGNAL_THREAD (1UL << 0)
#endif

/* Child exit codes other than 0. */
#define CHILD_SETUP_FAILED 40
#define CHILD_TIMED_OUT 41

static volatile sig_atomic_t usr1_count = 0;
static volatile sig_atomic_t usr1_code = 0;
static volatile sig_atomic_t usr1_from_parent = 0;
static volatile sig_atomic_t usr1_value = 0;
static volatile sig_atomic_t usr1_in_new_thread = 0;
static volatile sig_atomic_t alarm_count = 0;
static volatile sig_atomic_t stop_requested = 0;

/* This program's path, which the exec mode runs again. */
static const char *self_path;

static void on_usr1(int signo, siginfo_t *info, void *context) {
  (void)signo;
  (void)context;
  usr1_count++;
  usr1_code = info->si_code;
  usr1_from_parent = info->si_pid == getppid();
  usr1_value = info->si_value.sival_int;
  usr1_in_new_thread = gettid() != getpid();
}

static void on_alarm(int signo) {
  (void)signo;
  alarm_count++;
}

static void install(int signo, void (*handler)(int)) {
  struct sigaction sa;
  memset(&sa, 0, sizeof(sa));
  sa.sa_handler = handler;
  sigemptyset(&sa.sa_mask);
  if (sigaction(signo, &sa, NULL) != 0) {
    _exit(CHILD_SETUP_FAILED);
  }
}

static void install_usr1(void) {
  struct sigaction sa;
  memset(&sa, 0, sizeof(sa));
  sa.sa_sigaction = on_usr1;
  sa.sa_flags = SA_SIGINFO;
  sigemptyset(&sa.sa_mask);
  if (sigaction(SIGUSR1, &sa, NULL) != 0) {
    _exit(CHILD_SETUP_FAILED);
  }
}

static void block(int signo) {
  sigset_t set;
  sigemptyset(&set);
  sigaddset(&set, signo);
  if (sigprocmask(SIG_BLOCK, &set, NULL) != 0) {
    _exit(CHILD_SETUP_FAILED);
  }
}

static const char *code_name(int code) {
  switch (code) {
    case SI_USER:
      return "SI_USER";
    case SI_TKILL:
      return "SI_TKILL";
    case SI_QUEUE:
      return "SI_QUEUE";
    default:
      return "other";
  }
}

static double monotonic(void) {
  struct timespec now;
  clock_gettime(CLOCK_MONOTONIC, &now);
  return (double)now.tv_sec + (double)now.tv_nsec / 1e9;
}

/* The child's report of its SIGUSR1 handler, for the modes that catch it. */
static void report_usr1(void) {
  printf(
      "child: handler ran %d time(s)%s%s%s\n",
      (int)usr1_count,
      usr1_count ? ", code " : "",
      usr1_count ? code_name(usr1_code) : "",
      usr1_count ? (usr1_from_parent ? ", from the parent" : ", NOT from the parent")
                 : "");
  if (usr1_count && usr1_code == SI_QUEUE) {
    printf("child: value %d\n", (int)usr1_value);
  }
}

static void report_call(const char *call, long result, int error) {
  if (result < 0) {
    printf("child: %s returned -1 %s\n", call, strerrorname_np(error));
  } else {
    printf("child: %s returned %ld\n", call, result);
  }
}

/* The parent waits until the child is about to enter its state. */
static int ready_pipe[2];
/* The parent writes here once it has printed what its sends answered; a
 * runnable child also ends its loop at it. The child reports only after it,
 * so the output's order does not depend on the host's scheduling. */
static int go_pipe[2];
static int go_seen = 0;

static void child_ready(void) {
  char byte = 'r';
  if (write(ready_pipe[1], &byte, 1) != 1) {
    _exit(CHILD_SETUP_FAILED);
  }
}

/* A runnable child: system calls, until a handler ran or the parent's go
 * byte came. */
static void child_run_until_stopped(void) {
  for (;;) {
    char byte;
    if (usr1_count > 0) {
      return;
    }
    if (read(go_pipe[0], &byte, 1) == 1) {
      go_seen = 1;
      return;
    }
    (void)getppid();
  }
}

/* The newthread mode's new thread: runnable, as child_run_until_stopped. */
static void *new_thread_main(void *arg) {
  (void)arg;
  child_run_until_stopped();
  return NULL;
}

/* The sigsuspend-two mode's other thread: it says it is about to wait, and
 * waits in sigsuspend under the same temporary mask as the first thread. If
 * it is the one that returns, it reports that and ends the child. */
static int thread_ready_pipe[2];
static sigset_t suspend_admit;
static volatile sig_atomic_t other_returned = 0;

static void child_wait_for_go(void);

static void *other_suspend_main(void *arg) {
  (void)arg;
  pid_t tid = gettid();
  if (write(thread_ready_pipe[1], &tid, sizeof(tid)) != sizeof(tid)) {
    _exit(CHILD_SETUP_FAILED);
  }
  int result = sigsuspend(&suspend_admit);
  int error = errno;
  other_returned = 1;
  child_wait_for_go();
  report_call("sigsuspend in the other thread", result, error);
  report_usr1();
  printf("child: the handler ran in the %s thread\n", usr1_in_new_thread ? "other" : "first");
  _exit(alarm_count ? CHILD_TIMED_OUT : 0);
}

/* The select-raw-two modes' other thread: it says it runs, then runs user
 * code with SIGUSR1 unblocked until it sees that the handler ran. It makes
 * no system call in the loop, which under Hermit would each take a turn
 * while the parent waits for the first thread to block. */
static volatile sig_atomic_t other_saw_handler = 0;

static void *other_runnable_main(void *arg) {
  (void)arg;
  char byte = 't';
  if (write(thread_ready_pipe[1], &byte, 1) != 1) {
    _exit(CHILD_SETUP_FAILED);
  }
  while (usr1_count == 0 && alarm_count == 0) {
  }
  other_saw_handler = usr1_count > 0;
  return NULL;
}

/* Wait for the parent's go byte before reporting. */
static void child_wait_for_go(void) {
  if (go_seen) {
    return;
  }
  struct pollfd go = {.fd = go_pipe[0], .events = POLLIN};
  while (poll(&go, 1, -1) < 0 && errno == EINTR) {
  }
  char byte;
  (void)!read(go_pipe[0], &byte, 1);
  go_seen = 1;
}

static void child_main(const char *mode) {
  close(ready_pipe[0]);
  close(go_pipe[1]);
  /* A backstop for a signal that never comes: a test that would otherwise
   * hang reports it. */
  install(SIGALRM, on_alarm);
  if (strcmp(mode, "ignored") == 0) {
    struct sigaction ignore;
    memset(&ignore, 0, sizeof(ignore));
    ignore.sa_handler = SIG_IGN;
    if (sigaction(SIGUSR2, &ignore, NULL) != 0) {
      _exit(CHILD_SETUP_FAILED);
    }
    block(SIGHUP);
    alarm(1);
    child_ready();
    double start = monotonic();
    int result = pause();
    int error = errno;
    double slept = monotonic() - start;
    child_wait_for_go();
    report_call("pause", result, error);
    sigset_t pending;
    sigpending(&pending);
    printf(
        "child: alarm %d, slept a second: %s, pending SIGHUP %d SIGUSR2 %d SIGWINCH %d "
        "SIGCONT %d\n",
        (int)alarm_count,
        slept >= 0.9 ? "yes" : "no",
        sigismember(&pending, SIGHUP),
        sigismember(&pending, SIGUSR2),
        sigismember(&pending, SIGWINCH),
        sigismember(&pending, SIGCONT));
    _exit(0);
  }
  if (strcmp(mode, "runnable") == 0 || strcmp(mode, "fatal") == 0 ||
      strcmp(mode, "sigkill") == 0 || strcmp(mode, "stop") == 0) {
    install_usr1();
    alarm(10);
    child_ready();
    child_run_until_stopped();
    if (alarm_count) {
      _exit(CHILD_TIMED_OUT);
    }
    child_wait_for_go();
    report_usr1();
    _exit(0);
  }
  if (strcmp(mode, "select") == 0) {
    install_usr1();
    child_ready();
    struct timeval timeout = {10, 0};
    int result = select(0, NULL, NULL, NULL, &timeout);
    child_wait_for_go();
    report_call("select", result, errno);
    report_usr1();
    _exit(0);
  }
  if (strcmp(mode, "pselect") == 0) {
    install_usr1();
    block(SIGUSR1);
    sigset_t admit;
    sigprocmask(SIG_SETMASK, NULL, &admit);
    sigdelset(&admit, SIGUSR1);
    child_ready();
    struct timespec timeout = {10, 0};
    int result = pselect(0, NULL, NULL, NULL, &timeout, &admit);
    child_wait_for_go();
    report_call("pselect", result, errno);
    report_usr1();
    _exit(0);
  }
  if (strcmp(mode, "sigsuspend") == 0) {
    install_usr1();
    block(SIGUSR1);
    sigset_t admit;
    sigprocmask(SIG_SETMASK, NULL, &admit);
    sigdelset(&admit, SIGUSR1);
    sigdelset(&admit, SIGALRM);
    alarm(10);
    child_ready();
    int result = sigsuspend(&admit);
    child_wait_for_go();
    report_call("sigsuspend", result, errno);
    report_usr1();
    _exit(alarm_count ? CHILD_TIMED_OUT : 0);
  }
  if (strcmp(mode, "sigsuspend-two") == 0) {
    install_usr1();
    block(SIGUSR1);
    sigprocmask(SIG_SETMASK, NULL, &suspend_admit);
    sigdelset(&suspend_admit, SIGUSR1);
    sigdelset(&suspend_admit, SIGALRM);
    if (pipe(thread_ready_pipe) != 0) {
      _exit(CHILD_SETUP_FAILED);
    }
    pthread_t other;
    if (pthread_create(&other, NULL, other_suspend_main, NULL) != 0) {
      _exit(CHILD_SETUP_FAILED);
    }
    /* Both thread ids follow the ready byte, for the parent's check that
     * both threads are asleep in rt_sigsuspend. */
    pid_t threads[2] = {gettid(), 0};
    if (read(thread_ready_pipe[0], &threads[1], sizeof(threads[1])) != sizeof(threads[1])) {
      _exit(CHILD_SETUP_FAILED);
    }
    alarm(10);
    child_ready();
    if (write(ready_pipe[1], threads, sizeof(threads)) != sizeof(threads)) {
      _exit(CHILD_SETUP_FAILED);
    }
    int result = sigsuspend(&suspend_admit);
    int error = errno;
    child_wait_for_go();
    report_call("sigsuspend in the first thread", result, error);
    report_usr1();
    printf(
        "child: the handler ran in the %s thread; the other thread %s\n",
        usr1_in_new_thread ? "other" : "first",
        other_returned ? "returned from sigsuspend" : "is still in sigsuspend");
    _exit(alarm_count ? CHILD_TIMED_OUT : 0);
  }
  if (strcmp(mode, "select-raw") == 0 || strcmp(mode, "select-raw-timed") == 0) {
    install_usr1();
    alarm(10);
    child_ready();
    struct timeval timeout = {5, 0};
    long result = syscall(
        SYS_select, 0, NULL, NULL, NULL, strcmp(mode, "select-raw") == 0 ? NULL : &timeout);
    int error = errno;
    child_wait_for_go();
    report_call("select", result, error);
    report_usr1();
    _exit(alarm_count ? CHILD_TIMED_OUT : 0);
  }
  if (strcmp(mode, "select-raw-two") == 0 || strcmp(mode, "select-raw-two-timed") == 0) {
    install_usr1();
    if (pipe(thread_ready_pipe) != 0) {
      _exit(CHILD_SETUP_FAILED);
    }
    pthread_t other;
    if (pthread_create(&other, NULL, other_runnable_main, NULL) != 0) {
      _exit(CHILD_SETUP_FAILED);
    }
    char byte;
    if (read(thread_ready_pipe[0], &byte, 1) != 1) {
      _exit(CHILD_SETUP_FAILED);
    }
    alarm(10);
    child_ready();
    struct timeval timeout = {5, 0};
    long result = syscall(
        SYS_select, 0, NULL, NULL, NULL, strcmp(mode, "select-raw-two") == 0 ? NULL : &timeout);
    int error = errno;
    if (pthread_join(other, NULL) != 0) {
      _exit(CHILD_SETUP_FAILED);
    }
    child_wait_for_go();
    report_call("select in the first thread", result, error);
    report_usr1();
    printf(
        "child: the handler ran in the %s thread; the other thread %s\n",
        usr1_in_new_thread ? "other" : "first",
        other_saw_handler ? "ran and saw it" : "never saw it");
    _exit(alarm_count ? CHILD_TIMED_OUT : 0);
  }
  if (strcmp(mode, "sigtimedwait") == 0) {
    install_usr1();
    block(SIGUSR1);
    sigset_t wanted;
    sigemptyset(&wanted);
    sigaddset(&wanted, SIGUSR1);
    child_ready();
    siginfo_t info;
    struct timespec timeout = {10, 0};
    int result = sigtimedwait(&wanted, &info, &timeout);
    child_wait_for_go();
    report_call("sigtimedwait", result, errno);
    if (result == SIGUSR1) {
      printf(
          "child: code %s, %s\n",
          code_name(info.si_code),
          info.si_pid == getppid() ? "from the parent" : "NOT from the parent");
    }
    report_usr1();
    _exit(0);
  }
  if (strcmp(mode, "poll") == 0) {
    install_usr1();
    int empty[2];
    if (pipe(empty) != 0) {
      _exit(CHILD_SETUP_FAILED);
    }
    child_ready();
    struct pollfd wait = {.fd = empty[0], .events = POLLIN};
    int result = poll(&wait, 1, 10000);
    child_wait_for_go();
    report_call("poll", result, errno);
    report_usr1();
    _exit(0);
  }
  if (strcmp(mode, "pause") == 0) {
    install_usr1();
    alarm(5);
    child_ready();
    int result = pause();
    child_wait_for_go();
    report_call("pause", result, errno);
    printf("child: alarm %d\n", (int)alarm_count);
    report_usr1();
    _exit(0);
  }
  if (strcmp(mode, "read") == 0 || strcmp(mode, "recv") == 0) {
    install_usr1();
    int fds[2];
    int made = strcmp(mode, "read") == 0 ? pipe(fds) : socketpair(AF_UNIX, SOCK_STREAM, 0, fds);
    if (made != 0) {
      _exit(CHILD_SETUP_FAILED);
    }
    alarm(5);
    child_ready();
    char byte;
    long result = strcmp(mode, "read") == 0 ? read(fds[0], &byte, 1) : recv(fds[0], &byte, 1, 0);
    int error = errno;
    child_wait_for_go();
    report_call(mode, result, error);
    report_usr1();
    _exit(alarm_count ? CHILD_TIMED_OUT : 0);
  }
  if (strcmp(mode, "waitfatal") == 0) {
    block(SIGTERM);
    sigset_t wanted;
    sigemptyset(&wanted);
    sigaddset(&wanted, SIGTERM);
    child_ready();
    siginfo_t info;
    struct timespec timeout = {10, 0};
    int result = sigtimedwait(&wanted, &info, &timeout);
    int error = errno;
    child_wait_for_go();
    report_call("sigtimedwait", result, error);
    if (result == SIGTERM) {
      printf(
          "child: code %s, %s\n",
          code_name(info.si_code),
          info.si_pid == getppid() ? "from the parent" : "NOT from the parent");
    }
    _exit(0);
  }
  if (strcmp(mode, "newthread") == 0) {
    install_usr1();
    pthread_t worker;
    if (pthread_create(&worker, NULL, new_thread_main, NULL) != 0) {
      _exit(CHILD_SETUP_FAILED);
    }
    /* Only this thread blocks it; the new one has its creator's mask from
     * before. */
    block(SIGUSR1);
    child_ready();
    pthread_join(worker, NULL);
    child_wait_for_go();
    report_usr1();
    printf("child: handler in the new thread: %s\n", usr1_in_new_thread ? "yes" : "no");
    _exit(0);
  }
  if (strcmp(mode, "exec") == 0) {
    install_usr1();
    block(SIGUSR1);
    child_ready();
    child_wait_for_go();
    execl(self_path, self_path, "exec-after", (char *)NULL);
    _exit(CHILD_SETUP_FAILED);
  }
  if (strcmp(mode, "futex") == 0) {
    install_usr1();
    static uint32_t word = 0;
    child_ready();
    struct timespec timeout = {5, 0};
    long result = syscall(SYS_futex, &word, FUTEX_WAIT_PRIVATE, 0, &timeout, NULL, 0);
    child_wait_for_go();
    report_call("futex wait", result, errno);
    report_usr1();
    _exit(0);
  }
  _exit(CHILD_SETUP_FAILED);
}

/* Send `signo` to `child` by `how`, print the answer, and return it. */
static long send_signal(const char *how, pid_t child, int signo, int pidfd) {
  long result;
  if (strcmp(how, "kill") == 0) {
    result = kill(child, signo);
  } else if (strcmp(how, "tgkill") == 0) {
    result = syscall(SYS_tgkill, child, child, signo);
  } else if (strcmp(how, "tkill") == 0) {
    result = syscall(SYS_tkill, child, signo);
  } else if (strcmp(how, "sigqueue") == 0 || strcmp(how, "tgsigqueue") == 0) {
    siginfo_t info;
    memset(&info, 0, sizeof(info));
    info.si_signo = signo;
    info.si_code = SI_QUEUE;
    info.si_pid = getpid();
    info.si_uid = getuid();
    info.si_value.sival_int = 42;
    result = strcmp(how, "sigqueue") == 0
        ? syscall(SYS_rt_sigqueueinfo, child, signo, &info)
        : syscall(SYS_rt_tgsigqueueinfo, child, child, signo, &info);
  } else if (strcmp(how, "pidfd") == 0) {
    result = syscall(SYS_pidfd_send_signal, pidfd, signo, NULL, 0);
  } else if (strcmp(how, "pidfd-thread") == 0) {
    result = syscall(SYS_pidfd_send_signal, pidfd, signo, NULL, PIDFD_SIGNAL_THREAD);
  } else {
    fprintf(stderr, "unknown HOW %s\n", how);
    exit(2);
  }
  if (result < 0) {
    printf("%s of %s: -1 %s\n", how, sigabbrev_np(signo), strerrorname_np(errno));
  } else {
    printf("%s of %s: %ld\n", how, sigabbrev_np(signo), result);
  }
  return result;
}

static void report_status(int status) {
  if (WIFEXITED(status)) {
    printf("child exited %d\n", WEXITSTATUS(status));
  } else if (WIFSIGNALED(status)) {
    printf("child killed by %s\n", sigabbrev_np(WTERMSIG(status)));
  } else if (WIFSTOPPED(status)) {
    printf("child stopped by %s\n", sigabbrev_np(WSTOPSIG(status)));
  } else if (WIFCONTINUED(status)) {
    printf("child continued\n");
  }
}

/* Long enough, after the child's ready byte, for it to enter a blocking call
 * natively; under Hermit the child reaches it first anyway. */
static void let_the_child_block(void) {
  struct timespec delay = {0, 50 * 1000 * 1000};
  nanosleep(&delay, NULL);
}

/* The system call /proc/CHILD/task/TID/syscall says the thread is blocked
 * in: 1 when it is SYS_rt_sigsuspend, 0 when it is another or none, -1
 * when the file cannot be read (Hermit's /proc has no such file). */
static int blocked_in_rt_sigsuspend(pid_t child, pid_t tid) {
  char path[64];
  snprintf(path, sizeof(path), "/proc/%d/task/%d/syscall", (int)child, (int)tid);
  FILE *file = fopen(path, "r");
  if (file == NULL) {
    return -1;
  }
  long number = -1;
  int fields = fscanf(file, "%ld", &number);
  fclose(file);
  return fields == 1 && number == SYS_rt_sigsuspend;
}

/* The sigsuspend-two mode's wait: until both of the child's threads are
 * asleep in rt_sigsuspend, polled for up to 10 seconds, so that a loaded
 * host cannot send the signal while one of them is still on its way.
 * Where /proc cannot say (under Hermit), the fixed delay above, which there
 * is virtual time and lets both threads reach their calls first. */
static void let_both_threads_block(pid_t child, const pid_t threads[2]) {
  double deadline = monotonic() + 10;
  for (;;) {
    int first = blocked_in_rt_sigsuspend(child, threads[0]);
    int other = blocked_in_rt_sigsuspend(child, threads[1]);
    if (first < 0 || other < 0) {
      let_the_child_block();
      return;
    }
    if (first && other) {
      return;
    }
    if (monotonic() > deadline) {
      printf("parent: the child's threads were not both in rt_sigsuspend after 10 seconds\n");
      return;
    }
    struct timespec poll_delay = {0, 1000 * 1000};
    nanosleep(&poll_delay, NULL);
  }
}

int main(int argc, char **argv) {
  if (argc < 2) {
    fprintf(stderr, "usage: %s MODE [HOW]\n", argv[0]);
    return 2;
  }
  const char *mode = argv[1];
  const char *how = argc > 2 ? argv[2] : "kill";
  self_path = argv[0];
  setvbuf(stdout, NULL, _IONBF, 0);
  if (strcmp(mode, "exec-after") == 0) {
    /* The exec mode's child after its exec: the copy queued before it is
     * still pending, its handler reset to the default. */
    sigset_t pending;
    sigpending(&pending);
    printf("child: after the exec, SIGUSR1 pending %d\n", sigismember(&pending, SIGUSR1));
    sigset_t usr1;
    sigemptyset(&usr1);
    sigaddset(&usr1, SIGUSR1);
    sigprocmask(SIG_UNBLOCK, &usr1, NULL);
    printf("child: still alive after unblocking SIGUSR1\n");
    return 0;
  }
  if (pipe(ready_pipe) != 0 || pipe2(go_pipe, O_NONBLOCK) != 0) {
    perror("pipe");
    return 1;
  }
  pid_t child = fork();
  if (child < 0) {
    perror("fork");
    return 1;
  }
  if (child == 0) {
    child_main(mode);
  }
  close(ready_pipe[1]);
  close(go_pipe[0]);
  /* The go byte goes to a child that may already be dead. */
  signal(SIGPIPE, SIG_IGN);
  int pidfd = (int)syscall(SYS_pidfd_open, child, 0);
  if (pidfd < 0) {
    perror("pidfd_open");
    return 1;
  }
  char byte;
  if (read(ready_pipe[0], &byte, 1) != 1) {
    fprintf(stderr, "the child never got ready\n");
    return 1;
  }
  int status;
  if (strcmp(mode, "ignored") == 0) {
    let_the_child_block();
    send_signal(how, child, SIGUSR2, pidfd);
    send_signal(how, child, SIGHUP, pidfd);
    send_signal(how, child, SIGWINCH, pidfd);
    send_signal(how, child, SIGCONT, pidfd);
  } else if (strcmp(mode, "fatal") == 0 || strcmp(mode, "sigkill") == 0) {
    send_signal(how, child, strcmp(mode, "fatal") == 0 ? SIGTERM : SIGKILL, pidfd);
  } else if (strcmp(mode, "waitfatal") == 0) {
    let_the_child_block();
    send_signal(how, child, SIGTERM, pidfd);
  } else if (strcmp(mode, "stop") == 0) {
    /* A send that failed stops nothing, so there is no stop to wait for. */
    if (send_signal(how, child, SIGTSTP, pidfd) == 0 &&
        waitpid(child, &status, WUNTRACED) > 0) {
      report_status(status);
    }
    send_signal(how, child, SIGCONT, pidfd);
  } else {
    if (strcmp(mode, "sigsuspend-two") == 0) {
      pid_t threads[2];
      if (read(ready_pipe[0], threads, sizeof(threads)) != sizeof(threads)) {
        fprintf(stderr, "the child never sent its thread ids\n");
        return 1;
      }
      let_both_threads_block(child, threads);
    } else if (strcmp(mode, "runnable") != 0 && strcmp(mode, "newthread") != 0) {
      let_the_child_block();
    }
    send_signal(how, child, SIGUSR1, pidfd);
  }
  /* What every send answered is printed: the child may report, and a
   * runnable child whose signal was refused ends its loop. */
  (void)!write(go_pipe[1], "g", 1);
  while (waitpid(child, &status, WUNTRACED) > 0) {
    report_status(status);
    if (WIFEXITED(status) || WIFSIGNALED(status)) {
      break;
    }
  }
  return 0;
}
