/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * The main thread's stack as glibc's pthread_getattr_np finds it in
 * /proc/self/maps, while the program changes that stack. glibc takes the
 * stack's top from __libc_stack_end, and its size from the stack's rlimit,
 * less the part of the line holding __libc_stack_end above that top, and no
 * more than reaches the end of the line before it (the JVM and Rust's
 * standard library read the main thread's stack this way).
 *
 * At each step the program prints the step's name; the lines of the maps
 * file it read that end within 16 MiB below the stack's top and start below
 * it, the stack and the mappings a change split from it; and what
 * pthread_getattr_np answered: "stack RET TOP SIZE LIMIT", with the stack's
 * soft rlimit. The steps change pages 64 KiB below the frame that computes
 * them, inside the stack Linux maps at exec and below anything the program's
 * calls use:
 *   start:     as the program started; then a read of the file opened
 *              with O_PATH, which fails EBADF ("o_path read -1 9"), and the
 *              size fstat and statx report for a descriptor of the file,
 *              which is 0 ("fstat size 0 statx size 0").
 *   guarded:   a guard zone of four pages made inaccessible (PROT_NONE), as
 *              the JVM protects the yellow and red zones of a thread's stack.
 *   used:      after a recursion wrote each page down to the zone.
 *   read-only: the page above the zone made read-only, then
 *   writable:  read-write again, which joins it to the stack again.
 *   random:    MADV_RANDOM on the page above the zone, a hint Linux keeps
 *              in the mapping's flags, so the page is a mapping of its own;
 *   normal:    MADV_NORMAL on it, which joins it again.
 *   dontneed:  MADV_DONTNEED on the four pages above the zone, which
 *              changes no mapping.
 *   child:     in a forked child, which makes its copy of the zone
 *              accessible; on Linux the copy stays a mapping of its own.
 *   unguarded: the zone accessible again in the parent, once the child
 *              exited.
 *   orphaned:  a forked child opens its maps, forks a grandchild, which
 *              inherits the descriptor, and exits; once the program has
 *              reaped the child, the grandchild reads the descriptor, which
 *              fails ESRCH ("read -1 3"): Linux finds the task the file names
 *              again at every read.
 *   vfork:     in another forked child, once a vfork child it made, which
 *              shares its address space, made the page above the zone
 *              read-only and exited.
 *   unmapped:  the page above the zone unmapped.
 *   hole:      an mprotect of two pages on each side of that hole, which
 *              Linux applies to the two pages below it before it fails
 *              ENOMEM ("hole mprotect -1 12").
 * With the argument "descriptors", five more steps read a maps file through
 * a descriptor opened elsewhere, which Linux binds to its task and that
 * task's address space when the file is opened, or look at its position:
 *   inherited: after "unguarded", a forked child makes the page above the
 *              zone read-only and reads the descriptor its parent opened,
 *              which lists the parent's stack;
 *   other:     another child makes that page read-only, and the parent reads
 *              the child's maps, /proc/<pid>/maps, which list the child's
 *              stack;
 *   reaped:    a child that shares the program's address space and its
 *              descriptor table (CLONE_VM | CLONE_VFORK | CLONE_FILES) opens
 *              its maps and exits; once it is reaped, the program reads the
 *              descriptor, which fails ESRCH: the address space is the
 *              reader's, but the task is gone;
 *   position:  the position /proc/self/fdinfo shows for a maps descriptor
 *              after a read of 100 bytes ("fdinfo pos 100"); then a forked
 *              child reads 100 bytes of its maps, sends that descriptor over
 *              a socketpair and closes it, and the program reads the copy it
 *              received to its end, which continues where the child stopped,
 *              and whose fdinfo position is then the bytes both read
 *              ("received continues 1 pos 1");
 *   exec:      last, the program executes itself with a descriptor it opened
 *              before, which reads nothing ("bytes 0"): the address space it
 *              names is gone.
 * With the argument "no-vfork", the program skips the "vfork" step, which
 * runs in a forked child and so changes nothing a later step prints. KVM runs
 * a vfork child on a copy of its parent's memory, which carries contents back
 * to the parent but not mapping changes, so it refuses by name a vfork child
 * that leaves a mapping changed, as this step's child does
 * (https://github.com/rrnewton/reverie/issues/1087).
 *
 * Usage: main_stack_maps [descriptors] [no-vfork]
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <sched.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/resource.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>

#define PAGE 4096UL
#define AREA (16UL << 20)

extern void *__libc_stack_end;

static void fail(int line) {
  char message[64];
  int length = snprintf(message, sizeof(message),
                        "main_stack_maps.c:%d: errno %d\n", line, errno);
  if (write(2, message, (size_t)length) < 0) {
  }
  _exit(90);
}

#define CHECK(condition) \
  do {                   \
    if (!(condition)) {  \
      fail(__LINE__);    \
    }                    \
  } while (0)

static void out(const char *text, size_t length) {
  while (length > 0) {
    ssize_t written = write(1, text, length);
    CHECK(written > 0);
    text += written;
    length -= (size_t)written;
  }
}

static char text[1 << 20];

/* Reads the maps file open on `fd` to its end into `text`, and closes it. */
static size_t read_maps(int fd) {
  size_t length = 0;
  for (;;) {
    ssize_t got = read(fd, text + length, sizeof(text) - length);
    CHECK(got >= 0);
    if (got == 0) {
      break;
    }
    length += (size_t)got;
    CHECK(length < sizeof(text));
  }
  CHECK(close(fd) == 0);
  return length;
}

/* Prints step `name`: the stack area's lines of the maps file open on `fd`,
 * then what pthread_getattr_np answered for this, the main thread. */
static void step_through(const char *name, int fd) {
  size_t length = read_maps(fd);
  char line[128];
  int n = snprintf(line, sizeof(line), "step %s\n", name);
  out(line, (size_t)n);
  uintptr_t top = ((uintptr_t)__libc_stack_end & ~(PAGE - 1)) + PAGE;
  for (size_t at = 0; at < length;) {
    char *end = memchr(text + at, '\n', length - at);
    CHECK(end != NULL);
    unsigned long from = 0;
    unsigned long to = 0;
    CHECK(sscanf(text + at, "%lx-%lx", &from, &to) == 2);
    if (to > top - AREA && from < top) {
      out(text + at, (size_t)(end - (text + at)) + 1);
    }
    at = (size_t)(end - text) + 1;
  }
  pthread_attr_t attr;
  void *low = NULL;
  size_t size = 0;
  int ret = pthread_getattr_np(pthread_self(), &attr);
  if (ret == 0) {
    CHECK(pthread_attr_getstack(&attr, &low, &size) == 0);
    CHECK(pthread_attr_destroy(&attr) == 0);
  }
  struct rlimit limit;
  CHECK(getrlimit(RLIMIT_STACK, &limit) == 0);
  n = snprintf(line, sizeof(line), "stack %d %lx %lx %lx\n", ret,
               (unsigned long)((uintptr_t)low + size), (unsigned long)size,
               (unsigned long)limit.rlim_cur);
  out(line, (size_t)n);
}

/* Prints step `name` from a maps file opened now. */
static void step(const char *name) {
  int fd = open("/proc/self/maps", O_RDONLY | O_CLOEXEC);
  CHECK(fd >= 0);
  step_through(name, fd);
}

/* Prints step `name`: what a read of the maps file open on `fd`, whose task
 * is gone, answered ("read -1 3", ESRCH), and closes it. */
static void step_gone(const char *name, int fd) {
  errno = 0;
  ssize_t got = read(fd, text, sizeof(text));
  char line[96];
  int n = snprintf(line, sizeof(line), "step %s\nread %zd %d\n", name, got,
                   got < 0 ? errno : 0);
  out(line, (size_t)n);
  CHECK(close(fd) == 0);
}

/* Opens this task's maps into the int `result` points to (`reaped`). */
static int open_own_maps(void *result) {
  *(volatile int *)result = open("/proc/self/maps", O_RDONLY | O_CLOEXEC);
  return 0;
}

static char clone_stack[64 * 1024] __attribute__((aligned(16)));

/* Writes a page of each frame down to `floor`, then returns how many frames
 * it took. */
static int __attribute__((noinline)) run_down(uintptr_t floor) {
  volatile char frame[PAGE];
  frame[0] = 1;
  frame[PAGE - 1] = 1;
  if ((uintptr_t)&frame[0] < floor + 2 * PAGE) {
    return 1;
  }
  return run_down(floor) + frame[0];
}

/* Waits for `child`, which must exit 0. */
static void reap(pid_t child) {
  int status = 0;
  CHECK(waitpid(child, &status, 0) == child);
  CHECK(WIFEXITED(status) && WEXITSTATUS(status) == 0);
}

/* The position /proc/self/fdinfo shows for descriptor `fd`. */
static long long fdinfo_position(int fd) {
  char path[64];
  snprintf(path, sizeof(path), "/proc/self/fdinfo/%d", fd);
  int info = open(path, O_RDONLY | O_CLOEXEC);
  CHECK(info >= 0);
  char buffer[256];
  ssize_t got = read(info, buffer, sizeof(buffer) - 1);
  CHECK(got > 0);
  buffer[got] = 0;
  long long position = -1;
  CHECK(sscanf(buffer, "pos: %lld", &position) == 1);
  CHECK(close(info) == 0);
  return position;
}

/* Writes the `length` bytes at `data` to `fd`, a pipe. */
static void write_all(int fd, const char *data, size_t length) {
  while (length > 0) {
    ssize_t written = write(fd, data, length);
    CHECK(written > 0);
    data += written;
    length -= (size_t)written;
  }
}

/* Step "position" (see the top of this file). */
static void step_position(void) {
  int fd = open("/proc/self/maps", O_RDONLY | O_CLOEXEC);
  CHECK(fd >= 0);
  char part[100];
  CHECK(read(fd, part, sizeof(part)) == (ssize_t)sizeof(part));
  long long position = fdinfo_position(fd);
  CHECK(close(fd) == 0);
  int pair[2];
  int whole[2];
  int done[2];
  CHECK(socketpair(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0, pair) == 0);
  CHECK(pipe2(whole, O_CLOEXEC) == 0 && pipe2(done, O_CLOEXEC) == 0);
  pid_t sender = fork();
  CHECK(sender >= 0);
  if (sender == 0) {
    /* The descriptor it sends, after a read of 100 bytes; then its whole
     * map, through another descriptor, for the program to compare. */
    int sent = open("/proc/self/maps", O_RDONLY | O_CLOEXEC);
    CHECK(sent >= 0);
    CHECK(read(sent, part, sizeof(part)) == (ssize_t)sizeof(part));
    int all = open("/proc/self/maps", O_RDONLY | O_CLOEXEC);
    CHECK(all >= 0);
    size_t length = read_maps(all);
    write_all(whole[1], text, length);
    CHECK(close(whole[1]) == 0);
    char byte = 'm';
    struct iovec vector = {.iov_base = &byte, .iov_len = 1};
    union {
      struct cmsghdr header;
      char space[CMSG_SPACE(sizeof(int))];
    } control;
    memset(&control, 0, sizeof(control));
    struct msghdr message = {
        .msg_iov = &vector,
        .msg_iovlen = 1,
        .msg_control = control.space,
        .msg_controllen = sizeof(control.space),
    };
    struct cmsghdr *header = CMSG_FIRSTHDR(&message);
    header->cmsg_level = SOL_SOCKET;
    header->cmsg_type = SCM_RIGHTS;
    header->cmsg_len = CMSG_LEN(sizeof(int));
    memcpy(CMSG_DATA(header), &sent, sizeof(int));
    CHECK(sendmsg(pair[1], &message, 0) == 1);
    CHECK(close(sent) == 0);
    /* Alive until the program has read, so that its address space is. */
    CHECK(read(done[0], &byte, 1) == 1);
    _exit(0);
  }
  CHECK(close(whole[1]) == 0);
  size_t expected = read_maps(whole[0]);
  static char sender_map[1 << 20];
  memcpy(sender_map, text, expected);
  char byte = 0;
  struct iovec vector = {.iov_base = &byte, .iov_len = 1};
  union {
    struct cmsghdr header;
    char space[CMSG_SPACE(sizeof(int))];
  } control;
  struct msghdr message = {
      .msg_iov = &vector,
      .msg_iovlen = 1,
      .msg_control = control.space,
      .msg_controllen = sizeof(control.space),
  };
  CHECK(recvmsg(pair[0], &message, MSG_CMSG_CLOEXEC) == 1);
  struct cmsghdr *header = CMSG_FIRSTHDR(&message);
  CHECK(header != NULL && header->cmsg_type == SCM_RIGHTS);
  int received = -1;
  memcpy(&received, CMSG_DATA(header), sizeof(int));
  size_t length = 0;
  for (;;) {
    ssize_t got = read(received, text + length, sizeof(text) - length);
    CHECK(got >= 0);
    if (got == 0) {
      break;
    }
    length += (size_t)got;
    CHECK(length < sizeof(text));
  }
  int continues = expected == sizeof(part) + length &&
                  memcmp(sender_map + sizeof(part), text, length) == 0;
  int at_end = fdinfo_position(received) == (long long)expected;
  CHECK(close(received) == 0);
  CHECK(write(done[1], "d", 1) == 1);
  reap(sender);
  CHECK(close(pair[0]) == 0 && close(pair[1]) == 0);
  CHECK(close(done[0]) == 0 && close(done[1]) == 0);
  char line[96];
  int n = snprintf(line, sizeof(line),
                   "step position\nfdinfo pos %lld\nreceived continues %d pos %d\n",
                   position, continues, at_end);
  out(line, (size_t)n);
}

int main(int argc, char **argv) {
  if (argc == 3 && strcmp(argv[1], "exec") == 0) {
    char line[64];
    int n = snprintf(line, sizeof(line), "step exec\nbytes %zu\n",
                     read_maps(atoi(argv[2])));
    out(line, (size_t)n);
    return 0;
  }
  int descriptors = 0;
  int vfork_step = 1;
  for (int arg = 1; arg < argc; arg++) {
    if (strcmp(argv[arg], "descriptors") == 0) {
      descriptors = 1;
    } else if (strcmp(argv[arg], "no-vfork") == 0) {
      vfork_step = 0;
    } else {
      fail(__LINE__);
    }
  }
  step("start");
  int path_only = open("/proc/self/maps", O_PATH | O_CLOEXEC);
  CHECK(path_only >= 0);
  char unread[8];
  errno = 0;
  ssize_t got = read(path_only, unread, sizeof(unread));
  char line[64];
  int n = snprintf(line, sizeof(line), "o_path read %zd %d\n", got, errno);
  out(line, (size_t)n);
  CHECK(close(path_only) == 0);
  int sized = open("/proc/self/maps", O_RDONLY | O_CLOEXEC);
  CHECK(sized >= 0);
  struct stat status;
  CHECK(fstat(sized, &status) == 0);
  struct statx extended;
  CHECK(statx(sized, "", AT_EMPTY_PATH, STATX_SIZE, &extended) == 0);
  n = snprintf(line, sizeof(line), "fstat size %lld statx size %llu\n",
               (long long)status.st_size,
               (unsigned long long)extended.stx_size);
  out(line, (size_t)n);
  CHECK(close(sized) == 0);
  volatile char here = 0;
  uintptr_t zone_top = ((uintptr_t)&here & ~(PAGE - 1)) - 16 * PAGE;
  char *zone = (char *)(zone_top - 4 * PAGE);
  char *above = (char *)zone_top;
  CHECK(mprotect(zone, 4 * PAGE, PROT_NONE) == 0);
  step("guarded");
  CHECK(run_down(zone_top) > 1);
  step("used");
  CHECK(mprotect(above, PAGE, PROT_READ) == 0);
  step("read-only");
  CHECK(mprotect(above, PAGE, PROT_READ | PROT_WRITE) == 0);
  step("writable");
  CHECK(madvise(above, PAGE, MADV_RANDOM) == 0);
  step("random");
  CHECK(madvise(above, PAGE, MADV_NORMAL) == 0);
  step("normal");
  CHECK(madvise(above, 4 * PAGE, MADV_DONTNEED) == 0);
  step("dontneed");
  pid_t child = fork();
  CHECK(child >= 0);
  if (child == 0) {
    CHECK(mprotect(zone, 4 * PAGE, PROT_READ | PROT_WRITE) == 0);
    step("child");
    _exit(0);
  }
  reap(child);
  CHECK(mprotect(zone, 4 * PAGE, PROT_READ | PROT_WRITE) == 0);
  step("unguarded");
  if (descriptors) {
    int fd = open("/proc/self/maps", O_RDONLY | O_CLOEXEC);
    CHECK(fd >= 0);
    pid_t reader = fork();
    CHECK(reader >= 0);
    if (reader == 0) {
      CHECK(mprotect(above, PAGE, PROT_READ) == 0);
      step_through("inherited", fd);
      _exit(0);
    }
    reap(reader);
    CHECK(close(fd) == 0);
    int ready[2];
    int done[2];
    CHECK(pipe(ready) == 0 && pipe(done) == 0);
    pid_t other = fork();
    CHECK(other >= 0);
    char byte = 0;
    if (other == 0) {
      CHECK(mprotect(above, PAGE, PROT_READ) == 0);
      CHECK(write(ready[1], "r", 1) == 1);
      CHECK(read(done[0], &byte, 1) == 1);
      _exit(0);
    }
    CHECK(read(ready[0], &byte, 1) == 1);
    char path[64];
    snprintf(path, sizeof(path), "/proc/%d/maps", (int)other);
    fd = open(path, O_RDONLY | O_CLOEXEC);
    CHECK(fd >= 0);
    step_through("other", fd);
    CHECK(write(done[1], "d", 1) == 1);
    reap(other);
    CHECK(close(ready[0]) == 0 && close(ready[1]) == 0);
    CHECK(close(done[0]) == 0 && close(done[1]) == 0);
  }
  if (descriptors) {
    volatile int shared = -1;
    pid_t opener = clone(open_own_maps, clone_stack + sizeof(clone_stack),
                         CLONE_VM | CLONE_VFORK | CLONE_FILES | SIGCHLD,
                         (void *)&shared);
    CHECK(opener > 0);
    reap(opener);
    CHECK(shared >= 0);
    step_gone("reaped", shared);
    step_position();
  }
  int go[2];
  int done[2];
  CHECK(pipe(go) == 0 && pipe(done) == 0);
  pid_t opener = fork();
  CHECK(opener >= 0);
  if (opener == 0) {
    int fd = open("/proc/self/maps", O_RDONLY | O_CLOEXEC);
    CHECK(fd >= 0);
    pid_t reader = fork();
    CHECK(reader >= 0);
    if (reader == 0) {
      char byte = 0;
      CHECK(read(go[0], &byte, 1) == 1);
      step_gone("orphaned", fd);
      CHECK(write(done[1], "d", 1) == 1);
      _exit(0);
    }
    _exit(0);
  }
  reap(opener);
  char ack = 0;
  CHECK(write(go[1], "g", 1) == 1);
  CHECK(read(done[0], &ack, 1) == 1);
  CHECK(close(go[0]) == 0 && close(go[1]) == 0);
  CHECK(close(done[0]) == 0 && close(done[1]) == 0);
  if (vfork_step) {
    pid_t vforker = fork();
    CHECK(vforker >= 0);
    if (vforker == 0) {
      pid_t partner = vfork();
      if (partner == 0) {
        _exit(mprotect(above, PAGE, PROT_READ) == 0 ? 0 : 91);
      }
      CHECK(partner > 0);
      reap(partner);
      step("vfork");
      _exit(0);
    }
    reap(vforker);
  }
  CHECK(munmap(above, PAGE) == 0);
  step("unmapped");
  errno = 0;
  int changed = mprotect(above - 2 * PAGE, 4 * PAGE, PROT_READ);
  n = snprintf(line, sizeof(line), "hole mprotect %d %d\n", changed, errno);
  out(line, (size_t)n);
  step("hole");
  if (descriptors) {
    int fd = open("/proc/self/maps", O_RDONLY);
    CHECK(fd >= 0);
    char number[16];
    snprintf(number, sizeof(number), "%d", fd);
    execl(argv[0], argv[0], "exec", number, (char *)NULL);
    fail(__LINE__);
  }
  return here;
}
