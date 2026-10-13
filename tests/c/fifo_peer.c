/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/* A FIFO whose other end is held by another guest thread or process
 * (https://github.com/rrnewton/hermit/issues/4200), and one whose other end is
 * a host process.
 *
 *   fifo_peer thread DIR    a reader thread blocks on an empty FIFO; the main
 *                           thread writes one byte
 *   fifo_peer process DIR   a forked child blocks reading; the parent writes
 *   fifo_peer fill DIR      a writer thread writes more than the FIFO holds;
 *                           the main thread reads it all
 *   fifo_peer shared DIR    parent and forked child share the read end; the
 *                           child's read waits first, then the parent's
 *   fifo_peer vector DIR    a reader thread's readv waits for the main
 *                           thread's writev; then a writer thread's writev of
 *                           more than the FIFO holds waits for the main thread
 *   fifo_peer signal DIR    the main thread signals a reader thread blocked on
 *                           the FIFO (handler without SA_RESTART): EINTR
 *   fifo_peer open-writer-first DIR
 *                           a parent's blocking open for writing waits for
 *                           its child's blocking open for reading
 *   fifo_peer open-reader-first DIR
 *                           the same, the child's reader opening first
 *   fifo_peer open-beside-pipe DIR
 *                           the parent's blocking open for reading waits while
 *                           one child fills a pipe another drains; the filler
 *                           then opens the FIFO for writing
 *   fifo_peer open-eintr DIR
 *                           a child signals the parent waiting in a blocking
 *                           open (handler without SA_RESTART): EINTR
 *   fifo_peer open-restart DIR
 *                           the same with SA_RESTART; the child then opens
 *                           the writer, and the restarted open completes
 *   fifo_peer open-ignored DIR
 *                           the same with SIGUSR1 ignored
 *   fifo_peer open-two-signals DIR
 *                           a child sends SIGUSR1 and SIGUSR2 to the parent
 *                           waiting in the open: both handlers run, EINTR
 *   fifo_peer open-reader-closes DIR
 *                           a child opens the read end nonblocking and closes
 *                           it while the parent's open for writing waits: the
 *                           open completes, and the write fails with EPIPE
 *   fifo_peer open-unpublished-reader|open-unpublished-writer DIR
 *                           while a thread's blocking open waits, the number it
 *                           will get is still free (F_GETFD: EBADF)
 *   fifo_peer open-after-sleep DIR
 *                           a child sleeps 100 s, then opens the writer the
 *                           parent's blocking open for reading waits for
 *   fifo_peer open-renamed|open-renamed-new|open-linked-unlinked DIR
 *                           a child waits to open a FIFO made here for reading;
 *                           the parent renames it (then makes a new FIFO at the
 *                           old name), or links and unlinks it, and writes to
 *                           its new name: the child reads that byte
 *   fifo_peer open-without-ptrace-capability|open-without-ptrace-capability-writer DIR
 *                           a child without capabilities (so not
 *                           CAP_SYS_PTRACE) waits to open a FIFO made here,
 *                           for reading (for writing); the parent opens the
 *                           other end and writes (reads) one byte
 *   fifo_peer open-chrooted|open-chrooted-writer DIR
 *                           the same, chrooted to DIR, which has no /proc
 *   fifo_peer open-on-a-tight-stack DIR
 *                           a child waits to open a FIFO made here for
 *                           reading, by a raw openat whose stack pointer is
 *                           16 bytes above an inaccessible page; the parent
 *                           renames it and writes one byte to its new name
 *   fifo_peer open-leaves-no-mapping DIR
 *                           the main thread waits to open a FIFO made here for
 *                           reading; a child renames it and writes one byte to
 *                           its new name; /proc/self/maps is the same before
 *                           and after the open
 *   fifo_peer open-killed-beside-a-sharer DIR
 *                           a CLONE_VM child waits to open a FIFO made here,
 *                           which nothing opens, while the parent waits for it;
 *                           a host process kills the child (SIGKILL), and the
 *                           parent's /proc/self/maps is the same as before
 *   fifo_peer open-killed-then-its-cleaner-killed DIR
 *                           two CLONE_VM children: "opener" waits to open a
 *                           FIFO made here for reading, "cleaner" opens it for
 *                           writing and then sleeps; a host process kills the
 *                           opener (with its staging page mapped) and then the
 *                           cleaner (as it reclaims that page); the parent's
 *                           /proc/self/maps is the same as before
 *   fifo_peer open-killed-in-a-spawned-child DIR
 *                           the same with a posix_spawn child (CLONE_VM and
 *                           CLONE_VFORK), whose file action opens the FIFO
 *   fifo_peer open-chrooted-beside-a-fake-proc DIR
 *                           chrooted to DIR, whose /proc/1/fd/0..1023 are
 *                           regular files, a child waits to open /fifo for
 *                           reading; the parent writes one byte
 *   fifo_peer open-writer-chrooted-beside-a-fake-proc DIR
 *                           the same with FIFOs at /proc/1/fd/N, and a child
 *                           waiting to open /fifo for writing
 *   fifo_peer open-renamed-without-ptrace-capability DIR
 *                           a child without capabilities waits to open a FIFO
 *                           made here for reading; the parent renames it and
 *                           writes to its new name
 *   fifo_peer open-after-link-removed|open-after-name-removed|open-after-rename-between-links|open-after-self-rename DIR
 *                           a FIFO made here keeps a name (linked twice, then
 *                           one link removed or renamed onto the other; or
 *                           renamed onto itself); then parent and child
 *                           rendezvous on it
 *   fifo_peer guest-made-host-reader-renamed DIR
 *                           the same, while a thread renames the FIFO to
 *                           DIR/other, which the host opens
 *   fifo_peer guest-made-host-reader DIR
 *                           a blocking open for writing of a FIFO made here,
 *                           whose reader is a host process that opens later,
 *                           beside a thread waiting on an empty pipe
 *   fifo_peer exclusive-reader-beside-host DIR
 *                           O_RDONLY|O_CREAT|O_EXCL on a FIFO made here, while
 *                           a host writer waits in its open: EEXIST, and the
 *                           host writer keeps waiting
 *   fifo_peer open-exclusive DIR
 *                           a thread's O_WRONLY|O_CREAT|O_EXCL on an existing
 *                           FIFO with no reader fails with EEXIST at once,
 *                           while the main thread yields until it returns
 *   fifo_peer host PATH     read one byte from PATH, a FIFO a host process
 *                           writes
 *   fifo_peer host-rdwr PATH
 *                           the same through a read-write descriptor
 *                           (`exec 3<>fifo`), which is its own writer
 *   fifo_peer host-readers PATH
 *                           the same through one of two read-only
 *                           descriptions: the other is a reader, not a writer
 *   fifo_peer private-table PATH DIR
 *                           the leader holds another FIFO, with a guest
 *                           writer, at fd 10; a thread cloned without
 *                           CLONE_FILES puts PATH, which only a host process
 *                           writes, at its own fd 10 and reads one byte
 *   fifo_peer private-table-guest DIR
 *                           the same, but the leader writes the thread's FIFO
 *   fifo_peer private-writer DIR
 *                           the leader reads a FIFO whose only writer a
 *                           thread cloned without CLONE_FILES opened in its
 *                           own table
 *   fifo_peer host-reader-opath PATH
 *                           the same, with an O_PATH handle on PATH opened
 *                           first: it is neither a reader nor a writer
 *   fifo_peer open-host PATH
 *                           a blocking open of PATH for reading, whose only
 *                           writer is a host process, then read one byte
 *   fifo_peer open-host-writer PATH
 *                           a blocking open of PATH for writing, whose only
 *                           reader is a host process, then write one byte
 *   fifo_peer open-host-beside-poller PATH
 *                           the same while another thread waits to read an
 *                           empty pipe that only the main thread writes
 *   fifo_peer open-host-beside-sleeper PATH
 *                           open-host while another thread sleeps for 100 s
 *   fifo_peer open-host-beside-yielder PATH
 *                           open-host while another thread yields 3000 times
 *   fifo_peer open-host-beside-spinner PATH
 *                           open-host while another thread yields until the
 *                           open returns; the host holds PATH open for writing
 *                           from the start but writes only later
 *   fifo_peer open-host-order PATH
 *                           open-host while a thread yields 50 times and
 *                           prints S; the main thread prints M
 *   fifo_peer open-host-eof PATH
 *                           open-host while a thread yields 15000 times; the
 *                           host writer opens and closes without writing
 *   fifo_peer open-host-exclusive-reader PATH
 *                           O_RDONLY|O_CREAT|O_EXCL on PATH, which exists
 *   fifo_peer host-beside-yielder PATH
 *                           a thread opens PATH, already open on the host,
 *                           for writing while the main thread yields
 *   fifo_peer host-beside-pause PATH
 *                           the main thread opens PATH, already open on the
 *                           host, for writing while a thread waits in pause
 *   fifo_peer host-reader PATH
 *                           write more than the FIFO holds to PATH, which only
 *                           a host process reads
 *
 * Each mode prints what it read, as Linux gives it.
 */
#define _GNU_SOURCE
#include <fcntl.h>
#include <errno.h>
#include <sched.h>
#include <signal.h>
#include <spawn.h>
#include <sys/syscall.h>
#include <linux/capability.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/prctl.h>
#include <sys/stat.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <unistd.h>

#define FILL_BYTES (3 * 65536 + 123)

static int rfd = -1, wfd = -1;

static void die(const char *what) {
  perror(what);
  exit(2);
}

/* Both ends in this process, the read end blocking, as a shell's `mkfifo`
 * followed by `exec 3<>` would leave them. */
static void open_both_ends(const char *dir) {
  char path[4096];
  snprintf(path, sizeof path, "%s/fifo", dir);
  unlink(path);
  if (mkfifo(path, 0600) != 0) die("mkfifo");
  rfd = open(path, O_RDONLY | O_NONBLOCK);
  if (rfd < 0) die("open read end");
  wfd = open(path, O_WRONLY);
  if (wfd < 0) die("open write end");
  if (fcntl(rfd, F_SETFL, fcntl(rfd, F_GETFL) & ~O_NONBLOCK) != 0) die("F_SETFL");
  unlink(path);
}

static volatile sig_atomic_t handled;

static void on_signal(int signo) {
  (void)signo;
  handled++;
}

static void *read_one_reporting_errno(void *arg) {
  char b = '-';
  ssize_t n = read(rfd, &b, 1);
  int err = errno;
  printf("thread read: %zd %s, handled %d\n", n, n < 0 ? strerror(err) : "ok", (int)handled);
  return arg;
}

static void *read_one(void *arg) {
  char b = '-';
  ssize_t n = read(rfd, &b, 1);
  printf("thread read: %zd '%c'\n", n, b);
  return arg;
}

static void *write_b_later(void *arg) {
  usleep(1000);
  if (write(wfd, "b", 1) != 1) die("write b");
  return arg;
}

static void *readv_two(void *arg) {
  char a = '-', b = '-';
  struct iovec iov[2] = {{&a, 1}, {&b, 1}};
  ssize_t n = readv(rfd, iov, 2);
  printf("thread readv: %zd '%c%c'\n", n, a, b);
  return arg;
}

static void *writev_fill(void *arg) {
  static char buf[FILL_BYTES];
  for (size_t i = 0; i < sizeof buf; i++) buf[i] = (char)(i * 7);
  struct iovec iov[2] = {{buf, 1000}, {buf + 1000, sizeof buf - 1000}};
  ssize_t n = writev(wfd, iov, 2);
  printf("thread writev: %zd\n", n);
  close(wfd);
  return arg;
}

static void read_all(const char *who) {
  static char buf[FILL_BYTES];
  size_t total = 0;
  unsigned sum = 0;
  for (;;) {
    ssize_t n = read(rfd, buf, sizeof buf);
    if (n < 0) die("read");
    if (n == 0) break;
    for (ssize_t i = 0; i < n; i++) sum = sum * 31 + (unsigned char)buf[i];
    total += (size_t)n;
  }
  printf("%s read: %zu bytes, sum %u\n", who, total, sum);
}

/* A thread with its own descriptor table, which uses raw system calls only:
 * it shares the leader's thread-local storage. */
static const char *private_fifo;
static volatile int private_done;
static char private_byte = '-';
static long private_read = -2;

static int private_thread(void *arg) {
  (void)arg;
  syscall(SYS_close, 10);
  long fd = syscall(SYS_openat, AT_FDCWD, private_fifo, O_RDONLY | O_NONBLOCK);
  if (fd != 10) {
    private_read = -100 - fd; /* the hole filling failed */
    __atomic_store_n(&private_done, 1, __ATOMIC_SEQ_CST);
    return 0;
  }
  syscall(SYS_fcntl, 10, F_SETFL, 0);
  private_read = syscall(SYS_read, 10, &private_byte, 1);
  __atomic_store_n(&private_done, 1, __ATOMIC_SEQ_CST);
  return 0;
}

/* The only writer, opened in a thread's own table. */
static volatile int private_writer_open;

static int private_writer_thread(void *arg) {
  (void)arg;
  long fd = syscall(SYS_openat, AT_FDCWD, private_fifo, O_WRONLY);
  __atomic_store_n(&private_writer_open, 1, __ATOMIC_SEQ_CST);
  struct timespec pause = {0, 2000000};
  syscall(SYS_nanosleep, &pause, NULL);
  if (fd >= 0) {
    syscall(SYS_write, fd, "w", 1);
    syscall(SYS_close, fd);
  }
  __atomic_store_n(&private_done, 1, __ATOMIC_SEQ_CST);
  return 0;
}

static void private_writer(const char *dir) {
  char path[4096];
  snprintf(path, sizeof path, "%s/writer-fifo", dir);
  unlink(path);
  if (mkfifo(path, 0600) != 0) die("mkfifo");
  rfd = open(path, O_RDONLY | O_NONBLOCK);
  if (rfd < 0) die("open the FIFO for reading");
  if (fcntl(rfd, F_SETFL, 0) != 0) die("F_SETFL");
  private_fifo = path;
  static char stack[65536];
  int flags = CLONE_VM | CLONE_FS | CLONE_SIGHAND | CLONE_THREAD | CLONE_SYSVSEM;
  if (clone(private_writer_thread, stack + sizeof stack, flags, NULL) < 0) die("clone");
  /* A read with no writer would return 0 at once. */
  while (!__atomic_load_n(&private_writer_open, __ATOMIC_SEQ_CST)) usleep(1000);
  char b = '-';
  ssize_t n = read(rfd, &b, 1);
  while (!__atomic_load_n(&private_done, __ATOMIC_SEQ_CST)) usleep(1000);
  printf("leader read: %zd '%c'\n", n, b);
  unlink(path);
}

static void private_table(const char *thread_fifo, const char *dir, int leader_writes) {
  /* The leader's FIFO at fd 10, with a guest writer. */
  open_both_ends(dir);
  if (dup2(rfd, 10) != 10) die("dup2");
  close(rfd);
  /* Fill every lower number, so that the thread's open after it closes its
   * fd 10 returns 10 itself: the open's own handling then meets the number
   * the leader uses for the other FIFO. */
  for (;;) {
    int filler = open("/dev/null", O_RDONLY);
    if (filler < 0) die("open /dev/null");
    if (filler > 10) {
      close(filler);
      break;
    }
  }
  /* The guest writer of the thread's FIFO, opened read-write so the open
   * does not wait; the thread's copied table holds it too. */
  int thread_writer = leader_writes ? open(thread_fifo, O_RDWR) : -1;
  if (leader_writes && thread_writer < 0) die("open the thread's FIFO");
  private_fifo = thread_fifo;
  static char stack[65536];
  int flags = CLONE_VM | CLONE_FS | CLONE_SIGHAND | CLONE_THREAD | CLONE_SYSVSEM;
  if (clone(private_thread, stack + sizeof stack, flags, NULL) < 0) die("clone");
  if (leader_writes) {
    usleep(2000);
    if (write(thread_writer, "g", 1) != 1) die("write");
  }
  while (!__atomic_load_n(&private_done, __ATOMIC_SEQ_CST)) usleep(1000);
  printf("thread read: %ld '%c'\n", private_read, private_byte);
}

static volatile sig_atomic_t usr1, usr2;

static void on_usr1(int signo) {
  (void)signo;
  usr1 = 1;
}

static void on_usr2(int signo) {
  (void)signo;
  usr2 = 1;
}

static char made_fifo[4096];

/* Make DIR/fifo afresh and return its path; main removes it at the end, so a
 * verifying second run starts from the same directory. */
static const char *fresh_fifo(const char *dir) {
  snprintf(made_fifo, sizeof made_fifo, "%s/fifo", dir);
  unlink(made_fifo);
  if (mkfifo(made_fifo, 0600) != 0) die("mkfifo");
  return made_fifo;
}

/* openat(AT_FDCWD, path, flags) with the stack pointer at sp. No
 * instruction between the stack switch and its restoration touches the
 * stack; the original rsp is kept in r12, whose own value is saved on the
 * original stack. */
extern long openat_on_stack(void *sp, const char *path, long flags);
__asm__(".text\n"
        ".globl openat_on_stack\n"
        ".type openat_on_stack,@function\n"
        "openat_on_stack:\n"
        "push %r12\n"
        "mov %rsp,%r12\n"
        "mov %rdi,%rsp\n"
        "mov $-100,%rdi\n"
        "xor %r10d,%r10d\n"
        "mov $257,%eax\n"
        "syscall\n"
        "mov %r12,%rsp\n"
        "pop %r12\n"
        "ret\n"
        ".size openat_on_stack,.-openat_on_stack\n");

/* Read /proc/self/maps into buf (at most size - 1 bytes, NUL-terminated). */
static void read_maps(char *buf, size_t size) {
  int fd = open("/proc/self/maps", O_RDONLY | O_CLOEXEC);
  if (fd < 0) die("open maps");
  size_t used = 0;
  ssize_t n;
  while (used + 1 < size && (n = read(fd, buf + used, size - 1 - used)) > 0) used += (size_t)n;
  buf[used] = '\0';
  close(fd);
}

static char maps_before[1 << 16];
static char maps_after[1 << 16];
static const char *sharer_path;

static int open_as_opener(void *arg) {
  (void)arg;
  prctl(PR_SET_NAME, "opener");
  int fd = open(sharer_path, O_RDONLY);
  return fd < 0 ? 1 : 0;
}

static int open_as_cleaner(void *arg) {
  (void)arg;
  prctl(PR_SET_NAME, "cleaner");
  usleep(200000);
  int fd = open(sharer_path, O_WRONLY);
  for (int i = 0; i < 100; i++) usleep(100000);
  return fd < 0 ? 1 : 0;
}

static int open_in_a_sharer(void *arg) {
  (void)arg;
  int fd = open(sharer_path, O_RDONLY);
  return fd < 0 ? 1 : 0;
}

/* Drop every capability, CAP_SYS_PTRACE among them. */
static void drop_capabilities(void) {
  struct __user_cap_header_struct header = {_LINUX_CAPABILITY_VERSION_3, 0};
  struct __user_cap_data_struct data[2];
  memset(data, 0, sizeof data);
  if (syscall(SYS_capset, &header, data) != 0) die("capset");
}

static void *sleep_long(void *arg) {
  (void)arg;
  sleep(100);
  return NULL;
}

static const char *pending_path;
static int pending_writer;
static volatile int pending_started, pending_finished;
static int pending_fd = -1;

static void *open_pending(void *arg) {
  (void)arg;
  pending_started = 1;
  pending_fd = open(pending_path, (pending_writer ? O_WRONLY : O_RDONLY) | O_CLOEXEC);
  if (pending_fd < 0) die("pending open");
  char c = 'u';
  if (pending_writer ? write(pending_fd, &c, 1) != 1 : read(pending_fd, &c, 1) != 1) die("pending io");
  pending_finished = 1;
  return NULL;
}

static const char *ready_path;
static volatile int ready_done;
static int ready_fd = -1;

static void *open_ready_writer(void *arg) {
  (void)arg;
  ready_fd = open(ready_path, O_WRONLY);
  ready_done = 1;
  return NULL;
}

static const char *exclusive_path;
static volatile int exclusive_done;
static int exclusive_fd, exclusive_errno;

static void *open_exclusive(void *arg) {
  (void)arg;
  exclusive_fd = open(exclusive_path, O_WRONLY | O_CREAT | O_EXCL, 0600);
  exclusive_errno = errno;
  exclusive_done = 1;
  return NULL;
}

static void *yield_a_while(void *arg) {
  (void)arg;
  for (int i = 0; i < 3000; i++) sched_yield();
  return NULL;
}

static volatile int spinner_stop;

static void *yield_until_stopped(void *arg) {
  (void)arg;
  while (!spinner_stop) sched_yield();
  return NULL;
}

static void *yield_then_say_s(void *arg) {
  (void)arg;
  for (int i = 0; i < 50; i++) sched_yield();
  printf("S\n");
  fflush(stdout);
  return NULL;
}

static void *yield_15000(void *arg) {
  (void)arg;
  for (int i = 0; i < 15000; i++) sched_yield();
  return NULL;
}

static void *read_poller_pipe(void *arg);

static char rename_from[4096], rename_to[4096], renamed_marker[4096];

/* Rename the FIFO main waits on, say so, then wait on the empty pipe. */
static void *rename_then_read_poller_pipe(void *arg) {
  usleep(100000);
  if (rename(rename_from, rename_to) != 0) die("rename");
  int m = open(renamed_marker, O_WRONLY | O_CREAT, 0600);
  if (m < 0) die("renamed marker");
  close(m);
  return read_poller_pipe(arg);
}

static volatile int paused_started;

static void *wait_in_pause(void *arg) {
  (void)arg;
  paused_started = 1;
  while (!usr1) pause();
  return NULL;
}

static int poller_pipe[2];

static void *read_poller_pipe(void *arg) {
  char b = '-';
  ssize_t n = read(poller_pipe[0], &b, 1);
  printf("thread read: %zd '%c'\n", n, b);
  return arg;
}

static void *write_fill(void *arg) {
  static char buf[FILL_BYTES];
  for (size_t i = 0; i < sizeof buf; i++) buf[i] = (char)(i * 7);
  ssize_t n = write(wfd, buf, sizeof buf);
  printf("thread wrote: %zd\n", n);
  close(wfd);
  return arg;
}

int main(int argc, char **argv) {
  if (argc != 3 && !(argc == 4 && strcmp(argv[1], "private-table") == 0)) {
    fprintf(stderr, "usage: fifo_peer thread|signal|process|fill|shared|vector|open-writer-first|open-reader-first|open-beside-pipe|open-eintr|open-restart|open-ignored|open-two-signals|open-reader-closes|open-unpublished-reader|open-unpublished-writer|open-exclusive|open-after-sleep|exclusive-reader-beside-host|open-renamed|open-renamed-new|open-linked-unlinked|open-after-link-removed|open-after-name-removed|open-after-rename-between-links|open-after-self-rename|open-without-ptrace-capability|open-without-ptrace-capability-writer|open-chrooted|open-chrooted-writer|open-on-a-tight-stack|open-leaves-no-mapping|open-killed-beside-a-sharer|open-killed-in-a-spawned-child|open-killed-then-its-cleaner-killed|open-chrooted-beside-a-fake-proc|open-writer-chrooted-beside-a-fake-proc|open-renamed-without-ptrace-capability|guest-made-host-reader|guest-made-host-reader-renamed DIR | host|host-rdwr|host-readers|host-reader|host-reader-opath|open-host|open-host-writer|open-host-beside-poller|open-host-beside-sleeper|open-host-beside-yielder|open-host-beside-spinner|host-beside-yielder|host-beside-pause|open-host-order|open-host-eof|open-host-exclusive-reader PATH | private-table PATH DIR | private-table-guest DIR | private-writer DIR\n");
    return 2;
  }
  const char *mode = argv[1];
  setvbuf(stdout, NULL, _IONBF, 0);
  if (strcmp(mode, "thread") == 0) {
    open_both_ends(argv[2]);
    pthread_t t;
    if (pthread_create(&t, NULL, read_one, NULL) != 0) die("pthread_create");
    usleep(1000);
    if (write(wfd, "x", 1) != 1) die("write");
    pthread_join(t, NULL);
  } else if (strcmp(mode, "signal") == 0) {
    open_both_ends(argv[2]);
    struct sigaction action;
    memset(&action, 0, sizeof action);
    action.sa_handler = on_signal; /* no SA_RESTART */
    if (sigaction(SIGUSR1, &action, NULL) != 0) die("sigaction");
    pthread_t t;
    if (pthread_create(&t, NULL, read_one_reporting_errno, NULL) != 0) die("pthread_create");
    usleep(2000);
    if (pthread_kill(t, SIGUSR1) != 0) die("pthread_kill");
    pthread_join(t, NULL);
  } else if (strcmp(mode, "process") == 0) {
    open_both_ends(argv[2]);
    pid_t child = fork();
    if (child < 0) die("fork");
    if (child == 0) {
      close(wfd);
      char b = '-';
      ssize_t n = read(rfd, &b, 1);
      printf("child read: %zd '%c'\n", n, b);
      _exit(0);
    }
    close(rfd);
    usleep(1000);
    if (write(wfd, "y", 1) != 1) die("write");
    int status;
    if (waitpid(child, &status, 0) != child) die("waitpid");
  } else if (strcmp(mode, "fill") == 0) {
    open_both_ends(argv[2]);
    pthread_t t;
    if (pthread_create(&t, NULL, write_fill, NULL) != 0) die("pthread_create");
    usleep(1000);
    static char buf[FILL_BYTES];
    size_t total = 0;
    unsigned sum = 0;
    for (;;) {
      ssize_t n = read(rfd, buf, sizeof buf);
      if (n < 0) die("read");
      if (n == 0) break;
      for (ssize_t i = 0; i < n; i++) sum = sum * 31 + (unsigned char)buf[i];
      total += (size_t)n;
    }
    pthread_join(t, NULL);
    printf("main read: %zu bytes, sum %u\n", total, sum);
  } else if (strcmp(mode, "shared") == 0) {
    open_both_ends(argv[2]);
    pid_t child = fork();
    if (child < 0) die("fork");
    if (child == 0) {
      char b = '-';
      ssize_t n = read(rfd, &b, 1);
      printf("child read: %zd '%c'\n", n, b);
      _exit(0);
    }
    usleep(1000);
    if (write(wfd, "a", 1) != 1) die("write a");
    int status;
    if (waitpid(child, &status, 0) != child) die("waitpid");
    pthread_t t;
    if (pthread_create(&t, NULL, write_b_later, NULL) != 0) die("pthread_create");
    char b = '-';
    ssize_t n = read(rfd, &b, 1);
    printf("parent read: %zd '%c'\n", n, b);
    pthread_join(t, NULL);
  } else if (strcmp(mode, "vector") == 0) {
    open_both_ends(argv[2]);
    pthread_t t;
    if (pthread_create(&t, NULL, readv_two, NULL) != 0) die("pthread_create");
    usleep(1000);
    struct iovec iov[2] = {{"a", 1}, {"b", 1}};
    if (writev(wfd, iov, 2) != 2) die("writev");
    pthread_join(t, NULL);
    if (pthread_create(&t, NULL, writev_fill, NULL) != 0) die("pthread_create");
    usleep(1000);
    read_all("main");
    pthread_join(t, NULL);
  } else if (strcmp(mode, "host-rdwr") == 0) {
    rfd = open(argv[2], O_RDWR);
    if (rfd < 0) die("open host fifo read-write");
    char b = '-';
    ssize_t n = read(rfd, &b, 1);
    printf("host-rdwr read: %zd '%c'\n", n, b);
  } else if (strcmp(mode, "host-readers") == 0) {
    rfd = open(argv[2], O_RDONLY | O_NONBLOCK);
    int other = open(argv[2], O_RDONLY | O_NONBLOCK);
    if (rfd < 0 || other < 0) die("open host fifo for reading");
    if (fcntl(rfd, F_SETFL, fcntl(rfd, F_GETFL) & ~O_NONBLOCK) != 0) die("F_SETFL");
    char b = '-';
    ssize_t n = read(rfd, &b, 1);
    printf("host-readers read: %zd '%c'\n", n, b);
    close(other);
  } else if (strcmp(mode, "private-table") == 0 && argc == 4) {
    private_table(argv[2], argv[3], 0);
  } else if (strcmp(mode, "private-writer") == 0) {
    private_writer(argv[2]);
  } else if (strcmp(mode, "private-table-guest") == 0) {
    char path[4096];
    snprintf(path, sizeof path, "%s/thread-fifo", argv[2]);
    unlink(path);
    if (mkfifo(path, 0600) != 0) die("mkfifo");
    private_table(path, argv[2], 1);
    unlink(path);
  } else if (strcmp(mode, "host-reader-opath") == 0) {
    int observer = open(argv[2], O_PATH | O_CLOEXEC);
    if (observer < 0) die("open host fifo O_PATH");
    wfd = open(argv[2], O_WRONLY);
    if (wfd < 0) die("open host fifo for writing");
    static char buf[FILL_BYTES];
    for (size_t i = 0; i < sizeof buf; i++) buf[i] = (char)(i * 7);
    ssize_t n = write(wfd, buf, sizeof buf);
    printf("host-reader-opath wrote: %zd\n", n);
    close(observer);
  } else if (strcmp(mode, "open-host") == 0) {
    rfd = open(argv[2], O_RDONLY);
    if (rfd < 0) die("open host fifo for reading");
    char b = '-';
    ssize_t n = read(rfd, &b, 1);
    printf("open-host read: %zd '%c'\n", n, b);
  } else if (strcmp(mode, "open-host-writer") == 0) {
    wfd = open(argv[2], O_WRONLY);
    if (wfd < 0) die("open host fifo for writing");
    printf("open-host-writer wrote: %zd\n", write(wfd, "w", 1));
  } else if (strcmp(mode, "open-host-beside-poller") == 0) {
    if (pipe(poller_pipe) != 0) die("pipe");
    pthread_t t;
    if (pthread_create(&t, NULL, read_poller_pipe, NULL) != 0) die("pthread_create");
    usleep(1000);
    wfd = open(argv[2], O_WRONLY);
    if (wfd < 0) die("open host fifo for writing");
    printf("main wrote: %zd\n", write(wfd, "w", 1));
    if (write(poller_pipe[1], "p", 1) != 1) die("write pipe");
    pthread_join(t, NULL);
  } else if (strcmp(mode, "host-reader") == 0) {
    wfd = open(argv[2], O_WRONLY);
    if (wfd < 0) die("open host fifo for writing");
    static char buf[FILL_BYTES];
    for (size_t i = 0; i < sizeof buf; i++) buf[i] = (char)(i * 7);
    ssize_t n = write(wfd, buf, sizeof buf);
    printf("host-reader wrote: %zd\n", n);
  } else if (strcmp(mode, "open-writer-first") == 0 ||
             strcmp(mode, "open-reader-first") == 0) {
    int writer_first = strcmp(mode, "open-writer-first") == 0;
    char path[4096];
    snprintf(path, sizeof path, "%s/fifo", argv[2]);
    unlink(path);
    if (mkfifo(path, 0600) != 0) die("mkfifo");
    pid_t child = fork();
    if (child < 0) die("fork");
    if (child == 0) {
      if (writer_first) usleep(1000);
      int fd = open(path, O_RDONLY);
      if (fd < 0) die("child open");
      char b = '-';
      ssize_t n = read(fd, &b, 1);
      printf("child read: %zd '%c'\n", n, b);
      _exit(0);
    }
    if (!writer_first) usleep(1000);
    int fd = open(path, O_WRONLY);
    if (fd < 0) die("parent open");
    if (write(fd, "z", 1) != 1) die("write");
    close(fd);
    int status;
    if (waitpid(child, &status, 0) != child) die("waitpid");
    unlink(path);
    printf("parent wrote 1\n");
  } else if (strcmp(mode, "open-beside-pipe") == 0) {
    const char *path = fresh_fifo(argv[2]);
    int p[2];
    if (pipe(p) != 0) die("pipe");
    if (fork() == 0) {
      close(p[0]);
      static char buf[262144];
      memset(buf, 'a', sizeof buf);
      for (size_t off = 0; off < sizeof buf;) {
        ssize_t n = write(p[1], buf + off, sizeof buf - off);
        if (n <= 0) die("fill pipe");
        off += n;
      }
      close(p[1]);
      int fd = open(path, O_WRONLY);
      if (fd < 0 || write(fd, "x", 1) != 1) die("child writer");
      _exit(0);
    }
    if (fork() == 0) {
      close(p[1]);
      char b[512];
      long total = 0;
      ssize_t n;
      while ((n = read(p[0], b, sizeof b)) > 0) total += n;
      printf("drained %ld\n", total);
      _exit(0);
    }
    close(p[0]);
    close(p[1]);
    int fd = open(path, O_RDONLY);
    char c = '-';
    ssize_t n = read(fd, &c, 1);
    while (wait(NULL) > 0) {}
    printf("parent read: %zd '%c'\n", n, c);
  } else if (strcmp(mode, "open-eintr") == 0 || strcmp(mode, "open-restart") == 0 ||
             strcmp(mode, "open-ignored") == 0 || strcmp(mode, "open-two-signals") == 0) {
    const char *path = fresh_fifo(argv[2]);
    int restart = strcmp(mode, "open-restart") == 0;
    int ignored = strcmp(mode, "open-ignored") == 0;
    int two = strcmp(mode, "open-two-signals") == 0;
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_handler = ignored ? SIG_IGN : on_usr1;
    sa.sa_flags = restart ? SA_RESTART : 0;
    if (sigaction(SIGUSR1, &sa, NULL) != 0) die("sigaction");
    sa.sa_handler = on_usr2;
    if (sigaction(SIGUSR2, &sa, NULL) != 0) die("sigaction");
    pid_t parent = getpid();
    pid_t child = fork();
    if (child == 0) {
      usleep(100000);
      kill(parent, SIGUSR1);
      if (two) kill(parent, SIGUSR2);
      if (restart || ignored) {
        usleep(100000);
        int fd = open(path, O_WRONLY);
        if (fd < 0 || write(fd, "r", 1) != 1) die("child writer");
      }
      _exit(0);
    }
    int fd = open(path, O_RDONLY);
    int e = errno;
    char c = '-';
    if (fd >= 0 && read(fd, &c, 1) != 1) die("read");
    waitpid(child, NULL, 0);
    printf("open: %s, read '%c', usr1 %d, usr2 %d\n", fd < 0 ? strerror(e) : "ok", c, (int)usr1, (int)usr2);
  } else if (strcmp(mode, "open-reader-closes") == 0) {
    const char *path = fresh_fifo(argv[2]);
    signal(SIGPIPE, SIG_IGN);
    pid_t child = fork();
    if (child == 0) {
      usleep(100000);
      int r = open(path, O_RDONLY | O_NONBLOCK);
      if (r < 0) die("child reader");
      close(r);
      _exit(0);
    }
    int fd = open(path, O_WRONLY);
    if (fd < 0) die("parent open");
    ssize_t n = write(fd, "w", 1);
    int e = errno;
    waitpid(child, NULL, 0);
    printf("open: ok, write: %zd %s\n", n, n < 0 ? strerror(e) : "");
  } else if (strcmp(mode, "open-unpublished-reader") == 0 ||
             strcmp(mode, "open-unpublished-writer") == 0) {
    pending_path = fresh_fifo(argv[2]);
    pending_writer = strcmp(mode, "open-unpublished-writer") == 0;
    int hint = open("/dev/null", O_RDONLY);
    if (hint < 0 || close(hint) != 0) die("hint");
    pthread_t t;
    if (pthread_create(&t, NULL, open_pending, NULL) != 0) die("pthread_create");
    while (!pending_started) sched_yield();
    for (int i = 0; i < 128; i++) sched_yield();
    errno = 0;
    int seen = fcntl(hint, F_GETFD);
    int seen_errno = errno;
    int peer = open(pending_path, O_RDWR | O_NONBLOCK);
    if (peer < 0) die("peer");
    if (!pending_writer && write(peer, "u", 1) != 1) die("peer write");
    while (!pending_finished) sched_yield();
    pthread_join(t, NULL);
    printf("pending number: F_GETFD %d %s\n", seen, seen < 0 ? strerror(seen_errno) : "");
  } else if (strcmp(mode, "open-exclusive") == 0) {
    exclusive_path = fresh_fifo(argv[2]);
    pthread_t t;
    if (pthread_create(&t, NULL, open_exclusive, NULL) != 0) die("pthread_create");
    while (!exclusive_done) sched_yield();
    pthread_join(t, NULL);
    printf("exclusive open: %d %s\n", exclusive_fd, exclusive_fd < 0 ? strerror(exclusive_errno) : "");
  } else if (strcmp(mode, "open-host-beside-yielder") == 0) {
    pthread_t t;
    if (pthread_create(&t, NULL, yield_a_while, NULL) != 0) die("pthread_create");
    rfd = open(argv[2], O_RDONLY);
    if (rfd < 0) die("open host fifo for reading");
    char b = '-';
    ssize_t n = read(rfd, &b, 1);
    printf("open-host-beside-yielder read: %zd '%c'\n", n, b);
    pthread_join(t, NULL);
  } else if (strcmp(mode, "open-host-beside-spinner") == 0) {
    pthread_t t;
    if (pthread_create(&t, NULL, yield_until_stopped, NULL) != 0) die("pthread_create");
    rfd = open(argv[2], O_RDONLY);
    if (rfd < 0) die("open host fifo for reading");
    spinner_stop = 1;
    pthread_join(t, NULL);
    char b = '-';
    ssize_t n = read(rfd, &b, 1);
    printf("open-host-beside-spinner read: %zd '%c'\n", n, b);
  } else if (strcmp(mode, "open-after-sleep") == 0) {
    const char *path = fresh_fifo(argv[2]);
    pid_t child = fork();
    if (child == 0) {
      sleep(100);
      int fd = open(path, O_WRONLY);
      if (fd < 0 || write(fd, "s", 1) != 1) die("child writer");
      _exit(0);
    }
    int fd = open(path, O_RDONLY);
    char c = '-';
    ssize_t n = read(fd, &c, 1);
    waitpid(child, NULL, 0);
    printf("parent read: %zd '%c'\n", n, c);
  } else if (strcmp(mode, "exclusive-reader-beside-host") == 0) {
    /* DIR/fifo made here; the host blocks opening it for writing once
     * DIR/made appears, and says so with DIR/blocked. */
    const char *path = fresh_fifo(argv[2]);
    char made[4096], blocked[4096];
    snprintf(made, sizeof made, "%s/made", argv[2]);
    snprintf(blocked, sizeof blocked, "%s/blocked", argv[2]);
    int m = open(made, O_WRONLY | O_CREAT, 0600);
    if (m < 0) die("made");
    close(m);
    while (access(blocked, F_OK) != 0) usleep(1000);
    int fd = open(path, O_RDONLY | O_CREAT | O_EXCL, 0600);
    printf("exclusive reader: %d %s\n", fd, fd < 0 ? strerror(errno) : "");
    made_fifo[0] = '\0';
  } else if (strcmp(mode, "open-renamed") == 0 || strcmp(mode, "open-renamed-new") == 0 ||
             strcmp(mode, "open-linked-unlinked") == 0) {
    /* The child waits to open DIR/fifo for reading; the parent then renames
     * it (and makes a new FIFO at the old name), or links and unlinks it,
     * and opens the FIFO's new name for writing. The child's open completes
     * on the FIFO it found. */
    const char *path = fresh_fifo(argv[2]);
    char other[4096];
    snprintf(other, sizeof other, "%s/other", argv[2]);
    unlink(other);
    pid_t child = fork();
    if (child == 0) {
      int fd = open(path, O_RDONLY);
      if (fd < 0) {
        printf("child open: %s\n", strerror(errno));
        _exit(0);
      }
      char c = '-';
      ssize_t n = read(fd, &c, 1);
      printf("child open: ok, read %zd '%c'\n", n, c);
      _exit(0);
    }
    usleep(200000);
    if (strcmp(mode, "open-linked-unlinked") == 0) {
      if (link(path, other) != 0 || unlink(path) != 0) die("link and unlink");
    } else {
      if (rename(path, other) != 0) die("rename");
      if (strcmp(mode, "open-renamed-new") == 0 && mkfifo(path, 0600) != 0) die("mkfifo again");
    }
    int fd = open(other, O_WRONLY);
    if (fd < 0 || write(fd, "x", 1) != 1) die("parent writer");
    close(fd);
    waitpid(child, NULL, 0);
    unlink(other);
    printf("parent done\n");
  } else if (strcmp(mode, "open-leaves-no-mapping") == 0) {
    /* The open waits, the FIFO is renamed, and the open completes on it:
     * whatever Hermit stages for that leaves no mapping behind. */
    const char *path = fresh_fifo(argv[2]);
    char other[4096];
    snprintf(other, sizeof other, "%s/other", argv[2]);
    unlink(other);
    pid_t child = fork();
    if (child == 0) {
      usleep(200000);
      if (rename(path, other) != 0) die("rename");
      int fd = open(other, O_WRONLY);
      if (fd < 0 || write(fd, "x", 1) != 1) die("child writer");
      _exit(0);
    }
    read_maps(maps_before, sizeof maps_before);
    int fd = open(path, O_RDONLY);
    if (fd < 0) die("open");
    char c = '-';
    ssize_t n = read(fd, &c, 1);
    read_maps(maps_after, sizeof maps_after);
    printf("read %zd '%c'; maps unchanged: %d\n", n, c, strcmp(maps_before, maps_after) == 0);
    close(fd);
    waitpid(child, NULL, 0);
    unlink(other);
  } else if (strcmp(mode, "open-killed-beside-a-sharer") == 0) {
    /* The child shares this address space; the open it waits in is killed
     * from outside. */
    sharer_path = fresh_fifo(argv[2]);
    size_t stack_size = 1 << 16;
    char *stack = mmap(NULL, stack_size, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (stack == MAP_FAILED) die("mmap stack");
    read_maps(maps_before, sizeof maps_before);
    pid_t child = clone(open_in_a_sharer, stack + stack_size, CLONE_VM | SIGCHLD, NULL);
    if (child < 0) die("clone");
    int status = 0;
    if (waitpid(child, &status, 0) != child) die("waitpid");
    read_maps(maps_after, sizeof maps_after);
    printf("child killed by signal %d; maps unchanged: %d\n",
           WIFSIGNALED(status) ? WTERMSIG(status) : 0, strcmp(maps_before, maps_after) == 0);
  } else if (strcmp(mode, "open-killed-then-its-cleaner-killed") == 0) {
    sharer_path = fresh_fifo(argv[2]);
    size_t stack_size = 1 << 16;
    char *stacks = mmap(NULL, 2 * stack_size, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (stacks == MAP_FAILED) die("mmap stacks");
    read_maps(maps_before, sizeof maps_before);
    pid_t opener = clone(open_as_opener, stacks + stack_size, CLONE_VM | SIGCHLD, NULL);
    pid_t cleaner = clone(open_as_cleaner, stacks + 2 * stack_size, CLONE_VM | SIGCHLD, NULL);
    if (opener < 0 || cleaner < 0) die("clone");
    int opener_status = 0, cleaner_status = 0;
    if (waitpid(cleaner, &cleaner_status, 0) != cleaner || waitpid(opener, &opener_status, 0) != opener)
      die("waitpid");
    read_maps(maps_after, sizeof maps_after);
    printf("opener killed by signal %d; cleaner killed by signal %d; maps unchanged: %d\n",
           WIFSIGNALED(opener_status) ? WTERMSIG(opener_status) : 0,
           WIFSIGNALED(cleaner_status) ? WTERMSIG(cleaner_status) : 0,
           strcmp(maps_before, maps_after) == 0);
  } else if (strcmp(mode, "open-killed-in-a-spawned-child") == 0) {
    /* posix_spawn's child shares this address space and holds this thread
     * until it execs; its file action's open waits and is killed. */
    const char *path = fresh_fifo(argv[2]);
    posix_spawn_file_actions_t actions;
    posix_spawn_file_actions_init(&actions);
    posix_spawn_file_actions_addopen(&actions, 0, path, O_RDONLY, 0);
    read_maps(maps_before, sizeof maps_before);
    char *child_argv[] = {"true", NULL};
    char *child_envp[] = {NULL};
    pid_t child = 0;
    int spawned = posix_spawn(&child, "/bin/true", &actions, NULL, child_argv, child_envp);
    int status = 0;
    if (spawned == 0 && waitpid(child, &status, 0) != child) die("waitpid");
    read_maps(maps_after, sizeof maps_after);
    printf("spawn %d; child killed by signal %d; maps unchanged: %d\n", spawned,
           WIFSIGNALED(status) ? WTERMSIG(status) : 0, strcmp(maps_before, maps_after) == 0);
  } else if (strcmp(mode, "open-chrooted-beside-a-fake-proc") == 0 ||
             strcmp(mode, "open-writer-chrooted-beside-a-fake-proc") == 0) {
    /* /proc/1/fd/N in this root are regular files (or FIFOs), not the
     * tracer's descriptors. */
    int writer = strcmp(mode, "open-writer-chrooted-beside-a-fake-proc") == 0;
    char name[4096];
    snprintf(name, sizeof name, "%s/proc", argv[2]);
    mkdir(name, 0700);
    snprintf(name, sizeof name, "%s/proc/1", argv[2]);
    mkdir(name, 0700);
    snprintf(name, sizeof name, "%s/proc/1/fd", argv[2]);
    mkdir(name, 0700);
    for (int i = 0; i < 1024; i++) {
      snprintf(name, sizeof name, "%s/proc/1/fd/%d", argv[2], i);
      if (writer) {
        if (mkfifo(name, 0600) != 0) die("fake proc fifo");
        continue;
      }
      int fd = open(name, O_WRONLY | O_CREAT | O_TRUNC, 0600);
      if (fd < 0 || write(fd, "W", 1) != 1) die("fake proc");
      close(fd);
    }
    if (chroot(argv[2]) != 0 || chdir("/") != 0) die("chroot");
    const char *path = fresh_fifo("");
    pid_t child = fork();
    if (child == 0) {
      int fd = open(path, writer ? O_WRONLY : O_RDONLY);
      if (fd < 0) {
        printf("child open: %s\n", strerror(errno));
        _exit(0);
      }
      if (writer) {
        printf("child open: ok, wrote %zd\n", write(fd, "y", 1));
        _exit(0);
      }
      char c = '-';
      ssize_t n = read(fd, &c, 1);
      printf("child open: ok, read %zd '%c'\n", n, c);
      _exit(0);
    }
    usleep(200000);
    int fd = open(path, writer ? O_RDONLY : O_WRONLY);
    if (fd < 0) die("parent open");
    if (writer) {
      waitpid(child, NULL, 0);
      char c = '-';
      ssize_t n = read(fd, &c, 1);
      printf("parent read %zd '%c'\n", n, c);
    } else {
      if (write(fd, "x", 1) != 1) die("parent writer");
      waitpid(child, NULL, 0);
    }
    close(fd);
    /* A verifying second run starts from the same directory. */
    for (int i = 0; i < 1024; i++) {
      snprintf(name, sizeof name, "/proc/1/fd/%d", i);
      unlink(name);
    }
    rmdir("/proc/1/fd");
    rmdir("/proc/1");
    rmdir("/proc");
    printf("parent done\n");
  } else if (strcmp(mode, "open-on-a-tight-stack") == 0) {
    /* The child's blocking open runs with no writable memory 16 bytes
     * below its stack pointer; its path lives elsewhere. The FIFO is renamed
     * while it waits, so the open completes on it only as Linux's does. */
    const char *path = fresh_fifo(argv[2]);
    char other[4096];
    snprintf(other, sizeof other, "%s/other", argv[2]);
    unlink(other);
    pid_t child = fork();
    if (child == 0) {
      long page = sysconf(_SC_PAGESIZE);
      unsigned char *area =
          mmap(NULL, page * 2, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
      if (area == MAP_FAILED || mprotect(area + page, page, PROT_READ | PROT_WRITE) != 0)
        die("tight stack");
      long fd = openat_on_stack(area + page + 16, path, O_RDONLY);
      if (fd < 0) {
        printf("child open: %s\n", strerror((int)-fd));
        _exit(0);
      }
      char c = '-';
      ssize_t n = read((int)fd, &c, 1);
      printf("child open: ok, read %zd '%c', O_NONBLOCK %d\n", n, c,
             (fcntl((int)fd, F_GETFL) & O_NONBLOCK) != 0);
      _exit(0);
    }
    usleep(200000);
    if (rename(path, other) != 0) die("rename");
    int fd = open(other, O_WRONLY);
    if (fd < 0 || write(fd, "x", 1) != 1) die("parent writer");
    close(fd);
    waitpid(child, NULL, 0);
    unlink(other);
    printf("parent done\n");
  } else if (strcmp(mode, "open-without-ptrace-capability") == 0 ||
             strcmp(mode, "open-without-ptrace-capability-writer") == 0 ||
             strcmp(mode, "open-chrooted") == 0 || strcmp(mode, "open-chrooted-writer") == 0 ||
             strcmp(mode, "open-renamed-without-ptrace-capability") == 0) {
    /* A child that cannot reach the tracer's descriptors (no CAP_SYS_PTRACE,
     * or no /proc under its root) waits to open DIR/fifo; the parent opens
     * the other end (renaming the FIFO first, if asked) and moves one byte. */
    int chrooted = strncmp(mode, "open-chrooted", 13) == 0;
    int renamed = strcmp(mode, "open-renamed-without-ptrace-capability") == 0;
    int reader = strstr(mode, "-writer") == NULL;
    if (chrooted && (chroot(argv[2]) != 0 || chdir("/") != 0)) die("chroot");
    const char *path = fresh_fifo(chrooted ? "" : argv[2]);
    char other[4096];
    snprintf(other, sizeof other, "%s/other", chrooted ? "" : argv[2]);
    unlink(other);
    pid_t child = fork();
    if (child == 0) {
      if (!chrooted) drop_capabilities();
      int fd = open(path, reader ? O_RDONLY : O_WRONLY);
      if (fd < 0) {
        printf("child open: %s\n", strerror(errno));
        _exit(0);
      }
      if (reader) {
        char c = '-';
        ssize_t n = read(fd, &c, 1);
        printf("child open: ok, read %zd '%c'\n", n, c);
      } else {
        printf("child open: ok, wrote %zd\n", write(fd, "y", 1));
      }
      _exit(0);
    }
    usleep(200000);
    if (renamed && rename(path, other) != 0) die("rename");
    int fd = open(renamed ? other : path, reader ? O_WRONLY : O_RDONLY);
    if (fd < 0) die("parent open");
    if (reader) {
      if (write(fd, "x", 1) != 1) die("parent write");
      close(fd);
      waitpid(child, NULL, 0);
    } else {
      waitpid(child, NULL, 0);
      char c = '-';
      ssize_t n = read(fd, &c, 1);
      printf("parent read %zd '%c'\n", n, c);
      close(fd);
    }
    if (renamed) unlink(other);
    printf("parent done\n");
  } else if (strcmp(mode, "open-after-link-removed") == 0 ||
             strcmp(mode, "open-after-name-removed") == 0 ||
             strcmp(mode, "open-after-rename-between-links") == 0 ||
             strcmp(mode, "open-after-self-rename") == 0) {
    /* The FIFO keeps a name: a second link removed, the first name removed
     * (the second kept), or renamed onto its own other link. */
    const char *path = fresh_fifo(argv[2]);
    char other[4096];
    snprintf(other, sizeof other, "%s/other", argv[2]);
    unlink(other);
    const char *name = path;
    if (strcmp(mode, "open-after-self-rename") == 0) {
      if (rename(path, path) != 0) die("rename onto itself");
    } else if (link(path, other) != 0) {
      die("link");
    } else if (strcmp(mode, "open-after-link-removed") == 0) {
      if (unlink(other) != 0) die("unlink other");
    } else if (strcmp(mode, "open-after-name-removed") == 0) {
      if (unlink(path) != 0) die("unlink");
      name = other;
    } else if (rename(other, path) != 0) {
      die("rename onto its own link");
    }
    pid_t child = fork();
    if (child == 0) {
      int fd = open(name, O_RDONLY);
      char c = '-';
      ssize_t n = fd >= 0 ? read(fd, &c, 1) : -2;
      printf("child read: %zd '%c'\n", n, c);
      _exit(0);
    }
    int fd = open(name, O_WRONLY);
    if (fd < 0 || write(fd, "k", 1) != 1) die("parent writer");
    close(fd);
    waitpid(child, NULL, 0);
    unlink(other);
    printf("parent wrote 1\n");
  } else if (strcmp(mode, "guest-made-host-reader-renamed") == 0) {
    /* The same, but a thread renames DIR/fifo to DIR/other while main's open
     * waits, and announces it with DIR/renamed; the host opens DIR/other. */
    const char *path = fresh_fifo(argv[2]);
    snprintf(rename_from, sizeof rename_from, "%s", path);
    snprintf(rename_to, sizeof rename_to, "%s/other", argv[2]);
    snprintf(renamed_marker, sizeof renamed_marker, "%s/renamed", argv[2]);
    if (pipe(poller_pipe) != 0) die("pipe");
    pthread_t t;
    if (pthread_create(&t, NULL, rename_then_read_poller_pipe, NULL) != 0) die("pthread_create");
    wfd = open(path, O_WRONLY);
    if (wfd < 0) die("open guest fifo for writing");
    printf("main wrote: %zd\n", write(wfd, "w", 1));
    if (write(poller_pipe[1], "p", 1) != 1) die("write pipe");
    pthread_join(t, NULL);
    unlink(rename_to);
    unlink(renamed_marker);
  } else if (strcmp(mode, "guest-made-host-reader") == 0) {
    /* DIR/fifo made here, announced by DIR/made, whose reader is a host
     * process that opens later; beside it a thread waits on an empty pipe. */
    const char *path = fresh_fifo(argv[2]);
    char made[4096];
    snprintf(made, sizeof made, "%s/made", argv[2]);
    int m = open(made, O_WRONLY | O_CREAT, 0600);
    if (m < 0) die("made");
    close(m);
    if (pipe(poller_pipe) != 0) die("pipe");
    pthread_t t;
    if (pthread_create(&t, NULL, read_poller_pipe, NULL) != 0) die("pthread_create");
    wfd = open(path, O_WRONLY);
    if (wfd < 0) die("open guest fifo for writing");
    printf("main wrote: %zd\n", write(wfd, "w", 1));
    if (write(poller_pipe[1], "p", 1) != 1) die("write pipe");
    pthread_join(t, NULL);
    unlink(made);
  } else if (strcmp(mode, "open-host-order") == 0) {
    pthread_t t;
    if (pthread_create(&t, NULL, yield_then_say_s, NULL) != 0) die("pthread_create");
    rfd = open(argv[2], O_RDONLY);
    char c = '-';
    ssize_t n = rfd >= 0 ? read(rfd, &c, 1) : -2;
    printf("M %zd '%c'\n", n, c);
    fflush(stdout);
    pthread_join(t, NULL);
  } else if (strcmp(mode, "open-host-eof") == 0) {
    pthread_t t;
    if (pthread_create(&t, NULL, yield_15000, NULL) != 0) die("pthread_create");
    rfd = open(argv[2], O_RDONLY);
    char c = '-';
    ssize_t n = rfd >= 0 ? read(rfd, &c, 1) : -2;
    pthread_join(t, NULL);
    printf("open-host-eof open: %d, read: %zd\n", rfd >= 0, n);
  } else if (strcmp(mode, "open-host-exclusive-reader") == 0) {
    int fd = open(argv[2], O_RDONLY | O_CREAT | O_EXCL, 0600);
    printf("exclusive reader: %d %s\n", fd, fd < 0 ? strerror(errno) : "");
  } else if (strcmp(mode, "open-host-beside-sleeper") == 0) {
    pthread_t t;
    if (pthread_create(&t, NULL, sleep_long, NULL) != 0) die("pthread_create");
    rfd = open(argv[2], O_RDONLY);
    if (rfd < 0) die("open host fifo for reading");
    char b = '-';
    ssize_t n = read(rfd, &b, 1);
    printf("open-host-beside-sleeper read: %zd '%c'\n", n, b);
    pthread_join(t, NULL);
  } else if (strcmp(mode, "host-beside-yielder") == 0) {
    ready_path = argv[2];
    pthread_t t;
    if (pthread_create(&t, NULL, open_ready_writer, NULL) != 0) die("pthread_create");
    while (!ready_done) sched_yield();
    pthread_join(t, NULL);
    printf("host-beside-yielder opened: %s\n", ready_fd >= 0 ? "yes" : "no");
  } else if (strcmp(mode, "host-beside-pause") == 0) {
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_handler = on_usr1;
    if (sigaction(SIGUSR1, &sa, NULL) != 0) die("sigaction");
    pthread_t t;
    if (pthread_create(&t, NULL, wait_in_pause, NULL) != 0) die("pthread_create");
    while (!paused_started) sched_yield();
    usleep(1000);
    wfd = open(argv[2], O_WRONLY);
    if (wfd < 0) die("open host fifo for writing");
    if (pthread_kill(t, SIGUSR1) != 0) die("pthread_kill");
    pthread_join(t, NULL);
    printf("host-beside-pause opened: yes, usr1 %d\n", (int)usr1);
  } else if (strcmp(mode, "host") == 0) {
    rfd = open(argv[2], O_RDONLY | O_NONBLOCK);
    if (rfd < 0) die("open host fifo");
    if (fcntl(rfd, F_SETFL, fcntl(rfd, F_GETFL) & ~O_NONBLOCK) != 0) die("F_SETFL");
    char b = '-';
    ssize_t n = read(rfd, &b, 1);
    printf("host read: %zd '%c'\n", n, b);
  } else {
    fprintf(stderr, "unknown mode %s\n", mode);
    return 2;
  }
  if (made_fifo[0] != '\0') unlink(made_fifo);
  return 0;
}
