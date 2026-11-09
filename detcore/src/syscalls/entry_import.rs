/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The guest memory a vectored call copies once, when it begins.
//!
//! Linux copies a vectored call's iovec array into the kernel as the call
//! begins (`import_iovec`, called by `vfs_readv` and `vfs_writev`), and a
//! `sendmsg` or `recvmsg` copies its `msghdr` first (`copy_msghdr_from_user`);
//! `sendmsg` also copies its address (`move_addr_to_kernel`) and its control
//! data (`____sys_sendmsg`) before it sends anything, as `sendto` copies its
//! address (`__sys_sendto`). A call that then waits
//! moves its bytes through the buffers that copy names, whatever another
//! thread writes to the array, the header, the address or the control data
//! meanwhile.
//!
//! Detcore runs a blocking call on a physically nonblocking descriptor as a
//! series of nonblocking attempts, each of which the kernel copies again. An
//! [`EntryImport`] is that memory as the call began. Before each attempt,
//! [`EntryImport::reinstate`] writes it back where another thread changed it,
//! and [`EntryImport::put_back`] then restores what that thread wrote, keeping
//! whatever the attempt itself wrote. Detcore runs one guest thread at a time,
//! so no guest thread runs between the two and none can see the copy.

use reverie::Errno;
use reverie::syscalls::Addr;
use reverie::syscalls::AddrMut;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::Syscall;
use reverie::syscalls::linux_abi::Msghdr;

/// `UIO_MAXIOV`: a longer iovec array fails before Linux copies it, with
/// `EINVAL` (`readv`, `writev`) or `EMSGSIZE` (`sendmsg`, `recvmsg`).
const MAX_IOVECS: usize = libc::UIO_MAXIOV as usize;

/// `sizeof(struct sockaddr_storage)`: Linux copies at most this much of a
/// `sendmsg` address (`__copy_msghdr` truncates a longer `msg_namelen`).
const MAX_NAME: usize = 128;

/// Control data longer than this is not copied here. Linux allocates the
/// control buffer from the socket's option memory, which `net.core.optmem_max`
/// bounds (128 KiB by default), so while that limit is at most this size a
/// longer one fails with `ENOBUFS` before the call can wait, and no later
/// attempt follows. Above it, such a call's control data is copied again at
/// each attempt, as before.
const MAX_CONTROL: usize = 256 * 1024;

/// One run of guest bytes: where it is and what it held.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Region {
    address: usize,
    bytes: Vec<u8>,
}

/// The memory a vectored call copied when it began, taken by
/// [`EntryImport::capture`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct EntryImport {
    /// The call, as the guest made it.
    call: Syscall,
    regions: Vec<Region>,
    /// The `msghdr` fields a completed `recvmsg` writes back
    /// (`____sys_recvmsg`): `msg_namelen` when the call named an address,
    /// `msg_flags` and `msg_controllen`. As `(start, end)` guest addresses.
    written_on_completion: Vec<(usize, usize)>,
    /// Complete geometry read immediately before the successful generic read
    /// attempt. An explicit error cannot reuse an earlier snapshot. Retry and
    /// exit-dependency readers continue to use the unchanged `regions`.
    observation: Option<Result<Vec<(usize, usize)>, Errno>>,
}

/// What another thread wrote over an [`EntryImport`]'s regions, held between
/// [`EntryImport::reinstate`] and [`EntryImport::put_back`].
#[derive(Debug, Default)]
#[must_use = "put back what another thread wrote with EntryImport::put_back"]
pub(crate) struct Displaced {
    /// The index of the region in [`EntryImport::regions`] and the bytes
    /// another thread left there.
    regions: Vec<(usize, Vec<u8>)>,
}

impl Displaced {
    /// Whether another thread had changed none of the call's memory.
    pub(crate) fn is_empty(&self) -> bool {
        self.regions.is_empty()
    }
}

fn read_region<M: MemoryAccess>(memory: &M, address: usize, len: usize) -> Option<Region> {
    let at = Addr::<u8>::from_raw(address)?;
    let mut bytes = vec![0; len];
    memory.read_exact(at, &mut bytes).ok()?;
    Some(Region { address, bytes })
}

/// The iovec array of `count` entries at `address`, if Linux would copy it.
fn iovec_array<M: MemoryAccess>(memory: &M, address: usize, count: usize) -> Option<Region> {
    if count == 0 || count > MAX_IOVECS {
        return None;
    }
    read_region(memory, address, count * size_of::<libc::iovec>())
}

/// A field of the `msghdr` at `header` as a `(start, end)` guest range.
fn field(header: usize, offset: usize, len: usize) -> (usize, usize) {
    (header + offset, header + offset + len)
}

impl EntryImport {
    /// The memory `call` copies when it begins, read now: the iovec array of
    /// a `readv` or `writev`; the `msghdr` and its iovec array of a `recvmsg`;
    /// those, the address and the control data of a `sendmsg`; and the
    /// address of a `sendto`. A part
    /// Linux would not copy (a NULL or overlong array, an absent address) or
    /// that cannot be read now is left out, as the kernel then fails the call
    /// before it waits. `None` for any other call, or when nothing is left.
    pub(crate) fn capture<M: MemoryAccess>(memory: &M, call: &Syscall) -> Option<Self> {
        let mut regions = Vec::new();
        let mut written_on_completion = Vec::new();
        match call {
            Syscall::Readv(call) => {
                regions.extend(iovec_array(memory, call.iov()?.as_raw(), call.len()));
            }
            Syscall::Writev(call) => {
                regions.extend(iovec_array(memory, call.iov()?.as_raw(), call.len()));
            }
            Syscall::Recvmsg(call) => {
                let header = call.msg()?;
                let message: Msghdr = memory.read_value(header).ok()?;
                let header = header.as_raw();
                regions.extend(read_region(memory, header, size_of::<Msghdr>()));
                regions.extend(iovec_array(
                    memory,
                    message.msg_iov as usize,
                    message.msg_iovlen,
                ));
                if !message.msg_name.is_null() {
                    written_on_completion.push(field(
                        header,
                        std::mem::offset_of!(Msghdr, msg_namelen),
                        size_of::<libc::socklen_t>(),
                    ));
                }
                written_on_completion.push(field(
                    header,
                    std::mem::offset_of!(Msghdr, msg_controllen),
                    size_of::<usize>(),
                ));
                written_on_completion.push(field(
                    header,
                    std::mem::offset_of!(Msghdr, msg_flags),
                    size_of::<libc::c_int>(),
                ));
            }
            Syscall::Sendmsg(call) => {
                let header = call.msg()?;
                let message: Msghdr = memory.read_value(header).ok()?;
                regions.extend(read_region(memory, header.as_raw(), size_of::<Msghdr>()));
                regions.extend(iovec_array(
                    memory,
                    message.msg_iov as usize,
                    message.msg_iovlen,
                ));
                let name = (message.msg_namelen as usize).min(MAX_NAME);
                if !message.msg_name.is_null() && name > 0 {
                    regions.extend(read_region(memory, message.msg_name as usize, name));
                }
                let control = message.msg_controllen;
                if !message.msg_control.is_null() && control > 0 && control <= MAX_CONTROL {
                    regions.extend(read_region(memory, message.msg_control as usize, control));
                }
            }
            Syscall::Sendto(call) => {
                let name = usize::try_from(call.addr_len()).unwrap_or(0).min(MAX_NAME);
                if name > 0 {
                    regions.extend(read_region(memory, call.addr()?.as_raw(), name));
                }
            }
            _ => return None,
        }
        (!regions.is_empty()).then_some(EntryImport {
            call: *call,
            regions,
            written_on_completion,
            observation: None,
        })
    }

    /// Read only the metadata of one actual attempt, after reinstatement and
    /// immediately before delegation. These calls preserve their pointer
    /// arguments in `into_nonblocking` and have one terminal data-bearing
    /// attempt. Aggregated pipe writev deliberately uses a different contract.
    pub(crate) fn capture_observation<M: MemoryAccess>(
        memory: &M,
        call: &Syscall,
    ) -> Option<Result<Vec<(usize, usize)>, Errno>> {
        let geometry = || -> Result<Vec<(usize, usize)>, Errno> {
            let (address, count) = match call {
                Syscall::Readv(call) => (
                    call.iov().map_or(0, |address| address.as_raw()),
                    crate::io_buffers::linux_iovec_count(call.len() as u64),
                ),
                Syscall::Recvmsg(call) => {
                    let message: Msghdr = memory.read_value(call.msg().ok_or(Errno::EFAULT)?)?;
                    (message.msg_iov as usize, message.msg_iovlen)
                }
                _ => unreachable!("only supported observation calls enter capture"),
            };
            if count == 0 {
                return Ok(Vec::new());
            }
            if count > MAX_IOVECS {
                return Err(Errno::EINVAL);
            }
            let address = Addr::<libc::iovec>::from_raw(address).ok_or(Errno::EFAULT)?;
            let mut iovecs = vec![
                libc::iovec {
                    iov_base: std::ptr::null_mut(),
                    iov_len: 0,
                };
                count
            ];
            memory.read_values(address, &mut iovecs)?;
            Ok(iovecs
                .into_iter()
                .map(|iov| (iov.iov_base as usize, iov.iov_len))
                .collect())
        };
        matches!(call, Syscall::Readv(_) | Syscall::Recvmsg(_)).then(geometry)
    }

    /// Attach the successful terminal attempt's observer result without
    /// changing the metadata used by retry or exit-dependency observers.
    pub(crate) fn with_observation(
        entry: Option<Box<Self>>,
        call: Syscall,
        observation: Option<Result<Vec<(usize, usize)>, Errno>>,
    ) -> Option<Box<Self>> {
        let Some(observation) = observation else {
            return entry;
        };
        let mut entry = entry.unwrap_or_else(|| {
            Box::new(Self {
                call,
                regions: Vec::new(),
                written_on_completion: Vec::new(),
                observation: None,
            })
        });
        assert_eq!(
            entry.call, call,
            "observation belongs to the same guest call"
        );
        entry.observation = Some(observation);
        Some(entry)
    }

    pub(crate) fn has_observation(&self) -> bool {
        self.observation.is_some()
    }

    /// Complete geometry of the actual successful read attempt, or no
    /// certificate. Unknown must use the ordinary post-call observer, never
    /// an older candidate. Outbound and aggregate write observations do not
    /// use this read-side certificate.
    pub(crate) fn observation_geometry(&self) -> Option<&[(usize, usize)]> {
        self.observation.as_ref()?.as_ref().ok().map(Vec::as_slice)
    }

    /// The call this memory belongs to.
    pub(crate) fn call(&self) -> Syscall {
        self.call
    }

    /// Whether the byte at `address` is one a completed `recvmsg` writes back.
    fn written_back(&self, address: usize) -> bool {
        self.written_on_completion
            .iter()
            .any(|&(start, end)| (start..end).contains(&address))
    }

    /// The bytes `region` holds in the kernel's view: as the call began, or,
    /// for a call that `returned`, with the fields it writes back as the
    /// call left them (`now`).
    fn kernel_view(&self, region: &Region, now: &[u8], returned: bool) -> Vec<u8> {
        let mut bytes = region.bytes.clone();
        if returned {
            for (offset, byte) in bytes.iter_mut().enumerate() {
                if self.written_back(region.address + offset) {
                    *byte = now[offset];
                }
            }
        }
        bytes
    }

    /// Whether another thread has changed the call's memory since it began,
    /// as far as it can still be read, apart from the fields a completed
    /// `recvmsg` writes back, which the kernel itself changes.
    pub(crate) fn differs<M: MemoryAccess>(&self, memory: &M) -> bool {
        self.regions.iter().any(|region| {
            read_region(memory, region.address, region.bytes.len())
                .is_some_and(|now| now.bytes != self.kernel_view(region, &now.bytes, true))
        })
    }

    /// Write the call's memory as it began back wherever another thread has
    /// changed it since, and return what that thread wrote, for
    /// [`EntryImport::put_back`] after the attempt. A region that can no
    /// longer be read or written is left as it is: that attempt copies it
    /// again, as one did before this copy was kept.
    pub(crate) fn reinstate<M: MemoryAccess>(&self, memory: &mut M) -> Displaced {
        self.reinstate_view(memory, false)
    }

    /// As [`EntryImport::reinstate`], for a reader of a call that has
    /// returned: the fields a completed `recvmsg` wrote back keep the
    /// values the call left, as the kernel's own view of the call then
    /// holds them. Undo with `put_back(.., false)`.
    pub(crate) fn reinstate_returned<M: MemoryAccess>(&self, memory: &mut M) -> Displaced {
        self.reinstate_view(memory, true)
    }

    fn reinstate_view<M: MemoryAccess>(&self, memory: &mut M, returned: bool) -> Displaced {
        let mut displaced = Displaced::default();
        for (index, region) in self.regions.iter().enumerate() {
            let Some(now) = read_region(memory, region.address, region.bytes.len()) else {
                continue;
            };
            let view = self.kernel_view(region, &now.bytes, returned);
            if now.bytes == view {
                continue;
            }
            let Some(at) = AddrMut::<u8>::from_raw(region.address) else {
                continue;
            };
            if memory.write_exact(at, &view).is_ok() {
                displaced.regions.push((index, now.bytes));
            }
        }
        displaced
    }

    /// Restore what another thread wrote over the call's memory
    /// ([`EntryImport::reinstate`]) after an attempt, except for each byte the
    /// attempt itself changed, which Linux would have written after that
    /// thread did, and, when the attempt `completed`, the fields a completed
    /// `recvmsg` writes back. The kernel writes into the captured memory only
    /// those `recvmsg` fields, which a would-block attempt leaves alone, so a
    /// changed byte can only come from an attempt whose data, address or
    /// control buffer overlaps the captured array or header, where Linux too
    /// writes it last. Fails only if the guest's bytes cannot be written back.
    pub(crate) fn put_back<M: MemoryAccess>(
        &self,
        memory: &mut M,
        displaced: Displaced,
        completed: bool,
    ) -> Result<(), Errno> {
        for (index, mut bytes) in displaced.regions {
            let region = &self.regions[index];
            if let Some(after) = read_region(memory, region.address, region.bytes.len()) {
                for (offset, byte) in bytes.iter_mut().enumerate() {
                    let written_back = completed && self.written_back(region.address + offset);
                    if after.bytes[offset] != region.bytes[offset] || written_back {
                        *byte = after.bytes[offset];
                    }
                }
            }
            let at = AddrMut::<u8>::from_raw(region.address).ok_or(Errno::EFAULT)?;
            memory.write_exact(at, &bytes)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use reverie::syscalls::LocalMemory;
    use reverie::syscalls::Readv;
    use reverie::syscalls::Recvmsg;
    use reverie::syscalls::Sendmsg;
    use reverie::syscalls::Sendto;
    use reverie::syscalls::Writev;

    use super::*;

    fn iovecs(buffers: &mut [[u8; 8]]) -> Vec<libc::iovec> {
        buffers
            .iter_mut()
            .map(|buffer| libc::iovec {
                iov_base: buffer.as_mut_ptr().cast(),
                iov_len: buffer.len(),
            })
            .collect()
    }

    fn readv(iov: &[libc::iovec]) -> Syscall {
        Syscall::Readv(
            Readv::new()
                .with_fd(3)
                .with_iov(Addr::from_ptr(iov.as_ptr()))
                .with_len(iov.len()),
        )
    }

    fn empty_msghdr() -> Msghdr {
        // SAFETY: an all-zero msghdr is a valid empty message header.
        unsafe { std::mem::zeroed() }
    }

    #[test]
    fn a_rewritten_iovec_array_is_reinstated_for_the_attempt_and_put_back_after() {
        let mut first = [[0u8; 8]; 2];
        let mut second = [[0u8; 8]; 2];
        let mut iov = iovecs(&mut first);
        let call = readv(&iov);
        let mut memory = LocalMemory::new();
        let entry = EntryImport::capture(&memory, &call).expect("readv copies its array");
        let rewritten = iovecs(&mut second);
        iov.copy_from_slice(&rewritten);
        let displaced = entry.reinstate(&mut memory);
        assert!(!displaced.is_empty());
        assert_eq!(iov[0].iov_base, first[0].as_mut_ptr().cast());
        assert_eq!(iov[1].iov_base, first[1].as_mut_ptr().cast());
        entry
            .put_back(&mut memory, displaced, true)
            .expect("the array is writable");
        assert_eq!(iov[0].iov_base, second[0].as_mut_ptr().cast());
        assert_eq!(iov[1].iov_base, second[1].as_mut_ptr().cast());
    }

    #[test]
    fn an_unchanged_array_is_neither_written_nor_put_back() {
        let mut first = [[0u8; 8]; 2];
        let iov = iovecs(&mut first);
        let mut memory = LocalMemory::new();
        let entry = EntryImport::capture(&memory, &readv(&iov)).expect("readv copies its array");
        assert!(entry.reinstate(&mut memory).is_empty());
    }

    #[test]
    fn a_byte_the_attempt_wrote_over_the_array_is_kept() {
        // A read whose data lands on its own array: the attempt's bytes are
        // written after the other thread's, as on Linux.
        let mut first = [[0u8; 8]; 2];
        let mut second = [[0u8; 8]; 2];
        let mut iov = iovecs(&mut first);
        let mut memory = LocalMemory::new();
        let entry = EntryImport::capture(&memory, &readv(&iov)).expect("readv copies its array");
        let rewritten = iovecs(&mut second);
        iov.copy_from_slice(&rewritten);
        let displaced = entry.reinstate(&mut memory);
        iov[1].iov_len = 7;
        entry
            .put_back(&mut memory, displaced, true)
            .expect("the array is writable");
        assert_eq!(iov[0].iov_base, second[0].as_mut_ptr().cast());
        assert_eq!(iov[1].iov_base, second[1].as_mut_ptr().cast());
        assert_eq!(iov[1].iov_len, 7);
    }

    #[test]
    fn a_recvmsg_header_and_array_are_reinstated_and_its_written_back_fields_kept() {
        let mut first = [[0u8; 8]; 2];
        let mut second = [[0u8; 8]; 2];
        let iov = iovecs(&mut first);
        let other = iovecs(&mut second);
        let mut name = [0u8; 16];
        let mut header = empty_msghdr();
        header.msg_name = name.as_mut_ptr().cast();
        header.msg_namelen = name.len() as libc::socklen_t;
        header.msg_iov = iov.as_ptr().cast_mut();
        header.msg_iovlen = iov.len();
        header.msg_flags = 0;
        let call = Syscall::Recvmsg(
            Recvmsg::new()
                .with_sockfd(3)
                .with_msg(AddrMut::from_ptr(&raw mut header)),
        );
        let mut memory = LocalMemory::new();
        let entry = EntryImport::capture(&memory, &call).expect("recvmsg copies its header");
        header.msg_iov = other.as_ptr().cast_mut();
        header.msg_flags = 5;
        let displaced = entry.reinstate(&mut memory);
        assert_eq!(header.msg_iov, iov.as_ptr().cast_mut());
        assert_eq!(header.msg_flags, 0);
        // The completed attempt writes the fields back, msg_flags with the
        // value it held: a byte left unchanged is still the call's.
        header.msg_namelen = 2;
        entry
            .put_back(&mut memory, displaced, true)
            .expect("the header is writable");
        assert_eq!(header.msg_iov, other.as_ptr().cast_mut());
        assert_eq!(header.msg_namelen, 2);
        assert_eq!(header.msg_flags, 0);
    }

    #[test]
    fn an_attempt_that_did_not_complete_puts_back_every_field() {
        let mut first = [[0u8; 8]; 2];
        let iov = iovecs(&mut first);
        let mut header = empty_msghdr();
        header.msg_iov = iov.as_ptr().cast_mut();
        header.msg_iovlen = iov.len();
        let call = Syscall::Recvmsg(
            Recvmsg::new()
                .with_sockfd(3)
                .with_msg(AddrMut::from_ptr(&raw mut header)),
        );
        let mut memory = LocalMemory::new();
        let entry = EntryImport::capture(&memory, &call).expect("recvmsg copies its header");
        header.msg_flags = 5;
        let displaced = entry.reinstate(&mut memory);
        assert_eq!(header.msg_flags, 0);
        entry
            .put_back(&mut memory, displaced, false)
            .expect("the header is writable");
        assert_eq!(header.msg_flags, 5);
    }

    #[test]
    fn a_sendmsg_address_and_control_data_are_kept_as_the_call_began() {
        let mut first = [[0u8; 8]; 1];
        let iov = iovecs(&mut first);
        let mut name = [1u8; 16];
        let mut control = [2u8; 24];
        let mut header = empty_msghdr();
        header.msg_name = name.as_mut_ptr().cast();
        header.msg_namelen = name.len() as libc::socklen_t;
        header.msg_iov = iov.as_ptr().cast_mut();
        header.msg_iovlen = iov.len();
        header.msg_control = control.as_mut_ptr().cast();
        header.msg_controllen = control.len();
        let call = Syscall::Sendmsg(
            Sendmsg::new()
                .with_fd(3)
                .with_msg(Addr::from_ptr(&raw const header))
                .with_flags(0),
        );
        let mut memory = LocalMemory::new();
        let entry = EntryImport::capture(&memory, &call).expect("sendmsg copies its header");
        name[0] = 9;
        control[23] = 9;
        let displaced = entry.reinstate(&mut memory);
        assert_eq!((name[0], control[23]), (1, 2));
        entry
            .put_back(&mut memory, displaced, true)
            .expect("the buffers are writable");
        assert_eq!((name[0], control[23]), (9, 9));
    }

    #[test]
    fn a_reader_of_a_returned_recvmsg_sees_its_header_as_began_with_the_written_back_fields() {
        let mut first = [[0u8; 8]; 2];
        let mut second = [[0u8; 8]; 2];
        let iov = iovecs(&mut first);
        let other = iovecs(&mut second);
        let mut control = [0u8; 32];
        let mut other_control = [0u8; 32];
        let mut header = empty_msghdr();
        header.msg_iov = iov.as_ptr().cast_mut();
        header.msg_iovlen = iov.len();
        header.msg_control = control.as_mut_ptr().cast();
        header.msg_controllen = control.len();
        let call = Syscall::Recvmsg(
            Recvmsg::new()
                .with_sockfd(3)
                .with_msg(AddrMut::from_ptr(&raw mut header)),
        );
        let mut memory = LocalMemory::new();
        let entry = EntryImport::capture(&memory, &call).expect("recvmsg copies its header");
        // The kernel's own write-back alone is not another thread's rewrite.
        header.msg_controllen = 20;
        header.msg_flags = libc::MSG_CTRUNC;
        assert!(!entry.differs(&memory));
        // Another thread repoints the array and the control buffer.
        header.msg_iov = other.as_ptr().cast_mut();
        header.msg_control = other_control.as_mut_ptr().cast();
        assert!(entry.differs(&memory));
        let displaced = entry.reinstate_returned(&mut memory);
        assert_eq!(header.msg_iov, iov.as_ptr().cast_mut());
        assert_eq!(header.msg_control, control.as_mut_ptr().cast());
        assert_eq!(
            (header.msg_controllen, header.msg_flags),
            (20, libc::MSG_CTRUNC)
        );
        entry
            .put_back(&mut memory, displaced, false)
            .expect("the header is writable");
        assert_eq!(header.msg_iov, other.as_ptr().cast_mut());
        assert_eq!(header.msg_control, other_control.as_mut_ptr().cast());
        assert_eq!(
            (header.msg_controllen, header.msg_flags),
            (20, libc::MSG_CTRUNC)
        );
    }

    #[test]
    fn a_sendto_address_is_kept_as_the_call_began() {
        let mut address = [3u8; 16];
        let call = Syscall::Sendto(
            Sendto::new()
                .with_fd(3)
                .with_size(0)
                .with_addr(AddrMut::from_ptr(address.as_mut_ptr().cast()))
                .with_addr_len(address.len() as i32),
        );
        let mut memory = LocalMemory::new();
        let entry = EntryImport::capture(&memory, &call).expect("sendto copies its address");
        address[15] = 9;
        let displaced = entry.reinstate(&mut memory);
        assert_eq!(address[15], 3);
        entry
            .put_back(&mut memory, displaced, false)
            .expect("the address is writable");
        assert_eq!(address[15], 9);
        let unnamed = Syscall::Sendto(Sendto::new().with_fd(3).with_size(0));
        assert_eq!(EntryImport::capture(&memory, &unnamed), None);
    }

    #[test]
    fn what_linux_does_not_copy_is_not_captured() {
        let memory = LocalMemory::new();
        let none = Syscall::Writev(Writev::new().with_fd(3).with_len(2));
        assert_eq!(EntryImport::capture(&memory, &none), None);
        let mut first = [[0u8; 8]; 1];
        let iov = iovecs(&mut first);
        let overlong = Syscall::Writev(
            Writev::new()
                .with_fd(3)
                .with_iov(Addr::from_ptr(iov.as_ptr()))
                .with_len(MAX_IOVECS + 1),
        );
        assert_eq!(EntryImport::capture(&memory, &overlong), None);
    }
}
