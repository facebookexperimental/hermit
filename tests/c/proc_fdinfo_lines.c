/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Reads its own /proc/self/fdinfo/<fd> for every kind of descriptor whose
 * fdinfo prints state of its own (package K slice K8-h of
 * https://github.com/rrnewton/hermit/issues/4213), and prints each file with
 * the values that name host objects checked and masked, so that one run's
 * output can be compared across native Linux and Hermit's backends:
 *
 * - `mnt_id:` must equal statx's STATX_MNT_ID for the descriptor, and `ino:`
 *   fstat's st_ino (printed as `=statx` / `=fstat`, or the value when they
 *   disagree);
 * - a `lock:` line's object must be the superblock device's major and minor
 *   in hexadecimal and the inode, whose inode must equal st_ino (`INO`); its
 *   device is printed as `DEV`, and its owner as `SELF` when it is this
 *   process's pid;
 * - `eventfd-id:` is printed as `ID`, and a timerfd's `it_value`
 *   nanoseconds as `NS` (the clocks differ), its seconds as they are.
 *
 * It covers: a pipe's position (always 0), a sealed memfd's (no seals line),
 * an eventfd's count after writes and a read and an EFD_SEMAPHORE one's, a
 * signalfd's mask (SIGKILL taken out) before and after an update, a timerfd
 * unarmed, armed, absolute, expired once, forwarded by timerfd_gettime and
 * read, a Unix socket's queued descriptors (scm_fds) before and after a
 * receive, F_SETLK, F_OFD_SETLK, flock and F_SETLEASE lock lines, a re-read
 * after a rewind, ENOENT for a descriptor that is not open, for a
 * non-canonical name, and for a read after the descriptor closed. The calls
 * a backend refuses print their errno, so a run shows what it did.
 *
 * Last, the standard output and error, which are whatever the runner gave
 * (a pipe natively, Hermit's capture under --verify): only whether each line
 * agrees with the guest's own view of the descriptor is printed, not the
 * values.
 */

#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/eventfd.h>
#include <sys/file.h>
#include <sys/mman.h>
#include <sys/resource.h>
#include <sys/signalfd.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/sysmacros.h>
#include <sys/timerfd.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static int failures;
/* This program's path, which --received-lock-exec execs again. */
static const char* self_path;

static const char* errno_name(int error) {
  switch (error) {
    case 0:
      return "0";
    case ENOENT:
      return "ENOENT";
    case ENOSYS:
      return "ENOSYS";
    case EAGAIN:
      return "EAGAIN";
    case EACCES:
      return "EACCES";
    case EINVAL:
      return "EINVAL";
    case EBADF:
      return "EBADF";
    case EPERM:
      return "EPERM";
    case EOPNOTSUPP:
      return "EOPNOTSUPP";
    default:
      return "OTHER";
  }
}

/* The descriptor's fdinfo, read whole into `text`; its length, or -errno. */
static ssize_t read_fdinfo_path(const char* path, char* text, size_t size) {
  int info = open(path, O_RDONLY | O_CLOEXEC);
  if (info < 0) {
    return -errno;
  }
  size_t used = 0;
  for (;;) {
    ssize_t got = read(info, text + used, size - 1 - used);
    if (got < 0) {
      int error = errno;
      close(info);
      return -error;
    }
    if (got == 0) {
      break;
    }
    used += (size_t)got;
    if (used == size - 1) {
      break;
    }
  }
  close(info);
  text[used] = '\0';
  return (ssize_t)used;
}

/*
 * Whether `text` is a decimal number and nothing else, its value in `value`.
 * Linux prints these fields with %i, %llu or %d of a nonnegative value, so a
 * sign, a space or anything after the digits is malformed.
 */
static int whole_number(const char* text, unsigned long long* value) {
  if (*text < '0' || *text > '9') {
    return 0;
  }
  char* end = NULL;
  errno = 0;
  *value = strtoull(text, &end, 10);
  return errno == 0 && *end == '\0';
}

/*
 * Whether `line` is a timerfd's `it_value: (%llu, %llu)` exactly, with the
 * nanoseconds below one second, and its seconds in `seconds`.
 */
static int timer_value_line(const char* line, unsigned long long* seconds) {
  const char* prefix = "it_value: (";
  if (strncmp(line, prefix, strlen(prefix)) != 0) {
    return 0;
  }
  char copy[128];
  snprintf(copy, sizeof(copy), "%s", line + strlen(prefix));
  char* comma = strstr(copy, ", ");
  size_t length = strlen(copy);
  if (comma == NULL || length < 2 || copy[length - 1] != ')') {
    return 0;
  }
  copy[length - 1] = '\0';
  *comma = '\0';
  unsigned long long nanoseconds = 0;
  return whole_number(copy, seconds) && whole_number(comma + 2, &nanoseconds) &&
      nanoseconds < 1000000000ULL;
}

/*
 * Whether `text` (`length` bytes) is at least two lowercase hexadecimal
 * digits and nothing else, as Linux prints a device's major or minor (%02x).
 */
static int hex_field(const char* text, size_t length) {
  if (length < 2) {
    return 0;
  }
  for (size_t i = 0; i < length; i++) {
    if (!((text[i] >= '0' && text[i] <= '9') || (text[i] >= 'a' && text[i] <= 'f'))) {
      return 0;
    }
  }
  return 1;
}

/*
 * Whether `object` is a lock line's "%02x:%02x:%llu" exactly, the superblock
 * device's major and minor and the inode (lock_get_status, fs/locks.c), with
 * its inode in `ino`.
 */
static int lock_object(const char* object, unsigned long long* ino) {
  const char* first = strchr(object, ':');
  const char* second = first ? strchr(first + 1, ':') : NULL;
  if (second == NULL || strchr(second + 1, ':') != NULL) {
    return 0;
  }
  return hex_field(object, (size_t)(first - object)) &&
      hex_field(first + 1, (size_t)(second - first - 1)) && whole_number(second + 1, ino);
}

/* Print one fdinfo line, its host identities checked against `fd`. */
static void print_line(int fd, char* line) {
  struct stat st;
  struct statx sx;
  int have_stat = fstat(fd, &st) == 0;
  int have_statx =
      statx(fd, "", AT_EMPTY_PATH, STATX_MNT_ID, &sx) == 0 && (sx.stx_mask & STATX_MNT_ID);
  if (strncmp(line, "mnt_id:\t", 8) == 0) {
    unsigned long long mnt = 0;
    if (whole_number(line + 8, &mnt) && have_statx && mnt == sx.stx_mnt_id) {
      printf("mnt_id:\t=statx\n");
    } else {
      printf("%s (statx %llu)\n", line, have_statx ? (unsigned long long)sx.stx_mnt_id : 0ULL);
      failures++;
    }
    return;
  }
  if (strncmp(line, "ino:\t", 5) == 0) {
    unsigned long long ino = 0;
    if (whole_number(line + 5, &ino) && have_stat && ino == (unsigned long long)st.st_ino) {
      printf("ino:\t=fstat\n");
    } else {
      printf("%s (fstat %llu)\n", line, have_stat ? (unsigned long long)st.st_ino : 0ULL);
      failures++;
    }
    return;
  }
  if (strncmp(line, "eventfd-id: ", 12) == 0) {
    unsigned long long id = 0;
    if (whole_number(line + 12, &id)) {
      printf("eventfd-id: ID\n");
    } else {
      printf("%s (malformed)\n", line);
      failures++;
    }
    return;
  }
  if (strncmp(line, "it_value:", 9) == 0) {
    unsigned long long seconds = 0;
    if (timer_value_line(line, &seconds)) {
      printf("it_value: (%llu, NS)\n", seconds);
    } else {
      printf("%s (malformed)\n", line);
      failures++;
    }
    return;
  }
  if (strncmp(line, "lock:\t", 6) == 0) {
    /* lock:\tN: CLASS MODE TYPE PID MAJ:MIN:INO START END, Linux's padding. */
    char* fields[16];
    int count = 0;
    char copy[512];
    snprintf(copy, sizeof(copy), "%s", line + 6);
    for (char* save = NULL, *field = strtok_r(copy, " ", &save); field && count < 16;
         field = strtok_r(NULL, " ", &save)) {
      fields[count++] = field;
    }
    if (count != 8) {
      printf("lock:\tUNPARSED %s\n", line + 6);
      failures++;
      return;
    }
    unsigned long long ino = 0;
    unsigned long long number = 0;
    /* The owner is %d of a pid, -1 or 0; the object is the device's major
     * and minor and the inode ("%02x:%02x:%llu"); the range is "%Ld EOF" or
     * "%Ld %Ld". Anything else is malformed. */
    int owner_ok = strcmp(fields[4], "-1") == 0 || whole_number(fields[4], &number);
    int object_ok = lock_object(fields[5], &ino);
    int range_ok = whole_number(fields[6], &number) &&
        (strcmp(fields[7], "EOF") == 0 || whole_number(fields[7], &number));
    if (!owner_ok || !object_ok || !range_ok) {
      printf("lock:\tUNPARSED %s\n", line + 6);
      failures++;
      return;
    }
    long owner = strtol(fields[4], NULL, 10);
    char owner_text[32];
    if (owner == (long)getpid()) {
      snprintf(owner_text, sizeof(owner_text), "SELF");
    } else {
      snprintf(owner_text, sizeof(owner_text), "%ld", owner);
    }
    /* Keep Linux's column padding: the text through the type column. */
    char head[128];
    const char* type_at = strstr(line + 6, fields[3]);
    const char* after_type = type_at ? type_at + strlen(fields[3]) + 1 : NULL;
    if (!after_type || after_type - (line + 6) >= (long)sizeof(head)) {
      printf("lock:\tUNPARSED %s\n", line + 6);
      failures++;
      return;
    }
    snprintf(head, (size_t)(after_type - (line + 6)) + 1, "%s", line + 6);
    printf(
        "lock:\t%s%s DEV:%s %s %s\n",
        head,
        owner_text,
        have_stat && ino == (unsigned long long)st.st_ino ? "INO" : "ino?",
        fields[6],
        fields[7]);
    return;
  }
  printf("%s\n", line);
}

static void show(const char* label, int fd) {
  char path[64];
  char text[8192];
  snprintf(path, sizeof(path), "/proc/self/fdinfo/%d", fd);
  ssize_t length = read_fdinfo_path(path, text, sizeof(text));
  printf("== %s\n", label);
  if (length < 0) {
    printf("open or read: %s\n", errno_name((int)-length));
    return;
  }
  if (length == 0) {
    printf("(empty)\n");
    return;
  }
  for (char* save = NULL, *line = strtok_r(text, "\n", &save); line;
       line = strtok_r(NULL, "\n", &save)) {
    print_line(fd, line);
  }
}

static void check_to(FILE* out, const char* what, int result) {
  if (result < 0) {
    fprintf(out, "%s: -1 %s\n", what, errno_name(errno));
  } else {
    fprintf(out, "%s: ok\n", what);
  }
}

static void check(const char* what, int result) {
  check_to(stdout, what, result);
}

static void pipes(void) {
  int ends[2];
  if (pipe2(ends, 0) != 0) {
    printf("pipe2: %s\n", errno_name(errno));
    failures++;
    return;
  }
  show("pipe read end", ends[0]);
  if (write(ends[1], "hello", 5) != 5) {
    failures++;
  }
  show("pipe write end after a write", ends[1]);
  close(ends[0]);
  close(ends[1]);
}

static void memfds(void) {
  int memfd = memfd_create("k8h-sealed", MFD_ALLOW_SEALING);
  if (memfd < 0) {
    printf("memfd_create: %s\n", errno_name(errno));
    failures++;
    return;
  }
  if (write(memfd, "0123456789", 10) != 10) {
    failures++;
  }
  check("F_ADD_SEALS", fcntl(memfd, F_ADD_SEALS, F_SEAL_SHRINK | F_SEAL_GROW));
  printf("seals: %d\n", fcntl(memfd, F_GET_SEALS));
  lseek(memfd, 3, SEEK_SET);
  show("sealed memfd at 3", memfd);
  close(memfd);
}

static void eventfds(void) {
  int counter = eventfd(0, EFD_CLOEXEC);
  if (counter < 0) {
    printf("eventfd: %s\n", errno_name(errno));
    failures++;
    return;
  }
  uint64_t value = 3;
  if (write(counter, &value, 8) != 8) {
    failures++;
  }
  value = 4;
  if (write(counter, &value, 8) != 8) {
    failures++;
  }
  show("eventfd after writes of 3 and 4", counter);
  if (read(counter, &value, 8) != 8 || value != 7) {
    failures++;
  }
  show("eventfd after a read", counter);
  close(counter);

  int semaphore = eventfd(2, EFD_SEMAPHORE | EFD_NONBLOCK);
  if (semaphore < 0) {
    printf("eventfd semaphore: %s\n", errno_name(errno));
    failures++;
    return;
  }
  if (read(semaphore, &value, 8) != 8 || value != 1) {
    failures++;
  }
  show("semaphore eventfd after one read", semaphore);
  close(semaphore);
}

static void signalfds(void) {
  sigset_t mask;
  sigemptyset(&mask);
  sigaddset(&mask, SIGUSR1);
  sigaddset(&mask, SIGRTMIN + 2);
  sigaddset(&mask, SIGKILL);
  int fd = signalfd(-1, &mask, 0);
  if (fd < 0) {
    printf("signalfd: %s\n", errno_name(errno));
    failures++;
    return;
  }
  show("signalfd of SIGUSR1, SIGRTMIN+2 and SIGKILL", fd);
  sigemptyset(&mask);
  sigaddset(&mask, SIGUSR2);
  sigaddset(&mask, SIGSTOP);
  /* Linux keeps the descriptor's flags on an update, and these are the
   * ones it was made with. */
  if (signalfd(fd, &mask, 0) != fd) {
    printf("signalfd update: %s\n", errno_name(errno));
    failures++;
  }
  show("signalfd updated to SIGUSR2 and SIGSTOP", fd);
  /* An update changes the one context every descriptor of it shares
   * (do_signalfd4): a dup made before the update reads the new mask. */
  int alias = dup(fd);
  sigemptyset(&mask);
  sigaddset(&mask, SIGHUP);
  if (alias < 0 || signalfd(fd, &mask, 0) != fd) {
    printf("signalfd update through a dup: %s\n", errno_name(errno));
    failures++;
  }
  show("signalfd updated to SIGHUP", fd);
  show("its dup, made before that update", alias);
  /* Linux validates an update's flags and applies none of them. */
  errno = 0;
  int updated = signalfd(fd, &mask, SFD_NONBLOCK);
  printf("signalfd update with SFD_NONBLOCK: %s\n", updated == fd ? "ok" : errno_name(errno));
  int status_flags = fcntl(fd, F_GETFL);
  printf(
      "the updated descriptor is nonblocking after it: %s\n",
      status_flags >= 0 && (status_flags & O_NONBLOCK) ? "yes" : "no");
  close(alias);
  close(fd);
}

static void sleep_ns(long ns) {
  struct timespec span = {ns / 1000000000L, ns % 1000000000L};
  while (nanosleep(&span, &span) != 0 && errno == EINTR) {
  }
}

static void timerfds(void) {
  int timer = timerfd_create(CLOCK_MONOTONIC, TFD_CLOEXEC);
  if (timer < 0) {
    printf("timerfd_create: %s\n", errno_name(errno));
    failures++;
    return;
  }
  show("unarmed monotonic timerfd", timer);
  struct itimerspec setting = {{1, 500000000}, {100, 0}};
  check("timerfd_settime relative", timerfd_settime(timer, 0, &setting, NULL));
  show("timerfd armed for 100 s, every 1.5 s", timer);
  struct timespec now;
  clock_gettime(CLOCK_MONOTONIC, &now);
  struct itimerspec absolute = {{0, 0}, {now.tv_sec + 50, now.tv_nsec}};
  check("timerfd_settime absolute", timerfd_settime(timer, TFD_TIMER_ABSTIME, &absolute, NULL));
  show("timerfd armed absolute", timer);
  close(timer);

  int once = timerfd_create(CLOCK_REALTIME, TFD_NONBLOCK);
  struct itimerspec soon = {{0, 0}, {0, 1000000}};
  check("timerfd_settime one shot", timerfd_settime(once, 0, &soon, NULL));
  sleep_ns(20000000);
  show("one-shot realtime timerfd after it fired", once);
  uint64_t ticks = 0;
  if (read(once, &ticks, 8) != 8 || ticks != 1) {
    printf("one-shot read: %llu\n", (unsigned long long)ticks);
    failures++;
  }
  show("one-shot timerfd after the read", once);
  close(once);

  /* A periodic timer that fired once, 100 ms in, and next fires at 10.1 s:
   * Linux counts that one expiration when it fires and leaves the timer
   * stopped (it_value 0) until timerfd_gettime or a read forwards it. */
  int periodic = timerfd_create(CLOCK_MONOTONIC, TFD_NONBLOCK);
  struct itimerspec every = {{10, 0}, {0, 100000000}};
  check("timerfd_settime periodic", timerfd_settime(periodic, 0, &every, NULL));
  sleep_ns(300000000L);
  show("periodic timerfd after it fired once", periodic);
  struct itimerspec left;
  check("timerfd_gettime", timerfd_gettime(periodic, &left));
  show("periodic timerfd after timerfd_gettime", periodic);
  if (read(periodic, &ticks, 8) != 8) {
    failures++;
  }
  printf("periodic read: %llu\n", (unsigned long long)ticks);
  show("periodic timerfd after the read", periodic);
  close(periodic);
}

static int send_descriptors(int socket, const int* fds, int count) {
  char data = 'x';
  struct iovec io = {&data, 1};
  union {
    char buffer[CMSG_SPACE(4 * sizeof(int))];
    struct cmsghdr align;
  } control;
  memset(&control, 0, sizeof(control));
  struct msghdr message = {0};
  message.msg_iov = &io;
  message.msg_iovlen = 1;
  message.msg_control = control.buffer;
  message.msg_controllen = CMSG_SPACE((size_t)count * sizeof(int));
  struct cmsghdr* header = CMSG_FIRSTHDR(&message);
  header->cmsg_level = SOL_SOCKET;
  header->cmsg_type = SCM_RIGHTS;
  header->cmsg_len = CMSG_LEN((size_t)count * sizeof(int));
  memcpy(CMSG_DATA(header), fds, (size_t)count * sizeof(int));
  return (int)sendmsg(socket, &message, 0);
}

static int receive_descriptors(int socket, int* fds, int count) {
  char data;
  struct iovec io = {&data, 1};
  union {
    char buffer[CMSG_SPACE(4 * sizeof(int))];
    struct cmsghdr align;
  } control;
  struct msghdr message = {0};
  message.msg_iov = &io;
  message.msg_iovlen = 1;
  message.msg_control = control.buffer;
  message.msg_controllen = sizeof(control.buffer);
  if (recvmsg(socket, &message, MSG_CMSG_CLOEXEC) != 1) {
    return -1;
  }
  struct cmsghdr* header = CMSG_FIRSTHDR(&message);
  if (!header || header->cmsg_type != SCM_RIGHTS) {
    return -1;
  }
  int got = (int)((header->cmsg_len - CMSG_LEN(0)) / sizeof(int));
  memcpy(fds, CMSG_DATA(header), (size_t)(got < count ? got : count) * sizeof(int));
  return got;
}

static void sockets(void) {
  int pair[2];
  if (socketpair(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0, pair) != 0) {
    printf("socketpair: %s\n", errno_name(errno));
    failures++;
    return;
  }
  show("stream socket, nothing queued", pair[1]);
  int carried[2] = {pair[0], pair[0]};
  printf("sendmsg of two descriptors: %d\n", send_descriptors(pair[0], carried, 2));
  show("receiving end with two descriptors queued", pair[1]);
  show("sending end", pair[0]);
  int received[4] = {-1, -1, -1, -1};
  printf("recvmsg: %d descriptors\n", receive_descriptors(pair[1], received, 4));
  show("receiving end after the receive", pair[1]);
  for (int i = 0; i < 4; i++) {
    if (received[i] >= 0) {
      close(received[i]);
    }
  }
  close(pair[0]);
  close(pair[1]);

  int datagrams[2];
  if (socketpair(AF_UNIX, SOCK_DGRAM, 0, datagrams) == 0) {
    int one[1] = {datagrams[0]};
    printf("datagram sendmsg: %d\n", send_descriptors(datagrams[0], one, 1));
    show("datagram socket with one descriptor queued", datagrams[1]);
    close(datagrams[0]);
    close(datagrams[1]);
  }
}

/*
 * The lock lines: a record lock (skipped with `skip_posix`), an OFD lock, a
 * flock through a second open (skipped with `skip_flock`), a lease (skipped
 * with `skip_lease`), and the record lock's unlock.
 */
static void locks(const char* directory, int skip_flock, int skip_lease, int skip_posix) {
  char path[512];
  snprintf(path, sizeof(path), "%s/locked", directory);
  int fd = open(path, O_CREAT | O_RDWR | O_TRUNC | O_CLOEXEC, 0600);
  if (fd < 0) {
    printf("open locked: %s\n", errno_name(errno));
    failures++;
    return;
  }
  if (write(fd, "0123456789", 10) != 10) {
    failures++;
  }
  struct flock record = {.l_type = F_WRLCK, .l_whence = SEEK_SET, .l_start = 0, .l_len = 10};
  if (!skip_posix) {
    check("F_SETLK write 0-9", fcntl(fd, F_SETLK, &record));
    show("after F_SETLK", fd);
  }
  struct flock ofd = {.l_type = F_RDLCK, .l_whence = SEEK_SET, .l_start = 100, .l_len = 0};
  check("F_OFD_SETLK read 100-EOF", fcntl(fd, F_OFD_SETLK, &ofd));
  show("after F_OFD_SETLK", fd);

  int second = open(path, O_RDONLY | O_CLOEXEC);
  if (!skip_flock) {
    check("flock LOCK_SH", flock(second, LOCK_SH | LOCK_NB));
    show("second open after flock", second);
  }

  char leased[512];
  snprintf(leased, sizeof(leased), "%s/leased", directory);
  int create = open(leased, O_CREAT | O_WRONLY | O_TRUNC, 0600);
  if (create >= 0) {
    close(create);
  }
  int reader = open(leased, O_RDONLY | O_CLOEXEC);
  if (!skip_lease) {
    check("F_SETLEASE F_RDLCK", fcntl(reader, F_SETLEASE, F_RDLCK));
    show("after F_SETLEASE", reader);
    check("F_SETLEASE F_UNLCK", fcntl(reader, F_SETLEASE, F_UNLCK));
  }

  if (!skip_posix) {
    struct flock unlock = {.l_type = F_UNLCK, .l_whence = SEEK_SET, .l_start = 0, .l_len = 10};
    check("F_SETLK unlock 0-9", fcntl(fd, F_SETLK, &unlock));
    show("after the unlock", fd);
  }
  close(reader);
  close(second);
  close(fd);
  unlink(path);
  unlink(leased);
}

static void proc_locks_snapshot_to(FILE* out, const char* label) {
  char text[16384];
  ssize_t length = read_fdinfo_path("/proc/locks", text, sizeof(text));
  fprintf(out, "--- %s ---\n", label);
  if (length < 0) {
    fprintf(out, "open or read: %s\n", errno_name((int)-length));
    return;
  }
  fputs(text, out);
}

static void proc_locks_snapshot(const char* label) {
  proc_locks_snapshot_to(stdout, label);
}

/*
 * As proc_locks_snapshot, with the whole of /proc/locks however long it is:
 * natively it lists every lock on the host, more than the fixed buffer above
 * holds, and a harness finds this guest's rows among them by inode.
 */
static void proc_locks_snapshot_whole(const char* label) {
  printf("--- %s ---\n", label);
  int info = open("/proc/locks", O_RDONLY | O_CLOEXEC);
  if (info < 0) {
    printf("open: %s\n", errno_name(errno));
    return;
  }
  char chunk[4096];
  for (;;) {
    ssize_t got = read(info, chunk, sizeof(chunk));
    if (got < 0) {
      printf("read: %s\n", errno_name(errno));
      break;
    }
    if (got == 0) {
      break;
    }
    fwrite(chunk, 1, (size_t)got, stdout);
  }
  close(info);
}

static int lock_file(const char* directory, const char* name) {
  char path[512];
  snprintf(path, sizeof(path), "%s/%s", directory, name);
  int fd = open(path, O_CREAT | O_RDWR | O_TRUNC | O_CLOEXEC, 0600);
  if (fd < 0) {
    printf("open %s: %s\n", name, errno_name(errno));
    failures++;
  }
  return fd;
}

/*
 * The rows /proc/locks shows as the guest takes, shares and lets go of locks:
 * a record lock, an OFD lock, an OFD lock a forked child takes on a file it
 * opened (shown while it lives), the record lock's unlock, and the OFD lock's
 * close. With `skip_posix`, no record lock is taken or unlocked. Under Hermit
 * every snapshot is canonical (identities numbered, rows sorted), so two
 * backends' rows compare directly.
 */
static void proc_locks_rows(const char* directory, int skip_posix) {
  int a = lock_file(directory, "rows-a");
  int b = lock_file(directory, "rows-b");
  struct flock whole = {.l_type = F_WRLCK, .l_whence = SEEK_SET};
  if (!skip_posix) {
    check("F_SETLK a", fcntl(a, F_SETLK, &whole));
    proc_locks_snapshot("record lock on a");
  }
  struct flock first = {.l_type = F_RDLCK, .l_whence = SEEK_SET, .l_start = 0, .l_len = 1};
  check("F_OFD_SETLK b", fcntl(b, F_OFD_SETLK, &first));
  proc_locks_snapshot("and an OFD lock on b");
  int ready[2];
  int go[2];
  if (pipe(ready) != 0 || pipe(go) != 0) {
    failures++;
    return;
  }
  pid_t child = fork();
  if (child == 0) {
    int c = lock_file(directory, "rows-c");
    struct flock write_lock = {.l_type = F_WRLCK, .l_whence = SEEK_SET};
    char byte = fcntl(c, F_OFD_SETLK, &write_lock) == 0 ? 'y' : 'n';
    if (write(ready[1], &byte, 1) != 1 || read(go[0], &byte, 1) != 1) {
      _exit(1);
    }
    _exit(0);
  }
  char answer = 0;
  if (child < 0 || read(ready[0], &answer, 1) != 1) {
    failures++;
    return;
  }
  printf("child's F_OFD_SETLK: %s\n", answer == 'y' ? "ok" : "failed");
  proc_locks_snapshot("and the child's OFD lock on c");
  if (write(go[1], "x", 1) != 1) {
    failures++;
  }
  int status = 0;
  if (waitpid(child, &status, 0) != child || !WIFEXITED(status) || WEXITSTATUS(status) != 0) {
    failures++;
  }
  proc_locks_snapshot("after the child exited");
  if (!skip_posix) {
    struct flock unlock = {.l_type = F_UNLCK, .l_whence = SEEK_SET};
    check("F_SETLK a unlock", fcntl(a, F_SETLK, &unlock));
    proc_locks_snapshot("after a's unlock");
  }
  close(b);
  proc_locks_snapshot("after b's close");
  close(a);
  const char* names[] = {"rows-a", "rows-b", "rows-c"};
  for (size_t i = 0; i < sizeof(names) / sizeof(names[0]); i++) {
    char path[512];
    snprintf(path, sizeof(path), "%s/%s", directory, names[i]);
    unlink(path);
  }
}

static void lookups(void) {
  char text[4096];
  ssize_t missing = read_fdinfo_path("/proc/self/fdinfo/987", text, sizeof(text));
  printf("fdinfo of a descriptor that is not open: %s\n", errno_name(missing < 0 ? (int)-missing : 0));
  int fd = eventfd(0, 0);
  char leading[64];
  snprintf(leading, sizeof(leading), "/proc/self/fdinfo/0%d", fd);
  ssize_t padded = read_fdinfo_path(leading, text, sizeof(text));
  printf("fdinfo named with a leading zero: %s\n", errno_name(padded < 0 ? (int)-padded : 0));

  /* A rewind shows the descriptor's state anew, as seq_show runs again. */
  char path[64];
  snprintf(path, sizeof(path), "/proc/self/fdinfo/%d", fd);
  int info = open(path, O_RDONLY);
  ssize_t first = read(info, text, sizeof(text) - 1);
  uint64_t value = 9;
  if (write(fd, &value, 8) != 8) {
    failures++;
  }
  lseek(info, 0, SEEK_SET);
  ssize_t again = read(info, text, sizeof(text) - 1);
  text[again > 0 ? again : 0] = '\0';
  printf("re-read after a rewind: %s\n", strstr(text, "eventfd-count:                9") ? "count 9" : "stale");
  printf("first read length nonzero: %d\n", first > 0);
  close(info);

  /* seq_show looks the descriptor up at the first read. */
  info = open(path, O_RDONLY);
  close(fd);
  errno = 0;
  ssize_t after = read(info, text, sizeof(text) - 1);
  printf("read after the descriptor closed: %zd %s\n", after < 0 ? (ssize_t)-1 : after, errno_name(after < 0 ? errno : 0));
  close(info);
}

/*
 * The launcher holds a shared flock on the file it gave this process as its
 * standard input. The guest's first lock call through that description takes
 * a record lock (`record`, F_SETLK) or an OFD lock (F_OFD_SETLK) on its first
 * byte: the flock was never this process's, so its fdinfo line names an
 * owner the guest's PID namespace cannot see (0) and /proc/locks leaves its
 * row out (locks_translate_pid, locks_show), whichever call came first.
 */
static void inherited_stdin_lock(int record) {
  struct flock first = {.l_type = F_RDLCK, .l_whence = SEEK_SET, .l_start = 0, .l_len = 1};
  check(
      record ? "F_SETLK on standard input" : "F_OFD_SETLK on standard input",
      fcntl(0, record ? F_SETLK : F_OFD_SETLK, &first));
  show("standard input", 0);
  proc_locks_snapshot("with the launcher's flock on standard input");
}

/*
 * Two opens of one file, each holding the same shared OFD read lock, and a
 * dup of one of them: Linux lists each lock object, so /proc/locks shows two
 * rows, and the dup adds none.
 */
static void twin_ofd_locks(const char* directory) {
  int first = lock_file(directory, "twin");
  char path[512];
  snprintf(path, sizeof(path), "%s/twin", directory);
  int second = open(path, O_RDWR | O_CLOEXEC);
  int alias = first >= 0 ? dup(first) : -1;
  struct flock shared = {.l_type = F_RDLCK, .l_whence = SEEK_SET, .l_start = 0, .l_len = 10};
  check("F_OFD_SETLK through the first open", fcntl(first, F_OFD_SETLK, &shared));
  check("F_OFD_SETLK through the second open", fcntl(second, F_OFD_SETLK, &shared));
  check("F_OFD_SETLK through the first open's dup", fcntl(alias, F_OFD_SETLK, &shared));
  proc_locks_snapshot("two opens' identical shared OFD locks and a dup");
  close(alias);
  close(second);
  close(first);
  unlink(path);
}

/*
 * The launcher gave this process its standard input and output as one
 * description (`<>file >&0`) or as two opens of one file. An OFD read lock
 * through standard input, the same request through standard output, and an
 * unlock through standard output. With one description the second request
 * is the same lock, so Linux lists the one lock once and nothing after the
 * unlock; with two, each open's lock is a row of its own, and the unlock
 * leaves standard input's. The report goes to standard output, that file.
 */
static void shared_stdio_locks(void) {
  struct flock shared = {.l_type = F_RDLCK, .l_whence = SEEK_SET, .l_start = 0, .l_len = 1};
  check("F_OFD_SETLK through standard input", fcntl(0, F_OFD_SETLK, &shared));
  proc_locks_snapshot("one lock through standard input");
  check("F_OFD_SETLK through standard output", fcntl(1, F_OFD_SETLK, &shared));
  proc_locks_snapshot("the same lock through standard output");
  struct flock unlock = {.l_type = F_UNLCK, .l_whence = SEEK_SET, .l_start = 0, .l_len = 1};
  check("F_OFD_SETLK unlock through standard output", fcntl(1, F_OFD_SETLK, &unlock));
  proc_locks_snapshot("after the unlock through standard output");
}

/*
 * The launcher gave this process one description as its standard input and
 * output. An OFD read lock through standard input, then a fork, after which
 * the child closes standard input and the parent standard output, so each
 * process holds the description through one stream: the lock is still one
 * row. The parent's unlock through standard input then releases it, and no
 * process shows it any more, as on Linux. The report goes to
 * DIRECTORY/shared-stdio-fork, since the parent closes standard output.
 */
static void shared_stdio_fork_unlock(const char* directory) {
  char path[512];
  snprintf(path, sizeof(path), "%s/shared-stdio-fork", directory);
  FILE* report = fopen(path, "w");
  if (report == NULL) {
    failures++;
    return;
  }
  setvbuf(report, NULL, _IONBF, 0);
  struct flock shared = {.l_type = F_RDLCK, .l_whence = SEEK_SET, .l_start = 0, .l_len = 1};
  check_to(report, "F_OFD_SETLK through standard input", fcntl(0, F_OFD_SETLK, &shared));
  proc_locks_snapshot_to(report, "one lock through standard input");
  int closed[2];
  int go[2];
  if (pipe2(closed, O_CLOEXEC) != 0 || pipe2(go, O_CLOEXEC) != 0) {
    fprintf(report, "pipe2: %s\n", errno_name(errno));
    failures++;
    fclose(report);
    return;
  }
  pid_t child = fork();
  if (child == 0) {
    char byte = close(0) == 0 ? 'y' : 'n';
    if (write(closed[1], &byte, 1) != 1 || read(go[0], &byte, 1) != 1) {
      _exit(1);
    }
    _exit(0);
  }
  char answer = 0;
  if (child < 0 || read(closed[0], &answer, 1) != 1) {
    failures++;
  }
  fprintf(report, "child's close of standard input: %s\n", answer == 'y' ? "ok" : "failed");
  check_to(report, "close of standard output", close(1));
  proc_locks_snapshot_to(report, "each process holding the description through one stream");
  struct flock unlock = {.l_type = F_UNLCK, .l_whence = SEEK_SET, .l_start = 0, .l_len = 1};
  check_to(report, "F_OFD_SETLK unlock through standard input", fcntl(0, F_OFD_SETLK, &unlock));
  proc_locks_snapshot_to(report, "after the parent's unlock through standard input");
  if (child > 0) {
    int status = 0;
    if (write(go[1], "x", 1) != 1 || waitpid(child, &status, 0) != child || !WIFEXITED(status) ||
        WEXITSTATUS(status) != 0) {
      failures++;
    }
  }
  fprintf(report, "failures=%d\n", failures);
  fclose(report);
}

/*
 * One description as standard input and output: an OFD read lock through
 * standard input, then standard input closed, then an exec that keeps
 * standard output (which runs --after-close-exec). The lock is the
 * description's, which standard output still holds, so Linux lists it before
 * the exec and after it. The report goes to standard output, that file.
 * Returns only where a step failed.
 */
static void shared_stdio_close_exec(const char* directory) {
  struct flock shared = {.l_type = F_RDLCK, .l_whence = SEEK_SET, .l_start = 0, .l_len = 1};
  check("F_OFD_SETLK through standard input", fcntl(0, F_OFD_SETLK, &shared));
  check("close of standard input", close(0));
  proc_locks_snapshot("standard output holding the description");
  if (failures != 0) {
    return;
  }
  char* const arguments[] = {(char*)self_path, (char*)directory, (char*)"--after-close-exec", NULL};
  execv(self_path, arguments);
  printf("execv: %s\n", errno_name(errno));
  failures++;
}

/*
 * One description as standard input and output: an OFD read lock through
 * standard input, then both streams closed, standard output first. The
 * description's last close releases the lock, so Linux lists it no more.
 * The report goes to DIRECTORY/shared-stdio-close-both.
 */
static void shared_stdio_close_both(const char* directory) {
  char path[512];
  snprintf(path, sizeof(path), "%s/shared-stdio-close-both", directory);
  FILE* report = fopen(path, "w");
  if (report == NULL) {
    failures++;
    return;
  }
  setvbuf(report, NULL, _IONBF, 0);
  struct flock shared = {.l_type = F_RDLCK, .l_whence = SEEK_SET, .l_start = 0, .l_len = 1};
  check_to(report, "F_OFD_SETLK through standard input", fcntl(0, F_OFD_SETLK, &shared));
  proc_locks_snapshot_to(report, "one lock through standard input");
  check_to(report, "close of standard output", close(1));
  check_to(report, "close of standard input", close(0));
  proc_locks_snapshot_to(report, "after both streams closed");
  fprintf(report, "failures=%d\n", failures);
  fclose(report);
}

/*
 * A fork first, then the parent opens a file, takes an OFD write lock and
 * sends the descriptor to the child over SCM_RIGHTS; once the child has
 * received it, the parent closes its own. The child holds the description,
 * and with it the lock, so Linux lists the lock until the child exits.
 */
static void received_from_parent(const char* directory) {
  int pair[2];
  int done[2];
  if (socketpair(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0, pair) != 0 ||
      pipe2(done, O_CLOEXEC) != 0) {
    printf("socketpair or pipe2: %s\n", errno_name(errno));
    failures++;
    return;
  }
  pid_t child = fork();
  if (child == 0) {
    int received[1] = {-1};
    char byte = receive_descriptors(pair[1], received, 1) == 1 ? 'y' : 'n';
    if (write(pair[1], &byte, 1) != 1 || read(done[0], &byte, 1) != 1) {
      _exit(1);
    }
    _exit(0);
  }
  int locked = lock_file(directory, "from-parent");
  struct flock whole = {.l_type = F_WRLCK, .l_whence = SEEK_SET};
  check("F_OFD_SETLK", fcntl(locked, F_OFD_SETLK, &whole));
  int carried[1] = {locked};
  printf("sendmsg to the child: %d\n", send_descriptors(pair[0], carried, 1));
  char answer = 0;
  if (child < 0 || read(pair[0], &answer, 1) != 1) {
    failures++;
  }
  printf("child's receipt: %s\n", answer == 'y' ? "ok" : "failed");
  check("close of the parent's descriptor", close(locked));
  proc_locks_snapshot("the child holding the description the parent closed");
  int status = 0;
  if (child > 0 && (write(done[1], "x", 1) != 1 || waitpid(child, &status, 0) != child ||
                    !WIFEXITED(status) || WEXITSTATUS(status) != 0)) {
    failures++;
  }
  proc_locks_snapshot("after the child exited");
  char path[512];
  snprintf(path, sizeof(path), "%s/from-parent", directory);
  unlink(path);
}

/*
 * A fork first; then the child opens DIRECTORY/in-flight, so that only it
 * holds it, takes a KIND lock on it (ofd: an OFD write lock; flock: an
 * exclusive flock; posix: an F_SETLK write lock), sends the descriptor to the
 * parent over SCM_RIGHTS and lets go of its own: it exits holding it
 * (`sender_exits`, then the parent looks while the child is a zombie) or
 * closes it and waits. The parent looks while the description is in flight,
 * after it receives the descriptor, and after it closes it, and then lets
 * the child go and reaps it. Linux's in-flight reference holds the
 * description, and with it an OFD lock or a flock, until the receipt hands
 * it to the parent; a POSIX lock is the child's, which its close of the
 * descriptor or its exit releases. The file stays, so that a native run's
 * harness can find its rows by inode among the host's.
 */
static void in_flight_scenario(const char* directory, const char* kind, int sender_exits) {
  int pair[2];
  int closed[2];
  if (socketpair(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0, pair) != 0 ||
      pipe2(closed, O_CLOEXEC) != 0) {
    printf("socketpair or pipe2: %s\n", errno_name(errno));
    failures++;
    return;
  }
  pid_t child = fork();
  if (child == 0) {
    int locked = lock_file(directory, "in-flight");
    struct flock whole = {.l_type = F_WRLCK, .l_whence = SEEK_SET};
    int taken = strcmp(kind, "flock") == 0   ? flock(locked, LOCK_EX | LOCK_NB)
                : strcmp(kind, "posix") == 0 ? fcntl(locked, F_SETLK, &whole)
                                             : fcntl(locked, F_OFD_SETLK, &whole);
    int carried[1] = {locked};
    if (taken != 0 || send_descriptors(pair[1], carried, 1) != 1) {
      _exit(1);
    }
    if (sender_exits) {
      _exit(0);
    }
    char byte = close(locked) == 0 ? 'y' : 'n';
    if (write(closed[1], &byte, 1) != 1 || read(pair[1], &byte, 1) != 1) {
      _exit(1);
    }
    _exit(0);
  }
  if (child < 0) {
    printf("fork: %s\n", errno_name(errno));
    failures++;
    return;
  }
  if (sender_exits) {
    siginfo_t exited;
    memset(&exited, 0, sizeof(exited));
    int waited = waitid(P_PID, (id_t)child, &exited, WEXITED | WNOWAIT);
    printf("the sender's %s lock, send and exit: %s\n", kind,
           waited == 0 && exited.si_code == CLD_EXITED && exited.si_status == 0 ? "ok"
                                                                                : "failed");
    proc_locks_snapshot_whole("in flight after the sender exited");
  } else {
    char byte = 0;
    ssize_t got = read(closed[0], &byte, 1);
    printf("the sender's %s lock, send and close: %s\n", kind,
           got == 1 && byte == 'y' ? "ok" : "failed");
    proc_locks_snapshot_whole("in flight after the sender closed its descriptor");
  }
  int received[1] = {-1};
  printf("recvmsg: %d descriptors\n", receive_descriptors(pair[0], received, 1));
  proc_locks_snapshot_whole("received");
  check("close of the received descriptor", close(received[0]));
  proc_locks_snapshot_whole("closed");
  int status = 0;
  if ((!sender_exits && write(pair[0], "x", 1) != 1) || waitpid(child, &status, 0) != child ||
      !WIFEXITED(status) || WEXITSTATUS(status) != 0) {
    failures++;
  }
  close(pair[0]);
  close(pair[1]);
  close(closed[0]);
  close(closed[1]);
}

/*
 * The pipes the image --in-flight-receiver-gone=exec or =exec-terminated
 * execs talks through: a byte to READY_FD once it runs, then it waits for one
 * on RELEASE_FD. With exec-terminated it holds the receiving socket as
 * SOCKET_FD, which the exec keeps.
 */
#define AFTER_EXEC_READY_FD 20
#define AFTER_EXEC_RELEASE_FD 21
#define AFTER_EXEC_SOCKET_FD 22

/*
 * A fork first: the child holds the only descriptor of the receiving socket
 * (close-on-exec), the parent the sending one. The parent opens
 * DIRECTORY/receiver-gone, takes an OFD write lock, sends the descriptor and
 * closes its own, so that only the message queued to the child's socket
 * holds the description, and looks. Then the child's socket goes without a
 * close: the child execs this program again, so that close-on-exec drops the
 * socket (`exec`; the new image, --in-flight-receiver-after-exec, says so
 * through a pipe and waits), kills itself with SIGKILL (`kills-itself`),
 * leaves through a raw exit of its only thread (`exits`), is killed by the
 * parent's SIGKILL (`killed`), or dies by the default action of a fatal
 * signal of its own: SIGTERM it sends itself (`terminated`), abort()'s
 * SIGABRT (`aborts`), or the SIGSEGV of a write through a null pointer
 * (`faults`), with no core file. Or the child execs this program again
 * before the parent locks, keeping the socket through a descriptor without
 * close-on-exec, and the new image dies by its own SIGTERM once the
 * description is queued (`exec-terminated`; the new image is
 * --in-flight-receiver-terminated-after-exec): the socket goes with the
 * table the exec gave the child. Linux destroys the queued message with the
 * socket's last reference (do_exit's exit_files, before exit_notify lets the
 * parent wait), and the description, and its lock, with it: the parent looks
 * again, takes and drops a conflicting OFD lock through a fresh open, which
 * Linux grants, looks again, closes its own socket and looks a last time.
 *
 * With `parent-keeps-socket`, the parent keeps its own descriptor of the
 * receiving socket, and the child dies by its own SIGTERM. The child's death
 * does not take the socket's last reference, so the message, and the lock,
 * stay: the parent looks after the death, and after a fresh open's
 * conflicting OFD lock, which Linux refuses (EAGAIN), closes its receiving
 * end, which takes the last reference, and looks again; a second fresh
 * open's conflicting lock is granted, and the parent closes its sending end
 * and looks a last time.
 *
 * The file stays, so that a native run's harness can find its rows by inode.
 */
static void receiver_gone_scenario(const char* directory, const char* how) {
  int exec_terminated = strcmp(how, "exec-terminated") == 0;
  int parent_keeps = strcmp(how, "parent-keeps-socket") == 0;
  int pair[2];
  int go[2];
  int ready[2];
  int release[2];
  if (socketpair(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0, pair) != 0 || pipe2(go, O_CLOEXEC) != 0 ||
      pipe2(ready, O_CLOEXEC) != 0 || pipe2(release, O_CLOEXEC) != 0) {
    printf("socketpair or pipe2: %s\n", errno_name(errno));
    failures++;
    return;
  }
  pid_t child = fork();
  if (child == 0) {
    close(pair[1]);
    if (exec_terminated) {
      if (dup2(pair[0], AFTER_EXEC_SOCKET_FD) != AFTER_EXEC_SOCKET_FD ||
          dup2(ready[1], AFTER_EXEC_READY_FD) != AFTER_EXEC_READY_FD ||
          dup2(release[0], AFTER_EXEC_RELEASE_FD) != AFTER_EXEC_RELEASE_FD) {
        _exit(1);
      }
      char* const arguments[] = {
          (char*)self_path,
          (char*)directory,
          (char*)"--in-flight-receiver-terminated-after-exec",
          NULL};
      execv(self_path, arguments);
      _exit(1);
    }
    char byte = 0;
    if (read(go[0], &byte, 1) != 1) {
      _exit(1);
    }
    if (strcmp(how, "exec") == 0) {
      if (dup2(ready[1], AFTER_EXEC_READY_FD) != AFTER_EXEC_READY_FD ||
          dup2(release[0], AFTER_EXEC_RELEASE_FD) != AFTER_EXEC_RELEASE_FD) {
        _exit(1);
      }
      char* const arguments[] = {
          (char*)self_path, (char*)directory, (char*)"--in-flight-receiver-after-exec", NULL};
      execv(self_path, arguments);
      _exit(1);
    }
    if (strcmp(how, "kills-itself") == 0) {
      kill(getpid(), SIGKILL);
      _exit(1);
    }
    if (strcmp(how, "exits") == 0) {
      syscall(SYS_exit, 0);
      _exit(1);
    }
    if (strcmp(how, "terminated") == 0 || strcmp(how, "aborts") == 0 ||
        strcmp(how, "faults") == 0 || parent_keeps) {
      /* The default action of SIGABRT and SIGSEGV dumps core: not here. */
      struct rlimit no_core = {.rlim_cur = 0, .rlim_max = 0};
      setrlimit(RLIMIT_CORE, &no_core);
      if (strcmp(how, "terminated") == 0 || parent_keeps) {
        kill(getpid(), SIGTERM);
      } else if (strcmp(how, "aborts") == 0) {
        abort();
      } else {
        /* A volatile pointer, so that the compiler cannot turn the write
         * into a trap of its own. */
        int* volatile nowhere = NULL;
        *nowhere = 1;
      }
      _exit(1);
    }
    if (write(ready[1], "r", 1) != 1) {
      _exit(1);
    }
    for (;;) {
      pause();
    }
  }
  if (child < 0) {
    printf("fork: %s\n", errno_name(errno));
    failures++;
    return;
  }
  char byte = 0;
  if (exec_terminated) {
    printf("the child's exec: %s\n", read(ready[0], &byte, 1) == 1 ? "ok" : "failed");
  }
  if (!parent_keeps) {
    close(pair[0]);
  }
  int locked = lock_file(directory, "receiver-gone");
  struct flock whole = {.l_type = F_WRLCK, .l_whence = SEEK_SET};
  check("F_OFD_SETLK", fcntl(locked, F_OFD_SETLK, &whole));
  int carried[1] = {locked};
  printf("sendmsg to the child's socket: %d\n", send_descriptors(pair[1], carried, 1));
  check("close of the parent's descriptor", close(locked));
  proc_locks_snapshot_whole("in flight to the child's socket");
  if (write(exec_terminated ? release[1] : go[1], "g", 1) != 1) {
    failures++;
  }
  int status = 0;
  if (strcmp(how, "exec") == 0) {
    printf("the child's exec: %s\n", read(ready[0], &byte, 1) == 1 ? "ok" : "failed");
  } else {
    if (strcmp(how, "killed") == 0) {
      if (read(ready[0], &byte, 1) != 1) {
        failures++;
      }
      check("the parent's SIGKILL", kill(child, SIGKILL));
    }
    pid_t waited = waitpid(child, &status, 0);
    if (strcmp(how, "exits") == 0) {
      printf(
          "the child's raw exit: %s\n",
          waited == child && WIFEXITED(status) && WEXITSTATUS(status) == 0 ? "ok" : "failed");
    } else {
      int signal = SIGKILL;
      const char* name = "SIGKILL";
      if (strcmp(how, "terminated") == 0 || exec_terminated || parent_keeps) {
        signal = SIGTERM;
        name = "SIGTERM";
      } else if (strcmp(how, "aborts") == 0) {
        signal = SIGABRT;
        name = "SIGABRT";
      } else if (strcmp(how, "faults") == 0) {
        signal = SIGSEGV;
        name = "SIGSEGV";
      }
      printf(
          "the child's death by %s: %s\n",
          name,
          waited == child && WIFSIGNALED(status) && WTERMSIG(status) == signal ? "ok" : "failed");
    }
  }
  char path[512];
  snprintf(path, sizeof(path), "%s/receiver-gone", directory);
  struct flock unlock = {.l_type = F_UNLCK, .l_whence = SEEK_SET};
  if (parent_keeps) {
    proc_locks_snapshot_whole("after the child's death");
    int fresh = open(path, O_RDWR | O_CLOEXEC);
    int refused = fcntl(fresh, F_OFD_SETLK, &whole);
    check("a fresh open's conflicting F_OFD_SETLK", refused);
    if (refused == 0) {
      fcntl(fresh, F_OFD_SETLK, &unlock);
    }
    close(fresh);
    proc_locks_snapshot_whole("after a fresh open's attempt");
    check("close of the parent's receiving end", close(pair[0]));
    proc_locks_snapshot_whole("after the parent's receiving end closed");
    fresh = open(path, O_RDWR | O_CLOEXEC);
    check("a second fresh open's conflicting F_OFD_SETLK", fcntl(fresh, F_OFD_SETLK, &whole));
    check("its unlock", fcntl(fresh, F_OFD_SETLK, &unlock));
    close(fresh);
  } else {
    proc_locks_snapshot_whole("after the child's socket went");
    int fresh = open(path, O_RDWR | O_CLOEXEC);
    check("a fresh open's conflicting F_OFD_SETLK", fcntl(fresh, F_OFD_SETLK, &whole));
    check("its unlock", fcntl(fresh, F_OFD_SETLK, &unlock));
    close(fresh);
    proc_locks_snapshot_whole("after a fresh open's lock and unlock");
  }
  if (strcmp(how, "exec") == 0 &&
      (write(release[1], "x", 1) != 1 || waitpid(child, &status, 0) != child ||
       !WIFEXITED(status) || WEXITSTATUS(status) != 0)) {
    failures++;
  }
  close(pair[1]);
  proc_locks_snapshot_whole("at the end");
  close(go[0]);
  close(go[1]);
  close(ready[0]);
  close(ready[1]);
  close(release[0]);
  close(release[1]);
}

/*
 * What the busy sibling of --in-flight-receiver-dies-busy saw, in memory its
 * process shares with the parent, which reads it after the child's death.
 */
struct busy_sibling_record {
  volatile int rounds;
  volatile int failed_calls;
  volatile int missing_rows;
};

static struct busy_sibling_record* busy_record;
static int busy_socket = -1;
static char busy_path[512];

/*
 * Whether /proc/locks lists a whole-file OFD write lock. Under Hermit it lists
 * only the guest's own rows, so this is the in-flight lock; natively another
 * process's identical lock may be listed too, which only makes the answer yes.
 */
static int lists_whole_file_write_lock(void) {
  int info = open("/proc/locks", O_RDONLY | O_CLOEXEC);
  if (info < 0) {
    return 0;
  }
  static char text[1 << 20];
  size_t length = 0;
  for (;;) {
    ssize_t got = read(info, text + length, sizeof(text) - 1 - length);
    if (got <= 0) {
      break;
    }
    length += (size_t)got;
    if (length == sizeof(text) - 1) {
      break;
    }
  }
  close(info);
  text[length] = '\0';
  int listed = 0;
  char* rest = NULL;
  for (char* line = strtok_r(text, "\n", &rest); line != NULL && !listed;
       line = strtok_r(NULL, "\n", &rest)) {
    size_t line_length = strlen(line);
    listed = strstr(line, "->") == NULL && strstr(line, "OFDLCK") != NULL &&
             strstr(line, "WRITE") != NULL && line_length >= 6 &&
             strcmp(line + line_length - 6, " 0 EOF") == 0;
  }
  return listed;
}

/*
 * The child's second thread, round after round: a read of /proc/locks, which
 * lists the in-flight lock while the child's receiving socket holds it, that
 * is for as long as any thread of the child runs; then a dup and close and an
 * F_GETFL through that socket's descriptor. It counts the reads that listed no
 * lock and the calls that failed, and goes on until the process ends. Each
 * round's one open is its only call that can wait for a turn under Hermit, so
 * a round that starts runs whole.
 */
static void* busy_sibling(void* unused) {
  (void)unused;
  for (;;) {
    if (!lists_whole_file_write_lock()) {
      busy_record->missing_rows++;
    }
    int copy = dup(busy_socket);
    if (copy < 0) {
      busy_record->failed_calls++;
    } else {
      close(copy);
    }
    if (fcntl(busy_socket, F_GETFL) < 0) {
      busy_record->failed_calls++;
    }
    busy_record->rounds++;
  }
  return NULL;
}

/*
 * As --in-flight-receiver-gone, where the child that holds the only receiving
 * socket has a second thread that keeps calling through the descriptors the
 * child holds (busy_sibling) while the main thread ends the process: by
 * SIGKILL to itself (`kills-itself`) or by exit_group (`exit-group`). Until the
 * process ends, Linux keeps every descriptor of it, so each of the sibling's
 * calls succeeds, and the queued message keeps holding the description and
 * its lock, so /proc/locks lists it; once the process is dying, the sibling
 * runs no more user code. The parent prints what
 * the sibling saw after the child's death, besides the rows of
 * --in-flight-receiver-gone (Claude's and Codex's narrow checks of K8-h reb7:
 * Hermit let go of the child's table before granting its exit, so the sibling's
 * calls failed with EBADF and the row went while the lock was held).
 */
static void receiver_dies_busy_scenario(const char* directory, const char* how) {
  busy_record = mmap(NULL, sizeof(*busy_record), PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS,
                     -1, 0);
  int pair[2];
  int go[2];
  if (busy_record == MAP_FAILED || socketpair(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0, pair) != 0 ||
      pipe2(go, O_CLOEXEC) != 0) {
    printf("mmap, socketpair or pipe2: %s\n", errno_name(errno));
    failures++;
    return;
  }
  memset((void*)busy_record, 0, sizeof(*busy_record));
  snprintf(busy_path, sizeof(busy_path), "%s/receiver-dies-busy", directory);
  pid_t child = fork();
  if (child == 0) {
    close(pair[1]);
    busy_socket = pair[0];
    char byte = 0;
    if (read(go[0], &byte, 1) != 1) {
      _exit(1);
    }
    pthread_t sibling;
    if (pthread_create(&sibling, NULL, busy_sibling, NULL) != 0) {
      _exit(1);
    }
    while (busy_record->rounds < 3) {
      sched_yield();
    }
    if (strcmp(how, "exit-group") == 0) {
      syscall(SYS_exit_group, 0);
    } else {
      kill(getpid(), SIGKILL);
    }
    _exit(1);
  }
  if (child < 0) {
    printf("fork: %s\n", errno_name(errno));
    failures++;
    return;
  }
  close(pair[0]);
  int locked = lock_file(directory, "receiver-dies-busy");
  struct flock whole = {.l_type = F_WRLCK, .l_whence = SEEK_SET};
  check("F_OFD_SETLK", fcntl(locked, F_OFD_SETLK, &whole));
  int carried[1] = {locked};
  printf("sendmsg to the child's socket: %d\n", send_descriptors(pair[1], carried, 1));
  check("close of the parent's descriptor", close(locked));
  proc_locks_snapshot_whole("in flight to the child's socket");
  if (write(go[1], "g", 1) != 1) {
    failures++;
  }
  int status = 0;
  pid_t waited = waitpid(child, &status, 0);
  if (strcmp(how, "exit-group") == 0) {
    printf(
        "the child's exit_group: %s\n",
        waited == child && WIFEXITED(status) && WEXITSTATUS(status) == 0 ? "ok" : "failed");
  } else {
    printf(
        "the child's death by SIGKILL: %s\n",
        waited == child && WIFSIGNALED(status) && WTERMSIG(status) == SIGKILL ? "ok" : "failed");
  }
  printf("the busy sibling ran: %s\n", busy_record->rounds >= 3 ? "yes" : "no");
  printf("its calls through the child's descriptors that failed: %d\n", busy_record->failed_calls);
  printf("its reads of /proc/locks that listed no lock: %d\n", busy_record->missing_rows);
  proc_locks_snapshot_whole("after the child's death");
  int fresh = open(busy_path, O_RDWR | O_CLOEXEC);
  check("a fresh open's conflicting F_OFD_SETLK", fcntl(fresh, F_OFD_SETLK, &whole));
  struct flock unlock = {.l_type = F_UNLCK, .l_whence = SEEK_SET};
  check("its unlock", fcntl(fresh, F_OFD_SETLK, &unlock));
  close(fresh);
  proc_locks_snapshot_whole("after a fresh open's lock and unlock");
  close(pair[1]);
  close(go[0]);
  close(go[1]);
  munmap((void*)busy_record, sizeof(*busy_record));
}

/*
 * The launcher gave this process one description as its standard input and
 * output (`<>file >&0`). An OFD read lock through standard input; standard
 * input sent over SCM_RIGHTS on a socket pair this process holds both ends
 * of, and closed, so that only the message queued to the receiving socket
 * holds that descriptor; an unlock through standard output; the receipt; the
 * received descriptor's close. One description holds one lock, so Linux
 * lists it after the lock and while the descriptor is in flight, and no more
 * from the unlock on. The report goes to standard output, that file.
 */
static void stdio_alias_in_flight(void) {
  struct flock shared = {.l_type = F_RDLCK, .l_whence = SEEK_SET, .l_start = 0, .l_len = 1};
  check("F_OFD_SETLK through standard input", fcntl(0, F_OFD_SETLK, &shared));
  proc_locks_snapshot_whole("one lock through standard input");
  int pair[2];
  if (socketpair(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0, pair) != 0) {
    printf("socketpair: %s\n", errno_name(errno));
    failures++;
    return;
  }
  int carried[1] = {0};
  printf("sendmsg of standard input: %d\n", send_descriptors(pair[0], carried, 1));
  check("close of standard input", close(0));
  proc_locks_snapshot_whole("standard input in flight");
  struct flock unlock = {.l_type = F_UNLCK, .l_whence = SEEK_SET, .l_start = 0, .l_len = 1};
  check("F_OFD_SETLK unlock through standard output", fcntl(1, F_OFD_SETLK, &unlock));
  proc_locks_snapshot_whole("after the unlock through standard output");
  int received[1] = {-1};
  printf("recvmsg: %d descriptors\n", receive_descriptors(pair[1], received, 1));
  proc_locks_snapshot_whole("received");
  check("close of the received descriptor", close(received[0]));
  proc_locks_snapshot_whole("closed");
  close(pair[0]);
  close(pair[1]);
}

/*
 * Two opens of one file hold identical OFD read locks; the parent sends the
 * second to a child forked before it opened either, which receives it (the
 * receipt joins the description the parent's table models), and closes its
 * own. Linux lists each lock object: two rows while the child holds the
 * second, one after the child closes it, none after the parent closes the
 * first. The file stays, so that a native run's harness can find its rows by
 * inode.
 */
static void joined_twin_locks(const char* directory) {
  int pair[2];
  int done[2];
  if (socketpair(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0, pair) != 0 ||
      pipe2(done, O_CLOEXEC) != 0) {
    printf("socketpair or pipe2: %s\n", errno_name(errno));
    failures++;
    return;
  }
  pid_t child = fork();
  if (child == 0) {
    int received[1] = {-1};
    char byte = receive_descriptors(pair[1], received, 1) == 1 ? 'y' : 'n';
    if (write(pair[1], &byte, 1) != 1 || read(done[0], &byte, 1) != 1) {
      _exit(1);
    }
    byte = close(received[0]) == 0 ? 'y' : 'n';
    if (write(pair[1], &byte, 1) != 1 || read(done[0], &byte, 1) != 1) {
      _exit(1);
    }
    _exit(0);
  }
  if (child < 0) {
    printf("fork: %s\n", errno_name(errno));
    failures++;
    return;
  }
  int first = lock_file(directory, "joined-twins");
  char path[512];
  snprintf(path, sizeof(path), "%s/joined-twins", directory);
  int second = open(path, O_RDWR | O_CLOEXEC);
  struct flock shared = {.l_type = F_RDLCK, .l_whence = SEEK_SET};
  check("F_OFD_SETLK through the first open", fcntl(first, F_OFD_SETLK, &shared));
  check("F_OFD_SETLK through the second open", fcntl(second, F_OFD_SETLK, &shared));
  proc_locks_snapshot_whole("two opens' identical OFD locks");
  int carried[1] = {second};
  printf("sendmsg of the second open to the child: %d\n", send_descriptors(pair[0], carried, 1));
  char answer = 0;
  if (read(pair[0], &answer, 1) != 1) {
    failures++;
  }
  printf("child's receipt: %s\n", answer == 'y' ? "ok" : "failed");
  check("close of the parent's second open", close(second));
  proc_locks_snapshot_whole("the child holding the second open");
  answer = 0;
  if (write(done[1], "x", 1) != 1 || read(pair[0], &answer, 1) != 1) {
    failures++;
  }
  printf("child's close of the second open: %s\n", answer == 'y' ? "ok" : "failed");
  proc_locks_snapshot_whole("after the child closed the second open");
  int status = 0;
  if (write(done[1], "x", 1) != 1 || waitpid(child, &status, 0) != child || !WIFEXITED(status) ||
      WEXITSTATUS(status) != 0) {
    failures++;
  }
  check("close of the first open", close(first));
  proc_locks_snapshot_whole("after the first open's close");
  close(pair[0]);
  close(pair[1]);
  close(done[0]);
  close(done[1]);
}

/*
 * An OFD read lock on the first byte through standard input, then one on the
 * third byte through standard output: the rows /proc/locks shows after each.
 * Where the two are descriptions of their own (a library caller's reserved
 * standard input of its own, while the caller's own standard input and output
 * are one description), the first lock stays shown after the second call.
 */
static void stdin_and_stdout_locks(void) {
  struct flock first = {.l_type = F_RDLCK, .l_whence = SEEK_SET, .l_start = 0, .l_len = 1};
  check("F_OFD_SETLK through standard input", fcntl(0, F_OFD_SETLK, &first));
  proc_locks_snapshot("a lock through standard input");
  struct flock third = {.l_type = F_RDLCK, .l_whence = SEEK_SET, .l_start = 2, .l_len = 1};
  check("F_OFD_SETLK through standard output", fcntl(1, F_OFD_SETLK, &third));
  proc_locks_snapshot("and one through standard output");
}

/*
 * This process sends its own locked description to itself over SCM_RIGHTS,
 * so that it holds the one OFD lock through two descriptors, and execs
 * itself with --after-exec, keeping both: Linux lists the one lock once,
 * before the exec and after it. Returns only where a step failed.
 */
static void received_lock_exec(const char* directory) {
  int locked = lock_file(directory, "received");
  struct flock whole = {.l_type = F_WRLCK, .l_whence = SEEK_SET};
  check("F_OFD_SETLK", fcntl(locked, F_OFD_SETLK, &whole));
  int pair[2];
  if (socketpair(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0, pair) != 0) {
    printf("socketpair: %s\n", errno_name(errno));
    failures++;
    return;
  }
  int carried[1] = {locked};
  int received[1] = {-1};
  printf("sendmsg of the locked descriptor: %d\n", send_descriptors(pair[0], carried, 1));
  printf("recvmsg: %d descriptors\n", receive_descriptors(pair[1], received, 1));
  proc_locks_snapshot("one lock through the opened and the received descriptor");
  if (received[0] < 0 || fcntl(locked, F_SETFD, 0) != 0 || fcntl(received[0], F_SETFD, 0) != 0) {
    printf("keeping both descriptors across the exec: %s\n", errno_name(errno));
    failures++;
  }
  if (failures != 0) {
    return;
  }
  char* const arguments[] = {(char*)self_path, (char*)directory, (char*)"--after-exec", NULL};
  execv(self_path, arguments);
  printf("execv: %s\n", errno_name(errno));
  failures++;
}

/*
 * Writes to DIRECTORY/standard-streams what each standard stream is, by its
 * own fstat, and the names of its fdinfo's lines (or "(empty)"), then the
 * failure count. The runner gives the streams different kinds of file (a
 * regular file, a pseudo-terminal master, whose fdinfo prints its
 * tty-index, a pipe), so each stream's fdinfo must follow its own object.
 */
static void standard_stream_kinds(const char* directory) {
  char path[512];
  snprintf(path, sizeof(path), "%s/standard-streams", directory);
  FILE* report = fopen(path, "w");
  if (report == NULL) {
    failures++;
    return;
  }
  for (int fd = 0; fd <= 2; fd++) {
    struct stat st;
    char kind[64];
    if (fstat(fd, &st) != 0) {
      snprintf(kind, sizeof(kind), "fstat %s", errno_name(errno));
    } else {
      switch (st.st_mode & S_IFMT) {
        case S_IFREG:
          snprintf(kind, sizeof(kind), "regular file");
          break;
        case S_IFIFO:
          snprintf(kind, sizeof(kind), "pipe");
          break;
        case S_IFSOCK:
          snprintf(kind, sizeof(kind), "socket");
          break;
        case S_IFCHR:
          snprintf(
              kind,
              sizeof(kind),
              "character device %u:%u",
              major(st.st_rdev),
              minor(st.st_rdev));
          break;
        case 0:
          snprintf(kind, sizeof(kind), "anonymous inode");
          break;
        default:
          snprintf(kind, sizeof(kind), "other");
          break;
      }
    }
    char fdinfo[64];
    char text[8192];
    snprintf(fdinfo, sizeof(fdinfo), "/proc/self/fdinfo/%d", fd);
    ssize_t length = read_fdinfo_path(fdinfo, text, sizeof(text));
    fprintf(report, "%d (%s):", fd, kind);
    if (length < 0) {
      fprintf(report, " open or read %s\n", errno_name((int)-length));
      failures++;
      continue;
    }
    if (length == 0) {
      fprintf(report, " (empty)\n");
      continue;
    }
    for (char* save = NULL, *line = strtok_r(text, "\n", &save); line;
         line = strtok_r(NULL, "\n", &save)) {
      char* colon = strchr(line, ':');
      if (colon != NULL) {
        *colon = '\0';
      }
      fprintf(report, " %s", line);
    }
    fprintf(report, "\n");
  }
  fprintf(report, "failures=%d\n", failures);
  fclose(report);
}

/*
 * Feeds the line normalizer well-formed and malformed timerfd, eventfd and
 * lock lines: each malformed one must count as a failure, and each
 * well-formed one must not. Prints "self-test ok" when they do.
 */
static int self_test(void) {
  const char* good[] = {
      "it_value: (12, 345)",
      "it_value: (0, 999999999)",
      "eventfd-id: 7",
      "lock:\t1: OFDLCK ADVISORY  READ -1 00:18:7 0 9",
      "lock:\t1: OFDLCK ADVISORY  READ -1 103:0a:7 0 EOF",
  };
  const char* bad[] = {
      "it_value: (0, BAD)",
      "it_value: (0)",
      "it_value: (0, 1000000000)",
      "it_value: (1, 2) trailing",
      "it_value: (-1, 2)",
      "it_value: (1,2)",
      "eventfd-id: x",
      "eventfd-id: 7 8",
      "lock:\t1: OFDLCK ADVISORY  READ -1 00:18:7 0 BAD",
      "lock:\t1: OFDLCK ADVISORY  READ ME 00:18:7 0 EOF",
      "lock:\t1: OFDLCK ADVISORY  READ -1 GG:GG:7 0 9",
      "lock:\t1: OFDLCK ADVISORY  READ -1 00:18:EXTRA:7 0 9",
      "lock:\t1: OFDLCK ADVISORY  READ -1 00:7 0 9",
      "lock:\t1: OFDLCK ADVISORY  READ -1 0:18:7 0 9",
  };
  int wrong = 0;
  for (size_t i = 0; i < sizeof(good) / sizeof(good[0]); i++) {
    char line[128];
    snprintf(line, sizeof(line), "%s", good[i]);
    int before = failures;
    print_line(0, line);
    if (failures != before) {
      printf("self-test: accepted line refused: %s\n", good[i]);
      wrong++;
    }
  }
  for (size_t i = 0; i < sizeof(bad) / sizeof(bad[0]); i++) {
    char line[128];
    snprintf(line, sizeof(line), "%s", bad[i]);
    int before = failures;
    print_line(0, line);
    if (failures == before) {
      printf("self-test: malformed line accepted: %s\n", bad[i]);
      wrong++;
    }
  }
  printf(wrong == 0 ? "self-test ok\n" : "self-test failed\n");
  return wrong == 0 ? 0 : 1;
}

/*
 * The standard output's and error's fdinfo against the guest's own views of
 * them: `pos` with lseek(fd, 0, SEEK_CUR) (0 where the file cannot seek),
 * `flags` with F_GETFL plus O_CLOEXEC from F_GETFD, `mnt_id` with statx's
 * STATX_MNT_ID (0 where statx reports no mount, as for KVM's captured
 * output), and `ino` with fstat. Each verdict is printed; a disagreement is
 * a finding to compare, not a failure, as Hermit's ptrace backend prints
 * Detcore's own open flags there. A missing line or file is a failure.
 */
static void standard_streams(void) {
  for (int fd = 1; fd <= 2; fd++) {
    char path[64];
    char text[8192];
    snprintf(path, sizeof(path), "/proc/self/fdinfo/%d", fd);
    ssize_t length = read_fdinfo_path(path, text, sizeof(text));
    /* The position now, before this function writes to the stream. */
    off_t at = lseek(fd, 0, SEEK_CUR);
    printf("== standard stream %d\n", fd);
    if (length <= 0) {
      printf("open or read: %s\n", length < 0 ? errno_name((int)-length) : "(empty)");
      failures++;
      continue;
    }
    long long pos = 0;
    unsigned long flags = 0;
    unsigned long long mnt = 0;
    unsigned long long ino = 0;
    int seen = 0;
    for (char* save = NULL, *line = strtok_r(text, "\n", &save); line;
         line = strtok_r(NULL, "\n", &save)) {
      if (sscanf(line, "pos:\t%lld", &pos) == 1) {
        seen |= 1;
      } else if (sscanf(line, "flags:\t%lo", &flags) == 1) {
        seen |= 2;
      } else if (sscanf(line, "mnt_id:\t%llu", &mnt) == 1) {
        seen |= 4;
      } else if (sscanf(line, "ino:\t%llu", &ino) == 1) {
        seen |= 8;
      }
    }
    if (seen != 15) {
      printf("lines: incomplete\n");
      failures++;
      continue;
    }
    long long position = at < 0 ? 0 : (long long)at;
    int getfl = fcntl(fd, F_GETFL);
    int getfd = fcntl(fd, F_GETFD);
    unsigned long expected =
        (unsigned long)getfl | ((getfd >= 0 && (getfd & FD_CLOEXEC)) ? (unsigned long)O_CLOEXEC : 0);
    struct statx sx;
    int have_statx =
        statx(fd, "", AT_EMPTY_PATH, STATX_MNT_ID, &sx) == 0 && (sx.stx_mask & STATX_MNT_ID);
    struct stat st;
    int have_stat = fstat(fd, &st) == 0;
    printf("pos agrees with lseek: %s\n", pos == position ? "yes" : "no");
    printf(
        "flags agree with F_GETFL and F_GETFD: %s\n",
        getfl >= 0 && getfd >= 0 && flags == expected ? "yes" : "no");
    printf(
        "mnt_id agrees with statx: %s\n",
        (have_statx ? mnt == sx.stx_mnt_id : mnt == 0) ? "yes" : "no");
    printf(
        "ino agrees with fstat: %s\n",
        have_stat && ino == (unsigned long long)st.st_ino ? "yes" : "no");
  }
}

/*
 * Usage: proc_fdinfo_lines --self-test, or proc_fdinfo_lines [DIRECTORY
 * [--skip-flock] [--skip-lease] [--skip-posix] [--proc-locks | --twin-ofd-locks |
 * --inherited-stdin-lock[=record] | --shared-stdio-locks |
 * --shared-stdio-fork-unlock | --shared-stdio-close-exec | --shared-stdio-close-both |
 * --received-from-parent | --stdin-and-stdout-locks |
 * --in-flight-sender-exits=ofd|flock|posix |
 * --in-flight-sender-closes=ofd|flock|posix |
 * --in-flight-receiver-gone=exec|kills-itself|exits|killed|terminated|aborts|
 * faults|exec-terminated|parent-keeps-socket |
 * --in-flight-receiver-dies-busy=kills-itself|exit-group |
 * --stdio-alias-in-flight | --joined-twin-locks |
 * --received-lock-exec | --standard-stream-kinds]]. The lock files go in
 * DIRECTORY (default /tmp). A backend that refuses flock(2) or F_SETLEASE by
 * stopping the run (KVM refuses flock) is run with the matching option;
 * --skip-posix takes no record lock (F_SETLK), whose /proc/locks rows and
 * fdinfo lines Hermit refuses on KVM, whose provider holds a record lock as
 * an OFD lock.
 * --proc-locks prints /proc/locks through a lock scenario instead of the
 * fdinfo files, --twin-ofd-locks through two opens' identical OFD locks,
 * --inherited-stdin-lock takes a lock on a standard input the launcher holds
 * a flock on, --shared-stdio-locks locks through a standard input and output
 * (one description, or two), --shared-stdio-fork-unlock through one
 * description that a fork leaves in each process through one stream (its
 * report in DIRECTORY/shared-stdio-fork), --shared-stdio-close-exec through
 * one description whose standard input is closed before an exec that keeps
 * standard output (which runs --after-close-exec), --shared-stdio-close-both
 * through one description whose two streams are both closed (its report in
 * DIRECTORY/shared-stdio-close-both), --received-from-parent through a
 * descriptor a parent sends to a child forked before it opened it,
 * --stdin-and-stdout-locks through standard input and then standard output,
 * --in-flight-sender-exits=KIND and --in-flight-sender-closes=KIND through a
 * description in flight whose sender, which holds a lock of that kind
 * through it, exits or closes its descriptor,
 * --in-flight-receiver-gone=HOW through a description in flight to a socket
 * that goes without a close (by an exec, which runs
 * --in-flight-receiver-after-exec, a SIGKILL, a raw exit, or the default
 * action of the receiver's own SIGTERM, abort() or SIGSEGV, or of the SIGTERM
 * of an image the receiver exec'd, which runs
 * --in-flight-receiver-terminated-after-exec), or whose receiver dies by its
 * own SIGTERM while the parent keeps a descriptor of the socket,
 * --in-flight-receiver-dies-busy=HOW through the same with a second thread in
 * the receiver that keeps calling through its descriptors while the receiver
 * ends itself (by SIGKILL or exit_group), --stdio-alias-in-flight
 * through one description as standard input and output whose standard input
 * is in flight when standard output unlocks it, --joined-twin-locks through
 * two opens' identical OFD locks, the second sent to a child,
 * --received-lock-exec through a descriptor and
 * its received copy across an exec (which runs --after-exec), and
 * --standard-stream-kinds writes each standard stream's kind and fdinfo
 * lines to DIRECTORY/standard-streams.
 */
int main(int argc, char** argv) {
  setvbuf(stdout, NULL, _IONBF, 0);
  self_path = argv[0];
  if (argc == 2 && strcmp(argv[1], "--self-test") == 0) {
    return self_test();
  }
  const char* directory = argc > 1 ? argv[1] : "/tmp";
  int skip_flock = 0;
  int skip_lease = 0;
  int skip_posix = 0;
  const char* mode = NULL;
  int inherited = 0;
  for (int i = 2; i < argc; i++) {
    if (strcmp(argv[i], "--skip-flock") == 0) {
      skip_flock = 1;
    } else if (strcmp(argv[i], "--skip-lease") == 0) {
      skip_lease = 1;
    } else if (strcmp(argv[i], "--skip-posix") == 0) {
      skip_posix = 1;
    } else if (
        strcmp(argv[i], "--proc-locks") == 0 || strcmp(argv[i], "--twin-ofd-locks") == 0 ||
        strcmp(argv[i], "--inherited-stdin-lock") == 0 ||
        strcmp(argv[i], "--shared-stdio-locks") == 0 ||
        strcmp(argv[i], "--shared-stdio-fork-unlock") == 0 ||
        strcmp(argv[i], "--shared-stdio-close-exec") == 0 ||
        strcmp(argv[i], "--after-close-exec") == 0 ||
        strcmp(argv[i], "--shared-stdio-close-both") == 0 ||
        strcmp(argv[i], "--received-from-parent") == 0 ||
        strcmp(argv[i], "--stdin-and-stdout-locks") == 0 ||
        strcmp(argv[i], "--in-flight-sender-exits=ofd") == 0 ||
        strcmp(argv[i], "--in-flight-sender-exits=flock") == 0 ||
        strcmp(argv[i], "--in-flight-sender-exits=posix") == 0 ||
        strcmp(argv[i], "--in-flight-sender-closes=ofd") == 0 ||
        strcmp(argv[i], "--in-flight-sender-closes=flock") == 0 ||
        strcmp(argv[i], "--in-flight-sender-closes=posix") == 0 ||
        strcmp(argv[i], "--in-flight-receiver-gone=exec") == 0 ||
        strcmp(argv[i], "--in-flight-receiver-gone=kills-itself") == 0 ||
        strcmp(argv[i], "--in-flight-receiver-gone=exits") == 0 ||
        strcmp(argv[i], "--in-flight-receiver-gone=killed") == 0 ||
        strcmp(argv[i], "--in-flight-receiver-gone=terminated") == 0 ||
        strcmp(argv[i], "--in-flight-receiver-gone=aborts") == 0 ||
        strcmp(argv[i], "--in-flight-receiver-gone=faults") == 0 ||
        strcmp(argv[i], "--in-flight-receiver-gone=exec-terminated") == 0 ||
        strcmp(argv[i], "--in-flight-receiver-gone=parent-keeps-socket") == 0 ||
        strcmp(argv[i], "--in-flight-receiver-dies-busy=kills-itself") == 0 ||
        strcmp(argv[i], "--in-flight-receiver-dies-busy=exit-group") == 0 ||
        strcmp(argv[i], "--in-flight-receiver-after-exec") == 0 ||
        strcmp(argv[i], "--in-flight-receiver-terminated-after-exec") == 0 ||
        strcmp(argv[i], "--stdio-alias-in-flight") == 0 ||
        strcmp(argv[i], "--joined-twin-locks") == 0 ||
        strcmp(argv[i], "--received-lock-exec") == 0 || strcmp(argv[i], "--after-exec") == 0 ||
        strcmp(argv[i], "--standard-stream-kinds") == 0) {
      mode = argv[i];
    } else if (strcmp(argv[i], "--inherited-stdin-lock=record") == 0) {
      mode = "--inherited-stdin-lock";
      inherited = 1;
    } else {
      fprintf(stderr, "unknown option %s\n", argv[i]);
      return 2;
    }
  }
  if (mode != NULL) {
    if (strcmp(mode, "--proc-locks") == 0) {
      proc_locks_rows(directory, skip_posix);
    } else if (strcmp(mode, "--twin-ofd-locks") == 0) {
      twin_ofd_locks(directory);
    } else if (strcmp(mode, "--inherited-stdin-lock") == 0) {
      inherited_stdin_lock(inherited);
    } else if (strcmp(mode, "--shared-stdio-locks") == 0) {
      shared_stdio_locks();
    } else if (strcmp(mode, "--shared-stdio-fork-unlock") == 0) {
      shared_stdio_fork_unlock(directory);
      return 0;
    } else if (strcmp(mode, "--shared-stdio-close-exec") == 0) {
      shared_stdio_close_exec(directory);
    } else if (strcmp(mode, "--after-close-exec") == 0) {
      proc_locks_snapshot("after an exec that kept standard output");
    } else if (strcmp(mode, "--shared-stdio-close-both") == 0) {
      shared_stdio_close_both(directory);
      return 0;
    } else if (strcmp(mode, "--received-from-parent") == 0) {
      received_from_parent(directory);
    } else if (strcmp(mode, "--stdin-and-stdout-locks") == 0) {
      stdin_and_stdout_locks();
    } else if (strncmp(mode, "--in-flight-sender-exits=", 25) == 0) {
      in_flight_scenario(directory, mode + 25, 1);
    } else if (strncmp(mode, "--in-flight-sender-closes=", 26) == 0) {
      in_flight_scenario(directory, mode + 26, 0);
    } else if (strncmp(mode, "--in-flight-receiver-gone=", 26) == 0) {
      receiver_gone_scenario(directory, mode + 26);
    } else if (strncmp(mode, "--in-flight-receiver-dies-busy=", 31) == 0) {
      receiver_dies_busy_scenario(directory, mode + 31);
    } else if (strcmp(mode, "--in-flight-receiver-after-exec") == 0) {
      /* The exec'd receiver: it shares its parent's standard output, so it
       * prints nothing. */
      char byte = 'r';
      return write(AFTER_EXEC_READY_FD, &byte, 1) == 1 && read(AFTER_EXEC_RELEASE_FD, &byte, 1) == 1
                 ? 0
                 : 1;
    } else if (strcmp(mode, "--in-flight-receiver-terminated-after-exec") == 0) {
      /* The exec'd receiver of exec-terminated, holding the receiving socket
       * as AFTER_EXEC_SOCKET_FD: it says it runs, waits for the description to
       * be queued to that socket, and dies by the default action of its own
       * SIGTERM. It prints nothing. */
      char byte = 'r';
      if (fcntl(AFTER_EXEC_SOCKET_FD, F_GETFD) < 0 || write(AFTER_EXEC_READY_FD, &byte, 1) != 1 ||
          read(AFTER_EXEC_RELEASE_FD, &byte, 1) != 1) {
        return 1;
      }
      struct rlimit no_core = {.rlim_cur = 0, .rlim_max = 0};
      setrlimit(RLIMIT_CORE, &no_core);
      kill(getpid(), SIGTERM);
      return 1;
    } else if (strcmp(mode, "--stdio-alias-in-flight") == 0) {
      stdio_alias_in_flight();
    } else if (strcmp(mode, "--joined-twin-locks") == 0) {
      joined_twin_locks(directory);
    } else if (strcmp(mode, "--received-lock-exec") == 0) {
      received_lock_exec(directory);
    } else if (strcmp(mode, "--after-exec") == 0) {
      proc_locks_snapshot("after an exec that kept both");
      char path[512];
      snprintf(path, sizeof(path), "%s/received", directory);
      unlink(path);
    } else {
      standard_stream_kinds(directory);
      return 0;
    }
    printf("failures=%d\n", failures);
    return 0;
  }
  pipes();
  memfds();
  eventfds();
  signalfds();
  timerfds();
  sockets();
  locks(directory, skip_flock, skip_lease, skip_posix);
  lookups();
  standard_streams();
  printf("failures=%d\n", failures);
  return 0;
}
