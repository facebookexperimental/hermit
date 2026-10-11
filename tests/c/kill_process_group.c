/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Signals addressed to a process group
 * (https://github.com/rrnewton/hermit/issues/4046). The guest first makes
 * itself a process group leader, so -getpgrp() is a real negative pid inside
 * Hermit's PID namespace too.
 *   - rt_sigqueueinfo has no process groups: Linux finds no process for pid
 *     0, a negative pid or -1 and returns ESRCH, natively and under Hermit.
 *   - kill(INT_MIN, sig) is ESRCH, for SIGKILL too: Linux rejects INT_MIN
 *     before anything else.
 *   - kill(0, sig), kill(-pgrp, sig) and kill(-1, sig) signal every process
 *     in a set; Detcore does not model that and refuses them by name. A
 *     fail-closed run stops with the policy-refusal status at kill(0);
 *     --allow-unsupported-syscalls returns ENOSYS for each.
 * kill(-1, sig) runs only with the argument "broadcast": natively it would
 * signal every process the user may signal. With the argument "inherited" the
 * guest keeps the process group it started in, which under Hermit is
 * Hermit's own (https://github.com/rrnewton/hermit/issues/4057), first
 * checks the group with signal 0, which sends nothing, and last sends
 * SIGKILL to it; run it only inside setsid. With the argument "group-only"
 * it reports whether it leads its group without changing it, sends SIGUSR1
 * to its group with kill(0) and returns: under --namespace-only that group is
 * Hermit's, and Hermit must refuse the call
 * (https://github.com/rrnewton/hermit/issues/4065). With "lead-then-kill" it
 * first leads a group of its own, whose kill(0) must still work. With
 * "other-routes" it stays in Hermit's group and tries every other route a
 * signal could take to that group from inside the namespace: rt_sigqueueinfo
 * to 0 or -1, tkill and tgkill to 0, a pidfd for its parent (pid 0 inside the
 * namespace), and kill(0) through the i386 syscall table (int $0x80), next to
 * an i386 getpid that must still work. Run all three only inside setsid.
 * With "tty" it runs on a terminal: it reads a line from it, tries
 * TIOCSIG on /dev/tty, then waits for Ctrl-C.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <sys/ioctl.h>
#include <limits.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>

#ifndef SYS_pidfd_open
#define SYS_pidfd_open 434
#endif

/* An i386 syscall from this 64-bit process: int $0x80 takes the i386 table,
 * where kill is 37 and getpid is 20. */
static long int80(long number, long first, long second) {
  long ret;
  __asm__ volatile("int $0x80" : "=a"(ret) : "a"(number), "b"(first), "c"(second) : "memory");
  return ret;
}

static void report_raw(const char* what, long ret) {
  if (ret < 0) {
    errno = (int)-ret;
    ret = -1;
  }
  if (ret == -1) {
    printf("%s: -1 %s\n", what, strerrorname_np(errno));
  } else {
    printf("%s: ok\n", what);
  }
}

static volatile sig_atomic_t handled;

static void on_usr1(int sig) {
  (void)sig;
  handled++;
}

static void report(const char* what, long ret) {
  if (ret == 0) {
    printf("%s: 0, handled %d\n", what, (int)handled);
  } else {
    printf("%s: %ld %s, handled %d\n", what, ret, strerrorname_np(errno), (int)handled);
  }
}

static long queue(pid_t pid) {
  siginfo_t info;
  memset(&info, 0, sizeof(info));
  info.si_signo = SIGUSR1;
  info.si_code = SI_QUEUE;
  info.si_pid = getpid();
  info.si_uid = getuid();
  return syscall(SYS_rt_sigqueueinfo, pid, SIGUSR1, &info);
}

int main(int argc, char** argv) {
  int broadcast = argc > 1 && strcmp(argv[1], "broadcast") == 0;
  int inherited = argc > 1 && strcmp(argv[1], "inherited") == 0;
  int group_only = argc > 1 && strcmp(argv[1], "group-only") == 0;
  int lead_then_kill = argc > 1 && strcmp(argv[1], "lead-then-kill") == 0;
  int other_routes = argc > 1 && strcmp(argv[1], "other-routes") == 0;
  if (argc > 1 && strcmp(argv[1], "tty") == 0) {
    char line[64];
    setvbuf(stdout, NULL, _IONBF, 0);
    printf("ready isatty=%d pgrp=%d\n", isatty(0), (int)getpgrp());
    if (fgets(line, sizeof line, stdin) == NULL) {
      return 4;
    }
    printf("got %s", line);
    int tty = open("/dev/tty", O_RDWR);
    report("ioctl(TIOCSIG)", ioctl(tty, TIOCSIG, SIGUSR1));
    printf("waiting\n");
    pause();
    return 0;
  }
  setvbuf(stdout, NULL, _IONBF, 0);
  struct sigaction action;
  memset(&action, 0, sizeof(action));
  action.sa_handler = on_usr1;
  sigaction(SIGUSR1, &action, NULL);

  if (other_routes) {
    printf("own group: %s, parent: %d\n", getpgrp() == getpid() ? "yes" : "no", (int)getppid());
    report("rt_sigqueueinfo(0)", queue(0));
    report("rt_sigqueueinfo(-1)", queue(-1));
    report("tkill(0)", syscall(SYS_tkill, 0, SIGUSR1));
    report("tgkill(0, 0)", syscall(SYS_tgkill, 0, 0, SIGUSR1));
    report("pidfd_open(parent)", syscall(SYS_pidfd_open, getppid(), 0));
    report_raw("i386 getpid", int80(20, 0, 0));
    report_raw("i386 kill(0)", int80(37, 0, SIGUSR1));
    printf("handled %d\n", (int)handled);
    return 0;
  }
  if (lead_then_kill) {
    (void)setpgid(0, 0);
  }
  if (group_only || lead_then_kill) {
    printf("own group: %s\n", getpgrp() == getpid() ? "yes" : "no");
    report("kill(0)", kill(0, SIGUSR1));
    return 0;
  }
  /* EPERM when the caller already leads a session, which is fine: it then
   * already leads its own group. */
  if (!inherited) {
    (void)setpgid(0, 0);
  }
  printf("own group: %s\n", getpgrp() == getpid() ? "yes" : "no");
  if (inherited) {
    report("kill(0, 0)", kill(0, 0));
  }
  report("rt_sigqueueinfo(0)", queue(0));
  report("rt_sigqueueinfo(-pgrp)", queue(-getpgrp()));
  report("rt_sigqueueinfo(-1)", queue(-1));
  report("kill(INT_MIN)", kill(INT_MIN, SIGUSR1));
  report("kill(INT_MIN, SIGKILL)", kill(INT_MIN, SIGKILL));
  report("kill(0)", kill(0, SIGUSR1));
  report("kill(-pgrp)", kill(-getpgrp(), SIGUSR1));
  if (inherited) {
    report("kill(0, SIGKILL)", kill(0, SIGKILL));
  }
  if (broadcast) {
    report("kill(-1)", kill(-1, SIGUSR1));
  }
  return 0;
}
