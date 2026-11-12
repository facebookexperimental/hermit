/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Every signal-sending call to a target that is not live, with Linux's
 * answers: a child that has exited and has not been reaped (a zombie, which
 * Linux still finds and whose signal it drops, answering 0), the same child
 * once reaped (ESRCH), and a thread of this process that has exited (ESRCH).
 * A caller's siginfo is checked before the target is looked up (EFAULT,
 * E2BIG, EPERM), and the signal number only once it is found (EINVAL). Each
 * line is one call's answer, so a run compares line by line with the native
 * one.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

/* The part of a siginfo_t Linux copies first. */
#define COPIED 48

/* include/uapi/linux/pidfd.h, which older C libraries do not carry. */
#ifndef PIDFD_SIGNAL_THREAD
#define PIDFD_SIGNAL_THREAD (1UL << 0)
#endif

static void report(const char *label, long result) {
  if (result == 0) {
    printf("%s: 0\n", label);
  } else {
    printf("%s: -1 %s\n", label, strerrorname_np(errno));
  }
}

/* A siginfo for `signo` with `code`; `padding` puts a 1 past the copied
 * part, which a code of unknown layout must leave 0. */
static siginfo_t *siginfo(int signo, int code, int padding) {
  static siginfo_t info;
  memset(&info, 0, sizeof info);
  info.si_signo = signo;
  info.si_code = code;
  if (padding) {
    ((unsigned char *)&info)[COPIED] = 1;
  }
  return &info;
}

static long kill_(pid_t pid, int sig) { return syscall(SYS_kill, pid, sig); }

static long tgkill_(pid_t tgid, pid_t tid, int sig) {
  return syscall(SYS_tgkill, tgid, tid, sig);
}

static long tkill_(pid_t tid, int sig) { return syscall(SYS_tkill, tid, sig); }

static long queue(pid_t pid, int sig, void *info) {
  return syscall(SYS_rt_sigqueueinfo, pid, sig, info);
}

static long tgqueue(pid_t tgid, pid_t tid, int sig, void *info) {
  return syscall(SYS_rt_tgsigqueueinfo, tgid, tid, sig, info);
}

static long pidfd_send(int pidfd, int sig, void *info, unsigned flags) {
  return syscall(SYS_pidfd_send_signal, pidfd, sig, info, flags);
}

/* Every call, naming `pid`, and `pidfd` for it, with each kind of siginfo
 * check; `unmapped` is an address no siginfo can be read from. */
static void send_all(const char *who, pid_t pid, int pidfd, void *unmapped) {
  char label[96];
#define LABEL(what) (snprintf(label, sizeof label, "%s, %s", who, what), label)
  report(LABEL("kill"), kill_(pid, SIGUSR1));
  report(LABEL("kill 0"), kill_(pid, 0));
  report(LABEL("kill 99"), kill_(pid, 99));
  report(LABEL("tgkill"), tgkill_(pid, pid, SIGUSR1));
  report(LABEL("tgkill 0"), tgkill_(pid, pid, 0));
  report(LABEL("tgkill 99"), tgkill_(pid, pid, 99));
  report(LABEL("tgkill in another thread group"), tgkill_(getpid(), pid, SIGUSR1));
  report(LABEL("tkill"), tkill_(pid, SIGUSR1));
  report(LABEL("rt_sigqueueinfo"), queue(pid, SIGUSR1, siginfo(0, SI_QUEUE, 0)));
  report(LABEL("rt_sigqueueinfo 0"), queue(pid, 0, siginfo(0, SI_QUEUE, 0)));
  report(LABEL("rt_sigqueueinfo 99"), queue(pid, 99, siginfo(0, SI_QUEUE, 0)));
  report(LABEL("rt_sigqueueinfo SI_USER"), queue(pid, SIGUSR1, siginfo(0, SI_USER, 0)));
  report(LABEL("rt_sigqueueinfo unknown code with padding"),
         queue(pid, SIGUSR1, siginfo(0, -1000, 1)));
  report(LABEL("rt_sigqueueinfo bad pointer"), queue(pid, SIGUSR1, unmapped));
  report(LABEL("rt_tgsigqueueinfo"), tgqueue(pid, pid, SIGUSR1, siginfo(0, SI_QUEUE, 0)));
  report(LABEL("rt_tgsigqueueinfo 99"), tgqueue(pid, pid, 99, siginfo(0, SI_QUEUE, 0)));
  report(LABEL("rt_tgsigqueueinfo SI_TKILL"),
         tgqueue(pid, pid, SIGUSR1, siginfo(0, SI_TKILL, 0)));
  report(LABEL("rt_tgsigqueueinfo unknown code with padding"),
         tgqueue(pid, pid, SIGUSR1, siginfo(0, -1000, 1)));
  report(LABEL("rt_tgsigqueueinfo bad pointer"), tgqueue(pid, pid, SIGUSR1, unmapped));
  report(LABEL("pidfd_send_signal"), pidfd_send(pidfd, SIGUSR1, NULL, 0));
  report(LABEL("pidfd_send_signal 0"), pidfd_send(pidfd, 0, NULL, 0));
  report(LABEL("pidfd_send_signal 99"), pidfd_send(pidfd, 99, NULL, 0));
  report(LABEL("pidfd_send_signal thread scope"),
         pidfd_send(pidfd, SIGUSR1, NULL, PIDFD_SIGNAL_THREAD));
  report(LABEL("pidfd_send_signal SI_QUEUE"),
         pidfd_send(pidfd, SIGUSR1, siginfo(SIGUSR1, SI_QUEUE, 0), 0));
  report(LABEL("pidfd_send_signal SI_USER"),
         pidfd_send(pidfd, SIGUSR1, siginfo(SIGUSR1, SI_USER, 0), 0));
  report(LABEL("pidfd_send_signal other signo"),
         pidfd_send(pidfd, SIGUSR1, siginfo(SIGUSR2, SI_QUEUE, 0), 0));
  report(LABEL("pidfd_send_signal unknown code with padding"),
         pidfd_send(pidfd, SIGUSR1, siginfo(SIGUSR2, -1000, 1), 0));
  report(LABEL("pidfd_send_signal bad pointer"), pidfd_send(pidfd, SIGUSR1, unmapped, 0));
#undef LABEL
}

static void *exit_at_once(void *arg) {
  *(pid_t *)arg = (pid_t)syscall(SYS_gettid);
  return NULL;
}

int main(void) {
  setvbuf(stdout, NULL, _IONBF, 0);
  void *unmapped = mmap(NULL, 4096, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  if (unmapped == MAP_FAILED) {
    return 2;
  }
  pid_t child = fork();
  if (child < 0) {
    return 2;
  }
  if (child == 0) {
    _exit(0);
  }
  int pidfd = (int)syscall(SYS_pidfd_open, child, 0);
  if (pidfd < 0) {
    return 3;
  }
  siginfo_t exited;
  memset(&exited, 0, sizeof exited);
  if (waitid(P_PID, child, &exited, WEXITED | WNOWAIT) != 0) {
    return 4;
  }
  send_all("zombie", child, pidfd, unmapped);
  int status = 0;
  if (waitpid(child, &status, 0) != child) {
    return 5;
  }
  send_all("reaped", child, pidfd, unmapped);

  pid_t thread_id = 0;
  pthread_t thread;
  if (pthread_create(&thread, NULL, exit_at_once, &thread_id) != 0 ||
      pthread_join(thread, NULL) != 0) {
    return 6;
  }
  report("exited thread, tgkill", tgkill_(getpid(), thread_id, SIGUSR1));
  report("exited thread, tgkill 0", tgkill_(getpid(), thread_id, 0));
  report("exited thread, tkill", tkill_(thread_id, SIGUSR1));
  report("exited thread, rt_tgsigqueueinfo",
         tgqueue(getpid(), thread_id, SIGUSR1, siginfo(0, SI_QUEUE, 0)));
  report("exited thread, rt_tgsigqueueinfo SI_TKILL",
         tgqueue(getpid(), thread_id, SIGUSR1, siginfo(0, SI_TKILL, 0)));
  report("exited thread, kill", kill_(thread_id, SIGUSR1));
  return 0;
}
