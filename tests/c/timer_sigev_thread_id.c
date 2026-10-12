/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/* Guest for hermit-cli/tests/cli.rs: a POSIX timer created with
 * SIGEV_THREAD_ID notifies exactly the thread it names
 * (https://github.com/rrnewton/hermit/issues/4164).
 *
 * usage: timer_sigev_thread_id <mode>
 *
 * Every timer is created with the raw syscall, so libc adds nothing.
 *   answers: what timer_create answers for each notification, as Linux's
 *            good_sigevent checks it: a thread of another process, a thread
 *            that has exited, ids 0 and -1, signal 0 with a good thread,
 *            SIGEV_THREAD (which the kernel treats as SIGEV_SIGNAL), and the
 *            invalid notify values 3, 6 and 7.
 *   worker:  a one-shot timer names the worker, which sleeps in 1 ms naps
 *            with SIGUSR1 unblocked, as does the main thread, which armed it
 *            and waits in pthread_join. Linux runs the handler on the worker.
 *   blocked_worker: the same, but the worker blocks SIGUSR1. Linux keeps the
 *            expiration pending on the worker (the main thread never takes
 *            it); the worker sees it in sigpending, then unblocks and runs
 *            the handler itself.
 *   exited:  a periodic 1 ms timer names a worker that has exited. Linux
 *            drops every expiration and keeps the timer armed.
 *   self:    a one-shot timer names the main thread while a worker sleeps
 *            with SIGUSR1 unblocked: the handler runs on the main thread.
 *   rt_blocked_worker: a one-shot timer notifying SIGRTMIN+2 names a worker
 *            that blocks it and exits without looking; Linux discards the
 *            pending signal with the thread, and nothing is delivered.
 *   napping_main: the worker blocks SIGUSR1 while the main thread naps with
 *            it unblocked and keeps running; the expiration stays pending on
 *            the worker, so the main thread never takes it.
 *   overrun: a periodic 3 ms timer names the main thread, which blocks
 *            SIGUSR1 across several expirations: Linux queues the timer's
 *            signal once and counts the rest as overruns, so the unblock
 *            delivers it once.
 *   coalesce_self: the main thread blocks SIGUSR1, sends it to itself with
 *            tgkill, then a timer naming it expires: Linux queues the timer's
 *            own signal beside the pending one and delivers both. Hermit can
 *            deliver only one, and refuses the run by name.
 *   two_timers_self: two timers naming the blocked main thread with SIGUSR1
 *            expire: Linux delivers both; Hermit refuses by name.
 *   pending_then_raise: a timer's copy is pending on the blocked main thread
 *            when it sends SIGUSR1 to itself (Linux drops that legacy copy),
 *            then the timer expires again (an overrun): one delivery.
 * After the thread has taken a timer's copy, these put another SIGUSR1 on it
 * while it blocks the signal, and the timer expires again: Linux delivers both
 * copies, Hermit refuses by name.
 *   raise_rearm: the thread sends it to itself with tgkill.
 *   tgkill_cross: the main thread sends it to the worker with tgkill.
 *   pidfd_leader: a sibling sends it with pidfd_send_signal and
 *            PIDFD_SIGNAL_THREAD on a pidfd of the process (its leader).
 * The timer's copy is pending on the blocked main thread, then taken away
 * without a delivery; then (except in the _noraise mode) the thread sends the
 * signal to itself and the re-armed timer expires: Linux delivers both copies,
 * Hermit refuses by name.
 *   ign_flush: SIG_IGN, then the handler again (a flush).
 *   ign_flush_noraise: the same with no tgkill: one delivery, as on Linux.
 *   ign_flush_efault: SIG_IGN installed by a raw rt_sigaction whose old
 *            action pointer faults: EFAULT, with SIG_IGN installed (a flush).
 *   dfl_flush: SIGURG, whose default is to ignore it: SIG_DFL, then the
 *            handler again.
 *   stw_zero: a zero-timeout sigtimedwait takes it.
 *   cont_flush: the timer notifies SIGTSTP (handled, blocked); a SIGCONT the
 *            thread sends itself flushes the pending copy; then SIGTSTP from
 *            itself and the re-armed timer: Linux delivers 2.
 *   tstp_flush: the reverse: the timer notifies SIGCONT, a SIGTSTP flushes it.
 *   signalfd_overrun: the process holds a signalfd for SIGUSR1 when the timer
 *            expires again beside its own pending copy. Linux counts an
 *            overrun; a signalfd read is not observed, so Hermit cannot prove
 *            the copy is the timer's own and refuses by name.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/signalfd.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

static volatile sig_atomic_t hits, hit_tid;

static void on_signal(int sig, siginfo_t *info, void *context) {
  (void)sig;
  (void)info;
  (void)context;
  hits += 1;
  hit_tid = (int)syscall(SYS_gettid);
}

static void catch_signal(int sig) {
  struct sigaction action;
  memset(&action, 0, sizeof action);
  action.sa_sigaction = on_signal;
  action.sa_flags = SA_SIGINFO;
  if (sigaction(sig, &action, NULL) != 0) exit(70);
}

static void mask(int how, int sig) {
  sigset_t set;
  sigemptyset(&set);
  sigaddset(&set, sig);
  if (pthread_sigmask(how, &set, NULL) != 0) exit(71);
}

static int gettid_raw(void) { return (int)syscall(SYS_gettid); }

/* timer_create's answer: the timer id, or -errno. */
static long create(int notify, int signo, int tid, int *id) {
  struct sigevent event;
  memset(&event, 0, sizeof event);
  event.sigev_notify = notify;
  event.sigev_signo = signo;
  event._sigev_un._tid = tid; /* sigev_notify_thread_id; older glibc lacks the name */
  long rc = syscall(SYS_timer_create, CLOCK_MONOTONIC, &event, id);
  return rc < 0 ? -errno : rc;
}

static const char *answer(long rc) { return rc < 0 ? strerrorname_np((int)-rc) : "0"; }

static void arm(int id, long first_ns, long interval_ns) {
  struct itimerspec spec;
  memset(&spec, 0, sizeof spec);
  spec.it_value.tv_nsec = first_ns;
  spec.it_interval.tv_nsec = interval_ns;
  if (syscall(SYS_timer_settime, id, 0, &spec, NULL) != 0) exit(72);
}

static volatile int sender_rc;
static void *pidfd_sender(void *unused) {
  (void)unused;
  int fd = (int)syscall(SYS_pidfd_open, getpid(), 0);
  if (fd < 0 || syscall(SYS_pidfd_send_signal, fd, SIGUSR1, NULL, 1 /* PIDFD_SIGNAL_THREAD */))
    sender_rc = errno;
  return NULL;
}

static void nap(void) {
  struct timespec ms = {0, 1000000};
  nanosleep(&ms, NULL);
}

static volatile int worker_tid, worker_ready, worker_go, worker_blocks, worker_done;
static volatile int worker_saw_pending;

static void *worker(void *unused) {
  (void)unused;
  if (worker_blocks) mask(SIG_BLOCK, worker_blocks);
  worker_tid = gettid_raw();
  __atomic_store_n(&worker_ready, 1, __ATOMIC_SEQ_CST);
  while (!__atomic_load_n(&worker_go, __ATOMIC_SEQ_CST)) nap();
  if (__atomic_load_n(&worker_go, __ATOMIC_SEQ_CST) == 2) {
    /* tgkill_cross: block, let the main thread send, then unblock. */
    mask(SIG_BLOCK, SIGUSR1);
    __atomic_store_n(&worker_go, 3, __ATOMIC_SEQ_CST);
    while (__atomic_load_n(&worker_go, __ATOMIC_SEQ_CST) != 4) nap();
    mask(SIG_UNBLOCK, SIGUSR1);
    return NULL;
  }
  /* Sleep well past the expiry, or until the handler has run. */
  for (int i = 0; i < 20 && !hits; i++) nap();
  if (worker_blocks == SIGUSR1) {
    sigset_t pending;
    sigpending(&pending);
    worker_saw_pending = sigismember(&pending, SIGUSR1);
    __atomic_store_n(&worker_done, 1, __ATOMIC_SEQ_CST);
    mask(SIG_UNBLOCK, SIGUSR1);
  }
  return NULL;
}

static pthread_t start_worker(int blocks) {
  pthread_t thread;
  worker_blocks = blocks;
  if (pthread_create(&thread, NULL, worker, NULL) != 0) exit(73);
  while (!__atomic_load_n(&worker_ready, __ATOMIC_SEQ_CST)) nap();
  return thread;
}

static void release_and_join(pthread_t thread) {
  __atomic_store_n(&worker_go, 1, __ATOMIC_SEQ_CST);
  if (pthread_join(thread, NULL) != 0) exit(74);
}

int main(int argc, char **argv) {
  const char *mode = argc > 1 ? argv[1] : "worker";
  int id;
  catch_signal(SIGUSR1);
  if (!strcmp(mode, "answers")) {
    pthread_t thread = start_worker(0);
    int gone = worker_tid;
    release_and_join(thread);
    int me = gettid_raw();
    printf("answers foreign=%s exited=%s zero=%s minus_one=%s signo0=%s thread=%s"
           " notify3=%s notify6=%s notify7=%s\n",
           answer(create(SIGEV_THREAD_ID, SIGUSR1, getppid(), &id)),
           answer(create(SIGEV_THREAD_ID, SIGUSR1, gone, &id)),
           answer(create(SIGEV_THREAD_ID, SIGUSR1, 0, &id)),
           answer(create(SIGEV_THREAD_ID, SIGUSR1, -1, &id)),
           answer(create(SIGEV_THREAD_ID, 0, me, &id)),
           answer(create(SIGEV_THREAD, SIGUSR1, 0, &id)),
           answer(create(3, SIGUSR1, me, &id)),
           answer(create(6, SIGUSR1, me, &id)),
           answer(create(7, SIGUSR1, me, &id)));
  } else if (!strcmp(mode, "worker") || !strcmp(mode, "blocked_worker")) {
    int blocked = !strcmp(mode, "blocked_worker");
    pthread_t thread = start_worker(blocked ? SIGUSR1 : 0);
    if (create(SIGEV_THREAD_ID, SIGUSR1, worker_tid, &id) != 0) exit(75);
    arm(id, 1000, 0);
    release_and_join(thread);
    printf("%s hits=%d on_worker=%d", mode, (int)hits, hit_tid == worker_tid);
    if (blocked) printf(" pending_on_worker=%d", worker_saw_pending);
    printf("\n");
  } else if (!strcmp(mode, "exited")) {
    pthread_t thread = start_worker(0);
    int gone = worker_tid;
    if (create(SIGEV_THREAD_ID, SIGUSR1, gone, &id) != 0) exit(75);
    release_and_join(thread);
    arm(id, 1000000, 1000000);
    for (int i = 0; i < 20; i++) nap();
    struct itimerspec left;
    if (syscall(SYS_timer_gettime, id, &left) != 0) exit(76);
    printf("exited hits=%d armed=%d\n", (int)hits,
           left.it_value.tv_sec != 0 || left.it_value.tv_nsec != 0);
  } else if (!strcmp(mode, "self")) {
    pthread_t thread = start_worker(0);
    int me = gettid_raw();
    if (create(SIGEV_THREAD_ID, SIGUSR1, me, &id) != 0) exit(75);
    arm(id, 1000, 0);
    for (int i = 0; i < 20 && !hits; i++) nap();
    release_and_join(thread);
    printf("self hits=%d on_main=%d\n", (int)hits, hit_tid == me);
  } else if (!strcmp(mode, "napping_main")) {
    pthread_t thread = start_worker(SIGUSR1);
    if (create(SIGEV_THREAD_ID, SIGUSR1, worker_tid, &id) != 0) exit(75);
    arm(id, 1000, 0);
    __atomic_store_n(&worker_go, 1, __ATOMIC_SEQ_CST);
    int main_hits = 0;
    while (!__atomic_load_n(&worker_done, __ATOMIC_SEQ_CST)) {
      nap();
      if (hits) main_hits = hit_tid != worker_tid;
    }
    if (pthread_join(thread, NULL) != 0) exit(74);
    printf("napping_main hits=%d on_worker=%d main_took=%d pending_on_worker=%d\n", (int)hits,
           hit_tid == worker_tid, main_hits, worker_saw_pending);
  } else if (!strcmp(mode, "overrun")) {
    mask(SIG_BLOCK, SIGUSR1);
    if (create(SIGEV_THREAD_ID, SIGUSR1, gettid_raw(), &id) != 0) exit(75);
    /* Expirations at 1, 4, 7 and 10 ms; the next is 3 ms after the naps. */
    arm(id, 1000000, 3000000);
    for (int i = 0; i < 10; i++) nap();
    /* Take the pending signal before disarming: a disarm discards it. */
    mask(SIG_UNBLOCK, SIGUSR1);
    int taken = hits;
    struct itimerspec off;
    memset(&off, 0, sizeof off);
    if (syscall(SYS_timer_settime, id, 0, &off, NULL) != 0) exit(72);
    printf("overrun hits=%d\n", taken);
  } else if (!strcmp(mode, "coalesce_self")) {
    int me = gettid_raw();
    mask(SIG_BLOCK, SIGUSR1);
    syscall(SYS_tgkill, getpid(), me, SIGUSR1);
    if (create(SIGEV_THREAD_ID, SIGUSR1, me, &id) != 0) exit(75);
    arm(id, 1000, 0);
    for (int i = 0; i < 5; i++) nap();
    mask(SIG_UNBLOCK, SIGUSR1);
    printf("coalesce_self hits=%d\n", (int)hits);
  } else if (!strcmp(mode, "two_timers_self")) {
    int me = gettid_raw();
    int id2;
    mask(SIG_BLOCK, SIGUSR1);
    if (create(SIGEV_THREAD_ID, SIGUSR1, me, &id) != 0) exit(75);
    if (create(SIGEV_THREAD_ID, SIGUSR1, me, &id2) != 0) exit(75);
    arm(id, 1000, 0);
    arm(id2, 2000, 0);
    for (int i = 0; i < 5; i++) nap();
    mask(SIG_UNBLOCK, SIGUSR1);
    printf("two_timers_self hits=%d\n", (int)hits);
  } else if (!strcmp(mode, "pending_then_raise")) {
    int me = gettid_raw();
    mask(SIG_BLOCK, SIGUSR1);
    if (create(SIGEV_THREAD_ID, SIGUSR1, me, &id) != 0) exit(75);
    arm(id, 1000, 0);
    for (int i = 0; i < 3; i++) nap();
    syscall(SYS_tgkill, getpid(), me, SIGUSR1);
    arm(id, 1000, 0);
    for (int i = 0; i < 3; i++) nap();
    mask(SIG_UNBLOCK, SIGUSR1);
    printf("pending_then_raise hits=%d\n", (int)hits);
  } else if (!strcmp(mode, "raise_rearm") || !strcmp(mode, "pidfd_leader")) {
    int me = gettid_raw();
    if (create(SIGEV_THREAD_ID, SIGUSR1, me, &id) != 0) exit(75);
    arm(id, 1000, 0);
    for (int i = 0; i < 20 && !hits; i++) nap();
    int first = hits;
    mask(SIG_BLOCK, SIGUSR1);
    if (!strcmp(mode, "raise_rearm")) {
      syscall(SYS_tgkill, getpid(), me, SIGUSR1);
    } else {
      pthread_t t;
      if (pthread_create(&t, NULL, pidfd_sender, NULL) != 0) exit(73);
      if (pthread_join(t, NULL) != 0 || sender_rc) exit(77);
    }
    arm(id, 1000, 0);
    for (int i = 0; i < 3; i++) nap();
    mask(SIG_UNBLOCK, SIGUSR1);
    printf("%s first=%d hits=%d\n", mode, first, (int)hits);
  } else if (!strcmp(mode, "tgkill_cross")) {
    /* The worker takes the first copy unblocked, then blocks SIGUSR1. */
    pthread_t thread = start_worker(0);
    if (create(SIGEV_THREAD_ID, SIGUSR1, worker_tid, &id) != 0) exit(75);
    arm(id, 1000, 0);
    for (int i = 0; i < 20 && !hits; i++) nap();
    int first = hits;
    worker_blocks = SIGUSR1;
    __atomic_store_n(&worker_go, 2, __ATOMIC_SEQ_CST);
    while (__atomic_load_n(&worker_go, __ATOMIC_SEQ_CST) != 3) nap();
    syscall(SYS_tgkill, getpid(), worker_tid, SIGUSR1);
    arm(id, 1000, 0);
    for (int i = 0; i < 3; i++) nap();
    __atomic_store_n(&worker_go, 4, __ATOMIC_SEQ_CST);
    if (pthread_join(thread, NULL) != 0) exit(74);
    printf("tgkill_cross first=%d hits=%d\n", first, (int)hits);
  } else if (!strcmp(mode, "ign_flush_efault")) {
    int me = gettid_raw();
    mask(SIG_BLOCK, SIGUSR1);
    if (create(SIGEV_THREAD_ID, SIGUSR1, me, &id) != 0) exit(75);
    arm(id, 1000, 0);
    for (int i = 0; i < 3; i++) nap();
    struct {
      void *handler;
      unsigned long flags;
      void *restorer;
      unsigned long mask;
    } ignore = {SIG_IGN, 0, NULL, 0};
    long rc = syscall(SYS_rt_sigaction, SIGUSR1, &ignore, (void *)8, 8);
    int error = errno;
    catch_signal(SIGUSR1);
    syscall(SYS_tgkill, getpid(), me, SIGUSR1);
    arm(id, 1000, 0);
    for (int i = 0; i < 3; i++) nap();
    mask(SIG_UNBLOCK, SIGUSR1);
    printf("ign_flush_efault rc=%s hits=%d\n", rc ? strerrorname_np(error) : "0", (int)hits);
  } else if (!strcmp(mode, "ign_flush") || !strcmp(mode, "ign_flush_noraise") ||
             !strcmp(mode, "dfl_flush") || !strcmp(mode, "stw_zero")) {
    int sig = !strcmp(mode, "dfl_flush") ? SIGURG : SIGUSR1;
    int me = gettid_raw();
    catch_signal(sig);
    mask(SIG_BLOCK, sig);
    if (create(SIGEV_THREAD_ID, sig, me, &id) != 0) exit(75);
    arm(id, 1000, 0);
    for (int i = 0; i < 3; i++) nap();
    if (!strcmp(mode, "stw_zero")) {
      sigset_t set;
      sigemptyset(&set);
      sigaddset(&set, sig);
      struct timespec zero = {0, 0};
      if (sigtimedwait(&set, NULL, &zero) != sig) exit(78);
    } else {
      signal(sig, !strcmp(mode, "dfl_flush") ? SIG_DFL : SIG_IGN);
      catch_signal(sig);
    }
    if (strcmp(mode, "ign_flush_noraise")) syscall(SYS_tgkill, getpid(), me, sig);
    arm(id, 1000, 0);
    for (int i = 0; i < 3; i++) nap();
    mask(SIG_UNBLOCK, sig);
    printf("%s hits=%d\n", mode, (int)hits);
  } else if (!strcmp(mode, "cont_flush") || !strcmp(mode, "tstp_flush")) {
    int cont = !strcmp(mode, "cont_flush");
    int timer_sig = cont ? SIGTSTP : SIGCONT;
    int flusher = cont ? SIGCONT : SIGTSTP;
    int me = gettid_raw();
    catch_signal(SIGTSTP);
    catch_signal(SIGCONT);
    mask(SIG_BLOCK, timer_sig);
    if (!cont) mask(SIG_BLOCK, SIGTSTP);
    if (create(SIGEV_THREAD_ID, timer_sig, me, &id) != 0) exit(75);
    arm(id, 1000, 0);
    for (int i = 0; i < 3; i++) nap();
    syscall(SYS_tgkill, getpid(), me, flusher);
    syscall(SYS_tgkill, getpid(), me, timer_sig);
    arm(id, 1000, 0);
    for (int i = 0; i < 3; i++) nap();
    int before = hits;
    mask(SIG_UNBLOCK, SIGTSTP);
    mask(SIG_UNBLOCK, SIGCONT);
    printf("%s before=%d hits=%d\n", mode, before, (int)hits);
  } else if (!strcmp(mode, "signalfd_overrun")) {
    sigset_t set;
    sigemptyset(&set);
    sigaddset(&set, SIGUSR1);
    mask(SIG_BLOCK, SIGUSR1);
    int fd = signalfd(-1, &set, SFD_NONBLOCK);
    if (fd < 0) exit(79);
    if (create(SIGEV_THREAD_ID, SIGUSR1, gettid_raw(), &id) != 0) exit(75);
    arm(id, 1000, 0);
    for (int i = 0; i < 3; i++) nap();
    arm(id, 1000, 0);
    for (int i = 0; i < 3; i++) nap();
    close(fd);
    mask(SIG_UNBLOCK, SIGUSR1);
    printf("signalfd_overrun hits=%d\n", (int)hits);
  } else if (!strcmp(mode, "rt_blocked_worker")) {
    int rt = SIGRTMIN + 2;
    catch_signal(rt);
    pthread_t thread = start_worker(rt);
    if (create(SIGEV_THREAD_ID, rt, worker_tid, &id) != 0) exit(75);
    arm(id, 1000, 0);
    release_and_join(thread);
    for (int i = 0; i < 5; i++) nap();
    printf("rt_blocked_worker hits=%d\n", (int)hits);
  } else {
    return 64;
  }
  return 0;
}
