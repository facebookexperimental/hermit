/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The host's page size, which Linux applies to every page-granular rule a
//! guest can observe: alignment, rounding and page counts. It is 4 KiB on
//! x86_64 and 4, 16 or 64 KiB on aarch64, fixed when the kernel is built.

use std::sync::OnceLock;

/// The host's page size in bytes, `sysconf(_SC_PAGESIZE)`, read once. libc
/// answers from the kernel's `AT_PAGESZ` without a syscall, so in-guest
/// Detcore may call it too.
pub(crate) fn page_size() -> u64 {
    static PAGE_SIZE: OnceLock<u64> = OnceLock::new();
    *PAGE_SIZE.get_or_init(|| {
        // SAFETY: sysconf has no preconditions.
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        u64::try_from(page)
            .ok()
            .filter(|page| page.is_power_of_two())
            .expect("the page size is a power of two")
    })
}

/// Linux's `MAX_RW_COUNT`, `INT_MAX & PAGE_MASK` (include/linux/fs.h): the
/// most one read, write or vectored transfer moves, and the length a lone
/// iovec or a getrandom request is shortened to.
pub(crate) fn max_rw_count() -> usize {
    let page = usize::try_from(page_size()).expect("the page size fits in usize");
    (i32::MAX as usize) & !(page - 1)
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_page_size_is_the_kernels() {
        let page = super::page_size();
        assert!(page.is_power_of_two() && page >= 4096);
        #[cfg(target_arch = "x86_64")]
        assert_eq!(page, 4096);
    }

    #[test]
    fn max_rw_count_is_int_max_rounded_down_to_a_page() {
        let count = super::max_rw_count();
        assert_eq!(count % super::page_size() as usize, 0);
        assert!(i32::MAX as usize - count < super::page_size() as usize);
        #[cfg(target_arch = "x86_64")]
        assert_eq!(count, 0x7fff_f000);
    }
}
