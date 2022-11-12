/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::ffi::OsStr;
use std::fs::OpenOptions;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;

use self::run::HermitRunBuilder;
use crate::common::RunEnvironment;

mod run;
#[derive(Default)]
pub struct Hermit {
    log_level: Option<tracing::Level>,
    log_file: Option<PathBuf>,
}

impl Hermit {
    pub fn new() -> Self {
        Self {
            ..Default::default()
        }
    }

    pub fn log_level(mut self, level: tracing::Level) -> Self {
        self.log_level = Some(level);
        self
    }

    pub fn log_file(mut self, file: PathBuf) -> Self {
        self.log_file = Some(file);
        self
    }

    pub fn run(self, guest_program: PathBuf, args: Vec<String>) -> HermitRunBuilder {
        HermitRunBuilder {
            log_file: self.log_file,
            level: self.log_level,
            guest_program,
            guest_args: args,
            ..Default::default()
        }
    }
}

pub fn build_hermit_cmd(
    hermit_bin: &PathBuf,
    args: Vec<String>,
    run: &RunEnvironment,
) -> std::io::Result<std::process::Command> {
    let mut command = std::process::Command::new(hermit_bin);
    command.args(args);
    command.stdout(Stdio::from(
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(run.std_out_file_path.as_path())?,
    ));
    command.stderr(Stdio::from(
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(run.std_err_file_path.as_path())?,
    ));
    Ok(command)
}

pub fn display_cmd(command: &Command) -> String {
    format!(
        "{} {}",
        command.get_program().to_string_lossy(),
        command
            .get_args()
            .into_iter()
            .map(|x| x.to_os_string())
            .collect::<Vec<_>>()
            .join(OsStr::new(" "))
            .to_string_lossy()
    )
}
