/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <sched.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static int
write_vector(int fd, const char* first, const char* second, const char* third) {
  struct iovec iov[3] = {
      {.iov_base = (void*)first, .iov_len = strlen(first)},
      {.iov_base = (void*)second, .iov_len = strlen(second)},
      {.iov_base = (void*)third, .iov_len = strlen(third)},
  };
  size_t expected = iov[0].iov_len + iov[1].iov_len + iov[2].iov_len;
  ssize_t written = writev(fd, iov, 3);
  if (written != (ssize_t)expected) {
    fprintf(
        stderr,
        "writev returned %zd, expected %zu: %s\n",
        written,
        expected,
        strerror(errno));
    return -1;
  }
  return (int)expected;
}

static int read_exact(int fd, char* buffer, size_t length) {
  size_t offset = 0;
  while (offset < length) {
    ssize_t count = read(fd, buffer + offset, length - offset);
    if (count < 0 && errno == EINTR) {
      continue;
    }
    if (count <= 0) {
      fprintf(
          stderr,
          "read returned %zd after %zu/%zu bytes: %s\n",
          count,
          offset,
          length,
          strerror(errno));
      return -1;
    }
    offset += (size_t)count;
  }
  return 0;
}

static int check_endpoint(
    int write_fd,
    int read_fd,
    const char* first,
    const char* second,
    const char* third,
    const char* expected) {
  int length = write_vector(write_fd, first, second, third);
  if (length < 0) {
    return -1;
  }

  char buffer[64] = {0};
  if ((size_t)length >= sizeof(buffer) ||
      read_exact(read_fd, buffer, (size_t)length) != 0) {
    return -1;
  }
  if (memcmp(buffer, expected, (size_t)length) != 0) {
    fprintf(
        stderr,
        "writev payload mismatch: got %.*s, expected %s\n",
        length,
        buffer,
        expected);
    return -1;
  }
  return 0;
}

enum {
  ATOMIC_FIRST_SIZE = 1024,
  ATOMIC_SECOND_SIZE = 1536,
  ATOMIC_THIRD_SIZE = 1536,
};

struct atomic_reader_context {
  int read_fd;
  int capacity;
  struct iovec* iov;
  char* poisoned_first;
  _Atomic int* done;
};

static void* atomic_pipe_reader(void* opaque) {
  struct atomic_reader_context* context = opaque;
  struct timespec delay = {.tv_sec = 0, .tv_nsec = 1000000};
  while (nanosleep(&delay, &delay) != 0 && errno == EINTR) {
  }
  context->iov[0].iov_base = context->poisoned_first;
  context->iov[0].iov_len = ATOMIC_FIRST_SIZE;

  const size_t atomic_size =
      ATOMIC_FIRST_SIZE + ATOMIC_SECOND_SIZE + ATOMIC_THIRD_SIZE;
  size_t expected_size = (size_t)context->capacity + atomic_size;
  char* received = malloc(expected_size);
  int success = received != NULL &&
      read_exact(context->read_fd, received, expected_size) == 0;
  if (success) {
    for (int index = 0; index < context->capacity; index++) {
      if (received[index] != 'F') {
        success = 0;
        break;
      }
    }
  }
  if (success) {
    for (size_t index = 0; index < atomic_size; index++) {
      char expected = index < ATOMIC_FIRST_SIZE
          ? 'A'
          : (index < ATOMIC_FIRST_SIZE + ATOMIC_SECOND_SIZE ? 'B' : 'C');
      if (received[(size_t)context->capacity + index] != expected) {
        success = 0;
        break;
      }
    }
  }
  free(received);
  atomic_store_explicit(context->done, success ? 1 : -1, memory_order_release);
  for (;;) {
    pause();
  }
  return NULL;
}

static int check_atomic_full_pipe(void) {
  const size_t atomic_size =
      ATOMIC_FIRST_SIZE + ATOMIC_SECOND_SIZE + ATOMIC_THIRD_SIZE;
  static char first[ATOMIC_FIRST_SIZE];
  static char second[ATOMIC_SECOND_SIZE];
  static char third[ATOMIC_THIRD_SIZE];
  static char poisoned_first[ATOMIC_FIRST_SIZE];
  memset(first, 'A', sizeof(first));
  memset(second, 'B', sizeof(second));
  memset(third, 'C', sizeof(third));
  memset(poisoned_first, 'X', sizeof(poisoned_first));
  struct iovec iov[3] = {
      {.iov_base = first, .iov_len = sizeof(first)},
      {.iov_base = second, .iov_len = sizeof(second)},
      {.iov_base = third, .iov_len = sizeof(third)},
  };

  int pipe_fds[2];
  if (pipe(pipe_fds) != 0) {
    perror("atomic pipe");
    return -1;
  }
  int capacity = fcntl(pipe_fds[1], F_GETPIPE_SZ);
  if (capacity <= 0) {
    perror("atomic pipe F_GETPIPE_SZ");
    return -1;
  }
  char* fill = malloc((size_t)capacity);
  if (fill == NULL) {
    perror("atomic pipe malloc");
    return -1;
  }
  memset(fill, 'F', (size_t)capacity);
  if (write(pipe_fds[1], fill, (size_t)capacity) != capacity) {
    perror("atomic pipe fill");
    return -1;
  }
  free(fill);

  _Atomic int reader_done = 0;
  struct atomic_reader_context context = {
      .read_fd = pipe_fds[0],
      .capacity = capacity,
      .iov = iov,
      .poisoned_first = poisoned_first,
      .done = &reader_done,
  };
  pthread_t reader;
  if (pthread_create(&reader, NULL, atomic_pipe_reader, &context) != 0) {
    perror("atomic pipe pthread_create");
    return -1;
  }

  ssize_t written = writev(pipe_fds[1], iov, 3);
  close(pipe_fds[1]);
  int reader_result = 0;
  while ((reader_result =
              atomic_load_explicit(&reader_done, memory_order_acquire)) == 0) {
    sched_yield();
  }
  close(pipe_fds[0]);
  if (written != (ssize_t)atomic_size || reader_result != 1 ||
      iov[0].iov_base != poisoned_first ||
      iov[0].iov_len != ATOMIC_FIRST_SIZE) {
    fprintf(
        stderr,
        "atomic full-pipe writev returned %zd/%zu, reader %d, live iov %p/%zu\n",
        written,
        atomic_size,
        reader_result,
        iov[0].iov_base,
        iov[0].iov_len);
    return -1;
  }
  return 0;
}

static int check_readonly_iovec(void) {
  static char first[] = "read";
  static char second[] = "only";
  static char third[] = "iov";
  long page_size = sysconf(_SC_PAGESIZE);
  if (page_size <= 0) {
    perror("readonly iovec page size");
    return -1;
  }
  struct iovec* iov = mmap(
      NULL,
      (size_t)page_size,
      PROT_READ | PROT_WRITE,
      MAP_PRIVATE | MAP_ANONYMOUS,
      -1,
      0);
  if (iov == MAP_FAILED) {
    perror("readonly iovec mmap");
    return -1;
  }
  iov[0] = (struct iovec){.iov_base = first, .iov_len = sizeof(first) - 1};
  iov[1] = (struct iovec){.iov_base = second, .iov_len = sizeof(second) - 1};
  iov[2] = (struct iovec){.iov_base = third, .iov_len = sizeof(third) - 1};
  if (mprotect(iov, (size_t)page_size, PROT_READ) != 0) {
    perror("readonly iovec mprotect");
    return -1;
  }

  int pipe_fds[2];
  char received[12] = {0};
  ssize_t written = -1;
  if (pipe(pipe_fds) == 0) {
    written = writev(pipe_fds[1], iov, 3);
    close(pipe_fds[1]);
    if (read_exact(pipe_fds[0], received, sizeof(received) - 1) != 0) {
      written = -1;
    }
    close(pipe_fds[0]);
  }
  if (munmap(iov, (size_t)page_size) != 0) {
    perror("readonly iovec munmap");
    return -1;
  }
  if (written != (ssize_t)(sizeof(received) - 1) ||
      memcmp(received, "readonlyiov", sizeof(received) - 1) != 0) {
    fprintf(stderr, "readonly iovec writev failed\n");
    return -1;
  }
  return 0;
}

static int check_large_iovec_snapshot(void) {
  enum { IOV_COUNT = 33, BYTES_PER_IOV = 4 };
  char chunks[IOV_COUNT][BYTES_PER_IOV];
  struct iovec iov[IOV_COUNT];
  char expected[IOV_COUNT * BYTES_PER_IOV];
  for (size_t index = 0; index < IOV_COUNT; index++) {
    char value = (char)('a' + (index % 26));
    memset(chunks[index], value, BYTES_PER_IOV);
    memset(expected + index * BYTES_PER_IOV, value, BYTES_PER_IOV);
    iov[index].iov_base = chunks[index];
    iov[index].iov_len = BYTES_PER_IOV;
  }

  long page_size = sysconf(_SC_PAGESIZE);
  if (page_size <= 0) {
    perror("large iovec page size");
    return -1;
  }
  void* probe_before = mmap(
      NULL,
      (size_t)page_size,
      PROT_READ | PROT_WRITE,
      MAP_PRIVATE | MAP_ANONYMOUS,
      -1,
      0);
  if (probe_before == MAP_FAILED ||
      munmap(probe_before, (size_t)page_size) != 0) {
    perror("large iovec first mmap probe");
    return -1;
  }

  int pipe_fds[2];
  char received[sizeof(expected)];
  if (pipe(pipe_fds) != 0) {
    perror("large iovec pipe");
    return -1;
  }
  ssize_t written = writev(pipe_fds[1], iov, IOV_COUNT);
  close(pipe_fds[1]);
  int read_status = read_exact(pipe_fds[0], received, sizeof(received));
  close(pipe_fds[0]);

  void* probe_after = mmap(
      NULL,
      (size_t)page_size,
      PROT_READ | PROT_WRITE,
      MAP_PRIVATE | MAP_ANONYMOUS,
      -1,
      0);
  if (probe_after == MAP_FAILED) {
    perror("large iovec second mmap probe");
    return -1;
  }
  int cleanup = munmap(probe_after, (size_t)page_size);
  if (written != (ssize_t)sizeof(expected) || read_status != 0 ||
      memcmp(received, expected, sizeof(expected)) != 0 ||
      probe_after != probe_before || cleanup != 0) {
    fprintf(
        stderr,
        "large-iovec writev failed: %zd/%zu, read %d, probes %p/%p, cleanup %d\n",
        written,
        sizeof(expected),
        read_status,
        probe_before,
        probe_after,
        cleanup);
    return -1;
  }
  return 0;
}

static int check_large_blocking_pipe(void) {
  enum { CHUNK_COUNT = 4, CHUNK_SIZE = 32768 };
  static char chunks[CHUNK_COUNT][CHUNK_SIZE];
  struct iovec iov[CHUNK_COUNT];
  for (size_t index = 0; index < CHUNK_COUNT; index++) {
    memset(chunks[index], 'A' + (int)index, CHUNK_SIZE);
    iov[index].iov_base = chunks[index];
    iov[index].iov_len = CHUNK_SIZE;
  }
  const size_t expected = CHUNK_COUNT * CHUNK_SIZE;

  int pipe_fds[2];
  if (pipe(pipe_fds) != 0) {
    perror("large pipe");
    return -1;
  }
  pid_t child = fork();
  if (child < 0) {
    perror("fork");
    return -1;
  }
  if (child == 0) {
    close(pipe_fds[1]);
    size_t received = 0;
    char buffer[4096];
    while (received < expected) {
      ssize_t count = read(pipe_fds[0], buffer, sizeof(buffer));
      if (count < 0 && errno == EINTR) {
        continue;
      }
      if (count <= 0) {
        _exit(2);
      }
      for (ssize_t index = 0; index < count; index++) {
        size_t position = received + (size_t)index;
        char expected_byte = (char)('A' + (position / CHUNK_SIZE));
        if (buffer[index] != expected_byte) {
          _exit(4);
        }
      }
      received += (size_t)count;
    }
    close(pipe_fds[0]);
    _exit(received == expected ? 0 : 3);
  }

  close(pipe_fds[0]);
  ssize_t written = writev(pipe_fds[1], iov, CHUNK_COUNT);
  close(pipe_fds[1]);
  int status = 0;
  if (waitpid(child, &status, 0) != child) {
    perror("waitpid");
    return -1;
  }
  if (written != (ssize_t)expected || !WIFEXITED(status) ||
      WEXITSTATUS(status) != 0) {
    fprintf(
        stderr,
        "large blocking pipe writev returned %zd/%zu, child status %#x\n",
        written,
        expected,
        status);
    return -1;
  }
  return 0;
}

static int check_failed_write_preserves_metadata(void) {
  char path[] = "/tmp/hermit-writev-XXXXXX";
  int fd = mkstemp(path);
  if (fd < 0) {
    perror("mkstemp");
    return -1;
  }

  struct stat before;
  struct stat after;
  if (fstat(fd, &before) != 0) {
    perror("fstat before");
    return -1;
  }
  errno = 0;
  long invalid = syscall(SYS_writev, fd, (void*)1, 1);
  if (invalid != -1 || errno != EFAULT) {
    fprintf(
        stderr,
        "invalid writev returned %ld with errno %d, expected EFAULT\n",
        invalid,
        errno);
    return -1;
  }
  if (fstat(fd, &after) != 0) {
    perror("fstat after");
    return -1;
  }
  close(fd);
  unlink(path);

  if (before.st_mtim.tv_sec != after.st_mtim.tv_sec ||
      before.st_mtim.tv_nsec != after.st_mtim.tv_nsec ||
      before.st_ctim.tv_sec != after.st_ctim.tv_sec ||
      before.st_ctim.tv_nsec != after.st_ctim.tv_nsec) {
    fprintf(stderr, "failed writev changed virtual file timestamps\n");
    return -1;
  }
  return 0;
}

int main(int argc, char** argv) {
  if (write_vector(STDOUT_FILENO, "writev-", "stdout", "\n") < 0) {
    return 1;
  }

  if (argc > 1 && strcmp(argv[1], "record") == 0) {
    puts("writev-determinism-ok");
    return 0;
  }
  if (argc > 1 && strcmp(argv[1], "record-pipe") == 0) {
    if (check_atomic_full_pipe() != 0 || check_large_iovec_snapshot() != 0 ||
        check_large_blocking_pipe() != 0) {
      return 1;
    }
    puts("writev-determinism-ok");
    return 0;
  }

  int pipe_fds[2];
  if (pipe(pipe_fds) != 0) {
    perror("pipe");
    return 1;
  }
  if (check_endpoint(
          pipe_fds[1], pipe_fds[0], "pipe", "-", "payload", "pipe-payload") !=
      0) {
    return 1;
  }
  close(pipe_fds[0]);
  close(pipe_fds[1]);

  int sockets[2];
  if (socketpair(AF_UNIX, SOCK_STREAM, 0, sockets) != 0) {
    perror("socketpair");
    return 1;
  }
  if (check_endpoint(
          sockets[0], sockets[1], "socket", "-", "payload", "socket-payload") !=
      0) {
    return 1;
  }
  close(sockets[0]);
  close(sockets[1]);

  if (check_atomic_full_pipe() != 0 || check_readonly_iovec() != 0 ||
      check_large_iovec_snapshot() != 0 || check_large_blocking_pipe() != 0 ||
      check_failed_write_preserves_metadata() != 0) {
    return 1;
  }

  puts("writev-determinism-ok");
  return 0;
}
