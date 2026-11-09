//! Fixed-gate, preallocated receive half of the one-use native provider.
//! Caller holds authenticated namespace/peer authority and a bootstrap FD lease.

use reverie::syscalls::linux_abi::Cmsghdr;
use reverie::syscalls::linux_abi::Msghdr;
use reverie_inguest::trap::raw_syscall6;
use sha2::Digest;
use sha2::Sha256;

/// Create the one-use, unconnected provider socket before Guest work.
/// The actual transaction connects this same owned descriptor in `exchange`.
pub fn create_socket() -> std::io::Result<i32> {
    let call = |number, args| unsafe { reverie_inguest::trap::raw_syscall6(number, args) };
    let fd = call(
        libc::SYS_socket,
        [
            libc::AF_UNIX as u64,
            (libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC) as u64,
            0,
            0,
            0,
            0,
        ],
    );
    if fd < 0 {
        return Err(std::io::Error::from_raw_os_error(-fd as i32));
    }
    Ok(fd as i32)
}

const ALL_SEALS: i32 =
    libc::F_SEAL_SEAL | libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_WRITE;
const MAX_OBJECT: u64 = 256 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Error(pub &'static str);

fn call(number: i64, args: [u64; 6]) -> Result<i64, Error> {
    let result = unsafe { raw_syscall6(number, args) };
    if result < 0 {
        Err(Error("native provider kernel operation failed"))
    } else {
        Ok(result)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Object {
    pub device: u64,
    pub inode: u64,
    pub size: u64,
    pub digest: [u8; 32],
}

fn metadata(fd: i32) -> Result<libc::stat, Error> {
    let mut stat: libc::stat = unsafe { core::mem::zeroed() };
    call(
        libc::SYS_fstat,
        [fd as u64, (&raw mut stat) as u64, 0, 0, 0, 0],
    )?;
    if stat.st_mode & libc::S_IFMT != libc::S_IFREG
        || stat.st_size <= 0
        || stat.st_size as u64 > MAX_OBJECT
    {
        return Err(Error("native provider role is not a bounded regular file"));
    }
    Ok(stat)
}

fn hash(fd: i32, size: u64, scratch: &mut [u8; 16384]) -> Result<[u8; 32], Error> {
    let mut sha = Sha256::new();
    let mut offset = 0;
    while offset < size {
        let length = (size - offset).min(scratch.len() as u64) as usize;
        let n = call(
            libc::SYS_pread64,
            [
                fd as u64,
                scratch.as_mut_ptr() as u64,
                length as u64,
                offset,
                0,
                0,
            ],
        )? as usize;
        if n == 0 || n > length {
            return Err(Error("native provider role shortened during read"));
        }
        sha.update(&scratch[..n]);
        offset += n as u64;
    }
    if call(
        libc::SYS_pread64,
        [fd as u64, scratch.as_mut_ptr() as u64, 1, offset, 0, 0],
    )? != 0
    {
        return Err(Error("native provider role grew during read"));
    }
    Ok(sha.finalize().into())
}

fn required_flags_and_seals(fd: i32) -> Result<(), Error> {
    if call(
        libc::SYS_fcntl,
        [fd as u64, libc::F_GETFD as u64, 0, 0, 0, 0],
    )? != i64::from(libc::FD_CLOEXEC)
    {
        return Err(Error("native provider source is not CLOEXEC"));
    }
    let flags = call(
        libc::SYS_fcntl,
        [fd as u64, libc::F_GETFL as u64, 0, 0, 0, 0],
    )? as i32;
    if flags & libc::O_PATH != 0 {
        return Err(Error("native provider source has O_PATH"));
    }
    let seals = call(
        libc::SYS_fcntl,
        [fd as u64, libc::F_GET_SEALS as u64, 0, 0, 0, 0],
    )? as i32;
    if seals & ALL_SEALS != ALL_SEALS {
        return Err(Error("native provider source is not fully sealed"));
    }
    Ok(())
}

impl Object {
    pub fn verify(&self, fd: i32, scratch: &mut [u8; 16384]) -> Result<(), Error> {
        required_flags_and_seals(fd)?;
        let before = metadata(fd)?;
        if (before.st_dev, before.st_ino, before.st_size as u64)
            != (self.device, self.inode, self.size)
            || hash(fd, self.size, scratch)? != self.digest
        {
            return Err(Error("native provider role identity or exact bytes differ"));
        }
        let after = metadata(fd)?;
        if (after.st_dev, after.st_ino, after.st_size, after.st_mode)
            != (before.st_dev, before.st_ino, before.st_size, before.st_mode)
        {
            return Err(Error("native provider role changed during read"));
        }
        Ok(())
    }
}

/// Sources received CLOEXEC; they remain separate from the owned fixed slots.
/// Every installed received FD is reclaimed even on malformed ancillary data.
#[derive(Debug)]
pub struct Received {
    fds: [i32; 16],
    count: usize,
}
impl Received {
    fn empty() -> Self {
        Self {
            fds: [-1; 16],
            count: 0,
        }
    }
    pub fn fds(&self) -> Result<[i32; 5], Error> {
        if self.count != 5 {
            return Err(Error(
                "native provider did not send exactly five descriptors",
            ));
        }
        Ok(self.fds[..5].try_into().unwrap())
    }
    pub fn close(mut self) -> Result<(), Error> {
        for fd in &mut self.fds[..self.count] {
            let owned = core::mem::replace(fd, -1);
            call(libc::SYS_close, [owned as u64, 0, 0, 0, 0, 0])?;
        }
        Ok(())
    }
}
impl Drop for Received {
    fn drop(&mut self) {
        for fd in &mut self.fds[..self.count] {
            if *fd >= 0 {
                let owned = core::mem::replace(fd, -1);
                if call(libc::SYS_close, [owned as u64, 0, 0, 0, 0, 0]).is_err() {
                    unsafe {
                        raw_syscall6(libc::SYS_exit_group, [122, 0, 0, 0, 0, 0]);
                    }
                    loop {
                        core::hint::spin_loop();
                    }
                }
            }
        }
    }
}

pub fn request(ticket: &[u8; 32]) -> [u8; 48] {
    let mut bytes = [0; 48];
    bytes[..8].copy_from_slice(b"HLPROV01");
    bytes[8..10].copy_from_slice(&1u16.to_be_bytes());
    bytes[10..12].copy_from_slice(&48u16.to_be_bytes());
    bytes[12..44].copy_from_slice(ticket);
    bytes
}
pub fn expected_response(ticket: &[u8; 32], digest: &[u8; 32]) -> [u8; 80] {
    let mut bytes = [0; 80];
    bytes[..8].copy_from_slice(b"HLPRACK1");
    bytes[8..10].copy_from_slice(&1u16.to_be_bytes());
    bytes[10..12].copy_from_slice(&80u16.to_be_bytes());
    bytes[12..14].copy_from_slice(&5u16.to_be_bytes());
    bytes[16..48].copy_from_slice(ticket);
    bytes[48..80].copy_from_slice(digest);
    bytes
}

// Linux LP64 CMSG alignment is sizeof(size_t), independent of libc's
// wrapper ABI. The shared headers are checked against generated Linux UAPI.
// Like GNU CMSG_FIRSTHDR/NXTHDR, trailing bytes shorter than a header terminate
// the cursor; a present header must fit completely before any payload is read.
type ControlMessage<'a> = (Cmsghdr, &'a [u8], usize);

fn control_message(control: &[u8], offset: usize) -> Result<Option<ControlMessage<'_>>, Error> {
    let minimum = core::mem::size_of::<Cmsghdr>();
    if offset > control.len() || control.len() - offset < minimum {
        return Ok(None);
    }
    // SAFETY: the complete integer-only header lies in initialized bytes.
    // read_unaligned does not require a reference into an aligned C object.
    let header = unsafe {
        control
            .as_ptr()
            .add(offset)
            .cast::<Cmsghdr>()
            .read_unaligned()
    };
    if header.cmsg_len < minimum || header.cmsg_len > control.len() - offset {
        return Err(Error("native provider kernel ancillary shape differs"));
    }
    let end = offset + header.cmsg_len;
    let alignment = core::mem::size_of::<usize>();
    let aligned = header
        .cmsg_len
        .checked_add(alignment - 1)
        .ok_or(Error("native provider kernel ancillary shape differs"))?
        & !(alignment - 1);
    let next = offset
        .checked_add(aligned)
        .ok_or(Error("native provider kernel ancillary shape differs"))?;
    Ok(Some((header, &control[offset + minimum..end], next)))
}

/// # Safety
/// This is the actual protected, unconnected seqpacket socket for the same
/// independently authenticated coordinator as the owned RPC stream. Caller has
/// already reserved all fixed target FDs, captured counters, sealed the actual
/// transaction and preencoded exact bytes. Connect occurs only here, immediately
/// before the one request. Any error is terminal for this native transaction;
/// the protected descriptor is never closed, replaced, reconnected or retried.
pub unsafe fn exchange(
    fd: i32,
    request: &[u8; 48],
    expected: &[u8; 80],
) -> Result<Received, Error> {
    let mut address: libc::sockaddr_un = unsafe { core::mem::zeroed() };
    address.sun_family = libc::AF_UNIX as _;
    let path = b"/run/provision.sock\0";
    for (to, from) in address.sun_path.iter_mut().zip(path) {
        *to = *from as _;
    }
    let result = call(
        libc::SYS_connect,
        [
            fd as u64,
            (&raw const address) as u64,
            (2 + path.len()) as u64,
            0,
            0,
            0,
        ],
    )?;
    if result != 0 {
        return Err(Error("native provider connect did not complete"));
    }
    let sent = call(
        libc::SYS_sendto,
        [
            fd as u64,
            request.as_ptr() as u64,
            request.len() as u64,
            libc::MSG_NOSIGNAL as u64,
            0,
            0,
        ],
    )?;
    if sent != request.len() as i64 {
        return Err(Error("native provider request was partial"));
    }
    let mut bytes = [0u8; 80];
    // Space for unexpected rights is deliberate so every kernel-installed FD
    // can be closed before rejecting. Truncation still always refuses.
    let mut ancillary = [0u64; 10];
    let mut iov = libc::iovec {
        iov_base: bytes.as_mut_ptr().cast(),
        iov_len: bytes.len(),
    };
    let mut msg: Msghdr = unsafe { core::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = ancillary.as_mut_ptr().cast();
    msg.msg_controllen = core::mem::size_of_val(&ancillary);
    let n = call(
        libc::SYS_recvmsg,
        [
            fd as u64,
            (&raw mut msg) as u64,
            (libc::MSG_CMSG_CLOEXEC | libc::MSG_TRUNC) as u64,
            0,
            0,
            0,
        ],
    )?;
    let mut received = Received::empty();
    let mut shape = true;
    let mut records = 0;
    if msg.msg_controllen > core::mem::size_of_val(&ancillary) {
        return Err(Error("native provider kernel ancillary shape differs"));
    }
    // SAFETY: ancillary is initialized storage retained for the entire parse;
    // the kernel-returned byte count is bounded above before forming the slice.
    let control =
        unsafe { core::slice::from_raw_parts(ancillary.as_ptr().cast::<u8>(), msg.msg_controllen) };
    let mut offset = 0;
    while let Some((header, data, next)) = control_message(control, offset)? {
        records += 1;
        if header.cmsg_level != libc::SOL_SOCKET || header.cmsg_type != libc::SCM_RIGHTS {
            shape = false;
        } else {
            let count = data.len() / core::mem::size_of::<i32>();
            shape &= data.len().is_multiple_of(core::mem::size_of::<i32>());
            for index in 0..count {
                // SAFETY: the cursor checked the complete payload range; each
                // index denotes one complete native-endian kernel descriptor.
                let raw = unsafe { data.as_ptr().cast::<i32>().add(index).read_unaligned() };
                if received.count == received.fds.len() {
                    call(libc::SYS_close, [raw as u64, 0, 0, 0, 0, 0])?;
                    shape = false;
                } else {
                    received.fds[received.count] = raw;
                    received.count += 1;
                }
            }
        }
        offset = next;
    }
    if n != bytes.len() as i64
        || &bytes != expected
        || msg.msg_flags & (libc::MSG_TRUNC | libc::MSG_CTRUNC) != 0
        || !shape
        || records != 1
        || received.count != 5
    {
        return Err(Error("native provider reply or descriptor count differs"));
    }
    for fd in received.fds()? {
        required_flags_and_seals(fd)?;
    }
    Ok(received)
}

/// Compare a received fully sealed input against the independently encoded
/// expected HLNEXEC1 packet, including exact file length. Never decode and trust
/// expectations carried by that same received packet.
pub fn verify_input(fd: i32, expected: &[u8], buffer: &mut [u8; 4096]) -> Result<(), Error> {
    required_flags_and_seals(fd)?;
    let stat = metadata(fd)?;
    if expected.is_empty()
        || expected.len() > buffer.len()
        || stat.st_size as usize != expected.len()
    {
        return Err(Error("native successor input length differs"));
    }
    let n = call(
        libc::SYS_pread64,
        [
            fd as u64,
            buffer.as_mut_ptr() as u64,
            expected.len() as u64,
            0,
            0,
            0,
        ],
    )?;
    if n != expected.len() as i64 || &buffer[..expected.len()] != expected {
        return Err(Error("native successor input bytes differ"));
    }
    Ok(())
}

#[cfg(test)]
mod wire_tests {
    use std::os::fd::AsRawFd;
    use std::os::fd::FromRawFd;
    use std::os::fd::OwnedFd;

    use super::*;

    // The sender uses libc's actual wrapper and native CMSG ABI. The receiver
    // bypasses libc with SYS_recvmsg, just like exchange, and uses the shared
    // Linux wire layout. No native admission or fixed-gate readiness is forged.
    fn kernel_rights(count: usize) -> (Vec<OwnedFd>, i32, usize) {
        assert!(count == 5 || count == 20);
        let source = std::fs::File::open("/dev/null").unwrap();
        let mut sockets = [-1; 2];
        assert_eq!(
            unsafe {
                libc::socketpair(
                    libc::AF_UNIX,
                    libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
                    0,
                    sockets.as_mut_ptr(),
                )
            },
            0
        );
        let sockets = sockets.map(|fd| unsafe { OwnedFd::from_raw_fd(fd) });
        let mut send_control = [0u64; 12];
        let mut byte = 17u8;
        let mut iov = libc::iovec {
            iov_base: (&raw mut byte).cast(),
            iov_len: 1,
        };
        let mut send: libc::msghdr = unsafe { core::mem::zeroed() };
        send.msg_iov = &mut iov;
        send.msg_iovlen = 1;
        send.msg_control = send_control.as_mut_ptr().cast();
        // Both bounded lengths fit GNU size_t and musl socklen_t exactly.
        send.msg_controllen = if count == 5 { 40 } else { 96 };
        let header = unsafe { libc::CMSG_FIRSTHDR(&send) };
        assert!(!header.is_null());
        unsafe {
            (*header).cmsg_level = libc::SOL_SOCKET;
            (*header).cmsg_type = libc::SCM_RIGHTS;
            (*header).cmsg_len = if count == 5 { 36 } else { 96 };
            for index in 0..count {
                libc::CMSG_DATA(header)
                    .cast::<i32>()
                    .add(index)
                    .write_unaligned(source.as_raw_fd());
            }
        }
        assert_eq!(
            unsafe { libc::sendmsg(sockets[0].as_raw_fd(), &send, 0) },
            1
        );
        let mut control = [0u64; 10];
        let mut recv: Msghdr = unsafe { core::mem::zeroed() };
        recv.msg_iov = &mut iov;
        recv.msg_iovlen = 1;
        recv.msg_control = control.as_mut_ptr().cast();
        recv.msg_controllen = core::mem::size_of_val(&control);
        assert_eq!(
            unsafe {
                libc::syscall(
                    libc::SYS_recvmsg,
                    sockets[1].as_raw_fd(),
                    &raw mut recv,
                    libc::MSG_CMSG_CLOEXEC | libc::MSG_TRUNC,
                )
            },
            1
        );
        assert!(recv.msg_controllen <= core::mem::size_of_val(&control));
        let bytes = unsafe {
            core::slice::from_raw_parts(control.as_ptr().cast::<u8>(), recv.msg_controllen)
        };
        let (header, payload, next) = control_message(bytes, 0).unwrap().unwrap();
        assert_eq!(header.cmsg_level, libc::SOL_SOCKET);
        assert_eq!(header.cmsg_type, libc::SCM_RIGHTS);
        assert!(control_message(bytes, next).unwrap().is_none());
        let owned: Vec<_> = payload
            .as_chunks::<4>()
            .0
            .iter()
            .map(|chunk| {
                let fd = i32::from_ne_bytes(*chunk);
                let fd = unsafe { OwnedFd::from_raw_fd(fd) };
                assert_eq!(
                    unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) },
                    libc::FD_CLOEXEC
                );
                fd
            })
            .collect();
        (owned, recv.msg_flags, header.cmsg_len)
    }

    #[test]
    fn kernel_wire_receive_preserves_five_rights_and_full_width_header() {
        let (owned, flags, length) = kernel_rights(5);
        assert_eq!(owned.len(), 5);
        assert_eq!(length, 36);
        assert_eq!(flags & (libc::MSG_TRUNC | libc::MSG_CTRUNC), 0);
        // OwnedFd retires every actual kernel-installed descriptor.
    }

    #[test]
    fn kernel_wire_receive_exposes_ancillary_truncation_without_losing_rights() {
        let (owned, flags, length) = kernel_rights(20);
        assert_eq!(owned.len(), 16);
        assert_eq!(length, 80);
        assert_ne!(flags & libc::MSG_CTRUNC, 0);
        // exchange's unchanged flag guard refuses this reply after collecting
        // all installed descriptors into Received for retirement on error.
    }

    #[test]
    fn kernel_control_cursor_rejects_short_and_out_of_bounds_records() {
        let mut bytes = [0u8; 40];
        // Literal Linux LP64 header bytes, not serialization of the adapter.
        bytes[8..12].copy_from_slice(&libc::SOL_SOCKET.to_ne_bytes());
        bytes[12..16].copy_from_slice(&libc::SCM_RIGHTS.to_ne_bytes());
        for bad in [0usize, 15, 41, usize::MAX] {
            bytes[..8].copy_from_slice(&bad.to_ne_bytes());
            assert!(control_message(&bytes, 0).is_err());
        }
        bytes[..8].copy_from_slice(&36usize.to_ne_bytes());
        let (header, payload, next) = control_message(&bytes, 0).unwrap().unwrap();
        assert_eq!(header.cmsg_len, 36);
        assert_eq!(payload.len(), 20);
        assert_eq!(next, 40);
        assert!(control_message(&bytes, next).unwrap().is_none());
        assert!(control_message(&bytes[..15], 0).unwrap().is_none());
        // A second complete header must be validated, never read past the end.
        bytes[..8].copy_from_slice(&16usize.to_ne_bytes());
        bytes[16..24].copy_from_slice(&25usize.to_ne_bytes());
        assert!(control_message(&bytes, 16).is_err());
    }
}
