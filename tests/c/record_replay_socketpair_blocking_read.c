/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * A guest that blocks reading a channel made by pipe(2) or socketpair(2)
 * until the other process writes "hello" to it after a short sleep. Hermit
 * keeps both kinds of channel physically nonblocking so that a blocking read
 * can wait deterministically; a recording must make the read wait for the
 * writer rather than return EAGAIN. Each of three rounds uses a fresh channel,
 * so a round cannot pass on bytes an earlier round left unread:
 *
 *   original: the parent reads the descriptor the channel call returned;
 *   dup:      the parent reads a dup(2) of it, with the original closed;
 *   fork:     a forked child reads the descriptor it inherited.
 *
 * On a socketpair the dup round uses recv(2) and send(2), and the fork round
 * recvmsg(2) and sendmsg(2); every pipe round uses read(2) and write(2).
 *
 * With a second argument, each round's reader first turns O_NONBLOCK on and
 * then off again, with ioctl(FIONBIO) or with fcntl(F_SETFL), and checks that
 * the flag reads back clear. Clearing the guest's flag must not clear the
 * physical nonblocking state Hermit relies on.
 *
 * Usage: record_replay_socketpair_blocking_read
 *            pipe|stream|seqpacket|dgram [fionbio|setfl]
 * Prints one line per round and exits 0 only if every read got "hello".
 */

#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif

#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/socket.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

/* How a round moves its bytes. */
enum transfer { PLAIN, SEND_RECV, SENDMSG_RECVMSG };

static const char* receive_name(enum transfer how) {
  return how == SEND_RECV ? "recv"
      : how == SENDMSG_RECVMSG ? "recvmsg"
                               : "read";
}

static ssize_t send_hello(int fd, enum transfer how) {
  char data[] = "hello";
  struct iovec iov = {.iov_base = data, .iov_len = 5};
  struct msghdr msg = {.msg_iov = &iov, .msg_iovlen = 1};
  return how == SEND_RECV ? send(fd, data, 5, 0)
      : how == SENDMSG_RECVMSG ? sendmsg(fd, &msg, 0)
                               : write(fd, data, 5);
}

static ssize_t receive(int fd, char* buf, size_t len, enum transfer how) {
  struct iovec iov = {.iov_base = buf, .iov_len = len};
  struct msghdr msg = {.msg_iov = &iov, .msg_iovlen = 1};
  return how == SEND_RECV ? recv(fd, buf, len, 0)
      : how == SENDMSG_RECVMSG ? recvmsg(fd, &msg, 0)
                               : read(fd, buf, len);
}

static int set_nonblocking(int fd, const char* clear, int enabled) {
  if (strcmp(clear, "fionbio") == 0) {
    return ioctl(fd, FIONBIO, &enabled);
  }
  int flags = fcntl(fd, F_GETFL);
  if (flags < 0) {
    return -1;
  }
  flags = enabled ? (flags | O_NONBLOCK) : (flags & ~O_NONBLOCK);
  return fcntl(fd, F_SETFL, flags);
}

/* Turns fd's O_NONBLOCK on and off again; returns 0 if it then reads back
 * clear. Does nothing without a clear mode. */
static int toggle_nonblocking(int fd, const char* clear) {
  if (clear == NULL) {
    return 0;
  }
  if (set_nonblocking(fd, clear, 1) != 0 || set_nonblocking(fd, clear, 0) != 0) {
    fprintf(stderr, "%s: %s\n", clear, strerror(errno));
    return 1;
  }
  int flags = fcntl(fd, F_GETFL);
  return flags >= 0 && (flags & O_NONBLOCK) == 0 ? 0 : 1;
}

static int make_channel(const char* kind, int fds[2]) {
  if (strcmp(kind, "pipe") == 0) {
    return pipe(fds);
  }
  int type;
  if (strcmp(kind, "stream") == 0) {
    type = SOCK_STREAM;
  } else if (strcmp(kind, "seqpacket") == 0) {
    type = SOCK_SEQPACKET;
  } else if (strcmp(kind, "dgram") == 0) {
    type = SOCK_DGRAM;
  } else {
    errno = EINVAL;
    return -1;
  }
  return socketpair(AF_UNIX, type, 0, fds);
}

static void sleep_briefly(void) {
  struct timespec delay = {.tv_sec = 0, .tv_nsec = 10 * 1000 * 1000};
  nanosleep(&delay, NULL);
}

/* Receives once from fd and prints the round's result; returns 0 on "hello". */
static int read_hello(const char* kind, const char* round, int fd,
                      enum transfer how) {
  char buf[16];
  ssize_t got = receive(fd, buf, sizeof buf, how);
  int read_errno = got < 0 ? errno : 0;
  printf("%s %s: %s=%zd errno=%d data=%.*s\n", kind, round, receive_name(how),
         got, read_errno, got > 0 ? (int)got : 0, buf);
  fflush(stdout);
  return got == 5 && memcmp(buf, "hello", 5) == 0 ? 0 : 1;
}

static int wait_for(pid_t pid) {
  int status = 0;
  if (waitpid(pid, &status, 0) != pid) {
    perror("waitpid");
    return 1;
  }
  return WIFEXITED(status) && WEXITSTATUS(status) == 0 ? 0 : 1;
}

/* The parent reads fd while a forked child writes write_fd. */
static int parent_reads(const char* kind, const char* round, int fd,
                        int write_fd, enum transfer how) {
  fflush(stdout);
  pid_t pid = fork();
  if (pid < 0) {
    perror("fork");
    return 1;
  }
  if (pid == 0) {
    sleep_briefly();
    _exit(send_hello(write_fd, how) == 5 ? 0 : 3);
  }
  int failed = read_hello(kind, round, fd, how);
  return wait_for(pid) | failed;
}

/* A forked child reads its inherited fd while the parent writes write_fd. */
static int child_reads(const char* kind, int fd, int write_fd,
                       enum transfer how) {
  fflush(stdout);
  pid_t pid = fork();
  if (pid < 0) {
    perror("fork");
    return 1;
  }
  if (pid == 0) {
    _exit(read_hello(kind, "fork", fd, how));
  }
  sleep_briefly();
  int failed = send_hello(write_fd, how) == 5 ? 0 : 1;
  return wait_for(pid) | failed;
}

int main(int argc, char** argv) {
  if (argc != 2 && argc != 3) {
    fprintf(stderr, "usage: %s pipe|stream|seqpacket|dgram [fionbio|setfl]\n",
            argv[0]);
    return 2;
  }
  const char* kind = argv[1];
  const char* clear = argc == 3 ? argv[2] : NULL;
  if (clear != NULL && strcmp(clear, "fionbio") != 0 &&
      strcmp(clear, "setfl") != 0) {
    fprintf(stderr, "unknown clear mode %s\n", clear);
    return 2;
  }
  int is_socket = strcmp(kind, "pipe") != 0;
  int original[2], duped[2], forked[2];
  if (make_channel(kind, original) != 0 || make_channel(kind, duped) != 0 ||
      make_channel(kind, forked) != 0) {
    fprintf(stderr, "%s: %s\n", kind, strerror(errno));
    return 2;
  }
  int alias = dup(duped[0]);
  if (alias < 0 || close(duped[0]) != 0) {
    perror("dup");
    return 2;
  }
  int failed = toggle_nonblocking(original[0], clear) |
      toggle_nonblocking(alias, clear) | toggle_nonblocking(forked[0], clear);
  failed |= parent_reads(kind, "original", original[0], original[1], PLAIN);
  failed |= parent_reads(kind, "dup", alias, duped[1],
                         is_socket ? SEND_RECV : PLAIN);
  failed |= child_reads(kind, forked[0], forked[1],
                        is_socket ? SENDMSG_RECVMSG : PLAIN);
  return failed ? 16 : 0;
}
