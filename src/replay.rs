// (c) Meta Platforms, Inc. and affiliates. Confidential and proprietary.

use std::fs;
use std::io;
use std::path::Path;

use reverie::process::Command;
use reverie::process::Mount;
use reverie::process::Output;
use reverie::process::Stdio;
use reverie::ExitStatus;

use crate::chroot::TempChroot;
use crate::consts::EXE_NAME;
use crate::consts::METADATA_NAME;
use crate::error::Context;
use crate::error::Error;
use crate::interp;
use crate::metadata::record_or_replay_config;
use crate::metadata::Metadata;
use crate::metadata::RECORD_VERSION;
use crate::replayer::Replayer;
use crate::Shebang;

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
        // shared libraries.
        command.mount(
            Mount::bind(
                Path::new("/usr/local/fbcode"),
                &chroot.path().join("usr/local/fbcode"),
            )
            .recursive()
            .touch_target(),
        );

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
        let (exit_status, _global_state) = self.tracer.wait().await?;

        self.chroot.remove()?;

        Ok(exit_status)
    }

    /// Waits for the replay to finish and collects its output.
    pub async fn wait_with_output(self) -> Result<Output, reverie::Error> {
        let (output, _global_state) = self.tracer.wait_with_output().await?;

        self.chroot.remove()?;

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
    if let Some(shebang) = Shebang::new(&metadata.exe) {
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

    if let Some(interp) = interp::elf_get_interp(&metadata.exe) {
        if interp.is_file() && interp != default_ldso {
            chroot.copy_same(&interp)?;
        }
    }

    // Create the working directory.
    chroot.create_dir_all(&metadata.current_dir)?;

    Ok(chroot)
}
