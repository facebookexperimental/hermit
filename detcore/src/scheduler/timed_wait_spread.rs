/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! `--chaos-timed-wait-spread`: seeded wake thresholds for timed waits.
//!
//! Chaos reorders runnable threads, but a sleeping thread wakes when the
//! global clock reaches its deadline, and the clock advances only with the
//! other threads' work and the scheduler's turn charges. The ratio between a
//! sleep and the work done meanwhile is therefore fixed by the program, and a
//! race ordered by that ratio never changes order. This module draws, for
//! each timed wait, a factor `f` log-uniformly from `[1/R, R]` and queues the
//! wait at the wake threshold `start + f * duration` instead of at its
//! deadline. A threshold at or past the deadline oversleeps, which Linux
//! allows. A threshold before the deadline is promised its deadline
//! (`TimedEvents::insert_spread`): when it comes due, the scheduler advances
//! the global clock to the deadline before the waiter runs, as if every other
//! thread had been descheduled for the rest of the wait, so the waiter never
//! observes less than the duration it asked for.

use std::collections::BTreeMap;

use rand::Rng as _;
use rand::SeedableRng;
use rand_pcg::Pcg64Mcg;

use crate::types::DetTid;
use crate::types::LogicalTime;

/// The factor of a thread's `index`-th spread wait: `spread^t` for a `t`
/// uniform in `[-1, 1)` (so the factor is log-uniform in `[1/spread,
/// spread)`), a pure function of the scheduler seed, the stable thread id and
/// the index, as `chaos_per_thread_slowdown_factor` keys a thread's slowdown.
/// Keying by thread and per-thread index, not by a global draw order, keeps a
/// wait's factor when other threads' events move.
///
/// The factor sets a wake time a replay recomputes from the recorded spread
/// and seed, so it must come out bit-identical on every host and Rust
/// version. It therefore uses no transcendental function: Rust documents the
/// precision of `f64::powf`, `exp` and `ln` as unspecified across platforms
/// and versions. `t` comes from the generator's integer output, not from
/// `rand`'s float conversion, and `spread^|t|` is a product of repeated square
/// roots of `spread` selected by the bits of `|t|`: square root,
/// multiplication and division are IEEE 754 basic operations, correctly
/// rounded and so identical everywhere, and they run in a fixed order.
pub(crate) fn timed_wait_factor(seed: u64, dettid: DetTid, index: u64, spread: f64) -> f64 {
    if spread <= 1.0 {
        return 1.0;
    }
    const SPREAD_SALT: u64 = 0x7469_6d65_6477_6169; // "timedwai"
    const INDEX_GOLDEN: u64 = 0xbf58_476d_1ce4_e5b9;
    let mixed = seed
        ^ SPREAD_SALT
        ^ ((dettid.as_raw() as u32 as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15))
        ^ index.wrapping_mul(INDEX_GOLDEN);
    // `u` in `0..2^53`, so `t = (2u - 2^53) / 2^53` in `[-1, 1)` and the
    // magnitude `|t| = m / 2^53` with `m` in `0..=2^53`.
    let u = Pcg64Mcg::seed_from_u64(mixed).next_u64() >> 11;
    let (m, shorten) = if u >= 1 << 52 {
        (2 * u - (1 << 53), false)
    } else {
        ((1 << 53) - 2 * u, true)
    };
    let magnitude = spread_power(spread, m);
    if shorten { 1.0 / magnitude } else { magnitude }
}

/// `spread^(m / 2^53)` for `m` in `0..=2^53`, from IEEE 754 basic operations
/// only: bit `k` of `m` contributes `spread^(2^(k - 53))`, the `53 - k`-th
/// repeated square root of `spread`, multiplied in from the lowest bit up.
fn spread_power(spread: f64, m: u64) -> f64 {
    // roots[j] = spread^(2^-j), for j in 0..=53.
    let mut roots = [spread; 54];
    for j in 1..roots.len() {
        roots[j] = roots[j - 1].sqrt();
    }
    let mut product = 1.0;
    for k in 0..=53u32 {
        if m >> k & 1 == 1 {
            product *= roots[53 - k as usize];
        }
    }
    product
}

/// The draws of one run: the configured spread, the seed, and how many spread
/// waits each thread has filed.
#[derive(Debug, Clone)]
pub(crate) struct TimedWaitSpread {
    spread: f64,
    seed: u64,
    filed: BTreeMap<DetTid, u64>,
}

impl TimedWaitSpread {
    pub(crate) fn new(spread: f64, seed: u64) -> Self {
        Self {
            spread,
            seed,
            filed: BTreeMap::new(),
        }
    }

    /// The wake threshold of a timed wait `dettid` files at committed time
    /// `now` with deadline `deadline`, with the factor it was drawn. A wait
    /// with no finite future deadline (already due, or indefinite) keeps its
    /// deadline, draws nothing and does not advance the thread's index. The
    /// threshold is at least one nanosecond after `now` and before the
    /// indefinite sentinel, so it is always a real, future deadline.
    pub(crate) fn wake_threshold(
        &mut self,
        dettid: DetTid,
        now: LogicalTime,
        deadline: LogicalTime,
    ) -> (LogicalTime, f64) {
        if deadline <= now || deadline.is_indefinite() || now.as_nanos() >= u64::MAX - 2 {
            return (deadline, 1.0);
        }
        let index = self.filed.entry(dettid).or_default();
        let factor = timed_wait_factor(self.seed, dettid, *index, self.spread);
        *index += 1;
        let duration = (deadline.as_nanos() - now.as_nanos()) as f64;
        // Clamp in integers: near 2^64 an f64 step is 2048 ns, so a bound in
        // floating point could round up to the indefinite sentinel.
        let latest = LogicalTime::MAX.as_nanos() - 1 - now.as_nanos();
        let scaled = ((duration * factor).round() as u64).clamp(1, latest);
        (now + LogicalTime::from_nanos(scaled), factor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `timed_wait_factor(7, 5, 0..4, 10.0)`, bit for bit.
    const PINNED_FACTOR_BITS: [u64; 4] = [
        0x3fc2_db9b_b60e_f9f3,
        0x400d_59c7_4804_2813,
        0x3fe2_934e_2e7d_3e11,
        0x3ff1_c1ce_2371_53be,
    ];

    fn at(ns: u64) -> LogicalTime {
        LogicalTime::from_nanos(ns)
    }

    #[test]
    fn factors_are_log_uniform_within_the_spread_and_pure() {
        let spread = 100.0;
        let mut below = 0;
        for raw in 1..=8 {
            for index in 0..256 {
                let tid = DetTid::from_raw(raw);
                let factor = timed_wait_factor(7, tid, index, spread);
                assert_eq!(factor, timed_wait_factor(7, tid, index, spread));
                assert!(
                    (1.0 / spread..=spread).contains(&factor),
                    "{factor} out of range"
                );
                below += usize::from(factor < 1.0);
            }
        }
        // Log-uniform: about half the draws shorten. 2048 draws, so a share
        // outside 40%-60% would be more than eight standard deviations out.
        assert!((819..=1229).contains(&below), "{below} of 2048 below one");
        assert_ne!(
            timed_wait_factor(7, DetTid::from_raw(1), 0, spread),
            timed_wait_factor(8, DetTid::from_raw(1), 0, spread),
            "the seed changes the draw"
        );
        assert_eq!(timed_wait_factor(7, DetTid::from_raw(1), 3, 1.0), 1.0);
    }

    /// The factor is `spread^t` to within a few ulps of the libm result, its
    /// ends are exact, and it is computed without libm: these exact bits are
    /// what every host and Rust version must produce, since a replay
    /// recomputes them from the record (pinned so a change to the arithmetic,
    /// the generator or the keying shows up as a failure, not as replays that
    /// drift by a nanosecond).
    #[test]
    fn factors_match_the_power_and_are_pinned_bit_for_bit() {
        for m in [0u64, 1, 1 << 20, 1 << 52, (1 << 53) - 1, 1 << 53] {
            let exact = 10f64.powf(m as f64 / (1u64 << 53) as f64);
            let ours = spread_power(10.0, m);
            assert!(
                ((ours - exact) / exact).abs() < 64.0 * f64::EPSILON,
                "m {m}: {ours} against {exact}"
            );
        }
        assert_eq!(spread_power(10.0, 0), 1.0);
        assert_eq!(spread_power(10.0, 1 << 53), 10.0);
        let pinned: Vec<u64> = (0..4)
            .map(|index| timed_wait_factor(7, DetTid::from_raw(5), index, 10.0).to_bits())
            .collect();
        assert_eq!(pinned, PINNED_FACTOR_BITS, "{pinned:#x?}");
    }

    #[test]
    fn thresholds_scale_the_remaining_duration_and_count_per_thread() {
        let mut spread = TimedWaitSpread::new(10.0, 3);
        let tid = DetTid::from_raw(5);
        let other = DetTid::from_raw(6);
        let (wake, factor) = spread.wake_threshold(tid, at(1_000), at(51_000));
        assert_eq!(
            wake,
            at(1_000 + (50_000.0 * factor).round() as u64),
            "start + factor * duration"
        );
        assert_eq!(factor, timed_wait_factor(3, tid, 0, 10.0));
        // Another thread's draw does not advance this thread's index.
        spread.wake_threshold(other, at(1_000), at(2_000));
        let (_, second) = spread.wake_threshold(tid, at(1_000), at(2_000));
        assert_eq!(second, timed_wait_factor(3, tid, 1, 10.0));
        // Due, indefinite and past deadlines draw nothing.
        assert_eq!(
            spread.wake_threshold(tid, at(5_000), at(5_000)),
            (at(5_000), 1.0)
        );
        assert_eq!(
            spread.wake_threshold(tid, at(5_000), LogicalTime::INDEFINITE),
            (LogicalTime::INDEFINITE, 1.0)
        );
        let (_, third) = spread.wake_threshold(tid, at(1_000), at(2_000));
        assert_eq!(third, timed_wait_factor(3, tid, 2, 10.0));
    }

    #[test]
    fn thresholds_stay_future_and_finite_at_the_extremes() {
        let mut spread = TimedWaitSpread::new(1_000_000.0, 11);
        let tid = DetTid::from_raw(9);
        for _ in 0..64 {
            let (wake, _) = spread.wake_threshold(tid, at(1_000), at(1_001));
            assert!(
                wake > at(1_000),
                "a one-nanosecond wait stays in the future"
            );
            let now = at(u64::MAX - 1_000_000);
            let (wake, _) = spread.wake_threshold(tid, now, at(u64::MAX - 500_000));
            assert!(wake > now && !wake.is_indefinite());
        }
        // An absolute clock near 1.8e18 ns and a far-future timeout: a
        // lengthened threshold past the clock's range stops just short of
        // the indefinite sentinel, so the timeout still fires.
        let mut spread = TimedWaitSpread::new(1_000.0, 5);
        let now = at(1_800_000_000_000_000_000);
        for _ in 0..256 {
            let (wake, _) = spread.wake_threshold(tid, now, now + at(400_000_000_000_000_000));
            assert!(wake > now && !wake.is_indefinite(), "{wake:?}");
        }
    }
}
