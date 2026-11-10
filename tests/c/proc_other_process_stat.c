/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Another process's /proc/<pid>/stat, task/<tid>/stat and comm, as Linux
 * shows them to a reader in the same PID namespace (fs/proc/array.c
 * do_task_stat, fs/proc/base.c comm_show), read the ways procfs consumers
 * read them: by path, and relative to a descriptor of the process's
 * directory (ps and pgrep open /proc/<pid> and then "stat" in it).
 *
 * A child process reads its parent's stat (the program's first process,
 * whose own parent is outside it, before that process opened its own stat),
 * names itself (PR_SET_NAME), starts a thread that names itself, uses some
 * CPU, reports its own stat's CPU time and the thread's tid, and blocks. The parent then reads the child's files. A second child
 * exits with status 7 and the parent reads its files as a zombie, before
 * reaping it (waitid WNOWAIT). More children die other ways, a raw exit(3)
 * of the only thread, a NULL store (SIGSEGV) and abort(), and with
 * --sigkill one its parent kills with SIGKILL (a backend that refuses a
 * signal to another process runs without it): each zombie's exit code
 * (stat field 52) must be the wait status the parent's own waitid reports
 * for that child, `si_status << 8` for CLD_EXITED, the signal for
 * CLD_KILLED and the signal with 0x80 for CLD_DUMPED, whether or not this
 * host's core_pattern dumps a core.
 *
 * It prints the fields whose values do not depend on the host as values,
 * and the ones that name host numbers (pids, process group, session,
 * niceness, limits) or host CPU time as comparisons, so one transcript
 * describes every run. Standard output is the oracle; the program exits 0
 * when every step it requires ran.
 */

#define _GNU_SOURCE
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/prctl.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

static int failures;

static void fail(const char* what) {
  printf("FAIL %s errno=%d\n", what, errno);
  failures++;
}

/* The stat fields of `line`: fields[0] is field 1 (pid), fields[1] the name
 * without its parentheses, fields[2] field 3 (state), and so on. Returns
 * how many there are, 0 where the line is not a stat line. */
static int split_stat(char* line, char* fields[64]) {
  char* open = strchr(line, '(');
  char* close = strrchr(line, ')');
  if (open == NULL || close == NULL || close < open || close[1] != ' ') {
    return 0;
  }
  *open = 0;
  *close = 0;
  int n = 0;
  fields[n++] = line;
  fields[n++] = open + 1;
  char* save = NULL;
  for (char* field = strtok_r(close + 2, " \n", &save); field != NULL && n < 64;
       field = strtok_r(NULL, " \n", &save)) {
    fields[n++] = field;
  }
  return n;
}

/* Read the whole file `path` (relative to `dirfd`) into `buffer`. */
static ssize_t read_file_at(int dirfd, const char* path, char* buffer, size_t size) {
  int fd = openat(dirfd, path, O_RDONLY);
  if (fd < 0) {
    return -1;
  }
  size_t total = 0;
  ssize_t n;
  while (total + 1 < size && (n = read(fd, buffer + total, size - 1 - total)) > 0) {
    total += (size_t)n;
  }
  int saved = errno;
  close(fd);
  errno = saved;
  if (n < 0) {
    return -1;
  }
  buffer[total] = 0;
  return (ssize_t)total;
}

static long long field_ll(char* fields[64], int field) {
  return strtoll(fields[field - 1], NULL, 10);
}

/* The reader's own stat fields, for the comparisons. */
static char own_line[4096];
static char* own[64];

static void load_own(void) {
  if (read_file_at(AT_FDCWD, "/proc/self/stat", own_line, sizeof own_line) < 0 ||
      split_stat(own_line, own) != 52) {
    fail("own stat");
  }
}

static int same_as_own(char* fields[64], int field) {
  return strcmp(fields[field - 1], own[field - 1]) == 0;
}

/* Whether fields first..last are all "0". */
static int all_zero(char* fields[64], int first, int last) {
  for (int field = first; field <= last; field++) {
    if (strcmp(fields[field - 1], "0") != 0) {
      return 0;
    }
  }
  return 1;
}

#define PF_EXITING 0x4ULL
#define PF_FORKNOEXEC 0x40ULL

/* The fields of a live or zombie task's stat that every run agrees on. */
static void print_stat(const char* label, char* line, pid_t pid, pid_t parent, pid_t process) {
  char* f[64];
  int n = split_stat(line, f);
  printf("%s: fields=%d\n", label, n);
  if (n != 52) {
    failures++;
    return;
  }
  unsigned long long flags = strtoull(f[8], NULL, 10);
  printf("%s: pid matches=%d name=(%s) state=%s ppid is parent=%d\n", label,
         field_ll(f, 1) == pid, f[1], f[2], field_ll(f, 4) == parent);
  printf("%s: pgrp=getpgid %d session=getsid %d tty and tpgid as own=%d\n", label,
         field_ll(f, 5) == getpgid(process), field_ll(f, 6) == getsid(process),
         same_as_own(f, 7) && same_as_own(f, 8));
  printf("%s: flags forknoexec=%d exiting=%d\n", label, (flags & PF_FORKNOEXEC) != 0,
         (flags & PF_EXITING) != 0);
  printf("%s: children's cpu=%s,%s priority,nice as own=%d num_threads=%s itrealvalue=%s\n",
         label, f[15], f[16], same_as_own(f, 18) && same_as_own(f, 19), f[19], f[20]);
  printf("%s: starttime not before own=%d rsslim as own=%d\n", label,
         field_ll(f, 22) >= field_ll(own, 22), same_as_own(f, 25));
  printf("%s: exit_signal=%s rt_priority,policy as own=%d exit_code=%s\n", label, f[37],
         same_as_own(f, 40) && same_as_own(f, 41), f[51]);
  if (f[2][0] == 'Z') {
    printf("%s: vsize,rss=%s,%s startcode..sigcatch zero=%d start_data..env_end zero=%d\n",
           label, f[22], f[23], all_zero(f, 26, 34), all_zero(f, 45, 51));
  }
}

static int ready_pipe[2];
static int go_pipe[2];

static void* named_thread(void* arg) {
  (void)arg;
  if (prctl(PR_SET_NAME, "other-thread", 0, 0, 0) != 0) {
    fail("thread PR_SET_NAME");
  }
  pid_t tid = (pid_t)syscall(SYS_gettid);
  if (write(ready_pipe[1], &tid, sizeof tid) != sizeof tid) {
    fail("thread write");
  }
  char byte;
  if (read(go_pipe[0], &byte, 1) != 1) {
    fail("thread read");
  }
  return NULL;
}

static volatile unsigned long long spin_sink;

/* The parent's own getppid answer, taken before the fork. */
static pid_t parent_ppid;

/* The live child's view of its parent's stat (the program's first process,
 * whose own parent is outside the program): the parent is blocked reading
 * the child's report. */
static void parent_from_child(void) {
  char path[64];
  char line[4096];
  char* f[64];
  snprintf(path, sizeof path, "/proc/%d/stat", (int)getppid());
  if (read_file_at(AT_FDCWD, path, line, sizeof line) < 0 || split_stat(line, f) != 52) {
    fail("parent stat from the child");
    return;
  }
  printf("parent stat (from the child): name=(%s) state=%s ppid as its getppid=%d exit_code=%s\n",
         f[1], f[2], field_ll(f, 4) == parent_ppid, f[51]);
}

/* The live child: names itself and a thread, uses CPU, reports its own CPU
 * ticks and the thread's tid, then waits for the parent. */
static void live_child(void) {
  if (prctl(PR_SET_NAME, "other-child", 0, 0, 0) != 0) {
    fail("child PR_SET_NAME");
  }
  pthread_t thread;
  if (pthread_create(&thread, NULL, named_thread, NULL) != 0) {
    fail("pthread_create");
    _exit(1);
  }
  for (unsigned long long i = 0; i < 300000000ULL; i++) {
    spin_sink += i;
  }
  parent_from_child();
  char line[4096];
  char* f[64];
  long long ticks = -1;
  if (read_file_at(AT_FDCWD, "/proc/self/stat", line, sizeof line) >= 0 &&
      split_stat(line, f) == 52) {
    ticks = field_ll(f, 14) + field_ll(f, 15);
  }
  if (write(ready_pipe[1], &ticks, sizeof ticks) != sizeof ticks) {
    fail("child write");
  }
  char byte;
  if (read(go_pipe[0], &byte, 1) != 1) {
    fail("child read");
  }
  pthread_join(thread, NULL);
  _exit(failures == 0 ? 0 : 1);
}

/* Which of the names in `want` the directory `dirfd` lists, each printed
 * as `shown` names it. */
static void listed(const char* label, int dirfd, const char* const* want, const char* const* shown,
                   int count) {
  int fd = openat(dirfd, ".", O_RDONLY | O_DIRECTORY);
  DIR* dir = fd < 0 ? NULL : fdopendir(fd);
  if (dir == NULL) {
    fail(label);
    return;
  }
  int found[16] = {0};
  struct dirent* entry;
  while ((entry = readdir(dir)) != NULL) {
    for (int i = 0; i < count; i++) {
      if (strcmp(entry->d_name, want[i]) == 0) {
        found[i] = 1;
      }
    }
  }
  closedir(dir);
  printf("%s:", label);
  for (int i = 0; i < count; i++) {
    printf(" %s=%d", shown[i], found[i]);
  }
  printf("\n");
}

static void live(void) {
  if (pipe(ready_pipe) != 0 || pipe(go_pipe) != 0) {
    fail("pipe");
    return;
  }
  parent_ppid = getppid();
  pid_t child = fork();
  if (child < 0) {
    fail("fork");
    return;
  }
  if (child == 0) {
    live_child();
  }
  pid_t tid;
  long long child_ticks;
  if (read(ready_pipe[0], &tid, sizeof tid) != sizeof tid ||
      read(ready_pipe[0], &child_ticks, sizeof child_ticks) != sizeof child_ticks) {
    fail("read child report");
    return;
  }
  /* Only now does the parent open its own stat: the child read the parent's
   * before the parent had (a process records its outside parent at its
   * first clone, too). */
  load_own();
  char path[128];
  char line[4096];
  char other[4096];
  char* f[64];

  snprintf(path, sizeof path, "/proc/%d/stat", (int)child);
  if (read_file_at(AT_FDCWD, path, line, sizeof line) < 0) {
    fail("child stat");
  } else {
    memcpy(other, line, sizeof line);
    print_stat("child stat", line, child, getpid(), child);
    if (split_stat(other, f) == 52) {
      long long ticks = field_ll(f, 14) + field_ll(f, 15);
      printf("child stat: cpu ticks used=%d at least the child's own=%d within one tick=%d\n",
             child_ticks > 0, ticks >= child_ticks, ticks - child_ticks <= 1);
    }
  }

  snprintf(path, sizeof path, "/proc/%d", (int)child);
  int dirfd = open(path, O_RDONLY | O_DIRECTORY);
  if (dirfd < 0) {
    fail("open child directory");
  } else {
    char by_path[4096];
    char relative[4096];
    snprintf(path, sizeof path, "/proc/%d/stat", (int)child);
    ssize_t a = read_file_at(AT_FDCWD, path, by_path, sizeof by_path);
    ssize_t b = read_file_at(dirfd, "stat", relative, sizeof relative);
    printf("child directory: stat read relative to it=%d same as by path=%d\n", b > 0,
           a == b && memcmp(by_path, relative, (size_t)a) == 0);
    const char* names[] = {"stat", "comm", "task"};
    listed("child directory lists", dirfd, names, names, 3);
    int tasks = openat(dirfd, "task", O_RDONLY | O_DIRECTORY);
    if (tasks < 0) {
      fail("open child task directory");
    } else {
      char pid_name[16];
      char tid_name[16];
      snprintf(pid_name, sizeof pid_name, "%d", (int)child);
      snprintf(tid_name, sizeof tid_name, "%d", (int)tid);
      const char* threads[] = {pid_name, tid_name};
      const char* shown[] = {"<pid>", "<tid>"};
      listed("child task directory lists", tasks, threads, shown, 2);
      close(tasks);
    }
    snprintf(path, sizeof path, "task/%d/stat", (int)tid);
    if (read_file_at(dirfd, path, line, sizeof line) < 0) {
      fail("thread stat relative");
    } else {
      print_stat("thread stat (relative)", line, tid, getpid(), child);
    }
    if (read_file_at(dirfd, "comm", line, sizeof line) < 0) {
      fail("child comm relative");
    } else {
      printf("child comm (relative)=%s", line);
    }
    close(dirfd);
  }

  snprintf(path, sizeof path, "/proc/%d/task/%d/stat", (int)child, (int)tid);
  if (read_file_at(AT_FDCWD, path, line, sizeof line) < 0) {
    fail("thread stat");
  } else {
    print_stat("thread stat", line, tid, getpid(), child);
  }
  snprintf(path, sizeof path, "/proc/%d/comm", (int)child);
  if (read_file_at(AT_FDCWD, path, line, sizeof line) < 0) {
    fail("child comm");
  } else {
    printf("child comm=%s", line);
  }
  snprintf(path, sizeof path, "/proc/%d/task/%d/comm", (int)child, (int)tid);
  if (read_file_at(AT_FDCWD, path, line, sizeof line) < 0) {
    fail("thread comm");
  } else {
    printf("thread comm=%s", line);
  }
  /* The thread's own directory by its number at the top: Linux finds any
   * task there. */
  snprintf(path, sizeof path, "/proc/%d/comm", (int)tid);
  if (read_file_at(AT_FDCWD, path, line, sizeof line) < 0) {
    fail("thread comm by top-level number");
  } else {
    printf("thread comm by top-level number=%s", line);
  }

  char go[2] = {'g', 'g'};
  if (write(go_pipe[1], go, sizeof go) != sizeof go) {
    fail("write go");
  }
  int status;
  if (waitpid(child, &status, 0) != child || !WIFEXITED(status) || WEXITSTATUS(status) != 0) {
    fail("wait live child");
  }
  close(ready_pipe[0]);
  close(ready_pipe[1]);
  close(go_pipe[0]);
  close(go_pipe[1]);
}

static void zombie(void) {
  pid_t child = fork();
  if (child < 0) {
    fail("fork zombie");
    return;
  }
  if (child == 0) {
    _exit(7);
  }
  siginfo_t info;
  memset(&info, 0, sizeof info);
  if (waitid(P_PID, (id_t)child, &info, WEXITED | WNOWAIT) != 0) {
    fail("waitid WNOWAIT");
    return;
  }
  char path[128];
  char line[4096];
  char thread_line[4096];
  snprintf(path, sizeof path, "/proc/%d/stat", (int)child);
  ssize_t a = read_file_at(AT_FDCWD, path, line, sizeof line);
  snprintf(path, sizeof path, "/proc/%d/task/%d/stat", (int)child, (int)child);
  ssize_t b = read_file_at(AT_FDCWD, path, thread_line, sizeof thread_line);
  if (a < 0 || b < 0) {
    fail("zombie stat");
  } else {
    printf("zombie: task stat same as stat=%d\n", a == b && memcmp(line, thread_line, (size_t)a) == 0);
    char copy[4096];
    memcpy(copy, line, sizeof line);
    print_stat("zombie stat", line, child, getpid(), child);
    char* f[64];
    if (split_stat(copy, f) == 52) {
      printf("zombie stat: name as own=%d cpu ticks=%lld\n", strcmp(f[1], own[1]) == 0,
             field_ll(f, 14) + field_ll(f, 15));
    }
  }
  snprintf(path, sizeof path, "/proc/%d/comm", (int)child);
  if (read_file_at(AT_FDCWD, path, line, sizeof line) < 0) {
    fail("zombie comm");
  } else {
    line[strcspn(line, "\n")] = 0;
    printf("zombie comm: name as own=%d\n", strcmp(line, own[1]) == 0);
  }
  int status;
  if (waitpid(child, &status, 0) != child || !WIFEXITED(status) || WEXITSTATUS(status) != 7) {
    fail("reap zombie");
  }
}

/* How a child of `deaths` ends. */
enum death { RAW_EXIT, SEGV, ABORT, KILLED };

static const char* const death_names[] = {"raw exit(3)", "SIGSEGV", "abort", "SIGKILL"};

/* A child that dies as `how` says; its parent reads its zombie's stat and
 * compares the exit code with what its own waitid reports. */
static void death(enum death how) {
  const char* name = death_names[how];
  pid_t child = fork();
  if (child < 0) {
    fail("fork death");
    return;
  }
  if (child == 0) {
    switch (how) {
      case RAW_EXIT:
        syscall(SYS_exit, 3);
        break;
      case SEGV:
        *(volatile int*)0 = 1;
        break;
      case ABORT:
        abort();
      case KILLED:
        for (;;) {
          pause();
        }
    }
    _exit(99);
  }
  if (how == KILLED && kill(child, SIGKILL) != 0) {
    fail("kill");
  }
  siginfo_t info;
  memset(&info, 0, sizeof info);
  if (waitid(P_PID, (id_t)child, &info, WEXITED | WNOWAIT) != 0) {
    fail("death waitid WNOWAIT");
    return;
  }
  int expected = info.si_code == CLD_EXITED   ? info.si_status << 8
                 : info.si_code == CLD_KILLED ? info.si_status
                 : info.si_code == CLD_DUMPED ? info.si_status | 0x80
                                              : -1;
  char path[64];
  char line[4096];
  char* f[64];
  snprintf(path, sizeof path, "/proc/%d/stat", (int)child);
  if (read_file_at(AT_FDCWD, path, line, sizeof line) < 0 || split_stat(line, f) != 52) {
    fail("death stat");
  } else {
    int code = (int)field_ll(f, 52);
    printf("%s zombie: state=%s si_code=%d si_status=%d exit_code=%d as its waitid shows=%d\n",
           name, f[2], info.si_code, info.si_status, code, code == expected);
    if (code != expected) {
      failures++;
    }
  }
  int status;
  if (waitpid(child, &status, 0) != child) {
    fail("reap death");
  }
}

int main(int argc, char** argv) {
  setvbuf(stdout, NULL, _IOLBF, 0);
  int sigkill = argc > 1 && strcmp(argv[1], "--sigkill") == 0;
  live();
  zombie();
  death(RAW_EXIT);
  death(SEGV);
  death(ABORT);
  if (sigkill) {
    death(KILLED);
  }
  printf("failures=%d\n", failures);
  return failures == 0 ? 0 : 1;
}
