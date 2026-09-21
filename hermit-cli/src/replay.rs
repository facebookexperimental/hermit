/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::ffi::OsString;
use std::fs;
use std::fs::OpenOptions;
use std::io;
use std::io::Seek;
use std::io::SeekFrom;
use std::os::fd::AsRawFd;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::path::PathBuf;

use reverie::ExitStatus;
use reverie::process::Mount;
use reverie::process::MountFlags;
use reverie::process::Output;
use reverie::process::Stdio;

use crate::chroot::TempChroot;
use crate::consts::EXEC_FILES_NAME;
use crate::consts::METADATA_NAME;
use crate::error::Context;
use crate::error::Error;
use crate::event::ExecEvent;
use crate::event::ExecImage;
use crate::event::ExecTarget;
use crate::event::SyscallEvent;
use crate::event_stream::EventReader;
use crate::event_stream::EventStreamId;
use crate::metadata::Metadata;
use crate::metadata::RECORD_VERSION;
use crate::metadata::record_or_replay_config;
use crate::replayer::Replayer;

type ReplayTool = detcore::Detcore<Replayer>;
type Tracer = reverie_ptrace::Tracer<detcore::GlobalState>;

/// Represents a replay that is currently running.
pub struct Replay {
    // The running tracee.
    tracer: Tracer,

    // The chroot. When dropped, everything in this directory will be
    // recursively deleted.
    chroot: TempChroot,

    // Retires this replay root's physical-file trust entries on every exit
    // path, including cancellation or a spawn/wait error.
    _materialization_scope: crate::replayer::ReplayMaterializationScope,
}

impl Replay {
    /// Spawns a new replay using the provided base directory where the replay
    /// data is stored.
    pub async fn spawn(
        dir: &Path,
        capture_output: bool,
        gdbserver: Option<u16>,
        mounts: &[Mount],
    ) -> Result<Self, Error> {
        let metadata_path = dir.join(METADATA_NAME);

        let metadata: Metadata = serde_json::from_reader(
            fs::File::open(&metadata_path)
                .with_context(|| format!("Failed to open {:?}", metadata_path))?,
        )
        .with_context(|| format!("Failed to parse {:?}", metadata_path))?;

        let recording_version = &metadata.version;
        let replayer_version = &RECORD_VERSION;
        if !replayer_version.compatible_with(recording_version) {
            return Err(anyhow::anyhow!(format!(
                "Version mismatch, recording version {:?}, replayer version {:?}",
                recording_version, replayer_version
            )));
        }

        let mut command = metadata.command();

        if capture_output {
            command.stdin(Stdio::null());
            command.stdout(Stdio::piped());
            command.stderr(Stdio::piped());
        }

        let mut config = record_or_replay_config(dir);
        // Recorder events contain the raw bytes from the recording namespace.
        // Reapply Detcore's sanitizer with the recording-time mount IDs, not
        // the unrelated IDs of this fresh replay namespace.
        config.mountinfo_root_rewrites = metadata.mountinfo_root_rewrites.clone();
        config.mountinfo_mount_ids = metadata.mountinfo_mount_ids.clone();
        config.mountinfo_mount_ids_captured = metadata.mountinfo_mount_ids_captured;
        config.fdinfo_unlisted_mount_ids = metadata.fdinfo_unlisted_mount_ids.clone();
        let sequentialize_threads = config.sequentialize_threads;

        let (chroot, bootstrap_program, materialization_scope, replay_mounts) =
            prepare_chroot(dir, &metadata, mounts)
                .context("Failed to create chroot environment")?;
        // `Command::program` resets argv[0], so restore the recorded value.
        command.program(&bootstrap_program).arg0(&metadata.arg0);

        for mount in replay_mounts {
            command.mount(mount);
        }

        // bind mount fbcode otherwise many program can fail to execve due to missing
        // shared libraries. This path only exists on Meta hosts; skip it elsewhere
        // (e.g. generic external CI runners) where the missing source would make
        // mount(2) fail with ENOENT.
        //
        // The bind-mount target directory is created here, in the parent process,
        // rather than via `Mount::touch_target()`. `touch_target` defers directory
        // creation to the cloned child immediately before `execve`, where
        // reverie-process runs it on a fixed 4 KiB clone stack (see
        // reverie-process `clone.rs`). Its `create_dir_all`/`touch_path` helpers
        // each place a `[0; PATH_MAX]` (4 KiB) buffer on that stack, overflowing
        // it and corrupting the `envp` pointer that the child then passes to
        // `execve`. That made the guest's initial `execve` fail with `EFAULT` on
        // every Meta-host replay (recording spawns without mounts, so it was
        // unaffected), so replay diverged from the recording at syscall #1.
        // Pre-creating the target keeps the child's pre-exec path allocation-free.
        let fbcode = Path::new("/usr/local/fbcode");
        if fbcode.exists() {
            // prepare_chroot created this target before recorded symlinks were
            // installed, so the controller-visible mount path cannot escape.
            let target = chroot.path().join("usr/local/fbcode");
            command.mount(Mount::bind(fbcode, &target).readonly());
            command.mount(
                Mount::new(target)
                    .flags(MountFlags::MS_BIND | MountFlags::MS_REMOUNT | MountFlags::MS_RDONLY),
            );
        }

        // Keep process-relative executable aliases (`/proc/self/exe`,
        // `/proc/thread-self/exe`, and `/proc/self/fd/N`) attached to the
        // replayed guest. Resolving those spellings in the controller would
        // select Hermit itself. The target is pre-created to avoid allocation
        // in reverie-process's small pre-exec clone stack.
        let proc_target = chroot.path().join("proc");
        command.mount(Mount::proc().target(&proc_target));

        command.chroot(chroot.path());

        let mut builder = reverie_ptrace::TracerBuilder::<ReplayTool>::new(command).config(config);
        if let Some(port) = gdbserver {
            builder = builder.gdbserver(port);
        }
        if sequentialize_threads {
            // Inform gdbserver not to serialize guests because this is
            // done by detcore already.
            builder = builder.sequentialized_guest();
        }
        let tracer = builder.spawn().await?;

        Ok(Self {
            tracer,
            chroot,
            _materialization_scope: materialization_scope,
        })
    }

    /// Waits for the replay to finish and returns its exit status.
    pub async fn wait(self) -> Result<ExitStatus, reverie::Error> {
        let (exit_status, global_state) = self.tracer.wait().await?;
        self.chroot.remove()?;
        global_state.clean_up(false, &None).await;
        Ok(exit_status)
    }

    /// Waits for the replay to finish and collects its output.
    pub async fn wait_with_output(self) -> Result<Output, reverie::Error> {
        let (output, global_state) = self.tracer.wait_with_output().await?;
        self.chroot.remove()?;
        global_state.clean_up(false, &None).await;
        Ok(output)
    }
}

fn prepare_replay_mounts(chroot: &TempChroot, mounts: &[Mount]) -> io::Result<Vec<Mount>> {
    mounts
        .iter()
        .map(|mount| {
            let target = chroot.relpath(mount.get_target());
            let source_is_file = match mount.get_source() {
                Some(source) => fs::metadata(source)?.is_file(),
                None => false,
            };
            if source_is_file {
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent)?;
                }
                match fs::symlink_metadata(&target) {
                    Ok(metadata) if metadata.file_type().is_file() => {}
                    Ok(_) => {
                        return Err(io::Error::new(
                            io::ErrorKind::AlreadyExists,
                            format!(
                                "replay mount target {} is not a regular file",
                                target.display()
                            ),
                        ));
                    }
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {
                        fs::OpenOptions::new()
                            .write(true)
                            .create_new(true)
                            .open(&target)?;
                    }
                    Err(error) => return Err(error),
                }
            } else {
                fs::create_dir_all(&target)?;
            }
            Ok(mount.clone().target(target))
        })
        .collect()
}

/// Creates the temporary chroot directory.
fn prepare_chroot(
    dir: &Path,
    metadata: &Metadata,
    mounts: &[Mount],
) -> io::Result<(
    TempChroot,
    PathBuf,
    crate::replayer::ReplayMaterializationScope,
    Vec<Mount>,
)> {
    let chroot = TempChroot::new_in(dir)?;

    // Mount targets must be created before recorded paths install symlinks.
    // prepare_replay_mounts uses controller-side path traversal, which is safe
    // only while this new chroot is empty; afterward a recorded absolute
    // symlink could redirect target creation outside the replay root.
    let replay_mounts = prepare_replay_mounts(&chroot, mounts)?;
    let fbcode = Path::new("/usr/local/fbcode");
    if fbcode.exists() {
        chroot.create_dir_all(fbcode)?;
    }
    let replay_root = crate::record_replay_path::open_directory_path(chroot.path())?;
    let materialization_scope =
        crate::replayer::ReplayMaterializationScope::new(replay_root.as_raw_fd())?;

    // These are replay-owned mount/alias points, so create them while the
    // chroot is still empty. No recorded symlink topology has been installed
    // yet, making TempChroot's path-based helpers safe here.
    chroot.create_dir_all(Path::new("/proc"))?;
    chroot.create_dir_all(Path::new("/dev"))?;
    chroot.symlink(Path::new("/proc/self/fd"), Path::new("/dev/fd"))?;

    let bootstrap = read_bootstrap_exec_event(dir, metadata)?;
    let ExecTarget::Materialize(target) = &bootstrap.target else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "recorded bootstrap exec does not contain materializable path topology",
        ));
    };
    let target_path = PathBuf::from(OsString::from_vec(target.path.clone()));
    let root_snapshot = fs::canonicalize(
        dir.join(EXEC_FILES_NAME)
            .join(bootstrap.executable.digest.to_string()),
    )?;
    let canonical_chroot = fs::canonicalize(chroot.path())?;
    let controller_relative =
        padded_bootstrap_path(&canonical_chroot, bootstrap.request.path.len())?;
    let bootstrap_program = canonical_chroot.join(&controller_relative);

    // `TracerBuilder` validates the program before entering the chroot, so use
    // a padded private copy backed by the immutable recording snapshot. The
    // replayer overwrites this trapped pathname buffer with the recorded path
    // before Linux sees the exec, so kernel-built argv/auxv/stack state remains
    // identical to recording.
    stage_recorded_exec_image(
        &replay_root,
        &replay_root,
        &root_snapshot,
        &controller_relative,
        &[],
        &bootstrap.executable,
        true,
    )?;
    stage_recorded_exec_image(
        &replay_root,
        &replay_root,
        &root_snapshot,
        &bootstrap_program,
        &[],
        &bootstrap.executable,
        true,
    )?;
    stage_recorded_exec_image(
        &replay_root,
        &replay_root,
        &root_snapshot,
        &target_path,
        &target.symlinks,
        &bootstrap.executable,
        false,
    )?;

    crate::record_replay_path::ensure_directory_path_follow_final(
        &replay_root,
        &replay_root,
        &metadata.current_dir,
    )?;
    let replay_cwd = crate::record_replay_path::resolve_existing_path(
        &replay_root,
        &replay_root,
        &metadata.current_dir,
        false,
    )?
    .object;
    for dependency in &bootstrap.dependencies {
        let path = PathBuf::from(OsString::from_vec(dependency.path.clone()));
        let start = match &dependency.base {
            crate::event::ExecMaterializationBase::Root => &replay_root,
            crate::event::ExecMaterializationBase::Cwd => &replay_cwd,
            crate::event::ExecMaterializationBase::DirectoryFd(fd) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("bootstrap dependency requires unavailable directory descriptor {fd}"),
                ));
            }
        };
        stage_recorded_exec_image(
            &replay_root,
            start,
            &dir.join(EXEC_FILES_NAME)
                .join(dependency.image.digest.to_string()),
            &path,
            &dependency.symlinks,
            &dependency.image,
            false,
        )?;
    }

    ensure_replay_standard_directories(&replay_root)?;

    Ok((
        chroot,
        bootstrap_program,
        materialization_scope,
        replay_mounts,
    ))
}

/// Chooses a controller-visible pathname whose byte length is at least the
/// recorded exec pathname, allowing the trapped pathname buffer to be safely
/// overwritten in place before kernel injection. Every component remains
/// within Linux NAME_MAX and the complete path remains below PATH_MAX.
fn padded_bootstrap_path(chroot: &Path, minimum_absolute_len: usize) -> io::Result<PathBuf> {
    const COMPONENT_LIMIT: usize = 255;
    const FINAL_COMPONENT_MINIMUM: usize = 3;

    let mut relative = PathBuf::from(".hermit-record-replay-bootstrap");
    loop {
        let prefix_len = chroot.join(&relative).as_os_str().as_bytes().len() + 1;
        let final_len = minimum_absolute_len
            .saturating_sub(prefix_len)
            .max(FINAL_COMPONENT_MINIMUM);
        if final_len <= COMPONENT_LIMIT {
            relative.push("e".repeat(final_len));
            break;
        }
        let directory_len = (final_len - COMPONENT_LIMIT).min(COMPONENT_LIMIT);
        relative.push("p".repeat(directory_len));
    }
    let absolute_len = chroot.join(&relative).as_os_str().as_bytes().len();
    if absolute_len < minimum_absolute_len || absolute_len >= libc::PATH_MAX as usize {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "cannot construct padded replay bootstrap path of at least {minimum_absolute_len} bytes"
            ),
        ));
    }
    Ok(relative)
}

/// Creates directories that may overlap recorded executable symlink topology.
/// Descriptor-only traversal keeps absolute symlink targets rooted inside the
/// replay chroot and rejects a final symlink instead of following host paths.
fn ensure_replay_standard_directories(replay_root: &OwnedFd) -> io::Result<()> {
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#653)
    // Successful recorded mkdir calls must have their parent in the replay root.
    crate::record_replay_path::ensure_directory_path_follow_final(
        replay_root,
        replay_root,
        Path::new("/tmp"),
    )?;
    crate::record_replay_path::ensure_directory_path_follow_final(
        replay_root,
        replay_root,
        Path::new("/var/tmp"),
    )
}

/// Reads the root thread's first successful exec without consuming the stream
/// used by the runtime replayer. Valid pre-exec events remain in that independent
/// runtime stream. The first successful exec is validated immediately so a later
/// exec cannot mask a corrupt bootstrap record.
fn read_bootstrap_exec_event(dir: &Path, metadata: &Metadata) -> io::Result<ExecEvent> {
    let mut events = EventReader::open(dir, &EventStreamId::root())?;
    let mut count = 0_u64;
    let exec = loop {
        let event = events.next_event().map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("no successful root exec after {count} events: {error}"),
            )
        })?;
        count = count.checked_add(1).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "root event count overflow before successful exec",
            )
        })?;
        if let Ok(SyscallEvent::Exec(exec)) = event.event {
            break exec;
        }
    };
    let expected_path = metadata.exe.as_os_str().as_bytes();
    if exec.request.dirfd != libc::AT_FDCWD
        || exec.request.flags != 0
        || exec.request.path != expected_path
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "recorded bootstrap exec request does not match metadata executable {:?}",
                metadata.exe
            ),
        ));
    }
    match &exec.target {
        ExecTarget::Materialize(materialization)
            if matches!(
                &materialization.base,
                crate::event::ExecMaterializationBase::Root
            ) && materialization.path == expected_path => {}
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "recorded bootstrap exec is missing root-relative path topology",
            ));
        }
    }
    Ok(exec)
}

/// Materializes one immutable recorded image at its original guest pathname.
/// The source is opened without following symlinks and verified by digest;
/// destination traversal is confined to the pinned replay root and follows
/// only the symlinks captured in the bootstrap event.
fn stage_recorded_exec_image(
    replay_root: &OwnedFd,
    start: &OwnedFd,
    source_path: &Path,
    guest_path: &Path,
    symlinks: &[crate::record_replay_path::ResolvedSymlink],
    image: &ExecImage,
    require_absent: bool,
) -> io::Result<()> {
    if guest_path.as_os_str().is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "bootstrap replay path is empty",
        ));
    }
    let mut source = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(source_path)?;
    let metadata = source.metadata()?;
    if !metadata.file_type().is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("bootstrap replay source is not a regular file: {source_path:?}"),
        ));
    }
    let digest = detcore::Digest::digest_reader(&mut source)?;
    if digest != image.digest {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "recorded bootstrap image digest mismatch: expected {}, found {}",
                image.digest, digest
            ),
        ));
    }
    source.seek(SeekFrom::Start(0))?;
    if require_absent {
        match crate::record_replay_path::resolve_existing_path(
            replay_root,
            start,
            guest_path,
            false,
        ) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("private replay bootstrap path already exists: {guest_path:?}"),
                ));
            }
        }
    }
    crate::record_replay_path::materialize_regular_file(
        replay_root,
        start,
        guest_path,
        symlinks,
        &mut source,
        image.digest,
        image.mode,
    )?;
    let staged =
        crate::record_replay_path::resolve_existing_path(replay_root, start, guest_path, false)?;
    crate::replayer::remember_materialized_object(
        replay_root.as_raw_fd(),
        staged.object.as_raw_fd(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_bootstrap_path_is_long_enough_without_oversized_components() {
        let chroot = Path::new("/tmp/replay-root");
        let minimum = 3_000;
        let relative = padded_bootstrap_path(chroot, minimum).unwrap();
        let absolute = chroot.join(&relative);

        assert!(absolute.as_os_str().as_bytes().len() >= minimum);
        assert!(absolute.as_os_str().as_bytes().len() < libc::PATH_MAX as usize);
        assert!(
            relative
                .components()
                .all(|component| component.as_os_str().as_bytes().len() <= 255)
        );
    }

    #[test]
    fn private_bootstrap_staging_rejects_regular_file_collision() {
        let data = tempfile::tempdir().unwrap();
        let source_path = data.path().join("source");
        fs::write(&source_path, b"recorded").unwrap();
        let chroot = TempChroot::new_in(data.path()).unwrap();
        let destination = Path::new("/private-bootstrap");
        fs::write(chroot.relpath(destination), b"collision").unwrap();
        let replay_root = crate::record_replay_path::open_directory_path(chroot.path()).unwrap();
        let image = ExecImage {
            digest: detcore::Digest::new(b"recorded"),
            mode: 0o755,
        };

        let error = stage_recorded_exec_image(
            &replay_root,
            &replay_root,
            &source_path,
            destination,
            &[],
            &image,
            true,
        )
        .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(chroot.relpath(destination)).unwrap(), b"collision");
    }

    #[test]
    fn materialization_scope_retires_pins_on_early_error() {
        let data = tempfile::tempdir().unwrap();
        let source_path = data.path().join("source");
        fs::write(&source_path, b"recorded").unwrap();
        let chroot = TempChroot::new_in(data.path()).unwrap();
        let replay_root = crate::record_replay_path::open_directory_path(chroot.path()).unwrap();
        let image = ExecImage {
            digest: detcore::Digest::new(b"recorded"),
            mode: 0o755,
        };

        let result: io::Result<()> = (|| {
            let _scope = crate::replayer::ReplayMaterializationScope::new(replay_root.as_raw_fd())?;
            stage_recorded_exec_image(
                &replay_root,
                &replay_root,
                &source_path,
                Path::new("/staged"),
                &[],
                &image,
                false,
            )?;
            assert_eq!(
                crate::replayer::registered_materialized_count(replay_root.as_raw_fd()),
                1
            );
            Err(io::Error::other("forced post-staging failure"))
        })();

        assert!(result.is_err());
        assert_eq!(
            crate::replayer::registered_materialized_count(replay_root.as_raw_fd()),
            0
        );
    }

    #[test]
    fn replay_directory_creation_cannot_follow_absolute_symlink_outside_root() {
        let data = tempfile::tempdir().unwrap();
        let chroot = TempChroot::new_in(data.path()).unwrap();
        let outside = tempfile::tempdir().unwrap();
        let canary = outside.path().join("canary");
        fs::write(&canary, b"untouched").unwrap();
        std::os::unix::fs::symlink(outside.path(), chroot.relpath(Path::new("/work"))).unwrap();

        let replay_root = crate::record_replay_path::open_directory_path(chroot.path()).unwrap();
        crate::record_replay_path::ensure_directory_path(
            &replay_root,
            &replay_root,
            Path::new("/work/created"),
        )
        .unwrap();
        ensure_replay_standard_directories(&replay_root).unwrap();

        assert!(!outside.path().join("created").exists());
        assert_eq!(fs::read(&canary).unwrap(), b"untouched");
        assert!(chroot.relpath(outside.path()).join("created").is_dir());
    }

    #[test]
    fn requested_mount_targets_are_rebased_into_the_replay_chroot() {
        let data_dir = tempfile::tempdir().unwrap();
        let chroot = TempChroot::new_in(data_dir.path()).unwrap();
        let mounts = prepare_replay_mounts(&chroot, &[Mount::tmpfs("/test")]).unwrap();

        assert_eq!(mounts.len(), 1);
        assert_eq!(mounts[0].get_target(), chroot.path().join("test"));
        assert!(chroot.path().join("test").is_dir());
    }

    #[test]
    fn mount_targets_preempt_recorded_symlink_redirection() {
        let data_dir = tempfile::tempdir().unwrap();
        let chroot = TempChroot::new_in(data_dir.path()).unwrap();
        let outside = tempfile::tempdir().unwrap();
        let mounts = prepare_replay_mounts(&chroot, &[Mount::tmpfs("/work/inside")]).unwrap();

        assert_eq!(mounts[0].get_target(), chroot.path().join("work/inside"));
        assert!(chroot.path().join("work/inside").is_dir());
        let error = std::os::unix::fs::symlink(outside.path(), chroot.path().join("work"))
            .expect_err("a later recorded symlink must not replace a prepared mount path");
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert!(!outside.path().join("inside").exists());
    }

    #[test]
    fn existing_regular_file_is_a_valid_replay_bind_target() {
        let data_dir = tempfile::tempdir().unwrap();
        let source = data_dir.path().join("source");
        fs::write(&source, b"source").unwrap();
        let chroot = TempChroot::new_in(data_dir.path()).unwrap();
        let target = chroot.path().join("mounted-file");
        fs::write(&target, b"existing").unwrap();

        let mounts =
            prepare_replay_mounts(&chroot, &[Mount::bind(&source, "/mounted-file")]).unwrap();

        assert_eq!(mounts[0].get_target(), target);
        assert_eq!(
            fs::read(chroot.path().join("mounted-file")).unwrap(),
            b"existing"
        );
    }
}
