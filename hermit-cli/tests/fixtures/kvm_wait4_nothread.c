/* Copyright (c) Meta Platforms, Inc. and affiliates.
 * Licensed under the BSD-style license in the LICENSE file. */
#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/resource.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

/* Raw wait4 with __WNOTHREAD. Linux restricts such a wait to children that
 * the calling thread created. A sibling thread gets ECHILD whether the child
 * is live or ready and whether the call blocks or not, and it consumes
 * nothing; the creating thread still reaps the child with its exact status.
 * Every expectation below is also what native Linux returns. */
_Static_assert(sizeof(struct rusage) == 144, "x86-64 rusage ABI");
enum { PAGE = 4096, OFFSET = 64, WINDOW = 128 };
enum { DENY = 1, RELEASE = 2, CREATOR = 3 };
static unsigned assertions, calls, current_case;
static pid_t leader;

static void require(int condition, const char *what) {
  __atomic_add_fetch(&assertions, 1, __ATOMIC_SEQ_CST);
  if (!condition) {
    fprintf(stderr, "wait4 __WNOTHREAD case=%u: %s errno=%d\n",
            current_case, what, errno);
    /* Stop every thread at once; never strand a pipe waiter. */
    syscall(SYS_exit_group, 90);
    __builtin_unreachable();
  }
}

static long thread_id(void) { return syscall(SYS_gettid); }
static const char *actor(void) { return thread_id() == leader ? "main" : "worker"; }
static void flush(void) { require(fflush(stdout) == 0, "flush complete row"); }

static void hex(const unsigned char *bytes, size_t length) {
  static const char digits[] = "0123456789abcdef";
  for (size_t i = 0; i < length; ++i) {
    putchar(digits[bytes[i] >> 4]);
    putchar(digits[bytes[i] & 15]);
  }
}

/* A held child exits only after one release byte, so a wait made while it
 * is held sees a live child. A free child (release == NULL) exits at once. */
static pid_t spawn(const char *name, int status, int release[2]) {
  flush();
  pid_t child = fork();
  require(child >= 0, "fork");
  if (!child) {
    if (release) {
      char byte = 0;
      close(release[1]);
      if (read(release[0], &byte, 1) != 1 || byte != 'r') _exit(91);
    }
    for (unsigned i = 0; i < 64; ++i) syscall(SYS_getpid);
    _exit(status);
  }
  if (release) require(close(release[0]) == 0, "close release read end");
  printf("{\"type\":\"spawn\",\"case\":%u,\"name\":\"%s\",\"actor\":\"%s\","
         "\"tid\":%ld,\"child\":%d,\"status\":%d,\"held\":%d}\n",
         current_case, name, actor(), thread_id(), child, status, release != NULL);
  flush();
  return child;
}

static void release_child(int release[2]) {
  require(write(release[1], "r", 1) == 1, "release held child");
  require(close(release[1]) == 0, "close release write end");
}

/* fault 1 makes the status page read-only, fault 2 the usage page. A call
 * that must succeed passes NULL rusage, because native child CPU time
 * varies; every other call passes a writable or read-only usage page that
 * must stay intact. Whole-page equality is checked here, and the first
 * 128 bytes of each page are printed for the harness. */
static void wait4_check(const char *name, pid_t selector, int options, int fault,
                        int usage, long expected_rc, int error, int status) {
  unsigned char *status_page = mmap(NULL, PAGE, PROT_READ | PROT_WRITE,
      MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  unsigned char *usage_page = mmap(NULL, PAGE, PROT_READ | PROT_WRITE,
      MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  require(status_page != MAP_FAILED && usage_page != MAP_FAILED, "output mappings");
  memset(status_page, 0xa5, PAGE);
  memset(usage_page, 0xa5, PAGE);
  unsigned char expected_status[PAGE], expected_usage[PAGE];
  memset(expected_status, 0xa5, PAGE);
  memset(expected_usage, 0xa5, PAGE);
  if (expected_rc > 0 || fault == 2) {
    int raw_status = status << 8;
    memcpy(expected_status + OFFSET, &raw_status, sizeof(raw_status));
  }
  if (fault == 1) require(mprotect(status_page, PAGE, PROT_READ) == 0, "protect status");
  if (fault == 2) require(mprotect(usage_page, PAGE, PROT_READ) == 0, "protect usage");
  errno = 0;
  long rc = syscall(SYS_wait4, selector, status_page + OFFSET, options,
                    usage ? usage_page + OFFSET : NULL);
  int saved = errno;
  __atomic_add_fetch(&calls, 1, __ATOMIC_SEQ_CST);
  printf("{\"type\":\"wait4\",\"case\":%u,\"name\":\"%s\",\"actor\":\"%s\","
         "\"tid\":%ld,\"selector\":%d,\"options\":%d,\"fault\":%d,\"usage\":%d,"
         "\"rc\":%ld,\"errno\":%d,\"status_window\":\"",
         current_case, name, actor(), thread_id(), selector, options, fault,
         usage, rc, saved);
  hex(status_page, WINDOW);
  printf("\",\"usage_window\":\"");
  hex(usage_page, WINDOW);
  puts("\"}");
  flush();
  require(rc == expected_rc && saved == error, name);
  require(memcmp(status_page, expected_status, PAGE) == 0, "whole status page");
  require(memcmp(usage_page, expected_usage, PAGE) == 0, "whole usage page");
  require(munmap(status_page, PAGE) == 0 && munmap(usage_page, PAGE) == 0, "unmap outputs");
}

static void waitid_check(const char *name, int which, pid_t id, int options,
                         int error, pid_t event, int status) {
  siginfo_t info;
  memset(&info, 0, sizeof(info));
  errno = 0;
  long rc = syscall(SYS_waitid, which, id, &info, options, NULL);
  int saved = errno;
  __atomic_add_fetch(&calls, 1, __ATOMIC_SEQ_CST);
  printf("{\"type\":\"waitid\",\"case\":%u,\"name\":\"%s\",\"actor\":\"%s\","
         "\"tid\":%ld,\"which\":%d,\"id\":%d,\"options\":%d,\"rc\":%ld,"
         "\"errno\":%d,\"si_signo\":%d,\"si_code\":%d,\"si_pid\":%d,"
         "\"si_status\":%d}\n",
         current_case, name, actor(), thread_id(), which, id, options, rc, saved,
         info.si_signo, info.si_code, info.si_pid, info.si_status);
  flush();
  require(rc == (error ? -1 : 0) && saved == error, name);
  if (event)
    require(info.si_signo == SIGCHLD && info.si_code == CLD_EXITED &&
            info.si_pid == event && info.si_status == status, "waitid event");
  else
    require(info.si_signo == 0 && info.si_pid == 0, "no waitid event");
}

struct cpu { int64_t user, system; };
static struct cpu children_cpu(const char *name) {
  struct rusage value;
  require(getrusage(RUSAGE_CHILDREN, &value) == 0, "children CPU query");
  struct cpu result = {value.ru_utime.tv_sec * INT64_C(1000000) + value.ru_utime.tv_usec,
                       value.ru_stime.tv_sec * INT64_C(1000000) + value.ru_stime.tv_usec};
  printf("{\"type\":\"cpu\",\"case\":%u,\"name\":\"%s\",\"actor\":\"%s\","
         "\"user_us\":%ld,\"system_us\":%ld}\n", current_case, name, actor(),
         (long)result.user, (long)result.system);
  flush();
  return result;
}
static void equal_cpu(struct cpu a, struct cpu b) {
  require(a.user == b.user && a.system == b.system, "children CPU unchanged");
}
static void added_cpu(struct cpu a, struct cpu b) {
  require(b.user >= a.user && b.system >= a.system &&
          (b.user > a.user || b.system > a.system), "consuming wait adds child CPU");
}

static void case_row(const char *name, pid_t worker) {
  printf("{\"type\":\"case\",\"case\":%u,\"name\":\"%s\",\"worker\":%d,"
         "\"passed\":true}\n", current_case, name, worker);
  flush();
}

/* Five refused waits for a child some other thread created. The blocking
 * forms must return at once: the caller owns no child it could wait for. */
static void deny(pid_t child) {
  wait4_check("deny-exact-nohang", child, WNOHANG | __WNOTHREAD, 0, 1, -1, ECHILD, 0);
  wait4_check("deny-any-nohang", -1, WNOHANG | __WNOTHREAD, 0, 1, -1, ECHILD, 0);
  wait4_check("deny-exact-blocking", child, __WNOTHREAD, 0, 1, -1, ECHILD, 0);
  wait4_check("deny-any-blocking", -1, __WNOTHREAD, 0, 1, -1, ECHILD, 0);
  wait4_check("deny-any-untraced", -1, WUNTRACED | __WNOTHREAD, 0, 1, -1, ECHILD, 0);
}

struct worker {
  int mode;
  pid_t child; /* DENY: the leader's child. */
  int release[2], handoff[2], done[2];
  pid_t tid; /* Written by the worker, read only after pthread_join. */
};

static void *worker_main(void *opaque) {
  struct worker *w = opaque;
  w->tid = (pid_t)thread_id();
  require(getpid() == leader && w->tid != leader, "same-process sibling thread");
  if (w->mode == DENY) {
    deny(w->child);
  } else if (w->mode == RELEASE) {
    release_child(w->release);
  } else {
    pid_t child = spawn("c5", 45, w->release);
    /* Pass the PID as pipe data, not a racy shared update. */
    require(write(w->handoff[1], &child, sizeof(child)) == (ssize_t)sizeof(child),
            "send worker child PID");
    char byte = 0;
    require(read(w->done[0], &byte, 1) == 1 && byte == 'd', "leader finished");
    wait4_check("creator-exact-consume", child, __WNOTHREAD, 0, 0, child, 0, 45);
    wait4_check("creator-exact-echild", child, WNOHANG | __WNOTHREAD, 0, 1, -1, ECHILD, 0);
  }
  return NULL;
}

static void start_worker(pthread_t *thread, struct worker *w) {
  flush();
  require(pthread_create(thread, NULL, worker_main, w) == 0, "pthread_create");
}

static void join_worker(pthread_t thread) {
  void *value = (void *)1;
  require(pthread_join(thread, &value) == 0 && value == NULL, "pthread_join");
}

int main(void) {
  require(setvbuf(stdout, NULL, _IOLBF, 0) == 0, "line buffered observations");
  leader = (pid_t)thread_id();
  require(leader == getpid(), "main is the thread-group leader");
  pthread_t thread;
  struct worker w;

  /* Case 0: the creator's own held child is live, so WNOHANG returns 0. */
  current_case = 0;
  int c1_release[2];
  require(pipe(c1_release) == 0, "c1 release pipe");
  pid_t c1 = spawn("c1", 41, c1_release);
  wait4_check("own-exact-live", c1, WNOHANG | __WNOTHREAD, 0, 1, 0, 0, 0);
  wait4_check("own-any-live", -1, WNOHANG | __WNOTHREAD, 0, 1, 0, 0, 0);
  wait4_check("own-exact-live-untraced", c1, WNOHANG | WUNTRACED | __WNOTHREAD,
              0, 1, 0, 0, 0);
  wait4_check("own-any-live-untraced", -1, WNOHANG | WUNTRACED | __WNOTHREAD,
              0, 1, 0, 0, 0);
  struct cpu last = children_cpu("baseline");
  case_row("owned-live", 0);

  /* Case 1: a sibling thread is refused the leader's live child. */
  current_case = 1;
  w = (struct worker){.mode = DENY, .child = c1};
  start_worker(&thread, &w);
  join_worker(thread);
  equal_cpu(last, children_cpu("after-live-denial"));
  case_row("sibling-denial-live", w.tid);

  /* Case 2: the same refusals once the child is ready; it stays waitable. */
  current_case = 2;
  release_child(c1_release);
  waitid_check("c1-peek", P_PID, c1, WEXITED | WNOWAIT, 0, c1, 41);
  w = (struct worker){.mode = DENY, .child = c1};
  start_worker(&thread, &w);
  join_worker(thread);
  waitid_check("c1-peek-again", P_PID, c1, WEXITED | WNOWAIT, 0, c1, 41);
  equal_cpu(last, children_cpu("after-ready-denial"));
  case_row("sibling-denial-ready", w.tid);

  /* Case 3: the creator consumes the ready child exactly once. */
  current_case = 3;
  wait4_check("own-exact-consume", c1, __WNOTHREAD, 0, 0, c1, 0, 41);
  struct cpu now = children_cpu("after-consume");
  added_cpu(last, now);
  last = now;
  wait4_check("own-exact-echild", c1, WNOHANG | __WNOTHREAD, 0, 1, -1, ECHILD, 0);
  wait4_check("own-any-echild", -1, WNOHANG | __WNOTHREAD, 0, 1, -1, ECHILD, 0);
  waitid_check("c1-echild", P_PID, c1, WEXITED | WNOHANG, ECHILD, 0, 0);
  equal_cpu(last, children_cpu("after-echild"));
  case_row("owned-ready-completion", 0);

  /* Case 4: a blocking any-child wait by the creator completes after a
   * sibling thread releases the held child. */
  current_case = 4;
  int c2_release[2];
  require(pipe(c2_release) == 0, "c2 release pipe");
  pid_t c2 = spawn("c2", 42, c2_release);
  w = (struct worker){.mode = RELEASE, .release = {-1, c2_release[1]}};
  start_worker(&thread, &w);
  wait4_check("own-any-blocking-consume", -1, WUNTRACED | __WNOTHREAD, 0, 0, c2, 0, 42);
  join_worker(thread);
  now = children_cpu("after-consume");
  added_cpu(last, now);
  last = now;
  wait4_check("own-any-echild", -1, WNOHANG | __WNOTHREAD, 0, 1, -1, ECHILD, 0);
  equal_cpu(last, children_cpu("after-echild"));
  case_row("owned-blocking-completion", w.tid);

  /* Case 5: WNOHANG any-child consumes the creator's ready child. */
  current_case = 5;
  pid_t c3 = spawn("c3", 43, NULL);
  waitid_check("c3-peek", P_PID, c3, WEXITED | WNOWAIT, 0, c3, 43);
  wait4_check("own-any-nohang-consume", -1, WNOHANG | __WNOTHREAD, 0, 0, c3, 0, 43);
  now = children_cpu("after-consume");
  added_cpu(last, now);
  last = now;
  wait4_check("own-exact-echild", c3, WNOHANG | __WNOTHREAD, 0, 1, -1, ECHILD, 0);
  equal_cpu(last, children_cpu("after-echild"));
  case_row("owned-nohang-completion", 0);

  /* Cases 6-9: a consuming copy-out fault still reaps the child once. */
  static const char *const fault_names[] = {"fault0", "fault1", "fault2", "fault3"};
  static const struct { int fault, any, options, peek; } faults[] = {
    {1, 0, __WNOTHREAD, 0},
    {1, 1, WNOHANG | __WNOTHREAD, 1},
    {2, 0, WUNTRACED | __WNOTHREAD, 0},
    {2, 1, WNOHANG | WUNTRACED | __WNOTHREAD, 1},
  };
  for (unsigned k = 0; k < 4; ++k) {
    current_case = 6 + k;
    int status = 50 + (int)k;
    pid_t child = spawn(fault_names[k], status, NULL);
    if (faults[k].peek)
      waitid_check("fault-peek", P_PID, child, WEXITED | WNOWAIT, 0, child, status);
    equal_cpu(last, children_cpu("before-fault"));
    wait4_check("own-consuming-fault", faults[k].any ? -1 : child, faults[k].options,
                faults[k].fault, 1, -1, EFAULT, status);
    now = children_cpu("after-fault");
    added_cpu(last, now);
    last = now;
    wait4_check("own-exact-echild", child, WNOHANG | __WNOTHREAD, 0, 1, -1, ECHILD, 0);
    waitid_check("fault-echild", P_PID, child, WEXITED | WNOHANG, ECHILD, 0, 0);
    equal_cpu(last, children_cpu("after-echild"));
    case_row("owned-fault", 0);
  }

  /* Case 10: the leader is refused a sibling thread's child, live or ready,
   * and the creating thread reaps it with its exact status. */
  current_case = 10;
  w = (struct worker){.mode = CREATOR};
  require(pipe(w.release) == 0 && pipe(w.handoff) == 0 && pipe(w.done) == 0,
          "creator pipes");
  start_worker(&thread, &w);
  pid_t c5 = 0;
  require(read(w.handoff[0], &c5, sizeof(c5)) == (ssize_t)sizeof(c5), "receive worker child PID");
  require(c5 > 0 && c5 != leader, "worker child PID");
  deny(c5);
  equal_cpu(last, children_cpu("before-release"));
  release_child(w.release);
  waitid_check("c5-peek", P_PID, c5, WEXITED | WNOWAIT, 0, c5, 45);
  deny(c5);
  waitid_check("c5-peek-again", P_PID, c5, WEXITED | WNOWAIT, 0, c5, 45);
  equal_cpu(last, children_cpu("after-denials"));
  require(write(w.done[1], "d", 1) == 1, "let the creator reap");
  join_worker(thread);
  now = children_cpu("after-creator-consume");
  added_cpu(last, now);
  last = now;
  wait4_check("final-any-echild", -1, WNOHANG, 0, 1, -1, ECHILD, 0);
  waitid_check("final-all-echild", P_ALL, 0, WEXITED | WNOHANG, ECHILD, 0, 0);
  equal_cpu(last, children_cpu("after-final-echild"));
  require(close(w.handoff[0]) == 0 && close(w.handoff[1]) == 0 &&
          close(w.done[0]) == 0 && close(w.done[1]) == 0, "close creator pipes");
  case_row("reverse-sibling-denial", w.tid);

  printf("{\"type\":\"summary\",\"cases\":11,\"children\":8,\"calls\":%u,"
         "\"assertions\":%u,\"leader\":%d,\"passed\":true}\n",
         calls, assertions, leader);
  flush();
  return 0;
}
