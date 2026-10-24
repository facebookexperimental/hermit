/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Host-side per-physical-run filesystem and network isolation.
//!
//! Callers must create their tracer runtime inside the callback and finish its
//! cleanup before returning. A pre-existing executor could spawn the guest in
//! its original namespace. This helper does not change the calling thread's
//! namespace, cwd, or filesystem sharing. It leaves /tmp transport visible
//! unless the run binds paths into a private /tmp ([`Isolation::binds`]); then
//! only the paths in [`Isolation::preserve`] stay reachable there.

use std::ffi::CString;
use std::ffi::OsStr;
use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;

/// The pinned-root runner requests a fresh /test for each physical run.
pub const REQUEST_ENV: &str = "HERMIT_E2E_EMPTY_WORKDIR";
/// The pre-existing mountpoint supplied by the pinned-root container.
pub const WORKDIR: &str = "/test";
/// The directory a run with binds gets as a fresh tmpfs.
pub const TMP_DIR: &str = "/tmp";

/// One host path bound into the run's private /tmp, as `hermit run --bind`
/// binds it for the other backends: `source` must exist, and `target` is an
/// absolute path below /tmp, created as a file or a directory to match.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BindMount {
    pub source: PathBuf,
    pub target: PathBuf,
}

/// The filesystem and network isolation one physical run gets.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Isolation {
    /// Create a fresh network namespace with loopback up and a fresh /sys.
    /// False leaves the caller's network namespace in place.
    pub local_networking: bool,
    /// Mount a fresh tmpfs on the pre-existing [`WORKDIR`].
    pub test_workdir: bool,
    /// Mount a fresh tmpfs on /tmp and bind each source at its target there,
    /// in order. Empty leaves /tmp as it is.
    pub binds: Vec<BindMount>,
    /// Paths below the original /tmp, made before the run, that the run must
    /// still reach at the same path once /tmp is replaced: bound at that path
    /// before the binds. Ignored when there are no binds.
    pub preserve: Vec<PathBuf>,
}

impl Isolation {
    /// Whether the run needs neither filesystem nor network isolation.
    pub fn is_empty(&self) -> bool {
        !self.local_networking && !self.test_workdir && self.binds.is_empty()
    }
}

/// Refuse malformed requests instead of silently running without isolation.
pub fn requested_workdir(value: Option<&OsStr>) -> io::Result<Option<&'static Path>> {
    match value {
        None => Ok(None),
        Some(value) if value == OsStr::new(WORKDIR) => Ok(Some(Path::new(WORKDIR))),
        Some(value) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{REQUEST_ENV} must be {WORKDIR}, got {value:?}"),
        )),
    }
}

/// Run on a new host thread with a fresh tmpfs mounted at /test.
///
/// This requires CAP_SYS_ADMIN in the owning user namespace (the pinned root
/// grants it; [`enter_root_user_namespace`] gives it to an unprivileged
/// process) and an existing /test directory. No user-namespace fallback or
/// shared-directory substitute is used here. Setup errors return before the callback can launch a guest. The
/// scoped thread is joined on success, error and panic; a callback panic keeps
/// its original payload. Each invocation creates its own mount namespace.
pub fn with_isolated_workdir<F, T>(run: F) -> io::Result<T>
where
    F: FnOnce() -> T + Send,
    T: Send,
{
    with_isolation(
        &Isolation {
            test_workdir: true,
            ..Isolation::default()
        },
        run,
    )
}

/// Run on a new host thread in a new mount namespace with the requested mounts
/// and, for local networking, a new network namespace with only loopback.
///
/// The same contract as [`with_isolated_workdir`]: CAP_SYS_ADMIN is required,
/// every setup error returns before the callback can launch a guest, and the
/// mounts disappear with the namespace. Local networking also needs
/// CAP_NET_ADMIN to bring loopback up; failure never falls back to host networking.
/// A bind target must be a normal absolute path strictly below /tmp; a bind
/// source and a preserved path are resolved before /tmp is replaced, so they
/// may lie below the original /tmp.
pub fn with_isolation<F, T>(isolation: &Isolation, run: F) -> io::Result<T>
where
    F: FnOnce() -> T + Send,
    T: Send,
{
    for bind in &isolation.binds {
        check_bind_target(&bind.target)?;
    }
    std::thread::scope(|scope| {
        let thread = std::thread::Builder::new()
            .name("hermit-test-workdir".into())
            .spawn_scoped(scope, move || {
                enter_namespace(isolation)?;
                Ok(run())
            })?;
        match thread.join() {
            Ok(result) => result,
            Err(panic) => std::panic::resume_unwind(panic),
        }
    })
}

/// Whether the calling process holds CAP_SYS_ADMIN (in its effective set), which
/// [`with_isolation`]'s mount namespace needs.
pub fn has_cap_sys_admin() -> io::Result<bool> {
    const CAP_SYS_ADMIN: u32 = 21;
    let status = std::fs::read_to_string("/proc/self/status")?;
    let effective = status
        .lines()
        .find_map(|line| line.strip_prefix("CapEff:"))
        .and_then(|hex| u64::from_str_radix(hex.trim(), 16).ok())
        .ok_or_else(|| io::Error::other("/proc/self/status has no readable CapEff line"))?;
    Ok(effective & (1 << CAP_SYS_ADMIN) != 0)
}

/// Make the calling process root in a new user namespace that maps root to the
/// caller's own user and group, exactly as `reverie::process::Container::map_root`
/// maps the guest of every other backend. The process then holds CAP_SYS_ADMIN
/// over the mount namespaces it creates, so [`with_isolation`] works on a host
/// that grants no privilege, and a guest sees the same identity as under the
/// other backends.
///
/// Linux refuses to move a multithreaded process into a new user namespace, so
/// this must run before the process starts its first thread; it fails, with
/// the thread count, instead of leaving the process where it was.
pub fn enter_root_user_namespace() -> io::Result<()> {
    let threads = std::fs::read_dir("/proc/self/task")?.count();
    if threads != 1 {
        return Err(io::Error::other(format!(
            "a new user namespace needs a single-threaded process; this one has {threads} threads"
        )));
    }
    let uid = unsafe { libc::geteuid() };
    let gid = unsafe { libc::getegid() };
    syscall_result(
        unsafe { libc::unshare(libc::CLONE_NEWUSER) },
        "unshare user namespace",
    )?;
    let write = |path: &str, contents: String| {
        std::fs::write(path, contents)
            .map_err(|error| io::Error::new(error.kind(), format!("write {path}: {error}")))
    };
    write("/proc/self/uid_map", format!("0 {uid} 1"))?;
    write("/proc/self/setgroups", "deny".to_string())?;
    write("/proc/self/gid_map", format!("0 {gid} 1"))
}

fn syscall_result(result: libc::c_int, operation: &str) -> io::Result<()> {
    if result == 0 {
        Ok(())
    } else {
        let error = io::Error::last_os_error();
        Err(io::Error::new(
            error.kind(),
            format!("{operation}: {error}"),
        ))
    }
}

/// Refuse a bind target the private /tmp cannot hold: relative, outside /tmp,
/// /tmp itself, or with `.`/`..` components that could leave it.
pub fn check_bind_target(target: &Path) -> io::Result<()> {
    let mut components = target.components();
    let below_tmp = components.next() == Some(Component::RootDir)
        && components.next() == Some(Component::Normal(OsStr::new("tmp")))
        && target.components().count() > 2
        && components.all(|component| matches!(component, Component::Normal(_)));
    if below_tmp {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "bind target {} must be a normal absolute path below {TMP_DIR}",
                target.display()
            ),
        ))
    }
}

fn path_cstring(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("path {} contains a NUL byte", path.display()),
        )
    })
}

/// A path opened before /tmp is replaced: `O_PATH` keeps the original file
/// reachable through `/proc/thread-self/fd` after its path is covered.
struct OpenedSource {
    file: File,
    is_dir: bool,
    target: PathBuf,
}

fn open_source(source: &Path, target: &Path) -> io::Result<OpenedSource> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_PATH)
        .open(source)
        .map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("open bind source {}: {error}", source.display()),
            )
        })?;
    let is_dir = file.metadata()?.is_dir();
    Ok(OpenedSource {
        file,
        is_dir,
        target: target.to_path_buf(),
    })
}

fn mount_tmpfs(target: &std::ffi::CStr, what: &str) -> io::Result<()> {
    syscall_result(
        unsafe {
            libc::mount(
                c"tmpfs".as_ptr(),
                target.as_ptr(),
                c"tmpfs".as_ptr(),
                libc::MS_NOSUID | libc::MS_NODEV,
                c"mode=1777".as_ptr().cast(),
            )
        },
        what,
    )
}

fn bind_opened(source: &OpenedSource) -> io::Result<()> {
    let target = &source.target;
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let created = if source.is_dir {
        std::fs::create_dir(target)
    } else {
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(target)
            .map(drop)
    };
    match created {
        Err(error) if error.kind() != io::ErrorKind::AlreadyExists => {
            return Err(io::Error::new(
                error.kind(),
                format!("create bind target {}: {error}", target.display()),
            ));
        }
        _ => {}
    }
    let from = CString::new(format!("/proc/thread-self/fd/{}", source.file.as_raw_fd()))
        .expect("a descriptor path has no NUL");
    let to = path_cstring(target)?;
    syscall_result(
        unsafe {
            libc::mount(
                from.as_ptr(),
                to.as_ptr(),
                std::ptr::null(),
                libc::MS_BIND | libc::MS_REC,
                std::ptr::null(),
            )
        },
        &format!("bind {}", target.display()),
    )
}

/// Open the setup socket only after unsharing, so its ioctls address the new
/// network namespace. CLOEXEC and OwnedFd keep it out of the launched guest.
fn bring_loopback_up() -> io::Result<()> {
    let socket = unsafe {
        libc::socket(
            libc::AF_INET,
            libc::SOCK_DGRAM | libc::SOCK_CLOEXEC,
            libc::IPPROTO_IP,
        )
    };
    if socket < 0 {
        let error = io::Error::last_os_error();
        return Err(io::Error::new(
            error.kind(),
            format!("open loopback setup socket: {error}"),
        ));
    }
    // SAFETY: socket is a newly opened descriptor owned by this function.
    let socket = unsafe { OwnedFd::from_raw_fd(socket) };
    // SAFETY: an all-zero ifreq is valid; the short interface name stays NUL-terminated.
    let mut interface: libc::ifreq = unsafe { std::mem::zeroed() };
    interface.ifr_name[0] = b'l' as libc::c_char;
    interface.ifr_name[1] = b'o' as libc::c_char;
    syscall_result(
        unsafe { libc::ioctl(socket.as_raw_fd(), libc::SIOCGIFFLAGS, &mut interface) },
        "read loopback interface flags",
    )?;
    // SAFETY: SIOCGIFFLAGS initialized this union member on success.
    interface.ifr_ifru.ifru_flags =
        unsafe { interface.ifr_ifru.ifru_flags } | libc::IFF_UP as libc::c_short;
    syscall_result(
        unsafe { libc::ioctl(socket.as_raw_fd(), libc::SIOCSIFFLAGS, &interface) },
        "bring loopback interface up",
    )
}

fn enter_namespace(isolation: &Isolation) -> io::Result<()> {
    // A new thread is essential: unshare also separates its fs_struct, which
    // setns could not restore to the caller's original CLONE_FS sharing.
    syscall_result(
        unsafe { libc::unshare(libc::CLONE_NEWNS) },
        "unshare mount namespace",
    )?;
    if isolation.local_networking {
        syscall_result(
            unsafe { libc::unshare(libc::CLONE_NEWNET) },
            "unshare network namespace",
        )?;
    }
    syscall_result(
        unsafe {
            libc::mount(
                std::ptr::null(),
                c"/".as_ptr(),
                std::ptr::null(),
                libc::MS_REC | libc::MS_PRIVATE,
                std::ptr::null(),
            )
        },
        "make mount propagation private",
    )?;
    if isolation.local_networking {
        syscall_result(
            unsafe {
                libc::mount(
                    std::ptr::null(),
                    c"/sys".as_ptr(),
                    c"sysfs".as_ptr(),
                    0,
                    std::ptr::null(),
                )
            },
            "mount per-run network sysfs",
        )?;
        bring_loopback_up()?;
    }
    if isolation.test_workdir {
        mount_tmpfs(c"/test", "mount per-run /test tmpfs")?;
    }
    if isolation.binds.is_empty() {
        return Ok(());
    }
    // Everything is opened before the tmpfs covers /tmp, so a source or a
    // preserved path below the original /tmp is still reachable.
    let mut opened = Vec::new();
    for path in &isolation.preserve {
        if path.starts_with(TMP_DIR) {
            opened.push(open_source(path, path)?);
        }
    }
    for bind in &isolation.binds {
        opened.push(open_source(&bind.source, &bind.target)?);
    }
    mount_tmpfs(c"/tmp", "mount per-run /tmp tmpfs")?;
    opened.iter().try_for_each(bind_opened)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_is_exact_and_fail_closed() {
        assert_eq!(requested_workdir(None).unwrap(), None);
        assert_eq!(
            requested_workdir(Some(OsStr::new("/test"))).unwrap(),
            Some(Path::new("/test"))
        );
        for value in [b"".as_slice(), b"/tmp", b"/test/", b"/test\0", b"/test\xff"] {
            let error = requested_workdir(Some(OsStr::from_bytes(value))).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
            assert!(
                error
                    .to_string()
                    .contains("HERMIT_E2E_EMPTY_WORKDIR must be /test")
            );
        }
    }

    #[test]
    fn bind_targets_must_stay_strictly_below_tmp() {
        // `Path::components` drops an interior `.`, so `/tmp/./test` is `/tmp/test`.
        for target in ["/tmp/test", "/tmp/e2e/home", "/tmp/a/b/c", "/tmp/./test"] {
            check_bind_target(Path::new(target)).unwrap();
        }
        for target in [
            "",
            "tmp/test",
            "/tmp",
            "/tmp/",
            "/test",
            "/tmpx/test",
            "/tmp/../etc",
            "/tmp/a/../../etc",
            "/var/tmp/test",
        ] {
            let error = check_bind_target(Path::new(target)).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput, "{target}");
            assert!(
                error.to_string().contains("below /tmp"),
                "{target}: {error}"
            );
        }
    }

    #[test]
    fn an_empty_isolation_needs_no_namespace() {
        assert!(Isolation::default().is_empty());
        let bound = Isolation {
            binds: vec![BindMount {
                source: "/".into(),
                target: "/tmp/test".into(),
            }],
            ..Isolation::default()
        };
        assert!(!bound.is_empty());
        let workdir = Isolation {
            test_workdir: true,
            ..Isolation::default()
        };
        assert!(!workdir.is_empty());
        let local_networking = Isolation {
            local_networking: true,
            ..Isolation::default()
        };
        assert!(!local_networking.is_empty());
    }

    #[test]
    fn a_bad_bind_target_is_refused_before_any_namespace_or_callback() {
        // This would need CAP_SYS_ADMIN to get further, so it also shows that
        // the target check runs first: the error is the target's, not EPERM.
        let launched = std::sync::atomic::AtomicBool::new(false);
        let isolation = Isolation {
            binds: vec![BindMount {
                source: "/".into(),
                target: "/etc/passwd".into(),
            }],
            ..Isolation::default()
        };
        let error = with_isolation(&isolation, || {
            launched.store(true, std::sync::atomic::Ordering::SeqCst)
        })
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(!launched.load(std::sync::atomic::Ordering::SeqCst));
    }
}
