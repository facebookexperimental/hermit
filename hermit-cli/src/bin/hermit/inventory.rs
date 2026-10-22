/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! `hermit list` and `hermit show`: the recordings of every kind in a data
//! directory, by the one name `hermit replay` and `hermit run --from` take
//! (<https://github.com/rrnewton/hermit/issues/4271>).

use std::ffi::OsString;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use clap::Parser;
use hermit::Error;
use hermit::HermitData;
use reverie::ExitStatus;
use serde::Serialize;

use crate::manifest::Kind;
use crate::manifest::Target;

/// How many hex digits of an ID `hermit list` shows; any unique prefix of
/// at least [`crate::manifest::MIN_ID_PREFIX`] names the recording.
const SHOWN_ID_DIGITS: usize = 8;

/// Command-line options for `hermit list`.
#[derive(Debug, Parser)]
pub struct ListOpts {
    /// Directory where recordings are stored.
    #[clap(long, value_name = "DIR", env = "HERMIT_DATA_DIR")]
    data_dir: Option<PathBuf>,

    /// Print the recordings as a JSON array, newest first, each with its
    /// full `id`, `kind`, `created`, `exit` (null when not recorded),
    /// `program`, `args` and `path` (its manifest, or for a syscall
    /// recording without one its directory).
    #[clap(long)]
    json: bool,
}

/// Command-line options for `hermit show`.
#[derive(Debug, Parser)]
pub struct ShowOpts {
    /// The recording to show, by default `last`. It may be `last`,
    /// `last-run` (the newest of any kind), a recording ID or a unique
    /// prefix of at least 6 hex digits, or the path of a manifest.json or of
    /// its directory; `hermit list` lists them.
    #[clap(value_name = "RECORDING")]
    recording: Option<OsString>,

    /// Directory where recordings are stored.
    #[clap(long, value_name = "DIR", env = "HERMIT_DATA_DIR")]
    data_dir: Option<PathBuf>,
}

/// One recording, as `hermit list --json` prints it.
#[derive(Debug, Serialize)]
struct Entry {
    id: String,
    kind: &'static str,
    created: Option<String>,
    exit: Option<crate::manifest::Exit>,
    program: Option<String>,
    args: Vec<String>,
    path: PathBuf,
}

impl Entry {
    fn exit_text(&self) -> String {
        match &self.exit {
            Some(crate::manifest::Exit {
                code: Some(code), ..
            }) => code.to_string(),
            Some(crate::manifest::Exit {
                signal: Some(signal),
                ..
            }) => format!("signal {signal}"),
            _ => "-".to_owned(),
        }
    }

    fn command_text(&self) -> String {
        std::iter::once(self.program.as_deref().unwrap_or("-"))
            .chain(self.args.iter().map(String::as_str))
            .map(|word| shell_words::quote(word).into_owned())
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// Every recording in `data_dir`, newest first. A syscall recording without
/// a manifest has no `created` time and sorts last. A directory whose
/// manifest does not load is reported on stderr and left out.
fn entries(data_dir: &Path) -> Result<Vec<Entry>, Error> {
    let store = HermitData::with_dir(data_dir);
    let mut entries = Vec::new();
    for id in store.try_recordings()? {
        let directory = data_dir.join(id.to_string());
        let manifest_path = directory.join(crate::manifest::MANIFEST_NAME);
        if manifest_path.is_file() {
            match crate::manifest::read(&manifest_path) {
                Ok(manifest) => entries.push(Entry {
                    id: manifest.id,
                    kind: manifest.kind.name(),
                    created: Some(manifest.created),
                    exit: manifest.exit,
                    program: manifest.command.program,
                    args: manifest.command.args,
                    path: manifest_path,
                }),
                Err(error) => eprintln!("hermit list: skipping {}: {error:#}", directory.display()),
            }
        } else if directory.join(hermit::RECORDING_METADATA_NAME).is_file() {
            match store.recording_metadata(id) {
                Ok(metadata) => entries.push(Entry {
                    id: id.to_string(),
                    kind: Kind::FullSyscall.name(),
                    created: None,
                    exit: None,
                    program: Some(metadata.program),
                    args: metadata.args,
                    path: directory,
                }),
                Err(error) => eprintln!("hermit list: skipping {}: {error:#}", directory.display()),
            }
        }
    }
    // RFC 3339 UTC times written by one formatter sort as text.
    entries.sort_by(|left, right| {
        (right.created.is_some(), &right.created, &left.id).cmp(&(
            left.created.is_some(),
            &left.created,
            &right.id,
        ))
    });
    Ok(entries)
}

impl ListOpts {
    pub fn main(&self) -> Result<ExitStatus, Error> {
        let store = HermitData::from(self.data_dir.as_ref());
        let entries = entries(store.data_dir())?;
        if self.json {
            println!("{}", serde_json::to_string(&entries)?);
            return Ok(ExitStatus::SUCCESS);
        }
        if entries.is_empty() {
            eprintln!(
                "hermit list: {} has no recordings; `hermit record`, and a `hermit run` that \
                 draws an input from the host, each make one",
                store.data_dir().display()
            );
            return Ok(ExitStatus::SUCCESS);
        }
        println!(
            "{:<8}  {:<12}  {:<20}  {:<10}  COMMAND",
            "ID", "KIND", "CREATED", "EXIT"
        );
        for entry in &entries {
            println!(
                "{:<8}  {:<12}  {:<20}  {:<10}  {}",
                &entry.id[..SHOWN_ID_DIGITS.min(entry.id.len())],
                entry.kind,
                entry.created.as_deref().map_or_else(
                    || "-".to_owned(),
                    // To the second: `2026-10-13T19:55:00Z`.
                    |created| format!("{}Z", created.split(['.', 'Z']).next().unwrap_or(created))
                ),
                entry.exit_text(),
                entry.command_text(),
            );
        }
        Ok(ExitStatus::SUCCESS)
    }
}

impl ShowOpts {
    pub fn main(&self) -> Result<ExitStatus, Error> {
        let store = HermitData::from(self.data_dir.as_ref());
        let spec = self
            .recording
            .clone()
            .unwrap_or_else(|| hermit::LAST.into());
        let file = match crate::manifest::resolve(store.data_dir(), &spec)? {
            Target::Manifest(path) => {
                // Refuse what is not a manifest rather than print it as one.
                crate::manifest::read(&path)?;
                path
            }
            Target::Recording { data_dir, id } => {
                let metadata = data_dir
                    .join(id.to_string())
                    .join(hermit::RECORDING_METADATA_NAME);
                eprintln!(
                    "hermit show: recording {id} is a syscall recording made before hermit \
                     wrote manifests; printing its {}",
                    hermit::RECORDING_METADATA_NAME
                );
                metadata
            }
        };
        let text = std::fs::read_to_string(&file)
            .with_context(|| format!("cannot read {}", file.display()))?;
        print!("{text}");
        if !text.ends_with('\n') {
            println!();
        }
        Ok(ExitStatus::SUCCESS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest;

    fn write_manifest(data_dir: &Path, kind: Kind, created: &str) -> hermit::Id {
        let id = hermit::Id::unique();
        let mut document = manifest::Manifest::new(
            id,
            kind,
            manifest::Command {
                program: Some("/bin/echo".to_owned()),
                args: vec!["two words".to_owned()],
                cwd: None,
            },
            manifest::Inputs::default(),
        );
        document.created = created.to_owned();
        manifest::write(
            &data_dir.join(id.to_string()).join(manifest::MANIFEST_NAME),
            &document,
        )
        .unwrap();
        id
    }

    #[test]
    fn a_list_is_newest_first_and_skips_what_is_not_a_recording() {
        let data = tempfile::tempdir().unwrap();
        let old = write_manifest(data.path(), Kind::InputsOnly, "2026-10-13T19:55:00.000000Z");
        let new = write_manifest(data.path(), Kind::Network, "2026-10-13T19:58:00.000000Z");
        std::fs::create_dir_all(data.path().join("tmp")).unwrap();
        std::fs::create_dir_all(data.path().join(hermit::Id::unique().to_string())).unwrap();
        let listed = entries(data.path()).unwrap();
        let ids: Vec<_> = listed.iter().map(|entry| entry.id.clone()).collect();
        assert_eq!(ids, [new.to_string(), old.to_string()]);
        assert_eq!(listed[0].kind, "network");
        assert_eq!(listed[0].command_text(), "/bin/echo 'two words'");
        assert_eq!(listed[0].exit_text(), "-");
    }
}
