/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/* POSIX timer entries beside other signals and consumers
 * (https://github.com/rrnewton/hermit/issues/4262), the rows
 * tests/c/timer_coalesce.c does not cover: the recurrence of a storm, the raw
 * siginfo of an entry standing on a discarded copy, which signal a take
 * selects first, a wait ended past a stale head, rt_sigsuspend transitions, a
 * faulted take, the signals whose timers take the fallback, timer-free
 * controls, a non-leader exec, and the shared-queue forged form. Each mode
 * prints deterministic lines; no wall-clock value is printed. Waits that
 * decide an overrun end 0.2 periods past a grid point (see
 * tests/c/timer_coalesce.c). */

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <pthread.h>
#include <sched.h>
#include <spawn.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdint.h>
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

#define MAXREC 16
#define VALUE 0x1122334455667788ULL

static volatile int n;
static volatile int rsig[MAXREC], rcode[MAXREC], rtid[MAXREC], rov[MAXREC], rpid[MAXREC];
static volatile int rthread[MAXREC];
static volatile unsigned long long rval[MAXREC];
static unsigned char rraw[MAXREC][48];

static int gettid_(void) { return (int)syscall(SYS_gettid); }

static void handler(int s, siginfo_t *si, void *u) {
  (void)u;
  if (n < MAXREC) {
    rsig[n] = s;
    rcode[n] = si->si_code;
    rthread[n] = gettid_();
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
    memcpy(rraw[n], si, 48);
  }
  n++;
}

static void install_flags(int sig, int how, int flags) {
  struct sigaction a;
  memset(&a, 0, sizeof a);
  if (how == 0) {
    a.sa_sigaction = handler;
    a.sa_flags = SA_SIGINFO | flags;
  } else {
    a.sa_handler = how == 1 ? SIG_IGN : SIG_DFL;
  }
  if (sigaction(sig, &a, NULL)) {
    perror("sigaction");
    exit(2);
  }
}

static void install(int sig, int how) { install_flags(sig, how, 0); }

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

static long now_ns(void) {
  struct timespec t;
  clock_gettime(CLOCK_MONOTONIC, &t);
  return t.tv_sec * 1000000000L + t.tv_nsec;
}

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

static int take_set(sigset_t *w, siginfo_t *si) {
  struct timespec z = {0, 0};
  if (si) memset(si, 0, sizeof *si);
  return sigtimedwait(w, si, &z);
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
      printf("  %zd sig=%u SI_TIMER timer=%u overrun=%u ptr=%#llx\n", i, rec[i].ssi_signo,
             rec[i].ssi_tid, rec[i].ssi_overrun, (unsigned long long)rec[i].ssi_ptr);
    } else {
      printf("  %zd sig=%u code=%d own_pid=%d ptr=%#llx\n", i, rec[i].ssi_signo, rec[i].ssi_code,
             rec[i].ssi_pid == (unsigned)getpid(), (unsigned long long)rec[i].ssi_ptr);
    }
  }
}

static void set2(sigset_t *w, int a, int b) {
  sigemptyset(w);
  sigaddset(w, a);
  if (b) sigaddset(w, b);
}

/* T1: a 1 ms timer during a syscall-free loop. Each delivery re-arms the
 * timer on its grid, so the deliveries plus their overruns count the grid
 * points up to the last dequeue: between the grid points before the clock
 * read just ahead of the last handler and those up to the read inside it. */
/* The storm's handler keeps its own count and sum, with no MAXREC cap: how
 * many deliveries a native run takes depends on the host's speed. */
static volatile long storm_loop, storm_last, storm_sum;
static volatile int storm_n, storm_all_si_timer = 1;
static void storm_handler(int s, siginfo_t *si, void *u) {
  (void)s;
  (void)u;
  storm_last = now_ns();
  storm_n++;
  if (si->si_code == SI_TIMER)
    storm_sum += 1 + si->si_overrun;
  else
    storm_all_si_timer = 0;
}
static void storm(void) {
  struct sigaction a;
  memset(&a, 0, sizeof a);
  a.sa_sigaction = storm_handler;
  a.sa_flags = SA_SIGINFO;
  sigaction(SIGUSR1, &a, NULL);
  timer_t t = mk(SIGUSR1, VALUE, 0);
  long start = now_ns();
  arm(t, 1000, 1000);
  long armed = now_ns();
  for (long i = 0; i < 30000000L; i++) storm_loop = i;
  mask(SIG_BLOCK, SIGUSR1);
  timer_delete(t);
  int ok = 0;
  if (storm_n > 0 && storm_all_si_timer) {
    /* Grid points start + k ms (k >= 1) up to the last handler's entry; the
     * timer was armed between `start` and `armed`. */
    long lo = (storm_last - armed) / 1000000L, hi = (storm_last - start) / 1000000L;
    ok = storm_sum >= lo && storm_sum <= hi + 1;
  }
  printf("storm deliveries_positive=%d all_si_timer=%d recurrence_ok=%d\n", storm_n > 0,
         storm_n > 0 && storm_all_si_timer, ok);
}

/* D1: a copy discarded while ignored (SI_QUEUE, nonzero bytes 32-47) waits in
 * a parked worker's queue under ptrace; a handler is installed and the
 * worker's SIGEV_THREAD_ID timer expires on top of it. The worker's handler
 * must see exactly the timer's own record, bytes 0-47. */
static volatile int wtid;
static int futex_word;
static void *parked_worker(void *a) {
  long ms = (long)(intptr_t)a;
  wtid = gettid_();
  struct timespec ts = {ms / 1000, ms % 1000 * 1000000L};
  syscall(SYS_futex, &futex_word, 128 /* FUTEX_WAIT_PRIVATE */, 0, &ts, 0, 0);
  return 0;
}
static int raw_matches_timer(int i, int timer, unsigned long long value) {
  unsigned char want[48];
  memset(want, 0, sizeof want);
  int sig = SIGUSR1, code = SI_TIMER, ov = 0;
  memcpy(want + 0, &sig, 4);
  memcpy(want + 8, &code, 4);
  memcpy(want + 16, &timer, 4);
  memcpy(want + 20, &ov, 4);
  memcpy(want + 24, &value, 8);
  return memcmp(rraw[i], want, 48) == 0;
}
static void d1(void) {
  install(SIGUSR1, 1);
  pthread_t th;
  pthread_create(&th, 0, parked_worker, (void *)(intptr_t)300);
  mask(SIG_BLOCK, SIGUSR1);
  while (!wtid) sleep_us(1000);
  sleep_us(50000);
  siginfo_t si;
  memset(&si, 0, sizeof si);
  si.si_signo = SIGUSR1;
  si.si_code = SI_QUEUE;
  si.si_value.sival_int = 5;
  memset((char *)&si + 32, 0xab, 16);
  if (syscall(SYS_rt_tgsigqueueinfo, getpid(), wtid, SIGUSR1, &si)) perror("discarded");
  install(SIGUSR1, 0);
  timer_t t = mk(SIGUSR1, VALUE, wtid);
  arm(t, 1000, 0);
  pthread_join(th, 0);
  printf("d1 deliveries=%d on_worker=%d raw_is_the_timers_record=%d\n", n, n > 0 && rthread[0] == wtid,
         n == 1 && raw_matches_timer(0, 0, VALUE));
  print_handled("d1");
}

/* D2: the same discarded copy with no timer: the worker's dequeue of it
 * delivers nothing, and a later kill to the worker is delivered once. */
static void d2(void) {
  install(SIGUSR1, 1);
  pthread_t th;
  pthread_create(&th, 0, parked_worker, (void *)(intptr_t)200);
  mask(SIG_BLOCK, SIGUSR1);
  while (!wtid) sleep_us(1000);
  sleep_us(50000);
  siginfo_t si;
  memset(&si, 0, sizeof si);
  si.si_signo = SIGUSR1;
  si.si_code = SI_QUEUE;
  if (syscall(SYS_rt_tgsigqueueinfo, getpid(), wtid, SIGUSR1, &si)) perror("discarded");
  install(SIGUSR1, 0);
  pthread_join(th, 0);
  printf("d2 deliveries=%d\n", n);
}

/* R4-1 and R5-1: a signal the lists do not hold, taken before a shared USR1
 * timer's entry, which stays for the next take. */
static void selection(const char *how) {
  char label[64];
  install(SIGUSR1, 0);
  install(SIGUSR2, 0);
  install(SIGHUP, 0);
  sigset_t all;
  sigfillset(&all);
  sigprocmask(SIG_BLOCK, &all, NULL);
  timer_t t = mk(SIGUSR1, VALUE, 0);
  arm(t, 1000, 0);
  sleep_us(2000);
  int other = SIGUSR2;
  if (!strcmp(how, "hup")) {
    other = SIGHUP;
    kill(getpid(), SIGHUP);
  } else if (!strcmp(how, "rtmin")) {
    other = SIGRTMIN;
    siginfo_t q;
    memset(&q, 0, sizeof q);
    q.si_signo = SIGRTMIN;
    q.si_code = SI_QUEUE;
    q.si_value.sival_int = 21;
    syscall(SYS_rt_tgsigqueueinfo, getpid(), gettid_(), SIGRTMIN, &q);
  } else {
    syscall(SYS_tgkill, getpid(), gettid_(), SIGUSR2);
  }
  if (!strcmp(how, "stale")) arm(t, 0, 0);
  sigset_t w;
  set2(&w, SIGUSR1, other);
  snprintf(label, sizeof label, "sel_%s", how);
  if (!strcmp(how, "sfd")) {
    int fd = signalfd(-1, &w, SFD_NONBLOCK);
    struct signalfd_siginfo rec[3];
    memset(rec, 0, sizeof rec);
    print_records(label, rec, read(fd, rec, sizeof rec));
    return;
  }
  siginfo_t si;
  if (!strcmp(how, "fault")) {
    void *ro = mmap(0, 4096, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    struct timespec z = {0, 0};
    int r = (int)syscall(SYS_rt_sigtimedwait, &w, ro, &z, 8);
    printf("%s first=%s\n", label, r < 0 ? (errno == EFAULT ? "EFAULT" : strerror(errno)) : "taken");
  } else {
    int r = take_set(&w, &si);
    snprintf(label, sizeof label, "sel_%s first", how);
    print_take(label, r, &si);
  }
  int r = take_set(&w, &si);
  snprintf(label, sizeof label, "sel_%s second", how);
  print_take(label, r, &si);
  r = take_set(&w, &si);
  snprintf(label, sizeof label, "sel_%s third", how);
  print_take(label, r, &si);
}

/* R4-4: [stale T1, valid T2] queued, then a ppoll whose mask admits USR1:
 * EINTR with one handler run for T2; with both stale, the ppoll restarts
 * and times out. */
static void ppoll_past_stale(int both) {
  install(SIGUSR1, 0);
  mask(SIG_BLOCK, SIGUSR1);
  timer_t t1 = mk(SIGUSR1, 1, 0);
  timer_t t2 = mk(SIGUSR1, 2, 0);
  arm(t1, 1000, 0);
  sleep_us(1500);
  arm(t2, 1000, 0);
  sleep_us(1500);
  arm(t1, 0, 0);
  if (both) arm(t2, 0, 0);
  sigset_t admit;
  sigemptyset(&admit);
  struct timespec to = {0, 20 * 1000000L};
  int r = ppoll(NULL, 0, &to, &admit);
  printf("ppoll_%s ret=%d errno=%s\n", both ? "both_stale" : "stale_valid", r,
         r < 0 ? (errno == EINTR ? "EINTR" : strerror(errno)) : "-");
  print_handled(both ? "ppoll_both_stale" : "ppoll_stale_valid");
}

/* R3-4: two timers' entries pending in the shared queue; the main thread
 * takes the first in its handler, which blocks USR1, and only then lets a
 * sibling enter rt_sigsuspend admitting USR1, which takes the second. (A
 * kill of the process would be refused by
 * https://github.com/rrnewton/hermit/issues/4034's rule for a sender that
 * blocks the signal.) */
static volatile int go, sibling_done;
static void *suspender(void *a) {
  (void)a;
  while (!go) sleep_us(1000);
  sigset_t admit;
  sigemptyset(&admit);
  sigsuspend(&admit);
  sibling_done = 1;
  return 0;
}
static int sibling_tid;
static void *suspender_tid(void *a) {
  sibling_tid = gettid_();
  return suspender(a);
}
static void promoted_handler(int s, siginfo_t *si, void *u) {
  handler(s, si, u);
  if (!go) {
    go = 1;
    while (!sibling_done) sleep_us(1000);
  }
}
static void armed_promoted(void) {
  struct sigaction a;
  memset(&a, 0, sizeof a);
  a.sa_sigaction = promoted_handler;
  a.sa_flags = SA_SIGINFO;
  sigaction(SIGUSR1, &a, NULL);
  mask(SIG_BLOCK, SIGUSR1);
  pthread_t th;
  pthread_create(&th, 0, suspender_tid, 0);
  timer_t t1 = mk(SIGUSR1, 1, 0);
  arm(t1, 1000, 0);
  sleep_us(1500);
  timer_t t2 = mk(SIGUSR1, VALUE, 0);
  arm(t2, 1000, 0);
  sleep_us(1500);
  mask(SIG_UNBLOCK, SIGUSR1);
  pthread_join(th, 0);
  printf("armed_promoted deliveries=%d first_on_main=%d second_on_sibling=%d\n", n,
         n > 0 && rthread[0] != sibling_tid, n > 1 && rthread[1] == sibling_tid);
  print_handled("armed_promoted");
}

/* Rev 8 B': rt_sigsuspend with one valid entry (delivered, overrun 0), a
 * stale one (Linux sleeps on; a USR2 timer ends it), a valid one behind a
 * stale one, and two valid ones. */
static void susp(const char *how) {
  char label[64];
  snprintf(label, sizeof label, "susp_%s", how);
  install(SIGUSR1, 0);
  install(SIGUSR2, 0);
  mask(SIG_BLOCK, SIGUSR1);
  mask(SIG_BLOCK, SIGUSR2);
  timer_t t1 = mk(SIGUSR1, 1, 0);
  arm(t1, 1000, 0);
  sleep_us(1500);
  timer_t t2 = mk(SIGUSR1, 2, 0);
  if (!strcmp(how, "behind_stale") || !strcmp(how, "two")) {
    arm(t2, 1000, 0);
    sleep_us(1500);
  }
  if (!strcmp(how, "stale") || !strcmp(how, "behind_stale")) arm(t1, 0, 0);
  timer_t w = mk(SIGUSR2, 9, 0);
  arm(w, 5000, 0);
  sigset_t admit;
  sigemptyset(&admit);
  sigsuspend(&admit);
  timer_delete(w);
  printf("%s pending_after=%d\n", label, pending(SIGUSR1));
  print_handled(label);
}

/* Rev 8 A': a shared USR1 timer (value 111) and two private SIGRTMIN records
 * (21, 22); the first take faults copying out SIGRTMIN 21. */
static void rt2_fault(int sfd) {
  install(SIGUSR1, 0);
  sigset_t all;
  sigfillset(&all);
  sigprocmask(SIG_BLOCK, &all, NULL);
  timer_t t = mk(SIGUSR1, 111, 0);
  arm(t, 1000, 0);
  sleep_us(2000);
  for (int v = 21; v <= 22; v++) {
    siginfo_t q;
    memset(&q, 0, sizeof q);
    q.si_signo = SIGRTMIN;
    q.si_code = SI_QUEUE;
    q.si_value.sival_int = v;
    syscall(SYS_rt_tgsigqueueinfo, getpid(), gettid_(), SIGRTMIN, &q);
  }
  sigset_t w;
  set2(&w, SIGUSR1, SIGRTMIN);
  void *ro = mmap(0, 4096, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  if (sfd) {
    int fd = signalfd(-1, &w, SFD_NONBLOCK);
    ssize_t r = read(fd, ro, sizeof(struct signalfd_siginfo));
    printf("rt2_fault_sfd first=%s\n", r < 0 ? (errno == EFAULT ? "EFAULT" : strerror(errno)) : "read");
    for (int i = 0; i < 3; i++) {
      struct signalfd_siginfo rec;
      memset(&rec, 0, sizeof rec);
      r = read(fd, &rec, sizeof rec);
      if (r < 0)
        printf("  read=%s\n", errno == EAGAIN ? "EAGAIN" : strerror(errno));
      else
        printf("  sig_is_rtmin=%d code=%d int=%d\n", (int)rec.ssi_signo == SIGRTMIN, rec.ssi_code,
               rec.ssi_int);
    }
    return;
  }
  struct timespec z = {0, 0};
  int r = (int)syscall(SYS_rt_sigtimedwait, &w, ro, &z, 8);
  printf("rt2_fault first=%s\n", r < 0 ? (errno == EFAULT ? "EFAULT" : strerror(errno)) : "taken");
  for (int i = 0; i < 3; i++) {
    siginfo_t si;
    r = take_set(&w, &si);
    if (r < 0)
      printf("  take=%s\n", errno == EAGAIN ? "EAGAIN" : strerror(errno));
    else
      printf("  sig_is_rtmin=%d code=%d int=%d\n", r == SIGRTMIN, si.si_code,
             si.si_value.sival_int);
  }
}

/* A kill and a timer of a signal whose pairs Hermit does not follow (SIGPIPE,
 * SIGCONT, SIGALRM after alarm()), both pending, then unblocked. */
static void fallback(const char *how) {
  char label[64];
  snprintf(label, sizeof label, "fallback_%s", how);
  int sig = !strcmp(how, "pipe") ? SIGPIPE : !strcmp(how, "cont") ? SIGCONT : SIGALRM;
  install(sig, 0);
  mask(SIG_BLOCK, sig);
  if (sig == SIGALRM) alarm(100);
  kill(getpid(), sig);
  timer_t t = mk(sig, VALUE, 0);
  arm(t, 1000, 0);
  sleep_us(2000);
  mask(SIG_UNBLOCK, sig);
  if (sig == SIGALRM) alarm(0);
  print_handled(label);
}

/* F_SETSIG names a host-timed source of its signal: with a kill and a
 * timer's entry of USR1 pending, Hermit cannot follow USR1 from then on. The
 * USR2 neighbour leaves USR1 followed. */
static void setsig(int sig) {
  install(SIGUSR1, 0);
  mask(SIG_BLOCK, SIGUSR1);
  kill(getpid(), SIGUSR1);
  timer_t t = mk(SIGUSR1, VALUE, 0);
  arm(t, 1000, 0);
  sleep_us(2000);
  int p[2];
  if (pipe(p)) perror("pipe");
  int r = fcntl(p[0], F_SETSIG, sig);
  printf("setsig_%s fcntl=%d\n", sig == SIGUSR1 ? "usr1" : "usr2", r);
  mask(SIG_UNBLOCK, SIGUSR1);
  print_handled(sig == SIGUSR1 ? "setsig_usr1" : "setsig_usr2");
}

/* Timer-free controls: two kills pending give one delivery (H'); a pending
 * kill read through a signalfd with room for two is one record, by read and
 * by readv (N8-7). */
static void two_kills(void) {
  install(SIGUSR1, 0);
  mask(SIG_BLOCK, SIGUSR1);
  kill(getpid(), SIGUSR1);
  kill(getpid(), SIGUSR1);
  mask(SIG_UNBLOCK, SIGUSR1);
  print_handled("two_kills");
}
static void sfd_notimer(int vectored) {
  mask(SIG_BLOCK, SIGUSR1);
  kill(getpid(), SIGUSR1);
  sigset_t w;
  set2(&w, SIGUSR1, 0);
  int fd = signalfd(-1, &w, SFD_NONBLOCK);
  struct signalfd_siginfo rec[2];
  memset(rec, 0, sizeof rec);
  ssize_t got;
  if (vectored) {
    struct iovec v[2] = {{&rec[0], sizeof rec[0]}, {&rec[1], sizeof rec[1]}};
    got = readv(fd, v, 2);
  } else {
    got = read(fd, rec, sizeof rec);
  }
  print_records(vectored ? "sfd_notimer_readv" : "sfd_notimer_read", rec, got);
}


/* Review of https://github.com/rrnewton/hermit/pull/4491, P1-1: a periodic
 * timer whose entry is queued when its process makes a call outside the
 * schedule must keep expiring. Each mode takes later ticks with a bounded
 * sigtimedwait and prints whether all arrived. */
static int later_ticks(int want) {
  sigset_t w;
  set2(&w, SIGUSR1, 0);
  struct timespec to = {0, 100 * 1000000L};
  int got = 0;
  for (int k = 0; k < want; k++) {
    siginfo_t si;
    if (sigtimedwait(&w, &si, &to) == SIGUSR1 && si.si_code == SI_TIMER) got++;
  }
  return got;
}
/* rt_sigsuspend with a periodic timer's single valid entry queued: the entry
 * is delivered (the pair falls back), and the timer keeps expiring. */
static void susp_periodic(void) {
  install(SIGUSR1, 0);
  mask(SIG_BLOCK, SIGUSR1);
  timer_t t = mk(SIGUSR1, 3, 0);
  arm(t, 1000, 1000);
  sleep_us(1200);
  sigset_t admit;
  sigemptyset(&admit);
  sigsuspend(&admit);
  int first = n;
  int later = later_ticks(5);
  timer_delete(t);
  printf("susp_periodic first=%d first_si_timer=%d later_5_of_5=%d\n", first,
         first > 0 && rcode[0] == SI_TIMER, later == 5);
}
/* The tick loop of the review's sigwait_bg probe: USR1 blocked, a periodic
 * 1 ms timer, 20 sigwaitinfo ticks, while a sibling enters sigsuspend. */
static void tick_h2(int s) { (void)s; }
static void *tick_sibling(void *a) {
  (void)a;
  sleep_us(3000);
  sigset_t w;
  sigfillset(&w);
  sigdelset(&w, SIGUSR2);
  sigsuspend(&w);
  return NULL;
}
static void sigwait_bg(void) {
  struct sigaction sb;
  memset(&sb, 0, sizeof sb);
  sb.sa_handler = tick_h2;
  sigaction(SIGUSR2, &sb, NULL);
  mask(SIG_BLOCK, SIGUSR1);
  mask(SIG_BLOCK, SIGUSR2);
  pthread_t th;
  pthread_create(&th, NULL, tick_sibling, NULL);
  timer_t t = mk(SIGUSR1, 7, 0);
  arm(t, 1000, 1000);
  for (volatile long i = 0; i < 20000000L; i++) {
  }
  sigset_t one;
  set2(&one, SIGUSR1, 0);
  long ticks = 0;
  for (int k = 0; k < 20; k++) {
    siginfo_t si;
    if (sigwaitinfo(&one, &si) == SIGUSR1) ticks += 1 + si.si_overrun;
  }
  timer_delete(t);
  pthread_kill(th, SIGUSR2);
  pthread_join(th, NULL);
  printf("sigwait_bg took 20 ticks, expiries>=20: %d\n", ticks >= 20);
}
/* posix_spawn (a vfork-style call outside the schedule) while a periodic
 * timer's entry is queued: the ticks continue after it. */
extern char **environ;
static void spawn_tick(void) {
  mask(SIG_BLOCK, SIGUSR1);
  timer_t t = mk(SIGUSR1, 4, 0);
  arm(t, 1000, 1000);
  sleep_us(1200);
  pid_t pid;
  char *argv[] = {(char *)"/bin/true", NULL};
  int r = posix_spawn(&pid, "/bin/true", NULL, NULL, argv, environ);
  int status = -1;
  if (r == 0) waitpid(pid, &status, 0);
  int later = later_ticks(5);
  timer_delete(t);
  printf("spawn_tick spawn=%d status=%d later_5_of_5=%d\n", r, status, later == 5);
}

/* Review P2-1: a stale timer entry among more pending signals than a
 * signalfd read has room for; Linux skips it inside the dequeue and fills the
 * slot with the next signal ("behind": a valid second entry behind it). */
static void sfd_stale_room(int behind) {
  sigset_t set;
  sigemptyset(&set);
  sigaddset(&set, SIGHUP);
  sigaddset(&set, SIGUSR1);
  sigaddset(&set, SIGUSR2);
  sigprocmask(SIG_BLOCK, &set, NULL);
  timer_t a = mk(SIGUSR1, 1, 0);
  arm(a, 1000, 0);
  sleep_us(20000);
  if (behind) {
    timer_t b = mk(SIGUSR1, 2, 0);
    arm(b, 1000, 0);
    sleep_us(20000);
  }
  arm(a, 100000000, 0);
  kill(getpid(), SIGHUP);
  kill(getpid(), SIGUSR2);
  int fd = signalfd(-1, &set, SFD_NONBLOCK);
  struct signalfd_siginfo rec[2];
  memset(rec, 0, sizeof rec);
  ssize_t got = read(fd, rec, sizeof rec);
  const char *label = behind ? "sfd_stale_behind" : "sfd_stale_single";
  print_records(label, rec, got);
  for (int k = 0; k < 3; k++) {
    struct signalfd_siginfo r;
    if (read(fd, &r, sizeof r) <= 0) {
      printf("%s then=EAGAIN\n", label);
      break;
    }
    print_records(label, &r, sizeof r);
  }
}

/* Rev 6 §9: strict verify with a polling sibling in an observable process. A
 * sibling polls an empty internal pipe without blocking while the main
 * thread takes a tgkill and a SIGEV_THREAD_ID timer entry of USR1, then a
 * timer blocked for 5 periods. */
static volatile int poll_stop;
static void *poller(void *a) {
  int fd = (int)(intptr_t)a;
  char c;
  while (!poll_stop) {
    if (read(fd, &c, 1) < 0 && errno != EAGAIN) break;
    sched_yield();
  }
  return NULL;
}
static void poll_sibling(void) {
  int p[2];
  if (pipe2(p, O_NONBLOCK)) perror("pipe2");
  sigset_t block;
  set2(&block, SIGUSR1, 0);
  pthread_sigmask(SIG_BLOCK, &block, NULL);
  pthread_t th;
  pthread_create(&th, NULL, poller, (void *)(intptr_t)p[0]);
  install(SIGUSR1, 0);
  /* Thread-directed: a process-directed kill by a thread that blocks the
   * signal, in a process with two live threads, is refused by
   * https://github.com/rrnewton/hermit/issues/4034's rule. */
  syscall(SYS_tgkill, getpid(), gettid_(), SIGUSR1);
  timer_t t = mk(SIGUSR1, VALUE, gettid_());
  arm(t, 1000, 0);
  sleep_us(2000);
  mask(SIG_UNBLOCK, SIGUSR1);
  mask(SIG_BLOCK, SIGUSR1);
  int before = n;
  timer_t b = mk(SIGUSR1, 5, gettid_());
  long armed = now_ns();
  arm(b, 1000, 1000);
  sleep_us(5200);
  long woke = now_ns();
  mask(SIG_UNBLOCK, SIGUSR1);
  long after = now_ns();
  mask(SIG_BLOCK, SIGUSR1);
  timer_delete(b);
  poll_stop = 1;
  pthread_join(th, NULL);
  /* The polling sibling's turns may advance the main thread's clock between
   * its wake and its unblock; the overrun must be the one its own clock
   * reads imply: the grid points from the wake to just after the unblock. */
  int ov = n > before ? rov[before] : -1;
  long lo = (woke - armed) / 1000000L - 1, hi = (after - armed) / 1000000L - 1;
  n = before;
  print_handled("poll_sibling");
  printf("poll_sibling blocked_entry_si_timer=%d overrun_matches_clock=%d\n",
         rcode[before] == SI_TIMER && rtid[before] == 1, ov >= lo && ov <= hi);
}

/* Rev 6 §9: a SIGCHLD timer while a child is unreaped (the pair takes the
 * fallback): blocked for 5 periods, then unblocked. */
static void sigchld_timer(void) {
  install(SIGCHLD, 0);
  mask(SIG_BLOCK, SIGCHLD);
  pid_t child = fork();
  if (child == 0) {
    sleep_us(2000000);
    _exit(0);
  }
  timer_t t = mk(SIGCHLD, VALUE, 0);
  arm(t, 1000, 1000);
  sleep_us(5200);
  mask(SIG_UNBLOCK, SIGCHLD);
  mask(SIG_BLOCK, SIGCHLD);
  timer_delete(t);
  int timer_deliveries = 0, overruns = 0;
  for (int i = 0; i < n && i < MAXREC; i++) {
    if (rcode[i] == SI_TIMER) {
      timer_deliveries++;
      overruns += rov[i];
    }
  }
  printf("sigchld_timer timer_deliveries=%d overruns=%d\n", timer_deliveries, overruns);
  mask(SIG_BLOCK, SIGCHLD);
  kill(child, SIGKILL);
  waitpid(child, NULL, 0);
}

/* Rev 6 §9 "D1 + single-entry transition": the D1 construction, whose
 * worker timer entry stands on the discarded copy, then a call outside the
 * schedule (rt_sigsuspend in the main thread): begun before the expiry, or
 * ("queued") while the entry is queued. */
static void d1_transition(int queued) {
  install(SIGUSR1, 1);
  install(SIGUSR2, 0);
  pthread_t th;
  pthread_create(&th, 0, parked_worker, (void *)(intptr_t)300);
  mask(SIG_BLOCK, SIGUSR1);
  mask(SIG_BLOCK, SIGUSR2);
  while (!wtid) sleep_us(1000);
  sleep_us(50000);
  siginfo_t si;
  memset(&si, 0, sizeof si);
  si.si_signo = SIGUSR1;
  si.si_code = SI_QUEUE;
  si.si_value.sival_int = 5;
  if (syscall(SYS_rt_tgsigqueueinfo, getpid(), wtid, SIGUSR1, &si)) perror("discarded");
  install(SIGUSR1, 0);
  sigset_t block;
  set2(&block, SIGUSR1, 0);
  timer_t t = mk(SIGUSR1, VALUE, wtid);
  arm(t, 1000, 0);
  if (queued) sleep_us(2000);
  timer_t w = mk(SIGUSR2, 9, 0);
  arm(w, 5000, 0);
  sigset_t admit;
  sigfillset(&admit);
  sigdelset(&admit, SIGUSR2);
  sigsuspend(&admit);
  pthread_join(th, 0);
  printf("%s deliveries=%d\n", queued ? "d1_transition_queued" : "d1_transition", n);
}

/* Review P2-2: the delivery stop and the delivery's turn apart in virtual
 * time. Two siblings spin (USR1 blocked) while a 1 ms timer's handler runs on
 * the main thread, so each delivery's turn may wait behind theirs. For each
 * delivery, the expiries it accounts for (1 + si_overrun, summed) must not
 * exceed the grid points up to the handler's own clock read, nor trail them
 * by more than the one that may expire between the dequeue and that read. */
static volatile int spin_stop;
static void *spinner(void *a) {
  (void)a;
  sigset_t block;
  set2(&block, SIGUSR1, 0);
  pthread_sigmask(SIG_BLOCK, &block, NULL);
  while (!spin_stop) {
  }
  return NULL;
}
static volatile long dt_at[MAXREC];
static void turn_handler(int s, siginfo_t *si, void *u) {
  long t = now_ns();
  if (n < MAXREC) dt_at[n] = t;
  handler(s, si, u);
}
static void delivery_turn(void) {
  pthread_t a, b;
  pthread_create(&a, NULL, spinner, NULL);
  pthread_create(&b, NULL, spinner, NULL);
  struct sigaction sa;
  memset(&sa, 0, sizeof sa);
  sa.sa_sigaction = turn_handler;
  sa.sa_flags = SA_SIGINFO;
  sigaction(SIGUSR1, &sa, NULL);
  timer_t t = mk(SIGUSR1, VALUE, 0);
  long start = now_ns();
  arm(t, 1000, 1000);
  long armed = now_ns();
  for (volatile long i = 0; i < 30000000L; i++) {
  }
  mask(SIG_BLOCK, SIGUSR1);
  timer_delete(t);
  spin_stop = 1;
  pthread_join(a, NULL);
  pthread_join(b, NULL);
  int got = n < MAXREC ? n : MAXREC;
  long sum = 0;
  int ok = got > 0 && got == n;
  for (int i = 0; i < got; i++) {
    sum += 1 + rov[i];
    long lo = (dt_at[i] - armed) / 1000000L, hi = (dt_at[i] - start) / 1000000L;
    if (sum > hi || sum + 1 < lo) ok = 0;
    fprintf(stderr, "delivery_turn %d sum=%ld lo=%ld hi=%ld\n", i, sum, lo, hi);
  }
  printf("delivery_turn deliveries_positive=%d all_si_timer=%d accounting_ok=%d\n", n > 0,
         got > 0 && rcode[0] == SI_TIMER, ok);
}

static char *self_path;

/* N9-5: a worker with a tgkill USR1 pending in its own queue execs; the
 * kept copy survives with the leader's tid and is delivered after exec. */
static void *exec_worker(void *a) {
  (void)a;
  wtid = gettid_();
  while (!futex_word) sleep_us(1000);
  char *argv[] = {self_path, (char *)"nonleader_exec_child", NULL};
  execv(self_path, argv);
  perror("execv");
  exit(2);
}
static void nonleader_exec(void) {
  mask(SIG_BLOCK, SIGUSR1);
  pthread_t th;
  pthread_create(&th, 0, exec_worker, 0);
  while (!wtid) sleep_us(1000);
  syscall(SYS_tgkill, getpid(), wtid, SIGUSR1);
  futex_word = 1;
  pthread_join(th, 0);
  printf("nonleader_exec join_returned\n");
}
static void nonleader_exec_child(void) {
  printf("nonleader_exec pending_after_exec=%d\n", pending(SIGUSR1));
  install(SIGUSR1, 0);
  mask(SIG_UNBLOCK, SIGUSR1);
  print_handled("nonleader_exec");
}

/* The J construction with a process-directed SI_QUEUE send in a process with
 * two live threads. */
static void forged_shared(void) {
  install(SIGUSR1, 1);
  pthread_t th;
  pthread_create(&th, 0, parked_worker, (void *)(intptr_t)200);
  mask(SIG_BLOCK, SIGUSR1);
  while (!wtid) sleep_us(1000);
  siginfo_t si;
  memset(&si, 0, sizeof si);
  si.si_signo = SIGUSR1;
  si.si_code = SI_QUEUE;
  si.si_value.sival_ptr = (void *)(uintptr_t)VALUE;
  int r = (int)syscall(SYS_rt_sigqueueinfo, getpid(), SIGUSR1, &si);
  printf("forged_shared queue=%d\n", r);
  pthread_join(th, 0);
}

/* The Codex review of https://github.com/rrnewton/hermit/pull/4491. */
static int cx_timer(int value) {
  struct sigevent ev;
  memset(&ev, 0, sizeof ev);
  ev.sigev_notify = SIGEV_SIGNAL;
  ev.sigev_signo = SIGUSR1;
  ev.sigev_value.sival_int = value;
  int id = -1;
  if (syscall(SYS_timer_create, CLOCK_MONOTONIC, &ev, &id)) exit(3);
  return id;
}
static void cx_arm(int id, long ns, long period) {
  struct itimerspec t = {{period / 1000000000, period % 1000000000}, {ns / 1000000000, ns % 1000000000}};
  if (syscall(SYS_timer_settime, id, 0, &t, NULL)) exit(3);
}
static void cx_sleep_ms(long ms) {
  struct timespec ts = {ms / 1000, (ms % 1000) * 1000000};
  while (nanosleep(&ts, &ts)) if (errno != EINTR) exit(3);
}
static void cx_print_read(const char *name, int fd) {
  struct signalfd_siginfo rec[4];
  memset(rec, 0, sizeof rec);
  errno = 0;
  ssize_t got = read(fd, rec, sizeof rec);
  int e = errno;
  printf("%s bytes=%zd errno=%d", name, got, got < 0 ? e : 0);
  for (ssize_t i = 0; i < got / (ssize_t)sizeof(rec[0]); i++)
    printf(" code%zd=%d val%zd=%u", i, rec[i].ssi_code, i, rec[i].ssi_int);
  putchar('\n');
}
/* P2-1: [Plain, stale A, valid B] read by one signalfd read. */
static void cx_stale_middle(void) {
  mask(SIG_BLOCK, SIGUSR1);
  kill(getpid(), SIGUSR1);
  int a = cx_timer(111), b = cx_timer(222);
  cx_arm(a, 1000000, 0);
  cx_sleep_ms(3);
  cx_arm(b, 1000000, 0);
  cx_sleep_ms(3);
  cx_arm(a, 0, 0);
  sigset_t s;
  sigemptyset(&s);
  sigaddset(&s, SIGUSR1);
  int fd = signalfd(-1, &s, SFD_NONBLOCK);
  cx_print_read("stale_middle first", fd);
  cx_print_read("stale_middle second", fd);
}
/* A thread spinning in user code catches SIGUSR1 while another thread
 * tgkills it again and again, as cargo's jobserver interrupts its helper: a
 * send can land after the kernel dequeued the previous copy for a delivery
 * and before that delivery's handler ran, and Linux queues it. A model that
 * dropped it refused the run (the regression of
 * https://github.com/rrnewton/hermit/pull/4491). */
static atomic_int rs_stop, rs_handled, rs_ready, rs_tid;
static void rs_handler(int s) {
  (void)s;
  atomic_fetch_add(&rs_handled, 1);
}
static void *rs_spinner(void *a) {
  (void)a;
  atomic_store(&rs_tid, gettid_());
  atomic_store(&rs_ready, 1);
  volatile unsigned long spins = 0;
  while (!atomic_load(&rs_stop)) spins++;
  return NULL;
}
static void resend_while_delivering(void) {
  struct sigaction sa;
  memset(&sa, 0, sizeof sa);
  sa.sa_handler = rs_handler;
  sigaction(SIGUSR1, &sa, NULL);
  pthread_t t;
  pthread_create(&t, NULL, rs_spinner, NULL);
  while (!atomic_load(&rs_ready)) sched_yield();
  /* 40 sends, not 300: since the delivery that continues an rt_sigreturn
   * runs at once (https://github.com/rrnewton/hermit/issues/4505), the
   * spinner runs its own loop for a whole timeslice after every other
   * delivery, about 140 ms of wall time under a debug Hermit. 40 sends still
   * land in a delivery's wait about 20 times. */
  for (int i = 0; i < 40; i++) {
    syscall(SYS_tgkill, getpid(), atomic_load(&rs_tid), SIGUSR1);
    sched_yield();
  }
  while (atomic_load(&rs_handled) == 0) sched_yield();
  atomic_store(&rs_stop, 1);
  pthread_join(t, NULL);
  printf("resend_while_delivering handled_at_least_once=%d\n", atomic_load(&rs_handled) > 0);
}
/* The Codex re-review's P2: [Plain, 80 stale entries, valid B] read by one
 * signalfd read with room for 4. Linux skips every stale entry inside the
 * dequeue, so the read returns the Plain and B. */
static void cx_many_stale(void) {
  enum { STALE = 80 };
  mask(SIG_BLOCK, SIGUSR1);
  kill(getpid(), SIGUSR1);
  int old[STALE];
  for (int i = 0; i < STALE; i++) {
    old[i] = cx_timer(100 + i);
    cx_arm(old[i], 1000000, 0);
    cx_sleep_ms(3);
  }
  int b = cx_timer(222);
  cx_arm(b, 1000000, 0);
  cx_sleep_ms(3);
  for (int i = 0; i < STALE; i++) cx_arm(old[i], 0, 0);
  sigset_t s;
  sigemptyset(&s);
  sigaddset(&s, SIGUSR1);
  int fd = signalfd(-1, &s, SFD_NONBLOCK);
  cx_print_read("many_stale first", fd);
  cx_print_read("many_stale second", fd);
}
/* P2-2: a disarm whose old_value copy faults still disarms. */
static void cx_bad_old_disarm(void) {
  mask(SIG_BLOCK, SIGUSR1);
  int a = cx_timer(333);
  cx_arm(a, 1000000, 0);
  cx_sleep_ms(3);
  struct itimerspec zero;
  memset(&zero, 0, sizeof zero);
  errno = 0;
  long change = syscall(SYS_timer_settime, a, 0, &zero, (void *)(uintptr_t)1);
  int ce = errno;
  sigset_t s;
  sigemptyset(&s);
  sigaddset(&s, SIGUSR1);
  struct timespec z = {0, 0};
  siginfo_t si;
  memset(&si, 0, sizeof si);
  errno = 0;
  long got = syscall(SYS_rt_sigtimedwait, &s, &si, &z, 8);
  int ge = errno;
  printf("bad_old_disarm settime=%ld errno=%d take=%ld errno=%d code=%d value=%d\n", change, ce, got,
         ge, si.si_code, si.si_value.sival_int);
}
/* P2-2: a set whose old_value copy faults still resets the overrun. */
static void cx_bad_old_overrun(void) {
  mask(SIG_BLOCK, SIGUSR1);
  int a = cx_timer(444);
  cx_arm(a, 1000000, 1000000);
  cx_sleep_ms(8);
  sigset_t s;
  sigemptyset(&s);
  sigaddset(&s, SIGUSR1);
  struct timespec z = {0, 0};
  siginfo_t si;
  memset(&si, 0, sizeof si);
  long got = syscall(SYS_rt_sigtimedwait, &s, &si, &z, 8);
  long before = syscall(SYS_timer_getoverrun, a);
  struct itimerspec far = {{0, 0}, {3600, 0}};
  errno = 0;
  long change = syscall(SYS_timer_settime, a, 0, &far, (void *)(uintptr_t)1);
  int ce = errno;
  long after = syscall(SYS_timer_getoverrun, a);
  printf("bad_old_overrun taken=%d before_positive=%d settime=%ld errno=%d after=%ld\n",
         got == SIGUSR1, before > 0, change, ce, after);
}
/* P2-3: F_SETSIG makes the pair unobservable with a periodic entry queued;
 * the timer keeps expiring. */
static void cx_setsig_periodic(void) {
  mask(SIG_BLOCK, SIGUSR1);
  int p[2];
  if (pipe(p)) exit(3);
  int a = cx_timer(555);
  cx_arm(a, 1000000, 10000000);
  cx_sleep_ms(3);
  errno = 0;
  int change = fcntl(p[0], F_SETSIG, SIGUSR1);
  int ce = errno;
  sigset_t s;
  sigemptyset(&s);
  sigaddset(&s, SIGUSR1);
  struct timespec z = {0, 0};
  siginfo_t si;
  memset(&si, 0, sizeof si);
  errno = 0;
  long first = syscall(SYS_rt_sigtimedwait, &s, &si, &z, 8);
  int fe = errno;
  cx_sleep_ms(25);
  memset(&si, 0, sizeof si);
  errno = 0;
  long second = syscall(SYS_rt_sigtimedwait, &s, &si, &z, 8);
  int se = errno;
  printf("setsig_periodic setsig=%d errno=%d first=%ld errno=%d second=%ld errno=%d\n", change, ce,
         first, fe, second, se);
}

int main(int argc, char **argv) {
  if (argc < 2) {
    fprintf(stderr, "usage: %s <mode>\n", argv[0]);
    return 2;
  }
  self_path = argv[0];
  const char *m = argv[1];
  setvbuf(stdout, NULL, _IONBF, 0);
  if (!strcmp(m, "storm")) storm();
  else if (!strcmp(m, "d1")) d1();
  else if (!strcmp(m, "d2")) d2();
  else if (!strncmp(m, "sel_", 4)) selection(m + 4);
  else if (!strcmp(m, "ppoll_stale_valid")) ppoll_past_stale(0);
  else if (!strcmp(m, "ppoll_both_stale")) ppoll_past_stale(1);
  else if (!strcmp(m, "armed_promoted")) armed_promoted();
  else if (!strcmp(m, "susp_periodic")) susp_periodic();
  else if (!strncmp(m, "susp_", 5)) susp(m + 5);
  else if (!strcmp(m, "rt2_fault")) rt2_fault(0);
  else if (!strcmp(m, "rt2_fault_sfd")) rt2_fault(1);
  else if (!strncmp(m, "fallback_", 9)) fallback(m + 9);
  else if (!strcmp(m, "setsig_usr1")) setsig(SIGUSR1);
  else if (!strcmp(m, "setsig_usr2")) setsig(SIGUSR2);
  else if (!strcmp(m, "two_kills")) two_kills();
  else if (!strcmp(m, "sfd_notimer_read")) sfd_notimer(0);
  else if (!strcmp(m, "sfd_notimer_readv")) sfd_notimer(1);
  else if (!strcmp(m, "nonleader_exec")) nonleader_exec();
  else if (!strcmp(m, "nonleader_exec_child")) nonleader_exec_child();
  else if (!strcmp(m, "forged_shared")) forged_shared();
  else if (!strcmp(m, "sigwait_bg")) sigwait_bg();
  else if (!strcmp(m, "spawn_tick")) spawn_tick();
  else if (!strcmp(m, "sfd_stale_single")) sfd_stale_room(0);
  else if (!strcmp(m, "sfd_stale_behind")) sfd_stale_room(1);
  else if (!strcmp(m, "poll_sibling")) poll_sibling();
  else if (!strcmp(m, "sigchld_timer")) sigchld_timer();
  else if (!strcmp(m, "d1_transition")) d1_transition(0);
  else if (!strcmp(m, "d1_transition_queued")) d1_transition(1);
  else if (!strcmp(m, "delivery_turn")) delivery_turn();
  else if (!strcmp(m, "stale_middle")) cx_stale_middle();
  else if (!strcmp(m, "many_stale")) cx_many_stale();
  else if (!strcmp(m, "resend_while_delivering")) resend_while_delivering();
  else if (!strcmp(m, "bad_old_disarm")) cx_bad_old_disarm();
  else if (!strcmp(m, "bad_old_overrun")) cx_bad_old_overrun();
  else if (!strcmp(m, "setsig_periodic")) cx_setsig_periodic();
  else {
    fprintf(stderr, "unknown mode %s\n", m);
    return 2;
  }
  return 0;
}
