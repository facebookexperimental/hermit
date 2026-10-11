/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Shared policy tokens for the log comparator and report consumers.

/// Versioned policy token for the only prefix removed by `BitwiseInfoV1`.
pub const STRIP_WALL_CLOCK_PREFIX_V1: &str = "real-wall-clock-prefix/v1";

/// Versioned policy token for lossless host-address ordinalization.
pub const CANON_ADDRESS_ORDINAL_V1: &str = "host-address-to-first-appearance-ordinal/v1";
