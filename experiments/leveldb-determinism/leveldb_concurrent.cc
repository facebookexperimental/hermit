// Copyright (c) Meta Platforms, Inc. and affiliates.
// All rights reserved.
//
// This source code is licensed under the BSD-style license found in the
// LICENSE file in the root directory of this source tree.
//
// Concurrent LevelDB workload used to demonstrate thread-scheduling
// nondeterminism and Hermit's determinization of it.
//
// Each of N worker threads performs the SAME small, fixed sequence of LevelDB
// Put/Get operations against a shared database, so the *content* every thread
// computes is identical across runs. When a thread finishes it appends a single
// summary line to a mutex-guarded vector. The ORDER of those lines is the order
// in which threads reached the mutex, which is governed by thread scheduling and
// therefore varies run-to-run natively. Under `hermit run` the schedule is
// deterministic, so the order -- and thus the whole output -- is identical on
// every run.
//
// Usage: leveldb_concurrent DB_PATH [NTHREADS] [OPS_PER_THREAD]
//
// The dataset is intentionally tiny (default 8 threads x 50 ops) to keep CI
// fast; the effect does not depend on scale.

#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <mutex>
#include <string>
#include <thread>
#include <vector>

#include <leveldb/db.h>
#include <leveldb/options.h>
#include <leveldb/write_batch.h>

int main(int argc, char** argv) {
  if (argc < 2) {
    std::fprintf(stderr, "usage: %s DB_PATH [NTHREADS] [OPS_PER_THREAD]\n",
                 argv[0]);
    return 2;
  }
  const std::string db_path = argv[1];
  const int nthreads = argc > 2 ? std::atoi(argv[2]) : 8;
  const int ops = argc > 3 ? std::atoi(argv[3]) : 50;
  if (nthreads < 1 || ops < 1) {
    std::fprintf(stderr, "NTHREADS and OPS_PER_THREAD must be positive\n");
    return 2;
  }

  leveldb::DB* db = nullptr;
  leveldb::Options options;
  options.create_if_missing = true;
  // Force some real storage engine work at this tiny scale: flush to an SST
  // frequently so background compaction threads run too.
  options.write_buffer_size = 4 * 1024;
  leveldb::Status status = leveldb::DB::Open(options, db_path, &db);
  if (!status.ok()) {
    std::fprintf(stderr, "failed to open %s: %s\n", db_path.c_str(),
                 status.ToString().c_str());
    return 1;
  }

  std::mutex mu;
  std::vector<std::string> completion_order;
  completion_order.reserve(nthreads);

  std::vector<std::thread> workers;
  workers.reserve(nthreads);
  for (int t = 0; t < nthreads; ++t) {
    workers.emplace_back([&, t]() {
      uint64_t acc = 0;
      for (int i = 0; i < ops; ++i) {
        char key[64];
        char val[64];
        std::snprintf(key, sizeof(key), "t%03d-k%06d", t, i);
        std::snprintf(val, sizeof(val), "value-%d-%d", t, i);
        db->Put(leveldb::WriteOptions(), key, val);
        std::string got;
        leveldb::Status s = db->Get(leveldb::ReadOptions(), key, &got);
        if (s.ok()) {
          for (char c : got) {
            acc += static_cast<unsigned char>(c);
          }
        }
      }
      char line[96];
      std::snprintf(line, sizeof(line), "thread %03d done ops=%d acc=%llu", t,
                    ops, static_cast<unsigned long long>(acc));
      std::lock_guard<std::mutex> lk(mu);
      completion_order.emplace_back(line);
    });
  }
  for (auto& w : workers) {
    w.join();
  }

  // Scheduling-dependent section: order reflects thread completion order.
  for (const auto& line : completion_order) {
    std::printf("%s\n", line.c_str());
  }

  // Scheduling-independent integrity check: the final key set is identical
  // regardless of interleaving, so this line is deterministic even natively and
  // confirms the workload actually exercised the store.
  uint64_t total_keys = 0;
  leveldb::Iterator* it = db->NewIterator(leveldb::ReadOptions());
  for (it->SeekToFirst(); it->Valid(); it->Next()) {
    ++total_keys;
  }
  delete it;
  std::printf("total_keys=%llu\n", static_cast<unsigned long long>(total_keys));

  delete db;
  return 0;
}
