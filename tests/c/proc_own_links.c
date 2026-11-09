/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * proc_own_links: readlink of the caller's own /proc/<pid>/exe,
 * /proc/<pid>/cwd and /proc/<pid>/root links, as Linux answers them
 * (package K slice E1). Under Hermit, Detcore answers them from its own
 * knowledge on every backend: exe from its exec record, cwd from the calling
 * thread's getcwd, root as "/".
 *
 * It prints, for each link: that every spelling (self, the pid, thread-self,
 * self/task/<tid>, <pid>/task/<tid>, //proc/./self, and readlinkat of
 * "self/<link>" from a /proc descriptor) reads the same target; what the
 * target is (exe: argv[0]'s realpath; cwd: getcwd's answer; root: "/");
 * readlink into a 3-byte buffer (at most 3 bytes, no NUL, the rest of the
 * buffer untouched); EINVAL for a zero size and EFAULT for an unwritable
 * buffer; and the spellings that follow the link (exe/: ENOTDIR; cwd/ and
 * root/: EINVAL). Then: a task outside the process below its task
 * directory (ENOENT); the working directory after an absolute and a relative
 * chdir, and after the directory it is in was removed ("<path> (deleted)"
 * on Linux); a thread's links, read by the thread and by the main thread
 * through /proc/<tid> and /proc/self/task/<tid>, and a chdir in the thread,
 * which the process shares; a fork's child, whose chdir leaves its parent's
 * working directory alone; a process whose leader has exited, where the
 * process's links are gone (ENOENT) and the thread's remain; and a copy of
 * the program exec'd from a scratch directory, whose exe link names the copy
 * until it unlinks it ("<path> (deleted)"). Paths below the scratch directory
 * are printed from "<dir>". A child's failed checks fail its parent.
 *
 * usage: proc_own_links [--no-leader-exit] [--no-threads] [--no-exec]
 *                       [--no-fs-threads] [--no-exe-changes]
 *                       [--base-exe-truncation]
 *        proc_own_links --after-exec DIR [--no-exe-changes]
 *   --no-leader-exit: skip the process whose leader exits (DBT and SaBRe
 *   cannot run it).
 *   --no-threads: skip the thread and the process whose leader exits (the
 *   in-guest LiteInst backends create no threads).
 *   --no-exec: skip the exec'd copy (the in-guest LiteInst backends run no
 *   exec).
 *   --no-fs-threads: skip the thread created without CLONE_FS (KVM refuses
 *   it).
 *   --no-exe-changes: skip the exec'd copy's changes to its own image
 *   (DBT refuses an exe link of an image no longer untouched at its path,
 *   by name).
 *   --base-exe-truncation: check the exe link's answer for a 3-byte buffer
 *   against DBT's base answer, EINVAL after writing the 3 bytes
 *   (https://github.com/rrnewton/hermit/issues/4711).
 */

#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sched.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>
#include <linux/capability.h>

static int failures;

#define CHECK(cond, ...)                                  \
  do {                                                    \
    if (!(cond)) {                                        \
      failures++;                                         \
      fprintf(stderr, "FAIL %s:%d: ", __FILE__, __LINE__); \
      fprintf(stderr, __VA_ARGS__);                       \
      fputc('\n', stderr);                                \
    }                                                     \
  } while (0)

static const char *errno_name(int error) {
  switch (error) {
    case 0:
      return "0";
    case EINVAL:
      return "EINVAL";
    case ENOENT:
      return "ENOENT";
    case ENOTDIR:
      return "ENOTDIR";
    case EFAULT:
      return "EFAULT";
    case EACCES:
      return "EACCES";
    default: {
      static char number[2][32];
      static int which;
      which ^= 1;
      snprintf(number[which], sizeof(number[which]), "%d", error);
      return number[which];
    }
  }
}

/* The scratch directory, which paths below it are printed from. */
static char scratch[64];

/* `path` with a leading scratch directory printed as "<dir>". */
static const char *shown(const char *path) {
  static char buffer[2][PATH_MAX + 16];
  static int which;
  size_t length = strlen(scratch);
  which ^= 1;
  if (length > 0 && strncmp(path, scratch, length) == 0 &&
      (path[length] == '\0' || path[length] == '/' || path[length] == ' ')) {
    snprintf(buffer[which], sizeof(buffer[which]), "<dir>%s", path + length);
    return buffer[which];
  }
  return path;
}

/* readlink of `path` into `target` (NUL-terminated here), or -1 with errno. */
static ssize_t read_link(const char *path, char *target, size_t size) {
  errno = 0;
  ssize_t length = readlink(path, target, size - 1);
  if (length >= 0) {
    target[length] = '\0';
  }
  return length;
}

static int proc_directory = -1;

/* The process's image, as /proc/self/exe named it at the start. */
static char exe[PATH_MAX + 16];

/* Every spelling of the caller's own `link` reads `target`; returns how many
 * spellings agree, out of `*spellings`. */
static int spellings_agree(const char *link, const char *target, int *spellings) {
  pid_t pid = getpid();
  pid_t tid = (pid_t)syscall(SYS_gettid);
  char paths[6][96];
  snprintf(paths[0], sizeof(paths[0]), "/proc/self/%s", link);
  snprintf(paths[1], sizeof(paths[1]), "/proc/%d/%s", pid, link);
  snprintf(paths[2], sizeof(paths[2]), "/proc/thread-self/%s", link);
  snprintf(paths[3], sizeof(paths[3]), "/proc/self/task/%d/%s", tid, link);
  snprintf(paths[4], sizeof(paths[4]), "/proc/%d/task/%d/%s", pid, tid, link);
  snprintf(paths[5], sizeof(paths[5]), "//proc/./self/%s", link);
  int agree = 0;
  char read_target[PATH_MAX + 16];
  for (int i = 0; i < 6; i++) {
    if (read_link(paths[i], read_target, sizeof(read_target)) >= 0 &&
        strcmp(read_target, target) == 0) {
      agree++;
    } else {
      fprintf(stderr, "spelling %s read %s errno=%s\n", paths[i], read_target,
              errno_name(errno));
    }
  }
  char relative[32];
  snprintf(relative, sizeof(relative), "self/%s", link);
  errno = 0;
  ssize_t length = readlinkat(proc_directory, relative, read_target, sizeof(read_target) - 1);
  if (length >= 0) {
    read_target[length] = '\0';
    if (strcmp(read_target, target) == 0) {
      agree++;
    }
  } else {
    fprintf(stderr, "readlinkat /proc %s errno=%s\n", relative, errno_name(errno));
  }
  *spellings = 7;
  return agree;
}

static void print_link(const char *label, const char *path);

/* Each of the seven spellings of `link` and what it reads, one line each. */
static void print_spellings(const char *label, const char *link) {
  pid_t pid = getpid();
  pid_t tid = (pid_t)syscall(SYS_gettid);
  static const char *names[] = {"self", "<pid>", "thread-self", "self/task/<tid>",
                                "<pid>/task/<tid>", "//proc/./self"};
  char paths[6][96];
  snprintf(paths[0], sizeof(paths[0]), "/proc/self/%s", link);
  snprintf(paths[1], sizeof(paths[1]), "/proc/%d/%s", pid, link);
  snprintf(paths[2], sizeof(paths[2]), "/proc/thread-self/%s", link);
  snprintf(paths[3], sizeof(paths[3]), "/proc/self/task/%d/%s", tid, link);
  snprintf(paths[4], sizeof(paths[4]), "/proc/%d/task/%d/%s", pid, tid, link);
  snprintf(paths[5], sizeof(paths[5]), "//proc/./self/%s", link);
  for (int i = 0; i < 6; i++) {
    char shown_label[160];
    snprintf(shown_label, sizeof(shown_label), "%s, %s/%s", label, names[i], link);
    print_link(shown_label, paths[i]);
  }
  char relative[32], target[PATH_MAX + 16];
  snprintf(relative, sizeof(relative), "self/%s", link);
  errno = 0;
  ssize_t length = readlinkat(proc_directory, relative, target, sizeof(target) - 1);
  if (length >= 0) {
    target[length] = '\0';
    printf("%s, readlinkat(/proc, self/%s)=%s\n", label, link, shown(target));
  } else {
    printf("%s, readlinkat(/proc, self/%s)=-1 errno=%s\n", label, link, errno_name(errno));
  }
}

/* Whether the exe link's truncation to a short buffer is Linux's (DBT
 * answers EINVAL there after writing the bytes, as at the base:
 * --base-exe-truncation, https://github.com/rrnewton/hermit/issues/4711). */
static int exe_truncation_linux = 1;

/* The checks every link takes: spellings, truncation, EINVAL, EFAULT. */
static void link_contract(const char *link, const char *target) {
  int spellings;
  int agree = spellings_agree(link, target, &spellings);
  printf("%s: spellings=%d same=%d\n", link, spellings, agree);
  CHECK(agree == spellings, "%s: %d of %d spellings agree", link, agree, spellings);

  char path[32];
  snprintf(path, sizeof(path), "/proc/self/%s", link);
  char small[8];
  memset(small, '#', sizeof(small));
  errno = 0;
  ssize_t length = readlink(path, small, 3);
  int error = length < 0 ? errno : 0;
  size_t expected = strlen(target) < 3 ? strlen(target) : 3;
  int written = memcmp(small, target, expected) == 0;
  int untouched = 1;
  for (size_t i = expected; i < sizeof(small); i++) {
    untouched &= small[i] == '#';
  }
  printf("%s: 3-byte buffer n=%zd errno=%s written=%d untouched=%d\n", link, length,
         errno_name(error), written, untouched);
  if (exe_truncation_linux || strcmp(link, "exe") != 0) {
    CHECK(length == (ssize_t)expected && written && untouched, "%s: 3-byte buffer", link);
  } else {
    CHECK(length == -1 && error == EINVAL && written && untouched,
          "%s: 3-byte buffer, DBT's base answer", link);
  }

  errno = 0;
  length = syscall(SYS_readlink, path, small, 0);
  printf("%s: size 0=%zd errno=%s\n", link, length, errno_name(length < 0 ? errno : 0));
  errno = 0;
  length = syscall(SYS_readlink, path, (char *)8, 64);
  printf("%s: unwritable buffer=%zd errno=%s\n", link, length,
         errno_name(length < 0 ? errno : 0));
  /* A buffer that is mapped but read-only. */
  char *readonly = mmap(NULL, 4096, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  CHECK(readonly != MAP_FAILED, "mmap: %s", strerror(errno));
  errno = 0;
  length = syscall(SYS_readlink, path, readonly, 64);
  printf("%s: read-only buffer=%zd errno=%s untouched=%d\n", link, length,
         errno_name(length < 0 ? errno : 0), readonly[0] == 0 && readonly[1] == 0);
  munmap(readonly, 4096);
}

static void print_link(const char *label, const char *path) {
  char target[PATH_MAX + 16];
  ssize_t length = read_link(path, target, sizeof(target));
  if (length >= 0) {
    printf("%s=%s\n", label, shown(target));
  } else {
    printf("%s=-1 errno=%s\n", label, errno_name(errno));
  }
}

/* What the main thread passes the thread, and the thread's results. */
struct thread_state {
  int ready[2];
  int release[2];
  pid_t tid;
  char exe[PATH_MAX + 16];
};

static void *thread_links(void *argument) {
  struct thread_state *state = argument;
  state->tid = (pid_t)syscall(SYS_gettid);
  char cwd[PATH_MAX];
  CHECK(getcwd(cwd, sizeof(cwd)) != NULL, "thread getcwd: %s", strerror(errno));
  int spellings;
  int agree = spellings_agree("exe", state->exe, &spellings);
  printf("thread: exe spellings=%d same=%d\n", spellings, agree);
  CHECK(agree == spellings, "thread exe spellings");
  agree = spellings_agree("cwd", cwd, &spellings);
  printf("thread: cwd spellings=%d same=%d\n", spellings, agree);
  CHECK(agree == spellings, "thread cwd spellings");
  agree = spellings_agree("root", "/", &spellings);
  printf("thread: root spellings=%d same=%d\n", spellings, agree);
  CHECK(agree == spellings, "thread root spellings");
  /* The thread's own number names a directory with a task directory too. */
  char own[96], target[PATH_MAX + 16];
  snprintf(own, sizeof(own), "/proc/%d/task/%d/root", state->tid, state->tid);
  print_link("thread: /proc/<tid>/task/<tid>/root", own);
  snprintf(own, sizeof(own), "/proc/%d/task/%d/cwd", state->tid, state->tid);
  print_link("thread: /proc/<tid>/task/<tid>/cwd", own);
  snprintf(own, sizeof(own), "/proc/%d/task/%d/exe", state->tid, state->tid);
  ssize_t length = read_link(own, target, sizeof(target));
  printf("thread: /proc/<tid>/task/<tid>/exe is the process's=%d\n",
         length >= 0 && strcmp(target, state->exe) == 0);
  CHECK(chdir("sub") == 0, "thread chdir: %s", strerror(errno));
  print_link("thread: after its chdir(\"sub\"), thread-self/cwd", "/proc/thread-self/cwd");
  print_link("thread: after its chdir(\"sub\"), self/cwd", "/proc/self/cwd");
  fflush(stdout);
  char byte = 1;
  CHECK(write(state->ready[1], &byte, 1) == 1, "thread ready");
  CHECK(read(state->release[0], &byte, 1) == 1, "thread release");
  return NULL;
}

/* A thread created without CLONE_FS: its working directory is its own, and
 * its process's cwd link is the leader's. */
static char fs_stack[64 * 1024] __attribute__((aligned(16)));
static int fs_done[2];
static int fs_thread(void *argument) {
  (void)argument;
  CHECK(chdir("sub") == 0, "fs thread chdir: %s", strerror(errno));
  print_link("fs thread: after its own chdir(\"sub\"), thread-self/cwd", "/proc/thread-self/cwd");
  print_link("fs thread: after its own chdir(\"sub\"), self/cwd", "/proc/self/cwd");
  fflush(stdout);
  char byte = 1;
  CHECK(write(fs_done[1], &byte, 1) == 1, "fs thread done");
  syscall(SYS_exit, 0);
  return 0;
}

/* A thread of a process whose leader has exited: the process's links are
 * gone, the thread's are its own. */
static void *orphaned_thread(void *argument) {
  pthread_t leader = *(pthread_t *)argument;
  CHECK(pthread_join(leader, NULL) == 0, "join the leader");
  /* The leader is a zombie once the join returns. */
  static const char *links[] = {"exe", "cwd", "root"};
  for (int i = 0; i < 3; i++) {
    char path[48], label[64];
    snprintf(path, sizeof(path), "/proc/self/%s", links[i]);
    snprintf(label, sizeof(label), "leader exited: self/%s", links[i]);
    print_link(label, path);
    snprintf(path, sizeof(path), "/proc/thread-self/%s", links[i]);
    snprintf(label, sizeof(label), "leader exited: thread-self/%s", links[i]);
    if (i == 0) {
      char target[PATH_MAX + 16];
      ssize_t length = read_link(path, target, sizeof(target));
      printf("%s is the process's image=%d\n", label, length >= 0 && strcmp(target, exe) == 0);
      CHECK(length >= 0 && strcmp(target, exe) == 0, "%s", label);
    } else {
      print_link(label, path);
    }
  }
  fflush(stdout);
  exit(failures);
}

static int wait_child(pid_t child, const char *label) {
  int status = 0;
  CHECK(waitpid(child, &status, 0) == child, "%s: waitpid: %s", label, strerror(errno));
  int exited = WIFEXITED(status) ? WEXITSTATUS(status) : 128 + WTERMSIG(status);
  printf("%s exited=%d\n", label, exited);
  CHECK(exited == 0, "%s exited %d", label, exited);
  return exited;
}

/* The exec'd copy: its exe link names the copy until it unlinks it. */
/* The exe link by every spelling, each printed, the canonical one checked to
 * be `expected`. */
static void exe_link_is(const char *label, const char *expected) {
  char shown_label[96];
  snprintf(shown_label, sizeof(shown_label), "copy: %s", label);
  print_spellings(shown_label, "exe");
  char target[PATH_MAX + 16];
  CHECK(read_link("/proc/self/exe", target, sizeof(target)) >= 0 && strcmp(target, expected) == 0,
        "copy: %s: exe is %s", label, expected);
}

/* The exec'd copy, at <dir>/a/copy with a second link at <dir>/b/copy: its
 * exe link names the dentry it was exec'd through, as that dentry is now. */
static int after_exec(const char *directory, int changes) {
  snprintf(scratch, sizeof(scratch), "%s", directory);
  proc_directory = open("/proc", O_RDONLY | O_DIRECTORY | O_CLOEXEC);
  char a[PATH_MAX], b[PATH_MAX], away[PATH_MAX], copy[PATH_MAX], moved[PATH_MAX];
  char renamed[PATH_MAX], deleted[PATH_MAX + 16], other[PATH_MAX];
  snprintf(a, sizeof(a), "%s/a", scratch);
  snprintf(b, sizeof(b), "%s/b", scratch);
  snprintf(away, sizeof(away), "%s/away", scratch);
  snprintf(copy, sizeof(copy), "%s/a/copy", scratch);
  snprintf(moved, sizeof(moved), "%s/away/copy", scratch);
  snprintf(renamed, sizeof(renamed), "%s/away/renamed", scratch);
  snprintf(deleted, sizeof(deleted), "%s (deleted)", renamed);
  exe_link_is("as exec'd", copy);
  print_link("copy: cwd", "/proc/self/cwd");
  if (!changes) {
    snprintf(other, sizeof(other), "%s/b/copy", scratch);
    CHECK(unlink(copy) == 0 && unlink(other) == 0 && rmdir(a) == 0 && rmdir(b) == 0,
          "clean up: %s", strerror(errno));
    fflush(stdout);
    return failures;
  }
  /* The image's directory renamed away and the directory holding its second
   * link renamed into its place: the file at the old path is the same,
   * untouched inode, and Linux's link names the new path. */
  CHECK(rename(a, away) == 0 && rename(b, a) == 0, "swap the directories: %s",
        strerror(errno));
  exe_link_is("after its directory was swapped", moved);
  /* The same path after a chmod, the new one after a rename, and
   * " (deleted)" after an unlink (the second link lives on). */
  CHECK(chmod(moved, 0700) == 0, "chmod the copy: %s", strerror(errno));
  exe_link_is("after a chmod of itself", moved);
  CHECK(rename(moved, renamed) == 0, "rename the copy: %s", strerror(errno));
  exe_link_is("after renaming itself", renamed);
  CHECK(unlink(renamed) == 0, "unlink the copy: %s", strerror(errno));
  exe_link_is("after unlinking itself", deleted);
  CHECK(unlink(copy) == 0 && rmdir(a) == 0 && rmdir(away) == 0, "clean up: %s",
        strerror(errno));
  fflush(stdout);
  return failures;
}

int main(int argc, char **argv) {
  if (argc >= 3 && strcmp(argv[1], "--after-exec") == 0) {
    return after_exec(argv[2], argc == 3);
  }
  int leader_exit = 1, threads = 1, exec_copy = 1, fs_threads = 1, exe_changes = 1;
  for (int arg = 1; arg < argc; arg++) {
    if (strcmp(argv[arg], "--no-leader-exit") == 0) {
      leader_exit = 0;
    } else if (strcmp(argv[arg], "--no-threads") == 0) {
      threads = 0;
      leader_exit = 0;
      fs_threads = 0;
    } else if (strcmp(argv[arg], "--no-fs-threads") == 0) {
      fs_threads = 0;
    } else if (strcmp(argv[arg], "--no-exe-changes") == 0) {
      exe_changes = 0;
    } else if (strcmp(argv[arg], "--base-exe-truncation") == 0) {
      exe_truncation_linux = 0;
    } else if (strcmp(argv[arg], "--no-exec") == 0) {
      exec_copy = 0;
    }
  }
  proc_directory = open("/proc", O_RDONLY | O_DIRECTORY | O_CLOEXEC);
  CHECK(proc_directory >= 0, "open /proc: %s", strerror(errno));

  /* exe: argv[0]'s realpath, by every spelling. */
  CHECK(read_link("/proc/self/exe", exe, sizeof(exe)) > 0, "readlink exe: %s",
        errno_name(errno));
  char resolved[PATH_MAX];
  if (strchr(argv[0], '/') != NULL && realpath(argv[0], resolved) != NULL) {
    printf("exe: is argv[0]'s realpath=%d\n", strcmp(resolved, exe) == 0);
    CHECK(strcmp(resolved, exe) == 0, "exe %s is not argv[0]'s realpath %s", exe, resolved);
  } else {
    printf("exe: is argv[0]'s realpath=1\n");
  }
  link_contract("exe", exe);

  /* cwd: getcwd's answer. */
  char cwd[PATH_MAX];
  CHECK(getcwd(cwd, sizeof(cwd)) != NULL, "getcwd: %s", strerror(errno));
  char target[PATH_MAX + 16];
  CHECK(read_link("/proc/self/cwd", target, sizeof(target)) > 0, "readlink cwd");
  printf("cwd: is getcwd=%d\n", strcmp(target, cwd) == 0);
  CHECK(strcmp(target, cwd) == 0, "cwd %s is not getcwd's %s", target, cwd);
  link_contract("cwd", cwd);

  /* root: "/". */
  print_link("root", "/proc/self/root");
  link_contract("root", "/");

  /* Spellings that follow the link, and a task outside the process. */
  print_link("followed: self/exe/", "/proc/self/exe/");
  print_link("followed: self/cwd/", "/proc/self/cwd/");
  print_link("followed: thread-self/root/", "/proc/thread-self/root/");
  /* A "." after the link: a lookup in the directory it leads to. */
  print_link("dot: self/root/.", "/proc/self/root/.");
  print_link("dot: self/cwd/.", "/proc/self/cwd/.");
  print_link("outside: self/task/999999/exe", "/proc/self/task/999999/exe");
  print_link("outside: self/task/999999/cwd", "/proc/self/task/999999/cwd");

  /* The working directory after chdir. */
  char template[] = "/tmp/proc-own-links.XXXXXX";
  CHECK(mkdtemp(template) != NULL, "mkdtemp: %s", strerror(errno));
  snprintf(scratch, sizeof(scratch), "%s", template);
  CHECK(chdir(scratch) == 0, "chdir scratch: %s", strerror(errno));
  print_link("after chdir(<dir>): cwd", "/proc/self/cwd");
  CHECK(mkdir("sub", 0755) == 0, "mkdir sub: %s", strerror(errno));
  CHECK(chdir("sub") == 0, "chdir sub: %s", strerror(errno));
  print_link("after chdir(\"sub\"): cwd", "/proc/self/cwd");
  print_link("after chdir(\"sub\"): thread-self/cwd", "/proc/thread-self/cwd");
  CHECK(chdir("..") == 0, "chdir ..: %s", strerror(errno));
  print_link("after chdir(\"..\"): cwd", "/proc/self/cwd");
  /* A "." after the cwd link of a directory the caller may not search:
   * EACCES, once the capabilities that override that check are dropped. */
  fflush(stdout);
  CHECK(mkdir("locked", 0755) == 0, "mkdir locked: %s", strerror(errno));
  pid_t locker = fork();
  if (locker == 0) {
    failures = 0;
    CHECK(chdir("locked") == 0, "chdir locked: %s", strerror(errno));
    struct __user_cap_header_struct header = {_LINUX_CAPABILITY_VERSION_3, 0};
    struct __user_cap_data_struct data[2];
    memset(data, 0, sizeof(data));
    errno = 0;
    long dropped = syscall(SYS_capset, &header, data);
    printf("denied search: capset=%ld errno=%s\n", dropped, errno_name(dropped < 0 ? errno : 0));
    CHECK(chmod(".", 0) == 0, "chmod locked: %s", strerror(errno));
    print_link("denied search: self/cwd/.", "/proc/self/cwd/.");
    print_link("denied search: self/cwd", "/proc/self/cwd");
    fflush(stdout);
    _exit(failures);
  }
  wait_child(locker, "denied search child");
  CHECK(rmdir("locked") == 0, "rmdir locked: %s", strerror(errno));
  CHECK(mkdir("gone", 0755) == 0, "mkdir gone: %s", strerror(errno));
  CHECK(chdir("gone") == 0, "chdir gone: %s", strerror(errno));
  char gone[PATH_MAX];
  snprintf(gone, sizeof(gone), "%s/gone", scratch);
  CHECK(rmdir(gone) == 0, "rmdir gone: %s", strerror(errno));
  print_link("removed: cwd", "/proc/self/cwd");
  errno = 0;
  char *answered = getcwd(cwd, sizeof(cwd));
  printf("removed: getcwd=%s errno=%s\n", answered ? shown(answered) : "NULL",
         errno_name(answered ? 0 : errno));
  CHECK(chdir(scratch) == 0, "chdir back: %s", strerror(errno));

  /* A thread, which shares the process's working directory. */
  fflush(stdout);
  if (threads) {
    struct thread_state state;
    memset(&state, 0, sizeof(state));
    snprintf(state.exe, sizeof(state.exe), "%s", exe);
    CHECK(pipe(state.ready) == 0 && pipe(state.release) == 0, "pipes");
    pthread_t thread;
    CHECK(pthread_create(&thread, NULL, thread_links, &state) == 0, "pthread_create");
    char byte;
    CHECK(read(state.ready[0], &byte, 1) == 1, "wait for the thread");
    char through[96];
    snprintf(through, sizeof(through), "/proc/%d/exe", state.tid);
    CHECK(read_link(through, target, sizeof(target)) >= 0, "readlink %s", through);
    printf("main: the thread's exe by /proc/<tid>/exe is the process's=%d\n",
           strcmp(target, exe) == 0);
    CHECK(strcmp(target, exe) == 0, "%s", through);
    snprintf(through, sizeof(through), "/proc/self/task/%d/exe", state.tid);
    CHECK(read_link(through, target, sizeof(target)) >= 0, "readlink %s", through);
    printf("main: the thread's exe by self/task/<tid>/exe is the process's=%d\n",
           strcmp(target, exe) == 0);
    CHECK(strcmp(target, exe) == 0, "%s", through);
    print_link("main: after the thread's chdir, self/cwd", "/proc/self/cwd");
    errno = 0;
    answered = getcwd(cwd, sizeof(cwd));
    printf("main: after the thread's chdir, getcwd=%s\n", answered ? shown(answered) : "NULL");
    byte = 1;
    CHECK(write(state.release[1], &byte, 1) == 1, "release the thread");
    CHECK(pthread_join(thread, NULL) == 0, "pthread_join");
    CHECK(chdir(scratch) == 0, "chdir back: %s", strerror(errno));
  }

  /* A thread created without CLONE_FS. */
  if (fs_threads) {
    fflush(stdout);
    CHECK(pipe(fs_done) == 0, "pipe");
    int flags = CLONE_VM | CLONE_THREAD | CLONE_SIGHAND | CLONE_FILES | CLONE_SYSVSEM;
    CHECK(clone(fs_thread, fs_stack + sizeof(fs_stack), flags, NULL) > 0, "clone: %s",
          strerror(errno));
    char byte;
    CHECK(read(fs_done[0], &byte, 1) == 1, "wait for the fs thread");
    print_link("main: after the fs thread's chdir, self/cwd", "/proc/self/cwd");
  }

  /* A fork's child: its chdir is its own. */
  fflush(stdout);
  pid_t child = fork();
  if (child == 0) {
    failures = 0;
    CHECK(chdir("sub") == 0, "child chdir: %s", strerror(errno));
    print_link("child: after its chdir(\"sub\"), cwd", "/proc/self/cwd");
    CHECK(read_link("/proc/self/exe", target, sizeof(target)) >= 0 && strcmp(target, exe) == 0,
          "child exe");
    printf("child: exe is the parent's=%d\n", strcmp(target, exe) == 0);
    char own[PATH_MAX];
    CHECK(getcwd(own, sizeof(own)) != NULL, "child getcwd: %s", strerror(errno));
    int spellings;
    int agree = spellings_agree("cwd", own, &spellings);
    printf("child: cwd spellings=%d same=%d\n", spellings, agree);
    CHECK(agree == spellings, "child cwd spellings");
    fflush(stdout);
    _exit(failures);
  }
  wait_child(child, "child");
  print_link("parent: after the child's chdir, cwd", "/proc/self/cwd");

  /* A process whose leader has exited. */
  if (leader_exit) {
    fflush(stdout);
    child = fork();
    if (child == 0) {
      failures = 0;
      static pthread_t leader;
      leader = pthread_self();
      pthread_t orphan;
      CHECK(pthread_create(&orphan, NULL, orphaned_thread, &leader) == 0, "orphan thread");
      pthread_exit(NULL);
    }
    wait_child(child, "leader-exit child");
  }

  /* A copy of the program, exec'd from the scratch directory. */
  if (exec_copy) {
    char copy[PATH_MAX], second[PATH_MAX];
    CHECK(mkdir("a", 0755) == 0 && mkdir("b", 0755) == 0, "mkdir a b: %s", strerror(errno));
    snprintf(copy, sizeof(copy), "%s/a/copy", scratch);
    snprintf(second, sizeof(second), "%s/b/copy", scratch);
    int image = open(exe, O_RDONLY | O_CLOEXEC);
    CHECK(image >= 0, "open the image: %s", strerror(errno));
    int written = open(copy, O_WRONLY | O_CREAT | O_EXCL | O_CLOEXEC, 0755);
    CHECK(written >= 0, "create the copy: %s", strerror(errno));
    char block[65536];
    ssize_t got;
    while ((got = read(image, block, sizeof(block))) > 0) {
      CHECK(write(written, block, (size_t)got) == got, "write the copy: %s", strerror(errno));
    }
    CHECK(got == 0, "read the image: %s", strerror(errno));
    close(image);
    CHECK(close(written) == 0, "close the copy: %s", strerror(errno));
    CHECK(link(copy, second) == 0, "link the copy: %s", strerror(errno));
    fflush(stdout);
    child = fork();
    if (child == 0) {
      failures = 0;
      if (exe_changes) {
        execl(copy, copy, "--after-exec", scratch, (char *)NULL);
      } else {
        execl(copy, copy, "--after-exec", scratch, "--no-exe-changes", (char *)NULL);
      }
      fprintf(stderr, "exec the copy: %s\n", strerror(errno));
      _exit(127);
    }
    wait_child(child, "copy");
  }

  char sub[PATH_MAX];
  snprintf(sub, sizeof(sub), "%s/sub", scratch);
  CHECK(rmdir(sub) == 0, "rmdir sub: %s", strerror(errno));
  CHECK(chdir("/") == 0 && rmdir(scratch) == 0, "remove the scratch directory: %s",
        strerror(errno));
  printf("failures=%d\n", failures);
  return failures != 0;
}
