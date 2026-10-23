/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Record/replay regression guest for
 * https://github.com/rrnewton/hermit/issues/3964.
 *
 * The parent redirects its own stdout into a pipe and then forks, as flex
 * does for each stage of its filter chain. The child's stdout is therefore a
 * pipe inside the container, not the stdout the container inherited. The
 * child writes 128 KiB into it, twice a pipe's default capacity, and the
 * parent reads it all back and prints a count and a checksum.
 *
 * The recorder used to take each process's descriptor 1 at its creation as a
 * captured output, so the child's writes were recorded as container output.
 * Replay then wrote them into the live pipe while it served the parent's
 * reads from the recording, so nothing drained the pipe and the child's
 * write waited forever once the pipe was full.
 *
 * With the argument `saved-stdout`, the parent first saves its stdout as
 * another descriptor, and the child writes its 128 KiB through that saved
 * descriptor to the container's real stdout, then one line into the pipe, as
 * `exec 3>&1; { head -c 131072 /dev/zero >&3; echo x; } | cat` does in bash.
 * The write is captured output, and replay used to emit it into the child's
 * own copy of descriptor 1, which is the pipe
 * (https://github.com/rrnewton/hermit/issues/4006).
 *
 * Two modes write little, for a regular-file stdout whose final offset the
 * test reads (review of https://github.com/rrnewton/hermit/pull/4037):
 *   last-writer  the saved-stdout fork, with the child writing only AAAA
 *                through the saved copy of the root's description, the last
 *                write to move its offset;
 *   reopen       the root opens /proc/self/fd/1 again, a second description
 *                of the same file, as descriptor 1 and forks; the child
 *                writes BBBBBBBB through it, and nothing moves the root's
 *                original description.
 */

#define _GNU_SOURCE

#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

#define CHUNK 4096
#define CHUNKS 32

static int write_all(int fd, const unsigned char* buffer, size_t length) {
  while (length > 0) {
    ssize_t written = write(fd, buffer, length);
    if (written <= 0) {
      return -1;
    }
    buffer += written;
    length -= (size_t)written;
  }
  return 0;
}

static int saved_stdout_mode(void) {
  int pipe_fds[2];
  if (pipe(pipe_fds) != 0) {
    perror("pipe");
    return 1;
  }
  int saved_stdout = dup(STDOUT_FILENO);
  if (saved_stdout < 0 || dup2(pipe_fds[1], STDOUT_FILENO) < 0) {
    perror("redirect stdout");
    return 1;
  }
  pid_t child = fork();
  if (child < 0) {
    perror("fork");
    return 1;
  }
  if (child == 0) {
    close(pipe_fds[0]);
    close(pipe_fds[1]);
    unsigned char chunk[CHUNK];
    memset(chunk, 'x', sizeof chunk);
    for (int i = 0; i < CHUNKS; i++) {
      if (write_all(saved_stdout, chunk, sizeof chunk) != 0) {
        _exit(2);
      }
    }
    if (write_all(saved_stdout, (const unsigned char*)"\n", 1) != 0 ||
        write_all(
            STDOUT_FILENO, (const unsigned char*)"through-pipe\n", 13) != 0) {
      _exit(3);
    }
    _exit(0);
  }
  if (dup2(saved_stdout, STDOUT_FILENO) < 0) {
    perror("restore stdout");
    return 1;
  }
  close(saved_stdout);
  close(pipe_fds[1]);
  char line[64];
  size_t total = 0;
  for (;;) {
    ssize_t got = read(pipe_fds[0], line + total, sizeof line - 1 - total);
    if (got < 0) {
      perror("read");
      return 1;
    }
    if (got == 0) {
      break;
    }
    total += (size_t)got;
  }
  line[total] = '\0';
  close(pipe_fds[0]);
  int status = 0;
  if (waitpid(child, &status, 0) != child) {
    perror("waitpid");
    return 1;
  }
  printf("saved-stdout pipe=%s child_status=%d\n", line, status);
  if (strcmp(line, "through-pipe\n") != 0 || status != 0) {
    fprintf(stderr, "saved-stdout mismatch\n");
    return 1;
  }
  return 0;
}

static int last_writer_mode(void) {
  int pipe_fds[2];
  if (pipe(pipe_fds) != 0) {
    perror("pipe");
    return 1;
  }
  int saved_stdout = dup(STDOUT_FILENO);
  if (saved_stdout < 0 || dup2(pipe_fds[1], STDOUT_FILENO) < 0) {
    perror("redirect stdout");
    return 1;
  }
  pid_t child = fork();
  if (child < 0) {
    perror("fork");
    return 1;
  }
  if (child == 0) {
    close(pipe_fds[0]);
    close(pipe_fds[1]);
    _exit(write_all(saved_stdout, (const unsigned char*)"AAAA", 4) == 0 ? 0 : 2);
  }
  if (dup2(saved_stdout, STDOUT_FILENO) < 0) {
    perror("restore stdout");
    return 1;
  }
  close(saved_stdout);
  close(pipe_fds[1]);
  char drain[16];
  while (read(pipe_fds[0], drain, sizeof drain) > 0) {
  }
  close(pipe_fds[0]);
  int status = 0;
  if (waitpid(child, &status, 0) != child) {
    perror("waitpid");
    return 1;
  }
  return status == 0 ? 0 : 1;
}

static int reopen_mode(void) {
  int saved_stdout = dup(STDOUT_FILENO);
  int reopened = open("/proc/self/fd/1", O_WRONLY);
  if (saved_stdout < 0 || reopened < 0 || dup2(reopened, STDOUT_FILENO) < 0) {
    perror("reopen stdout");
    return 1;
  }
  close(reopened);
  pid_t child = fork();
  if (child < 0) {
    perror("fork");
    return 1;
  }
  if (child == 0) {
    _exit(
        write_all(STDOUT_FILENO, (const unsigned char*)"BBBBBBBB", 8) == 0 ? 0
                                                                          : 2);
  }
  int status = 0;
  if (waitpid(child, &status, 0) != child) {
    perror("waitpid");
    return 1;
  }
  if (dup2(saved_stdout, STDOUT_FILENO) < 0) {
    perror("restore stdout");
    return 1;
  }
  close(saved_stdout);
  return status == 0 ? 0 : 1;
}

int main(int argc, char** argv) {
  if (argc == 2 && strcmp(argv[1], "saved-stdout") == 0) {
    return saved_stdout_mode();
  }
  if (argc == 2 && strcmp(argv[1], "last-writer") == 0) {
    return last_writer_mode();
  }
  if (argc == 2 && strcmp(argv[1], "reopen") == 0) {
    return reopen_mode();
  }
  int pipe_fds[2];
  if (pipe(pipe_fds) != 0) {
    perror("pipe");
    return 1;
  }
  int saved_stdout = dup(STDOUT_FILENO);
  if (saved_stdout < 0 || dup2(pipe_fds[1], STDOUT_FILENO) < 0) {
    perror("redirect stdout");
    return 1;
  }

  pid_t child = fork();
  if (child < 0) {
    perror("fork");
    return 1;
  }
  if (child == 0) {
    close(pipe_fds[0]);
    close(pipe_fds[1]);
    close(saved_stdout);
    unsigned char chunk[CHUNK];
    for (int i = 0; i < CHUNKS; i++) {
      for (int j = 0; j < CHUNK; j++) {
        chunk[j] = (unsigned char)(i * 31 + j);
      }
      if (write_all(STDOUT_FILENO, chunk, sizeof chunk) != 0) {
        _exit(2);
      }
    }
    _exit(0);
  }

  if (dup2(saved_stdout, STDOUT_FILENO) < 0) {
    perror("restore stdout");
    return 1;
  }
  close(saved_stdout);
  close(pipe_fds[1]);

  unsigned char buffer[CHUNK];
  size_t total = 0;
  uint32_t checksum = 0;
  for (;;) {
    ssize_t got = read(pipe_fds[0], buffer, sizeof buffer);
    if (got < 0) {
      perror("read");
      return 1;
    }
    if (got == 0) {
      break;
    }
    for (ssize_t k = 0; k < got; k++) {
      checksum = checksum * 33 + buffer[k];
    }
    total += (size_t)got;
  }
  close(pipe_fds[0]);

  int status = 0;
  if (waitpid(child, &status, 0) != child) {
    perror("waitpid");
    return 1;
  }
  printf(
      "forked-stdout-pipe bytes=%zu checksum=%08x child_status=%d\n",
      total,
      checksum,
      status);
  if (total != (size_t)CHUNK * CHUNKS || status != 0) {
    fprintf(stderr, "forked-stdout-pipe mismatch\n");
    return 1;
  }
  return 0;
}
