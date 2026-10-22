/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Syscall-argument decoding that both recording and replay read the same way.

/// The iovecs a guest `msghdr` points at, read from guest MEMORY.
pub fn read_iovecs<M: reverie::syscalls::MemoryAccess>(
    memory: &M,
    message: &libc::msghdr,
) -> Result<Vec<libc::iovec>, reverie::Errno> {
    if message.msg_iovlen == 0 {
        return Ok(Vec::new());
    }
    let address = reverie::syscalls::Addr::from_raw(message.msg_iov as usize)
        .ok_or(reverie::Errno::EFAULT)?;
    let mut iovecs = vec![
        libc::iovec {
            iov_base: std::ptr::null_mut(),
            iov_len: 0,
        };
        message.msg_iovlen
    ];
    memory.read_values(address, &mut iovecs)?;
    Ok(iovecs)
}

/// The file offset of a vectored positional syscall (`preadv`, `pwritev`, ...)
/// from its split LOW and HIGH arguments.
pub fn vectored_offset(low: u64, high: u64) -> i64 {
    if std::mem::size_of::<usize>() == 8 {
        low as i64
    } else {
        ((high << 32) | (low & u32::MAX as u64)) as i64
    }
}
