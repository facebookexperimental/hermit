/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// SO_SNDTIMEO and SO_RCVTIMEO bound how long a blocking socket call waits:
// Linux fails it with -1 EAGAIN once the timeout passes (a TCP connect
// with EINPROGRESS instead), and a negative
// second count succeeds as an already-expired timeout. The recvmmsg timeout
// is checked before the descriptor: a timespec Linux cannot use fails with
// -1 EINVAL at once. Every line is what Linux prints; host-dependent counts
// and elapsed times are never printed. The one elapsed-time comparison
// ("waited >= timeout") relies on the kernel storing the timeout in
// jiffies that round the wait up to at least the asked duration: true at
// CONFIG_HZ=1000 (this host, checked in /boot/config), where 100 ms falls
// in a timer-wheel level that rounds up; at HZ=250 a native wait can end
// a tick early (96-100 ms) and that line would print "no" natively too,
// so the comparison stays honest and a mismatch there is a host-HZ flake
// to name, not a result to relax.
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <netinet/in.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/time.h>
#include <sys/uio.h>
#include <sys/un.h>
#include <time.h>
#include <unistd.h>

static long now_ms(void) {
  struct timespec ts;
  clock_gettime(CLOCK_MONOTONIC, &ts);
  return ts.tv_sec * 1000 + ts.tv_nsec / 1000000;
}

static void result(const char *what, long r) {
  if (r < 0) {
    printf("%s: -1 (%s)\n", what, strerror(errno));
  } else {
    printf("%s: %ld\n", what, r);
  }
}

// Whether to make recvmmsg and sendmmsg calls: record and replay record
// neither, as in socket_dontwait.c.
static int batched = 1;

static void check(int ok, const char *what) {
  if (!ok) {
    perror(what);
    _exit(2);
  }
}

// The send timeout alone, without printing: filling a backlog takes a
// host-dependent number of connects, so their count never reaches the
// output.
static void set_send_timeout_quiet(int fd, long sec, long usec) {
  struct timeval value = {.tv_sec = sec, .tv_usec = usec};
  check(setsockopt(fd, SOL_SOCKET, SO_SNDTIMEO, &value, sizeof(value)) == 0,
        "filler setsockopt SO_SNDTIMEO");
}

// A listener whose port the next section's listener may be assigned
// again: let the bind reuse it while the accepted connection lingers.
static void reuse_address(int fd) {
  int one = 1;
  check(setsockopt(fd, SOL_SOCKET, SO_REUSEADDR, &one, sizeof(one)) == 0,
        "setsockopt SO_REUSEADDR");
}

static void set_timeouts(const char *kind, int fd, long sec, long usec) {
  struct timeval value = {.tv_sec = sec, .tv_usec = usec};
  char what[64];
  snprintf(what, sizeof(what), "%s setsockopt SO_SNDTIMEO", kind);
  result(what, setsockopt(fd, SOL_SOCKET, SO_SNDTIMEO, &value, sizeof(value)));
  snprintf(what, sizeof(what), "%s setsockopt SO_RCVTIMEO", kind);
  result(what, setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &value, sizeof(value)));
}

// A usec count outside [0, 1e6) is EDOM, a short option is EINVAL, and
// neither changes the timeout already set.
static void setsockopt_errors(void) {
  int pair[2];
  char buffer[4];
  struct timeval good = {.tv_sec = 0, .tv_usec = 100000};
  struct timeval bad = {.tv_sec = 0, .tv_usec = 2000000};
  struct timeval negative = {.tv_sec = 0, .tv_usec = -1};
  check(socketpair(AF_UNIX, SOCK_DGRAM, 0, pair) == 0, "socketpair");
  check(setsockopt(pair[1], SOL_SOCKET, SO_RCVTIMEO, &good, sizeof(good)) == 0,
        "setsockopt good");
  result("setsockopt usec too large",
         setsockopt(pair[1], SOL_SOCKET, SO_RCVTIMEO, &bad, sizeof(bad)));
  result("setsockopt usec negative",
         setsockopt(pair[1], SOL_SOCKET, SO_SNDTIMEO, &negative,
                    sizeof(negative)));
  result("setsockopt short option",
         setsockopt(pair[1], SOL_SOCKET, SO_RCVTIMEO, &good, 4));
  result("recv keeps good timeout", recv(pair[1], buffer, sizeof(buffer), 0));
  close(pair[0]);
  close(pair[1]);
}

// A negative second count is an already-expired timeout, so the next
// blocking call fails at once.
static void negative_seconds(void) {
  int pair[2];
  char buffer[4];
  check(socketpair(AF_UNIX, SOCK_DGRAM, 0, pair) == 0, "socketpair");
  set_timeouts("negative seconds", pair[1], -1, 0);
  result("negative seconds recv", recv(pair[1], buffer, sizeof(buffer), 0));
  close(pair[0]);
  close(pair[1]);
}

struct later {
  int fd;
};

static volatile sig_atomic_t usr1_seen;

static void on_usr1(int signal_number) {
  (void)signal_number;
  usr1_seen = 1;
}

// The abstract AF_UNIX address both connect sections use, unique to this
// process so parallel native runs cannot collide.
static struct sockaddr_un abstract_unix_address(const char *section) {
  struct sockaddr_un address = {.sun_family = AF_UNIX};
  snprintf(address.sun_path + 1, sizeof(address.sun_path) - 1,
           "socket_timeout-%s-%d", section, (int)getpid());
  return address;
}

// Wait, then queue one datagram for the peer to receive.
static void *send_later(void *argument) {
  struct later *later = argument;
  usleep(50000);
  check(send(later->fd, "late", 4, 0) == 4, "late send");
  return NULL;
}

// Wait past a 100 ms timeout, then queue one datagram.
static void *send_later_slow(void *argument) {
  struct later *later = argument;
  usleep(200000);
  check(send(later->fd, "late", 4, 0) == 4, "slow late send");
  return NULL;
}

// Wait, then make room by receiving everything queued.
static void *drain_later(void *argument) {
  struct later *later = argument;
  char buffer[4096];
  usleep(50000);
  while (recv(later->fd, buffer, sizeof(buffer), MSG_DONTWAIT) > 0) {
  }
  check(send(later->fd, "room", 4, 0) == 4 || errno == EAGAIN, "drain send");
  return NULL;
}

// Blocking receives on an empty datagram socket expire with the receive
// timeout, having waited at least that long by the guest's own clock;
// data queued before the deadline, or sent while the receive waits, is
// still delivered, and with no timeout the same receive waits for the
// peer.
static void receive_timeout(void) {
  int pair[2];
  char buffer[16] = {0};
  struct iovec iov = {buffer, sizeof(buffer)};
  struct mmsghdr message = {.msg_hdr = {.msg_iov = &iov, .msg_iovlen = 1}};
  pthread_t thread;
  struct later later;
  long began;
  check(socketpair(AF_UNIX, SOCK_DGRAM, 0, pair) == 0, "socketpair");
  set_timeouts("receive timeout", pair[1], 0, 100000);
  began = now_ms();
  result("timed recvfrom empty",
         recvfrom(pair[1], buffer, sizeof(buffer), 0, NULL, NULL));
  printf("waited >= timeout: %s\n", now_ms() - began >= 100 ? "yes" : "no");
  result("timed recvmsg empty", recvmsg(pair[1], &message.msg_hdr, 0));
  if (batched) {
    result("timed recvmmsg empty", recvmmsg(pair[1], &message, 1, 0, NULL));
  }
  result("dontwait recv empty",
         recv(pair[1], buffer, sizeof(buffer), MSG_DONTWAIT));
  check(send(pair[0], "here", 4, 0) == 4, "queued send");
  result("timed recv queued", recv(pair[1], buffer, sizeof(buffer), 0));
  printf("timed recv queued data: %.4s\n", buffer);
  set_timeouts("mid-wait", pair[1], 5, 0);
  later.fd = pair[0];
  check(pthread_create(&thread, NULL, send_later, &later) == 0,
        "pthread_create");
  memset(buffer, 0, sizeof(buffer));
  result("mid-wait recv", recv(pair[1], buffer, sizeof(buffer), 0));
  printf("mid-wait data: %.4s\n", buffer);
  pthread_join(thread, NULL);
  set_timeouts("no timeout", pair[1], 0, 0);
  later.fd = pair[0];
  check(pthread_create(&thread, NULL, send_later, &later) == 0,
        "pthread_create");
  memset(buffer, 0, sizeof(buffer));
  result("untimed recv waits", recv(pair[1], buffer, sizeof(buffer), 0));
  printf("untimed recv data: %.4s\n", buffer);
  pthread_join(thread, NULL);
  close(pair[0]);
  close(pair[1]);
}

// Blocking sends to a full datagram socket expire with the send timeout;
// with no timeout the same send waits until the peer drains the queue.
static void send_timeout(void) {
  int pair[2];
  char byte = 'x';
  struct iovec iov = {&byte, 1};
  struct mmsghdr message = {.msg_hdr = {.msg_iov = &iov, .msg_iovlen = 1}};
  int sent = 0;
  pthread_t thread;
  struct later later;
  check(socketpair(AF_UNIX, SOCK_DGRAM, 0, pair) == 0, "socketpair");
  while (send(pair[0], "", 1, MSG_DONTWAIT) == 1) {
    sent++;
  }
  printf("send fill ends after %s sends: %s\n", sent > 0 ? "some" : "no",
         strerror(errno));
  set_timeouts("send timeout", pair[0], 0, 100000);
  result("timed sendto full", sendto(pair[0], &byte, 1, 0, NULL, 0));
  result("timed sendmsg full", sendmsg(pair[0], &message.msg_hdr, 0));
  if (batched) {
    result("timed sendmmsg full", sendmmsg(pair[0], &message, 1, 0));
  }
  result("dontwait send full", send(pair[0], &byte, 1, MSG_DONTWAIT));
  set_timeouts("mid-wait send", pair[0], 5, 0);
  later.fd = pair[1];
  check(pthread_create(&thread, NULL, drain_later, &later) == 0,
        "pthread_create");
  result("mid-wait send", send(pair[0], &byte, 1, 0));
  pthread_join(thread, NULL);
  while (send(pair[0], "", 1, MSG_DONTWAIT) == 1) {
  }
  set_timeouts("send no timeout", pair[0], 0, 0);
  later.fd = pair[1];
  check(pthread_create(&thread, NULL, drain_later, &later) == 0,
        "pthread_create");
  result("untimed send waits", send(pair[0], &byte, 1, 0));
  pthread_join(thread, NULL);
  close(pair[0]);
  close(pair[1]);
}

// A listening socket's receive timeout bounds accept the same way.
static void accept_timeout(void) {
  struct sockaddr_in address = {.sin_family = AF_INET};
  int listener = socket(AF_INET, SOCK_STREAM, 0);
  address.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
  check(listener >= 0, "tcp listener socket");
  reuse_address(listener);
  check(bind(listener, (struct sockaddr *)&address, sizeof(address)) == 0 &&
            listen(listener, 1) == 0,
        "listen");
  set_timeouts("accept timeout", listener, 0, 100000);
  result("timed accept empty", accept(listener, NULL, NULL));
  close(listener);
}

// An accepted Internet socket starts with its listener's timeouts, so
// a receive on it expires; an accepted AF_UNIX socket is created fresh
// with none, so the same receive waits past the listener's timeout
// for data sent later.
static void accept_inherits(void) {
  struct sockaddr_in tcp = {.sin_family = AF_INET};
  struct sockaddr_un unix_address = abstract_unix_address("inherits");
  socklen_t tcp_length = sizeof(tcp);
  char buffer[16] = {0};
  pthread_t thread;
  struct later later;
  int listener = socket(AF_INET, SOCK_STREAM, 0);
  int client;
  int accepted;
  tcp.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
  check(listener >= 0, "tcp listener socket");
  reuse_address(listener);
  check(bind(listener, (struct sockaddr *)&tcp, sizeof(tcp)) == 0 &&
            getsockname(listener, (struct sockaddr *)&tcp, &tcp_length) ==
                0 &&
            listen(listener, 1) == 0,
        "tcp listen");
  set_timeouts("inherit tcp listener", listener, 0, 100000);
  client = socket(AF_INET, SOCK_STREAM, 0);
  check(client >= 0 &&
            connect(client, (struct sockaddr *)&tcp, tcp_length) == 0,
        "tcp connect");
  accepted = accept(listener, NULL, NULL);
  check(accepted >= 0, "tcp accept");
  result("accepted tcp recv expires",
         recv(accepted, buffer, sizeof(buffer), 0));
  close(accepted);
  close(client);
  close(listener);

  listener = socket(AF_UNIX, SOCK_STREAM, 0);
  check(listener >= 0 &&
            bind(listener, (struct sockaddr *)&unix_address,
                 sizeof(unix_address)) == 0 &&
            listen(listener, 1) == 0,
        "unix listen");
  set_timeouts("inherit unix listener", listener, 0, 100000);
  client = socket(AF_UNIX, SOCK_STREAM, 0);
  check(client >= 0 &&
            connect(client, (struct sockaddr *)&unix_address,
                    sizeof(unix_address)) == 0,
        "unix connect");
  accepted = accept(listener, NULL, NULL);
  check(accepted >= 0, "unix accept");
  later.fd = client;
  check(pthread_create(&thread, NULL, send_later_slow, &later) == 0,
        "pthread_create");
  memset(buffer, 0, sizeof(buffer));
  result("accepted unix recv waits",
         recv(accepted, buffer, sizeof(buffer), 0));
  printf("accepted unix data: %.4s\n", buffer);
  pthread_join(thread, NULL);
  close(accepted);
  close(client);
  close(listener);
}

// A blocking connect to a listener whose backlog is full waits by the
// send timeout, and the two families report its passing differently: a
// TCP connect is already in progress when it gives up waiting, so Linux
// returns EINPROGRESS and the attempt continues in the background,
// while an AF_UNIX connect that never got a slot in the listener's
// queue fails with EAGAIN. Filling each backlog with timed connects
// ends the same way the final timed connect does.
static void connect_timeout(void) {
  struct sockaddr_in tcp = {.sin_family = AF_INET};
  struct sockaddr_un unix_address = abstract_unix_address("connect");
  socklen_t tcp_length = sizeof(tcp);
  int tcp_listener = socket(AF_INET, SOCK_STREAM, 0);
  check(tcp_listener >= 0, "tcp listener socket");
  reuse_address(tcp_listener);
  int unix_listener = socket(AF_UNIX, SOCK_STREAM, 0);
  int filled[8];
  int count = 0;
  int client;
  tcp.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
  check(tcp_listener >= 0 &&
            bind(tcp_listener, (struct sockaddr *)&tcp, sizeof(tcp)) == 0 &&
            getsockname(tcp_listener, (struct sockaddr *)&tcp, &tcp_length) ==
                0 &&
            listen(tcp_listener, 1) == 0,
        "tcp listen");
  for (;;) {
    int filler = socket(AF_INET, SOCK_STREAM, 0);
    check(filler >= 0, "tcp filler socket");
    set_send_timeout_quiet(filler, 0, 100000);
    if (connect(filler, (struct sockaddr *)&tcp, tcp_length) != 0) {
      printf("tcp fill ends after %s connects: %s\n",
             count > 0 ? "some" : "no", strerror(errno));
      close(filler);
      break;
    }
    check(count < 8, "tcp fill bound");
    filled[count++] = filler;
  }
  client = socket(AF_INET, SOCK_STREAM, 0);
  check(client >= 0, "tcp client socket");
  set_timeouts("tcp connect timeout", client, 0, 100000);
  result("timed tcp connect full backlog",
         connect(client, (struct sockaddr *)&tcp, tcp_length));
  close(client);
  while (count > 0) {
    close(filled[--count]);
  }
  close(tcp_listener);

  check(unix_listener >= 0 &&
            bind(unix_listener, (struct sockaddr *)&unix_address,
                 sizeof(unix_address)) == 0 &&
            listen(unix_listener, 1) == 0,
        "unix listen");
  for (;;) {
    int filler = socket(AF_UNIX, SOCK_STREAM, 0);
    check(filler >= 0, "unix filler socket");
    set_send_timeout_quiet(filler, 0, 100000);
    if (connect(filler, (struct sockaddr *)&unix_address,
                sizeof(unix_address)) != 0) {
      printf("unix fill ends after %s connects: %s\n",
             count > 0 ? "some" : "no", strerror(errno));
      close(filler);
      break;
    }
    check(count < 8, "unix fill bound");
    filled[count++] = filler;
  }
  client = socket(AF_UNIX, SOCK_STREAM, 0);
  check(client >= 0, "unix client socket");
  set_timeouts("unix connect timeout", client, 0, 100000);
  result("timed unix connect full backlog",
         connect(client, (struct sockaddr *)&unix_address,
                 sizeof(unix_address)));
  close(client);
  while (count > 0) {
    close(filled[--count]);
  }
  close(unix_listener);
}

// A signal that ends a timed wait is EINTR even under SA_RESTART, where
// an untimed wait would restart (Linux's sock_intr_errno): a timed send
// and write to a full socket, a timed read of an empty one and a timed
// AF_UNIX connect to a full backlog, each with half a minute of timeout
// left, end at the timer signal instead.
static void arm_alarm(void) {
  struct itimerval timer = {.it_value = {.tv_sec = 0, .tv_usec = 50000}};
  check(setitimer(ITIMER_REAL, &timer, NULL) == 0, "setitimer");
}

static void timed_interrupt(void) {
  struct sigaction action = {.sa_handler = on_usr1, .sa_flags = SA_RESTART};
  struct sockaddr_un unix_address = abstract_unix_address("interrupt");
  int pair[2];
  int listener;
  int filled[8];
  int count = 0;
  int client;
  check(sigaction(SIGALRM, &action, NULL) == 0, "sigaction");

  check(socketpair(AF_UNIX, SOCK_DGRAM, 0, pair) == 0, "socketpair");
  while (send(pair[0], "", 1, MSG_DONTWAIT) == 1) {
  }
  usr1_seen = 0;
  set_timeouts("interrupt send", pair[0], 30, 0);
  arm_alarm();
  result("timed send interrupted", send(pair[0], "x", 1, 0));
  printf("interrupt send handler ran: %d\n", usr1_seen);
  usr1_seen = 0;
  arm_alarm();
  result("timed write interrupted", write(pair[0], "x", 1));
  printf("interrupt write handler ran: %d\n", usr1_seen);
  close(pair[0]);
  close(pair[1]);
  check(socketpair(AF_UNIX, SOCK_DGRAM, 0, pair) == 0, "socketpair");
  set_timeouts("interrupt read", pair[1], 30, 0);
  usr1_seen = 0;
  arm_alarm();
  result("timed read interrupted", read(pair[1], (char[4]){0}, 4));
  printf("interrupt read handler ran: %d\n", usr1_seen);
  close(pair[0]);
  close(pair[1]);

  listener = socket(AF_UNIX, SOCK_STREAM, 0);
  check(listener >= 0 &&
            bind(listener, (struct sockaddr *)&unix_address,
                 sizeof(unix_address)) == 0 &&
            listen(listener, 1) == 0,
        "unix listen");
  for (;;) {
    int filler = socket(AF_UNIX, SOCK_STREAM, 0);
    check(filler >= 0, "unix filler socket");
    set_send_timeout_quiet(filler, 0, 100000);
    if (connect(filler, (struct sockaddr *)&unix_address,
                sizeof(unix_address)) != 0) {
      close(filler);
      break;
    }
    check(count < 8, "unix fill bound");
    filled[count++] = filler;
  }
  usr1_seen = 0;
  client = socket(AF_UNIX, SOCK_STREAM, 0);
  check(client >= 0, "unix client socket");
  set_timeouts("interrupt connect", client, 30, 0);
  arm_alarm();
  result("timed connect interrupted",
         connect(client, (struct sockaddr *)&unix_address,
                 sizeof(unix_address)));
  printf("interrupt connect handler ran: %d\n", usr1_seen);
  close(client);
  while (count > 0) {
    close(filled[--count]);
  }
  close(listener);
}

// A negative second count is already expired, so the call fails with
// EAGAIN without waiting -- and without taking a pending signal first:
// one held pending by the mask stays pending across the call and is
// delivered only when the mask lifts.
static void negative_seconds_signal(void) {
  struct sigaction action = {.sa_handler = on_usr1, .sa_flags = SA_RESTART};
  sigset_t block, previous;
  int pair[2];
  char buffer[4];
  check(sigaction(SIGUSR1, &action, NULL) == 0, "sigaction");
  check(socketpair(AF_UNIX, SOCK_DGRAM, 0, pair) == 0, "socketpair");
  set_timeouts("pending signal", pair[1], -1, 0);
  sigemptyset(&block);
  sigaddset(&block, SIGUSR1);
  check(pthread_sigmask(SIG_BLOCK, &block, &previous) == 0, "block");
  usr1_seen = 0;
  check(pthread_kill(pthread_self(), SIGUSR1) == 0, "pthread_kill");
  result("expired recv with pending signal",
         recv(pair[1], buffer, sizeof(buffer), 0));
  printf("pending signal handler ran during call: %d\n", usr1_seen);
  check(pthread_sigmask(SIG_SETMASK, &previous, NULL) == 0, "unblock");
  printf("pending signal handler ran after unblock: %d\n", usr1_seen);
  close(pair[0]);
  close(pair[1]);
}

// An unusable recvmmsg timespec fails before anything is received, and
// before the descriptor itself is checked.
static void recvmmsg_invalid(void) {
  int pair[2];
  char buffer[16];
  struct iovec iov = {buffer, sizeof(buffer)};
  struct mmsghdr message = {.msg_hdr = {.msg_iov = &iov, .msg_iovlen = 1}};
  struct timespec both_negative = {.tv_sec = -1, .tv_nsec = -1};
  struct timespec sec_negative = {.tv_sec = -1, .tv_nsec = 0};
  struct timespec nsec_large = {.tv_sec = 0, .tv_nsec = 1000000000};
  check(socketpair(AF_UNIX, SOCK_DGRAM, 0, pair) == 0, "socketpair");
  result("recvmmsg timeout -1 -1",
         recvmmsg(pair[1], &message, 1, 0, &both_negative));
  result("recvmmsg timeout -1 0",
         recvmmsg(pair[1], &message, 1, 0, &sec_negative));
  result("recvmmsg timeout nsec large",
         recvmmsg(pair[1], &message, 1, 0, &nsec_large));
  result("recvmmsg bad fd timeout -1 -1",
         recvmmsg(-1, &message, 1, 0, &both_negative));
  close(pair[0]);
  close(pair[1]);
}

// Runs every section, or only those named on the command line, and makes
// no recvmmsg or sendmmsg call after the argument "unbatched".
int main(int argc, char **argv) {
  static const struct {
    const char *name;
    void (*run)(void);
  } sections[] = {{"setsockopt_errors", setsockopt_errors},
                  {"negative_seconds", negative_seconds},
                  {"receive_timeout", receive_timeout},
                  {"send_timeout", send_timeout},
                  {"accept_timeout", accept_timeout},
                  {"accept_inherits", accept_inherits},
                  {"connect_timeout", connect_timeout},
                  {"timed_interrupt", timed_interrupt},
                  {"negative_seconds_signal", negative_seconds_signal},
                  {"recvmmsg_invalid", recvmmsg_invalid}};
  int named = 0;
  setvbuf(stdout, NULL, _IOLBF, 0);
  for (int j = 1; j < argc; j++) {
    if (strcmp(argv[j], "unbatched") == 0) {
      batched = 0;
    } else {
      named++;
    }
  }
  for (size_t i = 0; i < sizeof(sections) / sizeof(sections[0]); i++) {
    int selected = !named;
    for (int j = 1; j < argc; j++) {
      selected |= strcmp(argv[j], sections[i].name) == 0;
    }
    if (selected) {
      sections[i].run();
    }
  }
  return 0;
}
