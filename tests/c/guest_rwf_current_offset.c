/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * preadv2 and pwritev2 at offset -1, the current position, on descriptors
 * the guest left blocking. Linux runs them as readv and writev (preadv2 calls
 * do_readv, pwritev2 do_writev), so they wait as those do. The calls are
 * raw syscalls: glibc's wrappers fall back to readv and writev on ENOSYS,
 * which would hide a regression. Usage: guest_rwf_current_offset MODE
 *
 *   preadv2-pipe         an empty pipe; a thread writes 16 bytes after 50 ms.
 *   preadv2-socket       the same with a stream socketpair.
 *   preadv2-eventfd      an eventfd at 0; a thread adds 1 after 50 ms.
 *   preadv2-timerfd      a timerfd armed to expire once, 50 ms on.
 *   preadv2-timerfd-short the same with a 4-byte buffer: fails with EINVAL.
 *   preadv2-fifo         a FIFO opened by path; a thread opens its write end
 *                        and writes 16 bytes after 50 ms.
 *   preadv2-nowait       an empty pipe with RWF_NOWAIT: fails with EAGAIN.
 *   pwritev2-pipe-full   16 bytes into a full pipe; a thread drains it.
 *   pwritev2-pipe-large  70000 bytes into a pipe with room for 4096; a
 *                        thread drains it: the call writes all 70000.
 *   pwritev2-socket-full 16 bytes into a full stream socket; a thread drains.
 *
 * Prints "MODE RESULT ERRNO". Natively: the readers print 16 (8 for the
 * eventfd and the timerfd), the writers 16 (70000 for the large one),
 * preadv2-nowait prints "-1 EAGAIN" and preadv2-timerfd-short "-1 EINVAL".
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/eventfd.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/timerfd.h>
#include <sys/uio.h>
#include <time.h>
#include <unistd.h>

static const char *mode;
static int fds[2];
static atomic_int done;
static char fifo_path[64];

static int is(const char *name) {
  return strcmp(mode, name) == 0;
}

static void sleep_ms(long ms) {
  struct timespec delay = {ms / 1000, (ms % 1000) * 1000000};
  nanosleep(&delay, NULL);
}

static void *writer(void *unused) {
  (void)unused;
  if (is("preadv2-fifo")) {
    /* Opens once main opens the read end. */
    fds[1] = open(fifo_path, O_WRONLY);
  }
  sleep_ms(50);
  if (is("preadv2-eventfd")) {
    uint64_t one = 1;
    if (write(fds[0], &one, sizeof(one)) != sizeof(one)) {
      perror("eventfd write");
    }
  } else if (write(fds[1], "0123456789abcdef", 16) != 16) {
    perror("write");
  }
  return NULL;
}

static void *drainer(void *unused) {
  (void)unused;
  static char sink[1 << 16];
  sleep_ms(50);
  fcntl(fds[0], F_SETFL, fcntl(fds[0], F_GETFL) | O_NONBLOCK);
  while (!atomic_load(&done)) {
    if (read(fds[0], sink, sizeof(sink)) <= 0) {
      sleep_ms(1);
    }
  }
  return NULL;
}

/* preadv2 or pwritev2 at offset -1, split into low and high words as
 * glibc does, with `flags`. */
static long at_current_offset(long nr, int fd, struct iovec *iov, int count, int flags) {
  return syscall(nr, fd, iov, count, (long)-1, (long)((uint64_t)-1 >> 32), flags);
}

/* Write to fds[1] until only `room` bytes of a pipe's capacity are left, or,
 * for a socket (room 0), until a write would block. */
static void fill(long room) {
  static char filler[1 << 16];
  int flags = fcntl(fds[1], F_GETFL);
  fcntl(fds[1], F_SETFL, flags | O_NONBLOCK);
  if (room > 0) {
    long left = fcntl(fds[1], F_GETPIPE_SZ) - room;
    while (left > 0) {
      long wrote = write(fds[1], filler, left);
      if (wrote <= 0) {
        break;
      }
      left -= wrote;
    }
  } else {
    while (write(fds[1], filler, sizeof(filler)) > 0) {
    }
    while (write(fds[1], filler, 1) > 0) {
    }
  }
  fcntl(fds[1], F_SETFL, flags);
}

int main(int argc, char **argv) {
  if (argc != 2) {
    fprintf(stderr, "usage: %s MODE\n", argv[0]);
    return 2;
  }
  mode = argv[1];
  setvbuf(stdout, NULL, _IONBF, 0);
  int socket_mode = strstr(mode, "socket") != NULL;
  int timer_mode = strncmp(mode, "preadv2-timerfd", 15) == 0;
  int fifo_mode = is("preadv2-fifo");
  if (is("preadv2-eventfd")) {
    fds[0] = eventfd(0, 0);
    if (fds[0] < 0) {
      return 2;
    }
  } else if (timer_mode) {
    fds[0] = timerfd_create(CLOCK_MONOTONIC, 0);
    struct itimerspec once = {{0, 0}, {0, 50000000}};
    if (fds[0] < 0 || timerfd_settime(fds[0], 0, &once, NULL)) {
      return 2;
    }
  } else if (fifo_mode) {
    strcpy(fifo_path, "/tmp/rwf-XXXXXX");
    if (!mkdtemp(fifo_path)) {
      return 2;
    }
    strcat(fifo_path, "/fifo");
    if (mkfifo(fifo_path, 0600)) {
      return 2;
    }
  } else if (socket_mode ? socketpair(AF_UNIX, SOCK_STREAM, 0, fds) : pipe(fds)) {
    return 2;
  }
  static char buffer[70000];
  memset(buffer, 'x', sizeof(buffer));
  int reads = strncmp(mode, "preadv2", 7) == 0;
  struct iovec iov[2] = {{buffer, 8}, {buffer + 8, 8}};
  /* An eventfd or timerfd read takes exactly 8 bytes. */
  int count = is("preadv2-eventfd") || timer_mode ? 1 : 2;
  /* No thread is needed where nothing must arrive: the timer expires by
   * itself, and RWF_NOWAIT returns at once. */
  int threaded = !is("preadv2-nowait") && !timer_mode;
  pthread_t thread;
  long result;
  if (reads) {
    if (threaded) {
      pthread_create(&thread, NULL, writer, NULL);
    }
    if (fifo_mode) {
      /* Opens once the writer thread opens the write end. */
      fds[0] = open(fifo_path, O_RDONLY);
    }
    if (is("preadv2-timerfd-short")) {
      iov[0].iov_len = 4;
    }
    int flags = is("preadv2-nowait") ? RWF_NOWAIT : 0;
    result = at_current_offset(SYS_preadv2, fds[0], iov, count, flags);
  } else {
    if (is("pwritev2-pipe-large")) {
      iov[0] = (struct iovec){buffer, 35000};
      iov[1] = (struct iovec){buffer + 35000, 35000};
      fill(4096);
    } else {
      fill(0);
    }
    pthread_create(&thread, NULL, drainer, NULL);
    result = at_current_offset(SYS_pwritev2, fds[1], iov, 2, 0);
  }
  int error = result < 0 ? errno : 0;
  atomic_store(&done, 1);
  if (threaded) {
    pthread_join(thread, NULL);
  }
  if (fifo_mode) {
    unlink(fifo_path);
    *strrchr(fifo_path, '/') = '\0';
    rmdir(fifo_path);
  }
  printf("%s %ld %s\n", mode, result, error ? strerrorname_np(error) : "0");
  return 0;
}
