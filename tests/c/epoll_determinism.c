/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <signal.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/signalfd.h>
#include <sys/socket.h>
#include <sys/timerfd.h>
#include <unistd.h>

#define ARRAY_SIZE(values) (sizeof(values) / sizeof((values)[0]))

static void fail(const char* message) {
  fprintf(stderr, "%s\n", message);
  exit(1);
}

static void fail_errno(const char* operation) {
  fprintf(stderr, "%s failed: %s\n", operation, strerror(errno));
  exit(1);
}

static int create_epoll(void) {
  const int fd = epoll_create1(EPOLL_CLOEXEC);
  if (fd < 0) {
    fail_errno("epoll_create1");
  }
  return fd;
}

static void create_pipe(int pipefd[2]) {
  if (pipe2(pipefd, O_CLOEXEC | O_NONBLOCK) != 0) {
    fail_errno("pipe2");
  }
}

static void control_fd(
    int epoll_fd,
    int operation,
    int fd,
    uint32_t events,
    uint64_t tag) {
  struct epoll_event event = {
      .events = events,
      .data.u64 = tag,
  };
  if (epoll_ctl(epoll_fd, operation, fd, &event) != 0) {
    fail_errno("epoll_ctl");
  }
}

static void write_byte(int fd, char byte) {
  if (write(fd, &byte, sizeof(byte)) != sizeof(byte)) {
    fail_errno("write");
  }
}

static void read_exact(int fd, void* buffer, size_t length) {
  const ssize_t result = read(fd, buffer, length);
  if (result < 0) {
    fail_errno("read");
  }
  if ((size_t)result != length) {
    fail("read returned an unexpected byte count");
  }
}

static void wait_until_readable(int fd, const char* label) {
  struct pollfd poll_fd = {
      .fd = fd,
      .events = POLLIN,
  };
  const int count = poll(&poll_fd, 1, 1000);
  if (count < 0) {
    fail_errno("poll");
  }
  if (count != 1 || (poll_fd.revents & POLLIN) == 0) {
    fail(label);
  }
}

static void expect_no_events(int epoll_fd, const char* label) {
  struct epoll_event event;
  const int count = epoll_wait(epoll_fd, &event, 1, 0);
  if (count < 0) {
    fail_errno("epoll_wait");
  }
  if (count != 0) {
    fail("epoll_wait returned an event while delivery should be disabled");
  }
  printf("%s none\n", label);
}

static void expect_events(
    int epoll_fd,
    const char* label,
    const uint64_t* expected_tags,
    size_t expected_count) {
  struct epoll_event events[8];
  if (expected_count > ARRAY_SIZE(events)) {
    fail("test requested too many epoll events");
  }

  const int count = epoll_wait(epoll_fd, events, (int)expected_count, 1000);
  if (count < 0) {
    fail_errno("epoll_wait");
  }
  if ((size_t)count != expected_count) {
    fprintf(
        stderr,
        "%s returned %d events, expected %zu\n",
        label,
        count,
        expected_count);
    exit(1);
  }

  bool seen[8] = {false};
  printf("%s", label);
  for (size_t event_index = 0; event_index < expected_count; ++event_index) {
    if ((events[event_index].events & EPOLLIN) == 0) {
      fail("epoll event did not contain EPOLLIN");
    }

    bool matched = false;
    for (size_t tag_index = 0; tag_index < expected_count; ++tag_index) {
      const bool same_tag =
          events[event_index].data.u64 == expected_tags[tag_index];
      if (!seen[tag_index] && same_tag) {
        seen[tag_index] = true;
        matched = true;
        break;
      }
    }
    if (!matched) {
      fail("epoll returned an unknown or duplicate tag");
    }

    printf(
        "%s%llu:0x%x",
        event_index == 0 ? " order=" : ",",
        (unsigned long long)events[event_index].data.u64,
        events[event_index].events);
  }
  putchar('\n');
}

static void run_multi(void) {
  const int epoll_fd = create_epoll();
  int pipes[3][2];
  const uint64_t tags[] = {11, 22, 33};

  for (size_t index = 0; index < ARRAY_SIZE(pipes); ++index) {
    create_pipe(pipes[index]);
    control_fd(epoll_fd, EPOLL_CTL_ADD, pipes[index][0], EPOLLIN, tags[index]);
  }

  write_byte(pipes[2][1], 'c');
  write_byte(pipes[0][1], 'a');
  write_byte(pipes[1][1], 'b');
  expect_events(epoll_fd, "multi-ready", tags, ARRAY_SIZE(tags));

  for (size_t index = 0; index < ARRAY_SIZE(pipes); ++index) {
    char byte;
    read_exact(pipes[index][0], &byte, sizeof(byte));
    close(pipes[index][0]);
    close(pipes[index][1]);
  }
  close(epoll_fd);
}

static void run_edge(void) {
  const int epoll_fd = create_epoll();
  int pipefd[2];
  const uint64_t tag = 101;
  create_pipe(pipefd);
  control_fd(epoll_fd, EPOLL_CTL_ADD, pipefd[0], EPOLLIN | EPOLLET, tag);

  write_byte(pipefd[1], 'a');
  write_byte(pipefd[1], 'b');
  expect_events(epoll_fd, "edge-first", &tag, 1);
  expect_no_events(epoll_fd, "edge-undrained");

  char bytes[2];
  read_exact(pipefd[0], bytes, sizeof(bytes));
  write_byte(pipefd[1], 'c');
  expect_events(epoll_fd, "edge-after-drain", &tag, 1);
  read_exact(pipefd[0], bytes, 1);

  close(pipefd[0]);
  close(pipefd[1]);
  close(epoll_fd);
}

static void run_oneshot(void) {
  const int epoll_fd = create_epoll();
  int pipefd[2];
  const uint64_t tag = 102;
  create_pipe(pipefd);
  control_fd(epoll_fd, EPOLL_CTL_ADD, pipefd[0], EPOLLIN | EPOLLONESHOT, tag);

  write_byte(pipefd[1], 'a');
  expect_events(epoll_fd, "oneshot-first", &tag, 1);
  write_byte(pipefd[1], 'b');
  expect_no_events(epoll_fd, "oneshot-disabled");

  control_fd(epoll_fd, EPOLL_CTL_MOD, pipefd[0], EPOLLIN | EPOLLONESHOT, tag);
  expect_events(epoll_fd, "oneshot-rearmed", &tag, 1);
  char bytes[2];
  read_exact(pipefd[0], bytes, sizeof(bytes));

  close(pipefd[0]);
  close(pipefd[1]);
  close(epoll_fd);
}

static void run_mixed(void) {
  sigset_t signal_mask;
  sigemptyset(&signal_mask);
  sigaddset(&signal_mask, SIGUSR1);
  if (sigprocmask(SIG_BLOCK, &signal_mask, NULL) != 0) {
    fail_errno("sigprocmask");
  }

  const int signal_fd = signalfd(-1, &signal_mask, SFD_CLOEXEC | SFD_NONBLOCK);
  if (signal_fd < 0) {
    fail_errno("signalfd");
  }
  const int timer_fd =
      timerfd_create(CLOCK_MONOTONIC, TFD_CLOEXEC | TFD_NONBLOCK);
  if (timer_fd < 0) {
    fail_errno("timerfd_create");
  }
  int pipefd[2];
  create_pipe(pipefd);
  int sockets[2];
  const int socket_type = SOCK_STREAM | SOCK_CLOEXEC | SOCK_NONBLOCK;
  if (socketpair(AF_UNIX, socket_type, 0, sockets) != 0) {
    fail_errno("socketpair");
  }

  const int epoll_fd = create_epoll();
  const uint64_t tags[] = {201, 202, 203, 204};
  control_fd(epoll_fd, EPOLL_CTL_ADD, pipefd[0], EPOLLIN, tags[0]);
  control_fd(epoll_fd, EPOLL_CTL_ADD, sockets[0], EPOLLIN, tags[1]);
  control_fd(epoll_fd, EPOLL_CTL_ADD, timer_fd, EPOLLIN, tags[2]);
  control_fd(epoll_fd, EPOLL_CTL_ADD, signal_fd, EPOLLIN, tags[3]);

  write_byte(sockets[1], 's');
  if (kill(getpid(), SIGUSR1) != 0) {
    fail_errno("kill");
  }
  write_byte(pipefd[1], 'p');
  const struct itimerspec timer = {
      .it_value =
          {
              .tv_sec = 1,
              .tv_nsec = 0,
          },
  };
  if (timerfd_settime(timer_fd, TFD_TIMER_ABSTIME, &timer, NULL) != 0) {
    fail_errno("timerfd_settime");
  }

  wait_until_readable(timer_fd, "timerfd did not become readable");
  wait_until_readable(signal_fd, "signalfd did not become readable");
  expect_events(epoll_fd, "mixed-ready", tags, ARRAY_SIZE(tags));

  char byte;
  read_exact(sockets[0], &byte, sizeof(byte));
  read_exact(pipefd[0], &byte, sizeof(byte));
  uint64_t expirations;
  read_exact(timer_fd, &expirations, sizeof(expirations));
  struct signalfd_siginfo signal_info;
  read_exact(signal_fd, &signal_info, sizeof(signal_info));
  if (signal_info.ssi_signo != SIGUSR1) {
    fail("signalfd returned the wrong signal");
  }

  close(sockets[0]);
  close(sockets[1]);
  close(pipefd[0]);
  close(pipefd[1]);
  close(timer_fd);
  close(signal_fd);
  close(epoll_fd);
}

static void run_nested(void) {
  const int inner_epoll = create_epoll();
  const int outer_epoll = create_epoll();
  int pipefd[2];
  create_pipe(pipefd);

  const uint64_t pipe_tag = 301;
  const uint64_t nested_tag = 302;
  control_fd(inner_epoll, EPOLL_CTL_ADD, pipefd[0], EPOLLIN, pipe_tag);
  control_fd(outer_epoll, EPOLL_CTL_ADD, inner_epoll, EPOLLIN, nested_tag);

  write_byte(pipefd[1], 'n');
  expect_events(outer_epoll, "nested-outer", &nested_tag, 1);
  expect_events(inner_epoll, "nested-inner", &pipe_tag, 1);

  char byte;
  read_exact(pipefd[0], &byte, sizeof(byte));
  close(pipefd[0]);
  close(pipefd[1]);
  close(outer_epoll);
  close(inner_epoll);
}

// Regression test for an epoll fd that was never registered in Detcore's
// descriptor table. Descriptor-table operations (F_GETFL, F_SETFD, dup,
// F_DUPFD, F_DUPFD_CLOEXEC) used to fail with EBADF under Hermit even though
// the underlying kernel fd was perfectly valid. This broke the rustup proxies
// (cargo/rustc), whose tokio runtime dups its epoll fd at startup.
static void run_dupfd(void) {
  const int epoll_fd = create_epoll();

  // Status flags must be readable from the descriptor table.
  if (fcntl(epoll_fd, F_GETFL) < 0) {
    fail_errno("fcntl(F_GETFL) on epoll fd");
  }

  // Descriptor flags must be settable.
  if (fcntl(epoll_fd, F_SETFD, FD_CLOEXEC) < 0) {
    fail_errno("fcntl(F_SETFD) on epoll fd");
  }

  // Plain dup of the epoll fd.
  const int dup_fd = dup(epoll_fd);
  if (dup_fd < 0) {
    fail_errno("dup(epoll fd)");
  }

  // F_DUPFD returns the lowest free fd >= the requested minimum.
  const int dupfd_fd = fcntl(epoll_fd, F_DUPFD, 3);
  if (dupfd_fd < 0) {
    fail_errno("fcntl(F_DUPFD) on epoll fd");
  }

  // F_DUPFD_CLOEXEC does the same but sets close-on-exec on the new fd; this
  // is the exact call rustup's tokio runtime makes.
  const int cloexec_fd = fcntl(epoll_fd, F_DUPFD_CLOEXEC, 3);
  if (cloexec_fd < 0) {
    fail_errno("fcntl(F_DUPFD_CLOEXEC) on epoll fd");
  }
  const int cloexec_flags = fcntl(cloexec_fd, F_GETFD);
  if (cloexec_flags < 0) {
    fail_errno("fcntl(F_GETFD) on duplicated epoll fd");
  }
  if ((cloexec_flags & FD_CLOEXEC) == 0) {
    fail("F_DUPFD_CLOEXEC did not set close-on-exec on the new fd");
  }

  close(cloexec_fd);
  close(dupfd_fd);
  close(dup_fd);
  close(epoll_fd);
  printf("dupfd ops-ok\n");
}

int main(int argc, char** argv) {
  if (argc != 2) {
    fprintf(
        stderr,
        "usage: %s <multi|edge|oneshot|mixed|nested|dupfd>\n",
        argv[0]);
    return 2;
  }

  if (strcmp(argv[1], "multi") == 0) {
    run_multi();
  } else if (strcmp(argv[1], "edge") == 0) {
    run_edge();
  } else if (strcmp(argv[1], "oneshot") == 0) {
    run_oneshot();
  } else if (strcmp(argv[1], "mixed") == 0) {
    run_mixed();
  } else if (strcmp(argv[1], "nested") == 0) {
    run_nested();
  } else if (strcmp(argv[1], "dupfd") == 0) {
    run_dupfd();
  } else {
    fprintf(stderr, "unknown scenario: %s\n", argv[1]);
    return 2;
  }

  printf("%s success\n", argv[1]);
  return 0;
}
