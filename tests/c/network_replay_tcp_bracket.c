#define _GNU_SOURCE

/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Network record/replay bracket. The controller is a loopback TCP server
 * with a fixed protocol; the client connects to it and two threads, one on
 * the socket and one on a dup of it, race to poll and receive the two
 * three-byte inputs. Which thread wins depends on the schedule; the bytes
 * the client sends and receives do not.
 *
 * The select client runs the same protocol on one thread, waiting with
 * select, pselect6 and glibc's select and pselect, alongside a pipe that
 * the host answers for. Before the request it checks that a closed
 * descriptor fails the call with EBADF, that poll and ppoll with more
 * entries than RLIMIT_NOFILE fail with EINVAL though the array names the
 * socket, that a wait in which nothing arrives times out with no time left,
 * and that only the pipe is writable.
 *
 * The other client modes each do one thing network record/replay must
 * refuse: end before sending the whole request, send with sendmsg or
 * sendfile, select or pselect6 on an alias of the socket above FD_SETSIZE,
 * pselect6 with a signal mask pointer, connect to the unspecified address, send
 * UDP, or listen. The
 * guard modes each try a way around the channel before any connection: a
 * 24-byte IPv6 connect, a netlink socket, an abstract AF_UNIX connect, a
 * socket receive timeout, a negative (immediate) receive timeout, an IPv4
 * socket received through SCM_RIGHTS, epoll registration, signal-driven
 * I/O, an interface ioctl on an AF_UNIX socket, or opening /proc/net/dev.
 *
 * The backpressure controller reads nothing from a data connection until a
 * second, control connection says "go". The backpressure clients fill the
 * data connection from one thread, past what the small socket buffers hold,
 * while a second thread sends "go" only after the first is stuck: after a
 * nonblocking send reported EAGAIN, or while a blocking send waits. Linux
 * completes both; so must record, which may not hold the second thread back
 * while the first waits for buffer space. In the interleaved mode the first
 * thread fills the buffer, then waits in a blocking send before any byte of
 * it is accepted, and the second thread's nonblocking send on the same
 * connection is refused before it sends "go". Linux completes that too;
 * record refuses it, because replay would hand the refusal to the waiting
 * blocking send.
 *
 * Three more blocking modes check what replay and record do around the
 * wait. The observe mode prints whether the "go" thread had run, and the
 * monotonic clock before and after the send, so that a replay that
 * completed the send earlier or later than the recording prints something
 * else. In the replace mode the "go" thread installs a third connection
 * over the waiting send's descriptor number, keeping the original open
 * through a dup; Linux finishes the send on the original connection. In
 * the shared mode the send buffer is a shared file mapping; the data thread
 * first fills the connection, so its blocking send waits before its first
 * byte, and a child process rewrites the unsent second half during that
 * wait. Linux sends the rewritten bytes. The shared and replace
 * controllers report what each connection received instead of checking it.
 *
 * The poll-high-word mode needs no controller: it polls a readable pipe
 * with poll and then ppoll, passing nfds with its high word set. Linux reads
 * nfds as an unsigned int, so each is a one-entry call that returns 1.
 * Record reads the pollfd array before it knows whether the call names a
 * channel, so it must size that read as Linux does.
 */

#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <net/if.h>
#include <netinet/in.h>
#include <poll.h>
#include <pthread.h>
#include <signal.h>
#include <stddef.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/sendfile.h>
#include <sys/epoll.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/resource.h>
#include <sys/select.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/un.h>
#include <sys/uio.h>
#include <sys/time.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define PAYLOAD_SIZE 3
#define FIXTURE_DEADLINE_SECONDS 8
#define CONTROLLER_ACCEPT_DEADLINE_SECONDS 30
/*
 * The client's own bounds: how long it waits for the peer, and its alarm.
 * Under Hermit they count the guest's virtual time, not wall time. While a
 * recorded send or poll waits for the real peer, each retry advances virtual
 * time by a fixed amount however little wall time it took, so virtual time
 * runs ahead of the wall clock in proportion to how long the host keeps the
 * peer from answering: in one failed run, 8 virtual seconds passed in about
 * one wall second, and a peer delayed by host load turned into a client
 * timeout or into a SIGALRM that record refuses. These bounds are therefore
 * set far beyond the virtual time a run has been seen to reach within the
 * wall-clock bounds: at the fastest rate measured, about 8 virtual seconds per
 * wall second, the tests' 45-second process timeout comes to about 6 virtual
 * minutes, a tenth of the wait below. Nothing enforces that rate, so this is
 * a wide margin, not a guarantee. The wall-clock bounds stay where wall time
 * is measured: the controllers' alarm(FIXTURE_DEADLINE_SECONDS) and socket
 * timeouts, and the tests' own process timeouts. A client whose threads are
 * all blocked still ends at its alarm at once, because Hermit then advances
 * virtual time straight to the next timer. A client still retrying a network
 * wait keeps that from happening; it ends when the controller's alarm closes
 * the connection, or at the test's process timeout.
 */
#define CLIENT_PEER_WAIT_SECONDS 3600
#define CLIENT_DEADLINE_SECONDS (2 * CLIENT_PEER_WAIT_SECONDS)
#define ADDRESS_LENGTH_SENTINEL 123
/* Above FD_SETSIZE, so only a raw select bitmap can name it. */
#define HIGH_ALIAS_FD 1100
/* Far more than the small socket buffers below hold. */
#define BACKPRESSURE_BYTES (256 * 1024)
#define BACKPRESSURE_SOCKET_BUFFER 4096

static const char REQUEST[] = "request\n";
static const char PROGRESS[] = "next\n";
static const char COMPLETION[] = "done\n";
static const char FIRST_INPUT[] = "abc";
static const char SECOND_INPUT[] = "def";
static const char OUTBOUND_HEX[] = "726571756573740a6e6578740a646f6e650a";
static const char GO[] = "go\n";
static const char DRAINED[] = "ok\n";

static void fail(const char *operation) {
  perror(operation);
  exit(1);
}

static void fail_message(const char *message) {
  fprintf(stderr, "%s\n", message);
  exit(1);
}

static void set_deadline(void) {
  signal(SIGPIPE, SIG_IGN);
  alarm(CLIENT_DEADLINE_SECONDS);
}

static void set_socket_timeouts(int fd) {
  const struct timeval timeout = {.tv_sec = 5, .tv_usec = 0};
  if (setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof(timeout)) != 0 ||
      setsockopt(fd, SOL_SOCKET, SO_SNDTIMEO, &timeout, sizeof(timeout)) != 0) {
    fail("setsockopt timeout");
  }
}

static void send_all(int fd, const void *raw, size_t length) {
  const char *bytes = raw;
  while (length != 0) {
    ssize_t sent = send(fd, bytes, length, MSG_NOSIGNAL);
    if (sent < 0 && errno == EINTR)
      continue;
    if (sent <= 0)
      fail("send");
    bytes += sent;
    length -= (size_t)sent;
  }
}

static void receive_exact(int fd, void *raw, size_t length) {
  char *bytes = raw;
  while (length != 0) {
    ssize_t received = recv(fd, bytes, length, 0);
    if (received < 0 && errno == EINTR)
      continue;
    if (received < 0)
      fail("recv");
    if (received == 0)
      fail_message("peer closed before the fixed protocol message completed");
    bytes += received;
    length -= (size_t)received;
  }
}

static uint64_t fnv1a64_update(uint64_t digest, const void *raw, size_t length) {
  const unsigned char *bytes = raw;
  for (size_t index = 0; index < length; ++index) {
    digest ^= bytes[index];
    digest *= UINT64_C(1099511628211);
  }
  return digest;
}

static uint64_t expected_outbound_digest(void) {
  uint64_t digest = UINT64_C(14695981039346656037);
  digest = fnv1a64_update(digest, REQUEST, sizeof(REQUEST) - 1);
  digest = fnv1a64_update(digest, PROGRESS, sizeof(PROGRESS) - 1);
  return fnv1a64_update(digest, COMPLETION, sizeof(COMPLETION) - 1);
}

static void publish_text(const char *path, const char *text) {
  char temporary[4096];
  int count = snprintf(temporary, sizeof(temporary), "%s.tmp.%ld", path,
                       (long)getpid());
  if (count < 0 || (size_t)count >= sizeof(temporary))
    fail_message("publication path is too long");

  int fd = open(temporary, O_WRONLY | O_CREAT | O_EXCL | O_CLOEXEC, 0600);
  if (fd < 0)
    fail("open publication");
  size_t length = strlen(text);
  const char *cursor = text;
  while (length != 0) {
    ssize_t written = write(fd, cursor, length);
    if (written < 0 && errno == EINTR)
      continue;
    if (written <= 0)
      fail("write publication");
    cursor += written;
    length -= (size_t)written;
  }
  if (close(fd) != 0)
    fail("close publication");
  if (rename(temporary, path) != 0)
    fail("rename publication");
}

static int run_controller(const char *port_path, const char *report_path,
                          const char *contact_path) {
  signal(SIGPIPE, SIG_IGN);
  alarm(CONTROLLER_ACCEPT_DEADLINE_SECONDS);
  int listener = socket(AF_INET, SOCK_STREAM | SOCK_CLOEXEC, 0);
  if (listener < 0)
    fail("socket controller");

  struct sockaddr_in address = {
      .sin_family = AF_INET,
      .sin_port = htons(0),
      .sin_addr.s_addr = htonl(INADDR_LOOPBACK),
  };
  if (bind(listener, (struct sockaddr *)&address, sizeof(address)) != 0)
    fail("bind controller");
  socklen_t address_length = sizeof(address);
  if (getsockname(listener, (struct sockaddr *)&address, &address_length) != 0)
    fail("getsockname controller");
  if (listen(listener, 1) != 0)
    fail("listen controller");

  char port[32];
  int port_length = snprintf(port, sizeof(port), "%u\n", ntohs(address.sin_port));
  if (port_length <= 0 || (size_t)port_length >= sizeof(port))
    fail_message("controller port did not fit its publication buffer");
  publish_text(port_path, port);

  int client = accept4(listener, NULL, NULL, SOCK_CLOEXEC);
  if (client < 0)
    fail("accept controller");
  // Hermit startup happens before the guest can connect. Once accepted, the
  // exact stream protocol retains its original, shorter wall bound.
  alarm(FIXTURE_DEADLINE_SECONDS);
  publish_text(contact_path, "accepted\n");
  set_socket_timeouts(client);
  close(listener);

  char request[sizeof(REQUEST) - 1];
  receive_exact(client, request, sizeof(request));
  if (memcmp(request, REQUEST, sizeof(request)) != 0)
    fail_message("outbound request mismatch");
  send_all(client, FIRST_INPUT, sizeof(FIRST_INPUT) - 1);

  char progress[sizeof(PROGRESS) - 1];
  receive_exact(client, progress, sizeof(progress));
  if (memcmp(progress, PROGRESS, sizeof(progress)) != 0)
    fail_message("outbound progress mismatch");
  send_all(client, SECOND_INPUT, sizeof(SECOND_INPUT) - 1);

  char completion[sizeof(COMPLETION) - 1];
  receive_exact(client, completion, sizeof(completion));
  if (memcmp(completion, COMPLETION, sizeof(completion)) != 0)
    fail_message("outbound completion mismatch");
  if (shutdown(client, SHUT_WR) != 0)
    fail("shutdown controller");

  char unexpected;
  for (;;) {
    ssize_t received = recv(client, &unexpected, 1, 0);
    if (received < 0 && errno == EINTR)
      continue;
    if (received < 0)
      fail("recv controller tail");
    if (received != 0)
      fail_message("unexpected outbound bytes followed the fixed request");
    break;
  }
  close(client);

  char report[256];
  int report_length = snprintf(
      report, sizeof(report),
      "controller=complete\noutbound_hex=%s\noutbound_fnv1a64=%016llx\n",
      OUTBOUND_HEX, (unsigned long long)expected_outbound_digest());
  if (report_length <= 0 || (size_t)report_length >= sizeof(report))
    fail_message("controller report did not fit its fixed buffer");
  publish_text(report_path, report);
  return 0;
}

struct reader {
  int fd;
  int index;
  pthread_barrier_t *start;
  char bytes[PAYLOAD_SIZE + 1];
  ssize_t received;
  short readiness;
  int error;
};

static void *run_reader(void *raw) {
  struct reader *reader = raw;
  int barrier = pthread_barrier_wait(reader->start);
  if (barrier != 0 && barrier != PTHREAD_BARRIER_SERIAL_THREAD) {
    reader->error = barrier;
    return NULL;
  }

  struct pollfd interest = {.fd = reader->fd, .events = POLLIN};
  int ready;
  do {
    ready = poll(&interest, 1, CLIENT_PEER_WAIT_SECONDS * 1000);
  } while (ready < 0 && errno == EINTR);
  if (ready < 0) {
    reader->error = errno;
    return NULL;
  }
  if (ready != 1) {
    reader->error = ETIMEDOUT;
    return NULL;
  }
  reader->readiness = interest.revents;
  if (reader->readiness != POLLIN) {
    reader->error = EIO;
    return NULL;
  }

  /* Linux ignores the address length when no address is requested. */
  socklen_t ignored_length = ADDRESS_LENGTH_SENTINEL;
  do {
    reader->received = recvfrom(reader->fd, reader->bytes, PAYLOAD_SIZE, 0, NULL,
                                &ignored_length);
  } while (reader->received < 0 && errno == EINTR);
  if (reader->received < 0) {
    reader->error = errno;
    return NULL;
  }
  if (ignored_length != ADDRESS_LENGTH_SENTINEL) {
    reader->error = EPROTO;
    return NULL;
  }
  if (reader->received == PAYLOAD_SIZE)
    reader->bytes[PAYLOAD_SIZE] = '\0';
  if (reader->received == PAYLOAD_SIZE &&
      memcmp(reader->bytes, FIRST_INPUT, PAYLOAD_SIZE) == 0) {
    send_all(reader->fd, PROGRESS, sizeof(PROGRESS) - 1);
  }
  return NULL;
}

static uint16_t parse_port(const char *port_text) {
  char *end = NULL;
  unsigned long parsed = strtoul(port_text, &end, 10);
  if (end == port_text || *end != '\0' || parsed == 0 || parsed > UINT16_MAX)
    fail_message("invalid controller port");
  return (uint16_t)parsed;
}

static struct sockaddr_in loopback_address(uint16_t port) {
  struct sockaddr_in address = {
      .sin_family = AF_INET,
      .sin_port = htons(port),
      .sin_addr.s_addr = htonl(INADDR_LOOPBACK),
  };
  return address;
}

/* A client that each mode ends differently, all of them refused by network
 * record/replay. Returns only if no refusal ended the run. */
/* Pass `fd` across a socket pair with SCM_RIGHTS and return the copy. */
static int pass_descriptor(int fd) {
  int pair[2];
  if (socketpair(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0, pair) != 0)
    fail("socketpair");
  char byte = 'x';
  struct iovec part = {.iov_base = &byte, .iov_len = 1};
  union {
    struct cmsghdr header;
    char bytes[CMSG_SPACE(sizeof(int))];
  } control;
  memset(&control, 0, sizeof(control));
  struct msghdr message = {.msg_iov = &part,
                           .msg_iovlen = 1,
                           .msg_control = control.bytes,
                           .msg_controllen = sizeof(control.bytes)};
  struct cmsghdr *header = CMSG_FIRSTHDR(&message);
  header->cmsg_level = SOL_SOCKET;
  header->cmsg_type = SCM_RIGHTS;
  header->cmsg_len = CMSG_LEN(sizeof(int));
  memcpy(CMSG_DATA(header), &fd, sizeof(int));
  if (sendmsg(pair[0], &message, 0) != 1)
    fail("sendmsg SCM_RIGHTS");
  memset(&control, 0, sizeof(control));
  message.msg_controllen = sizeof(control.bytes);
  if (recvmsg(pair[1], &message, MSG_CMSG_CLOEXEC) != 1)
    fail("recvmsg SCM_RIGHTS");
  header = CMSG_FIRSTHDR(&message);
  if (header == NULL || header->cmsg_type != SCM_RIGHTS)
    fail_message("no descriptor received");
  int received;
  memcpy(&received, CMSG_DATA(header), sizeof(int));
  return received;
}

/* Try one way around the channel; returns 1 if `mode` is not a guard mode. */
static int run_guard_probe(uint16_t port, const char *mode) {
  if (strcmp(mode, "ipv6-24") == 0) {
    /* Linux accepts the RFC 2133 length, without sin6_scope_id. */
    int fd = socket(AF_INET6, SOCK_STREAM | SOCK_CLOEXEC, 0);
    struct sockaddr_in6 address = {.sin6_family = AF_INET6,
                                   .sin6_port = htons(port)};
    if (fd < 0)
      fail("socket ipv6");
    if (connect(fd, (struct sockaddr *)&address, 24) != 0)
      fail("connect ipv6-24");
    return 0;
  }
  if (strcmp(mode, "netlink") == 0) {
    if (socket(AF_NETLINK, SOCK_RAW | SOCK_CLOEXEC, 0) < 0)
      fail("socket netlink");
    return 0;
  }
  if (strcmp(mode, "abstract") == 0) {
    int fd = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
    struct sockaddr_un address = {.sun_family = AF_UNIX};
    static const char name[] = "hermit-network-replay-probe";
    if (fd < 0)
      fail("socket unix");
    memcpy(address.sun_path + 1, name, sizeof(name) - 1);
    connect(fd, (struct sockaddr *)&address,
            offsetof(struct sockaddr_un, sun_path) + sizeof(name));
    return 0;
  }
  if (strcmp(mode, "ifindex") == 0) {
    /* glibc's if_nametoindex asks an AF_UNIX socket. */
    int fd = socket(AF_UNIX, SOCK_DGRAM | SOCK_CLOEXEC, 0);
    struct ifreq request = {0};
    if (fd < 0)
      fail("socket unix");
    strcpy(request.ifr_name, "lo");
    if (ioctl(fd, SIOCGIFINDEX, &request) != 0)
      fail("ioctl SIOCGIFINDEX");
    return 0;
  }
  if (strcmp(mode, "procnet") == 0) {
    if (open("/proc/net/dev", O_RDONLY | O_CLOEXEC) < 0)
      fail("open /proc/net/dev");
    return 0;
  }
  /* Other modes must not open a socket here: replay matches sockets by
     their creation order. */
  if (strcmp(mode, "rcvtimeo") != 0 && strcmp(mode, "rcvtimeo-negative") != 0 &&
      strcmp(mode, "scm-rights") != 0 && strcmp(mode, "epoll") != 0 &&
      strcmp(mode, "async") != 0)
    return 1;
  int fd = socket(AF_INET, SOCK_STREAM | SOCK_CLOEXEC, 0);
  if (fd < 0)
    fail("socket guard");
  if (strcmp(mode, "rcvtimeo") == 0) {
    set_socket_timeouts(fd);
  } else if (strcmp(mode, "rcvtimeo-negative") == 0) {
    /* Linux makes a negative timeout an immediate one. */
    const struct timeval timeout = {.tv_sec = -1, .tv_usec = 0};
    if (setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof(timeout)) != 0)
      fail("setsockopt negative timeout");
  } else if (strcmp(mode, "scm-rights") == 0) {
    pass_descriptor(fd);
  } else if (strcmp(mode, "epoll") == 0) {
    int epoll = epoll_create1(EPOLL_CLOEXEC);
    struct epoll_event event = {.events = EPOLLIN};
    if (epoll < 0)
      fail("epoll_create1");
    if (epoll_ctl(epoll, EPOLL_CTL_ADD, fd, &event) != 0)
      fail("epoll_ctl");
  } else {
    if (fcntl(fd, F_SETFL, O_ASYNC) != 0)
      fail("fcntl O_ASYNC");
  }
  return 0;
}

/* Select on an alias of `fd` above FD_SETSIZE through a raw bitmap of
 * HIGH_ALIAS_FD + 1 bits that names only the alias. */
static void select_high_alias(int fd, int pselect) {
  struct rlimit limit;
  if (getrlimit(RLIMIT_NOFILE, &limit) != 0)
    fail("getrlimit");
  if (limit.rlim_cur <= HIGH_ALIAS_FD) {
    if (limit.rlim_max <= HIGH_ALIAS_FD)
      fail_message("RLIMIT_NOFILE hard limit is too low for the high alias");
    limit.rlim_cur = HIGH_ALIAS_FD + 1;
    if (setrlimit(RLIMIT_NOFILE, &limit) != 0)
      fail("setrlimit");
  }
  if (dup2(fd, HIGH_ALIAS_FD) != HIGH_ALIAS_FD)
    fail("dup2 high alias");
  enum { BITS = 8 * sizeof(unsigned long) };
  unsigned long readable[HIGH_ALIAS_FD / BITS + 1] = {0};
  readable[HIGH_ALIAS_FD / BITS] = 1UL << (HIGH_ALIAS_FD % BITS);
  long ready;
  if (pselect) {
    struct timespec zero = {0};
    ready = syscall(SYS_pselect6, HIGH_ALIAS_FD + 1, readable, NULL, NULL, &zero, NULL);
  } else {
    struct timeval zero = {0};
    ready = syscall(SYS_select, HIGH_ALIAS_FD + 1, readable, NULL, NULL, &zero);
  }
  if (ready < 0)
    fail("select high alias");
}

static int run_refused_client(const char *port_text, const char *mode) {
  set_deadline();
  if (run_guard_probe(parse_port(port_text), mode) == 0)
    return 0;
  struct sockaddr_in address = loopback_address(parse_port(port_text));
  if (strcmp(mode, "udp") == 0) {
    /* A resolver's IPv6 probe: creating and closing a socket stays allowed. */
    int probe = socket(AF_INET6, SOCK_DGRAM | SOCK_CLOEXEC, 0);
    if (probe < 0)
      fail("socket probe");
    close(probe);
    int udp = socket(AF_INET, SOCK_DGRAM | SOCK_CLOEXEC, 0);
    if (udp < 0)
      fail("socket udp");
    if (sendto(udp, REQUEST, sizeof(REQUEST) - 1, 0, (struct sockaddr *)&address,
               sizeof(address)) < 0)
      fail("sendto udp");
    return 0;
  }
  int socket_fd = socket(AF_INET, SOCK_STREAM | SOCK_CLOEXEC, 0);
  if (socket_fd < 0)
    fail("socket client");
  if (strcmp(mode, "listen") == 0) {
    address.sin_port = 0;
    if (bind(socket_fd, (struct sockaddr *)&address, sizeof(address)) != 0)
      fail("bind client");
    if (listen(socket_fd, 1) != 0)
      fail("listen client");
    return 0;
  }
  if (strcmp(mode, "unspecified") == 0)
    address.sin_addr.s_addr = htonl(INADDR_ANY);
  if (connect(socket_fd, (struct sockaddr *)&address, sizeof(address)) != 0)
    fail("connect client");
  if (strcmp(mode, "truncated") == 0) {
    send_all(socket_fd, REQUEST, 3);
  } else if (strcmp(mode, "sendmsg") == 0) {
    struct iovec part = {.iov_base = (void *)REQUEST, .iov_len = sizeof(REQUEST) - 1};
    struct msghdr message = {.msg_iov = &part, .msg_iovlen = 1};
    if (sendmsg(socket_fd, &message, MSG_NOSIGNAL) < 0)
      fail("sendmsg client");
  } else if (strcmp(mode, "sendfile") == 0) {
    int source = open("/dev/zero", O_RDONLY | O_CLOEXEC);
    if (source < 0)
      fail("open /dev/zero");
    if (sendfile(socket_fd, source, NULL, 1) < 0)
      fail("sendfile client");
  } else if (strcmp(mode, "select-high") == 0 || strcmp(mode, "pselect-high") == 0) {
    select_high_alias(socket_fd, strcmp(mode, "pselect-high") == 0);
  } else if (strcmp(mode, "pselect-mask") == 0) {
    fd_set readable;
    FD_ZERO(&readable);
    FD_SET(socket_fd, &readable);
    struct timespec zero = {0};
    sigset_t mask;
    sigemptyset(&mask);
    if (pselect(socket_fd + 1, &readable, NULL, NULL, &zero, &mask) < 0)
      fail("pselect with a signal mask");
  }
  close(socket_fd);
  return 0;
}

/* Nothing has arrived before the request is sent, so each receive fails with
 * EAGAIN. Linux copies out no address length on failure, and ignores the
 * length pointer, even an invalid one, when no address is requested. */
static void check_failed_recvfrom_leaves_address_length(int fd) {
  char byte;
  struct sockaddr_storage source;
  socklen_t length = ADDRESS_LENGTH_SENTINEL;
  if (recvfrom(fd, &byte, 1, MSG_DONTWAIT, NULL, &length) != -1 || errno != EAGAIN)
    fail("recvfrom without an address before any input");
  if (length != ADDRESS_LENGTH_SENTINEL)
    fail_message("a failed recvfrom without an address wrote its length");
  if (recvfrom(fd, &byte, 1, MSG_DONTWAIT, NULL, (socklen_t *)8) != -1 || errno != EAGAIN)
    fail("recvfrom with an ignored invalid length pointer");
  if (recvfrom(fd, &byte, 1, MSG_DONTWAIT, (struct sockaddr *)&source, &length) != -1 ||
      errno != EAGAIN)
    fail("recvfrom with an address before any input");
  if (length != ADDRESS_LENGTH_SENTINEL)
    fail_message("a failed recvfrom with an address wrote its length");
}

static int run_client(const char *port_text, int mismatch) {
  set_deadline();
  uint16_t port = parse_port(port_text);

  int socket_fd = socket(AF_INET, SOCK_STREAM | SOCK_CLOEXEC, 0);
  if (socket_fd < 0)
    fail("socket client");
  int low_water = PAYLOAD_SIZE;
  if (setsockopt(socket_fd, SOL_SOCKET, SO_RCVLOWAT, &low_water,
                 sizeof(low_water)) != 0)
    fail("setsockopt SO_RCVLOWAT");
  struct sockaddr_in address = loopback_address(port);
  if (connect(socket_fd, (struct sockaddr *)&address, sizeof(address)) != 0)
    fail("connect client");
  check_failed_recvfrom_leaves_address_length(socket_fd);

  int alias = dup(socket_fd);
  if (alias < 0)
    fail("dup client socket");
  pthread_barrier_t start;
  if (pthread_barrier_init(&start, NULL, 3) != 0)
    fail_message("pthread_barrier_init failed");
  struct reader readers[2] = {
      {.fd = socket_fd, .index = 0, .start = &start},
      {.fd = alias, .index = 1, .start = &start},
  };
  pthread_t threads[2];
  for (size_t index = 0; index < 2; ++index) {
    int created = pthread_create(&threads[index], NULL, run_reader, &readers[index]);
    if (created != 0) {
      errno = created;
      fail("pthread_create");
    }
  }
  int barrier = pthread_barrier_wait(&start);
  if (barrier != 0 && barrier != PTHREAD_BARRIER_SERIAL_THREAD)
    fail_message("client start barrier failed");

  char request[sizeof(REQUEST) - 1];
  memcpy(request, REQUEST, sizeof(request));
  if (mismatch)
    request[3] ^= 0x20;
  send_all(socket_fd, request, sizeof(request));

  for (size_t index = 0; index < 2; ++index) {
    int joined = pthread_join(threads[index], NULL);
    if (joined != 0) {
      errno = joined;
      fail("pthread_join");
    }
    if (readers[index].error != 0) {
      errno = readers[index].error;
      fail("competing reader");
    }
    if (readers[index].received != PAYLOAD_SIZE)
      fail_message("competing reader did not consume exactly one input chunk");
  }
  pthread_barrier_destroy(&start);

  int first = memcmp(readers[0].bytes, FIRST_INPUT, PAYLOAD_SIZE) == 0 ? 0 : 1;
  int second = 1 - first;
  if (memcmp(readers[first].bytes, FIRST_INPUT, PAYLOAD_SIZE) != 0 ||
      memcmp(readers[second].bytes, SECOND_INPUT, PAYLOAD_SIZE) != 0)
    fail_message("competing readers did not consume the exact abc/def stream");

  send_all(socket_fd, COMPLETION, sizeof(COMPLETION) - 1);

  char eof_byte;
  ssize_t eof;
  struct sockaddr_storage source;
  socklen_t source_length;
  do {
    source_length = sizeof(source);
    eof = recvfrom(socket_fd, &eof_byte, 1, 0, (struct sockaddr *)&source,
                   &source_length);
  } while (eof < 0 && errno == EINTR);
  if (eof != 0)
    fail_message("peer half-close did not produce EOF after abcdef");
  /* A successful receive that asks for an address gets a TCP socket's: none. */
  if (source_length != 0)
    fail_message("recvfrom on a connected TCP socket reported a source address");

  printf("aggregate=abcdef readiness=pollin,pollin eof=1 outbound_hex=%s "
         "outbound_fnv1a64=%016llx\n",
         OUTBOUND_HEX, (unsigned long long)expected_outbound_digest());
  printf("reader-marker=%d:abc,%d:def\n", readers[first].index,
         readers[second].index);
  close(alias);
  close(socket_fd);
  return 0;
}

/* The descriptors in `set`, below `nfds`, as "3+5" or "-". */
static const char *describe_set(int nfds, const fd_set *set, char *text, size_t size) {
  size_t used = 0;
  text[0] = '\0';
  for (int fd = 0; fd < nfds; ++fd) {
    if (!FD_ISSET(fd, set))
      continue;
    int written = snprintf(text + used, size - used, "%s%d", used ? "+" : "", fd);
    if (written <= 0 || (size_t)written >= size - used)
      fail_message("select description did not fit its buffer");
    used += (size_t)written;
  }
  if (used == 0)
    snprintf(text, size, "-");
  return text;
}

/* Fails unless `actual` holds exactly the descriptors in `expected`. */
static void expect_set(int nfds, const fd_set *actual, const fd_set *expected,
                       const char *what) {
  for (int fd = 0; fd < nfds; ++fd) {
    if (FD_ISSET(fd, actual) != FD_ISSET(fd, expected)) {
      char got[64];
      char want[64];
      fprintf(stderr, "%s: select reported %s, expected %s\n", what,
              describe_set(nfds, actual, got, sizeof(got)),
              describe_set(nfds, expected, want, sizeof(want)));
      exit(1);
    }
  }
}

/* The match protocol on one thread, waiting with the select family on the
 * socket and a pipe. Every result is checked against what Linux returns. */
/* One more entry than Hermit's default RLIMIT_NOFILE (1048576). */
#define POLL_OVER_LIMIT_ENTRIES ((size_t)1048577)

/* Linux refuses a poll or ppoll with more entries than RLIMIT_NOFILE with
 * EINVAL before it reads the array, so a ready socket in its first entry is
 * neither reported nor written. */
static void check_poll_count_limit(int socket_fd) {
  struct rlimit limit;
  if (getrlimit(RLIMIT_NOFILE, &limit) != 0)
    fail("getrlimit");
  if (limit.rlim_cur >= POLL_OVER_LIMIT_ENTRIES)
    fail_message("RLIMIT_NOFILE is too high for the poll count check");
  size_t bytes = POLL_OVER_LIMIT_ENTRIES * sizeof(struct pollfd);
  struct pollfd *entries =
      mmap(NULL, bytes, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  if (entries == MAP_FAILED)
    fail("mmap pollfds");
  /* Every other entry is descriptor -1, which poll ignores, so an
   * implementation that read the array would report only the socket. One
   * memset sets them: a loop over a million entries takes seconds under
   * Hermit's chaos scheduler, which preempts by instruction count, long
   * enough for the controller to give up on the connection. */
  memset(entries, 0xff, bytes);
  entries[0] = (struct pollfd){.fd = socket_fd, .events = POLLOUT};
  errno = 0;
  long ready = syscall(SYS_poll, entries, POLL_OVER_LIMIT_ENTRIES, 0);
  if (ready != -1 || errno != EINVAL)
    fail_message("poll over RLIMIT_NOFILE entries did not fail with EINVAL");
  struct timespec zero = {0};
  errno = 0;
  ready = syscall(SYS_ppoll, entries, POLL_OVER_LIMIT_ENTRIES, &zero, NULL, (size_t)8);
  if (ready != -1 || errno != EINVAL)
    fail_message("ppoll over RLIMIT_NOFILE entries did not fail with EINVAL");
  if (entries[0].revents != 0)
    fail_message("a refused poll wrote revents");
  if (munmap(entries, bytes) != 0)
    fail("munmap pollfds");
}

static int run_select_client(const char *port_text) {
  set_deadline();
  struct sockaddr_in address = loopback_address(parse_port(port_text));
  int socket_fd = socket(AF_INET, SOCK_STREAM | SOCK_CLOEXEC, 0);
  if (socket_fd < 0)
    fail("socket client");
  if (connect(socket_fd, (struct sockaddr *)&address, sizeof(address)) != 0)
    fail("connect client");
  int pipe_fds[2];
  if (pipe2(pipe_fds, O_CLOEXEC) != 0)
    fail("pipe2");
  int closed = dup(pipe_fds[0]);
  if (closed < 0 || close(closed) != 0)
    fail("dup and close");
  int nfds = socket_fd;
  for (int index = 0; index < 2; ++index)
    if (pipe_fds[index] > nfds)
      nfds = pipe_fds[index];
  if (closed > nfds)
    nfds = closed;
  nfds += 1;
  fd_set readable, writable, exceptional, expected, none;
  FD_ZERO(&none);

  /* A closed descriptor fails the call before any wait and leaves the sets
   * alone, even beside a channel. */
  FD_ZERO(&readable);
  FD_SET(socket_fd, &readable);
  FD_SET(closed, &readable);
  expected = readable;
  struct timespec zero = {0};
  long ready = syscall(SYS_pselect6, nfds, &readable, NULL, NULL, &zero, NULL);
  if (ready != -1 || errno != EBADF)
    fail_message("pselect6 naming a closed descriptor did not fail with EBADF");
  expect_set(nfds, &readable, &expected, "EBADF pselect6");

  check_poll_count_limit(socket_fd);

  /* Nothing arrives before the request, so a timed wait expires, clears
   * every set, and reports no time left. */
  FD_ZERO(&readable);
  FD_SET(socket_fd, &readable);
  FD_SET(pipe_fds[0], &readable);
  FD_ZERO(&exceptional);
  FD_SET(socket_fd, &exceptional);
  struct timeval short_wait = {.tv_sec = 0, .tv_usec = 20000};
  ready = syscall(SYS_select, nfds, &readable, NULL, &exceptional, &short_wait);
  if (ready != 0)
    fail_message("select before the request did not time out");
  expect_set(nfds, &readable, &none, "timed-out select readable");
  expect_set(nfds, &exceptional, &none, "timed-out select exceptional");
  if (short_wait.tv_sec != 0 || short_wait.tv_usec != 0)
    fail_message("timed-out select left time in its timeout");

  /* Only the pipe is writable among the descriptors asked about. */
  FD_ZERO(&readable);
  FD_SET(socket_fd, &readable);
  FD_ZERO(&writable);
  FD_SET(pipe_fds[1], &writable);
  ready = syscall(SYS_pselect6, nfds, &readable, &writable, NULL, &zero, NULL);
  FD_ZERO(&expected);
  FD_SET(pipe_fds[1], &expected);
  if (ready != 1)
    fail_message("zero-timeout pselect6 did not report exactly the pipe");
  expect_set(nfds, &readable, &none, "zero-timeout pselect6 readable");
  expect_set(nfds, &writable, &expected, "zero-timeout pselect6 writable");

  send_all(socket_fd, REQUEST, sizeof(REQUEST) - 1);
  FD_ZERO(&readable);
  FD_SET(socket_fd, &readable);
  FD_SET(pipe_fds[0], &readable);
  struct timeval wait = {.tv_sec = CLIENT_PEER_WAIT_SECONDS, .tv_usec = 0};
  ready = select(nfds, &readable, NULL, NULL, &wait);
  FD_ZERO(&expected);
  FD_SET(socket_fd, &expected);
  if (ready != 1)
    fail_message("select did not report the first input");
  expect_set(nfds, &readable, &expected, "first-input select");
  if (wait.tv_sec < 0 || wait.tv_sec > CLIENT_PEER_WAIT_SECONDS || wait.tv_usec < 0 ||
      wait.tv_usec >= 1000000 || (wait.tv_sec == CLIENT_PEER_WAIT_SECONDS && wait.tv_usec != 0))
    fail_message("select reported an impossible remaining time");

  /* With the socket readable and writable and a byte in the pipe, each set
   * reports only its own events: the result counts bits, not descriptors,
   * the pipe's read end is never writable, and nothing is exceptional. */
  if (write(pipe_fds[1], "p", 1) != 1)
    fail("write pipe");
  FD_ZERO(&readable);
  FD_SET(socket_fd, &readable);
  FD_SET(pipe_fds[0], &readable);
  writable = readable;
  exceptional = readable;
  ready = syscall(SYS_pselect6, nfds, &readable, &writable, &exceptional, &zero, NULL);
  if (ready != 3)
    fail_message("pselect6 over three sets did not report three bits");
  FD_ZERO(&expected);
  FD_SET(socket_fd, &expected);
  FD_SET(pipe_fds[0], &expected);
  expect_set(nfds, &readable, &expected, "three-set pselect6 readable");
  FD_ZERO(&expected);
  FD_SET(socket_fd, &expected);
  expect_set(nfds, &writable, &expected, "three-set pselect6 writable");
  expect_set(nfds, &exceptional, &none, "three-set pselect6 exceptional");
  char pipe_byte;
  if (read(pipe_fds[0], &pipe_byte, 1) != 1)
    fail("read pipe");
  char first[PAYLOAD_SIZE];
  receive_exact(socket_fd, first, sizeof(first));
  if (memcmp(first, FIRST_INPUT, sizeof(first)) != 0)
    fail_message("select client received the wrong first input");
  send_all(socket_fd, PROGRESS, sizeof(PROGRESS) - 1);

  /* Glibc's pselect passes the mask wrapper even with no mask, but keeps the
   * kernel's remaining time from its caller; pass the wrapper directly. */
  FD_ZERO(&readable);
  FD_SET(socket_fd, &readable);
  struct timespec long_wait = {.tv_sec = CLIENT_PEER_WAIT_SECONDS, .tv_nsec = 0};
  struct {
    const sigset_t *mask;
    size_t size;
  } no_mask = {NULL, _NSIG / 8};
  ready = syscall(SYS_pselect6, nfds, &readable, NULL, NULL, &long_wait, &no_mask);
  if (ready != 1)
    fail_message("pselect6 did not report the second input");
  expect_set(nfds, &readable, &expected, "second-input pselect6");
  if (long_wait.tv_sec < 0 || long_wait.tv_sec > CLIENT_PEER_WAIT_SECONDS - 1 ||
      long_wait.tv_nsec < 0 || long_wait.tv_nsec >= 1000000000)
    fail_message("pselect6 did not report a remaining time below its timeout");
  char second[PAYLOAD_SIZE];
  receive_exact(socket_fd, second, sizeof(second));
  if (memcmp(second, SECOND_INPUT, sizeof(second)) != 0)
    fail_message("select client received the wrong second input");
  send_all(socket_fd, COMPLETION, sizeof(COMPLETION) - 1);

  /* The peer's half-close is readable to a wait with no timeout. */
  FD_ZERO(&readable);
  FD_SET(socket_fd, &readable);
  ready = select(nfds, &readable, NULL, NULL, NULL);
  if (ready != 1)
    fail_message("select did not report the peer's half-close");
  expect_set(nfds, &readable, &expected, "half-close select");
  char eof_byte;
  if (recv(socket_fd, &eof_byte, 1, 0) != 0)
    fail_message("peer half-close did not produce EOF after abcdef");

  printf("select=ebadf,poll-limit,timeout,pipe-writable,first,three-sets,second,eof "
         "aggregate=abcdef "
         "outbound_hex=%s outbound_fnv1a64=%016llx\n",
         OUTBOUND_HEX, (unsigned long long)expected_outbound_digest());
  close(pipe_fds[0]);
  close(pipe_fds[1]);
  close(socket_fd);
  return 0;
}

static unsigned char backpressure_byte(size_t index) {
  return (unsigned char)(index * 131 + (index >> 9));
}

static uint64_t backpressure_digest(void) {
  uint64_t digest = UINT64_C(14695981039346656037);
  for (size_t index = 0; index < BACKPRESSURE_BYTES; ++index) {
    unsigned char byte = backpressure_byte(index);
    digest = fnv1a64_update(digest, &byte, 1);
  }
  return digest;
}

static void expect_eof(int fd, const char *what) {
  char unexpected;
  for (;;) {
    ssize_t received = recv(fd, &unexpected, 1, 0);
    if (received < 0 && errno == EINTR)
      continue;
    if (received < 0)
      fail(what);
    if (received != 0)
      fail_message("unexpected bytes followed the backpressure protocol");
    return;
  }
}

/* Listens with the small receive buffer, publishes the port, and accepts
 * the data connection, which it returns, and then the control connection.
 * Leaves the listener open for a third connection. */
static int accept_backpressure_connections(const char *port_path,
                                           const char *contact_path, int *listener_out,
                                           int *control) {
  signal(SIGPIPE, SIG_IGN);
  alarm(CONTROLLER_ACCEPT_DEADLINE_SECONDS);
  int listener = socket(AF_INET, SOCK_STREAM | SOCK_CLOEXEC, 0);
  if (listener < 0)
    fail("socket backpressure controller");
  // Accepted sockets inherit the small receive buffer.
  int buffer = BACKPRESSURE_SOCKET_BUFFER;
  if (setsockopt(listener, SOL_SOCKET, SO_RCVBUF, &buffer, sizeof(buffer)) != 0)
    fail("setsockopt SO_RCVBUF");
  struct sockaddr_in address = loopback_address(0);
  if (bind(listener, (struct sockaddr *)&address, sizeof(address)) != 0)
    fail("bind backpressure controller");
  socklen_t address_length = sizeof(address);
  if (getsockname(listener, (struct sockaddr *)&address, &address_length) != 0)
    fail("getsockname backpressure controller");
  if (listen(listener, 3) != 0)
    fail("listen backpressure controller");
  char port[32];
  int port_length = snprintf(port, sizeof(port), "%u\n", ntohs(address.sin_port));
  if (port_length <= 0 || (size_t)port_length >= sizeof(port))
    fail_message("controller port did not fit its publication buffer");
  publish_text(port_path, port);

  // The client connects the data connection first.
  int data = accept4(listener, NULL, NULL, SOCK_CLOEXEC);
  if (data < 0)
    fail("accept data connection");
  alarm(FIXTURE_DEADLINE_SECONDS);
  publish_text(contact_path, "accepted\n");
  *control = accept4(listener, NULL, NULL, SOCK_CLOEXEC);
  if (*control < 0)
    fail("accept control connection");
  set_socket_timeouts(data);
  set_socket_timeouts(*control);
  *listener_out = listener;
  return data;
}

static void receive_go(int control) {
  char go[sizeof(GO) - 1];
  receive_exact(control, go, sizeof(go));
  if (memcmp(go, GO, sizeof(go)) != 0)
    fail_message("control connection mismatch");
}

/* Drains the data connection only after "go". With `report_digest` it
 * reports the digest of what it received; otherwise it requires the fixed
 * stream. */
static int run_backpressure_controller(const char *port_path,
                                       const char *report_path,
                                       const char *contact_path, int report_digest) {
  int listener;
  int control;
  int data = accept_backpressure_connections(port_path, contact_path, &listener, &control);
  close(listener);

  receive_go(control);
  uint64_t digest = UINT64_C(14695981039346656037);
  for (size_t received = 0; received < BACKPRESSURE_BYTES;) {
    unsigned char bytes[BACKPRESSURE_SOCKET_BUFFER];
    size_t wanted = BACKPRESSURE_BYTES - received;
    if (wanted > sizeof(bytes))
      wanted = sizeof(bytes);
    receive_exact(data, bytes, wanted);
    digest = fnv1a64_update(digest, bytes, wanted);
    received += wanted;
  }
  if (!report_digest && digest != backpressure_digest())
    fail_message("backpressure data mismatch");
  send_all(data, DRAINED, sizeof(DRAINED) - 1);
  expect_eof(data, "recv data tail");
  expect_eof(control, "recv control tail");
  close(data);
  close(control);

  char report[256];
  int report_length = snprintf(
      report, sizeof(report), "controller=%s\nbytes=%d\nfnv1a64=%016llx\n",
      report_digest ? "shared" : "backpressure", BACKPRESSURE_BYTES,
      (unsigned long long)digest);
  if (report_length <= 0 || (size_t)report_length >= sizeof(report))
    fail_message("controller report did not fit its fixed buffer");
  publish_text(report_path, report);
  return 0;
}

/* Accepts a third, replacement connection, and after "go" counts what the
 * data and replacement connections each receive until both end. Replies
 * on the data connection once the whole stream arrived. */
static int run_replace_controller(const char *port_path, const char *report_path,
                                  const char *contact_path) {
  int listener;
  int control;
  int data = accept_backpressure_connections(port_path, contact_path, &listener, &control);
  int replacement = accept4(listener, NULL, NULL, SOCK_CLOEXEC);
  if (replacement < 0)
    fail("accept replacement connection");
  set_socket_timeouts(replacement);
  close(listener);

  receive_go(control);
  int fds[2] = {data, replacement};
  size_t counts[2] = {0, 0};
  int open_count = 2;
  int replied = 0;
  while (open_count != 0) {
    struct pollfd ready[2];
    for (int index = 0; index < 2; ++index)
      ready[index] = (struct pollfd){.fd = fds[index], .events = POLLIN};
    int count = poll(ready, 2, 5000);
    if (count < 0 && errno == EINTR)
      continue;
    if (count <= 0)
      fail_message("replace controller saw no progress within five seconds");
    for (int index = 0; index < 2; ++index) {
      if (ready[index].revents == 0)
        continue;
      unsigned char bytes[BACKPRESSURE_SOCKET_BUFFER];
      ssize_t received = recv(fds[index], bytes, sizeof(bytes), 0);
      if (received < 0 && errno == EINTR)
        continue;
      if (received < 0)
        fail("recv replace controller");
      if (received == 0) {
        fds[index] = -1;
        --open_count;
        continue;
      }
      counts[index] += (size_t)received;
    }
    if (!replied && counts[0] == BACKPRESSURE_BYTES) {
      send_all(data, DRAINED, sizeof(DRAINED) - 1);
      replied = 1;
    }
  }
  expect_eof(control, "recv control tail");
  close(data);
  close(replacement);
  close(control);

  char report[256];
  int report_length =
      snprintf(report, sizeof(report), "controller=replace\ndata_bytes=%zu\nreplacement_bytes=%zu\n",
               counts[0], counts[1]);
  if (report_length <= 0 || (size_t)report_length >= sizeof(report))
    fail_message("controller report did not fit its fixed buffer");
  publish_text(report_path, report);
  return 0;
}

struct backpressure {
  int control;
  int blocking;
  /* The data connection, on which this thread first makes a refused
   * nonblocking send; -1 if it makes none. */
  int interleave;
  /* The data connection and a connection this thread installs over its
   * descriptor number, or -1 and -1. */
  int replaced;
  int replacement;
  /* A pipe on which this thread releases the child that rewrites the
   * unsent half of the send buffer, and one on which the child reports the
   * rewrite done; or -1 and -1. */
  int rewrite;
  int rewrite_done;
  /* Set just before "go". */
  atomic_int released;
  pthread_mutex_t lock;
  pthread_cond_t stuck_cond;
  int stuck;
};

static void announce_stuck(struct backpressure *state) {
  pthread_mutex_lock(&state->lock);
  state->stuck = 1;
  pthread_cond_signal(&state->stuck_cond);
  pthread_mutex_unlock(&state->lock);
}

/* Sends "go" once the data thread is stuck; the controller drains nothing
 * before it, so the data thread finishes only if this thread runs. */
static void *run_go_sender(void *raw) {
  struct backpressure *state = raw;
  pthread_mutex_lock(&state->lock);
  while (!state->stuck)
    pthread_cond_wait(&state->stuck_cond, &state->lock);
  pthread_mutex_unlock(&state->lock);
  if (state->blocking) {
    // Let the data thread reach its blocking wait first.
    const struct timespec delay = {.tv_sec = 0, .tv_nsec = 50 * 1000 * 1000};
    nanosleep(&delay, NULL);
  }
  if (state->interleave >= 0) {
    unsigned char byte = backpressure_byte(0);
    ssize_t accepted;
    do
      accepted = send(state->interleave, &byte, 1, MSG_DONTWAIT | MSG_NOSIGNAL);
    while (accepted < 0 && errno == EINTR);
    if (!(accepted < 0 && errno == EAGAIN))
      fail_message("a nonblocking send on the full data connection was not refused");
  }
  if (state->replacement >= 0 && dup2(state->replacement, state->replaced) < 0)
    fail("dup2 replacement");
  if (state->rewrite >= 0) {
    const char release = 1;
    ssize_t written;
    do
      written = write(state->rewrite, &release, 1);
    while (written < 0 && errno == EINTR);
    if (written != 1)
      fail("write rewriter release");
    char done;
    ssize_t received;
    do
      received = read(state->rewrite_done, &done, 1);
    while (received < 0 && errno == EINTR);
    if (received != 1)
      fail_message("the child failed to rewrite the send buffer");
  }
  atomic_store(&state->released, 1);
  send_all(state->control, GO, sizeof(GO) - 1);
  return NULL;
}

static int connect_loopback(uint16_t port, int send_buffer) {
  int fd = socket(AF_INET, SOCK_STREAM | SOCK_CLOEXEC, 0);
  if (fd < 0)
    fail("socket backpressure client");
  if (send_buffer != 0 &&
      setsockopt(fd, SOL_SOCKET, SO_SNDBUF, &send_buffer, sizeof(send_buffer)) != 0)
    fail("setsockopt SO_SNDBUF");
  struct sockaddr_in address = loopback_address(port);
  if (connect(fd, (struct sockaddr *)&address, sizeof(address)) != 0)
    fail("connect backpressure client");
  return fd;
}

enum backpressure_kind { NONBLOCKING, BLOCKING, INTERLEAVED, OBSERVE, REPLACE, SHARED };

/* Writes the stream's bytes [first, first + length) to `out`, each inverted
 * when `invert` is set: inverted is what the shared mode's child writes over
 * the second half. The bytes are backpressure_byte's.
 *
 * Optimized, but not vectorized: with GCC, which builds this fixture for the
 * record and replay tests (the pinned root's 15.3), still one loop branch per
 * byte, so Hermit's timer preempts the fill at the same retired-branch counts,
 * in 8 instructions per branch rather than about 17 at -O0. (Clang ignores
 * this attribute and keeps -O0's loop.) Hermit delivers each
 * preemption exactly by single-stepping its last 1000 or so branches under
 * ptrace, one stop per instruction, and a loaded host makes every stop slow:
 * at -O0 a replay of the shared mode spent most of its time there, 0.74 s
 * of wall time per preemption at load average 280, and overran its 45 s bound
 * at higher load (https://github.com/rrnewton/hermit/issues/4401). */
__attribute__((optimize("O2", "no-tree-vectorize"))) static void fill_stream(unsigned char *out,
                                                                           size_t first,
                                                                           size_t length,
                                                                           int invert) {
  unsigned char mask = invert ? 0xff : 0;
  for (size_t index = 0; index < length; ++index) {
    size_t at = first + index;
    out[index] = (unsigned char)((at * 131 + (at >> 9)) ^ mask);
  }
}

static void pwrite_all(int fd, const unsigned char *bytes, size_t length, off_t offset) {
  while (length != 0) {
    ssize_t written = pwrite(fd, bytes, length, offset);
    if (written < 0 && errno == EINTR)
      continue;
    if (written <= 0)
      fail("pwrite shared stream");
    bytes += written;
    length -= (size_t)written;
    offset += written;
  }
}

/* Maps a temporary file holding the stream with MAP_SHARED, and forks a
 * child that rewrites the file's second half once released through
 * `*rewrite`, reports that through `*rewrite_done`, and exits when `*rewrite`
 * is closed. It must not exit while the send waits: network record refuses a
 * signal during that wait, and its exit sends SIGCHLD. */
static const unsigned char *map_shared_stream(int *rewrite, int *rewrite_done,
                                              pid_t *rewriter) {
  char path[] = "/tmp/network-replay-shared-XXXXXX";
  int fd = mkstemp(path);
  if (fd < 0)
    fail("mkstemp shared stream");
  if (unlink(path) != 0)
    fail("unlink shared stream");
  static unsigned char half[BACKPRESSURE_BYTES / 2];
  for (size_t offset = 0; offset < BACKPRESSURE_BYTES; offset += sizeof(half)) {
    fill_stream(half, offset, sizeof(half), 0);
    pwrite_all(fd, half, sizeof(half), (off_t)offset);
  }
  // The child only writes, so the rewrite fits the controller's timeout.
  static unsigned char rewritten[BACKPRESSURE_BYTES / 2];
  fill_stream(rewritten, sizeof(rewritten), sizeof(rewritten), 1);
  const unsigned char *bytes =
      mmap(NULL, BACKPRESSURE_BYTES, PROT_READ, MAP_SHARED, fd, 0);
  if (bytes == MAP_FAILED)
    fail("mmap shared stream");
  int release[2];
  int done[2];
  if (pipe2(release, O_CLOEXEC) != 0 || pipe2(done, O_CLOEXEC) != 0)
    fail("pipe2 rewriter");
  pid_t child = fork();
  if (child < 0)
    fail("fork rewriter");
  if (child == 0) {
    close(release[1]);
    close(done[0]);
    char byte;
    ssize_t received;
    do
      received = read(release[0], &byte, 1);
    while (received < 0 && errno == EINTR);
    if (received != 1)
      _exit(1);
    pwrite_all(fd, rewritten, sizeof(rewritten), (off_t)sizeof(rewritten));
    ssize_t written;
    do
      written = write(done[1], &byte, 1);
    while (written < 0 && errno == EINTR);
    if (written != 1)
      _exit(1);
    do
      received = read(release[0], &byte, 1);
    while (received < 0 && errno == EINTR);
    _exit(received == 0 ? 0 : 1);
  }
  close(release[0]);
  close(done[1]);
  close(fd);
  *rewrite = release[1];
  *rewrite_done = done[0];
  *rewriter = child;
  return bytes;
}

/* Sends nonblocking from `sent` until the connection refuses; returns the
 * new total. */
static size_t fill_until_refused(int fd, const unsigned char *bytes, size_t sent) {
  for (;;) {
    ssize_t accepted =
        send(fd, bytes + sent, BACKPRESSURE_BYTES - sent, MSG_DONTWAIT | MSG_NOSIGNAL);
    if (accepted > 0)
      sent += (size_t)accepted;
    else if (accepted < 0 && errno == EAGAIN)
      return sent;
    else if (!(accepted < 0 && errno == EINTR))
      fail("send nonblocking fill");
    if (sent == BACKPRESSURE_BYTES)
      fail_message("the data connection accepted everything without refusing");
  }
}

/* Fills the connection until it stays full: acknowledgements still in
 * flight after the first refusal free buffer space, which a later
 * one-byte send would take. */
static size_t fill_until_full(int fd, const unsigned char *bytes) {
  size_t sent = fill_until_refused(fd, bytes, 0);
  for (int quiet = 0; quiet < 3;) {
    const struct timespec settle = {.tv_sec = 0, .tv_nsec = 20 * 1000 * 1000};
    nanosleep(&settle, NULL);
    size_t more = fill_until_refused(fd, bytes, sent);
    quiet = more == sent ? quiet + 1 : 0;
    sent = more;
  }
  return sent;
}

static int run_backpressure_client(const char *port_text, enum backpressure_kind kind) {
  set_deadline();
  uint16_t port = parse_port(port_text);
  // Build the stream before connecting: the controller's receive timeouts
  // start at accept, and filling the buffer is slow under Hermit.
  static unsigned char filled[BACKPRESSURE_BYTES];
  const unsigned char *bytes = filled;
  int rewrite = -1;
  int rewrite_done = -1;
  pid_t rewriter = 0;
  if (kind == SHARED) {
    bytes = map_shared_stream(&rewrite, &rewrite_done, &rewriter);
  } else {
    fill_stream(filled, 0, BACKPRESSURE_BYTES, 0);
  }
  int data = connect_loopback(port, BACKPRESSURE_SOCKET_BUFFER);
  int control = connect_loopback(port, 0);
  int replacement = -1;
  // Linux finishes a send on the connection it started on, which this
  // descriptor keeps open after another descriptor replaces `data`.
  int original = data;
  if (kind == REPLACE) {
    replacement = connect_loopback(port, 0);
    original = dup(data);
    if (original < 0)
      fail("dup data connection");
  }

  struct backpressure state = {.control = control,
                               .blocking = kind != NONBLOCKING,
                               .interleave = kind == INTERLEAVED ? data : -1,
                               .replaced = kind == REPLACE ? data : -1,
                               .replacement = replacement,
                               .rewrite = rewrite,
                               .rewrite_done = rewrite_done};
  if (pthread_mutex_init(&state.lock, NULL) != 0 ||
      pthread_cond_init(&state.stuck_cond, NULL) != 0)
    fail_message("backpressure synchronization setup failed");
  pthread_t go_sender;
  if (pthread_create(&go_sender, NULL, run_go_sender, &state) != 0)
    fail_message("pthread_create go sender failed");

  int released = 0;
  struct timespec before = {0, 0};
  struct timespec after = {0, 0};
  if (kind == INTERLEAVED || kind == SHARED) {
    // The blocking send waits before it sends its first byte.
    size_t sent = fill_until_full(data, bytes);
    announce_stuck(&state);
    send_all(data, bytes + sent, BACKPRESSURE_BYTES - sent);
  } else if (kind == NONBLOCKING) {
    int refused = 0;
    for (size_t sent = 0; sent < BACKPRESSURE_BYTES;) {
      ssize_t accepted = send(data, bytes + sent, BACKPRESSURE_BYTES - sent,
                              MSG_DONTWAIT | MSG_NOSIGNAL);
      if (accepted > 0) {
        sent += (size_t)accepted;
      } else if (accepted < 0 && errno == EAGAIN) {
        if (!refused)
          announce_stuck(&state);
        refused = 1;
        const struct timespec pause = {.tv_sec = 0, .tv_nsec = 1000 * 1000};
        nanosleep(&pause, NULL);
      } else if (!(accepted < 0 && errno == EINTR)) {
        fail("send nonblocking");
      }
    }
    if (!refused)
      fail_message("a nonblocking send on a full buffer never reported EAGAIN");
  } else {
    if (kind == OBSERVE && clock_gettime(CLOCK_MONOTONIC, &before) != 0)
      fail("clock_gettime before send");
    announce_stuck(&state);
    send_all(data, bytes, BACKPRESSURE_BYTES);
    // The peer drains nothing before "go", so the send cannot complete
    // before the go thread set this.
    released = atomic_load(&state.released);
    if (kind == OBSERVE && clock_gettime(CLOCK_MONOTONIC, &after) != 0)
      fail("clock_gettime after send");
  }
  char reply[sizeof(DRAINED) - 1];
  receive_exact(original, reply, sizeof(reply));
  if (memcmp(reply, DRAINED, sizeof(reply)) != 0)
    fail_message("backpressure reply mismatch");
  if (pthread_join(go_sender, NULL) != 0)
    fail_message("pthread_join go sender failed");
  if (kind == REPLACE) {
    close(original);
    close(replacement);
  }
  if (kind == SHARED) {
    // The send is done, so the child may exit now.
    close(rewrite);
    close(rewrite_done);
    int status;
    if (waitpid(rewriter, &status, 0) != rewriter)
      fail("waitpid rewriter");
    if (!WIFEXITED(status) || WEXITSTATUS(status) != 0)
      fail_message("the rewriting child failed");
  }
  close(data);
  close(control);
  if (kind == OBSERVE) {
    printf("observe released=%d\n", released);
    printf("observe clock_before=%lld.%09ld clock_after=%lld.%09ld\n",
           (long long)before.tv_sec, before.tv_nsec, (long long)after.tv_sec,
           after.tv_nsec);
  }
  const char *names[] = {"nonblocking", "blocking", "interleaved",
                         "observe",     "replace",  "shared"};
  printf("backpressure=%s bytes=%d reply=ok\n", names[kind], BACKPRESSURE_BYTES);
  return 0;
}

static int run_poll_high_word(void) {
  int fds[2];
  if (pipe(fds) != 0)
    fail("pipe");
  if (write(fds[1], "x", 1) != 1)
    fail("write pipe");
  // The low word is 1. Taken whole, the count is more entries than any array
  // can hold.
  const unsigned long nfds = 0xffffffff00000001UL;
  struct pollfd polled = {.fd = fds[0], .events = POLLIN};
  long poll_ready = syscall(SYS_poll, &polled, nfds, 0);
  struct pollfd ppolled = {.fd = fds[0], .events = POLLIN};
  struct timespec zero = {0, 0};
  long ppoll_ready = syscall(SYS_ppoll, &ppolled, nfds, &zero, NULL, (size_t)8);
  printf("poll-high-word poll=%ld/%s ppoll=%ld/%s\n", poll_ready,
         polled.revents == POLLIN ? "pollin" : "other", ppoll_ready,
         ppolled.revents == POLLIN ? "pollin" : "other");
  return poll_ready == 1 && ppoll_ready == 1 ? 0 : 1;
}

int main(int argc, char **argv) {
  if (argc == 2 && strcmp(argv[1], "poll-high-word") == 0)
    return run_poll_high_word();
  if (argc == 5 && strcmp(argv[1], "controller") == 0)
    return run_controller(argv[2], argv[3], argv[4]);
  if (argc == 5 && strcmp(argv[1], "backpressure-controller") == 0)
    return run_backpressure_controller(argv[2], argv[3], argv[4], 0);
  if (argc == 5 && strcmp(argv[1], "shared-controller") == 0)
    return run_backpressure_controller(argv[2], argv[3], argv[4], 1);
  if (argc == 5 && strcmp(argv[1], "replace-controller") == 0)
    return run_replace_controller(argv[2], argv[3], argv[4]);
  if (argc == 4 && strcmp(argv[1], "client") == 0) {
    if (strcmp(argv[3], "match") == 0)
      return run_client(argv[2], 0);
    if (strcmp(argv[3], "mismatch") == 0)
      return run_client(argv[2], 1);
    if (strcmp(argv[3], "select") == 0)
      return run_select_client(argv[2]);
    if (strcmp(argv[3], "backpressure-nonblocking") == 0)
      return run_backpressure_client(argv[2], NONBLOCKING);
    if (strcmp(argv[3], "backpressure-blocking") == 0)
      return run_backpressure_client(argv[2], BLOCKING);
    if (strcmp(argv[3], "backpressure-interleaved") == 0)
      return run_backpressure_client(argv[2], INTERLEAVED);
    if (strcmp(argv[3], "backpressure-observe") == 0)
      return run_backpressure_client(argv[2], OBSERVE);
    if (strcmp(argv[3], "backpressure-replace") == 0)
      return run_backpressure_client(argv[2], REPLACE);
    if (strcmp(argv[3], "backpressure-shared") == 0)
      return run_backpressure_client(argv[2], SHARED);
    const char *refused[] = {"truncated", "sendmsg",    "sendfile", "unspecified",
                             "udp",       "listen",     "ipv6-24",  "netlink",
                             "abstract",  "rcvtimeo",   "scm-rights", "epoll",
                             "async",     "rcvtimeo-negative", "ifindex", "procnet",
                             "select-high", "pselect-high", "pselect-mask"};
    for (size_t index = 0; index < sizeof(refused) / sizeof(refused[0]); ++index)
      if (strcmp(argv[3], refused[index]) == 0)
        return run_refused_client(argv[2], argv[3]);
  }
  fprintf(stderr,
          "usage: %s poll-high-word | "
          "controller|backpressure-controller|shared-controller|"
          "replace-controller PORT_FILE REPORT_FILE CONTACT_FILE | client PORT "
          "match|mismatch|select|backpressure-nonblocking|backpressure-blocking|"
          "backpressure-interleaved|backpressure-observe|backpressure-replace|"
          "backpressure-shared|truncated|sendmsg|sendfile|unspecified|udp|listen|"
          "ipv6-24|netlink|abstract|rcvtimeo|scm-rights|epoll|async|"
          "rcvtimeo-negative|ifindex|procnet|select-high|pselect-high|pselect-mask\n",
          argv[0]);
  return 2;
}
