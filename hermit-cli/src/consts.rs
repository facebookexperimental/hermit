/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/// The name of the JSON metadata file that is saved for each recording.
pub const METADATA_NAME: &str = "metadata.json";

/// The name of the run manifest a recording directory holds
/// (<https://github.com/rrnewton/hermit/issues/4271>).
pub const MANIFEST_NAME: &str = "manifest.json";

/// The file in the data directory naming the newest recording a user asked
/// for (`hermit record`, `hermit run --record-networking`), which a bare
/// `hermit replay` replays.
pub const LAST: &str = "last";

/// The file in the data directory naming the newest recording of any kind,
/// the manifest of a plain `hermit run` included.
pub const LAST_RUN: &str = "last-run";

/// The name of the root executable.
pub const EXE_NAME: &str = "exe";

/// Directory containing content-addressed executable snapshots referenced by
/// successful exec events.
pub const EXEC_FILES_NAME: &str = "exec_files";
