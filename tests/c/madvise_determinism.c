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
#include <limits.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <unistd.h>

#ifndef MADV_FREE
#define MADV_FREE 8
#endif
#ifndef MADV_POPULATE_READ
#define MADV_POPULATE_READ 22
#endif

static int
expect_errno(void* address, size_t length, int advice, int expected) {
  errno = 0;
  if (madvise(address, length, advice) != -1 || errno != expected) {
    fprintf(
        stderr,
        "madvise(%d) expected errno %d, got %d\n",
        advice,
        expected,
        errno);
    return 1;
  }
  return 0;
}

/* A dropped shared file page reads back what was written through the
 * descriptor after the mapping was made, which replay's anonymous stand-in
 * never saw. Closes `fd`. */
static int check_shared_coherence(int fd, size_t page_size) {
  if (fd < 0 || pwrite(fd, "A", 1, 0) != 1) {
    return 1;
  }
  unsigned char* shared = mmap(NULL, page_size, PROT_READ, MAP_SHARED, fd, 0);
  if (shared == MAP_FAILED || pwrite(fd, "B", 1, 0) != 1 ||
      madvise(shared, page_size, MADV_DONTNEED) != 0 || shared[0] != 'B') {
    fprintf(stderr, "shared MADV_DONTNEED read %c, not B\n",
            shared == MAP_FAILED ? '?' : shared[0]);
    return 2;
  }
  if (munmap(shared, page_size) != 0 || close(fd) != 0) {
    return 3;
  }
  return 0;
}

/* Guest-semantic advice whose effects depend on the backing store. Record and
 * replay must agree with a native run: replay stands in anonymous memory for
 * file mappings. */
static int check_semantic_advice(
    const char* path,
    unsigned char* anonymous,
    size_t page_size) {
  int fd = open(path, O_RDONLY);
  if (fd < 0) {
    return 1;
  }

  /* Dropped anonymous pages read back as zeros. */
  anonymous[0] = 0x5a;
  if (madvise(anonymous, page_size, MADV_DONTNEED) != 0 || anonymous[0] != 0) {
    fprintf(stderr, "anonymous MADV_DONTNEED kept %d\n", anonymous[0]);
    return 2;
  }

  /* Dropped private file pages read back as the file contents. */
  unsigned char* file_mapping =
      mmap(NULL, page_size, PROT_READ | PROT_WRITE, MAP_PRIVATE, fd, 0);
  if (file_mapping == MAP_FAILED) {
    return 3;
  }
  const unsigned char original = file_mapping[1];
  file_mapping[1] = original ^ 0xff;
  if (madvise(file_mapping, page_size, MADV_DONTNEED) != 0 ||
      file_mapping[1] != original) {
    fprintf(stderr, "file MADV_DONTNEED read %d, not %d\n", file_mapping[1],
            original);
    return 4;
  }

  /* The same for a read-only file mapping. */
  unsigned char* read_only = mmap(NULL, page_size, PROT_READ, MAP_PRIVATE, fd, 0);
  if (read_only == MAP_FAILED) {
    return 5;
  }
  if (madvise(read_only, page_size, MADV_DONTNEED) != 0 ||
      read_only[1] != original) {
    return 6;
  }

  /* A shared file mapping keeps its contents too. */
  unsigned char* shared = mmap(NULL, page_size, PROT_READ, MAP_SHARED, fd, 0);
  if (shared == MAP_FAILED || madvise(shared, page_size, MADV_DONTNEED) != 0 ||
      shared[1] != original || munmap(shared, page_size) != 0) {
    return 13;
  }

  /* The same after a write through the descriptor, for a memfd and for a
   * regular file. */
  if (check_shared_coherence(memfd_create("madvise_determinism", 0), page_size)) {
    return 14;
  }
  char temp_path[] = "/tmp/madvise_determinism.XXXXXX";
  int temp = mkstemp(temp_path);
  if (temp < 0 || unlink(temp_path) != 0 ||
      check_shared_coherence(temp, page_size)) {
    return 15;
  }

  /* Dropping a page of this program's own text, which replay maps from the
   * real executable, leaves it intact. */
  unsigned char* text = (unsigned char*)((uintptr_t)&check_semantic_advice &
                                         ~(uintptr_t)(page_size - 1));
  if (madvise(text, page_size, MADV_DONTNEED) != 0) {
    return 12;
  }

  /* MADV_WIPEONFORK applies to the anonymous page, then fails at the file
   * page that follows it. A child sees the wiped anonymous page and the
   * file page's contents. */
  unsigned char* mixed = mmap(
      NULL, 2 * page_size, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS,
      -1, 0);
  if (mixed == MAP_FAILED ||
      mmap(mixed + page_size, page_size, PROT_READ | PROT_WRITE,
           MAP_PRIVATE | MAP_FIXED, fd, 0) == MAP_FAILED) {
    return 7;
  }
  mixed[0] = 0x33;
  if (expect_errno(mixed, 2 * page_size, MADV_WIPEONFORK, EINVAL)) {
    return 8;
  }
  pid_t child = fork();
  if (child < 0) {
    return 9;
  }
  if (child == 0) {
    _exit(mixed[0] == 0 && mixed[page_size + 1] == original ? 0 : 1);
  }
  int status = 0;
  if (waitpid(child, &status, 0) != child || !WIFEXITED(status) ||
      WEXITSTATUS(status) != 0 || mixed[0] != 0x33) {
    fprintf(stderr, "MADV_WIPEONFORK child status %d\n", status);
    return 10;
  }

  if (munmap(mixed, 2 * page_size) != 0 ||
      munmap(read_only, page_size) != 0 ||
      munmap(file_mapping, page_size) != 0 || close(fd) != 0) {
    return 11;
  }
  return 0;
}

/* KVM implements MADV_DONTNEED for anonymous memory: private pages read back
 * as zeros and shared ones keep their contents. It refuses the advice with
 * ENOSYS, changing nothing, for a private file copy it cannot read again, and
 * still refuses the other guest-visible advice. */
static int check_kvm_advice(
    const char* path,
    unsigned char* anonymous,
    size_t page_size) {
  anonymous[0] = 0x5a;
  if (madvise(anonymous, page_size, MADV_DONTNEED) != 0 || anonymous[0] != 0) {
    fprintf(stderr, "KVM anonymous MADV_DONTNEED kept %d\n", anonymous[0]);
    return 1;
  }
  unsigned char* shared = mmap(
      NULL,
      page_size,
      PROT_READ | PROT_WRITE,
      MAP_SHARED | MAP_ANONYMOUS,
      -1,
      0);
  if (shared == MAP_FAILED) {
    return 2;
  }
  shared[0] = 0x6b;
  if (madvise(shared, page_size, MADV_DONTNEED) != 0 || shared[0] != 0x6b) {
    fprintf(stderr, "KVM shared anonymous MADV_DONTNEED read %d\n", shared[0]);
    return 3;
  }
  int fd = open(path, O_RDONLY);
  if (fd < 0) {
    return 4;
  }
  unsigned char* file_mapping =
      mmap(NULL, page_size, PROT_READ | PROT_WRITE, MAP_PRIVATE, fd, 0);
  if (file_mapping == MAP_FAILED) {
    return 5;
  }
  const unsigned char changed = file_mapping[1] ^ 0xff;
  file_mapping[1] = changed;
  if (expect_errno(file_mapping, page_size, MADV_DONTNEED, ENOSYS) ||
      file_mapping[1] != changed) {
    return 6;
  }
  if (expect_errno(anonymous, page_size, MADV_DONTFORK, ENOSYS)) {
    return 7;
  }
  if (munmap(file_mapping, page_size) != 0 || munmap(shared, page_size) != 0 ||
      close(fd) != 0) {
    return 8;
  }
  return 0;
}

int main(int argc, char** argv) {
  const bool kvm = argc == 2 && strcmp(argv[1], "--kvm") == 0;
  const long page_size_raw = sysconf(_SC_PAGESIZE);
  if (page_size_raw <= 0) {
    return 10;
  }
  const size_t page_size = (size_t)page_size_raw;

  unsigned char* anonymous = mmap(
      NULL,
      page_size,
      PROT_READ | PROT_WRITE,
      MAP_PRIVATE | MAP_ANONYMOUS,
      -1,
      0);
  if (anonymous == MAP_FAILED) {
    return 11;
  }
  anonymous[0] = 0x5a;

  if (madvise(anonymous, page_size, MADV_WILLNEED) != 0 ||
      madvise(anonymous, page_size, MADV_FREE) != 0 || anonymous[0] != 0x5a) {
    return 12;
  }
  if (expect_errno(anonymous, page_size, MADV_POPULATE_READ, EINVAL) ||
      expect_errno(anonymous, page_size, INT_MAX, EINVAL) ||
      expect_errno(anonymous + 1, 0, MADV_FREE, EINVAL)) {
    return 13;
  }

  if (kvm) {
    if (check_kvm_advice(argv[0], anonymous, page_size)) {
      return 14;
    }
  } else if (check_semantic_advice(argv[0], anonymous, page_size)) {
    return 17;
  }

  if (munmap(anonymous, page_size) != 0) {
    return 19;
  }
  puts("madvise-ok");
  return 0;
}
