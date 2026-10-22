/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * A guest that accepts its own loopback TCP connections. The parent binds a
 * listener to 127.0.0.1 port 0 and, in each round, forks a client that
 * connects, waits for the server's "go", sends "ping" and reads the echo. The
 * parent accepts the connection, writes "go", waits in poll(2) until the ping
 * is readable, reads it, echoes it and shuts down its sending side.
 *
 * The server polls before it reads, and nothing sleeps, because under
 * `hermit record` an accepted socket is physically nonblocking, so a read that
 * would block returns EAGAIN instead of waiting (see
 * https://github.com/rrnewton/hermit/issues/3882). With the poll, every read
 * finds its data queued and the output does not depend on how the two
 * processes interleave. The rounds:
 *
 *   accept:    accept(2) with a peer address buffer; a blocking read;
 *   accept4:   accept4(2) with SOCK_CLOEXEC and no address buffer;
 *   truncated: accept(2) with a 4-byte address capacity: Linux copies 4
 *              bytes, leaves the rest of the buffer alone and reports the
 *              full length;
 *   nonblock:  accept4(2) with SOCK_NONBLOCK | SOCK_CLOEXEC: a read before
 *              "go" fails with EAGAIN, and the server retries until the ping
 *              arrives.
 *
 * In the accept and accept4 rounds a forked child execs this program, which
 * opens /dev/null and reports whether it got the accepted descriptor's number:
 * it must for SOCK_CLOEXEC, which closed that descriptor at exec, and must not
 * otherwise. Replay serves the guest's reads of its descriptor flags from the
 * log, but the exec'd child's open really allocates a descriptor, so a wrong
 * close-on-exec flag on replay's stand-in makes that open diverge.
 *
 * A last round uses a nonblocking listener with no client: accept4 fails with
 * EAGAIN, and with EINVAL for unknown flags.
 *
 * No line includes a port, since Linux picks the client's port. Exits 0 only
 * if every step got what Linux promises.
 */

#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif

#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <netinet/in.h>
#include <netinet/tcp.h>
#include <poll.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/wait.h>
#include <unistd.h>

static const char* errno_name(int error) {
  switch (error) {
    case EAGAIN:
      return "EAGAIN";
    case EINVAL:
      return "EINVAL";
    default:
      return "other";
  }
}

/* Connects to the listener, waits for "go", sends "ping" and expects it echoed back. */
static int client(const struct sockaddr_in* server) {
  int fd = socket(AF_INET, SOCK_STREAM, 0);
  if (fd < 0 ||
      connect(fd, (const struct sockaddr*)server, sizeof *server) != 0) {
    perror("client connect");
    return 1;
  }
  char go[2];
  ssize_t got_go = read(fd, go, sizeof go);
  char buf[8];
  ssize_t sent = write(fd, "ping", 4);
  ssize_t got = read(fd, buf, sizeof buf);
  printf("client: go=%zd sent=%zd echo=%.*s\n", got_go, sent,
         got > 0 ? (int)got : 0, buf);
  fflush(stdout);
  close(fd);
  return got_go == 2 && memcmp(go, "go", 2) == 0 && sent == 4 && got == 4 &&
          memcmp(buf, "ping", 4) == 0
      ? 0
      : 1;
}

/* Waits until `conn` has data to read or has hung up. */
static int wait_readable(int conn) {
  struct pollfd pfd = {.fd = conn, .events = POLLIN};
  return poll(&pfd, 1, -1) == 1 ? 0 : -1;
}

/*
 * Echoes one message on an accepted connection; returns the bytes read. A
 * nonblocking connection retries a read that fails with EAGAIN.
 */
static ssize_t echo(int conn, int nonblocking) {
  int one = 1;
  if (setsockopt(conn, IPPROTO_TCP, TCP_NODELAY, &one, sizeof one) != 0 ||
      write(conn, "go", 2) != 2) {
    perror("go");
    return -1;
  }
  char buf[8];
  ssize_t got;
  for (;;) {
    if (wait_readable(conn) != 0) {
      perror("poll");
      return -1;
    }
    got = read(conn, buf, sizeof buf);
    if (!(nonblocking && got < 0 && errno == EAGAIN)) {
      break;
    }
  }
  if (got <= 0 || write(conn, buf, got) != got ||
      shutdown(conn, SHUT_WR) != 0) {
    fprintf(stderr, "echo: got=%zd errno=%s\n", got, strerror(errno));
    return -1;
  }
  return got;
}

static int wait_for(pid_t pid) {
  int status = 0;
  if (waitpid(pid, &status, 0) != pid) {
    perror("waitpid");
    return 1;
  }
  return WIFEXITED(status) && WEXITSTATUS(status) == 0 ? 0 : 1;
}

static pid_t spawn_client(const struct sockaddr_in* server) {
  fflush(stdout);
  pid_t pid = fork();
  if (pid == 0) {
    _exit(client(server));
  }
  if (pid < 0) {
    perror("fork");
  }
  return pid;
}

/*
 * Forks and execs this program as an exec child that opens /dev/null and
 * checks whether it reused descriptor `conn`; `expect_reuse` says whether it
 * should. Returns 0 if it did as expected.
 */
static int exec_child_check(const char* self, int conn, int expect_reuse) {
  char conn_arg[16];
  snprintf(conn_arg, sizeof conn_arg, "%d", conn);
  fflush(stdout);
  pid_t pid = fork();
  if (pid == 0) {
    execl(self, self, "exec-child", conn_arg, expect_reuse ? "reuse" : "fresh",
          (char*)NULL);
    perror("execl");
    _exit(127);
  }
  if (pid < 0) {
    perror("fork");
    return 1;
  }
  return wait_for(pid);
}

/* The exec'd side of exec_child_check. */
static int exec_child(const char* conn_arg, const char* expect) {
  int conn = atoi(conn_arg);
  int fd = open("/dev/null", O_RDONLY);
  const char* got = fd == conn ? "reuse" : "fresh";
  printf("exec child: open %s\n", got);
  fflush(stdout);
  return strcmp(got, expect) == 0 ? 0 : 1;
}

static int accept_round(
    const char* self,
    int listener,
    const struct sockaddr_in* server) {
  pid_t pid = spawn_client(server);
  if (pid < 0) {
    return 1;
  }
  struct sockaddr_in peer;
  memset(&peer, 0, sizeof peer);
  socklen_t peer_len = sizeof peer;
  int conn = accept(listener, (struct sockaddr*)&peer, &peer_len);
  ssize_t got = conn < 0 ? -1 : echo(conn, 0);
  int failed = wait_for(pid);
  char host[INET_ADDRSTRLEN] = "";
  inet_ntop(AF_INET, &peer.sin_addr, host, sizeof host);
  printf("server accept: echoed=%zd family=%d len=%u host=%s\n", got,
         peer.sin_family, (unsigned)peer_len, host);
  if (conn >= 0) {
    failed |= exec_child_check(self, conn, 0);
    close(conn);
  }
  return failed || got != 4 || peer.sin_family != AF_INET ||
      peer_len != sizeof peer || strcmp(host, "127.0.0.1") != 0;
}

static int accept4_round(
    const char* self,
    int listener,
    const struct sockaddr_in* server) {
  pid_t pid = spawn_client(server);
  if (pid < 0) {
    return 1;
  }
  int conn = accept4(listener, NULL, NULL, SOCK_CLOEXEC);
  ssize_t got = conn < 0 ? -1 : echo(conn, 0);
  int failed = wait_for(pid);
  int cloexec = conn < 0 ? -1 : (fcntl(conn, F_GETFD) & FD_CLOEXEC) != 0;
  printf("server accept4: echoed=%zd cloexec=%d\n", got, cloexec);
  if (conn >= 0) {
    failed |= exec_child_check(self, conn, 1);
    close(conn);
  }
  return failed || got != 4 || cloexec != 1;
}

static int truncated_round(int listener, const struct sockaddr_in* server) {
  pid_t pid = spawn_client(server);
  if (pid < 0) {
    return 1;
  }
  unsigned char buf[sizeof(struct sockaddr_in)];
  memset(buf, 0xAA, sizeof buf);
  socklen_t len = 4;
  int conn = accept(listener, (struct sockaddr*)buf, &len);
  ssize_t got = conn < 0 ? -1 : echo(conn, 0);
  int failed = wait_for(pid);
  unsigned family = buf[0] | (buf[1] << 8);
  int untouched = 1;
  for (size_t i = 4; i < sizeof buf; i++) {
    untouched &= buf[i] == 0xAA;
  }
  printf("server truncated: echoed=%zd family=%u len=%u untouched=%d\n", got,
         family, (unsigned)len, untouched);
  if (conn >= 0) {
    close(conn);
  }
  return failed || got != 4 || family != AF_INET ||
      len != sizeof(struct sockaddr_in) || !untouched;
}

static int nonblock_round(int listener, const struct sockaddr_in* server) {
  pid_t pid = spawn_client(server);
  if (pid < 0) {
    return 1;
  }
  int conn = accept4(listener, NULL, NULL, SOCK_NONBLOCK | SOCK_CLOEXEC);
  char early[8];
  /* The client waits for "go", so nothing can be queued yet. */
  ssize_t early_got = conn < 0 ? -2 : read(conn, early, sizeof early);
  int early_errno = early_got < 0 ? errno : 0;
  int nonblock = conn < 0 ? -1 : (fcntl(conn, F_GETFL) & O_NONBLOCK) != 0;
  ssize_t got = conn < 0 ? -1 : echo(conn, 1);
  int failed = wait_for(pid);
  printf("server nonblock: early=%zd errno=%s nonblock=%d echoed=%zd\n",
         early_got, errno_name(early_errno), nonblock, got);
  if (conn >= 0) {
    close(conn);
  }
  return failed || early_got != -1 || early_errno != EAGAIN || nonblock != 1 ||
      got != 4;
}

static int error_round(void) {
  int listener = socket(AF_INET, SOCK_STREAM | SOCK_NONBLOCK, 0);
  struct sockaddr_in addr;
  memset(&addr, 0, sizeof addr);
  addr.sin_family = AF_INET;
  addr.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
  if (listener < 0 ||
      bind(listener, (struct sockaddr*)&addr, sizeof addr) != 0 ||
      listen(listener, 1) != 0) {
    perror("error listener");
    return 1;
  }
  int empty = accept4(listener, NULL, NULL, 0);
  int empty_errno = empty < 0 ? errno : 0;
  int bad = accept4(listener, NULL, NULL, 0x12345);
  int bad_errno = bad < 0 ? errno : 0;
  printf("server errors: empty=%d errno=%s badflags=%d errno=%s\n", empty,
         errno_name(empty_errno), bad, errno_name(bad_errno));
  close(listener);
  return empty != -1 || empty_errno != EAGAIN || bad != -1 ||
      bad_errno != EINVAL;
}

int main(int argc, char** argv) {
  if (argc == 4 && strcmp(argv[1], "exec-child") == 0) {
    return exec_child(argv[2], argv[3]);
  }
  int listener = socket(AF_INET, SOCK_STREAM, 0);
  struct sockaddr_in server;
  memset(&server, 0, sizeof server);
  server.sin_family = AF_INET;
  server.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
  socklen_t server_len = sizeof server;
  if (listener < 0 ||
      bind(listener, (struct sockaddr*)&server, sizeof server) != 0 ||
      listen(listener, 2) != 0 ||
      getsockname(listener, (struct sockaddr*)&server, &server_len) != 0) {
    fprintf(stderr, "listener: %s\n", strerror(errno));
    return 2;
  }
  int failed = accept_round(argv[0], listener, &server);
  failed |= accept4_round(argv[0], listener, &server);
  failed |= truncated_round(listener, &server);
  failed |= nonblock_round(listener, &server);
  /* The first listener stays open through the error round. Hermit gives a
   * port-0 bind a deterministic port and reuses a port as soon as its socket
   * closes, but the server's side of each connection above is in TIME_WAIT
   * on that port, so a second listener there would fail with EADDRINUSE
   * (https://github.com/rrnewton/hermit/issues/1819). */
  failed |= error_round();
  close(listener);
  return failed ? 16 : 0;
}
