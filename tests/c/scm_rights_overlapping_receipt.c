/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// An SCM_RIGHTS receive whose 8-byte payload lands on the msg_name field of
// its own msghdr, with its control buffer elsewhere. It receives one end of a
// datagram socketpair over another socketpair; then two threads send a record
// each through the received descriptor, and the main thread reads both from
// the other end. Linux takes the msghdr's fields before it writes the payload,
// so the receive is valid: the payload replaces msg_name, the descriptor is a
// usable alias, and the program prints four lines and exits 0.
//
// Hermit cannot record that descriptor: the receive writes over a header it
// reads, so Detcore does not trust the control data it reads back after the
// call. A later call through an unrecorded descriptor would run outside the
// turn order, so a fail-closed run ends at the receive as a named policy
// refusal, before "received" is printed.
//
// argv[1] chooses how endpoint[0] is sent, and so what Detcore sees of the
// send: by default sendmsg, which it reads plainly. With "sendmmsg-efault", a
// sendmmsg of one entry whose mmsghdr lies on a read-only page: Linux sends
// the message, then cannot write its msg_len back and fails with EFAULT,
// counting nothing sent. With "sendmmsg-partial", a sendmmsg of a data-only
// entry and then that one, the second at the start of a read-only page: Linux
// sends both, writes only the first msg_len and returns 1; the data-only
// message is read first. With "sendmmsg-alias", a sendmmsg of one entry whose
// control buffer starts 8 bytes before the entry's msg_len, so the msg_len
// Linux writes back after sending lands on the control message's cmsg_level.
// Every way Linux sent the descriptor, and the receive takes it as before.

#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <stddef.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/types.h>
#include <unistd.h>

static int received_fd = -1;

static void fail(const char* what) {
  perror(what);
  exit(1);
}

static void* send_record(void* text) {
  size_t length = strlen(text);
  if (send(received_fd, text, length, 0) != (ssize_t)length) {
    fail("send through the received descriptor");
  }
  return NULL;
}

// Sends `sent` as the one entry of a sendmmsg whose mmsghdr is on a
// read-only page, which must fail with EFAULT after sending.
static void send_by_faulting_sendmmsg(int socket, const struct msghdr* sent) {
  long page = sysconf(_SC_PAGESIZE);
  struct mmsghdr* entry = mmap(
      NULL, page, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  if (entry == MAP_FAILED) {
    fail("mmap");
  }
  memset(entry, 0, sizeof *entry);
  entry->msg_hdr = *sent;
  if (mprotect(entry, page, PROT_READ) != 0) {
    fail("mprotect");
  }
  errno = 0;
  int result = sendmmsg(socket, entry, 1, 0);
  if (result != -1 || errno != EFAULT) {
    fprintf(stderr, "sendmmsg returned %d (errno %d), not EFAULT\n", result,
            errno);
    exit(1);
  }
  printf("sent by a sendmmsg that failed with EFAULT\n");
}

// The data-only message send_by_partly_faulting_sendmmsg sends first.
static const char data_only[] = "data!";

// Sends a data-only entry and then `sent` as the two entries of a sendmmsg
// whose second entry starts a read-only page: Linux sends both, writes the
// first entry's msg_len, cannot write the second's, and returns 1.
static void send_by_partly_faulting_sendmmsg(int socket, const struct msghdr* sent) {
  long page = sysconf(_SC_PAGESIZE);
  char* pages = mmap(NULL, 2 * page, PROT_READ | PROT_WRITE,
                     MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  if (pages == MAP_FAILED) {
    fail("mmap");
  }
  struct mmsghdr* entries = (struct mmsghdr*)(pages + page - sizeof(struct mmsghdr));
  memset(entries, 0, 2 * sizeof *entries);
  struct iovec data = {.iov_base = (void*)data_only, .iov_len = strlen(data_only)};
  entries[0].msg_hdr.msg_iov = &data;
  entries[0].msg_hdr.msg_iovlen = 1;
  entries[1].msg_hdr = *sent;
  if (mprotect(pages + page, page, PROT_READ) != 0) {
    fail("mprotect");
  }
  int result = sendmmsg(socket, entries, 2, 0);
  if (result != 1 || entries[0].msg_len != data.iov_len) {
    fprintf(stderr, "sendmmsg returned %d (errno %d), msg_len %u, not 1\n",
            result, errno, entries[0].msg_len);
    exit(1);
  }
  printf("sent by a sendmmsg that counted only the message before it\n");
}

// Sends `payload` and `fd` as the one entry of a sendmmsg whose control
// buffer starts 8 bytes before the entry's msg_len, at its cmsg_level.
static void send_by_aliased_sendmmsg(int socket, struct iovec* payload, int fd) {
  static union {
    struct mmsghdr entry;
    char bytes[sizeof(struct mmsghdr) + CMSG_SPACE(sizeof(int))];
  } block;
  memset(&block, 0, sizeof block);
  block.entry.msg_hdr.msg_iov = payload;
  block.entry.msg_hdr.msg_iovlen = 1;
  char* control = (char*)&block.entry.msg_len - offsetof(struct cmsghdr, cmsg_level);
  block.entry.msg_hdr.msg_control = control;
  block.entry.msg_hdr.msg_controllen = CMSG_SPACE(sizeof(int));
  // These bytes also hold msg_flags, which sendmmsg reads only for MSG_EOR,
  // clear in a cmsg_len of 20, and the entry's msg_len.
  struct cmsghdr header = {
      .cmsg_len = CMSG_LEN(sizeof(int)),
      .cmsg_level = SOL_SOCKET,
      .cmsg_type = SCM_RIGHTS,
  };
  memcpy(control, &header, sizeof header);
  memcpy(control + CMSG_LEN(0), &fd, sizeof fd);
  if (sendmmsg(socket, &block.entry, 1, 0) != 1) {
    fail("sendmmsg");
  }
  int level;
  memcpy(&level, control + offsetof(struct cmsghdr, cmsg_level), sizeof level);
  if (block.entry.msg_len != payload->iov_len || level != (int)payload->iov_len) {
    fprintf(stderr, "msg_len %u is not over cmsg_level %d\n",
            block.entry.msg_len, level);
    exit(1);
  }
  printf("sent by a sendmmsg whose msg_len overwrote its cmsg_level\n");
}

int main(int argc, char** argv) {
  const char* mode = argc > 1 ? argv[1] : "sendmsg";
  int channel[2];
  int endpoint[2];
  if (socketpair(AF_UNIX, SOCK_DGRAM, 0, channel) != 0 ||
      socketpair(AF_UNIX, SOCK_DGRAM, 0, endpoint) != 0) {
    fail("socketpair");
  }

  // Send endpoint[0] with an 8-byte payload, the size of msg_name.
  char payload[sizeof(void*)];
  memcpy(payload, "payload!", sizeof payload);
  struct iovec sent_iov = {.iov_base = payload, .iov_len = sizeof payload};
  union {
    char bytes[CMSG_SPACE(sizeof(int))];
    struct cmsghdr align;
  } sent_control;
  memset(&sent_control, 0, sizeof sent_control);
  struct msghdr sent = {0};
  sent.msg_iov = &sent_iov;
  sent.msg_iovlen = 1;
  sent.msg_control = sent_control.bytes;
  sent.msg_controllen = sizeof sent_control.bytes;
  struct cmsghdr* rights = CMSG_FIRSTHDR(&sent);
  rights->cmsg_level = SOL_SOCKET;
  rights->cmsg_type = SCM_RIGHTS;
  rights->cmsg_len = CMSG_LEN(sizeof(int));
  memcpy(CMSG_DATA(rights), &endpoint[0], sizeof(int));
  if (strcmp(mode, "sendmmsg-efault") == 0) {
    send_by_faulting_sendmmsg(channel[0], &sent);
  } else if (strcmp(mode, "sendmmsg-partial") == 0) {
    send_by_partly_faulting_sendmmsg(channel[0], &sent);
    char data[sizeof data_only];
    if (recv(channel[1], data, sizeof data, 0) != (ssize_t)strlen(data_only)) {
      fail("recv of the data-only message");
    }
  } else if (strcmp(mode, "sendmmsg-alias") == 0) {
    send_by_aliased_sendmmsg(channel[0], &sent_iov, endpoint[0]);
  } else if (strcmp(mode, "sendmsg") == 0) {
    if (sendmsg(channel[0], &sent, 0) != (ssize_t)sizeof payload) {
      fail("sendmsg");
    }
    printf("sent\n");
  } else {
    fprintf(stderr, "unknown mode %s\n", mode);
    return 2;
  }
  close(endpoint[0]);
  fflush(stdout);

  // Receive it with the payload buffer on this msghdr's own msg_name field
  // (NULL, so no address is written) and the control buffer elsewhere.
  union {
    char bytes[CMSG_SPACE(sizeof(int))];
    struct cmsghdr align;
  } control;
  memset(&control, 0, sizeof control);
  struct msghdr message = {0};
  struct iovec iov = {
      .iov_base = &message.msg_name, .iov_len = sizeof message.msg_name};
  message.msg_iov = &iov;
  message.msg_iovlen = 1;
  message.msg_control = control.bytes;
  message.msg_controllen = sizeof control.bytes;
  if (recvmsg(channel[1], &message, 0) != (ssize_t)sizeof payload) {
    fail("recvmsg");
  }
  if (memcmp(&message.msg_name, payload, sizeof payload) != 0) {
    fprintf(stderr, "the payload is not in msg_name\n");
    return 1;
  }
  rights = CMSG_FIRSTHDR(&message);
  if (rights == NULL || rights->cmsg_level != SOL_SOCKET ||
      rights->cmsg_type != SCM_RIGHTS ||
      rights->cmsg_len != CMSG_LEN(sizeof(int))) {
    fprintf(stderr, "no SCM_RIGHTS message with one descriptor\n");
    return 1;
  }
  memcpy(&received_fd, CMSG_DATA(rights), sizeof(int));
  printf("received\n");
  fflush(stdout);

  pthread_t threads[2];
  char* texts[2] = {"first", "second"};
  for (int i = 0; i < 2; i++) {
    if (pthread_create(&threads[i], NULL, send_record, texts[i]) != 0) {
      fprintf(stderr, "pthread_create failed\n");
      return 1;
    }
  }
  for (int i = 0; i < 2; i++) {
    pthread_join(threads[i], NULL);
  }
  printf("sent through the received descriptor from two threads\n");

  // Both records arrive, in an order the threads decide.
  int seen = 0;
  for (int i = 0; i < 2; i++) {
    char record[16] = {0};
    ssize_t length = recv(endpoint[1], record, sizeof record - 1, 0);
    if (length < 0) {
      fail("recv");
    }
    if (strcmp(record, "first") == 0) {
      seen |= 1;
    } else if (strcmp(record, "second") == 0) {
      seen |= 2;
    } else {
      fprintf(stderr, "unexpected record \"%s\"\n", record);
      return 1;
    }
  }
  if (seen != 3) {
    fprintf(stderr, "a record arrived twice\n");
    return 1;
  }
  printf("both records arrived\n");
  return 0;
}
