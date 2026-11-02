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

#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
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

int main(void) {
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
  if (sendmsg(channel[0], &sent, 0) != (ssize_t)sizeof payload) {
    fail("sendmsg");
  }
  close(endpoint[0]);
  printf("sent\n");
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
