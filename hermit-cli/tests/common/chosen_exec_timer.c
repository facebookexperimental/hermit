/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <linux/futex.h>
#include <pthread.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

/* The ignored ordinary send stays physically queued under ptrace while its
 * leader is stopped. A later POSIX timer then has a logical Timer entry
 * standing on a non-SI_TIMER record. Exec removes that entry, so Detcore
 * must take the physical record back before this program's new image runs.
 *
 * The fixture itself checks Linux-visible results. The CLI test additionally
 * requires the production INFO emitted only after the injected, post-exec
 * rt_sigtimedwait returns SIGUSR1. A correct stdout alone is not drain proof.
 * No diagnostic write, host sleep or external I/O occurs between the first
 * signal and exec. Sticky scheduling and syscall-boundary virtual slices
 * are chosen to keep the worker selected after the timer wakes the leader.
 * The CLI test fixes the seed and fails if that schedule misses the branch.
 */

_Static_assert(ATOMIC_INT_LOCK_FREE == 2, "the signal-handler handoff must be lock-free");

extern char **environ;
static int parked_word;
static _Atomic int worker_has_run;
static volatile sig_atomic_t hits;
static int use_ordinary;
static int keep_ordinary;
static int exec_from_handler;
static char *after_argv[5];

static void on_usr1(int signal_number) {
  if (signal_number != SIGUSR1) _exit(61);
  ++hits;
}

static void after_worker_has_run(void) {
  if (!atomic_load_explicit(&worker_has_run, memory_order_acquire)) _exit(62);
  execve("/proc/self/exe", after_argv, environ);
  _exit(63);
}

static void on_hup(int signal_number) {
  /* This marker can only originate in the original leader's actual handler.
   * Its sa_mask already blocks SIGUSR1, and that mask survives exec. The
   * private HUP is dequeued before a shared USR1 on Linux. */
  if (signal_number != SIGHUP || syscall(SYS_gettid) != getpid()) _exit(64);
  after_worker_has_run();
}

static void mask_usr1_and_hup(void) {
  sigset_t set;
  if (sigemptyset(&set) || sigaddset(&set, SIGUSR1) || sigaddset(&set, SIGHUP) ||
      pthread_sigmask(SIG_BLOCK, &set, NULL))
    _exit(65);
}

static void park(void) {
  for (;;) {
    long result = syscall(SYS_futex, &parked_word, FUTEX_WAIT_PRIVATE, 0, NULL, NULL, 0);
    if (result == -1 && errno != EINTR) _exit(66);
  }
}

static void expire_one_posix_timer(void) {
  struct sigevent event;
  memset(&event, 0, sizeof event);
  event.sigev_notify = SIGEV_SIGNAL;
  event.sigev_signo = SIGUSR1;
  event.sigev_value.sival_int = 79;
  int id = -1;
  if (syscall(SYS_timer_create, CLOCK_MONOTONIC, &event, &id)) _exit(67);
  struct itimerspec spec;
  memset(&spec, 0, sizeof spec);
  spec.it_value.tv_nsec = 1;
  if (syscall(SYS_timer_settime, id, 0, &spec, NULL)) _exit(68);
  unsigned attempt;
  for (attempt = 0; attempt != 1024; ++attempt) {
    if (syscall(SYS_timer_gettime, id, &spec)) _exit(69);
    if (spec.it_value.tv_sec == 0 && spec.it_value.tv_nsec == 0) break;
  }
  if (attempt == 1024) _exit(70);
  /* An expired clock read is not the scheduler's timed-pop acknowledgment.
   * These nonblocking, charged syscalls cross logical slice boundaries;
   * the test's exact take-back log remains the required branch oracle. */
  struct timespec now;
  for (unsigned i = 0; i != 16; ++i)
    if (syscall(SYS_clock_gettime, CLOCK_MONOTONIC, &now)) _exit(71);
}

static void *worker(void *unused) {
  (void)unused;
  mask_usr1_and_hup();
  if (syscall(SYS_gettid) == getpid()) _exit(72);
  atomic_store_explicit(&worker_has_run, 1, memory_order_release);

  /* Unlike ITIMER_REAL, this send does not permanently end POSIX SIGUSR1
   * observability. Ptrace keeps kill process-directed; the blocked sender
   * cannot take the physical record on its way back to user space. */
  if (use_ordinary && kill(getpid(), SIGUSR1)) _exit(73);
  struct sigaction action;
  memset(&action, 0, sizeof action);
  action.sa_handler = on_usr1;
  if (sigemptyset(&action.sa_mask) || sigaction(SIGUSR1, &action, NULL)) _exit(74);
  expire_one_posix_timer();

  if (exec_from_handler) {
    if (syscall(SYS_tgkill, getpid(), getpid(), SIGHUP)) _exit(75);
    park();
  }
  after_worker_has_run();
  return NULL;
}

static int new_image(int argc, char **argv) {
  const char *marker = exec_from_handler ? "after-leader-handler" : "after-worker";
  if (argc != 4 || strcmp(argv[2], marker) || strcmp(argv[3], "child-checked")) return 76;
  int replacement_is_leader = syscall(SYS_gettid) == getpid();
  if (!replacement_is_leader) return 77;
  sigset_t pending;
  if (sigpending(&pending)) return 78;
  int was_pending = sigismember(&pending, SIGUSR1);
  struct sigaction action;
  memset(&action, 0, sizeof action);
  action.sa_handler = on_usr1;
  if (sigemptyset(&action.sa_mask) || sigaction(SIGUSR1, &action, NULL)) return 79;
  sigset_t set;
  if (sigemptyset(&set) || sigaddset(&set, SIGUSR1) ||
      pthread_sigmask(SIG_UNBLOCK, &set, NULL))
    return 80;
  if (was_pending != keep_ordinary || hits != keep_ordinary) return 81;
  printf("%s caller=%s child_checked=1 post_exec_leader=%d pending_after_exec=%d hits=%d\n",
         argv[1], exec_from_handler ? "leader-handler" : "worker", replacement_is_leader,
         was_pending, (int)hits);
  return 0;
}

int main(int argc, char **argv) {
  if (argc != 2 && argc != 4) return 82;
  const char *mode = argv[1];
  if (!strcmp(mode, "proxy-nonleader") || !strcmp(mode, "proxy-leader")) {
    use_ordinary = 1;
  } else if (!strcmp(mode, "plain-nonleader") || !strcmp(mode, "plain-leader")) {
    use_ordinary = 1;
    keep_ordinary = 1;
  } else if (strcmp(mode, "own-timer-nonleader") && strcmp(mode, "own-timer-leader")) {
    return 83;
  }
  exec_from_handler = !strcmp(mode, "proxy-leader") || !strcmp(mode, "plain-leader") ||
                      !strcmp(mode, "own-timer-leader");
  if (argc == 4) return new_image(argc, argv);

  after_argv[0] = argv[0];
  after_argv[1] = argv[1];
  after_argv[2] = exec_from_handler ? "after-leader-handler" : "after-worker";
  after_argv[3] = "child-checked";
  after_argv[4] = NULL;
  struct sigaction action;
  memset(&action, 0, sizeof action);
  action.sa_handler = SIG_IGN;
  if (sigemptyset(&action.sa_mask) || sigaction(SIGUSR1, &action, NULL)) return 84;

  /* The ordinary neighbor differs only in this original leader mask: its
   * ordinary copy is kept natively and must survive exec alongside a timer. */
  sigset_t set;
  if (sigemptyset(&set) || sigaddset(&set, SIGUSR1)) return 85;
  if (pthread_sigmask(keep_ordinary ? SIG_BLOCK : SIG_UNBLOCK, &set, NULL)) return 86;
  memset(&action, 0, sizeof action);
  action.sa_handler = on_hup;
  if (sigemptyset(&action.sa_mask) || sigaddset(&action.sa_mask, SIGUSR1) ||
      sigaction(SIGHUP, &action, NULL))
    return 87;
  if (sigemptyset(&set) || sigaddset(&set, SIGHUP) ||
      pthread_sigmask(SIG_UNBLOCK, &set, NULL))
    return 88;
  pthread_t thread;
  if (pthread_create(&thread, NULL, worker, NULL)) return 89;
  park();
}
