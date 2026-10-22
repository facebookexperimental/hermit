/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <sched.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/prctl.h>
#include <sys/wait.h>
#include <stdlib.h>
#include <unistd.h>

/*
 * A child subreaper (prctl(PR_SET_CHILD_SUBREAPER)) inherits the orphans of
 * its descendants and can wait for them
 * (https://github.com/rrnewton/hermit/issues/3997). The guest becomes a
 * subreaper, then forks a child that forks a grandchild and exits with status
 * 3. The grandchild exits with status 7 in two orders:
 *   - "late": after the child, as a live orphan;
 *   - "early": before the child, held as a zombie with WNOWAIT, so it is a
 *     zombie when it is re-parented.
 * In both, the subreaper must reap the child, then the orphan, then see
 * ECHILD. A third phase leaves four live orphans at once ("multi"); the
 * subreaper reaps the child and all four (statuses 10 to 13, printed sorted),
 * then sees ECHILD. Its re-parenting log lines must come out in pid order on
 * every run; Detcore's unit test orphans_are_decided_in_pid_order guards that
 * order, because a harness run of this cell did not catch it reversed.
 *
 * The flag is set with argument 2 (Linux treats any nonzero value as on),
 * and the program then re-execs itself: the flag survives exec, so the
 * phases run after the exec and PR_GET_CHILD_SUBREAPER reads 1. Natively
 * that is what Linux does. The output names processes by role, never by pid,
 * so it is the same on every run.
 *
 * Only backends whose Detcore wait model re-parents orphans support the
 * flag; the others refuse it, which c-programs/child-subreaper-refusal
 * asserts.
 */

static int run(const char *order, int orphan_first) {
  pid_t child = fork();
  if (child < 0) {
    return 0;
  }
  if (child == 0) {
    pid_t orphan = fork();
    if (orphan == 0) {
      if (!orphan_first) {
        for (int i = 0; i < 50; i++) {
          sched_yield();
        }
      }
      _exit(7);
    }
    if (orphan_first) {
      /* The zombie precondition: exactly this orphan, exited with 7. */
      siginfo_t info;
      memset(&info, 0, sizeof(info));
      int rc = waitid(P_PID, (id_t)orphan, &info, WEXITED | WNOWAIT);
      if (rc != 0 || info.si_pid != orphan || info.si_code != CLD_EXITED ||
          info.si_status != 7) {
        _exit(99);
      }
    }
    _exit(3);
  }
  int reaped = 0;
  int ok = 1;
  for (;;) {
    int status = 0;
    pid_t pid = wait(&status);
    if (pid < 0) {
      int wait_errno = errno;
      printf("%s wait ended errno=%d\n", order, wait_errno);
      ok = ok && wait_errno == ECHILD;
      break;
    }
    int expected = reaped == 0 ? 3 : 7;
    printf(
        "%s reaped %s status %d\n",
        order,
        pid == child ? "child" : "orphan",
        WEXITSTATUS(status));
    ok = ok && WIFEXITED(status) && WEXITSTATUS(status) == expected &&
        (reaped == 0) == (pid == child);
    reaped++;
  }
  return ok && reaped == 2;
}

static int compare_ints(const void *a, const void *b) {
  return *(const int *)a - *(const int *)b;
}

/* The child forks four orphans that outlive it and exit with 10 to 13. */
static int run_multi(void) {
  enum { ORPHANS = 4 };
  pid_t child = fork();
  if (child < 0) {
    return 0;
  }
  if (child == 0) {
    for (int i = 0; i < ORPHANS; i++) {
      if (fork() == 0) {
        for (int j = 0; j < 50 + 10 * i; j++) {
          sched_yield();
        }
        _exit(10 + i);
      }
    }
    _exit(3);
  }
  int statuses[ORPHANS];
  int orphans = 0;
  int ok = 1;
  int child_reaped = 0;
  for (;;) {
    int status = 0;
    pid_t pid = wait(&status);
    if (pid < 0) {
      int wait_errno = errno;
      printf("multi wait ended errno=%d\n", wait_errno);
      ok = ok && wait_errno == ECHILD;
      break;
    }
    if (pid == child) {
      ok = ok && !child_reaped && WIFEXITED(status) && WEXITSTATUS(status) == 3;
      child_reaped = 1;
    } else if (orphans < ORPHANS && WIFEXITED(status)) {
      statuses[orphans++] = WEXITSTATUS(status);
    } else {
      ok = 0;
    }
  }
  qsort(statuses, (size_t)orphans, sizeof(int), compare_ints);
  printf("multi reaped child=%d orphans=%d statuses", child_reaped, orphans);
  for (int i = 0; i < orphans; i++) {
    printf(" %d", statuses[i]);
    ok = ok && statuses[i] == 10 + i;
  }
  printf("\n");
  return ok && child_reaped && orphans == ORPHANS;
}

int main(int argc, char **argv) {
  if (argc < 2 || strcmp(argv[1], "after-exec") != 0) {
    errno = 0;
    int set_rc = prctl(PR_SET_CHILD_SUBREAPER, 2, 0, 0, 0);
    int set_errno = errno;
    printf("subreaper set_rc=%d set_errno=%d (argument 2)\n", set_rc, set_errno);
    fflush(stdout);
    if (set_rc != 0) {
      printf("subreaper ok=0\n");
      return 1;
    }
    char *const args[] = {argv[0], "after-exec", NULL};
    execv("/proc/self/exe", args);
    printf("subreaper exec failed errno=%d\n", errno);
    return 1;
  }
  int value = -1;
  errno = 0;
  int get_rc = prctl(PR_GET_CHILD_SUBREAPER, &value, 0, 0, 0);
  int get_errno = errno;
  printf(
      "subreaper after exec get_rc=%d get_errno=%d value=%d\n",
      get_rc,
      get_errno,
      value);
  fflush(stdout);
  int ok = get_rc == 0 && value == 1;
  ok = run("late", 0) && ok;
  fflush(stdout);
  ok = run("early", 1) && ok;
  fflush(stdout);
  ok = run_multi() && ok;
  printf("subreaper ok=%d\n", ok);
  return ok ? 0 : 1;
}
