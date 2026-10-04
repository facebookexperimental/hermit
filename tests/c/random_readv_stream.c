// @lint-ignore LICENSELINT

#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif

#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <unistd.h>

/*
 * Default mode checks Hermit's canonical stream through independent opens.
 * --native checks only Linux counts, errno, and untouched memory. Native random
 * bytes are deliberately never compared, including between independent opens.
 */
static int native;
static int expected_seed;

#define CHECK(condition)            \
  do {                              \
    if (!(condition)) {             \
      fprintf(                      \
          stderr,                   \
          "%s:%d: %s (errno=%d)\n", \
          __FILE__,                 \
          __LINE__,                 \
          #condition,               \
          errno);                   \
      exit(1);                      \
    }                               \
  } while (0)

static long raw_readv(int fd, const void* iov, unsigned long count) {
  return syscall(SYS_readv, fd, iov, count);
}

static void result(
    int fd,
    const void* iov,
    unsigned long count,
    long expected,
    int expected_errno) {
  errno = 0;
  long actual = raw_readv(fd, iov, count);
  CHECK(actual == expected);
  CHECK(errno == expected_errno);
}

static void dump(const char* name, const uint8_t* bytes, size_t length) {
  if (native) {
    return;
  }
  printf("%s", name);
  for (size_t i = 0; i < length; ++i) {
    printf(" %02x", bytes[i]);
  }
  putchar('\n');
}

static void control(const char* path, uint8_t* bytes, size_t length) {
  if (!native) {
    int fd = open(path, O_RDONLY);
    CHECK(fd >= 0);
    CHECK(read(fd, bytes, length) == (ssize_t)length);
    /* A literal oracle independent of either Hermit read implementation. */
    const uint8_t seed0[] = {
        41, 114, 187, 4, 77, 150, 223, 40, 113, 186, 3, 76, 149, 222, 39, 112};
    const uint8_t seed17[] = {
        56, 114, 187, 4, 77, 150, 223, 40, 96, 186, 3, 76, 149, 222, 39, 112};
    size_t checked = length < sizeof(seed0) ? length : sizeof(seed0);
    CHECK(memcmp(bytes, expected_seed == 17 ? seed17 : seed0, checked) == 0);
    CHECK(close(fd) == 0);
  }
}

static void
equal(const uint8_t* actual, const uint8_t* expected, size_t length) {
  if (!native) {
    CHECK(memcmp(actual, expected, length) == 0);
  }
}

static void mixed_stream(const char* path) {
  uint8_t expected[15] = {0};
  uint8_t actual[15] = {0};
  control(path, expected, sizeof(expected));
  int fd = open(path, O_RDONLY);
  CHECK(fd >= 0);
  CHECK(read(fd, actual, 7) == 7);
  int alias = dup(fd);
  CHECK(alias >= 0);
  struct iovec iov[] = {{actual + 7, 2}, {actual + 9, 3}};
  result(alias, iov, 2, 5, 0);
  CHECK(read(fd, actual + 12, 3) == 3);
  equal(actual, expected, sizeof(actual));
  dump("mixed", actual, sizeof(actual));
  CHECK(close(alias) == 0);
  CHECK(close(fd) == 0);
}

static void long_stream(const char* path) {
  enum {
    PREFIX = 7,
    VECTOR = 65553,
    SUFFIX = 3,
    TOTAL = PREFIX + VECTOR + SUFFIX
  };
  uint8_t* expected = calloc(TOTAL, 1);
  uint8_t* actual = calloc(TOTAL, 1);
  CHECK(expected != NULL && actual != NULL);
  control(path, expected, TOTAL);
  int fd = open(path, O_RDONLY);
  CHECK(fd >= 0);
  CHECK(read(fd, actual, PREFIX) == PREFIX);
  struct iovec iov[] = {
      {actual + PREFIX, 65529}, {actual + PREFIX + 65529, VECTOR - 65529}};
  result(fd, iov, 2, VECTOR, 0);
  CHECK(read(fd, actual + PREFIX + VECTOR, SUFFIX) == SUFFIX);
  equal(actual, expected, TOTAL);
  dump("long-tail", actual + TOTAL - 16, 16);
  CHECK(close(fd) == 0);
  free(actual);
  free(expected);
}

static void child_read(int fd, int output) {
  uint8_t bytes[5];
  struct iovec iov[] = {{bytes, 2}, {bytes + 2, 3}};
  result(fd, iov, 2, 5, 0);
  CHECK(write(output, bytes, sizeof(bytes)) == (ssize_t)sizeof(bytes));
  CHECK(close(fd) == 0);
  CHECK(close(output) == 0);
  _exit(0);
}

static void
inherited_stream(const char* path, const char* executable, int with_exec) {
  uint8_t expected[15] = {0};
  uint8_t actual[15] = {0};
  control(path, expected, sizeof(expected));
  int fd = open(path, O_RDONLY);
  CHECK(fd >= 0);
  CHECK(read(fd, actual, 7) == 7);
  int alias = dup(fd);
  CHECK(alias >= 0);
  int channel[2];
  CHECK(pipe(channel) == 0);
  pid_t child = fork();
  CHECK(child >= 0);
  if (child == 0) {
    CHECK(close(channel[0]) == 0);
    CHECK(close(fd) == 0);
    if (with_exec) {
      char input_arg[32];
      char output_arg[32];
      CHECK(snprintf(input_arg, sizeof(input_arg), "%d", alias) > 0);
      CHECK(snprintf(output_arg, sizeof(output_arg), "%d", channel[1]) > 0);
      execl(
          executable,
          executable,
          "--exec-child",
          input_arg,
          output_arg,
          (char*)NULL);
      CHECK(0);
    }
    child_read(alias, channel[1]);
  }
  CHECK(close(alias) == 0);
  CHECK(close(channel[1]) == 0);
  CHECK(read(channel[0], actual + 7, 5) == 5);
  CHECK(close(channel[0]) == 0);
  int status;
  CHECK(waitpid(child, &status, 0) == child);
  CHECK(WIFEXITED(status) && WEXITSTATUS(status) == 0);
  CHECK(read(fd, actual + 12, 3) == 3);
  equal(actual, expected, sizeof(actual));
  dump(with_exec ? "exec-alias" : "fork-alias", actual, sizeof(actual));
  CHECK(close(fd) == 0);
}

struct stream {
  int fd;
  size_t offset;
  uint8_t expected[8192];
};

/* Every rejected/short request is followed by this independent cursor oracle.
 */
static void next_scalar(struct stream* stream) {
  uint8_t actual[3];
  CHECK(stream->offset + sizeof(actual) <= sizeof(stream->expected));
  CHECK(read(stream->fd, actual, sizeof(actual)) == (ssize_t)sizeof(actual));
  equal(actual, stream->expected + stream->offset, sizeof(actual));
  dump("next", actual, sizeof(actual));
  stream->offset += sizeof(actual);
}

static void access_modes(const char* path) {
  const void* invalid = (const void*)UINTPTR_MAX;
  result(-1, invalid, 1025, -1, EBADF);
  errno = 0;
  CHECK(read(-1, NULL, 0) == -1);
  CHECK(errno == EBADF);
  const int modes[] = {O_WRONLY, O_PATH, O_ACCMODE};
  for (size_t i = 0; i < sizeof(modes) / sizeof(modes[0]); ++i) {
    int fd = open(path, modes[i]);
    CHECK(fd >= 0);
    result(fd, invalid, 1025, -1, EBADF);
    result(fd, NULL, 0, -1, EBADF);
    CHECK(close(fd) == 0);
  }
  const int status_modes[] = {O_RDONLY, O_WRONLY, O_RDWR, O_ACCMODE};
  const int updates[] = {O_NONBLOCK, O_RDWR, O_WRONLY, O_PATH | O_NONBLOCK};
  for (size_t i = 0; i < sizeof(status_modes) / sizeof(status_modes[0]); ++i) {
    int fd = open(path, status_modes[i]);
    CHECK(fd >= 0);
    int alias = dup(fd);
    CHECK(alias >= 0);
    struct stream stream = {.fd = fd};
    control(path, stream.expected, sizeof(stream.expected));
    for (size_t j = 0; j < sizeof(updates) / sizeof(updates[0]); ++j) {
      CHECK(fcntl(alias, F_SETFL, updates[j]) == 0);
      CHECK((fcntl(fd, F_GETFL) & O_ACCMODE) == status_modes[i]);
      int readable = status_modes[i] == O_RDONLY || status_modes[i] == O_RDWR;
      result(fd, NULL, 0, readable ? 0 : -1, readable ? 0 : EBADF);
      errno = 0;
      CHECK(read(fd, NULL, 0) == (readable ? 0 : -1));
      CHECK(errno == (readable ? 0 : EBADF));
      errno = 0;
      CHECK(syscall(SYS_read, fd, invalid, 0) == -1);
      CHECK(errno == (readable ? EFAULT : EBADF));
      uint8_t byte = 0xa5;
      struct iovec one = {&byte, 1};
      result(alias, &one, 1, readable ? 1 : -1, readable ? 0 : EBADF);
      if (readable) {
        equal(&byte, stream.expected + stream.offset, 1);
        stream.offset += 1;
        next_scalar(&stream);
      } else {
        CHECK(byte == 0xa5);
        errno = 0;
        CHECK(read(fd, &byte, 1) == -1);
        CHECK(errno == EBADF);
        CHECK(byte == 0xa5);
      }
    }
    CHECK(close(alias) == 0);
    CHECK(close(fd) == 0);
  }
}

static void import_and_faults(const char* path) {
  struct stream stream = {.fd = open(path, O_RDONLY)};
  CHECK(stream.fd >= 0);
  control(path, stream.expected, sizeof(stream.expected));
  const void* invalid = (const void*)UINTPTR_MAX;

  result(stream.fd, invalid, 0, 0, 0);
  next_scalar(&stream);
  result(stream.fd, invalid, 1UL << 32, 0, 0);
  next_scalar(&stream);
  result(stream.fd, NULL, 1025, -1, EINVAL);
  next_scalar(&stream);
  result(stream.fd, NULL, (1UL << 32) | 1025, -1, EINVAL);
  next_scalar(&stream);
  result(stream.fd, invalid, 1, -1, EFAULT);
  next_scalar(&stream);

  struct iovec empty[1024] = {{0}};
  result(stream.fd, empty, 1024, 0, 0);
  next_scalar(&stream);
  result(stream.fd, empty, 1, 0, 0);
  next_scalar(&stream);
  empty[0].iov_base = (void*)UINTPTR_MAX;
  result(stream.fd, empty, 1, -1, EFAULT);
  next_scalar(&stream);

  uint8_t good[16];
  memset(good, 0xa5, sizeof(good));
  struct iovec iov[] = {{good, 2}, {good + 2, 3}};
  result(stream.fd, iov, (1UL << 32) | 2, 5, 0);
  equal(good, stream.expected + stream.offset, 5);
  stream.offset += 5;
  CHECK(good[5] == 0xa5);
  next_scalar(&stream);

  memset(good, 0xa5, sizeof(good));
  iov[0] = (struct iovec){good, 3};
  iov[1] = (struct iovec){(void*)UINTPTR_MAX, 1};
  result(stream.fd, iov, 2, -1, EFAULT);
  for (size_t i = 0; i < sizeof(good); ++i) {
    CHECK(good[i] == 0xa5);
  }
  next_scalar(&stream);

  /* Even an empty later segment must have structurally valid geometry. */
  iov[1] = (struct iovec){(void*)UINTPTR_MAX, 0};
  result(stream.fd, iov, 2, -1, EFAULT);
  for (size_t i = 0; i < sizeof(good); ++i) {
    CHECK(good[i] == 0xa5);
  }
  next_scalar(&stream);
  iov[1] = (struct iovec){(void*)((1ULL << 47) - 4096), 0};
  result(stream.fd, iov, 2, 3, 0);
  equal(good, stream.expected + stream.offset, 3);
  stream.offset += 3;
  CHECK(good[3] == 0xa5);
  next_scalar(&stream);

  long page_size = sysconf(_SC_PAGESIZE);
  CHECK(page_size >= 4096);
  size_t page = (size_t)page_size;
  /* Low non-fixed hint plus an explicit bound makes the single-vector clamp
     range valid on both four-level and LA57 hosts, without replacing mappings.
   */
  uint8_t* mapping = mmap(
      (void*)0x100000000ULL,
      2 * page,
      PROT_READ | PROT_WRITE,
      MAP_PRIVATE | MAP_ANONYMOUS,
      -1,
      0);
  CHECK(mapping != MAP_FAILED);
  CHECK((uintptr_t)mapping <= (1ULL << 47) - 4096 - 2 * page - 0x7ffff000ULL);
  memset(mapping, 0xa5, 2 * page);
  struct iovec* last = (struct iovec*)(mapping + page - sizeof(struct iovec));
  *last = (struct iovec){good, SIZE_MAX};
  CHECK(mprotect(mapping + page, page, PROT_NONE) == 0);
  /* The first invalid signed length wins over an inaccessible later entry. */
  result(stream.fd, last, 2, -1, EINVAL);
  next_scalar(&stream);

  /* Structural array geometry is valid; the load itself must fault. */
  result(stream.fd, mapping + page, 1, -1, EFAULT);
  next_scalar(&stream);
  memset(good, 0xa5, sizeof(good));
  *last = (struct iovec){good, 3};
  result(stream.fd, last, 2, -1, EFAULT);
  for (size_t i = 0; i < sizeof(good); ++i) {
    CHECK(good[i] == 0xa5);
  }
  next_scalar(&stream);

  /* 2^62 is signed-positive and exceeds BOTH 47- and 56-bit user ceilings. */
  memset(mapping + page - 16, 0xa5, 16);
  iov[0] = (struct iovec){mapping + page - 16, 1ULL << 62};
  result(stream.fd, iov, 1, 16, 0);
  equal(mapping + page - 16, stream.expected + stream.offset, 16);
  stream.offset += 16;
  next_scalar(&stream);
  memset(mapping + page - 16, 0xa5, 16);
  iov[1] = (struct iovec){NULL, 0};
  result(stream.fd, iov, 2, -1, EFAULT);
  for (size_t i = 0; i < 16; ++i) {
    CHECK(mapping[page - 16 + i] == 0xa5);
  }
  next_scalar(&stream);

  iov[0] = (struct iovec){mapping + page, 8};
  result(stream.fd, iov, 1, -1, EFAULT);
  next_scalar(&stream);
  CHECK(mprotect(mapping + page, page, PROT_READ | PROT_WRITE) == 0);
  for (size_t i = 0; i < 8; ++i) {
    CHECK(mapping[page + i] == 0xa5);
  }
  CHECK(mprotect(mapping + page, page, PROT_NONE) == 0);

  iov[0] = (struct iovec){good, 3};
  iov[1] = (struct iovec){mapping + page, 5};
  result(stream.fd, iov, 2, 3, 0);
  equal(good, stream.expected + stream.offset, 3);
  stream.offset += 3;
  CHECK(good[3] == 0xa5);
  next_scalar(&stream);

  memset(mapping + page - 8, 0xa5, 8);
  iov[0] = (struct iovec){mapping + page - 4, 8};
  result(stream.fd, iov, 1, 4, 0);
  equal(mapping + page - 4, stream.expected + stream.offset, 4);
  stream.offset += 4;
  CHECK(mapping[page - 5] == 0xa5);
  next_scalar(&stream);
  CHECK(mprotect(mapping + page, page, PROT_READ | PROT_WRITE) == 0);
  for (size_t i = 0; i < 8; ++i) {
    CHECK(mapping[page + i] == 0xa5);
  }
  CHECK(mprotect(mapping + page, page, PROT_READ) == 0);
  for (size_t length = 7; length <= 8; ++length) {
    iov[0] = (struct iovec){mapping + page, length};
    result(stream.fd, iov, 1, -1, EFAULT);
    next_scalar(&stream);
    for (size_t i = 0; i < 8; ++i) {
      CHECK(mapping[page + i] == 0xa5);
    }
  }
  /* Every 4096-byte scatter chunk, including an 8-byte tail, honors protection.
   */
  memset(mapping + page - 4096, 0xa5, 4096);
  iov[0] = (struct iovec){mapping + page - 4096, 4096 + 8};
  result(stream.fd, iov, 1, 4096, 0);
  equal(mapping + page - 4096, stream.expected + stream.offset, 4096);
  stream.offset += 4096;
  next_scalar(&stream);
  CHECK(mprotect(mapping + page, page, PROT_READ | PROT_WRITE) == 0);
  for (size_t i = 0; i < 8; ++i) {
    CHECK(mapping[page + i] == 0xa5);
  }
  CHECK(munmap(mapping, 2 * page) == 0);

  /* Later vectors overwrite earlier output but still consume stream bytes. */
  memset(good, 0xa5, sizeof(good));
  iov[0] = (struct iovec){good, 4};
  iov[1] = (struct iovec){good + 2, 3};
  result(stream.fd, iov, 2, 7, 0);
  equal(good, stream.expected + stream.offset, 2);
  equal(good + 2, stream.expected + stream.offset + 4, 3);
  stream.offset += 7;
  CHECK(good[5] == 0xa5);
  next_scalar(&stream);

  /* Import the whole array before writing: segment zero overwrites segment one.
   */
  memset(good, 0xa5, sizeof(good));
  iov[0] = (struct iovec){&iov[1], sizeof(iov[1])};
  iov[1] = (struct iovec){good, 3};
  result(stream.fd, iov, 2, sizeof(iov[1]) + 3, 0);
  equal(
      (const uint8_t*)&iov[1], stream.expected + stream.offset, sizeof(iov[1]));
  stream.offset += sizeof(iov[1]);
  equal(good, stream.expected + stream.offset, 3);
  stream.offset += 3;
  CHECK(good[3] == 0xa5);
  next_scalar(&stream);
  CHECK(close(stream.fd) == 0);
}

/* x86-64 uses a full signed offset in arg3; arg4 is ignored padding. */
static void positioned_result(
    int version2,
    int fd,
    const void* iov,
    unsigned long count,
    int64_t offset,
    int flags,
    long expected,
    int expected_errno) {
  errno = 0;
  long actual = syscall(
      version2 ? SYS_preadv2 : SYS_preadv,
      fd,
      iov,
      count,
      (uint64_t)offset,
      0x123456789abcdef0UL,
      flags);
  CHECK(actual == expected);
  CHECK(errno == expected_errno);
}

static void positioned_stream(const char* path) {
  uint8_t expected[16] = {0};
  uint8_t actual[16] = {0};
  control(path, expected, sizeof(expected));
  int fd = open(path, O_RDONLY);
  CHECK(fd >= 0);
  int alias = dup(fd);
  CHECK(alias >= 0);
  CHECK(read(fd, actual, 3) == 3);
  struct iovec iov[] = {{actual + 3, 1}, {actual + 4, 1}};
  positioned_result(1, alias, iov, 2, -1, 0, 2, 0);
  CHECK(read(fd, actual + 5, 2) == 2);
  equal(actual, expected, 7);
  dump("positioned-mixed", actual, 7);

  const int64_t offsets[] = {5, 65536, 0x100000005LL, INT64_MAX - 1};
  /* Independent literal seed0/17 values, including the former carrier EOF. */
  const uint8_t values[] = {150, expected_seed == 17 ? 56 : 41, 150, 151};
  for (int version2 = 0; version2 <= 1; ++version2) {
    for (size_t i = 0; i < sizeof(offsets) / sizeof(offsets[0]); ++i) {
      uint8_t guarded[] = {0xa5, 0xa5, 0xa5};
      struct iovec one = {guarded + 1, 1};
      positioned_result(version2, alias, &one, 1, offsets[i], 0, 1, 0);
      CHECK(guarded[0] == 0xa5 && guarded[2] == 0xa5);
      equal(guarded + 1, values + i, 1);
      dump("positioned-byte", guarded + 1, 1);
    }
  }
  CHECK(read(fd, actual + 7, 3) == 3);
  equal(actual, expected, 10);
  dump("positioned-cursor", actual, 10);
  CHECK(close(alias) == 0);
  CHECK(close(fd) == 0);
}

static void positioned_errors(const char* path) {
  struct stream stream = {.fd = open(path, O_RDONLY)};
  CHECK(stream.fd >= 0);
  control(path, stream.expected, sizeof(stream.expected));
  const void* invalid = (const void*)UINTPTR_MAX;
  uint8_t guarded[3] = {0xa5, 0xa5, 0xa5};
  struct iovec one = {guarded + 1, 1};
  struct iovec empty[2] = {{NULL, 0}, {NULL, 0}};
  for (int version2 = 0; version2 <= 1; ++version2) {
    positioned_result(version2, -1, invalid, 1025, -2, 0, -1, EINVAL);
    positioned_result(
        version2, -1, invalid, 1025, -1, 0, -1, version2 ? EBADF : EINVAL);
    positioned_result(version2, -1, invalid, 1025, 0, 0, -1, EBADF);
    const int modes[] = {O_WRONLY, O_PATH};
    for (size_t i = 0; i < sizeof(modes) / sizeof(modes[0]); ++i) {
      int fd = open(path, modes[i]);
      CHECK(fd >= 0);
      positioned_result(version2, fd, invalid, 1, 0, 0, -1, EBADF);
      positioned_result(version2, fd, invalid, 0, 0, 0, -1, EBADF);
      CHECK(close(fd) == 0);
    }
    positioned_result(version2, stream.fd, invalid, 0, 0, 0, 0, 0);
    positioned_result(version2, stream.fd, invalid, 1UL << 32, 0, 0, 0, 0);
    positioned_result(version2, stream.fd, invalid, 1025, 0, 0, -1, EINVAL);
    positioned_result(version2, stream.fd, invalid, ~0UL, 0, 0, -1, EINVAL);
    positioned_result(version2, stream.fd, invalid, 1, 0, 0, -1, EFAULT);
    positioned_result(version2, stream.fd, empty, 2, INT64_MAX, 0, 0, 0);
    positioned_result(version2, stream.fd, &one, 1, INT64_MAX, 0, -1, EINVAL);
    CHECK(guarded[0] == 0xa5 && guarded[1] == 0xa5 && guarded[2] == 0xa5);
    positioned_result(version2, stream.fd, &one, (1UL << 32) | 1, 0, 0, 1, 0);
    equal(guarded + 1, stream.expected, 1);
    CHECK(guarded[0] == 0xa5 && guarded[2] == 0xa5);
    guarded[1] = 0xa5;
    next_scalar(&stream);
  }

  /* Observed random-device flag policy. 0x100 is Linux 7.1 RWF_NOSIGNAL. */
  const struct {
    int flags;
    int error;
  } cases[] = {
      {1, 0},
      {2, 0},
      {4, 0},
      {8, 0},
      {16, 0},
      {32, 0},
      {256, 0},
      {256 | 8, 0},
      {64, EOPNOTSUPP},
      {128, EOPNOTSUPP},
      {0x40000000, EOPNOTSUPP},
      {16 | 32, EINVAL},
      {0x40000000 | 16 | 32, EOPNOTSUPP},
      {64 | 16 | 32, EINVAL},
      {256 | 16 | 32, EINVAL},
      {256 | 0x40000000, EOPNOTSUPP},
  };
  for (size_t i = 0; i < sizeof(cases) / sizeof(cases[0]); ++i) {
    int flags = cases[i].flags, error = cases[i].error;
    guarded[1] = 0xa5;
    positioned_result(1, stream.fd, &one, 1, 0, flags, error ? -1 : 1, error);
    if (error) {
      CHECK(guarded[1] == 0xa5);
    } else {
      equal(guarded + 1, stream.expected, 1);
    }
    CHECK(guarded[0] == 0xa5 && guarded[2] == 0xa5);
    positioned_result(1, stream.fd, invalid, 1, 0, flags, -1, EFAULT);
    positioned_result(1, stream.fd, invalid, 0, 0, flags, 0, 0);
    positioned_result(1, stream.fd, empty, 2, INT64_MAX, flags, 0, 0);
    struct iovec bad = {(void*)1, 1};
    positioned_result(
        1, stream.fd, &bad, 1, 0, flags, -1, error ? error : EFAULT);
    next_scalar(&stream);
  }
  struct iovec noncanonical = {(void*)UINTPTR_MAX, 0};
  positioned_result(1, stream.fd, &noncanonical, 1, 0, 0x40000000, -1, EFAULT);
  noncanonical.iov_len = 1;
  positioned_result(1, stream.fd, &noncanonical, 1, 0, 0x40000000, -1, EFAULT);
  positioned_result(1, stream.fd, &one, 1, INT64_MAX, 0x40000000, -1, EINVAL);
  next_scalar(&stream);
  CHECK(close(stream.fd) == 0);
}

static void positioned_faults(const char* path) {
  struct stream stream = {.fd = open(path, O_RDONLY)};
  CHECK(stream.fd >= 0);
  control(path, stream.expected, sizeof(stream.expected));
  size_t page = (size_t)sysconf(_SC_PAGESIZE);
  CHECK(page >= 4096);
  uint8_t* mapping = mmap(
      NULL,
      2 * page,
      PROT_READ | PROT_WRITE,
      MAP_PRIVATE | MAP_ANONYMOUS,
      -1,
      0);
  CHECK(mapping != MAP_FAILED);
  CHECK(mprotect(mapping + page, page, PROT_NONE) == 0);
  for (int mode = 0; mode < 3; ++mode) {
    int current = mode == 2;
    int64_t offset = current ? -1 : 19;
    int version2 = mode != 0;
    uint8_t good[8];
    memset(good, 0xa5, sizeof(good));
    struct iovec iov[] = {{good, 3}, {mapping + page, 5}};
    size_t start = current ? stream.offset : 19;
    positioned_result(version2, stream.fd, iov, 2, offset, 0, 3, 0);
    equal(good, stream.expected + start, 3);
    CHECK(good[3] == 0xa5);
    stream.offset += current ? 3 : 0;
    next_scalar(&stream);

    mapping[page - 2] = mapping[page - 1] = 0xa5;
    iov[0] = (struct iovec){mapping + page - 1, 2};
    start = current ? stream.offset : 19;
    positioned_result(version2, stream.fd, iov, 1, offset, 0, 1, 0);
    equal(mapping + page - 1, stream.expected + start, 1);
    CHECK(mapping[page - 2] == 0xa5);
    stream.offset += current ? 1 : 0;
    next_scalar(&stream);

    memset(good, 0xa5, sizeof(good));
    iov[0] = (struct iovec){good, 3};
    iov[1] = (struct iovec){(void*)UINTPTR_MAX, 1};
    positioned_result(version2, stream.fd, iov, 2, offset, 0, -1, EFAULT);
    for (size_t i = 0; i < sizeof(good); ++i) {
      CHECK(good[i] == 0xa5);
    }
    next_scalar(&stream);

    /* Segment zero overwrites segment one's descriptor before its copyout. */
    iov[0] = (struct iovec){&iov[1], sizeof(iov[1])};
    iov[1] = (struct iovec){good, 3};
    start = current ? stream.offset : 19;
    positioned_result(
        version2, stream.fd, iov, 2, offset, 0, sizeof(iov[1]) + 3, 0);
    equal((const uint8_t*)&iov[1], stream.expected + start, sizeof(iov[1]));
    equal(good, stream.expected + start + sizeof(iov[1]), 3);
    CHECK(good[3] == 0xa5);
    stream.offset += current ? sizeof(iov[1]) + 3 : 0;
    next_scalar(&stream);
  }
  CHECK(munmap(mapping, 2 * page) == 0);
  CHECK(close(stream.fd) == 0);
}

int main(int argc, char** argv) {
  if (argc == 4 && strcmp(argv[1], "--exec-child") == 0) {
    child_read(atoi(argv[2]), atoi(argv[3]));
  }
  const char* selected = "all";
  const char* device = "all";
  for (int i = 1; i < argc; ++i) {
    if (strcmp(argv[i], "--native") == 0) {
      native = 1;
    } else if (strcmp(argv[i], "--case") == 0 && i + 1 < argc) {
      selected = argv[++i];
    } else if (strcmp(argv[i], "--device") == 0 && i + 1 < argc) {
      device = argv[++i];
    } else if (strcmp(argv[i], "--expect-seed") == 0 && i + 1 < argc) {
      ++i;
      CHECK(strcmp(argv[i], "0") == 0 || strcmp(argv[i], "17") == 0);
      expected_seed = atoi(argv[i]);
    } else {
      CHECK(0);
    }
  }
  CHECK(
      strcmp(selected, "all") == 0 || strcmp(selected, "mixed") == 0 ||
      strcmp(selected, "long") == 0 || strcmp(selected, "faults") == 0 ||
      strcmp(selected, "aliases") == 0 || strcmp(selected, "access") == 0 ||
      strcmp(selected, "positioned") == 0);
  CHECK(
      strcmp(device, "all") == 0 || strcmp(device, "random") == 0 ||
      strcmp(device, "urandom") == 0);
  const char* devices[] = {"/dev/random", "/dev/urandom"};
  for (size_t i = 0; i < sizeof(devices) / sizeof(devices[0]); ++i) {
    if (strcmp(device, "all") != 0 && strcmp(device, devices[i] + 5) != 0) {
      continue;
    }
    puts(devices[i]);
    if (strcmp(selected, "all") == 0 || strcmp(selected, "mixed") == 0) {
      mixed_stream(devices[i]);
    }
    if (strcmp(selected, "all") == 0 || strcmp(selected, "long") == 0) {
      long_stream(devices[i]);
    }
    if (strcmp(selected, "all") == 0 || strcmp(selected, "faults") == 0) {
      import_and_faults(devices[i]);
    }
    if (strcmp(selected, "all") == 0 || strcmp(selected, "access") == 0) {
      access_modes(devices[i]);
    }
    if (strcmp(selected, "all") == 0 || strcmp(selected, "positioned") == 0) {
      positioned_stream(devices[i]);
      positioned_errors(devices[i]);
      positioned_faults(devices[i]);
    }
    if (strcmp(selected, "all") == 0 || strcmp(selected, "aliases") == 0) {
      inherited_stream(devices[i], argv[0], 0);
      inherited_stream(devices[i], argv[0], 1);
    }
  }
  puts(native ? "random-readv-native-semantics ok" : "random-readv-stream ok");
  return 0;
}
