/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * A standard signal Linux discards when it is sent never ends a wait or
 * reaches a take, whatever handler is installed afterwards
 * (https://github.com/rrnewton/hermit/issues/4293). Linux discards a signal
 * at generation when the task the sender names leaves it unblocked and
 * ignores it: SIG_IGN, or the default action of SIGCHLD, SIGCONT, SIGURG or
 * SIGWINCH. Under ptrace the kernel queues the copy for the tracer anyway, so
 * a check that read /proc after a handler appeared used to count it.
 *
 * Usage: guest_discard_at_send MODE. Each mode prints one line, its native
 * output; a handler count is the count when the wait returned.
 *
 *   futex-dfl, futex-dfl-yield
 *       A thread waits in FUTEX_WAIT with a 300 ms timeout. Main sends it
 *       SIGWINCH while SIGWINCH is SIG_DFL, then installs a handler (after
 *       one sched_yield in the -yield form). The wait times out.
 *   futex-ign, futex-ign-yield
 *       The same with SIGUSR1 set to SIG_IGN when sent.
 *   sleep-dfl, sleep-ign
 *       The same for a 300 ms nanosleep, which completes.
 *   wait4-dfl
 *       The same for a wait4 on a child that exits after 300 ms.
 *   kept-after-discard
 *       The thread waits in FUTEX_WAIT with a 1 s timeout; main sends it a
 *       discarded SIGUSR1, installs a handler and sends a kept one, which
 *       ends the wait at once.
 *   blocked-keeps
 *       SIGUSR1 blocked in the thread and SIG_IGN when sent, so Linux keeps
 *       the copy; a handler, then the thread unblocks it: the handler runs.
 *   kept-timer-after-discard
 *       The thread waits in FUTEX_WAIT with a 1 s timeout. Main sends it a
 *       discarded SIGUSR1 (SIG_IGN), installs a handler, and a 10 ms
 *       SIGEV_THREAD_ID SIGUSR1 timer for the thread fires: that kept copy
 *       ends the wait.
 *   sole-nonleader, exited-leader
 *       A child process ignores SIGUSR1; its leader blocks SIGUSR1 and exits,
 *       leaving one thread, which leaves it unblocked and waits for it in
 *       sigtimedwait for 300 ms. The parent kills that thread by its id
 *       (Linux reads its mask: discarded, EAGAIN), or the exited leader by
 *       the process id (Linux reads the leader's frozen mask, which blocks
 *       it: kept, and the thread takes it).
 *   init-dfl, init-caught, init-own
 *       A child made with CLONE_NEWPID, its namespace's init, waits for
 *       SIGUSR1 in sigtimedwait for 300 ms with SIGUSR1 unblocked. The parent
 *       sends it SIGUSR1 while it is SIG_DFL (an init discards it: EAGAIN), or
 *       while it has a handler (kept: taken); or the init's own child sends it
 *       to pid 1 while SIG_DFL (discarded: EAGAIN).
 *   signalfd-fault-inread
 *       As signalfd-inread, with the read's buffer on a PROT_NONE page: the
 *       read dequeues SIGUSR2 and fails copying it out (EFAULT).
 *   tight-stack-null, tight-stack-info
 *       rt_sigtimedwait for a pending SIGUSR1 issued with the stack pointer
 *       256 bytes above a PROT_NONE page (an assembly wrapper), with a null
 *       or a valid static siginfo: SIGUSR1 either way.
 *   fault-partial-16, fault-partial-64
 *       A kept SIGUSR1 is pending; sigtimedwait's siginfo starts 16 or 64
 *       bytes before a PROT_NONE page, over 0xa5 bytes: EFAULT, with that
 *       prefix written, and a second take finds nothing.
 *   fault-partial-discarded
 *       A thread waits for SIGUSR1 for 300 ms with such a siginfo, SIGUSR1
 *       unblocked; main sends it SIGUSR1 while SIG_IGN and installs a
 *       handler: EAGAIN, and no byte of the buffer changes.
 *   fault-partial-hole
 *       As fault-partial-discarded, with an unmapped page after the siginfo's
 *       64 bytes instead of a PROT_NONE one: EAGAIN, no byte changed.
 *   signalfd-eintr, signalfd-eintr-8k
 *       A thread with SIGUSR1 and SIGUSR2 unblocked reads a blocking signalfd
 *       for SIGUSR1 into 256 (or 8192) bytes of 0xa5; main sends it SIGUSR1
 *       while SIG_IGN, installs handlers, and later sends SIGUSR2 (no
 *       SA_RESTART): EINTR, one SIGUSR2 handler, and the buffer unchanged.
 *   futex-flush-resend
 *       A thread waits on a futex for 300 ms with SIGUSR1 unblocked and
 *       handler h1; main sends it a kept SIGUSR1, yields, makes SIGUSR1
 *       SIG_IGN (flushing the copy), sends a discarded SIGUSR1 and installs
 *       h2: h2 never runs.
 *   armed-posix, armed-itimer, armed-unignore
 *       A timer signal sent while every thread blocks it, then a sigsuspend
 *       that admits it beside a yielding sibling (run_armed).
 *   take-info
 *       Main blocks SIGUSR1, installs a handler, raises it and takes it with
 *       sigtimedwait into a siginfo buffer: SIGUSR1, and the buffer's signal.
 *   self-take
 *       Main sends itself a discarded SIGUSR1 (SIG_IGN), blocks it, installs
 *       a handler, raises it, and takes it with sigtimedwait: SIGUSR1 at once.
 *   sigtimedwait-ign
 *       Main sends a thread SIGUSR1 while SIG_IGN and unblocked, then
 *       installs a handler; the thread then blocks SIGUSR1 and waits for it
 *       in sigtimedwait for 300 ms: EAGAIN.
 *   sigtimedwait-ign-inwait
 *       The same, with the send after the thread entered its sigtimedwait,
 *       with SIGUSR1 unblocked in its own mask.
 *   signalfd-ign
 *       As sigtimedwait-ign, read through a nonblocking signalfd after a
 *       300 ms poll: the poll times out, and the read is EAGAIN.
 *   sigpipe-take, sigpipe-signalfd
 *       SIGPIPE SIG_IGN and unblocked; main sends a thread SIGPIPE, which is
 *       discarded; the thread blocks SIGPIPE and takes with a zero timeout
 *       (EAGAIN). It installs a handler, writes to a pipe with no reader,
 *       which queues a kept SIGPIPE no send hook sees, and takes again
 *       (SIGPIPE). The second form takes through a nonblocking signalfd.
 *   timer-unchanged, timer-rearm-far, timer-rearm-near, timer-disarm,
 *   timer-delete
 *       A periodic SIGUSR1 POSIX timer (first expiry 10 ms, period 300 ms)
 *       fires once while SIGUSR1 is SIG_IGN; then the timer is left alone,
 *       rearmed 1000 s ahead, rearmed to expire again at once while still
 *       ignored, disarmed, or deleted; then SIGUSR1 is blocked, a handler
 *       installed, and sigtimedwait waits 100 ms.
 *   wait4-timer
 *       A thread waits in wait4 for a child that exits after 300 ms; a
 *       one-shot SIGWINCH POSIX timer expires at 50 ms while SIGWINCH is
 *       SIG_DFL, then main installs a handler: the wait goes on.
 *   itimer-leader
 *       SIGALRM is SIG_IGN. Main blocks SIGALRM; a thread that leaves it
 *       unblocked arms a 50 ms ITIMER_REAL and sleeps 500 ms; main waits for
 *       SIGALRM in sigtimedwait for 300 ms. ITIMER_REAL sends to the leader,
 *       which blocks it, so Linux keeps the copy and the wait returns it.
 *   xproc
 *       A child process ignores SIGUSR1; the parent sends it SIGUSR1; the
 *       child installs a handler and waits in FUTEX_WAIT for 300 ms.
 *   fault-empty, fault-discarded, fault-kept
 *       sigtimedwait with its siginfo on a PROT_NONE page for 100 ms, with
 *       nothing pending, a discarded SIGUSR1 pending, or a kept one.
 *   signalfd-two
 *       A blocking signalfd read with room for two records while one signal
 *       is pending returns one record at once.
 *   signalfd-partial
 *       A signalfd read whose second record would land on a PROT_NONE page,
 *       with two signals pending, returns the first record.
 *   sigpipe-inwait
 *       As sigpipe-take, with the discarded SIGPIPE sent while the thread
 *       waits for it in sigtimedwait for 300 ms (EAGAIN), SIGPIPE unblocked
 *       in its own mask, so the take finds the discarded copy still queued.
 *   signalfd-inread
 *       A thread blocks SIGUSR2, leaves SIGUSR1 unblocked, and reads a
 *       blocking signalfd for both; main sends it SIGUSR1 while SIG_IGN,
 *       installs a handler, waits 100 ms and sends SIGUSR2: the read returns
 *       SIGUSR2's record.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <linux/futex.h>
#include <poll.h>
#include <pthread.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/signalfd.h>
#include <sys/time.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static atomic_int handled;
static atomic_int handled_usr2;
static atomic_int ready;
static atomic_int sent;
static atomic_int futex_word;
static pid_t target_tid;
static const char *mode;

static void on_signal(int signal) {
  if (signal == SIGUSR2) {
    atomic_fetch_add(&handled_usr2, 1);
  }
  atomic_fetch_add(&handled, 1);
}

/* Two distinct handlers, so a copy delivered before a flush (h1) is told
 * from a stale one reaching the later handler (h2). */
static atomic_int handled_h1;
static atomic_int handled_h2;
static void on_h1(int signal) {
  (void)signal;
  atomic_fetch_add(&handled_h1, 1);
}
static void on_h2(int signal) {
  (void)signal;
  atomic_fetch_add(&handled_h2, 1);
}


static void set_action(int signal, void (*handler)(int)) {
  struct sigaction action;
  memset(&action, 0, sizeof(action));
  action.sa_handler = handler;
  sigaction(signal, &action, NULL);
}

static void mask(int how, int signal) {
  sigset_t set;
  sigemptyset(&set);
  sigaddset(&set, signal);
  pthread_sigmask(how, &set, NULL);
}

static void sleep_ms(long ms) {
  struct timespec delay = {ms / 1000, (ms % 1000) * 1000000};
  nanosleep(&delay, NULL);
}

static const char *errno_name(int error) {
  switch (error) {
  case 0:
    return "0";
  case EINTR:
    return "EINTR";
  case EAGAIN:
    return "EAGAIN";
  case ETIMEDOUT:
    return "ETIMEDOUT";
  case EPIPE:
    return "EPIPE";
  default:
    return "other";
  }
}

static int is(const char *name) { return !strcmp(mode, name); }

static int starts(const char *prefix) {
  return !strncmp(mode, prefix, strlen(prefix));
}

/* rt_sigtimedwait(set, info, timeout, 8) issued on the stack `new_sp`. */
extern long tight_wait(void *new_sp, const unsigned long *set, siginfo_t *info,
                       const struct timespec *timeout);
__asm__(".text\n"
        ".globl tight_wait\n"
        ".type tight_wait,@function\n"
        "tight_wait:\n"
        "  push %r12\n"
        "  mov %rsp,%r12\n"
        "  mov %rdi,%rsp\n"
        "  mov %rsi,%rdi\n"
        "  mov %rdx,%rsi\n"
        "  mov %rcx,%rdx\n"
        "  mov $8,%r10\n"
        "  mov $128,%eax\n"
        "  syscall\n"
        "  mov %r12,%rsp\n"
        "  pop %r12\n"
        "  ret\n"
        ".size tight_wait,.-tight_wait\n");

static unsigned long tight_set = 1UL << (SIGUSR1 - 1);
static siginfo_t tight_info;
static const struct timespec tight_zero = {0, 0};

static int run_tight_stack(void) {
  unsigned char *mapping =
      mmap(NULL, 8192, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  if (mapping == MAP_FAILED || mprotect(mapping + 4096, 4096, PROT_READ | PROT_WRITE)) {
    perror("stack mapping");
    return 2;
  }
  mask(SIG_BLOCK, SIGUSR1);
  syscall(SYS_tgkill, getpid(), (pid_t)syscall(SYS_gettid), SIGUSR1);
  siginfo_t *info = is("tight-stack-info") ? &tight_info : NULL;
  long result = tight_wait(mapping + 4096 + 256, &tight_set, info, &tight_zero);
  printf("%s: sigtimedwait %ld, info signal %d\n", mode, result,
         info ? info->si_signo : -1);
  return 0;
}

/* A siginfo buffer `prefix` bytes before a PROT_NONE page, over 0xa5. */
static unsigned char *boundary_buffer(long prefix) {
  unsigned char *pages =
      mmap(NULL, 8192, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  memset(pages, 0xa5, 8192);
  if (is("fault-partial-hole")) {
    munmap(pages + 4096, 4096); /* a hole, not a PROT_NONE page */
  } else {
    mprotect(pages + 4096, 4096, PROT_NONE);
  }
  return pages + 4096 - prefix;
}

static int fault_discarded(void) {
  return is("fault-partial-discarded") || is("fault-partial-hole");
}

static int signalfd_eintr(void) {
  return is("signalfd-eintr") || is("signalfd-eintr-8k");
}

static int changed_bytes(const unsigned char *buffer, long prefix) {
  int changed = 0;
  for (long at = 0; at < prefix; at++) {
    changed += buffer[at] != 0xa5;
  }
  return changed;
}

static int run_fault_partial(void) {
  long prefix = is("fault-partial-16") ? 16 : 64;
  unsigned char *buffer = boundary_buffer(prefix);
  mask(SIG_BLOCK, SIGUSR1);
  set_action(SIGUSR1, on_signal);
  raise(SIGUSR1);
  sigset_t set;
  sigemptyset(&set);
  sigaddset(&set, SIGUSR1);
  struct timespec zero = {0, 0};
  long result = sigtimedwait(&set, (siginfo_t *)buffer, &zero);
  int error = result < 0 ? errno : 0;
  long next = sigtimedwait(&set, NULL, &zero);
  int next_error = next < 0 ? errno : 0;
  printf("%s: sigtimedwait %ld %s, changed %d, next %ld %s\n", mode, result,
         error == EFAULT ? "EFAULT" : errno_name(error), changed_bytes(buffer, prefix),
         next, errno_name(next_error));
  return 0;
}

/* The signal a waiting mode is sent, and how it is discarded. */
static int waited_signal(void) {
  return strstr(mode, "-dfl") ? SIGWINCH : SIGUSR1;
}

static void wait_until_ready(void) {
  while (!atomic_load(&ready)) {
    sched_yield();
  }
  /* Let the thread reach its wait. */
  sleep_ms(50);
}

/* Main's side of the waiting modes: the discarded send, then the handler. */
static void send_discarded_then_handle(void) {
  int signal = waited_signal();
  wait_until_ready();
  syscall(SYS_tgkill, getpid(), target_tid, signal);
  if (strstr(mode, "-yield")) {
    sched_yield();
  }
  set_action(signal, on_signal);
  atomic_store(&sent, 1);
}

static void *waiter(void *unused) {
  (void)unused;
  target_tid = (pid_t)syscall(SYS_gettid);
  long result;
  int error;
  if (is("futex-flush-resend")) {
    struct timespec timeout = {0, 300000000};
    atomic_store(&ready, 1);
    result = syscall(SYS_futex, &futex_word, FUTEX_WAIT_PRIVATE, 0, &timeout,
                     NULL, 0);
    error = result < 0 ? errno : 0;
    printf("%s: futex %ld %s, h1 %d, h2 %d\n", mode, result, errno_name(error),
           atomic_load(&handled_h1), atomic_load(&handled_h2));
  } else if (starts("futex")) {
    struct timespec timeout = {0, 300000000};
    atomic_store(&ready, 1);
    result = syscall(SYS_futex, &futex_word, FUTEX_WAIT_PRIVATE, 0, &timeout,
                     NULL, 0);
    error = result < 0 ? errno : 0;
    printf("%s: futex %ld %s, handled %d\n", mode, result, errno_name(error),
           atomic_load(&handled));
  } else if (starts("sleep")) {
    struct timespec delay = {0, 300000000};
    atomic_store(&ready, 1);
    result = nanosleep(&delay, NULL);
    error = result < 0 ? errno : 0;
    printf("%s: nanosleep %ld %s, handled %d\n", mode, result,
           errno_name(error), atomic_load(&handled));
  } else if (is("wait4-timer")) {
    pid_t child = fork();
    if (child == 0) {
      sleep_ms(300);
      _exit(7);
    }
    int status = 0;
    atomic_store(&ready, 1);
    result = wait4(child, &status, 0, NULL);
    error = result < 0 ? errno : 0;
    printf("%s: wait4 %s %s, status %d, handled %d\n", mode,
           result == child ? "child" : "other", errno_name(error),
           WIFEXITED(status) ? WEXITSTATUS(status) : -1,
           atomic_load(&handled));
  } else if (starts("wait4")) {
    pid_t child = fork();
    if (child == 0) {
      sleep_ms(300);
      _exit(7);
    }
    int status = 0;
    atomic_store(&ready, 1);
    result = wait4(child, &status, 0, NULL);
    error = result < 0 ? errno : 0;
    printf("%s: wait4 %s %s, status %d, handled %d\n", mode,
           result == child ? "child" : "other", errno_name(error),
           WIFEXITED(status) ? WEXITSTATUS(status) : -1,
           atomic_load(&handled));
  } else if (is("kept-after-discard") || is("kept-timer-after-discard")) {
    atomic_store(&ready, 1);
    struct timespec timeout = {1, 0};
    result = syscall(SYS_futex, &futex_word, FUTEX_WAIT_PRIVATE, 0, &timeout,
                     NULL, 0);
    error = result < 0 ? errno : 0;
    printf("%s: futex %ld %s, handled %d\n", mode, result, errno_name(error),
           atomic_load(&handled));
  } else if (is("blocked-keeps")) {
    mask(SIG_BLOCK, SIGUSR1);
    atomic_store(&ready, 1);
    while (!atomic_load(&sent)) {
      sched_yield();
    }
    mask(SIG_UNBLOCK, SIGUSR1);
    printf("%s: handled %d\n", mode, atomic_load(&handled));
  } else if (starts("sigtimedwait") || is("signalfd-ign")) {
    sigset_t set;
    sigemptyset(&set);
    sigaddset(&set, SIGUSR1);
    atomic_store(&ready, 1);
    if (!is("sigtimedwait-ign-inwait")) {
      while (!atomic_load(&sent)) {
        sched_yield();
      }
      mask(SIG_BLOCK, SIGUSR1);
    }
    if (is("signalfd-ign")) {
      int fd = signalfd(-1, &set, SFD_NONBLOCK);
      struct pollfd poller = {fd, POLLIN, 0};
      int polled = poll(&poller, 1, 300);
      struct signalfd_siginfo info;
      long got = read(fd, &info, sizeof(info));
      error = got < 0 ? errno : 0;
      printf("%s: poll %d, read %ld %s, handled %d\n", mode, polled, got,
             errno_name(error), atomic_load(&handled));
    } else {
      struct timespec timeout = {0, 300000000};
      result = sigtimedwait(&set, NULL, &timeout);
      error = result < 0 ? errno : 0;
      printf("%s: sigtimedwait %ld %s, handled %d\n", mode, result,
             errno_name(error), atomic_load(&handled));
    }
  } else if (is("sigpipe-inwait")) {
    sigset_t set;
    sigemptyset(&set);
    sigaddset(&set, SIGPIPE);
    atomic_store(&ready, 1);
    struct timespec timeout = {0, 300000000};
    long first = sigtimedwait(&set, NULL, &timeout);
    int first_error = first < 0 ? errno : 0;
    mask(SIG_BLOCK, SIGPIPE);
    set_action(SIGPIPE, on_signal);
    int pipes[2];
    if (pipe(pipes)) {
      perror("pipe");
      exit(2);
    }
    close(pipes[0]);
    long wrote = write(pipes[1], "x", 1);
    int write_error = wrote < 0 ? errno : 0;
    struct timespec zero = {0, 0};
    long second = sigtimedwait(&set, NULL, &zero);
    int second_error = second < 0 ? errno : 0;
    printf("%s: first %ld %s, write %ld %s, second %ld %s\n", mode, first,
           errno_name(first_error), wrote, errno_name(write_error), second,
           errno_name(second_error));
  } else if (fault_discarded()) {
    unsigned char *buffer = boundary_buffer(64);
    sigset_t set;
    sigemptyset(&set);
    sigaddset(&set, SIGUSR1);
    atomic_store(&ready, 1);
    struct timespec timeout = {0, 300000000};
    result = sigtimedwait(&set, (siginfo_t *)buffer, &timeout);
    error = result < 0 ? errno : 0;
    printf("%s: sigtimedwait %ld %s, changed %d\n", mode, result,
           error == EFAULT ? "EFAULT" : errno_name(error), changed_bytes(buffer, 64));
  } else if (signalfd_eintr()) {
    sigset_t set;
    sigemptyset(&set);
    sigaddset(&set, SIGUSR1);
    int fd = signalfd(-1, &set, 0);
    static unsigned char buffer[8192];
    long size = is("signalfd-eintr-8k") ? 8192 : 256;
    memset(buffer, 0xa5, size);
    atomic_store(&ready, 1);
    long got = read(fd, buffer, size);
    error = got < 0 ? errno : 0;
    printf("%s: read %ld %s, usr2 handled %d, changed %d\n", mode, got,
           errno_name(error), atomic_load(&handled_usr2), changed_bytes(buffer, size));
  } else if (is("signalfd-inread") || is("signalfd-fault-inread")) {
    mask(SIG_BLOCK, SIGUSR2);
    sigset_t set;
    sigemptyset(&set);
    sigaddset(&set, SIGUSR1);
    sigaddset(&set, SIGUSR2);
    int fd = signalfd(-1, &set, 0);
    void *page = mmap(NULL, 4096, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    atomic_store(&ready, 1);
    struct signalfd_siginfo info;
    long got = is("signalfd-fault-inread") ? read(fd, page, sizeof(info))
                                           : read(fd, &info, sizeof(info));
    error = got < 0 ? errno : 0;
    printf("%s: read %ld %s, signal %ld\n", mode, got,
           error == EFAULT ? "EFAULT" : errno_name(error),
           got > 0 ? (long)info.ssi_signo : 0L);
  } else if (starts("sigpipe")) {
    atomic_store(&ready, 1);
    while (!atomic_load(&sent)) {
      sched_yield();
    }
    mask(SIG_BLOCK, SIGPIPE);
    sigset_t set;
    sigemptyset(&set);
    sigaddset(&set, SIGPIPE);
    int fd = is("sigpipe-signalfd") ? signalfd(-1, &set, SFD_NONBLOCK) : -1;
    struct timespec zero = {0, 0};
    struct signalfd_siginfo info;
    long first = fd >= 0 ? read(fd, &info, sizeof(info))
                         : sigtimedwait(&set, NULL, &zero);
    int first_error = first < 0 ? errno : 0;
    set_action(SIGPIPE, on_signal);
    int pipes[2];
    if (pipe(pipes)) {
      perror("pipe");
      exit(2);
    }
    close(pipes[0]);
    long wrote = write(pipes[1], "x", 1);
    int write_error = wrote < 0 ? errno : 0;
    long second = fd >= 0 ? read(fd, &info, sizeof(info))
                          : sigtimedwait(&set, NULL, &zero);
    int second_error = second < 0 ? errno : 0;
    long second_signal = fd >= 0 && second > 0 ? (long)info.ssi_signo : second;
    printf("%s: first %ld %s, write %ld %s, second %ld %s\n", mode, first,
           errno_name(first_error), wrote, errno_name(write_error),
           second_signal, errno_name(second_error));
  }
  return NULL;
}

static void *arm_itimer(void *unused) {
  (void)unused;
  mask(SIG_UNBLOCK, SIGALRM);
  struct itimerval value = {{0, 0}, {0, 50000}};
  setitimer(ITIMER_REAL, &value, NULL);
  atomic_store(&ready, 1);
  sleep_ms(500);
  return NULL;
}

static int run_itimer_leader(void) {
  set_action(SIGALRM, SIG_IGN);
  mask(SIG_BLOCK, SIGALRM);
  pthread_t thread;
  pthread_create(&thread, NULL, arm_itimer, NULL);
  while (!atomic_load(&ready)) {
    sched_yield();
  }
  sigset_t set;
  sigemptyset(&set);
  sigaddset(&set, SIGALRM);
  struct timespec timeout = {0, 300000000};
  long result = sigtimedwait(&set, NULL, &timeout);
  int error = result < 0 ? errno : 0;
  printf("%s: sigtimedwait %ld %s\n", mode, result, errno_name(error));
  pthread_join(thread, NULL);
  return 0;
}

static int run_xproc(void) {
  int ready_pipe[2];
  if (pipe(ready_pipe)) {
    perror("pipe");
    return 2;
  }
  pid_t child = fork();
  if (child == 0) {
    set_action(SIGUSR1, SIG_IGN);
    char byte = 1;
    if (write(ready_pipe[1], &byte, 1) != 1) {
      _exit(2);
    }
    sleep_ms(100); /* the parent's send lands while SIGUSR1 is ignored */
    set_action(SIGUSR1, on_signal);
    struct timespec timeout = {0, 300000000};
    long result =
        syscall(SYS_futex, &futex_word, FUTEX_WAIT_PRIVATE, 0, &timeout, NULL, 0);
    int error = result < 0 ? errno : 0;
    printf("%s: futex %ld %s, handled %d\n", mode, result, errno_name(error),
           atomic_load(&handled));
    _exit(0);
  }
  char byte;
  if (read(ready_pipe[0], &byte, 1) != 1) {
    return 2;
  }
  kill(child, SIGUSR1);
  int status;
  waitpid(child, &status, 0);
  return 0;
}

static int run_fault(void) {
  void *page = mmap(NULL, 4096, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  if (page == MAP_FAILED) {
    perror("mmap");
    return 2;
  }
  if (!is("fault-empty")) {
    set_action(SIGUSR1, is("fault-discarded") ? SIG_IGN : on_signal);
    if (is("fault-kept")) {
      mask(SIG_BLOCK, SIGUSR1);
    }
    syscall(SYS_tgkill, getpid(), (pid_t)syscall(SYS_gettid), SIGUSR1);
    mask(SIG_BLOCK, SIGUSR1);
    set_action(SIGUSR1, on_signal);
  } else {
    mask(SIG_BLOCK, SIGUSR1);
  }
  sigset_t set;
  sigemptyset(&set);
  sigaddset(&set, SIGUSR1);
  struct timespec timeout = {0, 100000000};
  long result = sigtimedwait(&set, (siginfo_t *)page, &timeout);
  int error = result < 0 ? errno : 0;
  printf("%s: sigtimedwait %ld %s\n", mode, result,
         error == EFAULT ? "EFAULT" : errno_name(error));
  return 0;
}

static int run_signalfd_records(void) {
  set_action(SIGUSR1, on_signal);
  set_action(SIGUSR2, on_signal);
  mask(SIG_BLOCK, SIGUSR1);
  mask(SIG_BLOCK, SIGUSR2);
  sigset_t set;
  sigemptyset(&set);
  sigaddset(&set, SIGUSR1);
  sigaddset(&set, SIGUSR2);
  int fd = signalfd(-1, &set, 0);
  raise(SIGUSR1);
  long got;
  if (is("signalfd-two")) {
    struct signalfd_siginfo records[2];
    got = read(fd, records, sizeof(records));
  } else {
    raise(SIGUSR2);
    char *pages = mmap(NULL, 8192, PROT_READ | PROT_WRITE,
                       MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    mprotect(pages + 4096, 4096, PROT_NONE);
    got = read(fd, pages + 4096 - sizeof(struct signalfd_siginfo),
               2 * sizeof(struct signalfd_siginfo));
  }
  int error = got < 0 ? errno : 0;
  printf("%s: read %ld %s\n", mode, got,
         error == EFAULT ? "EFAULT" : errno_name(error));
  return 0;
}

static int report_pipe[2];

static void *sole_thread(void *unused) {
  (void)unused;
  mask(SIG_UNBLOCK, SIGUSR1);
  pid_t tid = (pid_t)syscall(SYS_gettid);
  if (write(report_pipe[1], &tid, sizeof(tid)) != sizeof(tid)) {
    _exit(2);
  }
  sigset_t set;
  sigemptyset(&set);
  sigaddset(&set, SIGUSR1);
  struct timespec timeout = {0, 300000000};
  long result = sigtimedwait(&set, NULL, &timeout);
  int error = result < 0 ? errno : 0;
  printf("%s: sigtimedwait %ld %s\n", mode, result, errno_name(error));
  exit(0);
}

static int run_exited_leader(void) {
  if (pipe(report_pipe)) {
    perror("pipe");
    return 2;
  }
  pid_t child = fork();
  if (child == 0) {
    set_action(SIGUSR1, SIG_IGN);
    mask(SIG_BLOCK, SIGUSR1);
    pthread_t thread;
    pthread_create(&thread, NULL, sole_thread, NULL);
    pthread_exit(NULL);
  }
  pid_t tid;
  if (read(report_pipe[0], &tid, sizeof(tid)) != sizeof(tid)) {
    return 2;
  }
  sleep_ms(50);
  kill(is("sole-nonleader") ? tid : child, SIGUSR1);
  int status;
  waitpid(child, &status, 0);
  return 0;
}

static int run_init(void) {
  if (pipe(report_pipe)) {
    perror("pipe");
    return 2;
  }
  long init = syscall(SYS_clone, CLONE_NEWPID | SIGCHLD, 0, 0, 0, 0);
  if (init < 0) {
    perror("clone(CLONE_NEWPID)");
    return 2;
  }
  if (init == 0) {
    if (is("init-caught")) {
      set_action(SIGUSR1, on_signal);
    }
    char byte = 1;
    if (write(report_pipe[1], &byte, 1) != 1) {
      _exit(2);
    }
    pid_t sender = -1;
    if (is("init-own")) {
      sender = fork();
      if (sender == 0) {
        sleep_ms(50);
        kill(1, SIGUSR1);
        _exit(0);
      }
    }
    sigset_t set;
    sigemptyset(&set);
    sigaddset(&set, SIGUSR1);
    struct timespec timeout = {0, 300000000};
    long result = sigtimedwait(&set, NULL, &timeout);
    int error = result < 0 ? errno : 0;
    printf("%s: sigtimedwait %ld %s\n", mode, result, errno_name(error));
    if (sender > 0) {
      int status;
      waitpid(sender, &status, 0);
    }
    _exit(0);
  }
  char byte;
  if (read(report_pipe[0], &byte, 1) != 1) {
    return 2;
  }
  if (!is("init-own")) {
    sleep_ms(50);
    kill((pid_t)init, SIGUSR1);
  }
  int status;
  waitpid((pid_t)init, &status, 0);
  return 0;
}

static int run_take_info(void) {
  mask(SIG_BLOCK, SIGUSR1);
  set_action(SIGUSR1, on_signal);
  raise(SIGUSR1);
  sigset_t set;
  sigemptyset(&set);
  sigaddset(&set, SIGUSR1);
  siginfo_t info;
  memset(&info, 0, sizeof(info));
  struct timespec timeout = {1, 0};
  long result = sigtimedwait(&set, &info, &timeout);
  int error = result < 0 ? errno : 0;
  printf("%s: sigtimedwait %ld %s, info signal %d\n", mode, result,
         error == EFAULT ? "EFAULT" : errno_name(error), info.si_signo);
  return 0;
}

static int run_self_take(void) {
  set_action(SIGUSR1, SIG_IGN);
  syscall(SYS_tgkill, getpid(), (pid_t)syscall(SYS_gettid), SIGUSR1);
  mask(SIG_BLOCK, SIGUSR1);
  set_action(SIGUSR1, on_signal);
  raise(SIGUSR1);
  sigset_t set;
  sigemptyset(&set);
  sigaddset(&set, SIGUSR1);
  struct timespec timeout = {1, 0};
  long result = sigtimedwait(&set, NULL, &timeout);
  int error = result < 0 ? errno : 0;
  printf("%s: sigtimedwait %ld %s, handled %d\n", mode, result,
         errno_name(error), atomic_load(&handled));
  return 0;
}

static int run_timer(void) {
  set_action(SIGUSR1, SIG_IGN);
  struct sigevent event;
  memset(&event, 0, sizeof(event));
  event.sigev_notify = SIGEV_SIGNAL;
  event.sigev_signo = SIGUSR1;
  timer_t timer;
  if (timer_create(CLOCK_MONOTONIC, &event, &timer)) {
    perror("timer_create");
    return 2;
  }
  struct itimerspec periodic = {{0, 300000000}, {0, 10000000}};
  timer_settime(timer, 0, &periodic, NULL);
  sleep_ms(50); /* one expiry, ignored */
  if (is("timer-rearm-far")) {
    struct itimerspec far = {{1000, 0}, {1000, 0}};
    timer_settime(timer, 0, &far, NULL);
  } else if (is("timer-rearm-near")) {
    struct itimerspec near = {{0, 300000000}, {0, 1000000}};
    timer_settime(timer, 0, &near, NULL);
    sleep_ms(30); /* a new expiry, still ignored */
  } else if (is("timer-disarm")) {
    struct itimerspec off;
    memset(&off, 0, sizeof(off));
    timer_settime(timer, 0, &off, NULL);
  } else if (is("timer-delete")) {
    timer_delete(timer);
  }
  mask(SIG_BLOCK, SIGUSR1);
  set_action(SIGUSR1, on_signal);
  sigset_t set;
  sigemptyset(&set);
  sigaddset(&set, SIGUSR1);
  struct timespec timeout = {0, 100000000};
  long result = sigtimedwait(&set, NULL, &timeout);
  int error = result < 0 ? errno : 0;
  printf("%s: sigtimedwait %ld %s\n", mode, result, errno_name(error));
  return 0;
}

/* A timer signal the scheduler sends while every thread blocks it waits in
 * the shared queue, and a later sigsuspend that admits it ends at once while a
 * sibling spins on sched_yield (https://github.com/rrnewton/hermit/pull/4314).
 * "armed-posix": a one-shot SIGEV_SIGNAL SIGUSR2 timer; "armed-itimer":
 * ITIMER_REAL, the control; "armed-unignore": the periodic timer's expiry
 * Linux discarded (SIG_IGN, unblocked in main) and re-queues at unignore
 * (posixtimer_sig_unignore) while every thread blocks it. From the Claude
 * re-check of https://github.com/rrnewton/hermit/pull/4351 (N1, N2). */
static atomic_int armed_woke;
static atomic_int armed_spinner_ready;

static void *armed_spin(void *unused) {
  (void)unused;
  if (is("armed-unignore")) {
    mask(SIG_BLOCK, SIGUSR2);
  }
  atomic_store(&armed_spinner_ready, 1);
  while (!atomic_load(&armed_woke)) {
    sched_yield();
  }
  return NULL;
}

static int run_armed(void) {
  int itimer = is("armed-itimer");
  int unignore = is("armed-unignore");
  int signo = itimer ? SIGALRM : SIGUSR2;
  if (unignore) {
    set_action(SIGUSR2, SIG_IGN);
  } else {
    mask(SIG_BLOCK, signo); /* the spinner inherits it */
    set_action(signo, on_signal);
  }
  pthread_t spinner;
  pthread_create(&spinner, NULL, armed_spin, NULL);
  while (!atomic_load(&armed_spinner_ready)) {
    sched_yield();
  }
  if (itimer) {
    struct itimerval once = {{0, 0}, {0, 1000}};
    setitimer(ITIMER_REAL, &once, NULL);
  } else {
    struct sigevent event;
    memset(&event, 0, sizeof(event));
    event.sigev_notify = SIGEV_SIGNAL;
    event.sigev_signo = SIGUSR2;
    timer_t timer;
    if (timer_create(CLOCK_MONOTONIC, &event, &timer)) {
      perror("timer_create");
      return 2;
    }
    /* unignore: first expiry at 10 ms, discarded and parked; no second one */
    struct itimerspec once = {{0, 0}, {0, 1000000}};
    struct itimerspec parked = {{1000, 0}, {0, 10000000}};
    timer_settime(timer, 0, unignore ? &parked : &once, NULL);
  }
  sleep_ms(unignore ? 30 : 20);
  if (unignore) {
    mask(SIG_BLOCK, SIGUSR2);
    set_action(SIGUSR2, on_signal); /* Linux re-queues the parked expiry */
  }
  sigset_t pending;
  sigpending(&pending);
  int was_pending = sigismember(&pending, signo);
  sigset_t none;
  sigemptyset(&none);
  int result = sigsuspend(&none);
  int error = errno;
  atomic_store(&armed_woke, 1);
  pthread_join(spinner, NULL);
  printf("%s: pending %d, sigsuspend %d %s, handled %d\n", mode, was_pending,
         result, errno_name(error), atomic_load(&handled));
  return 0;
}

int main(int argc, char **argv) {
  if (argc != 2) {
    fprintf(stderr, "usage: %s MODE\n", argv[0]);
    return 2;
  }
  mode = argv[1];
  setvbuf(stdout, NULL, _IONBF, 0);
  if (starts("armed-")) {
    return run_armed();
  }
  if (is("self-take")) {
    return run_self_take();
  }
  if (is("take-info")) {
    return run_take_info();
  }
  if (starts("tight-stack-")) {
    return run_tight_stack();
  }
  if (is("fault-partial-16") || is("fault-partial-64")) {
    return run_fault_partial();
  }
  if (starts("init-")) {
    return run_init();
  }
  if (is("sole-nonleader") || is("exited-leader")) {
    return run_exited_leader();
  }
  if (starts("timer-")) {
    return run_timer();
  }
  if (is("itimer-leader")) {
    return run_itimer_leader();
  }
  if (is("xproc")) {
    return run_xproc();
  }
  if (starts("fault-") && !starts("fault-partial-")) {
    return run_fault();
  }
  if (starts("signalfd-") && !is("signalfd-ign") && !is("signalfd-inread") &&
      !is("signalfd-fault-inread") && !signalfd_eintr()) {
    return run_signalfd_records();
  }
  if (is("wait4-timer")) {
    pthread_t thread;
    pthread_create(&thread, NULL, waiter, NULL);
    wait_until_ready();
    struct sigevent event;
    memset(&event, 0, sizeof(event));
    event.sigev_notify = SIGEV_SIGNAL;
    event.sigev_signo = SIGWINCH;
    timer_t timer;
    timer_create(CLOCK_MONOTONIC, &event, &timer);
    struct itimerspec once = {{0, 0}, {0, 50000000}};
    timer_settime(timer, 0, &once, NULL);
    sleep_ms(100);
    set_action(SIGWINCH, on_signal);
    pthread_join(thread, NULL);
    return 0;
  }
  if (strstr(mode, "-ign") || starts("sigpipe") || is("blocked-keeps") ||
      is("kept-after-discard") || is("signalfd-inread") ||
      is("kept-timer-after-discard") || is("signalfd-fault-inread") ||
      fault_discarded() || signalfd_eintr()) {
    set_action(starts("sigpipe") ? SIGPIPE : SIGUSR1, SIG_IGN);
  }
  if (is("futex-flush-resend")) {
    set_action(SIGUSR1, on_h1);
  }
  pthread_t thread;
  pthread_create(&thread, NULL, waiter, NULL);
  if (is("futex-flush-resend")) {
    /* A kept copy, a turn, a flush (SIG_IGN), a discarded copy, then h2. */
    wait_until_ready();
    syscall(SYS_tgkill, getpid(), target_tid, SIGUSR1);
    sched_yield();
    set_action(SIGUSR1, SIG_IGN);
    syscall(SYS_tgkill, getpid(), target_tid, SIGUSR1);
    set_action(SIGUSR1, on_h2);
  } else if (is("kept-after-discard")) {
    wait_until_ready();
    syscall(SYS_tgkill, getpid(), target_tid, SIGUSR1);
    set_action(SIGUSR1, on_signal);
    syscall(SYS_tgkill, getpid(), target_tid, SIGUSR1);
    atomic_store(&sent, 1);
  } else if (is("kept-timer-after-discard")) {
    wait_until_ready();
    syscall(SYS_tgkill, getpid(), target_tid, SIGUSR1);
    set_action(SIGUSR1, on_signal);
    struct sigevent event;
    memset(&event, 0, sizeof(event));
    event.sigev_notify = SIGEV_THREAD_ID;
    event.sigev_signo = SIGUSR1;
    event._sigev_un._tid = target_tid; /* sigev_notify_thread_id */
    timer_t timer;
    timer_create(CLOCK_MONOTONIC, &event, &timer);
    struct itimerspec once = {{0, 0}, {0, 10000000}};
    timer_settime(timer, 0, &once, NULL);
    sleep_ms(50);
  } else if (is("blocked-keeps")) {
    wait_until_ready();
    syscall(SYS_tgkill, getpid(), target_tid, SIGUSR1);
    set_action(SIGUSR1, on_signal);
    atomic_store(&sent, 1);
  } else if (starts("sigpipe")) {
    wait_until_ready();
    syscall(SYS_tgkill, getpid(), target_tid, SIGPIPE);
    atomic_store(&sent, 1);
  } else if (fault_discarded()) {
    wait_until_ready();
    syscall(SYS_tgkill, getpid(), target_tid, SIGUSR1);
    set_action(SIGUSR1, on_signal);
  } else if (signalfd_eintr()) {
    wait_until_ready();
    syscall(SYS_tgkill, getpid(), target_tid, SIGUSR1);
    set_action(SIGUSR1, on_signal);
    set_action(SIGUSR2, on_signal);
    sleep_ms(100);
    syscall(SYS_tgkill, getpid(), target_tid, SIGUSR2);
  } else if (is("signalfd-inread") || is("signalfd-fault-inread")) {
    wait_until_ready();
    syscall(SYS_tgkill, getpid(), target_tid, SIGUSR1);
    set_action(SIGUSR1, on_signal);
    sleep_ms(100);
    syscall(SYS_tgkill, getpid(), target_tid, SIGUSR2);
  } else {
    send_discarded_then_handle();
  }
  pthread_join(thread, NULL);
  return 0;
}
