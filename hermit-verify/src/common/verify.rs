/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::ffi::OsStr;
use std::io::BufRead;
use std::io::BufReader;
use std::path::Path;

use colored::Colorize;

use super::RunEnvironment;
use crate::cli_wrapper::display_cmd;

pub struct Verify<P: AsRef<OsStr>> {
    hermit_bin: P,
}

/// options to pass for log diff tool
#[derive(Default)]
pub struct LogDiffOptions {
    /// Number of syscalls to display when failure
    pub syscall_history: usize,
    /// Whether to skip commits for determinism check
    pub skip_commits: bool,
    /// Whether to skip syscalls for determinism check
    pub skip_detlog_syscalls: bool,
    /// Whether to skip syscall results for determinism check
    pub skip_detlog_syscall_results: bool,
    /// Whether to skip the rest of DETLOGs for determinism check
    pub skip_detlog_others: bool,
    /// Which lines to ignore for DETLOGs
    pub ignore_lines: Vec<String>,
}

impl LogDiffOptions {
    fn into_args(self) -> Vec<String> {
        // hermit-verify compares ordinary Hermit logs with the all-records
        // envelope, which is the standalone command's default. It is NOT named
        // explicitly here: `hermit_bin` defaults to whatever `HERMIT_BIN` or
        // the PATH supplies, which can be a published bundle built without the
        // flag, and passing it there is `error: unexpected argument`, exit 2.
        let mut result: Vec<String> = Vec::new();
        if self.skip_detlog_others && self.skip_detlog_syscalls && self.skip_detlog_syscall_results
        {
            result.push("--skip-detlog".to_owned());
        } else {
            if !self.skip_detlog_others {
                result.push("--include-detlogs=other".to_owned());
            }
            if !self.skip_detlog_syscall_results {
                result.push("--include-detlogs=syscallresult".to_owned());
            }
            if !self.skip_detlog_syscalls {
                result.push("--include-detlogs=syscall".to_owned());
            }
        }

        if self.skip_commits {
            result.push("--skip-commit".to_owned())
        }

        result.push(format!("--syscall-history={}", self.syscall_history));

        for ignore_lines in self.ignore_lines {
            result.push(format!("--ignore-lines={}", ignore_lines));
        }

        result
    }
}

impl<P: AsRef<OsStr>> Verify<P> {
    pub fn new(hermit_bin: P) -> Self {
        Self { hermit_bin }
    }

    /// Whether the child hermit's `log-diff` offers
    /// `--canonicalize-host-addresses`. `hermit_bin` comes from `HERMIT_BIN` or
    /// the PATH, so it can be a published bundle that predates the flag, where
    /// passing it is `error: unexpected argument` (see
    /// `log_diff_command_does_not_pin_a_record_envelope_the_child_may_lack`).
    fn child_offers_host_address_canonicalization(&self) -> bool {
        std::process::Command::new(&self.hermit_bin)
            .args(["log-diff", "--help"])
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .is_ok_and(|output| {
                output.status.success()
                    && String::from_utf8_lossy(&output.stdout)
                        .contains("--canonicalize-host-addresses")
            })
    }

    fn verify_lines<S1: AsRef<str>, S2: AsRef<str>>(left: S1, right: S2) -> anyhow::Result<bool> {
        let result = similar::TextDiff::configure()
            .algorithm(similar::Algorithm::Myers)
            .diff_lines(left.as_ref(), right.as_ref());

        if result.ratio() == 1.0 {
            Ok(true)
        } else {
            for c in result.iter_all_changes() {
                match c.tag() {
                    similar::ChangeTag::Equal => print!("{}", c),
                    similar::ChangeTag::Delete => {
                        print!("{}", format!("-{}", c).red().bold())
                    }
                    similar::ChangeTag::Insert => {
                        print!("{}", format!("+{}", c).green().bold())
                    }
                }
            }
            Ok(false)
        }
    }

    fn verify_files(left: &Path, right: &Path) -> anyhow::Result<bool> {
        let left = std::fs::read_to_string(left)?;
        let right = std::fs::read_to_string(right)?;
        Self::verify_lines(left, right)
    }

    /// Every difference between two schedule files in what priority-history
    /// replay must reproduce beside the global events: each per-thread
    /// history, whole, and each callback checkpoint. A key present on one
    /// side only is a difference; absent on both is equal. Nothing is
    /// projected, reordered or normalized.
    fn priority_history_differences(
        left: &serde_json::Value,
        right: &serde_json::Value,
    ) -> Vec<String> {
        let mut differences = Vec::new();
        for key in ["per_thread", "callback_checkpoints"] {
            match (left.get(key), right.get(key)) {
                (None, None) => {}
                (Some(_), None) => differences.push(format!("{key}: in the first run only")),
                (None, Some(_)) => differences.push(format!("{key}: in the second run only")),
                (Some(left), Some(right)) => {
                    Self::value_differences(key.to_owned(), left, right, &mut differences)
                }
            }
        }
        differences
    }

    /// The number of per-thread histories, their preemption rows, and the
    /// callback checkpoints in one schedule file.
    fn priority_history_extent(schedule: &serde_json::Value) -> (usize, usize, usize) {
        let threads = schedule.get("per_thread").and_then(|t| t.as_object());
        let rows = threads.map_or(0, |threads| {
            threads
                .values()
                .filter_map(|t| t.get("preemption_rcbs")?.as_array().map(Vec::len))
                .sum()
        });
        let checkpoints = schedule
            .get("callback_checkpoints")
            .and_then(|c| c.as_object())
            .map_or(0, |c| {
                c.values().filter_map(|t| t.as_array().map(Vec::len)).sum()
            });
        (threads.map_or(0, |t| t.len()), rows, checkpoints)
    }

    fn value_differences(
        path: String,
        left: &serde_json::Value,
        right: &serde_json::Value,
        differences: &mut Vec<String>,
    ) {
        use serde_json::Value;
        if left == right {
            return;
        }
        match (left, right) {
            (Value::Object(left), Value::Object(right)) => {
                let keys: std::collections::BTreeSet<&String> =
                    left.keys().chain(right.keys()).collect();
                for key in keys {
                    match (left.get(key), right.get(key)) {
                        (Some(l), Some(r)) => {
                            Self::value_differences(format!("{path}.{key}"), l, r, differences)
                        }
                        (Some(_), None) => {
                            differences.push(format!("{path}.{key}: in the first run only"))
                        }
                        (None, Some(_)) => {
                            differences.push(format!("{path}.{key}: in the second run only"))
                        }
                        (None, None) => unreachable!(),
                    }
                }
            }
            (Value::Array(l), Value::Array(r)) => {
                if l.len() != r.len() {
                    differences.push(format!("{path}: {} entries vs {}", l.len(), r.len()));
                }
                for (index, (l, r)) in l.iter().zip(r).enumerate() {
                    Self::value_differences(format!("{path}[{index}]"), l, r, differences);
                }
            }
            _ => differences.push(format!("{path}: {left} vs {right}")),
        }
    }

    /// The fixed point of priority-history replay, which re-records what it
    /// replays: the two schedule files must agree exactly on every
    /// per-thread priority history and every callback checkpoint.
    /// `verify_schedules`, which compares only the global events, is
    /// unchanged and checked separately.
    pub fn verify_priority_histories(
        &self,
        left: &RunEnvironment,
        right: &RunEnvironment,
    ) -> anyhow::Result<bool> {
        println!(
            "{}",
            "::  Checking that per-thread priority histories and callback checkpoints match".bold()
        );
        let read = |path: &Path| -> anyhow::Result<serde_json::Value> {
            Ok(serde_json::from_reader(BufReader::new(
                std::fs::File::open(path)?,
            ))?)
        };
        let (left, right) = (read(&left.schedule_file)?, read(&right.schedule_file)?);
        // Name what was compared, so an exact agreement of two empty
        // histories is never mistaken for coverage.
        let (threads, rows, checkpoints) = Self::priority_history_extent(&left);
        println!(
            "   first run: {threads} per-thread histories, {rows} preemption rows, \
             {checkpoints} callback checkpoints"
        );
        let differences = Self::priority_history_differences(&left, &right);
        const SHOWN: usize = 20;
        for difference in differences.iter().take(SHOWN) {
            println!("{}", difference.red().bold());
        }
        if differences.len() > SHOWN {
            println!("... and {} more differences", differences.len() - SHOWN);
        }
        Ok(differences.is_empty())
    }

    fn format_json<TPath: AsRef<Path>>(path: TPath) -> anyhow::Result<String> {
        let value: serde_json::Value =
            serde_json::from_reader(BufReader::new(std::fs::File::open(path.as_ref())?))?;
        let schedules = match &value {
            serde_json::Value::Object(sched_file) => sched_file.get("global").ok_or_else(|| {
                anyhow::Error::msg("expecting \"global\" key in the target json file")
            }),
            _ => Err(anyhow::Error::msg(format!(
                "{} has unexpected format",
                path.as_ref().display()
            ))),
        };
        Ok(serde_json::to_string_pretty(schedules?)?)
    }

    pub fn verify_schedules(
        &self,
        left: &RunEnvironment,
        right: &RunEnvironment,
    ) -> anyhow::Result<bool> {
        println!(
            "{}",
            "::  Checking that event schedules match (fixed point)".bold()
        );
        Self::verify_lines(
            Self::format_json(&left.schedule_file)?,
            Self::format_json(&right.schedule_file)?,
        )
    }

    pub fn verify_stdout(
        &self,
        left: &RunEnvironment,
        right: &RunEnvironment,
    ) -> anyhow::Result<bool> {
        println!("{}", "::  Comparing stdout".bold());
        Self::verify_files(
            left.std_out_file_path.as_path(),
            right.std_out_file_path.as_path(),
        )
    }

    pub fn verify_stderr(
        &self,
        left: &RunEnvironment,
        right: &RunEnvironment,
    ) -> anyhow::Result<bool> {
        println!("{}", "::  Comparing stderr".bold());
        Self::verify_files(
            left.std_err_file_path.as_path(),
            right.std_err_file_path.as_path(),
        )
    }

    pub fn verify_desync(&self, right: &RunEnvironment) -> anyhow::Result<bool> {
        //FIXME: extract log-diff out of hermit and handle DESYNCs there (T135657122 + some extra work)
        println!("{}", "::  Looking for desync events:".bold());
        let buffer = BufReader::new(std::fs::File::open(&right.log_file_path)?);
        for line in buffer.lines() {
            if line?.contains("DESYNC") {
                println!("{}", "WARNING: DESYNCs found".red());
                return Ok(false);
            }
        }
        println!("DESYNC events not found");
        Ok(true)
    }

    /// The `log-diff` argument vector. `canonicalize_host_addresses` says
    /// whether the child hermit offers `--canonicalize-host-addresses`.
    ///
    /// The two logs come from two hermit processes, so ASLR moves every marked
    /// launcher pointer (`<hostaddr 0x...>`) between them; compared by raw
    /// value they always differ. The flag compares them by per-run ordinal
    /// instead, keeping identity, order and aliasing
    /// (<https://github.com/rrnewton/hermit/issues/3412>).
    fn build_command_args(
        &self,
        left: &RunEnvironment,
        right: &RunEnvironment,
        options: LogDiffOptions,
        canonicalize_host_addresses: bool,
    ) -> Vec<String> {
        let mut result = vec![];
        result.push(String::from("log-diff"));
        result.append(&mut options.into_args());
        if canonicalize_host_addresses {
            result.push("--canonicalize-host-addresses".to_owned());
        }
        result.push(format!("{}", left.log_file_path.display()));
        result.push(format!("{}", right.log_file_path.display()));

        result
    }

    pub fn verify_logs(
        &self,
        left: &RunEnvironment,
        right: &RunEnvironment,
        options: LogDiffOptions,
    ) -> anyhow::Result<bool> {
        println!("{}", "::  Comparing log files".bold());
        let canonicalize_host_addresses = self.child_offers_host_address_canonicalization();
        if !canonicalize_host_addresses {
            println!(
                "    (this hermit's log-diff has no --canonicalize-host-addresses; \
                 marked host addresses are compared by raw value)"
            );
        }
        let mut command = std::process::Command::new(&self.hermit_bin);
        command.args(self.build_command_args(left, right, options, canonicalize_host_addresses));

        println!("{}", format!("    {}", display_cmd(&command)).bold());
        Ok(command.status()?.success())
    }

    pub fn verify_exit_statuses(
        &self,
        left: &RunEnvironment,
        right: &RunEnvironment,
    ) -> anyhow::Result<bool> {
        println!("{}", "::  Comparing exit codes".bold());
        Self::verify_files(
            left.exit_status_file_path.as_path(),
            right.exit_status_file_path.as_path(),
        )
    }
}

#[cfg(test)]
mod test {
    use std::fs::File;
    use std::io::Write;
    use std::path::Path;
    use std::path::PathBuf;

    use pretty_assertions::assert_eq;

    use super::*;
    use crate::common::TemporaryEnvironmentBuilder;

    /// A schedule file as priority-history replay writes it: global events,
    /// one thread's history and one callback checkpoint.
    fn priority_schedule() -> serde_json::Value {
        serde_json::json!({
            "global": [{"dettid": 3, "op": "Branch", "count": 4}],
            "per_thread": {
                "3": {
                    "final_prio": 2195,
                    "prio_changes": [[1792847361183390955u64, 2195]],
                    "preemption_rcbs": [1253548],
                    "chaos_epochs": []
                }
            },
            "callback_checkpoints": {
                "3": [{
                    "history_index": 0,
                    "origin": {
                        "callback": {"Syscall": 1},
                        "site": "Posthook",
                        "progress": 1256742,
                        "last_op": {"Syscall": [1, "Posthook"]},
                        "time": 1792847361183390955u64,
                        "rcbs": 1253548
                    }
                }]
            }
        })
    }

    /// The priority-history fixed point passes only on exact agreement of
    /// every per-thread history and every checkpoint: one changed field, a
    /// missing checkpoint map, a removed checkpoint and an added row each
    /// fail, named by path, while the global events are left to
    /// `verify_schedules`.
    #[test]
    fn priority_histories_must_agree_exactly() {
        let left = priority_schedule();
        assert!(Verify::<PathBuf>::priority_history_differences(&left, &left).is_empty());
        assert_eq!(Verify::<PathBuf>::priority_history_extent(&left), (1, 1, 1));
        assert_eq!(
            Verify::<PathBuf>::priority_history_extent(&serde_json::json!({"global": []})),
            (0, 0, 0)
        );
        let mut global_only = left.clone();
        global_only["global"] = serde_json::json!([]);
        assert!(
            Verify::<PathBuf>::priority_history_differences(&left, &global_only).is_empty(),
            "global events are verify_schedules' to compare"
        );

        let mut site = left.clone();
        site["callback_checkpoints"]["3"][0]["origin"]["site"] = serde_json::json!("Prehook");
        assert_eq!(
            Verify::<PathBuf>::priority_history_differences(&left, &site),
            vec!["callback_checkpoints.3[0].origin.site: \"Posthook\" vs \"Prehook\"".to_owned()]
        );

        let mut absent = left.clone();
        absent
            .as_object_mut()
            .unwrap()
            .remove("callback_checkpoints");
        assert_eq!(
            Verify::<PathBuf>::priority_history_differences(&left, &absent),
            vec!["callback_checkpoints: in the first run only".to_owned()]
        );

        let mut removed = left.clone();
        removed["callback_checkpoints"]["3"] = serde_json::json!([]);
        assert_eq!(
            Verify::<PathBuf>::priority_history_differences(&left, &removed),
            vec!["callback_checkpoints.3: 1 entries vs 0".to_owned()]
        );

        let mut added = left.clone();
        added["per_thread"]["3"]["preemption_rcbs"] = serde_json::json!([1253548, 2000000]);
        assert_eq!(
            Verify::<PathBuf>::priority_history_differences(&left, &added),
            vec!["per_thread.3.preemption_rcbs: 1 entries vs 2".to_owned()]
        );

        let mut thread = left.clone();
        thread["per_thread"]["5"] = left["per_thread"]["3"].clone();
        assert_eq!(
            Verify::<PathBuf>::priority_history_differences(&left, &thread),
            vec!["per_thread.5: in the second run only".to_owned()]
        );
    }

    #[test]
    fn test_build_command_args_ignore_lines_provided() -> anyhow::Result<()> {
        let env = TemporaryEnvironmentBuilder::new().run_count(2).build()?;
        let verify = Verify::new(PathBuf::from("hermit"));
        let args = verify.build_command_args(
            &env.runs()[0],
            &env.runs()[1],
            LogDiffOptions {
                ignore_lines: vec![String::from("test")],
                ..Default::default()
            },
            false,
        );

        assert_eq!(args.into_iter().any(|x| x == "--ignore-lines=test"), true);
        Ok(())
    }

    #[test]
    fn test_build_command_args_ignore_lines_not_provided() -> anyhow::Result<()> {
        let env = TemporaryEnvironmentBuilder::new().run_count(2).build()?;
        let verify = Verify::new(PathBuf::from("hermit"));
        let args = verify.build_command_args(
            &env.runs()[0],
            &env.runs()[1],
            LogDiffOptions {
                ignore_lines: Vec::new(),
                ..Default::default()
            },
            false,
        );

        assert_eq!(
            args.into_iter().any(|x| x.contains("--ignore-lines")),
            false
        );
        Ok(())
    }

    /// hermit-verify must NOT name a record envelope on the `log-diff` it
    /// spawns. `hermit_bin` defaults to `HERMIT_BIN` or the PATH, so the child
    /// can be a published bundle built without the flag, where passing it is
    /// `error: unexpected argument` and exit 2. The all-records envelope this
    /// callsite wants is that command's default, so omitting it is both
    /// correct and compatible.
    #[test]
    fn log_diff_command_does_not_pin_a_record_envelope_the_child_may_lack() -> anyhow::Result<()> {
        let env = TemporaryEnvironmentBuilder::new().run_count(2).build()?;
        let verify = Verify::new(PathBuf::from("hermit"));
        for canonicalize_host_addresses in [false, true] {
            let args = verify.build_command_args(
                &env.runs()[0],
                &env.runs()[1],
                LogDiffOptions::default(),
                canonicalize_host_addresses,
            );
            assert!(
                !args.iter().any(|arg| arg.starts_with("--record-envelope")),
                "hermit-verify must not pass --record-envelope to a hermit it did not build: \
                 {args:?}"
            );
        }
        Ok(())
    }

    /// <https://github.com/rrnewton/hermit/issues/3412>: a child that offers the
    /// flag is asked to canonicalize marked host addresses; one that does not
    /// is never passed an argument it would reject.
    #[test]
    fn log_diff_canonicalizes_host_addresses_only_when_the_child_offers_it() -> anyhow::Result<()> {
        let env = TemporaryEnvironmentBuilder::new().run_count(2).build()?;
        let verify = Verify::new(PathBuf::from("hermit"));
        let args = |offered| {
            verify.build_command_args(
                &env.runs()[0],
                &env.runs()[1],
                LogDiffOptions::default(),
                offered,
            )
        };
        let flag = "--canonicalize-host-addresses".to_owned();
        assert!(args(true).contains(&flag));
        assert!(!args(false).contains(&flag));

        // The probe itself: a child whose log-diff help lacks the flag, and a
        // child that cannot run at all, both read as "not offered".
        let directory = tempfile::tempdir()?;
        let old = directory.path().join("old-hermit");
        std::fs::write(&old, "#!/bin/sh\necho 'Usage: hermit log-diff [OPTIONS]'\n")?;
        let new = directory.path().join("new-hermit");
        std::fs::write(
            &new,
            "#!/bin/sh\necho '      --canonicalize-host-addresses'\n",
        )?;
        for script in [&old, &new] {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(script, std::fs::Permissions::from_mode(0o755))?;
        }
        assert!(!Verify::new(&old).child_offers_host_address_canonicalization());
        assert!(Verify::new(&new).child_offers_host_address_canonicalization());
        assert!(
            !Verify::new(directory.path().join("absent"))
                .child_offers_host_address_canonicalization()
        );
        Ok(())
    }

    #[test]
    fn test_compare_equal_files() -> anyhow::Result<()> {
        let env = TemporaryEnvironmentBuilder::new().run_count(2).build()?;
        write_to_file(&env.runs()[0].std_out_file_path, "test1")?;
        write_to_file(&env.runs()[1].std_out_file_path, "test1")?;

        let verify = Verify::new(PathBuf::from("hermit"));

        let files_equal = verify.verify_stdout(&env.runs()[0], &env.runs()[1])?;

        assert_eq!(files_equal, true);

        Ok(())
    }

    #[test]
    fn test_compare_not_equal_files() -> anyhow::Result<()> {
        let env = TemporaryEnvironmentBuilder::new().run_count(2).build()?;
        write_to_file(&env.runs()[0].std_out_file_path, "test1")?;
        write_to_file(&env.runs()[1].std_out_file_path, "test2")?;

        let verify = Verify::new(PathBuf::from("hermit"));
        let files_equal = verify.verify_stdout(&env.runs()[0], &env.runs()[1])?;
        assert_eq!(files_equal, false);

        Ok(())
    }

    #[test]
    fn test_compare_desync() -> anyhow::Result<()> {
        let env = TemporaryEnvironmentBuilder::new().run_count(3).build()?;

        write_to_file(&env.runs()[0].log_file_path, "DESYNC Hello")?;
        write_to_file(&env.runs()[1].log_file_path, "Test 1")?;
        write_to_file(&env.runs()[2].log_file_path, "Test 1: TIME-DESYNC")?;

        let verify = Verify::new(PathBuf::from("hermit"));
        let logs_equal = [
            verify.verify_desync(&env.runs()[0])?,
            verify.verify_desync(&env.runs()[1])?,
            verify.verify_desync(&env.runs()[2])?,
        ];
        assert_eq!(logs_equal, [false, true, false]);

        Ok(())
    }

    #[test]
    fn test_compare_schedules() -> anyhow::Result<()> {
        let env = TemporaryEnvironmentBuilder::new().run_count(4).build()?;
        let verify = Verify::new("hermit");
        write_to_file(
            &env.runs()[0].schedule_file,
            r#"{ "global" : { "name" : "name1"} }"#,
        )?;
        write_to_file(
            &env.runs()[1].schedule_file,
            r#"{ "global" : { "name" : "name2"} }"#,
        )?;
        write_to_file(
            &env.runs()[2].schedule_file,
            r#"{ "global" : { "name" : "name123"} }"#,
        )?;
        write_to_file(
            &env.runs()[3].schedule_file,
            r#"{ "some": "value", "global" : { "name" : "name123"} }"#,
        )?;

        let result = [
            verify.verify_schedules(&env.runs()[0], &env.runs()[1])?,
            verify.verify_schedules(&env.runs()[2], &env.runs()[3])?,
        ];
        assert_eq!(result, [false, true]);
        Ok(())
    }

    fn write_to_file<P: AsRef<Path>>(file_path: &P, content: &str) -> anyhow::Result<()> {
        let mut file = File::create(file_path.as_ref())?;
        write!(file, "{}", content)?;
        Ok(())
    }
}
