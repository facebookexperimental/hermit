/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! A buffer for a syscall Detcore injects in a guest thread, for the kernel
//! to read or write: in the guest stack scratch, or, where that cannot hold
//! it, in a transient private page.

use reverie::Errno;
use reverie::Guest;
use reverie::Stack;
use reverie::syscalls;
use reverie::syscalls::AddrMut;
use reverie::syscalls::MapFlags;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::ProtFlags;
use reverie::syscalls::Syscall;
use tracing::info;
use tracing::warn;

use crate::record_or_replay::RecordOrReplay;
use crate::tool_local::Detcore;

/// Inject the syscall that `call` makes for a buffer of `N` bytes holding
/// `initial`, and return its result with what the buffer holds after it.
///
/// The buffer is staged as `Detcore::inject_fstat` stages its own. It goes
/// first in the guest stack scratch, which on the ptrace backend lies below
/// the red zone under the guest's stack pointer; the bytes it covers are
/// saved and put back after the call. That memory is not always writable: a
/// guest may run with its stack pointer just above a guard page (a thread,
/// fiber or alternate signal stack), or within a few hundred bytes of the
/// lowest page of the main-thread stack, which a tracer's write does not
/// grow (<https://github.com/rrnewton/hermit/issues/3328>). The fault then
/// belongs to Detcore's bookkeeping, not to the guest, so where the scratch
/// cannot hold the buffer, and where the call fails with EFAULT there, the
/// call is repeated with the buffer in a transient private page that is
/// unmapped before the guest resumes.
pub(crate) async fn inject_with_scratch<G, T, const N: usize>(
    guest: &mut G,
    initial: [u8; N],
    call: impl Fn(AddrMut<'_, u8>) -> Syscall,
) -> Result<(i64, [u8; N]), Errno>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    match on_stack(guest, &initial, &call).await {
        Err(Errno::EFAULT) => {
            info!(
                "Guest stack scratch cannot hold an injected syscall's {N}-byte buffer; using a \
                 transient page instead."
            );
            in_transient_page(guest, &initial, &call).await
        }
        result => result,
    }
}

/// [`inject_with_scratch`] with the buffer in the guest stack scratch.
/// `Err(EFAULT)` when the scratch cannot hold it.
async fn on_stack<G, T, const N: usize>(
    guest: &mut G,
    initial: &[u8; N],
    call: &impl Fn(AddrMut<'_, u8>) -> Syscall,
) -> Result<(i64, [u8; N]), Errno>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let mut stack = guest.stack().await;
    let buffer = stack.reserve::<[u8; N]>().cast::<u8>();
    let mut saved = [0_u8; N];
    guest.memory().read_exact(buffer, &mut saved)?;
    let guard = match stack.commit() {
        Ok(guard) => guard,
        Err(errno) => {
            let _ = guest.memory().write_exact(buffer, &saved);
            return Err(errno);
        }
    };
    let outcome = run(guest, buffer, initial, call).await;
    let restored = guest.memory().write_exact(buffer, &saved);
    drop(guard);
    let outcome = outcome?;
    restored?;
    Ok(outcome)
}

/// [`inject_with_scratch`] with the buffer in a transient private page.
async fn in_transient_page<G, T, const N: usize>(
    guest: &mut G,
    initial: &[u8; N],
    call: &impl Fn(AddrMut<'_, u8>) -> Syscall,
) -> Result<(i64, [u8; N]), Errno>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let mapped = guest
        .inject_with_retry(Syscall::Mmap(
            syscalls::Mmap::new()
                .with_addr(None)
                .with_len(N)
                .with_prot(ProtFlags::PROT_READ | ProtFlags::PROT_WRITE)
                .with_flags(MapFlags::MAP_PRIVATE | MapFlags::MAP_ANONYMOUS)
                .with_fd(-1)
                .with_offset(0),
        ))
        .await?;
    let page = usize::try_from(mapped)
        .ok()
        .and_then(AddrMut::<u8>::from_raw)
        .unwrap_or_else(|| panic!("transient scratch page mmap returned {mapped}"));
    let outcome = run(guest, page, initial, call).await;
    if let Err(errno) = guest
        .inject_with_retry(Syscall::Munmap(
            syscalls::Munmap::new()
                .with_addr(Some(page.cast::<libc::c_void>().into()))
                .with_len(N),
        ))
        .await
    {
        // Not expected: the page was mapped by this call and its address
        // never reached the guest.
        warn!("[detcore] could not unmap the transient scratch page: {errno}");
    }
    outcome
}

/// Write `initial` at `buffer`, inject `call(buffer)`, and read the buffer
/// back.
async fn run<G, T, const N: usize>(
    guest: &mut G,
    buffer: AddrMut<'_, u8>,
    initial: &[u8; N],
    call: &impl Fn(AddrMut<'_, u8>) -> Syscall,
) -> Result<(i64, [u8; N]), Errno>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    guest.memory().write_exact(buffer, initial)?;
    let result = guest.inject_with_retry(call(buffer)).await?;
    let mut bytes = [0_u8; N];
    guest.memory().read_exact(buffer, &mut bytes)?;
    Ok((result, bytes))
}
