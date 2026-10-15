/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/* POSIX interval timers as Linux coalesces them
 * (https://github.com/rrnewton/hermit/issues/4262): a timer's expiry is the
 * timer's own queue entry, beside any ordinary copy of the same signal; a
 * periodic timer waits for that entry's dequeue, which reports the missed
 * periods as si_overrun; an entry whose timer was set again since it was
 * queued is skipped inside the dequeue; SIG_IGN parks a periodic timer's
 * entry and a later handler re-queues it. Each mode prints one or more
 * deterministic lines; no wall-clock value is printed. Waits use nanosleep,
 * so virtual time advances under Hermit. */

#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/signalfd.h>
#include <sys/syscall.h>
#include <sys/uio.h>
#include <time.h>
#include <unistd.h>

#define MAXREC 16
#define VALUE 0x1122334455667788ULL

static volatile int n;
static volatile int rsig[MAXREC], rcode[MAXREC], rtid[MAXREC], rov[MAXREC], rpid[MAXREC];
static volatile unsigned long long rval[MAXREC];

static void handler(int s, siginfo_t *si, void *u) {
  (void)u;
  if (n < MAXREC) {
    rsig[n] = s;
    rcode[n] = si->si_code;
    if (si->si_code == SI_TIMER) {
      rtid[n] = si->si_timerid;
      rov[n] = si->si_overrun;
      rpid[n] = 0;
    } else {
      rtid[n] = -1;
      rov[n] = -1;
      rpid[n] = si->si_pid;
    }
    rval[n] = (unsigned long long)(uintptr_t)si->si_value.sival_ptr;
  }
  n++;
}

static void install(int sig, int how) {
  struct sigaction a;
  memset(&a, 0, sizeof a);
  if (how == 0) {
    a.sa_sigaction = handler;
    a.sa_flags = SA_SIGINFO;
  } else {
    a.sa_handler = how == 1 ? SIG_IGN : SIG_DFL;
  }
  if (sigaction(sig, &a, NULL)) {
    perror("sigaction");
    exit(2);
  }
}

static void mask(int how, int sig) {
  sigset_t s;
  sigemptyset(&s);
  sigaddset(&s, sig);
  sigprocmask(how, &s, NULL);
}

static void sleep_us(long us) {
  struct timespec s = {us / 1000000, us % 1000000 * 1000};
  while (nanosleep(&s, &s) && errno == EINTR) {
  }
}

/* A timer notifying `sig` with `value`; to thread `tid` when nonzero. */
static timer_t mk(int sig, unsigned long long value, int tid) {
  timer_t t;
  struct sigevent e;
  memset(&e, 0, sizeof e);
  e.sigev_signo = sig;
  e.sigev_value.sival_ptr = (void *)(uintptr_t)value;
  if (tid) {
    e.sigev_notify = SIGEV_THREAD_ID;
    e._sigev_un._tid = tid;
  } else {
    e.sigev_notify = SIGEV_SIGNAL;
  }
  if (timer_create(CLOCK_MONOTONIC, &e, &t)) {
    perror("timer_create");
    exit(2);
  }
  return t;
}

static void arm(timer_t t, long value_us, long interval_us) {
  struct itimerspec s = {{interval_us / 1000000, interval_us % 1000000 * 1000},
                         {value_us / 1000000, value_us % 1000000 * 1000}};
  if (timer_settime(t, 0, &s, NULL)) {
    perror("timer_settime");
    exit(2);
  }
}

static int pending(int sig) {
  sigset_t p;
  sigpending(&p);
  return sigismember(&p, sig);
}

/* One zero-timeout sigtimedwait for `sig`: its return, and the siginfo. */
static int take(int sig, siginfo_t *si) {
  sigset_t w;
  sigemptyset(&w);
  sigaddset(&w, sig);
  struct timespec z = {0, 0};
  memset(si, 0, sizeof *si);
  return sigtimedwait(&w, si, &z);
}

static void print_take(const char *label, int r, siginfo_t *si) {
  if (r < 0) {
    printf("%s=%s\n", label, errno == EAGAIN ? "EAGAIN" : strerror(errno));
  } else if (si->si_code == SI_TIMER) {
    printf("%s=%d code=SI_TIMER timer=%d overrun=%d value=%#llx\n", label, r, si->si_timerid,
           si->si_overrun, (unsigned long long)(uintptr_t)si->si_value.sival_ptr);
  } else {
    printf("%s=%d code=%d own_pid=%d value=%#llx\n", label, r, si->si_code,
           si->si_pid == getpid(), (unsigned long long)(uintptr_t)si->si_value.sival_ptr);
  }
}

static void print_handled(const char *mode) {
  printf("%s deliveries=%d\n", mode, n);
  for (int i = 0; i < n && i < MAXREC; i++) {
    if (rcode[i] == SI_TIMER) {
      printf("  %d sig=%d SI_TIMER timer=%d overrun=%d value=%#llx\n", i, rsig[i], rtid[i], rov[i],
             rval[i]);
    } else {
      printf("  %d sig=%d code=%d own_pid=%d value=%#llx\n", i, rsig[i], rcode[i],
             rpid[i] == getpid(), rval[i]);
    }
  }
}

static void print_records(const char *mode, struct signalfd_siginfo *rec, ssize_t got) {
  if (got < 0) {
    printf("%s read=%s\n", mode, errno == EAGAIN ? "EAGAIN" : strerror(errno));
    return;
  }
  printf("%s records=%zd\n", mode, got / (ssize_t)sizeof rec[0]);
  for (ssize_t i = 0; i < got / (ssize_t)sizeof rec[0]; i++) {
    if (rec[i].ssi_code == SI_TIMER) {
      printf("  %zd sig=%u SI_TIMER timer=%u overrun=%u int=%#x ptr=%#llx\n", i, rec[i].ssi_signo,
             rec[i].ssi_tid, rec[i].ssi_overrun, rec[i].ssi_int,
             (unsigned long long)rec[i].ssi_ptr);
    } else {
      printf("  %zd sig=%u code=%d own_pid=%d ptr=%#llx\n", i, rec[i].ssi_signo, rec[i].ssi_code,
             rec[i].ssi_pid == (unsigned)getpid(), (unsigned long long)rec[i].ssi_ptr);
    }
  }
}

static int sfd_for(int a, int b) {
  sigset_t w;
  sigemptyset(&w);
  sigaddset(&w, a);
  if (b) sigaddset(&w, b);
  int fd = signalfd(-1, &w, SFD_NONBLOCK);
  if (fd < 0) {
    perror("signalfd");
    exit(2);
  }
  return fd;
}

static void sfd_read(const char *mode, int a, int b, int room) {
  struct signalfd_siginfo rec[8];
  memset(rec, 0, sizeof rec);
  int fd = sfd_for(a, b);
  ssize_t got = read(fd, rec, (size_t)room * sizeof rec[0]);
  print_records(mode, rec, got);
  close(fd);
}

static int gettid_(void) { return (int)syscall(SYS_gettid); }

static void invalidate(timer_t t, const char *how) {
  if (!strcmp(how, "far")) {
    arm(t, 100 * 1000000L, 0);
  } else if (!strcmp(how, "disarm")) {
    arm(t, 0, 0);
  } else {
    timer_delete(t);
  }
}

/* T2: blocked for 5.2 periods of a 1 ms timer. Every wait that decides an
 * overrun ends 0.2 periods past a grid point: Hermit's sleeps end about
 * 0.55 ms past their deadline, Linux's about 0.06 ms, and both must land in
 * the same period. */
static void blocked(void) {
  install(SIGUSR1, 0);
  mask(SIG_BLOCK, SIGUSR1);
  timer_t t = mk(SIGUSR1, VALUE, 0);
  arm(t, 1000, 1000);
  sleep_us(5200);
  struct itimerspec c;
  timer_gettime(t, &c);
  long rem = c.it_value.tv_sec * 1000000L + c.it_value.tv_nsec / 1000;
  int before = timer_getoverrun(t);
  mask(SIG_UNBLOCK, SIGUSR1);
  int after = timer_getoverrun(t);
  timer_delete(t);
  printf("blocked waiting_remaining_within_interval=%d getoverrun_before=%d getoverrun_after=%d\n",
         rem > 0 && rem <= 1000, before, after);
  print_handled("blocked");
}

/* T3: a one-shot timer's entry reaches each consumer with every _timer field. */
static void consumer(const char *how) {
  install(SIGUSR1, 0);
  mask(SIG_BLOCK, SIGUSR1);
  timer_t dummy = mk(SIGUSR1, 0, 0); /* so the timer of interest is id 1 */
  timer_t t = mk(SIGUSR1, VALUE, 0);
  (void)dummy;
  arm(t, 1000, 0);
  sleep_us(2000);
  if (!strcmp(how, "handler")) {
    mask(SIG_UNBLOCK, SIGUSR1);
    print_handled("consumer_handler");
  } else if (!strcmp(how, "wait")) {
    siginfo_t si;
    int r = take(SIGUSR1, &si);
    print_take("consumer_wait", r, &si);
  } else {
    sfd_read("consumer_sfd", SIGUSR1, 0, 1);
  }
}

/* T4: SIG_IGN for 3.2 periods, then a handler: the parked entry is re-queued
 * and delivered at once, with the periods missed while parked. */
static void parked(void) {
  install(SIGUSR1, 1);
  timer_t t = mk(SIGUSR1, VALUE, 0);
  arm(t, 1000, 1000);
  sleep_us(3200);
  install(SIGUSR1, 0);
  int at_install = n;
  timer_delete(t);
  printf("parked at_install=%d\n", at_install);
  print_handled("parked");
}

/* T5: queued while blocked, flushed by SIG_IGN (parked), re-queued by a
 * handler, then delivered at the unblock with the accumulated overrun. */
static void flush(void) {
  install(SIGUSR1, 0);
  mask(SIG_BLOCK, SIGUSR1);
  timer_t t = mk(SIGUSR1, VALUE, 0);
  arm(t, 1000, 1000);
  sleep_us(2200);
  install(SIGUSR1, 1);
  install(SIGUSR1, 0);
  sleep_us(1000);
  mask(SIG_UNBLOCK, SIGUSR1);
  timer_delete(t);
  print_handled("flush");
}

/* T6: a queued entry made stale reaches no consumer. */
static void stale(const char *inv, const char *how) {
  char label[64];
  snprintf(label, sizeof label, "stale_%s_%s", inv, how);
  install(SIGUSR1, 0);
  mask(SIG_BLOCK, SIGUSR1);
  timer_t t = mk(SIGUSR1, VALUE, 0);
  arm(t, 1000, 0);
  sleep_us(2000);
  invalidate(t, inv);
  printf("%s pending=%d\n", label, pending(SIGUSR1));
  if (!strcmp(how, "handler")) {
    mask(SIG_UNBLOCK, SIGUSR1);
    sleep_us(1000);
    print_handled(label);
  } else if (!strcmp(how, "wait")) {
    siginfo_t si;
    int r = take(SIGUSR1, &si);
    print_take(label, r, &si);
  } else {
    sfd_read(label, SIGUSR1, 0, 2);
  }
}

/* T6's positive row: re-armed near while queued, the new expiry refreshes the
 * queued entry, which is then delivered once. */
static void stale_rearm_near(void) {
  install(SIGUSR1, 0);
  mask(SIG_BLOCK, SIGUSR1);
  timer_t t = mk(SIGUSR1, VALUE, 0);
  arm(t, 1000, 0);
  sleep_us(2000);
  arm(t, 1000, 0);
  sleep_us(2000);
  mask(SIG_UNBLOCK, SIGUSR1);
  print_handled("stale_rearm_near");
}

/* T7: a kill and a timer's expiry of one signal, both pending. */
static void merged(const char *how) {
  char label[64];
  snprintf(label, sizeof label, "merged_%s", how);
  install(SIGUSR1, 0);
  mask(SIG_BLOCK, SIGUSR1);
  kill(getpid(), SIGUSR1);
  timer_t t = mk(SIGUSR1, VALUE, 0);
  arm(t, 1000, 0);
  sleep_us(2000);
  if (!strcmp(how, "handler")) {
    mask(SIG_UNBLOCK, SIGUSR1);
    print_handled(label);
  } else if (!strcmp(how, "wait")) {
    siginfo_t si;
    for (int i = 0; i < 3; i++) {
      int r = take(SIGUSR1, &si);
      print_take(label, r, &si);
    }
  } else if (!strcmp(how, "sfd2")) {
    sfd_read(label, SIGUSR1, 0, 2);
  } else if (!strcmp(how, "sfd3")) {
    sfd_read(label, SIGUSR1, 0, 3);
  } else {
    /* readv into two 128-byte extents */
    struct signalfd_siginfo rec[2];
    memset(rec, 0, sizeof rec);
    struct iovec v[2] = {{&rec[0], sizeof rec[0]}, {&rec[1], sizeof rec[1]}};
    int fd = sfd_for(SIGUSR1, 0);
    ssize_t got = readv(fd, v, 2);
    print_records(label, rec, got);
  }
}

/* T7b: two one-shot USR1 timers (values 1, 2) and a USR2 kill, read by one
 * signalfd read with room for 4, in the shared or the private queue. */
static void order(int private) {
  install(SIGUSR1, 0);
  install(SIGUSR2, 0);
  mask(SIG_BLOCK, SIGUSR1);
  mask(SIG_BLOCK, SIGUSR2);
  int tid = private ? gettid_() : 0;
  timer_t a = mk(SIGUSR1, 1, tid);
  timer_t b = mk(SIGUSR1, 2, tid);
  arm(a, 1000, 0);
  arm(b, 1500, 0);
  if (private) {
    syscall(SYS_tgkill, getpid(), gettid_(), SIGUSR2);
  } else {
    kill(getpid(), SIGUSR2);
  }
  sleep_us(3000);
  sfd_read(private ? "order_private" : "order_shared", SIGUSR1, SIGUSR2, 4);
}

/* A1: the order of an ordinary send and a timer's expiry. */
static void a1(const char *how) {
  char label[64];
  snprintf(label, sizeof label, "a1_%s", how);
  install(SIGUSR1, 0);
  mask(SIG_BLOCK, SIGUSR1);
  if (!strcmp(how, "timer_then_kill")) {
    timer_t t = mk(SIGUSR1, VALUE, 0);
    arm(t, 1000, 0);
    sleep_us(2000);
    kill(getpid(), SIGUSR1);
  } else if (!strcmp(how, "kill_then_timer")) {
    kill(getpid(), SIGUSR1);
    timer_t t = mk(SIGUSR1, VALUE, 0);
    arm(t, 1000, 0);
    sleep_us(2000);
  } else if (!strcmp(how, "two_kills")) {
    kill(getpid(), SIGUSR1);
    kill(getpid(), SIGUSR1);
  } else {
    /* stale_<inv>_then_kill */
    char inv[16] = {0};
    sscanf(how, "stale_%15[a-z]", inv);
    timer_t t = mk(SIGUSR1, VALUE, 0);
    arm(t, 1000, 0);
    sleep_us(2000);
    invalidate(t, inv);
    printf("%s pending_before_kill=%d\n", label, pending(SIGUSR1));
    kill(getpid(), SIGUSR1);
  }
  mask(SIG_UNBLOCK, SIGUSR1);
  sleep_us(1000);
  print_handled(label);
}

/* A stale entry drained first, then a kill. */
static void drain_then_kill(const char *inv) {
  char label[64];
  snprintf(label, sizeof label, "drain_%s_then_kill", inv);
  mask(SIG_BLOCK, SIGUSR1);
  timer_t t = mk(SIGUSR1, VALUE, 0);
  arm(t, 1000, 0);
  sleep_us(2000);
  invalidate(t, inv);
  siginfo_t si;
  int r = take(SIGUSR1, &si);
  print_take(label, r, &si);
  kill(getpid(), SIGUSR1);
  r = take(SIGUSR1, &si);
  print_take(label, r, &si);
}

/* Unignore (R3-3, R4-3, R5-3, R5-4, R5-5). */
static void unignore(const char *how) {
  char label[64];
  snprintf(label, sizeof label, "unignore_%s", how);
  if (!strcmp(how, "far") || !strcmp(how, "near") || !strcmp(how, "current") ||
      !strcmp(how, "deleted")) {
    install(SIGUSR1, 1);
    timer_t t = mk(SIGUSR1, VALUE, 0);
    arm(t, 1000, 1000);
    sleep_us(!strcmp(how, "current") ? 10200 : 3500);
    if (!strcmp(how, "far")) {
      arm(t, 100 * 1000000L, 0);
      install(SIGUSR1, 0);
      printf("%s pending_before_kill=%d\n", label, pending(SIGUSR1));
      kill(getpid(), SIGUSR1);
      sleep_us(1000);
    } else if (!strcmp(how, "near")) {
      arm(t, 1000, 0);
      mask(SIG_BLOCK, SIGUSR1);
      install(SIGUSR1, 0);
      sleep_us(2000);
      mask(SIG_UNBLOCK, SIGUSR1);
    } else if (!strcmp(how, "current")) {
      install(SIGUSR1, 0);
      timer_delete(t);
    } else {
      timer_delete(t);
      install(SIGUSR1, 0);
      kill(getpid(), SIGUSR1);
    }
    print_handled(label);
    return;
  }
  if (!strcmp(how, "high_first") || !strcmp(how, "low_first")) {
    install(SIGUSR1, 1);
    timer_t t0 = mk(SIGUSR1, 0, 0);
    timer_t t1 = mk(SIGUSR1, 1, 0);
    int high_first = !strcmp(how, "high_first");
    arm(high_first ? t1 : t0, 1000, 10000);
    arm(high_first ? t0 : t1, 1500, 10000);
    sleep_us(2000);
    mask(SIG_BLOCK, SIGUSR1);
    install(SIGUSR1, 0);
    mask(SIG_UNBLOCK, SIGUSR1);
    timer_delete(t0);
    timer_delete(t1);
    print_handled(label);
    return;
  }
  if (!strcmp(how, "flush_order")) {
    install(SIGUSR1, 0);
    mask(SIG_BLOCK, SIGUSR1);
    timer_t a = mk(SIGUSR1, 0, 0);
    timer_t b = mk(SIGUSR1, 1, 0);
    arm(a, 1000, 10000);
    arm(b, 1500, 10000);
    sleep_us(2000);
    install(SIGUSR1, 1);
    install(SIGUSR1, 0);
    mask(SIG_UNBLOCK, SIGUSR1);
    timer_delete(a);
    timer_delete(b);
    print_handled(label);
    return;
  }
  if (!strcmp(how, "oneshot_rearmed_periodic")) {
    install(SIGUSR1, 0);
    mask(SIG_BLOCK, SIGUSR1);
    timer_t t = mk(SIGUSR1, VALUE, 0);
    arm(t, 1000, 0);
    sleep_us(2000);
    arm(t, 100 * 1000000L, 100 * 1000000L);
    install(SIGUSR1, 1);
    install(SIGUSR1, 0);
    kill(getpid(), SIGUSR1);
    mask(SIG_UNBLOCK, SIGUSR1);
    timer_delete(t);
    print_handled(label);
    return;
  }
  if (!strcmp(how, "periodic_rearmed_oneshot")) {
    install(SIGUSR1, 0);
    mask(SIG_BLOCK, SIGUSR1);
    timer_t t = mk(SIGUSR1, VALUE, 0);
    arm(t, 1000, 1000);
    sleep_us(1500);
    arm(t, 100 * 1000000L, 0);
    install(SIGUSR1, 1);
    install(SIGUSR1, 0);
    printf("%s pending_before_kill=%d\n", label, pending(SIGUSR1));
    kill(getpid(), SIGUSR1);
    mask(SIG_UNBLOCK, SIGUSR1);
    timer_delete(t);
    print_handled(label);
    return;
  }
  fprintf(stderr, "unknown unignore mode %s\n", how);
  exit(2);
}

/* Rev 7 C: every expiry re-latches the queued entry's periodicity, which
 * decides whether a flush parks it. */
static void refresh(int oneshot_first) {
  const char *label = oneshot_first ? "ref_os_then_p" : "ref_p_then_os";
  mask(SIG_BLOCK, SIGUSR1);
  timer_t t = mk(SIGUSR1, VALUE, 0);
  arm(t, 1000, oneshot_first ? 0 : 1000);
  sleep_us(oneshot_first ? 2000 : 1500);
  arm(t, 1000, oneshot_first ? 10000 : 0);
  sleep_us(1500);
  install(SIGUSR1, 1);
  kill(getpid(), SIGUSR1);
  siginfo_t si;
  int r = take(SIGUSR1, &si);
  print_take(label, r, &si);
  install(SIGUSR1, 0);
  r = take(SIGUSR1, &si);
  print_take(label, r, &si);
  timer_delete(t);
  kill(getpid(), SIGUSR1);
  r = take(SIGUSR1, &si);
  print_take(label, r, &si);
}

/* si_value with no sigevent is the kernel timer id (`do_timer_create`).
 * glibc's timer_create passes a sigevent of its own, so the raw call is
 * used. */
static void sival_null(void) {
  mask(SIG_BLOCK, SIGALRM);
  timer_t first = mk(SIGALRM, 5, 0);
  (void)first;
  int id = -1;
  if (syscall(SYS_timer_create, CLOCK_MONOTONIC, NULL, &id)) {
    perror("timer_create");
    exit(2);
  }
  struct itimerspec s = {{0, 0}, {0, 1000000}};
  if (syscall(SYS_timer_settime, id, 0, &s, NULL)) {
    perror("timer_settime");
    exit(2);
  }
  sleep_us(2000);
  siginfo_t si;
  int r = take(SIGALRM, &si);
  printf("sival_null take=%d SI_TIMER=%d timer=%d int_is_timer_id=%d\n", r, si.si_code == SI_TIMER,
         si.si_timerid, si.si_value.sival_int == id && si.si_timerid == id);
}

/* timer_settime resets timer_getoverrun. */
static void settime_resets(void) {
  install(SIGUSR1, 0);
  mask(SIG_BLOCK, SIGUSR1);
  timer_t t = mk(SIGUSR1, VALUE, 0);
  arm(t, 1000, 1000);
  sleep_us(3200);
  mask(SIG_UNBLOCK, SIGUSR1);
  int after_delivery = timer_getoverrun(t);
  arm(t, 100 * 1000000L, 0);
  int after_set = timer_getoverrun(t);
  timer_delete(t);
  printf("settime_resets after_delivery=%d after_set=%d deliveries=%d\n", after_delivery, after_set,
         n);
}

/* timer_gettime's classes: armed and not due; a popped one-shot reads 0. */
static void gettime(void) {
  mask(SIG_BLOCK, SIGUSR1);
  timer_t t = mk(SIGUSR1, VALUE, 0);
  arm(t, 1000, 0);
  struct itimerspec c;
  timer_gettime(t, &c);
  long armed = c.it_value.tv_sec * 1000000L + c.it_value.tv_nsec / 1000;
  sleep_us(2000);
  timer_gettime(t, &c);
  printf("gettime armed_within=%d popped_zero=%d popped_interval_zero=%d\n",
         armed > 0 && armed <= 1000, c.it_value.tv_sec == 0 && c.it_value.tv_nsec == 0,
         c.it_interval.tv_sec == 0 && c.it_interval.tv_nsec == 0);
}

/* J (Claude rev 9): a kept SI_QUEUE send standing on a discarded forged
 * SI_TIMER copy in a parked worker's private queue. */
static volatile int wtid, htid;
static int futex_word;
static long worker_futex;
static void forged_handler(int s, siginfo_t *si, void *u) {
  handler(s, si, u);
  htid = gettid_();
}
static void *forged_worker(void *a) {
  (void)a;
  wtid = gettid_();
  struct timespec ts = {0, 500000000L};
  worker_futex = syscall(SYS_futex, &futex_word, 128 /* FUTEX_WAIT_PRIVATE */, 0, &ts, 0, 0);
  return 0;
}
static void forged(int timer, int use_tgkill) {
  if (timer) {
    timer_t t = mk(SIGUSR1, 0, 0);
    arm(t, 3600 * 1000000L, 3600 * 1000000L);
    arm(t, 0, 0);
  }
  install(SIGUSR1, 1);
  mask(SIG_BLOCK, SIGUSR1);
  mask(SIG_UNBLOCK, SIGUSR1);
  pthread_t th;
  pthread_create(&th, 0, forged_worker, 0);
  mask(SIG_BLOCK, SIGUSR1);
  while (!wtid) sleep_us(1000);
  sleep_us(50000);
  siginfo_t si;
  memset(&si, 0, sizeof si);
  si.si_signo = SIGUSR1;
  si.si_code = SI_TIMER;
  si.si_value.sival_int = 7;
  if (syscall(SYS_rt_tgsigqueueinfo, getpid(), wtid, SIGUSR1, &si)) perror("forge");
  struct sigaction sa;
  memset(&sa, 0, sizeof sa);
  sa.sa_sigaction = forged_handler;
  sa.sa_flags = SA_SIGINFO;
  sigaction(SIGUSR1, &sa, 0);
  if (use_tgkill) {
    syscall(SYS_tgkill, getpid(), wtid, SIGUSR1);
  } else {
    memset(&si, 0, sizeof si);
    si.si_signo = SIGUSR1;
    si.si_code = SI_QUEUE;
    si.si_value.sival_ptr = (void *)(uintptr_t)VALUE;
    if (syscall(SYS_rt_tgsigqueueinfo, getpid(), wtid, SIGUSR1, &si)) perror("queue");
  }
  pthread_join(th, 0);
  printf("forged_%s%s on_worker=%d\n", timer ? "timer" : "notimer", use_tgkill ? "_tgkill" : "",
         htid == wtid);
  print_handled("forged");
}

/* A kept SI_QUEUE send standing on a forged SI_TIMER copy, across exec. */
static void forged_exec(char *self, int timer) {
  if (timer) {
    timer_t t = mk(SIGUSR1, 0, 0);
    arm(t, 3600 * 1000000L, 3600 * 1000000L);
    arm(t, 0, 0);
  }
  install(SIGUSR1, 1);
  siginfo_t si;
  memset(&si, 0, sizeof si);
  si.si_signo = SIGUSR1;
  si.si_code = SI_TIMER;
  si.si_value.sival_int = 7;
  if (syscall(SYS_rt_tgsigqueueinfo, getpid(), gettid_(), SIGUSR1, &si)) perror("forge");
  install(SIGUSR1, 0);
  mask(SIG_BLOCK, SIGUSR1);
  memset(&si, 0, sizeof si);
  si.si_signo = SIGUSR1;
  si.si_code = SI_QUEUE;
  si.si_value.sival_ptr = (void *)(uintptr_t)VALUE;
  if (syscall(SYS_rt_tgsigqueueinfo, getpid(), gettid_(), SIGUSR1, &si)) perror("queue");
  char *argv[] = {self, (char *)"forged_exec_child", NULL};
  execv(self, argv);
  perror("execv");
  exit(2);
}

static void forged_exec_child(void) {
  printf("forged_exec pending_after_exec=%d\n", pending(SIGUSR1));
  install(SIGUSR1, 0);
  mask(SIG_UNBLOCK, SIGUSR1);
  print_handled("forged_exec");
}

int main(int argc, char **argv) {
  if (argc < 2) {
    fprintf(stderr, "usage: %s <mode>\n", argv[0]);
    return 2;
  }
  const char *m = argv[1];
  char a[32] = {0}, b[32] = {0};
  setvbuf(stdout, NULL, _IONBF, 0);
  if (!strcmp(m, "blocked")) blocked();
  else if (sscanf(m, "consumer_%31s", a) == 1) consumer(a);
  else if (!strcmp(m, "parked")) parked();
  else if (!strcmp(m, "flush")) flush();
  else if (!strcmp(m, "stale_rearm_near")) stale_rearm_near();
  else if (sscanf(m, "stale_%31[a-z]_%31s", a, b) == 2) stale(a, b);
  else if (sscanf(m, "merged_%31s", a) == 1) merged(a);
  else if (!strcmp(m, "order_shared")) order(0);
  else if (!strcmp(m, "order_private")) order(1);
  else if (sscanf(m, "a1_%31s", a) == 1) a1(a);
  else if (sscanf(m, "drain_%31[a-z]_then_kill", a) == 1) drain_then_kill(a);
  else if (sscanf(m, "unignore_%31s", a) == 1) unignore(a);
  else if (!strcmp(m, "ref_os_then_p")) refresh(1);
  else if (!strcmp(m, "ref_p_then_os")) refresh(0);
  else if (!strcmp(m, "sival_null")) sival_null();
  else if (!strcmp(m, "settime_resets")) settime_resets();
  else if (!strcmp(m, "gettime")) gettime();
  else if (!strcmp(m, "forged_timer")) forged(1, 0);
  else if (!strcmp(m, "forged_notimer")) forged(0, 0);
  else if (!strcmp(m, "forged_timer_tgkill")) forged(1, 1);
  else if (!strcmp(m, "forged_exec_timer")) forged_exec(argv[0], 1);
  else if (!strcmp(m, "forged_exec_notimer")) forged_exec(argv[0], 0);
  else if (!strcmp(m, "forged_exec_child")) forged_exec_child();
  else {
    fprintf(stderr, "unknown mode %s\n", m);
    return 2;
  }
  return 0;
}
