/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * sendfile(2) in-kernel copy parity probe.
 *
 * A single process fills one temporary regular file with the bytes 0..255 and
 * copies it to a second temporary file entirely through sendfile(2), checking
 * the invariants Detcore must preserve identically on every backend:
 *
 *   - With a NULL offset argument, sendfile transfers from the source's own
 *     file offset and ADVANCES that offset by the number of bytes moved. The
 *     copy is performed in two calls (100 then 156 bytes) and the source offset
 *     is verified to advance to 100 and then 256.
 *   - With an explicit off_t* offset argument, sendfile transfers starting at
 *     that offset, updates the pointed-to value, and does NOT move the source's
 *     own file offset. A trailing call copies 50 bytes from position 0 with an
 *     explicit offset and confirms both the updated pointer (50) and that the
 *     source's own offset is unchanged (still 256).
 *   - The bytes that arrive in the destination are exactly the source bytes:
 *     the first 256 destination bytes checksum to 0+1+...+255 = 32640.
 *
 * Both files are mkstemp'd and immediately unlinked; the open descriptors keep
 * them alive and no path is ever observed. Only invariants are printed:
 *
 *   sendfile copied=256 checksum=32640 pos=50 own_offset_kept=1
 *
 * It is deliberately free of gated concerns: single process, no fork/thread,
 * and no pid, timestamp, cpu-time, or address is observed.
 */

#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/sendfile.h>
#include <sys/stat.h>
#include <unistd.h>

static void fail(const char *message) {
  fprintf(stderr, "%s: %s\n", message, strerror(errno));
  exit(1);
}

/* Create a temporary regular file, unlink its path, and return the open fd. */
static int temp_file(void) {
  char template[] = "/tmp/sendfile_XXXXXX";
  int fd = mkstemp(template);
  if (fd < 0)
    fail("mkstemp");
  if (unlink(template) != 0)
    fail("unlink");
  return fd;
}

/* Write exactly n bytes, retrying short writes. */
static void write_all(int fd, const char *buf, size_t n) {
  size_t done = 0;
  while (done < n) {
    ssize_t w = write(fd, buf + done, n - done);
    if (w < 0) {
      if (errno == EINTR)
        continue;
      fail("write");
    }
    done += (size_t)w;
  }
}

/* Copy exactly n bytes with sendfile and a NULL offset (advances src offset),
 * retrying short transfers. */
static void sendfile_all(int out_fd, int in_fd, size_t n) {
  size_t done = 0;
  while (done < n) {
    ssize_t s = sendfile(out_fd, in_fd, NULL, n - done);
    if (s < 0) {
      if (errno == EINTR)
        continue;
      fail("sendfile");
    }
    if (s == 0)
      fail("sendfile short (unexpected EOF)");
    done += (size_t)s;
  }
}

int main(void) {
  int src = temp_file();
  int dst = temp_file();

  char src_bytes[256];
  for (int i = 0; i < 256; i++)
    src_bytes[i] = (char)i;
  write_all(src, src_bytes, sizeof(src_bytes));
  if (lseek(src, 0, SEEK_SET) != 0)
    fail("lseek src rewind");

  /* NULL-offset copy in two chunks; the source offset must advance. */
  sendfile_all(dst, src, 100);
  if (lseek(src, 0, SEEK_CUR) != 100)
    fail("src offset after first sendfile");
  sendfile_all(dst, src, 156);
  off_t own_offset = lseek(src, 0, SEEK_CUR);
  if (own_offset != 256)
    fail("src offset after second sendfile");

  size_t copied = 256;

  /* Read the destination back and checksum the copied bytes. */
  if (lseek(dst, 0, SEEK_SET) != 0)
    fail("lseek dst rewind");
  char dst_bytes[256];
  size_t got = 0;
  while (got < sizeof(dst_bytes)) {
    ssize_t r = read(dst, dst_bytes + got, sizeof(dst_bytes) - got);
    if (r < 0) {
      if (errno == EINTR)
        continue;
      fail("read dst");
    }
    if (r == 0)
      break;
    got += (size_t)r;
  }
  if (got != sizeof(dst_bytes))
    fail("short destination readback");
  long checksum = 0;
  for (size_t i = 0; i < sizeof(dst_bytes); i++)
    checksum += (unsigned char)dst_bytes[i];

  /* Explicit-offset form: copy 50 bytes from position 0, update the pointer,
   * and leave the source's own file offset unchanged. */
  off_t pos = 0;
  size_t off_done = 0;
  while (off_done < 50) {
    ssize_t s = sendfile(dst, src, &pos, 50 - off_done);
    if (s < 0) {
      if (errno == EINTR)
        continue;
      fail("sendfile explicit offset");
    }
    if (s == 0)
      fail("sendfile explicit offset short");
    off_done += (size_t)s;
  }
  int own_offset_kept = (lseek(src, 0, SEEK_CUR) == 256) ? 1 : 0;

  if (close(src) != 0)
    fail("close src");
  if (close(dst) != 0)
    fail("close dst");

  printf("sendfile copied=%zu checksum=%ld pos=%ld own_offset_kept=%d\n", copied,
         checksum, (long)pos, own_offset_kept);
  return 0;
}
