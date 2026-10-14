/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/* Guest for hermit-cli/tests/cli.rs: the thread-group leader calls
 * pthread_exit while another thread lives. Linux keeps the leader as a
 * zombie, the group runs on, and its last thread ends the process
 * (https://github.com/rrnewton/hermit/issues/4100).
 *
 * usage: leader_pthread_exit <mode>
 *
 *   sleep:    the worker sleeps 20 ms, prints and returns.
 *   yield:    the worker spins on sched_yield, prints and calls exit(0).
 *   kill:     a second worker blocks SIGUSR1 and pauses; the first sends
 *             kill(getpid(), SIGUSR1), which a live thread takes.
 *   join:     the worker joins the exited main thread.
 *   join16:   16 forked children in turn do `join`, each worker exiting with
 *             pthread_join's result; the parent reaps them and counts the
 *             zeros (16 chances for the joiner to see the main thread's tid
 *             word before it is cleared).
 *   procself: the worker joins the exited main thread, then reads
 *             /proc/self/exe, /proc/self/fd and /proc/self/maps, which name the
 *             zombie leader: its exe link and files are gone and its maps
 *             empty, once its exit is complete.
 *   procself16: 16 forked children in turn do `procself`, each worker
 *             exiting 0 only if it saw the leader's exit complete; the parent
 *             counts them.
 *   lastexit: the worker, the group's last thread, calls pthread_exit.
 *   exec:     the worker execs this program.
 *   child:    a forked child does `code`; the parent reaps it.
 *   code:     the worker calls exit(7).
 */
#define _GNU_SOURCE
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static const char *mode;
static char *self;
static pthread_t main_thread;
static atomic_int receiver;

static void on_signal(int signo) {
  (void)signo;
  atomic_store(&receiver, (int)syscall(SYS_gettid));
}

/* After pthread_join of the exited main thread: whether the leader's exit
   shows complete in /proc/self, which names the zombie leader. */
static int leader_exit_seen(int *exe_errno, int *fds, int *maps_nonempty) {
  char buf[4096];
  ssize_t n = readlink("/proc/self/exe", buf, sizeof buf - 1);
  *exe_errno = n < 0 ? errno : 0;
  *fds = -1;
  DIR *dir = opendir("/proc/self/fd");
  if (dir) {
    *fds = 0;
    while (readdir(dir)) (*fds)++;
    closedir(dir);
  }
  int maps = open("/proc/self/maps", O_RDONLY);
  ssize_t got = maps >= 0 ? read(maps, buf, sizeof buf) : -1;
  if (maps >= 0) close(maps);
  *maps_nonempty = got > 0;
  return n < 0 && *fds == 2 && !*maps_nonempty;
}

static void nap(long ns) {
  struct timespec wait = {0, ns};
  nanosleep(&wait, NULL);
}

static void *pauser(void *arg) {
  (void)arg;
  sigset_t set;
  sigemptyset(&set);
  sigaddset(&set, SIGUSR1);
  pthread_sigmask(SIG_BLOCK, &set, NULL);
  for (;;) pause();
  return NULL;
}

static void *worker(void *arg) {
  (void)arg;
  if (!strcmp(mode, "sleep")) {
    nap(20000000);
    printf("sleep worker survived the leader's exit\n");
    fflush(stdout);
    return NULL;
  }
  if (!strcmp(mode, "yield") || !strcmp(mode, "kill")) {
    for (int i = 0; i < 2000; i++) sched_yield();
  } else if (strncmp(mode, "procself", 8)) {
    nap(2000000);
  }
  if (!strcmp(mode, "yield")) {
    printf("yield worker survived the leader's exit\n");
  } else if (!strcmp(mode, "kill")) {
    int rc = kill(getpid(), SIGUSR1);
    printf("kill rc=%d receiver=%s\n", rc,
           atomic_load(&receiver) == (int)syscall(SYS_gettid) ? "sender" : "other");
  } else if (!strcmp(mode, "join")) {
    printf("join rc=%d\n", pthread_join(main_thread, NULL));
  } else if (!strcmp(mode, "joinquiet")) {
    exit(pthread_join(main_thread, NULL));
  } else if (!strcmp(mode, "procself")) {
    int joined = pthread_join(main_thread, NULL), exe_errno, fds, maps;
    leader_exit_seen(&exe_errno, &fds, &maps);
    printf("procself join=%d exe_errno=%d fds=%d maps_nonempty=%d\n", joined, exe_errno, fds, maps);
  } else if (!strcmp(mode, "procselfquiet")) {
    int exe_errno, fds, maps;
    if (pthread_join(main_thread, NULL) != 0) exit(2);
    exit(leader_exit_seen(&exe_errno, &fds, &maps) ? 0 : 1);
  } else if (!strcmp(mode, "lastexit")) {
    printf("lastexit worker leaving by pthread_exit\n");
    fflush(stdout);
    pthread_exit(NULL);
  } else if (!strcmp(mode, "exec")) {
    /* /proc/self/exe is unreadable once the leader is a zombie. */
    execl(self, self, "after", (char *)NULL);
    _exit(78);
  } else if (!strcmp(mode, "code")) {
    printf("code worker exits 7\n");
    fflush(stdout);
    exit(7);
  }
  fflush(stdout);
  exit(0);
}

static int leader_exits(void) {
  main_thread = pthread_self();
  if (!strcmp(mode, "kill")) {
    struct sigaction action;
    memset(&action, 0, sizeof action);
    action.sa_handler = on_signal;
    sigaction(SIGUSR1, &action, NULL);
    pthread_t second;
    if (pthread_create(&second, NULL, pauser, NULL) != 0) return 70;
  }
  pthread_t thread;
  if (pthread_create(&thread, NULL, worker, NULL) != 0) return 70;
  pthread_exit(NULL);
}

int main(int argc, char **argv) {
  mode = argc > 1 ? argv[1] : "sleep";
  self = realpath(argv[0], NULL);
  if (!self) return 71;
  if (!strcmp(mode, "after")) {
    printf("exec after the leader's exit\n");
    return 0;
  }
  if (!strcmp(mode, "join16") || !strcmp(mode, "procself16")) {
    const char *name = mode;
    mode = !strcmp(name, "join16") ? "joinquiet" : "procselfquiet";
    int ok = 0;
    for (int i = 0; i < 16; i++) {
      pid_t child = fork();
      if (child == 0) return leader_exits();
      int status = 0;
      if (waitpid(child, &status, 0) == child && WIFEXITED(status) && WEXITSTATUS(status) == 0)
        ok++;
    }
    printf("%s ok=%d\n", name, ok);
    return 0;
  }
  if (!strcmp(mode, "child")) {
    mode = "code";
    pid_t child = fork();
    if (child == 0) return leader_exits();
    int status = 0;
    pid_t reaped = waitpid(child, &status, 0);
    printf("child reaped=%d exited=%d code=%d\n", reaped == child, WIFEXITED(status),
           WIFEXITED(status) ? WEXITSTATUS(status) : -1);
    return 0;
  }
  return leader_exits();
}
