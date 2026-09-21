/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#define _GNU_SOURCE

/* Regular-file sendfile coverage recovered from
 * https://github.com/rrnewton/hermit/pull/2425.
 * record_replay_file_state.c separately retains sendfile-to-stdout coverage.
 * Relative paths keep each invocation inside the harness's private workdir.
 */

#include <errno.h>
#include <fcntl.h>
#include <linux/fs.h>
#include <stdio.h>
#include <sys/sendfile.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>

static int fail(const char* operation) {
  fprintf(stderr, "%s failed: %s\n", operation, strerror(errno));
  return 1;
}

static int write_all(int fd, const char* bytes, size_t length) {
  while (length > 0) {
    const ssize_t written = write(fd, bytes, length);
    if (written < 0) {
      return -1;
    }
    bytes += written;
    length -= (size_t)written;
  }
  return 0;
}

static int sendfile_through_regular_sink(int sink_fd, int source_fd,
                                        off_t source_offset,
                                        const char* expected, size_t length) {
  if (ftruncate(sink_fd, 0) != 0 || lseek(sink_fd, 0, SEEK_SET) != 0) {
    return -1;
  }

  off_t offset = source_offset;
  if (sendfile(sink_fd, source_fd, &offset, length) != (ssize_t)length ||
      lseek(sink_fd, 0, SEEK_SET) != 0) {
    return -1;
  }

  char bytes[64];
  if (length > sizeof(bytes)) {
    errno = EOVERFLOW;
    return -1;
  }
  size_t copied = 0;
  while (copied < length) {
    const ssize_t count = read(sink_fd, bytes + copied, length - copied);
    if (count <= 0) {
      if (count == 0) {
        errno = EIO;
      }
      return -1;
    }
    copied += (size_t)count;
  }
  if (memcmp(bytes, expected, length) != 0) {
    errno = EIO;
    return -1;
  }
  if (write_all(STDOUT_FILENO, bytes, length) != 0 ||
      write_all(STDOUT_FILENO, "\n", 1) != 0) {
    return -1;
  }
  return 0;
}

int main(void) {
  const char* dir = "hermit-record-file-state-regular-sink";
  const char* file = "hermit-record-file-state-regular-sink/data";
  const char* source = "hermit-record-file-state-regular-sink/source";
  const char* clone = "hermit-record-file-state-regular-sink/clone";
  const char* sendfile_sink =
      "hermit-record-file-state-regular-sink/sendfile-sink";
  const char* write_only_clone =
      "hermit-record-file-state-regular-sink/write-only-clone";

  unlink(write_only_clone);
  unlink(sendfile_sink);
  unlink(clone);
  unlink(source);
  unlink(file);
  rmdir(dir);
  if (mkdir(dir, 0700) != 0) {
    return fail("mkdir");
  }

  const int sendfile_sink_fd =
      open(sendfile_sink, O_CREAT | O_TRUNC | O_RDWR, 0600);
  if (sendfile_sink_fd < 0) {
    return fail("open(sendfile sink)");
  }

  int fd = open(file, O_CREAT | O_TRUNC | O_RDWR, 0600);
  if (fd < 0) {
    return fail("open(create)");
  }
  if (write_all(fd, "parent", 6) != 0) {
    return fail("write(parent)");
  }

  const pid_t child = fork();
  if (child < 0) {
    return fail("fork");
  }
  if (child == 0) {
    if (write_all(fd, "child", 5) != 0) {
      _exit(2);
    }
    _exit(0);
  }

  int status = 0;
  if (waitpid(child, &status, 0) != child || status != 0) {
    fprintf(stderr, "child write failed: status=%d\n", status);
    return 1;
  }
  if (close(fd) != 0) {
    return fail("close(created)");
  }

  fd = open(file, O_RDWR);
  if (fd < 0) {
    return fail("open(reopen)");
  }
  if (lseek(fd, 0, SEEK_END) < 0) {
    return fail("lseek(end)");
  }
  if (write_all(fd, "reopen", 6) != 0) {
    return fail("write(reopen)");
  }
  if (ftruncate(fd, 5) != 0) {
    return fail("ftruncate");
  }
  if (lseek(fd, 0, SEEK_SET) != 0) {
    return fail("lseek(start)");
  }

  char bytes[6] = {0};
  if (read(fd, bytes, 5) != 5) {
    return fail("read(truncated)");
  }
  if (strcmp(bytes, "paren") != 0) {
    fprintf(stderr, "unexpected file contents: %s\n", bytes);
    return 1;
  }

  if (unlink(file) != 0) {
    return fail("unlink(open file)");
  }
  if (lseek(fd, 0, SEEK_END) < 0 || write_all(fd, "B", 1) != 0) {
    return fail("write(unlinked file)");
  }
  if (sendfile_through_regular_sink(sendfile_sink_fd, fd, 0, "parenB", 6) !=
      0) {
    return fail("sendfile(unlinked file)");
  }
  if (close(fd) != 0) {
    return fail("close(reopened)");
  }

  const int source_fd = open(source, O_CREAT | O_TRUNC | O_RDWR, 0600);
  if (source_fd < 0) {
    return fail("open(source)");
  }
  if (ftruncate(source_fd, 1024 * 1024) != 0 ||
      pwrite(source_fd, "payload", 7, 4096) != 7) {
    return fail("prepare sparse source");
  }
  const int clone_fd = open(clone, O_CREAT | O_TRUNC | O_RDWR, 0600);
  if (clone_fd < 0) {
    return fail("open(clone)");
  }

  const off_t source_position = lseek(source_fd, 0, SEEK_CUR);
  if (source_position < 0) {
    return fail("lseek(source before clone)");
  }
  int clone_supported = 0;
  if (ioctl(clone_fd, FICLONE, source_fd) == 0) {
    if (lseek(source_fd, 0, SEEK_CUR) != source_position) {
      fprintf(stderr, "clone changed source file offset\n");
      return 1;
    }
    clone_supported = 1;
    char payload[8] = {0};
    if (pread(clone_fd, payload, 7, 4096) != 7 ||
        strcmp(payload, "payload") != 0) {
      fprintf(stderr, "cloned payload mismatch\n");
      return 1;
    }
    const off_t data = lseek(clone_fd, 0, SEEK_DATA);
    const off_t hole = data < 0 ? -1 : lseek(clone_fd, data, SEEK_HOLE);
    if (data < 0 || hole < 0) {
      return fail("seek cloned extent");
    }
    printf("clone extent: %lld %lld\n", (long long)data, (long long)hole);
    if (sendfile_through_regular_sink(sendfile_sink_fd, clone_fd, 4096,
                                     "payload", 7) != 0) {
      return fail("sendfile(clone)");
    }
  } else if (errno == EOPNOTSUPP || errno == ENOTTY || errno == EXDEV ||
             errno == EINVAL) {
    printf("clone unsupported\n");
  } else {
    return fail("ioctl(FICLONE)");
  }

  if (close(clone_fd) != 0) {
    return fail("close(clone)");
  }

  if (clone_supported) {
    const int write_only_fd =
        open(write_only_clone, O_CREAT | O_TRUNC | O_WRONLY, 0200);
    if (write_only_fd < 0) {
      return fail("open(write-only clone)");
    }
    if (ioctl(write_only_fd, FICLONE, source_fd) != 0) {
      return fail("ioctl(write-only FICLONE)");
    }
    struct stat clone_stat;
    if (fstat(write_only_fd, &clone_stat) != 0) {
      return fail("fstat(write-only clone)");
    }
    if ((clone_stat.st_mode & 0777) != 0200) {
      fprintf(stderr, "write-only clone permissions changed: %#o\n",
              clone_stat.st_mode & 0777);
      return 1;
    }
    if (close(write_only_fd) != 0 || chmod(write_only_clone, 0600) != 0) {
      return fail("close/chmod(write-only clone)");
    }
    const int verify_fd = open(write_only_clone, O_RDONLY);
    if (verify_fd < 0) {
      return fail("open(write-only clone for verify)");
    }
    char payload[8] = {0};
    if (pread(verify_fd, payload, 7, 4096) != 7 ||
        strcmp(payload, "payload") != 0) {
      fprintf(stderr, "write-only cloned payload mismatch\n");
      return 1;
    }
    if (close(verify_fd) != 0) {
      return fail("close(write-only clone verify)");
    }
  }

  if (close(source_fd) != 0) {
    return fail("close(clone files)");
  }
  if (close(sendfile_sink_fd) != 0) {
    return fail("close(sendfile sink)");
  }
  if ((clone_supported && unlink(write_only_clone) != 0) ||
      unlink(sendfile_sink) != 0 || unlink(clone) != 0 ||
      unlink(source) != 0 || rmdir(dir) != 0) {
    return fail("cleanup");
  }

  puts("file state preserved");
  return 0;
}
