/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// Treat all Clippy warnings as errors.
#![doc = include_str!("../README.md")]
#![deny(clippy::all)]
#![allow(clippy::uninlined_format_args)]

mod backend_stats;
pub mod build_info;
pub mod canonical_verdict;
mod chroot;
mod consts;
mod desync;
pub mod logdiff_report;
// TODO-HUMAN-REVIEW(PR-594): Review the public e9patch preprocessing API.
pub mod e9patch;
mod error;
mod event;
mod event_stream;
mod fd;
pub mod happens_before;
pub mod host_input_change;
mod id;
#[cfg(feature = "liteinst")]
mod in_guest_exits;
pub mod instruction_map;
mod interp;
pub mod liteinst_bootstrap;
pub mod liteinst_record;
mod metadata;
/// Diagnostic writes that never wait for a reader; they live in detcore, whose
/// own stderr writer reads the same flag.
pub use detcore::nonwaiting_write;
pub mod physical_exit_watch;
pub mod proc_mount;
mod ptrace_completion;
pub mod run_evidence;

pub use canonical_verdict::Verdict;

/// Whether record/replay verification hashes syscall output buffers, and
/// therefore whether its log comparison can see buffer CONTENT at all.
///
/// ONE CONSTANT BECAUSE TWO INDEPENDENT SWITCHES HAD TO AGREE AND SILENTLY DID
/// NOT MATTER. Record/replay carried this decision twice: `metadata.rs`'s
/// `record_or_replay_config` decided whether the recorder EMITS the hash, and
/// `record_start.rs`'s `ComparisonOptions` decided whether the comparator READS
/// it. Both were hard-coded `false`, neither was reachable from the command
/// line, and nothing related them. Setting either alone achieves nothing: hash
/// without compare is dead weight, compare without hash finds nothing. Naming
/// the decision once makes the pair impossible to desynchronize, and makes
/// changing it a one-line edit in a place a reader can find.
pub const RECORD_REPLAY_HASHES_IO_BUFFERS: bool = true;

/// Whether record/replay virtualizes time. FALSE, DELIBERATELY, AND THAT IS WHY
/// A GREEN RECORD/REPLAY VERDICT IS NOT A DETERMINISM RESULT.
///
/// Named for the same reason as [`RECORD_REPLAY_HASHES_IO_BUFFERS`] directly
/// above: the decision was about to be carried twice. `metadata.rs`'s
/// `record_or_replay_config` sets the config the recorder and replayer run
/// under, and `record_start.rs`'s `ComparisonOptions` now discloses that setting
/// in the verdict. If those two disagreed, the report would describe a time
/// policy the run did not use — which is precisely the class of defect the
/// disclosure exists to close, reintroduced one layer up.
///
/// `hermit run --verify` reads its value from the live `det_config` instead,
/// because `--no-virtualize-time` makes it a genuine runtime choice there. Only
/// the record/replay path is a fixed decision, so only it gets a constant.
pub const RECORD_REPLAY_VIRTUALIZES_TIME: bool = false;

/// Exit status for a completed comparison that found a divergence.
///
/// This is the conventional `diff` result: zero means equal and one means
/// different. The verification JSON distinguishes it from a guest that exits
/// one of its own accord.
pub const HERMIT_VERIFICATION_DIVERGENCE_EXIT: i32 = 1;

/// Exit status for a failure of HERMIT ITSELF, as distinct from the guest's.
///
/// ⚠️ EVERY CLI ERROR USED TO BE `Exited(1)`, WHICH IS THE SINGLE MOST COMMON
/// GUEST FAILURE CODE. A tracer panic and a guest that exited 1 of its own
/// accord were therefore INDISTINGUISHABLE from `$?` alone — and every harness,
/// gate and script on this project decides pass/fail from exactly that value.
/// The information was not missing, only discarded: reverie's container reports
/// the child's real status as a typed `ExitStatus`, and hermit collapsed it to
/// prose and then to `1`.
///
/// WHY THIS DOES NOT BREAK THE "hermit's exit IS the guest's exit" CONTRACT.
/// That contract describes the case where the guest RAN AND EXITED, and it is
/// untouched: a guest status still flows through `raise_or_exit` unchanged, and
/// only the `Err` arm — where there is NO guest exit code, because hermit failed
/// before or instead of producing one — uses this value. The contract did not
/// cover that case; it does now.
///
/// WHY 125. It is the established convention for "the wrapper tool itself
/// failed" — GNU `env`, `chroot` and `timeout` all reserve 125 for exactly this,
/// leaving 126/127 for exec-level problems. It is not a value hermit emits
/// anywhere else.
///
/// ⚠️ THIS REDUCES A COLLISION, IT DOES NOT REMOVE ONE. A guest is free to exit
/// 125 deliberately, and every value in 0..=255 is a legal guest status, so no
/// code can be reserved outright. What it removes is the GUARANTEED collision
/// with the most common failure code. For an unambiguous signal, read the
/// `HERMIT_TASK_PANIC` marker on stderr or the verification JSON; this makes the
/// cheap check — `$?` — stop actively lying.
///
/// ⚠️ IT LIVES IN THE LIBRARY, NOT IN `main.rs`, AND THAT PLACEMENT IS THE FIX
/// FOR HOW IT WENT STALE. It was a private `const` inside the binary, so
/// `tests/cli.rs` — the only thing that asserts on it — could not name it and
/// wrote the number out by hand instead. When hermit#2558 moved the value from
/// `1` to `125`, the definition moved and the sixteen copies did not: eight cli
/// tests went red on main and stayed red, each failing an assertion about the
/// exit code rather than about the behaviour it exists to check. Same shape as
/// [`RECORD_REPLAY_HASHES_IO_BUFFERS`] above — a decision carried in two places
/// that had to agree and had no way to. Exported here, the tests move with it.
///
/// ⚠️ DO NOT FLIP EVERY `Some(1)` YOU FIND. `tests/stress_suite.rs`
/// asserts `Some(1)` as THE GUEST'S OWN SIGNAL, not as a stale copy of this
/// constant: there `Some(1) => GuestOutcome::Exposed`, and
/// `status.code() == Some(1)` IS the `exposes_bug` predicate. It is
/// pattern-identical to the tests that DID go stale, and it is the OPPOSITE
/// case -- the assertion is CORRECT and must not move with this constant.
///
/// ⚠️ ITS FAILURE MODE IS SILENT SUCCESS. Changing it to 125 would not
/// turn the stress suite red; it would make `exposes_bug` NEVER FIRE, so the
/// suite would report no bug exposed forever and pass while blind. A red is
/// recoverable; a green that cannot fail is not.
///
/// REVIEWER RULE: a change to `tests/stress_suite.rs` inside an exit-code head
/// SHOULD BE REFUSED ON SIGHT AND QUESTIONED.
pub const HERMIT_INTERNAL_FAILURE_EXIT: i32 = 125;

/// A deadline hermit itself was asked to enforce expired: **124**.
///
/// The status the container init exits with when a hermit-owned bound ends the
/// run -- `run --timeout`'s SIGALRM fallback here, and `record --record-timeout`'s
/// handler in `record_start.rs`, which has always used this value. 124 is the
/// established code for "a deadline fired" across GNU `timeout`, `safehermit`'s
/// wall bound and both hermit spellings, so this names an existing convention
/// rather than claiming a new number.
///
/// ⚠️ IT MUST BE READ AT THE CONTAINER BOUNDARY OR IT IS SILENTLY REWRITTEN TO
/// 125. Found only by forcing the fallback to fire, 2026-08-26: the init exited
/// 124, `classify_container_result` had no arm for it, and it fell through to
/// `ContainerChildExit` -- so the run reported `exit 125` and
/// `class=container-child-exit`, "the child died with a status it did not
/// pick", for a deadline hermit chose to enforce. Identical in shape to the
/// refusal that arrives as 122 and needs its own arm for the same reason.
pub const HERMIT_DEADLINE_EXIT: i32 = 124;

/// Hermit's own log output exceeded `--max-log-bytes`: **123**.
///
/// The status hermit exits with when the byte budget on its tracing output
/// (stderr, or `--log-file`) is exhausted and it deliberately tears the run
/// down. 123 was already reserved below for "a LOG BYTE CAP fired" on behalf of
/// dev-hermit's `bin/safehermit`; this is the same event enforced one layer
/// lower, exactly as 124 is shared by hermit's `--timeout` and safehermit's wall
/// deadline. It is not 125 because the cap working is not hermit breaking, and
/// not 122 because no fail-closed determinism policy was involved: the remedy
/// is "lower the log level or raise the cap", not "change the program".
///
/// Like 124, it must be read at the container boundary
/// (`classify_container_result`) or it is rewritten to 125.
pub const HERMIT_LOG_CAP_EXIT: i32 = 123;

/// The guest program could not be found at all: **127**.
///
/// Exported for the same reason as [`HERMIT_INTERNAL_FAILURE_EXIT`] and with the
/// same history one step behind it. `GuestProgramFault::exit_code` wrote `127`
/// and `126` as bare literals, and `tests/cli.rs` had no way to name either, so
/// a test meaning "not found" and a test meaning "hermit refused" were asserted
/// with the same helper and the same number until the values diverged.
///
/// ⚠️ THIS IS NOT A SUBDIVISION OF HERMIT'S OWN FAILURE, AND THAT IS THE WHOLE
/// DISTINCTION. 125 says *hermit* broke; 127 and 126 say *the guest program* was
/// unusable and hermit correctly declined to start it. A caller that cannot tell
/// those apart cannot tell "fix my tooling" from "fix my command line".
pub const GUEST_PROGRAM_NOT_FOUND_EXIT: i32 = 127;

/// The guest program exists but cannot be executed as given: **126**.
///
/// See [`GUEST_PROGRAM_NOT_FOUND_EXIT`]. 127/126 are the GNU `env`/`chroot`/
/// `timeout` convention that 125 itself came from, so the three sit in one
/// scheme rather than being three unrelated numbers.
pub const GUEST_PROGRAM_NOT_EXECUTABLE_EXIT: i32 = 126;

// ⚠️ THE VALUES THEMSELVES ARE PINNED, NOT ONLY THEIR NAMES.
//
// Naming a constant makes every consumer agree with the definition; it does not
// make the definition right. Once `tests/cli.rs` asserts `Some(THE_CONSTANT)`
// everywhere, the whole suite is self-consistent and completely unanchored -- a
// one-character edit here would move all sixteen assertions with it and nothing
// would fail. That is the exact shape of the defect this scheme exists to close,
// displaced one level up, and it is why these are compile-time assertions rather
// than a comment: a deliberate edit "would be noticed" is the argument that
// already failed once, when hermit#2558 moved 1 -> 125 and sixteen copies went
// stale.
//
// WHY EACH VALUE AND NOT ANOTHER. GNU `env`, `chroot` and `timeout` reserve 125
// for "the wrapper itself failed", 127 for "command not found" and 126 for
// "found but not executable". Borrowing the whole scheme rather than one number
// is what keeps the three answers distinguishable from each other.
//
// ⚠️ THE EXIT-CODE ALLOCATION FOR THE `hermit` BINARY. ONE TABLE, HERE.
//
// Five separate exit-code defects were found in one evening and every one of
// them came from a value being chosen, copied or rewritten somewhere that could
// not see the others. This is the single place the space is allocated; anything
// that emits or rewrites a hermit exit status is a consumer OF this table and
// must not invent its own reading of it.
//
//   0          success.
//   1          HERMIT_VERIFICATION_DIVERGENCE_EXIT, matching `diff`; also a
//              legal guest status. Read the verification JSON to distinguish.
//   2 ..= 121  THE GUEST'S OWN STATUS, passed through untouched. (The range
//              stops at 121 because 122 is reserved below; a guest may still
//              CHOOSE any value, see the caveat at the end.)
//   122        HERMIT_POLICY_REFUSAL_EXIT -- hermit REFUSED the run under a
//              fail-closed policy. Hermit WORKED; the run was stopped on
//              purpose and the reason was printed. Distinct from 125 because
//              "hermit refused" and "hermit broke" demand opposite responses:
//              read the refusal and change your program or flags, versus file a
//              bug. Defined in `detcore-model` because `detcore` emits it and
//              this crate recognises it, and that is the only crate both
//              depend on.
//   123        HERMIT_LOG_CAP_EXIT -- a LOG BYTE CAP fired. Hermit's own
//              `--max-log-bytes` budget on its tracing output, and the value
//              reserved for dev-hermit's `bin/safehermit` log-cap kill (which,
//              as of 2026-09-26, still exits 125 -- see the 125 note below).
//              Hermit's spelling prints `HERMIT_LOG_CAP class=log-cap`.
//   124        DO NOT USE. GNU `timeout`'s deadline, and dev-hermit's
//              `bin/safehermit` WALL DEADLINE kill. `tests/cli.rs` asserts
//              `assert_ne!(code, Some(124))` on the awk-mincore probe.
//   125        HERMIT_INTERNAL_FAILURE_EXIT -- hermit itself failed, no guest
//              ran. NOT YET THE SOLE MEANING through dev-hermit's
//              `bin/safehermit`: that wrapper has exited 125 for its
//              log-byte-cap kill since it was created on 2026-08-20 and still
//              did on 2026-10-05, so a run through it produces one number for
//              two faults with opposite remedies (see "125 ⚠️ NOT FREE" below).
//              An earlier version of this table said safehermit had already
//              moved off 125; it never did. The intended fix is that SAFEHERMIT
//              MOVES -- to the 123 reserved for it above -- NOT HERMIT, because
//              that wrapper is the only layer that knows which happened (it
//              relays hermit's status otherwise) and because 125/126/127 is one
//              GNU scheme that cannot be broken up. Tracked as
//              `hermit_s_125_collides`.
//   126        GUEST_PROGRAM_NOT_EXECUTABLE_EXIT.
//   127        GUEST_PROGRAM_NOT_FOUND_EXIT -- the ABSOLUTE-PATH form. The
//              bare-name-on-guest-PATH form currently exits 125 instead, which
//              is an inconsistency tracked as `hermit_reports_a_missing`, not a
//              second meaning for 127.
//   128 + N    killed by signal N (shell convention). Hermit does not emit these
//              deliberately; a wrapper reporting a signal death must use them
//              rather than borrow a code that already means something.
//
// ⚠️ NO CODE IS EXCLUSIVELY HERMIT'S. Every value in 0..=255 is a legal guest
// status, so a guest may return 125 or 127 of its own accord and this table
// cannot stop it. The table removes GUARANTEED collisions, not possible ones.
// The only unambiguous discriminator is the `HERMIT_INTERNAL_FAILURE` marker on
// stderr, present exactly when hermit itself failed. Any consumer deciding on
// `$?` alone is guessing, and should say so.
//
// ⚠️ THE SAME ARGUMENT APPLIES TO 124, AND MORE SHARPLY, BECAUSE FIVE
// MECHANISMS PRODUCE IT. `hermit run --timeout`, its unwind fallback, `hermit
// record --record-timeout`, the `timeout(1)` wrapped around a manifest cell, and
// `safehermit`'s wall deadline all exit 124, and GNU `timeout` uses it for the
// same event. They demand different responses -- a slow guest, a wedged
// teardown inside hermit, a cell bound set too low, a run that escaped every
// inner bound -- so `$?` cannot route the reader to any of them.
//
// The discriminator is again the marker on stderr, and it is a CONTRACT rather
// than a diagnostic nicety: `class=run-timeout` for hermit's own bound (which
// also states the bound in seconds), `HERMIT_RUN_TIMEOUT_FALLBACK` for the
// unwind failing to complete, `safehermit: bound.wall=` for the cgroup reap.
// EXIT 124 WITH NO MARKER AT ALL MEANS NO INNER BOUND FIRED -- something outside
// hermit killed the run -- which is a configuration error and not a slow guest.
// The full picture, including which rung bounds which quantity and the
// strict-inequality invariant between them, is docs/TIMEOUT_LADDER.md.
//
// ⚠️ DO NOT CLAMP INTO THIS RANGE. `scripts/hermit-code-coverage.rs` used
// `clamp(1, 125)` and so rewrote 127 and 126 into 125 -- turning "your program is
// missing" into "hermit broke" and pointing the reader at the wrong project.
// Truncating a status does not lose information here, it MANUFACTURES a false
// attribution, because the ceiling is itself a meaning.
//
// ⚠️ WHAT EACH VALUE MUST NOT COLLIDE WITH -- AND 125 ALREADY DOES.
//
//   0    success. Never a failure code; see the nonzero pin below.
//   1    the commonest guest exit status. Sharing it is the collision
//        hermit#2558 introduced 125 to escape, so 125 must never drift back.
//   124  GNU `timeout`'s "deadline fired", and dev-hermit's `bin/safehermit`
//        uses it for its WALL DEADLINE kill. `tests/cli.rs` asserts
//        `assert_ne!(code, Some(124))` on the awk-mincore probe for that reason.
//   126  hermit's own GUEST_PROGRAM_NOT_EXECUTABLE_EXIT.
//   127  hermit's own GUEST_PROGRAM_NOT_FOUND_EXIT -- already in use for the
//        ABSOLUTE-PATH form of a missing guest program. Note the bare-name form
//        resolved against the guest PATH exits 125 instead, which is an
//        inconsistency filed separately, not a licence to reuse 127.
//
//   125  ⚠️ NOT FREE. dev-hermit's `bin/safehermit` exits 125 to mean "the LOG
//        BYTE CAP fired: safehermit killed the run through its cgroup"
//        (`demos/lib/demo_common.py`'s SAFEHERMIT_EXIT_REASON). Any hermit run
//        launched through that wrapper -- which is how the demos and the repeat
//        harness run it -- can therefore produce 125 for two unrelated reasons:
//        hermit refused, or the wrapper killed it for log volume. They demand
//        opposite responses (fix the invocation vs raise the cap), and `$?`
//        cannot tell them apart. This is a LIVE ambiguity, not a hypothetical:
//        demo 5 has been observed reporting `exited with status 125` when the
//        cap fired. It is recorded here rather than resolved because changing
//        either value is a cross-repository decision.
//
//        The discriminator meanwhile is the same one that separates hermit's 125
//        from a guest's own 125: the `HERMIT_INTERNAL_FAILURE` marker on stderr,
//        present only when hermit itself failed. safehermit writes its verdict
//        to its `--sh-report` file instead. Read one of those, never the number
//        alone.
//
// ⚠️ 0 IS THE DANGEROUS DRIFT, NOT 126 OR 127. Every one of these is asserted as
// `assert_eq!(code, Some(CONST))`. At 0 those assertions would stop demanding a
// FAILURE and start demanding a SUCCESS, and would still pass -- sixteen tests
// silently inverted. `tests/cli.rs` also asserts `!status.success()` beside the
// code for that reason; this pin is the second of the two locks.
const _: () = assert!(
    HERMIT_VERIFICATION_DIVERGENCE_EXIT == 1,
    "1 is the established diff-style result for a completed comparison that diverged"
);
const _: () = assert!(
    HERMIT_INTERNAL_FAILURE_EXIT == 125,
    "125 is the GNU wrapper-failed convention; 1 is the commonest guest status and 0 is success"
);
const _: () = assert!(
    GUEST_PROGRAM_NOT_FOUND_EXIT == 127,
    "127 is the GNU command-not-found convention and must stay distinct from 125 and 126"
);
const _: () = assert!(
    GUEST_PROGRAM_NOT_EXECUTABLE_EXIT == 126,
    "126 is the GNU found-but-not-executable convention and must stay distinct from 125 and 127"
);
const _: () = assert!(
    HERMIT_LOG_CAP_EXIT == 123,
    "123 is Hermit's log-cap code, reserved for bin/safehermit's log-cap kill as well; see the allocation table"
);
const _: () = assert!(
    HERMIT_LOG_CAP_EXIT != 0
        && HERMIT_VERIFICATION_DIVERGENCE_EXIT != 0
        && HERMIT_INTERNAL_FAILURE_EXIT != 0
        && GUEST_PROGRAM_NOT_FOUND_EXIT != 0
        && GUEST_PROGRAM_NOT_EXECUTABLE_EXIT != 0,
    "a failure code of 0 would invert every assert_eq!(code, Some(CONST)) into demanding success"
);
mod record;
mod record_replay_path;
mod recorder;
mod replay;
mod replayer;
#[cfg(feature = "sabre")]
mod sabre_bootstrap;
#[cfg(feature = "sabre")]
mod sabre_ptrace;
mod script;

#[cfg(feature = "sabre")]
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
#[cfg(any(feature = "sabre", feature = "liteinst"))]
use std::os::unix::ffi::OsStrExt;
#[cfg(feature = "sabre")]
use std::os::unix::fs::OpenOptionsExt;
#[cfg(any(feature = "sabre", feature = "liteinst"))]
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;
#[cfg(feature = "sabre")]
use std::sync::Arc;
use std::sync::Mutex;
#[cfg(feature = "sabre")]
use std::sync::atomic::AtomicBool;
use std::time::Duration;
use std::time::Instant;

use anyhow::anyhow;
use clap::ValueEnum;
use consts::METADATA_NAME;
pub use detcore::Config as DetConfig;
pub use detcore::Detcore;
pub use detcore::RecordOrReplay;
#[doc(hidden)]
#[cfg(feature = "dbt")]
pub use detcore_dbt::reverie_dbt_runtime_background_init_v2;
#[doc(hidden)]
#[cfg(feature = "dbt")]
pub use detcore_dbt::reverie_dbt_runtime_background_init_v3;
#[doc(hidden)]
#[cfg(feature = "dbt")]
pub use detcore_dbt::reverie_dbt_runtime_name;
#[doc(hidden)]
#[cfg(feature = "dbt")]
pub use detcore_dbt::reverie_dbt_runtime_pre_syscall;
#[doc(hidden)]
#[cfg(feature = "dbt")]
pub use detcore_dbt::reverie_dbt_runtime_ready;
#[doc(hidden)]
#[cfg(feature = "dbt")]
pub use detcore_dbt::reverie_dbt_runtime_thread_exit;
#[doc(hidden)]
#[cfg(feature = "dbt")]
pub use detcore_dbt::reverie_dbt_runtime_thread_init;
#[doc(hidden)]
#[cfg(feature = "dbt")]
pub use detcore_dbt::reverie_dbt_runtime_totals;
pub use error::Context;
pub use error::Error;
pub use error::FailureKind;
pub use error::SerializableError;
use goblin::elf::Elf;
use goblin::elf::header;
use goblin::elf::section_header;
pub use id::Id;
use metadata::Metadata;
pub use ptrace_completion::HermitCleanupStage;
pub use ptrace_completion::HermitCleanupUnconfirmed;
pub use ptrace_completion::HermitPtraceFailure;
pub use ptrace_completion::RecoveryRefusal;
use record::Record;
use replay::Replay;
pub use reverie::ExitStatus;
#[cfg(feature = "sabre")]
use reverie::GlobalTool;
pub use reverie::process;
pub use reverie::process::Command;
pub use reverie::process::Mount;
pub use reverie::process::Namespace;
pub use reverie::process::Output;
pub use reverie::process::Stdio;
pub use script::Shebang;
use serde::Deserialize;
use serde::Serialize;

fn read_iovecs<M: reverie::syscalls::MemoryAccess>(
    memory: &M,
    message: &libc::msghdr,
) -> Result<Vec<libc::iovec>, reverie::Errno> {
    if message.msg_iovlen == 0 {
        return Ok(Vec::new());
    }
    let address = reverie::syscalls::Addr::from_raw(message.msg_iov as usize)
        .ok_or(reverie::Errno::EFAULT)?;
    let mut iovecs = vec![
        libc::iovec {
            iov_base: std::ptr::null_mut(),
            iov_len: 0,
        };
        message.msg_iovlen
    ];
    memory.read_values(address, &mut iovecs)?;
    Ok(iovecs)
}

fn vectored_offset(low: u64, high: u64) -> i64 {
    if std::mem::size_of::<usize>() == 8 {
        low as i64
    } else {
        ((high << 32) | (low & u32::MAX as u64)) as i64
    }
}

enum KvmStdinReservation {
    Open(fs::File),
    Closed,
}

static KVM_STDIN_RESERVATION: Mutex<Option<KvmStdinReservation>> = Mutex::new(None);

/// Saves stdin captured before Rust's process startup can reuse a closed fd 0.
pub fn reserve_kvm_stdin(stdin: Option<fs::File>) -> io::Result<()> {
    let mut reservation = KVM_STDIN_RESERVATION
        .lock()
        .map_err(|_| io::Error::other("KVM stdin reservation lock is poisoned"))?;
    if reservation.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "KVM stdin is already reserved",
        ));
    }
    *reservation = Some(match stdin {
        Some(file) => KvmStdinReservation::Open(file),
        None => KvmStdinReservation::Closed,
    });
    Ok(())
}

fn duplicate_current_stdin() -> io::Result<Option<fs::File>> {
    // SAFETY: F_DUPFD_CLOEXEC duplicates fd 0 without taking ownership of it.
    let duplicate = unsafe { libc::fcntl(libc::STDIN_FILENO, libc::F_DUPFD_CLOEXEC, 3) };
    if duplicate >= 0 {
        // SAFETY: F_DUPFD_CLOEXEC returned a new owned descriptor.
        return Ok(Some(unsafe { fs::File::from_raw_fd(duplicate) }));
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::EBADF) {
        Ok(None)
    } else {
        Err(error)
    }
}

fn ensure_kvm_stdin_reserved() -> io::Result<()> {
    let mut reservation = KVM_STDIN_RESERVATION
        .lock()
        .map_err(|_| io::Error::other("KVM stdin reservation lock is poisoned"))?;
    if reservation.is_none() {
        *reservation = Some(match duplicate_current_stdin()? {
            Some(file) => KvmStdinReservation::Open(file),
            None => KvmStdinReservation::Closed,
        });
    }
    Ok(())
}

fn reserved_kvm_stdin() -> Result<Option<fs::File>, Error> {
    ensure_kvm_stdin_reserved()?;
    let reservation = KVM_STDIN_RESERVATION
        .lock()
        .map_err(|_| io::Error::other("KVM stdin reservation lock is poisoned"))?;
    match reservation.as_ref() {
        Some(KvmStdinReservation::Open(file)) => Ok(Some(file.try_clone()?)),
        Some(KvmStdinReservation::Closed) => Ok(None),
        None => unreachable!("stdin reservation was initialized above"),
    }
}

/// A replayable snapshot of the guest's stdin for the output-capturing backends.
///
/// `hermit run --verify` executes the guest twice and compares the two runs. The
/// output-capturing backends historically fed the guest `Stdio::null()`, so any
/// data piped into hermit (`echo prog | hermit run --strict --verify -- ...`)
/// was silently dropped. Both runs then saw identical *empty* input and hermit
/// reported a false "deterministic" success even though the guest never received
/// its input. `Seekable` holds a read-only, rewindable file so both runs receive
/// the exact same bytes without being able to modify the caller's input; `None`
/// means stdin should be `/dev/null` (nothing to replay, or a terminal that
/// cannot be replayed identically to two runs).
enum StdinSnapshot {
    Seekable(fs::File),
    None,
}

static OUTPUT_STDIN_SNAPSHOT: Mutex<Option<StdinSnapshot>> = Mutex::new(None);

/// Records a rewindable snapshot of the process stdin so the output-capturing
/// backends can replay identical input to each run of `hermit run --verify`.
///
/// `stdin` is the descriptor captured before Rust startup could reuse a closed
/// fd 0 (see the binary's `startup_stdin`). A regular-file redirect is reopened
/// read-only; a pipe/fifo/socket is drained once into a seekable temporary file
/// and that file is reopened read-only. A terminal (or absent stdin) is treated
/// as `/dev/null` because a live terminal cannot be replayed identically to two
/// runs.
pub fn reserve_output_stdin_snapshot(stdin: Option<fs::File>) -> io::Result<()> {
    let snapshot = match stdin {
        None => StdinSnapshot::None,
        Some(mut file) => {
            // SAFETY: as_raw_fd borrows the descriptor without taking ownership.
            let is_tty = unsafe { libc::isatty(file.as_raw_fd()) } == 1;
            if is_tty {
                StdinSnapshot::None
            } else {
                let replay = if file.stream_position().is_ok() {
                    // Already seekable (e.g. `< file`): retain it below through
                    // a read-only descriptor with an independent file offset.
                    file
                } else {
                    // Non-seekable (pipe/fifo/socket): buffer once into a
                    // seekable temporary file so both --verify runs receive
                    // identical input.
                    //
                    // NAME THE WAIT BEFORE ENTERING IT. This `io::copy` reads to
                    // EOF, so a stdin that never reaches EOF -- an inherited socket,
                    // a fifo whose writer stays open -- blocks here FOREVER, before
                    // the guest has started. Measured: the process wedges with ZERO
                    // bytes on stdout and stderr and is only cleared by killing it,
                    // which is indistinguishable from a slow run. That
                    // indistinguishability is the whole harm; an unbounded wait that
                    // says what it is waiting for is merely slow, and a reader can
                    // act on it.
                    //
                    // Deliberately UNCONDITIONAL rather than emitted after a delay:
                    // a message that appears only once N seconds have passed makes
                    // hermit's own stderr depend on timing, and this project does
                    // not accept timing-dependent output. Deliberately NOT a
                    // deadline either -- a 12s slow producer is legitimate and
                    // delivers, and nothing can separate "slow" from "never" except
                    // by waiting. This changes no control flow: every input that
                    // worked before still works, byte for byte, on stdout.
                    const BUFFERING_NOTE: &str = "hermit: --verify is buffering stdin from a non-seekable stream so both \
                         runs receive identical input. If this appears to hang, stdin has not \
                         reached end-of-file; pass `< /dev/null` when the guest needs no input.";
                    if nonwaiting_write::diagnostics_must_not_wait() {
                        // Under --max-log-bytes, termination must not depend
                        // on this write; see `nonwaiting_write`.
                        nonwaiting_write::write_without_waiting(
                            libc::STDERR_FILENO,
                            format!("{BUFFERING_NOTE}\n").as_bytes(),
                        );
                    } else {
                        eprintln!("{BUFFERING_NOTE}");
                    }
                    let mut buffered = tempfile::tempfile()?;
                    io::copy(&mut file, &mut buffered)?;
                    buffered.seek(SeekFrom::Start(0))?;
                    buffered
                };

                // A duplicate would keep the original access mode. Reopen the
                // reserved descriptor read-only so neither verification run
                // can modify the caller's stdin or the next run's input.
                let replay_path = format!("/proc/self/fd/{}", replay.as_raw_fd());
                StdinSnapshot::Seekable(fs::File::open(replay_path)?)
            }
        }
    };
    let mut reservation = OUTPUT_STDIN_SNAPSHOT
        .lock()
        .map_err(|_| io::Error::other("output stdin snapshot lock is poisoned"))?;
    if reservation.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "output stdin snapshot is already reserved",
        ));
    }
    *reservation = Some(snapshot);
    Ok(())
}

/// Returns a rewound descriptor for the reserved stdin snapshot, if any.
///
/// Each call rewinds the snapshot and hands back a fresh descriptor, so
/// repeated `--verify` runs read identical input from the start. Returns `None`
/// when no snapshot was reserved or the reserved stdin is a terminal/absent
/// (nothing to replay). Runs are sequential (each executes in its own forked
/// container child), so resetting the shared open file description's offset here
/// is safe.
fn output_backend_stdin_reservation() -> Result<(bool, Option<fs::File>), Error> {
    let mut reservation = OUTPUT_STDIN_SNAPSHOT
        .lock()
        .map_err(|_| io::Error::other("output stdin snapshot lock is poisoned"))?;
    match reservation.as_mut() {
        Some(StdinSnapshot::Seekable(file)) => {
            file.seek(SeekFrom::Start(0))?;
            Ok((true, Some(file.try_clone()?)))
        }
        Some(StdinSnapshot::None) => Ok((true, None)),
        None => Ok((false, None)),
    }
}

fn output_backend_stdin_file() -> Result<Option<fs::File>, Error> {
    Ok(output_backend_stdin_reservation()?.1)
}

/// Returns the stdin to hand a guest run in an output-capturing backend.
///
/// When [`reserve_output_stdin_snapshot`] has stored a replayable snapshot the
/// file is rewound and a fresh descriptor handed to the guest, so each
/// `--verify` run reads identical input. Otherwise (no snapshot reserved, or a
/// terminal/absent stdin) the guest gets `/dev/null`, matching the previous
/// behavior for callers that do not reserve a snapshot.
fn output_backend_stdin() -> Result<Stdio, Error> {
    match output_backend_stdin_file()? {
        Some(file) => Ok(Stdio::from(file)),
        None => Ok(Stdio::null()),
    }
}

/// The result of recording a command.
#[derive(Debug, Serialize, Deserialize)]
pub struct Recording {
    /// The unique ID of the recording.
    pub id: Id,

    /// The exit code of the command.
    pub exit_status: ExitStatus,
}

#[derive(Clone, Copy)]
enum CapabilityProbe {
    Namespaces,
    Ptrace,
    Seccomp,
}

fn run_capability_probe(probe: CapabilityProbe) -> Result<bool, Error> {
    // SAFETY: The child calls only async-signal-safe syscalls and exits immediately.
    let pid = unsafe { libc::fork() };
    if pid == -1 {
        return Err(std::io::Error::last_os_error()).context("Failed to fork capability probe");
    }
    if pid == 0 {
        let supported = match probe {
            CapabilityProbe::Namespaces => unsafe {
                libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWPID) == 0
            },
            CapabilityProbe::Ptrace => {
                // SAFETY: PTRACE_TRACEME ignores the pid and address arguments.
                unsafe {
                    libc::ptrace(
                        libc::PTRACE_TRACEME,
                        0,
                        std::ptr::null_mut::<libc::c_void>(),
                        std::ptr::null_mut::<libc::c_void>(),
                    ) != -1
                }
            }
            CapabilityProbe::Seccomp => {
                let mut filter = libc::sock_filter {
                    code: 0x06, // BPF_RET | BPF_K
                    jt: 0,
                    jf: 0,
                    k: 0x7fff0000, // SECCOMP_RET_ALLOW
                };
                let program = libc::sock_fprog {
                    len: 1,
                    filter: &mut filter,
                };
                // SAFETY: The filter is an allow-all program with a valid one-element lifetime.
                unsafe {
                    libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) == 0
                        && libc::syscall(
                            libc::SYS_seccomp,
                            1, // SECCOMP_SET_MODE_FILTER
                            0,
                            &program,
                        ) == 0
                }
            }
        };
        // SAFETY: Avoid running Rust destructors after fork.
        unsafe { libc::_exit(i32::from(!supported)) }
    }

    let mut status = 0;
    loop {
        // SAFETY: pid is the child created above and status points to valid storage.
        let result = unsafe { libc::waitpid(pid, &mut status, 0) };
        if result == pid {
            return Ok(libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0);
        }
        if result == -1 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error).context("Failed to wait for capability probe");
        }
    }
}

fn validate_tracing_environment() -> Result<(), Error> {
    if !run_capability_probe(CapabilityProbe::Namespaces)? {
        anyhow::bail!(
            "Hermit cannot create its required user and PID namespaces: \
             unshare(CLONE_NEWUSER | CLONE_NEWPID) was denied. Allow unprivileged user namespaces \
             and the unshare syscall in the host/container policy."
        );
    }
    if !run_capability_probe(CapabilityProbe::Ptrace)? {
        anyhow::bail!(
            "Hermit cannot use ptrace in this environment: a child PTRACE_TRACEME probe was \
             denied. Allow same-UID parent-child ptrace in the container seccomp and host \
             Yama/LSM policy; CAP_SYS_PTRACE is normally not required. Use --namespace-only for \
             a sandbox smoke test without syscall interception."
        );
    }
    if !run_capability_probe(CapabilityProbe::Seccomp)? {
        anyhow::bail!(
            "Hermit cannot install its tracee seccomp filter: \
             seccomp(SECCOMP_SET_MODE_FILTER) was denied. Allow seccomp and \
             prctl(PR_SET_NO_NEW_PRIVS) in the container policy, or use --namespace-only for a \
             sandbox smoke test without syscall interception."
        );
    }
    Ok(())
}

#[cfg(feature = "dbt")]
fn is_dynamorio_sdk(path: &Path) -> bool {
    path.join("include/dr_api.h").is_file()
        || path.join("DynamoRIOConfig.cmake").is_file()
        || path.join("cmake/DynamoRIOConfig.cmake").is_file()
}

#[cfg(feature = "dbt")]
fn dynamorio_sdk_available() -> bool {
    if hermit_resources::resource("dynamorio/bin64/drrun")
        .is_ok_and(|path| path.is_some_and(|path| path.is_file()))
    {
        return true;
    }
    if reverie_dbt::bundled_drrun_path().is_file() {
        return true;
    }
    const DEFAULT_ROOTS: [&str; 3] = [
        "/usr/lib/cmake/DynamoRIO",
        "/usr/local/lib/cmake/DynamoRIO",
        "/opt/dynamorio",
    ];

    ["DYNAMORIO_HOME", "DynamoRIO_DIR"]
        .into_iter()
        .filter_map(std::env::var_os)
        .map(PathBuf::from)
        .chain(DEFAULT_ROOTS.into_iter().map(PathBuf::from))
        .any(|path| is_dynamorio_sdk(&path))
}

#[cfg(feature = "dbt")]
fn dbt_runtime_unavailable_reason() -> Option<String> {
    dbt_runtime_unavailable_reason_from(detcore_dbt::runtime_library_path())
}

#[cfg(feature = "dbt")]
fn dbt_runtime_unavailable_reason_from(runtime: io::Result<PathBuf>) -> Option<String> {
    runtime.err().map(|error| {
        format!(
            "the Detcore DBT runtime is unavailable: {error}; build the hermit binary and \
             cdylib in the same target directory"
        )
    })
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(#688): Review LiteInst runtime discovery.
/// Checks that `path` is an x86-64 shared object that exports every name in
/// `required` and registers `initializer_name` in its constructor array, so
/// that preloading it runs that initializer. `label` names the library in every
/// error message. Returns the canonical path.
fn validate_preload_constructor_library(
    path: &Path,
    label: &str,
    required: &[&str],
    initializer_name: &str,
) -> io::Result<PathBuf> {
    let bytes = fs::read(path).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("failed to read {label} {}: {error}", path.display()),
        )
    })?;
    let elf = Elf::parse(&bytes).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{label} {} is not an ELF DSO: {error}", path.display()),
        )
    })?;
    if elf.header.e_type != header::ET_DYN || elf.header.e_machine != header::EM_X86_64 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{label} {} is not an x86-64 shared object", path.display()),
        ));
    }

    for &name in required {
        if !elf
            .dynsyms
            .iter()
            .any(|symbol| elf.dynstrtab.get_at(symbol.st_name) == Some(name))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "{label} {} is missing required export {name}",
                    path.display()
                ),
            ));
        }
    }
    let (initializer_index, initializer) = elf
        .dynsyms
        .iter()
        .enumerate()
        .find(|(_, symbol)| elf.dynstrtab.get_at(symbol.st_name) == Some(initializer_name))
        .ok_or_else(|| {
            io::Error::other(format!(
                "checked {label} initializer {initializer_name} disappeared"
            ))
        })?;
    let init_array = elf
        .section_headers
        .iter()
        .find(|section| section.sh_type == section_header::SHT_INIT_ARRAY)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{label} {} has no constructor array", path.display()),
            )
        })?;
    let init_start = init_array.sh_addr;
    let init_end = init_start.saturating_add(init_array.sh_size);
    let relocated_initializer = elf
        .dynrelas
        .iter()
        .chain(elf.dynrels.iter())
        .any(|relocation| {
            (init_start..init_end).contains(&relocation.r_offset)
                && relocation.r_sym == initializer_index
        });
    let init_bytes = usize::try_from(init_array.sh_offset)
        .ok()
        .and_then(|start| {
            usize::try_from(init_array.sh_size)
                .ok()
                .and_then(|size| bytes.get(start..start.checked_add(size)?))
        })
        .unwrap_or_default();
    let direct_initializer = init_bytes
        .as_chunks::<8>()
        .0
        .iter()
        .any(|entry| u64::from_le_bytes(*entry) == initializer.st_value);
    if !relocated_initializer && !direct_initializer {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{label} {} does not register {initializer_name} as a preload constructor",
                path.display()
            ),
        ));
    }
    path.canonicalize()
}

/// Environment variable naming the in-guest Detcore preload
/// (`libdetcore_liteinst.so`) explicitly.
pub const LITEINST_TOOL_RUNTIME_ENV: &str = "HERMIT_LITEINST_TOOL_RUNTIME";

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-3635): Review in-guest Detcore runtime discovery.
/// Returns the in-guest Detcore preload: the one [`LITEINST_TOOL_RUNTIME_ENV`]
/// names; otherwise the packaged `rsrcs/libdetcore_liteinst.so` of the selected
/// Hermit installation (`hermit-install` stages it, as it stages the DBT and
/// SaBRe Detcore runtimes, and a standalone installation or published bundle
/// carries it); otherwise the one `cargo build -p detcore-liteinst` built beside
/// the Hermit binary or in its `deps/` directory. Packaged resources come before
/// the Cargo artifact directory, the order the DBT and SaBRe runtimes use.
///
/// A runtime built from another tree is refused when it starts: Hermit passes
/// it [`detcore::config_wire_fingerprint`], which it compares with its own
/// before decoding anything (<https://github.com/rrnewton/hermit/issues/3986>).
#[doc(hidden)]
pub fn liteinst_tool_runtime_library_path() -> io::Result<PathBuf> {
    if let Some(path) = std::env::var_os(LITEINST_TOOL_RUNTIME_ENV) {
        let path = PathBuf::from(path);
        if !path.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("{LITEINST_TOOL_RUNTIME_ENV} does not name a regular file"),
            ));
        }
        return validate_liteinst_tool_runtime_library(&path).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("{LITEINST_TOOL_RUNTIME_ENV} is invalid: {error}"),
            )
        });
    }

    if let Some(path) = hermit_resources::resource("libdetcore_liteinst.so")?
        && path.is_file()
    {
        return validate_liteinst_tool_runtime_library(&path);
    }

    let executable = std::env::current_exe()?;
    let directory = executable.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "Hermit executable has no parent directory",
        )
    })?;
    if let Some(path) = [
        directory.join("libdetcore_liteinst.so"),
        directory.join("deps/libdetcore_liteinst.so"),
    ]
    .into_iter()
    .find(|path| path.is_file())
    {
        return validate_liteinst_tool_runtime_library(&path);
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!(
            "libdetcore_liteinst.so was not built beside {} or staged as an installed resource",
            executable.display()
        ),
    ))
}

fn validate_liteinst_tool_runtime_library(path: &Path) -> io::Result<PathBuf> {
    validate_preload_constructor_library(
        path,
        "in-guest Detcore runtime",
        &["detcore_liteinst_initialize"],
        "detcore_liteinst_initialize",
    )
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-3635): Review the in-guest LiteInst library refusals.
/// In-guest LiteInst refused a run before starting its guest, because the
/// runtime cannot honour the request or could not monitor the program.
///
/// A type rather than a message, because it crosses the container error
/// boundary as [`error::FailureKind::PolicyRefusal`] (exit 122).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiteinstInGuestRefusal {
    backend: Backend,
    reason: String,
}

impl LiteinstInGuestRefusal {
    /// A refusal of a `--backend=liteinst` run.
    pub fn new(reason: impl Into<String>) -> Self {
        Self::for_backend(Backend::Liteinst, reason)
    }

    /// A refusal of a run of `backend`, one of the in-guest LiteInst backends
    /// ([`Backend::is_in_guest_liteinst`]), so the message names the backend
    /// the caller selected.
    pub fn for_backend(backend: Backend, reason: impl Into<String>) -> Self {
        debug_assert!(backend.is_in_guest_liteinst(), "{backend:?}");
        Self {
            backend,
            reason: reason.into(),
        }
    }
}

impl std::fmt::Display for LiteinstInGuestRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let runtime = match self.backend {
            Backend::InGuestTrap => "in-guest LiteInst, site patching off",
            _ => "in-guest LiteInst",
        };
        write!(
            f,
            "--backend={} ({runtime}) refuses this run: {}",
            self.backend.as_str(),
            self.reason
        )
    }
}

impl std::error::Error for LiteinstInGuestRefusal {}

/// Refuses, before the guest starts, an in-guest LiteInst run that the runtime
/// cannot honour or could not monitor.
///
/// - A maximum timeslice. Reverie's in-guest Tool host has no preemption timer:
///   its `set_timer` refuses with `ENOSYS`, and Detcore treats a failed
///   `set_timer` as fatal (`.expect("Failed to set timer")`), so the guest
///   would panic at its first scheduling point. The CLI refuses
///   `--max-timeslice` earlier; this covers library callers.
/// - A kernel older than Linux 6.5 ([`in_guest_liteinst_kernel_gap`]).
/// - A guest program that would run code before, or without, the runtime's
///   constructor ([`in_guest_liteinst_program_gap`]).
/// - For `in-guest-trap`, a guest environment that sets
///   `REVERIE_LITEINST_SITE_PATCHING` to anything but `0`
///   ([`in_guest_trap_site_patching_conflict`]).
///
/// This runs where the guest is spawned (inside the container for the CLI),
/// so it resolves the program in the filesystem the guest sees. It pins the
/// resolved program on `command` ([`pin_spawn_program`]), so the launch
/// executes the same path this check inspected. For `in-guest-trap` it then
/// sets `REVERIE_LITEINST_SITE_PATCHING=0` on `command`
/// ([`force_in_guest_site_patching`]).
#[cfg(feature = "liteinst")]
fn refuse_in_guest_liteinst_run(
    command: &mut Command,
    config: &DetConfig,
    backend: Backend,
) -> Result<(), Error> {
    let reason = if let Some(conflict) = in_guest_trap_site_patching_conflict(command, backend) {
        Some(conflict)
    } else if config.max_timeslice.is_some() {
        Some(
            "it cannot deliver Detcore's preemption timer yet (the in-guest Tool host refuses \
             set_timer with ENOSYS), so the run would fail at its first timeslice; pass \
             --max-timeslice=disabled"
                .to_owned(),
        )
    } else if let Some(gap) = in_guest_liteinst_kernel_gap(&running_kernel_release()) {
        Some(gap)
    } else {
        in_guest_liteinst_program_gap(command)?
    };
    match reason {
        Some(reason) => Err(Error::new(LiteinstInGuestRefusal::for_backend(
            backend, reason,
        ))),
        None => {
            force_in_guest_site_patching(command, backend);
            Ok(())
        }
    }
}

/// The value of `REVERIE_LITEINST_SITE_PATCHING` (`reverie_liteinst::SITE_PATCHING_ENV`)
/// that the guest sees under `backend`, when Hermit decides it: `0` for
/// `in-guest-trap`, which exists to run with no syscall site patched, and
/// `None` for `liteinst`, which leaves the variable to the guest environment
/// (unset means patching on).
pub fn forced_site_patching_value(backend: Backend) -> Option<&'static str> {
    match backend {
        Backend::InGuestTrap => Some("0"),
        _ => None,
    }
}

/// Why an `in-guest-trap` run cannot start with the guest environment of
/// `command`: the environment (explicit or inherited) sets
/// `REVERIE_LITEINST_SITE_PATCHING` to a value other than the one Hermit
/// forces. Hermit refuses such a run rather than overriding the value
/// silently, because the caller asked for two contradictory things: site
/// patching (or a value the runtime would reject) and the backend that exists
/// to run without it. A value equal to the forced one is accepted. `None`
/// for every other backend.
#[cfg(feature = "liteinst")]
fn in_guest_trap_site_patching_conflict(command: &Command, backend: Backend) -> Option<String> {
    let forced = forced_site_patching_value(backend)?;
    let name = reverie_liteinst::SITE_PATCHING_ENV;
    let value = command.get_env(name)?;
    (*value != *std::ffi::OsStr::new(forced)).then(|| {
        format!(
            "the guest environment sets {name}={}, but --backend={} runs with syscall site \
             patching off and sets {name}={forced} itself; drop the variable, or select \
             --backend=liteinst to patch syscall sites",
            value.to_string_lossy(),
            backend.as_str()
        )
    })
}

/// Sets the guest's `REVERIE_LITEINST_SITE_PATCHING` to the value Hermit
/// forces for `backend` ([`forced_site_patching_value`]), leaving the
/// environment unchanged for a backend that forces none. The guest's
/// descendants inherit it unless the guest changes its own environment; a
/// descendant that turns patching back on shows in the run's dispatch record
/// as patched sites.
#[cfg(feature = "liteinst")]
fn force_in_guest_site_patching(command: &mut Command, backend: Backend) {
    if let Some(value) = forced_site_patching_value(backend) {
        command.env(reverie_liteinst::SITE_PATCHING_ENV, value);
    }
}

/// Whether every in-guest Tool image of a `backend` run checks that its runtime
/// captured site patching off and statistics on
/// (`detcore::Config::in_guest_site_patching_off`): true for `in-guest-trap`.
#[cfg(feature = "liteinst")]
fn in_guest_site_patching_off(backend: Backend) -> bool {
    forced_site_patching_value(backend) == Some("0")
}

/// Whether an in-guest run of `backend` collects LiteInst's statistics: when
/// something reads them ([`backend_stats::request`]), and on every
/// `in-guest-trap` run, whose result is checked against them
/// ([`in_guest_trap_patching_evidence`]).
#[cfg(feature = "liteinst")]
fn in_guest_stats_request(
    backend: Backend,
    summary_json: &Option<PathBuf>,
) -> reverie::BackendStatsRequest {
    if backend == Backend::InGuestTrap {
        reverie::BackendStatsRequest::ENABLED
    } else {
        backend_stats::request(summary_json)
    }
}

/// Why a finished `in-guest-trap` run did not show that it ran with syscall
/// site patching off, from its dispatch record and the number of guest
/// processes that reported LiteInst statistics: a site the runtime patched or
/// considered patching (with patching off it considers none), no record, or no
/// process report at all, in which case the record's zeros are not
/// measurements (a run whose guests collected no statistics still yields an
/// empty record). `None` for every other backend.
///
/// This is the coordinator's postcondition. Each Tool image also refuses to
/// run its program when the runtime started it with patching on
/// (`detcore::in_guest_site_patching`), which holds for every image whatever
/// it reports; this one sees only the images that report, because an image
/// reports its statistics when it exits (`exit` or `exit_group`), so an image
/// that `execve`s, or is killed by a signal, sends none, and the coordinator
/// cannot tell that from a report that was suppressed. It refuses a run in
/// which no process reported; it does not detect a run in which some did and
/// others did not. Each image's own check covers that: an image whose runtime
/// is not collecting statistics refuses to run its program.
#[cfg(feature = "liteinst")]
fn in_guest_trap_patching_evidence(
    backend: Backend,
    dispatch: Option<&reverie::DispatchStats>,
    process_reports: u64,
) -> Option<String> {
    if backend != Backend::InGuestTrap {
        return None;
    }
    let Some(record) = dispatch else {
        return Some(
            "the run produced no LiteInst dispatch record, so it cannot show that no syscall \
             site was patched"
                .to_owned(),
        );
    };
    if process_reports == 0 {
        return Some(
            "no guest process reported LiteInst statistics, so the run's zero patched sites \
             are not a measurement and it cannot show that no syscall site was patched"
                .to_owned(),
        );
    }
    let shown =
        |count: Option<u64>| count.map_or_else(|| "unmeasured".to_owned(), |n| n.to_string());
    (record.sites.patched != Some(0) || record.sites.candidates != Some(0)).then(|| {
        format!(
            "its guests' LiteInst statistics report {} patched and {} candidate syscall sites, \
             so it did not run with syscall site patching off and its result is not an \
             in-guest-trap result",
            shown(record.sites.patched),
            shown(record.sites.candidates)
        )
    })
}

/// Refuses a finished `in-guest-trap` run whose dispatch record shows site
/// patching ([`in_guest_trap_patching_evidence`]): records a determinism loss,
/// so verification refuses to compare it, and returns the typed in-guest
/// refusal (exit 122).
#[cfg(feature = "liteinst")]
fn refuse_in_guest_trap_patching(backend: Backend, evidence: Option<String>) -> Result<(), Error> {
    match evidence {
        Some(reason) => {
            detcore::detlog::record_determinism_loss(&reason);
            Err(Error::new(LiteinstInGuestRefusal::for_backend(
                backend, reason,
            )))
        }
        None => Ok(()),
    }
}

/// The oldest kernel in-guest LiteInst runs on, as (major, minor). Linux 6.5
/// added `SO_PEERPIDFD`: the coordinator admits each guest process through the
/// pidfd of the socket's peer and watches that pidfd for the process's exit
/// (`in_guest_exits`). On an older kernel every admission would fail.
#[cfg(feature = "liteinst")]
const IN_GUEST_LITEINST_KERNEL_FLOOR: (u32, u32) = (6, 5);

/// The running kernel's release string (`uname -r`), or empty if unreadable.
#[cfg(feature = "liteinst")]
fn running_kernel_release() -> String {
    let mut name: libc::utsname = unsafe { std::mem::zeroed() };
    // SAFETY: uname fills the struct it is given.
    if unsafe { libc::uname(&mut name) } != 0 {
        return String::new();
    }
    // SAFETY: uname NUL-terminates each field.
    unsafe { std::ffi::CStr::from_ptr(name.release.as_ptr()) }
        .to_string_lossy()
        .into_owned()
}

/// Why kernel `release` cannot run in-guest LiteInst, or `None` when it is at
/// least [`IN_GUEST_LITEINST_KERNEL_FLOOR`]. A release whose version cannot be
/// read is refused.
#[cfg(feature = "liteinst")]
fn in_guest_liteinst_kernel_gap(release: &str) -> Option<String> {
    let mut numbers = release
        .split(|c: char| !c.is_ascii_digit())
        .map(|part| part.parse::<u32>().ok());
    let (floor_major, floor_minor) = IN_GUEST_LITEINST_KERNEL_FLOOR;
    match (numbers.next().flatten(), numbers.next().flatten()) {
        (Some(major), Some(minor)) if (major, minor) >= IN_GUEST_LITEINST_KERNEL_FLOOR => None,
        (Some(_), Some(_)) => Some(format!(
            "it needs Linux {floor_major}.{floor_minor} or later (SO_PEERPIDFD, which it uses to \
             admit each guest process and observe its exit); this kernel is {release}"
        )),
        _ => Some(format!(
            "it needs Linux {floor_major}.{floor_minor} or later (SO_PEERPIDFD), and the kernel \
             release {release:?} does not say which version this is"
        )),
    }
}

/// How many `#!` interpreters [`in_guest_liteinst_program_gap`] follows before
/// it refuses the program. The kernel has its own small limit (`exec_binprm` in
/// `fs/exec.c`); a chain this check stops following is refused, not run, so the
/// check fails closed whichever limit is lower.
#[cfg(feature = "liteinst")]
const MAX_INTERPRETER_DEPTH: usize = 4;

/// Why the guest program would run code before, or without, the in-guest
/// runtime, or `None` when it would not.
///
/// The runtime is an `LD_PRELOAD` library whose constructor installs Detcore.
/// The dynamic loader never loads it into a statically linked image or an image
/// of another architecture, and ignores `LD_PRELOAD` in secure-execution mode
/// (set-user-ID, set-group-ID, or file capabilities). An executable's
/// `DT_PREINIT_ARRAY` runs before every library constructor. Each of those
/// would run guest code unmonitored, so each is refused. `#!` scripts are
/// checked through their interpreter, read as the kernel's script handler
/// reads it ([`script::KernelScript`]). A program or interpreter that is not a
/// regular file is refused before it is opened: the kernel executes only
/// regular files, opening a device can act on it, opening a socket fails, and
/// reading a FIFO that has no writer would block the check.
///
/// Every fact about one file (its type, header, ELF headers, mode bits and
/// capabilities) is read through one open descriptor, so they all describe
/// the same file. The check is still advisory, not containment: the `execve`
/// opens the path again, and a file replaced at that path in between is not
/// checked again.
///
/// Not covered, and recorded on
/// <https://github.com/rrnewton/hermit/issues/3520>:
/// - The loader itself, the C library's initialization, the executable's
///   `IFUNC` resolvers and the constructors of the executable's own shared
///   libraries all run before the runtime's constructor in every guest.
/// - Secure-execution mode that a security module or a parent with differing
///   real and effective IDs imposes; only the file's own mode bits and
///   capabilities are checked.
/// - Whether the `PT_INTERP` loader honours `LD_PRELOAD` at all.
/// - A `binfmt_misc` rule, which the kernel consults before its ELF and script
///   handlers, and which can hand a file this check accepted to another
///   interpreter (for example an emulator).
/// - A runtime library the loader fails to load: the loader continues without
///   it, and only the launch's check that the guest connected before it exited
///   reports the failure, after the guest ran.
/// - A `pre_exec` callback on `command` that changes the working directory or
///   the root directory: the check resolves paths against the working
///   directory `command` is configured with, in Hermit's root, before any
///   such callback runs.
#[cfg(feature = "liteinst")]
fn in_guest_liteinst_program_gap(command: &mut Command) -> Result<Option<String>, Error> {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;

    use script::KernelScript;

    let mut path = pin_spawn_program(command)?;
    for _ in 0..=MAX_INTERPRETER_DEPTH {
        let shown = path.display().to_string();
        let not_regular = || {
            Ok(Some(format!(
                "{shown} is not a regular file, and the kernel executes only regular files"
            )))
        };
        if !fs::metadata(&path)
            .with_context(|| format!("cannot read guest program {shown}"))?
            .is_file()
        {
            return not_regular();
        }
        // The path can change type after the check above. O_NONBLOCK then keeps
        // the open of a FIFO that has no writer from blocking, and O_NOCTTY
        // keeps a terminal from becoming Hermit's controlling terminal; neither
        // changes how a regular file is read. The descriptor's own type is
        // checked again below.
        let mut file = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)
            .open(&path)
            .with_context(|| format!("cannot read guest program {shown}"))?;
        if !file
            .metadata()
            .with_context(|| format!("cannot read guest program {shown}"))?
            .is_file()
        {
            return not_regular();
        }
        let mut header = Vec::new();
        (&file)
            .take(256)
            .read_to_end(&mut header)
            .with_context(|| format!("cannot read guest program {shown}"))?;
        match KernelScript::from_buf(&header) {
            KernelScript::Interpreter(interpreter) => {
                // The kernel opens a relative interpreter against the working
                // directory of the process calling execve.
                path = match command.get_current_dir() {
                    Some(directory) => directory.join(interpreter),
                    None => interpreter,
                };
                continue;
            }
            KernelScript::Declined => {
                return Ok(Some(format!(
                    "{shown} starts with #! but the kernel's script handler declines it (it \
                     names no interpreter, or the interpreter path fills the first 256 bytes), \
                     so the kernel would fail the execve or hand the file to another handler \
                     that need not load the runtime"
                )));
            }
            KernelScript::NotScript => {}
        }
        let Some(startup) = interp::elf_startup(&mut file)
            .with_context(|| format!("cannot read the ELF headers of guest program {shown}"))?
        else {
            return Ok(Some(format!(
                "{shown} is neither an ELF executable nor a #! script, so the kernel would hand \
                 it to a handler that need not load the runtime"
            )));
        };
        let reason = if !startup.x86_64 {
            "is not a 64-bit x86-64 ELF image, so the dynamic loader cannot load the 64-bit \
             runtime into it"
        } else if !startup.has_interp {
            "is statically linked (it has no PT_INTERP), so no dynamic loader runs to load the \
             runtime"
        } else if is_secure_execution_image(&file, &shown)? {
            "is set-user-ID, set-group-ID or has file capabilities, so the kernel runs it in \
             secure-execution mode, where the dynamic loader ignores LD_PRELOAD"
        } else if startup.has_preinit_array {
            "has a DT_PREINIT_ARRAY, whose functions run before the runtime's constructor"
        } else {
            return Ok(None);
        };
        return Ok(Some(format!(
            "guest program {shown} {reason}, so it would run unmonitored if Hermit launched it"
        )));
    }
    Ok(Some(format!(
        "the #! interpreters of guest program {} nest more than {MAX_INTERPRETER_DEPTH} deep",
        Path::new(command.get_program()).display()
    )))
}

/// Resolves the program `command` executes with Reverie's
/// [`Command::find_program`], the call the in-guest launch makes, and pins the
/// result on `command`, keeping `argv[0]`.
///
/// The result is absolute, so the launch's own `find_program` returns it
/// unchanged and the launch executes the same path the check inspected,
/// whatever `PATH` or working directory either side would otherwise use. The
/// path, not its contents: a file replaced at that path in between is not
/// checked again (see [`in_guest_liteinst_program_gap`]). A program the launch
/// could not find is an error here, as it would be at launch.
#[cfg(feature = "liteinst")]
fn pin_spawn_program(command: &mut Command) -> Result<PathBuf, Error> {
    let arg0 = command.get_arg0().to_owned();
    let program = command.find_program().with_context(|| {
        format!(
            "cannot find guest program {}",
            Path::new(command.get_program()).display()
        )
    })?;
    command.program(&program).arg0(arg0);
    Ok(program)
}

/// Whether executing `file` (shown as `shown`) puts the kernel in
/// secure-execution mode (`AT_SECURE`): set-user-ID, set-group-ID (with group
/// execute, as the kernel requires), or a `security.capability` attribute.
#[cfg(feature = "liteinst")]
fn is_secure_execution_image(file: &fs::File, shown: &str) -> Result<bool, Error> {
    let mode = file
        .metadata()
        .with_context(|| format!("cannot stat guest program {shown}"))?
        .permissions()
        .mode();
    if mode & 0o4000 != 0 || mode & 0o2010 == 0o2010 {
        return Ok(true);
    }
    // SAFETY: the descriptor is open for the call, the name is NUL-terminated,
    // and a zero-sized query writes no value.
    let size = unsafe {
        libc::fgetxattr(
            file.as_raw_fd(),
            c"security.capability".as_ptr(),
            std::ptr::null_mut(),
            0,
        )
    };
    if size >= 0 {
        return Ok(true);
    }
    match io::Error::last_os_error().raw_os_error() {
        Some(libc::ENODATA) | Some(libc::ENOTSUP) => Ok(false),
        _ => Err(Error::new(io::Error::last_os_error()).context(format!(
            "cannot read the file capabilities of guest program {shown}"
        ))),
    }
}

/// Refuses a wall-clock bound on an in-guest LiteInst run. The bound drops the
/// run's future, and dropping it neither kills the guest nor stops the
/// blocking task that waits for it.
#[cfg(feature = "liteinst")]
fn refuse_in_guest_liteinst_timeout(
    backend: Backend,
    timeout: Option<Duration>,
) -> Result<(), Error> {
    if backend.is_in_guest_liteinst() && timeout.is_some() {
        return Err(Error::new(LiteinstInGuestRefusal::for_backend(
            backend,
            "a wall-clock timeout has not been qualified for the in-guest runtime, whose run \
             future does not kill the guest when it is dropped",
        )));
    }
    Ok(())
}

#[cfg(not(feature = "liteinst"))]
fn refuse_in_guest_liteinst_timeout(
    _backend: Backend,
    _timeout: Option<Duration>,
) -> Result<(), Error> {
    Ok(())
}

#[cfg(feature = "liteinst")]
fn liteinst_unavailable_reason() -> Option<String> {
    liteinst_runtime_unavailable_reason()
}

// LiteInst is an experimental backend whose architecture is not settled, so the
// published crate leaves it out: `reverie-liteinst` (and through it `liteinst2`)
// is an optional dependency behind the `liteinst` feature, which
// `third-party-backends` enables for every validation build.
#[cfg(not(feature = "liteinst"))]
fn liteinst_unavailable_reason() -> Option<String> {
    Some("this build was compiled without the liteinst backend: the `liteinst` feature is not enabled; rebuild with `--features liteinst` (or `--features third-party-backends`). This says nothing about whether LiteInst works on this machine -- it has not been checked".to_owned())
}

#[cfg(feature = "liteinst")]
fn liteinst_runtime_unavailable_reason() -> Option<String> {
    liteinst_tool_runtime_library_path().err().map(|error| {
        format!(
            "the in-guest Detcore runtime is unavailable: {error}; build it with `cargo build -p detcore-liteinst` in the target directory of this hermit binary, install it as rsrcs/libdetcore_liteinst.so, or name it with {LITEINST_TOOL_RUNTIME_ENV}"
        )
    })
}

fn kvm_device_unavailable_reason(path: &Path) -> Option<String> {
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .err()
        .map(|error| {
            format!(
                "cannot open {} read-write: {error}; grant access through the device owner/group \
                 or root",
                path.display()
            )
        })
}

/// Process instrumentation backend used to run a Hermit guest.
#[derive(Debug, Clone, Copy, Default, Eq, PartialEq, ValueEnum)]
pub enum Backend {
    /// Use Reverie's ptrace backend.
    #[default]
    Ptrace,
    /// Use the DynamoRIO backend.
    Dbt,
    /// Use LiteInst, with Detcore's Tool running inside the guest.
    Liteinst,
    /// Use in-guest LiteInst with syscall site patching off: the same
    /// in-guest Detcore host as `liteinst`, but every trapped syscall reaches
    /// the Tool through the seccomp `SIGSYS` fallback and no syscall site is
    /// rewritten. Hermit sets `REVERIE_LITEINST_SITE_PATCHING=0` in the
    /// guest's environment and refuses a run whose environment asks for any
    /// other value.
    InGuestTrap,
    /// Use the SaBRe static binary rewriting backend.
    Sabre,
    /// Use the KVM backend.
    Kvm,
    /// Preprocess the main ELF with e9patch, then use the ptrace runtime.
    // TODO-HUMAN-REVIEW(PR-594): Review the CLI-only hybrid backend selection.
    E9patch,
}

/// A requested backend could not start because its required runtime support is absent.
///
/// Keep this as a typed error until the top-level command records its failure class.
/// The human-readable message is for the operator; consumers must use the class line
/// emitted by the command rather than matching this prose.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct BackendUnavailable {
    backend: Backend,
    reason: String,
}

impl BackendUnavailable {
    pub fn new(backend: Backend, reason: impl Into<String>) -> Self {
        Self {
            backend,
            reason: reason.into(),
        }
    }

    pub fn backend(&self) -> Backend {
        self.backend
    }

    pub fn reason(&self) -> &str {
        &self.reason
    }
}

impl std::fmt::Display for BackendUnavailable {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "backend `{}` is unavailable: {}",
            self.backend.as_str(),
            self.reason
        )
    }
}

impl std::error::Error for BackendUnavailable {}

impl Backend {
    const ALL: [Self; 7] = [
        Self::Ptrace,
        Self::Dbt,
        Self::Liteinst,
        Self::InGuestTrap,
        Self::Sabre,
        Self::Kvm,
        Self::E9patch,
    ];

    /// Returns the command-line spelling for this backend.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ptrace => "ptrace",
            Self::Dbt => "dbt",
            Self::Liteinst => "liteinst",
            Self::InGuestTrap => "in-guest-trap",
            Self::Sabre => "sabre",
            Self::Kvm => "kvm",
            Self::E9patch => "e9patch",
        }
    }

    /// Whether this backend hosts Detcore's Tool inside each guest process
    /// through the in-guest LiteInst preload runtime: `liteinst` and
    /// `in-guest-trap`, which differ only in whether syscall sites are patched.
    /// Every refusal, capability and launch step of in-guest LiteInst applies to
    /// both.
    pub const fn is_in_guest_liteinst(self) -> bool {
        matches!(self, Self::Liteinst | Self::InGuestTrap)
    }

    /// The name of the backend runtime that executes this backend's guest, as
    /// its statistics source names itself (`BackendStatsSnapshot::BACKEND_NAME`).
    /// `in-guest-trap` runs the in-guest LiteInst runtime, so its statistics come
    /// from that runtime's source, which is named `liteinst`.
    pub const fn runtime_name(self) -> &'static str {
        match self {
            Self::InGuestTrap => Self::Liteinst.as_str(),
            other => other.as_str(),
        }
    }

    fn uses_ptrace_pmu_timers(self) -> bool {
        match self {
            Self::Ptrace | Self::E9patch => true,
            // In-guest Detcore has no ptrace tracer to arm the PMU timer from.
            Self::Liteinst | Self::InGuestTrap | Self::Dbt | Self::Sabre | Self::Kvm => false,
        }
    }

    /// Returns backends whose Hermit integration prerequisites are met.
    ///
    /// Some integrations use CLI launch adapters rather than direct
    /// [`run_with_backend`] dispatch.
    pub fn available() -> impl Iterator<Item = Self> {
        Self::ALL
            .into_iter()
            .filter(|backend| backend.is_available())
    }

    /// Returns whether this backend's integration prerequisites are met.
    pub fn is_available(self) -> bool {
        self.unavailable_reason().is_none()
    }

    /// Returns an actionable error when this backend's prerequisites are not met.
    pub fn ensure_available(self) -> Result<(), Error> {
        if let Some(reason) = self.unavailable_reason() {
            Err(Error::new(BackendUnavailable::new(self, reason)))
        } else {
            Ok(())
        }
    }

    fn unavailable_reason(self) -> Option<String> {
        match self {
            Self::Ptrace => validate_tracing_environment()
                .err()
                .map(|error| error.to_string()),
            Self::Dbt => dbt_unavailable_reason(),
            // The same runtime, so the same prerequisites and the same reason.
            Self::Liteinst | Self::InGuestTrap => liteinst_unavailable_reason(),
            // TODO-HUMAN-REVIEW(#589): Review SaBRe backend availability reporting.
            Self::Sabre => sabre_unavailable_reason(),
            Self::Kvm => kvm_device_unavailable_reason(Path::new("/dev/kvm")),
            Self::E9patch => e9patch_unavailable_reason(),
        }
    }
}

struct SkidOvershootReport {
    enabled: bool,
}

/// The backend failed while running the guest, after Hermit launched it: the
/// DBT evidence check, a KVM execution failure, a SaBRe guest that never
/// reached Detcore. `run --verify` records it as `run_failed` with
/// `backend_failure` instead of leaving the `not_run` stamp.
///
/// It sits in the chain below the message that names the failure, so the
/// first `Error:` line is unchanged; it crosses the container error boundary
/// as [`error::FailureKind::BackendRunFailure`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackendRunFailure;

impl std::fmt::Display for BackendRunFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the backend failed while running the guest")
    }
}

impl std::error::Error for BackendRunFailure {}

/// A ptrace-backed run observed late precise-timer delivery, so Hermit refuses
/// to treat the completed execution as deterministic evidence.
///
/// This is a type rather than a message match because it crosses the container
/// error boundary as [`error::FailureKind::PolicyRefusal`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SkidOvershootError {
    count: u64,
}

impl SkidOvershootError {
    pub fn new(count: u64) -> Self {
        assert!(
            count > 0,
            "a skid-overshoot refusal requires a positive count"
        );
        Self { count }
    }

    pub fn count(&self) -> u64 {
        self.count
    }
}

impl std::fmt::Display for SkidOvershootError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "observed {} {} report(s); refusing the result because precise PMU timer \
             delivery passed its target and deterministic execution was not established",
            self.count,
            reverie::SKID_OVERSHOOT_MARKER
        )
    }
}

impl std::error::Error for SkidOvershootError {}

fn take_skid_overshoot_error() -> Option<SkidOvershootError> {
    let count = reverie::take_skid_overshoot_count();
    (count > 0).then(|| SkidOvershootError::new(count))
}

impl SkidOvershootReport {
    fn begin(enabled: bool) -> Self {
        if enabled {
            // A previous invocation in this process must not qualify this one.
            let _ = reverie::take_skid_overshoot_count();
        }
        Self { enabled }
    }

    fn finish<T>(self, result: Result<T, Error>) -> Result<T, Error> {
        if !self.enabled {
            return result;
        }

        let Some(overshoot) = take_skid_overshoot_error() else {
            return result;
        };
        match result {
            Ok(_) => Err(Error::new(overshoot)),
            Err(error) => Err(error.context(overshoot)),
        }
    }

    fn finish_with_count<T>(self, result: Result<T, Error>) -> Result<(T, u64), Error> {
        if !self.enabled {
            return result.map(|value| (value, 0));
        }

        let count = reverie::take_skid_overshoot_count();
        match result {
            Ok(value) => Ok((value, count)),
            Err(error) if count > 0 => Err(error.context(SkidOvershootError::new(count))),
            Err(error) => Err(error),
        }
    }
}

// SaBRe's statistics and RPC support are selected only by `sabre`; the external
// loader and Detcore plugin must also be supplied. e9patch preprocessing requires
// external `e9tool`/`e9patch` artifacts. Neither path is enabled by default.
#[cfg(feature = "sabre")]
fn sabre_unavailable_reason() -> Option<String> {
    sabre_runtime_unavailable_reason()
}

#[cfg(not(feature = "sabre"))]
fn sabre_unavailable_reason() -> Option<String> {
    Some("the `sabre` feature is not enabled in this build; rebuild with `--features sabre` (or `--features third-party-backends`). This says nothing about whether SaBRe works on this machine -- it has not been checked".to_owned())
}

#[cfg(feature = "e9patch")]
fn e9patch_unavailable_reason() -> Option<String> {
    validate_tracing_environment()
        .err()
        .map(|error| error.to_string())
        .or_else(e9patch::unavailable_reason)
}

#[cfg(not(feature = "e9patch"))]
fn e9patch_unavailable_reason() -> Option<String> {
    Some("the `e9patch` feature is not enabled in this build; rebuild with `--features e9patch` (or `--features third-party-backends`). This says nothing about whether e9patch works on this machine -- it has not been checked".to_owned())
}

#[cfg(feature = "dbt")]
fn dbt_unavailable_reason() -> Option<String> {
    if !dynamorio_sdk_available() {
        return Some(
            "the DynamoRIO runtime was not found; build target/install_pkg, set HERMIT_INSTALL_DIR, or set DYNAMORIO_HOME/DynamoRIO_DIR to a valid SDK"
                .to_owned(),
        );
    }
    dbt_runtime_unavailable_reason()
}

#[cfg(not(feature = "dbt"))]
// TODO-HUMAN-REVIEW(PR-1150): Review the default-on DBT compile-time feature boundary.
fn dbt_unavailable_reason() -> Option<String> {
    Some(dbt_disabled_build_reason(
        option_env!("CARGO_PKG_NAME").unwrap_or("hermit"),
    ))
}

#[cfg(not(feature = "dbt"))]
fn dbt_disabled_build_reason(package_name: &str) -> String {
    if package_name == "hermit-run" {
        "the dbt backend is not included in this registry release; use a source build from https://github.com/rrnewton/hermit with `--features dbt`. The registry's `third-party-backends` feature does not include DBT. DynamoRIO availability on this machine has not been checked".to_owned()
    } else {
        "the `dbt` feature is not enabled in this source build; rebuild with `--features dbt` (or `--features third-party-backends`). DynamoRIO availability on this machine has not been checked".to_owned()
    }
}

#[cfg(feature = "sabre")]
const SABRE_BINARY_ENV: &str = "HERMIT_SABRE_BINARY";

#[cfg(feature = "sabre")]
fn is_executable_file(path: &Path) -> bool {
    fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

// TODO-HUMAN-REVIEW(PR-739): Review SaBRe loader discovery and executable validation.
#[cfg(feature = "sabre")]
fn resolve_sabre_binary_from(
    override_path: Option<&OsStr>,
    packaged_path: Option<&Path>,
    executable: &Path,
    path_env: &OsStr,
) -> Result<PathBuf, Error> {
    if let Some(requested) = override_path {
        if requested.is_empty() {
            return Err(anyhow!("{SABRE_BINARY_ENV} is empty"));
        }
        let path = PathBuf::from(requested);
        return is_executable_file(&path)
            .then_some(path.clone())
            .ok_or_else(|| {
                anyhow!(
                    "{SABRE_BINARY_ENV}={} is not an executable file",
                    path.display()
                )
            });
    }

    if let Some(path) = packaged_path
        && is_executable_file(path)
    {
        return Ok(path.to_path_buf());
    }

    let directory = executable
        .parent()
        .ok_or_else(|| anyhow!("Hermit executable has no parent directory"))?;
    let sibling = directory.join("sabre");
    let target_build = directory.parent().map(|target| target.join("sabre/sabre"));

    if is_executable_file(&sibling) {
        return Ok(sibling);
    }
    if let Some(candidate) = &target_build
        && is_executable_file(candidate)
    {
        return Ok(candidate.clone());
    }
    if !path_env.is_empty()
        && let Some(candidate) = std::env::split_paths(path_env)
            .map(|directory| directory.join("sabre"))
            .find(|candidate| is_executable_file(candidate))
    {
        return Ok(candidate);
    }

    Err(anyhow!(
        "SaBRe executable was not found in the Hermit installation, beside {}, or in PATH; set {} or {SABRE_BINARY_ENV}",
        executable.display(),
        hermit_resources::INSTALL_DIR_ENV
    ))
}

#[cfg(feature = "sabre")]
fn resolve_sabre_binary() -> Result<PathBuf, Error> {
    let executable =
        std::env::current_exe().context("failed to locate running Hermit executable")?;
    let override_path = std::env::var_os(SABRE_BINARY_ENV);
    let packaged_path = hermit_resources::resource("sabre")?;
    let path_env = std::env::var_os("PATH").unwrap_or_default();
    resolve_sabre_binary_from(
        override_path.as_deref(),
        packaged_path.as_deref(),
        &executable,
        &path_env,
    )
}

#[cfg(feature = "sabre")]
const SABRE_RPC_SOCKET_ENV: &str = "REVERIE_SABRE_HERMIT_RPC_SOCKET";
#[cfg(feature = "sabre")]
const SABRE_DETLOG_FORWARD_ENV: &str = "REVERIE_SABRE_HERMIT_FORWARD_DETLOG";
/// `reverie_sabre::TOOL_OUTPUT_ENV`: the descriptor reverie-sabre protects
/// for the in-guest tool's own output. Hermit does not link reverie-sabre.
#[cfg(feature = "sabre")]
const SABRE_TOOL_OUTPUT_ENV: &str = "REVERIE_SABRE_TOOL_OUTPUT_FD";
/// `detcore_liteinst::DETLOG_FORWARD_ENV`; that crate is a preload `cdylib`, so
/// Hermit cannot import the constant.
#[cfg(feature = "liteinst")]
const LITEINST_DETLOG_FORWARD_ENV: &str = "HERMIT_LITEINST_FORWARD_DETLOG";
/// `detcore_liteinst::DETLOG_FORWARD_POLICY_ENV`, for the same reason.
#[cfg(feature = "liteinst")]
const LITEINST_DETLOG_FORWARD_POLICY_ENV: &str = "HERMIT_LITEINST_FORWARD_DETLOG_POLICY";

/// Tells the in-guest LiteInst runtime this build's
/// [`detcore::config_wire_fingerprint`], which it compares with its own before
/// it decodes the configuration the coordinator sends. A runtime built from
/// another tree then fails naming both fingerprints and its own path, not with
/// a bare decoding error (<https://github.com/rrnewton/hermit/issues/3986>).
#[cfg(feature = "liteinst")]
fn announce_liteinst_config_fingerprint(command: &mut Command) {
    command.env(
        detcore::LITEINST_CONFIG_FINGERPRINT_ENV,
        detcore::config_wire_fingerprint(),
    );
}

/// Where in-guest LiteInst and SaBRe runs in this process forward Detcore's
/// DETLOG records; see [`forward_in_guest_detlogs_to`].
static IN_GUEST_DETLOG_SINK: Mutex<Option<std::os::fd::OwnedFd>> = Mutex::new(None);

/// Makes the in-guest LiteInst and SaBRe runs this process starts forward the
/// DETLOG records Detcore logs inside the guest to `socket`, the sending end of a Unix
/// `SOCK_SEQPACKET` pair. `hermit run --verify` gives each run its own pair and
/// reads the records from the other end into that run's log. Each record is one
/// message, one line (`detcore::detlog::forwarded_line`).
/// A record is forwarded when the CLI's filter logs INFO for its module (see
/// [`in_guest_detlog_forward_policy`]); without a socket none are forwarded.
#[doc(hidden)]
pub fn forward_in_guest_detlogs_to(socket: std::os::fd::OwnedFd) {
    *IN_GUEST_DETLOG_SINK.lock().unwrap() = Some(socket);
}

/// Hands the in-guest LiteInst runtime the socket chosen with
/// [`forward_in_guest_detlogs_to`], when `policy` forwards any record: an
/// inheritable duplicate whose number goes in the private opt-in variable, and
/// the encoded policy, which the guest's Detcore applies to each record by its
/// module as each in-process `detlog!` callsite asks tracing. The runtime
/// moves the socket to a descriptor Reverie reserves and protects from the
/// guest. The caller keeps the returned duplicate open until the guest has
/// started.
#[cfg(feature = "liteinst")]
fn request_liteinst_detlog_forwarding(
    command: &mut Command,
    policy: &detcore::detlog::ForwardPolicy,
) -> Result<Option<std::os::fd::OwnedFd>, Error> {
    command.env_remove(LITEINST_DETLOG_FORWARD_ENV);
    command.env_remove(LITEINST_DETLOG_FORWARD_POLICY_ENV);
    // Asking only whether INFO is on for `detcore`, and then forwarding
    // everything, kept records a target-scoped filter drops under ptrace
    // (info,detcore::random=warn) and could forward nothing where it keeps
    // some (warn,detcore::random=info).
    if !policy.forwards_any() {
        return Ok(None);
    }
    let Some(duplicate) = inheritable_in_guest_detlog_socket()? else {
        return Ok(None);
    };
    command.env(
        LITEINST_DETLOG_FORWARD_ENV,
        detcore::detlog::tool_output_env_value(duplicate.as_raw_fd())?,
    );
    command.env(LITEINST_DETLOG_FORWARD_POLICY_ENV, policy.encode());
    Ok(Some(duplicate))
}

/// An inheritable duplicate of the socket chosen with
/// [`forward_in_guest_detlogs_to`], or `None` when none was chosen. The
/// caller passes its number to the guest and keeps it open until the guest
/// has started.
#[cfg(any(feature = "sabre", feature = "liteinst"))]
fn inheritable_in_guest_detlog_socket() -> Result<Option<std::os::fd::OwnedFd>, Error> {
    let sink = IN_GUEST_DETLOG_SINK.lock().unwrap();
    let Some(socket) = sink.as_ref() else {
        return Ok(None);
    };
    // SAFETY: F_DUPFD on an open descriptor returns a new descriptor or -1.
    let duplicate = unsafe { libc::fcntl(socket.as_raw_fd(), libc::F_DUPFD, 3) };
    if duplicate < 0 {
        return Err(Error::new(std::io::Error::last_os_error())
            .context("duplicating the in-guest DETLOG forwarding socket"));
    }
    // SAFETY: fcntl returned a new descriptor this function now owns.
    Ok(Some(unsafe {
        std::os::fd::OwnedFd::from_raw_fd(duplicate)
    }))
}
#[cfg(feature = "sabre")]
const SABRE_PATH_EVIDENCE_ENV: &str = "HERMIT_SABRE_PATH_EVIDENCE";

/// The INFO policy for DETLOG records an in-guest Detcore (SaBRe's plugin,
/// in-guest LiteInst) forwards: the current subscriber's answer for every
/// target the CLI's `RUST_LOG` names (the text
/// `EffectiveFilter::from_default_env` builds every CLI subscriber from) and
/// for any other target.
#[cfg(any(feature = "sabre", feature = "liteinst"))]
fn in_guest_detlog_forward_policy() -> detcore::detlog::ForwardPolicy {
    let directives = std::env::var(tracing_subscriber::EnvFilter::DEFAULT_ENV).unwrap_or_default();
    detcore::detlog::ForwardPolicy::from_current_subscriber(&directives)
}
#[cfg(feature = "sabre")]
const SABRE_STAGING_DIRECTORY: &str = "/dev/shm";

#[cfg(feature = "sabre")]
struct StagedSabreProgram {
    path: PathBuf,
}

#[cfg(feature = "sabre")]
impl Drop for StagedSabreProgram {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

#[cfg(feature = "sabre")]
fn sabre_program_needs_neutral_name(program: &Path) -> bool {
    program
        .file_name()
        .is_some_and(|name| name.as_bytes().starts_with(b"ld"))
}

// TODO-HUMAN-REVIEW(PR-845): Review the neutral-name workaround for SaBRe's
// dynamic-loader prefix collision.
#[cfg(feature = "sabre")]
fn stage_sabre_program_in(
    program: &Path,
    staging_directory: &Path,
) -> Result<Option<StagedSabreProgram>, Error> {
    if !sabre_program_needs_neutral_name(program) {
        return Ok(None);
    }

    let path = staging_directory.join(format!("hermit-sabre-program-{}", std::process::id()));
    let mut source = fs::File::open(program).map_err(|error| {
        anyhow!(
            "failed to open SaBRe guest executable {} for neutral-name staging: {error}",
            program.display()
        )
    })?;
    let mut staged = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o700)
        .open(&path)
        .map_err(|error| {
            anyhow!(
                "failed to stage SaBRe guest executable {} as {}: {error}",
                program.display(),
                path.display()
            )
        })?;
    if let Err(error) = io::copy(&mut source, &mut staged) {
        let _ = fs::remove_file(&path);
        return Err(anyhow!(
            "failed to stage SaBRe guest executable {} as {}: {error}",
            program.display(),
            path.display()
        ));
    }
    drop(staged);

    Ok(Some(StagedSabreProgram { path }))
}

// TODO-HUMAN-REVIEW(PR-738): Review controller/plugin artifact separation.
#[cfg(feature = "sabre")]
fn sabre_runtime_library_path() -> io::Result<PathBuf> {
    if let Some(path) = hermit_resources::resource("libdetcore_sabre.so")?
        && path.is_file()
    {
        return Ok(path);
    }

    let executable = std::env::current_exe()?;
    let directory = executable.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "Hermit executable has no parent directory",
        )
    })?;
    [
        directory.join("libdetcore_sabre.so"),
        directory.join("deps/libdetcore_sabre.so"),
    ]
    .into_iter()
    .find(|path| path.is_file())
    .ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "libdetcore_sabre.so was not built beside {}",
                executable.display()
            ),
        )
    })
}

#[cfg(feature = "sabre")]
fn sabre_runtime_unavailable_reason() -> Option<String> {
    if let Err(error) = resolve_sabre_binary() {
        return Some(error.to_string());
    }
    sabre_runtime_library_path().err().map(|error| {
        format!(
            "the Detcore SaBRe plugin is unavailable: {error}; build detcore-sabre and hermit in the same target directory"
        )
    })
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-774): Review the bounded SaBRe RPC disconnect drain.
#[cfg(feature = "sabre")]
const SABRE_RPC_DISCONNECT_TIMEOUT: Duration = Duration::from_secs(1);

#[cfg(feature = "sabre")]
async fn wait_for_sabre_rpc_disconnects<T>(
    global: &Arc<T>,
    timeout: Duration,
) -> Result<(), usize> {
    let disconnected = tokio::time::timeout(timeout, async {
        while Arc::strong_count(global) > 1 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await;

    if disconnected.is_ok() {
        return Ok(());
    }

    let live_references = Arc::strong_count(global).saturating_sub(1);
    if live_references == 0 {
        Ok(())
    } else {
        Err(live_references)
    }
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-782): Review SaBRe RPC server shutdown errors.
#[cfg(feature = "sabre")]
async fn stop_sabre_rpc_server<E>(
    server_task: tokio::task::JoinHandle<Result<(), E>>,
) -> Result<(), Error>
where
    E: std::fmt::Display,
{
    server_task.abort();
    match server_task.await {
        Err(error) if error.is_cancelled() => Ok(()),
        Err(error) => Err(anyhow!("SaBRe coordinator task failed: {error}")),
        Ok(Err(error)) => Err(anyhow!("SaBRe coordinator server failed: {error}")),
        Ok(Ok(())) => Err(anyhow!("SaBRe coordinator server stopped unexpectedly")),
    }
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-789): Review complete SaBRe RPC shutdown ordering.
#[cfg(feature = "sabre")]
async fn shutdown_sabre_rpc<T, E>(
    server_task: tokio::task::JoinHandle<Result<(), E>>,
    global: &Arc<T>,
    timeout: Duration,
) -> Result<(), Error>
where
    E: std::fmt::Display,
{
    let server_result = stop_sabre_rpc_server(server_task).await;
    let disconnect_result = wait_for_sabre_rpc_disconnects(global, timeout).await;

    server_result?;
    disconnect_result.map_err(|live_references| {
        anyhow!("SaBRe coordinator stopped with {live_references} live RPC reference(s)")
    })
}

fn ensure_backend_dispatch(backend: Backend) -> Result<(), Error> {
    // The CLI probes ptrace readiness before entering its container; repeating
    // the namespace probe here would test nested namespaces instead of the host.
    if backend == Backend::Ptrace {
        return Ok(());
    }
    if backend == Backend::E9patch {
        return Err(anyhow!(
            "backend `e9patch` requires CLI preprocessing; library callers must use \
             e9patch::prepare and then select `ptrace`"
        ));
    }
    // KVM and DBT have dedicated dispatches (`run_kvm` and `run_dbt`); neither
    // must reach this generic rejection path.
    backend.ensure_available()?;
    Err(anyhow!(
        "backend `{}` has no Hermit dispatch implementation",
        backend.as_str()
    ))
}

/// Run one command with the Detcore tool executing inside a SaBRe plugin and
/// the single GlobalState held by this Hermit coordinator process.
// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-738): Review SaBRe coordinator lifetime and artifact loading.
#[cfg(feature = "sabre")]
async fn run_sabre(
    mut command: Command,
    config: DetConfig,
    print_summary: bool,
    print_summary_to_json_file: &Option<PathBuf>,
    capture_output: bool,
) -> Result<Output, Error> {
    let path_evidence_file = std::env::var_os(SABRE_PATH_EVIDENCE_ENV).map(PathBuf::from);
    // Whether this run passes the plugin a DETLOG socket (below), in which case
    // every Tool image must forward on it; see
    // detcore::Config::in_guest_detlog_forward_policy.
    let mut config = config;
    let policy = in_guest_detlog_forward_policy();
    config.in_guest_detlog_forward_policy = (policy.forwards_any()
        && IN_GUEST_DETLOG_SINK.lock().unwrap().is_some())
    .then(|| policy.encode());
    let sabre = resolve_sabre_binary()?;
    let plugin = sabre_runtime_library_path()
        .map_err(|error| anyhow!("failed to locate the Detcore SaBRe plugin: {error}"))?;
    let program = command.find_program().map_err(|error| {
        anyhow!(
            "failed to resolve SaBRe guest executable {:?}: {error}",
            command.get_program()
        )
    })?;
    let staged_program = stage_sabre_program_in(&program, Path::new(SABRE_STAGING_DIRECTORY))?;
    let launch_program = staged_program
        .as_ref()
        .map_or(program.as_path(), |staged| staged.path.as_path());

    let bootstrap_launch =
        sabre_bootstrap::Launch::new(Path::new(&sabre), launch_program, &config)?;

    // This runs after Hermit enters the guest mount namespace, where `/tmp`
    // names the container-visible temporary filesystem. Do not inherit a
    // host-side nested TMPDIR: validation commonly sets TMPDIR=/tmp/<run>/tmp,
    // but that parent path is hidden once the private `/tmp` mount is active.
    // The SaBRe plugin and coordinator must name the socket through the same
    // namespace-visible path.
    let socket_dir = tempfile::Builder::new()
        .prefix("hermit-sabre-rpc-")
        .tempdir_in(Path::new("/tmp"))?;
    let socket_path = socket_dir.path().join("coordinator.sock");
    let fallback_ready = Arc::new(AtomicBool::new(false));
    let global = Arc::new(detcore::GlobalState::init_global_state(&config).await);
    let server = reverie_rpc_transport::RpcServer::bind_with_readiness(
        &socket_path,
        global.clone(),
        config.clone(),
        fallback_ready.clone(),
    )
    .map_err(|error| anyhow!("failed to start SaBRe coordinator RPC: {error}"))?;
    let server_task = tokio::spawn(async move { server.serve().await });

    command.prepend_args([
        plugin.as_os_str(),
        OsStr::new("--"),
        launch_program.as_os_str(),
    ]);
    command.program(&sabre);
    command.env(SABRE_RPC_SOCKET_ENV, &socket_path);
    command.env_remove(sabre_bootstrap::ENVIRONMENT);
    if bootstrap_launch.initializes_random() {
        command.env(sabre_bootstrap::ENVIRONMENT, "1");
    }
    // Publish the configuration and clock RPC fingerprint for this coordinator.
    // The plugin is a separate artifact in the same target directory, so it can
    // be stale without looking it. Its check before connecting names the mismatch
    // instead of failing to decode Config or a later DetTime request field.
    // See detcore_model::config_wire_fingerprint.
    command.env(
        detcore::CONFIG_FINGERPRINT_ENV,
        detcore::config_wire_fingerprint(),
    );
    command.env_remove(SABRE_PATH_EVIDENCE_ENV);
    command.env_remove(SABRE_DETLOG_FORWARD_ENV);
    // Per emitting module, as each in-process `detlog!` callsite asks tracing:
    // asking only about `detcore` dropped every record a target-scoped
    // RUST_LOG (warn,detcore::random=info) keeps under ptrace.
    command.env_remove(SABRE_TOOL_OUTPUT_ENV);
    let detlog_policy = in_guest_detlog_forward_policy();
    // Kept open until the guest has started (it inherits its own copy).
    let mut _detlog_descriptor = None;
    if detlog_policy.forwards_any() {
        command.env(SABRE_DETLOG_FORWARD_ENV, detlog_policy.encode());
        // With a socket (`hermit run --verify`), the plugin sends each record
        // on it from a descriptor reverie-sabre protects from the guest, and
        // the coordinator writes each thread's records into the log at that
        // thread's scheduler turns, as for in-guest LiteInst. Without one it
        // writes them to the guest's standard error.
        if let Some(socket) = inheritable_in_guest_detlog_socket()? {
            command.env(
                SABRE_TOOL_OUTPUT_ENV,
                detcore::detlog::tool_output_env_value(socket.as_raw_fd())?,
            );
            _detlog_descriptor = Some(socket);
        }
    }
    command.env_remove("SABRE_BINARY");
    command.env_remove("SABRE_PLUGIN");
    // Always collect: the statistics page is a descriptor the guest can see,
    // so creating it only when a summary or DEBUG log is requested would make
    // those flags change the guest's view. The plugin takes the variable out
    // of the guest environment, and the descriptor sits above the plugin's
    // fixed RPC socket.
    let stats = reverie_sabre_stats::SabreStats::create(reverie::BackendStatsRequest::ENABLED)
        .map_err(|error| anyhow!("failed to create the SaBRe statistics page: {error}"))?
        .expect("an enabled request creates the statistics page");
    command.env(
        reverie_sabre_stats::BACKEND_STATS_ENV,
        stats.raw_fd().to_string(),
    );

    // THE SOCKET PATH IS DELIBERATELY RANDOM AND MUST NOT BE COMPARED.
    //
    // `tempfile` appends a random 6-character suffix to the directory above, so
    // the absolute path differs on every launch by design -- that is what keeps
    // concurrent hermit invocations on a shared host from binding the same
    // coordinator socket. Emitting it at INFO put per-launch randomness into the
    // INFO stream, which `--verify-strict` compares:
    //
    //   Mismatch at log messages 2 (run 1) and 2 (run 2):
    //   < socket=/tmp/hermit-sabre-rpc-Gw6tUi/coordinator.sock
    //   > socket=/tmp/hermit-sabre-rpc-7SZ94p/coordinator.sock
    //
    // That fails EVERY SaBRe verify cell before the guest executes an
    // instruction, for a host-side value with no guest-visible content. The
    // defect was in the oracle, not the backend.
    //
    // WHY THE VALUE IS DROPPED FROM INFO RATHER THAN MADE DETERMINISTIC: the two
    // `--verify` runs execute in SEPARATE CHILD PROCESSES, so any path stable
    // across both would have to be derived from state shared between them. That
    // trades `tempfile`'s guaranteed-fresh directory for a reused one, and a
    // run1 that dies holding the socket would then make run2 fail to bind -- a
    // real new failure mode in exchange for a cosmetic field. The path keeps its
    // randomness; only the compared stream stops carrying it.
    //
    // This is NOT a comparator relaxation. No normalization envelope is widened,
    // so any future randomness leaking into this banner still fails the
    // comparison. The full path remains available one level down.
    tracing::debug!(
        target: "hermit::sabre",
        socket = %socket_path.display(),
        "SaBRe coordinator RPC socket (per-launch temp path, deliberately not at INFO)",
    );
    tracing::info!(
        target: "hermit::sabre",
        guest = %program.display(),
        plugin = %plugin.display(),
        "launching Detcore guest through SaBRe with coordinator RPC",
    );

    let supervised = match sabre_ptrace::run(
        command.into_std_lossy(),
        PathBuf::from(&sabre),
        plugin.clone(),
        fallback_ready,
        global.clone(),
        bootstrap_launch,
        stats.clone(),
        capture_output,
    )
    .await
    {
        Ok(supervised) => supervised,
        Err(error) => {
            global.release_all_physical_process_exits();
            global.force_shutdown_with_error();
            let _ = shutdown_sabre_rpc(server_task, &global, SABRE_RPC_DISCONNECT_TIMEOUT).await;
            return Err(error);
        }
    };
    // The supervisor returns only after every tracee reached a final kernel wait status. Release
    // the root process's barrier (and any intentionally unreaped child barriers) before scheduler
    // shutdown; no guest thread remains that could race timer fast-forward here.
    global.release_all_physical_process_exits();
    // A SaBRe execution in which no guest thread ever reached the coordinator
    // is an execution in which Detcore was never loaded as a Reverie tool: the
    // guest ran on bare Linux. Treat it exactly like a failed run so the
    // scheduler is torn down here instead of blocking forever in `clean_up`
    // below, waiting for a guest thread that will never register.
    let detcore_never_engaged = !supervised.path_evidence.guest_rpc_observed;
    let requires_forced_shutdown = !supervised.status.success() || detcore_never_engaged;
    if requires_forced_shutdown {
        global.force_shutdown_with_error();
    }
    // Emit one unconditional, machine-readable per-run fact instead of a
    // free-form warning. SaBRe loads the guest interpreter before the Detcore
    // plugin, so pre-plugin loader syscalls are structurally absent from its
    // observation envelope. Versioning that fact lets scorecard consumers
    // reject old or incomplete records rather than infer coverage from silence.
    //
    // Keep this in the controller-diagnostic stream rather than writing
    // directly to the process stderr that also carries captured guest stderr.
    // WARN is Hermit's default tracing level, and `--log-file` can therefore
    // separate the unconditional controller fact from guest output without a
    // comparator exception. A missing record remains distinct from a record
    // whose counters are zero.
    let backend_evidence = sabre_backend_evidence_line(&supervised.path_evidence);
    tracing::warn!(target: "hermit::sabre", "{backend_evidence}");
    if let Some(path) = path_evidence_file {
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&path)
            .map_err(|error| {
                anyhow!(
                    "failed to open SaBRe path evidence {}: {error}",
                    path.display()
                )
            })?;
        serde_json::to_writer(&mut file, &supervised.path_evidence)?;
        writeln!(file)?;
    }
    let output = Output {
        status: supervised.status.into(),
        stdout: supervised.stdout,
        stderr: supervised.stderr,
    };

    shutdown_sabre_rpc(server_task, &global, SABRE_RPC_DISCONNECT_TIMEOUT).await?;
    let mut global = Arc::try_unwrap(global).map_err(|global| {
        anyhow!(
            "SaBRe coordinator stopped with {} live RPC reference(s)",
            Arc::strong_count(&global) - 1
        )
    })?;
    if requires_forced_shutdown {
        global.cancel_internal_scheduler().await;
    }
    let dispatch_stats = backend_stats::report(
        Backend::Sabre,
        backend_stats::request(print_summary_to_json_file),
        &stats,
    );
    global
        .clean_up_with_dispatch_stats(print_summary, print_summary_to_json_file, dispatch_stats)
        .await;
    tracing::info!(
        target: "hermit::sabre::fallback",
        ptrace_fallback_sites = supervised.path_evidence.ptrace_fallback_sites,
        trusted_shared_object_sites = supervised.path_evidence.trusted_shared_object_sites,
        guest_rpc_observed = supervised.path_evidence.guest_rpc_observed,
        "SaBRe ptrace fallback completed",
    );
    if detcore_never_engaged {
        return Err(Error::new(BackendRunFailure)
            .context(sabre_uninstrumented_guest_message(&output.status)));
    }
    Ok(output)
}

#[cfg(not(feature = "sabre"))]
async fn run_sabre(
    _command: Command,
    _config: DetConfig,
    _print_summary: bool,
    _print_summary_to_json_file: &Option<PathBuf>,
    _capture_output: bool,
) -> Result<Output, Error> {
    Backend::Sabre.ensure_available()?;
    unreachable!("SaBRe availability must fail when the feature is disabled");
}

/// Classify the existing SaBRe reach evidence without collapsing "never
/// engaged" into the same zero-valued state as a fully exercised run.
#[cfg(feature = "sabre")]
fn sabre_reach_state(guest_rpc_observed: bool, ptrace_fallback_sites: usize) -> &'static str {
    match (guest_rpc_observed, ptrace_fallback_sites) {
        (false, _) => "no-detcore-reached",
        (true, 1..) => "degraded-ptrace-fallback",
        (true, 0) => "sabre-exercised",
    }
}

/// Produce the versioned SaBRe backend fact consumed by compatibility reports.
///
/// `preplugin_coverage=absent` is a property of SaBRe's launch order: the guest
/// interpreter runs before the Detcore plugin is loaded. It is recorded rather
/// than inferred from counters because the ptrace fallback cannot observe that
/// interval either.
///
/// Preserve the originating #1725 measurement as provenance for that field:
/// on its measurement host, `/bin/true` produced 3 COMMITs under SaBRe versus
/// 14 under ptrace; the 11 absent records were the loader's libc.so.6 path
/// resolution. The launch-order fact, not that host-specific count, is the
/// runtime contract.
#[cfg(feature = "sabre")]
fn sabre_backend_evidence_line(evidence: &sabre_ptrace::PathEvidence) -> String {
    format!(
        ":: Backend: sabre static rewriting + ptrace runtime; run_mode=run; \
         evidence_schema={}; preplugin_coverage=absent; ptrace_fallback_sites={}; \
         trusted_shared_object_sites={}; guest_rpc_observed={}; reach_state={}",
        evidence.schema,
        evidence.ptrace_fallback_sites,
        evidence.trusted_shared_object_sites,
        evidence.guest_rpc_observed,
        sabre_reach_state(evidence.guest_rpc_observed, evidence.ptrace_fallback_sites),
    )
}

/// Explain a SaBRe run whose guest never reached the Detcore coordinator.
///
/// `guest_rpc_observed` is set by the coordinator RPC listener the first time a
/// SaBRe-loaded guest connects. When it stays false the guest completed without
/// Detcore intercepting anything, so the run carries no determinism guarantee
/// whatsoever -- its timing, scheduling, PIDs, and clock reads all came from the
/// host. Reporting that as a successful `hermit run` would be a fail-open
/// determinism hole, so the caller turns it into a hard error and this function
/// supplies the diagnosis.
///
/// The dominant cause is a guest whose syscall sites SaBRe never rewrote. A
/// statically linked ELF is the sharp edge: it has no dynamic loader and no
/// shared library through which SaBRe could regain control, so an unrewritten
/// static client runs entirely on bare Linux with no second chance.
#[cfg(feature = "sabre")]
fn sabre_uninstrumented_guest_message(status: &ExitStatus) -> String {
    format!(
        "the SaBRe backend finished ({status:?}) without the guest ever reaching the Detcore \
         coordinator: no syscall was intercepted, so this run applied no determinization at all \
         and its result is not a Hermit guarantee. This means SaBRe rewrote no syscall site in \
         the guest -- most often a statically linked ELF, whose syscall sites SaBRe must patch \
         in the client image itself because there is no dynamic loader to intercept."
    )
}

/// Guest-physical memory available to the single-process KVM personality.
// The KVM personality is a sparse MAP_NORESERVE address space. QEMU needs room
// for its own ELF mappings in addition to the nested machine's RAM mapping.
const KVM_GUEST_MEMORY_BYTES: usize = 1024 * 1024 * 1024;

/// Maximum `#!` interpreter indirection levels, matching the Linux kernel's
/// `BINPRM_MAX_RECURSION` limit for chained script interpreters.
const MAX_SHEBANG_DEPTH: usize = 4;

/// Resolve `#!` interpreter scripts before the reverie-kvm ELF loader runs.
///
/// The KVM ELF loader can only map ELF images, so a guest program that is
/// actually a `#!`-script (for example `/usr/local/bin/file` -> `#!/bin/bash`,
/// or `/usr/bin/pkg-config` -> `#!/usr/bin/sh`) must be rewritten to launch its
/// interpreter, exactly as the kernel's `execve(2)` `binfmt_script` handler
/// does. On success the returned image is an ELF and `argv` has the interpreter,
/// its shebang arguments, and the script path prepended in kernel order:
/// `[interp, shebang_args.., script_path, <original argv[1..]>]`.
///
/// The interpreter line is parsed with hermit's shared [`Shebang`] so the KVM
/// backend matches how the ptrace backend and recorder treat `#!`-scripts.
fn resolve_kvm_shebang(
    resolved_program: &Path,
    mut argv: Vec<String>,
) -> Result<(PathBuf, Vec<String>, Vec<u8>), Error> {
    let mut load_path = resolved_program.to_path_buf();
    let mut image = fs::read(&load_path)
        .map_err(|error| anyhow!("failed to read KVM guest executable {load_path:?}: {error}"))?;

    let mut depth = 0;
    while image.starts_with(b"#!") {
        depth += 1;
        if depth > MAX_SHEBANG_DEPTH {
            return Err(anyhow!(
                "too many levels of `#!` interpreter indirection loading {resolved_program:?}"
            ));
        }
        let (interpreter, shebang_args) = Shebang::from_buf(&image)
            .ok_or_else(|| anyhow!("malformed `#!` interpreter line in {load_path:?}"))?
            .into_parts();
        let interpreter_str = interpreter
            .to_str()
            .ok_or_else(|| anyhow!("non-UTF-8 `#!` interpreter path in {load_path:?}"))?
            .to_owned();

        // Rewrite argv in kernel order. The prior argv[0] (the script's own
        // name) is dropped on the first level; on deeper levels the previous
        // interpreter path is preserved as a positional argument, matching
        // `binfmt_script`.
        let mut rewritten = Vec::with_capacity(argv.len() + shebang_args.len() + 2);
        rewritten.push(interpreter_str);
        for arg in &shebang_args {
            rewritten.push(
                arg.to_str()
                    .ok_or_else(|| anyhow!("non-UTF-8 `#!` interpreter argument in {load_path:?}"))?
                    .to_owned(),
            );
        }
        rewritten.push(load_path.to_string_lossy().into_owned());
        rewritten.extend_from_slice(&argv[1..]);
        argv = rewritten;

        load_path = interpreter;
        image = fs::read(&load_path).map_err(|error| {
            anyhow!(
                "failed to read `#!` interpreter {load_path:?} for {resolved_program:?}: {error}"
            )
        })?;
    }

    Ok((load_path, argv, image))
}

/// The container replaces this host directory with a private, empty tmpfs, so
/// nothing beneath it is visible to the guest. Kept next to the diagnostic that
/// depends on it; `run.rs` has its own `TMP_DIR` for the mount itself.
const CONTAINER_REPLACED_TMP: &str = "/tmp";

/// Explain why a working directory that plainly exists on the host could not be
/// resolved for the guest.
///
/// ⚠️ THIS FUNCTION EXISTS BECAUSE THE BARE ERROR CAUSED A MISDIAGNOSIS, not to
/// be tidy. `fs::canonicalize` runs after the container's mounts are applied, and
/// the container bind-mounts a fresh empty tmpfs over `/tmp`
/// (`bin/hermit/container.rs`). So a cwd beneath `/tmp` is genuinely absent from
/// the guest's mount namespace and `canonicalize` returns a perfectly correct
/// `NotFound` -- for a directory the user can `ls`.
///
/// Reported 2026-08-25 as "run_kvm_ cli tests fail from a git worktree". They do
/// not. Measured across five working directories with one binary: a PLAIN
/// directory under `/tmp` with no git in it FAILS, and a linked git worktree
/// under `/home` WORKS. The only property that tracks the failure is "the cwd is
/// under /tmp"; `/tmp` itself resolves because the mount POINT exists. The git
/// hypothesis cost real investigation time, and it came from this message saying
/// only "No such file or directory".
///
/// It mattered more than a stray report because the historical landing recipe
/// placed detached worktrees below `/tmp`, exactly where this fires. Current
/// dev-hermit worktrees are created through wrkslots below `worktrees/`, but the
/// diagnostic remains necessary for arbitrary callers and older checkouts.
///
/// The message names the mechanism and both ways out. It deliberately does NOT
/// silently fall back to `/`: changing the guest's working directory out from
/// under a caller who asked for a specific one would trade a loud, correct
/// failure for a quiet, wrong success.
fn kvm_cwd_resolution_error(requested_cwd: &Path, error: &std::io::Error) -> Error {
    let under_replaced_tmp = requested_cwd.starts_with(CONTAINER_REPLACED_TMP)
        && requested_cwd != Path::new(CONTAINER_REPLACED_TMP);
    if error.kind() == std::io::ErrorKind::NotFound && under_replaced_tmp {
        anyhow!(
            "failed to resolve KVM guest working directory {:?}: {error}\n\
             \n\
             This directory may well exist on the host. The container replaces \
             {CONTAINER_REPLACED_TMP} with a private, empty tmpfs, so nothing beneath \
             {CONTAINER_REPLACED_TMP} is visible to the guest, and resolving the working \
             directory happens inside that mount namespace.\n\
             Either run from a directory outside {CONTAINER_REPLACED_TMP}, or pass \
             --workdir with a path that exists inside the guest.",
            requested_cwd
        )
    } else {
        anyhow!(
            "failed to resolve KVM guest working directory {:?}: {error}",
            requested_cwd
        )
    }
}

/// Bind KVM's raw mountinfo and fdinfo to the same filesystem namespace.
///
/// The CLI supplies IdentityGuard provenance from the completed container.
/// Recapture that full row/parent order before ELF installation, which snapshots
/// the same namespace in the backend, and refuse stale producer provenance.
/// Library and no-namespace callers without provenance retain this capture.
/// KVM does not permit guest namespace changes. Later guest-visible mountinfo
/// views still use Detcore's existing ordered-subset validation.
fn prepare_kvm_mountinfo_config(
    config: &mut DetConfig,
    capture: impl FnOnce() -> Result<Vec<u64>, Error>,
) -> Result<(), Error> {
    if !config.mountinfo_mount_ids_captured
        && (!config.mountinfo_mount_ids.is_empty() || !config.mount_id_assignment_order.is_empty())
    {
        anyhow::bail!("KVM mount identities have no producer-captured namespace");
    }
    let order = capture().context("failed to capture the KVM backend mount namespace")?;
    if config.mountinfo_mount_ids_captured {
        if config.mountinfo_mount_ids != order {
            anyhow::bail!(
                "KVM producer-captured mount identities do not match the backend namespace"
            );
        }
    } else {
        config.mountinfo_mount_ids = order;
        config.mountinfo_mount_ids_captured = true;
    }
    // The backend now exposes the actual namespace's devices. There is no
    // synthetic 0:1 root device to translate; stat and mountinfo use the same
    // raw identities before entering Detcore's shared DevicePool.
    config.mountinfo_device_rewrites.clear();
    Ok(())
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(https://github.com/rrnewton/hermit/issues/3695): Review
// host metadata timestamps for KVM runs.
/// Whether a KVM run asks the backend for the timestamps the kernel stored
/// rather than its fixed value.
///
/// With virtual metadata, Detcore rewrites every timestamp that a stat call
/// returns to the guest, and it reads the stored host values for its own
/// checks: `set_file_times` confirms that `utimensat` stored the requested
/// mtime. KVM replaces those timestamps with a fixed value unless the run asks
/// for the stored ones. The backend refuses this setting unless Detcore owns
/// every guest thread, which `run_kvm` never opts out of. The stored values sit
/// in the guest's buffer until Detcore rewrites them, so the run must also
/// sequentialize threads: then no other guest thread runs in that interval.
fn kvm_reports_stored_metadata_timestamps(config: &DetConfig) -> bool {
    config.virtualize_metadata && config.sequentialize_threads
}

/// Dispatch a command onto the real reverie-kvm Tool runtime.
async fn run_kvm(
    command: &Command,
    mut config: DetConfig,
    print_summary: bool,
    print_summary_to_json_file: &Option<PathBuf>,
    capture_output: bool,
) -> Result<Output, Error> {
    let dispatch_started = Instant::now();
    prepare_kvm_mountinfo_config(&mut config, capture_mountinfo_identity_order)?;
    let stdin = if capture_output {
        let (snapshot_reserved, snapshot) = output_backend_stdin_reservation()?;
        if snapshot_reserved {
            snapshot
        } else {
            // Public KVM output-capture callers do not pass through the CLI's
            // verify setup. Preserve their existing stdin behavior instead of
            // silently replacing it with /dev/null merely because output is
            // captured.
            reserved_kvm_stdin()?
        }
    } else {
        reserved_kvm_stdin()?
    };
    let requested_cwd = command
        .get_current_dir()
        .map(Path::to_owned)
        .unwrap_or(std::env::current_dir()?);
    let cwd = fs::canonicalize(&requested_cwd)
        .map_err(|error| kvm_cwd_resolution_error(&requested_cwd, &error))?;
    let program = command
        .get_program()
        .to_str()
        .ok_or_else(|| anyhow!("KVM guest executable path is not valid UTF-8"))?
        .to_owned();
    if !cwd.is_dir() {
        return Err(anyhow!(
            "KVM guest working directory is not a directory: {:?}",
            cwd
        ));
    }
    let resolved_program = command.find_program().map_err(|error| {
        anyhow!("failed to resolve KVM guest executable {program:?} in the guest PATH: {error}")
    })?;
    let mut argv = Vec::with_capacity(1 + command.get_args().count());
    argv.push(program.clone());
    for argument in command.get_args() {
        argv.push(
            argument
                .to_str()
                .ok_or_else(|| anyhow!("KVM guest argument is not valid UTF-8"))?
                .to_owned(),
        );
    }

    // Rewrite `#!`-scripts to their interpreter before the ELF loader sees them.
    let (_interpreter_path, argv, image) = resolve_kvm_shebang(&resolved_program, argv)?;
    // After shebang resolution the executable is the interpreter (argv[0]).
    let program = argv.first().cloned().unwrap_or(program);
    let envp = command
        .get_captured_envs()
        .into_iter()
        .map(|(key, value)| {
            let key = key
                .to_str()
                .ok_or_else(|| anyhow!("KVM guest environment key is not valid UTF-8"))?;
            let value = value
                .to_str()
                .ok_or_else(|| anyhow!("KVM guest environment value is not valid UTF-8"))?;
            Ok(format!("{key}={value}"))
        })
        .collect::<Result<Vec<_>, Error>>()?;
    tracing::info!(
        target: "hermit::kvm",
        program = %program,
        argv = ?argv,
        cwd = %cwd.display(),
        env_count = envp.len(),
        "launching guest through reverie-kvm",
    );
    let argv = argv.iter().map(String::as_str).collect::<Vec<_>>();
    let envp = envp.iter().map(String::as_str).collect::<Vec<_>>();

    let setup_started = Instant::now();
    // `prepare_backend_config` already copied `KvmBackend::capabilities()`,
    // which reports the backend's own CPUID table and that, of the madvise
    // advice with guest-visible semantics, it implements only MADV_DONTNEED.
    // KVM does not enter Hermit's UTS namespace, so Detcore must provide the
    // same synthetic identity that the namespace-backed ptrace path exposes.
    // TODO-HUMAN-REVIEW(PR-998): Review KVM UTS namespace parity.
    config.has_uts_namespace = false;
    let random_seed = config.rng_seed();
    let mut backend = reverie_kvm::KvmBackend::new_with_stdin(KVM_GUEST_MEMORY_BYTES, stdin)
        .map_err(|error| anyhow!("failed to initialize reverie-kvm: {error}"))?;
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1120): Review KVM's canonical Detcore root identity.
    backend
        .set_root_pid(detcore::ROOT_DETPID.as_raw())
        .map_err(|error| anyhow!("failed to configure KVM root PID: {error}"))?;
    // Guests see their task IDs, so number them as the ptrace reference does
    // (one extra ID per task for its tracer thread; see reverie::task_ids).
    backend
        .set_ids_per_task(reverie::task_ids::HERMIT_PTRACE_IDS_PER_TASK)
        .map_err(|error| anyhow!("failed to configure KVM task IDs: {error}"))?;
    // The ptrace launcher execs the path `find_program` resolved, so that is
    // the filename Linux records in AT_EXECFN and copies to the top of the
    // stack, including for a `#!` script whose argv now starts with the
    // interpreter.
    backend
        .install_static_elf_with_exec_context(
            &image,
            resolved_program.as_os_str(),
            &argv,
            &envp,
            &cwd,
        )
        .map_err(|error| anyhow!("failed to load KVM guest executable {program:?}: {error}"))?;
    backend
        .set_random_seed(random_seed)
        .map_err(|error| anyhow!("failed to configure KVM guest random seed: {error}"))?;
    // The KVM backend defaults to Tool-owned guest threads: CLONE_THREAD
    // workers are driven through the Detcore tool loop and their futex/CLEARTID
    // synchronization routes to Detcore, matching the golden ptrace model
    // ("follow children"). Detcore registers every cloned thread as a tool
    // thread (detcore/src/syscalls/threads.rs), so the backend must stay in
    // that mode: host-owned threads would split worker execution and futex
    // ownership across two synchronization domains, which deadlocks
    // pthread_join.
    if kvm_reports_stored_metadata_timestamps(&config) {
        backend
            .set_host_metadata_timestamps(true)
            .map_err(|error| anyhow!("failed to configure KVM metadata timestamps: {error}"))?;
    }

    let execution_started = Instant::now();
    let completion = backend
        .run_static_elf_with_tool_completion::<Detcore>(config, capture_output)
        .await
        .map_err(|error| kvm_execution_error(error, None))?;
    let cleanup_started = Instant::now();
    let result =
        finish_kvm_tool_completion(completion, print_summary, print_summary_to_json_file).await;
    let teardown_started = Instant::now();
    // Drop explicitly so the host's KVM VM teardown cost remains observable.
    drop(backend);
    let teardown_finished = Instant::now();
    let (code, stdout, stderr) = result?;

    // Every field below is a host wall-clock duration. Keep this diagnostic at
    // DEBUG so it remains available for backend profiling without entering the
    // INFO stream exposed to canonical comparison. KVM's built-in verification
    // is still output/status-only; this classification does not claim KVM L2.
    tracing::debug!(
        target: "hermit::kvm",
        prepare_us = setup_started.duration_since(dispatch_started).as_micros() as u64,
        setup_us = execution_started.duration_since(setup_started).as_micros() as u64,
        execution_us = cleanup_started.duration_since(execution_started).as_micros() as u64,
        cleanup_us = teardown_started.duration_since(cleanup_started).as_micros() as u64,
        teardown_us = teardown_finished.duration_since(teardown_started).as_micros() as u64,
        lifecycle_us = teardown_finished.duration_since(dispatch_started).as_micros() as u64,
        "reverie-kvm lifecycle phase timings",
    );

    if !capture_output {
        std::io::stdout().write_all(&stdout)?;
        std::io::stderr().write_all(&stderr)?;
    }

    Ok(Output {
        status: ExitStatus::Exited(code),
        stdout,
        stderr,
    })
}

// A runtime error still owns GlobalState. Finish its terminal daemon before
// returning the typed cause to the CLI's error classifier.
async fn finish_kvm_tool_completion(
    completion: reverie_kvm::ToolRunCompletion<detcore::GlobalState>,
    print_summary: bool,
    print_summary_to_json_file: &Option<PathBuf>,
) -> Result<(i32, Vec<u8>, Vec<u8>), Error> {
    match completion.result {
        Ok(output) => {
            completion
                .global_state
                .clean_up(print_summary, print_summary_to_json_file)
                .await;
            Ok(output)
        }
        Err(error) => {
            let cleanup = completion
                .global_state
                .clean_up_after_backend_failure()
                .await;
            Err(kvm_execution_error(error, Some(cleanup)))
        }
    }
}

fn kvm_execution_error(
    primary: reverie_kvm::Error,
    cleanup: Option<detcore::BackendFailureCleanup>,
) -> Error {
    // The manifest runner retains the first Error line. Keep the cause there,
    // as well as the typed backend and optional scheduler errors in the chain.
    let message = format!("KVM guest execution failed: {primary}");
    let mut error = Error::new(primary).context(BackendRunFailure);
    if let Some(cleanup) = cleanup {
        if let Err(recording) = cleanup.preemption_recording {
            error = error.context(format!("partial preemption recording failed: {recording}"));
        }
        if let Err(scheduler) = cleanup.scheduler {
            error = error.context(scheduler);
        }
    }
    error.context(message)
}

#[cfg(test)]
mod kvm_failure_tests;

#[cfg(all(test, feature = "kvm-execution-tests"))]
mod kvm_execution_tests;

// TODO-HUMAN-REVIEW(PR-743): Review bounded relaunch before DBT guest execution.
#[cfg(feature = "dbt")]
fn dbt_client_thread_start_failed(status: &std::process::ExitStatus) -> bool {
    status.code() == Some(reverie_dbt::CLIENT_THREAD_START_FAILURE_EXIT_CODE)
}

/// Keys that `HERMIT_DBT_DETCONFIG` still carries although [`DetConfig`] no
/// longer has the switches they named. Each entry is the key, its fixed value,
/// and the key it preceded in the old field order.
///
/// These are serialization constants. Nothing reads them back: the admitted
/// DBT standalone path decodes the variable with
/// [`detcore::decode_backend_config_json`], whose legacy reader ignores these
/// keys. The values are the only ones the deleted switches ever took on any
/// backend.
const LEGACY_DBT_DETCONFIG_KEYS: [(&str, bool, &str); 3] = [
    (
        "use_thread_local_clock_reads",
        false,
        "detect_host_clock_futex_timeouts",
    ),
    (
        "backend_serializes_fork_children",
        false,
        "backend_tracks_process_children",
    ),
    (
        "backend_dispatches_thread_tools",
        true,
        "backend_tracks_process_children",
    ),
];

/// Serialize `config` for the DBT runtime's `HERMIT_DBT_DETCONFIG` variable.
///
/// This is not plain `serde_json::to_string` because the variable sits in the
/// guest's own environment. A guest can read it through `getenv`, `printenv`
/// or `/proc/self/environ`, so its bytes are guest-visible. Two later changes
/// to [`DetConfig`] would otherwise have changed what every DBT guest
/// observes, and each is undone here:
///
/// - The backend facts moved into [`DetConfig::backend`].
///   [`detcore::to_legacy_backend_json`] writes them back out as the fifteen
///   separate keys they were, at the position the object carried them.
/// - Three always-constant switches were deleted, which would have shortened
///   the variable by 117 bytes. The keys in `LEGACY_DBT_DETCONFIG_KEYS` are
///   re-inserted at their old positions.
///
/// When the legacy keys express every backend capability exactly, the output
/// is byte-identical to what the same config produced before either change.
/// That is pinned by golden bytes in the tests below. Otherwise a versioned
/// envelope carries the exact capabilities alongside the legacy config.
///
/// Coordinated DBT startup receives the authoritative typed Config through the
/// RPC handshake. This JSON remains guest-visible and supplies the admitted
/// standalone fallback, so its representable bytes still need preservation.
/// Remove this shim only when that variable no longer reaches the guest.
#[doc(hidden)]
pub fn dbt_detconfig_json(config: &DetConfig) -> serde_json::Result<String> {
    let legacy = insert_legacy_dbt_detconfig_keys(&detcore::to_legacy_backend_json(config)?)?;
    detcore::encode_backend_config_json(legacy, config.backend)
}

/// Re-insert [`LEGACY_DBT_DETCONFIG_KEYS`] into a serialized [`DetConfig`].
///
/// The insertion works on the parsed top-level object, never on substrings,
/// so string values that happen to contain a key name cannot move it. It
/// refuses, rather than guessing, when an anchor key is missing or repeated or
/// when a legacy key is already present.
fn insert_legacy_dbt_detconfig_keys(serialized: &str) -> serde_json::Result<String> {
    use serde::de::MapAccess;
    use serde::ser::Error as _;
    use serde::ser::SerializeMap;
    use serde_json::value::RawValue;

    /// The top-level members of a JSON object in document order, each value
    /// kept as its exact serialized bytes.
    struct Members<'a>(Vec<(&'a str, &'a RawValue)>);

    impl<'de> serde::Deserialize<'de> for Members<'de> {
        fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            struct MembersVisitor;
            impl<'de> serde::de::Visitor<'de> for MembersVisitor {
                type Value = Members<'de>;
                fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                    formatter.write_str("a serialized Detcore config object")
                }
                fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                    let mut members = Vec::new();
                    while let Some(member) = map.next_entry()? {
                        members.push(member);
                    }
                    Ok(Members(members))
                }
            }
            deserializer.deserialize_map(MembersVisitor)
        }
    }

    impl serde::Serialize for Members<'_> {
        fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            let mut map = serializer.serialize_map(None)?;
            for (key, value) in &self.0 {
                for (legacy_key, fixed_value, anchor) in LEGACY_DBT_DETCONFIG_KEYS {
                    if anchor == *key {
                        map.serialize_entry(legacy_key, &fixed_value)?;
                    }
                }
                map.serialize_entry(key, value)?;
            }
            map.end()
        }
    }

    let members: Members<'_> = serde_json::from_str(serialized)?;
    for (legacy_key, _, anchor) in LEGACY_DBT_DETCONFIG_KEYS {
        let anchors = members.0.iter().filter(|(key, _)| *key == anchor).count();
        if anchors != 1 {
            return Err(serde_json::Error::custom(format!(
                "cannot place legacy DBT config key {legacy_key:?}: its anchor key {anchor:?} \
                 appears {anchors} times, expected exactly once"
            )));
        }
        if members.0.iter().any(|(key, _)| *key == legacy_key) {
            return Err(serde_json::Error::custom(format!(
                "legacy DBT config key {legacy_key:?} is already present; it must come only \
                 from the fixed legacy table"
            )));
        }
    }
    serde_json::to_string(&members)
}

#[cfg(test)]
mod dbt_detconfig_tests {
    use std::path::PathBuf;

    use clap::Parser;
    use detcore_model::config::MountInfoRootRewrite;

    use super::Backend;
    use super::DetConfig;
    use super::dbt_detconfig_json;
    use super::insert_legacy_dbt_detconfig_keys;
    use super::prepare_backend_config;

    /// `HERMIT_DBT_DETCONFIG` bytes for each config below, captured with
    /// `serde_json::to_string` from a build of
    /// 806cf2fa38d13b806195d54aeba22d282d79f33d, which still had the legacy
    /// switches. Any later change to `DetConfig`'s serialized fields also
    /// changes this guest-visible variable, so this test is meant to fail on
    /// it and make that change a deliberate one.
    ///
    /// One deliberate change since that capture: the empty
    /// `fdinfo_unlisted_mount_ids` key was renamed `mount_id_assignment_order`
    /// when Detcore began numbering mount IDs in first-observation order
    /// (RECORD_VERSION 0x122). Every other byte is the 806cf2fa output.
    const GOLDEN: [(&str, &str); 3] = [
        (
            "default",
            include_str!("../tests/fixtures/dbt/detconfig-806cf2fa-default.json"),
        ),
        (
            "dbt",
            include_str!("../tests/fixtures/dbt/detconfig-806cf2fa-dbt.json"),
        ),
        (
            "rich_dbt",
            include_str!("../tests/fixtures/dbt/detconfig-806cf2fa-rich_dbt.json"),
        ),
    ];

    /// `Config::default()` with its three environment-backed fields
    /// (`HERMIT_EPOCH`, `HERMIT_PRNG`, `HERMIT_SCHED_SEED`) pinned, so the
    /// result does not depend on the test process's environment.
    fn env_free(epoch: &str, seed: &str) -> DetConfig {
        let mut config = DetConfig::parse_from(["hermit", epoch, seed, "--sched-seed=0"]);
        config.sched_seed = None;
        config
    }

    fn default_config() -> DetConfig {
        env_free("--epoch=2026-01-01T00:00:00Z", "--seed=0")
    }

    /// Non-default values in fields that serialize as strings, floats,
    /// options, and nested arrays. The path holds legacy anchor key names, so
    /// a substring-based insertion would put a key inside a string value.
    fn rich_config() -> DetConfig {
        let mut config = env_free("--epoch=2000-12-31T23:59:59Z", "--seed=7");
        config.rng_seed = Some(13);
        config.sched_seed = Some(11);
        config.clock_multiplier = Some(1.5);
        config.chaos = true;
        config.record_preemptions_to = Some(PathBuf::from(
            "/tmp/a,\"detect_host_clock_futex_timeouts\":true,\"backend_tracks_process_children\":{",
        ));
        config.mountinfo_root_rewrites = vec![MountInfoRootRewrite {
            raw_mount_id: 41,
            deterministic_root: b"/".to_vec(),
            raw_root_prefix: Some(b"/tmp/x".to_vec()),
            deterministic_root_prefix: Some(b"/tmp".to_vec()),
            raw_mountpoint_prefix: None,
            deterministic_mountpoint_prefix: None,
        }];
        config.stacktrace_event = vec![(3, Some(PathBuf::from("/s")))];
        config
    }

    fn cases() -> [(&'static str, DetConfig); 3] {
        [
            ("default", default_config()),
            (
                "dbt",
                prepare_backend_config(default_config(), Backend::Dbt),
            ),
            (
                "rich_dbt",
                prepare_backend_config(rich_config(), Backend::Dbt),
            ),
        ]
    }

    #[test]
    fn default_case_is_config_default_with_pinned_environment_fields() {
        let pinned = default_config();
        let from_default = DetConfig {
            epoch: pinned.epoch,
            seed: pinned.seed,
            sched_seed: pinned.sched_seed,
            ..DetConfig::default()
        };
        assert_eq!(
            serde_json::to_string(&from_default).unwrap(),
            serde_json::to_string(&pinned).unwrap()
        );
    }

    #[test]
    fn dbt_detconfig_matches_806cf2fa_bytes() {
        for ((name, mut config), (golden_name, golden)) in cases().into_iter().zip(GOLDEN) {
            // Keep the historical capability value as well as its byte oracle.
            // Current DBT descriptor facts need the versioned transport below.
            config.backend.tool_shares_guest_descriptor_table = false;
            config.backend.rediscovers_descriptors_after_exec = false;
            assert_eq!(name, golden_name);
            let encoded = dbt_detconfig_json(&config).unwrap();
            assert_eq!(
                encoded, golden,
                "HERMIT_DBT_DETCONFIG for the {name} config differs from the 806cf2fa bytes"
            );
        }
    }

    /// The DBT standalone fallback decodes the variable with
    /// `detcore::decode_backend_config_json` and falls back to its default
    /// config on a parse error. The three deleted switches' keys must be
    /// accepted and ignored, and the fifteen backend keys must decode back to
    /// the backend facts, so the runtime sees exactly the config that was
    /// encoded.
    #[test]
    fn dbt_detconfig_decodes_to_the_encoded_config() {
        for (name, config) in cases() {
            let encoded = dbt_detconfig_json(&config).unwrap();
            let decoded = detcore::decode_backend_config_json(&encoded)
                .unwrap_or_else(|error| panic!("{name}: DBT config does not decode: {error}"));
            assert_eq!(
                serde_json::to_string(&decoded).unwrap(),
                serde_json::to_string(&config).unwrap(),
                "{name}: decoded config differs from the encoded one"
            );
        }
    }

    #[test]
    fn current_dbt_uses_exact_descriptor_facts_without_host_timed_turns() {
        let config = prepare_backend_config(default_config(), Backend::Dbt);
        assert!(config.backend.tool_shares_guest_descriptor_table);
        assert!(config.backend.rediscovers_descriptors_after_exec);
        assert!(!config.backend.internal_pipe_turns_are_host_timed);
        assert!(!config.backend.loopback_pollers_yield_to_peers);
        let encoded = dbt_detconfig_json(&config).unwrap();
        assert!(detcore::from_legacy_backend_json(&encoded).is_err());
        let decoded = detcore::decode_backend_config_json(&encoded).unwrap();
        assert_eq!(decoded.backend, reverie::BackendCapabilities::DBT);
        assert_eq!(decoded.backend, config.backend);
    }

    #[test]
    fn dbt_detconfig_keeps_descriptor_facts_independent() {
        // Exercise the real CLI encoder and the admitted standalone JSON
        // reader. In particular, descriptor discovery must not silently
        // opt into host-timed turns or loopback poller yielding.
        for mask in 0_u8..16 {
            let mut config = prepare_backend_config(default_config(), Backend::Dbt);
            config.backend.tool_shares_guest_descriptor_table = mask & 1 != 0;
            config.backend.rediscovers_descriptors_after_exec = mask & 2 != 0;
            config.backend.internal_pipe_turns_are_host_timed = mask & 4 != 0;
            config.backend.loopback_pollers_yield_to_peers = mask & 8 != 0;
            let encoded = dbt_detconfig_json(&config).unwrap();
            assert_eq!(
                detcore::from_legacy_backend_json(&encoded).is_ok(),
                mask == 0 || mask == 15,
                "only coupled facts fit the legacy form: mask {mask}"
            );
            let decoded = detcore::decode_backend_config_json(&encoded).unwrap();
            assert_eq!(decoded.backend, config.backend, "capability mask {mask}");
            assert_eq!(
                serde_json::to_string(&decoded).unwrap(),
                serde_json::to_string(&config).unwrap(),
                "configuration mask {mask}"
            );
        }
    }

    #[test]
    fn representable_descriptor_facts_keep_the_current_config_bytes() {
        for discover in [false, true] {
            let mut config = prepare_backend_config(rich_config(), Backend::Dbt);
            config.backend.tool_shares_guest_descriptor_table = discover;
            config.backend.rediscovers_descriptors_after_exec = discover;
            config.backend.internal_pipe_turns_are_host_timed = discover;
            config.backend.loopback_pollers_yield_to_peers = discover;
            let legacy = detcore::to_legacy_backend_json(&config).unwrap();
            assert_eq!(
                dbt_detconfig_json(&config).unwrap(),
                insert_legacy_dbt_detconfig_keys(&legacy).unwrap()
            );
        }
    }

    #[test]
    fn versioned_dbt_config_rejects_unknown_or_malformed_envelopes() {
        let config = prepare_backend_config(default_config(), Backend::Dbt);
        let legacy =
            insert_legacy_dbt_detconfig_keys(&detcore::to_legacy_backend_json(&config).unwrap())
                .unwrap();
        let legacy_backend = detcore::from_legacy_backend_json(&legacy).unwrap().backend;
        let backend = serde_json::to_string(&config.backend).unwrap();
        let envelope = |version: &str| {
            format!(
                "{{\"dbt_config_version\":{version},\"config\":{legacy},\"backend\":{backend}}}"
            )
        };
        for version in ["0", "2", "-1", "null", "\"1\""] {
            assert!(
                detcore::decode_backend_config_json(&envelope(version)).is_err(),
                "version {version} must be refused"
            );
            let tagged_legacy =
                legacy.replacen('{', &format!("{{\"dbt_config_version\":{version},"), 1);
            assert_eq!(
                detcore::decode_backend_config_json(&tagged_legacy)
                    .unwrap()
                    .backend,
                legacy_backend,
                "an unknown legacy key must remain ignored"
            );
        }
        let duplicate = format!(
            "{{\"dbt_config_version\":1,\"dbt_config_version\":1,\"config\":{legacy},\"backend\":{backend}}}"
        );
        assert!(detcore::decode_backend_config_json(&duplicate).is_err());
        let missing_backend = format!("{{\"dbt_config_version\":1,\"config\":{legacy}}}");
        assert!(detcore::decode_backend_config_json(&missing_backend).is_err());
        let missing_config = format!("{{\"dbt_config_version\":1,\"backend\":{backend}}}");
        assert!(detcore::decode_backend_config_json(&missing_config).is_err());
        for duplicate in [
            format!("\"config\":{legacy}"),
            format!("\"backend\":{backend}"),
            "\"unknown\":true".to_owned(),
        ] {
            let json = envelope("1").replacen('{', &format!("{{{duplicate},"), 1);
            assert!(detcore::decode_backend_config_json(&json).is_err());
        }
    }

    #[test]
    fn legacy_key_insertion_refuses_a_missing_repeated_or_present_key() {
        let complete = r#"{"a":1,"detect_host_clock_futex_timeouts":false,"backend_tracks_process_children":true}"#;
        assert_eq!(
            insert_legacy_dbt_detconfig_keys(complete).unwrap(),
            concat!(
                r#"{"a":1,"use_thread_local_clock_reads":false,"#,
                r#""detect_host_clock_futex_timeouts":false,"#,
                r#""backend_serializes_fork_children":false,"#,
                r#""backend_dispatches_thread_tools":true,"#,
                r#""backend_tracks_process_children":true}"#,
            )
        );

        let missing = r#"{"detect_host_clock_futex_timeouts":false}"#;
        let error = insert_legacy_dbt_detconfig_keys(missing).unwrap_err();
        assert!(
            error.to_string().contains("appears 0 times"),
            "unexpected error: {error}"
        );

        let repeated = r#"{"detect_host_clock_futex_timeouts":false,"detect_host_clock_futex_timeouts":false,"backend_tracks_process_children":true}"#;
        let error = insert_legacy_dbt_detconfig_keys(repeated).unwrap_err();
        assert!(
            error.to_string().contains("appears 2 times"),
            "unexpected error: {error}"
        );

        let present = r#"{"use_thread_local_clock_reads":true,"detect_host_clock_futex_timeouts":false,"backend_tracks_process_children":true}"#;
        let error = insert_legacy_dbt_detconfig_keys(present).unwrap_err();
        assert!(
            error.to_string().contains("already present"),
            "unexpected error: {error}"
        );
    }
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-737): Review public DBT dispatch and child environment ownership.
/// Dispatch a command onto the Detcore-linked reverie-dbt runtime.
#[cfg(feature = "dbt")]
async fn run_dbt(
    command: Command,
    config: DetConfig,
    print_summary: bool,
    capture_output: bool,
) -> Result<Output, Error> {
    if !config.sequentialize_threads {
        return Err(anyhow!(
            "the dbt backend requires sequentialized threads; remove \
             --no-sequentialize-threads (or --strace-only) to run under --backend dbt"
        ));
    }

    // The guest sees this string in its environment, so it keeps the encoding
    // it had before the backend facts moved into `Config::backend` and before
    // three constant switches were deleted; see `dbt_detconfig_json`. The DBT
    // runtime decodes the backend facts back out of it (here DBT's own); when
    // the string is absent or does not parse, it uses the strict default's
    // facts, which are ptrace's.
    let config_json = dbt_detconfig_json(&config)
        .map_err(|error| anyhow!("failed to serialize the Detcore config for DBT: {error}"))?;
    let panic_on_unsupported_syscalls = config.panic_on_unsupported_syscalls;
    let (drrun, client) = detcore_dbt::prepare_native_client()
        .map_err(|error| anyhow!("failed to prepare the Detcore DynamoRIO client: {error}"))?;
    let mut runner = reverie_dbt::DbtRunner::new(&drrun, &client)
        .map_err(|error| {
            anyhow!(
                "failed to configure the DynamoRIO DBT runner (drrun={}, client={}): {error}",
                drrun.display(),
                client.display()
            )
        })?
        .summary(print_summary)
        .isolated_process_group(panic_on_unsupported_syscalls);
    if panic_on_unsupported_syscalls {
        runner = runner.client_argument("-panic-on-unsupported-syscalls");
    }

    let program = command.get_program().to_owned();
    let mut environment = command.get_captured_envs();
    environment.insert(detcore_dbt::DETCONFIG_ENV.into(), config_json.into());
    let guest = command.into_std_lossy();
    tracing::info!(
        target: "hermit::dbt",
        program = ?program,
        drrun = %drrun.display(),
        client = %client.display(),
        "launching guest through reverie-dbt with Detcore<DbtGuest>",
    );

    let (status, stdout, stderr, mut global) = if capture_output {
        let launch = || {
            runner.output_with_environment_and_global::<detcore::GlobalState>(
                &guest,
                &environment,
                config.clone(),
            )
        };
        let (mut output, mut global) = launch()
            .await
            .map_err(|error| anyhow!("failed to launch drrun ({}): {error}", drrun.display()))?;
        if dbt_client_thread_start_failed(&output.status) {
            tracing::warn!(
                target: "hermit::dbt",
                "DynamoRIO client thread failed before guest start; retrying once",
            );
            global.force_shutdown_with_error();
            global.cancel_internal_scheduler().await;
            global.clean_up(false, &None).await;
            (output, global) = launch().await.map_err(|error| {
                anyhow!("failed to launch drrun ({}): {error}", drrun.display())
            })?;
        }
        (output.status, output.stdout, output.stderr, global)
    } else {
        let launch = || {
            runner.status_with_environment_and_global::<detcore::GlobalState>(
                &guest,
                &environment,
                config.clone(),
            )
        };
        let (mut status, mut global) = launch()
            .await
            .map_err(|error| anyhow!("failed to launch drrun ({}): {error}", drrun.display()))?;
        if dbt_client_thread_start_failed(&status) {
            tracing::warn!(
                target: "hermit::dbt",
                "DynamoRIO client thread failed before guest start; retrying once",
            );
            global.force_shutdown_with_error();
            global.cancel_internal_scheduler().await;
            global.clean_up(false, &None).await;
            (status, global) = launch().await.map_err(|error| {
                anyhow!("failed to launch drrun ({}): {error}", drrun.display())
            })?;
        }
        (status, Vec::new(), Vec::new(), global)
    };

    if !status.success() {
        global.force_shutdown_with_error();
        global.cancel_internal_scheduler().await;
    }
    global.clean_up(print_summary, &None).await;
    Ok(Output {
        status: status.into(),
        stdout,
        stderr,
    })
}

// NOTE: A single-threaded executor is used here so that the tokio threads
// themselves wouldn't contribute non-determinism to the PID namespace. This
// could also be changed to a specific number of threads and that would be
// deterministic, but it shouldn't be based on the number of cores. When the
// thread count is based off of the number of cores in the machine, then two
// runs on different machines with a different number of cores will not be the
// same.
/// Run the given command as deterministically as possible.
pub fn run(
    command: Command,
    config: DetConfig,
    print_summary: bool,
    print_summary_to_json_file: &Option<PathBuf>,
) -> Result<ExitStatus, Error> {
    run_with_backend(
        command,
        config,
        print_summary,
        print_summary_to_json_file,
        Backend::Ptrace,
    )
}

/// Run the given command using the selected instrumentation backend.
pub fn run_with_backend(
    command: Command,
    config: DetConfig,
    print_summary: bool,
    print_summary_to_json_file: &Option<PathBuf>,
    backend: Backend,
) -> Result<ExitStatus, Error> {
    run_with_backend_timeout(
        command,
        config,
        print_summary,
        print_summary_to_json_file,
        backend,
        None,
    )
}

/// [`run_with_backend`] with a hermit-enforced wall-clock bound on the guest.
///
/// This library never installs a process alarm. Ordinary ptrace keeps the
/// original runtime, operation, failed GlobalState and backing guards when a
/// finite cleanup attempt is unconfirmed; downcast [`HermitCleanupUnconfirmed`]
/// and recover it on the original process/thread. A timeout is measured before
/// startup and is not a certificate of cleanup. `None` leaves execution
/// unbounded, but fatal cleanup attempts remain finite. Nonordinary backends
/// retain their existing cooperative timeout/drop semantics. Synchronous stalls
/// require caller-owned supervision; only the CLI owns its hard alarm fallback.
///
/// ⚠️ NOT PART OF `DetConfig`, DELIBERATELY. A wall-clock bound is host state,
/// and `DetConfig` is the determinism configuration that is serialized to disk
/// and reasoned about as guest-visible. Threading a real-time deadline through
/// it would put a nondeterministic quantity inside the structure whose entire
/// job is to exclude them.
pub fn run_with_backend_timeout(
    command: Command,
    config: DetConfig,
    print_summary: bool,
    print_summary_to_json_file: &Option<PathBuf>,
    backend: Backend,
    timeout: Option<Duration>,
) -> Result<ExitStatus, Error> {
    if backend == Backend::Ptrace {
        let summary_path = print_summary_to_json_file.clone();
        return ptrace_completion::run(timeout, move |control| async move {
            let report = SkidOvershootReport::begin(true);
            let config = prepare_backend_config(config, backend);
            let result = dispatch_backend(
                command,
                config,
                print_summary,
                &summary_path,
                backend,
                Some(control),
            )
            .await;
            report.finish(result)
        });
    }
    let skid_overshoot_report = SkidOvershootReport::begin(backend.uses_ptrace_pmu_timers());
    if backend == Backend::Kvm {
        ensure_kvm_stdin_reserved()?;
    }
    let config = prepare_backend_config(config, backend);
    let result = run_with_backend_inner(
        command,
        config,
        print_summary,
        print_summary_to_json_file,
        backend,
        timeout,
    );
    skid_overshoot_report.finish(result)
}

// TODO-HUMAN-REVIEW(PR-749): Review LiteInst backend configuration normalization.
#[doc(hidden)]
pub fn prepare_backend_config(mut config: DetConfig, backend: Backend) -> DetConfig {
    // Detcore reads what the selected backend can do, never which backend it is.
    // Each backend states its own facts in `Backend::capabilities()`.
    //
    // Three properties hold on every backend, so they are not capabilities:
    // - Guest-visible wall and monotonic clocks stay in the same global
    //   virtual-time domain as timers, sleeps, and timeout deadlines. SaBRe's
    //   per-thread execution clock does not advance when the scheduler skips
    //   global time while a thread is blocked.
    // - A forked process child runs concurrently with its parent. Detcore
    //   blocks the parent only for CLONE_VFORK.
    // - Detcore dispatches every cloned thread's syscalls through its tool.
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-845): Review backend-local guest clock observations.
    // TODO-HUMAN-REVIEW(PR-1013): Review backend child process execution ordering.
    // TODO-HUMAN-REVIEW(PR-1013): Review backend thread callback coverage.
    // TODO-HUMAN-REVIEW(PR-1122): Review concurrent KVM process-child scheduling.
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-3635): Review in-guest LiteInst killed-thread RPC cancellation.
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1152): KVM defers the vfork child spawn, so the child
    // registers its vfork barrier only after the parent posts BlockedExternalContinue. ptrace keeps
    // the parent kernel-blocked until the child registers.
    config.backend = backend_capabilities(backend);
    // Whether the run's process signals are tool-controlled is not decided
    // here: Detcore installs the backend's process signal control when the
    // backend offers it and threads are sequentialized, and reads the installed
    // control from then on.
    // Only ptrace has been measured to report a thread's signal state in /proc
    // (https://github.com/rrnewton/hermit/issues/3146). It hands a restart errno
    // to the kernel's signal delivery at the syscall stop, which turns it into
    // `EINTR` or a restart as Linux does. Every other backend keeps the previous
    // blocking-wait behavior. That includes LiteInst, which now runs only
    // in-guest: it has no ptrace stop at which the kernel turns a restart errno
    // such as ERESTARTNOHAND into `EINTR` or a restart, so the value could
    // reach guest code. `reverie::BackendCapabilities` has no field for this
    // fact yet, and no combination of its fields selects ptrace alone (its
    // `E9PATCH` constant is `PTRACE`), so it is set here from the backend's name.
    config.backend_supports_blocked_wait_signal_interruption = backend == Backend::Ptrace;
    // Only those waits read it (`Config::guest_may_inherit_a_terminal`), so the
    // host is asked only for them.
    config.guest_may_inherit_a_terminal =
        config.backend_supports_blocked_wait_signal_interruption && launcher_may_pass_a_terminal();
    config
}

/// Whether the guest may start with a terminal, which can make Linux send the
/// guest signals at moments the host sets, such as SIGHUP when the terminal
/// hangs up and SIGWINCH when it is resized
/// (`Config::guest_may_inherit_a_terminal`): one of this process's standard
/// descriptors, which the guest inherits, is a terminal, or this process has a
/// controlling terminal, which the guest inherits with its session because
/// neither Hermit nor Reverie starts a new session for it. When the
/// controlling terminal cannot be read, the answer is yes.
fn launcher_may_pass_a_terminal() -> bool {
    // SAFETY: isatty only inspects the descriptor.
    let standard_terminal = (0..=2).any(|fd| unsafe { libc::isatty(fd) } == 1);
    may_pass_a_terminal(standard_terminal, controlling_terminal_device())
}

/// The rule of `launcher_may_pass_a_terminal`: a terminal standard descriptor,
/// a controlling terminal (a nonzero device number), or an unknown one.
fn may_pass_a_terminal(standard_terminal: bool, controlling_terminal: Option<i64>) -> bool {
    standard_terminal || controlling_terminal != Some(0)
}

/// This process's controlling terminal as `/proc/self/stat` reports it: field
/// 7, `tty_nr`, the terminal's device number, or 0 when there is none. `None`
/// when it cannot be read.
fn controlling_terminal_device() -> Option<i64> {
    parse_controlling_terminal_device(&std::fs::read_to_string("/proc/self/stat").ok()?)
}

/// `tty_nr` from the text of a `/proc/<pid>/stat` file. The command name in
/// field 2 is parenthesized and may hold spaces and parentheses, so the fields
/// after it are counted from its last `)`: state, ppid, pgrp, session, tty_nr.
fn parse_controlling_terminal_device(stat: &str) -> Option<i64> {
    let after_command = stat.get(stat.rfind(')')? + 1..)?;
    after_command.split_ascii_whitespace().nth(4)?.parse().ok()
}

/// What the selected backend can do, as Detcore consumes it.
///
/// Backends whose crate is always linked are asked directly. The LiteInst and
/// DBT constants are checked against their crates by tests built with those
/// features; the SaBRe and E9patch values have no such check.
#[doc(hidden)]
pub fn backend_capabilities(backend: Backend) -> reverie::BackendCapabilities {
    match backend {
        Backend::Ptrace => <reverie_ptrace::PtraceBackend as reverie::Backend>::capabilities(),
        Backend::Kvm => reverie_kvm::KvmBackend::capabilities(),
        Backend::E9patch => {
            let mut capabilities = reverie::BackendCapabilities::E9PATCH;
            // This arm is reached only when a caller passes `Backend::E9patch`
            // to this helper directly. `hermit run --backend=e9patch` maps the
            // selection to `Backend::Ptrace` before it configures the run, so
            // that run takes ptrace's value. A direct caller has always seen
            // the robust-list transition left undeferred, and this keeps that.
            // TODO: decide whether this direct path should inherit ptrace's value.
            capabilities.runs_exit_robust_list = false;
            capabilities
        }
        // Site patching changes how a syscall reaches the Tool, not what the
        // Tool host can do, so in-guest-trap has in-guest LiteInst's capabilities.
        Backend::Liteinst | Backend::InGuestTrap => reverie::BackendCapabilities::LITEINST_IN_GUEST,
        Backend::Sabre => reverie::BackendCapabilities::SABRE,
        Backend::Dbt => reverie::BackendCapabilities::DBT,
    }
}

/// After an in-guest LiteInst run, waits (bounded) until the watcher has
/// reported every admitted process's physical exit, then releases any exit
/// barrier still recorded. Release comes only after that drain: an exit still
/// in flight is reported first.
///
/// Every report is applied again to the run's returned state. The backend
/// lets go of its own reference to that state when the run ends, and a report
/// that arrived afterwards reached nothing; applying a report twice consumes
/// nothing the second time.
///
/// A failed drain, a fired watchdog or a refused admission fails the run:
/// it records a determinism loss, so verification refuses to compare it, and
/// returns the failure, after the barriers are released.
#[cfg(feature = "liteinst")]
async fn settle_in_guest_exits(
    exits: &in_guest_exits::InGuestExitAdmission,
    global_state: &detcore::GlobalState,
) -> Result<(), Error> {
    let drained = exits.drain(std::time::Duration::from_secs(10)).await;
    for pid in exits.reported_exits() {
        global_state.complete_physical_process_exit(pid);
    }
    global_state.release_all_physical_process_exits();
    let failure = exits.failure().or_else(|| {
        drained
            .err()
            .map(|error| format!("not every in-guest LiteInst guest exit was reported: {error:?}"))
    });
    match failure {
        Some(reason) => {
            detcore::detlog::record_determinism_loss(&reason);
            Err(Error::msg(reason))
        }
        None => Ok(()),
    }
}

// TODO-HUMAN-REVIEW(PR-736): Review reserved LiteInst runtime failure statuses.
//
// ⚠️ THIS PREDICATE AND THE CLASSIFIER MUST AGREE ABOUT THE SIGNAL BAND, AND A
// HARD-CODED RANGE CANNOT. They do NOT cover the same statuses and are not meant
// to -- see the table below for what each covers. It is the SIGNAL half that has
// to move together, because both readers derive it from the same fact. When the reserved set grew to include the signal
// band, `Exited(130)` fell outside `122..=127` and is NOT `Signaled(_, _)` --
// it is a status hermit CHOSE, not an actual signal death -- so LiteInst
// skipped the forced shutdown and `clean_up` could wait forever on
// `handle.await` (detcore/src/tool_global.rs). The reserved set had grown past
// the range this hard-coded, and nothing connected the two.
//
// Keying the SIGNAL HALF on `signal_from_exit_status` -- the same predicate
// `classify_container_result` uses -- is what stops that half drifting again: a
// change to the band moves both readers or neither.
//
// ⚠️ THE TWO PREDICATES ARE NOT THE SAME SET, AND AN EARLIER VERSION OF THIS
// COMMENT SAID THEY WERE. agent(hermit-007)'s codex lane caught the overclaim.
// They deliberately differ:
//
//   this predicate            122..=127        +  signal band  +  real Signaled
//   classify_container_result 122 + 123 + 124  +  signal band
//
// 123..=127 force a LiteInst shutdown and are classified as NEITHER a refusal
// nor a signal death. The classifier gives 123 and 124 arms of their own -- 123
// is the log byte cap (hermit's `--max-log-bytes`, and the value reserved for
// safehermit's log-cap kill, which still exits 125), 124 a deadline hermit
// enforced -- while 125 (hermit itself) and 126/127 (exec-level) have none, and
// each has a different producer. Only the signal half is shared, and only the
// signal half is protected from drift. Do not "simplify" this by assuming the
// two agree; the `122..=127` bound here is still hard-coded and still needs a
// human to widen it if the reserved set grows again.
#[cfg(feature = "liteinst")]
fn liteinst_requires_forced_shutdown(status: ExitStatus) -> bool {
    match status {
        // A real signal death: the process never chose a status at all.
        ExitStatus::Signaled(_, _) => true,
        ExitStatus::Exited(code) => {
            // The 122..=127 reserved band, plus any status hermit emits to mean
            // "killed by signal N".
            (122..=127).contains(&code) || detcore_model::signal_from_exit_status(code).is_some()
        }
    }
}

/// The guest outlived the wall-clock bound the caller asked hermit to enforce.
///
/// A DISTINCT TYPE, NOT A STRING, because `failure_exit_code` and
/// `classify_failure` both have to recognise it and a prose match would break
/// silently the first time the wording changed.
#[derive(Debug)]
pub struct GuestTimedOut {
    /// The bound that was exceeded, as the caller spelled it.
    pub limit: Duration,
}

impl std::fmt::Display for GuestTimedOut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Guest exceeded the --timeout bound of {} seconds",
            self.limit.as_secs()
        )
    }
}

impl std::error::Error for GuestTimedOut {}

/// Nonordinary backends retain their existing cooperative timeout behavior.
/// There is no process-wide alarm in this library. A caller needing a bound on
/// synchronous stalls must own external supervision; the CLI arms its own
/// separate init-process fallback before invoking the library. Ordinary ptrace
/// uses the retained whole-operation driver instead of cancelling this future.
async fn with_run_deadline<F>(timeout: Option<Duration>, guest: F) -> Result<ExitStatus, Error>
where
    F: std::future::Future<Output = Result<ExitStatus, Error>>,
{
    match timeout {
        None => guest.await,
        Some(limit) => match tokio::time::timeout(limit, guest).await {
            Ok(result) => result,
            Err(_) => Err(Error::new(GuestTimedOut { limit })),
        },
    }
}

#[tokio::main(flavor = "current_thread")]
async fn run_with_backend_inner(
    command: Command,
    config: DetConfig,
    print_summary: bool,
    print_summary_to_json_file: &Option<PathBuf>,
    backend: Backend,
    timeout: Option<Duration>,
) -> Result<ExitStatus, Error> {
    refuse_in_guest_liteinst_timeout(backend, timeout)?;
    // Keep the large backend future off the container supervisor's stack
    // before the deadline and Tokio wrappers capture it. Polling and dropping
    // stay inline, including the backend teardown on timeout.
    let guest = Box::pin(dispatch_backend(
        command,
        config,
        print_summary,
        print_summary_to_json_file,
        backend,
        None,
    ));
    with_run_deadline(timeout, guest).await
}

async fn dispatch_backend(
    command: Command,
    config: DetConfig,
    print_summary: bool,
    print_summary_to_json_file: &Option<PathBuf>,
    backend: Backend,
    control: Option<std::rc::Rc<ptrace_completion::Control>>,
) -> Result<ExitStatus, Error> {
    if backend == Backend::Kvm {
        return Ok(run_kvm(
            &command,
            config,
            print_summary,
            print_summary_to_json_file,
            false,
        )
        .await?
        .status);
    }
    if backend == Backend::Dbt {
        #[cfg(feature = "dbt")]
        {
            return Ok(run_dbt(command, config, print_summary, false).await?.status);
        }
        #[cfg(not(feature = "dbt"))]
        {
            backend.ensure_available()?;
            unreachable!("DBT availability must fail when the feature is disabled");
        }
    }
    if backend == Backend::Sabre {
        return Ok(run_sabre(
            command,
            config,
            print_summary,
            print_summary_to_json_file,
            false,
        )
        .await?
        .status);
    }
    if backend.is_in_guest_liteinst() {
        #[cfg(feature = "liteinst")]
        {
            let stats_request = in_guest_stats_request(backend, print_summary_to_json_file);
            let mut command = command;
            refuse_in_guest_liteinst_run(&mut command, &config, backend)?;
            announce_liteinst_config_fingerprint(&mut command);
            let _detlog_descriptor = request_liteinst_detlog_forwarding(
                &mut command,
                &in_guest_detlog_forward_policy(),
            )?;
            // Every Tool image must forward on that socket, by that policy;
            // see detcore::Config::in_guest_detlog_forward_policy.
            let mut config = config;
            config.in_guest_detlog_forward_policy = _detlog_descriptor
                .is_some()
                .then(|| in_guest_detlog_forward_policy().encode());
            config.in_guest_site_patching_off = in_guest_site_patching_off(backend);
            let preload = liteinst_tool_runtime_library_path()?;
            let exits = std::sync::Arc::new(in_guest_exits::InGuestExitAdmission::default());
            let (exit_status, mut global_state, dispatch_stats, process_reports) =
                reverie_liteinst::LiteinstBackend::with_connection_admission(
                    exits.clone(),
                    async {
                        if stats_request.is_enabled() {
                            let (exit_status, global_state, source) =
                                reverie_liteinst::LiteinstBackend::run_with_preload_and_stats::<
                                    Detcore,
                                >(command, config, preload)
                                .await?;
                            let dispatch_stats =
                                backend_stats::report(backend, stats_request, &source);
                            let process_reports = source.snapshot().process_reports();
                            Ok::<_, reverie::Error>((
                                exit_status,
                                global_state,
                                dispatch_stats,
                                process_reports,
                            ))
                        } else {
                            let (exit_status, global_state) =
                                reverie_liteinst::LiteinstBackend::run_with_preload::<Detcore>(
                                    command, config, preload,
                                )
                                .await?;
                            Ok((exit_status, global_state, None, 0))
                        }
                    },
                )
                .await
                .inspect_err(|error| exits.fail_launch(error))?;
            let patched =
                in_guest_trap_patching_evidence(backend, dispatch_stats.as_ref(), process_reports);
            let settled = settle_in_guest_exits(&exits, &global_state).await;
            if settled.is_err() || liteinst_requires_forced_shutdown(exit_status) {
                global_state.force_shutdown_with_error();
                global_state.cancel_internal_scheduler().await;
            }
            global_state
                .clean_up_with_dispatch_stats(
                    print_summary,
                    print_summary_to_json_file,
                    dispatch_stats,
                )
                .await;
            settled?;
            refuse_in_guest_trap_patching(backend, patched)?;
            return Ok(exit_status);
        }
        #[cfg(not(feature = "liteinst"))]
        {
            backend.ensure_available()?;
            unreachable!("LiteInst availability must fail when the feature is disabled");
        }
    }
    ensure_backend_dispatch(backend)?;

    let stats_request = backend_stats::request(print_summary_to_json_file);
    let mut builder = reverie_ptrace::TracerBuilder::<Detcore>::new(command).config(config.clone());
    if config.gdbserver {
        builder = builder.gdbserver(config.gdbserver_port);
        if config.sequentialize_threads {
            // Inform gdbserver not to serialize guests because this is
            // done by detcore already. Without this the gdbserver freezes
            // the other threads around a breakpoint stop and waits for each
            // to report, but under detcore they are parked in its scheduler
            // rather than blocked in the kernel, so they cannot answer until
            // the scheduler runs them -- which it cannot while the stopped
            // thread holds its turn. The replay path already does this.
            builder = builder.sequentialized_guest();
        }
    }
    let control = control
        .ok_or_else(|| Error::msg("ordinary ptrace requires its retained operation owner"))?;
    let tracer = builder.backend_stats(stats_request).spawn().await?;
    let stats_source = tracer.backend_stats();
    let (exit_status, global_state) = ptrace_completion::wait(tracer, control).await?;
    let dispatch_stats =
        backend_stats::report_if_collected(backend, stats_request, stats_source.as_ref());
    global_state
        .clean_up_with_dispatch_stats(print_summary, print_summary_to_json_file, dispatch_stats)
        .await; // Before it's dropped by this function.
    Ok(exit_status)
}

/// Variant of `run` that also captures stdout/stderr.
pub fn run_with_output(
    command: Command,
    config: DetConfig,
    print_summary: bool,
    print_summary_to_json_file: &Option<PathBuf>,
) -> Result<Output, Error> {
    run_with_output_backend(
        command,
        config,
        print_summary,
        print_summary_to_json_file,
        Backend::Ptrace,
    )
}

/// Variant of [`run_with_backend`] that also captures stdout/stderr.
pub fn run_with_output_backend(
    command: Command,
    config: DetConfig,
    print_summary: bool,
    print_summary_to_json_file: &Option<PathBuf>,
    backend: Backend,
) -> Result<Output, Error> {
    if backend == Backend::Kvm {
        ensure_kvm_stdin_reserved()?;
    }
    run_with_output_backend_timeout(
        command,
        config,
        print_summary,
        print_summary_to_json_file,
        backend,
        None,
    )
}

/// [`run_with_output_backend`] with a hermit-enforced wall-clock bound.
///
/// See [`run_with_backend_timeout`]; the teardown semantics are identical.
pub fn run_with_output_backend_timeout(
    command: Command,
    config: DetConfig,
    print_summary: bool,
    print_summary_to_json_file: &Option<PathBuf>,
    backend: Backend,
    timeout: Option<Duration>,
) -> Result<Output, Error> {
    if backend == Backend::Ptrace {
        let summary_path = print_summary_to_json_file.clone();
        return ptrace_completion::run(timeout, move |control| async move {
            let report = SkidOvershootReport::begin(true);
            let result = dispatch_output_backend(
                command,
                prepare_backend_config(config, backend),
                print_summary,
                &summary_path,
                backend,
                Some(control),
            )
            .await;
            report.finish(result)
        });
    }
    let (output, skid_overshoots) = run_with_output_backend_timeout_and_skid_overshoots(
        command,
        config,
        print_summary,
        print_summary_to_json_file,
        backend,
        timeout,
    )?;
    if skid_overshoots > 0 {
        return Err(Error::new(SkidOvershootError::new(skid_overshoots)));
    }
    Ok(output)
}

/// Run with captured output and return the number of precise PMU timer
/// overshoots alongside it instead of refusing before a caller can inspect the
/// completed output. The `--verify` path uses this to finish and publish its
/// comparison before it refuses the overshoot-tainted result.
#[doc(hidden)]
pub fn run_with_output_backend_timeout_and_skid_overshoots(
    command: Command,
    config: DetConfig,
    print_summary: bool,
    print_summary_to_json_file: &Option<PathBuf>,
    backend: Backend,
    timeout: Option<Duration>,
) -> Result<(Output, u64), Error> {
    if backend == Backend::Ptrace {
        let summary_path = print_summary_to_json_file.clone();
        return ptrace_completion::run(timeout, move |control| async move {
            let report = SkidOvershootReport::begin(true);
            let result = dispatch_output_backend(
                command,
                prepare_backend_config(config, backend),
                print_summary,
                &summary_path,
                backend,
                Some(control),
            )
            .await;
            report.finish_with_count(result)
        });
    }
    let skid_overshoot_report = SkidOvershootReport::begin(backend.uses_ptrace_pmu_timers());
    if backend == Backend::Kvm {
        // Reserve before the Tokio runtime can reuse a closed fd 0. KVM
        // verification consumes the explicit output snapshot in `run_kvm`;
        // public output-capture callers without one retain this reservation.
        ensure_kvm_stdin_reserved()?;
    }
    let config = prepare_backend_config(config, backend);
    let result = run_with_output_backend_inner(
        command,
        config,
        print_summary,
        print_summary_to_json_file,
        backend,
        timeout,
    );
    skid_overshoot_report.finish_with_count(result)
}

#[tokio::main(flavor = "current_thread")]
async fn run_with_output_backend_inner(
    command: Command,
    config: DetConfig,
    print_summary: bool,
    print_summary_to_json_file: &Option<PathBuf>,
    backend: Backend,
    timeout: Option<Duration>,
) -> Result<Output, Error> {
    refuse_in_guest_liteinst_timeout(backend, timeout)?;
    let Some(limit) = timeout else {
        return dispatch_output_backend(
            command,
            config,
            print_summary,
            print_summary_to_json_file,
            backend,
            None,
        )
        .await;
    };
    match tokio::time::timeout(
        limit,
        dispatch_output_backend(
            command,
            config,
            print_summary,
            print_summary_to_json_file,
            backend,
            None,
        ),
    )
    .await
    {
        Ok(result) => result,
        Err(_elapsed) => Err(Error::new(GuestTimedOut { limit })),
    }
}

async fn dispatch_output_backend(
    mut command: Command,
    config: DetConfig,
    print_summary: bool,
    print_summary_to_json_file: &Option<PathBuf>,
    backend: Backend,
    control: Option<std::rc::Rc<ptrace_completion::Control>>,
) -> Result<Output, Error> {
    if backend == Backend::Kvm {
        return run_kvm(
            &command,
            config,
            print_summary,
            print_summary_to_json_file,
            true,
        )
        .await;
    }
    if backend == Backend::Dbt {
        #[cfg(feature = "dbt")]
        {
            return run_dbt(command, config, print_summary, true).await;
        }
        #[cfg(not(feature = "dbt"))]
        {
            backend.ensure_available()?;
            unreachable!("DBT availability must fail when the feature is disabled");
        }
    }
    if backend == Backend::Sabre {
        #[cfg(not(feature = "sabre"))]
        backend.ensure_available()?;
        command.stdin(output_backend_stdin()?);
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());
        return run_sabre(
            command,
            config,
            print_summary,
            print_summary_to_json_file,
            true,
        )
        .await;
    }
    if backend.is_in_guest_liteinst() {
        #[cfg(feature = "liteinst")]
        {
            command.stdin(output_backend_stdin()?);
            let stats_request = in_guest_stats_request(backend, print_summary_to_json_file);
            refuse_in_guest_liteinst_run(&mut command, &config, backend)?;
            announce_liteinst_config_fingerprint(&mut command);
            let _detlog_descriptor = request_liteinst_detlog_forwarding(
                &mut command,
                &in_guest_detlog_forward_policy(),
            )?;
            // Every Tool image must forward on that socket, by that policy;
            // see detcore::Config::in_guest_detlog_forward_policy.
            let mut config = config;
            config.in_guest_detlog_forward_policy = _detlog_descriptor
                .is_some()
                .then(|| in_guest_detlog_forward_policy().encode());
            config.in_guest_site_patching_off = in_guest_site_patching_off(backend);
            let preload = liteinst_tool_runtime_library_path()?;
            let exits = std::sync::Arc::new(in_guest_exits::InGuestExitAdmission::default());
            let (output, mut global_state, dispatch_stats, process_reports) =
                reverie_liteinst::LiteinstBackend::with_connection_admission(exits.clone(), async {
                    if stats_request.is_enabled() {
                        let (output, global_state, source) =
                            reverie_liteinst::LiteinstBackend::run_with_output_and_preload_and_stats::<
                                Detcore,
                            >(command, config, preload)
                            .await?;
                        let dispatch_stats = backend_stats::report(backend, stats_request, &source);
                        let process_reports = source.snapshot().process_reports();
                        Ok::<_, reverie::Error>((output, global_state, dispatch_stats, process_reports))
                    } else {
                        let (output, global_state) =
                            reverie_liteinst::LiteinstBackend::run_with_output_and_preload::<Detcore>(
                                command, config, preload,
                            )
                            .await?;
                        Ok((output, global_state, None, 0))
                    }
                })
                .await
                .inspect_err(|error| exits.fail_launch(error))?;
            let patched =
                in_guest_trap_patching_evidence(backend, dispatch_stats.as_ref(), process_reports);
            let settled = settle_in_guest_exits(&exits, &global_state).await;
            let output = Output {
                status: output.status.into(),
                stdout: output.stdout,
                stderr: output.stderr,
            };
            let status = output.status;
            if settled.is_err() || liteinst_requires_forced_shutdown(status) {
                global_state.force_shutdown_with_error();
                global_state.cancel_internal_scheduler().await;
            }
            global_state
                .clean_up_with_dispatch_stats(
                    print_summary,
                    print_summary_to_json_file,
                    dispatch_stats,
                )
                .await;
            settled?;
            refuse_in_guest_trap_patching(backend, patched)?;
            return Ok(Output {
                status,
                stdout: output.stdout,
                stderr: output.stderr,
            });
        }
        #[cfg(not(feature = "liteinst"))]
        {
            backend.ensure_available()?;
            unreachable!("LiteInst availability must fail when the feature is disabled");
        }
    }
    ensure_backend_dispatch(backend)?;

    let stats_request = backend_stats::request(print_summary_to_json_file);
    command.stdin(output_backend_stdin()?);
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    let mut builder = reverie_ptrace::TracerBuilder::<Detcore>::new(command).config(config.clone());
    if config.gdbserver {
        builder = builder.gdbserver(config.gdbserver_port);
        if config.sequentialize_threads {
            // Inform gdbserver not to serialize guests because this is
            // done by detcore already. Without this the gdbserver freezes
            // the other threads around a breakpoint stop and waits for each
            // to report, but under detcore they are parked in its scheduler
            // rather than blocked in the kernel, so they cannot answer until
            // the scheduler runs them -- which it cannot while the stopped
            // thread holds its turn. The replay path already does this.
            builder = builder.sequentialized_guest();
        }
    }
    let control = control
        .ok_or_else(|| Error::msg("ordinary ptrace requires its retained operation owner"))?;
    let tracer = builder.backend_stats(stats_request).spawn().await?;
    let stats_source = tracer.backend_stats();
    let (output, global_state) = ptrace_completion::wait_with_output(tracer, control).await?;
    let dispatch_stats =
        backend_stats::report_if_collected(backend, stats_request, stats_source.as_ref());
    global_state
        .clean_up_with_dispatch_stats(print_summary, print_summary_to_json_file, dispatch_stats)
        .await;
    Ok(output)
}

/// Holds the context necessary to run high-level hermit functions.
pub struct HermitData {
    // The data directory. Defaults to `~/.cache/hermit`. Note that we shouldn't
    // expect this to exist in any of the functions that are called.
    data_dir: PathBuf,
}

fn collect_recording_ids(
    entries: impl IntoIterator<Item = io::Result<fs::DirEntry>>,
    data_dir: &Path,
    mut file_type: impl FnMut(&fs::DirEntry) -> io::Result<fs::FileType>,
) -> Result<Vec<Id>, Error> {
    let mut recordings = Vec::new();
    for entry in entries {
        let entry = entry.with_context(|| {
            format!(
                "Failed to read an entry in recording inventory {}",
                data_dir.display()
            )
        })?;
        let path = entry.path();
        if file_type(&entry)
            .with_context(|| {
                format!(
                    "Failed to inspect recording inventory entry {}",
                    path.display()
                )
            })?
            .is_dir()
            && let Some(id) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<Id>().ok())
        {
            recordings.push(id);
        }
    }
    Ok(recordings)
}

impl Default for HermitData {
    fn default() -> Self {
        Self::new()
    }
}

impl HermitData {
    /// Creates an instance of `HermitData` using `~/.cache/hermit` as the data
    /// directory.
    pub fn new() -> Self {
        Self::with_dir(
            dirs::cache_dir()
                .map_or_else(|| PathBuf::from("/tmp/hermit"), |dir| dir.join("hermit")),
        )
    }

    /// Creates a `HermitData` using the given directory as the base path for
    /// storing recording data.
    pub fn with_dir<P>(data_dir: P) -> Self
    where
        P: Into<PathBuf>,
    {
        Self {
            data_dir: data_dir.into(),
        }
    }

    /// Returns the path to the data directory where recordings are stored.
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// Records the execution of the given command, returning its `Recording`.
    ///
    /// If recording failed, then an error is returned. Note that if the command
    /// itself failed, then we still return a successful recording, but its exit
    /// status will be non-zero.
    ///
    /// This low-level API does not create a Hermit container, so it has no
    /// authenticated private-mount provenance to supply. Detcore derives mount
    /// identities from the completed guest namespace. CLI container callers
    /// with producer-owned root provenance must use
    /// [`Self::record_with_mountinfo`].
    pub fn record(&self, command: Command) -> Result<Recording, Error> {
        let store = Self {
            data_dir: self.data_dir.clone(),
        };
        ptrace_completion::run(None, move |control| async move {
            let data = store.create_recording_dir()?;
            let exit_status =
                record_to_async(command, data.path(), Vec::new(), None, None, control).await?;
            store.commit_recording(data, exit_status)
        })
    }

    /// Records with mountinfo provenance captured from the active container.
    pub fn record_with_mountinfo(
        &self,
        command: Command,
        mountinfo_root_rewrites: Vec<detcore_model::config::MountInfoRootRewrite>,
    ) -> Result<Recording, Error> {
        let store = Self {
            data_dir: self.data_dir.clone(),
        };
        ptrace_completion::run(None, move |control| async move {
            let data = store.create_recording_dir()?;
            let mount_ids = capture_mountinfo_identity_order()?;
            let exit_status = record_to_async(
                command,
                data.path(),
                mountinfo_root_rewrites,
                Some(mount_ids),
                None,
                control,
            )
            .await?;
            store.commit_recording(data, exit_status)
        })
    }

    /// Creates a temporary directory for a recording that has not been committed yet.
    pub fn create_recording_dir(&self) -> Result<tempfile::TempDir, Error> {
        let tmp_data_dir = self.data_dir.join("tmp");

        fs::create_dir_all(&tmp_data_dir).with_context(|| {
            format!(
                "Failed to create recording directory: {}",
                self.data_dir.display()
            )
        })?;

        Ok(tempfile::TempDir::new_in(tmp_data_dir)?)
    }

    /// Commits a completed temporary recording to the recording store.
    pub fn commit_recording(
        &self,
        data: tempfile::TempDir,
        exit_status: ExitStatus,
    ) -> Result<Recording, Error> {
        let id = Id::unique();

        // Atomically move the temporary recording to its final location.
        fs::rename(data.keep(), self.data_dir.join(id.to_string()))?;

        self.update_last_id(&id)
            .with_context(|| format!("Failed to update {:?}", self.data_dir.join("last")))?;

        Ok(Recording { id, exit_status })
    }

    /// Replays the given recording ID.
    pub fn replay(&self, id: Id) -> Result<ExitStatus, Error> {
        let data = self.data_dir.join(id.to_string());
        replay_from(&data)
    }

    /// Replays the given recording ID with a gdbserver available to attach to.
    pub fn replay_with_gdbserver(&self, id: Id, port: u16) -> Result<ExitStatus, Error> {
        let data = self.data_dir.join(id.to_string());
        replay_with_gdbserver(&data, port)
    }

    /// Returns an iterator over the recordings.
    ///
    /// Use [`Self::recording_metadata`] to get more information about a recording.
    pub fn recordings(&self) -> impl Iterator<Item = Id> + use<> {
        fs::read_dir(&self.data_dir)
            .ok()
            .into_iter()
            .flatten()
            .filter_map(|entry| {
                let entry = entry.ok()?;

                if entry.file_type().ok()?.is_dir() {
                    Some(entry.file_name().to_str()?.parse::<Id>().ok()?)
                } else {
                    None
                }
            })
    }

    /// Returns all recordings, or an error if the data directory cannot be
    /// enumerated completely.
    ///
    /// A missing data directory has no recordings. Other filesystem errors are
    /// returned rather than being mistaken for an empty or partial inventory.
    pub fn try_recordings(&self) -> Result<Vec<Id>, Error> {
        let entries = match fs::read_dir(&self.data_dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "Failed to read recording inventory in {}",
                        self.data_dir.display()
                    )
                });
            }
        };

        collect_recording_ids(entries, &self.data_dir, fs::DirEntry::file_type)
    }

    /// Returns the metadata of a recording.
    pub fn recording_metadata(&self, id: Id) -> Result<Metadata, Error> {
        Metadata::load(&self.data_dir.join(id.to_string()))
    }

    /// Deletes a recording.
    pub fn remove(&self, id: Id) -> Result<(), Error> {
        let path = self.data_dir.join(id.to_string());

        // Before deleting anything, make sure this file exists. This may not be a
        // recording if this file does not exist.
        let metadata_path = path.join(METADATA_NAME);
        let metadata = fs::metadata(&metadata_path)
            .with_context(|| format!("Failed to find {:?}", metadata_path))?;

        if !metadata.is_file() {
            return Err(anyhow!("{:?} is not a file", metadata_path));
        }

        // Do a recursive delete on the directory. Note that this does not follow
        // symlinks.
        fs::remove_dir_all(path)?;

        Ok(())
    }

    /// Returns the last recorded ID.
    pub fn last_id(&self) -> Result<Id, Error> {
        Ok(fs::read_to_string(self.data_dir.join("last"))?.parse()?)
    }

    /// Atomically updates the last recording ID.
    fn update_last_id(&self, id: &Id) -> Result<(), Error> {
        let mut file = tempfile::NamedTempFile::new_in(self.data_dir.join("tmp"))?;
        write!(file, "{}", id)?;
        file.persist(self.data_dir.join("last"))?;
        Ok(())
    }
}

/// Capture the current mount namespace's raw IDs in the exact row/parent order
/// used by Detcore's canonical mountinfo mapping.
pub fn capture_mountinfo_identity_order() -> Result<Vec<u64>, Error> {
    mountinfo_identity_order_from_raw(&fs::read("/proc/self/mountinfo")?)
}

/// Producer provenance follows the guest mount model: ephemeral host seed rows
/// (other processes' squashfuse mounts) are not namespace members and must not
/// enter the captured identity order. Detcore drops the same rows with the same
/// filter when it captures the guest's mountinfo.
fn mountinfo_identity_order_from_raw(raw: &[u8]) -> Result<Vec<u64>, Error> {
    mountinfo_identity_order_from(&detcore_model::procfs::exclude_ephemeral_host_seed_mounts(
        raw,
    ))
}

fn mountinfo_identity_order_from(contents: &[u8]) -> Result<Vec<u64>, Error> {
    use std::collections::BTreeSet;

    let rows = detcore_model::procfs::parse_mountinfo(contents)
        .ok_or_else(|| anyhow!("malformed /proc/self/mountinfo"))?;
    let mut visible = Vec::new();
    let mut parents = Vec::new();
    let mut seen = BTreeSet::new();
    for row in rows {
        seen.insert(row.raw_mount_id);
        visible.push(row.raw_mount_id);
        parents.push(row.raw_parent_id);
    }
    for parent in parents {
        if seen.insert(parent) {
            visible.push(parent);
        }
    }
    Ok(visible)
}

impl<'a> From<Option<&'a PathBuf>> for HermitData {
    fn from(data_dir: Option<&'a PathBuf>) -> Self {
        data_dir.map_or_else(Self::new, Self::with_dir)
    }
}

/// Records to an existing directory. The caller must keep its backing resources
/// alive through any [`HermitCleanupUnconfirmed`] recovery. [`HermitData::record`]
/// owns its temporary recording directory inside the retained operation.
pub fn record_to(command: Command, dir: &Path) -> Result<ExitStatus, Error> {
    let dir = dir.to_owned();
    ptrace_completion::run(None, move |control| async move {
        record_to_async(command, &dir, Vec::new(), None, None, control).await
    })
}

/// Records with producer-owned mountinfo provenance. The directory-lifetime
/// obligation is the same as [`record_to`]. `local_networking` is stored in
/// the recording's `metadata.json`, and an autopilot replay uses that network.
pub fn record_to_with_mountinfo(
    command: Command,
    dir: &Path,
    mountinfo_root_rewrites: Vec<detcore_model::config::MountInfoRootRewrite>,
    local_networking: bool,
) -> Result<ExitStatus, Error> {
    let dir = dir.to_owned();
    ptrace_completion::run(None, move |control| async move {
        let mount_ids = capture_mountinfo_identity_order()?;
        record_to_async(
            command,
            &dir,
            mountinfo_root_rewrites,
            Some(mount_ids),
            Some(local_networking),
            control,
        )
        .await
    })
}

async fn record_to_async(
    command: Command,
    dir: &Path,
    mountinfo_root_rewrites: Vec<detcore_model::config::MountInfoRootRewrite>,
    mountinfo_mount_ids: Option<Vec<u64>>,
    local_networking: Option<bool>,
    control: std::rc::Rc<ptrace_completion::Control>,
) -> Result<ExitStatus, Error> {
    let report = SkidOvershootReport::begin(true);
    let result = async {
        Record::spawn_with_mountinfo(
            command,
            dir,
            mountinfo_root_rewrites,
            mountinfo_mount_ids,
            local_networking,
        )
        .await?
        .wait(control)
        .await
    }
    .await;
    report.finish(result)
}

/// Records and captures output. Keep the directory's backing resources alive
/// through any cleanup checkpoint, as for [`record_to`].
pub fn record_with_output(command: Command, dir: &Path) -> Result<Output, Error> {
    let dir = dir.to_owned();
    ptrace_completion::run(None, move |control| async move {
        record_with_output_async(command, &dir, Vec::new(), None, None, control).await
    })
}

/// Records with captured output and exact mountinfo provenance.
/// `local_networking` is as for [`record_to_with_mountinfo`].
pub fn record_with_output_with_mountinfo(
    command: Command,
    dir: &Path,
    mountinfo_root_rewrites: Vec<detcore_model::config::MountInfoRootRewrite>,
    local_networking: bool,
) -> Result<Output, Error> {
    let dir = dir.to_owned();
    ptrace_completion::run(None, move |control| async move {
        let mount_ids = capture_mountinfo_identity_order()?;
        record_with_output_async(
            command,
            &dir,
            mountinfo_root_rewrites,
            Some(mount_ids),
            Some(local_networking),
            control,
        )
        .await
    })
}

async fn record_with_output_async(
    mut command: Command,
    dir: &Path,
    mountinfo_root_rewrites: Vec<detcore_model::config::MountInfoRootRewrite>,
    mountinfo_mount_ids: Option<Vec<u64>>,
    local_networking: Option<bool>,
    control: std::rc::Rc<ptrace_completion::Control>,
) -> Result<Output, Error> {
    let report = SkidOvershootReport::begin(true);
    command.stdin(Stdio::null());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    let result = async {
        Record::spawn_with_mountinfo(
            command,
            dir,
            mountinfo_root_rewrites,
            mountinfo_mount_ids,
            local_networking,
        )
        .await?
        .wait_with_output(control)
        .await
    }
    .await;
    report.finish(result)
}

/// Replays an existing recording. Materialization and chroot guards remain with
/// the original runtime through any cleanup checkpoint.
pub fn replay_from(dir: &Path) -> Result<ExitStatus, Error> {
    replay_plain(dir, None, &[])
}

/// Replays with a gdb server.
pub fn replay_with_gdbserver(dir: &Path, port: u16) -> Result<ExitStatus, Error> {
    replay_plain(dir, Some(port), &[])
}

/// Replays with a gdb server and mounts inside the replay chroot.
pub fn replay_with_gdbserver_and_mounts(
    dir: &Path,
    port: u16,
    mounts: &[Mount],
) -> Result<ExitStatus, Error> {
    replay_plain(dir, Some(port), mounts)
}

fn replay_plain(dir: &Path, port: Option<u16>, mounts: &[Mount]) -> Result<ExitStatus, Error> {
    let dir = dir.to_owned();
    let mounts = mounts.to_vec();
    ptrace_completion::run(None, move |control| async move {
        let report = SkidOvershootReport::begin(true);
        let result = async {
            Replay::spawn(&dir, false, port, &mounts)
                .await?
                .wait(control)
                .await
        }
        .await;
        report.finish(result)
    })
}

/// Replays with captured output.
pub fn replay_with_output(dir: &Path) -> Result<Output, Error> {
    replay_with_output_and_mounts(dir, &[])
}

/// Replays with captured output and mounts inside the replay chroot.
pub fn replay_with_output_and_mounts(dir: &Path, mounts: &[Mount]) -> Result<Output, Error> {
    let dir = dir.to_owned();
    let mounts = mounts.to_vec();
    ptrace_completion::run(None, move |control| async move {
        let report = SkidOvershootReport::begin(true);
        let result = async {
            Replay::spawn(&dir, true, None, &mounts)
                .await?
                .wait_with_output(control)
                .await
        }
        .await;
        report.finish(result)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    static SKID_OVERSHOOT_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// The launch-time capture drops ephemeral host seed rows before ordering,
    /// so a seed's raw ID never enters the provenance Detcore checks guest
    /// views against, and every retained row keeps its place.
    #[test]
    fn identity_capture_excludes_host_seed_mounts_and_keeps_order() {
        let raw = b"10 1 8:1 / / rw - ext4 /dev/root rw\n\
                    76 10 0:50 / /mnt/xarfuse/uid-1/e62a203d-seed-nspid4026531836_cgpid16161-ns-4026531832 rw - fuse.squashfuse_ll squashfuse_ll rw\n\
                    20 10 0:7 / /test rw - tmpfs tmpfs rw\n\
                    30 10 0:51 / /mnt/xarfuse/stable-release rw - fuse.squashfuse_ll squashfuse_ll rw\n";
        let order = mountinfo_identity_order_from_raw(raw).unwrap();
        assert!(!order.contains(&76), "seed mount ID captured: {order:?}");
        assert_eq!(order, [10, 20, 30, 1]);
        assert_eq!(
            mountinfo_identity_order_from(raw).unwrap(),
            [10, 76, 20, 30, 1],
            "control: without the filter the seed ID is captured"
        );
    }

    #[test]
    fn kvm_mountinfo_config_preserves_captured_provenance() {
        let rewrite = detcore_model::config::MountInfoRootRewrite {
            raw_mount_id: 20,
            deterministic_root: b"/".to_vec(),
            raw_root_prefix: Some(b"/private-source".to_vec()),
            deterministic_root_prefix: Some(b"/tmp".to_vec()),
            raw_mountpoint_prefix: Some(b"/private-source".to_vec()),
            deterministic_mountpoint_prefix: Some(b"/tmp".to_vec()),
        };
        let mut config = DetConfig {
            mountinfo_mount_ids: vec![10, 20, 30],
            mountinfo_mount_ids_captured: true,
            mount_id_assignment_order: vec![700, 701],
            mountinfo_root_rewrites: vec![rewrite],
            mountinfo_device_rewrites: vec![(libc::makedev(0, 1), libc::makedev(8, 1))],
            ..DetConfig::default()
        };
        let mut expected = config.clone();
        expected.mountinfo_device_rewrites.clear();
        let mut captures = 0;
        prepare_kvm_mountinfo_config(&mut config, || {
            captures += 1;
            mountinfo_identity_order_from(
                b"10 30 8:1 / / rw - ext4 /dev/root rw\n\
                  20 10 0:7 / /test rw - tmpfs tmpfs rw\n",
            )
        })
        .unwrap();
        assert_eq!(
            captures, 1,
            "captured provenance must be checked before any guest read"
        );
        assert_eq!(
            serde_json::to_value(&config).unwrap(),
            serde_json::to_value(&expected).unwrap(),
            "root rewrites, listed and assignment orders and every other setting must remain intact"
        );

        let mut empty = DetConfig {
            mountinfo_mount_ids_captured: true,
            ..DetConfig::default()
        };
        let mut captures = 0;
        prepare_kvm_mountinfo_config(&mut empty, || {
            captures += 1;
            mountinfo_identity_order_from(b"")
        })
        .unwrap();
        assert_eq!(captures, 1);
        assert!(empty.mountinfo_mount_ids.is_empty());
        assert!(empty.mountinfo_mount_ids_captured);

        // Both captures describe the same completed namespace. An ordered
        // subset is valid for some later guest views, but not this boundary.
        for actual in [
            vec![10, 20],
            vec![10, 20, 30, 40],
            vec![20, 10, 30],
            vec![10, 20, 31],
            vec![10, 20, 20, 30],
            vec![],
        ] {
            let mut stale = config.clone();
            stale.mountinfo_device_rewrites = vec![(libc::makedev(0, 1), libc::makedev(8, 1))];
            let before = serde_json::to_value(&stale).unwrap();
            let error = prepare_kvm_mountinfo_config(&mut stale, || Ok(actual)).unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("do not match the backend namespace")
            );
            assert_eq!(serde_json::to_value(&stale).unwrap(), before);
        }
        let before = serde_json::to_value(&empty).unwrap();
        let error = prepare_kvm_mountinfo_config(&mut empty, || Ok(vec![10])).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("do not match the backend namespace")
        );
        assert_eq!(serde_json::to_value(&empty).unwrap(), before);
    }

    #[test]
    fn kvm_mountinfo_config_captures_validated_ambient_namespace() {
        let contents = b"80 20 8:1 / / rw - ext4 /dev/root rw\n\
                         10 80 0:7 / /test rw - tmpfs tmpfs rw\n";
        let mut config = DetConfig::default();
        let mut expected = config.clone();
        expected.mountinfo_mount_ids = vec![80, 10, 20];
        expected.mountinfo_mount_ids_captured = true;
        prepare_kvm_mountinfo_config(&mut config, || mountinfo_identity_order_from(contents))
            .unwrap();
        assert_eq!(
            serde_json::to_value(&config).unwrap(),
            serde_json::to_value(&expected).unwrap(),
            "visible rows precede parent-only IDs and no guest setting changes"
        );
        assert!(detcore_model::procfs::mount_ids_are_ordered_subset(
            &[80, 10],
            &config.mountinfo_mount_ids
        ));
        assert!(!detcore_model::procfs::mount_ids_are_ordered_subset(
            &[10, 80],
            &config.mountinfo_mount_ids
        ));
        assert!(!detcore_model::procfs::mount_ids_are_ordered_subset(
            &[80, 99],
            &config.mountinfo_mount_ids
        ));
        assert!(!detcore_model::procfs::mount_ids_are_ordered_subset(
            &[80, 80],
            &config.mountinfo_mount_ids
        ));

        let mut empty = DetConfig::default();
        prepare_kvm_mountinfo_config(&mut empty, || mountinfo_identity_order_from(b"")).unwrap();
        assert!(empty.mountinfo_mount_ids.is_empty());
        assert!(empty.mountinfo_mount_ids_captured);
    }

    #[test]
    fn kvm_mountinfo_config_refuses_missing_or_malformed_capture() {
        let valid = b"10 20 8:1 / / rw - ext4 /dev/root rw\n";
        for captured in [false, true] {
            let config = DetConfig {
                mountinfo_mount_ids: if captured { vec![10, 20] } else { vec![] },
                mountinfo_mount_ids_captured: captured,
                mount_id_assignment_order: if captured { vec![700, 701] } else { vec![] },
                mountinfo_device_rewrites: vec![(libc::makedev(0, 1), libc::makedev(8, 1))],
                ..DetConfig::default()
            };
            for contents in [
                b"malformed".as_slice(),
                b"10 20 8:1 / / rw - ext4 /dev/root rw\n10 20 8:1 / / rw - ext4 /dev/root rw\n",
                b"10 20 8:1 / / rw unknown:7 - ext4 /dev/root rw\n",
            ] {
                let mut candidate = config.clone();
                let before = serde_json::to_value(&candidate).unwrap();
                let error = prepare_kvm_mountinfo_config(&mut candidate, || {
                    mountinfo_identity_order_from(contents)
                })
                .unwrap_err();
                assert!(format!("{error:#}").contains("malformed /proc/self/mountinfo"));
                assert_eq!(serde_json::to_value(&candidate).unwrap(), before);
            }
            let mut candidate = config;
            let before = serde_json::to_value(&candidate).unwrap();
            let error = prepare_kvm_mountinfo_config(&mut candidate, || {
                Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied).into())
            })
            .unwrap_err();
            assert!(
                error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::PermissionDenied)
            );
            assert_eq!(serde_json::to_value(&candidate).unwrap(), before);
        }
        assert_eq!(mountinfo_identity_order_from(valid).unwrap(), [10, 20]);
    }

    #[test]
    fn kvm_mountinfo_config_refuses_uncaptured_provenance_vectors() {
        for (listed, assigned) in [(vec![10], vec![]), (vec![], vec![700])] {
            let mut config = DetConfig {
                mountinfo_mount_ids: listed,
                mount_id_assignment_order: assigned,
                ..DetConfig::default()
            };
            let before = serde_json::to_value(&config).unwrap();
            let error = prepare_kvm_mountinfo_config(&mut config, || {
                panic!("must not overwrite incomplete provenance")
            })
            .unwrap_err();
            assert!(error.to_string().contains("no producer-captured namespace"));
            assert_eq!(serde_json::to_value(&config).unwrap(), before);
        }
    }

    #[test]
    fn skid_overshoot_report_covers_success_error_and_disabled_backends() {
        let _lock = SKID_OVERSHOOT_TEST_LOCK.lock().unwrap();
        let _ = reverie::take_skid_overshoot_count();

        reverie::record_skid_overshoot();
        let empty_run = SkidOvershootReport::begin(true);
        assert_eq!(
            reverie::take_skid_overshoot_count(),
            0,
            "begin must clear evidence from a previous invocation"
        );
        empty_run.finish(Ok::<_, Error>(())).unwrap();

        let success = SkidOvershootReport::begin(true);
        reverie::record_skid_overshoot();
        reverie::record_skid_overshoot();
        let error = success.finish(Ok::<_, Error>(7)).unwrap_err();
        assert_eq!(
            error
                .downcast_ref::<SkidOvershootError>()
                .expect("skid refusal must remain typed")
                .count(),
            2
        );
        assert!(
            error
                .to_string()
                .contains("observed 2 HERMIT_SKID_OVERSHOOT report(s)"),
            "{error:#}"
        );
        assert!(
            error
                .to_string()
                .contains("deterministic execution was not established"),
            "{error:#}"
        );
        assert_eq!(reverie::take_skid_overshoot_count(), 0);

        let error = SkidOvershootReport::begin(true);
        reverie::record_skid_overshoot();
        let error = error
            .finish(Err::<(), _>(anyhow!("guest failed")))
            .unwrap_err();
        assert!(error.downcast_ref::<SkidOvershootError>().is_some());
        let error = format!("{error:#}");
        assert!(error.contains("guest failed"), "{error}");
        assert!(
            error.contains("observed 1 HERMIT_SKID_OVERSHOOT report(s)"),
            "{error}"
        );
        assert_eq!(reverie::take_skid_overshoot_count(), 0);

        reverie::record_skid_overshoot();
        let disabled = SkidOvershootReport::begin(false);
        disabled.finish(Ok::<_, Error>(())).unwrap();
        assert_eq!(
            reverie::take_skid_overshoot_count(),
            1,
            "a backend that cannot produce ptrace PMU overshoots must not consume another run's evidence"
        );
    }

    #[test]
    fn only_ptrace_hosted_backends_consume_skid_overshoot_reports() {
        for backend in [Backend::Ptrace, Backend::E9patch] {
            assert!(backend.uses_ptrace_pmu_timers(), "{backend:?}");
        }
        for backend in [Backend::Dbt, Backend::Sabre, Backend::Kvm] {
            assert!(!backend.uses_ptrace_pmu_timers(), "{backend:?}");
        }
        // In-guest Detcore has no ptrace tracer to arm the PMU timer from.
        assert!(!Backend::Liteinst.uses_ptrace_pmu_timers());
        assert!(!Backend::InGuestTrap.uses_ptrace_pmu_timers());
    }

    #[cfg(feature = "liteinst")]
    fn write_guest_program(directory: &Path, name: &str, bytes: &[u8], mode: u32) -> PathBuf {
        let path = directory.join(name);
        fs::write(&path, bytes).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
        // The kernel may clear a set-group-ID bit that this user cannot set;
        // a test that silently lost its bit would test nothing.
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o7777,
            mode,
            "{}",
            path.display()
        );
        path
    }

    #[cfg(feature = "liteinst")]
    fn shebang_to(interpreter: &Path) -> Vec<u8> {
        let mut script = b"#!".to_vec();
        script.extend_from_slice(interpreter.as_os_str().as_bytes());
        script.extend_from_slice(b" -e\necho script\n");
        script
    }

    #[test]
    #[cfg(feature = "liteinst")]
    fn in_guest_liteinst_runs_a_dynamically_linked_x86_64_program() {
        use interp::startup_fixtures::dynamic_executable;

        let directory = tempfile::tempdir().unwrap();
        let program = write_guest_program(
            directory.path(),
            "dynamic",
            &dynamic_executable(false),
            0o755,
        );
        assert_eq!(
            in_guest_liteinst_program_gap(&mut Command::new(&program)).unwrap(),
            None
        );
    }

    #[test]
    #[cfg(feature = "liteinst")]
    fn in_guest_liteinst_refuses_programs_that_start_before_the_runtime() {
        use interp::startup_fixtures::*;

        let directory = tempfile::tempdir().unwrap();
        let cases: [(&str, Vec<u8>, &str); 4] = [
            (
                "static",
                elf(EM_X86_64, &[(PT_LOAD, b"code")]),
                "is statically linked",
            ),
            (
                "preinit",
                dynamic_executable(true),
                "has a DT_PREINIT_ARRAY",
            ),
            (
                "aarch64",
                elf(EM_AARCH64, &[(PT_INTERP, INTERP)]),
                "is not a 64-bit x86-64 ELF image",
            ),
            (
                "text",
                b"plain text, no #! line\n".to_vec(),
                "is neither an ELF executable nor a #! script",
            ),
        ];
        for (name, bytes, reason) in cases {
            let program = write_guest_program(directory.path(), name, &bytes, 0o755);
            let gap = in_guest_liteinst_program_gap(&mut Command::new(&program))
                .unwrap()
                .unwrap_or_else(|| panic!("{name} was not refused"));
            assert!(
                gap.contains(reason) && gap.contains(&program.display().to_string()),
                "{name}: {gap}"
            );
        }
    }

    /// A failure recorded during an in-guest run fails the settle step and
    /// latches a determinism loss, so verification refuses the run (Codex
    /// implementation review, HIGH 3 and 4).
    #[tokio::test]
    #[cfg(feature = "liteinst")]
    async fn a_recorded_in_guest_failure_fails_the_run_and_latches_a_loss() {
        use std::os::fd::FromRawFd;

        use reverie::GlobalTool;
        use reverie_rpc_transport::ConnectionAdmission;

        let config = DetConfig::default().with_backend(|backend| {
            backend.process_exits_complete_asynchronously = true;
        });
        let global_state = detcore::GlobalState::init_global_state(&config).await;
        let exits = in_guest_exits::InGuestExitAdmission::default();
        // Admitting before any reporter is attached is refused and recorded.
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, std::process::id() as i32, 0) };
        assert!(fd >= 0);
        let pidfd = unsafe { std::os::fd::OwnedFd::from_raw_fd(fd as i32) };
        assert!(exits.admit(pidfd).is_err());
        let error = settle_in_guest_exits(&exits, &global_state)
            .await
            .expect_err("a recorded failure must fail the run");
        assert!(
            error
                .to_string()
                .starts_with("in-guest LiteInst refused a guest connection:"),
            "{error}"
        );
        assert!(detcore::detlog::determinism_loss().is_some());
    }

    #[test]
    fn in_guest_trap_is_in_guest_liteinst_and_forces_site_patching_off() {
        assert!(Backend::Liteinst.is_in_guest_liteinst());
        assert!(Backend::InGuestTrap.is_in_guest_liteinst());
        for backend in [
            Backend::Ptrace,
            Backend::Dbt,
            Backend::Sabre,
            Backend::Kvm,
            Backend::E9patch,
        ] {
            assert!(!backend.is_in_guest_liteinst(), "{backend:?}");
            assert_eq!(backend.runtime_name(), backend.as_str(), "{backend:?}");
            assert_eq!(forced_site_patching_value(backend), None, "{backend:?}");
        }
        assert_eq!(Backend::InGuestTrap.as_str(), "in-guest-trap");
        assert_eq!(Backend::InGuestTrap.runtime_name(), "liteinst");
        assert_eq!(Backend::Liteinst.runtime_name(), "liteinst");
        // liteinst leaves the variable to the guest environment.
        assert_eq!(forced_site_patching_value(Backend::Liteinst), None);
        assert_eq!(forced_site_patching_value(Backend::InGuestTrap), Some("0"));
        // The CLI spelling parses to the variant and back.
        assert_eq!(
            <Backend as ValueEnum>::from_str("in-guest-trap", false),
            Ok(Backend::InGuestTrap)
        );
        assert!(Backend::ALL.contains(&Backend::InGuestTrap));
        for backend in Backend::ALL {
            let name = backend
                .to_possible_value()
                .expect("every backend is a visible CLI value");
            assert_eq!(name.get_name(), backend.as_str(), "{backend:?}");
        }
    }

    /// in-guest-trap sets `REVERIE_LITEINST_SITE_PATCHING=0` on the guest
    /// command when the guest environment leaves it unset or already `0`, and
    /// refuses, before anything starts, an environment that sets any other
    /// value; the refusal names the value and both ways out. liteinst neither
    /// refuses nor changes the variable.
    #[test]
    #[cfg(feature = "liteinst")]
    fn in_guest_trap_refuses_a_guest_environment_that_turns_site_patching_on() {
        let name = reverie_liteinst::SITE_PATCHING_ENV;
        let mut unset = Command::new("/bin/true");
        unset.env_remove(name);
        assert_eq!(
            in_guest_trap_site_patching_conflict(&unset, Backend::InGuestTrap),
            None
        );
        force_in_guest_site_patching(&mut unset, Backend::InGuestTrap);
        assert_eq!(unset.get_env(name).as_deref(), Some(OsStr::new("0")));

        let mut zero = Command::new("/bin/true");
        zero.env(name, "0");
        assert_eq!(
            in_guest_trap_site_patching_conflict(&zero, Backend::InGuestTrap),
            None
        );

        for value in ["1", "", "yes", " 0"] {
            let mut command = Command::new("/bin/true");
            command.env(name, value);
            let conflict = in_guest_trap_site_patching_conflict(&command, Backend::InGuestTrap)
                .unwrap_or_else(|| panic!("{value:?} was accepted"));
            assert!(conflict.contains(&format!("{name}={value},")), "{conflict}");
            assert!(conflict.contains("--backend=liteinst"), "{conflict}");
            assert!(conflict.contains("drop the variable"), "{conflict}");
            // The whole refusal path stops on it first, before the program,
            // kernel or timeslice checks, and leaves the value untouched.
            let error = refuse_in_guest_liteinst_run(
                &mut command,
                &DetConfig::default(),
                Backend::InGuestTrap,
            )
            .unwrap_err();
            let refusal = error
                .downcast_ref::<LiteinstInGuestRefusal>()
                .expect("a typed in-guest refusal, which the CLI reports as exit 122");
            assert!(
                refusal.to_string().starts_with(
                    "--backend=in-guest-trap (in-guest LiteInst, site patching off) refuses \
                     this run: the guest environment sets"
                ),
                "{refusal}"
            );
            assert_eq!(command.get_env(name).as_deref(), Some(OsStr::new(value)));

            // liteinst takes the variable from the guest environment as it is.
            let mut liteinst = Command::new("/bin/true");
            liteinst.env(name, value);
            assert_eq!(
                in_guest_trap_site_patching_conflict(&liteinst, Backend::Liteinst),
                None
            );
            force_in_guest_site_patching(&mut liteinst, Backend::Liteinst);
            assert_eq!(liteinst.get_env(name).as_deref(), Some(OsStr::new(value)));
        }
    }

    /// The coordinator's postcondition for an in-guest-trap run: a record
    /// with no patched and no candidate site passes; a patched site, a
    /// candidate, an unmeasured count or a missing record is refused, with a
    /// typed in-guest refusal and a determinism loss. Other backends are not
    /// held to it.
    #[test]
    #[cfg(feature = "liteinst")]
    fn an_in_guest_trap_run_that_patched_a_site_is_refused_after_it_ends() {
        let record = |candidates: Option<u64>, patched: Option<u64>| {
            reverie::DispatchStats::new(
                "in-guest-trap",
                reverie::DispatchCounters::ZERO,
                reverie::SiteCounters {
                    candidates,
                    patched,
                    fell_back: Some(0),
                },
            )
        };
        let clean = record(Some(0), Some(0));
        assert_eq!(
            in_guest_trap_patching_evidence(Backend::InGuestTrap, Some(&clean), 1),
            None
        );
        for (candidates, patched, shown) in [
            (Some(3), Some(3), "3 patched and 3 candidate"),
            (Some(1), Some(0), "0 patched and 1 candidate"),
            (Some(0), None, "unmeasured patched and 0 candidate"),
        ] {
            let reason = in_guest_trap_patching_evidence(
                Backend::InGuestTrap,
                Some(&record(candidates, patched)),
                1,
            )
            .unwrap_or_else(|| panic!("{candidates:?} {patched:?} was accepted"));
            assert!(reason.contains(shown), "{reason}");
        }
        assert!(
            in_guest_trap_patching_evidence(Backend::InGuestTrap, None, 1)
                .is_some_and(|reason| reason.contains("no LiteInst dispatch record"))
        );
        // An empty record from guests that collected nothing is not zero.
        assert!(
            in_guest_trap_patching_evidence(Backend::InGuestTrap, Some(&clean), 0)
                .is_some_and(|reason| reason.contains("no guest process reported"))
        );
        let patched = record(Some(3), Some(3));
        assert_eq!(
            in_guest_trap_patching_evidence(Backend::Liteinst, Some(&patched), 1),
            None
        );
        assert_eq!(
            in_guest_trap_patching_evidence(Backend::Liteinst, None, 0),
            None
        );

        refuse_in_guest_trap_patching(Backend::InGuestTrap, None).unwrap();
        let error = refuse_in_guest_trap_patching(
            Backend::InGuestTrap,
            in_guest_trap_patching_evidence(Backend::InGuestTrap, Some(&patched), 1),
        )
        .unwrap_err();
        assert!(
            error.downcast_ref::<LiteinstInGuestRefusal>().is_some(),
            "{error:#}"
        );
        assert!(
            error
                .to_string()
                .starts_with("--backend=in-guest-trap (in-guest LiteInst, site patching off)"),
            "{error:#}"
        );
        assert!(
            detcore::detlog::determinism_loss().is_some_and(|loss| loss.contains("3 patched")),
            "the refusal must record a determinism loss"
        );
    }

    /// Only an in-guest-trap run tells every Tool image to check the settings
    /// its runtime captured, and that run always collects the statistics the
    /// check requires.
    #[test]
    #[cfg(feature = "liteinst")]
    fn only_in_guest_trap_asks_tool_images_to_check_their_runtime() {
        for backend in Backend::ALL {
            let checks = in_guest_site_patching_off(backend);
            assert_eq!(checks, backend == Backend::InGuestTrap, "{backend:?}");
            if checks {
                assert!(in_guest_stats_request(backend, &None).is_enabled());
            }
        }
        assert!(!in_guest_stats_request(Backend::Liteinst, &None).is_enabled());
    }

    #[test]
    #[cfg(feature = "liteinst")]
    fn in_guest_liteinst_refuses_a_kernel_older_than_6_5() {
        for release in [
            "6.5.0",
            "6.5",
            "6.10.3-200.fc40.x86_64",
            "7.1.3-0_fbk0_rc18",
            "10.0",
        ] {
            assert_eq!(in_guest_liteinst_kernel_gap(release), None, "{release}");
        }
        for release in ["6.4.16", "5.15.0-91-generic", "4.19"] {
            let gap = in_guest_liteinst_kernel_gap(release)
                .unwrap_or_else(|| panic!("{release} was not refused"));
            assert!(gap.contains("Linux 6.5 or later"), "{gap}");
            assert!(gap.contains(release), "{gap}");
        }
        for release in ["", "unknown", "6"] {
            assert!(
                in_guest_liteinst_kernel_gap(release).is_some(),
                "{release:?}"
            );
        }
        // No assertion about the kernel running the tests: that would make
        // the test's result depend on the host, and the releases above
        // already test both sides of the floor.
    }

    #[test]
    #[cfg(feature = "liteinst")]
    fn in_guest_liteinst_refuses_secure_execution_images() {
        use interp::startup_fixtures::dynamic_executable;

        let directory = tempfile::tempdir().unwrap();
        let bytes = dynamic_executable(false);
        let gap = |name: &str, mode: u32| {
            let program = write_guest_program(directory.path(), name, &bytes, mode);
            in_guest_liteinst_program_gap(&mut Command::new(&program)).unwrap()
        };
        for (name, mode) in [("setuid", 0o4755), ("setgid", 0o2755)] {
            let refusal = gap(name, mode).unwrap_or_else(|| panic!("{name} was not refused"));
            assert!(refusal.contains("secure-execution mode"), "{refusal}");
        }
        // Without group execute the kernel treats set-group-ID as mandatory
        // locking, not a credential change, so the loader honours LD_PRELOAD.
        assert_eq!(gap("setgid-no-group-exec", 0o2744), None);
    }

    #[test]
    #[cfg(feature = "liteinst")]
    fn in_guest_liteinst_checks_scripts_through_their_interpreter() {
        use interp::startup_fixtures::*;

        let directory = tempfile::tempdir().unwrap();
        let dynamic = write_guest_program(
            directory.path(),
            "dynamic",
            &dynamic_executable(false),
            0o755,
        );
        let static_elf = write_guest_program(
            directory.path(),
            "static",
            &elf(EM_X86_64, &[(PT_LOAD, b"code")]),
            0o755,
        );

        let script = write_guest_program(
            directory.path(),
            "to-static",
            &shebang_to(&static_elf),
            0o755,
        );
        let gap = in_guest_liteinst_program_gap(&mut Command::new(&script))
            .unwrap()
            .expect("a script run by a static interpreter was not refused");
        assert!(
            gap.contains(&static_elf.display().to_string()) && gap.contains("statically linked"),
            "{gap}"
        );

        // A relative interpreter resolves against the guest's working directory.
        let relative = write_guest_program(directory.path(), "relative", b"#!static\n", 0o755);
        let gap =
            in_guest_liteinst_program_gap(Command::new(&relative).current_dir(directory.path()))
                .unwrap()
                .expect("a relative static interpreter was not refused");
        assert!(gap.contains("statically linked"), "{gap}");

        // The kernel keeps a carriage return in the interpreter path, so a
        // script with CRLF line endings runs `interp\r`, not `interp`.
        write_guest_program(
            directory.path(),
            "interp",
            &dynamic_executable(false),
            0o755,
        );
        let interp_cr = write_guest_program(
            directory.path(),
            "interp\r",
            &elf(EM_X86_64, &[(PT_LOAD, b"code")]),
            0o755,
        );
        let mut crlf = b"#!".to_vec();
        crlf.extend_from_slice(directory.path().join("interp").as_os_str().as_bytes());
        crlf.extend_from_slice(b"\r\necho script\r\n");
        let crlf = write_guest_program(directory.path(), "crlf", &crlf, 0o755);
        let gap = in_guest_liteinst_program_gap(&mut Command::new(&crlf))
            .unwrap()
            .expect("a CRLF script run by a static interp\\r was not refused");
        assert!(
            gap.contains(&interp_cr.display().to_string()) && gap.contains("statically linked"),
            "{gap}"
        );

        // A #! line the kernel's script handler declines runs no interpreter:
        // the kernel fails the execve or tries another handler, so it is
        // refused.
        let mut truncated = b"#!/".to_vec();
        truncated.resize(256, b'a');
        for (name, bytes) in [
            ("empty", b"#! \t\nbody\n".to_vec()),
            ("truncated", truncated),
        ] {
            let program = write_guest_program(directory.path(), name, &bytes, 0o755);
            let gap = in_guest_liteinst_program_gap(&mut Command::new(&program))
                .unwrap()
                .unwrap_or_else(|| panic!("{name} was not refused"));
            assert!(
                gap.contains("the kernel's script handler declines it"),
                "{name}: {gap}"
            );
        }

        // Four nested scripts are followed to the ELF image; a fifth is refused.
        let mut target = dynamic;
        for level in 1..=5 {
            target = write_guest_program(
                directory.path(),
                &format!("level-{level}"),
                &shebang_to(&target),
                0o755,
            );
            let gap = in_guest_liteinst_program_gap(&mut Command::new(&target)).unwrap();
            if level <= MAX_INTERPRETER_DEPTH {
                assert_eq!(gap, None, "level {level}");
            } else {
                let gap = gap.expect("five nested scripts were not refused");
                assert!(gap.contains("nest more than 4 deep"), "{gap}");
            }
        }
    }

    #[test]
    #[cfg(feature = "liteinst")]
    fn in_guest_liteinst_refuses_a_fifo_without_blocking() {
        let directory = tempfile::tempdir().unwrap();
        let fifo = directory.path().join("fifo");
        let fifo_c = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo_c.as_ptr(), 0o700) }, 0);
        let script = write_guest_program(directory.path(), "to-fifo", &shebang_to(&fifo), 0o755);
        // A socket cannot be opened at all (ENXIO), and opening a device can
        // act on it; both must be refused, not reported as unreadable.
        let socket = directory.path().join("socket");
        let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let to_socket =
            write_guest_program(directory.path(), "to-socket", &shebang_to(&socket), 0o755);
        let device = PathBuf::from("/dev/null");
        let to_device =
            write_guest_program(directory.path(), "to-device", &shebang_to(&device), 0o755);
        // `./fifo` reaches the check through find_program's working-directory
        // branch, which does not require a regular file; the scripts reach the
        // FIFO, the socket and the device as their interpreters. Nothing opens
        // the FIFO for writing, so a blocking open or read would never return:
        // each check runs on its own thread and must answer within 10 s.
        let cases = [
            (
                "program",
                PathBuf::from("./fifo"),
                Some(directory.path().to_owned()),
                fs::canonicalize(&fifo).unwrap(),
            ),
            ("interpreter", script, None, fifo.clone()),
            ("socket interpreter", to_socket, None, socket.clone()),
            ("device interpreter", to_device, None, device),
        ];
        for (name, program, current_dir, shown) in cases {
            let (sender, receiver) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let mut command = Command::new(&program);
                if let Some(directory) = current_dir {
                    command.current_dir(directory);
                }
                let gap = in_guest_liteinst_program_gap(&mut command)
                    .map_err(|error| format!("{error:#}"));
                let _ = sender.send(gap);
            });
            let gap = receiver
                .recv_timeout(std::time::Duration::from_secs(10))
                .unwrap_or_else(|_| panic!("{name}: the check blocked on the FIFO"))
                .unwrap()
                .unwrap_or_else(|| panic!("{name}: the FIFO was not refused"));
            assert!(
                gap.contains(&format!("{} is not a regular file", shown.display())),
                "{name}: {gap}"
            );
        }
    }

    #[test]
    #[cfg(feature = "liteinst")]
    fn in_guest_liteinst_pins_the_program_the_launch_resolves() {
        use interp::startup_fixtures::*;

        let directory = tempfile::tempdir().unwrap();
        // A name that no host PATH directory holds.
        let name = "hermit-3520-static";
        let static_elf = write_guest_program(
            directory.path(),
            name,
            &elf(EM_X86_64, &[(PT_LOAD, b"code")]),
            0o755,
        );
        let search = format!("/nonexistent-directory:{}", directory.path().display());
        // The launch resolves the program with `find_program` on the command it
        // is given. Builds the command twice: once for the launch's own
        // resolution, once for the check, which must agree with it.
        let check = |build: &dyn Fn() -> Command| {
            let launch = build().find_program();
            let mut command = build();
            let arg0 = command.get_arg0().to_owned();
            let gap = in_guest_liteinst_program_gap(&mut command);
            match launch {
                Ok(program) => {
                    let gap = gap.unwrap().expect("the static image was not refused");
                    assert!(gap.contains("is statically linked"), "{gap}");
                    assert_eq!(Path::new(command.get_program()), program);
                    assert_eq!(command.get_arg0(), arg0);
                    // Pinned: the launch's own resolution returns it unchanged.
                    assert_eq!(command.find_program().unwrap(), program);
                    assert_eq!(
                        fs::canonicalize(&program).unwrap(),
                        fs::canonicalize(&static_elf).unwrap()
                    );
                    true
                }
                Err(_) => {
                    let error = gap.expect_err("the check found what the launch cannot");
                    assert!(
                        format!("{error:#}").contains("cannot find guest program"),
                        "{error:#}"
                    );
                    false
                }
            }
        };

        // A path with a slash resolves against the guest's working directory.
        assert!(check(&|| {
            let mut command = Command::new(format!("./{name}"));
            command.current_dir(directory.path());
            command
        }));
        // A bare name is searched for in the command's PATH once the base
        // environment is cleared, skipping entries without such a file.
        assert!(check(&|| {
            let mut command = Command::new(name);
            command.env_clear().env("PATH", &search);
            command
        }));
        // Without a cleared environment the launch searches Hermit's own PATH,
        // not the command's, so the static image is not found by either.
        assert!(!check(&|| {
            let mut command = Command::new(name);
            command.env("PATH", &search);
            command
        }));
        // A cleared environment without PATH (`--base-env=empty`) searches
        // Hermit's working directory, not the guest's and not /bin:/usr/bin.
        assert!(!check(&|| {
            let mut command = Command::new(name);
            command.env_clear().current_dir(directory.path());
            command
        }));
        // Not executable, or missing: the launch cannot run it, and neither
        // resolution finds it.
        fs::set_permissions(&static_elf, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(!check(&|| {
            let mut command = Command::new(name);
            command.env_clear().env("PATH", &search);
            command
        }));
        assert!(!check(&|| Command::new(&static_elf)));
        assert!(!check(&|| Command::new("hermit-no-such-program-3520")));
    }

    #[test]
    fn skid_overshoot_error_refuses_a_successful_result() {
        let _lock = SKID_OVERSHOOT_TEST_LOCK.lock().unwrap();
        let _ = reverie::take_skid_overshoot_count();
        assert!(take_skid_overshoot_error().is_none());

        reverie::record_skid_overshoot();
        let error = take_skid_overshoot_error().expect("recorded overshoot must be reported");
        assert!(
            error
                .to_string()
                .contains("observed 1 HERMIT_SKID_OVERSHOOT report(s)")
        );
        assert!(
            error
                .to_string()
                .contains("deterministic execution was not established")
        );
        assert_eq!(reverie::take_skid_overshoot_count(), 0);
    }

    #[test]
    fn skid_overshoot_report_preserves_success_without_an_overshoot() {
        let _lock = SKID_OVERSHOOT_TEST_LOCK.lock().unwrap();
        let _ = reverie::take_skid_overshoot_count();

        let report = SkidOvershootReport::begin(true);
        assert_eq!(report.finish(Ok::<_, Error>(7)).unwrap(), 7);
        assert_eq!(reverie::take_skid_overshoot_count(), 0);
    }

    #[test]
    fn skid_overshoot_count_can_cross_the_verify_boundary_with_the_output() {
        let _lock = SKID_OVERSHOOT_TEST_LOCK.lock().unwrap();
        let _ = reverie::take_skid_overshoot_count();

        let report = SkidOvershootReport::begin(true);
        reverie::record_skid_overshoot();
        let (value, count) = report.finish_with_count(Ok::<_, Error>(7)).unwrap();
        assert_eq!(value, 7);
        assert_eq!(count, 1);
        assert_eq!(reverie::take_skid_overshoot_count(), 0);
    }

    /// ⚠️ THE REGRESSION agent(hermit-007)'s CODEX LANE CAUGHT, PINNED FOR THE
    /// SIGNAL BAND. This test fails if the signal half stops being covered; it
    /// does NOT prove the predicate can never drift again, because the
    /// `122..=127` half is still a hard-coded literal that a future reserved
    /// value would fall outside exactly as 130 did.
    /// `liteinst_requires_forced_shutdown` hard-coded
    /// `Exited(122..=127)`. When the reserved set grew to include the signal
    /// band, `Exited(130)` fell outside it and is not `Signaled(_, _)` -- it is a
    /// status hermit CHOSE -- so LiteInst skipped the forced shutdown and
    /// `clean_up` could wait forever on `handle.await`. A reachable hang,
    /// introduced by widening the reserved set in one place and not the other.
    ///
    /// The 130 and 143 rows are the ones with teeth: revert the predicate to the
    /// bare range and they fail.
    #[test]
    #[cfg(feature = "liteinst")]
    fn liteinst_forced_shutdown_covers_every_status_hermit_reserves() {
        // The signal band. Not `Signaled`: hermit CHOSE these codes, because a
        // namespace init cannot produce a genuine signalled wait status.
        assert!(
            liteinst_requires_forced_shutdown(ExitStatus::Exited(130)),
            "128+SIGINT"
        );
        assert!(
            liteinst_requires_forced_shutdown(ExitStatus::Exited(143)),
            "128+SIGTERM"
        );
        assert!(
            liteinst_requires_forced_shutdown(ExitStatus::Exited(129)),
            "128+SIGHUP"
        );

        // The pre-existing reserved band, unchanged.
        assert!(liteinst_requires_forced_shutdown(ExitStatus::Exited(122)));
        assert!(liteinst_requires_forced_shutdown(ExitStatus::Exited(125)));
        assert!(liteinst_requires_forced_shutdown(ExitStatus::Exited(127)));

        // A real signal death still forces shutdown.
        assert!(liteinst_requires_forced_shutdown(ExitStatus::Signaled(
            nix::sys::signal::Signal::SIGKILL,
            false
        )));

        // ⚠️ CONTROLS, so a predicate that simply returned `true` would fail
        // this test rather than pass it. An ordinary guest exit must NOT force a
        // shutdown; 128 is excluded because there is no signal 0, and 200 is
        // above SIGRTMAX so it is a guest's own number.
        assert!(!liteinst_requires_forced_shutdown(ExitStatus::Exited(0)));
        assert!(!liteinst_requires_forced_shutdown(ExitStatus::Exited(1)));
        assert!(!liteinst_requires_forced_shutdown(ExitStatus::Exited(121)));
        assert!(!liteinst_requires_forced_shutdown(ExitStatus::Exited(128)));
        assert!(!liteinst_requires_forced_shutdown(ExitStatus::Exited(200)));
        // ⚠️ THE BAND EDGES, WHICH THIS TEST STOPPED SHORT OF. It went up to 143
        // and then jumped to 200, so `MAX_SIGNO` could shrink from 64 to 15 with
        // every row still green -- proved vacuous by agent(hermit-005)'s codex
        // lane. 192 is `128 + SIGRTMAX`, the last status in the band; 193 is the
        // first outside it. Testing a range in the middle pins nothing at either end.
        assert!(
            liteinst_requires_forced_shutdown(ExitStatus::Exited(192)),
            "128 + SIGRTMAX is still a signal death and must force shutdown"
        );
        assert!(
            !liteinst_requires_forced_shutdown(ExitStatus::Exited(193)),
            "above SIGRTMAX the status is the guest's own and must NOT force shutdown"
        );
    }

    /// ⚠️ The bare `NotFound` here was read as a git-worktree bug and cost real
    /// investigation time. The message must name the mount namespace, because
    /// the directory it is complaining about usually exists on the host.
    #[test]
    fn a_cwd_under_tmp_is_explained_by_the_container_tmpfs_not_by_a_missing_directory() {
        let error = std::io::Error::from(std::io::ErrorKind::NotFound);
        let rendered = format!(
            "{}",
            super::kvm_cwd_resolution_error(std::path::Path::new("/tmp/agent-checkout"), &error)
        );
        assert!(
            rendered.contains("private, empty tmpfs"),
            "must name the mechanism: {rendered}"
        );
        assert!(
            rendered.contains("--workdir"),
            "must name a way out: {rendered}"
        );
        assert!(
            rendered.contains("/tmp/agent-checkout"),
            "must still name the path asked for: {rendered}"
        );
    }

    /// The explanation must NOT be attached to failures it does not explain --
    /// an error that volunteers the wrong cause is what produced the original
    /// misdiagnosis, in the other direction.
    #[test]
    fn the_tmpfs_explanation_is_withheld_where_it_does_not_apply() {
        let not_found = std::io::Error::from(std::io::ErrorKind::NotFound);
        // ⚠️ A CONSTANT ABSOLUTE PATH, NOT ONE READ FROM THE ENVIRONMENT. The only
        // property this arm needs is "outside /tmp", and a literal that is outside
        // /tmp by construction supplies it without asking the runner anything.
        //
        // The earlier form wrote a developer-specific home path, which made
        // `scripts/check-portable-paths.sh` red on main -- correctly: that gate
        // rejects such a path anywhere in a tracked build or run file, tests and
        // COMMENTS included, so spelling one out even to explain this would trip
        // it. Replacing it by reading $HOME then made the test depend on the
        // runner instead: measured by `agent(codex-rev-2641)`, HOME unset, empty,
        // relative and /tmp-rooted gave three different fixtures and one failure.
        // A unit test that consults the environment is answering a question about
        // the environment.
        //
        // ⚠️ AND NO GUARD IS NEEDED HERE, WHICH IS THE POINT OF USING A CONSTANT.
        // The $HOME form carried `assert!(!outside_path.starts_with("/tmp"))` to
        // stop a /tmp-rooted HOME making this vacuous. That assertion could never
        // fire: the `!outside.contains("tmpfs")` check below sits ABOVE it in
        // source order and panics first in exactly that case. It was decorative,
        // and an assertion over a compile-time constant would be too, so there is
        // none. The fixture cannot drift into /tmp because nothing computes it.
        let outside_path = std::path::Path::new("/nonexistent-home/wt");
        let outside = format!(
            "{}",
            super::kvm_cwd_resolution_error(outside_path, &not_found)
        );
        assert!(!outside.contains("tmpfs"), "not a /tmp path: {outside}");

        // `/tmp` itself resolves in the guest -- the mount POINT exists -- so a
        // failure there is something else and must not be mislabelled.
        let tmp_itself = format!(
            "{}",
            super::kvm_cwd_resolution_error(std::path::Path::new("/tmp"), &not_found)
        );
        assert!(!tmp_itself.contains("tmpfs"), "/tmp itself: {tmp_itself}");

        // A different errno under /tmp is a different fault (permissions, say).
        let denied = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        let other = format!(
            "{}",
            super::kvm_cwd_resolution_error(std::path::Path::new("/tmp/x"), &denied)
        );
        assert!(!other.contains("tmpfs"), "not NotFound: {other}");
    }

    /// A build-flag message must not assert a machine condition it never tested.
    ///
    /// ⚠️ THIS IS THE FOURTH DIAGNOSTIC TONIGHT THAT NAMED AN UNESTABLISHED CAUSE,
    /// after the sigpipe checker, the manifest gate and the LiteInst pidfd error.
    /// "DBT support was not included in this build" reads as "this machine cannot
    /// run DBT", and cost four or five unnecessary abstentions across the fleet --
    /// agents declined to make DBT claims on a box where DynamoRIO is installed
    /// and `--features dbt` builds and runs. The absence is a build flag, and the
    /// message must say which flag and admit what it did not check.
    #[test]
    fn an_unbuilt_backend_names_the_flag_and_claims_nothing_about_the_machine() {
        let feature_disabled_backends: &[(Backend, &str)] = &[
            #[cfg(not(feature = "dbt"))]
            (Backend::Dbt, "dbt"),
            #[cfg(not(feature = "sabre"))]
            (Backend::Sabre, "sabre"),
            #[cfg(not(feature = "e9patch"))]
            (Backend::E9patch, "e9patch"),
            #[cfg(not(feature = "liteinst"))]
            (Backend::Liteinst, "liteinst"),
            // in-guest-trap is the same runtime behind the same feature.
            #[cfg(not(feature = "liteinst"))]
            (Backend::InGuestTrap, "liteinst"),
        ];

        for &(backend, feature) in feature_disabled_backends {
            let reason = backend
                .unavailable_reason()
                .expect("a feature-disabled backend must report why it is unavailable");
            assert!(
                reason.contains(feature),
                "{backend:?}: a build-flag message must name the flag to rebuild with, got: {reason}"
            );
            assert!(
                reason.contains("has not been checked"),
                "{backend:?}: a build-flag message must not imply the machine was tested, got: {reason}"
            );
        }
    }

    #[cfg(not(feature = "dbt"))]
    #[test]
    fn dbt_registry_diagnostic_redirects_to_a_source_build() {
        let reason = super::dbt_disabled_build_reason("hermit-run");
        assert!(reason.contains("not included in this registry release"));
        assert!(reason.contains("source build from https://github.com/rrnewton/hermit"));
        assert!(reason.contains("`third-party-backends` feature does not include DBT"));
        assert!(reason.contains("has not been checked"));

        let source_reason = super::dbt_disabled_build_reason("hermit");
        assert!(source_reason.contains("rebuild with `--features dbt`"));
        assert!(!source_reason.contains("not included in this registry release"));
    }

    #[cfg(not(feature = "sabre"))]
    #[tokio::test]
    async fn disabled_sabre_dispatch_refuses_before_resolving_the_program() {
        let program = "/hermit-missing-disabled-sabre-program";
        let error = super::dispatch_backend(
            super::Command::new(program),
            super::DetConfig::default(),
            false,
            &None,
            Backend::Sabre,
            None,
        )
        .await
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("the `sabre` feature is not enabled")
        );
        let error = super::dispatch_output_backend(
            super::Command::new(program),
            super::DetConfig::default(),
            false,
            &None,
            Backend::Sabre,
            None,
        )
        .await
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("the `sabre` feature is not enabled")
        );
    }

    #[test]
    #[cfg(feature = "dbt")]
    fn a_cold_dbt_runtime_diagnostic_is_not_feature_absence() {
        let reason = dbt_runtime_unavailable_reason_from(Err(io::Error::new(
            io::ErrorKind::NotFound,
            "Hermit DBT runtime was not built beside /cold-target/hermit or in its deps directory",
        )))
        .expect("a missing cold-target runtime must remain unavailable");

        assert!(reason.contains("was not built beside"));
        assert!(reason.contains("build the hermit binary and cdylib"));
        assert!(!reason.contains("not enabled in this build"));
        assert!(!reason.contains("has not been checked"));
    }
    #[cfg(feature = "sabre")]
    use std::ffi::OsStr;
    use std::fs;
    #[cfg(feature = "sabre")]
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    #[cfg(feature = "sabre")]
    use std::sync::Arc;
    #[cfg(feature = "sabre")]
    use std::time::Duration;

    use super::Backend;
    #[cfg(any(feature = "sabre", feature = "liteinst"))]
    use super::ExitStatus;
    use super::HermitData;
    use super::Id;
    #[cfg(feature = "sabre")]
    use super::SABRE_RPC_SOCKET_ENV;
    use super::collect_recording_ids;
    #[cfg(feature = "dbt")]
    use super::dbt_runtime_unavailable_reason;
    #[cfg(feature = "dbt")]
    use super::dbt_runtime_unavailable_reason_from;
    #[cfg(feature = "dbt")]
    use super::dynamorio_sdk_available;
    use super::ensure_backend_dispatch;
    #[cfg(feature = "dbt")]
    use super::is_dynamorio_sdk;
    use super::kvm_device_unavailable_reason;
    #[cfg(feature = "liteinst")]
    use super::liteinst_requires_forced_shutdown;
    #[cfg(feature = "dbt")]
    use super::liteinst_unavailable_reason;
    use super::output_backend_stdin_file;
    use super::prepare_backend_config;
    use super::reserve_output_stdin_snapshot;
    use super::resolve_kvm_shebang;
    #[cfg(feature = "sabre")]
    use super::resolve_sabre_binary_from;
    #[cfg(feature = "sabre")]
    use super::sabre_backend_evidence_line;
    #[cfg(feature = "sabre")]
    use super::sabre_program_needs_neutral_name;
    #[cfg(feature = "sabre")]
    use super::sabre_reach_state;
    #[cfg(feature = "sabre")]
    use super::sabre_uninstrumented_guest_message;
    #[cfg(feature = "sabre")]
    use super::shutdown_sabre_rpc;
    #[cfg(feature = "sabre")]
    use super::stage_sabre_program_in;
    #[cfg(feature = "sabre")]
    use super::stop_sabre_rpc_server;
    #[cfg(feature = "sabre")]
    use super::wait_for_sabre_rpc_disconnects;

    #[test]
    fn recording_inventory_preserves_missing_and_non_recording_cases() {
        let parent = tempfile::tempdir().expect("failed to create recording inventory parent");
        let missing = parent.path().join("missing");
        assert!(
            HermitData::with_dir(&missing)
                .try_recordings()
                .expect("a missing data directory should be empty")
                .is_empty()
        );

        let inventory = parent.path().join("inventory");
        fs::create_dir(&inventory).expect("failed to create recording inventory");
        let recording: Id = "0123456789abcdef0123456789abcdef"
            .parse()
            .expect("valid recording ID");
        fs::create_dir(inventory.join(recording.to_string()))
            .expect("failed to create recording directory");
        fs::create_dir(inventory.join("not-a-recording"))
            .expect("failed to create invalid-name directory");
        fs::write(
            inventory.join("fedcba9876543210fedcba9876543210"),
            b"not a directory",
        )
        .expect("failed to create valid-looking non-directory");

        assert_eq!(
            HermitData::with_dir(&inventory)
                .try_recordings()
                .expect("recording inventory should be readable"),
            vec![recording]
        );
        assert_eq!(
            HermitData::with_dir(&inventory)
                .recordings()
                .collect::<Vec<_>>(),
            vec![recording]
        );
    }

    #[test]
    fn recording_inventory_rejects_an_error_after_a_valid_entry() {
        let inventory = tempfile::tempdir().expect("failed to create recording inventory");
        let recording: Id = "0123456789abcdef0123456789abcdef"
            .parse()
            .expect("valid recording ID");
        fs::create_dir(inventory.path().join(recording.to_string()))
            .expect("failed to create recording directory");
        let valid_entry = fs::read_dir(inventory.path())
            .expect("failed to open recording inventory")
            .next()
            .expect("missing recording entry")
            .expect("failed to read recording entry");

        let entries = vec![
            Ok(valid_entry),
            Err(std::io::Error::other("injected entry failure")),
        ];
        let error = collect_recording_ids(entries, inventory.path(), fs::DirEntry::file_type)
            .expect_err("a partial inventory must not be returned");
        assert!(
            error
                .to_string()
                .contains("Failed to read an entry in recording inventory"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn recording_inventory_rejects_an_entry_inspection_error() {
        let inventory = tempfile::tempdir().expect("failed to create recording inventory");
        let recording = inventory.path().join("0123456789abcdef0123456789abcdef");
        fs::create_dir(&recording).expect("failed to create recording directory");
        let entry = fs::read_dir(inventory.path())
            .expect("failed to open recording inventory")
            .next()
            .expect("missing recording entry")
            .expect("failed to read recording entry");

        let error = collect_recording_ids([Ok(entry)], inventory.path(), |_| {
            Err(std::io::Error::other("injected inspection failure"))
        })
        .expect_err("an uninspectable entry must fail the inventory");
        assert!(
            error
                .to_string()
                .contains("Failed to inspect recording inventory entry"),
            "unexpected error: {error:#}"
        );
    }

    /// Regression test for the `hermit run --verify` empty-stdin bug: a pipe
    /// (non-seekable) fed to hermit must be buffered and replayed *identically*
    /// to every run of `--verify`. Before the fix the output-capturing backend
    /// used `Stdio::null()`, so piped input was silently dropped and both runs
    /// saw empty input (a false "deterministic" pass, e.g. `gcc -x c -`
    /// compiling nothing). This asserts the reserved snapshot replays the exact
    /// bytes twice.
    #[test]
    fn output_stdin_snapshot_replays_pipe_to_repeated_runs() {
        use std::io::Read;
        use std::io::Write;
        use std::os::fd::FromRawFd;

        // A pipe is non-seekable, exercising the buffer-into-tempfile path that
        // matches the real `echo prog | hermit run --verify` scenario.
        let mut fds = [0i32; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        // SAFETY: pipe(2) just returned two fresh owned descriptors.
        let read_end = unsafe { fs::File::from_raw_fd(fds[0]) };
        let mut write_end = unsafe { fs::File::from_raw_fd(fds[1]) };

        let payload = b"int main(){return 0;}\n";
        write_end.write_all(payload).unwrap();
        // Close the write end so draining the pipe hits EOF.
        drop(write_end);

        reserve_output_stdin_snapshot(Some(read_end)).unwrap();

        // Two successive runs (as `--verify` performs) must each read the full
        // payload from the start.
        for run in 0..2 {
            let mut file = output_backend_stdin_file()
                .unwrap()
                .unwrap_or_else(|| panic!("run {run}: expected a replayable stdin snapshot"));
            let mut got = Vec::new();
            file.read_to_end(&mut got).unwrap();
            assert_eq!(got, payload, "run {run} stdin replay mismatch");
        }

        let mut file = output_backend_stdin_file().unwrap().unwrap();
        let error = file
            .write_all(b"guest-must-not-change-the-snapshot")
            .expect_err("the replayed stdin must be read-only");
        assert_eq!(error.raw_os_error(), Some(libc::EBADF));
    }

    #[test]
    #[cfg(feature = "liteinst")]
    fn liteinst_reserved_failures_require_scheduler_cancellation() {
        for status in 122..=127 {
            assert!(liteinst_requires_forced_shutdown(ExitStatus::Exited(
                status
            )));
        }
        assert!(!liteinst_requires_forced_shutdown(ExitStatus::Exited(121)));
        assert!(!liteinst_requires_forced_shutdown(ExitStatus::Exited(128)));
    }

    /// A SaBRe guest that never reaches the coordinator ran with no Detcore in
    /// the loop at all. The diagnosis must say so in those terms -- a
    /// zero-syscall SaBRe run is not a weak result, it is *no* result -- and it
    /// must name the statically linked ELF case, which is the shape that
    /// reaches this path in practice because a static client has no dynamic
    /// loader through which SaBRe could regain control.
    #[test]
    #[cfg(feature = "sabre")]
    fn uninstrumented_sabre_guest_is_reported_as_no_determinization() {
        let message = sabre_uninstrumented_guest_message(&ExitStatus::Exited(0));
        assert!(
            message.contains("no determinization at all"),
            "must not present an uninstrumented run as a weaker guarantee: {message}"
        );
        assert!(
            message.contains("Detcore coordinator"),
            "must name the authority that was never reached: {message}"
        );
        assert!(
            message.contains("statically linked ELF"),
            "must name the dominant cause so the reader can act: {message}"
        );
        // The exit status is carried because a successful-looking status is
        // exactly what makes this failure mode dangerous.
        assert!(
            message.contains("Exited(0)"),
            "must carry the observed status: {message}"
        );
        assert!(
            sabre_uninstrumented_guest_message(&ExitStatus::Exited(139)).contains("Exited(139)"),
            "must carry a failing status too"
        );
    }

    /// The old two-way banner treated both `guest_rpc_observed=false` and an
    /// engaged zero-fallback run as `sabre-exercised`. Assert the three states
    /// together so no pair can collapse back onto one value.
    #[test]
    #[cfg(feature = "sabre")]
    fn sabre_reach_states_are_pairwise_distinct() {
        let no_detcore = sabre_reach_state(false, 0);
        let degraded = sabre_reach_state(true, 1);
        let exercised = sabre_reach_state(true, 0);

        assert_eq!(no_detcore, "no-detcore-reached");
        assert_eq!(degraded, "degraded-ptrace-fallback");
        assert_eq!(exercised, "sabre-exercised");
        assert_ne!(no_detcore, degraded);
        assert_ne!(no_detcore, exercised);
        assert_ne!(degraded, exercised);
        assert_eq!(
            sabre_reach_state(false, 9),
            "no-detcore-reached",
            "absence of a guest RPC must dominate any fallback count"
        );
    }

    #[test]
    #[cfg(feature = "sabre")]
    fn sabre_backend_fact_is_versioned_and_names_preplugin_coverage() {
        let exercised = super::sabre_ptrace::PathEvidence {
            schema: 1,
            guest_rpc_observed: true,
            ptrace_fallback_sites: 0,
            trusted_shared_object_sites: 2,
            trusted_shared_objects: vec!["/usr/lib/libc.so.6".to_owned()],
        };
        assert_eq!(
            sabre_backend_evidence_line(&exercised),
            ":: Backend: sabre static rewriting + ptrace runtime; run_mode=run; \
             evidence_schema=1; preplugin_coverage=absent; ptrace_fallback_sites=0; \
             trusted_shared_object_sites=2; guest_rpc_observed=true; \
             reach_state=sabre-exercised"
        );

        let unengaged = super::sabre_ptrace::PathEvidence {
            guest_rpc_observed: false,
            ..exercised
        };
        let fact = sabre_backend_evidence_line(&unengaged);
        assert!(fact.contains("preplugin_coverage=absent"));
        assert!(fact.contains("reach_state=no-detcore-reached"));
        assert!(!fact.contains("reach_state=sabre-exercised"));
    }

    #[test]
    fn liteinst_backend_config_preserves_the_requested_timeslice() {
        // Configuration must not silently clear a requested timeslice: the
        // in-guest LiteInst refusal relies on seeing it.
        let config = super::DetConfig::default();
        assert!(config.max_timeslice.is_some());
        for backend in [Backend::Liteinst, Backend::InGuestTrap] {
            assert!(
                prepare_backend_config(config.clone(), backend)
                    .max_timeslice
                    .is_some(),
                "{backend:?}"
            );
        }
        assert!(
            prepare_backend_config(config, Backend::Ptrace)
                .max_timeslice
                .is_some()
        );
    }

    #[test]
    fn in_guest_liteinst_cancels_killed_thread_rpcs() {
        let config = super::DetConfig::default();
        for backend in [Backend::Liteinst, Backend::InGuestTrap] {
            assert!(
                prepare_backend_config(config.clone(), backend)
                    .backend
                    .needs_killed_thread_rpc_cancellation,
                "{backend:?}"
            );
        }
        // LiteInst's cancellation changes no other backend.
        assert!(
            !prepare_backend_config(config, Backend::Ptrace)
                .backend
                .needs_killed_thread_rpc_cancellation
        );
    }

    #[test]
    fn sabre_backend_configures_process_local_capabilities() {
        let config = super::DetConfig::default();
        let sabre = prepare_backend_config(config.clone(), Backend::Sabre).backend;
        assert!(sabre.tool_shares_guest_descriptor_table);
        assert!(sabre.rediscovers_descriptors_after_exec);
        assert!(sabre.internal_pipe_turns_are_host_timed);
        assert!(sabre.loopback_pollers_yield_to_peers);
        assert!(sabre.guest_clock_reads_bypass_backend);
        assert!(sabre.virtualizes_syscall_clobbers);
        assert!(sabre.needs_killed_thread_rpc_cancellation);
        assert!(sabre.reports_physical_process_exits);
        assert!(sabre.signal_interrupts_external_syscalls);
        assert!(sabre.tracks_process_children);
        assert!(!sabre.runs_exit_robust_list);
        assert!(!sabre.requires_thread_directed_process_signals);
        assert!(!sabre.virtualizes_capability_prctls);
        assert!(!sabre.defers_vfork_child_registration);
        let ptrace = prepare_backend_config(config, Backend::Ptrace).backend;
        assert!(!ptrace.tool_shares_guest_descriptor_table);
        assert!(!ptrace.rediscovers_descriptors_after_exec);
        assert!(!ptrace.internal_pipe_turns_are_host_timed);
        assert!(!ptrace.loopback_pollers_yield_to_peers);
        assert!(!ptrace.guest_clock_reads_bypass_backend);
        assert!(!ptrace.virtualizes_syscall_clobbers);
        assert!(!ptrace.needs_killed_thread_rpc_cancellation);
        assert!(!ptrace.reports_physical_process_exits);
        assert!(!ptrace.signal_interrupts_external_syscalls);
        assert!(ptrace.tracks_process_children);
        assert!(ptrace.runs_exit_robust_list);
        assert!(!ptrace.requires_thread_directed_process_signals);
        assert!(!ptrace.virtualizes_capability_prctls);
        assert!(!ptrace.defers_vfork_child_registration);
    }

    #[test]
    fn kvm_reports_stored_metadata_timestamps_only_for_sequential_tool_threads() {
        let sequential = prepare_backend_config(
            super::DetConfig {
                sequentialize_threads: true,
                virtualize_metadata: true,
                ..super::DetConfig::default()
            },
            Backend::Kvm,
        );
        assert!(super::kvm_reports_stored_metadata_timestamps(&sequential));

        // Another guest thread could read the stored values before Detcore
        // rewrites them.
        let concurrent = super::DetConfig {
            sequentialize_threads: false,
            ..sequential.clone()
        };
        assert!(!super::kvm_reports_stored_metadata_timestamps(&concurrent));

        // Nothing rewrites the stored values.
        let host_metadata = super::DetConfig {
            virtualize_metadata: false,
            ..sequential
        };
        assert!(!super::kvm_reports_stored_metadata_timestamps(
            &host_metadata
        ));
    }

    #[test]
    fn kvm_backend_config_marks_concurrent_process_children() {
        let defaults = super::DetConfig::default();
        assert!(!defaults.sequentialize_threads);
        let nonsequential = prepare_backend_config(defaults, Backend::Kvm);
        assert!(nonsequential.backend.provides_process_signal_control);
        assert!(nonsequential.backend.emulates_child_waits);
        assert!(!may_observe_signal_dequeues(&nonsequential));
        assert!(!requires_signal_control(&nonsequential));

        let config = super::DetConfig {
            sequentialize_threads: true,
            ..super::DetConfig::default()
        };
        let kvm = prepare_backend_config(config, Backend::Kvm);
        assert!(may_observe_signal_dequeues(&kvm));
        assert!(requires_signal_control(&kvm));
        assert!(kvm.backend.needs_killed_thread_rpc_cancellation);
        assert!(kvm.backend.tracks_process_children);
        assert!(!kvm.backend.runs_exit_robust_list);
        assert!(!kvm.backend.requires_thread_directed_process_signals);
        assert!(kvm.backend.virtualizes_capability_prctls);
        assert!(kvm.backend.defers_vfork_child_registration);

        // A controlled run needs both the backend's process-signal control and
        // sequentialized threads; neither alone admits one.
        for backend in [
            Backend::Ptrace,
            Backend::Dbt,
            Backend::Sabre,
            Backend::Liteinst,
            Backend::InGuestTrap,
            Backend::E9patch,
        ] {
            let config = super::DetConfig {
                sequentialize_threads: true,
                ..super::DetConfig::default()
            };
            let prepared = prepare_backend_config(config, backend);
            assert!(!may_observe_signal_dequeues(&prepared), "{backend:?}");
            assert!(!requires_signal_control(&prepared), "{backend:?}");
        }
    }

    /// Whether Detcore asks the backend to install process signal control for
    /// `config`: the bound Reverie checks before it offers the control.
    fn may_observe_signal_dequeues(config: &DetConfig) -> bool {
        <super::Detcore as reverie::Tool>::may_observe_signal_dequeues(config)
    }

    /// Whether Reverie must refuse `config` on a runner that offers no
    /// process signal control, as Detcore's installation did before.
    fn requires_signal_control(config: &DetConfig) -> bool {
        <super::Detcore as reverie::Tool>::requires_signal_control(config)
    }

    /// The values the hand-written assignments in `prepare_backend_config` and
    /// `run_kvm` gave each backend before Detcore read
    /// [`reverie::BackendCapabilities`], transcribed field by field. A backend
    /// identity check became the capability it selected:
    /// `discover_live_file_metadata` split into the first four fields, the old
    /// `backend_reports_physical_process_exits` became two, and
    /// `backend_is_kvm` became five, of which the last two remain here:
    /// `refuses_nonleader_exec_with_enosys` became the backend's typed exec
    /// refusal (Reverie d78f770f), `failed_gettimeofday_may_store_host_time`
    /// the guest's storable-memory report (Reverie 8bfb450e) and
    /// `user_address_limit` the guest's user address limit query (Reverie
    /// 6f75cc4c). Four fields are newer than that transcription:
    /// `process_exits_complete_asynchronously` (Reverie 056c8cac), true only
    /// for in-guest LiteInst, whose process exits complete some time after the
    /// tool grants them, `supports_madv_dontneed` (Reverie 8d8697aa), true for
    /// every backend, and `reports_child_exit_publication` (Reverie 9cb6f7b7),
    /// true for ptrace and e9patch, whose tracer reports when the kernel
    /// publishes a process's exit to its parent, and
    /// `virtualizes_guest_sigalrm` (Reverie 7142ff8c), true only for in-guest
    /// LiteInst, whose runtime keeps a guest SIGALRM handler virtual (signal
    /// phase 1). `in-guest-trap` is in-guest LiteInst with site patching off,
    /// which changes how a syscall reaches the Tool and nothing the Tool host
    /// can do, so it has exactly in-guest LiteInst's row.
    /// DBT now also shares the guest descriptor table and rediscovers it after
    /// exec (Reverie 15374907); its host-timed-turn and poller-yield
    /// capabilities remain false.
    fn golden_backend_capabilities(backend: Backend) -> serde_json::Value {
        let sabre = backend == Backend::Sabre;
        let kvm = backend == Backend::Kvm;
        let dbt = backend == Backend::Dbt;
        // LiteInst runs only through its in-guest runtime.
        let in_guest_liteinst = matches!(backend, Backend::Liteinst | Backend::InGuestTrap);
        serde_json::json!({
            "tool_shares_guest_descriptor_table": sabre || dbt,
            "rediscovers_descriptors_after_exec": sabre || dbt,
            "internal_pipe_turns_are_host_timed": sabre,
            "loopback_pollers_yield_to_peers": sabre,
            "guest_clock_reads_bypass_backend": sabre,
            "virtualizes_syscall_clobbers": sabre,
            "needs_killed_thread_rpc_cancellation": in_guest_liteinst || sabre || dbt || kvm,
            "reports_physical_process_exits": sabre,
            "process_exits_complete_asynchronously": in_guest_liteinst,
            "virtualizes_guest_sigalrm": in_guest_liteinst,
            "signal_interrupts_external_syscalls": sabre,
            "tracks_process_children": !dbt,
            "runs_exit_robust_list": backend == Backend::Ptrace,
            "requires_thread_directed_process_signals": dbt,
            "supports_parked_write_signal_interruption":
                matches!(backend, Backend::Ptrace | Backend::E9patch),
            "defers_vfork_child_registration": kvm,
            "virtualizes_capability_prctls": kvm,
            // `run_kvm` set these two after `prepare_backend_config`.
            "virtualizes_cpuid": kvm,
            "supports_madvise": !kvm,
            "supports_madv_dontneed": true,
            "reports_child_exit_publication":
                matches!(backend, Backend::Ptrace | Backend::E9patch),
            "provides_process_signal_control": kvm,
            "emulates_child_waits": kvm,
        })
    }

    #[test]
    fn backend_capabilities_match_the_golden_table() {
        for backend in [
            Backend::Ptrace,
            Backend::Kvm,
            Backend::E9patch,
            Backend::Liteinst,
            Backend::InGuestTrap,
            Backend::Sabre,
            Backend::Dbt,
        ] {
            let config = prepare_backend_config(super::DetConfig::default(), backend);
            assert_eq!(
                serde_json::to_value(config.backend).unwrap(),
                golden_backend_capabilities(backend),
                "{backend:?}"
            );
        }
    }

    #[test]
    fn backend_capabilities_are_the_ones_each_backend_reports() {
        assert_eq!(
            super::backend_capabilities(Backend::Ptrace),
            <reverie_ptrace::PtraceBackend as reverie::Backend>::capabilities()
        );
        assert_eq!(
            super::backend_capabilities(Backend::Kvm),
            reverie_kvm::KvmBackend::capabilities()
        );
        #[cfg(feature = "liteinst")]
        {
            assert_eq!(
                super::backend_capabilities(Backend::Liteinst),
                <reverie_liteinst::LiteinstBackend as reverie::Backend>::capabilities()
            );
            // in-guest-trap runs the same Reverie backend.
            assert_eq!(
                super::backend_capabilities(Backend::InGuestTrap),
                <reverie_liteinst::LiteinstBackend as reverie::Backend>::capabilities()
            );
        }
        #[cfg(feature = "dbt")]
        {
            assert_eq!(
                super::backend_capabilities(Backend::Dbt),
                reverie_dbt::DbtRunner::capabilities()
            );
            // The DBT runtime re-asserts the same value when it loads the
            // configuration this crate serializes for it.
            assert_eq!(
                super::backend_capabilities(Backend::Dbt),
                detcore_dbt::backend_capabilities()
            );
        }
    }

    /// `Config` with its three environment-backed fields (`HERMIT_EPOCH`,
    /// `HERMIT_PRNG`, `HERMIT_SCHED_SEED`) pinned, so the encoding does not
    /// depend on the test process's environment.
    fn env_free_detconfig(epoch: &str, seed: &str) -> DetConfig {
        use clap::Parser;
        let mut config = DetConfig::parse_from(["hermit", epoch, seed, "--sched-seed=0"]);
        config.sched_seed = None;
        config
    }

    /// Non-default values in fields that serialize as strings, floats,
    /// options and nested arrays. The path contains legacy key names, so an
    /// encoder that edited text would put a key inside a string value.
    fn rich_detconfig() -> DetConfig {
        let mut config = env_free_detconfig("--epoch=2000-12-31T23:59:59Z", "--seed=7");
        config.rng_seed = Some(13);
        config.sched_seed = Some(11);
        config.clock_multiplier = Some(1.5);
        config.chaos = true;
        config.record_preemptions_to = Some(std::path::PathBuf::from(
            "/tmp/a,\"detect_host_clock_futex_timeouts\":true,\"backend_tracks_process_children\":{",
        ));
        config.mountinfo_root_rewrites = vec![detcore_model::config::MountInfoRootRewrite {
            raw_mount_id: 41,
            deterministic_root: b"/".to_vec(),
            raw_root_prefix: Some(b"/tmp/x".to_vec()),
            deterministic_root_prefix: Some(b"/tmp".to_vec()),
            raw_mountpoint_prefix: None,
            deterministic_mountpoint_prefix: None,
        }];
        config.stacktrace_event = vec![(3, Some(std::path::PathBuf::from("/s")))];
        config
    }

    /// `HERMIT_DBT_DETCONFIG` is in the guest's environment, so a guest can
    /// read it with `getenv` or `/proc/self/environ`. These fixtures are the
    /// output of `serde_json::to_string` on the same configs, captured from a
    /// build of the commit before Detcore read [`reverie::BackendCapabilities`],
    /// when `Config` still had one boolean field per backend switch. They are
    /// the variable's bytes before `dbt_detconfig_json` re-inserts the three
    /// deleted switches' keys; `dbt_detconfig_matches_806cf2fa_bytes` pins the
    /// complete variable. Every DBT run sequentializes threads, so the DBT
    /// cases do too. The fixtures carry one deliberate later change, the
    /// `fdinfo_unlisted_mount_ids` -> `mount_id_assignment_order` key rename
    /// described on `dbt_detconfig_tests::GOLDEN`. Preserve the historical DBT
    /// descriptor capabilities here as well as the historical bytes. Current
    /// DBT's independent descriptor facts require the typed envelope, whose
    /// exact round trip and all sixteen coupled-flag combinations are checked
    /// by `dbt_detconfig_tests`.
    #[test]
    fn historical_dbt_capabilities_preserve_legacy_detconfig_bytes() {
        let sequential = |mut config: DetConfig| {
            config.sequentialize_threads = true;
            config
        };
        let default_config = || env_free_detconfig("--epoch=2026-01-01T00:00:00Z", "--seed=0");
        for (name, mut config, before) in [
            (
                "default",
                default_config(),
                include_str!("../tests/fixtures/dbt/detconfig-before-capabilities-default.json"),
            ),
            (
                "dbt",
                prepare_backend_config(sequential(default_config()), Backend::Dbt),
                include_str!("../tests/fixtures/dbt/detconfig-before-capabilities-dbt.json"),
            ),
            (
                "rich_dbt",
                prepare_backend_config(sequential(rich_detconfig()), Backend::Dbt),
                include_str!("../tests/fixtures/dbt/detconfig-before-capabilities-rich_dbt.json"),
            ),
        ] {
            config.backend.tool_shares_guest_descriptor_table = false;
            config.backend.rediscovers_descriptors_after_exec = false;
            assert_eq!(
                detcore::to_legacy_backend_json(&config).unwrap(),
                before,
                "HERMIT_DBT_DETCONFIG for the {name} config changed"
            );
            // A reader of the earlier bytes recovers the capabilities.
            let decoded = detcore::from_legacy_backend_json(before).unwrap();
            assert_eq!(decoded.backend, config.backend, "{name}");
            assert_eq!(
                decoded.sequentialize_threads, config.sequentialize_threads,
                "{name}"
            );
            assert_eq!(
                detcore::to_legacy_backend_json(&decoded).unwrap(),
                before,
                "{name}"
            );
        }
    }

    /// The fifteen per-backend keys, in the order the encoding has always
    /// carried them.
    const LEGACY_BACKEND_KEYS: [&str; 15] = [
        "cpuid_virtualized_by_backend",
        "backend_supports_madvise",
        "discover_live_file_metadata",
        "detect_host_clock_futex_timeouts",
        "syscall_clobbers_virtualized_by_backend",
        "cancel_killed_thread_rpcs",
        "backend_reports_physical_process_exits",
        "backend_tracks_process_children",
        "backend_runs_exit_robust_list",
        "backend_requires_thread_directed_process_signals",
        "backend_is_kvm",
        "kvm_shared_dequeue_timers",
        "backend_supports_parked_write_signal_interruption",
        "backend_virtualizes_capability_prctls",
        "backend_defers_vfork_child_registration",
    ];

    /// Pin the current projection onto all fifteen legacy backend keys, once
    /// without and once with sequentialized threads. DBT's two descriptor
    /// facts project to `discover_live_file_metadata=true`; that legacy bit
    /// cannot retain their independence from the host-timed capabilities.
    /// Current DBT therefore uses the separately checked typed envelope, not
    /// this lossy projection. LiteInst's row is its in-guest runtime. KVM's
    /// first two digits are the capabilities `run_kvm` formerly set itself.
    #[test]
    fn every_backend_encodes_its_current_legacy_projection() {
        for (backend, unsequenced, sequential) in [
            (Backend::Ptrace, "010000011000100", "010000011000100"),
            (Backend::E9patch, "010000010000100", "010000010000100"),
            (Backend::Liteinst, "010001010000000", "010001010000000"),
            (Backend::InGuestTrap, "010001010000000", "010001010000000"),
            (Backend::Sabre, "011111110000000", "011111110000000"),
            (Backend::Dbt, "011001000100000", "011001000100000"),
            (Backend::Kvm, "100001010010011", "100001010011011"),
        ] {
            for (sequentialize_threads, expected) in [(false, unsequenced), (true, sequential)] {
                let config = prepare_backend_config(
                    DetConfig {
                        sequentialize_threads,
                        ..DetConfig::default()
                    },
                    backend,
                );
                let encoded: serde_json::Map<String, serde_json::Value> =
                    serde_json::from_str(&detcore::to_legacy_backend_json(&config).unwrap())
                        .unwrap();
                let digits: String = LEGACY_BACKEND_KEYS
                    .iter()
                    .map(|key| match encoded.get(*key) {
                        Some(serde_json::Value::Bool(true)) => '1',
                        Some(serde_json::Value::Bool(false)) => '0',
                        other => panic!("{key} encoded as {other:?}"),
                    })
                    .collect();
                assert_eq!(
                    digits, expected,
                    "{backend:?}, sequentialize_threads={sequentialize_threads}"
                );
            }
        }
    }

    #[test]
    fn dbt_backend_config_translates_process_signals_to_host_threads() {
        let config = prepare_backend_config(super::DetConfig::default(), Backend::Dbt);
        assert!(config.backend.needs_killed_thread_rpc_cancellation);
        assert!(!config.backend.tracks_process_children);
        assert!(!config.backend.runs_exit_robust_list);
        assert!(config.backend.requires_thread_directed_process_signals);
        assert!(!config.backend.defers_vfork_child_registration);
    }

    #[test]
    fn backend_write_signal_contract_is_explicit() {
        for (backend, supports_interruption) in [
            (Backend::Ptrace, true),
            (Backend::Dbt, false),
            (Backend::Kvm, false),
            (Backend::Sabre, false),
            (Backend::Liteinst, false),
            (Backend::InGuestTrap, false),
            (Backend::E9patch, true),
        ] {
            let config = prepare_backend_config(super::DetConfig::default(), backend);
            assert_eq!(
                config.backend.supports_parked_write_signal_interruption, supports_interruption,
                "unexpected parked-write signal support for {backend:?}"
            );
        }
    }

    #[test]
    fn backend_blocked_wait_signal_contract_is_explicit() {
        for (backend, supports_interruption) in [
            (Backend::Ptrace, true),
            (Backend::Dbt, false),
            (Backend::Kvm, false),
            (Backend::Sabre, false),
            (Backend::Liteinst, false),
            (Backend::InGuestTrap, false),
            (Backend::E9patch, false),
        ] {
            let config = prepare_backend_config(super::DetConfig::default(), backend);
            assert_eq!(
                config.backend_supports_blocked_wait_signal_interruption, supports_interruption,
                "unexpected blocked-wait signal support for {backend:?}"
            );
        }
    }

    /// A launch may pass the guest a terminal when a standard descriptor is
    /// one, when the launcher has a controlling terminal, or when that cannot
    /// be read; only a launch with neither is known not to. The device number
    /// is read past a command name that holds spaces and parentheses.
    #[test]
    fn a_launch_may_pass_a_terminal_unless_it_proves_it_has_none() {
        assert!(!super::may_pass_a_terminal(false, Some(0)));
        assert!(super::may_pass_a_terminal(true, Some(0)));
        assert!(super::may_pass_a_terminal(false, Some(34817)));
        assert!(super::may_pass_a_terminal(false, None));

        let stat = "4242 (a) b (c)) S 1 4242 4242 34817 4242 4194560 0 0 0 0";
        assert_eq!(super::parse_controlling_terminal_device(stat), Some(34817));
        let stat = "4242 (hermit) S 1 4242 4242 0 -1 4194560 0 0 0 0";
        assert_eq!(super::parse_controlling_terminal_device(stat), Some(0));
        assert_eq!(
            super::parse_controlling_terminal_device("4242 (hermit"),
            None
        );
        assert_eq!(
            super::parse_controlling_terminal_device("4242 (hermit) S 1"),
            None
        );
        assert!(super::controlling_terminal_device().is_some());
    }

    /// Only a backend whose blocked waits decide interruption from the kernel's
    /// signal state asks whether the launch may pass the guest a terminal.
    #[test]
    fn only_gated_backends_record_an_inherited_terminal() {
        for backend in [
            Backend::Dbt,
            Backend::Kvm,
            Backend::Sabre,
            Backend::Liteinst,
            Backend::InGuestTrap,
            Backend::E9patch,
        ] {
            let config = prepare_backend_config(super::DetConfig::default(), backend);
            assert!(
                !config.guest_may_inherit_a_terminal,
                "unexpected terminal record for {backend:?}"
            );
        }
        let config = prepare_backend_config(super::DetConfig::default(), Backend::Ptrace);
        assert_eq!(
            config.guest_may_inherit_a_terminal,
            super::launcher_may_pass_a_terminal()
        );
    }

    #[test]
    fn liteinst_public_dispatch_runs_in_guest_detcore() {
        if Backend::Liteinst.ensure_available().is_err() {
            return;
        }

        // In-guest LiteInst has no preemption timer and refuses a maximum
        // timeslice, so this run disables preemption; preemption is not what
        // this test checks.
        let config = super::DetConfig {
            max_timeslice: None,
            ..super::DetConfig::default()
        };
        let mut command = super::Command::new("/bin/echo");
        command.arg("hello");
        let output = super::run_with_output_backend(
            command,
            config.clone(),
            false,
            &None,
            Backend::Liteinst,
        )
        .expect("run /bin/echo through in-guest LiteInst");
        // Include captured output so a failed backend launch reports its diagnostics.
        assert_eq!(
            output.status,
            super::ExitStatus::Exited(0),
            "guest did not exit 0.\n--- guest stderr ---\n{}\n--- guest stdout ---\n{}",
            String::from_utf8_lossy(&output.stderr),
            String::from_utf8_lossy(&output.stdout),
        );
        assert_eq!(output.stdout, b"hello\n");

        let status = super::run_with_backend(
            super::Command::new("/bin/true"),
            config,
            false,
            &None,
            Backend::Liteinst,
        )
        .expect("run /bin/true through in-guest LiteInst");
        assert_eq!(status, super::ExitStatus::Exited(0));
    }

    #[test]
    #[cfg(feature = "dbt")]
    fn default_and_available_backends_reflect_host_probes() {
        assert_eq!(Backend::default(), Backend::Ptrace);
        let available = Backend::available().collect::<Vec<_>>();
        assert_eq!(
            available.contains(&Backend::Ptrace),
            Backend::Ptrace.is_available()
        );
        assert_eq!(
            available.contains(&Backend::Dbt),
            dynamorio_sdk_available() && dbt_runtime_unavailable_reason().is_none()
        );
        assert_eq!(
            available.contains(&Backend::Liteinst),
            liteinst_unavailable_reason().is_none()
        );
        // The same runtime, so available exactly when liteinst is.
        assert_eq!(
            available.contains(&Backend::InGuestTrap),
            available.contains(&Backend::Liteinst)
        );
        assert_eq!(
            available.contains(&Backend::Sabre),
            Backend::Sabre.is_available()
        );
        assert_eq!(
            available.contains(&Backend::Kvm),
            kvm_device_unavailable_reason(std::path::Path::new("/dev/kvm")).is_none(),
        );
        assert_eq!(
            available.contains(&Backend::E9patch),
            Backend::E9patch.is_available()
        );
    }

    #[test]
    #[cfg(feature = "dbt")]
    fn dependency_probes_require_usable_paths() {
        let temp = tempfile::tempdir().unwrap();
        assert!(!is_dynamorio_sdk(temp.path()));
        fs::create_dir(temp.path().join("include")).unwrap();
        fs::write(temp.path().join("include/dr_api.h"), b"/* marker */").unwrap();
        assert!(is_dynamorio_sdk(temp.path()));

        let reason = kvm_device_unavailable_reason(temp.path())
            .expect("a directory must not pass the read-write KVM device probe");
        assert!(reason.contains("read-write"));
    }

    #[cfg(feature = "sabre")]
    fn write_test_executable(path: &std::path::Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"test loader").unwrap();
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(path, permissions).unwrap();
    }

    #[test]
    #[cfg(feature = "sabre")]
    fn sabre_rpc_socket_uses_private_exec_environment() {
        assert!(SABRE_RPC_SOCKET_ENV.starts_with("REVERIE_SABRE_"));
    }

    #[test]
    #[cfg(feature = "sabre")]
    fn sabre_stages_program_names_that_collide_with_loader_prefix() {
        assert!(sabre_program_needs_neutral_name(
            PathBuf::from("/usr/bin/ld").as_path()
        ));
        assert!(sabre_program_needs_neutral_name(
            PathBuf::from("/usr/bin/ld.bfd").as_path()
        ));
        assert!(!sabre_program_needs_neutral_name(
            PathBuf::from("/usr/bin/gold").as_path()
        ));
    }

    #[test]
    #[cfg(feature = "sabre")]
    fn sabre_neutral_name_staging_preserves_program_bytes_and_cleans_up() {
        let temp = tempfile::tempdir().unwrap();
        let program = temp.path().join("ld.test");
        fs::write(&program, b"linker image").unwrap();

        let staged = stage_sabre_program_in(&program, temp.path())
            .unwrap()
            .expect("ld-prefixed program should be staged");
        let staged_path = staged.path.clone();
        assert_eq!(fs::read(&staged_path).unwrap(), b"linker image");
        assert!(
            staged_path
                .file_name()
                .unwrap()
                .as_encoded_bytes()
                .starts_with(b"hermit-sabre-program-")
        );

        drop(staged);
        assert!(!staged_path.exists());
    }

    #[tokio::test(start_paused = true)]
    #[cfg(feature = "sabre")]
    async fn sabre_rpc_disconnect_wait_observes_delayed_release() {
        let global = Arc::new(());
        let connection = global.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            drop(connection);
        });

        assert_eq!(
            wait_for_sabre_rpc_disconnects(&global, Duration::from_millis(50)).await,
            Ok(())
        );
    }

    #[tokio::test(start_paused = true)]
    #[cfg(feature = "sabre")]
    async fn sabre_rpc_disconnect_wait_reports_stuck_connection() {
        let global = Arc::new(());
        let _connection = global.clone();

        assert_eq!(
            wait_for_sabre_rpc_disconnects(&global, Duration::from_millis(10)).await,
            Err(1)
        );
    }

    #[tokio::test]
    #[cfg(feature = "sabre")]
    async fn sabre_rpc_server_intentional_abort_is_clean() {
        let server_task = tokio::spawn(std::future::pending::<Result<(), &'static str>>());

        assert!(stop_sabre_rpc_server(server_task).await.is_ok());
    }

    #[tokio::test]
    #[cfg(feature = "sabre")]
    async fn sabre_rpc_server_failure_is_reported() {
        let server_task = tokio::spawn(async { Err::<(), _>("accept failed") });
        while !server_task.is_finished() {
            tokio::task::yield_now().await;
        }

        let error = stop_sabre_rpc_server(server_task).await.unwrap_err();
        assert!(error.to_string().contains("accept failed"));
    }

    #[tokio::test(start_paused = true)]
    #[cfg(feature = "sabre")]
    async fn sabre_rpc_shutdown_drains_connections_after_server_failure() {
        let global = Arc::new(());
        let connection = global.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            drop(connection);
        });
        let server_task = tokio::spawn(async { Err::<(), _>("accept failed") });
        while !server_task.is_finished() {
            tokio::task::yield_now().await;
        }

        let error = shutdown_sabre_rpc(server_task, &global, Duration::from_millis(50))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("accept failed"));
        assert_eq!(Arc::strong_count(&global), 1);
    }

    #[test]
    #[cfg(feature = "sabre")]
    fn sabre_binary_resolver_finds_cargo_target_build() {
        let temp = tempfile::tempdir().unwrap();
        let executable = temp.path().join("target/release/hermit");
        let loader = temp.path().join("target/sabre/sabre");
        write_test_executable(&loader);

        assert_eq!(
            resolve_sabre_binary_from(None, None, &executable, OsStr::new("")).unwrap(),
            loader
        );
    }

    #[test]
    #[cfg(feature = "sabre")]
    fn sabre_binary_resolver_uses_packaged_loader() {
        let temp = tempfile::tempdir().unwrap();
        let executable = temp.path().join("target/release/hermit");
        let packaged = temp.path().join("install_pkg/rsrcs/sabre");
        write_test_executable(&packaged);

        assert_eq!(
            resolve_sabre_binary_from(None, Some(&packaged), &executable, OsStr::new("")).unwrap(),
            packaged
        );
    }

    #[test]
    #[cfg(feature = "sabre")]
    fn sabre_binary_resolver_prefers_and_validates_override() {
        let temp = tempfile::tempdir().unwrap();
        let executable = temp.path().join("target/release/hermit");
        let discovered = temp.path().join("target/sabre/sabre");
        let requested = temp.path().join("requested-sabre");
        write_test_executable(&discovered);
        write_test_executable(&requested);

        assert_eq!(
            resolve_sabre_binary_from(
                Some(requested.as_os_str()),
                Some(&discovered),
                &executable,
                OsStr::new(""),
            )
            .unwrap(),
            requested
        );

        let mut permissions = fs::metadata(&requested).unwrap().permissions();
        permissions.set_mode(0o600);
        fs::set_permissions(&requested, permissions).unwrap();
        let error = resolve_sabre_binary_from(
            Some(requested.as_os_str()),
            Some(&discovered),
            &executable,
            OsStr::new(""),
        )
        .unwrap_err();
        assert!(error.to_string().contains("is not an executable file"));
    }

    #[test]
    #[cfg(feature = "dbt")]
    fn optional_backends_report_accurate_availability() {
        match Backend::Dbt.ensure_available() {
            Ok(()) => assert!(
                dynamorio_sdk_available() && dbt_runtime_unavailable_reason().is_none(),
                "DBT reported available without its SDK and runtime"
            ),
            Err(dbt_error) => {
                let message = dbt_error.to_string();
                assert!(
                    message.contains("DynamoRIO runtime")
                        || message.contains("Detcore DBT runtime"),
                    "unexpected DBT availability error: {message}"
                );
            }
        }
        assert_eq!(
            Backend::Liteinst.ensure_available().is_ok(),
            liteinst_unavailable_reason().is_none()
        );
        assert_eq!(
            Backend::InGuestTrap.ensure_available().is_ok(),
            liteinst_unavailable_reason().is_none()
        );

        match Backend::Kvm.ensure_available() {
            Ok(()) => assert!(
                kvm_device_unavailable_reason(std::path::Path::new("/dev/kvm")).is_none(),
                "KVM reported available without a usable /dev/kvm",
            ),
            Err(kvm_error) => {
                let message = kvm_error.to_string();
                assert!(
                    message.contains("/dev/kvm"),
                    "unexpected KVM availability error: {message}",
                );
                assert!(!message.contains("requires root privileges"));
            }
        }
    }

    #[test]
    fn public_backend_dispatch_rejects_unprepared_e9patch() {
        let error = ensure_backend_dispatch(Backend::E9patch).unwrap_err();
        assert!(
            error.to_string().contains("requires CLI preprocessing"),
            "unexpected error: {error}"
        );
    }

    #[test]
    #[cfg(feature = "dbt")]
    fn dbt_retries_only_the_pre_guest_bootstrap_failure() {
        use std::os::unix::process::ExitStatusExt as _;

        let failure = std::process::ExitStatus::from_raw(
            reverie_dbt::CLIENT_THREAD_START_FAILURE_EXIT_CODE << 8,
        );
        assert!(super::dbt_client_thread_start_failed(&failure));
        assert!(!super::dbt_client_thread_start_failed(
            &std::process::ExitStatus::from_raw(1 << 8)
        ));
    }

    #[test]
    #[cfg(feature = "dbt")]
    fn dbt_public_dispatch_requires_sequentialized_threads() {
        let command = super::Command::new("/bin/true");
        let config = super::DetConfig {
            sequentialize_threads: false,
            ..Default::default()
        };

        let error = super::run_with_output_backend(command, config, false, &None, Backend::Dbt)
            .expect_err("DBT must reject non-sequentialized execution");
        assert!(
            error
                .to_string()
                .contains("dbt backend requires sequentialized threads"),
            "unexpected error: {error}"
        );
    }

    #[test]
    #[cfg(feature = "dbt")]
    fn dbt_public_dispatch_runs_echo_through_detcore() {
        use clap::Parser;

        if Backend::Dbt.ensure_available().is_err() {
            return;
        }

        let mut command = super::Command::new("/bin/echo");
        command.arg("hello");
        let mut config = super::DetConfig::parse_from(["hermit-dbt-test"]);
        config.sequentialize_threads = true;
        config.validate();
        let output = super::run_with_output_backend(command, config, true, &None, Backend::Dbt)
            .expect("run /bin/echo through DbtGuest<Detcore>");

        // Include captured output so a failed backend launch reports its diagnostics.
        assert_eq!(
            output.status,
            super::ExitStatus::Exited(0),
            "guest did not exit 0.\n--- guest stderr ---\n{}\n--- guest stdout ---\n{}",
            String::from_utf8_lossy(&output.stderr),
            String::from_utf8_lossy(&output.stdout),
        );
        assert_eq!(output.stdout, b"hello\n");
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .lines()
                .any(|line| line.starts_with("reverie-dbt: tool=Detcore ")),
            "DBT native summary did not prove Detcore dispatch: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    #[cfg(feature = "dbt")]
    fn dbt_public_status_dispatch_runs_true_through_detcore() {
        use clap::Parser;

        if Backend::Dbt.ensure_available().is_err() {
            return;
        }

        let command = super::Command::new("/bin/true");
        let mut config = super::DetConfig::parse_from(["hermit-dbt-test"]);
        config.sequentialize_threads = true;
        config.validate();
        let status = super::run_with_backend(command, config, true, &None, Backend::Dbt)
            .expect("run /bin/true through DbtGuest<Detcore>");

        assert_eq!(status, super::ExitStatus::Exited(0));
    }

    #[test]
    fn kvm_runs_dynamic_echo_through_detcore() {
        use clap::Parser;

        if kvm_device_unavailable_reason(std::path::Path::new("/dev/kvm")).is_some() {
            return;
        }

        let mut command = super::Command::new("/bin/echo");
        command.arg("hello");
        let mut config = super::DetConfig::parse_from(["hermit-kvm-test"]);
        config.validate();
        let output = super::run_with_output_backend(command, config, false, &None, Backend::Kvm)
            .expect("run dynamic /bin/echo through KvmGuest<Detcore>");

        // Include captured output so a failed backend launch reports its diagnostics.
        assert_eq!(
            output.status,
            super::ExitStatus::Exited(0),
            "guest did not exit 0.\n--- guest stderr ---\n{}\n--- guest stdout ---\n{}",
            String::from_utf8_lossy(&output.stderr),
            String::from_utf8_lossy(&output.stdout),
        );
        assert_eq!(output.stdout, b"hello\n");
        assert!(output.stderr.is_empty());
    }

    // Keep the low-level vmcall transport covered independently from the ELF
    // process personality. Requires /dev/kvm; skips cleanly otherwise.
    #[test]
    fn detcore_drives_kvm_guest_for_synthetic_syscall() {
        use clap::Parser;

        const MEMORY_SIZE: usize = 0x10_000;
        const ENTRY_POINT: u64 = 0x1000;
        const FRAME_ADDRESS: u64 = 0x2000;

        let mut backend = match reverie_kvm::KvmBackend::new(MEMORY_SIZE) {
            Ok(backend) => backend,
            Err(error) => {
                eprintln!("skipping KVM Detcore experiment: cannot init VM: {error}");
                return;
            }
        };

        // A guest that issues one `getpid` through the vmcall transport, then HLTs.
        backend
            .install_syscall(
                ENTRY_POINT,
                FRAME_ADDRESS,
                reverie_kvm::SyscallRequest::new(libc::SYS_getpid as u64, [0; 6]),
            )
            .expect("install synthetic getpid guest");

        // Minimal deterministic Detcore config with RCB preemption disabled.
        let mut config =
            super::DetConfig::parse_from(["hermit-kvm-test", "--max-timeslice=disabled"]);
        config.validate();

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build tokio runtime");

        let outcome = runtime.block_on(async {
            backend
                .run_with_tool::<super::Detcore, _>(
                    config,
                    // Executor: forward anything Detcore injects to the host.
                    |request: &reverie_kvm::SyscallRequest, _memory: &reverie_kvm::GuestMemory| {
                        // SAFETY: forwarding a register-only syscall (getpid) to the
                        // host; no guest pointers are dereferenced by the kernel.
                        unsafe {
                            libc::syscall(
                                request.number() as libc::c_long,
                                request.args()[0],
                                request.args()[1],
                                request.args()[2],
                                request.args()[3],
                                request.args()[4],
                                request.args()[5],
                            ) as i64
                        }
                    },
                )
                .await
        });

        // The point of the experiment is to observe whether Detcore can be driven
        // to completion over KvmGuest at all; assert it did not error.
        outcome.expect("Detcore drove the synthetic KVM guest to completion");
    }

    // Minimal fake ELF payload: the loader only needs the image to NOT start
    // with `#!`, and a real ELF magic makes the intent obvious.
    const FAKE_ELF: &[u8] = b"\x7fELF\x02\x01\x01\x00 fake elf body";

    fn shebang_tmpdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "hermit-shebang-test-{}-{}",
            std::process::id(),
            tag
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn resolve_shebang_plain_elf_is_unchanged() {
        let dir = shebang_tmpdir("plain");
        let prog = dir.join("prog");
        fs::write(&prog, FAKE_ELF).unwrap();

        let argv = vec!["prog".to_owned(), "-a".to_owned()];
        let (path, out_argv, image) = resolve_kvm_shebang(&prog, argv).unwrap();
        assert_eq!(path, prog);
        assert_eq!(out_argv, vec!["prog".to_owned(), "-a".to_owned()]);
        assert_eq!(image, FAKE_ELF);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn resolve_shebang_single_level_kernel_order() {
        let dir = shebang_tmpdir("single");
        let interp = dir.join("fakebash");
        fs::write(&interp, FAKE_ELF).unwrap();
        let script = dir.join("script");
        // Interpreter with a single optional argument.
        fs::write(&script, format!("#!{} -x\necho hi\n", interp.display())).unwrap();

        let argv = vec!["script".to_owned(), "arg1".to_owned()];
        let (path, out_argv, image) = resolve_kvm_shebang(&script, argv).unwrap();
        assert_eq!(path, interp);
        // Kernel order: [interp, optarg, script_path, original args after argv[0]].
        assert_eq!(
            out_argv,
            vec![
                interp.to_string_lossy().into_owned(),
                "-x".to_owned(),
                script.to_string_lossy().into_owned(),
                "arg1".to_owned(),
            ]
        );
        assert_eq!(image, FAKE_ELF);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn resolve_shebang_nested_accumulates_like_binfmt_script() {
        let dir = shebang_tmpdir("nested");
        let interp = dir.join("fakebash");
        fs::write(&interp, FAKE_ELF).unwrap();
        let mid = dir.join("mid"); // a #!-interpreter that is itself a script
        fs::write(&mid, format!("#!{}\n", interp.display())).unwrap();
        let script = dir.join("script");
        fs::write(&script, format!("#!{} -e\n", mid.display())).unwrap();

        let argv = vec!["script".to_owned(), "arg1".to_owned()];
        let (path, out_argv, image) = resolve_kvm_shebang(&script, argv).unwrap();
        assert_eq!(path, interp);
        // Level 1: [mid, -e, script, arg1]; level 2 prepends [interp, mid].
        assert_eq!(
            out_argv,
            vec![
                interp.to_string_lossy().into_owned(),
                mid.to_string_lossy().into_owned(),
                "-e".to_owned(),
                script.to_string_lossy().into_owned(),
                "arg1".to_owned(),
            ]
        );
        assert_eq!(image, FAKE_ELF);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn resolve_shebang_rejects_infinite_recursion() {
        let dir = shebang_tmpdir("loop");
        let a = dir.join("a");
        let b = dir.join("b");
        fs::write(&a, format!("#!{}\n", b.display())).unwrap();
        fs::write(&b, format!("#!{}\n", a.display())).unwrap();

        let argv = vec!["a".to_owned()];
        assert!(resolve_kvm_shebang(&a, argv).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }

    /// The build scripts' pin reader, bracketed against the inputs that
    /// first-match-wins got wrong. Included rather than imported because the
    /// reader is build-script code with no library home.
    mod reverie_pin_reader {
        include!("../reverie_pin.rs");

        const A: &str = "1111111111111111111111111111111111111111";
        const B: &str = "2222222222222222222222222222222222222222";

        fn dep(rev: &str) -> String {
            format!(
                "reverie = {{ git = \"https://github.com/rrnewton/reverie.git\", rev = \"{rev}\" }}\n"
            )
        }

        #[test]
        fn a_single_revision_is_read() {
            assert_eq!(parse_reverie_pin(&dep(A)).as_deref(), Some(A));
        }

        #[test]
        fn agreeing_revisions_are_read_once() {
            let text = format!("{}{}", dep(A), dep(A));
            assert_eq!(parse_reverie_pin(&text).as_deref(), Some(A));
        }

        /// A manifest halfway through a bump names two revisions. First-match-wins
        /// silently returned the first; the canonical rule refuses to resolve it.
        #[test]
        fn disagreeing_revisions_are_refused() {
            let text = format!("{}{}", dep(A), dep(B));
            assert_eq!(parse_reverie_pin(&text), None);
        }

        /// The reported reproduction: ONE commented line above the live pin made
        /// the binary embed the comment's revision, so a correctly staged runtime
        /// would be refused by a guard naming a revision that was never the pin.
        #[test]
        fn a_commented_out_dependency_is_not_the_pin() {
            let text = format!("# {}{}", dep(B), dep(A));
            assert_eq!(
                parse_reverie_pin(&text).as_deref(),
                Some(A),
                "a commented-out revision must not win"
            );
        }

        #[test]
        fn a_manifest_naming_no_revision_yields_none() {
            assert_eq!(parse_reverie_pin("[dependencies]\nlibc = \"0.2\"\n"), None);
        }

        /// An unparseable Reverie line could be hiding a disagreement, so it makes
        /// the manifest ambiguous rather than being skipped as absent.
        #[test]
        fn an_unparseable_revision_is_ambiguous() {
            let text = format!(
                "{}reverie-x = {{ git = \"https://github.com/rrnewton/reverie.git\", rev = \"beef\" }}\n",
                dep(A)
            );
            assert_eq!(parse_reverie_pin(&text), None);
        }
    }

    /// SaBRe's guest plugin has no subscriber, so it forwards DETLOG records by
    /// the policy the CLI hands it. For each filter the CLI builds, that policy
    /// must answer every emitting module as the CLI's own subscriber does,
    /// which is what each in-process `detlog!` callsite asks under ptrace.
    /// `warn,detcore::random=info` once yielded no forwarding at all, because
    /// only the generic `detcore` target was asked, so SaBRe dropped the
    /// AT_RANDOM and early getrandom records ptrace prints.
    #[cfg(feature = "sabre")]
    #[test]
    fn sabre_detlog_forwarding_answers_each_target_as_the_cli_filter_does() {
        use detcore::detlog::ForwardPolicy;
        use detcore::detlog::info_enabled_for_target;
        use tracing::level_filters::LevelFilter;
        use tracing_subscriber::layer::SubscriberExt;

        use crate::liteinst_bootstrap::EffectiveFilter;

        let targets = [
            "",
            "detcore",
            "detcore::random",
            "detcore::randomness",
            "detcore::tool_local",
            "detcore::scheduler",
            "detcore::scheduler::runqueue",
            "detcore::syscalls::files",
            "hermit",
            "warn",
        ];
        let policy_under = |raw: &str, level: LevelFilter| {
            let subscriber = tracing_subscriber::registry()
                .with(EffectiveFilter::from_directives_lossy(raw, level).into_filter());
            tracing::subscriber::with_default(subscriber, || {
                let policy = ForwardPolicy::from_current_subscriber(raw);
                let subscriber_says: Vec<bool> = targets
                    .iter()
                    .map(|target| info_enabled_for_target(target))
                    .collect();
                (policy, subscriber_says)
            })
        };
        for (raw, level) in [
            ("warn,detcore::random=info", LevelFilter::WARN),
            ("detcore::random=info", LevelFilter::WARN),
            ("detcore::random=warn", LevelFilter::INFO),
            (
                "warn,detcore=info,detcore::scheduler=off",
                LevelFilter::WARN,
            ),
            (
                "warn,detcore[work]=info,detcore::random=info",
                LevelFilter::WARN,
            ),
            ("", LevelFilter::INFO),
            ("", LevelFilter::WARN),
        ] {
            let (policy, subscriber_says) = policy_under(raw, level);
            for (target, enabled) in targets.iter().zip(subscriber_says) {
                assert_eq!(
                    policy.forwards(target),
                    enabled,
                    "RUST_LOG={raw:?} --log={level}: policy {policy:?} disagrees on {target:?}"
                );
            }
        }

        let (scoped, subscriber_says) =
            policy_under("warn,detcore::random=info", LevelFilter::WARN);
        assert!(!subscriber_says[1], "the generic detcore target is off");
        assert!(scoped.forwards_any());
        assert!(scoped.forwards("detcore::random"));
        assert!(!scoped.forwards("detcore::tool_local"));
        assert_eq!(policy_under("", LevelFilter::INFO).0, ForwardPolicy::all());
        assert!(!policy_under("", LevelFilter::WARN).0.forwards_any());
    }

    /// In-guest LiteInst gets the per-target policy it is given next to the
    /// forwarding descriptor, and a policy that forwards nothing requests no
    /// forwarding and clears both variables.
    #[cfg(feature = "liteinst")]
    #[test]
    fn liteinst_detlog_forwarding_hands_the_guest_its_per_target_policy() {
        use std::os::fd::AsRawFd;

        use detcore::detlog::ForwardPolicy;
        use reverie::process::Command;

        use super::LITEINST_DETLOG_FORWARD_ENV;
        use super::LITEINST_DETLOG_FORWARD_POLICY_ENV;
        use super::forward_in_guest_detlogs_to;
        use super::request_liteinst_detlog_forwarding;

        let (sink, _peer) = std::os::unix::net::UnixDatagram::pair().unwrap();
        forward_in_guest_detlogs_to(sink.into());
        let env = |command: &Command, key: &str| {
            command
                .get_envs()
                .find(|(name, _)| *name == std::ffi::OsStr::new(key))
                .map(|(_, value)| value.map(|value| value.to_str().unwrap().to_owned()))
        };

        let scoped = ForwardPolicy::decode("0,detcore::random=1").unwrap();
        let mut command = Command::new("/bin/true");
        let duplicate = request_liteinst_detlog_forwarding(&mut command, &scoped)
            .unwrap()
            .expect("a policy that forwards a target requests forwarding");
        let passed = env(&command, LITEINST_DETLOG_FORWARD_ENV)
            .flatten()
            .expect("the descriptor is passed");
        assert_eq!(
            detcore::detlog::tool_output_env_target(&passed),
            Some(duplicate.as_raw_fd()),
            "{passed}"
        );
        let sent = env(&command, LITEINST_DETLOG_FORWARD_POLICY_ENV)
            .flatten()
            .expect("the policy goes with the descriptor");
        let sent = ForwardPolicy::decode(&sent).unwrap();
        assert_eq!(sent, scoped);
        assert!(sent.forwards("detcore::random"));
        assert!(!sent.forwards("detcore::tool_local"));

        let nothing = ForwardPolicy::decode("0").unwrap();
        let mut command = Command::new("/bin/true");
        command
            .env(LITEINST_DETLOG_FORWARD_ENV, "9")
            .env(LITEINST_DETLOG_FORWARD_POLICY_ENV, "1");
        assert!(
            request_liteinst_detlog_forwarding(&mut command, &nothing)
                .unwrap()
                .is_none()
        );
        assert_eq!(env(&command, LITEINST_DETLOG_FORWARD_ENV), Some(None));
        assert_eq!(
            env(&command, LITEINST_DETLOG_FORWARD_POLICY_ENV),
            Some(None)
        );
    }

    /// The forwarding policy answers as a real in-process `detlog!` record
    /// behaves under the CLI's filter: a record is logged only when both its
    /// `tracing::enabled!` hint and its event pass, and a directive with a
    /// field filter applies to the hint whatever its fields but to the event
    /// only when the event has that field. `info,<module>[{unused}]=warn`
    /// suppresses the record; the policy once forwarded it, because it probed
    /// only the event.
    #[test]
    fn detlog_forward_policy_matches_real_detlog_emission_under_field_filters() {
        use std::sync::Arc;
        use std::sync::atomic::AtomicUsize;
        use std::sync::atomic::Ordering;

        use detcore::detlog::ForwardPolicy;
        use tracing::level_filters::LevelFilter;
        use tracing_subscriber::layer::SubscriberExt;

        use crate::liteinst_bootstrap::EffectiveFilter;

        struct Count(Arc<AtomicUsize>);
        impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Count {
            fn on_event(
                &self,
                _event: &tracing::Event<'_>,
                _context: tracing_subscriber::layer::Context<'_, S>,
            ) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }

        let module = module_path!();
        for (raw, level) in [
            (format!("info,{module}[{{unused}}]=warn"), LevelFilter::INFO),
            (
                format!("warn,{module}[{{unused}}]=trace"),
                LevelFilter::WARN,
            ),
            (
                format!("info,{module}[{{message}}]=warn"),
                LevelFilter::INFO,
            ),
            (format!("info,{module}=warn"), LevelFilter::INFO),
            (format!("warn,{module}=info"), LevelFilter::WARN),
            ("info".to_owned(), LevelFilter::INFO),
            ("warn".to_owned(), LevelFilter::WARN),
        ] {
            let emitted = Arc::new(AtomicUsize::new(0));
            let subscriber = tracing_subscriber::registry()
                .with(EffectiveFilter::from_directives_lossy(&raw, level).into_filter())
                .with(Count(emitted.clone()));
            let forwards = tracing::subscriber::with_default(subscriber, || {
                detcore::detlog!("probe record");
                ForwardPolicy::from_current_subscriber(&raw).forwards(module)
            });
            assert_eq!(
                forwards,
                emitted.load(Ordering::Relaxed) == 1,
                "RUST_LOG={raw:?} --log={level}: the policy and a real detlog! disagree"
            );
        }
    }
}
