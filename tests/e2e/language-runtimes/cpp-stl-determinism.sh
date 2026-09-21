#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# End-to-end C++ STL determinism fixture.
#
# C++ has no implicit compile support in the harness (only .c and .rs), so this
# shell wrapper compiles a small STL program natively during --prepare into the
# persisted E2E_FIXTURE_DIR and execs the prebuilt binary during --run. The
# program's only entropy source is std::random_device (OS getrandom/urandom),
# which varies every run natively and is determinized by Hermit --strict; every
# downstream STL result (std::set / std::map / std::unordered_map / std::sort /
# std::string) plus the file-I/O roundtrip and std::chrono timestamp is derived
# from it, so the whole observation is bitwise reproducible under verification.

set -euo pipefail

case ${1:-} in
    --prepare)
        command -v c++ >/dev/null
        : "${E2E_FIXTURE_DIR:?E2E_FIXTURE_DIR must be set}"
        mkdir -p "$E2E_FIXTURE_DIR"
        src="$E2E_FIXTURE_DIR/program.cpp"
        cat >"$src" <<'CPP'
#include <algorithm>
#include <chrono>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <filesystem>
#include <fstream>
#include <map>
#include <random>
#include <set>
#include <sstream>
#include <string>
#include <unordered_map>
#include <vector>

// 64-bit FNV-1a fold used to compress the STL-derived material into a compact,
// well-defined fingerprint. Pure function of the input bytes.
static uint64_t fnv1a(const std::string& s) {
    uint64_t h = 1469598103934665603ULL;
    for (unsigned char c : s) {
        h ^= c;
        h *= 1099511628211ULL;
    }
    return h;
}

int main() {
    // std::random_device seeded explicitly from /dev/urandom. The default
    // libstdc++ token would prefer the RDRAND CPU instruction when the host
    // exposes it, and the portable harness profile runs with
    // --no-virtualize-cpuid, so RDRAND would be visible and executed directly
    // in userspace where Hermit cannot intercept it -- making the seed
    // nondeterministic. Reading /dev/urandom instead routes entropy through a
    // syscall that Hermit --strict determinizes, so the seed (and everything
    // derived from it below) is reproducible. Natively /dev/urandom still
    // varies every run.
    std::random_device rd("/dev/urandom");
    std::mt19937 gen(rd());
    std::uniform_int_distribution<int> dist(0, 9999);

    const int N = 256;
    std::vector<int> nums;
    nums.reserve(N);
    for (int i = 0; i < N; ++i) {
        nums.push_back(dist(gen));
    }

    // Ordered STL containers over the random data.
    std::set<int> uniq(nums.begin(), nums.end());
    std::map<int, int> freq;
    for (int v : nums) {
        ++freq[v];
    }

    // Hashed STL container. Its iteration order is implementation-defined, so
    // fold it back in key-sorted order to keep the fingerprint well-defined.
    std::unordered_map<int, int> hmap;
    for (int v : nums) {
        ++hmap[v];
    }
    std::map<int, int> hmap_ordered(hmap.begin(), hmap.end());

    // std::sort of the raw sequence.
    std::vector<int> sorted_nums = nums;
    std::sort(sorted_nums.begin(), sorted_nums.end());

    // std::string construction and lexical sort.
    std::vector<std::string> words;
    words.reserve(uniq.size());
    for (int v : uniq) {
        words.push_back("k" + std::to_string(v));
    }
    std::sort(words.begin(), words.end());

    // std::chrono: determinized wall clock under Hermit.
    auto now = std::chrono::system_clock::now().time_since_epoch();
    long long epoch_s =
        std::chrono::duration_cast<std::chrono::seconds>(now).count();

    // Assemble a payload from the STL-derived material.
    std::ostringstream oss;
    oss << "uniq=" << uniq.size() << ";";
    for (int v : uniq) {
        oss << v << ",";
    }
    oss << "\nmap=";
    for (const auto& kv : freq) {
        oss << kv.first << ":" << kv.second << ",";
    }
    oss << "\nhmap=";
    for (const auto& kv : hmap_ordered) {
        oss << kv.first << ":" << kv.second << ",";
    }
    oss << "\nsorted=";
    for (int v : sorted_nums) {
        oss << v << ",";
    }
    oss << "\nwords=";
    for (const auto& w : words) {
        oss << w << ",";
    }
    std::string payload = oss.str();

    // File I/O: write the payload to E2E_TMPDIR and read it back as bytes.
    // Hermit runs the guest with a fresh isolated /tmp per repeat, so create
    // the directory first; otherwise the ofstream below fails silently.
    const char* tmp = std::getenv("E2E_TMPDIR");
    std::string dir = tmp ? tmp : "/tmp";
    std::filesystem::create_directories(dir);
    std::string path = dir + "/hermit-cpp-stl.txt";
    {
        std::ofstream out(path, std::ios::binary | std::ios::trunc);
        out << payload;
    }
    std::string readback;
    {
        std::ifstream in(path, std::ios::binary);
        std::ostringstream buf;
        buf << in.rdbuf();
        readback = buf.str();
    }

    std::printf(
        "CPPSTL uniq=%zu map=%zu sorted0=%d sortedN=%d epoch_s=%lld "
        "bytes=%zu payload_fnv=%016llx roundtrip=%d\n",
        uniq.size(), freq.size(), sorted_nums.front(), sorted_nums.back(),
        epoch_s, readback.size(),
        static_cast<unsigned long long>(fnv1a(payload)),
        static_cast<int>(readback == payload));
    return 0;
}
CPP
        c++ -std=c++17 -O2 -g -Wall -Wextra -Werror \
            "$src" -o "$E2E_FIXTURE_DIR/program"
        ;;
    --run)
        : "${E2E_FIXTURE_DIR:?E2E_FIXTURE_DIR must be set}"
        exec "$E2E_FIXTURE_DIR/program"
        ;;
    *)
        echo "usage: $0 --prepare|--run" >&2
        exit 2
        ;;
esac
