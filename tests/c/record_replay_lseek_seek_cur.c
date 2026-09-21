/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Regression guest for the record/replay regular-file lseek(SEEK_CUR) bug.
 *
 * Detcore's handle_lseek used to live-inject a seek on a non-procfs (regular)
 * descriptor instead of routing it through the record/replay strategy. During
 * record the descriptor is a real open file, so the injected seek returned the
 * true offset. During replay the descriptor is a virtual placeholder whose
 * kernel position never advances -- reads are served from the recorded log, not
 * the file -- so a live lseek(fd, -N, SEEK_CUR) returned 0 instead of the
 * recorded offset. That is exactly the pattern glibc's __tzfile_read uses to
 * rewind /etc/localtime (read to EOF, then a negative SEEK_CUR, then read
 * again); the wrong offset changed the guest's control flow, injected an extra
 * read, and desynchronized the replay event stream.
 *
 * The fixture file is created by the harness rather than by this guest, so on
 * replay it is NOT in the replay root and is served as a virtual placeholder --
 * the descriptor shape that triggered the bug. The post-rewind offset, the
 * re-read byte checksum, and the SEEK_END offset are all printed so that any
 * divergence between the recorded and replayed run surfaces as differing
 * stdout (or, in the original failure mode, an aborted replay).
 */

#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/types.h>
#include <unistd.h>

static int fail(const char* operation) {
  fprintf(stderr, "%s failed: %s\n", operation, strerror(errno));
  return 1;
}

int main(int argc, char** argv) {
  if (argc != 2) {
    fprintf(stderr, "usage: %s <fixture-file>\n", argv[0]);
    return 2;
  }

  const int fd = open(argv[1], O_RDONLY);
  if (fd < 0) {
    return fail("open(fixture)");
  }

  /* Read the whole file, advancing the offset to EOF. */
  char buffer[512];
  ssize_t total = 0;
  for (;;) {
    const ssize_t got = read(fd, buffer, sizeof(buffer));
    if (got < 0) {
      return fail("read(to EOF)");
    }
    if (got == 0) {
      break;
    }
    total += got;
  }

  /* Rewind partway with a negative SEEK_CUR, mirroring __tzfile_read. */
  const off_t rewound = lseek(fd, -(total / 2), SEEK_CUR);
  if (rewound < 0) {
    return fail("lseek(SEEK_CUR rewind)");
  }

  /*
   * Re-read from the rewound position and checksum the bytes. Both the offset
   * and these bytes must be identical between the recorded and replayed runs.
   */
  unsigned long checksum = 0;
  ssize_t reread = 0;
  for (;;) {
    const ssize_t got = read(fd, buffer, sizeof(buffer));
    if (got < 0) {
      return fail("read(after rewind)");
    }
    if (got == 0) {
      break;
    }
    for (ssize_t i = 0; i < got; i++) {
      checksum = checksum * 131 + (unsigned char)buffer[i];
    }
    reread += got;
  }

  /* SEEK_END is a second position probe independent of the SEEK_CUR rewind. */
  const off_t end = lseek(fd, 0, SEEK_END);
  if (end < 0) {
    return fail("lseek(SEEK_END)");
  }

  if (close(fd) != 0) {
    return fail("close");
  }

  printf(
      "size=%zd rewound=%lld reread=%zd checksum=%lu end=%lld\n",
      total,
      (long long)rewound,
      reread,
      checksum,
      (long long)end);
  return 0;
}
