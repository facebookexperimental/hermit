// Replacement for gVisor's test/util/test_util_runfiles.cc, which needs
// Bazel's runfiles library. Data dependencies (helper binaries such as
// exec_basic_workload) are laid out by the CMake build under
//   <dir of the running binary>/runfiles/<package>/<name>
// GVT_RUNFILES_DIR overrides that directory.
#include <limits.h>
#include <stdlib.h>
#include <unistd.h>

#include <string>

#include "test/util/test_util.h"

namespace gvisor {
namespace testing {

std::string RunfilePath(std::string path) {
  const char* env = getenv("GVT_RUNFILES_DIR");
  std::string dir;
  if (env != nullptr && *env != '\0') {
    dir = env;
  } else {
    char buf[PATH_MAX];
    ssize_t n = readlink("/proc/self/exe", buf, sizeof(buf) - 1);
    if (n > 0) {
      buf[n] = '\0';
      std::string exe(buf);
      dir = exe.substr(0, exe.rfind('/')) + "/runfiles";
    } else {
      dir = "runfiles";
    }
  }
  return dir + "/" + path;
}

}  // namespace testing
}  // namespace gvisor
