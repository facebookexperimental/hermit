/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif
#include <errno.h>
#include <poll.h>
#include <pthread.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/resource.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

static void* delayed_writer(void* argument) {
  int fd = *(int*)argument;
  struct timespec delay = {.tv_sec = 0, .tv_nsec = 1000000};
  if (nanosleep(&delay, NULL) != 0 || write(fd, "w", 1) != 1) {
    return (void*)(uintptr_t)1;
  }
  return NULL;
}

static int run_raw_timeout_copyout(void) {
  int pipefd[2];
  if (pipe(pipefd) != 0 || write(pipefd[1], "r", 1) != 1) {
    perror("record-replay ready pipe");
    return 1;
  }
  struct pollfd ready = {.fd = pipefd[0], .events = POLLIN};

  const struct timespec raw_ready_input = {.tv_sec = 3, .tv_nsec = 456789123};
  struct timespec raw_ready_timeout = raw_ready_input;
  if (syscall(
          SYS_ppoll, &ready, 1, &raw_ready_timeout, NULL, sizeof(uint64_t)) !=
          1 ||
      !(ready.revents & POLLIN) || raw_ready_timeout.tv_sec < 0 ||
      raw_ready_timeout.tv_nsec < 0 ||
      raw_ready_timeout.tv_nsec >= 1000000000 ||
      (raw_ready_timeout.tv_sec == 0 && raw_ready_timeout.tv_nsec == 0) ||
      raw_ready_timeout.tv_sec > raw_ready_input.tv_sec ||
      (raw_ready_timeout.tv_sec == raw_ready_input.tv_sec &&
       raw_ready_timeout.tv_nsec >= raw_ready_input.tv_nsec)) {
    fprintf(
        stderr,
        "raw ready ppoll did not preserve a positive irregular remainder: "
        "%ld.%09ld errno=%d revents=%d\n",
        raw_ready_timeout.tv_sec,
        raw_ready_timeout.tv_nsec,
        errno,
        ready.revents);
    return 1;
  }

  long page_size = sysconf(_SC_PAGESIZE);
  if (page_size <= 0) {
    perror("sysconf page size");
    return 1;
  }
  void* timeout_page = mmap(
      NULL,
      (size_t)page_size,
      PROT_READ | PROT_WRITE,
      MAP_PRIVATE | MAP_ANONYMOUS,
      -1,
      0);
  if (timeout_page == MAP_FAILED) {
    perror("mmap readonly timeout");
    return 1;
  }
  struct timespec* readonly_timeout = timeout_page;
  *readonly_timeout = raw_ready_input;
  if (mprotect(timeout_page, (size_t)page_size, PROT_READ) != 0) {
    int saved_errno = errno;
    munmap(timeout_page, (size_t)page_size);
    errno = saved_errno;
    perror("mprotect readonly timeout");
    return 1;
  }
  struct pollfd ready_before_timeout_fault = {
      .fd = pipefd[0], .events = POLLIN};
  errno = 0;
  long timeout_fault_result = syscall(
      SYS_ppoll,
      &ready_before_timeout_fault,
      1,
      readonly_timeout,
      NULL,
      sizeof(uint64_t));
  int timeout_fault_errno = errno;
  int timeout_fault_revents = ready_before_timeout_fault.revents;
  struct timespec timeout_after_write_fault = *readonly_timeout;
  if (munmap(timeout_page, (size_t)page_size) != 0) {
    perror("munmap readonly timeout");
    return 1;
  }
  if (timeout_fault_result != 1 || timeout_fault_errno != 0 ||
      !(timeout_fault_revents & POLLIN) ||
      timeout_after_write_fault.tv_sec != raw_ready_input.tv_sec ||
      timeout_after_write_fault.tv_nsec != raw_ready_input.tv_nsec) {
    fprintf(
        stderr,
        "raw ready ppoll did not preserve its result across timeout "
        "writeback failure: result=%ld errno=%d revents=%d "
        "timeout=%ld.%09ld\n",
        timeout_fault_result,
        timeout_fault_errno,
        timeout_fault_revents,
        timeout_after_write_fault.tv_sec,
        timeout_after_write_fault.tv_nsec);
    return 1;
  }

  if (close(pipefd[0]) != 0 || close(pipefd[1]) != 0) {
    perror("close record-replay descriptors");
    return 1;
  }
  return 0;
}

static int run_invalid_nfds(void) {
  struct rlimit limit;
  if (getrlimit(RLIMIT_NOFILE, &limit) != 0) {
    perror("getrlimit");
    return 1;
  }
  if (limit.rlim_cur == RLIM_INFINITY ||
      limit.rlim_cur >= (rlim_t)((nfds_t)-1)) {
    fputs("RLIMIT_NOFILE cannot be incremented as nfds_t\n", stderr);
    return 1;
  }

  nfds_t nfds = (nfds_t)limit.rlim_cur + 1;
  errno = 0;
  long result =
      syscall(SYS_ppoll, (void*)1, nfds, NULL, NULL, sizeof(uint64_t));
  if (result != -1 || errno != EINVAL) {
    fprintf(
        stderr,
        "invalid nfds ppoll did not return EINVAL: result=%ld errno=%d\n",
        result,
        errno);
    return 1;
  }
  return 0;
}

static int run_zero_timeout_alias(void) {
  int saved_stdin = dup(STDIN_FILENO);
  int pipefd[2];
  if (saved_stdin < 0 || pipe(pipefd) != 0) {
    perror("zero timeout alias setup");
    return 1;
  }
  if (dup2(pipefd[0], STDIN_FILENO) < 0) {
    perror("zero timeout alias dup2");
    return 1;
  }
  if (close(pipefd[0]) != 0 || close(pipefd[1]) != 0) {
    perror("zero timeout alias close pipe");
    return 1;
  }

  union {
    struct timespec timeout;
    struct pollfd pfd;
  } aliased;
  memset(&aliased, 0, sizeof(aliased));
  errno = 0;
  long result = syscall(
      SYS_ppoll, &aliased.pfd, 1, &aliased.timeout, NULL, sizeof(uint64_t));
  int observed_errno = errno;
  short observed_revents = aliased.pfd.revents;

  if (dup2(saved_stdin, STDIN_FILENO) < 0 || close(saved_stdin) != 0) {
    perror("zero timeout alias restore stdin");
    return 1;
  }
  if (result != 1 || observed_errno != 0 || !(observed_revents & POLLHUP)) {
    fprintf(
        stderr,
        "zero timeout alias ppoll diverged: result=%ld errno=%d "
        "revents=%d\n",
        result,
        observed_errno,
        observed_revents);
    return 1;
  }
  return 0;
}

static int run_masked_readonly_timeout(void) {
  int pipefd[2];
  if (pipe(pipefd) != 0 || write(pipefd[1], "r", 1) != 1) {
    perror("masked readonly timeout ready pipe");
    return 1;
  }

  long page_size = sysconf(_SC_PAGESIZE);
  if (page_size <= 0) {
    perror("sysconf page size");
    return 1;
  }
  void* timeout_page = mmap(
      NULL,
      (size_t)page_size,
      PROT_READ | PROT_WRITE,
      MAP_PRIVATE | MAP_ANONYMOUS,
      -1,
      0);
  if (timeout_page == MAP_FAILED) {
    perror("mmap masked readonly timeout");
    return 1;
  }
  const struct timespec input = {.tv_sec = 3, .tv_nsec = 456789123};
  struct timespec* timeout = timeout_page;
  *timeout = input;
  if (mprotect(timeout_page, (size_t)page_size, PROT_READ) != 0) {
    int saved_errno = errno;
    munmap(timeout_page, (size_t)page_size);
    errno = saved_errno;
    perror("mprotect masked readonly timeout");
    return 1;
  }

  sigset_t mask;
  sigemptyset(&mask);
  sigaddset(&mask, SIGUSR1);
  struct pollfd ready = {.fd = pipefd[0], .events = POLLIN};
  errno = 0;
  long result = syscall(SYS_ppoll, &ready, 1, timeout, &mask, sizeof(uint64_t));
  int observed_errno = errno;
  struct timespec observed_timeout = *timeout;

  if (munmap(timeout_page, (size_t)page_size) != 0) {
    perror("munmap masked readonly timeout");
    return 1;
  }
  if (close(pipefd[0]) != 0 || close(pipefd[1]) != 0) {
    perror("close masked readonly timeout descriptors");
    return 1;
  }
  if (result != 1 || observed_errno != 0 || !(ready.revents & POLLIN) ||
      observed_timeout.tv_sec != input.tv_sec ||
      observed_timeout.tv_nsec != input.tv_nsec) {
    fprintf(
        stderr,
        "masked readonly timeout ppoll diverged: result=%ld errno=%d "
        "revents=%d timeout=%ld.%09ld\n",
        result,
        observed_errno,
        ready.revents,
        observed_timeout.tv_sec,
        observed_timeout.tv_nsec);
    return 1;
  }
  return 0;
}

static int run_masked_readonly_zero_timeout(void) {
  int pipefd[2];
  if (pipe(pipefd) != 0 || write(pipefd[1], "r", 1) != 1) {
    perror("masked readonly zero timeout ready pipe");
    return 1;
  }

  long page_size = sysconf(_SC_PAGESIZE);
  if (page_size <= 0) {
    perror("sysconf page size");
    return 1;
  }
  void* timeout_page = mmap(
      NULL,
      (size_t)page_size,
      PROT_READ | PROT_WRITE,
      MAP_PRIVATE | MAP_ANONYMOUS,
      -1,
      0);
  if (timeout_page == MAP_FAILED) {
    perror("mmap masked readonly zero timeout");
    return 1;
  }
  const struct timespec input = {.tv_sec = 0, .tv_nsec = 0};
  struct timespec* timeout = timeout_page;
  *timeout = input;
  if (mprotect(timeout_page, (size_t)page_size, PROT_READ) != 0) {
    int saved_errno = errno;
    munmap(timeout_page, (size_t)page_size);
    errno = saved_errno;
    perror("mprotect masked readonly zero timeout");
    return 1;
  }

  sigset_t mask;
  sigemptyset(&mask);
  sigaddset(&mask, SIGUSR1);
  struct pollfd ready = {.fd = pipefd[0], .events = POLLIN};
  errno = 0;
  long result = syscall(SYS_ppoll, &ready, 1, timeout, &mask, sizeof(uint64_t));
  int observed_errno = errno;
  struct timespec observed_timeout = *timeout;

  if (munmap(timeout_page, (size_t)page_size) != 0) {
    perror("munmap masked readonly zero timeout");
    return 1;
  }
  if (close(pipefd[0]) != 0 || close(pipefd[1]) != 0) {
    perror("close masked readonly zero timeout descriptors");
    return 1;
  }
  if (result != 1 || observed_errno != 0 || !(ready.revents & POLLIN) ||
      observed_timeout.tv_sec != input.tv_sec ||
      observed_timeout.tv_nsec != input.tv_nsec) {
    fprintf(
        stderr,
        "masked readonly zero timeout ppoll diverged: result=%ld "
        "errno=%d revents=%d timeout=%ld.%09ld\n",
        result,
        observed_errno,
        ready.revents,
        observed_timeout.tv_sec,
        observed_timeout.tv_nsec);
    return 1;
  }
  return 0;
}

/* A masked ppoll on a ready pipe, then on the drained pipe with a 1 ms
 * timeout. The blocking call times out with 0, as on Linux, when `times_out`
 * is set: record and replay let the kernel own the wait, and a sequentialized
 * ptrace run polls it under its mask (https://github.com/rrnewton/hermit/issues/4042).
 * A backend whose kernel does not report the guest's signal state fails it
 * closed with ENOSYS. */
static int run_masked_fail_closed(int times_out) {
  int pipefd[2];
  if (pipe(pipefd) != 0 || write(pipefd[1], "r", 1) != 1) {
    perror("masked fail-closed ready pipe");
    return 1;
  }

  sigset_t mask;
  sigemptyset(&mask);
  sigaddset(&mask, SIGUSR1);
  struct pollfd fd = {.fd = pipefd[0], .events = POLLIN};
  struct timespec ready_timeout = {.tv_sec = 1, .tv_nsec = 0};
  if (ppoll(&fd, 1, &ready_timeout, &mask) != 1 || !(fd.revents & POLLIN)) {
    perror("masked ready ppoll");
    return 1;
  }

  char byte;
  if (read(pipefd[0], &byte, 1) != 1) {
    perror("drain masked ready pipe");
    return 1;
  }
  fd.revents = 0;
  struct timespec blocked_timeout = {.tv_sec = 0, .tv_nsec = 1000000};
  errno = 0;
  int blocked = ppoll(&fd, 1, &blocked_timeout, &mask);
  if (times_out ? blocked != 0 : blocked != -1 || errno != ENOSYS) {
    fprintf(
        stderr,
        "blocking masked ppoll: result=%d errno=%d revents=%d, expected %s\n",
        blocked,
        errno,
        fd.revents,
        times_out ? "0 after the timeout" : "ENOSYS");
    return 1;
  }

  if (close(pipefd[0]) != 0 || close(pipefd[1]) != 0) {
    perror("close masked fail-closed descriptors");
    return 1;
  }
  return 0;
}

static int run_inaccessible_mask_kernel_wait(void) {
  long page_size = sysconf(_SC_PAGESIZE);
  if (page_size <= 0) {
    perror("sysconf page size");
    return 1;
  }
  void* mask_page = mmap(
      NULL,
      (size_t)page_size,
      PROT_READ | PROT_WRITE,
      MAP_PRIVATE | MAP_ANONYMOUS,
      -1,
      0);
  if (mask_page == MAP_FAILED) {
    perror("mmap inaccessible ppoll mask");
    return 1;
  }
  if (mprotect(mask_page, (size_t)page_size, PROT_NONE) != 0) {
    int saved_errno = errno;
    munmap(mask_page, (size_t)page_size);
    errno = saved_errno;
    perror("mprotect inaccessible ppoll mask");
    return 1;
  }

  struct timespec timeout = {.tv_sec = 0, .tv_nsec = 1};
  errno = 0;
  long result =
      syscall(SYS_ppoll, NULL, 0, &timeout, mask_page, sizeof(uint64_t));
  int observed_errno = errno;
  if (munmap(mask_page, (size_t)page_size) != 0) {
    perror("munmap inaccessible ppoll mask");
    return 1;
  }
  if (result != -1 || observed_errno != EFAULT) {
    fprintf(
        stderr,
        "inaccessible ppoll mask did not return EFAULT: result=%ld "
        "errno=%d\n",
        result,
        observed_errno);
    return 1;
  }
  return 0;
}

/* How often SIGUSR1's handler ran, and the mask it last ran under. */
static volatile sig_atomic_t usr1_handled;
static sigset_t usr1_handler_mask;
/* The thread the sibling signals, and whether its masked wait has returned. */
static pid_t usr1_target;
static volatile int usr1_wait_returned;

static void note_usr1_handler_mask(int signal) {
  (void)signal;
  pthread_sigmask(SIG_BLOCK, NULL, &usr1_handler_mask);
  usr1_handled++;
}

/* Sends SIGUSR1 to `usr1_target` every 20 ms until its wait returns, so a
 * signal that lands before the wait begins is followed by one that lands in
 * it. */
static void* send_usr1_until_the_wait_returns(void* argument) {
  (void)argument;
  struct timespec delay = {.tv_sec = 0, .tv_nsec = 20000000};
  while (!__atomic_load_n(&usr1_wait_returned, __ATOMIC_SEQ_CST)) {
    if (nanosleep(&delay, NULL) != 0 ||
        syscall(SYS_tgkill, getpid(), usr1_target, SIGUSR1) != 0) {
      return (void*)(uintptr_t)1;
    }
  }
  return NULL;
}

/* A ppoll on an empty pipe, with a 5 s timeout and a mask that blocks
 * SIGUSR2, which a sibling's SIGUSR1 interrupts. The thread's own mask blocks
 * SIGHUP, and SIGUSR1 too when `thread_blocks` is set: the race-free idiom
 * that ppoll's mask exists for, where only the call lets the signal in. Linux
 * returns EINTR once the handler has run under the call's mask, then puts the
 * thread's mask back. A sequentialized run polls the wait under the mask
 * (https://github.com/rrnewton/hermit/issues/4042); a probe that missed a signal
 * the thread's mask lets in, which the ptrace backend can take before the
 * call installs its mask, would run on to the timeout and return 0. That
 * backend delivers such a signal under the thread's mask
 * (https://github.com/rrnewton/reverie/issues/1011), so the handler's mask is
 * checked only when `thread_blocks` is set. */
static int run_masked_signal_interrupts(int thread_blocks) {
  struct sigaction action;
  memset(&action, 0, sizeof(action));
  action.sa_handler = note_usr1_handler_mask;
  sigemptyset(&action.sa_mask);
  if (sigaction(SIGUSR1, &action, NULL) != 0) {
    perror("SIGUSR1 handler");
    return 1;
  }
  sigset_t thread_mask;
  sigemptyset(&thread_mask);
  sigaddset(&thread_mask, SIGHUP);
  if (thread_blocks) {
    sigaddset(&thread_mask, SIGUSR1);
  }
  if (pthread_sigmask(SIG_SETMASK, &thread_mask, NULL) != 0) {
    perror("thread mask");
    return 1;
  }
  sigset_t call_mask;
  sigemptyset(&call_mask);
  sigaddset(&call_mask, SIGUSR2);

  int pipefd[2];
  if (pipe(pipefd) != 0) {
    perror("signal-interrupted pipe");
    return 1;
  }
  usr1_target = (pid_t)syscall(SYS_gettid);
  pthread_t sender;
  if (pthread_create(&sender, NULL, send_usr1_until_the_wait_returns, NULL) !=
      0) {
    perror("SIGUSR1 sender");
    return 1;
  }
  struct pollfd fd = {.fd = pipefd[0], .events = POLLIN};
  struct timespec timeout = {.tv_sec = 5, .tv_nsec = 0};
  errno = 0;
  int result = ppoll(&fd, 1, &timeout, &call_mask);
  int observed_errno = errno;
  __atomic_store_n(&usr1_wait_returned, 1, __ATOMIC_SEQ_CST);
  void* sent = NULL;
  if (pthread_join(sender, &sent) != 0 || sent != NULL) {
    fprintf(stderr, "SIGUSR1 sender failed\n");
    return 1;
  }
  sigset_t after;
  pthread_sigmask(SIG_BLOCK, NULL, &after);
  if (result != -1 || observed_errno != EINTR || usr1_handled < 1) {
    fprintf(
        stderr,
        "signal-interrupted masked ppoll: result=%d errno=%d handled=%d, "
        "expected EINTR after the handler\n",
        result,
        observed_errno,
        (int)usr1_handled);
    return 1;
  }
  if (thread_blocks && (sigismember(&usr1_handler_mask, SIGUSR2) != 1 ||
                        sigismember(&usr1_handler_mask, SIGHUP) != 0)) {
    fprintf(
        stderr,
        "SIGUSR1 handler ran under the thread's mask, not the call's: "
        "SIGUSR2 blocked=%d SIGHUP blocked=%d\n",
        sigismember(&usr1_handler_mask, SIGUSR2),
        sigismember(&usr1_handler_mask, SIGHUP));
    return 1;
  }
  if (sigismember(&after, SIGHUP) != 1 || sigismember(&after, SIGUSR2) != 0 ||
      sigismember(&after, SIGUSR1) != thread_blocks) {
    fprintf(stderr, "the thread's mask was not put back after the wait\n");
    return 1;
  }
  if (close(pipefd[0]) != 0 || close(pipefd[1]) != 0) {
    perror("close signal-interrupted pipe");
    return 1;
  }
  return 0;
}

/* How often SIGUSR2's handler ran, whether the pipe was ready when it last
 * ran (-1 before it ran), and the pipe's two ends. */
static volatile sig_atomic_t usr2_handled;
static volatile sig_atomic_t usr2_ready_in_handler = -1;
static int usr2_pipe_reader = -1;
static int usr2_pipe_writer = -1;

static void note_usr2(int signal) {
  (void)signal;
  struct pollfd read_end = {.fd = usr2_pipe_reader, .events = POLLIN};
  usr2_ready_in_handler = poll(&read_end, 1, 0);
  usr2_handled++;
}

/* Sends SIGUSR2 to `usr1_target` 20 ms into its wait, then makes the pipe
 * ready 20 ms later. */
static void* send_usr2_then_write(void* argument) {
  (void)argument;
  struct timespec delay = {.tv_sec = 0, .tv_nsec = 20000000};
  if (nanosleep(&delay, NULL) != 0 ||
      syscall(SYS_tgkill, getpid(), usr1_target, SIGUSR2) != 0 ||
      nanosleep(&delay, NULL) != 0 || write(usr2_pipe_writer, "w", 1) != 1) {
    return (void*)(uintptr_t)1;
  }
  return NULL;
}

/* A ppoll on an empty pipe, with a 5 s timeout and a mask that blocks
 * SIGUSR2, which the thread's own mask lets in. A sibling sends SIGUSR2 into
 * the wait and then makes the pipe ready. Linux keeps waiting, since the
 * call's mask blocks the signal, returns 1 for the pipe, and runs the handler
 * as the call returns and the thread's mask comes back. A wait that decided
 * against the thread's mask would end with EINTR instead. */
static int run_masked_signal_blocked_by_call(void) {
  struct sigaction action;
  memset(&action, 0, sizeof(action));
  action.sa_handler = note_usr2;
  sigemptyset(&action.sa_mask);
  if (sigaction(SIGUSR2, &action, NULL) != 0) {
    perror("SIGUSR2 handler");
    return 1;
  }
  sigset_t thread_mask;
  sigemptyset(&thread_mask);
  sigaddset(&thread_mask, SIGHUP);
  if (pthread_sigmask(SIG_SETMASK, &thread_mask, NULL) != 0) {
    perror("thread mask");
    return 1;
  }
  sigset_t call_mask;
  sigemptyset(&call_mask);
  sigaddset(&call_mask, SIGUSR2);

  int pipefd[2];
  if (pipe(pipefd) != 0) {
    perror("call-blocked pipe");
    return 1;
  }
  usr2_pipe_reader = pipefd[0];
  usr2_pipe_writer = pipefd[1];
  usr1_target = (pid_t)syscall(SYS_gettid);
  pthread_t sender;
  if (pthread_create(&sender, NULL, send_usr2_then_write, NULL) != 0) {
    perror("SIGUSR2 sender");
    return 1;
  }
  struct pollfd fd = {.fd = pipefd[0], .events = POLLIN};
  struct timespec timeout = {.tv_sec = 5, .tv_nsec = 0};
  errno = 0;
  int result = ppoll(&fd, 1, &timeout, &call_mask);
  int observed_errno = errno;
  void* sent = NULL;
  if (pthread_join(sender, &sent) != 0 || sent != NULL) {
    fprintf(stderr, "SIGUSR2 sender failed\n");
    return 1;
  }
  if (result != 1 || !(fd.revents & POLLIN) || usr2_handled != 1 ||
      usr2_ready_in_handler != 1) {
    fprintf(
        stderr,
        "masked ppoll with a call-blocked signal: result=%d errno=%d "
        "revents=%d handled=%d ready_in_handler=%d, expected 1 for the pipe "
        "and the handler after the call, with the pipe ready\n",
        result,
        observed_errno,
        fd.revents,
        (int)usr2_handled,
        (int)usr2_ready_in_handler);
    return 1;
  }
  if (close(pipefd[0]) != 0 || close(pipefd[1]) != 0) {
    perror("close call-blocked pipe");
    return 1;
  }
  return 0;
}

/* The raw ppoll system call with a mask, which, unlike glibc's ppoll, gets the
 * caller's own timeout and writes the time left back to it. With `signal`
 * clear it waits 1 ms on an empty pipe and must time out with 0 and a zero
 * time left. With `signal` set the thread blocks SIGUSR1, only the call's
 * mask lets it in, and a sibling's SIGUSR1 ends the 5 s wait with EINTR,
 * which must leave a time left above zero and below 5 s. */
static int run_masked_raw_remaining(int signal) {
  struct sigaction action;
  memset(&action, 0, sizeof(action));
  action.sa_handler = note_usr1_handler_mask;
  sigemptyset(&action.sa_mask);
  if (sigaction(SIGUSR1, &action, NULL) != 0) {
    perror("SIGUSR1 handler");
    return 1;
  }
  sigset_t thread_mask;
  sigemptyset(&thread_mask);
  sigaddset(&thread_mask, SIGUSR1);
  if (pthread_sigmask(SIG_SETMASK, &thread_mask, NULL) != 0) {
    perror("thread mask");
    return 1;
  }
  uint64_t call_mask = 0;
  int pipefd[2];
  if (pipe(pipefd) != 0) {
    perror("raw remaining pipe");
    return 1;
  }
  pthread_t sender;
  usr1_target = (pid_t)syscall(SYS_gettid);
  if (signal &&
      pthread_create(&sender, NULL, send_usr1_until_the_wait_returns, NULL) !=
          0) {
    perror("SIGUSR1 sender");
    return 1;
  }
  struct pollfd fd = {.fd = pipefd[0], .events = POLLIN};
  struct timespec timeout = signal ? (struct timespec){.tv_sec = 5}
                                   : (struct timespec){.tv_nsec = 1000000};
  errno = 0;
  long result =
      syscall(SYS_ppoll, &fd, 1, &timeout, &call_mask, sizeof(call_mask));
  int observed_errno = errno;
  __atomic_store_n(&usr1_wait_returned, 1, __ATOMIC_SEQ_CST);
  void* sent = NULL;
  if (signal && (pthread_join(sender, &sent) != 0 || sent != NULL)) {
    fprintf(stderr, "SIGUSR1 sender failed\n");
    return 1;
  }
  int ok = signal
      ? (result == -1 && observed_errno == EINTR && usr1_handled >= 1 &&
         (timeout.tv_sec > 0 || timeout.tv_nsec > 0) && timeout.tv_sec < 5)
      : (result == 0 && timeout.tv_sec == 0 && timeout.tv_nsec == 0);
  if (!ok) {
    fprintf(
        stderr,
        "raw masked ppoll: result=%ld errno=%d handled=%d time_left=%ld.%09ld\n",
        result,
        observed_errno,
        (int)usr1_handled,
        (long)timeout.tv_sec,
        timeout.tv_nsec);
    return 1;
  }
  if (close(pipefd[0]) != 0 || close(pipefd[1]) != 0) {
    perror("close raw remaining pipe");
    return 1;
  }
  return 0;
}

static int run_default_workload(int recording) {
  int pipefd[2];
  if (pipe(pipefd) != 0 || write(pipefd[1], "r", 1) != 1) {
    perror("ready pipe");
    return 1;
  }

  sigset_t mask;
  sigemptyset(&mask);
  sigaddset(&mask, SIGUSR1);
  struct pollfd ready = {.fd = pipefd[0], .events = POLLIN};

  struct timespec ready_timeout = {.tv_sec = 1, .tv_nsec = 0};
  if (ppoll(&ready, 1, &ready_timeout, &mask) != 1 ||
      !(ready.revents & POLLIN)) {
    perror("ready ppoll");
    return 1;
  }

  char byte;
  if (read(pipefd[0], &byte, 1) != 1) {
    perror("drain ready pipe");
    return 1;
  }

  ready.revents = 0;
  struct timespec zero = {.tv_sec = 0, .tv_nsec = 0};
  if (ppoll(&ready, 1, &zero, NULL) != 0) {
    perror("zero ppoll");
    return 1;
  }

  struct timespec finite = {.tv_sec = 0, .tv_nsec = 20000000};
  if (ppoll(&ready, 1, &finite, NULL) != 0) {
    perror("finite ppoll");
    return 1;
  }

  struct timespec raw_timeout = {.tv_sec = 0, .tv_nsec = 5000000};
  if (syscall(SYS_ppoll, NULL, 0, &raw_timeout, NULL, sizeof(uint64_t)) != 0 ||
      raw_timeout.tv_sec != 0 || raw_timeout.tv_nsec != 0) {
    fprintf(
        stderr,
        "raw ppoll timeout was not consumed: %ld.%09ld\n",
        raw_timeout.tv_sec,
        raw_timeout.tv_nsec);
    return 1;
  }

  struct timespec invalid = {.tv_sec = 0, .tv_nsec = 1000000000};
  errno = 0;
  if (syscall(SYS_ppoll, NULL, 0, &invalid, NULL, sizeof(uint64_t)) != -1 ||
      errno != EINVAL) {
    fprintf(stderr, "invalid ppoll timeout: errno=%d\n", errno);
    return 1;
  }

  errno = 0;
  if (syscall(SYS_ppoll, NULL, 0, (void*)1, NULL, sizeof(uint64_t)) != -1 ||
      errno != EFAULT) {
    fprintf(stderr, "bad ppoll timeout pointer: errno=%d\n", errno);
    return 1;
  }

  // Recording lets the kernel own a masked wait, which times out with 0. A
  // sequentialized run's answer depends on the backend (`run_masked_fail_closed`),
  // and this workload runs on several, so its modes check it instead.
  if (recording) {
    struct timespec masked_timeout = {.tv_sec = 0, .tv_nsec = 1000000};
    errno = 0;
    int masked = ppoll(&ready, 1, &masked_timeout, &mask);
    if (masked != 0) {
      fprintf(
          stderr,
          "blocking masked ppoll: result=%d errno=%d, expected 0 after the "
          "kernel's timeout\n",
          masked,
          errno);
      return 1;
    }
  }

  int delayed_pipe[2];
  if (pipe(delayed_pipe) != 0) {
    perror("delayed pipe");
    return 1;
  }
  pthread_t writer;
  if (pthread_create(&writer, NULL, delayed_writer, &delayed_pipe[1]) != 0) {
    perror("pthread_create");
    return 1;
  }
  struct pollfd delayed = {.fd = delayed_pipe[0], .events = POLLIN};
  if (ppoll(&delayed, 1, NULL, NULL) != 1 || !(delayed.revents & POLLIN)) {
    perror("infinite ppoll");
    return 1;
  }
  void* writer_result = NULL;
  if (pthread_join(writer, &writer_result) != 0 || writer_result != NULL) {
    fputs("delayed writer failed\n", stderr);
    return 1;
  }

  if (run_zero_timeout_alias() != 0) {
    return 1;
  }

  puts("ppoll-simulation-ok");
  return 0;
}

int main(int argc, char** argv) {
  if (argc == 1) {
    return run_default_workload(0);
  }
  if (argc != 2) {
    fprintf(
        stderr,
        "usage: %s [raw-timeout-copyout|masked-readonly-timeout|"
        "masked-readonly-zero-timeout|masked-fail-closed|"
        "masked-simulated-wait|masked-signal-interrupts|"
        "masked-signal-thread-admits|masked-signal-blocked-by-call|"
        "masked-raw-timeout-remaining|masked-raw-signal-remaining|"
        "masked-record-kernel-wait|"
        "inaccessible-mask-kernel-wait|record-replay|zero-timeout-alias]\n",
        argv[0]);
    return 2;
  }
  if (strcmp(argv[1], "raw-timeout-copyout") == 0) {
    int result = run_raw_timeout_copyout();
    if (result == 0) {
      puts("ppoll-simulation-ok");
    }
    return result;
  }
  if (strcmp(argv[1], "record-replay") == 0) {
    int result = run_raw_timeout_copyout();
    if (result == 0) {
      result = run_invalid_nfds();
    }
    return result == 0 ? run_default_workload(1) : result;
  }
  if (strcmp(argv[1], "masked-readonly-timeout") == 0) {
    int result = run_masked_readonly_timeout();
    if (result == 0) {
      puts("ppoll-simulation-ok");
    }
    return result;
  }
  if (strcmp(argv[1], "masked-readonly-zero-timeout") == 0) {
    int result = run_masked_readonly_zero_timeout();
    if (result == 0) {
      puts("ppoll-simulation-ok");
    }
    return result;
  }
  if (strcmp(argv[1], "masked-fail-closed") == 0) {
    int result = run_masked_fail_closed(0);
    if (result == 0) {
      puts("ppoll-simulation-ok");
    }
    return result;
  }
  if (strcmp(argv[1], "masked-simulated-wait") == 0) {
    int result = run_masked_fail_closed(1);
    if (result == 0) {
      puts("ppoll-simulation-ok");
    }
    return result;
  }
  if (strcmp(argv[1], "masked-signal-interrupts") == 0) {
    int result = run_masked_signal_interrupts(1);
    if (result == 0) {
      puts("ppoll-simulation-ok");
    }
    return result;
  }
  if (strcmp(argv[1], "masked-signal-thread-admits") == 0) {
    int result = run_masked_signal_interrupts(0);
    if (result == 0) {
      puts("ppoll-simulation-ok");
    }
    return result;
  }
  if (strcmp(argv[1], "masked-signal-blocked-by-call") == 0) {
    int result = run_masked_signal_blocked_by_call();
    if (result == 0) {
      puts("ppoll-simulation-ok");
    }
    return result;
  }
  if (strcmp(argv[1], "masked-raw-timeout-remaining") == 0) {
    int result = run_masked_raw_remaining(0);
    if (result == 0) {
      puts("ppoll-simulation-ok");
    }
    return result;
  }
  if (strcmp(argv[1], "masked-raw-signal-remaining") == 0) {
    int result = run_masked_raw_remaining(1);
    if (result == 0) {
      puts("ppoll-simulation-ok");
    }
    return result;
  }
  if (strcmp(argv[1], "masked-record-kernel-wait") == 0) {
    int result = run_masked_fail_closed(1);
    if (result == 0) {
      puts("ppoll-simulation-ok");
    }
    return result;
  }
  if (strcmp(argv[1], "inaccessible-mask-kernel-wait") == 0) {
    int result = run_inaccessible_mask_kernel_wait();
    if (result == 0) {
      puts("ppoll-simulation-ok");
    }
    return result;
  }
  if (strcmp(argv[1], "zero-timeout-alias") == 0) {
    int result = run_zero_timeout_alias();
    if (result == 0) {
      puts("ppoll-simulation-ok");
    }
    return result;
  }

  fprintf(
      stderr,
      "usage: %s [raw-timeout-copyout|masked-readonly-timeout|"
      "masked-readonly-zero-timeout|masked-fail-closed|"
      "inaccessible-mask-kernel-wait|record-replay|zero-timeout-alias]\n",
      argv[0]);
  return 2;
}
