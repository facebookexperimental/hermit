/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::ffi::OsStr;
use std::ffi::OsString;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;
use std::sync::OnceLock;

// The typed skid-overshoot classifier and retry loop that `hermit_output`
// applies. Not every including binary calls them.
#[allow(dead_code)]
#[path = "skid_overshoot.rs"]
pub mod skid_overshoot;

// The typed x86-64-v3 host-fact refusal classifier `hermit_output`
// applies, and the skip marker it records. Not every including
// binary calls them.
#[allow(dead_code)]
#[path = "host_fact_v3.rs"]
pub mod host_fact_v3;
#[allow(dead_code)]
#[path = "test_skip.rs"]
pub mod test_skip;

/// Attempts a Hermit run gets when Hermit refuses it for a late PMU interrupt
/// (exit 122, `cause=skid-overshoot`): the one shared skid budget
/// ([`detcore_model::SKID_RETRY_MAX_ATTEMPTS`]). A refusal measures the
/// host's interrupt latency under load, not the guest
/// (https://github.com/rrnewton/hermit/issues/1845).
#[allow(dead_code)]
pub const SKID_ATTEMPTS: u32 = detcore_model::SKID_RETRY_MAX_ATTEMPTS;

/// Run `command` (a direct or timeout-wrapped Hermit command), and run it
/// again, up to [`SKID_ATTEMPTS`] in all, only after an attempt whose output
/// is exactly a typed skid-overshoot refusal
/// ([`skid_overshoot::skid_overshoot_refusal`]). Every other outcome returns
/// at once, for the caller to check as a single run. Each retry prints a
/// `SKID-RETRY` line and is counted by `ci/run-nextest-counted.sh`.
#[allow(dead_code)]
pub fn hermit_output(label: &str, command: &mut Command) -> Output {
    hermit_output_attempts(label, SKID_ATTEMPTS, command)
}

/// [`hermit_output`] with a bound of `max_attempts` instead of
/// [`SKID_ATTEMPTS`], for a test whose per-test CPU budget cannot pay for
/// more attempts of its costliest run.
#[allow(dead_code)]
pub fn hermit_output_attempts(label: &str, max_attempts: u32, command: &mut Command) -> Output {
    pin_canonical_epoch(command);
    let output = skid_overshoot::retry_typed_skid_refusal(
        label,
        max_attempts,
        || {
            let output = command
                .output()
                .unwrap_or_else(|error| panic!("failed to start {label}: {command:?}: {error}"));
            (output, ())
        },
        &|_| {},
    )
    .1;
    abort_on_host_fact_v3_refusal(label, command, &output);
    output
}

std::thread_local! {
    /// How many suppress_v3_skip guards this test thread holds. The
    /// hook records nothing and never aborts while any is held: a
    /// caller that handles a v3 refusal itself (the v3-run policies
    /// in cli.rs, an unpinned auto-choice run) keeps its own rules.
    static V3_SKIP_SUPPRESSED: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Suppress the v3 counted-skip hook on this thread until dropped:
/// hermit runs it drives answer for a v3 refusal themselves.
#[must_use = "the guard suppresses only while held"]
#[allow(dead_code)]
pub struct V3SkipSuppressed(());

#[allow(dead_code)]
pub fn suppress_v3_skip() -> V3SkipSuppressed {
    V3_SKIP_SUPPRESSED.with(|held| held.set(held.get() + 1));
    V3SkipSuppressed(())
}

impl Drop for V3SkipSuppressed {
    fn drop(&mut self) {
        V3_SKIP_SUPPRESSED.with(|held| held.set(held.get() - 1));
    }
}

/// Whether the hook may act at all here: only under run_node.py,
/// which alone sets HERMIT_RUN_NODE_SKIP_ISSUES=1 for its cases (so
/// the open-source nextest producer never sees these markers), never
/// under a suppression guard, and never where the launch
/// environment forbids the skip (host_fact_v3::env_forbids_skip:
/// v3 required, or a refusal seam made the verdict synthetic).
/// That environment rule is the one rule both paths share; the
/// verdict binary applies it to its own (the test's) environment.
/// A variable's value for a launch: the command's own setting (or
/// removal) where it names one, else this process's. The seam tests
/// set the seam variable on the Hermit command, not the test
/// process, so the command is the first place to look.
fn command_env(cmd: Option<&Command>, key: &str) -> Option<String> {
    if let Some(cmd) = cmd
        && let Some((_, value)) = cmd.get_envs().find(|(name, _)| *name == key)
    {
        return value.and_then(OsStr::to_str).map(str::to_owned);
    }
    std::env::var(key).ok()
}

/// Whether the hook may act for `cmd` (for funnel callers that
/// decide their bead before the command is consumed by spawning).
#[allow(dead_code)]
pub fn v3_skip_enabled_for(cmd: &Command) -> bool {
    v3_skip_enabled(Some(cmd))
}

fn v3_skip_enabled(cmd: Option<&Command>) -> bool {
    std::env::var_os("HERMIT_RUN_NODE_SKIP_ISSUES").is_some_and(|v| v == "1")
        && !host_fact_v3::env_forbids_skip(|key| command_env(cmd, key))
        && V3_SKIP_SUPPRESSED.with(|held| held.get() == 0)
}

/// The bead a host-fact v3 refusal of `command` names, where the
/// launch requests x86-64-v3 at all (host_fact_v3::bead_for on the
/// command's facts): its explicit profile (flag, else the command's
/// or this process's HERMIT_CPU_PROFILE), whether the launch can
/// present v3 ([`admits_x86_64_v3`]), and, for an auto choice only,
/// whether its guest declares more than v2
/// ([`guest_needs_above_v2`]). `None` where the launch never
/// requests v3 — a SaBRe, DBT, LiteInst, namespace-only,
/// strace-only, no-virtualize-cpuid or gdbserver launch, an
/// explicit non-v3 profile, or a guest that declares no more than
/// v2 — so such a case is never skipped for v3.
#[allow(dead_code)]
pub fn v3_skip_bead(cmd: &Command) -> Option<&'static str> {
    let args = cmd.get_args().collect::<Vec<_>>();
    let end = args
        .iter()
        .position(|arg| *arg == "--")
        .unwrap_or(args.len());
    let options = &args[..end];
    let mut explicit: Option<String> = None;
    for (index, option) in options.iter().enumerate() {
        if let Some(profile) = option
            .to_str()
            .and_then(|o| o.strip_prefix("--cpu-profile="))
        {
            explicit = Some(profile.to_owned());
        } else if *option == "--cpu-profile"
            && let Some(profile) = options.get(index + 1).and_then(|v| v.to_str())
        {
            explicit = Some(profile.to_owned());
        }
    }
    if explicit.is_none() {
        let named = cmd
            .get_envs()
            .find(|(key, _)| *key == "HERMIT_CPU_PROFILE")
            .map(|(_, value)| value.and_then(OsStr::to_str).map(str::to_owned));
        explicit = match named {
            Some(profile) => profile,
            None => std::env::var("HERMIT_CPU_PROFILE").ok(),
        };
    }
    let admits = admits_x86_64_v3(cmd);
    let needs = explicit.is_none() && admits && guest_needs_above_v2(cmd);
    host_fact_v3::bead_for(explicit.as_deref(), admits, needs)
}

/// Abort this case at once if `output` is Hermit's host-fact
/// x86-64-v3 refusal of a launch that requested v3: record the
/// counted skip for its bead, then panic with the abort payload so
/// the refusal is the case's failure and nothing after it runs.
/// The runner counts the case `skipped` only where the marker and
/// this payload agree.
#[allow(dead_code)]
pub fn abort_on_host_fact_v3_refusal(label: &str, cmd: &Command, output: &Output) {
    if !v3_skip_enabled(Some(cmd)) {
        return;
    }
    let Some(bead) = v3_skip_bead(cmd) else {
        return;
    };
    abort_on_host_fact_v3_refusal_with_bead(label, bead, output);
}

/// [`abort_on_host_fact_v3_refusal`] for a harness that already
/// knows its guest declares more than x86-64-v2 at `program` (or
/// that no guest declaration applies, `None`) and whether its launch
/// can present v3, because its Hermit command is not a `Command`
/// this module can read (a shell script, a wrapper pipeline). An
/// explicit process-level `HERMIT_CPU_PROFILE` counts as in
/// [`v3_skip_bead`]: an explicit x86-64-v3 requests v3 without a
/// guest declaration, and any other explicit profile never does.
#[allow(dead_code)]
pub fn abort_on_host_fact_v3_refusal_for(
    label: &str,
    program: Option<&Path>,
    admits: bool,
    code: Option<i32>,
    stderr: &str,
) {
    if !v3_skip_enabled(None) {
        return;
    }
    let explicit = std::env::var("HERMIT_CPU_PROFILE").ok();
    let needs = program.is_some_and(needs_above_v2);
    if host_fact_v3::bead_for(explicit.as_deref(), admits, needs).is_none() {
        return;
    }
    let Some(refusal) = host_fact_v3::host_fact_refusal(code, stderr) else {
        return;
    };
    record_v3_skip_abort(label, host_fact_v3::SKIP_ISSUE_XSAVE, refusal);
}

/// [`abort_on_host_fact_v3_refusal`] with the bead its caller
/// already decided (`v3_skip_bead` on its own command, captured
/// before the command was consumed by spawning it).
#[allow(dead_code)]
pub fn abort_on_host_fact_v3_refusal_with_bead(label: &str, bead: &str, output: &Output) {
    if !v3_skip_enabled(None) {
        return;
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let Some(refusal) = host_fact_v3::host_fact_refusal(output.status.code(), &stderr) else {
        return;
    };
    record_v3_skip_abort(label, bead, refusal);
}

/// Record the counted skip for `bead` into `skip_file` (the
/// runner's sidecar where `None`): the marker half of the abort,
/// returning the payload the abort leaves by. Split from the abort
/// so a unit test can drive it with a private file and an explicit
/// enabled flag, with no process-environment change.
pub(crate) fn record_v3_skip(
    label: &str,
    bead: &str,
    refusal: &str,
    skip_file: Option<&Path>,
) -> String {
    let reason = host_fact_v3::skip_issue_reason(bead, refusal);
    match skip_file {
        Some(path) => test_skip::record_test_skip_to(Some(path), label, &reason),
        // The runner's sidecar, named by HERMIT_TEST_SKIP_FILE:
        // record_test_skip_to's None only prints, so resolve here.
        None => test_skip::record_test_skip(label, &reason),
    }
    host_fact_v3::skip_abort_payload(bead, refusal)
}

/// The abort itself, on arguments: where `enabled` is false it
/// returns having recorded nothing; where true it records the
/// marker into `skip_file`, prints the abort payload, and exits
/// with SKIP_ABORT_EXIT. Exiting, not panicking, is what makes the
/// abort uncatchable: no catch_unwind or sibling thread can
/// continue the case past it.
pub(crate) fn v3_skip_abort_core(
    label: &str,
    bead: &str,
    refusal: &str,
    enabled: bool,
    skip_file: Option<&Path>,
) {
    if !enabled {
        return;
    }
    let payload = record_v3_skip(label, bead, refusal, skip_file);
    // Straight to /dev/stderr, not eprintln!: libtest captures the
    // print macros' output per test and an exiting process never
    // flushes that capture, which would lose the payload the
    // runner requires beside the marker and this exit status.
    if let Ok(mut dev_stderr) = std::fs::OpenOptions::new().append(true).open("/dev/stderr") {
        use std::io::Write as _;
        let _ = writeln!(dev_stderr, "{payload}");
    } else {
        eprintln!("{payload}");
    }
    std::process::exit(host_fact_v3::SKIP_ABORT_EXIT);
}

fn record_v3_skip_abort(label: &str, bead: &str, refusal: &str) -> ! {
    v3_skip_abort_core(label, bead, refusal, true, None);
    unreachable!("an enabled abort exits the process")
}

/// [`hermit_output`] for a caller that makes each attempt itself, with state
/// of its own per attempt (a fresh scratch directory, a recording, a report
/// path): `attempt` runs Hermit and returns its output beside that state, and
/// the state of the attempt that is returned comes back with it.
#[allow(dead_code)]
pub fn hermit_output_with<T>(label: &str, attempt: impl FnMut() -> (Output, T)) -> (Output, T) {
    let (_, output, kept) =
        skid_overshoot::retry_typed_skid_refusal(label, SKID_ATTEMPTS, attempt, &|_| {});
    (output, kept)
}

/// Resolve the Hermit executable selected for this test process.
///
/// Validation exports `HERMIT_BIN` only after verifying the content-addressed
/// artifact. Ordinary `cargo test` invocations retain Cargo's compile-time
/// binary path as their fallback.
pub fn hermit_binary() -> &'static Path {
    static PATH: OnceLock<PathBuf> = OnceLock::new();
    PATH.get_or_init(|| {
        std::env::var_os("HERMIT_BIN")
            .filter(|path| !path.is_empty())
            .or_else(|| option_env!("CARGO_BIN_EXE_hermit").map(OsString::from))
            .map(PathBuf::from)
            .expect("HERMIT_BIN and CARGO_BIN_EXE_hermit are both unavailable")
    })
    .as_path()
}

// Each integration target includes this module independently; identity-only
// consumers use `hermit_binary` without constructing a guest command.
#[allow(dead_code)]
const ISOLATED_WORKDIR_ENV: &str = "HERMIT_E2E_EMPTY_WORKDIR";
#[allow(dead_code)]
const HERMETIC_TEST_WORKDIR: &str = "/test";

#[allow(dead_code)]
pub fn guest_args_for<I, S>(args: I, requested: Option<&OsStr>) -> Result<Vec<OsString>, String>
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    let mut args = args.into_iter().map(Into::into).collect::<Vec<_>>();
    let Some(requested) = requested else {
        return Ok(args);
    };
    if requested != OsStr::new(HERMETIC_TEST_WORKDIR) {
        return Err(format!(
            "{ISOLATED_WORKDIR_ENV} must be {HERMETIC_TEST_WORKDIR}, got {requested:?}"
        ));
    }
    let Some(separator) = args.iter().position(|arg| arg == OsStr::new("--")) else {
        return Ok(args);
    };
    let options = &args[..separator];
    let uses_outer_mount = options
        .iter()
        .any(|arg| arg == OsStr::new("--no-namespace"))
        || options.iter().any(|arg| arg == OsStr::new("--backend=dbt"))
        || options
            .windows(2)
            .any(|pair| pair[0] == OsStr::new("--backend") && pair[1] == OsStr::new("dbt"));
    let explicit_base_env = options.iter().enumerate().find_map(|(index, arg)| {
        arg.to_str()
            .and_then(|arg| arg.strip_prefix("--base-env="))
            .map(OsString::from)
            .or_else(|| {
                (arg == OsStr::new("--base-env"))
                    .then(|| options.get(index + 1).cloned())
                    .flatten()
            })
    });
    if explicit_base_env
        .as_deref()
        .is_some_and(|base_env| base_env != OsStr::new("minimal"))
    {
        return Err(format!(
            "{ISOLATED_WORKDIR_ENV} requires --base-env=minimal, got {explicit_base_env:?}"
        ));
    }
    let has_test_mount = options.iter().enumerate().any(|(index, arg)| {
        let mount = arg
            .to_str()
            .and_then(|arg| arg.strip_prefix("--mount="))
            .or_else(|| {
                (arg == OsStr::new("--mount"))
                    .then(|| options.get(index + 1).and_then(|value| value.to_str()))
                    .flatten()
            });
        mount.is_some_and(|mount| mount.split(',').any(|field| field == "target=/test"))
    });
    let first_mount = options.iter().position(|arg| {
        arg == OsStr::new("--mount") || arg.to_str().is_some_and(|arg| arg.starts_with("--mount="))
    });
    let explicit_workdir = options.iter().enumerate().find_map(|(index, arg)| {
        arg.to_str()
            .and_then(|arg| arg.strip_prefix("--workdir="))
            .map(OsString::from)
            .or_else(|| {
                (arg == OsStr::new("--workdir"))
                    .then(|| options.get(index + 1).cloned())
                    .flatten()
            })
    });
    if explicit_workdir
        .as_deref()
        .is_some_and(|workdir| workdir != OsStr::new(HERMETIC_TEST_WORKDIR))
    {
        return Err(format!(
            "{ISOLATED_WORKDIR_ENV} requires --workdir={HERMETIC_TEST_WORKDIR}, got {explicit_workdir:?}"
        ));
    }
    let mut execution_root = Vec::with_capacity(3);
    if explicit_base_env.is_none() {
        execution_root.push(OsString::from("--base-env=minimal"));
    }
    let needs_test_mount = !uses_outer_mount && !has_test_mount;
    if needs_test_mount && first_mount.is_none() {
        execution_root.push(OsString::from("--mount=type=tmpfs,target=/test"));
    }
    if explicit_workdir.is_none() {
        execution_root.push(OsString::from("--workdir=/test"));
    }
    args.splice(separator..separator, execution_root);
    if needs_test_mount && let Some(index) = first_mount {
        // Mounts are applied in argument order. Install the parent first so a
        // later explicit child fixture remains visible inside the fresh root.
        args.insert(index, OsString::from("--mount=type=tmpfs,target=/test"));
    }
    Ok(args)
}

/// Give a Hermit command the canonical test epoch
/// ([`detcore_model::config::CANONICAL_TEST_EPOCH`]) through `HERMIT_EPOCH`,
/// so its runs do not depend on when they ran: `hermit run --strict` refuses
/// an epoch drawn from the host clock. A command that sets or removes
/// `HERMIT_EPOCH` itself, or a test process that inherited one (Buck gives
/// every cell of a run one), keeps it.
#[allow(dead_code)]
pub fn pin_canonical_epoch(command: &mut Command) {
    if std::env::var_os("HERMIT_EPOCH").is_none()
        && !command.get_envs().any(|(key, _)| key == "HERMIT_EPOCH")
    {
        command.env("HERMIT_EPOCH", detcore_model::config::CANONICAL_TEST_EPOCH);
    }
    isolate_cache_home(command);
    pin_test_cpu_profile(command);
}

/// Give a Hermit command `HERMIT_CPU_PROFILE=x86-64-v2`, unless the command
/// sets or removes `HERMIT_CPU_PROFILE` itself, the test process inherited
/// one, or the command runs a guest that declares it needs more than
/// x86-64-v2 in a launch where Hermit can present x86-64-v3
/// ([`guest_needs_above_v2`], [`admits_x86_64_v3`]). With no profile named,
/// auto presents x86-64-v3 where the run and host qualify, which would make
/// each test process qualify the host (seconds) and move the baselines that
/// count retired branches or size signal frames. A guest that needs more
/// cannot run under the pin: Hermit refuses an image that declares it, and
/// glibc a C library that declares it. Where Hermit can present x86-64-v3,
/// such a guest's command is left to auto, which presents the x86-64-v3 the
/// guest declares where the host qualifies and refuses the run by name where
/// it does not. Everywhere else Hermit refuses x86-64-v3 by name: on any other
/// backend, where Detcore does not answer the guest's CPUID, for a gdbserver,
/// and in `hermit analyze`. There auto would only trade the x86-64-v2 refusal
/// for that one, and the pin stays. A `--cpu-profile` flag still wins over
/// the variable, so a test of auto names `--cpu-profile auto`, or removes the
/// variable, and a test that wants x86-64-v2 for such a guest names
/// `--cpu-profile x86-64-v2`.
///
/// A command pinned before its guest was added (a builder that returns it
/// without one, or a caller that appends the guest to `hermit_command`'s
/// result) gets x86-64-v2 as before, marked [`PINNED_BEFORE_ITS_GUEST`]. The
/// next pin, which the launch helpers (`hermit_output`,
/// `configure_guest_execution`) apply, decides again from the guest, unless
/// the test changed the variable in between.
pub fn pin_test_cpu_profile(command: &mut Command) {
    if std::env::var_os("HERMIT_CPU_PROFILE").is_some() {
        return;
    }
    let named = command
        .get_envs()
        .find(|(key, _)| *key == "HERMIT_CPU_PROFILE")
        .map(|(_, value)| value.map(OsStr::to_os_string));
    match named {
        None => match guest_program(command) {
            Some(program) => {
                if !(admits_x86_64_v3(command) && needs_above_v2(&program)) {
                    command.env("HERMIT_CPU_PROFILE", "x86-64-v2");
                }
            }
            None => {
                command
                    .env("HERMIT_CPU_PROFILE", "x86-64-v2")
                    .env_remove(PINNED_BEFORE_ITS_GUEST);
            }
        },
        Some(Some(profile))
            if profile == "x86-64-v2"
                && command
                    .get_envs()
                    .any(|(key, value)| key == PINNED_BEFORE_ITS_GUEST && value.is_none())
                && admits_x86_64_v3(command)
                && guest_needs_above_v2(command) =>
        {
            command.env_remove("HERMIT_CPU_PROFILE");
        }
        // Set or removed by the test, or pinned for a guest already read.
        Some(_) => {}
    }
}

/// Give `command` the `HERMIT_CPU_PROFILE` its guest would see under `first`,
/// the first run of a test that compares later runs with it: the value
/// `first` names or removes, or else the one this process passes on. Hermit
/// passes the variable on into the guest's environment, and
/// [`pin_test_cpu_profile`] decides each command from its own guest, while
/// a rerun from a run config, a manifest or a recording names none: alone,
/// the pin would name x86-64-v2 for the rerun of a first run it left to auto.
/// A later pin keeps the copy, unless the copy is x86-64-v2 on a command
/// pinned before its guest was added, which that pin decides again from the
/// guest.
#[allow(dead_code)]
pub fn pin_cpu_profile_like(command: &mut Command, first: &Command) {
    let profile = match first
        .get_envs()
        .find(|(key, _)| *key == "HERMIT_CPU_PROFILE")
    {
        Some((_, value)) => value.map(OsStr::to_os_string),
        None => std::env::var_os("HERMIT_CPU_PROFILE"),
    };
    match profile {
        Some(profile) => command.env("HERMIT_CPU_PROFILE", profile),
        None => command.env_remove("HERMIT_CPU_PROFILE"),
    };
}

/// Whether Hermit can present x86-64-v3 to the guest of the Hermit
/// `command`. It does so on the ptrace backend alone, where Detcore answers
/// the guest's CPUID and no gdbserver writes the guest's registers
/// (`hermit::cpu_profile_admission::check_request`), in a launch that carries
/// the host's qualification, which a `hermit analyze` run does not ("the
/// configuration carries no qualified host facts"). So before its first `--`
/// the command names no other `--backend` and none of [`NO_VIRTUAL_CPUID`]
/// and [`SERVES_A_GDBSERVER`], and its subcommand is not `analyze`
/// ([`hermit_subcommand`]).
fn admits_x86_64_v3(command: &Command) -> bool {
    if hermit_subcommand(command).is_some_and(|subcommand| subcommand == "analyze") {
        return false;
    }
    let args = command.get_args().collect::<Vec<_>>();
    let end = args
        .iter()
        .position(|arg| *arg == "--")
        .unwrap_or(args.len());
    let options = &args[..end];
    let mut backend = None;
    for (index, option) in options.iter().enumerate() {
        if NO_VIRTUAL_CPUID.iter().any(|flag| *option == *flag)
            || SERVES_A_GDBSERVER
                .iter()
                .any(|flag| names_flag(option, flag))
        {
            return false;
        }
        if *option == "--backend" {
            backend = options.get(index + 1).copied();
        } else if let Some(name) = option.to_str().and_then(|o| o.strip_prefix("--backend=")) {
            backend = Some(OsStr::new(name));
        }
    }
    backend.is_none_or(|backend| backend == "ptrace")
}

/// The `hermit run` flags with which Detcore does not answer the guest's
/// CPUID. Hermit refuses x86-64-v3 under each of them by name, because the
/// profile is a virtual CPUID table, while the pin's x86-64-v2 presents
/// nothing and is admitted: Detcore checks a guest's declared ISA level only
/// where it virtualizes CPUID. So a command naming one keeps the pin.
const NO_VIRTUAL_CPUID: &[&str] = &[
    // Runs the guest without Detcore on any backend.
    "--namespace-only",
    // `--namespace-only`'s alias.
    "--lite",
    // Shorthand that includes `--no-virtualize-cpuid` (`lower_strace_only`).
    "--strace-only",
    // Detcore's own switch (`Config::virtualize_cpuid`).
    "--no-virtualize-cpuid",
];

/// The flags with which a Hermit command serves its guest to gdb, each alone
/// or with its value after `=` ([`names_flag`]); a value given as the next
/// argument follows the flag alone. Hermit refuses x86-64-v3 for a gdbserver
/// by name ("--gdbserver writes the guest's FP registers, and the kernel's
/// regset write forces XSAVE header bits the qualification binds"), and
/// admits the pin's x86-64-v2. So a command naming one keeps the pin.
/// `--gdbserver-port` is not among them: it only chooses the port of a
/// gdbserver one of these asks for.
const SERVES_A_GDBSERVER: &[&str] = &[
    // `hermit run` and `hermit replay` (`Config::gdbserver`).
    "--gdbserver",
    // `hermit run`: sets `Config::gdbserver` for the run it supervises.
    "--dap-listen",
    // `hermit record start`: replays the recording with a gdbserver, which
    // `hermit::replay` admits as a gdbserver's replay.
    "--verify-with-gdbex",
];

/// Whether the argument `option` is the flag `flag`, alone or with its value
/// after `=`.
fn names_flag(option: &OsStr, flag: &str) -> bool {
    option
        .to_str()
        .and_then(|option| option.strip_prefix(flag))
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('='))
}

/// The subcommand of the Hermit `command`: the first argument after the
/// Hermit binary that is not one of Hermit's global options or the value of
/// one. The binary is the command's program, or, after a wrapper such as
/// `timeout`, the first argument whose file name is `hermit`. `None` where
/// there is no such argument before the first `--`.
fn hermit_subcommand(command: &Command) -> Option<&OsStr> {
    let is_hermit = |path: &OsStr| Path::new(path).file_name() == Some(OsStr::new("hermit"));
    let mut args = command.get_args();
    if !is_hermit(command.get_program()) {
        args.find(|&arg| is_hermit(arg))?;
    }
    while let Some(arg) = args.next() {
        if arg == "--" {
            return None;
        }
        if GLOBAL_OPTIONS_WITH_A_VALUE
            .iter()
            .any(|option| arg == *option)
        {
            args.next();
        } else if !arg.as_encoded_bytes().starts_with(b"-") {
            return Some(arg);
        }
    }
    None
}

/// Hermit's global options that take a value, spelled with the value as the
/// next argument (`GlobalOpts` in `bin/hermit/global_opts.rs`). With `=`, the
/// value is part of the option's own argument.
const GLOBAL_OPTIONS_WITH_A_VALUE: &[&str] = &[
    "-l",
    "--log",
    "--log-file",
    "--max-log-bytes",
    "--log-ring",
    "--log-ring-file",
    "--backend",
];

/// The variable whose removal marks a command that [`pin_test_cpu_profile`]
/// pinned before it had a guest. Nothing sets it, so removing it changes no
/// environment Hermit or its guest sees, while `Command::get_envs` still
/// lists the removal for a later pin to read.
const PINNED_BEFORE_ITS_GUEST: &str = "HERMIT_TEST_CPU_PROFILE_PINNED_BEFORE_ITS_GUEST";

/// Whether the guest of the Hermit `command` declares that it needs more than
/// x86-64-v2, in the declarations `--cpu-profile auto` reads
/// (`hermit::cpu_profile_admission::resolve_auto` and `libc_needs_v3`): those
/// of the image the kernel would exec first for it, through a bounded chain
/// of scripts; of that image's program interpreter; and of glibc's C library
/// beside a glibc interpreter, where CentOS 10's glibc declares x86-64-v3
/// alone. The guest is the first argument after a `--` that is not an option
/// ([`guest_program`]). Paths are read on the host, as the test spells them.
/// A guest this cannot find, or whose declarations it cannot read, declares
/// nothing here.
pub fn guest_needs_above_v2(command: &Command) -> bool {
    guest_program(command).is_some_and(|program| needs_above_v2(&program))
}

/// The guest program of the Hermit `command`: the first argument after a
/// `--` that is not itself an option. In a Hermit command, direct or after a
/// wrapper such as `timeout`, the first `--` ends Hermit's own arguments;
/// `hermit analyze` puts its run's options after a first `--` and the
/// program after a second. A name with no directory is found on the
/// command's `PATH`, or else this process's; a relative path is taken from
/// the command's directory.
fn guest_program(command: &Command) -> Option<PathBuf> {
    let args = command.get_args().collect::<Vec<_>>();
    let program = args
        .iter()
        .enumerate()
        .filter(|(_, arg)| **arg == "--")
        .find_map(|(separator, _)| {
            args.get(separator + 1)
                .filter(|next| !next.as_encoded_bytes().starts_with(b"-"))
        })
        .map(Path::new)?;
    if program
        .parent()
        .is_some_and(|parent| !parent.as_os_str().is_empty())
    {
        return Some(match command.get_current_dir() {
            Some(directory) => directory.join(program),
            None => program.to_path_buf(),
        });
    }
    let path = match command.get_envs().find(|(key, _)| *key == "PATH") {
        Some((_, value)) => value?.to_os_string(),
        None => std::env::var_os("PATH")?,
    };
    std::env::split_paths(&path)
        .map(|directory| directory.join(program))
        .find(|candidate| candidate.is_file())
}

/// Whether the guest at host path `program` declares that it needs more than
/// x86-64-v2 ([`guest_needs_above_v2`]).
fn needs_above_v2(program: &Path) -> bool {
    use detcore::exec_record::IsaNeeds;
    let above_v2 = |path: &Path| {
        matches!(
            IsaNeeds::read_path_strict(path),
            Some(IsaNeeds::Level(level)) if level > 2
        )
    };
    let Some(image) = exec_image(program) else {
        return false;
    };
    let interpreter = detcore::exec_record::program_interpreter(&image)
        .ok()
        .flatten();
    above_v2(&image)
        || interpreter.as_deref().is_some_and(|interpreter| {
            above_v2(interpreter) || glibc_c_library(interpreter).is_some_and(|c| above_v2(&c))
        })
}

/// The image the kernel would exec first for `program`: `program` itself, or
/// the interpreter of each script in turn (Detcore's `script_interpreter`),
/// through at most four scripts. `None` for a script the kernel would not
/// run, an unreadable one, and a longer chain.
fn exec_image(program: &Path) -> Option<PathBuf> {
    use detcore::exec_record::Script;
    let mut image = program.to_path_buf();
    for _ in 0..=4 {
        match detcore::exec_record::script_interpreter(&image) {
            Script::Not => return Some(image),
            Script::Interpreter(interpreter) => image = interpreter,
            Script::Unread | Script::Declined => return None,
        }
    }
    None
}

/// glibc's C library for the program interpreter `interpreter`, where it is
/// glibc's (`ld-linux-x86-64.so.2`): the regular file `libc.so.6` in the
/// directory the interpreter's path leads to, as `libc_needs_v3` reads it.
fn glibc_c_library(interpreter: &Path) -> Option<PathBuf> {
    if interpreter.file_name()? != "ld-linux-x86-64.so.2" {
        return None;
    }
    let library = std::fs::canonicalize(interpreter)
        .ok()?
        .parent()?
        .join("libc.so.6");
    std::fs::metadata(&library)
        .ok()?
        .is_file()
        .then_some(library)
}

/// Point a Hermit command's `XDG_CACHE_HOME` at a directory of this test
/// process's own, unless the test set one itself, so that Hermit's host
/// qualification cache (`~/.cache/hermit/`) never carries a verdict from one
/// test, or from the developer's own runs, into another test
/// (<https://github.com/rrnewton/hermit/issues/4515>). Nextest runs each test
/// in its own process, so each test starts with an empty cache.
pub fn isolate_cache_home(command: &mut Command) {
    static CACHE_HOME: OnceLock<PathBuf> = OnceLock::new();
    if command.get_envs().any(|(key, _)| key == "XDG_CACHE_HOME") {
        return;
    }
    let home = CACHE_HOME.get_or_init(|| {
        tempfile::Builder::new()
            .prefix("xdg-cache-")
            .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
            .expect("create this test process's cache directory")
            .keep()
    });
    command.env("XDG_CACHE_HOME", home);
}

/// Insert the validate v3 guest arguments into a direct or timeout-wrapped
/// Hermit command. Call this before configuring standard I/O, process-group or
/// user/group identity, pre-exec callbacks, or clearing the environment. The
/// marked path rebuilds the command; its getters expose only the program,
/// arguments, current directory and explicit environment entries.
#[allow(dead_code)]
pub fn configure_guest_execution(command: &mut Command) {
    pin_canonical_epoch(command);
    let requested = std::env::var_os(ISOLATED_WORKDIR_ENV);
    configure_guest_execution_for(command, requested.as_deref())
        .unwrap_or_else(|error| panic!("PATH-CONTRACT: {error}"));
}

#[allow(dead_code)]
pub fn configure_guest_execution_for(
    command: &mut Command,
    requested: Option<&OsStr>,
) -> Result<(), String> {
    if requested.is_none() {
        return Ok(());
    }

    let program = command.get_program().to_os_string();
    let args = command
        .get_args()
        .map(OsStr::to_os_string)
        .collect::<Vec<_>>();
    let hermit = hermit_binary().as_os_str();
    let hermit_args_start = if program == hermit {
        0
    } else {
        let Some(index) = args.iter().position(|arg| arg == hermit) else {
            return Ok(());
        };
        index + 1
    };
    let adjusted = guest_args_for(args[hermit_args_start..].iter().cloned(), requested)?;
    let prefix = args[..hermit_args_start].to_vec();
    let current_dir = command.get_current_dir().map(Path::to_path_buf);
    let env = command
        .get_envs()
        .map(|(key, value)| (key.to_os_string(), value.map(OsStr::to_os_string)))
        .collect::<Vec<_>>();

    let mut rebuilt = Command::new(program);
    rebuilt.args(prefix).args(adjusted);
    if let Some(current_dir) = current_dir {
        rebuilt.current_dir(current_dir);
    }
    for (key, value) in env {
        if let Some(value) = value {
            rebuilt.env(key, value);
        } else {
            rebuilt.env_remove(key);
        }
    }
    *command = rebuilt;
    Ok(())
}

/// The preemption record in the recording at `path`, as the JSON it
/// serializes to alone (`per_thread`, `global`, `epoch`, `pipe_wake_latency`,
/// ...).
///
/// A recording wraps its record in the `rcb_history` envelope that states the
/// writer's RCB clock identity, so the record is not the file's top level.
/// This reads the file through Detcore's own decoder and requires the stated
/// identity, so a file that is not such a recording fails here rather than as
/// a missing field.
///
/// For inspection only: the returned JSON omits the identity (the record's
/// identity field is not serialized), so it must never be written back as a
/// history.
#[allow(dead_code)]
pub fn recorded_preemption_json(path: &Path) -> serde_json::Value {
    let recording = detcore::preemptions::boundary_recording::Recording::read(path)
        .unwrap_or_else(|error| panic!("{} is not a recording: {error:#}", path.display()));
    assert!(
        recording.identity().is_some(),
        "{} states no RCB coordinate identity",
        path.display()
    );
    let record = recording
        .into_legacy("a test reading a preemption record")
        .unwrap_or_else(|error| panic!("{}: {error:#}", path.display()));
    serde_json::to_value(&record).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

/// The preemption record inside a recording's parsed JSON, for a test
/// editing a recording it then writes back: through the `rcb_history`
/// envelope and the `slice_boundary_recording` envelope when present, so
/// the edited file keeps the identity the recording stated.
#[allow(dead_code)]
pub fn recorded_preemption_inner_mut(record: &mut serde_json::Value) -> &mut serde_json::Value {
    if record.get("rcb_history").is_some() {
        let inner = record["rcb_history"]
            .get_mut("recording")
            .expect("an enveloped recording carries its record");
        return recorded_preemption_inner_mut(inner);
    }
    if record.get("slice_boundary_recording").is_some() {
        return &mut record["slice_boundary_recording"]["preemptions"];
    }
    record
}
