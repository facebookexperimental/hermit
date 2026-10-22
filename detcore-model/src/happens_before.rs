/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Happens-before edges: a sparse, authored partial order over dynamic events.
//!
//! Where `--replay-schedule-from` replays a *complete* total order captured from
//! a prior run, a happens-before specification pins down only the *few* events
//! that matter for a race and lets the deterministic scheduler fill in the rest.
//! An agent (or human) that already knows a target race can therefore construct
//! it deterministically instead of blind seed-search.
//!
//! # Model
//!
//! An [`Anchor`] names a precise, deterministic per-thread stop point. Following
//! the owner's refinement of RFC #1146, the *primary* addressing is a
//! [`Position`] — "after N syscalls" or "after M retired conditional branches
//! (RCBs)" on a specific thread — optionally decorated with a [`CodeLocation`]
//! (function and/or source line, resolved from debug info) for readability. The
//! RFC's richer addressing modes (Nth occurrence of a named syscall, RIP hit, or
//! function entry) remain expressible but are deliberately *not* the lead
//! addressing scheme.
//!
//! A [`HappensBeforeEdge`] states that one anchor must be observed before another
//! thread is allowed to proceed past its anchor. A [`HappensBeforeSpec`] is the
//! whole authored partial order: a table of named threads, a table of named
//! events (anchors), and the edge list connecting them.
//!
//! This module owns the *model* only: parsing (JSON and a terse DSL),
//! normalization, and static validation (name resolution, exactly-one-position,
//! and cycle detection). Resolving a [`CodeLocation`] to a concrete address via
//! debug info, and enforcing edges in the scheduler, live in higher layers.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fmt;
use std::str::FromStr;

use reverie_syscalls::Sysno;
use serde::Deserialize;
use serde::Serialize;

use crate::pid::DetTid;
use crate::schedule::SyscallPhase;

/// The newest schema version understood by this build. Version 2 adds
/// relative anchors (`from`), the `futex_op` filter and the hold budget
/// (`max_hold_ns`); version 1 files load unchanged, with their old semantics.
/// A build that understands only version 1 refuses a version-2 file instead of
/// silently dropping the fields it does not know.
pub const HAPPENS_BEFORE_VERSION: u32 = 2;

/// The original schema version, still accepted. It may not use any field that
/// version 2 added ([`HappensBeforeError::NewFieldInVersion1`]).
pub const HAPPENS_BEFORE_VERSION_1: u32 = 1;

/// The hold budget of a version-2 spec that sets no `max_hold_ns`: 10 s of
/// virtual time.
pub const DEFAULT_MAX_HOLD_NS: u64 = 10_000_000_000;

// ================================================================================
// Declarative on-disk / on-wire format (serde)
// ================================================================================

/// The declarative happens-before specification, as read from a JSON file or
/// desugared from the terse DSL. This mirrors the reviewed RFC #1146 file format
/// verbatim so an authored file round-trips.
#[derive(PartialEq, Eq, Debug, Clone, Serialize, Deserialize)]
pub struct HappensBeforeSpec {
    /// Schema version: [`HAPPENS_BEFORE_VERSION_1`] or
    /// [`HAPPENS_BEFORE_VERSION`].
    pub version: u32,

    /// Version 2 only: how long a thread may stay held at a gate, in committed
    /// virtual nanoseconds, before the run is refused
    /// (`HERMIT_HB_HOLD_BUDGET_EXCEEDED`). Defaults to [`DEFAULT_MAX_HOLD_NS`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_hold_ns: Option<u64>,

    /// Symbolic thread labels mapped to a resolution rule, so authors need not
    /// hard-code raw `DetTid`s.
    #[serde(default)]
    pub threads: BTreeMap<String, ThreadSpec>,

    /// Named events (anchors). Naming events separately from edges lets one event
    /// participate in several edges and keeps the edge list readable.
    #[serde(default)]
    pub events: BTreeMap<String, EventSpec>,

    /// The partial order itself.
    #[serde(default)]
    pub edges: Vec<EdgeSpec>,
}

/// How a symbolic thread label resolves to a concrete `DetTid`.
///
/// Exactly one resolution rule should be provided; if `dettid` is present it wins.
#[derive(PartialEq, Eq, Debug, Clone, Default, Serialize, Deserialize)]
pub struct ThreadSpec {
    /// A human-facing label (defaults to the map key).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,

    /// An explicit deterministic thread id, when the author knows it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dettid: Option<i32>,

    /// Resolve to the thread created by the Nth `clone`/`fork`, 1-based. Thread
    /// creation is deterministic under sequentialization, so this is stable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spawn_ordinal: Option<u32>,
}

/// A single named event (anchor) in the declarative format.
///
/// The addressing fields are flat and optional to match the RFC JSON; exactly one
/// *position* selector must be set (see [`HappensBeforeSpec::normalize`]). The
/// two owner-preferred primaries are [`syscalls`](Self::syscalls) ("after N
/// syscalls") and [`rcbs`](Self::rcbs) ("after M RBCs").
#[derive(PartialEq, Eq, Debug, Clone, Default, Serialize, Deserialize)]
pub struct EventSpec {
    /// The thread this event is on: a key into [`HappensBeforeSpec::threads`], or
    /// a raw integer `DetTid`.
    pub thread: String,

    // ---- primary positions (owner's refinement) ----
    /// After the thread has executed this many syscalls in total (any syscall).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub syscalls: Option<u64>,

    /// After the thread has retired this many conditional branches (its RCB
    /// clock reaches this absolute value). Accepted as `rcbs` or `rcb`.
    #[serde(default, alias = "rcb", skip_serializing_if = "Option::is_none")]
    pub rcbs: Option<u64>,

    // ---- code location (readability; resolves to a RIP via debug info) ----
    /// A function name; with `line` this is "function+line", the preferred
    /// human-legible location.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub func: Option<String>,

    /// A source file name (optional companion to `line`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,

    /// A source line number.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,

    // ---- RFC richer addressing (expressible, not led with) ----
    /// A specific syscall by name (e.g. `"futex"`), the Nth occurrence of which
    /// is the anchor. Combine with `phase` and `nth`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub syscall: Option<String>,

    /// Which phase of the named `syscall` to anchor on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<PhaseSpec>,

    /// Restrict the named `syscall` to calls whose first argument is this file
    /// descriptor, for example `{"syscall": "writev", "fd": 9, "nth": 3}`: the
    /// thread's third `writev` to fd 9. The descriptor is matched by number at
    /// the time of the call: a write to a `dup` of fd 9 under another number
    /// does not count, and after fd 9 is closed and the number reused, calls
    /// on the new fd 9 do. Only valid with `syscall`, and only for
    /// a syscall whose first argument is a file descriptor
    /// ([`syscall_takes_fd_first`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fd: Option<i32>,

    /// Version 2 only: restrict `"syscall": "futex"` to calls whose operation
    /// is this value. The kernel takes `int futex_op`, so the low 32 bits of
    /// the second argument are compared, read as a signed `int`; the upper bits
    /// of the register are ignored. The value must fit an `i32`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub futex_op: Option<i64>,

    /// Version 2 only: count this event's syscall occurrences strictly after
    /// the entry at which the named event is reached on the same thread, so
    /// `nth` is 1-based from there. The base entry itself never counts, even
    /// when it matches. The base must be a count or syscall-occurrence anchor
    /// on the same thread and may not itself use `from`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,

    /// A raw instruction pointer, as a hex string like `"0x401f3c"` or decimal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rip: Option<String>,

    /// A cooperative marker name (guest-emitted). Reserved for a future backend.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mark: Option<String>,

    /// Which occurrence of the addressed point (1-based). Defaults to 1. Only
    /// meaningful for the occurrence-counted modes (`syscall`, `func`, `rip`,
    /// `mark`); the absolute-count primaries (`syscalls`, `rcbs`) ignore it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nth: Option<u64>,
}

/// Serializable mirror of [`SyscallPhase`] using lowercase author-friendly names.
#[derive(PartialEq, Eq, Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PhaseSpec {
    /// Before the syscall instruction executes.
    #[serde(alias = "pre")]
    Prehook,
    /// A nonblocking poll retry.
    Polling,
    /// After the syscall logically completes.
    #[serde(alias = "post")]
    Posthook,
}

impl From<PhaseSpec> for SyscallPhase {
    fn from(p: PhaseSpec) -> Self {
        match p {
            PhaseSpec::Prehook => SyscallPhase::Prehook,
            PhaseSpec::Polling => SyscallPhase::Polling,
            PhaseSpec::Posthook => SyscallPhase::Posthook,
        }
    }
}

/// One ordering constraint: `before` happens-before `after`.
#[derive(PartialEq, Eq, Debug, Clone, Serialize, Deserialize)]
pub struct EdgeSpec {
    /// The name of the source event (must be observed first).
    pub before: String,
    /// The name of the sink event (blocked until the source fires).
    pub after: String,
    /// Enforcement strength; defaults to [`Strength::Hard`].
    #[serde(default)]
    pub strength: Strength,
}

/// How strictly an edge is enforced by the scheduler.
#[derive(PartialEq, Eq, Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Strength {
    /// Park the sink thread in a true gate until the source fires. The guarantee
    /// wanted for constructed repros, and the default.
    #[default]
    Hard,
    /// Merely bias scheduling (priority nudge); the sink may still run if it is
    /// the only runnable thread.
    Soft,
}

// ================================================================================
// Normalized model
// ================================================================================

/// A resolved reference to a thread.
#[derive(PartialEq, Eq, Debug, Clone, PartialOrd, Ord)]
pub struct ThreadRef {
    /// The symbolic label (map key, or the raw id as text).
    pub label: String,
    /// The concrete `DetTid`, when statically known.
    pub dettid: Option<DetTid>,
    /// Resolve to the Nth spawned thread, when that is the rule.
    pub spawn_ordinal: Option<u32>,
}

/// A source-level location, resolvable to/from an address via debug info.
#[derive(PartialEq, Eq, Debug, Clone, Default)]
pub struct CodeLocation {
    /// Function name.
    pub function: Option<String>,
    /// Source file name.
    pub file: Option<String>,
    /// Source line number.
    pub line: Option<u32>,
}

impl CodeLocation {
    /// True when this location carries no information.
    pub fn is_empty(&self) -> bool {
        self.function.is_none() && self.file.is_none() && self.line.is_none()
    }
}

impl fmt::Display for CodeLocation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (&self.function, &self.file, self.line) {
            (Some(func), _, Some(line)) => write!(f, "{}:{}", func, line),
            (Some(func), _, None) => write!(f, "{}", func),
            (None, Some(file), Some(line)) => write!(f, "{}:{}", file, line),
            (None, Some(file), None) => write!(f, "{}", file),
            (None, None, Some(line)) => write!(f, "line {}", line),
            (None, None, None) => write!(f, "<unlocated>"),
        }
    }
}

/// The deterministic per-thread stop point that an anchor addresses.
///
/// All variants reduce to "a predicate over this thread's deterministic event
/// stream plus an occurrence count," but the two leading variants
/// ([`Position::SyscallCount`] and [`Position::Rcb`]) are absolute counts that
/// need no per-anchor occurrence tracking.
#[derive(PartialEq, Eq, Debug, Clone)]
pub enum Position {
    /// After the thread has executed exactly this many syscalls (any syscall).
    SyscallCount(u64),

    /// When the thread's RCB clock reaches this absolute value.
    Rcb(u64),

    /// The `nth` occurrence of a specific syscall on the thread, optionally
    /// phase-qualified and restricted to one file descriptor. The occurrence is
    /// counted per thread, over that thread's calls that match `sysno` (and
    /// `fd`), so an anchor needs no calibration run to find a syscall count.
    ///
    /// Two properties to know when writing one:
    /// - What is counted is syscall *entries*, the same unit as
    ///   [`Position::SyscallCount`]. A matching call that a signal interrupts
    ///   and the kernel restarts is entered, and counted, again.
    /// - The counters belong to the thread, like its syscall count: a new
    ///   thread starts at zero, an exec keeps them, and after a non-leader exec
    ///   the calling thread's counters continue under the leader's thread id.
    /// - By default it fires at the prehook: an edge then orders the BEFORE
    ///   thread *reaching* its syscall, not that syscall's effect. The held
    ///   thread is re-admitted at the next drain, so a BEFORE syscall that
    ///   takes more scheduler turns than the AFTER thread needs (a `write` to
    ///   stdout takes a checkpoint turn and a resource turn) can let the AFTER
    ///   thread's effect land first, even under the default scheduler.
    /// - With `phase` posthook it fires when the syscall's result is final:
    ///   a value or an errno, `EINTR` included, decided before the guest
    ///   resumes (<https://github.com/rrnewton/hermit/issues/3929>). An entry
    ///   that returns an internal restart code is refused
    ///   (`HERMIT_HB_POSTHOOK_INTERRUPTED`), because the kernel decides after
    ///   that point whether it returns at all. A call that only a signal
    ///   handler ends (`pause`, `rt_sigsuspend`) always returns one, so a
    ///   posthook anchor on it always refuses. Occurrences are counted at entry
    ///   either way, and the five syscalls that do not return on success are
    ///   refused at load ([`syscall_never_returns_on_success`]).
    /// - What a hard edge orders depends on both ends. A posthook BEFORE and a
    ///   prehook AFTER order the BEFORE syscall's completed effect before the
    ///   AFTER syscall runs. A posthook AFTER holds its thread only after its
    ///   own call, so that call's effect may come before the BEFORE one's.
    ///   With a prehook BEFORE, effects may land in either order.
    /// - A posthook BEFORE on a call that only a held thread can complete
    ///   never fires. A child wait or futex wait on the held thread ends in
    ///   `HERMIT_HB_ANCHOR_NEVER_FIRED`; a read whose only writer is held is
    ///   external input to the scheduler, and the run ends at `--timeout`.
    Syscall {
        /// The syscall number.
        sysno: Sysno,
        /// Restrict to a phase, or match any phase when `None`.
        phase: Option<SyscallPhase>,
        /// 1-based occurrence.
        nth: u64,
        /// Restrict to calls whose first argument is this file descriptor.
        fd: Option<i32>,
        /// Restrict `futex` to calls whose operation (the low 32 bits of the
        /// second argument, as an `int`) is this value.
        futex_op: Option<i32>,
    },

    /// The `nth` execution of the instruction at an absolute address. The address
    /// is resolved later when it comes from a [`CodeLocation`].
    Rip {
        /// Absolute instruction pointer, or `None` until resolved from a
        /// [`CodeLocation`].
        addr: Option<u64>,
        /// 1-based occurrence.
        nth: u64,
    },

    /// A cooperative guest marker. Reserved for a future backend.
    Marker {
        /// Marker name.
        name: String,
        /// 1-based occurrence.
        nth: u64,
    },
}

impl fmt::Display for Position {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Position::SyscallCount(n) => write!(f, "after {} syscalls", n),
            Position::Rcb(m) => write!(f, "at RCB {}", m),
            Position::Syscall {
                sysno,
                phase,
                nth,
                fd,
                futex_op,
            } => {
                write!(f, "{}", sysno.name())?;
                if let Some(fd) = fd {
                    write!(f, "(fd={})", fd)?;
                }
                if let Some(op) = futex_op {
                    write!(f, "(op={})", op)?;
                }
                if let Some(p) = phase {
                    write!(f, "@{:?}", p)?;
                }
                write!(f, "#{}", nth)
            }
            Position::Rip { addr, nth } => match addr {
                Some(a) => write!(f, "@{:#x}#{}", a, nth),
                None => write!(f, "@<unresolved>#{}", nth),
            },
            Position::Marker { name, nth } => write!(f, "mark:{}#{}", name, nth),
        }
    }
}

/// A fully normalized anchor: a named, deterministic per-thread stop point.
#[derive(PartialEq, Eq, Debug, Clone)]
pub struct Anchor {
    /// The event name (map key), for diagnostics and edge references.
    pub name: String,
    /// The thread this anchor is on.
    pub thread: ThreadRef,
    /// The deterministic position selector.
    pub position: Position,
    /// Optional human-legible / debug-info-resolved code location.
    pub location: CodeLocation,
    /// For a relative anchor, the event on the same thread after which its
    /// occurrences are counted (`from`).
    pub from: Option<String>,
}

impl fmt::Display for Anchor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}[{}: {}", self.name, self.thread.label, self.position)?;
        if let Some(base) = &self.from {
            write!(f, " after {}", base)?;
        }
        if !self.location.is_empty() {
            write!(f, " ({})", self.location)?;
        }
        write!(f, "]")
    }
}

impl Anchor {
    /// True for an enforced syscall-occurrence anchor: a [`Position::Syscall`]
    /// with no phase, the prehook phase or the posthook phase. Its
    /// occurrences are counted at the prehook of each matching syscall entry.
    /// It fires at the prehook of the thread's `nth` matching entry, or, for
    /// the posthook phase ([`Anchor::fires_at_posthook`]), when that entry
    /// completes. The polling phase is not enforced.
    pub fn is_syscall_occurrence(&self) -> bool {
        matches!(
            self.position,
            Position::Syscall {
                phase: None | Some(SyscallPhase::Prehook) | Some(SyscallPhase::Posthook),
                ..
            }
        )
    }

    /// True for a syscall-occurrence anchor that fires when its syscall
    /// completes: after the call's effect and result, before the guest
    /// resumes (<https://github.com/rrnewton/hermit/issues/3929>). An edge
    /// whose BEFORE anchor fires at the posthook orders that syscall's
    /// completed effect, not just its entry.
    pub fn fires_at_posthook(&self) -> bool {
        matches!(
            self.position,
            Position::Syscall {
                phase: Some(SyscallPhase::Posthook),
                ..
            }
        )
    }

    /// The per-thread counter this anchor's occurrences are counted in: its
    /// syscall number, fd and futex operation, or `None` for any other kind of
    /// anchor.
    pub fn occurrence_key(&self) -> Option<OccurrenceKey> {
        match &self.position {
            Position::Syscall {
                sysno,
                fd,
                futex_op,
                ..
            } if self.is_syscall_occurrence() => Some((*sysno as usize, *fd, *futex_op)),
            _ => None,
        }
    }

    /// True when this is an enforced syscall-occurrence anchor and a call to
    /// `sysno` with first two arguments `args` is one of the calls it counts.
    pub fn syscall_occurrence_matches(&self, sysno: Sysno, args: [usize; 2]) -> bool {
        match &self.position {
            Position::Syscall {
                sysno: want,
                fd,
                futex_op,
                ..
            } if self.is_syscall_occurrence() && *want == sysno => {
                // A file descriptor and a futex operation are each an `int`:
                // the kernel reads the low 32 bits of the register.
                fd.is_none_or(|fd| args[0] as u32 as i32 == fd)
                    && futex_op.is_none_or(|op| args[1] as u32 as i32 == op)
            }
            _ => false,
        }
    }
}

/// True for a syscall that does not return to its caller when it succeeds: the
/// exec family replaces the image, and the exit family ends the thread. A
/// posthook anchor on one is refused at load.
pub fn syscall_never_returns_on_success(sysno: Sysno) -> bool {
    matches!(
        sysno,
        Sysno::execve | Sysno::execveat | Sysno::exit | Sysno::exit_group | Sysno::rt_sigreturn
    )
}

/// True for a syscall that never reaches Hermit's tracer on the ptrace
/// backend, whatever Detcore subscribes to: Reverie lets `rt_sigreturn` run
/// untraced from its private page so a signal frame is restored safely. A
/// syscall anchor on one could never fire, so the run refuses it.
pub fn syscall_never_reaches_tracer(sysno: Sysno) -> bool {
    sysno == Sysno::rt_sigreturn
}

/// The key of one per-thread occurrence counter: a syscall number, an optional
/// file descriptor and an optional futex operation ([`Anchor::occurrence_key`]).
pub type OccurrenceKey = (usize, Option<i32>, Option<i32>);

/// One thread's syscall-occurrence counters
/// ([`HappensBeforeProgram::count_syscall_occurrences`]).
pub type OccurrenceCounters = BTreeMap<OccurrenceKey, u64>;

/// One thread's happens-before counting state: its occurrence counters, and for
/// each relative anchor whose base this thread has reached, the origin: the
/// count of the relative anchor's counter just after the base entry. Belongs to
/// the thread, like its syscall count: empty for a new thread, kept across the
/// thread's own exec.
#[derive(PartialEq, Eq, Debug, Clone, Default, Serialize, Deserialize)]
pub struct HbThreadCounters {
    /// Per occurrence key, how many of this thread's entries matched.
    pub counts: OccurrenceCounters,
    /// Per relative anchor whose base was reached, its origin. Each relative
    /// anchor has its own origin, so the shared counters are never reset.
    pub origins: BTreeMap<String, u64>,
}

/// True when `sysno`'s first argument is a file descriptor, so an anchor may
/// restrict it with `fd`. The list is the fd-first syscalls a guest is likely to
/// anchor on; an `fd` on any other syscall is refused when the spec is loaded
/// rather than matched against an argument that is not a descriptor.
pub fn syscall_takes_fd_first(sysno: Sysno) -> bool {
    matches!(
        sysno,
        Sysno::read
            | Sysno::write
            | Sysno::readv
            | Sysno::writev
            | Sysno::pread64
            | Sysno::pwrite64
            | Sysno::preadv
            | Sysno::pwritev
            | Sysno::preadv2
            | Sysno::pwritev2
            | Sysno::close
            | Sysno::fsync
            | Sysno::fdatasync
            | Sysno::ioctl
            | Sysno::fcntl
            | Sysno::lseek
            | Sysno::fstat
            | Sysno::ftruncate
            | Sysno::fallocate
            | Sysno::flock
            | Sysno::getdents64
            | Sysno::sendto
            | Sysno::recvfrom
            | Sysno::sendmsg
            | Sysno::recvmsg
            | Sysno::sendmmsg
            | Sysno::recvmmsg
            | Sysno::accept
            | Sysno::accept4
            | Sysno::connect
            | Sysno::bind
            | Sysno::listen
            | Sysno::shutdown
            | Sysno::epoll_wait
            | Sysno::epoll_pwait
            | Sysno::epoll_ctl
            | Sysno::dup
            | Sysno::dup2
            | Sysno::dup3
    )
}

/// A normalized happens-before edge between two anchors.
#[derive(PartialEq, Eq, Debug, Clone)]
pub struct HappensBeforeEdge {
    /// The source anchor name (observed first).
    pub before: String,
    /// The sink anchor name (gated until the source fires).
    pub after: String,
    /// Enforcement strength.
    pub strength: Strength,
}

/// A validated, normalized happens-before program: anchors indexed by name plus
/// the edge list, guaranteed acyclic with all references resolved.
#[derive(PartialEq, Eq, Debug, Clone)]
pub struct HappensBeforeProgram {
    /// The spec's schema version.
    pub version: u32,
    /// Normalized anchors, keyed by event name.
    pub anchors: BTreeMap<String, Anchor>,
    /// The validated, acyclic edge list.
    pub edges: Vec<HappensBeforeEdge>,
    /// The hold budget in committed virtual nanoseconds: `Some` for a
    /// version-2 spec (its `max_hold_ns`, or [`DEFAULT_MAX_HOLD_NS`]), `None`
    /// for version 1, which has no budget.
    pub max_hold_ns: Option<u64>,
}

impl HappensBeforeProgram {
    /// Anchors that still require debug-info resolution (an unresolved RIP from a
    /// code location, i.e. a `func`/`line` that has not been turned into an
    /// address yet).
    pub fn unresolved_locations(&self) -> impl Iterator<Item = &Anchor> {
        self.anchors.values().filter(|a| {
            matches!(a.position, Position::Rip { addr: None, .. }) && !a.location.is_empty()
        })
    }

    /// True when thread `dettid` must check in with the scheduler at its
    /// syscall number `count`: some anchor sits at exactly
    /// [`Position::SyscallCount`]`(count)` and names `dettid`, or names its
    /// thread by spawn ordinal (which only the scheduler can resolve, so the
    /// guest side must ask). For any other (thread, count) the scheduler's
    /// checkpoint finds no anchor and does nothing, so skipping it changes no
    /// gate. It costs one scheduler round trip per anchor its thread reaches,
    /// not one per syscall (<https://github.com/rrnewton/hermit/issues/3877>),
    /// and threads a spec does not name keep their schedule: a count another
    /// thread passes early (QEMU's main loop makes about a million syscalls a
    /// run) no longer adds a scheduler turn there.
    pub fn may_have_syscall_count_anchor_at(&self, dettid: DetTid, count: u64) -> bool {
        self.anchors.values().any(|a| {
            matches!(a.position, Position::SyscallCount(n) if n == count)
                && a.thread.dettid.is_none_or(|d| d == dettid)
        })
    }

    /// True when the program has any enforced syscall-occurrence anchor
    /// ([`Anchor::is_syscall_occurrence`]), so the guest must count its calls.
    pub fn has_syscall_occurrence_anchors(&self) -> bool {
        self.anchors.values().any(Anchor::is_syscall_occurrence)
    }

    /// Count one syscall entry of thread `dettid` (`sysno`, first two
    /// arguments `args`, the thread's `entry_count`th syscall) in that
    /// thread's `counters`, and return the names of the syscall-occurrence
    /// anchors this entry fires.
    ///
    /// `counters` belongs to one thread and is kept with its other per-thread
    /// counts (the syscall count that count anchors use), so it starts empty
    /// for every new thread. Per entry, in this order:
    /// 1. Every distinct occurrence key (syscall, fd, futex operation) an
    ///    anchor names that the entry matches is incremented once; an entry
    ///    that matches both an fd-specific and an fd-agnostic anchor counts
    ///    toward each.
    /// 2. Each relative anchor whose base this entry reaches gets its origin:
    ///    its own key's count after step 1, so the base entry is never counted
    ///    in its window. A count base is reached when `entry_count` equals its
    ///    count, whether or not the entry matches any key.
    /// 3. An absolute anchor fires when its key's count equals `nth`; a
    ///    relative anchor fires when its count minus its origin equals `nth`.
    ///
    /// An entry is counted exactly once, when the thread enters the syscall,
    /// so a thread held at an anchor and re-admitted does not count again. The
    /// result names anchors on `dettid` or on a spawn ordinal; the scheduler
    /// resolves spawn ordinals and fires only the anchors on this thread.
    pub fn count_syscall_occurrences(
        &self,
        counters: &mut HbThreadCounters,
        dettid: DetTid,
        sysno: Sysno,
        args: [usize; 2],
        entry_count: u64,
    ) -> Vec<String> {
        let on_thread = |a: &&Anchor| a.thread.dettid.is_none_or(|d| d == dettid);
        let keys: BTreeSet<OccurrenceKey> = self
            .anchors
            .values()
            .filter(|a| a.syscall_occurrence_matches(sysno, args))
            .filter_map(Anchor::occurrence_key)
            .collect();
        for key in &keys {
            *counters.counts.entry(*key).or_insert(0) += 1;
        }
        for relative in self.anchors.values().filter(on_thread) {
            let Some(base) = relative.from.as_ref().and_then(|b| self.anchors.get(b)) else {
                continue;
            };
            if counters.origins.contains_key(&relative.name) {
                continue;
            }
            let reached = match &base.position {
                Position::SyscallCount(n) => *n == entry_count,
                Position::Syscall { nth, .. } => {
                    base.syscall_occurrence_matches(sysno, args)
                        && base
                            .occurrence_key()
                            .is_some_and(|key| counters.counts.get(&key) == Some(nth))
                }
                _ => false,
            };
            if reached {
                let origin = relative
                    .occurrence_key()
                    .and_then(|key| counters.counts.get(&key).copied())
                    .unwrap_or(0);
                counters.origins.insert(relative.name.clone(), origin);
            }
        }
        if keys.is_empty() {
            return Vec::new();
        }
        self.anchors
            .values()
            .filter(on_thread)
            .filter(|a| a.syscall_occurrence_matches(sysno, args))
            .filter(|a| match (&a.position, a.occurrence_key()) {
                (Position::Syscall { nth, .. }, Some(key)) => {
                    let count = counters.counts.get(&key).copied().unwrap_or(0);
                    match &a.from {
                        None => count == *nth,
                        Some(_) => counters
                            .origins
                            .get(&a.name)
                            .is_some_and(|origin| count - origin == *nth),
                    }
                }
                _ => false,
            })
            .map(|a| a.name.clone())
            .collect()
    }

    /// True for a version-2 program, which carries a hold budget and needs an
    /// effective preemption timer to charge it.
    pub fn is_version_2(&self) -> bool {
        self.version == HAPPENS_BEFORE_VERSION
    }

    /// The relative anchors and their bases, in name order.
    pub fn relative_anchors(&self) -> impl Iterator<Item = (&Anchor, &Anchor)> {
        self.anchors.values().filter_map(|a| {
            a.from
                .as_ref()
                .and_then(|b| self.anchors.get(b))
                .map(|base| (a, base))
        })
    }

    /// Anchors whose position kind the current scheduler does not yet enforce:
    /// everything other than [`Position::SyscallCount`] and the syscall
    /// occurrence anchors ([`Anchor::is_syscall_occurrence`]). Reported so a run
    /// does not silently ignore an authored ordering constraint it cannot honor.
    pub fn unenforced_positions(&self) -> impl Iterator<Item = &Anchor> {
        self.anchors.values().filter(|a| {
            !matches!(a.position, Position::SyscallCount(_)) && !a.is_syscall_occurrence()
        })
    }
}

// ================================================================================
// Errors
// ================================================================================

/// An error produced while parsing or validating a happens-before specification.
#[derive(PartialEq, Eq, Debug, Clone)]
pub enum HappensBeforeError {
    /// The schema `version` is not understood by this build.
    UnsupportedVersion(u32),
    /// An event named more than one position selector, or none.
    AmbiguousPosition {
        /// The offending event name.
        event: String,
        /// The selectors that were set.
        found: Vec<String>,
    },
    /// A syscall name could not be parsed.
    UnknownSyscall {
        /// The offending event name.
        event: String,
        /// The unparseable name.
        name: String,
    },
    /// An occurrence-counted event (`syscall`, `rip`, `mark` or a code
    /// location) set `nth` to 0; occurrences are 1-based.
    NthZero {
        /// The offending event name.
        event: String,
    },
    /// An event set `fd` without `syscall`.
    FdWithoutSyscall {
        /// The offending event name.
        event: String,
    },
    /// An event set `fd` on a syscall whose first argument is not a file
    /// descriptor ([`syscall_takes_fd_first`]).
    FdNotFirstArgument {
        /// The offending event name.
        event: String,
        /// The syscall name.
        syscall: String,
    },
    /// A RIP string could not be parsed as an address.
    BadRip {
        /// The offending event name.
        event: String,
        /// The unparseable text.
        text: String,
    },
    /// An edge referenced an event that does not exist.
    UnknownEvent {
        /// `before` or `after`.
        which: String,
        /// The dangling name.
        name: String,
    },
    /// An event referenced a thread label that is not in the `threads` table and
    /// is not a raw integer id.
    UnknownThread {
        /// The offending event name.
        event: String,
        /// The dangling thread label.
        thread: String,
    },
    /// The edge graph contains a cycle (listed in discovery order).
    Cycle(Vec<String>),
    /// A version-1 spec used a field that version 2 added.
    NewFieldInVersion1 {
        /// The offending event name, or `None` for a spec-level field.
        event: Option<String>,
        /// The field.
        field: String,
    },
    /// A version-2 spec carried a field this build does not know.
    UnknownField {
        /// Where: `spec`, `event 'x'`, `thread 'x'` or `edge N`.
        place: String,
        /// The field.
        field: String,
    },
    /// An event set `futex_op` on a syscall other than `futex`, or without
    /// `syscall`.
    FutexOpWithoutFutex {
        /// The offending event name.
        event: String,
    },
    /// An event's `futex_op` does not fit the kernel's `int`.
    FutexOpOutOfRange {
        /// The offending event name.
        event: String,
        /// The value given.
        value: i64,
    },
    /// An event's `from` names an event that does not exist.
    FromUnknownEvent {
        /// The relative event.
        event: String,
        /// The missing base.
        base: String,
    },
    /// An event that is not a prehook syscall-occurrence anchor set `from`.
    FromOnNonOccurrence {
        /// The offending event name.
        event: String,
    },
    /// A relative event's base cannot be shown to be on the same thread: it
    /// names another dettid or spawn ordinal, or a dettid where the relative
    /// event names a spawn ordinal (or the reverse).
    FromOtherThread {
        /// The relative event.
        event: String,
        /// Its base.
        base: String,
    },
    /// A relative event's base is neither a count anchor nor an enforced
    /// syscall-occurrence anchor, so it can never be reached.
    FromUnenforcedPosition {
        /// The relative event.
        event: String,
        /// Its base.
        base: String,
    },
    /// A relative event's base is itself relative; chains are not supported.
    FromRelativeBase {
        /// The relative event.
        event: String,
        /// Its base.
        base: String,
    },
    /// The relative bases and the hard edges together form a cycle, so some
    /// event on it could only be reached after itself.
    FromCycle(Vec<String>),
    /// A posthook anchor names a syscall that does not return to its caller
    /// when it succeeds ([`syscall_never_returns_on_success`]).
    PosthookOnNoReturnSyscall {
        /// The offending event name.
        event: String,
        /// The syscall name.
        syscall: String,
    },
    /// A DSL line could not be parsed.
    DslSyntax {
        /// 1-based line number.
        line: usize,
        /// What went wrong.
        message: String,
    },
}

impl fmt::Display for HappensBeforeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HappensBeforeError::UnsupportedVersion(v) => write!(
                f,
                "unsupported happens-before schema version {} (this build understands {} and {})",
                v, HAPPENS_BEFORE_VERSION_1, HAPPENS_BEFORE_VERSION
            ),
            HappensBeforeError::AmbiguousPosition { event, found } => {
                if found.is_empty() {
                    write!(
                        f,
                        "event '{}' must specify a position: a count (syscalls/rcbs), a syscall, a \
                         rip, a mark, or a code location (func/file/line)",
                        event
                    )
                } else {
                    write!(
                        f,
                        "event '{}' names conflicting positions {:?}; use at most one explicit \
                         position selector (a code location may accompany it)",
                        event, found
                    )
                }
            }
            HappensBeforeError::UnknownSyscall { event, name } => {
                write!(f, "event '{}' names unknown syscall '{}'", event, name)
            }
            HappensBeforeError::NthZero { event } => write!(
                f,
                "event '{}' sets 'nth' to 0; occurrences are counted from 1",
                event
            ),
            HappensBeforeError::FdWithoutSyscall { event } => write!(
                f,
                "event '{}' sets 'fd' without 'syscall'; 'fd' restricts a named syscall",
                event
            ),
            HappensBeforeError::FdNotFirstArgument { event, syscall } => write!(
                f,
                "event '{}' sets 'fd' on syscall '{}', whose first argument is not a file \
                 descriptor",
                event, syscall
            ),
            HappensBeforeError::BadRip { event, text } => {
                write!(f, "event '{}' has unparseable rip '{}'", event, text)
            }
            HappensBeforeError::UnknownEvent { which, name } => {
                write!(f, "edge '{}' references unknown event '{}'", which, name)
            }
            HappensBeforeError::UnknownThread { event, thread } => write!(
                f,
                "event '{}' references unknown thread '{}'",
                event, thread
            ),
            HappensBeforeError::Cycle(names) => {
                write!(
                    f,
                    "happens-before edges contain a cycle: {}",
                    names.join(" -> ")
                )
            }
            HappensBeforeError::DslSyntax { line, message } => {
                write!(f, "DSL parse error on line {}: {}", line, message)
            }
            HappensBeforeError::NewFieldInVersion1 { event, field } => match event {
                Some(event) => write!(
                    f,
                    "event '{}' sets '{}', which needs \"version\": 2",
                    event, field
                ),
                None => write!(f, "the spec sets '{}', which needs \"version\": 2", field),
            },
            HappensBeforeError::UnknownField { place, field } => write!(
                f,
                "{} has unknown field '{}' (version 2 refuses unknown fields)",
                place, field
            ),
            HappensBeforeError::FutexOpWithoutFutex { event } => write!(
                f,
                "event '{}' sets 'futex_op', which applies only to \"syscall\": \"futex\"",
                event
            ),
            HappensBeforeError::FutexOpOutOfRange { event, value } => write!(
                f,
                "event '{}' sets 'futex_op' to {}, which does not fit the kernel's int",
                event, value
            ),
            HappensBeforeError::FromUnknownEvent { event, base } => {
                write!(f, "event '{}' counts from unknown event '{}'", event, base)
            }
            HappensBeforeError::FromOnNonOccurrence { event } => write!(
                f,
                "event '{}' sets 'from', which applies only to a syscall anchor at the prehook",
                event
            ),
            HappensBeforeError::FromOtherThread { event, base } => write!(
                f,
                "event '{}' counts from '{}', which cannot be shown to be on the same thread; a \
                 base must name the same dettid, or the same spawn ordinal",
                event, base
            ),
            HappensBeforeError::FromUnenforcedPosition { event, base } => write!(
                f,
                "event '{}' counts from '{}', which is neither a count anchor nor a syscall anchor \
                 at the prehook, so it is never reached",
                event, base
            ),
            HappensBeforeError::FromRelativeBase { event, base } => write!(
                f,
                "event '{}' counts from '{}', which itself uses 'from'; relative chains are not \
                 supported",
                event, base
            ),
            HappensBeforeError::PosthookOnNoReturnSyscall { event, syscall } => write!(
                f,
                "event '{}' fires at the posthook of {}, which does not return to its caller when \
                 it succeeds, so the anchor could only fire on a failed call",
                event, syscall
            ),
            HappensBeforeError::FromCycle(names) => write!(
                f,
                "relative bases and hard edges together contain a cycle: {}",
                names.join(" -> ")
            ),
        }
    }
}

impl std::error::Error for HappensBeforeError {}

// ================================================================================
// Parsing & normalization
// ================================================================================

impl HappensBeforeSpec {
    /// Parse a JSON specification. A version-2 spec is also checked for
    /// fields this build does not know ([`HappensBeforeError::UnknownField`]),
    /// so a misspelled field is refused instead of silently ignored; version 1
    /// keeps its old, lenient handling of unknown fields.
    ///
    /// The spec itself is deserialized from the original text, not from the
    /// `Value` used for that check: a `Value` keeps only the last of two equal
    /// keys, which would accept `"nth": 1, "nth": 2` as `nth` 2. Deserializing
    /// the text refuses a duplicated field, in either version.
    pub fn from_json(s: &str) -> anyhow::Result<HappensBeforeSpec> {
        let value: serde_json::Value = serde_json::from_str(s)?;
        if value.get("version").and_then(serde_json::Value::as_u64)
            == Some(u64::from(HAPPENS_BEFORE_VERSION))
        {
            check_known_fields(&value)?;
        }
        Ok(serde_json::from_str(s)?)
    }

    /// Serialize to pretty JSON.
    pub fn to_json(&self) -> anyhow::Result<String> {
        Ok(serde_json::to_string_pretty(self)?)
    }

    /// Normalize and statically validate: check the version, resolve thread and
    /// event references, require exactly one position per event, parse syscalls
    /// and RIPs, and confirm the edge graph is acyclic.
    pub fn normalize(&self) -> Result<HappensBeforeProgram, HappensBeforeError> {
        if self.version != HAPPENS_BEFORE_VERSION && self.version != HAPPENS_BEFORE_VERSION_1 {
            return Err(HappensBeforeError::UnsupportedVersion(self.version));
        }
        if self.version == HAPPENS_BEFORE_VERSION_1 {
            self.refuse_version_2_fields()?;
        }

        let mut anchors = BTreeMap::new();
        for (name, ev) in &self.events {
            anchors.insert(name.clone(), self.normalize_event(name, ev)?);
        }
        check_relative_bases(&anchors)?;

        // Resolve edges against the anchor table.
        let mut edges = Vec::with_capacity(self.edges.len());
        for e in &self.edges {
            if !anchors.contains_key(&e.before) {
                return Err(HappensBeforeError::UnknownEvent {
                    which: "before".to_string(),
                    name: e.before.clone(),
                });
            }
            if !anchors.contains_key(&e.after) {
                return Err(HappensBeforeError::UnknownEvent {
                    which: "after".to_string(),
                    name: e.after.clone(),
                });
            }
            edges.push(HappensBeforeEdge {
                before: e.before.clone(),
                after: e.after.clone(),
                strength: e.strength,
            });
        }

        detect_cycle(&anchors, &edges)?;
        // A relative event is reached only after its base, on the same thread:
        // an activation edge base -> relative. With the hard edges, a cycle
        // means some event could only be reached after itself.
        let mut with_activation = edges.clone();
        with_activation.extend(
            anchors
                .values()
                .filter_map(|a| a.from.as_ref().map(|base| (base, a)))
                .map(|(base, a)| HappensBeforeEdge {
                    before: base.clone(),
                    after: a.name.clone(),
                    strength: Strength::Hard,
                }),
        );
        with_activation.retain(|e| e.strength == Strength::Hard);
        if let Err(HappensBeforeError::Cycle(names)) = detect_cycle(&anchors, &with_activation) {
            return Err(HappensBeforeError::FromCycle(names));
        }

        let max_hold_ns = (self.version == HAPPENS_BEFORE_VERSION)
            .then(|| self.max_hold_ns.unwrap_or(DEFAULT_MAX_HOLD_NS));
        Ok(HappensBeforeProgram {
            version: self.version,
            anchors,
            edges,
            max_hold_ns,
        })
    }

    /// Refuse, in a version-1 spec, every field that version 2 added.
    fn refuse_version_2_fields(&self) -> Result<(), HappensBeforeError> {
        if self.max_hold_ns.is_some() {
            return Err(HappensBeforeError::NewFieldInVersion1 {
                event: None,
                field: "max_hold_ns".to_owned(),
            });
        }
        for (name, ev) in &self.events {
            for (field, set) in [
                ("from", ev.from.is_some()),
                ("futex_op", ev.futex_op.is_some()),
            ] {
                if set {
                    return Err(HappensBeforeError::NewFieldInVersion1 {
                        event: Some(name.clone()),
                        field: field.to_owned(),
                    });
                }
            }
        }
        Ok(())
    }

    /// Resolve one event into a normalized [`Anchor`].
    fn normalize_event(&self, name: &str, ev: &EventSpec) -> Result<Anchor, HappensBeforeError> {
        let thread = self.resolve_thread(name, &ev.thread)?;

        // A code location can accompany any position; it also *supplies* a RIP
        // position when no explicit position selector is present.
        let location = CodeLocation {
            function: ev.func.clone(),
            file: ev.file.clone(),
            line: ev.line,
        };

        // Determine which *explicit* position selectors are present. A code
        // location (`func`/`file`/`line`) is descriptive and may accompany any
        // one of these — the owner's primary anchor is "function foo on thread T
        // after N syscalls / M RBCs", i.e. a code location *and* a count. The
        // code location only *becomes* the (deferred RIP) position when no
        // explicit selector is present at all.
        let mut found: Vec<&str> = Vec::new();
        if ev.syscalls.is_some() {
            found.push("syscalls");
        }
        if ev.rcbs.is_some() {
            found.push("rcbs");
        }
        if ev.syscall.is_some() {
            found.push("syscall");
        }
        if ev.rip.is_some() {
            found.push("rip");
        }
        if ev.mark.is_some() {
            found.push("mark");
        }
        let has_code_location = !location.is_empty();

        // Reject multiple explicit selectors outright. A single explicit
        // selector wins as the position (code location stays descriptive). Zero
        // explicit selectors is only valid when a code location supplies a RIP.
        if found.len() > 1 {
            return Err(HappensBeforeError::AmbiguousPosition {
                event: name.to_string(),
                found: found.iter().map(|s| s.to_string()).collect(),
            });
        }
        if found.is_empty() && !has_code_location {
            return Err(HappensBeforeError::AmbiguousPosition {
                event: name.to_string(),
                found: Vec::new(),
            });
        }

        if ev.fd.is_some() && ev.syscall.is_none() {
            return Err(HappensBeforeError::FdWithoutSyscall {
                event: name.to_string(),
            });
        }
        if ev.futex_op.is_some() && ev.syscall.is_none() {
            return Err(HappensBeforeError::FutexOpWithoutFutex {
                event: name.to_string(),
            });
        }

        if ev.nth == Some(0) && ev.syscalls.is_none() && ev.rcbs.is_none() {
            return Err(HappensBeforeError::NthZero {
                event: name.to_string(),
            });
        }

        let nth = ev.nth.unwrap_or(1);
        let position = if let Some(n) = ev.syscalls {
            Position::SyscallCount(n)
        } else if let Some(m) = ev.rcbs {
            Position::Rcb(m)
        } else if let Some(sc) = &ev.syscall {
            let sysno = Sysno::from_str(sc).map_err(|_| HappensBeforeError::UnknownSyscall {
                event: name.to_string(),
                name: sc.clone(),
            })?;
            if ev.fd.is_some() && !syscall_takes_fd_first(sysno) {
                return Err(HappensBeforeError::FdNotFirstArgument {
                    event: name.to_string(),
                    syscall: sc.clone(),
                });
            }
            let futex_op = match ev.futex_op {
                None => None,
                Some(_) if sysno != Sysno::futex => {
                    return Err(HappensBeforeError::FutexOpWithoutFutex {
                        event: name.to_string(),
                    });
                }
                Some(value) => Some(i32::try_from(value).map_err(|_| {
                    HappensBeforeError::FutexOpOutOfRange {
                        event: name.to_string(),
                        value,
                    }
                })?),
            };
            if ev.phase == Some(PhaseSpec::Posthook) && syscall_never_returns_on_success(sysno) {
                return Err(HappensBeforeError::PosthookOnNoReturnSyscall {
                    event: name.to_string(),
                    syscall: sc.clone(),
                });
            }
            Position::Syscall {
                sysno,
                phase: ev.phase.map(Into::into),
                nth,
                fd: ev.fd,
                futex_op,
            }
        } else if let Some(rip) = &ev.rip {
            let addr = parse_rip(rip).ok_or_else(|| HappensBeforeError::BadRip {
                event: name.to_string(),
                text: rip.clone(),
            })?;
            Position::Rip {
                addr: Some(addr),
                nth,
            }
        } else if let Some(mark) = &ev.mark {
            Position::Marker {
                name: mark.clone(),
                nth,
            }
        } else {
            // Code-location-only: a RIP to be resolved later from debug info.
            debug_assert!(has_code_location);
            Position::Rip { addr: None, nth }
        };

        Ok(Anchor {
            name: name.to_string(),
            thread,
            position,
            location,
            from: ev.from.clone(),
        })
    }

    /// Resolve an event's `thread` field to a [`ThreadRef`], consulting the
    /// `threads` table and falling back to a raw integer id.
    fn resolve_thread(&self, event: &str, thread: &str) -> Result<ThreadRef, HappensBeforeError> {
        if let Some(spec) = self.threads.get(thread) {
            Ok(ThreadRef {
                label: spec.label.clone().unwrap_or_else(|| thread.to_string()),
                dettid: spec.dettid.map(DetTid::from_raw),
                spawn_ordinal: spec.spawn_ordinal,
            })
        } else if let Ok(raw) = thread.parse::<i32>() {
            Ok(ThreadRef {
                label: thread.to_string(),
                dettid: Some(DetTid::from_raw(raw)),
                spawn_ordinal: None,
            })
        } else {
            Err(HappensBeforeError::UnknownThread {
                event: event.to_string(),
                thread: thread.to_string(),
            })
        }
    }
}

/// Check every relative anchor's `from`: the base exists, is on the same
/// thread, is a count or syscall-occurrence anchor and is not itself relative;
/// the relative anchor is a syscall-occurrence anchor.
fn check_relative_bases(anchors: &BTreeMap<String, Anchor>) -> Result<(), HappensBeforeError> {
    for anchor in anchors.values() {
        let Some(base_name) = &anchor.from else {
            continue;
        };
        let event = anchor.name.clone();
        let base = base_name.clone();
        let Some(base_anchor) = anchors.get(base_name) else {
            return Err(HappensBeforeError::FromUnknownEvent { event, base });
        };
        if !anchor.is_syscall_occurrence() {
            return Err(HappensBeforeError::FromOnNonOccurrence { event });
        }
        if !same_thread(&base_anchor.thread, &anchor.thread) {
            return Err(HappensBeforeError::FromOtherThread { event, base });
        }
        if base_anchor.from.is_some() {
            return Err(HappensBeforeError::FromRelativeBase { event, base });
        }
        if !matches!(base_anchor.position, Position::SyscallCount(_))
            && !base_anchor.is_syscall_occurrence()
        {
            return Err(HappensBeforeError::FromUnenforcedPosition { event, base });
        }
    }
    Ok(())
}

/// True when two thread references resolve to the same thread whatever they
/// are called: the same `dettid`, or, with no `dettid` on either, the same
/// `spawn_ordinal`. A `dettid` and a `spawn_ordinal` cannot be shown equal
/// before the run, so they are not.
fn same_thread(a: &ThreadRef, b: &ThreadRef) -> bool {
    match (a.dettid, b.dettid) {
        (Some(x), Some(y)) => x == y,
        (None, None) => a.spawn_ordinal.is_some() && a.spawn_ordinal == b.spawn_ordinal,
        _ => false,
    }
}

/// Refuse a field a version-2 spec does not define. The field lists mirror
/// the serde structs above, including the `rcb` alias.
fn check_known_fields(value: &serde_json::Value) -> Result<(), HappensBeforeError> {
    const SPEC: &[&str] = &["version", "max_hold_ns", "threads", "events", "edges"];
    const THREAD: &[&str] = &["label", "dettid", "spawn_ordinal"];
    const EVENT: &[&str] = &[
        "thread", "syscalls", "rcbs", "rcb", "func", "file", "line", "syscall", "phase", "fd",
        "futex_op", "from", "rip", "mark", "nth",
    ];
    const EDGE: &[&str] = &["before", "after", "strength"];
    fn check(
        object: Option<&serde_json::Map<String, serde_json::Value>>,
        known: &[&str],
        place: impl Fn() -> String,
    ) -> Result<(), HappensBeforeError> {
        for field in object.into_iter().flat_map(|o| o.keys()) {
            if !known.contains(&field.as_str()) {
                return Err(HappensBeforeError::UnknownField {
                    place: place(),
                    field: field.clone(),
                });
            }
        }
        Ok(())
    }
    check(value.as_object(), SPEC, || "the spec".to_owned())?;
    for (name, thread) in value
        .get("threads")
        .and_then(serde_json::Value::as_object)
        .into_iter()
        .flatten()
    {
        check(thread.as_object(), THREAD, || format!("thread '{name}'"))?;
    }
    for (name, event) in value
        .get("events")
        .and_then(serde_json::Value::as_object)
        .into_iter()
        .flatten()
    {
        check(event.as_object(), EVENT, || format!("event '{name}'"))?;
    }
    for (index, edge) in value
        .get("edges")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
    {
        check(edge.as_object(), EDGE, || format!("edge {index}"))?;
    }
    Ok(())
}

/// Parse a RIP string: hex (`0x...`) or plain decimal.
fn parse_rip(s: &str) -> Option<u64> {
    let s = s.trim();
    if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u64::from_str_radix(hex, 16).ok()
    } else {
        s.parse::<u64>().ok()
    }
}

/// Detect a cycle in the edge graph via depth-first search, returning the cycle
/// path if one exists. Anchors are visited in name order for determinism.
fn detect_cycle(
    anchors: &BTreeMap<String, Anchor>,
    edges: &[HappensBeforeEdge],
) -> Result<(), HappensBeforeError> {
    // Adjacency: before -> [after...]
    let mut adj: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for name in anchors.keys() {
        adj.entry(name.as_str()).or_default();
    }
    for e in edges {
        adj.entry(e.before.as_str())
            .or_default()
            .push(e.after.as_str());
    }

    #[derive(Clone, Copy, PartialEq)]
    enum Mark {
        Visiting,
        Done,
    }
    let mut state: BTreeMap<&str, Mark> = BTreeMap::new();

    // Iterative DFS to avoid stack overflow on deep chains, tracking the current
    // path so we can report a concrete cycle.
    for root in adj.keys().copied() {
        if state.contains_key(root) {
            continue;
        }
        // Stack of (node, index of next neighbor to visit).
        let mut stack: Vec<(&str, usize)> = vec![(root, 0)];
        let mut path: Vec<&str> = vec![root];
        state.insert(root, Mark::Visiting);

        while let Some(&mut (node, ref mut idx)) = stack.last_mut() {
            let neighbors = &adj[node];
            if *idx < neighbors.len() {
                let next = neighbors[*idx];
                *idx += 1;
                match state.get(next) {
                    Some(Mark::Visiting) => {
                        // Found a back-edge: assemble the cycle from `path`.
                        let start = path.iter().position(|&n| n == next).unwrap_or(0);
                        let mut cycle: Vec<String> =
                            path[start..].iter().map(|s| s.to_string()).collect();
                        cycle.push(next.to_string());
                        return Err(HappensBeforeError::Cycle(cycle));
                    }
                    Some(Mark::Done) => {}
                    None => {
                        state.insert(next, Mark::Visiting);
                        path.push(next);
                        stack.push((next, 0));
                    }
                }
            } else {
                state.insert(node, Mark::Done);
                stack.pop();
                path.pop();
            }
        }
    }
    Ok(())
}

// ================================================================================
// Terse DSL
// ================================================================================
//
// One edge per non-empty, non-comment line:
//
//     writer:free_buffer#342  <  reader:read_buffer#97
//     writer:futex@post#5     <  reader:@0x401f3c#1
//     A:rcb=123456            <  B:sc=97
//
// Each side is `thread:anchor[#ordinal]`. The anchor token is one of:
//   * `name`            -> function name (code location)
//   * `@0xADDR`         -> raw RIP
//   * `syscall@phase`   -> a named syscall, optional `@pre`/`@post`/`@polling`
//   * `rcb=M`           -> after M RBCs (owner primary)
//   * `sc=N`            -> after N syscalls (owner primary)
// A trailing `#N` sets the occurrence ordinal (ignored by `rcb=`/`sc=`).
// A `!soft` suffix on the line marks the edge soft; default is hard.

impl HappensBeforeSpec {
    /// Parse the terse line-oriented DSL into a specification. Symbolic threads
    /// mentioned by name become entries in the `threads` table.
    pub fn from_dsl(input: &str) -> Result<HappensBeforeSpec, HappensBeforeError> {
        // The DSL has no version-2 syntax, so it builds a version-1 spec.
        let mut spec = HappensBeforeSpec {
            version: HAPPENS_BEFORE_VERSION_1,
            max_hold_ns: None,
            threads: BTreeMap::new(),
            events: BTreeMap::new(),
            edges: Vec::new(),
        };
        let mut seen_names: BTreeSet<String> = BTreeSet::new();

        for (i, raw_line) in input.lines().enumerate() {
            let lineno = i + 1;
            let line = strip_comment(raw_line).trim();
            if line.is_empty() {
                continue;
            }

            // Optional trailing "!soft" / "!hard".
            let (body, strength) = if let Some(b) = line.strip_suffix("!soft") {
                (b.trim(), Strength::Soft)
            } else if let Some(b) = line.strip_suffix("!hard") {
                (b.trim(), Strength::Hard)
            } else {
                (line, Strength::Hard)
            };

            let (lhs, rhs) = body
                .split_once('<')
                .ok_or_else(|| HappensBeforeError::DslSyntax {
                    line: lineno,
                    message: "expected '<' separating two events".to_string(),
                })?;

            let before = parse_dsl_side(lhs.trim(), lineno, &mut spec, &mut seen_names)?;
            let after = parse_dsl_side(rhs.trim(), lineno, &mut spec, &mut seen_names)?;
            spec.edges.push(EdgeSpec {
                before,
                after,
                strength,
            });
        }
        Ok(spec)
    }
}

/// Strip a `#`-or-`//` comment, but not a `#ordinal` that is part of a token.
/// We treat `//` as the only comment marker to avoid clashing with `#ordinal`.
fn strip_comment(line: &str) -> &str {
    match line.find("//") {
        Some(idx) => &line[..idx],
        None => line,
    }
}

/// Parse one side of a DSL edge, registering the event and thread in `spec`, and
/// returning the generated event name.
fn parse_dsl_side(
    token: &str,
    lineno: usize,
    spec: &mut HappensBeforeSpec,
    seen_names: &mut BTreeSet<String>,
) -> Result<String, HappensBeforeError> {
    let (thread, anchor) = token
        .split_once(':')
        .ok_or_else(|| HappensBeforeError::DslSyntax {
            line: lineno,
            message: format!("expected 'thread:anchor' in '{}'", token),
        })?;
    let thread = thread.trim();
    let anchor = anchor.trim();
    if thread.is_empty() || anchor.is_empty() {
        return Err(HappensBeforeError::DslSyntax {
            line: lineno,
            message: format!("empty thread or anchor in '{}'", token),
        });
    }

    // Split a trailing "#ordinal".
    let (anchor_body, nth) = match anchor.split_once('#') {
        Some((a, n)) => {
            let parsed = n
                .parse::<u64>()
                .map_err(|_| HappensBeforeError::DslSyntax {
                    line: lineno,
                    message: format!("bad ordinal '#{}'", n),
                })?;
            (a.trim(), Some(parsed))
        }
        None => (anchor, None),
    };

    let mut ev = EventSpec {
        thread: thread.to_string(),
        nth,
        ..Default::default()
    };

    if let Some(rest) = anchor_body.strip_prefix('@') {
        // raw rip: @0x...
        ev.rip = Some(rest.to_string());
    } else if let Some(m) = anchor_body.strip_prefix("rcb=") {
        ev.rcbs = Some(
            m.parse::<u64>()
                .map_err(|_| HappensBeforeError::DslSyntax {
                    line: lineno,
                    message: format!("bad rcb count '{}'", m),
                })?,
        );
        ev.nth = None;
    } else if let Some(n) = anchor_body.strip_prefix("sc=") {
        ev.syscalls = Some(
            n.parse::<u64>()
                .map_err(|_| HappensBeforeError::DslSyntax {
                    line: lineno,
                    message: format!("bad syscall count '{}'", n),
                })?,
        );
        ev.nth = None;
    } else if let Some((sc, phase)) = anchor_body.split_once('@') {
        // syscall@phase
        ev.syscall = Some(sc.to_string());
        ev.phase = Some(parse_dsl_phase(phase, lineno)?);
    } else if is_syscall_name(anchor_body) {
        // bare syscall name
        ev.syscall = Some(anchor_body.to_string());
    } else {
        // function name (code location)
        ev.func = Some(anchor_body.to_string());
    }

    // Generate a stable, unique event name from the token.
    let base = sanitize_name(token);
    let mut ev_name = base.clone();
    let mut suffix = 1;
    while seen_names.contains(&ev_name) && spec.events.get(&ev_name) != Some(&ev) {
        suffix += 1;
        ev_name = format!("{}_{}", base, suffix);
    }
    seen_names.insert(ev_name.clone());
    spec.events.entry(ev_name.clone()).or_insert(ev);

    // Register the thread label if not already present and not a raw id.
    if thread.parse::<i32>().is_err() {
        spec.threads
            .entry(thread.to_string())
            .or_insert(ThreadSpec {
                label: Some(thread.to_string()),
                dettid: None,
                spawn_ordinal: None,
            });
    }

    Ok(ev_name)
}

fn parse_dsl_phase(phase: &str, lineno: usize) -> Result<PhaseSpec, HappensBeforeError> {
    match phase.trim().to_ascii_lowercase().as_str() {
        "pre" | "prehook" => Ok(PhaseSpec::Prehook),
        "post" | "posthook" => Ok(PhaseSpec::Posthook),
        "poll" | "polling" => Ok(PhaseSpec::Polling),
        other => Err(HappensBeforeError::DslSyntax {
            line: lineno,
            message: format!("unknown syscall phase '{}'", other),
        }),
    }
}

/// True when the token parses as a known syscall name.
fn is_syscall_name(s: &str) -> bool {
    Sysno::from_str(s).is_ok()
}

/// Turn a DSL token into a valid, readable event-name slug.
fn sanitize_name(token: &str) -> String {
    let mut out = String::with_capacity(token.len());
    for ch in token.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        out.push('e');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec_json() -> &'static str {
        r#"{
          "version": 1,
          "threads": { "writer": {"label": "writer"}, "reader": {"label": "reader"} },
          "events": {
            "X_342": {"thread": "writer", "func": "free_buffer", "line": 120, "nth": 342},
            "Y_97":  {"thread": "reader", "func": "read_buffer", "nth": 97},
            "lockA":  {"thread": "writer", "syscall": "futex", "phase": "posthook", "nth": 5},
            "storeB": {"thread": "reader", "rip": "0x401f3c", "nth": 1},
            "scA":    {"thread": "writer", "syscalls": 10},
            "rcbB":   {"thread": "reader", "rcbs": 123456}
          },
          "edges": [
            {"before": "X_342", "after": "Y_97", "strength": "hard"},
            {"before": "lockA", "after": "storeB"},
            {"before": "scA", "after": "rcbB", "strength": "soft"}
          ]
        }"#
    }

    /// The guest-side checkpoint filter is exact per thread: true for the named
    /// dettid at exactly its anchored count, false for another dettid or for
    /// count +/- 1 or at count 0, true for every thread at a spawn-ordinal
    /// anchor's count (the guest side cannot resolve ordinals), and false at
    /// numbers that belong to other position kinds (an RCB value, a
    /// syscall-by-name occurrence).
    #[test]
    fn may_have_syscall_count_anchor_at_is_exact_per_thread() {
        let prog = HappensBeforeSpec::from_json(
            r#"{"version": 1,
                "threads": {"T7": {"dettid": 7}, "S2": {"spawn_ordinal": 2}},
                "events": {"a": {"thread": "T7", "syscalls": 10},
                           "b": {"thread": "S2", "syscalls": 20},
                           "c": {"thread": "T7", "rcbs": 30},
                           "d": {"thread": "T7", "syscall": "futex", "phase": "pre", "nth": 40}},
                "edges": [{"before": "a", "after": "b"}]}"#,
        )
        .unwrap()
        .normalize()
        .unwrap();
        assert!(prog.may_have_syscall_count_anchor_at(DetTid::from_raw(7), 10));
        assert!(!prog.may_have_syscall_count_anchor_at(DetTid::from_raw(8), 10));
        assert!(!prog.may_have_syscall_count_anchor_at(DetTid::from_raw(7), 9));
        assert!(!prog.may_have_syscall_count_anchor_at(DetTid::from_raw(7), 11));
        assert!(prog.may_have_syscall_count_anchor_at(DetTid::from_raw(7), 20));
        assert!(prog.may_have_syscall_count_anchor_at(DetTid::from_raw(8), 20));
        assert!(!prog.may_have_syscall_count_anchor_at(DetTid::from_raw(8), 21));
        assert!(!prog.may_have_syscall_count_anchor_at(DetTid::from_raw(7), 30));
        assert!(!prog.may_have_syscall_count_anchor_at(DetTid::from_raw(7), 0));
        assert!(matches!(
            prog.anchors["d"].position,
            Position::Syscall { nth: 40, .. }
        ));
        assert!(!prog.may_have_syscall_count_anchor_at(DetTid::from_raw(7), 40));
    }

    /// `{"syscall": "writev", "fd": 9, "nth": 3}` is the thread's third
    /// `writev` to fd 9: a syscall-occurrence anchor that needs no calibration.
    #[test]
    fn syscall_fd_anchor_normalizes_and_displays() {
        let prog = HappensBeforeSpec::from_json(
            r#"{"version": 1,
                "events": {"m": {"thread": "7", "syscall": "writev", "fd": 9, "nth": 3}}}"#,
        )
        .unwrap()
        .normalize()
        .unwrap();
        let anchor = &prog.anchors["m"];
        assert_eq!(
            anchor.position,
            Position::Syscall {
                sysno: Sysno::writev,
                phase: None,
                nth: 3,
                fd: Some(9),
                futex_op: None,
            }
        );
        assert_eq!(anchor.position.to_string(), "writev(fd=9)#3");
        assert!(anchor.is_syscall_occurrence());
        assert_eq!(prog.unenforced_positions().count(), 0);
    }

    /// `fd` restricts a named syscall whose first argument is a descriptor; set
    /// anywhere else it is refused by name when the spec is loaded, never
    /// matched against an argument that is not a descriptor.
    #[test]
    fn syscall_fd_anchor_is_refused_where_it_cannot_apply() {
        let err = HappensBeforeSpec::from_json(
            r#"{"version": 1, "events": {"x": {"thread": "7", "syscalls": 5, "fd": 9}}}"#,
        )
        .unwrap()
        .normalize()
        .unwrap_err();
        assert_eq!(
            err,
            HappensBeforeError::FdWithoutSyscall {
                event: "x".to_string()
            }
        );
        let err = HappensBeforeSpec::from_json(
            r#"{"version": 1, "events": {"y": {"thread": "7", "syscall": "getpid", "fd": 9}}}"#,
        )
        .unwrap()
        .normalize()
        .unwrap_err();
        assert_eq!(
            err,
            HappensBeforeError::FdNotFirstArgument {
                event: "y".to_string(),
                syscall: "getpid".to_string()
            }
        );
        assert!(err.to_string().contains("'y'"), "{err}");
        let err = HappensBeforeSpec::from_json(
            r#"{"version": 1, "events": {"z": {"thread": "7", "syscall": "write", "fd": 1, "nth": 0}}}"#,
        )
        .unwrap()
        .normalize()
        .unwrap_err();
        assert_eq!(
            err,
            HappensBeforeError::NthZero {
                event: "z".to_string()
            }
        );
    }

    /// Occurrences are counted per thread over the calls that match: the
    /// anchor's syscall and, when it has one, its fd (the register's low 32
    /// bits). The second matching `writev` to fd 9 reaches an `nth: 2` anchor
    /// even with non-matching calls in between, an fd-agnostic anchor counts
    /// every fd, a spawn-ordinal anchor is returned on any thread (the
    /// scheduler resolves it), and a posthook anchor is never counted and is
    /// reported as unenforced.
    #[test]
    fn count_syscall_occurrences_is_exact_per_thread() {
        let prog = HappensBeforeSpec::from_json(
            r#"{"version": 1,
                "threads": {"S1": {"spawn_ordinal": 1}},
                "events": {"fd9": {"thread": "7", "syscall": "writev", "fd": 9, "nth": 2},
                           "anyfd": {"thread": "7", "syscall": "writev", "nth": 3},
                           "spawned": {"thread": "S1", "syscall": "write", "fd": 1},
                           "polling": {"thread": "7", "syscall": "close", "phase": "polling"}}}"#,
        )
        .unwrap()
        .normalize()
        .unwrap();
        assert!(prog.has_syscall_occurrence_anchors());
        let t7 = DetTid::from_raw(7);
        let t8 = DetTid::from_raw(8);
        let mut c7 = HbThreadCounters::default();
        let count = |c: &mut HbThreadCounters, t, sysno, arg0: usize| {
            prog.count_syscall_occurrences(c, t, sysno, [arg0, 0], 1)
        };
        assert!(count(&mut c7, t7, Sysno::writev, 9).is_empty());
        // Another fd and another syscall do not count toward fd9.
        assert!(count(&mut c7, t7, Sysno::writev, 8).is_empty());
        assert!(count(&mut c7, t7, Sysno::write, 9).is_empty());
        // Low 32 bits: this is fd 9, the second, and the third writev of any fd.
        assert_eq!(
            count(&mut c7, t7, Sysno::writev, 0x1_0000_0009),
            ["anyfd", "fd9"]
        );
        assert!(count(&mut c7, t7, Sysno::writev, 9).is_empty());
        // Thread 8 has its own counters; anchors on thread 7 are never its.
        let mut c8 = HbThreadCounters::default();
        assert!(count(&mut c8, t8, Sysno::writev, 9).is_empty());
        assert!(count(&mut c8, t8, Sysno::writev, 9).is_empty());
        // A spawn-ordinal anchor is returned on any thread at its nth.
        assert_eq!(count(&mut c8, t8, Sysno::write, 1), ["spawned"]);
        assert!(count(&mut c8, t8, Sysno::write, 2).is_empty());
        // A polling anchor is not enforced, never counted.
        assert!(count(&mut c7, t7, Sysno::close, 3).is_empty());
        assert!(!c7.counts.contains_key(&(Sysno::close as usize, None, None)));
        let unenforced: Vec<&str> = prog
            .unenforced_positions()
            .map(|a| a.name.as_str())
            .collect();
        assert_eq!(unenforced, ["polling"]);
        // A count anchor is not a syscall-occurrence anchor and is unaffected.
        assert!(!prog.may_have_syscall_count_anchor_at(t7, 2));
        let count_only = HappensBeforeSpec::from_json(
            r#"{"version": 1, "events": {"a": {"thread": "7", "syscalls": 2}}}"#,
        )
        .unwrap()
        .normalize()
        .unwrap();
        assert!(!count_only.has_syscall_occurrence_anchors());
    }

    #[test]
    fn parse_and_normalize_rfc_example() {
        let spec = HappensBeforeSpec::from_json(spec_json()).unwrap();
        let prog = spec.normalize().unwrap();
        assert_eq!(prog.anchors.len(), 6);
        assert_eq!(prog.edges.len(), 3);

        // Owner-primary positions.
        assert_eq!(prog.anchors["scA"].position, Position::SyscallCount(10));
        assert_eq!(prog.anchors["rcbB"].position, Position::Rcb(123456));

        // Function+line becomes an unresolved RIP with a code location attached.
        match &prog.anchors["X_342"].position {
            Position::Rip { addr: None, nth } => assert_eq!(*nth, 342),
            other => panic!("expected unresolved RIP, got {:?}", other),
        }
        assert_eq!(
            prog.anchors["X_342"].location.function.as_deref(),
            Some("free_buffer")
        );
        assert_eq!(prog.anchors["X_342"].location.line, Some(120));

        // Syscall anchor parses the name and phase.
        match &prog.anchors["lockA"].position {
            Position::Syscall {
                sysno,
                phase,
                nth,
                fd: None,
                futex_op: None,
            } => {
                assert_eq!(*sysno, Sysno::futex);
                assert_eq!(*phase, Some(SyscallPhase::Posthook));
                assert_eq!(*nth, 5);
            }
            other => panic!("expected syscall, got {:?}", other),
        }

        // RIP anchor.
        assert_eq!(
            prog.anchors["storeB"].position,
            Position::Rip {
                addr: Some(0x401f3c),
                nth: 1
            }
        );

        // Soft strength preserved.
        assert_eq!(prog.edges[2].strength, Strength::Soft);
        // Default strength is hard.
        assert_eq!(prog.edges[1].strength, Strength::Hard);

        // One anchor needs debug-info resolution (X_342, Y_97).
        assert_eq!(prog.unresolved_locations().count(), 2);
    }

    #[test]
    fn round_trip_json() {
        let spec = HappensBeforeSpec::from_json(spec_json()).unwrap();
        let json = spec.to_json().unwrap();
        let spec2 = HappensBeforeSpec::from_json(&json).unwrap();
        assert_eq!(spec, spec2);
    }

    #[test]
    fn rejects_wrong_version() {
        let spec = HappensBeforeSpec {
            version: 999,
            ..HappensBeforeSpec::from_json(spec_json()).unwrap()
        };
        assert_eq!(
            spec.normalize().unwrap_err(),
            HappensBeforeError::UnsupportedVersion(999)
        );
    }

    #[test]
    fn rejects_ambiguous_position() {
        let json = r#"{
          "version": 1,
          "events": { "bad": {"thread": "1", "syscalls": 3, "rcbs": 5} },
          "edges": []
        }"#;
        let spec = HappensBeforeSpec::from_json(json).unwrap();
        match spec.normalize().unwrap_err() {
            HappensBeforeError::AmbiguousPosition { event, found } => {
                assert_eq!(event, "bad");
                assert_eq!(found.len(), 2);
            }
            other => panic!("expected AmbiguousPosition, got {:?}", other),
        }
    }

    #[test]
    fn rejects_no_position() {
        let json = r#"{
          "version": 1,
          "events": { "bad": {"thread": "1"} },
          "edges": []
        }"#;
        let spec = HappensBeforeSpec::from_json(json).unwrap();
        assert!(matches!(
            spec.normalize().unwrap_err(),
            HappensBeforeError::AmbiguousPosition { .. }
        ));
    }

    #[test]
    fn code_location_accompanies_count() {
        // The owner's primary anchor: "function foo (line L) on thread T after
        // N syscalls / M RBCs". The code location is descriptive and the count
        // is the enforced position; the two must coexist, not conflict.
        let json = r#"{
          "version": 1,
          "events": {
            "w": {"thread": "1", "func": "free_buffer", "line": 342, "syscalls": 7},
            "r": {"thread": "1", "func": "read_buffer", "rcbs": 900}
          },
          "edges": [ {"before": "w", "after": "r"} ]
        }"#;
        let prog = HappensBeforeSpec::from_json(json)
            .unwrap()
            .normalize()
            .unwrap();

        // The count wins as the position; the code location is retained.
        assert_eq!(prog.anchors["w"].position, Position::SyscallCount(7));
        assert_eq!(
            prog.anchors["w"].location.function.as_deref(),
            Some("free_buffer")
        );
        assert_eq!(prog.anchors["w"].location.line, Some(342));

        assert_eq!(prog.anchors["r"].position, Position::Rcb(900));
        assert_eq!(
            prog.anchors["r"].location.function.as_deref(),
            Some("read_buffer")
        );

        // A descriptive-only code location is not an unresolved RIP position.
        assert_eq!(prog.unresolved_locations().count(), 0);
    }

    #[test]
    fn raw_dettid_thread() {
        let json = r#"{
          "version": 1,
          "events": { "e": {"thread": "42", "rcbs": 7} },
          "edges": []
        }"#;
        let prog = HappensBeforeSpec::from_json(json)
            .unwrap()
            .normalize()
            .unwrap();
        assert_eq!(prog.anchors["e"].thread.dettid, Some(DetTid::from_raw(42)));
    }

    #[test]
    fn rejects_unknown_thread() {
        let json = r#"{
          "version": 1,
          "events": { "e": {"thread": "ghost", "rcbs": 7} },
          "edges": []
        }"#;
        let spec = HappensBeforeSpec::from_json(json).unwrap();
        assert!(matches!(
            spec.normalize().unwrap_err(),
            HappensBeforeError::UnknownThread { .. }
        ));
    }

    #[test]
    fn rejects_unknown_event_in_edge() {
        let json = r#"{
          "version": 1,
          "events": { "a": {"thread": "1", "rcbs": 7} },
          "edges": [ {"before": "a", "after": "missing"} ]
        }"#;
        let spec = HappensBeforeSpec::from_json(json).unwrap();
        assert!(matches!(
            spec.normalize().unwrap_err(),
            HappensBeforeError::UnknownEvent { .. }
        ));
    }

    #[test]
    fn rejects_unknown_syscall() {
        let json = r#"{
          "version": 1,
          "events": { "a": {"thread": "1", "syscall": "not_a_syscall"} },
          "edges": []
        }"#;
        let spec = HappensBeforeSpec::from_json(json).unwrap();
        assert!(matches!(
            spec.normalize().unwrap_err(),
            HappensBeforeError::UnknownSyscall { .. }
        ));
    }

    #[test]
    fn detects_cycle() {
        let json = r#"{
          "version": 1,
          "events": {
            "a": {"thread": "1", "rcbs": 1},
            "b": {"thread": "1", "rcbs": 2},
            "c": {"thread": "1", "rcbs": 3}
          },
          "edges": [
            {"before": "a", "after": "b"},
            {"before": "b", "after": "c"},
            {"before": "c", "after": "a"}
          ]
        }"#;
        let spec = HappensBeforeSpec::from_json(json).unwrap();
        match spec.normalize().unwrap_err() {
            HappensBeforeError::Cycle(path) => {
                // Path forms a closed loop.
                assert_eq!(path.first(), path.last());
                assert!(path.len() >= 4);
            }
            other => panic!("expected Cycle, got {:?}", other),
        }
    }

    #[test]
    fn accepts_dag() {
        let json = r#"{
          "version": 1,
          "events": {
            "a": {"thread": "1", "rcbs": 1},
            "b": {"thread": "1", "rcbs": 2},
            "c": {"thread": "1", "rcbs": 3}
          },
          "edges": [
            {"before": "a", "after": "c"},
            {"before": "b", "after": "c"}
          ]
        }"#;
        let spec = HappensBeforeSpec::from_json(json).unwrap();
        assert!(spec.normalize().is_ok());
    }

    #[test]
    fn dsl_desugars() {
        let dsl = "\
            // btrfs race: erase-by-key must precede the re-insert
            writer:free_buffer#342  <  reader:read_buffer#97
            writer:futex@post#5     <  reader:@0x401f3c#1
            A:rcb=123456            <  B:sc=97   !soft
        ";
        let spec = HappensBeforeSpec::from_dsl(dsl).unwrap();
        let prog = spec.normalize().unwrap();
        assert_eq!(prog.edges.len(), 3);
        assert_eq!(prog.anchors.len(), 6);

        // The rcb/sc line desugars to owner-primary positions and is soft.
        let soft = &prog.edges[2];
        assert_eq!(soft.strength, Strength::Soft);
        assert_eq!(prog.anchors[&soft.before].position, Position::Rcb(123456));
        assert_eq!(
            prog.anchors[&soft.after].position,
            Position::SyscallCount(97)
        );

        // futex@post#5 desugars to a phase-qualified syscall.
        let futex = prog
            .anchors
            .values()
            .find(
                |a| matches!(a.position, Position::Syscall { sysno, .. } if sysno == Sysno::futex),
            )
            .unwrap();
        match &futex.position {
            Position::Syscall { phase, nth, .. } => {
                assert_eq!(*phase, Some(SyscallPhase::Posthook));
                assert_eq!(*nth, 5);
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn dsl_rejects_missing_arrow() {
        let err = HappensBeforeSpec::from_dsl("writer:foo reader:bar").unwrap_err();
        assert!(matches!(err, HappensBeforeError::DslSyntax { line: 1, .. }));
    }

    #[test]
    fn parse_rip_forms() {
        assert_eq!(parse_rip("0x401f3c"), Some(0x401f3c));
        assert_eq!(parse_rip("4201276"), Some(4201276));
        assert_eq!(parse_rip("nonsense"), None);
    }

    fn normalize_json(json: &str) -> Result<HappensBeforeProgram, HappensBeforeError> {
        HappensBeforeSpec::from_json(json).unwrap().normalize()
    }

    /// Version 2 is a new schema: a version-1 spec loads with its old meaning
    /// and no hold budget, but may not use a version-2 field; a version-2 spec
    /// gets the default budget; any other version is refused. The DSL has no
    /// version-2 syntax and builds version 1.
    #[test]
    fn version_2_fields_need_version_2() {
        let v1 = normalize_json(
            r#"{"version": 1, "events": {"w": {"thread": "7", "syscall": "write", "fd": 1}}}"#,
        )
        .unwrap();
        assert_eq!((v1.version, v1.max_hold_ns), (1, None));
        assert!(!v1.is_version_2());
        let v2 = normalize_json(
            r#"{"version": 2, "events": {"w": {"thread": "7", "syscall": "write", "fd": 1}}}"#,
        )
        .unwrap();
        assert_eq!(v2.max_hold_ns, Some(DEFAULT_MAX_HOLD_NS));
        assert!(v2.is_version_2());
        let budget = normalize_json(
            r#"{"version": 2, "max_hold_ns": 5000,
                "events": {"w": {"thread": "7", "syscall": "write", "fd": 1}}}"#,
        )
        .unwrap();
        assert_eq!(budget.max_hold_ns, Some(5000));

        for (json, event, field) in [
            (
                r#"{"version": 1, "events": {
                    "a": {"thread": "7", "syscall": "write", "fd": 1},
                    "r": {"thread": "7", "syscall": "getppid", "from": "a"}}}"#,
                Some("r"),
                "from",
            ),
            (
                r#"{"version": 1, "events": {
                    "f": {"thread": "7", "syscall": "futex", "futex_op": 393}}}"#,
                Some("f"),
                "futex_op",
            ),
            (
                r#"{"version": 1, "max_hold_ns": 5,
                    "events": {"w": {"thread": "7", "syscall": "write"}}}"#,
                None,
                "max_hold_ns",
            ),
        ] {
            assert_eq!(
                normalize_json(json).unwrap_err(),
                HappensBeforeError::NewFieldInVersion1 {
                    event: event.map(str::to_owned),
                    field: field.to_owned(),
                }
            );
        }
        assert_eq!(
            normalize_json(r#"{"version": 3, "events": {}}"#).unwrap_err(),
            HappensBeforeError::UnsupportedVersion(3)
        );
        let dsl = HappensBeforeSpec::from_dsl("A:sc=3 < B:sc=4").unwrap();
        assert_eq!(dsl.version, HAPPENS_BEFORE_VERSION_1);
    }

    /// A version-2 spec refuses a field this build does not know, at every
    /// level, so a misspelling is not silently ignored; version 1 keeps its
    /// lenient parsing.
    #[test]
    fn version_2_refuses_unknown_fields() {
        for (json, place, field) in [
            (r#"{"version": 2, "evnets": {}}"#, "the spec", "evnets"),
            (
                r#"{"version": 2, "events": {"w": {"thread": "7", "syscal": "write"}}}"#,
                "event 'w'",
                "syscal",
            ),
            (
                r#"{"version": 2, "threads": {"t": {"dettid": 7, "tid": 7}}}"#,
                "thread 't'",
                "tid",
            ),
            (
                r#"{"version": 2, "events": {}, "edges": [{"before": "a", "after": "b", "kind": 1}]}"#,
                "edge 0",
                "kind",
            ),
        ] {
            let error = HappensBeforeSpec::from_json(json).unwrap_err();
            assert_eq!(
                error.downcast_ref::<HappensBeforeError>(),
                Some(&HappensBeforeError::UnknownField {
                    place: place.to_owned(),
                    field: field.to_owned(),
                }),
                "{json}"
            );
        }
        // The `rcb` alias is a known field.
        normalize_json(r#"{"version": 2, "events": {"r": {"thread": "7", "rcb": 5}}}"#).unwrap();
        // Version 1 ignores an unknown field, as it always has.
        normalize_json(r#"{"version": 1, "events": {}, "evnets": {}}"#).unwrap();
    }

    /// A field given twice is refused in both versions, rather than the last
    /// value silently winning (the Codex review of
    /// https://github.com/rrnewton/hermit/pull/4004).
    #[test]
    fn a_duplicated_field_is_refused() {
        for json in [
            r#"{"version": 1, "events": {"w": {"thread": "7", "syscall": "write", "nth": 1, "nth": 2}}}"#,
            r#"{"version": 2, "events": {"w": {"thread": "7", "syscall": "write", "nth": 1, "nth": 2}}}"#,
            r#"{"version": 2, "max_hold_ns": 5, "max_hold_ns": 6, "events": {}}"#,
        ] {
            let error = HappensBeforeSpec::from_json(json).unwrap_err().to_string();
            assert!(error.contains("duplicate field"), "{json}: {error}");
        }
    }

    /// A posthook syscall anchor is enforced: it is counted at its entry like
    /// any occurrence anchor and named at that entry, and only its firing
    /// point differs (https://github.com/rrnewton/hermit/issues/3929). It is
    /// refused at load on syscalls that do not return on success.
    #[test]
    fn a_posthook_anchor_is_counted_at_entry_and_refused_on_no_return_syscalls() {
        let prog = normalize_json(
            r#"{"version": 1, "events": {
                "w": {"thread": "7", "syscall": "write", "fd": 1, "phase": "posthook", "nth": 2}}}"#,
        )
        .unwrap();
        let w = &prog.anchors["w"];
        assert!(w.is_syscall_occurrence() && w.fires_at_posthook());
        assert_eq!(prog.unenforced_positions().count(), 0);
        let t7 = DetTid::from_raw(7);
        let mut c = HbThreadCounters::default();
        assert!(
            prog.count_syscall_occurrences(&mut c, t7, Sysno::write, [1, 0], 1)
                .is_empty()
        );
        assert_eq!(
            prog.count_syscall_occurrences(&mut c, t7, Sysno::write, [1, 0], 2),
            ["w"]
        );
        for syscall in ["execve", "execveat", "exit", "exit_group", "rt_sigreturn"] {
            assert_eq!(
                normalize_json(&format!(
                    r#"{{"version": 1, "events": {{"x": {{"thread": "7", "syscall": "{syscall}", "phase": "posthook"}}}}}}"#
                ))
                .unwrap_err(),
                HappensBeforeError::PosthookOnNoReturnSyscall {
                    event: "x".to_owned(),
                    syscall: syscall.to_owned()
                }
            );
        }
        // At the prehook the same syscalls stay valid anchors.
        normalize_json(r#"{"version": 1, "events": {"x": {"thread": "7", "syscall": "execve"}}}"#)
            .unwrap();
    }

    /// `futex_op` restricts `futex` to one operation, compared on the low 32
    /// bits of the second argument as the kernel's `int`; it is refused on any
    /// other syscall and outside `i32`.
    #[test]
    fn futex_op_matches_the_low_32_bits_of_the_operation() {
        let prog = normalize_json(
            r#"{"version": 2, "events": {
                "halt": {"thread": "7", "syscall": "futex", "futex_op": 393, "nth": 2},
                "wait": {"thread": "7", "syscall": "futex", "futex_op": 0}}}"#,
        )
        .unwrap();
        assert_eq!(prog.anchors["halt"].position.to_string(), "futex(op=393)#2");
        let t7 = DetTid::from_raw(7);
        let mut c = HbThreadCounters::default();
        let mut entry = 0;
        let mut call = |c: &mut HbThreadCounters, op: usize| {
            entry += 1;
            prog.count_syscall_occurrences(c, t7, Sysno::futex, [0x1000, op], entry)
        };
        assert_eq!(call(&mut c, 393), Vec::<String>::new());
        // Operation 0 counts only toward its own matcher.
        assert_eq!(call(&mut c, 0), ["wait"]);
        // The same operation with other upper register bits is operation 393.
        assert_eq!(call(&mut c, 0xdead_0000_0000_0189), ["halt"]);
        assert_eq!(c.counts[&(Sysno::futex as usize, None, Some(393))], 2);
        assert_eq!(c.counts[&(Sysno::futex as usize, None, Some(0))], 1);

        assert_eq!(
            normalize_json(
                r#"{"version": 2, "events": {"w": {"thread": "7", "syscall": "write", "futex_op": 1}}}"#,
            )
            .unwrap_err(),
            HappensBeforeError::FutexOpWithoutFutex {
                event: "w".to_owned()
            }
        );
        assert_eq!(
            normalize_json(
                r#"{"version": 2, "events": {"w": {"thread": "7", "futex_op": 1, "syscalls": 3}}}"#
            )
            .unwrap_err(),
            HappensBeforeError::FutexOpWithoutFutex {
                event: "w".to_owned()
            }
        );
        assert_eq!(
            normalize_json(
                r#"{"version": 2, "events": {"f": {"thread": "7", "syscall": "futex", "futex_op": 2147483648}}}"#,
            )
            .unwrap_err(),
            HappensBeforeError::FutexOpOutOfRange {
                event: "f".to_owned(),
                value: 2147483648
            }
        );
    }

    /// Each malformed `from` is refused by name when the spec is loaded.
    #[test]
    fn from_refusals_name_the_event() {
        let refused = |events: &str, edges: &str| {
            normalize_json(&format!(
                r#"{{"version": 2, "events": {{{events}}}, "edges": [{edges}]}}"#
            ))
            .unwrap_err()
        };
        let names = |e: &str, b: &str| (e.to_owned(), b.to_owned());
        let (event, base) = names("r", "nope");
        assert_eq!(
            refused(
                r#""r": {"thread": "7", "syscall": "getppid", "from": "nope"}"#,
                ""
            ),
            HappensBeforeError::FromUnknownEvent { event, base }
        );
        let (event, base) = names("r", "a");
        assert_eq!(
            refused(
                r#""a": {"thread": "8", "syscall": "write", "fd": 1},
                   "r": {"thread": "7", "syscall": "getppid", "from": "a"}"#,
                ""
            ),
            HappensBeforeError::FromOtherThread { event, base }
        );
        let (event, base) = names("r", "a");
        assert_eq!(
            refused(
                r#""a": {"thread": "7", "rcbs": 100},
                   "r": {"thread": "7", "syscall": "getppid", "from": "a"}"#,
                ""
            ),
            HappensBeforeError::FromUnenforcedPosition { event, base }
        );
        let (event, base) = names("r", "a");
        assert_eq!(
            refused(
                r#""a": {"thread": "7", "syscall": "write", "phase": "polling"},
                   "r": {"thread": "7", "syscall": "getppid", "from": "a"}"#,
                ""
            ),
            HappensBeforeError::FromUnenforcedPosition { event, base }
        );
        let (event, base) = names("r2", "r1");
        assert_eq!(
            refused(
                r#""a": {"thread": "7", "syscalls": 3},
                   "r1": {"thread": "7", "syscall": "getppid", "from": "a"},
                   "r2": {"thread": "7", "syscall": "getppid", "from": "r1"}"#,
                ""
            ),
            HappensBeforeError::FromRelativeBase { event, base }
        );
        assert_eq!(
            refused(
                r#""a": {"thread": "7", "syscalls": 3},
                   "r": {"thread": "7", "syscalls": 9, "from": "a"}"#,
                ""
            ),
            HappensBeforeError::FromOnNonOccurrence {
                event: "r".to_owned()
            }
        );
        // R counts from A, so R comes after A on that thread; a hard edge
        // R before A would need A after R.
        assert_eq!(
            refused(
                r#""a": {"thread": "7", "syscalls": 3},
                   "r": {"thread": "7", "syscall": "getppid", "from": "a"}"#,
                r#"{"before": "r", "after": "a"}"#
            ),
            HappensBeforeError::FromCycle(vec!["a".to_owned(), "r".to_owned(), "a".to_owned()])
        );
        // One thread named two ways (a label for dettid 3, and "3") is the
        // same thread; a dettid and a spawn ordinal cannot be shown equal.
        normalize_json(
            r#"{"version": 2, "threads": {"main": {"dettid": 3}, "s1": {"spawn_ordinal": 1}},
                "events": {"a": {"thread": "main", "syscall": "write", "fd": 1},
                           "r": {"thread": "3", "syscall": "getppid", "from": "a"},
                           "b": {"thread": "s1", "syscalls": 4},
                           "q": {"thread": "s1", "syscall": "getppid", "from": "b"}}}"#,
        )
        .unwrap();
        let (event, base) = names("r", "a");
        assert_eq!(
            normalize_json(
                r#"{"version": 2, "threads": {"s1": {"spawn_ordinal": 1}},
                    "events": {"a": {"thread": "7", "syscall": "write", "fd": 1},
                               "r": {"thread": "s1", "syscall": "getppid", "from": "a"}}}"#,
            )
            .unwrap_err(),
            HappensBeforeError::FromOtherThread { event, base }
        );
        // A soft edge does not hold a thread, so it closes no cycle.
        normalize_json(
            r#"{"version": 2, "events": {
                "a": {"thread": "7", "syscalls": 3},
                "r": {"thread": "7", "syscall": "getppid", "from": "a"}},
                "edges": [{"before": "r", "after": "a", "strength": "soft"}]}"#,
        )
        .unwrap();
    }

    /// The design's per-entry order: increment, then record origins, then
    /// fire. Absolute and relative anchors on one matcher share its counter;
    /// each relative anchor has its own origin; a base entry that matches the
    /// relative matcher is not in the window; a count base resolves on an
    /// entry that matches no key.
    #[test]
    fn relative_anchors_count_strictly_after_their_base() {
        let prog = normalize_json(
            r#"{"version": 2, "events": {
                "m2": {"thread": "7", "syscall": "write", "fd": 1, "nth": 2},
                "abs3": {"thread": "7", "syscall": "getppid", "nth": 3},
                "after_m2": {"thread": "7", "syscall": "getppid", "from": "m2", "nth": 1},
                "c4": {"thread": "7", "syscalls": 4},
                "after_c4": {"thread": "7", "syscall": "getppid", "from": "c4", "nth": 2},
                "gp1": {"thread": "7", "syscall": "getppid", "nth": 1},
                "after_gp1": {"thread": "7", "syscall": "getppid", "from": "gp1", "nth": 1}}}"#,
        )
        .unwrap();
        assert_eq!(
            prog.anchors["after_m2"].to_string(),
            "after_m2[7: getppid#1 after m2]"
        );
        let relative: Vec<(&str, &str)> = prog
            .relative_anchors()
            .map(|(r, b)| (r.name.as_str(), b.name.as_str()))
            .collect();
        assert_eq!(
            relative,
            [("after_c4", "c4"), ("after_gp1", "gp1"), ("after_m2", "m2")]
        );
        let t7 = DetTid::from_raw(7);
        let mut c = HbThreadCounters::default();
        let mut entry = 0;
        let mut call = |c: &mut HbThreadCounters, sysno: Sysno, arg0: usize| {
            entry += 1;
            prog.count_syscall_occurrences(c, t7, sysno, [arg0, 0], entry)
        };
        // Entry 1: getppid #1 fires gp1, which is after_gp1's base; that
        // entry is not in after_gp1's window.
        assert_eq!(call(&mut c, Sysno::getppid, 0), ["gp1"]);
        assert_eq!(c.origins["after_gp1"], 1);
        // Entry 2: write #1 to fd 1.
        assert!(call(&mut c, Sysno::write, 1).is_empty());
        // Entry 3: getppid #2, the first after gp1.
        assert_eq!(call(&mut c, Sysno::getppid, 0), ["after_gp1"]);
        // Entry 4: an unwatched syscall that matches no key is c4's base
        // entry; after_c4's origin is getppid's count so far.
        assert!(call(&mut c, Sysno::getpid, 0).is_empty());
        assert_eq!(c.origins["after_c4"], 2);
        // Entry 5: write #2 to fd 1 fires m2 and starts after_m2's window.
        assert_eq!(call(&mut c, Sysno::write, 1), ["m2"]);
        assert_eq!(c.origins["after_m2"], 2);
        // Entry 6: getppid #3 is abs3, the first after m2, and the first
        // after c4; one shared counter, independent origins.
        assert_eq!(call(&mut c, Sysno::getppid, 0), ["abs3", "after_m2"]);
        // Entry 7: getppid #4 is the second after c4.
        assert_eq!(call(&mut c, Sysno::getppid, 0), ["after_c4"]);
        assert!(call(&mut c, Sysno::getppid, 0).is_empty());
        assert_eq!(c.counts[&(Sysno::getppid as usize, None, None)], 5);

        // A new thread starts with no origins: the relative anchors on thread
        // 7 are never counted for thread 8.
        let mut c8 = HbThreadCounters::default();
        assert!(
            prog.count_syscall_occurrences(&mut c8, DetTid::from_raw(8), Sysno::getppid, [0, 0], 1)
                .is_empty()
        );
        assert!(c8.origins.is_empty());
    }

    /// The phase is not part of an anchor's counting key: a prehook and a
    /// posthook anchor on one matcher share one counter, and both are named at
    /// the same entry; a posthook base starts its relative anchor's window at
    /// its entry, as a prehook base does
    /// (https://github.com/rrnewton/hermit/issues/3929).
    #[test]
    fn a_posthook_anchor_shares_its_counter_and_starts_windows_at_its_entry() {
        let prog = normalize_json(
            r#"{"version": 2, "events": {
                "pre2": {"thread": "7", "syscall": "write", "fd": 1, "nth": 2},
                "post2": {"thread": "7", "syscall": "write", "fd": 1, "nth": 2,
                          "phase": "posthook"},
                "post3": {"thread": "7", "syscall": "write", "fd": 1, "nth": 3,
                          "phase": "posthook"},
                "base": {"thread": "7", "syscall": "getpid", "phase": "posthook"},
                "after_base": {"thread": "7", "syscall": "write", "fd": 1, "from": "base",
                               "nth": 1}}}"#,
        )
        .unwrap();
        let t7 = DetTid::from_raw(7);
        let mut c = HbThreadCounters::default();
        let mut entry = 0;
        let mut call = |c: &mut HbThreadCounters, sysno: Sysno, arg0: usize| {
            entry += 1;
            prog.count_syscall_occurrences(c, t7, sysno, [arg0, 0], entry)
        };
        assert!(call(&mut c, Sysno::write, 1).is_empty());
        assert_eq!(call(&mut c, Sysno::write, 1), ["post2", "pre2"]);
        // Entry 3: the posthook base, named at its entry; its window starts
        // at the write count so far.
        assert_eq!(call(&mut c, Sysno::getpid, 0), ["base"]);
        assert_eq!(c.origins["after_base"], 2);
        // Entry 4: write #3 is post3 and the first write after the base.
        assert_eq!(call(&mut c, Sysno::write, 1), ["after_base", "post3"]);
        assert_eq!(c.counts[&(Sysno::write as usize, Some(1), None)], 3);
    }
}
