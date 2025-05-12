/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::path::PathBuf;

use clap::Parser;
use colored::Colorize;
use hermit::Context;
use hermit::Error;
use hermit::HermitData;
use hermit::SerializableError;
use hermit::Shebang;
use reverie::process::Command;
use reverie::process::ExitStatus;

use super::container::default_container;
use super::global_opts::GlobalOpts;
use super::verify::compare_two_runs;
use super::verify::setup_double_run;

#[derive(Debug, Parser)]
pub struct StartOpts {
    /// Program to run.
    #[clap(value_name = "PROGRAM")]
    program: PathBuf,

    /// Arguments for the program.
    #[clap(value_name = "ARGS")]
    args: Vec<String>,

    /// Directory where recorded syscall data is stored.
    #[clap(long, value_name = "DIR", env = "HERMIT_DATA_DIR")]
    data_dir: Option<PathBuf>,

    /// After recording, immediately replays the command to verify that it works.
    /// This is useful for testing purposes where we often want to verify that
    /// recording was successful.
    ///
    /// The recording is deleted if the replay was successful.
    #[clap(long)]
    verify: bool,

    /// After recording, immediately replays the command to verify that it works
    /// With provided gdb command (passed by `-ex`).
    /// This is useful for testing purposes where we often want to verify that
    /// recording was successful with gdbserver enabled.
    ///
    /// The recording is deleted if the replay was successful.
    #[clap(
        long = "verify-with-gdbex",
        value_delimiter = ';',
        use_delimiter = true
    )]
    gdbex: Vec<String>,
}
impl StartOpts {
    pub fn main(&self, global: &GlobalOpts) -> Result<ExitStatus, Error> {
        if self.verify {
            self.record_verify(global)
        } else if !self.gdbex.is_empty() {
            self.record_verify_debug(global)
        } else {
            let hermit = HermitData::from(self.data_dir.as_ref());

            let mut container = default_container(true);

            let recording = container
                .run(|| {
                    let _guard = global.init_tracing();

                    let mut command = Command::new(&self.program);
                    command.args(&self.args);

                    hermit.record(command).map_err(SerializableError::from)
                })
                .context("Container exited unexpectedly")??;

            eprintln!(
                "\n{message}:\n\n    {command} {id}\n",
                message = "RECORDING COMPLETE! To replay, run".yellow().bold(),
                command = "hermit replay".blue().bold(),
                id = recording.id.to_string().bold()
            );

            Ok(recording.exit_status)
        }
    }

    /// This is called when `--verify` is passed to the command line.
    fn record_verify(&self, global: &GlobalOpts) -> Result<ExitStatus, Error> {
        let ((global1, log1), (global2, log2)) = setup_double_run(global, "record", "replay");

        let mut container = default_container(true);

        eprintln!(":: {}", "Recording...".yellow().bold());

        let temp_data_dir = tempfile::tempdir()?;
        let data_dir = temp_data_dir.path();

        let recording = container
            .run(|| {
                let _guard = global1.init_tracing();

                let mut command = Command::new(&self.program);
                command.args(&self.args);

                hermit::record_with_output(command, data_dir).map_err(SerializableError::from)
            })
            .context("Container exited unexpectedly")??;

        eprintln!(":: {}", "Replaying...".yellow().bold());

        // Set this var to make sure we detect desynchronization errors upon
        // replay.
        //
        // FIXME: This is a little hacky. This should be configured via a config
        // value instead. That's not done because there is no nesting of global
        // state yet.
        // TODO: Audit that the environment access only happens in single-threaded code.
        unsafe { std::env::set_var("HERMIT_VERIFY", "1") };

        // Replay the recording.
        let replay = container
            .run(|| {
                let _guard = global2.init_tracing();
                hermit::replay_with_output(data_dir).map_err(SerializableError::from)
            })
            .context("Container exited unexpectedly")??;

        compare_two_runs(
            &recording,
            log1.into_temp_path(),
            &replay,
            log2.into_temp_path(),
            "Success: replay matched recording.",
            "Recording output did not match replay output!",
        )
    }
    /// This is called when `--verify-with-gdbex` is passed to the command line.
    fn record_verify_debug(&self, global: &GlobalOpts) -> Result<ExitStatus, Error> {
        let mut container = default_container(true);

        eprintln!(":: {}", "Recording...".yellow().bold());

        let temp_data_dir = tempfile::tempdir()?;
        let data_dir = temp_data_dir.path();

        let _result = container
            .run(|| {
                let _guard = global.init_tracing();

                let mut command = Command::new(&self.program);
                command.args(&self.args);

                hermit::record_to(command, data_dir).map_err(SerializableError::from)
            })
            .context("Container exited unexpectedly")??;

        eprintln!(":: {}", "Replaying...".yellow().bold());

        // Set this var to make sure we detect desynchronization errors upon
        // replay.
        //
        // FIXME: This is a little hacky. This should be configured via a config
        // value instead. That's not done because there is no nesting of global
        // state yet.
        // TODO: Audit that the environment access only happens in single-threaded code.
        unsafe { std::env::set_var("HERMIT_VERIFY", "1") };

        // Find the path to the executable so that GDB can use it to resolve
        // symbols.
        let exe = data_dir.join("exe");
        let real_exe = Shebang::new(&exe).map_or(exe, |s| s.interpreter().into());

        // Not using fixed port (such as 1234) here because this is mainly
        // intended for tests, which could be running in parallel. This could
        // be flakey when port is already in use.
        let gdbserver_port = 16384 + nix::unistd::gettid().as_raw() as u16 % 1024;

        // Run the gdb client outside of the PID namespace. This cannot be done
        // inside of the PID namespace because it would perturb the
        // deterministic PID allocation that is needed for the replay.
        let mut gdb_command = std::process::Command::new("gdb");
        gdb_command
            .arg(real_exe)
            .arg("-quiet")
            .arg("-iex")
            // don't prompt (dialog) when breakpoint symbol doesn't exist.
            .arg("set breakpoint pending on")
            .arg("-ex")
            .arg(format!("target remote :{}", gdbserver_port));
        for ex in &self.gdbex {
            gdb_command.arg("-ex").arg(ex);
        }
        // Make sure gdb always exit.
        gdb_command.arg("-batch");
        gdb_command.arg("--return-child-result");
        let mut gdb_client = gdb_command
            .spawn()
            .context("Failed to run gdb command. Please make sure it is in your $PATH.")?;

        // TODO: For replay, we ought to construct the container from
        // `metadata.json`. That logic belongs in `hermit::replay`, but we have
        // to initialize logging inside the container because it may spawn a
        // thread. If we can guarantee that tracing won't spawn a thread, then
        // that restriction be lifted.
        let mut container = default_container(true);
        let result = container
            .run(|| {
                let _guard = global.init_tracing();
                hermit::replay_with_gdbserver(data_dir, gdbserver_port)
                    .map_err(SerializableError::from)
            })
            .context("Container exited unexpectedly")??;
        let _ = gdb_client.wait();
        Ok(result)
    }
}
