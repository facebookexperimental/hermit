/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Sends and armings of the signals Hermit's in-guest runtime keeps for itself
 * (SIGSYS, SIGSEGV, SIGTRAP), A5 step 3c slice 0a
 * (https://github.com/rrnewton/hermit/issues/4485). Run by
 * hermit-cli/tests/common/liteinst_in_guest_programs.rs; never run it
 * natively in the group modes, which signal the caller's process group or
 * every process it may signal.
 *
 *   sends     For each send call and each of the three signals: fork a child
 *             that waits on a pipe and exits 0, send it the signal, close the
 *             pipe, and print the call's result and how the child ended.
 *             Linux kills the child with the signal. Hermit's in-guest
 *             backends refuse each send with EPERM and the child exits 0.
 *             Hermit's ptrace backend kills the child too, except that a
 *             SIGTRAP from another process never reaches it and it exits 0
 *             (https://github.com/rrnewton/hermit/issues/4555).
 *   siginfo   Send each signal to a waiting child by rt_sigqueueinfo,
 *             rt_tgsigqueueinfo and pidfd_send_signal with a siginfo Linux
 *             refuses before any permission check: one at an unmapped
 *             address (EFAULT), one with a code whose layout Linux does not
 *             know and a nonzero expansion byte (E2BIG), and, for pidfd, one
 *             whose si_signo is not the call's signal (EINVAL). The child
 *             exits 0: nothing is sent.
 *   tail      Send each signal to a fresh waiting child by rt_sigqueueinfo,
 *             rt_tgsigqueueinfo and pidfd_send_signal with a siginfo whose
 *             copy depends on where it ends: one 8 bytes below the top of
 *             the address space (EFAULT); one of a layout Linux knows
 *             (SI_QUEUE) whose last 80 bytes lie on an unmapped page, which
 *             Linux never reads, so the send proceeds (Linux kills the
 *             child); and one of a layout Linux does not know with the same
 *             placement (EFAULT).
 *   exited    Send each signal by each call to a child that has exited but
 *             is not yet reaped (a zombie, which takes a signal: 0), then to
 *             one that has been reaped (ESRCH), as Linux answers.
 *   armings   Arm each signal as a notification signal (fcntl F_SETSIG,
 *             timer_create with SIGEV_SIGNAL, SIGEV_THREAD and
 *             SIGEV_THREAD_ID, prctl PR_SET_PDEATHSIG), then disarm
 *             (F_SETSIG 0, PR_SET_PDEATHSIG 0), then the answers Linux gives
 *             before it looks at the signal: F_SETSIG on a closed and on an
 *             O_PATH descriptor (EBADF), timer_create on an id with no clock
 *             (EINVAL) and on a clock with no timers (EOPNOTSUPP), that
 *             answer coming first also with an invalid notification or a
 *             SIGEV_THREAD_ID thread outside the process. Under
 *             Hermit's ptrace backend both timer_create calls succeed: Detcore's
 *             timers do not check the clock
 *             (https://github.com/rrnewton/hermit/issues/4556).
 *   group-kill0, group-killpgid, group-killall, group-pidfd
 *             Lead a process group of its own, then send SIGSYS to it by
 *             kill(0), kill(-pgrp), kill(-1) or pidfd_send_signal with
 *             PIDFD_SIGNAL_PROCESS_GROUP, which Hermit refuses by name before
 *             any reserved-signal rule applies.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/prctl.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#ifndef PIDFD_SIGNAL_PROCESS_GROUP
#define PIDFD_SIGNAL_PROCESS_GROUP (1U << 2)
#endif

static const int SIGNALS[] = {SIGSYS, SIGSEGV, SIGTRAP};

static const char *const CALLS[] = {
    "kill",
    "tkill",
    "tgkill",
    "rt_sigqueueinfo",
    "rt_tgsigqueueinfo",
    "pidfd_send_signal",
};

#define COUNT(array) (sizeof(array) / sizeof((array)[0]))

static const char *signal_name(int signal) {
  switch (signal) {
    case 0:
      return "0";
    case SIGSYS:
      return "SIGSYS";
    case SIGSEGV:
      return "SIGSEGV";
    case SIGTRAP:
      return "SIGTRAP";
    default:
      return "other";
  }
}

static const char *errno_name(int error) {
  switch (error) {
    case 0:
      return "0";
    case EPERM:
      return "EPERM";
    case ESRCH:
      return "ESRCH";
    case EBADF:
      return "EBADF";
    case EINVAL:
      return "EINVAL";
    case EFAULT:
      return "EFAULT";
    case E2BIG:
      return "E2BIG";
    case ENOSYS:
      return "ENOSYS";
    case EOPNOTSUPP:
      return "EOPNOTSUPP";
    default:
      return "other";
  }
}

static int open_pidfd(pid_t pid) {
  long pidfd = syscall(SYS_pidfd_open, pid, 0);
  if (pidfd < 0) {
    printf("pidfd_open failed: %s\n", errno_name(errno));
    exit(1);
  }
  return (int)pidfd;
}

/* Sends `signal` to `child` by `call`; `pidfd` names the child. */
static long send_with(const char *call, pid_t child, int pidfd, int signal) {
  if (strcmp(call, "kill") == 0) {
    return kill(child, signal);
  }
  if (strcmp(call, "tkill") == 0) {
    return syscall(SYS_tkill, child, signal);
  }
  if (strcmp(call, "tgkill") == 0) {
    return syscall(SYS_tgkill, child, child, signal);
  }
  if (strcmp(call, "rt_sigqueueinfo") == 0 ||
      strcmp(call, "rt_tgsigqueueinfo") == 0) {
    siginfo_t info;
    memset(&info, 0, sizeof(info));
    info.si_signo = signal;
    info.si_code = SI_QUEUE;
    info.si_pid = getpid();
    info.si_uid = getuid();
    if (call[3] == 's') {
      return syscall(SYS_rt_sigqueueinfo, child, signal, &info);
    }
    return syscall(SYS_rt_tgsigqueueinfo, child, child, signal, &info);
  }
  return syscall(SYS_pidfd_send_signal, pidfd, signal, NULL, 0);
}

static void print_send(const char *prefix, const char *call, int signal, long result,
                       int error) {
  printf("%s%s %s: rc=%ld errno=%s",
         prefix,
         call,
         signal_name(signal),
         result,
         errno_name(result < 0 ? error : 0));
}

static void print_status(const char *label, int status) {
  if (WIFEXITED(status)) {
    printf("%sexit %d\n", label, WEXITSTATUS(status));
  } else if (WIFSIGNALED(status)) {
    printf("%ssignal %d\n", label, WTERMSIG(status));
  } else {
    printf("%sstatus %#x\n", label, (unsigned)status);
  }
}

static int sends(void) {
  for (size_t c = 0; c < COUNT(CALLS); c++) {
    for (size_t s = 0; s < COUNT(SIGNALS); s++) {
      int signal = SIGNALS[s];
      int fds[2];
      if (pipe(fds) != 0) {
        perror("pipe");
        return 1;
      }
      pid_t child = fork();
      if (child < 0) {
        perror("fork");
        return 1;
      }
      if (child == 0) {
        char byte;
        close(fds[1]);
        /* EOF when the parent closes its end, after its send. */
        while (read(fds[0], &byte, 1) > 0) {
        }
        _exit(0);
      }
      close(fds[0]);
      int pidfd = open_pidfd(child);
      errno = 0;
      long result = send_with(CALLS[c], child, pidfd, signal);
      int error = errno;
      close(pidfd);
      close(fds[1]);
      int status = 0;
      if (waitpid(child, &status, 0) != child) {
        perror("waitpid");
        return 1;
      }
      print_send("", CALLS[c], signal, result, error);
      print_status(" child=", status);
    }
  }
  return 0;
}

static int siginfo_mode(void) {
  int fds[2];
  if (pipe(fds) != 0) {
    perror("pipe");
    return 1;
  }
  pid_t child = fork();
  if (child < 0) {
    perror("fork");
    return 1;
  }
  if (child == 0) {
    char byte;
    close(fds[1]);
    while (read(fds[0], &byte, 1) > 0) {
    }
    _exit(0);
  }
  close(fds[0]);
  int pidfd = open_pidfd(child);
  /* An address no mapping covers. */
  void *unmapped = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  if (unmapped == MAP_FAILED || munmap(unmapped, 4096) != 0) {
    perror("mmap");
    return 1;
  }
  for (size_t s = 0; s < COUNT(SIGNALS); s++) {
    int signal = SIGNALS[s];
    /* -8: below SI_DETHREAD and not SI_ASYNCNL, so a layout Linux does not
       know, whose expansion must be all zero. */
    siginfo_t expanded;
    memset(&expanded, 0, sizeof(expanded));
    expanded.si_signo = signal;
    expanded.si_code = -8;
    ((unsigned char *)&expanded)[sizeof(expanded) - 1] = 1;
    siginfo_t mismatched;
    memset(&mismatched, 0, sizeof(mismatched));
    mismatched.si_signo = SIGUSR1;
    mismatched.si_code = SI_QUEUE;
    static const char *const CASES[] = {"unmapped", "expansion", "mismatched"};
    for (size_t k = 0; k < COUNT(CASES); k++) {
      void *info = k == 0 ? unmapped : k == 1 ? (void *)&expanded : (void *)&mismatched;
      char prefix[32];
      snprintf(prefix, sizeof(prefix), "siginfo %s ", CASES[k]);
      long result;
      if (k < 2) {
        errno = 0;
        result = syscall(SYS_rt_sigqueueinfo, child, signal, info);
        print_send(prefix, "rt_sigqueueinfo", signal, result, errno);
        printf("\n");
        errno = 0;
        result = syscall(SYS_rt_tgsigqueueinfo, child, child, signal, info);
        print_send(prefix, "rt_tgsigqueueinfo", signal, result, errno);
        printf("\n");
      }
      errno = 0;
      result = syscall(SYS_pidfd_send_signal, pidfd, signal, info, 0);
      print_send(prefix, "pidfd_send_signal", signal, result, errno);
      printf("\n");
    }
  }
  close(pidfd);
  close(fds[1]);
  int status = 0;
  if (waitpid(child, &status, 0) != child) {
    perror("waitpid");
    return 1;
  }
  print_status("siginfo child=", status);
  return 0;
}

static int tail_mode(void) {
  /* Two pages; the second is unmapped, so a siginfo whose 48-byte head ends
     the first page has its 80-byte expansion there. */
  char *pages = mmap(NULL, 8192, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  if (pages == MAP_FAILED || munmap(pages + 4096, 4096) != 0) {
    perror("mmap");
    return 1;
  }
  siginfo_t *edge = (siginfo_t *)(pages + 4096 - 48);
  static const char *const SEND_CALLS[] = {
      "rt_sigqueueinfo", "rt_tgsigqueueinfo", "pidfd_send_signal"};
  static const char *const CASES[] = {"top", "known", "unknown"};
  for (size_t k = 0; k < COUNT(CASES); k++) {
    for (size_t c = 0; c < COUNT(SEND_CALLS); c++) {
      for (size_t s = 0; s < COUNT(SIGNALS); s++) {
        int signal = SIGNALS[s];
        void *info;
        if (k == 0) {
          info = (void *)(uintptr_t)-8;
        } else {
          memset(edge, 0, 48);
          edge->si_signo = signal;
          edge->si_code = k == 1 ? SI_QUEUE : -8;
          edge->si_pid = getpid();
          edge->si_uid = getuid();
          info = edge;
        }
        int fds[2];
        if (pipe(fds) != 0) {
          perror("pipe");
          return 1;
        }
        pid_t child = fork();
        if (child < 0) {
          perror("fork");
          return 1;
        }
        if (child == 0) {
          char byte;
          close(fds[1]);
          while (read(fds[0], &byte, 1) > 0) {
          }
          _exit(0);
        }
        close(fds[0]);
        int pidfd = open_pidfd(child);
        long result;
        errno = 0;
        if (c == 0) {
          result = syscall(SYS_rt_sigqueueinfo, child, signal, info);
        } else if (c == 1) {
          result = syscall(SYS_rt_tgsigqueueinfo, child, child, signal, info);
        } else {
          result = syscall(SYS_pidfd_send_signal, pidfd, signal, info, 0);
        }
        int error = errno;
        close(pidfd);
        close(fds[1]);
        int status = 0;
        if (waitpid(child, &status, 0) != child) {
          perror("waitpid");
          return 1;
        }
        char prefix[32];
        snprintf(prefix, sizeof(prefix), "tail %s ", CASES[k]);
        print_send(prefix, SEND_CALLS[c], signal, result, error);
        print_status(" child=", status);
      }
    }
  }
  return 0;
}

/* A child that exits 7 at once, and a pidfd for it. */
static pid_t exiting_child(int *pidfd) {
  pid_t child = fork();
  if (child < 0) {
    perror("fork");
    exit(1);
  }
  if (child == 0) {
    _exit(7);
  }
  *pidfd = open_pidfd(child);
  return child;
}

static void send_every_way(const char *prefix, pid_t child, int pidfd) {
  for (size_t c = 0; c < COUNT(CALLS); c++) {
    for (size_t s = 0; s < COUNT(SIGNALS); s++) {
      errno = 0;
      long result = send_with(CALLS[c], child, pidfd, SIGNALS[s]);
      print_send(prefix, CALLS[c], SIGNALS[s], result, errno);
      printf("\n");
    }
  }
}

static int exited(void) {
  int pidfd;
  pid_t child = exiting_child(&pidfd);
  siginfo_t info;
  memset(&info, 0, sizeof(info));
  /* Exited, and left a zombie: WNOWAIT reaps nothing. */
  if (waitid(P_PID, (id_t)child, &info, WEXITED | WNOWAIT) != 0) {
    perror("waitid");
    return 1;
  }
  send_every_way("zombie ", child, pidfd);
  int status = 0;
  if (waitpid(child, &status, 0) != child) {
    perror("waitpid");
    return 1;
  }
  print_status("zombie child=", status);
  close(pidfd);

  child = exiting_child(&pidfd);
  if (waitpid(child, &status, 0) != child) {
    perror("waitpid");
    return 1;
  }
  print_status("reaped child=", status);
  send_every_way("reaped ", child, pidfd);
  close(pidfd);
  return 0;
}

static void print_result(const char *call, int signal, long result) {
  printf("%s %s: rc=%ld errno=%s\n",
         call,
         signal_name(signal),
         result,
         errno_name(result < 0 ? errno : 0));
}

/* timer_create on `clock` notifying `signal` by `notify`; for
   SIGEV_THREAD_ID, thread `tid` (0: the caller). */
static long create_timer_for(clockid_t clock, int notify, int signal, pid_t tid) {
  struct sigevent event;
  memset(&event, 0, sizeof(event));
  event.sigev_notify = notify;
  event.sigev_signo = signal;
  if (notify == SIGEV_THREAD_ID) {
    event._sigev_un._tid = tid != 0 ? tid : (pid_t)syscall(SYS_gettid);
  }
  int timer = -1;
  errno = 0;
  long created = syscall(SYS_timer_create, clock, &event, &timer);
  int saved = errno;
  if (created == 0) {
    /* Created, never set: it never fires. */
    syscall(SYS_timer_delete, timer);
  }
  errno = saved;
  return created;
}

static long create_timer(clockid_t clock, int notify, int signal) {
  return create_timer_for(clock, notify, signal, 0);
}

static int armings(void) {
  static const struct {
    const char *name;
    int notify;
  } NOTIFY[] = {
      {"timer_create(SIGEV_SIGNAL)", SIGEV_SIGNAL},
      {"timer_create(SIGEV_THREAD)", SIGEV_THREAD},
      {"timer_create(SIGEV_THREAD_ID)", SIGEV_THREAD_ID},
  };
  int fds[2];
  if (pipe(fds) != 0) {
    perror("pipe");
    return 1;
  }
  for (size_t s = 0; s < COUNT(SIGNALS); s++) {
    int signal = SIGNALS[s];
    errno = 0;
    print_result("fcntl(F_SETSIG)", signal, fcntl(fds[0], F_SETSIG, signal));
    for (size_t n = 0; n < COUNT(NOTIFY); n++) {
      long created = create_timer(CLOCK_MONOTONIC, NOTIFY[n].notify, signal);
      print_result(NOTIFY[n].name, signal, created);
    }
    errno = 0;
    print_result("prctl(PR_SET_PDEATHSIG)",
                 signal,
                 prctl(PR_SET_PDEATHSIG, (unsigned long)signal, 0UL, 0UL, 0UL));
  }
  errno = 0;
  print_result("fcntl(F_SETSIG)", 0, fcntl(fds[0], F_SETSIG, 0));
  errno = 0;
  print_result("prctl(PR_SET_PDEATHSIG)",
               0,
               prctl(PR_SET_PDEATHSIG, 0UL, 0UL, 0UL, 0UL));
  int armed = -1;
  errno = 0;
  long got = prctl(PR_GET_PDEATHSIG, (unsigned long)&armed, 0UL, 0UL, 0UL);
  printf("prctl(PR_GET_PDEATHSIG): rc=%ld armed=%d\n", got, armed);

  /* Linux's answers that come before it looks at the signal. */
  int closed = fds[1];
  close(closed);
  errno = 0;
  print_result("fcntl(closed fd, F_SETSIG)", SIGSEGV, fcntl(closed, F_SETSIG, SIGSEGV));
  int path = open("/", O_PATH);
  if (path < 0) {
    perror("open O_PATH");
    return 1;
  }
  errno = 0;
  print_result("fcntl(O_PATH fd, F_SETSIG)", SIGSEGV, fcntl(path, F_SETSIG, SIGSEGV));
  close(path);
  print_result("timer_create(clock 10, SIGEV_SIGNAL)",
               SIGSEGV,
               create_timer((clockid_t)10, SIGEV_SIGNAL, SIGSEGV));
  print_result("timer_create(CLOCK_MONOTONIC_RAW, SIGEV_SIGNAL)",
               SIGSEGV,
               create_timer(CLOCK_MONOTONIC_RAW, SIGEV_SIGNAL, SIGSEGV));
  /* The clock's answer comes before the notification's: EOPNOTSUPP, not
     EINVAL, for an invalid notification or a thread outside the caller's
     process. */
  print_result("timer_create(CLOCK_MONOTONIC_RAW, notify 99)",
               SIGSEGV,
               create_timer(CLOCK_MONOTONIC_RAW, 99, SIGSEGV));
  print_result("timer_create(CLOCK_MONOTONIC_RAW, SIGEV_THREAD_ID tid 1)",
               SIGSEGV,
               create_timer_for(CLOCK_MONOTONIC_RAW, SIGEV_THREAD_ID, SIGSEGV, 1));
  close(fds[0]);
  return 0;
}

static int group(const char *mode) {
  /* Only this process is in the group it signals. */
  if (setpgid(0, 0) != 0) {
    perror("setpgid");
    return 1;
  }
  printf("own group: %s\n", getpgrp() == getpid() ? "yes" : "no");
  fflush(stdout);
  long result;
  errno = 0;
  if (strcmp(mode, "group-kill0") == 0) {
    result = kill(0, SIGSYS);
  } else if (strcmp(mode, "group-killpgid") == 0) {
    result = kill(-getpgrp(), SIGSYS);
  } else if (strcmp(mode, "group-killall") == 0) {
    result = kill(-1, SIGSYS);
  } else {
    int pidfd = open_pidfd(getpid());
    result = syscall(SYS_pidfd_send_signal,
                     pidfd,
                     SIGSYS,
                     NULL,
                     PIDFD_SIGNAL_PROCESS_GROUP);
  }
  /* A run that refuses the call stops before this line. */
  print_result(mode, SIGSYS, result);
  return 0;
}

int main(int argc, char **argv) {
  if (argc != 2) {
    fprintf(stderr,
            "usage: %s sends|siginfo|tail|exited|armings|group-kill0|group-killpgid|group-killall|"
            "group-pidfd\n",
            argv[0]);
    return 2;
  }
  setvbuf(stdout, NULL, _IOLBF, 0);
  if (strcmp(argv[1], "sends") == 0) {
    return sends();
  }
  if (strcmp(argv[1], "exited") == 0) {
    return exited();
  }
  if (strcmp(argv[1], "siginfo") == 0) {
    return siginfo_mode();
  }
  if (strcmp(argv[1], "tail") == 0) {
    return tail_mode();
  }
  if (strcmp(argv[1], "armings") == 0) {
    return armings();
  }
  if (strncmp(argv[1], "group-", 6) == 0) {
    return group(argv[1]);
  }
  fprintf(stderr, "unknown mode %s\n", argv[1]);
  return 2;
}
