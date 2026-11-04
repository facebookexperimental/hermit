// Dispatcher for the combined gvisor_tests binary.
//
// Every test object from test/syscalls/linux is linked in once. gVisor's own
// test_main is left out; tests that define their own main() had that symbol
// renamed to gvt_main_<target> by objcopy (see generated/gvisor_tests.cmake).
//
// Selecting a target (in priority order):
//   1. argv[0] basename is a target name (busybox-style link, argv passed
//      through untouched -- required by exec_test, which compares argv with
//      /proc/self/cmdline);
//   2. the basename of /proc/self/exe is a target name (bin/combined/<target>
//      are hard links, so this survives re-executing /proc/self/exe with an
//      empty environment and argv[0] = "/proc/self/exe");
//   3. argv[1] is a target name (argv[1] is then removed);
//   4. $GVT_TARGET (set by mode 3 only, so its re-executions keep the
//      selection; modes 1 and 2 leave the environment untouched).
// With no target, every gtest case whose target does not define its own main()
// runs in this one process.
//
// gvisor::testing::RunAllTests is wrapped (-Wl,--wrap) so that, after flag
// parsing, the gtest filter is narrowed to the selected target's cases
// intersected with the user's --gtest_filter.
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#include <algorithm>
#include <utility>
#include <string>
#include <vector>

#include "gtest/gtest.h"
#include "test/util/test_util.h"

namespace {

struct Target {
  const char* name;
  int (*main_fn)(int, char**);
  std::vector<std::string> files;      // all test/syscalls/linux files linked
  std::vector<std::string> own_srcs;   // the binary's own srcs
  std::vector<std::string> prefixes;   // INSTANTIATE_*_P prefixes in files
  // Suites renamed in the combined binary only (two fixture types shared one
  // name across files): {renamed, upstream}.
  std::vector<std::pair<std::string, std::string>> renames;
};

}  // namespace

#define GVT_LIST(...) {__VA_ARGS__}
#define GVT_TARGET(...)
#define GVT_MAIN_DECL(sym) extern "C" int sym(int, char**);
#include "combined_manifest.inc"
#undef GVT_TARGET
#undef GVT_MAIN_DECL

namespace {

#define GVT_TARGET(name, fn, files, own, pref, ren) Target{name, fn, files, own, pref, ren},
#define GVT_MAIN_DECL(sym)
const std::vector<Target>& Targets() {
  static const std::vector<Target>* t = new std::vector<Target>{
#include "combined_manifest.inc"
  };
  return *t;
}
#undef GVT_TARGET
#undef GVT_MAIN_DECL

const Target* g_selected = nullptr;

const Target* Find(const char* name) {
  for (const Target& t : Targets()) {
    if (strcmp(t.name, name) == 0) return &t;
  }
  return nullptr;
}

bool EndsWithPath(const std::string& file, const std::string& suffix) {
  if (file.size() < suffix.size()) return false;
  if (file.compare(file.size() - suffix.size(), suffix.size(), suffix) != 0)
    return false;
  return file.size() == suffix.size() ||
         file[file.size() - suffix.size() - 1] == '/';
}

bool InFiles(const char* file, const std::vector<std::string>& files) {
  if (file == nullptr) return false;
  for (const std::string& f : files) {
    if (EndsWithPath(file, f)) return true;
  }
  return false;
}

// gtest glob: '*' and '?'.
bool Glob(const char* p, const char* s) {
  if (*p == '\0') return *s == '\0';
  if (*p == '*') return Glob(p + 1, s) || (*s != '\0' && Glob(p, s + 1));
  if (*s == '\0') return false;
  return (*p == '?' || *p == *s) && Glob(p + 1, s + 1);
}

bool MatchesAny(const std::string& patterns, const std::string& name) {
  size_t start = 0;
  while (start <= patterns.size()) {
    size_t end = patterns.find(':', start);
    if (end == std::string::npos) end = patterns.size();
    std::string pat = patterns.substr(start, end - start);
    if (!pat.empty() && Glob(pat.c_str(), name.c_str())) return true;
    start = end + 1;
  }
  return false;
}

// Same semantics as gtest's --gtest_filter: POSITIVE[-NEGATIVE].
bool MatchesFilter(const std::string& filter, const std::string& name) {
  size_t dash = filter.find('-');
  std::string pos = filter.substr(0, dash);
  std::string neg = dash == std::string::npos ? "" : filter.substr(dash + 1);
  if (pos.empty()) pos = "*";
  return MatchesAny(pos, name) && !MatchesAny(neg, name);
}

bool BelongsTo(const Target& t, const ::testing::TestInfo& ti) {
  if (!InFiles(ti.file(), t.files)) return false;
  std::string suite = ti.test_suite_name();
  bool instantiated = ti.value_param() != nullptr ||
                      (ti.type_param() != nullptr &&
                       std::count(suite.begin(), suite.end(), '/') >= 2);
  size_t slash = suite.find('/');
  if (!instantiated || slash == std::string::npos) return true;
  std::string prefix = suite.substr(0, slash);
  for (const std::string& p : t.prefixes) {
    if (p == prefix) return true;
  }
  return false;
}

bool OwnedByOwnMainTarget(const ::testing::TestInfo& ti) {
  for (const Target& t : Targets()) {
    if (t.main_fn != nullptr && InFiles(ti.file(), t.own_srcs)) return true;
  }
  return false;
}

}  // namespace

// Used by support/gvt_env_gate.h: run a global environment only for the
// target that links the file registering it.
bool GvtEnvironmentSelected(const char* file) {
  return g_selected != nullptr && InFiles(file, g_selected->files);
}

// Called instead of gvisor::testing::RunAllTests() (link with
// -Wl,--wrap=_ZN6gvisor7testing11RunAllTestsEv).
extern "C" int __real__ZN6gvisor7testing11RunAllTestsEv();
extern "C" int __wrap__ZN6gvisor7testing11RunAllTestsEv() {
  ::testing::UnitTest* ut = ::testing::UnitTest::GetInstance();
  std::string user = GTEST_FLAG_GET(filter);
  std::string narrowed;
  int n = 0;
  for (int i = 0; i < ut->total_test_suite_count(); i++) {
    const ::testing::TestSuite* ts = ut->GetTestSuite(i);
    for (int j = 0; j < ts->total_test_count(); j++) {
      const ::testing::TestInfo* ti = ts->GetTestInfo(j);
      bool in = g_selected ? BelongsTo(*g_selected, *ti)
                           : !OwnedByOwnMainTarget(*ti);
      if (!in) continue;
      std::string full = std::string(ti->test_suite_name()) + "." + ti->name();
      // Match the user's filter against the upstream name.
      std::string upstream = full;
      if (g_selected) {
        for (const auto& r : g_selected->renames) {
          size_t at = upstream.find(r.first + ".");
          if (at != std::string::npos && (at == 0 || upstream[at - 1] == '/'))
            upstream.replace(at, r.first.size(), r.second);
        }
      }
      if (!MatchesFilter(user, upstream)) continue;
      if (n++) narrowed += ':';
      narrowed += full;
    }
  }
  if (n == 0) narrowed = "GVT_NO_TEST_SELECTED.none";
  GTEST_FLAG_SET(filter, narrowed);
  return __real__ZN6gvisor7testing11RunAllTestsEv();
}

int main(int argc, char** argv) {
  const char* base = strrchr(argv[0], '/');
  base = base ? base + 1 : argv[0];
  const Target* t = Find(base);
  if (t == nullptr) {
    // bin/combined/<target> are hard links, so /proc/self/exe names the target
    // even when a test re-executes "/proc/self/exe" with an empty environment
    // (exec_test does).
    char exe[4096];
    ssize_t n = readlink("/proc/self/exe", exe, sizeof(exe) - 1);
    if (n > 0) {
      exe[n] = '\0';
      const char* eb = strrchr(exe, '/');
      t = Find(eb ? eb + 1 : exe);
    }
  }
  if (t == nullptr && argc > 1) {
    if (strcmp(argv[1], "--list-targets") == 0) {
      for (const Target& x : Targets())
        printf("%s%s\n", x.name, x.main_fn ? " (own main)" : "");
      return 0;
    }
    if ((t = Find(argv[1])) != nullptr) {
      argv[1] = argv[0];
      argv++;
      argc--;
      // Only this mode needs the environment to carry the selection into a
      // re-executed /proc/self/exe. The other modes leave environ untouched:
      // proc_test compares environ with /proc/self/environ and finds the
      // auxiliary vector by walking past the end of environ.
      setenv("GVT_TARGET", t->name, 1);
    }
  }
  if (t == nullptr) {
    const char* env = getenv("GVT_TARGET");
    if (env != nullptr && *env != '\0') {
      t = Find(env);
      if (t == nullptr) {
        fprintf(stderr, "gvisor_tests: unknown GVT_TARGET=%s\n", env);
        return 2;
      }
    }
  }
  g_selected = t;
  if (t != nullptr && t->main_fn != nullptr) return t->main_fn(argc, argv);
  gvisor::testing::TestInit(&argc, &argv);
  return gvisor::testing::RunAllTests();
}
