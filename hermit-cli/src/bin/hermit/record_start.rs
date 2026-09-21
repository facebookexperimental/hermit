/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#[cfg(test)]
use std::ffi::OsStr;
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

use super::container::Classified;
use super::container::IdentityGuard;
use super::container::RunGuarded;
use super::container::default_container;
use super::container::identity_hardening_mounts;
use super::gdb_client::CLIENT_EXITED_BEFORE_CONNECTING;
use super::gdb_client::GdbClientWatch;
use super::global_opts::GlobalOpts;
use super::record_envelope::RecordEnvelope;
use super::run::BaseEnv;
use super::run::apply_base_environment;
use super::run::is_elf_file;
use super::run::parse_assignment;
use super::run::path_resolution_visits_prefix;
use super::verify::ComparedRun;
use super::verify::ComparisonOptions;
use super::verify::LogCompareStrictness;
use super::verify::announce_verification_outcome;
use super::verify::compare_two_runs;
use super::verify::setup_double_run;
use super::verify::validate_log_level;
use super::verify::write_pending_verification_json;
use super::verify::write_verification_json;

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

    /// Accepted and ignored, for command-line compatibility with `hermit run --strict`.
    /// Recording does NOT run under `run --strict`'s configuration: see
    /// `hermit_cli::metadata::record_or_replay_config`, which deliberately sets
    /// `virtualize_time: false`, `deterministic_io: false`, `passthru_opt: true` and
    /// `panic_on_unsupported_syscalls: false`. A recorded guest therefore reads the REAL
    /// host clock, and two independent recordings of the same program observe different
    /// times. What `--verify` establishes is that a recording REPLAYS faithfully, not that
    /// two independent recordings agree.
    #[clap(long = "strict")]
    _strict: bool,

    /// Arguments for the program.
    #[clap(value_name = "ARGS")]
    args: Vec<String>,

    /// Additionally append one or more environment variables to the recorded
    /// guest environment. If a name is provided without a value, pass that
    /// variable through from the host.
    #[clap(short = 'e', long, value_parser = parse_assignment, value_name = "name[=val]")]
    env: Vec<(String, Option<String>)>,

    /// The base environment presented to the recorded guest. This uses the
    /// same contract as `hermit run`; record and replay must not diverge merely
    /// because the manifest selected a different front door.
    #[clap(long, default_value = "host", value_name = "str")]
    base_env: BaseEnv,

    /// Mount a file, directory, or fresh filesystem for recording and an immediate verify replay.
    #[clap(long)]
    mount: Vec<Mount>,

    /// Set the working directory for both recording and replay after mounts apply.
    #[clap(long, value_name = "path")]
    workdir: Option<String>,

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

    /// With --verify, write the verification verdict as a single JSON line to
    /// this path: `{"verified":bool,"bitwise_parity":bool,
    /// "verdict":"matched"|"diverged","comparison":{"strictness":
    /// "stripped"|"canonical","display_name":str,"compare_logs":bool,
    /// "compare_io_buffers":bool,
    /// "log_scope":
    /// "deterministic"|"info"|"full_trace","record_envelope":
    /// "all_records_v1","strip_lines":bool,
    /// "canonicalize_addresses":bool,"full_trace":bool,"exact_remainder":bool,
    /// "stripped_prefixes":[str],"canonicalizations":[str],"ignore_lines":bool,
    /// "skip_commit":bool,"skip_detlog":bool},"dbt_counted_branches":
    /// {"left":int,"right":int},"guest_exit_code":int|null,
    /// "guest_signal":int|null,"first_divergent_scheduler_turn":int|null,
    /// "first_divergent_virtual_nanoseconds":int|null,
    /// "first_divergent_record":int|null,"first_divergent_syscall":int|null}`.
    /// `dbt_counted_branches` is present only when DBT completed a typed
    /// whole-process comparison; it is omitted for other backends and no-result.
    /// ALL FOUR divergence coordinates are emitted, and they are four
    /// DIFFERENT KEYSPACES that must never be compared across axes: one real
    /// divergence was record 7495, syscall 1074, scheduler turn 196 -- three
    /// numbers for one event. A consumer that reads a subset silently drops
    /// located evidence. This is the
    /// exit-code-independent verdict
    /// channel: `verified` reflects whether the record and replay runs matched,
    /// regardless of what the guest exited with, so a caller need not (and must
    /// not) infer the verdict from the process exit code. A record/replay parity
    /// ratchet must key on `bitwise_parity`, NOT `verified`: `bitwise_parity` is
    /// true only under the `canonical` (`BitwiseInfoV1`) policy — a full-INFO
    /// comparison inside a named canonical record envelope that strips only the
    /// real wall-clock prefix, canonicalizes host addresses to first-appearance
    /// ordinals, includes syscall output-buffer hashes, and compares everything
    /// else exactly (see --verify-strict) — rather than a stripped, content-blind,
    /// or opaque filtered match.
    #[clap(long, requires = "verify", value_name = "PATH")]
    verify_json: Option<PathBuf>,

    /// With --verify, compare the record and replay logs under the CANONICAL
    /// parity policy: strip only the real wall-clock timestamp prefix, canonicalize
    /// host memory addresses to first-appearance ordinals (tolerating an ASLR
    /// shift while still diverging on allocation-order or aliasing changes), and
    /// compare every INFO message's remaining bytes — virtual-time timestamps,
    /// raw syscall argument/result values, counts, sizes, flags — exactly. An
    /// explicit DEBUG/TRACE level remains captured for diagnostics but does not
    /// change the INFO verdict. Without this the
    /// default `--verify` normalizes away numbers, addresses, tmp paths, and
    /// timestamps before comparing, so a "verified" result asserts only stripped
    /// parity, not bitwise identity. A record/replay determinism ratchet keying on
    /// the verdict should set this so it cannot be silently weakened to a stripped
    /// comparison.
    #[clap(long, requires = "verify")]
    verify_strict: bool,

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

    fn guest_command(&self) -> Result<Command, Error> {
        let mut command = Command::new(self.program());
        command.args(&self.args);
        if let Some(workdir) = &self.workdir {
            command.current_dir(workdir);
        }
        apply_base_environment(&mut command, &self.base_env, &self.env)?;
        Ok(command)
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

    fn configured_container(&self) -> Result<(Container, IdentityGuard), Error> {
        let mut container = default_container(true);
        let (mut identity_mounts, mut identity_guard) = identity_hardening_mounts()?;
        for mount in &self.mount {
            let user_target = mount.get_target();
            identity_guard.discard_mounts_shadowed_by(&mut identity_mounts, user_target);
        }
        container.mounts(identity_mounts);
        container.mounts(self.mount.clone());
        if let Some(workdir) = &self.workdir {
            container.current_dir(workdir);
        }
        Ok((container, identity_guard))
    }

    fn recording_container(
        &self,
        global: &GlobalOpts,
    ) -> Result<(Container, IdentityGuard), Error> {
        let overlay = self.prepare_e9patch_overlay(global)?;
        let (mut container, identity_guard) = self.configured_container()?;
        if let Some(overlay) = overlay {
            container.mount(Mount::bind(&overlay.source, &overlay.target).readonly());
            container.mount(
                Mount::new(overlay.target)
                    .flags(MountFlags::MS_BIND | MountFlags::MS_REMOUNT | MountFlags::MS_RDONLY),
            );
        }
        Ok((container, identity_guard))
    }

    /// The `--verify-json` path this invocation will publish a verdict to, if
    /// any. See `RunOpts::verify_json_path`: the stamp must precede the
    /// top-level preflight, not merely `record_verify`'s first statement.
    pub(crate) fn verify_json_path(&self) -> Option<&Path> {
        self.verify.then_some(self.verify_json.as_deref()).flatten()
    }

    pub fn main(&self, global: &GlobalOpts) -> Result<ExitStatus, Error> {
        if self.verify {
            validate_log_level(global)?;
            self.record_verify(global)
        } else if !self.gdbex.is_empty() {
            self.record_verify_debug(global)
        } else {
            let hermit = HermitData::from(self.data_dir.as_ref());
            let record_timeout = self.record_timeout();

            let (mut container, identity_guard) = self.recording_container(global)?;

            let recording = match record_timeout {
                Some(timeout) => {
                    let data = hermit.create_recording_dir()?;
                    let data_path = data.path().to_path_buf();
                    let exit_status = container
                        .run_guarded_at("record.main.deadline", || {
                            // Namespace init: arm the stop guards before anything else.
                            crate::container::arm_container_init_guards()?;
                            let _guard = global.init_tracing();
                            let command = self.guest_command().map_err(SerializableError::from)?;
                            let mountinfo = identity_guard
                                .mountinfo_root_rewrites()
                                .map_err(SerializableError::from)?;
                            with_recording_deadline(timeout, || {
                                hermit::record_to_with_mountinfo(command, &data_path, mountinfo)
                            })
                            .map_err(SerializableError::from)
                        })
                        .classified()?;
                    hermit.commit_recording(data, exit_status)?
                }
                None => container
                    .run_guarded_at("record.main", || {
                        // Namespace init: arm the stop guards before anything else.
                        crate::container::arm_container_init_guards()?;
                        let _guard = global.init_tracing();
                        let command = self.guest_command().map_err(SerializableError::from)?;
                        let mountinfo = identity_guard
                            .mountinfo_root_rewrites()
                            .map_err(SerializableError::from)?;
                        hermit
                            .record_with_mountinfo(command, mountinfo)
                            .map_err(SerializableError::from)
                    })
                    .classified()?,
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
        // Stamp an explicit no-result BEFORE any fallible work: the record and
        // replay steps below can fail long before a verdict exists, and a reused
        // --verify-json path must never keep showing a previous invocation's
        // green as though it described this one.
        if let Some(path) = &self.verify_json {
            write_pending_verification_json(path)?;
        }
        let strictness = if self.verify_strict {
            LogCompareStrictness::Canonical
        } else {
            LogCompareStrictness::Stripped
        };
        let ((global1, log1), (global2, log2)) =
            setup_double_run(global, "record", "replay", strictness);

        let (mut recording_container, record_identity_guard) = self.recording_container(global)?;

        eprintln!(":: {}", "Recording...".yellow().bold());

        let temp_data_dir = tempfile::tempdir()?;
        let data_dir = temp_data_dir.path();
        let record_timeout = self.record_timeout();

        let recording = recording_container
            .run_guarded_at("record_verify.record", || {
                // Namespace init: arm the stop guards before anything else.
                crate::container::arm_container_init_guards()?;
                let _guard = global1.init_tracing();

                let command = self.guest_command().map_err(SerializableError::from)?;
                let mountinfo = record_identity_guard
                    .mountinfo_root_rewrites()
                    .map_err(SerializableError::from)?;

                match record_timeout {
                    Some(timeout) => with_recording_deadline(timeout, || {
                        hermit::record_with_output_with_mountinfo(command, data_dir, mountinfo)
                    }),
                    None => hermit::record_with_output_with_mountinfo(command, data_dir, mountinfo),
                }
                .map_err(SerializableError::from)
            })
            .classified()?;

        eprintln!(":: {}", "Replaying...".yellow().bold());

        // Replay the recording.
        let (mut replay_container, _replay_identity_guard) = self.configured_container()?;
        let replay = replay_container
            .run_guarded_at("record_verify.replay", || {
                // Namespace init: arm the stop guards before anything else.
                crate::container::arm_container_init_guards()?;
                let _guard = global2.init_tracing();
                hermit::replay_with_output_and_mounts(data_dir, &self.mount)
                    .map_err(SerializableError::from)
            })
            .classified()?;

        let outcome = compare_two_runs(
            ComparedRun {
                output: &recording,
                log: log1.into_temp_path(),
                label: "the recording",
            },
            ComparedRun {
                output: &replay,
                log: log2.into_temp_path(),
                label: "the replay",
            },
            ComparisonOptions {
                verbose: false,
                strictness,
                compare_logs: true,
                diagnostic_full_trace: false,
                // Recording DOES enable the syscall output-buffer hash, so a
                // replay verdict here is a content-parity claim. It was not
                // before: this line and the recorder's own setting were two
                // independent hard-coded `false`s, and they are now one
                // constant precisely so they cannot drift apart again. See its
                // doc on `RECORD_REPLAY_HASHES_IO_BUFFERS`.
                compare_io_buffers: hermit::RECORD_REPLAY_HASHES_IO_BUFFERS,
                // Disclose the time policy this path actually runs under. It is
                // the SAME constant `record_or_replay_config` uses, so the report
                // cannot describe a policy the run did not use.
                virtualize_time: hermit::RECORD_REPLAY_VIRTUALIZES_TIME,
                keep_logs: false,
                failed_log_retention: Some(super::verify::default_failed_verify_log_retention()),
                record_envelope: RecordEnvelope::all_records_v1(),
            },
        )?;

        // Emit the machine-readable verdict (if requested) before collapsing the
        // outcome to the historical exit-code convention, so the verdict is
        // recorded whether or not the runs matched and independent of the guest's
        // own exit status.
        if let Some(path) = &self.verify_json {
            write_verification_json(path, &outcome)?;
        }
        announce_verification_outcome(
            &outcome,
            "Success: replay matched recording.",
            "Recording output did not match replay output!",
        );

        outcome.into_exit_status()
    }
    /// This is called when `--verify-with-gdbex` is passed to the command line.
    fn record_verify_debug(&self, global: &GlobalOpts) -> Result<ExitStatus, Error> {
        let (mut container, identity_guard) = self.recording_container(global)?;

        eprintln!(":: {}", "Recording...".yellow().bold());

        let temp_data_dir = tempfile::tempdir()?;
        let data_dir = temp_data_dir.path();
        let record_timeout = self.record_timeout();

        let _result = container
            .run_guarded_at("record_verify_debug.record", || {
                // Namespace init: arm the stop guards before anything else.
                crate::container::arm_container_init_guards()?;
                let _guard = global.init_tracing();

                let command = self.guest_command().map_err(SerializableError::from)?;
                let mountinfo = identity_guard
                    .mountinfo_root_rewrites()
                    .map_err(SerializableError::from)?;

                match record_timeout {
                    Some(timeout) => with_recording_deadline(timeout, || {
                        hermit::record_to_with_mountinfo(command, data_dir, mountinfo)
                    }),
                    None => hermit::record_to_with_mountinfo(command, data_dir, mountinfo),
                }
                .map_err(SerializableError::from)
            })
            .classified()?;

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
        let gdb_client = gdb_command
            .spawn()
            .context("Failed to run gdb command. Please make sure it is in your $PATH.")?;

        // TODO: For replay, we ought to construct the container from
        // `metadata.json`. That logic belongs in `hermit::replay`, but we have
        // to initialize logging inside the container because it may spawn a
        // thread. If we can guarantee that tracing won't spawn a thread, then
        // that restriction be lifted.
        // ⚠️ HAND THE CLIENT OVER BEFORE THE CONTAINER STARTS. The gdbserver
        // inside the container waits for this process with an UNBOUNDED accept,
        // so if it has already died the container blocks forever -- and the
        // `gdb_client.wait()` that would have noticed used to sit BELOW the `?`
        // on that container result, unreachable in exactly the case that needed
        // it. The watch owns the reap and releases the accept.
        let mut gdb_watch = GdbClientWatch::spawn(gdb_client, gdbserver_port);
        let (mut container, _identity_guard) = self.configured_container()?;
        let ran = container.run_guarded_at("record_verify_debug.replay", || {
            // Namespace init: arm the stop guards before anything else.
            crate::container::arm_container_init_guards()?;
            let _guard = global.init_tracing();
            hermit::replay_with_gdbserver_and_mounts(data_dir, gdbserver_port, &self.mount)
                .map_err(SerializableError::from)
        });
        let client_exited_early = gdb_watch.finish();
        match ran.classified() {
            Ok(result) => Ok(result),
            // Name the cause rather than the symptom: without this the failure
            // reads as an opaque protocol error from a session that never began.
            Err(error) if client_exited_early => {
                Err(error.context(CLIENT_EXITED_BEFORE_CONNECTING))
            }
            Err(error) => Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replay_report_is_published_before_success_is_announced() {
        let source = include_str!("record_start.rs");
        let verification = source
            .split_once("let outcome = compare_two_runs(")
            .expect("record/replay comparison")
            .1;
        let publish = verification
            .find("write_verification_json(path, &outcome)")
            .expect("verification report publication");
        let announce = verification
            .find("announce_verification_outcome(")
            .expect("verification announcement");
        assert!(publish < announce);
    }

    fn start_options(env: Vec<(String, Option<String>)>) -> StartOpts {
        StartOpts {
            program: Some(PathBuf::from("/bin/true")),
            _strict: false,
            args: Vec::new(),
            env,
            base_env: BaseEnv::Host,
            mount: Vec::new(),
            workdir: None,
            data_dir: None,
            record_timeout: None,
            verify: false,
            verify_json: None,
            verify_strict: false,
            gdbex: Vec::new(),
        }
    }

    #[test]
    fn record_guest_command_applies_explicit_environment() {
        let options = start_options(vec![("RECORD_ENV_FIXTURE".into(), Some("value".into()))]);
        let command = options.guest_command().unwrap();
        assert!(
            command
                .get_captured_envs()
                .iter()
                .any(|(name, value)| name == "RECORD_ENV_FIXTURE" && value == "value")
        );
    }

    #[test]
    fn record_guest_command_can_use_the_run_minimal_environment_and_workdir() {
        let mut options = start_options(Vec::new());
        options.base_env = BaseEnv::Minimal;
        options.workdir = Some("/test".into());
        let command = options.guest_command().unwrap();
        let env = command.get_captured_envs();
        assert_eq!(command.get_current_dir(), Some(Path::new("/test")));
        assert_eq!(
            env.get(OsStr::new("HOME")).and_then(|value| value.to_str()),
            Some("/root")
        );
        assert_eq!(
            env.get(OsStr::new("HOSTNAME"))
                .and_then(|value| value.to_str()),
            Some("hermetic-container.local")
        );
        assert_eq!(
            env.get(OsStr::new("ASAN_OPTIONS"))
                .and_then(|value| value.to_str()),
            Some("detect_leaks=0")
        );
        assert!(!env.contains_key(OsStr::new("PWD")));
        assert!(!env.contains_key(OsStr::new("OLDPWD")));
    }

    #[test]
    fn record_guest_command_refuses_missing_passthrough_environment() {
        let name = "HERMIT_RECORD_MISSING_ENV_FIXTURE_5E7D2C";
        assert!(std::env::var_os(name).is_none());
        let error = match start_options(vec![(name.into(), None)]).guest_command() {
            Ok(_) => panic!("missing pass-through environment was accepted"),
            Err(error) => error,
        };
        assert!(
            error
                .to_string()
                .contains("not set in the host environment")
        );
    }

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
            env: Vec::new(),
            base_env: BaseEnv::Host,
            mount: Vec::new(),
            workdir: None,
            data_dir: None,
            record_timeout: None,
            verify: false,
            verify_json: None,
            verify_strict: false,
            gdbex: Vec::new(),
        };
        let error = options.resolve_e9patch_record_target().unwrap_err();
        assert!(
            error.to_string().contains("resolves through /proc"),
            "unexpected error: {error}"
        );
    }
}
