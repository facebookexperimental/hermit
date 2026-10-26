/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * The /proc names outside Hermit's --proc level do not exist
 * (https://github.com/rrnewton/hermit/issues/4266).
 *
 * usage: omitted_procfs_names [--host] [--minimal] [--kvm-local-procfs]
 *
 *   With no flag, under Hermit at the default level (standard), each name
 *   Hermit omits fails with ENOENT through every call that looks it up, by
 *   every spelling the guest can give (absolute, relative to /proc as the
 *   working directory or as a directory descriptor, a trailing slash, a
 *   symbolic link, a `..`), and no listing of /proc or /proc/sys shows it.
 *   The level's names still open, and the listings show them.
 *   --minimal: Hermit runs with --proc=minimal, which also omits the
 *   standard level's names (/proc/interrupts, /proc/sys/fs, ...).
 *   The calls that create, remove, rename or link a name fail as Linux
 *   fails them for a name procfs does not have; so do the checks in a thread
 *   with its own descriptor table, and after chroot("/").
 *   --kvm-local-procfs: KVM's own /proc lists no directory entries, and
 *   refuses /proc as a working directory, its directories, and renames and
 *   links that reach it (EACCES where Linux gives ENOENT, ENOTDIR or EXDEV),
 *   until it is removed (slice B4). Its executor refuses stat's sync flags
 *   (https://github.com/rrnewton/reverie/issues/1047), and the KVM backend
 *   does not implement inotify or chroot (ENOSYS), or a clone without
 *   CLONE_FILES (EINVAL). Those checks are skipped, and the output says so.
 *   --recorded: Hermit records and replays the run, where the check before
 *   a call and the recorded link of an opened file apply, so listings,
 *   spellings through a link or `..` of a name only root may open (refused
 *   for permission before its link is read), datagram sends, the thread and
 *   chroot checks are skipped.
 *   --chroot-into-proc: instead, chroot into /proc (a root in which /proc is
 *   not where Hermit's tool finds it) and check that omitted names are
 *   absent there, printing nothing.
 *   --kernel-listings: Hermit runs with --no-virtualize-metadata, where
 *   getdents is the kernel's and nothing is dropped from a listing (as for
 *   the CPUs a single virtual CPU leaves out), so listings are not checked.
 *   --host: natively, print which of the omitted names this host has, so
 *   the test shows that Hermit hides names that exist.
 *   --removals: instead, only the removals of an omitted name (unlink of
 *   /proc/kallsyms, rmdir of /proc/sysvipc), which fail as Linux fails them
 *   (ENOENT) where procfs is writable; the other changes are checked without
 *   this flag.
 *
 * Output: a line per name and per group of checks, and a line for each
 * group that was not checked; nothing that depends on the host.
 */

#define _GNU_SOURCE
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <linux/futex.h>
#include <sched.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/inotify.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/uio.h>
#include <sys/un.h>
#include <sys/wait.h>
#include <unistd.h>

#ifndef SYS_faccessat2
#define SYS_faccessat2 439
#endif

static int failures;
/* Under record and replay: only the check before a call applies. */
static int recorded;

#define CHECK(condition, ...)               \
  do {                                      \
    if (!(condition)) {                     \
      fprintf(stderr, "FAIL: " __VA_ARGS__); \
      fprintf(stderr, "\n");                \
      failures++;                           \
    }                                       \
  } while (0)

/* Names in no level. */
static const char* const omitted[] = {
    "/proc/kallsyms",
    "/proc/kcore",
    "/proc/sysvipc",
    "/proc/slabinfo",
    "/proc/bus",
    "/proc/fs",
    "/proc/sys/kernel/core_pattern",
    "/proc/sys/kernel/ngroups_max",
    "/proc/sys/kernel/random/poolsize",
    "/proc/sys/fs/binfmt_misc",
    "/proc/sys/user",
    /* In a process's and a thread's directory. */
    "/proc/self/limits",
    "/proc/self/attr",
    "/proc/self/oom_score_adj",
    "/proc/self/loginuid",
    "/proc/thread-self/wchan",
};

/* Names in the standard level, not in minimal. */
static const char* const standard_only[] = {
    "/proc/interrupts",
    "/proc/sys/fs",
    "/proc/sys/fs/file-nr",
    "/proc/sys/kernel/random/entropy_avail",
    "/proc/self/io",
};

/* Names in both levels. */
static const char* const minimal[] = {
    "/proc/version",
    "/proc/sys/kernel/osrelease",
    "/proc/sys/vm/overcommit_memory",
    "/proc/self/status",
    "/proc/self/task",
};

/* What a call that should have failed with ENOENT did instead. */
static const char* outcome(long result) {
  return result >= 0 ? "succeeded" : strerror(errno);
}

static int is_enoent(long result) {
  return result < 0 && errno == ENOENT;
}

/* `call` fails with ENOENT. */
#define ABSENT(call, ...)                                       \
  do {                                                          \
    errno = 0;                                                  \
    long result_ = (long)(call);                                \
    int enoent_ = is_enoent(result_);                           \
    const char* outcome_ = outcome(result_);                    \
    CHECK(enoent_, __VA_ARGS__);                                \
    if (!enoent_) {                                             \
      fprintf(stderr, "  (%s)\n", outcome_);                    \
    }                                                           \
    if (result_ >= 0 && strncmp(#call, "open", 4) == 0) {       \
      close((int)result_);                                      \
    }                                                           \
  } while (0)

/* The omitted names only root may open, which the kernel refuses for
 * permission before Detcore can see what a link or `..` reached. */
static int refused_for_permission(const char* path) {
  return strcmp(path, "/proc/kcore") == 0 ||
      strcmp(path, "/proc/slabinfo") == 0;
}

/* Every lookup of `path` fails with ENOENT. */
static void check_absent(const char* path, int kvm_local_procfs) {
  struct stat st;
  struct statx stx;
  char buf[64];
  ABSENT(open(path, O_RDONLY), "open %s", path);
  ABSENT(open(path, O_PATH | O_NOFOLLOW), "open O_PATH %s", path);
  ABSENT(stat(path, &st), "stat %s", path);
  ABSENT(lstat(path, &st), "lstat %s", path);
  ABSENT(statx(AT_FDCWD, path, 0, STATX_BASIC_STATS, &stx), "statx %s", path);
  ABSENT(access(path, F_OK), "access %s", path);
  ABSENT((int)readlink(path, buf, sizeof buf), "readlink %s", path);

  char slash[256];
  snprintf(slash, sizeof slash, "%s/", path);
  ABSENT(open(slash, O_RDONLY | O_DIRECTORY), "open %s", slash);

  if (!kvm_local_procfs) {
    int watch = inotify_init1(IN_CLOEXEC);
    CHECK(watch >= 0, "inotify_init1: %s", strerror(errno));
    ABSENT(inotify_add_watch(watch, path, IN_ACCESS), "inotify_add_watch %s",
        path);
    close(watch);
  }

  /* Relative to /proc, as a descriptor and as the working directory. */
  const char* relative = path + strlen("/proc/");
  int proc = open("/proc", O_RDONLY | O_DIRECTORY);
  CHECK(proc >= 0, "open /proc: %s", strerror(errno));
  ABSENT(openat(proc, relative, O_RDONLY), "openat /proc %s", relative);
  ABSENT(fstatat(proc, relative, &st, 0), "fstatat /proc %s", relative);
  close(proc);

  if (!kvm_local_procfs) {
    CHECK(chdir("/proc") == 0, "chdir /proc: %s", strerror(errno));
    ABSENT(open(relative, O_RDONLY), "open %s in /proc", relative);
    CHECK(chdir("/") == 0, "chdir /: %s", strerror(errno));
  }
  /* What the kernel reaches through a link or `..`. Under record and
   * replay a name the kernel refuses for permission is not named after the
   * kernel refuses it, so it stays Linux's refusal there. */
  if (recorded && refused_for_permission(path)) {
    return;
  }
  char dotdot[256];
  snprintf(dotdot, sizeof dotdot, "/proc/self/../%s", relative);
  ABSENT(open(dotdot, O_RDONLY), "open %s", dotdot);
  snprintf(dotdot, sizeof dotdot, "/proc/net/../../%s", relative);
  ABSENT(open(dotdot, O_RDONLY), "open %s", dotdot);
  char link[64];
  snprintf(link, sizeof link, "/tmp/omitted-procfs-link-%d", getpid());
  CHECK(symlink(path, link) == 0, "symlink %s: %s", link, strerror(errno));
  ABSENT(open(link, O_RDONLY), "open %s -> %s", link, path);
  ABSENT(open(link, O_PATH), "open O_PATH %s -> %s", link, path);
  unlink(link);
}

/* The level's names that KVM's own /proc refuses: its directories, and the
 * per-process files it does not serve. */
static int kvm_refuses(const char* path) {
  return strcmp(path, "/proc/sys/fs") == 0 ||
      strcmp(path, "/proc/self/task") == 0 ||
      strcmp(path, "/proc/self/io") == 0;
}

/* `path` opens; returns whether it was checked. */
static int check_present(const char* path, int kvm_local_procfs) {
  if (kvm_local_procfs && kvm_refuses(path)) {
    return 0;
  }
  int fd = open(path, O_RDONLY);
  CHECK(fd >= 0, "open %s: %s", path, strerror(errno));
  if (fd >= 0) {
    close(fd);
  }
  return 1;
}

/* `call` fails with `expected`. */
#define FAILS_WITH(expected, call, ...)                                    \
  do {                                                                     \
    errno = 0;                                                             \
    long result_ = (long)(call);                                           \
    int error_ = errno;                                                    \
    CHECK(result_ < 0 && error_ == (expected), __VA_ARGS__);               \
    if (!(result_ < 0 && error_ == (expected))) {                          \
      fprintf(stderr, "  (%s)\n", result_ >= 0 ? "succeeded" : strerror(error_)); \
    }                                                                      \
  } while (0)

/* The calls that create, remove, rename or link a name in /proc fail as
 * Linux fails them where procfs does not have the name: its lookups end in
 * ENOENT. rename compares the mounts (EXDEV) after it looks up both
 * directories and before either last name; link creates its second name
 * before it compares them. */
static void check_changes(int kvm_local_procfs) {
  char file[64];
  snprintf(file, sizeof file, "/tmp/omitted-procfs-file-%d", getpid());
  int fd = open(file, O_CREAT | O_WRONLY, 0600);
  CHECK(fd >= 0, "create %s: %s", file, strerror(errno));
  close(fd);
  ABSENT(creat("/proc/kallsyms", 0600), "creat /proc/kallsyms");
  ABSENT(open("/proc/kallsyms", O_CREAT | O_WRONLY, 0600), "open O_CREAT /proc/kallsyms");
  ABSENT(mkdir("/proc/kallsyms", 0700), "mkdir /proc/kallsyms");
  ABSENT(mkdir("/proc/sysvipc/x", 0700), "mkdir /proc/sysvipc/x");
  ABSENT(symlink(file, "/proc/kallsyms"), "symlink to /proc/kallsyms");
  if (!kvm_local_procfs) {
    ABSENT(link(file, "/proc/kallsyms"), "link to /proc/kallsyms");
    ABSENT(link("/proc/kallsyms", file), "link from /proc/kallsyms");
    ABSENT(rename("/proc/sysvipc/shm", file), "rename from /proc/sysvipc/shm");
    ABSENT(rename(file, "/proc/sysvipc/shm"), "rename to /proc/sysvipc/shm");
    FAILS_WITH(EXDEV, rename(file, "/proc/kallsyms"), "rename to /proc/kallsyms");
    FAILS_WITH(EXDEV, rename("/proc/kallsyms", file), "rename from /proc/kallsyms");
  }
  unlink(file);
}

/* Removing a name procfs does not have fails as Linux fails it: ENOENT.
 * Linux refuses a removal on a read-only mount before the lookup, so these
 * two run on their own, with --removals. */
static void check_removals(void) {
  ABSENT(unlink("/proc/kallsyms"), "unlink /proc/kallsyms");
  ABSENT(rmdir("/proc/sysvipc"), "rmdir /proc/sysvipc");
}

/* The arguments Linux checks before the lookup, and the order of its
 * lookups, are its own: an omitted name is missing only where the lookup
 * reaches it. */
static void check_linux_order(int kvm_local_procfs) {
  struct stat st;
  struct statx stx;
  char buf[8];
  /* The flags are an int: the high half is not Linux's to see. */
  ABSENT(
      syscall(SYS_faccessat2, AT_FDCWD, "/proc/kallsyms", F_OK, 1ULL << 32),
      "faccessat2 /proc/kallsyms with flags 1 << 32");
  FAILS_WITH(EINVAL, truncate("/proc/kallsyms", -1), "truncate /proc/kallsyms to -1");
  FAILS_WITH(EINVAL, readlink("/proc/kallsyms", buf, 0), "readlink /proc/kallsyms into nothing");
  FAILS_WITH(
      EFAULT,
      syscall(SYS_rename, NULL, "/proc/kallsyms"),
      "rename from NULL to /proc/kallsyms");
  /* Passing through an omitted name: it is missing where it is reached. */
  ABSENT(open("/proc/sysvipc/../version", O_RDONLY), "open /proc/sysvipc/../version");
  ABSENT(open("/proc/kallsyms/..", O_RDONLY), "open /proc/kallsyms/..");
  ABSENT(stat("/proc/sys/kernel/core_pattern/..", &st), "stat /proc/sys/kernel/core_pattern/..");
  if (kvm_local_procfs) {
    return;
  }
  ABSENT(
      fstatat(AT_FDCWD, "/proc/kallsyms", &st, AT_STATX_FORCE_SYNC),
      "fstatat AT_STATX_FORCE_SYNC /proc/kallsyms");
  FAILS_WITH(
      EINVAL,
      statx(
          AT_FDCWD,
          "/proc/kallsyms",
          AT_STATX_FORCE_SYNC | AT_STATX_DONT_SYNC,
          STATX_BASIC_STATS,
          &stx),
      "statx /proc/kallsyms with both sync flags");
  FAILS_WITH(
      ENOTDIR,
      rename("/proc/version/x", "/proc/kallsyms"),
      "rename from below a file to /proc/kallsyms");
  int watch = inotify_init1(IN_CLOEXEC);
  CHECK(watch >= 0, "inotify_init1: %s", strerror(errno));
  ABSENT(
      inotify_add_watch(watch, "/proc/kallsyms", IN_ONLYDIR),
      "inotify_add_watch IN_ONLYDIR /proc/kallsyms");
  FAILS_WITH(
      EINVAL,
      inotify_add_watch(watch, "/proc/kallsyms", 0),
      "inotify_add_watch with no events /proc/kallsyms");
  close(watch);
  CHECK(chdir("/proc/sys") == 0, "chdir /proc/sys: %s", strerror(errno));
  ABSENT(stat("kernel/core_pattern", &st), "stat kernel/core_pattern in /proc/sys");
  CHECK(chdir("/") == 0, "chdir /: %s", strerror(errno));
}

/* A pathname AF_UNIX address names a file the kernel looks up. A send's
 * address is looked up only by a datagram socket. */
static void check_sockets(int sends) {
  struct sockaddr_un address = {.sun_family = AF_UNIX};
  snprintf(address.sun_path, sizeof address.sun_path, "/proc/kallsyms");
  struct sockaddr* name = (struct sockaddr*)&address;
  int stream = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
  CHECK(stream >= 0, "socket: %s", strerror(errno));
  ABSENT(connect(stream, name, sizeof address), "connect to /proc/kallsyms");
  ABSENT(bind(stream, name, sizeof address), "bind to /proc/kallsyms");
  close(stream);
  if (!sends) {
    return;
  }
  int datagram = socket(AF_UNIX, SOCK_DGRAM | SOCK_CLOEXEC, 0);
  CHECK(datagram >= 0, "socket: %s", strerror(errno));
  char byte[1] = {'x'};
  ABSENT(sendto(datagram, byte, 1, 0, name, sizeof address), "sendto /proc/kallsyms");
  struct iovec vector = {.iov_base = byte, .iov_len = 1};
  struct msghdr message = {
      .msg_name = &address,
      .msg_namelen = sizeof address,
      .msg_iov = &vector,
      .msg_iovlen = 1,
  };
  ABSENT(sendmsg(datagram, &message, 0), "sendmsg to /proc/kallsyms");
  close(datagram);
}

static int private_table_thread(void* arg) {
  (void)arg;
  ABSENT(open("/proc/self/../kallsyms", O_RDONLY), "open /proc/self/../kallsyms in a thread with its own descriptor table");
  char link[64];
  snprintf(link, sizeof link, "/tmp/omitted-procfs-thread-link-%d", getpid());
  CHECK(symlink("/proc/kallsyms", link) == 0, "symlink %s: %s", link, strerror(errno));
  ABSENT(open(link, O_RDONLY), "open %s in a thread with its own descriptor table", link);
  unlink(link);
  return 0;
}

/* A thread made without CLONE_FILES has its own descriptor table: the
 * descriptor an open gives it is named in its table, not its process's. */
static void check_private_descriptor_table(void) {
  size_t size = 1 << 18;
  char* stack = mmap(NULL, size, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_STACK, -1, 0);
  CHECK(stack != MAP_FAILED, "mmap: %s", strerror(errno));
  if (stack == MAP_FAILED) {
    return;
  }
  /* Descriptors of the process that the thread's own table also has, so
   * the numbers the thread gets differ from any the process holds. */
  int filler[4];
  for (int i = 0; i < 4; i++) {
    filler[i] = open("/dev/null", O_RDONLY);
  }
  static volatile pid_t word;
  int flags = CLONE_VM | CLONE_FS | CLONE_SIGHAND | CLONE_THREAD | CLONE_SYSVSEM |
      CLONE_PARENT_SETTID | CLONE_CHILD_CLEARTID;
  int tid = clone(private_table_thread, stack + size, flags, NULL, &word, NULL, &word);
  CHECK(tid > 0, "clone without CLONE_FILES: %s", strerror(errno));
  for (pid_t seen; tid > 0 && (seen = word) != 0;) {
    syscall(SYS_futex, &word, FUTEX_WAIT, seen, NULL, NULL, 0);
  }
  for (int i = 0; i < 4; i++) {
    close(filler[i]);
  }
  munmap(stack, size);
}

/* After chroot("/"), /proc is the same procfs. */
static void check_after_chroot(void) {
  CHECK(chroot("/") == 0, "chroot /: %s", strerror(errno));
  struct stat st;
  ABSENT(open("/proc/kallsyms", O_RDONLY), "open /proc/kallsyms after chroot");
  ABSENT(stat("/proc/sys/kernel/core_pattern", &st), "stat /proc/sys/kernel/core_pattern after chroot");
  ABSENT(open("/proc/self/../sysvipc", O_RDONLY | O_DIRECTORY), "open /proc/self/../sysvipc after chroot");
  int fd = open("/proc/version", O_RDONLY);
  CHECK(fd >= 0, "open /proc/version after chroot: %s", strerror(errno));
  if (fd >= 0) {
    close(fd);
  }
}

/* In a root in which /proc is not where the tool finds it (`--chroot-into-
 * proc`, run as the whole guest, whose first chroot this is). */
static int chroot_into_proc(void) {
  CHECK(chroot("/proc") == 0, "chroot /proc: %s", strerror(errno));
  ABSENT(open("/kallsyms", O_RDONLY), "open /kallsyms after chroot(\"/proc\")");
  ABSENT(open("sysvipc", O_RDONLY | O_DIRECTORY), "open sysvipc after chroot(\"/proc\")");
  return failures ? 1 : 0;
}

/* The names in a process's directory by its other spellings: its pid, a
 * thread's directory below `task`, and a descriptor on the directory. */
static void check_process_spellings(int kvm_local_procfs) {
  char path[64];
  snprintf(path, sizeof path, "/proc/%d/limits", (int)getpid());
  ABSENT(open(path, O_RDONLY), "open %s", path);
  ABSENT(access(path, F_OK), "access %s", path);
  snprintf(path, sizeof path, "/proc/self/task/%ld/personality",
      (long)syscall(SYS_gettid));
  ABSENT(open(path, O_RDONLY), "open %s", path);
  snprintf(path, sizeof path, "/proc/%d/status", (int)getpid());
  int fd = open(path, O_RDONLY);
  CHECK(fd >= 0, "open %s: %s", path, strerror(errno));
  if (fd >= 0) {
    close(fd);
  }
  if (kvm_local_procfs) {
    return;
  }
  int dir = open("/proc/self", O_RDONLY | O_DIRECTORY);
  CHECK(dir >= 0, "open /proc/self: %s", strerror(errno));
  ABSENT(openat(dir, "limits", O_RDONLY), "openat /proc/self limits");
  fd = openat(dir, "status", O_RDONLY);
  CHECK(fd >= 0, "openat /proc/self status: %s", strerror(errno));
  if (fd >= 0) {
    close(fd);
  }
  close(dir);
}

/* Whether the listing of `directory` shows `name`; a directory that does
 * not exist shows nothing. */
static int listed(const char* directory, const char* name) {
  DIR* dir = opendir(directory);
  if (dir == NULL && errno == ENOENT) {
    return 0;
  }
  CHECK(dir != NULL, "opendir %s: %s", directory, strerror(errno));
  int found = 0;
  for (struct dirent* entry; dir && (entry = readdir(dir)) != NULL;) {
    found |= strcmp(entry->d_name, name) == 0;
  }
  if (dir) {
    closedir(dir);
  }
  return found;
}

/* The listing of `path`'s directory shows its last name as `shown` says. */
static void check_listed(const char* path, int shown) {
  char directory[256];
  snprintf(directory, sizeof directory, "%s", path);
  char* slash = strrchr(directory, '/');
  const char* name = path + (slash - directory) + 1;
  *slash = '\0';
  CHECK(
      listed(directory, name) == shown,
      "listing of %s %s %s",
      directory,
      shown ? "does not show" : "shows",
      name);
}

int main(int argc, char** argv) {
  int host = 0, minimal_level = 0, kvm_local_procfs = 0, kernel_listings = 0;
  int removals = 0;
  for (int i = 1; i < argc; i++) {
    if (strcmp(argv[i], "--host") == 0) {
      host = 1;
    } else if (strcmp(argv[i], "--minimal") == 0) {
      minimal_level = 1;
    } else if (strcmp(argv[i], "--kvm-local-procfs") == 0) {
      kvm_local_procfs = 1;
    } else if (strcmp(argv[i], "--kernel-listings") == 0) {
      kernel_listings = 1;
    } else if (strcmp(argv[i], "--recorded") == 0) {
      recorded = 1;
    } else if (strcmp(argv[i], "--chroot-into-proc") == 0) {
      return chroot_into_proc();
    } else if (strcmp(argv[i], "--removals") == 0) {
      removals = 1;
    } else {
      fprintf(
          stderr,
          "usage: %s [--host] [--minimal] [--kvm-local-procfs] "
          "[--kernel-listings] [--recorded] [--chroot-into-proc] [--removals]\n",
          argv[0]);
      return 2;
    }
  }
  size_t count = sizeof omitted / sizeof omitted[0];
  if (host) {
    struct stat st;
    for (size_t i = 0; i < count; i++) {
      if (lstat(omitted[i], &st) == 0) {
        printf("exists %s\n", omitted[i]);
      }
    }
    return 0;
  }
  if (removals) {
    check_removals();
    printf("removing a name procfs does not have: as Linux\n");
    return failures ? 1 : 0;
  }
  for (size_t i = 0; i < count; i++) {
    check_absent(omitted[i], kvm_local_procfs);
    if (!kvm_local_procfs && !kernel_listings && !recorded) {
      check_listed(omitted[i], 0);
    }
    printf("absent %s\n", omitted[i]);
  }
  for (size_t i = 0; i < sizeof standard_only / sizeof standard_only[0]; i++) {
    const char* path = standard_only[i];
    int checked = 1;
    if (minimal_level) {
      check_absent(path, kvm_local_procfs);
    } else {
      checked = check_present(path, kvm_local_procfs);
    }
    if (!kvm_local_procfs && !kernel_listings && !recorded) {
      check_listed(path, !minimal_level);
    }
    printf(
        "%s %s\n",
        !checked ? "unchecked (KVM's own /proc refuses it)"
                 : minimal_level ? "absent"
                                 : "present",
        path);
  }
  for (size_t i = 0; i < sizeof minimal / sizeof minimal[0]; i++) {
    int checked = check_present(minimal[i], kvm_local_procfs);
    if (!kvm_local_procfs && !kernel_listings && !recorded) {
      check_listed(minimal[i], 1);
    }
    printf(
        "%s %s\n",
        checked ? "present" : "unchecked (KVM's own /proc refuses it)",
        minimal[i]);
  }
  check_process_spellings(kvm_local_procfs);
  printf("a process's names by its pid, its threads and a descriptor: as the level\n");
  check_changes(kvm_local_procfs);
  printf("creating, renaming and linking: as Linux\n");
  check_linux_order(kvm_local_procfs);
  printf("arguments and lookups in Linux's order: as Linux\n");
  check_sockets(!recorded);
  printf("socket addresses: absent\n");
  if (kernel_listings) {
    printf("unchecked: listings (no virtualized metadata)\n");
  }
  if (recorded) {
    printf(
        "unchecked: listings, spellings through a link or `..` of a name only "
        "root may open, datagram sends, a thread with its own descriptor "
        "table, chroot (record and replay)\n");
    return failures ? 1 : 0;
  }
  if (kvm_local_procfs) {
    printf(
        "unchecked: listings, /proc as the working directory, process "
        "directory descriptors, renames and links (KVM's own /proc); stat sync flags (KVM's executor, "
        "https://github.com/rrnewton/reverie/issues/1047); inotify, a thread "
        "with its own descriptor table, chroot (no KVM support)\n");
    return failures ? 1 : 0;
  }
  check_private_descriptor_table();
  printf("a thread with its own descriptor table: absent\n");
  /* Last: the process stays in the new root. */
  check_after_chroot();
  printf("after chroot(\"/\"): absent\n");
  if (failures) {
    fprintf(stderr, "%d checks failed\n", failures);
    return 1;
  }
  return 0;
}
