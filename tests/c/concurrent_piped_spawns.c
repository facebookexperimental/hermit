/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Two threads each spawn a child whose standard output is a pipe, read the
 * pipe to EOF and wait for the child, while the main thread waits in
 * pthread_join. In each round both threads make their pipes before either
 * spawns, so each thread's descriptor table holds the other's write end when
 * it blocks in read. A reader sees EOF only once every descriptor of its
 * pipe's write end is closed: the child's at exit, and the parent's, which its
 * thread closes after the spawn. The other thread's view of that write end is
 * the same descriptor (CLONE_FILES), so it must go with that close, whatever
 * the other thread is doing.
 *
 * The program is its own oracle: it exits 1 unless every spawn read exactly
 * its child's word to EOF and every child exited 0, and it prints how many
 * spawns did.
 */

#include <fcntl.h>
#include <pthread.h>
#include <spawn.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

#define THREADS 2
#define ROUNDS 5

extern char** environ;

static pthread_barrier_t barrier;
static char results[THREADS][ROUNDS][160];
static int succeeded[THREADS][ROUNDS];

static void fail(const char* what) {
  perror(what);
  exit(1);
}

static void spawn_and_read(int thread, int round) {
  int fds[2];
  if (pipe2(fds, O_CLOEXEC) != 0)
    fail("pipe2");
  /* Both pipes exist before either thread spawns. */
  pthread_barrier_wait(&barrier);
  posix_spawn_file_actions_t actions;
  if (posix_spawn_file_actions_init(&actions) != 0 ||
      posix_spawn_file_actions_adddup2(&actions, fds[1], STDOUT_FILENO) != 0)
    fail("posix_spawn_file_actions");
  char word[32];
  snprintf(word, sizeof word, "child-%d-%d", thread, round);
  char* argv[] = {"/bin/echo", word, NULL};
  pid_t child;
  int error = posix_spawn(&child, "/bin/echo", &actions, NULL, argv, environ);
  if (error != 0) {
    fprintf(stderr, "posix_spawn: %s\n", strerror(error));
    exit(1);
  }
  posix_spawn_file_actions_destroy(&actions);
  if (close(fds[1]) != 0)
    fail("close write end");
  char line[64];
  size_t got = 0;
  for (;;) {
    ssize_t n = read(fds[0], line + got, sizeof line - 1 - got);
    if (n < 0)
      fail("read");
    if (n == 0)
      break;
    got += (size_t)n;
  }
  line[got] = '\0';
  if (got > 0 && line[got - 1] == '\n')
    line[got - 1] = '\0';
  if (close(fds[0]) != 0)
    fail("close read end");
  int status;
  if (waitpid(child, &status, 0) != child)
    fail("waitpid");
  int exited = WIFEXITED(status) ? WEXITSTATUS(status) : -1;
  succeeded[thread][round] = exited == 0 && strcmp(line, word) == 0;
  snprintf(
      results[thread][round],
      sizeof results[thread][round],
      "thread %d round %d read \"%s\" to EOF, child exit %d%s",
      thread,
      round,
      line,
      exited,
      succeeded[thread][round] ? "" : " (WRONG)");
}

static void* worker(void* argument) {
  int thread = (int)(long)argument;
  for (int round = 0; round < ROUNDS; round++) {
    pthread_barrier_wait(&barrier);
    spawn_and_read(thread, round);
  }
  return NULL;
}

int main(void) {
  if (pthread_barrier_init(&barrier, NULL, THREADS) != 0)
    fail("pthread_barrier_init");
  pthread_t threads[THREADS];
  for (long thread = 0; thread < THREADS; thread++)
    if (pthread_create(&threads[thread], NULL, worker, (void*)thread) != 0)
      fail("pthread_create");
  for (int thread = 0; thread < THREADS; thread++)
    if (pthread_join(threads[thread], NULL) != 0)
      fail("pthread_join");
  int spawns = 0;
  for (int thread = 0; thread < THREADS; thread++)
    for (int round = 0; round < ROUNDS; round++) {
      printf("%s\n", results[thread][round]);
      spawns += succeeded[thread][round];
    }
  if (spawns != THREADS * ROUNDS) {
    printf(
        "concurrent-piped-spawns: FAILED, %d of %d spawns read their child's word to EOF from a child that exited 0\n",
        spawns,
        THREADS * ROUNDS);
    return 1;
  }
  printf(
      "concurrent-piped-spawns: %d spawns, every pipe reached EOF with its child's word\n",
      spawns);
  return 0;
}
