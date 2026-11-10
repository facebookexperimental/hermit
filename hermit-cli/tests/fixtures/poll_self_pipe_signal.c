/* Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 * Licensed under the BSD-style license in the LICENSE file.
 *
 * Linux v7.1.3, fs/pipe.c:388 returns ERESTARTSYS when an empty read is
 * interrupted. arch/x86/kernel/signal.c:272 applies SA_RESTART. A handler
 * can write this self-pipe even though no ordinary guest thread is runnable.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <poll.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>

static int channel[2];
static volatile sig_atomic_t handled;
static int write_byte = 1;
static int one_shot;

static void handle_signal(int signal) {
  (void)signal;
  const int saved_errno = errno;
  const char value = 'x';
  if (write_byte && write(channel[1], &value, 1) != 1)
    _exit(4);
  handled++;
  if (one_shot) {
    const char marker[] = "HANDLED count=1\n";
    if (handled != 1 || write(STDOUT_FILENO, marker, sizeof(marker) - 1) !=
                            (ssize_t)(sizeof(marker) - 1))
      _exit(6);
  }
  errno = saved_errno;
}

static int close_other_handler_progress(void) {
  /* Default SIGTERM permits bounded native cleanup. Default SIGSTKFLT
   * cannot run a handler either, and Hermit's preemption reserves it. */
  struct sigaction terminate = {0};
  terminate.sa_handler = SIG_DFL;
  uint64_t mask = UINT64_MAX & ~(UINT64_C(1) << (SIGKILL - 1)) &
                  ~(UINT64_C(1) << (SIGSTOP - 1)) &
                  ~(UINT64_C(1) << (SIGSTKFLT - 1)) &
                  ~(UINT64_C(1) << (SIGTERM - 1)) &
                  ~(UINT64_C(1) << (SIGUSR1 - 1));
  uint64_t observed = 0;
  if (sigaction(SIGTERM, &terminate, NULL) != 0 ||
      sigaction(SIGSTKFLT, &terminate, NULL) != 0 ||
      syscall(SYS_rt_sigprocmask, SIG_SETMASK, &mask, NULL, sizeof(mask)) != 0 ||
      syscall(SYS_rt_sigprocmask, SIG_SETMASK, NULL, &observed,
              sizeof(observed)) != 0 || observed != mask) {
    fprintf(stderr, "could not close handler progress: wanted=%016llx got=%016llx\n",
            (unsigned long long)mask, (unsigned long long)observed);
    return 0;
  }
  return 1;
}

int main(int argc, char **argv) {
  if (argc != 2 || pipe(channel) != 0)
    return 2;
  one_shot = strncmp(argv[1], "caught-once-", 12) == 0;
  write_byte = strcmp(argv[1], "caught-once-no-write") != 0;
  const int use_poll = strstr(argv[1], "-poll") != NULL;
  const int use_ppoll = strstr(argv[1], "-ppoll") != NULL;
  const int restart = strcmp(argv[1], "caught-no-restart") != 0;
  struct sigaction action;
  memset(&action, 0, sizeof(action));
  action.sa_handler = strncmp(argv[1], "ignored", 7) == 0 ? SIG_IGN : handle_signal;
  action.sa_flags = (restart ? SA_RESTART : 0) | (one_shot ? SA_RESETHAND : 0);
  if (sigaction(SIGUSR1, &action, NULL) != 0)
    return 3;
  /* SA_RESETHAND resets the disposition before entering the handler (Linux
   * v6.12 kernel/signal.c:2817-2824). The host must also witness its return;
   * a reset disposition alone is not evidence that the handler finished. */
  if (one_shot && !close_other_handler_progress())
    return 3;
  if (strncmp(argv[1], "blocked", 7) == 0) {
    sigset_t mask;
    sigemptyset(&mask);
    sigaddset(&mask, SIGUSR1);
    if (sigprocmask(SIG_BLOCK, &mask, NULL) != 0)
      return 3;
  }
  puts("READY");
  if (one_shot || use_poll || use_ppoll)
    printf("WAIT fd=%d\n", channel[0]);
  fflush(stdout);
  char value = 0;
  struct pollfd waiting = {.fd = channel[0], .events = POLLIN};
  ssize_t first = use_poll ? poll(&waiting, 1, -1)
                  : use_ppoll ? ppoll(&waiting, 1, NULL, NULL)
                              : read(channel[0], &value, 1);
  int error = first < 0 ? errno : 0;
  ssize_t count = first;
  if (((use_poll || use_ppoll) && first > 0) || (first < 0 && error == EINTR))
    count = read(channel[0], &value, 1);
  printf("RESULT first=%zd errno=%d count=%zd value=%c handled=%d\n",
         first, error, count, value, handled);
  return count == 1 && value == 'x' && handled == 1 &&
                 (use_poll || use_ppoll || !restart
                      ? first == -1 && error == EINTR
                      : first == 1)
             ? 0
             : 5;
}
