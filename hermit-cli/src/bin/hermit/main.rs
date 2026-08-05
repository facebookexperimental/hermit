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
mod bnz;
mod clean;
mod container;
mod global_opts;
mod image;
mod instruction_map;
mod list;
mod logdiff;
mod record;
mod record_start;
mod remove;
mod replay;
mod run;
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
use hermit::Error;
use hermit::ExitStatus;

use self::analyze::AnalyzeOpts;
use self::bisect::BisectOpts;
use self::global_opts::GlobalOpts;
use self::instruction_map::InstructionMapOpts;
use self::logdiff::LogDiffCLIOpts;
use self::record::RecordOpts;
use self::replay::ReplayOpts;
use self::run::RunOpts;
use self::strace::StraceOpts;
use self::verify::write_pending_verification_json;
use self::version::Version;

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
    /// Run a program sandboxed and fully deterministically (unless external networking is allowed).
    #[clap(name = "run", trailing_var_arg = true)]
    Run(Box<RunOpts>),

    /// Trace a program's syscalls through the selected backend.
    #[clap(name = "strace")]
    Strace(StraceOpts),

    /// Record the execution of a program (EXPERIMENTAL).
    #[clap(name = "record", trailing_var_arg = true)]
    Record(RecordOpts),

    /// Replay the execution of a program.
    #[clap(name = "replay")]
    Replay(ReplayOpts),

    /// Take the difference of two (run/record) logs written to files.
    LogDiff(LogDiffCLIOpts),

    /// Analyze Pass and failing runs
    Analyze(Box<AnalyzeOpts>),

    /// Bisect passing and failing schedules to localize a race.
    #[clap(name = "bisect", trailing_var_arg = true)]
    Bisect(Box<BisectOpts>),

    /// Generate a JSON map of nondeterministic instructions in an ELF binary.
    #[clap(name = "instruction-map")]
    InstructionMap(InstructionMapOpts),
}

impl Subcommand {
    fn validate_backend_scope(&self, backend: Option<hermit::Backend>) -> Result<(), Error> {
        if backend == Some(hermit::Backend::Sabre)
            && !matches!(self, Subcommand::Strace(_) | Subcommand::Run(_))
        {
            anyhow::bail!(
                "the SaBRe backend is available only through `hermit --backend sabre strace`"
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
        //   * the DBI arm, which returns `run_dbi(..)` and never reaches
        //     `verify()`, so it produces no verdict at all;
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
            Subcommand::Run(x) => x.main(global),
            Subcommand::Strace(x) => x.main(global),
            Subcommand::Record(x) => x.main(global),
            Subcommand::Replay(x) => x.main(global),
            Subcommand::LogDiff(x) => Ok(x.main(global)),
            Subcommand::Analyze(x) => x.main(global),
            Subcommand::Bisect(x) => x.main(global),
            Subcommand::InstructionMap(x) => x.main(global),
        }
    }
}

#[fbinit::main]
fn main() {
    if let Some(status) = liteinst_activation_probe() {
        status.raise_or_exit();
    }
    let Args {
        global,
        mut command,
    } = Args::parse();

    command
        .main(&global)
        .unwrap_or_else(|err| {
            display_error(err);
            ExitStatus::Exited(1)
        })
        .raise_or_exit();
}

fn display_error(error: Error) {
    let mut chain = error.chain();

    if let Some(error) = chain.next() {
        eprintln!("{}: {}", "Error".red().bold(), error);
    }

    for cause in chain {
        eprintln!("     {} {}", ">".dimmed().bold(), cause);
    }
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;
    use clap::Parser;

    use super::Args;
    use super::Subcommand;

    #[test]
    fn clap_configuration_is_valid() {
        Args::command().debug_assert();
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

    /// TOP-LEVEL EXIT 2 -- the DBI arm of `RunOpts::main` RETURNS `run_dbi(..)`
    /// and never reaches `verify()`, so a `--verify --verify-json` DBI run
    /// cannot produce a verdict at all.
    ///
    /// Asserted STRUCTURALLY rather than by executing the arm. `run_dbi` takes
    /// `verify: bool` but no verdict-artifact path, in BOTH cfg arms
    /// (`backends.rs`), so the bypass is a property of the signature: there is
    /// no argument through which it could publish one. An earlier version of
    /// this test drove the arm for real; with the stamp removed it did not fail
    /// but HUNG (blocking in `reserve_output_stdin_snapshot` on the harness
    /// stdin), which is a no-result wearing another outcome -- precisely the
    /// bug class this file exists to prevent, and it would wedge a CI shard for
    /// its whole timeout. A test that cannot hang is worth more here than one
    /// that exercises the launch.
    #[test]
    fn dbi_arm_has_no_channel_to_publish_a_verdict() {
        let source = include_str!("backends.rs");
        let signatures: Vec<&str> = source
            .match_indices("fn run_dbi(")
            .map(|(i, _)| {
                let rest = &source[i..];
                &rest[..rest
                    .find(") -> Result<ExitStatus, Error>")
                    .expect("run_dbi signature")]
            })
            .collect();
        assert_eq!(signatures.len(), 2, "expected both cfg arms of run_dbi");
        for signature in signatures {
            assert!(
                signature.contains("verify"),
                "run_dbi should still receive the verify flag"
            );
            assert!(
                !signature.contains("verify_json") && !signature.contains("json"),
                "run_dbi gained a verdict-artifact parameter; the DBI arm can now publish a \
                 verdict, so it must clear or publish the receipt rather than relying solely on \
                 the top-level pending stamp:\n{signature}"
            );
        }
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
