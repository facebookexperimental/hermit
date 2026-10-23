/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! A guest's `siginfo_t`, in a form a syscall handler can hold across
//! `.await`s.
//!
//! Since libc 0.2.190 the Linux `siginfo_t` holds raw pointers
//! (`si_addr`, `si_ptr`, `si_call_addr`, ...) and is neither `Send` nor
//! `Sync`. Neither is an `AddrMut<siginfo_t>` naming one, because Reverie's
//! address types are `Send` only when their pointee is. A handler that keeps
//! either alive across an `.await` makes its future non-`Send`, which the
//! tool's `handle_syscall_event` must be. Detcore copies these values between
//! guest memory and the kernel and never dereferences their pointer fields, so
//! the value is plain data. Read and write it as [`GuestSiginfo`], casting the
//! guest address with `AddrMut::cast`.

/// A guest `siginfo_t` as plain data; see the module documentation.
#[derive(Clone, Copy)]
#[repr(transparent)]
pub(crate) struct GuestSiginfo(pub(crate) libc::siginfo_t);

// SAFETY: the raw-pointer fields are guest addresses, never dereferenced.
unsafe impl Send for GuestSiginfo {}
// SAFETY: as for `Send`; shared access only reads plain fields.
unsafe impl Sync for GuestSiginfo {}

impl GuestSiginfo {
    /// The all-zero value: siginfo_t has no portable initializer, and waitid
    /// leaves all-zero in `infop` when WNOHANG finds no child (POSIX and
    /// Linux).
    pub(crate) fn zeroed() -> Self {
        Self(unsafe { std::mem::zeroed() })
    }

    /// `si_pid`, as a waitid or SIGCHLD record carries it.
    pub(crate) fn pid(&self) -> libc::pid_t {
        unsafe { self.0.si_pid() }
    }

    /// `si_code`.
    pub(crate) fn code(&self) -> libc::c_int {
        self.0.si_code
    }
}

// The wrapper keeps a handler future `Send` on every libc only if it is
// `Send` and `Sync` even where `libc::siginfo_t` is not, an address typed by
// it is `Send`, and it has `siginfo_t`'s layout so a guest address can be
// cast to it. Checked at compile time.
const _: () = {
    fn assert_send<T: Send>() {}
    fn assert_send_sync<T: Send + Sync>() {}
    let _ = assert_send_sync::<GuestSiginfo>;
    let _ = assert_send::<reverie::syscalls::AddrMut<'static, GuestSiginfo>>;
    assert!(std::mem::size_of::<GuestSiginfo>() == std::mem::size_of::<libc::siginfo_t>());
    assert!(std::mem::align_of::<GuestSiginfo>() == std::mem::align_of::<libc::siginfo_t>());
};
