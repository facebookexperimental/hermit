/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#![feature(get_mut_unchecked)]
#![feature(thread_id_value)]
#![feature(atomic_mut_ptr)]

mod fds;
mod filesystem;
mod misc;
mod parallelism;
mod stats;
mod time;
