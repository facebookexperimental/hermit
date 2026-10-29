/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#include <cerrno>
#include <string>
#include <type_traits>
#include <utility>
#include <vector>

#include "gtest/gtest.h"
#include "test/util/test_util.h"

TEST(GvisorUtilities, RetryEINTRReusesOwningArguments) {
  std::vector<std::string> observed;
  auto operation = [&](std::string input) {
    observed.push_back(std::move(input));
    if (observed.size() == 1) {
      errno = EINTR;
      return -1;
    }
    return 7;
  };
  std::string original = "payload";
  EXPECT_EQ(gvisor::testing::RetryEINTR(std::move(operation))(std::move(original)), 7);
  EXPECT_EQ(observed, (std::vector<std::string>{"payload", "payload"}));
}

namespace {
struct OverloadedOperation {
  long operator()(std::string&) const {
    return 1;
  }
  int operator()(std::string&&) const {
    return 2;
  }
};
}

TEST(GvisorUtilities, RetryEINTRDeducesReusableCallReturnType) {
  auto operation = gvisor::testing::RetryEINTR(OverloadedOperation{});
  static_assert(std::is_same_v<decltype(operation(std::string{})), long>);
  EXPECT_EQ(operation(std::string{}), 1);
  EXPECT_EQ(OverloadedOperation{}(std::string{}), 2);
}
