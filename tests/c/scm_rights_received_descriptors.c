/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Descriptors a process receives over SCM_RIGHTS, used as Linux allows.
 *
 * A child creates a regular file descriptor (read 3 bytes in), both ends of
 * a pipe, both ends of a socketpair, a write-only append
 * descriptor and three aliases of another file, sends them to its parent
 * over a datagram socketpair and exits. Only then does the parent receive
 * them, so no sender still holds any of them: with recvmsg, with
 * MSG_CMSG_CLOEXEC, with recvmmsg (two messages in one call), into a control
 * buffer with room for one of three descriptors (MSG_CTRUNC: Linux installs
 * the one that fits and closes the others) and into one with room for none.
 *
 * For each received descriptor the parent prints its file type, F_GETFD and
 * F_GETFL, and uses it: reads on from the sender's file offset, writes
 * through the pipe and the socketpair,
 * duplicates it and moves the shared offset through the duplicate. An
 * exec'd image then reports which received descriptors survived exec
 * (FD_CLOEXEC from MSG_CMSG_CLOEXEC closes them; skipped with --no-exec, for
 * backends that cannot exec). Finally each is closed and shown closed.
 *
 * Standard output is the oracle; the program exits 0 when every check
 * matches Linux's answer and prints failures=N otherwise. Descriptor numbers
 * are never printed, only counts, so a backend that holds descriptors of its
 * own in the guest prints the same lines.
 */

#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif

#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>

#define MAX_FDS 4

static int failures = 0;

static void die(const char* what) {
  printf("FATAL %s errno=%d\n", what, errno);
  fflush(stdout);
  exit(EXIT_FAILURE);
}

static void check(int ok, const char* what) {
  if (!ok) {
    failures++;
    printf("FAIL %s\n", what);
  }
}

static const char* kind_of(int fd) {
  struct stat st;
  if (fstat(fd, &st) != 0) {
    return "fstat-failed";
  }
  switch (st.st_mode & S_IFMT) {
    case S_IFREG:
      return "regular";
    case S_IFIFO:
      return "fifo";
    case S_IFSOCK:
      return "socket";
    case 0:
      return "anon-inode";
    default:
      return "other";
  }
}

static const char* access_of(int flags) {
  switch (flags & O_ACCMODE) {
    case O_RDONLY:
      return "rdonly";
    case O_WRONLY:
      return "wronly";
    case O_RDWR:
      return "rdwr";
    default:
      return "?";
  }
}

/* One line of facts about `fd`; returns its FD_CLOEXEC bit. */
static int describe(const char* label, int fd) {
  int fd_flags = fcntl(fd, F_GETFD);
  int status = fcntl(fd, F_GETFL);
  if (fd_flags < 0 || status < 0) {
    printf(
        "%s F_GETFD=%d F_GETFL=%d errno=%d\n", label, fd_flags, status, errno);
    failures++;
    return -1;
  }
  printf(
      "%s kind=%s cloexec=%d access=%s nonblock=%d append=%d\n",
      label,
      kind_of(fd),
      (fd_flags & FD_CLOEXEC) != 0,
      access_of(status),
      (status & O_NONBLOCK) != 0,
      (status & O_APPEND) != 0);
  return (fd_flags & FD_CLOEXEC) != 0;
}

static int open_count(void) {
  int count = 0;
  for (int fd = 0; fd < 256; fd++) {
    if (fcntl(fd, F_GETFD) >= 0) {
      count++;
    }
  }
  return count;
}

static void send_fds(int sock, const int* fds, int count) {
  char byte = 'm';
  struct iovec iov = {.iov_base = &byte, .iov_len = 1};
  union {
    char buffer[CMSG_SPACE(sizeof(int) * MAX_FDS)];
    struct cmsghdr align;
  } control;
  memset(&control, 0, sizeof(control));
  struct msghdr message = {
      .msg_iov = &iov,
      .msg_iovlen = 1,
      .msg_control = control.buffer,
      .msg_controllen = CMSG_SPACE(sizeof(int) * (size_t)count),
  };
  struct cmsghdr* header = CMSG_FIRSTHDR(&message);
  header->cmsg_level = SOL_SOCKET;
  header->cmsg_type = SCM_RIGHTS;
  header->cmsg_len = CMSG_LEN(sizeof(int) * (size_t)count);
  memcpy(CMSG_DATA(header), fds, sizeof(int) * (size_t)count);
  if (sendmsg(sock, &message, 0) != 1) {
    die("sendmsg");
  }
}

/* The descriptors one received message carries, and whether it was cut. */
struct received {
  int fds[MAX_FDS];
  int count;
  int truncated;
};

static void parse(struct msghdr* message, struct received* out) {
  out->count = 0;
  out->truncated = (message->msg_flags & MSG_CTRUNC) != 0;
  for (struct cmsghdr* header = CMSG_FIRSTHDR(message); header != NULL;
       header = CMSG_NXTHDR(message, header)) {
    if (header->cmsg_level != SOL_SOCKET || header->cmsg_type != SCM_RIGHTS) {
      continue;
    }
    size_t n = (header->cmsg_len - CMSG_LEN(0)) / sizeof(int);
    for (size_t i = 0; i < n && out->count < MAX_FDS; i++) {
      memcpy(
          &out->fds[out->count++],
          CMSG_DATA(header) + i * sizeof(int),
          sizeof(int));
    }
  }
}

/* recvmsg into a control buffer of `room` bytes. */
static void receive(int sock, int flags, size_t room, struct received* out) {
  char byte = 0;
  struct iovec iov = {.iov_base = &byte, .iov_len = 1};
  union {
    char buffer[CMSG_SPACE(sizeof(int) * MAX_FDS)];
    struct cmsghdr align;
  } control;
  memset(&control, 0, sizeof(control));
  struct msghdr message = {
      .msg_iov = &iov,
      .msg_iovlen = 1,
      .msg_control = control.buffer,
      .msg_controllen = room,
  };
  if (recvmsg(sock, &message, flags) != 1 || byte != 'm') {
    die("recvmsg");
  }
  parse(&message, out);
}

static void write_file(const char* path, const char* text) {
  int fd = open(path, O_WRONLY | O_CREAT | O_TRUNC, 0600);
  if (fd < 0 || write(fd, text, strlen(text)) != (ssize_t)strlen(text) ||
      close(fd) != 0) {
    die("write_file");
  }
}

static void sender(int sock, const char* data, const char* log) {
  int file = open(data, O_RDONLY);
  char skip[3];
  if (file < 0 || read(file, skip, sizeof(skip)) != 3) {
    die("sender file");
  }
  send_fds(sock, &file, 1);

  int pipe_fds[2];
  if (pipe(pipe_fds) != 0) {
    die("pipe");
  }
  send_fds(sock, pipe_fds, 2);


  int pair[2];
  if (socketpair(AF_UNIX, SOCK_STREAM, 0, pair) != 0) {
    die("inner socketpair");
  }
  send_fds(sock, pair, 2);

  int append = open(log, O_WRONLY | O_APPEND);
  if (append < 0) {
    die("sender log");
  }
  send_fds(sock, &append, 1);

  int three[3];
  for (int i = 0; i < 3; i++) {
    three[i] = open(data, O_RDONLY);
    if (three[i] < 0) {
      die("sender three");
    }
  }
  send_fds(sock, three, 3);
  send_fds(sock, &file, 1);
  _exit(0);
}

/* Send `fd` to this process over a fresh socketpair and return what
 * arrives, received with `flags` (MSG_PEEK first when `peek_too`, returning
 * the peeked descriptor through `*peeked`). */
static int self_send(int fd, int flags, int peek_too, int* peeked) {
  int pair[2];
  if (socketpair(AF_UNIX, SOCK_DGRAM, 0, pair) != 0) {
    die("self socketpair");
  }
  send_fds(pair[0], &fd, 1);
  struct received got;
  if (peek_too) {
    receive(pair[1], MSG_PEEK, CMSG_SPACE(sizeof(int)), &got);
    *peeked = got.count == 1 ? got.fds[0] : -1;
  }
  receive(pair[1], flags, CMSG_SPACE(sizeof(int)), &got);
  close(pair[0]);
  close(pair[1]);
  return got.count == 1 ? got.fds[0] : -1;
}

static int nonblocking(int fd) {
  int status = fcntl(fd, F_GETFL);
  return status < 0 ? -1 : (status & O_NONBLOCK) != 0;
}

/* Descriptors this process sends to itself while it keeps the original:
 * the received one is an alias of a description it already holds. */
static void self_aliases(const char* dir) {
  /* A pipe end and a socketpair end. */
  int fds[2];
  if (pipe(fds) != 0) {
    die("self pipe");
  }
  int pipe_alias = self_send(fds[0], 0, 0, NULL);
  ssize_t put = write(fds[1], "self", 4);
  char text[16] = {0};
  ssize_t got = read(pipe_alias, text, sizeof(text) - 1);
  printf(
      "self pipe-read kind=%s write=%zd read=%zd text=%.*s nonblock=%d\n",
      kind_of(pipe_alias),
      put,
      got,
      (int)(got > 0 ? got : 0),
      text,
      nonblocking(pipe_alias));
  check(put == 4 && got == 4 && memcmp(text, "self", 4) == 0, "self pipe");
  close(pipe_alias);
  close(fds[0]);
  close(fds[1]);

  int pair[2];
  if (socketpair(AF_UNIX, SOCK_STREAM, 0, pair) != 0) {
    die("self stream pair");
  }
  int socket_alias = self_send(pair[0], 0, 0, NULL);
  put = write(socket_alias, "echo", 4);
  memset(text, 0, sizeof(text));
  got = read(pair[1], text, sizeof(text) - 1);
  printf(
      "self socket kind=%s write=%zd read=%zd text=%.*s\n",
      kind_of(socket_alias),
      put,
      got,
      (int)(got > 0 ? got : 0),
      text);
  check(put == 4 && got == 4 && memcmp(text, "echo", 4) == 0, "self socket");
  close(socket_alias);
  close(pair[0]);
  close(pair[1]);

  /* A directory read partly through the original: the alias shares its
   * position, so together they miss no entry. */
  char sub[256];
  snprintf(sub, sizeof(sub), "%s/listing", dir);
  if (mkdir(sub, 0700) != 0) {
    die("mkdir listing");
  }
  const int entries = 40;
  for (int i = 0; i < entries; i++) {
    char path[300];
    snprintf(path, sizeof(path), "%s/entry-%02d", sub, i);
    int fd = open(path, O_WRONLY | O_CREAT, 0600);
    if (fd < 0) {
      die("create entry");
    }
    close(fd);
  }
  int directory = open(sub, O_RDONLY | O_DIRECTORY);
  if (directory < 0) {
    die("open listing");
  }
  int seen[64] = {0};
  char records[256];
  long n = syscall(SYS_getdents64, directory, records, sizeof(records));
  int directory_alias = self_send(directory, 0, 0, NULL);
  char big[8192];
  int pass = 0;
  for (int fd = directory; pass < 2; fd = directory_alias, pass++) {
    for (;;) {
      char* buffer = pass == 0 ? records : big;
      if (pass == 1) {
        n = syscall(SYS_getdents64, fd, big, sizeof(big));
      }
      if (n <= 0) {
        break;
      }
      for (long offset = 0; offset < n;) {
        unsigned short length;
        memcpy(&length, buffer + offset + 16, sizeof(length));
        const char* entry_name = buffer + offset + 19;
        int index;
        if (sscanf(entry_name, "entry-%d", &index) == 1 && index >= 0 &&
            index < entries) {
          seen[index] = 1;
        }
        offset += length;
      }
      if (pass == 0) {
        break;
      }
    }
  }
  int missed = 0;
  for (int i = 0; i < entries; i++) {
    missed += !seen[i];
  }
  printf("self directory kind=dir missed=%d\n", missed);
  check(missed == 0, "self directory");
  close(directory_alias);
  close(directory);

  /* A regular file whose O_NONBLOCK is set through the original after the
   * alias arrived: Linux shares the status flags. */
  char path[300];
  snprintf(path, sizeof(path), "%s/flags", dir);
  int file = open(path, O_RDWR | O_CREAT, 0600);
  if (file < 0) {
    die("open flags file");
  }
  int file_alias = self_send(file, 0, 0, NULL);
  int before = nonblocking(file_alias);
  if (fcntl(file, F_SETFL, O_NONBLOCK) != 0) {
    die("F_SETFL");
  }
  printf(
      "self file nonblock-before=%d original=%d alias-after=%d\n",
      before,
      nonblocking(file),
      nonblocking(file_alias));
  close(file_alias);
  close(file);
  unlink(path);

  /* A directory received at offset zero, read through the received
   * descriptor alone with a small buffer: the order its entries come in. */
  int fresh = open(sub, O_RDONLY | O_DIRECTORY);
  if (fresh < 0) {
    die("open fresh listing");
  }
  int fresh_alias = self_send(fresh, 0, 0, NULL);
  close(fresh);
  char small[64];
  int order[64];
  int listed = 0;
  int calls = 0;
  for (;;) {
    long got_bytes = syscall(SYS_getdents64, fresh_alias, small, sizeof(small));
    if (got_bytes <= 0) {
      break;
    }
    calls++;
    for (long offset = 0; offset < got_bytes;) {
      unsigned short length;
      memcpy(&length, small + offset + 16, sizeof(length));
      int index;
      if (sscanf(small + offset + 19, "entry-%d", &index) == 1 && listed < 64) {
        order[listed++] = index;
      }
      offset += length;
    }
  }
  int sorted = 1;
  for (int i = 1; i < listed; i++) {
    sorted &= order[i - 1] < order[i];
  }
  printf(
      "received directory at offset 0: entries=%d small-buffer-calls=%d sorted=%d\n",
      listed,
      calls,
      sorted);
  close(fresh_alias);

  /* MSG_PEEK installs the descriptors too: a second, distinct descriptor. */
  int peeked = -1;
  int peek_source = open(dir, O_RDONLY | O_DIRECTORY);
  int received = self_send(peek_source, 0, 1, &peeked);
  int peek_works = nonblocking(peeked) >= 0;
  close(peeked);
  printf(
      "peek installed=%d distinct=%d peeked-usable=%d received-usable-after-closing-peeked=%d\n",
      peeked >= 0,
      peeked != received,
      peek_works,
      nonblocking(received) >= 0);
  check(peeked >= 0 && peeked != received && peek_works, "peek");
  close(received);
  close(peek_source);

  for (int i = 0; i < entries; i++) {
    char entry[300];
    snprintf(entry, sizeof(entry), "%s/entry-%02d", sub, i);
    unlink(entry);
  }
  rmdir(sub);
}

/*
 * Run this program again in a forked child with --exec-check, naming each
 * labelled descriptor, and wait for it: the exec'd image prints which of them
 * survived exec.
 */
static void check_exec_survivors(
    const char* self,
    const char* const* labels,
    const int* fds,
    int count) {
  char numbers[8][16];
  char* exec_argv[2 + 16 + 1];
  exec_argv[0] = (char*)self;
  exec_argv[1] = "--exec-check";
  for (int i = 0; i < count; i++) {
    snprintf(numbers[i], sizeof(numbers[i]), "%d", fds[i]);
    exec_argv[2 + 2 * i] = (char*)labels[i];
    exec_argv[3 + 2 * i] = numbers[i];
  }
  exec_argv[2 + 2 * count] = NULL;
  pid_t checker = fork();
  if (checker < 0) {
    die("fork checker");
  }
  if (checker == 0) {
    execv(self, exec_argv);
    die("execv");
  }
  int status = 0;
  if (waitpid(checker, &status, 0) != checker || !WIFEXITED(status) ||
      WEXITSTATUS(status) != 0) {
    die("checker exit");
  }
}

static int exec_check(int argc, char** argv) {
  /* argv: self --exec-check label fd label fd ... */
  for (int i = 2; i + 1 < argc; i += 2) {
    int fd = atoi(argv[i + 1]);
    printf(
        "after-exec %s %s\n",
        argv[i],
        fcntl(fd, F_GETFD) >= 0 ? "open" : "closed");
  }
  return 0;
}

int main(int argc, char** argv) {
  if (argc > 1 && strcmp(argv[1], "--exec-check") == 0) {
    return exec_check(argc, argv);
  }
  setvbuf(stdout, NULL, _IONBF, 0);

  char dir[] = "/tmp/scm-rights-receivedXXXXXX";
  if (mkdtemp(dir) == NULL) {
    die("mkdtemp");
  }
  char data[sizeof(dir) + 16];
  char log[sizeof(dir) + 16];
  snprintf(data, sizeof(data), "%s/data", dir);
  snprintf(log, sizeof(log), "%s/log", dir);
  write_file(data, "received-file\n");
  write_file(log, "first\n");

  int channel[2];
  if (socketpair(AF_UNIX, SOCK_DGRAM, 0, channel) != 0) {
    die("socketpair");
  }
  pid_t child = fork();
  if (child < 0) {
    die("fork");
  }
  if (child == 0) {
    close(channel[0]);
    sender(channel[1], data, log);
  }
  close(channel[1]);
  int status = 0;
  if (waitpid(child, &status, 0) != child || !WIFEXITED(status) ||
      WEXITSTATUS(status) != 0) {
    die("sender exit");
  }
  printf("sender exited before any receipt\n");

  /* 1. A regular file at the sender's offset, plain recvmsg. */
  struct received file;
  receive(channel[0], 0, CMSG_SPACE(sizeof(int)), &file);
  check(file.count == 1 && !file.truncated, "file count");
  describe("file", file.fds[0]);
  char text[32] = {0};
  ssize_t got = read(file.fds[0], text, sizeof(text) - 1);
  int from_offset = got == 11 && memcmp(text, "eived-file\n", 11) == 0;
  printf("file read=%zd from-sender-offset=%d\n", got, from_offset);
  check(from_offset, "file read");
  int file_dup = dup(file.fds[0]);
  check(file_dup >= 0, "file dup");
  describe("file-dup", file_dup);
  off_t rewound = lseek(file_dup, 0, SEEK_SET);
  memset(text, 0, sizeof(text));
  got = read(file.fds[0], text, 4);
  printf(
      "file dup-rewind=%lld shared-read=%zd text=%.*s\n",
      (long long)rewound,
      got,
      (int)(got > 0 ? got : 0),
      text);
  check(rewound == 0 && got == 4 && memcmp(text, "rece", 4) == 0, "file dup");

  /* 2. Both ends of a pipe, with MSG_CMSG_CLOEXEC. */
  struct received pipe_ends;
  receive(
      channel[0], MSG_CMSG_CLOEXEC, CMSG_SPACE(sizeof(int) * 2), &pipe_ends);
  check(pipe_ends.count == 2 && !pipe_ends.truncated, "pipe count");
  int read_cloexec = describe("pipe-read", pipe_ends.fds[0]);
  int write_cloexec = describe("pipe-write", pipe_ends.fds[1]);
  check(read_cloexec == 1 && write_cloexec == 1, "pipe cloexec");
  ssize_t put = write(pipe_ends.fds[1], "ping", 4);
  memset(text, 0, sizeof(text));
  got = read(pipe_ends.fds[0], text, sizeof(text) - 1);
  printf(
      "pipe write=%zd read=%zd text=%.*s\n",
      put,
      got,
      (int)(got > 0 ? got : 0),
      text);
  check(put == 4 && got == 4 && memcmp(text, "ping", 4) == 0, "pipe io");


  /* 4. Two messages in one recvmmsg: a socketpair and an append file. */
  char bytes[2] = {0, 0};
  struct iovec iovs[2] = {
      {.iov_base = &bytes[0], .iov_len = 1},
      {.iov_base = &bytes[1], .iov_len = 1},
  };
  union {
    char buffer[CMSG_SPACE(sizeof(int) * MAX_FDS)];
    struct cmsghdr align;
  } controls[2];
  memset(controls, 0, sizeof(controls));
  struct mmsghdr messages[2];
  memset(messages, 0, sizeof(messages));
  for (int i = 0; i < 2; i++) {
    messages[i].msg_hdr.msg_iov = &iovs[i];
    messages[i].msg_hdr.msg_iovlen = 1;
    messages[i].msg_hdr.msg_control = controls[i].buffer;
    messages[i].msg_hdr.msg_controllen = sizeof(controls[i].buffer);
  }
  int batch = recvmmsg(channel[0], messages, 2, MSG_CMSG_CLOEXEC, NULL);
  printf("recvmmsg messages=%d\n", batch);
  if (batch != 2) {
    die("recvmmsg");
  }
  struct received pair;
  struct received append;
  parse(&messages[0].msg_hdr, &pair);
  parse(&messages[1].msg_hdr, &append);
  check(pair.count == 2 && append.count == 1, "recvmmsg counts");
  describe("socket-a", pair.fds[0]);
  describe("socket-b", pair.fds[1]);
  put = write(pair.fds[0], "pong", 4);
  memset(text, 0, sizeof(text));
  got = read(pair.fds[1], text, sizeof(text) - 1);
  printf(
      "socket write=%zd read=%zd text=%.*s\n",
      put,
      got,
      (int)(got > 0 ? got : 0),
      text);
  check(put == 4 && got == 4 && memcmp(text, "pong", 4) == 0, "socket io");
  describe("append", append.fds[0]);
  put = write(append.fds[0], "second\n", 7);
  int log_fd = open(log, O_RDONLY);
  memset(text, 0, sizeof(text));
  got = read(log_fd, text, sizeof(text) - 1);
  close(log_fd);
  printf("append write=%zd log=%zd\n", put, got);
  check(
      put == 7 && got == 13 && memcmp(text, "first\nsecond\n", 13) == 0,
      "append io");

  /* 5. Room for one of three descriptors: MSG_CTRUNC, one installed. */
  int before = open_count();
  struct received cut;
  receive(channel[0], 0, CMSG_LEN(sizeof(int)), &cut);
  int after = open_count();
  printf(
      "truncated received=%d ctrunc=%d opened=%d\n",
      cut.count,
      cut.truncated,
      after - before);
  check(cut.count == 1 && cut.truncated && after - before == 1, "truncated");
  describe("truncated-kept", cut.fds[0]);

  /* 6. Room for no descriptor: MSG_CTRUNC, none installed. */
  before = open_count();
  struct received none;
  receive(channel[0], 0, CMSG_LEN(0), &none);
  after = open_count();
  printf(
      "no-room received=%d ctrunc=%d opened=%d\n",
      none.count,
      none.truncated,
      after - before);
  check(none.count == 0 && none.truncated && after == before, "no room");

  /* 6b. Aliases this process sends to itself, and a peeked receipt. */
  self_aliases(dir);

  /* 7. Which received descriptors survive exec. In-guest LiteInst cannot
   * replace a process image, so its cells pass --no-exec. */
  const char* labels[7] = {
      "file",
      "file-dup",
      "pipe-read",
      "pipe-write",
      "socket-a",
      "append",
      "truncated-kept"};
  int fds[7] = {
      file.fds[0],
      file_dup,
      pipe_ends.fds[0],
      pipe_ends.fds[1],
      pair.fds[0],
      append.fds[0],
      cut.fds[0]};
  if (argc > 1 && strcmp(argv[1], "--no-exec") == 0) {
    printf("after-exec not checked (--no-exec)\n");
  } else {
    check_exec_survivors(argv[0], labels, fds, 7);
  }

  /* 8. Close each one; it is gone. */
  int all[8] = {
      file.fds[0],
      file_dup,
      pipe_ends.fds[0],
      pipe_ends.fds[1],
      pair.fds[0],
      pair.fds[1],
      append.fds[0],
      cut.fds[0]};
  int closed = 0;
  for (int i = 0; i < 8; i++) {
    if (close(all[i]) == 0 && fcntl(all[i], F_GETFD) < 0 && errno == EBADF) {
      closed++;
    }
  }
  printf("closed=%d of 8\n", closed);
  check(closed == 8, "close");

  unlink(data);
  unlink(log);
  rmdir(dir);
  printf("failures=%d\n", failures);
  return failures == 0 ? 0 : 1;
}
