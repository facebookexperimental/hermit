/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::fs;
use std::io;
use std::path::Path;

use reverie::ExitStatus;
use reverie::process::Command;
use reverie::process::Mount;
use reverie::process::Output;
use reverie::process::Stdio;

use crate::Shebang;
use crate::chroot::TempChroot;
use crate::consts::EXE_NAME;
use crate::consts::EXEC_PATHS_NAME;
use crate::consts::METADATA_NAME;
use crate::error::Context;
use crate::error::Error;
use crate::interp;
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
}

impl Replay {
    /// Spawns a new replay using the provided base directory where the replay
    /// data is stored.
    pub async fn spawn(
        dir: &Path,
        capture_output: bool,
        gdbserver: Option<u16>,
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

        let config = record_or_replay_config(dir);
        let sequentialize_threads = config.sequentialize_threads;

        let chroot =
            prepare_chroot(dir, &metadata).context("Failed to create chroot environment")?;

        // bind mount fbcode otherwise many program can fail to execve due to missing
        // shared libraries. This path only exists on Meta hosts; skip it elsewhere
        // (e.g. generic self-hosted CI runners) where the missing source would make
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
            chroot
                .create_dir_all(fbcode)
                .context("Failed to create fbcode bind-mount target in chroot")?;
            command.mount(Mount::bind(fbcode, chroot.path().join("usr/local/fbcode")).recursive());
        }

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

        Ok(Self { tracer, chroot })
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

/// Creates the temporary chroot directory.
fn prepare_chroot(dir: &Path, metadata: &Metadata) -> io::Result<TempChroot> {
    let chroot = TempChroot::new_in(dir)?;

    let exe = dir.join(EXE_NAME);

    // Hard link the executable. Hard linking is okay here since the chroot
    // directory and the executable live on the same file system. The executable
    // is also unlikely to be modified during the program's lifetime.
    chroot.hard_link(&exe, &metadata.exe)?;
    add_executable_deps(&chroot, &metadata.exe)?;

    // Make every binary the guest exec'd during recording available inside the
    // chroot. A guest process that forks and execs another binary (e.g. a shell
    // running an external command, or a compiler driver spawning its passes)
    // would otherwise get `ENOENT` from the injected `execve` and desynchronize.
    populate_recorded_exec_paths(dir, &chroot, &metadata.exe)?;

    // Create the working directory.
    chroot.create_dir_all(&metadata.current_dir)?;

    Ok(chroot)
}

/// Copies the shared dependencies an executable needs to run inside the chroot:
/// its shebang interpreter (if it is a script) and its ELF interpreter (the
/// dynamic loader). Shared libraries do not need to be copied because their
/// contents are supplied from the recording during replay.
fn add_executable_deps(chroot: &TempChroot, exe: &Path) -> io::Result<()> {
    if let Some(shebang) = Shebang::new(exe) {
        chroot.copy_same(shebang.interpreter())?;
        // check if shebang is wrapped as #! /usr/bin/env <program>, in that
        // case, copy both /usr/bin/env and <program> (resolved)
        if let Some(program) = shebang.args().next() {
            // copy 2nd interpreter iff it is a valid program.
            if let Ok(program) = Command::new(program).find_program() {
                chroot.copy_same(&program)?;
            }
        }

        if let Ok(python3) = fs::read_link("/usr/local/bin/python3") {
            chroot.symlink(&python3, Path::new("/usr/local/bin/python3"))?;
        }
    }

    let default_ldso = Path::new("/lib64/ld-linux-x86-64.so.2");
    // FIXME: ld.so is copied over from the host system, but it really should be
    // recorded correctly.
    //
    // There are a few ways to find the path to `ld.so`.
    //  1. Parse the ELF. The path to `ld.so` can be found in the "INTERP"
    //     program header. (See `readelf -l /usr/bin/ls`.)
    //  2. Use the `AT_BASE` auxval to find the starting address of the
    //     interpreter. Then, use this to find which memory map it is associated
    //     with in `/proc/{pid}/maps`. This is the method used by RR.
    //  3. Use the `AT_PHDR`, `AT_PHNUM`, and `AT_PHENT` auxvals to read the
    //     program headers until reaching the `INTERP` program header.
    chroot.copy_same(default_ldso)?;

    if let Some(interp) = interp::elf_get_interp(exe)
        && interp.is_file()
        && interp != default_ldso
    {
        chroot.copy_same(&interp)?;
    }

    Ok(())
}

/// Populates the chroot with the executables recorded in the `exec_paths`
/// manifest (written by the recorder for every `execve`/`execveat`). The root
/// executable is already hard-linked in and is skipped. Missing or unreadable
/// entries are logged and skipped rather than aborting the whole replay.
fn populate_recorded_exec_paths(
    dir: &Path,
    chroot: &TempChroot,
    root_exe: &Path,
) -> io::Result<()> {
    let manifest = dir.join(EXEC_PATHS_NAME);
    let contents = match fs::read_to_string(&manifest) {
        Ok(contents) => contents,
        // No child ever exec'd another binary; nothing to do.
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    };

    let mut seen = std::collections::HashSet::new();
    for line in contents.lines() {
        let path = Path::new(line);
        if line.is_empty() || path == root_exe || !seen.insert(path.to_path_buf()) {
            continue;
        }
        if !path.is_file() {
            tracing::warn!(
                "Recorded exec target {:?} is not a file on the replay host; skipping",
                path
            );
            continue;
        }
        if let Err(err) = chroot
            .copy_same(path)
            .and_then(|()| add_executable_deps(chroot, path))
        {
            tracing::warn!(
                "Failed to stage exec target {:?} into chroot: {}",
                path,
                err
            );
        }
    }

    Ok(())
}
