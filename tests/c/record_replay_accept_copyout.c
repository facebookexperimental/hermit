/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * What accept4(2) writes back to the guest, at the edges. Linux accepts the
 * connection first and only then copies the peer address out
 * (move_addr_to_user). Since Linux 7.1 it reads *addrlen, writes the full
 * length 16 back to *addrlen unless *addrlen is negative, and then copies
 * min(*addrlen, 16) bytes of the address, which may stop part way at a fault;
 * Linux 7.0 and earlier copy the address before writing the length, so the
 * two fault cases below print differently when run natively there. Hermit's
 * record follows the 7.1 order on every host. A failure in any
 * of those steps returns EFAULT (EINVAL for a negative *addrlen) and installs
 * no descriptor, though the connection is consumed and whatever was already
 * written stays written: an *addrlen Linux cannot write leaves the address
 * untouched, and an address that faults part way still gets the length.
 *
 * With no argument, each case forks a client that connects to a loopback
 * listener and exits, then accepts with the case's buffers and prints:
 *
 *   result:  "fd" or the errno name;
 *   len:     *addrlen afterwards;
 *   prefix:  1 if the bytes Linux must copy hold the peer's family and, when
 *            at least eight were copied, its host 127.0.0.1;
 *   tail:    1 if every byte of the address buffer past those is untouched;
 *   nofd:    1 if a failed call left the next free descriptor where it was.
 *
 * The cases place the address buffer and *addrlen next to an unmapped or
 * read-only page, so that a short but legal extent, an address that faults
 * part way and an *addrlen Linux can read but not write are all exercised.
 * An *addrlen on a write-only page is legal, because on x86 a writable user
 * page is also readable, while one on a PROT_NONE page faults. Two cases make
 * the call on a stack of the guest's own: one whose stack pointer is only 192
 * bytes above an unmapped page, and one whose zero *addrlen sits 264 bytes
 * below its stack pointer, past the 128-byte red zone. Both are legal, and
 * Linux neither faults nor touches memory it was not asked to.
 *
 * With the argument "wait", one thread blocks in accept(2) with *addrlen 0
 * while a second thread sets *addrlen to 16 and only then connects. Linux
 * reads *addrlen after the wait, so the guest must get the whole address.
 *
 * With the argument "private-table" (or "private-table-noaddr", which passes
 * no address buffer), the accept is made by a thread cloned with CLONE_THREAD
 * but without CLONE_FILES, so it has a descriptor table of its own. The leader
 * first connects a decoy socket that occupies descriptor N in both tables; the
 * thread closes its copy and accepts, so the new socket is N in the thread's
 * table while the leader's N is still the decoy. The guest prints:
 *
 *   result:  "fd" or the errno name;
 *   fd:      1 if the new socket got descriptor N;
 *   len:     *addrlen afterwards (0 without an address buffer);
 *   peer:    1 if the address names the connecting client, not the decoy's
 *            peer (always 1 without an address buffer);
 *   decoy:   1 if the leader's N is still the decoy afterwards.
 *
 * With the argument "private-table-sockets", a private-table thread uses
 * descriptor numbers that name a different kind of object in the leader's
 * table, in two rounds:
 *
 *   1. The leader's N is a socket it never connects. The thread closes its
 *      copy, accepts into N, sets TCP_NODELAY on the connection and shuts it
 *      down. Prints accept_fd (1 if the connection got N), setsockopt and
 *      shutdown (the return values).
 *   2. The leader's M is an accepted connection. The thread closes its copy and
 *      makes a socketpair whose first end gets M, shuts down its writing side,
 *      and receives on the other end with recvmmsg(2) without waiting, which
 *      replay runs live. Prints pair_fd (1 if the first end got M),
 *      pair_shutdown, and pair_eof: 1 if Linux reported the shutdown as one
 *      message of length 0.
 *   3. The leader's K is one end of a socketpair. The thread closes its copy,
 *      connects a new socket, which gets K, to the listener and shuts it down.
 *      Prints client_fd (1 if the socket got K) and client_shutdown.
 *
 * Exits 0 only if every case got what Linux promises.
 */

#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif

#include <arpa/inet.h>
#include <errno.h>
#include <linux/futex.h>
#include <netinet/in.h>
#include <netinet/tcp.h>
#include <pthread.h>
#include <sched.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#define UNTOUCHED 0xa5

static struct sockaddr_in server;

/* accept4 made with the stack pointer at `stack`; returns -errno. */
static long accept4_on_stack(
    int listener,
    void* peer,
    socklen_t* length,
    void* stack) {
  long result;
  register long flags __asm__("r10") = 0;
  __asm__ volatile(
      "mov %%rsp, %%r12\n\t"
      "mov %[sp], %%rsp\n\t"
      "syscall\n\t"
      "mov %%r12, %%rsp\n\t"
      : "=a"(result)
      : "a"((long)SYS_accept4),
        "D"((long)listener),
        "S"(peer),
        "d"(length),
        "r"(flags),
        [sp] "r"(stack)
      : "rcx", "r11", "r12", "memory");
  return result;
}

static const char* result_name(int fd, int error) {
  if (fd >= 0) {
    return "fd";
  }
  switch (error) {
    case EFAULT:
      return "EFAULT";
    case EINVAL:
      return "EINVAL";
    default:
      return "other";
  }
}

static int make_listener(void) {
  int listener = socket(AF_INET, SOCK_STREAM, 0);
  memset(&server, 0, sizeof server);
  server.sin_family = AF_INET;
  server.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
  socklen_t length = sizeof server;
  if (listener < 0 ||
      bind(listener, (struct sockaddr*)&server, sizeof server) != 0 ||
      listen(listener, 4) != 0 ||
      getsockname(listener, (struct sockaddr*)&server, &length) != 0) {
    perror("listener");
    exit(2);
  }
  return listener;
}

/* Forks a client that connects to the listener and exits. */
static pid_t connect_client(void) {
  fflush(stdout);
  pid_t child = fork();
  if (child == 0) {
    int fd = socket(AF_INET, SOCK_STREAM, 0);
    int ok = fd >= 0 &&
        connect(fd, (struct sockaddr*)&server, sizeof server) == 0;
    _exit(ok ? 0 : 3);
  }
  if (child < 0) {
    perror("fork");
    exit(2);
  }
  return child;
}

/* The lowest free descriptor number. */
static int next_free_fd(int listener) {
  int fd = dup(listener);
  if (fd < 0) {
    perror("dup");
    exit(2);
  }
  close(fd);
  return fd;
}

/*
 * The first `copied` bytes of `peer` must hold the start of the peer address
 * and the rest of its `size` bytes must be untouched.
 */
static int prefix_ok(const unsigned char* peer, size_t copied) {
  static const unsigned char host[4] = {127, 0, 0, 1};
  return (copied < 1 || peer[0] == AF_INET) && (copied < 2 || peer[1] == 0) &&
      (copied < 8 || memcmp(peer + 4, host, sizeof host) == 0);
}

static int tail_ok(const unsigned char* peer, size_t copied, size_t size) {
  for (size_t i = copied; i < size; i++) {
    if (peer[i] != UNTOUCHED) {
      return 0;
    }
  }
  return 1;
}

enum length_access {
  LENGTH_WRITABLE, /* the case sets *addrlen and reads it back */
  LENGTH_READ_ONLY, /* *addrlen was set beforehand and is read back */
  LENGTH_NONE, /* *addrlen is NULL or unreadable; printed as -1 */
};

struct accept_case {
  const char* name;
  unsigned char* peer; /* address buffer */
  size_t size; /* bytes of `peer` the case may inspect */
  socklen_t* length; /* addrlen argument */
  enum length_access access;
  int capacity; /* *addrlen before the call, for LENGTH_WRITABLE */
  size_t copied; /* bytes Linux copies to `peer` */
  const char* expected_result;
  int expected_len;
  void* stack; /* if set, the stack pointer during the call */
};

static int run_case(int listener, const struct accept_case* c) {
  memset(c->peer, UNTOUCHED, c->size);
  if (c->access == LENGTH_WRITABLE) {
    *c->length = (socklen_t)c->capacity;
  }
  int free_before = next_free_fd(listener);
  pid_t child = connect_client();
  errno = 0;
  int fd = -1;
  int error = 0;
  if (c->stack != NULL) {
    long result = accept4_on_stack(listener, c->peer, c->length, c->stack);
    fd = result < 0 ? -1 : (int)result;
    error = result < 0 ? (int)-result : 0;
  } else {
    fd = (int)syscall(SYS_accept4, listener, c->peer, c->length, 0);
    error = errno;
  }
  int status = 0;
  if (waitpid(child, &status, 0) != child || !WIFEXITED(status) ||
      WEXITSTATUS(status) != 0) {
    printf("%s: client failed\n", c->name);
    return 0;
  }
  int nofd = fd >= 0 || next_free_fd(listener) == free_before;
  if (fd >= 0) {
    close(fd);
  }
  int len = c->access == LENGTH_NONE ? -1 : (int)*c->length;
  int prefix = prefix_ok(c->peer, c->copied);
  int tail = tail_ok(c->peer, c->copied, c->size);
  const char* result = result_name(fd, error);
  printf(
      "%s: result=%s len=%d prefix=%d tail=%d nofd=%d\n",
      c->name,
      result,
      len,
      prefix,
      tail,
      nofd);
  return strcmp(result, c->expected_result) == 0 && len == c->expected_len &&
      prefix && tail && nofd;
}

static int edges(void) {
  long page = sysconf(_SC_PAGESIZE);
  /* Pages [0] and [1] writable, [2] read-only, [3] unmapped. */
  unsigned char* map = mmap(
      NULL,
      4 * page,
      PROT_READ | PROT_WRITE,
      MAP_PRIVATE | MAP_ANONYMOUS,
      -1,
      0);
  if (map == MAP_FAILED || munmap(map + 3 * page, page) != 0) {
    perror("mmap");
    return 2;
  }
  socklen_t* read_only_length = (socklen_t*)(map + 2 * page);
  *read_only_length = 16;
  if (mprotect(map + 2 * page, page, PROT_READ) != 0) {
    perror("mprotect");
    return 2;
  }
  /* The last bytes before the unmapped page are read-only, so buffers that
   * must be writable up to an inaccessible page get their own mapping. */
  unsigned char* edge = mmap(
      NULL,
      2 * page,
      PROT_READ | PROT_WRITE,
      MAP_PRIVATE | MAP_ANONYMOUS,
      -1,
      0);
  if (edge == MAP_FAILED || munmap(edge + page, page) != 0) {
    perror("mmap");
    return 2;
  }
  unsigned char* edge_end = edge + page;
  /* A write-only page followed by an inaccessible one. */
  unsigned char* hidden = mmap(
      NULL,
      2 * page,
      PROT_READ | PROT_WRITE,
      MAP_PRIVATE | MAP_ANONYMOUS,
      -1,
      0);
  if (hidden == MAP_FAILED || mprotect(hidden, page, PROT_WRITE) != 0 ||
      mprotect(hidden + page, page, PROT_NONE) != 0) {
    perror("mmap");
    return 2;
  }
  socklen_t* length_write_only = (socklen_t*)(hidden + 64);
  socklen_t* length_inaccessible = (socklen_t*)(hidden + page + 64);
  /* A stack page with nothing mapped below it. */
  unsigned char* stack = mmap(
      NULL,
      2 * page,
      PROT_READ | PROT_WRITE,
      MAP_PRIVATE | MAP_ANONYMOUS,
      -1,
      0);
  if (stack == MAP_FAILED || munmap(stack, page) != 0) {
    perror("mmap");
    return 2;
  }
  unsigned char* tight_stack = stack + page + 192;
  unsigned char* roomy_stack = stack + page + 1024;
  socklen_t* length_below_red_zone = (socklen_t*)(roomy_stack - 264);
  socklen_t* length_straddling = (socklen_t*)(map + page - 2);
  socklen_t* length_at_edge = (socklen_t*)(edge_end - sizeof(socklen_t));
  socklen_t* length_unmapped = (socklen_t*)(map + 3 * page);
  unsigned char ordinary[sizeof(struct sockaddr_storage)];
  int whole = sizeof ordinary;
  socklen_t length;

  const struct accept_case cases[] = {
      {"full", ordinary, whole, &length, LENGTH_WRITABLE, whole, 16, "fd", 16},
      {"zero", ordinary, whole, &length, LENGTH_WRITABLE, 0, 0, "fd", 16},
      {"truncated", ordinary, whole, &length, LENGTH_WRITABLE, 4, 4, "fd", 16},
      {"address-ends-at-page-4",
       edge_end - 4,
       4,
       &length,
       LENGTH_WRITABLE,
       4,
       4,
       "fd",
       16},
      {"address-ends-at-page-1",
       edge_end - 1,
       1,
       &length,
       LENGTH_WRITABLE,
       1,
       1,
       "fd",
       16},
      {"addrlen-ends-at-page",
       ordinary,
       whole,
       length_at_edge,
       LENGTH_WRITABLE,
       whole,
       16,
       "fd",
       16},
      {"addrlen-straddles-pages",
       ordinary,
       whole,
       length_straddling,
       LENGTH_WRITABLE,
       whole,
       16,
       "fd",
       16},
      {"address-faults-after-4",
       edge_end - 4,
       4,
       &length,
       LENGTH_WRITABLE,
       whole,
       4,
       "EFAULT",
       16},
      {"addrlen-read-only",
       ordinary,
       whole,
       read_only_length,
       LENGTH_READ_ONLY,
       0,
       0,
       "EFAULT",
       16},
      {"addrlen-unmapped",
       ordinary,
       whole,
       length_unmapped,
       LENGTH_NONE,
       0,
       0,
       "EFAULT",
       -1},
      {"addrlen-write-only",
       ordinary,
       whole,
       length_write_only,
       LENGTH_WRITABLE,
       whole,
       16,
       "fd",
       16},
      {"addrlen-inaccessible",
       ordinary,
       whole,
       length_inaccessible,
       LENGTH_NONE,
       0,
       0,
       "EFAULT",
       -1},
      {"addrlen-null", ordinary, whole, NULL, LENGTH_NONE, 0, 0, "EFAULT", -1},
      {"negative", ordinary, whole, &length, LENGTH_WRITABLE, -1, 0, "EINVAL", -1},
      {"tight-stack",
       ordinary,
       whole,
       &length,
       LENGTH_WRITABLE,
       whole,
       16,
       "fd",
       16,
       tight_stack},
      {"zero-addrlen-below-red-zone",
       ordinary,
       whole,
       length_below_red_zone,
       LENGTH_WRITABLE,
       0,
       0,
       "fd",
       16,
       roomy_stack},
  };
  int listener = make_listener();
  int ok = 1;
  for (size_t i = 0; i < sizeof cases / sizeof cases[0]; i++) {
    ok &= run_case(listener, &cases[i]);
  }
  close(listener);
  return ok ? 0 : 1;
}

static volatile socklen_t shared_length;
static int connector_fd = -1;

static void* connector(void* unused) {
  (void)unused;
  for (int turn = 0; turn < 64; turn++) {
    sched_yield();
  }
  shared_length = sizeof(struct sockaddr_in);
  long failed =
      connect(connector_fd, (struct sockaddr*)&server, sizeof server) != 0;
  return (void*)failed;
}

static int wait_for_length(void) {
  int listener = make_listener();
  /* Opened here, and closed only after the join, so that the accepted
   * descriptor's number does not depend on how the threads interleave. */
  connector_fd = socket(AF_INET, SOCK_STREAM, 0);
  if (connector_fd < 0) {
    perror("socket");
    return 2;
  }
  shared_length = 0;
  pthread_t thread;
  if (pthread_create(&thread, NULL, connector, NULL) != 0) {
    return 2;
  }
  unsigned char peer[sizeof(struct sockaddr_storage)];
  memset(peer, UNTOUCHED, sizeof peer);
  errno = 0;
  int fd = accept(listener, (struct sockaddr*)peer, (socklen_t*)&shared_length);
  int error = errno;
  void* failed;
  if (pthread_join(thread, &failed) != 0) {
    return 2;
  }
  if (fd >= 0) {
    close(fd);
  }
  close(connector_fd);
  close(listener);
  int prefix = prefix_ok(peer, 16);
  int tail = tail_ok(peer, 16, sizeof peer);
  printf(
      "wait: result=%s len=%u prefix=%d tail=%d connector=%ld\n",
      result_name(fd, error),
      (unsigned)shared_length,
      prefix,
      tail,
      (long)failed);
  return fd >= 0 && shared_length == 16 && prefix && tail && failed == NULL
      ? 0
      : 1;
}

/* A raw system call that leaves errno, which the leader shares, alone. */
static long raw_syscall4(long number, long a, long b, long c, long d) {
  long result;
  register long r10 __asm__("r10") = d;
  __asm__ volatile("syscall"
                   : "=a"(result)
                   : "a"(number), "D"(a), "S"(b), "d"(c), "r"(r10)
                   : "rcx", "r11", "memory");
  return result;
}

static long raw_syscall5(long number, long a, long b, long c, long d, long e) {
  long result;
  register long r10 __asm__("r10") = d;
  register long r8 __asm__("r8") = e;
  __asm__ volatile("syscall"
                   : "=a"(result)
                   : "a"(number), "D"(a), "S"(b), "d"(c), "r"(r10), "r"(r8)
                   : "rcx", "r11", "memory");
  return result;
}

static int private_listener;
static int private_slot;
static int private_with_address;
static struct sockaddr_in private_peer;
static socklen_t private_length;
static long private_result;

/* Runs on a thread with its own descriptor table; raw system calls only. */
static int private_accept(void* unused) {
  (void)unused;
  raw_syscall4(SYS_close, private_slot, 0, 0, 0);
  private_length = private_with_address ? sizeof private_peer : 0;
  private_result = raw_syscall4(
      SYS_accept4,
      private_listener,
      private_with_address ? (long)&private_peer : 0,
      private_with_address ? (long)&private_length : 0,
      0);
  if (private_result >= 0) {
    raw_syscall4(SYS_close, private_result, 0, 0, 0);
  }
  return 0;
}

/* Runs fn on a thread with its own descriptor table and waits for its exit. */
static int run_on_private_table(int (*fn)(void*)) {
  enum { STACK_SIZE = 65536 };
  char* stack = mmap(
      NULL,
      STACK_SIZE,
      PROT_READ | PROT_WRITE,
      MAP_PRIVATE | MAP_ANONYMOUS,
      -1,
      0);
  if (stack == MAP_FAILED) {
    perror("mmap");
    return -1;
  }
  /* Cleared by Linux, with a futex wake, once the thread has exited. */
  static volatile pid_t thread_tid;
  int tid = clone(
      fn,
      stack + STACK_SIZE,
      CLONE_VM | CLONE_SIGHAND | CLONE_THREAD | CLONE_SYSVSEM |
          CLONE_PARENT_SETTID | CLONE_CHILD_CLEARTID,
      NULL,
      &thread_tid,
      NULL,
      &thread_tid);
  if (tid < 0) {
    perror("clone");
    return -1;
  }
  pid_t seen;
  while ((seen = thread_tid) != 0) {
    syscall(SYS_futex, &thread_tid, FUTEX_WAIT, seen, NULL, NULL, 0);
  }
  return 0;
}

static unsigned short local_port(int fd) {
  struct sockaddr_in local;
  socklen_t length = sizeof local;
  if (getsockname(fd, (struct sockaddr*)&local, &length) != 0) {
    perror("getsockname");
    exit(2);
  }
  return local.sin_port;
}

static unsigned short remote_port(int fd) {
  struct sockaddr_in remote;
  socklen_t length = sizeof remote;
  if (getpeername(fd, (struct sockaddr*)&remote, &length) != 0) {
    perror("getpeername");
    exit(2);
  }
  return remote.sin_port;
}

static int private_table(int with_address) {
  private_listener = make_listener();
  /* The decoy's peer is the decoy client, whose port no other socket has. */
  int decoy_client = socket(AF_INET, SOCK_STREAM, 0);
  if (decoy_client < 0 ||
      connect(decoy_client, (struct sockaddr*)&server, sizeof server) != 0) {
    perror("decoy");
    return 2;
  }
  private_slot = accept(private_listener, NULL, NULL);
  int client = socket(AF_INET, SOCK_STREAM, 0);
  if (private_slot < 0 || client < 0 ||
      connect(client, (struct sockaddr*)&server, sizeof server) != 0) {
    perror("client");
    return 2;
  }
  unsigned short decoy_peer = remote_port(private_slot);
  private_with_address = with_address;
  memset(&private_peer, UNTOUCHED, sizeof private_peer);
  if (run_on_private_table(private_accept) != 0) {
    return 2;
  }
  int accepted = private_result >= 0;
  int same_slot = private_result == private_slot;
  int peer = !with_address ||
      (private_length == sizeof private_peer &&
       private_peer.sin_family == AF_INET &&
       private_peer.sin_port == local_port(client) &&
       private_peer.sin_port != decoy_peer);
  int decoy = remote_port(private_slot) == decoy_peer;
  printf(
      "%s: result=%s fd=%d len=%u peer=%d decoy=%d\n",
      with_address ? "private-table" : "private-table-noaddr",
      result_name(accepted ? 0 : -1, (int)-private_result),
      same_slot,
      (unsigned)private_length,
      peer,
      decoy);
  return accepted && same_slot && peer && decoy ? 0 : 1;
}

static long sockets_accepted;
static long sockets_nodelay;
static long sockets_shutdown;
static int sockets_pair[2];
static long sockets_pair_result;
static long sockets_pair_shutdown;
static long sockets_pair_eof;
static long sockets_client;
static long sockets_client_shutdown;

/* Round 1: accept into a slot that is an unconnected socket in the leader. */
static int sockets_accept(void* unused) {
  (void)unused;
  raw_syscall4(SYS_close, private_slot, 0, 0, 0);
  sockets_accepted = raw_syscall4(SYS_accept4, private_listener, 0, 0, 0);
  if (sockets_accepted >= 0) {
    static const int one = 1;
    sockets_nodelay = raw_syscall5(
        SYS_setsockopt,
        sockets_accepted,
        IPPROTO_TCP,
        TCP_NODELAY,
        (long)&one,
        sizeof one);
    sockets_shutdown =
        raw_syscall4(SYS_shutdown, sockets_accepted, SHUT_RDWR, 0, 0);
    raw_syscall4(SYS_close, sockets_accepted, 0, 0, 0);
  }
  return 0;
}

/* Round 2: a socketpair end in a slot that is an accepted connection in the
 * leader. */
static int sockets_socketpair(void* unused) {
  (void)unused;
  raw_syscall4(SYS_close, private_slot, 0, 0, 0);
  sockets_pair_result = raw_syscall4(
      SYS_socketpair, AF_UNIX, SOCK_STREAM, 0, (long)sockets_pair);
  if (sockets_pair_result == 0) {
    sockets_pair_shutdown =
        raw_syscall4(SYS_shutdown, sockets_pair[0], SHUT_WR, 0, 0);
    char byte;
    struct iovec iov = {.iov_base = &byte, .iov_len = 1};
    struct mmsghdr message;
    memset(&message, 0, sizeof message);
    message.msg_hdr.msg_iov = &iov;
    message.msg_hdr.msg_iovlen = 1;
    message.msg_len = UNTOUCHED;
    long received = raw_syscall5(
        SYS_recvmmsg, sockets_pair[1], (long)&message, 1, MSG_DONTWAIT, 0);
    sockets_pair_eof = received == 1 && message.msg_len == 0;
    raw_syscall4(SYS_close, sockets_pair[0], 0, 0, 0);
    raw_syscall4(SYS_close, sockets_pair[1], 0, 0, 0);
  }
  return 0;
}

/* Round 3: a connected TCP socket in a slot that is a Unix socket in the
 * leader. */
static int sockets_connect(void* unused) {
  (void)unused;
  raw_syscall4(SYS_close, private_slot, 0, 0, 0);
  sockets_client = raw_syscall4(SYS_socket, AF_INET, SOCK_STREAM, 0, 0);
  if (sockets_client >= 0 &&
      raw_syscall4(
          SYS_connect, sockets_client, (long)&server, sizeof server, 0) == 0) {
    sockets_client_shutdown =
        raw_syscall4(SYS_shutdown, sockets_client, SHUT_RDWR, 0, 0);
  }
  if (sockets_client >= 0) {
    raw_syscall4(SYS_close, sockets_client, 0, 0, 0);
  }
  return 0;
}

static int private_table_sockets(void) {
  private_listener = make_listener();
  int client = socket(AF_INET, SOCK_STREAM, 0);
  if (client < 0 ||
      connect(client, (struct sockaddr*)&server, sizeof server) != 0) {
    perror("client");
    return 2;
  }
  int unconnected = socket(AF_INET, SOCK_STREAM, 0);
  if (unconnected < 0) {
    perror("socket");
    return 2;
  }
  private_slot = unconnected;
  if (run_on_private_table(sockets_accept) != 0) {
    return 2;
  }
  int accept_fd = sockets_accepted == unconnected;

  int second = socket(AF_INET, SOCK_STREAM, 0);
  if (second < 0 ||
      connect(second, (struct sockaddr*)&server, sizeof server) != 0) {
    perror("second client");
    return 2;
  }
  int connection = accept(private_listener, NULL, NULL);
  if (connection < 0) {
    perror("accept");
    return 2;
  }
  private_slot = connection;
  if (run_on_private_table(sockets_socketpair) != 0) {
    return 2;
  }
  int pair_fd = sockets_pair_result == 0 && sockets_pair[0] == connection;

  int ends[2];
  if (socketpair(AF_UNIX, SOCK_STREAM, 0, ends) != 0) {
    perror("socketpair");
    return 2;
  }
  private_slot = ends[0];
  sockets_client_shutdown = -1;
  if (run_on_private_table(sockets_connect) != 0) {
    return 2;
  }
  int client_fd = sockets_client == ends[0];
  printf(
      "private-table-sockets: accept_fd=%d setsockopt=%ld shutdown=%ld "
      "pair_fd=%d pair_shutdown=%ld pair_eof=%ld client_fd=%d "
      "client_shutdown=%ld\n",
      accept_fd,
      sockets_nodelay,
      sockets_shutdown,
      pair_fd,
      sockets_pair_shutdown,
      sockets_pair_eof,
      client_fd,
      sockets_client_shutdown);
  return accept_fd && sockets_nodelay == 0 && sockets_shutdown == 0 &&
          pair_fd && sockets_pair_shutdown == 0 && sockets_pair_eof == 1 &&
          client_fd && sockets_client_shutdown == 0
      ? 0
      : 1;
}

int main(int argc, char** argv) {
  if (argc > 1 && strcmp(argv[1], "wait") == 0) {
    return wait_for_length();
  }
  if (argc > 1 && strcmp(argv[1], "private-table") == 0) {
    return private_table(1);
  }
  if (argc > 1 && strcmp(argv[1], "private-table-noaddr") == 0) {
    return private_table(0);
  }
  if (argc > 1 && strcmp(argv[1], "private-table-sockets") == 0) {
    return private_table_sockets();
  }
  return edges();
}
