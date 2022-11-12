/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::path::PathBuf;

#[derive(Default)]
pub struct HermitRunBuilder {
    pub level: Option<tracing::Level>,
    pub log_file: Option<PathBuf>,
    pub bind: Option<PathBuf>,
    pub workdir: Option<PathBuf>,
    pub workdir_destination: Option<PathBuf>,
    pub hermit_args: Vec<String>,
    pub guest_program: PathBuf,
    pub guest_args: Vec<String>,
    pub record_preemptions_to: Option<PathBuf>,
    pub replay_schedule_from: Option<PathBuf>,
}

impl HermitRunBuilder {
    #[cfg(test)]
    pub fn new(guest_program: PathBuf, guest_args: Vec<String>) -> Self {
        Self {
            guest_program,
            guest_args,
            ..Default::default()
        }
    }

    fn to_level_string(level: tracing::Level) -> &'static str {
        match level {
            tracing::Level::TRACE => "trace",
            tracing::Level::DEBUG => "debug",
            tracing::Level::INFO => "info",
            tracing::Level::WARN => "warn",
            tracing::Level::ERROR => "error",
        }
    }

    pub fn record_preemptions_to(mut self, path: PathBuf) -> Self {
        self.record_preemptions_to = Some(path);
        self
    }

    pub fn replay_schedule_from(mut self, path: PathBuf) -> Self {
        self.replay_schedule_from = Some(path);
        self
    }

    pub fn bind(mut self, path: PathBuf) -> Self {
        self.bind = Some(path);
        self
    }

    pub fn workdir_isolate(self, path: PathBuf, isolate: bool) -> Self {
        self.workdir(
            path,
            if isolate {
                Some(PathBuf::from("/tmp/out"))
            } else {
                None
            },
        )
    }

    pub fn workdir(mut self, path: PathBuf, destination: Option<PathBuf>) -> Self {
        self.workdir = Some(path);
        self.workdir_destination = destination;
        self
    }

    pub fn hermit_args(mut self, hermit_args: Vec<String>) -> Self {
        self.hermit_args = hermit_args;
        self
    }

    // this is to make sure set theese arguments via strongly typed API
    fn is_hermit_arg_applicable(arg: &String) -> bool {
        arg != "run"
            && !arg.starts_with("--record-preemptions-to")
            && !arg.starts_with("--replay-schedule-from")
    }

    fn setup_workdir(
        args: &mut Vec<String>,
        workdir: Option<PathBuf>,
        destination: Option<PathBuf>,
    ) {
        match (workdir, destination) {
            (Some(workdir), Some(destination)) => args.append(&mut vec![
                format!(
                    "--mount=type=bind,source={},target={}",
                    workdir.display(),
                    destination.display()
                ),
                format!("--workdir={}", destination.display()),
            ]),
            (Some(workdir), None) => args.push(format!("--workdir={}", workdir.display())),

            (None, Some(_)) => {
                unreachable!("current invariant doesn't allow this usecase")
            }
            (None, None) => {}
        }
    }

    pub fn into_args(self) -> Vec<String> {
        let Self {
            bind,
            guest_args,
            guest_program,
            hermit_args,
            level,
            log_file,
            workdir,
            workdir_destination,
            record_preemptions_to,
            replay_schedule_from,
        } = self;

        let mut command_args = vec![];
        command_args.push(format!(
            "--log={}",
            level.map_or("info", Self::to_level_string)
        ));

        if let Some(log_file) = log_file {
            command_args.push(format!("--log-file={}", log_file.display()));
        }

        command_args.push(String::from("run"));

        for arg in hermit_args
            .into_iter()
            .filter(Self::is_hermit_arg_applicable)
        {
            command_args.push(arg);
        }

        if let Some(record_preemptions_to) = record_preemptions_to {
            command_args.push(format!(
                "--record-preemptions-to={}",
                record_preemptions_to.display()
            ));
        }
        if let Some(replay_schedule_from) = replay_schedule_from {
            command_args.push(format!(
                "--replay-schedule-from={}",
                replay_schedule_from.display()
            ));
        }

        if let Some(bind) = bind {
            command_args.push(String::from("--bind"));
            command_args.push(bind.display().to_string());
        }

        Self::setup_workdir(&mut command_args, workdir, workdir_destination);

        command_args.push(guest_program.display().to_string());

        for guest_arg in guest_args.into_iter() {
            command_args.push(guest_arg);
        }

        command_args
    }
}

#[cfg(test)]
mod test {
    use std::path::PathBuf;
    use std::vec;

    use pretty_assertions::assert_eq;

    use super::*;

    fn assert_args(builder: HermitRunBuilder, expected_args: Vec<&str>) {
        assert_eq!(
            builder.into_args(),
            expected_args
                .into_iter()
                .map(str::to_owned)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_default_args() {
        let hermit = HermitRunBuilder::new(PathBuf::from("cat"), vec!["/proc/meminfo".to_owned()]);
        assert_args(hermit, vec!["--log=info", "run", "cat", "/proc/meminfo"]);
    }

    #[test]
    fn test_with_hermit_args() {
        let hermit = HermitRunBuilder::new(PathBuf::from("ls"), vec![])
            .hermit_args(vec!["run".to_owned(), "--detlog-stack".to_owned()]);
        assert_args(hermit, vec!["--log=info", "run", "--detlog-stack", "ls"]);
    }

    #[test]
    fn test_record_replay_args() {
        let hermit = HermitRunBuilder::new(PathBuf::from("ls"), vec![])
            .record_preemptions_to(PathBuf::from("/temp/record"))
            .replay_schedule_from(PathBuf::from("/temp/replay"));
        assert_args(
            hermit,
            vec![
                "--log=info",
                "run",
                "--record-preemptions-to=/temp/record",
                "--replay-schedule-from=/temp/replay",
                "ls",
            ],
        );
    }

    #[test]
    fn test_workdir_with_destination() {
        let hermit = HermitRunBuilder::new(PathBuf::from("ls"), vec![]).workdir(
            PathBuf::from("/tmp/runs/1/out"),
            Some(PathBuf::from("/tmp/out")),
        );
        assert_args(
            hermit,
            vec![
                "--log=info",
                "run",
                r#"--mount=type=bind,source=/tmp/runs/1/out,target=/tmp/out"#,
                "--workdir=/tmp/out",
                "ls",
            ],
        )
    }
}
