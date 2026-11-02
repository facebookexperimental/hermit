/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Anonymous-inode descriptors (eventfd, signalfd) a process receives over
 * SCM_RIGHTS.
 *
 * fstat cannot tell one anonymous inode from another, so Detcore does not
 * record one it receives on ptrace and KVM (a record of the wrong kind would
 * mishandle it: a blocking signalfd read held in the caller's turn, an
 * EFD_SEMAPHORE eventfd read twice). Calls through such a descriptor that
 * consult Detcore's descriptor table therefore fail with EBADF, as they did
 * before Detcore recorded any received descriptor: a known gap, which this
 * program pins and proves does not hang. Natively every call succeeds.
 *
 * A child creates an eventfd holding 5, an EFD_SEMAPHORE eventfd holding 3,
 * a nonblocking eventfd and a signalfd for SIGUSR1, and sends each to its
 * parent, which has SIGUSR1 blocked and pending. The parent reads the
 * signalfd with room for two records (natively one record arrives), reads
 * 16 bytes from the semaphore eventfd (natively 8 bytes, one count), clears
 * O_NONBLOCK on the nonblocking eventfd through its received descriptor, and
 * reads and writes the counting eventfd.
 */

#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif

#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/eventfd.h>
#include <sys/signalfd.h>
#include <sys/socket.h>
#include <sys/wait.h>
#include <unistd.h>

static void die(const char* what) {
  printf("FATAL %s errno=%d\n", what, errno);
  fflush(stdout);
  exit(EXIT_FAILURE);
}

/* Send `fd` (or, if -1, no descriptor) with a one-byte message. */
static int send_fd(int sock, int fd) {
  char byte = 'm';
  struct iovec iov = {.iov_base = &byte, .iov_len = 1};
  union {
    char buffer[CMSG_SPACE(sizeof(int))];
    struct cmsghdr align;
  } control;
  memset(&control, 0, sizeof(control));
  struct msghdr message = {.msg_iov = &iov, .msg_iovlen = 1};
  if (fd >= 0) {
    message.msg_control = control.buffer;
    message.msg_controllen = sizeof(control.buffer);
    struct cmsghdr* header = CMSG_FIRSTHDR(&message);
    header->cmsg_level = SOL_SOCKET;
    header->cmsg_type = SCM_RIGHTS;
    header->cmsg_len = CMSG_LEN(sizeof(int));
    memcpy(CMSG_DATA(header), &fd, sizeof(int));
  }
  return sendmsg(sock, &message, 0) == 1 ? 0 : errno;
}

/* Receive one message; its descriptor, or -1 when it carried none. */
static int receive_fd(int sock) {
  char byte = 0;
  struct iovec iov = {.iov_base = &byte, .iov_len = 1};
  union {
    char buffer[CMSG_SPACE(sizeof(int))];
    struct cmsghdr align;
  } control;
  memset(&control, 0, sizeof(control));
  struct msghdr message = {
      .msg_iov = &iov,
      .msg_iovlen = 1,
      .msg_control = control.buffer,
      .msg_controllen = sizeof(control.buffer),
  };
  if (recvmsg(sock, &message, 0) != 1) {
    die("recvmsg");
  }
  struct cmsghdr* header = CMSG_FIRSTHDR(&message);
  if (header == NULL || header->cmsg_type != SCM_RIGHTS) {
    return -1;
  }
  int fd;
  memcpy(&fd, CMSG_DATA(header), sizeof(int));
  return fd;
}

/* Sends four descriptors, each in its own message; one that cannot be sent
 * is replaced by a message without one, and its errno is printed. */
static void sender(int sock) {
  sigset_t usr1;
  sigemptyset(&usr1);
  sigaddset(&usr1, SIGUSR1);
  int fds[4] = {
      eventfd(5, 0),
      eventfd(3, EFD_SEMAPHORE),
      eventfd(0, EFD_NONBLOCK),
      signalfd(-1, &usr1, 0),
  };
  const char* names[4] = {"eventfd", "semaphore", "nonblocking", "signalfd"};
  for (int i = 0; i < 4; i++) {
    if (fds[i] < 0) {
      printf("sender %s create errno=%d\n", names[i], errno);
    }
    int error = send_fd(sock, fds[i]);
    if (error != 0) {
      printf("sender %s sendmsg errno=%d\n", names[i], error);
      if (send_fd(sock, -1) != 0) {
        die("send placeholder message");
      }
    }
  }
  fflush(stdout);
  _exit(0);
}

static void result(const char* what, ssize_t ret) {
  printf("%s=%zd errno=%d\n", what, ret, ret < 0 ? errno : 0);
}

int main(void) {
  setvbuf(stdout, NULL, _IONBF, 0);
  sigset_t usr1;
  sigemptyset(&usr1);
  sigaddset(&usr1, SIGUSR1);
  if (sigprocmask(SIG_BLOCK, &usr1, NULL) != 0) {
    die("sigprocmask");
  }
  int channel[2];
  if (socketpair(AF_UNIX, SOCK_DGRAM, 0, channel) != 0) {
    die("socketpair");
  }
  pid_t child = fork();
  if (child < 0) {
    die("fork");
  }
  if (child == 0) {
    close(channel[0]);
    sender(channel[1]);
  }
  close(channel[1]);
  int status = 0;
  if (waitpid(child, &status, 0) != child || !WIFEXITED(status) ||
      WEXITSTATUS(status) != 0) {
    die("sender exit");
  }
  int counting = receive_fd(channel[0]);
  int semaphore = receive_fd(channel[0]);
  int nonblocking = receive_fd(channel[0]);
  int signals = receive_fd(channel[0]);
  printf(
      "received eventfd=%d semaphore=%d nonblocking=%d signalfd=%d\n",
      counting >= 0,
      semaphore >= 0,
      nonblocking >= 0,
      signals >= 0);

  uint64_t value = 0;
  result("eventfd read", read(counting, &value, sizeof(value)));
  printf("eventfd value=%llu\n", (unsigned long long)value);
  uint64_t two = 2;
  result("eventfd write", write(counting, &two, sizeof(two)));

  uint64_t counts[2] = {0, 0};
  result("semaphore read 16", read(semaphore, counts, sizeof(counts)));
  printf("semaphore value=%llu\n", (unsigned long long)counts[0]);

  result("nonblocking F_GETFL", fcntl(nonblocking, F_GETFL));
  result("nonblocking F_SETFL 0", fcntl(nonblocking, F_SETFL, 0));
  int flags = fcntl(nonblocking, F_GETFL);
  printf("nonblocking now=%d\n", flags < 0 ? -1 : (flags & O_NONBLOCK) != 0);

  if (signals >= 0) {
    if (raise(SIGUSR1) != 0) {
      die("raise");
    }
    struct signalfd_siginfo records[2];
    memset(records, 0, sizeof(records));
    result("signalfd read 2 records", read(signals, records, sizeof(records)));
    printf("signalfd signo=%u\n", records[0].ssi_signo);
  } else {
    printf("signalfd not received\n");
  }
  printf("done\n");
  return 0;
}
