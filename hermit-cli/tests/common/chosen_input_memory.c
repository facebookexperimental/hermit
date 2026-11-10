/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/* A real clone3 input can live below the syscall's stack pointer. The former
 * Chosen rewrite used [SP-344, SP-128) without preserving its 216 bytes: an
 * 88-byte clone_args followed by a 128-byte set_tid array. Keep that exact
 * placement independent of the C compiler's own stack layout.
 *
 * The child returns through the saved ordinary stack after a private-MM clone.
 * Both processes inspect the full old footprint before doing any further
 * clone. The output mode also puts parent_tid inside clone_args.cgroup and
 * child_tid/pidfd in the old array area. Restoring all 216 bytes after a
 * successful clone would erase valid kernel outputs and must fail this oracle.
 *
 * The fallback mode uses the same real capability distinction as the existing
 * chosen_guest_ids no-caps oracle: creating a new user/PID namespace with its
 * own set_tid [1] is allowed after dropping outer capabilities; adding an ID
 * in the existing outer PID namespace gets EPERM. No test-only product hook,
 * seccomp filter, or broad errno acceptance is involved.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <linux/capability.h>
#include <linux/sched.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>
#include <fcntl.h>

#define STRINGIFY_INNER(value) #value
#define STRINGIFY(value) STRINGIFY_INNER(value)

enum { OLD_ARGS_BYTES = 88, OLD_ARRAY_BYTES = 128, OLD_BYTES = 216 };
_Static_assert(sizeof(struct clone_args) == OLD_ARGS_BYTES,
               "the fixture exercises the 88-byte clone3 ABI");
_Static_assert(sizeof(pid_t) == 4, "clone3 set_tid elements are four bytes");

extern long clone_from_old_footprint(void* sp, struct clone_args* args,
                                     size_t size);

#if defined(__x86_64__)
__asm__(
    ".text\n"
    ".globl clone_from_old_footprint\n"
    ".type clone_from_old_footprint,@function\n"
    "clone_from_old_footprint:\n"
    "push %r12\n"
    "mov %rsp,%r12\n"
    "mov %rdi,%rsp\n"
    "mov %rsi,%rdi\n"
    "mov %rdx,%rsi\n"
    "mov $" STRINGIFY(SYS_clone3) ",%eax\n"
    "syscall\n"
    "mov %r12,%rsp\n"
    "pop %r12\n"
    "ret\n"
    ".size clone_from_old_footprint,.-clone_from_old_footprint\n");
#elif defined(__aarch64__)
__asm__(
    ".text\n"
    ".globl clone_from_old_footprint\n"
    ".type clone_from_old_footprint,%function\n"
    "clone_from_old_footprint:\n"
    "stp x19,x30,[sp,#-16]!\n"
    "mov x19,sp\n"
    "mov sp,x0\n"
    "mov x0,x1\n"
    "mov x1,x2\n"
    "mov x8,#" STRINGIFY(SYS_clone3) "\n"
    "svc #0\n"
    "mov sp,x19\n"
    "ldp x19,x30,[sp],#16\n"
    "ret\n"
    ".size clone_from_old_footprint,.-clone_from_old_footprint\n");
#else
#error "the explicit syscall stack fixture needs an implementation for this ABI"
#endif

static int require_image(const unsigned char* image,
                         const unsigned char* expected, const char* who) {
  for (size_t i = 0; i < OLD_BYTES; i++) {
    if (image[i] != expected[i]) {
      fprintf(stderr, "%s old stack byte %zu: got %02x, expected %02x\n",
              who, i, image[i], expected[i]);
      return 1;
    }
  }
  return 0;
}

static void expect_word(unsigned char* expected, const unsigned char* image,
                        const int* address, int value) {
  size_t offset = (const unsigned char*)address - image;
  if (offset > OLD_BYTES - sizeof value) {
    fprintf(stderr, "output word escaped the old footprint\n");
    exit(2);
  }
  memcpy(expected + offset, &value, sizeof value);
}

static int read_word(const int* address) {
  int value;
  memcpy(&value, address, sizeof value);
  return value;
}

int main(int argc, char** argv) {
  if (argc != 2) return 2;
  int fallback = strcmp(argv[1], "fallback-input") == 0 ||
                 strcmp(argv[1], "fallback-outputs") == 0;
  int outputs = strcmp(argv[1], "outputs") == 0 ||
                strcmp(argv[1], "fallback-outputs") == 0;
  if (!fallback && !outputs && strcmp(argv[1], "input") != 0) return 2;

  long page = sysconf(_SC_PAGESIZE);
  if (page < 1024) return 2;
  unsigned char* mapping = mmap(NULL, (size_t)page * 4,
                                PROT_READ | PROT_WRITE,
                                MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  if (mapping == MAP_FAILED) {
    perror("mmap");
    return 2;
  }
  unsigned char* syscall_sp = mapping + 2 * page;
  unsigned char* image = syscall_sp - 128 - OLD_BYTES;
  struct clone_args* args = (struct clone_args*)image;
  unsigned char* array = image + OLD_ARGS_BYTES;
  memset(image, 0xa5, OLD_BYTES);
  memset(args, 0, sizeof *args);
  args->exit_signal = SIGCHLD;

  /* Aliasing an inactive input field with a kernel output is legal: clone3
   * copies its input before writing parent_tid. Keep cgroup initially zero. */
  int* parent_tid = (int*)&args->cgroup;
  int* child_tid = (int*)(array + 16);
  int* pidfd = (int*)(array + 32);
  if (outputs) {
    *child_tid = -11;
    *pidfd = -13;
    args->flags |= CLONE_PARENT_SETTID | CLONE_CHILD_SETTID | CLONE_PIDFD;
    args->parent_tid = (uint64_t)(uintptr_t)parent_tid;
    args->child_tid = (uint64_t)(uintptr_t)child_tid;
    args->pidfd = (uint64_t)(uintptr_t)pidfd;
  }
  if (fallback) {
    struct __user_cap_header_struct header = {_LINUX_CAPABILITY_VERSION_3, 0};
    struct __user_cap_data_struct none[2] = {{0}, {0}};
    if (syscall(SYS_capset, &header, none) != 0) {
      perror("capset");
      return 2;
    }
    *(pid_t*)array = 1;
    args->flags |= CLONE_NEWUSER | CLONE_NEWPID;
    args->set_tid = (uint64_t)(uintptr_t)array;
    args->set_tid_size = 1;
  }

  unsigned char expected[OLD_BYTES];
  memcpy(expected, image, sizeof expected);
  long child = clone_from_old_footprint(syscall_sp, args, sizeof *args);
  if (child < 0) {
    fprintf(stderr, "clone3 %s failed: %s (%ld)\n", argv[1],
            strerror((int)-child), -child);
    require_image(image, expected, "failed original");
    return 3;
  }
  if (child == 0) {
    pid_t own_pid = getpid();
    if (fallback && own_pid != 1) _exit(40);
    if (outputs) expect_word(expected, image, child_tid, own_pid);
    _exit(require_image(image, expected, "copied child") ? 41 : 0);
  }

  if (outputs) {
    if (read_word(parent_tid) != child || read_word(pidfd) < 0 ||
        fcntl(read_word(pidfd), F_GETFD) < 0) {
      fprintf(stderr, "native outputs lost: child=%ld parent_tid=%d pidfd=%d\n",
              child, read_word(parent_tid), read_word(pidfd));
      return 4;
    }
    expect_word(expected, image, parent_tid, (int)child);
    expect_word(expected, image, pidfd, read_word(pidfd));
  }
  int parent_bad = require_image(image, expected, "parent");
  int status = 0;
  if (waitpid((pid_t)child, &status, 0) != child ||
      !WIFEXITED(status) || WEXITSTATUS(status) != 0) {
    fprintf(stderr, "copied-child image check failed: wait status=%d\n", status);
    return 5;
  }
  if (outputs && close(read_word(pidfd)) != 0) return 6;
  if (parent_bad) return 7;
  printf("clone3 old-stack %s: parent image preserved; child image preserved; "
         "outputs %s\n", argv[1], outputs ? "preserved" : "absent");
  return 0;
}
