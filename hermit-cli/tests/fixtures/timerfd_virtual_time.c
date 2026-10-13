/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * timerfd on Detcore's virtual clock: one mode per case, each with a
 * must-wake check (the timer becomes readable, with the right count) and a
 * must-not check (it is not readable before its deadline, or a call times out
 * instead of waking). Every line is printed only after the check it names
 * passed; a failed check prints FAIL and exits 1. Counts and remaining times
 * are printed because under Hermit they are a function of virtual time and
 * must repeat exactly.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <poll.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/select.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <sys/timerfd.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define MS 1000000L

static void fail(const char* what) {
  printf("FAIL %s (errno %d)\n", what, errno);
  fflush(stdout);
  exit(1);
}

static void check(int ok, const char* what) {
  if (!ok)
    fail(what);
}

static int make_timer(int clock, int flags, long value_ns, long interval_ns) {
  int fd = timerfd_create(clock, flags);
  check(fd >= 0, "timerfd_create");
  struct itimerspec its = {
      .it_value = {value_ns / 1000000000L, value_ns % 1000000000L},
      .it_interval = {interval_ns / 1000000000L, interval_ns % 1000000000L},
  };
  check(timerfd_settime(fd, 0, &its, NULL) == 0, "timerfd_settime");
  return fd;
}

static void sleep_ns(long ns) {
  struct timespec t = {ns / 1000000000L, ns % 1000000000L};
  while (nanosleep(&t, &t) != 0 && errno == EINTR) {
  }
}

static int readable_now(int fd) {
  struct pollfd p = {.fd = fd, .events = POLLIN};
  int n = poll(&p, 1, 0);
  check(n >= 0, "poll probe");
  return n == 1 && (p.revents & POLLIN);
}

static uint64_t read_count(int fd) {
  uint64_t n = 0;
  check(read(fd, &n, sizeof n) == (ssize_t)sizeof n, "read count");
  return n;
}

/* Periodic catch-up: a 10 ms periodic timer read late counts every period it
 * missed; timerfd_gettime then reports the time to the next one. */
static void mode_catchup(void) {
  int fd = make_timer(CLOCK_MONOTONIC, 0, 10 * MS, 10 * MS);
  check(!readable_now(fd), "must-not: periodic timer readable before 10 ms");
  sleep_ns(35 * MS);
  uint64_t n = read_count(fd);
  check(n >= 3, "must-wake: at least three periods after 35 ms");
  struct itimerspec cur;
  check(timerfd_gettime(fd, &cur) == 0, "timerfd_gettime");
  check(cur.it_interval.tv_nsec == 10 * MS, "interval kept");
  check(cur.it_value.tv_nsec > 0 && cur.it_value.tv_nsec <= 10 * MS, "remaining within a period");
  printf("catchup count=%llu remaining_ns=%ld\n", (unsigned long long)n, cur.it_value.tv_nsec);
  /* Disarmed with a zero value, the interval is still reported. */
  struct itimerspec disarm = {.it_interval = {0, 10 * MS}};
  check(timerfd_settime(fd, 0, &disarm, NULL) == 0, "disarm");
  check(timerfd_gettime(fd, &cur) == 0 && cur.it_value.tv_nsec == 0 &&
            cur.it_value.tv_sec == 0 && cur.it_interval.tv_nsec == 10 * MS,
        "disarmed timer keeps its interval");
  sleep_ns(25 * MS);
  check(!readable_now(fd), "must-not: disarmed timer readable");
  printf("catchup disarmed ok\n");
}

/* TFD_TIMER_ABSTIME against each clock: not readable before the deadline,
 * readable after; a deadline already past is readable at once. */
static void mode_abstime(void) {
  const int clocks[] = {CLOCK_REALTIME, CLOCK_MONOTONIC, CLOCK_BOOTTIME};
  const char* names[] = {"realtime", "monotonic", "boottime"};
  for (int i = 0; i < 3; i++) {
    int fd = timerfd_create(clocks[i], 0);
    check(fd >= 0, "timerfd_create abstime");
    struct timespec now;
    check(clock_gettime(clocks[i], &now) == 0, "clock_gettime");
    struct itimerspec its = {.it_value = now};
    its.it_value.tv_nsec += 20 * MS;
    if (its.it_value.tv_nsec >= 1000000000L) {
      its.it_value.tv_sec += 1;
      its.it_value.tv_nsec -= 1000000000L;
    }
    check(timerfd_settime(fd, TFD_TIMER_ABSTIME, &its, NULL) == 0, "settime abstime");
    struct pollfd p = {.fd = fd, .events = POLLIN};
    check(poll(&p, 1, 5) == 0, "must-not: abstime readable 5 ms before its deadline");
    check(poll(&p, 1, 100) == 1, "must-wake: abstime readable by its deadline");
    check(read_count(fd) == 1, "abstime count");
    /* On CLOCK_REALTIME also ask for TFD_TIMER_CANCEL_ON_SET, which a clock
     * that is never set must never fire. */
    struct itimerspec past = {.it_value = {1, 0}};
    int past_flags = TFD_TIMER_ABSTIME;
    if (clocks[i] == CLOCK_REALTIME)
      past_flags |= TFD_TIMER_CANCEL_ON_SET;
    check(timerfd_settime(fd, past_flags, &past, NULL) == 0, "settime past deadline");
    check(poll(&p, 1, 10) == 1, "must-wake: a past deadline is readable at once");
    check(read_count(fd) == 1, "past deadline count");
    printf("abstime %s ok\n", names[i]);
    close(fd);
  }
}

/* TFD_NONBLOCK: EAGAIN before the deadline; a short buffer is EINVAL. */
static void mode_nonblock(void) {
  int fd = make_timer(CLOCK_MONOTONIC, TFD_NONBLOCK, 10 * MS, 0);
  uint64_t n;
  check(read(fd, &n, sizeof n) == -1 && errno == EAGAIN, "must-not: EAGAIN before deadline");
  uint32_t small;
  check(read(fd, &small, sizeof small) == -1 && errno == EINVAL, "short buffer EINVAL");
  sleep_ns(15 * MS);
  check(read(fd, &n, sizeof n) == (ssize_t)sizeof n && n == 1, "must-wake: count 1 after deadline");
  check(read(fd, &n, sizeof n) == -1 && errno == EAGAIN, "must-not: EAGAIN after the read");
  printf("nonblock ok\n");
}

/* epoll_pwait2 with a timespec timeout: times out before the deadline; a
 * NULL timeout waits until it. */
static void mode_epoll_pwait2(void) {
  int fd = make_timer(CLOCK_MONOTONIC, 0, 20 * MS, 0);
  int ep = epoll_create1(0);
  struct epoll_event want = {.events = EPOLLIN, .data.fd = fd};
  check(epoll_ctl(ep, EPOLL_CTL_ADD, fd, &want) == 0, "epoll_ctl");
  struct epoll_event got;
  struct timespec short_wait = {0, 5 * MS};
  long n = syscall(SYS_epoll_pwait2, ep, &got, 1, &short_wait, NULL, 8);
  check(n == 0, "must-not: epoll_pwait2 times out before the deadline");
  n = syscall(SYS_epoll_pwait2, ep, &got, 1, NULL, NULL, 8);
  check(n == 1 && got.data.fd == fd, "must-wake: epoll_pwait2 NULL timeout wakes");
  check(read_count(fd) == 1, "epoll_pwait2 count");
  printf("epoll_pwait2 ok\n");
}

/* Zero-timeout ppoll, select and pselect6: not ready before the deadline,
 * ready after. */
static void mode_zero_timeout(void) {
  int fd = make_timer(CLOCK_MONOTONIC, 0, 10 * MS, 0);
  struct pollfd p = {.fd = fd, .events = POLLIN};
  struct timespec zero = {0, 0};
  struct timeval tvzero = {0, 0};
  fd_set set;
  check(ppoll(&p, 1, &zero, NULL) == 0, "must-not: ppoll ready early");
  FD_ZERO(&set);
  FD_SET(fd, &set);
  check(select(fd + 1, &set, NULL, NULL, &tvzero) == 0, "must-not: select ready early");
  FD_ZERO(&set);
  FD_SET(fd, &set);
  check(pselect(fd + 1, &set, NULL, NULL, &zero, NULL) == 0, "must-not: pselect6 ready early");
  sleep_ns(15 * MS);
  check(ppoll(&p, 1, &zero, NULL) == 1, "must-wake: ppoll");
  FD_ZERO(&set);
  FD_SET(fd, &set);
  tvzero.tv_sec = 0;
  tvzero.tv_usec = 0;
  check(select(fd + 1, &set, NULL, NULL, &tvzero) == 1, "must-wake: select");
  FD_ZERO(&set);
  FD_SET(fd, &set);
  check(pselect(fd + 1, &set, NULL, NULL, &zero, NULL) == 1, "must-wake: pselect6");
  check(read_count(fd) == 1, "zero-timeout count");
  printf("zero-timeout ok\n");
}

/* Waits with a nonzero or no timeout: each times out before the deadline and
 * wakes at it. */
static void mode_waits(void) {
  struct timespec early = {0, 5 * MS};
  struct timeval tvearly = {0, 5000};
  fd_set set;

  int fd = make_timer(CLOCK_MONOTONIC, 0, 20 * MS, 0);
  struct pollfd p = {.fd = fd, .events = POLLIN};
  check(poll(&p, 1, 5) == 0, "must-not: poll times out early");
  check(poll(&p, 1, -1) == 1, "must-wake: poll");
  check(read_count(fd) == 1, "poll count");
  printf("waits poll ok\n");

  fd = make_timer(CLOCK_MONOTONIC, 0, 20 * MS, 0);
  p.fd = fd;
  check(ppoll(&p, 1, &early, NULL) == 0, "must-not: ppoll times out early");
  check(ppoll(&p, 1, NULL, NULL) == 1, "must-wake: ppoll");
  check(read_count(fd) == 1, "ppoll count");
  printf("waits ppoll ok\n");

  fd = make_timer(CLOCK_MONOTONIC, 0, 20 * MS, 0);
  FD_ZERO(&set);
  FD_SET(fd, &set);
  check(select(fd + 1, &set, NULL, NULL, &tvearly) == 0, "must-not: select times out early");
  FD_ZERO(&set);
  FD_SET(fd, &set);
  check(select(fd + 1, &set, NULL, NULL, NULL) == 1, "must-wake: select");
  check(read_count(fd) == 1, "select count");
  printf("waits select ok\n");

  fd = make_timer(CLOCK_MONOTONIC, 0, 20 * MS, 0);
  FD_ZERO(&set);
  FD_SET(fd, &set);
  check(pselect(fd + 1, &set, NULL, NULL, &early, NULL) == 0, "must-not: pselect6 times out early");
  FD_ZERO(&set);
  FD_SET(fd, &set);
  check(pselect(fd + 1, &set, NULL, NULL, NULL, NULL) == 1, "must-wake: pselect6");
  check(read_count(fd) == 1, "pselect6 count");
  printf("waits pselect6 ok\n");

  fd = make_timer(CLOCK_MONOTONIC, 0, 20 * MS, 0);
  int ep = epoll_create1(0);
  struct epoll_event want = {.events = EPOLLIN, .data.fd = fd};
  check(epoll_ctl(ep, EPOLL_CTL_ADD, fd, &want) == 0, "epoll_ctl");
  struct epoll_event got;
  check(epoll_wait(ep, &got, 1, 5) == 0, "must-not: epoll_wait times out early");
  check(epoll_wait(ep, &got, 1, -1) == 1, "must-wake: epoll_wait");
  check(read_count(fd) == 1, "epoll_wait count");
  printf("waits epoll_wait ok\n");
}

/* epoll_pwait, ppoll and pselect6 with a temporary signal mask. */
static void mode_masked_waits(void) {
  sigset_t mask;
  sigemptyset(&mask);
  sigaddset(&mask, SIGUSR1);
  struct timespec early = {0, 5 * MS};
  struct timespec plenty = {1, 0};

  int fd = make_timer(CLOCK_MONOTONIC, 0, 20 * MS, 0);
  int ep = epoll_create1(0);
  struct epoll_event want = {.events = EPOLLIN, .data.fd = fd};
  check(epoll_ctl(ep, EPOLL_CTL_ADD, fd, &want) == 0, "epoll_ctl");
  struct epoll_event got;
  check(epoll_pwait(ep, &got, 1, 5, &mask) == 0, "must-not: masked epoll_pwait times out early");
  check(epoll_pwait(ep, &got, 1, 1000, &mask) == 1, "must-wake: masked epoll_pwait wakes");
  check(read_count(fd) == 1, "masked epoll count");
  printf("masked-waits epoll_pwait ok\n");

  fd = make_timer(CLOCK_MONOTONIC, 0, 20 * MS, 0);
  struct pollfd p = {.fd = fd, .events = POLLIN};
  check(ppoll(&p, 1, &early, &mask) == 0, "must-not: masked ppoll times out early");
  check(ppoll(&p, 1, &plenty, &mask) == 1, "must-wake: masked ppoll");
  check(read_count(fd) == 1, "masked ppoll count");
  printf("masked-waits ppoll ok\n");

  fd = make_timer(CLOCK_MONOTONIC, 0, 20 * MS, 0);
  fd_set set;
  FD_ZERO(&set);
  FD_SET(fd, &set);
  check(pselect(fd + 1, &set, NULL, NULL, &early, &mask) == 0,
        "must-not: masked pselect6 times out early");
  FD_ZERO(&set);
  FD_SET(fd, &set);
  check(pselect(fd + 1, &set, NULL, NULL, &plenty, &mask) == 1, "must-wake: masked pselect6");
  check(read_count(fd) == 1, "masked pselect6 count");
  printf("masked-waits pselect6 ok\n");
}

/* A masked ppoll on a timerfd that has already expired, not yet probed by
 * anything, wakes at once; before the deadline a zero-timeout probe does not.
 * (The probe carries no mask: on KVM a masked zero-timeout ppoll fails with
 * ENOSYS whatever its descriptors.) */
static void mode_masked_ppoll_expired(void) {
  sigset_t mask;
  sigemptyset(&mask);
  sigaddset(&mask, SIGUSR1);
  int fd = make_timer(CLOCK_MONOTONIC, 0, 10 * MS, 0);
  struct pollfd p = {.fd = fd, .events = POLLIN};
  struct timespec zero = {0, 0};
  struct timespec plenty = {1, 0};
  check(ppoll(&p, 1, &zero, NULL) == 0, "must-not: ppoll ready before the deadline");
  sleep_ns(15 * MS);
  check(ppoll(&p, 1, &plenty, &mask) == 1, "must-wake: masked ppoll on an expired timer");
  check(read_count(fd) == 1, "masked ppoll expired count");
  printf("masked-ppoll-expired ok\n");
}

/* Passing a timerfd, or an epoll descriptor while holding one, to another
 * process over SCM_RIGHTS: under Hermit the run is refused by name before the
 * message is sent, because the receiver would keep no model of the timer. */
static void send_descriptor(int sock, int fd) {
  char byte = 'x';
  struct iovec iov = {.iov_base = &byte, .iov_len = 1};
  union {
    char buf[CMSG_SPACE(sizeof(int))];
    struct cmsghdr align;
  } control;
  memset(&control, 0, sizeof control);
  struct msghdr msg = {.msg_iov = &iov,
                       .msg_iovlen = 1,
                       .msg_control = control.buf,
                       .msg_controllen = sizeof control.buf};
  struct cmsghdr* cmsg = CMSG_FIRSTHDR(&msg);
  cmsg->cmsg_level = SOL_SOCKET;
  cmsg->cmsg_type = SCM_RIGHTS;
  cmsg->cmsg_len = CMSG_LEN(sizeof(int));
  memcpy(CMSG_DATA(cmsg), &fd, sizeof fd);
  check(sendmsg(sock, &msg, 0) == 1, "sendmsg SCM_RIGHTS");
}

static void mode_scm_rights(int epoll) {
  int pair[2];
  check(socketpair(AF_UNIX, SOCK_STREAM, 0, pair) == 0, "socketpair");
  int fd = make_timer(CLOCK_MONOTONIC, 0, 10 * MS, 0);
  int sent = fd;
  if (epoll) {
    sent = epoll_create1(0);
    struct epoll_event want = {.events = EPOLLIN, .data.fd = fd};
    check(epoll_ctl(sent, EPOLL_CTL_ADD, fd, &want) == 0, "epoll_ctl");
  }
  send_descriptor(pair[0], sent);
  printf("scm-rights sent\n");
}

/* A timerfd sent before it is armed: the sender would model it once armed,
 * the receiver never would, so it is refused like an armed one. */
static void mode_scm_rights_unarmed(void) {
  int pair[2];
  check(socketpair(AF_UNIX, SOCK_STREAM, 0, pair) == 0, "socketpair");
  int fd = timerfd_create(CLOCK_MONOTONIC, 0);
  check(fd >= 0, "timerfd_create");
  send_descriptor(pair[0], fd);
  printf("scm-rights-unarmed sent\n");
}

/* catchup with a caught SIGCONT handler, which a host signal sent during
 * Detcore's work reaches. */
static volatile sig_atomic_t sigconts;
static void on_sigcont(int s) {
  (void)s;
  sigconts++;
}

static void mode_catchup_sigcont(void) {
  struct sigaction sa;
  memset(&sa, 0, sizeof sa);
  sa.sa_handler = on_sigcont;
  sa.sa_flags = SA_RESTART;
  check(sigaction(SIGCONT, &sa, NULL) == 0, "sigaction SIGCONT");
  mode_catchup();
  printf("sigcont caught=%d\n", (int)sigconts);
}

/* Several timerfds in one edge-triggered epoll set wake in deadline order. */
static void mode_epollet(void) {
  const long values[] = {15 * MS, 5 * MS, 10 * MS};
  int fds[3];
  int ep = epoll_create1(0);
  for (int i = 0; i < 3; i++) {
    fds[i] = make_timer(CLOCK_MONOTONIC, TFD_NONBLOCK, values[i], 0);
    struct epoll_event want = {.events = EPOLLIN | EPOLLET, .data.u32 = (uint32_t)i};
    check(epoll_ctl(ep, EPOLL_CTL_ADD, fds[i], &want) == 0, "epoll_ctl et");
  }
  printf("epollet order");
  for (int k = 0; k < 3; k++) {
    struct epoll_event got;
    check(epoll_wait(ep, &got, 1, 1000) == 1, "must-wake: edge-triggered timerfd");
    check(read_count(fds[got.data.u32]) == 1, "et count");
    printf(" %u", got.data.u32);
  }
  printf("\n");
  struct epoll_event got;
  check(epoll_wait(ep, &got, 1, 5) == 0, "must-not: no edge after every read");
  printf("epollet ok\n");
}

/* dup and fork share one timer: whoever reads consumes the expirations. */
static void mode_dup_fork(void) {
  int fd = make_timer(CLOCK_MONOTONIC, 0, 10 * MS, 10 * MS);
  int copy = dup(fd);
  check(copy >= 0, "dup");
  pid_t child = fork();
  check(child >= 0, "fork");
  if (child == 0) {
    uint64_t n = read_count(copy);
    printf("dup-fork child count=%llu\n", (unsigned long long)n);
    fflush(stdout);
    _exit(n >= 1 ? 0 : 1);
  }
  int status;
  check(waitpid(child, &status, 0) == child && WIFEXITED(status) && WEXITSTATUS(status) == 0,
        "child read");
  uint64_t n = read_count(fd);
  check(n >= 1, "must-wake: parent reads the next period");
  struct itimerspec cur;
  check(timerfd_gettime(copy, &cur) == 0 && cur.it_interval.tv_nsec == 10 * MS,
        "dup shares the setting");
  printf("dup-fork parent count=%llu\n", (unsigned long long)n);
  struct itimerspec off = {{0, 0}, {0, 0}};
  check(timerfd_settime(copy, 0, &off, NULL) == 0, "disarm through the dup");
  sleep_ns(25 * MS);
  check(!readable_now(fd), "must-not: disarmed through the dup");
  printf("dup-fork ok\n");
}

/* exec keeps the timer: the new image reads it. */
static void mode_exec(const char* self) {
  int fd = make_timer(CLOCK_MONOTONIC, 0, 20 * MS, 0);
  char fdtext[16];
  snprintf(fdtext, sizeof fdtext, "%d", fd);
  execl(self, self, "exec-child", fdtext, (char*)NULL);
  fail("execl");
}

static void mode_exec_child(const char* fdtext) {
  int fd = atoi(fdtext);
  struct itimerspec cur;
  check(timerfd_gettime(fd, &cur) == 0, "gettime after exec");
  check(cur.it_value.tv_nsec > 0, "must-not: not yet expired right after exec");
  check(!readable_now(fd), "must-not: readable right after exec");
  check(read_count(fd) == 1, "must-wake: read after exec");
  printf("exec ok\n");
}

/* readv returns the count scattered over its iovecs, as read_iter does. */
static void mode_readv(void) {
  int fd = make_timer(CLOCK_MONOTONIC, 0, 10 * MS, 10 * MS);
  check(!readable_now(fd), "must-not: readable before readv");
  uint64_t n = 0;
  struct iovec whole = {.iov_base = &n, .iov_len = sizeof n};
  check(readv(fd, &whole, 1) == (ssize_t)sizeof n && n == 1, "must-wake: readv count");
  check(!readable_now(fd), "must-not: readable after readv consumed it");
  sleep_ns(25 * MS);
  unsigned char halves[2][4];
  struct iovec part[2] = {{.iov_base = halves[0], .iov_len = 4},
                          {.iov_base = halves[1], .iov_len = 4}};
  check(readv(fd, part, 2) == (ssize_t)sizeof n, "must-wake: split readv");
  memcpy(&n, halves, sizeof n);
  check(n >= 2, "split readv count");
  printf("readv ok split=%llu\n", (unsigned long long)n);
}

/* Children exit while the parent polls a fast periodic timer, so host
 * SIGCHLD stops land during Detcore's work around the probes: every
 * expiration must still be counted, and no call fails. */
static void mode_sigchld_storm(void) {
  int fd = make_timer(CLOCK_MONOTONIC, 0, 1 * MS, 1 * MS);
  uint64_t total = 0;
  for (int i = 0; i < 40; i++) {
    pid_t child = fork();
    check(child >= 0, "fork storm");
    if (child == 0)
      _exit(0);
    struct pollfd p = {.fd = fd, .events = POLLIN};
    check(poll(&p, 1, 1000) == 1, "must-wake: poll during the storm");
    total += read_count(fd);
    check(waitpid(child, NULL, 0) == child, "reap");
  }
  check(total >= 40, "every expiration counted");
  printf("sigchld-storm total=%llu\n", (unsigned long long)total);
}

int main(int argc, char** argv) {
  const char* mode = argc > 1 ? argv[1] : "";
  setvbuf(stdout, NULL, _IOLBF, 0);
  if (!strcmp(mode, "catchup"))
    mode_catchup();
  else if (!strcmp(mode, "abstime"))
    mode_abstime();
  else if (!strcmp(mode, "nonblock"))
    mode_nonblock();
  else if (!strcmp(mode, "epoll-pwait2"))
    mode_epoll_pwait2();
  else if (!strcmp(mode, "zero-timeout"))
    mode_zero_timeout();
  else if (!strcmp(mode, "waits"))
    mode_waits();
  else if (!strcmp(mode, "masked-waits"))
    mode_masked_waits();
  else if (!strcmp(mode, "masked-ppoll-expired"))
    mode_masked_ppoll_expired();
  else if (!strcmp(mode, "scm-rights"))
    mode_scm_rights(0);
  else if (!strcmp(mode, "scm-rights-epoll"))
    mode_scm_rights(1);
  else if (!strcmp(mode, "scm-rights-unarmed"))
    mode_scm_rights_unarmed();
  else if (!strcmp(mode, "catchup-sigcont"))
    mode_catchup_sigcont();
  else if (!strcmp(mode, "epollet"))
    mode_epollet();
  else if (!strcmp(mode, "dup-fork"))
    mode_dup_fork();
  else if (!strcmp(mode, "exec"))
    mode_exec(argv[0]);
  else if (!strcmp(mode, "exec-child") && argc > 2)
    mode_exec_child(argv[2]);
  else if (!strcmp(mode, "readv"))
    mode_readv();
  else if (!strcmp(mode, "sigchld-storm"))
    mode_sigchld_storm();
  else {
    printf("FAIL unknown mode %s\n", mode);
    return 2;
  }
  return 0;
}
