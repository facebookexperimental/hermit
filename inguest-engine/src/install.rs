//! Ordinary Detcore installation shared by first-party entry mechanisms.
//!
//! The original leaf is unchanged. Only ordinary constructor branches are
//! composed here; no private issuer, lifecycle permit or native image path is
//! moved or represented by a flag. Private-image owners remain separate.

use core::ffi::c_char;
use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::path::Path;

use reverie_inguest::guest::support::describe_io_error;

use crate::runtime::EngineRuntime;
use crate::runtime::FrameHookHost;

#[path = "loader_settings.rs"]
mod loader_settings;

const DETLOG_FORWARD_ENV: &str = "HERMIT_LITEINST_FORWARD_DETLOG";
const DETLOG_FORWARD_POLICY_ENV: &str = "HERMIT_LITEINST_FORWARD_DETLOG_POLICY";
static TOOL_OUTPUT_IDENTITY: std::sync::OnceLock<(u64, u64)> = std::sync::OnceLock::new();

// Loader selection for ordinary entry only, never a private phase authority.
#[derive(Clone, Copy)]
enum Mechanism {
    PatchedSites,
    SavedFrames,
}

struct InitializationInputs {
    coordinator_socket: OsString,
    forward_request: Option<OsString>,
    forward_policy: Option<OsString>,
    coordinator_fingerprint: Option<OsString>,
}

pub(crate) unsafe fn preload() {
    // Retain the constructor's dispatch lifetime. The preload's allocator
    // owns Rust allocations unconditionally; this guard does not choose
    // between Tool storage and the guest's malloc.
    let _private_allocations = reverie_inguest::guest::alloc::enter_dispatch();
    // Hermit sends the coordinator and its settings in the sealed bootstrap,
    // so none of them is on the guest's initial stack or in
    // /proc/self/environ (detcore::in_guest_launch). A launcher that sets the
    // environment variables instead still works, below.
    // SAFETY: this is the preload constructor the launcher started.
    match unsafe { reverie_liteinst::take_preload_bootstrap() } {
        Ok(Some(bootstrap)) => {
            let launch = detcore::in_guest_launch::InGuestLaunch::decode(&bootstrap.tool_data)
                .unwrap_or_else(|error| {
                    fail(&format!("the launch bootstrap is unreadable: {error}"))
                });
            // SAFETY: as below.
            unsafe {
                initialize_detcore(
                    Mechanism::PatchedSites,
                    InitializationInputs {
                        coordinator_socket: bootstrap.coordinator.into_os_string(),
                        forward_request: launch.forward_request.map(OsString::from),
                        forward_policy: launch.forward_policy.map(OsString::from),
                        coordinator_fingerprint: launch.coordinator_fingerprint.map(OsString::from),
                    },
                );
            }
            return;
        }
        Ok(None) => {}
        Err(error) => fail(&format!(
            "the launch bootstrap is unreadable: {}",
            describe_io_error(&error)
        )),
    }
    let Some(socket) = std::env::var_os(reverie_liteinst::COORDINATOR_ENV) else {
        fail("the coordinator socket environment variable is missing");
    };
    let forward_request = std::env::var_os(DETLOG_FORWARD_ENV);
    let forward_policy = std::env::var_os(DETLOG_FORWARD_POLICY_ENV);
    // An empty value counts as absent: it is what the removal below leaves in
    // main's initial environment, which a shell the guest runs passes on.
    let coordinator_fingerprint = std::env::var_os(detcore::LITEINST_CONFIG_FINGERPRINT_ENV)
        .filter(|value| !value.is_empty());
    // SAFETY: the loader runs constructors while the process is still
    // single-threaded, so nothing reads the environment concurrently.
    unsafe {
        // Hermit's fingerprint is for this check only, so it is removed as
        // the DETLOG variables are: the value's bytes are zeroed, and a
        // program reading main's initial environment (bash does) sees the
        // name with an empty value. Later process images load this same file
        // through LD_PRELOAD, so the first image's check covers them.
        scrub_env(detcore::LITEINST_CONFIG_FINGERPRINT_ENV);
        std::env::remove_var(detcore::LITEINST_CONFIG_FINGERPRINT_ENV);
        // The value names a host socket identity: zero its bytes in the
        // environment block too, which /proc/self/environ shows.
        scrub_env(DETLOG_FORWARD_ENV);
        std::env::remove_var(DETLOG_FORWARD_ENV);
        std::env::remove_var(DETLOG_FORWARD_POLICY_ENV);
        std::env::remove_var(reverie_liteinst::COORDINATOR_ENV);
    }
    // SAFETY: this is the loader's single-threaded constructor window, and
    // its private allocation/dispatch scope remains live through installation.
    unsafe {
        initialize_detcore(
            Mechanism::PatchedSites,
            InitializationInputs {
                coordinator_socket: socket,
                forward_request,
                forward_policy,
                coordinator_fingerprint,
            },
        );
    }
}

// The real saved-frame loader post_load runs before libc's environment initialization.
// Copy bounded loader-owned NAME=VALUE strings; never consult std::env here.
pub(crate) unsafe fn loader(argc: usize, argv: *const *const c_char) {
    let _private_allocations = reverie_inguest::guest::alloc::enter_dispatch();
    if !(1..=64).contains(&argc) || argv.is_null() {
        fail("invalid saved-frame loader plugin argument population");
    }
    let mut socket = None;
    let mut forward_request = None;
    let mut forward_policy = None;
    let mut fingerprint = None;
    for index in 1..argc {
        // SAFETY: the loader owns argc valid strings; the bounded reader
        // refuses missing termination rather than scanning indefinitely.
        let entry = unsafe { bounded_setting(*argv.add(index)) };
        let Some(at) = entry.iter().position(|b| *b == b'=') else {
            fail("saved-frame loader plugin setting is not NAME=VALUE");
        };
        let (name, value) = (&entry[..at], &entry[at + 1..]);
        let slot = match name {
            b"HERMIT_ENGINE_COORDINATOR" => &mut socket,
            b"HERMIT_ENGINE_FORWARD_DETLOG" => &mut forward_request,
            b"HERMIT_ENGINE_FORWARD_DETLOG_POLICY" => &mut forward_policy,
            b"HERMIT_ENGINE_CONFIG_FINGERPRINT" => &mut fingerprint,
            _ => fail("unknown saved-frame loader common-engine setting"),
        };
        if slot.replace(OsString::from_vec(value.to_vec())).is_some() {
            fail("duplicate saved-frame loader common-engine setting");
        }
    }
    let socket = socket
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| fail("saved-frame loader common-engine coordinator is missing"));
    let fingerprint = fingerprint
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| fail("saved-frame loader common-engine fingerprint is missing"));
    // SAFETY: genuine post_load, with fixed storage and allocation scope live.
    unsafe {
        initialize_detcore(
            Mechanism::SavedFrames,
            InitializationInputs {
                coordinator_socket: socket,
                forward_request,
                forward_policy,
                coordinator_fingerprint: Some(fingerprint),
            },
        )
    };
}

unsafe fn bounded_setting(pointer: *const c_char) -> Vec<u8> {
    if pointer.is_null() {
        fail("null saved-frame loader setting");
    }
    for length in 0..16_384 {
        // SAFETY: the loader promised a valid NUL-terminated string. The cap
        // only bounds work; it cannot authenticate an arbitrary pointer.
        if unsafe { *pointer.add(length) } == 0 {
            return unsafe { core::slice::from_raw_parts(pointer.cast::<u8>(), length) }.to_vec();
        }
    }
    fail("saved-frame loader setting exceeds the loader ABI's 16-KiB cap");
}

unsafe fn initialize_detcore(mechanism: Mechanism, inputs: InitializationInputs) {
    #[cfg(feature = "dispatch-observer")]
    crate::observation::install();
    let InitializationInputs {
        coordinator_socket: socket,
        forward_request,
        forward_policy,
        coordinator_fingerprint,
    } = inputs;
    // Checked before anything is decoded: the coordinator sends Detcore's
    // configuration as positional bincode, which a runtime built from another
    // tree decodes as garbage and reports only as a decoding error
    // (https://github.com/rrnewton/hermit/issues/3986). Without the variable
    // (an image the first one executed, or a Hermit from before this check)
    // nothing is refused: a check that rejects matched pairs is worse than
    // none, as for the saved-frame loader plugin.
    if let Some(mismatch) = coordinator_fingerprint
        .as_deref()
        .and_then(detcore::config_fingerprint_mismatch)
    {
        fail(&format!(
            "the runtime at {} was built from a different tree than the hermit running it: \
             {mismatch}. Rebuild the common engine and coordinator from matching source",
            runtime_path()
        ));
    }
    if let Some(value) = forward_request {
        let Some((fd, identity)) = value
            .to_str()
            .and_then(detcore::detlog::parse_tool_output_env_value)
        else {
            fail("the DETLOG forwarding descriptor value is unreadable");
        };
        // Hermit sends the policy with the descriptor; without a readable one
        // this process cannot forward the records ptrace would log.
        let policy = match loader_settings::forwarding_policy(forward_policy.as_deref()) {
            Ok(Some(policy)) => policy,
            Err(error) => fail(&format!(
                "the DETLOG forwarding policy is unreadable: {error}"
            )),
            Ok(None) => fail("the DETLOG forwarding policy is missing"),
        };
        // A signal handler installed by guest code that ran before this
        // constructor could otherwise replace the number between the identity
        // check and the reservation; and the reserved copy, which is what the
        // Tool keeps, must itself be the socket Hermit passed.
        let blocked = block_signals();
        let reserved = if detcore::detlog::socket_identity(fd) != Some(identity) {
            Err(
                "it is not the socket Hermit passed (guest code that ran before this \
                 constructor closed or replaced it)"
                    .to_owned(),
            )
        } else {
            // SAFETY: the process is still single-threaded and Detcore is not
            // yet installed, which is when the reservation must be made.
            match unsafe {
                reverie_liteinst::reserve_tool_output_fd(
                    fd,
                    detcore::detlog::FORWARDING_RETIRED_NOTICE,
                )
            } {
                Ok(reserved) if detcore::detlog::socket_identity(reserved) == Some(identity) => {
                    Ok(reserved)
                }
                Ok(_) => Err("the reserved copy is not the socket Hermit passed".to_owned()),
                Err(error) => Err(describe_io_error(&error)),
            }
        };
        restore_signals(&blocked);
        match reserved {
            Ok(_) => {
                let _ = TOOL_OUTPUT_IDENTITY.set(identity);
                let _ = detcore::detlog::set_forwarder(forward_detlog, policy);
            }
            Err(error) => fail(&format!(
                "cannot reserve the DETLOG forwarding descriptor {fd}: {error}"
            )),
        }
    }
    // Detcore's exit-dependency refusals name only the capability; this
    // backend adds its name and the alternative.
    detcore::exit_dependencies::set_backend_advice(
        "the ordinary common in-guest engine cannot run this \
         program; run it with --backend=ptrace.",
    );
    // Coord rulings A and D.1: Detcore checks every descriptor that arrives
    // later; this checks the ones the process image starts with. A forked
    // child holds only what its parent held or received.
    match detcore::exit_dependencies::held_exit_dependency(Path::new("/proc/self/fd")) {
        Ok(None) => {}
        Ok(Some((fd, what))) => fail(&format!(
            "the common in-guest engine refuses a guest that starts holding {what} (descriptor {fd}): \
             Hermit holds every guest's turn until an exit completes, and a guest serving that \
             descriptor could make another guest's exit wait forever; run this program with \
             --backend=ptrace"
        )),
        Err(error) => fail(&format!(
            "cannot list the descriptors this process started with: {}",
            describe_io_error(&error)
        )),
    }
    // SAFETY: the loader runs constructors before any application thread
    // exists and before the application can install a seccomp filter, which is
    // the window `install_tool` requires.
    // Both paths publish exactly ToolHost<Detcore, EngineRuntime>. The
    // concrete backend services stay boxed within the same engine DSO.
    let installed = match mechanism {
        Mechanism::PatchedSites => unsafe {
            reverie_liteinst::install_tool_from_bootstrap_with_runtime::<
                detcore::Detcore,
                EngineRuntime,
            >(Path::new(&socket), EngineRuntime::new)
        },
        Mechanism::SavedFrames => unsafe {
            reverie_inguest::guest::tool_install::install_tool_with_runtime_and_root_initializer::<
                detcore::Detcore,
                _,
                EngineRuntime,
                _,
            >(
                Path::new(&socket),
                false,
                FrameHookHost::new(),
                |services| EngineRuntime::new(Box::new(services)),
                crate::loader_state::initialize_root,
            )
        },
    };
    if let Err(error) = installed {
        fail(&describe_io_error(&error));
    }
    // `--backend=in-guest-trap`: the Tool's constructor recorded the host's
    // request from the coordinator's configuration; compare it with the
    // settings the runtime actually captured while installing, which guest
    // code cannot change afterwards, and refuse to run the program when guest
    // code that ran before this point changed the environment they were read
    // from. The program's own code has not run.
    if let Some(violation) = detcore::in_guest_site_patching::violation(
        detcore::in_guest_site_patching::site_patching_off_required(),
        match mechanism {
            Mechanism::PatchedSites => reverie_liteinst::site_patching_enabled(),
            Mechanism::SavedFrames => false,
        },
        reverie_liteinst::guest_stats_enabled(),
    ) {
        fail(&violation);
    }
    // A description a fork shares is reached through the coordinator, over
    // this process's one existing connection. The first installation in a
    // process image is the only one, and a forked child inherits it.
    let _ = detcore::install_shared_open_file_channel(Box::new(
        detcore::ChunkedOpenFileChannel::new(CoordinatorOpenFiles),
    ));
    // Detcore's warnings reach the run's log through the coordinator, over the
    // same connection. A forked child inherits the subscriber.
    if !detcore::guest_log::install(guest_log_call, None) {
        fail("Detcore's in-guest subscriber was not installed: the process already has one");
    }
    // Detcore reports the guest signal controls this runtime refuses before
    // Detcore sees them, once per run (`detcore::refused_signal_controls`). A
    // forked child inherits the source.
    let _ = detcore::refused_signal_controls::install(refused_signal_controls);
}

fn coordinator_call(
    request: <detcore::GlobalState as reverie::GlobalTool>::Request,
) -> std::io::Result<<detcore::GlobalState as reverie::GlobalTool>::Response> {
    reverie_liteinst::blocking_global_rpc::<detcore::GlobalState>(request)
}

/// The guest signal controls this runtime refused since the last call
/// (`reverie_inguest::guest::protect::take_refused_signal_controls`), as
/// Detcore takes them.
fn refused_signal_controls() -> detcore::refused_signal_controls::RefusedSignalControls {
    let refused = reverie_inguest::guest::protect::take_refused_signal_controls();
    (refused.actions, refused.alternate_stack)
}

/// [`coordinator_call`] for Detcore's warnings, which can be raised anywhere:
/// it sends only where the runtime marks a Tool callback as running
/// (`tool_callback_active`), since outside one the runtime would refuse the
/// connection's syscalls as the guest's and end the guest. Elsewhere the
/// warning is refused, and `detcore::guest_log` counts it.
fn guest_log_call(
    request: <detcore::GlobalState as reverie::GlobalTool>::Request,
) -> std::io::Result<<detcore::GlobalState as reverie::GlobalTool>::Response> {
    if !reverie_inguest::guest::support::tool_callback_active() {
        return Err(std::io::Error::other(
            "a Detcore warning from outside a Tool callback",
        ));
    }
    coordinator_call(request)
}

/// Carries shared open file control messages to Detcore's global state from
/// inside a Tool callback, through [`coordinator_call`].
/// A `DetFd` method is synchronous code inside a callback's poll, so no other
/// request of this process is in flight when it runs; if one is, the call is
/// refused rather than interleaved.
struct CoordinatorOpenFiles;

impl detcore::OpenFileControlTransport for CoordinatorOpenFiles {
    fn call(
        &self,
        control: detcore::OpenFileControl,
    ) -> Result<detcore::OpenFileControlReply, detcore::SharedOpenFileError> {
        let response =
            coordinator_call(detcore::shared_open_file_request(control)).map_err(|error| {
                detcore::SharedOpenFileError(format!(
                    "the coordinator connection refused a shared open file message: {}",
                    describe_io_error(&error)
                ))
            })?;
        detcore::shared_open_file_reply(response)
    }
}

/// Sends one Detcore record on the reserved socket. Its number can move when the
/// guest `dup2`s onto it, so it is read for each record.
fn forward_detlog(target: &str, record_suffix: &str, index: u64, message: std::fmt::Arguments<'_>) {
    if let Some(socket) = authenticated_output_socket() {
        detcore::detlog::send_forwarded_record(socket, target, record_suffix, index, message);
    }
}

fn authenticated_output_socket() -> Option<libc::c_int> {
    let socket = reverie_liteinst::tool_output_fd()?;
    (TOOL_OUTPUT_IDENTITY
        .get()
        .is_some_and(|identity| detcore::detlog::socket_identity(socket) == Some(*identity)))
    .then_some(socket)
}

// Same ordinary constructor signal masks; private InitialImage rules are
// deliberately left in their unchanged maintained owner.
fn block_signals() -> u64 {
    let all = u64::MAX;
    let mut previous = 0_u64;
    // SAFETY: both masks are initialized and live for the synchronous syscall.
    let _ = unsafe {
        reverie_inguest::signal::raw_sigprocmask(libc::SIG_BLOCK, Some(&all), Some(&mut previous))
    };
    previous
}

fn restore_signals(previous: &u64) {
    // SAFETY: previous is the saved initialized mask.
    let _ = unsafe {
        reverie_inguest::signal::raw_sigprocmask(libc::SIG_SETMASK, Some(previous), None)
    };
}

/// Overwrites the bytes of `key`'s entry in the environment block with zeros.
///
/// # Safety
///
/// The process must be single-threaded.
unsafe fn scrub_env(key: &str) {
    unsafe extern "C" {
        static environ: *const *mut libc::c_char;
    }
    let mut prefix = key.as_bytes().to_vec();
    prefix.push(b'=');
    // SAFETY: `environ` is a NULL-terminated array of NUL-terminated strings.
    unsafe {
        let mut slot = environ;
        while !slot.is_null() && !(*slot).is_null() {
            let entry = *slot;
            let bytes = std::ffi::CStr::from_ptr(entry).to_bytes();
            if bytes.starts_with(&prefix) {
                std::ptr::write_bytes(
                    entry.cast::<u8>().add(prefix.len()),
                    0,
                    bytes.len() - prefix.len(),
                );
            }
            slot = slot.add(1);
        }
    }
}

/// The file this runtime was loaded from, as the dynamic loader reports it.
fn runtime_path() -> String {
    let mut info = std::mem::MaybeUninit::<libc::Dl_info>::zeroed();
    // SAFETY: `dladdr` writes `info` and reads nothing else; the address is a
    // function of this library.
    let found = unsafe {
        libc::dladdr(
            crate::hermit_engine_preload_initialize_v1 as *const libc::c_void,
            info.as_mut_ptr(),
        )
    };
    // SAFETY: `dladdr` filled `info` when it returned nonzero.
    let name = (found != 0).then(|| unsafe { info.assume_init() }.dli_fname);
    match name.filter(|name| !name.is_null()) {
        // SAFETY: the loader's file name is a NUL-terminated string it keeps
        // for as long as the library stays loaded.
        Some(name) => unsafe { std::ffi::CStr::from_ptr(name) }
            .to_string_lossy()
            .into_owned(),
        None => "an unknown path".to_owned(),
    }
}

pub(crate) fn fail(message: &str) -> ! {
    crate::poison();
    write_stderr(b"hermit common engine: initialization failed: ");
    write_stderr(message.as_bytes());
    write_stderr(b"\n");
    // Preserve the ordinary leaf's libc _exit route so an installed runtime
    // submits exit statistics. This is not an InitialImage failure path.
    unsafe { libc::_exit(127) }
}

pub(crate) fn refuse(message: &[u8]) -> ! {
    crate::poison();
    write_stderr(message);
    // The same trusted gate used by the existing rewritten-entry refusal.
    unsafe { reverie_inguest::guest::support::exit_now(122) }
}

fn write_stderr(message: &[u8]) {
    unsafe {
        reverie_inguest::trap::raw_syscall6(
            libc::SYS_write,
            [
                libc::STDERR_FILENO as u64,
                message.as_ptr() as u64,
                message.len() as u64,
                0,
                0,
                0,
            ],
        );
    }
}
