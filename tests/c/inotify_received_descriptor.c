/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// Inotify rename cookies read through descriptors received over SCM_RIGHTS.
// An inotify instance is an anonymous inode, so fstat does not tell its kind,
// but its descriptor link does. Instance A reads every rename whole. Instance
// R, sent to this process over a socketpair and received back, and instance
// C, created by the parent after a fork and sent to the child, must each give
// a rename the cookie A gives it, as Linux does for every instance. Linux
// draws cookies from one host-wide counter, so their values differ from run
// to run; that agreement is all the guest prints (under Hermit,
// `--verify-strict` also compares the values).

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/inotify.h>
#include <sys/socket.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <unistd.h>

// Every name here is two characters, which Linux pads to 16 bytes, so every
// event is 32 bytes.
#define EVENT 32

static char dir[] = "/tmp/inotify-received-XXXXXX";

static int watch(void) {
  int fd = inotify_init1(IN_NONBLOCK | IN_CLOEXEC);
  if (fd < 0 || inotify_add_watch(fd, dir, IN_MOVED_FROM | IN_MOVED_TO) < 0) exit(2);
  return fd;
}

static void rename_in_dir(const char *from, const char *to) {
  char old[64], new[64];
  snprintf(old, sizeof old, "%s/%s", dir, from);
  snprintf(new, sizeof new, "%s/%s", dir, to);
  if (rename(old, new) != 0) exit(3);
}

static void send_fd(int socket, int fd) {
  char byte = 0;
  struct iovec data = {&byte, 1};
  union {
    struct cmsghdr header;
    char space[CMSG_SPACE(sizeof(int))];
  } control;
  memset(&control, 0, sizeof control);
  struct msghdr message = {.msg_iov = &data, .msg_iovlen = 1, .msg_control = &control,
                           .msg_controllen = sizeof control};
  struct cmsghdr *header = CMSG_FIRSTHDR(&message);
  header->cmsg_level = SOL_SOCKET;
  header->cmsg_type = SCM_RIGHTS;
  header->cmsg_len = CMSG_LEN(sizeof(int));
  memcpy(CMSG_DATA(header), &fd, sizeof fd);
  if (sendmsg(socket, &message, 0) != 1) exit(4);
}

// The descriptor that arrives on `socket`, which is nonblocking: wait for it
// with poll, then receive it.
static int receive_fd(int socket) {
  struct pollfd ready = {.fd = socket, .events = POLLIN};
  if (poll(&ready, 1, -1) != 1) exit(4);
  char byte = 0;
  struct iovec data = {&byte, 1};
  union {
    struct cmsghdr header;
    char space[CMSG_SPACE(sizeof(int))];
  } control;
  memset(&control, 0, sizeof control);
  struct msghdr message = {.msg_iov = &data, .msg_iovlen = 1, .msg_control = &control,
                           .msg_controllen = sizeof control};
  if (recvmsg(socket, &message, MSG_CMSG_CLOEXEC) != 1) exit(4);
  struct cmsghdr *header = CMSG_FIRSTHDR(&message);
  if (!header || header->cmsg_type != SCM_RIGHTS) exit(4);
  int fd;
  memcpy(&fd, CMSG_DATA(header), sizeof fd);
  return fd;
}

// Read the two renames' four events from `fd` into `cookies`: whether they
// are f0 -> f1 and f1 -> f2, each pair with one nonzero cookie.
static int read_renames(int fd, uint32_t cookies[2]) {
  char buffer[4 * EVENT] __attribute__((aligned(8)));
  if (read(fd, buffer, sizeof buffer) != 4 * EVENT) return 0;
  const char *names[4] = {"f0", "f1", "f1", "f2"};
  const uint32_t masks[4] = {IN_MOVED_FROM, IN_MOVED_TO, IN_MOVED_FROM, IN_MOVED_TO};
  uint32_t seen[4];
  for (int i = 0; i < 4; i++) {
    const struct inotify_event *event = (const struct inotify_event *)(buffer + i * EVENT);
    if (!(event->mask & masks[i]) || event->len != 16 || strcmp(event->name, names[i]) != 0 ||
        event->cookie == 0) {
      return 0;
    }
    seen[i] = event->cookie;
  }
  cookies[0] = seen[0];
  cookies[1] = seen[2];
  return seen[0] == seen[1] && seen[2] == seen[3] && seen[0] != seen[2];
}

static void report(const char *what, int ok, int *all) {
  printf("%s: %s\n", what, ok ? "yes" : "no");
  *all = *all && ok;
}

int main(void) {
  int all = 1;
  if (!mkdtemp(dir)) return 1;
  char first[64];
  snprintf(first, sizeof first, "%s/f0", dir);
  int fd = open(first, O_CREAT | O_WRONLY | O_CLOEXEC, 0644);
  if (fd < 0 || close(fd) != 0) return 1;
  int self[2], child_pair[2], go[2], answer[2];
  if (socketpair(AF_UNIX, SOCK_STREAM | SOCK_NONBLOCK | SOCK_CLOEXEC, 0, self) != 0 ||
      socketpair(AF_UNIX, SOCK_STREAM | SOCK_NONBLOCK | SOCK_CLOEXEC, 0, child_pair) != 0 ||
      pipe(go) != 0 || pipe(answer) != 0) {
    return 1;
  }
  int a = watch();
  send_fd(self[0], watch());
  int r = receive_fd(self[1]);

  // Nothing buffered for the child to print again if it exits early.
  fflush(stdout);
  pid_t child = fork();
  if (child < 0) return 1;
  if (child == 0) {
    int c = receive_fd(child_pair[1]);
    char byte = 'r';
    if (write(answer[1], &byte, 1) != 1 || read(go[0], &byte, 1) != 1) _exit(1);
    uint32_t cookies[2] = {0, 0};
    if (!read_renames(c, cookies)) cookies[0] = cookies[1] = 0;
    _exit(write(answer[1], cookies, sizeof cookies) == sizeof cookies ? 0 : 1);
  }
  // C is created after the fork, so the child holds it only by receiving it.
  int c = watch();
  send_fd(child_pair[0], c);
  close(c);
  char byte = 0;
  if (read(answer[0], &byte, 1) != 1 || byte != 'r') return 5;

  rename_in_dir("f0", "f1");
  rename_in_dir("f1", "f2");
  uint32_t reference[2], received[2], child_cookies[2] = {0, 0};
  report("the reference instance reads both renames", read_renames(a, reference), &all);
  report("a descriptor received over SCM_RIGHTS agrees",
         read_renames(r, received) && received[0] == reference[0] && received[1] == reference[1],
         &all);
  int status = 0;
  if (write(go[1], &byte, 1) != 1 ||
      read(answer[0], child_cookies, sizeof child_cookies) != sizeof child_cookies ||
      waitpid(child, &status, 0) != child || !WIFEXITED(status) || WEXITSTATUS(status) != 0) {
    return 6;
  }
  report("a descriptor another process received agrees",
         child_cookies[0] == reference[0] && child_cookies[1] == reference[1], &all);

  char last[64];
  snprintf(last, sizeof last, "%s/f2", dir);
  unlink(last);
  rmdir(dir);
  return all ? 0 : 9;
}
