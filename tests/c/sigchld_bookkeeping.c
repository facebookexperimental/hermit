/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * How each SIGCHLD copy is accounted for: who may take the scheduler's copy,
 * which siginfo a coalesced delivery carries, and what a flush or a faulted
 * wait takes (https://github.com/rrnewton/hermit/issues/4126; the re-checks of
 * https://github.com/rrnewton/hermit/pull/4131).
 *
 *   susblk-pause, susblk-nano (with "late" as a further argument, the child
 *                  exits 100 ms in; with "sfd", the process also holds a
 *                  signalfd for SIGCHLD that nobody reads; with "open", the
 *                  sibling's sigsuspend mask leaves SIGCHLD unblocked, so
 *                  either thread may take it)
 *                  A sibling sleeps in sigsuspend under a mask that blocks
 *                  SIGCHLD, so it cannot take it. The main thread (SIGCHLD
 *                  caught) forks a child that exits and pauses or sleeps 5 s:
 *                  EINTR, the handler in main.
 *   flush-ign, flush-dfl
 *                  SIGCHLD blocked; a child's copy is pending when the action
 *                  becomes SIG_IGN or SIG_DFL (a flush). A second child's copy
 *                  then interrupts a nanosleep, with its own siginfo.
 *   sq-before, sq-after
 *                  Single-threaded, SIGCHLD blocked: the process sigqueues
 *                  itself SIGCHLD (value 7) before or after a child exits.
 *                  One delivery, carrying the first-queued copy's siginfo.
 *   efault-wait, efault-sfd, efault-sfdv, efault-sfdtail, efault-sfdshared,
 *   efault-sfdkern, efault-sfdhigh
 *                  A self-tgkilled SIGCHLD is taken by a sigtimedwait whose
 *                  siginfo buffer faults (EFAULT), or by a signalfd read or
 *                  readv whose buffer faults (Linux dequeues each record
 *                  before it copies it out). With -sfdtail, a self-tgkilled
 *                  SIGUSR1 is read into a writable record first, and the
 *                  SIGCHLD record after it faults, so the read returns 128.
 *                  Two children then exit with statuses 11 and 22, and each
 *                  wait reports its own. With -sfdshared, the faulting read
 *                  takes a reaped child's (status 11) SIGCHLD from the shared
 *                  queue instead, and only the second child's wait follows.
 *                  With -sfdkern, the read's buffer is a kernel address, so
 *                  it fails before it dequeues anything: the reaped child's
 *                  SIGCHLD is still there for the next wait. With -sfdhigh,
 *                  the buffer is the unmapped 2^52, a user address only with
 *                  five-level page tables: the read dequeues the SIGCHLD
 *                  there, and fails before it elsewhere ("as Linux does"
 *                  compares the pending bit with the kernel's address width,
 *                  measured with a mapping hint above 2^47).
 *   efin-priv, efin-shared
 *                  A reaped child's SIGCHLD is pending (with -priv, also the
 *                  process's own rt_tgsigqueueinfo SIGCHLD, value 9, in its
 *                  private queue) when a sigtimedwait's set pointer faults:
 *                  EFAULT before anything is dequeued, so later waits take
 *                  the private copy, then the child's.
 *   pidfd-thread   A thread-scoped pidfd_send_signal (PIDFD_SIGNAL_THREAD) of
 *                  SIGCHLD to itself is handled (SI_TKILL); with SIGCHLD then
 *                  blocked, a child exits, and once unblocked its own copy
 *                  arrives with its siginfo.
 *   pidfd-efault   A reaped child's SIGCHLD (status 11) is pending, SIGCHLD
 *                  blocked, when a thread-scoped pidfd_send_signal queues
 *                  SIGCHLD to the thread's private queue; a sigtimedwait whose
 *                  siginfo copy-out faults takes that private copy, so the
 *                  next wait takes the child's.
 *   efwide-priv, efwide-shared
 *                  A reaped child's SIGCHLD is pending, SIGCHLD and SIGUSR1
 *                  blocked, with SIGUSR1 queued to the thread (with -priv,
 *                  after the process's own rt_tgsigqueueinfo SIGCHLD, value 9)
 *                  or to the process, when a sigtimedwait for both faults
 *                  writing its siginfo: it took SIGUSR1, the lower signal of
 *                  the queue it took from, so later waits for SIGCHLD take the
 *                  private copy (if any), then the child's.
 *   sfdsib-pause, sfdsib-nano (with "late", the child exits 100 ms in; with
 *                  "rt", the signalfd also reads SIGRTMIN+2; with "poll", the
 *                  sibling polls a nonblocking signalfd with sched_yield
 *                  between reads, each read inside the schedule)
 *                  A sibling blocks SIGCHLD and blocks reading a signalfd for
 *                  SIGCHLD; the main thread (SIGCHLD caught) forks a child
 *                  that exits and pauses or sleeps 5 s. Exactly one of them
 *                  takes the one SIGCHLD: the main thread's handler (EINTR),
 *                  or the sibling's read, which then interrupts main with
 *                  SIGUSR1. Never both.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/signalfd.h>
#include <sys/syscall.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static volatile sig_atomic_t handled, got_usr1;
static volatile int codes[8], vals[8], pids[8];
static volatile pid_t htid[8];
static _Atomic int done, ready;

static void on_chld(int sig, siginfo_t* info, void* context) {
  (void)sig;
  (void)context;
  int k = handled;
  if (k < 8) {
    codes[k] = info->si_code;
    vals[k] = info->si_value.sival_int;
    pids[k] = info->si_pid;
    htid[k] = (pid_t)syscall(SYS_gettid);
  }
  handled++;
}

static void on_usr1(int sig) {
  (void)sig;
  got_usr1++;
}

static void catch_chld(void) {
  struct sigaction action;
  memset(&action, 0, sizeof(action));
  action.sa_sigaction = on_chld;
  action.sa_flags = SA_SIGINFO;
  sigaction(SIGCHLD, &action, NULL);
}

static void block_chld(int how) {
  sigset_t set;
  sigemptyset(&set);
  sigaddset(&set, SIGCHLD);
  pthread_sigmask(how, &set, NULL);
}

static void msleep(long ms) {
  struct timespec delay = {ms / 1000, (ms % 1000) * 1000000L};
  nanosleep(&delay, NULL);
}

static pid_t spawn(long ms, int status) {
  pid_t child = fork();
  if (child == 0) {
    if (ms) {
      msleep(ms);
    }
    _exit(status);
  }
  return child;
}

static const char* code_name(int code) {
  return code == CLD_EXITED ? "CLD_EXITED"
      : code == SI_QUEUE    ? "SI_QUEUE"
      : code == SI_USER     ? "SI_USER"
      : code == SI_TKILL    ? "SI_TKILL"
                            : "other";
}

static void report(const char* label, pid_t c1, pid_t c2) {
  printf("%s: handled %d:", label, (int)handled);
  for (int k = 0; k < handled && k < 8; k++) {
    printf(" [%s%s%s]", code_name(codes[k]),
           codes[k] == SI_QUEUE ? (vals[k] == 7 ? " v7" : " v?") : "",
           codes[k] == CLD_EXITED ? (pids[k] == c1 ? " c1" : pids[k] == c2 ? " c2" : " c?") : "");
  }
  printf("\n");
}

static void* blocked_spinner(void* arg) {
  (void)arg;
  block_chld(SIG_BLOCK);
  while (!atomic_load(&done)) {
    syscall(SYS_getpid);
    syscall(SYS_sched_yield);
  }
  return NULL;
}

static int open_suspend;

static void* suspender(void* arg) {
  (void)arg;
  sigset_t all;
  sigfillset(&all);
  pthread_sigmask(SIG_BLOCK, &all, NULL);
  sigset_t wait;
  sigfillset(&wait);
  sigdelset(&wait, SIGUSR1);
  if (open_suspend) {
    sigdelset(&wait, SIGCHLD);
  }
  atomic_store(&ready, 1);
  while (!got_usr1) {
    sigsuspend(&wait);
  }
  return NULL;
}

static int susblk(const char* mode, int late, int sfd) {
  int pausing = !strcmp(mode, "susblk-pause");
  catch_chld();
  signal(SIGUSR1, on_usr1);
  sigset_t usr1;
  sigemptyset(&usr1);
  sigaddset(&usr1, SIGUSR1);
  pthread_sigmask(SIG_BLOCK, &usr1, NULL);
  pthread_t thread;
  pthread_create(&thread, NULL, suspender, NULL);
  while (!atomic_load(&ready)) {
    syscall(SYS_sched_yield);
  }
  msleep(50);
  if (sfd) {
    sigset_t chld;
    sigemptyset(&chld);
    sigaddset(&chld, SIGCHLD);
    signalfd(-1, &chld, 0);
  }
  pid_t me = (pid_t)syscall(SYS_gettid);
  pid_t child = spawn(late ? 100 : 0, 0);
  int ret, error;
  if (pausing) {
    ret = pause();
  } else {
    struct timespec request = {5, 0};
    ret = nanosleep(&request, NULL);
  }
  error = errno;
  int at_return = handled;
  pthread_kill(thread, SIGUSR1);
  pthread_join(thread, NULL);
  waitpid(child, NULL, 0);
  printf("%s%s%s%s: %s %s, SIGCHLD at return %d, in main %s\n", mode, late ? " late" : "",
         sfd ? " sfd" : "", open_suspend ? " open" : "",
         pausing ? "pause" : "nanosleep", ret == 0 ? "0" : error == EINTR ? "EINTR" : strerror(error),
         at_return, handled && htid[0] == me ? "yes" : "no");
  return 0;
}

static int flush(const char* mode) {
  catch_chld();
  pthread_t thread;
  pthread_create(&thread, NULL, blocked_spinner, NULL);
  block_chld(SIG_BLOCK);
  pid_t c1 = spawn(0, 3);
  msleep(200);
  signal(SIGCHLD, !strcmp(mode, "flush-ign") ? SIG_IGN : SIG_DFL);
  catch_chld();
  block_chld(SIG_UNBLOCK);
  int after_flush = handled;
  pid_t c2 = spawn(0, 3);
  struct timespec request = {5, 0};
  int ret = nanosleep(&request, NULL);
  int error = errno;
  atomic_store(&done, 1);
  pthread_join(thread, NULL);
  waitpid(c1, NULL, 0);
  waitpid(c2, NULL, 0);
  printf("%s: after flush %d, nanosleep %s, ", mode, after_flush,
         ret == 0 ? "0" : error == EINTR ? "EINTR" : "?");
  report("then", c1, c2);
  return 0;
}

static int sigqueue_order(const char* mode) {
  catch_chld();
  block_chld(SIG_BLOCK);
  union sigval value = {.sival_int = 7};
  pid_t c1;
  if (!strcmp(mode, "sq-before")) {
    sigqueue(getpid(), SIGCHLD, value);
    c1 = spawn(50, 3);
    msleep(300);
  } else {
    c1 = spawn(0, 3);
    msleep(300);
    sigqueue(getpid(), SIGCHLD, value);
  }
  block_chld(SIG_UNBLOCK);
  msleep(100);
  waitpid(c1, NULL, 0);
  msleep(100);
  report(mode, c1, 0);
  return 0;
}

static int one_child(const sigset_t* set, int status, const char* label) {
  pid_t child = spawn(0, status);
  int observed = 0;
  if (waitpid(child, &observed, 0) != child || !WIFEXITED(observed) ||
      WEXITSTATUS(observed) != status) {
    return 0;
  }
  siginfo_t info;
  memset(&info, 0, sizeof(info));
  struct timespec zero = {0, 0};
  int got = sigtimedwait(set, &info, &zero);
  int matches = got == SIGCHLD && info.si_code == CLD_EXITED && info.si_pid == child &&
      info.si_status == status;
  printf("%s matches %d status %d\n", label, matches, info.si_status);
  return matches;
}

static void take(const sigset_t* set, pid_t child);

static int efault_wait(const char* mode) {
  catch_chld();
  sigset_t set;
  sigemptyset(&set);
  sigaddset(&set, SIGCHLD);
  sigset_t blocked = set;
  sigaddset(&blocked, SIGUSR1);
  sigprocmask(SIG_BLOCK, &blocked, NULL);
  size_t page = (size_t)sysconf(_SC_PAGESIZE);
  /* A writable page followed by an inaccessible one. */
  char* pages = mmap(NULL, 2 * page, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  char* bad = pages + page;
  mprotect(bad, page, PROT_NONE);
  int tail = !strcmp(mode, "efault-sfdtail");
  int kern = !strcmp(mode, "efault-sfdkern");
  int high = !strcmp(mode, "efault-sfdhigh");
  int shared = kern || !strcmp(mode, "efault-sfdshared");
  pid_t reaped = 0;
  if (tail) {
    syscall(SYS_tgkill, getpid(), syscall(SYS_gettid), SIGUSR1);
  }
  if (shared) {
    reaped = spawn(0, 11);
    waitpid(reaped, NULL, 0);
    msleep(100);
  } else {
    syscall(SYS_tgkill, getpid(), syscall(SYS_gettid), SIGCHLD);
  }
  errno = 0;
  long result;
  if (!strcmp(mode, "efault-wait")) {
    struct timespec zero = {0, 0};
    result = sigtimedwait(&set, (siginfo_t*)bad, &zero);
  } else {
    /* For SIGCHLD alone, but for SIGUSR1 too where it reads that first. */
    int fd = signalfd(-1, tail ? &blocked : &set, SFD_NONBLOCK);
    size_t record = sizeof(struct signalfd_siginfo);
    if (!strcmp(mode, "efault-sfdv")) {
      struct iovec iov = {bad, record};
      result = readv(fd, &iov, 1);
    } else if (tail) {
      result = read(fd, bad - record, 2 * record);
    } else if (kern) {
      result = read(fd, (void*)0xffff888000000000UL, record);
    } else if (high) {
      result = read(fd, (void*)(1UL << 52), record);
    } else {
      result = read(fd, bad, record);
    }
    close(fd);
  }
  int error = errno;
  sigset_t pending;
  sigpending(&pending);
  if (high) {
    void* probe = mmap((void*)(1UL << 48), 4096, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    int five_level = probe != MAP_FAILED && (unsigned long)probe >= 1UL << 47;
    if (probe != MAP_FAILED) {
      munmap(probe, 4096);
    }
    int taken = !sigismember(&pending, SIGCHLD);
    printf("%s: EFAULT %d, as Linux does %d\n", mode, result == -1 && error == EFAULT,
           taken == five_level);
    if (!taken) {
      siginfo_t info;
      struct timespec zero = {0, 0};
      sigtimedwait(&set, &info, &zero);
    }
  } else if (tail) {
    printf("%s: read %ld signo %u, still pending %d\n", mode, result,
           ((struct signalfd_siginfo*)(bad - sizeof(struct signalfd_siginfo)))->ssi_signo,
           sigismember(&pending, SIGCHLD));
  } else {
    printf("%s: EFAULT %d, still pending %d\n", mode, result == -1 && error == EFAULT,
           sigismember(&pending, SIGCHLD));
  }
  if (kern) {
    printf("reaped child:");
    take(&set, reaped);
    printf("\n");
  }
  int first = shared || one_child(&set, 11, "first");
  int second = one_child(&set, 22, "second");
  munmap(pages, 2 * page);
  return first && second ? 0 : 81;
}

#ifndef PIDFD_SIGNAL_THREAD
#define PIDFD_SIGNAL_THREAD (1UL << 0)
#endif

static void take(const sigset_t* set, pid_t child) {
  siginfo_t info;
  memset(&info, 0, sizeof(info));
  struct timespec wait = {0, 200 * 1000 * 1000};
  int got = sigtimedwait(set, &info, &wait);
  if (got < 0) {
    printf(" [wait %s]", strerror(errno));
    return;
  }
  printf(" [%s%s%s]", code_name(info.si_code),
         info.si_code == SI_QUEUE ? (info.si_value.sival_int == 9 ? " v9" : " v?") : "",
         info.si_code == CLD_EXITED ? (info.si_pid == child && info.si_status == 11 ? " c1 11" : " c?")
                                    : "");
}

static int efin(const char* mode) {
  int private_copy = !strcmp(mode, "efin-priv");
  signal(SIGCHLD, SIG_DFL);
  sigset_t set;
  sigemptyset(&set);
  sigaddset(&set, SIGCHLD);
  sigprocmask(SIG_BLOCK, &set, NULL);
  pid_t child = spawn(0, 11);
  waitpid(child, NULL, 0);
  msleep(100);
  if (private_copy) {
    siginfo_t info;
    memset(&info, 0, sizeof(info));
    info.si_signo = SIGCHLD;
    info.si_code = SI_QUEUE;
    info.si_pid = getpid();
    info.si_uid = getuid();
    info.si_value.sival_int = 9;
    syscall(SYS_rt_tgsigqueueinfo, getpid(), syscall(SYS_gettid), SIGCHLD, &info);
  }
  void* bad = mmap(NULL, 4096, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  siginfo_t info;
  struct timespec zero = {0, 0};
  long ret = syscall(SYS_rt_sigtimedwait, bad, &info, &zero, 8);
  int error = errno;
  printf("%s: input fault %s;", mode, ret == -1 && error == EFAULT ? "EFAULT" : "other");
  take(&set, child);
  take(&set, child);
  take(&set, child);
  printf("\n");
  return 0;
}

static int pidfd_thread(void) {
  catch_chld();
  int pidfd = (int)syscall(SYS_pidfd_open, getpid(), 0);
  if (syscall(SYS_pidfd_send_signal, pidfd, SIGCHLD, NULL, PIDFD_SIGNAL_THREAD)) {
    return 3;
  }
  block_chld(SIG_BLOCK);
  pid_t child = spawn(0, 3);
  msleep(300);
  block_chld(SIG_UNBLOCK);
  msleep(100);
  waitpid(child, NULL, 0);
  msleep(100);
  report("pidfd-thread", child, 0);
  return 0;
}

static int efwide(const char* mode) {
  int private_copy = !strcmp(mode, "efwide-priv");
  signal(SIGCHLD, SIG_DFL);
  sigset_t chld;
  sigemptyset(&chld);
  sigaddset(&chld, SIGCHLD);
  sigset_t set = chld;
  sigaddset(&set, SIGUSR1);
  sigprocmask(SIG_BLOCK, &set, NULL);
  pid_t child = spawn(0, 11);
  waitpid(child, NULL, 0);
  msleep(100);
  if (private_copy) {
    siginfo_t info;
    memset(&info, 0, sizeof(info));
    info.si_signo = SIGCHLD;
    info.si_code = SI_QUEUE;
    info.si_pid = getpid();
    info.si_uid = getuid();
    info.si_value.sival_int = 9;
    if (syscall(SYS_rt_tgsigqueueinfo, getpid(), syscall(SYS_gettid), SIGCHLD, &info)) {
      return 3;
    }
    syscall(SYS_tgkill, getpid(), syscall(SYS_gettid), SIGUSR1);
  } else {
    kill(getpid(), SIGUSR1);
  }
  void* bad = mmap(NULL, 4096, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  struct timespec zero = {0, 0};
  long ret = syscall(SYS_rt_sigtimedwait, &set, bad, &zero, 8);
  int error = errno;
  sigset_t pending;
  sigpending(&pending);
  printf("%s: output fault %s, usr1 pending %d;", mode,
         ret == -1 && error == EFAULT ? "EFAULT" : "other", sigismember(&pending, SIGUSR1));
  take(&chld, child);
  take(&chld, child);
  printf("\n");
  return 0;
}

static int sfd_reader, sfd_polling;
static pthread_t main_thread;
static _Atomic int sibling_read, sibling_code, sibling_stop;

static void* signalfd_sibling(void* arg) {
  (void)arg;
  sigset_t mask;
  sigemptyset(&mask);
  sigaddset(&mask, SIGCHLD);
  sigaddset(&mask, SIGUSR1);
  sigaddset(&mask, SIGUSR2);
  pthread_sigmask(SIG_BLOCK, &mask, NULL);
  atomic_store(&ready, 1);
  struct signalfd_siginfo info;
  ssize_t n = -1;
  if (sfd_polling) {
    while (!atomic_load(&sibling_stop)) {
      n = read(sfd_reader, &info, sizeof(info));
      if (n == (ssize_t)sizeof(info)) {
        break;
      }
      syscall(SYS_sched_yield);
    }
  } else {
    n = read(sfd_reader, &info, sizeof(info));
  }
  if (n == (ssize_t)sizeof(info) && info.ssi_signo == SIGCHLD) {
    atomic_store(&sibling_code, (int)info.ssi_code);
    atomic_store(&sibling_read, 1);
    pthread_kill(main_thread, SIGUSR1);
  }
  return NULL;
}

static int sfdsib(const char* mode, int late, int rt, int poll) {
  int pausing = !strcmp(mode, "sfdsib-pause");
  main_thread = pthread_self();
  catch_chld();
  signal(SIGUSR1, on_usr1);
  sigset_t mask;
  sigemptyset(&mask);
  sigaddset(&mask, SIGCHLD);
  sigaddset(&mask, SIGUSR2);
  if (rt) {
    sigaddset(&mask, SIGRTMIN + 2);
  }
  sfd_polling = poll;
  sfd_reader = signalfd(-1, &mask, poll ? SFD_NONBLOCK : 0);
  pthread_t thread;
  pthread_create(&thread, NULL, signalfd_sibling, NULL);
  while (!atomic_load(&ready)) {
    syscall(SYS_sched_yield);
  }
  msleep(50);
  pid_t child = spawn(late ? 100 : 0, 0);
  int ret, error;
  if (pausing) {
    ret = pause();
  } else {
    struct timespec request = {5, 0};
    ret = nanosleep(&request, NULL);
  }
  error = errno;
  int in_main = handled, usr1 = got_usr1;
  if (!atomic_load(&sibling_read)) {
    if (poll) {
      atomic_store(&sibling_stop, 1);
    } else {
      pthread_kill(thread, SIGUSR2);
    }
  }
  pthread_join(thread, NULL);
  waitpid(child, NULL, 0);
  printf("%s%s%s%s: %s %s, main handler %d, usr1 %d, sibling read %d code %s\n", mode,
         late ? " late" : "", rt ? " rt" : "", poll ? " poll" : "", pausing ? "pause" : "nanosleep",
         ret == 0 ? "0" : error == EINTR ? "EINTR" : strerror(error), in_main, usr1,
         atomic_load(&sibling_read),
         atomic_load(&sibling_read) ? code_name(atomic_load(&sibling_code)) : "none");
  return 0;
}

static int pidfd_efault(void) {
  signal(SIGCHLD, SIG_DFL);
  sigset_t set;
  sigemptyset(&set);
  sigaddset(&set, SIGCHLD);
  sigprocmask(SIG_BLOCK, &set, NULL);
  pid_t child = spawn(0, 11);
  waitpid(child, NULL, 0);
  msleep(100);
  int pidfd = (int)syscall(SYS_pidfd_open, getpid(), 0);
  if (syscall(SYS_pidfd_send_signal, pidfd, SIGCHLD, NULL, PIDFD_SIGNAL_THREAD)) {
    return 3;
  }
  void* bad = mmap(NULL, 4096, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  struct timespec zero = {0, 0};
  long ret = syscall(SYS_rt_sigtimedwait, &set, bad, &zero, 8);
  int error = errno;
  printf("pidfd-efault: output fault %s;", ret == -1 && error == EFAULT ? "EFAULT" : "other");
  take(&set, child);
  take(&set, child);
  printf("\n");
  return 0;
}

int main(int argc, char** argv) {
  setvbuf(stdout, NULL, _IONBF, 0);
  const char* mode = argc > 1 ? argv[1] : "susblk-pause";
  if (!strncmp(mode, "susblk-", 7)) {
    int late = 0, sfd = 0;
    for (int i = 2; i < argc; i++) {
      late |= !strcmp(argv[i], "late");
      sfd |= !strcmp(argv[i], "sfd");
      open_suspend |= !strcmp(argv[i], "open");
    }
    return susblk(mode, late, sfd);
  }
  if (!strncmp(mode, "flush-", 6)) {
    return flush(mode);
  }
  if (!strncmp(mode, "sq-", 3)) {
    return sigqueue_order(mode);
  }
  if (!strncmp(mode, "efault-", 7)) {
    return efault_wait(mode);
  }
  if (!strncmp(mode, "efin-", 5)) {
    return efin(mode);
  }
  if (!strcmp(mode, "pidfd-thread")) {
    return pidfd_thread();
  }
  if (!strcmp(mode, "pidfd-efault")) {
    return pidfd_efault();
  }
  if (!strncmp(mode, "efwide-", 7)) {
    return efwide(mode);
  }
  if (!strncmp(mode, "sfdsib-", 7)) {
    int late = 0, rt = 0, poll = 0;
    for (int i = 2; i < argc; i++) {
      late |= !strcmp(argv[i], "late");
      rt |= !strcmp(argv[i], "rt");
      poll |= !strcmp(argv[i], "poll");
    }
    return sfdsib(mode, late, rt, poll);
  }
  return 2;
}
