/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Armings of signal producers whose signal Linux sends at a moment host
 * timing decides, A5 step 3c slice 0b (design D28, D28a and D25,
 * https://github.com/rrnewton/hermit/issues/4485). Run by
 * hermit-cli/tests/common/liteinst_in_guest_programs.rs, natively and under
 * Hermit, from a writable working directory.
 *
 *   fcntl          Each operation of the F_SETOWN family once, printing its
 *                  result: the ones that complete a producer (an owner for a
 *                  socket or an O_ASYNC descriptor, O_ASYNC for a descriptor
 *                  with an owner, a lease, a directory notification), the
 *                  ones that do not (F_SETSIG alone, an owner for a plain
 *                  pipe, O_ASYNC with no owner, every disarm), and the answers
 *                  Linux gives before it arms anything (EBADF, ENOTDIR,
 *                  EINVAL, ENOTTY). Every arming that succeeds is disarmed
 *                  before anything could make its descriptor signal, and
 *                  SIGIO and SIGURG are ignored throughout.
 *   pdeathsig-arm  PR_SET_PDEATHSIG with each kind of value, each followed
 *                  by PR_GET_PDEATHSIG, disarming in between.
 *   pdeathsig-fire A child arms SIGUSR1 as its parent-death signal and sleeps
 *                  for an hour; its parent then exits. Linux kills the child
 *                  with SIGUSR1; if no signal came it would exit 3.
 *   pdeathsig-unscheduled
 *                  A vfork child arms SIGUSR1 as its parent-death signal and
 *                  kills its parent with SIGKILL (a death no system call of
 *                  the parent's own makes); the root reaps the parent.
 *   rlimit-cpu     setrlimit (through the C library, which uses prlimit64, and
 *                  through the raw setrlimit call) and prlimit64 of RLIMIT_CPU,
 *                  getrlimit, and a no-op setrlimit.
 *   cpu-burn       Burn about 3 s of CPU, then print "burned"; a CPU-time
 *                  soft limit of 1 s ends it with SIGXCPU first.
 *   noop           Print "ran" and exit 0.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/prctl.h>
#include <sys/resource.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#ifndef FIOSETOWN
#define FIOSETOWN 0x8901
#endif
#ifndef SIOCSPGRP
#define SIOCSPGRP 0x8902
#endif

static const char *errno_name(int error) {
  switch (error) {
    case 0:
      return "0";
    case EPERM:
      return "EPERM";
    case EBADF:
      return "EBADF";
    case EINVAL:
      return "EINVAL";
    case EAGAIN:
      return "EAGAIN";
    case ENOTDIR:
      return "ENOTDIR";
    case ENOTTY:
      return "ENOTTY";
    case ESRCH:
      return "ESRCH";
    case ENOSYS:
      return "ENOSYS";
    default:
      return "other";
  }
}

static void report(const char *what, long rc) {
  printf("%s: rc=%ld errno=%s\n", what, rc, errno_name(rc < 0 ? errno : 0));
}

static void die(const char *what) {
  printf("setup failed: %s: %s\n", what, errno_name(errno));
  exit(1);
}

static void make_pipe(int fds[2]) {
  if (pipe(fds) != 0) {
    die("pipe");
  }
}

static void make_socket(int fds[2]) {
  if (socketpair(AF_UNIX, SOCK_STREAM, 0, fds) != 0) {
    die("socketpair");
  }
}

static void close_pair(int fds[2]) {
  close(fds[0]);
  close(fds[1]);
}

/* Disarm `fd` completely: no owner, no O_ASYNC. */
static void disarm(int fd) {
  int flags = fcntl(fd, F_GETFL);
  if (flags >= 0) {
    fcntl(fd, F_SETFL, flags & ~O_ASYNC);
  }
  fcntl(fd, F_SETOWN, 0);
}

static long set_async(int fd, int on) {
  int flags = fcntl(fd, F_GETFL);
  if (flags < 0) {
    return -1;
  }
  return fcntl(fd, F_SETFL, on ? flags | O_ASYNC : flags & ~O_ASYNC);
}

static long set_owner_ex(int fd, int type, pid_t pid) {
  struct f_owner_ex owner = {.type = type, .pid = pid};
  return fcntl(fd, F_SETOWN_EX, &owner);
}

static long ioctl_int(int fd, unsigned long request, int value) {
  return ioctl(fd, request, &value);
}

/* A pid above PID_MAX_LIMIT (2^22), so no process, thread or group has it. */
#define NO_SUCH_PID 0x7ffffff0

/* In a fork child: run `op` on `fd`, print its result as `what`, disarm the
 * shared description and exit; the parent waits for it. */
static void in_fork_child(const char *what, int fd, int set_owner) {
  fflush(stdout);
  pid_t child = fork();
  if (child < 0) {
    die("fork");
  }
  if (child == 0) {
    long rc = set_owner ? fcntl(fd, F_SETOWN, getpid()) : set_async(fd, 1);
    report(what, rc);
    disarm(fd);
    fflush(stdout);
    _exit(0);
  }
  int status;
  if (waitpid(child, &status, 0) != child || !WIFEXITED(status) || WEXITSTATUS(status) != 0) {
    die("fork child");
  }
}

static int fcntl_family(void) {
  pid_t self = getpid();
  int fds[2];

  /* Lead a process group of its own, so the group owner below is one every
   * PID namespace the probe runs in can see. */
  report("setpgid(0, 0)", setpgid(0, 0));

  /* Refused where producers are: an owner on a socket. */
  make_socket(fds);
  report("F_SETOWN(pid) on a socket", fcntl(fds[0], F_SETOWN, self));
  disarm(fds[0]);
  report("F_SETOWN_EX(F_OWNER_PID, pid) on a socket", set_owner_ex(fds[0], F_OWNER_PID, self));
  disarm(fds[0]);
  report("F_SETOWN_EX(F_OWNER_TID, tid) on a socket",
         set_owner_ex(fds[0], F_OWNER_TID, (pid_t)syscall(SYS_gettid)));
  disarm(fds[0]);
  report("F_SETOWN_EX(F_OWNER_PGRP, pgrp) on a socket",
         set_owner_ex(fds[0], F_OWNER_PGRP, getpgrp()));
  disarm(fds[0]);
  report("ioctl(FIOSETOWN, pid) on a socket", ioctl_int(fds[0], FIOSETOWN, self));
  disarm(fds[0]);
  report("ioctl(SIOCSPGRP, pid) on a socket", ioctl_int(fds[0], SIOCSPGRP, self));
  disarm(fds[0]);
  report("F_SETOWN(-pgrp) on a socket", fcntl(fds[0], F_SETOWN, -getpgrp()));
  disarm(fds[0]);
  /* An owner that does not exist completes nothing: Linux's ESRCH. */
  report("F_SETOWN(no such pid) on a socket", fcntl(fds[0], F_SETOWN, NO_SUCH_PID));
  report("F_SETOWN(-no such pgrp) on a socket", fcntl(fds[0], F_SETOWN, -NO_SUCH_PID));
  report("F_SETOWN_EX(F_OWNER_PID, no such pid) on a socket",
         set_owner_ex(fds[0], F_OWNER_PID, NO_SUCH_PID));
  report("F_SETOWN_EX(F_OWNER_TID, no such tid) on a socket",
         set_owner_ex(fds[0], F_OWNER_TID, NO_SUCH_PID));
  report("F_SETOWN_EX(F_OWNER_PGRP, no such pgrp) on a socket",
         set_owner_ex(fds[0], F_OWNER_PGRP, NO_SUCH_PID));
  report("ioctl(FIOSETOWN, no such pid) on a socket", ioctl_int(fds[0], FIOSETOWN, NO_SUCH_PID));
  report("ioctl(SIOCSPGRP, no such pid) on a socket", ioctl_int(fds[0], SIOCSPGRP, NO_SUCH_PID));
  /* Allowed: every disarm on a socket. */
  report("F_SETOWN(0) on a socket", fcntl(fds[0], F_SETOWN, 0));
  report("F_SETOWN_EX(F_OWNER_PID, 0) on a socket", set_owner_ex(fds[0], F_OWNER_PID, 0));
  report("ioctl(FIOSETOWN, 0) on a socket", ioctl_int(fds[0], FIOSETOWN, 0));
  report("ioctl(SIOCSPGRP, 0) on a socket", ioctl_int(fds[0], SIOCSPGRP, 0));
  /* Allowed: O_ASYNC on an unowned socket. */
  report("F_SETFL(O_ASYNC) on an unowned socket", set_async(fds[0], 1));
  report("F_SETFL(~O_ASYNC) on an unowned socket", set_async(fds[0], 0));
  close_pair(fds);

  /* An owner on a pipe with O_ASYNC: refused. */
  make_pipe(fds);
  report("F_SETFL(O_ASYNC) on an unowned pipe", set_async(fds[0], 1));
  report("F_SETOWN(pid) on an O_ASYNC pipe", fcntl(fds[0], F_SETOWN, self));
  disarm(fds[0]);
  set_async(fds[0], 1);
  report("F_SETOWN_EX(F_OWNER_PID, pid) on an O_ASYNC pipe",
         set_owner_ex(fds[0], F_OWNER_PID, self));
  disarm(fds[0]);
  set_async(fds[0], 1);
  report("F_SETOWN(no such pid) on an O_ASYNC pipe", fcntl(fds[0], F_SETOWN, NO_SUCH_PID));
  /* FIOSETOWN and SIOCSPGRP reach no handler on a pipe. */
  report("ioctl(FIOSETOWN, pid) on an O_ASYNC pipe", ioctl_int(fds[0], FIOSETOWN, self));
  report("ioctl(SIOCSPGRP, pid) on an O_ASYNC pipe", ioctl_int(fds[0], SIOCSPGRP, self));
  report("F_SETFL(~O_ASYNC) on an unowned pipe", set_async(fds[0], 0));
  disarm(fds[0]);
  close_pair(fds);

  /* O_ASYNC on a pipe with an owner: refused; the owner itself is not. */
  make_pipe(fds);
  report("F_SETSIG(SIGUSR1) on a pipe", fcntl(fds[0], F_SETSIG, SIGUSR1));
  report("F_SETOWN(pid) on a pipe", fcntl(fds[0], F_SETOWN, self));
  printf("F_GETOWN on that pipe: %s\n", fcntl(fds[0], F_GETOWN) == self ? "pid" : "other");
  report("F_SETFL(O_ASYNC) on an owned pipe", set_async(fds[0], 1));
  disarm(fds[0]);
  fcntl(fds[0], F_SETOWN, self);
  report("ioctl(FIOASYNC, 1) on an owned pipe", ioctl_int(fds[0], FIOASYNC, 1));
  disarm(fds[0]);
  fcntl(fds[0], F_SETOWN, self);
  report("ioctl(FIOASYNC, 0) on an owned pipe", ioctl_int(fds[0], FIOASYNC, 0));
  report("F_SETFL(~O_ASYNC) on an owned pipe", set_async(fds[0], 0));
  report("F_SETOWN(0) on an owned pipe", fcntl(fds[0], F_SETOWN, 0));
  report("ioctl(FIOASYNC, 1) on an unowned pipe", ioctl_int(fds[0], FIOASYNC, 1));
  report("ioctl(FIOASYNC, 0) on an unowned pipe", ioctl_int(fds[0], FIOASYNC, 0));
  report("F_SETSIG(0) on a pipe", fcntl(fds[0], F_SETSIG, 0));
  disarm(fds[0]);
  close_pair(fds);

  /* Process-group owners. A group whose number a live child holds as its pid
   * but that no task leads yet: Linux sets it, and the child could form the
   * group later. A number only a session still holds (its leader gone, its
   * group empty): Linux sets it, and no signal can ever reach it. Group 1. */
  {
    int gate[2];
    make_pipe(gate);
    fflush(stdout);
    pid_t child = fork();
    if (child < 0) {
      die("fork");
    }
    if (child == 0) {
      char byte;
      close(gate[1]);
      if (read(gate[0], &byte, 1) < 0) {
        _exit(2);
      }
      _exit(0);
    }
    close(gate[0]);
    make_pipe(fds);
    set_async(fds[0], 1);
    report("F_SETOWN(-child leading no group) on an O_ASYNC pipe",
           fcntl(fds[0], F_SETOWN, -child));
    disarm(fds[0]);
    close_pair(fds);
    close(gate[1]);
    if (waitpid(child, NULL, 0) != child) {
      die("waitpid child");
    }
  }
  {
    int gate[2];
    int ready[2];
    make_pipe(gate);
    make_pipe(ready);
    fflush(stdout);
    pid_t leader = fork();
    if (leader < 0) {
      die("fork");
    }
    if (leader == 0) {
      /* A session leader whose own group empties when it exits: its one
       * child moves to a group of its own and outlives it. */
      if (setsid() < 0) {
        _exit(2);
      }
      pid_t member = fork();
      if (member < 0) {
        _exit(2);
      }
      if (member == 0) {
        char byte;
        if (setpgid(0, 0) != 0 || write(ready[1], "r", 1) != 1) {
          _exit(2);
        }
        close(gate[1]);
        if (read(gate[0], &byte, 1) < 0) {
          _exit(2);
        }
        _exit(0);
      }
      _exit(0);
    }
    close(ready[1]);
    char byte;
    if (read(ready[0], &byte, 1) != 1) {
      die("read ready");
    }
    close(ready[0]);
    if (waitpid(leader, NULL, 0) != leader) {
      die("waitpid leader");
    }
    close(gate[0]);
    make_pipe(fds);
    set_async(fds[0], 1);
    report("F_SETOWN(-a number only a session holds) on an O_ASYNC pipe",
           fcntl(fds[0], F_SETOWN, -leader));
    disarm(fds[0]);
    close_pair(fds);
    close(gate[1]);
  }
  {
    /* A child that has exited but is not yet reaped (a zombie) holds its pid
     * and can never form a group: Linux sets the owner, and no signal can
     * reach it. */
    fflush(stdout);
    pid_t child = fork();
    if (child < 0) {
      die("fork");
    }
    if (child == 0) {
      _exit(0);
    }
    siginfo_t info;
    if (waitid(P_PID, (id_t)child, &info, WEXITED | WNOWAIT) != 0) {
      die("waitid");
    }
    make_pipe(fds);
    set_async(fds[0], 1);
    report("F_SETOWN(-zombie child) on an O_ASYNC pipe", fcntl(fds[0], F_SETOWN, -child));
    disarm(fds[0]);
    close_pair(fds);
    if (waitpid(child, NULL, 0) != child) {
      die("waitpid zombie");
    }
  }
  make_socket(fds);
  report("F_SETOWN(-1) on a socket", fcntl(fds[0], F_SETOWN, -1));
  disarm(fds[0]);
  close_pair(fds);

  /* One open file description, two descriptors: the owner through one and
   * O_ASYNC through a duplicate are refused, in either order. */
  make_pipe(fds);
  fcntl(fds[0], F_SETOWN, self);
  int copy = dup(fds[0]);
  if (copy < 0) {
    die("dup");
  }
  report("F_SETFL(O_ASYNC) through a duplicate of an owned pipe", set_async(copy, 1));
  disarm(fds[0]);
  set_async(fds[0], 1);
  report("F_SETOWN(pid) through a duplicate of an O_ASYNC pipe", fcntl(copy, F_SETOWN, self));
  disarm(fds[0]);
  close(copy);
  /* The same across a fork: a child shares its parent's description. */
  fcntl(fds[0], F_SETOWN, self);
  in_fork_child("F_SETFL(O_ASYNC) in a fork child on its parent's owned pipe", fds[0], 0);
  disarm(fds[0]);
  set_async(fds[0], 1);
  in_fork_child("F_SETOWN(own pid) in a fork child on its parent's O_ASYNC pipe", fds[0], 1);
  disarm(fds[0]);
  close_pair(fds);

  /* Leases: refused; the disarm and a lease on a pipe keep Linux's answer. */
  int file = open("producer-file", O_CREAT | O_TRUNC | O_RDWR | O_CLOEXEC, 0600);
  if (file < 0) {
    die("open producer-file");
  }
  close(file);
  file = open("producer-file", O_RDONLY | O_CLOEXEC);
  if (file < 0) {
    die("reopen producer-file");
  }
  report("F_SETLEASE(F_RDLCK) on a file", fcntl(file, F_SETLEASE, F_RDLCK));
  report("F_SETLEASE(F_UNLCK) on a file", fcntl(file, F_SETLEASE, F_UNLCK));
  close(file);
  file = open("producer-file", O_RDWR | O_CLOEXEC);
  if (file < 0) {
    die("reopen producer-file read-write");
  }
  report("F_SETLEASE(F_WRLCK) on a file", fcntl(file, F_SETLEASE, F_WRLCK));
  report("F_SETLEASE(F_UNLCK) after it", fcntl(file, F_SETLEASE, F_UNLCK));
  /* Directory notification on a file: ENOTDIR before anything is armed. */
  report("F_NOTIFY(DN_MODIFY) on a file", fcntl(file, F_NOTIFY, DN_MODIFY));
  close(file);
  make_pipe(fds);
  report("F_SETLEASE(F_RDLCK) on a pipe", fcntl(fds[0], F_SETLEASE, F_RDLCK));
  close_pair(fds);

  /* Descriptors Linux refuses before any arming. */
  make_pipe(fds);
  close_pair(fds);
  report("F_SETOWN(pid) on a closed descriptor", fcntl(fds[0], F_SETOWN, self));
  int path = open(".", O_PATH | O_CLOEXEC);
  if (path < 0) {
    die("open O_PATH");
  }
  report("F_SETOWN(pid) on an O_PATH descriptor", fcntl(path, F_SETOWN, self));
  report("F_SETLEASE(F_RDLCK) on an O_PATH descriptor", fcntl(path, F_SETLEASE, F_RDLCK));
  close(path);

  /* Directory notification: refused; its disarm is not. Last, so nothing
   * changes the directory while it could be armed. */
  int dir = open(".", O_RDONLY | O_DIRECTORY | O_CLOEXEC);
  if (dir < 0) {
    die("open directory");
  }
  report("F_NOTIFY(DN_MODIFY) on a directory", fcntl(dir, F_NOTIFY, DN_MODIFY));
  report("F_NOTIFY(0) on a directory", fcntl(dir, F_NOTIFY, 0));
  report("F_NOTIFY(DN_MULTISHOT) on a directory", fcntl(dir, F_NOTIFY, DN_MULTISHOT));
  /* A bit that names no event makes no event either. */
  report("F_NOTIFY(unknown bit 0x40) on a directory", fcntl(dir, F_NOTIFY, 0x40));
  report("F_NOTIFY(0) after it", fcntl(dir, F_NOTIFY, 0));
  close(dir);
  if (unlink("producer-file") != 0) {
    die("unlink producer-file");
  }
  return 0;
}

static void pdeathsig_get(void) {
  int armed = -1;
  long rc = prctl(PR_GET_PDEATHSIG, &armed);
  printf("PR_GET_PDEATHSIG: rc=%ld armed=%d\n", rc, armed);
}

static int pdeathsig_arm(void) {
  static const int VALUES[] = {SIGUSR1, SIGUSR2, 0, SIGKILL, SIGSTOP, SIGALRM, 34, 64, 65};
  for (size_t i = 0; i < sizeof(VALUES) / sizeof(VALUES[0]); i++) {
    char what[64];
    snprintf(what, sizeof(what), "PR_SET_PDEATHSIG(%d)", VALUES[i]);
    report(what, prctl(PR_SET_PDEATHSIG, VALUES[i]));
    pdeathsig_get();
    report("PR_SET_PDEATHSIG(0)", prctl(PR_SET_PDEATHSIG, 0));
  }
  pdeathsig_get();
  return 0;
}

static int pdeathsig_fire(void) {
  int ready[2];
  make_pipe(ready);
  pid_t child = fork();
  if (child < 0) {
    die("fork");
  }
  if (child == 0) {
    close(ready[0]);
    if (prctl(PR_SET_PDEATHSIG, SIGUSR1) != 0) {
      _exit(2);
    }
    if (write(ready[1], "r", 1) != 1) {
      _exit(2);
    }
    sleep(3600);
    _exit(3);
  }
  close(ready[1]);
  char byte;
  if (read(ready[0], &byte, 1) != 1) {
    die("read ready");
  }
  printf("child armed; parent exits\n");
  return 0;
}

/* Burns about 3 s of CPU on the validation host (5e9 iterations at about
 * 0.6 ns each), with no system call in the loop, so the kernel's CPU-time
 * limit is what can stop it. A fixed count, not a clock: Hermit virtualizes
 * the clocks. */
static int cpu_burn(void) {
  volatile unsigned long sink = 0;
  for (unsigned long i = 1; i <= 5000000000UL; i++) {
    sink += i * i;
  }
  printf("burned\n");
  return 0;
}

/* A parent dies an unscheduled death while its child holds a parent-death
 * signal: the root forks a parent, whose vfork child arms SIGUSR1 as its
 * parent-death signal and kills that parent with SIGKILL, then waits for a
 * signal, so it ends only by one: Linux kills it with SIGUSR1 when the parent
 * dies. The root reaps the parent and prints how it ended. */
static int pdeathsig_unscheduled(void) {
  fflush(stdout);
  pid_t parent = fork();
  if (parent < 0) {
    die("fork");
  }
  if (parent == 0) {
    pid_t child = vfork();
    if (child < 0) {
      _exit(2);
    }
    if (child == 0) {
      if (prctl(PR_SET_PDEATHSIG, SIGUSR1) != 0) {
        _exit(2);
      }
      kill(getppid(), SIGKILL);
      for (;;) {
        pause();
      }
    }
    for (;;) {
      pause();
    }
  }
  int status = 0;
  if (waitpid(parent, &status, 0) != parent) {
    die("waitpid");
  }
  printf("parent ended by signal %d\n", WIFSIGNALED(status) ? WTERMSIG(status) : -1);
  return 0;
}

static int rlimit_cpu(void) {
  struct rlimit limit = {.rlim_cur = 10, .rlim_max = 10};
  report("setrlimit(RLIMIT_CPU, 10/10)", setrlimit(RLIMIT_CPU, &limit));
  struct rlimit raw = {.rlim_cur = 8, .rlim_max = 10};
  report("SYS_setrlimit(RLIMIT_CPU, 8/10)", syscall(SYS_setrlimit, RLIMIT_CPU, &raw));
  struct rlimit lower = {.rlim_cur = 5, .rlim_max = 10};
  report("prlimit64(0, RLIMIT_CPU, 5/10)",
         syscall(SYS_prlimit64, 0, RLIMIT_CPU, &lower, NULL));
  struct rlimit now;
  report("getrlimit(RLIMIT_CPU)", getrlimit(RLIMIT_CPU, &now));
  report("setrlimit(RLIMIT_CPU, unchanged)", setrlimit(RLIMIT_CPU, &now));
  return 0;
}

int main(int argc, char **argv) {
  if (argc != 2) {
    fprintf(stderr, "usage: %s fcntl|pdeathsig-arm|pdeathsig-fire|pdeathsig-unscheduled|rlimit-cpu|cpu-burn|noop\n", argv[0]);
    return 2;
  }
  setvbuf(stdout, NULL, _IOLBF, 0);
  signal(SIGIO, SIG_IGN);
  signal(SIGURG, SIG_IGN);
  if (strcmp(argv[1], "fcntl") == 0) {
    return fcntl_family();
  }
  if (strcmp(argv[1], "pdeathsig-arm") == 0) {
    return pdeathsig_arm();
  }
  if (strcmp(argv[1], "pdeathsig-fire") == 0) {
    return pdeathsig_fire();
  }
  if (strcmp(argv[1], "pdeathsig-unscheduled") == 0) {
    return pdeathsig_unscheduled();
  }
  if (strcmp(argv[1], "rlimit-cpu") == 0) {
    return rlimit_cpu();
  }
  if (strcmp(argv[1], "cpu-burn") == 0) {
    return cpu_burn();
  }
  if (strcmp(argv[1], "noop") == 0) {
    printf("ran\n");
    return 0;
  }
  fprintf(stderr, "unknown mode %s\n", argv[1]);
  return 2;
}
