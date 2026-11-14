/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::ffi::CString;
use std::ffi::c_void;
use std::os::unix::ffi::OsStrExt;
use std::sync::atomic::AtomicI32;
use std::sync::atomic::Ordering;

/// The libtest name of [`vfork_parent_resumes_after_child_exec`], which its
/// child's program runs alone.
const RESUMES_AFTER_EXEC_TEST: &str = "vfork::vfork_parent_resumes_after_child_exec";

/// Set in the environment of the program that
/// [`vfork_parent_resumes_after_child_exec`]'s child execs: there the test
/// reads stdin until it ends, as `cat` does, instead of running.
const DRAIN_STDIN_ENV: &str = "HERMIT_VFORK_DRAIN_STDIN";

extern "C" fn mark_vfork_child(arg: *mut c_void) -> libc::c_int {
    let marker = unsafe { &*arg.cast::<AtomicI32>() };
    marker.store(1, Ordering::SeqCst);
    0
}

#[test]
fn clone_vfork_parent_waits_for_child_exit() {
    super::det_test_fn_without_pmu(|| {
        let marker = AtomicI32::new(0);
        let mut stack = vec![0_u8; 64 * 1024];
        let stack_top = unsafe { stack.as_mut_ptr().add(stack.len()) }.cast::<c_void>();
        let flags = libc::CLONE_VM | libc::CLONE_VFORK | libc::SIGCHLD;

        let child = unsafe {
            libc::clone(
                mark_vfork_child,
                stack_top,
                flags,
                (&marker as *const AtomicI32).cast_mut().cast::<c_void>(),
            )
        };
        assert!(child > 0, "clone(CLONE_VFORK) failed");

        // The kernel must not return to the parent until the child has exited.
        assert_eq!(marker.load(Ordering::SeqCst), 1);

        let mut status = 0;
        assert_eq!(unsafe { libc::waitpid(child, &mut status, 0) }, child);
        assert!(libc::WIFEXITED(status));
        assert_eq!(libc::WEXITSTATUS(status), 0);
    });
}

#[test]
fn vfork_parent_resumes_after_child_exec() {
    if std::env::var_os(DRAIN_STDIN_ENV).is_some() {
        std::io::copy(&mut std::io::stdin().lock(), &mut std::io::sink()).unwrap();
        return;
    }
    super::det_test_fn_without_pmu(|| {
        // The child execs this test binary, running this test under
        // DRAIN_STDIN_ENV, not a host program such as `/bin/cat`: the run's
        // CPU profile admits an exec only of an image whose declared x86-64
        // level it covers, and a host's own programs may declare a later level
        // than this binary. Built before the vfork, because the child runs in
        // this process's memory and must not allocate.
        let exe = std::env::current_exe().unwrap();
        let args: Vec<CString> = std::iter::once(exe.as_os_str().as_bytes())
            .chain(
                [
                    RESUMES_AFTER_EXEC_TEST,
                    "--exact",
                    "--nocapture",
                    "--test-threads=1",
                ]
                .iter()
                .map(|arg| arg.as_bytes()),
            )
            .map(|arg| CString::new(arg).unwrap())
            .collect();
        let vars: Vec<CString> = std::env::vars_os()
            .map(|(name, value)| [name.as_bytes(), b"=".as_slice(), value.as_bytes()].concat())
            .chain(std::iter::once(format!("{DRAIN_STDIN_ENV}=1").into_bytes()))
            .map(|var| CString::new(var).unwrap())
            .collect();
        let pointers = |strings: &[CString]| -> Vec<*const libc::c_char> {
            strings
                .iter()
                .map(|string| string.as_ptr())
                .chain(std::iter::once(std::ptr::null()))
                .collect()
        };
        let argv = pointers(&args);
        let envp = pointers(&vars);
        // libtest reports on stdout as it starts. The child's report goes
        // nowhere, so the guest's output does not depend on how far the child
        // gets before it is killed.
        let null = unsafe { libc::open(c"/dev/null".as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC) };
        assert!(null >= 0, "open /dev/null failed");

        let mut pipe = [0; 2];
        assert_eq!(unsafe { libc::pipe(pipe.as_mut_ptr()) }, 0);

        #[allow(deprecated)]
        let child = unsafe { libc::vfork() };
        assert!(child >= 0, "vfork failed");

        if child == 0 {
            unsafe {
                libc::close(pipe[1]);
                assert_eq!(libc::dup2(pipe[0], libc::STDIN_FILENO), libc::STDIN_FILENO);
                libc::close(pipe[0]);
                assert_eq!(libc::dup2(null, libc::STDOUT_FILENO), libc::STDOUT_FILENO);

                libc::execve(argv[0], argv.as_ptr(), envp.as_ptr());
                libc::_exit(127);
            }
        }

        // The child's program would block reading the pipe after exec. The
        // parent must resume at exec so it can terminate and reap the child.
        assert_eq!(unsafe { libc::kill(child, libc::SIGKILL) }, 0);
        unsafe {
            libc::close(pipe[0]);
            libc::close(pipe[1]);
            libc::close(null);
        }

        let mut status = 0;
        assert_eq!(unsafe { libc::waitpid(child, &mut status, 0) }, child);
        assert!(libc::WIFSIGNALED(status));
        assert_eq!(libc::WTERMSIG(status), libc::SIGKILL);
    });
}
