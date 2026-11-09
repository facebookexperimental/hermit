/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! `--time-budget` and `--trial-budget`: how much wall-clock time and how
//! many trials one `hermit analyze` invocation may spend before it reports
//! what it has found so far.
//!
//! The budget decides only when analyze stops. It is checked before every
//! trial, and a trial gets at most the time that is left as its `--timeout`.
//! A trial that time cuts is thrown away rather than counted as an outcome
//! (`BudgetExpired::cut`), so no search decision ever depends on the budget.

use std::fmt;
use std::num::NonZeroU64;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;

use serde::Deserialize;
use serde::Serialize;

/// `--time-budget`: wall-clock time for the whole invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeBudget {
    /// No limit.
    Off,
    /// At most this long.
    Limit(Duration),
}

impl FromStr for TimeBudget {
    type Err = String;

    /// `off`, or a positive whole number of seconds, minutes or hours: `1800`,
    /// `1800s`, `30m`, `2h`.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s == "off" {
            return Ok(TimeBudget::Off);
        }
        let (digits, unit) = match s.char_indices().find(|(_, c)| !c.is_ascii_digit()) {
            Some((at, _)) => s.split_at(at),
            None => (s, "s"),
        };
        let seconds_per_unit = match unit {
            "s" => 1,
            "m" => 60,
            "h" => 3600,
            _ => 0,
        };
        match digits.parse::<u64>() {
            Ok(count) if count > 0 && seconds_per_unit > 0 => count
                .checked_mul(seconds_per_unit)
                .map(|seconds| TimeBudget::Limit(Duration::from_secs(seconds)))
                .ok_or_else(|| format!("time budget {s:?} is too large")),
            _ => Err(format!(
                "expected `off` or a positive whole number of seconds, minutes or hours (1800, \
                 1800s, 30m, 2h), got {s:?}"
            )),
        }
    }
}

/// `--trial-budget`: how many trials the invocation may run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrialBudget {
    /// No limit.
    Off,
    /// At most this many.
    Limit(NonZeroU64),
}

impl FromStr for TrialBudget {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s == "off" {
            return Ok(TrialBudget::Off);
        }
        s.parse()
            .map(TrialBudget::Limit)
            .map_err(|_| format!("expected `off` or a positive number of trials, got {s:?}"))
    }
}

/// Which budget ran out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BudgetKind {
    /// `--time-budget`.
    TimeBudget,
    /// `--trial-budget`.
    TrialBudget,
}

impl BudgetKind {
    /// The option, as the user spells it.
    pub fn option(self) -> &'static str {
        match self {
            BudgetKind::TimeBudget => "--time-budget",
            BudgetKind::TrialBudget => "--trial-budget",
        }
    }

    /// The `class=` value of the stderr class line.
    pub fn class(self) -> &'static str {
        match self {
            BudgetKind::TimeBudget => "time-budget",
            BudgetKind::TrialBudget => "trial-budget",
        }
    }
}

/// The budget ran out: analyze starts no further trial and reports what it
/// has found so far.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BudgetExpired {
    /// Which budget.
    pub kind: BudgetKind,
    /// The trial the time budget cut short, if one was running: its outcome
    /// is not read, and a resumed analysis runs it again.
    pub cut: Option<String>,
}

impl fmt::Display for BudgetExpired {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "hermit analyze reached its {}", self.kind.option())?;
        if let Some(run) = &self.cut {
            write!(f, " during {run}, whose outcome is not counted")?;
        }
        Ok(())
    }
}

impl std::error::Error for BudgetExpired {}

/// Where elapsed time comes from: the host's monotonic clock, or a test's.
type Clock = Arc<dyn Fn() -> Duration + Send + Sync>;

/// The shared state of a budget: every copy of the analyze options (each
/// run holds one) sees the same.
struct State {
    time: Option<Duration>,
    trials: Option<u64>,
    /// Trials started so far; a trial answered from a resumed journal does
    /// not count.
    started: u64,
    /// Which budget ran out, once one has.
    expired: Option<BudgetKind>,
    /// After expiry: how long the partial report's own replays may still run
    /// (elapsed time), if any may.
    grace_until: Option<Duration>,
}

/// One invocation's budget. Cloning shares it.
#[derive(Clone)]
pub struct Budget {
    state: Arc<Mutex<State>>,
    clock: Clock,
}

impl fmt::Debug for Budget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = self.state.lock().unwrap();
        f.debug_struct("Budget")
            .field("time", &state.time)
            .field("trials", &state.trials)
            .field("started", &state.started)
            .field("expired", &state.expired)
            .finish()
    }
}

impl Default for Budget {
    /// No limit.
    fn default() -> Self {
        Self::new(TimeBudget::Off, TrialBudget::Off)
    }
}

/// The longest grace period the partial report's stack-trace replays get
/// after the budget ran out.
pub const STACK_GRACE_CAP: Duration = Duration::from_secs(120);
/// The share of `--time-budget` the grace period may take, in percent.
pub const STACK_GRACE_PERCENT: u32 = 5;

impl Budget {
    /// A budget counted from now on the host's monotonic clock.
    pub fn new(time: TimeBudget, trials: TrialBudget) -> Self {
        let start = Instant::now();
        Self::with_clock(time, trials, Arc::new(move || start.elapsed()))
    }

    /// A budget that reads elapsed time from `clock`.
    pub fn with_clock(time: TimeBudget, trials: TrialBudget, clock: Clock) -> Self {
        Self {
            state: Arc::new(Mutex::new(State {
                time: match time {
                    TimeBudget::Off => None,
                    TimeBudget::Limit(limit) => Some(limit),
                },
                trials: match trials {
                    TrialBudget::Off => None,
                    TrialBudget::Limit(limit) => Some(limit.get()),
                },
                started: 0,
                expired: None,
                grace_until: None,
            })),
            clock,
        }
    }

    /// Elapsed time since the budget started.
    pub fn elapsed(&self) -> Duration {
        (self.clock)()
    }

    /// The time limit, if any.
    pub fn time_limit(&self) -> Option<Duration> {
        self.state.lock().unwrap().time
    }

    /// The trial limit, if any.
    pub fn trial_limit(&self) -> Option<u64> {
        self.state.lock().unwrap().trials
    }

    /// How many trials have started.
    pub fn trials_started(&self) -> u64 {
        self.state.lock().unwrap().started
    }

    /// Which budget ran out, if one has.
    #[cfg(test)]
    pub fn expired(&self) -> Option<BudgetKind> {
        self.state.lock().unwrap().expired
    }

    /// Called before a trial starts: `Err` once a budget has run out, which
    /// it then stays. Otherwise counts the trial and returns the whole
    /// seconds of time left to bound it with, if time is limited (at least
    /// one: a trial bound is a whole positive number of seconds).
    pub fn start_trial(&self) -> Result<Option<NonZeroU64>, BudgetExpired> {
        let now = self.elapsed();
        let mut state = self.state.lock().unwrap();
        if let Some(kind) = state.expired {
            return match state.grace_until {
                Some(until) if now < until => Ok(Some(whole_seconds(until - now))),
                _ => Err(BudgetExpired { kind, cut: None }),
            };
        }
        if state.trials.is_some_and(|limit| state.started >= limit) {
            state.expired = Some(BudgetKind::TrialBudget);
            return Err(BudgetExpired {
                kind: BudgetKind::TrialBudget,
                cut: None,
            });
        }
        let left = match state.time {
            Some(limit) if now >= limit => {
                state.expired = Some(BudgetKind::TimeBudget);
                return Err(BudgetExpired {
                    kind: BudgetKind::TimeBudget,
                    cut: None,
                });
            }
            Some(limit) => Some(whole_seconds(limit - now)),
            None => None,
        };
        state.started += 1;
        Ok(left)
    }

    /// The time budget cut the trial `run` short: it ended by a `--timeout`
    /// that the time left had shortened. Its outcome came from the budget,
    /// not from the guest, so it is thrown away.
    pub fn cut(&self, run: &str) -> BudgetExpired {
        let mut state = self.state.lock().unwrap();
        let kind = *state.expired.get_or_insert(BudgetKind::TimeBudget);
        BudgetExpired {
            kind,
            cut: Some(run.to_owned()),
        }
    }

    /// The grace period the partial report's stack-trace replays get after
    /// the budget ran out: min([`STACK_GRACE_CAP`], [`STACK_GRACE_PERCENT`]
    /// of `--time-budget`), or the cap when only trials were limited.
    pub fn stack_grace(&self) -> Duration {
        let state = self.state.lock().unwrap();
        match state.time {
            Some(limit) => STACK_GRACE_CAP.min(limit * STACK_GRACE_PERCENT / 100),
            None => STACK_GRACE_CAP,
        }
    }

    /// Open the grace period: until it ends, [`Budget::start_trial`] lets
    /// trials start again, each bounded by what is left of it.
    pub fn open_grace(&self, grace: Duration) {
        let now = self.elapsed();
        self.state.lock().unwrap().grace_until = Some(now + grace);
    }

    /// Close the grace period.
    pub fn close_grace(&self) {
        self.state.lock().unwrap().grace_until = None;
    }
}

/// `duration` rounded up to whole seconds, at least one.
fn whole_seconds(duration: Duration) -> NonZeroU64 {
    let seconds = duration.as_secs() + u64::from(duration.subsec_nanos() > 0);
    NonZeroU64::new(seconds.max(1)).unwrap()
}

/// `duration` as the user would write it: `1800 s`, or `30 min` when whole.
pub fn describe(duration: Duration) -> String {
    let seconds = duration.as_secs();
    if seconds >= 60 && seconds.is_multiple_of(60) {
        format!("{} min", seconds / 60)
    } else {
        format!("{seconds} s")
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::Ordering;

    use super::*;

    /// A clock a test moves by hand, in whole seconds.
    fn manual_clock() -> (Arc<AtomicU64>, Clock) {
        let seconds = Arc::new(AtomicU64::new(0));
        let read = Arc::clone(&seconds);
        (
            seconds,
            Arc::new(move || Duration::from_secs(read.load(Ordering::SeqCst))),
        )
    }

    #[test]
    fn a_time_budget_takes_seconds_minutes_or_hours() {
        for (text, seconds) in [("1800", 1800), ("1800s", 1800), ("30m", 1800), ("2h", 7200)] {
            assert_eq!(
                text.parse::<TimeBudget>(),
                Ok(TimeBudget::Limit(Duration::from_secs(seconds))),
                "{text}"
            );
        }
        assert_eq!("off".parse::<TimeBudget>(), Ok(TimeBudget::Off));
        for bad in ["0", "", "30x", "m", "-5", "1.5h", "30 m"] {
            assert!(bad.parse::<TimeBudget>().is_err(), "{bad:?}");
        }
        assert_eq!(
            "3".parse::<TrialBudget>(),
            Ok(TrialBudget::Limit(NonZeroU64::new(3).unwrap()))
        );
        assert_eq!("off".parse::<TrialBudget>(), Ok(TrialBudget::Off));
        assert!("0".parse::<TrialBudget>().is_err());
    }

    /// Each trial gets the time left, rounded up, and none starts once it is
    /// gone; the budget then stays spent.
    #[test]
    fn trials_get_the_time_left_and_none_starts_after_it() {
        let (now, clock) = manual_clock();
        let budget = Budget::with_clock(
            TimeBudget::Limit(Duration::from_secs(100)),
            TrialBudget::Off,
            clock,
        );
        assert_eq!(budget.start_trial(), Ok(NonZeroU64::new(100)));
        now.store(99, Ordering::SeqCst);
        assert_eq!(budget.start_trial(), Ok(NonZeroU64::new(1)));
        now.store(100, Ordering::SeqCst);
        let expired = budget.start_trial().unwrap_err();
        assert_eq!(expired.kind, BudgetKind::TimeBudget);
        assert_eq!(expired.cut, None);
        assert_eq!(budget.expired(), Some(BudgetKind::TimeBudget));
        assert_eq!(budget.trials_started(), 2);
        // A grace period lets the report's replays run, bounded by it.
        budget.open_grace(Duration::from_secs(5));
        assert_eq!(budget.start_trial(), Ok(NonZeroU64::new(5)));
        now.store(105, Ordering::SeqCst);
        assert!(budget.start_trial().is_err());
    }

    #[test]
    fn the_trial_budget_counts_trials_started() {
        let budget = Budget::new(
            TimeBudget::Off,
            TrialBudget::Limit(NonZeroU64::new(2).unwrap()),
        );
        assert_eq!(budget.start_trial(), Ok(None));
        assert_eq!(budget.start_trial(), Ok(None));
        assert_eq!(
            budget.start_trial(),
            Err(BudgetExpired {
                kind: BudgetKind::TrialBudget,
                cut: None
            })
        );
        assert_eq!(budget.trials_started(), 2);
    }

    #[test]
    fn the_stack_grace_is_the_lesser_of_two_minutes_and_five_percent() {
        let grace = |time: TimeBudget| Budget::new(time, TrialBudget::Off).stack_grace();
        assert_eq!(
            grace(TimeBudget::Limit(Duration::from_secs(1800))),
            Duration::from_secs(90)
        );
        assert_eq!(
            grace(TimeBudget::Limit(Duration::from_secs(7200))),
            STACK_GRACE_CAP
        );
        assert_eq!(grace(TimeBudget::Off), STACK_GRACE_CAP);
    }

    #[test]
    fn a_cut_trial_is_named_and_spends_the_time_budget() {
        let budget = Budget::new(TimeBudget::Limit(Duration::from_secs(60)), TrialBudget::Off);
        let cut = budget.cut("bisect_round_007");
        assert_eq!(cut.kind, BudgetKind::TimeBudget);
        assert_eq!(
            cut.to_string(),
            "hermit analyze reached its --time-budget during bisect_round_007, whose outcome is \
             not counted"
        );
        assert_eq!(budget.expired(), Some(BudgetKind::TimeBudget));
    }
}
