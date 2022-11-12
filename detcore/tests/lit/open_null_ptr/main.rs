/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// RUN: %me

fn main() {
    assert_eq!(unsafe { libc::open(core::ptr::null(), 0, 0) }, -1);
}
