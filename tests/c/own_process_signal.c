/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/* Guest for hermit-cli/tests/cli.rs: a process-directed signal a thread sends
 * to its own multi-threaded process with kill or rt_sigqueueinfo
 * (https://github.com/rrnewton/hermit/issues/4034).
 *
 * usage: own_process_signal <mode> [signal]
 *
 * Every mode runs with a second thread, so the process has two live threads
 * when it sends. A worker that idles waits on a condition variable (a futex
 * wait, in the schedule) until the main thread releases it at the end.
 *
 * Admitted modes print one line per send:
 *   <mode> sig=<n> rc=<r> ran_before_return=<0|1> on_sender=<0|1> code=<si_code>
 *   pid_ok=<0|1> uid_ok=<0|1>
 * and Hermit must print what Linux prints.
 *   leader:   the main thread sends kill(getpid(), sig) while a worker idles
 *             (sig defaults to SIGUSR1; `all` sends each of the seven admitted
 *             signals in turn).
 *   tid:      a worker sends kill(gettid(), SIGUSR1) while the main thread
 *             waits in pthread_join with SIGUSR1 unblocked.
 *   blocked:  a worker sends kill(getpid(), SIGUSR1) while the main thread,
 *             which blocks SIGUSR1, waits in pthread_join.
 *   sibling_tid: the main thread sends kill(<worker's id>, SIGUSR1) while the
 *             worker, which blocks SIGUSR1, idles: the id names the process
 *             (https://github.com/rrnewton/hermit/issues/4094), and the worker
 *             does not want the signal, so the main thread takes it.
 *   ignored:  the main thread sends a SIGUSR1 set to SIG_IGN: rc 0, no handler.
 *   queue:    the main thread sends rt_sigqueueinfo(getpid(), SIGUSR1) with
 *             SI_QUEUE and a value; the handler checks code and value.
 *   forged:   a worker sends rt_sigqueueinfo(getpid(), SIGUSR1) with SI_USER,
 *             which only the named thread may use: rc -1 EPERM.
 *   busy <sends> <step>: a runnable worker loops on getppid with SIGUSR1
 *             unblocked while the main thread sends kill(getpid(), SIGUSR1)
 *             <sends> times, each after a spin <step> iterations longer than
 *             the last, so the timeslice can end at any point around the send.
 *             Prints one line counting the sends whose handler ran on the main
 *             thread before kill returned; Linux gives each to the main thread,
 *             which the call names and which is running.
 *   timer_taken: a one-shot POSIX timer for SIGHUP expires and a thread
 *             takes its signal (the scheduler's copy), then the main thread
 *             sends kill(getpid(), SIGTERM), which it catches.
 *   merged:   with SIGHUP blocked, a child's kill(getppid(), SIGHUP) waits in
 *             the shared queue, then a POSIX timer for SIGHUP expires. Under
 *             Hermit, which emulates the timer with kill(), its copy merges
 *             into the child's (one delivery, with the child's siginfo); Linux
 *             queues the timer's own siginfo and delivers both. SIGHUP is
 *             unblocked, then the main thread sends kill(getpid(), SIGTERM);
 *             only that send is printed.
 *   sigwaited: as merged without the child, but sigtimedwait takes the
 *             timer's SIGHUP, so it never reaches a delivery stop.
 *
 * Refused modes send once and print `<mode> rc=<r> <errno>`; under
 * --allow-unsupported-syscalls a refused send returns -1 ENOSYS.
 *   sender_is_not_sole: a worker sends kill(getpid(), SIGUSR1) while the main
 *             thread waits with SIGUSR1 unblocked.
 *   sender_blocks: the main thread blocks SIGUSR1 and sends it.
 *   sibling_tid_unblocked: as sibling_tid, but the worker does not block
 *             SIGUSR1, and Linux prefers the thread the id names.
 *   not_admitted <n>: the main thread catches signal n and sends it.
 *   default:  the main thread sends SIGTERM, which it does not catch.
 *   owner_tid: F_SETOWN_EX naming the main thread and F_SETSIG(SIGUSR2) on a
 *             pipe, then kill(getpid(), SIGUSR1). With `blocked` as the second
 *             argument the main thread first blocks SIGIO, SIGURG and SIGUSR2.
 *   owner_pid: F_SETOWN with the pid on a pipe (SIGIO), then the send.
 *   pdeathsig: the worker arms and disarms PR_SET_PDEATHSIG(SIGUSR2), then the
 *             main thread sends.
 *   stop:     the main thread sends SIGTSTP to a child that ignores it, then
 *             sends; with `blocked` it first blocks SIGHUP and SIGCONT.
 *   exit_signal: a raw clone child with SIGUSR2 as its exit signal; the send
 *             before the child is reaped is refused, and after it (`reaped`)
 *             it is admitted.
 *   timer:    a one-shot POSIX timer for SIGHUP 1 us away, a loop with no
 *             system call, then kill(getpid(), SIGTERM), which it catches.
 *   signalfd: a signalfd for SIGUSR2, then the send.
 *   marker:   the main thread sends SIGSTKFLT, Reverie's timer marker, to a
 *             zombie child (the send has no effect), then sends.
 *   wo_queue <n>: the main thread sends rt_sigqueueinfo(getpid(), n) with
 *             SI_QUEUE and a siginfo_t in a write-only page, which the kernel
 *             reads (Codex review of https://github.com/rrnewton/hermit/pull/4116).
 *   vvar_queue: the main thread sends rt_sigqueueinfo(getpid(), SIGUSR1)
 *             with its siginfo_t pointer at the vDSO's [vvar] page, which the
 *             kernel copies from (Linux then answers E2BIG for the unknown
 *             si_code) and the tracer cannot read.
 *   straddle_queue: the main thread sends rt_sigqueueinfo(getpid(), SIGWINCH,
 *             caught) with SI_QUEUE whose siginfo_t starts 48 bytes before an
 *             unmapped page: the kernel copies only those 48 bytes for a known
 *             layout, so Linux delivers it.
 *   prot_none_queue <n>: the main thread fills a valid SI_QUEUE siginfo_t
 *             for n, makes its page PROT_NONE and sends
 *             rt_sigqueueinfo(getpid(), n): the kernel's copy faults, so
 *             Linux answers -1 EFAULT and sends nothing.
 *   growsdown_queue <n>: the main thread sends rt_sigqueueinfo(getpid(), n)
 *             with its siginfo_t pointer one page below a MAP_GROWSDOWN
 *             mapping, in the hole: the kernel's copy grows the mapping and
 *             Linux delivers the zero siginfo (SI_USER from the caller).
 *   pending <n>: a worker sends pthread_kill(main, n), calls getppid and
 *             parks until released, while the main thread sends
 *             kill(getpid(), SIGUSR1); both signals are caught. The CLI test
 *             orders the two with happens-before edges so that the worker's
 *             send lands while the main thread is stopped at its kill's entry,
 *             with n then pending on its private queue.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/prctl.h>
#include <sys/signalfd.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

/* The handler makes no system call, so it does not shift a thread's syscall
   count (the forced-order test anchors on those counts). */
static volatile pthread_t ran_thread;
static volatile sig_atomic_t ran, ran_code, ran_pid, ran_uid, ran_value, returned,
    ran_before_return;

static void on_signal(int sig, siginfo_t *info, void *context) {
  (void)sig;
  (void)context;
  ran += 1;
  ran_thread = pthread_self();
  ran_code = info->si_code;
  ran_pid = info->si_pid;
  ran_uid = (sig_atomic_t)info->si_uid;
  ran_value = info->si_value.sival_int;
  ran_before_return = !returned;
}

/* `stolen` mode: a worker's SIGHUP handler sleeps (with SIGHUP blocked), the
 * happens-before point the CLI test holds the main thread's send behind. */
static void on_stolen_hup(int sig) {
  (void)sig;
  struct timespec nap = {0, 1000000};
  syscall(SYS_nanosleep, &nap, NULL);
}

static void catch_signal(int sig) {
  struct sigaction action;
  memset(&action, 0, sizeof action);
  action.sa_sigaction = on_signal;
  action.sa_flags = SA_SIGINFO;
  if (sigaction(sig, &action, NULL) != 0) exit(70);
}

static void set_blocked(int how, int a, int b, int c) {
  sigset_t set;
  sigemptyset(&set);
  if (a) sigaddset(&set, a);
  if (b) sigaddset(&set, b);
  if (c) sigaddset(&set, c);
  if (pthread_sigmask(how, &set, NULL) != 0) exit(71);
}

static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t cond = PTHREAD_COND_INITIALIZER;
static int released;

static void *idle(void *unused) {
  (void)unused;
  pthread_mutex_lock(&lock);
  while (!released) pthread_cond_wait(&cond, &lock);
  pthread_mutex_unlock(&lock);
  return NULL;
}

static pthread_t idler;

static void start_idler(void) {
  if (pthread_create(&idler, NULL, idle, NULL) != 0) exit(72);
}

static pid_t idler_tid;

static void *idle_with_tid(void *unused) {
  (void)unused;
  pthread_mutex_lock(&lock);
  idler_tid = (pid_t)syscall(SYS_gettid);
  pthread_cond_broadcast(&cond);
  while (!released) pthread_cond_wait(&cond, &lock);
  pthread_mutex_unlock(&lock);
  return NULL;
}

/* Start an idler that blocks SIGUSR1 or not, and return its thread id. */
static pid_t start_idler_with_tid(int blocks_usr1) {
  sigset_t mask;
  sigemptyset(&mask);
  if (blocks_usr1) sigaddset(&mask, SIGUSR1);
  pthread_attr_t attr;
  pthread_attr_init(&attr);
  pthread_attr_setsigmask_np(&attr, &mask);
  if (pthread_create(&idler, &attr, idle_with_tid, NULL) != 0) exit(72);
  pthread_mutex_lock(&lock);
  while (!idler_tid) pthread_cond_wait(&cond, &lock);
  pid_t tid = idler_tid;
  pthread_mutex_unlock(&lock);
  return tid;
}

static void release_idler(void) {
  pthread_mutex_lock(&lock);
  released = 1;
  pthread_cond_signal(&cond);
  pthread_mutex_unlock(&lock);
  pthread_join(idler, NULL);
}

static void report_send(const char *mode, int sig, long rc) {
  if (!ran) {
    printf("%s sig=%d rc=%ld ran=0\n", mode, sig, rc);
    return;
  }
  printf("%s sig=%d rc=%ld ran_before_return=%d on_sender=%d code=%d pid_ok=%d uid_ok=%d\n", mode, sig,
         rc, (int)ran_before_return, pthread_equal(ran_thread, pthread_self()), (int)ran_code,
         ran_pid == getpid(), ran_uid == (sig_atomic_t)getuid());
  ran = ran_code = ran_pid = ran_uid = ran_before_return = 0;
  ran_thread = 0;
}

static void report_refused(const char *mode, long rc) {
  printf("%s rc=%ld %s\n", mode, rc, rc < 0 ? strerrorname_np(errno) : "-");
}

static long send_kill(int sig) {
  returned = 0;
  long rc = kill(getpid(), sig);
  returned = 1;
  return rc;
}

static long queue_to_self(int code, int value) {
  siginfo_t info;
  memset(&info, 0, sizeof info);
  info.si_signo = SIGUSR1;
  info.si_code = code;
  info.si_pid = getpid();
  info.si_uid = getuid();
  info.si_value.sival_int = value;
  returned = 0;
  long rc = syscall(SYS_rt_sigqueueinfo, getpid(), SIGUSR1, &info);
  returned = 1;
  return rc;
}

static void *tid_sender(void *unused) {
  (void)unused;
  returned = 0;
  long rc = kill((pid_t)syscall(SYS_gettid), SIGUSR1);
  returned = 1;
  report_send("tid", SIGUSR1, rc);
  return NULL;
}

static void *pid_sender(void *mode) {
  /* Let the main thread leave pthread_create, which blocks every signal
     around the clone, and wait in pthread_join under its own mask. */
  usleep(1000);
  if (!strcmp(mode, "forged")) {
    long rc = queue_to_self(SI_USER, 0);
    report_refused("forged", rc);
    return NULL;
  }
  long rc = send_kill(SIGUSR1);
  if (!strcmp(mode, "blocked")) {
    report_send("blocked", SIGUSR1, rc);
  } else {
    report_refused(mode, rc);
  }
  return NULL;
}

static void *pdeath_worker(void *unused) {
  (void)unused;
  if (prctl(PR_SET_PDEATHSIG, SIGUSR2) != 0) exit(73);
  if (prctl(PR_SET_PDEATHSIG, 0) != 0) exit(73);
  return NULL;
}

/* A worker that sends, while the main thread waits in pthread_join. */
static int worker_sends(const char *mode, void *(*sender)(void *)) {
  pthread_t worker;
  if (pthread_create(&worker, NULL, sender, (void *)mode) != 0) return 72;
  pthread_join(worker, NULL);
  return 0;
}

static volatile int busy_done;

static void *busy_worker(void *unused) {
  (void)unused;
  while (!busy_done) syscall(SYS_getppid);
  return NULL;
}

static int busy_main(int sends, long step) {
  catch_signal(SIGUSR1);
  pthread_t worker;
  if (pthread_create(&worker, NULL, busy_worker, NULL) != 0) return 72;
  for (int i = 0; i < 100; i++) sched_yield();
  int on_sender = 0;
  for (int k = 0; k < sends; k++) {
    for (volatile long spin = 0; spin < step * k; spin++) {
    }
    ran = 0;
    ran_thread = 0;
    long rc = send_kill(SIGUSR1);
    if (rc == 0 && ran == 1 && ran_before_return &&
        pthread_equal(ran_thread, pthread_self())) {
      on_sender++;
    }
  }
  busy_done = 1;
  pthread_join(worker, NULL);
  printf("busy sends=%d on_sender_before_return=%d\n", sends, on_sender);
  return 0;
}

static pthread_t main_thread_id;
static int pending_signal;

static void *pending_worker(void *unused) {
  (void)unused;
  pthread_kill(main_thread_id, pending_signal);
  syscall(SYS_getppid);
  /* Stay a live thread, parked, until the main thread has sent. */
  return idle(NULL);
}

static const int admitted[] = {SIGHUP, SIGINT, SIGQUIT, SIGUSR1, SIGUSR2, SIGPIPE, SIGTERM};

static int stolen_armed;

/* `stolen` mode's worker starts with SIGHUP blocked, waits until the main
 * thread has armed the timer, spins past its expiry, queues its own SIGHUP
 * with tgkill and unblocks it: it takes its own copy, from its private queue,
 * first, and its handler (SIGHUP blocked again) sleeps. */
static void *stolen_worker(void *unused) {
  (void)unused;
  pthread_mutex_lock(&lock);
  while (!stolen_armed) pthread_cond_wait(&cond, &lock);
  pthread_mutex_unlock(&lock);
  for (volatile int spin = 0; spin < 5000000; spin++) {
  }
  syscall(SYS_tgkill, getpid(), (pid_t)syscall(SYS_gettid), SIGHUP);
  set_blocked(SIG_UNBLOCK, SIGHUP, 0, 0);
  return NULL;
}

int main(int argc, char **argv) {
  setvbuf(stdout, NULL, _IONBF, 0);
  if (argc < 2) return 2;
  const char *mode = argv[1];
  const char *arg = argc > 2 ? argv[2] : "";

  if (!strcmp(mode, "leader") || !strcmp(mode, "all")) {
    start_idler();
    for (size_t i = 0; i < sizeof admitted / sizeof admitted[0]; i++) {
      int sig = admitted[i];
      if (!strcmp(mode, "leader") && sig != SIGUSR1) continue;
      catch_signal(sig);
      report_send(mode, sig, send_kill(sig));
    }
    release_idler();
    return 0;
  }
  if (!strcmp(mode, "pending")) {
    pending_signal = atoi(arg);
    catch_signal(SIGUSR1);
    catch_signal(pending_signal);
    main_thread_id = pthread_self();
    pthread_t worker;
    if (pthread_create(&worker, NULL, pending_worker, NULL) != 0) return 72;
    long rc = send_kill(SIGUSR1);
    report_refused(mode, rc);
    pthread_mutex_lock(&lock);
    released = 1;
    pthread_cond_signal(&cond);
    pthread_mutex_unlock(&lock);
    pthread_join(worker, NULL);
    return 0;
  }
  if (!strcmp(mode, "busy")) {
    return busy_main(atoi(arg), argc > 3 ? atol(argv[3]) : 1);
  }
  if (!strcmp(mode, "sibling_tid") || !strcmp(mode, "sibling_tid_unblocked")) {
    int blocks = !strcmp(mode, "sibling_tid");
    catch_signal(SIGUSR1);
    pid_t tid = start_idler_with_tid(blocks);
    returned = 0;
    long rc = kill(tid, SIGUSR1);
    returned = 1;
    if (blocks) {
      report_send(mode, SIGUSR1, rc);
    } else {
      report_refused(mode, rc);
    }
    release_idler();
    return 0;
  }
  if (!strcmp(mode, "tid")) {
    catch_signal(SIGUSR1);
    return worker_sends(mode, tid_sender);
  }
  if (!strcmp(mode, "blocked")) {
    catch_signal(SIGUSR1);
    set_blocked(SIG_BLOCK, SIGUSR1, 0, 0);
    pthread_t worker;
    /* The worker starts with the main thread's mask; it unblocks SIGUSR1. */
    sigset_t none;
    sigemptyset(&none);
    pthread_attr_t attr;
    pthread_attr_init(&attr);
    pthread_attr_setsigmask_np(&attr, &none);
    if (pthread_create(&worker, &attr, pid_sender, (void *)mode) != 0) return 72;
    pthread_join(worker, NULL);
    return 0;
  }
  if (!strcmp(mode, "ignored")) {
    signal(SIGUSR1, SIG_IGN);
    start_idler();
    report_send(mode, SIGUSR1, send_kill(SIGUSR1));
    release_idler();
    return 0;
  }
  if (!strcmp(mode, "queue")) {
    catch_signal(SIGUSR1);
    start_idler();
    long rc = queue_to_self(SI_QUEUE, 42);
    printf("queue rc=%ld ran_before_return=%d on_sender=%d code=%d value=%d\n", rc,
           (int)ran_before_return, pthread_equal(ran_thread, pthread_self()), (int)ran_code,
           (int)ran_value);
    release_idler();
    return 0;
  }
  if (!strcmp(mode, "prot_none_queue")) {
    int sig = atoi(arg);
    catch_signal(sig);
    siginfo_t *info = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (info == MAP_FAILED) return 83;
    memset(info, 0, sizeof *info);
    info->si_signo = sig;
    info->si_code = SI_QUEUE;
    info->si_pid = getpid();
    info->si_uid = getuid();
    if (mprotect(info, 4096, PROT_NONE) != 0) return 83;
    start_idler();
    long rc = syscall(SYS_rt_sigqueueinfo, getpid(), sig, info);
    report_refused(mode, rc);
    release_idler();
    return 0;
  }
  if (!strcmp(mode, "growsdown_queue")) {
    int sig = atoi(arg);
    catch_signal(sig);
    char *base = mmap((void *)0x500000000000UL, 4096, PROT_READ | PROT_WRITE,
                      MAP_PRIVATE | MAP_ANONYMOUS | MAP_GROWSDOWN | MAP_FIXED_NOREPLACE, -1, 0);
    if (base == MAP_FAILED) return 84;
    start_idler();
    long rc = syscall(SYS_rt_sigqueueinfo, getpid(), sig, base - 4096 + 64);
    report_refused(mode, rc);
    release_idler();
    return 0;
  }
  if (!strcmp(mode, "forged")) {
    catch_signal(SIGUSR1);
    return worker_sends(mode, pid_sender);
  }
  if (!strcmp(mode, "sender_is_not_sole")) {
    catch_signal(SIGUSR1);
    return worker_sends(mode, pid_sender);
  }

  /* The remaining refused modes send from the main thread with a worker idle. */
  int sig = SIGUSR1;
  int fds[2] = {-1, -1};
  pid_t child = -1;
  if (!strcmp(mode, "sender_blocks")) {
    catch_signal(SIGUSR1);
    set_blocked(SIG_BLOCK, SIGUSR1, 0, 0);
  } else if (!strcmp(mode, "not_admitted")) {
    sig = atoi(arg);
    catch_signal(sig);
  } else if (!strcmp(mode, "default")) {
    sig = SIGTERM;
  } else if (!strcmp(mode, "owner_tid") || !strcmp(mode, "owner_pid")) {
    catch_signal(SIGUSR1);
    if (pipe(fds) != 0) return 74;
    if (!strcmp(mode, "owner_tid")) {
      struct f_owner_ex owner = {F_OWNER_TID, (pid_t)syscall(SYS_gettid)};
      if (fcntl(fds[0], F_SETOWN_EX, &owner) != 0) return 74;
      if (fcntl(fds[0], F_SETSIG, SIGUSR2) != 0) return 74;
    } else if (fcntl(fds[0], F_SETOWN, getpid()) != 0) {
      return 74;
    }
    if (!strcmp(arg, "blocked")) set_blocked(SIG_BLOCK, SIGIO, SIGURG, SIGUSR2);
  } else if (!strcmp(mode, "pdeathsig")) {
    catch_signal(SIGUSR1);
    pthread_t worker;
    if (pthread_create(&worker, NULL, pdeath_worker, NULL) != 0) return 72;
    pthread_join(worker, NULL);
    if (!strcmp(arg, "blocked")) set_blocked(SIG_BLOCK, SIGUSR2, 0, 0);
  } else if (!strcmp(mode, "stop")) {
    catch_signal(SIGUSR1);
    int up[2], down[2];
    if (pipe(up) != 0 || pipe(down) != 0) return 74;
    child = fork();
    if (child < 0) return 75;
    if (child == 0) {
      signal(SIGTSTP, SIG_IGN);
      char byte;
      if (write(up[1], "x", 1) != 1) _exit(1);
      close(down[1]);
      while (read(down[0], &byte, 1) > 0) {
      }
      _exit(0);
    }
    char byte;
    if (read(up[0], &byte, 1) != 1) return 76;
    if (kill(child, SIGTSTP) != 0) return 76;
    close(down[1]);
    waitpid(child, NULL, 0);
    child = -1;
    if (!strcmp(arg, "blocked")) set_blocked(SIG_BLOCK, SIGHUP, SIGCONT, 0);
  } else if (!strcmp(mode, "exit_signal")) {
    catch_signal(SIGUSR1);
    catch_signal(SIGUSR2);
    long raw = syscall(SYS_clone, (unsigned long)SIGUSR2, NULL, NULL, NULL, NULL);
    if (raw < 0) return 77;
    if (raw == 0) _exit(0);
    child = (pid_t)raw;
    if (!strcmp(arg, "reaped")) {
      if (waitpid(child, NULL, __WALL) != child) return 77;
      child = -1;
    }
  } else if (!strcmp(mode, "timer")) {
    sig = SIGTERM;
    catch_signal(SIGTERM);
    catch_signal(SIGHUP);
    timer_t timer;
    struct sigevent event;
    memset(&event, 0, sizeof event);
    event.sigev_notify = SIGEV_SIGNAL;
    event.sigev_signo = SIGHUP;
    if (timer_create(CLOCK_MONOTONIC, &event, &timer) != 0) return 78;
    struct itimerspec spec;
    memset(&spec, 0, sizeof spec);
    spec.it_value.tv_nsec = 1000;
    start_idler();
    if (timer_settime(timer, 0, &spec, NULL) != 0) return 78;
    /* No system call between the arming and the send. */
    for (volatile int spin = 0; spin < 20000; spin++) {
    }
    long rc = send_kill(sig);
    report_refused(mode, rc);
    release_idler();
    timer_delete(timer);
    return 0;
  } else if (!strcmp(mode, "timer_taken")) {
    catch_signal(SIGTERM);
    catch_signal(SIGHUP);
    timer_t timer;
    struct sigevent event;
    memset(&event, 0, sizeof event);
    event.sigev_notify = SIGEV_SIGNAL;
    event.sigev_signo = SIGHUP;
    if (timer_create(CLOCK_MONOTONIC, &event, &timer) != 0) return 78;
    struct itimerspec spec;
    memset(&spec, 0, sizeof spec);
    spec.it_value.tv_nsec = 1000;
    start_idler();
    if (timer_settime(timer, 0, &spec, NULL) != 0) return 78;
    /* Wait until a thread has taken the timer's SIGHUP. */
    while (!ran) {
      struct timespec nap = {0, 1000000};
      nanosleep(&nap, NULL);
    }
    timer_delete(timer);
    ran = ran_code = ran_pid = ran_uid = ran_before_return = 0;
    ran_thread = 0;
    report_send(mode, SIGTERM, send_kill(SIGTERM));
    release_idler();
    return 0;
  } else if (!strcmp(mode, "merged") || !strcmp(mode, "sigwaited")) {
    catch_signal(SIGTERM);
    catch_signal(SIGHUP);
    set_blocked(SIG_BLOCK, SIGHUP, 0, 0);
    if (!strcmp(mode, "merged")) {
      /* A child's SIGHUP waits in the shared queue first. */
      pid_t parent = getpid();
      pid_t child = fork();
      if (child == 0) _exit(kill(parent, SIGHUP) != 0);
      int status;
      if (waitpid(child, &status, 0) != child || status != 0) return 79;
    }
    timer_t timer;
    struct sigevent event;
    memset(&event, 0, sizeof event);
    event.sigev_notify = SIGEV_SIGNAL;
    event.sigev_signo = SIGHUP;
    if (timer_create(CLOCK_MONOTONIC, &event, &timer) != 0) return 78;
    struct itimerspec spec;
    memset(&spec, 0, sizeof spec);
    spec.it_value.tv_nsec = 1000;
    if (timer_settime(timer, 0, &spec, NULL) != 0) return 78;
    /* Sleep well past the expiry: Hermit's kill() copy for the timer merges
     * into the child's (Linux queues the timer's separately), or waits
     * alone. */
    for (int i = 0; i < 20; i++) {
      struct timespec nap = {0, 1000000};
      nanosleep(&nap, NULL);
    }
    if (!strcmp(mode, "sigwaited")) {
      /* sigtimedwait takes the scheduler's copy; no delivery stop. */
      sigset_t set;
      sigemptyset(&set);
      sigaddset(&set, SIGHUP);
      struct timespec zero = {0, 0};
      if (sigtimedwait(&set, NULL, &zero) != SIGHUP) return 80;
    }
    start_idler();
    /* Under Hermit one SIGHUP is delivered here (Linux delivers the timer's
     * as well in merged mode); either way the handler takes them before
     * the send. */
    set_blocked(SIG_UNBLOCK, SIGHUP, 0, 0);
    ran = ran_code = ran_pid = ran_uid = ran_before_return = 0;
    ran_thread = 0;
    report_send(mode, SIGTERM, send_kill(SIGTERM));
    release_idler();
    /* Deleted last: Linux flushes a deleted timer's pending signal. */
    timer_delete(timer);
    return 0;
  } else if (!strcmp(mode, "stolen")) {
    /* The timer's SIGHUP (the scheduler's copy) reaches the shared queue
     * while the main thread is held at its kill; the worker takes its own
     * tgkill copy of SIGHUP first, so the scheduler's is still pending when
     * the kill is decided (the CLI test holds the kill behind the worker's
     * handler). */
    catch_signal(SIGTERM);
    struct sigaction action;
    memset(&action, 0, sizeof action);
    action.sa_handler = on_stolen_hup;
    if (sigaction(SIGHUP, &action, NULL) != 0) return 70;
    timer_t timer;
    struct sigevent event;
    memset(&event, 0, sizeof event);
    event.sigev_notify = SIGEV_SIGNAL;
    event.sigev_signo = SIGHUP;
    if (timer_create(CLOCK_MONOTONIC, &event, &timer) != 0) return 78;
    sigset_t hup;
    sigemptyset(&hup);
    sigaddset(&hup, SIGHUP);
    pthread_attr_t attr;
    pthread_attr_init(&attr);
    pthread_attr_setsigmask_np(&attr, &hup);
    pthread_t worker;
    if (pthread_create(&worker, &attr, stolen_worker, NULL) != 0) return 72;
    struct itimerspec spec;
    memset(&spec, 0, sizeof spec);
    spec.it_value.tv_nsec = 500000;
    if (timer_settime(timer, 0, &spec, NULL) != 0) return 78;
    pthread_mutex_lock(&lock);
    stolen_armed = 1;
    pthread_cond_broadcast(&cond);
    pthread_mutex_unlock(&lock);
    long rc = send_kill(SIGTERM);
    report_refused(mode, rc);
    pthread_join(worker, NULL);
    timer_delete(timer);
    return 0;
  } else if (!strcmp(mode, "wo_queue")) {
    sig = atoi(arg);
    catch_signal(sig);
    siginfo_t *page = mmap(NULL, 4096, PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (page == MAP_FAILED) return 80;
    memset(page, 0, sizeof *page);
    page->si_signo = sig;
    page->si_code = SI_QUEUE;
    page->si_pid = getpid();
    page->si_uid = getuid();
    start_idler();
    returned = 0;
    long rc = syscall(SYS_rt_sigqueueinfo, getpid(), sig, page);
    returned = 1;
    report_refused(mode, rc);
    release_idler();
    return 0;
  } else if (!strcmp(mode, "vvar_queue")) {
    catch_signal(SIGUSR1);
    FILE *maps = fopen("/proc/self/maps", "r");
    if (maps == NULL) return 81;
    char line[512];
    unsigned long vvar = 0;
    while (fgets(line, sizeof line, maps) != NULL) {
      if (strstr(line, "[vvar]") != NULL) {
        vvar = strtoul(line, NULL, 16);
        break;
      }
    }
    fclose(maps);
    if (vvar == 0) {
      printf("vvar_queue no [vvar] mapping\n");
      return 0;
    }
    start_idler();
    long rc = syscall(SYS_rt_sigqueueinfo, getpid(), SIGUSR1, (void *)vvar);
    report_refused(mode, rc);
    release_idler();
    return 0;
  } else if (!strcmp(mode, "straddle_queue")) {
    catch_signal(SIGWINCH);
    char *pages = mmap(NULL, 8192, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (pages == MAP_FAILED || munmap(pages + 4096, 4096) != 0) return 82;
    siginfo_t *info = (siginfo_t *)(pages + 4096 - 48);
    memset(info, 0, 48);
    info->si_signo = SIGWINCH;
    info->si_code = SI_QUEUE;
    info->si_pid = getpid();
    info->si_uid = getuid();
    start_idler();
    returned = 0;
    long rc = syscall(SYS_rt_sigqueueinfo, getpid(), SIGWINCH, info);
    returned = 1;
    report_refused(mode, rc);
    release_idler();
    return 0;
  } else if (!strcmp(mode, "marker")) {
    catch_signal(SIGUSR1);
    child = fork();
    if (child < 0) return 75;
    if (child == 0) _exit(0);
    siginfo_t exited;
    if (waitid(P_PID, child, &exited, WEXITED | WNOWAIT) != 0) return 75;
    /* Linux answers 0; Hermit answers ESRCH for a zombie until
       https://github.com/rrnewton/hermit/issues/4091 is fixed. The send's
       record is made either way. */
    (void)kill(child, SIGSTKFLT);
  } else if (!strcmp(mode, "signalfd")) {
    catch_signal(SIGUSR1);
    sigset_t set;
    sigemptyset(&set);
    sigaddset(&set, SIGUSR2);
    if (signalfd(-1, &set, 0) < 0) return 79;
  } else {
    return 2;
  }
  start_idler();
  long rc = send_kill(sig);
  if (rc == 0 && ran) {
    report_send(mode, sig, rc);
  } else {
    report_refused(mode, rc);
  }
  release_idler();
  if (child > 0) waitpid(child, NULL, __WALL);
  return 0;
}
