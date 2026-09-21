/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/// The name of the JSON metadata file that is saved for each recording.
pub const METADATA_NAME: &str = "metadata.json";

/// The name of the root executable.
pub const EXE_NAME: &str = "exe";

/// Directory containing content-addressed executable snapshots referenced by
/// successful exec events.
pub const EXEC_FILES_NAME: &str = "exec_files";
