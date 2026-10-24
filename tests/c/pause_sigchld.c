/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * A parent forks a child that exits at once and sleeps until its SIGCHLD
 * (https://github.com/rrnewton/hermit/issues/4126). The scheduler sends that
 * SIGCHLD when the child exits, which can be while the parent's sleep is filed
 * but not yet parked; with a "late-" prefix the child exits after 100 ms, once
 * the sleep has parked. Linux's answer is the same either way.
 *
 *   (no argument)  SIGCHLD caught; `while (!flag) pause();` returns.
 *   pause-blocked  SIGCHLD caught but blocked, SIGALRM caught, alarm(1); one
 *                  pause() ends only for SIGALRM, and SIGCHLD's handler runs
 *                  once it is unblocked.
 *   pause-default  SIGCHLD with its default (ignore) action, SIGALRM caught,
 *                  alarm(1); one pause() ends only for SIGALRM.
 *   nano-caught    SIGCHLD caught; nanosleep(5 s) ends early with EINTR.
 *   nano-blocked   SIGCHLD caught but blocked; nanosleep(1 s) returns 0.
 *   nano-default   SIGCHLD with its default action; nanosleep(1 s) returns 0.
 *   nano-default-yield
 *                  As nano-default, with a 500 ms nanosleep while a worker
 *                  thread spins on getpid and sched_yield until the sleep ends:
 *                  the sleep still runs its full length and returns 0.
 *   nano-caught-yield, nano-caught-yield-wblock
 *                  SIGCHLD caught, the same worker (with SIGCHLD blocked in it
 *                  for -wblock): the sleep ends with EINTR and the handler ran
 *                  in the sleeping thread.
 *   nano-blocked-yield
 *                  SIGCHLD caught but blocked during the sleep, the same
 *                  worker: the sleep returns 0, and once the sleeper unblocks
 *                  SIGCHLD, while the worker still spins, its handler runs in
 *                  the sleeper.
 *   nano-caught-nonleader
 *                  SIGCHLD caught; a second thread forks the child and sleeps
 *                  500 ms while the main thread spins on getpid and
 *                  sched_yield: the sleep ends with EINTR and the handler ran
 *                  in the sleeping thread, the child's creator.
 *   twokid-yield, twokid-nonleader
 *                  SIGCHLD caught; the sleeping thread forks two children, one
 *                  exiting at once and one after 10,000 getpid calls, and
 *                  sleeps 500 ms while another thread spins on getpid and
 *                  sched_yield. The sleeper is the main thread (-yield) or a
 *                  second thread (-nonleader). The sleep ends with EINTR and a
 *                  handler ran in the sleeper before it returned; both
 *                  children's SIGCHLDs are handled.
 *   sigqueue-private, tgkill-private
 *                  SIGCHLD caught and blocked in every thread. A second thread
 *                  is sent a SIGCHLD of its own with pthread_sigqueue (or
 *                  pthread_kill), forks a child that exits at once, sleeps
 *                  100 ms, and unblocks SIGCHLD while a third thread spins: it
 *                  handles both, its own copy and the child's, which carries
 *                  the child's exit (CLD_EXITED and its pid).
 *   tgsigqueue-forged
 *                  As sigqueue-private, but the second thread forks first and
 *                  sends itself its own copy with rt_tgsigqueueinfo, with a
 *                  forged siginfo claiming that child's exit (CLD_EXITED; Linux
 *                  allows a forged code only to the sender itself): it still
 *                  handles both, two claiming the child's exit.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <stdatomic.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static volatile sig_atomic_t got_chld, got_alrm;
static volatile pid_t handler_tid;
static int late_child;
static _Atomic int sleep_done;

static pid_t twokid_sleeper_tid;
static _Atomic int chld_in_sleeper;

static void on_chld(int sig) {
  (void)sig;
  handler_tid = (pid_t)syscall(SYS_gettid);
  if (handler_tid == twokid_sleeper_tid) {
    atomic_fetch_add(&chld_in_sleeper, 1);
  }
  got_chld++;
}

static void on_alrm(int sig) {
  (void)sig;
  got_alrm++;
}

static void handle(int sig, void (*handler)(int)) {
  struct sigaction action;
  memset(&action, 0, sizeof(action));
  action.sa_handler = handler;
  sigaction(sig, &action, NULL);
}

static void mask(int how, int sig) {
  sigset_t set;
  sigemptyset(&set);
  sigaddset(&set, sig);
  sigprocmask(how, &set, NULL);
}

static void* busy_worker(void* arg) {
  if (arg != NULL) {
    mask(SIG_BLOCK, SIGCHLD);
  }
  while (!atomic_load(&sleep_done)) {
    syscall(SYS_getpid);
    syscall(SYS_sched_yield);
  }
  return NULL;
}

static pid_t spawn(void) {
  pid_t child = fork();
  if (child == 0) {
    if (late_child) {
      struct timespec delay = {0, 100 * 1000 * 1000};
      nanosleep(&delay, NULL);
    }
    _exit(0);
  }
  return child;
}

static int reap(pid_t child) {
  int status = 0;
  waitpid(child, &status, 0);
  return WIFEXITED(status) ? WEXITSTATUS(status) : -1;
}

static const char* errno_name(int ret, int error) {
  return ret == 0 ? "0" : error == EINTR ? "-1 EINTR" : strerror(error);
}

static pid_t twokid_children[2];
static int twokid_ret, twokid_errno, twokid_own;

static void* twokid_sleep(void* arg) {
  (void)arg;
  twokid_sleeper_tid = (pid_t)syscall(SYS_gettid);
  twokid_children[0] = spawn();
  twokid_children[1] = fork();
  if (twokid_children[1] == 0) {
    for (int i = 0; i < 10000; i++) {
      syscall(SYS_getpid);
    }
    _exit(0);
  }
  struct timespec request = {0, 500 * 1000 * 1000};
  twokid_ret = nanosleep(&request, NULL);
  twokid_errno = errno;
  twokid_own = atomic_load(&chld_in_sleeper);
  atomic_store(&sleep_done, 1);
  return NULL;
}

static void* spinner(void* arg) {
  (void)arg;
  while (!atomic_load(&sleep_done)) {
    syscall(SYS_getpid);
    syscall(SYS_sched_yield);
  }
  return NULL;
}

static pid_t sigqueue_child;
static _Atomic int sigqueue_go, sigqueue_forked, child_exit_seen;

static void on_chld_info(int sig, siginfo_t* info, void* context) {
  (void)sig;
  (void)context;
  if (info->si_code == CLD_EXITED && info->si_pid == sigqueue_child) {
    atomic_fetch_add(&child_exit_seen, 1);
  }
  got_chld++;
}
static _Atomic pid_t sigqueue_receiver_tid;

static void* sigqueue_receiver(void* arg) {
  int forged = arg != NULL;
  atomic_store(&sigqueue_receiver_tid, (pid_t)syscall(SYS_gettid));
  if (forged) {
    sigqueue_child = spawn();
    siginfo_t info;
    memset(&info, 0, sizeof(info));
    info.si_signo = SIGCHLD;
    info.si_code = CLD_EXITED;
    info.si_pid = sigqueue_child;
    syscall(SYS_rt_tgsigqueueinfo, getpid(), atomic_load(&sigqueue_receiver_tid), SIGCHLD,
            &info);
    atomic_store(&sigqueue_forked, 1);
  }
  while (!atomic_load(&sigqueue_go)) {
    syscall(SYS_sched_yield);
  }
  if (!forged) {
    sigqueue_child = spawn();
  }
  struct timespec delay = {0, 100 * 1000 * 1000};
  nanosleep(&delay, NULL);
  mask(SIG_UNBLOCK, SIGCHLD);
  atomic_store(&sleep_done, 1);
  return NULL;
}

static pid_t nonleader_tid, nonleader_child;
static int nonleader_ret, nonleader_errno;

static void* nonleader_sleeper(void* arg) {
  (void)arg;
  nonleader_tid = (pid_t)syscall(SYS_gettid);
  nonleader_child = spawn();
  struct timespec request = {0, 500 * 1000 * 1000};
  nonleader_ret = nanosleep(&request, NULL);
  nonleader_errno = errno;
  atomic_store(&sleep_done, 1);
  return NULL;
}

int main(int argc, char** argv) {
  setvbuf(stdout, NULL, _IONBF, 0);
  const char* mode = argc > 1 ? argv[1] : "";
  if (strncmp(mode, "late-", 5) == 0) {
    late_child = 1;
    mode += 5;
  }
  if (*mode == '\0') {
    handle(SIGCHLD, on_chld);
    pid_t child = spawn();
    while (!got_chld) {
      pause();
    }
    int status = reap(child);
    printf("SIGCHLD handled: %d, child exit status: %d\n", (int)got_chld, status);
    return 0;
  }
  if (strcmp(mode, "twokid-yield") == 0 || strcmp(mode, "twokid-nonleader") == 0) {
    handle(SIGCHLD, on_chld);
    pthread_t other;
    if (strcmp(mode, "twokid-yield") == 0) {
      pthread_create(&other, NULL, spinner, NULL);
      twokid_sleep(NULL);
    } else {
      pthread_create(&other, NULL, twokid_sleep, NULL);
      spinner(NULL);
    }
    pthread_join(other, NULL);
    reap(twokid_children[0]);
    reap(twokid_children[1]);
    printf("%s: nanosleep %s, SIGCHLD in sleeper before return %d, in all %d\n", mode,
           errno_name(twokid_ret, twokid_errno), twokid_own, (int)got_chld);
    return 0;
  }
  if (strcmp(mode, "sigqueue-private") == 0 || strcmp(mode, "tgkill-private") == 0 ||
      strcmp(mode, "tgsigqueue-forged") == 0) {
    int forged = strcmp(mode, "tgsigqueue-forged") == 0;
    static int forged_arg = 1;
    struct sigaction action;
    memset(&action, 0, sizeof(action));
    action.sa_sigaction = on_chld_info;
    action.sa_flags = SA_SIGINFO;
    sigaction(SIGCHLD, &action, NULL);
    mask(SIG_BLOCK, SIGCHLD);
    pthread_t receiver, other;
    pthread_create(&receiver, NULL, sigqueue_receiver, forged ? &forged_arg : NULL);
    pthread_create(&other, NULL, spinner, NULL);
    if (forged) {
      while (!atomic_load(&sigqueue_forked)) {
        syscall(SYS_sched_yield);
      }
    } else if (strcmp(mode, "tgkill-private") == 0) {
      pthread_kill(receiver, SIGCHLD);
    } else {
      union sigval value = {.sival_int = 7};
      pthread_sigqueue(receiver, SIGCHLD, value);
    }
    atomic_store(&sigqueue_go, 1);
    pthread_join(receiver, NULL);
    pthread_join(other, NULL);
    reap(sigqueue_child);
    printf("%s: SIGCHLD handled %d, the child's exit %d\n", mode, (int)got_chld,
           atomic_load(&child_exit_seen));
    return 0;
  }
  if (strcmp(mode, "nano-caught-nonleader") == 0) {
    handle(SIGCHLD, on_chld);
    pthread_t sleeper;
    pthread_create(&sleeper, NULL, nonleader_sleeper, NULL);
    while (!atomic_load(&sleep_done)) {
      syscall(SYS_getpid);
      syscall(SYS_sched_yield);
    }
    pthread_join(sleeper, NULL);
    printf("%s: nanosleep %s, SIGCHLD %d, handler in sleeper: %s\n", mode,
           errno_name(nonleader_ret, nonleader_errno), (int)got_chld,
           got_chld == 0 ? "-" : handler_tid == nonleader_tid ? "yes" : "no");
    reap(nonleader_child);
    return 0;
  }
  int pause_mode = strncmp(mode, "pause-", 6) == 0;
  int blocked = strstr(mode, "-blocked") != NULL;
  int caught = blocked || strstr(mode, "-caught") != NULL;
  if (caught) {
    handle(SIGCHLD, on_chld);
  }
  if (blocked) {
    mask(SIG_BLOCK, SIGCHLD);
  }
  if (pause_mode) {
    handle(SIGALRM, on_alrm);
    alarm(1);
    pid_t child = spawn();
    int ret = pause();
    int error = errno;
    printf("%s: pause %s, SIGALRM %d, SIGCHLD %d\n", mode, errno_name(ret, error),
           (int)got_alrm, (int)got_chld);
    if (blocked) {
      mask(SIG_UNBLOCK, SIGCHLD);
      printf("%s: after unblocking, SIGCHLD %d\n", mode, (int)got_chld);
    }
    reap(child);
    return 0;
  }
  int yielding = strstr(mode, "-yield") != NULL;
  struct timespec request = {blocked || !caught ? 1 : 5, 0};
  pthread_t worker;
  pid_t sleeper = (pid_t)syscall(SYS_gettid);
  if (yielding) {
    request.tv_sec = 0;
    request.tv_nsec = 500 * 1000 * 1000;
    static int worker_blocks = 1;
    pthread_create(&worker, NULL, busy_worker,
                   strstr(mode, "-wblock") != NULL ? &worker_blocks : NULL);
  }
  pid_t child = spawn();
  int ret = nanosleep(&request, NULL);
  int error = errno;
  int chld_at_return = got_chld;
  if (yielding && blocked) {
    mask(SIG_UNBLOCK, SIGCHLD);
  }
  if (yielding) {
    atomic_store(&sleep_done, 1);
    pthread_join(worker, NULL);
  }
  if (yielding && caught) {
    printf("%s: nanosleep %s, SIGCHLD %d, then %d, handler in sleeper: %s\n", mode,
           errno_name(ret, error), chld_at_return, (int)got_chld,
           got_chld == 0 ? "-" : handler_tid == sleeper ? "yes" : "no");
    reap(child);
    return 0;
  }
  printf("%s: nanosleep %s, SIGCHLD %d\n", mode, errno_name(ret, error), (int)got_chld);
  reap(child);
  return 0;
}
