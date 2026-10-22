/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * The root forks H, whose main thread writes "child\n", and waits for H; then
 * it writes how H ended. hermit-cli/tests/cli.rs holds H's write until the
 * root's write, and H is killed while held, in one of these ways (argv[1]):
 *
 * - "other":      a second child K runs kill(H, SIGKILL);
 * - "group":      H is in its own process group, and K, outside it, runs
 *                 kill(-H, SIGKILL);
 * - "group-self": K joins H's group and runs kill(-H, SIGKILL), killing itself
 *                 too;
 * - "raise-kill", "abort", "raise-term", "segv": a helper thread of H raises
 *                 SIGKILL on itself, calls abort(), raises SIGTERM (default
 *                 action), or stores through a null pointer.
 * - "pdeathsig":  a helper thread of the root forks H, which arms
 *                 PR_SET_PDEATHSIG(SIGKILL) and tells the helper through a
 *                 pipe; the helper then exits, so the kernel kills H. The root
 *                 joins the helper, waits for H and writes how H ended.
 * - "pdeathsig-middle": as "pdeathsig", but the helper forks M, which arms
 *                 the parent-death SIGKILL, forks H (held) and waits for it.
 *                 The kernel kills M; the root waits for M and writes
 *                 "parent m-status=...", and the orphaned H then writes
 *                 "child\n".
 *
 * In every mode the root's wait ends, so the run is not a deadlock
 * (https://github.com/rrnewton/hermit/issues/3904; the probes of the reviews
 * of https://github.com/rrnewton/hermit/pull/4013).
 */
#define _GNU_SOURCE
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/prctl.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static const char *mode = "other";

static void *helper(void *arg) {
  (void)arg;
  struct timespec pause = {0, 1000000};
  nanosleep(&pause, 0);
  if (strcmp(mode, "raise-kill") == 0) {
    raise(SIGKILL);
  } else if (strcmp(mode, "abort") == 0) {
    abort();
  } else if (strcmp(mode, "raise-term") == 0) {
    raise(SIGTERM);
  } else if (strcmp(mode, "segv") == 0) {
    *(volatile int *)0 = 1;
  }
  return 0;
}

static pid_t doomed;
static int ready[2];

/* The root's helper thread for the "pdeathsig" modes: it forks the process
 * that arms the parent-death SIGKILL, waits until it is armed, and exits, so
 * the kernel sends that SIGKILL at a moment set by host timing. */
static void *parent_death_helper(void *arg) {
  (void)arg;
  int middle = strcmp(mode, "pdeathsig-middle") == 0;
  doomed = fork();
  if (doomed == 0) {
    prctl(PR_SET_PDEATHSIG, SIGKILL);
    pid_t h = middle ? fork() : 0;
    if (h == 0 && middle) {
      if (write(1, "child\n", 6) != 6) {
        _exit(2);
      }
      _exit(0);
    }
    char c = 'a';
    if (write(ready[1], &c, 1) != 1) {
      _exit(7);
    }
    if (middle) {
      waitpid(h, 0, 0);
      _exit(0);
    }
    if (write(1, "child\n", 6) != 6) {
      _exit(2);
    }
    _exit(0);
  }
  char c;
  if (read(ready[0], &c, 1) != 1) {
    return (void *)1;
  }
  return 0;
}

static int run_parent_death(void) {
  if (pipe(ready) != 0) {
    return 3;
  }
  pthread_t thread;
  if (pthread_create(&thread, 0, parent_death_helper, 0) != 0) {
    return 3;
  }
  pthread_join(thread, 0);
  int status = 0;
  if (waitpid(doomed, &status, 0) != doomed) {
    return 4;
  }
  char line[64];
  int length = snprintf(line, sizeof line, "parent %s-status=%s\n",
                        strcmp(mode, "pdeathsig-middle") == 0 ? "m" : "h",
                        WIFSIGNALED(status) ? "killed" : "exited");
  if (write(1, line, length) != length) {
    return 2;
  }
  return 0;
}

static int self_inflicted(void) {
  return strcmp(mode, "raise-kill") == 0 || strcmp(mode, "abort") == 0 ||
         strcmp(mode, "raise-term") == 0 || strcmp(mode, "segv") == 0;
}

int main(int argc, char **argv) {
  if (argc > 1) {
    mode = argv[1];
  }
  if (strncmp(mode, "pdeathsig", 9) == 0) {
    return run_parent_death();
  }
  int grouped = strcmp(mode, "group") == 0 || strcmp(mode, "group-self") == 0;
  pid_t h = fork();
  if (h < 0) {
    return 3;
  }
  if (h == 0) {
    if (grouped) {
      setpgid(0, 0);
    }
    if (self_inflicted()) {
      pthread_t thread;
      if (pthread_create(&thread, 0, helper, 0) != 0) {
        _exit(5);
      }
    }
    if (write(1, "child\n", 6) != 6) {
      _exit(2);
    }
    _exit(0);
  }
  if (grouped) {
    setpgid(h, h);
  }
  pid_t k = -1;
  if (!self_inflicted()) {
    k = fork();
    if (k < 0) {
      return 3;
    }
    if (k == 0) {
      if (strcmp(mode, "group-self") == 0) {
        setpgid(0, h);
      }
      if (grouped) {
        kill(-h, SIGKILL);
      } else {
        kill(h, SIGKILL);
      }
      _exit(0);
    }
    if (strcmp(mode, "group-self") == 0) {
      setpgid(k, h);
    }
  }
  int status = 0;
  if (waitpid(h, &status, 0) != h) {
    return 4;
  }
  char line[64];
  int length = snprintf(line, sizeof line, "parent h-status=%s\n",
                        WIFSIGNALED(status) ? "killed" : "exited");
  if (k > 0 && waitpid(k, NULL, 0) != k) {
    return 5;
  }
  if (write(1, line, length) != length) {
    return 2;
  }
  return 0;
}
