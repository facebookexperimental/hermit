/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::path::Path;
use std::path::PathBuf;

use super::TemporaryEnvironment;

pub struct TestArtifacts {
    pub test_result_artifact_dir: PathBuf,
}

impl TestArtifacts {
    pub fn new(test_result_artifact_dir: PathBuf) -> TestArtifacts {
        TestArtifacts {
            test_result_artifact_dir,
        }
    }

    pub fn copy_run_results(&self, tmp_env: &TemporaryEnvironment) -> anyhow::Result<()> {
        if !Path::new(&self.test_result_artifact_dir).exists() {
            std::fs::create_dir_all(&self.test_result_artifact_dir)?;
        }
        let mut env_number = 1;
        for env in tmp_env.runs().iter() {
            self.copy_file(&env.log_file_path, env_number)?;
            self.copy_file(&env.std_out_file_path, env_number)?;
            self.copy_file(&env.std_err_file_path, env_number)?;
            self.copy_file(&env.exit_status_file_path, env_number)?;
            self.copy_file(&env.schedule_file, env_number)?;
            env_number += 1;
        }

        Ok(())
    }

    fn copy_file(&self, file_path: &PathBuf, run_number: i32) -> anyhow::Result<()> {
        if file_path.as_path().exists() {
            let file_name = file_path.file_name();
            if let Some(file) = file_name {
                let new_file_name = format!("{}{}", run_number, file.to_str().unwrap());
                let new_file_path = self.test_result_artifact_dir.as_path().join(new_file_name);
                std::fs::copy(file_path, new_file_path)?;
            }
        } else {
            println!("File not exists {}", file_path.display());
        }
        Ok(())
    }
}

#[cfg(test)]
mod test {
    use tempfile::tempdir;

    use super::super::TemporaryEnvironmentBuilder;

    #[test]
    fn all_files_copied_to_artifacts_dir() -> anyhow::Result<()> {
        let env = TemporaryEnvironmentBuilder::new()
            .persist_temp_dir(false)
            .run_count(2)
            .build()?;
        let artifactd_dir = tempdir()?;
        let artifacts = super::TestArtifacts::new(artifactd_dir.path().to_path_buf());
        artifacts.copy_run_results(&env)?;

        let files = std::fs::read_dir(artifactd_dir.path())?;
        let expected_artifacts = vec![
            format!("{}/1log", artifactd_dir.path().display()),
            format!("{}/1std_out", artifactd_dir.path().display()),
            format!("{}/1std_err", artifactd_dir.path().display()),
            format!("{}/1exit_status", artifactd_dir.path().display()),
            format!("{}/1sched", artifactd_dir.path().display()),
            format!("{}/2log", artifactd_dir.path().display()),
            format!("{}/2std_out", artifactd_dir.path().display()),
            format!("{}/2std_err", artifactd_dir.path().display()),
            format!("{}/2exit_status", artifactd_dir.path().display()),
            format!("{}/2sched", artifactd_dir.path().display()),
        ];

        let copied_artifacts: Vec<String> = files
            .into_iter()
            .map(|p| format!("{}", p.unwrap().path().display()))
            .collect();

        assert_eq!(expected_artifacts, copied_artifacts);
        Ok(())
    }

    #[test]
    fn artifacts_dir_not_exists() -> anyhow::Result<()> {
        let env = TemporaryEnvironmentBuilder::new()
            .persist_temp_dir(false)
            .run_count(2)
            .build()?;
        let root_artifacts_dir = tempdir()?;
        let artifacts_dir = root_artifacts_dir.path().join("sub/artifacts");
        super::TestArtifacts::new(artifacts_dir.to_owned()).copy_run_results(&env)?;
        assert!(
            artifacts_dir.exists(),
            "Artifacts dir hierarchy should be ensured if not exists"
        );
        Ok(())
    }
}
