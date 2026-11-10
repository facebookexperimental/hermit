// A blocking receive on a datagram socket whose read side is shut down
// returns 0 at once on Linux, even with nothing queued; a nonblocking one
// fails with EAGAIN. Every line below is what Linux prints, so the output must
// not depend on whether a reader blocked before or after the shutdown.
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <pthread.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/uio.h>
#include <unistd.h>

static void result(const char *what, long r, int error) {
  if (r < 0) {
    printf("%s: -1 (%s)\n", what, strerror(error));
  } else {
    printf("%s: %ld\n", what, r);
  }
}

static int bound_udp(struct sockaddr_in *address) {
  int fd = socket(AF_INET, SOCK_DGRAM, 0);
  socklen_t length = sizeof(*address);
  memset(address, 0, sizeof(*address));
  address->sin_family = AF_INET;
  address->sin_addr.s_addr = htonl(INADDR_LOOPBACK);
  if (fd < 0 || bind(fd, (struct sockaddr *)address, sizeof(*address)) != 0 ||
      getsockname(fd, (struct sockaddr *)address, &length) != 0) {
    perror("udp socket");
    _exit(2);
  }
  return fd;
}

struct reader {
  int fd;
  long r;
  int error;
  socklen_t address_length;
  char data[16];
};

static void *receive_from(void *argument) {
  struct reader *reader = argument;
  struct sockaddr_in from;
  reader->address_length = sizeof(from);
  errno = 0;
  reader->r = recvfrom(reader->fd, reader->data, sizeof(reader->data) - 1, 0,
                       (struct sockaddr *)&from, &reader->address_length);
  reader->error = errno;
  return NULL;
}

struct drain {
  int fd;
  int received;
};

// Wait, then take every datagram a sender queued while it waited for room.
static void *drain_later(void *argument) {
  struct drain *drain = argument;
  char byte;
  usleep(50000);
  while (drain->received < 64 && recv(drain->fd, &byte, 1, 0) == 1) {
    drain->received++;
  }
  return NULL;
}

static void *read_from(void *argument) {
  struct reader *reader = argument;
  errno = 0;
  reader->r = read(reader->fd, reader->data, sizeof(reader->data) - 1);
  reader->error = errno;
  return NULL;
}

// Shut down the read side of a socket another thread may be receiving from,
// then join that thread, as a server does when it stops its receive thread.
static void shut_down_reader(const char *label, void *(*receive)(void *), int fd) {
  struct reader reader = {.fd = fd};
  pthread_t thread;
  char what[96];
  pthread_create(&thread, NULL, receive, &reader);
  usleep(50000);
  errno = 0;
  int r = shutdown(fd, SHUT_RD);
  int error = errno;
  snprintf(what, sizeof(what), "%s shutdown(SHUT_RD)", label);
  result(what, r, error);
  pthread_join(thread, NULL);
  snprintf(what, sizeof(what), "%s blocked receive after the shutdown", label);
  result(what, reader.r, reader.error);
  if (receive == receive_from) {
    printf("%s address length written: %u\n", label, (unsigned)reader.address_length);
  }
}

int main(void) {
  setvbuf(stdout, NULL, _IONBF, 0);
  struct sockaddr_in address;
  char data[16];

  // The reader that keeps waiting: a datagram sent after it blocked arrives.
  int open_fd = bound_udp(&address);
  struct reader reader = {.fd = open_fd};
  pthread_t thread;
  pthread_create(&thread, NULL, receive_from, &reader);
  usleep(50000);
  sendto(open_fd, "hello", 5, 0, (struct sockaddr *)&address, sizeof(address));
  pthread_join(thread, NULL);
  reader.data[reader.r > 0 ? reader.r : 0] = '\0';
  printf("udp open: blocked receive got \"%s\": %ld\n", reader.data, reader.r);
  close(open_fd);

  // The reported hang: the reader thread never returned, so the join waited.
  int udp = bound_udp(&address);
  shut_down_reader("udp", receive_from, udp);
  close(udp);

  // A datagram queued before the shutdown is still delivered first.
  udp = bound_udp(&address);
  sendto(udp, "ping", 4, 0, (struct sockaddr *)&address, sizeof(address));
  shutdown(udp, SHUT_RD);
  errno = 0;
  long r = recv(udp, data, sizeof(data), 0);
  result("udp queued datagram after the shutdown", r, errno);
  errno = 0;
  r = recv(udp, data, sizeof(data), 0);
  result("udp then end of file", r, errno);
  errno = 0;
  r = recv(udp, data, sizeof(data), MSG_DONTWAIT);
  result("udp MSG_DONTWAIT after the shutdown", r, errno);

  struct sockaddr_in from;
  struct iovec iov = {.iov_base = data, .iov_len = sizeof(data)};
  struct msghdr message = {
      .msg_name = &from, .msg_namelen = sizeof(from), .msg_iov = &iov, .msg_iovlen = 1};
  errno = 0;
  r = recvmsg(udp, &message, 0);
  result("udp recvmsg after the shutdown", r, errno);
  printf("udp recvmsg msg_namelen %u msg_flags %d\n", (unsigned)message.msg_namelen,
         message.msg_flags);

  struct mmsghdr messages[2];
  struct iovec iovs[2] = {{data, 8}, {data + 8, 8}};
  memset(messages, 0, sizeof(messages));
  for (int i = 0; i < 2; i++) {
    messages[i].msg_hdr.msg_iov = &iovs[i];
    messages[i].msg_hdr.msg_iovlen = 1;
    messages[i].msg_len = 99;
  }
  errno = 0;
  r = recvmmsg(udp, messages, 2, 0, NULL);
  result("udp recvmmsg after the shutdown", r, errno);
  printf("udp recvmmsg msg_len %u %u\n", messages[0].msg_len, messages[1].msg_len);
  close(udp);

  // A datagram socketpair end, read with read(2).
  int pair[2];
  if (socketpair(AF_UNIX, SOCK_DGRAM, 0, pair) != 0) {
    perror("socketpair");
    return 2;
  }
  shut_down_reader("unix datagram", read_from, pair[0]);
  // The end of file leaves the socket blocking as the guest opened it: a send
  // to a peer whose queue is full waits until the peer drains it.
  int small = 4096;
  setsockopt(pair[0], SOL_SOCKET, SO_SNDBUF, &small, sizeof(small));
  struct drain drain = {.fd = pair[1]};
  pthread_create(&thread, NULL, drain_later, &drain);
  int sent = 0;
  for (int i = 0; i < 64; i++) {
    sent += send(pair[0], "d", 1, 0) == 1;
  }
  pthread_join(thread, NULL);
  printf("unix datagram sent %d, peer drained %d\n", sent, drain.received);
  close(pair[0]);
  close(pair[1]);

  // Shutting a TCP listener's read side disconnects it instead (shutdown
  // returns 0) and leaves the read side open, so once it connects elsewhere a
  // blocked receive on it waits for data again.
  struct sockaddr_in listener_address, server_address;
  int listener = socket(AF_INET, SOCK_STREAM, 0);
  int server = socket(AF_INET, SOCK_STREAM, 0);
  socklen_t length = sizeof(server_address);
  memset(&listener_address, 0, sizeof(listener_address));
  listener_address.sin_family = AF_INET;
  listener_address.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
  server_address = listener_address;
  if (bind(listener, (struct sockaddr *)&listener_address, sizeof(listener_address)) != 0 ||
      listen(listener, 1) != 0 ||
      bind(server, (struct sockaddr *)&server_address, sizeof(server_address)) != 0 ||
      listen(server, 1) != 0 ||
      getsockname(server, (struct sockaddr *)&server_address, &length) != 0) {
    perror("tcp listeners");
    return 2;
  }
  errno = 0;
  r = shutdown(listener, SHUT_RD);
  result("tcp listener shutdown(SHUT_RD)", r, errno);
  errno = 0;
  r = connect(listener, (struct sockaddr *)&server_address, sizeof(server_address));
  result("tcp former listener connect", r, errno);
  int accepted = accept(server, NULL, NULL);
  reader = (struct reader){.fd = listener};
  pthread_create(&thread, NULL, read_from, &reader);
  usleep(50000);
  send(accepted, "tcp", 3, 0);
  pthread_join(thread, NULL);
  result("tcp blocked receive after the reconnect", reader.r, reader.error);
  close(accepted);
  close(listener);
  close(server);
  return 0;
}
