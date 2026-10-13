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
#include <sys/syscall.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
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
    fprintf(stderr, "usage: fifo_peer thread|signal|process|fill|shared|vector DIR | host|host-rdwr|host-readers|host-reader|host-reader-opath PATH | private-table PATH DIR | private-table-guest DIR | private-writer DIR\n");
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
  } else if (strcmp(mode, "host-reader") == 0) {
    wfd = open(argv[2], O_WRONLY);
    if (wfd < 0) die("open host fifo for writing");
    static char buf[FILL_BYTES];
    for (size_t i = 0; i < sizeof buf; i++) buf[i] = (char)(i * 7);
    ssize_t n = write(wfd, buf, sizeof buf);
    printf("host-reader wrote: %zd\n", n);
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
  return 0;
}
