/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/* Two writers on one AF_UNIX SOCK_SEQPACKET socketpair, and a reader with a
 * short buffer (socketpair(2), unix(7)).
 *
 * A child process holds one end and runs two writer threads that send at the
 * same time, with no order between them: writer A sends records of 2, 17, 64
 * and 200 bytes, writer B records of 9, 40 and 300 bytes, each with MSG_EOR,
 * and writer B also sends one record that carries the read end of a pipe in
 * an SCM_RIGHTS message. Every record starts with its tag ("A0" ... "B3") and
 * is filled with a pattern of its tag and offset.
 *
 * The parent reads every record from the other end. For each one it first
 * peeks at it with recv(MSG_PEEK | MSG_TRUNC) into a 4-byte buffer, which
 * returns the record's whole length and leaves it queued, and then reads it
 * with recvmsg into a 16-byte buffer, which returns at most 16 bytes and sets
 * MSG_TRUNC for a longer record. A record that carries a descriptor is read
 * from the received pipe end.
 *
 * Standard output does not depend on the order the two writers' records
 * arrive in: it lists each writer's records in that writer's send order with
 * what the reader saw of each (whole length, bytes returned, MSG_TRUNC, the
 * bytes' check), and checks that each writer's records arrived in its own
 * order. The arrival order itself goes to standard error. */
#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/wait.h>
#include <unistd.h>

#define REQUIRE(condition, step)                                               \
  do {                                                                        \
    if (!(condition)) {                                                       \
      fprintf(stderr, "%s failed errno=%d\n", step, errno);                    \
      _exit(1);                                                               \
    }                                                                         \
  } while (0)

enum { SHORT = 16, PEEK = 4, MAX_RECORD = 300 };

struct writer {
  char name;
  const size_t *sizes;
  size_t count;
  int fd;
  int rights; /* a pipe read end to send after the records, or -1 */
};

static const size_t a_sizes[] = {2, 17, 64, 200};
static const size_t b_sizes[] = {9, 40, 300};
static const char rights_payload[] = "through the passed pipe";

static unsigned char fill(char name, size_t index, size_t offset) {
  return (unsigned char)('a' + (name - 'A') * 7 + (index * 3 + offset) % 19);
}

static void make_record(unsigned char *record, char name, size_t index,
                        size_t size) {
  for (size_t offset = 0; offset < size; offset++) {
    record[offset] = fill(name, index, offset);
  }
  /* The tag, as much of it as fits. */
  record[0] = (unsigned char)name;
  if (size > 1) record[1] = (unsigned char)('0' + index);
}

static void *write_records(void *argument) {
  const struct writer *writer = argument;
  unsigned char record[MAX_RECORD];
  for (size_t index = 0; index < writer->count; index++) {
    size_t size = writer->sizes[index];
    make_record(record, writer->name, index, size);
    REQUIRE(send(writer->fd, record, size, MSG_EOR) == (ssize_t)size, "send");
  }
  if (writer->rights >= 0) {
    unsigned char record[2] = {(unsigned char)writer->name,
                               (unsigned char)('0' + writer->count)};
    struct iovec iov = {.iov_base = record, .iov_len = sizeof(record)};
    union {
      char buffer[CMSG_SPACE(sizeof(int))];
      struct cmsghdr align;
    } control;
    memset(&control, 0, sizeof(control));
    struct msghdr message = {.msg_iov = &iov,
                             .msg_iovlen = 1,
                             .msg_control = control.buffer,
                             .msg_controllen = sizeof(control.buffer)};
    struct cmsghdr *header = CMSG_FIRSTHDR(&message);
    header->cmsg_level = SOL_SOCKET;
    header->cmsg_type = SCM_RIGHTS;
    header->cmsg_len = CMSG_LEN(sizeof(int));
    memcpy(CMSG_DATA(header), &writer->rights, sizeof(int));
    REQUIRE(sendmsg(writer->fd, &message, MSG_EOR) == (ssize_t)sizeof(record),
            "sendmsg rights");
  }
  return NULL;
}

/* What the reader saw of one record. */
struct seen {
  char tag[3];
  ssize_t whole;    /* from the MSG_PEEK | MSG_TRUNC receive */
  ssize_t returned; /* from the short recvmsg */
  int truncated;    /* MSG_TRUNC in its msg_flags */
  int bytes_ok;     /* the returned bytes are the record's first ones */
  int fd;           /* a received descriptor, or -1 */
};

static int check_bytes(const unsigned char *bytes, ssize_t returned) {
  if (returned < 1) return 0;
  char name = (char)bytes[0];
  if (name != 'A' && name != 'B') return 0;
  size_t index = returned > 1 ? (size_t)(bytes[1] - '0') : 0;
  for (ssize_t offset = 2; offset < returned; offset++) {
    if (bytes[offset] != fill(name, index, (size_t)offset)) return 0;
  }
  return 1;
}

int main(void) {
  int pair[2];
  REQUIRE(socketpair(AF_UNIX, SOCK_SEQPACKET | SOCK_CLOEXEC, 0, pair) == 0,
          "socketpair");
  int pipe_fds[2];
  REQUIRE(pipe(pipe_fds) == 0, "pipe");
  fflush(stdout);
  pid_t child = fork();
  REQUIRE(child >= 0, "fork");
  if (child == 0) {
    close(pair[0]);
    REQUIRE(write(pipe_fds[1], rights_payload, sizeof(rights_payload)) ==
                (ssize_t)sizeof(rights_payload),
            "write pipe");
    close(pipe_fds[1]);
    struct writer a = {'A', a_sizes, 4, pair[1], -1};
    struct writer b = {'B', b_sizes, 3, pair[1], pipe_fds[0]};
    pthread_t thread_a, thread_b;
    REQUIRE(pthread_create(&thread_a, NULL, write_records, &a) == 0,
            "pthread_create A");
    REQUIRE(pthread_create(&thread_b, NULL, write_records, &b) == 0,
            "pthread_create B");
    REQUIRE(pthread_join(thread_a, NULL) == 0, "join A");
    REQUIRE(pthread_join(thread_b, NULL) == 0, "join B");
    _exit(0);
  }
  close(pair[1]);
  close(pipe_fds[0]);
  close(pipe_fds[1]);

  enum { RECORDS = 8 };
  struct seen seen[RECORDS];
  for (int i = 0; i < RECORDS; i++) {
    struct seen *record = &seen[i];
    memset(record, 0, sizeof(*record));
    record->fd = -1;
    unsigned char peek[PEEK];
    record->whole = recv(pair[0], peek, sizeof(peek), MSG_PEEK | MSG_TRUNC);
    REQUIRE(record->whole > 0, "recv MSG_PEEK|MSG_TRUNC");
    unsigned char bytes[SHORT];
    struct iovec iov = {.iov_base = bytes, .iov_len = sizeof(bytes)};
    union {
      char buffer[CMSG_SPACE(sizeof(int))];
      struct cmsghdr align;
    } control;
    memset(&control, 0, sizeof(control));
    struct msghdr message = {.msg_iov = &iov,
                             .msg_iovlen = 1,
                             .msg_control = control.buffer,
                             .msg_controllen = sizeof(control.buffer)};
    record->returned = recvmsg(pair[0], &message, MSG_CMSG_CLOEXEC);
    REQUIRE(record->returned > 0, "recvmsg");
    record->truncated = (message.msg_flags & MSG_TRUNC) != 0;
    record->bytes_ok = check_bytes(bytes, record->returned) &&
                       memcmp(peek, bytes, (size_t)(record->returned < PEEK
                                                        ? record->returned
                                                        : PEEK)) == 0;
    record->tag[0] = (char)bytes[0];
    record->tag[1] = record->returned > 1 ? (char)bytes[1] : '-';
    for (struct cmsghdr *header = CMSG_FIRSTHDR(&message); header != NULL;
         header = CMSG_NXTHDR(&message, header)) {
      if (header->cmsg_level == SOL_SOCKET && header->cmsg_type == SCM_RIGHTS) {
        memcpy(&record->fd, CMSG_DATA(header), sizeof(int));
      }
    }
  }
  /* The writers have sent everything: the next receive sees the end. */
  char end;
  ssize_t eof = recv(pair[0], &end, 1, 0);
  int status;
  REQUIRE(waitpid(child, &status, 0) == child, "waitpid");

  fprintf(stderr, "arrival order:");
  for (int i = 0; i < RECORDS; i++) fprintf(stderr, " %s", seen[i].tag);
  fprintf(stderr, "\n");

  int ordered = 1;
  for (char name = 'A'; name <= 'B'; name++) {
    int next = 0;
    for (int i = 0; i < RECORDS; i++) {
      const struct seen *record = &seen[i];
      if (record->tag[0] != name) continue;
      if (record->tag[1] != '0' + next) ordered = 0;
      next++;
      printf("%s: whole %zd, returned %zd%s, bytes %s", record->tag,
             record->whole, record->returned,
             record->truncated ? ", MSG_TRUNC" : "",
             record->bytes_ok ? "ok" : "WRONG");
      if (record->fd >= 0) {
        char payload[64] = {0};
        ssize_t got = read(record->fd, payload, sizeof(payload) - 1);
        printf(", passed descriptor reads \"%s\" (%zd bytes)",
               got > 0 ? payload : "", got);
        close(record->fd);
      }
      printf("\n");
    }
  }
  printf("each writer's records arrived in its own order: %s\n",
         ordered ? "yes" : "NO");
  printf("after the writers: recv returns %zd; writer process exited %d\n", eof,
         WIFEXITED(status) ? WEXITSTATUS(status) : -1);
  return ordered && eof == 0 ? 0 : 1;
}
