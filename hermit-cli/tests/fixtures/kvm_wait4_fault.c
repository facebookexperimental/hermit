/* Copyright (c) Meta Platforms, Inc. and affiliates.
 * Licensed under the BSD-style license in the LICENSE file. */
#define _GNU_SOURCE
#include <errno.h>
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

_Static_assert(sizeof(siginfo_t) == 128, "x86-64 siginfo ABI");
_Static_assert(sizeof(struct rusage) == 144, "x86-64 rusage ABI");
enum { PAGE = 4096, OFFSET = 64, INFO_BYTES = 160, INFO_OFFSET = 16 };
static unsigned assertions, calls, current_case;
static const size_t fields[] = {0, 4, 8, 16, 20, 24};

static void require(int condition, const char *what) {
  ++assertions;
  if (!condition) {
    fprintf(stderr, "mixed wait4/waitid case=%u: %s errno=%d\n",
            current_case, what, errno);
    syscall(SYS_exit_group, 90);
    __builtin_unreachable();
  }
}

static void hex(const unsigned char *bytes, size_t length) {
  static const char digits[] = "0123456789abcdef";
  for (size_t i = 0; i < length; ++i) {
    putchar(digits[bytes[i] >> 4]);
    putchar(digits[bytes[i] & 15]);
  }
}

static void flush(void) { require(fflush(stdout) == 0, "flush complete row"); }

static void child_work(int status) {
  for (unsigned i = 0; i < 64; ++i) syscall(SYS_getpid);
  _exit(status);
}

static void waitid_check(const char *name, pid_t child, int all, int options,
                         int error, int event, int status) {
  unsigned char actual[INFO_BYTES], expected[INFO_BYTES];
  memset(actual, 0xa5, sizeof(actual));
  memset(expected, 0xa5, sizeof(expected));
  uint32_t values[] = {event ? SIGCHLD : 0, 0, event ? CLD_EXITED : 0,
                       event ? (uint32_t)child : 0,
                       event ? (uint32_t)getuid() : 0,
                       event ? (uint32_t)status : 0};
  for (unsigned i = 0; i < sizeof(fields) / sizeof(fields[0]); ++i)
    memcpy(expected + INFO_OFFSET + fields[i], &values[i], 4);
  errno = 0;
  long rc = syscall(SYS_waitid, all ? P_ALL : P_PID, all ? 0 : child,
                    actual + INFO_OFFSET, options, NULL);
  int saved = errno;
  ++calls;
  printf("{\"type\":\"waitid\",\"case\":%u,\"name\":\"%s\","
         "\"child\":%d,\"uid\":%u,\"which\":%d,\"options\":%d,"
         "\"status\":%d,\"event\":%d,\"rc\":%ld,\"errno\":%d,\"arena\":\"",
         current_case, name, child, (unsigned)getuid(), all ? P_ALL : P_PID,
         options, status, event, rc, saved);
  hex(actual, sizeof(actual));
  puts("\"}");
  flush();
  require(rc == (error ? -1 : 0) && saved == error, name);
  require(memcmp(actual, expected, sizeof(actual)) == 0, "whole waitid arena");
}

static void wait4_check(const char *name, pid_t selector, int options,
                        int fault, int status, long expected_rc, int error) {
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
  if (fault == 1) require(mprotect(status_page, PAGE, PROT_READ) == 0, "protect status");
  if (fault == 2) {
    require(mprotect(usage_page, PAGE, PROT_READ) == 0, "protect usage");
    int raw_status = status << 8;
    memcpy(expected_status + OFFSET, &raw_status, sizeof(raw_status));
  }
  errno = 0;
  long rc = syscall(SYS_wait4, selector, status_page + OFFSET, options,
                    usage_page + OFFSET);
  int saved = errno;
  ++calls;
  printf("{\"type\":\"wait4\",\"case\":%u,\"name\":\"%s\","
         "\"selector\":%d,\"options\":%d,\"fault\":%d,\"status\":%d,"
         "\"rc\":%ld,\"errno\":%d,\"status_arena\":\"",
         current_case, name, selector, options, fault, status, rc, saved);
  hex(status_page, PAGE);
  printf("\",\"usage_arena\":\"");
  hex(usage_page, PAGE);
  puts("\"}");
  flush();
  require(rc == expected_rc && saved == error, name);
  require(memcmp(status_page, expected_status, PAGE) == 0, "whole status arena");
  require(memcmp(usage_page, expected_usage, PAGE) == 0, "whole usage arena");
  require(munmap(status_page, PAGE) == 0 && munmap(usage_page, PAGE) == 0, "unmap outputs");
}

struct cpu { int64_t user, system; };
static struct cpu children_cpu(const char *name) {
  struct rusage value;
  require(getrusage(RUSAGE_CHILDREN, &value) == 0, "children CPU query");
  struct cpu result = {value.ru_utime.tv_sec * INT64_C(1000000) + value.ru_utime.tv_usec,
                       value.ru_stime.tv_sec * INT64_C(1000000) + value.ru_stime.tv_usec};
  printf("{\"type\":\"cpu\",\"case\":%u,\"name\":\"%s\","
         "\"user_us\":%ld,\"system_us\":%ld}\n", current_case, name,
         (long)result.user, (long)result.system);
  flush();
  return result;
}
static void equal_cpu(struct cpu a, struct cpu b) {
  require(a.user == b.user && a.system == b.system, "children CPU rolled up only once");
}
static void added_cpu(struct cpu a, struct cpu b) {
  require(b.user >= a.user && b.system >= a.system &&
          (b.user > a.user || b.system > a.system), "consuming wait adds child CPU");
}

static void run_case(int fault, int nonblock, int untraced) {
  int release[2];
  require(pipe(release) == 0, "sibling release pipe");
  pid_t sibling = fork();
  require(sibling >= 0, "fork retained sibling");
  int child_status = 31 + (int)current_case;
  int sibling_status = 73 + (int)current_case;
  if (!sibling) {
    close(release[1]);
    char byte;
    require(read(release[0], &byte, 1) == 1 && byte == 'x', "sibling causal release");
    child_work(sibling_status);
  }
  close(release[0]);
  pid_t child = fork();
  require(child >= 0, "fork fault target");
  if (!child) child_work(child_status);
  waitid_check("target-peek", child, 0, WEXITED | WNOWAIT, 0, 1, child_status);
  struct cpu before = children_cpu("before-fault");
  int options = (nonblock ? WNOHANG : 0) | (untraced ? WUNTRACED : 0);
  /* The only other child remains live. P_ALL cannot legitimately consume it. */
  wait4_check("consuming-fault", untraced ? -1 : child, options,
              fault, child_status, -1, EFAULT);
  struct cpu after = children_cpu("after-fault");
  /* Preserve the mixed-call failure as the first post-fault lifecycle oracle. */
  waitid_check("target-ECHILD", child, 0, WEXITED | WNOHANG, ECHILD, 0, 0);
  added_cpu(before, after);
  wait4_check("target-wait4-ECHILD", child, WNOHANG, 0, 0, -1, ECHILD);
  waitid_check("live-sibling-P_ALL", sibling, 1, WEXITED | WNOHANG, 0, 0, 0);
  wait4_check("live-sibling-wait4-P_ALL", -1, WNOHANG, 0, 0, 0, 0);
  equal_cpu(after, children_cpu("after-empty-waits"));
  require(write(release[1], "x", 1) == 1, "release retained sibling");
  require(close(release[1]) == 0, "close release pipe");
  waitid_check("sibling-peek", sibling, 0, WEXITED | WNOWAIT, 0, 1, sibling_status);
  waitid_check("sibling-peek-again", sibling, 0, WEXITED | WNOWAIT, 0, 1, sibling_status);
  equal_cpu(after, children_cpu("after-sibling-peeks"));
  waitid_check("sibling-consume", sibling, 0, WEXITED, 0, 1, sibling_status);
  struct cpu final = children_cpu("after-sibling-consume");
  added_cpu(after, final);
  waitid_check("sibling-ECHILD", sibling, 0, WEXITED | WNOHANG, ECHILD, 0, 0);
  waitid_check("all-ECHILD", 0, 1, WEXITED | WNOHANG, ECHILD, 0, 0);
  wait4_check("all-wait4-ECHILD", -1, WNOHANG, 0, 0, -1, ECHILD);
  equal_cpu(final, children_cpu("after-final-ECHILD"));
  printf("{\"type\":\"case\",\"case\":%u,\"fault\":%d,\"nonblock\":%d,"
         "\"untraced\":%d,\"child\":%d,\"sibling\":%d,\"passed\":true}\n",
         current_case, fault, nonblock, untraced, child, sibling);
  flush();
}

int main(void) {
  require(setvbuf(stdout, NULL, _IOLBF, 0) == 0, "line buffered observations");
  for (int fault = 1; fault <= 2; ++fault)
    for (int nonblock = 0; nonblock <= 1; ++nonblock)
      for (int untraced = 0; untraced <= 1; ++untraced) {
        run_case(fault, nonblock, untraced);
        ++current_case;
      }
  printf("{\"type\":\"summary\",\"cases\":%u,\"children\":16,"
         "\"calls\":%u,\"assertions\":%u,\"passed\":true}\n",
         current_case, calls, assertions);
  flush();
  return 0;
}
