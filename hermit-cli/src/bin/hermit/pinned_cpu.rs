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
//! channel holds is held. A directory Hermit creates is private to its user
//! (0700) unless it runs as root; root's shared form is mode 1777 with the
//! sticky bit (`open_lock_dir`). A container with a private `/dev/shm` has a
//! private copy of the default directory, so it coordinates only within
//! itself unless one directory is bind-mounted into every container and named
//! by `--pinned-cpu-dir`; nothing can detect that case from inside.
//!
//! A run that cannot keep other runs off its CPU (it shares one, cannot
//! reserve one, or can reserve one only within its network namespace) says
//! so by name on Hermit's own stderr, once per process, or, under `hermit run
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
pub(crate) fn choose_for_run(allowed: &[usize], dir: &Path) -> Option<(Choice, Coordination)> {
    let mut held = HELD.lock().unwrap_or_else(|poison| poison.into_inner());
    let (dir, coordination) = match open_lock_dir(dir) {
        Ok(fd) => (Some(fd), Coordination::Host(dir.to_path_buf())),
        Err(reason) => (None, Coordination::NetworkNamespaceOnly(reason)),
    };
    let choice = choose_for_run_with(allowed, observe, &mut held, |cpu| {
        reserve_both(RESERVATION_PREFIX, dir.as_ref(), cpu)
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
    // EINVAL: the floor is at or above the limit. EMFILE: every number from
    // the floor to the limit is taken (a limit just above the floor).
    if !matches!(error.raw_os_error(), Some(libc::EINVAL | libc::EMFILE)) {
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
/// namespaces (`lock`), unless `hermit run --pinned-cpu-dir` or
/// [`LOCK_DIR_ENV`] names another. A container with a private `/dev/shm` (the
/// podman and Docker default) has a private copy of this path, so its runs
/// coordinate only with runs in the same container; coordination across
/// containers needs one directory bind-mounted into each and named here.
pub(crate) const LOCK_DIR_DEFAULT: &str = "/dev/shm/hermit-pinned-cpu";

/// Names the lock directory in place of [`LOCK_DIR_DEFAULT`] where no
/// `--pinned-cpu-dir` is given (every subcommand but `hermit run` takes it
/// from here). It changes only the path: the directory is checked as the
/// default one is.
pub(crate) const LOCK_DIR_ENV: &str = "HERMIT_PINNED_CPU_DIR";

/// Turns off the notices when set to `0`, where no `--pinned-cpu-notice` is
/// given. It does not change placement or the policy.
pub(crate) const NOTICE_ENV: &str = "HERMIT_PINNED_CPU_NOTICE";

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

/// The mode a lock directory is created with: root's shared form (1777 with
/// the sticky bit), which every user's runs can lock in, and otherwise 0700,
/// private to the user, since no other user's run would use another user's
/// directory (`acceptable_owner`) and a writable one would only let others
/// squat its CPUs' files.
fn created_lock_dir_mode(euid: u32) -> libc::mode_t {
    if euid == 0 { 0o1777 } else { 0o700 }
}

/// Opens, or creates, the lock directory `base`, and checks the directory it
/// opened: a real directory (`O_NOFOLLOW`, so not a symlink), owned by this
/// user, or else root's shared form (mode 1777 with the sticky bit), so no
/// other user can remove or replace this user's lock files. The checks apply
/// to the open descriptor, which `lock` then opens files in (`openat`), so a
/// component swapped after the check cannot redirect them. Components above
/// `base` (`/dev/shm` itself) are the administrator's configuration and are
/// trusted as given.
fn open_lock_dir(base: &Path) -> Result<OwnedFd, String> {
    use std::os::unix::ffi::OsStrExt;
    let shown = base.display();
    let euid = nix::unistd::geteuid().as_raw();
    let path = std::ffi::CString::new(base.as_os_str().as_bytes())
        .map_err(|_| format!("{shown} contains a NUL byte"))?;
    let mode = created_lock_dir_mode(euid);
    // SAFETY: `path` is a NUL-terminated string.
    let created = unsafe { libc::mkdir(path.as_ptr(), mode) } == 0;
    if !created {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EEXIST) {
            return Err(format!("cannot create {shown}: {error}"));
        }
    }
    // SAFETY: `path` is a NUL-terminated string; the result is checked.
    let fd = unsafe {
        libc::open(
            path.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        let error = io::Error::last_os_error();
        // With both O_DIRECTORY and O_NOFOLLOW, a symlink can fail with
        // ENOTDIR as well as ELOOP, so ask which it was.
        let symlink = fs::symlink_metadata(base).is_ok_and(|meta| meta.file_type().is_symlink());
        return Err(match error.raw_os_error() {
            Some(libc::ELOOP | libc::ENOTDIR) if symlink => format!("{shown} is a symbolic link"),
            Some(libc::ENOTDIR) => format!("{shown} is not a directory"),
            _ => format!("cannot open {shown}: {error}"),
        });
    }
    // SAFETY: `fd` is a new descriptor nothing else owns.
    let dir = unsafe { OwnedFd::from_raw_fd(fd) };
    let stat =
        nix::sys::stat::fstat(&dir).map_err(|error| format!("cannot stat {shown}: {error}"))?;
    if !acceptable_owner(stat.st_uid, stat.st_mode, euid) {
        return Err(format!(
            "{shown} belongs to user {} and is not root's shared form (mode 1777, sticky); \
             as root, `install -d -m 1777 -o root -g root {shown}` creates the shared form",
            stat.st_uid
        ));
    }
    if created {
        // The umask may have cleared bits `mkdir` asked for.
        // SAFETY: fchmod on a directory this function owns.
        if unsafe { libc::fchmod(dir.as_raw_fd(), mode) } != 0 {
            return Err(format!(
                "cannot set the mode of {shown}: {}",
                io::Error::last_os_error()
            ));
        }
    }
    Ok(dir)
}

/// Whether a lock directory owned by `owner` with `mode` may be used by user
/// `euid`: its own, or root's shared mode-1777 sticky directory.
fn acceptable_owner(owner: u32, mode: u32, euid: u32) -> bool {
    owner == euid || (owner == 0 && mode & 0o7777 == 0o1777)
}

/// Hold `cpu`'s lock file in the lock directory `dir` (`open_lock_dir`) while
/// the returned descriptor is open: an exclusive `flock`, which the kernel
/// releases when the last descriptor for it closes, however its holders exit.
/// `Ok(None)` when the CPU counts as held: another run holds the lock, or the
/// file is not one this run can lock (another user's that it cannot open, a
/// symlink, or not a regular file), which a squatter could arrange but which
/// only ever makes this run pass the CPU over or share it. The file is opened
/// without following a symlink and without blocking (a FIFO in its place
/// cannot stall the run). A file this user owns is made mode 0644 whatever the
/// umask, so other users' runs in root's shared directory can lock it too.
fn lock(dir: &OwnedFd, cpu: usize) -> io::Result<Option<OwnedFd>> {
    let name = std::ffi::CString::new(cpu.to_string()).expect("a number has no NUL");
    let flags = libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK;
    let open = |extra: libc::c_int| {
        // SAFETY: `dir` is an open directory and `name` is NUL-terminated.
        let fd = unsafe { libc::openat(dir.as_raw_fd(), name.as_ptr(), flags | extra, 0o644) };
        if fd < 0 {
            Err(io::Error::last_os_error())
        } else {
            // SAFETY: `fd` is a new descriptor nothing else owns.
            Ok(unsafe { OwnedFd::from_raw_fd(fd) })
        }
    };
    let file = match open(libc::O_RDWR | libc::O_CREAT).or_else(|error| {
        // Another user's file, readable: a shared lock file.
        if error.raw_os_error() == Some(libc::EACCES) {
            open(libc::O_RDONLY)
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
    let stat = nix::sys::stat::fstat(&file)?;
    if stat.st_mode & libc::S_IFMT != libc::S_IFREG {
        return Ok(None);
    }
    if stat.st_uid == nix::unistd::geteuid().as_raw() && stat.st_mode & 0o7777 != 0o644 {
        // SAFETY: fchmod on a file this function owns.
        if unsafe { libc::fchmod(file.as_raw_fd(), 0o644) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    // SAFETY: flock on a descriptor this function owns.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return Ok(Some(move_high(file)?));
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::EWOULDBLOCK) {
        return Ok(None);
    }
    Err(error)
}

/// Reserve `cpu` through both channels: its lock file in `dir`, when there is
/// one, and then its abstract socket name. `Ok(None)` when either is held.
fn reserve_both(
    prefix: &str,
    dir: Option<&OwnedFd>,
    cpu: usize,
) -> io::Result<Option<Reservation>> {
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

/// How this process's pinned runs are placed: `hermit run`'s
/// `--pinned-cpu` and `--timeout` (`RunOpts`), and the
/// [`PinnedCpuPlacementArgs`] every pinning subcommand takes; for any other
/// caller, the defaults with the environment's fallbacks (`from_env`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PinnedCpuSettings {
    pub(crate) policy: PinnedCpuPolicy,
    pub(crate) dir: std::path::PathBuf,
    pub(crate) notice: bool,
    /// The run's own wall-clock bound (`--timeout`), which a `wait` never
    /// outlasts: the wait comes before the timeout's alarm is armed.
    pub(crate) wait_cap: Option<Duration>,
}

impl PinnedCpuSettings {
    /// The default policy (`share`, no wait bound), and the directory and
    /// notice setting from [`LOCK_DIR_ENV`] and [`NOTICE_ENV`].
    pub(crate) fn from_env() -> Self {
        Self {
            policy: PinnedCpuPolicy::Share,
            dir: Self::dir_from_env(),
            notice: Self::notice_from_env(),
            wait_cap: None,
        }
    }

    /// [`LOCK_DIR_ENV`], else [`LOCK_DIR_DEFAULT`].
    pub(crate) fn dir_from_env() -> std::path::PathBuf {
        Self::dir_from(&|name| std::env::var_os(name))
    }

    /// Whether [`NOTICE_ENV`] leaves the notices on.
    pub(crate) fn notice_from_env() -> bool {
        Self::notice_from(&|name| std::env::var_os(name))
    }

    /// [`Self::dir_from_env`], reading the environment through `env`.
    fn dir_from(env: &dyn Fn(&str) -> Option<std::ffi::OsString>) -> std::path::PathBuf {
        env(LOCK_DIR_ENV)
            .filter(|dir| !dir.is_empty())
            .map_or_else(|| LOCK_DIR_DEFAULT.into(), std::path::PathBuf::from)
    }

    /// [`Self::notice_from_env`], reading the environment through `env`.
    fn notice_from(env: &dyn Fn(&str) -> Option<std::ffi::OsString>) -> bool {
        env(NOTICE_ENV).is_none_or(|value| value != "0")
    }
}

/// `--pinned-cpu-dir` and `--pinned-cpu-notice`, shared by every subcommand
/// that pins a guest (`hermit run`, `hermit record start` and `hermit
/// replay`). Each falls back to its environment variable when absent.
#[derive(Clone, Debug, Default, clap::Args)]
pub(crate) struct PinnedCpuPlacementArgs {
    /// The directory whose per-CPU lock files reserve a pinned run's CPU
    /// against Hermit runs in other network namespaces (default
    /// `/dev/shm/hermit-pinned-cpu`, or `HERMIT_PINNED_CPU_DIR` when set). It
    /// must be a real directory owned by this user, or root's shared mode-1777
    /// sticky directory (`install -d -m 1777 -o root -g root PATH`). A container
    /// with a private `/dev/shm` (the podman and Docker default) coordinates
    /// only within itself unless one directory is bind-mounted into every
    /// container and named here.
    #[clap(long, value_name = "PATH")]
    pub(crate) pinned_cpu_dir: Option<std::path::PathBuf>,

    /// Whether a pinned run prints its cpu-shared, cpu-unreserved or
    /// cpu-netns-only notice on stderr (default `on`, or `off` when
    /// `HERMIT_PINNED_CPU_NOTICE=0`). Placement is the same either way.
    #[clap(long, value_name = "on|off", value_parser = ["on", "off"])]
    pub(crate) pinned_cpu_notice: Option<String>,
}

impl PinnedCpuPlacementArgs {
    /// The settings these flags give under `policy` and `wait_cap`, with
    /// [`LOCK_DIR_ENV`] and [`NOTICE_ENV`] as the fallbacks.
    pub(crate) fn settings(
        &self,
        policy: PinnedCpuPolicy,
        wait_cap: Option<Duration>,
    ) -> PinnedCpuSettings {
        self.settings_with_env(policy, wait_cap, &|name| std::env::var_os(name))
    }

    /// [`Self::settings`], reading the fallbacks through `env`.
    fn settings_with_env(
        &self,
        policy: PinnedCpuPolicy,
        wait_cap: Option<Duration>,
        env: &dyn Fn(&str) -> Option<std::ffi::OsString>,
    ) -> PinnedCpuSettings {
        PinnedCpuSettings {
            policy,
            dir: self
                .pinned_cpu_dir
                .clone()
                .unwrap_or_else(|| PinnedCpuSettings::dir_from(env)),
            notice: match self.pinned_cpu_notice.as_deref() {
                Some(value) => value == "on",
                None => PinnedCpuSettings::notice_from(env),
            },
            wait_cap,
        }
    }

    /// The flags that were given, as the environment fallbacks a child
    /// `hermit` reads when it is not given the flags: `hermit replay
    /// --dap-listen` execs `hermit-dap`, which starts its own `hermit replay`.
    /// `--pinned-cpu-notice=on` sets the variable to `1`, so that it also
    /// overrides an inherited `0`.
    pub(crate) fn to_env(&self) -> Vec<(&'static str, std::ffi::OsString)> {
        let mut env = Vec::new();
        if let Some(dir) = &self.pinned_cpu_dir {
            env.push((LOCK_DIR_ENV, dir.clone().into_os_string()));
        }
        if let Some(notice) = &self.pinned_cpu_notice {
            env.push((NOTICE_ENV, if notice == "on" { "1" } else { "0" }.into()));
        }
        env
    }

    /// The flags that were given, spelled for another `hermit` command line
    /// (`hermit replay`'s rerun through `hermit run --from`).
    pub(crate) fn to_args(&self) -> Vec<std::ffi::OsString> {
        let mut args = Vec::new();
        if let Some(dir) = &self.pinned_cpu_dir {
            let mut arg = std::ffi::OsString::from("--pinned-cpu-dir=");
            arg.push(dir);
            args.push(arg);
        }
        if let Some(notice) = &self.pinned_cpu_notice {
            args.push(format!("--pinned-cpu-notice={notice}").into());
        }
        args
    }
}

/// The settings the current subcommand gave (`set_settings`); `None` until
/// one does, which reads as [`PinnedCpuSettings::from_env`].
static SETTINGS: Mutex<Option<PinnedCpuSettings>> = Mutex::new(None);

/// Set how every later pinned run of this process is placed. `hermit run`
/// sets its options; `hermit record start` sets the defaults, so that it
/// never inherits a policy another subcommand set.
pub(crate) fn set_settings(settings: PinnedCpuSettings) {
    *SETTINGS.lock().unwrap_or_else(|poison| poison.into_inner()) = Some(settings);
}

/// A run that could not hold a CPU of its own, under `--pinned-cpu=refuse` or
/// `wait`.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct PlacementRefusal(pub(crate) String);

/// What kept a run from holding a CPU of its own, or `None` when nothing did.
fn shortfall<'a>(choice: &'a Choice, coordination: &'a Coordination) -> Option<Shortfall<'a>> {
    match (&choice.hold, coordination) {
        (Hold::Shared, _) => Some(Shortfall::Shared),
        (Hold::Unreserved(error), _) => Some(Shortfall::Unreserved(error)),
        (_, Coordination::NetworkNamespaceOnly(reason)) => Some(Shortfall::NetnsOnly(reason)),
        _ => None,
    }
}

enum Shortfall<'a> {
    Shared,
    Unreserved(&'a str),
    NetnsOnly(&'a str),
}

impl Shortfall<'_> {
    /// The named notice a `share` run prints.
    fn notice(&self, allowed: &[usize], cpu: usize) -> String {
        let set = cpu_set(allowed);
        match self {
            Shortfall::Shared => format!(
                "hermit: warning: cpu-shared: every allowed CPU ({set}) is held by another Hermit \
                 run; this run shares CPU {cpu} and runs slower (placement only: guest output is \
                 unaffected). --pinned-cpu=wait or =refuse avoids sharing."
            ),
            Shortfall::Unreserved(error) => format!(
                "hermit: warning: cpu-unreserved: could not reserve CPU {cpu} ({error}); another \
                 Hermit run may be pinned to it."
            ),
            Shortfall::NetnsOnly(reason) => format!(
                "hermit: warning: cpu-netns-only: CPU {cpu} is reserved only against Hermit runs in \
                 this network namespace ({}): {reason}. Runs in other network namespaces may pick \
                 the same CPU; name a lock directory they share with --pinned-cpu-dir.",
                network_namespace()
            ),
        }
    }

    /// The refusal under `refuse`, or `wait` when waiting cannot help.
    fn refusal(&self, policy: PinnedCpuPolicy, allowed: &[usize], cpu: usize) -> String {
        let set = cpu_set(allowed);
        match self {
            Shortfall::Shared => format!(
                "--pinned-cpu={policy}: every allowed CPU ({set}) is held by another Hermit run, \
                 so the run was not started. Retry when one is free, wait for one with \
                 --pinned-cpu=wait[:SECONDS], allow more CPUs, or run on a shared CPU with \
                 --pinned-cpu=share."
            ),
            Shortfall::Unreserved(error) => format!(
                "--pinned-cpu={policy}: CPU {cpu} of the allowed CPUs ({set}) could not be \
                 reserved ({error}), so the run was not started. Fix that cause, or run \
                 unreserved with --pinned-cpu=share."
            ),
            Shortfall::NetnsOnly(reason) => format!(
                "--pinned-cpu={policy}: a reservation on the allowed CPUs ({set}) would reach only \
                 Hermit runs in this network namespace ({}): {reason}. The run was not started. \
                 Name a usable lock directory with --pinned-cpu-dir=PATH, or run with \
                 --pinned-cpu=share.",
                network_namespace()
            ),
        }
    }

    /// The notice kind, which a process prints once (`place`).
    fn kind(&self) -> &'static str {
        match self {
            Shortfall::Shared => "cpu-shared",
            Shortfall::Unreserved(_) => "cpu-unreserved",
            Shortfall::NetnsOnly(_) => "cpu-netns-only",
        }
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

/// Place a pinned run on a CPU from `allowed` under `policy`, writing each
/// notice (its kind and text) to `notices`. `choose` makes one attempt
/// (`choose_for_run`). Under `wait`, a run whose every CPU is held prints one
/// "waiting" line, then sleeps (`pause`) and tries again until `elapsed`
/// reaches the wait's bound, the smaller of the policy's and `wait_cap`, and
/// is refused then. A refusal names the CPU set, why, and what to do.
fn admit(
    allowed: &[usize],
    policy: PinnedCpuPolicy,
    wait_cap: Option<Duration>,
    mut choose: impl FnMut(&[usize]) -> Option<(Choice, Coordination)>,
    mut pause: impl FnMut(Duration),
    mut elapsed: impl FnMut() -> Duration,
    notices: &mut dyn FnMut(&'static str, &str),
) -> Result<Option<Choice>, PlacementRefusal> {
    let mut announced = false;
    loop {
        let Some((choice, coordination)) = choose(allowed) else {
            return Ok(None);
        };
        let Some(shortfall) = shortfall(&choice, &coordination) else {
            return Ok(Some(choice));
        };
        match (policy, &shortfall) {
            (PinnedCpuPolicy::Share, _) => {
                notices(shortfall.kind(), &shortfall.notice(allowed, choice.cpu));
                return Ok(Some(choice));
            }
            (PinnedCpuPolicy::Wait(wait), Shortfall::Shared) => {
                let bound = wait_cap.map_or(wait, |cap| cap.min(wait));
                let waited = elapsed();
                if waited >= bound {
                    let capped = if bound < wait {
                        " (bounded by --timeout)"
                    } else {
                        ""
                    };
                    return Err(PlacementRefusal(format!(
                        "--pinned-cpu={policy}: every allowed CPU ({}) stayed held by another \
                         Hermit run for the whole {} s wait{capped}, so the run was not started. \
                         Retry later, wait longer with --pinned-cpu=wait:SECONDS, allow more CPUs, \
                         or run on a shared CPU with --pinned-cpu=share.",
                        cpu_set(allowed),
                        bound.as_secs()
                    )));
                }
                if !announced {
                    announced = true;
                    notices(
                        "cpu-waiting",
                        &format!(
                            "hermit: waiting up to {} s for a CPU: every allowed CPU ({}) is held \
                             by another Hermit run (--pinned-cpu={policy}).",
                            bound.as_secs(),
                            cpu_set(allowed)
                        ),
                    );
                }
                pause(WAIT_INTERVAL.min(bound - waited));
            }
            (PinnedCpuPolicy::Refuse | PinnedCpuPolicy::Wait(_), _) => {
                return Err(PlacementRefusal(
                    shortfall.refusal(policy, allowed, choice.cpu),
                ));
            }
        }
    }
}

/// The notice kinds this process has printed: a `--verify` pair, or analyze's
/// trials, print each kind once.
static NOTICED: Mutex<BTreeSet<&'static str>> = Mutex::new(BTreeSet::new());

/// [`admit`] for this process: the settings `set_settings` gave, real
/// attempts, sleeps and time, and each kind of notice printed once on
/// Hermit's own stderr unless the notices are off.
pub(crate) fn place(allowed: &[usize]) -> Result<Option<Choice>, PlacementRefusal> {
    let settings = SETTINGS
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .clone()
        .unwrap_or_else(PinnedCpuSettings::from_env);
    let start = std::time::Instant::now();
    admit(
        allowed,
        settings.policy,
        settings.wait_cap,
        |allowed| choose_for_run(allowed, &settings.dir),
        thread::sleep,
        || start.elapsed(),
        &mut |kind, text| {
            let first = NOTICED
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .insert(kind);
            if settings.notice && first {
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

    /// Every subcommand that pins a guest takes both placement flags and
    /// documents them in its `--help` (owner CLI rule 2: an environment
    /// variable is never the only interface).
    #[test]
    fn every_pinning_subcommand_takes_the_placement_flags() {
        use clap::CommandFactory;
        let mut hermit = crate::app::Args::command();
        hermit.build();
        let run = hermit.find_subcommand("run").expect("run");
        let replay = hermit.find_subcommand("replay").expect("replay");
        let start = hermit
            .find_subcommand("record")
            .and_then(|record| record.find_subcommand("start"))
            .expect("record start");
        for (name, command) in [("run", run), ("replay", replay), ("record start", start)] {
            for flag in ["pinned-cpu-dir", "pinned-cpu-notice"] {
                let arg = command
                    .get_arguments()
                    .find(|arg| arg.get_long() == Some(flag))
                    .unwrap_or_else(|| panic!("`hermit {name}` has no --{flag}"));
                assert!(!arg.is_hide_set(), "`hermit {name} --{flag}` is hidden");
                assert!(
                    arg.get_help().is_some(),
                    "`hermit {name} --{flag}` has no help"
                );
            }
        }
    }

    /// Given flags win over the environment, and a rerun passes on exactly
    /// the flags that were given, spelled so that they parse back the same.
    #[test]
    fn placement_flags_give_the_settings_and_pass_on_unchanged() {
        #[derive(clap::Parser)]
        struct Wrapper {
            #[clap(flatten)]
            placement: PinnedCpuPlacementArgs,
        }
        use clap::Parser;
        let given = Wrapper::parse_from([
            "hermit",
            "--pinned-cpu-dir=/srv/locks with space",
            "--pinned-cpu-notice=off",
        ])
        .placement;
        let settings = given.settings(PinnedCpuPolicy::Share, Some(Duration::from_secs(9)));
        assert_eq!(
            settings,
            PinnedCpuSettings {
                policy: PinnedCpuPolicy::Share,
                dir: "/srv/locks with space".into(),
                notice: false,
                wait_cap: Some(Duration::from_secs(9)),
            }
        );
        let args = given.to_args();
        assert_eq!(
            args,
            [
                "--pinned-cpu-dir=/srv/locks with space",
                "--pinned-cpu-notice=off"
            ]
        );
        let reparsed = Wrapper::parse_from(std::iter::once("hermit".into()).chain(args)).placement;
        assert_eq!(
            reparsed.settings(PinnedCpuPolicy::Share, None).dir,
            settings.dir
        );
        assert_eq!(reparsed.pinned_cpu_notice.as_deref(), Some("off"));
        assert!(PinnedCpuPlacementArgs::default().to_args().is_empty());
        let on = Wrapper::parse_from(["hermit", "--pinned-cpu-notice=on"]).placement;
        assert!(on.settings(PinnedCpuPolicy::Refuse, None).notice);
    }

    /// With both a flag and its environment variable set, the flag wins; with
    /// only the variable, the variable is used; with neither, the default.
    /// The environment is passed in, so no process-wide variable is touched.
    #[test]
    fn a_placement_flag_wins_over_its_environment_variable() {
        #[derive(clap::Parser)]
        struct Wrapper {
            #[clap(flatten)]
            placement: PinnedCpuPlacementArgs,
        }
        use clap::Parser;
        let both = |name: &str| match name {
            LOCK_DIR_ENV => Some("/from/env".into()),
            NOTICE_ENV => Some("0".into()),
            _ => None,
        };
        let notice_on_in_env = |name: &str| (name == NOTICE_ENV).then(|| "1".into());
        let neither = |_: &str| None;

        let flags = Wrapper::parse_from([
            "hermit",
            "--pinned-cpu-dir=/from/flag",
            "--pinned-cpu-notice=on",
        ])
        .placement;
        let given = flags.settings_with_env(PinnedCpuPolicy::Share, None, &both);
        assert_eq!(given.dir, std::path::Path::new("/from/flag"));
        assert!(
            given.notice,
            "--pinned-cpu-notice=on lost to {NOTICE_ENV}=0"
        );
        let off = Wrapper::parse_from(["hermit", "--pinned-cpu-notice=off"]).placement;
        assert!(
            !off.settings_with_env(PinnedCpuPolicy::Share, None, &notice_on_in_env)
                .notice,
            "--pinned-cpu-notice=off lost to {NOTICE_ENV}=1"
        );

        let none = PinnedCpuPlacementArgs::default();
        let from_env = none.settings_with_env(PinnedCpuPolicy::Share, None, &both);
        assert_eq!(from_env.dir, std::path::Path::new("/from/env"));
        assert!(!from_env.notice);
        let default = none.settings_with_env(PinnedCpuPolicy::Share, None, &neither);
        assert_eq!(default.dir, std::path::Path::new(LOCK_DIR_DEFAULT));
        assert!(default.notice);

        assert_eq!(
            flags.to_env(),
            [
                (LOCK_DIR_ENV, "/from/flag".into()),
                (NOTICE_ENV, std::ffi::OsString::from("1")),
            ]
        );
        assert_eq!(off.to_env(), [(NOTICE_ENV, std::ffi::OsString::from("0"))]);
        assert!(none.to_env().is_empty());
    }

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

    /// What `admit` did with the attempts `answers` gives in turn: its
    /// result, the notices (kind and text) and the pauses, on a clock that
    /// advances only by the pauses.
    struct Admitted {
        result: Result<Option<Choice>, PlacementRefusal>,
        notices: Vec<(&'static str, String)>,
        pauses: Vec<Duration>,
    }

    fn admitted(
        policy: PinnedCpuPolicy,
        wait_cap: Option<Duration>,
        answers: Vec<(Choice, Coordination)>,
    ) -> Admitted {
        let mut answers = answers.into_iter();
        let mut last = None;
        let mut notices = Vec::new();
        let pauses = std::cell::RefCell::new(Vec::new());
        let result = admit(
            &[2, 3],
            policy,
            wait_cap,
            |_| {
                if let Some(answer) = answers.next() {
                    last = Some(answer);
                }
                last.clone()
            },
            |pause| pauses.borrow_mut().push(pause),
            || pauses.borrow().iter().sum(),
            &mut |kind, text| notices.push((kind, text.to_owned())),
        );
        Admitted {
            result,
            notices,
            pauses: pauses.into_inner(),
        }
    }

    /// (a) Every allowed CPU held: by default the run shares one and says so
    /// by name, naming the CPU set; under `refuse` it is refused with a
    /// message that says the run was not started and what to do instead; a
    /// reserved run says nothing.
    #[test]
    fn a_shared_cpu_is_noticed_by_name_and_refused_on_request() {
        let shared = (chose(2, Hold::Shared), host());
        let run = admitted(PinnedCpuPolicy::Share, None, vec![shared.clone()]);
        assert_eq!(run.result, Ok(Some(chose(2, Hold::Shared))));
        assert_eq!(
            run.notices,
            vec![(
                "cpu-shared",
                "hermit: warning: cpu-shared: every allowed CPU (2-3) is held by another Hermit \
                 run; this run shares CPU 2 and runs slower (placement only: guest output is \
                 unaffected). --pinned-cpu=wait or =refuse avoids sharing."
                    .to_owned()
            )]
        );

        let run = admitted(PinnedCpuPolicy::Refuse, None, vec![shared]);
        assert_eq!(
            run.result,
            Err(PlacementRefusal(
                "--pinned-cpu=refuse: every allowed CPU (2-3) is held by another Hermit run, so \
                 the run was not started. Retry when one is free, wait for one with \
                 --pinned-cpu=wait[:SECONDS], allow more CPUs, or run on a shared CPU with \
                 --pinned-cpu=share."
                    .to_owned()
            ))
        );
        assert!(
            run.notices.is_empty(),
            "a refusal is the message: {:?}",
            run.notices
        );

        let reserved = chose(3, Hold::Reserved { passed_over: 1 });
        for policy in [
            PinnedCpuPolicy::Share,
            PinnedCpuPolicy::Refuse,
            PinnedCpuPolicy::Wait(DEFAULT_WAIT),
        ] {
            let run = admitted(policy, None, vec![(reserved.clone(), host())]);
            assert_eq!(run.result, Ok(Some(reserved.clone())), "{policy}");
            assert!(run.notices.is_empty() && run.pauses.is_empty(), "{policy}");
        }
    }

    /// `wait` prints one "waiting" line, sleeps between attempts
    /// (`WAIT_INTERVAL`, never a spin) until a CPU comes free, and refuses
    /// once the wait's time has passed, naming the CPU set, the wait and what
    /// to do; `--timeout` bounds the wait too, and the refusal says so.
    #[test]
    fn wait_sleeps_until_a_cpu_frees_and_refuses_after_its_bound() {
        let shared = (chose(2, Hold::Shared), host());
        let freed = (chose(3, Hold::Reserved { passed_over: 0 }), host());
        let run = admitted(
            PinnedCpuPolicy::Wait(Duration::from_secs(10)),
            None,
            vec![shared.clone(), shared.clone(), freed.clone()],
        );
        assert_eq!(run.result, Ok(Some(freed.0)));
        assert_eq!(run.pauses, vec![WAIT_INTERVAL; 2]);
        assert_eq!(
            run.notices,
            vec![(
                "cpu-waiting",
                "hermit: waiting up to 10 s for a CPU: every allowed CPU (2-3) is held by \
                 another Hermit run (--pinned-cpu=wait:10)."
                    .to_owned()
            )]
        );

        let run = admitted(
            PinnedCpuPolicy::Wait(Duration::from_secs(1)),
            None,
            vec![shared.clone()],
        );
        assert_eq!(
            run.result,
            Err(PlacementRefusal(
                "--pinned-cpu=wait:1: every allowed CPU (2-3) stayed held by another Hermit run \
                 for the whole 1 s wait, so the run was not started. Retry later, wait longer \
                 with --pinned-cpu=wait:SECONDS, allow more CPUs, or run on a shared CPU with \
                 --pinned-cpu=share."
                    .to_owned()
            ))
        );
        assert_eq!(run.pauses, vec![WAIT_INTERVAL; 2], "1 s of 500 ms sleeps");

        let run = admitted(
            PinnedCpuPolicy::Wait(DEFAULT_WAIT),
            Some(Duration::from_secs(2)),
            vec![shared],
        );
        assert_eq!(run.pauses.iter().sum::<Duration>(), Duration::from_secs(2));
        assert!(
            run.result.expect_err("refused").0.contains(
                "for the whole 2 s wait (bounded by --timeout), so the run was not started"
            ),
        );
    }

    /// (b) A reservation that could not be made is noticed by name, and
    /// refused under `refuse` and `wait`, which cannot fix it, with a
    /// message that names the cause and the way to run anyway.
    #[test]
    fn an_unreserved_cpu_is_noticed_by_name() {
        let unreserved = (
            chose(2, Hold::Unreserved("Operation not permitted".into())),
            host(),
        );
        let run = admitted(PinnedCpuPolicy::Share, None, vec![unreserved.clone()]);
        assert!(run.result.is_ok());
        assert_eq!(
            run.notices,
            vec![(
                "cpu-unreserved",
                "hermit: warning: cpu-unreserved: could not reserve CPU 2 (Operation not \
                 permitted); another Hermit run may be pinned to it."
                    .to_owned()
            )]
        );
        for policy in [PinnedCpuPolicy::Refuse, PinnedCpuPolicy::Wait(DEFAULT_WAIT)] {
            let run = admitted(policy, None, vec![unreserved.clone()]);
            assert_eq!(
                run.result,
                Err(PlacementRefusal(format!(
                    "--pinned-cpu={policy}: CPU 2 of the allowed CPUs (2-3) could not be \
                     reserved (Operation not permitted), so the run was not started. Fix that \
                     cause, or run unreserved with --pinned-cpu=share."
                )))
            );
            assert!(run.pauses.is_empty(), "{policy}: waiting cannot fix it");
        }
    }

    /// (c) A run whose lock directory is unusable is reserved only within its
    /// network namespace, and says so, naming the namespace, the reason and
    /// `--pinned-cpu-dir`; `refuse` refuses it, naming the same.
    #[test]
    fn a_reservation_within_one_network_namespace_only_is_noticed_by_name() {
        let netns = network_namespace();
        assert!(netns.starts_with("net:["), "{netns}");
        let netns_only = (
            chose(2, Hold::Reserved { passed_over: 0 }),
            Coordination::NetworkNamespaceOnly("/x is a symbolic link".into()),
        );
        let run = admitted(PinnedCpuPolicy::Share, None, vec![netns_only.clone()]);
        assert!(run.result.is_ok());
        assert_eq!(
            run.notices,
            vec![(
                "cpu-netns-only",
                format!(
                    "hermit: warning: cpu-netns-only: CPU 2 is reserved only against Hermit runs \
                     in this network namespace ({netns}): /x is a symbolic link. Runs in other \
                     network namespaces may pick the same CPU; name a lock directory they share \
                     with --pinned-cpu-dir."
                )
            )]
        );
        let run = admitted(PinnedCpuPolicy::Refuse, None, vec![netns_only]);
        assert_eq!(
            run.result,
            Err(PlacementRefusal(format!(
                "--pinned-cpu=refuse: a reservation on the allowed CPUs (2-3) would reach only \
                 Hermit runs in this network namespace ({netns}): /x is a symbolic link. The run \
                 was not started. Name a usable lock directory with --pinned-cpu-dir=PATH, or run \
                 with --pinned-cpu=share."
            )))
        );
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

    /// A private lock directory for one test, and its open descriptor.
    fn private_lock_dir() -> (tempfile::TempDir, OwnedFd) {
        let dir = tempfile::tempdir().expect("tempdir");
        let fd = open_lock_dir(dir.path()).expect("a directory this user owns is usable");
        (dir, fd)
    }

    /// Whether `fd` is close-on-exec.
    fn close_on_exec(fd: &OwnedFd) -> bool {
        // SAFETY: F_GETFD on a descriptor this test owns.
        let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) };
        flags >= 0 && flags & libc::FD_CLOEXEC != 0
    }

    /// The lock file holds a CPU against a run in any network namespace: the
    /// kernel ties a `flock` to the open file, not to a namespace, so a second
    /// lock (as from another namespace's run) is refused while the first is
    /// open, and granted once it closes. The lock sits on a high
    /// close-on-exec descriptor, so no guest inherits it. A run that reserves
    /// through both channels counts a CPU held when only the lock file is.
    #[test]
    fn a_held_lock_file_holds_its_cpu_whatever_the_socket_says() {
        let (_dir, fd) = private_lock_dir();
        let first = lock(&fd, 3).expect("lock").expect("free");
        assert!(first.as_raw_fd() >= RESERVATION_FD_FLOOR, "{first:?}");
        assert!(
            close_on_exec(&first),
            "the lock descriptor is close-on-exec"
        );
        assert!(lock(&fd, 3).expect("lock").is_none(), "held");
        let prefix = private_prefix();
        assert!(
            reserve_both(&prefix, Some(&fd), 3)
                .expect("reserve")
                .is_none(),
            "a free socket name does not make a locked CPU free"
        );
        drop(first);
        let both = reserve_both(&prefix, Some(&fd), 3)
            .expect("reserve")
            .expect("free once the lock closes");
        assert!(
            reserve(&prefix, 3).expect("bind").is_none(),
            "the socket name is held too"
        );
        drop(both);
        assert!(lock(&fd, 3).expect("lock").is_some());
    }

    /// A lock file another user squatted, or replaced with a symlink (to a
    /// file this user can write, or dangling), a FIFO or a directory, only
    /// makes its CPU count as held: no error, no hang (the FIFO is opened
    /// without blocking), and neither symlink is followed (the writable
    /// target is not locked and the dangling one is not created).
    #[test]
    fn a_squatted_lock_file_only_counts_its_cpu_as_held() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, fd) = private_lock_dir();
        let writable = dir.path().join("writable-target");
        fs::write(&writable, b"").unwrap();
        std::os::unix::fs::symlink(&writable, dir.path().join("1")).unwrap();
        let dangling = dir.path().join("dangling-target");
        std::os::unix::fs::symlink(&dangling, dir.path().join("6")).unwrap();
        nix::unistd::mkfifo(&dir.path().join("2"), nix::sys::stat::Mode::S_IRWXU).unwrap();
        fs::create_dir(dir.path().join("3")).unwrap();
        for cpu in [1, 2, 3, 6] {
            assert!(lock(&fd, cpu).expect("not an error").is_none(), "CPU {cpu}");
        }
        assert!(
            !dangling.exists(),
            "the dangling symlink's target was created"
        );
        let target = fs::File::open(&writable).unwrap();
        // SAFETY: flock on a descriptor this test owns.
        assert_eq!(
            unsafe { libc::flock(target.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0,
            "the symlink's target was locked through the symlink"
        );
        // An unreadable file (root can open any file, so this case holds
        // only for other users).
        let squatted = dir.path().join("4");
        fs::write(&squatted, b"").unwrap();
        fs::set_permissions(&squatted, fs::Permissions::from_mode(0o000)).unwrap();
        if !nix::unistd::geteuid().is_root() {
            assert!(lock(&fd, 4).expect("not an error").is_none());
        }
        // A read-only file of this user's is still locked, and made 0644.
        let shared = dir.path().join("5");
        fs::write(&shared, b"").unwrap();
        fs::set_permissions(&shared, fs::Permissions::from_mode(0o444)).unwrap();
        assert!(lock(&fd, 5).expect("lock").is_some());
        assert_eq!(
            fs::metadata(&shared).unwrap().permissions().mode() & 0o7777,
            0o644
        );
    }

    /// The lock directory must be a real directory of this user's, or root's
    /// shared mode-1777 sticky one; one it creates is private to the user
    /// (0700) unless it runs as root. `--pinned-cpu-dir` and
    /// `HERMIT_PINNED_CPU_DIR` change only the path these checks apply to.
    #[test]
    fn only_an_own_or_shared_root_lock_directory_is_used() {
        use std::os::unix::fs::PermissionsExt;
        let parent = tempfile::tempdir().unwrap();
        let created = parent.path().join("locks");
        open_lock_dir(&created).expect("created");
        let euid = nix::unistd::geteuid().as_raw();
        assert_eq!(
            fs::metadata(&created).unwrap().permissions().mode() & 0o7777,
            created_lock_dir_mode(euid)
        );
        assert_eq!(created_lock_dir_mode(1000), 0o700);
        assert_eq!(created_lock_dir_mode(0), 0o1777);
        let link = parent.path().join("link");
        std::os::unix::fs::symlink(&created, &link).unwrap();
        assert!(
            open_lock_dir(&link)
                .unwrap_err()
                .ends_with("is a symbolic link")
        );
        let file = parent.path().join("file");
        fs::write(&file, b"").unwrap();
        assert!(
            open_lock_dir(&file)
                .unwrap_err()
                .ends_with("is not a directory")
        );
        if euid != 0 {
            // Root's, but not the shared form: the refusal names the remedy.
            let refusal = open_lock_dir(Path::new("/")).unwrap_err();
            assert!(
                refusal.ends_with(
                    "/ belongs to user 0 and is not root's shared form (mode 1777, sticky); as \
                     root, `install -d -m 1777 -o root -g root /` creates the shared form"
                ),
                "{refusal}"
            );
        }
        assert!(acceptable_owner(1000, 0o40700, 1000));
        assert!(acceptable_owner(0, 0o41777, 1000), "the shared root form");
        assert!(!acceptable_owner(0, 0o40777, 1000), "root's, not sticky");
        assert!(!acceptable_owner(0, 0o41775, 1000), "root's, not 1777");
        assert!(!acceptable_owner(1001, 0o41777, 1000), "another user's");
    }

    /// Set in the environment of a child process `in_a_child` starts, naming
    /// the check it runs.
    const CHILD_CHECK_ENV: &str = "HERMIT_PINNED_CPU_TEST_CHILD";

    /// Runs `check` (the name of an `#[ignore]`d test of this module) in a
    /// child process of this test binary, with the descriptor limit
    /// `nofile` and the umask `umask` set before it starts: both are
    /// process-wide, so they must not reach other tests. Returns whether the
    /// check passed.
    fn in_a_child(check: &str, nofile: Option<u64>, umask: Option<libc::mode_t>) -> bool {
        use std::os::unix::process::CommandExt;
        let module = module_path!()
            .split_once("::")
            .map_or(module_path!(), |(_, rest)| rest);
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", &format!("{module}::{check}"), "--ignored"])
            .env(CHILD_CHECK_ENV, check);
        // SAFETY: setrlimit and umask only, between fork and exec.
        unsafe {
            command.pre_exec(move || {
                if let Some(nofile) = nofile {
                    let limit = libc::rlimit {
                        rlim_cur: nofile,
                        rlim_max: nofile,
                    };
                    if libc::setrlimit(libc::RLIMIT_NOFILE, &limit) != 0 {
                        return Err(io::Error::last_os_error());
                    }
                }
                if let Some(mask) = umask {
                    libc::umask(mask);
                }
                Ok(())
            });
        }
        let output = command.output().expect("start the child check");
        output.status.success() && String::from_utf8_lossy(&output.stdout).contains("1 passed")
    }

    /// Whether this process is the child `in_a_child` started for `check`.
    fn is_child_for(check: &str) -> bool {
        std::env::var(CHILD_CHECK_ENV).is_ok_and(|value| value == check)
    }

    /// (b) Under a descriptor limit at or below the floor (RLIMIT_NOFILE
    /// 256), a reservation is still made, just below the limit and
    /// close-on-exec: Reserved, not Unreserved. And under a limit just above
    /// the floor (1001) with the floor's number taken, where the first move
    /// fails with EMFILE rather than EINVAL, likewise.
    #[test]
    fn a_low_descriptor_limit_still_reserves() {
        assert!(in_a_child(
            "child_reserves_under_its_limit",
            Some(256),
            None
        ));
        assert!(in_a_child(
            "child_reserves_under_its_limit",
            Some(1001),
            None
        ));
    }

    #[test]
    #[ignore = "run by a_low_descriptor_limit_still_reserves in a child with a low limit"]
    fn child_reserves_under_its_limit() {
        if !is_child_for("child_reserves_under_its_limit") {
            return;
        }
        let (_, limit) = nix::sys::resource::getrlimit(nix::sys::resource::Resource::RLIMIT_NOFILE)
            .expect("getrlimit");
        // Take the floor's number where the limit leaves it free.
        let _floor = (limit > RESERVATION_FD_FLOOR as u64).then(|| {
            let file = fs::File::open("/dev/null").unwrap();
            // SAFETY: duplicate onto the free floor number.
            let fd = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_DUPFD, RESERVATION_FD_FLOOR) };
            assert_eq!(fd, RESERVATION_FD_FLOOR);
            // SAFETY: `fd` is a new descriptor nothing else owns.
            unsafe { OwnedFd::from_raw_fd(fd) }
        });
        let reserved = reserve(&private_prefix(), 7)
            .expect("reserved, not an error")
            .expect("the name is free");
        assert!(
            (reserved.as_raw_fd() as u64) < limit,
            "{reserved:?} under {limit}"
        );
        assert!(close_on_exec(&reserved));
    }

    /// A lock file this run creates under a umask of 077 is still mode 0644,
    /// so other users' runs in root's shared directory can lock it.
    #[test]
    fn a_lock_file_is_0644_whatever_the_umask() {
        assert!(in_a_child("child_creates_a_lock_file", None, Some(0o077)));
    }

    #[test]
    #[ignore = "run by a_lock_file_is_0644_whatever_the_umask in a child with umask 077"]
    fn child_creates_a_lock_file() {
        use std::os::unix::fs::PermissionsExt;
        if !is_child_for("child_creates_a_lock_file") {
            return;
        }
        let (dir, fd) = private_lock_dir();
        let _held = lock(&fd, 9).expect("lock").expect("free");
        assert_eq!(
            fs::metadata(dir.path().join("9"))
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            0o644
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
