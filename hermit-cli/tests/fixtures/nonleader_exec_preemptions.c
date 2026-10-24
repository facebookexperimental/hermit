/* Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved. Licensed under the BSD-style license in LICENSE. */

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <limits.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

#define CHECK(expression)                                                    \
  do {                                                                       \
    if (!(expression)) {                                                     \
      fprintf(                                                               \
          stderr, "line %d: %s (errno=%d)\n", __LINE__, #expression, errno); \
      _exit(90);                                                             \
    }                                                                        \
  } while (0)

static const char* target;

static void* worker(void* unused) {
  (void)unused;
  pid_t pid = (pid_t)syscall(SYS_getpid);
  pid_t tid = (pid_t)syscall(SYS_gettid);
  CHECK(pid != tid);
  uint64_t previous = 0;
  for (unsigned sample = 0; sample < 2; ++sample) {
    volatile uint64_t work = 1;
    /* 120k actual branches exceed the ordinary 100k-RCB maximum at 1ms.
     * PAUSE is a nonblocking spin hint: the thread stays runnable while
     * retiring branches less densely than a bare LOOP. LOOP still decrements
     * RCX and conditionally branches in one instruction, so the finite branch
     * count, and every printed value, is the same as a bare LOOP's.
     *
     * Hermit asks for each precise PMU timer interrupt a fixed number of
     * branches (the skid margin) before its target and single-steps the rest
     * of the way, one step per instruction. The interrupt lands some time
     * late. A bare LOOP retires so many branches in that time that it
     * occasionally overran the 3,072-RCB margin, which Hermit refuses with
     * exit 122. With PAUSE the same timers land a few branches late. Each
     * branch is now two instructions, so each single-step tail is about
     * twice as long. */
    uint64_t remaining = 120000;
    __asm__ volatile("1: pause\n loop 1b" : "+c"(remaining));
    CHECK(remaining == 0);
    work += 3 * (120000 - remaining);
    struct timespec now;
    CHECK(syscall(SYS_clock_gettime, CLOCK_REALTIME, &now) == 0);
    CHECK(now.tv_sec >= 0 && now.tv_nsec >= 0 && now.tv_nsec < 1000000000);
    uint64_t nanos = (uint64_t)now.tv_sec * 1000000000 + (uint64_t)now.tv_nsec;
    CHECK(nanos > previous);
    printf(
        "prefix sample=%u pid=%d worker=%d nanos=%" PRIu64 " work=%" PRIu64
        "\n",
        sample,
        pid,
        tid,
        nanos,
        work);
    previous = nanos;
  }
  CHECK(fflush(stdout) == 0);
  /* Every phase uses the same binary, argv, and pathname. Only the host's
   * presence of an executable at this path changes the result of exec. */
  char* const args[] = {(char*)target, "replacement", "image", NULL};
  execv(target, args);
  CHECK(errno == ENOENT);
  puts("failed-exec-preserved");
  return NULL;
}

int main(int argc, char** argv) {
  if (argc == 3 && strcmp(argv[1], "replacement") == 0 &&
      strcmp(argv[2], "image") == 0) {
    char marker[PATH_MAX];
    int length = snprintf(marker, sizeof(marker), "%s.ran", argv[0]);
    CHECK(length > 0 && (size_t)length < sizeof(marker));
    int fd = open(marker, O_WRONLY | O_CREAT | O_TRUNC, 0600);
    CHECK(fd >= 0);
    CHECK(write(fd, "1", 1) == 1);
    CHECK(close(fd) == 0);
    puts("replacement-image-ran");
    return 0;
  }
  CHECK(argc == 2);
  target = argv[1];
  pthread_t thread;
  CHECK(pthread_create(&thread, NULL, worker, NULL) == 0);
  CHECK(pthread_join(thread, NULL) == 0);
  puts("failed-exec-control-ok");
  return 0;
}
