// (c) Meta Platforms, Inc. and affiliates. Confidential and proprietary.

// RUN: %me

// FIXME(T96027871): %hermit run --strict -- %me

use nix::sys::wait::waitpid;
use nix::sys::wait::WaitStatus;
use nix::unistd::close;
use nix::unistd::fork;
use nix::unistd::pipe;
use nix::unistd::read;
use nix::unistd::write;
use nix::unistd::ForkResult;

fn main() {
    let (fdread, fdwrite) = pipe().unwrap();
    let msg: [u8; 8] = [0x1, 0x2, 0x3, 0x4, 0x5, 0x6, 0x7, 0x8];
    match unsafe { fork().unwrap() } {
        ForkResult::Parent { child, .. } => {
            assert!(close(fdwrite).is_ok());
            let mut buf: [u8; 8] = [0; 8];
            // XXX: The following SYS_read would timeout due to detcore issue.
            // add a 5 seconds timeout to abort.
            unsafe { libc::alarm(5) };
            assert_eq!(read(fdread, &mut buf), Ok(8));
            assert_eq!(buf, msg);
            assert_eq!(waitpid(child, None), Ok(WaitStatus::Exited(child, 0)));
            unsafe { libc::syscall(libc::SYS_exit_group, 0) };
        }
        ForkResult::Child => {
            assert!(close(fdread).is_ok());
            assert_eq!(write(fdwrite, &msg), Ok(8));
            unsafe { libc::syscall(libc::SYS_exit_group, 0) };
        }
    }
}
