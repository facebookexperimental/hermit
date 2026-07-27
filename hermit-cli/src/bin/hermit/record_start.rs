/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::fs;
use std::num::NonZeroU64;
use std::path::Path;
use std::path::PathBuf;
use std::ptr;
use std::sync::atomic::AtomicPtr;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;

use clap::Args;
use colored::Colorize;
use hermit::Backend;
use hermit::Context;
use hermit::Error;
use hermit::HermitData;
use hermit::SerializableError;
use hermit::Shebang;
use nix::sys::signal::SaFlags;
use nix::sys::signal::SigAction;
use nix::sys::signal::SigHandler;
use nix::sys::signal::SigSet;
use nix::sys::signal::Signal;
use nix::sys::signal::sigaction;
use reverie::process::Command;
use reverie::process::Container;
use reverie::process::ExitStatus;
use reverie::process::Mount;
use reverie::process::MountFlags;

use super::container::IdentityGuard;
use super::container::deterministic_container;
use super::global_opts::GlobalOpts;
use super::run::is_elf_file;
use super::run::path_resolution_visits_prefix;
use super::verify::ComparedRun;
use super::verify::ComparisonOptions;
use super::verify::compare_two_runs;
use super::verify::setup_double_run;

#[derive(Debug)]
struct E9patchRecordOverlay {
    source: PathBuf,
    target: PathBuf,
}

static TIMEOUT_MESSAGE: AtomicPtr<u8> = AtomicPtr::new(ptr::null_mut());
static TIMEOUT_MESSAGE_LEN: AtomicUsize = AtomicUsize::new(0);

extern "C" fn recording_timeout_handler(_signal: libc::c_int) {
    let len = TIMEOUT_MESSAGE_LEN.load(Ordering::Acquire);
    let message = TIMEOUT_MESSAGE.load(Ordering::Acquire);
    if !message.is_null() && len != 0 {
        // SAFETY: `message` is leaked before the timer is armed, and fcntl(2),
        // write(2), and _exit(2) are all async-signal-safe.
        unsafe {
            // Make stderr non-blocking before writing. If stderr is a pipe or
            // socket whose buffer is full, a blocking write(2) would wedge the
            // handler and never reach _exit, defeating the deadline. A dropped
            // diagnostic is acceptable; a hung timeout is not.
            let flags = libc::fcntl(libc::STDERR_FILENO, libc::F_GETFL);
            if flags != -1 {
                libc::fcntl(libc::STDERR_FILENO, libc::F_SETFL, flags | libc::O_NONBLOCK);
            }
            libc::write(libc::STDERR_FILENO, message.cast(), len);
        }
    }

    // Exiting PID 1 tears down the isolated recording namespace and its tracees.
    // SAFETY: _exit(2) is async-signal-safe and does not run Rust destructors.
    unsafe { libc::_exit(124) }
}

struct RecordingDeadline {
    previous_handler: SigAction,
    // Whether SIGALRM was blocked in the inherited mask and must be re-blocked
    // when the deadline is disarmed.
    reblock_sigalrm: bool,
}

fn sigalrm_set() -> SigSet {
    let mut set = SigSet::empty();
    set.add(Signal::SIGALRM);
    set
}

impl RecordingDeadline {
    fn arm(timeout: Duration) -> Result<Self, Error> {
        let seconds: libc::c_uint = timeout
            .as_secs()
            .try_into()
            .map_err(|_| Error::msg("record timeout exceeds the platform alarm limit"))?;
        let message = Box::leak(
            format!(
                "Error: Recording timed out after {} seconds; the recording container was terminated\n",
                timeout.as_secs()
            )
            .into_boxed_str(),
        );
        TIMEOUT_MESSAGE.store(message.as_mut_ptr(), Ordering::Release);
        TIMEOUT_MESSAGE_LEN.store(message.len(), Ordering::Release);

        let action = SigAction::new(
            SigHandler::Handler(recording_timeout_handler),
            SaFlags::SA_RESETHAND,
            SigSet::empty(),
        );
        // SAFETY: the handler uses only async-signal-safe operations and remains
        // installed until this guard disarms the alarm.
        let previous_handler = unsafe { sigaction(Signal::SIGALRM, &action) }?;

        // A signal mask inherited from our parent may have SIGALRM blocked. If
        // it is, the alarm signal stays perpetually pending and the handler
        // never runs, silently disabling the deadline. Unblock SIGALRM so the
        // alarm is deliverable, and remember to restore the original state.
        let reblock_sigalrm = SigSet::thread_get_mask()
            .map(|mask| mask.contains(Signal::SIGALRM))
            .unwrap_or(false);
        if reblock_sigalrm {
            let _ = sigalrm_set().thread_unblock();
        }

        // SAFETY: the timeout is nonzero and fits c_uint.
        unsafe { libc::alarm(seconds) };

        Ok(Self {
            previous_handler,
            reblock_sigalrm,
        })
    }
}

impl Drop for RecordingDeadline {
    fn drop(&mut self) {
        // SAFETY: disarm the process-local alarm before restoring its handler.
        unsafe {
            libc::alarm(0);
            let _ = sigaction(Signal::SIGALRM, &self.previous_handler);
        }
        // Restore SIGALRM to its inherited blocked state without disturbing the
        // rest of the mask.
        if self.reblock_sigalrm {
            let _ = sigalrm_set().thread_block();
        }
        TIMEOUT_MESSAGE_LEN.store(0, Ordering::Release);
        TIMEOUT_MESSAGE.store(ptr::null_mut(), Ordering::Release);
    }
}

fn with_recording_deadline<T>(
    timeout: Duration,
    record: impl FnOnce() -> Result<T, Error>,
) -> Result<T, Error> {
    let _deadline = RecordingDeadline::arm(timeout)?;
    record()
}

#[derive(Debug, Args)]
pub struct StartOpts {
    /// Program to run.
    #[clap(value_name = "PROGRAM", required = true)]
    program: Option<PathBuf>,

    /// Enable strict deterministic recording. Recording is already strict; this flag is retained
    /// for command-line compatibility with `hermit run --strict`.
    #[clap(long = "strict")]
    _strict: bool,

    /// Arguments for the program.
    #[clap(value_name = "ARGS")]
    args: Vec<String>,

    /// Directory where recorded syscall data is stored.
    #[clap(long, value_name = "DIR", env = "HERMIT_DATA_DIR")]
    data_dir: Option<PathBuf>,

    /// Kill the recording if the guest does not finish within this many seconds.
    #[clap(long, value_name = "SECONDS")]
    record_timeout: Option<NonZeroU64>,

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
    #[clap(long = "verify-with-gdbex", value_delimiter = ';')]
    gdbex: Vec<String>,
}

impl StartOpts {
    fn program(&self) -> &PathBuf {
        self.program
            .as_ref()
            .expect("Clap requires PROGRAM unless a record management subcommand is selected")
    }

    fn record_timeout(&self) -> Option<Duration> {
        self.record_timeout
            .map(|seconds| Duration::from_secs(seconds.get()))
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-696): Review /proc magic-link rejection for record overlays.
    fn resolve_e9patch_record_target(&self) -> Result<PathBuf, Error> {
        let command = Command::new(self.program());
        let resolved = command.find_program().with_context(|| {
            format!(
                "Could not resolve program {:?} in PATH for e9patch preprocessing",
                self.program()
            )
        })?;
        if path_resolution_visits_prefix(&resolved, Path::new("/proc"))? {
            anyhow::bail!(
                "e9patch cannot safely overlay executable {} because its path resolves through \
                 /proc; use the executable's stable filesystem path",
                self.program().display()
            );
        }
        fs::canonicalize(&resolved).with_context(|| {
            format!(
                "failed to resolve e9patch executable {}",
                resolved.display()
            )
        })
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-696): Review e9patch record preparation and executable overlaying.
    fn prepare_e9patch_overlay(
        &self,
        global: &GlobalOpts,
    ) -> Result<Option<E9patchRecordOverlay>, Error> {
        if global.backend != Some(Backend::E9patch) {
            return Ok(None);
        }

        Backend::Ptrace.ensure_available()?;
        let target = self.resolve_e9patch_record_target()?;
        if !is_elf_file(&target)? {
            eprintln!(
                ":: Backend: e9patch preprocessing + ptrace record runtime; mapped_sites=0; \
                 main_executable=non-ELF; preprocessing=not-applicable"
            );
            return Ok(None);
        }
        if let Some(reason) = hermit::e9patch::unavailable_reason() {
            anyhow::bail!("backend `e9patch` is unavailable: {reason}");
        }

        let prepared = hermit::e9patch::prepare(&target)?;
        let rewrite_cache = if prepared.patched_sites == 0 {
            "not-applicable"
        } else if prepared.rewrite_cache_hit {
            "hit"
        } else {
            "miss"
        };
        eprintln!(
            ":: Backend: e9patch preprocessing + ptrace record runtime; candidate_sites={}; \
             mapped_sites={}; b0_sites={}; instruction_map_cache={:?}; rewrite_cache={}; \
             artifact_sha256={}",
            prepared.candidate_sites,
            prepared.patched_sites,
            prepared.b0_sites,
            prepared.instruction_map_cache_status,
            rewrite_cache,
            prepared.artifact_sha256.as_deref().unwrap_or("none"),
        );

        Ok(
            (prepared.patched_sites != 0).then_some(E9patchRecordOverlay {
                source: prepared.binary,
                target,
            }),
        )
    }

    fn recording_container(
        &self,
        global: &GlobalOpts,
    ) -> Result<(Container, IdentityGuard), Error> {
        let overlay = self.prepare_e9patch_overlay(global)?;
        let (mut container, identity_guard) = deterministic_container()?;
        if let Some(overlay) = overlay {
            container.mount(Mount::bind(&overlay.source, &overlay.target).readonly());
            container.mount(
                Mount::new(overlay.target)
                    .flags(MountFlags::MS_BIND | MountFlags::MS_REMOUNT | MountFlags::MS_RDONLY),
            );
        }
        Ok((container, identity_guard))
    }

    pub fn main(&self, global: &GlobalOpts) -> Result<ExitStatus, Error> {
        if self.verify {
            self.record_verify(global)
        } else if !self.gdbex.is_empty() {
            self.record_verify_debug(global)
        } else {
            let hermit = HermitData::from(self.data_dir.as_ref());
            let record_timeout = self.record_timeout();

            let (mut container, _identity_guard) = self.recording_container(global)?;

            let recording = match record_timeout {
                Some(timeout) => {
                    let data = hermit.create_recording_dir()?;
                    let data_path = data.path().to_path_buf();
                    let exit_status = container
                        .run(|| {
                            let _guard = global.init_tracing();
                            let mut command = Command::new(self.program());
                            command.args(&self.args);
                            with_recording_deadline(timeout, || {
                                hermit::record_to(command, &data_path)
                            })
                            .map_err(SerializableError::from)
                        })
                        .context("Container exited unexpectedly")??;
                    hermit.commit_recording(data, exit_status)?
                }
                None => container
                    .run(|| {
                        let _guard = global.init_tracing();
                        let mut command = Command::new(self.program());
                        command.args(&self.args);
                        hermit.record(command).map_err(SerializableError::from)
                    })
                    .context("Container exited unexpectedly")??,
            };

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

        let (mut recording_container, _record_identity_guard) = self.recording_container(global)?;

        eprintln!(":: {}", "Recording...".yellow().bold());

        let temp_data_dir = tempfile::tempdir()?;
        let data_dir = temp_data_dir.path();
        let record_timeout = self.record_timeout();

        let recording = recording_container
            .run(|| {
                let _guard = global1.init_tracing();

                let mut command = Command::new(self.program());
                command.args(&self.args);

                match record_timeout {
                    Some(timeout) => with_recording_deadline(timeout, || {
                        hermit::record_with_output(command, data_dir)
                    }),
                    None => hermit::record_with_output(command, data_dir),
                }
                .map_err(SerializableError::from)
            })
            .context("Container exited unexpectedly")??;

        eprintln!(":: {}", "Replaying...".yellow().bold());

        // Replay the recording.
        let (mut replay_container, _replay_identity_guard) = deterministic_container()?;
        let replay = replay_container
            .run(|| {
                let _guard = global2.init_tracing();
                hermit::replay_with_output(data_dir).map_err(SerializableError::from)
            })
            .context("Container exited unexpectedly")??;

        compare_two_runs(
            ComparedRun {
                output: &recording,
                log: log1.into_temp_path(),
            },
            ComparedRun {
                output: &replay,
                log: log2.into_temp_path(),
            },
            ComparisonOptions {
                success_message: "Success: replay matched recording.",
                failure_message: "Recording output did not match replay output!",
                verbose: false,
                compare_logs: true,
            },
        )
    }
    /// This is called when `--verify-with-gdbex` is passed to the command line.
    fn record_verify_debug(&self, global: &GlobalOpts) -> Result<ExitStatus, Error> {
        let (mut container, _identity_guard) = self.recording_container(global)?;

        eprintln!(":: {}", "Recording...".yellow().bold());

        let temp_data_dir = tempfile::tempdir()?;
        let data_dir = temp_data_dir.path();
        let record_timeout = self.record_timeout();

        let _result = container
            .run(|| {
                let _guard = global.init_tracing();

                let mut command = Command::new(self.program());
                command.args(&self.args);

                match record_timeout {
                    Some(timeout) => {
                        with_recording_deadline(timeout, || hermit::record_to(command, data_dir))
                    }
                    None => hermit::record_to(command, data_dir),
                }
                .map_err(SerializableError::from)
            })
            .context("Container exited unexpectedly")??;

        eprintln!(":: {}", "Replaying...".yellow().bold());

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
        let (mut container, _identity_guard) = deterministic_container()?;
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

#[cfg(test)]
mod tests {
    use super::*;

    // A blocked SIGALRM (e.g. inherited from the parent) would leave the alarm
    // perpetually pending and silently disable the deadline. Arming must unblock
    // SIGALRM, and dropping must restore the prior blocked state without
    // spuriously blocking a signal that started unblocked. A long timeout keeps
    // the process-wide alarm from firing during the test; the guard's Drop
    // cancels it.
    #[test]
    fn recording_deadline_manages_sigalrm_mask() {
        let sigalrm = sigalrm_set();

        // Case 1: SIGALRM starts blocked. Arm unblocks it; drop re-blocks it.
        sigalrm.thread_block().unwrap();
        assert!(SigSet::thread_get_mask().unwrap().contains(Signal::SIGALRM));
        {
            let _deadline = RecordingDeadline::arm(Duration::from_secs(3600)).unwrap();
            assert!(
                !SigSet::thread_get_mask().unwrap().contains(Signal::SIGALRM),
                "arming the deadline must unblock SIGALRM so the alarm is deliverable"
            );
        }
        assert!(
            SigSet::thread_get_mask().unwrap().contains(Signal::SIGALRM),
            "dropping the deadline must restore the inherited blocked state"
        );

        // Case 2: SIGALRM starts unblocked. Arm leaves it unblocked; drop must
        // not spuriously block it.
        sigalrm.thread_unblock().unwrap();
        assert!(!SigSet::thread_get_mask().unwrap().contains(Signal::SIGALRM));
        {
            let _deadline = RecordingDeadline::arm(Duration::from_secs(3600)).unwrap();
            assert!(!SigSet::thread_get_mask().unwrap().contains(Signal::SIGALRM));
        }
        assert!(
            !SigSet::thread_get_mask().unwrap().contains(Signal::SIGALRM),
            "dropping must not block SIGALRM when it started unblocked"
        );
    }

    #[test]
    fn e9patch_record_target_rejects_proc_magic_links() {
        let options = StartOpts {
            program: Some(PathBuf::from("/proc/self/exe")),
            _strict: false,
            args: Vec::new(),
            data_dir: None,
            record_timeout: None,
            verify: false,
            gdbex: Vec::new(),
        };
        let error = options.resolve_e9patch_record_target().unwrap_err();
        assert!(
            error.to_string().contains("resolves through /proc"),
            "unexpected error: {error}"
        );
    }
}
