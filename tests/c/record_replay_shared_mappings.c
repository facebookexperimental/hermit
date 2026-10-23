/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * MAP_SHARED mappings of one memfd, for the record/replay tests in
 * hermit-cli/tests/record_replay.rs. Linux backs every MAP_SHARED mapping of
 * a file with the file's own pages, so a store through one mapping is read
 * through every other mapping of the same page, however the mappings'
 * ranges overlap and however they were split, shrunk or moved since, and a
 * write(2) through a descriptor is read through the mappings too. Replay maps
 * no real file, so each mode checks one way its mappings must still share.
 *
 *   overlap: maps file pages [0, 2), then [1, 3); they share page 1.
 *   overlap-reverse: the same two mappings, made in the other order.
 *   adjacent-span: maps page 0 and page 1 apart, then [0, 2) across both.
 *   head-unmap: maps [0, 2), unmaps its first page, then maps page 1 again.
 *   tail-unmap: maps [0, 2), unmaps its last page, then maps [0, 2) again,
 *         and page 1 a third time.
 *   moved-source: maps [0, 2), moves it with mremap, then maps [0, 2) again.
 *   pwrite-remap: maps page 0, pwrites to it through the descriptor, then
 *         maps page 0 again and reads only the new mapping.
 *   pwrite-old-view: maps page 0, pwrites to it through the descriptor, and
 *         reads the old mapping.
 *   fork-child: a forked child maps the memfd again and writes through its
 *         own mapping; the parent reads through the mapping it made first.
 *   near-limit: fills its descriptor table up to its limit, then maps the
 *         memfd twice. Run it with a low limit (the record/replay test sets
 *         256); the table is filled with at most 65536 descriptors.
 *   private-table: a thread cloned without CLONE_FILES, whose descriptor
 *         table is its own, maps a memfd through a number that names another
 *         memfd in the leader's table, pwrites through it, and reads its
 *         mapping.
 *   nested-pidns: maps the memfd, then a child cloned into a new PID
 *         namespace reads the mapping it inherited, maps the memfd again and
 *         writes through the new mapping; the inherited one and the parent's
 *         see it. Natively it needs `unshare --user --map-root-user`.
 *   read-only-mprotect: maps a file through a descriptor opened read-only
 *         and asks mprotect to make the mapping writable, which Linux refuses
 *         with EACCES, then does the same through a descriptor opened for
 *         reading and writing, which it allows.
 *   chroot: maps the memfd, chroots into a new directory below the working
 *         directory, maps the memfd again and writes through the new
 *         mapping; the old one sees it. Natively it needs `unshare --user
 *         --map-root-user`.
 *   seccomp-filter: installs a seccomp filter that kills the process at
 *         socketpair, sendmsg, recvmsg or prlimit64, then maps the memfd
 *         twice. Linux installs it; Hermit refuses every filter
 *         (EOPNOTSUPP), in record and in replay.
 *
 * Usage: record_replay_shared_mappings <mode>
 * Prints one line per check and exits 0 only if every check saw what Linux
 * gives a native run.
 */

#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif

#include <errno.h>
#include <fcntl.h>
#include <sched.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <linux/audit.h>
#include <linux/filter.h>
#include <linux/seccomp.h>
#include <stddef.h>
#include <sys/mman.h>
#include <sys/prctl.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

static int failures;
static long page;

static void check(int ok, const char* what) {
  if (!ok) {
    printf("FAILED: %s (errno %s)\n", what, strerrorname_np(errno));
    failures++;
  }
}

/* A memfd of `pages` zero pages. */
static int memfd(int pages) {
  int fd = memfd_create("hermit-shared-mappings", 0);
  check(fd >= 0, "memfd_create");
  check(ftruncate(fd, pages * page) == 0, "ftruncate");
  return fd;
}

/* A read-write MAP_SHARED mapping of file pages [first, first + pages). */
static char* map_pages(int fd, int first, int pages) {
  char* address = mmap(NULL, pages * page, PROT_READ | PROT_WRITE, MAP_SHARED, fd, first * page);
  check(address != MAP_FAILED, "mmap");
  return address;
}

static void overlap(const char* mode, int reverse) {
  int fd = memfd(4);
  char *low, *high;
  if (reverse) {
    high = map_pages(fd, 1, 2);
    low = map_pages(fd, 0, 2);
  } else {
    low = map_pages(fd, 0, 2);
    high = map_pages(fd, 1, 2);
  }
  /* File page 1 is low's second page and high's first. */
  memcpy(low + page, "from low", 8);
  printf("%s: high sees %.8s\n", mode, high);
  memcpy(high + 8, "from high", 9);
  printf("%s: low sees %.9s\n", mode, low + page + 8);
}

static void adjacent_span(void) {
  int fd = memfd(2);
  char* left = map_pages(fd, 0, 1);
  char* right = map_pages(fd, 1, 1);
  char* span = map_pages(fd, 0, 2);
  memcpy(left, "left", 4);
  memcpy(right, "right", 5);
  printf("adjacent-span: span sees %.4s and %.5s\n", span, span + page);
  memcpy(span, "LEFT", 4);
  memcpy(span + page, "RIGHT", 5);
  printf("adjacent-span: pieces see %.4s and %.5s\n", left, right);
}

static void head_unmap(void) {
  int fd = memfd(2);
  char* source = map_pages(fd, 0, 2);
  check(munmap(source, page) == 0, "munmap head");
  char* again = map_pages(fd, 1, 1);
  memcpy(again, "new view", 8);
  printf("head-unmap: survivor sees %.8s\n", source + page);
  memcpy(source + page, "survivor", 8);
  printf("head-unmap: new view sees %.8s\n", again);
}

static void tail_unmap(void) {
  int fd = memfd(2);
  char* source = map_pages(fd, 0, 2);
  check(munmap(source + page, page) == 0, "munmap tail");
  char* again = map_pages(fd, 0, 2);
  memcpy(again, "new view", 8);
  printf("tail-unmap: survivor sees %.8s\n", source);
  memcpy(source, "survivor", 8);
  printf("tail-unmap: new view sees %.8s\n", again);
  memcpy(again + page, "page one", 8);
  char* third = map_pages(fd, 1, 1);
  printf("tail-unmap: third view sees %.8s\n", third);
}

static void moved_source(void) {
  int fd = memfd(2);
  char* source = map_pages(fd, 0, 2);
  void* target = mmap(NULL, 2 * page, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  check(target != MAP_FAILED, "reserve the target");
  char* moved = mremap(source, 2 * page, 2 * page, MREMAP_MAYMOVE | MREMAP_FIXED, target);
  check(moved == target, "mremap");
  char* again = map_pages(fd, 0, 2);
  memcpy(again + page, "new view", 8);
  printf("moved-source: moved mapping sees %.8s\n", moved + page);
  memcpy(moved, "moved", 5);
  printf("moved-source: new view sees %.5s\n", again);
}

static void pwrite_remap(int read_old_view) {
  const char* mode = read_old_view ? "pwrite-old-view" : "pwrite-remap";
  int fd = memfd(1);
  check(pwrite(fd, "before", 6, 0) == 6, "pwrite before");
  char* first = map_pages(fd, 0, 1);
  printf("%s: first view sees %.6s\n", mode, first);
  check(pwrite(fd, "after!", 6, 0) == 6, "pwrite after");
  if (read_old_view) {
    printf("%s: first view then sees %.6s\n", mode, first);
  } else {
    char* second = map_pages(fd, 0, 1);
    printf("%s: second view sees %.6s\n", mode, second);
  }
}

static void fork_child(void) {
  int fd = memfd(1);
  char* parent = map_pages(fd, 0, 1);
  memcpy(parent, "parent", 6);
  pid_t child = fork();
  check(child >= 0, "fork");
  if (child == 0) {
    char* own = map_pages(fd, 0, 1);
    printf("fork-child: child's mapping sees %.6s\n", own);
    memcpy(own, "child!", 6);
    _exit(failures == 0 ? 0 : 1);
  }
  int status = 0;
  check(waitpid(child, &status, 0) == child, "waitpid");
  check(WIFEXITED(status) && WEXITSTATUS(status) == 0, "child status");
  printf("fork-child: parent sees %.6s\n", parent);
}

static void near_limit(void) {
  /* Fills the descriptor table up to the limit the test started it with, so
   * the mappings are made with no slot free. */
  int fd = memfd(1);
  int filled = 0;
  while (filled < 65536 && dup(0) >= 0) {
    filled++;
  }
  check(errno == EMFILE, "the table fills up");
  char* first = map_pages(fd, 0, 1);
  char* second = map_pages(fd, 0, 1);
  memcpy(first, "at limit", 8);
  printf("near-limit: second sees %.8s\n", second);
  int full = dup(0) < 0 && errno == EMFILE;
  printf("near-limit: the table is %s\n", full ? "still full" : "no longer full");
}

/* The worker of private-table: its own descriptor table names `fds[1]` at
 * number 60, where the leader's names `fds[0]`. It shares the leader's
 * thread-local storage, so it makes raw calls only and leaves the printing
 * to the leader. */
#define PRIVATE_FD 60
static char worker_stack[64 * 1024] __attribute__((aligned(16)));
static int private_fds[2];
static char* volatile worker_view;
static volatile long worker_wrote = -1;
static volatile int worker_done;

static int private_table_worker(void* unused) {
  (void)unused;
  if (syscall(SYS_dup2, private_fds[1], PRIVATE_FD) == PRIVATE_FD) {
    long view = syscall(SYS_mmap, 0L, page, (long)(PROT_READ | PROT_WRITE), (long)MAP_SHARED,
                        (long)PRIVATE_FD, 0L);
    if (view != -1) {
      worker_view = (char*)view;
      worker_wrote = syscall(SYS_pwrite64, PRIVATE_FD, "worker", 6L, 0L);
    }
  }
  worker_done = 1;
  syscall(SYS_exit, 0);
  return 0;
}

static void private_table(void) {
  private_fds[0] = memfd(1);
  private_fds[1] = memfd(1);
  check(dup2(private_fds[0], PRIVATE_FD) == PRIVATE_FD, "dup2 in the leader");
  int flags = CLONE_VM | CLONE_FS | CLONE_SIGHAND | CLONE_THREAD | CLONE_SYSVSEM;
  check(clone(private_table_worker, worker_stack + sizeof(worker_stack), flags, NULL) > 0, "clone");
  while (!worker_done) {
    sched_yield();
  }
  check(worker_view != NULL && worker_wrote == 6, "the worker's mapping and pwrite");
  if (worker_view != NULL) {
    printf("private-table: worker's mapping sees %.6s\n", worker_view);
  }
  char* leaders = map_pages(PRIVATE_FD, 0, 1);
  printf("private-table: leader's memfd is %s\n", leaders[0] == 0 ? "untouched" : "written");
}

static void nested_pidns(void) {
  int fd = memfd(1);
  char* parent = map_pages(fd, 0, 1);
  memcpy(parent, "parent", 6);
  long child = syscall(SYS_clone, (long)(CLONE_NEWPID | SIGCHLD), 0L, 0L, 0L, 0L);
  check(child >= 0, "clone(CLONE_NEWPID)");
  if (child == 0) {
    printf("nested-pidns: child is pid %d and sees %.6s\n", (int)getpid(), parent);
    char* own = map_pages(fd, 0, 1);
    memcpy(own, "child!", 6);
    printf("nested-pidns: child's inherited mapping sees %.6s\n", parent);
    _exit(failures == 0 ? 0 : 1);
  }
  int status = 0;
  check(waitpid((pid_t)child, &status, 0) == child, "waitpid");
  check(WIFEXITED(status) && WEXITSTATUS(status) == 0, "child status");
  printf("nested-pidns: parent sees %.6s\n", parent);
}

static void read_only_mprotect(void) {
  const char* path = "/tmp/hermit-shared-mappings-read-only";
  int writable = open(path, O_RDWR | O_CREAT | O_TRUNC, 0600);
  check(writable >= 0, "create the file");
  check(pwrite(writable, "contents", 8, 0) == 8, "pwrite the file");
  int read_only = open(path, O_RDONLY);
  check(read_only >= 0, "open the file read-only");
  char* view = mmap(NULL, page, PROT_READ, MAP_SHARED, read_only, 0);
  check(view != MAP_FAILED, "mmap read-only");
  int refused = mprotect(view, page, PROT_READ | PROT_WRITE);
  printf("read-only-mprotect: read-only descriptor: %s\n",
         refused == 0 ? "made writable" : strerrorname_np(errno));
  char* other = mmap(NULL, page, PROT_READ, MAP_SHARED, writable, 0);
  check(other != MAP_FAILED, "mmap read-write");
  int allowed = mprotect(other, page, PROT_READ | PROT_WRITE);
  printf("read-only-mprotect: read-write descriptor: %s, sees %.8s\n",
         allowed == 0 ? "made writable" : strerrorname_np(errno), other);
  check(unlink(path) == 0, "unlink the file");
}

static void chroot_remap(void) {
  int fd = memfd(1);
  char* outside = map_pages(fd, 0, 1);
  memcpy(outside, "outside", 7);
  check(mkdir("hermit-shared-chroot", 0700) == 0 || errno == EEXIST, "mkdir");
  if (chroot("hermit-shared-chroot") != 0) {
    printf("chroot: chroot %s\n", strerrorname_np(errno));
    return;
  }
  char* inside = map_pages(fd, 0, 1);
  printf("chroot: the new mapping sees %.7s\n", inside);
  memcpy(inside, "inside!", 7);
  printf("chroot: the old mapping sees %.7s\n", outside);
}

static void seccomp_filter(void) {
  struct sock_filter filter[] = {
      BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, arch)),
      BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, AUDIT_ARCH_X86_64, 1, 0),
      BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ALLOW),
      BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, nr)),
      BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, SYS_socketpair, 4, 0),
      BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, SYS_sendmsg, 3, 0),
      BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, SYS_recvmsg, 2, 0),
      BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, SYS_prlimit64, 1, 0),
      BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ALLOW),
      BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_KILL_PROCESS),
  };
  struct sock_fprog program = {sizeof(filter) / sizeof(filter[0]), filter};
  /* An unprivileged filter needs no_new_privs, which Hermit does not set. */
  int no_new_privs = prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
  const char* no_new_privs_result = no_new_privs == 0 ? "set" : strerrorname_np(errno);
  long installed = syscall(SYS_seccomp, SECCOMP_SET_MODE_FILTER, 0, &program);
  printf("seccomp-filter: no_new_privs %s, install %s\n", no_new_privs_result,
         installed == 0 ? "succeeded" : strerrorname_np(errno));
  int fd = memfd(1);
  char* first = map_pages(fd, 0, 1);
  char* second = map_pages(fd, 0, 1);
  memcpy(first, "filtered", 8);
  printf("seccomp-filter: second sees %.8s\n", second);
}

int main(int argc, char** argv) {
  setvbuf(stdout, NULL, _IONBF, 0);
  page = sysconf(_SC_PAGESIZE);
  if (argc != 2) {
    fprintf(stderr, "usage: %s <mode>\n", argv[0]);
    return 2;
  }
  const char* mode = argv[1];
  if (strcmp(mode, "overlap") == 0) {
    overlap(mode, 0);
  } else if (strcmp(mode, "overlap-reverse") == 0) {
    overlap(mode, 1);
  } else if (strcmp(mode, "adjacent-span") == 0) {
    adjacent_span();
  } else if (strcmp(mode, "head-unmap") == 0) {
    head_unmap();
  } else if (strcmp(mode, "tail-unmap") == 0) {
    tail_unmap();
  } else if (strcmp(mode, "moved-source") == 0) {
    moved_source();
  } else if (strcmp(mode, "pwrite-remap") == 0) {
    pwrite_remap(0);
  } else if (strcmp(mode, "pwrite-old-view") == 0) {
    pwrite_remap(1);
  } else if (strcmp(mode, "fork-child") == 0) {
    fork_child();
  } else if (strcmp(mode, "near-limit") == 0) {
    near_limit();
  } else if (strcmp(mode, "private-table") == 0) {
    private_table();
  } else if (strcmp(mode, "nested-pidns") == 0) {
    nested_pidns();
  } else if (strcmp(mode, "read-only-mprotect") == 0) {
    read_only_mprotect();
  } else if (strcmp(mode, "chroot") == 0) {
    chroot_remap();
  } else if (strcmp(mode, "seccomp-filter") == 0) {
    seccomp_filter();
  } else {
    fprintf(stderr, "unknown mode %s\n", mode);
    return 2;
  }
  return failures == 0 ? 0 : 1;
}
