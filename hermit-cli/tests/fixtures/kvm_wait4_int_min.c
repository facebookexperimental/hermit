/* Copyright (c) Meta Platforms, Inc. and affiliates.
 * Licensed under the BSD-style license in the LICENSE file. */
#define _GNU_SOURCE
#include <errno.h>
#include <inttypes.h>
#include <limits.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

_Static_assert(sizeof(int) == 4, "Linux x86-64 int ABI");
enum { PAGE = 4096, OFFSET = 64 };
static unsigned current_case;
static const uint64_t pids[] = {
    UINT64_C(0x0000000080000000), UINT64_C(0xffffffff80000000),
    UINT64_C(0x1234567880000000), UINT64_C(0x1234567880000001)};
static const uint64_t options[] = {
    WUNTRACED, WNOHANG | WUNTRACED, 0, WNOHANG,
    UINT64_C(0x1234567800000002), UINT64_C(0xffffffff00000003),
    UINT64_C(0x0000000000000100), UINT64_C(0xfedcba9800000102)};
static void require(int yes, const char *what) {
  if (!yes) {
    fprintf(stderr, "wait4 INT_MIN case=%u %s errno=%d\n", current_case, what, errno);
    syscall(SYS_exit_group, 90);
    __builtin_unreachable();
  }
}
static void hex(const unsigned char *p) {
  static const char digits[] = "0123456789abcdef";
  for (unsigned i = 0; i < PAGE; ++i) {
    putchar(digits[p[i] >> 4]);
    putchar(digits[p[i] & 15]);
  }
}
int main(void) {
  unsigned char *status = mmap(NULL, PAGE, PROT_READ | PROT_WRITE,
      MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  unsigned char *usage = mmap(NULL, PAGE, PROT_READ | PROT_WRITE,
      MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  require(status != MAP_FAILED && usage != MAP_FAILED, "map full output arenas");
  require(setvbuf(stdout, NULL, _IOLBF, 0) == 0, "line-buffered output");
  for (unsigned p = 0; p < sizeof(pids) / sizeof(pids[0]); ++p)
    for (unsigned o = 0; o < sizeof(options) / sizeof(options[0]); ++o)
      for (unsigned protected = 0; protected < 2; ++protected) {
        memset(status, 0xa5, PAGE);
        memset(usage, 0xa5, PAGE);
        if (protected) {
          require(mprotect(status, PAGE, PROT_READ) == 0, "protect status");
          require(mprotect(usage, PAGE, PROT_READ) == 0, "protect rusage");
        }
        errno = 0;
        long rc = syscall(SYS_wait4, pids[p], status + OFFSET, options[o], usage + OFFSET);
        int saved = errno;
        int invalid = ((uint32_t)options[o] & ~(uint32_t)(WNOHANG | WUNTRACED)) != 0;
        int expected = invalid ? EINVAL : (int32_t)(uint32_t)pids[p] == INT_MIN ? ESRCH : ECHILD;
        printf("{\"type\":\"min-pid\",\"case\":%u,\"raw_pid\":%" PRIu64
               ",\"raw_options\":%" PRIu64 ",\"protected\":%u,\"rc\":%ld,"
               "\"errno\":%d,\"status_arena\":\"", current_case, pids[p], options[o],
               protected, rc, saved);
        hex(status);
        printf("\",\"usage_arena\":\"");
        hex(usage);
        puts("\"}");
        require(fflush(stdout) == 0, "flush complete observation");
        require(rc == -1 && saved == expected, "exact errno precedence");
        for (unsigned i = 0; i < PAGE; ++i)
          require(status[i] == 0xa5 && usage[i] == 0xa5, "complete output guards untouched");
        require(mprotect(status, PAGE, PROT_READ | PROT_WRITE) == 0, "restore status");
        require(mprotect(usage, PAGE, PROT_READ | PROT_WRITE) == 0, "restore usage");
        ++current_case;
      }
  require(current_case == 64, "all exact raw argument/protection combinations");
  require(munmap(status, PAGE) == 0 && munmap(usage, PAGE) == 0, "unmap outputs");
  printf("{\"type\":\"summary\",\"cases\":64,\"calls\":64,\"children\":0,\"passed\":true}\n");
  require(fflush(stdout) == 0, "flush summary");
  return 0;
}
