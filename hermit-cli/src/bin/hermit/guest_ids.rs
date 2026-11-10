/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! How a `--no-namespace` run numbers its guest (`--pid-base`).
//!
//! Without a PID namespace of its own, a guest would see the host's next free
//! process and thread IDs, different in every run. Where Hermit may choose
//! IDs (`CAP_SYS_ADMIN` or `CAP_CHECKPOINT_RESTORE` over its PID namespace:
//! root, or the root of an enclosing user namespace that owns it), the
//! guest's first process gets the base ID and Detcore gives each later task
//! the next one (`GuestIds::Chosen`).
//!
//! The ID space is cut into aligned windows of [`window_ids`] IDs. A run
//! takes the window or windows its IDs start in, checked free of host tasks,
//! and reserves them for its lifetime ([`Reservation`]), so that two Hermit
//! runs on one host do not start in the same IDs. IDs past those windows are
//! neither checked nor reserved (<https://github.com/rrnewton/hermit/issues/4870>).
//!
//! A run that asks for chosen IDs, with an explicit `--pid-base` or with
//! `--strict` (whose base, unless given, is half of `pid_max`, a fact of the
//! host's configuration rather than its state), is refused when it cannot
//! have them, naming the cause. Any other run first checks that it may
//! choose IDs at all, then takes the first free window it can reserve,
//! starting just behind the kernel's PID allocation cursor (so that the host
//! reaches it last) and going down, then from the top; it reports the base it
//! took. With none, or without the capability, it warns and the guest gets
//! host IDs.
//!
//! Chosen IDs also require the host to validate a legacy clone's original
//! exit signal before the conversion to clone3 can replace it. Admission
//! probes that behavior; a kernel version string does not establish it.

use std::collections::BTreeSet;

use anyhow::Error;
use detcore_model::pid::GuestIds;

use super::container::PolicyRefusal;

/// How many IDs a run's window holds: what Hermit checks free and reserves,
/// and what a guest's tasks stay within unless it creates more (they go on
/// past it, and an ID found taken then refuses or warns as Detcore's
/// `inject_with_chosen_id` says). 1024 where `pid_max` is at most 65536: an
/// Android device's 32768 has 31 windows above [`RESERVED_PIDS`]. 256 where it
/// is larger: a long-running host's tasks are spread over its whole ID space,
/// and one with 37,000 threads and `pid_max` 4,194,304 has a free window of
/// 256 about one time in ten, and of 1024 almost never.
pub(super) fn window_ids(pid_max: i32) -> i32 {
    if pid_max <= 65536 { 1024 } else { 256 }
}

/// Linux's `RESERVED_PIDS`: below this the kernel does not wrap, and no
/// default window is sought.
const RESERVED_PIDS: i32 = 300;

/// How the guest of a run is numbered, and the warning to print when it
/// takes host IDs: from `--no-namespace`, `--pid-base` and `--strict`.
/// `Err` is a usage error, or the [`PolicyRefusal`] of a run that asked for
/// chosen IDs and cannot have them. The windows the run takes stay reserved
/// until this process exits.
pub(super) fn resolve(
    no_namespace: bool,
    required: Option<Required<'_>>,
    strict: bool,
) -> Result<(GuestIds, Option<String>), Error> {
    resolve_with(no_namespace, required, strict, &Host)
}

/// Whether this process may choose its guests' process IDs at all, as the
/// default base search first checks: `Err` names why not. `hermit analyze`
/// and `hermit bisect` ask before they accept `--no-namespace`, whose trials
/// replay one another's records and so need IDs that do not depend on the
/// host.
pub(super) fn may_choose_ids() -> Result<(), String> {
    let host = Host;
    let pid_max = host.pid_max()?;
    let cursor = host.cursor().unwrap_or(pid_max / 2).clamp(1, pid_max - 1);
    host.may_choose(cursor)
}

/// A base the run must have, and why.
#[derive(Clone, Copy, Debug)]
pub(super) enum Required<'a> {
    /// The run gave `--pid-base`.
    PidBase(i32),
    /// The run replays a record (`source`, an option and a path) whose
    /// guest IDs were chosen from `base`.
    Replay { source: &'a str, base: i32 },
}

impl Required<'_> {
    fn base(&self) -> i32 {
        match *self {
            Required::PidBase(base) | Required::Replay { base, .. } => base,
        }
    }
}

/// The host facts [`resolve`] reads, replaced in tests.
trait HostIds {
    /// `/proc/sys/kernel/pid_max`.
    fn pid_max(&self) -> Result<i32, String>;
    /// The IDs of every host task `/proc` shows: each process's threads.
    fn taken(&self) -> BTreeSet<i32>;
    /// What holds `id` on the host.
    fn holder(&self, id: i32) -> String;
    /// Whether legacy clone validates its original exit signal before the
    /// chosen-ID conversion may replace that signal.
    fn legacy_clone_signal_validation(&self) -> Result<(), String>;
    /// Whether this process can create a child with process ID `id`.
    fn can_choose(&self, id: i32) -> Result<(), String>;
    /// Whether this process may choose IDs at all: [`Self::can_choose`],
    /// where `id` being taken (`EEXIST`) also shows the permission, which
    /// Linux checks first (`alloc_pid`).
    fn may_choose(&self, id: i32) -> Result<(), String>;
    /// The ID the kernel would give a new task next, if known.
    fn cursor(&self) -> Option<i32>;
    /// Reserve window number `index` of `size` IDs for this process's
    /// lifetime; `Err` when another holds it.
    fn reserve(&self, index: i32, size: i32) -> Result<(), String>;
}

struct Host;

impl HostIds for Host {
    fn pid_max(&self) -> Result<i32, String> {
        let text = std::fs::read_to_string("/proc/sys/kernel/pid_max")
            .map_err(|error| format!("cannot read /proc/sys/kernel/pid_max: {error}"))?;
        text.trim()
            .parse()
            .map_err(|error| format!("cannot parse /proc/sys/kernel/pid_max {text:?}: {error}"))
    }

    fn taken(&self) -> BTreeSet<i32> {
        let numbers = |dir: &str| {
            std::fs::read_dir(dir)
                .into_iter()
                .flatten()
                .flatten()
                .filter_map(|entry| entry.file_name().to_str()?.parse::<i32>().ok())
                .collect::<Vec<_>>()
        };
        numbers("/proc")
            .into_iter()
            .flat_map(|pid| {
                // A process that exits meanwhile still held its own ID.
                let mut tasks = numbers(&format!("/proc/{pid}/task"));
                tasks.push(pid);
                tasks
            })
            .collect()
    }

    fn holder(&self, id: i32) -> String {
        detcore::chosen_id_holder(id)
    }

    fn legacy_clone_signal_validation(&self) -> Result<(), String> {
        check_legacy_clone_signal_validation(probe_legacy_clone_errno)
    }

    fn can_choose(&self, id: i32) -> Result<(), String> {
        probe_chosen_pid(id).map_err(|(_, cause)| cause)
    }

    fn may_choose(&self, id: i32) -> Result<(), String> {
        match probe_chosen_pid(id) {
            Err((Some(libc::EEXIST), _)) => Ok(()),
            result => result.map_err(|(_, cause)| cause),
        }
    }

    fn cursor(&self) -> Option<i32> {
        probe_cursor()
    }

    fn reserve(&self, index: i32, size: i32) -> Result<(), String> {
        Reservation::take(index, size)
    }
}

/// A window of IDs this process holds for its lifetime: an abstract Unix
/// socket named after it and the PID namespace whose IDs it holds
/// (`hermit-guest-pid-window:<namespace inode>:<size>:<index>`), bound
/// with close-on-exec and never closed, so the kernel releases it when Hermit
/// exits. A second run that binds the same name gets `EADDRINUSE`.
///
/// Abstract socket names belong to a network namespace: the reservation
/// protects runs that share one, which every `--no-namespace` run on a host
/// does unless it was started inside another (on Android, some services and
/// apps run in their own). Runs in different network namespaces can still
/// take the same window; the later one's first process then cannot have its
/// ID, and the run ends with Hermit's policy-refusal status, naming it.
struct Reservation;

impl Reservation {
    fn take(index: i32, size: i32) -> Result<(), String> {
        use std::os::linux::net::SocketAddrExt;
        // Runs in different PID namespaces have separate IDs: only those in
        // this one contend. Without PID namespaces there is only one.
        let namespace = std::fs::read_link("/proc/self/ns/pid")
            .ok()
            .and_then(|link| {
                let link = link.to_string_lossy().into_owned();
                link.strip_prefix("pid:[")?
                    .strip_suffix(']')
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| "0".to_owned());
        let name = format!("hermit-guest-pid-window:{namespace}:{size}:{index}");
        let address = std::os::unix::net::SocketAddr::from_abstract_name(name.as_bytes())
            .map_err(|error| format!("cannot name the reservation {name}: {error}"))?;
        let socket = std::os::unix::net::UnixDatagram::bind_addr(&address).map_err(|error| {
            if error.raw_os_error() == Some(libc::EADDRINUSE) {
                format!(
                    "another Hermit run holds the {size} IDs from {} (reservation @{name})",
                    index * size
                )
            } else {
                format!("cannot reserve the IDs from {}: {error}", index * size)
            }
        })?;
        // Held until this process exits.
        std::mem::forget(socket);
        Ok(())
    }
}

fn resolve_with(
    no_namespace: bool,
    required: Option<Required<'_>>,
    strict: bool,
    host: &dyn HostIds,
) -> Result<(GuestIds, Option<String>), Error> {
    if !no_namespace {
        return Ok((GuestIds::Namespace, None));
    }
    let pid_max = host.pid_max().map_err(Error::msg)?;
    // A base the kernel can never give: a usage error when given, a refusal
    // when a record asks for it (it was recorded on a host with a larger
    // pid_max).
    match required {
        Some(Required::PidBase(base)) if base < 1 || base >= pid_max => anyhow::bail!(
            "--pid-base={base} is not a process ID this host can give: it must be at least 1 \
             and below pid_max, {pid_max}"
        ),
        Some(Required::Replay { source, base }) if base < 1 || base >= pid_max => {
            return Err(Error::new(PolicyRefusal).context(format!(
                "{source} was recorded with guest process IDs chosen from {base}, which this \
                 host cannot give (its pid_max is {pid_max}), so the replay cannot follow it. \
                 Record the run again on this host"
            )));
        }
        _ => {}
    }
    // The conversion in Detcore preserves early signal validation only on
    // hosts that demonstrate it. Refuse/fall back before drawing the cursor,
    // reserving windows or probing a chosen PID; Namespace returned above.
    if let Err(cause) = host.legacy_clone_signal_validation() {
        if required.is_none() && !strict {
            return Ok((
                GuestIds::Host,
                Some(host_ids_warning(&format!(
                    "{cause}. Use a host that passes this validation probe, or omit \
                     --no-namespace to use a PID namespace"
                ))),
            ));
        }
        let asked = match required {
            Some(Required::PidBase(_)) => "--pid-base",
            Some(Required::Replay { source, .. }) => source,
            None => "--strict",
        };
        return Err(Error::new(PolicyRefusal).context(format!(
            "{asked} with --no-namespace requires chosen guest process IDs, but Hermit cannot \
             safely choose them: {cause}. Use a host that passes this validation probe. For a \
             new recording or run, omit --no-namespace and --pid-base to use a PID namespace"
        )));
    }
    let pid_base = required.map(|required| required.base());
    let size = window_ids(pid_max);
    let cursor = host.cursor().unwrap_or(pid_max / 2).clamp(1, pid_max - 1);
    // The cheap check first: a run that may not choose IDs warns without
    // reading every host task.
    if pid_base.is_none()
        && !strict
        && let Err(cause) = host.may_choose(cursor)
    {
        return Ok((GuestIds::Host, Some(host_ids_warning(&cause))));
    }
    let taken = host.taken();
    let window = |base: i32| base..base.saturating_add(size).min(pid_max);
    let holder_in = |base: i32| taken.range(window(base)).next().copied();
    if pid_base.is_some() || strict {
        let base = pid_base.unwrap_or(pid_max / 2);
        // Every window the run's first IDs fall in.
        let windows = base / size..=(window(base).end - 1) / size;
        let cause = match holder_in(base) {
            Some(id) => Some(format!(
                "the {} IDs from {base} must be free, but {} holds one of them",
                window(base).len(),
                host.holder(id)
            )),
            None => windows
                .into_iter()
                .find_map(|index| host.reserve(index, size).err())
                .or_else(|| host.can_choose(base).err()),
        };
        return match cause {
            None => Ok((
                GuestIds::Chosen {
                    base,
                    refuse_host_ids: true,
                },
                None,
            )),
            Some(cause) => Err(Error::new(PolicyRefusal).context(match required {
                Some(Required::Replay { source, .. }) => format!(
                    "{source} was recorded with guest process IDs chosen from {base}, and this \
                     replay needs exactly those IDs but cannot have them: {cause}. Stop what \
                     holds them, run as a user that may choose process IDs \
                     (CAP_CHECKPOINT_RESTORE or CAP_SYS_ADMIN), or record the run again"
                ),
                _ => {
                    let asked = if pid_base.is_some() {
                        "--pid-base"
                    } else {
                        "--strict"
                    };
                    format!(
                        "{asked} with --no-namespace needs the guest's process IDs chosen from \
                         {base}, and Hermit cannot choose them: {cause}. Pass a --pid-base whose \
                         IDs no host task or other run holds, or run as a user that may choose \
                         process IDs (CAP_CHECKPOINT_RESTORE or CAP_SYS_ADMIN)"
                    )
                }
            })),
        };
    }
    // Windows wholly inside RESERVED_PIDS..pid_max: just behind the cursor's
    // and down, then from the top down to the cursor's own, last.
    let first = (RESERVED_PIDS + size - 1) / size;
    let last = pid_max / size - 1;
    // A pid_max under two windows (Linux allows down to 301) leaves no whole
    // window past the reserved IDs: no base is chosen, as when none is free.
    let free = if first > last {
        None
    } else {
        let own = (cursor / size).clamp(first, last);
        let order = (first..own).rev().chain((own..=last).rev());
        order
            .filter(|index| holder_in(index * size).is_none())
            .find(|index| host.reserve(*index, size).is_ok())
    };
    match free {
        Some(index) => Ok((
            GuestIds::Chosen {
                base: index * size,
                refuse_host_ids: false,
            },
            None,
        )),
        None => Ok((
            GuestIds::Host,
            Some(host_ids_warning(&format!(
                "no window of {size} IDs below pid_max, {pid_max}, is free of host tasks and of \
                 other Hermit runs"
            ))),
        )),
    }
}

/// The warning of a run whose guest gets host IDs, for `cause`.
fn host_ids_warning(cause: &str) -> String {
    format!(
        "WARNING: --no-namespace: the guest gets host process IDs, which differ from run to run, \
         because Hermit cannot choose them: {cause}. Pass --pid-base to refuse such a run instead"
    )
}

/// Admission needs both observations: an invalid signal's EINVAL alone
/// could instead come from an unrelated validation or policy failure.
fn check_legacy_clone_signal_validation(
    mut probe: impl FnMut(i32) -> Result<i32, String>,
) -> Result<(), String> {
    let valid = probe(libc::SIGCHLD);
    let invalid = probe(65);
    if valid == Ok(libc::EFAULT) && invalid == Ok(libc::EINVAL) {
        return Ok(());
    }
    let describe = |result: Result<i32, String>| match result {
        Ok(libc::EFAULT) => format!("EFAULT ({})", libc::EFAULT),
        Ok(libc::EINVAL) => format!("EINVAL ({})", libc::EINVAL),
        Ok(errno) => format!(
            "errno {errno} ({})",
            std::io::Error::from_raw_os_error(errno)
        ),
        Err(cause) => format!("probe failed: {cause}"),
    };
    Err(format!(
        "legacy clone exit-signal validation is unsupported or unproven: SIGCHLD probe returned \
         {}, signal 65 probe returned {}; expected EFAULT ({}) then EINVAL ({})",
        describe(valid),
        describe(invalid),
        libc::EFAULT,
        libc::EINVAL,
    ))
}

/// Use CLONE_PIDFD with an output address outside the Linux user address
/// range, so copy_process fails before kernel_clone can wake a new task.
/// Unlike a null pointer, usize::MAX does not rely on the zero page being
/// unmapped. No CLONE_PARENT means neither init restrictions nor reparenting
/// can confuse the probe.
///
/// Linux v6.16 copy_process writes the pidfd at kernel/fork.c:2210:
/// <https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/kernel/fork.c?id=038d61fd642278bab63ee8ef722c50d10ab01e8f#n2210>.
/// Failure releases the pidfd/PID at line 2430 and returns an error, before
/// kernel_clone could wake the task at line 2630. Its kernel_clone lacks
/// original-signal validation, so both probes reach EFAULT. Linux v7.1.3
/// kernel_clone checks the signal first, yielding EINVAL for 65:
/// <https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/kernel/fork.c?id=199c9959d3a9b53f346c221757fc7ac507fbac50#n2702>.
/// This may allocate/release an internal host PID before EFAULT; it allocates
/// no guest-chosen ID. Unexpected success is reaped and never admits Chosen.
fn probe_legacy_clone_errno(signal: i32) -> Result<i32, String> {
    // SAFETY: a fork-like clone without CLONE_VM. The invalid output pointer
    // should prevent creation; a defensive child path only calls _exit.
    let child = unsafe {
        libc::syscall(
            libc::SYS_clone,
            (libc::CLONE_PIDFD | signal) as libc::c_ulong,
            0usize,
            usize::MAX,
            0usize,
            0usize,
        )
    };
    if child == 0 {
        unsafe { libc::_exit(0) }
    }
    if child < 0 {
        return std::io::Error::last_os_error()
            .raw_os_error()
            .ok_or_else(|| "legacy clone failed without an OS error number".to_owned());
    }
    let mut status = 0;
    // SAFETY: wait only for this unexpected child, including a clone child
    // whose exit signal was not SIGCHLD. Its only userspace action is _exit.
    while unsafe { libc::waitpid(child as libc::pid_t, &mut status, libc::__WALL) } < 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EINTR) {
            return Err(format!(
                "legacy clone unexpectedly created child {child}; could not reap it: {error}"
            ));
        }
    }
    Err(format!(
        "legacy clone unexpectedly created child {child}; reaped with wait status {status}"
    ))
}

/// The ID the kernel gives a new task next: that of a child made with a
/// plain fork-like `clone` that exits at once and is reaped here.
fn probe_cursor() -> Option<i32> {
    // SAFETY: a fork-like clone without CLONE_VM. The child only calls
    // _exit, which is async-signal-safe.
    let child = unsafe { libc::syscall(libc::SYS_clone, libc::SIGCHLD, 0, 0, 0, 0) };
    if child == 0 {
        unsafe { libc::_exit(0) }
    }
    if child < 0 {
        return None;
    }
    let mut status = 0;
    // SAFETY: waits for the child created above.
    while unsafe { libc::waitpid(child as libc::pid_t, &mut status, libc::__WALL) } < 0 {
        if std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
            break;
        }
    }
    i32::try_from(child).ok().map(|child| child + 1)
}

/// Whether this process can create a child with process ID `id` in its own
/// PID namespace: a child made with `clone3`'s `set_tid` that exits at once.
/// The ID is free again once the child is reaped, which this does before it
/// returns.
fn probe_chosen_pid(id: i32) -> Result<(), (Option<i32>, String)> {
    // struct clone_args through set_tid_size (CLONE_ARGS_SIZE_VER1).
    let set_tid: libc::pid_t = id;
    let mut args = [0u64; 10];
    args[4] = libc::SIGCHLD as u64; // exit_signal
    args[8] = std::ptr::from_ref(&set_tid) as u64; // set_tid
    args[9] = 1; // set_tid_size
    // SAFETY: a fork-like clone3 without CLONE_VM. The child only calls
    // _exit, which is async-signal-safe.
    let child = unsafe {
        libc::syscall(
            libc::SYS_clone3,
            args.as_ptr(),
            std::mem::size_of_val(&args),
        )
    };
    if child == 0 {
        unsafe { libc::_exit(0) }
    }
    if child < 0 {
        let error = std::io::Error::last_os_error();
        let errno = error.raw_os_error();
        return Err((
            errno,
            match errno {
                Some(libc::EPERM) => format!(
                    "this user may not choose process IDs (clone3 set_tid: {error}): that takes \
                     CAP_CHECKPOINT_RESTORE or CAP_SYS_ADMIN over the host's PID namespace"
                ),
                Some(libc::ENOSYS) => format!(
                    "clone3 is unavailable (Linux before 5.3, or a seccomp filter refuses it): \
                     {error}"
                ),
                Some(libc::E2BIG) => {
                    format!("this kernel's clone3 has no set_tid (Linux 5.3 and 5.4): {error}")
                }
                _ => format!("clone3 with set_tid {id} failed: {error}"),
            },
        ));
    }
    let mut status = 0;
    // SAFETY: waits for the child created above.
    while unsafe { libc::waitpid(child as libc::pid_t, &mut status, libc::__WALL) } < 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EINTR) {
            return Err((
                None,
                format!("cannot reap the probe child {child}: {error}"),
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::cell::RefCell;

    use super::*;

    struct FakeHost {
        pid_max: i32,
        taken: BTreeSet<i32>,
        can_choose: Result<(), &'static str>,
        legacy_validation: Result<(), &'static str>,
        validation_checks: Cell<usize>,
        probed: RefCell<Vec<i32>>,
        cursor: Option<i32>,
        cursor_reads: Cell<usize>,
        /// Windows another run holds, and those this one reserved.
        held: RefCell<BTreeSet<i32>>,
    }

    impl FakeHost {
        fn new() -> Self {
            FakeHost {
                pid_max: 32768,
                taken: BTreeSet::new(),
                can_choose: Ok(()),
                legacy_validation: Ok(()),
                validation_checks: Cell::new(0),
                probed: RefCell::new(Vec::new()),
                cursor: Some(20000),
                cursor_reads: Cell::new(0),
                held: RefCell::new(BTreeSet::new()),
            }
        }
    }

    impl HostIds for FakeHost {
        fn pid_max(&self) -> Result<i32, String> {
            Ok(self.pid_max)
        }
        fn taken(&self) -> BTreeSet<i32> {
            self.taken.clone()
        }
        fn holder(&self, id: i32) -> String {
            format!("process {id} (sshd)")
        }
        fn legacy_clone_signal_validation(&self) -> Result<(), String> {
            self.validation_checks.set(self.validation_checks.get() + 1);
            self.legacy_validation.map_err(str::to_owned)
        }
        fn can_choose(&self, id: i32) -> Result<(), String> {
            self.probed.borrow_mut().push(id);
            self.can_choose.map_err(str::to_owned)
        }
        fn may_choose(&self, id: i32) -> Result<(), String> {
            self.can_choose(id)
        }
        fn cursor(&self) -> Option<i32> {
            self.cursor_reads.set(self.cursor_reads.get() + 1);
            self.cursor
        }
        fn reserve(&self, index: i32, size: i32) -> Result<(), String> {
            if self.held.borrow_mut().insert(index) {
                Ok(())
            } else {
                Err(format!(
                    "another Hermit run holds the {size} IDs from {}",
                    index * size
                ))
            }
        }
    }

    fn chosen(base: i32, refuse_host_ids: bool) -> GuestIds {
        GuestIds::Chosen {
            base,
            refuse_host_ids,
        }
    }

    fn refusal(result: Result<(GuestIds, Option<String>), Error>) -> String {
        let error = result.unwrap_err();
        assert!(error.downcast_ref::<PolicyRefusal>().is_some(), "{error:#}");
        format!("{error:#}")
    }

    /// Source-modelled Linux v6.16 reaches the invalid pidfd output write
    /// for both signals; it does not prove the early validation the chosen
    /// conversion requires. Exercise the actual production resolver.
    #[test]
    fn chosen_ids_require_legacy_clone_signal_validation() {
        let cause = "legacy clone exit-signal validation is unsupported: SIGCHLD probe returned \
                     EFAULT (14), signal 65 probe returned EFAULT (14)";
        for (required, strict, named) in [
            (Some(Required::PidBase(1000)), false, "--pid-base"),
            (None, true, "--strict"),
            (
                Some(Required::Replay {
                    source: "--replay-schedule-from chosen.json",
                    base: 1000,
                }),
                false,
                "--replay-schedule-from chosen.json",
            ),
        ] {
            let mut host = FakeHost::new();
            host.legacy_validation = Err(cause);
            assert_eq!(host.legacy_clone_signal_validation(), Err(cause.to_owned()));
            host.validation_checks.set(0);
            let error = refusal(resolve_with(true, required, strict, &host));
            assert!(error.contains(cause), "{error}");
            assert!(error.contains(named), "{error}");
            assert!(
                error.contains("Use a host that passes this validation probe"),
                "{error}"
            );
            assert!(error.contains("use a PID namespace"), "{error}");
            assert_eq!(host.validation_checks.get(), 1);
            assert_eq!(host.cursor_reads.get(), 0);
            assert!(host.probed.borrow().is_empty());
            assert!(host.held.borrow().is_empty());
        }
    }

    #[test]
    fn a_namespace_numbers_the_guest_itself() {
        let mut host = FakeHost::new();
        host.legacy_validation = Err("legacy validation unavailable");
        assert_eq!(
            resolve_with(false, None, true, &host).unwrap(),
            (GuestIds::Namespace, None)
        );
        assert!(host.probed.borrow().is_empty());
        assert_eq!(host.validation_checks.get(), 0);
        assert_eq!(host.cursor_reads.get(), 0);
        assert!(host.held.borrow().is_empty());
    }

    #[test]
    fn unproven_clone_validation_uses_host_ids_only_with_a_warning() {
        for cause in [
            "legacy clone exit-signal validation is unsupported: both probes returned EFAULT",
            "legacy clone validation probe failed: permission denied",
        ] {
            let mut host = FakeHost::new();
            host.legacy_validation = Err(cause);
            let (ids, warning) = resolve_with(true, None, false, &host).unwrap();
            assert_eq!(ids, GuestIds::Host);
            let warning = warning.expect("Host fallback must be explicit");
            assert!(warning.contains(cause), "{warning}");
            assert!(warning.contains("use a PID namespace"), "{warning}");
            assert!(warning.contains("Pass --pid-base to refuse"), "{warning}");
            assert_eq!(host.validation_checks.get(), 1);
            assert_eq!(host.cursor_reads.get(), 0);
            assert!(host.probed.borrow().is_empty());
            assert!(host.held.borrow().is_empty());
        }
    }

    #[test]
    fn legacy_clone_validation_requires_the_exact_probe_pair() {
        let mut signals = Vec::new();
        assert!(
            check_legacy_clone_signal_validation(|signal| {
                signals.push(signal);
                Ok(if signal == libc::SIGCHLD {
                    libc::EFAULT
                } else {
                    libc::EINVAL
                })
            })
            .is_ok()
        );
        assert_eq!(signals, [libc::SIGCHLD, 65]);

        // The older source's pair, unrelated EINVAL, and failed probes
        // cannot establish early original-signal validation.
        for (pair, expected) in [
            (
                [Ok(libc::EFAULT), Ok(libc::EFAULT)],
                ["EFAULT (14)", "EFAULT (14)"],
            ),
            (
                [Ok(libc::EINVAL), Ok(libc::EINVAL)],
                ["EINVAL (22)", "EINVAL (22)"],
            ),
            (
                [Ok(libc::EPERM), Ok(libc::EINVAL)],
                [
                    "errno 1 (Operation not permitted (os error 1))",
                    "EINVAL (22)",
                ],
            ),
            (
                [Ok(libc::EFAULT), Ok(libc::ENOSYS)],
                [
                    "EFAULT (14)",
                    "errno 38 (Function not implemented (os error 38))",
                ],
            ),
            (
                [
                    Err("unexpected child was reaped".to_owned()),
                    Ok(libc::EINVAL),
                ],
                ["probe failed: unexpected child was reaped", "EINVAL (22)"],
            ),
            (
                [Ok(libc::EFAULT), Err("probe failed".to_owned())],
                ["EFAULT (14)", "probe failed: probe failed"],
            ),
        ] {
            let mut results = pair.into_iter();
            let mut signals = Vec::new();
            let error = check_legacy_clone_signal_validation(|signal| {
                signals.push(signal);
                results.next().unwrap()
            })
            .expect_err("only the measured validation pair admits Chosen");
            assert_eq!(signals, [libc::SIGCHLD, 65]);
            assert!(error.contains("unsupported or unproven"), "{error}");
            for (label, outcome) in ["SIGCHLD", "signal 65"].into_iter().zip(expected) {
                assert!(
                    error.contains(&format!("{label} probe returned {outcome}")),
                    "{error}"
                );
            }
        }
    }

    /// A pid_max under two windows (Linux allows down to 301) leaves no whole
    /// window past the reserved IDs: the default run warns and takes host
    /// IDs instead of panicking; from two windows on, the second is chosen.
    #[test]
    fn a_pid_max_under_two_windows_leaves_no_default_window() {
        for pid_max in [301, 1024, 2047] {
            let mut host = FakeHost::new();
            host.pid_max = pid_max;
            host.cursor = Some(pid_max / 2);
            let (ids, warning) = resolve_with(true, None, false, &host).unwrap();
            assert_eq!(ids, GuestIds::Host, "pid_max {pid_max}");
            assert!(
                warning.is_some_and(|warning| warning.contains("no window of 1024 IDs")),
                "pid_max {pid_max}"
            );
        }
        let mut host = FakeHost::new();
        host.pid_max = 2048;
        host.cursor = Some(1500);
        assert_eq!(
            resolve_with(true, None, false, &host).unwrap(),
            (chosen(1024, false), None)
        );
    }

    /// Windows are 1024 IDs where pid_max is at most 65536, else 256.
    #[test]
    fn the_window_size_follows_pid_max() {
        assert_eq!(window_ids(32768), 1024);
        assert_eq!(window_ids(65536), 1024);
        assert_eq!(window_ids(4194304), 256);
    }

    /// Without a base or --strict, a run reserves the free window just behind
    /// the cursor's, then the next one down, then from the top; a second run
    /// on the host never gets a window the first holds.
    #[test]
    fn the_default_window_is_behind_the_cursor_and_reserved() {
        let mut host = FakeHost::new();
        // The cursor 20000 is in window 19 (19456..20480).
        assert_eq!(
            resolve_with(true, None, false, &host).unwrap(),
            (chosen(18 * 1024, false), None)
        );
        // Held now: the next run on the host takes the next one down.
        assert_eq!(
            resolve_with(true, None, false, &host).unwrap().0,
            chosen(17 * 1024, false)
        );
        // A host task in a window skips it.
        host.taken.insert(16 * 1024 + 1023);
        assert_eq!(
            resolve_with(true, None, false, &host).unwrap().0,
            chosen(15 * 1024, false)
        );
        // Below the cursor all taken or held: from the top.
        host.taken.extend((300..15 * 1024).step_by(500));
        assert_eq!(
            resolve_with(true, None, false, &host).unwrap().0,
            chosen(31 * 1024, false)
        );
        // The capability is probed at the cursor, before the scan.
        assert_eq!(host.probed.borrow().last(), Some(&20000));
    }

    /// An explicit base, or --strict's fixed one, is a requirement: free of
    /// host tasks, reserved, and choosable.
    #[test]
    fn an_explicit_base_or_strict_refuses_host_ids() {
        let host = FakeHost::new();
        assert_eq!(
            resolve_with(true, Some(Required::PidBase(20000)), false, &host)
                .unwrap()
                .0,
            chosen(20000, true)
        );
        // 20000 starts in window 19 and its 1024 IDs end in window 20.
        assert!(host.held.borrow().is_superset(&BTreeSet::from([19, 20])));
        assert_eq!(
            resolve_with(true, None, true, &host).unwrap().0,
            chosen(16384, true)
        );
        // Another run holding the window refuses the same base.
        let error = refusal(resolve_with(true, None, true, &host));
        assert!(
            error.contains("--strict") && error.contains("another Hermit run holds"),
            "{error}"
        );
        let mut host = FakeHost::new();
        host.taken.insert(16384 + 1023);
        let error = refusal(resolve_with(true, None, true, &host));
        assert!(
            error.contains("--strict") && error.contains("17407 (sshd)"),
            "{error}"
        );
        // One past the window is not checked.
        assert_eq!(
            resolve_with(true, Some(Required::PidBase(16384 - 1024)), false, &host)
                .unwrap()
                .0,
            chosen(15360, true)
        );
        let error = refusal(resolve_with(
            true,
            Some(Required::PidBase(16500)),
            false,
            &host,
        ));
        assert!(error.contains("--pid-base"), "{error}");
    }

    /// A replay's base is a requirement like `--pid-base`, but its refusals
    /// name the record, and a base this host cannot give is a refusal, not a
    /// usage error.
    #[test]
    fn a_replayed_base_is_required_and_named_by_its_record() {
        let source = "--replay-preemptions-from rec.json";
        let host = FakeHost::new();
        let replay = |base| Some(Required::Replay { source, base });
        assert_eq!(
            resolve_with(true, replay(20000), false, &host).unwrap().0,
            chosen(20000, true)
        );
        // Now held by that run: a second replay of the record is refused.
        let error = refusal(resolve_with(true, replay(20000), false, &host));
        assert!(
            error.contains(source)
                && error.contains("needs exactly those IDs")
                && !error.contains("--pid-base"),
            "{error}"
        );
        let error = refusal(resolve_with(true, replay(2097152), false, &host));
        assert!(error.contains("its pid_max is 32768"), "{error}");
    }

    #[test]
    fn a_base_the_kernel_cannot_give_is_a_usage_error() {
        let host = FakeHost::new();
        for base in [0, -5, 32768, 40000] {
            let error =
                resolve_with(true, Some(Required::PidBase(base)), false, &host).unwrap_err();
            assert!(error.downcast_ref::<PolicyRefusal>().is_none());
            assert!(format!("{error}").contains("pid_max, 32768"), "{error}");
        }
        // The window is cut at pid_max.
        assert!(resolve_with(true, Some(Required::PidBase(32767)), false, &host).is_ok());
    }

    /// Without the capability, a run that asked is refused and any other
    /// gets host IDs with a warning naming the cause; so does one that finds
    /// no window.
    #[test]
    fn without_the_capability_or_a_window() {
        let mut host = FakeHost::new();
        host.can_choose = Err("this user may not choose process IDs");
        let error = refusal(resolve_with(
            true,
            Some(Required::PidBase(1000)),
            false,
            &host,
        ));
        assert!(error.contains("may not choose"), "{error}");
        let (ids, warning) = resolve_with(true, None, false, &host).unwrap();
        assert_eq!(ids, GuestIds::Host);
        assert!(warning.unwrap().contains("may not choose"));
        let mut host = FakeHost::new();
        host.taken.extend((0..32768).step_by(1000));
        let (ids, warning) = resolve_with(true, None, false, &host).unwrap();
        assert_eq!(ids, GuestIds::Host);
        assert!(warning.unwrap().contains("no window of 1024 IDs"));
    }
}
