/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Host-side per-physical-run filesystem isolation inside the pinned root.
//!
//! Callers must create their tracer runtime inside the callback and finish its
//! cleanup before returning. A pre-existing executor could spawn the guest in
//! its original namespace. This helper does not change the calling thread's
//! namespace, cwd, or filesystem sharing, and leaves /tmp transport visible.

use std::ffi::OsStr;
use std::io;
use std::path::Path;

/// The pinned-root runner requests a fresh /test for each physical run.
pub const REQUEST_ENV: &str = "HERMIT_E2E_EMPTY_WORKDIR";
/// The pre-existing mountpoint supplied by the pinned-root container.
pub const WORKDIR: &str = "/test";

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
/// This requires CAP_SYS_ADMIN in the owning user namespace and an existing
/// /test directory. No user-namespace fallback or shared-directory substitute
/// is used. Setup errors return before the callback can launch a guest. The
/// scoped thread is joined on success, error and panic; a callback panic keeps
/// its original payload. Each invocation creates its own mount namespace.
pub fn with_isolated_workdir<F, T>(run: F) -> io::Result<T>
where
    F: FnOnce() -> T + Send,
    T: Send,
{
    std::thread::scope(|scope| {
        let thread = std::thread::Builder::new()
            .name("hermit-test-workdir".into())
            .spawn_scoped(scope, move || {
                enter_namespace()?;
                Ok(run())
            })?;
        match thread.join() {
            Ok(result) => result,
            Err(panic) => std::panic::resume_unwind(panic),
        }
    })
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

fn enter_namespace() -> io::Result<()> {
    // A new thread is essential: unshare also separates its fs_struct, which
    // setns could not restore to the caller's original CLONE_FS sharing.
    syscall_result(
        unsafe { libc::unshare(libc::CLONE_NEWNS) },
        "unshare mount namespace",
    )?;
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
    syscall_result(
        unsafe {
            libc::mount(
                c"tmpfs".as_ptr(),
                c"/test".as_ptr(),
                c"tmpfs".as_ptr(),
                libc::MS_NOSUID | libc::MS_NODEV,
                c"mode=1777".as_ptr().cast(),
            )
        },
        "mount per-run /test tmpfs",
    )
}

#[cfg(test)]
mod tests {
    use std::os::unix::ffi::OsStrExt;

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
}
