/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#include <curl/curl.h>
#include <fmt/format.h>
#include <gtest/gtest.h>
#include <netinet/in.h>
#include <sys/socket.h>
#include <unistd.h>
#include <cerrno>
#include <cstdlib>
#include <cstring>
#include <random>
#include <string>
#include <thread>

const size_t FLAKY_SERVICE_RESPONSE_BUFFER_SIZE = 1024;

size_t writeResponseData(char* ptr, size_t size, size_t nmemb, void* buffer);

// In-process fake of the configurable flaky service, on loopback. The
// fixture used to POST to a production endpoint; it now talks to this
// local fake instead, so it needs no credential and no external network.
// Like the real service, the fake fails about half its requests: by
// default each request independently gets either the small body "1" or
// an error body larger than the client's response buffer (probability
// 1/2 each), preserving the deliberate overflow/crash path this fixture
// exists to demonstrate. FAKE_FLAKY_LARGE_ERROR=0 forces the small body
// and =1 forces the large error body.
class FakeFlakyServer {
 public:
  FakeFlakyServer() {
    listenFd_ = ::socket(AF_INET, SOCK_STREAM, 0);
    EXPECT_NE(listenFd_, -1);
    int one = 1;
    ::setsockopt(listenFd_, SOL_SOCKET, SO_REUSEADDR, &one, sizeof(one));
  }

  // Fatal checks live here, not in the constructor: ASSERT_* returns from
  // the enclosing function, which is ill-formed in a ctor. Callers must
  // check HasFatalFailure() before using url().
  void start() {
    sockaddr_in addr{};
    addr.sin_family = AF_INET;
    addr.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    addr.sin_port = 0; // ephemeral port
    ASSERT_EQ(::bind(listenFd_, (sockaddr*)&addr, sizeof(addr)), 0)
        << std::strerror(errno);
    ASSERT_EQ(::listen(listenFd_, 1), 0);
    socklen_t len = sizeof(addr);
    ASSERT_EQ(::getsockname(listenFd_, (sockaddr*)&addr, &len), 0);
    port_ = ntohs(addr.sin_port);
    thread_ = std::thread([this] { serveOnce(); });
  }

  ~FakeFlakyServer() {
    if (listenFd_ != -1) {
      ::shutdown(listenFd_, SHUT_RDWR);
    }
    if (thread_.joinable()) {
      thread_.join();
    }
    if (listenFd_ != -1) {
      ::close(listenFd_);
    }
  }

  std::string url() const {
    return fmt::format(
        "http://127.0.0.1:{}/hermetic_infra/flaky_service", port_);
  }

 private:
  // FAKE_FLAKY_LARGE_ERROR=1 forces the large body, =0 forces the small
  // one; unset (or anything else) flips a fair coin per request, matching
  // the real service's roughly 50% error rate.
  bool useLargeErrorBody() {
    const char* forced = std::getenv("FAKE_FLAKY_LARGE_ERROR");
    if (forced != nullptr && std::string(forced) == "1") {
      return true;
    }
    if (forced != nullptr && std::string(forced) == "0") {
      return false;
    }
    std::random_device rd;
    std::uniform_int_distribution<int> coin(0, 1);
    return coin(rd) == 1;
  }

  void serveOnce() {
    int fd = ::accept(listenFd_, nullptr, nullptr);
    if (fd == -1) {
      return;
    }
    // Drain the request (headers + small body) before replying. Loop with
    // a short receive timeout: one recv() can return a partial request.
    timeval tv{.tv_sec = 0, .tv_usec = 200000};
    ::setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof(tv));
    std::string req;
    char chunk[1024];
    while (req.find("\r\n\r\n") == std::string::npos) {
      ssize_t n = ::recv(fd, chunk, sizeof(chunk), 0);
      if (n <= 0) {
        break;
      }
      req.append(chunk, (size_t)n);
      if (req.size() > 8192) {
        break;
      }
    }
    std::string body = "1";
    if (useLargeErrorBody()) {
      body = std::string(2048, 'e');
    }
    std::string resp = fmt::format(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: "
        "{}\r\nConnection: close\r\n\r\n{}",
        body.size(),
        body);
    ::send(fd, resp.data(), resp.size(), MSG_NOSIGNAL);
    ::close(fd);
  }

  int listenFd_ = -1;
  uint16_t port_ = 0;
  std::thread thread_;
};

// Talks to a service that may be flaky and return an error with an exception
// trace. The client code does not properly handle large errors due to a low
// buffer size, causing a crash.
TEST(UseConfigurableFlakyService, request) {
  FakeFlakyServer server;
  server.start();
  if (HasFatalFailure()) {
    return;
  }
  auto url = server.url();
  auto postFields = std::string("access_token=fake-local-token");
  char buffer[FLAKY_SERVICE_RESPONSE_BUFFER_SIZE];

  curl_global_init(CURL_GLOBAL_DEFAULT);

  auto curl = curl_easy_init();
  EXPECT_NE(curl, nullptr);

  // Never let an http_proxy environment variable route loopback traffic.
  curl_easy_setopt(curl, CURLOPT_PROXY, "");
  curl_easy_setopt(curl, CURLOPT_URL, url.c_str());
  curl_easy_setopt(curl, CURLOPT_POSTFIELDS, postFields.c_str());
  curl_easy_setopt(curl, CURLOPT_WRITEFUNCTION, &writeResponseData);
  curl_easy_setopt(curl, CURLOPT_WRITEDATA, &buffer);

  auto result = curl_easy_perform(curl);
  EXPECT_EQ(result, CURLE_OK);

  curl_easy_cleanup(curl);
  curl_global_cleanup();
}

size_t writeResponseData(char* ptr, size_t size, size_t nmemb, void* buffer) {
  auto realSize = size * nmemb;
  std::memcpy(buffer, ptr, realSize);
  return realSize;
}
