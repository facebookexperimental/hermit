/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The events recording and replay both subscribe to.

use reverie::Subscription;
use reverie::syscalls::Sysno;

/// The syscalls (and `rdtsc`/`cpuid`) the recorder captures and the replayer
/// answers. One list, so a replay always subscribes to exactly the events its
/// recording holds.
pub fn record_and_replay() -> Subscription {
    let mut subscription = Subscription::none();
    subscription.rdtsc().cpuid().syscalls([
        Sysno::execve,
        Sysno::execveat,
        //Sysno::brk,
        Sysno::mprotect,
        //Sysno::arch_prctl,
        Sysno::read,
        Sysno::pread64,
        Sysno::readv,
        Sysno::preadv,
        Sysno::preadv2,
        Sysno::recvfrom,
        Sysno::recvmsg,
        Sysno::write,
        Sysno::pwrite64,
        Sysno::writev,
        Sysno::pwritev,
        Sysno::pwritev2,
        Sysno::sendfile,
        Sysno::faccessat,
        Sysno::faccessat2,
        Sysno::lseek,
        Sysno::fstat,
        Sysno::statfs,
        Sysno::fstatfs,
        Sysno::statx,
        Sysno::getdents64,
        Sysno::mmap,
        //Sysno::munmap,
        Sysno::openat,
        Sysno::close,
        Sysno::openat2,
        Sysno::mkdirat,
        Sysno::mknodat,
        Sysno::fchownat,
        Sysno::linkat,
        Sysno::renameat,
        Sysno::renameat2,
        Sysno::symlinkat,
        Sysno::fchmodat,
        Sysno::utimensat,
        Sysno::chdir,
        Sysno::fchdir,
        Sysno::getcwd,
        Sysno::close_range,
        Sysno::fadvise64,
        Sysno::flock,
        Sysno::ftruncate,
        // Descriptor syncs, metadata changes and xattr calls: replay
        // returns the recorded results, because a replayed descriptor may
        // be a placeholder, and reapplies changes inside the replay root.
        Sysno::fsync,
        Sysno::fdatasync,
        Sysno::syncfs,
        Sysno::fchmod,
        Sysno::fchown,
        Sysno::fsetxattr,
        Sysno::fremovexattr,
        Sysno::setxattr,
        Sysno::lsetxattr,
        Sysno::removexattr,
        Sysno::lremovexattr,
        Sysno::getxattr,
        Sysno::lgetxattr,
        Sysno::fgetxattr,
        Sysno::listxattr,
        Sysno::llistxattr,
        Sysno::flistxattr,
        Sysno::dup,
        Sysno::dup3,
        Sysno::ioctl,
        Sysno::socket,
        Sysno::pidfd_getfd,
        Sysno::clock_gettime,
        Sysno::clock_getres,
        Sysno::gettimeofday,
        Sysno::settimeofday,
        Sysno::setsockopt,
        Sysno::fcntl,
        Sysno::epoll_ctl,
        Sysno::connect,
        Sysno::bind,
        Sysno::listen,
        Sysno::shutdown,
        Sysno::accept,
        Sysno::accept4,
        Sysno::sendto,
        Sysno::sendmsg,
        Sysno::ppoll,
        Sysno::pselect6,
        Sysno::epoll_pwait,
        Sysno::epoll_pwait2,
        Sysno::getsockopt,
        Sysno::getpeername,
        Sysno::getsockname,
        Sysno::getrandom,
        Sysno::unlinkat,
    ]);
    // Legacy calls only the x86_64 table has.
    #[cfg(target_arch = "x86_64")]
    subscription.syscalls([
        Sysno::access,
        Sysno::stat,
        Sysno::lstat,
        Sysno::newfstatat,
        Sysno::getdents,
        Sysno::open,
        Sysno::rename,
        Sysno::link,
        Sysno::symlink,
        Sysno::chmod,
        Sysno::chown,
        Sysno::lchown,
        Sysno::mknod,
        Sysno::rmdir,
        Sysno::dup2,
        Sysno::time,
        Sysno::poll,
        Sysno::select,
        Sysno::epoll_wait,
        Sysno::readlink,
        Sysno::mkdir,
        Sysno::unlink,
    ]);
    // aarch64's name for x86_64's `newfstatat`.
    #[cfg(target_arch = "aarch64")]
    subscription.syscall(Sysno::fstatat);

    subscription
}
