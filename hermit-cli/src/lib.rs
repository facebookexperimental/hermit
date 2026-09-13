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

mod backend_stats;
pub mod build_info;
pub mod canonical_verdict;
mod chroot;
mod consts;
mod desync;
// TODO-HUMAN-REVIEW(PR-594): Review the public e9patch preprocessing API.
pub mod e9patch;
mod error;
mod event;
mod event_stream;
mod fd;
pub mod happens_before;
mod id;
pub mod instruction_map;
mod interp;
mod metadata;
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
//   123        DO NOT USE. dev-hermit's `bin/safehermit` LOG BYTE CAP kill.
//              It moved here FROM 125 so it would stop colliding with the line
//              below; taking 123 back would undo that.
//   124        DO NOT USE. GNU `timeout`'s deadline, and dev-hermit's
//              `bin/safehermit` WALL DEADLINE kill. `tests/cli.rs` asserts
//              `assert_ne!(code, Some(124))` on the awk-mincore probe.
//   125        HERMIT_INTERNAL_FAILURE_EXIT -- hermit itself failed, no guest
//              ran. Sole meaning as of 2026-08-25: `bin/safehermit` previously
//              also emitted 125 for its byte-cap kill, so a run through that
//              wrapper produced one number for two faults with opposite
//              remedies. SAFEHERMIT MOVED, NOT HERMIT, because that wrapper is
//              the only layer that knows which happened -- it relays hermit's
//              status otherwise -- and because 125/126/127 is one GNU scheme
//              that cannot be broken up. Tracked as `hermit_s_125_collides`.
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
    HERMIT_VERIFICATION_DIVERGENCE_EXIT != 0
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
mod sabre_ptrace;
mod script;

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicPtr;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
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
use nix::sys::signal::SaFlags;
use nix::sys::signal::SigAction;
use nix::sys::signal::SigHandler;
use nix::sys::signal::SigSet;
use nix::sys::signal::Signal;
use nix::sys::signal::sigaction;
use record::Record;
use replay::Replay;
pub use reverie::ExitStatus;
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
                    eprintln!(
                        "hermit: --verify is buffering stdin from a non-seekable stream so both \
                         runs receive identical input. If this appears to hang, stdin has not \
                         reached end-of-file; pass `< /dev/null` when the guest needs no input."
                    );
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
/// Refuse a staged LiteInst runtime that was not built from the pin this binary
/// was built from.
///
/// ⚠️ THE STAGED RUNTIME CAN BE ARBITRARILY STALE AND NOTHING REPORTED IT. Two
/// independent causes, both silent: `hermit-install/build.rs` did not list the
/// pin-carrying manifests among its rerun triggers, so a pin bump left the
/// script "fresh"; and staging only runs under `PROFILE == release`, while the
/// e2e harness runs `target/debug/hermit`, so the ordinary loop never restaged.
/// The first is fixed at the trigger list. THIS is the guard for the second and
/// for any third cause nobody has found: it does not care WHY the artifact is
/// stale, only that it is.
///
/// A verdict produced against a stale runtime is a measurement of the old binary
/// published as a statement about the new pin. That is worse than a failure,
/// because it is a green.
fn liteinst_runtime_pin_matches(path: &Path) -> io::Result<()> {
    let expected = env!("HERMIT_REVERIE_PIN");
    if expected == "unknown" {
        // The build could not read the pin. Say nothing rather than assert a
        // match we cannot establish.
        return Ok(());
    }
    // Append rather than `with_extension("so.revision")`: the latter REPLACES the
    // final extension, so a caller-supplied `HERMIT_LITEINST_RUNTIME` pointing at
    // a versioned soname like `libreverie_liteinst.so.1` would look for
    // `libreverie_liteinst.so.so.revision` and refuse a correctly staged runtime.
    let marker = PathBuf::from(format!("{}.revision", path.display()));
    let staged = match fs::read_to_string(&marker) {
        Ok(text) => text.trim().to_owned(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "the staged LiteInst runtime {} records no Reverie revision, so it cannot be \
                     shown to match the pin this binary was built from ({expected}). It was staged \
                     by a build that predates revision recording, which is exactly the case where a \
                     stale runtime went unnoticed. Restage it with `cargo build --release -p \
                     hermit-install` -- staging is release-only.",
                    path.display()
                ),
            ));
        }
        Err(error) => return Err(error),
    };
    if staged != expected {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "the staged LiteInst runtime {} was built from Reverie {staged}, but this binary \
                 was built from {expected}. Running it would measure the OLD runtime and report a \
                 verdict about the NEW pin. Restage with `cargo build --release -p hermit-install`.",
                path.display()
            ),
        ));
    }
    Ok(())
}

fn validate_liteinst_runtime_library(path: &Path) -> io::Result<PathBuf> {
    liteinst_runtime_pin_matches(path)?;
    let bytes = fs::read(path).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "failed to read LiteInst runtime {}: {error}",
                path.display()
            ),
        )
    })?;
    let elf = Elf::parse(&bytes).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "LiteInst runtime {} is not an ELF DSO: {error}",
                path.display()
            ),
        )
    })?;
    if elf.header.e_type != header::ET_DYN || elf.header.e_machine != header::EM_X86_64 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "LiteInst runtime {} is not an x86-64 shared object",
                path.display()
            ),
        ));
    }

    let required = [
        "reverie_liteinst_initialize",
        "reverie_liteinst_site_trap_count",
        "reverie_liteinst_site_hook_count",
    ];
    for name in required {
        if !elf
            .dynsyms
            .iter()
            .any(|symbol| elf.dynstrtab.get_at(symbol.st_name) == Some(name))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "LiteInst runtime {} is missing required export {name}",
                    path.display()
                ),
            ));
        }
    }
    let (initializer_index, initializer) = elf
        .dynsyms
        .iter()
        .enumerate()
        .find(|(_, symbol)| {
            elf.dynstrtab.get_at(symbol.st_name) == Some("reverie_liteinst_initialize")
        })
        .ok_or_else(|| io::Error::other("checked LiteInst initializer disappeared"))?;
    let init_array = elf
        .section_headers
        .iter()
        .find(|section| section.sh_type == section_header::SHT_INIT_ARRAY)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "LiteInst runtime {} has no constructor array",
                    path.display()
                ),
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
                "LiteInst runtime {} does not register reverie_liteinst_initialize as a preload constructor",
                path.display()
            ),
        ));
    }
    path.canonicalize()
}

/// Returns the LiteInst preload cdylib produced beside the Hermit binary.
#[doc(hidden)]
pub fn liteinst_runtime_library_path() -> io::Result<PathBuf> {
    if let Some(path) = std::env::var_os("HERMIT_LITEINST_RUNTIME") {
        let path = PathBuf::from(path);
        if !path.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "HERMIT_LITEINST_RUNTIME does not name a regular file",
            ));
        }
        return validate_liteinst_runtime_library(&path).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("HERMIT_LITEINST_RUNTIME is invalid: {error}"),
            )
        });
    }

    let executable = std::env::current_exe()?;
    let directory = executable.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "Hermit executable has no parent directory",
        )
    })?;
    if let Some(path) = [
        directory.join("libreverie_liteinst.so"),
        directory.join("deps/libreverie_liteinst.so"),
    ]
    .into_iter()
    .find(|path| path.is_file())
    {
        return validate_liteinst_runtime_library(&path);
    }
    if let Some(path) = hermit_resources::resource("libreverie_liteinst.so")?
        && path.is_file()
    {
        return validate_liteinst_runtime_library(&path);
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!(
            "libreverie_liteinst.so was not built beside {} or staged as an installed resource",
            executable.display()
        ),
    ))
}

fn liteinst_runtime_unavailable_reason() -> Option<String> {
    liteinst_runtime_library_path().err().map(|error| {
        format!(
            "the LiteInst preload runtime is unavailable: {error}; build the locked liteinst-runtime-build manifest and stage its constructor-enabled DSO beside hermit"
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
    /// Use the ptrace-hosted LiteInst hybrid with one Detcore Tool.
    Liteinst,
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
    const ALL: [Self; 6] = [
        Self::Ptrace,
        Self::Dbt,
        Self::Liteinst,
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
            Self::Sabre => "sabre",
            Self::Kvm => "kvm",
            Self::E9patch => "e9patch",
        }
    }

    fn uses_ptrace_pmu_timers(self) -> bool {
        matches!(self, Self::Ptrace | Self::Liteinst | Self::E9patch)
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
            Self::Liteinst => liteinst_runtime_unavailable_reason(),
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

// SaBRe and e9patch add no third-party Rust dependencies to `hermit-cli` (SaBRe
// shells out to an external loader plus `libdetcore_sabre.so`, and e9patch shells
// out to `e9tool`/`e9patch`). They are still gated behind the `sabre` and
// `e9patch` cargo features so the default `hermit` binary reports them as absent
// and only the `third-party-backends` build offers them. The reverie-sabre Rust
// dependency lives in the `detcore-sabre` crate, which is excluded from the
// workspace's `default-members`.
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
    Some("the `dbt` feature is not enabled in this build; rebuild with `--features dbt` (or `--features third-party-backends`). This says nothing about whether DynamoRIO works on this machine -- it has not been checked".to_owned())
}

const SABRE_BINARY_ENV: &str = "HERMIT_SABRE_BINARY";

fn is_executable_file(path: &Path) -> bool {
    fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

// TODO-HUMAN-REVIEW(PR-739): Review SaBRe loader discovery and executable validation.
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

const SABRE_RPC_SOCKET_ENV: &str = "REVERIE_SABRE_HERMIT_RPC_SOCKET";
const SABRE_DETLOG_FORWARD_ENV: &str = "REVERIE_SABRE_HERMIT_FORWARD_DETLOG";
const SABRE_PATH_EVIDENCE_ENV: &str = "HERMIT_SABRE_PATH_EVIDENCE";
const SABRE_STAGING_DIRECTORY: &str = "/dev/shm";

struct StagedSabreProgram {
    path: PathBuf,
}

impl Drop for StagedSabreProgram {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn sabre_program_needs_neutral_name(program: &Path) -> bool {
    program
        .file_name()
        .is_some_and(|name| name.as_bytes().starts_with(b"ld"))
}

// TODO-HUMAN-REVIEW(PR-845): Review the neutral-name workaround for SaBRe's
// dynamic-loader prefix collision.
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
const SABRE_RPC_DISCONNECT_TIMEOUT: Duration = Duration::from_secs(1);

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
async fn run_sabre(
    mut command: Command,
    config: DetConfig,
    print_summary: bool,
    print_summary_to_json_file: &Option<PathBuf>,
    capture_output: bool,
) -> Result<Output, Error> {
    let path_evidence_file = std::env::var_os(SABRE_PATH_EVIDENCE_ENV).map(PathBuf::from);
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
    // Publish the shape of the Config this coordinator will send. The plugin is a
    // separate artifact in the same target directory, so it can be stale without
    // looking it; comparing here turns an opaque decode failure at connect into a
    // message that names the mismatch. See detcore_model::config_wire_fingerprint.
    command.env(
        detcore::CONFIG_FINGERPRINT_ENV,
        detcore::config_wire_fingerprint(),
    );
    command.env_remove(SABRE_PATH_EVIDENCE_ENV);
    command.env_remove(SABRE_DETLOG_FORWARD_ENV);
    if tracing::enabled!(target: "detcore", tracing::Level::INFO) {
        command.env(SABRE_DETLOG_FORWARD_ENV, "1");
    }
    command.env_remove("SABRE_BINARY");
    command.env_remove("SABRE_PLUGIN");

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
    global
        .clean_up(print_summary, print_summary_to_json_file)
        .await;
    tracing::info!(
        target: "hermit::sabre::fallback",
        ptrace_fallback_sites = supervised.path_evidence.ptrace_fallback_sites,
        trusted_shared_object_sites = supervised.path_evidence.trusted_shared_object_sites,
        guest_rpc_observed = supervised.path_evidence.guest_rpc_observed,
        "SaBRe ptrace fallback completed",
    );
    if detcore_never_engaged {
        return Err(anyhow!(
            "{}",
            sabre_uninstrumented_guest_message(&output.status)
        ));
    }
    Ok(output)
}

/// Classify the existing SaBRe reach evidence without collapsing "never
/// engaged" into the same zero-valued state as a fully exercised run.
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

/// Dispatch a command onto the real reverie-kvm Tool runtime.
async fn run_kvm(
    command: &Command,
    mut config: DetConfig,
    print_summary: bool,
    print_summary_to_json_file: &Option<PathBuf>,
    capture_output: bool,
) -> Result<Output, Error> {
    let dispatch_started = Instant::now();
    // KVM does not expose the outer container's procfs. Its executor serves a
    // synthetic `/proc/self/mountinfo` containing one deterministic rootfs row
    // (`reverie-kvm/src/executor.rs::synthetic_proc_content`). Consequently
    // none of the outer namespace's Hermit-owned bind mounts, or their raw
    // mount IDs, can appear in the bytes Detcore receives. The exact provenance
    // set for KVM's guest-visible mount table is therefore empty. Keeping outer
    // IDs here is not conservative: it makes the shared sanitizer reject a
    // valid synthetic row because those IDs name a different namespace.
    config.mountinfo_root_rewrites.clear();
    // reverie-kvm's synthetic mountinfo row spells the root filesystem device
    // as 0:1, while pathname metadata for `/` comes from the host root.  Record
    // that backend-proven equivalence explicitly so mountinfo and stat/statx
    // enter Detcore's shared DevicePool under the same raw identity.  This is
    // not a DevicePool ordinal: the guest-visible number remains allocated by
    // the same first-observation policy as every other filesystem device.
    let root_device = fs::metadata("/")
        .context("failed to inspect the KVM guest root filesystem device")?
        .dev();
    config.mountinfo_device_rewrites.clear();
    config
        .mountinfo_device_rewrites
        .push((libc::makedev(0, 1), root_device));
    // reverie-kvm supplies its own synthetic raw mount IDs. The shared
    // sanitizer derives their canonical order from that synthetic snapshot;
    // outer-container IDs describe a different namespace and must not leak in.
    // The current executor denies `/proc/self/fdinfo/*`, so KVM provides no
    // fdinfo/mountinfo cross-file assurance; tests pin mountinfo output/status
    // only and must not promote that result to L2 parity.
    config.mountinfo_mount_ids.clear();
    config.mountinfo_mount_ids_captured = false;
    config.fdinfo_unlisted_mount_ids.clear();
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
    config.cpuid_virtualized_by_backend = true;
    config.backend_supports_madvise = false;
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
    backend
        .install_static_elf_with_context(&image, &argv, &envp, &cwd)
        .map_err(|error| anyhow!("failed to load KVM guest executable {program:?}: {error}"))?;
    backend
        .set_random_seed(random_seed)
        .map_err(|error| anyhow!("failed to configure KVM guest random seed: {error}"))?;
    // The KVM backend now defaults to Tool-owned guest threads: CLONE_THREAD
    // workers are driven through the Detcore tool loop and their futex/CLEARTID
    // synchronization routes to Detcore, matching the golden ptrace model
    // ("follow children"). Detcore's own clone logic treats a CLONE_THREAD as
    // backend-uninstrumented iff `backend_dispatches_thread_tools` is false (see
    // detcore/src/syscalls/threads.rs), so in exactly that case opt the backend
    // out to host-owned threads to keep worker execution and futex ownership in
    // one synchronization domain (mixing them deadlocks pthread_join). In the
    // default (true) case the backend already follows children, so no call is
    // needed.
    if !config.backend_dispatches_thread_tools {
        backend.unmonitored_threads();
    }

    let execution_started = Instant::now();
    let (global_state, code, stdout, stderr) = backend
        .run_static_elf_with_tool::<Detcore>(config, capture_output)
        .await
        .map_err(|error| anyhow!("KVM guest execution failed: {error}"))?;
    let cleanup_started = Instant::now();
    global_state
        .clean_up(print_summary, print_summary_to_json_file)
        .await;
    let teardown_started = Instant::now();
    // Drop explicitly so the host's KVM VM teardown cost remains observable.
    drop(backend);
    let teardown_finished = Instant::now();

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

// TODO-HUMAN-REVIEW(PR-743): Review bounded relaunch before DBT guest execution.
#[cfg(feature = "dbt")]
fn dbt_client_thread_start_failed(status: &std::process::ExitStatus) -> bool {
    status.code() == Some(reverie_dbt::CLIENT_THREAD_START_FAILURE_EXIT_CODE)
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

    let config_json = serde_json::to_string(&config)
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

    let (status, stdout, stderr, global) = if capture_output {
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
            global.clean_up(false, &None).await;
            (status, global) = launch().await.map_err(|error| {
                anyhow!("failed to launch drrun ({}): {error}", drrun.display())
            })?;
        }
        (status, Vec::new(), Vec::new(), global)
    };

    if !status.success() {
        global.force_shutdown_with_error();
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
/// `timeout: None` is byte-for-byte the old behaviour: no alarm is armed and no
/// timer is created, so an unbounded run is not paying for a feature it did not
/// ask for. See `with_run_deadline` in this module for what firing actually
/// does; it is private, so this deliberately names it rather than linking to
/// it — a public doc link to a private item is refused by
/// `-D rustdoc::private-intra-doc-links`.
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
    config.discover_live_file_metadata = backend == Backend::Sabre;
    // Guest-visible wall and monotonic clocks must stay in the same global
    // virtual-time domain as timers, sleeps, and timeout deadlines. SaBRe's
    // per-thread execution clock does not advance when the scheduler skips
    // global time while a thread is blocked.
    config.use_thread_local_clock_reads = false;
    config.detect_host_clock_futex_timeouts = backend == Backend::Sabre;
    config.syscall_clobbers_virtualized_by_backend = backend == Backend::Sabre;
    config.cancel_killed_thread_rpcs = matches!(backend, Backend::Sabre | Backend::Dbt);
    config.backend_reports_physical_process_exits = backend == Backend::Sabre;
    // TODO-HUMAN-REVIEW(PR-1122): Review concurrent KVM process-child scheduling.
    config.backend_serializes_fork_children = false;
    config.backend_dispatches_thread_tools = true;
    config.backend_tracks_process_children = backend != Backend::Dbt;
    // Ptrace reports the task exit after Linux has atomically updated every
    // robust owner word. DBT and SaBRe report tool exit before executing the
    // native exit syscall, while KVM does not execute a native task exit, so
    // none of those backends can safely defer Detcore's modeled transition.
    config.backend_runs_exit_robust_list = backend == Backend::Ptrace;
    config.backend_requires_thread_directed_process_signals = backend == Backend::Dbt;
    // E9patch preprocesses the guest and then uses the ptrace builder. LiteInst,
    // DBT, KVM, and SaBRe re-invoke the Tool callback on ERESTARTSYS instead of
    // resuming through the kernel's ptrace syscall-restart frame.
    config.backend_supports_parked_write_signal_interruption =
        matches!(backend, Backend::Ptrace | Backend::E9patch);
    config.backend_virtualizes_capability_prctls = backend == Backend::Kvm;
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1152): KVM defers the vfork child spawn, so the child
    // registers its vfork barrier only after the parent posts BlockedExternalContinue. ptrace keeps
    // the parent kernel-blocked until the child registers, so this stays false there.
    config.backend_defers_vfork_child_registration = backend == Backend::Kvm;
    config
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
//   this predicate            122..=127  +  signal band  +  real Signaled
//   classify_container_result 122        +  signal band
//
// 123..=127 force a LiteInst shutdown and are classified as NEITHER a refusal
// nor a signal death -- 123 is safehermit's log cap, 124 a deadline, 125 hermit
// itself, 126/127 exec-level, and each has a different producer. Only the signal
// half is shared, and only the signal half is protected from drift. Do not
// "simplify" this by assuming the two agree; the `122..=127` bound here is still
// hard-coded and still needs a human to widen it if the reserved set grows
// again.
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
            "Guest exceeded the --timeout bound of {} seconds; hermit tore the container down",
            self.limit.as_secs()
        )
    }
}

impl std::error::Error for GuestTimedOut {}

/// How long the unwind gets before the hard fallback fires.
///
/// The fallback exists because the gentle path can itself wedge: if the guest
/// is stopped in a way that keeps a tracer task from completing, dropping the
/// future never returns and the deadline would be as inert as the mechanisms
/// this replaces. Ten seconds is the same grace the per-cell
/// `timeout --kill-after=10s` already uses, so the two tiers agree rather than
/// racing on different numbers.
const RUN_TIMEOUT_UNWIND_GRACE: Duration = Duration::from_secs(10);

/// Bound `guest` by `timeout`, preferring an unwind over a kill.
///
/// ⚠️ THE UNWIND IS THE POINT. `tokio::time::timeout` DROPS the guest future on
/// expiry, and dropping it is what runs every `Drop` in the async stack:
/// reverie detaches and reaps its tracees, detcore's global state is dropped,
/// and the error then propagates out through `with_container`, so the container
/// init returns NORMALLY instead of `_exit`ing. The mounts and the guest go away
/// because the namespace is torn down in order, not because the kernel demolished
/// it under us.
///
/// Contrast `record_start.rs`'s `recording_timeout_handler`, which this follows
/// in SHAPE but deliberately not in ACTION: it `_exit(124)`s from a signal
/// handler, skipping every destructor, because a signal handler may only call
/// async-signal-safe functions and cannot unwind. That remains the right answer
/// for the FALLBACK tier below and the wrong one for the primary path.
///
/// ⚠️ WHY A FALLBACK IS STILL REQUIRED. The primary path depends on the runtime
/// reaching the timer, and on the dropped future actually completing its own
/// teardown. Neither is guaranteed for an arbitrary wedged guest. A bound that
/// works only when the run was healthy enough not to need it is the inert
/// mechanism this exists to remove, so the alarm below is armed FIRST and is
/// disarmed by RAII only once the unwind has finished.
async fn with_run_deadline<F>(timeout: Option<Duration>, guest: F) -> Result<ExitStatus, Error>
where
    F: std::future::Future<Output = Result<ExitStatus, Error>>,
{
    let Some(limit) = timeout else {
        return guest.await;
    };

    // Armed before the guest starts and dropped after it finishes, so the
    // window it covers is exactly the window the bound applies to.
    let _fallback = RunTimeoutFallback::arm(limit + RUN_TIMEOUT_UNWIND_GRACE)?;

    match tokio::time::timeout(limit, guest).await {
        Ok(result) => result,
        // The future has already been dropped by `timeout` at this point; every
        // destructor in the guest stack has run before we get here.
        Err(_elapsed) => {
            stall_the_unwind_if_asked();
            Err(Error::new(GuestTimedOut { limit }))
        }
    }
}

/// Test-only: hold the post-expiry path open past the grace so the SIGALRM
/// fallback is the thing that ends the run.
///
/// ⚠️ THIS EXISTS BECAUSE THE FALLBACK COULD NOT BE MADE TO FIRE ANY OTHER WAY,
/// AND AN UNEXERCISED SAFETY PATH IS THE FAILURE MODE THIS PROJECT KEEPS
/// FINDING. Measured 2026-08-26 at this commit: the primary path fired at
/// exactly the bound for a userspace spinner, a guest blocked reading a pipe
/// with no writer, a guest that `SIGSTOP`s itself, an eight-thread guest
/// ignoring `SIGTERM`, and a multi-process guest ignoring `SIGTERM` -- five
/// shapes, five clean unwinds, no wedge. That is a good result for the primary
/// path and it leaves the fallback with zero executions, which is exactly the
/// mechanism-that-has-never-run shape.
///
/// ⚠️ WHAT THIS DOES AND DOES NOT REPRODUCE, stated precisely rather than
/// implied. It reproduces the CONDITION the fallback is specified against --
/// the post-expiry path not completing within `RUN_TIMEOUT_UNWIND_GRACE` -- and
/// it exercises the real alarm, the real inherited-mask handling, the real
/// handler, the real message and the real `_exit`. It does NOT reproduce any
/// particular upstream CAUSE of a slow unwind, because none is known; the delay
/// is here, after the drop, rather than inside a wedged destructor. A future
/// reader must not read a passing fallback test as evidence that some specific
/// teardown hang is handled.
///
/// Deliberately keyed off an environment variable named like the existing
/// `HERMIT_INTERNAL_LITEINST_ACTIVATION_PROBE` rather than a `cfg(test)` gate:
/// the fallback lives in the shipped binary and must be exercised there, not in
/// a differently-compiled one.
fn stall_the_unwind_if_asked() {
    const STALL_ENV: &str = "HERMIT_INTERNAL_RUN_TIMEOUT_STALL_UNWIND";
    if std::env::var_os(STALL_ENV).as_deref() != Some(std::ffi::OsStr::new("1")) {
        return;
    }
    // Comfortably past the grace, so the alarm -- not this sleep -- ends the
    // process. If the fallback is broken this returns and the caller sees an
    // ordinary timeout, which is what makes the test able to fail.
    std::thread::sleep(RUN_TIMEOUT_UNWIND_GRACE + Duration::from_secs(5));
}

static RUN_TIMEOUT_MESSAGE: AtomicPtr<u8> = AtomicPtr::new(std::ptr::null_mut());
static RUN_TIMEOUT_MESSAGE_LEN: AtomicUsize = AtomicUsize::new(0);

/// The hard fallback: fires only if the unwind above did not finish in time.
///
/// Identical in construction to `record_start.rs`'s `recording_timeout_handler`
/// -- non-blocking stderr so a full pipe cannot wedge the handler, then
/// `_exit` -- and identical in exit code, because "a deadline fired" is one
/// meaning and 124 already carries it for GNU `timeout`, for `safehermit`'s wall
/// bound, and for `hermit record`'s own deadline. Reusing it here adds no new
/// collision; inventing a fourth number for the same event would.
extern "C" fn run_timeout_fallback_handler(_signal: libc::c_int) {
    let len = RUN_TIMEOUT_MESSAGE_LEN.load(Ordering::Acquire);
    let message = RUN_TIMEOUT_MESSAGE.load(Ordering::Acquire);
    if !message.is_null() && len != 0 {
        // SAFETY: the message is leaked before the timer is armed, and
        // fcntl(2), write(2) and _exit(2) are async-signal-safe.
        unsafe {
            let flags = libc::fcntl(libc::STDERR_FILENO, libc::F_GETFL);
            if flags != -1 {
                libc::fcntl(libc::STDERR_FILENO, libc::F_SETFL, flags | libc::O_NONBLOCK);
            }
            libc::write(libc::STDERR_FILENO, message.cast(), len);
        }
    }
    // Exiting the namespace init tears down the container and its tracees.
    // SAFETY: _exit(2) is async-signal-safe and runs no Rust destructors --
    // which is precisely why this is the fallback and not the primary path.
    unsafe { libc::_exit(HERMIT_DEADLINE_EXIT) }
}

struct RunTimeoutFallback {
    previous_handler: SigAction,
    reblock_sigalrm: bool,
}

impl RunTimeoutFallback {
    fn arm(after: Duration) -> Result<Self, Error> {
        let seconds: libc::c_uint = after
            .as_secs()
            .try_into()
            .map_err(|_| Error::msg("--timeout exceeds the platform alarm limit"))?;
        let message = Box::leak(
            format!(
                "HERMIT_RUN_TIMEOUT_FALLBACK: the --timeout unwind did not complete within {} seconds; \
                 the container was terminated without a clean teardown\n",
                RUN_TIMEOUT_UNWIND_GRACE.as_secs()
            )
            .into_boxed_str(),
        );
        RUN_TIMEOUT_MESSAGE.store(message.as_mut_ptr(), Ordering::Release);
        RUN_TIMEOUT_MESSAGE_LEN.store(message.len(), Ordering::Release);

        let action = SigAction::new(
            SigHandler::Handler(run_timeout_fallback_handler),
            SaFlags::SA_RESETHAND,
            SigSet::empty(),
        );
        // SAFETY: the handler uses only async-signal-safe operations and stays
        // installed until this guard disarms it.
        let previous_handler = unsafe { sigaction(Signal::SIGALRM, &action) }?;

        // A blocked SIGALRM stays pending forever and the handler never runs,
        // silently disabling the fallback. `record_start.rs` learned this too.
        let mut alarm = SigSet::empty();
        alarm.add(Signal::SIGALRM);
        let reblock_sigalrm = SigSet::thread_get_mask()
            .map(|mask| mask.contains(Signal::SIGALRM))
            .unwrap_or(false);
        if reblock_sigalrm {
            let _ = alarm.thread_unblock();
        }

        // SAFETY: `seconds` fits c_uint.
        unsafe { libc::alarm(seconds) };
        Ok(Self {
            previous_handler,
            reblock_sigalrm,
        })
    }
}

impl Drop for RunTimeoutFallback {
    fn drop(&mut self) {
        // SAFETY: disarm the alarm before restoring the inherited handler.
        unsafe {
            libc::alarm(0);
            let _ = sigaction(Signal::SIGALRM, &self.previous_handler);
        }
        if self.reblock_sigalrm {
            let mut alarm = SigSet::empty();
            alarm.add(Signal::SIGALRM);
            let _ = alarm.thread_block();
        }
        RUN_TIMEOUT_MESSAGE_LEN.store(0, Ordering::Release);
        RUN_TIMEOUT_MESSAGE.store(std::ptr::null_mut(), Ordering::Release);
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
    with_run_deadline(timeout, async {
        dispatch_backend(
            command,
            config,
            print_summary,
            print_summary_to_json_file,
            backend,
        )
        .await
    })
    .await
}

async fn dispatch_backend(
    command: Command,
    config: DetConfig,
    print_summary: bool,
    print_summary_to_json_file: &Option<PathBuf>,
    backend: Backend,
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
    if backend == Backend::Liteinst {
        let preload = liteinst_runtime_library_path()?;
        let (exit_status, mut global_state) =
            reverie_liteinst::LiteinstBackend::run_host_with_preload::<Detcore>(
                command, config, preload,
            )
            .await?;
        if liteinst_requires_forced_shutdown(exit_status) {
            global_state.force_shutdown_with_error();
            global_state.cancel_internal_scheduler().await;
        }
        global_state
            .clean_up(print_summary, print_summary_to_json_file)
            .await;
        return Ok(exit_status);
    }
    ensure_backend_dispatch(backend)?;

    let stats_request = backend_stats::request();
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
    let (exit_status, global_state) = builder.spawn().await?.wait().await?;
    global_state
        .clean_up(print_summary, print_summary_to_json_file)
        .await; // Before it's dropped by this function.
    backend_stats::report(backend, stats_request, &backend_stats::PtraceStatsSource);
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
    let Some(limit) = timeout else {
        return dispatch_output_backend(
            command,
            config,
            print_summary,
            print_summary_to_json_file,
            backend,
        )
        .await;
    };
    let _fallback = RunTimeoutFallback::arm(limit + RUN_TIMEOUT_UNWIND_GRACE)?;
    match tokio::time::timeout(
        limit,
        dispatch_output_backend(
            command,
            config,
            print_summary,
            print_summary_to_json_file,
            backend,
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
    if backend == Backend::Liteinst {
        command.stdin(output_backend_stdin()?);
        let preload = liteinst_runtime_library_path()?;
        let (output, mut global_state) =
            reverie_liteinst::LiteinstBackend::run_host_with_output_and_preload::<Detcore>(
                command, config, preload,
            )
            .await?;
        let status = output.status;
        if liteinst_requires_forced_shutdown(status) {
            global_state.force_shutdown_with_error();
            global_state.cancel_internal_scheduler().await;
        }
        global_state
            .clean_up(print_summary, print_summary_to_json_file)
            .await;
        return Ok(Output {
            status,
            stdout: output.stdout,
            stderr: output.stderr,
        });
    }
    ensure_backend_dispatch(backend)?;

    let stats_request = backend_stats::request();
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
    let (output, global_state) = builder.spawn().await?.wait_with_output().await?;
    global_state
        .clean_up(print_summary, print_summary_to_json_file)
        .await;
    backend_stats::report(backend, stats_request, &backend_stats::PtraceStatsSource);
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
        let data = self.create_recording_dir()?;
        let exit_status = record_to(command, data.path())?;
        self.commit_recording(data, exit_status)
    }

    /// Records with mountinfo provenance captured from the active container.
    pub fn record_with_mountinfo(
        &self,
        command: Command,
        mountinfo_root_rewrites: Vec<detcore_model::config::MountInfoRootRewrite>,
    ) -> Result<Recording, Error> {
        let data = self.create_recording_dir()?;
        let exit_status = record_to_with_mountinfo(command, data.path(), mountinfo_root_rewrites)?;
        self.commit_recording(data, exit_status)
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
        let mut metadata_path = self.data_dir.join(id.to_string());
        metadata_path.push(METADATA_NAME);

        let metadata: Metadata = serde_json::from_reader(
            fs::File::open(&metadata_path)
                .with_context(|| format!("Failed to open {:?}", metadata_path))?,
        )
        .with_context(|| format!("Failed to parse {:?}", metadata_path))?;

        Ok(metadata)
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
    use std::collections::BTreeSet;

    let contents = fs::read("/proc/self/mountinfo")?;
    let rows = detcore_model::procfs::parse_mountinfo(&contents)
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

/// Records to the specified directory, which must already exist.
///
/// This low-level API supplies no pre-captured mount provenance. Detcore reads
/// the completed guest namespace after command mounts and stdio are installed.
/// Container-aware callers with producer-owned provenance must use
/// [`record_to_with_mountinfo`].
#[tokio::main(flavor = "current_thread")]
pub async fn record_to(command: Command, dir: &Path) -> Result<ExitStatus, Error> {
    record_to_async(command, dir, Vec::new(), None).await
}

/// Records to the specified directory with exact mountinfo provenance.
#[tokio::main(flavor = "current_thread")]
pub async fn record_to_with_mountinfo(
    command: Command,
    dir: &Path,
    mountinfo_root_rewrites: Vec<detcore_model::config::MountInfoRootRewrite>,
) -> Result<ExitStatus, Error> {
    let mountinfo_mount_ids = capture_mountinfo_identity_order()?;
    record_to_async(
        command,
        dir,
        mountinfo_root_rewrites,
        Some(mountinfo_mount_ids),
    )
    .await
}

async fn record_to_async(
    command: Command,
    dir: &Path,
    mountinfo_root_rewrites: Vec<detcore_model::config::MountInfoRootRewrite>,
    mountinfo_mount_ids: Option<Vec<u64>>,
) -> Result<ExitStatus, Error> {
    let skid_overshoot_report = SkidOvershootReport::begin(true);
    let result = async {
        Record::spawn_with_mountinfo(command, dir, mountinfo_root_rewrites, mountinfo_mount_ids)
            .await?
            .wait()
            .await
    }
    .await;
    skid_overshoot_report.finish(result)
}

/// Records to the specified directory, which must already exist. The
/// stderr/stdout of the recording is captured in `Output`.
///
/// This low-level API supplies no pre-captured mount provenance. Detcore reads
/// the completed guest namespace after command mounts and stdio are installed.
/// Container-aware callers with producer-owned provenance must use
/// [`record_with_output_with_mountinfo`].
#[tokio::main(flavor = "current_thread")]
pub async fn record_with_output(command: Command, dir: &Path) -> Result<Output, Error> {
    record_with_output_async(command, dir, Vec::new(), None).await
}

/// Records with captured output and exact mountinfo provenance.
#[tokio::main(flavor = "current_thread")]
pub async fn record_with_output_with_mountinfo(
    command: Command,
    dir: &Path,
    mountinfo_root_rewrites: Vec<detcore_model::config::MountInfoRootRewrite>,
) -> Result<Output, Error> {
    let mountinfo_mount_ids = capture_mountinfo_identity_order()?;
    record_with_output_async(
        command,
        dir,
        mountinfo_root_rewrites,
        Some(mountinfo_mount_ids),
    )
    .await
}

async fn record_with_output_async(
    mut command: Command,
    dir: &Path,
    mountinfo_root_rewrites: Vec<detcore_model::config::MountInfoRootRewrite>,
    mountinfo_mount_ids: Option<Vec<u64>>,
) -> Result<Output, Error> {
    let skid_overshoot_report = SkidOvershootReport::begin(true);
    command.stdin(Stdio::null());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());

    let result = async {
        Record::spawn_with_mountinfo(command, dir, mountinfo_root_rewrites, mountinfo_mount_ids)
            .await?
            .wait_with_output()
            .await
    }
    .await;
    skid_overshoot_report.finish(result)
}

/// Replays from the specified directory.
#[tokio::main(flavor = "current_thread")]
pub async fn replay_from(dir: &Path) -> Result<ExitStatus, Error> {
    let skid_overshoot_report = SkidOvershootReport::begin(true);
    let result = async { Ok(Replay::spawn(dir, false, None, &[]).await?.wait().await?) }.await;
    skid_overshoot_report.finish(result)
}

/// Replays with a gdb server.
#[tokio::main(flavor = "current_thread")]
pub async fn replay_with_gdbserver(dir: &Path, port: u16) -> Result<ExitStatus, Error> {
    let skid_overshoot_report = SkidOvershootReport::begin(true);
    let result = async {
        Ok(Replay::spawn(dir, false, Some(port), &[])
            .await?
            .wait()
            .await?)
    }
    .await;
    skid_overshoot_report.finish(result)
}

/// Replays with a gdb server and applies mounts inside the replay chroot.
#[tokio::main(flavor = "current_thread")]
pub async fn replay_with_gdbserver_and_mounts(
    dir: &Path,
    port: u16,
    mounts: &[Mount],
) -> Result<ExitStatus, Error> {
    let skid_overshoot_report = SkidOvershootReport::begin(true);
    let result = async {
        Ok(Replay::spawn(dir, false, Some(port), mounts)
            .await?
            .wait()
            .await?)
    }
    .await;
    skid_overshoot_report.finish(result)
}

/// Replays from the specified directory which must already exist. The
/// stderr/stdout of the replay is captured in `Output`.
#[tokio::main(flavor = "current_thread")]
pub async fn replay_with_output(dir: &Path) -> Result<Output, Error> {
    let skid_overshoot_report = SkidOvershootReport::begin(true);
    let result = async {
        Ok(Replay::spawn(dir, true, None, &[])
            .await?
            .wait_with_output()
            .await?)
    }
    .await;
    skid_overshoot_report.finish(result)
}

/// Replays with captured output and applies the requested mounts inside the replay chroot.
#[tokio::main(flavor = "current_thread")]
pub async fn replay_with_output_and_mounts(dir: &Path, mounts: &[Mount]) -> Result<Output, Error> {
    let skid_overshoot_report = SkidOvershootReport::begin(true);
    let result = async {
        Ok(Replay::spawn(dir, true, None, mounts)
            .await?
            .wait_with_output()
            .await?)
    }
    .await;
    skid_overshoot_report.finish(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    static SKID_OVERSHOOT_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

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
        for backend in [Backend::Ptrace, Backend::Liteinst, Backend::E9patch] {
            assert!(backend.uses_ptrace_pmu_timers(), "{backend:?}");
        }
        for backend in [Backend::Dbt, Backend::Sabre, Backend::Kvm] {
            assert!(!backend.uses_ptrace_pmu_timers(), "{backend:?}");
        }
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
    use std::ffi::OsStr;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Duration;

    use super::Backend;
    use super::ExitStatus;
    use super::HermitData;
    use super::Id;
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
    use super::liteinst_requires_forced_shutdown;
    #[cfg(feature = "dbt")]
    use super::liteinst_runtime_unavailable_reason;
    use super::output_backend_stdin_file;
    use super::prepare_backend_config;
    use super::reserve_output_stdin_snapshot;
    use super::resolve_kvm_shebang;
    use super::resolve_sabre_binary_from;
    use super::sabre_backend_evidence_line;
    use super::sabre_program_needs_neutral_name;
    use super::sabre_reach_state;
    use super::sabre_uninstrumented_guest_message;
    use super::shutdown_sabre_rpc;
    use super::stage_sabre_program_in;
    use super::stop_sabre_rpc_server;
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
    fn liteinst_host_backend_preserves_ptrace_rcb_timeslices() {
        let config = super::DetConfig::default();
        assert!(config.max_timeslice.is_some());
        assert!(
            prepare_backend_config(config.clone(), Backend::Liteinst)
                .max_timeslice
                .is_some()
        );
        assert!(
            prepare_backend_config(config, Backend::Ptrace)
                .max_timeslice
                .is_some()
        );
    }

    #[test]
    fn sabre_backend_configures_process_local_capabilities() {
        let config = super::DetConfig::default();
        let sabre = prepare_backend_config(config.clone(), Backend::Sabre);
        assert!(sabre.discover_live_file_metadata);
        assert!(!sabre.use_thread_local_clock_reads);
        assert!(sabre.detect_host_clock_futex_timeouts);
        assert!(sabre.syscall_clobbers_virtualized_by_backend);
        assert!(sabre.cancel_killed_thread_rpcs);
        assert!(sabre.backend_reports_physical_process_exits);
        assert!(!sabre.backend_serializes_fork_children);
        assert!(sabre.backend_dispatches_thread_tools);
        assert!(sabre.backend_tracks_process_children);
        assert!(!sabre.backend_runs_exit_robust_list);
        assert!(!sabre.backend_requires_thread_directed_process_signals);
        assert!(!sabre.backend_virtualizes_capability_prctls);
        assert!(!sabre.backend_defers_vfork_child_registration);
        let ptrace = prepare_backend_config(config, Backend::Ptrace);
        assert!(!ptrace.discover_live_file_metadata);
        assert!(!ptrace.use_thread_local_clock_reads);
        assert!(!ptrace.detect_host_clock_futex_timeouts);
        assert!(!ptrace.syscall_clobbers_virtualized_by_backend);
        assert!(!ptrace.cancel_killed_thread_rpcs);
        assert!(!ptrace.backend_reports_physical_process_exits);
        assert!(!ptrace.backend_serializes_fork_children);
        assert!(ptrace.backend_dispatches_thread_tools);
        assert!(ptrace.backend_tracks_process_children);
        assert!(ptrace.backend_runs_exit_robust_list);
        assert!(!ptrace.backend_requires_thread_directed_process_signals);
        assert!(!ptrace.backend_virtualizes_capability_prctls);
        assert!(!ptrace.backend_defers_vfork_child_registration);
    }

    #[test]
    fn kvm_backend_config_marks_concurrent_process_children() {
        let config = super::DetConfig::default();
        let kvm = prepare_backend_config(config, Backend::Kvm);
        assert!(!kvm.backend_serializes_fork_children);
        assert!(kvm.backend_dispatches_thread_tools);
        assert!(kvm.backend_tracks_process_children);
        assert!(!kvm.backend_runs_exit_robust_list);
        assert!(!kvm.backend_requires_thread_directed_process_signals);
        assert!(kvm.backend_virtualizes_capability_prctls);
        assert!(kvm.backend_defers_vfork_child_registration);
    }

    #[test]
    fn dbt_backend_config_translates_process_signals_to_host_threads() {
        let config = prepare_backend_config(super::DetConfig::default(), Backend::Dbt);
        assert!(config.cancel_killed_thread_rpcs);
        assert!(!config.backend_tracks_process_children);
        assert!(!config.backend_runs_exit_robust_list);
        assert!(config.backend_requires_thread_directed_process_signals);
        assert!(!config.backend_defers_vfork_child_registration);
    }

    #[test]
    fn backend_write_signal_contract_is_explicit() {
        for (backend, supports_interruption) in [
            (Backend::Ptrace, true),
            (Backend::Dbt, false),
            (Backend::Kvm, false),
            (Backend::Sabre, false),
            (Backend::Liteinst, false),
            (Backend::E9patch, true),
        ] {
            let config = prepare_backend_config(super::DetConfig::default(), backend);
            assert_eq!(
                config.backend_supports_parked_write_signal_interruption, supports_interruption,
                "unexpected parked-write signal support for {backend:?}"
            );
        }
    }

    #[test]
    fn liteinst_public_dispatch_runs_ptrace_host_hybrid() {
        if Backend::Liteinst.ensure_available().is_err() {
            return;
        }

        let mut command = super::Command::new("/bin/echo");
        command.arg("hello");
        let output = super::run_with_output_backend(
            command,
            super::DetConfig::default(),
            false,
            &None,
            Backend::Liteinst,
        )
        .expect("run /bin/echo through the ptrace-hosted LiteInst hybrid");
        assert_eq!(output.status, super::ExitStatus::Exited(0));
        assert_eq!(output.stdout, b"hello\n");

        let status = super::run_with_backend(
            super::Command::new("/bin/true"),
            super::DetConfig::default(),
            false,
            &None,
            Backend::Liteinst,
        )
        .expect("run /bin/true through the ptrace-hosted LiteInst hybrid");
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
            liteinst_runtime_unavailable_reason().is_none()
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

    fn write_test_executable(path: &std::path::Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"test loader").unwrap();
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(path, permissions).unwrap();
    }

    #[test]
    fn sabre_rpc_socket_uses_private_exec_environment() {
        assert!(SABRE_RPC_SOCKET_ENV.starts_with("REVERIE_SABRE_"));
    }

    #[test]
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
    async fn sabre_rpc_disconnect_wait_reports_stuck_connection() {
        let global = Arc::new(());
        let _connection = global.clone();

        assert_eq!(
            wait_for_sabre_rpc_disconnects(&global, Duration::from_millis(10)).await,
            Err(1)
        );
    }

    #[tokio::test]
    async fn sabre_rpc_server_intentional_abort_is_clean() {
        let server_task = tokio::spawn(std::future::pending::<Result<(), &'static str>>());

        assert!(stop_sabre_rpc_server(server_task).await.is_ok());
    }

    #[tokio::test]
    async fn sabre_rpc_server_failure_is_reported() {
        let server_task = tokio::spawn(async { Err::<(), _>("accept failed") });
        while !server_task.is_finished() {
            tokio::task::yield_now().await;
        }

        let error = stop_sabre_rpc_server(server_task).await.unwrap_err();
        assert!(error.to_string().contains("accept failed"));
    }

    #[tokio::test(start_paused = true)]
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
            liteinst_runtime_unavailable_reason().is_none()
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

        assert_eq!(output.status, super::ExitStatus::Exited(0));
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

        assert_eq!(output.status, super::ExitStatus::Exited(0));
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

    /// The pin guard shipped with zero tests, which made it deletable: replacing
    /// the call with `let _ = liteinst_runtime_pin_matches;` left the suite green
    /// and silently reverted the loader to the old "missing required export"
    /// path. That is the same shape as the defect the guard itself exists to
    /// prevent -- a mechanism nobody has watched refuse -- so it is bracketed
    /// here at the level that needs no CMake, no staged runtime and no box.
    mod liteinst_pin_guard {
        use std::fs;

        use crate::liteinst_runtime_pin_matches;

        /// The guard is inert by design when the build could not read a pin.
        /// Every case below asserts the *refusal*, so they would all pass
        /// vacuously in that configuration; skip loudly instead.
        fn pin_or_skip() -> Option<&'static str> {
            let pin = env!("HERMIT_REVERIE_PIN");
            (pin != "unknown").then_some(pin)
        }

        #[test]
        fn a_runtime_with_no_recorded_revision_is_refused() {
            let Some(_) = pin_or_skip() else { return };
            let dir = tempfile::tempdir().unwrap();
            let so = dir.path().join("libreverie_liteinst.so");
            let error = liteinst_runtime_pin_matches(&so).unwrap_err();
            let text = error.to_string();
            assert!(
                text.contains("records no Reverie revision"),
                "must say the runtime carries no revision, said: {text}"
            );
            // The message has to carry the way out, not just the complaint --
            // staging is release-only and nothing else restages.
            assert!(
                text.contains("cargo build --release -p hermit-install"),
                "must name the restage command, said: {text}"
            );
        }

        #[test]
        fn a_runtime_from_another_revision_is_refused_naming_both() {
            let Some(pin) = pin_or_skip() else { return };
            let dir = tempfile::tempdir().unwrap();
            let so = dir.path().join("libreverie_liteinst.so");
            let stale = "0".repeat(40);
            fs::write(dir.path().join("libreverie_liteinst.so.revision"), &stale).unwrap();
            let text = liteinst_runtime_pin_matches(&so).unwrap_err().to_string();
            // Naming BOTH is the point: a refusal that names only one revision
            // cannot tell the reader which side is stale.
            assert!(
                text.contains(&stale),
                "must name the staged revision: {text}"
            );
            assert!(text.contains(pin), "must name the binary's pin: {text}");
        }

        #[test]
        fn a_matching_runtime_is_accepted() {
            let Some(pin) = pin_or_skip() else { return };
            let dir = tempfile::tempdir().unwrap();
            let so = dir.path().join("libreverie_liteinst.so");
            // Trailing newline: this is exactly what the build script writes.
            fs::write(
                dir.path().join("libreverie_liteinst.so.revision"),
                format!("{pin}\n"),
            )
            .unwrap();
            assert!(
                liteinst_runtime_pin_matches(&so).is_ok(),
                "the guard must not block a correctly staged runtime"
            );
        }

        /// ⚠️ THE CALL SITE, NOT JUST THE FUNCTION. The reported defect was that
        /// replacing the call with `let _ = liteinst_runtime_pin_matches;` left the
        /// suite green -- so tests that only exercise the function directly would
        /// not have caught it. This one goes through `validate_liteinst_runtime_library`
        /// and asserts the PIN diagnosis specifically, because without the guard the
        /// same input still fails, just with the older "missing required export"
        /// message. Asserting `is_err()` here would pass either way.
        #[test]
        fn the_loader_consults_the_guard_before_anything_else() {
            let Some(pin) = pin_or_skip() else { return };
            let dir = tempfile::tempdir().unwrap();
            let so = dir.path().join("libreverie_liteinst.so");
            // A file that is not a valid DSO: if the guard is bypassed, the export
            // check rejects it for an unrelated reason and the test must notice.
            fs::write(&so, b"not an elf").unwrap();
            let stale = "0".repeat(40);
            fs::write(dir.path().join("libreverie_liteinst.so.revision"), &stale).unwrap();
            let text = crate::validate_liteinst_runtime_library(&so)
                .unwrap_err()
                .to_string();
            assert!(
                text.contains(&stale) && text.contains(pin),
                "the loader must refuse on the PIN, naming both revisions, before it \
                 reaches the export check; said: {text}"
            );
        }

        /// `HERMIT_LITEINST_RUNTIME` is caller-supplied, so the marker path has to
        /// survive a versioned soname. `with_extension("so.revision")` REPLACED the
        /// final extension and looked for `libreverie_liteinst.so.so.revision`,
        /// refusing a correctly staged runtime.
        #[test]
        fn a_versioned_soname_finds_its_marker() {
            let Some(pin) = pin_or_skip() else { return };
            let dir = tempfile::tempdir().unwrap();
            let so = dir.path().join("libreverie_liteinst.so.1");
            fs::write(
                dir.path().join("libreverie_liteinst.so.1.revision"),
                format!("{pin}\n"),
            )
            .unwrap();
            assert!(
                liteinst_runtime_pin_matches(&so).is_ok(),
                "the marker beside a versioned soname must be the one consulted"
            );
        }
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
}
