/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Which thread takes a real-timer SIGALRM, and whether the run still makes
 * progress, when the thread Linux's complete_signal chooses is waiting
 * (https://github.com/rrnewton/hermit/issues/4929). One mode per run:
 *
 *   poll, epoll, recv, select
 *       The main thread (the leader, which the expiry names) waits for a
 *       pipe or socketpair byte in that call, a sibling waits in pause(), and
 *       a one-shot 10 ms ITIMER_REAL expires. Linux wakes the leader: the call
 *       fails with EINTR, the handler runs on the leader and writes the byte,
 *       and the next call returns it. The sibling is never woken.
 *       Prints "mode=M on_leader=1 eintr=1 n=1".
 *   dflepoll
 *       As epoll with SIGALRM at its default action, the alarm() watchdog
 *       idiom: Linux ends the process at generation. Killed by SIGALRM.
 *   lonepoll, loneepoll, lonedfl
 *       As poll, epoll and dflepoll in a process with no other thread.
 *   join
 *       The main thread waits in pthread_join (a futex wait) for a worker
 *       that makes system calls until the handler sets a flag. Linux wakes
 *       the main thread and runs the handler there ("on_leader=1"). On KVM a
 *       futex wait cannot be interrupted until N8a, so the handler runs on
 *       the worker ("on_leader=0"): the named gap D1.
 *   twotimers
 *       Two child processes each run a recurring 100 us ITIMER_REAL with a
 *       handler, making system calls between deliveries, for 300 deliveries;
 *       a third child makes 50 system calls and finishes. Each timer child
 *       records, when it stops, whether the third had finished: on Linux it
 *       always has, as the timers' deliveries do not stop a runnable process
 *       from running. Prints "twotimers: worker finished first: 1 1".
 *   rstpoll
 *       As poll, with SA_RESTART on the handler: Linux never restarts a poll
 *       that a handler interrupted. Prints "mode=rstpoll on_leader=1 eintr=1
 *       n=1".
 *   ignpoll, ignppoll, ignepoll
 *       SIGALRM is ignored and the leader blocks it, so the expiry is queued
 *       (sig_ignored keeps a blocked signal); a second thread, which leaves it
 *       unblocked, waits for a pipe byte in poll(-1), ppoll(NULL) or
 *       epoll_wait(-1), and the leader writes the byte 50 ms after a one-shot
 *       10 ms ITIMER_REAL. Linux wakes the waiter for the copy, which it
 *       drops; with no handler, poll and ppoll restart and return the byte,
 *       while epoll_wait fails with EINTR. Prints "mode=M eintr=0 n=1", or
 *       "eintr=1 n=1" for ignepoll.
 *   igntpoll
 *       As ignpoll, with a 100 ms poll timeout and no byte written: the
 *       restarted poll keeps the call's end time, so it returns 0 within the
 *       100 ms. Prints "mode=igntpoll eintr=0 n=0 within_timeout=1".
 *   mixedpoll
 *       SIGALRM is ignored and SIGUSR1 has a handler with SA_RESTART; the
 *       leader blocks both, and a second thread, which leaves both
 *       unblocked, waits for a pipe byte in poll(-1). A one-shot 5 ms POSIX
 *       timer sends SIGUSR1 and a one-shot 10 ms ITIMER_REAL SIGALRM, and the
 *       leader writes the byte 50 ms later. Linux gives both copies to the
 *       waiter, and the poll that SIGUSR1's handler interrupts fails with
 *       EINTR whatever SA_RESTART says, however the ignored SIGALRM came
 *       with it. Prints "mode=mixedpoll first=-1 errno=EINTR eintr=1 usr1=1
 *       on_waiter=1".
 *   mixedtaken
 *       As mixedpoll, but the leader leaves SIGUSR1 unblocked and makes
 *       system calls until its handler has run: Linux gives the copy to the
 *       running leader, so the waiter is woken only for the ignored SIGALRM,
 *       finds no handler to run and restarts. Prints "mode=mixedtaken
 *       first=1 errno=0 eintr=0 usr1=1 on_waiter=0".
 *   mixedusr2
 *       As mixedpoll with SIGUSR2 for SIGUSR1, and the leader unblocks SIGUSR2
 *       as the 10 ms expiry falls due. Linux gives the timer's SIGUSR2 to the
 *       waiter, whose poll its handler interrupts. Prints "mode=mixedusr2
 *       first=-1 errno=EINTR eintr=1 usr1=1 on_waiter=1".
 *   mixedshared
 *       As mixedpoll with the actions swapped: SIGUSR1 is ignored and SIGALRM
 *       has the handler with SA_RESTART; the leader unblocks SIGALRM as its
 *       10 ms expiry falls due, and takes that copy. The waiter's poll is
 *       restarted: nothing is left that a handler takes on the waiter, and
 *       its ignored SIGUSR1 is dropped. Prints "mode=mixedshared first=1
 *       errno=0 eintr=0 usr1=1 on_waiter=0".
 *   mixedlate
 *       As mixedpoll with the POSIX timer's SIGUSR1 at 11 ms, after the
 *       ignored SIGALRM: the waiter restarts its poll for the SIGALRM, and
 *       the SIGUSR1 handler interrupts the restarted poll. Prints
 *       "mode=mixedlate first=-1 errno=EINTR eintr=1 usr1=1 on_waiter=1".
 *   mixedbehind, mixedbehindnors
 *       As mixedshared without the leader's unblock, so only the waiter can
 *       take the caught SIGALRM. On Linux the ignored SIGUSR1 wakes the poll
 *       first, is dropped, and the poll restarts; on KVM, which wakes no
 *       poller for a POSIX timer's SIGUSR1, it is still pending ahead of the
 *       SIGALRM when the alarm wakes the poll. Either way SIGALRM's handler,
 *       with SA_RESTART in mixedbehind and without it in mixedbehindnors,
 *       interrupts the poll. Prints "mode=M first=-1 errno=EINTR eintr=1
 *       usr1=1 on_waiter=1".
 *   mixedtgkill
 *       As mixedbehind, with the ignored SIGUSR1 sent to the waiter alone with
 *       tgkill instead of from a POSIX timer: Linux discards it when it is
 *       sent, as the waiter leaves it unblocked, and KVM keeps it for the
 *       waiter's next return. Prints "mode=mixedtgkill first=-1 errno=EINTR
 *       eintr=1 usr1=1 on_waiter=1".
 *   mixedtgkillusr2
 *       As mixedtgkill, with SIGUSR2 caught with SA_RESTART, blocked by the
 *       leader, which sends it to the waiter with tgkill as the 10 ms expiry
 *       falls due: on KVM it comes during the ignored copy's delivery, ahead
 *       of the pending SIGALRM in the waiter's own queue. Linux's return finds
 *       SIGALRM with or without SIGUSR2, and the poll fails with EINTR. Prints
 *       "mode=mixedtgkillusr2 first=-1 errno=EINTR ..." with each handler's
 *       count.
 *   mixedignlate
 *       SIGALRM has a handler with SA_RESTART and no other signal is involved;
 *       the leader blocks SIGALRM and ignores it as the 10 ms expiry falls due.
 *       Linux either runs the handler on the waiter, whose poll fails with
 *       EINTR, or, if the ignore comes first, discards the pending copy and
 *       restarts the poll. Prints "mode=mixedignlate first=-1 errno=EINTR
 *       eintr=1 usr1=1 on_waiter=1" or "first=1 errno=0 eintr=0 usr1=0
 *       on_waiter=0".
 *   mixedignterm
 *       As mixedignlate, with a POSIX timer's SIGTERM due at the same 10 ms,
 *       caught with SA_RESTART, which the leader blocks and the waiter does
 *       not: where the waiter's return finds SIGALRM ignored, SIGTERM's handler
 *       is still there to run, and Linux fails the poll with EINTR either way.
 *       Prints "mode=mixedignterm first=-1 errno=EINTR eintr=1 usr1=1
 *       on_waiter=1 term=1" (both handlers ran) or "first=-1 errno=EINTR
 *       eintr=1 usr1=0 on_waiter=0 term=1" (SIGTERM's alone).
 *   pollretry, pollretryraw
 *       A single thread whose SIGALRM handler, without SA_RESTART, only counts
 *       and makes no system call arms a one-shot 50 ms ITIMER_REAL and polls a
 *       pipe nobody writes for 100 ms, again after each EINTR, the usual retry
 *       loop: through libc's poll(), or with the raw system call and zeros in
 *       its unused argument registers (pollretryraw), as glibc 2.41 and later
 *       pass them too, so the retry is the same call at the same address as
 *       the first. The retry is a new call, which Linux gives its whole
 *       timeout: the loop ends about 150 ms after the arming. Prints
 *       "mode=M n=0 eintr=1 handled=1 on_waiter=1 elapsed=150", the time from
 *       the arming rounded to 50 ms.
 *   retryalone, retrybehind, retrytgkill
 *       As pollretryraw, in a second thread, while the leader blocks SIGALRM
 *       and SIGUSR1, which is ignored. In retrybehind a POSIX timer's SIGUSR1
 *       falls due at 5 ms, which KVM wakes no poller for, so it is pending
 *       ahead of SIGALRM at the poll's return (mixedbehindnors' shape); in
 *       retrytgkill it is a tgkill to the waiter, which Linux discards when it
 *       is sent and KVM keeps for the waiter's next return. Prints the same
 *       line.
 *   ignmove, ignmoveblocked
 *       The leader ignores SIGALRM; of two threads waiting in poll(-1), A
 *       blocks SIGALRM and B does not. Copy 1, a one-shot 10 ms POSIX timer's
 *       SIGALRM, expires while the leader waits on a futex word for 50 ms,
 *       leaving SIGALRM unblocked (ignmove) or blocking it (ignmoveblocked).
 *       Then the leader blocks SIGALRM and installs a handler, A unblocks it,
 *       and a one-shot 10 ms ITIMER_REAL sends copy 2. Linux drops copy 1 in
 *       ignmove, where the leader leaves it unblocked (sig_ignored), so the
 *       walk for copy 2 starts from the leader and reaches A; in
 *       ignmoveblocked copy 1 is queued and its walk moves curr_target to B,
 *       where copy 2's walk starts. Prints "mode=ignmove handler_on=A
 *       eintr_a=1 eintr_b=0", or "handler_on=B eintr_a=0 eintr_b=1".
 *   sendign PORT, sendcaught PORT
 *       A second thread, the only one that leaves SIGALRM unblocked, connects
 *       a TCP socket to 127.0.0.1:PORT, a peer that reads nothing for a
 *       while, fills its send buffer with MSG_DONTWAIT sends until EAGAIN,
 *       arms a one-shot 10 ms ITIMER_REAL and sends one byte, which waits for
 *       room. In sendign SIGALRM is ignored (the leader blocks it, so the copy
 *       is queued): Linux's send stops waiting with nothing sent, drops the
 *       copy and is made again, and returns the byte once the peer reads. In
 *       sendcaught a handler without SA_RESTART takes it, and the send fails
 *       with EINTR. Prints "mode=sendign n=1 errno=0 handled=0" or
 *       "mode=sendcaught n=-1 errno=EINTR handled=1"; under network recording
 *       (--record-networking) Hermit refuses sendcaught by name.
 *   sendlate PORT
 *       As sendign, with a handler without SA_RESTART installed for SIGALRM as
 *       the 10 ms expiry falls due: Linux restarts the send if it drops the
 *       copy first, or runs the handler and fails the send with EINTR.
 *       Prints "mode=sendlate n=1 errno=0 handled=0" or "n=-1 errno=EINTR
 *       handled=1"; under network recording Hermit refuses it by name.
 *   igntchannel PORT
 *       As igntpoll, waiting on a TCP socket connected to 127.0.0.1:PORT, a
 *       peer that sends nothing, instead of a pipe. Under network recording
 *       or replay (--record-networking, --replay-networking) the socket is
 *       a recorded one. Prints "mode=igntchannel eintr=0 n=0
 *       within_timeout=1".
 *   ignperiodic
 *       As ignpoll, with a periodic 10 ms ITIMER_REAL, and the byte written
 *       100 ms in: the waiter drops each copy, and that dequeue restarts the
 *       timer, so the timer still runs and no copy is pending then. Prints
 *       "mode=ignperiodic running=1 pending=0 eintr=0 n=1".
 *   posixtimer
 *       A periodic 100 us POSIX timer (timer_create with SIGEV_SIGNAL and
 *       SIGALRM, which rearms whether or not its copy was taken) delivers to
 *       a handler on the main thread, which yields until 300 deliveries, then
 *       disarms the timer; a worker that blocks SIGALRM counts its loop
 *       iterations, each a system call. Prints the worker's count just
 *       before the timer is disarmed: on Linux the worker runs between
 *       deliveries.
 *   posixspin
 *       As posixtimer, with three workers, and a main thread that spins on
 *       getppid(), a system call that ends no turn, until the 300 deliveries:
 *       only the deliveries stop it. Prints the three workers' total count.
 *   posixspin200
 *       As posixspin, until 200 deliveries, with a sched_yield after every
 *       20000 getppid calls, which ends the main thread's turn: the copies
 *       that fall due during one stretch still come one handler after
 *       another, but each stretch is short. Prints the three workers' total
 *       count and the least count of any one of them, both counted while the
 *       timer is armed, from its arming to just before its disarm: what a
 *       worker did at start-up, before the first copy could fall due, or once
 *       the disarm let it run, is not progress past the deliveries.
 *   posixpoll
 *       As posixspin, with a main thread that waits in poll(-1) on a pipe
 *       nobody writes, which each delivery ends with EINTR: the poller must
 *       be woken for each copy, or the 300 deliveries never come. Prints the
 *       three workers' total count.
 *   pjoin, pdflfutex, pdflfutexblocked, pdfljoin, pdfljoinforever
 *       As join, dflfutex, dflfutexblocked, dfljoin and dfljoinforever, with
 *       the one-shot 10 ms expiry from a POSIX timer (timer_create with
 *       SIGEV_SIGNAL and SIGALRM) instead of ITIMER_REAL: Linux chooses the
 *       thread for its signal the same way (posixtimer_queue_sigqueue ->
 *       complete_signal), ends the process for a default-action copy at
 *       once, and keeps a copy the only thread blocks pending.
 *   rcvtimeo, rcvtimeorst, rcvuntimed, rcvuntimedrst, rcvpartial
 *       A single thread receives on a stream socketpair, with a 1 s
 *       SO_RCVTIMEO (rcvtimeo*, rcvpartial) or none (rcvuntimed*), and a
 *       handler (with SA_RESTART in the *rst modes) that writes one byte to
 *       the socket; a one-shot 10 ms ITIMER_REAL interrupts the receive.
 *       Linux ends a receive with a finite timeout with EINTR whatever the
 *       handler's flags, and restarts an untimed one under SA_RESTART, which
 *       then reads the handler's byte. In rcvpartial three bytes are queued
 *       first and the receive asks for ten with MSG_WAITALL: it returns the
 *       three. In rcvalias the receive is untimed with SA_RESTART, and a
 *       second thread sets a finite SO_RCVTIMEO through a dup of the socket
 *       5 ms into it: the receive keeps the class it began with and
 *       restarts. Prints "mode=M first=R errno=E".
 *   acctimeo, accuntimed
 *       As rcvtimeorst and rcvuntimed for accept on a listening AF_UNIX
 *       socket (an abstract address) that nobody connects to: a 1 s
 *       SO_RCVTIMEO with SA_RESTART, and no timeout without it. Linux ends
 *       both with EINTR. Prints "mode=M first=-1 errno=EINTR".
 *   dflfutex, dflfutexblocked
 *       A single thread with SIGALRM at its default action arms a one-shot
 *       10 ms ITIMER_REAL and waits on a futex word nobody wakes: forever in
 *       dflfutex, which Linux ends at the expiry (killed by SIGALRM), and for
 *       50 ms in dflfutexblocked, whose thread blocks SIGALRM, so Linux keeps
 *       the copy pending and the wait times out: prints "futex: timed out".
 *   dfljoin, dfljoinforever
 *       SIGALRM at its default action (the alarm() watchdog); the main thread
 *       waits in pthread_join for a worker that blocks SIGALRM and prints a
 *       line every 1 ms, 100 times (dfljoin) or forever (dfljoinforever).
 *       Linux ends the process at the 10 ms expiry, whatever the main thread
 *       waits in. Killed by SIGALRM after a few lines; "joined: not killed"
 *       would mean it was not.
 */

#define _GNU_SOURCE

#include <arpa/inet.h>
#include <errno.h>
#include <linux/futex.h>
#include <netinet/in.h>
#include <stddef.h>
#include <poll.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/mman.h>
#include <sys/select.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <sys/un.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static int fds[2];
static volatile pid_t handled_by;
static volatile int handled;

static void wake(int sig) {
  (void)sig;
  handled_by = (pid_t)syscall(SYS_gettid);
  handled = 1;
  char byte = 'w';
  if (write(fds[1], &byte, 1) != 1) {
    _exit(3);
  }
}

static void* pauser(void* arg) {
  (void)arg;
  for (;;) {
    pause();
  }
  return NULL;
}

/* A system call that ends the caller's turn, so that each one is a point
 * where Hermit schedules. */
static void scheduled_call(void) {
  sched_yield();
}

static void* caller(void* arg) {
  (void)arg;
  while (!handled) {
    scheduled_call();
  }
  return NULL;
}

/* The shared record of the twotimers mode. */
struct shared {
  volatile int worker_done;
  volatile int first[2];
};

static volatile int deliveries;

static void count(int sig) {
  (void)sig;
  deliveries++;
}

static void timer_child(struct shared* shared, int which) {
  struct sigaction action;
  memset(&action, 0, sizeof(action));
  action.sa_handler = count;
  sigaction(SIGALRM, &action, NULL);
  struct itimerval every;
  memset(&every, 0, sizeof(every));
  every.it_value.tv_usec = 100;
  every.it_interval.tv_usec = 100;
  setitimer(ITIMER_REAL, &every, NULL);
  while (deliveries < 300) {
    scheduled_call();
  }
  memset(&every, 0, sizeof(every));
  setitimer(ITIMER_REAL, &every, NULL);
  shared->first[which] = shared->worker_done;
  _exit(0);
}

static int two_timers(void) {
  struct shared* shared =
      mmap(NULL, sizeof(*shared), PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
  if (shared == MAP_FAILED) {
    perror("mmap");
    return 1;
  }
  memset((void*)shared, 0, sizeof(*shared));
  pid_t children[3];
  for (int which = 0; which < 3; which++) {
    children[which] = fork();
    if (children[which] < 0) {
      perror("fork");
      return 1;
    }
    if (children[which] == 0) {
      if (which < 2) {
        timer_child(shared, which);
      }
      for (int call = 0; call < 50; call++) {
        scheduled_call();
      }
      shared->worker_done = 1;
      _exit(0);
    }
  }
  for (int which = 0; which < 3; which++) {
    int status = 0;
    if (waitpid(children[which], &status, 0) != children[which] || !WIFEXITED(status) ||
        WEXITSTATUS(status) != 0) {
      fprintf(stderr, "child %d ended with status %d\n", which, status);
      return 1;
    }
  }
  printf("twotimers: worker finished first: %d %d\n", shared->first[0], shared->first[1]);
  return 0;
}

/* The ignpoll, ignppoll, ignepoll, igntpoll and igntchannel modes' waiting
 * thread. */
static const char* ignored_mode;
static int channel_port;
static volatile int ignored_eintr;
static volatile int ignored_result;
static volatile int ignored_within_timeout;

static long monotonic_ms(void) {
  struct timespec now;
  clock_gettime(CLOCK_MONOTONIC, &now);
  return now.tv_sec * 1000 + now.tv_nsec / 1000000;
}

static void* ignored_waiter(void* arg) {
  (void)arg;
  int timed = strcmp(ignored_mode, "igntpoll") == 0 || strcmp(ignored_mode, "igntchannel") == 0;
  int epoll = -1;
  struct epoll_event event = {.events = EPOLLIN, .data.fd = fds[0]};
  struct epoll_event ready;
  if (strcmp(ignored_mode, "ignepoll") == 0) {
    epoll = epoll_create1(0);
    epoll_ctl(epoll, EPOLL_CTL_ADD, fds[0], &event);
  }
  long started = monotonic_ms();
  int n;
  for (;;) {
    struct pollfd wait_for = {.fd = fds[0], .events = POLLIN};
    if (epoll >= 0) {
      n = epoll_wait(epoll, &ready, 1, -1);
    } else if (strcmp(ignored_mode, "ignppoll") == 0) {
      n = ppoll(&wait_for, 1, NULL, NULL);
    } else {
      n = poll(&wait_for, 1, timed ? 100 : -1);
    }
    if (n == -1 && errno == EINTR) {
      ignored_eintr++;
      continue;
    }
    break;
  }
  ignored_within_timeout = monotonic_ms() - started < 115;
  ignored_result = n;
  return NULL;
}

static int ignored_queued(const char* mode) {
  ignored_mode = mode;
  int channel = strcmp(mode, "igntchannel") == 0;
  if (channel) {
    struct sockaddr_in peer;
    memset(&peer, 0, sizeof(peer));
    peer.sin_family = AF_INET;
    peer.sin_port = htons((unsigned short)channel_port);
    peer.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    fds[0] = socket(AF_INET, SOCK_STREAM, 0);
    if (fds[0] < 0 || connect(fds[0], (struct sockaddr*)&peer, sizeof(peer)) != 0) {
      perror("connect");
      return 1;
    }
  } else if (pipe(fds)) {
    perror("pipe");
    return 1;
  }
  struct sigaction action;
  memset(&action, 0, sizeof(action));
  action.sa_handler = SIG_IGN;
  sigaction(SIGALRM, &action, NULL);
  pthread_t thread;
  if (pthread_create(&thread, NULL, ignored_waiter, NULL) != 0) {
    return 1;
  }
  sigset_t alarm;
  sigemptyset(&alarm);
  sigaddset(&alarm, SIGALRM);
  pthread_sigmask(SIG_BLOCK, &alarm, NULL);
  struct timespec settle = {0, 20000000};
  struct timespec later = {0, 50000000};
  nanosleep(&settle, NULL);
  int periodic = strcmp(mode, "ignperiodic") == 0;
  struct itimerval once;
  memset(&once, 0, sizeof(once));
  once.it_value.tv_usec = 10000;
  if (periodic) {
    once.it_interval.tv_usec = 10000;
    later.tv_nsec = 100000000;
  }
  setitimer(ITIMER_REAL, &once, NULL);
  int running = 0;
  int pending = 0;
  if (strcmp(mode, "igntpoll") != 0 && !channel) {
    nanosleep(&later, NULL);
    if (periodic) {
      struct itimerval now;
      getitimer(ITIMER_REAL, &now);
      running = now.it_value.tv_sec != 0 || now.it_value.tv_usec != 0;
      sigset_t queued;
      sigpending(&queued);
      pending = sigismember(&queued, SIGALRM);
    }
    char byte = 'x';
    if (write(fds[1], &byte, 1) != 1) {
      return 1;
    }
  }
  pthread_join(thread, NULL);
  if (periodic) {
    printf("mode=%s running=%d pending=%d eintr=%d n=%d\n", mode, running, pending,
           ignored_eintr, ignored_result);
  } else if (strcmp(mode, "igntpoll") == 0 || channel) {
    printf("mode=%s eintr=%d n=%d within_timeout=%d\n", mode, ignored_eintr, ignored_result,
           ignored_within_timeout);
  } else {
    printf("mode=%s eintr=%d n=%d\n", mode, ignored_eintr, ignored_result);
  }
  return 0;
}

/* The mixed* modes. */
static volatile pid_t mixed_waiter_tid;
static volatile pid_t usr1_handled_by;
static volatile int usr1_count;
static volatile int mixed_first;
static volatile int mixed_first_errno;
static volatile int mixed_eintr;

static void count_usr1(int sig) {
  (void)sig;
  usr1_handled_by = (pid_t)syscall(SYS_gettid);
  usr1_count++;
}

/* mixedignterm's SIGTERM, which only its waiter leaves unblocked. */
static volatile int term_count;
static int mixed_term_open;

static void count_term(int sig) {
  (void)sig;
  term_count++;
}

/* mixedtgkillusr2's SIGUSR2. */
static volatile int usr2_count;

static void count_usr2(int sig) {
  (void)sig;
  usr2_count++;
}

static void* mixed_waiter(void* arg) {
  (void)arg;
  mixed_waiter_tid = (pid_t)syscall(SYS_gettid);
  sigset_t open;
  sigemptyset(&open);
  sigaddset(&open, SIGALRM);
  sigaddset(&open, SIGUSR1);
  sigaddset(&open, SIGUSR2);
  if (mixed_term_open) {
    sigaddset(&open, SIGTERM);
  }
  pthread_sigmask(SIG_UNBLOCK, &open, NULL);
  int first = 1;
  for (;;) {
    struct pollfd wait_for = {.fd = fds[0], .events = POLLIN};
    int n = poll(&wait_for, 1, -1);
    int saved = errno;
    if (first) {
      mixed_first = n;
      mixed_first_errno = n < 0 ? saved : 0;
      first = 0;
    }
    if (n == -1 && saved == EINTR) {
      mixed_eintr++;
      continue;
    }
    break;
  }
  return NULL;
}

static int mixed_pending(const char* mode) {
  int taken = strcmp(mode, "mixedtaken") == 0;
  int usr2 = strcmp(mode, "mixedusr2") == 0;
  int shared = strcmp(mode, "mixedshared") == 0;
  int late = strcmp(mode, "mixedlate") == 0;
  int behind = strncmp(mode, "mixedbehind", 11) == 0;
  int no_restart = strcmp(mode, "mixedbehindnors") == 0;
  int tgkill_copy = strncmp(mode, "mixedtgkill", 11) == 0;
  int late_usr2 = strcmp(mode, "mixedtgkillusr2") == 0;
  int ignore_term = strcmp(mode, "mixedignterm") == 0;
  int ignore_late = strcmp(mode, "mixedignlate") == 0 || ignore_term;
  /* SIGALRM has the handler, and the other signal, if any, is ignored. */
  int alarm_caught = shared || behind || tgkill_copy || ignore_late;
  /* The POSIX timer's signal; where SIGALRM has the handler, it is the ignored
   * one. */
  int timer_signal = usr2 ? SIGUSR2 : SIGUSR1;
  if (pipe(fds)) {
    perror("pipe");
    return 1;
  }
  struct sigaction action;
  memset(&action, 0, sizeof(action));
  action.sa_handler = SIG_IGN;
  sigaction(alarm_caught ? timer_signal : SIGALRM, &action, NULL);
  action.sa_handler = count_usr1;
  action.sa_flags = no_restart ? 0 : SA_RESTART;
  sigaction(alarm_caught ? SIGALRM : timer_signal, &action, NULL);
  sigset_t blocked;
  sigemptyset(&blocked);
  sigaddset(&blocked, SIGALRM);
  if (!taken) {
    sigaddset(&blocked, timer_signal);
  }
  if (ignore_term) {
    action.sa_handler = count_term;
    action.sa_flags = SA_RESTART;
    sigaction(SIGTERM, &action, NULL);
    sigaddset(&blocked, SIGTERM);
    mixed_term_open = 1;
  }
  if (late_usr2) {
    action.sa_handler = count_usr2;
    action.sa_flags = SA_RESTART;
    sigaction(SIGUSR2, &action, NULL);
    sigaddset(&blocked, SIGUSR2);
  }
  pthread_sigmask(SIG_BLOCK, &blocked, NULL);
  pthread_t thread;
  if (pthread_create(&thread, NULL, mixed_waiter, NULL) != 0) {
    return 1;
  }
  struct timespec settle = {0, 20000000};
  nanosleep(&settle, NULL);
  if (tgkill_copy) {
    /* An ignored copy sent to the waiter alone, which leaves it unblocked:
     * Linux discards it when it is sent. */
    syscall(SYS_tgkill, getpid(), mixed_waiter_tid, SIGUSR1);
  } else if (!ignore_late || ignore_term) {
    struct sigevent event;
    memset(&event, 0, sizeof(event));
    event.sigev_notify = SIGEV_SIGNAL;
    event.sigev_signo = ignore_term ? SIGTERM : timer_signal;
    timer_t timer;
    if (timer_create(CLOCK_MONOTONIC, &event, &timer) != 0) {
      perror("timer_create");
      return 1;
    }
    struct itimerspec usr1_once;
    memset(&usr1_once, 0, sizeof(usr1_once));
    usr1_once.it_value.tv_nsec = late ? 11000000 : ignore_term ? 10000000 : 5000000;
    timer_settime(timer, 0, &usr1_once, NULL);
  }
  struct itimerval alarm_once;
  memset(&alarm_once, 0, sizeof(alarm_once));
  alarm_once.it_value.tv_usec = 10000;
  setitimer(ITIMER_REAL, &alarm_once, NULL);
  if (taken) {
    while (!usr1_count) {
      scheduled_call();
    }
  }
  if (usr2 || shared || ignore_late || late_usr2) {
    /* As the 10 ms expiry falls due, the leader unblocks the caught signal
     * and may take a copy of it from the process's queue, or, in
     * mixedignlate, ignores SIGALRM, which discards a pending copy, or, in
     * mixedtgkillusr2, sends SIGUSR2 to the waiter. */
    struct timespec ten = {0, 10000000};
    nanosleep(&ten, NULL);
    if (late_usr2) {
      syscall(SYS_tgkill, getpid(), mixed_waiter_tid, SIGUSR2);
    } else if (ignore_late) {
      action.sa_handler = SIG_IGN;
      sigaction(SIGALRM, &action, NULL);
    } else {
      sigset_t caught;
      sigemptyset(&caught);
      sigaddset(&caught, shared ? SIGALRM : SIGUSR2);
      pthread_sigmask(SIG_UNBLOCK, &caught, NULL);
    }
  }
  struct timespec later = {0, 50000000};
  nanosleep(&later, NULL);
  char byte = 'x';
  if (write(fds[1], &byte, 1) != 1) {
    return 1;
  }
  pthread_join(thread, NULL);
  printf("mode=%s first=%d errno=%s eintr=%d usr1=%d on_waiter=%d", mode, mixed_first,
         mixed_first_errno == EINTR ? "EINTR" : (mixed_first_errno ? "other" : "0"),
         mixed_eintr, usr1_count, usr1_handled_by == mixed_waiter_tid);
  if (ignore_term) {
    printf(" term=%d", term_count);
  }
  if (late_usr2) {
    printf(" usr2=%d", usr2_count);
  }
  printf("\n");
  return 0;
}

/* The ignmove and ignmoveblocked modes. */
static int move_a[2], move_a2[2], move_b[2];
static volatile pid_t move_tid_a, move_tid_b;
static volatile int move_eintr_a, move_eintr_b;

static void alarm_mask(int how) {
  sigset_t alarm;
  sigemptyset(&alarm);
  sigaddset(&alarm, SIGALRM);
  pthread_sigmask(how, &alarm, NULL);
}

/* Waits in poll(-1) for a byte on fd; an EINTR whose handler ran on this
 * thread ends the wait. */
static void move_wait(int fd, volatile int* eintr) {
  for (;;) {
    struct pollfd wait_for = {.fd = fd, .events = POLLIN};
    int n = poll(&wait_for, 1, -1);
    if (n == -1 && errno == EINTR) {
      (*eintr)++;
      if (handled_by == (pid_t)syscall(SYS_gettid)) {
        return;
      }
      continue;
    }
    char byte;
    if (read(fd, &byte, 1) != 1) {
      _exit(3);
    }
    return;
  }
}

static void* move_thread_a(void* arg) {
  (void)arg;
  move_tid_a = (pid_t)syscall(SYS_gettid);
  alarm_mask(SIG_BLOCK);
  move_wait(move_a[0], &move_eintr_a);
  alarm_mask(SIG_UNBLOCK);
  move_wait(move_a2[0], &move_eintr_a);
  return NULL;
}

static void* move_thread_b(void* arg) {
  (void)arg;
  move_tid_b = (pid_t)syscall(SYS_gettid);
  move_wait(move_b[0], &move_eintr_b);
  return NULL;
}

static void record_handler(int sig) {
  (void)sig;
  handled_by = (pid_t)syscall(SYS_gettid);
}

static int ignored_move(const char* mode) {
  int leader_blocks = strcmp(mode, "ignmoveblocked") == 0;
  if (pipe(move_a) || pipe(move_a2) || pipe(move_b)) {
    perror("pipe");
    return 1;
  }
  struct sigaction action;
  memset(&action, 0, sizeof(action));
  action.sa_handler = SIG_IGN;
  sigaction(SIGALRM, &action, NULL);
  pthread_t a, b;
  if (pthread_create(&a, NULL, move_thread_a, NULL) != 0 ||
      pthread_create(&b, NULL, move_thread_b, NULL) != 0) {
    return 1;
  }
  struct timespec settle = {0, 20000000};
  nanosleep(&settle, NULL);
  /* Copy 1: a POSIX timer's SIGALRM, ignored, while the leader waits on a
   * futex word, leaving SIGALRM unblocked (ignmove) or blocking it. */
  if (leader_blocks) {
    alarm_mask(SIG_BLOCK);
  }
  struct sigevent event;
  memset(&event, 0, sizeof(event));
  event.sigev_notify = SIGEV_SIGNAL;
  event.sigev_signo = SIGALRM;
  timer_t timer;
  if (timer_create(CLOCK_MONOTONIC, &event, &timer) != 0) {
    perror("timer_create");
    return 1;
  }
  struct itimerspec once;
  memset(&once, 0, sizeof(once));
  once.it_value.tv_nsec = 10000000;
  timer_settime(timer, 0, &once, NULL);
  static int word;
  struct timespec fifty = {0, 50000000};
  syscall(SYS_futex, &word, FUTEX_WAIT_PRIVATE, 0, &fifty, NULL, 0);
  /* Copy 2: the leader blocks SIGALRM and catches it, A unblocks it, and a
   * one-shot ITIMER_REAL expires. */
  alarm_mask(SIG_BLOCK);
  action.sa_handler = record_handler;
  sigaction(SIGALRM, &action, NULL);
  char byte = 'x';
  if (write(move_a[1], &byte, 1) != 1) {
    return 1;
  }
  struct timespec five = {0, 5000000};
  nanosleep(&five, NULL);
  struct itimerval alarm_once;
  memset(&alarm_once, 0, sizeof(alarm_once));
  alarm_once.it_value.tv_usec = 10000;
  setitimer(ITIMER_REAL, &alarm_once, NULL);
  while (!handled_by) {
    struct timespec millisecond = {0, 1000000};
    nanosleep(&millisecond, NULL);
  }
  if (write(move_a2[1], &byte, 1) != 1 || write(move_b[1], &byte, 1) != 1) {
    return 1;
  }
  pthread_join(a, NULL);
  pthread_join(b, NULL);
  printf("mode=%s handler_on=%s eintr_a=%d eintr_b=%d\n", mode,
         handled_by == move_tid_a ? "A" : (handled_by == move_tid_b ? "B" : "other"),
         move_eintr_a, move_eintr_b);
  return 0;
}

/* The sendign, sendcaught and sendlate modes. */
static int send_port;
static volatile int send_result;
static volatile int send_errno;
static volatile int send_armed;

static void* sender(void* arg) {
  (void)arg;
  alarm_mask(SIG_UNBLOCK);
  int fd = socket(AF_INET, SOCK_STREAM, 0);
  if (fd < 0) {
    perror("socket");
    _exit(1);
  }
  int small = 4096;
  setsockopt(fd, SOL_SOCKET, SO_SNDBUF, &small, sizeof(small));
  struct sockaddr_in peer;
  memset(&peer, 0, sizeof(peer));
  peer.sin_family = AF_INET;
  peer.sin_port = htons((unsigned short)send_port);
  peer.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
  if (connect(fd, (struct sockaddr*)&peer, sizeof(peer)) != 0) {
    perror("connect");
    _exit(1);
  }
  /* Fill the send buffer and the peer's window, which reads nothing yet. */
  static char chunk[4096];
  while (send(fd, chunk, sizeof(chunk), MSG_DONTWAIT | MSG_NOSIGNAL) > 0) {
  }
  struct itimerval alarm_once;
  memset(&alarm_once, 0, sizeof(alarm_once));
  alarm_once.it_value.tv_usec = 10000;
  setitimer(ITIMER_REAL, &alarm_once, NULL);
  send_armed = 1;
  char byte = 'x';
  ssize_t n = send(fd, &byte, 1, MSG_NOSIGNAL);
  send_result = (int)n;
  send_errno = n < 0 ? errno : 0;
  close(fd);
  return NULL;
}

static int blocked_send(const char* mode) {
  int caught = strcmp(mode, "sendcaught") == 0;
  int late = strcmp(mode, "sendlate") == 0;
  struct sigaction action;
  memset(&action, 0, sizeof(action));
  action.sa_handler = caught ? count_usr1 : SIG_IGN;
  sigaction(SIGALRM, &action, NULL);
  alarm_mask(SIG_BLOCK);
  pthread_t thread;
  if (pthread_create(&thread, NULL, sender, NULL) != 0) {
    return 1;
  }
  if (late) {
    /* As the sender's 10 ms expiry falls due, a handler without SA_RESTART
     * takes SIGALRM's place. */
    while (!send_armed) {
      scheduled_call();
    }
    struct timespec ten = {0, 10000000};
    nanosleep(&ten, NULL);
    action.sa_handler = count_usr1;
    sigaction(SIGALRM, &action, NULL);
  }
  pthread_join(thread, NULL);
  printf("mode=%s n=%d errno=%s handled=%d\n", mode, send_result,
         send_errno == EINTR ? "EINTR" : (send_errno ? "other" : "0"), usr1_count);
  return 0;
}

/* The dfljoin and dfljoinforever modes' worker. */
static int ticks_forever;

static void* ticker(void* arg) {
  (void)arg;
  sigset_t alarm;
  sigemptyset(&alarm);
  sigaddset(&alarm, SIGALRM);
  pthread_sigmask(SIG_BLOCK, &alarm, NULL);
  for (int tick = 0; ticks_forever || tick < 100; tick++) {
    struct timespec millisecond = {0, 1000000};
    nanosleep(&millisecond, NULL);
    printf("tick %d\n", tick);
    fflush(stdout);
  }
  return NULL;
}

/* The rcv* modes. */
static void* set_timeout_through_alias(void* arg) {
  (void)arg;
  struct timespec five = {0, 5000000};
  nanosleep(&five, NULL);
  int alias = dup(fds[0]);
  struct timeval second = {1, 0};
  setsockopt(alias, SOL_SOCKET, SO_RCVTIMEO, &second, sizeof(second));
  return NULL;
}

static int timed_receive(const char* mode) {
  if (socketpair(AF_UNIX, SOCK_STREAM, 0, fds)) {
    perror("socketpair");
    return 1;
  }
  int timed = strncmp(mode, "rcvtimeo", 8) == 0 || strcmp(mode, "rcvpartial") == 0;
  int alias = strcmp(mode, "rcvalias") == 0;
  int restart = alias || strstr(mode, "rst") != NULL;
  int partial = strcmp(mode, "rcvpartial") == 0;
  if (timed) {
    struct timeval second = {1, 0};
    setsockopt(fds[0], SOL_SOCKET, SO_RCVTIMEO, &second, sizeof(second));
  }
  struct sigaction action;
  memset(&action, 0, sizeof(action));
  action.sa_handler = wake;
  action.sa_flags = restart ? SA_RESTART : 0;
  sigaction(SIGALRM, &action, NULL);
  if (partial && write(fds[1], "abc", 3) != 3) {
    return 1;
  }
  pthread_t setter;
  if (alias && pthread_create(&setter, NULL, set_timeout_through_alias, NULL) != 0) {
    return 1;
  }
  struct itimerval once;
  memset(&once, 0, sizeof(once));
  once.it_value.tv_usec = 10000;
  setitimer(ITIMER_REAL, &once, NULL);
  char buffer[10];
  long result = recv(fds[0], buffer, partial ? sizeof(buffer) : 1, partial ? MSG_WAITALL : 0);
  int saved = errno;
  if (alias) {
    pthread_join(setter, NULL);
  }
  printf("mode=%s first=%ld errno=%s\n", mode, result,
         result == -1 ? (saved == EINTR ? "EINTR" : "other") : "0");
  return 0;
}

/* The acc* modes. */
static int timed_accept(const char* mode) {
  int listener = socket(AF_UNIX, SOCK_STREAM, 0);
  struct sockaddr_un address;
  memset(&address, 0, sizeof(address));
  address.sun_family = AF_UNIX;
  snprintf(address.sun_path + 1, sizeof(address.sun_path) - 1, "alarm-receiver-reach-%d",
           (int)getpid());
  socklen_t length = offsetof(struct sockaddr_un, sun_path) + 1 +
                     strlen(address.sun_path + 1);
  if (listener < 0 || bind(listener, (struct sockaddr*)&address, length) != 0 ||
      listen(listener, 1) != 0) {
    perror("listener");
    return 1;
  }
  int timed = strcmp(mode, "acctimeo") == 0;
  if (timed) {
    struct timeval second = {1, 0};
    setsockopt(listener, SOL_SOCKET, SO_RCVTIMEO, &second, sizeof(second));
  }
  struct sigaction action;
  memset(&action, 0, sizeof(action));
  action.sa_handler = count;
  action.sa_flags = timed ? SA_RESTART : 0;
  sigaction(SIGALRM, &action, NULL);
  struct itimerval once;
  memset(&once, 0, sizeof(once));
  once.it_value.tv_usec = 10000;
  setitimer(ITIMER_REAL, &once, NULL);
  long result = accept(listener, NULL, NULL);
  int saved = errno;
  printf("mode=%s first=%ld errno=%s\n", mode, result < 0 ? -1L : result,
         result == -1 ? (saved == EINTR ? "EINTR" : "other") : "0");
  return 0;
}

/* The dflfutex and dflfutexblocked modes. */
/* The one-shot 10 ms SIGALRM of the join, dfl* and p* modes: ITIMER_REAL, or a
 * POSIX timer (timer_create with SIGEV_SIGNAL and SIGALRM) in the p* modes. */
static int posix_one_shot;

static void arm_one_shot(void) {
  if (!posix_one_shot) {
    struct itimerval once;
    memset(&once, 0, sizeof(once));
    once.it_value.tv_usec = 10000;
    setitimer(ITIMER_REAL, &once, NULL);
    return;
  }
  struct sigevent event;
  memset(&event, 0, sizeof(event));
  event.sigev_notify = SIGEV_SIGNAL;
  event.sigev_signo = SIGALRM;
  timer_t timer;
  if (timer_create(CLOCK_MONOTONIC, &event, &timer) != 0) {
    perror("timer_create");
    _exit(1);
  }
  struct itimerspec once;
  memset(&once, 0, sizeof(once));
  once.it_value.tv_nsec = 10000000;
  timer_settime(timer, 0, &once, NULL);
}

static int fatal_futex(int blocked) {
  if (blocked) {
    sigset_t alarm;
    sigemptyset(&alarm);
    sigaddset(&alarm, SIGALRM);
    sigprocmask(SIG_BLOCK, &alarm, NULL);
  }
  arm_one_shot();
  static int word;
  struct timespec fifty = {0, 50000000};
  long result =
      syscall(SYS_futex, &word, FUTEX_WAIT_PRIVATE, 0, blocked ? &fifty : NULL, NULL, 0);
  if (result == -1 && errno == ETIMEDOUT) {
    printf("futex: timed out\n");
    return 0;
  }
  printf("futex: returned %ld errno %d\n", result, errno);
  return 1;
}

static int fatal_join(const char* mode) {
  ticks_forever = strcmp(mode, "dfljoinforever") == 0;
  pthread_t thread;
  if (pthread_create(&thread, NULL, ticker, NULL) != 0) {
    return 1;
  }
  arm_one_shot();
  pthread_join(thread, NULL);
  printf("joined: not killed\n");
  return 0;
}

/* The posix* modes. */
static volatile int posix_deliveries;
static volatile long worker_progress;
static volatile long each_worker_progress[3];
static volatile int stop_worker;

static void posix_count(int sig) {
  (void)sig;
  posix_deliveries++;
}

static void* posix_worker(void* arg) {
  long which = (long)arg;
  sigset_t alarm;
  sigemptyset(&alarm);
  sigaddset(&alarm, SIGALRM);
  pthread_sigmask(SIG_BLOCK, &alarm, NULL);
  while (!stop_worker) {
    worker_progress++;
    each_worker_progress[which]++;
    scheduled_call();
  }
  return NULL;
}

static int posix_timer(const char* mode) {
  int short_spin = strcmp(mode, "posixspin200") == 0;
  int spin = short_spin || strcmp(mode, "posixspin") == 0;
  int polls = strcmp(mode, "posixpoll") == 0;
  int deliveries = short_spin ? 200 : 300;
  int never_written[2];
  if (polls && pipe(never_written) != 0) {
    perror("pipe");
    return 1;
  }
  struct sigaction action;
  memset(&action, 0, sizeof(action));
  action.sa_handler = posix_count;
  sigaction(SIGALRM, &action, NULL);
  pthread_t workers[3];
  int count = spin || polls ? 3 : 1;
  for (int which = 0; which < count; which++) {
    if (pthread_create(&workers[which], NULL, posix_worker, (void*)(long)which) != 0) {
      return 1;
    }
  }
  struct sigevent event;
  memset(&event, 0, sizeof(event));
  event.sigev_notify = SIGEV_SIGNAL;
  event.sigev_signo = SIGALRM;
  timer_t timer;
  if (timer_create(CLOCK_MONOTONIC, &event, &timer) != 0) {
    perror("timer_create");
    return 1;
  }
  struct itimerspec every;
  memset(&every, 0, sizeof(every));
  every.it_value.tv_nsec = 100000;
  every.it_interval.tv_nsec = 100000;
  timer_settime(timer, 0, &every, NULL);
  /* posixspin200 counts from here, once the timer is armed. */
  long progress_at_arming = short_spin ? worker_progress : 0;
  long each_at_arming[3] = {0, 0, 0};
  for (int which = 0; short_spin && which < count; which++) {
    each_at_arming[which] = each_worker_progress[which];
  }
  long spun = 0;
  while (posix_deliveries < deliveries) {
    if (spin) {
      syscall(SYS_getppid);
      if (short_spin && ++spun % 20000 == 0) {
        scheduled_call();
      }
    } else if (polls) {
      struct pollfd wait_for = {.fd = never_written[0], .events = POLLIN};
      poll(&wait_for, 1, -1);
    } else {
      scheduled_call();
    }
  }
  /* Read before the disarm, whose return may let the workers run first. */
  long progress = worker_progress - progress_at_arming;
  long least = each_worker_progress[0] - each_at_arming[0];
  for (int which = 1; which < count; which++) {
    if (each_worker_progress[which] - each_at_arming[which] < least) {
      least = each_worker_progress[which] - each_at_arming[which];
    }
  }
  struct itimerspec off;
  memset(&off, 0, sizeof(off));
  timer_settime(timer, 0, &off, NULL);
  stop_worker = 1;
  for (int which = 0; which < count; which++) {
    pthread_join(workers[which], NULL);
  }
  if (short_spin) {
    printf("%s: worker progress before disarm %ld, least %ld\n", mode, progress, least);
  } else {
    printf("%s: worker progress before disarm %ld\n", mode, progress);
  }
  return 0;
}

/* The pollretry and retry* modes. */
static volatile int retry_handled;
static volatile int retry_on_waiter;
static __thread int is_retry_waiter;
static int retry_raw;
static volatile int retry_ready;
static volatile pid_t retry_waiter_tid;
static volatile long retry_armed_ms;
static int retry_result;
static int retry_eintr;
static long retry_end_ms;

/* SIGALRM's handler in these modes: it counts and makes no system call, so
 * nothing the tool sees comes between the interrupted poll and its retry. */
static void count_only(int sig) {
  (void)sig;
  retry_handled++;
  if (is_retry_waiter) {
    retry_on_waiter = 1;
  }
}

/* Polls fds[0], which nobody writes, for 100 ms, and again after each EINTR:
 * through libc's poll(), or with retry_raw the raw system call with zeros in
 * its unused argument registers. */
static void retry_loop(void) {
  is_retry_waiter = 1;
  retry_ready = 1;
  int n;
  for (;;) {
    struct pollfd wait_for = {.fd = fds[0], .events = POLLIN};
    n = retry_raw ? (int)syscall(SYS_poll, &wait_for, 1, 100, 0, 0, 0)
                  : poll(&wait_for, 1, 100);
    if (n == -1 && errno == EINTR) {
      retry_eintr++;
      continue;
    }
    break;
  }
  retry_result = n;
  retry_end_ms = monotonic_ms();
}

/* Arms the one-shot 50 ms ITIMER_REAL, noting when. */
static void retry_arm(void) {
  struct itimerval once;
  memset(&once, 0, sizeof(once));
  once.it_value.tv_usec = 50000;
  retry_armed_ms = monotonic_ms();
  setitimer(ITIMER_REAL, &once, NULL);
}

static void* retry_waiter(void* arg) {
  (void)arg;
  retry_waiter_tid = (pid_t)syscall(SYS_gettid);
  sigset_t open;
  sigemptyset(&open);
  sigaddset(&open, SIGALRM);
  sigaddset(&open, SIGUSR1);
  pthread_sigmask(SIG_UNBLOCK, &open, NULL);
  retry_loop();
  return NULL;
}

static int poll_retry(const char* mode) {
  if (pipe(fds)) {
    perror("pipe");
    return 1;
  }
  struct sigaction action;
  memset(&action, 0, sizeof(action));
  action.sa_handler = count_only; /* no SA_RESTART */
  sigaction(SIGALRM, &action, NULL);
  if (strncmp(mode, "pollretry", 9) == 0) {
    retry_raw = strcmp(mode, "pollretryraw") == 0;
    retry_arm();
    retry_loop();
  } else {
    /* The waiter polls with the raw call while the leader blocks SIGALRM and
     * SIGUSR1, which is ignored. */
    retry_raw = 1;
    action.sa_handler = SIG_IGN;
    sigaction(SIGUSR1, &action, NULL);
    sigset_t blocked;
    sigemptyset(&blocked);
    sigaddset(&blocked, SIGALRM);
    sigaddset(&blocked, SIGUSR1);
    pthread_sigmask(SIG_BLOCK, &blocked, NULL);
    pthread_t thread;
    if (pthread_create(&thread, NULL, retry_waiter, NULL) != 0) {
      return 1;
    }
    while (!retry_ready) {
      scheduled_call();
    }
    struct timespec settle = {0, 5000000};
    nanosleep(&settle, NULL);
    if (strcmp(mode, "retrybehind") == 0) {
      struct sigevent event;
      memset(&event, 0, sizeof(event));
      event.sigev_notify = SIGEV_SIGNAL;
      event.sigev_signo = SIGUSR1;
      timer_t timer;
      if (timer_create(CLOCK_MONOTONIC, &event, &timer) != 0) {
        perror("timer_create");
        return 1;
      }
      struct itimerspec usr1_once;
      memset(&usr1_once, 0, sizeof(usr1_once));
      usr1_once.it_value.tv_nsec = 5000000;
      timer_settime(timer, 0, &usr1_once, NULL);
    } else if (strcmp(mode, "retrytgkill") == 0) {
      syscall(SYS_tgkill, getpid(), retry_waiter_tid, SIGUSR1);
    }
    retry_arm();
    pthread_join(thread, NULL);
  }
  long elapsed = retry_end_ms - retry_armed_ms;
  printf("mode=%s n=%d eintr=%d handled=%d on_waiter=%d elapsed=%ld\n", mode, retry_result,
         retry_eintr, retry_handled, retry_on_waiter, (elapsed + 25) / 50 * 50);
  return 0;
}

int main(int argc, char** argv) {
  const char* mode = argc > 1 ? argv[1] : "poll";
  if (strcmp(mode, "twotimers") == 0) {
    return two_timers();
  }
  if (strncmp(mode, "posix", 5) == 0) {
    return posix_timer(mode);
  }
  if (strncmp(mode, "pollretry", 9) == 0 || strncmp(mode, "retry", 5) == 0) {
    return poll_retry(mode);
  }
  /* The p* modes are their ITIMER_REAL modes with a POSIX timer's expiry. */
  const char* printed = mode;
  if (strcmp(mode, "pjoin") == 0 || strncmp(mode, "pdfl", 4) == 0) {
    posix_one_shot = 1;
    mode++;
  }
  if (strncmp(mode, "ignmove", 7) == 0) {
    return ignored_move(mode);
  }
  if (strncmp(mode, "ign", 3) == 0) {
    channel_port = argc > 2 ? atoi(argv[2]) : 0;
    return ignored_queued(mode);
  }
  if (strncmp(mode, "send", 4) == 0) {
    send_port = argc > 2 ? atoi(argv[2]) : 0;
    return blocked_send(mode);
  }
  if (strncmp(mode, "mixed", 5) == 0) {
    return mixed_pending(mode);
  }
  if (strncmp(mode, "dfljoin", 7) == 0) {
    return fatal_join(mode);
  }
  if (strncmp(mode, "rcv", 3) == 0) {
    return timed_receive(mode);
  }
  if (strncmp(mode, "acc", 3) == 0) {
    return timed_accept(mode);
  }
  if (strncmp(mode, "dflfutex", 8) == 0) {
    return fatal_futex(strcmp(mode, "dflfutexblocked") == 0);
  }
  int lone = strncmp(mode, "lone", 4) == 0;
  int fatal = strcmp(mode, "dflepoll") == 0 || strcmp(mode, "lonedfl") == 0;
  int join = strcmp(mode, "join") == 0;
  if (strcmp(mode, "recv") == 0 ? socketpair(AF_UNIX, SOCK_STREAM, 0, fds) : pipe(fds)) {
    perror("pipe");
    return 1;
  }
  struct sigaction action;
  memset(&action, 0, sizeof(action));
  action.sa_handler = fatal ? SIG_DFL : wake; /* no SA_RESTART, but for rstpoll */
  if (strcmp(mode, "rstpoll") == 0) {
    action.sa_flags = SA_RESTART;
  }
  sigaction(SIGALRM, &action, NULL);
  pthread_t thread;
  if (join) {
    if (pthread_create(&thread, NULL, caller, NULL) != 0) {
      return 1;
    }
  } else if (!lone) {
    if (pthread_create(&thread, NULL, pauser, NULL) != 0) {
      return 1;
    }
  }
  struct timespec settle = {0, 20000000};
  nanosleep(&settle, NULL);
  arm_one_shot();
  int n = 0;
  int eintr = 0;
  char byte;
  if (join) {
    pthread_join(thread, NULL);
  } else if (strcmp(mode, "poll") == 0 || strcmp(mode, "lonepoll") == 0 ||
             strcmp(mode, "rstpoll") == 0) {
    struct pollfd wait_for = {.fd = fds[0], .events = POLLIN};
    while ((n = poll(&wait_for, 1, -1)) == -1 && errno == EINTR) {
      eintr++;
    }
  } else if (strcmp(mode, "recv") == 0) {
    while ((n = (int)recv(fds[0], &byte, 1, 0)) == -1 && errno == EINTR) {
      eintr++;
    }
  } else if (strcmp(mode, "select") == 0) {
    for (;;) {
      fd_set readable;
      FD_ZERO(&readable);
      FD_SET(fds[0], &readable);
      n = select(fds[0] + 1, &readable, NULL, NULL, NULL);
      if (n == -1 && errno == EINTR) {
        eintr++;
        continue;
      }
      break;
    }
  } else {
    int epoll = epoll_create1(0);
    struct epoll_event event = {.events = EPOLLIN, .data.fd = fds[0]};
    struct epoll_event ready;
    epoll_ctl(epoll, EPOLL_CTL_ADD, fds[0], &event);
    while ((n = epoll_wait(epoll, &ready, 1, -1)) == -1 && errno == EINTR) {
      eintr++;
    }
  }
  printf("mode=%s on_leader=%d eintr=%d n=%d\n", printed, handled_by == getpid(), eintr, n);
  return 0;
}
