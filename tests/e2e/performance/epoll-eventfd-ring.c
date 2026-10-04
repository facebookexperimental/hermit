/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#define STEADY_PROFILE PROFILE_EPOLL_EVENTFD
#define PROFILE_NAME "epoll-eventfd-ring"
#include "steady_state_extra.c"
