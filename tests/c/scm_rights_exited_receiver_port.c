/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// A bound UDP socket rides an SCM_RIGHTS message queued to a datagram
// socket that a child holds; the child exits without receiving it, and the
// parent then binds a new socket to port 0. Linux frees the child's sockets
// at its exit (`exit_files`), so the queued message, and the bound socket
// it held, go with it, and the port is free again once the parent's last
// descriptor of it goes (https://github.com/rrnewton/hermit/issues/4930).
//
// Modes, each printing one line:
//   alone      the child alone holds the receiving socket; the parent
//              closes its own descriptor of the bound socket after the
//              child's exit;
//   shared     the parent keeps the receiving socket too, and closes it
//              after the child's exit;
//   inherited  the child inherits the bound socket itself and exits holding
//              it; the parent closes its own descriptor after the child's
//              exit;
//   threaded   as alone, but two threads of the child hold its table: one
//              exits first, then the other ends the child with exit_group.
// The address to bind is the second argument (a loopback address of the
// test's own, so that concurrent runs never contend for a port).

#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <netinet/in.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/wait.h>
#include <unistd.h>

static const char* address = "127.0.0.1";

static void die(const char* what) {
  fprintf(stderr, "exited-receiver-port: %s: %s\n", what, strerror(errno));
  exit(1);
}

static void send_fd(int socket, int fd) {
  char byte = 'x';
  char control[CMSG_SPACE(sizeof(int))];
  memset(control, 0, sizeof(control));
  struct iovec iov = {.iov_base = &byte, .iov_len = 1};
  struct msghdr message = {
      .msg_iov = &iov,
      .msg_iovlen = 1,
      .msg_control = control,
      .msg_controllen = sizeof(control),
  };
  struct cmsghdr* header = CMSG_FIRSTHDR(&message);
  header->cmsg_level = SOL_SOCKET;
  header->cmsg_type = SCM_RIGHTS;
  header->cmsg_len = CMSG_LEN(sizeof(int));
  memcpy(CMSG_DATA(header), &fd, sizeof(fd));
  if (sendmsg(socket, &message, 0) != 1) {
    die("sendmsg");
  }
}

static int bound_udp(unsigned short* port) {
  int s = socket(AF_INET, SOCK_DGRAM, 0);
  if (s < 0) {
    die("socket");
  }
  struct sockaddr_in name = {.sin_family = AF_INET, .sin_port = 0};
  if (inet_pton(AF_INET, address, &name.sin_addr) != 1) {
    die("inet_pton");
  }
  if (bind(s, (struct sockaddr*)&name, sizeof(name)) != 0) {
    die("bind");
  }
  socklen_t length = sizeof(name);
  if (getsockname(s, (struct sockaddr*)&name, &length) != 0) {
    die("getsockname");
  }
  *port = ntohs(name.sin_port);
  return s;
}

// A child that closes `close_first` (if not -1), waits for a byte on
// `go[0]`, and exits holding whatever else it inherited.
static pid_t exiting_child(int close_first, int go[2]) {
  pid_t child = fork();
  if (child < 0) {
    die("fork");
  }
  if (child == 0) {
    if (close_first >= 0) {
      close(close_first);
    }
    close(go[1]);
    char byte;
    if (read(go[0], &byte, 1) != 1) {
      _exit(2);
    }
    _exit(0);
  }
  close(go[0]);
  return child;
}

static void* thread_exits(void* unused) {
  (void)unused;
  return NULL;
}

// As exiting_child, with a second thread that exits first: the child's
// table has two holders, and its last one ends the child.
static pid_t threaded_exiting_child(int close_first, int go[2]) {
  pid_t child = fork();
  if (child < 0) {
    die("fork");
  }
  if (child == 0) {
    close(close_first);
    close(go[1]);
    pthread_t thread;
    if (pthread_create(&thread, NULL, thread_exits, NULL) != 0 ||
        pthread_join(thread, NULL) != 0) {
      _exit(3);
    }
    char byte;
    if (read(go[0], &byte, 1) != 1) {
      _exit(2);
    }
    _exit(0);
  }
  close(go[0]);
  return child;
}

static void finish(pid_t child, int go) {
  if (write(go, "g", 1) != 1) {
    die("write go");
  }
  int status = 0;
  if (waitpid(child, &status, 0) != child) {
    die("waitpid");
  }
  if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) {
    fprintf(stderr, "exited-receiver-port: child status %d\n", status);
    exit(1);
  }
  close(go);
}

int main(int argc, char** argv) {
  const char* mode = argc > 1 ? argv[1] : "alone";
  if (argc > 2) {
    address = argv[2];
  }
  int pair[2];
  int go[2];
  if (socketpair(AF_UNIX, SOCK_DGRAM, 0, pair) != 0 || pipe(go) != 0) {
    die("socketpair");
  }
  unsigned short first;
  unsigned short second;
  if (strcmp(mode, "inherited") == 0) {
    int bound = bound_udp(&first);
    pid_t child = exiting_child(-1, go);
    finish(child, go[1]);
    close(bound);
  } else {
    int shared = strcmp(mode, "shared") == 0;
    pid_t child = strcmp(mode, "threaded") == 0
                      ? threaded_exiting_child(pair[0], go)
                      : exiting_child(pair[0], go);
    if (!shared) {
      close(pair[1]);
    }
    int bound = bound_udp(&first);
    send_fd(pair[0], bound);  // queued on pair[1]
    close(bound);
    finish(child, go[1]);
    if (shared) {
      close(pair[1]);
    }
  }
  int next = bound_udp(&second);
  printf("%s: the next bind %s the first's port\n", mode,
         second == first ? "reuses" : "does not reuse");
  close(next);
  close(pair[0]);
  return 0;
}
