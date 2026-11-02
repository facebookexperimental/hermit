/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! SaBRe plugin that executes Hermit's Detcore tool inside each guest process.

// SHARED FILE: detcore-liteinst/src/lib.rs compiles glibc_compat.rs too
// (https://github.com/rrnewton/hermit/issues/3967); an edit changes both guest
// preloads.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
mod glibc_compat;

use std::ffi::CString;
use std::ffi::OsStr;
use std::ffi::OsString;
use std::io;
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

/// Private env var carrying the coordinator's configuration and clock RPC fingerprint.
pub use detcore::CONFIG_FINGERPRINT_ENV;
use detcore::Detcore;
use detcore::config_wire_fingerprint;
use detcore::detlog::ForwardPolicy;
use reverie::Signal;
use reverie_memory::LocalMemory;
use reverie_memory::MemoryAccess;
use reverie_sabre as sabre;
use reverie_sabre::RemoteReverieAdapter;
use reverie_syscalls::Errno;
use reverie_syscalls::Syscall;
use reverie_syscalls::SyscallArgs;
use reverie_syscalls::Sysno;

/// Environment variable containing the coordinator's Unix-domain socket path.
// TODO-HUMAN-REVIEW(PR-745): Review the private SaBRe exec environment contract.
pub const RPC_SOCKET_ENV: &str = "REVERIE_SABRE_HERMIT_RPC_SOCKET";

/// Private opt-in for forwarding injected-process Detcore INFO events.
pub const DETLOG_FORWARD_ENV: &str = "REVERIE_SABRE_HERMIT_FORWARD_DETLOG";

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-771): Review fork-inherited SaBRe coordinator discovery.
static RPC_SOCKET: OnceLock<PathBuf> = OnceLock::new();

fn coordinator_socket() -> Option<PathBuf> {
    if let Some(socket) = RPC_SOCKET.get() {
        return Some(socket.clone());
    }

    // SAFETY: Plugin construction runs before SaBRe starts guest callbacks.
    let requested = unsafe { sabre::take_private_env(RPC_SOCKET_ENV) };
    remember_coordinator_socket(&RPC_SOCKET, requested.as_deref())
}

// The coordinator sends its per-target INFO answer (detcore::detlog::ForwardPolicy), so a
// target-scoped RUST_LOG keeps the same records here as in-process. A value this plugin
// cannot read is a coordinator/plugin mismatch: say so rather than drop records silently.
fn detlog_forward_policy(requested: Option<&OsStr>) -> Result<Option<ForwardPolicy>, String> {
    let Some(requested) = requested else {
        return Ok(None);
    };
    let requested = requested
        .to_str()
        .ok_or_else(|| format!("{DETLOG_FORWARD_ENV} is not UTF-8"))?;
    ForwardPolicy::decode(requested).map(Some)
}

// Says why records are not forwarded, on the standard error the forwarder would use,
// with one raw write as detcore::detlog::forward_to_stderr does.
fn report_unforwarded_detlogs(error: &str) {
    let line = format!("hermit SaBRe plugin: no DETLOG forwarding: {error}\n");
    // SAFETY: `line` is a live, initialized byte slice of the given length.
    let _ = unsafe {
        libc::write(
            libc::STDERR_FILENO,
            line.as_ptr().cast::<libc::c_void>(),
            line.len(),
        )
    };
}

fn init_detlog_forwarder() {
    // SAFETY: Plugin construction runs before SaBRe starts guest callbacks.
    let requested = unsafe { sabre::take_private_env(DETLOG_FORWARD_ENV) };
    let policy = match detlog_forward_policy(requested.as_deref()) {
        Ok(Some(policy)) => policy,
        Ok(None) => return,
        Err(error) => {
            report_unforwarded_detlogs(&error);
            return;
        }
    };

    // With the socket `hermit run --verify` passes, each record goes to it from
    // the descriptor reverie-sabre protects from the guest, tagged with the
    // sending thread, and the coordinator writes it into the log at that
    // thread's scheduler turn, as for in-guest LiteInst. Without one, records
    // go to stderr, which reverie-sabre keeps open and verification captures
    // separately.
    // A socket that was requested but could not be adopted (closed or replaced
    // before adoption, or given up) never falls back to stderr, which the
    // guest may have pointed at a file of its own: its records are counted and
    // dropped, and the coordinator refuses the run (detlog::check_forwarded_count).
    let forwarder: detcore::detlog::DetlogForwarder =
        if sabre::tool_output_fd().is_some() || sabre::tool_output_requested() {
            // If reverie-sabre ever has to give the socket up (a guest dup onto
            // its number with no free descriptor to move it to), the coordinator
            // reads this and refuses the run's records as incomplete.
            sabre::set_retirement_message(detcore::detlog::FORWARDING_RETIRED_NOTICE);
            forward_detlog_to_socket
        } else {
            detcore::detlog::forward_to_stderr
        };
    let _ = detcore::detlog::set_forwarder(forwarder, policy);
}

/// Sends one Detcore record on the coordinator's DETLOG socket.
fn forward_detlog_to_socket(
    target: &str,
    record_suffix: &str,
    index: u64,
    message: std::fmt::Arguments<'_>,
) {
    sabre::with_tool_output(|socket| {
        detcore::detlog::send_forwarded_record(socket, target, record_suffix, index, message)
    });
}

fn remember_coordinator_socket(
    slot: &OnceLock<PathBuf>,
    requested: Option<&OsStr>,
) -> Option<PathBuf> {
    slot.get().cloned().or_else(|| {
        let requested = requested.map(PathBuf::from)?;
        Some(slot.get_or_init(|| requested).clone())
    })
}

// The coordinator publishes its fingerprint to the image it starts, as a
// launch-specification setting, and take_private_env consumes it. A forked child
// constructs the plugin again from the parent's memory, where it is already gone, so
// without this memo the child took the "published no fingerprint" branch and
// wrote that warning, with this build's fingerprint, to the guest's own fd 2.
// Fork inherits this slot as it inherits RPC_SOCKET; execve starts a new image,
// where reverie-sabre passes the REVERIE_SABRE_* settings again in a launch
// specification.
static COORDINATOR_FINGERPRINT: OnceLock<Option<String>> = OnceLock::new();

/// What the coordinator published to this process image (`None`: nothing),
/// read once. The flag is true only for the call that read it; later calls,
/// including a forked child's construction, reuse that reading.
fn remember_coordinator_fingerprint(
    slot: &OnceLock<Option<String>>,
    take: impl FnOnce() -> Option<String>,
) -> (Option<String>, bool) {
    let mut first = false;
    let published = slot.get_or_init(|| {
        first = true;
        take()
    });
    (published.clone(), first)
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-845): Review SaBRe guest comm-name restoration.
fn guest_comm_from_args(args: impl IntoIterator<Item = OsString>) -> Option<CString> {
    let program = args.into_iter().next()?;
    let name = Path::new(&program).file_name()?.as_bytes();
    CString::new(&name[..name.len().min(15)]).ok()
}

fn restore_guest_comm_name(thread_id: u32) {
    if thread_id != unsafe { libc::getpid() as u32 } {
        return;
    }
    let Some(name) = guest_comm_from_args(std::env::args_os()) else {
        return;
    };
    unsafe {
        libc::prctl(libc::PR_SET_NAME, name.as_ptr() as usize, 0, 0, 0);
    }
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-779): Review fail-closed SaBRe RDTSC errors.
fn require_virtual_rdtsc(result: Result<u64, Errno>) -> u64 {
    result.expect("SaBRe RDTSC virtualization failed")
}

fn is_post_load_bootstrap_random(syscall: &Syscall) -> bool {
    matches!(
        syscall,
        Syscall::Getrandom(call)
            if call.buflen() == 32 && call.flags() == libc::GRND_NONBLOCK as usize
    )
}

/// Returns the Detcore SaBRe plugin built beside the running Hermit binary.
// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-738): Review the Hermit-to-SaBRe plugin artifact boundary.
pub fn runtime_library_path() -> io::Result<PathBuf> {
    let executable = std::env::current_exe()?;
    let directory = executable.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "Hermit executable has no parent directory",
        )
    })?;
    [
        directory.join("libdetcore_sabre.so"),
        directory.join("deps/libdetcore_sabre.so"),
    ]
    .into_iter()
    .find(|path| path.is_file())
    .ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "libdetcore_sabre.so was not built beside {}",
                executable.display()
            ),
        )
    })
}

/// Optional loader transport for a supervisor-authenticated later exec or
/// initial static image. Absence of initial opt-in alone grants no authority.
///
/// # Safety
/// Called once by the loader before plugin initialization with its own held
/// callback. The callback itself must return an authenticated typed result.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reverie_sabre_install_loader_continuation_v1(
    callback: sabre::bootstrap::TakeStateFn,
) -> i32 {
    match unsafe { sabre::bootstrap::install(callback) } {
        Ok(()) => 0,
        Err(error) => -error.into_raw(),
    }
}

struct Plugin {
    adapter: RemoteReverieAdapter<Detcore>,
    // The SaBRe-injected runtime requests its hash seed on the first rewritten
    // syscall after post-load. Keep that tool-private draw out of Detcore's
    // guest-visible random stream.
    post_load_syscall_pending: AtomicBool,
}

fn initial_image_generation(pid: i32, bytes: &[u8]) -> io::Result<u64> {
    // The kernel's comm field is raw bytes, including possible ") " bytes.
    // Decode only the PID and start-time fields, outside the final delimiter.
    let end = bytes
        .windows(2)
        .rposition(|pair| pair == b") ")
        .ok_or_else(|| io::Error::other("invalid process stat"))?;
    let prefix = &bytes[..end];
    let owner_end = prefix
        .windows(2)
        .position(|pair| pair == b" (")
        .ok_or_else(|| io::Error::other("invalid stat owner"))?;
    let owner = std::str::from_utf8(&prefix[..owner_end]).map_err(io::Error::other)?;
    if owner.parse::<i32>().map_err(io::Error::other)? != pid {
        return Err(io::Error::other("initial random handoff owner mismatch"));
    }
    let start = bytes[end + 2..]
        .split(u8::is_ascii_whitespace)
        .filter(|field| !field.is_empty())
        .nth(19)
        .ok_or_else(|| io::Error::other("missing process generation"))?;
    std::str::from_utf8(start)
        .map_err(io::Error::other)?
        .parse()
        .map_err(io::Error::other)
}

fn own_initial_image(pid: i32) -> io::Result<detcore::random::InitialImage> {
    fn bounded(path: &str) -> io::Result<Vec<u8>> {
        let mut bytes = Vec::new();
        std::fs::File::open(path)?
            .take(4097)
            .read_to_end(&mut bytes)?;
        if bytes.len() > 4096 {
            return Err(io::Error::other("initial image identity exceeds bound"));
        }
        Ok(bytes)
    }
    let before = bounded("/proc/self/stat")?;
    let start_time_ticks = initial_image_generation(pid, &before)?;
    let auxv = bounded("/proc/self/auxv")?;
    if auxv.len() % 16 != 0 {
        return Err(io::Error::other("incomplete initial auxv"));
    }
    let mut at_random = None;
    let mut terminated = false;
    for row in auxv.as_chunks::<16>().0 {
        let key = u64::from_ne_bytes(row[..8].try_into().unwrap());
        let value = u64::from_ne_bytes(row[8..].try_into().unwrap());
        if key == libc::AT_NULL {
            terminated = value == 0;
            break;
        }
        if key == libc::AT_RANDOM && at_random.replace(value as usize).is_some() {
            return Err(io::Error::other("duplicate initial AT_RANDOM"));
        }
    }
    if !terminated {
        return Err(io::Error::other("unterminated initial auxv"));
    }
    Ok(detcore::random::InitialImage {
        pid,
        start_time_ticks,
        at_random: at_random
            .filter(|p| *p != 0)
            .ok_or_else(|| io::Error::other("missing initial AT_RANDOM"))?,
    })
}

impl Plugin {
    /// Refuse to connect when this plugin and the coordinator were built from
    /// different configuration or clock RPC definitions.
    ///
    /// This plugin is a separate Cargo artifact that lands in the same target
    /// directory as `hermit`, so changing `Config` or `DetTime` -- or merely
    /// switching branches -- leaves it stale while everything still looks built.
    /// `Config` crosses the wire during the handshake, and `DetTime` is the first
    /// field in each Detcore request. A stale plugin can decode either against
    /// the wrong layout. The damage was never the staleness; it was the
    /// diagnosis cost: one added `bool` field surfaced as
    /// `Decode(InvalidBooleanValue(20))` at connect, which names no version and
    /// points nowhere near the plugin, and it blocked every SaBRe measurement
    /// until someone guessed.
    ///
    /// Checked BEFORE the RPC connect so the mismatch is reported instead of
    /// being re-encountered as a codec error a few frames later.
    fn check_coordinator_compatibility() {
        let (expected, first_reading) =
            remember_coordinator_fingerprint(&COORDINATOR_FINGERPRINT, || {
                // SAFETY: plugin construction runs before SaBRe starts guest callbacks.
                unsafe { sabre::take_private_env(CONFIG_FINGERPRINT_ENV) }
                    .map(|v| v.to_string_lossy().into_owned())
            });
        let ours = config_wire_fingerprint();
        match expected {
            Some(expected) if expected == ours => {}
            Some(expected) => panic!(
                "Detcore SaBRe plugin/coordinator MISMATCH: this plugin's Config and clock RPC \
                 definitions have fingerprint {ours}, the coordinator expects {expected}. \
                 The plugin is a separate artifact in the same target directory and is stale -- rebuild \
                 it against this coordinator: cargo build -p detcore-sabre"
            ),
            // An older coordinator does not publish the fingerprint. Say so and
            // continue: refusing here would break pairs that are actually fine,
            // and a guard that rejects matched pairs is worse than no guard.
            None if first_reading => eprintln!(
                "detcore-sabre: coordinator published no configuration and clock RPC fingerprint \
                 ({CONFIG_FINGERPRINT_ENV} unset); proceeding unguarded. This plugin's fingerprint \
                 is {ours}."
            ),
            // A forked child of an image that already said so.
            None => {}
        }
    }

    fn connect() -> Self {
        init_detlog_forwarder();
        Self::check_coordinator_compatibility();
        let socket = coordinator_socket().unwrap_or_else(|| panic!("{RPC_SOCKET_ENV} is not set"));

        let adapter = RemoteReverieAdapter::<Detcore>::connect_with_root_initializer(
            socket,
            |config, pid, state| {
                // This runs only after the existing inherited-fork decision, on
                // the normally constructed guest root. No clocks/metadata are
                // imported and no coordinator readiness event is manufactured.
                let image = own_initial_image(pid.as_raw())?;
                let mut bytes = [0; detcore::random::MAX_INITIAL_STATE_BYTES];
                let count = sabre::bootstrap::take_state(&mut bytes)
                    .map_err(io::Error::other)?
                    .ok_or_else(|| {
                        io::Error::other("required initial random handoff was not negotiated")
                    })?;
                match detcore::random::decode_loader_state(&bytes[..count], config, image)
                    .map_err(io::Error::other)?
                {
                    detcore::random::LoaderState::InitialRandom { .. } => {
                        state
                            .apply_initial_random_state(&bytes[..count], config, image)
                            .map_err(io::Error::other)?;
                    }
                    detcore::random::LoaderState::ObservedExecContinuation
                    | detcore::random::LoaderState::InitialStaticLegacy => {
                        // The held loader and supervisor proved this legacy
                        // image. Preserve every normal constructor field and
                        // let the existing post-exec callback run unchanged.
                    }
                }
                Ok(())
            },
        )
        .expect("failed to connect Detcore SaBRe plugin to coordinator");

        Self {
            adapter,
            post_load_syscall_pending: AtomicBool::new(false),
        }
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1117): Review SaBRe bootstrap-random isolation.
    fn handle_post_load_syscall(&self, syscall: &Syscall) -> Option<Result<usize, Errno>> {
        if !self.post_load_syscall_pending.swap(false, Ordering::AcqRel) {
            return None;
        }

        if !is_post_load_bootstrap_random(syscall) {
            return None;
        }

        let Syscall::Getrandom(call) = syscall else {
            unreachable!("bootstrap-random classifier accepted a non-getrandom syscall")
        };
        let buffer = call.buf().ok_or(Errno::EFAULT);
        Some(buffer.and_then(|buffer| {
            let mut memory = LocalMemory::new();
            memory.write_exact(buffer, &[0; 32]).map(|()| 32)
        }))
    }

    fn handle_vdso(&self, sysno: Sysno, args: SyscallArgs) -> i32 {
        self.adapter
            .handle_syscall(Syscall::from_raw(sysno, args))
            .map_or_else(|errno| -errno.into_raw(), |result| result as i32)
    }
}

#[sabre::tool]
impl reverie_sabre::Tool for Plugin {
    type Client = ();

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1214): Review libc getrandom function interception.
    // Guest calls retain libc's algorithm, return/errno and domain without
    // constructing the tool. Plugin calls use Linux directly so their native
    // entropy cannot seed libc/vDSO opaque state later shared with the guest.
    // Rewritten libc and initial dynamic-bootstrap vDSO syscall sites still
    // intercept guest entropy; static/later-exec vDSO coverage is unchanged.
    // Serving a guest public request directly from Detcore would bypass libc's
    // key-refill/ChaCha path and change ptrace-parity bytes.
    #[detour(lib = "libc", func = "getrandom")]
    fn libc_getrandom(
        buffer: *mut libc::c_void,
        length: libc::size_t,
        flags: libc::c_uint,
    ) -> libc::ssize_t {
        if unsafe { sabre::ffi::calling_from_plugin() } {
            // libc::syscall preserves the C return/errno convention and leaves
            // the caller's domain intact. The kernel validates the buffer.
            return unsafe {
                libc::syscall(libc::SYS_getrandom, buffer, length, flags) as libc::ssize_t
            };
        }
        Self::libc_getrandom_undetoured(buffer, length, flags)
    }

    fn supports_loader_bootstrap() -> bool {
        true
    }

    fn new(_client: Self::Client) -> Self {
        Self::connect()
    }

    fn new_without_legacy_rpc() -> Option<Self> {
        Some(Self::connect())
    }

    fn syscall(&self, syscall: Syscall, _memory: &LocalMemory) -> Result<usize, Errno> {
        if let Some(result) = self.handle_post_load_syscall(&syscall) {
            return result;
        }
        self.adapter.handle_syscall(syscall)
    }

    fn syscall_with_inject<F>(
        &self,
        syscall: Syscall,
        _memory: &LocalMemory,
        inject: F,
    ) -> Result<usize, Errno>
    where
        F: FnMut() -> usize + Send + Sync,
    {
        self.adapter.handle_syscall_with_inject(syscall, inject)
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-755): Review SaBRe RDTSC virtualization.
    fn rdtsc(&self) -> u64 {
        require_virtual_rdtsc(self.adapter.handle_rdtsc())
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-755): Review SaBRe clock_gettime VDSO virtualization.
    fn vdso_clock_gettime(&self, clockid: libc::clockid_t, tp: *mut libc::timespec) -> i32 {
        self.handle_vdso(
            Sysno::clock_gettime,
            SyscallArgs::new(clockid as usize, tp as usize, 0, 0, 0, 0),
        )
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-755): Review SaBRe getcpu VDSO virtualization.
    fn vdso_getcpu(&self, cpu: *mut u32, node: *mut u32, unused: usize) -> i32 {
        self.handle_vdso(
            Sysno::getcpu,
            SyscallArgs::new(cpu as usize, node as usize, unused, 0, 0, 0),
        )
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-755): Review SaBRe gettimeofday VDSO virtualization.
    fn vdso_gettimeofday(&self, tv: *mut libc::timeval, tz: *mut libc::timezone) -> i32 {
        self.handle_vdso(
            Sysno::gettimeofday,
            SyscallArgs::new(tv as usize, tz as usize, 0, 0, 0, 0),
        )
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-755): Review SaBRe time VDSO virtualization.
    fn vdso_time(&self, tloc: *mut libc::time_t) -> i32 {
        self.handle_vdso(Sysno::time, SyscallArgs::new(tloc as usize, 0, 0, 0, 0, 0))
    }

    /// Tell Detcore a signal arrived, with the siginfo the kernel delivered,
    /// before the guest's handler runs, and apply its answer.
    ///
    /// Reverie's central handler delivers the signal, but nothing forwarded
    /// that fact to the tool: this method was the default empty one, and the
    /// remote adapter carried syscalls, RDTSC and lifecycle events but no
    /// signals. So Detcore never recorded a `SignalReceived` event and never
    /// made the `ResourceID::InboundSignal` request its scheduler needs
    /// (`detcore/src/lib.rs:1228`), and under this backend its model of the
    /// guest simply had no signals in it.
    ///
    /// Detcore answers with the signal to deliver, possibly with a rewritten
    /// siginfo (`Guest::set_signal_info`), or `None` to suppress it: the
    /// kernel's own copy of a child-exit `SIGCHLD` whose exit Detcore's
    /// scheduler notifies itself is suppressed, so the guest gets one `SIGCHLD`
    /// per child exit (`scheduler::child_exit_sigchld`). A suppressed signal's
    /// guest action does not run. A failed forward delivers the signal as the
    /// kernel gave it, and is reported.
    fn handle_signal_event_with_info(&self, signal: i32, info: &mut libc::siginfo_t) -> bool {
        let Ok(signal) = Signal::try_from(signal) else {
            return true;
        };
        match self.adapter.handle_signal_with_info(signal, info) {
            Ok(delivered) => delivered.is_some(),
            Err(error) => {
                sabre::eprintln!(
                    "detcore-sabre: forwarding signal {} to Detcore failed: {}",
                    signal,
                    error
                );
                true
            }
        }
    }

    fn on_thread_start(&self, thread_id: u32) {
        restore_guest_comm_name(thread_id);
        self.adapter.handle_thread_start(thread_id);
    }

    fn on_post_load(&self) {
        self.adapter.handle_post_exec();
        self.post_load_syscall_pending
            .store(true, Ordering::Release);
    }

    fn on_thread_exit(&self, thread_id: u32) {
        self.adapter.handle_thread_exit(thread_id);
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::CStr;

    use super::*;

    #[test]
    fn initial_generation_accepts_raw_worker_comm_and_rejects_invalid_identity() {
        struct RestoreComm {
            tid: i32,
            name: [u8; 16],
        }
        impl Drop for RestoreComm {
            fn drop(&mut self) {
                assert_eq!(unsafe { libc::syscall(libc::SYS_gettid) } as i32, self.tid);
                assert_eq!(
                    unsafe { libc::prctl(libc::PR_SET_NAME, self.name.as_ptr()) },
                    0
                );
            }
        }
        // /proc/self/stat names the leader. Use this owned libtest worker's
        // actual stat to exercise the same production parser without renaming
        // the leader or claiming that an injected SaBRe guest was executed.
        let tid = unsafe { libc::syscall(libc::SYS_gettid) } as i32;
        let path = format!("/proc/{tid}/stat");
        let expected = initial_image_generation(tid, &std::fs::read(&path).unwrap()).unwrap();
        let mut restore = RestoreComm { tid, name: [0; 16] };
        assert_eq!(
            unsafe { libc::prctl(libc::PR_GET_NAME, restore.name.as_mut_ptr()) },
            0
        );
        assert_eq!(
            unsafe { libc::prctl(libc::PR_SET_NAME, c"raw) \xff) task".as_ptr()) },
            0
        );
        let bytes = std::fs::read(path).unwrap();
        assert!(bytes.len() <= 4096);
        assert!(std::str::from_utf8(&bytes).is_err());
        assert!(bytes.windows(12).any(|row| row == b"raw) \xff) task"));
        assert_eq!(initial_image_generation(tid, &bytes).unwrap(), expected);
        assert!(
            initial_image_generation(tid + 1, &bytes)
                .unwrap_err()
                .to_string()
                .contains("owner mismatch")
        );
        assert!(initial_image_generation(tid, b"missing delimiter").is_err());
        let end = bytes.windows(2).rposition(|row| row == b") ").unwrap();
        let mut fields: Vec<&[u8]> = bytes[end + 2..]
            .split(u8::is_ascii_whitespace)
            .filter(|field| !field.is_empty())
            .collect();
        let mut malformed = bytes[..end + 2].to_vec();
        fields[19] = b"not-a-number";
        malformed.extend(fields.join(&b' '));
        assert!(initial_image_generation(tid, &malformed).is_err());
        assert!(initial_image_generation(tid, &bytes[..end + 2]).is_err());
        let owner_end = bytes.windows(2).position(|row| row == b" (").unwrap();
        let mut malformed_owner = b"not-a-pid".to_vec();
        malformed_owner.extend_from_slice(&bytes[owner_end..]);
        assert!(initial_image_generation(tid, &malformed_owner).is_err());
        drop(restore);
    }

    #[test]
    fn guest_comm_uses_target_basename_and_linux_limit() {
        assert_eq!(
            guest_comm_from_args(
                ["/usr/bin/bash", "-c", "exit 0"]
                    .into_iter()
                    .map(OsString::from)
            )
            .unwrap()
            .to_bytes(),
            b"bash"
        );
        assert_eq!(
            guest_comm_from_args(["abcdefghijklmnop"].into_iter().map(OsString::from))
                .unwrap()
                .to_bytes(),
            b"abcdefghijklmno"
        );
    }

    #[test]
    fn detlog_forwarding_follows_the_coordinators_per_target_policy() {
        assert_eq!(detlog_forward_policy(None), Ok(None));
        // The historical value still forwards everything.
        assert_eq!(
            detlog_forward_policy(Some(OsStr::new("1"))),
            Ok(Some(ForwardPolicy::all()))
        );
        let scoped = detlog_forward_policy(Some(OsStr::new("0,detcore::random=1")))
            .unwrap()
            .unwrap();
        assert!(scoped.forwards("detcore::random"));
        assert!(!scoped.forwards("detcore::tool_local"));
        assert!(detlog_forward_policy(Some(OsStr::new("yes"))).is_err());
        assert!(detlog_forward_policy(Some(OsStr::from_bytes(b"\xff"))).is_err());
    }

    #[test]
    fn rpc_socket_uses_sabre_private_environment_namespace() {
        assert!(RPC_SOCKET_ENV.starts_with("REVERIE_SABRE_"));
        assert!(DETLOG_FORWARD_ENV.starts_with("REVERIE_SABRE_"));
    }

    #[test]
    fn registers_libc_getrandom_detour() {
        let detour = <Plugin as reverie_sabre::Tool>::detours()
            .iter()
            .find(|detour| unsafe { CStr::from_ptr(detour.fn_name) }.to_bytes() == b"getrandom")
            .expect("libc getrandom detour should be registered");

        assert_eq!(
            unsafe { CStr::from_ptr(detour.lib_name) }.to_bytes(),
            b"libc"
        );

        use std::cell::Cell;

        type Original = fn(*mut libc::c_void, libc::size_t, libc::c_uint) -> libc::ssize_t;
        type Detour =
            unsafe extern "C" fn(*mut libc::c_void, libc::size_t, libc::c_uint) -> libc::ssize_t;

        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        struct Observation {
            buffer: usize,
            length: usize,
            flags: libc::c_uint,
            from_plugin: bool,
            errno: libc::c_int,
        }

        thread_local! {
            static EXPECTED_BUFFER: Cell<*mut libc::c_void> = const { Cell::new(std::ptr::null_mut()) };
            static OBSERVED: Cell<Option<Observation>> = const { Cell::new(None) };
            static CALLS: Cell<usize> = const { Cell::new(0) };
        }

        fn controlled_original(
            buffer: *mut libc::c_void,
            length: libc::size_t,
            flags: libc::c_uint,
        ) -> libc::ssize_t {
            OBSERVED.set(Some(Observation {
                buffer: buffer as usize,
                length,
                flags,
                from_plugin: unsafe { sabre::ffi::calling_from_plugin() },
                errno: unsafe { *libc::__errno_location() },
            }));
            CALLS.set(CALLS.get() + 1);
            if flags == 0x8000_0001 {
                unsafe { *libc::__errno_location() = libc::EINVAL };
                return -1;
            }
            if length == 0 {
                return 0;
            }
            // Do not dereference a pointer or extent corrupted by the wrapper;
            // the caller's exact observation and output assertions report it.
            if !buffer.is_null() && buffer == EXPECTED_BUFFER.get() && length == 16 && flags == 0 {
                unsafe {
                    std::ptr::copy_nonoverlapping(b"libc-vd".as_ptr(), buffer.cast::<u8>(), 7);
                }
            }
            7
        }

        struct RestoreThreadState {
            from_plugin: bool,
            errno: libc::c_int,
        }

        impl Drop for RestoreThreadState {
            fn drop(&mut self) {
                EXPECTED_BUFFER.set(std::ptr::null_mut());
                unsafe {
                    if self.from_plugin {
                        sabre::ffi::enter_plugin();
                    } else {
                        sabre::ffi::exit_plugin();
                    }
                    *libc::__errno_location() = self.errno;
                }
            }
        }

        let _restore = RestoreThreadState {
            from_plugin: unsafe { sabre::ffi::calling_from_plugin() },
            errno: unsafe { *libc::__errno_location() },
        };
        // The macro stores Original's Rust signature and returns its C-ABI
        // stub through SaBRe's erased function-pointer ABI. Exercise those
        // generated functions, not a direct call to the detour body.
        // This test alone installs the original; all probe state is per-thread.
        let original = unsafe {
            std::mem::transmute::<Original, sabre::ffi::void_void_fn>(
                controlled_original as Original,
            )
        };
        let stub = unsafe {
            std::mem::transmute::<sabre::ffi::void_void_fn, Detour>((detour.icept_callback)(
                original,
            ))
        };
        unsafe { sabre::ffi::exit_plugin() };
        for (length, flags, expected_result, expected_errno) in [
            (16, 0, 7, libc::E2BIG),
            (16, 0x8000_0001, -1, libc::EINVAL),
            (0, 0, 0, libc::E2BIG),
        ] {
            let mut bytes = [0xa5u8; 16];
            let buffer = if length == 0 {
                std::ptr::null_mut()
            } else {
                bytes.as_mut_ptr().cast::<libc::c_void>()
            };
            EXPECTED_BUFFER.set(buffer);
            OBSERVED.set(None);
            CALLS.set(0);
            unsafe { *libc::__errno_location() = libc::E2BIG };
            let result = unsafe { stub(buffer, length, flags) };
            let errno = unsafe { *libc::__errno_location() };
            let domain_after = unsafe { sabre::ffi::calling_from_plugin() };
            assert_eq!(CALLS.get(), 1);
            assert_eq!(
                OBSERVED.get(),
                Some(Observation {
                    buffer: buffer as usize,
                    length,
                    flags,
                    from_plugin: false,
                    errno: libc::E2BIG,
                })
            );
            assert_eq!(result, expected_result);
            assert_eq!(errno, expected_errno);
            assert!(!domain_after);
            let mut expected_bytes = [0xa5; 16];
            if expected_result == 7 {
                expected_bytes[..7].copy_from_slice(b"libc-vd");
            }
            assert_eq!(bytes, expected_bytes);
            EXPECTED_BUFFER.set(std::ptr::null_mut());
        }

        unsafe { sabre::ffi::enter_plugin() };
        for (length, flags, null_buffer, expected_result, expected_errno) in [
            (16, 0, false, 16, libc::E2BIG),
            (16, 0x8000_0001, false, -1, libc::EINVAL),
            (0, 0, true, 0, libc::E2BIG),
            (1, 0, true, -1, libc::EFAULT),
        ] {
            let mut bytes = [0xa5u8; 24];
            let buffer = if null_buffer {
                std::ptr::null_mut()
            } else {
                bytes[4..20].as_mut_ptr().cast::<libc::c_void>()
            };
            EXPECTED_BUFFER.set(buffer);
            OBSERVED.set(None);
            CALLS.set(0);
            unsafe { *libc::__errno_location() = libc::E2BIG };
            let result = unsafe { stub(buffer, length, flags) };
            let errno = unsafe { *libc::__errno_location() };
            let domain_after = unsafe { sabre::ffi::calling_from_plugin() };
            assert_eq!(CALLS.get(), 0, "plugin entropy must bypass libc state");
            assert_eq!(OBSERVED.get(), None);
            assert_eq!(result, expected_result);
            assert_eq!(errno, expected_errno);
            assert!(domain_after);
            assert_eq!(&bytes[..4], &[0xa5; 4]);
            assert_eq!(&bytes[20..], &[0xa5; 4]);
            if expected_result <= 0 {
                assert_eq!(bytes, [0xa5; 24]);
            }
            EXPECTED_BUFFER.set(std::ptr::null_mut());
        }
    }

    #[test]
    fn rpc_socket_survives_plugin_reinitialization() {
        let socket = OnceLock::new();

        assert_eq!(
            remember_coordinator_socket(&socket, Some(OsStr::new("/tmp/coordinator.sock"))),
            Some(PathBuf::from("/tmp/coordinator.sock"))
        );
        assert_eq!(
            remember_coordinator_socket(&socket, None),
            Some(PathBuf::from("/tmp/coordinator.sock"))
        );
    }

    #[test]
    fn coordinator_fingerprint_survives_plugin_reinitialization() {
        let published = OnceLock::new();
        assert_eq!(
            remember_coordinator_fingerprint(&published, || Some("0123456789abcdef".to_owned())),
            (Some("0123456789abcdef".to_owned()), true)
        );
        // A forked child's construction finds the variable already erased.
        assert_eq!(
            remember_coordinator_fingerprint(&published, || None),
            (Some("0123456789abcdef".to_owned()), false)
        );

        let unpublished = OnceLock::new();
        assert_eq!(
            remember_coordinator_fingerprint(&unpublished, || None),
            (None, true)
        );
        assert_eq!(
            remember_coordinator_fingerprint(&unpublished, || panic!("read twice")),
            (None, false)
        );
    }

    #[test]
    fn virtual_rdtsc_returns_coordinator_value() {
        assert_eq!(require_virtual_rdtsc(Ok(42)), 42);
    }

    #[test]
    fn recognizes_only_sabre_post_load_bootstrap_random_shape() {
        let buffer = 0x1234;
        let syscall = |length, flags| {
            Syscall::from_raw(
                Sysno::getrandom,
                SyscallArgs::new(buffer, length, flags, 0, 0, 0),
            )
        };

        assert!(is_post_load_bootstrap_random(&syscall(
            32,
            libc::GRND_NONBLOCK as usize
        )));
        assert!(!is_post_load_bootstrap_random(&syscall(
            31,
            libc::GRND_NONBLOCK as usize
        )));
        assert!(!is_post_load_bootstrap_random(&syscall(32, 0)));
        assert!(!is_post_load_bootstrap_random(&Syscall::from_raw(
            Sysno::getpid,
            SyscallArgs::new(0, 0, 0, 0, 0, 0),
        )));
    }

    #[test]
    #[should_panic(expected = "SaBRe RDTSC virtualization failed")]
    fn virtual_rdtsc_error_fails_closed() {
        require_virtual_rdtsc(Err(Errno::EIO));
    }
}
