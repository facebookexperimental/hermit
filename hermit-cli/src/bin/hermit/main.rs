/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// Treat all Clippy warnings as errors.
#![deny(clippy::all)]
#![allow(clippy::uninlined_format_args)]
#![allow(
    unexpected_cfgs,
    reason = "`fbcode_build` is supplied by the internal Buck build"
)]

use core::arch::global_asm;

mod analyze;
mod backends;
mod bisect;
mod clean;
mod container;
mod gdb_client;
mod global_opts;
mod guest_capture;
mod host_capabilities;
mod image;
mod instruction_map;
mod list;
mod logdiff;
mod oci;
mod podman_store;
mod record;
mod record_envelope;
mod record_start;
mod remove;
mod replay;
mod run;
mod run_evidence;
mod schedule_search;
mod strace;
mod tracing;
mod verify;
mod version;
use std::fs::File;
use std::io;
use std::os::fd::FromRawFd;
use std::path::Path;
use std::sync::atomic::AtomicI32;
use std::sync::atomic::Ordering;

const STDIN_UNCAPTURED: i32 = i32::MIN;
const STDIN_TAKEN: i32 = i32::MIN + 1;
static STARTUP_STDIN: AtomicI32 = AtomicI32::new(STDIN_UNCAPTURED);

const LITEINST_ACTIVATION_PROBE_ENV: &str = "HERMIT_INTERNAL_LITEINST_ACTIVATION_PROBE";
const LITEINST_ACTIVATION_CALLS: u64 = 32;

global_asm!(
    r#"
    .text
    .p2align 4
    .global hermit_liteinst_probe_getpid
    .hidden hermit_liteinst_probe_getpid
    .type hermit_liteinst_probe_getpid,@function
hermit_liteinst_probe_getpid:
    mov eax, 39
    .global hermit_liteinst_probe_getpid_site
    .hidden hermit_liteinst_probe_getpid_site
hermit_liteinst_probe_getpid_site:
    syscall
    nop
    nop
    nop
    ret
    .size hermit_liteinst_probe_getpid, .-hermit_liteinst_probe_getpid
"#
);

unsafe extern "C" {
    fn hermit_liteinst_probe_getpid() -> i64;
    static hermit_liteinst_probe_getpid_site: u8;
}

type LiteinstCountFn = unsafe extern "C" fn(u64) -> u64;

unsafe fn liteinst_count_function(name: &std::ffi::CStr) -> Option<LiteinstCountFn> {
    // SAFETY: RTLD_DEFAULT searches already loaded DSOs and name is terminated.
    let symbol = unsafe { libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr()) };
    if symbol.is_null() {
        return None;
    }
    // SAFETY: both required runtime counter exports have this exact C ABI.
    Some(unsafe { core::mem::transmute::<*mut libc::c_void, LiteinstCountFn>(symbol) })
}

fn liteinst_activation_probe() -> Option<ExitStatus> {
    if std::env::var_os(LITEINST_ACTIVATION_PROBE_ENV).as_deref() != Some(std::ffi::OsStr::new("1"))
    {
        return None;
    }
    let mut expected = None;
    for _ in 0..LITEINST_ACTIVATION_CALLS {
        // SAFETY: the assembly function preserves the C ABI and returns getpid.
        let observed = unsafe { hermit_liteinst_probe_getpid() };
        if *expected.get_or_insert(observed) != observed {
            eprintln!("LiteInst activation probe observed inconsistent getpid results");
            return Some(ExitStatus::Exited(126));
        }
    }
    let address = core::ptr::addr_of!(hermit_liteinst_probe_getpid_site) as usize as u64;
    // SAFETY: the expected runtime exports use the fixed counter ABI above.
    let Some(trap_count) =
        (unsafe { liteinst_count_function(c"reverie_liteinst_site_trap_count") })
    else {
        eprintln!("LiteInst activation probe could not resolve the trap counter");
        return Some(ExitStatus::Exited(126));
    };
    // SAFETY: the expected runtime exports use the fixed counter ABI above.
    let Some(hook_count) =
        (unsafe { liteinst_count_function(c"reverie_liteinst_site_hook_count") })
    else {
        eprintln!("LiteInst activation probe could not resolve the hook counter");
        return Some(ExitStatus::Exited(126));
    };
    // SAFETY: the counter functions accept the fixed syscall-site address.
    let traps = unsafe { trap_count(address) };
    // SAFETY: the counter functions accept the fixed syscall-site address.
    let hooks = unsafe { hook_count(address) };
    println!(
        "hermit-liteinst-activation calls={LITEINST_ACTIVATION_CALLS} traps={traps} hooks={hooks}"
    );
    Some(ExitStatus::Exited(i32::from(
        traps != 1 || hooks != LITEINST_ACTIVATION_CALLS - 1,
    )))
}

unsafe extern "C" fn capture_startup_stdin() {
    // SAFETY: this runs single-threaded before Rust can sanitize a closed fd 0.
    let fd = unsafe { libc::fcntl(libc::STDIN_FILENO, libc::F_DUPFD_CLOEXEC, 3) };
    let value = if fd >= 0 {
        fd
    } else {
        // SAFETY: fcntl failed in this thread, so errno contains its error.
        let errno = unsafe { *libc::__errno_location() };
        -errno - 1
    };
    STARTUP_STDIN.store(value, Ordering::Relaxed);
}

#[used]
#[unsafe(link_section = ".preinit_array")]
static CAPTURE_STARTUP_STDIN: unsafe extern "C" fn() = capture_startup_stdin;

fn startup_stdin() -> io::Result<Option<File>> {
    let value = STARTUP_STDIN.swap(STDIN_TAKEN, Ordering::AcqRel);
    if value >= 0 {
        // SAFETY: the startup hook created this owned descriptor and transfers it here once.
        return Ok(Some(unsafe { File::from_raw_fd(value) }));
    }
    if value == STDIN_UNCAPTURED || value == STDIN_TAKEN {
        return Err(io::Error::other(
            "startup stdin was not captured exactly once",
        ));
    }
    let errno = -value - 1;
    if errno == libc::EBADF {
        Ok(None)
    } else {
        Err(io::Error::from_raw_os_error(errno))
    }
}

use clap::Parser;
use colored::*;
use hermit::BackendUnavailable;
use hermit::Error;
use hermit::ExitStatus;
use hermit::HERMIT_INTERNAL_FAILURE_EXIT;

use self::analyze::AnalyzeOpts;
use self::bisect::BisectOpts;
use self::container::ContainerChildExit;
use self::container::ContainerChildPanic;
use self::container::PolicyRefusal;
use self::container::RunTimeoutMarker;
use self::container::SignalDeath;
use self::global_opts::GlobalOpts;
use self::host_capabilities::HostCapabilitiesOpts;
use self::instruction_map::InstructionMapOpts;
use self::logdiff::LogDiffCLIOpts;
use self::oci::OciOpts;
use self::record::RecordOpts;
use self::replay::ReplayOpts;
use self::run::GuestProgramFault;
use self::run::RunOpts;
use self::run_evidence::RunEvidenceSession;
use self::strace::StraceOpts;
use self::verify::write_pending_verification_json;
use self::version::Version;
use self::version::VersionOpts;

#[derive(Debug, Parser)]
#[clap(
    name = "hermit",
    version = Version::get(),
)]
struct Args {
    #[clap(flatten)]
    global: GlobalOpts,

    #[clap(subcommand)]
    command: Subcommand,
}

#[derive(Debug, Parser)]
enum Subcommand {
    /// Report the machine facilities that control validation selection.
    #[clap(name = "host-capabilities")]
    HostCapabilities(HostCapabilitiesOpts),

    /// Print version and build information.
    #[clap(name = "version")]
    Version(VersionOpts),

    /// Run a program sandboxed and fully deterministically (unless external networking is allowed).
    #[clap(name = "run", trailing_var_arg = true)]
    Run(Box<RunOpts>),

    /// Trace a program's syscalls through the selected backend.
    #[clap(name = "strace")]
    Strace(StraceOpts),

    /// Record the execution of a program (EXPERIMENTAL).
    #[clap(name = "record", trailing_var_arg = true)]
    Record(Box<RecordOpts>),

    /// Replay the execution of a program.
    #[clap(name = "replay")]
    Replay(ReplayOpts),

    /// Print one log canonically, or compare two run/record logs.
    ///
    /// COMPARING TWO SEPARATELY-PRODUCED RUNS: MAKE THE TWO COMMAND LINES
    /// BYTE-IDENTICAL, or you will measure your own inputs.
    ///
    /// The kernel places argv and the environment at the top of the initial
    /// process stack, so a command-line difference perturbs the guest before it
    /// executes a single instruction. That one fact shows up as two different
    /// failure modes, with two different triggers, and the obvious fix for the
    /// first does not fix the second:
    ///
    ///   * A LENGTH difference moves the entry stack pointer, so every stack
    ///     ADDRESS shifts. Measured: one byte of argv moved addresses by 32
    ///     bytes -- not one-for-one, because the stack is aligned -- and two
    ///     otherwise identical runs diverged 20 records in.
    ///
    ///   * A CONTENT difference at equal length leaves the addresses alone but
    ///     changes the stack BYTES, which --detlog-stack hashes. Measured: with
    ///     argv padded to equal length the stack range was identical in both
    ///     runs, yet the hash diverged 14 records in; with byte-identical argv
    ///     that same hash held for 5023 records.
    ///
    /// So padding to equal length fixes the first and leaves the second. Only
    /// byte-identical argv and environment fixes both. If the two runs must
    /// differ, put the difference somewhere other than the command line -- for
    /// example in a file the guest reads.
    ///
    /// `hermit run --verify` is NOT affected: it produces both runs from one
    /// invocation, so their command lines are identical by construction. This
    /// warning is for comparisons you assemble yourself.
    ///
    /// The general point, which outlives this particular trap: before reusing a
    /// control convention from earlier work, ask what the NEW measurement is
    /// sensitive to. Holding a run-directory name to a fixed width controls
    /// length but not content, and those are different properties.
    #[clap(verbatim_doc_comment)]
    LogDiff(LogDiffCLIOpts),

    /// Analyze Pass and failing runs
    Analyze(Box<AnalyzeOpts>),

    /// Bisect passing and failing schedules to localize a race.
    #[clap(name = "bisect", trailing_var_arg = true)]
    Bisect(Box<BisectOpts>),

    /// Generate a JSON map of nondeterministic instructions in an ELF binary.
    #[clap(name = "instruction-map")]
    InstructionMap(InstructionMapOpts),

    /// Discover and run OCI images from the local image store.
    #[clap(name = "oci")]
    Oci(Box<OciOpts>),
}

impl Subcommand {
    fn validate_backend_scope(&self, backend: Option<hermit::Backend>) -> Result<(), Error> {
        if backend == Some(hermit::Backend::Sabre)
            && !matches!(self, Subcommand::Strace(_) | Subcommand::Run(_))
        {
            // The predicate admits Strace AND Run, and `run` genuinely works --
            // measured 2026-08-06 on a 4-thread guest: `hermit --backend sabre run`
            // exited 0 with the correct deterministic result. The message used to
            // name only `strace`, so it told users a working path was unsupported
            // and hid real backend maturity. Message and predicate are now derived
            // from the same list; if the predicate changes, this text must too.
            anyhow::bail!(
                "the SaBRe backend is available only through `hermit --backend sabre run` \
                 and `hermit --backend sabre strace`"
            );
        }
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(PR-696): Review the expanded e9patch CLI scope.
        let starts_e9patch_guest = matches!(self, Subcommand::Run(_))
            || matches!(self, Subcommand::Record(record) if record.starts_recording());
        if backend == Some(hermit::Backend::E9patch) && !starts_e9patch_guest {
            anyhow::bail!(
                "the e9patch preprocessor is available only through `hermit --backend e9patch \
                 run` and `hermit --backend e9patch record`; other subcommands do not \
                 preprocess their guest"
            );
        }
        if backend == Some(hermit::Backend::Liteinst) && !matches!(self, Subcommand::Run(_)) {
            anyhow::bail!(
                "the LiteInst preload backend is available only through `hermit --backend \
                 liteinst run`; other subcommands do not use the preload runtime"
            );
        }
        if backend == Some(hermit::Backend::Kvm) && !matches!(self, Subcommand::Run(_)) {
            anyhow::bail!(
                "the KVM backend is available only through `hermit --backend kvm run`; record \
                 and replay require the ptrace runtime's sequentialized scheduler"
            );
        }
        if backend == Some(hermit::Backend::Dbt) && !matches!(self, Subcommand::Run(_)) {
            anyhow::bail!(
                "the DBT backend is available only through `hermit --backend dbt run`; record \
                 and replay use the ptrace runtime"
            );
        }
        Ok(())
    }

    /// The `--verify-json` path this invocation will publish a verdict to, if
    /// any. Only the two subcommands that can produce a verification verdict
    /// have one.
    fn verification_json_path(&self) -> Option<&Path> {
        match self {
            Subcommand::Run(run) => run.verify_json_path(),
            Subcommand::Record(record) => record.verify_json_path(),
            _ => None,
        }
    }

    fn run_evidence_request(
        &self,
        global_backend: Option<hermit::Backend>,
    ) -> Option<(&Path, hermit::Backend)> {
        match self {
            Subcommand::Run(run) => run.run_evidence_request(global_backend),
            _ => None,
        }
    }

    fn main(&mut self, global: &GlobalOpts) -> Result<ExitStatus, Error> {
        // Stamp the invocation-bound NO-RESULT record BEFORE the first fallible
        // statement of the whole program. This is the outermost point at which
        // `--verify-json` is known, and it is the only placement that dominates
        // every path that can exit without a verdict:
        //
        //   * `validate_backend_scope` immediately below;
        //   * `RunOpts::main`'s preflight -- log-level validation, stdin
        //     reservation, `validate_args`, backend availability, PMU config,
        //     mount-source and program validation, happens-before resolution,
        //     e9patch preparation;
        //   * the DBT arm, which returns `run_dbt(..)` and therefore must
        //     publish its verdict through that dedicated path;
        //   * `--namespace-only`, which likewise bypasses `verify()`;
        //   * `StartOpts::main`'s own pre-validation before `record_verify`.
        //
        // Stamping as the first statement of `verify()`/`record_verify()` did
        // NOT cover any of those: they all exit above it, leaving a previous
        // invocation's `{verified:true}` at the path to be read as this run's
        // result. If the stamp itself cannot be written we fail here rather than
        // run, so the operator learns the artifact is unreliable instead of
        // silently inheriting a stale one.
        if let Some(path) = self.verification_json_path() {
            write_pending_verification_json(path)?;
        }
        self.validate_backend_scope(global.backend)?;
        match self {
            Subcommand::HostCapabilities(x) => x.main(),
            Subcommand::Version(x) => x.main(),
            Subcommand::Run(x) => x.main(global),
            Subcommand::Strace(x) => x.main(global),
            Subcommand::Record(x) => x.main(global),
            Subcommand::Replay(x) => x.main(global),
            Subcommand::LogDiff(x) => Ok(x.main(global)),
            Subcommand::Analyze(x) => x.main(global),
            Subcommand::Bisect(x) => x.main(global),
            Subcommand::InstructionMap(x) => x.main(global),
            Subcommand::Oci(x) => x.main(global),
        }
    }
}

#[fbinit::main]
fn main() {
    // ⚠️ BEFORE ANYTHING THAT CAN FORK. The stderr diagnostic deadline is a total
    // for the INVOCATION, and the origin every hermit process measures from is a
    // shared mapping that children inherit across fork. A mapping made after the
    // first fork is not shared with the child that already exists, so each would
    // get its own origin and the deadline would be multiplied by the number of
    // container inits -- three on `--verify`, which forks once per run. This
    // placement dominates every with_container and run_guarded_at site.
    detcore::util::init_shared_stderr_deadline_origin();
    if let Some(status) = liteinst_activation_probe() {
        status.raise_or_exit();
    }
    let Args {
        mut global,
        mut command,
    } = Args::parse();

    // Claim the evidence destination before any fallible run preflight. The
    // directory itself is the invocation boundary: it must not exist, so no
    // stale success can survive and no concurrent invocation can share it.
    let evidence = match command.run_evidence_request(global.backend) {
        Some((directory, backend)) => match RunEvidenceSession::create(directory, backend) {
            Ok(session) => Some(session),
            Err(error) => {
                display_error(error);
                ExitStatus::Exited(HERMIT_INTERNAL_FAILURE_EXIT).raise_or_exit();
            }
        },
        None => None,
    };
    if let Some(session) = &evidence {
        global.set_run_evidence_log_handle(session.log_handle(), session.write_error_latch());
    }

    // Open --log-file HERE, in the host's filename namespace, before any container
    // exists. This is the moment a shell would perform `> file`, and doing it later
    // -- inside the container, where tracing must be initialized -- resolves the path
    // against the guest's fresh /tmp and silently discards the log.
    let result = global.open_log_file().and_then(|()| command.main(&global));

    // The manifest is the commit marker and is published only after the run
    // result and private log are final. Evidence failure is reported but cannot
    // replace the guest's status; a consumer observes the absent/no-result
    // manifest and fails closed independently of the public CLI channel.
    let evidence_error = evidence.and_then(|session| session.finish(result.as_ref()).err());
    let status = result.unwrap_or_else(|error| {
        let status = ExitStatus::Exited(failure_exit_code(&error));
        display_error(error);
        status
    });
    if let Some(error) = evidence_error {
        display_error(error);
    }
    status.raise_or_exit();
}

/// Machine-readable classification of a hermit-internal failure, on stderr.
///
/// ⚠️ WHY A STDERR MARKER AND NOT A SECOND EXIT CODE. Every value in `0..=255`
/// is a legal guest exit status and both channels a process has — code and
/// terminating signal — are already spoken for by the guest, so exit codes can
/// only ever REDUCE collisions, never remove them. Spending a second reserved
/// number to separate two *internal* failures buys the least and costs the most.
///
/// ⚠️ AND WHY NOT THE VERIFICATION JSON, which is the other candidate and looks
/// structurally stronger. Measured on `main` at `b92c2227fc`, it cannot see
/// these cases at all:
///   * `--verify-json` is clap-`requires`-gated on `--verify`, so a plain
///     `hermit run` has no channel — measured: `error: the following required
///     arguments were not provided: --verify`;
///   * with both flags, an `--log-file` failure still writes NO record — the
///     stamp lives inside `Subcommand::main`, and `open_log_file` fails above it;
///   * a bad flag never reaches hermit's code at all (clap exits 2 itself).
///
/// A channel that is absent for the common invocation cannot be the mechanism
/// that distinguishes failure classes. It remains the right home for a RICH
/// verdict when a path was requested; it is not an alternative to this.
///
/// stderr has neither limitation: it always exists, it has no 256-value ceiling,
/// and it needs no new plumbing — only that the typed status stop being
/// discarded, which is what [`ContainerChildExit`] now prevents.
/// The exit code for a failure, DERIVED FROM ITS CLASS rather than hardcoded.
///
/// ⚠️ THIS IS THE PIECE THAT WAS MISSING, AND ITS ABSENCE IS WHY ONE DEFECT KEPT
/// REAPPEARING. The failure's class is reconstructed at three boundaries and was
/// then thrown away again here, because the exit-code decision was a constant
/// that consulted nothing. Every new distinction therefore needed its own patch.
/// Expressed as a mapping FROM the class, the next distinction is a line in this
/// match instead of another fix to the same shape.
///
/// Most classes deliberately still map to 125: separating two INTERNAL failures
/// does not justify spending a reserved number, since every value in `0..=255`
/// is a legal guest status — that is the argument at
/// [`HERMIT_INTERNAL_FAILURE_EXIT`] and it is unchanged. A GUEST-SIDE fault is
/// the case that does justify one, and 127/126 are the established GNU
/// `env`/`chroot`/`timeout` conventions that 125 itself came from. The two
/// positions are consistent: the code channel exists to tell hermit's failures
/// apart from the guest's, not to subdivide hermit's own.
fn failure_exit_code(error: &Error) -> i32 {
    // A completed comparison with a recorded PMU skid overshoot is an
    // understood infrastructure failure. Hermit deliberately declines to
    // vouch for the result, so it uses the same coarse exit class as other
    // policy refusals; the verification receipt carries the finer cause and
    // count.
    if error.downcast_ref::<hermit::SkidOvershootError>().is_some() {
        return detcore_model::HERMIT_POLICY_REFUSAL_EXIT;
    }
    // ⚠️ A REFUSAL IS NOT A FAILURE, AND IT IS CHECKED FIRST FOR THAT REASON.
    // A fail-closed policy stopping the run is hermit working; reporting it as
    // HERMIT_INTERNAL_FAILURE_EXIT tells the operator to file a bug against a
    // shutdown path that behaved exactly as designed.
    if error.downcast_ref::<PolicyRefusal>().is_some() {
        return detcore_model::HERMIT_POLICY_REFUSAL_EXIT;
    }
    // A signal death reports 128 + signo, the status the shell would have shown
    // had the container been an ordinary process. Also not a failure.
    if let Some(SignalDeath(signal)) = error.downcast_ref::<SignalDeath>() {
        return detcore_model::signal_exit_status(*signal);
    }
    // ⚠️ A DEADLINE IS NOT A HERMIT FAILURE EITHER. The bound the caller asked
    // for was reached and hermit tore the container down on purpose, so
    // reporting HERMIT_INTERNAL_FAILURE_EXIT would send the operator to file a
    // bug against a mechanism that did its job. 124 is the established code for
    // "a deadline fired" -- GNU `timeout`, `safehermit`'s wall bound, and
    // `hermit record --record-timeout` all already use it, so this adds no new
    // meaning to the number.
    if error.downcast_ref::<RunTimeoutMarker>().is_some() {
        return 124;
    }
    match error.downcast_ref::<GuestProgramFault>() {
        Some(fault) => fault.exit_code(),
        None => HERMIT_INTERNAL_FAILURE_EXIT,
    }
}

fn classify_failure(error: &Error) -> String {
    if let Some(overshoot) = error.downcast_ref::<hermit::SkidOvershootError>() {
        return format!(
            "HERMIT_POLICY_REFUSAL class=policy-refusal cause=skid-overshoot count={}",
            overshoot.count()
        );
    }
    // ⚠️ REFUSAL BEFORE FAILURE. This arm and the ContainerChildExit arm below
    // describe the SAME observation -- a container child that exited -- and
    // only the status separates them. Ordering matters: a refusal that fell
    // through to the arm below would be reported as an accident, which is the
    // defect this exists to remove.
    if error.downcast_ref::<PolicyRefusal>().is_some() {
        // NOT HERMIT_INTERNAL_FAILURE: hermit did not fail. It refused, on
        // purpose, and printed why immediately above this line.
        return "HERMIT_POLICY_REFUSAL class=policy-refusal".to_string();
    }
    // ⚠️ THE MARKER IS WHAT MAKES 130 UNAMBIGUOUS, exactly as it is for 122. A
    // guest may exit 130 of its own accord and hermit cannot stop it; the status
    // alone therefore cannot say who produced it. `signal=` is included because
    // the band has several members and the number is the report.
    if let Some(SignalDeath(signal)) = error.downcast_ref::<SignalDeath>() {
        return format!("HERMIT_SIGNAL_DEATH class=signal-death signal={}", signal);
    }
    // NOT HERMIT_INTERNAL_FAILURE: the bound fired and hermit unwound the
    // container deliberately. The seconds are in the line because "it timed
    // out" without the bound it exceeded is the anonymous kill this replaces.
    if error.downcast_ref::<RunTimeoutMarker>().is_some() {
        return "HERMIT_RUN_TIMEOUT class=run-timeout".to_string();
    }
    // ONE discriminant, read once, covering all three flattenings.
    if let Some(ContainerChildExit(status)) = error.downcast_ref::<ContainerChildExit>() {
        // The child died with a status IT DID NOT CHOOSE: a kill, a fault, a
        // panic no handler caught. Nothing reported it; reverie observed it.
        return format!("HERMIT_INTERNAL_FAILURE class=container-child-exit status={status:?}");
    }
    if let Some(fault) = error.downcast_ref::<GuestProgramFault>() {
        // ⚠️ A GUEST-SIDE FACT, NOT A HERMIT FAILURE. Hermit worked; the path it
        // was handed is not executable. Bucketing this with `cli-error` told a
        // caller "hermit is broken" for what is almost always a typo.
        return format!("HERMIT_INTERNAL_FAILURE class={}", fault.class());
    }
    if let Some(unavailable) = error.downcast_ref::<BackendUnavailable>() {
        return format!(
            "HERMIT_INTERNAL_FAILURE class=backend-unavailable backend={}",
            unavailable.backend().as_str()
        );
    }
    if error.downcast_ref::<ContainerChildPanic>().is_some() {
        // The child PANICKED and the panic was caught and reported. Still the
        // tracer breaking, not the CLI refusing -- which is the distinction the
        // `kind` discriminant on SerializableError exists to carry.
        return "HERMIT_INTERNAL_FAILURE class=container-child-panic".to_string();
    }
    // Everything else: bad flag, unwritable log path, unreadable program.
    "HERMIT_INTERNAL_FAILURE class=cli-error".to_string()
}

fn display_error(error: Error) {
    // NOT `eprintln!` on any of these three.
    //
    // ⚠️ THIS IS THE LAST THING HERMIT SAYS, AND A GUEST CAN CUT IT OFF. fd 2 is
    // an open file description shared with the guest, so `fcntl(2, F_SETFL,
    // O_NONBLOCK)` in the guest makes these writes fail with EAGAIN once the
    // reader is slow. `eprintln!` calls `write_all`, `write_all` does not retry
    // EAGAIN, and the print macro panics -- taking the rest of this report with
    // it. Measured 2026-08-26 against a 4096-byte pipe with 196 bytes free: 170
    // of 2240 bytes arrived, the process exited 101 instead of 127, and the
    // delivered text was not even a prefix of the truth -- the first 92 bytes of
    // this report were followed by the tail of the panic message, so it read as
    // a complete short error rather than a severed one.
    //
    // The ordering below (class first) was already written for a truncated
    // capture. Ordering limits the damage; it does not prevent it.
    use std::io::Write;
    let mut out = detcore::util::RetryingStderr;
    let _ = writeln!(out, "{}", classify_failure(&error));

    let mut chain = error.chain();

    if let Some(error) = chain.next() {
        let _ = writeln!(out, "{}: {}", "Error".red().bold(), error);
    }

    for cause in chain {
        let _ = writeln!(out, "     {} {}", ">".dimmed().bold(), cause);
    }
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;
    use clap::Parser;
    use hermit::Backend;
    use hermit::BackendUnavailable;
    use hermit::Error;

    use super::Args;
    use super::Subcommand;
    use super::classify_failure;
    use super::failure_exit_code;

    #[test]
    fn clap_configuration_is_valid() {
        Args::command().debug_assert();
    }

    #[test]
    fn backend_unavailability_is_classified_by_type_not_message() {
        let typed = Error::new(BackendUnavailable::new(Backend::Dbt, "SDK absent"));
        assert_eq!(
            classify_failure(&typed),
            "HERMIT_INTERNAL_FAILURE class=backend-unavailable backend=dbt"
        );

        let prose_only = Error::msg("backend `dbt` is unavailable: SDK absent");
        assert_eq!(
            classify_failure(&prose_only),
            "HERMIT_INTERNAL_FAILURE class=cli-error",
            "changing only prose must not manufacture the typed class"
        );
    }

    #[test]
    fn skid_overshoot_uses_the_policy_refusal_exit_and_marker() {
        let error = Error::new(hermit::SkidOvershootError::new(2));
        assert_eq!(
            failure_exit_code(&error),
            detcore_model::HERMIT_POLICY_REFUSAL_EXIT
        );
        assert_eq!(
            classify_failure(&error),
            "HERMIT_POLICY_REFUSAL class=policy-refusal cause=skid-overshoot count=2"
        );
    }

    /// Plant a previous invocation's GREEN verdict at `path`, the way a caller
    /// that reuses one `--verify-json` file across runs would have.
    fn plant_previous_green(path: &std::path::Path) {
        std::fs::write(
            path,
            "{\"verified\":true,\"bitwise_parity\":true,\"verdict\":\"matched\"}\n",
        )
        .unwrap();
        let planted: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(planted["verified"], serde_json::json!(true));
    }

    fn read_verdict(path: &std::path::Path) -> serde_json::Value {
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    /// The record the stamp must leave behind on every non-verdict exit.
    fn assert_no_result(path: &std::path::Path, context: &str) {
        let now = read_verdict(path);
        assert_eq!(now["verdict"], serde_json::json!("no_result"), "{context}");
        assert_eq!(now["verified"], serde_json::json!(false), "{context}");
        assert_eq!(now["bitwise_parity"], serde_json::json!(false), "{context}");
    }

    /// Drive `Subcommand::main` for `argv` and assert that (a) it exits Err
    /// without reaching a verdict, and (b) the planted green has been replaced
    /// by an invocation-bound no-result.
    ///
    /// Each case is a DIFFERENT top-level exit that occurs ABOVE
    /// `verify()`/`record_verify()`. Stamping as the first statement of those
    /// inner functions did not cover any of them.
    fn assert_top_level_exit_leaves_no_result(argv: &[&str], context: &str) {
        let file = tempfile::NamedTempFile::new().unwrap();
        let path = file.path().to_path_buf();
        plant_previous_green(&path);

        let json = format!("--verify-json={}", path.display());
        let mut full: Vec<&str> = argv.to_vec();
        full.push(&json);
        full.push("--");
        // A guest that cannot pass program validation, so no case here can
        // accidentally start a real run and reach a genuine verdict.
        full.push("/nonexistent/hermit-test-guest");

        let mut args = Args::try_parse_from(full).expect("argv should parse");
        let result = args.command.main(&args.global);
        assert!(result.is_err(), "{context}: expected a non-verdict exit");
        assert_no_result(&path, context);
    }

    /// TOP-LEVEL EXIT 1 -- the main preflight (`validate_backend_scope`), which
    /// runs before `RunOpts::main` is even entered.
    #[test]
    fn main_preflight_exit_leaves_an_invocation_bound_no_result() {
        assert_top_level_exit_leaves_no_result(
            &["hermit", "--backend", "kvm", "record", "--verify"],
            "backend-scope preflight",
        );
    }

    // The series jumps 1 -> 3 on purpose. A DBT-flavoured case sat at 2 and was
    // removed with #2359; it should NOT be recreated, because it could not fail
    // on its own. `assert_top_level_exit_leaves_no_result` always appends a
    // nonexistent guest, and `RunOpts::main` calls `validate_program()`
    // (run.rs:2149) BEFORE the `match backend` that returns through the DBT
    // adapter (run.rs:2166), so such a case exits at exactly the statement
    // EXIT 3 already covers and never consults the backend at all. It was
    // `run_preflight_exit_leaves_an_invocation_bound_no_result` with one flag
    // changed. Executing is not the same as discriminating.
    //
    // What DID need restoring from that deletion is the ordering premise
    // immediately below.

    /// `RunOpts::main` returns through the dedicated DBT adapter BEFORE the
    /// generic `verify()` dispatch. That ordering is the whole reason `run_dbt`
    /// has to carry the verdict-artifact path itself: if the generic dispatch
    /// ran first, it would not.
    ///
    /// The next test's doc comment has asserted this in prose since #2359 while
    /// nothing checked it, which is the worse failure mode — a reader trusts a
    /// documented guarantee. Restored rather than deleted because reading
    /// `run.rs` confirms the property still holds. Only the OTHER half of the
    /// original assertion, that a DBT run therefore keeps `no_result`, was made
    /// false by `f0584c1aac` (which gave `run_dbt` a `verify_json` path, a
    /// `ComparisonOptions`, and a real `compare_two_runs` outcome), and that
    /// half is deliberately not restored.
    ///
    /// Both needles must appear EXACTLY once. A bare `find` is satisfied by a
    /// comment quoting the string, and #2359's own round-2 review caught
    /// precisely that: a text assertion a comment can satisfy is not a pin.
    /// Requiring uniqueness turns such a quote into a loud failure.
    #[test]
    fn dbt_arm_returns_before_the_generic_verify_dispatch() {
        let source = include_str!("run.rs");
        let sole_offset = |needle: &str| -> usize {
            let hits: Vec<usize> = source.match_indices(needle).map(|(i, _)| i).collect();
            assert_eq!(
                hits.len(),
                1,
                "expected exactly one occurrence of {needle:?} in run.rs, found {}. Zero means \
                 the shape this check reads has moved or gone, so it can no longer see the \
                 ordering; two or more (including one inside a comment) would make the check \
                 meaningless rather than merely wrong",
                hits.len()
            );
            hits[0]
        };
        let dbt_return = sole_offset("return super::backends::run_dbt(");
        let generic_verify = sole_offset("self.verify(global)");
        assert!(
            dbt_return < generic_verify,
            "RunOpts::main must return through the dedicated DBT adapter before the generic \
             verify() dispatch; if that order ever reverses, run_dbt no longer needs to carry \
             the verdict-artifact path and dbt_arm_has_a_channel_to_publish_a_verdict is \
             asserting something that no longer follows"
        );
    }

    /// The DBT arm of `RunOpts::main` returns `run_dbt(..)` and never reaches
    /// the common `verify()` function, so both cfg arms of `run_dbt` must carry
    /// the verdict-artifact path explicitly. The ordering that makes that true
    /// is pinned by `dbt_arm_returns_before_the_generic_verify_dispatch`.
    ///
    /// Asserted structurally rather than by executing the arm so this focused
    /// test cannot block on the harness stdin or require DynamoRIO.
    #[test]
    fn dbt_arm_has_a_channel_to_publish_a_verdict() {
        let source = include_str!("backends.rs");
        let production = source
            .split_once("#[cfg(test)]")
            .expect("backends test module")
            .0;
        let signatures: Vec<&str> = production
            .match_indices("fn run_dbt(")
            .map(|(i, _)| {
                let rest = &production[i..];
                &rest[..rest
                    .find(") -> Result<ExitStatus, Error>")
                    .expect("run_dbt signature")]
            })
            .collect();
        assert_eq!(signatures.len(), 2, "expected both cfg arms of run_dbt");
        for signature in signatures {
            assert!(
                signature.contains("verify"),
                "run_dbt should still receive the verify flag"
            );
            assert!(
                signature.contains("verify_json"),
                "run_dbt must receive the verdict-artifact path because the DBT arm bypasses \
                 the common verify() function:\n{signature}"
            );
        }
    }

    /// The DBT arm builds its own [`verify::ComparisonOptions`], so the record
    /// envelope it names is not covered by the generic verifier's tests. Pin it
    /// to the canonical transport policy selected by Reverie's authenticated
    /// decoder. An opaque `caller_defined` predicate here would silently
    /// disqualify every DBT verdict from bitwise parity, while `all_records_v1`
    /// would falsely claim that initialization records remain in the log.
    #[test]
    fn dbt_verdict_names_a_canonical_record_envelope() {
        let source = include_str!("backends.rs");
        let start = source
            .find("= compare_two_runs(")
            .expect("DBT arm must compare two runs");
        let options = source[start..]
            .find("ComparisonOptions {")
            .map(|offset| &source[start + offset..])
            .expect("DBT comparison must build ComparisonOptions");
        let block = &options[..options.find("\n        },").expect("options block end")];

        let record_envelope_literal = || {
            block
                .lines()
                .find_map(|line| line.trim().strip_prefix("record_envelope:"))
                .map(|value| value.trim().trim_end_matches(',').to_string())
                .expect(
                    "the DBT comparison must state its record envelope; an unnamed selection \
                     is exactly the undisclosed filtering the envelope exists to prevent",
                )
        };
        assert_eq!(
            record_envelope_literal(),
            "RecordEnvelope::dbt_evidence_transport_v1()",
            "the DBT adapter compares every non-transport evidence record and compares the \
             authenticated initialization count separately. Changing this envelope changes \
             which records are compared, so update the adapter and its evidence together, not \
             just this literal"
        );
        // Naming the envelope is worth nothing if the adapter applies another
        // unreported filter while materializing the already decoded records.
        // `write_canonical_info_with_filter` invites that at a backend boundary,
        // so pin the unfiltered call.
        let materialize = source
            .find("fn materialize_dbt_comparison_log")
            .expect("DBT comparison log materialization");
        let body = &source[materialize..];
        let body = &body[..body.find("\n}\n").expect("materialization body end")];
        assert!(
            body.contains("detcore::logdiff::write_canonical_info(path, &mut std::io::sink())"),
            "the DBT comparison log must be materialized unfiltered; filtering here would \
             contradict the envelope the verdict publishes"
        );
        assert!(
            !body.contains("write_canonical_info_with_filter"),
            "filtering at the DBT boundary would apply an undisclosed second policy"
        );
        // Bind the literal above to the real policy, so renaming the
        // constructor without preserving its meaning fails here too.
        assert!(
            crate::record_envelope::RecordEnvelope::dbt_evidence_transport_v1()
                .policy()
                .is_canonical(),
            "dbt_evidence_transport_v1 must remain canonical or DBT verdicts silently lose \
             bitwise-parity eligibility"
        );
        assert_eq!(
            crate::record_envelope::RecordEnvelope::dbt_evidence_transport_v1()
                .policy()
                .as_str(),
            "dbt_evidence_transport_v1"
        );
    }

    /// `--namespace-only` appears on the list of paths that bypass `verify()`,
    /// but it is NOT reachable with a verdict artifact: clap rejects
    /// `--verify` together with `--namespace-only`, and `--verify-json`
    /// requires `--verify`. Asserted rather than guarded, so the day that
    /// conflict is relaxed this test fails and the stamp coverage is revisited
    /// instead of silently developing a hole.
    #[test]
    fn namespace_only_cannot_carry_a_verdict_artifact() {
        let parsed = Args::try_parse_from([
            "hermit",
            "run",
            "--verify",
            "--namespace-only",
            "--",
            "/bin/true",
        ]);
        assert!(
            parsed.is_err(),
            "--verify with --namespace-only must remain a parse-time conflict; if this now \
             parses, --namespace-only bypasses verify() and needs the pending stamp too"
        );
    }

    /// TOP-LEVEL EXIT 3 -- `RunOpts::main`'s own preflight, entered after the
    /// dispatcher and still far above `verify()`.
    ///
    /// The case chosen is `validate_log_level`, which is the FIRST fallible
    /// statement of `RunOpts::main`. That choice is deliberate: everything after
    /// it reaches `reserve_output_stdin_snapshot(startup_stdin()?)`, which
    /// BLOCKS reading the harness's stdin, so a test driving any later preflight
    /// step through `main()` hangs instead of failing. The later steps
    /// (`validate_args`, `ensure_available`, `install_pmu_config`,
    /// `validate_mount_sources`, `validate_program`, happens-before resolution,
    /// e9patch preparation) are therefore NOT exercised here; they are covered
    /// by construction, because the stamp is the first statement of
    /// `Subcommand::main` and so dominates every one of them.
    #[test]
    fn run_preflight_exit_leaves_an_invocation_bound_no_result() {
        assert_top_level_exit_leaves_no_result(
            &[
                "hermit",
                "--log",
                "warn",
                "run",
                "--verify",
                "--backend=ptrace",
            ],
            "RunOpts::main log-level preflight",
        );
    }

    /// TOP-LEVEL EXIT 4 -- `StartOpts::main` pre-validation: the record path
    /// validates the log level before calling `record_verify`.
    #[test]
    fn record_start_prevalidation_exit_leaves_an_invocation_bound_no_result() {
        assert_top_level_exit_leaves_no_result(
            &["hermit", "--log", "warn", "record", "start", "--verify"],
            "record start log-level pre-validation",
        );
    }

    /// POSITIVE control: the stamp is not a dead end. A subcommand that carries
    /// no `--verify-json` must not have a path at all, so nothing is written and
    /// no unrelated file is disturbed.
    #[test]
    fn subcommands_without_verify_json_have_no_verdict_path() {
        for argv in [
            vec!["hermit", "run", "--", "/bin/true"],
            vec!["hermit", "record", "start", "--", "/bin/true"],
            vec!["hermit", "run", "--verify", "--", "/bin/true"],
        ] {
            let args = Args::try_parse_from(argv.clone()).expect("argv should parse");
            assert!(
                args.command.verification_json_path().is_none(),
                "{argv:?} should carry no verification-json path"
            );
        }
    }

    /// POSITIVE control for the accessor that feeds the stamp: when
    /// `--verify-json` IS present, every spelling that can produce a verdict
    /// reports it -- including `record`'s flattened direct form, which is a
    /// different code path from `record start`.
    #[test]
    fn every_verdict_producing_spelling_reports_its_verdict_path() {
        for argv in [
            vec![
                "hermit",
                "run",
                "--verify",
                "--verify-json=/tmp/v.json",
                "--",
                "/bin/true",
            ],
            vec![
                "hermit",
                "record",
                "--verify",
                "--verify-json=/tmp/v.json",
                "--",
                "/bin/true",
            ],
            vec![
                "hermit",
                "record",
                "start",
                "--verify",
                "--verify-json=/tmp/v.json",
                "--",
                "/bin/true",
            ],
        ] {
            let args = Args::try_parse_from(argv.clone()).expect("argv should parse");
            assert_eq!(
                args.command.verification_json_path(),
                Some(std::path::Path::new("/tmp/v.json")),
                "{argv:?} must report its verdict path to the stamp"
            );
        }
    }

    #[test]
    fn replay_accepts_an_optional_id_and_options() {
        let args = Args::try_parse_from([
            "hermit",
            "replay",
            "--autopilot",
            "--data-dir",
            "/tmp/recordings",
            "0123456789abcdef0123456789abcdef",
        ])
        .unwrap();

        assert!(matches!(args.command, Subcommand::Replay(_)));
    }

    #[test]
    fn replay_accepts_serve_only_for_an_external_debugger() {
        let args =
            Args::try_parse_from(["hermit", "replay", "--serve-only", "--gdbserver-port=2345"])
                .unwrap();

        assert!(matches!(args.command, Subcommand::Replay(_)));
    }

    #[test]
    fn replay_serve_only_rejects_gdb_commands() {
        let error =
            Args::try_parse_from(["hermit", "replay", "--serve-only", "--gdbex=break main"])
                .unwrap_err();

        assert_eq!(error.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn bisect_accepts_schedule_endpoints_and_run_args() {
        let args = Args::try_parse_from([
            "hermit",
            "bisect",
            "--good",
            "good.json",
            "--bad",
            "bad.json",
            "--",
            "--max-timeslice=disabled",
            "/bin/true",
        ])
        .unwrap();

        assert!(matches!(args.command, Subcommand::Bisect(_)));
    }

    #[test]
    fn backend_parses_in_global_position() {
        use hermit::Backend;

        let args = Args::try_parse_from(["hermit", "--backend", "kvm", "run", "prog"])
            .expect("global-position --backend should parse");
        assert_eq!(args.global.backend, Some(Backend::Kvm));
        assert!(matches!(args.command, Subcommand::Run(_)));
    }

    #[test]
    fn e9patch_is_allowed_for_recording_but_rejected_for_management_and_replay() {
        use hermit::Backend;

        for command in [
            vec![
                "hermit",
                "--backend",
                "e9patch",
                "record",
                "start",
                "--",
                "/bin/true",
            ],
            vec![
                "hermit",
                "--backend",
                "e9patch",
                "record",
                "--",
                "/bin/true",
            ],
        ] {
            let args = Args::try_parse_from(command).unwrap();
            args.command
                .validate_backend_scope(Some(Backend::E9patch))
                .unwrap();
        }

        for command in [
            vec!["hermit", "--backend", "e9patch", "record", "list"],
            vec![
                "hermit",
                "--backend",
                "e9patch",
                "replay",
                "0123456789abcdef0123456789abcdef",
            ],
        ] {
            let args = Args::try_parse_from(command).unwrap();
            let error = args
                .command
                .validate_backend_scope(Some(Backend::E9patch))
                .unwrap_err();
            assert!(error.to_string().contains("only through"));
        }
    }

    #[test]
    fn liteinst_is_rejected_outside_run() {
        use hermit::Backend;

        let args = Args::try_parse_from([
            "hermit",
            "--backend",
            "liteinst",
            "record",
            "list",
            "--json",
        ])
        .unwrap();
        let error = args
            .command
            .validate_backend_scope(Some(Backend::Liteinst))
            .unwrap_err();
        assert!(error.to_string().contains("only through"));
    }

    #[test]
    fn kvm_is_rejected_outside_run_instead_of_silently_recording_with_ptrace() {
        use hermit::Backend;

        let args = Args::try_parse_from([
            "hermit",
            "--backend",
            "kvm",
            "record",
            "start",
            "--",
            "/bin/true",
        ])
        .unwrap();
        let error = args
            .command
            .validate_backend_scope(Some(Backend::Kvm))
            .unwrap_err();
        assert!(error.to_string().contains("require the ptrace runtime"));
    }

    #[test]
    fn dbt_is_rejected_outside_run_instead_of_silently_recording_with_ptrace() {
        use hermit::Backend;

        let run =
            Args::try_parse_from(["hermit", "--backend", "dbt", "run", "--", "/bin/true"]).unwrap();
        run.command
            .validate_backend_scope(Some(Backend::Dbt))
            .expect("DBT run is the supported DBT execution path");

        let record = Args::try_parse_from([
            "hermit",
            "--backend",
            "dbt",
            "record",
            "start",
            "--",
            "/bin/true",
        ])
        .unwrap();
        let error = record
            .command
            .validate_backend_scope(Some(Backend::Dbt))
            .expect_err("DBT record must not silently execute through ptrace");
        assert!(error.to_string().contains("use the ptrace runtime"));
    }

    #[test]
    fn record_accepts_strict_direct_and_start_forms() {
        for args in [
            vec!["hermit", "record", "--strict", "--", "/bin/echo", "hello"],
            vec![
                "hermit",
                "record",
                "start",
                "--strict",
                "--",
                "/bin/echo",
                "hello",
            ],
        ] {
            let parsed = Args::try_parse_from(args).expect("record --strict should parse");
            assert!(matches!(parsed.command, Subcommand::Record(_)));
        }
    }

    /// The scope PREDICATE and the scope MESSAGE must name the same set.
    ///
    /// They drifted apart: the predicate admitted `Strace | Run` while the text
    /// said "only ... strace", so a working path was documented as unsupported.
    /// Both directions are asserted, because either half alone is satisfiable by
    /// a wrong fix -- narrowing the predicate to strace would pass a
    /// message-only test, and leaving the message alone would pass a
    /// predicate-only test.
    #[test]
    fn sabre_scope_admits_exactly_run_and_strace() {
        use hermit::Backend;

        // POSITIVE: both admitted subcommands must pass the scope guard.
        for sub in ["run", "strace"] {
            let args = Args::try_parse_from([
                "hermit",
                "--backend",
                "sabre",
                sub,
                "--",
                "/bin/echo",
                "hi",
            ])
            .unwrap_or_else(|e| panic!("`--backend sabre {sub}` should parse: {e}"));
            assert_eq!(args.global.backend, Some(Backend::Sabre));
            args.command
                .validate_backend_scope(args.global.backend)
                .unwrap_or_else(|e| panic!("`--backend sabre {sub}` must be in scope: {e}"));
        }

        // NEGATIVE: something outside the set is still refused. Without this the
        // guard could admit everything and still pass the positives above.
        let args = Args::try_parse_from([
            "hermit",
            "--backend",
            "sabre",
            "log-diff",
            "/dev/null",
            "/dev/null",
        ])
        .expect("log-diff should parse");
        let err = args
            .command
            .validate_backend_scope(args.global.backend)
            .expect_err("`--backend sabre log-diff` must be rejected");

        // And the refusal must NAME both supported forms. A message that omits
        // `run` is the original defect: it sends a user away from a path that
        // works.
        let text = err.to_string();
        assert!(text.contains("sabre run"), "message must name run: {text}");
        assert!(
            text.contains("sabre strace"),
            "message must name strace: {text}"
        );
    }

    #[test]
    fn sabre_strace_command_parses_in_requested_form() {
        use hermit::Backend;

        let args = Args::try_parse_from([
            "hermit",
            "--backend",
            "sabre",
            "strace",
            "--",
            "/bin/echo",
            "hello",
        ])
        .expect("requested SaBRe strace form should parse");
        assert_eq!(args.global.backend, Some(Backend::Sabre));
        assert!(matches!(args.command, Subcommand::Strace(_)));
    }

    #[test]
    fn sabre_strace_rejects_run_options_it_does_not_honor() {
        for option in [
            "--namespace-only",
            "--verify",
            "--strict",
            "--env=SHOULD_NOT_BE_IGNORED=1",
            "--workdir=/tmp",
        ] {
            let result = Args::try_parse_from([
                "hermit",
                "--backend",
                "sabre",
                "strace",
                option,
                "--",
                "/bin/true",
            ]);
            assert!(
                result.is_err(),
                "SaBRe strace unexpectedly accepted unsupported option {option}"
            );
        }
    }

    #[test]
    fn record_accepts_a_positive_timeout() {
        Args::try_parse_from([
            "hermit",
            "record",
            "start",
            "--record-timeout=1",
            "--",
            "/bin/true",
        ])
        .unwrap();
    }

    #[test]
    fn record_rejects_a_zero_timeout() {
        assert!(
            Args::try_parse_from([
                "hermit",
                "record",
                "start",
                "--record-timeout=0",
                "--",
                "/bin/true",
            ])
            .is_err()
        );
    }

    #[test]
    fn instruction_map_accepts_binary_and_cache_directory() {
        let args = Args::try_parse_from([
            "hermit",
            "instruction-map",
            "--cache-dir",
            "/tmp/instruction-maps",
            "/bin/ls",
        ])
        .unwrap();

        assert!(matches!(args.command, Subcommand::InstructionMap(_)));
    }
}
