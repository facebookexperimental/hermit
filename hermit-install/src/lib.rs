/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Build helper and finalizer for Hermit's release installation package.
//!
//! Staging includes the first-party LiteInst resources. DynamoRIO, SaBRe and
//! e9patch are staged by their external repositories. The core package requires
//! the ordinary `stage-detcore-liteinst` finalizer after Cargo releases the
//! target; selecting a package never bypasses its artifact proof.
