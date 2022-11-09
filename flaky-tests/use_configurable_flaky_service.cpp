/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#include <curl/curl.h>
#include <folly/Format.h>
#include <gtest/gtest.h>
#include <cstdlib>
#include <cstring>

const char* FLAKY_SERVICE_APP = "3355159521469756";
const char* FLAKY_SERVICE_TOKEN = "AeOr7DicKuQPHlMinzo";
const size_t FLAKY_SERVICE_RESPONSE_BUFFER_SIZE = 1024;

std::string getFlakyServiceURL();
size_t writeResponseData(char* ptr, size_t size, size_t nmemb, void* buffer);

// Talks to a service that may be flaky and return an error with an exception
// trace. The client code does not properly handle large errors due to a low
// buffer size, causing a crash.
TEST(UseConfigurableFlakyService, request) {
  auto url = getFlakyServiceURL();
  auto postFields =
      folly::sformat("app={}&token={}", FLAKY_SERVICE_APP, FLAKY_SERVICE_TOKEN);
  char buffer[FLAKY_SERVICE_RESPONSE_BUFFER_SIZE];

  curl_global_init(CURL_GLOBAL_DEFAULT);

  auto curl = curl_easy_init();
  EXPECT_NE(curl, nullptr);

  curl_easy_setopt(curl, CURLOPT_URL, url.c_str());
  curl_easy_setopt(curl, CURLOPT_POSTFIELDS, postFields.c_str());
  curl_easy_setopt(curl, CURLOPT_WRITEFUNCTION, &writeResponseData);
  curl_easy_setopt(curl, CURLOPT_WRITEDATA, &buffer);

  auto result = curl_easy_perform(curl);
  EXPECT_EQ(result, CURLE_OK);

  curl_easy_cleanup(curl);
  curl_global_cleanup();
}

std::string getFlakyServiceURL() {
  auto od = std::getenv("OD");
  auto tier = od != nullptr ? od : "intern";
  return folly::sformat(
      "https://interngraph.{}.facebook.com/hermetic_infra/flaky_service", tier);
}

size_t writeResponseData(char* ptr, size_t size, size_t nmemb, void* buffer) {
  auto realSize = size * nmemb;
  std::memcpy(buffer, ptr, realSize);
  return realSize;
}
