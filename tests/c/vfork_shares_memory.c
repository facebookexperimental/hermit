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
 * - none: the cases above without a view, then "blocking", "threads" and
 *   "clear-tid exec", then "leader-exec", whose exec replaces this image
 *   with one that prints its line and then runs the cases above with a view;
 * - "helper": the image the children exec, which exits 0;
 * - "blocking": a vfork child blocks in its window reading a pipe that a
 *   forked process writes after a pause; the parent stays inside vfork until
 *   the child exits and then sees what the child read;
 * - "threads": the parent has another live thread; the child and that thread
 *   store different bytes of the same page during the window, ordered by two
 *   pipes, and the parent sees both stores. Run alone in a lazily bound
 *   build, the child and the thread each resolve read or write through the
 *   dynamic loader during the window, and both change its relocation counter,
 *   which KVM refuses by name as a conflict; the full run resolves both
 *   before ("blocking" does), and so do the modes below that need it;
 * - "conflict": as "threads", but the thread stores to the byte the child
 *   stored, after the child: Linux keeps the thread's value;
 * - "map-change CALL": a vfork child of a process that holds no shared view
 *   changes a mapping with CALL (mmap, munmap, mprotect, mremap or brk) and
 *   exits, where on Linux the change is its parent's too; or, with CALL
 *   "scratch", maps a page, stores into it, makes it read-only and writable
 *   again and unmaps it, leaving its parent's mappings as they were; or, with
 *   CALL "remap", unmaps a page of the parent's and maps a new one there with
 *   the same protection, where on Linux the parent then has the new page;
 * - "parent-remap": the parent has another live thread; the child stores into
 *   a page of an anonymous mapping, and the thread then maps a new anonymous
 *   page over it with MAP_FIXED, during the window: on Linux the child's store
 *   went with the old page, and the parent sees zero;
 * - "parent-regrow": as "parent-remap", but the child stores into the second
 *   page of a zeroed two-page anonymous mapping, and the thread shrinks the
 *   mapping in place to its first page and grows it back (mremap) during the
 *   window: on Linux the child's store went with the page the shrink
 *   removed, the grow brings back a zero page, and the parent sees zero;
 * - "clear-tid ENDING": a vfork child registers a parent word holding 91 as
 *   its clear_child_tid (set_tid_address) and ends its window by ENDING (exec,
 *   failed-exec, exit, signal, or segv, a store to an unmapped address that
 *   kills it with SIGSEGV): Linux's mm_release() clears the word before
 *   the parent's call returns, since the parent shares the mm; a failed exec
 *   keeps the registration, and the child's exit clears it. With ENDING
 *   "exit-wake", another thread of the parent waits on the word (FUTEX_WAIT
 *   for 91, for at most 5 s) while the child exits after a pause: Linux wakes
 *   it, or it finds the word already 0;
 * - "thread-in-child": a vfork child creates a thread, waits for it to exit
 *   and exits;
 * - "leader-exec": a second thread of the process vforks a child that blocks
 *   in its window, and the main thread execs this program meanwhile: on
 *   Linux the exec kills the waiting thread (its wait is killable), the
 *   child runs on as the process's child, and the new image releases it and
 *   reaps it; the child's store went to the old image;
 * - "parent-dies": the main thread vforks a child that blocks reading a pipe
 *   only the parent's process can write, and another thread exits the
 *   process with status 3 during the window: on Linux the waiting thread
 *   dies, the process exits 3, and the child reads end of file;
 * - "parent-dies conflict": as "parent-dies", but the child and the exiting
 *   thread first store different values to the same byte;
 * - "parent-dies map": as "parent-dies", but the child maps a page after it
 *   reads end of file and keeps it. On Linux neither changes the outcome:
 *   once the parent's process has exited, the child is the only user of the
 *   address space.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <linux/futex.h>
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
    } else if (strcmp(call, "remap") == 0) {
      failed = munmap(page, 4096) != 0 ||
               mmap(page, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED,
                    -1, 0) != page;
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

/* Resolves, in a lazily bound build, every function the modes below call
 * during a window, so that no window resolves a symbol through the dynamic
 * loader there (see "threads" above). */
static void resolve_before_window(void) {
  int fds[2];
  char byte = 0;
  REQUIRE(pipe(fds) == 0, "pipe resolve");
  REQUIRE(write(fds[1], &byte, 1) == 1 && read(fds[0], &byte, 1) == 1, "resolve");
  REQUIRE(close(fds[0]) == 0 && close(fds[1]) == 0, "close resolve");
  struct timespec none = {0, 0};
  nanosleep(&none, NULL);
  void *page = mmap(NULL, 4096, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  REQUIRE(page != MAP_FAILED && munmap(page, 4096) == 0, "mmap resolve");
  REQUIRE(syscall(SYS_getpid) > 0, "syscall resolve");
}

static volatile unsigned char *remap_page;

static void *remap_thread(void *unused) {
  (void)unused;
  char byte;
  if (read(to_thread[0], &byte, 1) != 1) {
    return (void *)1;
  }
  if (mmap((void *)remap_page, 4096, PROT_READ | PROT_WRITE,
           MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, -1, 0) != (void *)remap_page) {
    return (void *)2;
  }
  if (write(to_child[1], "k", 1) != 1) {
    return (void *)3;
  }
  return NULL;
}

static volatile unsigned char *regrow_mapping;

static void *regrow_thread(void *unused) {
  (void)unused;
  char byte;
  if (read(to_thread[0], &byte, 1) != 1) {
    return (void *)1;
  }
  /* syscall() rather than mremap(), which resolve_before_window() leaves
   * unresolved. */
  if (syscall(SYS_mremap, regrow_mapping, 8192, 4096, 0) != (long)regrow_mapping ||
      syscall(SYS_mremap, regrow_mapping, 4096, 8192, 0) != (long)regrow_mapping) {
    return (void *)2;
  }
  if (write(to_child[1], "k", 1) != 1) {
    return (void *)3;
  }
  return NULL;
}

/* The parent's other thread shrinks, in place, a two-page mapping whose second
 * page the child stored into, and grows it back, during the window. */
static int parent_regrow(void) {
  resolve_before_window();
  regrow_mapping = mmap(NULL, 8192, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  REQUIRE(regrow_mapping != MAP_FAILED, "mmap regrow mapping");
  REQUIRE(pipe(to_thread) == 0 && pipe(to_child) == 0, "pipes");
  pthread_t thread;
  REQUIRE(pthread_create(&thread, NULL, regrow_thread, NULL) == 0, "pthread_create");
  pid_t child = vfork();
  REQUIRE(child >= 0, "vfork parent-regrow");
  if (child == 0) {
    regrow_mapping[4096 + 100] = 0x91;
    char byte;
    if (write(to_thread[1], "c", 1) != 1 || read(to_child[0], &byte, 1) != 1) {
      _exit(1);
    }
    _exit(0);
  }
  unsigned seen = regrow_mapping[4096 + 100];
  int status = wait_for(child);
  void *result;
  REQUIRE(pthread_join(thread, &result) == 0 && result == NULL, "pthread_join");
  printf("parent-regrow: child %s, page[1][100]=%#x\n",
         WIFEXITED(status) && WEXITSTATUS(status) == 0 ? "ok" : "failed", seen);
  return 0;
}

/* The parent's other thread replaces the mapping of a page the child stored
 * into, during the window. */
static int parent_remap(void) {
  resolve_before_window();
  remap_page = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  REQUIRE(remap_page != MAP_FAILED, "mmap remap page");
  REQUIRE(pipe(to_thread) == 0 && pipe(to_child) == 0, "pipes");
  pthread_t thread;
  REQUIRE(pthread_create(&thread, NULL, remap_thread, NULL) == 0, "pthread_create");
  pid_t child = vfork();
  REQUIRE(child >= 0, "vfork parent-remap");
  if (child == 0) {
    remap_page[100] = 0x91;
    char byte;
    if (write(to_thread[1], "c", 1) != 1 || read(to_child[0], &byte, 1) != 1) {
      _exit(1);
    }
    _exit(0);
  }
  unsigned seen = remap_page[100];
  int status = wait_for(child);
  void *result;
  REQUIRE(pthread_join(thread, &result) == 0 && result == NULL, "pthread_join");
  printf("parent-remap: child %s, page[100]=%#x\n",
         WIFEXITED(status) && WEXITSTATUS(status) == 0 ? "ok" : "failed", seen);
  return 0;
}

static volatile int tid_word;
static volatile int after_failed_exec;
/* An unmapped address the "segv" ending stores to. */
static volatile uintptr_t fault_address = 8;
static volatile int waiter_errno;

/* Waits on tid_word while it holds 91, for at most 5 s, once it has told the
 * child it is about to. */
static void *tid_waiter(void *unused) {
  (void)unused;
  if (write(to_child[1], "w", 1) != 1) {
    return (void *)1;
  }
  struct timespec timeout = {5, 0};
  long rc = syscall(SYS_futex, &tid_word, FUTEX_WAIT, 91, &timeout, NULL, 0);
  waiter_errno = rc == 0 ? 0 : errno;
  return NULL;
}

/* A vfork child registers tid_word as its clear_child_tid and ends its window
 * by `ending`. */
static int clear_tid(const char *ending, char *program) {
  tid_word = 91;
  after_failed_exec = 0;
  int waiting = strcmp(ending, "exit-wake") == 0;
  pthread_t thread;
  if (waiting) {
    resolve_before_window();
    REQUIRE(pipe(to_child) == 0, "pipe");
    REQUIRE(pthread_create(&thread, NULL, tid_waiter, NULL) == 0, "pthread_create");
  }
  pid_t child = vfork();
  REQUIRE(child >= 0, "vfork clear-tid");
  if (child == 0) {
    syscall(SYS_set_tid_address, &tid_word);
    if (strcmp(ending, "exec") == 0) {
      char *child_argv[] = {program, "helper", NULL};
      execve(program, child_argv, environ);
      _exit(127);
    }
    if (strcmp(ending, "failed-exec") == 0) {
      char *missing_argv[] = {"missing", NULL};
      execve("/nonexistent/vfork-shares-memory-helper", missing_argv, environ);
      after_failed_exec = tid_word;
      _exit(0);
    }
    if (strcmp(ending, "signal") == 0) {
      raw_kill(syscall(SYS_getpid), SIGTERM);
      _exit(11);
    }
    if (strcmp(ending, "segv") == 0) {
      /* Through a volatile, so that gcc's -Warray-bounds does not reject a
       * store to a constant null-page address (Reverie's fixture, built
       * without -Werror, stores to address 8 directly). */
      *(volatile int *)fault_address = 1;
      _exit(12);
    }
    if (waiting) {
      char byte;
      struct timespec pause = {0, 50 * 1000 * 1000};
      if (read(to_child[0], &byte, 1) != 1) {
        _exit(20);
      }
      nanosleep(&pause, NULL);
    }
    _exit(0);
  }
  int seen = tid_word;
  int status = wait_for(child);
  char described[32];
  describe_status(status, described, sizeof(described));
  if (waiting) {
    void *result;
    REQUIRE(pthread_join(thread, &result) == 0 && result == NULL, "pthread_join");
    printf("clear-tid %s: word=%d waiter %s %s\n", ending, seen,
           waiter_errno == 0 || waiter_errno == EAGAIN ? "woken" : strerror(waiter_errno),
           described);
    return 0;
  }
  printf("clear-tid %s: word=%d after-failed-exec=%d %s\n", ending, seen, after_failed_exec,
         described);
  return 0;
}

static char thread_stack[64 * 1024] __attribute__((aligned(16)));
static volatile int thread_ran;
static volatile pid_t thread_tid;

static int thread_body(void *unused) {
  (void)unused;
  thread_ran = 1;
  return 0;
}

/* A vfork child creates a thread, waits for it to exit, and exits. */
static int thread_in_child(void) {
  pid_t child = vfork();
  REQUIRE(child >= 0, "vfork thread-in-child");
  if (child == 0) {
    /* The kernel stores the new thread's ID in thread_tid before the thread
     * runs (CLONE_PARENT_SETTID) and clears it when the thread exits
     * (CLONE_CHILD_CLEARTID), so the exit cannot come before the store. */
    long tid = clone(thread_body, thread_stack + sizeof(thread_stack),
                     CLONE_VM | CLONE_FS | CLONE_FILES | CLONE_SIGHAND | CLONE_THREAD |
                         CLONE_SYSVSEM | CLONE_PARENT_SETTID | CLONE_CHILD_CLEARTID,
                     NULL, (pid_t *)&thread_tid, NULL, (pid_t *)&thread_tid);
    if (tid < 0) {
      _exit(20);
    }
    while (thread_tid != 0) {
      syscall(SYS_futex, &thread_tid, FUTEX_WAIT, (int)tid, NULL, NULL, 0);
    }
    _exit(thread_ran ? 0 : 21);
  }
  int status = wait_for(child);
  char described[32];
  describe_status(status, described, sizeof(described));
  printf("thread-in-child: %s\n", described);
  return 0;
}

static int ready_pipe[2];
static int release_pipe[2];

static void *vfork_worker(void *unused) {
  (void)unused;
  pid_t child = vfork();
  if (child == 0) {
    stored = 99;
    pid_t self = (pid_t)syscall(SYS_getpid);
    char byte;
    if (write(ready_pipe[1], &self, sizeof(self)) != (ssize_t)sizeof(self) ||
        read(release_pipe[0], &byte, 1) != 1) {
      _exit(20);
    }
    _exit(7);
  }
  /* Not reached on Linux: the main thread's exec kills this thread first. */
  return (void *)(intptr_t)child;
}

/* A second thread vforks a child that blocks in its window, and the main
 * thread execs this program meanwhile. */
static int leader_exec(char *program, char *then) {
  resolve_before_window();
  REQUIRE(pipe(ready_pipe) == 0 && pipe(release_pipe) == 0, "pipes");
  pthread_t thread;
  REQUIRE(pthread_create(&thread, NULL, vfork_worker, NULL) == 0, "pthread_create");
  pid_t child;
  REQUIRE(read(ready_pipe[0], &child, sizeof(child)) == (ssize_t)sizeof(child), "read child");
  char child_arg[16];
  char fd_arg[16];
  snprintf(child_arg, sizeof(child_arg), "%d", (int)child);
  snprintf(fd_arg, sizeof(fd_arg), "%d", release_pipe[1]);
  char *exec_argv[] = {program, "leader-exec-image", child_arg, fd_arg, then, NULL};
  /* The exec discards what stdio still buffers. */
  fflush(stdout);
  execve(program, exec_argv, environ);
  REQUIRE(0, "execve");
  return 1;
}

/* The cases with a view, which the default run's last image runs (see
 * main): the data file and the helper link are the first image's. */
static int view_cases(void) {
  int fd = open(data_path, O_RDWR | O_CLOEXEC);
  REQUIRE(fd >= 0, "open data");
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

/* The image the main thread exec'd: releases the child and reaps it. */
static int leader_exec_image(const char *child_arg, const char *fd_arg) {
  pid_t child = (pid_t)atoi(child_arg);
  int seen = stored;
  REQUIRE(write(atoi(fd_arg), "r", 1) == 1, "release child");
  int status = wait_for(child);
  char described[32];
  describe_status(status, described, sizeof(described));
  printf("leader-exec: stored=%d after=%d child %s\n", seen, stored, described);
  return 0;
}

static int dies_pipe[2];
static volatile unsigned char dies_byte;

/* Exits the process after a pause, storing 2 to dies_byte first when
 * `conflict` is not null. */
static void *exit_process(void *conflict) {
  struct timespec pause = {0, 50 * 1000 * 1000};
  nanosleep(&pause, NULL);
  if (conflict != NULL) {
    dies_byte = 2;
  }
  _exit(3);
}

/* Another thread exits the process while the main thread waits in vfork for
 * a child that blocks until the process's end closes a pipe; `variant` is
 * null, "conflict" or "map". */
static int parent_dies(const char *variant) {
  int conflict = variant != NULL && strcmp(variant, "conflict") == 0;
  int map = variant != NULL && strcmp(variant, "map") == 0;
  REQUIRE(variant == NULL || conflict || map, "parent-dies variant");
  resolve_before_window();
  REQUIRE(pipe(dies_pipe) == 0, "pipe");
  pthread_t thread;
  REQUIRE(pthread_create(&thread, NULL, exit_process, conflict ? (void *)&dies_byte : NULL) == 0,
          "pthread_create");
  pid_t child = vfork();
  REQUIRE(child >= 0, "vfork parent-dies");
  if (child == 0) {
    char byte;
    if (conflict) {
      dies_byte = 1;
    }
    /* The child's own file table holds a copy of the write end. */
    close(dies_pipe[1]);
    int end_of_file = read(dies_pipe[0], &byte, 1) == 0;
    if (map &&
        mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0) == MAP_FAILED) {
      _exit(2);
    }
    _exit(end_of_file ? 0 : 1);
  }
  /* Not reached on Linux: the other thread ends the process first. */
  printf("parent returned from vfork\n");
  return 4;
}

int main(int argc, char **argv) {
  if (argc == 2 && strcmp(argv[1], "helper") == 0) {
    return 0;
  }
  if (argc == 4 && strcmp(argv[1], "leader-exec-image") == 0) {
    return leader_exec_image(argv[2], argv[3]);
  }
  /* The default run's last image: the leader-exec line, then the cases with
   * a view. */
  if (argc == 5 && strcmp(argv[1], "leader-exec-image") == 0 && strcmp(argv[4], "view") == 0) {
    int status = leader_exec_image(argv[2], argv[3]);
    return status != 0 ? status : view_cases();
  }
  if (argc == 2 && strcmp(argv[1], "leader-exec") == 0) {
    return leader_exec(argv[0], NULL);
  }
  if (argc == 2 && strcmp(argv[1], "parent-dies") == 0) {
    return parent_dies(NULL);
  }
  if (argc == 3 && strcmp(argv[1], "parent-dies") == 0) {
    return parent_dies(argv[2]);
  }
  if (argc == 2 && strcmp(argv[1], "parent-remap") == 0) {
    return parent_remap();
  }
  if (argc == 2 && strcmp(argv[1], "parent-regrow") == 0) {
    return parent_regrow();
  }
  if (argc == 3 && strcmp(argv[1], "clear-tid") == 0) {
    return clear_tid(argv[2], argv[0]);
  }
  if (argc == 2 && strcmp(argv[1], "thread-in-child") == 0) {
    return thread_in_child();
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
   * this program's CPU budget there rather than on what it tests. So the
   * cases with a view run last, in the image "leader-exec" execs, which
   * starts without those pages. "threads" relies on the cases before it
   * having resolved read and write (see its description above). */
  blocking();
  threads(0);
  clear_tid("exec", helper_path);
  REQUIRE(close(fd) == 0, "close data");
  return leader_exec(argv[0], "view");
}
