/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * A signal that one guest process sends another while the target sleeps in
 * pause(2) or nanosleep(2) must reach it as Linux delivers it
 * (https://github.com/rrnewton/hermit/issues/3982). Detcore emulates both
 * calls by parking the thread in the scheduler, so the signal stays pending in
 * the kernel until the scheduler wakes the sleeper. Before the fix nothing
 * did: a child parked in pause() that its parent killed with SIGTERM slept
 * forever while the parent waited for it.
 *
 * Each case forks a child that sleeps, lets it reach the sleep, sends it
 * signals, and reports how the child ended:
 *
 *   pause-fatal     SIGTERM with its default action kills the paused child.
 *   pause-caught    A caught SIGUSR1 runs its handler; pause() fails with EINTR.
 *   pause-ignored   SIGUSR2 (SIG_IGN), SIGHUP (blocked) and SIGWINCH (ignored
 *                   by default) leave the child paused; a later SIGTERM kills
 *                   it, so none of them ended the pause.
 *   pause-stop      SIGTSTP stops the paused child and SIGCONT resumes it
 *                   without ending the pause, which Linux restarts; a later
 *                   caught SIGUSR1 then ends it with EINTR, once. The child
 *                   leads its own process group, so the group is not orphaned
 *                   and Linux does not discard the stop. (SIGTSTP rather than
 *                   SIGSTOP: Reverie's ptrace backend takes a SIGSTOP for its
 *                   own suspension and never delivers it to a guest.)
 *   sleep-fatal     SIGTERM kills a child in a long nanosleep() when it is
 *                   sent, not when the sleep would have ended: the parent
 *                   reaps the child well before the 1000 s the child asked
 *                   for, measured on CLOCK_MONOTONIC (virtual time under
 *                   Hermit).
 *   sleep-caught    A caught SIGUSR1 ends a long nanosleep() early with EINTR
 *                   and a positive remaining time.
 *
 * The child reports through its exit status only, never its pid, so the
 * output does not depend on the pid namespace.
 */

#define _GNU_SOURCE

#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

/* Child exit codes. */
#define CHILD_EINTR_ONE_HANDLER 10
#define CHILD_WRONG_RESULT 20
#define CHILD_WOKEN_WRONGLY 30
#define CHILD_SETUP_FAILED 40

static volatile sig_atomic_t handled = 0;

static void on_usr1(int signo) {
  (void)signo;
  handled++;
}

static void install_usr1(void) {
  struct sigaction sa;
  memset(&sa, 0, sizeof(sa));
  sa.sa_handler = on_usr1;
  sigemptyset(&sa.sa_mask);
  if (sigaction(SIGUSR1, &sa, NULL) != 0) {
    _exit(CHILD_SETUP_FAILED);
  }
}

/* Long enough that only a signal can end it within the test. */
static const struct timespec long_sleep = {1000, 0};

static double monotonic_seconds(void) {
  struct timespec now;
  if (clock_gettime(CLOCK_MONOTONIC, &now) != 0) {
    perror("clock_gettime");
    exit(2);
  }
  return (double)now.tv_sec + (double)now.tv_nsec / 1e9;
}

/* Give the child time to reach its sleep. */
static void settle(void) {
  usleep(100000);
}

enum pause_mode {
  PAUSE_PLAIN,
  PAUSE_IGNORING_OTHERS,
  PAUSE_OWN_GROUP,
};

static void child_pause(int mode) {
  int ignore_others = mode == PAUSE_IGNORING_OTHERS;
  install_usr1();
  if (mode == PAUSE_OWN_GROUP && setpgid(0, 0) != 0) {
    _exit(CHILD_SETUP_FAILED);
  }
  if (ignore_others) {
    sigset_t block;
    sigemptyset(&block);
    sigaddset(&block, SIGHUP);
    if (signal(SIGUSR2, SIG_IGN) == SIG_ERR ||
        sigprocmask(SIG_BLOCK, &block, NULL) != 0) {
      _exit(CHILD_SETUP_FAILED);
    }
  }
  errno = 0;
  int rc = pause();
  if (ignore_others) {
    _exit(CHILD_WOKEN_WRONGLY);
  }
  _exit(
      rc == -1 && errno == EINTR && handled == 1 ? CHILD_EINTR_ONE_HANDLER
                                                 : CHILD_WRONG_RESULT);
}

static void child_sleep(void) {
  install_usr1();
  struct timespec rem = {0, 0};
  errno = 0;
  int rc = nanosleep(&long_sleep, &rem);
  int early = rem.tv_sec > 0 || rem.tv_nsec > 0;
  _exit(
      rc == -1 && errno == EINTR && handled == 1 && early
          ? CHILD_EINTR_ONE_HANDLER
          : CHILD_WRONG_RESULT);
}

static void describe(const char *name, int status) {
  if (WIFSIGNALED(status)) {
    printf("%s: killed by signal %d\n", name, WTERMSIG(status));
  } else if (WIFEXITED(status)) {
    printf("%s: exit %d\n", name, WEXITSTATUS(status));
  } else {
    printf("%s: status %#x\n", name, status);
  }
}

static int expect(
    const char *name,
    int status,
    int want_signal,
    int want_exit) {
  describe(name, status);
  if (want_signal != 0) {
    return WIFSIGNALED(status) && WTERMSIG(status) == want_signal;
  }
  return WIFEXITED(status) && WEXITSTATUS(status) == want_exit;
}

static pid_t spawn(void (*child)(int), int arg) {
  pid_t pid = fork();
  if (pid < 0) {
    perror("fork");
    exit(2);
  }
  if (pid == 0) {
    child(arg);
  }
  settle();
  return pid;
}

static void sleep_child(int unused) {
  (void)unused;
  child_sleep();
}

static int reap(pid_t pid, int options) {
  int status = 0;
  if (waitpid(pid, &status, options) != pid) {
    perror("waitpid");
    exit(2);
  }
  return status;
}

int main(void) {
  int ok = 1;
  pid_t pid;

  pid = spawn(child_pause, PAUSE_PLAIN);
  kill(pid, SIGTERM);
  ok &= expect("pause-fatal", reap(pid, 0), SIGTERM, 0);

  pid = spawn(child_pause, PAUSE_PLAIN);
  kill(pid, SIGUSR1);
  ok &= expect("pause-caught", reap(pid, 0), 0, CHILD_EINTR_ONE_HANDLER);

  pid = spawn(child_pause, PAUSE_IGNORING_OTHERS);
  kill(pid, SIGUSR2);
  kill(pid, SIGHUP);
  kill(pid, SIGWINCH);
  settle();
  kill(pid, SIGTERM);
  ok &= expect("pause-ignored", reap(pid, 0), SIGTERM, 0);

  pid = spawn(child_pause, PAUSE_OWN_GROUP);
  kill(pid, SIGTSTP);
  int stopped = reap(pid, WUNTRACED);
  printf("pause-stop: stopped=%d\n", WIFSTOPPED(stopped));
  ok &= WIFSTOPPED(stopped) && WSTOPSIG(stopped) == SIGTSTP;
  kill(pid, SIGCONT);
  settle();
  kill(pid, SIGUSR1);
  ok &= expect("pause-stop", reap(pid, 0), 0, CHILD_EINTR_ONE_HANDLER);

  pid = spawn(sleep_child, 0);
  double sent = monotonic_seconds();
  kill(pid, SIGTERM);
  ok &= expect("sleep-fatal", reap(pid, 0), SIGTERM, 0);
  int prompt = monotonic_seconds() - sent < long_sleep.tv_sec / 10;
  printf("sleep-fatal: reaped before the sleep would end=%d\n", prompt);
  ok &= prompt;

  pid = spawn(sleep_child, 0);
  kill(pid, SIGUSR1);
  ok &= expect("sleep-caught", reap(pid, 0), 0, CHILD_EINTR_ONE_HANDLER);

  puts(ok ? "PAUSE_CROSS_TASK_SIGNAL_OK" : "PAUSE_CROSS_TASK_SIGNAL_FAILED");
  return ok ? 0 : 1;
}
