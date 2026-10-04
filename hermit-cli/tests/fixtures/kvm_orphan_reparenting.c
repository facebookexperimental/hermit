/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// Orphan reparenting and child-exit lifecycles, printed so that one backend's
// output can be compared with another's. Every observed parent identity is
// printed as measured; nothing assumes the reparent target is PID 1.
//
//   grandchild  the middle process exits while its child lives: getppid and
//               /proc/self/status PPid through a descriptor opened before the
//               reparent and first read after it, one read before and re-read
//               after lseek(0), and one opened after it; then whether the root
//               can wait for the orphan.
//   exits       child exits before its parent, SIG_IGN and SA_NOCLDWAIT
//               children, and finally an exited child the root never reaps.
//   root-exits  the root exits with a live child and grandchild, which then
//               print after the root is gone.
//   root-worker-exit-group
//               the root, which has no traced parent, becomes a writer group:
//               worker threads that can never exit voluntarily share the
//               write end of a pipe, and a worker, not the leader, calls
//               exit_group. An independent reader prints only after EOF.
//   orphan-worker-exit-group
//   orphan-worker-fatal
//               the root exits 0 first; its orphaned child, whose direct
//               parent is then terminal, becomes the same writer group. A
//               worker either calls exit_group or takes a default-action fatal
//               signal that ends the whole group. The reader, the root's
//               grandchild, prints only after EOF.
//   root-segfault
//               the single-threaded root, holding the last write end of a
//               pipe, stores through a null pointer. The SIGSEGV is a
//               synchronous hardware fault, not a signal that the Tool
//               delivers. The reader is the root's grandchild, whose own
//               parent exits 0 and is reaped first; it prints only after EOF.
//   orphan-segfault
//               the root forks a reader and then a writer, and exits 0. Once
//               its direct parent is terminal, the orphaned writer faults the
//               same way. The reader, the writer's sibling, prints only after
//               EOF.
//
// Neither reader is the faulting process's own child. Under KVM, a process
// whose direct parent died by a synchronous fault cannot yet exit: Detcore
// finds no terminal record for that parent. That is a separate defect.
//
// root-worker-exit-group and orphan-worker-exit-group port the semantics and
// the names of the same modes in Reverie's
// reverie-kvm/tests/fixtures/exit_descriptor_ordering.c (Reverie e3c8034e).
// orphan-worker-fatal follows that file's orphan-fatal, except that a worker,
// not the leader, takes the signal. That file lives inside a Git dependency,
// so this test carries its own copy.

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define PARENT_CHANGE_POLLS 100000

static void die(const char* what) {
  fprintf(stderr, "kvm_orphan_reparenting: %s: %s\n", what, strerror(errno));
  _exit(90);
}

static void wait_eof(int fd) {
  char byte;
  for (;;) {
    ssize_t n = read(fd, &byte, 1);
    if (n == 0) {
      return;
    }
    if (n < 0 && errno != EINTR) {
      die("read");
    }
  }
}

// EOF on a pipe the parent held arrives when the parent closes its files,
// which Linux does before it reparents the children. Poll for the reparent
// itself, and report failure rather than hang if it never happens.
static pid_t wait_parent_change(pid_t old_parent) {
  struct timespec pause = {0, 1000};
  for (int poll = 0; poll < PARENT_CHANGE_POLLS; poll++) {
    pid_t now = getppid();
    if (now != old_parent) {
      return now;
    }
    nanosleep(&pause, NULL);
  }
  return old_parent;
}

static long status_ppid(int fd) {
  char buf[8192];
  size_t used = 0;
  for (;;) {
    ssize_t n = read(fd, buf + used, sizeof(buf) - 1 - used);
    if (n < 0) {
      if (errno == EINTR) {
        continue;
      }
      return -errno;
    }
    if (n == 0 || used + (size_t)n == sizeof(buf) - 1) {
      used += (size_t)n;
      break;
    }
    used += (size_t)n;
  }
  buf[used] = '\0';
  const char* line = strstr(buf, "\nPPid:");
  if (line == NULL) {
    return -1000;
  }
  return strtol(line + strlen("\nPPid:"), NULL, 10);
}

static long open_status_ppid(void) {
  int fd = open("/proc/self/status", O_RDONLY | O_CLOEXEC);
  if (fd < 0) {
    return -errno;
  }
  long ppid = status_ppid(fd);
  close(fd);
  return ppid;
}

// Raw PIDs differ between backends, so a traced process is named by its role;
// any other value (the reparent target, or an error) is printed as measured.
static const char*
relation(char* buf, size_t len, long value, pid_t root, pid_t middle) {
  if (value == root) {
    return "root";
  }
  if (value == middle) {
    return "middle";
  }
  snprintf(buf, len, "other(%ld)", value);
  return buf;
}

static int grandchild(void) {
  int hold[2], ready[2], result[2];
  if (pipe(hold) != 0 || pipe(ready) != 0 || pipe(result) != 0) {
    die("pipe");
  }
  pid_t root = getpid();
  pid_t middle = fork();
  if (middle < 0) {
    die("fork middle");
  }
  if (middle == 0) {
    middle = getpid();
    close(result[0]);
    pid_t orphan = fork();
    if (orphan < 0) {
      die("fork orphan");
    }
    if (orphan == 0) {
      close(hold[1]);
      close(ready[0]);
      pid_t self_middle = getppid();
      int opened_before = open("/proc/self/status", O_RDONLY | O_CLOEXEC);
      int read_before = open("/proc/self/status", O_RDONLY | O_CLOEXEC);
      if (opened_before < 0 || read_before < 0) {
        die("open status");
      }
      long read_before_first = status_ppid(read_before);
      if (write(ready[1], "r", 1) != 1) {
        die("write ready");
      }
      close(ready[1]);
      wait_eof(hold[0]);
      pid_t after = wait_parent_change(self_middle);
      long opened_before_first = status_ppid(opened_before);
      if (lseek(read_before, 0, SEEK_SET) != 0) {
        die("lseek");
      }
      long read_before_reread = status_ppid(read_before);
      long opened_after = open_status_ppid();
      char b[6][32];
      dprintf(
          result[1],
          "grandchild: getppid before=%s after=%s changed=%s\n"
          "grandchild: status opened-before first-read-after=%s\n"
          "grandchild: status read-before=%s reread-after-lseek=%s\n"
          "grandchild: status opened-after=%s\n",
          relation(b[0], sizeof(b[0]), self_middle, root, middle),
          relation(b[1], sizeof(b[1]), after, root, middle),
          after == self_middle ? "no" : "yes",
          relation(b[2], sizeof(b[2]), opened_before_first, root, middle),
          relation(b[3], sizeof(b[3]), read_before_first, root, middle),
          relation(b[4], sizeof(b[4]), read_before_reread, root, middle),
          relation(b[5], sizeof(b[5]), opened_after, root, middle));
      _exit(0);
    }
    close(hold[0]);
    close(ready[1]);
    close(result[1]);
    // Exit only after the orphan has read its status once, while still ours.
    wait_eof(ready[0]);
    _exit(0);
  }
  close(hold[0]);
  close(hold[1]);
  close(ready[0]);
  close(ready[1]);
  close(result[1]);
  int status;
  if (waitpid(middle, &status, 0) != middle) {
    die("waitpid middle");
  }
  printf(
      "root: middle exited=%d status=%d\n",
      WIFEXITED(status),
      WEXITSTATUS(status));
  char buf[1024];
  ssize_t n;
  while ((n = read(result[0], buf, sizeof(buf))) > 0) {
    fwrite(buf, 1, (size_t)n, stdout);
  }
  pid_t waited = waitpid(-1, &status, 0);
  if (waited < 0) {
    printf(
        "root: wait for orphan -> %s\n",
        errno == ECHILD ? "ECHILD" : strerror(errno));
  } else {
    printf(
        "root: wait for orphan -> reaped pid=%ld status=%d\n",
        (long)waited,
        WEXITSTATUS(status));
  }
  return 0;
}

static void child_first(void) {
  pid_t child = fork();
  if (child < 0) {
    die("fork child");
  }
  if (child == 0) {
    pid_t inner = fork();
    if (inner < 0) {
      die("fork inner");
    }
    if (inner == 0) {
      _exit(5);
    }
    int status;
    if (waitpid(inner, &status, 0) != inner) {
      die("waitpid inner");
    }
    _exit(WEXITSTATUS(status) + 10);
  }
  int status;
  if (waitpid(child, &status, 0) != child) {
    die("waitpid child");
  }
  printf("child-first: parent saw %d\n", WEXITSTATUS(status));
}

static void unwaitable_child(const char* label, int use_nocldwait) {
  struct sigaction action, previous;
  memset(&action, 0, sizeof(action));
  if (use_nocldwait) {
    action.sa_handler = SIG_DFL;
    action.sa_flags = SA_NOCLDWAIT;
  } else {
    action.sa_handler = SIG_IGN;
  }
  if (sigaction(SIGCHLD, &action, &previous) != 0) {
    die("sigaction");
  }
  pid_t child = fork();
  if (child < 0) {
    die("fork unwaitable");
  }
  if (child == 0) {
    _exit(4);
  }
  int status;
  pid_t waited = waitpid(-1, &status, 0);
  int saved = errno;
  if (sigaction(SIGCHLD, &previous, NULL) != 0) {
    die("sigaction restore");
  }
  if (waited < 0) {
    printf(
        "%s: wait -> %s\n",
        label,
        saved == ECHILD ? "ECHILD" : strerror(saved));
  } else {
    printf("%s: wait -> reaped status=%d\n", label, WEXITSTATUS(status));
  }
}

static int exits(void) {
  child_first();
  unwaitable_child("sig-ign", 0);
  unwaitable_child("sa-nocldwait", 1);
  pid_t zombie = fork();
  if (zombie < 0) {
    die("fork zombie");
  }
  if (zombie == 0) {
    _exit(3);
  }
  siginfo_t info;
  memset(&info, 0, sizeof(info));
  if (waitid(P_PID, (id_t)zombie, &info, WEXITED | WNOWAIT) != 0) {
    die("waitid zombie");
  }
  printf(
      "zombie: exited status=%d, root exits without reaping it\n",
      info.si_status);
  return 0;
}

static int root_exits(void) {
  int hold[2], order[2];
  if (pipe(hold) != 0 || pipe(order) != 0) {
    die("pipe");
  }
  pid_t root = getpid();
  pid_t child = fork();
  if (child < 0) {
    die("fork child");
  }
  if (child == 0) {
    close(hold[1]);
    pid_t grandchild_pid = fork();
    if (grandchild_pid < 0) {
      die("fork grandchild");
    }
    if (grandchild_pid == 0) {
      close(order[1]);
      pid_t middle = getppid();
      wait_eof(hold[0]);
      wait_eof(order[0]);
      pid_t after = wait_parent_change(middle);
      char b[32];
      printf(
          "late grandchild: parent changed=%s now=%s\n",
          after == middle ? "no" : "yes",
          relation(b, sizeof(b), after, root, middle));
      fflush(stdout);
      _exit(0);
    }
    close(order[0]);
    wait_eof(hold[0]);
    pid_t after = wait_parent_change(root);
    char b[32];
    printf(
        "late child: parent changed=%s now=%s\n",
        after == root ? "no" : "yes",
        relation(b, sizeof(b), after, root, -1));
    fflush(stdout);
    _exit(0);
  }
  close(hold[0]);
  close(order[0]);
  close(order[1]);
  printf("root: exiting 7 with a live child and grandchild\n");
  fflush(stdout);
  _exit(7);
}

#define PEERS 2

static int writer_ready[2];
static int writer_parked[2];

static void write_exact(int fd, const char* bytes, size_t length) {
  while (length > 0) {
    ssize_t n = write(fd, bytes, length);
    if (n < 0 && errno == EINTR) {
      continue;
    }
    if (n <= 0) {
      die("write");
    }
    bytes += n;
    length -= (size_t)n;
  }
}

static void read_exact(int fd, char* bytes, size_t length) {
  while (length > 0) {
    ssize_t n = read(fd, bytes, length);
    if (n < 0 && errno == EINTR) {
      continue;
    }
    if (n <= 0) {
      die("read readiness");
    }
    bytes += n;
    length -= (size_t)n;
  }
}

// Each peer holds the shared descriptor table, and so the EOF writer, until
// group teardown cancels its read of a pipe nobody writes or closes.
static void* live_peer(void* unused) {
  (void)unused;
  write_exact(writer_ready[1], "p", 1);
  char byte;
  for (;;) {
    ssize_t n = read(writer_parked[0], &byte, 1);
    if (n < 0 && errno == EINTR) {
      continue;
    }
    die("live peer returned from permanent park");
  }
  __builtin_unreachable();
}

static void* exit_group_issuer(void* unused) {
  (void)unused;
  syscall(SYS_exit_group, 0);
  __builtin_unreachable();
}

static void* fatal_signal_taker(void* unused) {
  (void)unused;
  // SIGTERM with its default action, aimed at this worker thread, ends the
  // whole thread group.
  if (syscall(SYS_tgkill, getpid(), syscall(SYS_gettid), SIGTERM) != 0) {
    die("tgkill");
  }
  for (;;) {
    pause();
  }
  __builtin_unreachable();
}

static void writer_group(const char* class_name, int fatal) {
  const char* termination = fatal ? "fatal" : "exit_group";
  int eof[2];
  if (pipe(eof) != 0 || pipe(writer_ready) != 0 || pipe(writer_parked) != 0) {
    die("pipe");
  }
  // Fork while single-threaded, so the reader has its own descriptor table
  // and survives the writer group's termination.
  pid_t reader = fork();
  if (reader < 0) {
    die("fork reader");
  }
  if (reader == 0) {
    close(eof[1]);
    close(writer_ready[0]);
    close(writer_parked[0]);
    close(writer_parked[1]);
    write_exact(writer_ready[1], "r", 1);
    close(writer_ready[1]);
    wait_eof(eof[0]);
    dprintf(
        1,
        "reader: EOF class=%s termination=%s peers=%d\n",
        class_name,
        termination,
        PEERS);
    _exit(0);
  }
  close(eof[0]);
  // The leader never closes eof[1]; the peers share it until teardown.
  pthread_t peers[PEERS];
  for (int i = 0; i < PEERS; i++) {
    int error = pthread_create(&peers[i], NULL, live_peer, NULL);
    if (error != 0) {
      errno = error;
      die("pthread_create peer");
    }
  }
  char acknowledgements[PEERS + 1];
  read_exact(writer_ready[0], acknowledgements, sizeof(acknowledgements));
  int peer_count = 0, reader_count = 0;
  for (size_t i = 0; i < sizeof(acknowledgements); i++) {
    peer_count += acknowledgements[i] == 'p';
    reader_count += acknowledgements[i] == 'r';
  }
  if (peer_count != PEERS || reader_count != 1) {
    errno = EPROTO;
    die("unexpected readiness");
  }
  dprintf(
      1,
      "writer: class=%s termination=%s peers=%d\n",
      class_name,
      termination,
      PEERS);
  pthread_t issuer;
  int error = pthread_create(
      &issuer, NULL, fatal ? fatal_signal_taker : exit_group_issuer, NULL);
  if (error != 0) {
    errno = error;
    die("pthread_create issuer");
  }
  for (;;) {
    pause();
  }
}

// Loaded at run time, so the compiler cannot replace the faulting store with a
// trap instruction: the fault is a real page fault at address zero.
static volatile int* volatile segfault_target = NULL;

static void print_and_segfault(const char* class_name) {
  dprintf(1, "writer: class=%s termination=SIGSEGV\n", class_name);
  *segfault_target = 1;
  errno = EFAULT;
  die("store through a null pointer returned");
}

static int root_worker_exit_group(void) {
  writer_group("root", 0);
  __builtin_unreachable();
}

static int root_segfault(void) {
  int eof[2], ready[2];
  if (pipe(eof) != 0 || pipe(ready) != 0) {
    die("pipe");
  }
  pid_t middle = fork();
  if (middle < 0) {
    die("fork middle");
  }
  if (middle == 0) {
    pid_t reader = fork();
    if (reader < 0) {
      die("fork reader");
    }
    if (reader == 0) {
      close(eof[1]);
      close(ready[0]);
      write_exact(ready[1], "r", 1);
      close(ready[1]);
      wait_eof(eof[0]);
      dprintf(1, "reader: EOF class=root termination=SIGSEGV\n");
      _exit(0);
    }
    _exit(0);
  }
  close(eof[0]);
  close(ready[1]);
  int status;
  if (waitpid(middle, &status, 0) != middle) {
    die("waitpid middle");
  }
  if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) {
    errno = EPROTO;
    die("middle status");
  }
  char acknowledgement;
  read_exact(ready[0], &acknowledgement, 1);
  print_and_segfault("root");
  __builtin_unreachable();
}

static int orphan_segfault(void) {
  int eof[2], ready[2], parent_gone[2];
  if (pipe(eof) != 0 || pipe(ready) != 0 || pipe(parent_gone) != 0) {
    die("pipe");
  }
  pid_t root = getpid();
  pid_t reader = fork();
  if (reader < 0) {
    die("fork reader");
  }
  if (reader == 0) {
    close(eof[1]);
    close(ready[0]);
    close(parent_gone[0]);
    close(parent_gone[1]);
    write_exact(ready[1], "r", 1);
    close(ready[1]);
    wait_eof(eof[0]);
    dprintf(
        1, "reader: EOF class=direct-parent-terminal termination=SIGSEGV\n");
    _exit(0);
  }
  pid_t writer = fork();
  if (writer < 0) {
    die("fork writer");
  }
  if (writer != 0) {
    // The root waits for neither orphan.
    _exit(0);
  }
  close(eof[0]);
  close(ready[1]);
  close(parent_gone[1]);
  char acknowledgement;
  read_exact(ready[0], &acknowledgement, 1);
  close(ready[0]);
  wait_eof(parent_gone[0]);
  close(parent_gone[0]);
  // Fault only once the direct traced parent is terminal.
  if (wait_parent_change(root) == root) {
    errno = ETIMEDOUT;
    die("parent never changed");
  }
  print_and_segfault("direct-parent-terminal");
  __builtin_unreachable();
}

static int orphan_writer(int fatal) {
  struct sigaction action;
  memset(&action, 0, sizeof(action));
  action.sa_handler = SIG_DFL;
  sigemptyset(&action.sa_mask);
  sigset_t term;
  sigemptyset(&term);
  sigaddset(&term, SIGTERM);
  if (sigaction(SIGTERM, &action, NULL) != 0 ||
      sigprocmask(SIG_UNBLOCK, &term, NULL) != 0) {
    die("SIGTERM default");
  }
  int parent_gone[2];
  if (pipe(parent_gone) != 0) {
    die("pipe");
  }
  pid_t root = getpid();
  pid_t writer = fork();
  if (writer < 0) {
    die("fork writer");
  }
  if (writer != 0) {
    // The root does not wait for the future writer group.
    _exit(0);
  }
  close(parent_gone[1]);
  wait_eof(parent_gone[0]);
  close(parent_gone[0]);
  // Build the writer only once its direct traced parent is terminal.
  if (wait_parent_change(root) == root) {
    errno = ETIMEDOUT;
    die("parent never changed");
  }
  writer_group("direct-parent-terminal", fatal);
  __builtin_unreachable();
}

int main(int argc, char** argv) {
  setvbuf(stdout, NULL, _IOLBF, 0);
  if (argc != 2) {
    fprintf(
        stderr,
        "usage: %s grandchild|exits|root-exits|root-worker-exit-group|"
        "orphan-worker-exit-group|orphan-worker-fatal|root-segfault|"
        "orphan-segfault\n",
        argv[0]);
    return 2;
  }
  if (strcmp(argv[1], "root-worker-exit-group") == 0) {
    return root_worker_exit_group();
  }
  if (strcmp(argv[1], "orphan-worker-exit-group") == 0) {
    return orphan_writer(0);
  }
  if (strcmp(argv[1], "orphan-worker-fatal") == 0) {
    return orphan_writer(1);
  }
  if (strcmp(argv[1], "root-segfault") == 0) {
    return root_segfault();
  }
  if (strcmp(argv[1], "orphan-segfault") == 0) {
    return orphan_segfault();
  }
  if (strcmp(argv[1], "grandchild") == 0) {
    return grandchild();
  }
  if (strcmp(argv[1], "exits") == 0) {
    return exits();
  }
  if (strcmp(argv[1], "root-exits") == 0) {
    return root_exits();
  }
  fprintf(stderr, "unknown mode %s\n", argv[1]);
  return 2;
}
