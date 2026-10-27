/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * A server thread answers one request a client sends over a pair of pipes;
 * a shutdown thread, asked to stop right after the request goes out,
 * removes the client from the server's table. When the
 * removal comes first, the server logs that the client is gone and answers
 * 8 (no handler) instead of 0, and the client fails on it. The race is the
 * removal against the server's lookup, both in memory. The answer and the
 * client's read of it are ordered by the answer's bytes in every run, so
 * they are its effect, not the race. A poller thread naps in short steps
 * throughout, as a socket server's loop does: its naps land next to every
 * point that moves, and none of them decides.
 */

#include <pthread.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <time.h>
#include <unistd.h>

static atomic_int registered = 1;
static atomic_int stopping = 0;
static int request[2];
static int answer[2];
static int shutdown_request[2];

static void* server(void* arg) {
  (void)arg;
  int client = 0;
  if (read(request[0], &client, sizeof(client)) != sizeof(client)) {
    return NULL;
  }
  int known = atomic_load(&registered); /* the lookup */
  if (known) {
    fprintf(stderr, "server: handled the request of client %d\n", client);
  } else {
    fprintf(stderr, "server: client %d not found, no handler\n", client);
  }
  int code = known ? 0 : 8;
  if (write(answer[1], &code, sizeof(code)) != sizeof(code)) {
    perror("write");
  }
  return NULL;
}

static void* poller(void* arg) {
  (void)arg;
  struct timespec nap = {0, 200000};
  while (!atomic_load(&stopping)) {
    nanosleep(&nap, NULL);
  }
  return NULL;
}

static void* shutdown_server(void* arg) {
  (void)arg;
  char go = 0;
  if (read(shutdown_request[0], &go, 1) != 1) {
    return NULL;
  }
  fprintf(stderr, "shutdown: start\n");
  atomic_store(&registered, 0); /* the removal */
  fprintf(stderr, "shutdown: removed the clients\n");
  return NULL;
}

int main(void) {
  if (pipe(request) != 0 || pipe(answer) != 0 || pipe(shutdown_request) != 0) {
    perror("pipe");
    return 2;
  }
  pthread_t threads[3];
  if (pthread_create(&threads[2], NULL, poller, NULL) != 0 ||
      pthread_create(&threads[1], NULL, shutdown_server, NULL) != 0 ||
      pthread_create(&threads[0], NULL, server, NULL) != 0) {
    perror("pthread_create");
    return 2;
  }
  int client = 3;
  int code = -1;
  /* The request and the shutdown go out together; who runs first decides. */
  if (write(request[1], &client, sizeof(client)) != sizeof(client) ||
      write(shutdown_request[1], "x", 1) != 1 ||
      read(answer[0], &code, sizeof(code)) != sizeof(code)) {
    perror("rpc");
    return 2;
  }
  pthread_join(threads[0], NULL);
  pthread_join(threads[1], NULL);
  atomic_store(&stopping, 1);
  pthread_join(threads[2], NULL);
  if (code == 8) {
    printf("call failed: result 8, no handler\n");
    return 1;
  }
  printf("call answered: result %d\n", code);
  return 0;
}
