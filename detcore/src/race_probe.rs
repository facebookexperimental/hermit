/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Race probes: env-gated perturbation points on Hermit's own host-timed
//! edges, for reproducing rare races in Hermit itself (docs/RACE_PROBES.md).
//!
//! Off unless [`RACE_PROBE_ENV`] is set to a non-empty list of
//! `point=LO[-HI][@P]` items. At `point`, with probability `P` (default 1), a
//! visit is perturbed: an async point waits a uniform `LO..=HI` milliseconds,
//! and a synchronous point takes its perturbed branch (its numbers ignored).
//! [`RACE_PROBE_SEED_ENV`] fixes the choices; without it a seed is drawn from
//! the host clock. Either way the configuration and seed are printed once to
//! stderr, so a failing run names its seed.
//!
//! `point=hold` makes every visit to an async point wait, without a time
//! limit, until a file named after the point exists in
//! [`RACE_PROBE_HOLD_DIR_ENV`]: a test can then act on a task while it is
//! known to be at the point, and release it after. A held visit prints one
//! line to stderr naming the file it waits for. `point=hold-each` holds
//! every visit the same way and removes the file as it lets the visit go,
//! so that each visit waits for a release of its own.
//!
//! Unset, every point is one read of an initialized `OnceLock` and a branch;
//! nothing is logged, and no tracing event is emitted.
//!
//! A probe only moves host timing that Hermit already treats as
//! nondeterministic input (when a backgrounded call returns, and when the
//! scheduler samples that it has), so an outcome reached under a probe is one
//! host timing alone could produce. Decisions are drawn in visit order from one
//! process-wide counter, so a seed replays exactly only when the number of
//! visits before the deciding one does not itself depend on host timing.

use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;

use crate::random::splitmix64;

/// The variable that enables race probes and lists their settings.
pub const RACE_PROBE_ENV: &str = "HERMIT_RACE_PROBE";

/// The variable that fixes the seed of the race probes' choices.
pub const RACE_PROBE_SEED_ENV: &str = "HERMIT_RACE_PROBE_SEED";

/// The directory whose files release held points (`point=hold`).
pub const RACE_PROBE_HOLD_DIR_ENV: &str = "HERMIT_RACE_PROBE_HOLD_DIR";

/// How often a held visit looks for its release file.
const HOLD_POLL: Duration = Duration::from_millis(10);

/// The file a hold in Hermit's launcher or a container's init needs in the
/// hold directory, beside the configuration ([`delay_in_launcher`]). Such a
/// hold parks the run itself, so an environment a process inherited is not
/// enough to arm it: the test that holds it makes this file.
pub const LAUNCHER_HOLD_MARKER: &str = "launcher-holds-armed";

/// How long a hold in Hermit's launcher or a container's init waits for its
/// release before it fails, naming the point.
const LAUNCHER_HOLD_LIMIT: Duration = Duration::from_secs(120);

/// An async wait just before a backgrounded blocking call runs.
pub(crate) const BG_PRE_CALL: &str = "bg_pre_call";
/// An async wait after a backgrounded call returns, before it rejoins.
pub(crate) const BG_POST_CALL: &str = "bg_post_call";
/// The scheduler treats a finished backgrounded call as still running on one
/// of its checks.
pub(crate) const BG_HARVEST_SKIP: &str = "bg_harvest_skip";
/// An async wait after a FIFO open's staging page is mapped, before anything
/// is written to it: a task killed by a host process then leaves the page
/// mapped and empty, as one killed during the `mmap` does.
pub(crate) const FIFO_STAGING_MAPPED: &str = "fifo_staging_mapped";
/// An async wait before a task unmaps a staging page that a killed task left:
/// a task killed by a host process then leaves the page to the next task.
pub(crate) const FIFO_STAGING_RECLAIM: &str = "fifo_staging_reclaim";
/// An async wait in `openat` after Detcore has named the directory a relative
/// lookup starts from, before the kernel's open: without sequentialized
/// threads another guest thread can replace that directory's descriptor in
/// between, as host timing alone can let it.
pub(crate) const PROCFS_SPELLING_RESOLVED: &str = "procfs_spelling_resolved";
/// A wait at the end of Detcore's thread exit hook, after its last call to
/// the global state and before the backend releases the thread's state, and
/// with it the descriptor table it may be the last to hold: the backend runs
/// that hook at no particular point of the guest's schedule (KVM after the
/// parent can already wait for the child), so the release comes late, as
/// host timing alone can make it. Only in builds with debug assertions (the
/// debug and validate profiles), as is [`RECEIPT_DECISION`]; both wait
/// through [`delay_on_any_backend`].
pub(crate) const EXIT_HOOK_TAIL: &str = "exit_hook_tail";
/// A wait in the recording of a descriptor received over `SCM_RIGHTS`,
/// before the receipt decides how to record it
/// (`Detcore::record_received_facts`): exited senders' exit hooks run
/// meanwhile, as they can while a receive's probes wait for the host.
pub(crate) const RECEIPT_DECISION: &str = "receipt_decision";
/// A wait in the container's init after its setup made the container's
/// mounts and before the init records the mount table
/// (`IdentityGuard::record_plan` in hermit-cli): a host process that renames
/// a new file or directory over a bind's target meanwhile detaches the bind,
/// as the host's own timing can, and the record then finds it gone. Waits
/// through [`delay_in_launcher`].
pub const SETUP_MOUNTS_MADE: &str = "setup_mounts_made";

/// Every probe point, for validating a configuration.
const POINTS: [&str; 9] = [
    BG_PRE_CALL,
    BG_POST_CALL,
    BG_HARVEST_SKIP,
    FIFO_STAGING_MAPPED,
    FIFO_STAGING_RECLAIM,
    PROCFS_SPELLING_RESOLVED,
    EXIT_HOOK_TAIL,
    RECEIPT_DECISION,
    SETUP_MOUNTS_MADE,
];

#[derive(Clone, Copy, Debug, PartialEq)]
struct Spec {
    lo_ms: u64,
    hi_ms: u64,
    probability: f64,
    /// Every visit waits for the point's release file (`point=hold`).
    hold: bool,
    /// The release is removed as each visit goes on (`point=hold-each`).
    each: bool,
}

#[derive(Debug)]
struct Probes {
    seed: u64,
    counter: AtomicU64,
    points: BTreeMap<String, Spec>,
    hold_dir: Option<PathBuf>,
}

/// What a perturbed visit does.
#[derive(Clone, Debug, PartialEq)]
enum Perturbation {
    /// An async point waits this long; a synchronous point takes its branch.
    Wait(Duration),
    /// An async point waits until this file exists.
    Hold(PathBuf),
    /// As [`Self::Hold`], removing the file as the visit goes on.
    HoldEach(PathBuf),
}

fn parse_item(item: &str) -> Result<(String, Spec), String> {
    let (name, rest) = item
        .split_once('=')
        .ok_or_else(|| format!("{item:?} is not point=LO[-HI][@P]"))?;
    if !POINTS.contains(&name) {
        return Err(format!("unknown point {name:?}; points are {POINTS:?}"));
    }
    if rest == "hold" || rest == "hold-each" {
        return Ok((
            name.to_owned(),
            Spec {
                lo_ms: 0,
                hi_ms: 0,
                probability: 1.0,
                hold: true,
                each: rest == "hold-each",
            },
        ));
    }
    let (range, probability) = match rest.split_once('@') {
        Some((range, p)) => (
            range,
            p.parse::<f64>()
                .ok()
                .filter(|p| (0.0..=1.0).contains(p))
                .ok_or_else(|| format!("{item:?}: probability must be in [0, 1]"))?,
        ),
        None => (rest, 1.0),
    };
    let number = |text: &str| {
        text.parse::<u64>()
            .map_err(|_| format!("{item:?}: {text:?} is not a number of milliseconds"))
    };
    let (lo_ms, hi_ms) = match range.split_once('-') {
        Some((lo, hi)) => (number(lo)?, number(hi)?),
        None => {
            let ms = number(range)?;
            (ms, ms)
        }
    };
    if hi_ms < lo_ms {
        return Err(format!("{item:?}: the range is empty"));
    }
    Ok((
        name.to_owned(),
        Spec {
            lo_ms,
            hi_ms,
            probability,
            hold: false,
            each: false,
        },
    ))
}

impl Probes {
    /// The probes `config`, `seed` and `hold_dir` (the three variables'
    /// values) describe: `None` when probes are off. A malformed item is
    /// reported and skipped, as is a hold without a hold directory.
    fn from_settings(
        config: Option<&str>,
        seed: Option<&str>,
        hold_dir: Option<&Path>,
    ) -> Option<Self> {
        let config = config?;
        let hold_dir = hold_dir
            .filter(|dir| !dir.as_os_str().is_empty())
            .map(Path::to_path_buf);
        // A disabled probe must not create randomized hash state: even an
        // empty HashMap initializes RandomState's thread-local random keys.
        // These few exact names need no hashing, and insert still makes the
        // last valid duplicate win without changing lookup or draw order.
        let mut points = BTreeMap::new();
        for item in config.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            match parse_item(item) {
                Ok((_, spec)) if spec.hold && hold_dir.is_none() => {
                    eprintln!(
                        "race_probe: ignoring {item:?}: {RACE_PROBE_HOLD_DIR_ENV} is not set"
                    );
                }
                Ok((name, spec)) => {
                    points.insert(name, spec);
                }
                Err(error) => eprintln!("race_probe: ignoring {error}"),
            }
        }
        if points.is_empty() {
            return None;
        }
        let seed = match seed.map(str::parse::<u64>) {
            Some(Ok(seed)) => seed,
            _ => {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_nanos() as u64)
                    ^ u64::from(std::process::id())
            }
        };
        Some(Probes {
            seed,
            counter: AtomicU64::new(0),
            points,
            hold_dir,
        })
    }

    /// The decision at `point` for this visit: `None` when the point is not
    /// configured or this visit is not perturbed, otherwise what it does.
    fn choose(&self, point: &str) -> Option<Perturbation> {
        let spec = self.points.get(point)?;
        if spec.hold {
            // Configured only with a hold directory (`from_settings`).
            let release = self.hold_dir.as_ref()?.join(point);
            return Some(if spec.each {
                Perturbation::HoldEach(release)
            } else {
                Perturbation::Hold(release)
            });
        }
        let visit = self.counter.fetch_add(1, Ordering::Relaxed);
        let r = splitmix64(self.seed ^ splitmix64(visit));
        let unit = (r >> 11) as f64 / (1u64 << 53) as f64;
        if unit >= spec.probability {
            return None;
        }
        let span = spec.hi_ms - spec.lo_ms;
        // One of `span + 1` waits, or of every u64 when the range is all of it.
        let offset = match span.checked_add(1) {
            Some(choices) => splitmix64(r) % choices,
            None => splitmix64(r),
        };
        Some(Perturbation::Wait(Duration::from_millis(spec.lo_ms + offset)))
    }
}

fn probes() -> Option<&'static Probes> {
    static PROBES: OnceLock<Option<Probes>> = OnceLock::new();
    PROBES
        .get_or_init(|| {
            let config = std::env::var(RACE_PROBE_ENV).ok()?;
            let seed = std::env::var(RACE_PROBE_SEED_ENV).ok();
            // The directory's bytes as the environment gives them.
            let hold_dir = std::env::var_os(RACE_PROBE_HOLD_DIR_ENV);
            let probes = Probes::from_settings(
                Some(&config),
                seed.as_deref(),
                hold_dir.as_deref().map(Path::new),
            )?;
            eprintln!(
                "race_probe: {RACE_PROBE_ENV}={config} {RACE_PROBE_SEED_ENV}={}",
                probes.seed
            );
            Some(probes)
        })
        .as_ref()
}

fn choose(point: &str) -> Option<Perturbation> {
    let perturbation = probes()?.choose(point)?;
    tracing::debug!(target: "race_probe", "race_probe: {point} perturbed ({perturbation:?})");
    Some(perturbation)
}

/// An async wait at `point`, when configured.
pub(crate) async fn delay(point: &str) {
    match choose(point) {
        Some(Perturbation::Wait(wait)) if !wait.is_zero() => tokio::time::sleep(wait).await,
        Some(Perturbation::Hold(release)) => hold(point, &release, false).await,
        Some(Perturbation::HoldEach(release)) => hold(point, &release, true).await,
        _ => {}
    }
}

/// A wait at `point`, when configured, in Detcore code that every backend
/// runs ([`EXIT_HOOK_TAIL`], [`RECEIPT_DECISION`]). Where the tool runs
/// inside the guest's process (`in_guest`: the in-guest backends), whose
/// threads must touch no thread-local state a runtime keeps, it blocks the
/// calling thread and touches none: the disabled path is only [`choose`]'s
/// read of an initialized `OnceLock`. Elsewhere it waits on Tokio's timer
/// where the calling task runs on a Tokio runtime (the ptrace tracer, whose
/// one thread serves every guest task), so that only that task waits, and
/// blocks the calling thread where none runs (KVM drives each guest
/// process's and thread's tool on a thread of its own), which then serves
/// that task alone.
#[cfg(debug_assertions)]
pub(crate) async fn delay_on_any_backend(point: &str, in_guest: bool) {
    let Some(perturbation) = choose(point) else {
        return;
    };
    let on_tokio = !in_guest && tokio::runtime::Handle::try_current().is_ok();
    match perturbation {
        Perturbation::Wait(wait) if wait.is_zero() => {}
        Perturbation::Wait(wait) if on_tokio => tokio::time::sleep(wait).await,
        Perturbation::Wait(wait) => std::thread::sleep(wait),
        Perturbation::Hold(release) if on_tokio => hold(point, &release, false).await,
        Perturbation::Hold(release) => hold_blocking(point, &release, false, None),
        Perturbation::HoldEach(release) if on_tokio => hold(point, &release, true).await,
        Perturbation::HoldEach(release) => hold_blocking(point, &release, true, None),
    }
}

/// Waits until the visit is released ([`released`]), saying so on stderr
/// unless it is at once.
async fn hold(point: &str, release: &Path, each: bool) {
    if released(point, release, each) {
        return;
    }
    eprintln!(
        "race_probe: {point} holding until {} exists",
        release.display()
    );
    while !released(point, release, each) {
        tokio::time::sleep(HOLD_POLL).await;
    }
}

/// Whether a held visit may go on: its release file exists (`hold`), or, for
/// `hold-each`, this visit consumed it by renaming it away. Of two visits that
/// try to consume one release, one rename succeeds and the other finds the
/// release gone and goes on waiting, so two visits can never both go on for
/// one release. (The name it is renamed to need not be unique: in a PID
/// namespace two processes can share a process ID, and the later rename
/// simply replaces the earlier consumed file.) A consumption that fails for
/// any reason but the release being absent fails the visit, naming the point.
fn released(point: &str, release: &Path, each: bool) -> bool {
    if !each {
        return release.exists();
    }
    static CONSUMED: AtomicU64 = AtomicU64::new(0);
    let consumed = release.with_file_name(format!(
        "{point}.consumed.{}.{}",
        std::process::id(),
        CONSUMED.fetch_add(1, Ordering::Relaxed)
    ));
    match std::fs::rename(release, &consumed) {
        Ok(()) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => panic!(
            "race_probe: {point} could not consume {}: {error}",
            release.display()
        ),
    }
}

/// A hold at `point`, when configured, in Hermit's launcher or a container's
/// init, outside any guest ([`SETUP_MOUNTS_MADE`]): it blocks the calling
/// thread. A hold here parks the run itself, so the point is armed only where
/// [`RACE_PROBE_HOLD_DIR_ENV`] names a directory holding
/// [`LAUNCHER_HOLD_MARKER`], which only a test makes, and that is decided
/// first, from that one variable and that one file: an unarmed point reads,
/// parses and draws nothing else, whatever [`RACE_PROBE_ENV`] says. Armed,
/// nothing is ignored: [`armed_hold`] must find a complete, valid hold for the
/// point, or the visit fails, naming the point and why. The hold fails,
/// naming the point, if it is not released within [`LAUNCHER_HOLD_LIMIT`].
pub fn delay_in_launcher(point: &str) {
    launcher_visit(
        launcher_hold_dir(),
        || std::env::var_os(RACE_PROBE_ENV),
        point,
    );
}

/// The hold directory that arms launcher points in this process
/// ([`armed_by`]), or `None` when they are unarmed. Decided once.
fn launcher_hold_dir() -> Option<&'static Path> {
    static ARMED: OnceLock<Option<PathBuf>> = OnceLock::new();
    ARMED
        .get_or_init(|| {
            std::env::var_os(RACE_PROBE_HOLD_DIR_ENV)
                .map(PathBuf::from)
                .filter(|dir| armed_by(Some(dir)))
        })
        .as_deref()
}

/// Whether `hold_dir` holds [`LAUNCHER_HOLD_MARKER`], which arms
/// [`delay_in_launcher`].
fn armed_by(hold_dir: Option<&Path>) -> bool {
    hold_dir.is_some_and(|dir| {
        !dir.as_os_str().is_empty() && dir.join(LAUNCHER_HOLD_MARKER).exists()
    })
}

/// A [`delay_in_launcher`] visit, armed by `hold_dir`: unless armed, it
/// returns before `config` is read. Armed, the configuration's hold for the
/// point ([`armed_hold`]) holds the visit, and anything else fails it.
fn launcher_visit(
    hold_dir: Option<&Path>,
    config: impl FnOnce() -> Option<std::ffi::OsString>,
    point: &str,
) {
    let Some(hold_dir) = hold_dir else {
        return;
    };
    let config = config();
    let hold = match config.as_deref().map(std::ffi::OsStr::to_str) {
        Some(None) => Err(format!("{RACE_PROBE_ENV} is not UTF-8")),
        config => armed_hold(config.flatten(), hold_dir, point),
    };
    match hold {
        Ok(hold) => launcher_hold(point, hold),
        Err(reason) => panic!("race_probe: {point} is armed, and {RACE_PROBE_ENV} is refused there: {reason}"),
    }
}

/// The hold an armed launcher point takes under `config`, its release in
/// `hold_dir`, or why there is none. Nothing is ignored: every item must
/// parse and name a known point, and the point's own must be `hold` or
/// `hold-each`; a wait, an item that does not parse, an unknown point, or no
/// item for the point at all is refused.
fn armed_hold(config: Option<&str>, hold_dir: &Path, point: &str) -> Result<Perturbation, String> {
    let config = config.ok_or_else(|| format!("{RACE_PROBE_ENV} is not set"))?;
    let mut each = None;
    for item in config.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let (name, spec) = parse_item(item)?;
        if name == point {
            if !spec.hold {
                return Err(format!("{item:?} is not a hold (hold or hold-each)"));
            }
            each = Some(spec.each);
        }
    }
    let each = each.ok_or_else(|| format!("{config:?} names no hold for {point}"))?;
    let release = hold_dir.join(point);
    Ok(if each {
        Perturbation::HoldEach(release)
    } else {
        Perturbation::Hold(release)
    })
}

/// An armed launcher hold, failing after [`LAUNCHER_HOLD_LIMIT`].
fn launcher_hold(point: &str, hold: Perturbation) {
    match hold {
        Perturbation::Hold(release) => {
            hold_blocking(point, &release, false, Some(LAUNCHER_HOLD_LIMIT))
        }
        Perturbation::HoldEach(release) => {
            hold_blocking(point, &release, true, Some(LAUNCHER_HOLD_LIMIT))
        }
        Perturbation::Wait(_) => unreachable!("armed_hold returns holds only"),
    }
}

/// [`hold`], blocking the calling thread, and failing, naming the point,
/// after `limit` if one is given.
fn hold_blocking(point: &str, release: &Path, each: bool, limit: Option<Duration>) {
    if released(point, release, each) {
        return;
    }
    eprintln!(
        "race_probe: {point} holding until {} exists",
        release.display()
    );
    let start = std::time::Instant::now();
    while !released(point, release, each) {
        if let Some(limit) = limit
            && start.elapsed() >= limit
        {
            panic!(
                "race_probe: {point} was not released within {limit:?}: {} never appeared",
                release.display()
            );
        }
        std::thread::sleep(HOLD_POLL);
    }
}

/// Whether a synchronous point takes its perturbed branch on this visit.
pub(crate) fn perturb(point: &str) -> bool {
    choose(point).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn race_probes_are_off_unless_configured() {
        // Unset, empty, or naming no valid point: no probes, so every point
        // returns before drawing a decision.
        assert!(Probes::from_settings(None, None, None).is_none());
        assert!(Probes::from_settings(None, Some("7"), None).is_none());
        assert!(Probes::from_settings(Some(""), Some("7"), None).is_none());
        assert!(Probes::from_settings(Some(" , "), None, None).is_none());
        assert!(Probes::from_settings(Some("no_such_point=5"), None, None).is_none());
        // This test process does not set the variable, so the process-wide
        // probes are off and no point perturbs.
        assert!(std::env::var_os(RACE_PROBE_ENV).is_none());
        assert!(probes().is_none());
        for point in POINTS {
            assert!(!perturb(point));
            assert_eq!(choose(point), None);
        }
    }

    #[test]
    fn race_probe_settings_parse_as_documented() {
        assert_eq!(
            parse_item("bg_pre_call=300"),
            Ok((
                BG_PRE_CALL.to_owned(),
                Spec {
                    lo_ms: 300,
                    hi_ms: 300,
                    probability: 1.0,
                    hold: false,
                    each: false,
                }
            ))
        );
        assert_eq!(
            parse_item("bg_post_call=0-50@0.25"),
            Ok((
                BG_POST_CALL.to_owned(),
                Spec {
                    lo_ms: 0,
                    hi_ms: 50,
                    probability: 0.25,
                    hold: false,
                    each: false,
                }
            ))
        );
        for bad in [
            "bg_pre_call",
            "bg_pre_call=x",
            "bg_pre_call=5-2",
            "bg_pre_call=1@2",
            "bg_harvest_skip=0@-0.1",
            "unknown=1",
        ] {
            assert!(parse_item(bad).is_err(), "{bad} parsed");
        }
        // A bad item is skipped, the good ones stay.
        let probes = Probes::from_settings(
            Some("bg_pre_call=x, bg_harvest_skip=0@0.5"),
            Some("1"),
            None,
        )
        .unwrap();
        assert_eq!(probes.points.len(), 1);
        assert!(probes.points.contains_key(BG_HARVEST_SKIP));

        // A later valid duplicate replaces the earlier setting. A malformed
        // duplicate is ignored and must not erase that last valid value.
        let probes = Probes::from_settings(
            Some("bg_pre_call=10@0,bg_pre_call=20@1,bg_pre_call=x,bg_harvest_skip=0@0.5"),
            Some("7"),
            None,
        )
        .unwrap();
        assert_eq!(probes.points.len(), 2);
        assert_eq!(
            probes.points.get(BG_PRE_CALL),
            Some(&Spec {
                lo_ms: 20,
                hi_ms: 20,
                probability: 1.0,
                hold: false,
                each: false,
            })
        );
        assert_eq!(
            probes.choose(BG_PRE_CALL),
            Some(Perturbation::Wait(Duration::from_millis(20)))
        );
    }

    #[test]
    fn race_probe_choices_follow_probability_range_and_seed() {
        let draw = |config: &str, seed: &str, point: &str| {
            let probes = Probes::from_settings(Some(config), Some(seed), None).unwrap();
            (0..1000).map(|_| probes.choose(point)).collect::<Vec<_>>()
        };
        assert!(
            draw("bg_harvest_skip=0@0", "1", BG_HARVEST_SKIP)
                .iter()
                .all(Option::is_none)
        );
        assert!(
            draw("bg_harvest_skip=0@1", "1", BG_HARVEST_SKIP)
                .iter()
                .all(|wait| *wait == Some(Perturbation::Wait(Duration::ZERO)))
        );
        // A point that is not configured never perturbs.
        assert!(
            draw("bg_harvest_skip=0@1", "1", BG_PRE_CALL)
                .iter()
                .all(Option::is_none)
        );
        let half = draw("bg_pre_call=10-20@0.5", "3", BG_PRE_CALL);
        let taken = half.iter().flatten().count();
        assert!((400..600).contains(&taken), "{taken} of 1000 at 0.5");
        assert!(half.iter().flatten().all(|wait| {
            matches!(wait, Perturbation::Wait(wait)
                if (Duration::from_millis(10)..=Duration::from_millis(20)).contains(wait))
        }));
        // The same seed draws the same sequence; another seed does not.
        assert_eq!(half, draw("bg_pre_call=10-20@0.5", "3", BG_PRE_CALL));
        assert_ne!(half, draw("bg_pre_call=10-20@0.5", "4", BG_PRE_CALL));

        // Missing-point lookups must not consume a visit before the actual
        // seeded draws, including when configured points are interleaved.
        let config = Some("bg_pre_call=10-20@0.5,bg_harvest_skip=0@0.5");
        let with_missing = Probes::from_settings(config, Some("3"), None).unwrap();
        let direct = Probes::from_settings(config, Some("3"), None).unwrap();
        for _ in 0..1000 {
            assert_eq!(with_missing.choose("no_such_point"), None);
            assert_eq!(with_missing.choose(BG_POST_CALL), None);
            assert_eq!(with_missing.choose(BG_PRE_CALL), direct.choose(BG_PRE_CALL));
            assert_eq!(
                with_missing.choose(BG_HARVEST_SKIP),
                direct.choose(BG_HARVEST_SKIP)
            );
        }
    }

    #[test]
    fn a_hold_needs_a_hold_directory_and_names_its_release_file() {
        assert_eq!(
            parse_item("fifo_staging_mapped=hold"),
            Ok((
                FIFO_STAGING_MAPPED.to_owned(),
                Spec {
                    lo_ms: 0,
                    hi_ms: 0,
                    probability: 1.0,
                    hold: true,
                    each: false,
                }
            ))
        );
        assert_eq!(
            parse_item("setup_mounts_made=hold-each"),
            Ok((
                SETUP_MOUNTS_MADE.to_owned(),
                Spec {
                    lo_ms: 0,
                    hi_ms: 0,
                    probability: 1.0,
                    hold: true,
                    each: true,
                }
            ))
        );
        let each =
            Probes::from_settings(Some("setup_mounts_made=hold-each"), None, Some(Path::new("/hold"))).unwrap();
        assert_eq!(
            each.choose(SETUP_MOUNTS_MADE),
            Some(Perturbation::HoldEach(PathBuf::from("/hold/setup_mounts_made")))
        );
        for bad in ["fifo_staging_mapped=hold@0.5", "fifo_staging_mapped=holds"] {
            assert!(parse_item(bad).is_err(), "{bad} parsed");
        }
        // Without a directory the hold is skipped, and the other items stay.
        let probes = Probes::from_settings(
            Some("fifo_staging_mapped=hold,bg_harvest_skip=0"),
            Some("1"),
            None,
        )
        .unwrap();
        assert!(!probes.points.contains_key(FIFO_STAGING_MAPPED));
        assert!(Probes::from_settings(Some("fifo_staging_mapped=hold"), None, Some(Path::new(""))).is_none());
        // With one, every visit holds on the point's own file, and draws no
        // visit from the seeded sequence of the other points.
        let config = Some("fifo_staging_mapped=hold,fifo_staging_reclaim=hold,bg_pre_call=1-9@0.5");
        let holding = Probes::from_settings(config, Some("5"), Some(Path::new("/hold"))).unwrap();
        let direct = Probes::from_settings(Some("bg_pre_call=1-9@0.5"), Some("5"), None).unwrap();
        for _ in 0..100 {
            assert_eq!(
                holding.choose(FIFO_STAGING_MAPPED),
                Some(Perturbation::Hold(PathBuf::from(
                    "/hold/fifo_staging_mapped"
                )))
            );
            assert_eq!(
                holding.choose(FIFO_STAGING_RECLAIM),
                Some(Perturbation::Hold(PathBuf::from(
                    "/hold/fifo_staging_reclaim"
                )))
            );
            assert_eq!(holding.choose(BG_PRE_CALL), direct.choose(BG_PRE_CALL));
        }
    }

    /// A held visit returns only once its release file exists, and at once
    /// when it already does.
    #[tokio::test(start_paused = true)]
    async fn a_held_visit_waits_for_its_release_file() {
        let dir = tempfile::tempdir().unwrap();
        let release = dir.path().join(FIFO_STAGING_MAPPED);
        let mut held = Box::pin(hold(FIFO_STAGING_MAPPED, &release, false));
        // A minute of (paused, auto-advancing) time does not release it.
        tokio::select! {
            _ = &mut held => panic!("released before its file existed"),
            _ = tokio::time::sleep(Duration::from_secs(60)) => {}
        }
        std::fs::write(&release, b"").unwrap();
        tokio::time::timeout(Duration::from_secs(1), held)
            .await
            .expect("released once its file exists");
        tokio::time::timeout(Duration::ZERO, hold(FIFO_STAGING_MAPPED, &release, false))
            .await
            .expect("an existing file releases at once");
    }

    /// A `hold-each` visit consumes its release, renaming it away, so the
    /// next visit waits for a release of its own, and two visits never go on
    /// for one release; a plain hold leaves it.
    #[test]
    fn a_hold_each_visit_consumes_its_release() {
        let dir = tempfile::tempdir().unwrap();
        let release = dir.path().join(SETUP_MOUNTS_MADE);
        std::fs::write(&release, b"").unwrap();
        assert!(released(SETUP_MOUNTS_MADE, &release, true));
        assert!(!release.exists(), "a hold-each visit consumes its release");
        assert!(
            !released(SETUP_MOUNTS_MADE, &release, true),
            "a second visit finds no release to consume"
        );
        std::fs::write(&release, b"").unwrap();
        hold_blocking(SETUP_MOUNTS_MADE, &release, false, None);
        assert!(release.exists(), "a plain hold leaves its release for later visits");
    }

    /// A `hold-each` consumption that fails for any reason but an absent
    /// release fails the visit, naming the point.
    #[test]
    #[should_panic(expected = "race_probe: setup_mounts_made could not consume")]
    fn a_failed_consumption_fails_the_visit() {
        let dir = tempfile::tempdir().unwrap();
        // The release's directory is a regular file: the rename fails with
        // ENOTDIR, not with the release absent.
        let not_a_directory = dir.path().join("not-a-directory");
        std::fs::write(&not_a_directory, b"").unwrap();
        released(SETUP_MOUNTS_MADE, &not_a_directory.join(SETUP_MOUNTS_MADE), true);
    }

    /// A launcher point is armed only by a hold directory holding the marker
    /// a test makes: no directory, or one without the marker, arms nothing.
    #[test]
    fn a_launcher_point_needs_the_marker() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!armed_by(None));
        assert!(!armed_by(Some(Path::new(""))));
        assert!(!armed_by(Some(dir.path())));
        std::fs::write(dir.path().join(LAUNCHER_HOLD_MARKER), b"").unwrap();
        assert!(armed_by(Some(dir.path())));
    }

    /// An unarmed launcher point reads, parses and draws nothing: it returns
    /// before its configuration is asked for.
    #[test]
    fn an_unarmed_launcher_point_reads_no_settings() {
        launcher_visit(
            None,
            || panic!("an unarmed launcher point asked for its configuration"),
            SETUP_MOUNTS_MADE,
        );
    }

    /// Armed, the point's `hold` or `hold-each` is its hold, its release in
    /// the hold directory as the environment gives it, UTF-8 or not.
    #[test]
    fn an_armed_launcher_point_takes_its_hold() {
        use std::os::unix::ffi::OsStrExt;
        let dir = Path::new(std::ffi::OsStr::from_bytes(b"/hold-\xff"));
        let release = dir.join(SETUP_MOUNTS_MADE);
        assert_eq!(
            armed_hold(Some("setup_mounts_made=hold"), dir, SETUP_MOUNTS_MADE),
            Ok(Perturbation::Hold(release.clone()))
        );
        assert_eq!(
            armed_hold(
                Some("bg_pre_call=1-9, setup_mounts_made=hold-each"),
                dir,
                SETUP_MOUNTS_MADE
            ),
            Ok(Perturbation::HoldEach(release))
        );
    }

    /// Armed, nothing is ignored: anything but a complete, valid hold for the
    /// point is refused, naming why.
    #[test]
    fn an_armed_launcher_point_refuses_anything_but_its_hold() {
        let dir = Path::new("/hold");
        for (config, reason) in [
            (None, "HERMIT_RACE_PROBE is not set"),
            (Some(""), "names no hold for setup_mounts_made"),
            (Some("setup_mounts_made"), "\"setup_mounts_made\" is not point=LO[-HI][@P]"),
            (Some("setup_mounts_made="), "is not a number of milliseconds"),
            (Some("setup_mounts_made=9-1"), "the range is empty"),
            (Some("setup_mounts_made=1-2@7"), "probability must be in [0, 1]"),
            (Some("setup_mounts_made=garbage"), "is not a number of milliseconds"),
            (Some("setup_mounts_made=5"), "is not a hold (hold or hold-each)"),
            (Some("setup_mounts_made=hold@0.5"), "is not a number of milliseconds"),
            (Some("setup_mounts_made=hold,no_such_point=hold"), "unknown point \"no_such_point\""),
            (Some("setup_mounts_made=hold,bg_pre_call=x"), "\"x\" is not a number of milliseconds"),
            (Some("bg_pre_call=1-9"), "names no hold for setup_mounts_made"),
        ] {
            let refused = armed_hold(config, dir, SETUP_MOUNTS_MADE)
                .expect_err(&format!("{config:?} was taken as a hold"));
            assert!(refused.contains(reason), "{config:?}: {refused}");
        }
    }

    /// An armed launcher point whose configuration is refused fails the
    /// visit, naming the point and why.
    #[test]
    #[should_panic(
        expected = "race_probe: setup_mounts_made is armed, and HERMIT_RACE_PROBE is refused there: \"setup_mounts_made\" is not point=LO[-HI][@P]"
    )]
    fn an_armed_launcher_point_with_a_refused_configuration_fails_by_name() {
        launcher_visit(
            Some(Path::new("/hold")),
            || Some("setup_mounts_made".into()),
            SETUP_MOUNTS_MADE,
        );
    }

    /// A full-width range samples without overflowing.
    #[test]
    fn a_full_width_range_samples_without_overflow() {
        let full = format!("{SETUP_MOUNTS_MADE}=0-{}", u64::MAX);
        let probes = Probes::from_settings(Some(&full), Some("1"), None).unwrap();
        for _ in 0..1000 {
            assert!(matches!(
                probes.choose(SETUP_MOUNTS_MADE),
                Some(Perturbation::Wait(_))
            ));
        }
    }

    /// A blocking hold with a limit fails, naming the point, once the limit
    /// passes without a release.
    #[test]
    #[should_panic(expected = "race_probe: setup_mounts_made was not released within")]
    fn a_limited_hold_fails_after_its_limit() {
        let dir = tempfile::tempdir().unwrap();
        let release = dir.path().join(SETUP_MOUNTS_MADE);
        hold_blocking(
            SETUP_MOUNTS_MADE,
            &release,
            false,
            Some(Duration::from_millis(50)),
        );
    }
}
