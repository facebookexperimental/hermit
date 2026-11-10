// Force-included (via generated/combined_only/*.h) into test sources that
// register a gtest global environment, when they are compiled for the
// combined binary. A global environment is process-wide; upstream it only
// runs in its own test binary. Here it is wrapped so that its SetUp/TearDown
// run only when the selected target links the registering file.
// (socket_netlink_netfilter.cc's NetnsEnvironment unshares a user and network
// namespace for the whole process; unwrapped, it ran for every target.)
#ifndef GVT_ENV_GATE_H_
#define GVT_ENV_GATE_H_

#include "gtest/gtest.h"

// Defined in support/combined_main.cc.
bool GvtEnvironmentSelected(const char* file);

namespace gvt {

class GatedEnvironment : public ::testing::Environment {
 public:
  GatedEnvironment(const char* file, ::testing::Environment* env)
      : file_(file), env_(env) {}
  ~GatedEnvironment() override { delete env_; }
  void SetUp() override {
    on_ = GvtEnvironmentSelected(file_);
    if (on_) env_->SetUp();
  }
  void TearDown() override {
    if (on_) env_->TearDown();
  }

 private:
  const char* file_;
  ::testing::Environment* env_;
  bool on_ = false;
};

}  // namespace gvt

#define AddGlobalTestEnvironment(env) \
  AddGlobalTestEnvironment(new ::gvt::GatedEnvironment(__FILE__, (env)))

#endif  // GVT_ENV_GATE_H_
