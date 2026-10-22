/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <net/if.h>
#include <sched.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/socket.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

enum { TEST_PORT = 32768, SETUP_FAILURE = 70 };

static void setup_error(const char* operation) {
  perror(operation);
  exit(SETUP_FAILURE);
}

static void require(int condition, const char* message) {
  if (!condition) {
    fprintf(stderr, "%s\n", message);
    exit(SETUP_FAILURE);
  }
}

static void namespace_identity(char* identity, size_t capacity) {
  ssize_t size = readlink("/proc/thread-self/ns/net", identity, capacity - 1);
  if (size < 0) setup_error("readlink native network namespace");
  require((size_t)size < capacity - 1, "native namespace link was truncated");
  identity[size] = '\0';
}

static void write_map(const char* path, const char* value) {
  int fd = open(path, O_WRONLY | O_CLOEXEC);
  if (fd < 0) setup_error(path);
  size_t length = strlen(value);
  if (write(fd, value, length) != (ssize_t)length) setup_error("write user map");
  if (close(fd) < 0) setup_error("close user map");
}

static void private_outer_network(char* identity, size_t capacity) {
  char original[128];
  namespace_identity(original, sizeof(original));
  uid_t uid = geteuid();
  gid_t gid = getegid();
  if (unshare(CLONE_NEWUSER) < 0) setup_error("unshare outer user namespace");
  char mapping[64];
  snprintf(mapping, sizeof(mapping), "0 %lu 1\n", (unsigned long)uid);
  write_map("/proc/self/uid_map", mapping);
  write_map("/proc/self/setgroups", "deny\n");
  snprintf(mapping, sizeof(mapping), "0 %lu 1\n", (unsigned long)gid);
  write_map("/proc/self/gid_map", mapping);
  if (unshare(CLONE_NEWNET) < 0) setup_error("unshare outer network namespace");
  namespace_identity(identity, capacity);
  require(strcmp(identity, original) != 0, "outer network namespace was inherited");

  int control = socket(AF_INET, SOCK_DGRAM | SOCK_CLOEXEC, 0);
  if (control < 0) setup_error("socket loopback control");
  struct ifreq loopback = {0};
  strcpy(loopback.ifr_name, "lo");
  if (ioctl(control, SIOCGIFFLAGS, &loopback) < 0) setup_error("get loopback flags");
  loopback.ifr_flags |= IFF_UP;
  if (ioctl(control, SIOCSIFFLAGS, &loopback) < 0) setup_error("bring loopback up");
  if (close(control) < 0) setup_error("close loopback control");

  FILE* range = fopen("/proc/sys/net/ipv4/ip_local_port_range", "r");
  if (!range) setup_error("read outer ephemeral range");
  unsigned first = 0, last = 0;
  require(fscanf(range, "%u %u", &first, &last) == 2, "invalid outer ephemeral range");
  require(first == TEST_PORT && last > first && last <= 65535,
          "outer ephemeral range does not start at the fixture's required port 32768");
  if (fclose(range) != 0) setup_error("close ephemeral range");
  fprintf(stderr, "native outer namespace %s -> %s; ephemeral range %u %u\n",
          original, identity, first, last);
}

static struct sockaddr_in loopback_address(unsigned port) {
  struct sockaddr_in address = {0};
  address.sin_family = AF_INET;
  address.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
  address.sin_port = htons((uint16_t)port);
  return address;
}

static void check_holder(int holder, const char* namespace) {
  char current[128];
  namespace_identity(current, sizeof(current));
  require(strcmp(current, namespace) == 0, "outer helper changed network namespace");
  struct sockaddr_in address = {0};
  socklen_t length = sizeof(address);
  if (getsockname(holder, (struct sockaddr*)&address, &length) < 0)
    setup_error("getsockname outer holder");
  require(length == sizeof(address) && address.sin_family == AF_INET &&
              address.sin_addr.s_addr == htonl(INADDR_LOOPBACK) &&
              ntohs(address.sin_port) == TEST_PORT,
          "outer holder changed its IPv4 loopback address");
  const int options[] = {SO_REUSEADDR, SO_REUSEPORT, SO_ACCEPTCONN};
  const int expected[] = {0, 0, 1};
  for (size_t i = 0; i < sizeof(options) / sizeof(options[0]); ++i) {
    int value = -1;
    length = sizeof(value);
    if (getsockopt(holder, SOL_SOCKET, options[i], &value, &length) < 0)
      setup_error("getsockopt outer holder");
    require(length == sizeof(value) && value == expected[i],
            "outer holder lost its exclusive listening options");
  }
  int probe = socket(AF_INET, SOCK_STREAM | SOCK_CLOEXEC, 0);
  if (probe < 0) setup_error("socket outer collision probe");
  int result = bind(probe, (struct sockaddr*)&address, sizeof(address));
  int error = errno;
  if (close(probe) < 0) setup_error("close outer collision probe");
  require(result == -1 && error == EADDRINUSE,
          "native second bind did not encounter the exclusive outer holder");
}

static double monotonic_seconds(void) {
  struct timespec now;
  if (clock_gettime(CLOCK_MONOTONIC, &now) < 0) setup_error("native deadline clock");
  return (double)now.tv_sec + (double)now.tv_nsec / 1000000000.0;
}

static int outer(int argc, char** argv) {
  require(argc >= 3, "outer mode requires an exact executable and its argv");
  char namespace[128];
  private_outer_network(namespace, sizeof(namespace));
  int holder = socket(AF_INET, SOCK_STREAM | SOCK_CLOEXEC, 0);
  if (holder < 0) setup_error("socket outer holder");
  struct sockaddr_in address = loopback_address(TEST_PORT);
  if (bind(holder, (struct sockaddr*)&address, sizeof(address)) < 0)
    setup_error("bind exclusive outer holder");
  if (listen(holder, 1) < 0) setup_error("listen outer holder");
  check_holder(holder, namespace);
  fprintf(stderr, "native exclusive outer holder ready at 127.0.0.1:%d\n", TEST_PORT);
  fflush(NULL);

  pid_t child = fork();
  if (child < 0) setup_error("fork exact CLI");
  if (child == 0) {
    if (setpgid(0, 0) < 0) setup_error("set CLI process group");
    execv(argv[2], argv + 2);
    setup_error("exec exact CLI");
  }
  double deadline = monotonic_seconds() + 45.0;
  int status = 0;
  for (;;) {
    pid_t waited = waitpid(child, &status, WNOHANG);
    if (waited == child) break;
    if (waited < 0 && errno != EINTR) setup_error("wait exact CLI");
    if (monotonic_seconds() >= deadline) {
      kill(-child, SIGKILL);
      kill(child, SIGKILL);
      while (waitpid(child, &status, 0) < 0 && errno == EINTR) {}
      check_holder(holder, namespace);
      fputs("native outer CLI deadline exceeded\n", stderr);
      close(holder);
      return SETUP_FAILURE;
    }
    struct timespec pause = {.tv_sec = 0, .tv_nsec = 10000000};
    while (nanosleep(&pause, &pause) < 0 && errno == EINTR) {}
  }
  check_holder(holder, namespace);
  if (close(holder) < 0) setup_error("close outer holder");
  fputs("native exclusive outer holder verified after CLI exit\n", stderr);
  if (WIFEXITED(status)) return WEXITSTATUS(status);
  if (WIFSIGNALED(status)) return 128 + WTERMSIG(status);
  return SETUP_FAILURE;
}

static int enable_reuse(int fd) {
  int enabled = 1;
  if (setsockopt(fd, SOL_SOCKET, SO_REUSEADDR, &enabled, sizeof(enabled)) < 0 ||
      setsockopt(fd, SOL_SOCKET, SO_REUSEPORT, &enabled, sizeof(enabled)) < 0) {
    perror("guest setsockopt reuse");
    return -1;
  }
  return 0;
}

static int guest(int host_control) {
  int first = socket(AF_INET, SOCK_STREAM, 0);
  int second = socket(AF_INET, SOCK_STREAM, 0);
  if (first < 0 || second < 0) { perror("guest socket"); return 1; }
  if (enable_reuse(first) < 0 || enable_reuse(second) < 0) return 2;
  struct sockaddr_in address = loopback_address(0);
  int result = bind(first, (struct sockaddr*)&address, sizeof(address));
  if (host_control) {
    if (result != -1 || errno != EADDRINUSE) {
      fputs("explicit host networking did not preserve the outer port collision\n", stderr);
      return 7;
    }
    if (close(second) < 0 || close(first) < 0) return 8;
    puts("dbt-host-collision-observed");
    return 0;
  }
  if (result < 0) { perror("bind(first)"); return 3; }
  socklen_t length = sizeof(address);
  if (getsockname(first, (struct sockaddr*)&address, &length) < 0) {
    perror("guest getsockname"); return 4;
  }
  if (length != sizeof(address) || address.sin_family != AF_INET ||
      address.sin_addr.s_addr != htonl(INADDR_LOOPBACK) ||
      ntohs(address.sin_port) != TEST_PORT) {
    fputs("guest port-zero bind did not preserve exact loopback port 32768\n", stderr);
    return 5;
  }
  if (bind(second, (struct sockaddr*)&address, sizeof(address)) < 0) {
    perror("bind(second)"); return 5;
  }
  if (listen(first, 1) < 0 || listen(second, 1) < 0) {
    perror("guest listen"); return 6;
  }
  const int sockets[] = {first, second};
  for (size_t i = 0; i < sizeof(sockets) / sizeof(sockets[0]); ++i) {
    int listening = 0;
    length = sizeof(listening);
    if (getsockopt(sockets[i], SOL_SOCKET, SO_ACCEPTCONN, &listening, &length) < 0 ||
        length != sizeof(listening) || listening != 1) {
      fputs("guest socket did not retain listening semantics\n", stderr);
      return 6;
    }
  }
  if (close(second) < 0 || close(first) < 0) return 8;
  puts("dbt-local-port-zero-listeners-ok");
  return 0;
}

int main(int argc, char** argv) {
  if (argc >= 2 && strcmp(argv[1], "outer") == 0) return outer(argc, argv);
  if (argc == 2 && strcmp(argv[1], "guest-local") == 0) return guest(0);
  if (argc == 2 && strcmp(argv[1], "guest-host") == 0) return guest(1);
  fputs("expected outer, guest-local or guest-host mode\n", stderr);
  return 64;
}
