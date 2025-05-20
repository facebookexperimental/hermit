/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#include <folly/Random.h>
#include <gtest/gtest.h>
#include <netinet/in.h>
#include <sys/socket.h>
#include <ctime>

static int tcp4_stream(void) {
  return socket(AF_INET, SOCK_STREAM, 0);
}

static int bind_port(int sockfd, int port) {
  struct sockaddr_in sa;

  memset(&sa, 0, sizeof(sa));
  sa.sin_family = AF_INET;
  sa.sin_port = port;
  sa.sin_addr.s_addr = INADDR_ANY;

  return bind(sockfd, (const struct sockaddr*)&sa, sizeof(sa));
}

static void do_some_stuff(void) {
  struct timespec tp = {
      .tv_sec = 0,
      .tv_nsec = 100000000,
  };
  clock_nanosleep(CLOCK_MONOTONIC, 0, &tp, nullptr);
}

// Bind with a random port, this is generally a bad idea for tests because
// the tests would fail when run in parallel, and when other parallel job's
// rand32 call return the same number.
TEST(BindRandom, bindRandom) {
  int sockfd = tcp4_stream();
  ASSERT_GE(sockfd, 0);
  int port = folly::Random::rand32(9000, 9999);
  int ret = bind_port(sockfd, port);
  EXPECT_EQ(ret, 0);
  do_some_stuff();
  close(sockfd);
}
