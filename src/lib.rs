// (c) Meta Platforms, Inc. and affiliates. Confidential and proprietary.

// Treat all Clippy warnings as errors.
#![deny(clippy::all)]

mod chroot;
mod consts;
mod desync;
mod error;
mod event;
mod event_stream;
mod id;
mod interp;
mod metadata;
mod record;
mod recorder;
mod replay;
mod replayer;
mod script;

use std::fs;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

use anyhow::anyhow;
use consts::METADATA_NAME;
pub use detcore::Config as DetConfig;
pub use detcore::Detcore;
pub use detcore::RecordOrReplay;
pub use error::Context;
pub use error::Error;
pub use error::SerializableError;
pub use id::Id;
use metadata::Metadata;
use record::Record;
use replay::Replay;
pub use reverie::process;
pub use reverie::process::Command;
pub use reverie::process::Mount;
pub use reverie::process::Namespace;
pub use reverie::process::Output;
pub use reverie::process::Stdio;
pub use reverie::ExitStatus;
pub use script::Shebang;
use serde::Deserialize;
use serde::Serialize;

/// The result of recording a command.
#[derive(Debug, Serialize, Deserialize)]
pub struct Recording {
    /// The unique ID of the recording.
    pub id: Id,

    /// The exit code of the command.
    pub exit_status: ExitStatus,
}

// NOTE: A single-threaded executor is used here so that the tokio threads
// themselves wouldn't contribute non-determinism to the PID namespace. This
// could also be changed to a specific number of threads and that would be
// deterministic, but it shouldn't be based on the number of cores. When the
// thread count is based off of the number of cores in the machine, then two
// runs on different machines with a different number of cores will not be the
// same.
#[tokio::main(flavor = "current_thread")]
/// Run the given command as deterministically as possible.
pub async fn run(
    command: Command,
    config: DetConfig,
    print_summary: bool,
) -> Result<ExitStatus, Error> {
    let mut builder = reverie_ptrace::TracerBuilder::<Detcore>::new(command).config(config.clone());
    if config.gdbserver {
        builder = builder.gdbserver(config.gdbserver_port);
    }
    let (exit_status, global_state) = builder.spawn().await?.wait().await?;
    global_state.clean_up(print_summary).await; // Before it's dropped by this function.
    Ok(exit_status)
}

/// Variant of `run` that also captures stdout/stderr.
#[tokio::main(flavor = "current_thread")]
pub async fn run_with_output(
    mut command: Command,
    config: DetConfig,
    print_summary: bool,
) -> Result<Output, Error> {
    command.stdin(Stdio::null());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    let mut builder = reverie_ptrace::TracerBuilder::<Detcore>::new(command).config(config.clone());
    if config.gdbserver {
        builder = builder.gdbserver(config.gdbserver_port);
    }
    let (output, global_state) = builder.spawn().await?.wait_with_output().await?;
    global_state.clean_up(print_summary).await;
    Ok(output)
}

/// Holds the context necessary to run high-level hermit functions.
pub struct HermitData {
    // The data directory. Defaults to `~/.cache/hermit`. Note that we shouldn't
    // expect this to exist in any of the functions that are called.
    data_dir: PathBuf,
}

impl Default for HermitData {
    fn default() -> Self {
        Self::new()
    }
}

impl HermitData {
    /// Creates an instance of `HermitData` using `~/.cache/hermit` as the data
    /// directory.
    pub fn new() -> Self {
        Self::with_dir(
            dirs::cache_dir()
                .map_or_else(|| PathBuf::from("/tmp/hermit"), |dir| dir.join("hermit")),
        )
    }

    /// Creates a `HermitData` using the given directory as the base path for
    /// storing recording data.
    pub fn with_dir<P>(data_dir: P) -> Self
    where
        P: Into<PathBuf>,
    {
        Self {
            data_dir: data_dir.into(),
        }
    }

    /// Returns the path to the data directory where recordings are stored.
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// Records the execution of the given command, returning its `Recording`.
    ///
    /// If recording failed, then an error is returned. Note that if the command
    /// itself failed, then we still return a successful recording, but its exit
    /// status will be non-zero.
    pub fn record(&self, command: Command) -> Result<Recording, Error> {
        let tmp_data_dir = self.data_dir.join("tmp");

        fs::create_dir_all(&tmp_data_dir).with_context(|| {
            format!(
                "Failed to create recording directory: {}",
                self.data_dir.display()
            )
        })?;

        // Create a temporary location to store all the recorded data. This will get
        // renamed later upon a successful recording.
        let data = tempfile::TempDir::new_in(tmp_data_dir)?;

        let exit_status = record_to(command, data.path())?;

        let id = Id::unique();

        // Do an atomic rename from the temporary recording directory to the final
        // location. This is the final resting place for our data that will be used
        // during replay.
        fs::rename(data.into_path(), self.data_dir.join(id.to_string()))?;

        self.update_last_id(&id)
            .with_context(|| format!("Failed to update {:?}", self.data_dir.join("last")))?;

        Ok(Recording { id, exit_status })
    }

    /// Replays the given recording ID.
    pub fn replay(&self, id: Id) -> Result<ExitStatus, Error> {
        let data = self.data_dir.join(id.to_string());
        replay_from(&data)
    }

    /// Replays the given recording ID with a gdbserver available to attach to.
    pub fn replay_with_gdbserver(&self, id: Id, port: u16) -> Result<ExitStatus, Error> {
        let data = self.data_dir.join(id.to_string());
        replay_with_gdbserver(&data, port)
    }

    /// Returns an iterator over the recordings.
    ///
    /// Use [`recording_metadata`] to get more information about a recording.
    pub fn recordings(&self) -> impl Iterator<Item = Id> {
        fs::read_dir(&self.data_dir)
            .ok()
            .into_iter()
            .flatten()
            .filter_map(|entry| {
                let entry = entry.ok()?;

                if entry.file_type().ok()?.is_dir() {
                    Some(entry.file_name().to_str()?.parse::<Id>().ok()?)
                } else {
                    None
                }
            })
    }

    /// Returns the metadata of a recording.
    pub fn recording_metadata(&self, id: Id) -> Result<Metadata, Error> {
        let mut metadata_path = self.data_dir.join(id.to_string());
        metadata_path.push(METADATA_NAME);

        let metadata: Metadata = serde_json::from_reader(
            fs::File::open(&metadata_path)
                .with_context(|| format!("Failed to open {:?}", metadata_path))?,
        )
        .with_context(|| format!("Failed to parse {:?}", metadata_path))?;

        Ok(metadata)
    }

    /// Deletes a recording.
    pub fn remove(&self, id: Id) -> Result<(), Error> {
        let path = self.data_dir.join(id.to_string());

        // Before deleting anything, make sure this file exists. This may not be a
        // recording if this file does not exist.
        let metadata_path = path.join(METADATA_NAME);
        let metadata = fs::metadata(&metadata_path)
            .with_context(|| format!("Failed to find {:?}", &metadata_path))?;

        if !metadata.is_file() {
            return Err(anyhow!("{:?} is not a file", metadata_path));
        }

        // Do a recursive delete on the directory. Note that this does not follow
        // symlinks.
        fs::remove_dir_all(path)?;

        Ok(())
    }

    /// Returns the last recorded ID.
    pub fn last_id(&self) -> Result<Id, Error> {
        Ok(fs::read_to_string(self.data_dir.join("last"))?.parse()?)
    }

    /// Atomically updates the last recording ID.
    fn update_last_id(&self, id: &Id) -> Result<(), Error> {
        let mut file = tempfile::NamedTempFile::new_in(self.data_dir.join("tmp"))?;
        write!(file, "{}", id)?;
        file.persist(self.data_dir.join("last"))?;
        Ok(())
    }
}

impl<'a> From<Option<&'a PathBuf>> for HermitData {
    fn from(data_dir: Option<&'a PathBuf>) -> Self {
        data_dir.map_or_else(Self::new, Self::with_dir)
    }
}

/// Records to the specified directory, which must already exist.
#[tokio::main(flavor = "current_thread")]
pub async fn record_to(command: Command, dir: &Path) -> Result<ExitStatus, Error> {
    Ok(Record::spawn(command, dir).await?.wait().await?)
}

/// Records to the specified directory, which must already exist. The
/// stderr/stdout of the recording is captured in `Output`.
#[tokio::main(flavor = "current_thread")]
pub async fn record_with_output(mut command: Command, dir: &Path) -> Result<Output, Error> {
    command.stdin(Stdio::null());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());

    Ok(Record::spawn(command, dir)
        .await?
        .wait_with_output()
        .await?)
}

/// Replays from the specified directory.
#[tokio::main(flavor = "current_thread")]
pub async fn replay_from(dir: &Path) -> Result<ExitStatus, Error> {
    Ok(Replay::spawn(dir, false, None).await?.wait().await?)
}

/// Replays with a gdb server.
#[tokio::main(flavor = "current_thread")]
pub async fn replay_with_gdbserver(dir: &Path, port: u16) -> Result<ExitStatus, Error> {
    Ok(Replay::spawn(dir, false, Some(port)).await?.wait().await?)
}

/// Replays from the specified directory which must already exist. The
/// stderr/stdout of the replay is captured in `Output`.
#[tokio::main(flavor = "current_thread")]
pub async fn replay_with_output(dir: &Path) -> Result<Output, Error> {
    Ok(Replay::spawn(dir, true, None)
        .await?
        .wait_with_output()
        .await?)
}
