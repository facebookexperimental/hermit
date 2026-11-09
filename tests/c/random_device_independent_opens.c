/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Every read of a Linux random device is a fresh draw, so two opens of
 * /dev/urandom never return the same bytes. Programs rely on that: libstdc++'s
 * std::random_device opens /dev/urandom for each object when the CPU does not
 * advertise RDRAND (Hermit hides it), and peers that elect a leader by
 * exchanging such draws assume the two values differ.
 *
 * Each check below prints one line that is the same natively and under Hermit
 * (no byte values are printed), and the program exits with the number of
 * failed checks:
 *
 *   election: two threads draw from their own opens of /dev/urandom, swap the
 *     values over an AF_UNIX stream socket and retry on a tie, as a symmetric
 *     leader election does. Equal draws on every attempt mean the peers can
 *     never pick a leader.
 *   sequential: one thread opens /dev/urandom twice.
 *   fork: a parent and its child each open /dev/urandom after the fork.
 */

#include <fcntl.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/wait.h>
#include <unistd.h>

enum { ATTEMPTS = 8 };

/* One draw from a new open of /dev/urandom, as std::random_device makes it. */
static int draw(uint64_t* value) {
  int fd = open("/dev/urandom", O_RDONLY | O_CLOEXEC);
  if (fd < 0) {
    return -1;
  }
  ssize_t got = read(fd, value, sizeof *value);
  close(fd);
  return got == (ssize_t)sizeof *value ? 0 : -1;
}

static int send_value(int fd, uint64_t value) {
  return write(fd, &value, sizeof value) == (ssize_t)sizeof value ? 0 : -1;
}

static int receive_value(int fd, uint64_t* value) {
  return read(fd, value, sizeof *value) == (ssize_t)sizeof *value ? 0 : -1;
}

struct peer {
  int socket;
  /* 1: elected leader, 0: follower, -1: tie on every attempt, -2: I/O error */
  int outcome;
};

static void* elect(void* argument) {
  struct peer* peer = argument;
  peer->outcome = -1;
  for (int attempt = 0; attempt < ATTEMPTS; attempt++) {
    uint64_t mine;
    uint64_t theirs;
    if (draw(&mine) != 0 || send_value(peer->socket, mine) != 0 ||
        receive_value(peer->socket, &theirs) != 0) {
      peer->outcome = -2;
      break;
    }
    if (mine != theirs) {
      peer->outcome = mine < theirs;
      break;
    }
  }
  return NULL;
}

static int check_election(void) {
  int sockets[2];
  if (socketpair(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0, sockets) != 0) {
    puts("election: socketpair failed");
    return 1;
  }
  struct peer peers[2] = {{sockets[0], 0}, {sockets[1], 0}};
  pthread_t threads[2];
  for (int i = 0; i < 2; i++) {
    if (pthread_create(&threads[i], NULL, elect, &peers[i]) != 0) {
      puts("election: pthread_create failed");
      return 1;
    }
  }
  for (int i = 0; i < 2; i++) {
    pthread_join(threads[i], NULL);
  }
  close(sockets[0]);
  close(sockets[1]);
  if (peers[0].outcome == -2 || peers[1].outcome == -2) {
    puts("election: I/O error");
    return 1;
  }
  if (peers[0].outcome < 0 || peers[1].outcome < 0) {
    printf("election: tie on all %d attempts\n", ATTEMPTS);
    return 1;
  }
  if (peers[0].outcome + peers[1].outcome != 1) {
    puts("election: both peers took the same role");
    return 1;
  }
  puts("election: one leader");
  return 0;
}

static int report(const char* check, int status, uint64_t a, uint64_t b) {
  if (status != 0) {
    printf("%s: read failed\n", check);
    return 1;
  }
  printf("%s: %s\n", check, a != b ? "distinct" : "IDENTICAL");
  return a == b;
}

static int check_sequential(void) {
  uint64_t first = 0;
  uint64_t second = 0;
  int status = draw(&first) | draw(&second);
  return report("sequential", status, first, second);
}

static int check_fork(void) {
  int pipe_fds[2];
  if (pipe(pipe_fds) != 0) {
    puts("fork: pipe failed");
    return 1;
  }
  pid_t child = fork();
  if (child < 0) {
    puts("fork: fork failed");
    return 1;
  }
  if (child == 0) {
    uint64_t value = 0;
    int ok = draw(&value) == 0 && send_value(pipe_fds[1], value) == 0;
    _exit(ok ? 0 : 1);
  }
  close(pipe_fds[1]);
  uint64_t parent_value = 0;
  uint64_t child_value = 0;
  int status = draw(&parent_value) | receive_value(pipe_fds[0], &child_value);
  close(pipe_fds[0]);
  int wait_status = 0;
  if (waitpid(child, &wait_status, 0) != child || !WIFEXITED(wait_status) ||
      WEXITSTATUS(wait_status) != 0) {
    status = -1;
  }
  return report("fork", status, parent_value, child_value);
}

int main(void) {
  setvbuf(stdout, NULL, _IONBF, 0);
  int failures = check_election() + check_sequential() + check_fork();
  return failures;
}
