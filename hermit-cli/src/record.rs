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
use std::rc::Rc;

use hermit_record::Tracer;
use reverie::ExitStatus;
use reverie::process::Command;
use reverie::process::Output;

use crate::consts::EXE_NAME;
use crate::consts::METADATA_NAME;
use crate::error::Context;
use crate::error::Error;
use crate::metadata::Metadata;
use crate::metadata::record_or_replay_config;
use crate::ptrace_completion::Control;
use crate::ptrace_completion::{self};

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
        mountinfo: crate::MountProvenance,
        mountinfo_mount_ids: Option<Vec<u64>>,
        local_networking: Option<bool>,
        proc_level: Option<detcore_model::config::ProcLevel>,
        cpu_profile: detcore_model::cpu_profile::CpuProfile,
    ) -> Result<Self, Error> {
        crate::cpu_profile_admission::check_record_or_replay(cpu_profile)?;
        let mut metadata = Metadata::new(&command)?;
        metadata.local_networking = local_networking;
        // A recording always names its level, so its replay does not depend
        // on the replaying Hermit's default.
        metadata.proc_level = Some(proc_level.unwrap_or_default());
        // Under x86-64-v3 the host must qualify, and the recording binds the
        // host facts the guest's answers come from
        // (https://github.com/rrnewton/hermit/issues/4243).
        let cpu_profile_host = if cpu_profile.is_default() {
            None
        } else {
            let request = crate::cpu_profile_admission::record_or_replay_request(false);
            let (_probe, host) = crate::cpu_profile_admission::admit(&request).map_err(|why| {
                crate::cpu_profile_admission::CpuProfileRefusal::new(cpu_profile, why)
            })?;
            Some(host)
        };
        metadata.cpu_profile = cpu_profile;
        metadata.cpu_profile_host = cpu_profile_host.clone();
        metadata.cpu_profile_residual = (cpu_profile
            == detcore_model::cpu_profile::CpuProfile::X86_64V3)
            .then(|| detcore_model::cpu_profile::V3_RESIDUAL.to_owned());
        metadata.mountinfo_root_rewrites = mountinfo.root_rewrites;
        metadata.container_mount_table = mountinfo.container_mount_table;
        metadata.mountinfo_mount_ids_captured = mountinfo_mount_ids.is_some();
        metadata.mountinfo_mount_ids = mountinfo_mount_ids.unwrap_or_default();

        let exe = dir.join(EXE_NAME);

        // Record the full program executable to `{hermit_data}/{id}/exe`.
        //
        // TODO: Handle shebang lines.
        fs::copy(&metadata.exe, &exe)
            .with_context(|| format!("Failed to record {:?}", metadata.exe))?;

        let mut config = record_or_replay_config(dir);
        config.cpu_profile = cpu_profile;
        config.cpu_profile_host = cpu_profile_host;
        if !cpu_profile.is_default() {
            metadata.cpu_table_digest = Some(hex_digest(&detcore::virtual_cpuid_digest(&config)));
        }

        let metadata_path = dir.join(METADATA_NAME);
        write_metadata(fs::File::create(&metadata_path)?, &metadata)
            .context("Failed to serialize metadata")?;

        config.mountinfo_root_rewrites = metadata.mountinfo_root_rewrites.clone();
        config.mountinfo_mount_ids = metadata.mountinfo_mount_ids.clone();
        config.mountinfo_mount_ids_captured = metadata.mountinfo_mount_ids_captured;
        config.mount_id_assignment_order = metadata.mount_id_assignment_order.clone();
        config.container_mount_table = metadata.container_mount_table.clone();
        config.proc_level = metadata.proc_level.unwrap_or_default();

        crate::cpu_profile_admission::refuse_unadmitted_launch(&config, true)?;
        let tracer = hermit_record::spawn_tracer(command, config).await?;

        metadata.proc_readonly = crate::proc_mount::guest_proc_readonly(tracer.guest_pid());
        crate::proc_mount::report_proc_mode(None, metadata.proc_readonly);

        Ok(Self {
            tracer,
            metadata,
            metadata_path,
        })
    }

    fn persist_completed_metadata(
        metadata: &mut Metadata,
        metadata_path: &Path,
        provenance: Option<(Vec<u64>, Vec<u64>)>,
    ) -> Result<(), Error> {
        if let Some((mountinfo_order, assignment_order)) = provenance {
            metadata.mountinfo_mount_ids = mountinfo_order;
            metadata.mountinfo_mount_ids_captured = true;
            metadata.mount_id_assignment_order = assignment_order;
        }
        metadata.replay_refused = detcore::detlog::replay_refusal();
        // Also persist the proc mode observed after guest setup, even when no
        // mount-identity provenance was collected. Do this only after joining
        // the recording, so a metadata write failure cannot abandon a tracee.
        let directory = metadata_path
            .parent()
            .ok_or_else(|| Error::msg("recording metadata path has no parent"))?;
        let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
        write_metadata(temporary.as_file_mut(), metadata)
            .context("Failed to serialize final recording metadata")?;
        temporary
            .persist(metadata_path)
            .map_err(|error| error.error)
            .context("Failed to persist final recording metadata")?;
        Ok(())
    }

    /// Waits for the recording to finish and returns its exit status.
    pub async fn wait(self, control: Rc<Control>) -> Result<ExitStatus, Error> {
        let Self {
            tracer,
            mut metadata,
            metadata_path,
        } = self;
        let (exit_status, global_state) = ptrace_completion::wait(tracer, control).await?;
        let provenance = global_state
            .mount_identity_provenance()
            .map(|value| value.map(|p| (p.mountinfo_order, p.assignment_order)))
            .map_err(Error::msg);
        global_state.clean_up(false, &None).await;
        // Publish final metadata only after the original successful state joined.
        Self::persist_completed_metadata(&mut metadata, &metadata_path, provenance?)?;
        Ok(exit_status)
    }

    /// Waits for the recording to finish and collects its output.
    pub async fn wait_with_output(self, control: Rc<Control>) -> Result<Output, Error> {
        let Self {
            tracer,
            mut metadata,
            metadata_path,
        } = self;
        let (output, global_state) = ptrace_completion::wait_with_output(tracer, control).await?;
        let provenance = global_state
            .mount_identity_provenance()
            .map(|value| value.map(|p| (p.mountinfo_order, p.assignment_order)))
            .map_err(Error::msg);
        global_state.clean_up(false, &None).await;
        // Publish final metadata only after the original successful state joined.
        Self::persist_completed_metadata(&mut metadata, &metadata_path, provenance?)?;
        Ok(output)
    }
}

/// The lowercase hex form of a digest, as the recording stores it.
pub(crate) fn hex_digest(digest: &[u8; 32]) -> String {
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Writes a recording's metadata as pretty JSON through a buffer: serde_json
/// issues one write(2) per token to an unbuffered file, and a container mount
/// table makes that thousands of lines.
fn write_metadata(file: impl std::io::Write, metadata: &Metadata) -> std::io::Result<()> {
    let mut writer = std::io::BufWriter::new(file);
    serde_json::to_writer_pretty(&mut writer, metadata)?;
    std::io::Write::flush(&mut writer)
}
