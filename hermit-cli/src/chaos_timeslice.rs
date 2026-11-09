/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The maximum timeslice that `hermit run --chaos --chaos-seeded-timeslice`
//! draws from the scheduler seed.
//!
//! Chaos mode ends each timeslice after a random number of retired
//! conditional branches, drawn from an exponential distribution whose mean
//! parameter is the maximum timeslice and capped at the maximum, so slices
//! average about 0.63 of it (1 - 1/e, before rounding to whole branches).
//! Which maximum finds a race depends on the race: one whose window is a few
//! dozen branches long needs slices of about a hundred branches, while one
//! whose second thread must run far into its own work before the conflicting
//! access needs slices that let it get there. A fixed maximum suits one of
//! them. Drawing the maximum once per scheduler seed spreads a sweep of seeds
//! across five orders of magnitude, so it covers both without a guessed value.
//!
//! The draw is not uniform over those orders of magnitude. The races measured
//! needed either the shortest slices or the longest: a 46-branch window in a
//! multithreaded C++ test binary failed in 9 of 32 seeds at 1000 ns and in
//! none of 32 at 1e4, 1e5, 1e6 or the default 2e8 ns (1e7 was not measured),
//! while races whose other thread must first run tens of thousands of branches
//! need 1000000 ns or more, and none of the races measured needed the decades
//! in between. So
//! [`SEEDED_MAX_TIMESLICE_PIECES`] gives two in five seeds 1000 to 2000 ns (100
//! to 200 branches per slice), two in five 1000000 to 200000000 ns, and keeps
//! one in five for the decades between them.
//!
//! The draw is a pure function of the scheduler seed (and of the one-RCB floor,
//! which only a large clock multiplier or slowdown factor raises above the
//! range: there every seed draws the floor, and a run that draws nothing keeps
//! the default raised to its own floor, [`seeded_fallback_max_timeslice`]). It
//! seeds a generator of its own and takes nothing from the streams Detcore
//! already derives from that seed (each thread's chaos generator, the post-fork
//! generator, the per-thread slowdown factors), so those are seeded exactly as
//! before. Its result is an ordinary `--max-timeslice` value: Detcore receives
//! the configuration of a run given that value explicitly.

use std::num::NonZeroU64;

use rand::RngExt as _;
use rand::SeedableRng;
use rand_pcg::Pcg64Mcg;

/// The smallest drawn maximum, in virtual nanoseconds: 100 retired
/// conditional branches at the default clock multiplier.
pub const SEEDED_MAX_TIMESLICE_MIN_NS: u64 = 1_000;

/// The largest drawn maximum, in virtual nanoseconds: the `--max-timeslice`
/// default. No draw exceeds it unless one RCB is longer, and then the draw is
/// one RCB. A replay of a recorded chaos run keeps the larger of the two
/// ([`seeded_fallback_max_timeslice`]), so it is never bounded more tightly
/// than the recording was.
pub const SEEDED_MAX_TIMESLICE_MAX_NS: u64 = 200_000_000;

/// [`SEEDED_MAX_TIMESLICE_MAX_NS`] as a maximum: the `--max-timeslice`
/// default.
pub const DEFAULT_MAX_TIMESLICE: NonZeroU64 = NonZeroU64::new(SEEDED_MAX_TIMESLICE_MAX_NS).unwrap();

/// The maximum timeslice `--chaos-seeded-timeslice` stands in for where it
/// draws nothing (a run that replays preemptions or a schedule, or one
/// without chaos, such as `hermit analyze`'s baseline): the `--max-timeslice`
/// default, raised to `minimum_ns` (one RCB at the run's clock multiplier and
/// slowdown, [`detcore::Config::minimum_max_timeslice_nanos`]) where that is
/// longer, as every draw is. A replay under the same clock multiplier and
/// slowdown settings therefore keeps what its recording drew. A run without
/// chaos raises the default to its own floor, which leaves out the chaos
/// slowdown factor. Without the raise, a recording whose one RCB is longer
/// than the default drew one RCB, and its replay or baseline was then refused
/// for a maximum below one RCB.
pub fn seeded_fallback_max_timeslice(minimum_ns: u64) -> NonZeroU64 {
    NonZeroU64::new(SEEDED_MAX_TIMESLICE_MAX_NS.max(minimum_ns))
        .expect("the --max-timeslice default is positive")
}

/// The pieces of the drawn maximum's distribution, in order: each is
/// `(share, low, high)`, the share of seeds whose maximum falls in
/// [`low`, `high`] virtual nanoseconds, log-uniformly within it. The pieces
/// are contiguous, run from [`SEEDED_MAX_TIMESLICE_MIN_NS`] to
/// [`SEEDED_MAX_TIMESLICE_MAX_NS`], and their shares sum to one.
pub const SEEDED_MAX_TIMESLICE_PIECES: [(f64, u64, u64); 3] = [
    (0.4, SEEDED_MAX_TIMESLICE_MIN_NS, 2_000),
    (0.2, 2_000, 1_000_000),
    (0.4, 1_000_000, SEEDED_MAX_TIMESLICE_MAX_NS),
];

/// Keeps this stream apart from the other streams seeded from the scheduler
/// seed (`post_fork_prng`'s "postfork", the slowdown factors' "slowdown").
const SEEDED_TIMESLICE_SALT: u64 = 0x7469_6d65_736c_6963; // "timeslic"

/// The maximum timeslice, in virtual nanoseconds, drawn for `sched_seed`: one
/// uniform number `u` from the seed's own generator, mapped through the
/// quantile function of [`SEEDED_MAX_TIMESLICE_PIECES`] (the piece whose
/// cumulative share passes `u`, log-uniformly within it), rounded to a whole
/// nanosecond, then raised to `minimum_ns` (one RCB at the run's clock
/// multiplier and slowdown, [`detcore::Config::minimum_max_timeslice_nanos`])
/// if it falls below it. The mapping is monotone in `u`.
pub fn seeded_max_timeslice(sched_seed: u64, minimum_ns: u64) -> NonZeroU64 {
    let mut prng = Pcg64Mcg::seed_from_u64(sched_seed ^ SEEDED_TIMESLICE_SALT);
    let u: f64 = prng.random();
    let drawn = quantile(u).round() as u64;
    let drawn = drawn
        .clamp(SEEDED_MAX_TIMESLICE_MIN_NS, SEEDED_MAX_TIMESLICE_MAX_NS)
        .max(minimum_ns);
    NonZeroU64::new(drawn).expect("the drawn maximum is at least SEEDED_MAX_TIMESLICE_MIN_NS")
}

/// The maximum, in virtual nanoseconds, at cumulative probability `u` in
/// [0, 1) of [`SEEDED_MAX_TIMESLICE_PIECES`].
fn quantile(u: f64) -> f64 {
    let mut below = 0.0;
    for (share, low, high) in SEEDED_MAX_TIMESLICE_PIECES {
        if u < below + share {
            let t = (u - below) / share;
            let (low, high) = ((low as f64).ln(), (high as f64).ln());
            return (low + t * (high - low)).exp();
        }
        below += share;
    }
    // Rounding can leave the shares' sum a hair below one.
    SEEDED_MAX_TIMESLICE_MAX_NS as f64
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    #[test]
    fn the_largest_draw_is_the_max_timeslice_default() {
        let default = detcore::Config::parse_from(["hermit"]).max_timeslice;
        assert_eq!(default, Some(DEFAULT_MAX_TIMESLICE));
    }

    #[test]
    fn a_seed_always_draws_the_same_value() {
        for seed in [0, 1, 7, u64::MAX] {
            assert_eq!(
                seeded_max_timeslice(seed, 10),
                seeded_max_timeslice(seed, 10)
            );
        }
        // Pinned, so a change to the draw (which changes every seed's
        // schedule) is a visible decision rather than an accident.
        let pinned: Vec<u64> = (0..4)
            .map(|seed| seeded_max_timeslice(seed, 10).get())
            .collect();
        assert_eq!(pinned, [1_113, 5_103_600, 12_779_429, 219_666]);
    }

    #[test]
    fn the_pieces_tile_the_range_and_their_shares_sum_to_one() {
        let pieces = SEEDED_MAX_TIMESLICE_PIECES;
        assert_eq!(pieces[0].1, SEEDED_MAX_TIMESLICE_MIN_NS);
        assert_eq!(pieces[pieces.len() - 1].2, SEEDED_MAX_TIMESLICE_MAX_NS);
        for pair in pieces.windows(2) {
            assert_eq!(pair[0].2, pair[1].1, "{pair:?}");
        }
        let total: f64 = pieces.iter().map(|(share, _, _)| share).sum();
        assert!((total - 1.0).abs() < 1e-12, "{total}");
        // The quantile function is monotone and meets each piece's bounds.
        let mut last = 0.0;
        for i in 0..1000 {
            let v = quantile(f64::from(i) / 1000.0);
            assert!(v >= last, "quantile decreases at {i}: {last} > {v}");
            last = v;
        }
        assert!((quantile(0.0) - SEEDED_MAX_TIMESLICE_MIN_NS as f64).abs() < 1e-6);
        assert!((quantile(0.4) - 2_000.0).abs() < 1e-6);
        assert!((quantile(0.6) - 1_000_000.0).abs() < 1e-3);
    }

    /// 4096 seeds: every draw is in range, each piece holds its share (within
    /// 10%) of the draws, and within the last piece each decade holds its
    /// log-uniform share (within 20%).
    #[test]
    fn draws_follow_the_pieces() {
        const SEEDS: u64 = 4096;
        let mut pieces = [0u64; SEEDED_MAX_TIMESLICE_PIECES.len()];
        let mut decades = [0u64; 2];
        for seed in 0..SEEDS {
            let drawn = seeded_max_timeslice(seed, 10).get();
            assert!(
                (SEEDED_MAX_TIMESLICE_MIN_NS..=SEEDED_MAX_TIMESLICE_MAX_NS).contains(&drawn),
                "seed {seed} drew {drawn}"
            );
            let piece = SEEDED_MAX_TIMESLICE_PIECES
                .iter()
                .position(|(_, _, high)| drawn <= *high)
                .unwrap();
            pieces[piece] += 1;
            if (1_000_000..10_000_000).contains(&drawn) {
                decades[0] += 1;
            } else if (10_000_000..100_000_000).contains(&drawn) {
                decades[1] += 1;
            }
        }
        for (count, (share, low, high)) in pieces.iter().zip(SEEDED_MAX_TIMESLICE_PIECES) {
            let expected = share * SEEDS as f64;
            let ratio = *count as f64 / expected;
            assert!(
                (0.9..1.1).contains(&ratio),
                "[{low}, {high}] holds {count} of {SEEDS} draws, expected about {expected:.0}"
            );
        }
        let last = SEEDED_MAX_TIMESLICE_PIECES[2];
        let per_decade =
            last.0 * SEEDS as f64 / ((last.2 as f64).log10() - (last.1 as f64).log10());
        for count in decades {
            let ratio = count as f64 / per_decade;
            assert!(
                (0.8..1.2).contains(&ratio),
                "a decade of the last piece holds {count} draws, expected about {per_decade:.0}"
            );
        }
    }

    /// Where one RCB is longer than the default maximum (a clock multiplier
    /// of 30000000 makes it 300000000 virtual ns), every seed draws one RCB,
    /// and a run that draws nothing keeps the same value rather than the
    /// default below it. Elsewhere the fallback is the default.
    #[test]
    fn the_fallback_is_at_least_one_rcb() {
        assert_eq!(seeded_fallback_max_timeslice(10), DEFAULT_MAX_TIMESLICE);
        assert_eq!(
            seeded_fallback_max_timeslice(SEEDED_MAX_TIMESLICE_MAX_NS),
            DEFAULT_MAX_TIMESLICE
        );
        let one_rcb = 300_000_000;
        assert_eq!(seeded_fallback_max_timeslice(one_rcb).get(), one_rcb);
        for seed in 0..64 {
            assert_eq!(
                seeded_max_timeslice(seed, one_rcb),
                seeded_fallback_max_timeslice(one_rcb),
                "seed {seed}"
            );
        }
    }

    #[test]
    fn a_draw_below_one_rcb_is_raised_to_it() {
        // A clock multiplier of 1000 makes one RCB 10,000 virtual ns.
        for seed in 0..256 {
            assert!(seeded_max_timeslice(seed, 10_000).get() >= 10_000);
            let unclamped = seeded_max_timeslice(seed, 10).get();
            if unclamped >= 10_000 {
                assert_eq!(seeded_max_timeslice(seed, 10_000).get(), unclamped);
            }
        }
    }
}
