/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// Shared file views in a process with several threads
// (https://github.com/rrnewton/reverie/issues/1108). Two worker threads start
// first. Then:
// - The main thread maps a MAP_SHARED read-write view of a file. Each worker
//   stores its own byte through the view and, after a barrier, reads every
//   worker's byte back; the main thread checks the file with pread.
// - With the workers alive, the main thread unmaps the view and maps a new
//   view of the file, and the workers use the new view the same way.
// - The main thread makes the view read-only; a worker's gettimeofday(2) into
//   it then fails with EFAULT and stores nothing. The call is made with
//   syscall(2): the kernel's put_user checks that the page is writable, where
//   a vDSO store would fault instead. (A read(2) into the view would not do:
//   KVM's copy-out stores into a read-only page,
//   https://github.com/rrnewton/reverie/issues/1239.)
// - The main thread maps its own executable read-only and MAP_SHARED with the
//   workers alive, as the fbcode Python interpreter does at startup with a
//   second thread running.
// - With both views held, the main thread creates a third thread, which reads
//   the workers' bytes through the file's view and the ELF magic through the
//   executable's, and joins it.
// Every step prints one line, the same natively and under Hermit.

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <unistd.h>

enum { PAGE = 4096, WORKERS = 2 };

#define REQUIRE(condition, step)                                               \
  do {                                                                        \
    if (!(condition)) {                                                       \
      fprintf(stderr, "%s failed errno=%d\n", step, errno);                    \
      exit(1);                                                                \
    }                                                                         \
  } while (0)

static pthread_barrier_t barrier;
static unsigned char* volatile view;
static const unsigned char* volatile image;

static unsigned char mark(int round, long worker) {
  return (unsigned char)('a' + round * WORKERS + worker);
}

static void wait_all(void) {
  int rc = pthread_barrier_wait(&barrier);
  REQUIRE(rc == 0 || rc == PTHREAD_BARRIER_SERIAL_THREAD, "pthread_barrier_wait");
}

static void* worker(void* argument) {
  long index = (long)argument;
  wait_all(); // the first view is mapped
  for (int round = 0; round < 2; round++) {
    view[index] = mark(round, index);
    wait_all(); // every worker stored
    for (long other = 0; other < WORKERS; other++) {
      REQUIRE(view[other] == mark(round, other), "a worker's read of the view");
    }
    wait_all(); // every worker read
    wait_all(); // the view is mapped again, or made read-only
  }
  if (index == 0) {
    unsigned char before[16];
    memcpy(before, view, sizeof(before));
    errno = 0;
    long result = syscall(SYS_gettimeofday, (void*)view, NULL);
    REQUIRE(result == -1 && errno == EFAULT, "gettimeofday into the read-only view");
    REQUIRE(memcmp(before, view, sizeof(before)) == 0, "the read-only view's bytes");
  }
  wait_all(); // the call is done
  wait_all(); // the executable is mapped
  return NULL;
}

// The third thread: it starts while both views are held.
static void* reader(void* argument) {
  (void)argument;
  for (long index = 0; index < WORKERS; index++) {
    REQUIRE(view[index] == mark(1, index), "a new thread's read of the file's view");
  }
  REQUIRE(memcmp((const void*)image, "\177ELF", 4) == 0,
          "a new thread's read of the executable's view");
  return NULL;
}

int main(int argc, char** argv) {
  (void)argc;
  char path[] = "shared-file-threads-XXXXXX";
  int fd = mkstemp(path);
  REQUIRE(fd >= 0 && unlink(path) == 0 && ftruncate(fd, PAGE) == 0, "the file");
  REQUIRE(pthread_barrier_init(&barrier, NULL, WORKERS + 1) == 0, "pthread_barrier_init");
  pthread_t threads[WORKERS];
  for (long index = 0; index < WORKERS; index++) {
    REQUIRE(pthread_create(&threads[index], NULL, worker, (void*)index) == 0,
            "pthread_create");
  }
  view = mmap(NULL, PAGE, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
  REQUIRE(view != MAP_FAILED, "mmap with threads alive");
  printf("mapped a shared view with %d threads alive\n", WORKERS);
  wait_all();
  for (int round = 0; round < 2; round++) {
    wait_all();
    wait_all();
    unsigned char bytes[WORKERS];
    REQUIRE(pread(fd, bytes, WORKERS, 0) == WORKERS, "pread");
    for (long index = 0; index < WORKERS; index++) {
      REQUIRE(bytes[index] == mark(round, index), "the file's bytes");
    }
    printf("round %d: the file holds every thread's store, and every thread read them all\n",
           round);
    if (round == 0) {
      REQUIRE(munmap(view, PAGE) == 0, "munmap with threads alive");
      view = mmap(NULL, PAGE, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
      REQUIRE(view != MAP_FAILED, "a new mmap with threads alive");
      printf("unmapped the view and mapped it again with threads alive\n");
    } else {
      REQUIRE(mprotect(view, PAGE, PROT_READ) == 0, "mprotect with threads alive");
    }
    wait_all();
  }
  wait_all();
  printf("a thread's gettimeofday into the read-only view failed with EFAULT\n");

  int self = open(argv[0], O_RDONLY | O_CLOEXEC);
  struct stat status;
  REQUIRE(self >= 0 && fstat(self, &status) == 0, "the executable");
  void* mapped = mmap(NULL, status.st_size, PROT_READ, MAP_SHARED, self, 0);
  REQUIRE(mapped != MAP_FAILED, "a read-only view of the executable");
  image = mapped;
  REQUIRE(memcmp(mapped, "\177ELF", 4) == 0, "the executable's view");
  printf("mapped the executable read-only beside the threads\n");
  pthread_t third;
  REQUIRE(pthread_create(&third, NULL, reader, NULL) == 0, "pthread_create with views held");
  REQUIRE(pthread_join(third, NULL) == 0, "joining the third thread");
  printf("a thread created while both views were held read them\n");
  wait_all();

  for (long index = 0; index < WORKERS; index++) {
    REQUIRE(pthread_join(threads[index], NULL) == 0, "pthread_join");
  }
  printf("threads joined\n");
  return 0;
}
