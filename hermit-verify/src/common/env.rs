/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::fs::create_dir;
use std::fs::File;
use std::path::Path;
use std::path::PathBuf;

use colored::Colorize;
use tempfile::tempdir;
use tempfile::TempDir;

pub struct TemporaryEnvironmentBuilder {
    // Whether to keep temp directory after cleaning
    keep_temp_dir: bool,
    // Count of temp environments to prepare
    run_count: i32,
}

impl TemporaryEnvironmentBuilder {
    pub fn new() -> TemporaryEnvironmentBuilder {
        TemporaryEnvironmentBuilder {
            keep_temp_dir: false,
            run_count: 2,
        }
    }

    pub fn persist_temp_dir(mut self, persist: bool) -> TemporaryEnvironmentBuilder {
        self.keep_temp_dir = persist;
        self
    }

    pub fn run_count(mut self, run_count: i32) -> TemporaryEnvironmentBuilder {
        self.run_count = run_count;
        self
    }

    fn resolve_dir_for_run(&self, root_path: &Path, execution_num: i32) -> PathBuf {
        root_path.join(execution_num.to_string())
    }

    fn create_file(&self, root_dir_path: &Path, file_name: &str) -> anyhow::Result<PathBuf> {
        let file_path = root_dir_path.join(file_name);
        File::create(&file_path)?;
        Ok(file_path)
    }

    pub fn build(self) -> anyhow::Result<TemporaryEnvironment> {
        println!("Preparing temporary environment to run experiments");
        let root_temp_dir = tempdir()?;
        println!(
            "{}",
            format!("Root dir created: {}", root_temp_dir.path().display()).dimmed()
        );

        let mut result = TemporaryEnvironment::new();

        for i in 1..=self.run_count {
            println!("Start initializing environment for execution #{}", i);
            let run_temp_dir_path = self.resolve_dir_for_run(root_temp_dir.path(), i);
            create_dir(&run_temp_dir_path)?;
            println!(
                "{}",
                format!("Temp dir created: {:?}", run_temp_dir_path).dimmed()
            );
            let workdir = run_temp_dir_path.join("workdir");
            create_dir(workdir.as_path())?;
            println!("{}", format!("Work dir created: {:?}", &workdir).dimmed());

            let log_file_path = self.create_file(&run_temp_dir_path, "log")?;
            println!(
                "{}",
                format!("Log file created: {:?}", log_file_path).dimmed()
            );

            let std_out_path = self.create_file(&run_temp_dir_path, "std_out")?;
            println!("{}", format!("Std out path: {:?}", std_out_path).dimmed());

            let std_err_path = self.create_file(&run_temp_dir_path, "std_err")?;
            println!("{}", format!("Std err path: {:?}", std_err_path).dimmed());

            let exit_status_file_path = self.create_file(&run_temp_dir_path, "exit_status")?;
            println!(
                "{}",
                format!("Exit status path: {:?}", exit_status_file_path).dimmed()
            );

            let schedules_path = self.create_file(&run_temp_dir_path, "sched")?;
            println!("{}", format!("Sched path: {:?}", schedules_path).dimmed());

            result.run_envs.push(RunEnvironment {
                temp_dir: run_temp_dir_path,
                log_file_path,
                std_out_file_path: std_out_path,
                std_err_file_path: std_err_path,
                exit_status_file_path,
                schedule_file: schedules_path,
                workdir,
            });

            println!("Environment for execution #{} initialized", i);
        }

        if self.keep_temp_dir {
            // Temp dir is persisted now.
            result.path = EnvPath::Path(root_temp_dir.into_path());
        } else {
            // Store ref to temp dir to bind it lifetime to environment lifetime and prevent it from been cleaned.
            result.path = EnvPath::Temp(root_temp_dir);
        }

        Ok(result)
    }
}

pub struct TemporaryEnvironment {
    // Temporary dir instance. If dir is persisted it will conain path to dir if not it will contain temp dir instance.
    path: EnvPath,

    // Running environments
    run_envs: Vec<RunEnvironment>,
}

impl TemporaryEnvironment {
    fn new() -> TemporaryEnvironment {
        TemporaryEnvironment {
            path: EnvPath::Path(PathBuf::new()),
            run_envs: vec![],
        }
    }

    pub fn runs(&self) -> &[RunEnvironment] {
        &self.run_envs
    }

    pub fn path(&self) -> &Path {
        self.path.path()
    }
}

pub enum EnvPath {
    Temp(TempDir),
    Path(PathBuf),
}

impl EnvPath {
    pub fn path(&self) -> &Path {
        match self {
            EnvPath::Temp(path) => path.path(),
            EnvPath::Path(path) => path.as_path(),
        }
    }
}

#[derive(Clone)]
pub struct RunEnvironment {
    // Root directory for one run
    pub temp_dir: PathBuf,

    // Path to log file
    pub log_file_path: PathBuf,

    // Path to execution std out file
    pub std_out_file_path: PathBuf,

    // Path to execution std err file
    pub std_err_file_path: PathBuf,

    // Path to exit status file
    pub exit_status_file_path: PathBuf,

    // Path to schedule file to be recorded
    pub schedule_file: PathBuf,

    // Path to an isolated CWD
    pub workdir: PathBuf,
}

#[cfg(test)]
mod test {
    use std::path::PathBuf;

    use pretty_assertions::assert_eq;

    #[test]
    fn test_create_env_for_every_run() -> anyhow::Result<()> {
        let env = super::TemporaryEnvironmentBuilder::new()
            .persist_temp_dir(false)
            .run_count(2)
            .build()?;

        let path = env.path.path().display().to_string();

        let result: Vec<_> = env
            .runs()
            .iter()
            .map(|run| std::fs::read_dir(run.temp_dir.as_path()))
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .flatten()
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .map(|entry| {
                entry
                    .path()
                    .display()
                    .to_string()
                    // since every file in the directory starts with a tempdir we strip it out to easier compare
                    .trim_start_matches(path.as_str())
                    .to_owned()
            })
            .collect();

        let expected = vec![
            "/1/workdir",
            "/1/log",
            "/1/std_out",
            "/1/std_err",
            "/1/exit_status",
            "/1/sched",
            "/2/workdir",
            "/2/log",
            "/2/std_out",
            "/2/std_err",
            "/2/exit_status",
            "/2/sched",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();

        assert_eq!(result, expected);

        Ok(())
    }

    #[test]
    fn test_clean_temp_dir() -> anyhow::Result<()> {
        let temp_dir_path = create_tmp_env(false)?;
        let temp_root_exists = temp_dir_path.is_dir();
        assert_eq!(
            temp_root_exists, false,
            "Should clean temp directory when persist is not specified"
        );
        Ok(())
    }

    #[test]
    fn test_persist_temp_dir() -> anyhow::Result<()> {
        let temp_dir_path = create_tmp_env(true)?;
        let temp_root_exists = temp_dir_path.is_dir();
        assert_eq!(
            temp_root_exists, true,
            "Should keep temp dir if persist specified"
        );
        std::fs::remove_dir_all(temp_dir_path)?;
        Ok(())
    }

    #[test]
    fn test_clean_temp_dir_by_default() -> anyhow::Result<()> {
        fn create_tmp_env() -> anyhow::Result<PathBuf> {
            let env = super::TemporaryEnvironmentBuilder::new()
                .run_count(1)
                .build()?;
            Ok(get_temp_dir_path(&env.path))
        }

        let temp_dir_path = create_tmp_env()?;
        let temp_root_exists = temp_dir_path.is_dir();
        assert_eq!(
            temp_root_exists, false,
            "Should clean temp direrctory when persist is not specified"
        );
        Ok(())
    }

    fn create_tmp_env(keep_temp_dir: bool) -> anyhow::Result<PathBuf> {
        let env = super::TemporaryEnvironmentBuilder::new()
            .persist_temp_dir(keep_temp_dir)
            .run_count(1)
            .build()?;

        Ok(get_temp_dir_path(&env.path))
    }

    fn get_temp_dir_path(env_path: &super::EnvPath) -> PathBuf {
        match env_path {
            super::EnvPath::Path(path) => path.clone(),
            super::EnvPath::Temp(dir) => dir.path().to_path_buf(),
        }
    }
}
