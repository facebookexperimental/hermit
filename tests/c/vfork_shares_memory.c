/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/* A vfork child shares its parent's whole address space until it execs or
 * exits (vfork(2); copy_mm() gives a CLONE_VM child its parent's mm), and the
 * parent's call returns only then. Each case's child stores into a parent
 * global and reads a file into a parent buffer, then ends its window: a
 * successful exec of this program, a failed exec followed by _exit (storing
 * errno first), an _exit, or death by SIGTERM. The parent reads what it sees
 * as its call returns, before it waits for the child, and prints it after.
 * Every case runs for vfork(), for clone() with CLONE_VM|CLONE_VFORK on
 * a stack of its own, and for clone3() with CLONE_VM|CLONE_VFORK on a stack of
 * its own, first while the process holds no shared view of a file and then
 * while it holds one. posix_spawn of a missing program reports ENOENT, which
 * glibc's child writes into the parent's memory before it exits. The children
 * exec this program by a relative path, through a link in the working
 * directory, as a shell or make often starts a program.
 *
 * Arguments:
 * - none: the cases above without a view, then "blocking" and "threads",
 *   then the cases above with a view;
 * - "helper": the image the children exec, which exits 0;
 * - "blocking": a vfork child blocks in its window reading a pipe that a
 *   forked process writes after a pause; the parent stays inside vfork until
 *   the child exits and then sees what the child read;
 * - "threads": the parent has another live thread; the child and that thread
 *   store different bytes of the same page during the window, ordered by two
 *   pipes, and the parent sees both stores;
 * - "conflict": as "threads", but the thread stores to the byte the child
 *   stored, after the child: Linux keeps the thread's value;
 * - "map-change CALL": a vfork child of a process that holds no shared view
 *   changes a mapping with CALL (mmap, munmap, mprotect, mremap or brk) and
 *   exits, where on Linux the change is its parent's too; or, with CALL
 *   "scratch", maps a page, stores into it, makes it read-only and writable
 *   again and unmaps it, leaving its parent's mappings as they were.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <spawn.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

extern char **environ;

#define REQUIRE(condition, step)                                               \
  do {                                                                        \
    if (!(condition)) {                                                       \
      fprintf(stderr, "%s failed errno=%d\n", step, errno);                    \
      _exit(1);                                                               \
    }                                                                         \
  } while (0)

static const char data_path[] = "vfork-shares-memory.data";
static char helper_path[] = "./vfork-shares-memory-helper";
static const char data[] = "vfork child read this from the file";

/* What the children change in their parent's memory. */
static volatile int stored = 7;
static char read_buffer[sizeof(data)];
static volatile int failed_errno;

/* The page the "threads" and "conflict" cases share: the child stores at
 * [0], the other thread at [64] or [0]. */
static volatile unsigned char shared_page[4096] __attribute__((aligned(4096)));

enum ending { EXEC, FAILED_EXEC, EXIT, SIGNAL };
static const char *const ending_names[] = {"exec", "failed-exec", "exit", "signal"};

struct child_case {
  int value;
  enum ending ending;
  int fd;
  char *program;
};

static long raw_kill(long pid, long signal) {
  return syscall(SYS_kill, pid, signal);
}

/* The child's work: store, read the file into the parent's buffer, end. */
static int child_body(void *argument) {
  struct child_case *child_case = argument;
  stored = child_case->value;
  if (pread(child_case->fd, read_buffer, sizeof(read_buffer), 0) !=
      (ssize_t)sizeof(read_buffer)) {
    _exit(10);
  }
  switch (child_case->ending) {
  case EXEC: {
    char *child_argv[] = {child_case->program, "helper", NULL};
    execve(child_case->program, child_argv, environ);
    _exit(127);
  }
  case FAILED_EXEC: {
    char *missing_argv[] = {"missing", NULL};
    execve("/nonexistent/vfork-shares-memory-helper", missing_argv, environ);
    failed_errno = errno;
    _exit(0);
  }
  case EXIT:
    _exit(0);
  case SIGNAL:
    raw_kill(syscall(SYS_getpid), SIGTERM);
    _exit(11);
  }
  _exit(12);
}

/* struct clone_args of clone3(2), its first version (64 bytes). */
struct vfork_clone_args {
  uint64_t flags;
  uint64_t pidfd;
  uint64_t child_tid;
  uint64_t parent_tid;
  uint64_t exit_signal;
  uint64_t stack;
  uint64_t stack_size;
  uint64_t tls;
};

/* clone3 with CLONE_VM|CLONE_VFORK on `stack`, running `function` there, as
 * glibc's own clone3 entry does: the kernel resumes the child after the
 * syscall instruction on the new stack, and the child calls the function and
 * exits with its result. */
static long clone3_vfork(void *stack, size_t stack_size, int (*function)(void *),
                         void *argument) {
  struct vfork_clone_args args;
  memset(&args, 0, sizeof(args));
  args.flags = CLONE_VM | CLONE_VFORK;
  args.exit_signal = SIGCHLD;
  args.stack = (uintptr_t)stack;
  args.stack_size = stack_size;
  long result;
  register void *r12 __asm__("r12") = (void *)function;
  register void *r13 __asm__("r13") = argument;
  __asm__ volatile("syscall\n\t"
                   "test %%rax, %%rax\n\t"
                   "jnz 1f\n\t"
                   "xor %%ebp, %%ebp\n\t"
                   "mov %%r13, %%rdi\n\t"
                   "call *%%r12\n\t"
                   "mov %%eax, %%edi\n\t"
                   "mov %[exit], %%eax\n\t"
                   "syscall\n\t"
                   "hlt\n"
                   "1:"
                   : "=a"(result)
                   : "0"((long)SYS_clone3), "D"(&args), "S"(sizeof(args)),
                     "r"(r12), "r"(r13), [exit] "i"(SYS_exit)
                   : "rcx", "r11", "memory");
  return result;
}

static void describe_status(int status, char *out, size_t size) {
  if (WIFEXITED(status)) {
    snprintf(out, size, "exit %d", WEXITSTATUS(status));
  } else if (WIFSIGNALED(status)) {
    snprintf(out, size, "signal %d", WTERMSIG(status));
  } else {
    snprintf(out, size, "status %#x", status);
  }
}

static int wait_for(pid_t child) {
  int status = 0;
  pid_t waited;
  do {
    waited = waitpid(child, &status, 0);
  } while (waited < 0 && errno == EINTR);
  REQUIRE(waited == child, "waitpid");
  return status;
}

static void run_case(const char *family, enum ending ending, const char *view,
                     int fd, char *program, int value) {
  stored = 7;
  failed_errno = 0;
  memset(read_buffer, 0, sizeof(read_buffer));
  struct child_case child_case = {value, ending, fd, program};
  static char stack[64 * 1024] __attribute__((aligned(16)));
  pid_t child;
  if (strcmp(family, "vfork") == 0) {
    child = vfork();
    REQUIRE(child >= 0, "vfork");
    if (child == 0) {
      child_body(&child_case);
    }
  } else if (strcmp(family, "clone") == 0) {
    child = clone(child_body, stack + sizeof(stack), CLONE_VM | CLONE_VFORK | SIGCHLD,
                  &child_case);
    REQUIRE(child >= 0, "clone");
  } else {
    child = (pid_t)clone3_vfork(stack, sizeof(stack), child_body, &child_case);
    REQUIRE(child >= 0, "clone3");
  }
  /* The call returns once the child's window has ended, so what the child
   * stored is the parent's now, before any wait. */
  int seen_stored = stored;
  int seen_read = memcmp(read_buffer, data, sizeof(data)) == 0;
  int seen_failed = failed_errno;
  int status = wait_for(child);
  char described[32];
  describe_status(status, described, sizeof(described));
  printf("%s %s %s: stored=%d read=%s failed=%d %s\n", family,
         ending_names[ending], view, seen_stored, seen_read ? "file" : "other",
         seen_failed, described);
}

static void run_cases(const char *view, int fd, char *program) {
  static const char *const families[] = {"vfork", "clone", "clone3"};
  int value = 91;
  for (unsigned family = 0; family < 3; family++) {
    for (enum ending ending = EXEC; ending <= SIGNAL; ending++) {
      run_case(families[family], ending, view, fd, program, value++);
    }
  }
  pid_t child = -1;
  char *missing_argv[] = {"missing", NULL};
  int result = posix_spawn(&child, "/nonexistent/vfork-shares-memory-program", NULL,
                           NULL, missing_argv, environ);
  printf("posix_spawn missing %s: %s\n", view,
         result == ENOENT ? "ENOENT" : result == 0 ? "0" : strerror(result));
  if (result == 0) {
    wait_for(child);
  }
  char *helper_argv[] = {program, "helper", NULL};
  result = posix_spawn(&child, program, NULL, NULL, helper_argv, environ);
  REQUIRE(result == 0, "posix_spawn helper");
  int status = wait_for(child);
  char described[32];
  describe_status(status, described, sizeof(described));
  printf("posix_spawn helper %s: %s\n", view, described);
}

/* A vfork child blocks in its window, reading a pipe a forked process writes
 * after a pause. */
static int blocking(void) {
  int pipe_fds[2];
  REQUIRE(pipe(pipe_fds) == 0, "pipe");
  pid_t writer = fork();
  REQUIRE(writer >= 0, "fork writer");
  if (writer == 0) {
    struct timespec pause = {0, 20 * 1000 * 1000};
    nanosleep(&pause, NULL);
    _exit(write(pipe_fds[1], "written by another process", 27) == 27 ? 0 : 1);
  }
  memset(read_buffer, 0, sizeof(read_buffer));
  pid_t child = vfork();
  REQUIRE(child >= 0, "vfork blocking");
  if (child == 0) {
    ssize_t got = read(pipe_fds[0], read_buffer, 27);
    stored = (int)got;
    _exit(got == 27 ? 0 : 1);
  }
  char seen[sizeof(read_buffer)];
  memcpy(seen, read_buffer, sizeof(seen));
  int seen_stored = stored;
  int status = wait_for(child);
  int writer_status = wait_for(writer);
  printf("blocking: child read \"%s\" (%d bytes), child %s, writer %s\n", seen,
         seen_stored, WIFEXITED(status) && WEXITSTATUS(status) == 0 ? "ok" : "failed",
         WIFEXITED(writer_status) && WEXITSTATUS(writer_status) == 0 ? "ok" : "failed");
  return 0;
}

static int to_thread[2];
static int to_child[2];
static int conflict;

static void *thread_main(void *unused) {
  (void)unused;
  char byte;
  if (read(to_thread[0], &byte, 1) != 1) {
    return (void *)1;
  }
  shared_page[conflict ? 0 : 64] = 0x55;
  if (write(to_child[1], "k", 1) != 1) {
    return (void *)2;
  }
  return NULL;
}

/* The parent has another live thread, which stores into the page the child
 * stores into, during the window, after the child's store. */
static int threads(int same_byte) {
  conflict = same_byte;
  memset((void *)shared_page, 0, sizeof(shared_page));
  REQUIRE(pipe(to_thread) == 0 && pipe(to_child) == 0, "pipes");
  pthread_t thread;
  REQUIRE(pthread_create(&thread, NULL, thread_main, NULL) == 0, "pthread_create");
  pid_t child = vfork();
  REQUIRE(child >= 0, "vfork threads");
  if (child == 0) {
    shared_page[0] = 0x91;
    char byte;
    if (write(to_thread[1], "c", 1) != 1 || read(to_child[0], &byte, 1) != 1) {
      _exit(1);
    }
    _exit(0);
  }
  unsigned seen_child = shared_page[0];
  unsigned seen_thread = shared_page[64];
  int status = wait_for(child);
  void *result;
  REQUIRE(pthread_join(thread, &result) == 0 && result == NULL, "pthread_join");
  printf("%s: child %s, page[0]=%#x page[64]=%#x\n", same_byte ? "conflict" : "threads",
         WIFEXITED(status) && WEXITSTATUS(status) == 0 ? "ok" : "failed", seen_child,
         seen_thread);
  return 0;
}

/* A vfork child changes a mapping with `call` before it exits. */
static int map_change(const char *call) {
  unsigned char *page =
      mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  REQUIRE(page != MAP_FAILED, "mmap page");
  long program_break = syscall(SYS_brk, 0);
  pid_t child = vfork();
  REQUIRE(child >= 0, "vfork map-change");
  if (child == 0) {
    long failed;
    if (strcmp(call, "mmap") == 0) {
      failed = mmap(NULL, 4096, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0) == MAP_FAILED;
    } else if (strcmp(call, "munmap") == 0) {
      failed = munmap(page, 4096) != 0;
    } else if (strcmp(call, "mprotect") == 0) {
      failed = mprotect(page, 4096, PROT_READ) != 0;
    } else if (strcmp(call, "mremap") == 0) {
      failed = mremap(page, 4096, 8192, MREMAP_MAYMOVE) == MAP_FAILED;
    } else if (strcmp(call, "scratch") == 0) {
      unsigned char *scratch =
          mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
      failed = scratch == MAP_FAILED;
      if (!failed) {
        scratch[0] = 1;
        failed = mprotect(scratch, 4096, PROT_READ) != 0 ||
                 mprotect(scratch, 4096, PROT_READ | PROT_WRITE) != 0 ||
                 munmap(scratch, 4096) != 0;
      }
    } else {
      failed = syscall(SYS_brk, program_break + 4096) != program_break + 4096;
    }
    _exit(failed ? 1 : 0);
  }
  int status = wait_for(child);
  printf("vfork map-change=%s child=%s\n", call,
         WIFEXITED(status) && WEXITSTATUS(status) == 0 ? "ok" : "failed");
  return 0;
}

int main(int argc, char **argv) {
  if (argc == 2 && strcmp(argv[1], "helper") == 0) {
    return 0;
  }
  if (argc == 3 && strcmp(argv[1], "map-change") == 0) {
    return map_change(argv[2]);
  }
  if (argc == 2 && strcmp(argv[1], "blocking") == 0) {
    return blocking();
  }
  if (argc == 2 && strcmp(argv[1], "threads") == 0) {
    return threads(0);
  }
  if (argc == 2 && strcmp(argv[1], "conflict") == 0) {
    return threads(1);
  }
  REQUIRE(argc == 1, "usage");
  char *self = realpath(argv[0], NULL);
  unlink(helper_path);
  REQUIRE(symlink(self != NULL ? self : argv[0], helper_path) == 0, "symlink helper");
  free(self);
  int fd = open(data_path, O_RDWR | O_CREAT | O_TRUNC | O_CLOEXEC, 0600);
  REQUIRE(fd >= 0, "open data");
  REQUIRE(write(fd, data, sizeof(data)) == (ssize_t)sizeof(data), "write data");
  REQUIRE(ftruncate(fd, 4096) == 0, "ftruncate data");
  run_cases("no-view", fd, helper_path);
  /* Before the process maps a view: once it has unmapped one, KVM keeps the
   * pages that held it as separately backed pages, and every later fork or
   * vfork copy of this process copies its whole arena, which would spend
   * this program's CPU budget there rather than on what it tests. */
  blocking();
  threads(0);
  void *view = mmap(NULL, 4096, PROT_READ, MAP_SHARED, fd, 0);
  REQUIRE(view != MAP_FAILED, "mmap view");
  REQUIRE(memcmp(view, data, sizeof(data)) == 0, "view contents");
  run_cases("view", fd, helper_path);
  REQUIRE(munmap(view, 4096) == 0, "munmap view");
  /* Leave the working directory as it was, so a second run there sees what
   * the first saw. */
  unlink(helper_path);
  return 0;
}
