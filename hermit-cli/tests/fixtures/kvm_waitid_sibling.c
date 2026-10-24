/* Copyright (c) Meta Platforms, Inc. and affiliates.
 * Licensed under the BSD-style license in the LICENSE file. */
#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <signal.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/resource.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

/* Real serial-KVM Detcore coverage, including logical __WNOTHREAD selection. */
_Static_assert(sizeof(siginfo_t) == 128, "x86-64 siginfo ABI");
_Static_assert(sizeof(struct rusage) == 144, "x86-64 rusage ABI");
_Static_assert(offsetof(siginfo_t, si_signo) == 0, "signo offset");
_Static_assert(offsetof(siginfo_t, si_errno) == 4, "errno offset");
_Static_assert(offsetof(siginfo_t, si_code) == 8, "code offset");
_Static_assert(offsetof(siginfo_t, si_pid) == 16, "pid offset");
_Static_assert(offsetof(siginfo_t, si_uid) == 20, "uid offset");
_Static_assert(offsetof(siginfo_t, si_status) == 24, "status offset");
enum { INFO_BYTES = 160, INFO_OFFSET = 16, CHILD_STATUS = 73 };
static const size_t field_offsets[] = {0, 4, 8, 16, 20, 24};

struct context {
  int reverse, all, fault;
  pid_t process, leader, initial_child;
  uid_t uid;
  int ready[2], release[2];
  pid_t worker_tid; /* Written by worker, read only after pthread_join. */
  unsigned worker_calls;
};

static void fail(const char *what) {
  fprintf(stderr, "cross-thread waitid failed: %s\n", what);
  /* Abort all sibling threads on failure; never strand a pipe waiter. */
  syscall(SYS_exit_group, 90);
  __builtin_unreachable();
}

static void require(int condition, const char *what) {
  if (!condition) fail(what);
}

static void send_bytes(int fd, const void *bytes, size_t size) {
  require(write(fd, bytes, size) == (ssize_t)size, "complete pipe write");
}

static void receive_bytes(int fd, void *bytes, size_t size) {
  require(read(fd, bytes, size) == (ssize_t)size, "complete pipe read");
}

static void hex_bytes(const unsigned char *bytes, size_t length) {
  static const char digits[] = "0123456789abcdef";
  for (size_t i = 0; i < length; ++i) {
    putchar(digits[bytes[i] >> 4]);
    putchar(digits[bytes[i] & 15]);
  }
}

/* event: 1 = six child fields; 0 = six zero fields; -1 = no write.
 * Whole-arena equality proves values/footprint, not temporal store order. */
static void wait_call(const struct context *c, pid_t child, int all,
                      unsigned long options, void *usage, int error,
                      int event, unsigned *calls, const char *name) {
  unsigned char actual[INFO_BYTES], expected[INFO_BYTES], before[INFO_BYTES];
  memset(actual, 0xa5, sizeof(actual));
  memset(expected, 0xa5, sizeof(expected));
  memset(before, 0xa5, sizeof(before));
  if (event >= 0) {
    uint32_t fields[] = {event ? SIGCHLD : 0, 0, event ? CLD_EXITED : 0,
                        event ? (uint32_t)child : 0,
                        event ? (uint32_t)c->uid : 0,
                        event ? CHILD_STATUS : 0};
    for (size_t i = 0; i < sizeof(field_offsets) / sizeof(field_offsets[0]); ++i)
      memcpy(expected + INFO_OFFSET + field_offsets[i], &fields[i], 4);
  }
  errno = 0;
  long result = syscall(SYS_waitid, all ? P_ALL : P_PID, all ? 0 : child,
                        actual + INFO_OFFSET, options, usage);
  int saved_errno = errno;
  ++*calls;
  printf("{\"type\":\"wait\",\"name\":\"%s\",\"child\":%d,"
         "\"tid\":%ld,\"uid\":%u,\"which\":%d,\"options\":%lu,"
         "\"rc\":%ld,\"errno\":%d,\"before\":\"", name, child,
         syscall(SYS_gettid), (unsigned)c->uid, (int)(all ? P_ALL : P_PID), options,
         result, saved_errno);
  hex_bytes(before, sizeof(before));
  printf("\",\"after\":\"");
  hex_bytes(actual, sizeof(actual));
  puts("\"}");
  require(fflush(stdout) == 0, "complete wait arena output");
  if (result != (error ? -1 : 0) || saved_errno != error) {
    fprintf(stderr, "%s: result=%ld errno=%d expected=%d\n", name, result,
            saved_errno, error);
    fail("raw wait result");
  }
  require(memcmp(actual, expected, sizeof(actual)) == 0, name);
}

static pid_t new_child(void) {
  pid_t child = fork();
  require(child >= 0, "fork");
  if (!child) _exit(CHILD_STATUS);
  return child;
}

static void no_children(const struct context *c, pid_t child, unsigned *calls) {
  wait_call(c, child, 0, WEXITED | WNOHANG, NULL, ECHILD, 0, calls,
            "exact child consumed once");
  wait_call(c, child, 1, WEXITED | WNOHANG, NULL, ECHILD, 0, calls,
            "all children consumed once");
}

static void foreign_waits(const struct context *c, pid_t child,
                          unsigned *calls) {
  /* Detcore supports this logical owner restriction; a denied sibling
   * cannot consume the event, even when the selector is P_ALL. */
  wait_call(c, child, c->all, WEXITED | WNOWAIT | __WNOTHREAD, NULL,
            ECHILD, 0, calls, "sibling owner restriction leaves child intact");
  wait_call(c, child, c->all, WEXITED | WNOWAIT, NULL, 0, 1, calls,
            "first sibling peek values and padding");
  wait_call(c, child, c->all, WEXITED | WNOWAIT, NULL, 0, 1, calls,
            "repeated sibling peek values and padding");
  if (c->fault) {
    long page = sysconf(_SC_PAGESIZE);
    require(page >= 4096 && page <= 65536, "bounded host page size");
    unsigned char *usage = mmap(NULL, (size_t)page, PROT_READ | PROT_WRITE,
                                MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    require(usage != MAP_FAILED, "usage mmap");
    unsigned char usage_before[65536];
    memset(usage, 0xa5, (size_t)page);
    memcpy(usage_before, usage, (size_t)page);
    require(mprotect(usage, (size_t)page, PROT_NONE) == 0, "protect usage");
    wait_call(c, child, c->all, WEXITED, usage + 16, EFAULT, -1, calls,
              "usage fault precedes every info store");
    require(mprotect(usage, (size_t)page, PROT_READ | PROT_WRITE) == 0,
            "restore usage for readback");
    for (long i = 0; i < page; ++i)
      require(usage[i] == 0xa5, "protected usage and guards untouched");
    printf("{\"type\":\"usage\",\"before\":\"");
    hex_bytes(usage_before, (size_t)page);
    printf("\",\"after\":\"");
    hex_bytes(usage, (size_t)page);
    puts("\"}");
    require(fflush(stdout) == 0, "complete protected usage output");
    require(munmap(usage, (size_t)page) == 0, "usage munmap");
  } else {
    /* Native CPU accounting is variable; successful calls use NULL rusage. */
    wait_call(c, child, c->all, WEXITED, NULL, 0, 1, calls,
              "sibling consumption values and padding");
  }
  no_children(c, child, calls);
}

static void *worker(void *opaque) {
  struct context *c = opaque;
  unsigned calls = 0;
  pid_t tid = (pid_t)syscall(SYS_gettid);
  c->worker_tid = tid;
  require(getpid() == c->process && tid != c->leader, "same-process sibling");
  if (c->reverse) {
    pid_t child = new_child();
    require(child != tid && child != c->leader, "distinct worker child");
    wait_call(c, child, 0, WEXITED | WNOWAIT, NULL, 0, 1, &calls,
              "creator worker proves child waitability");
    /* Pass the PID as pipe data, not a racy shared context update. */
    send_bytes(c->ready[1], &child, sizeof(child));
    unsigned char done = 0;
    receive_bytes(c->release[0], &done, sizeof(done));
    require(done == 'd', "leader completed while creator stayed alive");
    no_children(c, child, &calls);
  } else {
    require(c->initial_child != tid, "distinct leader child");
    foreign_waits(c, c->initial_child, &calls);
  }
  c->worker_calls = calls;
  return NULL;
}

int main(int argc, char **argv) {
  if (argc != 4) fail("three exact mode arguments required");
  struct context c = {0};
  if (!strcmp(argv[1], "worker-child")) c.reverse = 1;
  else require(!strcmp(argv[1], "leader-child"), "direction");
  if (!strcmp(argv[2], "all")) c.all = 1;
  else require(!strcmp(argv[2], "pid"), "selector");
  if (!strcmp(argv[3], "efault")) c.fault = 1;
  else require(!strcmp(argv[3], "success"), "consume mode");
  c.process = getpid();
  c.leader = (pid_t)syscall(SYS_gettid);
  c.uid = getuid(); /* Validate each backend's real/virtual UID; no filtering. */
  require(c.leader == c.process, "main is process leader");
  require(pipe(c.ready) == 0 && pipe(c.release) == 0, "two synchronization pipes");
  unsigned calls = 0;
  pid_t child = 0;
  if (!c.reverse) {
    child = c.initial_child = new_child();
    wait_call(&c, child, 0, WEXITED | WNOWAIT, NULL, 0, 1, &calls,
              "creator leader proves readiness before pthread_create");
  }
  pthread_t thread;
  require(pthread_create(&thread, NULL, worker, &c) == 0, "pthread_create");
  if (c.reverse) {
    receive_bytes(c.ready[0], &child, sizeof(child));
    require(child > 0 && child != c.leader, "worker child PID transfer");
    foreign_waits(&c, child, &calls);
    const unsigned char done = 'd';
    send_bytes(c.release[1], &done, sizeof(done));
  }
  void *value = (void *)1;
  require(pthread_join(thread, &value) == 0 && value == NULL, "pthread_join");
  if (!c.reverse) no_children(&c, child, &calls);
  require(calls == (c.reverse ? 6u : 3u) &&
          c.worker_calls == (c.reverse ? 3u : 6u), "nine exact wait calls");
  /* Threads share one FD table. Close only after join, in the owner main. */
  require(close(c.ready[0]) == 0 && close(c.ready[1]) == 0 &&
          close(c.release[0]) == 0 && close(c.release[1]) == 0, "close owned pipes");
  require(printf("{\"type\":\"summary\",\"direction\":\"%s\","
                 "\"selector\":\"%s\",\"consume\":\"%s\",\"leader\":%d,"
                 "\"worker\":%d,\"child\":%d,\"calls\":9,\"passed\":true}\n",
                 argv[1], argv[2], argv[3], c.leader, c.worker_tid, child) > 0,
          "final summary write");
  require(fflush(stdout) == 0, "flush final marker");
  return 0;
}
