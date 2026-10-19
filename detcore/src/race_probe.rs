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
use std::sync::OnceLock;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;

/// The variable that enables race probes and lists their settings.
pub const RACE_PROBE_ENV: &str = "HERMIT_RACE_PROBE";

/// The variable that fixes the seed of the race probes' choices.
pub const RACE_PROBE_SEED_ENV: &str = "HERMIT_RACE_PROBE_SEED";

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

/// Every probe point, for validating a configuration.
const POINTS: [&str; 6] = [
    BG_PRE_CALL,
    BG_POST_CALL,
    BG_HARVEST_SKIP,
    FIFO_STAGING_MAPPED,
    FIFO_STAGING_RECLAIM,
    PROCFS_SPELLING_RESOLVED,
];

#[derive(Clone, Copy, Debug, PartialEq)]
struct Spec {
    lo_ms: u64,
    hi_ms: u64,
    probability: f64,
}

#[derive(Debug)]
struct Probes {
    seed: u64,
    counter: AtomicU64,
    points: BTreeMap<String, Spec>,
}

fn parse_item(item: &str) -> Result<(String, Spec), String> {
    let (name, rest) = item
        .split_once('=')
        .ok_or_else(|| format!("{item:?} is not point=LO[-HI][@P]"))?;
    if !POINTS.contains(&name) {
        return Err(format!("unknown point {name:?}; points are {POINTS:?}"));
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
        },
    ))
}

impl Probes {
    /// The probes `config` and `seed` (the two variables' values) describe:
    /// `None` when probes are off. A malformed item is reported and skipped.
    fn from_settings(config: Option<&str>, seed: Option<&str>) -> Option<Self> {
        let config = config?;
        // A disabled probe must not create randomized hash state: even an
        // empty HashMap initializes RandomState's thread-local random keys.
        // These few exact names need no hashing, and insert still makes the
        // last valid duplicate win without changing lookup or draw order.
        let mut points = BTreeMap::new();
        for item in config.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            match parse_item(item) {
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
        })
    }

    /// The decision at `point` for this visit: `None` when the point is not
    /// configured or this visit is not perturbed, otherwise the wait.
    fn choose(&self, point: &str) -> Option<Duration> {
        let spec = self.points.get(point)?;
        let visit = self.counter.fetch_add(1, Ordering::Relaxed);
        let r = splitmix(self.seed ^ splitmix(visit));
        let unit = (r >> 11) as f64 / (1u64 << 53) as f64;
        if unit >= spec.probability {
            return None;
        }
        let span = spec.hi_ms - spec.lo_ms;
        let ms = spec.lo_ms
            + if span == 0 {
                0
            } else {
                splitmix(r) % (span + 1)
            };
        Some(Duration::from_millis(ms))
    }
}

fn probes() -> Option<&'static Probes> {
    static PROBES: OnceLock<Option<Probes>> = OnceLock::new();
    PROBES
        .get_or_init(|| {
            let config = std::env::var(RACE_PROBE_ENV).ok()?;
            let seed = std::env::var(RACE_PROBE_SEED_ENV).ok();
            let probes = Probes::from_settings(Some(&config), seed.as_deref())?;
            eprintln!(
                "race_probe: {RACE_PROBE_ENV}={config} {RACE_PROBE_SEED_ENV}={}",
                probes.seed
            );
            Some(probes)
        })
        .as_ref()
}

fn splitmix(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

fn choose(point: &str) -> Option<Duration> {
    let wait = probes()?.choose(point)?;
    tracing::debug!(target: "race_probe", "race_probe: {point} perturbed ({wait:?})");
    Some(wait)
}

/// An async wait at `point`, when configured.
pub(crate) async fn delay(point: &str) {
    if let Some(wait) = choose(point)
        && !wait.is_zero()
    {
        tokio::time::sleep(wait).await;
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
        assert!(Probes::from_settings(None, None).is_none());
        assert!(Probes::from_settings(None, Some("7")).is_none());
        assert!(Probes::from_settings(Some(""), Some("7")).is_none());
        assert!(Probes::from_settings(Some(" , "), None).is_none());
        assert!(Probes::from_settings(Some("no_such_point=5"), None).is_none());
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
                    probability: 1.0
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
                    probability: 0.25
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
        let probes =
            Probes::from_settings(Some("bg_pre_call=x, bg_harvest_skip=0@0.5"), Some("1")).unwrap();
        assert_eq!(probes.points.len(), 1);
        assert!(probes.points.contains_key(BG_HARVEST_SKIP));

        // A later valid duplicate replaces the earlier setting. A malformed
        // duplicate is ignored and must not erase that last valid value.
        let probes = Probes::from_settings(
            Some("bg_pre_call=10@0,bg_pre_call=20@1,bg_pre_call=x,bg_harvest_skip=0@0.5"),
            Some("7"),
        )
        .unwrap();
        assert_eq!(probes.points.len(), 2);
        assert_eq!(
            probes.points.get(BG_PRE_CALL),
            Some(&Spec {
                lo_ms: 20,
                hi_ms: 20,
                probability: 1.0,
            })
        );
        assert_eq!(probes.choose(BG_PRE_CALL), Some(Duration::from_millis(20)));
    }

    #[test]
    fn race_probe_choices_follow_probability_range_and_seed() {
        let draw = |config: &str, seed: &str, point: &str| {
            let probes = Probes::from_settings(Some(config), Some(seed)).unwrap();
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
                .all(|wait| *wait == Some(Duration::ZERO))
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
            (Duration::from_millis(10)..=Duration::from_millis(20)).contains(wait)
        }));
        // The same seed draws the same sequence; another seed does not.
        assert_eq!(half, draw("bg_pre_call=10-20@0.5", "3", BG_PRE_CALL));
        assert_ne!(half, draw("bg_pre_call=10-20@0.5", "4", BG_PRE_CALL));

        // Missing-point lookups must not consume a visit before the actual
        // seeded draws, including when configured points are interleaved.
        let config = Some("bg_pre_call=10-20@0.5,bg_harvest_skip=0@0.5");
        let with_missing = Probes::from_settings(config, Some("3")).unwrap();
        let direct = Probes::from_settings(config, Some("3")).unwrap();
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
}
