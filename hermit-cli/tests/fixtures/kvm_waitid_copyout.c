/* Copyright (c) Meta Platforms, Inc. and affiliates.
 * Licensed under the BSD-style license in the LICENSE file. */
#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdatomic.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/resource.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <sys/wait.h>
#include <unistd.h>

/* Raw x86-64 Linux waitid writes these six scalars, not a whole siginfo_t.
 * Its rusage copy precedes them, including on a consuming copyout fault.
 * KVM's raw rusage bytes are zero; this is not native accounting parity. */
_Static_assert(sizeof(siginfo_t) == 128, "x86-64 siginfo ABI");
_Static_assert(sizeof(struct rusage) == 144, "x86-64 rusage ABI");
_Static_assert(offsetof(siginfo_t, si_pid) == 16, "SIGCHLD union offset");
static const size_t field_offsets[] = {0, 4, 8, 16, 20, 24};
static pid_t children[64];
static size_t child_count, page_size, cases, assertions;
static volatile sig_atomic_t alarms;

/* The restarting handler uses only lock-free atomic pointer access,
 * volatile sig_atomic_t observations and the async-signal-safe write/_exit
 * operations. It emits nothing until the interrupted wait has returned. */
_Static_assert(ATOMIC_POINTER_LOCK_FREE == 2, "signal-safe arena pointer");
static _Atomic(unsigned char *) restart_info;
static volatile sig_atomic_t restart_length, restart_pipe;
static volatile sig_atomic_t restart_seen, restart_write_result;
static volatile sig_atomic_t restart_write_errno;
static volatile sig_atomic_t restart_snapshot[2 * 65536];

static void require(int yes, const char *what) {
  ++assertions;
  if (!yes) {
    fprintf(stderr, "waitid-copyout assertion failed: %s (errno=%d)\n", what,
            errno);
    fflush(stdout);
    exit(1);
  }
}

static void hex(const unsigned char *bytes, size_t length) {
  static const char digits[] = "0123456789abcdef";
  for (size_t i = 0; i < length; ++i) {
    putchar(digits[bytes[i] >> 4]);
    putchar(digits[bytes[i] & 15]);
  }
}

struct result { long value; int error; };

static struct result raw_waitid(int which, long id, void *info,
                                unsigned long options, void *usage) {
  errno = 0;
  long value = syscall(SYS_waitid, which, id, info, options, usage);
  return (struct result){value, errno};
}

static void fields(unsigned char *where, size_t count, pid_t child, int status) {
  int32_t values[] = {child ? SIGCHLD : 0, 0, child ? CLD_EXITED : 0,
                      child, child ? (int32_t)getuid() : 0, status};
  for (size_t i = 0; i < count; ++i)
    memcpy(where + field_offsets[i], &values[i], sizeof(values[i]));
}

/* No failed test can leave a pipe-held child behind. The official Rust
 * harness also owns the whole process group and a finite wall deadline. */
static void cleanup_children(void) {
  for (size_t i = 0; i < child_count; ++i) {
    pid_t child = children[i];
    if (child <= 0) continue;
    int status;
    pid_t found = waitpid(child, &status, WNOHANG);
    if (found == 0) {
      (void)kill(child, SIGKILL);
      (void)waitpid(child, &status, 0);
    }
  }
}

static void forget(pid_t child) {
  for (size_t i = 0; i < child_count; ++i)
    if (children[i] == child) {
      children[i] = 0;
      return;
    }
  require(0, "exact tracked child");
}

static void track(pid_t child) {
  require(child > 0 && child_count < 64, "finite fresh child");
  children[child_count++] = child;
}

static void info_check(const char *name, pid_t child, unsigned long options,
                       int error, int event_status) {
  unsigned char bytes[160], expected[160];
  memset(bytes, 0xa5, sizeof(bytes));
  memcpy(expected, bytes, sizeof(bytes));
  fields(expected + 16, 6, event_status >= 0 ? child : 0,
         event_status >= 0 ? event_status : 0);
  struct result r = raw_waitid(P_PID, child, bytes + 16, options, NULL);
  printf("{\"type\":\"check\",\"name\":\"%s\",\"pid\":%d,\"rc\":%ld,"
         "\"errno\":%d,\"options\":\"%#lx\",\"arena\":\"",
         name, child, r.value, r.error, options);
  hex(bytes, sizeof(bytes));
  puts("\"}");
  fflush(stdout);
  require(r.value == (error ? -1 : 0) && r.error == error, name);
  require(memcmp(bytes, expected, sizeof(bytes)) == 0,
          "six fields and all stack arena guards/padding");
}

static pid_t terminal_child(int status) {
  pid_t child = fork();
  if (child == 0) _exit(status);
  track(child);
  info_check("terminal-peek", child, WEXITED | WNOWAIT, 0, status);
  return child;
}

struct arena {
  unsigned char *memory, *before, *expected;
  size_t length;
};

static struct arena arena_new(void) {
  struct arena a = {.length = 2 * page_size};
  a.memory = mmap(NULL, a.length, PROT_READ | PROT_WRITE,
                  MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  require(a.memory != MAP_FAILED, "map actual caller arena");
  a.before = malloc(a.length);
  a.expected = malloc(a.length);
  require(a.before && a.expected, "allocate exact arena snapshots");
  memset(a.memory, 0xa5, a.length);
  memcpy(a.before, a.memory, a.length);
  memcpy(a.expected, a.memory, a.length);
  return a;
}

static void arena_protect(struct arena *a, size_t cut, int protection) {
  if (protection != (PROT_READ | PROT_WRITE))
    require(mprotect(a->memory + (cut ? page_size : 0), page_size,
                     protection) == 0, "protect actual output page");
}

static void arena_restore(struct arena *a) {
  require(mprotect(a->memory, a->length, PROT_READ | PROT_WRITE) == 0,
          "restore arena only after waitid returns");
}

static void arena_drop(struct arena *a) {
  require(munmap(a->memory, a->length) == 0, "unmap output arena");
  free(a->before);
  free(a->expected);
}

#define RW (PROT_READ | PROT_WRITE)
struct event_case {
  const char *name;
  size_t info_cut, usage_cut, info_fields, usage_prefix;
  int info_protection, usage_protection, null_info, null_usage;
  int error, nowait, alias, all_children;
  unsigned long high_options;
};

static const struct event_case event_cases[] = {
    {.name="writable", .info_fields=6, .usage_prefix=144,
     .info_protection=RW, .usage_protection=RW},
    {.name="null-info", .usage_prefix=144, .null_info=1,
     .info_protection=RW, .usage_protection=RW},
    {.name="null-usage", .info_fields=6, .null_usage=1,
     .info_protection=RW, .usage_protection=RW},
    {.name="both-null", .null_info=1, .null_usage=1,
     .info_protection=RW, .usage_protection=RW},
    {.name="protected-unused-tail", .info_cut=28, .info_fields=6,
     .usage_prefix=144, .info_protection=PROT_NONE, .usage_protection=RW},
    {.name="read-only-info", .usage_prefix=144, .error=EFAULT,
     .info_protection=PROT_READ, .usage_protection=RW},
    {.name="inaccessible-info", .usage_prefix=144, .error=EFAULT,
     .info_protection=PROT_NONE, .usage_protection=RW},
    {.name="read-only-usage", .error=EFAULT,
     .info_protection=RW, .usage_protection=PROT_READ},
    {.name="inaccessible-usage", .error=EFAULT,
     .info_protection=RW, .usage_protection=PROT_NONE},
    {.name="usage-prefix-1", .usage_cut=1, .usage_prefix=1, .error=EFAULT,
     .info_protection=RW, .usage_protection=PROT_READ},
    {.name="usage-prefix-8", .usage_cut=8, .usage_prefix=8, .error=EFAULT,
     .info_protection=RW, .usage_protection=PROT_NONE},
    {.name="usage-prefix-64", .usage_cut=64, .usage_prefix=64, .error=EFAULT,
     .info_protection=RW, .usage_protection=PROT_READ},
    {.name="first-scalar-split", .info_cut=1, .usage_prefix=144,
     .error=EFAULT, .info_protection=PROT_READ, .usage_protection=RW},
    {.name="pid-scalar-split", .info_cut=18, .info_fields=3,
     .usage_prefix=144, .error=EFAULT,
     .info_protection=PROT_NONE, .usage_protection=RW},
    {.name="status-scalar-split", .info_cut=27, .info_fields=5,
     .usage_prefix=144, .error=EFAULT,
     .info_protection=PROT_READ, .usage_protection=RW},
    {.name="aliased-outputs", .info_fields=6, .usage_prefix=144, .alias=1,
     .info_protection=RW, .usage_protection=RW},
    {.name="wnowait-writable", .info_fields=6, .usage_prefix=144, .nowait=1,
     .info_protection=RW, .usage_protection=RW},
    {.name="wnowait-info-fault", .usage_prefix=144, .error=EFAULT, .nowait=1,
     .info_protection=PROT_READ, .usage_protection=RW},
    {.name="wnowait-usage-fault", .usage_cut=8, .usage_prefix=8,
     .error=EFAULT, .nowait=1,
     .info_protection=RW, .usage_protection=PROT_NONE},
    {.name="high-option-register-bits", .info_fields=6, .usage_prefix=144,
     .high_options=1UL << 32, .info_protection=RW, .usage_protection=RW},
    {.name="p-all-ignores-id", .info_fields=6, .usage_prefix=144,
     .all_children=1, .info_protection=RW, .usage_protection=RW},
};

static void event_call(const struct event_case *c, pid_t child, int status) {
  struct arena i = arena_new(), u = arena_new();
  size_t io = c->info_cut ? page_size - c->info_cut : 64;
  size_t uo = c->usage_cut ? page_size - c->usage_cut : 64;
  struct arena *usage_arena = c->alias ? &i : &u;
  if (c->alias) io = 80;
  memset(usage_arena->expected + uo, 0, c->usage_prefix);
  fields(i.expected + io, c->info_fields, child, status);
  arena_protect(&i, c->info_cut, c->info_protection);
  arena_protect(&u, c->usage_cut, c->usage_protection);
  unsigned long options = WEXITED | c->high_options | (c->nowait ? WNOWAIT : 0);
  struct result r = raw_waitid(c->all_children ? P_ALL : P_PID,
                               c->all_children ? -1 : child,
                               c->null_info ? NULL : i.memory + io, options,
                               c->null_usage ? NULL : usage_arena->memory + uo);
  arena_restore(&i);
  arena_restore(&u);
  printf("{\"type\":\"case\",\"name\":\"%s\",\"pid\":%d,\"rc\":%ld,"
         "\"errno\":%d,\"info_offset\":%zu,\"usage_offset\":%zu,"
         "\"options\":\"%#lx\",\"alias\":%d,\"info_protection\":%d,"
         "\"usage_protection\":%d,\"null_info\":%d,\"null_usage\":%d,"
         "\"info_before\":\"", c->name, child, r.value, r.error, io, uo,
         options, c->alias, c->info_protection, c->usage_protection,
         c->null_info, c->null_usage);
  hex(i.before, i.length);
  printf("\",\"info_after\":\""); hex(i.memory, i.length);
  printf("\",\"aux_before\":\""); hex(u.before, u.length);
  printf("\",\"aux_after\":\""); hex(u.memory, u.length);
  puts("\"}"); fflush(stdout);
  require(r.value == (c->error ? -1 : 0) && r.error == c->error,
          "exact event return and errno");
  require(memcmp(i.memory, i.expected, i.length) == 0,
          "full info arena: ordered scalar stores, padding, tail and canaries");
  require(memcmp(u.memory, u.expected, u.length) == 0,
          "full usage arena: exact zero prefix and untouched suffix/canaries");
  arena_drop(&i); arena_drop(&u); ++cases;
}

static void terminal_cases(void) {
  for (size_t n = 0; n < sizeof(event_cases) / sizeof(event_cases[0]); ++n) {
    const struct event_case *c = &event_cases[n];
    int status = 31 + (int)n;
    pid_t child = terminal_child(status);
    event_call(c, child, status);
    if (c->nowait) {
      /* A protected-info EFAULT alone could hide ECHILD. A successful NULL
       * WNOWAIT followed by the exact event proves the child was retained. */
      struct result r = raw_waitid(P_PID, child, NULL, WEXITED | WNOWAIT, NULL);
      printf("{\"type\":\"retained\",\"pid\":%d,\"rc\":%ld,\"errno\":%d}\n",
             child, r.value, r.error);
      require(r.value == 0 && r.error == 0, "WNOWAIT fault retained child");
      info_check("retained-reap", child, WEXITED, 0, status);
    }
    info_check("consumed-ECHILD", child, WEXITED | WNOHANG, ECHILD, -1);
    forget(child);
  }
  pid_t first = terminal_child(61), sibling = terminal_child(62);
  struct event_case c = {.name="fault-preserves-sibling", .usage_prefix=144,
                         .error=EFAULT, .info_protection=PROT_READ,
                         .usage_protection=RW};
  event_call(&c, first, 61);
  info_check("selected-ECHILD", first, WEXITED | WNOHANG, ECHILD, -1);
  forget(first);
  info_check("exact-sibling-retained", sibling, WEXITED | WNOWAIT, 0, 62);
  info_check("exact-sibling-reaped", sibling, WEXITED, 0, 62);
  info_check("sibling-ECHILD", sibling, WEXITED | WNOHANG, ECHILD, -1);
  forget(sibling);
}

static void alarm_handler(int signal) {
  if (signal != SIGALRM) _exit(91);
  ++alarms;
}

static void error_call(const char *name, int which, long id,
                       unsigned long options, int info_mode,
                       int usage_protection, int error, int interrupted) {
  struct arena i = arena_new(), u = arena_new();
  if (info_mode == 0) fields(i.expected + 64, 6, 0, 0);
  if (info_mode == 2) arena_protect(&i, 0, PROT_READ);
  arena_protect(&u, 0, usage_protection);
  if (interrupted) {
    struct itimerval timer = {.it_value = {.tv_usec = 50000}};
    require(setitimer(ITIMER_REAL, &timer, NULL) == 0, "arm interrupt timer");
  }
  struct result r = raw_waitid(which, id, info_mode == 1 ? NULL : i.memory + 64,
                               options, u.memory + 64);
  if (interrupted) {
    struct itimerval timer = {0};
    require(setitimer(ITIMER_REAL, &timer, NULL) == 0, "disarm interrupt timer");
    require(alarms == 1, "one actual non-restarting wait interruption");
  }
  arena_restore(&i); arena_restore(&u);
  printf("{\"type\":\"case\",\"name\":\"%s\",\"which\":%d,\"id\":%ld,"
         "\"rc\":%ld,\"errno\":%d,\"options\":\"%#lx\",\"alarms\":%d,"
         "\"info_mode\":%d,\"usage_protection\":%d,\"info_before\":\"",
         name, which, id, r.value, r.error, options, alarms,
         info_mode, usage_protection);
  hex(i.before, i.length);
  printf("\",\"info_after\":\""); hex(i.memory, i.length);
  printf("\",\"aux_before\":\""); hex(u.before, u.length);
  printf("\",\"aux_after\":\""); hex(u.memory, u.length);
  puts("\"}"); fflush(stdout);
  require(r.value == (error ? -1 : 0) && r.error == error,
          "error/no-event errno including copyout precedence");
  require(memcmp(i.memory, i.expected, i.length) == 0,
          "error/no-event writes only the six zero fields");
  require(memcmp(u.memory, u.expected, u.length) == 0,
          "no positive event must not write rusage");
  arena_drop(&i); arena_drop(&u); ++cases;
}

/* This handler must observe interruption copyout before it makes a positive
 * child event possible. An early timer sees A5 instead of zeros and fails the
 * later snapshot assertion; no sleep, retry or favorable rerun hides it.
 * Setup-only signal blocking ends before the timer is armed. */
static void restart_alarm_handler(int signal) {
  int saved_errno = errno;
  if (signal != SIGALRM) _exit(91);
  ++alarms;
  unsigned char *info = atomic_load_explicit(&restart_info, memory_order_relaxed);
  sig_atomic_t length = restart_length;
  if (info == NULL || length <= 0 || length > 2 * 65536) _exit(92);
  for (sig_atomic_t n = 0; n < length; ++n)
    restart_snapshot[n] = ((volatile unsigned char *)info)[n];
  restart_seen = 1;
  errno = 0;
  restart_write_result = (sig_atomic_t)write(restart_pipe, "r", 1);
  restart_write_errno = errno;
  if (restart_write_result != 1) _exit(94);
  errno = saved_errno;
}

static pid_t interrupt_child(int pipefd[2], int status) {
  require(pipe(pipefd) == 0, "fresh interrupt-child pipe");
  pid_t child = fork();
  if (child == 0) {
    close(pipefd[1]);
    char byte;
    if (read(pipefd[0], &byte, 1) != 1) _exit(93);
    close(pipefd[0]); _exit(status);
  }
  track(child);
  require(close(pipefd[0]) == 0, "close interrupt-child parent read end");
  return child;
}

static void block_alarm_for_setup(sigset_t *previous) {
  sigset_t blocked;
  sigemptyset(&blocked);
  sigaddset(&blocked, SIGALRM);
  require(sigprocmask(SIG_BLOCK, &blocked, previous) == 0,
          "block alarm only while initializing handler state");
  require(sigismember(previous, SIGALRM) == 0,
          "the tested wait must restore an unblocked SIGALRM");
}

static void writable_interrupted_wait(pid_t child) {
  sigset_t previous_mask;
  block_alarm_for_setup(&previous_mask);
  struct arena i = arena_new(), u = arena_new();
  fields(i.expected + 64, 6, 0, 0);
  alarms = 0;
  struct sigaction action = {.sa_handler = alarm_handler}, previous;
  sigemptyset(&action.sa_mask);
  require(sigaction(SIGALRM, &action, &previous) == 0,
          "install fresh non-SA_RESTART handler");
  require(sigprocmask(SIG_SETMASK, &previous_mask, NULL) == 0,
          "restore unblocked signal before arming interrupt");
  struct itimerval timer = {.it_value = {.tv_usec = 50000}};
  require(setitimer(ITIMER_REAL, &timer, NULL) == 0, "arm writable interrupt timer");
  struct result r = raw_waitid(P_PID, child, i.memory + 64, WEXITED, u.memory + 64);
  timer = (struct itimerval){0};
  require(setitimer(ITIMER_REAL, &timer, NULL) == 0, "disarm writable interrupt timer");
  require(sigaction(SIGALRM, &previous, NULL) == 0, "restore non-restarting handler");
  printf("{\"type\":\"case\",\"name\":\"interrupted-writable-info\","
         "\"which\":%d,\"id\":%d,\"rc\":%ld,\"errno\":%d,"
         "\"options\":\"%#x\",\"alarms\":%d,\"info_mode\":0,"
         "\"usage_protection\":%d,\"info_before\":\"",
         P_PID, child, r.value, r.error, WEXITED, alarms, RW);
  hex(i.before, i.length);
  printf("\",\"info_after\":\""); hex(i.memory, i.length);
  printf("\",\"aux_before\":\""); hex(u.before, u.length);
  printf("\",\"aux_after\":\""); hex(u.memory, u.length);
  puts("\"}"); fflush(stdout);
  require(alarms == 1 && r.value == -1 && r.error == EINTR,
          "one writable-info interruption returns exact EINTR");
  require(memcmp(i.memory, i.expected, i.length) == 0,
          "EINTR writes six zeros and preserves the full A5 arena");
  require(memcmp(u.memory, u.expected, u.length) == 0,
          "EINTR leaves every rusage byte and guard untouched");
  arena_drop(&i); arena_drop(&u); ++cases;
}

static void restarted_wait(pid_t child, int pipefd, int status) {
  sigset_t previous_mask;
  block_alarm_for_setup(&previous_mask);
  struct arena i = arena_new(), u = arena_new();
  fields(i.expected + 64, 6, 0, 0);
  unsigned char *snapshot = malloc(i.length);
  require(snapshot != NULL, "allocate complete handler snapshot");
  atomic_store_explicit(&restart_info, i.memory, memory_order_relaxed);
  restart_length = (sig_atomic_t)i.length;
  restart_pipe = pipefd;
  restart_seen = 0;
  restart_write_result = -2;
  restart_write_errno = 0;
  for (size_t n = 0; n < i.length; ++n) restart_snapshot[n] = -1;
  alarms = 0;
  struct sigaction action = {.sa_handler = restart_alarm_handler,
                            .sa_flags = SA_RESTART}, previous;
  sigemptyset(&action.sa_mask);
  require(sigaction(SIGALRM, &action, &previous) == 0, "SA_RESTART handler");
  require(sigprocmask(SIG_SETMASK, &previous_mask, NULL) == 0,
          "restore unblocked signal before arming restart");
  struct itimerval timer = {.it_value = {.tv_usec = 50000}};
  require(setitimer(ITIMER_REAL, &timer, NULL) == 0, "arm restart timer");
  /* Exactly one application call: only the kernel/backend may restart it.
   * NULL rusage keeps this errors-mode case valid on native Linux as well. */
  unsigned wait_calls = 0;
  ++wait_calls;
  struct result r = raw_waitid(P_PID, child, i.memory + 64, WEXITED, NULL);
  timer = (struct itimerval){0};
  require(setitimer(ITIMER_REAL, &timer, NULL) == 0, "disarm restart timer");
  require(sigaction(SIGALRM, &previous, NULL) == 0, "restore prior alarm handler");
  for (size_t n = 0; n < i.length; ++n) {
    require(restart_snapshot[n] >= 0 && restart_snapshot[n] <= 255,
            "handler copied every arena byte");
    snapshot[n] = (unsigned char)restart_snapshot[n];
  }
  printf("{\"type\":\"case\",\"name\":\"restarted-writable-info\","
         "\"which\":%d,\"id\":%d,\"rc\":%ld,\"errno\":%d,"
         "\"options\":\"%#x\",\"alarms\":%d,\"info_mode\":0,"
         "\"usage_protection\":%d,\"null_usage\":1,\"info_offset\":64,"
         "\"sa_restart\":true,\"wait_calls\":%u,\"handler_seen\":%d,"
         "\"release_rc\":%d,\"release_errno\":%d,\"handler_info\":\"",
         P_PID, child, r.value, r.error, WEXITED, alarms, RW, wait_calls,
         restart_seen, restart_write_result, restart_write_errno);
  hex(snapshot, i.length);
  printf("\",\"info_before\":\""); hex(i.before, i.length);
  printf("\",\"info_after\":\""); hex(i.memory, i.length);
  printf("\",\"aux_before\":\""); hex(u.before, u.length);
  printf("\",\"aux_after\":\""); hex(u.memory, u.length);
  puts("\"}"); fflush(stdout);
  require(alarms == 1 && restart_seen == 1, "one actual restarting interruption");
  require(wait_calls == 1 && restart_write_result == 1 && restart_write_errno == 0,
          "one wait and one exact handler release write");
  require(memcmp(snapshot, i.expected, i.length) == 0,
          "six zero fields and all A5 guards before handler releases child");
  require(r.value == 0 && r.error == 0, "SA_RESTART returns the positive event");
  fields(i.expected + 64, 6, child, status);
  require(memcmp(i.memory, i.expected, i.length) == 0,
          "restarted exact-child event preserves all padding and guards");
  require(memcmp(u.memory, u.expected, u.length) == 0,
          "NULL restart rusage leaves auxiliary arena untouched");
  atomic_store_explicit(&restart_info, NULL, memory_order_relaxed);
  free(snapshot); arena_drop(&i); arena_drop(&u); ++cases;
}

static void additional_interrupt_cases(void) {
  int pipefd[2];
  pid_t child = interrupt_child(pipefd, 73);
  writable_interrupted_wait(child);
  info_check("interrupted-child-live", child, WEXITED | WNOHANG, 0, -1);
  require(write(pipefd[1], "x", 1) == 1, "release interrupted live child");
  require(close(pipefd[1]) == 0, "close interrupted-child release pipe");
  info_check("interrupted-child-reap", child, WEXITED, 0, 73);
  info_check("interrupted-child-ECHILD", child, WEXITED | WNOHANG, ECHILD, -1);
  forget(child);

  child = interrupt_child(pipefd, 74);
  restarted_wait(child, pipefd[1], 74);
  require(close(pipefd[1]) == 0, "close restarted-child release pipe");
  info_check("restarted-child-ECHILD", child, WEXITED | WNOHANG, ECHILD, -1);
  forget(child);
}

/* Same-process ignored signal: one wait, one send, one exact child. Only
 * atomics cross the thread boundary before join. The sibling owns its three
 * descriptors because pthreads share the descriptor table. Yield counts are
 * finite opportunities; the Rust harness also requires the actual park,
 * signal grant, re-park and release ordering in BOTH retained INFO logs. */
_Static_assert(ATOMIC_INT_LOCK_FREE == 2, "lock-free sibling protocol");
struct ignored_sibling {
  _Atomic int returned, release_started;
  pid_t tgid, waiter_tid, sender_tid;
  int start_fd, release_fd, ack_fd;
  const unsigned char *info, *aux;
  unsigned char *info_snapshot, *aux_snapshot;
  size_t length;
  int failure, pre_returned, wait_release_seen, send_calls;
  struct result start, send, release, ack;
};

static struct result byte_read(int fd, char *byte) {
  errno = 0;
  long value = read(fd, byte, 1);
  int error = errno;
  return (struct result){value, error};
}

static struct result byte_write(int fd, char byte) {
  errno = 0;
  long value = write(fd, &byte, 1);
  int error = errno;
  return (struct result){value, error};
}

static void sibling_close(int *fd, int *failure) {
  if (*fd < 0) return;
  int owned = *fd;
  *fd = -1;
  if (close(owned) != 0) *failure = 1; /* Do not retry close after EINTR. */
}

static void *ignored_sender(void *opaque) {
  struct ignored_sibling *s = opaque;
  s->sender_tid = (pid_t)syscall(SYS_gettid);
  char byte = 0;
  s->start = byte_read(s->start_fd, &byte);
  sibling_close(&s->start_fd, &s->failure);
  if (s->start.value == 1 && byte == 's') {
    for (int n = 0; n < 4; ++n)
      if (sched_yield() != 0) s->failure = 1;
    ++s->send_calls;
    errno = 0;
    long value = syscall(SYS_tgkill, s->tgid, s->waiter_tid, SIGURG);
    int error = errno;
    s->send = (struct result){value, error};
    if (value != 0 || error != 0) s->failure = 1;
    for (int n = 0; n < 4; ++n)
      if (sched_yield() != 0) s->failure = 1;
  } else {
    s->failure = 1;
  }
  s->pre_returned = atomic_load_explicit(&s->returned, memory_order_seq_cst);
  /* Info is read-only and usage is NULL: the correct kernel cannot write
   * either arena here. Record every byte, not just the six scalar fields. */
  memcpy(s->info_snapshot, s->info, s->length);
  memcpy(s->aux_snapshot, s->aux, s->length);
  if (s->pre_returned != 0) s->failure = 1;
  for (size_t n = 0; n < s->length; ++n)
    if (s->info_snapshot[n] != 0xa5 || s->aux_snapshot[n] != 0xa5)
      s->failure = 1;
  /* Failure still releases/closes this owned child path, then main fails.
   * A pre-release assertion must not strand a correct blocking waiter. */
  atomic_store_explicit(&s->release_started, 1, memory_order_seq_cst);
  s->release = byte_write(s->release_fd, 'r');
  if (s->release.value != 1 || s->release.error != 0) s->failure = 1;
  sibling_close(&s->release_fd, &s->failure);
  byte = 0;
  s->ack = byte_read(s->ack_fd, &byte);
  if (s->ack.value != 1 || s->ack.error != 0 || byte != 'a') s->failure = 1;
  sibling_close(&s->ack_fd, &s->failure);
  return NULL;
}

static void ignored_sibling_case(void) {
  struct arena i = arena_new(), u = arena_new();
  unsigned char *is = malloc(i.length), *us = malloc(u.length);
  int start[2] = {-1, -1}, release[2] = {-1, -1}, ack[2] = {-1, -1};
  int failure = 0, have_action = 0, have_mask = 0, protected = 0;
  int thread_started = 0, wait_calls = 0, child_consumed = 0, release_number = -1;
  pid_t child = -1;
  pthread_t thread;
  struct sigaction previous_action, action = {.sa_handler = SIG_DFL};
  sigset_t previous_mask, unblocked;
  struct result r = {-2, 0}, start_result = {-2, 0};
  struct ignored_sibling s = {.start_fd = -1, .release_fd = -1, .ack_fd = -1,
      .info = i.memory, .aux = u.memory, .info_snapshot = is, .aux_snapshot = us,
      .length = i.length, .send = {-2, 0}, .release = {-2, 0}, .ack = {-2, 0}};
  atomic_init(&s.returned, 0);
  atomic_init(&s.release_started, 0);
  if (!is || !us) { failure = 1; goto cleanup; }
  memset(is, 0, i.length); memset(us, 0, u.length);
  sigemptyset(&action.sa_mask);
  if (sigaction(SIGURG, &action, &previous_action) != 0) {
    failure = 1; goto cleanup;
  }
  have_action = 1;
  sigemptyset(&unblocked); sigaddset(&unblocked, SIGURG);
  if (pthread_sigmask(SIG_UNBLOCK, &unblocked, &previous_mask) != 0) {
    failure = 1; goto cleanup;
  }
  have_mask = 1;
  if (mprotect(i.memory, page_size, PROT_READ) != 0) {
    failure = 1; goto cleanup;
  }
  protected = 1;
  if (pipe(start) != 0 || pipe(release) != 0 || pipe(ack) != 0) {
    failure = 1; goto cleanup;
  }
  child = fork();
  if (child < 0) { failure = 1; goto cleanup; }
  if (child == 0) {
    close(start[0]); close(start[1]); close(release[1]); close(ack[0]);
    char byte = 0;
    struct result got = byte_read(release[0], &byte);
    if (got.value != 1 || got.error != 0 || byte != 'r') _exit(93);
    struct result sent = byte_write(ack[1], 'a');
    if (sent.value != 1 || sent.error != 0) _exit(94);
    close(release[0]); close(ack[1]); _exit(75);
  }
  track(child);
  sibling_close(&release[0], &failure);
  sibling_close(&ack[1], &failure);
  s.tgid = getpid(); s.waiter_tid = (pid_t)syscall(SYS_gettid);
  s.start_fd = start[0]; s.release_fd = release[1]; s.ack_fd = ack[0];
  release_number = release[1];
  /* Transfer descriptor ownership before creating the sibling. Main must
   * not close those descriptor numbers until the sibling has joined. */
  start[0] = release[1] = ack[0] = -1;
  if (pthread_create(&thread, NULL, ignored_sender, &s) != 0) {
    failure = 1; goto cleanup;
  }
  thread_started = 1;
  start_result = byte_write(start[1], 's');
  sibling_close(&start[1], &failure);
  if (start_result.value == 1 && start_result.error == 0) {
    ++wait_calls;
    r = raw_waitid(P_PID, child, i.memory + 64, WEXITED, NULL);
    s.wait_release_seen = atomic_load_explicit(&s.release_started, memory_order_seq_cst);
    atomic_store_explicit(&s.returned, 1, memory_order_seq_cst);
  } else {
    failure = 1;
  }
  /* All sender-owned ordinary fields and snapshots are read only after join.
   * A failed join cannot safely reclaim the live argument/arena; fail the
   * process visibly and let the existing whole-group bound retire it. */
  if (pthread_join(thread, NULL) != 0) {
    fputs("waitid-copyout assertion failed: ignored sibling join\n", stderr);
    _exit(95);
  }
  thread_started = 0;
  failure |= s.failure;
  unsigned char check[160], expected[160];
  memset(check, 0xa5, sizeof(check)); memcpy(expected, check, sizeof(check));
  fields(expected + 16, 6, 0, 0);
  struct result follow = raw_waitid(P_PID, child, check + 16,
                                   WEXITED | WNOHANG, NULL);
  child_consumed = follow.value == -1 && follow.error == ECHILD;
  printf("{\"type\":\"case\",\"name\":\"ignored-sibling-sigurg\","
         "\"which\":%d,\"id\":%d,\"tgid\":%d,\"waiter_tid\":%d,\"sender_tid\":%d,"
         "\"rc\":%ld,\"errno\":%d,\"options\":\"%#x\",\"alarms\":%d,"
         "\"info_mode\":2,\"info_offset\":64,\"usage_protection\":%d,\"null_usage\":1,"
         "\"signal\":%d,\"signal_default\":true,\"wait_calls\":%d,\"send_calls\":%d,"
         "\"send_rc\":%ld,\"send_errno\":%d,\"pre_returned\":%d,\"release_seen\":%d,"
         "\"release_fd\":%d,\"release_rc\":%ld,\"release_errno\":%d,"
         "\"ack_rc\":%ld,\"ack_errno\":%d,\"sender_failure\":%d,\"pre_info\":\"",
         P_PID, child, s.tgid, s.waiter_tid, s.sender_tid, r.value, r.error,
         WEXITED, alarms, RW, SIGURG, wait_calls, s.send_calls,
         s.send.value, s.send.error, s.pre_returned, s.wait_release_seen,
         /* The saved number remains a trace identity after sender close. */
         release_number, s.release.value, s.release.error,
         s.ack.value, s.ack.error, s.failure);
  hex(is, i.length); printf("\",\"pre_aux\":\""); hex(us, u.length);
  printf("\",\"info_before\":\""); hex(i.before, i.length);
  printf("\",\"info_after\":\""); hex(i.memory, i.length);
  printf("\",\"aux_before\":\""); hex(u.before, u.length);
  printf("\",\"aux_after\":\""); hex(u.memory, u.length); puts("\"}");
  printf("{\"type\":\"check\",\"name\":\"ignored-child-ECHILD\",\"pid\":%d,"
         "\"rc\":%ld,\"errno\":%d,\"options\":\"%#x\",\"arena\":\"",
         child, follow.value, follow.error, WEXITED | WNOHANG);
  hex(check, sizeof(check)); puts("\"}"); fflush(stdout);
  failure |= !(r.value == -1 && r.error == EFAULT && wait_calls == 1 &&
      s.send_calls == 1 && s.send.value == 0 && s.send.error == 0 &&
      s.sender_tid > 0 && s.sender_tid != s.waiter_tid && s.pre_returned == 0 &&
      s.wait_release_seen == 1 && child_consumed &&
      memcmp(check, expected, sizeof(check)) == 0 &&
      memcmp(is, i.before, i.length) == 0 && memcmp(us, u.before, u.length) == 0 &&
      memcmp(i.memory, i.before, i.length) == 0 &&
      memcmp(u.memory, u.before, u.length) == 0);
cleanup:
  /* No running sibling may outlive its stack argument on a returning path. */
  if (thread_started) _exit(96);
  sibling_close(&s.start_fd, &failure);
  sibling_close(&s.release_fd, &failure);
  sibling_close(&s.ack_fd, &failure);
  for (int n = 0; n < 2; ++n) {
    sibling_close(&start[n], &failure);
    sibling_close(&release[n], &failure);
    sibling_close(&ack[n], &failure);
  }
  if (child > 0) {
    if (!child_consumed) {
      int status;
      errno = 0;
      pid_t retired = waitpid(child, &status, 0);
      int error = errno;
      child_consumed = retired == child || (retired == -1 && error == ECHILD);
      if (!child_consumed) failure = 1;
    }
    if (child_consumed) forget(child);
  }
  if (protected && mprotect(i.memory, i.length, RW) != 0) failure = 1;
  if (have_mask && pthread_sigmask(SIG_SETMASK, &previous_mask, NULL) != 0) failure = 1;
  if (have_action && sigaction(SIGURG, &previous_action, NULL) != 0) failure = 1;
  free(is); free(us); arena_drop(&i); arena_drop(&u);
  require(failure == 0, "ignored sibling: exact order, arenas and consuming EFAULT");
  ++cases;
}


static void error_cases(void) {
  pid_t gone = terminal_child(71);
  info_check("setup-reap", gone, WEXITED, 0, 71); forget(gone);
  error_call("already-reaped", P_PID, gone, WEXITED, 0, RW, ECHILD, 0);
  int pipefd[2]; require(pipe(pipefd) == 0, "live-child pipe");
  pid_t child = fork();
  if (child == 0) {
    close(pipefd[1]);
    char byte;
    if (read(pipefd[0], &byte, 1) != 1) _exit(93);
    close(pipefd[0]); _exit(72);
  }
  track(child); require(close(pipefd[0]) == 0, "close parent read end");
  error_call("invalid-options", P_PID, child, WEXITED | 0x10000000UL,
             0, RW, EINVAL, 0);
  error_call("invalid-options-null", P_PID, child, 0, 1, RW, EINVAL, 0);
  error_call("invalid-options-protected", P_PID, child, WEXITED | 0x10000000UL,
             2, RW, EFAULT, 0);
  error_call("pid-zero", P_PID, 0, WEXITED | WNOHANG, 0, RW, EINVAL, 0);
  error_call("pid-zero-null", P_PID, 0, WEXITED | WNOHANG, 1, RW, EINVAL, 0);
  error_call("pid-zero-protected", P_PID, 0, WEXITED | WNOHANG,
             2, RW, EFAULT, 0);
  error_call("pid-negative", P_PID, -1, WEXITED | WNOHANG, 0, RW, EINVAL, 0);
  error_call("pgid-negative", P_PGID, -1, WEXITED | WNOHANG, 0, RW, EINVAL, 0);
  error_call("invalid-selector", 99, child, WEXITED | WNOHANG,
             0, PROT_NONE, EINVAL, 0);
  error_call("live-no-event", P_PID, child, WEXITED | WNOHANG, 0, RW, 0, 0);
  error_call("live-no-event-null", P_PID, child, WEXITED | WNOHANG, 1, RW, 0, 0);
  error_call("live-no-event-protected", P_PID, child, WEXITED | WNOHANG,
             2, RW, EFAULT, 0);
  error_call("live-usage-protected", P_PID, child, WEXITED | WNOHANG,
             0, PROT_READ, 0, 0);
  error_call("live-high-options", P_PID, child, WEXITED | WNOHANG | (1UL << 32),
             0, RW, 0, 0);
  error_call("live-p-all-ignores-id", P_ALL, -1, WEXITED | WNOHANG,
             0, RW, 0, 0);
  struct sigaction action = {.sa_handler = alarm_handler};
  sigemptyset(&action.sa_mask);
  require(sigaction(SIGALRM, &action, NULL) == 0, "non-SA_RESTART handler");
  sigset_t unblocked;
  sigemptyset(&unblocked);
  sigaddset(&unblocked, SIGALRM);
  require(sigprocmask(SIG_UNBLOCK, &unblocked, NULL) == 0,
          "unblock the process timer signal");
  error_call("interrupted-protected-info", P_PID, child, WEXITED,
             2, RW, EFAULT, 1);
  require(write(pipefd[1], "x", 1) == 1, "release exact live child");
  require(close(pipefd[1]) == 0, "close release pipe");
  info_check("live-child-reap", child, WEXITED, 0, 72);
  info_check("live-child-ECHILD", child, WEXITED | WNOHANG, ECHILD, -1);
  forget(child);
  additional_interrupt_cases();
  ignored_sibling_case();
}

int main(int argc, char **argv) {
  static char stdout_buffer[65536];
  require(setvbuf(stdout, stdout_buffer, _IOFBF, sizeof(stdout_buffer)) == 0,
          "bounded buffered stdout");
  require(argc == 2, "one explicit fixture mode");
  long page = sysconf(_SC_PAGESIZE);
  require(page > 128 && page <= 65536, "finite Linux page size");
  page_size = (size_t)page;
  require(atexit(cleanup_children) == 0, "install child cleanup");
  if (strcmp(argv[1], "terminal") == 0) terminal_cases();
  else if (strcmp(argv[1], "errors") == 0) error_cases();
  else require(0, "known fixture mode");
  for (size_t i = 0; i < child_count; ++i)
    require(children[i] == 0, "every exact child retired");
  printf("{\"type\":\"summary\",\"mode\":\"%s\",\"cases\":%zu,"
         "\"children\":%zu,\"assertions\":%zu,\"passed\":true}\n",
         argv[1], cases, child_count, assertions);
  require(fflush(stdout) == 0, "retain complete output");
  return 0;
}
