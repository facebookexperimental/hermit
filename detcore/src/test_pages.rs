/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Pages of this process with chosen protections, for tests of copies to and
//! from guest memory. `reverie::syscalls::LocalMemory` reaches them through
//! `process_vm_readv` and `process_vm_writev`, which respect page protection
//! as a guest's copies do.

use reverie::syscalls::AddrMut;

/// The host's page size, the granularity of the pages' protections: 4 KiB on
/// x86_64, and 4, 16 or 64 KiB on aarch64.
pub(crate) fn page_size() -> usize {
    // SAFETY: sysconf has no preconditions.
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    usize::try_from(page).expect("the page size is positive")
}

/// Consecutive pages, each filled with [`Pages::FILL`] and then given its
/// protection.
pub(crate) struct Pages {
    address: *mut u8,
    count: usize,
}

impl Pages {
    /// The byte every page holds before a test writes to it.
    pub(crate) const FILL: u8 = 0xa5;

    /// Map one page for each of `protections` (`libc::PROT_*`).
    pub(crate) fn new(protections: &[libc::c_int]) -> Self {
        let count = protections.len();
        let address = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                count * page_size(),
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert_ne!(address, libc::MAP_FAILED);
        let address = address.cast::<u8>();
        unsafe { std::ptr::write_bytes(address, Self::FILL, count * page_size()) };
        for (page, &protection) in protections.iter().enumerate() {
            assert_eq!(
                unsafe {
                    libc::mprotect(
                        address.add(page * page_size()).cast(),
                        page_size(),
                        protection,
                    )
                },
                0
            );
        }
        Self { address, count }
    }

    /// The address `offset` bytes into the pages.
    pub(crate) fn address(&self, offset: usize) -> AddrMut<'static, u8> {
        assert!(offset < self.count * page_size());
        AddrMut::from_raw(self.address as usize + offset).unwrap()
    }

    /// Every byte of the pages, whatever their protection. The pages are left
    /// readable and writable.
    pub(crate) fn contents(&self) -> Vec<u8> {
        assert_eq!(
            unsafe {
                libc::mprotect(
                    self.address.cast(),
                    self.count * page_size(),
                    libc::PROT_READ | libc::PROT_WRITE,
                )
            },
            0
        );
        unsafe { std::slice::from_raw_parts(self.address, self.count * page_size()) }.to_vec()
    }
}

impl Drop for Pages {
    fn drop(&mut self) {
        assert_eq!(
            unsafe { libc::munmap(self.address.cast(), self.count * page_size()) },
            0
        );
    }
}
