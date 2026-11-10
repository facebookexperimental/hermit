/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * The main thread connects a loopback TCP pair to itself and accepts the
 * connection, then starts a worker that sleeps 0.5 ms and writes a byte to
 * the client end, reads the byte and joins. Hermit runs the connect as a
 * call that waits on the network, and whether it completes at its first
 * attempt is up to the kernel: how many polling retries it makes does not
 * follow a replayed schedule, so a replay makes the retries the kernel's
 * state calls for, whatever the recording held. Prints "done" once main has
 * read the byte.
 */

#include <arpa/inet.h>
#include <netinet/in.h>
#include <pthread.h>
#include <stdio.h>
#include <sys/socket.h>
#include <time.h>
#include <unistd.h>

static int client;

static void* writer(void* arg) {
  (void)arg;
  struct timespec pause = {.tv_sec = 0, .tv_nsec = 500000};
  nanosleep(&pause, NULL);
  char byte = 'x';
  if (write(client, &byte, 1) != 1) {
    perror("write");
  }
  return NULL;
}

int main(void) {
  struct sockaddr_in addr = {
      .sin_family = AF_INET, .sin_addr.s_addr = htonl(INADDR_LOOPBACK)};
  socklen_t len = sizeof addr;
  int listener = socket(AF_INET, SOCK_STREAM, 0);
  if (listener < 0 || bind(listener, (struct sockaddr*)&addr, sizeof addr) != 0 ||
      listen(listener, 1) != 0 ||
      getsockname(listener, (struct sockaddr*)&addr, &len) != 0) {
    perror("listen");
    return 1;
  }
  client = socket(AF_INET, SOCK_STREAM, 0);
  if (client < 0 || connect(client, (struct sockaddr*)&addr, sizeof addr) != 0) {
    perror("connect");
    return 1;
  }
  int server = accept(listener, NULL, NULL);
  if (server < 0) {
    perror("accept");
    return 1;
  }
  pthread_t thread;
  if (pthread_create(&thread, NULL, writer, NULL) != 0) {
    perror("pthread_create");
    return 1;
  }
  char byte;
  if (read(server, &byte, 1) != 1) {
    perror("read");
    return 1;
  }
  if (pthread_join(thread, NULL) != 0) {
    perror("pthread_join");
    return 1;
  }
  printf("done\n");
  return 0;
}
