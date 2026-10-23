/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * A guest that passes descriptors over AF_UNIX sockets with SCM_RIGHTS, for
 * the record/replay tests in hermit-cli/tests/record_replay.rs. Replay holds
 * placeholders where the recording held received descriptors, so whatever
 * Detcore learns about a received descriptor must come from the recording.
 *
 *   pipe: within one process, sends both ends of a pipe over a socketpair.
 *         The received write end names the sender's open file, so it keeps
 *         that file's blocking state and its bytes reach the pipe; a blocking
 *         read through the received read end waits for a writer thread that
 *         writes only after a sleep. Then a forked child sends a memfd and a
 *         pipe read end it made, which are new to the parent's table.
 *
 *   stream: over connections to an abstract AF_UNIX listener, made with
 *         socket, bind, listen, connect and accept inside the guest. Six
 *         receiver threads wait in blocking recvmsg calls while the main
 *         thread sends each a pipe read end, in the reverse of their thread
 *         order, opening an eventfd after each send. The first receiver then
 *         waits in an infinite poll on another received pipe end and on its
 *         socket until the main thread writes to that pipe after a sleep. It
 *         acknowledges the completed poll and pipe read on that same socket;
 *         only then does the main thread send its goodbye for a blocking recv.
 *
 *   eventfd: sends blocking eventfds over a socketpair. Within the process,
 *         a count written through the received descriptor is read through the
 *         original and the reverse; an eventfd whose sender closed its copy
 *         before the receive, and one a forked child made, are read through
 *         the received descriptor alone. Those two name no descriptor of the
 *         receiving table, and fstat cannot tell an eventfd from another
 *         anonymous inode, so Detcore leaves them unrecorded and their reads
 *         fail with EBADF where Linux reads the count: the known gap
 *         c-programs/scm-rights-received-anon-inodes pins for plain runs.
 *         Their lines print the errno, so that record and replay must agree
 *         on it.
 *
 *   inotify: sends an inotify instance watching a directory over a
 *         socketpair, closes the original, creates a file in the directory
 *         and reads the event through the received descriptor alone. Its link
 *         names an inotify instance, which Detcore records as one; replay,
 *         which holds a stand-in eventfd in that slot, must take the link
 *         from the recording and serve the same event.
 *
 *   memfd: maps one memfd MAP_SHARED several times in one process: twice
 *         whole, once at an offset through the descriptor received over a
 *         socketpair, and once at offset 0 by a thread. A write through any
 *         mapping must be read through the others.
 *
 * Usage: record_replay_scm_rights pipe|stream|eventfd|inotify|memfd
 * Prints one line per step and exits 0 only if every step saw what Linux
 * gives a native run, the two eventfd lines that print an errno aside.
 */

#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif

#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <pthread.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/eventfd.h>
#include <sys/inotify.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/un.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define MAX_FDS 4

static int failures;

static void check(int ok, const char* what) {
  if (!ok) {
    printf("FAILED: %s (errno %s)\n", what, strerrorname_np(errno));
    failures++;
  }
}

static void sleep_ms(long ms) {
  struct timespec delay = {.tv_sec = 0, .tv_nsec = ms * 1000000L};
  nanosleep(&delay, NULL);
}

/* Sends one byte carrying `fds` in a single SCM_RIGHTS message. */
static int send_fds(int socket, const int* fds, int count) {
  char byte = 'f';
  struct iovec iov = {.iov_base = &byte, .iov_len = 1};
  union {
    struct cmsghdr header;
    char bytes[CMSG_SPACE(MAX_FDS * sizeof(int))];
  } control;
  memset(&control, 0, sizeof(control));
  struct msghdr msg = {
      .msg_iov = &iov,
      .msg_iovlen = 1,
      .msg_control = control.bytes,
      .msg_controllen = CMSG_SPACE(count * sizeof(int)),
  };
  struct cmsghdr* header = CMSG_FIRSTHDR(&msg);
  header->cmsg_level = SOL_SOCKET;
  header->cmsg_type = SCM_RIGHTS;
  header->cmsg_len = CMSG_LEN(count * sizeof(int));
  memcpy(CMSG_DATA(header), fds, count * sizeof(int));
  return sendmsg(socket, &msg, 0) == 1 ? 0 : -1;
}

/* Receives one byte and the descriptors that came with it; returns their
 * count, or -1. */
static int recv_fds(int socket, int* fds, int flags) {
  char byte;
  struct iovec iov = {.iov_base = &byte, .iov_len = 1};
  union {
    struct cmsghdr header;
    char bytes[CMSG_SPACE(MAX_FDS * sizeof(int))];
  } control;
  struct msghdr msg = {
      .msg_iov = &iov,
      .msg_iovlen = 1,
      .msg_control = control.bytes,
      .msg_controllen = sizeof(control.bytes),
  };
  if (recvmsg(socket, &msg, flags) != 1) {
    return -1;
  }
  int count = 0;
  for (struct cmsghdr* header = CMSG_FIRSTHDR(&msg); header != NULL;
       header = CMSG_NXTHDR(&msg, header)) {
    if (header->cmsg_level == SOL_SOCKET && header->cmsg_type == SCM_RIGHTS) {
      int n = (header->cmsg_len - CMSG_LEN(0)) / sizeof(int);
      for (int i = 0; i < n && count < MAX_FDS; i++) {
        memcpy(&fds[count++], CMSG_DATA(header) + i * sizeof(int), sizeof(int));
      }
    }
  }
  return count;
}

static int read_exact(int fd, char* buf, size_t len) {
  size_t filled = 0;
  while (filled < len) {
    ssize_t n = read(fd, buf + filled, len - filled);
    if (n <= 0) {
      return -1;
    }
    filled += n;
  }
  return 0;
}

static void* write_late(void* arg) {
  int fd = *(int*)arg;
  sleep_ms(10);
  check(write(fd, "late", 4) == 4, "late write");
  return NULL;
}

static void pipe_mode(void) {
  int ends[2], pair[2];
  check(pipe(ends) == 0, "pipe");
  check(socketpair(AF_UNIX, SOCK_STREAM, 0, pair) == 0, "socketpair");

  /* The write end, received within the process, names the sender's file. */
  int received[MAX_FDS];
  check(send_fds(pair[0], &ends[1], 1) == 0, "send write end");
  int count = recv_fds(pair[1], received, 0);
  check(count == 1, "receive write end");
  int write_end = received[0];
  printf(
      "pipe write end: distinct=%d nonblock=%d\n",
      write_end != ends[1],
      (fcntl(write_end, F_GETFL) & O_NONBLOCK) != 0);
  check(write(write_end, "through", 7) == 7, "write through received end");
  char buf[16] = {0};
  check(read_exact(ends[0], buf, 7) == 0, "read sent bytes");
  printf("pipe write end: read=%s\n", buf);

  /* A blocking read through a received read end waits for the writer. */
  check(send_fds(pair[0], &ends[0], 1) == 0, "send read end");
  count = recv_fds(pair[1], received, MSG_CMSG_CLOEXEC);
  check(count == 1, "receive read end");
  int read_end = received[0];
  printf(
      "pipe read end: cloexec=%d nonblock=%d\n",
      (fcntl(read_end, F_GETFD) & FD_CLOEXEC) != 0,
      (fcntl(read_end, F_GETFL) & O_NONBLOCK) != 0);
  pthread_t writer;
  check(pthread_create(&writer, NULL, write_late, &ends[1]) == 0, "spawn");
  memset(buf, 0, sizeof(buf));
  check(read_exact(read_end, buf, 4) == 0, "blocking read");
  pthread_join(writer, NULL);
  printf("pipe read end: read=%s\n", buf);

  /* Descriptors made by another process are new to this table. */
  pid_t child = fork();
  if (child == 0) {
    int memfd = memfd_create("scm_rights", 0);
    int fresh[2];
    if (memfd < 0 || pipe(fresh) != 0 || write(memfd, "memfd", 5) != 5 ||
        write(fresh[1], "child", 5) != 5) {
      _exit(1);
    }
    int sent[2] = {memfd, fresh[0]};
    _exit(send_fds(pair[0], sent, 2) == 0 ? 0 : 2);
  }
  count = recv_fds(pair[1], received, 0);
  check(count == 2, "receive from child");
  memset(buf, 0, sizeof(buf));
  check(pread(received[0], buf, 5, 0) == 5, "pread memfd");
  printf("child memfd: read=%s\n", buf);
  memset(buf, 0, sizeof(buf));
  check(read_exact(received[1], buf, 5) == 0, "read child pipe");
  printf("child pipe: read=%s\n", buf);
  int status = 0;
  check(waitpid(child, &status, 0) == child, "waitpid");
  printf("child: exit=%d\n", WIFEXITED(status) ? WEXITSTATUS(status) : -1);
}

#define RECEIVERS 6

struct stream_receiver {
  struct sockaddr_un* address;
  socklen_t address_len;
  char got;
  int polled;
};

/* Connects, waits in a blocking recvmsg for a pipe read end, and reads the
 * one byte the server left in that pipe. The first receiver then waits in an
 * infinite poll on a second received pipe end and on its socket, until the
 * server writes to that pipe after a sleep. It acknowledges the completed
 * poll and pipe read on that same socket before receiving the server's goodbye
 * in a blocking recv. */
static void* receive_stream(void* arg) {
  struct stream_receiver* receiver = arg;
  int client = socket(AF_UNIX, SOCK_STREAM, 0);
  check(client >= 0, "client socket");
  check(
      connect(client, (struct sockaddr*)receiver->address, receiver->address_len) == 0,
      "connect");
  int received[MAX_FDS];
  check(recv_fds(client, received, 0) == 1, "blocking receive of a pipe end");
  check(read(received[0], &receiver->got, 1) == 1, "read received pipe end");
  if (receiver->polled) {
    check(recv_fds(client, received, 0) == 1, "receive the polled pipe end");
    struct pollfd fds[2] = {
        {.fd = received[0], .events = POLLIN},
        {.fd = client, .events = POLLIN},
    };
    int ready = poll(fds, 2, -1);
    char buf[8] = {0};
    check(read_exact(received[0], buf, 4) == 0, "read polled pipe end");
    printf(
        "stream poll: ready=%d pipe=%d socket=%d read=%s\n",
        ready,
        (fds[0].revents & POLLIN) != 0,
        (fds[1].revents & POLLIN) != 0,
        buf);
    /* Sending in the reverse direction does not make this socket readable.
     * The server waits for this byte before it can make the socket readable. */
    check(send(client, "P", 1, 0) == 1, "send poll acknowledgement");
    memset(buf, 0, sizeof(buf));
    check(recv(client, buf, 3, MSG_WAITALL) == 3, "blocking recv");
    printf("stream socket: recv=%s\n", buf);
  }
  return NULL;
}

static void stream_mode(void) {
  struct sockaddr_un address = {.sun_family = AF_UNIX};
  /* An abstract name; the leading NUL keeps it off the file system. */
  int name_len =
      snprintf(address.sun_path + 1, sizeof(address.sun_path) - 1, "hermit-rr-scm-%d", getpid());
  socklen_t address_len = offsetof(struct sockaddr_un, sun_path) + 1 + name_len;
  int listener = socket(AF_UNIX, SOCK_STREAM, 0);
  check(listener >= 0, "socket");
  check(bind(listener, (struct sockaddr*)&address, address_len) == 0, "bind");
  check(listen(listener, RECEIVERS) == 0, "listen");

  pthread_t threads[RECEIVERS];
  struct stream_receiver receivers[RECEIVERS];
  int conns[RECEIVERS], pipes[RECEIVERS][2];
  for (int i = 0; i < RECEIVERS; i++) {
    receivers[i] = (struct stream_receiver){
        .address = &address, .address_len = address_len, .polled = i == 0};
    check(pthread_create(&threads[i], NULL, receive_stream, &receivers[i]) == 0, "spawn");
    conns[i] = accept(listener, NULL, NULL);
    check(conns[i] >= 0, "accept");
    check(pipe(pipes[i]) == 0, "pipe");
    check(write(pipes[i][1], &"abcdef"[i], 1) == 1, "fill pipe");
  }
  /* Every receiver is now waiting in its recvmsg. Each send installs a
   * descriptor in the receiver's table at once, and an eventfd opened right
   * after takes the next slot; in reverse order, the receivers' thread order
   * differs from the order of their slots. */
  sleep_ms(20);
  int events[RECEIVERS];
  for (int i = RECEIVERS - 1; i >= 0; i--) {
    check(send_fds(conns[i], &pipes[i][0], 1) == 0, "send pipe end");
    events[i] = eventfd(0, 0);
    check(events[i] >= 0, "eventfd");
  }
  int late[2];
  check(pipe(late) == 0, "late pipe");
  check(send_fds(conns[0], &late[0], 1) == 0, "send polled pipe end");
  sleep_ms(10);
  check(write(late[1], "late", 4) == 4, "late write");
  sleep_ms(10);
  char acknowledgement = 0;
  ssize_t acknowledgement_size = recv(conns[0], &acknowledgement, 1, MSG_WAITALL);
  check(acknowledgement_size == 1, "receive poll acknowledgement");
  check(acknowledgement == 'P', "poll acknowledgement content");
  if (acknowledgement_size != 1 || acknowledgement != 'P') {
    exit(1);
  }
  check(send(conns[0], "bye", 3, 0) == 3, "send goodbye");

  char got[RECEIVERS + 1] = {0};
  for (int i = 0; i < RECEIVERS; i++) {
    pthread_join(threads[i], NULL);
    got[i] = receivers[i].got;
  }
  printf("stream: received=%s\n", got);
}

static uint64_t eventfd_take(int fd) {
  uint64_t value = 0;
  check(read(fd, &value, sizeof(value)) == sizeof(value), "eventfd read");
  return value;
}

/* Reads one count through `fd`, printing it, or the errno if the read fails. */
static void eventfd_report(const char* what, int fd) {
  uint64_t value = 0;
  ssize_t got = read(fd, &value, sizeof(value));
  if (got == (ssize_t)sizeof(value)) {
    printf("%s: received read %llu\n", what, (unsigned long long)value);
  } else {
    printf("%s: received read=%zd errno=%s\n", what, got, strerrorname_np(errno));
  }
}

static void eventfd_add(int fd, uint64_t value) {
  check(write(fd, &value, sizeof(value)) == sizeof(value), "eventfd write");
}

static void eventfd_mode(void) {
  int pair[2];
  check(socketpair(AF_UNIX, SOCK_STREAM, 0, pair) == 0, "socketpair");
  int received[MAX_FDS];

  /* Within the process the received descriptor names the sender's file. */
  int original = eventfd(0, 0);
  check(original >= 0, "eventfd");
  check(send_fds(pair[0], &original, 1) == 0, "send eventfd");
  check(recv_fds(pair[1], received, 0) == 1, "receive eventfd");
  eventfd_add(received[0], 5);
  printf("eventfd alias: original read %llu\n", (unsigned long long)eventfd_take(original));
  eventfd_add(original, 7);
  printf(
      "eventfd alias: received read %llu\n", (unsigned long long)eventfd_take(received[0]));

  /* The sender's only copy is closed before the receive. */
  int closed = eventfd(9, 0);
  check(closed >= 0, "eventfd");
  check(send_fds(pair[0], &closed, 1) == 0, "send eventfd");
  close(closed);
  check(recv_fds(pair[1], received, 0) == 1, "receive closed sender's eventfd");
  eventfd_report("eventfd closed sender", received[0]);

  pid_t child = fork();
  if (child == 0) {
    int made = eventfd(3, 0);
    _exit(made >= 0 && send_fds(pair[0], &made, 1) == 0 ? 0 : 1);
  }
  check(recv_fds(pair[1], received, 0) == 1, "receive child's eventfd");
  int status = 0;
  check(waitpid(child, &status, 0) == child, "waitpid");
  printf("eventfd child: exit=%d\n", WIFEXITED(status) ? WEXITSTATUS(status) : -1);
  eventfd_report("eventfd child", received[0]);
}

static void inotify_mode(void) {
  const char* dir = "scm-inotify-dir";
  const char* file = "scm-inotify-dir/created";
  check(mkdir(dir, 0700) == 0 || errno == EEXIST, "mkdir");
  unlink(file);
  int pair[2];
  check(socketpair(AF_UNIX, SOCK_STREAM, 0, pair) == 0, "socketpair");
  int original = inotify_init1(0);
  check(original >= 0, "inotify_init1");
  check(inotify_add_watch(original, dir, IN_CREATE) >= 0, "inotify_add_watch");
  check(send_fds(pair[0], &original, 1) == 0, "send inotify");
  close(original);
  int received[MAX_FDS];
  check(recv_fds(pair[1], received, 0) == 1, "receive inotify");
  int created = open(file, O_CREAT | O_WRONLY, 0600);
  check(created >= 0, "create");
  close(created);
  /* Room for a few events: the read returns every queued one. */
  char buf[4 * (sizeof(struct inotify_event) + 16)]
      __attribute__((aligned(__alignof__(struct inotify_event))));
  ssize_t got = read(received[0], buf, sizeof(buf));
  if (got < 0) {
    printf("inotify: read=%zd errno=%s\n", got, strerrorname_np(errno));
  } else {
    int events = 0;
    const struct inotify_event* first = (const struct inotify_event*)buf;
    for (ssize_t at = 0; at < got;
         at += sizeof(struct inotify_event) + ((const struct inotify_event*)(buf + at))->len) {
      events++;
    }
    printf(
        "inotify: events=%d mask=0x%x name=%s\n",
        events,
        got > 0 ? first->mask : 0,
        got > 0 && first->len > 0 ? first->name : "");
  }
  unlink(file);
  rmdir(dir);
}

static void* map_shared(int fd, size_t len, off_t offset) {
  void* address = mmap(NULL, len, PROT_READ | PROT_WRITE, MAP_SHARED, fd, offset);
  check(address != MAP_FAILED, "mmap");
  return address;
}

static void* touch_mapping(void* arg) {
  int fd = *(int*)arg;
  char* mine = map_shared(fd, 16, 0);
  memcpy(mine + 10, "thread", 6);
  return NULL;
}

static void memfd_mode(void) {
  long page = sysconf(_SC_PAGESIZE);
  int fd = memfd_create("hermit-shared", 0);
  check(fd >= 0, "memfd_create");
  check(ftruncate(fd, 2 * page) == 0, "ftruncate");
  char* first = map_shared(fd, 2 * page, 0);
  char* second = map_shared(fd, 2 * page, 0);
  memcpy(first, "hello", 5);
  printf("memfd: second sees %.5s\n", second);
  memcpy(second + 5, "world", 5);
  printf("memfd: first sees %.10s\n", first);

  /* The received descriptor names the same file. */
  int pair[2], received[MAX_FDS];
  check(socketpair(AF_UNIX, SOCK_STREAM, 0, pair) == 0, "socketpair");
  check(send_fds(pair[0], &fd, 1) == 0, "send memfd");
  check(recv_fds(pair[1], received, 0) == 1, "receive memfd");
  char* offset = map_shared(received[0], page, page);
  memcpy(offset, "page two", 8);
  printf("memfd: offset mapping reaches %.8s\n", first + page);

  pthread_t thread;
  check(pthread_create(&thread, NULL, touch_mapping, &fd) == 0, "spawn");
  pthread_join(thread, NULL);
  printf("memfd: thread's mapping reaches %.16s\n", second);
}

int main(int argc, char** argv) {
  setvbuf(stdout, NULL, _IONBF, 0);
  if (argc != 2) {
    fprintf(stderr, "usage: %s pipe|stream|eventfd|inotify|memfd\n", argv[0]);
    return 2;
  }
  if (strcmp(argv[1], "pipe") == 0) {
    pipe_mode();
  } else if (strcmp(argv[1], "stream") == 0) {
    stream_mode();
  } else if (strcmp(argv[1], "eventfd") == 0) {
    eventfd_mode();
  } else if (strcmp(argv[1], "inotify") == 0) {
    inotify_mode();
  } else if (strcmp(argv[1], "memfd") == 0) {
    memfd_mode();
  } else {
    fprintf(stderr, "unknown mode %s\n", argv[1]);
    return 2;
  }
  return failures == 0 ? 0 : 1;
}
