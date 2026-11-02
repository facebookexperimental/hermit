/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The CPU a sequentialized run is pinned to.
//!
//! A run with a `max_timeslice` pins its tracer, its notifier threads and
//! every guest thread to one CPU for the whole run, because its retired-branch
//! counts must not see a migration (`RunOpts::pin_threads`). Every ptrace stop
//! then costs several wakeups on that one CPU: the guest stops, its notifier
//! thread wakes, the tracer wakes, and the tracer resumes the guest.
//!
//! So the CPU matters, and a CPU-bound task already on it is the worst case.
//! Linux shares a CPU between cgroups by weight, and it scales a cgroup's
//! weight on one CPU by about the share of the cgroup's load that is on that
//! CPU (`calc_group_shares`, kernel/sched/fair.c). A run inside a cgroup that
//! is busy on many other CPUs, as a test runner's is, therefore has almost no
//! weight on its own CPU. Beside a CPU-bound task from a cgroup whose load is
//! concentrated there, each of the run's wakeups waits for that task, and the
//! run does the same work hundreds of times slower
//! (https://github.com/rrnewton/hermit/issues/4824). Measured on a 316-CPU
//! host: one strict verify of an epoll guest took 1.5 s on an idle CPU and
//! 15.5 s on a CPU running a busy loop from another cgroup, while the run's
//! own cgroup kept 24 busy loops on other CPUs. Under a loaded test runner,
//! runs were 170 to 1,400 times slower and hit test deadlines.
//!
//! A uniformly random CPU lands on such a task as often as there are busy
//! CPUs. Instead, a run samples the allowed CPUs' busy time from `/proc/stat`
//! over [`SAMPLE_WINDOW`] and draws uniformly from the ones that were idle, or,
//! when none was, from the least busy. Runs that sample at the same moment
//! draw from the same candidates: every idle CPU while there are some, but
//! often a single CPU when there are none, since at 10 ms ticks the least busy
//! fraction is rarely tied. Such runs then share that CPU, which costs speed,
//! never determinism.
//!
//! Among the candidates, a run passes over the CPUs a PMU is bound to while
//! any other candidate remains. Some PMUs open their events on a few CPUs,
//! named by `/sys/bus/event_source/devices/<pmu>/cpumask` (on AMD, `amd_l3`
//! on one CPU per L3 and `amd_df` and `amd_umc` on CPU 0), and ptrace stops
//! cost several times as much CPU there
//! (https://github.com/rrnewton/hermit/issues/3265: 7.6-7.9x for a
//! ptrace-stop-heavy cell; 2.5x for the epoll verify above on CPU 0). Because
//! validation and `bin/safehermit` keep runs off those CPUs, they are often
//! idle, and an idle draw would otherwise favour them. The set is derived as
//! the validation launcher derives the CPUs it excludes: the union of every
//! PMU `cpumask` that names at most a quarter of the online CPUs, since a
//! wider one belongs to a per-core PMU. Where the launcher would skip a PMU
//! entry or cpumask that is not what sysfs presents, this passes nothing over.
//!
//! Not handled: a CPU-bound task that reaches the CPU after the sample (load
//! balancing moves one there, or a job starts or pins itself there) still
//! slows the run for the rest of its life, and nothing reports it. Moving the
//! run then would migrate its guest threads, which is what pinning exists to
//! prevent. Nor can a sample tell a CPU-bound task from the run's own
//! launcher: with few allowed CPUs, the launcher can keep the only other CPU
//! busy for the whole window too, and the draw then takes either.
//!
//! The choice is host placement only, and it was already a random draw per
//! run. The guest sees its own virtual CPU through getcpu, sched_getaffinity,
//! `/proc/self/status` and RDTSCP's TSC_AUX. As before, only a guest that
//! executes RDPID although CPUID hides it, or that uses rseq under the
//! compatibility opt-out, can read the host CPU number.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fmt;
use std::fs;
use std::path::Path;
use std::thread;
use std::time::Duration;

/// How long the allowed CPUs' busy time is sampled before a run takes one.
/// `/proc/stat` counts in 1/USER_HZ ticks, 10 ms on Linux, so this is about
/// five ticks per CPU. A CPU that was busy for 10 ms or more of the window
/// shows at least one busy tick, so a CPU-bound task is always seen; the
/// window is long enough that one which runs half of every 100 ms is too.
pub(crate) const SAMPLE_WINDOW: Duration = Duration::from_millis(50);

/// A CPU is idle when at most this fraction of the sample was busy, expressed
/// as a denominator: one tick in five. Each read rounds to whole ticks, so an
/// idle CPU can show one busy tick, while a CPU-bound task shows four or five.
const IDLE_DENOMINATOR: u64 = 5;

/// The widest PMU `cpumask` that still counts as bound to a few CPUs, as a
/// share of the online CPUs: one quarter, the validation launcher's rule
/// (ci-hub/validate/cpu_placement.py in dev-hermit). A wider mask, such as
/// `power_core=0-157` on a 316-CPU host, belongs to a per-core PMU.
const PMU_BOUND_SHARE_DENOMINATOR: usize = 4;

/// The highest CPU number Linux allows (CONFIG_NR_CPUS is at most 8192). A
/// CPU list naming a higher one is malformed, and is refused before its range
/// is expanded.
const MAX_CPU_NUMBER: usize = 8191;

/// One CPU's cumulative time from `/proc/stat`, in USER_HZ ticks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CpuTime {
    /// idle plus iowait.
    idle: u64,
    /// user, nice, system, idle, iowait, irq, softirq and steal. The guest
    /// columns are already counted in user and nice, so they are not added.
    total: u64,
}

/// Per-CPU times from one reading of `/proc/stat`.
pub(crate) type Reading = BTreeMap<usize, CpuTime>;

/// What a run's choice is made from.
#[derive(Clone, Debug)]
pub(crate) struct Observation {
    /// `/proc/stat` before [`SAMPLE_WINDOW`].
    pub(crate) before: Reading,
    /// `/proc/stat` after it.
    pub(crate) after: Reading,
    /// The CPUs a PMU is bound to (see the module documentation).
    pub(crate) pmu_bound: BTreeSet<usize>,
}

/// How the CPUs a PMU is bound to figured in a draw.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Pmu {
    /// No candidate was PMU-bound.
    NoneBound,
    /// This many PMU-bound candidates were passed over for the others.
    PassedOver(usize),
    /// Every candidate was PMU-bound, so the run takes one of them.
    AllBound,
}

/// Why a run got the CPU it got, for the log line that names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Basis {
    /// The allowed set has one CPU.
    OnlyAllowed,
    /// Drawn from `candidates` of the `sampled` allowed CPUs, all idle across
    /// the sample.
    Idle {
        candidates: usize,
        sampled: usize,
        pmu: Pmu,
    },
    /// No sampled CPU was idle: drawn from the `candidates` least busy (ties
    /// all kept).
    LeastBusy {
        candidates: usize,
        sampled: usize,
        pmu: Pmu,
    },
    /// `/proc/stat` was unreadable or covered none of the allowed CPUs: drawn
    /// from every allowed CPU.
    Unsampled,
}

impl fmt::Display for Pmu {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Pmu::NoneBound => write!(f, "none PMU-bound"),
            Pmu::PassedOver(count) => write!(f, "passing over {count} PMU-bound"),
            Pmu::AllBound => write!(f, "all of them PMU-bound"),
        }
    }
}

impl fmt::Display for Basis {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let window = SAMPLE_WINDOW.as_millis();
        match self {
            Basis::OnlyAllowed => write!(f, "the only allowed CPU"),
            Basis::Idle {
                candidates,
                sampled,
                pmu,
            } => write!(
                f,
                "one of {candidates} of {sampled} allowed CPUs idle over {window} ms, {pmu}"
            ),
            Basis::LeastBusy {
                candidates,
                sampled,
                pmu,
            } => write!(
                f,
                "none of {sampled} allowed CPUs idle over {window} ms; one of the {candidates} \
                 least busy, {pmu}"
            ),
            Basis::Unsampled => {
                write!(f, "drawn from every allowed CPU; /proc/stat gave no sample")
            }
        }
    }
}

/// Choose the CPU a run is pinned to from `allowed`, the tracer's affinity
/// mask, and say why. `None` only when `allowed` is empty.
pub(crate) fn choose(allowed: &[usize]) -> Option<(usize, Basis)> {
    choose_with(allowed, observe)
}

/// [`choose`] with the observation made by `observe`, which tests replace.
pub(crate) fn choose_with(
    allowed: &[usize],
    observe: impl FnOnce(&[usize]) -> Option<Observation>,
) -> Option<(usize, Basis)> {
    match allowed {
        [] => None,
        [only] => Some((*only, Basis::OnlyAllowed)),
        _ => {
            let (candidates, basis) = observe(allowed)
                .and_then(|observation| candidates(allowed, &observation))
                .unwrap_or_else(|| (allowed.to_vec(), Basis::Unsampled));
            Some((candidates[rand::random_range(0..candidates.len())], basis))
        }
    }
}

/// Two readings of `/proc/stat`, [`SAMPLE_WINDOW`] apart, and the PMU-bound
/// CPUs.
fn observe(allowed: &[usize]) -> Option<Observation> {
    let read = || {
        fs::read_to_string("/proc/stat")
            .map_err(|error| tracing::debug!("cannot sample CPU load from /proc/stat: {error}"))
            .ok()
            .map(|text| parse_proc_stat(&text, allowed))
    };
    let before = read()?;
    thread::sleep(SAMPLE_WINDOW);
    let after = read()?;
    Some(Observation {
        before,
        after,
        pmu_bound: pmu_bound_cpus(Path::new("/sys")),
    })
}

/// The per-CPU lines of `/proc/stat` for the CPUs in `allowed`:
/// `cpuN user nice system idle iowait irq softirq steal guest guest_nice`,
/// where kernels before 2.6.11 stop earlier. The first line, `cpu`, sums
/// every CPU and has no number, so it is skipped.
fn parse_proc_stat(text: &str, allowed: &[usize]) -> Reading {
    let mut times = BTreeMap::new();
    for line in text.lines() {
        let Some(rest) = line.strip_prefix("cpu") else {
            continue;
        };
        if !rest.starts_with(|c: char| c.is_ascii_digit()) {
            continue;
        }
        let mut fields = rest.split_ascii_whitespace();
        let Some(Ok(cpu)) = fields.next().map(str::parse::<usize>) else {
            continue;
        };
        if !allowed.contains(&cpu) {
            continue;
        }
        let ticks: Vec<u64> = fields
            .take(8)
            .map_while(|field| field.parse().ok())
            .collect();
        // user, nice, system and idle are the four columns every kernel has.
        if ticks.len() < 4 {
            continue;
        }
        let iowait = ticks.get(4).copied().unwrap_or(0);
        times.insert(
            cpu,
            CpuTime {
                idle: ticks[3] + iowait,
                total: ticks.iter().sum(),
            },
        );
    }
    times
}

/// The CPUs a PMU is bound to, read under `sys` (normally `/sys`): the union
/// of every `bus/event_source/devices/<pmu>/cpumask` that names at most a
/// quarter of the online CPUs (`devices/system/cpu/online`). A PMU with no
/// cpumask names no CPUs. Anything else that is not what sysfs presents (an
/// unreadable directory, a PMU entry that does not resolve to a directory, a
/// cpumask that does not resolve to a regular file, a malformed list) makes
/// the result empty, so nothing is passed over: a partial set from a sysfs
/// that cannot be read is not trusted.
fn pmu_bound_cpus(sys: &Path) -> BTreeSet<usize> {
    let read_list = |path: &Path| {
        fs::read_to_string(path)
            .ok()
            .and_then(|text| parse_cpu_list(&text))
    };
    let Some(online) = read_list(&sys.join("devices/system/cpu/online")) else {
        return BTreeSet::new();
    };
    let Ok(entries) = fs::read_dir(sys.join("bus/event_source/devices")) else {
        return BTreeSet::new();
    };
    let mut masks = Vec::new();
    for entry in entries {
        let Ok(entry) = entry else {
            return BTreeSet::new();
        };
        if !fs::metadata(entry.path()).is_ok_and(|metadata| metadata.is_dir()) {
            return BTreeSet::new();
        }
        // Absent means no CPUs. Present, even as a symlink that resolves to
        // nothing, it must resolve to a regular file.
        let path = entry.path().join("cpumask");
        match fs::symlink_metadata(&path) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return BTreeSet::new(),
        }
        if !fs::metadata(&path).is_ok_and(|metadata| metadata.is_file()) {
            return BTreeSet::new();
        }
        let Some(mask) = read_list(&path) else {
            return BTreeSet::new();
        };
        masks.push(mask);
    }
    bound_cpus(&online, masks)
}

/// The union of the `masks` that name at most a quarter of `online`, each
/// limited to `online`.
fn bound_cpus(
    online: &BTreeSet<usize>,
    masks: impl IntoIterator<Item = BTreeSet<usize>>,
) -> BTreeSet<usize> {
    let mut bound = BTreeSet::new();
    for mask in masks {
        let mask: BTreeSet<usize> = mask.intersection(online).copied().collect();
        if mask.len() * PMU_BOUND_SHARE_DENOMINATOR <= online.len() {
            bound.extend(mask);
        }
    }
    bound
}

/// Parse a kernel CPU list such as `0,16-19\n`. `None` if it is malformed or
/// names a CPU above [`MAX_CPU_NUMBER`].
fn parse_cpu_list(text: &str) -> Option<BTreeSet<usize>> {
    let text = text.trim();
    let mut cpus = BTreeSet::new();
    if text.is_empty() {
        return Some(cpus);
    }
    for part in text.split(',') {
        let (low, high) = part.split_once('-').unwrap_or((part, part));
        let number = |value: &str| {
            (!value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()))
                .then(|| value.parse::<usize>().ok())
                .flatten()
        };
        let (low, high) = (number(low)?, number(high)?);
        if high < low || high > MAX_CPU_NUMBER {
            return None;
        }
        cpus.extend(low..=high);
    }
    Some(cpus)
}

/// The allowed CPUs to draw from: the idle ones, or the least busy when none
/// was idle, passing over PMU-bound CPUs while any other remains. `None` when
/// the readings cover none of `allowed`.
fn candidates(allowed: &[usize], observation: &Observation) -> Option<(Vec<usize>, Basis)> {
    // (cpu, busy ticks, total ticks) over the sample.
    let busy: Vec<(usize, u64, u64)> = allowed
        .iter()
        .filter_map(|cpu| {
            let before = observation.before.get(cpu)?;
            let after = observation.after.get(cpu)?;
            let total = after.total.checked_sub(before.total)?;
            let idle = after.idle.checked_sub(before.idle)?;
            (total > 0).then(|| (*cpu, total.saturating_sub(idle), total))
        })
        .collect();
    if busy.is_empty() {
        return None;
    }
    let sampled = busy.len();
    let idle: Vec<usize> = busy
        .iter()
        .filter(|(_, busy, total)| busy * IDLE_DENOMINATOR <= *total)
        .map(|(cpu, ..)| *cpu)
        .collect();
    let (pool, found_idle) = if idle.is_empty() {
        // The least busy fraction, every CPU at it kept; busy_a/total_a and
        // busy_b/total_b compare by cross-multiplying. Not widened by a tick:
        // with few allowed CPUs, the run's own launcher can make the only
        // other CPU look four ticks in five busy, and a widened set would then
        // admit a CPU a CPU-bound task held for the whole window.
        let (_, least_busy, least_total) =
            *busy
                .iter()
                .min_by(|(_, busy_a, total_a), (_, busy_b, total_b)| {
                    (busy_a * total_b).cmp(&(busy_b * total_a))
                })?;
        let tied: Vec<usize> = busy
            .iter()
            .filter(|(_, busy, total)| busy * least_total == least_busy * total)
            .map(|(cpu, ..)| *cpu)
            .collect();
        (tied, false)
    } else {
        (idle, true)
    };
    let (unbound, bound): (Vec<usize>, Vec<usize>) = pool
        .into_iter()
        .partition(|cpu| !observation.pmu_bound.contains(cpu));
    let (candidates, pmu) = match (unbound.is_empty(), bound.len()) {
        (false, 0) => (unbound, Pmu::NoneBound),
        (false, passed_over) => (unbound, Pmu::PassedOver(passed_over)),
        (true, _) => (bound, Pmu::AllBound),
    };
    let basis = if found_idle {
        Basis::Idle {
            candidates: candidates.len(),
            sampled,
            pmu,
        }
    } else {
        Basis::LeastBusy {
            candidates: candidates.len(),
            sampled,
            pmu,
        }
    };
    Some((candidates, basis))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `/proc/stat` with the aggregate line, two full CPU lines, one short
    /// pre-2.6.11 line and the lines that follow the CPUs.
    const STAT: &str = "cpu  100 0 50 1000 10 0 0 0 0 0\n\
                        cpu0 10 2 5 100 4 1 1 0 3 0\n\
                        cpu7 20 0 10 200 0 0 0 0 0 0\n\
                        cpu31 1 2 3 4\n\
                        intr 12345 0 0\n\
                        ctxt 99\n\
                        cpus_allowed 5\n";

    fn times(entries: &[(usize, u64, u64)]) -> Reading {
        entries
            .iter()
            .map(|&(cpu, idle, total)| (cpu, CpuTime { idle, total }))
            .collect()
    }

    /// Readings over five ticks: each entry is (cpu, busy ticks).
    fn busy(entries: &[(usize, u64)], pmu_bound: &[usize]) -> Observation {
        Observation {
            before: times(
                &entries
                    .iter()
                    .map(|&(cpu, _)| (cpu, 0, 0))
                    .collect::<Vec<_>>(),
            ),
            after: times(
                &entries
                    .iter()
                    .map(|&(cpu, busy)| (cpu, 5 - busy, 5))
                    .collect::<Vec<_>>(),
            ),
            pmu_bound: pmu_bound.iter().copied().collect(),
        }
    }

    fn set(cpus: &[usize]) -> BTreeSet<usize> {
        cpus.iter().copied().collect()
    }

    #[test]
    fn parsing_reads_each_numbered_cpu_and_skips_the_aggregate_and_other_lines() {
        let parsed = parse_proc_stat(STAT, &[0, 7, 31, 99]);
        // cpu0: idle 100 + iowait 4; total user..steal = 10+2+5+100+4+1+1+0,
        // without the guest column, which user already counts.
        assert_eq!(
            parsed,
            times(&[(0, 104, 123), (7, 200, 230), (31, 4, 10)]),
            "the aggregate `cpu` line must not be read as a CPU numbered 100"
        );
    }

    #[test]
    fn parsing_keeps_only_allowed_cpus() {
        assert_eq!(parse_proc_stat(STAT, &[7]), times(&[(7, 200, 230)]));
        assert!(parse_proc_stat(STAT, &[]).is_empty());
    }

    #[test]
    fn a_cpu_running_a_cpu_bound_task_is_never_a_candidate_beside_idle_ones() {
        // CPU 3 busy throughout, CPU 5 idle, CPU 9 one rounding tick busy,
        // which still counts as idle.
        assert_eq!(
            candidates(&[3, 5, 9], &busy(&[(3, 5), (5, 0), (9, 1)], &[])),
            Some((
                vec![5, 9],
                Basis::Idle {
                    candidates: 2,
                    sampled: 3,
                    pmu: Pmu::NoneBound
                }
            ))
        );
    }

    #[test]
    fn with_no_idle_cpu_the_least_busy_fraction_wins_and_ties_are_all_kept() {
        // CPU 1: 4 of 5 busy; CPU 2: 2 of 5; CPU 4: 2 of 5; CPU 6: 3 of 6,
        // one tick above the least busy fraction; CPU 8: 4 of 6.
        let before = times(&[(1, 0, 0), (2, 0, 0), (4, 0, 0), (6, 0, 0), (8, 0, 0)]);
        let after = times(&[(1, 1, 5), (2, 3, 5), (4, 3, 5), (6, 3, 6), (8, 2, 6)]);
        let observation = Observation {
            before,
            after,
            pmu_bound: BTreeSet::new(),
        };
        assert_eq!(
            candidates(&[1, 2, 4, 6, 8], &observation),
            Some((
                vec![2, 4],
                Basis::LeastBusy {
                    candidates: 2,
                    sampled: 5,
                    pmu: Pmu::NoneBound
                }
            ))
        );
    }

    #[test]
    fn a_cpu_busy_for_the_whole_window_loses_to_one_busy_for_most_of_it() {
        // Measured with two allowed CPUs: a busy loop held CPU 28 for every
        // tick while the run's own launcher kept CPU 26 busy for four of five.
        // CPU 26 is the right choice; a set widened by a tick would admit 28.
        let observation = busy(&[(26, 4), (28, 5)], &[]);
        assert_eq!(
            candidates(&[26, 28], &observation),
            Some((
                vec![26],
                Basis::LeastBusy {
                    candidates: 1,
                    sampled: 2,
                    pmu: Pmu::NoneBound
                }
            ))
        );
    }

    #[test]
    fn idle_pmu_bound_cpus_are_passed_over_while_another_idle_cpu_remains() {
        // CPUs 0 and 16 carry a bound PMU; 5 and 20 are idle too; 9 is busy.
        let observation = busy(&[(0, 0), (5, 0), (9, 5), (16, 1), (20, 0)], &[0, 16]);
        assert_eq!(
            candidates(&[0, 5, 9, 16, 20], &observation),
            Some((
                vec![5, 20],
                Basis::Idle {
                    candidates: 2,
                    sampled: 5,
                    pmu: Pmu::PassedOver(2)
                }
            ))
        );
    }

    #[test]
    fn pmu_bound_cpus_are_drawn_when_no_other_candidate_remains() {
        // Every idle CPU carries a bound PMU: they are still better than a
        // CPU-bound co-runner.
        let observation = busy(&[(0, 0), (5, 5), (16, 0)], &[0, 16]);
        assert_eq!(
            candidates(&[0, 5, 16], &observation),
            Some((
                vec![0, 16],
                Basis::Idle {
                    candidates: 2,
                    sampled: 3,
                    pmu: Pmu::AllBound
                }
            ))
        );
        // With no idle CPU, the rule applies among the least busy.
        let observation = busy(&[(0, 3), (5, 3), (16, 5)], &[0]);
        assert_eq!(
            candidates(&[0, 5, 16], &observation),
            Some((
                vec![5],
                Basis::LeastBusy {
                    candidates: 1,
                    sampled: 3,
                    pmu: Pmu::PassedOver(1)
                }
            ))
        );
    }

    #[test]
    fn cpus_the_readings_do_not_cover_are_left_out_and_no_coverage_gives_none() {
        let before = times(&[(5, 0, 0)]);
        let after = times(&[(5, 5, 5), (8, 5, 5)]);
        let observation = Observation {
            before: before.clone(),
            after: after.clone(),
            pmu_bound: BTreeSet::new(),
        };
        // CPU 8 has no first reading and CPU 11 no reading at all.
        assert_eq!(
            candidates(&[5, 8, 11], &observation),
            Some((
                vec![5],
                Basis::Idle {
                    candidates: 1,
                    sampled: 1,
                    pmu: Pmu::NoneBound
                }
            ))
        );
        assert_eq!(candidates(&[8, 11], &observation), None);
        // A counter that went backwards, or did not move, is no reading.
        let backwards = Observation {
            before: after,
            after: before.clone(),
            pmu_bound: BTreeSet::new(),
        };
        assert_eq!(candidates(&[5], &backwards), None);
        let still = Observation {
            before: before.clone(),
            after: before,
            pmu_bound: BTreeSet::new(),
        };
        assert_eq!(candidates(&[5], &still), None);
    }

    #[test]
    fn cpu_lists_parse_as_the_kernel_prints_them_and_malformed_ones_are_refused() {
        assert_eq!(parse_cpu_list("0\n"), Some(set(&[0])));
        assert_eq!(
            parse_cpu_list("0,16-18,144"),
            Some(set(&[0, 16, 17, 18, 144]))
        );
        assert_eq!(parse_cpu_list(""), Some(set(&[])));
        for malformed in ["0-", "-3", "3-1", "a", "1,,2", "0-8192", "1 2"] {
            assert_eq!(parse_cpu_list(malformed), None, "{malformed:?}");
        }
    }

    #[test]
    fn only_masks_of_at_most_a_quarter_of_the_online_cpus_are_bound() {
        let online: BTreeSet<usize> = (0..316).collect();
        // This host's amd_l3, amd_df and power_core masks, and one naming an
        // offline CPU.
        let amd_l3: BTreeSet<usize> = (0..160).step_by(16).collect();
        let masks = [amd_l3.clone(), set(&[0]), (0..158).collect(), set(&[400])];
        let mut expected = amd_l3;
        expected.insert(0);
        assert_eq!(bound_cpus(&online, masks), expected);
        // Exactly a quarter still counts as bound; one more CPU does not.
        let online: BTreeSet<usize> = (0..8).collect();
        assert_eq!(bound_cpus(&online, [set(&[0, 1])]), set(&[0, 1]));
        assert_eq!(bound_cpus(&online, [set(&[0, 1, 2])]), set(&[]));
    }

    /// One PMU directory entry in a test sysfs.
    #[derive(Clone, Copy)]
    enum Entry {
        /// A directory with this cpumask.
        Mask(&'static str),
        /// A symlink to a directory with this cpumask, as sysfs presents one.
        LinkedMask(&'static str),
        /// A directory with no cpumask.
        NoMask,
        /// A directory whose cpumask is a directory.
        MaskIsDir,
        /// A directory whose cpumask is a symlink that resolves to nothing.
        DanglingMask,
        /// A symlink that resolves to nothing.
        Dangling,
        /// A plain file.
        PlainFile,
    }

    /// A sysfs root with 16 online CPUs and these PMU entries.
    fn sysfs(entries: &[(&str, Entry)]) -> tempfile::TempDir {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().expect("temporary sysfs root");
        let cpu = root.path().join("devices/system/cpu");
        let devices = root.path().join("bus/event_source/devices");
        let targets = root.path().join("devices/pmus");
        for directory in [&cpu, &devices, &targets] {
            fs::create_dir_all(directory).expect("sysfs directory");
        }
        fs::write(cpu.join("online"), "0-15\n").expect("online file");
        for (name, entry) in entries {
            let path = devices.join(name);
            let gone = targets.join("gone");
            match entry {
                Entry::Mask(mask) => {
                    fs::create_dir(&path).expect("PMU directory");
                    fs::write(path.join("cpumask"), mask).expect("cpumask");
                }
                Entry::LinkedMask(mask) => {
                    let target = targets.join(name);
                    fs::create_dir(&target).expect("PMU directory");
                    fs::write(target.join("cpumask"), mask).expect("cpumask");
                    symlink(&target, &path).expect("PMU entry symlink");
                }
                Entry::NoMask => fs::create_dir(&path).expect("PMU directory"),
                Entry::MaskIsDir => {
                    fs::create_dir(&path).expect("PMU directory");
                    fs::create_dir(path.join("cpumask")).expect("cpumask directory");
                }
                Entry::DanglingMask => {
                    fs::create_dir(&path).expect("PMU directory");
                    symlink(&gone, path.join("cpumask")).expect("dangling cpumask");
                }
                Entry::Dangling => symlink(&gone, &path).expect("dangling entry"),
                Entry::PlainFile => fs::write(&path, "").expect("plain file"),
            }
        }
        root
    }

    #[test]
    fn an_unreadable_or_malformed_sysfs_passes_nothing_over() {
        let valid = [
            ("amd_l3", Entry::LinkedMask("0,8\n")),
            ("amd_df", Entry::Mask("0\n")),
            ("power_core", Entry::Mask("0-7\n")),
            ("cpu", Entry::NoMask),
        ];
        let root = sysfs(&valid);
        assert_eq!(
            pmu_bound_cpus(root.path()),
            set(&[0, 8]),
            "per-core power_core is skipped and cpu names no CPUs"
        );
        let empty = tempfile::tempdir().expect("temporary sysfs root");
        assert!(pmu_bound_cpus(empty.path()).is_empty(), "no online file");
        for (strange, what) in [
            (Entry::Mask("zero\n"), "a malformed cpumask"),
            (Entry::MaskIsDir, "a cpumask that is not a regular file"),
            (
                Entry::DanglingMask,
                "a cpumask symlink that resolves to nothing",
            ),
            (Entry::Dangling, "a PMU entry that resolves to nothing"),
            (Entry::PlainFile, "a PMU entry that is not a directory"),
        ] {
            let mut entries = valid.to_vec();
            entries.push(("strange", strange));
            let root = sysfs(&entries);
            assert!(
                pmu_bound_cpus(root.path()).is_empty(),
                "{what} must not leave a partial set"
            );
        }
    }

    #[test]
    fn every_draw_over_a_sparse_mask_is_an_idle_member_of_it() {
        // CPU 31 runs a CPU-bound task; 7 and 211 are idle.
        let observation = busy(&[(7, 0), (31, 5), (211, 0)], &[]);
        let mut drawn = BTreeMap::new();
        for _ in 0..128 {
            let (cpu, basis) =
                choose_with(&[7, 31, 211], |_| Some(observation.clone())).expect("non-empty mask");
            assert_eq!(
                basis,
                Basis::Idle {
                    candidates: 2,
                    sampled: 3,
                    pmu: Pmu::NoneBound
                }
            );
            *drawn.entry(cpu).or_insert(0) += 1;
        }
        assert!(!drawn.contains_key(&31), "drew the busy CPU: {drawn:?}");
        assert_eq!(
            drawn.keys().copied().collect::<Vec<_>>(),
            [7, 211],
            "128 uniform draws from two idle CPUs reach both"
        );
    }

    #[test]
    fn without_a_reading_every_allowed_cpu_stays_a_candidate() {
        for _ in 0..128 {
            let (cpu, basis) = choose_with(&[7, 31, 211], |_| None).expect("non-empty mask");
            assert!([7, 31, 211].contains(&cpu), "chose CPU {cpu}");
            assert_eq!(basis, Basis::Unsampled);
        }
    }

    #[test]
    fn choose_returns_a_member_of_the_allowed_set_or_none_when_it_is_empty() {
        assert_eq!(choose(&[]), None);
        assert_eq!(choose(&[42]), Some((42, Basis::OnlyAllowed)));
        let allowed = cpu_ids_from_this_host();
        let (cpu, basis) = choose(&allowed).expect("this process has allowed CPUs");
        assert!(
            allowed.contains(&cpu),
            "chose CPU {cpu} from {allowed:?} ({basis})"
        );
    }

    /// The CPUs this test process may run on.
    fn cpu_ids_from_this_host() -> Vec<usize> {
        let mask = nix::sched::sched_getaffinity(nix::unistd::Pid::from_raw(0))
            .expect("sched_getaffinity of this process");
        (0..nix::sched::CpuSet::count())
            .filter(|cpu| mask.is_set(*cpu).unwrap_or(false))
            .collect()
    }
}
