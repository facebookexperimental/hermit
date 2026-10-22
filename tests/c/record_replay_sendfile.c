/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// Copies with sendfile, falling back to read and write when sendfile is
// refused, as cp and glibc's callers do
// (https://github.com/rrnewton/hermit/issues/4416). Each mode prints which
// path copied the bytes ("sendfile" or "read") and the positions it left.
//
// Usage: record_replay_sendfile MODE ...
//   position FILE OUT  FILE must hold "abcdef". Seek it to 2, copy 2 bytes
//                      into OUT (created) at its position, then print both
//                      positions, OUT's size and contents, and the next 2
//                      bytes read from FILE.
//   made FILE OUT      As position, from a FILE the guest creates and writes
//                      "abcdef" into first, so replay holds a copy of it.
//   offset FILE OUT    Copy 3 bytes from FILE at offset 1 into OUT, then print
//                      the offset the call left and FILE's position (still 0).
//   stdout FILE        Copy 3 bytes from FILE at its position to stdout.
//   pipe OUT           Copy 3 bytes a pipe holds into OUT.
//   ro-offset FILE     Copy 3 bytes from FILE at offset 1 to stdout through an
//                      offset argument on a read-only page, then print the
//                      result, errno and the offset. Linux copies and then
//                      fails with EFAULT writing the offset back, so stdout
//                      gets the bytes and the offset stays 1. The input is
//                      at position 2 first, which the call leaves alone.
//   ro-shared FILE     Create FILE holding "abcdef" and copy its first 2
//                      bytes to position 4 through one O_RDWR descriptor
//                      used as input and output, with a read-only offset 0:
//                      Linux leaves "abcdab", position 6 and EFAULT.
//   xfsz FILE          FILE, sparse and 10 bytes short of the file-size limit
//                      Hermit runs under, is copied onto its own end through
//                      one O_RDWR descriptor from read-only offset size - 2,
//                      count 100: Linux keeps reading what it has just written,
//                      stops at the limit after 10 bytes with one SIGXFSZ,
//                      which a handler counts, and returns EFAULT.
//   tight-xfsz FILE    FILE, sparse and 1 byte short of the file-size limit,
//                      gets 3 bytes from read-only offset 8 copied onto its
//                      end through one O_RDWR descriptor, on the tight stack:
//                      Linux writes the 1 byte to the limit, sends SIGXFSZ
//                      (counted by a handler on an alternate stack) and
//                      returns EFAULT.
//   eof-tight FILE     Create FILE holding "01234567abcdefgh" and copy from
//                      read-only offset 8 to position 0, count 1 GiB, through
//                      one O_RDWR descriptor on the tight stack: the input
//                      ends after 8 bytes, so Linux copies 8, sends no
//                      SIGXFSZ, and returns EFAULT.
//   ro-dup FILE        ro-shared with the input a dup of the output: two
//                      descriptor numbers, one description.
//   ro-two-opens FILE  Create FILE holding "abcdef", open it again read-only
//                      as input and write-only as output (two descriptions
//                      of one file), and copy 2 bytes from read-only offset
//                      0 to position 4: Linux leaves "abcdab", position 6,
//                      EFAULT.
//   ro-distinct IN OUT Create IN holding "abcdefghijklmnop" and OUT holding
//                      "0123456789ABCDEF", and copy 3 bytes from read-only
//                      offset 1 of IN to position 0 of OUT: Linux leaves
//                      "bcd3456789ABCDEF", position 3, and EFAULT.
//   append-xfsz FILE   As tight-xfsz, with FILE at the limit and opened
//                      O_APPEND: Linux refuses an O_APPEND output (EINVAL)
//                      before writing, so no SIGXFSZ, and returns EFAULT.
//   tight-shared FILE  Create FILE holding "01234567abcdefgh" and copy 3
//                      bytes from read-only offset 8 to position 0 through
//                      one O_RDWR descriptor, on the tight stack of
//                      tight-offset: Linux leaves "abc34567abcdefgh",
//                      position 3, offset 8 and EFAULT.
//   none-offset FILE   The same with the offset on a PROT_NONE page: Linux
//                      cannot read it and fails with EFAULT before copying.
//   tight-offset FILE  ro-offset, with the call made on a stack whose pointer
//                      is 64 bytes above an unmapped page.
//   copyfile SRC DST   Copy SRC into DST as Python's shutil.copyfile does:
//                      DST opened write-only, sendfile in 4-byte steps until
//                      it returns 0, then print what DST holds.
//   overlap-back FILE  Create FILE holding "12ZQWK", then copy 4 bytes from
//                      offset 2 to offset 1 through a second, write-only
//                      descriptor: the copy overwrites its own input. Linux leaves "1ZQWKK",
//                      so the copy wrote "ZQWK".
//   overlap-fwd FILE   The same from offset 0 to offset 2. Linux leaves
//                      "121212", so the copy wrote "1212".
// The overlap modes print no file contents, so the copied bytes reach the
// recording only through the sendfile event, and FILE's contents are built at
// run time, so they are not in this program's image either.
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/sendfile.h>
#include <sys/syscall.h>
#include <unistd.h>

// sendfile(out, in, offset, count), or read and write when it is refused.
static ssize_t copy(int out, int in, off_t* offset, size_t count, const char** path) {
  *path = "sendfile";
  ssize_t copied = sendfile(out, in, offset, count);
  if (copied >= 0 || (errno != ENOSYS && errno != EINVAL)) {
    return copied;
  }
  char buf[16];
  *path = "read";
  copied = offset ? pread(in, buf, count, *offset) : read(in, buf, count);
  if (copied > 0 && write(out, buf, (size_t)copied) != copied) {
    return -1;
  }
  if (copied > 0 && offset) {
    *offset += copied;
  }
  return copied;
}

// sendfile(out, in, offset, count) made with the stack pointer at `sp`.
static long sendfile_on_stack(long out, long in, void* offset, long count, char* sp) {
  long result;
  register long r10 __asm__("r10") = count;
  __asm__ volatile(
      "mov %%rsp, %%r12\n\t"
      "mov %[sp], %%rsp\n\t"
      "syscall\n\t"
      "mov %%r12, %%rsp\n\t"
      : "=a"(result)
      : "a"((long)SYS_sendfile), "D"(out), "S"(in), "d"(offset), "r"(r10), [sp] "r"(sp)
      : "rcx", "r11", "r12", "memory");
  return result;
}

static volatile sig_atomic_t xfsz_count;
static volatile sig_atomic_t xfsz_code;
static volatile sig_atomic_t xfsz_from_self;

static void on_xfsz(int signal, siginfo_t* info, void* context) {
  (void)signal;
  (void)context;
  xfsz_count++;
  xfsz_code = info->si_code;
  xfsz_from_self = info->si_pid == getpid();
}

static int open_out(const char* name) {
  return open(name, O_RDWR | O_CREAT | O_TRUNC, 0644);
}

static void print_out(int out) {
  char held[16] = {0};
  if (pread(out, held, sizeof(held) - 1, 0) < 0) {
    perror("pread");
  }
  printf(" out \"%s\"\n", held);
}

// Creates FILE holding "12ZQWK" and copies 4 bytes from offset `from` to
// offset `to` of it through two descriptors.
static int overlap(const char* mode, const char* name, off_t from, off_t to) {
  static const int text[] = {'1', '2', 'Z', 'Q', 'W', 'K'};
  char bytes[6];
  for (int i = 0; i < 6; i++) {
    bytes[i] = (char)text[i];
  }
  int in = open_out(name);
  int out = open(name, O_WRONLY);
  if (in < 0 || out < 0 || write(in, bytes, 6) != 6 || lseek(in, from, SEEK_SET) != from ||
      lseek(out, to, SEEK_SET) != to) {
    perror("setup");
    return 1;
  }
  const char* path;
  ssize_t copied = copy(out, in, NULL, 4, &path);
  printf(
      "%s via %s copied %zd in at %ld out at %ld\n",
      mode,
      path,
      copied,
      (long)lseek(in, 0, SEEK_CUR),
      (long)lseek(out, 0, SEEK_CUR));
  return 0;
}

int main(int argc, char** argv) {
  const char* mode = argc >= 2 ? argv[1] : "";
  const char* path;
  setvbuf(stdout, NULL, _IONBF, 0);
  int made = !strcmp(mode, "made");
  if ((made || !strcmp(mode, "position")) && argc == 4) {
    int in = made ? open_out(argv[2]) : open(argv[2], O_RDONLY);
    int out = open_out(argv[3]);
    if (in < 0 || out < 0 || (made && write(in, "abcdef", 6) != 6) || lseek(in, 2, SEEK_SET) != 2) {
      perror("setup");
      return 1;
    }
    ssize_t copied = copy(out, in, NULL, 2, &path);
    long in_at = lseek(in, 0, SEEK_CUR);
    long out_at = lseek(out, 0, SEEK_CUR);
    long out_end = lseek(out, 0, SEEK_END);
    char next[3] = {0};
    if (read(in, next, 2) < 0) {
      perror("read");
      return 1;
    }
    printf(
        "%s via %s copied %zd in at %ld out at %ld size %ld next \"%s\"",
        mode,
        path,
        copied,
        in_at,
        out_at,
        out_end,
        next);
    print_out(out);
    return 0;
  }
  if (!strcmp(mode, "offset") && argc == 4) {
    int in = open(argv[2], O_RDONLY);
    int out = open_out(argv[3]);
    if (in < 0 || out < 0) {
      perror("setup");
      return 1;
    }
    off_t offset = 1;
    ssize_t copied = copy(out, in, &offset, 3, &path);
    printf(
        "offset via %s copied %zd offset %ld in at %ld",
        path,
        copied,
        (long)offset,
        (long)lseek(in, 0, SEEK_CUR));
    print_out(out);
    return 0;
  }
  if (!strcmp(mode, "stdout") && argc == 3) {
    int in = open(argv[2], O_RDONLY);
    if (in < 0) {
      perror("setup");
      return 1;
    }
    printf("stdout [");
    ssize_t copied = copy(STDOUT_FILENO, in, NULL, 3, &path);
    printf("] via %s copied %zd\n", path, copied);
    return 0;
  }
  if (!strcmp(mode, "pipe") && argc == 3) {
    int fds[2];
    int out = open_out(argv[2]);
    if (out < 0 || pipe(fds) != 0 || write(fds[1], "xyz", 3) != 3) {
      perror("setup");
      return 1;
    }
    ssize_t copied = copy(out, fds[0], NULL, 3, &path);
    printf("pipe via %s copied %zd", path, copied);
    print_out(out);
    return 0;
  }
  if ((!strcmp(mode, "none-offset") || !strcmp(mode, "tight-offset")) && argc == 3) {
    int none = !strcmp(mode, "none-offset");
    int in = open(argv[2], O_RDONLY);
    char* pages = mmap(NULL, 3 * 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (in < 0 || pages == MAP_FAILED || lseek(in, 2, SEEK_SET) != 2) {
      perror("setup");
      return 1;
    }
    off_t* offset = (off_t*)(pages + 2 * 4096);
    *offset = 1;
    // The stack is the middle page, its pointer 64 bytes above the first,
    // which is unmapped.
    if (mprotect(offset, 4096, none ? PROT_NONE : PROT_READ) != 0 || munmap(pages, 4096) != 0) {
      perror("mprotect");
      return 1;
    }
    printf("%s [", mode);
    long copied = none ? sendfile(STDOUT_FILENO, in, offset, 3)
                       : sendfile_on_stack(STDOUT_FILENO, in, offset, 3, pages + 4096 + 64);
    int error = none ? errno : (copied < 0 ? (int)-copied : 0);
    if (!none && copied < 0) {
      copied = -1;
    }
    if (none) {
      printf("] copied %ld %s pos %ld\n", copied, copied < 0 && error == EFAULT ? "EFAULT" : "-", (long)lseek(in, 0, SEEK_CUR));
    } else {
      printf(
          "] copied %ld %s offset %ld pos %ld\n",
          copied,
          copied < 0 && error == EFAULT ? "EFAULT" : "-",
          (long)*offset,
          (long)lseek(in, 0, SEEK_CUR));
    }
    return 0;
  }
  if (!strcmp(mode, "ro-offset") && argc == 3) {
    int in = open(argv[2], O_RDONLY);
    off_t* offset = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (in < 0 || offset == MAP_FAILED || lseek(in, 2, SEEK_SET) != 2) {
      perror("setup");
      return 1;
    }
    *offset = 1;
    if (mprotect(offset, 4096, PROT_READ) != 0) {
      perror("mprotect");
      return 1;
    }
    printf("ro-offset [");
    ssize_t copied = sendfile(STDOUT_FILENO, in, offset, 3);
    int error = errno;
    printf(
        "] copied %zd %s offset %ld pos %ld\n",
        copied,
        copied < 0 && error == EFAULT ? "EFAULT" : "-",
        (long)*offset,
        (long)lseek(in, 0, SEEK_CUR));
    return 0;
  }
  if ((!strcmp(mode, "ro-shared") || !strcmp(mode, "ro-dup")) && argc == 3) {
    int fd = open_out(argv[2]);
    off_t* offset = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (fd < 0 || offset == MAP_FAILED || write(fd, "abcdef", 6) != 6 || lseek(fd, 4, SEEK_SET) != 4) {
      perror("setup");
      return 1;
    }
    *offset = 0;
    if (mprotect(offset, 4096, PROT_READ) != 0) {
      perror("mprotect");
      return 1;
    }
    // ro-dup: the input is another descriptor for the same description.
    int in = !strcmp(mode, "ro-dup") ? dup(fd) : fd;
    ssize_t copied = in < 0 ? -1 : sendfile(fd, in, offset, 2);
    int error = errno;
    char held[8] = {0};
    if (pread(fd, held, 6, 0) != 6) {
      perror("pread");
      return 1;
    }
    printf(
        "%s copied %zd %s pos %ld offset %ld file \"%s\"\n",
        mode,
        copied,
        copied < 0 && error == EFAULT ? "EFAULT" : "-",
        (long)lseek(fd, 0, SEEK_CUR),
        (long)*offset,
        held);
    return 0;
  }
  if (!strcmp(mode, "tight-shared") && argc == 3) {
    int fd = open_out(argv[2]);
    char* pages = mmap(NULL, 3 * 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (fd < 0 || pages == MAP_FAILED || write(fd, "01234567abcdefgh", 16) != 16 || lseek(fd, 0, SEEK_SET) != 0) {
      perror("setup");
      return 1;
    }
    off_t* offset = (off_t*)(pages + 2 * 4096);
    *offset = 8;
    if (mprotect(offset, 4096, PROT_READ) != 0 || munmap(pages, 4096) != 0) {
      perror("mprotect");
      return 1;
    }
    long copied = sendfile_on_stack(fd, fd, offset, 3, pages + 4096 + 64);
    char held[17] = {0};
    if (pread(fd, held, 16, 0) != 16) {
      perror("pread");
      return 1;
    }
    printf(
        "tight-shared copied %ld %s pos %ld offset %ld file \"%s\"\n",
        copied < 0 ? -1L : copied,
        copied == -EFAULT ? "EFAULT" : "-",
        (long)lseek(fd, 0, SEEK_CUR),
        (long)*offset,
        held);
    return 0;
  }
  if (!strcmp(mode, "ro-two-opens") && argc == 3) {
    int made = open_out(argv[2]);
    int in = open(argv[2], O_RDONLY);
    int out = open(argv[2], O_WRONLY);
    off_t* offset = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (made < 0 || in < 0 || out < 0 || offset == MAP_FAILED || write(made, "abcdef", 6) != 6 ||
        lseek(out, 4, SEEK_SET) != 4) {
      perror("setup");
      return 1;
    }
    *offset = 0;
    if (mprotect(offset, 4096, PROT_READ) != 0) {
      perror("mprotect");
      return 1;
    }
    ssize_t copied = sendfile(out, in, offset, 2);
    int error = errno;
    char held[8] = {0};
    if (pread(made, held, 6, 0) != 6) {
      perror("pread");
      return 1;
    }
    printf(
        "ro-two-opens copied %zd %s pos %ld offset %ld file \"%s\"\n",
        copied,
        copied < 0 && error == EFAULT ? "EFAULT" : "-",
        (long)lseek(out, 0, SEEK_CUR),
        (long)*offset,
        held);
    return 0;
  }
  if (!strcmp(mode, "ro-distinct") && argc == 4) {
    int in = open_out(argv[2]);
    int out = open_out(argv[3]);
    off_t* offset = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (in < 0 || out < 0 || offset == MAP_FAILED || write(in, "abcdefghijklmnop", 16) != 16 ||
        write(out, "0123456789ABCDEF", 16) != 16 || lseek(out, 0, SEEK_SET) != 0) {
      perror("setup");
      return 1;
    }
    *offset = 1;
    if (mprotect(offset, 4096, PROT_READ) != 0) {
      perror("mprotect");
      return 1;
    }
    ssize_t copied = sendfile(out, in, offset, 3);
    int error = errno;
    char held[17] = {0};
    if (pread(out, held, 16, 0) != 16) {
      perror("pread");
      return 1;
    }
    printf(
        "ro-distinct copied %zd %s pos %ld offset %ld out \"%s\"\n",
        copied,
        copied < 0 && error == EFAULT ? "EFAULT" : "-",
        (long)lseek(out, 0, SEEK_CUR),
        (long)*offset,
        held);
    return 0;
  }
  if (!strcmp(mode, "eof-tight") && argc == 3) {
    static char altstack[64 * 1024];
    stack_t alt = {.ss_sp = altstack, .ss_size = sizeof(altstack)};
    struct sigaction action = {0};
    action.sa_sigaction = on_xfsz;
    action.sa_flags = SA_ONSTACK | SA_SIGINFO;
    int fd = open_out(argv[2]);
    char* pages = mmap(NULL, 3 * 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (sigaltstack(&alt, NULL) != 0 || sigaction(SIGXFSZ, &action, NULL) != 0 || fd < 0 ||
        pages == MAP_FAILED || write(fd, "01234567abcdefgh", 16) != 16 || lseek(fd, 0, SEEK_SET) != 0) {
      perror("setup");
      return 1;
    }
    off_t* offset = (off_t*)(pages + 2 * 4096);
    *offset = 8;
    if (mprotect(offset, 4096, PROT_READ) != 0 || munmap(pages, 4096) != 0) {
      perror("mprotect");
      return 1;
    }
    long copied = sendfile_on_stack(fd, fd, offset, 1L << 30, pages + 4096 + 64);
    char held[17] = {0};
    if (pread(fd, held, 16, 0) != 16) {
      perror("pread");
      return 1;
    }
    printf(
        "eof-tight handler %d copied %ld %s pos %ld offset %ld file \"%s\"\n",
        (int)xfsz_count,
        copied < 0 ? -1L : copied,
        copied == -EFAULT ? "EFAULT" : "-",
        (long)lseek(fd, 0, SEEK_CUR),
        (long)*offset,
        held);
    return 0;
  }
  if ((!strcmp(mode, "tight-xfsz") || !strcmp(mode, "append-xfsz")) && argc == 3) {
    int append = !strcmp(mode, "append-xfsz");
    static char altstack[64 * 1024];
    stack_t alt = {.ss_sp = altstack, .ss_size = sizeof(altstack)};
    struct sigaction action = {0};
    action.sa_sigaction = on_xfsz;
    action.sa_flags = SA_ONSTACK | SA_SIGINFO;
    int fd = open(argv[2], append ? O_RDWR | O_APPEND : O_RDWR);
    char* pages = mmap(NULL, 3 * 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    off_t size = fd < 0 ? -1 : lseek(fd, 0, SEEK_END);
    if (sigaltstack(&alt, NULL) != 0 || sigaction(SIGXFSZ, &action, NULL) != 0 || size < 16 ||
        pages == MAP_FAILED) {
      perror("setup");
      return 1;
    }
    off_t* offset = (off_t*)(pages + 2 * 4096);
    *offset = 8;
    if (mprotect(offset, 4096, PROT_READ) != 0 || munmap(pages, 4096) != 0) {
      perror("mprotect");
      return 1;
    }
    long copied = sendfile_on_stack(fd, fd, offset, 3, pages + 4096 + 64);
    printf(
        "%s handler %d si_code %d si_pid %s copied %ld %s grew %ld\n",
        mode,
        (int)xfsz_count,
        (int)xfsz_code,
        xfsz_from_self ? "self" : "other",
        copied < 0 ? -1L : copied,
        copied == -EFAULT ? "EFAULT" : "-",
        (long)(lseek(fd, 0, SEEK_CUR) - size));
    return 0;
  }
  if (!strcmp(mode, "xfsz") && argc == 3) {
    struct sigaction action = {0};
    action.sa_sigaction = on_xfsz;
    action.sa_flags = SA_SIGINFO;
    int fd = open(argv[2], O_RDWR);
    off_t* offset = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    off_t size = fd < 0 ? -1 : lseek(fd, 0, SEEK_END);
    if (sigaction(SIGXFSZ, &action, NULL) != 0 || size < 2 || offset == MAP_FAILED) {
      perror("setup");
      return 1;
    }
    *offset = size - 2;
    if (mprotect(offset, 4096, PROT_READ) != 0) {
      perror("mprotect");
      return 1;
    }
    ssize_t copied = sendfile(fd, fd, offset, 100);
    int error = errno;
    printf(
        "xfsz handler %d si_code %d si_pid %s copied %zd %s grew %ld\n",
        (int)xfsz_count,
        (int)xfsz_code,
        xfsz_from_self ? "self" : "other",
        copied,
        copied < 0 && error == EFAULT ? "EFAULT" : "-",
        (long)(lseek(fd, 0, SEEK_CUR) - size));
    return 0;
  }
  if (!strcmp(mode, "copyfile") && argc == 4) {
    int in = open(argv[2], O_RDONLY);
    int out = open(argv[3], O_WRONLY | O_CREAT | O_TRUNC, 0644);
    if (in < 0 || out < 0) {
      perror("setup");
      return 1;
    }
    ssize_t copied, total = 0;
    while ((copied = sendfile(out, in, NULL, 4)) > 0) {
      total += copied;
    }
    if (copied < 0) {
      printf("copyfile failed: %s\n", strerror(errno));
      return 0;
    }
    char held[16] = {0};
    int check = open(argv[3], O_RDONLY);
    if (check < 0 || read(check, held, sizeof(held) - 1) < 0) {
      perror("check");
      return 1;
    }
    printf("copyfile copied %zd \"%s\"\n", total, held);
    return 0;
  }
  if (!strcmp(mode, "overlap-back") && argc == 3) {
    return overlap(mode, argv[2], 2, 1);
  }
  if (!strcmp(mode, "overlap-fwd") && argc == 3) {
    return overlap(mode, argv[2], 0, 2);
  }
  fprintf(
      stderr,
      "usage: %s position|made|offset FILE OUT | copyfile SRC DST | ro-distinct IN OUT | stdout|ro-offset|ro-shared|ro-dup|ro-two-opens|none-offset|tight-offset|tight-shared|xfsz|tight-xfsz|append-xfsz|eof-tight FILE | pipe OUT | overlap-back|overlap-fwd FILE\n",
      argv[0]);
  return 64;
}
