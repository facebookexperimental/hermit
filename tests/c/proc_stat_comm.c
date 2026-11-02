/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * A process's own /proc/<pid>/stat as Linux shows it (fs/proc/array.c
 * do_task_stat), and the task name stat shows after every exec, clone,
 * PR_SET_NAME and write to /proc/<pid>/comm.
 *
 * The program re-executes itself several times so that every name it prints
 * is one it chose, whatever path started it:
 *   stage0 -> execve(argv[0])            -> stage1: the main checks;
 *   stage1 -> execve("/proc/self/exe")   -> stage2: named "exe", read through
 *                                           a stat descriptor opened before
 *                                           the exec;
 *   stage2 -> execve(<long symlink>)     -> stage3: named by the link, cut to
 *                                           15 bytes;
 *   stage3 -> execveat(fd, "", AT_EMPTY_PATH), or where that exec fails,
 *             execve of the same file    -> stage4: named by the file.
 * It prints the fields of stat whose values do not depend on the host as
 * values, and the ones that name host numbers (pid, parent, process group,
 * session) as comparisons with the system calls that report them. It prints
 * the outcome of every /proc/<pid>/comm read and write and of the execveat
 * as it is, an errno included, and keeps going, so one program describes
 * every backend: a backend that refuses /proc/<pid>/comm writes or
 * descriptor execs shows those errnos on those lines. It compares the comm
 * file with stat's name and with PR_GET_NAME wherever it checks a name, the
 * root image (stage0) included, and reads a comm file in parts, by pread and
 * after a rewind across renames (fs/seq_file.c seq_read_iter and
 * seq_lseek). Standard output is the oracle; the program exits 0 when every
 * step it requires ran.
 */

#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <pthread.h>
#include <sched.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/prctl.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <unistd.h>

extern char** environ;

static int failures;

static void fail(const char* what) {
  printf("FAIL %s errno=%d\n", what, errno);
  failures++;
}

static pid_t gettid_(void) {
  return (pid_t)syscall(SYS_gettid);
}

/* Print `bytes` with NUL as \0, backslash as \\ and other unprintable bytes
 * as \xNN. */
static void put_bytes(const char* bytes, size_t n) {
  for (size_t i = 0; i < n; i++) {
    unsigned char c = (unsigned char)bytes[i];
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
}

/* Read the whole file at `path` into `buffer` (NUL-terminated), or -1. */
static ssize_t read_path(const char* path, char* buffer, size_t size) {
  int fd = open(path, O_RDONLY | O_CLOEXEC);
  if (fd < 0) {
    return -1;
  }
  size_t total = 0;
  for (;;) {
    ssize_t got = read(fd, buffer + total, size - 1 - total);
    if (got < 0) {
      int saved = errno;
      close(fd);
      errno = saved;
      return -1;
    }
    if (got == 0) {
      break;
    }
    total += (size_t)got;
  }
  close(fd);
  buffer[total] = 0;
  return (ssize_t)total;
}

/* A stat line split as a parser must split it: the name is between the
 * first " (" and the last ") ", and may hold any byte. */
struct stat_line {
  int fields;
  long pid;
  char comm[32];
  size_t comm_len;
  char state;
  long ppid, pgrp, session, tty_nr, tpgid;
  unsigned long flags;
  long priority, nice, num_threads;
  unsigned long long rsslim;
  long exit_signal, rt_priority, policy, exit_code;
};

static int parse_stat(const char* text, size_t length, struct stat_line* out) {
  const char* open = strstr(text, " (");
  const char* close = NULL;
  for (size_t i = length; i >= 2; i--) {
    if (text[i - 2] == ')' && text[i - 1] == ' ') {
      close = text + i - 2;
      break;
    }
  }
  if (open == NULL || close == NULL || close < open + 2) {
    return -1;
  }
  memset(out, 0, sizeof(*out));
  out->pid = strtol(text, NULL, 10);
  out->comm_len = (size_t)(close - (open + 2));
  if (out->comm_len >= sizeof(out->comm)) {
    return -1;
  }
  memcpy(out->comm, open + 2, out->comm_len);
  /* Fields 3..52 follow the name. */
  char rest[1024];
  snprintf(rest, sizeof(rest), "%s", close + 2);
  out->fields = 2;
  char* save = NULL;
  for (char* field = strtok_r(rest, " \n", &save); field != NULL;
       field = strtok_r(NULL, " \n", &save)) {
    out->fields++;
    switch (out->fields) {
      case 3: out->state = field[0]; break;
      case 4: out->ppid = strtol(field, NULL, 10); break;
      case 5: out->pgrp = strtol(field, NULL, 10); break;
      case 6: out->session = strtol(field, NULL, 10); break;
      case 7: out->tty_nr = strtol(field, NULL, 10); break;
      case 8: out->tpgid = strtol(field, NULL, 10); break;
      case 9: out->flags = strtoul(field, NULL, 10); break;
      case 18: out->priority = strtol(field, NULL, 10); break;
      case 19: out->nice = strtol(field, NULL, 10); break;
      case 20: out->num_threads = strtol(field, NULL, 10); break;
      case 25: out->rsslim = strtoull(field, NULL, 10); break;
      case 38: out->exit_signal = strtol(field, NULL, 10); break;
      case 40: out->rt_priority = strtol(field, NULL, 10); break;
      case 41: out->policy = strtol(field, NULL, 10); break;
      case 52: out->exit_code = strtol(field, NULL, 10); break;
      default: break;
    }
  }
  return text[length - 1] == '\n' ? 0 : -1;
}

/* PF_FORKNOEXEC (include/linux/sched.h). */
#define PF_FORKNOEXEC_BIT 0x40UL

/* Print the stat at `path`, named `label`, for the task numbered `task`. */
static void print_stat(const char* label, const char* text, ssize_t n,
                       pid_t task);

static void show_stat(const char* label, const char* path, pid_t task) {
  char text[2048];
  ssize_t n = read_path(path, text, sizeof(text));
  print_stat(label, text, n, task);
}

/* Print the stat read from descriptor `fd` from its position to the end. */
static void show_stat_fd(const char* label, int fd, pid_t task) {
  char text[2048];
  size_t total = 0;
  ssize_t got;
  while ((got = read(fd, text + total, sizeof(text) - 1 - total)) > 0) {
    total += (size_t)got;
  }
  text[total] = 0;
  print_stat(label, text, got < 0 ? got : (ssize_t)total, task);
}

static void print_stat(const char* label, const char* text, ssize_t n,
                       pid_t task) {
  struct stat_line line;
  if (n <= 0) {
    printf("%s errno=%d\n", label, errno);
    return;
  }
  if (parse_stat(text, (size_t)n, &line) != 0) {
    printf("%s unparsed\n", label);
    failures++;
    return;
  }
  printf("%s fields=%d pid-is-task=%d comm=", label, line.fields,
         line.pid == task);
  put_bytes(line.comm, line.comm_len);
  printf(" state=%c ppid-is-getppid=%d pgrp-is-getpgrp=%d"
         " session-is-getsid=%d tty_nr=%ld tpgid=%ld forknoexec=%d"
         " priority=%ld nice=%ld num_threads=%ld rsslim=%llu"
         " exit_signal=%ld rt_priority=%ld policy=%ld exit_code=%ld\n",
         line.state, line.ppid == getppid(), line.pgrp == getpgrp(),
         line.session == getsid(0), line.tty_nr, line.tpgid,
         (line.flags & PF_FORKNOEXEC_BIT) != 0, line.priority, line.nice,
         line.num_threads, line.rsslim, line.exit_signal, line.rt_priority,
         line.policy, line.exit_code);
}

/* Read the stat at `path` until the task is not running (state other than
 * R), at most 100000 times, yielding between reads, then print it: natively
 * a thread that has just released another may still be running for a
 * moment before it blocks; under Hermit only the reader runs. */
static void show_stat_once_sleeping(const char* label, const char* path,
                                    pid_t task) {
  char text[2048];
  struct stat_line line;
  for (int tries = 0; tries < 100000; tries++) {
    ssize_t n = read_path(path, text, sizeof(text));
    if (n <= 0 || parse_stat(text, (size_t)n, &line) != 0 ||
        line.state != 'R') {
      break;
    }
    sched_yield();
  }
  show_stat(label, path, task);
}

static void show_comm_file(const char* label, const char* path) {
  char text[64];
  ssize_t n = read_path(path, text, sizeof(text));
  if (n < 0) {
    printf("%s errno=%d\n", label, errno);
    return;
  }
  printf("%s len=%zd bytes=", label, n);
  put_bytes(text, (size_t)n);
  putchar('\n');
}

static void show_prctl_name(const char* label) {
  char name[17] = {0};
  if (prctl(PR_GET_NAME, name, 0, 0, 0) != 0) {
    printf("%s PR_GET_NAME errno=%d\n", label, errno);
    return;
  }
  printf("%s PR_GET_NAME=", label);
  put_bytes(name, 16);
  putchar('\n');
}

/* The last component of `path`, cut to the 15 bytes a task name holds. */
static void last_component(const char* path, char* out) {
  const char* slash = strrchr(path, '/');
  snprintf(out, 16, "%s", slash == NULL ? path : slash + 1);
}

/* Whether this task's name is `expected` (by every route), printing
 * `label` and the outcome; the expected name itself depends on the path
 * that started the program, so it is not printed. */
static void check_comm_is(const char* label, const char* expected) {
  char name[17] = {0};
  if (prctl(PR_GET_NAME, name, 0, 0, 0) != 0) {
    printf("%s PR_GET_NAME errno=%d\n", label, errno);
    failures++;
    return;
  }
  char text[2048];
  struct stat_line line;
  ssize_t n = read_path("/proc/self/stat", text, sizeof(text));
  int in_stat = n > 0 && parse_stat(text, (size_t)n, &line) == 0 &&
                line.comm_len == strlen(expected) &&
                memcmp(line.comm, expected, line.comm_len) == 0;
  char comm[64];
  n = read_path("/proc/self/comm", comm, sizeof(comm));
  if (n < 0) {
    printf("%s prctl=%d stat=%d comm-file errno=%d\n", label,
           strcmp(name, expected) == 0, in_stat, errno);
    return;
  }
  int file = (size_t)n == strlen(expected) + 1 &&
             memcmp(comm, expected, strlen(expected)) == 0;
  /* The comm file's name (before its newline) against stat's. */
  struct stat_line again;
  ssize_t s = read_path("/proc/self/stat", text, sizeof(text));
  int comm_is_stat = n > 0 && comm[n - 1] == '\n' && s > 0 &&
                     parse_stat(text, (size_t)s, &again) == 0 &&
                     again.comm_len == (size_t)n - 1 &&
                     memcmp(again.comm, comm, again.comm_len) == 0;
  printf("%s prctl=%d stat=%d comm-file=%d comm-is-stat=%d\n", label,
         strcmp(name, expected) == 0, in_stat, file, comm_is_stat);
}

/* The root image, which Hermit launched: its name by PR_GET_NAME, its comm
 * file and its stat, compared, since the name itself depends on the path
 * that started the program. Linux names all three from that path
 * (kbasename(bprm->filename)). */
static void root_names(void) {
  char name[17] = {0};
  if (prctl(PR_GET_NAME, name, 0, 0, 0) != 0) {
    printf("root image PR_GET_NAME errno=%d\n", errno);
    failures++;
    return;
  }
  char comm[64];
  ssize_t n = read_path("/proc/self/comm", comm, sizeof(comm));
  if (n < 0) {
    printf("root image comm-file errno=%d\n", errno);
    return;
  }
  char text[2048];
  struct stat_line line;
  ssize_t s = read_path("/proc/self/stat", text, sizeof(text));
  int newline = n > 0 && comm[n - 1] == '\n';
  size_t length = newline ? (size_t)n - 1 : 0;
  int prctl_is_comm = newline && strlen(name) == length &&
                      memcmp(name, comm, length) == 0;
  int stat_is_comm = newline && s > 0 &&
                     parse_stat(text, (size_t)s, &line) == 0 &&
                     line.comm_len == length &&
                     memcmp(line.comm, comm, length) == 0;
  printf("root image prctl-is-comm-file=%d stat-is-comm-file=%d\n",
         prctl_is_comm, stat_is_comm);
}

/* preadv2 of the file at `path` at 0 with flags 0, RWF_HIPRI, RWF_DSYNC,
 * RWF_SYNC and RWF_APPEND: a comm and a stat file have seq_read and no
 * read_iter (fs/proc/base.c proc_pid_set_comm_operations,
 * proc_single_file_operations), so do_loop_readv_writev takes RWF_HIPRI
 * alone and refuses the others with EOPNOTSUPP. Prints the byte count where
 * `lengths`, else ok, and the errno of a refusal. */
static void preadv2_flags(const char* label, const char* path, int lengths) {
  int fd = open(path, O_RDONLY | O_CLOEXEC);
  if (fd < 0) {
    printf("%s preadv2 open errno=%d\n", label, errno);
    return;
  }
  static const struct {
    const char* name;
    int flags;
  } FLAGS[] = {{"0", 0},
               {"hipri", RWF_HIPRI},
               {"dsync", RWF_DSYNC},
               {"sync", RWF_SYNC},
               {"append", RWF_APPEND}};
  printf("%s preadv2:", label);
  for (size_t i = 0; i < sizeof(FLAGS) / sizeof(FLAGS[0]); i++) {
    char text[1024];
    struct iovec vector = {text, sizeof(text)};
    ssize_t got = preadv2(fd, &vector, 1, 0, FLAGS[i].flags);
    if (got < 0) {
      printf(" %s=errno%d", FLAGS[i].name, errno);
    } else if (lengths) {
      printf(" %s=%zd", FLAGS[i].name, got);
    } else {
      printf(" %s=%s", FLAGS[i].name, got > 0 ? "ok" : "empty");
    }
  }
  putchar('\n');
  close(fd);
}

/* A comm file is a seq_file opened with single_open (fs/proc/base.c
 * comm_open): a read renders the name once and later reads continue that
 * rendering, a rename between them notwithstanding; a pread at 0, and a read
 * after a rewind, render it again (fs/seq_file.c seq_read_iter, seq_lseek). */
static void comm_positions(void) {
  if (prctl(PR_SET_NAME, "first-name", 0, 0, 0) != 0) {
    fail("PR_SET_NAME first-name");
  }
  int fd = open("/proc/self/comm", O_RDONLY | O_CLOEXEC);
  if (fd < 0) {
    printf("comm positions errno=%d\n", errno);
    return;
  }
  char text[64];
  ssize_t first = read(fd, text, 3);
  if (prctl(PR_SET_NAME, "second-name", 0, 0, 0) != 0) {
    fail("PR_SET_NAME second-name");
  }
  ssize_t rest = first < 0 ? -1 : read(fd, text + first, 32);
  if (first < 0 || rest < 0) {
    printf("comm read in parts errno=%d\n", errno);
  } else {
    printf("comm read in parts: %zd+%zd bytes=", first, rest);
    put_bytes(text, (size_t)(first + rest));
    putchar('\n');
  }
  ssize_t again = pread(fd, text, 32, 0);
  if (again < 0) {
    printf("comm pread at 0 errno=%d\n", errno);
  } else {
    printf("comm pread at 0 after a rename: bytes=");
    put_bytes(text, (size_t)again);
    putchar('\n');
  }
  if (prctl(PR_SET_NAME, "third-name", 0, 0, 0) != 0) {
    fail("PR_SET_NAME third-name");
  }
  if (lseek(fd, 0, SEEK_SET) != 0) {
    printf("comm rewind errno=%d\n", errno);
  } else {
    ssize_t rewound = read(fd, text, 32);
    if (rewound < 0) {
      printf("comm read after a rewind errno=%d\n", errno);
    } else {
      printf("comm read after a rewind and a rename: bytes=");
      put_bytes(text, (size_t)rewound);
      putchar('\n');
    }
  }
  close(fd);
  preadv2_flags("comm", "/proc/self/comm", 1);
  preadv2_flags("stat", "/proc/self/stat", 0);
}

/* comm writes: comm_write copies at most 15 bytes, raw, and returns the
 * count; the position does not move; a positioned write is ESPIPE. */
static void comm_writes(void) {
  int fd = open("/proc/self/comm", O_RDWR | O_CLOEXEC);
  if (fd < 0) {
    printf("open comm for writing errno=%d\n", errno);
    return;
  }
  printf("write w1\\n: %zd\n", write(fd, "w1\n", 3));
  printf("position after write: %ld\n", (long)lseek(fd, 0, SEEK_CUR));
  show_comm_file("comm after write", "/proc/self/comm");
  show_stat("stat after write", "/proc/self/stat", getpid());
  show_prctl_name("after write");
  printf("write 19 bytes: %zd\n", write(fd, "0123456789abcdefXYZ", 19));
  show_comm_file("comm after a long write", "/proc/self/comm");
  printf("write 0 bytes: %zd\n", write(fd, "ignored", 0));
  show_comm_file("comm after an empty write", "/proc/self/comm");
  struct iovec vectors[] = {
      {.iov_base = "first", .iov_len = 5},
      {.iov_base = "", .iov_len = 0},
      {.iov_base = "second", .iov_len = 6},
  };
  printf("writev: %zd\n", writev(fd, vectors, 3));
  show_comm_file("comm after writev", "/proc/self/comm");
  errno = 0;
  ssize_t positioned = pwrite(fd, "p", 1, 0);
  printf("pwrite: %zd errno=%d\n", positioned, errno);
  errno = 0;
  /* The raw call: the buffer at address 1 is unreadable on purpose. */
  long bad = syscall(SYS_write, fd, 1L, 4L);
  printf("write from a bad buffer: %ld errno=%d\n", bad, errno);
  /* A read-only descriptor cannot write. */
  int ro = open("/proc/self/comm", O_RDONLY | O_CLOEXEC);
  errno = 0;
  ssize_t refused = write(ro, "x", 1);
  printf("write read-only: %zd errno=%d\n", refused, errno);
  close(ro);
  close(fd);
  show_comm_file("comm in the end", "/proc/self/comm");
}

/* The thread checks: a sibling's task files, its rename by PR_SET_NAME, and
 * glibc's pthread_setname_np of another thread, which writes its
 * task/<tid>/comm. The thread blocks in read(2) on a pipe while the main
 * thread reads its files, so it is sleeping (S). */
static int to_thread[2], from_thread[2];
static pid_t thread_tid;

static void* thread_main(void* unused) {
  (void)unused;
  thread_tid = gettid_();
  if (prctl(PR_SET_NAME, "worker-thread", 0, 0, 0) != 0) {
    fail("PR_SET_NAME in the thread");
  }
  show_stat("thread's thread-self/stat", "/proc/thread-self/stat",
            gettid_());
  show_comm_file("thread's thread-self/comm", "/proc/thread-self/comm");
  char byte = 't';
  if (write(from_thread[1], &byte, 1) != 1) {
    fail("signal main");
  }
  if (read(to_thread[0], &byte, 1) != 1) {
    fail("wait for main");
  }
  /* Main renamed this thread with pthread_setname_np. */
  show_prctl_name("thread after pthread_setname_np");
  show_comm_file("thread's comm after pthread_setname_np",
                 "/proc/thread-self/comm");
  /* The leader blocks in pthread_join: sleeping. */
  show_stat_once_sleeping("thread reads /proc/self/stat", "/proc/self/stat",
                          getpid());
  return NULL;
}

static void threads(void) {
  if (pipe(to_thread) != 0 || pipe(from_thread) != 0) {
    fail("pipes");
    return;
  }
  pthread_t thread;
  if (pthread_create(&thread, NULL, thread_main, NULL) != 0) {
    fail("pthread_create");
    return;
  }
  char byte;
  if (read(from_thread[0], &byte, 1) != 1) {
    fail("wait for the thread");
  }
  char path[64];
  snprintf(path, sizeof(path), "/proc/self/task/%d/stat", (int)thread_tid);
  /* The thread blocks in read(2) on the pipe: sleeping. */
  show_stat_once_sleeping("main reads the thread's task stat", path,
                          thread_tid);
  snprintf(path, sizeof(path), "/proc/%d/task/%d/comm", (int)getpid(),
           (int)thread_tid);
  show_comm_file("main reads the thread's task comm", path);
  show_stat("main's own stat with a thread", "/proc/self/stat", getpid());
  int named = pthread_setname_np(thread, "renamed-by-main");
  printf("pthread_setname_np: %d\n", named);
  show_comm_file("main reads the renamed thread's comm", path);
  char name[16] = {0};
  int got = pthread_getname_np(thread, name, sizeof(name));
  printf("pthread_getname_np: %d name=%s\n", got, got == 0 ? name : "");
  if (write(to_thread[1], "m", 1) != 1) {
    fail("release the thread");
  }
  pthread_join(thread, NULL);
  close(to_thread[0]);
  close(to_thread[1]);
  close(from_thread[0]);
  close(from_thread[1]);
}

/* A forked child takes its parent's name, and is a forked task that has not
 * exec'd (PF_FORKNOEXEC). */
static void fork_child(void) {
  fflush(stdout);
  pid_t child = fork();
  if (child < 0) {
    fail("fork");
    return;
  }
  if (child == 0) {
    show_comm_file("fork child's comm", "/proc/self/comm");
    show_stat("fork child's stat", "/proc/self/stat", getpid());
    fflush(stdout);
    _exit(0);
  }
  int status;
  if (waitpid(child, &status, 0) != child || status != 0) {
    fail("wait for the fork child");
  }
}

/* A process whose parent is a guest process (a child of this one, so its
 * parent's number is above 1) renames itself, opens its own stat by number
 * and forks a reader, which gives itself another name and reads the
 * opener's stat through the inherited descriptor: the described task's own
 * name, not the reader's (on DBT the tool reads it by the kernel's numbers),
 * and the opener's parent, which must be the opener's own getppid answer (on
 * DBT translated to the kernel's numbering). */
static void inherited_descriptor_reader(int fd, pid_t opener,
                                        pid_t openers_parent) {
  if (prctl(PR_SET_NAME, "probe-reader", 0, 0, 0) != 0) {
    printf("PR_SET_NAME probe-reader errno=%d\n", errno);
  }
  char text[2048];
  size_t total = 0;
  ssize_t got;
  while ((got = read(fd, text + total, sizeof(text) - 1 - total)) > 0) {
    total += (size_t)got;
  }
  text[total] = 0;
  struct stat_line line;
  if (got < 0) {
    printf("child reads its parent's stat errno=%d\n", errno);
  } else if (parse_stat(text, total, &line) != 0) {
    printf("child reads its parent's stat unparsed\n");
  } else {
    printf("child reads its parent's stat fields=%d pid-is-parent=%d comm=",
           line.fields, line.pid == opener);
    put_bytes(line.comm, line.comm_len);
    printf(" ppid-is-parents-getppid=%d parents-parent-above-1=%d\n",
           line.ppid == openers_parent, openers_parent > 1);
  }
  fflush(stdout);
}

static void inherited_descriptor(void) {
  fflush(stdout);
  pid_t opener = fork();
  if (opener < 0) {
    fail("fork the inherited-descriptor opener");
    return;
  }
  if (opener == 0) {
    if (prctl(PR_SET_NAME, "probe-opener", 0, 0, 0) != 0) {
      printf("PR_SET_NAME probe-opener errno=%d\n", errno);
    }
    char path[64];
    snprintf(path, sizeof(path), "/proc/%d/stat", (int)getpid());
    int fd = open(path, O_RDONLY);
    if (fd < 0) {
      printf("open the opener's stat by pid errno=%d\n", errno);
      fflush(stdout);
      _exit(1);
    }
    pid_t self = getpid();
    pid_t parent = getppid();
    pid_t reader = fork();
    if (reader < 0) {
      printf("fork the inherited-descriptor reader errno=%d\n", errno);
      fflush(stdout);
      _exit(1);
    }
    if (reader == 0) {
      inherited_descriptor_reader(fd, self, parent);
      _exit(0);
    }
    int status;
    if (waitpid(reader, &status, 0) != reader || status != 0) {
      _exit(1);
    }
    _exit(0);
  }
  int status;
  if (waitpid(opener, &status, 0) != opener || status != 0) {
    fail("wait for the inherited-descriptor opener");
  }
}

static void stage1(const char* started_as) {
  char expected[16];
  last_component(started_as, expected);
  check_comm_is("named by the path it was started by", expected);
  /* A name of its own, so that what it prints does not depend on the path
   * that started it. */
  if (prctl(PR_SET_NAME, "probe-start", 0, 0, 0) != 0) {
    fail("PR_SET_NAME probe-start");
  }
  show_stat("own stat", "/proc/self/stat", getpid());
  char path[64];
  snprintf(path, sizeof(path), "/proc/%d/stat", (int)getpid());
  show_stat("own stat by pid", path, getpid());
  show_stat("thread-self stat", "/proc/thread-self/stat", gettid_());
  snprintf(path, sizeof(path), "/proc/self/task/%d/stat", (int)gettid_());
  show_stat("own task stat", path, gettid_());

  if (prctl(PR_SET_NAME, "0123456789abcdefghij", 0, 0, 0) != 0) {
    fail("PR_SET_NAME long");
  }
  show_prctl_name("after PR_SET_NAME");
  show_comm_file("comm after PR_SET_NAME", "/proc/self/comm");
  show_comm_file("thread-self comm after PR_SET_NAME", "/proc/thread-self/comm");
  show_stat("stat after PR_SET_NAME", "/proc/self/stat", getpid());
  if (prctl(PR_SET_NAME, "a)b\n\\c", 0, 0, 0) != 0) {
    fail("PR_SET_NAME raw");
  }
  show_comm_file("comm with raw bytes", "/proc/self/comm");
  show_stat("stat with raw bytes", "/proc/self/stat", getpid());

  comm_positions();
  comm_writes();
  if (prctl(PR_SET_NAME, "probe-main", 0, 0, 0) != 0) {
    fail("PR_SET_NAME probe-main");
  }
  threads();
  fork_child();
  inherited_descriptor();
}

int main(int argc, char** argv) {
  setvbuf(stdout, NULL, _IOLBF, 0);
  const char* stage = argc > 1 ? argv[1] : "stage0";
  if (strcmp(stage, "stage0") == 0) {
    root_names();
    fflush(stdout);
    /* By the path that started this program: KVM keeps no executable for
     * /proc/self/exe to name in an image Hermit loaded itself. */
    char* const next[] = {"stat-comm-probe", "stage1", argv[0], NULL};
    char* const envp[] = {"PROBE=1", NULL};
    execve(argv[0], next, envp);
    fail("execve stage1");
    return 1;
  }
  if (strcmp(stage, "stage1") == 0 && argc == 3) {
    stage1(argv[2]);
    /* A stat descriptor kept across the next exec, unread: it names the
     * process, so after the exec it shows the new image's name. */
    int kept = open("/proc/self/stat", O_RDONLY);
    if (kept < 0) {
      fail("open the kept stat");
      return 1;
    }
    char number[16];
    snprintf(number, sizeof(number), "%d", kept);
    printf("failures before exec=%d\n", failures);
    fflush(stdout);
    char* const next[] = {"stat-comm-probe", "stage2", number, NULL};
    char* const envp[] = {"PROBE=2", NULL};
    execve("/proc/self/exe", next, envp);
    fail("execve stage2");
    return 1;
  }
  if (strcmp(stage, "stage2") == 0 && argc == 3) {
    check_comm_is("exec of /proc/self/exe: named exe", "exe");
    show_stat_fd("stat descriptor opened before the exec", atoi(argv[2]),
                 getpid());
    close(atoi(argv[2]));
    /* An exec through a symbolic link with a long name: the link's name,
     * cut to 15 bytes. */
    char image[PATH_MAX];
    ssize_t n = readlink("/proc/self/exe", image, sizeof(image) - 1);
    if (n < 0) {
      fail("readlink /proc/self/exe");
      return 1;
    }
    image[n] = 0;
    char directory[] = "/tmp/proc-stat-comm-XXXXXX";
    if (mkdtemp(directory) == NULL) {
      fail("mkdtemp");
      return 1;
    }
    char link[PATH_MAX];
    snprintf(link, sizeof(link), "%s/a-very-long-symlink-name", directory);
    if (symlink(image, link) != 0) {
      fail("symlink");
      return 1;
    }
    char* const next[] = {"stat-comm-probe", "stage3", image, link,
                          directory, NULL};
    char* const envp[] = {"PROBE=3", NULL};
    fflush(stdout);
    execve(link, next, envp);
    fail("execve stage3");
    return 1;
  }
  if (strcmp(stage, "stage3") == 0 && argc == 5) {
    check_comm_is("exec through a long symlink: named by the link",
                  "a-very-long-sym");
    if (unlink(argv[3]) != 0) {
      fail("unlink the link");
    }
    /* execveat(fd, "", AT_EMPTY_PATH): Linux names the task by the file the
     * descriptor opened (begin_new_exec's comm_from_dentry), not by the
     * /dev/fd/<N> name it makes up. */
    int fd = open(argv[2], O_RDONLY);
    if (fd < 0) {
      fail("open the image");
      return 1;
    }
    char expected[16];
    last_component(argv[2], expected);
    char* const next[] = {"stat-comm-probe", "stage4", expected, argv[4],
                          "execveat", NULL};
    char* const envp[] = {"PROBE=4", NULL};
    fflush(stdout);
    syscall(SYS_execveat, fd, "", next, envp, AT_EMPTY_PATH);
    /* A backend that cannot exec a descriptor says so (KVM's executor:
     * ENOTSUP); the same file by its path names the task the same way. */
    printf("execveat(AT_EMPTY_PATH) errno=%d\n", errno);
    fflush(stdout);
    char* const by_path[] = {"stat-comm-probe", "stage4", expected, argv[4],
                             "execve", NULL};
    execve(argv[2], by_path, envp);
    fail("execve stage4");
    return 1;
  }
  if (strcmp(stage, "stage4") == 0 && argc == 5) {
    printf("stage4 reached by %s\n", argv[4]);
    check_comm_is("exec of the image file: named by its file", argv[2]);
    if (rmdir(argv[3]) != 0) {
      fail("rmdir");
    }
    printf("failures=%d\n", failures);
    return failures == 0 ? 0 : 1;
  }
  printf("unknown stage\n");
  return 2;
}
