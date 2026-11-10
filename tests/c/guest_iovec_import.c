/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * A vectored call that blocks while another thread rewrites the memory the
 * call copied when it began. Linux copies the iovec array (import_iovec), and
 * a message's header and address (copy_msghdr_from_user), before the call
 * waits, so the call moves its bytes as it began. Usage:
 * guest_iovec_import MODE
 *
 *   readv           a readv of an empty pipe; main points every iovec at the
 *                   second buffer, then writes 16 bytes.
 *   recvmsg         a recvmsg of an empty stream socket; main rewrites the
 *                   iovec array the same way, then writes 16 bytes.
 *   recvmsg-header  as recvmsg, but main points the header's msg_iov at a
 *                   second array, which names the second buffer.
 *   writev          a writev to a full stream socket; main points every iovec
 *                   at the second buffer, then drains the socket.
 *   writev-pipe     the same with a full pipe (an atomic 16-byte write).
 *   sendmsg         a sendmsg to a full stream socket, rewritten as writev is.
 *   sendmsg-header  as sendmsg, with msg_iov pointed at the second array.
 *   sendmsg-name    a sendmsg to a full datagram socket bound to the first
 *                   address; main rewrites that address to name a second
 *                   socket, then drains the first.
 *   sendto-name     the same with sendto, whose address Linux also copies
 *                   when the call begins (__sys_sendto).
 *
 * The worker sets `entering` just before its call; main waits for it and
 * sleeps 50 ms, so the call has begun and blocks, before it rewrites
 * anything. Prints the call's result and which buffer (or address) the bytes
 * went through. Natively every mode prints "MODE 16: bytes through the first
 * buffer" ("the first address" for sendmsg-name and sendto-name).
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <sched.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/uio.h>
#include <sys/un.h>
#include <time.h>
#include <unistd.h>

static const char *mode;
static int fds[2];
static int second_socket = -1;
static atomic_int entering, done;
static char first[16], second[16];
static struct iovec iov[2], second_iov[2];
static struct msghdr msg;
static struct sockaddr_un first_address, second_address;
static long result;

static int is(const char *name) {
  return strcmp(mode, name) == 0;
}

static void sleep_ms(long ms) {
  struct timespec delay = {ms / 1000, (ms % 1000) * 1000000};
  nanosleep(&delay, NULL);
}

static void point(struct iovec *array, char *buffer) {
  array[0] = (struct iovec){buffer, 8};
  array[1] = (struct iovec){buffer + 8, 8};
}

static void *worker(void *unused) {
  (void)unused;
  atomic_store(&entering, 1);
  if (is("readv")) {
    result = readv(fds[0], iov, 2);
  } else if (is("recvmsg") || is("recvmsg-header")) {
    result = recvmsg(fds[0], &msg, 0);
  } else if (is("writev") || is("writev-pipe")) {
    result = writev(fds[1], iov, 2);
  } else if (is("sendto-name")) {
    result = sendto(fds[1], first, sizeof(first), 0, (struct sockaddr *)&first_address,
                    sizeof(first_address));
  } else {
    result = sendmsg(fds[1], &msg, 0);
  }
  atomic_store(&done, 1);
  return NULL;
}

/* Write to fds[1] until a write would block. */
static void fill(void) {
  static char filler[4096];
  memset(filler, 'f', sizeof(filler));
  int flags = fcntl(fds[1], F_GETFL);
  fcntl(fds[1], F_SETFL, flags | O_NONBLOCK);
  while (write(fds[1], filler, sizeof(filler)) > 0) {
  }
  while (write(fds[1], filler, 1) > 0) {
  }
  fcntl(fds[1], F_SETFL, flags);
}

static int bound_datagram_socket(struct sockaddr_un *address, const char *name) {
  int fd = socket(AF_UNIX, SOCK_DGRAM, 0);
  memset(address, 0, sizeof(*address));
  address->sun_family = AF_UNIX;
  snprintf(address->sun_path + 1, sizeof(address->sun_path) - 1, "iovec-import-%s-%d", name,
           (int)getpid());
  if (fd < 0 || bind(fd, (struct sockaddr *)address, sizeof(*address))) {
    perror("bind");
    exit(2);
  }
  return fd;
}

/* Count the 'x' and 'y' bytes of one nonblocking read of `fd`. */
static long drain_once(int fd, long *x, long *y) {
  static char sink[1 << 16];
  long got = read(fd, sink, sizeof(sink));
  for (long i = 0; i < got; i++) {
    *x += sink[i] == 'x';
    *y += sink[i] == 'y';
  }
  return got;
}

int main(int argc, char **argv) {
  if (argc != 2) {
    fprintf(stderr, "usage: %s MODE\n", argv[0]);
    return 2;
  }
  mode = argv[1];
  setvbuf(stdout, NULL, _IONBF, 0);
  int reads = is("readv") || is("recvmsg") || is("recvmsg-header");
  int named = is("sendmsg-name") || is("sendto-name");
  if (is("readv") || is("writev-pipe")) {
    if (pipe(fds)) {
      return 2;
    }
  } else if (named) {
    fds[0] = bound_datagram_socket(&first_address, "first");
    second_socket = bound_datagram_socket(&second_address, "second");
    fds[1] = socket(AF_UNIX, SOCK_DGRAM, 0);
    if (fds[1] < 0) {
      return 2;
    }
  } else if (socketpair(AF_UNIX, SOCK_STREAM, 0, fds)) {
    return 2;
  }
  point(iov, first);
  point(second_iov, second);
  msg.msg_iov = iov;
  msg.msg_iovlen = 2;
  if (named) {
    msg.msg_name = &first_address;
    msg.msg_namelen = sizeof(first_address);
  }
  if (!reads) {
    memset(first, 'x', sizeof(first));
    memset(second, 'y', sizeof(second));
    if (named) {
      int flags = fcntl(fds[1], F_GETFL);
      fcntl(fds[1], F_SETFL, flags | O_NONBLOCK);
      while (sendto(fds[1], "f", 1, 0, (struct sockaddr *)&first_address,
                    sizeof(first_address)) == 1) {
      }
      fcntl(fds[1], F_SETFL, flags);
    } else {
      fill();
    }
  }
  pthread_t thread;
  pthread_create(&thread, NULL, worker, NULL);
  while (!atomic_load(&entering)) {
    sched_yield();
  }
  sleep_ms(50);
  if (named) {
    memcpy(&first_address, &second_address, sizeof(first_address));
  } else if (is("recvmsg-header") || is("sendmsg-header")) {
    msg.msg_iov = second_iov;
  } else {
    point(iov, second);
  }
  long x = 0, y = 0;
  if (reads) {
    if (write(fds[1], "xxxxxxxxxxxxxxxx", 16) != 16) {
      return 2;
    }
    pthread_join(thread, NULL);
    for (int i = 0; i < 16; i++) {
      x += first[i] == 'x';
      y += second[i] == 'x';
    }
  } else {
    fcntl(fds[0], F_SETFL, fcntl(fds[0], F_GETFL) | O_NONBLOCK);
    while (!atomic_load(&done)) {
      if (drain_once(fds[0], &x, &y) <= 0) {
        sleep_ms(1);
      }
    }
    while (drain_once(fds[0], &x, &y) > 0) {
    }
    pthread_join(thread, NULL);
    if (named) {
      long through_second = 0, unused = 0;
      fcntl(second_socket, F_SETFL, fcntl(second_socket, F_GETFL) | O_NONBLOCK);
      while (drain_once(second_socket, &through_second, &unused) > 0) {
      }
      printf("%s %ld: bytes through %s\n", mode, result,
             x && through_second ? "both addresses"
                                 : (x ? "the first address"
                                      : (through_second ? "the second address" : "no address")));
      return 0;
    }
  }
  printf("%s %ld: bytes through %s\n", mode, result,
         x && y ? "both buffers"
                : (x ? "the first buffer" : (y ? "the second buffer" : "no buffer")));
  return 0;
}
