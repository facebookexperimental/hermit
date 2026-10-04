/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#define STEADY_PROFILE PROFILE_SPIN_TRYLOCK
#define PROFILE_NAME "spin-trylock-contention"
#include "steady_state_third.c"
