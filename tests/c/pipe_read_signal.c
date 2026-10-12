/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * A signal sent to a guest blocked in read() on an empty pipe, under each
 * disposition Linux distinguishes (https://github.com/rrnewton/hermit/issues/4187).
 * The mode is argv[1]:
 *
 *   eintr    a sibling thread sends SIGUSR1 to the reader, whose handler has no
 *            SA_RESTART: the read fails with EINTR after the handler runs.
 *   restart  the same with SA_RESTART: the handler runs, the read restarts and
 *            returns the byte written afterwards.
 *   ignored  SIGUSR1 is ignored: the read returns the byte, no handler runs.
 *   blocked  the reader blocks SIGUSR1: the read returns the byte, and the
 *            signal stays pending until the reader exits. This mode sends
 *            without the ready handshake, after the 1 ms settle: at that
 *            schedule, a scheduler wake for the blocked signal (before the
 *            drain kept only signals the kernel would deliver) returned the
 *            kernel's restart code itself to the guest (read -1, errno 512).
 *   ready    the sibling writes a byte, then signals the reader, whose
 *            handler has no SA_RESTART: the read returns the byte, as Linux
 *            checks for a pending signal only when the read must wait.
 *   eof      the sibling closes the last write end, then signals: the read
 *            returns 0 (end of file). These two modes do not print the handler
 *            count, which natively depends on when the reader resumes.
 *   ready2   as ready, then SIGUSR1 and SIGUSR2, both caught: prints the
 *            read and both handler counts. Natively the read returns the byte
 *            and both handlers run. Under Hermit the CLI test runs it only
 *            by record and replay, where neither signal may be lost though the
 *            read returns EINTR; the backend holds one signal at a time
 *            (https://github.com/rrnewton/reverie/issues/1012).
 *   cancel   glibc's pthread_cancel (signal 32) ends a reader blocked in the
 *            read, a cancellation point, and the join reports PTHREAD_CANCELED.
 *   process  the parent sends SIGUSR1 to a forked child blocked in the read:
 *            the read fails with EINTR after the child's handler runs.
 *   write-zero     a sibling signals a thread blocked writing one byte to a
 *                  full pipe: the write fails with EINTR.
 *   write-room     as write-zero, but the main thread reads 4096 bytes, so
 *                  there is room, before it signals: the write returns 1, as
 *                  Linux's pipe_write checks for a signal only when it must
 *                  wait. It does not print the handler count, which natively
 *                  depends on when the writer resumes.
 *   write-partial  a sibling signals a thread that wrote one pipe's capacity
 *                  of a larger write and blocks for the rest: the write
 *                  returns the bytes written, exactly the capacity
 *                  (F_GETPIPE_SZ, which differs under Hermit).
 *   waitpid        a sibling signals a thread blocked in
 *                  waitpid(child, WUNTRACED) for a child that has not exited:
 *                  the wait fails with EINTR.
 *
 * Each mode prints one line per observation, which equals the native run's.
 */

#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

static volatile sig_atomic_t handled;
static volatile sig_atomic_t handled_usr2;
static int pipe_fds[2];
/* The reader writes one byte here just before its read, so the signal is
 * sent while it waits in the read, not before. */
static int ready_fds[2];
static const char* mode;

static void handler(int signal) {
  (void)signal;
  handled++;
}

static void handler_usr2(int signal) {
  (void)signal;
  handled_usr2++;
}

static void fail(const char* operation) {
  fprintf(stderr, "%s: %s\n", operation, strerror(errno));
  exit(2);
}

static void install(int flags, void (*action)(int)) {
  struct sigaction sa;
  memset(&sa, 0, sizeof(sa));
  sa.sa_handler = action;
  sa.sa_flags = flags;
  sigemptyset(&sa.sa_mask);
  if (sigaction(SIGUSR1, &sa, NULL) != 0) {
    fail("sigaction");
  }
}

static void report_read(const char* who) {
  char byte = 0;
  ssize_t n = read(pipe_fds[0], &byte, 1);
  if (strcmp(mode, "ready2") == 0) {
    if (n < 0) {
      printf("%s read: -1 %s", who, strerror(errno));
    } else {
      printf("%s read: %zd byte '%c'", who, n, n == 1 ? byte : '-');
    }
    printf(", usr1 %d, usr2 %d\n", (int)handled, (int)handled_usr2);
    return;
  }
  if (strcmp(mode, "ready") == 0 || strcmp(mode, "eof") == 0) {
    if (n < 0) {
      printf("%s read: -1 %s\n", who, strerror(errno));
    } else {
      printf("%s read: %zd byte '%c'\n", who, n, n == 1 ? byte : '-');
    }
    return;
  }
  if (n < 0) {
    printf("%s read: -1 %s, handled %d\n", who, strerror(errno), (int)handled);
  } else {
    printf("%s read: %zd byte '%c', handled %d\n", who, n, byte, (int)handled);
  }
}

static void announce_ready(void) {
  if (write(ready_fds[1], "r", 1) != 1) {
    fail("write");
  }
}

static void await_ready(void) {
  char byte;
  if (read(ready_fds[0], &byte, 1) != 1) {
    fail("read");
  }
}

static void* reader(void* unused) {
  (void)unused;
  if (strcmp(mode, "blocked") == 0) {
    sigset_t set;
    sigemptyset(&set);
    sigaddset(&set, SIGUSR1);
    if (pthread_sigmask(SIG_BLOCK, &set, NULL) != 0) {
      fail("pthread_sigmask");
    }
  } else {
    announce_ready();
  }
  report_read("thread");
  return NULL;
}

static void* cancelled_reader(void* unused) {
  (void)unused;
  announce_ready();
  char byte;
  for (;;) {
    if (read(pipe_fds[0], &byte, 1) <= 0) {
      break;
    }
  }
  return NULL;
}

/* After the ready byte, lets the reader enter its read before the signal.
 * Under Hermit the sleep is virtual and the reader runs meanwhile; natively
 * it makes the race unlikely. */
static void settle(void) {
  usleep(1000);
}

static void write_byte(void) {
  if (write(pipe_fds[1], "x", 1) != 1) {
    fail("write");
  }
}

static int thread_mode(void) {
  if (strcmp(mode, "ignored") == 0) {
    install(0, SIG_IGN);
  } else {
    install(strcmp(mode, "restart") == 0 ? SA_RESTART : 0, handler);
  }
  if (strcmp(mode, "ready2") == 0) {
    struct sigaction sa;
    memset(&sa, 0, sizeof(sa));
    sa.sa_handler = handler_usr2;
    sigemptyset(&sa.sa_mask);
    if (sigaction(SIGUSR2, &sa, NULL) != 0) {
      fail("sigaction");
    }
  }
  pthread_t thread;
  if (pthread_create(&thread, NULL, reader, NULL) != 0) {
    fail("pthread_create");
  }
  if (strcmp(mode, "blocked") != 0) {
    await_ready();
  }
  settle();
  if (strcmp(mode, "ready") == 0 || strcmp(mode, "ready2") == 0) {
    /* The byte is ready before the signal. */
    write_byte();
  } else if (strcmp(mode, "eof") == 0) {
    /* The last writer is gone before the signal. */
    if (close(pipe_fds[1]) != 0) {
      fail("close");
    }
  }
  if (pthread_kill(thread, SIGUSR1) != 0) {
    fail("pthread_kill");
  }
  if (strcmp(mode, "ready2") == 0 && pthread_kill(thread, SIGUSR2) != 0) {
    fail("pthread_kill");
  }
  if (strcmp(mode, "ready") != 0 && strcmp(mode, "eof") != 0 && strcmp(mode, "ready2") != 0) {
    settle();
    write_byte();
  }
  if (pthread_join(thread, NULL) != 0) {
    fail("pthread_join");
  }
  return 0;
}

static int cancel_mode(void) {
  pthread_t thread;
  void* result;
  if (pthread_create(&thread, NULL, cancelled_reader, NULL) != 0) {
    fail("pthread_create");
  }
  await_ready();
  settle();
  printf("cancel: %d\n", pthread_cancel(thread));
  if (pthread_join(thread, &result) != 0) {
    fail("pthread_join");
  }
  printf("joined, canceled %d\n", result == PTHREAD_CANCELED);
  return 0;
}

static void* blocked_writer(void* unused) {
  (void)unused;
  int capacity = fcntl(pipe_fds[1], F_GETPIPE_SZ);
  if (capacity <= 0) {
    fail("F_GETPIPE_SZ");
  }
  ssize_t n;
  if (strcmp(mode, "write-zero") == 0 || strcmp(mode, "write-room") == 0) {
    int flags = fcntl(pipe_fds[1], F_GETFL);
    if (fcntl(pipe_fds[1], F_SETFL, flags | O_NONBLOCK) != 0) {
      fail("fcntl");
    }
    char* fill = calloc((size_t)capacity, 1);
    if (fill == NULL) {
      fail("calloc");
    }
    while (write(pipe_fds[1], fill, (size_t)capacity) > 0) {
    }
    free(fill);
    if (fcntl(pipe_fds[1], F_SETFL, flags) != 0) {
      fail("fcntl");
    }
    announce_ready();
    n = write(pipe_fds[1], "y", 1);
  } else {
    size_t length = 2 * (size_t)capacity;
    char* data = calloc(length, 1);
    if (data == NULL) {
      fail("calloc");
    }
    announce_ready();
    n = write(pipe_fds[1], data, length);
    free(data);
  }
  if (strcmp(mode, "write-room") == 0) {
    if (n < 0) {
      printf("write: -1 %s\n", strerror(errno));
    } else {
      printf("write: %zd\n", n);
    }
    return NULL;
  }
  if (n < 0) {
    printf("write: -1 %s, handled %d\n", strerror(errno), (int)handled);
  } else if (n == capacity) {
    printf("write: one pipe capacity, handled %d\n", (int)handled);
  } else {
    printf("write: %zd of capacity %d, handled %d\n", n, capacity, (int)handled);
  }
  return NULL;
}

static int write_mode(void) {
  install(0, handler);
  pthread_t thread;
  if (pthread_create(&thread, NULL, blocked_writer, NULL) != 0) {
    fail("pthread_create");
  }
  await_ready();
  settle();
  if (strcmp(mode, "write-room") == 0) {
    /* Room for the blocked byte, before the signal. */
    static char sink[4096];
    if (read(pipe_fds[0], sink, sizeof(sink)) != (ssize_t)sizeof(sink)) {
      fail("read");
    }
  }
  if (pthread_kill(thread, SIGUSR1) != 0) {
    fail("pthread_kill");
  }
  if (pthread_join(thread, NULL) != 0) {
    fail("pthread_join");
  }
  return 0;
}

static pid_t waited_child;

static void* child_waiter(void* unused) {
  (void)unused;
  int status;
  announce_ready();
  pid_t r = waitpid(waited_child, &status, WUNTRACED);
  if (r < 0) {
    printf("waitpid: -1 %s, handled %d\n", strerror(errno), (int)handled);
  } else {
    printf("waitpid: child, handled %d\n", (int)handled);
  }
  return NULL;
}

static int waitpid_mode(void) {
  install(0, handler);
  waited_child = fork();
  if (waited_child < 0) {
    fail("fork");
  }
  if (waited_child == 0) {
    char byte;
    /* Exits once the parent closes the pipe's write end, after the wait. */
    close(pipe_fds[1]);
    if (read(pipe_fds[0], &byte, 1) < 0) {
      _exit(1);
    }
    _exit(0);
  }
  pthread_t thread;
  if (pthread_create(&thread, NULL, child_waiter, NULL) != 0) {
    fail("pthread_create");
  }
  await_ready();
  settle();
  if (pthread_kill(thread, SIGUSR1) != 0) {
    fail("pthread_kill");
  }
  if (pthread_join(thread, NULL) != 0) {
    fail("pthread_join");
  }
  close(pipe_fds[1]);
  int status;
  if (waitpid(waited_child, &status, 0) != waited_child) {
    fail("waitpid");
  }
  printf("child exited %d\n", WIFEXITED(status) ? WEXITSTATUS(status) : -1);
  return 0;
}

static int process_mode(void) {
  pid_t child = fork();
  if (child < 0) {
    fail("fork");
  }
  if (child == 0) {
    install(0, handler);
    announce_ready();
    report_read("child");
    _exit(0);
  }
  await_ready();
  settle();
  if (kill(child, SIGUSR1) != 0) {
    fail("kill");
  }
  int status;
  if (waitpid(child, &status, 0) != child) {
    fail("waitpid");
  }
  printf("child exited %d\n", WIFEXITED(status) ? WEXITSTATUS(status) : -1);
  return 0;
}

int main(int argc, char** argv) {
  setvbuf(stdout, NULL, _IONBF, 0);
  if (argc != 2) {
    fprintf(stderr,
            "usage: %s eintr|restart|ignored|blocked|ready|eof|ready2|cancel|process|write-zero|write-room|write-partial|"
            "waitpid\n",
            argv[0]);
    return 2;
  }
  mode = argv[1];
  if (pipe(pipe_fds) != 0 || pipe(ready_fds) != 0) {
    fail("pipe");
  }
  if (strcmp(mode, "cancel") == 0) {
    return cancel_mode();
  }
  if (strcmp(mode, "process") == 0) {
    return process_mode();
  }
  if (strcmp(mode, "write-zero") == 0 || strcmp(mode, "write-room") == 0 ||
      strcmp(mode, "write-partial") == 0) {
    return write_mode();
  }
  if (strcmp(mode, "waitpid") == 0) {
    return waitpid_mode();
  }
  if (strcmp(mode, "eintr") == 0 || strcmp(mode, "restart") == 0 ||
      strcmp(mode, "ignored") == 0 || strcmp(mode, "blocked") == 0 ||
      strcmp(mode, "ready") == 0 || strcmp(mode, "eof") == 0 ||
      strcmp(mode, "ready2") == 0) {
    return thread_mode();
  }
  fprintf(stderr, "unknown mode %s\n", mode);
  return 2;
}
