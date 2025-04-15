/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::fs::File;
use std::fs::create_dir;
use std::path::Path;
use std::path::PathBuf;

use colored::Colorize;
use tempfile::TempDir;
use tempfile::tempdir_in;

pub struct TemporaryEnvironmentBuilder {
    // Whether to keep temp directory after cleaning
    keep_temp_dir: bool,
    // Count of temp environments to prepare
    run_count: i32,

    // Path to test artifacts directory usually in test environment
    temp_dir_path: PathBuf,

    // Whether to be chatty.
    verbose: bool,
}

impl TemporaryEnvironmentBuilder {
    pub fn new() -> TemporaryEnvironmentBuilder {
        TemporaryEnvironmentBuilder {
            keep_temp_dir: false,
            run_count: 2,
            temp_dir_path: std::env::temp_dir(),
            verbose: false,
        }
    }

    pub fn temp_dir_path(mut self, path: Option<&PathBuf>) -> TemporaryEnvironmentBuilder {
        if let Some(p) = path {
            self.keep_temp_dir = true;
            self.temp_dir_path = p.to_owned();
        }
        self
    }

    pub fn persist_temp_dir(mut self, persist: bool) -> TemporaryEnvironmentBuilder {
        self.keep_temp_dir = persist;
        self
    }

    pub fn verbose(mut self, verbose: bool) -> TemporaryEnvironmentBuilder {
        self.verbose = verbose;
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

    fn create_root_temp_dir(&self) -> anyhow::Result<TempDir> {
        if !Path::new(&self.temp_dir_path).exists() {
            std::fs::create_dir_all(&self.temp_dir_path)?;
        }

        Ok(tempdir_in(&self.temp_dir_path)?)
    }

    pub fn build(self) -> anyhow::Result<TemporaryEnvironment> {
        println!("Preparing temporary environment to run experiments");
        let root_temp_dir = self.create_root_temp_dir()?;
        println!(
            "{}",
            format!("  Root dir created: {}", root_temp_dir.path().display()).dimmed()
        );

        let mut result = TemporaryEnvironment::new();

        for i in 1..=self.run_count {
            println!("Start initializing environment for execution #{}", i);
            let run_temp_dir_path = self.resolve_dir_for_run(root_temp_dir.path(), i);
            create_dir(&run_temp_dir_path)?;
            println!(
                "{}",
                format!("  Temp dir created: {:?}", run_temp_dir_path).dimmed()
            );
            let workdir = run_temp_dir_path.join("workdir");
            create_dir(workdir.as_path())?;
            if self.verbose {
                println!("  {}", format!("Work dir created: {:?}", &workdir).dimmed());
            }

            let log_file_path = self.create_file(&run_temp_dir_path, "log")?;
            if self.verbose {
                println!(
                    "{}",
                    format!("  Log file created: {:?}", log_file_path).dimmed()
                );
            }

            let std_out_path = self.create_file(&run_temp_dir_path, "std_out")?;
            if self.verbose {
                println!("{}", format!("  Std out path: {:?}", std_out_path).dimmed());
            }

            let std_err_path = self.create_file(&run_temp_dir_path, "std_err")?;
            if self.verbose {
                println!("{}", format!("  Std err path: {:?}", std_err_path).dimmed());
            }

            let exit_status_file_path = self.create_file(&run_temp_dir_path, "exit_status")?;
            if self.verbose {
                println!(
                    "{}",
                    format!("  Exit status path: {:?}", exit_status_file_path).dimmed()
                );
            }

            let schedules_path = self.create_file(&run_temp_dir_path, "sched")?;
            println!("{}", format!("  Sched path: {:?}", schedules_path).dimmed());

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
    #[allow(dead_code)]
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
    use std::path::Path;
    use std::path::PathBuf;

    use pretty_assertions::assert_eq;
    use tempfile::TempDir;
    use tempfile::tempdir;

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
    fn test_root_path_specified() -> anyhow::Result<()> {
        let temp_dir = tempdir()?;
        fn create_tmp_env(dir: &TempDir) -> anyhow::Result<PathBuf> {
            let temp_dir_path = dir.path();
            let env = super::TemporaryEnvironmentBuilder::new()
                .temp_dir_path(Some(&temp_dir_path.to_path_buf()))
                .run_count(1)
                .build()?;
            Ok(get_temp_dir_path(&env.path))
        }
        let temp_dir_path = create_tmp_env(&temp_dir)?;
        let temp_root_exists = temp_dir_path.is_dir();
        assert_eq!(
            temp_dir_path.starts_with(temp_dir.path()),
            true,
            "Should create environment in temp_dir_path if specified"
        );
        assert_eq!(
            temp_root_exists, true,
            "Should not clean temp direrctory if temp_dir_path specified"
        );

        Ok(())
    }

    #[test]
    fn test_root_temp_dir_specified_but_not_exists() -> anyhow::Result<()> {
        let temp_dir = tempdir()?;
        let root_dir_path = format!("{}/not_exists", temp_dir.path().display());
        fn create_tmp_env(path: &str) -> anyhow::Result<PathBuf> {
            let env = super::TemporaryEnvironmentBuilder::new()
                .temp_dir_path(Some(&PathBuf::from(&path)))
                .run_count(1)
                .build()?;
            Ok(get_temp_dir_path(&env.path))
        }
        create_tmp_env(&root_dir_path)?;
        assert_eq!(
            Path::new(&root_dir_path).exists(),
            true,
            "If not existing path to root dir provided, it should be created"
        );

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
