use std::ffi::c_void;
use std::sync::atomic::AtomicI32;
use std::sync::atomic::Ordering;

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
    super::det_test_fn_without_pmu(|| {
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

                let path = c"/bin/cat";
                let argv = [path.as_ptr(), std::ptr::null()];
                let envp = [std::ptr::null()];
                libc::execve(path.as_ptr(), argv.as_ptr(), envp.as_ptr());
                libc::_exit(127);
            }
        }

        // cat would block reading the pipe after exec. The parent must resume at
        // exec so it can terminate and reap the child.
        assert_eq!(unsafe { libc::kill(child, libc::SIGKILL) }, 0);
        unsafe {
            libc::close(pipe[0]);
            libc::close(pipe[1]);
        }

        let mut status = 0;
        assert_eq!(unsafe { libc::waitpid(child, &mut status, 0) }, child);
        assert!(libc::WIFSIGNALED(status));
        assert_eq!(libc::WTERMSIG(status), libc::SIGKILL);
    });
}
