/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Blocking `waitid` and `wait4` have signal-sensitive contracts covered here:
 * an interrupt must make progress and run its handler, SA_RESTART must run the
 * handler before resuming the wait, non-interrupting default dispositions must
 * not interrupt,
 * blocked signals must remain pending, and an already-ready child status must
 * win when SIGCHLD is pending at the same scheduling boundary.
 */

#define _GNU_SOURCE

#include <errno.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <ucontext.h>
#include <unistd.h>

static volatile sig_atomic_t handler_ran;
static volatile sig_atomic_t handler_target;
static volatile sig_atomic_t handler_expected_wait_target;
static volatile sig_atomic_t handler_replacement_wait_target;
static volatile sig_atomic_t handler_wait_uses_wait4;
static volatile sig_atomic_t handler_saw_expected_wait_target;
static volatile sig_atomic_t restart_handler_ran;
static volatile sig_atomic_t interrupt_handler_ran;

static void on_signal(int signum) {
  (void)signum;
  handler_ran = 1;
}

static void on_signal_kill_target(int signum) {
  (void)signum;
  handler_ran = 1;
  if (handler_target > 0) {
    kill((pid_t)handler_target, SIGKILL);
  }
}

static void on_restart_signal(int signum) {
  (void)signum;
  restart_handler_ran = 1;
}

static void on_interrupt_signal(int signum) {
  (void)signum;
  interrupt_handler_ran = 1;
}

static void note_signal_wait_target(ucontext_t *signal_context) {
  greg_t target =
      signal_context->uc_mcontext
          .gregs[handler_wait_uses_wait4 ? REG_RDI : REG_RSI];
  handler_saw_expected_wait_target =
      target == (greg_t)handler_expected_wait_target;
}

static void on_signal_observe_wait_target(int signum, siginfo_t *info,
                                          void *context) {
  (void)signum;
  (void)info;
  handler_ran = 1;
  note_signal_wait_target((ucontext_t *)context);
}

static void on_signal_change_wait_context(int signum, siginfo_t *info,
                                          void *context) {
  (void)signum;
  (void)info;
  handler_ran = 1;
  ucontext_t *signal_context = (ucontext_t *)context;
  note_signal_wait_target(signal_context);
  signal_context->uc_mcontext.gregs[REG_RAX] = -EINTR;
  signal_context->uc_mcontext.gregs[REG_RIP] += 2;
}

static void on_signal_change_wait_target(int signum, siginfo_t *info,
                                         void *context) {
  (void)signum;
  (void)info;
  handler_ran = 1;
  ucontext_t *signal_context = (ucontext_t *)context;
  note_signal_wait_target(signal_context);
  signal_context->uc_mcontext
      .gregs[handler_wait_uses_wait4 ? REG_RDI : REG_RSI] =
      (greg_t)handler_replacement_wait_target;
  if (handler_replacement_wait_target > 0) {
    kill((pid_t)handler_replacement_wait_target, SIGKILL);
  }
}

static int signal_interrupt(int use_wait4) {
  struct sigaction action;
  memset(&action, 0, sizeof action);
  action.sa_sigaction = on_signal_observe_wait_target;
  sigemptyset(&action.sa_mask);
  action.sa_flags = SA_SIGINFO; /* deliberately NOT SA_RESTART */
  if (sigaction(SIGALRM, &action, NULL) != 0) {
    printf("waitid-signal-interrupt-setup-failed sigaction errno=%d\n", errno);
    return 1;
  }

  pid_t child = fork();
  if (child < 0) {
    printf("waitid-signal-interrupt-setup-failed fork errno=%d\n", errno);
    return 1;
  }
  if (child == 0) {
    /* Never exits on its own, so the parent's wait condition stays unsatisfied
     * and only the signal can end the wait. */
    for (;;) {
      pause();
    }
  }

  handler_expected_wait_target = (sig_atomic_t)child;
  handler_wait_uses_wait4 = use_wait4;
  siginfo_t info;
  memset(&info, 0, sizeof info);
  int status = 0;
  alarm(1);
  int rc = use_wait4 ? waitpid(child, &status, 0)
                     : waitid(P_PID, child, &info, WEXITED);
  int saved_errno = errno;
  alarm(0);

  printf(
      use_wait4
          ? "wait4-signal-interrupt rc=%d errno=%d handler=%d target-match=%d\n"
          : "waitid-signal-interrupt rc=%d errno=%d handler=%d target-match=%d\n",
      rc, rc < 0 ? saved_errno : 0, (int)handler_ran,
      (int)handler_saw_expected_wait_target);

  kill(child, SIGKILL);
  waitpid(child, &status, 0);
  printf(use_wait4 ? "wait4-signal-interrupt-done\n"
                   : "waitid-signal-interrupt-done\n");
  return rc == -1 && saved_errno == EINTR && handler_ran &&
                 handler_saw_expected_wait_target
             ? 0
             : 2;
}

static int child_ready_wins(int use_wait4) {
  struct sigaction action;
  memset(&action, 0, sizeof action);
  action.sa_handler = on_signal;
  sigemptyset(&action.sa_mask);
  action.sa_flags = 0; /* deliberately NOT SA_RESTART */
  if (sigaction(SIGCHLD, &action, NULL) != 0) {
    printf("waitid-ready-wins-setup-failed sigaction errno=%d\n", errno);
    return 1;
  }

  pid_t child = fork();
  if (child < 0) {
    printf("waitid-ready-wins-setup-failed fork errno=%d\n", errno);
    return 1;
  }
  if (child == 0) {
    // Give the parent time to enter waitid. The resulting child exit makes
    // SIGCHLD pending at the same scheduler boundary at which the child's
    // wait status becomes observable.
    const struct timespec delay = {.tv_sec = 0, .tv_nsec = 20 * 1000 * 1000};
    struct timespec remaining = delay;
    while (nanosleep(&remaining, &remaining) != 0 && errno == EINTR) {
    }
    _exit(23);
  }

  siginfo_t info;
  memset(&info, 0, sizeof info);
  int status = 0;
  errno = 0;
  int rc = use_wait4 ? waitpid(child, &status, 0)
                     : waitid(P_PID, child, &info, WEXITED);
  int saved_errno = errno;

  if (use_wait4) {
    printf(
        "wait4-ready-wins rc-ok=%d errno=%d handler=%d pid-match=%d exited=%d status=%d\n",
        rc >= 0, rc < 0 ? saved_errno : 0, (int)handler_ran, rc == child,
        WIFEXITED(status), WIFEXITED(status) ? WEXITSTATUS(status) : -1);
  } else {
    printf(
        "waitid-ready-wins rc=%d errno=%d handler=%d pid-match=%d code=%d status=%d\n",
        rc, rc < 0 ? saved_errno : 0, (int)handler_ran,
        info.si_pid == child, info.si_code, info.si_status);
  }

  int correct = use_wait4
                    ? rc == child && WIFEXITED(status) && WEXITSTATUS(status) == 23
                    : rc == 0 && info.si_pid == child &&
                          info.si_code == CLD_EXITED && info.si_status == 23;
  if (!correct) {
    // The rejected implementation reaches this path: it observes SIGCHLD and
    // returns EINTR without first polling the now-ready child status.
    waitpid(child, NULL, 0);
    return 2;
  }
  return 0;
}

static int signal_restart(int use_wait4) {
  struct sigaction action;
  memset(&action, 0, sizeof action);
  action.sa_handler = on_signal;
  sigemptyset(&action.sa_mask);
  action.sa_flags = SA_RESTART;
  if (sigaction(SIGALRM, &action, NULL) != 0) {
    printf("waitid-signal-restart-setup-failed sigaction errno=%d\n", errno);
    return 1;
  }

  pid_t child = fork();
  if (child < 0) {
    printf("waitid-signal-restart-setup-failed fork errno=%d\n", errno);
    return 1;
  }
  if (child == 0) {
    sleep(2);
    _exit(17);
  }

  siginfo_t info;
  memset(&info, 0, sizeof info);
  int status = 0;
  alarm(1);
  int rc = use_wait4 ? waitpid(child, &status, 0)
                     : waitid(P_PID, child, &info, WEXITED);
  int saved_errno = errno;
  alarm(0);
  if (use_wait4) {
    printf(
        "wait4-signal-restart rc-ok=%d errno=%d handler=%d pid-match=%d exited=%d status=%d\n",
        rc >= 0, rc < 0 ? saved_errno : 0, (int)handler_ran, rc == child,
        WIFEXITED(status), WIFEXITED(status) ? WEXITSTATUS(status) : -1);
  } else {
    printf(
        "waitid-signal-restart rc=%d errno=%d handler=%d pid-match=%d code=%d status=%d\n",
        rc, rc < 0 ? saved_errno : 0, (int)handler_ran,
        info.si_pid == child, info.si_code, info.si_status);
  }

  int correct = use_wait4
                    ? rc == child && WIFEXITED(status) && WEXITSTATUS(status) == 17
                    : rc == 0 && info.si_pid == child &&
                          info.si_code == CLD_EXITED && info.si_status == 17;
  if (!correct) {
    kill(child, SIGKILL);
    waitpid(child, NULL, 0);
    return 2;
  }
  if (!handler_ran) {
    return 3;
  }
  return 0;
}

static int signal_restart_handler_makes_child_waitable(int use_wait4) {
  struct sigaction action;
  memset(&action, 0, sizeof action);
  action.sa_handler = on_signal_kill_target;
  sigemptyset(&action.sa_mask);
  action.sa_flags = SA_RESTART;
  if (sigaction(SIGALRM, &action, NULL) != 0) {
    printf("wait-restart-handler-setup-failed sigaction errno=%d\n", errno);
    return 1;
  }

  pid_t child = fork();
  if (child < 0) {
    printf("wait-restart-handler-setup-failed fork errno=%d\n", errno);
    return 1;
  }
  if (child == 0) {
    for (;;) {
      pause();
    }
  }

  handler_target = (sig_atomic_t)child;
  siginfo_t info;
  memset(&info, 0, sizeof info);
  int status = 0;
  alarm(1);
  int rc = use_wait4 ? waitpid(child, &status, 0)
                     : waitid(P_PID, child, &info, WEXITED);
  int saved_errno = errno;
  alarm(0);
  handler_target = 0;

  if (use_wait4) {
    printf(
        "wait4-restart-handler rc-ok=%d errno=%d handler=%d pid-match=%d signaled=%d signal=%d\n",
        rc >= 0, rc < 0 ? saved_errno : 0, (int)handler_ran, rc == child,
        WIFSIGNALED(status), WIFSIGNALED(status) ? WTERMSIG(status) : 0);
  } else {
    printf(
        "waitid-restart-handler rc=%d errno=%d handler=%d pid-match=%d code=%d status=%d\n",
        rc, rc < 0 ? saved_errno : 0, (int)handler_ran,
        info.si_pid == child, info.si_code, info.si_status);
  }

  int correct = use_wait4
                    ? rc == child && WIFSIGNALED(status) &&
                          WTERMSIG(status) == SIGKILL
                    : rc == 0 && info.si_pid == child &&
                          info.si_code == CLD_KILLED && info.si_status == SIGKILL;
  if (!correct) {
    kill(child, SIGKILL);
    waitpid(child, NULL, 0);
    return 2;
  }
  return handler_ran ? 0 : 3;
}

static int signal_restart_then_interrupt(int use_wait4) {
  struct sigaction restart_action;
  memset(&restart_action, 0, sizeof restart_action);
  restart_action.sa_handler = on_restart_signal;
  sigemptyset(&restart_action.sa_mask);
  restart_action.sa_flags = SA_RESTART;
  struct sigaction interrupt_action;
  memset(&interrupt_action, 0, sizeof interrupt_action);
  interrupt_action.sa_handler = on_interrupt_signal;
  sigemptyset(&interrupt_action.sa_mask);
  if (sigaction(SIGUSR1, &restart_action, NULL) != 0 ||
      sigaction(SIGUSR2, &interrupt_action, NULL) != 0) {
    printf("wait-restart-then-interrupt-setup-failed sigaction errno=%d\n",
           errno);
    return 1;
  }

  pid_t target = fork();
  if (target < 0) {
    printf("wait-restart-then-interrupt-setup-failed target-fork errno=%d\n",
           errno);
    return 1;
  }
  if (target == 0) {
    for (;;) {
      pause();
    }
  }

  pid_t parent = getpid();
  pid_t signaler = fork();
  if (signaler < 0) {
    printf("wait-restart-then-interrupt-setup-failed signaler-fork errno=%d\n",
           errno);
    kill(target, SIGKILL);
    waitpid(target, NULL, 0);
    return 1;
  }
  if (signaler == 0) {
    const struct timespec first_delay = {
        .tv_sec = 0, .tv_nsec = 20 * 1000 * 1000};
    nanosleep(&first_delay, NULL);
    if (kill(parent, SIGUSR1) != 0) {
      _exit(1);
    }
    const struct timespec second_delay = {
        .tv_sec = 0, .tv_nsec = 100 * 1000 * 1000};
    nanosleep(&second_delay, NULL);
    if (kill(parent, SIGUSR2) != 0) {
      _exit(2);
    }
    for (;;) {
      pause();
    }
  }

  siginfo_t info;
  memset(&info, 0, sizeof info);
  int status = 0;
  errno = 0;
  int rc = use_wait4 ? waitpid(target, &status, 0)
                     : waitid(P_PID, target, &info, WEXITED);
  int saved_errno = errno;
  int target_live = kill(target, 0) == 0;
  int sender_live = kill(signaler, 0) == 0;
  printf(use_wait4
             ? "wait4-restart-then-interrupt rc=%d errno=%d restart-handler=%d interrupt-handler=%d target-live=%d sender-live=%d\n"
             : "waitid-restart-then-interrupt rc=%d errno=%d restart-handler=%d interrupt-handler=%d target-live=%d sender-live=%d\n",
         rc, rc < 0 ? saved_errno : 0, (int)restart_handler_ran,
         (int)interrupt_handler_ran, target_live, sender_live);

  kill(signaler, SIGKILL);
  kill(target, SIGKILL);
  waitpid(signaler, NULL, 0);
  waitpid(target, NULL, 0);
  return rc == -1 && saved_errno == EINTR && restart_handler_ran &&
                 interrupt_handler_ran && target_live && sender_live
             ? 0
             : 2;
}

static int signal_restart_handler_changes_context(int use_wait4) {
  struct sigaction action;
  memset(&action, 0, sizeof action);
  action.sa_sigaction = on_signal_change_wait_context;
  sigemptyset(&action.sa_mask);
  action.sa_flags = SA_RESTART | SA_SIGINFO;
  if (sigaction(SIGALRM, &action, NULL) != 0) {
    printf("wait-restart-context-setup-failed sigaction errno=%d\n", errno);
    return 1;
  }

  pid_t child = fork();
  if (child < 0) {
    printf("wait-restart-context-setup-failed fork errno=%d\n", errno);
    return 1;
  }
  if (child == 0) {
    for (;;) {
      pause();
    }
  }

  handler_expected_wait_target = (sig_atomic_t)child;
  handler_wait_uses_wait4 = use_wait4;
  siginfo_t info;
  memset(&info, 0, sizeof info);
  int status = 0;
  alarm(1);
  errno = 0;
  int rc = use_wait4 ? waitpid(child, &status, 0)
                     : waitid(P_PID, child, &info, WEXITED);
  int saved_errno = errno;
  alarm(0);
  printf(use_wait4
             ? "wait4-restart-context rc=%d errno=%d handler=%d target-match=%d\n"
             : "waitid-restart-context rc=%d errno=%d handler=%d target-match=%d\n",
         rc, rc < 0 ? saved_errno : 0, (int)handler_ran,
         (int)handler_saw_expected_wait_target);

  kill(child, SIGKILL);
  waitpid(child, NULL, 0);
  return rc == -1 && saved_errno == EINTR && handler_ran &&
                 handler_saw_expected_wait_target
             ? 0
             : 2;
}

static int signal_restart_handler_changes_wait_target(int use_wait4) {
  struct sigaction action;
  memset(&action, 0, sizeof action);
  action.sa_sigaction = on_signal_change_wait_target;
  sigemptyset(&action.sa_mask);
  action.sa_flags = SA_RESTART | SA_SIGINFO;
  if (sigaction(SIGALRM, &action, NULL) != 0) {
    printf("wait-restart-target-setup-failed sigaction errno=%d\n", errno);
    return 1;
  }

  pid_t original = fork();
  if (original < 0) {
    printf("wait-restart-target-setup-failed original-fork errno=%d\n", errno);
    return 1;
  }
  if (original == 0) {
    for (;;) {
      pause();
    }
  }

  pid_t replacement = fork();
  if (replacement < 0) {
    printf("wait-restart-target-setup-failed replacement-fork errno=%d\n",
           errno);
    kill(original, SIGKILL);
    waitpid(original, NULL, 0);
    return 1;
  }
  if (replacement == 0) {
    for (;;) {
      pause();
    }
  }

  handler_expected_wait_target = (sig_atomic_t)original;
  handler_replacement_wait_target = (sig_atomic_t)replacement;
  handler_wait_uses_wait4 = use_wait4;
  siginfo_t info;
  memset(&info, 0, sizeof info);
  int status = 0;
  alarm(1);
  errno = 0;
  int rc = use_wait4 ? waitpid(original, &status, 0)
                     : waitid(P_PID, original, &info, WEXITED);
  int saved_errno = errno;
  alarm(0);
  int original_live = kill(original, 0) == 0;

  if (use_wait4) {
    printf(
        "wait4-restart-target rc-ok=%d errno=%d handler=%d target-match=%d replacement-match=%d signaled=%d signal=%d original-live=%d\n",
        rc >= 0, rc < 0 ? saved_errno : 0, (int)handler_ran,
        (int)handler_saw_expected_wait_target, rc == replacement,
        WIFSIGNALED(status), WIFSIGNALED(status) ? WTERMSIG(status) : 0,
        original_live);
  } else {
    printf(
        "waitid-restart-target rc=%d errno=%d handler=%d target-match=%d replacement-match=%d code=%d status=%d original-live=%d\n",
        rc, rc < 0 ? saved_errno : 0, (int)handler_ran,
        (int)handler_saw_expected_wait_target, info.si_pid == replacement,
        info.si_code, info.si_status, original_live);
  }

  kill(original, SIGKILL);
  waitpid(original, NULL, 0);
  int correct = use_wait4
                    ? rc == replacement && WIFSIGNALED(status) &&
                          WTERMSIG(status) == SIGKILL
                    : rc == 0 && info.si_pid == replacement &&
                          info.si_code == CLD_KILLED && info.si_status == SIGKILL;
  return correct && handler_ran && handler_saw_expected_wait_target &&
                 original_live
             ? 0
             : 2;
}

static int default_disposition_does_not_interrupt(int use_wait4, int signum) {
  pid_t target = fork();
  if (target < 0) {
    printf("wait-default-disposition-setup-failed target-fork errno=%d\n", errno);
    return 1;
  }
  if (target == 0) {
    for (;;) {
      pause();
    }
  }

  pid_t parent = getpid();
  pid_t signaler = fork();
  if (signaler < 0) {
    printf("wait-default-disposition-setup-failed signaler-fork errno=%d\n", errno);
    kill(target, SIGKILL);
    waitpid(target, NULL, 0);
    return 1;
  }
  if (signaler == 0) {
    const struct timespec delay = {.tv_sec = 0, .tv_nsec = 20 * 1000 * 1000};
    nanosleep(&delay, NULL);
    if (kill(parent, signum) != 0) {
      _exit(1);
    }
    const struct timespec target_delay = {
        .tv_sec = 0, .tv_nsec = 100 * 1000 * 1000};
    nanosleep(&target_delay, NULL);
    if (kill(target, SIGKILL) != 0) {
      _exit(2);
    }
    for (;;) {
      pause();
    }
  }

  siginfo_t info;
  memset(&info, 0, sizeof info);
  int status = 0;
  errno = 0;
  int rc = use_wait4 ? waitpid(target, &status, 0)
                     : waitid(P_PID, target, &info, WEXITED);
  int saved_errno = errno;
  int sender_live = kill(signaler, 0) == 0;

  if (use_wait4) {
    printf(
        "wait4-default-disposition signal=%d rc-ok=%d errno=%d pid-match=%d signaled=%d signal-status=%d sender-live=%d\n",
        signum, rc >= 0, rc < 0 ? saved_errno : 0, rc == target,
        WIFSIGNALED(status), WIFSIGNALED(status) ? WTERMSIG(status) : 0,
        sender_live);
  } else {
    printf(
        "waitid-default-disposition signal=%d rc=%d errno=%d pid-match=%d code=%d signal-status=%d sender-live=%d\n",
        signum, rc, rc < 0 ? saved_errno : 0, info.si_pid == target,
        info.si_code, info.si_status, sender_live);
  }

  kill(signaler, SIGKILL);
  waitpid(signaler, NULL, 0);
  int correct = use_wait4
                    ? rc == target && WIFSIGNALED(status) &&
                          WTERMSIG(status) == SIGKILL
                    : rc == 0 && info.si_pid == target &&
                          info.si_code == CLD_KILLED &&
                          info.si_status == SIGKILL;
  return correct && sender_live ? 0 : 2;
}

static int legacy_waitid_signal(int options) {
  struct sigaction action;
  memset(&action, 0, sizeof action);
  action.sa_handler = on_signal;
  sigemptyset(&action.sa_mask);
  action.sa_flags = 0; /* deliberately NOT SA_RESTART */
  if (sigaction(SIGALRM, &action, NULL) != 0) {
    printf("waitid-legacy-signal-setup-failed sigaction errno=%d\n", errno);
    return 1;
  }

  pid_t child = fork();
  if (child < 0) {
    printf("waitid-legacy-signal-setup-failed fork errno=%d\n", errno);
    return 1;
  }
  if (child == 0) {
    for (;;) {
      pause();
    }
  }

  siginfo_t info;
  memset(&info, 0, sizeof info);
  errno = 0;
  alarm(1);
  int rc = waitid(P_PID, child, &info, options);
  int saved_errno = errno;
  alarm(0);

  if (options == WSTOPPED) {
    printf("waitid-wstopped-signal-interrupt rc=%d errno=%d handler=%d\n",
           rc, rc < 0 ? saved_errno : 0, (int)handler_ran);
  } else {
    printf("waitid-wcontinued-signal-interrupt rc=%d errno=%d handler=%d\n",
           rc, rc < 0 ? saved_errno : 0, (int)handler_ran);
  }

  kill(child, SIGKILL);
  waitpid(child, NULL, 0);
  return rc == -1 && saved_errno == EINTR && handler_ran ? 0 : 2;
}

static int legacy_waitid_signal_context(int options) {
  struct sigaction action;
  memset(&action, 0, sizeof action);
  action.sa_sigaction = on_signal_change_wait_context;
  sigemptyset(&action.sa_mask);
  action.sa_flags = SA_RESTART | SA_SIGINFO;
  if (sigaction(SIGALRM, &action, NULL) != 0) {
    printf("waitid-legacy-context-setup-failed sigaction errno=%d\n", errno);
    return 1;
  }

  pid_t child = fork();
  if (child < 0) {
    printf("waitid-legacy-context-setup-failed fork errno=%d\n", errno);
    return 1;
  }
  if (child == 0) {
    for (;;) {
      pause();
    }
  }

  handler_expected_wait_target = (sig_atomic_t)child;
  handler_wait_uses_wait4 = 0;
  siginfo_t info;
  memset(&info, 0, sizeof info);
  errno = 0;
  alarm(1);
  int rc = waitid(P_PID, child, &info, options);
  int saved_errno = errno;
  alarm(0);

  printf(options == WSTOPPED
             ? "waitid-wstopped-signal-context rc=%d errno=%d handler=%d target-match=%d\n"
             : "waitid-wcontinued-signal-context rc=%d errno=%d handler=%d target-match=%d\n",
         rc, rc < 0 ? saved_errno : 0, (int)handler_ran,
         (int)handler_saw_expected_wait_target);

  kill(child, SIGKILL);
  waitpid(child, NULL, 0);
  return rc == -1 && saved_errno == EINTR && handler_ran &&
                 handler_saw_expected_wait_target
             ? 0
             : 2;
}

static int live_sibling_signal(int mode) {
  struct sigaction action;
  memset(&action, 0, sizeof action);
  action.sa_handler = on_signal;
  sigemptyset(&action.sa_mask);
  action.sa_flags = mode == 1 ? SA_RESTART : 0;
  if (sigaction(SIGUSR1, &action, NULL) != 0 ||
      sigaction(SIGUSR2, &action, NULL) != 0) {
    printf("waitid-live-sibling-setup-failed sigaction errno=%d\n", errno);
    return 1;
  }

  sigset_t preserve_blocked;
  sigset_t original_mask;
  sigemptyset(&preserve_blocked);
  sigaddset(&preserve_blocked, SIGUSR2);
  if (mode == 2 || mode == 3) {
    sigaddset(&preserve_blocked, SIGUSR1);
  }
  if (sigprocmask(SIG_BLOCK, &preserve_blocked, &original_mask) != 0) {
    printf("waitid-live-sibling-setup-failed sigprocmask errno=%d\n", errno);
    return 1;
  }

  pid_t target = fork();
  if (target < 0) {
    printf("waitid-live-sibling-setup-failed target-fork errno=%d\n", errno);
    sigprocmask(SIG_SETMASK, &original_mask, NULL);
    return 1;
  }
  if (target == 0) {
    if (mode != 0) {
      const struct timespec delay = {.tv_sec = 0, .tv_nsec = 100 * 1000 * 1000};
      nanosleep(&delay, NULL);
      _exit(29);
    }
    for (;;) {
      pause();
    }
  }

  pid_t parent = getpid();
  pid_t signaler = fork();
  if (signaler < 0) {
    printf("waitid-live-sibling-setup-failed signaler-fork errno=%d\n", errno);
    kill(target, SIGKILL);
    waitpid(target, NULL, 0);
    sigprocmask(SIG_SETMASK, &original_mask, NULL);
    return 1;
  }
  if (signaler == 0) {
    const struct timespec delay = {.tv_sec = 0, .tv_nsec = 20 * 1000 * 1000};
    nanosleep(&delay, NULL);
    if (kill(parent, SIGUSR2) != 0 || kill(parent, SIGUSR1) != 0) {
      _exit(0);
    }
    /* Stay live after the independently generated signals. The wait must
     * not rely on sender exit or SIGCHLD to make progress. */
    for (;;) {
      pause();
    }
  }

  siginfo_t info;
  memset(&info, 0, sizeof info);
  int wait_status = 0;
  errno = 0;
  int rc = mode == 3 ? waitpid(target, &wait_status, 0)
                     : waitid(P_PID, target, &info, WEXITED);
  int saved_errno = errno;
  sigset_t current_mask;
  sigprocmask(SIG_BLOCK, NULL, &current_mask);
  sigset_t expected_mask = original_mask;
  sigaddset(&expected_mask, SIGUSR2);
  if (mode == 2 || mode == 3) {
    sigaddset(&expected_mask, SIGUSR1);
  }
  int mask_preserved = 1;
  for (int signum = 1; signum < NSIG; ++signum) {
    if (sigismember(&current_mask, signum) !=
        sigismember(&expected_mask, signum)) {
      mask_preserved = 0;
      break;
    }
  }
  sigset_t pending_signals;
  int signals_pending =
      sigpending(&pending_signals) == 0 &&
      sigismember(&pending_signals, SIGUSR1) == 1 &&
      sigismember(&pending_signals, SIGUSR2) == 1;
  int sender_live = kill(signaler, 0) == 0;

  int handler_before_restore = handler_ran;
  if (mode == 1) {
    printf(
        "waitid-live-sibling-restart rc=%d errno=%d handler=%d pid-match=%d code=%d status=%d mask-preserved=%d sender-live=%d\n",
        rc,
        rc < 0 ? saved_errno : 0,
        handler_before_restore,
        info.si_pid == target,
        info.si_code,
        info.si_status,
        mask_preserved,
        sender_live);
  } else if (mode == 2) {
    printf(
        "waitid-live-sibling-blocked rc=%d errno=%d handler=%d pid-match=%d code=%d status=%d mask-preserved=%d sender-live=%d\n",
        rc,
        rc < 0 ? saved_errno : 0,
        handler_before_restore,
        info.si_pid == target,
        info.si_code,
        info.si_status,
        mask_preserved,
        sender_live);
  } else if (mode == 3) {
    printf(
        "wait4-live-sibling-blocked rc-ok=%d errno=%d handler=%d pid-match=%d exited=%d status=%d mask-preserved=%d signals-pending=%d sender-live=%d\n",
        rc >= 0,
        rc < 0 ? saved_errno : 0,
        handler_before_restore,
        rc == target,
        WIFEXITED(wait_status),
        WIFEXITED(wait_status) ? WEXITSTATUS(wait_status) : -1,
        mask_preserved,
        signals_pending,
        sender_live);
  } else {
    printf(
        "waitid-live-sibling rc=%d errno=%d handler=%d mask-preserved=%d sender-live=%d\n",
        rc,
        rc < 0 ? saved_errno : 0,
        handler_before_restore,
        mask_preserved,
        sender_live);
  }

  kill(signaler, SIGKILL);
  kill(target, SIGKILL);
  waitpid(signaler, NULL, 0);
  waitpid(target, NULL, 0);
  sigprocmask(SIG_SETMASK, &original_mask, NULL);
  printf(mode == 3 ? "wait4-live-sibling-done\n"
                   : "waitid-live-sibling-done\n");

  if (!mask_preserved || !sender_live ||
      ((mode == 2 || mode == 3) ? handler_before_restore
                                : !handler_before_restore)) {
    return 2;
  }
  if (mode == 3) {
    return signals_pending && rc == target && WIFEXITED(wait_status) &&
                   WEXITSTATUS(wait_status) == 29
               ? 0
               : 3;
  }
  if (mode != 0) {
    return rc == 0 && info.si_pid == target && info.si_code == CLD_EXITED &&
                   info.si_status == 29
               ? 0
               : 3;
  }
  return rc == -1 && saved_errno == EINTR ? 0 : 4;
}

/*
 * A sibling THREAD, not a sibling process. `pthread_kill` lowers to `tgkill`,
 * which is a different Detcore handler from `kill`: an earlier revision of the
 * waitid wakeup notified the scheduler only from `handle_kill`, so a
 * thread-directed signal to a waiter parked on a child that never exits was
 * recorded nowhere and the wait hung forever. Every fixture above sends from a
 * forked PROCESS, which is why none of them could see it.
 */
static pthread_t waitid_waiter_thread;

static void *thread_signaler(void *unused) {
  (void)unused;
  const struct timespec delay = {.tv_sec = 0, .tv_nsec = 150 * 1000 * 1000};
  nanosleep(&delay, NULL);
  pthread_kill(waitid_waiter_thread, SIGUSR1);
  return NULL;
}

static int live_sibling_thread_signal(void) {
  struct sigaction action;
  memset(&action, 0, sizeof action);
  action.sa_handler = on_signal;
  sigemptyset(&action.sa_mask);
  action.sa_flags = 0; /* deliberately NOT SA_RESTART */
  if (sigaction(SIGUSR1, &action, NULL) != 0) {
    printf("waitid-thread-sibling-setup-failed sigaction errno=%d\n", errno);
    return 1;
  }

  pid_t target = fork();
  if (target < 0) {
    printf("waitid-thread-sibling-setup-failed fork errno=%d\n", errno);
    return 1;
  }
  if (target == 0) {
    for (;;) {
      pause();
    }
  }

  waitid_waiter_thread = pthread_self();
  pthread_t signaler;
  if (pthread_create(&signaler, NULL, thread_signaler, NULL) != 0) {
    printf("waitid-thread-sibling-setup-failed pthread_create errno=%d\n", errno);
    kill(target, SIGKILL);
    return 1;
  }

  siginfo_t info;
  memset(&info, 0, sizeof info);
  errno = 0;
  int rc = waitid(P_PID, target, &info, WEXITED);
  int captured = errno;

  printf("waitid-thread-sibling rc=%d errno=%d handler=%d\n", rc,
         rc < 0 ? captured : 0, handler_ran);

  pthread_join(signaler, NULL);
  kill(target, SIGKILL);
  int status = 0;
  waitpid(target, &status, 0);
  printf("waitid-thread-sibling-done\n");
  return 0;
}

int main(int argc, char **argv) {
  if (argc == 1) {
    return signal_interrupt(0);
  }
  if (argc == 2 && strcmp(argv[1], "--child-ready-wins") == 0) {
    return child_ready_wins(0);
  }
  if (argc == 2 && strcmp(argv[1], "--signal-restart") == 0) {
    return signal_restart(0);
  }
  if (argc == 2 && strcmp(argv[1], "--wait4-signal-interrupt") == 0) {
    return signal_interrupt(1);
  }
  if (argc == 2 && strcmp(argv[1], "--wait4-child-ready-wins") == 0) {
    return child_ready_wins(1);
  }
  if (argc == 2 && strcmp(argv[1], "--wait4-signal-restart") == 0) {
    return signal_restart(1);
  }
  if (argc == 2 && strcmp(argv[1], "--signal-restart-handler") == 0) {
    return signal_restart_handler_makes_child_waitable(0);
  }
  if (argc == 2 && strcmp(argv[1], "--wait4-signal-restart-handler") == 0) {
    return signal_restart_handler_makes_child_waitable(1);
  }
  if (argc == 2 && strcmp(argv[1], "--signal-restart-then-interrupt") == 0) {
    return signal_restart_then_interrupt(0);
  }
  if (argc == 2 && strcmp(argv[1], "--wait4-signal-restart-then-interrupt") == 0) {
    return signal_restart_then_interrupt(1);
  }
  if (argc == 2 && strcmp(argv[1], "--signal-restart-context") == 0) {
    return signal_restart_handler_changes_context(0);
  }
  if (argc == 2 && strcmp(argv[1], "--wait4-signal-restart-context") == 0) {
    return signal_restart_handler_changes_context(1);
  }
  if (argc == 2 && strcmp(argv[1], "--signal-restart-target") == 0) {
    return signal_restart_handler_changes_wait_target(0);
  }
  if (argc == 2 && strcmp(argv[1], "--wait4-signal-restart-target") == 0) {
    return signal_restart_handler_changes_wait_target(1);
  }
  if (argc == 3 && strcmp(argv[1], "--waitid-default-disposition") == 0) {
    return default_disposition_does_not_interrupt(0, atoi(argv[2]));
  }
  if (argc == 3 && strcmp(argv[1], "--wait4-default-disposition") == 0) {
    return default_disposition_does_not_interrupt(1, atoi(argv[2]));
  }
  if (argc == 2 && strcmp(argv[1], "--waitid-wcontinued-signal-interrupt") == 0) {
    return legacy_waitid_signal(WCONTINUED);
  }
  if (argc == 2 && strcmp(argv[1], "--waitid-wstopped-signal-interrupt") == 0) {
    return legacy_waitid_signal(WSTOPPED);
  }
  if (argc == 2 && strcmp(argv[1], "--waitid-wcontinued-signal-context") == 0) {
    return legacy_waitid_signal_context(WCONTINUED);
  }
  if (argc == 2 && strcmp(argv[1], "--waitid-wstopped-signal-context") == 0) {
    return legacy_waitid_signal_context(WSTOPPED);
  }
  if (argc == 2 && strcmp(argv[1], "--live-sibling-signal") == 0) {
    return live_sibling_signal(0);
  }
  if (argc == 2 && strcmp(argv[1], "--live-sibling-signal-restart") == 0) {
    return live_sibling_signal(1);
  }
  if (argc == 2 && strcmp(argv[1], "--live-sibling-signal-blocked") == 0) {
    return live_sibling_signal(2);
  }
  if (argc == 2 && strcmp(argv[1], "--live-sibling-thread-signal") == 0) {
    return live_sibling_thread_signal();
  }
  if (argc == 2 && strcmp(argv[1], "--wait4-live-sibling-signal-blocked") == 0) {
    return live_sibling_signal(3);
  }
  fprintf(stderr, "usage: %s [--child-ready-wins|--signal-restart|--wait4-signal-interrupt|--wait4-child-ready-wins|--wait4-signal-restart|--signal-restart-handler|--wait4-signal-restart-handler|--signal-restart-then-interrupt|--wait4-signal-restart-then-interrupt|--signal-restart-context|--wait4-signal-restart-context|--signal-restart-target|--wait4-signal-restart-target|--waitid-default-disposition SIGNAL|--wait4-default-disposition SIGNAL|--waitid-wcontinued-signal-interrupt|--waitid-wstopped-signal-interrupt|--waitid-wcontinued-signal-context|--waitid-wstopped-signal-context|--live-sibling-signal|--live-sibling-signal-restart|--live-sibling-signal-blocked|--live-sibling-thread-signal|--wait4-live-sibling-signal-blocked]\n", argv[0]);
  return 64;
}
