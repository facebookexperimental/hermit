/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * A guest whose main thread accepts loopback TCP connections while a second
 * thread opens and closes /dev/null in a loop, so the two threads keep
 * changing the descriptor table at the same time. The main thread forks 20
 * clients one after another; each connects, sends "ping" and reads the echo.
 * The main thread accepts, polls until the ping is readable, echoes it and
 * closes the connection. It prints one line: how many of the 20 echoes
 * completed.
 *
 * Replay serves each accept from the recording and puts a stand-in at the
 * recorded descriptor number, refusing to continue if that number is not the
 * lowest free one at that point. Which number is free depends on where the
 * accept runs among the other thread's open and close calls, so replay must
 * place it the same way every time: every replay of one recording must end
 * the same way. The server polls before it reads, and nothing sleeps, so the
 * guest's output does not depend on how its processes interleave under
 * `hermit record`, where an accepted socket is physically nonblocking.
 *
 * With the argument "scm-rights", the main thread first passes the listener
 * to itself over an AF_UNIX socketpair and accepts through the descriptor it
 * received. Detcore does not track a received descriptor, so replay must
 * recognise the accept by the call alone.
 */

#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif

#include <arpa/inet.h>
#include <fcntl.h>
#include <netinet/in.h>
#include <poll.h>
#include <pthread.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/wait.h>
#include <unistd.h>

enum { CONNECTIONS = 20, OPENS = 2000 };

static void* open_and_close(void* arg) {
  (void)arg;
  for (int i = 0; i < OPENS; i++) {
    int fd = open("/dev/null", O_RDONLY);
    if (fd >= 0) {
      close(fd);
    }
  }
  return NULL;
}

/* Connects, sends "ping" and expects it echoed back. */
static int client(const struct sockaddr_in* server) {
  int fd = socket(AF_INET, SOCK_STREAM, 0);
  if (fd < 0 ||
      connect(fd, (const struct sockaddr*)server, sizeof *server) != 0) {
    return 1;
  }
  char buf[8];
  ssize_t sent = write(fd, "ping", 4);
  ssize_t got = read(fd, buf, sizeof buf);
  close(fd);
  return sent == 4 && got == 4 && memcmp(buf, "ping", 4) == 0 ? 0 : 1;
}

/*
 * Sends `listener` over a new socketpair and returns the descriptor received
 * for it, closing the original and the socketpair; -1 on failure.
 */
static int pass_over_scm_rights(int listener) {
  int channel[2];
  if (socketpair(AF_UNIX, SOCK_STREAM, 0, channel) != 0) {
    return -1;
  }
  char tag = 'L';
  struct iovec iov = {.iov_base = &tag, .iov_len = 1};
  union {
    struct cmsghdr alignment;
    unsigned char bytes[CMSG_SPACE(sizeof(int))];
  } control;
  memset(&control, 0, sizeof control);
  struct msghdr message = {
      .msg_iov = &iov,
      .msg_iovlen = 1,
      .msg_control = control.bytes,
      .msg_controllen = sizeof control.bytes,
  };
  struct cmsghdr* header = CMSG_FIRSTHDR(&message);
  header->cmsg_level = SOL_SOCKET;
  header->cmsg_type = SCM_RIGHTS;
  header->cmsg_len = CMSG_LEN(sizeof(int));
  memcpy(CMSG_DATA(header), &listener, sizeof listener);
  int received = -1;
  if (sendmsg(channel[0], &message, 0) == 1) {
    memset(&control, 0, sizeof control);
    header = NULL;
    if (recvmsg(channel[1], &message, 0) == 1) {
      header = CMSG_FIRSTHDR(&message);
    }
    if (header && header->cmsg_level == SOL_SOCKET &&
        header->cmsg_type == SCM_RIGHTS &&
        header->cmsg_len == CMSG_LEN(sizeof(int))) {
      memcpy(&received, CMSG_DATA(header), sizeof received);
    }
  }
  close(channel[0]);
  close(channel[1]);
  if (received >= 0) {
    close(listener);
  }
  return received;
}

/* Accepts one connection and echoes one message; returns 1 on success. */
static int serve_one(int listener) {
  int conn = accept(listener, NULL, NULL);
  if (conn < 0) {
    return 0;
  }
  struct pollfd pfd = {.fd = conn, .events = POLLIN};
  char buf[8];
  ssize_t got = poll(&pfd, 1, -1) == 1 ? read(conn, buf, sizeof buf) : -1;
  int ok = got == 4 && write(conn, buf, got) == got;
  close(conn);
  return ok;
}

int main(int argc, char** argv) {
  int listener = socket(AF_INET, SOCK_STREAM, 0);
  struct sockaddr_in server;
  memset(&server, 0, sizeof server);
  server.sin_family = AF_INET;
  server.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
  socklen_t server_len = sizeof server;
  if (listener < 0 ||
      bind(listener, (struct sockaddr*)&server, sizeof server) != 0 ||
      listen(listener, CONNECTIONS) != 0 ||
      getsockname(listener, (struct sockaddr*)&server, &server_len) != 0) {
    perror("listener");
    return 2;
  }
  if (argc > 1 && strcmp(argv[1], "scm-rights") == 0) {
    listener = pass_over_scm_rights(listener);
    if (listener < 0) {
      perror("scm-rights");
      return 2;
    }
  }
  pthread_t opener;
  if (pthread_create(&opener, NULL, open_and_close, NULL) != 0) {
    perror("pthread_create");
    return 2;
  }
  int echoed = 0;
  int clients_ok = 1;
  for (int i = 0; i < CONNECTIONS; i++) {
    fflush(stdout);
    pid_t pid = fork();
    if (pid == 0) {
      _exit(client(&server));
    }
    if (pid < 0) {
      perror("fork");
      return 2;
    }
    echoed += serve_one(listener);
    int status = 0;
    clients_ok &= waitpid(pid, &status, 0) == pid && WIFEXITED(status) &&
        WEXITSTATUS(status) == 0;
  }
  pthread_join(opener, NULL);
  printf("threads: echoed=%d clients_ok=%d\n", echoed, clients_ok);
  return echoed == CONNECTIONS && clients_ok ? 0 : 16;
}
