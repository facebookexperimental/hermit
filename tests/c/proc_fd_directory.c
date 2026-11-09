/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * proc_fd_directory: the caller's own descriptor directory, /proc/self/fd,
 * and /proc itself, listed and used as Linux allows (package K slices F1
 * and P1, toward https://github.com/rrnewton/hermit/issues/4604). Under
 * Hermit, Detcore serves both on ptrace and KVM, from its own descriptor
 * table and procfs model, each on a descriptor open on procfs's root
 * (ptrace) or on a placeholder directory (KVM).
 *
 * It prints: the directory's fstat and statx; a listing with a large buffer
 * (each name, d_type and d_off), the position at the end, and that "."'s
 * d_ino is fstat's; the same listing one record per call, and the EINVAL of
 * buffers too small for one; lseek of every whence; a listing that sees a
 * descriptor opened and one closed while it is read; the position a dup
 * shares; an O_PATH open (EBADF for getdents64, read and lseek; F_GETFL;
 * lookups below it); openat, readlinkat, fstatat and faccessat of names
 * below the directory's descriptor, and its "."; readlink of the
 * descriptor's own link; fchdir to it; the /proc/<pid>/fd, /dev/fd,
 * /proc/thread-self/fd and /proc/<pid>/task/<tid>/fd spellings from the
 * main thread, and the thread spellings and /proc/self/fd from another
 * thread; descriptors received over SCM_RIGHTS, the directory's own among
 * them; a fork's child's own listing; a listing after exec, which a
 * close-on-exec descriptor has left; and CPython's close-all loop
 * (_posixsubprocess's _close_open_fds_safe), which reads the directory with
 * raw getdents64 into a buffer of one record and closes each listed
 * descriptor above 2 but its own, in a child that opened descriptors up to
 * 70.
 *
 * It also checks: a /proc descriptor seeked to 0x40000000 before any
 * descriptor directory is open (Linux accepts any position up to 2^31-1);
 * /proc's own metadata, listing, statfs, fdinfo, flock, refusals and the
 * names below its descriptor, and those of the descriptor directory
 * (check_procfs_directories); a fork's child's listing of the directory
 * it inherited (the parent's table, whole); the end after a descriptor table
 * grew and shrank, in a fork's child, and after a dup2 whose old descriptor
 * is not open; a received eventfd (which Detcore lists without modelling)
 * closed by the close-all loop; that neither an ordinary /tmp directory
 * seeked to 0x40000000 nor /proc passes for a descriptor directory when
 * received; a process's descriptor link read by a thread with a table of its
 * own; a FIFO opened through the directory; the main thread's
 * /proc/thread-self/fd read by another thread; the directory's own link,
 * followed and not, and a path through it; status flags, the O_PATH
 * refusals, and an anonymous mmap naming an O_PATH descriptor; a 4093-byte
 * path of ./ repeats; fchmodat2 below it; a name below a directory reached
 * through the directory, by its descriptor and through its link spelled
 * with // and /./; 40 and 41 descriptor links in one path (MAXSYMLINKS);
 * each directory's own descriptor link, alone, for the calls that follow it
 * (chdir and getcwd, access, the extended-attribute calls, chmod, utimes,
 * inotify_add_watch, statfs: check_bare_links); Linux's order of checks for
 * O_NOATIME, F_SETFL's O_NOATIME and O_ASYNC, the write hint (the inode's,
 * which another open reads), the O_PATH query commands, a negative position
 * or length through O_PATH, a read's buffer, copy_file_range, sendfile,
 * splice, tee, FIFREEZE, FITHAW, FIDEDUPERANGE and FICLONE (check_orders);
 * and, after exec, received eventfds made
 * close-on-exec by F_SETFD and not by FIONCLEX. A child's failed checks fail
 * its parent, and the checks before exec count after it.
 *
 * usage: proc_fd_directory [--no-exec] [--no-private-tables]
 *                          [--no-fifo-threads]
 *        proc_fd_directory --chroot-only | --stale-fchdir | --exec-below |
 *                          --unix-below | --send-locked | --lock-in-flight |
 *                          --transfer-into-pipe | --splice-into-pipe |
 *                          --lock-after-private-table |
 *                          --send-unscanned-control |
 *                          --send-rewritten-while-blocked |
 *                          --send-holding-the-directory |
 *                          --send-on-stdin-holding-the-directory
 *   --no-exec: skip the exec check.
 *   --no-private-tables: skip the thread with a table of its own (KVM gives
 *   no thread one).
 *   --no-fifo-threads: skip the FIFO whose writer is another thread (KVM's
 *   executor holds the turn in a blocking FIFO open).
 *   --chroot-only: only the child that keeps /proc and the descriptor
 *   directory across a chroot, and the children that chroot through each
 *   directory's own link (check_kept_after_chroot), alone.
 *   --stale-fchdir: fchdir to a descriptor directory whose task was reaped,
 *   which Linux enters (stale_fchdir).
 *   --exec-below, --unix-below: an exec of a name below the descriptor
 *   directory's descriptor, and an AF_UNIX address below its link, which
 *   Linux follows (below_refusals).
 *   --send-locked, --lock-in-flight, --transfer-into-pipe,
 *   --splice-into-pipe, --lock-after-private-table, --send-unscanned-control,
 *   --send-rewritten-while-blocked, --send-holding-the-directory,
 *   --send-on-stdin-holding-the-directory: the lock, transfer and send cases
 *   Hermit refuses by name, and one it runs (lifetime_refusals).
 */

#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif
#include <dirent.h>
#include <errno.h>
#include <stddef.h>
#include <fcntl.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sched.h>
#include <poll.h>
#include <time.h>
#include <netinet/in.h>
#include <sys/epoll.h>
#include <sys/eventfd.h>
#include <sys/file.h>
#include <sys/inotify.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/sendfile.h>
#include <sys/un.h>
#include <sys/xattr.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/statfs.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <sys/types.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <unistd.h>
#include <linux/fs.h>
#include <linux/futex.h>
#include <linux/openat2.h>

#ifndef F_DUPFD_QUERY
#define F_DUPFD_QUERY 1027
#endif
#ifndef F_CREATED_QUERY
#define F_CREATED_QUERY 1028
#endif
#ifndef F_GET_RW_HINT
#define F_GET_RW_HINT 1035
#define F_SET_RW_HINT 1036
#endif

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

struct record64 {
  uint64_t ino;
  int64_t off;
  unsigned short reclen;
  unsigned char type;
  char name[];
};

/* A record of CPython's size: struct linux_dirent64 with a 256-byte name. */
struct cpython_dirent64 {
  uint64_t d_ino;
  int64_t d_off;
  unsigned short d_reclen;
  unsigned char d_type;
  char d_name[256];
};

static const char *type_name(unsigned char type) {
  switch (type) {
    case DT_DIR:
      return "d";
    case DT_LNK:
      return "l";
    default:
      return "?";
  }
}

/* Read `fd` from its position with buffers of `capacity` bytes until the
 * end, appending " name:type@d_off" to `out` (with "@" omitted when
 * `offsets` is 0). Returns the number of getdents64 calls, or -errno of
 * the first that failed. If `dot_ino` is not NULL, it gets "."'s d_ino. */
static int list(int fd, size_t capacity, int offsets, char *out, size_t size,
                uint64_t *dot_ino) {
  char buffer[4096];
  int calls = 0;
  out[0] = '\0';
  for (;;) {
    long n = syscall(SYS_getdents64, fd, buffer, capacity);
    calls++;
    if (n < 0) {
      return -errno;
    }
    if (n == 0) {
      return calls;
    }
    for (long at = 0; at < n;) {
      struct record64 *record = (struct record64 *)(buffer + at);
      size_t used = strlen(out);
      if (offsets) {
        snprintf(out + used, size - used, " %s:%s@%lld", record->name,
                 type_name(record->type), (long long)record->off);
      } else {
        snprintf(out + used, size - used, " %s", record->name);
      }
      if (dot_ino != NULL && strcmp(record->name, ".") == 0) {
        *dot_ino = record->ino;
      }
      at += record->reclen;
    }
  }
}

static const char *errno_name(int error) {
  switch (error) {
    case 0:
      return "0";
    case EBADF:
      return "EBADF";
    case EINVAL:
      return "EINVAL";
    case ENXIO:
      return "ENXIO";
    case EACCES:
      return "EACCES";
    case ENOENT:
      return "ENOENT";
    case EISDIR:
      return "EISDIR";
    case EEXIST:
      return "EEXIST";
    case EOPNOTSUPP:
      return "EOPNOTSUPP";
    case ELOOP:
      return "ELOOP";
    case EPERM:
      return "EPERM";
    case ENOTTY:
      return "ENOTTY";
    case EWOULDBLOCK:
      return "EWOULDBLOCK";
    case ENOSYS:
      return "ENOSYS";
    case EROFS:
      return "EROFS";
    case ENOTDIR:
      return "ENOTDIR";
    case EXDEV:
      return "EXDEV";
    case ENOLCK:
      return "ENOLCK";
    case EFAULT:
      return "EFAULT";
    case ENODATA:
      return "ENODATA";
    case ENOMEM:
      return "ENOMEM";
    case ECONNREFUSED:
      return "ECONNREFUSED";
    case ESPIPE:
      return "ESPIPE";
    case EISCONN:
      return "EISCONN";
    case EPROTONOSUPPORT:
      return "EPROTONOSUPPORT";
    default:
      return "other";
  }
}

static void print_seek(const char *label, int fd, off_t offset, int whence) {
  errno = 0;
  off_t result = lseek(fd, offset, whence);
  printf("lseek %s=%lld errno=%s\n", label, (long long)result,
         errno_name(result < 0 ? errno : 0));
}

static int open_directory(const char *path, int flags) {
  int fd = open(path, flags);
  CHECK(fd >= 0, "open %s: %s", path, strerror(errno));
  return fd;
}

/* Send `fd` to ourselves over `pair` and return the received descriptor,
 * received with recvmsg `flags`. */
static int pass_to_self_with(int pair[2], int fd, int flags) {
  char byte = 'x';
  struct iovec iov = {.iov_base = &byte, .iov_len = 1};
  union {
    char buffer[CMSG_SPACE(sizeof(int))];
    struct cmsghdr align;
  } control;
  memset(&control, 0, sizeof(control));
  struct msghdr message = {
      .msg_iov = &iov,
      .msg_iovlen = 1,
      .msg_control = control.buffer,
      .msg_controllen = sizeof(control.buffer),
  };
  struct cmsghdr *header = CMSG_FIRSTHDR(&message);
  header->cmsg_level = SOL_SOCKET;
  header->cmsg_type = SCM_RIGHTS;
  header->cmsg_len = CMSG_LEN(sizeof(int));
  memcpy(CMSG_DATA(header), &fd, sizeof(int));
  CHECK(sendmsg(pair[0], &message, 0) == 1, "sendmsg: %s", strerror(errno));
  memset(&control, 0, sizeof(control));
  message.msg_controllen = sizeof(control.buffer);
  CHECK(recvmsg(pair[1], &message, flags) == 1, "recvmsg: %s", strerror(errno));
  header = CMSG_FIRSTHDR(&message);
  if (header == NULL || header->cmsg_type != SCM_RIGHTS) {
    CHECK(0, "no descriptor received");
    return -1;
  }
  int received;
  memcpy(&received, CMSG_DATA(header), sizeof(int));
  return received;
}

static int pass_to_self(int pair[2], int fd) {
  return pass_to_self_with(pair, fd, 0);
}

/* Wait for a child, which exits non-zero if any of its checks failed, and
 * count its failure as this process's. Returns its exit status, or -1. */
static int reap(pid_t child, const char *what) {
  int status = 0;
  CHECK(waitpid(child, &status, 0) == child, "waitpid %s: %s", what, strerror(errno));
  int exited = WIFEXITED(status) ? WEXITSTATUS(status) : -1;
  CHECK(exited == 0, "%s exited %d", what, exited);
  return exited;
}

static void *thread_spellings(void *argument) {
  (void)argument;
  char path[64], names[512];
  int fd = open_directory("/proc/thread-self/fd", O_RDONLY | O_DIRECTORY);
  list(fd, sizeof(names), 0, names, sizeof(names), NULL);
  printf("thread: thread-self/fd:%s\n", names);
  close(fd);
  snprintf(path, sizeof(path), "/proc/self/task/%ld/fd", (long)syscall(SYS_gettid));
  fd = open_directory(path, O_RDONLY | O_DIRECTORY);
  list(fd, sizeof(names), 0, names, sizeof(names), NULL);
  printf("thread: self/task/<tid>/fd:%s\n", names);
  close(fd);
  /* The process's directory, which Linux reads from the leader's table,
   * which this thread shares. */
  fd = open_directory("/proc/self/fd", O_RDONLY | O_DIRECTORY);
  list(fd, sizeof(names), 0, names, sizeof(names), NULL);
  printf("thread: self/fd:%s\n", names);
  close(fd);
  return NULL;
}

/* After exec: the inherited directory lists the new image's table. */
static int after_exec(int directory, int cloexec, int received_cloexec, int received_kept,
                      int inherited_failures) {
  failures += inherited_failures;
  char names[512];
  printf("after exec: rewind=%lld\n", (long long)lseek(directory, 0, SEEK_SET));
  list(directory, 4096, 0, names, sizeof(names), NULL);
  char own[16], gone[16];
  snprintf(own, sizeof(own), " %d", directory);
  snprintf(gone, sizeof(gone), " %d", cloexec);
  printf("after exec: listing:%s\n", names);
  printf("after exec: directory listed=%d close-on-exec listed=%d\n", strstr(names, own) != NULL,
         strstr(names, gone) != NULL);
  char received_gone[16], received_here[16];
  snprintf(received_gone, sizeof(received_gone), " %d", received_cloexec);
  snprintf(received_here, sizeof(received_here), " %d", received_kept);
  printf("after exec: received eventfd made close-on-exec by F_SETFD listed=%d, received "
         "MSG_CMSG_CLOEXEC then FIONCLEX listed=%d\n",
         strstr(names, received_gone) != NULL, strstr(names, received_here) != NULL);
  close(received_kept);
  close(directory);
  printf("failures=%d\n", failures);
  return failures == 0 ? 0 : 1;
}

/* CPython 3.9's _close_open_fds_safe, in a child: returns how many it
 * closed. */
static int cpython_close_all(void) {
  int fd_dir_fd = open("/proc/self/fd", O_RDONLY | O_DIRECTORY | O_CLOEXEC);
  if (fd_dir_fd < 0) {
    return -1;
  }
  char buffer[sizeof(struct cpython_dirent64)];
  int closed = 0;
  long bytes;
  while ((bytes = syscall(SYS_getdents64, fd_dir_fd, buffer, sizeof(buffer))) > 0) {
    for (long offset = 0; offset < bytes;) {
      struct cpython_dirent64 *entry = (struct cpython_dirent64 *)(buffer + offset);
      offset += entry->d_reclen;
      if (entry->d_name[0] < '0' || entry->d_name[0] > '9') {
        continue;
      }
      int fd = atoi(entry->d_name);
      if (fd > 2 && fd != fd_dir_fd) {
        close(fd);
        closed++;
      }
    }
  }
  close(fd_dir_fd);
  return closed;
}

/* A thread with a descriptor table of its own (clone without CLONE_FILES):
 * its descriptor `number` is its thread's directory, where the leader's is
 * /dev/null. Linux reads /proc/self/fd/N in the leader's table and
 * /proc/thread-self/fd/N in the thread's. */
struct private_table {
  int number;
};

static int in_private_table(void *argument) {
  struct private_table *table = argument;
  char link[128], path[64], expected[64];
  int own = open("/proc/thread-self/fd", O_RDONLY | O_DIRECTORY);
  CHECK(own >= 0, "open /proc/thread-self/fd in the private table: %s", strerror(errno));
  CHECK(dup2(own, table->number) == table->number, "dup2: %s", strerror(errno));
  close(own);
  snprintf(path, sizeof(path), "/proc/self/fd/%d", table->number);
  ssize_t length = readlink(path, link, sizeof(link) - 1);
  link[length < 0 ? 0 : length] = '\0';
  CHECK(strcmp(link, "/dev/null") == 0, "private table: %s reads %s, not the leader's /dev/null",
        path, link);
  errno = 0;
  int opened = open(path, O_RDONLY | O_DIRECTORY);
  CHECK(opened < 0 && errno == ENOTDIR, "private table: open %s O_DIRECTORY=%d errno=%d", path,
        opened, errno);
  snprintf(path, sizeof(path), "/proc/thread-self/fd/%d", table->number);
  length = readlink(path, link, sizeof(link) - 1);
  link[length < 0 ? 0 : length] = '\0';
  snprintf(expected, sizeof(expected), "/proc/%d/task/%ld/fd", (int)getpid(),
           (long)syscall(SYS_gettid));
  CHECK(strcmp(link, expected) == 0, "private table: %s reads %s, not %s", path, link, expected);
  close(table->number);
  return 0;
}

static void check_private_table(int null) {
  struct private_table table = {null};
  size_t size = 1 << 18;
  char *stack =
      mmap(NULL, size, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_STACK, -1, 0);
  CHECK(stack != MAP_FAILED, "mmap: %s", strerror(errno));
  if (stack == MAP_FAILED) {
    return;
  }
  static volatile pid_t word;
  int flags = CLONE_VM | CLONE_FS | CLONE_SIGHAND | CLONE_THREAD | CLONE_SYSVSEM |
              CLONE_PARENT_SETTID | CLONE_CHILD_CLEARTID;
  int tid = clone(in_private_table, stack + size, flags, &table, &word, NULL, &word);
  CHECK(tid > 0, "clone without CLONE_FILES: %s", strerror(errno));
  for (pid_t seen; tid > 0 && (seen = word) != 0;) {
    syscall(SYS_futex, &word, FUTEX_WAIT, seen, NULL, NULL, 0);
  }
  munmap(stack, size);
}

/* A FIFO opened through the directory waits for a writer, another thread. */
static const char *fifo_path;
static void *fifo_writer(void *argument) {
  (void)argument;
  int end = open(fifo_path, O_WRONLY);
  CHECK(end >= 0, "open the FIFO for writing: %s", strerror(errno));
  CHECK(write(end, "fifo", 4) == 4, "write the FIFO: %s", strerror(errno));
  close(end);
  return NULL;
}

/* A thread reading the main thread's /proc/thread-self/fd, whose table it
 * shares. */
static int main_thread_directory;
static void *shared_table_reader(void *argument) {
  char *names = argument;
  lseek(main_thread_directory, 0, SEEK_SET);
  list(main_thread_directory, 4096, 0, names, 512, NULL);
  return NULL;
}

static void print_errno(const char *label, long result) {
  printf("%s=%ld errno=%s\n", label, result, errno_name(result < 0 ? errno : 0));
}

/* The fdinfo of descriptor `fd`: its pos and ino, and its mnt_id. */
static int read_fdinfo(int fd, long long *pos, unsigned long long *ino, int *mnt_id) {
  char path[64], text[512];
  snprintf(path, sizeof(path), "/proc/self/fdinfo/%d", fd);
  int info = open(path, O_RDONLY);
  if (info < 0) {
    return -errno;
  }
  ssize_t n = read(info, text, sizeof(text) - 1);
  int error = n < 0 ? errno : 0;
  close(info);
  if (n < 0) {
    return -error;
  }
  text[n] = '\0';
  const char *at = strstr(text, "pos:");
  *pos = at != NULL ? strtoll(at + 4, NULL, 10) : -1;
  at = strstr(text, "ino:");
  *ino = at != NULL ? strtoull(at + 4, NULL, 10) : 0;
  at = strstr(text, "mnt_id:");
  *mnt_id = at != NULL ? atoi(at + 7) : -1;
  return 0;
}

/* The mount ID of the mount /proc/self/mountinfo shows at /proc, the last
 * row there (the one on top), or -1. */
static int proc_mount_id(void) {
  FILE *rows = fopen("/proc/self/mountinfo", "r");
  if (rows == NULL) {
    return -1;
  }
  char line[1024];
  int found = -1;
  while (fgets(line, sizeof(line), rows) != NULL) {
    int id = 0, parent = 0;
    char device[64], root[256], point[256];
    if (sscanf(line, "%d %d %63s %255s %255s", &id, &parent, device, root, point) == 5 &&
        strcmp(point, "/proc") == 0) {
      found = id;
    }
  }
  fclose(rows);
  return found;
}

/* A process that changes its root keeps the /proc and the descriptor
 * directory it holds, and Linux still looks names up below them in procfs
 * (Hermit serves no placeholder directory after a chroot, so they are the
 * kernel's there). A fork's child changes its root to an empty directory,
 * opens self/status below the /proc it kept, and lists the descriptor
 * directory it kept, which is its parent's (/proc/<parent>/fd). A child
 * whose chroot fails (natively without CAP_SYS_CHROOT, or a backend that
 * refuses chroot) reports E for both. */
static void check_kept_after_chroot(int root, int directory) {
  char empty[] = "/tmp/proc-fd-directory-root-XXXXXX";
  int report[2];
  if (mkdtemp(empty) == NULL || pipe(report) != 0) {
    CHECK(0, "chroot check setup: %s", strerror(errno));
    return;
  }
  pid_t child = fork();
  CHECK(child >= 0, "fork: %s", strerror(errno));
  if (child == 0) {
    char result[3] = "EE";
    if (chroot(empty) == 0 && chdir("/") == 0) {
      int status = openat(root, "self/status", O_RDONLY | O_CLOEXEC);
      char line[16] = "";
      ssize_t got = status < 0 ? -1 : read(status, line, 5);
      result[0] = got == 5 && strncmp(line, "Name:", 5) == 0 ? '1' : '0';
      char names[1024];
      lseek(directory, 0, SEEK_SET);
      int calls = list(directory, 4096, 0, names, sizeof(names), NULL);
      strcat(names, " ");
      result[1] = calls > 0 && strstr(names, " . ") != NULL && strstr(names, " 0 ") != NULL &&
                          strstr(names, " 1 ") != NULL
                      ? '1'
                      : '0';
    }
    ssize_t written = write(report[1], result, 2);
    _exit(written == 2 ? 0 : 1);
  }
  close(report[1]);
  char result[3] = "??";
  ssize_t got = read(report[0], result, 2);
  close(report[0]);
  int status = 0;
  CHECK(waitpid(child, &status, 0) == child && WIFEXITED(status) && WEXITSTATUS(status) == 0 &&
            got == 2,
        "the chrooted child");
  rmdir(empty);
  printf("chroot: a child that changed its root opens self/status below the /proc it kept=%c, "
         "lists the descriptor directory it kept=%c\n",
         result[0], result[1]);
  /* chroot through each directory's own descriptor link, which it follows:
   * the new root is the directory, /proc or the descriptor directory its
   * descriptor is bound to. */
  for (int which = 0; which < 2; which++) {
    int fd = which == 0 ? root : directory;
    fflush(stdout);
    pid_t through = fork();
    CHECK(through >= 0, "fork: %s", strerror(errno));
    if (through == 0) {
      char link[64];
      snprintf(link, sizeof(link), "/proc/self/fd/%d", fd);
      errno = 0;
      int changed = chroot(link);
      int change_error = changed < 0 ? errno : 0;
      struct stat inside;
      int found = changed == 0 && stat(which == 0 ? "/self" : "/0", &inside) == 0;
      printf("chroot through the %s descriptor's link=%d errno=%s finds %s=%d\n",
             which == 0 ? "/proc" : "/proc/self/fd", changed, errno_name(change_error),
             which == 0 ? "/self" : "/0", found);
      fflush(stdout);
      _exit(0);
    }
    int through_status = 0;
    CHECK(waitpid(through, &through_status, 0) == through && WIFEXITED(through_status),
          "the chroot-through-link child");
  }
}

/* /proc and /proc/self/fd are directories of their own on Linux: procfs's
 * root and the task's descriptor directory. Hermit serves both on
 * placeholder descriptors (package K slice P1), and every call through such
 * a descriptor gets procfs's answer. `directory` is open on /proc/self/fd. */
static void check_procfs_directories(int directory) {
  char cwd[128], listing[8192], label[160];
  int root = open_directory("/proc", O_RDONLY | O_DIRECTORY);
  struct stat root_stat, root_path, dir_stat;
  CHECK(fstat(root, &root_stat) == 0, "fstat /proc: %s", strerror(errno));
  CHECK(stat("/proc", &root_path) == 0, "stat /proc: %s", strerror(errno));
  CHECK(fstat(directory, &dir_stat) == 0, "fstat directory: %s", strerror(errno));
  printf("root: fstat dir=%d mode=%o blksize=%ld size=%lld stat-path-same-ino=%d\n",
         S_ISDIR(root_stat.st_mode), root_stat.st_mode & 07777, (long)root_stat.st_blksize,
         (long long)root_stat.st_size, root_path.st_ino == root_stat.st_ino);
  struct statx extended;
  CHECK(statx(root, "", AT_EMPTY_PATH, STATX_BASIC_STATS, &extended) == 0, "statx /proc: %s",
        strerror(errno));
  printf("root: statx mount-root=%d same-ino=%d\n",
         (extended.stx_attributes & STATX_ATTR_MOUNT_ROOT) != 0,
         extended.stx_ino == root_stat.st_ino);
  uint64_t dot = 0;
  list(root, 4096, 0, listing, sizeof(listing), &dot);
  char own_pid[24];
  snprintf(own_pid, sizeof(own_pid), " %d ", (int)getpid());
  strcat(listing, " ");
  printf("root: listing self=%d thread-self=%d own-pid=%d fs=%d dot-ino-is-fstat-ino=%d\n",
         strstr(listing, " self ") != NULL, strstr(listing, " thread-self ") != NULL,
         strstr(listing, own_pid) != NULL, strstr(listing, " fs ") != NULL,
         dot == root_stat.st_ino);
  /* The descriptor's own link names /proc. */
  char link[128], own_link[64];
  snprintf(own_link, sizeof(own_link), "/proc/self/fd/%d", root);
  ssize_t length = readlink(own_link, link, sizeof(link) - 1);
  link[length < 0 ? 0 : length] = '\0';
  printf("root: own link names /proc=%d\n", strcmp(link, "/proc") == 0);

  /* fstatfs: procfs (PROC_SUPER_MAGIC), whose blocks and files are none. */
  struct statfs fs_root, fs_dir;
  CHECK(fstatfs(root, &fs_root) == 0, "fstatfs /proc: %s", strerror(errno));
  CHECK(fstatfs(directory, &fs_dir) == 0, "fstatfs directory: %s", strerror(errno));
  printf("fstatfs: /proc type=%lx bsize=%ld namelen=%ld blocks=%llu files=%llu, "
         "/proc/self/fd type=%lx\n",
         (unsigned long)fs_root.f_type, (long)fs_root.f_bsize, (long)fs_root.f_namelen,
         (unsigned long long)fs_root.f_blocks, (unsigned long long)fs_root.f_files,
         (unsigned long)fs_dir.f_type);

  /* The calls procfs refuses, each with its errno, on both directories. */
  int fds[2] = {root, directory};
  const char *which[2] = {"/proc", "/proc/self/fd"};
  for (int i = 0; i < 2; i++) {
    int fd = fds[i];
    errno = 0;
    int fsynced = fsync(fd);
    int fsync_error = fsynced < 0 ? errno : 0;
    errno = 0;
    int datasynced = fdatasync(fd);
    int datasync_error = datasynced < 0 ? errno : 0;
    errno = 0;
    int synced = syncfs(fd);
    printf("sync: %s fsync=%d errno=%s fdatasync=%d errno=%s syncfs=%d errno=%s\n", which[i],
           fsynced, errno_name(fsync_error), datasynced, errno_name(datasync_error), synced,
           errno_name(synced < 0 ? errno : 0));
    char value[16], names[64];
    errno = 0;
    ssize_t got = fgetxattr(fd, "user.hermit", value, sizeof(value));
    int get_error = got < 0 ? errno : 0;
    errno = 0;
    ssize_t listed = flistxattr(fd, names, sizeof(names));
    int list_error = listed < 0 ? errno : 0;
    errno = 0;
    int set = fsetxattr(fd, "user.hermit", "1", 1, 0);
    int set_error = set < 0 ? errno : 0;
    errno = 0;
    int removed = fremovexattr(fd, "user.hermit");
    printf("xattr: %s fgetxattr=%zd errno=%s flistxattr=%zd errno=%s fsetxattr=%d errno=%s "
           "fremovexattr=%d errno=%s\n",
           which[i], got, errno_name(get_error), listed, errno_name(list_error), set,
           errno_name(set_error), removed, errno_name(removed < 0 ? errno : 0));
    unsigned int attributes = 0;
    errno = 0;
    int flags = ioctl(fd, FS_IOC_GETFLAGS, &attributes);
    int flags_error = flags < 0 ? errno : 0;
    int block = 0;
    errno = 0;
    int sized = ioctl(fd, FIGETBSZ, &block);
    int sized_error = sized < 0 ? errno : 0;
    int pending = 0;
    errno = 0;
    int counted = ioctl(fd, FIONREAD, &pending);
    printf("ioctl: %s FS_IOC_GETFLAGS=%d errno=%s FIGETBSZ=%d errno=%s block=%d FIONREAD=%d "
           "errno=%s\n",
           which[i], flags, errno_name(flags_error), sized, errno_name(sized_error), block,
           counted, errno_name(counted < 0 ? errno : 0));
    errno = 0;
    int moded = fchmod(fd, 0700);
    int mode_error = moded < 0 ? errno : 0;
    errno = 0;
    int owned = fchown(fd, (uid_t)-1, (gid_t)-1);
    int own_error = owned < 0 ? errno : 0;
    errno = 0;
    int touched = futimens(fd, NULL);
    int touch_error = touched < 0 ? errno : 0;
    errno = 0;
    int truncated = ftruncate(fd, 0);
    int truncate_error = truncated < 0 ? errno : 0;
    errno = 0;
    ssize_t read_bytes = read(fd, value, sizeof(value));
    printf("mutation: %s fchmod=%d errno=%s fchown=%d errno=%s futimens=%d errno=%s "
           "ftruncate=%d errno=%s read=%zd errno=%s\n",
           which[i], moded, errno_name(mode_error), owned, errno_name(own_error), touched,
           errno_name(touch_error), truncated, errno_name(truncate_error), read_bytes,
           errno_name(read_bytes < 0 ? errno : 0));
  }
  /* A name made below /proc: procfs refuses it. */
  errno = 0;
  int made = mkdirat(root, "hermit-placeholder", 0755);
  printf("mutation: mkdirat /proc=%d errno=%s\n", made, errno_name(made < 0 ? errno : 0));

  /* flock: each directory is an inode of its own, whose lock another open
   * of the same directory meets, and a dup shares. */
  int root_again = open_directory("/proc", O_RDONLY | O_DIRECTORY);
  int directory_again = open_directory("/proc/self/fd", O_RDONLY | O_DIRECTORY);
  int thread_directory = open_directory("/proc/thread-self/fd", O_RDONLY | O_DIRECTORY);
  int locks[8];
  locks[0] = flock(root, LOCK_EX | LOCK_NB);
  locks[1] = flock(directory, LOCK_EX | LOCK_NB);
  errno = 0;
  locks[2] = flock(root_again, LOCK_EX | LOCK_NB) < 0 ? -errno : 0;
  errno = 0;
  locks[3] = flock(directory_again, LOCK_SH | LOCK_NB) < 0 ? -errno : 0;
  locks[4] = flock(thread_directory, LOCK_EX | LOCK_NB);
  int root_dup = dup(root);
  locks[5] = flock(root_dup, LOCK_EX | LOCK_NB);
  locks[6] = flock(root, LOCK_UN);
  locks[7] = flock(root_again, LOCK_EX | LOCK_NB);
  printf("flock: /proc=%d /proc/self/fd=%d /proc again=%s /proc/self/fd again=%s "
         "/proc/thread-self/fd=%d dup of /proc=%d unlock=%d /proc again after=%d\n",
         locks[0], locks[1], errno_name(-locks[2]), errno_name(-locks[3]), locks[4], locks[5],
         locks[6], locks[7]);
  flock(root_again, LOCK_UN);
  flock(directory, LOCK_UN);
  flock(thread_directory, LOCK_UN);
  close(root_dup);
  close(root_again);
  close(directory_again);
  close(thread_directory);

  /* fdinfo: the position, the directory's inode, and the mount mountinfo
   * shows at /proc. */
  long long root_pos = -2, dir_pos = -2;
  unsigned long long root_ino = 0, dir_ino = 0;
  int root_mnt = -1, dir_mnt = -2;
  lseek(root, 3, SEEK_SET);
  lseek(directory, 2, SEEK_SET);
  int root_info = read_fdinfo(root, &root_pos, &root_ino, &root_mnt);
  int dir_info = read_fdinfo(directory, &dir_pos, &dir_ino, &dir_mnt);
  int proc_mount = proc_mount_id();
  printf("fdinfo: /proc rc=%s pos=%lld ino-is-fstat-ino=%d, /proc/self/fd rc=%s pos=%lld "
         "ino-is-fstat-ino=%d, both on the mount mountinfo shows at /proc=%d\n",
         errno_name(-root_info), root_pos, root_ino == root_stat.st_ino, errno_name(-dir_info),
         dir_pos, dir_ino == dir_stat.st_ino, root_mnt == proc_mount && dir_mnt == proc_mount);
  lseek(root, 0, SEEK_SET);
  lseek(directory, 0, SEEK_SET);

  /* fchdir to /proc: getcwd names /proc. */
  errno = 0;
  int entered = fchdir(root);
  int enter_error = entered < 0 ? errno : 0;
  int in_proc = entered == 0 && getcwd(cwd, sizeof(cwd)) != NULL && strcmp(cwd, "/proc") == 0;
  if (entered == 0) {
    CHECK(chdir("/") == 0, "chdir: %s", strerror(errno));
  }
  printf("fchdir: /proc=%d errno=%s cwd is /proc=%d\n", entered, errno_name(enter_error),
         in_proc);

  /* openat2 below /proc (Hermit answers every openat2 with ENOSYS). */
  struct open_how how;
  memset(&how, 0, sizeof(how));
  how.flags = O_RDONLY;
  errno = 0;
  long opened2 = syscall(SYS_openat2, root, "self", &how, sizeof(how));
  printf("openat2: below /proc=%s\n", opened2 >= 0 ? "opened" : errno_name(errno));
  if (opened2 >= 0) {
    close((int)opened2);
  }

  /* /proc/fs, which Hermit hides at every --proc level, by every spelling
   * below the two directories. */
  struct {
    int dirfd;
    const char *path;
  } hidden[] = {
      {root, "fs"},
      {root, "fs/nfsd"},
      {root, "./fs/"},
      {root, "self/../fs"},
      {directory, "../../fs"},
      {AT_FDCWD, "/proc/fs/nfsd"},
      {AT_FDCWD, "/proc//fs/nfsd/."},
  };
  printf("hidden:");
  for (size_t i = 0; i < sizeof(hidden) / sizeof(hidden[0]); i++) {
    struct stat found;
    errno = 0;
    int stated = fstatat(hidden[i].dirfd, hidden[i].path, &found, 0);
    int stat_error = stated < 0 ? errno : 0;
    errno = 0;
    int opened = openat(hidden[i].dirfd, hidden[i].path, O_RDONLY | O_DIRECTORY);
    int open_error = opened < 0 ? errno : 0;
    if (opened >= 0) {
      close(opened);
    }
    printf(" %s%s:stat=%s,open=%s",
           hidden[i].dirfd == root        ? "(/proc)"
           : hidden[i].dirfd == directory ? "(/proc/self/fd)"
                                          : "",
           hidden[i].path, errno_name(stat_error), errno_name(open_error));
  }
  snprintf(label, sizeof(label), "/proc/self/fd/%d/fs", root);
  struct stat found;
  errno = 0;
  int through = stat(label, &found);
  printf(" /proc/self/fd/(/proc)/fs:stat=%s\n", errno_name(through < 0 ? errno : 0));

  /* Names below /proc's descriptor, each looked up as its absolute spelling
   * would be: self (the caller's pid), self/fd (the descriptor directory,
   * fstat's inode of it), self/fd/<it> (this descriptor's own link: /proc),
   * uptime and self/status (readable), locks (as /proc/locks: absent where
   * Hermit's --proc level omits it), and .. (the root directory). */
  char self_name[32], pid_text[24], it_link_name[32], it_link[64], status_line[64];
  snprintf(pid_text, sizeof(pid_text), "%d", (int)getpid());
  ssize_t self_length = readlinkat(root, "self", self_name, sizeof(self_name) - 1);
  self_name[self_length < 0 ? 0 : self_length] = '\0';
  struct stat below_fd, below_parent, slash;
  int below_fd_rc = fstatat(root, "self/fd", &below_fd, 0);
  snprintf(it_link_name, sizeof(it_link_name), "self/fd/%d", root);
  ssize_t it_length = readlinkat(root, it_link_name, it_link, sizeof(it_link) - 1);
  it_link[it_length < 0 ? 0 : it_length] = '\0';
  errno = 0;
  int uptime_fd = openat(root, "uptime", O_RDONLY | O_CLOEXEC);
  int uptime_error = uptime_fd < 0 ? errno : 0;
  if (uptime_fd >= 0) {
    close(uptime_fd);
  }
  errno = 0;
  int locks_fd = openat(root, "locks", O_RDONLY | O_CLOEXEC);
  int locks_error = locks_fd < 0 ? errno : 0;
  if (locks_fd >= 0) {
    close(locks_fd);
  }
  errno = 0;
  int locks_path = open("/proc/locks", O_RDONLY | O_CLOEXEC);
  int locks_path_error = locks_path < 0 ? errno : 0;
  if (locks_path >= 0) {
    close(locks_path);
  }
  int status_fd = openat(root, "self/status", O_RDONLY | O_CLOEXEC);
  ssize_t status_bytes = status_fd < 0 ? -1 : read(status_fd, status_line, sizeof(status_line) - 1);
  status_line[status_bytes < 0 ? 0 : status_bytes] = '\0';
  if (status_fd >= 0) {
    close(status_fd);
  }
  int parent_rc = fstatat(root, "..", &below_parent, 0);
  CHECK(stat("/", &slash) == 0, "stat /: %s", strerror(errno));
  printf("below /proc: self is the caller's pid=%d self/fd is the directory=%d "
         "self/fd/<it> names /proc=%d uptime=%s locks as /proc/locks=%d "
         "self/status starts with Name=%d ..=/:%d\n",
         strcmp(self_name, pid_text) == 0, below_fd_rc == 0 && below_fd.st_ino == dir_stat.st_ino,
         strcmp(it_link, "/proc") == 0, uptime_fd >= 0 ? "opened" : errno_name(uptime_error),
         (locks_fd >= 0) == (locks_path >= 0) && locks_error == locks_path_error,
         strncmp(status_line, "Name:", 5) == 0,
         parent_rc == 0 && below_parent.st_ino == slash.st_ino && below_parent.st_dev == slash.st_dev);

  /* A received /proc descriptor is /proc: its identity is the placeholder's,
   * and its kernel position tags it. */
  int pair[2];
  CHECK(socketpair(AF_UNIX, SOCK_STREAM, 0, pair) == 0, "socketpair: %s", strerror(errno));
  int received = pass_to_self(pair, root);
  struct stat received_stat;
  CHECK(fstat(received, &received_stat) == 0, "fstat received: %s", strerror(errno));
  list(received, 4096, 0, listing, sizeof(listing), NULL);
  strcat(listing, " ");
  printf("received /proc: same-ino=%d lists self=%d\n", received_stat.st_ino == root_stat.st_ino,
         strstr(listing, " self ") != NULL);
  close(received);
  close(pair[0]);
  close(pair[1]);
  close(root);
}

/* What a call that follows a final link gets for each directory's own
 * descriptor link, /proc/self/fd/<it>, and nothing more: the directory
 * itself, as Linux follows the link (chdir and getcwd, access, the
 * extended-attribute calls, chmod, utimes, inotify_add_watch, statfs). */
static void check_bare_links(int root, int directory) {
  char link_root[64], link_dir[64], expected_dir[64], cwd[128];
  snprintf(link_root, sizeof(link_root), "/proc/self/fd/%d", root);
  snprintf(link_dir, sizeof(link_dir), "/proc/self/fd/%d", directory);
  snprintf(expected_dir, sizeof(expected_dir), "/proc/%d/fd", (int)getpid());
  for (int which = 0; which < 2; which++) {
    const char *link = which == 0 ? link_root : link_dir;
    const char *name = which == 0 ? "/proc" : "/proc/self/fd";
    errno = 0;
    int entered = chdir(link);
    int enter_error = entered < 0 ? errno : 0;
    int named = entered == 0 && getcwd(cwd, sizeof(cwd)) != NULL &&
                strcmp(cwd, which == 0 ? "/proc" : expected_dir) == 0;
    if (entered == 0) {
      CHECK(chdir("/") == 0, "chdir: %s", strerror(errno));
    }
    errno = 0;
    int write_access = access(link, W_OK);
    int write_error = write_access < 0 ? errno : 0;
    errno = 0;
    int search_access = access(link, R_OK | X_OK);
    int search_error = search_access < 0 ? errno : 0;
    char value[16];
    errno = 0;
    ssize_t got = getxattr(link, "user.hermit", value, sizeof(value));
    int get_error = got < 0 ? errno : 0;
    errno = 0;
    int set = setxattr(link, "user.hermit", "1", 1, 0);
    int set_error = set < 0 ? errno : 0;
    errno = 0;
    int removed = removexattr(link, "user.hermit");
    int remove_error = removed < 0 ? errno : 0;
    char names[64];
    errno = 0;
    ssize_t listed = listxattr(link, names, sizeof(names));
    int list_error = listed < 0 ? errno : 0;
    errno = 0;
    int changed = chmod(link, which == 0 ? 0555 : 0500);
    int chmod_error = changed < 0 ? errno : 0;
    errno = 0;
    int touched = utimes(link, NULL);
    int utimes_error = touched < 0 ? errno : 0;
    int notify = inotify_init1(IN_CLOEXEC);
    errno = 0;
    int watch = inotify_add_watch(notify, link, IN_ALL_EVENTS);
    int watch_error = watch < 0 ? errno : 0;
    close(notify);
    struct statfs fs;
    errno = 0;
    int stated = statfs(link, &fs);
    int statfs_error = stated < 0 ? errno : 0;
    printf("bare link %s: chdir=%d errno=%s getcwd names it=%d access W_OK=%s R_OK|X_OK=%s "
           "getxattr=%s setxattr=%s removexattr=%s listxattr=%zd errno=%s chmod=%s utimes=%s "
           "inotify_add_watch=%d errno=%s statfs=%s type=%lx\n",
           name, entered, errno_name(enter_error), named, errno_name(write_error),
           errno_name(search_error), errno_name(get_error), errno_name(set_error),
           errno_name(remove_error), listed, errno_name(list_error), errno_name(chmod_error),
           errno_name(utimes_error), watch >= 0, errno_name(watch_error),
           errno_name(statfs_error), stated == 0 ? (unsigned long)fs.f_type : 0UL);
  }
}

/* Linux's answers, in its order of checks, to calls through the two
 * directories' descriptors that Detcore answers itself. */
static void check_orders(int directory) {
  int root = open_directory("/proc", O_RDONLY | O_DIRECTORY);
  /* O_NOATIME needs the directory's owner, or one who may act as it:
   * /proc belongs to global root, the caller's directory to the caller. */
  errno = 0;
  int noatime_root = open("/proc", O_RDONLY | O_DIRECTORY | O_NOATIME);
  int noatime_root_error = noatime_root < 0 ? errno : 0;
  errno = 0;
  int noatime_dir = open("/proc/self/fd", O_RDONLY | O_DIRECTORY | O_NOATIME);
  int noatime_dir_error = noatime_dir < 0 ? errno : 0;
  int dir_flags = noatime_dir < 0 ? -1 : fcntl(noatime_dir, F_GETFL);
  printf("O_NOATIME: open /proc=%s /proc/self/fd=%s kept=%d\n",
         noatime_root >= 0 ? "opened" : errno_name(noatime_root_error),
         noatime_dir >= 0 ? "opened" : errno_name(noatime_dir_error),
         dir_flags >= 0 && (dir_flags & O_NOATIME) != 0);
  if (noatime_root >= 0) {
    close(noatime_root);
  }
  if (noatime_dir >= 0) {
    close(noatime_dir);
  }
  int second = open_directory("/proc/self/fd", O_RDONLY | O_DIRECTORY);
  errno = 0;
  int setfl_root = fcntl(root, F_SETFL, O_NOATIME);
  int setfl_root_error = setfl_root < 0 ? errno : 0;
  errno = 0;
  int setfl_dir = fcntl(second, F_SETFL, O_NOATIME);
  int setfl_dir_error = setfl_dir < 0 ? errno : 0;
  int after_noatime = fcntl(second, F_GETFL);
  errno = 0;
  int setfl_async = fcntl(second, F_SETFL, O_ASYNC | O_NOATIME);
  int setfl_async_error = setfl_async < 0 ? errno : 0;
  int after_async = fcntl(second, F_GETFL);
  printf("F_SETFL: /proc O_NOATIME=%d errno=%s, /proc/self/fd O_NOATIME=%d errno=%s kept=%d, "
         "O_ASYNC=%d errno=%s kept=%d\n",
         setfl_root, errno_name(setfl_root_error), setfl_dir, errno_name(setfl_dir_error),
         after_noatime >= 0 && (after_noatime & O_NOATIME) != 0, setfl_async,
         errno_name(setfl_async_error), after_async >= 0 && (after_async & O_ASYNC) != 0);
  /* The write hint is the inode's: another open of the directory reads it. */
  uint64_t hint = 2, read_back = 99, root_hint = 99;
  errno = 0;
  int set_dir = fcntl(second, F_SET_RW_HINT, &hint);
  int set_dir_error = set_dir < 0 ? errno : 0;
  int third = open_directory("/proc/self/fd", O_RDONLY | O_DIRECTORY);
  int got_dir = fcntl(third, F_GET_RW_HINT, &read_back);
  errno = 0;
  int set_root = fcntl(root, F_SET_RW_HINT, &hint);
  int set_root_error = set_root < 0 ? errno : 0;
  int got_root = fcntl(root, F_GET_RW_HINT, &root_hint);
  printf("write hint: /proc/self/fd set=%d errno=%s another open reads=%d:%llu, /proc set=%d "
         "errno=%s reads=%d:%llu\n",
         set_dir, errno_name(set_dir_error), got_dir, (unsigned long long)read_back, set_root,
         errno_name(set_root_error), got_root, (unsigned long long)root_hint);
  close(third);
  close(second);
  /* The query commands take an O_PATH descriptor. */
  int path_only = open_directory("/proc/self/fd", O_PATH | O_DIRECTORY);
  errno = 0;
  int same = fcntl(path_only, F_DUPFD_QUERY, path_only);
  int same_error = same < 0 ? errno : 0;
  errno = 0;
  int created = fcntl(path_only, F_CREATED_QUERY);
  int created_error = created < 0 ? errno : 0;
  char byte;
  errno = 0;
  ssize_t negative_pread = pread(path_only, &byte, 1, -1);
  int negative_pread_error = negative_pread < 0 ? errno : 0;
  errno = 0;
  int negative_truncate = ftruncate(path_only, -1);
  int negative_truncate_error = negative_truncate < 0 ? errno : 0;
  printf("O_PATH: F_DUPFD_QUERY=%d errno=%s F_CREATED_QUERY=%d errno=%s pread at -1=%s "
         "ftruncate to -1=%s\n",
         same, errno_name(same_error), created, errno_name(created_error),
         errno_name(negative_pread_error), errno_name(negative_truncate_error));
  close(path_only);
  /* A read checks its buffer before the directory refuses it. */
  char buffer[16];
  errno = 0;
  long bad_buffer = syscall(SYS_read, root, (void *)UINTPTR_MAX, 1);
  int bad_buffer_error = bad_buffer < 0 ? errno : 0;
  errno = 0;
  long huge = syscall(SYS_read, root, buffer, (size_t)SIZE_MAX);
  int huge_error = huge < 0 ? errno : 0;
  errno = 0;
  ssize_t nothing = read(root, buffer, 0);
  int nothing_error = nothing < 0 ? errno : 0;
  printf("read: /proc at UINTPTR_MAX=%s count SIZE_MAX=%s count 0=%zd errno=%s\n",
         errno_name(bad_buffer_error), errno_name(huge_error), nothing, errno_name(nothing_error));
  /* preadv2's flags are an int: a bit above 32 is no flag (EISDIR), where
   * an unknown one is EOPNOTSUPP. */
  {
    char byte;
    struct iovec vector = {.iov_base = &byte, .iov_len = 1};
    errno = 0;
    long high = syscall(SYS_preadv2, root, &vector, 1, 0, 0, 1ULL << 32);
    int high_error = high < 0 ? errno : 0;
    errno = 0;
    long unknown = syscall(SYS_preadv2, root, &vector, 1, 0, 0, 0x40);
    int unknown_error = unknown < 0 ? errno : 0;
    printf("preadv2: /proc flags 1<<32=%s flags 0x40=%s\n", errno_name(high_error),
           errno_name(unknown_error));
  }
  /* The transfers, in each one's order of checks. */
  char scratch[] = "/tmp/proc-fd-directory-file-XXXXXX";
  int file = mkstemp(scratch);
  CHECK(file >= 0, "mkstemp: %s", strerror(errno));
  unlink(scratch);
  CHECK(write(file, "x", 1) == 1, "write: %s", strerror(errno));
  int pipes[2];
  CHECK(pipe(pipes) == 0, "pipe: %s", strerror(errno));
  CHECK(write(pipes[1], "y", 1) == 1, "pipe write: %s", strerror(errno));
  struct {
    const char *label;
    long result;
    int error;
  } transfers[10];
  size_t count = 0;
#define TRANSFER(label_, call_)                                     \
  do {                                                              \
    errno = 0;                                                      \
    long result_ = (call_);                                         \
    transfers[count].label = (label_);                              \
    transfers[count].result = result_;                              \
    transfers[count].error = result_ < 0 ? errno : 0;               \
    count++;                                                        \
  } while (0)
  TRANSFER("copy_file_range(/proc,-1,1)", syscall(SYS_copy_file_range, root, NULL, -1, NULL, 1, 0));
  TRANSFER("copy_file_range(/proc,file,0)",
           syscall(SYS_copy_file_range, root, NULL, file, NULL, 0, 0));
  TRANSFER("copy_file_range(file,/proc,1)",
           syscall(SYS_copy_file_range, file, NULL, root, NULL, 1, 0));
  TRANSFER("sendfile(-1,/proc,1)", sendfile(-1, root, NULL, 1));
  TRANSFER("sendfile(file,/proc,1)", sendfile(file, root, NULL, 1));
  TRANSFER("splice(/proc,pipe,0)", splice(root, NULL, pipes[1], NULL, 0, 0));
  TRANSFER("tee(pipe,/proc,1)", tee(pipes[0], root, 1, 0));
  TRANSFER("tee(/proc,pipe,0)", tee(root, pipes[1], 0, 0));
  /* The flags at Linux's width: copy_file_range's are an unsigned int, so a
   * bit above 32 is no flag (EISDIR, as for no flags). */
  TRANSFER("copy_file_range(file,/proc,1,flags 1<<32)",
           syscall(SYS_copy_file_range, file, NULL, root, NULL, 1, 1ULL << 32));
#undef TRANSFER
  printf("transfers:");
  for (size_t i = 0; i < count; i++) {
    printf(" %s=%ld:%s", transfers[i].label, transfers[i].result,
           errno_name(transfers[i].error));
  }
  printf("\n");
  /* The ioctls with checks of their own. */
  errno = 0;
  int frozen = ioctl(root, FIFREEZE, 0);
  int freeze_error = frozen < 0 ? errno : 0;
  errno = 0;
  int thawed = ioctl(root, FITHAW, 0);
  int thaw_error = thawed < 0 ? errno : 0;
  errno = 0;
  int deduped = ioctl(root, FIDEDUPERANGE, NULL);
  int dedupe_error = deduped < 0 ? errno : 0;
  errno = 0;
  int cloned = ioctl(root, FICLONE, file);
  int clone_error = cloned < 0 ? errno : 0;
  errno = 0;
  int cloned_none = ioctl(directory, FICLONE, -1);
  int clone_none_error = cloned_none < 0 ? errno : 0;
  printf("ioctl: /proc FIFREEZE=%s FITHAW=%s FIDEDUPERANGE(NULL)=%s FICLONE(file)=%s, "
         "/proc/self/fd FICLONE(-1)=%s\n",
         errno_name(freeze_error), errno_name(thaw_error), errno_name(dedupe_error),
         errno_name(clone_error), errno_name(clone_none_error));
  close(pipes[0]);
  close(pipes[1]);
  close(file);
  close(root);
}

/* --stale-fchdir: a child sends its own /proc/self/fd and exits; once it is
 * reaped, fchdir to the directory still enters it on Linux, a directory
 * below which nothing exists, and getcwd fails. Under Hermit no directory
 * stands for it, and the fchdir is refused by name. */
static int stale_fchdir(void) {
  int pair[2];
  CHECK(socketpair(AF_UNIX, SOCK_STREAM, 0, pair) == 0, "socketpair: %s", strerror(errno));
  pid_t child = fork();
  CHECK(child >= 0, "fork: %s", strerror(errno));
  if (child == 0) {
    close(pair[0]);
    int own = open_directory("/proc/self/fd", O_RDONLY | O_DIRECTORY);
    char byte = 'd';
    struct iovec vector = {.iov_base = &byte, .iov_len = 1};
    char control[CMSG_SPACE(sizeof(int))];
    memset(control, 0, sizeof(control));
    struct msghdr message = {.msg_iov = &vector,
                             .msg_iovlen = 1,
                             .msg_control = control,
                             .msg_controllen = sizeof(control)};
    struct cmsghdr *header = CMSG_FIRSTHDR(&message);
    header->cmsg_level = SOL_SOCKET;
    header->cmsg_type = SCM_RIGHTS;
    header->cmsg_len = CMSG_LEN(sizeof(int));
    memcpy(CMSG_DATA(header), &own, sizeof(int));
    _exit(sendmsg(pair[1], &message, 0) == 1 ? 0 : 1);
  }
  close(pair[1]);
  char byte;
  struct iovec vector = {.iov_base = &byte, .iov_len = 1};
  char control[CMSG_SPACE(sizeof(int))];
  struct msghdr message = {.msg_iov = &vector,
                           .msg_iovlen = 1,
                           .msg_control = control,
                           .msg_controllen = sizeof(control)};
  CHECK(recvmsg(pair[0], &message, 0) == 1, "recvmsg: %s", strerror(errno));
  int received = -1;
  memcpy(&received, CMSG_DATA(CMSG_FIRSTHDR(&message)), sizeof(int));
  reap(child, "the sender");
  errno = 0;
  int entered = fchdir(received);
  int enter_error = entered < 0 ? errno : 0;
  char cwd[128];
  errno = 0;
  char *named = entered == 0 ? getcwd(cwd, sizeof(cwd)) : NULL;
  int getcwd_error = named == NULL ? errno : 0;
  printf("stale fchdir=%d errno=%s getcwd=%s\n", entered, errno_name(enter_error),
         entered == 0 ? (named != NULL ? "named" : errno_name(getcwd_error)) : "-");
  printf("failures=%d\n", failures);
  return failures == 0 ? 0 : 1;
}

/* --exec-below: execveat of a name below the caller's own /proc/self/fd
 * descriptor: Linux follows /proc/<pid>/fd/<T> to /bin/true and runs it
 * (exit 0). Under Hermit the path below the placeholder is refused by name.
 * --unix-below: connect to an AF_UNIX path that goes on below
 * /proc/self/fd/<D>/../root/: Linux reaches the socket through the caller's
 * root (connected). Under Hermit it is refused by name. */
static int below_refusals(int exec) {
  int directory = open_directory("/proc/self/fd", O_RDONLY | O_DIRECTORY);
  if (exec) {
    int image = open("/bin/true", O_RDONLY);
    CHECK(image >= 0, "open /bin/true: %s", strerror(errno));
    char name[16];
    snprintf(name, sizeof(name), "%d", image);
    char *const arguments[] = {"true", NULL};
    char *const environment[] = {NULL};
    errno = 0;
    long result = syscall(SYS_execveat, directory, name, arguments, environment, 0);
    printf("exec below the descriptor directory=%ld errno=%s\n", result,
           errno_name(result < 0 ? errno : 0));
    return 1;
  }
  char socket_path[] = "/tmp/proc-fd-directory-sock-XXXXXX";
  int placeholder = mkstemp(socket_path);
  CHECK(placeholder >= 0, "mkstemp: %s", strerror(errno));
  close(placeholder);
  unlink(socket_path);
  int listener = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
  struct sockaddr_un address = {.sun_family = AF_UNIX};
  snprintf(address.sun_path, sizeof(address.sun_path), "%s", socket_path);
  CHECK(bind(listener, (struct sockaddr *)&address, sizeof(address)) == 0, "bind: %s",
        strerror(errno));
  CHECK(listen(listener, 1) == 0, "listen: %s", strerror(errno));
  int client = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
  struct sockaddr_un through = {.sun_family = AF_UNIX};
  snprintf(through.sun_path, sizeof(through.sun_path), "/proc/self/fd/%d/../root%s", directory,
           socket_path);
  errno = 0;
  int connected = connect(client, (struct sockaddr *)&through, sizeof(through));
  printf("connect below the descriptor directory=%d errno=%s\n", connected,
         errno_name(connected < 0 ? errno : 0));
  unlink(socket_path);
  printf("failures=%d\n", failures);
  return failures == 0 ? 0 : 1;
}

/* What an O_PATH descriptor on each directory gets from the calls Linux
 * refuses on one, those Hermit does not answer itself included (EBADF;
 * poll's POLLNVAL), and what a descriptor that is no O_PATH one gets from
 * syncfs (0). */
static void check_path_only_refusals(void) {
  const char *names[] = {"/proc", "/proc/self/fd"};
  for (int which = 0; which < 2; which++) {
    int path_only = open_directory(names[which], O_PATH | O_DIRECTORY);
    int readable = open_directory(names[which], O_RDONLY | O_DIRECTORY);
    errno = 0;
    int synced = syncfs(path_only);
    int sync_error = synced < 0 ? errno : 0;
    errno = 0;
    int synced_readable = syncfs(readable);
    int sync_readable_error = synced_readable < 0 ? errno : 0;
    int epoll = epoll_create1(EPOLL_CLOEXEC);
    struct epoll_event event = {.events = EPOLLIN};
    errno = 0;
    int added = epoll_ctl(epoll, EPOLL_CTL_ADD, path_only, &event);
    int add_error = added < 0 ? errno : 0;
    close(epoll);
    struct pollfd polled = {.fd = path_only, .events = POLLIN};
    int ready = poll(&polled, 1, 0);
    int type = 0;
    socklen_t length = sizeof(type);
    errno = 0;
    int got = getsockopt(path_only, SOL_SOCKET, SO_TYPE, &type, &length);
    int sockopt_error = got < 0 ? errno : 0;
    printf("O_PATH %s: syncfs=%s epoll_ctl=%s poll=%d revents=%#x getsockopt=%s; readable syncfs=%s\n",
           names[which], errno_name(sync_error), errno_name(add_error), ready,
           (unsigned)polled.revents, errno_name(sockopt_error), errno_name(sync_readable_error));
    close(readable);
    close(path_only);
  }
}

/* A /proc/self/fd descriptor received back over SCM_RIGHTS is the same open
 * file description as the one sent: a lock through either is that
 * description's one lock, and an unlock through the received copy releases
 * it, so that another open of the directory then locks it. A sequenced-packet
 * send that names a destination below the descriptor directory sends to its
 * peer, as Linux ignores the destination of a connected sequenced-packet
 * socket, and a connected stream socket's fails with EISCONN, without
 * looking the destination up (unix_stream_sendmsg). */
static void check_received_alias_lock_and_ignored_destination(void) {
  int pair[2];
  CHECK(socketpair(AF_UNIX, SOCK_STREAM, 0, pair) == 0, "socketpair: %s", strerror(errno));
  int directory = open_directory("/proc/self/fd", O_RDONLY | O_DIRECTORY);
  int alias = pass_to_self(pair, directory);
  errno = 0;
  int through_alias = flock(alias, LOCK_EX | LOCK_NB);
  int alias_error = through_alias < 0 ? errno : 0;
  errno = 0;
  int through_original = flock(directory, LOCK_EX | LOCK_NB);
  int original_error = through_original < 0 ? errno : 0;
  errno = 0;
  int unlocked = flock(alias, LOCK_UN);
  int other = open_directory("/proc/self/fd", O_RDONLY | O_DIRECTORY);
  errno = 0;
  int other_locked = flock(other, LOCK_EX | LOCK_NB);
  int other_error = other_locked < 0 ? errno : 0;
  printf("received alias: lock through it=%s through the original=%s unlock=%d, then another "
         "open locks=%s\n",
         errno_name(alias_error), errno_name(original_error), unlocked, errno_name(other_error));
  close(other);
  close(alias);
  close(pair[0]);
  close(pair[1]);
  int packets[2];
  errno = 0;
  if (socketpair(AF_UNIX, SOCK_SEQPACKET, 0, packets) != 0) {
    printf("sequenced-packet send to a destination below the descriptor directory: "
           "socketpair=%s\n",
           errno_name(errno));
  } else {
    struct sockaddr_un below = {.sun_family = AF_UNIX};
    snprintf(below.sun_path, sizeof(below.sun_path), "/proc/self/fd/%d/anything", directory);
    errno = 0;
    ssize_t sent = sendto(packets[0], "x", 1, 0, (struct sockaddr *)&below, sizeof(below));
    int send_error = sent < 0 ? errno : 0;
    char byte = 0;
    ssize_t got = recv(packets[1], &byte, 1, MSG_DONTWAIT);
    printf("sequenced-packet send to a destination below the descriptor directory: sent=%zd "
           "errno=%s received=%d\n",
           sent, errno_name(send_error), got == 1 && byte == 'x');
    close(packets[0]);
    close(packets[1]);
  }
  /* A connected stream socket looks no destination up: EISCONN. */
  int stream[2];
  CHECK(socketpair(AF_UNIX, SOCK_STREAM, 0, stream) == 0, "socketpair: %s", strerror(errno));
  struct sockaddr_un below = {.sun_family = AF_UNIX};
  snprintf(below.sun_path, sizeof(below.sun_path), "/proc/self/fd/%d/anything", directory);
  errno = 0;
  ssize_t streamed = sendto(stream[0], "x", 1, 0, (struct sockaddr *)&below, sizeof(below));
  printf("stream send to a destination below the descriptor directory: sent=%zd errno=%s\n",
         streamed, errno_name(streamed < 0 ? errno : 0));
  close(stream[0]);
  close(stream[1]);
  close(directory);
}

/* A name looked up through the descriptor directory's own link from a
 * descriptor on / (proc/self/fd/D/T) and from the working directory /:
 * the file descriptor T is open on, as Linux resolves it. */
static void check_relative_paths_through_links(void) {
  int directory = open_directory("/proc/self/fd", O_RDONLY | O_DIRECTORY);
  int target = open("/bin/true", O_RDONLY | O_CLOEXEC);
  CHECK(target >= 0, "open /bin/true: %s", strerror(errno));
  struct stat expected, from_descriptor, from_cwd;
  CHECK(fstat(target, &expected) == 0, "fstat: %s", strerror(errno));
  int slash = open("/", O_PATH | O_DIRECTORY | O_CLOEXEC);
  char relative[64];
  snprintf(relative, sizeof(relative), "proc/self/fd/%d/%d", directory, target);
  errno = 0;
  int by_descriptor = fstatat(slash, relative, &from_descriptor, 0);
  int descriptor_error = by_descriptor < 0 ? errno : 0;
  char cwd[256];
  CHECK(getcwd(cwd, sizeof(cwd)) != NULL, "getcwd: %s", strerror(errno));
  CHECK(chdir("/") == 0, "chdir /: %s", strerror(errno));
  errno = 0;
  int by_cwd = stat(relative, &from_cwd);
  int cwd_error = by_cwd < 0 ? errno : 0;
  CHECK(chdir(cwd) == 0, "chdir back: %s", strerror(errno));
  printf("names through the descriptor directory's link: from / by descriptor=%s same=%d, "
         "from the working directory /=%s same=%d\n",
         errno_name(descriptor_error),
         by_descriptor == 0 && from_descriptor.st_ino == expected.st_ino,
         errno_name(cwd_error), by_cwd == 0 && from_cwd.st_ino == expected.st_ino);
  close(slash);
  close(target);
  close(directory);
}

/* /proc/fs/nfsd, procfs's empty mount point, by spellings Hermit's --proc
 * level table does not resolve: through the caller's root link, through a
 * descriptor link to /, through a symbolic link the guest makes, and relative
 * to the working directory /. Linux finds procfs's own empty directory each
 * way (statfs: procfs). */
static void check_mount_point_by_other_spellings(void) {
  struct stat st;
  struct statx stx;
  struct statfs fs;
  errno = 0;
  int root_open = open("/proc/self/root/proc/fs/nfsd", O_RDONLY | O_DIRECTORY | O_CLOEXEC);
  int root_open_error = root_open < 0 ? errno : 0;
  if (root_open >= 0) {
    close(root_open);
  }
  errno = 0;
  int root_stat = stat("/proc/self/root/proc/fs/nfsd", &st);
  int root_stat_error = root_stat < 0 ? errno : 0;
  errno = 0;
  int root_statfs = statfs("/proc/self/root/proc/fs/nfsd", &fs);
  int root_statfs_error = root_statfs < 0 ? errno : 0;
  unsigned long root_type = root_statfs == 0 ? (unsigned long)fs.f_type : 0;
  int slash = open("/", O_PATH | O_DIRECTORY | O_CLOEXEC);
  char through[64];
  snprintf(through, sizeof(through), "/dev/fd/%d/proc/fs/nfsd", slash);
  errno = 0;
  int link_open = open(through, O_RDONLY | O_DIRECTORY | O_CLOEXEC);
  int link_open_error = link_open < 0 ? errno : 0;
  if (link_open >= 0) {
    close(link_open);
  }
  close(slash);
  char symlink_path[] = "/tmp/proc-fd-directory-link-XXXXXX";
  int made = mkstemp(symlink_path);
  CHECK(made >= 0, "mkstemp: %s", strerror(errno));
  close(made);
  unlink(symlink_path);
  CHECK(symlink("/proc/fs/nfsd", symlink_path) == 0, "symlink: %s", strerror(errno));
  errno = 0;
  int symlink_open = open(symlink_path, O_RDONLY | O_DIRECTORY | O_CLOEXEC);
  int symlink_open_error = symlink_open < 0 ? errno : 0;
  if (symlink_open >= 0) {
    close(symlink_open);
  }
  errno = 0;
  int symlink_stat = stat(symlink_path, &st);
  int symlink_stat_error = symlink_stat < 0 ? errno : 0;
  errno = 0;
  int symlink_statx = statx(AT_FDCWD, symlink_path, 0, STATX_BASIC_STATS, &stx);
  int symlink_statx_error = symlink_statx < 0 ? errno : 0;
  unlink(symlink_path);
  int previous = open(".", O_RDONLY | O_DIRECTORY | O_CLOEXEC);
  CHECK(previous >= 0 && chdir("/") == 0, "chdir /: %s", strerror(errno));
  errno = 0;
  int relative_open = open("proc/fs/nfsd", O_RDONLY | O_DIRECTORY | O_CLOEXEC);
  int relative_open_error = relative_open < 0 ? errno : 0;
  if (relative_open >= 0) {
    close(relative_open);
  }
  errno = 0;
  int relative_stat = stat("proc/fs/nfsd", &st);
  int relative_stat_error = relative_stat < 0 ? errno : 0;
  errno = 0;
  int relative_statx = statx(AT_FDCWD, "proc/fs/nfsd", 0, STATX_BASIC_STATS, &stx);
  int relative_statx_error = relative_statx < 0 ? errno : 0;
  CHECK(fchdir(previous) == 0, "fchdir back: %s", strerror(errno));
  close(previous);
  printf("/proc/fs/nfsd by other spellings: root link open=%s stat=%s statfs=%s type=%lx; "
         "descriptor link to / open=%s; symbolic link open=%s stat=%s statx=%s; "
         "from the working directory / open=%s stat=%s statx=%s\n",
         errno_name(root_open_error), errno_name(root_stat_error), errno_name(root_statfs_error),
         root_type, errno_name(link_open_error), errno_name(symlink_open_error),
         errno_name(symlink_stat_error), errno_name(symlink_statx_error),
         errno_name(relative_open_error), errno_name(relative_stat_error),
         errno_name(relative_statx_error));
}

/* The sibling of --send-rewritten-while-blocked: once the main thread waits
 * in its send for room, it rewrites the descriptor the send's control data
 * names (an ordinary one) with the locked /proc/self/fd descriptor, then
 * drains the peer so that the send completes. */
struct rewrite {
  int *slot;
  int directory;
  int peer;
};

static void *rewrite_while_blocked(void *argument) {
  struct rewrite *rewrite = argument;
  struct timespec pause = {0, 100 * 1000 * 1000};
  nanosleep(&pause, NULL);
  *rewrite->slot = rewrite->directory;
  char sink[65536];
  while (read(rewrite->peer, sink, sizeof(sink)) > 0) {
  }
  return NULL;
}

/* --send-locked: a /proc/self/fd descriptor whose open file holds a flock,
 * passed over SCM_RIGHTS (Linux keeps the lock while the message is
 * queued: sent). --lock-in-flight: a flock of a /proc/self/fd descriptor
 * while a copy of it is in flight (granted). --transfer-into-pipe and
 * --splice-into-pipe: sendfile and splice of 1 byte from /proc into a pipe
 * that has a reader and room (EINVAL: a directory has no splice_read).
 * --lock-after-private-table: a flock of a /proc descriptor in a process one
 * of whose threads had a descriptor table of its own (granted).
 * --send-unscanned-control: a /proc/self/fd descriptor passed over
 * SCM_RIGHTS after 65520 bytes of control data of another level, which
 * Linux skips (__scm_send), in a 65544-byte control buffer, within the
 * socket's net.core.optmem_max (sent). --send-rewritten-while-blocked: a
 * send that waits for room in a full socket while a sibling rewrites the
 * descriptor its control data names with a locked /proc/self/fd descriptor
 * (sent: Linux copied the control data when the call began).
 * --send-holding-the-directory: a send on a connected AF_UNIX socket from
 * socket(2) while the process holds a /proc/self/fd descriptor (sent; in
 * Hermit's turn, not refused). --send-on-stdin-holding-the-directory: the
 * same send on the socket the program inherited as its standard input,
 * which blocks in the kernel, so that Hermit runs the send outside its turn
 * (sent). Under Hermit each is refused by name but the last but one. */
static int lifetime_refusals(const char *mode) {
  int pair[2];
  CHECK(socketpair(AF_UNIX, SOCK_STREAM, 0, pair) == 0, "socketpair: %s", strerror(errno));
  if (strcmp(mode, "--lock-after-private-table") == 0) {
    int null = open("/dev/null", O_RDONLY);
    CHECK(null >= 0, "open /dev/null: %s", strerror(errno));
    check_private_table(null);
    int root = open_directory("/proc", O_RDONLY | O_DIRECTORY);
    errno = 0;
    int locked = flock(root, LOCK_EX | LOCK_NB);
    printf("lock after a thread with a table of its own=%d errno=%s\n", locked,
           errno_name(locked < 0 ? errno : 0));
  } else if (strcmp(mode, "--transfer-into-pipe") == 0) {
    int root = open_directory("/proc", O_RDONLY | O_DIRECTORY);
    int pipes[2];
    CHECK(pipe(pipes) == 0, "pipe: %s", strerror(errno));
    errno = 0;
    ssize_t moved = sendfile(pipes[1], root, NULL, 1);
    printf("sendfile from /proc into a pipe=%zd errno=%s\n", moved,
           errno_name(moved < 0 ? errno : 0));
  } else if (strcmp(mode, "--splice-into-pipe") == 0) {
    int root = open_directory("/proc", O_RDONLY | O_DIRECTORY);
    int pipes[2];
    CHECK(pipe(pipes) == 0, "pipe: %s", strerror(errno));
    errno = 0;
    ssize_t moved = splice(root, NULL, pipes[1], NULL, 1, 0);
    printf("splice from /proc into a pipe=%zd errno=%s\n", moved,
           errno_name(moved < 0 ? errno : 0));
  } else if (strcmp(mode, "--send-holding-the-directory") == 0) {
    int directory = open_directory("/proc/self/fd", O_RDONLY | O_DIRECTORY);
    (void)directory;
    int listener = socket(AF_UNIX, SOCK_STREAM, 0);
    CHECK(listener >= 0, "socket: %s", strerror(errno));
    struct sockaddr_un address = {.sun_family = AF_UNIX};
    snprintf(address.sun_path + 1, sizeof(address.sun_path) - 1, "proc-fd-directory-%d",
             (int)getpid());
    socklen_t length =
        (socklen_t)(offsetof(struct sockaddr_un, sun_path) + 1 + strlen(address.sun_path + 1));
    CHECK(bind(listener, (struct sockaddr *)&address, length) == 0, "bind: %s", strerror(errno));
    CHECK(listen(listener, 1) == 0, "listen: %s", strerror(errno));
    int client = socket(AF_UNIX, SOCK_STREAM, 0);
    CHECK(client >= 0, "socket: %s", strerror(errno));
    CHECK(connect(client, (struct sockaddr *)&address, length) == 0, "connect: %s",
          strerror(errno));
    int server = accept(listener, NULL, NULL);
    CHECK(server >= 0, "accept: %s", strerror(errno));
    char byte = 'o';
    struct iovec vector = {.iov_base = &byte, .iov_len = 1};
    struct msghdr message = {.msg_iov = &vector, .msg_iovlen = 1};
    errno = 0;
    ssize_t sent = sendmsg(client, &message, 0);
    printf("send on a connected socket while holding the descriptor directory=%zd errno=%s\n",
           sent, errno_name(sent < 0 ? errno : 0));
  } else if (strcmp(mode, "--send-on-stdin-holding-the-directory") == 0) {
    int directory = open_directory("/proc/self/fd", O_RDONLY | O_DIRECTORY);
    (void)directory;
    char byte = 'i';
    struct iovec vector = {.iov_base = &byte, .iov_len = 1};
    struct msghdr message = {.msg_iov = &vector, .msg_iovlen = 1};
    errno = 0;
    ssize_t sent = sendmsg(0, &message, 0);
    printf("send on the inherited socket while holding the descriptor directory=%zd errno=%s\n",
           sent, errno_name(sent < 0 ? errno : 0));
  } else if (strcmp(mode, "--send-unscanned-control") == 0) {
    int directory = open_directory("/proc/self/fd", O_RDONLY | O_DIRECTORY);
    enum { CONTROL = 65544 };
    static char control[CONTROL] __attribute__((aligned(8)));
    memset(control, 0, sizeof(control));
    size_t rights = CMSG_SPACE(sizeof(int));
    struct cmsghdr *other = (struct cmsghdr *)control;
    other->cmsg_len = CONTROL - rights;
    other->cmsg_level = IPPROTO_IP;
    other->cmsg_type = 0;
    struct cmsghdr *header = (struct cmsghdr *)(control + CONTROL - rights);
    header->cmsg_level = SOL_SOCKET;
    header->cmsg_type = SCM_RIGHTS;
    header->cmsg_len = CMSG_LEN(sizeof(int));
    memcpy(CMSG_DATA(header), &directory, sizeof(int));
    char byte = 'u';
    struct iovec vector = {.iov_base = &byte, .iov_len = 1};
    struct msghdr message = {.msg_iov = &vector,
                             .msg_iovlen = 1,
                             .msg_control = control,
                             .msg_controllen = CONTROL};
    errno = 0;
    ssize_t sent = sendmsg(pair[0], &message, 0);
    printf("send of the descriptor directory after 65520 bytes of other control data=%zd "
           "errno=%s\n",
           sent, errno_name(sent < 0 ? errno : 0));
  } else if (strcmp(mode, "--send-rewritten-while-blocked") == 0) {
    int directory = open_directory("/proc/self/fd", O_RDONLY | O_DIRECTORY);
    CHECK(flock(directory, LOCK_EX | LOCK_NB) == 0, "flock: %s", strerror(errno));
    int null = open("/dev/null", O_RDONLY);
    CHECK(null >= 0, "open /dev/null: %s", strerror(errno));
    /* Fill the socket, nonblocking, then wait in a blocking send. */
    int flags = fcntl(pair[0], F_GETFL);
    CHECK(fcntl(pair[0], F_SETFL, flags | O_NONBLOCK) == 0, "F_SETFL: %s", strerror(errno));
    static char filler[65536];
    while (write(pair[0], filler, sizeof(filler)) > 0) {
    }
    CHECK(fcntl(pair[0], F_SETFL, flags) == 0, "F_SETFL: %s", strerror(errno));
    int peer_flags = fcntl(pair[1], F_GETFL);
    CHECK(fcntl(pair[1], F_SETFL, peer_flags | O_NONBLOCK) == 0, "F_SETFL: %s", strerror(errno));
    char byte = 'r';
    struct iovec vector = {.iov_base = &byte, .iov_len = 1};
    static char control[CMSG_SPACE(sizeof(int))] __attribute__((aligned(8)));
    memset(control, 0, sizeof(control));
    struct msghdr message = {.msg_iov = &vector,
                             .msg_iovlen = 1,
                             .msg_control = control,
                             .msg_controllen = sizeof(control)};
    struct cmsghdr *header = CMSG_FIRSTHDR(&message);
    header->cmsg_level = SOL_SOCKET;
    header->cmsg_type = SCM_RIGHTS;
    header->cmsg_len = CMSG_LEN(sizeof(int));
    memcpy(CMSG_DATA(header), &null, sizeof(int));
    struct rewrite rewrite = {(int *)CMSG_DATA(header), directory, pair[1]};
    pthread_t sibling;
    CHECK(pthread_create(&sibling, NULL, rewrite_while_blocked, &rewrite) == 0, "pthread_create");
    errno = 0;
    ssize_t sent = sendmsg(pair[0], &message, 0);
    int send_error = sent < 0 ? errno : 0;
    pthread_join(sibling, NULL);
    printf("send rewritten while it waits for room=%zd errno=%s\n", sent,
           errno_name(send_error));
  } else {
    int directory = open_directory("/proc/self/fd", O_RDONLY | O_DIRECTORY);
    int locked_first = strcmp(mode, "--send-locked") == 0;
    if (locked_first) {
      CHECK(flock(directory, LOCK_EX | LOCK_NB) == 0, "flock: %s", strerror(errno));
    }
    char byte = 'd';
    struct iovec vector = {.iov_base = &byte, .iov_len = 1};
    char control[CMSG_SPACE(sizeof(int))];
    memset(control, 0, sizeof(control));
    struct msghdr message = {.msg_iov = &vector,
                             .msg_iovlen = 1,
                             .msg_control = control,
                             .msg_controllen = sizeof(control)};
    struct cmsghdr *header = CMSG_FIRSTHDR(&message);
    header->cmsg_level = SOL_SOCKET;
    header->cmsg_type = SCM_RIGHTS;
    header->cmsg_len = CMSG_LEN(sizeof(int));
    memcpy(CMSG_DATA(header), &directory, sizeof(int));
    errno = 0;
    ssize_t sent = sendmsg(pair[0], &message, 0);
    printf("send of the descriptor directory%s=%zd errno=%s\n",
           locked_first ? " holding a lock" : "", sent, errno_name(sent < 0 ? errno : 0));
    if (!locked_first) {
      errno = 0;
      int locked = flock(directory, LOCK_EX | LOCK_NB);
      printf("lock while its copy is in flight=%d errno=%s\n", locked,
             errno_name(locked < 0 ? errno : 0));
    }
  }
  printf("failures=%d\n", failures);
  return failures == 0 ? 0 : 1;
}

int main(int argc, char **argv) {
  if (argc == 7 && strcmp(argv[1], "--after-exec") == 0) {
    return after_exec(atoi(argv[2]), atoi(argv[3]), atoi(argv[4]), atoi(argv[5]), atoi(argv[6]));
  }
  /* --chroot-only: the one check of a process that changes its root, apart
   * from the rest, whose verify runs it cannot join (Hermit names no procfs
   * inode of a process whose root is not the container's, a determinism
   * loss --verify refuses). */
  if (argc == 2 && strcmp(argv[1], "--stale-fchdir") == 0) {
    return stale_fchdir();
  }
  if (argc == 2 && (strcmp(argv[1], "--exec-below") == 0 || strcmp(argv[1], "--unix-below") == 0)) {
    return below_refusals(strcmp(argv[1], "--exec-below") == 0);
  }
  if (argc == 2 &&
      (strcmp(argv[1], "--send-locked") == 0 || strcmp(argv[1], "--lock-in-flight") == 0 ||
       strcmp(argv[1], "--transfer-into-pipe") == 0 ||
       strcmp(argv[1], "--splice-into-pipe") == 0 ||
       strcmp(argv[1], "--lock-after-private-table") == 0 ||
       strcmp(argv[1], "--send-unscanned-control") == 0 ||
       strcmp(argv[1], "--send-rewritten-while-blocked") == 0 ||
       strcmp(argv[1], "--send-holding-the-directory") == 0 ||
       strcmp(argv[1], "--send-on-stdin-holding-the-directory") == 0)) {
    return lifetime_refusals(argv[1]);
  }
  if (argc == 2 && strcmp(argv[1], "--chroot-only") == 0) {
    int root = open_directory("/proc", O_RDONLY | O_DIRECTORY);
    int directory = open_directory("/proc/self/fd", O_RDONLY | O_DIRECTORY);
    check_kept_after_chroot(root, directory);
    printf("failures=%d\n", failures);
    return failures == 0 ? 0 : 1;
  }
  int no_exec = 0, private_tables = 1, fifo_threads = 1;
  for (int arg = 1; arg < argc; arg++) {
    if (strcmp(argv[arg], "--no-exec") == 0) {
      no_exec = 1;
    } else if (strcmp(argv[arg], "--no-private-tables") == 0) {
      private_tables = 0;
    } else if (strcmp(argv[arg], "--no-fifo-threads") == 0) {
      fifo_threads = 0;
    }
  }
  char names[1024];
  uint64_t dot_ino = 0;

  /* Before any descriptor directory is open, a /proc descriptor seeked to
   * 0x40000000, which Linux accepts (any position up to 2^31-1). */
  {
    int early = open_directory("/proc", O_RDONLY | O_DIRECTORY);
    errno = 0;
    off_t moved = lseek(early, 0x40000000, SEEK_SET);
    printf("forgery before any descriptor directory: /proc lseek 0x40000000=%lld errno=%s\n",
           (long long)moved, errno_name(moved < 0 ? errno : 0));
    close(early);
  }

  /* The directory, its metadata and a whole listing. */
  int directory = open_directory("/proc/self/fd", O_RDONLY | O_DIRECTORY | O_CLOEXEC);
  struct stat by_fd, by_path;
  CHECK(fstat(directory, &by_fd) == 0, "fstat: %s", strerror(errno));
  printf("fstat: dir=%d mode=%o nlink=%lu blksize=%ld size=%lld uid=%u\n",
         S_ISDIR(by_fd.st_mode), by_fd.st_mode & 07777, (unsigned long)by_fd.st_nlink,
         (long)by_fd.st_blksize, (long long)by_fd.st_size, by_fd.st_uid);
  struct statx extended;
  CHECK(statx(directory, "", AT_EMPTY_PATH, STATX_BASIC_STATS, &extended) == 0, "statx: %s",
        strerror(errno));
  printf("statx: dir=%d mode=%o nlink=%u same-ino=%d\n", S_ISDIR(extended.stx_mode),
         extended.stx_mode & 07777, extended.stx_nlink, extended.stx_ino == by_fd.st_ino);
  CHECK(stat("/proc/self/fd", &by_path) == 0, "stat: %s", strerror(errno));
  printf("stat path: same-ino=%d\n", by_path.st_ino == by_fd.st_ino);
  int calls = list(directory, 4096, 1, names, sizeof(names), &dot_ino);
  printf("listing:%s\n", names);
  printf("listing calls=%d dot-ino-is-fstat-ino=%d end=%lld\n", calls, dot_ino == by_fd.st_ino,
         (long long)lseek(directory, 0, SEEK_CUR));

  /* One record per call, and buffers too small for one. */
  printf("rewind=%lld\n", (long long)lseek(directory, 0, SEEK_SET));
  calls = list(directory, 24, 1, names, sizeof(names), NULL);
  printf("paged:%s\npaged calls=%d\n", names, calls);
  lseek(directory, 0, SEEK_SET);
  char small[64];
  errno = 0;
  long n = syscall(SYS_getdents64, directory, small, 10);
  printf("getdents64 10 bytes=%ld errno=%s position=%lld\n", n, errno_name(n < 0 ? errno : 0),
         (long long)lseek(directory, 0, SEEK_CUR));
  errno = 0;
  n = syscall(SYS_getdents64, directory, small, 0);
  printf("getdents64 0 bytes=%ld errno=%s\n", n, errno_name(n < 0 ? errno : 0));

  /* lseek. */
  print_seek("end+0", directory, 0, SEEK_END);
  print_seek("end+5", directory, 5, SEEK_END);
  print_seek("data", directory, 0, SEEK_DATA);
  print_seek("hole", directory, 0, SEEK_HOLE);
  print_seek("set-1", directory, -1, SEEK_SET);
  print_seek("set-2^31", directory, 1LL << 31, SEEK_SET);
  print_seek("whence-7", directory, 0, 7);
  print_seek("set-max", directory, (1LL << 31) - 1, SEEK_SET);
  calls = list(directory, 4096, 1, names, sizeof(names), NULL);
  printf("past the end:%s calls=%d position=%lld\n", names, calls,
         (long long)lseek(directory, 0, SEEK_CUR));
  print_seek("set-4", directory, 4, SEEK_SET);
  list(directory, 4096, 1, names, sizeof(names), NULL);
  printf("from position 4:%s\n", names);

  /* The table as it is at each call: one descriptor opened past the
   * position, one closed before it is reached. */
  lseek(directory, 0, SEEK_SET);
  char first[256];
  n = syscall(SYS_getdents64, directory, first, 72);
  printf("live: first call=%ld\n", n);
  int opened = open("/dev/null", O_RDONLY);
  CHECK(dup2(opened, 20) == 20, "dup2: %s", strerror(errno));
  close(opened);
  list(directory, 4096, 1, names, sizeof(names), NULL);
  printf("live: rest:%s\n", names);
  close(20);

  /* A dup shares the position. */
  int duplicate = dup(directory);
  lseek(directory, 0, SEEK_SET);
  n = syscall(SYS_getdents64, directory, first, 48);
  list(duplicate, 24, 1, names, sizeof(names), NULL);
  printf("dup: after 2 entries through the original:%s\n", names);
  close(duplicate);

  /* Names below the directory's descriptor. */
  int null = open("/dev/null", O_RDONLY);
  char name[16], link[128];
  snprintf(name, sizeof(name), "%d", null);
  ssize_t length = readlinkat(directory, name, link, sizeof(link) - 1);
  link[length < 0 ? 0 : length] = '\0';
  printf("below: readlinkat N=%s\n", link);
  struct stat below;
  int rc = fstatat(directory, name, &below, 0);
  printf("below: fstatat N rc=%d chr=%d\n", rc, rc == 0 && S_ISCHR(below.st_mode));
  /* Only the type: the link's mode is the backend's (KVM's link is 0777
   * where Linux's is 0500), which this slice does not decide. */
  rc = fstatat(directory, name, &below, AT_SYMLINK_NOFOLLOW);
  printf("below: fstatat N nofollow rc=%d lnk=%d\n", rc, rc == 0 && S_ISLNK(below.st_mode));
  printf("below: faccessat N R_OK=%d\n", faccessat(directory, name, R_OK, 0));
  int reopened = openat(directory, name, O_RDONLY);
  char byte;
  printf("below: openat N read=%zd\n", reopened >= 0 ? read(reopened, &byte, 1) : -1);
  if (reopened >= 0) {
    close(reopened);
  }
  errno = 0;
  length = readlinkat(directory, "999", link, sizeof(link));
  printf("below: readlinkat 999=%zd errno=%s\n", length, errno_name(length < 0 ? errno : 0));
  int dot = openat(directory, ".", O_RDONLY | O_DIRECTORY);
  list(dot, 4096, 0, names, sizeof(names), NULL);
  printf("below: openat . listing:%s\n", names);
  close(dot);

  /* The descriptor's own link names the directory. */
  char own_link[64], expected[64];
  snprintf(own_link, sizeof(own_link), "/proc/self/fd/%d", directory);
  snprintf(expected, sizeof(expected), "/proc/%d/fd", (int)getpid());
  length = readlink(own_link, link, sizeof(link) - 1);
  link[length < 0 ? 0 : length] = '\0';
  printf("own link names /proc/<pid>/fd=%d\n", strcmp(link, expected) == 0);

  /* O_PATH. */
  int path_only = open_directory("/proc/self/fd", O_PATH | O_DIRECTORY);
  errno = 0;
  n = syscall(SYS_getdents64, path_only, small, sizeof(small));
  printf("O_PATH: getdents64=%ld errno=%s\n", n, errno_name(n < 0 ? errno : 0));
  errno = 0;
  n = read(path_only, small, 1);
  printf("O_PATH: read=%ld errno=%s\n", n, errno_name(n < 0 ? errno : 0));
  print_seek("O_PATH", path_only, 0, SEEK_SET);
  struct stat path_stat;
  CHECK(fstat(path_only, &path_stat) == 0, "fstat O_PATH: %s", strerror(errno));
  printf("O_PATH: fstat dir=%d mode=%o F_GETFL=%o\n", S_ISDIR(path_stat.st_mode),
         path_stat.st_mode & 07777, fcntl(path_only, F_GETFL));
  length = readlinkat(path_only, name, link, sizeof(link) - 1);
  link[length < 0 ? 0 : length] = '\0';
  printf("O_PATH: readlinkat N=%s\n", link);
  close(path_only);

  /* fchdir enters the directory, as Linux does: getcwd then names
   * /proc/<pid>/fd. (KVM's executor refuses a working directory on /proc
   * with EACCES.) */
  errno = 0;
  rc = fchdir(directory);
  printf("fchdir=%d errno=%s\n", rc, errno_name(rc < 0 ? errno : 0));
  if (rc == 0) {
    char cwd[128], expected[64];
    snprintf(expected, sizeof(expected), "/proc/%d/fd", (int)getpid());
    printf("fchdir: cwd is /proc/<pid>/fd=%d\n",
           getcwd(cwd, sizeof(cwd)) != NULL && strcmp(cwd, expected) == 0);
    CHECK(chdir("/") == 0, "chdir: %s", strerror(errno));
  }

  /* The thread spellings, from the main thread and another. */
  char path[96];
  int thread_fd = open_directory("/proc/thread-self/fd", O_RDONLY | O_DIRECTORY);
  list(thread_fd, sizeof(names), 0, names, sizeof(names), NULL);
  printf("main: thread-self/fd:%s\n", names);
  close(thread_fd);
  snprintf(path, sizeof(path), "/proc/%d/task/%ld/fd", (int)getpid(), (long)syscall(SYS_gettid));
  thread_fd = open_directory(path, O_RDONLY | O_DIRECTORY);
  list(thread_fd, sizeof(names), 0, names, sizeof(names), NULL);
  printf("main: <pid>/task/<tid>/fd:%s\n", names);
  close(thread_fd);
  snprintf(path, sizeof(path), "/proc/%d/fd", (int)getpid());
  thread_fd = open_directory(path, O_RDONLY | O_DIRECTORY);
  list(thread_fd, sizeof(names), 0, names, sizeof(names), NULL);
  printf("main: <pid>/fd:%s\n", names);
  close(thread_fd);
  /* /dev/fd, Linux's symbolic link to /proc/self/fd. */
  thread_fd = open_directory("/dev/fd", O_RDONLY | O_DIRECTORY);
  list(thread_fd, sizeof(names), 0, names, sizeof(names), NULL);
  printf("main: /dev/fd:%s\n", names);
  close(thread_fd);
  fflush(stdout);
  pthread_t thread;
  CHECK(pthread_create(&thread, NULL, thread_spellings, NULL) == 0, "pthread_create");
  pthread_join(thread, NULL);

  /* SCM_RIGHTS: a received descriptor is listed, and a received descriptor
   * directory lists the table and looks names up below it. */
  int pair[2];
  CHECK(socketpair(AF_UNIX, SOCK_STREAM, 0, pair) == 0, "socketpair: %s", strerror(errno));
  int received_null = pass_to_self(pair, null);
  int received_directory = pass_to_self(pair, directory);
  printf("received: null=%d directory=%d\n", received_null, received_directory);
  lseek(directory, 0, SEEK_SET);
  list(directory, 4096, 0, names, sizeof(names), NULL);
  printf("received: listing:%s\n", names);
  /* Linux shares the position with the original (at its end now); Detcore
   * models a received descriptor as a description of its own
   * (https://github.com/rrnewton/hermit/issues/4670), so both rewind. */
  lseek(received_directory, 0, SEEK_SET);
  list(received_directory, 4096, 0, names, sizeof(names), NULL);
  printf("received: through the received directory:%s\n", names);
  length = readlinkat(received_directory, name, link, sizeof(link) - 1);
  link[length < 0 ? 0 : length] = '\0';
  printf("received: readlinkat N below it=%s\n", link);
  close(received_directory);
  close(received_null);
  close(pair[0]);
  close(pair[1]);

  /* A fork's child lists its own table. */
  fflush(stdout);
  pid_t child = fork();
  if (child == 0) {
    close(null);
    int own = open_directory("/proc/self/fd", O_RDONLY | O_DIRECTORY);
    list(own, 4096, 0, names, sizeof(names), NULL);
    printf("child: own listing:%s\n", names);
    close(own);
    /* The directory it inherited is the parent's, which still holds the
     * descriptor this child closed, whose number the descriptor opened to
     * read the parent's table takes in this one. */
    lseek(directory, 0, SEEK_SET);
    list(directory, 4096, 0, names, sizeof(names), NULL);
    char parent_null[16];
    snprintf(parent_null, sizeof(parent_null), " %d", null);
    printf("child: the inherited directory lists the parent's descriptor %d=%d\n", null,
           strstr(names, parent_null) != NULL);
    fflush(stdout);
    _exit(failures == 0 ? 0 : 1);
  }
  printf("child exited=%d\n", reap(child, "the listing child"));

  /* CPython's close-all loop, in a child holding descriptors up to 70. */
  fflush(stdout);
  child = fork();
  if (child == 0) {
    int targets[] = {10, 11, 30, 64, 70};
    for (unsigned i = 0; i < sizeof(targets) / sizeof(targets[0]); i++) {
      dup2(null, targets[i]);
    }
    int listing = open_directory("/proc/self/fd", O_RDONLY | O_DIRECTORY);
    list(listing, 4096, 1, names, sizeof(names), NULL);
    printf("close-all: before:%s\n", names);
    close(listing);
    int closed = cpython_close_all();
    listing = open_directory("/proc/self/fd", O_RDONLY | O_DIRECTORY);
    list(listing, 4096, 0, names, sizeof(names), NULL);
    printf("close-all: closed=%d after:%s\n", closed, names);
    fflush(stdout);
    _exit(failures == 0 ? 0 : 1);
  }
  printf("close-all child exited=%d\n", reap(child, "the close-all child"));

  /* The end follows the table's allocation: dup2(0, 100) grows it to 128
   * slots, which a close keeps; a fork's copy is sized from what is open. */
  fflush(stdout);
  child = fork();
  if (child == 0) {
    dup2(0, 100);
    close(100);
    int grown = open_directory("/proc/self/fd", O_RDONLY | O_DIRECTORY);
    list(grown, 4096, 1, names, sizeof(names), NULL);
    printf("capacity: after dup2(0, 100) and close(100):%s\n", names);
    close(grown);
    fflush(stdout);
    pid_t grandchild = fork();
    if (grandchild == 0) {
      grown = open_directory("/proc/self/fd", O_RDONLY | O_DIRECTORY);
      list(grown, 4096, 1, names, sizeof(names), NULL);
      printf("capacity: in a fork's child:%s\n", names);
      fflush(stdout);
      _exit(failures == 0 ? 0 : 1);
    }
    reap(grandchild, "the capacity grandchild");
    _exit(failures == 0 ? 0 : 1);
  }
  reap(child, "the capacity child");

  /* A dup2 whose old descriptor is not open grows the table for its target
   * first (ksys_dup3), then fails with EBADF. */
  fflush(stdout);
  child = fork();
  if (child == 0) {
    errno = 0;
    int rc = dup2(-1, 100);
    int error = rc < 0 ? errno : 0;
    int grown = open_directory("/proc/self/fd", O_RDONLY | O_DIRECTORY);
    list(grown, 4096, 1, names, sizeof(names), NULL);
    printf("capacity: after a failed dup2(-1, 100)=%d errno=%s:%s\n", rc, errno_name(error), names);
    close(grown);
    fflush(stdout);
    _exit(failures == 0 ? 0 : 1);
  }
  reap(child, "the failed-dup2 child");

  /* The close-all loop closes a received eventfd, which Detcore lists
   * though it does not model it. */
  fflush(stdout);
  child = fork();
  if (child == 0) {
    int pair[2];
    CHECK(socketpair(AF_UNIX, SOCK_STREAM, 0, pair) == 0, "socketpair: %s", strerror(errno));
    int event = eventfd(0, 0);
    int received_event = pass_to_self(pair, event);
    close(event);
    int listing = open_directory("/proc/self/fd", O_RDONLY | O_DIRECTORY);
    list(listing, 4096, 0, names, sizeof(names), NULL);
    char received_name[16];
    snprintf(received_name, sizeof(received_name), " %d", received_event);
    printf("close-all eventfd: received=%d listed=%d\n", received_event,
           strstr(names, received_name) != NULL);
    close(listing);
    int closed = cpython_close_all();
    errno = 0;
    int again = close(received_event);
    printf("close-all eventfd: closed=%d close of the received eventfd after=%d errno=%s\n",
           closed, again, errno_name(again < 0 ? errno : 0));
    fflush(stdout);
    _exit(failures == 0 ? 0 : 1);
  }
  reap(child, "the close-all eventfd child");

  /* A position in the tags' range does not make a directory a descriptor
   * directory: an ordinary /tmp directory seeked there, and /proc (which
   * Hermit refuses to seek there), each passed over SCM_RIGHTS. */
  {
    int pair[2];
    CHECK(socketpair(AF_UNIX, SOCK_STREAM, 0, pair) == 0, "socketpair: %s", strerror(errno));
    char scratch[] = "/tmp/proc-fd-directory-XXXXXX";
    CHECK(mkdtemp(scratch) != NULL, "mkdtemp: %s", strerror(errno));
    int plain = open_directory(scratch, O_RDONLY | O_DIRECTORY);
    errno = 0;
    off_t moved = lseek(plain, 0x40000000, SEEK_SET);
    printf("forgery: /tmp directory lseek 0x40000000=%lld errno=%s\n", (long long)moved,
           errno_name(moved < 0 ? errno : 0));
    int received_plain = pass_to_self(pair, plain);
    errno = 0;
    char link[128];
    ssize_t length = readlinkat(received_plain, "1", link, sizeof(link));
    printf("forgery: below the received /tmp directory, readlinkat 1=%zd errno=%s\n", length,
           errno_name(length < 0 ? errno : 0));
    close(received_plain);
    close(plain);
    rmdir(scratch);
    int proc = open_directory("/proc", O_RDONLY | O_DIRECTORY);
    errno = 0;
    moved = lseek(proc, 0x40000000, SEEK_SET);
    printf("forgery: /proc lseek 0x40000000=%lld errno=%s\n", (long long)moved,
           errno_name(moved < 0 ? errno : 0));
    int received_proc = pass_to_self(pair, proc);
    length = readlinkat(received_proc, "self", link, sizeof(link));
    printf("forgery: below the received /proc, self is a link=%d\n", length > 0);
    close(received_proc);
    close(proc);
    close(pair[0]);
    close(pair[1]);
  }

  /* A thread with a descriptor table of its own (Linux reads a process's
   * links in the leader's table), in a child, so that this process's later
   * locks stay in a process whose tables Hermit can tell apart; KVM gives no
   * thread a table of its own. */
  if (private_tables) {
    pid_t child = fork();
    if (child == 0) {
      check_private_table(null);
      _exit(failures == 0 ? 0 : 1);
    }
    reap(child, "the private-table child");
  } else {
    fprintf(stderr, "SKIPPED: a thread with a descriptor table of its own (--no-private-tables)\n");
  }

  /* A FIFO opened through the directory, its writer another thread (KVM's
   * executor holds the turn in a blocking FIFO open, with or without the
   * directory, so it skips this; checked, not printed). */
  if (!fifo_threads) {
    fprintf(stderr, "SKIPPED: a FIFO whose writer is another thread (--no-fifo-threads)\n");
  } else {
    char fifo[] = "/tmp/proc-fd-directory-fifo-XXXXXX";
    int made = mkstemp(fifo);
    close(made);
    unlink(fifo);
    CHECK(mkfifo(fifo, 0600) == 0, "mkfifo: %s", strerror(errno));
    fifo_path = fifo;
    int named = open(fifo, O_PATH);
    char fifo_name[16];
    snprintf(fifo_name, sizeof(fifo_name), "%d", named);
    pthread_t writer;
    CHECK(pthread_create(&writer, NULL, fifo_writer, NULL) == 0, "pthread_create");
    int reader = openat(directory, fifo_name, O_RDONLY);
    char text[8] = {0};
    ssize_t got = reader >= 0 ? read(reader, text, sizeof(text) - 1) : -1;
    CHECK(got == 4 && strcmp(text, "fifo") == 0,
          "a FIFO opened through the directory read %zd bytes: %s", got, text);
    pthread_join(writer, NULL);
    if (reader >= 0) {
      close(reader);
    }
    close(named);
    unlink(fifo);
  }

  /* Another thread reads the main thread's /proc/thread-self/fd, whose table
   * it shares. */
  {
    char shared[512];
    main_thread_directory = open_directory("/proc/thread-self/fd", O_RDONLY | O_DIRECTORY);
    pthread_t reader;
    CHECK(pthread_create(&reader, NULL, shared_table_reader, shared) == 0, "pthread_create");
    pthread_join(reader, NULL);
    printf("shared table: the main thread's directory read by another thread:%s\n", shared);
    close(main_thread_directory);
  }

  /* The directory's own link, followed and not. */
  {
    char own[64];
    snprintf(own, sizeof(own), "/proc/self/fd/%d", directory);
    struct stat followed, link_itself, by_descriptor;
    fstat(directory, &by_descriptor);
    int rc = stat(own, &followed);
    printf("own link followed: rc=%d dir=%d mode=%o same-ino=%d\n", rc, S_ISDIR(followed.st_mode),
           followed.st_mode & 07777, followed.st_ino == by_descriptor.st_ino);
    rc = lstat(own, &link_itself);
    printf("own link not followed: rc=%d lnk=%d\n", rc, S_ISLNK(link_itself.st_mode));
    char own_name[16];
    snprintf(own_name, sizeof(own_name), "%d", directory);
    rc = fstatat(directory, own_name, &followed, 0);
    printf("own link below the directory followed: rc=%d dir=%d same-ino=%d\n", rc,
           S_ISDIR(followed.st_mode), followed.st_ino == by_descriptor.st_ino);
    /* Through the link to the directory, and on below it. */
    char through[96], link[128];
    snprintf(through, sizeof(through), "/proc/self/fd/%d/%d", directory, null);
    ssize_t length = readlink(through, link, sizeof(link) - 1);
    link[length < 0 ? 0 : length] = '\0';
    printf("through the directory's link: readlink /proc/self/fd/D/N=%s\n", link);
  }

  /* Status flags and the O_PATH refusals. */
  {
    int nofollow = open_directory("/proc/self/fd", O_RDONLY | O_DIRECTORY | O_NOFOLLOW);
    printf("flags: O_NOFOLLOW open F_GETFL=%o\n", fcntl(nofollow, F_GETFL));
    print_errno("flags: F_SETFL O_NONBLOCK", fcntl(nofollow, F_SETFL, O_NONBLOCK));
    printf("flags: after F_SETFL F_GETFL=%o\n", fcntl(nofollow, F_GETFL));
    close(nofollow);
    errno = 0;
    int direct = open("/proc/self/fd", O_RDONLY | O_DIRECTORY | O_DIRECT);
    print_errno("flags: O_DIRECT open", direct);
    if (direct >= 0) {
      close(direct);
    }
    int path_only = open_directory("/proc/self/fd", O_PATH | O_DIRECTORY);
    errno = 0;
    print_errno("O_PATH: F_SETFL", fcntl(path_only, F_SETFL, 0));
    errno = 0;
    print_errno("O_PATH: fchmod", fchmod(path_only, 0500));
    errno = 0;
    print_errno("O_PATH: fsync", fsync(path_only));
    errno = 0;
    print_errno("O_PATH: flock", flock(path_only, LOCK_SH));
    int pending = 0;
    errno = 0;
    print_errno("O_PATH: ioctl FIONREAD", ioctl(path_only, FIONREAD, &pending));
    errno = 0;
    char value[8];
    print_errno("O_PATH: fgetxattr", fgetxattr(path_only, "user.x", value, sizeof(value)));
    errno = 0;
    void *mapped = mmap(NULL, 4096, PROT_READ, MAP_PRIVATE, path_only, 0);
    print_errno("O_PATH: mmap", mapped == MAP_FAILED ? -1 : 0);
    /* An anonymous mapping ignores its descriptor argument. */
    errno = 0;
    void *anonymous = mmap(NULL, 4096, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS, path_only, 0);
    print_errno("O_PATH: anonymous mmap naming it", anonymous == MAP_FAILED ? -1 : 0);
    if (anonymous != MAP_FAILED) {
      munmap(anonymous, 4096);
    }
    close(path_only);
  }

  /* A long path of ./ repeats below the directory, valid on Linux. */
  {
    static char dots[4096];
    size_t at = 0;
    while (at + 2 < 4093) {
      dots[at++] = '.';
      dots[at++] = '/';
    }
    dots[at++] = '.';
    dots[at] = '\0';
    int again = openat(directory, dots, O_RDONLY | O_DIRECTORY);
    printf("long path: %zu bytes opened=%d\n", strlen(dots), again >= 0);
    if (again >= 0) {
      close(again);
    }
  }

  /* fchmodat2 of a name below the directory changes the file N is. */
  {
    char file[] = "/tmp/proc-fd-directory-mode-XXXXXX";
    int target = mkstemp(file);
    fchmod(target, 0644);
    char target_name[16];
    snprintf(target_name, sizeof(target_name), "%d", target);
    errno = 0;
    long rc = syscall(452 /* fchmodat2 */, directory, target_name, 0600, 0);
    struct stat after;
    fstat(target, &after);
    printf("fchmodat2: rc=%ld errno=%s mode=%o\n", rc, errno_name(rc < 0 ? errno : 0),
           after.st_mode & 0777);
    close(target);
    unlink(file);
  }
  close(null);

  /* A name below a directory reached through the directory's link, by its
   * descriptor and through the link spelled with // and /./ before the
   * descriptor's number. (A name that is not UTF-8 is not used: Hermit's
   * scheduler request for an open of one fails before the open, base and
   * head alike; the bytes after the link are kept as they are, which a unit
   * test checks.) */
  {
    char scratch[] = "/tmp/proc-fd-directory-names-XXXXXX";
    CHECK(mkdtemp(scratch) != NULL, "mkdtemp: %s", strerror(errno));
    int inner = open_directory(scratch, O_RDONLY | O_DIRECTORY);
    int made = openat(inner, "entry", O_CREAT | O_WRONLY | O_CLOEXEC, 0600);
    CHECK(made >= 0, "create entry: %s", strerror(errno));
    close(made);
    char path[192];
    struct stat found;
    snprintf(path, sizeof(path), "/proc/self/fd/%d/%d/entry", directory, inner);
    printf("names: /proc/self/fd/D/T/entry stat=%d\n", stat(path, &found));
    snprintf(path, sizeof(path), "%d/entry", inner);
    printf("names: fstatat D T/entry=%d\n", fstatat(directory, path, &found, 0));
    snprintf(path, sizeof(path), "/proc//self/fd/%d/%d/entry", directory, inner);
    printf("names: /proc//self/fd/D/T/entry stat=%d\n", stat(path, &found));
    snprintf(path, sizeof(path), "/proc/./self/fd/%d/%d/entry", directory, inner);
    printf("names: /proc/./self/fd/D/T/entry stat=%d\n", stat(path, &found));
    snprintf(path, sizeof(path), "//proc/self/fd/%d/%d/entry", directory, inner);
    printf("names: //proc/self/fd/D/T/entry stat=%d\n", stat(path, &found));
    snprintf(path, sizeof(path), "/proc/self/fd/./%d//%d/entry", directory, inner);
    printf("names: /proc/self/fd/./D//T/entry stat=%d\n", stat(path, &found));
    unlinkat(inner, "entry", 0);
    close(inner);
    rmdir(scratch);
  }

  /* The links a lookup follows through the directory's descriptor links
   * count against MAXSYMLINKS, 40: /proc/<pid>/fd and then 40 of them reach
   * the directory, 41 fail with ELOOP, for an open and for a stat. */
  {
    char deep[512];
    int at = snprintf(deep, sizeof(deep), "/proc/%d/fd", (int)getpid());
    struct stat found;
    for (int link = 1; link <= 41; link++) {
      at += snprintf(deep + at, sizeof(deep) - (size_t)at, "/%d", directory);
      if (link == 40 || link == 41) {
        errno = 0;
        int reached = open(deep, O_RDONLY | O_DIRECTORY);
        int open_error = reached < 0 ? errno : 0;
        if (reached >= 0) {
          close(reached);
        }
        errno = 0;
        int stated = stat(deep, &found);
        printf("links: %d descriptor links open=%d errno=%s stat=%d errno=%s\n", link,
               reached >= 0, errno_name(open_error), stated, errno_name(stated < 0 ? errno : 0));
      }
    }
  }

  check_procfs_directories(directory);
  {
    int root = open_directory("/proc", O_RDONLY | O_DIRECTORY);
    check_bare_links(root, directory);
    close(root);
  }
  check_orders(directory);
  check_path_only_refusals();
  check_received_alias_lock_and_ignored_destination();
  check_relative_paths_through_links();
  check_mount_point_by_other_spellings();

  /* exec: the directory, not close-on-exec, survives; a close-on-exec
   * descriptor does not. */
  if (no_exec) {
    close(directory);
    printf("after exec: not checked (--no-exec)\n");
    printf("failures=%d\n", failures);
    return failures == 0 ? 0 : 1;
  }
  close(directory);
  int inherited = open_directory("/proc/self/fd", O_RDONLY | O_DIRECTORY);
  int cloexec = open("/dev/null", O_RDONLY | O_CLOEXEC);
  /* Received eventfds, listed though Detcore does not model them, keep the
   * close-on-exec flag F_SETFD and FIONCLEX give them. */
  CHECK(socketpair(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0, pair) == 0, "socketpair: %s",
        strerror(errno));
  int event = eventfd(0, EFD_CLOEXEC);
  int received_cloexec = pass_to_self(pair, event);
  CHECK(fcntl(received_cloexec, F_SETFD, FD_CLOEXEC) == 0, "F_SETFD: %s", strerror(errno));
  int received_kept = pass_to_self_with(pair, event, MSG_CMSG_CLOEXEC);
  CHECK(ioctl(received_kept, FIONCLEX) == 0, "FIONCLEX: %s", strerror(errno));
  close(event);
  close(pair[0]);
  close(pair[1]);
  char inherited_text[16], cloexec_text[16], received_cloexec_text[16], received_kept_text[16],
      failures_text[16];
  snprintf(inherited_text, sizeof(inherited_text), "%d", inherited);
  snprintf(cloexec_text, sizeof(cloexec_text), "%d", cloexec);
  snprintf(received_cloexec_text, sizeof(received_cloexec_text), "%d", received_cloexec);
  snprintf(received_kept_text, sizeof(received_kept_text), "%d", received_kept);
  /* The checks this image made carry over to the next one. */
  snprintf(failures_text, sizeof(failures_text), "%d", failures);
  fflush(stdout);
  execl(argv[0], argv[0], "--after-exec", inherited_text, cloexec_text, received_cloexec_text,
        received_kept_text, failures_text, (char *)NULL);
  printf("exec failed: %s\n", strerror(errno));
  return 1;
}
