/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * pidfd_send_signal's flags and the descriptors it accepts, as Linux answers
 * them (https://github.com/rrnewton/hermit/issues/4108). Since Linux 6.9 the
 * flags select a scope: PIDFD_SIGNAL_THREAD (the thread whose id is the pid),
 * PIDFD_SIGNAL_THREAD_GROUP (the process) or PIDFD_SIGNAL_PROCESS_GROUP; any
 * other bit, or two of them, is EINVAL. Besides a pidfd, a /proc/<pid>
 * directory names its process; any other file is EBADF.
 *
 * usage: pidfd_send_signal_flags [all | pgrp | hermit | sibling | record]
 *   all:  every case below, each printed as `<case> rc=<r> <errno> ran=<n>`,
 *         where ran counts the SIGUSR1 handler runs the call caused on the
 *         main thread. Last, a one-thread child blocked in pause() takes a
 *         PIDFD_SIGNAL_THREAD send and its handler exits 42: the scheduler
 *         must wake it (as for https://github.com/rrnewton/hermit/issues/4099).
 *         An O_PATH descriptor is EBADF, and a reaped child's /proc/<pid>
 *         ESRCH. Then a worker idling with SIGUSR1 unblocked takes a
 *         PIDFD_SIGNAL_THREAD send through its own /proc/<tid>; a child in
 *         pause() takes a send through /proc/<child>; a SIGKILL through an
 *         O_PATH /proc/self of this two-thread process is EBADF; and a SIGKILL kills a
 *         two-thread child through /proc/<child>, /proc/<child's worker tid>,
 *         its pidfd, and its pidfd with PIDFD_SIGNAL_THREAD.
 *   hermit: sends through /proc/1, Hermit's own process under Hermit, and
 *         through /proc/<tid> of another of its threads: each is EPERM. Run it
 *         only under Hermit.
 *   sibling: prints `process <pid>`, then sends with no flag through
 *         /proc/<tid> of a sibling thread, which names this two-thread
 *         process; Hermit refuses it by name, naming the process.
 *   record: the flag cases through the pidfd, and a pipe, under record and
 *         replay (which refuse a /proc descriptor by name).
 *   pgrp: PIDFD_SIGNAL_PROCESS_GROUP with signal 0, then with SIGUSR1. Hermit
 *         refuses both by name: the guest's process group is Hermit's, so even
 *         the existence check would answer for processes outside the guest.
 *         Never run this mode natively: it signals the caller's process group.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#ifndef PIDFD_SIGNAL_THREAD
#define PIDFD_SIGNAL_THREAD (1U << 0)
#define PIDFD_SIGNAL_THREAD_GROUP (1U << 1)
#define PIDFD_SIGNAL_PROCESS_GROUP (1U << 2)
#endif

static pthread_t main_thread;
static volatile sig_atomic_t ran, worker_ran;

static void on_usr1(int sig) {
  (void)sig;
  if (pthread_equal(pthread_self(), main_thread)) {
    ran++;
  } else {
    worker_ran++;
  }
}

static long send(int fd, int sig, unsigned flags) {
  return syscall(SYS_pidfd_send_signal, fd, sig, NULL, flags);
}

static void report(const char *what, long rc) {
  printf("%s rc=%ld %s ran=%d\n", what, rc, rc < 0 ? strerrorname_np(errno) : "-", (int)ran);
  ran = 0;
}

static int open_dir(const char *format, int a, int b) {
  char path[64];
  snprintf(path, sizeof path, format, a, b);
  return open(path, O_DIRECTORY | O_RDONLY);
}

static int pidfd;

static void exit_42(int sig) {
  (void)sig;
  _exit(42);
}

/* A worker sends with PIDFD_SIGNAL_THREAD while the main thread waits in
 * pthread_join with SIGUSR1 unblocked: the leader takes it. */
static void *thread_sender(void *unused) {
  (void)unused;
  long rc = send(pidfd, SIGUSR1, PIDFD_SIGNAL_THREAD);
  printf("worker, thread flag rc=%ld %s\n", rc, rc < 0 ? strerrorname_np(errno) : "-");
  return NULL;
}

static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t cond = PTHREAD_COND_INITIALIZER;
static int released;
static pid_t idler_tid;

/* A worker that records its id and idles on a condition variable (a futex
 * wait) with SIGUSR1 unblocked until released. */
static void *idle(void *unused) {
  (void)unused;
  pthread_mutex_lock(&lock);
  idler_tid = (pid_t)syscall(SYS_gettid);
  pthread_cond_broadcast(&cond);
  while (!released) pthread_cond_wait(&cond, &lock);
  pthread_mutex_unlock(&lock);
  return NULL;
}

static pthread_t start_idler(void) {
  pthread_mutex_lock(&lock);
  idler_tid = 0;
  released = 0;
  pthread_mutex_unlock(&lock);
  pthread_t idler;
  if (pthread_create(&idler, NULL, idle, NULL) != 0) _exit(70);
  pthread_mutex_lock(&lock);
  while (!idler_tid) pthread_cond_wait(&cond, &lock);
  pthread_mutex_unlock(&lock);
  return idler;
}

static void release_idler(pthread_t idler) {
  pthread_mutex_lock(&lock);
  released = 1;
  pthread_cond_broadcast(&cond);
  pthread_mutex_unlock(&lock);
  pthread_join(idler, NULL);
}

static void *pause_forever(void *unused) {
  (void)unused;
  for (;;) pause();
  return NULL;
}

/* A child with two threads, both in pause(); returns its pid and stores its
 * worker's id. */
static pid_t two_thread_child(pid_t *worker) {
  int ready[2];
  if (pipe(ready) != 0) _exit(71);
  pid_t child = fork();
  if (child == 0) {
    /* Forget the parent's earlier idler. */
    idler_tid = 0;
    released = 0;
    pthread_t thread;
    if (pthread_create(&thread, NULL, idle, NULL) != 0) _exit(72);
    pthread_mutex_lock(&lock);
    while (!idler_tid) pthread_cond_wait(&cond, &lock);
    pthread_mutex_unlock(&lock);
    if (write(ready[1], &idler_tid, sizeof idler_tid) != sizeof idler_tid) _exit(73);
    pause_forever(NULL);
  }
  if (read(ready[0], worker, sizeof *worker) != sizeof *worker) _exit(74);
  close(ready[0]);
  close(ready[1]);
  return child;
}

static void report_child(const char *what, pid_t child) {
  int status = 0;
  waitpid(child, &status, 0);
  printf("%s: %s %d\n", what, WIFEXITED(status) ? "exited" : "killed by signal",
         WIFEXITED(status) ? WEXITSTATUS(status) : WTERMSIG(status));
}

static void sigkill_child(const char *what, int how) {
  pid_t worker = 0;
  pid_t child = two_thread_child(&worker);
  int fd;
  unsigned flags = 0;
  if (how == 0) {
    fd = open_dir("/proc/%d", child, 0);
  } else if (how == 1) {
    fd = open_dir("/proc/%d", worker, 0);
  } else {
    fd = syscall(SYS_pidfd_open, child, 0);
    flags = how == 3 ? PIDFD_SIGNAL_THREAD : 0;
  }
  report(what, send(fd, SIGKILL, flags));
  report_child(what, child);
  close(fd);
}

/* The other threads of Hermit's process, read from /proc/1/task. */
static pid_t another_hermit_thread(void) {
  int task = open("/proc/1/task", O_DIRECTORY | O_RDONLY);
  if (task < 0) return 0;
  char buf[4096];
  long n = syscall(SYS_getdents64, task, buf, sizeof buf);
  close(task);
  for (long at = 0; at < n;) {
    struct {
      unsigned long long ino;
      long long off;
      unsigned short reclen;
      unsigned char type;
      char name[];
    } *entry = (void *)(buf + at);
    int tid = atoi(entry->name);
    if (tid > 1) return tid;
    at += entry->reclen;
  }
  return 0;
}

int main(int argc, char **argv) {
  setvbuf(stdout, NULL, _IONBF, 0);
  main_thread = pthread_self();
  signal(SIGUSR1, on_usr1);
  pidfd = syscall(SYS_pidfd_open, getpid(), 0);
  if (pidfd < 0) return 2;
  const char *mode = argc > 1 ? argv[1] : "all";

  if (!strcmp(mode, "hermit")) {
    int init = open("/proc/1", O_DIRECTORY | O_RDONLY);
    report("/proc/1, signal 0", send(init, 0, 0));
    report("/proc/1", send(init, SIGTERM, 0));
    int init_path = open("/proc/1", O_PATH | O_DIRECTORY);
    report("/proc/1, O_PATH", send(init_path, SIGTERM, 0));
    pid_t thread = another_hermit_thread();
    printf("another thread of process 1: %s\n", thread > 1 ? "found" : "none");
    int other = open_dir("/proc/%d", thread, 0);
    report("/proc/<hermit thread>, signal 0", send(other, 0, 0));
    report("/proc/<hermit thread>", send(other, SIGTERM, 0));
    report("/proc/<hermit thread>, thread", send(other, SIGTERM, PIDFD_SIGNAL_THREAD));
    return 0;
  }
  if (!strcmp(mode, "sibling")) {
    printf("process %d\n", getpid());
    pthread_t idler = start_idler();
    int sibling = open_dir("/proc/%d", idler_tid, 0);
    report("/proc/<sibling tid>", send(sibling, SIGUSR1, 0));
    release_idler(idler);
    return 0;
  }
  if (!strcmp(mode, "record")) {
    report("no flag", send(pidfd, SIGUSR1, 0));
    report("thread", send(pidfd, SIGUSR1, PIDFD_SIGNAL_THREAD));
    report("thread group", send(pidfd, SIGUSR1, PIDFD_SIGNAL_THREAD_GROUP));
    report("two scopes", send(pidfd, SIGUSR1, 3));
    report("unknown flag", send(pidfd, SIGUSR1, 1U << 3));
    int pipe_fds[2];
    if (pipe(pipe_fds) != 0) return 9;
    report("pipe", send(pipe_fds[0], SIGUSR1, 0));
    return 0;
  }
  if (!strcmp(mode, "pgrp")) {
    report("process group, signal 0", send(pidfd, 0, PIDFD_SIGNAL_PROCESS_GROUP));
    report("process group", send(pidfd, SIGUSR1, PIDFD_SIGNAL_PROCESS_GROUP));
    return 0;
  }

  report("no flag", send(pidfd, SIGUSR1, 0));
  report("thread", send(pidfd, SIGUSR1, PIDFD_SIGNAL_THREAD));
  report("thread group", send(pidfd, SIGUSR1, PIDFD_SIGNAL_THREAD_GROUP));
  report("two scopes", send(pidfd, SIGUSR1, PIDFD_SIGNAL_THREAD | PIDFD_SIGNAL_THREAD_GROUP));
  report("unknown flag", send(pidfd, SIGUSR1, 1U << 3));

  int process = open_dir("/proc/%d", getpid(), 0);
  report("/proc/<pid>", send(process, SIGUSR1, 0));
  report("/proc/<pid>, signal 0", send(process, 0, 0));
  report("/proc/<pid>, thread", send(process, SIGUSR1, PIDFD_SIGNAL_THREAD));
  report("/proc/<pid>, two scopes", send(process, SIGUSR1, 3));
  int self = open("/proc/self", O_DIRECTORY | O_RDONLY);
  report("/proc/self", send(self, SIGUSR1, 0));
  int task = open_dir("/proc/%d/task/%d", getpid(), getpid());
  report("/proc/<pid>/task/<tid>", send(task, SIGUSR1, 0));
  int status = open("/proc/self/status", O_RDONLY);
  report("/proc/self/status", send(status, SIGUSR1, 0));
  int root = open("/", O_DIRECTORY | O_RDONLY);
  report("/", send(root, SIGUSR1, 0));
  report("closed", send(1000, SIGUSR1, 0));
  /* Linux's fdget skips an O_PATH descriptor. */
  int opath = open("/proc/self", O_PATH | O_DIRECTORY);
  report("/proc/self, O_PATH", send(opath, SIGUSR1, 0));

  /* A reaped child's /proc/<pid>, opened while it lived, is ESRCH. */
  int gate[2];
  if (pipe(gate) != 0) return 10;
  pid_t reaped = fork();
  if (reaped == 0) {
    char byte;
    close(gate[1]);
    while (read(gate[0], &byte, 1) > 0) {
    }
    _exit(0);
  }
  close(gate[0]);
  int reaped_dir = open_dir("/proc/%d", reaped, 0);
  close(gate[1]);
  waitpid(reaped, NULL, 0);
  report("/proc/<reaped child>", send(reaped_dir, SIGUSR1, 0));
  report("/proc/<reaped child>, signal 0", send(reaped_dir, 0, 0));

  /* A child its parent ignores SIGCHLD for is reaped at its exit, so its pidfd
   * is ESRCH once it has exited, however late a tracer releases it. */
  signal(SIGCHLD, SIG_IGN);
  int go[2], done[2];
  if (pipe(go) != 0 || pipe(done) != 0) return 11;
  pid_t autoreaped = fork();
  if (autoreaped == 0) {
    char byte;
    close(go[1]);
    close(done[0]);
    while (read(go[0], &byte, 1) > 0) {
    }
    _exit(0);
  }
  close(go[0]);
  close(done[1]);
  int auto_fd = syscall(SYS_pidfd_open, autoreaped, 0);
  int auto_dir = open_dir("/proc/%d", autoreaped, 0);
  close(go[1]);
  char eof;
  while (read(done[0], &eof, 1) > 0) {
  }
  struct timespec nap = {0, 200 * 1000 * 1000};
  nanosleep(&nap, NULL);
  report("auto-reaped child, pidfd", send(auto_fd, SIGUSR1, 0));
  /* Linux copies and checks a siginfo before it finds the task missing. */
  siginfo_t info;
  memset(&info, 0, sizeof info);
  info.si_signo = SIGUSR1;
  info.si_code = SI_QUEUE;
  info.si_pid = getpid();
  info.si_uid = getuid();
  for (int dir = 0; dir < 2; dir++) {
    int fd = dir ? auto_dir : auto_fd;
    const char *what = dir ? "auto-reaped child, /proc" : "auto-reaped child, pidfd";
    char label[96];
    snprintf(label, sizeof label, "%s, bad info", what);
    report(label, syscall(SYS_pidfd_send_signal, fd, SIGUSR1, (siginfo_t *)1, 0));
    snprintf(label, sizeof label, "%s, SI_QUEUE info", what);
    report(label, syscall(SYS_pidfd_send_signal, fd, SIGUSR1, &info, 0));
    info.si_signo = SIGUSR2;
    snprintf(label, sizeof label, "%s, other si_signo", what);
    report(label, syscall(SYS_pidfd_send_signal, fd, SIGUSR1, &info, 0));
    info.si_signo = SIGUSR1;
    info.si_code = SI_USER;
    snprintf(label, sizeof label, "%s, forged SI_USER", what);
    report(label, syscall(SYS_pidfd_send_signal, fd, SIGUSR1, &info, 0));
    info.si_code = SI_QUEUE;
    /* The layout is checked against the info's own si_signo: FPE_FLTSUB is a
     * known code for SIGFPE (then the signals differ: EINVAL) but not for
     * SIGUSR1, whose nonzero expansion byte is E2BIG. */
    siginfo_t layout;
    memset(&layout, 0, sizeof layout);
    layout.si_signo = SIGFPE;
    layout.si_code = FPE_FLTSUB;
    ((unsigned char *)&layout)[127] = 1;
    snprintf(label, sizeof label, "%s, SIGFPE layout for SIGUSR1", what);
    report(label, syscall(SYS_pidfd_send_signal, fd, SIGUSR1, &layout, 0));
    layout.si_signo = SIGUSR1;
    snprintf(label, sizeof label, "%s, SIGUSR1 layout for SIGFPE", what);
    report(label, syscall(SYS_pidfd_send_signal, fd, SIGFPE, &layout, 0));
  }
  signal(SIGCHLD, SIG_DFL);

  pthread_t worker;
  if (pthread_create(&worker, NULL, thread_sender, NULL) != 0) return 3;
  pthread_join(worker, NULL);
  report("main after the worker's thread send", 0);

  int ready[2];
  if (pipe(ready) != 0) return 4;
  pid_t blocked = fork();
  if (blocked == 0) {
    signal(SIGUSR1, exit_42);
    if (write(ready[1], "r", 1) != 1) _exit(5);
    for (;;) pause();
  }
  char byte;
  if (read(ready[0], &byte, 1) != 1) return 6;
  int blocked_fd = syscall(SYS_pidfd_open, blocked, 0);
  report("blocked child, thread", send(blocked_fd, SIGUSR1, PIDFD_SIGNAL_THREAD));
  int child = 0;
  waitpid(blocked, &child, 0);
  printf("blocked child: %s %d\n", WIFEXITED(child) ? "exited" : "killed by signal",
         WIFEXITED(child) ? WEXITSTATUS(child) : WTERMSIG(child));

  pthread_t idler = start_idler();
  int sibling = open_dir("/proc/%d", idler_tid, 0);
  report("/proc/<sibling tid>, thread", send(sibling, SIGUSR1, PIDFD_SIGNAL_THREAD));
  release_idler(idler);
  printf("sibling ran=%d\n", (int)worker_ran);

  /* A SIGKILL through an O_PATH descriptor of this two-thread process is
   * EBADF, and kills nothing. */
  idler = start_idler();
  int opath_self = open("/proc/self", O_PATH | O_DIRECTORY);
  report("SIGKILL, /proc/self, O_PATH", send(opath_self, SIGKILL, 0));
  release_idler(idler);

  int paused_ready[2];
  if (pipe(paused_ready) != 0) return 7;
  pid_t paused = fork();
  if (paused == 0) {
    signal(SIGUSR1, exit_42);
    if (write(paused_ready[1], "r", 1) != 1) _exit(5);
    for (;;) pause();
  }
  if (read(paused_ready[0], &byte, 1) != 1) return 8;
  int paused_dir = open_dir("/proc/%d", paused, 0);
  report("/proc/<paused child>", send(paused_dir, SIGUSR1, 0));
  report_child("/proc/<paused child>", paused);

  sigkill_child("SIGKILL, /proc/<child>", 0);
  sigkill_child("SIGKILL, /proc/<child worker>", 1);
  sigkill_child("SIGKILL, pidfd", 2);
  sigkill_child("SIGKILL, pidfd, thread", 3);
  return 0;
}
