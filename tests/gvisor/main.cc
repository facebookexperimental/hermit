/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#include <cstdio>
#include <cstdlib>

#include "gtest/gtest.h"
#include "test/util/test_util.h"

#ifndef GVISOR_CASE
#error "Each binary must select one gVisor test case"
#endif

int main(int argc, char** argv) {
  if (argc != 1) {
    std::fprintf(stderr, "This program runs only %s; no arguments accepted.\n", GVISOR_CASE);
    return 2;
  }
  for (const char* name : {
           "GTEST_OUTPUT", "GUNIT_OUTPUT", "XML_OUTPUT_FILE",
           "TEST_RESULTS_OUTPUT_FILE", "GTEST_STREAM_RESULT_TO",
           "GTEST_TOTAL_SHARDS", "GTEST_SHARD_INDEX", "GTEST_SHARD_STATUS_FILE"}) {
    unsetenv(name);
  }
  if (setenv("TEST_ON_GVISOR", "native", 1) || setenv("GVISOR_SAVE_TEST", "false", 1)) {
    return 2;
  }
  char filter[] = "--gtest_filter=" GVISOR_CASE;
  char repeat[] = "--gtest_repeat=1";
  char shuffle[] = "--gtest_shuffle=0";
  char timing[] = "--gtest_print_time=0";
  char colour[] = "--gtest_color=no";
  char stream[] = "--gtest_stream_result_to=";
  char output[] = "--gtest_output=";
  char benchmark[] = "--benchmark_filter=^$";
  char* args[] = {argv[0], filter, repeat, shuffle, timing, colour, stream, output, benchmark, nullptr};
  int count = 9;
  char** argv_ptr = args;
  gvisor::testing::TestInit(&count, &argv_ptr);
  const int result = gvisor::testing::RunAllTests();
  const auto& tests = *testing::UnitTest::GetInstance();
  if (tests.test_to_run_count() != 1 || tests.successful_test_count() + tests.failed_test_count() != 1) {
    std::fprintf(stderr, "Exactly one non-skipped case must execute: %s\n", GVISOR_CASE);
    return tests.skipped_test_count() ? 77 : 78;
  }
  return result;
}
