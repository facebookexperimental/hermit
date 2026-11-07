/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Another process's /proc/<pid>/cmdline and /proc/<pid>/environ, as Linux
 * shows them (fs/proc/base.c get_mm_cmdline and environ_read): the argument
 * and environment strings the process's image was started with. No process
 * here rewrites its arguments or environment in memory after exec, so every
 * file shows exactly what execve passed.
 *
 * The parent starts each child by execve of this program with an argument
 * vector and environment it chooses (the child's role comes from the
 * environment, since one child has no arguments at all), waits until the
 * child reports it is running, and reads the child's files:
 *   - a child with several arguments, an empty one among them, and an
 *     environment with an empty value: both files whole, then cmdline three
 *     bytes at a time and from an offset (lseek, then pread);
 *   - a child started with an empty argument vector, which Linux gives an
 *     empty argv[0] (fs/exec.c, since 5.18);
 *   - a child whose one argument is longer than a page: its length, the
 *     bytes either side of the page boundary (pread), and a checksum;
 *   - a forked child that did not exec, whose files are the parent's own;
 *   - a child that has exited and not been reaped (a zombie), whose files
 *     open and read empty;
 *   - a child that made itself non-dumpable (PR_SET_DUMPABLE 0), whose
 *     environ a reader without effective capabilities cannot open (EACCES:
 *     environ_open checks PTRACE_MODE_READ_FSCREDS) while its cmdline,
 *     which Linux checks for nothing, still reads;
 *   - a child that execs a second image while the parent holds its environ
 *     and cmdline open: the kept environ reads nothing after the exec
 *     (environ_open pinned the first image's memory map, which no task uses
 *     any more: environ_read's mmget_not_zero fails), the kept cmdline reads
 *     the second image's arguments (get_mm_cmdline takes the task's map at
 *     each read), and a fresh open of environ reads the second image's;
 *   - a child that opens its own environ and cmdline and execs a second
 *     image that reads the kept descriptors: the same answers, the caller's
 *     own files included.
 * No pid and no path is printed, so one transcript describes every run.
 * Standard output is the oracle; the program exits 0 when every step ran.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <linux/capability.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/auxv.h>
#include <sys/prctl.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#define LONG_ARGUMENT 5000
/* The child's ends of its two pipes, at fixed numbers, so that the
 * environment its parent reads does not depend on which descriptors the
 * program inherited. */
#define READY_FD 100
#define HOLD_FD 101
/* A child's own environ and cmdline, kept open across its exec. */
#define OWN_ENVIRON_FD 102
#define OWN_CMDLINE_FD 103

static int failures;

static void fail(const char* what) {
  printf("FAIL %s errno=%d\n", what, errno);
  failures++;
}

static const char* errno_name(int error) {
  switch (error) {
    case 0:
      return "ok";
    case EACCES:
      return "EACCES";
    case ENOENT:
      return "ENOENT";
    case ESRCH:
      return "ESRCH";
    case EPERM:
      return "EPERM";
    default:
      return "other";
  }
}

/* The bytes, printable ones as they are, NUL as \0 and any other as \xHH. */
static void print_bytes(const char* label, const char* bytes, ssize_t length) {
  printf("%s: length=%zd bytes=", label, length);
  for (ssize_t i = 0; i < length; i++) {
    unsigned char byte = (unsigned char)bytes[i];
    if (byte == 0) {
      printf("\\0");
    } else if (byte >= 0x20 && byte < 0x7f && byte != '\\') {
      putchar(byte);
    } else {
      printf("\\x%02x", byte);
    }
  }
  putchar('\n');
}

/* The whole of /proc/<pid>/<name>, read `chunk` bytes at a time. */
static ssize_t read_file(pid_t pid, const char* name, char* buffer, size_t size, size_t chunk) {
  char path[64];
  if (pid == 0) {
    snprintf(path, sizeof(path), "/proc/self/%s", name);
  } else {
    snprintf(path, sizeof(path), "/proc/%d/%s", pid, name);
  }
  int fd = open(path, O_RDONLY);
  if (fd < 0) {
    return -errno;
  }
  size_t total = 0;
  for (;;) {
    size_t want = size - total < chunk ? size - total : chunk;
    if (want == 0) {
      break;
    }
    ssize_t got = read(fd, buffer + total, want);
    if (got < 0) {
      int error = errno;
      close(fd);
      return -error;
    }
    if (got == 0) {
      break;
    }
    total += got;
  }
  close(fd);
  return total;
}

static void show_file(const char* label, pid_t pid, const char* name) {
  static char buffer[1 << 16];
  char full[96];
  snprintf(full, sizeof(full), "%s %s", label, name);
  ssize_t got = read_file(pid, name, buffer, sizeof(buffer), sizeof(buffer));
  if (got < 0) {
    printf("%s: %s\n", full, errno_name(-got));
    return;
  }
  print_bytes(full, buffer, got);
}

/* Exec this program again, by the path its current image was started with
 * (no path is printed), as `argv` with role `role` and `PHASE=phase`, keeping
 * the pipes at their numbers. */
static int exec_again(char* const argv[], const char* role, const char* phase) {
  const char* self = (const char*)getauxval(AT_EXECFN);
  char role_var[64], phase_var[64];
  snprintf(role_var, sizeof(role_var), "TWIN_ROLE=%s", role);
  snprintf(phase_var, sizeof(phase_var), "PHASE=%s", phase);
  char* envp[] = {role_var, "TWIN_READY=100", "TWIN_HOLD=101", phase_var, NULL};
  if (self == NULL) {
    return 7;
  }
  syscall(SYS_execve, self, argv, envp);
  return 8;
}

/* The bytes of `fd` from its start, at most `size`. */
static ssize_t read_kept(int fd, char* buffer, size_t size) {
  return pread(fd, buffer, size, 0);
}

/* The second image of a child that kept its own environ and cmdline open:
 * report what the kept descriptors and a fresh environ read, as three
 * lengths, the kept cmdline's bytes and the fresh environ's, then wait for
 * the parent. */
static int own_after_exec(int ready, int hold) {
  static char kept_environ[4096], kept_cmdline[4096], fresh[4096];
  ssize_t lengths[3];
  lengths[0] = read_kept(OWN_ENVIRON_FD, kept_environ, sizeof(kept_environ));
  lengths[1] = read_kept(OWN_CMDLINE_FD, kept_cmdline, sizeof(kept_cmdline));
  lengths[2] = read_file(0, "environ", fresh, sizeof(fresh), sizeof(fresh));
  if (write(ready, lengths, sizeof(lengths)) != sizeof(lengths) ||
      (lengths[1] > 0 && write(ready, kept_cmdline, lengths[1]) != lengths[1]) ||
      (lengths[2] > 0 && write(ready, fresh, lengths[2]) != lengths[2])) {
    return 4;
  }
  char byte;
  while (read(hold, &byte, 1) > 0) {
  }
  return 0;
}

/* The child's side: report that it runs, then wait for the parent (or exit
 * at once, a zombie to be). */
static int child(const char* role) {
  int ready = atoi(getenv("TWIN_READY"));
  int hold = atoi(getenv("TWIN_HOLD"));
  if (strcmp(role, "own-exec") == 0) {
    int environ_fd = open("/proc/self/environ", O_RDONLY);
    int cmdline_fd = open("/proc/self/cmdline", O_RDONLY);
    if (environ_fd < 0 || cmdline_fd < 0 || dup2(environ_fd, OWN_ENVIRON_FD) != OWN_ENVIRON_FD ||
        dup2(cmdline_fd, OWN_CMDLINE_FD) != OWN_CMDLINE_FD) {
      return 5;
    }
    char* argv[] = {"own", "two", NULL};
    return exec_again(argv, "own-after-exec", "own-two");
  }
  if (strcmp(role, "own-after-exec") == 0) {
    return own_after_exec(ready, hold);
  }
  if (strcmp(role, "private") == 0 && prctl(PR_SET_DUMPABLE, 0, 0, 0, 0) != 0) {
    return 3;
  }
  if (strcmp(role, "exit") == 0) {
    return 0;
  }
  if (write(ready, "r", 1) != 1) {
    return 4;
  }
  char byte;
  if (strcmp(role, "reexec") == 0) {
    /* Until the parent says to exec, then the second image runs as "wait". */
    if (read(hold, &byte, 1) != 1) {
      return 6;
    }
    char* argv[] = {"after", "two", NULL};
    return exec_again(argv, "wait", "two");
  }
  /* Until the parent closes the write end. */
  while (read(hold, &byte, 1) > 0) {
  }
  return 0;
}

struct spawned {
  pid_t pid;
  int release;
};

/* Fork and exec this program as `argv` with role `role` and the extra
 * environment `extra`, then wait until it reports it runs (unless it is to
 * exit at once). */
static struct spawned spawn(const char* self, char* const argv[], const char* role,
                            const char* const extra[]) {
  int ready[2], hold[2];
  struct spawned none = {-1, -1};
  if (pipe(ready) != 0 || pipe(hold) != 0) {
    fail("pipe");
    return none;
  }
  pid_t pid = fork();
  if (pid < 0) {
    fail("fork");
    return none;
  }
  if (pid == 0) {
    close(ready[0]);
    close(hold[1]);
    if (dup2(ready[1], READY_FD) != READY_FD || dup2(hold[0], HOLD_FD) != HOLD_FD) {
      _exit(126);
    }
    close(ready[1]);
    close(hold[0]);
    char role_var[64], ready_var[32], hold_var[32];
    snprintf(role_var, sizeof(role_var), "TWIN_ROLE=%s", role);
    snprintf(ready_var, sizeof(ready_var), "TWIN_READY=%d", READY_FD);
    snprintf(hold_var, sizeof(hold_var), "TWIN_HOLD=%d", HOLD_FD);
    char* envp[16];
    int n = 0;
    envp[n++] = role_var;
    envp[n++] = ready_var;
    envp[n++] = hold_var;
    for (int i = 0; extra[i] != NULL && n < 15; i++) {
      envp[n++] = (char*)extra[i];
    }
    envp[n] = NULL;
    syscall(SYS_execve, self, argv, envp);
    _exit(127);
  }
  close(ready[1]);
  close(hold[0]);
  if (strcmp(role, "exit") != 0) {
    char byte;
    if (read(ready[0], &byte, 1) != 1) {
      fail("child ready");
    }
  }
  close(ready[0]);
  struct spawned spawned = {pid, hold[1]};
  return spawned;
}

static void finish(struct spawned spawned, const char* label) {
  close(spawned.release);
  int status;
  if (waitpid(spawned.pid, &status, 0) != spawned.pid || !WIFEXITED(status) ||
      WEXITSTATUS(status) != 0) {
    printf("FAIL %s exit status=%#x\n", label, status);
    failures++;
  }
}

static void several_arguments(const char* self) {
  char* argv[] = {"kid", "alpha", "", "beta gamma", NULL};
  const char* extra[] = {"K1=v1", "EMPTY=", NULL};
  struct spawned kid = spawn(self, argv, "wait", extra);
  if (kid.pid < 0) {
    return;
  }
  show_file("arguments", kid.pid, "cmdline");
  show_file("arguments", kid.pid, "environ");
  static char whole[4096], chunked[4096];
  ssize_t a = read_file(kid.pid, "cmdline", whole, sizeof(whole), sizeof(whole));
  ssize_t b = read_file(kid.pid, "cmdline", chunked, sizeof(chunked), 3);
  printf("arguments cmdline: three-byte reads as one read=%d\n",
         a > 0 && a == b && memcmp(whole, chunked, a) == 0);
  char path[64];
  snprintf(path, sizeof(path), "/proc/%d/cmdline", kid.pid);
  int fd = open(path, O_RDONLY);
  if (fd < 0) {
    fail("open cmdline");
  } else {
    char tail[64];
    off_t at = lseek(fd, 6, SEEK_SET);
    ssize_t got = read(fd, tail, sizeof(tail));
    printf("arguments cmdline: lseek=%lld\n", (long long)at);
    print_bytes("arguments cmdline from 6", tail, got < 0 ? 0 : got);
    got = pread(fd, tail, 4, 13);
    print_bytes("arguments cmdline pread 4 at 13", tail, got < 0 ? 0 : got);
    got = pread(fd, tail, sizeof(tail), 1000);
    printf("arguments cmdline: pread past the end=%zd\n", got);
    close(fd);
  }
  finish(kid, "arguments child");
}

static void empty_arguments(const char* self) {
  char* argv[] = {NULL};
  const char* extra[] = {NULL};
  struct spawned kid = spawn(self, argv, "wait", extra);
  if (kid.pid < 0) {
    return;
  }
  show_file("no arguments", kid.pid, "cmdline");
  finish(kid, "no-arguments child");
}

static void long_argument(const char* self) {
  static char argument[LONG_ARGUMENT + 1];
  for (int i = 0; i < LONG_ARGUMENT; i++) {
    argument[i] = 'a' + i % 26;
  }
  char* argv[] = {"kid", argument, "tail", NULL};
  const char* extra[] = {NULL};
  struct spawned kid = spawn(self, argv, "wait", extra);
  if (kid.pid < 0) {
    return;
  }
  static char buffer[LONG_ARGUMENT + 64];
  ssize_t got = read_file(kid.pid, "cmdline", buffer, sizeof(buffer), 4096);
  unsigned long sum = 0;
  for (ssize_t i = 0; i < got; i++) {
    sum = sum * 31 + (unsigned char)buffer[i];
  }
  int same = got == 4 + LONG_ARGUMENT + 1 + 5 && memcmp(buffer, "kid", 4) == 0 &&
             memcmp(buffer + 4, argument, LONG_ARGUMENT + 1) == 0 &&
             memcmp(buffer + 4 + LONG_ARGUMENT + 1, "tail", 5) == 0;
  printf("long argument cmdline: length=%zd as passed=%d checksum=%lu\n", got, same, sum);
  char path[64];
  snprintf(path, sizeof(path), "/proc/%d/cmdline", kid.pid);
  int fd = open(path, O_RDONLY);
  if (fd < 0) {
    fail("open long cmdline");
  } else {
    char around[8];
    ssize_t n = pread(fd, around, sizeof(around), 4092);
    print_bytes("long argument cmdline pread 8 at 4092", around, n < 0 ? 0 : n);
    close(fd);
  }
  finish(kid, "long-argument child");
}

static void forked_child(void) {
  int hold[2], ready[2];
  if (pipe(hold) != 0 || pipe(ready) != 0) {
    fail("pipe");
    return;
  }
  pid_t pid = fork();
  if (pid == 0) {
    close(hold[1]);
    close(ready[0]);
    char byte;
    if (write(ready[1], "r", 1) != 1) {
      _exit(4);
    }
    while (read(hold[0], &byte, 1) > 0) {
    }
    _exit(0);
  }
  close(hold[0]);
  close(ready[1]);
  char byte;
  if (read(ready[0], &byte, 1) != 1) {
    fail("forked child ready");
  }
  static char own[1 << 16], theirs[1 << 16];
  for (int i = 0; i < 2; i++) {
    const char* name = i == 0 ? "cmdline" : "environ";
    ssize_t a = read_file(0, name, own, sizeof(own), sizeof(own));
    ssize_t b = read_file(pid, name, theirs, sizeof(theirs), sizeof(theirs));
    printf("forked child %s: as the parent's own=%d nonempty=%d\n", name,
           a == b && a >= 0 && memcmp(own, theirs, a) == 0, a > 0);
  }
  struct spawned spawned = {pid, hold[1]};
  close(ready[0]);
  finish(spawned, "forked child");
}

static void zombie(const char* self) {
  char* argv[] = {"zombie-kid", "x", NULL};
  const char* extra[] = {"Z=1", NULL};
  struct spawned kid = spawn(self, argv, "exit", extra);
  if (kid.pid < 0) {
    return;
  }
  siginfo_t info;
  memset(&info, 0, sizeof(info));
  if (waitid(P_PID, kid.pid, &info, WEXITED | WNOWAIT) != 0) {
    fail("waitid zombie");
  }
  show_file("zombie", kid.pid, "cmdline");
  show_file("zombie", kid.pid, "environ");
  finish(kid, "zombie child");
}

/* Clear the caller's effective capability set, keeping the permitted. */
static int drop_effective(void) {
  struct __user_cap_header_struct header = {_LINUX_CAPABILITY_VERSION_3, 0};
  struct __user_cap_data_struct data[2];
  if (syscall(SYS_capget, &header, data) != 0) {
    return -1;
  }
  data[0].effective = 0;
  data[1].effective = 0;
  return syscall(SYS_capset, &header, data);
}

static void private_child(const char* self) {
  char* argv[] = {"private-kid", NULL};
  const char* extra[] = {"SECRET=1", NULL};
  struct spawned kid = spawn(self, argv, "private", extra);
  if (kid.pid < 0) {
    return;
  }
  int report[2];
  if (pipe(report) != 0) {
    fail("pipe");
    return;
  }
  pid_t reader = fork();
  if (reader == 0) {
    close(report[0]);
    if (drop_effective() != 0) {
      _exit(5);
    }
    char path[64];
    snprintf(path, sizeof(path), "/proc/%d/environ", kid.pid);
    int fd = open(path, O_RDONLY);
    int environ_error = fd < 0 ? errno : 0;
    if (fd >= 0) {
      close(fd);
    }
    static char buffer[256];
    ssize_t got = read_file(kid.pid, "cmdline", buffer, sizeof(buffer), sizeof(buffer));
    int answers[2] = {environ_error, (int)got};
    if (write(report[1], answers, sizeof(answers)) != sizeof(answers)) {
      _exit(6);
    }
    _exit(0);
  }
  close(report[1]);
  int answers[2] = {-1, -1};
  if (read(report[0], answers, sizeof(answers)) != sizeof(answers)) {
    fail("reader report");
  }
  close(report[0]);
  int status;
  waitpid(reader, &status, 0);
  printf("non-dumpable child: environ open=%s cmdline length=%d\n", errno_name(answers[0]),
         answers[1]);
  finish(kid, "non-dumpable child");
}

/* Another process's environ and cmdline, held open across its exec. */
static void across_exec(const char* self) {
  int ready[2], hold[2];
  if (pipe(ready) != 0 || pipe(hold) != 0) {
    fail("pipe");
    return;
  }
  pid_t pid = fork();
  if (pid < 0) {
    fail("fork");
    return;
  }
  if (pid == 0) {
    close(ready[0]);
    close(hold[1]);
    if (dup2(ready[1], READY_FD) != READY_FD || dup2(hold[0], HOLD_FD) != HOLD_FD) {
      _exit(126);
    }
    char* argv[] = {"before", "one", NULL};
    char* envp[] = {"TWIN_ROLE=reexec", "TWIN_READY=100", "TWIN_HOLD=101", "PHASE=one", NULL};
    syscall(SYS_execve, self, argv, envp);
    _exit(127);
  }
  close(ready[1]);
  close(hold[0]);
  char byte;
  if (read(ready[0], &byte, 1) != 1) {
    fail("across-exec child ready");
  }
  char path[64];
  snprintf(path, sizeof(path), "/proc/%d/environ", pid);
  int environ_fd = open(path, O_RDONLY);
  snprintf(path, sizeof(path), "/proc/%d/cmdline", pid);
  int cmdline_fd = open(path, O_RDONLY);
  if (environ_fd < 0 || cmdline_fd < 0) {
    fail("open across-exec files");
  } else {
    static char buffer[4096];
    ssize_t got = read_kept(environ_fd, buffer, sizeof(buffer));
    print_bytes("across exec: kept environ before", buffer, got < 0 ? 0 : got);
    got = read_kept(cmdline_fd, buffer, sizeof(buffer));
    print_bytes("across exec: kept cmdline before", buffer, got < 0 ? 0 : got);
    /* Exec, and wait until the second image runs. */
    if (write(hold[1], "x", 1) != 1 || read(ready[0], &byte, 1) != 1) {
      fail("across-exec second image");
    }
    got = read_kept(environ_fd, buffer, sizeof(buffer));
    printf("across exec: kept environ after: read=%zd errno=%s\n", got,
           errno_name(got < 0 ? errno : 0));
    got = read_kept(cmdline_fd, buffer, sizeof(buffer));
    print_bytes("across exec: kept cmdline after", buffer, got < 0 ? 0 : got);
    close(environ_fd);
    close(cmdline_fd);
  }
  show_file("across exec: reopened", pid, "environ");
  close(ready[0]);
  struct spawned spawned = {pid, hold[1]};
  finish(spawned, "across-exec child");
}

/* A child's own environ and cmdline, kept open across its own exec. */
static void own_across_exec(const char* self) {
  int ready[2], hold[2];
  if (pipe(ready) != 0 || pipe(hold) != 0) {
    fail("pipe");
    return;
  }
  pid_t pid = fork();
  if (pid < 0) {
    fail("fork");
    return;
  }
  if (pid == 0) {
    close(ready[0]);
    close(hold[1]);
    if (dup2(ready[1], READY_FD) != READY_FD || dup2(hold[0], HOLD_FD) != HOLD_FD) {
      _exit(126);
    }
    char* argv[] = {"own", "one", NULL};
    char* envp[] = {"TWIN_ROLE=own-exec", "TWIN_READY=100", "TWIN_HOLD=101", "PHASE=own-one",
                    NULL};
    syscall(SYS_execve, self, argv, envp);
    _exit(127);
  }
  close(ready[1]);
  close(hold[0]);
  ssize_t lengths[3] = {-1, -1, -1};
  static char cmdline[4096], fresh[4096];
  if (read(ready[0], lengths, sizeof(lengths)) != sizeof(lengths) ||
      (lengths[1] > 0 && read(ready[0], cmdline, lengths[1]) != lengths[1]) ||
      (lengths[2] > 0 && read(ready[0], fresh, lengths[2]) != lengths[2])) {
    fail("own-across-exec report");
  } else {
    printf("own across exec: kept environ after: read=%zd\n", lengths[0]);
    print_bytes("own across exec: kept cmdline after", cmdline, lengths[1] < 0 ? 0 : lengths[1]);
    print_bytes("own across exec: fresh environ", fresh, lengths[2] < 0 ? 0 : lengths[2]);
  }
  close(ready[0]);
  struct spawned spawned = {pid, hold[1]};
  finish(spawned, "own-across-exec child");
}

int main(int argc, char** argv) {
  const char* role = getenv("TWIN_ROLE");
  if (role != NULL) {
    return child(role);
  }
  if (argc < 1) {
    return 2;
  }
  several_arguments(argv[0]);
  empty_arguments(argv[0]);
  long_argument(argv[0]);
  forked_child();
  zombie(argv[0]);
  private_child(argv[0]);
  across_exec(argv[0]);
  own_across_exec(argv[0]);
  printf("failures=%d\n", failures);
  return failures != 0;
}
