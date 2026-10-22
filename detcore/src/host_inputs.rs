/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Writes a run's host-input records to its `host_input_log` when the run
//! ends (see `detcore_model::host_input`).
//!
//! The records are an observation for the verification layer, not Detcore
//! state: Detcore's openat handler reports each one to the global state
//! (`tool_global::record_host_input`), which keeps them for the run, and
//! nothing reads them back, no scheduling decision consults them, and nothing
//! the guest sees depends on them. They are written once, after the run, so
//! the log stays empty while any guest runs; a run that does not end normally
//! writes nothing, which leaves its divergence unexplained rather than
//! explained by partial evidence.

use std::collections::BTreeSet;
use std::path::Path;

use detcore_model::host_input::HostInputLogEnd;
use detcore_model::host_input::HostInputRecord;
use detcore_model::host_input::HostMutationRecord;
use tracing::warn;

/// Write `records` to `log`, one JSON line each, then one
/// [`HostMutationRecord`] line for each path in `mutated`, followed by a
/// [`HostInputLogEnd`] line naming how many lines precede it, so that a
/// reader can tell a complete log from a cut one.
pub(crate) fn write(log: &Path, records: &[HostInputRecord], mutated: &BTreeSet<String>) {
    let mut text = Vec::new();
    let end = HostInputLogEnd {
        records: (records.len() + mutated.len()) as u64,
    };
    let encoded = records
        .iter()
        .map(serde_json::to_vec)
        .chain(mutated.iter().map(|path| {
            serde_json::to_vec(&HostMutationRecord {
                mutated: path.clone(),
            })
        }))
        .chain(std::iter::once(serde_json::to_vec(&end)))
        .try_for_each(|line| {
            text.extend(line?);
            text.push(b'\n');
            Ok::<(), serde_json::Error>(())
        });
    let written = encoded
        .map_err(std::io::Error::other)
        .and_then(|()| std::fs::write(log, &text));
    if let Err(error) = written {
        warn!(
            "[detcore] cannot write the host-input log {}: {}",
            log.display(),
            error
        );
    }
}
