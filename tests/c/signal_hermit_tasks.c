/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Signals a guest addresses to Hermit's own tasks in its PID namespace, and
 * signals that must still reach the guest's own tasks
 * (https://github.com/rrnewton/hermit/issues/4066). Run it only inside setsid.
 *
 *   hermit       Signals to Hermit's container init, PID 1 (the guest's
 *                parent), and another thread of that process, found in
 *                /proc/1/task. A forwarded SIGTERM to either ended Hermit's
 *                run. Linux drops a signal sent from inside a PID namespace to
 *                its init when the init has no handler, and the call returns
 *                0; each send here must do the same. pidfd_open(1) is refused
 *                with EPERM, so no pidfd reaches Hermit, and so is naming one
 *                of Hermit's tasks as a descriptor's signal owner (F_SETOWN,
 *                F_SETOWN_EX, ioctl FIOSETOWN and SIOCSPGRP). The guest then
 *                signals itself, a child process and a sibling thread, each of
 *                which must still be delivered.
 *   nested       A child in a nested PID namespace (its PID 1) and its own
 *                child (PID 2 there) signal themselves by their ids in that
 *                namespace with tkill, tgkill and raise; each is delivered.
 *   nested-kill  The same nested PID 1 signals itself with kill.
 *   owner-race N F_SETOWN_EX, then ioctl(FIOSETOWN), N times each, from
 *                memory a sibling thread keeps switching between the guest
 *                and PID 1: PID 1 must never become the owner.
 *   newborn N    A sibling thread tgkills each of N new threads as soon as the
 *                kernel publishes its tid (CLONE_PARENT_SETTID), before the
 *                creating clone returns; no send may fail. The new threads
 *                start with SIGUSR1 blocked.
 */
#define _GNU_SOURCE
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <linux/sockios.h>
#include <sys/ioctl.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static volatile sig_atomic_t handled;
static volatile sig_atomic_t async_signals;

static void on_usr2(int sig) {
  (void)sig;
  async_signals++;
}

static void on_usr1(int sig) {
  (void)sig;
  handled++;
}

static void report(const char* what, long ret) {
  if (ret >= 0) {
    printf("%s: %ld\n", what, ret);
  } else {
    printf("%s: %ld %s\n", what, ret, strerrorname_np(errno));
  }
}

static long queue(pid_t pid, int sig) {
  siginfo_t info;
  memset(&info, 0, sizeof(info));
  info.si_signo = sig;
  info.si_code = SI_QUEUE;
  info.si_pid = getpid();
  info.si_uid = getuid();
  return syscall(SYS_rt_sigqueueinfo, pid, sig, &info);
}

static long tgqueue(pid_t tgid, pid_t tid, int sig) {
  siginfo_t info;
  memset(&info, 0, sizeof(info));
  info.si_signo = sig;
  info.si_code = SI_QUEUE;
  info.si_pid = getpid();
  info.si_uid = getuid();
  return syscall(SYS_rt_tgsigqueueinfo, tgid, tid, sig, &info);
}

/* A thread of PID 1 other than PID 1 itself, or 0. */
static pid_t hermit_thread(void) {
  DIR* dir = opendir("/proc/1/task");
  if (dir == NULL) {
    return 0;
  }
  pid_t found = 0;
  struct dirent* entry;
  while ((entry = readdir(dir)) != NULL) {
    pid_t tid = (pid_t)atoi(entry->d_name);
    if (tid > 1) {
      found = tid;
      break;
    }
  }
  closedir(dir);
  return found;
}

static void* sibling(void* arg) {
  (void)arg;
  for (;;) {
    pause();
  }
  return NULL;
}

/* Waits, boundedly, for the handler count to reach `want`. */
static void await_handled(int want) {
  for (int i = 0; i < 1000 && handled < want; i++) {
    struct timespec ts = {0, 1000 * 1000};
    nanosleep(&ts, NULL);
  }
}

static int hermit_tasks(void) {
  pid_t thread = hermit_thread();
  printf("parent is 1: %s, another hermit thread: %s\n", getppid() == 1 ? "yes" : "no",
         thread > 0 ? "yes" : "no");
  report("kill(1, 0)", kill(1, 0));
  report("kill(1, SIGTERM)", kill(1, SIGTERM));
  report("kill(1, SIGKILL)", kill(1, SIGKILL));
  report("rt_sigqueueinfo(1, SIGTERM)", queue(1, SIGTERM));
  report("tkill(1, SIGTERM)", syscall(SYS_tkill, 1, SIGTERM));
  report("tgkill(1, 1, SIGTERM)", syscall(SYS_tgkill, 1, 1, SIGTERM));
  report("rt_tgsigqueueinfo(1, 1, SIGTERM)", tgqueue(1, 1, SIGTERM));
  if (thread > 0) {
    report("kill(thread, SIGTERM)", kill(thread, SIGTERM));
    report("rt_sigqueueinfo(thread, SIGTERM)", queue(thread, SIGTERM));
    report("tkill(thread, SIGTERM)", syscall(SYS_tkill, thread, SIGTERM));
    report("tgkill(1, thread, SIGTERM)", syscall(SYS_tgkill, 1, thread, SIGTERM));
    report("rt_tgsigqueueinfo(1, thread, SIGTERM)", tgqueue(1, thread, SIGTERM));
  }
  report("pidfd_open(1)", syscall(SYS_pidfd_open, 1, 0));
  /* An owner for a descriptor's signals: Hermit's tasks are refused
   * (https://github.com/rrnewton/hermit/issues/4083), the guest itself is not. */
  int owned[2];
  if (pipe(owned) == 0) {
    struct f_owner_ex owner = {F_OWNER_PID, 1};
    report("F_SETOWN(1)", fcntl(owned[0], F_SETOWN, 1));
    report("F_SETOWN_EX(PID 1)", fcntl(owned[0], F_SETOWN_EX, &owner));
    if (thread > 0) {
      owner.type = F_OWNER_TID;
      owner.pid = thread;
      report("F_SETOWN_EX(TID thread)", fcntl(owned[0], F_SETOWN_EX, &owner));
    }
    report("F_SETOWN(self)", fcntl(owned[0], F_SETOWN, getpid()));
    close(owned[0]);
    close(owned[1]);
  }
  /* The two ioctls that set the owner as F_SETOWN does. */
  int pair[2];
  if (socketpair(AF_UNIX, SOCK_STREAM, 0, pair) == 0) {
    int owner_pid = 1;
    report("ioctl(FIOSETOWN, 1)", ioctl(pair[0], FIOSETOWN, &owner_pid));
    report("ioctl(SIOCSPGRP, 1)", ioctl(pair[0], SIOCSPGRP, &owner_pid));
    if (thread > 0) {
      owner_pid = thread;
      report("ioctl(FIOSETOWN, thread)", ioctl(pair[0], FIOSETOWN, &owner_pid));
    }
    owner_pid = getpid();
    report("ioctl(FIOSETOWN, self)", ioctl(pair[0], FIOSETOWN, &owner_pid));
    /* With the guest as owner, readiness still signals it. */
    struct sigaction usr2;
    memset(&usr2, 0, sizeof(usr2));
    usr2.sa_handler = on_usr2;
    sigaction(SIGUSR2, &usr2, NULL);
    fcntl(pair[0], F_SETSIG, SIGUSR2);
    fcntl(pair[0], F_SETFL, fcntl(pair[0], F_GETFL) | O_ASYNC);
    if (write(pair[1], "x", 1) == 1) {
      for (int i = 0; i < 1000 && async_signals == 0; i++) {
        struct timespec ts = {0, 1000 * 1000};
        nanosleep(&ts, NULL);
      }
    }
    printf("async signal to self: %s\n", async_signals > 0 ? "delivered" : "not delivered");
    close(pair[0]);
    close(pair[1]);
  }

  report("kill(self, SIGUSR1)", kill(getpid(), SIGUSR1));
  printf("handled %d\n", (int)handled);
  pid_t child = fork();
  if (child == 0) {
    pause();
    _exit(0);
  }
  report("kill(child, SIGTERM)", kill(child, SIGTERM));
  int status = 0;
  waitpid(child, &status, 0);
  printf("child: %s\n",
         WIFSIGNALED(status) && WTERMSIG(status) == SIGTERM ? "killed by SIGTERM" : "other");
  pthread_t t;
  pthread_create(&t, NULL, sibling, NULL);
  report("pthread_kill(sibling, SIGUSR1)", pthread_kill(t, SIGUSR1) == 0 ? 0 : -1);
  await_handled(2);
  printf("handled %d\n", (int)handled);
  return 0;
}

/* In a nested PID namespace: its PID 1 signals itself with `kill` (when
 * `with_kill`) or with tkill and tgkill, and its child, PID 2 there, with
 * raise. */
static int nested(int with_kill) {
  long child = syscall(SYS_clone, CLONE_NEWPID | SIGCHLD, 0, 0, 0, 0);
  if (child < 0) {
    report("clone(CLONE_NEWPID)", child);
    return 1;
  }
  if (child == 0) {
    pid_t tid = (pid_t)syscall(SYS_gettid);
    printf("nested pid %d, tid %d\n", getpid(), tid);
    if (with_kill) {
      report("kill(self, SIGUSR1)", kill(getpid(), SIGUSR1));
      printf("handled %d\n", (int)handled);
      _exit(0);
    }
    report("tkill(self, SIGUSR1)", syscall(SYS_tkill, tid, SIGUSR1));
    report("tgkill(self, self, SIGUSR1)", syscall(SYS_tgkill, getpid(), tid, SIGUSR1));
    printf("handled %d\n", (int)handled);
    /* The child's end of the pipe closes when it exits. PID 1 waits for that
     * rather than with waitpid, which needs the child's pid as fork returned
     * it. */
    int done[2];
    if (pipe(done) < 0) {
      _exit(1);
    }
    pid_t grandchild = fork();
    if (grandchild == 0) {
      close(done[0]);
      printf("nested child pid %d\n", getpid());
      report("raise(SIGUSR1)", raise(SIGUSR1));
      printf("handled %d\n", (int)handled);
      _exit(0);
    }
    close(done[1]);
    char byte;
    while (read(done[0], &byte, 1) > 0) {
    }
    _exit(0);
  }
  int status = 0;
  waitpid((pid_t)child, &status, 0);
  printf("nested exit %d\n", WIFEXITED(status) ? WEXITSTATUS(status) : -1);
  return 0;
}

static _Atomic int newborn_tid;
static _Atomic int round_done;
static _Atomic int newborn_done;
static int sent, refused, failed;

static void* newborn_killer(void* arg) {
  int rounds = *(int*)arg;
  pid_t me = getpid();
  for (int r = 1; r <= rounds; r++) {
    int tid;
    while ((tid = atomic_load(&newborn_tid)) == 0) {
    }
    long ret = syscall(SYS_tgkill, me, tid, SIGUSR1);
    if (ret == 0) {
      sent++;
    } else if (errno == ESRCH) {
      refused++;
    } else {
      failed++;
    }
    atomic_store(&newborn_tid, 0);
    atomic_store(&round_done, r);
  }
  return NULL;
}

static int newborn_body(void* arg) {
  (void)arg;
  struct timespec ts = {0, 20 * 1000 * 1000};
  syscall(SYS_nanosleep, &ts, NULL);
  atomic_store(&newborn_done, 1);
  return 0;
}

static struct f_owner_ex race_owner;
static volatile int race_pid;
static _Atomic int race_stop;

/* Flips the shared f_owner_ex and owner pid between the guest and PID 1. */
static void* owner_flipper(void* arg) {
  pid_t self = *(pid_t*)arg;
  while (!atomic_load(&race_stop)) {
    race_owner.pid = 1;
    race_pid = 1;
    race_owner.pid = self;
    race_pid = self;
  }
  return NULL;
}

/* F_SETOWN_EX, then ioctl(FIOSETOWN), from memory another thread keeps
 * changing: the owner may be refused or set to the guest, never set to PID 1. */
static int owner_race(int attempts) {
  pid_t self = getpid();
  race_owner.type = F_OWNER_PID;
  race_owner.pid = self;
  int fds[2];
  if (pipe(fds) != 0) {
    return 1;
  }
  pthread_t flipper;
  pthread_create(&flipper, NULL, owner_flipper, &self);
  race_pid = self;
  int hermit_owner = 0, failed = 0;
  for (int i = 0; i < attempts; i++) {
    if (fcntl(fds[0], F_SETOWN_EX, &race_owner) != 0) {
      if (errno != EPERM) {
        failed++;
      }
      continue;
    }
    struct f_owner_ex got;
    if (fcntl(fds[0], F_GETOWN_EX, &got) == 0 && got.pid == 1) {
      hermit_owner++;
      fcntl(fds[0], F_SETOWN, self);
    }
  }
  printf("F_SETOWN_EX race: owner PID 1 %d times, other errors %d\n", hermit_owner, failed);
  hermit_owner = 0;
  failed = 0;
  int sockets[2];
  if (socketpair(AF_UNIX, SOCK_STREAM, 0, sockets) != 0) {
    return 1;
  }
  for (int i = 0; i < attempts; i++) {
    if (ioctl(sockets[0], FIOSETOWN, &race_pid) != 0) {
      if (errno != EPERM) {
        failed++;
      }
      continue;
    }
    if (fcntl(sockets[0], F_GETOWN) == 1) {
      hermit_owner++;
      fcntl(sockets[0], F_SETOWN, self);
    }
  }
  atomic_store(&race_stop, 1);
  pthread_join(flipper, NULL);
  printf("FIOSETOWN race: owner PID 1 %d times, other errors %d\n", hermit_owner, failed);
  return 0;
}

static int newborn(int rounds) {
  pthread_t killer;
  pthread_create(&killer, NULL, newborn_killer, &rounds);
  /* Each new thread inherits SIGUSR1 blocked, so the send stays pending and
   * is discarded at its exit: what is measured is the send's answer, and a
   * delivery before the thread's first ptrace stop fails Hermit's run
   * separately. */
  sigset_t usr1;
  sigemptyset(&usr1);
  sigaddset(&usr1, SIGUSR1);
  pthread_sigmask(SIG_BLOCK, &usr1, NULL);
  size_t size = 1 << 16;
  char* stack = malloc(size);
  for (int r = 1; r <= rounds; r++) {
    atomic_store(&newborn_done, 0);
    int tid = clone(newborn_body, stack + size,
                    CLONE_VM | CLONE_FS | CLONE_FILES | CLONE_SIGHAND | CLONE_THREAD |
                        CLONE_SYSVSEM | CLONE_PARENT_SETTID,
                    NULL, (int*)&newborn_tid);
    if (tid < 0) {
      report("clone", tid);
      return 1;
    }
    while (atomic_load(&round_done) != r) {
    }
    while (atomic_load(&newborn_done) == 0) {
    }
    /* Let the thread finish exiting before its stack is reused. */
    struct timespec ts = {0, 2 * 1000 * 1000};
    nanosleep(&ts, NULL);
  }
  pthread_join(killer, NULL);
  printf("tgkill(newborn): sent %d, ESRCH %d, other %d\n", sent, refused, failed);
  return 0;
}

int main(int argc, char** argv) {
  setvbuf(stdout, NULL, _IONBF, 0);
  struct sigaction action;
  memset(&action, 0, sizeof(action));
  action.sa_handler = on_usr1;
  sigaction(SIGUSR1, &action, NULL);
  const char* mode = argc > 1 ? argv[1] : "hermit";
  if (strcmp(mode, "hermit") == 0) {
    return hermit_tasks();
  }
  if (strcmp(mode, "nested") == 0) {
    return nested(0);
  }
  if (strcmp(mode, "nested-kill") == 0) {
    return nested(1);
  }
  if (strcmp(mode, "owner-race") == 0 && argc > 2) {
    return owner_race(atoi(argv[2]));
  }
  if (strcmp(mode, "newborn") == 0 && argc > 2) {
    return newborn(atoi(argv[2]));
  }
  fprintf(stderr, "usage: %s [hermit | nested | nested-kill | owner-race N | newborn N]\n",
          argv[0]);
  return 2;
}
