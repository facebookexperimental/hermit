/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Record/replay regression guest for select(2) and pselect6(2)
 * (https://github.com/rrnewton/hermit/issues/3569).
 *
 * Replay does not refill pipes, so a pipe that held data while recording is
 * empty at replay. If select or pselect6 asks the live kernel again at replay
 * instead of restoring the recorded outputs, a ready descriptor reads as not
 * ready: the result, the returned fd sets, and the remaining timeout diverge.
 * Every observed output is printed with fixed widths, so a divergence changes
 * the bytes of an otherwise identical write rather than its shape.
 *
 * Modes:
 *   raw          raw SYS_select
 *   glibc        glibc select(), which x86-64 glibc issues as pselect6
 *   pselect-mask glibc pselect() with a real temporary signal mask
 *   efault       raw select whose write set is on a read-only page: Linux
 *                writes the read set, faults on the write set, returns EFAULT
 *   einval       raw select and pselect6 with a negative nfds
 *   poll         poll and ppoll on a ready pipe, an already-recorded control
 *   thread-wake  a second thread blocks in raw select with a NULL timeout on an
 *                empty pipe that the main thread then fills: the call records
 *                as blocking external I/O, and replay must not wait on the
 *                live pipe
 *   pselect-mask-blocks-alarm
 *                the main thread blocks in glibc pselect() with a NULL timeout
 *                and a temporary mask that blocks SIGALRM, which its ordinary
 *                mask leaves unblocked and caught; a 100 ms ITIMER_REAL fires
 *                during the wait, and a second thread, which blocks SIGALRM,
 *                fills the pipe at 300 ms. Linux does not wake the call for
 *                the blocked SIGALRM, so it returns 1 and the handler runs
 *                only after it returns
 *                (https://github.com/rrnewton/hermit/issues/3963)
 *   pselect-mask-writeonly-wrapper
 *                the blocks-alarm wait, with pselect6's { sigmask, sigsetsize }
 *                wrapper on a page mapped PROT_WRITE only, which Linux/x86_64
 *                still copies from; a tracer read of more than eight bytes
 *                cannot reach it
 *   pselect-mask-sibling-rewrite
 *                the blocks-alarm wait, with a mask that is zero when the
 *                waiter announces its pselect6 and that the writer, which
 *                blocks SIGALRM, sets to block SIGALRM; the call returns 1, or
 *                EINTR if the kernel copied the mask before the rewrite
 *   pselect-mask-shared-alarm
 *                eight rounds of the same wait with a writer thread that leaves
 *                SIGALRM unblocked, so the kernel gives the process-directed
 *                SIGALRM to the writer while the main thread's pselect6 blocks
 *                it; replay must deliver it to the same thread
 *   pselect-mask-unblocks-alarm
 *                the opposite idiom: the main thread blocks SIGALRM, which it
 *                catches, and waits in glibc pselect() with a 30 s bound and
 *                an empty temporary mask; the 100 ms ITIMER_REAL SIGALRM ends
 *                the wait with EINTR and its handler runs, under the temporary
 *                mask, before the call returns. Replay serves the call from the
 *                recording, and the kernel, finding the signal blocked by the
 *                thread's own mask, restarted it
 *                (https://github.com/rrnewton/hermit/issues/3992)
 *   pselect-mask-unblocks-rewritten
 *                the unblocks wait, with a second thread, which blocks SIGALRM,
 *                adding SIGALRM to the call's mask buffer after 1000 yields,
 *                once the call sleeps. Linux copied the mask at entry, so the
 *                call still ends with EINTR, and replay must not read the
 *                buffer again (review of
 *                https://github.com/rrnewton/hermit/pull/4052)
 *
 * The first three modes each run four shapes: a ready pipe with a 5 s timeout,
 * the same with a zero timeout, the same with nfds == FD_SETSIZE (larger than
 * the fd table, which Linux clamps), and an empty pipe with a 10 ms timeout.
 */

#define _GNU_SOURCE

#include <errno.h>
#include <poll.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/select.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <time.h>
#include <unistd.h>

enum call_kind { RAW_SELECT, GLIBC_SELECT, GLIBC_PSELECT_MASKED };

struct fds {
  int ready_read;
  int ready_write;
  int empty_read;
  int empty_write;
};

static int open_fds(struct fds* fds) {
  int ready[2];
  int empty[2];
  if (pipe(ready) != 0 || pipe(empty) != 0 || write(ready[1], "x", 1) != 1) {
    perror("prepare pipes");
    return -1;
  }
  fds->ready_read = ready[0];
  fds->ready_write = ready[1];
  fds->empty_read = empty[0];
  fds->empty_write = empty[1];
  return 0;
}

static void close_fds(const struct fds* fds) {
  close(fds->ready_read);
  close(fds->ready_write);
  close(fds->empty_read);
  close(fds->empty_write);
}

static int highest_fd(const struct fds* fds) {
  int result = fds->ready_read;
  if (fds->ready_write > result) {
    result = fds->ready_write;
  }
  if (fds->empty_read > result) {
    result = fds->empty_read;
  }
  if (fds->empty_write > result) {
    result = fds->empty_write;
  }
  return result;
}

/*
 * One select-family call. With `ready` set, the read set holds the ready and
 * the empty read ends, the write set holds the ready pipe's write end, and the
 * except set holds the ready read end, so Linux returns 2. Without it, only the
 * empty read end is watched, so the call times out with 0.
 */
static int one_call(
    const char* label,
    enum call_kind kind,
    const struct fds* fds,
    int ready,
    int nfds,
    long timeout_sec,
    long timeout_usec) {
  fd_set readfds;
  fd_set writefds;
  fd_set exceptfds;
  FD_ZERO(&readfds);
  FD_ZERO(&writefds);
  FD_ZERO(&exceptfds);
  FD_SET(fds->empty_read, &readfds);
  if (ready) {
    FD_SET(fds->ready_read, &readfds);
    FD_SET(fds->ready_write, &writefds);
    FD_SET(fds->ready_read, &exceptfds);
  }

  struct timeval tv = {.tv_sec = timeout_sec, .tv_usec = timeout_usec};
  struct timespec ts = {.tv_sec = timeout_sec, .tv_nsec = timeout_usec * 1000};
  long result;
  errno = 0;
  if (kind == RAW_SELECT) {
    result = syscall(SYS_select, nfds, &readfds, &writefds, &exceptfds, &tv);
  } else if (kind == GLIBC_SELECT) {
    result = select(nfds, &readfds, &writefds, &exceptfds, &tv);
  } else {
    sigset_t mask;
    sigemptyset(&mask);
    sigaddset(&mask, SIGUSR1);
    /* glibc pselect passes a copy of the timespec, so ts stays the input. */
    result = pselect(nfds, &readfds, &writefds, &exceptfds, &ts, &mask);
    tv.tv_sec = ts.tv_sec;
    tv.tv_usec = ts.tv_nsec / 1000;
  }
  int observed_errno = errno;
  int read_ready = FD_ISSET(fds->ready_read, &readfds) ? 1 : 0;
  int read_empty = FD_ISSET(fds->empty_read, &readfds) ? 1 : 0;
  int write_ready = FD_ISSET(fds->ready_write, &writefds) ? 1 : 0;
  int except_ready = FD_ISSET(fds->ready_read, &exceptfds) ? 1 : 0;

  /* Fixed widths keep the write syscall shape identical if a value diverges. */
  printf(
      "%-24s result=%011ld errno=%011d read_ready=%d read_empty=%d "
      "write_ready=%d except=%d timeout=%011ld.%06ld\n",
      label,
      result,
      observed_errno,
      read_ready,
      read_empty,
      write_ready,
      except_ready,
      (long)tv.tv_sec,
      (long)tv.tv_usec);

  long expected = ready ? 2 : 0;
  if (result != expected || read_ready != ready || write_ready != ready ||
      read_empty != 0 || except_ready != 0) {
    fprintf(
        stderr,
        "%s mismatch: result=%ld (expected %ld) errno=%d read_ready=%d "
        "read_empty=%d write_ready=%d except=%d\n",
        label,
        result,
        expected,
        observed_errno,
        read_ready,
        read_empty,
        write_ready,
        except_ready);
    return 1;
  }
  if (tv.tv_sec < 0 || tv.tv_sec > timeout_sec || tv.tv_usec < 0 ||
      tv.tv_usec >= 1000000) {
    fprintf(
        stderr,
        "%s invalid remaining timeout: %ld.%06ld\n",
        label,
        (long)tv.tv_sec,
        (long)tv.tv_usec);
    return 1;
  }
  return 0;
}

static int run_shapes(const char* prefix, enum call_kind kind) {
  struct fds fds;
  if (open_fds(&fds) != 0) {
    return 1;
  }
  int nfds = highest_fd(&fds) + 1;
  char label[64];
  int failures = 0;

  snprintf(label, sizeof(label), "%s-ready", prefix);
  failures += one_call(label, kind, &fds, 1, nfds, 5, 0);
  snprintf(label, sizeof(label), "%s-zero-timeout", prefix);
  failures += one_call(label, kind, &fds, 1, nfds, 0, 0);
  snprintf(label, sizeof(label), "%s-fd-setsize", prefix);
  failures += one_call(label, kind, &fds, 1, FD_SETSIZE, 5, 0);
  snprintf(label, sizeof(label), "%s-timeout", prefix);
  failures += one_call(label, kind, &fds, 0, nfds, 0, 10000);

  close_fds(&fds);
  return failures == 0 ? 0 : 1;
}

static int efault(void) {
  struct fds fds;
  if (open_fds(&fds) != 0) {
    return 1;
  }
  long page_size = sysconf(_SC_PAGESIZE);
  if (page_size <= 0 || (size_t)page_size < sizeof(fd_set)) {
    fprintf(stderr, "invalid page size: %ld\n", page_size);
    return 1;
  }
  unsigned char* pages = mmap(
      NULL,
      (size_t)page_size * 2,
      PROT_READ | PROT_WRITE,
      MAP_PRIVATE | MAP_ANONYMOUS,
      -1,
      0);
  if (pages == MAP_FAILED) {
    perror("mmap fd_set pages");
    return 1;
  }
  fd_set* readfds = (fd_set*)pages;
  fd_set* writefds = (fd_set*)(pages + page_size);
  FD_ZERO(readfds);
  FD_ZERO(writefds);
  FD_SET(fds.ready_read, readfds);
  FD_SET(fds.empty_read, readfds);
  FD_SET(fds.ready_write, writefds);
  /*
   * The write set stays readable, so Linux accepts it as input, but writing
   * the result back faults after the read set was already written.
   */
  if (mprotect(pages + page_size, (size_t)page_size, PROT_READ) != 0) {
    perror("mprotect write set page");
    return 1;
  }

  struct timeval tv = {.tv_sec = 5, .tv_usec = 0};
  errno = 0;
  long result = syscall(
      SYS_select, highest_fd(&fds) + 1, readfds, writefds, NULL, &tv);
  int observed_errno = errno;
  int read_ready = FD_ISSET(fds.ready_read, readfds) ? 1 : 0;
  int read_empty = FD_ISSET(fds.empty_read, readfds) ? 1 : 0;
  int write_input = FD_ISSET(fds.ready_write, writefds) ? 1 : 0;

  if (mprotect(pages + page_size, (size_t)page_size, PROT_READ | PROT_WRITE) !=
          0 ||
      munmap(pages, (size_t)page_size * 2) != 0) {
    perror("release fd_set pages");
    return 1;
  }
  close_fds(&fds);

  printf(
      "%-24s result=%011ld errno=%011d read_ready=%d read_empty=%d "
      "write_input=%d timeout=%011ld.%06ld\n",
      "raw-efault",
      result,
      observed_errno,
      read_ready,
      read_empty,
      write_input,
      (long)tv.tv_sec,
      (long)tv.tv_usec);

  /* The read set holds the result (ready only); the write set is untouched. */
  if (result != -1 || observed_errno != EFAULT || read_ready != 1 ||
      read_empty != 0 || write_input != 1) {
    fprintf(
        stderr,
        "efault mismatch: result=%ld errno=%d read_ready=%d read_empty=%d "
        "write_input=%d\n",
        result,
        observed_errno,
        read_ready,
        read_empty,
        write_input);
    return 1;
  }
  return 0;
}

static int einval(void) {
  fd_set readfds;
  FD_ZERO(&readfds);
  FD_SET(0, &readfds);
  struct timeval tv = {.tv_sec = 5, .tv_usec = 0};
  errno = 0;
  long select_result = syscall(SYS_select, -1, &readfds, NULL, NULL, &tv);
  int select_errno = errno;

  struct timespec ts = {.tv_sec = 5, .tv_nsec = 0};
  errno = 0;
  long pselect_result =
      syscall(SYS_pselect6, -1, &readfds, NULL, NULL, &ts, NULL);
  int pselect_errno = errno;

  printf(
      "%-24s select=%011ld errno=%011d pselect6=%011ld errno=%011d "
      "read0=%d timeout=%011ld.%06ld/%011ld.%09ld\n",
      "einval",
      select_result,
      select_errno,
      pselect_result,
      pselect_errno,
      FD_ISSET(0, &readfds) ? 1 : 0,
      (long)tv.tv_sec,
      (long)tv.tv_usec,
      (long)ts.tv_sec,
      ts.tv_nsec);

  if (select_result != -1 || select_errno != EINVAL || pselect_result != -1 ||
      pselect_errno != EINVAL || !FD_ISSET(0, &readfds)) {
    fprintf(stderr, "einval mismatch\n");
    return 1;
  }
  return 0;
}

static int poll_control(void) {
  struct fds fds;
  if (open_fds(&fds) != 0) {
    return 1;
  }
  struct pollfd pfds[2] = {
      {.fd = fds.ready_read, .events = POLLIN},
      {.fd = fds.empty_read, .events = POLLIN},
  };
  errno = 0;
  long poll_result = poll(pfds, 2, 5000);
  int poll_errno = errno;
  int poll_ready = pfds[0].revents;
  int poll_empty = pfds[1].revents;

  pfds[0].revents = 0;
  pfds[1].revents = 0;
  struct timespec ts = {.tv_sec = 5, .tv_nsec = 0};
  errno = 0;
  long ppoll_result =
      syscall(SYS_ppoll, pfds, 2, &ts, NULL, sizeof(uint64_t));
  int ppoll_errno = errno;
  close_fds(&fds);

  printf(
      "%-24s poll=%011ld errno=%011d revents=%06d/%06d ppoll=%011ld "
      "errno=%011d revents=%06d/%06d timeout=%011ld.%09ld\n",
      "poll",
      poll_result,
      poll_errno,
      poll_ready,
      poll_empty,
      ppoll_result,
      ppoll_errno,
      pfds[0].revents,
      pfds[1].revents,
      (long)ts.tv_sec,
      ts.tv_nsec);

  if (poll_result != 1 || poll_ready != POLLIN || poll_empty != 0 ||
      ppoll_result != 1 || pfds[0].revents != POLLIN || pfds[1].revents != 0) {
    fprintf(stderr, "poll control mismatch\n");
    return 1;
  }
  return 0;
}

struct select_wait {
  int fd;
  long result;
  int result_errno;
  int read_ready;
};

static void* select_without_timeout(void* arg) {
  struct select_wait* wait = arg;
  fd_set read_set;
  FD_ZERO(&read_set);
  FD_SET(wait->fd, &read_set);
  errno = 0;
  wait->result = syscall(SYS_select, wait->fd + 1, &read_set, NULL, NULL, NULL);
  wait->result_errno = errno;
  wait->read_ready = FD_ISSET(wait->fd, &read_set) ? 1 : 0;
  return NULL;
}

/*
 * A peer thread blocks in a select that has no timeout, and the main thread
 * wakes it by writing the pipe. Hermit runs the new thread up to its select
 * request first; the yield then lets the scheduler background that select on
 * the empty pipe before the write, so the select really blocks in the kernel
 * and only the write can finish it. Only the no-delay wake is covered: a writer
 * that first sleeps hangs the recording, because a pending blocking external
 * call keeps the record scheduler from releasing timed waiters
 * (https://github.com/rrnewton/hermit/issues/3576).
 */
static int thread_wake(void) {
  int pipe_fds[2];
  if (pipe(pipe_fds) != 0) {
    perror("pipe");
    return 1;
  }
  struct select_wait wait = {.fd = pipe_fds[0]};
  pthread_t waiter;
  if (pthread_create(&waiter, NULL, select_without_timeout, &wait) != 0) {
    perror("pthread_create");
    return 1;
  }
  sched_yield();
  if (write(pipe_fds[1], "x", 1) != 1) {
    perror("fill pipe");
    return 1;
  }
  if (pthread_join(waiter, NULL) != 0) {
    perror("pthread_join");
    return 1;
  }
  close(pipe_fds[0]);
  close(pipe_fds[1]);

  printf(
      "%-24s result=%011ld errno=%011d read_ready=%01d\n",
      "thread-wake",
      wait.result,
      wait.result_errno,
      wait.read_ready);

  if (wait.result != 1 || wait.read_ready != 1) {
    fprintf(stderr, "thread-wake mismatch\n");
    return 1;
  }
  return 0;
}

static volatile sig_atomic_t masked_alarms = 0;

static void count_masked_alarm(int signal) {
  (void)signal;
  masked_alarms++;
}

static int alarm_pipe_fds[2];

/*
 * The writer inherits a mask that blocks SIGALRM, so the kernel can give the
 * process-directed SIGALRM of ITIMER_REAL only to the waiting thread. Its
 * yields keep the scheduler busy before the timer is due, which gives the
 * waiter time to enter the kernel and install its temporary mask: a SIGALRM
 * sent before that would be taken under the ordinary mask, at a point that
 * host speed decides.
 */
static void* fill_alarm_pipe_later(void* arg) {
  (void)arg;
  for (int i = 0; i < 1000; i++) {
    sched_yield();
  }
  usleep(300000);
  if (write(alarm_pipe_fds[1], "x", 1) != 1) {
    perror("fill pipe");
  }
  return NULL;
}

/*
 * Hermit records pselect6 as a blocking call outside the schedule. The
 * scheduler must know the call sleeps under its own temporary mask: if it
 * takes the thread's ordinary mask instead, it expects the SIGALRM its timer
 * sends to wake the call, waits for a wake Linux never delivers, and refuses
 * the recording after 30 seconds.
 */
static int pselect_mask_blocks_alarm(void) {
  struct sigaction action;
  memset(&action, 0, sizeof action);
  action.sa_handler = count_masked_alarm;
  if (sigaction(SIGALRM, &action, NULL) != 0) {
    perror("sigaction");
    return 1;
  }
  if (pipe(alarm_pipe_fds) != 0) {
    perror("pipe");
    return 1;
  }
  sigset_t during;
  sigemptyset(&during);
  sigaddset(&during, SIGALRM);
  if (pthread_sigmask(SIG_BLOCK, &during, NULL) != 0) {
    perror("pthread_sigmask block");
    return 1;
  }
  pthread_t writer;
  if (pthread_create(&writer, NULL, fill_alarm_pipe_later, NULL) != 0) {
    perror("pthread_create");
    return 1;
  }
  if (pthread_sigmask(SIG_UNBLOCK, &during, NULL) != 0) {
    perror("pthread_sigmask unblock");
    return 1;
  }
  struct itimerval alarm_in = {{0, 0}, {0, 100000}};
  if (setitimer(ITIMER_REAL, &alarm_in, NULL) != 0) {
    perror("setitimer");
    return 1;
  }
  fd_set read_set;
  FD_ZERO(&read_set);
  FD_SET(alarm_pipe_fds[0], &read_set);
  errno = 0;
  int result =
      pselect(alarm_pipe_fds[0] + 1, &read_set, NULL, NULL, NULL, &during);
  int result_errno = errno;
  int read_ready = result > 0 && FD_ISSET(alarm_pipe_fds[0], &read_set);
  if (pthread_join(writer, NULL) != 0) {
    perror("pthread_join");
    return 1;
  }
  close(alarm_pipe_fds[0]);
  close(alarm_pipe_fds[1]);

  printf(
      "%-24s result=%011d errno=%011d read_ready=%01d alarms=%01d\n",
      "pselect-mask-alarm",
      result,
      result < 0 ? result_errno : 0,
      read_ready,
      (int)masked_alarms);

  if (result != 1 || read_ready != 1 || masked_alarms != 1) {
    fprintf(stderr, "pselect-mask-blocks-alarm mismatch\n");
    return 1;
  }
  return 0;
}

static void* fill_shared_alarm_pipe_later(void* arg) {
  (void)arg;
  usleep(300000);
  if (write(alarm_pipe_fds[1], "x", 1) != 1) {
    perror("fill pipe");
  }
  return NULL;
}

/*
 * Recording must not delay the waiting thread's entry into pselect6 by
 * injecting a probe syscall after the scheduler has committed the call: a
 * SIGALRM due in that window was taken by the waiter under its ordinary mask in
 * some recordings and by the writer in others, and replay refused the
 * recording (review of https://github.com/rrnewton/hermit/pull/3989). Each
 * round repeats the chance.
 */
static int pselect_mask_shared_alarm(void) {
  struct sigaction action;
  memset(&action, 0, sizeof action);
  action.sa_handler = count_masked_alarm;
  if (sigaction(SIGALRM, &action, NULL) != 0) {
    perror("sigaction");
    return 1;
  }
  sigset_t during;
  sigemptyset(&during);
  sigaddset(&during, SIGALRM);
  for (int round = 0; round < 8; round++) {
    masked_alarms = 0;
    if (pipe(alarm_pipe_fds) != 0) {
      perror("pipe");
      return 1;
    }
    struct itimerval alarm_in = {{0, 0}, {0, 100000}};
    if (setitimer(ITIMER_REAL, &alarm_in, NULL) != 0) {
      perror("setitimer");
      return 1;
    }
    pthread_t writer;
    if (pthread_create(&writer, NULL, fill_shared_alarm_pipe_later, NULL) != 0) {
      perror("pthread_create");
      return 1;
    }
    fd_set read_set;
    FD_ZERO(&read_set);
    FD_SET(alarm_pipe_fds[0], &read_set);
    errno = 0;
    int result =
        pselect(alarm_pipe_fds[0] + 1, &read_set, NULL, NULL, NULL, &during);
    int result_errno = errno;
    int read_ready = result > 0 && FD_ISSET(alarm_pipe_fds[0], &read_set);
    if (pthread_join(writer, NULL) != 0) {
      perror("pthread_join");
      return 1;
    }
    close(alarm_pipe_fds[0]);
    close(alarm_pipe_fds[1]);
    printf(
        "%-24s round=%01d result=%011d errno=%011d read_ready=%01d alarms=%01d\n",
        "pselect-mask-shared",
        round,
        result,
        result < 0 ? result_errno : 0,
        read_ready,
        (int)masked_alarms);
    if (result != 1 || read_ready != 1 || masked_alarms != 1) {
      fprintf(stderr, "pselect-mask-shared-alarm mismatch\n");
      return 1;
    }
  }
  return 0;
}

struct pselect_mask_wrapper {
  const void* mask;
  size_t size;
};

static void* fill_alarm_pipe_after_yields(void* arg) {
  (void)arg;
  for (int i = 0; i < 1000; i++) {
    sched_yield();
  }
  usleep(300000);
  if (write(alarm_pipe_fds[1], "x", 1) != 1) {
    perror("fill pipe");
  }
  return NULL;
}

/*
 * Starts the timer and the writer for the raw pselect6 modes below: the writer
 * inherits a mask that blocks SIGALRM, so only the waiter can take the timer's
 * SIGALRM, as in pselect-mask-blocks-alarm.
 */
static int start_alarm_and_writer(pthread_t* writer, void* (*body)(void*)) {
  struct sigaction action;
  memset(&action, 0, sizeof action);
  action.sa_handler = count_masked_alarm;
  if (sigaction(SIGALRM, &action, NULL) != 0 || pipe(alarm_pipe_fds) != 0) {
    perror("setup");
    return 1;
  }
  sigset_t alarm_set;
  sigemptyset(&alarm_set);
  sigaddset(&alarm_set, SIGALRM);
  if (pthread_sigmask(SIG_BLOCK, &alarm_set, NULL) != 0 ||
      pthread_create(writer, NULL, body, NULL) != 0 ||
      pthread_sigmask(SIG_UNBLOCK, &alarm_set, NULL) != 0) {
    perror("writer");
    return 1;
  }
  struct itimerval alarm_in = {{0, 0}, {0, 100000}};
  if (setitimer(ITIMER_REAL, &alarm_in, NULL) != 0) {
    perror("setitimer");
    return 1;
  }
  return 0;
}

static int finish_raw_pselect(
    const char* name,
    pthread_t writer,
    int result,
    int result_errno,
    fd_set* read_set,
    int eintr_allowed) {
  int read_ready = result > 0 && FD_ISSET(alarm_pipe_fds[0], read_set);
  if (pthread_join(writer, NULL) != 0) {
    perror("pthread_join");
    return 1;
  }
  close(alarm_pipe_fds[0]);
  close(alarm_pipe_fds[1]);
  printf(
      "%-24s result=%011d errno=%011d read_ready=%01d alarms=%01d\n",
      name,
      result,
      result < 0 ? result_errno : 0,
      read_ready,
      (int)masked_alarms);
  int ready_result = result == 1 && read_ready == 1;
  int eintr_result = eintr_allowed && result == -1 && result_errno == EINTR;
  if (!(ready_result || eintr_result) || masked_alarms != 1) {
    fprintf(stderr, "%s mismatch\n", name);
    return 1;
  }
  return 0;
}

/*
 * The wrapper sits on a PROT_WRITE page. Recording must not lose the call's
 * temporary mask to a tracer read that the page refuses, which made the
 * scheduler take the ordinary mask and wait for a SIGALRM wake Linux never
 * delivers (Codex review of https://github.com/rrnewton/hermit/pull/3989).
 */
static int pselect_mask_writeonly_wrapper(void) {
  size_t page = (size_t)sysconf(_SC_PAGESIZE);
  void* mapping = mmap(
      NULL, page, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  if (mapping == MAP_FAILED) {
    perror("mmap");
    return 1;
  }
  static uint64_t blocks_alarm;
  blocks_alarm = UINT64_C(1) << (SIGALRM - 1);
  struct pselect_mask_wrapper* wrapper =
      (struct pselect_mask_wrapper*)((char*)mapping + 32);
  wrapper->mask = &blocks_alarm;
  wrapper->size = sizeof blocks_alarm;
  if (mprotect(mapping, page, PROT_WRITE) != 0) {
    perror("mprotect");
    return 1;
  }
  pthread_t writer;
  if (start_alarm_and_writer(&writer, fill_alarm_pipe_after_yields) != 0) {
    return 1;
  }
  fd_set read_set;
  FD_ZERO(&read_set);
  FD_SET(alarm_pipe_fds[0], &read_set);
  errno = 0;
  int result = (int)syscall(
      SYS_pselect6, alarm_pipe_fds[0] + 1, &read_set, NULL, NULL, NULL, wrapper);
  int result_errno = errno;
  int status = finish_raw_pselect(
      "pselect-writeonly", writer, result, result_errno, &read_set, 0);
  munmap(mapping, page);
  return status;
}

static _Atomic uint64_t rewritten_mask;
static _Atomic int about_to_pselect;

static void* rewrite_mask_then_fill_pipe(void* arg) {
  (void)arg;
  while (!atomic_load_explicit(&about_to_pselect, memory_order_acquire)) {
    sched_yield();
  }
  atomic_store_explicit(
      &rewritten_mask, UINT64_C(1) << (SIGALRM - 1), memory_order_release);
  return fill_alarm_pipe_after_yields(NULL);
}

/*
 * The writer sets the mask to block SIGALRM after the waiter announces its
 * pselect6 and before the scheduler runs the call. The scheduler must not
 * record a mask the kernel does not install (Codex review of
 * https://github.com/rrnewton/hermit/pull/3989). Natively the waiter usually
 * enters the call first, under the zero mask, and the SIGALRM ends it with
 * EINTR; both outcomes are Linux's, and either must finish.
 */
static int pselect_mask_sibling_rewrite(void) {
  pthread_t writer;
  if (start_alarm_and_writer(&writer, rewrite_mask_then_fill_pipe) != 0) {
    return 1;
  }
  fd_set read_set;
  FD_ZERO(&read_set);
  FD_SET(alarm_pipe_fds[0], &read_set);
  struct pselect_mask_wrapper wrapper = {
      (const void*)&rewritten_mask, sizeof(uint64_t)};
  errno = 0;
  atomic_store_explicit(&about_to_pselect, 1, memory_order_release);
  int result = (int)syscall(
      SYS_pselect6, alarm_pipe_fds[0] + 1, &read_set, NULL, NULL, NULL, &wrapper);
  int result_errno = errno;
  return finish_raw_pselect(
      "pselect-sibling", writer, result, result_errno, &read_set, 1);
}

/*
 * The waiter keeps SIGALRM blocked outside the call, so a SIGALRM the timer
 * sends before the waiter enters the kernel stays pending until pselect6
 * installs its empty mask, and the call ends with EINTR either way. Nothing
 * else can end the call early: there is no writer, and the 30 s timeout is only
 * a bound. A writer whose real sleep raced the virtual 100 ms timer made the
 * recording return 1 in about 1 run in 12 (Claude re-check of
 * https://github.com/rrnewton/hermit/pull/4052). A broken implementation
 * returns 0 after 30 s and fails the check.
 */
static sigset_t unblocks_during;

/* Adds SIGALRM to the waiter's mask buffer while its pselect6 sleeps. The
 * yields keep the scheduler busy first, as in fill_alarm_pipe_later, so the
 * waiter enters the kernel and copies its mask before the rewrite. There is no
 * sleep: while this thread is runnable the timer cannot fire, so the rewrite
 * lands between the call's entry and the SIGALRM that ends it. */
static void* rewrite_unblocks_mask_later(void* arg) {
  (void)arg;
  for (int i = 0; i < 1000; i++) {
    sched_yield();
  }
  sigaddset(&unblocks_during, SIGALRM);
  return NULL;
}

static int pselect_mask_unblocks_alarm(int rewrite) {
  struct sigaction action;
  memset(&action, 0, sizeof action);
  action.sa_handler = count_masked_alarm;
  if (sigaction(SIGALRM, &action, NULL) != 0 || pipe(alarm_pipe_fds) != 0) {
    perror("setup");
    return 1;
  }
  sigset_t alarm_set;
  sigemptyset(&alarm_set);
  sigaddset(&alarm_set, SIGALRM);
  if (pthread_sigmask(SIG_BLOCK, &alarm_set, NULL) != 0) {
    perror("pthread_sigmask block");
    return 1;
  }
  sigemptyset(&unblocks_during);
  pthread_t rewriter;
  if (rewrite &&
      pthread_create(&rewriter, NULL, rewrite_unblocks_mask_later, NULL) != 0) {
    perror("pthread_create");
    return 1;
  }
  struct itimerval alarm_in = {{0, 0}, {0, 100000}};
  if (setitimer(ITIMER_REAL, &alarm_in, NULL) != 0) {
    perror("setitimer");
    return 1;
  }
  fd_set read_set;
  FD_ZERO(&read_set);
  FD_SET(alarm_pipe_fds[0], &read_set);
  struct timespec bound = {30, 0};
  errno = 0;
  int result = pselect(
      alarm_pipe_fds[0] + 1, &read_set, NULL, NULL, &bound, &unblocks_during);
  int result_errno = errno;
  int alarms_at_return = (int)masked_alarms;
  if (rewrite && pthread_join(rewriter, NULL) != 0) {
    perror("pthread_join");
    return 1;
  }
  close(alarm_pipe_fds[0]);
  close(alarm_pipe_fds[1]);
  printf(
      "%-24s result=%011d errno=%011d alarms=%01d\n",
      "pselect-mask-unblocks",
      result,
      result < 0 ? result_errno : 0,
      alarms_at_return);
  /* Linux copied the mask at entry, so the rewrite, which lands after the
   * waiter entered the kernel, changes nothing: EINTR in both modes. */
  if (result != -1 || result_errno != EINTR || alarms_at_return != 1) {
    fprintf(stderr, "pselect-mask-unblocks-alarm mismatch\n");
    return 1;
  }
  return 0;
}

int main(int argc, char** argv) {
  if (argc != 2) {
    fprintf(
        stderr,
        "usage: %s [raw|glibc|pselect-mask|efault|einval|poll|thread-wake|"
        "pselect-mask-blocks-alarm|pselect-mask-shared-alarm|"
        "pselect-mask-writeonly-wrapper|pselect-mask-sibling-rewrite|"
        "pselect-mask-unblocks-alarm|pselect-mask-unblocks-rewritten]\n",
        argv[0]);
    return 2;
  }
  if (strcmp(argv[1], "raw") == 0) {
    return run_shapes("raw", RAW_SELECT);
  }
  if (strcmp(argv[1], "glibc") == 0) {
    return run_shapes("glibc", GLIBC_SELECT);
  }
  if (strcmp(argv[1], "pselect-mask") == 0) {
    return run_shapes("pselect-mask", GLIBC_PSELECT_MASKED);
  }
  if (strcmp(argv[1], "pselect-mask-blocks-alarm") == 0) {
    return pselect_mask_blocks_alarm();
  }
  if (strcmp(argv[1], "pselect-mask-writeonly-wrapper") == 0) {
    return pselect_mask_writeonly_wrapper();
  }
  if (strcmp(argv[1], "pselect-mask-sibling-rewrite") == 0) {
    return pselect_mask_sibling_rewrite();
  }
  if (strcmp(argv[1], "pselect-mask-shared-alarm") == 0) {
    return pselect_mask_shared_alarm();
  }
  if (strcmp(argv[1], "pselect-mask-unblocks-alarm") == 0) {
    return pselect_mask_unblocks_alarm(0);
  }
  if (strcmp(argv[1], "pselect-mask-unblocks-rewritten") == 0) {
    return pselect_mask_unblocks_alarm(1);
  }
  if (strcmp(argv[1], "efault") == 0) {
    return efault();
  }
  if (strcmp(argv[1], "einval") == 0) {
    return einval();
  }
  if (strcmp(argv[1], "poll") == 0) {
    return poll_control();
  }
  if (strcmp(argv[1], "thread-wake") == 0) {
    return thread_wake();
  }
  fprintf(stderr, "unknown mode: %s\n", argv[1]);
  return 2;
}
