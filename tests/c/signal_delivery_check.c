/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// Caught signal deliveries that Hermit's Detcore checks against the kernel
// before its signal-state copy records them (A5 step 3.2,
// https://github.com/rrnewton/hermit/issues/4485). Each mode makes one
// delivery of a known shape, from a single thread, and checks what Linux
// guarantees about it: that the handler ran once, the mask it ran under, the
// interrupted call's result or restart, and the action afterwards. It prints
// "OK <mode>" and exits 0, or prints what differed and exits 1.
//
// Signals come from the thread itself (raise, sigqueue) or from the
// scheduler (an ITIMER_REAL expiry), never from outside, so every run is
// deterministic.

#include <errno.h>
#include <fcntl.h>
#include <setjmp.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/resource.h>
#include <sys/time.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static volatile sig_atomic_t handled;
// Written by the handler: a lock-free atomic and sig_atomic_t objects.
static _Atomic uint64_t handler_mask;
_Static_assert(ATOMIC_LONG_LOCK_FREE == 2, "the handler's atomic must be lock-free");
static volatile sig_atomic_t handler_value;
static volatile sig_atomic_t handler_values[2];
static volatile sig_atomic_t write_end = -1;
static sigjmp_buf recovery;
// A null pointer the compiler cannot see through, for a deliberate fault.
static int* volatile nowhere;

static uint64_t current_mask(void) {
  sigset_t now;
  uint64_t bits = 0;
  if (sigprocmask(SIG_BLOCK, NULL, &now) != 0) {
    _exit(2);
  }
  for (int sig = 1; sig <= 64; sig++) {
    if (sigismember(&now, sig) == 1) {
      bits |= 1ull << (sig - 1);
    }
  }
  return bits;
}

static uint64_t bit(int sig) {
  return 1ull << (sig - 1);
}

static void on_signal(int sig, siginfo_t* info, void* context) {
  (void)sig;
  (void)context;
  if (handled < 2) {
    handler_values[handled] = info->si_value.sival_int;
  }
  handled++;
  handler_mask = current_mask();
  handler_value = info->si_value.sival_int;
  if (write_end >= 0) {
    char byte = 'x';
    if (write(write_end, &byte, 1) != 1) {
      _exit(3);
    }
  }
}

// Catches `sig`, with SIGUSR2 in its sa_mask unless `empty_mask`.
static void catch_signal_with(int sig, int flags, int empty_mask) {
  struct sigaction action;
  memset(&action, 0, sizeof action);
  action.sa_sigaction = on_signal;
  action.sa_flags = SA_SIGINFO | flags;
  sigemptyset(&action.sa_mask);
  if (!empty_mask) {
    sigaddset(&action.sa_mask, SIGUSR2);
  }
  if (sigaction(sig, &action, NULL) != 0) {
    perror("sigaction");
    exit(2);
  }
}

// Catches `sig` with SIGUSR2 in its sa_mask.
static void catch_signal(int sig, int flags) {
  catch_signal_with(sig, flags, 0);
}

static void on_fault(int sig) {
  (void)sig;
  siglongjmp(recovery, 1);
}

static void set_mask(uint64_t bits) {
  sigset_t set;
  sigemptyset(&set);
  for (int sig = 1; sig <= 64; sig++) {
    if (bits & bit(sig)) {
      sigaddset(&set, sig);
    }
  }
  if (sigprocmask(SIG_SETMASK, &set, NULL) != 0) {
    perror("sigprocmask");
    exit(2);
  }
}

static void arm_alarm(void) {
  struct itimerval timer = {{0, 0}, {0, 20000}};
  if (setitimer(ITIMER_REAL, &timer, NULL) != 0) {
    perror("setitimer");
    exit(2);
  }
}

static int failures;

static void expect(const char* what, long long actual, long long expected) {
  if (actual != expected) {
    printf("FAIL %s: %lld, expected %lld\n", what, actual, expected);
    failures++;
  }
}

int main(int argc, char** argv) {
  const char* mode = argc > 1 ? argv[1] : "";
  const uint64_t own = bit(SIGINT);
  const uint64_t sa_mask = bit(SIGUSR2);
  set_mask(own);
  long result = 0;
  int error = 0;

  if (!strcmp(mode, "raise")) {
    catch_signal(SIGUSR1, 0);
    result = raise(SIGUSR1);
    expect("raise", result, 0);
    expect("handler mask", (long long)handler_mask, (long long)(own | sa_mask | bit(SIGUSR1)));
  } else if (!strcmp(mode, "pause")) {
    catch_signal(SIGALRM, 0);
    arm_alarm();
    result = pause();
    error = errno;
    expect("pause", result, -1);
    expect("pause errno", error, EINTR);
  } else if (!strcmp(mode, "nanosleep")) {
    catch_signal(SIGALRM, 0);
    arm_alarm();
    struct timespec duration = {5, 0};
    result = nanosleep(&duration, NULL);
    error = errno;
    expect("nanosleep", result, -1);
    expect("nanosleep errno", error, EINTR);
  } else if (!strcmp(mode, "read") || !strcmp(mode, "read-restart")) {
    int restart = !strcmp(mode, "read-restart");
    catch_signal(SIGALRM, restart ? SA_RESTART : 0);
    int pipe_ends[2];
    if (pipe(pipe_ends) != 0) {
      perror("pipe");
      return 2;
    }
    // With SA_RESTART the read restarts, and the handler's byte ends it.
    if (restart) {
      write_end = pipe_ends[1];
    }
    arm_alarm();
    char byte;
    result = read(pipe_ends[0], &byte, 1);
    error = errno;
    if (restart) {
      expect("restarted read", result, 1);
    } else {
      expect("read", result, -1);
      expect("read errno", error, EINTR);
    }
  } else if (!strcmp(mode, "spin")) {
    catch_signal(SIGALRM, 0);
    arm_alarm();
    volatile unsigned long spins = 0;
    while (!handled) {
      spins++;
    }
  } else if (!strcmp(mode, "suspend")) {
    catch_signal(SIGALRM, 0);
    set_mask(own | bit(SIGALRM));
    arm_alarm();
    sigset_t temporary;
    sigemptyset(&temporary);
    result = sigsuspend(&temporary);
    error = errno;
    expect("sigsuspend", result, -1);
    expect("sigsuspend errno", error, EINTR);
    expect("handler mask", (long long)handler_mask, (long long)(sa_mask | bit(SIGALRM)));
    expect("mask after sigsuspend", (long long)current_mask(), (long long)(own | bit(SIGALRM)));
  } else if (!strcmp(mode, "realtime")) {
    catch_signal(SIGRTMIN + 3, 0);
    union sigval value = {.sival_int = 7};
    result = sigqueue(getpid(), SIGRTMIN + 3, value);
    expect("sigqueue", result, 0);
    expect("handler value", handler_value, 7);
    expect(
        "handler mask",
        (long long)handler_mask,
        (long long)(own | sa_mask | bit(SIGRTMIN + 3)));
  } else if (!strcmp(mode, "resethand")) {
    // The first delivery runs the handler and resets the action; the second
    // takes SIGWINCH's default action, which ignores it.
    catch_signal(SIGWINCH, SA_RESETHAND);
    raise(SIGWINCH);
    struct sigaction now;
    sigaction(SIGWINCH, NULL, &now);
    expect("action after SA_RESETHAND", now.sa_handler == SIG_DFL, 1);
    raise(SIGWINCH);
  } else if (!strcmp(mode, "nodefer")) {
    catch_signal(SIGUSR1, SA_NODEFER);
    raise(SIGUSR1);
    expect("handler mask", (long long)handler_mask, (long long)(own | sa_mask));
  } else if (!strcmp(mode, "two-pending")) {
    // Two caught signals pending at one exit: the kernel delivers the second
    // right after building the first one's frame.
    catch_signal_with(SIGUSR1, 0, 1);
    catch_signal_with(SIGWINCH, 0, 1);
    set_mask(own | bit(SIGUSR1) | bit(SIGWINCH));
    raise(SIGUSR1);
    raise(SIGWINCH);
    sigset_t temporary;
    sigemptyset(&temporary);
    result = sigsuspend(&temporary);
    error = errno;
    expect("sigsuspend", result, -1);
    expect("sigsuspend errno", error, EINTR);
    expect("handled twice", handled, 2);
    handled = 1;
  } else if (!strcmp(mode, "blocked-sleep")) {
    // A blocked signal does not end a sleep; it is delivered at the unblock.
    catch_signal(SIGALRM, 0);
    set_mask(own | bit(SIGALRM));
    arm_alarm();
    struct timespec duration = {0, 50000000};
    result = nanosleep(&duration, NULL);
    expect("blocked nanosleep", result, 0);
    expect("handled while blocked", handled, 0);
    sigset_t pending;
    sigpending(&pending);
    expect("pending after the sleep", sigismember(&pending, SIGALRM), 1);
    set_mask(own);
  } else if (!strcmp(mode, "default-ignored")) {
    // SIGWINCH's default action is to ignore it.
    raise(SIGWINCH);
    expect("handled", handled, 0);
    handled = 1;
  } else if (!strcmp(mode, "default-fatal")) {
    pid_t child = fork();
    if (child == 0) {
      raise(SIGTERM);
      _exit(9);
    }
    int status = 0;
    expect("waitpid", waitpid(child, &status, 0), child);
    expect("killed by SIGTERM", WIFSIGNALED(status) && WTERMSIG(status) == SIGTERM, 1);
    handled = 1;
  } else if (!strcmp(mode, "coalesce")) {
    // Two sends of one standard signal while it is blocked are one pending
    // signal, delivered once at the unblock.
    catch_signal(SIGUSR1, 0);
    set_mask(own | bit(SIGUSR1));
    raise(SIGUSR1);
    raise(SIGUSR1);
    set_mask(own);
  } else if (!strcmp(mode, "realtime-direct")) {
    // Two queued sends of one realtime signal, unblocked: each is delivered
    // as its send returns, in order.
    catch_signal(SIGRTMIN + 3, 0);
    union sigval first = {.sival_int = 1};
    union sigval second = {.sival_int = 2};
    sigqueue(getpid(), SIGRTMIN + 3, first);
    sigqueue(getpid(), SIGRTMIN + 3, second);
    expect("handled twice", handled, 2);
    expect("first value", handler_values[0], 1);
    expect("second value", handler_values[1], 2);
    handled = 1;
  } else if (!strcmp(mode, "partial-write")) {
    // A write that has moved bytes when a caught signal arrives returns the
    // count, with no EINTR: the pipe takes what fits, and nobody reads.
    catch_signal(SIGALRM, 0);
    int pipe_ends[2];
    if (pipe(pipe_ends) != 0) {
      perror("pipe");
      return 2;
    }
    long capacity = fcntl(pipe_ends[1], F_GETPIPE_SZ);
    static char buffer[1 << 20];
    arm_alarm();
    result = write(pipe_ends[1], buffer, sizeof buffer);
    expect("partial write", result, capacity);
  } else if (!strcmp(mode, "realtime-two")) {
    // Two queued instances of one realtime signal, delivered in order.
    catch_signal(SIGRTMIN + 3, 0);
    set_mask(own | bit(SIGRTMIN + 3));
    union sigval first = {.sival_int = 1};
    union sigval second = {.sival_int = 2};
    sigqueue(getpid(), SIGRTMIN + 3, first);
    sigqueue(getpid(), SIGRTMIN + 3, second);
    set_mask(own);
    expect("handled twice", handled, 2);
    expect("first value", handler_values[0], 1);
    expect("second value", handler_values[1], 2);
    handled = 1;
  } else if (!strcmp(mode, "child-wait-restart")) {
    // The child's SIGUSR1 interrupts the parent's wait, which SA_RESTART
    // restarts until the child exits.
    catch_signal(SIGUSR1, SA_RESTART);
    pid_t child = fork();
    if (child == 0) {
      // Long enough for the parent to be waiting when the signal comes.
      struct timespec pause = {0, 10000000};
      nanosleep(&pause, NULL);
      kill(getppid(), SIGUSR1);
      // Still alive, so the signal interrupts the wait, which restarts.
      nanosleep(&pause, NULL);
      _exit(3);
    }
    int status = 0;
    result = waitpid(child, &status, 0);
    expect("waitpid", result, child);
    expect("child exit status", WIFEXITED(status) ? WEXITSTATUS(status) : -1, 3);
  } else if (!strcmp(mode, "frame-fault") || !strcmp(mode, "frame-fault-nodefer")) {
    // SIGUSR1's frame goes on an alternate stack that cannot be written: the
    // kernel raises SIGSEGV, and SA_RESETHAND still resets the action. With
    // SA_NODEFER and an empty sa_mask, the handler's mask equals the frame's.
    int nodefer = !strcmp(mode, "frame-fault-nodefer");
    void* stack = mmap(NULL, 65536, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (stack == MAP_FAILED) {
      perror("mmap");
      return 2;
    }
    stack_t alternate = {.ss_sp = stack, .ss_size = 65536, .ss_flags = 0};
    if (sigaltstack(&alternate, NULL) != 0) {
      perror("sigaltstack");
      return 2;
    }
    struct sigaction fault;
    memset(&fault, 0, sizeof fault);
    fault.sa_handler = on_fault;
    fault.sa_flags = SA_NODEFER;
    sigemptyset(&fault.sa_mask);
    sigaction(SIGSEGV, &fault, NULL);
    catch_signal_with(SIGUSR1, SA_ONSTACK | SA_RESETHAND | (nodefer ? SA_NODEFER : 0), nodefer);
    if (sigsetjmp(recovery, 1) == 0) {
      raise(SIGUSR1);
      expect("reached after the fault", 0, 1);
    }
    struct sigaction now;
    sigaction(SIGUSR1, NULL, &now);
    expect("action after the frame fault", now.sa_handler == SIG_DFL, 1);
    expect("handled", handled, 0);
    handled = 1;
  } else if (!strcmp(mode, "forced-ignored") || !strcmp(mode, "forced-blocked")) {
    // A fault forces SIGSEGV: Linux resets an ignored action, or a caught one
    // the thread blocks, to the default, and unblocks it, which kills the
    // child.
    int blocked = !strcmp(mode, "forced-blocked");
    pid_t child = fork();
    if (child == 0) {
      struct rlimit no_core = {0, 0};
      setrlimit(RLIMIT_CORE, &no_core);
      if (blocked) {
        catch_signal(SIGSEGV, 0);
        set_mask(own | bit(SIGSEGV));
      } else {
        signal(SIGSEGV, SIG_IGN);
      }
      *nowhere = 1;
      _exit(9);
    }
    int status = 0;
    expect("waitpid", waitpid(child, &status, 0), child);
    expect("killed by SIGSEGV", WIFSIGNALED(status) && WTERMSIG(status) == SIGSEGV, 1);
    handled = 1;
  } else if (!strcmp(mode, "ignored")) {
    signal(SIGUSR1, SIG_IGN);
    raise(SIGUSR1);
    expect("handled", handled, 0);
  } else {
    printf("unknown mode %s\n", mode);
    return 2;
  }
  if (strcmp(mode, "ignored")) {
    expect("handled", handled, 1);
  }
  if (failures) {
    return 1;
  }
  printf("OK %s\n", mode);
  return 0;
}
