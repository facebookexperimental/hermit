/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Which thread of a multithreaded process takes a process-directed signal
 * that the kernel itself generates: a child's exit SIGCHLD, an ITIMER_REAL
 * expiry and a POSIX timer's SIGEV_SIGNAL expiry (SIGSHARE S3b). Linux
 * queues such a signal on the process's shared queue and complete_signal
 * (kernel/signal.c) wakes the thread it names, if that thread wants it, and
 * otherwise the first thread that does, walking the thread list from the
 * process's curr_target and leaving curr_target at the thread it chose; with
 * none, the signal waits until a thread unblocks it. A thread wants it
 * (wants_signal) when it does not block it - a thread waiting in
 * sigtimedwait() for it does not, since do_sigtimedwait unblocks the waited
 * set - and it is running or has no signal pending. The thread named is the
 * child's parent thread for SIGCHLD (the thread that forked it,
 * do_notify_parent), and the leader for a timer.
 *
 * The process has the main thread M and two workers, W1 and W2, created in
 * that order. Each worker waits in read() on a pipe of its own, and M, when
 * it leaves the signal unblocked, in poll() on its own (bounded at 200 ms),
 * so no waiting thread has a signal pending and Linux would wake it; the
 * handler (no SA_RESTART) writes its thread's name to a report pipe, which M
 * reads once the event is over. Each mode prints one line, the same natively
 * and under Hermit.
 *
 * Every boundary the line depends on is reached causally, not by elapsed
 * time, so a native run has one outcome: a thread is known to wait in its
 * call when /proc shows it sleeping there (await_waiting: state S, or D in a
 * vfork wait, and the syscall number in /proc/<pid>/task/<tid>/syscall,
 * which must be readable where it exists); the child exits only once its
 * creator thread waits; a timer is
 * armed by a worker that does not decide the outcome, only once M waits, and
 * M waits for the arming, its expiry and, in a queued mode, the signal's
 * pending state (getitimer, timer_gettime); a SIGCHLD has been generated
 * once waitid(WNOWAIT) sees the child's exit; a handler has run once its
 * report is readable; a signal taken by sigtimedwait() has no handler left
 * to run; and a worker that unblocks a pending signal, or returns from a
 * vfork wait, has taken what Linux gave it before it acknowledges on the
 * done pipe, since the kernel delivers it on the return from the call.
 *
 * Each such boundary is a prerequisite and is required: one that is not
 * reached within its bound (a thread that never waits, an unreadable syscall
 * file, a timer that never expires, a queued signal that never shows, a
 * steering signal no thread takes, a child that never saw its creator wait)
 * ends the run with "unconfirmed: ..." on stderr and status 3, and no line.
 * What the line reports is an outcome, read once the event is over: whether
 * a handler ran is read with a 2 s bound, and one that ran later reads as
 * none. So a bound can turn Linux's line into another, never another into
 * Linux's: each Linux line that reports an absence has a positive
 * observation beside it (waited=1, generated=1, or the handlers that ran up
 * to the end of the run, read after every worker has ended: busy=, and
 * final= in the coalesced, suppressed and default modes). One boundary is weaker
 * under Hermit: its /proc has no syscall file (ENOENT), and it shows a thread
 * it holds as sleeping wherever the thread stopped, so there await_waiting
 * confirms that the thread stopped, not that it stopped in its call. Hermit's
 * line is then evidence only by being compared with Linux's: a boundary
 * passed early can make it differ, which the comparison reports. A vfork
 * wait is confirmed regardless: the vfork child acknowledges, and it runs
 * only once its parent sleeps in clone().
 *
 *   chld-creator  SIGCHLD caught and unblocked everywhere; M forks the child
 *                 and waits in poll(): M is named and takes it.
 *                 "receiver=M interrupted=M".
 *   chld-walk     the same, with M and W1 blocking SIGCHLD: the walk from
 *                 curr_target (M) passes W1 and reaches W2.
 *                 "receiver=W2 interrupted=W2".
 *   chld-worker   W1 forks the child and then waits in read(); every thread
 *                 leaves SIGCHLD unblocked: W1, the child's parent thread, is
 *                 named and takes it. "receiver=W1 interrupted=W1".
 *   chld-queued   every thread blocks SIGCHLD: it stays pending in the shared
 *                 queue (M's sigpending shows it) until W2 unblocks it, and
 *                 W2 takes it. "pending=1 receiver=W2".
 *   alrm-leader   a one-shot ITIMER_REAL with SIGALRM caught and unblocked
 *                 everywhere: the leader M is named and takes it.
 *                 "receiver=M interrupted=M".
 *   alrm-walk     the same, with M and W1 blocking SIGALRM: W2 takes it.
 *                 "receiver=W2 interrupted=W2".
 *   alrm-queued   every thread blocks SIGALRM: it stays pending until W1
 *                 unblocks it, and W1 takes it. "pending=1 receiver=W1".
 *   posix-walk    a one-shot CLOCK_MONOTONIC POSIX timer whose SIGEV_SIGNAL
 *                 is SIGUSR1, caught; M and W1 block SIGUSR1: W2 takes it.
 *                 "receiver=W2 interrupted=W2".
 *   alrm-ignored-blocked
 *                 SIGALRM ignored and blocked in every thread: Linux queues an
 *                 ignored signal that is blocked (sig_ignored), so after the
 *                 one-shot expiry it is pending; M then installs a handler and
 *                 W2 unblocks it and takes it. "pending=1 receiver=W2".
 *   alrm-ignored-unblocked
 *                 SIGALRM ignored, and unblocked in W2 alone, which sleeps in
 *                 a vfork wait (as in the busy modes). The leader blocks it,
 *                 so Linux queues it (sig_ignored) and gives it to W2, which
 *                 cannot take it there: M sees it pending (generated). Once
 *                 released, W2 takes it on its return from clone() and, as
 *                 it is ignored, discards it, so nothing is pending; M then
 *                 installs a handler and W2 takes nothing.
 *                 "generated=1 pending=0 receiver=none".
 *   chld-curr-target, alrm-curr-target, posix-curr-target
 *                 the walk starts at curr_target, not at the first thread. M
 *                 blocks the signal, W1 blocks it at first and W2 does not. A
 *                 first, steering signal that only W2 leaves unblocked (an
 *                 ITIMER_REAL SIGALRM for chld-, a child's SIGCHLD for the
 *                 others) walks to W2 and leaves curr_target there; W1 then
 *                 unblocks the signal, and its walk starts at W2, which takes
 *                 it (a walk from M would reach W1 first).
 *                 "receiver=W2 interrupted=W2".
 *   chld-sigwait, alrm-sigwait, posix-sigwait
 *                 M, the thread named, blocks the signal but waits for it in
 *                 sigtimedwait() (bounded at 2 s); W1 leaves it unblocked and
 *                 W2 blocks it. M's wait unblocks it, so M wants it, is named
 *                 and takes it in its wait, and no handler runs.
 *                 "waited=1 receiver=none interrupted=none".
 *   chld-busy, alrm-busy, posix-busy
 *                 a thread with a signal pending that is not running does not
 *                 want another. W1 and W2 leave the signal unblocked and M
 *                 blocks it; W1 makes a CLONE_VFORK child (no exit signal)
 *                 that waits for M, and so sleeps in its vfork wait, which no
 *                 caught signal ends, and M sends W1 a caught SIGUSR2 there.
 *                 The walk from M passes busy W1 and reaches W2; once the
 *                 vfork child exits W1 takes its SIGUSR2. busy= names every
 *                 handler that ran after the release.
 *                 "receiver=W2 interrupted=W2 busy=W1".
 *   ignored-busy  chld-busy with SIGWINCH ignored and sent to W1 instead of
 *                 the caught SIGUSR2: W1 does not block it, so Linux discards
 *                 it as it is sent and W1 has nothing pending. The walk from M
 *                 reaches W1, which takes the SIGCHLD once its vfork child
 *                 exits. "receiver=none interrupted=none busy=W1".
 *   coalesced-child-curr
 *                 a SIGCHLD that merges into one already pending moves no
 *                 curr_target. M blocks SIGCHLD and SIGALRM; W1 leaves SIGCHLD
 *                 unblocked, blocks SIGALRM and sleeps in a vfork wait; W2
 *                 blocks SIGCHLD and leaves SIGALRM unblocked. A first child's
 *                 SIGCHLD walks to W1, which cannot take it there; a SIGALRM
 *                 walks to W2 (steer=) and leaves curr_target there; a second
 *                 child's SIGCHLD finds the first pending (legacy_queue) and
 *                 reaches no complete_signal. Released, W1 takes the SIGCHLD
 *                 (chld=) and unblocks SIGALRM, and a final SIGALRM, which W1
 *                 and W2 both want, walks from W2 to W2 (final=). chld= names
 *                 the handlers that ran up to W1's acknowledgement.
 *                 "steer=W2 chld=W1 final=W2". A known ptrace limit: once a
 *                 thread of the process has waited outside the schedule with
 *                 SIGCHLD unblocked (an rt_sigsuspend, say), ptrace no longer
 *                 sees the merge, and an earlier such wait makes this mode
 *                 end final=W1 there, before SIGSHARE S3b as after it
 *                 (dh-yvwnk). This mode has no such wait.
 *   suppressed-child-curr
 *                 an explicitly ignored SIGCHLD moves no curr_target: Linux's
 *                 do_notify_parent sends nothing for it, whatever the masks.
 *                 SIGCHLD is ignored; M and W2 block it and W1 does not; W2
 *                 alone leaves SIGALRM unblocked. A SIGALRM walks to W2
 *                 (steer=). M forks a child that waits at a gate and sleeps in
 *                 a futex wait; W2 releases the child, sees it reaped, and
 *                 wakes M. Nothing is pending (pending=). M installs a
 *                 handler and W1 unblocks SIGALRM, and a final SIGALRM walks
 *                 from W2 to W2 (final=). revastra2's suppressed-child-curr.c.
 *                 "steer=W2 pending=0 final=W2".
 *   default-child-curr
 *                 suppressed-child-curr's positive control: SIGCHLD stays
 *                 SIG_DFL, whose action is to ignore it. do_notify_parent
 *                 sends it, and M, the thread named, blocks it, so it is not
 *                 ignored as sent (sig_ignored) and Linux queues it;
 *                 complete_signal walks from W2 to W1 and leaves curr_target
 *                 there. W2 sees the child's exit (waitid(WNOWAIT)) and wakes
 *                 M, which reaps the child. W1, given the SIGCHLD, discards
 *                 it as it dequeues it, which does not move curr_target
 *                 back: nothing is pending once W1 acknowledges (pending=).
 *                 W1 unblocks SIGALRM, and a final SIGALRM walks from W1 to
 *                 W1 (final=). revastra2's default-child-curr.c.
 *                 "steer=W2 pending=0 final=W1".
 *   chld-ppoll-pending-temp
 *                 M waits in ppoll() with SIGCHLD unblocked in both its
 *                 own mask and the call's temporary mask; the temporary
 *                 mask blocks SIGUSR2, which M's own mask admits. W2 posts
 *                 a private SIGUSR2 to M once M waits, so it stays pending
 *                 for M alone, and the child exits only once it sees that
 *                 copy pending. Linux's wants_signal reads the installed
 *                 (temporary) mask for both halves, so the blocked copy is
 *                 not pending work and M, the thread named, takes the
 *                 SIGCHLD in its call. "receiver=M interrupted=M".
 *   chld-ppoll-lets-in, chld-pselect-lets-in, chld-suspend-lets-in
 *                 chld-creator with M waiting in ppoll(), pselect() or
 *                 sigsuspend() instead of poll(): M's own mask blocks
 *                 SIGCHLD, and the call's temporary mask lets it in. Linux
 *                 installs that mask for the call (set_user_sigmask,
 *                 sigsuspend), and wants_signal reads the installed one, so M
 *                 is named, wants it and takes it in its call.
 *                 "receiver=M interrupted=M".
 *   chld-ppoll-keeps-out, chld-pselect-keeps-out, chld-suspend-keeps-out
 *                 the inverse: M's own mask lets SIGCHLD in, and the call's
 *                 temporary mask blocks it, so M does not want it and the
 *                 walk from M reaches W1. "receiver=W1 interrupted=W1".
 *                 A sigsuspend() has no bound of its own: W2 waits for a
 *                 handler's report (bounded at 2 s) and then ends M's wait
 *                 with a SIGUSR2 that only M's temporary mask lets in, whose
 *                 handler reports nothing; M's own mask blocks SIGUSR2, so
 *                 a SIGUSR2 that arrives after M's call stays pending, and
 *                 interrupted= names M only if M's SIGCHLD handler ran. W2
 *                 waits in poll() there, not read(), which neither line
 *                 depends on: Linux's walk from M reaches W1 first.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <linux/futex.h>
#include <poll.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/select.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

enum { M, W1, W2, THREADS };
static const char* const NAMES[THREADS] = {"M", "W1", "W2"};

/* What generates a mode's signal. */
enum source { CHILD_EXIT, ITIMER, POSIX_TIMER };

/* The call M waits in, in a TAKEN mode where it leaves the signal unblocked
 * (chld-*-lets-in, chld-*-keeps-out: its temporary mask and M's own mask
 * differ about the signal). */
enum call { POLL, PPOLL, PSELECT, SUSPEND };

/* How M waits for it and what the mode prints. */
enum shape {
  /* M blocks the signal, or waits in poll() with it unblocked:
   * "receiver=R interrupted=I". */
  TAKEN,
  /* every thread blocks it until one worker unblocks it:
   * "pending=P receiver=R". */
  QUEUED,
  /* M waits in sigtimedwait(): "waited=W receiver=R interrupted=I". */
  SIGWAIT,
  /* W1 is busy in its vfork wait: "receiver=R interrupted=I busy=B". */
  BUSY,
  /* W2 is held in its vfork wait while an ignored signal is generated:
   * "generated=G pending=P receiver=R". */
  HELD,
  /* coalesced-child-curr: "steer=S chld=C final=F". */
  COALESCED,
  /* suppressed-child-curr: "steer=S pending=P final=F". */
  SUPPRESSED,
  /* default-child-curr: "steer=S pending=P final=F". */
  DEFAULTED,
};

struct mode {
  const char* name;
  enum source source;
  enum shape shape;
  /* Whether each thread blocks the signal; W1's block in a curr-target mode
   * is lifted after the steering signal. */
  int blocks[THREADS];
  /* The forking thread for a child exit; the worker that unblocks a queued
   * signal; unused otherwise. */
  int actor;
  /* Ignore the signal until it has been generated. */
  int ignored;
  /* Steer curr_target to W2 first with this source's signal (curr-target
   * modes). */
  int steer;
  enum source steer_source;
  /* In a busy mode, the signal pending for W1 is an ignored SIGWINCH, which
   * Linux discards as it is sent, instead of a caught SIGUSR2. */
  int busy_ignored;
  /* The call M waits in; with a temporary mask (not POLL), whether that mask
   * blocks the signal (M's own then lets it in) or lets it in (M's own then
   * blocks it). */
  enum call call;
  int temporary_blocks;
  /* chld-ppoll-pending-temp: the temporary mask also blocks SIGUSR2, which
   * W2 posts privately to M once M waits and the child confirms pending. */
  int pending_temp;
};

static const struct mode MODES[] = {
    {"chld-creator", CHILD_EXIT, TAKEN, {0, 0, 0}, M, 0, 0, 0, 0, POLL, 0, 0},
    {"chld-walk", CHILD_EXIT, TAKEN, {1, 1, 0}, M, 0, 0, 0, 0, POLL, 0, 0},
    {"chld-worker", CHILD_EXIT, TAKEN, {0, 0, 0}, W1, 0, 0, 0, 0, POLL, 0, 0},
    {"chld-queued", CHILD_EXIT, QUEUED, {1, 1, 1}, W2, 0, 0, 0, 0, POLL, 0, 0},
    {"alrm-leader", ITIMER, TAKEN, {0, 0, 0}, M, 0, 0, 0, 0, POLL, 0, 0},
    {"alrm-walk", ITIMER, TAKEN, {1, 1, 0}, M, 0, 0, 0, 0, POLL, 0, 0},
    {"alrm-queued", ITIMER, QUEUED, {1, 1, 1}, W1, 0, 0, 0, 0, POLL, 0, 0},
    {"posix-walk", POSIX_TIMER, TAKEN, {1, 1, 0}, M, 0, 0, 0, 0, POLL, 0, 0},
    {"alrm-ignored-blocked", ITIMER, QUEUED, {1, 1, 1}, W2, 1, 0, 0, 0, POLL, 0, 0},
    {"alrm-ignored-unblocked", ITIMER, HELD, {1, 1, 0}, W2, 1, 0, 0, 0, POLL, 0, 0},
    {"chld-curr-target", CHILD_EXIT, TAKEN, {1, 1, 0}, M, 0, 1, ITIMER, 0, POLL, 0, 0},
    {"alrm-curr-target", ITIMER, TAKEN, {1, 1, 0}, M, 0, 1, CHILD_EXIT, 0, POLL, 0, 0},
    {"posix-curr-target", POSIX_TIMER, TAKEN, {1, 1, 0}, M, 0, 1, CHILD_EXIT, 0, POLL, 0, 0},
    {"chld-sigwait", CHILD_EXIT, SIGWAIT, {1, 0, 1}, M, 0, 0, 0, 0, POLL, 0, 0},
    {"alrm-sigwait", ITIMER, SIGWAIT, {1, 0, 1}, M, 0, 0, 0, 0, POLL, 0, 0},
    {"posix-sigwait", POSIX_TIMER, SIGWAIT, {1, 0, 1}, M, 0, 0, 0, 0, POLL, 0, 0},
    {"chld-busy", CHILD_EXIT, BUSY, {1, 0, 0}, M, 0, 0, 0, 0, POLL, 0, 0},
    {"alrm-busy", ITIMER, BUSY, {1, 0, 0}, M, 0, 0, 0, 0, POLL, 0, 0},
    {"posix-busy", POSIX_TIMER, BUSY, {1, 0, 0}, M, 0, 0, 0, 0, POLL, 0, 0},
    {"coalesced-child-curr", CHILD_EXIT, COALESCED, {1, 0, 1}, M, 0, 1, ITIMER, 0, POLL, 0, 0},
    {"ignored-busy", CHILD_EXIT, BUSY, {1, 0, 0}, M, 0, 0, 0, 1, POLL, 0, 0},
    {"suppressed-child-curr", CHILD_EXIT, SUPPRESSED, {1, 0, 1}, M, 1, 1, ITIMER, 0, POLL, 0, 0},
    {"default-child-curr", CHILD_EXIT, DEFAULTED, {1, 0, 1}, M, 0, 1, ITIMER, 0, POLL, 0, 0},
    {"chld-ppoll-lets-in", CHILD_EXIT, TAKEN, {0, 0, 0}, M, 0, 0, 0, 0, PPOLL, 0, 0},
    {"chld-ppoll-keeps-out", CHILD_EXIT, TAKEN, {0, 0, 0}, M, 0, 0, 0, 0, PPOLL, 1, 0},
    {"chld-pselect-lets-in", CHILD_EXIT, TAKEN, {0, 0, 0}, M, 0, 0, 0, 0, PSELECT, 0, 0},
    {"chld-pselect-keeps-out", CHILD_EXIT, TAKEN, {0, 0, 0}, M, 0, 0, 0, 0, PSELECT, 1, 0},
    {"chld-suspend-lets-in", CHILD_EXIT, TAKEN, {0, 0, 0}, M, 0, 0, 0, 0, SUSPEND, 0, 0},
    {"chld-suspend-keeps-out", CHILD_EXIT, TAKEN, {0, 0, 0}, M, 0, 0, 0, 0, SUSPEND, 1, 0},
    {"chld-ppoll-pending-temp", CHILD_EXIT, TAKEN, {0, 0, 0}, M, 0, 0, 0, 0, PPOLL, 0, 1},
};

static const struct mode* mode;
static __thread int self = M;
static pid_t tids[THREADS];
static int wake_pipes[THREADS][2];
static int report_pipe[2];
static int done_pipe[2];
static int vfork_gate[2];
static int interrupted_by = -1;
static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t cond = PTHREAD_COND_INITIALIZER;
static int ready;
static int fork_now;
static pid_t forked;
static pid_t creator_tid;
static timer_t timer;
/* suppressed-child-curr and default-child-curr: the gated child, the gate,
 * and M's futex word. */
static pid_t gated_child;
static int child_gate[2];
static int creator_release;
static char vfork_stack[64 * 1024] __attribute__((aligned(16)));
static pthread_t threads_of_main[THREADS];

static void die(const char* what) {
  fprintf(stderr, "%s: %s\n", what, strerror(errno));
  _exit(99);
}

/* A boundary the line depends on was not reached: the run ends here, with no
 * line, rather than go on from an unconfirmed state. */
static void unconfirmed(const char* what) {
  fprintf(stderr, "unconfirmed: %s\n", what);
  _exit(3);
}

static int signal_of(enum source source) {
  return source == CHILD_EXIT ? SIGCHLD : source == ITIMER ? SIGALRM : SIGUSR1;
}

/* Whether this thread's handler for the mode's signals ran (sigsuspend modes:
 * M's call also ends for the SIGUSR2 that bounds it). */
static __thread volatile sig_atomic_t took_signal;

static void on_signal(int sig) {
  (void)sig;
  int saved = errno;
  took_signal = 1;
  char who = (char)self;
  if (write(report_pipe[1], &who, 1) != 1) {
    _exit(98);
  }
  errno = saved;
}

/* The SIGUSR2 that ends a sigsuspend mode's wait: it reports nothing. */
static void on_wake(int sig) {
  (void)sig;
}

static void set_action(int sig, void (*handler)(int)) {
  struct sigaction action;
  memset(&action, 0, sizeof(action));
  action.sa_handler = handler;
  if (sigaction(sig, &action, NULL) != 0) {
    die("sigaction");
  }
}

static void mask(int how, int sig) {
  sigset_t set;
  sigemptyset(&set);
  sigaddset(&set, sig);
  if (pthread_sigmask(how, &set, NULL) != 0) {
    die("pthread_sigmask");
  }
}

static void sleep_ms(long ms) {
  struct timespec ts = {ms / 1000, (ms % 1000) * 1000000L};
  while (nanosleep(&ts, &ts) != 0 && errno == EINTR) {
  }
}

/* Whether task tid of process pid sleeps in syscall sysno (any when < 0),
 * as /proc shows it: state S (D in a vfork wait; t or T for a thread a
 * tracer holds), and the syscall number, which must be readable where the
 * file exists. */
static int waits_in(pid_t pid, pid_t tid, long sysno) {
  char path[64];
  char buf[512];
  snprintf(path, sizeof(path), "/proc/%d/task/%d/stat", (int)pid, (int)tid);
  FILE* file = fopen(path, "r");
  if (!file) {
    return 0;
  }
  char* line = fgets(buf, sizeof(buf), file);
  fclose(file);
  char* paren = line ? strrchr(buf, ')') : NULL;
  if (!paren || !strchr("SDtT", paren[2])) {
    return 0;
  }
  if (sysno < 0) {
    return 1;
  }
  snprintf(path, sizeof(path), "/proc/%d/task/%d/syscall", (int)pid, (int)tid);
  file = fopen(path, "r");
  if (!file && errno == ENOENT) {
    /* Hermit's /proc has no syscall file (the header's rule). */
    return 1;
  }
  if (!file) {
    unconfirmed("a waiting thread's /proc syscall file is unreadable");
  }
  line = fgets(buf, sizeof(buf), file);
  fclose(file);
  if (!line) {
    unconfirmed("a waiting thread's /proc syscall file is empty");
  }
  return strtol(buf, NULL, 10) == sysno;
}

/* Wait until task tid of process pid sleeps in syscall sysno (waits_in),
 * bounded at 10 s, past which the run ends unconfirmed. */
static void await_waiting(pid_t pid, pid_t tid, long sysno) {
  for (int i = 0; !waits_in(pid, tid, sysno); i++) {
    if (i == 10000) {
      unconfirmed("a thread did not wait in its call");
    }
    sleep_ms(1);
  }
}

/* Whether fd became readable within 2 s. */
static int await_readable(int fd) {
  struct pollfd pfd = {fd, POLLIN, 0};
  int n;
  while ((n = poll(&pfd, 1, 2000)) < 0 && errno == EINTR) {
  }
  return n > 0;
}

static pid_t fork_child(void) {
  creator_tid = gettid();
  pid_t child = fork();
  if (child < 0) {
    die("fork");
  }
  if (child == 0) {
    /* Exit only once the thread that forked it waits (status 3 if it never
     * did: unconfirmed). */
    await_waiting(getppid(), creator_tid, -1);
    _exit(0);
  }
  return child;
}

/* chld-ppoll-pending-temp's child: exits only once SIGUSR2 is pending for
 * its creator alone, as /proc shows it, so the private copy W2 posted is
 * in place before the SIGCHLD exists (status 3 if it never is: the run
 * ends unconfirmed at the reap). */
static pid_t fork_pending_child(void) {
  creator_tid = gettid();
  pid_t child = fork();
  if (child < 0) {
    die("fork");
  }
  if (child == 0) {
    char path[64];
    snprintf(path, sizeof(path), "/proc/%d/task/%d/status", (int)getppid(),
             (int)creator_tid);
    for (int i = 0; i < 10000; i++) {
      FILE* file = fopen(path, "r");
      if (file) {
        char buf[1024];
        while (fgets(buf, sizeof(buf), file)) {
          unsigned long pending = 0;
          if (sscanf(buf, "SigPnd: %lx", &pending) == 1 &&
              (pending & (1UL << (SIGUSR2 - 1)))) {
            fclose(file);
            _exit(0);
          }
        }
        fclose(file);
      }
      sleep_ms(1);
    }
    _exit(3);
  }
  return child;
}

/* Wait until child has exited, without reaping it, and require that it
 * confirmed its creator's wait (its status). */
static void await_child_exit(pid_t child) {
  siginfo_t info;
  memset(&info, 0, sizeof(info));
  while (waitid(P_PID, child, &info, WEXITED | WNOWAIT) != 0) {
    if (errno != EINTR) {
      die("waitid");
    }
  }
  if (info.si_code != CLD_EXITED || info.si_status != 0) {
    unconfirmed("a child did not see its creator wait");
  }
}

/* Reap child and require that it confirmed its creator's wait. */
static void reap(pid_t child) {
  int status;
  if (waitpid(child, &status, 0) != child) {
    die("waitpid");
  }
  if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) {
    unconfirmed("a child did not see its creator wait");
  }
}

/* Arm a one-shot timer (20 ms), from whichever thread calls it: ITIMER_REAL
 * and a SIGEV_SIGNAL POSIX timer belong to the process. */
static void arm_timer(enum source source) {
  if (source == POSIX_TIMER) {
    struct itimerspec spec = {{0, 0}, {0, 20 * 1000000L}};
    if (timer_settime(timer, 0, &spec, NULL) != 0) {
      die("timer_settime");
    }
  } else {
    struct itimerval value = {{0, 0}, {0, 20000}};
    if (setitimer(ITIMER_REAL, &value, NULL) != 0) {
      die("setitimer");
    }
  }
}

/* Wait until a one-shot timer has expired, bounded at 10 s, past which the
 * run ends unconfirmed. */
static void await_expiry(enum source source) {
  for (int i = 0; i < 10000; i++) {
    if (source == POSIX_TIMER) {
      struct itimerspec spec;
      if (timer_gettime(timer, &spec) != 0) {
        die("timer_gettime");
      }
      if (spec.it_value.tv_sec == 0 && spec.it_value.tv_nsec == 0) {
        return;
      }
    } else {
      struct itimerval value;
      if (getitimer(ITIMER_REAL, &value) != 0) {
        die("getitimer");
      }
      if (value.it_value.tv_sec == 0 && value.it_value.tv_usec == 0) {
        return;
      }
    }
    sleep_ms(1);
  }
  unconfirmed("the timer did not expire");
}

static int shared_pending(int sig) {
  sigset_t set;
  if (sigpending(&set) != 0) {
    die("sigpending");
  }
  return sigismember(&set, sig);
}

/* Whether sig's pending state became `want` within `ms`. */
static int await_pending(int sig, int want, int ms) {
  for (int i = 0; shared_pending(sig) != want; i++) {
    if (i == ms) {
      return 0;
    }
    sleep_ms(1);
  }
  return 1;
}

/* Wait in read() on this thread's own pipe for a command byte; a handler
 * that interrupts the read is noted. */
static char wait_for_command(void) {
  char byte;
  for (;;) {
    ssize_t n = read(wake_pipes[self][0], &byte, 1);
    if (n == 1) {
      return byte;
    }
    if (n < 0 && errno == EINTR) {
      pthread_mutex_lock(&lock);
      if (interrupted_by < 0) {
        interrupted_by = self;
      }
      pthread_mutex_unlock(&lock);
      continue;
    }
    die("read");
  }
}

static void command(int thread, char byte) {
  if (write(wake_pipes[thread][1], &byte, 1) != 1) {
    die("write");
  }
}

static void acknowledge(void) {
  char byte = 'd';
  if (write(done_pipe[1], &byte, 1) != 1) {
    die("write");
  }
}

static void await_acknowledgement(void) {
  char byte;
  if (read(done_pipe[0], &byte, 1) != 1) {
    die("read");
  }
}

/* A system call made without the C library, for the CLONE_VFORK child. */
static long raw_syscall3(long number, long a, long b, long c) {
  long result;
  __asm__ volatile("syscall"
                   : "=a"(result)
                   : "a"(number), "D"(a), "S"(b), "d"(c)
                   : "rcx", "r11", "memory");
  return result;
}

/* A CLONE_VFORK child shares its parent's memory, so it touches nothing of
 * the C library's (not even a lazily bound symbol): it acknowledges, waits
 * for M's byte, and exits. */
static int vfork_child(void* arg) {
  (void)arg;
  char byte = 'd';
  raw_syscall3(SYS_write, done_pipe[1], (long)&byte, 1);
  raw_syscall3(SYS_read, vfork_gate[0], (long)&byte, 1);
  raw_syscall3(SYS_exit, 0, 0, 0);
  return 0;
}

/* The syscall M waits in while a worker arms the timer, or -1 when M blocks
 * the signal and need not be waiting. */
static long main_wait_syscall(void) {
  if (mode->shape == SIGWAIT) {
    return SYS_rt_sigtimedwait;
  }
  return mode->blocks[M] ? -1 : SYS_poll;
}

/* Each worker sets its own mask (a thread's mask is its own), reports ready,
 * and waits for commands:
 *   'a' arm the mode's timer once M waits (main_wait_syscall), acknowledge;
 *   's' arm the steering ITIMER_REAL, acknowledge;
 *   'n' acknowledge;
 *   'u' unblock the signal and acknowledge (a pending copy is taken before
 *       pthread_sigmask returns);
 *   'v' make the CLONE_VFORK child, which acknowledges, and wait for it;
 *       acknowledge once it exited and any signal pending was taken;
 *   'k' end M's sigsuspend() once a handler reported (sigsuspend modes, W2);
 *   'q' end.
 * W1 forks the child first in chld-worker. */
static void* worker(void* arg) {
  self = (int)(long)arg;
  tids[self] = gettid();
  int signo = signal_of(mode->source);
  mask(mode->blocks[self] ? SIG_BLOCK : SIG_UNBLOCK, signo);
  if (mode->steer) {
    mask(self == W2 ? SIG_UNBLOCK : SIG_BLOCK, signal_of(mode->steer_source));
  }
  pthread_mutex_lock(&lock);
  ready++;
  pthread_cond_broadcast(&cond);
  int forks = mode->source == CHILD_EXIT && mode->shape == TAKEN && mode->actor == self;
  while (forks && !fork_now) {
    pthread_cond_wait(&cond, &lock);
  }
  pthread_mutex_unlock(&lock);
  if (forks) {
    forked = fork_child();
  }
  for (;;) {
    char byte = wait_for_command();
    switch (byte) {
      case 'q':
        return NULL;
      case 'a': {
        long sysno = main_wait_syscall();
        if (sysno >= 0) {
          await_waiting(getpid(), tids[M], sysno);
        }
        arm_timer(mode->source);
        acknowledge();
        break;
      }
      case 's':
        arm_timer(ITIMER);
        acknowledge();
        break;
      case 'e': {
        /* suppressed-child-curr and default-child-curr, W2: once M sleeps
         * in its futex wait, release the gated child, wait until it is gone
         * (an explicitly ignored SIGCHLD auto-reaps it) or, left SIG_DFL,
         * until its exit is waitable, and wake M. */
        await_waiting(getpid(), tids[M], SYS_futex);
        char gate = 'x';
        if (write(child_gate[1], &gate, 1) != 1) {
          die("write");
        }
        for (int i = 0;; i++) {
          if (mode->shape == DEFAULTED) {
            siginfo_t info;
            memset(&info, 0, sizeof(info));
            if (waitid(P_PID, gated_child, &info, WEXITED | WNOWAIT | WNOHANG) == 0 &&
                info.si_pid == gated_child) {
              break;
            }
          } else if (kill(gated_child, 0) != 0 && errno == ESRCH) {
            break;
          }
          if (i == 10000) {
            unconfirmed("the gated child's exit was not seen");
          }
          sleep_ms(1);
        }
        __atomic_store_n(&creator_release, 1, __ATOMIC_RELEASE);
        if (syscall(SYS_futex, &creator_release, FUTEX_WAKE_PRIVATE, 1, NULL, NULL, 0) != 1) {
          unconfirmed("M did not wait in its futex wait");
        }
        acknowledge();
        break;
      }
      case 'r':
        /* suppressed-child-curr and default-child-curr, W1: unblock
         * SIGALRM. */
        mask(SIG_UNBLOCK, SIGALRM);
        acknowledge();
        break;
      case 'p':
        /* chld-ppoll-pending-temp, W2: once M waits in its ppoll (commanded
         * only at the event, past every setup sleep of M's, so a sleeping M
         * is in that ppoll), post the private SIGUSR2 the temporary mask
         * blocks; it stays pending for M alone. No acknowledgement: M is
         * in the call, and the child gates its exit on the pending copy. */
        await_waiting(getpid(), tids[M], SYS_ppoll);
        if (syscall(SYS_tgkill, getpid(), tids[M], SIGUSR2) != 0) {
          die("tgkill");
        }
        break;
      case 'k':
        /* A sigsuspend mode, W2: once a handler has reported (bounded at
         * 2 s), end M's wait. A handler runs only after the child's exit,
         * which waits for M to sleep in its call; one that ran on M has
         * ended that call already, and the SIGUSR2 then stays pending. */
        await_readable(report_pipe[0]);
        if (syscall(SYS_tgkill, getpid(), tids[M], SIGUSR2) != 0) {
          die("tgkill");
        }
        acknowledge();
        break;
      case 'n':
        acknowledge();
        break;
      case 'u':
        mask(SIG_UNBLOCK, signo);
        acknowledge();
        break;
      case 'v': {
        pid_t child = clone(vfork_child, vfork_stack + sizeof(vfork_stack), CLONE_VM | CLONE_VFORK,
                            NULL);
        if (child < 0) {
          die("clone");
        }
        /* It has no exit signal, so it is left to go with the process:
         * waiting for it takes __WCLONE, which Hermit's waitid emulation
         * does not accept. A signal Linux gave this thread while it was held
         * is taken on the return from clone(), before this. */
        if (mode->shape == COALESCED) {
          mask(SIG_UNBLOCK, SIGALRM);
        }
        acknowledge();
        break;
      }
      default:
        fprintf(stderr, "unknown command %c\n", byte);
        _exit(97);
    }
  }
}

/* The name of the thread whose handler ran, if one did, read without
 * blocking once the event is over. */
static const char* receiver(void) {
  char who;
  ssize_t n = read(report_pipe[0], &who, 1);
  if (n == 1 && who >= 0 && who < THREADS) {
    return NAMES[(int)who];
  }
  return "none";
}

/* Every thread whose handler ran since the last read, in order, joined by
 * '+', or "none". */
static void receivers(char* out, size_t size) {
  snprintf(out, size, "none");
  size_t used = 0;
  for (;;) {
    const char* who = receiver();
    if (!strcmp(who, "none")) {
      return;
    }
    int n = snprintf(out + used, size - used, "%s%s", used ? "+" : "", who);
    if (n < 0 || (size_t)n >= size - used) {
      return;
    }
    used += (size_t)n;
  }
}

/* M's wait in a chld-*-lets-in or chld-*-keeps-out mode: the mode's call,
 * under a temporary mask that differs from M's own about signo, on M's own
 * pipe (ppoll and pselect bounded at 200 ms, as poll is; sigsuspend ended by
 * W2). Returns as poll does: -1 with EINTR when M's handler for the mode's
 * signal ended the wait, 0 otherwise. */
static int wait_in_call(struct pollfd* fd, int signo) {
  sigset_t temporary;
  if (pthread_sigmask(SIG_SETMASK, NULL, &temporary) != 0) {
    die("pthread_sigmask");
  }
  if (mode->temporary_blocks) {
    sigaddset(&temporary, signo);
  } else {
    sigdelset(&temporary, signo);
  }
  if (mode->pending_temp) {
    sigaddset(&temporary, SIGUSR2);
  }
  struct timespec bound = {0, 200 * 1000000L};
  int n;
  switch (mode->call) {
    case PPOLL:
      n = ppoll(fd, 1, &bound, &temporary);
      break;
    case PSELECT: {
      fd_set readable;
      FD_ZERO(&readable);
      FD_SET(fd->fd, &readable);
      n = pselect(fd->fd + 1, &readable, NULL, NULL, &bound, &temporary);
      break;
    }
    case SUSPEND:
      sigdelset(&temporary, SIGUSR2);
      n = sigsuspend(&temporary) < 0 && errno == EINTR && took_signal ? -1 : 0;
      /* W2 sent its SIGUSR2 once it acknowledges. */
      await_acknowledgement();
      errno = n < 0 ? EINTR : 0;
      break;
    default:
      die("call");
      return 0;
  }
  if (n > 0) {
    errno = 0;
    die("the call found M's pipe readable");
  }
  return n;
}

/* Steer curr_target to W2 with the steering signal, which only W2 leaves
 * unblocked, then let W1 unblock the mode's signal. */
static void steer(void) {
  if (mode->steer_source == CHILD_EXIT) {
    forked = fork_child();
  } else {
    command(W1, 's');
    await_acknowledgement();
  }
  if (!await_readable(report_pipe[0])) {
    unconfirmed("no thread took the steering signal");
  }
  const char* steered = receiver();
  if (strcmp(steered, "W2")) {
    printf("steered=%s\n", steered);
    exit(1);
  }
  if (forked > 0) {
    reap(forked);
    forked = 0;
  }
  /* W2 has noted its interrupted read once it acknowledges a command. */
  command(W2, 'n');
  await_acknowledgement();
  pthread_mutex_lock(&lock);
  interrupted_by = -1;
  pthread_mutex_unlock(&lock);
  command(W1, 'u');
  await_acknowledgement();
  await_waiting(getpid(), tids[W1], SYS_read);
  await_waiting(getpid(), tids[W2], SYS_read);
}

/* coalesced-child-curr: W1 is held in its vfork wait when a first child's
 * SIGCHLD chooses it; a SIGALRM then walks to W2, the only thread that does
 * not block it, and leaves curr_target there; a second child's SIGCHLD merges
 * into the one still pending (legacy_queue) and moves nothing. Once released,
 * W1 takes the SIGCHLD and unblocks SIGALRM, and a final SIGALRM, which W1
 * and W2 both want, walks from curr_target to W2. */
static void run_coalesced(void) {
  command(W1, 'v');
  await_acknowledgement();
  await_waiting(getpid(), tids[W1], SYS_clone);
  pid_t first = fork_child();
  await_child_exit(first);
  if (!shared_pending(SIGCHLD)) {
    unconfirmed("the first SIGCHLD is not pending while W1 is held");
  }
  arm_timer(ITIMER);
  if (!await_readable(report_pipe[0])) {
    unconfirmed("no thread took the steering SIGALRM");
  }
  const char* steered = receiver();
  command(W2, 'n');
  await_acknowledgement();
  await_waiting(getpid(), tids[W2], SYS_read);
  pid_t second = fork_child();
  await_child_exit(second);
  if (!shared_pending(SIGCHLD)) {
    unconfirmed("no SIGCHLD is pending after the second child's exit");
  }
  char byte = 'x';
  if (write(vfork_gate[1], &byte, 1) != 1) {
    die("write");
  }
  /* W1 took any SIGCHLD it was given on its return from clone(). */
  await_acknowledgement();
  char chld[32];
  receivers(chld, sizeof(chld));
  await_waiting(getpid(), tids[W1], SYS_read);
  await_waiting(getpid(), tids[W2], SYS_read);
  arm_timer(ITIMER);
  await_readable(report_pipe[0]);
  for (int i = W1; i < THREADS; i++) {
    command(i, 'q');
  }
  for (int i = W1; i < THREADS; i++) {
    pthread_join(threads_of_main[i], NULL);
  }
  char final[32];
  receivers(final, sizeof(final));
  reap(first);
  reap(second);
  printf("steer=%s chld=%s final=%s\n", steered, chld, final);
}

/* suppressed-child-curr: SIGCHLD is explicitly ignored and every thread but
 * W1 blocks it; W2 alone leaves SIGALRM unblocked. A SIGALRM walks to W2
 * (steer=) and leaves curr_target there. M forks a child that waits at a
 * gate and sleeps in a futex wait; W2 releases the child, sees it reaped
 * and wakes M. Linux's do_notify_parent sends nothing for an explicitly
 * ignored SIGCHLD whatever the masks, so no walk runs and nothing is pending
 * (pending=). M installs a handler and W1 unblocks SIGALRM; a final SIGALRM,
 * which W1 and W2 both want, walks from W2 to W2 (final= names every handler
 * that ran up to the end of the run).
 *
 * default-child-curr: the same with SIGCHLD left SIG_DFL. Linux queues the
 * SIGCHLD, since M, the thread named, blocks it, and complete_signal walks
 * from W2 to W1, which does not block it, and leaves curr_target there. W2
 * sees the child's exit and wakes M, which reaps it; W1 discards the SIGCHLD
 * as it dequeues it, before it acknowledges, so nothing is pending
 * (pending=), and SIGCHLD stays SIG_DFL. W1 unblocks SIGALRM, and the final
 * SIGALRM walks from W1 to W1. */
static void run_suppressed(void) {
  int defaulted = mode->shape == DEFAULTED;
  arm_timer(ITIMER);
  if (!await_readable(report_pipe[0])) {
    unconfirmed("no thread took the steering SIGALRM");
  }
  const char* steered = receiver();
  command(W2, 'n');
  await_acknowledgement();
  await_waiting(getpid(), tids[W2], SYS_read);
  if (pipe(child_gate) != 0) {
    die("pipe");
  }
  gated_child = fork();
  if (gated_child < 0) {
    die("fork");
  }
  if (gated_child == 0) {
    char gate;
    _exit(read(child_gate[0], &gate, 1) == 1 ? 0 : 3);
  }
  command(W2, 'e');
  for (int i = 0; !__atomic_load_n(&creator_release, __ATOMIC_ACQUIRE); i++) {
    if (i == 10) {
      unconfirmed("W2 did not wake M");
    }
    struct timespec bound = {1, 0};
    syscall(SYS_futex, &creator_release, FUTEX_WAIT_PRIVATE, 0, &bound, NULL, 0);
  }
  await_acknowledgement();
  if (defaulted) {
    reap(gated_child);
    /* W1 has dequeued any SIGCHLD it was given once it acknowledges. */
    command(W1, 'n');
    await_acknowledgement();
  }
  int pending = shared_pending(SIGCHLD);
  if (!defaulted) {
    set_action(SIGCHLD, on_signal);
  }
  command(W1, 'r');
  await_acknowledgement();
  await_waiting(getpid(), tids[W1], SYS_read);
  await_waiting(getpid(), tids[W2], SYS_read);
  arm_timer(ITIMER);
  await_readable(report_pipe[0]);
  for (int i = W1; i < THREADS; i++) {
    command(i, 'q');
  }
  for (int i = W1; i < THREADS; i++) {
    pthread_join(threads_of_main[i], NULL);
  }
  char final[32];
  receivers(final, sizeof(final));
  printf("steer=%s pending=%d final=%s\n", steered, pending, final);
}

int main(int argc, char** argv) {
  if (argc != 2) {
    fprintf(stderr, "usage: %s MODE\n", argv[0]);
    return 2;
  }
  for (size_t i = 0; i < sizeof(MODES) / sizeof(MODES[0]); i++) {
    if (!strcmp(argv[1], MODES[i].name)) {
      mode = &MODES[i];
    }
  }
  if (!mode) {
    fprintf(stderr, "unknown mode %s\n", argv[1]);
    return 2;
  }
  tids[M] = gettid();
  int signo = signal_of(mode->source);
  for (int i = 0; i < THREADS; i++) {
    if (pipe(wake_pipes[i]) != 0) {
      die("pipe");
    }
  }
  if (pipe2(report_pipe, O_NONBLOCK) != 0 || pipe(done_pipe) != 0 || pipe(vfork_gate) != 0) {
    die("pipe2");
  }
  if (mode->shape != DEFAULTED) {
    set_action(signo, mode->ignored ? SIG_IGN : on_signal);
  }
  if (mode->steer) {
    set_action(signal_of(mode->steer_source), on_signal);
    mask(SIG_BLOCK, signal_of(mode->steer_source));
  }
  if (mode->shape == BUSY) {
    set_action(mode->busy_ignored ? SIGWINCH : SIGUSR2,
               mode->busy_ignored ? SIG_IGN : on_signal);
  }
  if (mode->pending_temp) {
    /* The privately posted SIGUSR2 needs a handler: M takes it once its
     * ppoll ends and its own mask is back, after the SIGCHLD it waited
     * for, so the first report byte is still the SIGCHLD's. */
    set_action(SIGUSR2, on_signal);
  }
  if (mode->blocks[M]) {
    mask(SIG_BLOCK, signo);
  }
  if (mode->call != POLL && !mode->temporary_blocks && !mode->pending_temp) {
    /* M's own mask blocks the signal; its call's temporary mask lets it in. */
    mask(SIG_BLOCK, signo);
  }
  if (mode->call == SUSPEND) {
    set_action(SIGUSR2, on_wake);
    mask(SIG_BLOCK, SIGUSR2);
  }
  if (mode->source == POSIX_TIMER) {
    struct sigevent event;
    memset(&event, 0, sizeof(event));
    event.sigev_notify = SIGEV_SIGNAL;
    event.sigev_signo = SIGUSR1;
    if (timer_create(CLOCK_MONOTONIC, &event, &timer) != 0) {
      die("timer_create");
    }
  }
  pthread_t* threads = threads_of_main;
  for (long i = W1; i < THREADS; i++) {
    if (pthread_create(&threads[i], NULL, worker, (void*)i) != 0) {
      die("pthread_create");
    }
  }
  pthread_mutex_lock(&lock);
  while (ready != THREADS - 1) {
    pthread_cond_wait(&cond, &lock);
  }
  pthread_mutex_unlock(&lock);
  /* Every worker waits in its read() (W1 in chld-worker once it forked). */
  int forker = mode->source == CHILD_EXIT && mode->shape == TAKEN ? mode->actor : M;
  if (forker == W1) {
    await_waiting(getpid(), tids[W2], SYS_read);
    pthread_mutex_lock(&lock);
    fork_now = 1;
    pthread_cond_broadcast(&cond);
    pthread_mutex_unlock(&lock);
  } else {
    await_waiting(getpid(), tids[W1], SYS_read);
    await_waiting(getpid(), tids[W2], SYS_read);
  }
  if (mode->shape == COALESCED) {
    run_coalesced();
    return 0;
  }
  if (mode->shape == SUPPRESSED || mode->shape == DEFAULTED) {
    run_suppressed();
    return 0;
  }
  if (mode->steer) {
    steer();
  }
  if (mode->shape == BUSY) {
    /* W1 sleeps in its vfork wait, and SIGUSR2 is pending for it there (or
     * an ignored SIGWINCH, which Linux discards as it is sent). */
    command(W1, 'v');
    await_acknowledgement();
    await_waiting(getpid(), tids[W1], SYS_clone);
    if (syscall(SYS_tgkill, getpid(), tids[W1], mode->busy_ignored ? SIGWINCH : SIGUSR2) != 0) {
      die("tgkill");
    }
  }
  if (mode->shape == HELD) {
    /* W2, the only thread that does not block the signal, sleeps in its
     * vfork wait, which no signal it does not block ends. */
    command(W2, 'v');
    await_acknowledgement();
    await_waiting(getpid(), tids[W2], SYS_clone);
  }
  /* The event: M forks the child (unless W1 did), or a worker arms the
   * timer; W2 does in a busy mode, where W1 is in its vfork wait, and W1
   * otherwise. */
  if (mode->source == CHILD_EXIT) {
    if (mode->call == SUSPEND) {
      command(W2, 'k');
    }
    if (mode->pending_temp) {
      command(W2, 'p');
    }
    if (forker == M) {
      forked = mode->pending_temp ? fork_pending_child() : fork_child();
    }
  } else {
    command(mode->shape == BUSY ? W2 : W1, 'a');
  }
  int waited = 0;
  if (mode->shape == SIGWAIT) {
    sigset_t set;
    sigemptyset(&set);
    sigaddset(&set, signo);
    struct timespec bound = {2, 0};
    siginfo_t info;
    int got;
    while ((got = sigtimedwait(&set, &info, &bound)) < 0 && errno == EINTR) {
    }
    waited = got == signo;
  } else if (mode->blocks[M]) {
    /* M waits, with the signal blocked, until the event has happened. */
    if (mode->source == CHILD_EXIT) {
      await_child_exit(forked);
    } else {
      await_acknowledgement();
      await_expiry(mode->source);
      /* A timer reads as expired when it is due, which can be before its
       * signal has been generated (seen on Linux 7.1), so a queued mode also
       * waits for the signal itself: with every thread blocking it stays
       * pending. */
      if (mode->shape == QUEUED && !await_pending(signo, 1, 10000)) {
        unconfirmed("the queued signal never became pending");
      }
    }
  } else {
    /* M waits for its own pipe in poll(), with a 200 ms bound so a run
     * where Linux gave the signal to another thread cannot hang; nothing
     * writes the pipe, so only a handler ends the wait early. */
    struct pollfd fd = {wake_pipes[M][0], POLLIN, 0};
    int n = mode->call == POLL ? poll(&fd, 1, 200) : wait_in_call(&fd, signo);
    if (n < 0 && errno == EINTR) {
      pthread_mutex_lock(&lock);
      if (interrupted_by < 0) {
        interrupted_by = M;
      }
      pthread_mutex_unlock(&lock);
    } else if (n != 0) {
      die("poll");
    }
  }
  if (mode->source != CHILD_EXIT && (mode->shape == SIGWAIT || !mode->blocks[M])) {
    await_acknowledgement();
  }
  if (mode->shape == TAKEN || (mode->shape == BUSY && !mode->busy_ignored) ||
      (mode->shape == SIGWAIT && !waited)) {
    /* The thread that took it has run its handler: an outcome, so a handler
     * that did not run within the bound reads as none (the header's rule). */
    await_readable(report_pipe[0]);
  }
  /* alrm-ignored-unblocked: the leader blocks the ignored signal, so Linux
   * queues it (sig_ignored) for W2, which cannot take it in its vfork wait;
   * M sees it pending (generated), then releases W2, which takes and discards
   * it on its return from clone() and acknowledges after. */
  int generated = 0;
  if (mode->shape == HELD) {
    generated = await_pending(signo, 1, 10000);
    char byte = 'x';
    if (write(vfork_gate[1], &byte, 1) != 1) {
      die("write");
    }
    await_acknowledgement();
  }
  int pending = shared_pending(signo);
  if (mode->ignored) {
    set_action(signo, on_signal);
  }
  if (mode->shape == QUEUED || mode->shape == HELD) {
    /* The worker named by the mode unblocks the signal and acknowledges once
     * it took any pending copy. */
    command(mode->actor, 'u');
    await_acknowledgement();
  }
  const char* got = receiver();
  char busy[32] = "none";
  if (mode->shape == BUSY) {
    /* Release the vfork child; W1 acknowledges once it took what Linux gave
     * it on its return from clone(). Every handler that ran since is named,
     * so a late one is seen. */
    char byte = 'x';
    if (write(vfork_gate[1], &byte, 1) != 1) {
      die("write");
    }
    await_acknowledgement();
  }
  for (int i = W1; i < THREADS; i++) {
    command(i, 'q');
  }
  for (int i = W1; i < THREADS; i++) {
    pthread_join(threads[i], NULL);
  }
  if (mode->shape == BUSY) {
    receivers(busy, sizeof(busy));
  }
  if (forked > 0) {
    reap(forked);
  }
  const char* interrupted = interrupted_by < 0 ? "none" : NAMES[interrupted_by];
  switch (mode->shape) {
    case QUEUED:
      printf("pending=%d receiver=%s\n", pending, got);
      break;
    case SIGWAIT:
      printf("waited=%d receiver=%s interrupted=%s\n", waited, got, interrupted);
      break;
    case BUSY:
      printf("receiver=%s interrupted=%s busy=%s\n", got, interrupted, busy);
      break;
    case HELD:
      printf("generated=%d pending=%d receiver=%s\n", generated, pending, got);
      break;
    case COALESCED:
    case SUPPRESSED:
    case DEFAULTED:
      break;
    case TAKEN:
      printf("receiver=%s interrupted=%s\n", got, interrupted);
      break;
  }
  return 0;
}
