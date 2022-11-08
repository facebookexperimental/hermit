// (c) Meta Platforms, Inc. and affiliates. Confidential and proprietary.

// RUN: %me

use nix::sys::wait::waitpid;
use nix::sys::wait::WaitStatus;
use nix::unistd::alarm;
use nix::unistd::close;
use nix::unistd::dup2;
use nix::unistd::fork;
use nix::unistd::pipe;
use nix::unistd::read;
use nix::unistd::write;
use nix::unistd::ForkResult;

fn main() {
    let (fdread, fdwrite) = pipe().unwrap();

    let msg = [0x1u8, 0x2, 0x3, 0x4, 0x5, 0x6, 0x7, 0x8];

    match unsafe { fork().unwrap() } {
        ForkResult::Parent { child, .. } => {
            assert!(close(fdwrite).is_ok());
            let mut buf = [0u8; 8];

            // If the file descriptor newfd was previously open,
            // it is silently closed before being reused.
            assert!(dup2(fdread, libc::STDIN_FILENO).is_ok());

            // FIXME: The following SYS_read will timeout due to detcore
            // scheduler issue. Abort if we're still going in 5 seconds.
            alarm::set(5);

            assert_eq!(read(libc::STDIN_FILENO, &mut buf), Ok(msg.len()));
            assert_eq!(buf, msg);

            alarm::cancel();

            assert_eq!(waitpid(child, None), Ok(WaitStatus::Exited(child, 0)));
        }
        ForkResult::Child => {
            assert!(close(fdread).is_ok());
            assert_eq!(write(fdwrite, &msg), Ok(msg.len()));
        }
    }
}
