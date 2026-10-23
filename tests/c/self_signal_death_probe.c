/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Processes that end themselves with a signal they send themselves, at a
 * point of their own program order: abort(3), raise(3), kill(2) of their own
 * process id, and raw tgkill(2) and tkill(2) naming the calling thread. This
 * is how assert(3) and most fatal-error paths end a program. One child raises
 * SIGTRAP, whose action in-guest LiteInst routes through a handler of its own
 * while the program's action is the default.
 *
 *   children    forks one child per case below; each child ends itself, and
 *               the parent reaps it and prints how it ended. Three control
 *               cases send themselves a signal that does NOT end them (one
 *               blocked, two ignored) and then exit with a code. The parent
 *               then exits 0.
 *   root-abort  the process Hermit started prints a line and calls abort().
 *   excluded-sigsegv / excluded-sigsys
 *               one child sends itself a runtime-owned signal. Its native
 *               death is unchanged, but in-guest verification must report
 *               the missing exit callbacks instead of granting credit.
 *
 * Every line printed is fixed. Whether a core file was written is the host's
 * policy (RLIMIT_CORE, core_pattern), so it is not printed.
 */

#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif

#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

static void fail(const char* what) {
  fprintf(stderr, "%s: %s\n", what, strerror(errno));
  exit(1);
}

static void say(const char* line) {
  if (write(STDOUT_FILENO, line, strlen(line)) < 0) {
    _exit(1);
  }
}

static void end_by_abort(void) {
  abort();
}

static void end_by_raise_sigterm(void) {
  raise(SIGTERM);
}

static void end_by_kill_sigkill(void) {
  kill(getpid(), SIGKILL);
}

static void end_by_tgkill_sigusr1(void) {
  syscall(SYS_tgkill, getpid(), (pid_t)syscall(SYS_gettid), SIGUSR1);
}

static void end_by_tkill_sigquit(void) {
  syscall(SYS_tkill, (pid_t)syscall(SYS_gettid), SIGQUIT);
}

/* SIGTRAP with its default action, as a debugger-less breakpoint path does. */
static void end_by_raise_sigtrap(void) {
  raise(SIGTRAP);
}

static void end_by_raise_sigsegv(void) {
  raise(SIGSEGV);
}

static void end_by_raise_sigsys(void) {
  raise(SIGSYS);
}

/* Control: a blocked signal stays pending and does not end the process. */
static void survive_blocked_sigusr2(void) {
  sigset_t set;
  sigemptyset(&set);
  sigaddset(&set, SIGUSR2);
  if (sigprocmask(SIG_BLOCK, &set, NULL) != 0) {
    fail("sigprocmask");
  }
  raise(SIGUSR2);
  _exit(3);
}

/* Control: an ignored signal is discarded and does not end the process. */
static void survive_ignored_sigusr2(void) {
  if (signal(SIGUSR2, SIG_IGN) == SIG_ERR) {
    fail("signal");
  }
  raise(SIGUSR2);
  _exit(4);
}

/* Control: an ignored SIGTRAP is discarded too. */
static void survive_ignored_sigtrap(void) {
  if (signal(SIGTRAP, SIG_IGN) == SIG_ERR) {
    fail("signal");
  }
  raise(SIGTRAP);
  _exit(5);
}

struct child_case {
  const char* label;
  void (*body)(void);
};

static const struct child_case CASES[] = {
    {"abort", end_by_abort},
    {"raise-sigterm", end_by_raise_sigterm},
    {"kill-sigkill", end_by_kill_sigkill},
    {"tgkill-sigusr1", end_by_tgkill_sigusr1},
    {"tkill-sigquit", end_by_tkill_sigquit},
    {"raise-sigtrap", end_by_raise_sigtrap},
    {"blocked-sigusr2", survive_blocked_sigusr2},
    {"ignored-sigusr2", survive_ignored_sigusr2},
    {"ignored-sigtrap", survive_ignored_sigtrap},
};

static int children(const struct child_case* cases, size_t count) {
  for (size_t i = 0; i < count; i++) {
    pid_t child = fork();
    if (child < 0) {
      fail("fork");
    }
    if (child == 0) {
      cases[i].body();
      /* Reached only if the signal did not end the child. */
      _exit(99);
    }
    int status = 0;
    if (waitpid(child, &status, 0) != child) {
      fail("waitpid");
    }
    char line[96];
    if (WIFSIGNALED(status)) {
      snprintf(
          line, sizeof(line), "%s: signal %d\n", cases[i].label, WTERMSIG(status));
    } else if (WIFEXITED(status)) {
      snprintf(
          line, sizeof(line), "%s: exit %d\n", cases[i].label, WEXITSTATUS(status));
    } else {
      snprintf(line, sizeof(line), "%s: neither\n", cases[i].label);
    }
    say(line);
  }
  return 0;
}

int main(int argc, char** argv) {
  if (argc == 2 && strcmp(argv[1], "children") == 0) {
    return children(CASES, sizeof(CASES) / sizeof(CASES[0]));
  }
  if (argc == 2 && strcmp(argv[1], "excluded-sigsegv") == 0) {
    const struct child_case child = {"raise-sigsegv", end_by_raise_sigsegv};
    return children(&child, 1);
  }
  if (argc == 2 && strcmp(argv[1], "excluded-sigsys") == 0) {
    const struct child_case child = {"raise-sigsys", end_by_raise_sigsys};
    return children(&child, 1);
  }
  if (argc == 2 && strcmp(argv[1], "root-abort") == 0) {
    say("root aborts\n");
    abort();
  }
  fprintf(
      stderr,
      "usage: %s children|root-abort|excluded-sigsegv|excluded-sigsys\n",
      argv[0]);
  return 2;
}
