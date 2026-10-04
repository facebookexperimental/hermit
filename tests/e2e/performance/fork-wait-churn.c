/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#define STEADY_PROFILE PROFILE_FORK_WAIT
#define PROFILE_NAME "fork-wait-churn"
#include "steady_state_extra.c"
