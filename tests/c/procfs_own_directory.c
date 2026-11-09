/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/* Every spelling of the caller's own /proc directories (package K slice K10,
 * https://github.com/rrnewton/hermit/issues/5039): /proc/self, /proc/<pid>,
 * /proc/<pid>/, /proc/thread-self, /proc/<pid>/task/<tid>, /proc/self/task,
 * and the `..` spellings that reach them, which Linux resolves through
 * procfs's own directories (/proc/self/fd/.. is /proc/self).
 *
 * For each spelling it prints what an O_PATH|O_DIRECTORY open, a plain
 * O_DIRECTORY open, stat and lstat answer, then, through the O_PATH
 * descriptor, fstat, fstatat of the directory itself, of its `stat` file and
 * of its `fd` directory, an openat and read of its `stat` file, and
 * readlinkat of a descriptor link below it, and through the plain descriptor
 * what getdents64 lists: whether `.` and `..` come first, whether every entry
 * stats (fstatat, AT_SYMLINK_NOFOLLOW) as the type it was listed with, and
 * whether a few names every such directory has are there. Then readlink of
 * /proc/self, /proc/thread-self and /proc/self/fd/../fd/<fd>, the opens of
 * /proc/self and /proc/thread-self that do not follow the link (O_NOFOLLOW,
 * O_CREAT|O_EXCL), lstat and readlink of the links spelled with a final `/`
 * or `/.` (which follows the link) or with empty and `.` components before
 * them, and an open of /proc/thread-self/status. Then the links reached
 * through `..` (/proc/self/../self): the opens that do not follow them, lstat,
 * stat and readlink, which answer as for the link spelled plainly, and
 * readlink of spellings whose last `..` reaches a directory; and a file
 * of the caller's own directories spelled with a final `/` or `/.`, which
 * asks for a directory (ENOTDIR), through `thread-self` and the leader's
 * number. A second thread repeats the spellings of its own thread, whose
 * number is then not the process's, and the files of its own directory with
 * a final `/` or `/.`. Last, the opens that create of the caller's own
 * thread's directory and of one that does not exist, by a spelling with and
 * without a final `/` or `/.`: EINVAL for O_CREAT|O_DIRECTORY, then EISDIR
 * for O_CREAT where a name ends the spelling with a `/`, before any lookup of
 * that name. And readlinkat of names relative to descriptors of the
 * caller's own directories (`../self`, `.`, `..`), and readlink of /proc and
 * of a descriptor directory, which are no links (EINVAL). Then calls whose
 * own arguments Linux refuses before any walk (EINVAL), by spellings whose
 * walk would fail; O_TMPFILE opens of the caller's own process and thread
 * directories, which are immutable (EPERM); readlink of a directory
 * reached through a descriptor link to it or through /dev/fd (EINVAL); and
 * readlink, an open and stat of spellings through a descriptor link to the
 * caller's own process or thread directory that follow more links in all
 * than the limit (ELOOP).
 *
 * Lines name the caller's numbers by template: <pid> and <tid>, and <fd> for
 * the descriptor of the file it opens in DIRECTORY, whose path is <dir>. An
 * owner is `own` when it is the caller's effective uid (or gid). Nothing
 * printed depends on the host or the backend, except the lines that start
 * with `list `, each a directory's entries in getdents order, which the
 * caller compares by rule (a listing may leave out names; it keeps their
 * types and order).
 *
 * Usage: procfs_own_directory DIRECTORY [MODE]
 *
 * MODE link: instead, open /proc/self and /proc/thread-self O_PATH|O_NOFOLLOW,
 * which opens each link itself, and print its type, mode and target through
 * the descriptor. dotdot-link: the same for /proc/self/../self and
 * /proc/self/../thread-self, the links reached through `..`.
 * thread-status-dot, task-status-dot: instead, a second thread opens and
 * reads its own `status` by a spelling with a `.` component,
 * /proc/thread-self/./status or /proc/self/./task/<tid>/status, and prints
 * its `Pid`. held-thread: instead, descriptors of a second thread's
 * directory held while it exits: fstat still describes the directory, owned
 * by global root once its task is gone (`pid_getattr`), and getdents64 fails
 * with ENOENT, after a read while it lived and after a rewind too
 * (`proc_pident_readdir`). read-listed: instead, list /proc/self and
 * /proc/thread-self and open each entry they list, reading a regular file
 * once and listing a directory once, and print `read <dir>/<name>: ok` or
 * the errno of the open or the read; a listing must not name what cannot be
 * read (https://github.com/rrnewton/hermit/issues/5063, dh-62azc).
 */
#define _GNU_SOURCE
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <sched.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <unistd.h>

#define CHECK(condition)                                                    \
  do {                                                                      \
    if (!(condition)) {                                                     \
      fprintf(stderr, "check failed at line %d: errno %d\n", __LINE__, errno); \
      exit(2);                                                              \
    }                                                                       \
  } while (0)

static int pid, probe_fd;
static char directory[4096];

static const char *errno_name(int number) {
  switch (number) {
    case ENOENT: return "ENOENT";
    case ENOTDIR: return "ENOTDIR";
    case EACCES: return "EACCES";
    case ELOOP: return "ELOOP";
    case EISDIR: return "EISDIR";
    case EINVAL: return "EINVAL";
    case EPERM: return "EPERM";
    case EBADF: return "EBADF";
    case EEXIST: return "EEXIST";
    default: return "other";
  }
}

/* `template` with <pid>, <tid> and <fd> replaced by the numbers. */
static void expand(const char *template, int tid, char *out, size_t size) {
  size_t used = 0;
  out[0] = 0;
  while (*template != 0) {
    int number = -1;
    size_t skip = 0;
    if (strncmp(template, "<pid>", 5) == 0) {
      number = pid;
      skip = 5;
    } else if (strncmp(template, "<tid>", 5) == 0) {
      number = tid;
      skip = 5;
    } else if (strncmp(template, "<fd>", 4) == 0) {
      number = probe_fd;
      skip = 4;
    }
    int written = number >= 0 ? snprintf(out + used, size - used, "%d", number)
                              : snprintf(out + used, size - used, "%c", *template);
    CHECK(written > 0 && (size_t)written < size - used);
    used += (size_t)written;
    template += number >= 0 ? skip : 1;
  }
}

/* `text` with the directory argument and the caller's numbers replaced by
 * their templates: the tid, where it is not the pid, before the pid. */
static void templated(const char *text, int tid, char *out, size_t size) {
  char numbers[2][16];
  const char *names[2] = {"<tid>", "<pid>"};
  snprintf(numbers[0], sizeof numbers[0], "%d", tid);
  snprintf(numbers[1], sizeof numbers[1], "%d", pid);
  size_t used = 0;
  size_t length = strlen(directory);
  out[0] = 0;
  while (*text != 0 && used + 8 < size) {
    if (strncmp(text, directory, length) == 0) {
      used += (size_t)snprintf(out + used, size - used, "<dir>");
      text += length;
      continue;
    }
    int replaced = 0;
    for (int i = tid == pid ? 1 : 0; i < 2 && !replaced; i++) {
      size_t n = strlen(numbers[i]);
      if (strncmp(text, numbers[i], n) == 0 && (text[n] < '0' || text[n] > '9')) {
        used += (size_t)snprintf(out + used, size - used, "%s", names[i]);
        text += n;
        replaced = 1;
      }
    }
    if (!replaced) {
      out[used++] = *text++;
      out[used] = 0;
    }
  }
}

static const char *type_name(mode_t mode) {
  switch (mode & S_IFMT) {
    case S_IFDIR: return "dir";
    case S_IFLNK: return "lnk";
    case S_IFREG: return "reg";
    default: return "other";
  }
}

static int dtype_mode(unsigned char type) {
  switch (type) {
    case DT_DIR: return S_IFDIR;
    case DT_LNK: return S_IFLNK;
    case DT_REG: return S_IFREG;
    default: return -1;
  }
}

static const char *dtype_name(unsigned char type) {
  switch (type) {
    case DT_DIR: return "d";
    case DT_LNK: return "l";
    case DT_REG: return "r";
    default: return "?";
  }
}

static void print_stat(const char *label, const char *what, int rc, const struct stat *st) {
  if (rc < 0) {
    printf("%s %s: %s\n", label, what, errno_name(errno));
    return;
  }
  printf("%s %s: %s %04o nlink %lu owner %s:%s size %lld\n", label, what, type_name(st->st_mode),
         (unsigned)(st->st_mode & 07777), (unsigned long)st->st_nlink,
         st->st_uid == geteuid() ? "own" : "other", st->st_gid == getegid() ? "own" : "other",
         (long long)st->st_size);
}

struct linux_dirent64 {
  unsigned long long d_ino;
  long long d_off;
  unsigned short d_reclen;
  unsigned char d_type;
  char d_name[];
};

/* What getdents64 lists through `plain`, checked through `dirfd`. */
static void list(const char *label, int plain, int dirfd, int tid, int process) {
  static char buffer[65536];
  char line[16384] = "";
  size_t used = 0;
  int count = 0, dots_first = 1, types_agree = 1;
  int has_fd = 0, has_task = 0, has_stat = 0, has_comm = 0, has_cmdline = 0, has_tid = 0;
  char tid_name[16];
  snprintf(tid_name, sizeof tid_name, "%d", tid);
  for (;;) {
    long n = syscall(SYS_getdents64, plain, buffer, sizeof buffer);
    if (n < 0) {
      printf("%s getdents: %s\n", label, errno_name(errno));
      return;
    }
    if (n == 0) break;
    for (long off = 0; off < n;) {
      struct linux_dirent64 *entry = (struct linux_dirent64 *)(buffer + off);
      const char *name = entry->d_name;
      if ((count == 0 && strcmp(name, ".") != 0) || (count == 1 && strcmp(name, "..") != 0)) {
        dots_first = 0;
      }
      if (strcmp(name, ".") != 0 && strcmp(name, "..") != 0) {
        struct stat st;
        if (fstatat(dirfd, name, &st, AT_SYMLINK_NOFOLLOW) < 0 ||
            (int)(st.st_mode & S_IFMT) != dtype_mode(entry->d_type)) {
          types_agree = 0;
        }
      }
      has_fd |= strcmp(name, "fd") == 0 && entry->d_type == DT_DIR;
      has_task |= strcmp(name, "task") == 0 && entry->d_type == DT_DIR;
      has_stat |= strcmp(name, "stat") == 0 && entry->d_type == DT_REG;
      has_comm |= strcmp(name, "comm") == 0 && entry->d_type == DT_REG;
      has_cmdline |= strcmp(name, "cmdline") == 0 && entry->d_type == DT_REG;
      has_tid |= strcmp(name, tid_name) == 0 && entry->d_type == DT_DIR;
      char shown[300];
      templated(name, tid, shown, sizeof shown);
      int written = snprintf(line + used, sizeof line - used, " %s/%s", shown, dtype_name(entry->d_type));
      CHECK(written > 0 && (size_t)written < sizeof line - used);
      used += (size_t)written;
      count++;
      off += entry->d_reclen;
    }
  }
  printf("list %s:%s\n", label, line);
  printf("%s getdents: dots first %s; every entry stats as its type %s", label, dots_first ? "yes" : "no",
         types_agree ? "yes" : "no");
  if (process >= 0) {
    printf("; fd %s; task %s; stat %s; comm %s; cmdline %s\n", has_fd ? "yes" : "no", has_task ? "yes" : "no",
           has_stat ? "yes" : "no", has_comm ? "yes" : "no", has_cmdline ? "yes" : "no");
  } else {
    printf("; <tid> %s\n", has_tid ? "yes" : "no");
  }
}

/* The checks of one directory spelling, by the thread `tid`. `process` is 1
 * for a process's directory, 0 for a thread's, -1 for a `task` directory. */
static void show_directory(const char *template, int tid, int process) {
  char path[512], label[512];
  expand(template, tid, path, sizeof path);
  snprintf(label, sizeof label, "%s", template);
  int opath = open(path, O_PATH | O_DIRECTORY | O_CLOEXEC);
  printf("%s open O_PATH|O_DIRECTORY: %s\n", label, opath < 0 ? errno_name(errno) : "ok");
  int plain = open(path, O_RDONLY | O_DIRECTORY | O_CLOEXEC);
  printf("%s open O_RDONLY|O_DIRECTORY: %s\n", label, plain < 0 ? errno_name(errno) : "ok");
  struct stat st;
  int rc = stat(path, &st);
  print_stat(label, "stat", rc, &st);
  rc = lstat(path, &st);
  print_stat(label, "lstat", rc, &st);
  if (opath >= 0) {
    rc = fstat(opath, &st);
    print_stat(label, "fstat", rc, &st);
    rc = fstatat(opath, "", &st, AT_EMPTY_PATH);
    print_stat(label, "fstatat \"\" AT_EMPTY_PATH", rc, &st);
    if (process >= 0) {
      rc = fstatat(opath, "stat", &st, 0);
      print_stat(label, "fstatat stat", rc, &st);
      rc = fstatat(opath, "fd", &st, AT_SYMLINK_NOFOLLOW);
      print_stat(label, "fstatat fd", rc, &st);
      int stat_fd = openat(opath, "stat", O_RDONLY | O_CLOEXEC);
      if (stat_fd < 0) {
        printf("%s openat stat: %s\n", label, errno_name(errno));
      } else {
        char buffer[1024];
        ssize_t n = read(stat_fd, buffer, sizeof buffer - 1);
        buffer[n > 0 ? n : 0] = 0;
        char *space = strchr(buffer, ' ');
        if (space != NULL) *space = 0;
        char shown[64];
        templated(buffer, tid, shown, sizeof shown);
        printf("%s openat stat: first field %s\n", label, n > 0 ? shown : "none");
        close(stat_fd);
      }
      char link_name[32], target[512], shown[600];
      snprintf(link_name, sizeof link_name, "fd/%d", probe_fd);
      ssize_t length = readlinkat(opath, link_name, target, sizeof target - 1);
      if (length < 0) {
        printf("%s readlinkat fd/<fd>: %s\n", label, errno_name(errno));
      } else {
        target[length] = 0;
        templated(target, tid, shown, sizeof shown);
        printf("%s readlinkat fd/<fd>: %s\n", label, shown);
      }
    } else {
      char name[16];
      snprintf(name, sizeof name, "%d", tid);
      rc = fstatat(opath, name, &st, AT_SYMLINK_NOFOLLOW);
      print_stat(label, "fstatat <tid>", rc, &st);
    }
  }
  if (plain >= 0 && opath >= 0) list(label, plain, opath, tid, process);
  if (plain >= 0) close(plain);
  if (opath >= 0) close(opath);
}

static void show_link(const char *template, int tid) {
  char path[256], target[512], shown[600];
  expand(template, tid, path, sizeof path);
  ssize_t length = readlink(path, target, sizeof target - 1);
  if (length < 0) {
    printf("%s readlink: %s\n", template, errno_name(errno));
    return;
  }
  target[length] = 0;
  templated(target, tid, shown, sizeof shown);
  printf("%s readlink: %s\n", template, shown);
}

/* The opens of the link `path` that do not follow it: O_NOFOLLOW without and
 * with O_DIRECTORY or O_CREAT, O_CREAT|O_EXCL, and O_PATH|O_NOFOLLOW with
 * O_DIRECTORY (O_PATH|O_NOFOLLOW alone opens the link: link mode). */
static void show_link_opens(const char *path) {
  static const struct {
    const char *name;
    int flags;
  } opens[] = {
      {"O_RDONLY|O_NOFOLLOW", O_RDONLY | O_NOFOLLOW},
      {"O_WRONLY|O_NOFOLLOW", O_WRONLY | O_NOFOLLOW},
      {"O_RDONLY|O_NOFOLLOW|O_DIRECTORY", O_RDONLY | O_NOFOLLOW | O_DIRECTORY},
      {"O_RDONLY|O_NOFOLLOW|O_CREAT", O_RDONLY | O_NOFOLLOW | O_CREAT},
      {"O_RDONLY|O_CREAT|O_EXCL", O_RDONLY | O_CREAT | O_EXCL},
      {"O_RDONLY|O_NOFOLLOW|O_CREAT|O_DIRECTORY", O_RDONLY | O_NOFOLLOW | O_CREAT | O_DIRECTORY},
      {"O_PATH|O_NOFOLLOW|O_DIRECTORY", O_PATH | O_NOFOLLOW | O_DIRECTORY},
  };
  for (size_t i = 0; i < sizeof opens / sizeof opens[0]; i++) {
    int fd = open(path, opens[i].flags | O_CLOEXEC, 0600);
    if (fd < 0) {
      printf("%s open %s: %s\n", path, opens[i].name, errno_name(errno));
    } else {
      struct stat st;
      CHECK(fstat(fd, &st) == 0);
      printf("%s open %s: ok %s\n", path, opens[i].name, type_name(st.st_mode));
      close(fd);
    }
  }
}

/* lstat and readlink of each spelling of the link `name` (self or
 * thread-self): a final `/` or `/.` follows it; empty and `.` components
 * name nothing. */
static void show_link_spellings(const char *name, int tid) {
  static const char *const forms[] = {"/proc/%s/", "/proc/%s/.", "//proc/%s", "/./proc/%s", "/proc//%s"};
  for (size_t i = 0; i < sizeof forms / sizeof forms[0]; i++) {
    char path[64];
    snprintf(path, sizeof path, forms[i], name);
    struct stat st;
    int rc = lstat(path, &st);
    if (rc < 0) {
      printf("%s lstat: %s\n", path, errno_name(errno));
    } else {
      printf("%s lstat: %s %04o\n", path, type_name(st.st_mode), (unsigned)(st.st_mode & 07777));
    }
    show_link(path, tid);
  }
}

/* A selector link reached through `..` (`path`, /proc/self/../self): the
 * opens that do not follow it, then lstat, which describes the link, stat,
 * which describes its directory, and readlink, which reads its target, as
 * for the link spelled plainly (the walk resolves `..` before the last
 * component, which the call follows or not). */
static void show_dotdot_link(const char *path, int tid) {
  show_link_opens(path);
  struct stat st;
  if (lstat(path, &st) < 0) {
    printf("%s lstat: %s\n", path, errno_name(errno));
  } else {
    printf("%s lstat: %s %04o nlink %lu\n", path, type_name(st.st_mode), (unsigned)(st.st_mode & 07777),
           (unsigned long)st.st_nlink);
  }
  int rc = stat(path, &st);
  print_stat(path, "stat", rc, &st);
  show_link(path, tid);
}

/* The spellings of the calling thread's own directory and links, the
 * `thread-self` link reached through `..` included. */
static void show_thread(int tid) {
  show_directory("/proc/thread-self", tid, 0);
  show_directory("/proc/<pid>/task/<tid>", tid, 0);
  show_directory("/proc/self/task/<tid>/", tid, 0);
  show_directory("/proc/thread-self/..", tid, -1);
  show_link("/proc/thread-self", tid);
  show_link_opens("/proc/thread-self");
  show_link_spellings("thread-self", tid);
  show_dotdot_link("/proc/self/../thread-self", tid);
  show_dotdot_link("/proc/thread-self/../../../thread-self", tid);
}

/* A file of the caller's own directories spelled with a final `/` or `/.`
 * (`template`), which asks for a directory: Linux's walk fails with ENOTDIR
 * at the file, for open, stat, lstat and readlink alike. */
static void show_file_as_directory(const char *template, int tid) {
  char path[256], target[256];
  expand(template, tid, path, sizeof path);
  int fd = open(path, O_RDONLY | O_CLOEXEC);
  printf("%s open: %s\n", template, fd < 0 ? errno_name(errno) : "ok");
  if (fd >= 0) close(fd);
  struct stat st;
  int rc = stat(path, &st);
  printf("%s stat: %s\n", template, rc < 0 ? errno_name(errno) : type_name(st.st_mode));
  rc = lstat(path, &st);
  printf("%s lstat: %s\n", template, rc < 0 ? errno_name(errno) : type_name(st.st_mode));
  ssize_t length = readlink(path, target, sizeof target - 1);
  printf("%s readlink: %s\n", template, length < 0 ? errno_name(errno) : "ok");
}

/* The spellings of a file of the calling thread's own directory with a
 * final `/` or `/.`. */
static void show_files_as_directories(int tid) {
  static const char *const templates[] = {
      "/proc/thread-self/status/",     "/proc/thread-self/status/.",
      "/proc/thread-self/maps/.",      "/proc/self/task/<pid>/status/",
      "/proc/self/task/<pid>/status/.", "/proc/<pid>/task/<pid>/maps/.",
      "/proc/self/task/<tid>/status/.",
  };
  for (size_t i = 0; i < sizeof templates / sizeof templates[0]; i++) {
    show_file_as_directory(templates[i], tid);
  }
}

/* The opens that create (O_CREAT), and a plain one, of a thread's directory
 * by the spelling `template`: Linux refuses O_CREAT|O_DIRECTORY before it
 * looks a name up (EINVAL), and O_CREAT where the last component is a name
 * followed by `/` once the walk reaches that name, before it looks the name
 * up, whether or not that thread exists (EISDIR); O_PATH drops O_CREAT. */
static void show_creation_opens(const char *template, int tid) {
  static const struct {
    const char *name;
    int flags;
  } opens[] = {
      {"O_RDONLY|O_CREAT", O_RDONLY | O_CREAT},
      {"O_RDONLY|O_CREAT|O_EXCL", O_RDONLY | O_CREAT | O_EXCL},
      {"O_RDONLY|O_CREAT|O_DIRECTORY", O_RDONLY | O_CREAT | O_DIRECTORY},
      {"O_PATH|O_CREAT", O_PATH | O_CREAT},
      {"O_RDONLY", O_RDONLY},
  };
  char path[256];
  expand(template, tid, path, sizeof path);
  for (size_t i = 0; i < sizeof opens / sizeof opens[0]; i++) {
    int fd = open(path, opens[i].flags | O_CLOEXEC, 0600);
    if (fd < 0) {
      printf("%s open %s: %s\n", template, opens[i].name, errno_name(errno));
    } else {
      struct stat st;
      CHECK(fstat(fd, &st) == 0);
      printf("%s open %s: ok %s\n", template, opens[i].name, type_name(st.st_mode));
      close(fd);
    }
  }
}

/* readlinkat of names relative to descriptors of the caller's own
 * directories: the links reached through `..` read their targets, and a name
 * that reaches a directory (`.`, `..`) fails with EINVAL, as readlink of any
 * directory does; then readlink of /proc and of a descriptor directory by
 * absolute spellings. */
static void show_relative_links(int tid) {
  static const struct {
    const char *label;
    const char *directory;
    const char *names[4];
  } directories[] = {
      {"[self]", "/proc/self", {"../self", "../thread-self", ".", ".."}},
      {"[fd]", "/proc/self/fd", {"../../self", ".", "..", NULL}},
      {"[task]", "/proc/self/task", {"../../self", ".", "..", NULL}},
      {"[thread-self]", "/proc/thread-self", {"../../../thread-self", ".", "..", NULL}},
  };
  for (size_t i = 0; i < sizeof directories / sizeof directories[0]; i++) {
    int dirfd = open(directories[i].directory, O_RDONLY | O_DIRECTORY | O_CLOEXEC);
    CHECK(dirfd >= 0);
    for (size_t j = 0; j < 4 && directories[i].names[j] != NULL; j++) {
      char target[256], shown[300];
      ssize_t length = readlinkat(dirfd, directories[i].names[j], target, sizeof target - 1);
      if (length < 0) {
        printf("%s%s readlinkat: %s\n", directories[i].label, directories[i].names[j], errno_name(errno));
      } else {
        target[length] = 0;
        templated(target, tid, shown, sizeof shown);
        printf("%s%s readlinkat: %s\n", directories[i].label, directories[i].names[j], shown);
      }
    }
    close(dirfd);
  }
  static const char *const absolute[] = {"/proc", "/proc/", "/proc/self/fd", "/proc/self/.."};
  for (size_t i = 0; i < sizeof absolute / sizeof absolute[0]; i++) {
    show_link(absolute[i], tid);
  }
}

/* What `rc`, a raw system call's result, says: "ok" or the errno's name. */
static const char *answer(long rc) { return rc < 0 ? errno_name(errno) : "ok"; }

/* Calls whose own arguments Linux refuses (EINVAL) before it walks any path:
 * open flags (build_open_flags), an access mode or flag (do_faccessat), a stat
 * flag or a reserved statx mask bit (vfs_statx, do_statx); by spellings whose
 * walk fails (a `..` through a thread that does not exist, or more links than
 * the limit), with a valid call after each as the control. */
static void show_arguments_before_the_walk(void) {
  static const char *const paths[] = {
      "/proc/self/task/999999999/../../fd/",
      "/proc/self/task/999999999/../../cmdline",
      "/proc/self/task/999999999/../../exe",
  };
  struct stat st;
  struct statx sx;
  for (size_t i = 0; i < sizeof paths / sizeof paths[0]; i++) {
    const char *path = paths[i];
    printf("%s open O_RDONLY|O_CREAT|O_DIRECTORY: %s\n", path,
           answer(syscall(SYS_openat, AT_FDCWD, path, O_RDONLY | O_CREAT | O_DIRECTORY, 0600)));
    printf("%s open O_RDONLY|O_TMPFILE: %s\n", path,
           answer(syscall(SYS_openat, AT_FDCWD, path, O_RDONLY | O_TMPFILE, 0600)));
    printf("%s open O_RDONLY: %s\n", path, answer(syscall(SYS_openat, AT_FDCWD, path, O_RDONLY)));
    printf("%s faccessat mode 0100: %s\n", path, answer(syscall(SYS_faccessat, AT_FDCWD, path, 0100)));
    printf("%s faccessat2 unknown flag: %s\n", path,
           answer(syscall(SYS_faccessat2, AT_FDCWD, path, R_OK, 0x40000000)));
    printf("%s faccessat F_OK: %s\n", path, answer(syscall(SYS_faccessat, AT_FDCWD, path, F_OK)));
    printf("%s newfstatat unknown flag: %s\n", path,
           answer(syscall(SYS_newfstatat, AT_FDCWD, path, &st, 0x40000000)));
    printf("%s statx unknown flag: %s\n", path,
           answer(syscall(SYS_statx, AT_FDCWD, path, 0x40000000, STATX_BASIC_STATS, &sx)));
    printf("%s statx reserved mask: %s\n", path, answer(syscall(SYS_statx, AT_FDCWD, path, 0, 0x80000000u, &sx)));
    printf("%s newfstatat: %s\n", path, answer(syscall(SYS_newfstatat, AT_FDCWD, path, &st, 0)));
  }
  char far[512] = "/proc/";
  for (int i = 0; i < 40; i++) strcat(far, "self/../");
  strcat(far, "self/cmdline");
  printf("40 self/.. hops then self/cmdline open O_RDONLY|O_CREAT|O_DIRECTORY: %s\n",
         answer(syscall(SYS_openat, AT_FDCWD, far, O_RDONLY | O_CREAT | O_DIRECTORY, 0600)));
  printf("40 self/.. hops then self/cmdline open O_RDONLY: %s\n",
         answer(syscall(SYS_openat, AT_FDCWD, far, O_RDONLY)));
}

/* An O_TMPFILE open of the caller's own process and thread directories,
 * which are immutable: EPERM (vfs_tmpfile checks write permission first). */
static void show_tmpfile_opens(int tid) {
  static const char *const directories[] = {"/proc/self", "/proc/thread-self", "/proc/<pid>/task/<tid>"};
  for (size_t i = 0; i < sizeof directories / sizeof directories[0]; i++) {
    char path[128];
    expand(directories[i], tid, path, sizeof path);
    printf("%s open O_TMPFILE|O_RDWR: %s\n", directories[i],
           answer(syscall(SYS_openat, AT_FDCWD, path, O_TMPFILE | O_RDWR, 0600)));
  }
}

/* readlink of a directory reached through a descriptor link to it, or
 * through /dev/fd: no link (EINVAL). */
static void show_links_to_directories(void) {
  int procfd = open("/proc", O_RDONLY | O_DIRECTORY | O_CLOEXEC);
  int fdfd = open("/proc/self/fd", O_RDONLY | O_DIRECTORY | O_CLOEXEC);
  CHECK(procfd >= 0 && fdfd >= 0);
  static const char *const forms[] = {"/proc/self/fd/%d/", "/proc/self/fd/%d/.", "/proc/self/fd/%d/.."};
  static const char *const labels[] = {"/proc/self/fd/<pfd>/", "/proc/self/fd/<pfd>/.", "/proc/self/fd/<pfd>/.."};
  char target[256], path[64];
  for (size_t i = 0; i < sizeof forms / sizeof forms[0]; i++) {
    snprintf(path, sizeof path, forms[i], procfd);
    printf("%s readlink: %s\n", labels[i], answer(readlink(path, target, sizeof target)));
  }
  printf("/dev/fd/ readlink: %s\n", answer(readlink("/dev/fd/", target, sizeof target)));
  snprintf(path, sizeof path, "%d/", procfd);
  printf("[fd]<pfd>/ readlinkat: %s\n", answer(readlinkat(fdfd, path, target, sizeof target)));
  close(fdfd);
  close(procfd);
}

/* A descriptor link to the caller's own process or thread directory, `..`
 * back to /proc, n `self/../` hops and a final `self/`: the selector link
 * and the descriptor link count against the lookup's limit (MAXSYMLINKS,
 * pick_link) with the hops' links, n + 3 in all, so 41 and 42 links are
 * ELOOP for readlink, an open and stat alike. Only spellings past the limit,
 * whose answer an early ELOOP of the kernel's own walk cannot change. */
static void show_links_past_the_limit_through_a_descriptor(void) {
  static const struct {
    const char *directory, *label, *up;
  } links[] = {
      {"/proc/self", "<process fd>", "../"},
      {"/proc/thread-self", "<thread fd>", "../../../"},
  };
  for (size_t i = 0; i < sizeof links / sizeof links[0]; i++) {
    int fd = open(links[i].directory, O_RDONLY | O_DIRECTORY | O_CLOEXEC);
    CHECK(fd >= 0);
    for (int hops = 38; hops <= 39; hops++) {
      char path[512], label[128], target[256];
      snprintf(path, sizeof path, "/proc/self/fd/%d/%s", fd, links[i].up);
      for (int j = 0; j < hops; j++) strcat(path, "self/../");
      strcat(path, "self/");
      snprintf(label, sizeof label, "/proc/self/fd/%s/%s then %d self/.. hops then self/", links[i].label,
               links[i].up, hops);
      printf("%s readlink: %s\n", label, answer(readlink(path, target, sizeof target)));
      long opened = syscall(SYS_openat, AT_FDCWD, path, O_RDONLY | O_DIRECTORY | O_CLOEXEC);
      if (opened >= 0) close((int)opened);
      printf("%s open O_RDONLY|O_DIRECTORY: %s\n", label, answer(opened));
      struct stat st;
      printf("%s stat: %s\n", label, answer(stat(path, &st)));
    }
    close(fd);
  }
}

/* link and dotdot-link modes: each link opened itself, O_PATH|O_NOFOLLOW. */
static void show_links_themselves(int tid, int dotdot) {
  static const char *const plain[] = {"/proc/self", "/proc/thread-self"};
  static const char *const through_dotdot[] = {"/proc/self/../self", "/proc/self/../thread-self"};
  const char *const *links = dotdot ? through_dotdot : plain;
  for (size_t i = 0; i < 2; i++) {
    int fd = open(links[i], O_PATH | O_NOFOLLOW | O_CLOEXEC);
    if (fd < 0) {
      printf("%s open O_PATH|O_NOFOLLOW: %s\n", links[i], errno_name(errno));
      continue;
    }
    struct stat st;
    CHECK(fstat(fd, &st) == 0);
    char target[64], shown[128];
    ssize_t length = readlinkat(fd, "", target, sizeof target - 1);
    if (length < 0) {
      snprintf(shown, sizeof shown, "readlinkat %s", errno_name(errno));
    } else {
      target[length] = 0;
      templated(target, tid, shown, sizeof shown);
    }
    printf("%s open O_PATH|O_NOFOLLOW: %s %04o, %s\n", links[i], type_name(st.st_mode),
           (unsigned)(st.st_mode & 07777), shown);
    close(fd);
  }
}

/* An open and read of the calling thread's `status` by the spelling
 * `template`, and the `Pid` it shows. */
static void show_status(const char *template, int tid) {
  char path[256];
  expand(template, tid, path, sizeof path);
  int status = open(path, O_RDONLY | O_CLOEXEC);
  if (status < 0) {
    printf("%s open: %s\n", template, errno_name(errno));
    return;
  }
  char buffer[8192];
  ssize_t n = read(status, buffer, sizeof buffer - 1);
  buffer[n > 0 ? n : 0] = 0;
  char *line = strstr(buffer, "\nPid:\t");
  char shown[64] = "none";
  if (line != NULL) {
    char value[32];
    CHECK(sscanf(line + 6, "%31s", value) == 1);
    templated(value, tid, shown, sizeof shown);
  }
  printf("%s open: Pid %s\n", template, shown);
  close(status);
}

/* The spelling of its own `status` the second thread opens in the
 * thread-status-dot and task-status-dot modes; NULL in the default mode. */
static const char *status_spelling;

static void *second_thread(void *unused) {
  (void)unused;
  int tid = (int)syscall(SYS_gettid);
  printf("== second thread\n");
  if (status_spelling != NULL) {
    show_status(status_spelling, tid);
    return NULL;
  }
  show_thread(tid);
  show_files_as_directories(tid);
  return NULL;
}

static pthread_mutex_t worker_lock = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t worker_changed = PTHREAD_COND_INITIALIZER;
static int worker_tid, worker_released;

/* A thread that names itself, then waits until it is released and exits. */
static void *waiting_thread(void *unused) {
  (void)unused;
  CHECK(pthread_mutex_lock(&worker_lock) == 0);
  worker_tid = (int)syscall(SYS_gettid);
  CHECK(pthread_cond_broadcast(&worker_changed) == 0);
  while (!worker_released) CHECK(pthread_cond_wait(&worker_changed, &worker_lock) == 0);
  CHECK(pthread_mutex_unlock(&worker_lock) == 0);
  return NULL;
}

/* What one getdents64 through `fd` answers: an errno, entries, or the end. */
static const char *getdents_answer(int fd) {
  static char buffer[65536];
  long n = syscall(SYS_getdents64, fd, buffer, sizeof buffer);
  return n < 0 ? errno_name(errno) : n == 0 ? "end" : "entries";
}

/* Descriptors of a second thread's directory, /proc/self/task/<its tid>,
 * held while it exits: one read while it lived, one not read, one O_PATH.
 * The thread is gone once a lookup of its directory fails (its exit wakes
 * pthread_join a little before Linux releases it, `mm_release`). */
static void show_held_after_exit(void) {
  CHECK(pthread_mutex_lock(&worker_lock) == 0);
  pthread_t thread;
  CHECK(pthread_create(&thread, NULL, waiting_thread, NULL) == 0);
  while (worker_tid == 0) CHECK(pthread_cond_wait(&worker_changed, &worker_lock) == 0);
  char path[64];
  CHECK(snprintf(path, sizeof path, "/proc/self/task/%d", worker_tid) < (int)sizeof path);
  int unread = open(path, O_RDONLY | O_DIRECTORY | O_CLOEXEC);
  int read_once = open(path, O_RDONLY | O_DIRECTORY | O_CLOEXEC);
  int opath = open(path, O_PATH | O_DIRECTORY | O_CLOEXEC);
  CHECK(unread >= 0 && read_once >= 0 && opath >= 0);
  struct stat st;
  int rc = fstat(unread, &st);
  print_stat("live thread's directory", "fstat", rc, &st);
  printf("live thread's directory getdents: %s\n", getdents_answer(read_once));
  worker_released = 1;
  CHECK(pthread_cond_broadcast(&worker_changed) == 0);
  CHECK(pthread_mutex_unlock(&worker_lock) == 0);
  CHECK(pthread_join(thread, NULL) == 0);
  for (int i = 0; i < 1000000 && stat(path, &st) == 0; i++) sched_yield();
  rc = stat(path, &st);
  print_stat("exited thread's directory", "stat", rc, &st);
  rc = fstat(unread, &st);
  print_stat("exited thread's directory", "fstat", rc, &st);
  rc = fstat(opath, &st);
  print_stat("exited thread's directory", "fstat O_PATH", rc, &st);
  rc = fstatat(opath, "", &st, AT_EMPTY_PATH);
  print_stat("exited thread's directory", "fstatat \"\" AT_EMPTY_PATH", rc, &st);
  printf("exited thread's directory getdents: %s\n", getdents_answer(unread));
  printf("exited thread's directory getdents after a read: %s\n", getdents_answer(read_once));
  CHECK(lseek(read_once, 0, SEEK_SET) == 0);
  printf("exited thread's directory getdents after a rewind: %s\n", getdents_answer(read_once));
  close(unread);
  close(read_once);
  close(opath);
}

/* Open each entry `path` lists and read it once: a regular file with read,
 * a directory with getdents64. Links are not followed. */
static void read_listed(const char *path) {
  static char listing[65536];
  static char contents[65536];
  int dir = open(path, O_RDONLY | O_DIRECTORY | O_CLOEXEC);
  CHECK(dir >= 0);
  for (;;) {
    long n = syscall(SYS_getdents64, dir, listing, sizeof listing);
    CHECK(n >= 0);
    if (n == 0) break;
    for (long off = 0; off < n;) {
      struct linux_dirent64 *entry = (struct linux_dirent64 *)(listing + off);
      off += entry->d_reclen;
      const char *name = entry->d_name;
      if (strcmp(name, ".") == 0 || strcmp(name, "..") == 0 ||
          (entry->d_type != DT_REG && entry->d_type != DT_DIR)) {
        continue;
      }
      int directory_entry = entry->d_type == DT_DIR;
      int fd = openat(dir, name, (directory_entry ? O_DIRECTORY : 0) | O_RDONLY | O_CLOEXEC);
      if (fd < 0) {
        printf("read %s/%s: open %s\n", path, name, errno_name(errno));
        continue;
      }
      long got = directory_entry ? syscall(SYS_getdents64, fd, contents, sizeof contents)
                                 : (long)read(fd, contents, sizeof contents);
      if (got < 0) {
        printf("read %s/%s: read %s\n", path, name, errno_name(errno));
      } else {
        printf("read %s/%s: ok\n", path, name);
      }
      close(fd);
    }
  }
  close(dir);
}

int main(int argc, char **argv) {
  const char *mode = argc == 3 ? argv[2] : "";
  int link_mode = strcmp(mode, "link") == 0 || strcmp(mode, "dotdot-link") == 0;
  int held_mode = strcmp(mode, "held-thread") == 0;
  int read_mode = strcmp(mode, "read-listed") == 0;
  if (strcmp(mode, "thread-status-dot") == 0) {
    status_spelling = "/proc/thread-self/./status";
  } else if (strcmp(mode, "task-status-dot") == 0) {
    status_spelling = "/proc/self/./task/<tid>/status";
  }
  CHECK(argc == 2 || (argc == 3 && (link_mode || held_mode || read_mode || status_spelling != NULL)));
  CHECK(snprintf(directory, sizeof directory, "%s", argv[1]) < (int)sizeof directory);
  pid = getpid();
  int tid = (int)syscall(SYS_gettid);
  pthread_t thread;
  if (link_mode) {
    show_links_themselves(tid, strcmp(mode, "dotdot-link") == 0);
    printf("done\n");
    return 0;
  }
  if (status_spelling != NULL) {
    CHECK(pthread_create(&thread, NULL, second_thread, NULL) == 0);
    CHECK(pthread_join(thread, NULL) == 0);
    printf("done\n");
    return 0;
  }
  if (read_mode) {
    read_listed("/proc/self");
    read_listed("/proc/thread-self");
    printf("done\n");
    return 0;
  }
  if (held_mode) {
    show_held_after_exit();
    printf("done\n");
    return 0;
  }
  char target[4200];
  CHECK(snprintf(target, sizeof target, "%s/probe", directory) < (int)sizeof target);
  probe_fd = open(target, O_CREAT | O_RDWR | O_CLOEXEC, 0600);
  CHECK(probe_fd >= 0);
  printf("== main thread\n");
  show_directory("/proc/self", tid, 1);
  show_directory("/proc/<pid>", tid, 1);
  show_directory("/proc/<pid>/", tid, 1);
  show_directory("/proc/self/fd/..", tid, 1);
  show_directory("/proc/thread-self/../..", tid, 1);
  show_directory("/proc/self/task", tid, -1);
  show_thread(tid);
  show_link("/proc/self", tid);
  show_link_opens("/proc/self");
  show_link_spellings("self", tid);
  show_link("/proc/self/fd/../fd/<fd>", tid);
  show_status("/proc/thread-self/status", tid);
  show_dotdot_link("/proc/self/../self", tid);
  show_dotdot_link("/proc/self/fd/../../self", tid);
  show_dotdot_link("/proc/self/../self/", tid);
  /* A last `..` reaches a directory, never a link: readlink fails. */
  show_link("/proc/self/fd/..", tid);
  show_link("/proc/thread-self/../..", tid);
  show_files_as_directories(tid);
  /* The caller's own thread, then a thread that does not exist. */
  static const char *const created[] = {
      "/proc/self/task/<tid>/",        "/proc/self/task/999999999/",
      "/proc/self/task/999999999",     "/proc/self/task/999999999/.",
      "/proc/self/task/./999999999//", "/proc/self/task/999999999/../",
  };
  for (size_t i = 0; i < sizeof created / sizeof created[0]; i++) {
    show_creation_opens(created[i], tid);
  }
  show_relative_links(tid);
  show_arguments_before_the_walk();
  show_tmpfile_opens(tid);
  show_links_to_directories();
  show_links_past_the_limit_through_a_descriptor();
  fflush(stdout);
  CHECK(pthread_create(&thread, NULL, second_thread, NULL) == 0);
  CHECK(pthread_join(thread, NULL) == 0);
  printf("done\n");
  return 0;
}
