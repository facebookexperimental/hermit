/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Loopback TCP servers whose blocking accept(2) `hermit record` runs in the
 * caller's turn, one nonblocking attempt at a time. The first argument names
 * the case; each prints one line, which the tests compare with what Linux
 * prints when the same case runs outside Hermit.
 *
 * Waiting:
 *   "late-connector": a thread sleeps, then connects, sends "ping" and reads
 *     the echo, while the main thread accepts with a plain blocking call.
 *   "unwakeable": a blocking accept that nothing ever connects to. It never
 *     prints; the test expects `--record-timeout` to end the recording.
 *   "reset": a client connects and resets the connection (SO_LINGER 0) before
 *     the server accepts.
 *   "sibling-setfl": a thread sets O_NONBLOCK on the listener while the main
 *     thread waits in accept, then connects. Linux reads O_NONBLOCK at accept
 *     entry, so the wait goes on; F_GETFL shows the sibling's value.
 *   "timeout-dup": SO_RCVTIMEO set through a dup of the listener; the accept on
 *     the original fails with EAGAIN at the deadline.
 *   "signal-restart", "signal-norestart", "signal-timed-restart",
 *   "signal-timed-norestart": a thread interrupts the waiting accept with
 *     SIGUSR1, handled with or without SA_RESTART, on a listener with or
 *     without SO_RCVTIMEO, then connects. Linux restarts only an untimed accept
 *     whose handler has SA_RESTART (signal(7)).
 *   "signal-unbounded-restart": as "signal-restart", on a listener with
 *     SO_RCVTIMEO {LONG_MAX, 0}, which Linux stores as no timeout (it reads
 *     back as zero), so the accept still restarts.
 *   "shutdown": a thread calls shutdown(SHUT_RD) on the listener while the
 *     main thread waits; the accept fails with EINVAL.
 *   "nonblocking": accepts on an O_NONBLOCK listener, first with nothing
 *     queued (EAGAIN), then with a queued connection.
 *
 * Cancellation (with HERMIT_TEST_ACCEPT_BRACKET_FAULT set by the test):
 *   "bracket-kill": a forked child accepts on the parent's listener and is
 *     killed (by the parent, and under the fault hook by Hermit, while the
 *     listener is temporarily nonblocking). The parent then passes the listener
 *     over SCM_RIGHTS, so Hermit no longer runs its accepts in turn, and its
 *     own accept must still wait for a delayed connector: that accept fails
 *     with EAGAIN if the shared description was left nonblocking. (F_GETFL is
 *     answered from Hermit's logical flags, so it alone cannot see a leak.)
 *   "accept-once": one blocking accept of a delayed forked client.
 *
 * Export while an accept waits (record refuses these; Linux accepts):
 *   "export-sendmsg", "export-sendmmsg-second", "export-failed-send": while
 *   the main thread waits in accept, a thread passes the listener over
 *   SCM_RIGHTS with sendmsg, in the second message of a sendmmsg, or with a
 *   sendmsg that fails with EPIPE, then connects.
 *
 * Closing a listener an accept waits on:
 *   "close-one-left": two threads wait in accept; one client connects; once
 *     one accept has returned, the main thread closes the listener while the
 *     other still waits (record refuses the close), then connects again.
 *   "close-both-done": the same, but the close comes after both accepts
 *     returned, so record must allow it.
 *   "exec-sharer-close": a process sharing the descriptor table (CLONE_FILES,
 *     not a thread) execs this program as "close-helper", which closes its own
 *     copy of the listener; then the waiting accept is connected to.
 *   "exec-sharer-then-close": as above, then the main thread closes the
 *     listener while its accept still waits in another thread (refused).
 *   "failed-exec-close": the sharer's exec fails, so it still shares the
 *     waiter's table, and its close of the listener must be refused.
 *   "close-helper FD": closes FD and prints the result.
 *
 * Entry and the first attempt (each with its connection queued first):
 *   "export-at-entry N": a thread yields N times, then passes the listener over
 *     SCM_RIGHTS, while the main thread accepts. The test sweeps N so that one
 *     value lands the export after the accept's entry and before its first
 *     attempt, which record must refuse rather than bracket.
 *   "export-write-only": passes the listener in an SCM_RIGHTS control buffer
 *     the process can write but not read (Linux reads it all the same), then
 *     accepts.
 *   "scratch-tight": accept with the stack pointer 128 bytes above an
 *     inaccessible page. "scratch-alias": accept with the address length at
 *     RSP-152 and a capacity of 0, so the address must stay unwritten. Linux
 *     uses no memory below RSP.
 *   "worker-queued": a thread other than the leader, sharing its table,
 *     accepts.
 *
 * Receive timeouts:
 *   "negative-timeout": SO_RCVTIMEO with a negative second count is an
 *     immediate timeout, so the accept fails at once with EAGAIN.
 *   "timeout-set-by-child": a forked child sets SO_RCVTIMEO on the listener it
 *     shares with its parent; the parent's accept then times out.
 */

#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif

#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <netinet/in.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <sys/wait.h>
#include <unistd.h>

enum { DELAY_US = 50000 };

static struct sockaddr_in server;
static int listener = -1;

/* A loopback TCP listener on a free port, which it records in `server`; -1 on
 * failure. */
static int make_listener(int backlog) {
  int fd = socket(AF_INET, SOCK_STREAM, 0);
  memset(&server, 0, sizeof server);
  server.sin_family = AF_INET;
  server.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
  socklen_t len = sizeof server;
  if (fd < 0 || bind(fd, (struct sockaddr*)&server, sizeof server) != 0 ||
      getsockname(fd, (struct sockaddr*)&server, &len) != 0 ||
      listen(fd, backlog) != 0) {
    return -1;
  }
  return fd;
}

static const char* outcome(int result, int error) {
  return result >= 0 ? "fd" : strerrorname_np(error);
}

static int nonblocking(int fd) {
  return (fcntl(fd, F_GETFL) & O_NONBLOCK) != 0;
}

static int set_receive_timeout(int fd, long usec) {
  struct timeval timeout = {.tv_sec = usec / 1000000, .tv_usec = usec % 1000000};
  return setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof timeout);
}

/* Connects, sends "ping" and expects it echoed; 0 on success. */
static int client(void) {
  int fd = socket(AF_INET, SOCK_STREAM, 0);
  if (fd < 0 ||
      connect(fd, (const struct sockaddr*)&server, sizeof server) != 0) {
    return 1;
  }
  char buf[8];
  ssize_t sent = write(fd, "ping", 4);
  ssize_t got = read(fd, buf, sizeof buf);
  close(fd);
  return sent == 4 && got == 4 && memcmp(buf, "ping", 4) == 0 ? 0 : 1;
}

/* Reads one message from `conn`, echoes it and closes `conn`; returns the
 * bytes echoed, or -1. */
static ssize_t echo(int conn) {
  char buf[8];
  ssize_t got = read(conn, buf, sizeof buf);
  if (got > 0 && write(conn, buf, got) != got) {
    got = -1;
  }
  close(conn);
  return got;
}

/* Forks a client that sleeps, then runs client(); returns its pid. */
static pid_t fork_delayed_client(void) {
  fflush(stdout);
  pid_t pid = fork();
  if (pid == 0) {
    usleep(DELAY_US);
    _exit(client());
  }
  return pid;
}

static int reaped_ok(pid_t pid) {
  int status = 0;
  return pid > 0 && waitpid(pid, &status, 0) == pid && WIFEXITED(status) &&
      WEXITSTATUS(status) == 0;
}

/* One blocking accept on `listener`, echoed; prints and returns the result. */
struct accept_result {
  int result;
  int error;
  ssize_t echoed;
};

static struct accept_result accept_and_echo(int fd) {
  struct accept_result r;
  r.result = accept(fd, NULL, NULL);
  r.error = errno;
  r.echoed = r.result >= 0 ? echo(r.result) : -1;
  return r;
}

/* ---- Waiting ---- */

static void* sleep_then_client(void* result) {
  usleep(DELAY_US);
  *(int*)result = client();
  return NULL;
}

static int late_connector(void) {
  listener = make_listener(1);
  int helper = -1;
  pthread_t thread;
  if (listener < 0 ||
      pthread_create(&thread, NULL, sleep_then_client, &helper) != 0) {
    return 2;
  }
  struct accept_result r = accept_and_echo(listener);
  pthread_join(thread, NULL);
  printf(
      "late-connector: accept=%s echoed=%zd helper=%d\n",
      outcome(r.result, r.error),
      r.echoed,
      helper);
  return 0;
}

static int unwakeable(void) {
  listener = make_listener(1);
  if (listener < 0) {
    return 2;
  }
  int conn = accept(listener, NULL, NULL);
  printf("unwakeable: accept returned %d\n", conn);
  return 16;
}

static int reset(void) {
  listener = make_listener(1);
  int fd = socket(AF_INET, SOCK_STREAM, 0);
  struct linger linger = {.l_onoff = 1, .l_linger = 0};
  if (listener < 0 || fd < 0 ||
      connect(fd, (const struct sockaddr*)&server, sizeof server) != 0 ||
      setsockopt(fd, SOL_SOCKET, SO_LINGER, &linger, sizeof linger) != 0) {
    return 2;
  }
  close(fd);
  int conn = accept(listener, NULL, NULL);
  int accept_errno = errno;
  char buf[8];
  ssize_t got = conn >= 0 ? read(conn, buf, sizeof buf) : -1;
  int read_errno = errno;
  printf(
      "reset: accept=%s read=%zd %s\n",
      outcome(conn, accept_errno),
      got,
      got < 0 ? strerrorname_np(read_errno) : "-");
  return 0;
}

static void* set_nonblocking_then_client(void* result) {
  usleep(DELAY_US);
  int flags = fcntl(listener, F_GETFL);
  int set = fcntl(listener, F_SETFL, flags | O_NONBLOCK);
  usleep(DELAY_US);
  *(int*)result = set == 0 ? client() : 3;
  return NULL;
}

static int sibling_setfl(void) {
  listener = make_listener(1);
  int helper = -1;
  pthread_t thread;
  if (listener < 0 ||
      pthread_create(&thread, NULL, set_nonblocking_then_client, &helper) !=
          0) {
    return 2;
  }
  struct accept_result r = accept_and_echo(listener);
  pthread_join(thread, NULL);
  printf(
      "sibling-setfl: accept=%s echoed=%zd helper=%d nonblock-after=%d\n",
      outcome(r.result, r.error),
      r.echoed,
      helper,
      nonblocking(listener));
  return 0;
}

static int timeout_dup(void) {
  listener = make_listener(1);
  int copy = listener >= 0 ? dup(listener) : -1;
  if (copy < 0 || set_receive_timeout(copy, 200000) != 0) {
    return 2;
  }
  int first = accept(listener, NULL, NULL);
  int first_errno = errno;
  int second = accept(copy, NULL, NULL);
  int second_errno = errno;
  printf(
      "timeout-dup: original=%s dup=%s\n",
      outcome(first, first_errno),
      outcome(second, second_errno));
  return 0;
}

static volatile sig_atomic_t handled;

static void on_signal(int sig) {
  (void)sig;
  handled++;
}

static pthread_t waiter;

static void* signal_then_client(void* result) {
  usleep(DELAY_US);
  int sent = pthread_kill(waiter, SIGUSR1);
  usleep(DELAY_US);
  *(int*)result = sent == 0 ? client() : 3;
  return NULL;
}

/* Sets SO_RCVTIMEO {LONG_MAX, 0}, which Linux stores as no timeout and reads
 * back as zero; 0 on success. */
static int set_unbounded_receive_timeout(int fd) {
  struct timeval timeout = {.tv_sec = LONG_MAX, .tv_usec = 0};
  struct timeval readback;
  socklen_t len = sizeof readback;
  if (setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof timeout) != 0 ||
      getsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &readback, &len) != 0) {
    return -1;
  }
  return readback.tv_sec == 0 && readback.tv_usec == 0 ? 0 : -1;
}

/* `timed`: 0 for no SO_RCVTIMEO, 1 for 5 s, 2 for {LONG_MAX, 0}. */
static int signal_case(const char* name, int restart, int timed) {
  struct sigaction action;
  memset(&action, 0, sizeof action);
  action.sa_handler = on_signal;
  action.sa_flags = restart ? SA_RESTART : 0;
  listener = make_listener(1);
  if (listener < 0 || sigaction(SIGUSR1, &action, NULL) != 0 ||
      (timed == 1 && set_receive_timeout(listener, 5000000) != 0) ||
      (timed == 2 && set_unbounded_receive_timeout(listener) != 0)) {
    return 2;
  }
  waiter = pthread_self();
  int helper = -1;
  pthread_t thread;
  if (pthread_create(&thread, NULL, signal_then_client, &helper) != 0) {
    return 2;
  }
  struct accept_result r = accept_and_echo(listener);
  if (r.result < 0) {
    /* The interrupted accept returned; take the client that still comes. */
    int conn = accept(listener, NULL, NULL);
    if (conn >= 0) {
      echo(conn);
    }
  }
  pthread_join(thread, NULL);
  printf(
      "%s: accept=%s echoed=%zd handled=%d helper=%d\n",
      name,
      outcome(r.result, r.error),
      r.echoed,
      (int)handled,
      helper);
  return 0;
}

static void* shutdown_listener(void* result) {
  usleep(DELAY_US);
  *(int*)result = shutdown(listener, SHUT_RD);
  return NULL;
}

static int shutdown_case(void) {
  listener = make_listener(1);
  int shut = -1;
  pthread_t thread;
  if (listener < 0 ||
      pthread_create(&thread, NULL, shutdown_listener, &shut) != 0) {
    return 2;
  }
  int conn = accept(listener, NULL, NULL);
  int accept_errno = errno;
  pthread_join(thread, NULL);
  printf("shutdown: shutdown=%d accept=%s\n", shut, outcome(conn, accept_errno));
  return 0;
}

static int nonblocking_case(void) {
  listener = make_listener(1);
  if (listener < 0 ||
      fcntl(listener, F_SETFL, fcntl(listener, F_GETFL) | O_NONBLOCK) != 0) {
    return 2;
  }
  int empty = accept(listener, NULL, NULL);
  int empty_errno = errno;
  int fd = socket(AF_INET, SOCK_STREAM, 0);
  if (fd < 0 ||
      connect(fd, (const struct sockaddr*)&server, sizeof server) != 0) {
    return 2;
  }
  int queued = accept4(listener, NULL, NULL, SOCK_NONBLOCK);
  int queued_errno = errno;
  int conn_nonblock = queued >= 0 ? nonblocking(queued) : -1;
  printf(
      "nonblocking: empty=%s queued=%s conn-nonblock=%d\n",
      outcome(empty, empty_errno),
      outcome(queued, queued_errno),
      conn_nonblock);
  return 0;
}

/* ---- Cancellation ---- */

static int export_listener(int fd);

static int bracket_kill(void) {
  listener = make_listener(1);
  if (listener < 0) {
    return 2;
  }
  fflush(stdout);
  pid_t child = fork();
  if (child == 0) {
    int conn = accept(listener, NULL, NULL);
    _exit(conn >= 0 ? 3 : 4);
  }
  if (child < 0) {
    return 2;
  }
  usleep(DELAY_US);
  kill(child, SIGKILL);
  int status = 0;
  int killed = waitpid(child, &status, 0) == child && WIFSIGNALED(status) &&
      WTERMSIG(status) == SIGKILL;
  int nonblock = nonblocking(listener);
  if (export_listener(listener) != 0) {
    return 2;
  }
  pid_t connector = fork_delayed_client();
  struct accept_result r = accept_and_echo(listener);
  if (r.result < 0) {
    /* Nothing will echo to the connector, which would wait forever. */
    kill(connector, SIGKILL);
  }
  printf(
      "bracket-kill: child-killed=%d nonblock=%d accept=%s echoed=%zd "
      "client=%d\n",
      killed,
      nonblock,
      outcome(r.result, r.error),
      r.echoed,
      reaped_ok(connector));
  return 0;
}

static int accept_once(void) {
  listener = make_listener(1);
  if (listener < 0) {
    return 2;
  }
  pid_t connector = fork_delayed_client();
  struct accept_result r = accept_and_echo(listener);
  printf(
      "accept-once: accept=%s echoed=%zd client=%d\n",
      outcome(r.result, r.error),
      r.echoed,
      reaped_ok(connector));
  return 0;
}

/* ---- Export while an accept waits ---- */

/* Fills `msg` to carry `fd` over SCM_RIGHTS, using `control` and `iov`. */
union control_buffer {
  struct cmsghdr alignment;
  unsigned char bytes[CMSG_SPACE(sizeof(int))];
};

static void carry_rights(
    struct msghdr* msg,
    struct iovec* iov,
    union control_buffer* control,
    int fd) {
  static char tag = 'L';
  iov->iov_base = &tag;
  iov->iov_len = 1;
  memset(msg, 0, sizeof *msg);
  memset(control, 0, sizeof *control);
  msg->msg_iov = iov;
  msg->msg_iovlen = 1;
  msg->msg_control = control->bytes;
  msg->msg_controllen = sizeof control->bytes;
  struct cmsghdr* header = CMSG_FIRSTHDR(msg);
  header->cmsg_level = SOL_SOCKET;
  header->cmsg_type = SCM_RIGHTS;
  header->cmsg_len = CMSG_LEN(sizeof(int));
  memcpy(CMSG_DATA(header), &fd, sizeof fd);
}

/* Passes `fd` over SCM_RIGHTS through a socketpair, then closes the pair. */
static int export_listener(int fd) {
  int pair[2];
  if (socketpair(AF_UNIX, SOCK_STREAM, 0, pair) != 0) {
    return -1;
  }
  struct msghdr msg;
  struct iovec iov;
  union control_buffer control;
  carry_rights(&msg, &iov, &control, fd);
  ssize_t sent = sendmsg(pair[0], &msg, 0);
  close(pair[0]);
  close(pair[1]);
  return sent == 1 ? 0 : -1;
}

enum export_kind { EXPORT_SENDMSG, EXPORT_SENDMMSG_SECOND, EXPORT_FAILED };

struct export_args {
  enum export_kind kind;
  int sent;
  int send_errno;
  int helper;
};

static void* export_then_client(void* arg) {
  struct export_args* args = arg;
  int channel[2];
  usleep(DELAY_US);
  if (socketpair(AF_UNIX, SOCK_STREAM, 0, channel) != 0) {
    args->helper = 3;
    return NULL;
  }
  struct iovec iov[2];
  union control_buffer control;
  if (args->kind == EXPORT_SENDMMSG_SECOND) {
    static char plain = 'P';
    struct mmsghdr messages[2];
    memset(messages, 0, sizeof messages);
    iov[0].iov_base = &plain;
    iov[0].iov_len = 1;
    messages[0].msg_hdr.msg_iov = &iov[0];
    messages[0].msg_hdr.msg_iovlen = 1;
    carry_rights(&messages[1].msg_hdr, &iov[1], &control, listener);
    args->sent = sendmmsg(channel[0], messages, 2, 0);
  } else {
    struct msghdr msg;
    carry_rights(&msg, &iov[0], &control, listener);
    if (args->kind == EXPORT_FAILED) {
      close(channel[1]);
      channel[1] = -1;
    }
    args->sent = (int)sendmsg(channel[0], &msg, MSG_NOSIGNAL);
  }
  args->send_errno = errno;
  close(channel[0]);
  if (channel[1] >= 0) {
    close(channel[1]);
  }
  usleep(DELAY_US);
  args->helper = client();
  return NULL;
}

static int export_case(const char* name, enum export_kind kind) {
  listener = make_listener(1);
  struct export_args args = {.kind = kind, .sent = -1, .helper = -1};
  pthread_t thread;
  if (listener < 0 ||
      pthread_create(&thread, NULL, export_then_client, &args) != 0) {
    return 2;
  }
  struct accept_result r = accept_and_echo(listener);
  pthread_join(thread, NULL);
  printf(
      "%s: sent=%d %s accept=%s echoed=%zd helper=%d\n",
      name,
      args.sent,
      args.sent < 0 ? strerrorname_np(args.send_errno) : "-",
      outcome(r.result, r.error),
      r.echoed,
      args.helper);
  return 0;
}

/* ---- Closing a listener an accept waits on ---- */

static int returned[2];
static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t changed = PTHREAD_COND_INITIALIZER;

static void* accept_and_report(void* slot) {
  int conn = accept(listener, NULL, NULL);
  if (conn >= 0) {
    echo(conn);
  }
  pthread_mutex_lock(&lock);
  returned[(long)slot] = conn >= 0 ? 1 : -1;
  pthread_cond_broadcast(&changed);
  pthread_mutex_unlock(&lock);
  return NULL;
}

static int accepts_returned(void) {
  return (returned[0] != 0) + (returned[1] != 0);
}

static void wait_for_returns(int count) {
  pthread_mutex_lock(&lock);
  while (accepts_returned() < count) {
    pthread_cond_wait(&changed, &lock);
  }
  pthread_mutex_unlock(&lock);
}

static int close_case(const char* name, int both_first) {
  listener = make_listener(2);
  pthread_t threads[2];
  if (listener < 0) {
    return 2;
  }
  for (long i = 0; i < 2; i++) {
    if (pthread_create(&threads[i], NULL, accept_and_report, (void*)i) != 0) {
      return 2;
    }
  }
  usleep(DELAY_US);
  int first = client();
  wait_for_returns(1);
  int second = 0;
  if (both_first) {
    second = client();
    wait_for_returns(2);
  }
  int closed = close(listener);
  if (!both_first) {
    /* Linux keeps the listening socket open for the accept still waiting. */
    second = client();
    wait_for_returns(2);
  }
  for (int i = 0; i < 2; i++) {
    pthread_join(threads[i], NULL);
  }
  printf(
      "%s: close=%d clients=%d,%d accepted=%d,%d\n",
      name,
      closed,
      first,
      second,
      returned[0],
      returned[1]);
  return 0;
}

static char self_path[4096];

/* A process sharing this descriptor table (CLONE_FILES without CLONE_VM or
 * CLONE_THREAD). It execs this program as "close-helper", or, with
 * `bad_exec`, fails to exec and closes the listener itself. */
static pid_t spawn_sharer(int bad_exec) {
  fflush(stdout);
  pid_t pid = (pid_t)syscall(SYS_clone, CLONE_FILES | SIGCHLD, 0, 0, 0, 0);
  if (pid != 0) {
    return pid;
  }
  char fd_arg[16];
  snprintf(fd_arg, sizeof fd_arg, "%d", listener);
  char* args[] = {self_path, "close-helper", fd_arg, NULL};
  if (bad_exec) {
    char* missing[] = {"/nonexistent/hermit-accept-helper", NULL};
    execv(missing[0], missing);
    int exec_errno = errno;
    int closed = close(listener);
    printf(
        "failed-exec: exec=%s close=%d\n",
        strerrorname_np(exec_errno),
        closed);
    fflush(stdout);
    _exit(0);
  }
  execv(self_path, args);
  _exit(5);
}

static int exec_case(const char* name, int bad_exec, int close_after) {
  ssize_t len = readlink("/proc/self/exe", self_path, sizeof self_path - 1);
  listener = make_listener(1);
  pthread_t thread;
  if (len <= 0 || listener < 0 ||
      pthread_create(&thread, NULL, accept_and_report, (void*)0) != 0) {
    return 2;
  }
  self_path[len] = '\0';
  usleep(DELAY_US);
  int sharer_ok = reaped_ok(spawn_sharer(bad_exec));
  char closed[16] = "skipped";
  if (close_after) {
    snprintf(closed, sizeof closed, "%d", close(listener));
  }
  int helper = client();
  wait_for_returns(1);
  pthread_join(thread, NULL);
  printf(
      "%s: sharer=%d close=%s helper=%d accepted=%d\n",
      name,
      sharer_ok,
      closed,
      helper,
      returned[0]);
  return 0;
}

/* ---- Entry and the first attempt ---- */

/* Connects a client to `server` and leaves the connection queued; the
 * descriptor, or -1. */
static int queue_connection(void) {
  int fd = socket(AF_INET, SOCK_STREAM, 0);
  if (fd < 0 ||
      connect(fd, (const struct sockaddr*)&server, sizeof server) != 0) {
    return -1;
  }
  return fd;
}

struct export_at_entry_args {
  int spins;
  int exported;
};

static void* yield_then_export(void* arg) {
  struct export_at_entry_args* args = arg;
  for (int i = 0; i < args->spins; i++) {
    sched_yield();
  }
  args->exported = export_listener(listener);
  return NULL;
}

static int export_at_entry(const char* spins) {
  listener = make_listener(1);
  int connector = listener < 0 ? -1 : queue_connection();
  struct export_at_entry_args args = {.spins = atoi(spins), .exported = -1};
  pthread_t thread;
  if (connector < 0 ||
      pthread_create(&thread, NULL, yield_then_export, &args) != 0) {
    return 2;
  }
  int conn = accept(listener, NULL, NULL);
  int error = errno;
  pthread_join(thread, NULL);
  printf(
      "export-at-entry: accept=%s export=%d\n",
      outcome(conn, error),
      args.exported);
  return 0;
}

static int export_write_only(void) {
  long page = sysconf(_SC_PAGESIZE);
  unsigned char* mapping =
      mmap(NULL, page, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  listener = make_listener(1);
  int connector = listener < 0 ? -1 : queue_connection();
  int pair[2];
  if (mapping == MAP_FAILED || connector < 0 ||
      socketpair(AF_UNIX, SOCK_STREAM, 0, pair) != 0) {
    return 2;
  }
  struct msghdr msg;
  struct iovec iov;
  union control_buffer control;
  carry_rights(&msg, &iov, &control, listener);
  memcpy(mapping, control.bytes, sizeof control.bytes);
  msg.msg_control = mapping;
  if (mprotect(mapping, page, PROT_WRITE) != 0) {
    return 2;
  }
  ssize_t sent = sendmsg(pair[0], &msg, 0);
  int conn = accept(listener, NULL, NULL);
  int error = errno;
  printf("export-write-only: sent=%zd accept=%s\n", sent, outcome(conn, error));
  return 0;
}

/* accept4(fd, addr, addrlen, 0) with the stack pointer at `stack`. */
extern long accept_on_stack(int fd, void* addr, socklen_t* addrlen, void* stack);
__asm__(
    ".text\n"
    ".globl accept_on_stack\n"
    ".type accept_on_stack,@function\n"
    "accept_on_stack:\n"
    "push %r12\n"
    "mov %rsp,%r12\n"
    "mov %rcx,%rsp\n"
    "xor %r10d,%r10d\n"
    "mov $288,%eax\n"
    "syscall\n"
    "mov %r12,%rsp\n"
    "pop %r12\n"
    "ret\n"
    ".size accept_on_stack,.-accept_on_stack\n"
    ".section .note.GNU-stack,\"\",@progbits\n"
    ".text\n");

static int scratch_case(const char* name, int alias) {
  long page = sysconf(_SC_PAGESIZE);
  unsigned char* mapping = mmap(
      NULL, page * 2, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  if (mapping == MAP_FAILED || mprotect(mapping, page, PROT_NONE) != 0) {
    return 2;
  }
  unsigned char* stack = mapping + page + (alias ? 1024 : 128);
  listener = make_listener(1);
  int connector = listener < 0 ? -1 : queue_connection();
  if (connector < 0) {
    return 2;
  }
  unsigned char peer[sizeof(struct sockaddr_in)];
  memset(peer, 0xa5, sizeof peer);
  socklen_t ordinary = sizeof peer;
  socklen_t* length = alias ? (socklen_t*)(stack - 152) : &ordinary;
  *length = alias ? 0 : sizeof peer;
  long result = accept_on_stack(listener, peer, length, stack);
  int changed = 0;
  for (size_t i = 0; i < sizeof peer; i++) {
    changed |= peer[i] != 0xa5;
  }
  printf(
      "%s: accept=%s len=%u changed=%d\n",
      name,
      outcome(result < 0 ? -1 : (int)result, (int)-result),
      (unsigned)*length,
      changed);
  return 0;
}

struct worker_result {
  int result;
  int error;
  int nonleader;
};

static void* accept_on_worker(void* arg) {
  struct worker_result* r = arg;
  r->nonleader = syscall(SYS_gettid) != getpid();
  r->result = accept(listener, NULL, NULL);
  r->error = errno;
  return NULL;
}

static int worker_queued(void) {
  listener = make_listener(1);
  int connector = listener < 0 ? -1 : queue_connection();
  struct worker_result r = {.result = -1};
  pthread_t thread;
  if (connector < 0 || pthread_create(&thread, NULL, accept_on_worker, &r) != 0 ||
      pthread_join(thread, NULL) != 0) {
    return 2;
  }
  printf(
      "worker-queued: accept=%s nonleader=%d\n",
      outcome(r.result, r.error),
      r.nonleader);
  return 0;
}

/* ---- Receive timeouts ---- */

static int negative_timeout(void) {
  listener = make_listener(1);
  struct timeval timeout = {.tv_sec = -1, .tv_usec = 0};
  if (listener < 0 ||
      setsockopt(listener, SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof timeout) !=
          0) {
    return 2;
  }
  int conn = accept(listener, NULL, NULL);
  printf("negative-timeout: accept=%s\n", outcome(conn, errno));
  return 0;
}

static int timeout_set_by_child(void) {
  listener = make_listener(1);
  if (listener < 0) {
    return 2;
  }
  pid_t child = fork();
  if (child == 0) {
    _exit(set_receive_timeout(listener, 100000) == 0 ? 0 : 1);
  }
  int set = reaped_ok(child);
  int conn = accept(listener, NULL, NULL);
  printf("timeout-set-by-child: set=%d accept=%s\n", set, outcome(conn, errno));
  return 0;
}

static int close_helper(const char* fd_arg) {
  int fd = atoi(fd_arg);
  int was_open = fcntl(fd, F_GETFD) >= 0;
  int closed = close(fd);
  printf("close-helper: open=%d close=%d\n", was_open, closed);
  return 0;
}

int main(int argc, char** argv) {
  const char* mode = argc > 1 ? argv[1] : "";
  if (strcmp(mode, "late-connector") == 0) {
    return late_connector();
  }
  if (strcmp(mode, "unwakeable") == 0) {
    return unwakeable();
  }
  if (strcmp(mode, "reset") == 0) {
    return reset();
  }
  if (strcmp(mode, "sibling-setfl") == 0) {
    return sibling_setfl();
  }
  if (strcmp(mode, "timeout-dup") == 0) {
    return timeout_dup();
  }
  if (strcmp(mode, "signal-restart") == 0) {
    return signal_case(mode, 1, 0);
  }
  if (strcmp(mode, "signal-norestart") == 0) {
    return signal_case(mode, 0, 0);
  }
  if (strcmp(mode, "signal-timed-restart") == 0) {
    return signal_case(mode, 1, 1);
  }
  if (strcmp(mode, "signal-unbounded-restart") == 0) {
    return signal_case(mode, 1, 2);
  }
  if (strcmp(mode, "signal-timed-norestart") == 0) {
    return signal_case(mode, 0, 1);
  }
  if (strcmp(mode, "shutdown") == 0) {
    return shutdown_case();
  }
  if (strcmp(mode, "nonblocking") == 0) {
    return nonblocking_case();
  }
  if (strcmp(mode, "bracket-kill") == 0) {
    return bracket_kill();
  }
  if (strcmp(mode, "accept-once") == 0) {
    return accept_once();
  }
  if (strcmp(mode, "export-sendmsg") == 0) {
    return export_case(mode, EXPORT_SENDMSG);
  }
  if (strcmp(mode, "export-sendmmsg-second") == 0) {
    return export_case(mode, EXPORT_SENDMMSG_SECOND);
  }
  if (strcmp(mode, "export-failed-send") == 0) {
    return export_case(mode, EXPORT_FAILED);
  }
  if (strcmp(mode, "close-one-left") == 0) {
    return close_case(mode, 0);
  }
  if (strcmp(mode, "close-both-done") == 0) {
    return close_case(mode, 1);
  }
  if (strcmp(mode, "exec-sharer-close") == 0) {
    return exec_case(mode, 0, 0);
  }
  if (strcmp(mode, "exec-sharer-then-close") == 0) {
    return exec_case(mode, 0, 1);
  }
  if (strcmp(mode, "failed-exec-close") == 0) {
    return exec_case(mode, 1, 0);
  }
  if (strcmp(mode, "export-at-entry") == 0 && argc > 2) {
    return export_at_entry(argv[2]);
  }
  if (strcmp(mode, "export-write-only") == 0) {
    return export_write_only();
  }
  if (strcmp(mode, "scratch-tight") == 0) {
    return scratch_case(mode, 0);
  }
  if (strcmp(mode, "scratch-alias") == 0) {
    return scratch_case(mode, 1);
  }
  if (strcmp(mode, "worker-queued") == 0) {
    return worker_queued();
  }
  if (strcmp(mode, "negative-timeout") == 0) {
    return negative_timeout();
  }
  if (strcmp(mode, "timeout-set-by-child") == 0) {
    return timeout_set_by_child();
  }
  if (strcmp(mode, "close-helper") == 0 && argc > 2) {
    return close_helper(argv[2]);
  }
  fprintf(stderr, "unknown case %s\n", mode);
  return 2;
}
