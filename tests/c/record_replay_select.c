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
 *   pselect-mask-unblocks-tight-stack
 *   pselect-mask-unblocks-tight-stack24
 *   pselect-mask-unblocks-tight-stack8
 *                the unblocks wait as a raw pselect6 made on a stack 144 (or 24) bytes
 *                into a writable page with an inaccessible page below it, so
 *                there is no writable memory below the red zone; the handler
 *                runs on an alternate stack. Replay stages the call's mask
 *                where it can on such a stack and must still deliver the
 *                signal, and the 128-byte red zone below the stack pointer,
 *                filled with a pattern before the call, must hold it after
 *                (follow-up to https://github.com/rrnewton/hermit/pull/4052,
 *                and its re-check)
 *   epoll-pwait-ready
 *   epoll-pwait-mask-ready
 *   epoll-pwait-writer
 *                raw epoll_pwait on an epoll set holding a pipe: a ready pipe
 *                with no mask, the same with an empty temporary mask, and an
 *                empty pipe that a second thread fills after 300 ms, with no
 *                mask and a 30 s bound. Replay does not refill pipes,
 *                and epoll_pwait ran live in record and replay alike, so replay
 *                saw an empty pipe (a masked call returned 0 at its timeout; an
 *                unmasked one was refused). Replay now serves it from the
 *                recording, as it does epoll_wait
 *   epoll-pwait-mask-blocks-alarm
 *   epoll-pwait-mask-unblocks-alarm
 *                epoll_pwait with a temporary mask while a 100 ms ITIMER_REAL
 *                runs: a mask that blocks SIGALRM, ended by a writer's byte at
 *                300 ms with 1 and the handler after; and an empty mask while
 *                the thread blocks SIGALRM, ended by the SIGALRM with EINTR
 *                after its handler. A masked epoll_pwait kept its scheduler
 *                turn, so record returned 0 at its 30 s timeout
 *                (https://github.com/rrnewton/hermit/issues/4041)
 *   epoll-pwait-mask-raise-tight
 *   epoll-pwait-mask-raise-tight24
 *                the pending-SIGUSR1 masked epoll_pwait on a stack 128 (or 24)
 *                bytes
 *                into a writable page with an inaccessible page below it, with
 *                the handler on an alternate stack and a pattern in the red
 *                zone: Hermit stages its private mask copy in the red zone and
 *                must restore it (re-review of
 *                https://github.com/rrnewton/hermit/pull/4087)
 *   epoll-pwait-mask-redzone-events
 *                a masked epoll_pwait on a ready pipe on a stack 128 bytes into
 *                its page, with its events buffer in the top 12 bytes of the red
 *                zone: the kernel writes the event there, so Hermit's red-zone
 *                mask copy must stay clear of it
 *   epoll-pwait-mask-pipe-epfd
 *                a masked epoll_pwait whose epoll descriptor is a pipe: EINVAL
 *   epoll-pwait-mask-sibling-race
 *                a masked epoll_pwait (mask buffer blocking SIGUSR1, 3 s timeout)
 *                whose buffer a sibling zeroes as soon as it next runs after the
 *                waiter announces the call; the sibling then sends SIGUSR1 at
 *                100 ms and writes the pipe at 300 ms. Linux allows either
 *                outcome; replay must reproduce the recording's, which needs
 *                the call to sleep under a private copy of the mask it read
 *                (re-review of https://github.com/rrnewton/hermit/pull/4087)
 *   epoll-pwait2-ready
 *   epoll-pwait2-writer
 *   epoll-pwait2-mask-raise
 *                the same shapes as raw epoll_pwait2, which takes a timespec
 *                timeout (2 s): a ready pipe, a pipe a thread fills at 300 ms,
 *                and an empty mask with SIGUSR1 pending at entry (EINTR,
 *                handler run). epoll_pwait2 ran live in record and replay alike
 *   epoll-pwait2-mask-raise-tight, epoll-pwait2-mask-raise-tight24
 *                epoll-pwait-mask-raise-tight and -tight24 through
 *                epoll_pwait2: its private mask copy must go in the red zone,
 *                clear of the events buffer and the timespec
 *   epoll-pwait2-mask-sibling-race
 *                epoll-pwait-mask-sibling-race through epoll_pwait2 (3 s
 *                timespec): the call must sleep under a private copy of the
 *                mask it read
 *   epoll-pwait2-timeout-in-red-zone
 *                a masked epoll_pwait2 48 bytes into its page whose 30 s
 *                timespec sits at the top of the red zone, ended by a writer
 *                at 300 ms: 1, so the mask copy must stay clear of the
 *                timespec the kernel reads
 *   epoll-pwait2-zero-beside-busy-sibling
 *                a masked zero-timespec epoll_pwait2 on a ready pipe while a
 *                sibling loops without a system call: 1. A call with a zero
 *                timeout keeps its turn; run in the background, it would wait
 *                behind the sibling forever
 *   epoll-pwait2-error-order
 *                an unreadable timespec with sigsetsize 4 (EFAULT), and an
 *                invalid tv_nsec with an unreadable mask (EINVAL): Linux checks
 *                the timespec before the mask
 *   epoll-pwait-mask-raise
 *                the thread catches and blocks SIGUSR1, raises it, then waits in
 *                epoll_pwait with a 30 s timeout and an empty temporary mask:
 *                the signal is pending when the call starts, so Linux ends it
 *                with EINTR and runs the handler. Replay must deliver the
 *                signal under that mask (review of
 *                https://github.com/rrnewton/hermit/pull/4087)
 *   pselect-mask-zero-raise
 *   ppoll-mask-zero-raise
 *                the thread catches and blocks SIGUSR1, raises it, then calls
 *                glibc pselect() or ppoll() with a zero timeout and an empty
 *                temporary mask. Linux installs the mask even for a zero
 *                timeout, finds SIGUSR1 pending, and ends the call with EINTR
 *                after the handler runs. Replay serves the call from the
 *                recording and must deliver the signal under that mask
 *                (https://github.com/rrnewton/hermit/issues/4054)
 *   pselect-mask-zero-race
 *                300 rounds of raise(SIGUSR1) and a raw zero-timeout pselect6
 *                whose mask buffer a sibling toggles between blocking SIGUSR1
 *                and empty. Linux copies the mask when the call enters the
 *                kernel, so each round returns EINTR or 0; replay must get the
 *                same sequence, which it did not when it read the buffer before
 *                the call's scheduler turn (review of
 *                https://github.com/rrnewton/hermit/pull/4084)
 *   pselect-beside-a-yielding-sibling
 *                ten raw pselect6 calls with a 1 ms timeout while a sibling
 *                loops on sched_yield: the even ones have no mask and return 0,
 *                the odd ones raise SIGUSR1, which the thread blocks, and wait
 *                under an empty mask: EINTR with the handler run. Under record
 *                the first finished call was never readmitted while the
 *                sibling stayed queued (https://github.com/rrnewton/hermit/issues/4092)
 *   pselect-beside-two-yielding-siblings
 *                the same with two spinning siblings, which keep replacing
 *                each other's one-turn yield exclusion
 *   pselect-mask-hook-race
 *                a raw pselect6 with a 1 s timeout and a mask buffer holding
 *                SIGALRM, which the thread blocks, while a 100 ms ITIMER_REAL
 *                runs; a sibling clears the buffer once, just after the main
 *                thread announces its call, while the call's scheduler request
 *                waits. The kernel copies the mask when the call enters it: an
 *                empty mask gives EINTR with the handler run, the old one 0 at
 *                the timeout. Replay must read the mask where recording's
 *                kernel did, after the grant, not at the call's entry (review
 *                of https://github.com/rrnewton/hermit/pull/4084)
 *   ppoll-mask-zero-raise-tight
 *                the ppoll zero-timeout raise on a stack 160 bytes into a
 *                writable page with an inaccessible page below it, with the
 *                handler on an alternate stack and a pattern in the red zone
 *                that must survive the replayed delivery
 *   ppoll-mask-zero-raise-tight24, ppoll-mask-raise-tight24,
 *   ppoll-mask-raise-tight128
 *                the same at 24 and 128 bytes, with a zero timeout or a 30 s
 *                one: no stack scratch below the red zone, so Hermit stages
 *                its private zero timeout and mask copy in the red zone
 *                (https://github.com/rrnewton/hermit/issues/4113)
 *   ppoll-zero-tight16
 *                an unmasked zero-timeout ppoll at 16 bytes: 0
 *   ppoll-mask-redzone-pollfd
 *                a masked ppoll at 128 bytes on a ready pipe whose pollfd sits
 *                at the top of the red zone, where the kernel writes revents:
 *                1 with POLLIN, so the private copy must go below it
 *   ppoll-mask-timeout-in-red-zone
 *                a masked ppoll at 48 bytes whose 30 s timeout sits at the top
 *                of the red zone, ended by a writer at 300 ms: 1, POLLIN, and
 *                29 s left written back, so the private mask copy must stay
 *                clear of the timeout
 *   ppoll-mask-raise-tight8
 *                at 8 bytes no 24-byte window is writable; a fail-closed run
 *                refuses the call by name
 *   ppoll-mask-blocks-alarm
 *                the blocks-alarm wait as glibc ppoll() with no timeout and a
 *                temporary mask that blocks SIGALRM: the writer's byte at
 *                300 ms returns 1 and the handler runs after the call
 *   ppoll-mask-unblocks-alarm
 *                the unblocks-alarm wait as glibc ppoll() with a 30 s bound
 *                and an empty temporary mask: the 100 ms SIGALRM ends it with
 *                EINTR after its handler runs. Under sequentialization a
 *                masked ppoll that would block recorded ENOSYS; record/replay
 *                now lets the kernel own the wait
 *                (https://github.com/rrnewton/hermit/issues/4042)
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
#include <sys/epoll.h>
#include <sys/mman.h>
#include <sys/select.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <time.h>
#include <unistd.h>

#ifndef SYS_epoll_pwait2
#define SYS_epoll_pwait2 441
#endif

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

/* No instruction between the stack switch and its restoration touches the
 * stack. SysV inputs: new_sp, nfds, readfds, timeout, wrapper. Preserves the
 * caller's r12 on the original stack and keeps the original rsp in r12 across
 * the syscall. */
extern long pselect_on_stack(void*, long, fd_set*, struct timespec*, void*);
__asm__(
    ".text\n"
    ".globl pselect_on_stack\n"
    ".type pselect_on_stack,@function\n"
    "pselect_on_stack:\n"
    "push %r12\n"
    "mov %rsp,%r12\n"
    "mov %rdi,%r11\n"
    "mov %rsi,%rdi\n"
    "mov %rdx,%rsi\n"
    "mov %rcx,%rax\n"
    "xor %edx,%edx\n"
    "xor %r10d,%r10d\n"
    "mov %r8,%r9\n"
    "mov %rax,%r8\n"
    "mov %r11,%rsp\n"
    "mov $270,%eax\n"
    "syscall\n"
    "mov %r12,%rsp\n"
    "pop %r12\n"
    "ret\n"
    ".size pselect_on_stack,.-pselect_on_stack\n");

static unsigned char alternate_stack[65536];

static int pselect_mask_unblocks_tight_stack(long offset) {
  long page = sysconf(_SC_PAGESIZE);
  if (page <= 144) {
    return 1;
  }
  unsigned char* area = mmap(
      NULL, (size_t)page * 2, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  if (area == MAP_FAILED ||
      mprotect(area + page, (size_t)page, PROT_READ | PROT_WRITE) != 0) {
    perror("tight stack");
    return 1;
  }
  stack_t alternate = {
      .ss_sp = alternate_stack, .ss_size = sizeof alternate_stack, .ss_flags = 0};
  struct sigaction action;
  memset(&action, 0, sizeof action);
  action.sa_handler = count_masked_alarm;
  action.sa_flags = SA_ONSTACK;
  if (sigaltstack(&alternate, NULL) != 0 ||
      sigaction(SIGALRM, &action, NULL) != 0 || pipe(alarm_pipe_fds) != 0) {
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
  static uint64_t empty_mask;
  static struct pselect_mask_wrapper wrapper = {
      (const void*)&empty_mask, sizeof(uint64_t)};
  struct itimerval alarm_in = {{0, 0}, {0, 100000}};
  if (setitimer(ITIMER_REAL, &alarm_in, NULL) != 0) {
    perror("setitimer");
    return 1;
  }
  static fd_set read_set;
  FD_ZERO(&read_set);
  FD_SET(alarm_pipe_fds[0], &read_set);
  static struct timespec bound = {30, 0};
  unsigned char* tight_stack = area + page + offset;
  int low = offset < 128 ? (int)-offset : -128;
  for (int i = low; i < 0; i++) {
    tight_stack[i] = (unsigned char)(0xa5 ^ (i & 0xff));
  }
  long raw = pselect_on_stack(
      tight_stack, alarm_pipe_fds[0] + 1, &read_set, &bound, &wrapper);
  int alarms_at_return = (int)masked_alarms;
  int red_zone_changed = 0;
  for (int i = low; i < 0; i++) {
    red_zone_changed += tight_stack[i] != (unsigned char)(0xa5 ^ (i & 0xff));
  }
  close(alarm_pipe_fds[0]);
  close(alarm_pipe_fds[1]);
  printf(
      "%-24s result=%011ld alarms=%01d red_zone_changed=%03d\n",
      "pselect-tight-stack",
      raw,
      alarms_at_return,
      red_zone_changed);
  if (raw != -EINTR || alarms_at_return != 1 || red_zone_changed != 0) {
    fprintf(stderr, "pselect-mask-unblocks-tight-stack mismatch\n");
    return 1;
  }
  return 0;
}

/* A masked ppoll that waits for the timer's SIGALRM. With `blocks`, the
 * temporary mask blocks SIGALRM and a writer fills the pipe at 300 ms; without
 * it, the thread blocks SIGALRM itself and the empty temporary mask lets the
 * 100 ms SIGALRM end the call. */
static int ppoll_mask_alarm(int blocks) {
  struct sigaction action;
  memset(&action, 0, sizeof action);
  action.sa_handler = count_masked_alarm;
  sigset_t alarm_set, during;
  sigemptyset(&alarm_set);
  sigaddset(&alarm_set, SIGALRM);
  if (blocks) {
    during = alarm_set;
  } else {
    sigemptyset(&during);
  }
  if (sigaction(SIGALRM, &action, NULL) != 0 || pipe(alarm_pipe_fds) != 0 ||
      pthread_sigmask(SIG_BLOCK, &alarm_set, NULL) != 0) {
    perror("setup");
    return 1;
  }
  pthread_t writer;
  if (blocks) {
    if (pthread_create(&writer, NULL, fill_alarm_pipe_later, NULL) != 0 ||
        pthread_sigmask(SIG_UNBLOCK, &alarm_set, NULL) != 0) {
      perror("writer");
      return 1;
    }
  }
  struct itimerval alarm_in = {{0, 0}, {0, 100000}};
  if (setitimer(ITIMER_REAL, &alarm_in, NULL) != 0) {
    perror("setitimer");
    return 1;
  }
  struct pollfd watched = {alarm_pipe_fds[0], POLLIN, 0};
  struct timespec bound = {30, 0};
  errno = 0;
  int result = ppoll(&watched, 1, blocks ? NULL : &bound, &during);
  int result_errno = errno;
  int read_ready = result > 0 && (watched.revents & POLLIN);
  if (blocks && pthread_join(writer, NULL) != 0) {
    perror("pthread_join");
    return 1;
  }
  int alarms = (int)masked_alarms;
  close(alarm_pipe_fds[0]);
  close(alarm_pipe_fds[1]);
  printf(
      "%-24s result=%011d errno=%011d read_ready=%01d alarms=%01d\n",
      blocks ? "ppoll-mask-blocks" : "ppoll-mask-unblocks",
      result,
      result < 0 ? result_errno : 0,
      read_ready,
      alarms);
  int expected = blocks ? result == 1 && read_ready == 1 && alarms == 1
                        : result == -1 && result_errno == EINTR && alarms == 1;
  if (!expected) {
    fprintf(stderr, "ppoll-mask-%s-alarm mismatch\n", blocks ? "blocks" : "unblocks");
    return 1;
  }
  return 0;
}

static volatile uint64_t hook_mask = UINT64_C(1) << (SIGALRM - 1);
static struct pselect_mask_wrapper hook_wrapper = {(const void*)&hook_mask, 8};
static atomic_int hook_announced;

static void* clear_hook_mask(void* arg) {
  (void)arg;
  while (!atomic_load(&hook_announced)) {
    sched_yield();
  }
  hook_mask = 0;
  return NULL;
}

static int pselect_mask_hook_race(void) {
  struct sigaction action;
  memset(&action, 0, sizeof action);
  action.sa_handler = count_masked_alarm;
  sigset_t alarm_set;
  sigemptyset(&alarm_set);
  sigaddset(&alarm_set, SIGALRM);
  if (sigaction(SIGALRM, &action, NULL) != 0 ||
      pthread_sigmask(SIG_BLOCK, &alarm_set, NULL) != 0) {
    perror("setup");
    return 1;
  }
  pthread_t clearer;
  if (pthread_create(&clearer, NULL, clear_hook_mask, NULL) != 0) {
    perror("pthread_create");
    return 1;
  }
  struct itimerval alarm_in = {{0, 0}, {0, 100000}};
  if (setitimer(ITIMER_REAL, &alarm_in, NULL) != 0) {
    perror("setitimer");
    return 1;
  }
  struct timespec bound = {1, 0};
  atomic_store(&hook_announced, 1);
  errno = 0;
  long result = syscall(SYS_pselect6, 0, NULL, NULL, NULL, &bound, &hook_wrapper);
  int result_errno = errno;
  int handled = (int)masked_alarms;
  if (pthread_join(clearer, NULL) != 0) {
    perror("pthread_join");
    return 1;
  }
  printf(
      "%-24s result=%011ld errno=%011d alarms=%01d\n",
      "pselect-hook-race",
      result,
      result < 0 ? result_errno : 0,
      handled);
  int interrupted = result == -1 && result_errno == EINTR && handled == 1;
  int timed_out = result == 0 && handled == 0;
  if (!interrupted && !timed_out) {
    fprintf(stderr, "pselect-mask-hook-race mismatch\n");
    return 1;
  }
  return 0;
}

/* Six-argument syscall on a switched stack: `args` holds the five arguments.
 * No instruction between the stack switch and its restoration touches the
 * stack. */
extern long syscall5_on_stack(void* new_sp, long number, const long* args);
__asm__(
    ".text\n"
    ".globl syscall5_on_stack\n"
    ".type syscall5_on_stack,@function\n"
    "syscall5_on_stack:\n"
    "push %r12\n"
    "mov %rsp,%r12\n"
    "mov %rsi,%rax\n"
    "mov %rdi,%rcx\n"
    "mov %rdx,%r11\n"
    "mov 0(%r11),%rdi\n"
    "mov 8(%r11),%rsi\n"
    "mov 16(%r11),%rdx\n"
    "mov 24(%r11),%r10\n"
    "mov 32(%r11),%r8\n"
    "mov %rcx,%rsp\n"
    "syscall\n"
    "mov %r12,%rsp\n"
    "pop %r12\n"
    "ret\n"
    ".size syscall5_on_stack,.-syscall5_on_stack\n");

/* syscall5_on_stack with a sixth argument, args[5], in r9. */
extern long syscall6_on_stack(void* new_sp, long number, const long* args);
__asm__(
    ".text\n"
    ".globl syscall6_on_stack\n"
    ".type syscall6_on_stack,@function\n"
    "syscall6_on_stack:\n"
    "push %r12\n"
    "mov %rsp,%r12\n"
    "mov %rsi,%rax\n"
    "mov %rdi,%rcx\n"
    "mov %rdx,%r11\n"
    "mov 0(%r11),%rdi\n"
    "mov 8(%r11),%rsi\n"
    "mov 16(%r11),%rdx\n"
    "mov 24(%r11),%r10\n"
    "mov 32(%r11),%r8\n"
    "mov 40(%r11),%r9\n"
    "mov %rcx,%rsp\n"
    "syscall\n"
    "mov %r12,%rsp\n"
    "pop %r12\n"
    "ret\n"
    ".size syscall6_on_stack,.-syscall6_on_stack\n");

static volatile uint64_t race_mask;
static struct pselect_mask_wrapper race_wrapper = {(const void*)&race_mask, 8};
static atomic_int race_done;
static volatile sig_atomic_t race_handled;

static void count_race_usr1(int signo) {
  (void)signo;
  race_handled++;
}

static void* toggle_race_mask(void* arg) {
  (void)arg;
  while (!atomic_load(&race_done)) {
    race_mask = 0;
    sched_yield();
    race_mask = UINT64_C(1) << (SIGUSR1 - 1);
    sched_yield();
  }
  return NULL;
}

static int pselect_mask_zero_race(void) {
  struct sigaction action;
  memset(&action, 0, sizeof action);
  action.sa_handler = count_race_usr1;
  sigset_t usr1;
  sigemptyset(&usr1);
  sigaddset(&usr1, SIGUSR1);
  if (sigaction(SIGUSR1, &action, NULL) != 0 ||
      pthread_sigmask(SIG_BLOCK, &usr1, NULL) != 0) {
    perror("setup");
    return 1;
  }
  race_mask = UINT64_C(1) << (SIGUSR1 - 1);
  pthread_t toggler;
  if (pthread_create(&toggler, NULL, toggle_race_mask, NULL) != 0) {
    perror("pthread_create");
    return 1;
  }
  struct timespec zero = {0, 0};
  int eintr = 0, zeros = 0, other = 0;
  unsigned long long trace = 0;
  for (int i = 0; i < 300; i++) {
    raise(SIGUSR1);
    long result = syscall(SYS_pselect6, 0, NULL, NULL, NULL, &zero, &race_wrapper);
    if (result == -1 && errno == EINTR) {
      eintr++;
      trace = trace * 3 + 1;
    } else if (result == 0) {
      zeros++;
      trace = trace * 3 + 2;
    } else {
      other++;
      trace = trace * 3;
    }
  }
  atomic_store(&race_done, 1);
  if (pthread_join(toggler, NULL) != 0) {
    perror("pthread_join");
    return 1;
  }
  /* Each SIGUSR1 left pending by a round that returned 0 is taken by a later
   * round that returns EINTR, or stays pending at the end. */
  int handled = (int)race_handled;
  printf(
      "%-24s eintr=%03d zero=%03d other=%03d handled=%03d trace=%016llx\n",
      "pselect-zero-race",
      eintr,
      zeros,
      other,
      handled,
      trace);
  if (other != 0 || eintr + zeros != 300 || handled != eintr) {
    fprintf(stderr, "pselect-mask-zero-race mismatch\n");
    return 1;
  }
  return 0;
}

static volatile sig_atomic_t raised_usr1;

static void count_raised_usr1(int signo) {
  (void)signo;
  raised_usr1++;
}

/* The zero-timeout idiom: SIGUSR1 is pending and blocked when the call starts,
 * and only the call's empty temporary mask unblocks it. */
static int zero_timeout_mask_raise(int use_ppoll) {
  struct sigaction action;
  memset(&action, 0, sizeof action);
  action.sa_handler = count_raised_usr1;
  sigset_t usr1, empty;
  sigemptyset(&usr1);
  sigaddset(&usr1, SIGUSR1);
  sigemptyset(&empty);
  int fds[2];
  if (sigaction(SIGUSR1, &action, NULL) != 0 ||
      pthread_sigmask(SIG_BLOCK, &usr1, NULL) != 0 || pipe(fds) != 0 ||
      raise(SIGUSR1) != 0) {
    perror("setup");
    return 1;
  }
  struct timespec zero = {0, 0};
  errno = 0;
  int result;
  if (use_ppoll) {
    struct pollfd watched = {fds[0], POLLIN, 0};
    result = ppoll(&watched, 1, &zero, &empty);
  } else {
    fd_set read_set;
    FD_ZERO(&read_set);
    FD_SET(fds[0], &read_set);
    result = pselect(fds[0] + 1, &read_set, NULL, NULL, &zero, &empty);
  }
  int result_errno = errno;
  int handled = (int)raised_usr1;
  close(fds[0]);
  close(fds[1]);
  printf(
      "%-24s result=%011d errno=%011d handled=%01d\n",
      use_ppoll ? "ppoll-zero-raise" : "pselect-zero-raise",
      result,
      result < 0 ? result_errno : 0,
      handled);
  if (result != -1 || result_errno != EINTR || handled != 1) {
    fprintf(stderr, "zero-timeout masked raise mismatch\n");
    return 1;
  }
  return 0;
}

static unsigned char tight_alternate[65536];

/* A ppoll on a stack `offset` bytes into a writable page with an inaccessible
 * page below it. With `masked`, SIGUSR1 is pending, blocked by the thread and
 * unblocked by the call's empty mask, so Linux returns EINTR with the handler
 * run (on an alternate stack); without, the call returns 0. `wait` gives a
 * 30 s timeout, a zero one otherwise. With `ready_pollfd_in_red_zone` no
 * signal is raised, a byte is waiting in the pipe, and the pollfd sits at the
 * top of the red zone, where the kernel writes its revents: Linux returns 1
 * with POLLIN. The pattern in the rest of the writable red zone must survive. */
static int ppoll_tight(
    const char* name,
    long offset,
    int masked,
    int wait,
    int ready_pollfd_in_red_zone) {
  long page = sysconf(_SC_PAGESIZE);
  unsigned char* area = mmap(
      NULL, (size_t)page * 2, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  if (area == MAP_FAILED ||
      mprotect(area + page, (size_t)page, PROT_READ | PROT_WRITE) != 0) {
    perror("tight stack");
    return 1;
  }
  stack_t alternate = {
      .ss_sp = tight_alternate, .ss_size = sizeof tight_alternate, .ss_flags = 0};
  struct sigaction action;
  memset(&action, 0, sizeof action);
  action.sa_handler = count_raised_usr1;
  action.sa_flags = SA_ONSTACK;
  sigset_t usr1;
  sigemptyset(&usr1);
  sigaddset(&usr1, SIGUSR1);
  int fds[2];
  if (sigaltstack(&alternate, NULL) != 0 ||
      sigaction(SIGUSR1, &action, NULL) != 0 ||
      pthread_sigmask(SIG_BLOCK, &usr1, NULL) != 0 || pipe(fds) != 0) {
    perror("setup");
    return 1;
  }
  if (ready_pollfd_in_red_zone ? write(fds[1], "x", 1) != 1 : raise(SIGUSR1) != 0) {
    perror("write or raise");
    return 1;
  }
  raised_usr1 = 0;
  unsigned char* tight_stack = area + page + offset;
  static struct pollfd static_watched;
  struct pollfd* watched = ready_pollfd_in_red_zone
      ? (struct pollfd*)(tight_stack - sizeof(struct pollfd))
      : &static_watched;
  int low = offset < 128 ? (int)-offset : -128;
  int high = ready_pollfd_in_red_zone ? -(int)sizeof(struct pollfd) : 0;
  for (int i = low; i < high; i++) {
    tight_stack[i] = (unsigned char)(0xa5 ^ (i & 0xff));
  }
  watched->fd = fds[0];
  watched->events = POLLIN;
  watched->revents = 0;
  static struct timespec timeout;
  timeout.tv_sec = wait ? 30 : 0;
  timeout.tv_nsec = 0;
  static uint64_t empty_mask;
  const long args[5] = {
      (long)watched, 1, (long)&timeout, masked ? (long)&empty_mask : 0, 8};
  long raw = syscall5_on_stack(tight_stack, SYS_ppoll, args);
  int handled = (int)raised_usr1;
  int revents = watched->revents;
  int red_zone_changed = 0;
  for (int i = low; i < high; i++) {
    red_zone_changed += tight_stack[i] != (unsigned char)(0xa5 ^ (i & 0xff));
  }
  close(fds[0]);
  close(fds[1]);
  printf(
      "%-30s result=%011ld handled=%01d revents=%02x red_zone_changed=%03d\n",
      name,
      raw,
      handled,
      revents,
      red_zone_changed);
  long expected = ready_pollfd_in_red_zone ? 1 : masked ? -EINTR : 0;
  if (raw != expected || handled != (masked && !ready_pollfd_in_red_zone) ||
      revents != (ready_pollfd_in_red_zone ? POLLIN : 0) || red_zone_changed != 0) {
    fprintf(stderr, "%s mismatch\n", name);
    return 1;
  }
  return 0;
}

static int timeout_pipe_fds[2];

static void* fill_timeout_pipe_later(void* arg) {
  (void)arg;
  usleep(300000);
  if (write(timeout_pipe_fds[1], "x", 1) != 1) {
    perror("fill pipe");
  }
  return NULL;
}

/* A masked ppoll with a 30 s timeout on a stack 48 bytes into a writable page
 * with an inaccessible page below it, its timeout in the top 16 bytes of the
 * red zone, and a writer that fills the pipe after 300 ms. Linux returns 1
 * with POLLIN and writes the time left (29 s and change) into the timeout;
 * the pattern in the rest of the red zone must survive. Hermit's private mask
 * copy must stay clear of the timeout, which the kernel reads at entry and
 * writes when the call returns (Claude review of
 * https://github.com/rrnewton/hermit/pull/4127). */
static int ppoll_timeout_in_red_zone(void) {
  long page = sysconf(_SC_PAGESIZE);
  unsigned char* area = mmap(
      NULL, (size_t)page * 2, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  if (area == MAP_FAILED ||
      mprotect(area + page, (size_t)page, PROT_READ | PROT_WRITE) != 0 ||
      pipe(timeout_pipe_fds) != 0) {
    perror("setup");
    return 1;
  }
  unsigned char* tight_stack = area + page + 48;
  struct timespec* timeout = (struct timespec*)(tight_stack - sizeof(struct timespec));
  for (int i = -48; i < -(int)sizeof(struct timespec); i++) {
    tight_stack[i] = (unsigned char)(0xa5 ^ (i & 0xff));
  }
  timeout->tv_sec = 30;
  timeout->tv_nsec = 0;
  static struct pollfd watched;
  watched.fd = timeout_pipe_fds[0];
  watched.events = POLLIN;
  watched.revents = 0;
  static uint64_t empty_mask;
  pthread_t writer;
  if (pthread_create(&writer, NULL, fill_timeout_pipe_later, NULL) != 0) {
    perror("pthread_create");
    return 1;
  }
  const long args[5] = {(long)&watched, 1, (long)timeout, (long)&empty_mask, 8};
  long raw = syscall5_on_stack(tight_stack, SYS_ppoll, args);
  long left_sec = timeout->tv_sec;
  int red_zone_changed = 0;
  for (int i = -48; i < -(int)sizeof(struct timespec); i++) {
    red_zone_changed += tight_stack[i] != (unsigned char)(0xa5 ^ (i & 0xff));
  }
  pthread_join(writer, NULL);
  close(timeout_pipe_fds[0]);
  close(timeout_pipe_fds[1]);
  printf(
      "%-30s result=%011ld revents=%02x time_left=%01d red_zone_changed=%03d\n",
      "ppoll-timeout-in-red-zone",
      raw,
      watched.revents,
      left_sec >= 20 && left_sec <= 29,
      red_zone_changed);
  /* Any time left below the 30 s asked for, short of a stalled writer. */
  if (raw != 1 || watched.revents != POLLIN || left_sec < 20 || left_sec > 29 ||
      red_zone_changed != 0) {
    fprintf(stderr, "ppoll-mask-timeout-in-red-zone mismatch\n");
    return 1;
  }
  return 0;
}

static int epoll_pipe_fds[2];

static void* fill_epoll_pipe_later(void* arg) {
  (void)arg;
  usleep(300000);
  if (write(epoll_pipe_fds[1], "x", 1) != 1) {
    perror("fill pipe");
  }
  return NULL;
}

/* epoll_pwait on a pipe. `masked` passes an empty temporary mask; `writer`
 * leaves the pipe empty for a second thread to fill. */
static int epoll_pwait_mode(const char* name, int masked, int writer) {
  int epoll_fd = epoll_create1(EPOLL_CLOEXEC);
  if (epoll_fd < 0 || pipe(epoll_pipe_fds) != 0) {
    perror("setup");
    return 1;
  }
  struct epoll_event watched = {.events = EPOLLIN, .data.u64 = 0x5eedULL};
  if (epoll_ctl(epoll_fd, EPOLL_CTL_ADD, epoll_pipe_fds[0], &watched) != 0) {
    perror("epoll_ctl");
    return 1;
  }
  pthread_t filler;
  if (writer) {
    if (pthread_create(&filler, NULL, fill_epoll_pipe_later, NULL) != 0) {
      perror("pthread_create");
      return 1;
    }
  } else if (write(epoll_pipe_fds[1], "x", 1) != 1) {
    perror("write");
    return 1;
  }
  sigset_t empty;
  sigemptyset(&empty);
  struct epoll_event ready;
  memset(&ready, 0, sizeof ready);
  errno = 0;
  int result = epoll_pwait(epoll_fd, &ready, 1, 30000, masked ? &empty : NULL);
  int result_errno = errno;
  if (writer && pthread_join(filler, NULL) != 0) {
    perror("pthread_join");
    return 1;
  }
  printf(
      "%-24s result=%011d errno=%011d events=%08x data=%016llx\n",
      name,
      result,
      result < 0 ? result_errno : 0,
      result > 0 ? ready.events : 0,
      result > 0 ? (unsigned long long)ready.data.u64 : 0ULL);
  close(epoll_pipe_fds[0]);
  close(epoll_pipe_fds[1]);
  close(epoll_fd);
  if (result != 1 || !(ready.events & EPOLLIN) || ready.data.u64 != 0x5eedULL) {
    fprintf(stderr, "%s mismatch\n", name);
    return 1;
  }
  return 0;
}

static volatile sig_atomic_t epoll_raised;

static void count_epoll_raised(int signo) {
  (void)signo;
  epoll_raised++;
}

/* epoll_pwait(epfd, events, 1, timeout, mask, 8) on a switched stack. SysV:
 * new_sp, epfd, events, timeout, mask. No instruction between the stack switch
 * and its restoration touches the stack. */
extern long epoll_pwait_on_stack(void* new_sp, long epfd, void* events, long timeout,
                                 void* mask);
__asm__(
    ".text\n"
    ".globl epoll_pwait_on_stack\n"
    ".type epoll_pwait_on_stack,@function\n"
    "epoll_pwait_on_stack:\n"
    "push %r12\n"
    "mov %rsp,%r12\n"
    "mov %rdi,%r11\n"
    "mov %rsi,%rdi\n"
    "mov %rdx,%rsi\n"
    "mov %rcx,%r10\n"
    "mov $1,%edx\n"
    "mov $8,%r9d\n"
    "mov %r11,%rsp\n"
    "mov $281,%eax\n"
    "syscall\n"
    "mov %r12,%rsp\n"
    "pop %r12\n"
    "ret\n"
    ".size epoll_pwait_on_stack,.-epoll_pwait_on_stack\n");

static unsigned char epoll_alternate[65536];

/* With `pwait2`, the same wait through epoll_pwait2 with a 30 s timespec. */
static int epoll_pwait_mask_raise_tight(long offset, int pwait2) {
  long page = sysconf(_SC_PAGESIZE);
  unsigned char* area = mmap(
      NULL, (size_t)page * 2, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  if (area == MAP_FAILED ||
      mprotect(area + page, (size_t)page, PROT_READ | PROT_WRITE) != 0) {
    perror("tight stack");
    return 1;
  }
  stack_t alternate = {
      .ss_sp = epoll_alternate, .ss_size = sizeof epoll_alternate, .ss_flags = 0};
  struct sigaction action;
  memset(&action, 0, sizeof action);
  action.sa_handler = count_epoll_raised;
  action.sa_flags = SA_ONSTACK;
  sigset_t usr1;
  sigemptyset(&usr1);
  sigaddset(&usr1, SIGUSR1);
  int epoll_fd = epoll_create1(EPOLL_CLOEXEC);
  int fds[2];
  if (sigaltstack(&alternate, NULL) != 0 || sigaction(SIGUSR1, &action, NULL) != 0 ||
      epoll_fd < 0 || pipe(fds) != 0 || pthread_sigmask(SIG_BLOCK, &usr1, NULL) != 0) {
    perror("setup");
    return 1;
  }
  struct epoll_event watched = {.events = EPOLLIN, .data.u64 = 0x5eedULL};
  if (epoll_ctl(epoll_fd, EPOLL_CTL_ADD, fds[0], &watched) != 0 || raise(SIGUSR1) != 0) {
    perror("epoll_ctl or raise");
    return 1;
  }
  static struct epoll_event ready;
  static uint64_t empty_mask;
  /* At 128 bytes the 8-byte copy cannot go below the red zone (rsp - 136);
   * at 24 only the top 24 red-zone bytes are writable. */
  unsigned char* tight_stack = area + page + offset;
  int low = offset < 128 ? (int)-offset : -128;
  for (int i = low; i < 0; i++) {
    tight_stack[i] = (unsigned char)(0xa5 ^ (i & 0xff));
  }
  static struct timespec bound = {30, 0};
  const long args[6] = {
      epoll_fd, (long)&ready, 1, (long)&bound, (long)&empty_mask, 8};
  long raw = pwait2 ? syscall6_on_stack(tight_stack, SYS_epoll_pwait2, args)
                    : epoll_pwait_on_stack(tight_stack, epoll_fd, &ready, 30000, &empty_mask);
  int handled = (int)epoll_raised;
  int red_zone_changed = 0;
  for (int i = low; i < 0; i++) {
    red_zone_changed += tight_stack[i] != (unsigned char)(0xa5 ^ (i & 0xff));
  }
  close(fds[0]);
  close(fds[1]);
  close(epoll_fd);
  printf(
      "%-24s offset=%03ld result=%011ld handled=%01d red_zone_changed=%03d\n",
      pwait2 ? "epoll-pwait2-raise-tight" : "epoll-pwait-raise-tight",
      offset,
      raw,
      handled,
      red_zone_changed);
  if (raw != -EINTR || handled != 1 || red_zone_changed != 0) {
    fprintf(stderr, "epoll-pwait-mask-raise-tight mismatch\n");
    return 1;
  }
  return 0;
}

/* A masked epoll_pwait on a ready pipe with its events buffer in the red zone
 * of a stack 128 bytes into its page. */
static int epoll_pwait_mask_redzone_events(void) {
  long page = sysconf(_SC_PAGESIZE);
  unsigned char* area = mmap(
      NULL, (size_t)page * 2, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  if (area == MAP_FAILED ||
      mprotect(area + page, (size_t)page, PROT_READ | PROT_WRITE) != 0) {
    perror("tight stack");
    return 1;
  }
  int epoll_fd = epoll_create1(EPOLL_CLOEXEC);
  int fds[2];
  if (epoll_fd < 0 || pipe(fds) != 0) {
    perror("setup");
    return 1;
  }
  struct epoll_event watched = {.events = EPOLLIN, .data.u64 = 0x5eedULL};
  if (epoll_ctl(epoll_fd, EPOLL_CTL_ADD, fds[0], &watched) != 0 ||
      write(fds[1], "x", 1) != 1) {
    perror("epoll_ctl or write");
    return 1;
  }
  static uint64_t empty_mask;
  unsigned char* tight_stack = area + page + 128;
  struct epoll_event* events =
      (struct epoll_event*)(void*)(tight_stack - (long)sizeof(struct epoll_event));
  memset(events, 0, sizeof(*events));
  long raw = epoll_pwait_on_stack(tight_stack, epoll_fd, events, 30000, &empty_mask);
  int intact = raw == 1 && (events->events & EPOLLIN) && events->data.u64 == 0x5eedULL;
  close(fds[0]);
  close(fds[1]);
  close(epoll_fd);
  printf(
      "%-24s result=%011ld events=%08x data=%016llx\n",
      "epoll-pwait-redzone-events",
      raw,
      (unsigned)events->events,
      (unsigned long long)events->data.u64);
  if (!intact) {
    fprintf(stderr, "epoll-pwait-mask-redzone-events mismatch\n");
    return 1;
  }
  return 0;
}

/* A masked epoll_pwait whose epoll descriptor is a pipe. */
static int epoll_pwait_mask_pipe_epfd(void) {
  int fds[2];
  if (pipe(fds) != 0) {
    perror("pipe");
    return 1;
  }
  sigset_t empty;
  sigemptyset(&empty);
  struct epoll_event ready;
  errno = 0;
  int result = epoll_pwait(fds[0], &ready, 1, 1000, &empty);
  int result_errno = errno;
  close(fds[0]);
  close(fds[1]);
  printf(
      "%-24s result=%011d errno=%011d\n",
      "epoll-pwait-pipe-epfd",
      result,
      result < 0 ? result_errno : 0);
  if (result != -1 || result_errno != EINVAL) {
    fprintf(stderr, "epoll-pwait-mask-pipe-epfd mismatch\n");
    return 1;
  }
  return 0;
}

static volatile uint64_t race_epoll_mask;
static atomic_int race_epoll_announced;
static pthread_t race_epoll_waiter;

static void* zero_epoll_mask_then_signal(void* arg) {
  (void)arg;
  while (!atomic_load(&race_epoll_announced)) {
    sched_yield();
  }
  race_epoll_mask = 0;
  usleep(100000);
  pthread_kill(race_epoll_waiter, SIGUSR1);
  usleep(200000);
  if (write(epoll_pipe_fds[1], "x", 1) != 1) {
    perror("fill pipe");
  }
  return NULL;
}

/* With `pwait2`, the same wait through epoll_pwait2 with a 3 s timespec. */
static int epoll_pwait_mask_sibling_race(int pwait2) {
  struct sigaction action;
  memset(&action, 0, sizeof action);
  action.sa_handler = count_epoll_raised;
  sigset_t usr1;
  sigemptyset(&usr1);
  sigaddset(&usr1, SIGUSR1);
  int epoll_fd = epoll_create1(EPOLL_CLOEXEC);
  if (sigaction(SIGUSR1, &action, NULL) != 0 || epoll_fd < 0 ||
      pipe(epoll_pipe_fds) != 0 || pthread_sigmask(SIG_BLOCK, &usr1, NULL) != 0) {
    perror("setup");
    return 1;
  }
  struct epoll_event watched = {.events = EPOLLIN, .data.u64 = 0x5eedULL};
  if (epoll_ctl(epoll_fd, EPOLL_CTL_ADD, epoll_pipe_fds[0], &watched) != 0) {
    perror("epoll_ctl");
    return 1;
  }
  race_epoll_mask = UINT64_C(1) << (SIGUSR1 - 1);
  race_epoll_waiter = pthread_self();
  pthread_t racer;
  if (pthread_create(&racer, NULL, zero_epoll_mask_then_signal, NULL) != 0) {
    perror("pthread_create");
    return 1;
  }
  struct epoll_event ready;
  memset(&ready, 0, sizeof ready);
  atomic_store(&race_epoll_announced, 1);
  errno = 0;
  static struct timespec three_seconds = {3, 0};
  long result = pwait2 ? syscall(SYS_epoll_pwait2, epoll_fd, &ready, 1,
                                 &three_seconds, &race_epoll_mask, (size_t)8)
                       : syscall(SYS_epoll_pwait, epoll_fd, &ready, 1, 3000,
                                 &race_epoll_mask, (size_t)8);
  int result_errno = errno;
  int handled_at_return = (int)epoll_raised;
  if (pthread_join(racer, NULL) != 0) {
    perror("pthread_join");
    return 1;
  }
  close(epoll_pipe_fds[0]);
  close(epoll_pipe_fds[1]);
  close(epoll_fd);
  printf(
      "%-24s result=%011ld errno=%011d handled_at_return=%01d\n",
      pwait2 ? "epoll-pwait2-sibling-race" : "epoll-pwait-sibling-race",
      result,
      result < 0 ? result_errno : 0,
      handled_at_return);
  /* The kernel copied either the zeroed buffer (EINTR, handler run) or the
   * original mask (the pipe's byte, handler after the call). */
  int interrupted = result == -1 && result_errno == EINTR && handled_at_return == 1;
  int ready_first = result == 1 && handled_at_return == 0;
  if (!interrupted && !ready_first) {
    fprintf(stderr, "epoll-pwait-mask-sibling-race mismatch\n");
    return 1;
  }
  return 0;
}

/* epoll_pwait under an empty temporary mask with SIGUSR1 already pending. */
static int epoll_pwait_mask_raise(void) {
  struct sigaction action;
  memset(&action, 0, sizeof action);
  action.sa_handler = count_epoll_raised;
  sigset_t usr1, empty;
  sigemptyset(&usr1);
  sigaddset(&usr1, SIGUSR1);
  sigemptyset(&empty);
  int epoll_fd = epoll_create1(EPOLL_CLOEXEC);
  int fds[2];
  if (sigaction(SIGUSR1, &action, NULL) != 0 || epoll_fd < 0 || pipe(fds) != 0 ||
      pthread_sigmask(SIG_BLOCK, &usr1, NULL) != 0) {
    perror("setup");
    return 1;
  }
  struct epoll_event watched = {.events = EPOLLIN, .data.u64 = 0x5eedULL};
  if (epoll_ctl(epoll_fd, EPOLL_CTL_ADD, fds[0], &watched) != 0 || raise(SIGUSR1) != 0) {
    perror("epoll_ctl or raise");
    return 1;
  }
  struct epoll_event ready;
  memset(&ready, 0, sizeof ready);
  errno = 0;
  int result = epoll_pwait(epoll_fd, &ready, 1, 30000, &empty);
  int result_errno = errno;
  int handled = (int)epoll_raised;
  sigset_t pending;
  sigpending(&pending);
  int still_pending = sigismember(&pending, SIGUSR1);
  close(fds[0]);
  close(fds[1]);
  close(epoll_fd);
  printf(
      "%-24s result=%011d errno=%011d handled=%01d pending=%01d\n",
      "epoll-pwait-raise",
      result,
      result < 0 ? result_errno : 0,
      handled,
      still_pending);
  if (result != -1 || result_errno != EINTR || handled != 1 || still_pending != 0) {
    fprintf(stderr, "epoll-pwait-mask-raise mismatch\n");
    return 1;
  }
  return 0;
}

/* Raw epoll_pwait2 on a pipe: `mode` 0 a ready pipe, 1 a pipe a thread fills,
 * 2 an empty mask with SIGUSR1 pending at entry. */
static int epoll_pwait2_mode(const char* name, int mode) {
  struct sigaction action;
  memset(&action, 0, sizeof action);
  action.sa_handler = count_epoll_raised;
  sigset_t usr1, empty;
  sigemptyset(&usr1);
  sigaddset(&usr1, SIGUSR1);
  sigemptyset(&empty);
  int epoll_fd = epoll_create1(EPOLL_CLOEXEC);
  if (epoll_fd < 0 || pipe(epoll_pipe_fds) != 0 ||
      sigaction(SIGUSR1, &action, NULL) != 0 ||
      pthread_sigmask(SIG_BLOCK, &usr1, NULL) != 0) {
    perror("setup");
    return 1;
  }
  struct epoll_event watched = {.events = EPOLLIN, .data.u64 = 0x5eedULL};
  if (epoll_ctl(epoll_fd, EPOLL_CTL_ADD, epoll_pipe_fds[0], &watched) != 0) {
    perror("epoll_ctl");
    return 1;
  }
  pthread_t filler;
  if (mode == 1) {
    if (pthread_create(&filler, NULL, fill_epoll_pipe_later, NULL) != 0) {
      perror("pthread_create");
      return 1;
    }
  } else if (mode == 0) {
    if (write(epoll_pipe_fds[1], "x", 1) != 1) {
      perror("write");
      return 1;
    }
  } else if (raise(SIGUSR1) != 0) {
    perror("raise");
    return 1;
  }
  struct timespec bound = {2, 0};
  struct epoll_event ready;
  memset(&ready, 0, sizeof ready);
  errno = 0;
  long result = syscall(
      SYS_epoll_pwait2, epoll_fd, &ready, 1, &bound, mode == 2 ? &empty : NULL,
      (size_t)8);
  int result_errno = errno;
  if (mode == 1 && pthread_join(filler, NULL) != 0) {
    perror("pthread_join");
    return 1;
  }
  int handled = (int)epoll_raised;
  close(epoll_pipe_fds[0]);
  close(epoll_pipe_fds[1]);
  close(epoll_fd);
  printf(
      "%-24s result=%011ld errno=%011d events=%08x handled=%01d\n",
      name,
      result,
      result < 0 ? result_errno : 0,
      result > 0 ? ready.events : 0,
      handled);
  int expected = mode == 2 ? result == -1 && result_errno == EINTR && handled == 1
                           : result == 1 && (ready.events & EPOLLIN) &&
                                 ready.data.u64 == 0x5eedULL;
  if (!expected) {
    fprintf(stderr, "%s mismatch\n", name);
    return 1;
  }
  return 0;
}

/* A masked epoll_pwait2 with a 30 s timespec on a stack 48 bytes into a
 * writable page above an inaccessible one, the timespec in the top 16 bytes of
 * the red zone, and a writer at 300 ms (Claude review of
 * https://github.com/rrnewton/hermit/pull/4128). */
static int epoll_pwait2_timeout_in_red_zone(void) {
  long page = sysconf(_SC_PAGESIZE);
  unsigned char* area = mmap(
      NULL, (size_t)page * 2, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  int epoll_fd = epoll_create1(EPOLL_CLOEXEC);
  if (area == MAP_FAILED ||
      mprotect(area + page, (size_t)page, PROT_READ | PROT_WRITE) != 0 ||
      epoll_fd < 0 || pipe(epoll_pipe_fds) != 0) {
    perror("setup");
    return 1;
  }
  struct epoll_event watched = {.events = EPOLLIN, .data.u64 = 0x5eedULL};
  if (epoll_ctl(epoll_fd, EPOLL_CTL_ADD, epoll_pipe_fds[0], &watched) != 0) {
    perror("epoll_ctl");
    return 1;
  }
  unsigned char* tight_stack = area + page + 48;
  struct timespec* timeout = (struct timespec*)(tight_stack - sizeof(struct timespec));
  for (int i = -48; i < -(int)sizeof(struct timespec); i++) {
    tight_stack[i] = (unsigned char)(0xa5 ^ (i & 0xff));
  }
  timeout->tv_sec = 30;
  timeout->tv_nsec = 0;
  static struct epoll_event ready;
  static uint64_t empty_mask;
  pthread_t filler;
  if (pthread_create(&filler, NULL, fill_epoll_pipe_later, NULL) != 0) {
    perror("pthread_create");
    return 1;
  }
  const long args[6] = {
      epoll_fd, (long)&ready, 1, (long)timeout, (long)&empty_mask, 8};
  long raw = syscall6_on_stack(tight_stack, SYS_epoll_pwait2, args);
  int red_zone_changed = 0;
  for (int i = -48; i < -(int)sizeof(struct timespec); i++) {
    red_zone_changed += tight_stack[i] != (unsigned char)(0xa5 ^ (i & 0xff));
  }
  pthread_join(filler, NULL);
  close(epoll_pipe_fds[0]);
  close(epoll_pipe_fds[1]);
  close(epoll_fd);
  printf(
      "%-30s result=%011ld events=%08x red_zone_changed=%03d\n",
      "epoll-pwait2-timeout-in-red-zone",
      raw,
      raw > 0 ? ready.events : 0,
      red_zone_changed);
  if (raw != 1 || !(ready.events & EPOLLIN) || red_zone_changed != 0) {
    fprintf(stderr, "epoll-pwait2-timeout-in-red-zone mismatch\n");
    return 1;
  }
  return 0;
}

static atomic_int busy_sibling_done;

static void* loop_without_syscalls(void* arg) {
  (void)arg;
  volatile unsigned long spins = 0;
  while (!atomic_load(&busy_sibling_done)) {
    spins++;
  }
  return NULL;
}

/* A masked zero-timespec epoll_pwait2 on a ready pipe beside a sibling that
 * loops without a system call (Claude review of
 * https://github.com/rrnewton/hermit/pull/4128). */
static int epoll_pwait2_zero_beside_busy_sibling(void) {
  int epoll_fd = epoll_create1(EPOLL_CLOEXEC);
  if (epoll_fd < 0 || pipe(epoll_pipe_fds) != 0 ||
      write(epoll_pipe_fds[1], "x", 1) != 1) {
    perror("setup");
    return 1;
  }
  struct epoll_event watched = {.events = EPOLLIN, .data.u64 = 0x5eedULL};
  if (epoll_ctl(epoll_fd, EPOLL_CTL_ADD, epoll_pipe_fds[0], &watched) != 0) {
    perror("epoll_ctl");
    return 1;
  }
  pthread_t sibling;
  if (pthread_create(&sibling, NULL, loop_without_syscalls, NULL) != 0) {
    perror("pthread_create");
    return 1;
  }
  static uint64_t empty_mask;
  static struct timespec zero = {0, 0};
  struct epoll_event ready;
  memset(&ready, 0, sizeof ready);
  long result =
      syscall(SYS_epoll_pwait2, epoll_fd, &ready, 1, &zero, &empty_mask, (size_t)8);
  atomic_store(&busy_sibling_done, 1);
  pthread_join(sibling, NULL);
  close(epoll_pipe_fds[0]);
  close(epoll_pipe_fds[1]);
  close(epoll_fd);
  printf(
      "%-30s result=%011ld events=%08x\n",
      "epoll-pwait2-zero-busy-sibling",
      result,
      result > 0 ? ready.events : 0);
  if (result != 1 || !(ready.events & EPOLLIN)) {
    fprintf(stderr, "epoll-pwait2-zero-beside-busy-sibling mismatch\n");
    return 1;
  }
  return 0;
}

/* Linux copies and checks epoll_pwait2's timespec before its mask (Claude
 * review of https://github.com/rrnewton/hermit/pull/4128). */
static int epoll_pwait2_error_order(void) {
  int epoll_fd = epoll_create1(EPOLL_CLOEXEC);
  if (epoll_fd < 0) {
    perror("epoll_create1");
    return 1;
  }
  static uint64_t empty_mask;
  struct epoll_event ready;
  errno = 0;
  long unreadable_timeout = syscall(
      SYS_epoll_pwait2, epoll_fd, &ready, 1, (void*)8, &empty_mask, (size_t)4);
  int unreadable_timeout_errno = errno;
  static struct timespec invalid = {0, 2000000000};
  errno = 0;
  long invalid_timeout = syscall(
      SYS_epoll_pwait2, epoll_fd, &ready, 1, &invalid, (void*)8, (size_t)8);
  int invalid_timeout_errno = errno;
  close(epoll_fd);
  printf(
      "%-30s unreadable_timeout=%ld/%d invalid_timeout=%ld/%d\n",
      "epoll-pwait2-error-order",
      unreadable_timeout,
      unreadable_timeout_errno,
      invalid_timeout,
      invalid_timeout_errno);
  if (unreadable_timeout != -1 || unreadable_timeout_errno != EFAULT ||
      invalid_timeout != -1 || invalid_timeout_errno != EINVAL) {
    fprintf(stderr, "epoll-pwait2-error-order mismatch\n");
    return 1;
  }
  return 0;
}

/* epoll_pwait under a temporary mask while the timer's SIGALRM is due. */
static int epoll_pwait_mask_alarm(int blocks) {
  struct sigaction action;
  memset(&action, 0, sizeof action);
  action.sa_handler = count_masked_alarm;
  sigset_t alarm_set, during;
  sigemptyset(&alarm_set);
  sigaddset(&alarm_set, SIGALRM);
  if (blocks) {
    during = alarm_set;
  } else {
    sigemptyset(&during);
  }
  int epoll_fd = epoll_create1(EPOLL_CLOEXEC);
  if (sigaction(SIGALRM, &action, NULL) != 0 || epoll_fd < 0 ||
      pipe(alarm_pipe_fds) != 0 ||
      pthread_sigmask(SIG_BLOCK, &alarm_set, NULL) != 0) {
    perror("setup");
    return 1;
  }
  struct epoll_event watched = {.events = EPOLLIN, .data.u64 = 0x5eedULL};
  if (epoll_ctl(epoll_fd, EPOLL_CTL_ADD, alarm_pipe_fds[0], &watched) != 0) {
    perror("epoll_ctl");
    return 1;
  }
  pthread_t writer;
  if (blocks) {
    if (pthread_create(&writer, NULL, fill_alarm_pipe_later, NULL) != 0 ||
        pthread_sigmask(SIG_UNBLOCK, &alarm_set, NULL) != 0) {
      perror("writer");
      return 1;
    }
  }
  struct itimerval alarm_in = {{0, 0}, {0, 100000}};
  if (setitimer(ITIMER_REAL, &alarm_in, NULL) != 0) {
    perror("setitimer");
    return 1;
  }
  struct epoll_event ready;
  memset(&ready, 0, sizeof ready);
  errno = 0;
  int result = epoll_pwait(epoll_fd, &ready, 1, 30000, &during);
  int result_errno = errno;
  if (blocks && pthread_join(writer, NULL) != 0) {
    perror("pthread_join");
    return 1;
  }
  int alarms = (int)masked_alarms;
  close(alarm_pipe_fds[0]);
  close(alarm_pipe_fds[1]);
  close(epoll_fd);
  printf(
      "%-24s result=%011d errno=%011d alarms=%01d\n",
      blocks ? "epoll-pwait-blocks" : "epoll-pwait-unblocks",
      result,
      result < 0 ? result_errno : 0,
      alarms);
  int expected = blocks ? result == 1 && alarms == 1
                        : result == -1 && result_errno == EINTR && alarms == 1;
  if (!expected) {
    fprintf(stderr, "epoll-pwait-mask-%s-alarm mismatch\n", blocks ? "blocks" : "unblocks");
    return 1;
  }
  return 0;
}

static atomic_int yielding_sibling_done;

static void* yield_until_done(void* arg) {
  (void)arg;
  while (!atomic_load(&yielding_sibling_done)) {
    sched_yield();
  }
  return NULL;
}

/* With `yielders` siblings: two spinners keep replacing each other's
 * one-turn yield exclusion (Claude review of
 * https://github.com/rrnewton/hermit/pull/4120). */
static int pselect_beside_yielding_siblings(const char* name, int yielders) {
  struct sigaction action;
  memset(&action, 0, sizeof action);
  action.sa_handler = count_raised_usr1;
  sigset_t usr1;
  sigemptyset(&usr1);
  sigaddset(&usr1, SIGUSR1);
  if (sigaction(SIGUSR1, &action, NULL) != 0 ||
      pthread_sigmask(SIG_BLOCK, &usr1, NULL) != 0) {
    perror("setup");
    return 1;
  }
  raised_usr1 = 0;
  pthread_t siblings[2];
  for (int i = 0; i < yielders; i++) {
    if (pthread_create(&siblings[i], NULL, yield_until_done, NULL) != 0) {
      perror("pthread_create");
      return 1;
    }
  }
  static uint64_t empty_mask;
  static struct pselect_mask_wrapper wrapper = {
      (const void*)&empty_mask, sizeof(uint64_t)};
  int zeros = 0, interrupted = 0, other = 0;
  for (int i = 0; i < 10; i++) {
    struct timespec bound = {0, 1000000};
    long raw;
    if (i % 2 == 0) {
      raw = syscall(SYS_pselect6, 0, NULL, NULL, NULL, &bound, NULL);
    } else {
      raise(SIGUSR1);
      raw = syscall(SYS_pselect6, 0, NULL, NULL, NULL, &bound, &wrapper);
    }
    if (raw == 0) {
      zeros++;
    } else if (raw == -1 && errno == EINTR) {
      interrupted++;
    } else {
      other++;
    }
  }
  atomic_store(&yielding_sibling_done, 1);
  for (int i = 0; i < yielders; i++) {
    pthread_join(siblings[i], NULL);
  }
  printf(
      "%-24s yielders=%01d zeros=%02d interrupted=%02d other=%02d handled=%02d\n",
      "pselect-yielding-sibling",
      yielders,
      zeros,
      interrupted,
      other,
      (int)raised_usr1);
  if (zeros != 5 || interrupted != 5 || other != 0 || raised_usr1 != 5) {
    fprintf(stderr, "%s mismatch\n", name);
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
        "pselect-mask-unblocks-alarm|pselect-mask-unblocks-rewritten|"
        "pselect-mask-unblocks-tight-stack|pselect-mask-zero-raise|"
        "ppoll-mask-zero-raise|ppoll-mask-blocks-alarm|"
        "ppoll-mask-unblocks-alarm|pselect-mask-zero-race|"
        "ppoll-mask-zero-raise-tight|ppoll-mask-zero-raise-tight24|"
        "ppoll-mask-raise-tight24|ppoll-mask-raise-tight128|"
        "ppoll-zero-tight16|ppoll-mask-redzone-pollfd|"
        "ppoll-mask-raise-tight8|ppoll-mask-timeout-in-red-zone|"
        "pselect-mask-hook-race|"
        "epoll-pwait-ready|epoll-pwait-mask-ready|epoll-pwait-writer|"
        "epoll-pwait-mask-blocks-alarm|epoll-pwait-mask-unblocks-alarm|"
        "epoll-pwait-mask-raise|epoll-pwait-mask-sibling-race|"
        "epoll-pwait-mask-raise-tight|epoll-pwait-mask-raise-tight24|"
        "epoll-pwait-mask-redzone-events|epoll-pwait-mask-pipe-epfd|"
        "pselect-mask-unblocks-tight-stack24|"
        "pselect-beside-a-yielding-sibling|"
        "pselect-beside-two-yielding-siblings|"
        "pselect-mask-unblocks-tight-stack8|epoll-pwait2-ready|"
        "epoll-pwait2-writer|epoll-pwait2-mask-raise|"
        "epoll-pwait2-mask-raise-tight|epoll-pwait2-mask-raise-tight24|"
        "epoll-pwait2-mask-sibling-race|epoll-pwait2-timeout-in-red-zone|"
        "epoll-pwait2-zero-beside-busy-sibling|epoll-pwait2-error-order]\n",
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
  if (strcmp(argv[1], "pselect-mask-unblocks-tight-stack") == 0) {
    return pselect_mask_unblocks_tight_stack(144);
  }
  if (strcmp(argv[1], "pselect-mask-unblocks-tight-stack8") == 0) {
    return pselect_mask_unblocks_tight_stack(8);
  }
  if (strcmp(argv[1], "pselect-mask-unblocks-tight-stack24") == 0) {
    return pselect_mask_unblocks_tight_stack(24);
  }
  if (strcmp(argv[1], "epoll-pwait-ready") == 0) {
    return epoll_pwait_mode("epoll-pwait-ready", 0, 0);
  }
  if (strcmp(argv[1], "epoll-pwait-mask-ready") == 0) {
    return epoll_pwait_mode("epoll-pwait-mask-ready", 1, 0);
  }
  if (strcmp(argv[1], "epoll-pwait-writer") == 0) {
    return epoll_pwait_mode("epoll-pwait-writer", 0, 1);
  }
  if (strcmp(argv[1], "epoll-pwait-mask-blocks-alarm") == 0) {
    return epoll_pwait_mask_alarm(1);
  }
  if (strcmp(argv[1], "epoll-pwait-mask-unblocks-alarm") == 0) {
    return epoll_pwait_mask_alarm(0);
  }
  if (strcmp(argv[1], "epoll-pwait-mask-raise") == 0) {
    return epoll_pwait_mask_raise();
  }
  if (strcmp(argv[1], "epoll-pwait-mask-sibling-race") == 0) {
    return epoll_pwait_mask_sibling_race(0);
  }
  if (strcmp(argv[1], "epoll-pwait2-mask-sibling-race") == 0) {
    return epoll_pwait_mask_sibling_race(1);
  }
  if (strcmp(argv[1], "epoll-pwait-mask-raise-tight") == 0) {
    return epoll_pwait_mask_raise_tight(128, 0);
  }
  if (strcmp(argv[1], "epoll-pwait-mask-raise-tight24") == 0) {
    return epoll_pwait_mask_raise_tight(24, 0);
  }
  if (strcmp(argv[1], "epoll-pwait2-mask-raise-tight") == 0) {
    return epoll_pwait_mask_raise_tight(128, 1);
  }
  if (strcmp(argv[1], "epoll-pwait2-mask-raise-tight24") == 0) {
    return epoll_pwait_mask_raise_tight(24, 1);
  }
  if (strcmp(argv[1], "epoll-pwait-mask-redzone-events") == 0) {
    return epoll_pwait_mask_redzone_events();
  }
  if (strcmp(argv[1], "epoll-pwait-mask-pipe-epfd") == 0) {
    return epoll_pwait_mask_pipe_epfd();
  }
  if (strcmp(argv[1], "pselect-mask-zero-raise") == 0) {
    return zero_timeout_mask_raise(0);
  }
  if (strcmp(argv[1], "ppoll-mask-zero-raise") == 0) {
    return zero_timeout_mask_raise(1);
  }
  if (strcmp(argv[1], "ppoll-mask-blocks-alarm") == 0) {
    return ppoll_mask_alarm(1);
  }
  if (strcmp(argv[1], "ppoll-mask-unblocks-alarm") == 0) {
    return ppoll_mask_alarm(0);
  }
  if (strcmp(argv[1], "pselect-mask-zero-race") == 0) {
    return pselect_mask_zero_race();
  }
  if (strcmp(argv[1], "ppoll-mask-zero-raise-tight") == 0) {
    return ppoll_tight(argv[1], 160, 1, 0, 0);
  }
  if (strcmp(argv[1], "ppoll-mask-zero-raise-tight24") == 0) {
    return ppoll_tight(argv[1], 24, 1, 0, 0);
  }
  if (strcmp(argv[1], "ppoll-mask-raise-tight24") == 0) {
    return ppoll_tight(argv[1], 24, 1, 1, 0);
  }
  if (strcmp(argv[1], "ppoll-mask-raise-tight128") == 0) {
    return ppoll_tight(argv[1], 128, 1, 1, 0);
  }
  if (strcmp(argv[1], "ppoll-zero-tight16") == 0) {
    return ppoll_tight(argv[1], 16, 0, 0, 0);
  }
  if (strcmp(argv[1], "ppoll-mask-redzone-pollfd") == 0) {
    return ppoll_tight(argv[1], 128, 1, 1, 1);
  }
  if (strcmp(argv[1], "ppoll-mask-timeout-in-red-zone") == 0) {
    return ppoll_timeout_in_red_zone();
  }
  if (strcmp(argv[1], "ppoll-mask-raise-tight8") == 0) {
    return ppoll_tight(argv[1], 8, 1, 1, 0);
  }
  if (strcmp(argv[1], "pselect-beside-a-yielding-sibling") == 0) {
    return pselect_beside_yielding_siblings(argv[1], 1);
  }
  if (strcmp(argv[1], "pselect-beside-two-yielding-siblings") == 0) {
    return pselect_beside_yielding_siblings(argv[1], 2);
  }
  if (strcmp(argv[1], "pselect-mask-hook-race") == 0) {
    return pselect_mask_hook_race();
  }
  if (strcmp(argv[1], "epoll-pwait2-timeout-in-red-zone") == 0) {
    return epoll_pwait2_timeout_in_red_zone();
  }
  if (strcmp(argv[1], "epoll-pwait2-zero-beside-busy-sibling") == 0) {
    return epoll_pwait2_zero_beside_busy_sibling();
  }
  if (strcmp(argv[1], "epoll-pwait2-error-order") == 0) {
    return epoll_pwait2_error_order();
  }
  if (strcmp(argv[1], "epoll-pwait2-ready") == 0) {
    return epoll_pwait2_mode("epoll-pwait2-ready", 0);
  }
  if (strcmp(argv[1], "epoll-pwait2-writer") == 0) {
    return epoll_pwait2_mode("epoll-pwait2-writer", 1);
  }
  if (strcmp(argv[1], "epoll-pwait2-mask-raise") == 0) {
    return epoll_pwait2_mode("epoll-pwait2-mask-raise", 2);
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
