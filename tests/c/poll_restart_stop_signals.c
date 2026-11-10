// A poll with a positive timeout that a job-control stop ends in the kernel
// returns ERESTART_RESTARTBLOCK, and with no handler to run Linux restarts it
// through restart_syscall until the end time it kept (do_restart_poll). A child
// stops and continues its parent while the parent polls an empty pipe for
// 300 ms, then, gap_ms later, sends SIGUSR1 and SIGUSR2, which have handlers:
// they reach the parent around the restart, which then returns EINTR after
// both handlers ran, and the parent polls again until the timeout.
//
// usage: poll_restart_stop_signals [rounds [gap_ms]]
#define _GNU_SOURCE
#include <errno.h>
#include <poll.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

static volatile sig_atomic_t usr1;
static volatile sig_atomic_t usr2;

static void on_usr1(int sig) {
  (void)sig;
  usr1++;
}

static void on_usr2(int sig) {
  (void)sig;
  usr2++;
}

// A poll of no descriptors: it waits in the kernel, on host time, where
// Hermit records a nanosleep on virtual time.
static void sleep_ms(int ms) {
  while (poll(NULL, 0, ms) < 0 && errno == EINTR) {
  }
}

int main(int argc, char **argv) {
  int rounds = argc > 1 ? atoi(argv[1]) : 3;
  int gap = argc > 2 ? atoi(argv[2]) : 2;
  struct sigaction sa;
  memset(&sa, 0, sizeof(sa));
  sa.sa_handler = on_usr1;
  sigaction(SIGUSR1, &sa, NULL);
  sa.sa_handler = on_usr2;
  sigaction(SIGUSR2, &sa, NULL);
  for (int round = 0; round < rounds; round++) {
    int empty[2], release[2];
    if (pipe(empty) != 0 || pipe(release) != 0) {
      perror("pipe");
      return 1;
    }
    pid_t parent = getpid();
    pid_t child = fork();
    if (child < 0) {
      perror("fork");
      return 1;
    }
    if (child == 0) {
      close(release[1]);
      sleep_ms(30);
      kill(parent, SIGSTOP);
      kill(parent, SIGCONT);
      // Let the stopped poll restart first, so the caught signals arrive
      // around its restart.
      if (gap > 0) {
        sleep_ms(gap);
      }
      kill(parent, SIGUSR1);
      kill(parent, SIGUSR2);
      // Exit only after the parent's polls end, so no SIGCHLD reaches them.
      char byte;
      while (read(release[0], &byte, 1) < 0 && errno == EINTR) {
      }
      _exit(0);
    }
    close(release[0]);
    struct pollfd pfd = {.fd = empty[0], .events = POLLIN};
    int interrupted = 0;
    int r;
    while ((r = poll(&pfd, 1, 300)) < 0 && errno == EINTR) {
      interrupted = 1;
    }
    char ch = 'x';
    if (write(release[1], &ch, 1) != 1) {
      perror("write");
    }
    int status;
    waitpid(child, &status, 0);
    printf("round %d: poll=%d revents=%#x interrupted=%d\n", round, r, pfd.revents,
           interrupted);
    close(empty[0]);
    close(empty[1]);
    close(release[1]);
  }
  printf("usr1=%d usr2=%d\n", (int)usr1, (int)usr2);
  return 0;
}
