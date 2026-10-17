/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * The three LSM self-attribute and module-list refusals, each member the
 * unchanged program it was before (its main renamed by the preprocessor),
 * run in a fresh child process (see probe_family.h). This one program
 * replaces the c-programs/lsm-{get-self-attr,set-self-attr,list-modules}-enosys
 * rows; the member sources stay the only copy of each check, and
 * hermit-cli/tests/host_security_identity.rs and host_kernel_probes.rs still
 * compile them directly.
 */

#define _GNU_SOURCE

#include "probe_family.h"

#define main lsm_get_self_attr_enosys_main
#include "lsm_get_self_attr_enosys.c"
#undef main

#define main lsm_set_self_attr_enosys_main
#include "lsm_set_self_attr_enosys.c"
#undef main

#define main lsm_list_modules_enosys_main
#include "lsm_list_modules_enosys.c"
#undef main

int main(void) {
  const probe_member members[] = {
      lsm_get_self_attr_enosys_main,
      lsm_set_self_attr_enosys_main,
      lsm_list_modules_enosys_main,
  };
  return run_probe_family(members, sizeof(members) / sizeof(members[0]));
}
