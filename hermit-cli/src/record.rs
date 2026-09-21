/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::fs;
use std::path::Path;
use std::path::PathBuf;

use detcore_model::config::MountInfoRootRewrite;
use reverie::ExitStatus;
use reverie::process::Command;
use reverie::process::Output;

use crate::consts::EXE_NAME;
use crate::consts::METADATA_NAME;
use crate::error::Context;
use crate::error::Error;
use crate::metadata::Metadata;
use crate::metadata::record_or_replay_config;
use crate::recorder::Recorder;

type RecordTool = detcore::Detcore<Recorder>;
type Tracer = reverie_ptrace::Tracer<detcore::GlobalState>;

/// Represents a recording that is currently running.
pub struct Record {
    /// The running tracee.
    tracer: Tracer,
    metadata: Metadata,
    metadata_path: PathBuf,
}

impl Record {
    /// Spawns a recording with exact mountinfo provenance captured from the
    /// completed recording container namespace.
    pub async fn spawn_with_mountinfo(
        command: Command,
        dir: &Path,
        mountinfo_root_rewrites: Vec<MountInfoRootRewrite>,
        mountinfo_mount_ids: Option<Vec<u64>>,
    ) -> Result<Self, Error> {
        let mut metadata = Metadata::new(&command)?;
        metadata.mountinfo_root_rewrites = mountinfo_root_rewrites;
        metadata.mountinfo_mount_ids_captured = mountinfo_mount_ids.is_some();
        metadata.mountinfo_mount_ids = mountinfo_mount_ids.unwrap_or_default();

        let exe = dir.join(EXE_NAME);

        // Record the full program executable to `{hermit_data}/{id}/exe`.
        //
        // TODO: Handle shebang lines.
        fs::copy(&metadata.exe, &exe)
            .with_context(|| format!("Failed to record {:?}", metadata.exe))?;

        let metadata_path = dir.join(METADATA_NAME);
        serde_json::to_writer_pretty(fs::File::create(&metadata_path)?, &metadata)
            .context("Failed to serialize metadata")?;

        let mut config = record_or_replay_config(dir);
        config.mountinfo_root_rewrites = metadata.mountinfo_root_rewrites.clone();
        config.mountinfo_mount_ids = metadata.mountinfo_mount_ids.clone();
        config.mountinfo_mount_ids_captured = metadata.mountinfo_mount_ids_captured;
        config.fdinfo_unlisted_mount_ids = metadata.fdinfo_unlisted_mount_ids.clone();

        let tracer = reverie_ptrace::TracerBuilder::<RecordTool>::new(command)
            .config(config)
            .spawn()
            .await?;

        Ok(Self {
            tracer,
            metadata,
            metadata_path,
        })
    }

    fn persist_mount_identity_provenance(
        metadata: &mut Metadata,
        metadata_path: &Path,
        global_state: &detcore::GlobalState,
    ) -> Result<(), Error> {
        if let Some(provenance) = global_state
            .mount_identity_provenance()
            .map_err(Error::msg)?
        {
            metadata.mountinfo_mount_ids = provenance.mountinfo_order;
            metadata.mountinfo_mount_ids_captured = true;
            metadata.fdinfo_unlisted_mount_ids = provenance.unlisted_order;
            let directory = metadata_path
                .parent()
                .ok_or_else(|| Error::msg("recording metadata path has no parent"))?;
            let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
            serde_json::to_writer_pretty(temporary.as_file_mut(), metadata)
                .context("Failed to serialize final recording metadata")?;
            temporary
                .persist(metadata_path)
                .map_err(|error| error.error)
                .context("Failed to persist final recording metadata")?;
        }
        Ok(())
    }

    /// Waits for the recording to finish and returns its exit status.
    pub async fn wait(self) -> Result<ExitStatus, Error> {
        let Self {
            tracer,
            mut metadata,
            metadata_path,
        } = self;
        let (exit_status, global_state) = tracer.wait().await?;
        let persist =
            Self::persist_mount_identity_provenance(&mut metadata, &metadata_path, &global_state);
        global_state.clean_up(false, &None).await;
        persist?;
        Ok(exit_status)
    }

    /// Waits for the recording to finish and collects its output.
    pub async fn wait_with_output(self) -> Result<Output, Error> {
        let Self {
            tracer,
            mut metadata,
            metadata_path,
        } = self;
        let (output, global_state) = tracer.wait_with_output().await?;
        let persist =
            Self::persist_mount_identity_provenance(&mut metadata, &metadata_path, &global_state);
        global_state.clean_up(false, &None).await;
        persist?;
        Ok(output)
    }
}
