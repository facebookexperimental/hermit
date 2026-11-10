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
//! (<https://github.com/rrnewton/hermit/issues/4824>). Measured on a 316-CPU
//! host: one strict verify of an epoll guest took 1.5 s on an idle CPU and
//! 15.5 s on a CPU running a busy loop from another cgroup, while the run's
//! own cgroup kept 24 busy loops on other CPUs. Under a loaded test runner,
//! runs were 170 to 1,400 times slower and hit test deadlines.
//!
//! A uniformly random CPU lands on such a task as often as there are busy
//! CPUs. Instead, a run samples the allowed CPUs' busy time from `/proc/stat`
//! over [`SAMPLE_WINDOW`] and draws uniformly from the ones that were idle, or,
//! when none was, from the least busy.
//!
//! A sample alone cannot keep concurrent runs apart. Runs that sample at the
//! same moment draw from the same candidates: every idle CPU while there are
//! some (three runs among eight idle CPUs share one 34% of the time), but
//! often a single CPU when there are none, since at 10 ms ticks the least busy
//! fraction is rarely tied. And a run whose guest mostly waits leaves its CPU
//! looking idle to a later run's sample. Two runs on one CPU take turns at
//! every ptrace stop: two copies of a 40,000-syscall guest pinned to one core
//! took 260 s each, against 27 s on two cores. So a run also reserves its CPU
//! for its lifetime (`reserve`): an abstract Unix socket named after the CPU,
//! which the kernel lets one socket in a network namespace hold
//! (`unix_bind_abstract`, net/unix/af_unix.c) and releases when the last
//! descriptor for it closes: when Hermit and its container init, which holds
//! a copy and dies with Hermit, have exited, however they exit. A CPU another
//! run holds is passed over for the next draw from the same sample, and only
//! when every allowed CPU is held does a run share one: the CPU it drew
//! first. Sharing costs speed, never determinism. Runs in different network
//! namespaces do not see each other's socket names, so a run also takes an
//! exclusive `flock` on its CPU's file in a lock directory (`lock`,
//! [`LOCK_DIR_DEFAULT`]), which every run sharing that directory sees, and
//! which the kernel likewise releases with the last descriptor. A CPU either
//! channel holds is held.
//!
//! A run that cannot keep other runs off its CPU (it shares one, cannot
//! reserve one, or can reserve one only within its network namespace) says
//! so by name on Hermit's own stderr, or, under `hermit run
//! --pinned-cpu=refuse` or `wait`, refuses before the guest starts (`admit`).
//!
//! A process holds one reservation at a time (`HELD`). Its later runs (the
//! second run of a `--verify` pair, `analyze` trials) sample again, count the
//! CPU the process holds as free rather than as another run's, and move the
//! reservation when they draw a different CPU.
//!
//! Among the candidates, a run passes over the CPUs a PMU is bound to while
//! any other candidate remains. Some PMUs open their events on a few CPUs,
//! named by `/sys/bus/event_source/devices/<pmu>/cpumask` (on AMD, `amd_l3`
//! on one CPU per L3 and `amd_df` and `amd_umc` on CPU 0), and ptrace stops
//! cost several times as much CPU there
//! (<https://github.com/rrnewton/hermit/issues/3265>: 7.6-7.9x for a
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
use std::io;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::path::Path;
use std::sync::Mutex;
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

/// Choose a CPU from `allowed`, the tracer's affinity mask, with the
/// observation made by `observe`, which tests replace, and say why. `None`
/// only when `allowed` is empty. Reserves nothing: runs choose through
/// [`choose_for_run`], which draws the same way.
#[cfg(test)]
pub(crate) fn choose_with(
    allowed: &[usize],
    observe: impl FnOnce(&[usize]) -> Option<Observation>,
) -> Option<(usize, Basis)> {
    match allowed {
        [] => None,
        [only] => Some((*only, Basis::OnlyAllowed)),
        _ => Some(draw(allowed, observe(allowed).as_ref())),
    }
}

/// One draw from `pool`, two or more CPUs: from its candidates under
/// `observation`, or from all of it without one.
fn draw(pool: &[usize], observation: Option<&Observation>) -> (usize, Basis) {
    let (candidates, basis) = observation
        .and_then(|observation| candidates(pool, observation))
        .unwrap_or_else(|| (pool.to_vec(), Basis::Unsampled));
    (candidates[rand::random_range(0..candidates.len())], basis)
}

/// The prefix of the abstract socket names that reserve a CPU
/// (`hermit-pinned-core:<cpu>`, [`reserve`]).
const RESERVATION_PREFIX: &str = "hermit-pinned-core";

/// The lowest descriptor number a reservation is moved to, so that taking one
/// does not shift the numbers of the descriptors Hermit opens after it, some
/// of which reach the guest's setup.
const RESERVATION_FD_FLOOR: libc::c_int = 1000;

/// The reservation this process holds: its CPU and the descriptor that holds
/// its name. A later run of the same process (a `--verify` pair runs twice)
/// counts that CPU as free, and replacing the reservation closes the old
/// descriptor, so a process never holds a CPU it moved away from. Runs one
/// process makes at the same time (test threads, say) may draw its CPU too.
static HELD: Mutex<Option<(usize, Reservation)>> = Mutex::new(None);

/// The descriptors that hold a CPU: the abstract socket's name, and, where the
/// lock directory is usable, the CPU's lock file (`lock`). Either one held by
/// another run makes the CPU held.
#[derive(Debug)]
pub(crate) struct Reservation {
    _socket: OwnedFd,
    _lock: Option<OwnedFd>,
}

/// How a run holds its CPU against other Hermit runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Hold {
    /// This process holds the CPU: no other Hermit run in this network
    /// namespace is pinned to it. `passed_over` CPUs other runs hold were
    /// passed over first.
    Reserved { passed_over: usize },
    /// This process already held the CPU, for an earlier run, and keeps it.
    Kept,
    /// Every allowed CPU is held by another Hermit run, so this run shares
    /// the CPU it drew first with one of them.
    Shared,
    /// A reservation could not be made (the error), so the CPU is used
    /// unreserved, and another run may share it.
    Unreserved(String),
}

/// The CPU a run is pinned to, why, and how it holds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Choice {
    pub(crate) cpu: usize,
    /// How the CPU was drawn, over the allowed CPUs no other run was found to
    /// hold.
    pub(crate) basis: Basis,
    pub(crate) hold: Hold,
}

impl fmt::Display for Choice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Hold::Reserved { passed_over } = self.hold
            && passed_over > 0
        {
            write!(
                f,
                "among the allowed CPUs no other Hermit run holds ({passed_over} held): "
            )?;
        }
        write!(f, "{}; ", self.basis)?;
        match &self.hold {
            Hold::Reserved { .. } => write!(f, "reserved"),
            Hold::Kept => write!(f, "already reserved by this process"),
            Hold::Shared => write!(
                f,
                "every allowed CPU is held by another Hermit run, so this run shares it and \
                 both run slower"
            ),
            Hold::Unreserved(error) => {
                write!(f, "unreserved, so another Hermit run may share it: {error}")
            }
        }
    }
}

/// Choose and reserve the CPU this run is pinned to from `allowed`, the
/// tracer's affinity mask, over a fresh sample, counting the CPU this process
/// already holds as free. `None` only when `allowed` is empty.
pub(crate) fn choose_for_run(allowed: &[usize]) -> Option<(Choice, Coordination)> {
    let mut held = HELD.lock().unwrap_or_else(|poison| poison.into_inner());
    let base = std::env::var_os(LOCK_DIR_ENV)
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(LOCK_DIR_DEFAULT));
    let coordination = match lock_dir(&base) {
        Ok(()) => Coordination::Host(base),
        Err(reason) => Coordination::NetworkNamespaceOnly(reason),
    };
    let dir = match &coordination {
        Coordination::Host(dir) => Some(dir.as_path()),
        Coordination::NetworkNamespaceOnly(_) => None,
    };
    let choice = choose_for_run_with(allowed, observe, &mut held, |cpu| {
        reserve_both(RESERVATION_PREFIX, dir, cpu)
    })?;
    Some((choice, coordination))
}

/// [`choose_for_run`] with the observation, the process's reservation and
/// the reservation call supplied, which tests replace. `reserve` answers the
/// descriptor that now holds a CPU, or `None` when another run holds it.
/// A run that reserves a different CPU replaces `held`, closing the old
/// descriptor; a run that shares or goes unreserved leaves `held` empty, since
/// the process no longer runs on the CPU it held.
fn choose_for_run_with<R>(
    allowed: &[usize],
    observe: impl FnOnce(&[usize]) -> Option<Observation>,
    held: &mut Option<(usize, R)>,
    mut reserve: impl FnMut(usize) -> io::Result<Option<R>>,
) -> Option<Choice> {
    let own = held.as_ref().map(|(cpu, _)| *cpu);
    let mut taken = None;
    let mut choice = choose_reserving(allowed, observe, |cpu| {
        if Some(cpu) == own {
            return Ok(true);
        }
        Ok(reserve(cpu)?.map(|fd| taken = Some((cpu, fd))).is_some())
    })?;
    match (&choice.hold, taken) {
        (Hold::Reserved { .. }, Some(new)) => *held = Some(new),
        (Hold::Reserved { .. }, None) => choice.hold = Hold::Kept,
        _ => *held = None,
    }
    Some(choice)
}

/// [`choose_with`], passing over the CPUs other Hermit runs hold. `reserve`
/// takes a CPU for this run: `Ok(true)` when the run now holds it, `Ok(false)`
/// when another run does, and an error when reservations cannot be made at
/// all. Every draw uses the one observation: it is [`choose_with`]'s draw over
/// the allowed CPUs not yet found held, so a run passes a held idle CPU over
/// for another idle one, and then for the least busy free one. With every CPU
/// held, the run shares the CPU it drew first.
pub(crate) fn choose_reserving(
    allowed: &[usize],
    observe: impl FnOnce(&[usize]) -> Option<Observation>,
    mut reserve: impl FnMut(usize) -> io::Result<bool>,
) -> Option<Choice> {
    let observation = match allowed {
        [] => return None,
        [_] => None,
        _ => observe(allowed),
    };
    let mut free = allowed.to_vec();
    let mut first = None;
    loop {
        let (cpu, basis) = match allowed {
            [only] => (*only, Basis::OnlyAllowed),
            _ => draw(&free, observation.as_ref()),
        };
        let first = *first.get_or_insert((cpu, basis));
        let hold = match reserve(cpu) {
            Ok(true) => Hold::Reserved {
                passed_over: allowed.len() - free.len(),
            },
            Ok(false) => {
                free.retain(|other| *other != cpu);
                if !free.is_empty() {
                    continue;
                }
                return Some(Choice {
                    cpu: first.0,
                    basis: first.1,
                    hold: Hold::Shared,
                });
            }
            Err(error) => Hold::Unreserved(error.to_string()),
        };
        return Some(Choice { cpu, basis, hold });
    }
}

/// Hold `cpu` while the returned descriptor is open: an abstract Unix socket
/// named `<prefix>:<cpu>`, bound close-on-exec, which the kernel releases when
/// its last descriptor closes, at the latest when Hermit exits. Another run
/// that binds the same name gets `EADDRINUSE`, answered as `Ok(None)`. The
/// names belong to a network namespace: runs in different ones do not see
/// each other's CPUs.
fn reserve(prefix: &str, cpu: usize) -> io::Result<Option<OwnedFd>> {
    use std::os::linux::net::SocketAddrExt;
    let name = format!("{prefix}:{cpu}");
    let address = std::os::unix::net::SocketAddr::from_abstract_name(name.as_bytes())?;
    let socket = match std::os::unix::net::UnixDatagram::bind_addr(&address) {
        Ok(socket) => socket,
        Err(error) if error.raw_os_error() == Some(libc::EADDRINUSE) => return Ok(None),
        Err(error) => return Err(error),
    };
    Ok(Some(move_high(OwnedFd::from(socket))?))
}

/// `fd` moved to a high close-on-exec descriptor, and its low number closed:
/// at least [`RESERVATION_FD_FLOOR`] where the descriptor limit allows, and
/// otherwise a free number just below the limit, so that a run under a
/// low `RLIMIT_NOFILE` (1000 or less, where `F_DUPFD_CLOEXEC` from the floor
/// fails with `EINVAL`) still reserves its CPU. Keeps `fd` where it is only
/// when no number above it is free.
fn move_high(fd: OwnedFd) -> io::Result<OwnedFd> {
    // SAFETY: `fd` is an open descriptor this function owns.
    let moved = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, RESERVATION_FD_FLOOR) };
    if moved >= 0 {
        // SAFETY: `moved` is a new descriptor nothing else owns; `fd` closes
        // its own number when it drops.
        return Ok(unsafe { OwnedFd::from_raw_fd(moved) });
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() != Some(libc::EINVAL) {
        return Err(error);
    }
    let limit = nix::sys::resource::getrlimit(nix::sys::resource::Resource::RLIMIT_NOFILE)
        .map_err(io::Error::from)?
        .0;
    // `F_DUPFD_CLOEXEC` takes the lowest free number at or above its floor,
    // atomically, so no other thread's descriptor is ever replaced. Lower the
    // floor from just below the limit until a number is free (`EMFILE` means
    // none is at or above that floor).
    let limit = libc::c_int::try_from(limit.min(RESERVATION_FD_FLOOR as u64)).unwrap_or(0);
    let mut step = 1;
    while limit - step > fd.as_raw_fd() {
        // SAFETY: `fd` is an open descriptor this function owns.
        let moved = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, limit - step) };
        if moved >= 0 {
            // SAFETY: `moved` is a new descriptor nothing else owns.
            return Ok(unsafe { OwnedFd::from_raw_fd(moved) });
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EMFILE) {
            return Err(error);
        }
        step *= 2;
    }
    Ok(fd)
}

/// The directory whose per-CPU lock files hold reservations across network
/// namespaces (`lock`), unless [`LOCK_DIR_ENV`] names another.
pub(crate) const LOCK_DIR_DEFAULT: &str = "/dev/shm/hermit-pinned-cpu";

/// Names the lock directory in place of [`LOCK_DIR_DEFAULT`]. It changes only
/// the path: the directory is checked as the default one is.
pub(crate) const LOCK_DIR_ENV: &str = "HERMIT_PINNED_CPU_DIR";

/// Which Hermit runs a run's reservation keeps off its CPU.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Coordination {
    /// Every run that uses this lock directory, whatever its network
    /// namespace, as well as every run in this network namespace.
    Host(std::path::PathBuf),
    /// Only runs in this network namespace: the lock directory is unusable,
    /// for the reason given.
    NetworkNamespaceOnly(String),
}

/// Checks, or creates, the lock directory `base`. It must be a real directory
/// (not a symlink), owned by this user, or else the shared form: owned by
/// root, mode 1777 with the sticky bit, so no other user can remove or replace
/// this user's lock files. A directory this creates is mode 1777 with the
/// sticky bit, so other users' runs can lock in it too.
fn lock_dir(base: &Path) -> Result<(), String> {
    use std::os::unix::fs::DirBuilderExt;
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::fs::PermissionsExt;
    let shown = base.display();
    let metadata = match fs::symlink_metadata(base) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            match fs::DirBuilder::new().mode(0o1777).create(base) {
                Ok(()) => {
                    // The umask may have cleared bits `mode` asked for.
                    fs::set_permissions(base, fs::Permissions::from_mode(0o1777))
                        .map_err(|error| format!("cannot set the mode of {shown}: {error}"))?;
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(format!("cannot create {shown}: {error}")),
            }
            fs::symlink_metadata(base).map_err(|error| format!("cannot read {shown}: {error}"))?
        }
        Err(error) => return Err(format!("cannot read {shown}: {error}")),
    };
    if metadata.file_type().is_symlink() {
        return Err(format!("{shown} is a symbolic link"));
    }
    if !metadata.is_dir() {
        return Err(format!("{shown} is not a directory"));
    }
    if !acceptable_owner(
        metadata.uid(),
        metadata.mode(),
        nix::unistd::geteuid().as_raw(),
    ) {
        return Err(format!(
            "{shown} belongs to user {} and is not the shared root-owned form (mode 1777, sticky)",
            metadata.uid()
        ));
    }
    Ok(())
}

/// Whether a lock directory owned by `owner` with `mode` may be used by user
/// `euid`: its own, or root's shared mode-1777 sticky directory.
fn acceptable_owner(owner: u32, mode: u32, euid: u32) -> bool {
    owner == euid || (owner == 0 && mode & 0o7777 == 0o1777)
}

/// Hold `cpu`'s lock file in `dir` while the returned descriptor is open: an
/// exclusive `flock`, which the kernel releases when the last descriptor for
/// it closes, however its holders exit. `Ok(None)` when the CPU counts as
/// held: another run holds the lock, or the file is not one this run can lock
/// (another user's that it cannot open, or not a regular file), which a
/// squatter could arrange but which only ever makes this run pass the CPU
/// over or share it. The file is opened without following a symlink and
/// without blocking (a FIFO in its place cannot stall the run).
fn lock(dir: &Path, cpu: usize) -> io::Result<Option<OwnedFd>> {
    use std::os::unix::fs::OpenOptionsExt;
    let path = dir.join(cpu.to_string());
    let file = match fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o644)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(&path)
        .or_else(|error| {
            // Another user's file, readable: a shared lock file.
            if error.raw_os_error() == Some(libc::EACCES) {
                fs::OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
                    .open(&path)
            } else {
                Err(error)
            }
        }) {
        Ok(file) => file,
        // Unopenable (another user's, unreadable), a symlink (`ELOOP`), a
        // directory (`EISDIR`) or a device or socket with nothing behind it
        // (`ENXIO`).
        Err(error)
            if matches!(
                error.raw_os_error(),
                Some(libc::EACCES | libc::ELOOP | libc::EISDIR | libc::ENXIO)
            ) =>
        {
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    if !file.metadata()?.is_file() {
        return Ok(None);
    }
    // SAFETY: flock on a descriptor this function owns.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return Ok(Some(move_high(OwnedFd::from(file))?));
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::EWOULDBLOCK) {
        return Ok(None);
    }
    Err(error)
}

/// Reserve `cpu` through both channels: its lock file in `dir`, when there is
/// one, and then its abstract socket name. `Ok(None)` when either is held.
fn reserve_both(prefix: &str, dir: Option<&Path>, cpu: usize) -> io::Result<Option<Reservation>> {
    let lock = match dir {
        Some(dir) => match lock(dir, cpu)? {
            Some(lock) => Some(lock),
            None => return Ok(None),
        },
        None => None,
    };
    Ok(reserve(prefix, cpu)?.map(|socket| Reservation {
        _socket: socket,
        _lock: lock,
    }))
}

/// What a pinned run does when it cannot hold a CPU of its own
/// (`hermit run --pinned-cpu`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PinnedCpuPolicy {
    /// Run anyway, on a CPU another run may share, with a notice saying so.
    Share,
    /// Refuse the run before the guest starts.
    Refuse,
    /// Wait up to this long for a CPU to come free, then refuse.
    Wait(Duration),
}

/// The default `--pinned-cpu=wait` bound.
pub(crate) const DEFAULT_WAIT: Duration = Duration::from_secs(600);

/// How often `--pinned-cpu=wait` tries again. Each try sleeps; nothing spins.
pub(crate) const WAIT_INTERVAL: Duration = Duration::from_millis(500);

impl std::str::FromStr for PinnedCpuPolicy {
    type Err = String;
    fn from_str(text: &str) -> Result<Self, String> {
        match text {
            "share" => Ok(Self::Share),
            "refuse" => Ok(Self::Refuse),
            "wait" => Ok(Self::Wait(DEFAULT_WAIT)),
            _ => match text.strip_prefix("wait:").map(str::parse::<u64>) {
                Some(Ok(seconds)) if seconds > 0 => Ok(Self::Wait(Duration::from_secs(seconds))),
                _ => Err(format!(
                    "expected share, refuse, wait or wait:SECONDS (SECONDS above 0), got {text:?}"
                )),
            },
        }
    }
}

impl fmt::Display for PinnedCpuPolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Share => f.write_str("share"),
            Self::Refuse => f.write_str("refuse"),
            Self::Wait(wait) if *wait == DEFAULT_WAIT => f.write_str("wait"),
            Self::Wait(wait) => write!(f, "wait:{}", wait.as_secs()),
        }
    }
}

/// The policy `hermit run` set for this process (`set_policy`); `Share` for
/// every other subcommand.
static POLICY: Mutex<PinnedCpuPolicy> = Mutex::new(PinnedCpuPolicy::Share);

/// Set the policy every later pinned run of this process follows.
pub(crate) fn set_policy(policy: PinnedCpuPolicy) {
    *POLICY.lock().unwrap_or_else(|poison| poison.into_inner()) = policy;
}

/// Turns off the notices when set to `0`, for a caller that asserts Hermit
/// writes nothing to stderr. It does not change placement or the policy.
pub(crate) const NOTICE_ENV: &str = "HERMIT_PINNED_CPU_NOTICE";

/// A run that could not hold a CPU of its own, under `--pinned-cpu=refuse` or
/// `wait`.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct PlacementRefusal(pub(crate) String);

/// The notice a run gives when it cannot keep other Hermit runs off its CPU,
/// or `None` when it can.
fn notice(allowed: &[usize], choice: &Choice, coordination: &Coordination) -> Option<String> {
    let set = cpu_set(allowed);
    match (&choice.hold, coordination) {
        (Hold::Shared, _) => Some(format!(
            "hermit: warning: cpu-shared: every allowed CPU ({set}) is held by another Hermit \
             run; this run shares CPU {} and runs slower (placement only: guest output is \
             unaffected). --pinned-cpu=wait or =refuse avoids sharing.",
            choice.cpu
        )),
        (Hold::Unreserved(error), _) => Some(format!(
            "hermit: warning: cpu-unreserved: could not reserve CPU {} ({error}); another \
             Hermit run may be pinned to it.",
            choice.cpu
        )),
        (_, Coordination::NetworkNamespaceOnly(reason)) => Some(format!(
            "hermit: warning: cpu-netns-only: CPU {} is reserved only against Hermit runs in \
             this network namespace ({}): {reason}. Runs in other network namespaces may pick \
             the same CPU; point {LOCK_DIR_ENV} at a directory they share.",
            choice.cpu,
            network_namespace()
        )),
        _ => None,
    }
}

/// `0-3,8` for the CPUs in `cpus`.
fn cpu_set(cpus: &[usize]) -> String {
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for &cpu in cpus {
        match ranges.last_mut() {
            Some((_, end)) if *end + 1 == cpu => *end = cpu,
            _ => ranges.push((cpu, cpu)),
        }
    }
    ranges
        .iter()
        .map(|&(start, end)| {
            if start == end {
                start.to_string()
            } else {
                format!("{start}-{end}")
            }
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// This process's network namespace, as `/proc/self/ns/net` names it.
fn network_namespace() -> String {
    fs::read_link("/proc/self/ns/net")
        .map(|link| link.display().to_string())
        .unwrap_or_else(|_| "unknown".to_owned())
}

/// Place a pinned run on a CPU from `allowed` under `policy`, writing the
/// notice for a CPU it may share to `notices` (`notice`). `choose` makes
/// one attempt (`choose_for_run`); `pause` waits between attempts under
/// `wait`, which tries again until the bound has passed. A refusal names the
/// CPU set, why, and the wait.
fn admit(
    allowed: &[usize],
    policy: PinnedCpuPolicy,
    mut choose: impl FnMut(&[usize]) -> Option<(Choice, Coordination)>,
    mut pause: impl FnMut(Duration),
    notices: &mut dyn FnMut(&str),
) -> Result<Option<Choice>, PlacementRefusal> {
    let attempts = match policy {
        PinnedCpuPolicy::Wait(wait) => wait.as_millis().div_ceil(WAIT_INTERVAL.as_millis()) + 1,
        _ => 1,
    };
    for attempt in 1..=attempts {
        let Some((choice, coordination)) = choose(allowed) else {
            return Ok(None);
        };
        let Some(text) = notice(allowed, &choice, &coordination) else {
            return Ok(Some(choice));
        };
        let refusal = |why: &str| {
            PlacementRefusal(format!(
                "--pinned-cpu={policy}: refusing the run on CPUs {}: {why}",
                cpu_set(allowed)
            ))
        };
        match policy {
            PinnedCpuPolicy::Share => {
                notices(&text);
                return Ok(Some(choice));
            }
            PinnedCpuPolicy::Wait(wait) if choice.hold == Hold::Shared => {
                if attempt < attempts {
                    pause(WAIT_INTERVAL);
                    continue;
                }
                return Err(refusal(&format!(
                    "every allowed CPU stayed held by another Hermit run for the whole {} s wait",
                    wait.as_secs()
                )));
            }
            PinnedCpuPolicy::Refuse | PinnedCpuPolicy::Wait(_) => {
                let why = text
                    .split_once(": warning: ")
                    .map_or(text.as_str(), |(_, why)| why);
                return Err(refusal(why));
            }
        }
    }
    unreachable!("the last attempt returns")
}

/// [`admit`] for this process: the policy `set_policy` gave, real attempts and
/// sleeps, and notices on Hermit's own stderr unless [`NOTICE_ENV`] is `0`.
pub(crate) fn place(allowed: &[usize]) -> Result<Option<Choice>, PlacementRefusal> {
    let policy = *POLICY.lock().unwrap_or_else(|poison| poison.into_inner());
    let quiet = std::env::var_os(NOTICE_ENV).is_some_and(|value| value == "0");
    admit(
        allowed,
        policy,
        choose_for_run,
        thread::sleep,
        &mut |text| {
            if !quiet {
                use std::io::Write;
                let _ = writeln!(io::stderr().lock(), "{text}");
            }
        },
    )
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
        assert_eq!(choose_with(&[], observe), None);
        assert_eq!(choose_with(&[42], observe), Some((42, Basis::OnlyAllowed)));
        let allowed = cpu_ids_from_this_host();
        let (cpu, basis) = choose_with(&allowed, observe).expect("this process has allowed CPUs");
        assert!(
            allowed.contains(&cpu),
            "chose CPU {cpu} from {allowed:?} ({basis})"
        );
    }

    /// A reservation stub over `held`, the CPUs other runs hold.
    fn held_by_others(held: &[usize]) -> impl FnMut(usize) -> io::Result<bool> {
        let held = set(held);
        move |cpu| Ok(!held.contains(&cpu))
    }

    #[test]
    fn an_idle_cpu_another_run_holds_is_passed_over_for_another_idle_one() {
        // CPUs 1 and 2 idle, 3 busy throughout; another run holds CPU 1.
        for _ in 0..32 {
            let choice = choose_reserving(
                &[1, 2, 3],
                |_| Some(busy(&[(1, 0), (2, 0), (3, 5)], &[])),
                held_by_others(&[1]),
            )
            .expect("three CPUs");
            assert_eq!(choice.cpu, 2, "{choice}");
            assert!(
                matches!(choice.hold, Hold::Reserved { passed_over } if passed_over <= 1),
                "{choice}"
            );
        }
    }

    #[test]
    fn with_every_idle_cpu_held_the_least_busy_free_cpu_is_reserved() {
        // CPUs 1 and 2 idle but held; CPU 3 busy throughout; CPU 4 two ticks
        // in five, so not idle but the least busy of the free ones.
        let choice = choose_reserving(
            &[1, 2, 3, 4],
            |_| Some(busy(&[(1, 0), (2, 0), (3, 5), (4, 2)], &[])),
            held_by_others(&[1, 2]),
        )
        .expect("four CPUs");
        assert_eq!(
            choice,
            Choice {
                cpu: 4,
                basis: Basis::LeastBusy {
                    candidates: 1,
                    sampled: 2,
                    pmu: Pmu::NoneBound
                },
                hold: Hold::Reserved { passed_over: 2 },
            }
        );
    }

    #[test]
    fn with_every_cpu_held_a_run_shares_the_cpu_it_drew_first() {
        // CPU 1 idle, CPU 2 busy: the first draw is CPU 1, and both are held.
        let choice = choose_reserving(
            &[1, 2],
            |_| Some(busy(&[(1, 0), (2, 5)], &[])),
            held_by_others(&[1, 2]),
        )
        .expect("two CPUs");
        assert_eq!(
            choice,
            Choice {
                cpu: 1,
                basis: Basis::Idle {
                    candidates: 1,
                    sampled: 2,
                    pmu: Pmu::NoneBound
                },
                hold: Hold::Shared,
            }
        );
    }

    #[test]
    fn where_reservations_cannot_be_made_the_drawn_cpu_is_used_unreserved() {
        let choice = choose_reserving(
            &[1, 2, 3],
            |_| Some(busy(&[(1, 0), (2, 5), (3, 5)], &[])),
            |_| Err(io::Error::from_raw_os_error(libc::EPERM)),
        )
        .expect("three CPUs");
        assert_eq!(choice.cpu, 1, "{choice}");
        assert!(matches!(choice.hold, Hold::Unreserved(_)), "{choice}");
        assert_eq!(
            choose_reserving(&[], |_| None, |_| Ok(true)),
            None,
            "an empty mask has no CPU to reserve"
        );
    }

    /// A name prefix private to one test: its process ID alone could repeat
    /// across PID namespaces that share a network namespace.
    fn private_prefix() -> String {
        format!(
            "hermit-pinned-core-test:{}:{:016x}",
            std::process::id(),
            rand::random::<u64>()
        )
    }

    /// Two runs reserving within a two-CPU set never share a CPU while each
    /// holds its reservation: the second run's bind of the first one's name
    /// fails, and it takes the other CPU. A third finds both held and shares.
    #[test]
    fn runs_reserving_within_two_cpus_never_share_one() {
        let prefix = private_prefix();
        let mut holders = Vec::new();
        let mut run = || {
            choose_reserving(
                &[0, 1],
                |_| None,
                |cpu| Ok(reserve(&prefix, cpu)?.map(|fd| holders.push(fd)).is_some()),
            )
            .expect("two CPUs")
        };
        let (first, second, third) = (run(), run(), run());
        assert!(
            matches!(first.hold, Hold::Reserved { .. }),
            "first run: {first}"
        );
        assert!(
            matches!(second.hold, Hold::Reserved { .. }),
            "second run: {second}"
        );
        assert_ne!(first.cpu, second.cpu);
        assert_eq!(third.hold, Hold::Shared, "third run: {third}");
    }

    #[test]
    fn a_reservation_sits_on_a_high_close_on_exec_descriptor_until_it_closes() {
        let prefix = private_prefix();
        let fd = reserve(&prefix, 5)
            .expect("abstract sockets can be bound")
            .expect("the name is free");
        assert!(fd.as_raw_fd() >= RESERVATION_FD_FLOOR, "{fd:?}");
        // SAFETY: F_GETFD on a descriptor this test owns.
        let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) };
        assert_eq!(
            flags & libc::FD_CLOEXEC,
            libc::FD_CLOEXEC,
            "flags {flags:#x}"
        );
        assert!(
            reserve(&prefix, 5).expect("bind").is_none(),
            "a held name is refused"
        );
        drop(fd);
        assert!(
            reserve(&prefix, 5).expect("bind").is_some(),
            "closing the descriptor releases the name"
        );
    }

    #[test]
    fn a_later_run_of_the_process_keeps_the_cpu_it_holds() {
        let prefix = private_prefix();
        let mut held = reserve(&prefix, 1).expect("bind").map(|fd| (1, fd));
        // CPU 1 is idle and this process holds it; CPU 2 is busy.
        let choice = choose_for_run_with(
            &[1, 2],
            |_| Some(busy(&[(1, 0), (2, 5)], &[])),
            &mut held,
            |cpu| reserve(&prefix, cpu),
        )
        .expect("two CPUs");
        assert_eq!((choice.cpu, &choice.hold), (1, &Hold::Kept), "{choice}");
        assert_eq!(held.as_ref().map(|(cpu, _)| *cpu), Some(1));
    }

    #[test]
    fn a_run_that_moves_releases_the_cpu_the_process_held() {
        let prefix = private_prefix();
        let mut held = reserve(&prefix, 1).expect("bind").map(|fd| (1, fd));
        // The allowed set no longer has CPU 1: the run reserves CPU 2.
        let choice = choose_for_run_with(
            &[2, 3],
            |_| Some(busy(&[(2, 0), (3, 5)], &[])),
            &mut held,
            |cpu| reserve(&prefix, cpu),
        )
        .expect("two CPUs");
        assert_eq!(
            (choice.cpu, &choice.hold),
            (2, &Hold::Reserved { passed_over: 0 })
        );
        assert_eq!(held.as_ref().map(|(cpu, _)| *cpu), Some(2));
        assert!(
            reserve(&prefix, 1).expect("bind").is_some(),
            "the CPU the process moved away from is free again"
        );
        // Then a run restricted to CPU 4, which another run holds, shares it
        // and leaves this process holding nothing.
        let _other = reserve(&prefix, 4).expect("bind").expect("free");
        let choice = choose_for_run_with(&[4], |_| None, &mut held, |cpu| reserve(&prefix, cpu))
            .expect("one CPU");
        assert_eq!((choice.cpu, &choice.hold), (4, &Hold::Shared));
        assert!(held.is_none(), "a shared run holds no reservation");
        assert!(
            reserve(&prefix, 2).expect("bind").is_some(),
            "and the CPU the process held before is free again"
        );
    }

    /// A choice of `cpu` held as `hold`, for the admission tests.
    fn chose(cpu: usize, hold: Hold) -> Choice {
        Choice {
            cpu,
            basis: Basis::OnlyAllowed,
            hold,
        }
    }

    fn host() -> Coordination {
        Coordination::Host("/dev/shm/hermit-pinned-cpu".into())
    }

    /// Runs `admit` with the attempts `answers` gives in turn, and returns
    /// its result, the notices and the pauses.
    fn admitted(
        policy: PinnedCpuPolicy,
        answers: Vec<(Choice, Coordination)>,
    ) -> (
        Result<Option<Choice>, PlacementRefusal>,
        Vec<String>,
        Vec<Duration>,
    ) {
        let mut answers = answers.into_iter();
        let mut last = None;
        let mut notices = Vec::new();
        let mut pauses = Vec::new();
        let result = admit(
            &[2, 3],
            policy,
            |_| {
                if let Some(answer) = answers.next() {
                    last = Some(answer);
                }
                last.clone()
            },
            |pause| pauses.push(pause),
            &mut |text| notices.push(text.to_owned()),
        );
        (result, notices, pauses)
    }

    /// (a) Every allowed CPU held: by default the run shares one and says so
    /// by name, naming the CPU set; under `refuse` it is refused naming the
    /// set; a reserved run says nothing.
    #[test]
    fn a_shared_cpu_is_noticed_by_name_and_refused_on_request() {
        let (result, notices, _) = admitted(
            PinnedCpuPolicy::Share,
            vec![(chose(2, Hold::Shared), host())],
        );
        assert_eq!(result, Ok(Some(chose(2, Hold::Shared))));
        assert_eq!(notices.len(), 1, "{notices:?}");
        assert!(
            notices[0].starts_with("hermit: warning: cpu-shared: every allowed CPU (2-3) is held"),
            "{notices:?}"
        );
        assert!(notices[0].contains("shares CPU 2"), "{notices:?}");

        let (result, notices, _) = admitted(
            PinnedCpuPolicy::Refuse,
            vec![(chose(2, Hold::Shared), host())],
        );
        let refusal = result.expect_err("refused").0;
        assert!(
            refusal.starts_with("--pinned-cpu=refuse: refusing the run on CPUs 2-3: cpu-shared:"),
            "{refusal}"
        );
        assert!(notices.is_empty(), "a refusal is the message: {notices:?}");

        let reserved = chose(3, Hold::Reserved { passed_over: 1 });
        for policy in [
            PinnedCpuPolicy::Share,
            PinnedCpuPolicy::Refuse,
            PinnedCpuPolicy::Wait(DEFAULT_WAIT),
        ] {
            let (result, notices, pauses) = admitted(policy, vec![(reserved.clone(), host())]);
            assert_eq!(result, Ok(Some(reserved.clone())), "{policy}");
            assert!(notices.is_empty() && pauses.is_empty(), "{policy}");
        }
    }

    /// `wait` sleeps between attempts (`WAIT_INTERVAL`, never a spin) until a
    /// CPU comes free, and refuses after its bound, naming the CPU set and
    /// the wait.
    #[test]
    fn wait_sleeps_until_a_cpu_frees_and_refuses_after_its_bound() {
        let shared = (chose(2, Hold::Shared), host());
        let freed = (chose(3, Hold::Reserved { passed_over: 0 }), host());
        let (result, notices, pauses) = admitted(
            PinnedCpuPolicy::Wait(Duration::from_secs(10)),
            vec![shared.clone(), shared.clone(), freed.clone()],
        );
        assert_eq!(result, Ok(Some(freed.0)));
        assert_eq!(pauses, vec![WAIT_INTERVAL; 2]);
        assert!(notices.is_empty(), "{notices:?}");

        let (result, _, pauses) =
            admitted(PinnedCpuPolicy::Wait(Duration::from_secs(1)), vec![shared]);
        let refusal = result.expect_err("refused after the wait").0;
        assert_eq!(
            refusal,
            "--pinned-cpu=wait:1: refusing the run on CPUs 2-3: every allowed CPU stayed held by \
             another Hermit run for the whole 1 s wait"
        );
        assert_eq!(
            pauses,
            vec![WAIT_INTERVAL; 2],
            "1 s at 500 ms: three attempts"
        );
    }

    /// (b) A reservation that could not be made is noticed by name, and
    /// refused under `refuse` and `wait`, which cannot fix it.
    #[test]
    fn an_unreserved_cpu_is_noticed_by_name() {
        let unreserved = (
            chose(2, Hold::Unreserved("Operation not permitted".into())),
            host(),
        );
        let (result, notices, _) = admitted(PinnedCpuPolicy::Share, vec![unreserved.clone()]);
        assert!(result.is_ok());
        assert_eq!(
            notices,
            vec![
                "hermit: warning: cpu-unreserved: could not reserve CPU 2 (Operation not \
                 permitted); another Hermit run may be pinned to it."
                    .to_owned()
            ]
        );
        for policy in [PinnedCpuPolicy::Refuse, PinnedCpuPolicy::Wait(DEFAULT_WAIT)] {
            let (result, _, pauses) = admitted(policy, vec![unreserved.clone()]);
            assert!(
                result.expect_err("refused").0.contains("cpu-unreserved"),
                "{policy}"
            );
            assert!(pauses.is_empty(), "{policy}: waiting cannot fix it");
        }
    }

    /// (c) A run whose lock directory is unusable is reserved only within its
    /// network namespace, and says so, naming the namespace and the reason.
    #[test]
    fn a_reservation_within_one_network_namespace_only_is_noticed_by_name() {
        let netns_only = (
            chose(2, Hold::Reserved { passed_over: 0 }),
            Coordination::NetworkNamespaceOnly("/x is a symbolic link".into()),
        );
        let (result, notices, _) = admitted(PinnedCpuPolicy::Share, vec![netns_only.clone()]);
        assert!(result.is_ok());
        assert_eq!(notices.len(), 1, "{notices:?}");
        assert!(
            notices[0].starts_with("hermit: warning: cpu-netns-only: CPU 2 is reserved only"),
            "{notices:?}"
        );
        assert!(notices[0].contains("net:["), "{notices:?}");
        assert!(notices[0].contains("/x is a symbolic link"), "{notices:?}");
        assert!(notices[0].contains(LOCK_DIR_ENV), "{notices:?}");
        let (result, _, _) = admitted(PinnedCpuPolicy::Refuse, vec![netns_only]);
        assert!(result.expect_err("refused").0.contains("cpu-netns-only"));
    }

    #[test]
    fn the_policy_parses_and_renders() {
        for (text, policy) in [
            ("share", PinnedCpuPolicy::Share),
            ("refuse", PinnedCpuPolicy::Refuse),
            ("wait", PinnedCpuPolicy::Wait(DEFAULT_WAIT)),
            ("wait:30", PinnedCpuPolicy::Wait(Duration::from_secs(30))),
        ] {
            assert_eq!(text.parse::<PinnedCpuPolicy>(), Ok(policy));
            assert_eq!(policy.to_string(), text);
        }
        for bad in ["", "wait:", "wait:0", "wait:-1", "spin", "wait:1s"] {
            assert!(bad.parse::<PinnedCpuPolicy>().is_err(), "{bad:?}");
        }
        assert_eq!(cpu_set(&[0, 1, 2, 5, 7, 8]), "0-2,5,7-8");
    }

    /// A private lock directory for one test.
    fn private_lock_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        lock_dir(dir.path()).expect("a directory this user owns is usable");
        dir
    }

    /// The lock file holds a CPU against a run in any network namespace: the
    /// kernel ties a `flock` to the open file, not to a namespace, so a second
    /// lock (as from another namespace's run) is refused while the first is
    /// open, and granted once it closes. A run that reserves through both
    /// channels counts a CPU held when only the lock file is.
    #[test]
    fn a_held_lock_file_holds_its_cpu_whatever_the_socket_says() {
        let dir = private_lock_dir();
        let first = lock(dir.path(), 3).expect("lock").expect("free");
        assert!(first.as_raw_fd() >= RESERVATION_FD_FLOOR, "{first:?}");
        assert!(lock(dir.path(), 3).expect("lock").is_none(), "held");
        let prefix = private_prefix();
        assert!(
            reserve_both(&prefix, Some(dir.path()), 3)
                .expect("reserve")
                .is_none(),
            "a free socket name does not make a locked CPU free"
        );
        drop(first);
        let both = reserve_both(&prefix, Some(dir.path()), 3)
            .expect("reserve")
            .expect("free once the lock closes");
        assert!(
            reserve(&prefix, 3).expect("bind").is_none(),
            "the socket name is held too"
        );
        drop(both);
        assert!(lock(dir.path(), 3).expect("lock").is_some());
    }

    /// A lock file another user squatted, or replaced with a symlink, a FIFO or
    /// a directory, only makes its CPU count as held: no error, and no hang
    /// (the FIFO is opened without blocking).
    #[test]
    fn a_squatted_lock_file_only_counts_its_cpu_as_held() {
        use std::os::unix::fs::PermissionsExt;
        let dir = private_lock_dir();
        std::os::unix::fs::symlink("/etc/hostname", dir.path().join("1")).unwrap();
        nix::unistd::mkfifo(&dir.path().join("2"), nix::sys::stat::Mode::S_IRWXU).unwrap();
        fs::create_dir(dir.path().join("3")).unwrap();
        for cpu in [1, 2, 3] {
            assert!(
                lock(dir.path(), cpu).expect("not an error").is_none(),
                "CPU {cpu}"
            );
        }
        // Another user's unreadable file (root can open any file, so this
        // case holds only for other users).
        let squatted = dir.path().join("4");
        fs::write(&squatted, b"").unwrap();
        fs::set_permissions(&squatted, fs::Permissions::from_mode(0o000)).unwrap();
        if !nix::unistd::geteuid().is_root() {
            assert!(lock(dir.path(), 4).expect("not an error").is_none());
        }
        // A readable file of another user's is a shared lock file: locked
        // read-only. Here the same user, so it opens read-write.
        let shared = dir.path().join("5");
        fs::write(&shared, b"").unwrap();
        fs::set_permissions(&shared, fs::Permissions::from_mode(0o444)).unwrap();
        assert!(lock(dir.path(), 5).expect("lock").is_some());
    }

    /// The lock directory must be a real directory of this user's, or root's
    /// shared mode-1777 sticky one; `HERMIT_PINNED_CPU_DIR` changes only the
    /// path these checks apply to.
    #[test]
    fn only_an_own_or_shared_root_lock_directory_is_used() {
        let parent = tempfile::tempdir().unwrap();
        let created = parent.path().join("locks");
        lock_dir(&created).expect("created");
        let mode = {
            use std::os::unix::fs::PermissionsExt;
            fs::metadata(&created).unwrap().permissions().mode() & 0o7777
        };
        assert_eq!(mode, 0o1777, "created shared and sticky");
        let link = parent.path().join("link");
        std::os::unix::fs::symlink(&created, &link).unwrap();
        assert!(lock_dir(&link).unwrap_err().contains("symbolic link"));
        let file = parent.path().join("file");
        fs::write(&file, b"").unwrap();
        assert!(lock_dir(&file).unwrap_err().contains("not a directory"));
        assert!(acceptable_owner(1000, 0o40700, 1000));
        assert!(acceptable_owner(0, 0o41777, 1000), "the shared root form");
        assert!(!acceptable_owner(0, 0o40777, 1000), "root's, not sticky");
        assert!(!acceptable_owner(0, 0o41775, 1000), "root's, not 1777");
        assert!(!acceptable_owner(1001, 0o41777, 1000), "another user's");
    }

    /// Under a descriptor limit at or below the floor (`prlimit --nofile=256`)
    /// a reservation is still made, just below the limit, close-on-exec,
    /// instead of failing with `EINVAL` and leaving the run unreserved. Run
    /// in a child process, so the limit does not reach other tests.
    #[test]
    fn a_low_descriptor_limit_still_reserves() {
        let prefix = private_prefix();
        // SAFETY: the child calls only async-signal-safe functions until it
        // reaches `reserve`, and this test process runs no other test thread
        // under nextest; the child exits without unwinding.
        match unsafe { nix::unistd::fork() }.expect("fork") {
            nix::unistd::ForkResult::Child => {
                let code = (|| {
                    nix::sys::resource::setrlimit(
                        nix::sys::resource::Resource::RLIMIT_NOFILE,
                        256,
                        256,
                    )
                    .ok()?;
                    let fd = reserve(&prefix, 7).ok()??;
                    // SAFETY: F_GETFD on a descriptor this child owns.
                    let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) };
                    (fd.as_raw_fd() < 256 && fd.as_raw_fd() >= 128 && flags & libc::FD_CLOEXEC != 0)
                        .then_some(0)
                })()
                .unwrap_or(1);
                // SAFETY: exit the forked child without running destructors.
                unsafe { libc::_exit(code) }
            }
            nix::unistd::ForkResult::Parent { child } => {
                assert_eq!(
                    nix::sys::wait::waitpid(child, None).expect("waitpid"),
                    nix::sys::wait::WaitStatus::Exited(child, 0),
                    "the child reserved below the limit, close-on-exec"
                );
            }
        }
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
