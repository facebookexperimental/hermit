/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * tests/c/proc_cmdline.c's other-process variant: everything it reads, plus
 * another process's /proc/<pid>/cmdline, which only the kernel beneath can
 * serve (the same as running proc_cmdline.c with the guest argument
 * other-processes). A separate program because a manifest program belongs
 * to one manifest test.
 */
#define PROC_CMDLINE_OTHER_PROCESSES 1
#include "proc_cmdline.c"
