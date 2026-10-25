/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * /proc/<pid>/cmdline as Linux's get_mm_cmdline (fs/proc/base.c) shows it.
 *
 * The program re-executes itself with arguments and an
 * environment it chooses, so every byte it prints is its own, whatever path
 * and environment started it. Then it reads its own command line whole, in
 * three-byte chunks, at offsets with pread and after lseek, through every
 * spelling of its own process; after overwriting an argument in place; after
 * overwriting its last argument byte (setproctitle: one string from the
 * first argument, at most one page, running on into the environment area but
 * never past its end); with the page of its last argument byte made
 * PROT_NONE; a running child's file read by its parent; a descriptor
 * inherited across fork, read after the opener rewrote its arguments; a
 * thread's task/<tid> file, and that file once the thread is gone (ESRCH);
 * the top-level file of a thread group whose leader has exited (empty); and,
 * after a second exec with new arguments, a descriptor opened before it.
 *
 * Standard output is the oracle; the program exits 0 when every step ran.
 */

#include <errno.h>
#include <limits.h>
#include <fcntl.h>
#include <pthread.h>
#include <sched.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/uio.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

extern char** environ;

static int failures;

static void fail(const char* what) {
  printf("FAIL %s errno=%d\n", what, errno);
  failures++;
}

/* Print `bytes` with NUL as \0 and other unprintable bytes as \xNN. */
static void show(const char* label, const unsigned char* bytes, ssize_t n) {
  printf("%s len=%zd bytes=", label, n);
  for (ssize_t i = 0; i < n; i++) {
    unsigned char c = bytes[i];
    if (c == 0) {
      fputs("\\0", stdout);
    } else if (c == '\\') {
      fputs("\\\\", stdout);
    } else if (c < 0x20 || c >= 0x7f) {
      printf("\\x%02x", c);
    } else {
      putchar(c);
    }
  }
  putchar('\n');
}

/* Read all of `fd` from its position in reads of `chunk` bytes. */
static ssize_t read_all(int fd, unsigned char* buffer, size_t size,
                        size_t chunk, int* reads) {
  size_t total = 0;
  *reads = 0;
  for (;;) {
    size_t want = chunk < size - total ? chunk : size - total;
    if (want == 0) {
      return (ssize_t)total;
    }
    ssize_t got = read(fd, buffer + total, want);
    if (got < 0) {
      return -1;
    }
    (*reads)++;
    if (got == 0) {
      return (ssize_t)total;
    }
    total += (size_t)got;
  }
}

/* Read the whole file at `path`, or -1 with errno. */
static ssize_t read_path(const char* path, unsigned char* buffer, size_t size) {
  int fd = open(path, O_RDONLY | O_CLOEXEC);
  if (fd < 0) {
    return -1;
  }
  int reads;
  ssize_t n = read_all(fd, buffer, size, size, &reads);
  int saved = errno;
  close(fd);
  errno = saved;
  return n;
}

static void show_path(const char* label, const char* path) {
  unsigned char buffer[8192];
  ssize_t n = read_path(path, buffer, sizeof(buffer));
  if (n < 0) {
    printf("%s errno=%d\n", label, errno);
    return;
  }
  show(label, buffer, n);
}

static void show_self(const char* label) {
  show_path(label, "/proc/self/cmdline");
}

static pid_t gettid_(void) {
  return (pid_t)syscall(SYS_gettid);
}

/* The argument and environment areas, from argv and environ as the kernel
 * laid them out: argv[0] to just past the last argument's NUL, then the
 * environment strings. */
static char* arg_start;
static char* arg_end;
static char* env_end;

static void find_areas(int argc, char** argv) {
  arg_start = argv[0];
  arg_end = argv[argc - 1] + strlen(argv[argc - 1]) + 1;
  env_end = arg_end;
  int contiguous = 1;
  for (int i = 1; i < argc; i++) {
    contiguous &= argv[i] == argv[i - 1] + strlen(argv[i - 1]) + 1;
  }
  char* expected = arg_end;
  for (char** entry = environ; *entry; entry++) {
    contiguous &= *entry == expected;
    expected = *entry + strlen(*entry) + 1;
    env_end = expected;
  }
  printf("layout arg-area=%td env-area=%td contiguous=%d\n",
         arg_end - arg_start, env_end - arg_end, contiguous);
}

static void offsets(void) {
  unsigned char whole[8192];
  ssize_t length = read_path("/proc/self/cmdline", whole, sizeof(whole));
  if (length < 0) {
    fail("read whole");
    return;
  }
  int fd = open("/proc/self/cmdline", O_RDONLY | O_CLOEXEC);
  if (fd < 0) {
    fail("open for chunks");
    return;
  }
  unsigned char chunks[8192];
  int reads;
  ssize_t n = read_all(fd, chunks, sizeof(chunks), 3, &reads);
  printf("chunks of 3: len=%zd reads=%d same=%d\n", n, reads,
         n == length && memcmp(chunks, whole, (size_t)n) == 0);
  const off_t positions[] = {0, 5, 18, length - 1, length, length + 10};
  for (size_t i = 0; i < sizeof(positions) / sizeof(positions[0]); i++) {
    unsigned char part[4];
    ssize_t got = pread(fd, part, sizeof(part), positions[i]);
    char label[64];
    snprintf(label, sizeof(label), "pread 4 at %lld", (long long)positions[i]);
    if (got < 0) {
      printf("%s errno=%d\n", label, errno);
    } else {
      show(label, part, got);
    }
  }
  errno = 0;
  ssize_t negative = pread(fd, chunks, 4, -1);
  printf("pread at -1: %zd errno=%d\n", negative, errno);
  off_t at = lseek(fd, 7, SEEK_SET);
  unsigned char part[5];
  ssize_t got = read(fd, part, sizeof(part));
  printf("lseek 7: %lld\n", (long long)at);
  show("read 5 after lseek 7", part, got);
  printf("lseek 0 SEEK_CUR: %lld\n", (long long)lseek(fd, 0, SEEK_CUR));
  printf("lseek 0 SEEK_END: %lld\n", (long long)lseek(fd, 0, SEEK_END));
  /* rw_verify_area: a read whose end would pass LLONG_MAX is EINVAL, before
   * the flags are looked at. procfs refuses a seek past its s_maxbytes, so
   * the shared position cannot get there: readv and preadv2 at -1 after
   * that seek read at the position the descriptor kept. A read of zero
   * bytes is 0. */
  unsigned char byte;
  struct iovec one = {&byte, 1};
  errno = 0;
  ssize_t past = pread(fd, &byte, 1, LLONG_MAX);
  printf("pread 1 at LLONG_MAX: %zd errno=%d\n", past, errno);
  printf("lseek LLONG_MAX: %lld\n", (long long)lseek(fd, LLONG_MAX, SEEK_SET));
  errno = 0;
  past = readv(fd, &one, 1);
  printf("readv 1 after lseek LLONG_MAX: %zd errno=%d\n", past, errno);
  errno = 0;
  past = preadv2(fd, &one, 1, -1, 0x7fff);
  printf("preadv2 1 at -1 after lseek LLONG_MAX, unknown flags: %zd errno=%d\n", past, errno);
  printf("lseek 2^31-1: %lld\n", (long long)lseek(fd, 2147483647, SEEK_SET));
  errno = 0;
  past = readv(fd, &one, 1);
  printf("readv 1 at 2^31-1: %zd errno=%d\n", past, errno);
  errno = 0;
  off_t beyond = lseek(fd, 2147483648LL, SEEK_SET);
  printf("lseek 2^31: %lld errno=%d\n", (long long)beyond, errno);
  lseek(fd, 0, SEEK_SET);
  errno = 0;
  past = preadv2(fd, &one, 1, -1, 0x7fff);
  printf("preadv2 1 at -1 after lseek 0, unknown flags: %zd errno=%d\n", past, errno);
  errno = 0;
  past = read(fd, &byte, 0);
  printf("read 0: %zd errno=%d\n", past, errno);
  /* generic_file_llseek compares a SEEK_DATA or SEEK_HOLE offset with the
   * size, 0, as an unsigned number: every offset is past the end (ENXIO),
   * a negative one included. auxv seeks the same way. */
  int auxv = open("/proc/self/auxv", O_RDONLY | O_CLOEXEC);
  const struct {
    const char* name;
    int fd;
  } seekers[] = {{"cmdline", fd}, {"auxv", auxv}};
  for (size_t i = 0; i < sizeof(seekers) / sizeof(seekers[0]); i++) {
    const off_t requests[] = {-1, 0, 3};
    for (size_t r = 0; r < sizeof(requests) / sizeof(requests[0]); r++) {
      errno = 0;
      off_t data = lseek(seekers[i].fd, requests[r], SEEK_DATA);
      int data_errno = errno;
      errno = 0;
      off_t hole = lseek(seekers[i].fd, requests[r], SEEK_HOLE);
      printf("%s SEEK_DATA %lld: %lld errno=%d SEEK_HOLE: %lld errno=%d\n",
             seekers[i].name, (long long)requests[r], (long long)data,
             data_errno, (long long)hole, errno);
    }
  }
  if (auxv >= 0) {
    close(auxv);
  }
  lseek(fd, 0, SEEK_SET);
  errno = 0;
  ssize_t written = write(fd, "x", 1);
  printf("write: %zd errno=%d\n", written, errno);
  errno = 0;
  written = pwrite(fd, "x", 1, 0);
  printf("pwrite: %zd errno=%d\n", written, errno);
  errno = 0;
  written = pwrite(fd, "x", 0, -1);
  printf("pwrite at -1: %zd errno=%d\n", written, errno);
  close(fd);
}

/* splice from the file, which has no splice_read (fs/splice.c, v6.17): a
 * length of 0 is 0 and unknown flags EINVAL before the descriptors are
 * looked at; a destination that is not open or is O_PATH is EBADF; a pipe
 * destination with an output offset ESPIPE; an unreadable offset EFAULT; a
 * destination that cannot write EBADF; anything else EINVAL, for a pipe
 * with a reader and for a writable file alike. */
static void transfers(void) {
  int fd = open("/proc/self/cmdline", O_RDONLY | O_CLOEXEC);
  int pipes[2];
  int file = memfd_create("splice-destination", MFD_CLOEXEC);
  int readonly = open("/dev/null", O_RDONLY | O_CLOEXEC);
  int path_only = open("/dev/null", O_PATH | O_CLOEXEC);
  if (fd < 0 || pipe(pipes) != 0 || file < 0 || readonly < 0 || path_only < 0) {
    fail("open for splice");
    return;
  }
  loff_t offset = 0;
  loff_t* unreadable = (loff_t*)8;
  const struct {
    const char* label;
    int out;
    loff_t* off_in;
    loff_t* off_out;
    size_t len;
    unsigned flags;
  } cases[] = {
      {"to a pipe", pipes[1], NULL, NULL, 16, 0},
      {"to a pipe, input offset", pipes[1], &offset, NULL, 16, 0},
      {"to a pipe, output offset", pipes[1], NULL, &offset, 16, 0},
      {"to a pipe, unreadable input offset", pipes[1], unreadable, NULL, 16, 0},
      {"to a pipe's read end", pipes[0], NULL, NULL, 16, 0},
      {"to a pipe's read end, output offset", pipes[0], NULL, &offset, 16, 0},
      {"to a file", file, NULL, NULL, 16, 0},
      {"to a file, output offset", file, NULL, &offset, 16, 0},
      {"to a file, unreadable output offset", file, NULL, unreadable, 16, 0},
      {"to a read-only file", readonly, NULL, NULL, 16, 0},
      {"to an O_PATH descriptor", path_only, NULL, NULL, 16, 0},
      {"to no descriptor", -1, NULL, NULL, 16, 0},
      {"to no descriptor, length 0", -1, NULL, NULL, 0, 0},
      {"to no descriptor, unknown flags", -1, NULL, NULL, 16, 0x100},
  };
  for (size_t i = 0; i < sizeof(cases) / sizeof(cases[0]); i++) {
    errno = 0;
    ssize_t moved = splice(fd, cases[i].off_in, cases[i].out, cases[i].off_out,
                           cases[i].len, cases[i].flags);
    printf("splice %s: %zd errno=%d\n", cases[i].label, moved, errno);
  }
  close(fd);
  close(pipes[0]);
  close(pipes[1]);
  close(file);
  close(readonly);
  close(path_only);
}

static void spellings(void) {
  char path[64];
  snprintf(path, sizeof(path), "/proc/%d/cmdline", (int)getpid());
  show_path("own pid", path);
  show_path("thread-self", "/proc/thread-self/cmdline");
  snprintf(path, sizeof(path), "/proc/self/task/%d/cmdline", (int)gettid_());
  show_path("own task", path);
  /* Relative to a descriptor of /proc. */
  int anchor = open("/proc", O_RDONLY | O_DIRECTORY | O_CLOEXEC);
  if (anchor < 0) {
    fail("open /proc");
    return;
  }
  snprintf(path, sizeof(path), "%d/cmdline", (int)getpid());
  const char* relative[] = {"self/cmdline", path, "thread-self/cmdline"};
  for (size_t i = 0; i < sizeof(relative) / sizeof(relative[0]); i++) {
    unsigned char buffer[8192];
    char label[64];
    snprintf(label, sizeof(label), "relative to /proc: %s",
             i == 1 ? "<pid>/cmdline" : relative[i]);
    int fd = openat(anchor, relative[i], O_RDONLY | O_CLOEXEC);
    if (fd < 0) {
      printf("%s errno=%d\n", label, errno);
      continue;
    }
    int reads;
    ssize_t n = read_all(fd, buffer, sizeof(buffer), sizeof(buffer), &reads);
    if (n < 0) {
      printf("%s read errno=%d\n", label, errno);
    } else {
      show(label, buffer, n);
    }
    close(fd);
  }
  close(anchor);
}

/* Overwrite bytes of the argument area in place, as programs that rename
 * themselves do. */
static void overwrite(void) {
  char saved[8192];
  size_t area = (size_t)(env_end - arg_start);
  memcpy(saved, arg_start, area);

  arg_start[0] = 'C';
  show_self("argv[0] first byte changed");

  /* An inner NUL replaced, the last byte still NUL: the whole argument
   * area, exactly. */
  char* first_nul = arg_start + strlen(arg_start);
  *first_nul = '_';
  show_self("first separator replaced");
  memcpy(arg_start, saved, area);

  /* setproctitle: the last argument byte is not NUL. One string from
   * arg_start, through its NUL. */
  const char title[] = "title-one";
  memcpy(arg_start, title, sizeof(title));
  arg_end[-1] = 'Z';
  show_self("setproctitle short title");

  /* A title longer than the argument area runs on into the environment
   * area. */
  size_t long_length = (size_t)(arg_end - arg_start) + 20;
  memset(arg_start, 'L', long_length);
  arg_start[long_length] = '\0';
  show_self("setproctitle into environment");
  int fd = open("/proc/self/cmdline", O_RDONLY | O_CLOEXEC);
  if (fd >= 0) {
    unsigned char part[6];
    ssize_t got = pread(fd, part, sizeof(part), 4);
    show("setproctitle pread 6 at 4", part, got);
    got = pread(fd, part, sizeof(part), (off_t)long_length - 2);
    show("setproctitle pread 6 near the end", part, got);
    close(fd);
  }

  /* No NUL anywhere in the two areas: the title stops at the
   * environment area's end, without a NUL. */
  memset(arg_start, 'M', area);
  show_self("setproctitle filling both areas");

  memcpy(arg_start, saved, area);
  show_self("restored");
}

static void child_files(void) {
  int ready[2];
  int done[2];
  if (pipe(ready) != 0 || pipe(done) != 0) {
    fail("pipe");
    return;
  }
  pid_t child = fork();
  if (child < 0) {
    fail("fork");
    return;
  }
  if (child == 0) {
    arg_start[0] = 'K';
    char byte = 'r';
    if (write(ready[1], &byte, 1) != 1 || read(done[0], &byte, 1) != 1) {
      _exit(2);
    }
    _exit(0);
  }
  char byte;
  if (read(ready[0], &byte, 1) != 1) {
    fail("child ready");
  }
  char path[64];
  snprintf(path, sizeof(path), "/proc/%d/cmdline", (int)child);
  show_path("running child", path);
  /* A zombie's and a reaped child's file are not read here: on KVM their
   * answer depends on the host's timing, a defect of KVM's /proc beneath
   * Detcore (https://github.com/rrnewton/hermit/issues/4564). */
  if (write(done[1], &byte, 1) != 1) {
    fail("child done");
  }
  int status;
  if (waitpid(child, &status, 0) != child) {
    fail("waitpid");
  }
  close(ready[0]);
  close(ready[1]);
  close(done[0]);
  close(done[1]);
}

/* read(fd, rsp - 192, count) with the bytes copied to `out` in the same
 * asm block, before anything else can use the stack below rsp: a buffer
 * that Hermit's stack scratch for an injected call can overlap. */
static long read_below_stack_pointer(int fd, unsigned char* out, long count) {
  long result;
  __asm__ volatile(
      "lea -192(%%rsp), %%rsi\n\t"
      "mov %[fd], %%rdi\n\t"
      "mov %[count], %%rdx\n\t"
      "xor %%eax, %%eax\n\t"
      "syscall\n\t"
      "mov %%rax, %[result]\n\t"
      "lea -192(%%rsp), %%rsi\n\t"
      "mov %[out], %%rdi\n\t"
      "mov %[count], %%rcx\n\t"
      "rep movsb\n\t"
      : [result] "=&r"(result)
      : [fd] "r"((long)fd), [out] "r"(out), [count] "r"(count)
      : "rax", "rcx", "rdx", "rsi", "rdi", "r11", "memory");
  return result;
}

static void inherited(void) {
  int fd = open("/proc/self/cmdline", O_RDONLY | O_CLOEXEC);
  int go[2];
  if (fd < 0 || pipe(go) != 0) {
    fail("open to inherit");
    return;
  }
  pid_t child = fork();
  if (child == 0) {
    /* The descriptor names the parent, which rewrites its first argument
     * after the open and the fork: the child reads the parent's memory as
     * it is at the read. */
    char byte;
    if (read(go[0], &byte, 1) != 1) {
      _exit(2);
    }
    unsigned char buffer[8192];
    ssize_t n = read(fd, buffer, sizeof(buffer));
    if (n < 0) {
      printf("inherited descriptor, parent retitled errno=%d\n", errno);
    } else {
      show("inherited descriptor, parent retitled", buffer, n);
    }
    unsigned char below[64];
    memset(below, 0, sizeof(below));
    long m = read_below_stack_pointer(fd, below, 34);
    if (lseek(fd, 0, SEEK_SET) != 0) {
      _exit(3);
    }
    m = read_below_stack_pointer(fd, below, 34);
    if (m < 0) {
      printf("inherited descriptor, read below the stack pointer errno=%ld\n", -m);
    } else {
      show("inherited descriptor, read below the stack pointer", below, m);
    }
    fflush(stdout);
    _exit(0);
  }
  if (child < 0) {
    fail("inherited child");
    return;
  }
  arg_start[0] = 'P';
  char byte = 'g';
  if (write(go[1], &byte, 1) != 1) {
    fail("signal the child");
  }
  int status;
  if (waitpid(child, &status, 0) != child) {
    fail("inherited child");
  }
  arg_start[0] = 'c';
  close(fd);
  close(go[0]);
  close(go[1]);
}

/* read(fd, buffer, count) with the stack pointer at `sp` for the call
 * alone: Hermit's stack scratch for an injected call lies below the stack
 * pointer, here inside another process's argument area. */
static long read_with_stack_at(int fd, unsigned char* buffer, long count,
                               char* sp) {
  long result;
  __asm__ volatile(
      "mov %%rsp, %%r12\n\t"
      "mov %[sp], %%rsp\n\t"
      "mov %[fd], %%rdi\n\t"
      "mov %[buffer], %%rsi\n\t"
      "mov %[count], %%rdx\n\t"
      "xor %%eax, %%eax\n\t"
      "syscall\n\t"
      "mov %%r12, %%rsp\n\t"
      "mov %%rax, %[result]\n\t"
      : [result] "=&r"(result)
      : [fd] "r"((long)fd), [buffer] "r"(buffer), [count] "r"(count),
        [sp] "r"(sp)
      : "rax", "rcx", "rdx", "rsi", "rdi", "r11", "r12", "memory");
  return result;
}

static int clone_vm_fd;
static long clone_vm_result;
static unsigned char clone_vm_bytes[256];

static int clone_vm_child(void* unused) {
  (void)unused;
  /* 192 bytes into the parent's argument and environment area, which this
   * child shares (CLONE_VM): below that stack pointer lie the parent's
   * first argument bytes, which the kernel reads for this file. */
  char* sp = (char*)((unsigned long)(arg_start + 192) & ~15UL);
  clone_vm_result = read_with_stack_at(clone_vm_fd, clone_vm_bytes,
                                       sizeof(clone_vm_bytes), sp);
  return 0;
}

/* A CLONE_VM child (another process sharing this one's memory) reads this
 * process's file through an inherited descriptor, with its stack pointer
 * inside this process's argument area. */
static void clone_vm_reader(void) {
  if (env_end - arg_start < 256) {
    printf("clone-vm reader: argument area %td bytes, too short\n",
           env_end - arg_start);
    return;
  }
  clone_vm_fd = open("/proc/self/cmdline", O_RDONLY | O_CLOEXEC);
  size_t size = 64 * 1024;
  char* stack = malloc(size);
  if (clone_vm_fd < 0 || stack == NULL) {
    fail("clone-vm reader setup");
    return;
  }
  fflush(stdout);
  pid_t child = clone(clone_vm_child, stack + size, CLONE_VM | SIGCHLD, NULL);
  int status;
  if (child < 0 || waitpid(child, &status, 0) != child) {
    fail("clone-vm reader");
  } else if (clone_vm_result < 0) {
    printf("clone-vm reader, stack pointer in the arguments errno=%ld\n",
           -clone_vm_result);
  } else {
    show("clone-vm reader, stack pointer in the arguments", clone_vm_bytes,
         clone_vm_result);
  }
  free(stack);
  close(clone_vm_fd);
}

/* The page that holds the last argument byte made PROT_NONE: Linux reads
 * the argument area only where the process can read it (access_remote_vm
 * without FOLL_FORCE), so the file reads nothing. In a child, which
 * restores the protection before it prints. */
static void protected_page(void) {
  fflush(stdout);
  pid_t child = fork();
  if (child == 0) {
    long page = sysconf(_SC_PAGESIZE);
    char* last = arg_end - 1;
    void* start = (void*)((unsigned long)last & ~(unsigned long)(page - 1));
    static unsigned char buffer[8192];
    int fd = open("/proc/self/cmdline", O_RDONLY | O_CLOEXEC);
    if (fd < 0 || mprotect(start, (size_t)page, PROT_NONE) != 0) {
      _exit(2);
    }
    ssize_t n = read(fd, buffer, sizeof(buffer));
    int error = errno;
    if (mprotect(start, (size_t)page, PROT_READ | PROT_WRITE) != 0) {
      _exit(3);
    }
    if (n < 0) {
      printf("argument page PROT_NONE errno=%d\n", error);
    } else {
      show("argument page PROT_NONE", buffer, n);
    }
    fflush(stdout);
    _exit(0);
  }
  int status;
  if (child < 0 || waitpid(child, &status, 0) != child || status != 0) {
    fail("protected-page child");
  }
}

static pid_t thread_tid;
static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t changed = PTHREAD_COND_INITIALIZER;
static int stage;

static void wait_for(int wanted) {
  pthread_mutex_lock(&lock);
  while (stage < wanted) {
    pthread_cond_wait(&changed, &lock);
  }
  pthread_mutex_unlock(&lock);
}

static void advance(int to) {
  pthread_mutex_lock(&lock);
  stage = to;
  pthread_cond_broadcast(&changed);
  pthread_mutex_unlock(&lock);
}

static void* thread_body(void* unused) {
  (void)unused;
  thread_tid = gettid_();
  char path[64];
  snprintf(path, sizeof(path), "/proc/self/task/%d/cmdline", (int)thread_tid);
  show_path("thread's own task file", path);
  show_path("thread's thread-self", "/proc/thread-self/cmdline");
  advance(1);
  /* Stay until the main thread has read this thread's file. */
  wait_for(2);
  return NULL;
}

static void threads(void) {
  pthread_t thread;
  if (pthread_create(&thread, NULL, thread_body, NULL) != 0) {
    fail("pthread_create");
    return;
  }
  wait_for(1);
  char path[64];
  snprintf(path, sizeof(path), "/proc/self/task/%d/cmdline", (int)thread_tid);
  int fd = open(path, O_RDONLY | O_CLOEXEC);
  show_path("main reads the thread's task file", path);
  advance(2);
  pthread_join(thread, NULL);
  if (fd >= 0) {
    /* A joined thread has cleared its thread ID, but Linux releases it a
     * little later: read until its file reports that it is gone, a bounded
     * number of times. */
    unsigned char buffer[64];
    ssize_t n = 0;
    for (int attempt = 0; attempt < 1000; attempt++) {
      n = pread(fd, buffer, sizeof(buffer), 0);
      if (n < 0) {
        break;
      }
      struct timespec pause = {0, 1000000};
      nanosleep(&pause, NULL);
    }
    if (n < 0) {
      printf("joined thread's open task file errno=%d\n", errno);
    } else {
      show("joined thread's open task file", buffer, n);
    }
    close(fd);
  }
}

static void* survivor_body(void* unused) {
  (void)unused;
  /* Let the leader exit first: its exit is the only event in this process
   * meanwhile. Then its top-level file reads nothing (Linux frees the
   * leader's memory map at its exit), read again a bounded number of times
   * should the native leader not have finished, and this thread's own file
   * reads the process's arguments. */
  struct timespec pause = {0, 100000000};
  nanosleep(&pause, NULL);
  unsigned char buffer[8192];
  ssize_t n = read_path("/proc/self/cmdline", buffer, sizeof(buffer));
  for (int attempt = 0; attempt < 1000 && n > 0; attempt++) {
    struct timespec retry = {0, 1000000};
    nanosleep(&retry, NULL);
    n = read_path("/proc/self/cmdline", buffer, sizeof(buffer));
  }
  if (n < 0) {
    printf("leader exited, /proc/self/cmdline errno=%d\n", errno);
  } else {
    show("leader exited, /proc/self/cmdline", buffer, n);
  }
  show_path("leader exited, thread-self", "/proc/thread-self/cmdline");
  fflush(stdout);
  exit(0);
}

static void leader_exits(void) {
  fflush(stdout);
  pid_t child = fork();
  if (child == 0) {
    pthread_t thread;
    if (pthread_create(&thread, NULL, survivor_body, NULL) != 0) {
      _exit(3);
    }
    pthread_exit(NULL);
  }
  int status;
  if (child < 0 || waitpid(child, &status, 0) != child) {
    fail("leader-exit child");
  }
}

int main(int argc, char** argv) {
  setvbuf(stdout, NULL, _IOLBF, 0);
  if (argc < 2 ||
      (strcmp(argv[1], "stage1") != 0 && strcmp(argv[1], "stage2") != 0)) {
    /* The other-process variant (a guest argument): another process's file,
     * which only the kernel beneath can serve; its last argument tells
     * stage1 to read them. */
#ifndef PROC_CMDLINE_OTHER_PROCESSES
#define PROC_CMDLINE_OTHER_PROCESSES 0
#endif
    int others = PROC_CMDLINE_OTHER_PROCESSES ||
                 (argc > 1 && strcmp(argv[1], "other-processes") == 0);
    char* const next_argv[] = {"cmdline-probe", "stage1", "", "two words",
                               others ? "o" : "x", NULL};
    /* The other-process variant pads its environment, so that the argument
     * and environment area is long enough for clone_vm_reader. */
    char* const next_envp[] = {
        "PROBE_ENV_ONE=0123456789", "PROBE_ENV_TWO=abcdefghijklmnopqrstuvwxyz",
        others ? "PROBE_ENV_PAD="
                 "pppppppppppppppppppppppppppppppppppppppppppppppppppppppppppp"
                 "pppppppppppppppppppppppppppppppppppppppppppppppppppppppppppp"
                 "pppppppppppppppppppppppppppppppppppppppppppppppppppppppppppp"
                 "pppppppppppppppppppppppppppppppppppppppppppppppppppppppppppp"
               : NULL,
        NULL};
    /* By the path that started this program: KVM keeps no executable for
     * /proc/self/exe to name in an image Hermit loaded itself. */
    execve(argv[0], next_argv, next_envp);
    fail("execve stage1");
    return 1;
  }
  if (strcmp(argv[1], "stage2") == 0) {
    find_areas(argc, argv);
    show_self("after a second exec");
    int fd = atoi(argv[2]);
    unsigned char buffer[8192];
    if (lseek(fd, 0, SEEK_SET) != 0) {
      fail("lseek the kept descriptor");
    }
    ssize_t n = read(fd, buffer, sizeof(buffer));
    if (n < 0) {
      printf("descriptor opened before exec errno=%d\n", errno);
    } else {
      show("descriptor opened before exec", buffer, n);
    }
    printf("failures=%d\n", failures);
    return failures == 0 ? 0 : 1;
  }

  find_areas(argc, argv);
  show_self("own");
  offsets();
  spellings();
  overwrite();
  transfers();
  if (strcmp(argv[4], "o") == 0) {
    child_files();
    inherited();
    clone_vm_reader();
  }
  protected_page();
  threads();
  leader_exits();

  /* A descriptor without O_CLOEXEC, read after a second exec of this
   * process: it names the process, so it reads the new image's arguments. */
  int kept = open("/proc/self/cmdline", O_RDONLY);
  if (kept < 0) {
    fail("open to keep");
    return 1;
  }
  unsigned char first[4];
  if (read(kept, first, sizeof(first)) != sizeof(first)) {
    fail("read before exec");
  }
  char number[16];
  snprintf(number, sizeof(number), "%d", kept);
  char* const next_argv[] = {"second-image", "stage2", number, "", "", "end",
                             NULL};
  char* const next_envp[] = {"PROBE_ENV_THREE=3", NULL};
  printf("failures before exec=%d\n", failures);
  fflush(stdout);
  execve("/proc/self/exe", next_argv, next_envp);
  fail("execve stage2");
  return 1;
}
