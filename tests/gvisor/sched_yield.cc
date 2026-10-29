// Copyright 2018 The gVisor Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

// Copied from gVisor d345f7da26f7c5c1b3d4c584b787fb1ac9c8cac7; assertions retained.
// https://github.com/google/gvisor/blob/d345f7da26f7c5c1b3d4c584b787fb1ac9c8cac7/test/syscalls/linux/sched_yield.cc

#include <sched.h>

#include "gmock/gmock.h"
#include "gtest/gtest.h"
#include "test/util/test_util.h"

namespace gvisor {
namespace testing {

namespace {

TEST(SchedYieldTest, Success) {
  EXPECT_THAT(sched_yield(), SyscallSucceeds());
  EXPECT_THAT(sched_yield(), SyscallSucceeds());
}

}  // namespace

}  // namespace testing
}  // namespace gvisor
