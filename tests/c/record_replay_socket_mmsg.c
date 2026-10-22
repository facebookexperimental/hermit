/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Batched socket I/O next to socket effects that record/replay serves from
 * its log.
 *
 * With no argument, one end of a nonblocking AF_UNIX socketpair is shut down
 * for writing and the other end then receives with recvmmsg(2). Linux reports
 * the shutdown as end of file: one message of length 0. Prints
 * "socketpair: shutdown=0 recv=1 length=0 err=0" and exits 0 when it got that.
 *
 * With the argument "accepted", the guest accepts a loopback TCP connection
 * from a forked client and sends "ping" on it with sendmmsg(2). On Linux it
 * prints "accepted: sendmmsg=1 length=4 echo=ping" and exits 0. With
 * "accepted-recv", the client sends "ping" and the guest receives it with
 * recvmmsg(2), printing "accepted: recvmmsg=1 length=4 echo=ping".
 * Record/replay stands an accepted connection in with a descriptor that carries
 * no data, so it must refuse either call by name rather than let it run against
 * that.
 */

#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif

#include <arpa/inet.h>
#include <errno.h>
#include <netinet/in.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

static int socketpair_shutdown(void) {
  int fds[2];
  if (socketpair(AF_UNIX, SOCK_STREAM | SOCK_NONBLOCK, 0, fds) != 0) {
    perror("socketpair");
    return 2;
  }
  long shut = syscall(SYS_shutdown, (long)fds[0], (long)SHUT_WR);
  char byte = 'x';
  struct iovec iov = {.iov_base = &byte, .iov_len = 1};
  struct mmsghdr message;
  memset(&message, 0, sizeof message);
  message.msg_hdr.msg_iov = &iov;
  message.msg_hdr.msg_iovlen = 1;
  errno = 0;
  long got = syscall(
      SYS_recvmmsg, (long)fds[1], &message, 1L, (long)MSG_DONTWAIT, 0L);
  int error = errno;
  printf(
      "socketpair: shutdown=%ld recv=%ld length=%u err=%d\n",
      shut,
      got,
      message.msg_len,
      error);
  close(fds[0]);
  close(fds[1]);
  return shut == 0 && got == 1 && message.msg_len == 0 ? 0 : 1;
}

/*
 * Connects. With `receive` set, reads until end of file and succeeds if
 * "ping" arrived; otherwise sends "ping".
 */
static int client(const struct sockaddr_in* server, int receive) {
  int fd = socket(AF_INET, SOCK_STREAM, 0);
  if (fd < 0 ||
      connect(fd, (const struct sockaddr*)server, sizeof *server) != 0) {
    perror("connect");
    return 1;
  }
  if (!receive) {
    int sent = write(fd, "ping", 4) == 4;
    close(fd);
    return sent ? 0 : 1;
  }
  char buf[8];
  size_t total = 0;
  ssize_t got;
  while (total < sizeof buf &&
         (got = read(fd, buf + total, sizeof buf - total)) > 0) {
    total += (size_t)got;
  }
  close(fd);
  return total == 4 && memcmp(buf, "ping", 4) == 0 ? 0 : 1;
}

/*
 * Accepts one connection and sends "ping" on it with sendmmsg(2), or with
 * `receive` set receives the client's "ping" with recvmmsg(2).
 */
static int accepted_mmsg(int receive) {
  int listener = socket(AF_INET, SOCK_STREAM, 0);
  struct sockaddr_in server;
  memset(&server, 0, sizeof server);
  server.sin_family = AF_INET;
  server.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
  socklen_t length = sizeof server;
  if (listener < 0 ||
      bind(listener, (struct sockaddr*)&server, sizeof server) != 0 ||
      listen(listener, 1) != 0 ||
      getsockname(listener, (struct sockaddr*)&server, &length) != 0) {
    perror("listener");
    return 2;
  }
  fflush(stdout);
  pid_t child = fork();
  if (child == 0) {
    _exit(client(&server, !receive));
  }
  if (child < 0) {
    perror("fork");
    return 2;
  }
  int conn = accept(listener, NULL, NULL);
  char buf[] = "ping";
  if (receive) {
    memset(buf, 0, sizeof buf);
  }
  struct iovec iov = {.iov_base = buf, .iov_len = 4};
  struct mmsghdr message;
  memset(&message, 0, sizeof message);
  message.msg_hdr.msg_iov = &iov;
  message.msg_hdr.msg_iovlen = 1;
  long done = -1;
  if (conn >= 0) {
    long flags = receive ? MSG_WAITALL : MSG_NOSIGNAL;
    done = syscall(
        receive ? SYS_recvmmsg : SYS_sendmmsg, (long)conn, &message, 1L, flags,
        0L);
    close(conn);
  }
  int status = 0;
  int peer_ok = waitpid(child, &status, 0) == child && WIFEXITED(status) &&
      WEXITSTATUS(status) == 0;
  int echoed = peer_ok && (!receive || memcmp(buf, "ping", 4) == 0);
  printf(
      "accepted: %s=%ld length=%u echo=%s\n",
      receive ? "recvmmsg" : "sendmmsg",
      done,
      message.msg_len,
      echoed ? "ping" : "none");
  close(listener);
  return done == 1 && message.msg_len == 4 && echoed ? 0 : 1;
}

int main(int argc, char** argv) {
  if (argc > 1 && strcmp(argv[1], "accepted") == 0) {
    return accepted_mmsg(0);
  }
  if (argc > 1 && strcmp(argv[1], "accepted-recv") == 0) {
    return accepted_mmsg(1);
  }
  return socketpair_shutdown();
}
