/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Linux names a random device by its device number, 1:8 (/dev/random) or 1:9
 * (/dev/urandom), whatever path reached it. Under Hermit every open of one is
 * a new open file description that reads a deterministic stream of its own:
 * Hermit's canonical stream, whose byte i is (i * 73 + 41) mod 256 XOR byte
 * (i mod 8) of the stream's seed, under a seed no earlier open used. So the
 * first bytes read through any spelling reveal their seed, follow the
 * canonical pattern, and repeat no earlier open's stream, as Linux's opens
 * never repeat. Before Hermit classified random devices by device number,
 * every spelling but the two literal names read the host's entropy on ptrace
 * (https://github.com/rrnewton/hermit/issues/4544).
 *
 * Each check prints one line, and standard output is the oracle: each
 * backend's cell declares the exact output. The first open's bytes are
 * printed as the reference, which pins its stream under the run seed. A
 * spelling whose bytes are not the canonical pattern prints them; natively no
 * spelling's are, since the host's entropy has no pattern. Spellings a backend cannot open print the
 * refusal, so a backend's own expected output records what it supports. The
 * program exits 0 whatever it found, so a backend's known difference is
 * recorded in its expected output rather than failing the run before the
 * output is compared. With --strict-checks every printed check that differs
 * from Linux's answer counts as a failure and the program exits 1 on any,
 * for runs whose harness compares no expected output.
 *
 * A bind mount of the device and a copy of its node are covered by the same
 * device-number rule but cannot be tested under Hermit: Detcore refuses
 * mount(2) with EPERM, and the guest's user namespace cannot create a
 * character device. A regular file reached through a name like the device's
 * stays a regular file, and a write-only descriptor of the device reads
 * nothing (EBADF), as natively.
 */

#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/sysmacros.h>
#include <sys/wait.h>
#include <termios.h>
#include <unistd.h>

#define CHUNK 16

static int failures;
/* With --strict-checks every printed check that differs from Linux's answer
 * counts as a failure, and the program exits 1 on any. */
static int strict;

static void strict_check(int ok) {
  if (!ok && strict) {
    failures++;
  }
}
/* Seeds of the streams that earlier opens read, and the last bytes read. */
static uint64_t seeds[16];
static size_t opens;
static unsigned char last_chunk[CHUNK];

static void print_hex(const unsigned char* bytes, size_t len) {
  for (size_t i = 0; i < len; i++) {
    printf("%02x", bytes[i]);
  }
}

static int read_exact(int fd, unsigned char* buf, size_t len) {
  size_t done = 0;
  while (done < len) {
    ssize_t got = read(fd, buf + done, len - done);
    if (got <= 0) {
      return -1;
    }
    done += (size_t)got;
  }
  return 0;
}

static unsigned char canonical_byte(uint64_t seed, size_t index) {
  return (unsigned char)((unsigned char)index * 73 + 41) ^
      (unsigned char)(seed >> (8 * (index % 8)));
}

/* Read CHUNK bytes from `fd` at stream offset `offset` and check that they are
 * the canonical stream seeded `*seed`, or, for a new open (`offset` 0), learn
 * the seed from the first eight bytes, check the rest against it, and check
 * that no earlier open read this stream. */
static void expect_stream(
    const char* label,
    int fd,
    size_t offset,
    uint64_t* seed) {
  unsigned char* buf = last_chunk;
  errno = 0;
  if (read_exact(fd, buf, CHUNK) != 0) {
    printf("%s read-failed errno=%d\n", label, errno);
    failures++;
    return;
  }
  if (offset == 0) {
    *seed = 0;
    for (size_t i = 0; i < 8; i++) {
      *seed |= (uint64_t)(buf[i] ^ canonical_byte(0, i)) << (8 * i);
    }
  }
  int canonical = 1;
  for (size_t i = 0; i < CHUNK; i++) {
    canonical &= buf[i] == canonical_byte(*seed, offset + i);
  }
  int repeated = 0;
  for (size_t i = 0; offset == 0 && i < opens; i++) {
    repeated |= seeds[i] == *seed;
  }
  /* An alias whose cursor fell behind rereads an earlier chunk. */
  size_t reread = offset;
  for (size_t earlier = 0; !canonical && earlier < offset; earlier += CHUNK) {
    int same = 1;
    for (size_t i = 0; i < CHUNK; i++) {
      same &= buf[i] == canonical_byte(*seed, earlier + i);
    }
    reread = same ? earlier : reread;
  }
  if (reread != offset) {
    printf("%s REREADS stream offset %zu\n", label, reread);
    failures++;
  } else if (!canonical) {
    printf("%s DIFFERENT bytes=", label);
    print_hex(buf, CHUNK);
    printf("\n");
    failures++;
  } else if (repeated) {
    printf("%s REPEATS an earlier open's stream\n", label);
    failures++;
  } else {
    printf("%s %s\n", label, offset == 0 ? "new-stream" : "same-stream");
  }
  if (offset == 0 && opens == sizeof seeds / sizeof seeds[0]) {
    printf("%s too many opens to track\n", label);
    failures++;
  } else if (offset == 0) {
    seeds[opens++] = *seed;
  }
}

static void spelling(const char* name, int dirfd, const char* path) {
  char label[64];
  snprintf(label, sizeof label, "spelling %s", name);
  errno = 0;
  int fd = openat(dirfd, path, O_RDONLY | O_CLOEXEC);
  if (fd < 0) {
    printf("%s refused errno=%d\n", label, errno);
    strict_check(0);
    return;
  }
  uint64_t seed;
  expect_stream(label, fd, 0, &seed);
  close(fd);
}

static void symlink_spelling(void) {
  char dir[] = "/tmp/random-device-linkXXXXXX";
  if (mkdtemp(dir) == NULL) {
    printf("scratch-directory unavailable\n");
    failures++;
    return;
  }
  char link[64];
  snprintf(link, sizeof link, "%s/link", dir);
  if (symlink("/dev/urandom", link) == 0) {
    spelling("symlink", AT_FDCWD, link);
    unlink(link);
  } else {
    printf("spelling symlink unavailable errno=%d\n", errno);
    failures++;
  }
  rmdir(dir);
}

/* A regular file named like the device, reached through a directory link
 * named like /dev: a regular file, which reads its own bytes. */
static void regular_file_alias(void) {
  char dir[] = "/tmp/random-device-aliasXXXXXX";
  if (mkdtemp(dir) == NULL) {
    printf("regular-file-alias scratch unavailable\n");
    failures++;
    return;
  }
  char real[64], file[80], link[64], spelled[96];
  snprintf(real, sizeof real, "%s/real", dir);
  snprintf(file, sizeof file, "%s/urandom", real);
  snprintf(link, sizeof link, "%s/dev", dir);
  snprintf(spelled, sizeof spelled, "%s/./urandom", link);
  static const char contents[] = "regular-file-alias";
  int fd = -1;
  if (mkdir(real, 0700) == 0 && symlink(real, link) == 0) {
    fd = open(file, O_CREAT | O_WRONLY | O_CLOEXEC, 0600);
  }
  if (fd < 0 ||
      write(fd, contents, sizeof contents - 1) != (ssize_t)(sizeof contents - 1)) {
    printf("regular-file-alias setup failed errno=%d\n", errno);
    failures++;
  } else {
    close(fd);
    fd = open(spelled, O_RDONLY | O_CLOEXEC);
    char buf[64] = {0};
    ssize_t got = fd < 0 ? -1 : read(fd, buf, sizeof buf - 1);
    if (got == (ssize_t)(sizeof contents - 1) &&
        memcmp(buf, contents, (size_t)got) == 0) {
      printf("regular-file-alias reads its own bytes\n");
    } else {
      printf("regular-file-alias UNEXPECTED read=%zd errno=%d\n", got, errno);
      failures++;
    }
    if (fd >= 0) {
      close(fd);
    }
  }
  unlink(file);
  unlink(link);
  rmdir(real);
  rmdir(dir);
}

/* A write-only descriptor of the device, through an alternate spelling and the
 * literal name: read and pread refuse with EBADF, as natively. */
static void write_only_alias(void) {
  const char* paths[] = {"/dev//urandom", "/dev/urandom"};
  for (int i = 0; i < 2; i++) {
    int fd = open(paths[i], O_WRONLY | O_CLOEXEC);
    unsigned char byte;
    errno = 0;
    ssize_t r = fd < 0 ? -2 : read(fd, &byte, 1);
    int read_errno = errno;
    errno = 0;
    ssize_t p = fd < 0 ? -2 : pread(fd, &byte, 1, 0);
    printf(
        "write-only %s read=%zd errno=%d pread=%zd errno=%d\n",
        paths[i],
        r,
        r < 0 ? read_errno : 0,
        p,
        p < 0 ? errno : 0);
    strict_check(r == -1 && read_errno == EBADF && p == -1 && errno == EBADF);
    if (fd >= 0) {
      close(fd);
    }
  }
}

/* What the kernel reports about a descriptor of the device: its link, a
 * record lock, and a terminal query. */
static void descriptor_queries(void) {
  int fd = open("/dev//urandom", O_RDONLY | O_CLOEXEC);
  if (fd < 0) {
    printf("descriptor-queries open failed errno=%d\n", errno);
    failures++;
    return;
  }
  char link[64], target[128] = {0};
  snprintf(link, sizeof link, "/proc/self/fd/%d", fd);
  ssize_t len = readlink(link, target, sizeof target - 1);
  printf("readlink=%s\n", len < 0 ? "(failed)" : target);
  strict_check(len >= 0 && strcmp(target, "/dev/urandom") == 0);
  struct flock lock = {.l_type = F_RDLCK, .l_whence = SEEK_SET};
  errno = 0;
  int locked = fcntl(fd, F_SETLK, &lock);
  printf("fcntl F_SETLK=%d errno=%d\n", locked, locked < 0 ? errno : 0);
  strict_check(locked == 0);
  struct termios term;
  errno = 0;
  int tty = ioctl(fd, TCGETS, &term);
  printf("ioctl TCGETS=%d errno=%d\n", tty, tty < 0 ? errno : 0);
  strict_check(tty == -1 && errno == EINVAL);
  close(fd);
}

static void device_metadata(const char* path, unsigned minor) {
  struct stat st;
  int fd = open(path, O_RDONLY | O_CLOEXEC);
  if (fd < 0 || fstat(fd, &st) != 0) {
    printf("fstat %s failed errno=%d\n", path, errno);
    failures++;
  } else if (
      !S_ISCHR(st.st_mode) || major(st.st_rdev) != 1 ||
      minor(st.st_rdev) != minor || st.st_size != 0) {
    printf(
        "fstat %s UNEXPECTED mode=%o rdev=%u:%u size=%lld\n",
        path,
        (unsigned)st.st_mode,
        major(st.st_rdev),
        minor(st.st_rdev),
        (long long)st.st_size);
    failures++;
  } else {
    printf("fstat %s character-device 1:%u size=0\n", path, minor);
  }
  if (fd >= 0) {
    close(fd);
  }
}

/* One open file description: dup and fork aliases consume one cursor. */
static void shared_cursor(void) {
  int fd = open("/dev//urandom", O_RDONLY | O_CLOEXEC);
  if (fd < 0) {
    printf("shared-cursor open failed errno=%d\n", errno);
    failures++;
    return;
  }
  uint64_t seed = 0;
  expect_stream("cursor first", fd, 0, &seed);
  int alias = dup(fd);
  if (alias < 0) {
    printf("cursor dup failed errno=%d\n", errno);
    failures++;
  } else {
    expect_stream("cursor dup", alias, CHUNK, &seed);
    close(alias);
  }
  fflush(stdout);
  int before = failures;
  pid_t child = fork();
  if (child == 0) {
    expect_stream("cursor fork-child", fd, 2 * CHUNK, &seed);
    fflush(stdout);
    _exit(failures == before ? 0 : 1);
  }
  int status = 0;
  if (child < 0 || waitpid(child, &status, 0) != child ||
      !WIFEXITED(status) || WEXITSTATUS(status) != 0) {
    printf("cursor fork-child failed\n");
    failures++;
  }
  expect_stream("cursor fork-parent", fd, 3 * CHUNK, &seed);
  close(fd);
}

static void no_mapping(void) {
  int fd = open("/dev/./urandom", O_RDONLY | O_CLOEXEC);
  errno = 0;
  void* mapped = fd < 0
      ? MAP_FAILED
      : mmap(NULL, 4096, PROT_READ, MAP_PRIVATE, fd, 0);
  if (mapped != MAP_FAILED) {
    printf("mmap UNEXPECTED success\n");
    failures++;
    munmap(mapped, 4096);
  } else {
    printf("mmap errno=%d\n", errno);
    strict_check(errno == ENODEV);
  }
  if (fd >= 0) {
    close(fd);
  }
}

static void path_only(void) {
  int fd = open("/dev/../dev/urandom", O_PATH | O_CLOEXEC);
  unsigned char byte;
  errno = 0;
  ssize_t got = fd < 0 ? -2 : read(fd, &byte, 1);
  printf("o-path read=%zd errno=%d\n", got, got < 0 ? errno : 0);
  strict_check(got == -1 && errno == EBADF);
  if (fd >= 0) {
    close(fd);
  }
}

int main(int argc, char** argv) {
  strict = argc > 1 && strcmp(argv[1], "--strict-checks") == 0;
  setvbuf(stdout, NULL, _IONBF, 0);
  spelling("literal-urandom", AT_FDCWD, "/dev/urandom");
  /* The first open's bytes pin its stream under the run seed. */
  printf("reference=");
  print_hex(last_chunk, CHUNK);
  printf("\n");
  spelling("literal-random", AT_FDCWD, "/dev/random");
  spelling("double-slash", AT_FDCWD, "/dev//urandom");
  spelling("dot", AT_FDCWD, "/dev/./urandom");
  spelling("dot-dot", AT_FDCWD, "/dev/../dev/urandom");
  int dev = open("/dev", O_RDONLY | O_DIRECTORY | O_CLOEXEC);
  if (dev >= 0) {
    spelling("directory-relative", dev, "urandom");
    close(dev);
  }
  if (chdir("/dev") == 0) {
    spelling("cwd-relative", AT_FDCWD, "urandom");
    if (chdir("/") != 0) {
      return 2;
    }
  }
  symlink_spelling();
  int fd = open("/dev/urandom", O_RDONLY | O_CLOEXEC);
  if (fd >= 0) {
    char path[64];
    snprintf(path, sizeof path, "/proc/self/fd/%d", fd);
    spelling("proc-self-fd", AT_FDCWD, path);
    snprintf(path, sizeof path, "/dev/fd/%d", fd);
    spelling("dev-fd", AT_FDCWD, path);
    close(fd);
  }
  regular_file_alias();
  write_only_alias();
  descriptor_queries();
  device_metadata("/dev/urandom", 9);
  device_metadata("/dev/random", 8);
  shared_cursor();
  no_mapping();
  path_only();
  printf("failures=%d\n", failures);
  return strict && failures != 0 ? 1 : 0;
}
