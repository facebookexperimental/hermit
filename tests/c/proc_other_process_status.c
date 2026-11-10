/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Another process's /proc/<pid>/status and task/<tid>/status, and the
 * reader's own /proc/self/status, as Linux shows them (fs/proc/array.c
 * proc_pid_status), read by path and relative to a descriptor of the
 * process's directory (ps and pgrep open /proc/<pid> and then "status" in
 * it).
 *
 * A child names itself (PR_SET_NAME), starts a thread that names itself,
 * then blocks SIGUSR1 in its main thread, ignores SIGUSR2 and catches
 * SIGTERM, reports the thread's tid and blocks. The parent reads the
 * child's and the thread's status once each shows the task sleeping. A
 * second child exits with status 7 and the parent reads its status as a
 * zombie, before reaping it (waitid WNOWAIT). Last, the parent reads its
 * own.
 *
 * Every line is checked against what the reader's own queries answer in the
 * same run, not against fixed values: the ids against getpid, getppid,
 * getresuid and getresgid (and setfsuid(-1), setfsgid(-1)), Groups against
 * getgroups, NSpgid and NSsid against getpgid and getsid, the capability
 * lines against capget of the task (present exactly where capget answers
 * for it, with its sets), the reader's own CapBnd and CapAmb against
 * PR_CAPBSET_READ and PR_CAP_AMBIENT_IS_SET, and the affinity against
 * sched_getaffinity and get_mempolicy. Each status's line names are
 * printed, without:
 *   - the lines Hermit never renders (Detcore's OMITTED_STATUS_LINES:
 *     Umask, FDSize, CoreDumping, THP_enabled, untag_mask, SigQ,
 *     NoNewPrivs, Seccomp, Seccomp_filters, Speculation_Store_Bypass,
 *     SpeculationIndirectBranch), whose values no query of the guest's
 *     answers repeatably for every task;
 *   - the capability lines, checked against capget instead;
 *   - for a zombie, the signal-set lines, which no source reports for an
 *     exited task under Hermit (Linux prints them).
 * So one transcript describes every run. Standard output is the oracle; the
 * program exits 0 when every step it requires ran.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <linux/capability.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/fsuid.h>
#include <sys/prctl.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#define MPOL_F_MEMS_ALLOWED (1 << 2)

static int failures;

static void fail(const char* what) {
  printf("FAIL %s errno=%d\n", what, errno);
  failures++;
}

/* The status lines Hermit never renders (Detcore's OMITTED_STATUS_LINES). */
static const char* const omitted[] = {
    "Umask",           "FDSize",     "CoreDumping",
    "THP_enabled",     "untag_mask", "SigQ",
    "NoNewPrivs",      "Seccomp",    "Seccomp_filters",
    "Speculation_Store_Bypass",      "SpeculationIndirectBranch",
};

static const char* const capability_lines[] = {"CapInh", "CapPrm", "CapEff", "CapBnd", "CapAmb"};

static const char* const signal_lines[] = {"SigPnd", "ShdPnd", "SigBlk", "SigIgn", "SigCgt"};

static int listed_in(const char* name, const char* const* names, size_t count) {
  for (size_t i = 0; i < count; i++) {
    if (strcmp(name, names[i]) == 0) {
      return 1;
    }
  }
  return 0;
}

#define COUNT(array) (sizeof(array) / sizeof((array)[0]))

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

/* The value of status line `name` in `text` (after its tab), copied into
 * `value`; 0 where the line is absent. */
static int field(const char* text, const char* name, char* value, size_t size) {
  size_t length = strlen(name);
  for (const char* line = text; *line != 0;) {
    const char* end = strchr(line, '\n');
    size_t line_length = end == NULL ? strlen(line) : (size_t)(end - line);
    if (line_length > length && strncmp(line, name, length) == 0 && line[length] == ':') {
      const char* start = line + length + 1;
      while (*start == '\t' || *start == ' ') {
        start++;
      }
      size_t copied = (size_t)(line + line_length - start);
      if (copied >= size) {
        copied = size - 1;
      }
      memcpy(value, start, copied);
      value[copied] = 0;
      return 1;
    }
    if (end == NULL) {
      break;
    }
    line = end + 1;
  }
  return 0;
}

static long long number(const char* text, const char* name) {
  char value[256];
  if (!field(text, name, value, sizeof value)) {
    return -1;
  }
  return strtoll(value, NULL, 10);
}

static unsigned long long hex(const char* text, const char* name, int* present) {
  char value[256];
  *present = field(text, name, value, sizeof value);
  return *present ? strtoull(value, NULL, 16) : 0;
}

/* The status's line names in order, comma-separated, without the omitted
 * lines, the capability lines and, where `signals` is 0, the signal sets. */
static void names(const char* text, int signals, char* out, size_t size) {
  out[0] = 0;
  for (const char* line = text; *line != 0;) {
    const char* colon = strchr(line, ':');
    const char* end = strchr(line, '\n');
    if (colon == NULL || (end != NULL && colon > end)) {
      break;
    }
    char name[64];
    size_t length = (size_t)(colon - line);
    if (length >= sizeof name) {
      length = sizeof name - 1;
    }
    memcpy(name, line, length);
    name[length] = 0;
    if (!listed_in(name, omitted, COUNT(omitted)) &&
        !listed_in(name, capability_lines, COUNT(capability_lines)) &&
        (signals || !listed_in(name, signal_lines, COUNT(signal_lines)))) {
      if (out[0] != 0) {
        strncat(out, ",", size - strlen(out) - 1);
      }
      strncat(out, name, size - strlen(out) - 1);
    }
    if (end == NULL) {
      break;
    }
    line = end + 1;
  }
}

/* Whether the CapInh, CapPrm and CapEff lines are what capget of `pid`
 * answers: present with its sets where it answers, absent where it fails. */
static int capabilities_as_capget(const char* text, pid_t pid) {
  struct __user_cap_header_struct header = {_LINUX_CAPABILITY_VERSION_3, pid};
  struct __user_cap_data_struct data[2];
  memset(data, 0, sizeof data);
  int answered = syscall(SYS_capget, &header, data) == 0;
  int inh, prm, eff;
  unsigned long long inheritable = hex(text, "CapInh", &inh);
  unsigned long long permitted = hex(text, "CapPrm", &prm);
  unsigned long long effective = hex(text, "CapEff", &eff);
  if (!answered) {
    return !inh && !prm && !eff;
  }
  unsigned long long want_inh = data[0].inheritable | (unsigned long long)data[1].inheritable << 32;
  unsigned long long want_prm = data[0].permitted | (unsigned long long)data[1].permitted << 32;
  unsigned long long want_eff = data[0].effective | (unsigned long long)data[1].effective << 32;
  return inh && prm && eff && inheritable == want_inh && permitted == want_prm &&
         effective == want_eff;
}

/* Whether the Uid and Gid lines are the reader's real, effective, saved and
 * filesystem ids. */
static int ids_as_readers(const char* text) {
  uid_t ruid, euid, suid;
  gid_t rgid, egid, sgid;
  if (getresuid(&ruid, &euid, &suid) != 0 || getresgid(&rgid, &egid, &sgid) != 0) {
    return 0;
  }
  int fsuid = setfsuid((uid_t)-1);
  int fsgid = setfsgid((gid_t)-1);
  char want[128];
  char got[256];
  snprintf(want, sizeof want, "%u\t%u\t%u\t%u", ruid, euid, suid, (unsigned)fsuid);
  if (!field(text, "Uid", got, sizeof got) || strcmp(got, want) != 0) {
    return 0;
  }
  snprintf(want, sizeof want, "%u\t%u\t%u\t%u", rgid, egid, sgid, (unsigned)fsgid);
  return field(text, "Gid", got, sizeof got) && strcmp(got, want) == 0;
}

/* Whether the Groups line is the reader's getgroups answer. */
static int groups_as_readers(const char* text) {
  gid_t groups[256];
  int count = getgroups(256, groups);
  if (count < 0) {
    return 0;
  }
  char want[4096] = "";
  for (int i = 0; i < count; i++) {
    char one[16];
    snprintf(one, sizeof one, "%u ", groups[i]);
    strncat(want, one, sizeof want - strlen(want) - 1);
  }
  char got[4096];
  if (!field(text, "Groups", got, sizeof got)) {
    return 0;
  }
  /* The line's trailing space is part of Linux's format; compare up to it. */
  size_t length = strlen(want);
  while (length > 0 && want[length - 1] == ' ') {
    want[--length] = 0;
  }
  length = strlen(got);
  while (length > 0 && got[length - 1] == ' ') {
    got[--length] = 0;
  }
  return strcmp(got, want) == 0;
}

/* How many CPUs or nodes a kernel list (`0-3,5`) names. */
static int list_count(const char* list) {
  int count = 0;
  const char* at = list;
  while (*at != 0) {
    char* end;
    long first = strtol(at, &end, 10);
    long last = first;
    if (*end == '-') {
      last = strtol(end + 1, &end, 10);
    }
    count += (int)(last - first + 1);
    if (*end != ',') {
      break;
    }
    at = end + 1;
  }
  return count;
}

/* Whether the affinity lines are what sched_getaffinity of `pid` and
 * get_mempolicy(MPOL_F_MEMS_ALLOWED) answer. */
static int affinity_as_queries(const char* text, pid_t pid, int* mems) {
  cpu_set_t set;
  CPU_ZERO(&set);
  char value[4096];
  int cpus = sched_getaffinity(pid, sizeof set, &set) == 0 &&
             field(text, "Cpus_allowed_list", value, sizeof value) &&
             list_count(value) == CPU_COUNT(&set);
  unsigned long nodes[16] = {0};
  long answered = syscall(SYS_get_mempolicy, NULL, nodes, 1024, NULL, MPOL_F_MEMS_ALLOWED);
  int count = 0;
  for (size_t i = 0; i < COUNT(nodes); i++) {
    count += __builtin_popcountl(nodes[i]);
  }
  *mems = answered == 0 && field(text, "Mems_allowed_list", value, sizeof value) &&
          list_count(value) == count;
  return cpus;
}

static void print_lines(const char* label, const char* text, int signals) {
  char out[4096];
  names(text, signals, out, sizeof out);
  printf("%s: lines=%s\n", label, out);
}

/* The lines of another task's status every run agrees on. */
static void print_task(const char* label, const char* text, pid_t pid, pid_t tgid, int threads) {
  char name[64] = "";
  char state[64] = "";
  field(text, "Name", name, sizeof name);
  field(text, "State", state, sizeof state);
  printf("%s: name=%s state=%s pid,nspid match=%d tgid,nstgid match=%d ppid is reader=%d "
         "tracerpid=%lld threads=%lld (want %d)\n",
         label, name, state, number(text, "Pid") == pid && number(text, "NSpid") == pid,
         number(text, "Tgid") == tgid && number(text, "NStgid") == tgid,
         number(text, "PPid") == getpid(), number(text, "TracerPid"), number(text, "Threads"),
         threads);
  printf("%s: uid,gid as the reader's=%d groups as the reader's=%d "
         "nspgid,nssid as getpgid,getsid=%d ngid,kthread=%lld,%lld\n",
         label, ids_as_readers(text), groups_as_readers(text),
         number(text, "NSpgid") == getpgid(tgid) && number(text, "NSsid") == getsid(tgid),
         number(text, "Ngid"), number(text, "Kthread"));
  int mems;
  int cpus = affinity_as_queries(text, pid, &mems);
  printf("%s: capability lines as capget answers=%d cpus as sched_getaffinity=%d mems as "
         "get_mempolicy=%d\n",
         label, capabilities_as_capget(text, pid), cpus, mems);
}

static void print_masks(const char* label, const char* text) {
  int blk, ign, cgt;
  unsigned long long blocked = hex(text, "SigBlk", &blk);
  unsigned long long ignored = hex(text, "SigIgn", &ign);
  unsigned long long caught = hex(text, "SigCgt", &cgt);
  printf("%s: SIGUSR1 blocked=%d SIGUSR2 ignored=%d SIGTERM caught=%d\n", label,
         blk && (blocked >> (SIGUSR1 - 1) & 1), ign && (ignored >> (SIGUSR2 - 1) & 1),
         cgt && (caught >> (SIGTERM - 1) & 1));
}

static int ready_pipe[2];
static int go_pipe[2];

static void on_term(int signal) { (void)signal; }

static void* named_thread(void* arg) {
  (void)arg;
  if (prctl(PR_SET_NAME, "status-thread", 0, 0, 0) != 0) {
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

static void live_child(void) {
  if (prctl(PR_SET_NAME, "status-child", 0, 0, 0) != 0) {
    fail("child PR_SET_NAME");
  }
  pthread_t thread;
  if (pthread_create(&thread, NULL, named_thread, NULL) != 0) {
    fail("pthread_create");
    _exit(1);
  }
  sigset_t usr1;
  sigemptyset(&usr1);
  sigaddset(&usr1, SIGUSR1);
  if (pthread_sigmask(SIG_BLOCK, &usr1, NULL) != 0 || signal(SIGUSR2, SIG_IGN) == SIG_ERR ||
      signal(SIGTERM, on_term) == SIG_ERR) {
    fail("child signal setup");
  }
  pid_t ready = 0;
  if (write(ready_pipe[1], &ready, sizeof ready) != sizeof ready) {
    fail("child write");
  }
  char byte;
  if (read(go_pipe[0], &byte, 1) != 1) {
    fail("child read");
  }
  pthread_join(thread, NULL);
  _exit(failures == 0 ? 0 : 1);
}

/* Read `path` until the task it describes is sleeping (natively the task
 * may still be on its way into its blocking read), at most a few seconds. */
static ssize_t read_sleeping(int dirfd, const char* path, char* text, size_t size) {
  ssize_t n = -1;
  for (int attempt = 0; attempt < 5000; attempt++) {
    n = read_file_at(dirfd, path, text, size);
    char state[64];
    if (n < 0 || (field(text, "State", state, sizeof state) && state[0] == 'S')) {
      return n;
    }
    usleep(1000);
  }
  return n;
}

static void live(void) {
  if (pipe(ready_pipe) != 0 || pipe(go_pipe) != 0) {
    fail("pipe");
    return;
  }
  pid_t child = fork();
  if (child < 0) {
    fail("fork");
    return;
  }
  if (child == 0) {
    live_child();
  }
  pid_t first, second;
  if (read(ready_pipe[0], &first, sizeof first) != sizeof first ||
      read(ready_pipe[0], &second, sizeof second) != sizeof second) {
    fail("read child report");
    return;
  }
  pid_t tid = first != 0 ? first : second;
  char path[128];
  char text[8192];
  char relative[8192];

  snprintf(path, sizeof path, "/proc/%d/status", (int)child);
  if (read_sleeping(AT_FDCWD, path, text, sizeof text) < 0) {
    fail("child status");
  } else {
    print_task("child status", text, child, child, 2);
    print_masks("child status", text);
    print_lines("child status", text, 1);
  }

  snprintf(path, sizeof path, "/proc/%d", (int)child);
  int dirfd = open(path, O_RDONLY | O_DIRECTORY);
  if (dirfd < 0) {
    fail("open child directory");
  } else {
    if (read_file_at(dirfd, "status", relative, sizeof relative) < 0) {
      fail("child status relative");
    } else {
      char by_path[4096];
      char by_descriptor[4096];
      names(text, 1, by_path, sizeof by_path);
      names(relative, 1, by_descriptor, sizeof by_descriptor);
      char name[64] = "";
      field(relative, "Name", name, sizeof name);
      printf("child status (relative): lines as by path=%d name=%s\n",
             strcmp(by_path, by_descriptor) == 0, name);
    }
    snprintf(path, sizeof path, "task/%d/status", (int)tid);
    if (read_sleeping(dirfd, path, text, sizeof text) < 0) {
      fail("thread status");
    } else {
      print_task("thread status", text, tid, child, 2);
      print_masks("thread status", text);
      print_lines("thread status", text, 1);
    }
    close(dirfd);
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
  char text[8192];
  char own[8192];
  snprintf(path, sizeof path, "/proc/%d/status", (int)child);
  if (read_file_at(AT_FDCWD, path, text, sizeof text) < 0 ||
      read_file_at(AT_FDCWD, "/proc/self/status", own, sizeof own) < 0) {
    fail("zombie status");
  } else {
    char name[64] = "";
    char own_name[64] = "";
    field(text, "Name", name, sizeof name);
    field(own, "Name", own_name, sizeof own_name);
    print_task("zombie status", text, child, child, 1);
    printf("zombie status: name as the reader's=%d\n", strcmp(name, own_name) == 0);
    print_lines("zombie status", text, 0);
  }
  int status;
  if (waitpid(child, &status, 0) != child || !WIFEXITED(status) || WEXITSTATUS(status) != 7) {
    fail("reap zombie");
  }
}

/* The reader's own bounding or ambient set, as its prctl answers. */
static unsigned long long own_set(int ambient) {
  unsigned long long set = 0;
  for (int capability = 0; capability < 64; capability++) {
    int answer = ambient ? prctl(PR_CAP_AMBIENT, PR_CAP_AMBIENT_IS_SET, capability, 0, 0)
                         : prctl(PR_CAPBSET_READ, capability, 0, 0, 0);
    if (answer < 0) {
      break;
    }
    if (answer > 0) {
      set |= 1ULL << capability;
    }
  }
  return set;
}

static void own_status(void) {
  char text[8192];
  if (read_file_at(AT_FDCWD, "/proc/self/status", text, sizeof text) < 0) {
    fail("own status");
    return;
  }
  printf("own status: pid,tgid match=%d ppid as getppid=%d threads=%lld\n",
         number(text, "Pid") == getpid() && number(text, "Tgid") == getpid(),
         number(text, "PPid") == getppid(), number(text, "Threads"));
  printf("own status: uid,gid as getresuid,getresgid=%d groups as getgroups=%d\n",
         ids_as_readers(text), groups_as_readers(text));
  int bnd, amb;
  unsigned long long bounding = hex(text, "CapBnd", &bnd);
  unsigned long long ambient = hex(text, "CapAmb", &amb);
  printf("own status: capability lines as capget answers=%d bounding as PR_CAPBSET_READ=%d "
         "ambient as PR_CAP_AMBIENT=%d\n",
         capabilities_as_capget(text, 0), bnd && bounding == own_set(0),
         amb && ambient == own_set(1));
  print_lines("own status", text, 1);
}

int main(void) {
  setvbuf(stdout, NULL, _IOLBF, 0);
  live();
  zombie();
  own_status();
  printf("failures=%d\n", failures);
  return failures == 0 ? 0 : 1;
}
