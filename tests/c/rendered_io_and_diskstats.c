/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * rendered_io_and_diskstats: a task's io and /proc/diskstats, which Detcore
 * renders with no kernel file (the kvm lane's slice N3, package K of
 * https://github.com/rrnewton/hermit/issues/4213), answer as Linux's do.
 *
 * usage: rendered_io_and_diskstats --io-access
 *        rendered_io_and_diskstats --io-access-full-table
 *        rendered_io_and_diskstats --io-access-after-chroot
 *        rendered_io_and_diskstats --io-access-after-chroot-effective-only
 *        rendered_io_and_diskstats --io-access-after-chroot-own-userns
 *        rendered_io_and_diskstats --positioned-reads
 *        rendered_io_and_diskstats --positioned-reads-through-dotdot
 *        rendered_io_and_diskstats --sequence-across-fork
 *        rendered_io_and_diskstats --sequence-across-fork-through-dotdot
 *        rendered_io_and_diskstats --io-spellings
 *        rendered_io_and_diskstats --io-spellings-thread
 *        rendered_io_and_diskstats --diskstats-received
 *        rendered_io_and_diskstats --read-io-and-arch-status
 *        rendered_io_and_diskstats --stand-in-memfds
 *
 * --io-access: another process's /proc/<pid>/io as Linux's do_io_accounting
 *   decides it at every show, ptrace_may_access(PTRACE_MODE_READ_FSCREDS):
 *   a reader with CAP_SYS_PTRACE reads, dumpable target or not; without it, a
 *   target that made itself not dumpable refuses the reader with EACCES at
 *   the read (the open, by the file's owner, succeeds), also through a
 *   descriptor opened while it allowed it, and at a seek that runs show
 *   (which leaves the position at 0), while bytes a show already buffered are
 *   read on; a dumpable target refuses it too while the target's permitted
 *   capabilities are not a subset of the reader's effective ones (commoncap's
 *   cap_ptrace_access_check), and allows it once the target has dropped
 *   CAP_SYS_PTRACE as well; the reader's own process needs no permission;
 *   and once the target has exited and is not yet reaped, a descriptor opened
 *   while it ran still reads, rewinds, seeks and preads (Linux's read asks
 *   ptrace_may_access only, which passes; an open of the task's auxv, which
 *   Linux gives to global root once the task's memory is gone, would not).
 *   It prints the same lines natively, as root in a user namespace
 *   (unshare --user --map-root-user), and under Hermit.
 *
 * --io-access-full-table: another process's /proc/<pid>/io read, sought and
 *   pread through a descriptor opened before the reader filled its
 *   descriptor table: Linux needs no free descriptor to read an open file.
 *   The table fills as --diskstats-received's does.
 *
 * --io-access-after-chroot: another process's /proc/<pid>/io read through
 *   descriptors opened before the reader changed its root to /dev, a tree
 *   with no /proc: Linux's check, ptrace_may_access, does not depend on the
 *   reader's root, so the reader reads while the target allows it, gets
 *   EACCES while it is not dumpable, reads again once it is, and reads once
 *   it has exited and is not yet reaped. The reader has dropped
 *   CAP_SYS_PTRACE from its permitted set before forking the target, which
 *   inherits that set.
 *
 * --io-access-after-chroot-effective-only: the same read where the reader
 *   has dropped CAP_SYS_PTRACE from its effective set only, after forking a
 *   target that keeps it in its permitted set: Linux refuses with EACCES
 *   (commoncap's subset rule, the two sharing a user namespace), and again
 *   once the target has exited and is not yet reaped. Under Hermit the
 *   kernel beneath answers the check by number only with the reader's
 *   permitted set, so for the live target it is asked through a pidfd, and
 *   for the exited one, which has no namespaces left to ask about, Detcore's
 *   record of user namespaces answers.
 *
 * --io-access-after-chroot-own-userns: the same reader, whose target it
 *   made with clone(CLONE_NEWUSER), so that the target lives in a user
 *   namespace the reader owns and keeps every capability there: Linux reads
 *   both while the target lives and once it has exited (CAP_SYS_PTRACE over
 *   the target's namespace comes from owning it). Under Hermit the pidfd
 *   asks the kernel's check for the live target, which allows it, and for
 *   the exited one the record of user namespaces answers, as for the
 *   effective-only reader. First the target tries two clones that make no
 *   namespace: CLONE_NEWUSER with CLONE_FS, which Linux refuses with EINVAL
 *   whoever asks, and CLONE_NEWUSER alone, which it refuses with EPERM to a
 *   creator its own namespace does not map (the target's maps no ID, so its
 *   IDs read as the overflow ID). Neither changes what the reader reads.
 *
 * --positioned-reads: another process's io, statm, arch_status and cgroup,
 *   each a seq file of one record whose show Linux runs at a read at 0 and
 *   at a read or seek elsewhere than where the sequence is, positioned or
 *   not, and never for a read that continues the sequence or one at its end
 *   (seq_read_iter, seq_lseek): pread, preadv and read mixed on one
 *   descriptor; then, once the reader has dropped CAP_SYS_PTRACE from its
 *   effective set (the target's permitted set, which has it, is then not
 *   within the reader's effective set: commoncap refuses), io refuses each
 *   pread and preadv at 0, the read at the cursor, and the read after a
 *   rewind with EACCES, while a descriptor read to its end reads end of
 *   file and one whose first read buffered the record reads on, and reads
 *   again once the reader has it back; then seeks of io through fresh
 *   descriptors, which seq_lseek starts from the file's position and which
 *   traverse (running show) wherever the sequence is not, failing with
 *   EACCES there while the reader lacks CAP_SYS_PTRACE and leaving the
 *   position at 0, and whose traverse buffers the record a later read
 *   continues without show (the sequences of Claude's narrow check of the
 *   kvm lane's slice N3, round 4: p0 to p3); then, once the target is
 *   reaped, every file refuses pread and preadv at 0, the read, a seek to
 *   the cursor where it is past 0 (an empty record's cursor is at 0, where
 *   no traverse runs: the guest prints whether the answer is seq_lseek's for
 *   its cursor, since under Hermit arch_status is empty where natively it is
 *   not), and a seek to 5 with ESRCH, and a statm read before the reaping
 *   refuses pread at 0, a seek to the cursor and the read after it (e1).
 *
 * --positioned-reads-through-dotdot: the same, with every file opened as
 *   /proc/<pid>/../<pid>/<name>, a spelling Hermit leaves to the kernel to
 *   open (it serves the kernel's bytes, sanitized), so that the kernel's own
 *   show answers each read that runs one. Natively the spelling names the
 *   same files, and the guest prints the same lines.
 *
 * --sequence-across-fork: another process's io read through descriptors a
 *   fork shares: Linux keeps the file's position, where the sequence is and
 *   the buffered record in the one open file every process holding it shares
 *   (struct file and its seq_file), so a read by one process continues where
 *   a read by the other left the record, a positioned read by one moves the
 *   sequence for the other, and a read that continues the record runs no
 *   show, whoever reads and whatever its capabilities. Three descriptors: one
 *   the reader read 4 bytes of before forking a second reader, which reads 2
 *   more, after which the reader, without CAP_SYS_PTRACE in its effective
 *   set, reads the rest and then end of file (Codex's narrow check of the kvm
 *   lane's slice N3, round 4); one the fork shares before its first read, read
 *   4 bytes by the reader and 2 by the second reader, then, without
 *   CAP_SYS_PTRACE in either's effective set, 3 by the reader, the rest by the
 *   second reader, and end of file by the reader; and one the reader read 4
 *   bytes of before the fork, of which the second reader preads 2 bytes at 1,
 *   so that the reader's next read, without CAP_SYS_PTRACE, traverses and is
 *   refused with EACCES.
 *
 * --sequence-across-fork-through-dotdot: the same, with every file opened as
 *   /proc/<pid>/../<pid>/io, which Hermit leaves to the kernel to open.
 *
 * --io-spellings: under Hermit, which renders a task's io as zeros, every
 *   spelling of the calling thread's io reads every counter 0, where Linux
 *   shows the thread's own counters. It prints, for each spelling, "zeros",
 *   or the first counter that is not 0, or the errno.
 *
 * --io-spellings-thread: the same for a second thread's spellings.
 *
 * --diskstats-received: a /proc/diskstats descriptor received over
 *   SCM_RIGHTS by a process forked before the file was opened, whose
 *   descriptor table is then full, reads the table: read and pread at 0 both
 *   return the same nonempty bytes. Linux needs no free descriptor to read an
 *   open file. Natively the process lowers its RLIMIT_NOFILE to 64 to fill
 *   its table; under Hermit that limit is virtual, so the table fills at the
 *   real limit, which the test that runs it lowers for the whole run.
 *
 * --stand-in-memfds: two memfds the guest makes with the names of Detcore's
 *   stand-ins for /proc/diskstats and /proc/interrupts, with the four seals
 *   Detcore's carry, read as the empty memfds they are, as natively: sent
 *   over SCM_RIGHTS to a process forked before they existed; opened through
 *   /proc/self/fd/N, /proc/thread-self/fd/N, /proc/<pid>/fd/N and /dev/fd/N;
 *   by a process forked after them, through /proc/<pid>/fd/N; their links
 *   and fstat; and an unsealed one reopened through an O_PATH descriptor's
 *   link. Run at --proc=minimal, whose level omits both files, which must
 *   not appear through any of these.
 *
 * --read-io-and-arch-status: reads /proc/thread-self/io and
 *   /proc/self/arch_status and prints how many bytes each read returned: the
 *   reads a recording made before Detcore rendered these files holds
 *   differently, so its replay is refused by version.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <linux/capability.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/prctl.h>
#include <sys/resource.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <unistd.h>

static void die(const char* format, ...) {
  va_list args;
  va_start(args, format);
  fprintf(stderr, "rendered_io_and_diskstats: ");
  vfprintf(stderr, format, args);
  fprintf(stderr, "\n");
  va_end(args);
  exit(2);
}

/* Prints one line of the guest's report, unbuffered past fork. */
static void say(const char* format, ...) {
  va_list args;
  va_start(args, format);
  vprintf(format, args);
  printf("\n");
  va_end(args);
  fflush(stdout);
}

static const char* errno_name(int number) {
  switch (number) {
    case EACCES:
      return "EACCES";
    case EPERM:
      return "EPERM";
    case ESRCH:
      return "ESRCH";
    case ENOENT:
      return "ENOENT";
    case EBADF:
      return "EBADF";
    case EINVAL:
      return "EINVAL";
    case ELOOP:
      return "ELOOP";
    case EMFILE:
      return "EMFILE";
    case EOVERFLOW:
      return "EOVERFLOW";
    case EOPNOTSUPP:
      return "EOPNOTSUPP";
    default:
      return "another errno";
  }
}

static void send_descriptor(int socket, int fd);
static int receive_descriptor(int socket);

/* "opened" or the open's errno; *fd is the descriptor or -1. */
static const char* open_answer(const char* path, int* fd) {
  *fd = open(path, O_RDONLY | O_CLOEXEC);
  return *fd >= 0 ? "opened" : errno_name(errno);
}

/* "read" (some bytes), "end of file", or the read's errno. */
static const char* read_answer(int fd, size_t count) {
  char buffer[4096];
  if (count > sizeof(buffer)) {
    count = sizeof(buffer);
  }
  ssize_t got = read(fd, buffer, count);
  if (got > 0) {
    return "read";
  }
  return got == 0 ? "end of file" : errno_name(errno);
}

static const char* pread_answer(int fd) {
  char buffer[4096];
  ssize_t got = pread(fd, buffer, sizeof(buffer), 0);
  if (got > 0) {
    return "read";
  }
  return got == 0 ? "end of file" : errno_name(errno);
}

/* --io-access */

static int to_target[2];
static int from_target[2];

/* 'I' for EINVAL, 'P' for EPERM, 'O' for another errno, or 'S' where the
 * clone with `flags` made a child (which exits at once, and is reaped). */
static char clone_answer(long flags) {
  long child = syscall(SYS_clone, flags, 0, 0, 0, 0);
  if (child == 0) {
    _exit(0);
  }
  if (child > 0) {
    waitpid((pid_t)child, NULL, 0);
    return 'S';
  }
  return errno == EINVAL ? 'I' : errno == EPERM ? 'P' : 'O';
}

static const char* clone_answer_name(char answer) {
  switch (answer) {
    case 'I':
      return "EINVAL";
    case 'P':
      return "EPERM";
    case 'S':
      return "made a child";
    default:
      return "another errno";
  }
}

/* The target: changes its dumpability as told, and says when it has. */
static void target_main(void) {
  close(to_target[1]);
  close(from_target[0]);
  for (;;) {
    char command;
    if (read(to_target[0], &command, 1) != 1) {
      _exit(3);
    }
    if (command == 'q') {
      _exit(0);
    }
    if (command == 'u') {
      /* Two clones that make no user namespace, whose answers go back as
       * the acknowledgement's two bytes. */
      char answers[2] = {clone_answer(CLONE_NEWUSER | CLONE_FS | SIGCHLD),
                         clone_answer(CLONE_NEWUSER | SIGCHLD)};
      if (write(from_target[1], answers, sizeof(answers)) != sizeof(answers)) {
        _exit(5);
      }
      continue;
    }
    if (command == 'p') {
      /* Drop CAP_SYS_PTRACE from the permitted and effective sets. */
      struct __user_cap_header_struct header = {_LINUX_CAPABILITY_VERSION_3, 0};
      struct __user_cap_data_struct data[2];
      memset(data, 0, sizeof(data));
      if (syscall(SYS_capget, &header, data) != 0) {
        _exit(6);
      }
      data[CAP_SYS_PTRACE / 32].effective &= ~(1u << (CAP_SYS_PTRACE % 32));
      data[CAP_SYS_PTRACE / 32].permitted &= ~(1u << (CAP_SYS_PTRACE % 32));
      if (syscall(SYS_capset, &header, data) != 0) {
        _exit(7);
      }
    } else if (prctl(PR_SET_DUMPABLE, command == 'd', 0, 0, 0) != 0) {
      _exit(4);
    }
    if (write(from_target[1], "k", 1) != 1) {
      _exit(5);
    }
  }
}

static void tell_target(char command) {
  char ack;
  if (write(to_target[1], &command, 1) != 1) {
    die("write to the target: %s", strerror(errno));
  }
  if (command != 'q' && read(from_target[0], &ack, 1) != 1) {
    die("read from the target: %s", strerror(errno));
  }
}

/* Tells the target to exit and waits until it has, leaving it unreaped
 * (waitid's WNOWAIT): the wait ends once the task is a zombie, its memory
 * gone, and the waitpid that follows reaps it. A wait for its pipe's end
 * of file would do as natively, but under SaBRe the SIGCHLD of the exit can
 * interrupt that read in one run and not the other (measured: 3 of 6
 * --verify pairs of --io-access diverged there, before this change too;
 * https://github.com/rrnewton/hermit/issues/3862, SaBRe: a child's exit
 * SIGCHLD commits before or after the parent's wait depending on host
 * timing). */
static void await_target_exit(pid_t target) {
  tell_target('q');
  siginfo_t info;
  memset(&info, 0, sizeof(info));
  if (waitid(P_PID, (id_t)target, &info, WEXITED | WNOWAIT) != 0) {
    die("waitid: %s", strerror(errno));
  }
}

static int capability_data(struct __user_cap_data_struct data[2]) {
  struct __user_cap_header_struct header = {_LINUX_CAPABILITY_VERSION_3, 0};
  return (int)syscall(SYS_capget, &header, data);
}

static int has_effective(int capability) {
  struct __user_cap_data_struct data[2];
  memset(data, 0, sizeof(data));
  if (capability_data(data) != 0) {
    die("capget: %s", strerror(errno));
  }
  return (data[capability / 32].effective >> (capability % 32)) & 1;
}

/* Clears `capability` from the effective set only. */
static void drop_effective(int capability) {
  struct __user_cap_header_struct header = {_LINUX_CAPABILITY_VERSION_3, 0};
  struct __user_cap_data_struct data[2];
  memset(data, 0, sizeof(data));
  if (capability_data(data) != 0) {
    die("capget: %s", strerror(errno));
  }
  data[capability / 32].effective &= ~(1u << (capability % 32));
  if (syscall(SYS_capset, &header, data) != 0) {
    die("capset: %s", strerror(errno));
  }
}

/* Sets `capability` in the effective set again, from the permitted set. */
static void raise_effective(int capability) {
  struct __user_cap_header_struct header = {_LINUX_CAPABILITY_VERSION_3, 0};
  struct __user_cap_data_struct data[2];
  memset(data, 0, sizeof(data));
  if (capability_data(data) != 0) {
    die("capget: %s", strerror(errno));
  }
  data[capability / 32].effective |= 1u << (capability % 32);
  if (syscall(SYS_capset, &header, data) != 0) {
    die("capset: %s", strerror(errno));
  }
}

static void io_access(void) {
  if (pipe(to_target) != 0 || pipe(from_target) != 0) {
    die("pipe: %s", strerror(errno));
  }
  if (prctl(PR_SET_DUMPABLE, 1, 0, 0, 0) != 0) {
    die("PR_SET_DUMPABLE 1: %s", strerror(errno));
  }
  pid_t target = fork();
  if (target < 0) {
    die("fork: %s", strerror(errno));
  }
  if (target == 0) {
    target_main();
  }
  close(to_target[0]);
  close(from_target[1]);
  char path[64];
  snprintf(path, sizeof(path), "/proc/%d/io", (int)target);
  int fd;

  say("io access: CAP_SYS_PTRACE in the effective set: %d", has_effective(CAP_SYS_PTRACE));
  const char* opened = open_answer(path, &fd);
  say("io access: with CAP_SYS_PTRACE, dumpable: open %s, read %s", opened,
      fd >= 0 ? read_answer(fd, 4096) : "not tried");
  if (fd >= 0) {
    close(fd);
  }
  /* Opened while the target allows any reader: one never read, one whose
   * first read takes 10 bytes (its show buffers the whole file), and one
   * for a seek. */
  int unread = open(path, O_RDONLY | O_CLOEXEC);
  int partial = open(path, O_RDONLY | O_CLOEXEC);
  int seeker = open(path, O_RDONLY | O_CLOEXEC);
  if (unread < 0 || partial < 0 || seeker < 0) {
    die("open %s: %s", path, strerror(errno));
  }
  say("io access: a first read of 10 bytes: %s", read_answer(partial, 10));

  tell_target('n');
  opened = open_answer(path, &fd);
  say("io access: with CAP_SYS_PTRACE, not dumpable: open %s, read %s", opened,
      fd >= 0 ? read_answer(fd, 4096) : "not tried");
  if (fd >= 0) {
    close(fd);
  }

  drop_effective(CAP_SYS_PTRACE);
  say("io access: CAP_SYS_PTRACE in the effective set: %d", has_effective(CAP_SYS_PTRACE));
  say("io access: without CAP_SYS_PTRACE, not dumpable: a descriptor opened before reads %s",
      read_answer(unread, 4096));
  say("io access: without CAP_SYS_PTRACE, not dumpable: one whose first read buffered the "
      "file reads on: %s",
      read_answer(partial, 4096));
  off_t rewound = lseek(partial, 0, SEEK_SET);
  say("io access: without CAP_SYS_PTRACE, not dumpable: a rewind returns %lld, then a read "
      "%s",
      (long long)rewound, read_answer(partial, 4096));
  errno = 0;
  off_t sought = lseek(seeker, 5, SEEK_SET);
  int seek_errno = sought < 0 ? errno : 0;
  off_t position = lseek(seeker, 0, SEEK_CUR);
  say("io access: without CAP_SYS_PTRACE, not dumpable: a seek to 5 returns %lld (%s), and "
      "leaves the position at %lld",
      (long long)sought, sought < 0 ? errno_name(seek_errno) : "no error",
      (long long)position);
  opened = open_answer(path, &fd);
  if (fd >= 0) {
    const char* first = read_answer(fd, 4096);
    say("io access: without CAP_SYS_PTRACE, not dumpable: a new open %s, its read %s, its "
        "pread %s",
        opened, first, pread_answer(fd));
    close(fd);
  } else {
    say("io access: without CAP_SYS_PTRACE, not dumpable: a new open %s", opened);
  }
  const char* own_process = open_answer("/proc/self/io", &fd);
  const char* own_read = fd >= 0 ? read_answer(fd, 4096) : "not tried";
  if (fd >= 0) {
    close(fd);
  }
  const char* own_thread = open_answer("/proc/thread-self/io", &fd);
  const char* thread_read = fd >= 0 ? read_answer(fd, 4096) : "not tried";
  if (fd >= 0) {
    close(fd);
  }
  say("io access: without CAP_SYS_PTRACE, its own process's io: /proc/self/io %s and %s, "
      "/proc/thread-self/io %s and %s",
      own_process, own_read, own_thread, thread_read);

  tell_target('d');
  opened = open_answer(path, &fd);
  say("io access: without CAP_SYS_PTRACE, dumpable again, a target with CAP_SYS_PTRACE "
      "permitted: open %s, read %s",
      opened, fd >= 0 ? read_answer(fd, 4096) : "not tried");
  if (fd >= 0) {
    close(fd);
  }
  tell_target('p');
  opened = open_answer(path, &fd);
  say("io access: without CAP_SYS_PTRACE, dumpable, a target without CAP_SYS_PTRACE "
      "permitted: open %s, read %s",
      opened, fd >= 0 ? read_answer(fd, 4096) : "not tried");
  if (fd >= 0) {
    close(fd);
  }
  close(unread);
  close(partial);
  close(seeker);

  /* The target exits and is not reaped yet (await_target_exit). */
  raise_effective(CAP_SYS_PTRACE);
  int exited = open(path, O_RDONLY | O_CLOEXEC);
  if (exited < 0) {
    die("open %s: %s", path, strerror(errno));
  }
  await_target_exit(target);
  const char* first = read_answer(exited, 4096);
  off_t rewound_exited = lseek(exited, 0, SEEK_SET);
  const char* again = read_answer(exited, 4096);
  errno = 0;
  off_t sought_exited = lseek(exited, 5, SEEK_SET);
  int sought_errno = sought_exited < 0 ? errno : 0;
  say("io access: an exited target not yet reaped, through a descriptor opened while it ran: "
      "read %s, a rewind returns %lld then a read %s, a seek to 5 returns %lld (%s), pread %s",
      first, (long long)rewound_exited, again, (long long)sought_exited,
      sought_exited < 0 ? errno_name(sought_errno) : "no error", pread_answer(exited));
  close(exited);
  int status = 0;
  if (waitpid(target, &status, 0) != target || !WIFEXITED(status) ||
      WEXITSTATUS(status) != 0) {
    die("the target failed (status %#x)", status);
  }
}

/* --io-access-full-table */

static void io_access_full_table(void) {
  if (pipe(to_target) != 0 || pipe(from_target) != 0) {
    die("pipe: %s", strerror(errno));
  }
  if (prctl(PR_SET_DUMPABLE, 1, 0, 0, 0) != 0) {
    die("PR_SET_DUMPABLE 1: %s", strerror(errno));
  }
  pid_t target = fork();
  if (target < 0) {
    die("fork: %s", strerror(errno));
  }
  if (target == 0) {
    target_main();
  }
  close(to_target[0]);
  close(from_target[1]);
  char path[64];
  snprintf(path, sizeof(path), "/proc/%d/io", (int)target);
  int fd = open(path, O_RDONLY | O_CLOEXEC);
  if (fd < 0) {
    die("open %s: %s", path, strerror(errno));
  }
  struct rlimit limit = {64, 64};
  if (setrlimit(RLIMIT_NOFILE, &limit) != 0) {
    die("setrlimit: %s", strerror(errno));
  }
  enum { most = 8192 };
  static int fillers[most];
  int filled = 0;
  int fill_errno = 0;
  while (filled < most) {
    int filler = open("/dev/null", O_RDONLY | O_CLOEXEC);
    if (filler < 0) {
      fill_errno = errno;
      break;
    }
    fillers[filled++] = filler;
  }
  if (fill_errno != EMFILE || filled == 0) {
    die("the descriptor table did not fill: %d opened, then %s", filled,
        fill_errno ? strerror(fill_errno) : "no error");
  }
  int probe = open("/dev/null", O_RDONLY | O_CLOEXEC);
  int full = probe < 0 && errno == EMFILE;
  if (probe >= 0) {
    close(probe);
  }
  const char* first = read_answer(fd, 4096);
  errno = 0;
  off_t sought = lseek(fd, 5, SEEK_SET);
  int seek_errno = sought < 0 ? errno : 0;
  const char* positioned = pread_answer(fd);
  while (filled > 0) {
    close(fillers[--filled]);
  }
  say("io full table: the table is full: %s", full ? "yes" : "no");
  say("io full table: another process's io through a descriptor opened before: read %s, a "
      "seek to 5 returns %lld (%s), pread %s",
      first, (long long)sought, sought < 0 ? errno_name(seek_errno) : "no error", positioned);
  close(fd);
  tell_target('q');
  int status = 0;
  if (waitpid(target, &status, 0) != target || !WIFEXITED(status) ||
      WEXITSTATUS(status) != 0) {
    die("the target failed (status %#x)", status);
  }
}

/* --io-access-after-chroot */

/* Clears `capability` from the permitted and effective sets. */
static void drop_permitted(int capability) {
  struct __user_cap_header_struct header = {_LINUX_CAPABILITY_VERSION_3, 0};
  struct __user_cap_data_struct data[2];
  memset(data, 0, sizeof(data));
  if (capability_data(data) != 0) {
    die("capget: %s", strerror(errno));
  }
  data[capability / 32].effective &= ~(1u << (capability % 32));
  data[capability / 32].permitted &= ~(1u << (capability % 32));
  if (syscall(SYS_capset, &header, data) != 0) {
    die("capset: %s", strerror(errno));
  }
}

/* Forks the target (target_main), with the reader's credentials and
 * capability sets as they are, and returns its pid. */
static pid_t fork_target(void) {
  if (pipe(to_target) != 0 || pipe(from_target) != 0) {
    die("pipe: %s", strerror(errno));
  }
  if (prctl(PR_SET_DUMPABLE, 1, 0, 0, 0) != 0) {
    die("PR_SET_DUMPABLE 1: %s", strerror(errno));
  }
  pid_t target = fork();
  if (target < 0) {
    die("fork: %s", strerror(errno));
  }
  if (target == 0) {
    target_main();
  }
  close(to_target[0]);
  close(from_target[1]);
  return target;
}

/* Changes the reader's root to /dev, which has no proc entry. */
static void enter_a_root_without_proc(void) {
  if (chroot("/dev") != 0) {
    die("chroot /dev: %s", strerror(errno));
  }
  if (chdir("/") != 0) {
    die("chdir /: %s", strerror(errno));
  }
  int fd;
  say("io after chroot: in a root without /proc, an open of /proc/self/io: %s",
      open_answer("/proc/self/io", &fd));
  if (fd >= 0) {
    close(fd);
  }
}

static void io_access_after_chroot(void) {
  drop_permitted(CAP_SYS_PTRACE);
  pid_t target = fork_target();
  char path[64];
  snprintf(path, sizeof(path), "/proc/%d/io", (int)target);
  int live = open(path, O_RDONLY | O_CLOEXEC);
  int refused = open(path, O_RDONLY | O_CLOEXEC);
  int again = open(path, O_RDONLY | O_CLOEXEC);
  int exited = open(path, O_RDONLY | O_CLOEXEC);
  if (live < 0 || refused < 0 || again < 0 || exited < 0) {
    die("open %s: %s", path, strerror(errno));
  }
  say("io after chroot: CAP_SYS_PTRACE in the effective set: %d", has_effective(CAP_SYS_PTRACE));
  say("io after chroot: before it, another process's io reads: %s", read_answer(live, 4096));
  if (lseek(live, 0, SEEK_SET) != 0) {
    die("rewind: %s", strerror(errno));
  }
  enter_a_root_without_proc();
  say("io after chroot: a descriptor opened before, rewound: read %s", read_answer(live, 4096));
  tell_target('n');
  say("io after chroot: once the target is not dumpable: read %s", read_answer(refused, 4096));
  tell_target('d');
  const char* first = read_answer(again, 4096);
  off_t rewound = lseek(again, 0, SEEK_SET);
  const char* second = read_answer(again, 4096);
  errno = 0;
  off_t sought = lseek(again, 5, SEEK_SET);
  int seek_errno = sought < 0 ? errno : 0;
  say("io after chroot: once it is dumpable again: read %s, a rewind returns %lld then a read "
      "%s, a seek to 5 returns %lld (%s), pread %s",
      first, (long long)rewound, second, (long long)sought,
      sought < 0 ? errno_name(seek_errno) : "no error", pread_answer(again));
  await_target_exit(target);
  const char* zombie = read_answer(exited, 4096);
  say("io after chroot: an exited target not yet reaped: read %s, pread %s", zombie,
      pread_answer(exited));
  close(live);
  close(refused);
  close(again);
  close(exited);
  int status = 0;
  if (waitpid(target, &status, 0) != target || !WIFEXITED(status) ||
      WEXITSTATUS(status) != 0) {
    die("the target failed (status %#x)", status);
  }
}

static void io_access_after_chroot_effective_only(void) {
  pid_t target = fork_target();
  char path[64];
  snprintf(path, sizeof(path), "/proc/%d/io", (int)target);
  int fd = open(path, O_RDONLY | O_CLOEXEC);
  if (fd < 0) {
    die("open %s: %s", path, strerror(errno));
  }
  drop_effective(CAP_SYS_PTRACE);
  say("io after chroot: CAP_SYS_PTRACE in the effective set: %d", has_effective(CAP_SYS_PTRACE));
  enter_a_root_without_proc();
  say("io after chroot: without CAP_SYS_PTRACE in the effective set, a target with it "
      "permitted: read %s",
      read_answer(fd, 4096));
  await_target_exit(target);
  say("io after chroot: without CAP_SYS_PTRACE in the effective set, once that target has "
      "exited and is not yet reaped: read %s",
      read_answer(fd, 4096));
  close(fd);
  int status = 0;
  if (waitpid(target, &status, 0) != target || !WIFEXITED(status) ||
      WEXITSTATUS(status) != 0) {
    die("the target failed (status %#x)", status);
  }
}

/* --io-access-after-chroot-own-userns */

/* Forks the target (target_main) into a new user namespace, owned by the
 * reader, with clone(CLONE_NEWUSER), and returns its pid. */
static pid_t fork_target_in_its_own_user_namespace(void) {
  if (pipe(to_target) != 0 || pipe(from_target) != 0) {
    die("pipe: %s", strerror(errno));
  }
  if (prctl(PR_SET_DUMPABLE, 1, 0, 0, 0) != 0) {
    die("PR_SET_DUMPABLE 1: %s", strerror(errno));
  }
  pid_t target = (pid_t)syscall(SYS_clone, CLONE_NEWUSER | SIGCHLD, 0, 0, 0, 0);
  if (target < 0) {
    die("clone(CLONE_NEWUSER): %s", strerror(errno));
  }
  if (target == 0) {
    target_main();
  }
  close(to_target[0]);
  close(from_target[1]);
  return target;
}

static void io_access_after_chroot_own_userns(void) {
  pid_t target = fork_target_in_its_own_user_namespace();
  char clones[2];
  if (write(to_target[1], "u", 1) != 1 || read(from_target[0], clones, 2) != 2) {
    die("ask the target to clone: %s", strerror(errno));
  }
  say("io after chroot: in its own user namespace, the target's clone with CLONE_NEWUSER and "
      "CLONE_FS: %s, with CLONE_NEWUSER alone: %s",
      clone_answer_name(clones[0]), clone_answer_name(clones[1]));
  char path[64];
  snprintf(path, sizeof(path), "/proc/%d/io", (int)target);
  int live = open(path, O_RDONLY | O_CLOEXEC);
  int exited = open(path, O_RDONLY | O_CLOEXEC);
  if (live < 0 || exited < 0) {
    die("open %s: %s", path, strerror(errno));
  }
  drop_effective(CAP_SYS_PTRACE);
  say("io after chroot: CAP_SYS_PTRACE in the effective set: %d", has_effective(CAP_SYS_PTRACE));
  enter_a_root_without_proc();
  say("io after chroot: without CAP_SYS_PTRACE in the effective set, a target in a user "
      "namespace the reader owns: read %s",
      read_answer(live, 4096));
  await_target_exit(target);
  say("io after chroot: without CAP_SYS_PTRACE in the effective set, that target once it has "
      "exited and is not yet reaped: read %s",
      read_answer(exited, 4096));
  close(live);
  close(exited);
  int status = 0;
  if (waitpid(target, &status, 0) != target || !WIFEXITED(status) ||
      WEXITSTATUS(status) != 0) {
    die("the target failed (status %#x)", status);
  }
}

/* --positioned-reads */

/* "read", "end of file", or the errno of a pread of up to `count` bytes at
 * `position`. */
static const char* pread_answer_at(int fd, size_t count, off_t position) {
  char buffer[4096];
  if (count > sizeof(buffer)) {
    count = sizeof(buffer);
  }
  ssize_t got = pread(fd, buffer, count, position);
  if (got > 0) {
    return "read";
  }
  return got == 0 ? "end of file" : errno_name(errno);
}

/* The same for a preadv at 0 into two vectors, of 2 bytes and the rest. */
static const char* preadv_answer(int fd) {
  char first[2];
  char rest[4094];
  struct iovec vectors[2] = {{first, sizeof(first)}, {rest, sizeof(rest)}};
  ssize_t got = preadv(fd, vectors, 2, 0);
  if (got > 0) {
    return "read";
  }
  return got == 0 ? "end of file" : errno_name(errno);
}

/* "ok" or the errno of a read, whatever it returned: under Hermit a
 * process's statm and cgroup hold other bytes than the host's, and its
 * arch_status is empty where the CPU the guest's CPUID instruction reports
 * has no AVX-512F (Linux prints its one line only on such a CPU). */
static const char* outcome(ssize_t got) {
  return got >= 0 ? "ok" : errno_name(errno);
}

/* "returns N (no error)" or "returns -1 (ERRNO)" for a seek to `position`
 * from `whence`. */
static const char* seek_from(int fd, off_t position, int whence, char* answer, size_t size) {
  errno = 0;
  off_t sought = lseek(fd, position, whence);
  if (sought < 0) {
    snprintf(answer, size, "returns -1 (%s)", errno_name(errno));
  } else {
    snprintf(answer, size, "returns %lld (no error)", (long long)sought);
  }
  return answer;
}

/* The same for a seek to `position` from the start. */
static const char* seek_answer(int fd, off_t position, char* answer, size_t size) {
  return seek_from(fd, position, SEEK_SET, answer, size);
}

/* Whether lseek(fd, 0, SEEK_CUR), once every show of the file fails with
 * ESRCH and its sequence is at 0, answers as seq_lseek does at the file's
 * position `cursor` (fs/seq_file.c:308-341): past 0 it traverses, the show
 * fails, and it returns -1 (ESRCH); at 0, where the sequence is, it runs
 * none and returns 0. */
static const char* seek_to_cursor_answer(int fd, size_t cursor, char* answer, size_t size) {
  errno = 0;
  off_t told = lseek(fd, 0, SEEK_CUR);
  int told_errno = told < 0 ? errno : 0;
  int as_linux = cursor > 0 ? told == -1 && told_errno == ESRCH : told == 0;
  if (as_linux) {
    snprintf(answer, size, "answers as seq_lseek does at the cursor");
  } else {
    snprintf(answer, size, "returns %lld (%s) at the cursor %zu, not seq_lseek's answer",
             (long long)told, told < 0 ? errno_name(told_errno) : "no error", cursor);
  }
  return answer;
}

/* Spells another process's per-process file `name` as /proc/<pid>/<name>,
 * or, `through_dotdot`, as /proc/<pid>/../<pid>/<name>. */
static void process_file_path(char* path, size_t size, pid_t pid, const char* name,
                              int through_dotdot) {
  if (through_dotdot) {
    snprintf(path, size, "/proc/%d/../%d/%s", (int)pid, (int)pid, name);
  } else {
    snprintf(path, size, "/proc/%d/%s", (int)pid, name);
  }
}

static void positioned_reads(int through_dotdot) {
  static const char* const names[] = {"io", "statm", "arch_status", "cgroup"};
  enum { files = sizeof(names) / sizeof(names[0]) };
  pid_t target = fork_target();
  int fds[files];
  char path[96];
  for (size_t index = 0; index < files; index++) {
    process_file_path(path, sizeof(path), target, names[index], through_dotdot);
    fds[index] = open(path, O_RDONLY | O_CLOEXEC);
    if (fds[index] < 0) {
      die("open %s: %s", path, strerror(errno));
    }
  }
  process_file_path(path, sizeof(path), target, "io", through_dotdot);
  int to_end = open(path, O_RDONLY | O_CLOEXEC);
  int partial = open(path, O_RDONLY | O_CLOEXEC);
  int positioned = open(path, O_RDONLY | O_CLOEXEC);
  /* Claude's sequences p0 to p3, each on a descriptor of its own. */
  int seeks[4];
  for (size_t index = 0; index < 4; index++) {
    seeks[index] = open(path, O_RDONLY | O_CLOEXEC);
    if (seeks[index] < 0) {
      die("open %s: %s", path, strerror(errno));
    }
  }
  if (to_end < 0 || partial < 0 || positioned < 0) {
    die("open %s: %s", path, strerror(errno));
  }
  /* Claude's sequence e1, on a statm descriptor of its own. */
  process_file_path(path, sizeof(path), target, "statm", through_dotdot);
  int reaped_statm = open(path, O_RDONLY | O_CLOEXEC);
  if (reaped_statm < 0) {
    die("open %s: %s", path, strerror(errno));
  }
  /* Each file opened write-only, which seq_lseek seeks all the same, and
   * once more for the seeks past LLONG_MAX. */
  int write_only[files];
  int overflow[files];
  for (size_t index = 0; index < files; index++) {
    process_file_path(path, sizeof(path), target, names[index], through_dotdot);
    write_only[index] = open(path, O_WRONLY | O_CLOEXEC);
    overflow[index] = open(path, O_RDONLY | O_CLOEXEC);
    if (write_only[index] < 0 || overflow[index] < 0) {
      die("open %s: %s", path, strerror(errno));
    }
  }
  /* Where each file's position is after its reads below: the bytes its two
   * reads returned. */
  size_t cursors[files];
  for (size_t index = 0; index < files; index++) {
    int fd = fds[index];
    char buffer[4096];
    char first_vector[2];
    struct iovec vectors[2] = {{first_vector, sizeof(first_vector)}, {buffer, sizeof(buffer)}};
    const char* first = outcome(pread(fd, buffer, 4, 0));
    const char* again = outcome(pread(fd, buffer, sizeof(buffer), 0));
    const char* vectored = outcome(preadv(fd, vectors, 2, 0));
    ssize_t two = read(fd, buffer, 2);
    const char* at_cursor = outcome(two);
    ssize_t rest = read(fd, buffer, sizeof(buffer));
    const char* on = outcome(rest);
    cursors[index] = (size_t)(two > 0 ? two : 0) + (size_t)(rest > 0 ? rest : 0);
    const char* elsewhere = outcome(pread(fd, buffer, sizeof(buffer), 1));
    const char* last = outcome(pread(fd, buffer, sizeof(buffer), 0));
    say("positioned reads: %s: a pread of 4 bytes at 0 %s, a pread at 0 %s, a preadv at 0 %s, a "
        "read of 2 bytes %s, a read %s, a pread at 1 %s, a pread at 0 %s",
        names[index], first, again, vectored, at_cursor, on, elsewhere, last);
  }
  const char* whole = read_answer(to_end, 4096);
  const char* end = read_answer(to_end, 4096);
  const char* four = read_answer(partial, 4);
  say("positioned reads: io read to its end: a read %s, a read %s; a read of 4 bytes %s; a "
      "pread of 2 bytes at 0 %s",
      whole, end, four, pread_answer_at(positioned, 2, 0));

  drop_effective(CAP_SYS_PTRACE);
  int io = fds[0];
  const char* refused = pread_answer_at(io, 4096, 0);
  const char* vectored = preadv_answer(io);
  const char* at_cursor = read_answer(io, 4096);
  char sought[64];
  seek_answer(io, 0, sought, sizeof(sought));
  /* The rewind moved io's position to 0, and nothing below moves it. */
  cursors[0] = 0;
  const char* rewound = read_answer(io, 4096);
  say("positioned reads: io without CAP_SYS_PTRACE in the effective set: a pread at 0 %s, a "
      "preadv at 0 %s, a read %s, a rewind %s then a read %s",
      refused, vectored, at_cursor, sought, rewound);
  const char* ended = read_answer(to_end, 4096);
  const char* buffered = read_answer(partial, 4096);
  const char* continued = pread_answer_at(positioned, 4096, 2);
  const char* traversed = pread_answer_at(positioned, 4096, 1);
  say("positioned reads: io without CAP_SYS_PTRACE in the effective set, through descriptors "
      "read before: one at its end reads %s, one whose read buffered the record reads %s, one "
      "whose pread buffered it: a pread at 2 %s, then a pread at 1 %s",
      ended, buffered, continued, traversed);
  raise_effective(CAP_SYS_PTRACE);
  say("positioned reads: io with CAP_SYS_PTRACE again: a pread at 0 %s",
      pread_answer_at(io, 4096, 0));

  /* p0: a fresh descriptor sought to 5 traverses there, and a read at 5
   * continues the record the traverse buffered, with no show. */
  char told[64];
  seek_answer(seeks[0], 5, sought, sizeof(sought));
  drop_effective(CAP_SYS_PTRACE);
  const char* continues = read_answer(seeks[0], 4096);
  raise_effective(CAP_SYS_PTRACE);
  say("positioned reads: io, p0: a fresh descriptor sought to 5 %s; without CAP_SYS_PTRACE in "
      "the effective set a read %s",
      sought, continues);
  /* p1: after a refused pread the sequence is at 0, so a seek to the cursor
   * traverses, is refused, and leaves the position at 0, where a read shows
   * once allowed. */
  const char* first_read = read_answer(seeks[1], 4096);
  drop_effective(CAP_SYS_PTRACE);
  const char* refused_again = pread_answer_at(seeks[1], 4096, 0);
  seek_from(seeks[1], 0, SEEK_CUR, told, sizeof(told));
  raise_effective(CAP_SYS_PTRACE);
  say("positioned reads: io, p1: a read %s; without CAP_SYS_PTRACE a pread at 0 %s, then "
      "lseek(0, SEEK_CUR) %s; with it again a read %s",
      first_read, refused_again, told, read_answer(seeks[1], 4096));
  /* p2: the same with a seek to 5. */
  first_read = read_answer(seeks[2], 4096);
  drop_effective(CAP_SYS_PTRACE);
  refused_again = pread_answer_at(seeks[2], 4096, 0);
  seek_answer(seeks[2], 5, sought, sizeof(sought));
  raise_effective(CAP_SYS_PTRACE);
  say("positioned reads: io, p2: a read %s; without CAP_SYS_PTRACE a pread at 0 %s, then a seek "
      "to 5 %s; with it again a read %s",
      first_read, refused_again, sought, read_answer(seeks[2], 4096));
  /* p3: a seek to 5 once allowed again traverses, and a read at 5 without
   * the capability continues the record it buffered. */
  first_read = read_answer(seeks[3], 4096);
  drop_effective(CAP_SYS_PTRACE);
  refused_again = pread_answer_at(seeks[3], 4096, 0);
  raise_effective(CAP_SYS_PTRACE);
  seek_answer(seeks[3], 5, sought, sizeof(sought));
  drop_effective(CAP_SYS_PTRACE);
  continues = read_answer(seeks[3], 4096);
  raise_effective(CAP_SYS_PTRACE);
  say("positioned reads: io, p3: a read %s; without CAP_SYS_PTRACE a pread at 0 %s; with it a "
      "seek to 5 %s; without it again a read %s",
      first_read, refused_again, sought, continues);
  /* Write-only: seq_lseek never looks at the access mode (fs/seq_file.c:
   * 308-341), so a seek to 5 traverses, running show, and returns 5; a seek
   * to where the sequence is runs none. */
  size_t write_only_at[files];
  for (size_t index = 0; index < files; index++) {
    char again[64];
    seek_answer(write_only[index], 5, sought, sizeof(sought));
    seek_from(write_only[index], 0, SEEK_CUR, told, sizeof(told));
    seek_answer(write_only[index], 5, again, sizeof(again));
    write_only_at[index] = 5;
    say("positioned reads: %s write-only: a seek to 5 %s, lseek(0, SEEK_CUR) %s, a seek to 5 "
        "again %s",
        names[index], sought, told, again);
  }
  /* io's show refuses the reader without CAP_SYS_PTRACE at a write-only
   * descriptor's traverse too, and the refusal leaves the position at 0. */
  {
    char again[64];
    drop_effective(CAP_SYS_PTRACE);
    seek_answer(write_only[0], 3, sought, sizeof(sought));
    seek_from(write_only[0], 0, SEEK_CUR, told, sizeof(told));
    raise_effective(CAP_SYS_PTRACE);
    seek_answer(write_only[0], 3, again, sizeof(again));
    write_only_at[0] = 3;
    say("positioned reads: io write-only without CAP_SYS_PTRACE in the effective set: a seek to "
        "3 %s, then lseek(0, SEEK_CUR) %s; with it again a seek to 3 %s",
        sought, told, again);
  }
  /* Past LLONG_MAX: seq_lseek adds SEEK_CUR's offset to the position as a
   * signed 64-bit number, which wraps negative: EINVAL, the position kept,
   * and a read continues the record its seek's traverse buffered. */
  for (size_t index = 0; index < files; index++) {
    int fd = overflow[index];
    char past[64];
    char kept[64];
    char at_end[64];
    char past_end[64];
    char kept_end[64];
    char buffer[4096];
    seek_answer(fd, 4, sought, sizeof(sought));
    seek_from(fd, LLONG_MAX, SEEK_CUR, past, sizeof(past));
    seek_from(fd, 0, SEEK_CUR, kept, sizeof(kept));
    const char* continued = outcome(read(fd, buffer, sizeof(buffer)));
    seek_answer(fd, LLONG_MAX, at_end, sizeof(at_end));
    seek_from(fd, 1, SEEK_CUR, past_end, sizeof(past_end));
    seek_from(fd, 0, SEEK_CUR, kept_end, sizeof(kept_end));
    say("positioned reads: %s, past LLONG_MAX: a seek to 4 %s, lseek(LLONG_MAX, SEEK_CUR) %s, "
        "lseek(0, SEEK_CUR) %s, a read %s; a seek to LLONG_MAX %s, lseek(1, SEEK_CUR) %s, "
        "lseek(0, SEEK_CUR) %s",
        names[index], sought, past, kept, continued, at_end, past_end, kept_end);
    close(fd);
  }
  const char* statm_read = read_answer(reaped_statm, 4096);

  tell_target('q');
  int status = 0;
  if (waitpid(target, &status, 0) != target || !WIFEXITED(status) ||
      WEXITSTATUS(status) != 0) {
    die("the target failed (status %#x)", status);
  }
  for (size_t index = 0; index < files; index++) {
    int fd = fds[index];
    const char* gone = pread_answer_at(fd, 4096, 0);
    const char* gone_vectored = preadv_answer(fd);
    const char* gone_read = read_answer(fd, 4096);
    seek_to_cursor_answer(fd, cursors[index], told, sizeof(told));
    seek_answer(fd, 5, sought, sizeof(sought));
    say("positioned reads: %s once the target is reaped: a pread at 0 %s, a preadv at 0 %s, a "
        "read %s, a seek to the cursor %s, a seek to 5 %s",
        names[index], gone, gone_vectored, gone_read, told, sought);
    close(fd);
  }
  /* e1: a statm read before the reaping: a pread at 0 is refused, so the
   * sequence is at 0 and a seek to the cursor, at the record's end,
   * traverses and is refused, leaving the position at 0, where the read
   * shows and is refused. */
  const char* gone = pread_answer_at(reaped_statm, 4096, 0);
  seek_from(reaped_statm, 0, SEEK_CUR, told, sizeof(told));
  say("positioned reads: statm, e1: a read %s; once the target is reaped a pread at 0 %s, then "
      "lseek(0, SEEK_CUR) %s, then a read %s",
      statm_read, gone, told, read_answer(reaped_statm, 4096));
  close(reaped_statm);
  /* Write-only once the target is reaped: a seek to where the sequence is
   * runs no show; one elsewhere traverses, and show finds the task gone
   * (ESRCH), leaving the position and the sequence at 0. */
  for (size_t index = 0; index < files; index++) {
    char there[64];
    char elsewhere[64];
    seek_answer(write_only[index], (off_t)write_only_at[index], there, sizeof(there));
    seek_answer(write_only[index], 2, elsewhere, sizeof(elsewhere));
    seek_from(write_only[index], 0, SEEK_CUR, told, sizeof(told));
    say("positioned reads: %s write-only once the target is reaped: a seek to %zu, where the "
        "sequence is, %s, a seek to 2 %s, then lseek(0, SEEK_CUR) %s",
        names[index], write_only_at[index], there, elsewhere, told);
    close(write_only[index]);
  }
  for (size_t index = 0; index < 4; index++) {
    close(seeks[index]);
  }
  close(to_end);
  close(partial);
  close(positioned);
}

/* --sequence-across-fork */

static int to_second[2];
static int from_second[2];

/* One read the second reader makes, as the reader tells it. */
struct second_read {
  char op; /* 'r' read, 'p' pread, 'c' lseek(0, SEEK_CUR), 'd' drop
            * CAP_SYS_PTRACE, 'g' receive a descriptor over SCM_RIGHTS, 'z' a
            * pread of the whole record at 0, as whether every number in it
            * is 0, 'q' quit */
  int fd;  /* RECEIVED_COPY: the descriptor 'g' received */
  int count;
  long position;
};

/* The socket the reader sends descriptors over, and the descriptor the
 * second reader received last ('g'). */
static int descriptor_socket[2] = {-1, -1};
static int received_copy = -1;
enum { RECEIVED_COPY = -1 };

/* 'R' for a read that returned bytes, 'E' for end of file, 'A' for EACCES,
 * 'S' for ESRCH, 'N' for EOPNOTSUPP, 'O' for another errno. */
static char read_code(ssize_t got) {
  if (got > 0) {
    return 'R';
  }
  if (got == 0) {
    return 'E';
  }
  return errno == EACCES ? 'A' : errno == ESRCH ? 'S' : errno == EOPNOTSUPP ? 'N' : 'O';
}

static const char* read_code_name(char code) {
  static const char* const positions[] = {"returns 0", "returns 1", "returns 2", "returns 3",
                                          "returns 4", "returns 5", "returns 6", "returns 7",
                                          "returns 8", "returns 9"};
  if (code >= '0' && code <= '9') {
    return positions[code - '0'];
  }
  switch (code) {
    case 'R':
      return "read";
    case 'E':
      return "end of file";
    case 'A':
      return "EACCES";
    case 'S':
      return "ESRCH";
    case 'N':
      return "EOPNOTSUPP";
    case 'Z':
      return "read, every counter 0";
    case 'H':
      return "read, some counter not 0";
    default:
      return "another errno";
  }
}

/* The second reader: a process the reader forked, which holds the same open
 * files, reads as told and answers each read's outcome. */
static void second_reader_main(void) {
  close(to_second[1]);
  close(from_second[0]);
  for (;;) {
    struct second_read request;
    if (read(to_second[0], &request, sizeof(request)) != sizeof(request)) {
      _exit(3);
    }
    char buffer[4096];
    char code = 'k';
    int count = request.count < (int)sizeof(buffer) ? request.count : (int)sizeof(buffer);
    int fd = request.fd == RECEIVED_COPY ? received_copy : request.fd;
    switch (request.op) {
      case 'q':
        _exit(0);
      case 'r':
        code = read_code(read(fd, buffer, (size_t)count));
        break;
      case 'p':
        code = read_code(pread(fd, buffer, (size_t)count, (off_t)request.position));
        break;
      case 'c': {
        /* The position, as one digit, or the errno's code. */
        off_t position = lseek(fd, 0, SEEK_CUR);
        code = position >= 0 && position <= 9 ? (char)('0' + position)
               : position < 0                 ? read_code(-1)
                                              : 'O';
        break;
      }
      case 'd':
        drop_effective(CAP_SYS_PTRACE);
        break;
      case 'g':
        received_copy = receive_descriptor(descriptor_socket[1]);
        break;
      case 'z': {
        /* 'Z' where every number the record holds is 0 (Hermit's statm),
         * 'H' where one is not (the kernel's own), else the read's code. */
        ssize_t got = pread(fd, buffer, sizeof(buffer), 0);
        code = read_code(got);
        if (got > 0) {
          code = 'Z';
          for (ssize_t i = 0; i < got; i++) {
            if (buffer[i] >= '1' && buffer[i] <= '9') {
              code = 'H';
            }
          }
        }
        break;
      }
      default:
        _exit(4);
    }
    if (write(from_second[1], &code, 1) != 1) {
      _exit(5);
    }
  }
}

/* Has the second reader do `op` on `fd`, and returns its answer's name. */
static const char* second(char op, int fd, int count, long position) {
  struct second_read request = {op, fd, count, position};
  char code;
  if (write(to_second[1], &request, sizeof(request)) != sizeof(request)) {
    die("write to the second reader: %s", strerror(errno));
  }
  if (op == 'q') {
    return "";
  }
  if (read(from_second[0], &code, 1) != 1) {
    die("read from the second reader: %s", strerror(errno));
  }
  return read_code_name(code);
}

static const char* read_name(int fd, size_t count) {
  char buffer[4096];
  return read_code_name(read_code(read(fd, buffer, count < sizeof(buffer) ? count : sizeof(buffer))));
}

static void sequence_across_fork(int through_dotdot) {
  pid_t target = fork_target();
  char path[96];
  process_file_path(path, sizeof(path), target, "io", through_dotdot);
  int before = open(path, O_RDONLY | O_CLOEXEC);
  int unread = open(path, O_RDONLY | O_CLOEXEC);
  int moved = open(path, O_RDONLY | O_CLOEXEC);
  if (before < 0 || unread < 0 || moved < 0) {
    die("open %s: %s", path, strerror(errno));
  }
  const char* before_first = read_name(before, 4);
  const char* moved_first = read_name(moved, 4);
  if (pipe(to_second) != 0 || pipe(from_second) != 0) {
    die("pipe: %s", strerror(errno));
  }
  pid_t second_reader = fork();
  if (second_reader < 0) {
    die("fork: %s", strerror(errno));
  }
  if (second_reader == 0) {
    second_reader_main();
  }
  close(to_second[0]);
  close(from_second[1]);

  /* Read before the fork: the second reader continues the record, and so
   * does the reader, without the capability, to the record's end. */
  const char* before_second = second('r', before, 2, 0);
  /* Read before the fork: the second reader's pread moves the sequence. */
  const char* moved_second = second('p', moved, 2, 1);
  drop_effective(CAP_SYS_PTRACE);
  const char* before_rest = read_name(before, 4096);
  const char* before_end = read_name(before, 4096);
  const char* moved_after = read_name(moved, 4096);
  raise_effective(CAP_SYS_PTRACE);
  say("sequence across fork: io read 4 bytes before a fork: the reader %s; the second reader, 2 "
      "bytes: %s; the reader, without CAP_SYS_PTRACE in its effective set: %s, then %s",
      before_first, before_second, before_rest, before_end);
  say("sequence across fork: io read 4 bytes before a fork: the reader %s; the second reader's "
      "pread of 2 bytes at 1: %s; the reader's read, without CAP_SYS_PTRACE in its effective "
      "set: %s",
      moved_first, moved_second, moved_after);

  /* Shared before its first read: each continues the other's record. */
  const char* unread_first = read_name(unread, 4);
  const char* unread_second = second('r', unread, 2, 0);
  drop_effective(CAP_SYS_PTRACE);
  second('d', -1, 0, 0);
  const char* unread_third = read_name(unread, 3);
  const char* unread_fourth = second('r', unread, 4096, 0);
  const char* unread_end = read_name(unread, 4096);
  raise_effective(CAP_SYS_PTRACE);
  say("sequence across fork: io a fork shares before its first read: the reader, 4 bytes: %s; "
      "the second reader, 2 bytes: %s; without CAP_SYS_PTRACE in either's effective set, the "
      "reader, 3 bytes: %s; the second reader: %s; the reader: %s",
      unread_first, unread_second, unread_third, unread_fourth, unread_end);

  second('q', -1, 0, 0);
  int status = 0;
  if (waitpid(second_reader, &status, 0) != second_reader || !WIFEXITED(status) ||
      WEXITSTATUS(status) != 0) {
    die("the second reader failed (status %#x)", status);
  }
  close(before);
  close(unread);
  close(moved);
  tell_target('q');
  if (waitpid(target, &status, 0) != target || !WIFEXITED(status) ||
      WEXITSTATUS(status) != 0) {
    die("the target failed (status %#x)", status);
  }
}

/* --sequence-received-alias, --sequence-received-no-alias */

/* Opens another process's io (the target's) as /proc/<pid>/io or, with
 * `through_dotdot`, as /proc/<pid>/../<pid>/io. */
static int open_target_io(pid_t target, int through_dotdot) {
  char path[96];
  process_file_path(path, sizeof(path), target, "io", through_dotdot);
  int fd = open(path, O_RDONLY | O_CLOEXEC);
  if (fd < 0) {
    die("open %s: %s", path, strerror(errno));
  }
  return fd;
}

/* Forks the second reader (second_reader_main), with the descriptor socket
 * made first. */
static pid_t fork_second_reader(void) {
  if (pipe(to_second) != 0 || pipe(from_second) != 0) {
    die("pipe: %s", strerror(errno));
  }
  pid_t second_reader = fork();
  if (second_reader < 0) {
    die("fork: %s", strerror(errno));
  }
  if (second_reader == 0) {
    second_reader_main();
  }
  close(to_second[0]);
  close(from_second[1]);
  return second_reader;
}

static void finish_second_reader(pid_t second_reader, pid_t target) {
  second('q', -1, 0, 0);
  int status = 0;
  if (waitpid(second_reader, &status, 0) != second_reader || !WIFEXITED(status) ||
      WEXITSTATUS(status) != 0) {
    die("the second reader failed (status %#x)", status);
  }
  tell_target('q');
  if (waitpid(target, &status, 0) != target || !WIFEXITED(status) ||
      WEXITSTATUS(status) != 0) {
    die("the target failed (status %#x)", status);
  }
}

/* Another process's io sent over SCM_RIGHTS to a process that holds the
 * same open file, inherited at a fork after the reader read 4 bytes: Linux
 * installs the one struct file, so a read through the received copy
 * continues the record and moves the position the descriptor the second
 * reader inherited shares, to 6, and a read through that descriptor,
 * without CAP_SYS_PTRACE in its effective set, continues where the copy's
 * read left the record (a read that ran show would be refused with
 * EACCES), then reads end of file, as does the reader's own, without it. */
static void sequence_received_alias(int through_dotdot) {
  pid_t target = fork_target();
  if (socketpair(AF_UNIX, SOCK_STREAM, 0, descriptor_socket) != 0) {
    die("socketpair: %s", strerror(errno));
  }
  int io = open_target_io(target, through_dotdot);
  const char* first = read_name(io, 4);
  pid_t second_reader = fork_second_reader();
  send_descriptor(descriptor_socket[0], io);
  second('g', -1, 0, 0);
  const char* copied = second('r', RECEIVED_COPY, 2, 0);
  const char* position = second('c', io, 0, 0);
  second('d', -1, 0, 0);
  const char* own = second('r', io, 4096, 0);
  const char* own_end = second('r', io, 4096, 0);
  drop_effective(CAP_SYS_PTRACE);
  const char* reader_end = read_name(io, 4096);
  raise_effective(CAP_SYS_PTRACE);
  say("sequence received: io read 4 bytes before a fork, then sent to the second reader, which "
      "holds it: the reader %s; the second reader, 2 bytes through the received copy: %s; "
      "lseek(0, SEEK_CUR) through its own: %s; without CAP_SYS_PTRACE in its effective set, "
      "through its own: %s, then %s; the reader, without it: %s",
      first, copied, position, own, own_end, reader_end);
  close(io);
  finish_second_reader(second_reader, target);
}

/* Another process's io sent over SCM_RIGHTS to a process forked before it
 * was opened, which holds no other descriptor of it: Linux installs the
 * one struct file. Kept by the reader after 4 bytes, the second reader's
 * pread of 2 bytes at 0 through the copy moves the record's sequence, so
 * the reader's next read, without CAP_SYS_PTRACE, traverses and is refused
 * with EACCES (Codex's narrow check of the kvm lane's slice N3, round 5);
 * closed by the reader as soon as it is sent, still queued, the open file
 * lives on in the message: the second reader's pread at 0 through it reads,
 * and its read at the position, 4, without CAP_SYS_PTRACE, traverses and
 * is refused. */
static void sequence_received_no_alias(int through_dotdot) {
  pid_t target = fork_target();
  if (socketpair(AF_UNIX, SOCK_STREAM, 0, descriptor_socket) != 0) {
    die("socketpair: %s", strerror(errno));
  }
  pid_t second_reader = fork_second_reader();
  int kept = open_target_io(target, through_dotdot);
  const char* kept_first = read_name(kept, 4);
  send_descriptor(descriptor_socket[0], kept);
  second('g', -1, 0, 0);
  const char* kept_copy = second('p', RECEIVED_COPY, 2, 0);
  drop_effective(CAP_SYS_PTRACE);
  const char* kept_after = read_name(kept, 4096);
  raise_effective(CAP_SYS_PTRACE);
  say("sequence received: io read 4 bytes, then sent to a second reader forked before its open: "
      "the reader %s; the second reader's pread of 2 bytes at 0 through the received copy: %s; "
      "the reader's read, without CAP_SYS_PTRACE in its effective set: %s",
      kept_first, kept_copy, kept_after);
  close(kept);
  int closed = open_target_io(target, through_dotdot);
  const char* closed_first = read_name(closed, 4);
  send_descriptor(descriptor_socket[0], closed);
  close(closed);
  second('g', -1, 0, 0);
  const char* closed_copy = second('p', RECEIVED_COPY, 2, 0);
  second('d', -1, 0, 0);
  const char* closed_after = second('r', RECEIVED_COPY, 4096, 0);
  say("sequence received: io read 4 bytes, then sent and closed while queued: the reader %s; the "
      "second reader's pread of 2 bytes at 0 through the received copy: %s; its read at the "
      "position, without CAP_SYS_PTRACE in its effective set: %s",
      closed_first, closed_copy, closed_after);
  finish_second_reader(second_reader, target);
}

/* --sequence-received-dotdot-pread */

/* Another process's statm opened as /proc/<pid>/../<pid>/statm, a spelling
 * the kernel opens under Hermit, read 4 bytes, then sent over SCM_RIGHTS to
 * a second reader forked before the open: Linux installs the one struct
 * file, so the second reader's read through the copy continues the record,
 * and its pread at 0 runs show again and reads the whole record, the
 * kernel's own natively, whose counters are not all 0, and Hermit's, all 0,
 * where Detcore follows the sequence. Where it cannot (on SaBRe, which
 * answers EOPNOTSUPP rather than end the run), no read through the copy may
 * reach the kernel's file: Claude's final check of the kvm lane's slice N3
 * saw SaBRe's pread return the host's own statm, and its read end of file. */
static void sequence_received_dotdot_pread(void) {
  pid_t target = fork_target();
  if (socketpair(AF_UNIX, SOCK_STREAM, 0, descriptor_socket) != 0) {
    die("socketpair: %s", strerror(errno));
  }
  pid_t second_reader = fork_second_reader();
  char path[96];
  process_file_path(path, sizeof(path), target, "statm", 1);
  int statm = open(path, O_RDONLY | O_CLOEXEC);
  if (statm < 0) {
    die("open %s: %s", path, strerror(errno));
  }
  const char* first = read_name(statm, 4);
  send_descriptor(descriptor_socket[0], statm);
  second('g', -1, 0, 0);
  const char* rest = second('r', RECEIVED_COPY, 4096, 0);
  const char* again = second('z', RECEIVED_COPY, 4096, 0);
  say("sequence received through ..: statm read 4 bytes, then sent to a second reader forked "
      "before its open: the reader %s; the second reader's read through the received copy: %s; "
      "its pread at 0: %s",
      first, rest, again);
  close(statm);
  finish_second_reader(second_reader, target);
}

/* --sequence-across-exec */

/* Another process's statm, read 4 bytes before a fork whose child execs
 * (this program again, --read-inherited) and reads 2 bytes through the
 * descriptor it kept: Linux keeps the one struct file across exec, so once
 * the target is reaped the reader's read continues the record from byte 6,
 * with no show (one would find the task gone, ESRCH), then reads end of
 * file (Claude's narrow check of the kvm lane's slice N3, round 5, case 11:
 * on DBT and SaBRe the reader's read was ESRCH, and through .. the child's
 * own read was end of file). */
static void sequence_across_exec(const char* self, int through_dotdot) {
  pid_t target = fork_target();
  char path[96];
  process_file_path(path, sizeof(path), target, "statm", through_dotdot);
  int statm = open(path, O_RDONLY);
  if (statm < 0) {
    die("open %s: %s", path, strerror(errno));
  }
  const char* first = read_name(statm, 4);
  fflush(stdout);
  pid_t child = fork();
  if (child < 0) {
    die("fork: %s", strerror(errno));
  }
  if (child == 0) {
    char number[16];
    snprintf(number, sizeof(number), "%d", statm);
    execl(self, self, "--read-inherited", number, (char*)NULL);
    die("exec %s: %s", self, strerror(errno));
  }
  int status = 0;
  if (waitpid(child, &status, 0) != child || !WIFEXITED(status) || WEXITSTATUS(status) != 0) {
    die("the exec'd child failed (status %#x)", status);
  }
  tell_target('q');
  if (waitpid(target, &status, 0) != target || !WIFEXITED(status) ||
      WEXITSTATUS(status) != 0) {
    die("the target failed (status %#x)", status);
  }
  const char* rest = read_name(statm, 4096);
  const char* end = read_name(statm, 4096);
  say("sequence across exec: statm read 4 bytes before a fork: the reader %s; once the exec'd "
      "child read and the target is reaped, the reader: %s, then %s",
      first, rest, end);
  close(statm);
}

/* --read-inherited <fd>: the exec'd child of --sequence-across-exec. */
static void read_inherited(const char* number) {
  say("sequence across exec: the exec'd child, 2 bytes through the descriptor it kept: %s",
      read_name(atoi(number), 2));
}

/* --io-spellings */

/* "zeros" when every counter the file shows is 0, else the first counter
 * that is not, or the errno of the open or the read. */
static void spelling(const char* label, int dirfd, const char* name) {
  int fd = openat(dirfd, name, O_RDONLY | O_CLOEXEC);
  if (fd < 0) {
    say("io spellings: %s: open %s", label, errno_name(errno));
    return;
  }
  char buffer[4096];
  ssize_t got = read(fd, buffer, sizeof(buffer) - 1);
  int read_errno = errno;
  close(fd);
  if (got < 0) {
    say("io spellings: %s: read %s", label, errno_name(read_errno));
    return;
  }
  buffer[got] = '\0';
  int counters = 0;
  for (char* line = strtok(buffer, "\n"); line != NULL; line = strtok(NULL, "\n")) {
    char* colon = strchr(line, ':');
    if (colon == NULL) {
      continue;
    }
    counters++;
    if (strtoull(colon + 1, NULL, 10) != 0) {
      *colon = '\0';
      say("io spellings: %s: %s is not 0", label, line);
      return;
    }
  }
  say("io spellings: %s: %s", label, counters == 7 ? "zeros" : "not seven counters");
}

static void* second_thread(void* main_tid) {
  char path[96];
  snprintf(path, sizeof(path), "/proc/%d/task/%d/io", (int)getpid(), *(int*)main_tid);
  spelling("a second thread's /proc/<pid>/task/<main tid>/io", AT_FDCWD, path);
  spelling("a second thread's /proc/thread-self/io", AT_FDCWD, "/proc/thread-self/io");
  return NULL;
}

static void io_spellings(void) {
  int tid = (int)syscall(SYS_gettid);
  char path[96];
  spelling("/proc/self/io", AT_FDCWD, "/proc/self/io");
  spelling("/proc/thread-self/io", AT_FDCWD, "/proc/thread-self/io");
  snprintf(path, sizeof(path), "/proc/self/task/%d/io", tid);
  spelling("/proc/self/task/<tid>/io", AT_FDCWD, path);
  snprintf(path, sizeof(path), "/proc/%d/task/%d/io", (int)getpid(), tid);
  spelling("/proc/<pid>/task/<tid>/io", AT_FDCWD, path);
  snprintf(path, sizeof(path), "/proc/self/task/%d", tid);
  int task = open(path, O_RDONLY | O_DIRECTORY | O_CLOEXEC);
  if (task < 0) {
    say("io spellings: a task directory: open %s", errno_name(errno));
  } else {
    spelling("io relative to a task directory", task, "io");
    close(task);
  }
  task = open(path, O_PATH | O_DIRECTORY | O_CLOEXEC);
  if (task < 0) {
    say("io spellings: an O_PATH task directory: open %s", errno_name(errno));
  } else {
    spelling("io relative to an O_PATH task directory", task, "io");
    close(task);
  }
  int proc = open("/proc", O_RDONLY | O_DIRECTORY | O_CLOEXEC);
  if (proc < 0) {
    say("io spellings: /proc: open %s", errno_name(errno));
  } else {
    spelling("thread-self/io relative to /proc", proc, "thread-self/io");
    close(proc);
  }
}

static void io_spellings_thread(void) {
  int tid = (int)syscall(SYS_gettid);
  pthread_t thread;
  if (pthread_create(&thread, NULL, second_thread, &tid) != 0) {
    die("pthread_create");
  }
  pthread_join(thread, NULL);
}

/* --diskstats-received */

static void send_descriptor(int socket, int fd) {
  char byte = 'x';
  struct iovec io = {&byte, 1};
  char control[CMSG_SPACE(sizeof(int))];
  memset(control, 0, sizeof(control));
  struct msghdr message;
  memset(&message, 0, sizeof(message));
  message.msg_iov = &io;
  message.msg_iovlen = 1;
  message.msg_control = control;
  message.msg_controllen = sizeof(control);
  struct cmsghdr* header = CMSG_FIRSTHDR(&message);
  header->cmsg_level = SOL_SOCKET;
  header->cmsg_type = SCM_RIGHTS;
  header->cmsg_len = CMSG_LEN(sizeof(int));
  memcpy(CMSG_DATA(header), &fd, sizeof(fd));
  if (sendmsg(socket, &message, 0) != 1) {
    die("sendmsg: %s", strerror(errno));
  }
}

static int receive_descriptor(int socket) {
  char byte;
  struct iovec io = {&byte, 1};
  char control[CMSG_SPACE(sizeof(int))];
  memset(control, 0, sizeof(control));
  struct msghdr message;
  memset(&message, 0, sizeof(message));
  message.msg_iov = &io;
  message.msg_iovlen = 1;
  message.msg_control = control;
  message.msg_controllen = sizeof(control);
  if (recvmsg(socket, &message, 0) != 1) {
    die("recvmsg: %s", strerror(errno));
  }
  struct cmsghdr* header = CMSG_FIRSTHDR(&message);
  if (header == NULL || header->cmsg_level != SOL_SOCKET || header->cmsg_type != SCM_RIGHTS ||
      header->cmsg_len < CMSG_LEN(sizeof(int))) {
    die("recvmsg carried no descriptor (flags %#x)", message.msg_flags);
  }
  int fd;
  memcpy(&fd, CMSG_DATA(header), sizeof(fd));
  return fd;
}

/* The receiver: forked before the file is opened, it waits for the open,
 * fills its descriptor table but one slot, receives the descriptor into that
 * slot, and reads. */
static void receiver_main(int socket, int go, int ready) {
  char start;
  if (read(go, &start, 1) != 1) {
    die("read go: %s", strerror(errno));
  }
  struct rlimit limit = {64, 64};
  if (setrlimit(RLIMIT_NOFILE, &limit) != 0) {
    die("setrlimit: %s", strerror(errno));
  }
  enum { most = 8192 };
  static int fillers[most];
  int filled = 0;
  int fill_errno = 0;
  while (filled < most) {
    int fd = open("/dev/null", O_RDONLY | O_CLOEXEC);
    if (fd < 0) {
      fill_errno = errno;
      break;
    }
    fillers[filled++] = fd;
  }
  if (fill_errno != EMFILE || filled == 0) {
    die("the descriptor table did not fill: %d opened, then %s", filled,
        fill_errno ? strerror(fill_errno) : "no error");
  }
  close(fillers[--filled]);
  if (write(ready, "r", 1) != 1) {
    die("write ready: %s", strerror(errno));
  }
  int received = receive_descriptor(socket);
  int probe = open("/dev/null", O_RDONLY | O_CLOEXEC);
  int full = probe < 0 && errno == EMFILE;
  if (probe >= 0) {
    close(probe);
  }
  char bytes[256];
  char again[256];
  ssize_t got = read(received, bytes, sizeof(bytes));
  int read_errno = got < 0 ? errno : 0;
  ssize_t pgot = pread(received, again, sizeof(again), 0);
  int pread_errno = pgot < 0 ? errno : 0;
  say("diskstats received at the descriptor limit: the table is full: %s", full ? "yes" : "no");
  say("diskstats received at the descriptor limit: read %s, pread at 0 %s",
      got > 0 ? "a nonempty table" : got == 0 ? "end of file" : errno_name(read_errno),
      pgot > 0    ? (pgot == got && memcmp(bytes, again, (size_t)got) == 0 ? "the same bytes"
                                                                          : "other bytes")
      : pgot == 0 ? "end of file"
                  : errno_name(pread_errno));
  close(received);
  while (filled > 0) {
    close(fillers[--filled]);
  }
  _exit(0);
}

/* The sender opens the file after the receiver was forked and before it
 * fills its table, and keeps the descriptor open until the receiver has
 * read, so the receiver's table stays full however the backend counts
 * descriptors. */
static void diskstats_received(void) {
  int sockets[2];
  int go[2];
  int ready[2];
  if (socketpair(AF_UNIX, SOCK_DGRAM | SOCK_CLOEXEC, 0, sockets) != 0 || pipe(go) != 0 ||
      pipe(ready) != 0) {
    die("socketpair or pipe: %s", strerror(errno));
  }
  pid_t receiver = fork();
  if (receiver < 0) {
    die("fork: %s", strerror(errno));
  }
  if (receiver == 0) {
    close(sockets[0]);
    close(go[1]);
    close(ready[0]);
    receiver_main(sockets[1], go[0], ready[1]);
  }
  close(sockets[1]);
  close(go[0]);
  close(ready[1]);
  int fd = open("/proc/diskstats", O_RDONLY | O_CLOEXEC);
  if (fd < 0) {
    die("open /proc/diskstats: %s", strerror(errno));
  }
  if (write(go[1], "g", 1) != 1) {
    die("write go: %s", strerror(errno));
  }
  char byte;
  if (read(ready[0], &byte, 1) != 1) {
    die("the receiver did not get ready");
  }
  send_descriptor(sockets[0], fd);
  int status = 0;
  if (waitpid(receiver, &status, 0) != receiver || !WIFEXITED(status) ||
      WEXITSTATUS(status) != 0) {
    die("the receiver failed (status %#x)", status);
  }
  close(fd);
}

/* --stand-in-memfds */

static const char* const stand_in_names[] = {"hermit-procfs:/proc/diskstats@",
                                             "hermit-procfs:/proc/interrupts@"};
enum { stand_in_count = sizeof(stand_in_names) / sizeof(stand_in_names[0]) };

/* "end of file", "a nonempty file", or the errno of the read. */
static const char* contents_answer(int fd) {
  char buffer[4096];
  ssize_t got = read(fd, buffer, sizeof(buffer));
  return got > 0 ? "a nonempty file" : got == 0 ? "end of file" : errno_name(errno);
}

/* The contents an open of `path` reads, or the open's errno. */
static const char* contents_through(const char* path) {
  int fd = open(path, O_RDONLY | O_CLOEXEC);
  if (fd < 0) {
    return errno_name(errno);
  }
  const char* answer = contents_answer(fd);
  close(fd);
  return answer;
}

/* A memfd named `name`, with the four seals Detcore's stand-ins carry. */
static int sealed_memfd(const char* name) {
  int fd = memfd_create(name, MFD_CLOEXEC | MFD_ALLOW_SEALING);
  if (fd < 0) {
    die("memfd_create %s: %s", name, strerror(errno));
  }
  if (fcntl(fd, F_ADD_SEALS, F_SEAL_SEAL | F_SEAL_SHRINK | F_SEAL_GROW | F_SEAL_WRITE) != 0) {
    die("a sealed memfd named %s: %s", name, strerror(errno));
  }
  return fd;
}

/* Received over SCM_RIGHTS by a process forked before the memfds existed. */
static void stand_ins_received(void) {
  int sockets[2];
  if (socketpair(AF_UNIX, SOCK_DGRAM | SOCK_CLOEXEC, 0, sockets) != 0) {
    die("socketpair: %s", strerror(errno));
  }
  int go[2];
  if (pipe(go) != 0) {
    die("pipe: %s", strerror(errno));
  }
  pid_t receiver = fork();
  if (receiver < 0) {
    die("fork: %s", strerror(errno));
  }
  if (receiver == 0) {
    close(sockets[0]);
    close(go[1]);
    char start;
    if (read(go[0], &start, 1) != 1) {
      _exit(3);
    }
    for (int index = 0; index < stand_in_count; index++) {
      int fd = receive_descriptor(sockets[1]);
      say("stand-in memfd %s, received by a process forked before it: read %s",
          stand_in_names[index], contents_answer(fd));
      close(fd);
    }
    _exit(0);
  }
  close(sockets[1]);
  close(go[0]);
  int fds[stand_in_count];
  for (int index = 0; index < stand_in_count; index++) {
    fds[index] = sealed_memfd(stand_in_names[index]);
  }
  /* The receiver waits on a pipe, which this process's exit would end, so a
   * failure above cannot leave it waiting. */
  if (write(go[1], "g", 1) != 1) {
    die("write go: %s", strerror(errno));
  }
  for (int index = 0; index < stand_in_count; index++) {
    send_descriptor(sockets[0], fds[index]);
    close(fds[index]);
  }
  int status = 0;
  if (waitpid(receiver, &status, 0) != receiver || !WIFEXITED(status) ||
      WEXITSTATUS(status) != 0) {
    die("the receiver failed (status %#x)", status);
  }
  close(sockets[0]);
  close(go[1]);
}

/* Reached through descriptor links, by this process and by one forked after
 * the memfd existed. */
static void stand_in_links(const char* name) {
  int fd = sealed_memfd(name);
  char path[64];
  snprintf(path, sizeof(path), "/proc/self/fd/%d", fd);
  say("stand-in memfd %s: opened through /proc/self/fd/N: read %s", name, contents_through(path));
  snprintf(path, sizeof(path), "/proc/thread-self/fd/%d", fd);
  say("stand-in memfd %s: opened through /proc/thread-self/fd/N: read %s", name,
      contents_through(path));
  snprintf(path, sizeof(path), "/proc/%d/fd/%d", (int)getpid(), fd);
  say("stand-in memfd %s: opened through /proc/<pid>/fd/N: read %s", name,
      contents_through(path));
  snprintf(path, sizeof(path), "/dev/fd/%d", fd);
  say("stand-in memfd %s: opened through /dev/fd/N: read %s", name, contents_through(path));
  char link[512];
  snprintf(path, sizeof(path), "/proc/self/fd/%d", fd);
  ssize_t length = readlink(path, link, sizeof(link) - 1);
  if (length < 0) {
    say("stand-in memfd %s: its link: %s", name, errno_name(errno));
  } else {
    link[length] = 0;
    say("stand-in memfd %s: its link reads %s", name, link);
  }
  struct stat status;
  if (fstat(fd, &status) != 0) {
    say("stand-in memfd %s: fstat: %s", name, errno_name(errno));
  } else {
    say("stand-in memfd %s: fstat: a %s of %lld bytes with %lu links, mode %o", name,
        S_ISREG(status.st_mode) ? "regular file" : "file of another type",
        (long long)status.st_size, (unsigned long)status.st_nlink,
        (unsigned)(status.st_mode & 07777));
  }
  pid_t parent = getpid();
  pid_t child = fork();
  if (child < 0) {
    die("fork: %s", strerror(errno));
  }
  if (child == 0) {
    snprintf(path, sizeof(path), "/proc/%d/fd/%d", (int)parent, fd);
    say("stand-in memfd %s: opened by a process forked after it, through /proc/<pid>/fd/N: "
        "read %s",
        name, contents_through(path));
    _exit(0);
  }
  int child_status = 0;
  if (waitpid(child, &child_status, 0) != child || !WIFEXITED(child_status) ||
      WEXITSTATUS(child_status) != 0) {
    die("the child failed (status %#x)", child_status);
  }
  close(fd);
}

/* An unsealed memfd so named, reached through an O_PATH descriptor, which
 * shows no seals, and reopened through that descriptor's link. */
static void unsealed_stand_in(const char* name) {
  int fd = memfd_create(name, MFD_CLOEXEC);
  if (fd < 0) {
    die("memfd_create %s: %s", name, strerror(errno));
  }
  char path[64];
  snprintf(path, sizeof(path), "/proc/self/fd/%d", fd);
  int opath = open(path, O_PATH | O_CLOEXEC);
  if (opath < 0) {
    say("unsealed memfd %s: an O_PATH open of its link: %s", name, errno_name(errno));
  } else {
    snprintf(path, sizeof(path), "/proc/self/fd/%d", opath);
    say("unsealed memfd %s: reopened through an O_PATH descriptor's link: read %s", name,
        contents_through(path));
    close(opath);
  }
  close(fd);
}

static void stand_in_memfds(void) {
  stand_ins_received();
  for (int index = 0; index < stand_in_count; index++) {
    stand_in_links(stand_in_names[index]);
  }
  for (int index = 0; index < stand_in_count; index++) {
    unsealed_stand_in(stand_in_names[index]);
  }
}

/* --read-io-and-arch-status */

static void read_io_and_arch_status(void) {
  const char* paths[] = {"/proc/thread-self/io", "/proc/self/arch_status"};
  for (size_t index = 0; index < sizeof(paths) / sizeof(paths[0]); index++) {
    int fd = open(paths[index], O_RDONLY | O_CLOEXEC);
    if (fd < 0) {
      say("%s: open %s", paths[index], errno_name(errno));
      continue;
    }
    char buffer[4096];
    ssize_t got = read(fd, buffer, sizeof(buffer));
    int read_errno = errno;
    close(fd);
    if (got < 0) {
      say("%s: read %s", paths[index], errno_name(read_errno));
    } else {
      say("%s: read %zd bytes", paths[index], got);
    }
  }
}

int main(int argc, char** argv) {
  if (argc == 3 && strcmp(argv[1], "--read-inherited") == 0) {
    read_inherited(argv[2]);
    return 0;
  }
  if (argc != 2) {
    die("usage: rendered_io_and_diskstats --io-access|--io-access-full-table|"
        "--io-access-after-chroot|--io-access-after-chroot-effective-only|"
        "--io-access-after-chroot-own-userns|--positioned-reads|"
        "--positioned-reads-through-dotdot|--sequence-across-fork|"
        "--sequence-across-fork-through-dotdot|--sequence-received-alias|"
        "--sequence-received-alias-through-dotdot|--sequence-received-no-alias|"
        "--sequence-received-no-alias-through-dotdot|--sequence-received-dotdot-pread|"
        "--sequence-across-exec|"
        "--sequence-across-exec-through-dotdot|--io-spellings|"
        "--io-spellings-thread|--diskstats-received|--read-io-and-arch-status|"
        "--stand-in-memfds");
  }
  if (strcmp(argv[1], "--io-access") == 0) {
    io_access();
  } else if (strcmp(argv[1], "--io-access-full-table") == 0) {
    io_access_full_table();
  } else if (strcmp(argv[1], "--io-access-after-chroot") == 0) {
    io_access_after_chroot();
  } else if (strcmp(argv[1], "--io-access-after-chroot-effective-only") == 0) {
    io_access_after_chroot_effective_only();
  } else if (strcmp(argv[1], "--io-access-after-chroot-own-userns") == 0) {
    io_access_after_chroot_own_userns();
  } else if (strcmp(argv[1], "--positioned-reads") == 0) {
    positioned_reads(0);
  } else if (strcmp(argv[1], "--positioned-reads-through-dotdot") == 0) {
    positioned_reads(1);
  } else if (strcmp(argv[1], "--sequence-across-fork") == 0) {
    sequence_across_fork(0);
  } else if (strcmp(argv[1], "--sequence-across-fork-through-dotdot") == 0) {
    sequence_across_fork(1);
  } else if (strcmp(argv[1], "--sequence-received-alias") == 0) {
    sequence_received_alias(0);
  } else if (strcmp(argv[1], "--sequence-received-alias-through-dotdot") == 0) {
    sequence_received_alias(1);
  } else if (strcmp(argv[1], "--sequence-received-no-alias") == 0) {
    sequence_received_no_alias(0);
  } else if (strcmp(argv[1], "--sequence-received-no-alias-through-dotdot") == 0) {
    sequence_received_no_alias(1);
  } else if (strcmp(argv[1], "--sequence-received-dotdot-pread") == 0) {
    sequence_received_dotdot_pread();
  } else if (strcmp(argv[1], "--sequence-across-exec") == 0) {
    sequence_across_exec(argv[0], 0);
  } else if (strcmp(argv[1], "--sequence-across-exec-through-dotdot") == 0) {
    sequence_across_exec(argv[0], 1);
  } else if (strcmp(argv[1], "--stand-in-memfds") == 0) {
    stand_in_memfds();
  } else if (strcmp(argv[1], "--io-spellings") == 0) {
    io_spellings();
  } else if (strcmp(argv[1], "--io-spellings-thread") == 0) {
    io_spellings_thread();
  } else if (strcmp(argv[1], "--diskstats-received") == 0) {
    diskstats_received();
  } else if (strcmp(argv[1], "--read-io-and-arch-status") == 0) {
    read_io_and_arch_status();
  } else {
    die("unknown mode %s", argv[1]);
  }
  return 0;
}
