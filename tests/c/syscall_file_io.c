/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <unistd.h>

static void require_zero(long result, const char *name) {
  if (result != 0) {
    perror(name);
    exit(1);
  }
}

/* A path that a rename or unlink should have removed must be GONE, and gone for
 * the right reason: `stat` failing for any other errno is not evidence of
 * removal. */
static void require_absent(const char *path, const char *name) {
  struct stat gone;
  errno = 0;
  if (stat(path, &gone) == 0) {
    fprintf(stderr, "%s: %s still exists\n", name, path);
    exit(1);
  }
  if (errno != ENOENT) {
    fprintf(stderr, "%s: %s stat errno %d, want ENOENT\n", name, path, errno);
    exit(1);
  }
}

/* Read the file back and compare the BYTES.
 *
 * Without this the fixture only ever checked that `write` RETURNED 7 and that
 * `stat` reported the right size. A backend that reports a 7-byte write while
 * dropping or corrupting the buffer passed, because nothing ever looked at the
 * content -- the fixture asserted write parity it did not check.
 *
 * Two properties, and the second is not decoration: the payload must survive,
 * and the region `truncate` grew must read as zeros, which is the behaviour
 * POSIX requires of a file extended by truncation.
 */
static void require_content(const char *path, const char *payload,
                            size_t payload_len, size_t expected_size) {
  int fd = open(path, O_RDONLY);
  if (fd < 0) {
    perror("open for readback");
    exit(1);
  }
  char *buffer = calloc(1, expected_size + 1);
  if (buffer == NULL) {
    fprintf(stderr, "readback: out of memory\n");
    exit(1);
  }
  size_t got = 0;
  while (got < expected_size + 1) {
    ssize_t chunk = read(fd, buffer + got, expected_size + 1 - got);
    if (chunk < 0) {
      perror("read for readback");
      exit(1);
    }
    if (chunk == 0) {
      break;
    }
    got += (size_t)chunk;
  }
  require_zero(close(fd), "close after readback");
  if (got != expected_size) {
    fprintf(stderr, "readback %s: read %zu bytes, want %zu\n", path, got,
            expected_size);
    exit(1);
  }
  if (memcmp(buffer, payload, payload_len) != 0) {
    fprintf(stderr, "readback %s: payload mismatch, got \"%.*s\" want \"%.*s\"\n",
            path, (int)payload_len, buffer, (int)payload_len, payload);
    exit(1);
  }
  for (size_t at = payload_len; at < expected_size; ++at) {
    if (buffer[at] != 0) {
      fprintf(stderr,
              "readback %s: byte %zu is 0x%02x, want 0 (truncate must extend "
              "with zeros)\n",
              path, at, (unsigned char)buffer[at]);
      exit(1);
    }
  }
  free(buffer);
}

int main(void) {
  char original[128];
  char renamed[128];
  char renamed_at[128];
  char link_path[128];
  long pid = (long)getpid();
  snprintf(original, sizeof(original), "/tmp/hermit-file-io-%ld-a", pid);
  snprintf(renamed, sizeof(renamed), "/tmp/hermit-file-io-%ld-b", pid);
  snprintf(renamed_at, sizeof(renamed_at), "/tmp/hermit-file-io-%ld-c", pid);
  snprintf(link_path, sizeof(link_path), "/tmp/hermit-file-io-%ld-link", pid);

  int fd = open(original, O_CREAT | O_TRUNC | O_RDWR, 0600);
  if (fd < 0 || write(fd, "file-io", 7) != 7) {
    perror("open/write");
    return 1;
  }

  struct stat status;
  long fallocate_result = syscall(SYS_fallocate, fd, 0, 0, 8192);
  if (fallocate_result == 0) {
    require_zero(fstat(fd, &status), "fstat after fallocate");
    if (status.st_size != 8192) {
      fprintf(stderr, "fallocate size mismatch: %ld\n", (long)status.st_size);
      return 1;
    }
  } else if (errno != EOPNOTSUPP) {
    perror("fallocate");
    return 1;
  }
  require_zero(close(fd), "close");

  require_zero(syscall(SYS_truncate, original, 4096), "truncate");
  require_zero(stat(original, &status), "stat");
  if (status.st_size != 4096) {
    fprintf(stderr, "truncate size mismatch: %ld\n", (long)status.st_size);
    return 1;
  }

  require_zero(syscall(SYS_rename, original, renamed), "rename");
  require_absent(original, "after rename");
  require_zero(syscall(SYS_renameat, AT_FDCWD, renamed, AT_FDCWD, renamed_at),
               "renameat");
  require_absent(renamed, "after renameat");

  /* The point of doing this HERE: the bytes must have survived both renames,
   * so the comparison is against the final path, not the one they were
   * written to. */
  require_content(renamed_at, "file-io", 7, 4096);

  require_zero(symlinkat(renamed_at, AT_FDCWD, link_path), "symlinkat");

  char target[128] = {0};
  long length = syscall(SYS_readlinkat, AT_FDCWD, link_path, target,
                        sizeof(target) - 1);
  if (length < 0) {
    perror("readlinkat");
    return 1;
  }
  target[length] = '\0';
  if (strcmp(target, renamed_at) != 0) {
    fprintf(stderr, "readlinkat target mismatch: %s\n", target);
    return 1;
  }

  require_zero(unlink(link_path), "unlink link");
  require_zero(unlink(renamed_at), "unlink file");
  require_absent(link_path, "after unlink link");
  require_absent(renamed_at, "after unlink file");
  puts("syscall-file-io-ok count=5");
  return 0;
}
