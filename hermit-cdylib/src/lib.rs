/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! `libhermit.so`, the shared-object form of the hermit library, with every
//! symbol the hermit library and its dependencies export.
//!
//! This crate is the shared object only, so that `hermit` (hermit-cli) can be
//! an rlib alone; see detcore-dbt-cdylib for why that matters.

pub use hermit_rlib::*;
