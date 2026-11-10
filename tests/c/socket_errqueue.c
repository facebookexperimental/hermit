// A receive with MSG_ERRQUEUE reads a socket's error queue. On an IPv4 or
// IPv6 socket Linux never waits for it, whether or not the socket is blocking:
// an empty queue is -1 EAGAIN at once. AF_UNIX ignores the flag, so there the
// call is a plain receive, which on a blocking socket waits for data. Every
// line is what Linux prints.
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <linux/errqueue.h>
#include <netinet/in.h>
#include <poll.h>
#include <pthread.h>
#include <stddef.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>

static void result(const char *what, long r) {
  if (r < 0) {
    printf("%s: -1 (%s)\n", what, strerror(errno));
  } else {
    printf("%s: %ld\n", what, r);
  }
}

static void check(int ok, const char *what) {
  if (!ok) {
    perror(what);
    _exit(2);
  }
}

// Each receive call with MSG_ERRQUEUE on `fd`, whose error queue is empty.
static void read_empty_error_queue(const char *kind, int fd) {
  char buffer[16];
  char control[64];
  char what[64];
  struct iovec iov = {buffer, sizeof(buffer)};
  struct mmsghdr message = {.msg_hdr = {.msg_iov = &iov,
                                        .msg_iovlen = 1,
                                        .msg_control = control,
                                        .msg_controllen = sizeof(control)}};
  snprintf(what, sizeof(what), "%s recvfrom", kind);
  result(what, recvfrom(fd, buffer, sizeof(buffer), MSG_ERRQUEUE, NULL, NULL));
  snprintf(what, sizeof(what), "%s recvmsg", kind);
  result(what, recvmsg(fd, &message.msg_hdr, MSG_ERRQUEUE));
  snprintf(what, sizeof(what), "%s recvmmsg", kind);
  result(what, recvmmsg(fd, &message, 1, MSG_ERRQUEUE, NULL));
}

struct later {
  int fd;
  const char *data;
};

// Wait, then send `data` on `fd`.
static void *send_later(void *argument) {
  struct later *later = argument;
  usleep(50000);
  check(send(later->fd, later->data, strlen(later->data), 0) ==
            (long)strlen(later->data),
        "late send");
  return NULL;
}

// On AF_UNIX MSG_ERRQUEUE is a plain receive: queued data is returned, an
// empty queue fails with EAGAIN only under MSG_DONTWAIT, and a blocking receive
// waits for data, here sent by another thread later.
static void unix_receives(const char *kind, int sender, int receiver) {
  char buffer[16] = {0};
  char what[64];
  struct iovec iov = {buffer, sizeof(buffer) - 1};
  struct mmsghdr message = {.msg_hdr = {.msg_iov = &iov, .msg_iovlen = 1}};
  struct later later = {sender, "late"};
  pthread_t thread;

  check(send(sender, "abc", 3, 0) == 3, "send");
  snprintf(what, sizeof(what), "%s queued recv", kind);
  result(what, recv(receiver, buffer, sizeof(buffer) - 1, MSG_ERRQUEUE));
  snprintf(what, sizeof(what), "%s empty recv MSG_DONTWAIT", kind);
  result(what, recv(receiver, buffer, sizeof(buffer) - 1,
                    MSG_ERRQUEUE | MSG_DONTWAIT));

  for (int call = 0; call < 3; call++) {
    long r = -1;
    memset(buffer, 0, sizeof(buffer));
    check(pthread_create(&thread, NULL, send_later, &later) == 0,
          "pthread_create");
    if (call == 0) {
      snprintf(what, sizeof(what), "%s blocking recvfrom", kind);
      r = recvfrom(receiver, buffer, sizeof(buffer) - 1, MSG_ERRQUEUE, NULL,
                   NULL);
    } else if (call == 1) {
      snprintf(what, sizeof(what), "%s blocking recvmsg", kind);
      r = recvmsg(receiver, &message.msg_hdr, MSG_ERRQUEUE);
    } else {
      snprintf(what, sizeof(what), "%s blocking recvmmsg", kind);
      r = recvmmsg(receiver, &message, 1, MSG_ERRQUEUE, NULL);
    }
    result(what, r);
    printf("%s data: %s\n", what, buffer);
    pthread_join(thread, NULL);
  }
}

static void udp(void) {
  struct sockaddr_in address = {.sin_family = AF_INET};
  struct sockaddr_in closed = {.sin_family = AF_INET};
  socklen_t length = sizeof(address);
  int on = 1;
  int fd = socket(AF_INET, SOCK_DGRAM, 0);
  int gone = socket(AF_INET, SOCK_DGRAM, 0);
  char buffer[16];
  char control[256];
  struct iovec iov = {buffer, sizeof(buffer)};
  struct msghdr message = {.msg_iov = &iov,
                           .msg_iovlen = 1,
                           .msg_control = control,
                           .msg_controllen = sizeof(control)};
  struct pollfd poll_fd = {.fd = fd};
  check(fd >= 0, "udp socket");
  read_empty_error_queue("udp unbound", fd);

  // A datagram to a closed port comes back as an ICMP port unreachable,
  // which IP_RECVERR queues on the sender's error queue. The sender binds
  // first, so the port closed after it cannot become the sender's own.
  address.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
  closed.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
  check(bind(fd, (struct sockaddr *)&address, sizeof(address)) == 0 &&
            setsockopt(fd, SOL_IP, IP_RECVERR, &on, sizeof(on)) == 0,
        "udp bind");
  check(gone >= 0 &&
            bind(gone, (struct sockaddr *)&closed, sizeof(closed)) == 0 &&
            getsockname(gone, (struct sockaddr *)&closed, &length) == 0 &&
            close(gone) == 0,
        "closed port");
  check(sendto(fd, "ping", 4, 0, (struct sockaddr *)&closed, sizeof(closed)) ==
            4,
        "udp sendto");
  result("udp poll after a send to a closed port", poll(&poll_fd, 1, -1));
  printf("udp poll revents POLLERR: %s\n",
         poll_fd.revents & POLLERR ? "yes" : "no");
  result("udp error queue recvmsg", recvmsg(fd, &message, MSG_ERRQUEUE));
  printf("udp error queue msg_flags MSG_ERRQUEUE: %s\n",
         message.msg_flags & MSG_ERRQUEUE ? "yes" : "no");
  for (struct cmsghdr *header = CMSG_FIRSTHDR(&message); header != NULL;
       header = CMSG_NXTHDR(&message, header)) {
    if (header->cmsg_level == SOL_IP && header->cmsg_type == IP_RECVERR) {
      struct sock_extended_err error;
      memcpy(&error, CMSG_DATA(header), sizeof(error));
      printf("udp error: %s, origin %d, ICMP type %d code %d\n",
             strerror(error.ee_errno), error.ee_origin, error.ee_type,
             error.ee_code);
    }
  }
  read_empty_error_queue("udp drained", fd);
  close(fd);

  fd = socket(AF_INET6, SOCK_DGRAM, 0);
  if (fd < 0) {
    printf("udp6 socket: %s\n", strerror(errno));
  } else {
    read_empty_error_queue("udp6 unbound", fd);
    close(fd);
  }
}

static void tcp(void) {
  struct sockaddr_in address = {.sin_family = AF_INET};
  socklen_t length = sizeof(address);
  int listener = socket(AF_INET, SOCK_STREAM, 0);
  int client = socket(AF_INET, SOCK_STREAM, 0);
  int server;
  address.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
  check(listener >= 0 && client >= 0, "tcp socket");
  read_empty_error_queue("tcp unconnected", client);
  check(bind(listener, (struct sockaddr *)&address, sizeof(address)) == 0 &&
            getsockname(listener, (struct sockaddr *)&address, &length) == 0 &&
            listen(listener, 1) == 0 &&
            connect(client, (struct sockaddr *)&address, length) == 0,
        "tcp");
  server = accept(listener, NULL, NULL);
  check(server >= 0, "accept");
  read_empty_error_queue("tcp connected", client);
  read_empty_error_queue("tcp accepted", server);
  close(server);
  close(client);
  close(listener);
}

static void unix_sockets(void) {
  int pair[2];
  struct sockaddr_un address = {.sun_family = AF_UNIX};
  socklen_t length;
  int sender = socket(AF_UNIX, SOCK_DGRAM, 0);
  int receiver = socket(AF_UNIX, SOCK_DGRAM, 0);

  check(socketpair(AF_UNIX, SOCK_DGRAM, 0, pair) == 0, "socketpair");
  unix_receives("unix datagram pair", pair[0], pair[1]);
  close(pair[0]);
  close(pair[1]);
  check(socketpair(AF_UNIX, SOCK_STREAM, 0, pair) == 0, "socketpair");
  unix_receives("unix stream pair", pair[0], pair[1]);
  close(pair[0]);
  close(pair[1]);

  // Sockets socket(2) made rather than socketpair(2), named in the abstract
  // namespace.
  length = offsetof(struct sockaddr_un, sun_path) + 1 +
           snprintf(address.sun_path + 1, sizeof(address.sun_path) - 1,
                    "socket-errqueue-%d", getpid());
  check(sender >= 0 && receiver >= 0 &&
            bind(receiver, (struct sockaddr *)&address, length) == 0 &&
            connect(sender, (struct sockaddr *)&address, length) == 0,
        "unix datagram socket");
  unix_receives("unix datagram socket", sender, receiver);
  close(sender);
  close(receiver);
}

int main(void) {
  setvbuf(stdout, NULL, _IOLBF, 0);
  udp();
  tcp();
  unix_sockets();
  return 0;
}
