//! In-guest Detcore runtime for the LiteInst backend.
//!
//! Hermit preloads this library into the guest for `--backend=liteinst`. When
//! the dynamic loader loads it, its constructor installs Detcore's `Tool`
//! inside the guest process before the program's `main`. The loader, the C library's initialization, `IFUNC`
//! resolvers and the constructors of the program's own shared libraries run
//! earlier, unmonitored. A loader that fails to load the library runs the
//! guest without it; Hermit reports that the guest never connected only after
//! the guest exits. Once Detcore is installed, patched system calls and the
//! SIGSYS fallback dispatch to Detcore in the guest, and Detcore's global
//! state stays in Hermit, reached over the coordinator RPC socket that
//! `reverie_liteinst::LiteinstBackend::run_with_preload` creates. No ptrace
//! tracer sits on the system-call path.
#![deny(missing_docs)]

// SHARED FILE: the SaBRe plugin's glibc compatibility definitions, compiled
// into this runtime as well (https://github.com/rrnewton/hermit/issues/3967);
// an edit there changes both guest preloads. build.rs explains why this
// runtime needs them.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
#[path = "../../detcore-sabre/src/glibc_compat.rs"]
mod glibc_compat;

use std::path::Path;

// The preload leaf owns all Rust allocations, including constructor work
// outside installation and dispatch scopes. The shared LiteInst rlib remains
// allocator neutral.
reverie_liteinst::declare_fixed_tool_allocator!();

// Errors are printed without the C library's errno messages: strerror_r can
// allocate through the guest's malloc (see describe_io_error).
use reverie_inguest::guest::support::describe_io_error;
#[cfg(feature = "exec-private-init")]
mod exec_initial;
use std::ffi::OsString;

#[cfg(feature = "exec-private-init")]
pub use exec_initial::detcore_liteinst_exec_initialize;

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-3635): Review the
// in-guest Detcore constructor boundary.

/// Private opt-in, set by Hermit, for forwarding the in-guest Tool's
/// deterministic INFO records: the number and identity of an inherited socket
/// (the sending end of a Unix `SOCK_SEQPACKET` pair) to send them on
/// (`detcore::detlog::tool_output_env_value`). The constructor removes it from
/// the guest's environment (scrubbing its bytes), refuses to start when the
/// number no longer names that socket (guest `.preinit_array` code, which runs
/// before this constructor, closed it or put a socket of its own there), moves
/// the socket to a number
/// Reverie reserves and protects from the guest
/// (`reverie_liteinst::reserve_tool_output_fd`), and sends each record there
/// as one message, where Hermit's `--verify` reads them into the run's log. The
/// guest cannot write to, close or shut down that socket, so its own output
/// never mixes with the records.
pub const DETLOG_FORWARD_ENV: &str = "HERMIT_LITEINST_FORWARD_DETLOG";

/// Set by Hermit next to [`DETLOG_FORWARD_ENV`]: the encoded
/// `detcore::detlog::ForwardPolicy`, the CLI filter's INFO answer for each
/// target its `RUST_LOG` names and for any other. The forwarder sends a record
/// only when that policy logs its module, which is what each in-process
/// `detlog!` callsite asks tracing under ptrace. The constructor removes it
/// from the guest's environment.
pub const DETLOG_FORWARD_POLICY_ENV: &str = "HERMIT_LITEINST_FORWARD_DETLOG_POLICY";

/// Hermit validates that this constructor is registered in `.init_array`
/// before it preloads the library, so a DSO that would load without
/// installing Detcore is refused instead of running the guest unmonitored.
#[cfg(not(feature = "exec-private-init"))]
#[used]
#[unsafe(link_section = ".init_array")]
static DETCORE_LITEINST_INIT: unsafe extern "C" fn() = detcore_liteinst_initialize;

/// The identity (`st_dev`, `st_ino`) of the DETLOG socket Hermit passed, once
/// it is reserved. Until `install_tool` protects the reserved number, guest
/// code (a signal handler a guest library's constructor installed) could
/// replace it, so the constructor checks it again after installation and
/// every record's send checks it too.
static TOOL_OUTPUT_IDENTITY: std::sync::OnceLock<(u64, u64)> = std::sync::OnceLock::new();

/// Installs Detcore in the current guest process.
///
/// The `.init_array` entry above runs this once per process image, before
/// `main`. It exits the process with status 127 when the coordinator socket is
/// missing or Detcore cannot be installed, so a guest never runs past a failed
/// installation.
///
/// The native entry establishes a fixed guarded bootstrap stack before any
/// Rust code runs. It preserves the constructor's existing signal and
/// environment behavior; placing its stack does not isolate those effects.
///
/// # Safety
///
/// Only the dynamic loader may call this, from `.init_array`, while the process
/// is still single-threaded and before any seccomp filter is active.
#[unsafe(no_mangle)]
#[unsafe(naked)]
pub unsafe extern "C" fn detcore_liteinst_initialize() {
    core::arch::naked_asm!(
        "endbr64",
        "lea rdi, [rip + {body}]",
        "jmp {entry}",
        body = sym detcore_liteinst_initialize_body,
        entry = sym reverie_inguest::guest::tool_region::constructor_entry,
    );
}

// Keep the exported ABI while binding this DSO's loader registration and
// self-references locally. A competing executable export must not replace the
// native constructor before it establishes its owned stack.
core::arch::global_asm!(".protected detcore_liteinst_initialize");

// The pre-switch RIP-relative address must resolve locally, without an
// interposable body or a dynamic-loader resolver using the caller's stack.
core::arch::global_asm!(".hidden detcore_liteinst_initialize_body");

// Captured before initialization so the common path does not select its
// coordinator or DETLOG policy from the environment. The legacy constructor
// owns environment acquisition and scrubbing.
struct InitializationInputs {
    coordinator_socket: OsString,
    forward_request: Option<OsString>,
    forward_policy: Option<OsString>,
    coordinator_fingerprint: Option<OsString>,
    #[cfg(feature = "exec-private-init")]
    initial_image: Option<exec_initial::InitialImageInputs>,
}

#[inline(never)]
#[unsafe(no_mangle)]
unsafe extern "C" fn detcore_liteinst_initialize_body() {
    // Retain the constructor's dispatch lifetime. The preload's allocator
    // owns Rust allocations unconditionally; this guard does not choose
    // between Tool storage and the guest's malloc.
    let _private_allocations = reverie_inguest::guest::alloc::enter_dispatch();
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
    }
    // SAFETY: this is the loader's single-threaded constructor window, and
    // its private allocation/dispatch scope remains live through installation.
    unsafe {
        initialize_detcore(InitializationInputs {
            coordinator_socket: socket,
            forward_request,
            forward_policy,
            coordinator_fingerprint,
            #[cfg(feature = "exec-private-init")]
            initial_image: None,
        });
    }
}

// Consume captured inputs while retaining the genuine Detcore installation,
// descriptor checks and shared open-file channel on its one RPC connection.
//
// SAFETY: the caller must hold the private allocation/dispatch scope in the
// single-threaded, pre-seccomp initialization window required by install_tool.
unsafe fn initialize_detcore(inputs: InitializationInputs) {
    let InitializationInputs {
        coordinator_socket: socket,
        forward_request,
        forward_policy,
        coordinator_fingerprint,
        #[cfg(feature = "exec-private-init")]
        initial_image,
    } = inputs;
    // Checked before anything is decoded: the coordinator sends Detcore's
    // configuration as positional bincode, which a runtime built from another
    // tree decodes as garbage and reports only as a decoding error
    // (https://github.com/rrnewton/hermit/issues/3986). Without the variable
    // (an image the first one executed, or a Hermit from before this check)
    // nothing is refused: a check that rejects matched pairs is worse than
    // none, as for the SaBRe plugin.
    if let Some(mismatch) = coordinator_fingerprint
        .as_deref()
        .and_then(detcore::config_fingerprint_mismatch)
    {
        fail(&format!(
            "the runtime at {} was built from a different tree than the hermit running it: \
             {mismatch}. Rebuild it from Hermit's tree (`cargo build -p detcore-liteinst`), or \
             point HERMIT_LITEINST_TOOL_RUNTIME at a matching build",
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
        let policy = match forward_policy.as_deref().map(|value| {
            value
                .to_str()
                .ok_or_else(|| "it is not UTF-8".to_owned())
                .and_then(detcore::detlog::ForwardPolicy::decode)
        }) {
            Some(Ok(policy)) => policy,
            Some(Err(error)) => fail(&format!(
                "the DETLOG forwarding policy is unreadable: {error}"
            )),
            None => fail("the DETLOG forwarding policy is missing"),
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
                #[cfg(feature = "exec-private-init")]
                if exec_initial::active() {
                    if TOOL_OUTPUT_IDENTITY.set(identity).is_err()
                        || detcore::detlog::set_forwarder(forward_detlog, policy).is_err()
                    {
                        fail("InitialImage DETLOG identity or forwarder installed twice");
                    }
                } else {
                    let _ = TOOL_OUTPUT_IDENTITY.set(identity);
                    let _ = detcore::detlog::set_forwarder(forward_detlog, policy);
                }
                #[cfg(not(feature = "exec-private-init"))]
                {
                    let _ = TOOL_OUTPUT_IDENTITY.set(identity);
                    let _ = detcore::detlog::set_forwarder(forward_detlog, policy);
                }
            }
            Err(error) => fail(&format!(
                "cannot reserve the DETLOG forwarding descriptor {fd}: {error}"
            )),
        }
    }
    // Detcore's exit-dependency refusals name only the capability; this
    // backend adds its name and the alternative.
    detcore::exit_dependencies::set_backend_advice(
        "in-guest LiteInst (--backend=liteinst or --backend=in-guest-trap) cannot run this \
         program; run it with --backend=ptrace.",
    );
    // Coord rulings A and D.1: Detcore checks every descriptor that arrives
    // later; this checks the ones the process image starts with. A forked
    // child holds only what its parent held or received.
    match detcore::exit_dependencies::held_exit_dependency(Path::new("/proc/self/fd")) {
        Ok(None) => {}
        Ok(Some((fd, what))) => fail(&format!(
            "in-guest LiteInst refuses a guest that starts holding {what} (descriptor {fd}): \
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
    #[cfg(feature = "exec-private-init")]
    let initial_control = match initial_image {
        Some(initial) => Some(
            unsafe {
                reverie_liteinst::install_tool_initial_image::<detcore::Detcore>(
                    Path::new(&socket),
                    initial.prepared,
                )
            }
            .unwrap_or_else(|error| fail(&describe_io_error(&error))),
        ),
        None => {
            if let Err(error) =
                unsafe { reverie_liteinst::install_tool::<detcore::Detcore>(socket) }
            {
                fail(&describe_io_error(&error));
            }
            None
        }
    };
    #[cfg(not(feature = "exec-private-init"))]
    if let Err(error) = unsafe { reverie_liteinst::install_tool::<detcore::Detcore>(socket) } {
        fail(&describe_io_error(&error));
    }
    #[cfg(feature = "exec-private-init")]
    if exec_initial::active() {
        let identity = TOOL_OUTPUT_IDENTITY
            .get()
            .copied()
            .unwrap_or_else(|| fail("InitialImage DETLOG identity is absent"));
        if reverie_liteinst::tool_output_fd().and_then(detcore::detlog::socket_identity)
            != Some(identity)
        {
            fail("InitialImage installed DETLOG socket identity changed");
        }
    }
    // `--backend=in-guest-trap`: the Tool's constructor recorded the host's
    // request from the coordinator's configuration; compare it with the
    // settings the runtime actually captured while installing, which guest
    // code cannot change afterwards, and refuse to run the program when guest
    // code that ran before this point changed the environment they were read
    // from. The program's own code has not run.
    if let Some(violation) = detcore::in_guest_site_patching::violation(
        detcore::in_guest_site_patching::site_patching_off_required(),
        reverie_liteinst::site_patching_enabled(),
        reverie_liteinst::guest_stats_enabled(),
    ) {
        fail(&violation);
    }
    // A description a fork shares is reached through the coordinator, over
    // this process's one existing connection. The first installation in a
    // process image is the only one, and a forked child inherits it.
    #[cfg(feature = "exec-private-init")]
    if exec_initial::active() {
        if detcore::install_shared_open_file_channel(Box::new(
            detcore::ChunkedOpenFileChannel::new(CoordinatorOpenFiles),
        ))
        .is_err()
        {
            fail("InitialImage shared open-file channel installed twice");
        }
    } else {
        let _ = detcore::install_shared_open_file_channel(Box::new(
            detcore::ChunkedOpenFileChannel::new(CoordinatorOpenFiles),
        ));
    }
    #[cfg(not(feature = "exec-private-init"))]
    let _ = detcore::install_shared_open_file_channel(Box::new(
        detcore::ChunkedOpenFileChannel::new(CoordinatorOpenFiles),
    ));
    #[cfg(feature = "exec-private-init")]
    if let Some(control) = initial_control {
        exec_initial::install_dispatch_observer();
        if let Err(error) = reverie_liteinst::initialize_initial_image_lifecycle(&control) {
            fail(&describe_io_error(&error));
        }
        if let Err(error) = control.finish() {
            fail(&describe_io_error(&error));
        }
    }
}

/// Observe the real constructor's retained bootstrap in diagnostic builds.
/// Samples establish placement only, not caller-byte or interior protection.
///
/// # Safety
/// `output` must point to a writable, aligned complete record when `bytes`
/// names the record's exact size. This call does not allocate or initialize.
#[cfg(feature = "allocator-fixture")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn m3_constructor_stack_query(
    output: *mut reverie_inguest::guest::tool_region::ConstructorStackRecord,
    bytes: usize,
) -> i32 {
    use reverie_inguest::guest::tool_region::ConstructorStackRecord;
    if bytes != core::mem::size_of::<ConstructorStackRecord>()
        || output.is_null()
        || !(output as usize).is_multiple_of(core::mem::align_of::<ConstructorStackRecord>())
    {
        return -1;
    }
    unsafe { reverie_inguest::guest::tool_region::reverie_inguest_constructor_stack_query(output) }
}

/// Carries shared open file control messages to Detcore's global state from
/// inside a Tool callback, through `reverie_liteinst::blocking_global_rpc`.
/// A `DetFd` method is synchronous code inside a callback's poll, so no other
/// request of this process is in flight when it runs; if one is, the call is
/// refused rather than interleaved.
struct CoordinatorOpenFiles;

impl detcore::OpenFileControlTransport for CoordinatorOpenFiles {
    fn call(
        &self,
        control: detcore::OpenFileControl,
    ) -> Result<detcore::OpenFileControlReply, detcore::SharedOpenFileError> {
        let response = reverie_liteinst::blocking_global_rpc::<detcore::GlobalState>(
            detcore::shared_open_file_request(control),
        )
        .map_err(|error| {
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
    if let Some(socket) = reverie_liteinst::tool_output_fd() {
        // Never to a descriptor that is not the passed socket; the record is
        // then counted and missing, so the run is refused.
        if TOOL_OUTPUT_IDENTITY
            .get()
            .is_none_or(|identity| detcore::detlog::socket_identity(socket) != Some(*identity))
        {
            return;
        }
        detcore::detlog::send_forwarded_record(socket, target, record_suffix, index, message);
    }
}

/// Blocks ordinary asynchronous signals and returns the previous mask.
/// The legacy constructor blocks every blockable signal. Explicit InitialImage
/// initialization keeps SIGSYS deliverable for its owned bootstrap frontend.
/// The raw rt_sigprocmask avoids an interposable pthread_sigmask.
fn block_signals() -> u64 {
    #[cfg(feature = "exec-private-init")]
    let all = if exec_initial::active() {
        !(1_u64 << (libc::SIGSYS - 1))
    } else {
        u64::MAX
    };
    #[cfg(not(feature = "exec-private-init"))]
    let all = u64::MAX;
    let mut previous = 0_u64;
    // SAFETY: the masks are valid for the call.
    #[cfg(feature = "exec-private-init")]
    {
        let result = unsafe {
            reverie_inguest::signal::raw_sigprocmask(
                libc::SIG_BLOCK,
                Some(&all),
                Some(&mut previous),
            )
        };
        if exec_initial::active()
            && (result.is_err() || previous & (1_u64 << (libc::SIGSYS - 1)) != 0)
        {
            fail(&format!(
                "InitialImage block-signals result={result:?}, previous={previous:#x}"
            ));
        }
    }
    #[cfg(not(feature = "exec-private-init"))]
    let _ = unsafe {
        reverie_inguest::signal::raw_sigprocmask(libc::SIG_BLOCK, Some(&all), Some(&mut previous))
    };
    previous
}

fn restore_signals(previous: &u64) {
    // SAFETY: the mask is valid for the call.
    #[cfg(feature = "exec-private-init")]
    {
        let result = unsafe {
            reverie_inguest::signal::raw_sigprocmask(libc::SIG_SETMASK, Some(previous), None)
        };
        if exec_initial::active() && result.is_err() {
            fail(&format!("InitialImage restore-signals result={result:?}"));
        }
    }
    #[cfg(not(feature = "exec-private-init"))]
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
            detcore_liteinst_initialize as *const libc::c_void,
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

fn fail(message: &str) -> ! {
    #[cfg(feature = "exec-private-init")]
    if exec_initial::active() {
        exec_initial::report_initialization_error(message);
        exec_initial::fail();
    }
    eprintln!("detcore-liteinst: initialization failed: {message}");
    // libc's _exit, not a raw exit_group from this library: an exit_group
    // issued from the runtime's own code bypasses the runtime's exit path, so
    // the process's site statistics would never reach the coordinator, which
    // then could not say which sites the refused image patched. The process
    // ends here, so an interposed _exit can change only a heap that is about
    // to disappear.
    // SAFETY: _exit takes no pointers and does not return.
    unsafe { libc::_exit(127) }
}
