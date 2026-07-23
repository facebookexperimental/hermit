/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <sched.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/eventfd.h>
#include <sys/socket.h>
#include <unistd.h>

#define PRODUCERS 3
#define RECORDS 12

static void fail(const char *message) {
  perror(message);
  exit(1);
}

static void write_exact(int fd, const void *buffer, size_t length) {
  const uint8_t *cursor = buffer;
  while (length > 0) {
    ssize_t written = write(fd, cursor, length);
    if (written < 0 && errno == EINTR) {
      continue;
    }
    if (written <= 0) {
      fail("write");
    }
    cursor += written;
    length -= (size_t)written;
  }
}

static void read_exact(int fd, void *buffer, size_t length) {
  uint8_t *cursor = buffer;
  while (length > 0) {
    ssize_t count = read(fd, cursor, length);
    if (count < 0 && errno == EINTR) {
      continue;
    }
    if (count <= 0) {
      fail("read");
    }
    cursor += count;
    length -= (size_t)count;
  }
}

static void verify_blocking_flag_roundtrip(int fd) {
  int flags = fcntl(fd, F_GETFL);
  if (flags < 0) {
    fail("fcntl(F_GETFL)");
  }
  if (flags & O_NONBLOCK) {
    fprintf(stderr, "fd %d unexpectedly exposed O_NONBLOCK\n", fd);
    exit(1);
  }
  if (fcntl(fd, F_SETFL, flags | O_NONBLOCK) != 0) {
    fail("fcntl(F_SETFL O_NONBLOCK)");
  }
  if (!(fcntl(fd, F_GETFL) & O_NONBLOCK)) {
    fprintf(stderr, "fd %d did not expose requested O_NONBLOCK\n", fd);
    exit(1);
  }
  if (fcntl(fd, F_SETFL, flags) != 0) {
    fail("fcntl(F_SETFL blocking)");
  }
  if (fcntl(fd, F_GETFL) & O_NONBLOCK) {
    fprintf(stderr, "fd %d did not restore blocking mode\n", fd);
    exit(1);
  }
}

struct producer_args {
  int fd;
  int producer;
  pthread_barrier_t *barrier;
};

static void *write_pipe_records(void *opaque) {
  struct producer_args *args = opaque;
  pthread_barrier_wait(args->barrier);
  for (int sequence = 0; sequence < RECORDS; sequence++) {
    uint16_t token = (uint16_t)(args->producer * 100 + sequence);
    write_exact(args->fd, &token, sizeof(token));
    sched_yield();
  }
  return NULL;
}

static void validate_tokens(const uint16_t *tokens) {
  int seen[PRODUCERS][RECORDS] = {{0}};
  for (int i = 0; i < PRODUCERS * RECORDS; i++) {
    int producer = tokens[i] / 100;
    int sequence = tokens[i] % 100;
    if (producer < 0 || producer >= PRODUCERS || sequence < 0 ||
        sequence >= RECORDS || seen[producer][sequence]) {
      fprintf(stderr, "invalid token %u at index %d\n", tokens[i], i);
      exit(1);
    }
    seen[producer][sequence] = 1;
  }
}

static void pipe_order(void) {
  int fds[2];
  if (pipe(fds) != 0) {
    fail("pipe");
  }
  verify_blocking_flag_roundtrip(fds[0]);

  pthread_barrier_t barrier;
  pthread_barrier_init(&barrier, NULL, PRODUCERS + 1);
  pthread_t threads[PRODUCERS];
  struct producer_args args[PRODUCERS];
  for (int i = 0; i < PRODUCERS; i++) {
    args[i] = (struct producer_args){fds[1], i, &barrier};
    if (pthread_create(&threads[i], NULL, write_pipe_records, &args[i]) != 0) {
      fail("pthread_create");
    }
  }

  pthread_barrier_wait(&barrier);
  uint16_t tokens[PRODUCERS * RECORDS];
  read_exact(fds[0], tokens, sizeof(tokens));
  for (int i = 0; i < PRODUCERS; i++) {
    pthread_join(threads[i], NULL);
  }
  validate_tokens(tokens);

  printf("pipe-order:");
  for (int i = 0; i < PRODUCERS * RECORDS; i++) {
    printf("%s%u", i == 0 ? "" : ",", tokens[i]);
  }
  printf("\n");
  close(fds[0]);
  close(fds[1]);
  pthread_barrier_destroy(&barrier);
}

struct byte_writer_args {
  int fd;
  uint8_t byte;
  pthread_barrier_t *barrier;
};

static void *write_one_byte(void *opaque) {
  struct byte_writer_args *args = opaque;
  pthread_barrier_wait(args->barrier);
  write_exact(args->fd, &args->byte, sizeof(args->byte));
  return NULL;
}

static void pipe_capacity(void) {
  int fds[2];
  if (pipe(fds) != 0) {
    fail("pipe");
  }
  int capacity = fcntl(fds[1], F_GETPIPE_SZ);
  if (capacity <= 0) {
    fail("fcntl(F_GETPIPE_SZ)");
  }
  uint8_t *fill = malloc((size_t)capacity);
  if (fill == NULL) {
    fail("malloc");
  }
  memset(fill, 0x5a, (size_t)capacity);
  write_exact(fds[1], fill, (size_t)capacity);

  pthread_barrier_t barrier;
  pthread_barrier_init(&barrier, NULL, 2);
  struct byte_writer_args args = {fds[1], 0xa5, &barrier};
  pthread_t writer;
  if (pthread_create(&writer, NULL, write_one_byte, &args) != 0) {
    fail("pthread_create");
  }
  pthread_barrier_wait(&barrier);
  sched_yield();

  read_exact(fds[0], fill, (size_t)capacity);
  pthread_join(writer, NULL);
  uint8_t marker = 0;
  read_exact(fds[0], &marker, sizeof(marker));
  if (marker != 0xa5) {
    fprintf(stderr, "wrong capacity marker: %u\n", marker);
    exit(1);
  }
  printf("pipe-capacity:%d:%02x\n", capacity, marker);

  free(fill);
  close(fds[0]);
  close(fds[1]);
  pthread_barrier_destroy(&barrier);
}

static void socketpair_order(void) {
  int fds[2];
  if (socketpair(AF_UNIX, SOCK_STREAM, 0, fds) != 0) {
    fail("socketpair");
  }
  verify_blocking_flag_roundtrip(fds[0]);

  pthread_barrier_t barrier;
  pthread_barrier_init(&barrier, NULL, PRODUCERS + 1);
  pthread_t threads[PRODUCERS];
  struct producer_args args[PRODUCERS];
  for (int i = 0; i < PRODUCERS; i++) {
    args[i] = (struct producer_args){fds[0], i, &barrier};
    if (pthread_create(&threads[i], NULL, write_pipe_records, &args[i]) != 0) {
      fail("pthread_create");
    }
  }

  pthread_barrier_wait(&barrier);
  uint16_t tokens[PRODUCERS * RECORDS];
  read_exact(fds[1], tokens, sizeof(tokens));
  for (int i = 0; i < PRODUCERS; i++) {
    pthread_join(threads[i], NULL);
  }
  validate_tokens(tokens);

  printf("socketpair:");
  for (int i = 0; i < PRODUCERS * RECORDS; i++) {
    printf("%s%u", i == 0 ? "" : ",", tokens[i]);
  }
  printf("\n");
  close(fds[0]);
  close(fds[1]);
  pthread_barrier_destroy(&barrier);
}

struct event_writer_args {
  int fd;
  uint64_t value;
  pthread_barrier_t *barrier;
};

static void *write_event_values(void *opaque) {
  struct event_writer_args *args = opaque;
  pthread_barrier_wait(args->barrier);
  for (int i = 0; i < RECORDS; i++) {
    write_exact(args->fd, &args->value, sizeof(args->value));
    sched_yield();
  }
  return NULL;
}

static void eventfd_signaling(void) {
  int fd = eventfd(0, 0);
  if (fd < 0) {
    fail("eventfd");
  }
  verify_blocking_flag_roundtrip(fd);
  pthread_barrier_t barrier;
  pthread_barrier_init(&barrier, NULL, 3);
  pthread_t writers[2];
  struct event_writer_args args[2] = {
      {fd, 1, &barrier},
      {fd, 100, &barrier},
  };
  for (int i = 0; i < 2; i++) {
    if (pthread_create(&writers[i], NULL, write_event_values, &args[i]) != 0) {
      fail("pthread_create");
    }
  }

  pthread_barrier_wait(&barrier);
  uint64_t expected = (uint64_t)RECORDS * 101;
  uint64_t total = 0;
  printf("eventfd:");
  for (int reads = 0; total < expected; reads++) {
    uint64_t value = 0;
    read_exact(fd, &value, sizeof(value));
    total += value;
    printf("%s%llu", reads == 0 ? "" : ",", (unsigned long long)value);
  }
  printf("\n");
  if (total != expected) {
    fprintf(stderr, "wrong eventfd total: %llu\n", (unsigned long long)total);
    exit(1);
  }
  for (int i = 0; i < 2; i++) {
    pthread_join(writers[i], NULL);
  }
  close(fd);
  pthread_barrier_destroy(&barrier);
}

struct epoll_writer_args {
  int fd;
  int eventfd;
  pthread_barrier_t *barrier;
};

static void *signal_epoll_source(void *opaque) {
  struct epoll_writer_args *args = opaque;
  pthread_barrier_wait(args->barrier);
  if (args->eventfd) {
    uint64_t value = 1;
    write_exact(args->fd, &value, sizeof(value));
  } else {
    uint8_t value = 1;
    write_exact(args->fd, &value, sizeof(value));
  }
  return NULL;
}

static void epoll_sources(void) {
  int pipefds[2];
  if (pipe(pipefds) != 0) {
    fail("pipe");
  }
  int event = eventfd(0, 0);
  if (event < 0) {
    fail("eventfd");
  }
  int epoll = epoll_create1(EPOLL_CLOEXEC);
  if (epoll < 0) {
    fail("epoll_create1");
  }

  struct epoll_event registration = {.events = EPOLLIN};
  registration.data.u64 = 1;
  if (epoll_ctl(epoll, EPOLL_CTL_ADD, pipefds[0], &registration) != 0) {
    fail("epoll_ctl(pipe)");
  }
  registration.data.u64 = 2;
  if (epoll_ctl(epoll, EPOLL_CTL_ADD, event, &registration) != 0) {
    fail("epoll_ctl(eventfd)");
  }

  pthread_barrier_t barrier;
  pthread_barrier_init(&barrier, NULL, 3);
  struct epoll_writer_args args[2] = {
      {pipefds[1], 0, &barrier},
      {event, 1, &barrier},
  };
  pthread_t writers[2];
  for (int i = 0; i < 2; i++) {
    if (pthread_create(&writers[i], NULL, signal_epoll_source, &args[i]) != 0) {
      fail("pthread_create");
    }
  }

  pthread_barrier_wait(&barrier);
  int seen = 0;
  printf("epoll:");
  while (seen != 3) {
    struct epoll_event events[2];
    int count = epoll_wait(epoll, events, 2, -1);
    if (count < 0 && errno == EINTR) {
      continue;
    }
    if (count <= 0) {
      fail("epoll_wait");
    }
    for (int i = 0; i < count; i++) {
      uint64_t id = events[i].data.u64;
      printf("%s%llu", seen == 0 ? "" : ",", (unsigned long long)id);
      if (id == 1 && !(seen & 1)) {
        uint8_t value;
        read_exact(pipefds[0], &value, sizeof(value));
        seen |= 1;
      } else if (id == 2 && !(seen & 2)) {
        uint64_t value;
        read_exact(event, &value, sizeof(value));
        seen |= 2;
      } else {
        fprintf(stderr, "duplicate or invalid epoll source: %llu\n",
                (unsigned long long)id);
        exit(1);
      }
    }
  }
  printf("\n");

  for (int i = 0; i < 2; i++) {
    pthread_join(writers[i], NULL);
  }
  close(epoll);
  close(event);
  close(pipefds[0]);
  close(pipefds[1]);
  pthread_barrier_destroy(&barrier);
}

int main(int argc, char **argv) {
  if (argc != 2) {
    fprintf(stderr, "usage: %s PATTERN\n", argv[0]);
    return 2;
  }
  if (strcmp(argv[1], "pipe-order") == 0) {
    pipe_order();
  } else if (strcmp(argv[1], "pipe-capacity") == 0) {
    pipe_capacity();
  } else if (strcmp(argv[1], "socketpair") == 0) {
    socketpair_order();
  } else if (strcmp(argv[1], "eventfd") == 0) {
    eventfd_signaling();
  } else if (strcmp(argv[1], "epoll") == 0) {
    epoll_sources();
  } else {
    fprintf(stderr, "unknown pattern: %s\n", argv[1]);
    return 2;
  }
  return 0;
}
