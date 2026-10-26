/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Descriptors a guest receives in a binder transaction.
//!
//! A binder transaction can carry file descriptors: a `BINDER_TYPE_FD`
//! object, or a `BINDER_TYPE_FDA` array inside a buffer object. Linux opens
//! each one in the receiving process when that process reads the transaction
//! with `ioctl(BINDER_WRITE_READ)`, and writes the new numbers into the
//! transaction's buffer in the receiver's binder mapping
//! (`binder_apply_fd_fixups`, always with `O_CLOEXEC`). No call that names
//! them as new descriptors ever reaches Detcore, as with `SCM_RIGHTS`
//! (`crate::syscalls::scm_rights`): on a backend where the tool keeps its own
//! descriptor table (ptrace, KVM), every later call through one of them that
//! consults that table failed with `EBADF`.
//!
//! So after a `BINDER_WRITE_READ` that read something, Detcore walks the
//! return commands Linux wrote (`read_buffer[..read_consumed]`), and for
//! each delivered transaction (`BR_TRANSACTION`, `BR_TRANSACTION_SEC_CTX`,
//! `BR_REPLY`) reads its offsets array and objects from the receiver's
//! mapping and names the descriptors they now hold. They are then recorded as
//! received `SCM_RIGHTS` descriptors are
//! (`Detcore::record_received_descriptors`), from the kernel's own facts.
//!
//! Layouts are the 64-bit ABI of `include/uapi/linux/android/binder.h`, the
//! only one an arm64 or x86-64 kernel builds (`binder_uintptr_t` and
//! `binder_size_t` are 64 bits). Nothing here changes what the guest sees:
//! the call has already returned, and its memory is only read. A buffer that
//! cannot be read or parsed names no descriptor, and says why.

use std::os::fd::RawFd;

use reverie::syscalls::Addr;
use reverie::syscalls::MemoryAccess;

use crate::syscalls::scm_rights::Delivered;

/// `_IOWR('b', 1, struct binder_write_read)`.
pub(crate) const BINDER_WRITE_READ: u64 = binder_request(3, 1, size_of::<WriteRead>());

/// `_IOR('r', 2, struct binder_transaction_data)`.
const BR_TRANSACTION: u32 = 0x8040_7202;
/// `_IOR('r', 2, struct binder_transaction_data_secctx)`.
const BR_TRANSACTION_SEC_CTX: u32 = 0x8048_7202;
/// `_IOR('r', 3, struct binder_transaction_data)`.
const BR_REPLY: u32 = 0x8040_7203;

/// `TF_STATUS_CODE`: the transaction's data is a 32-bit status. Linux does
/// not read it when it installs descriptors; the tests use it.
#[cfg(test)]
const TF_STATUS_CODE: u32 = 0x08;

/// `B_PACK_CHARS('p', 't', '*', B_TYPE_LARGE)`, a buffer object: a
/// descriptor array's parent, which the tests build.
#[cfg(test)]
const BINDER_TYPE_PTR: u32 = 0x7074_2a85;

/// `B_PACK_CHARS('f', 'd', '*', B_TYPE_LARGE)`.
const BINDER_TYPE_FD: u32 = 0x6664_2a85;
/// `B_PACK_CHARS('f', 'd', 'a', B_TYPE_LARGE)`.
const BINDER_TYPE_FDA: u32 = 0x6664_6185;

/// The most return-command bytes read from one call: Linux writes at most
/// the caller's `read_size`, and libbinder reads 256 bytes at a time.
const MAX_COMMAND_BYTES: usize = 64 * 1024;
/// The most objects read from one transaction's offsets array.
const MAX_OBJECTS: usize = 4096;

/// `struct binder_write_read`, the argument of `BINDER_WRITE_READ`: six
/// 64-bit words on a 64-bit kernel, in the uapi header's order.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct WriteRead {
    pub(crate) write_size: u64,
    pub(crate) write_consumed: u64,
    pub(crate) write_buffer: u64,
    pub(crate) read_size: u64,
    pub(crate) read_consumed: u64,
    pub(crate) read_buffer: u64,
}

/// `struct binder_freeze_info`, the argument of `BINDER_FREEZE`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct FreezeInfo {
    _pid: u32,
    enable: u32,
    timeout_ms: u32,
}

/// `_IOC(dir, 'b', nr, size)` for binder's own requests.
const fn binder_request(dir: u64, nr: u64, size: usize) -> u64 {
    (dir << 30) | ((size as u64) << 16) | ((b'b' as u64) << 8) | nr
}

/// `_IOW('b', 14, struct binder_freeze_info)`, its size from the structure
/// rather than written out, so a test cannot share a wrong number.
const BINDER_FREEZE: u64 = binder_request(1, 14, size_of::<FreezeInfo>());

/// Whether `request`, an `ioctl` request word, is `BINDER_WRITE_READ`.
/// Only the low 32 bits are the request: the kernel's `cmd` is an
/// `unsigned int`.
pub(crate) fn is_write_read(request: u64) -> bool {
    request & 0xffff_ffff == BINDER_WRITE_READ
}

/// Why a binder `ioctl` waits in the kernel until another process acts, so
/// that Detcore runs it as a blocking external call rather than in the
/// caller's turn. `None` for every request that returns on its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Blocking {
    /// A `BINDER_WRITE_READ` whose read half waits for work for this thread:
    /// a transaction, a reply, or a death notification
    /// (`binder_thread_read` -> `binder_wait_for_work`).
    Read,
    /// A `BINDER_FREEZE` that waits for another process's outstanding
    /// transactions to drain, up to its own timeout
    /// (`binder_ioctl_freeze`).
    Freeze,
}

/// `WriteRead` of the call whose argument is at `argument`, read from guest
/// memory; `None` when it cannot be read.
pub(crate) fn write_read<M: MemoryAccess>(memory: &M, argument: usize) -> Option<WriteRead> {
    let address = Addr::<WriteRead>::from_raw(argument)?;
    memory.read_value(address).ok()
}

impl WriteRead {
    /// Whether this call has a read half, which is the half that waits.
    pub(crate) fn reads(&self) -> bool {
        self.read_size > self.read_consumed
    }
}

/// Why `request` waits in the kernel, from the call's argument already read
/// from guest memory: `None` when it returns on its own. `nonblocking` is the
/// descriptor's `O_NONBLOCK`, which makes a read return `EAGAIN` instead of
/// waiting (`binder_thread_read`). A read is classified as waiting whenever
/// it asks for bytes; the caller may then find work already queued and run
/// it in the caller's turn anyway.
pub(crate) fn blocking(
    request: u64,
    write_read: Option<&WriteRead>,
    freeze: Option<&[u32; 3]>,
    nonblocking: bool,
) -> Option<Blocking> {
    match request & 0xffff_ffff {
        BINDER_WRITE_READ if !nonblocking => write_read
            .filter(|call| call.reads())
            .map(|_| Blocking::Read),
        BINDER_FREEZE => freeze
            .filter(|info| info[1] != 0 && info[2] != 0)
            .map(|_| Blocking::Freeze),
        _ => None,
    }
}

/// `struct binder_freeze_info` at `argument`, read from guest memory.
pub(crate) fn freeze_info<M: MemoryAccess>(memory: &M, argument: usize) -> Option<[u32; 3]> {
    let address = Addr::<[u32; 3]>::from_raw(argument)?;
    memory.read_value(address).ok()
}

/// Warn once per Hermit process that this run's binder calls are external
/// input: their results come from processes Hermit does not schedule, so two
/// runs can differ wherever those processes do.
pub(crate) fn note_blocking(waits: Blocking) {
    static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        let call = match waits {
            Blocking::Read => "a binder read (BINDER_WRITE_READ)",
            Blocking::Freeze => "a binder freeze (BINDER_FREEZE)",
        };
        tracing::warn!(
            "[detcore] HERMIT_EXTERNAL_INPUT source=binder: {call} waits for another process, \
             which Hermit does not schedule. It runs as a blocking external call, so when it \
             finishes, and what it reads, are this host's, not the schedule's"
        );
    }
}

/// The `read_consumed` of the `struct binder_write_read` at `argument`,
/// read before the call: the bytes of `read_buffer` an earlier call wrote,
/// which this one leaves alone. Zero where it cannot be read, so the whole
/// buffer is parsed, as before.
pub(crate) fn consumed_before<M: MemoryAccess>(memory: &M, argument: usize) -> u64 {
    Addr::<[u64; 6]>::from_raw(argument)
        .and_then(|address| memory.read_value(address).ok())
        .map_or(0, |words: [u64; 6]| words[4])
}

/// The descriptors a successful `BINDER_WRITE_READ`, whose
/// `struct binder_write_read` is at `argument`, made Linux install in the
/// caller, read from guest memory after the call. `already_consumed` is the
/// `read_consumed` the caller passed in, read before the call: Linux writes
/// this call's commands after it.
pub(crate) fn delivered<M: MemoryAccess>(
    memory: &M,
    argument: usize,
    already_consumed: u64,
) -> Delivered {
    let Some(address) = Addr::<[u64; 6]>::from_raw(argument) else {
        return refused("its binder_write_read is at address 0");
    };
    let Ok(words) = memory.read_value(address) else {
        return refused("its binder_write_read cannot be read");
    };
    // write_size, write_consumed, write_buffer, read_size, read_consumed,
    // read_buffer.
    let [_, _, _, read_size, read_consumed, read_buffer] = words;
    if read_consumed == 0 || read_consumed == already_consumed {
        return Delivered::default();
    }
    if read_consumed > read_size || already_consumed > read_consumed {
        return refused("its read_consumed is not within its read_size");
    }
    // Linux writes this call's commands after the read_consumed the caller
    // passed in, leaving any earlier ones alone: those are an older call's,
    // and their buffers may already be freed.
    let newly_consumed = read_consumed - already_consumed;
    if newly_consumed > MAX_COMMAND_BYTES as u64 {
        return refused("its newly returned commands exceed Detcore's read limit");
    }
    let length = newly_consumed as usize;
    let Some(address) = Addr::<u8>::from_raw((read_buffer + already_consumed) as usize) else {
        return refused("its read_buffer is at address 0");
    };
    let mut commands = vec![0; length];
    if memory.read_exact(address, &mut commands).is_err() {
        return refused("its return commands cannot be read");
    }
    match installed_descriptors(memory, &commands) {
        Ok(fds) => Delivered {
            // One transaction: its descriptors arrive as one message.
            messages: vec![crate::syscalls::scm_rights::DeliveredMessage {
                count: fds.len(),
                truncated: false,
                bytes: None,
            }],
            fds,
            refused: None,
        },
        Err(why) => refused(why),
    }
}

fn refused(why: &'static str) -> Delivered {
    Delivered {
        fds: Vec::new(),
        messages: Vec::new(),
        refused: Some(why),
    }
}

/// `_IOC_SIZE(cmd)`: the payload size a return command carries after its
/// 32-bit code.
fn payload_size(cmd: u32) -> usize {
    ((cmd >> 16) & 0x3fff) as usize
}

fn u32_at(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_ne_bytes(
        bytes.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

fn u64_at(bytes: &[u8], offset: usize) -> Option<u64> {
    Some(u64::from_ne_bytes(
        bytes.get(offset..offset + 8)?.try_into().ok()?,
    ))
}

/// The descriptors the transactions in `commands`, a run of return commands
/// as Linux wrote them, carry, in order.
fn installed_descriptors<M: MemoryAccess>(
    memory: &M,
    commands: &[u8],
) -> Result<Vec<RawFd>, &'static str> {
    let mut fds = Vec::new();
    let mut offset = 0;
    while offset < commands.len() {
        let cmd = u32_at(commands, offset).ok_or("a return command is cut short")?;
        let payload = commands
            .get(offset + 4..offset + 4 + payload_size(cmd))
            .ok_or("a return command's payload is cut short")?;
        if matches!(cmd, BR_TRANSACTION | BR_TRANSACTION_SEC_CTX | BR_REPLY) {
            fds.extend(transaction_descriptors(memory, payload)?);
        }
        offset += 4 + payload.len();
    }
    Ok(fds)
}

/// The descriptors the transaction `data`, a `struct binder_transaction_data`
/// (the head of a `_secctx` one too), carries.
fn transaction_descriptors<M: MemoryAccess>(
    memory: &M,
    data: &[u8],
) -> Result<Vec<RawFd>, &'static str> {
    let short = "a transaction is cut short";
    // The driver never reads TF_STATUS_CODE when it installs descriptors, so
    // neither does this: a real status reply carries no objects anyway, and
    // skipping on the flag would hide a descriptor Linux did install.
    let offsets_size = u64_at(data, 40).ok_or(short)? as usize;
    let buffer = u64_at(data, 48).ok_or(short)? as usize;
    let offsets_at = u64_at(data, 56).ok_or(short)? as usize;
    let count = offsets_size / 8;
    if count == 0 {
        return Ok(Vec::new());
    }
    if count > MAX_OBJECTS {
        return Err("a transaction carries more objects than Detcore reads");
    }
    let mut offsets = vec![0u64; count];
    let address = Addr::<u64>::from_raw(offsets_at).ok_or("a transaction's offsets are at 0")?;
    memory
        .read_values(address, &mut offsets)
        .map_err(|_| "a transaction's offsets cannot be read")?;
    let object = |index: usize, length: usize| -> Result<Vec<u8>, &'static str> {
        let at = offsets
            .get(index)
            .ok_or("an object names a parent that is not in the transaction")?;
        let address = Addr::<u8>::from_raw(buffer.wrapping_add(*at as usize))
            .ok_or("a transaction's object is at 0")?;
        let mut bytes = vec![0; length];
        memory
            .read_exact(address, &mut bytes)
            .map_err(|_| "a transaction's object cannot be read")?;
        Ok(bytes)
    };
    let mut fds = Vec::new();
    for index in 0..count {
        // Every object starts with its 32-bit type.
        let header = object(index, 4)?;
        match u32_at(&header, 0) {
            // struct binder_fd_object: the type, a 32-bit pad, then the
            // descriptor in the low half of a 64-bit union.
            Some(BINDER_TYPE_FD) => {
                let fd_object = object(index, 24)?;
                fds.push(u32_at(&fd_object, 8).ok_or(short)? as RawFd);
            }
            // A BINDER_TYPE_FDA array's descriptors are the kernel's to
            // close: it closes them itself when the receiver frees the
            // transaction's buffer with BC_FREE_BUFFER, so a record of one
            // would outlive it and describe whatever next takes the number.
            // Left out until Detcore follows those frees
            // (https://github.com/rrnewton/hermit/issues/ISSUE-FDA).
            Some(BINDER_TYPE_FDA) => {
                let array = object(index, 32)?;
                let num_fds = u64_at(&array, 8).ok_or(short)?;
                if num_fds != 0 {
                    return Err(
                        "a transaction carries a descriptor array, whose descriptors Linux \
                         closes with the buffer",
                    );
                }
            }
            Some(_) => {}
            None => return Err(short),
        }
    }
    Ok(fds)
}

#[cfg(test)]
mod tests {
    use reverie::syscalls::LocalMemory;

    use super::*;

    /// A transaction's bytes, as a `struct binder_transaction_data`.
    fn transaction(flags: u32, offsets: &[u64], buffer: usize) -> Vec<u8> {
        let mut data = vec![0u8; 64];
        data[20..24].copy_from_slice(&flags.to_ne_bytes());
        data[40..48].copy_from_slice(&((offsets.len() * 8) as u64).to_ne_bytes());
        data[48..56].copy_from_slice(&(buffer as u64).to_ne_bytes());
        data[56..64].copy_from_slice(&(offsets.as_ptr() as u64).to_ne_bytes());
        data
    }

    fn command(cmd: u32, payload: &[u8]) -> Vec<u8> {
        let mut bytes = cmd.to_ne_bytes().to_vec();
        bytes.extend_from_slice(payload);
        bytes
    }

    fn fd_object(fd: u32) -> Vec<u8> {
        let mut object = vec![0u8; 24];
        object[0..4].copy_from_slice(&BINDER_TYPE_FD.to_ne_bytes());
        object[8..12].copy_from_slice(&fd.to_ne_bytes());
        object
    }

    /// A read half that wants bytes blocks, unless the descriptor is
    /// nonblocking; a write-only call and a freeze without a timeout do not.
    #[test]
    fn a_binder_call_blocks_when_it_waits_for_another_process() {
        let reading = WriteRead {
            read_size: 256,
            ..WriteRead::default()
        };
        let writing = WriteRead {
            write_size: 64,
            ..WriteRead::default()
        };
        assert_eq!(
            blocking(BINDER_WRITE_READ, Some(&reading), None, false),
            Some(Blocking::Read)
        );
        assert_eq!(
            blocking(BINDER_WRITE_READ, Some(&reading), None, true),
            None
        );
        assert_eq!(
            blocking(BINDER_WRITE_READ, Some(&writing), None, false),
            None
        );
        // A read whose buffer the caller has already filled wants nothing.
        let consumed = WriteRead {
            read_size: 256,
            read_consumed: 256,
            ..WriteRead::default()
        };
        assert_eq!(
            blocking(BINDER_WRITE_READ, Some(&consumed), None, false),
            None
        );
        assert_eq!(
            blocking(BINDER_FREEZE, None, Some(&[7, 1, 100]), false),
            Some(Blocking::Freeze)
        );
        assert_eq!(blocking(BINDER_FREEZE, None, Some(&[7, 1, 0]), false), None);
        assert_eq!(
            blocking(BINDER_FREEZE, None, Some(&[7, 0, 100]), false),
            None
        );
        // Binder's own other requests, and another driver's, wait for nothing.
        let max_threads = binder_request(1, 5, size_of::<u32>());
        assert_eq!(blocking(max_threads, Some(&reading), None, false), None);
        assert_eq!(blocking(0x4004_6205, Some(&reading), None, false), None);
    }

    /// The command codes are the uapi header's, from their `_IOR` parts.
    #[test]
    fn the_command_codes_encode_their_payload_sizes() {
        let ior = |nr: u32, size: u32| (2 << 30) | (size << 16) | (u32::from(b'r') << 8) | nr;
        assert_eq!(BR_TRANSACTION, ior(2, 64));
        assert_eq!(BR_TRANSACTION_SEC_CTX, ior(2, 72));
        assert_eq!(BR_REPLY, ior(3, 64));
        let iowr = (3u64 << 30) | (48 << 16) | (u64::from(b'b') << 8) | 1;
        assert_eq!(BINDER_WRITE_READ, iowr);
        let iow = (1u64 << 30) | (12 << 16) | (u64::from(b'b') << 8) | 14;
        assert_eq!(BINDER_FREEZE, iow);
        assert_eq!(BINDER_FREEZE, 0x400c_620e);
        assert_eq!(BINDER_WRITE_READ, 0xc030_6201);
        assert!(is_write_read(BINDER_WRITE_READ | 0xffff_ffff_0000_0000));
        assert!(!is_write_read(0x4004_6205));
    }

    /// A reply carrying one descriptor object, after commands without
    /// payload of interest, names it; a status-code reply names none.
    #[test]
    fn a_reply_names_the_descriptors_its_objects_hold() {
        let mut buffer = [0u8; 64];
        buffer[16..40].copy_from_slice(&fd_object(9));
        let offsets = [16u64];
        let noop = 0x0000_720c; // BR_NOOP, _IO('r', 12)
        let complete = 0x0000_7206; // BR_TRANSACTION_COMPLETE, _IO('r', 6)
        let mut commands = command(noop, &[]);
        commands.extend(command(complete, &[]));
        commands.extend(command(
            BR_REPLY,
            &transaction(0, &offsets, buffer.as_ptr() as usize),
        ));
        assert_eq!(
            installed_descriptors(&LocalMemory::new(), &commands),
            Ok(vec![9])
        );

        // A real status reply carries no objects. The flag itself is not
        // read, because the driver does not read it when it installs
        // descriptors: a reply that carries one still names it.
        let status = command(BR_REPLY, &transaction(TF_STATUS_CODE, &[], 0));
        assert_eq!(
            installed_descriptors(&LocalMemory::new(), &status),
            Ok(vec![])
        );
        let flagged = command(
            BR_REPLY,
            &transaction(TF_STATUS_CODE, &offsets, buffer.as_ptr() as usize),
        );
        assert_eq!(
            installed_descriptors(&LocalMemory::new(), &flagged),
            Ok(vec![9])
        );
    }

    /// A descriptor array is refused: Linux closes its numbers with the
    /// transaction's buffer, so a record of one would outlive it.
    #[test]
    fn a_descriptor_array_is_refused_rather_than_recorded() {
        let numbers: [u32; 3] = [4, 5, 6];
        let parent_data = {
            let mut data = vec![0u8; 8];
            numbers
                .iter()
                .for_each(|fd| data.extend_from_slice(&fd.to_ne_bytes()));
            data
        };
        let mut buffer = [0u8; 128];
        // The parent buffer object at 0.
        buffer[0..4].copy_from_slice(&BINDER_TYPE_PTR.to_ne_bytes());
        buffer[8..16].copy_from_slice(&(parent_data.as_ptr() as u64).to_ne_bytes());
        buffer[16..24].copy_from_slice(&(parent_data.len() as u64).to_ne_bytes());
        // The array object at 40: three descriptors at offset 8 of parent 0.
        buffer[40..44].copy_from_slice(&BINDER_TYPE_FDA.to_ne_bytes());
        buffer[48..56].copy_from_slice(&3u64.to_ne_bytes());
        buffer[56..64].copy_from_slice(&0u64.to_ne_bytes());
        buffer[64..72].copy_from_slice(&8u64.to_ne_bytes());
        let offsets = [0u64, 40];
        let commands = command(
            BR_TRANSACTION_SEC_CTX,
            &[
                transaction(0, &offsets, buffer.as_ptr() as usize),
                vec![0u8; 8],
            ]
            .concat(),
        );
        let refusal =
            installed_descriptors(&LocalMemory::new(), &commands).expect_err("a descriptor array");
        assert!(refusal.contains("Linux closes"), "{refusal}");
        // An empty array names nothing and is not refused.
        buffer[48..56].copy_from_slice(&0u64.to_ne_bytes());
        let commands = command(
            BR_TRANSACTION_SEC_CTX,
            &[
                transaction(0, &offsets, buffer.as_ptr() as usize),
                vec![0u8; 8],
            ]
            .concat(),
        );
        assert_eq!(
            installed_descriptors(&LocalMemory::new(), &commands),
            Ok(vec![])
        );
    }

    /// A buffer cut short names nothing and says so, rather than guessing.
    #[test]
    fn a_cut_short_buffer_names_nothing() {
        let commands = command(BR_REPLY, &[0u8; 10]);
        assert!(installed_descriptors(&LocalMemory::new(), &commands).is_err());
        assert!(installed_descriptors(&LocalMemory::new(), &[1, 2]).is_err());
    }

    /// The whole call: a read that consumed nothing names nothing, one that
    /// claims more than its size is refused, and one with a transaction
    /// names its descriptors.
    #[test]
    fn a_write_read_names_what_its_read_delivered() {
        let mut buffer = [0u8; 64];
        buffer[0..24].copy_from_slice(&fd_object(12));
        let offsets = [0u64];
        let commands = command(
            BR_TRANSACTION,
            &transaction(0, &offsets, buffer.as_ptr() as usize),
        );
        let call = |read_size: u64, read_consumed: u64| {
            [0, 0, 0, read_size, read_consumed, commands.as_ptr() as u64]
        };
        let memory = LocalMemory::new();
        let words = call(256, commands.len() as u64);

        assert_eq!(
            delivered(&memory, words.as_ptr() as usize, 0),
            Delivered {
                fds: vec![12],
                messages: vec![crate::syscalls::scm_rights::DeliveredMessage {
                    count: 1,
                    truncated: false,
                    bytes: None,
                }],
                refused: None
            }
        );
        let words = call(256, 0);
        assert_eq!(
            delivered(&memory, words.as_ptr() as usize, 0),
            Delivered::default()
        );
        let words = call(4, commands.len() as u64);
        assert!(
            delivered(&memory, words.as_ptr() as usize, 0)
                .refused
                .is_some()
        );

        // An exactly full command buffer is parsed through its last FD.
        // Cutting a larger one on a command boundary must refuse explicitly:
        // otherwise its valid completion prefix would hide the installed FD.
        let noop = 0x0000_720cu32; // BR_NOOP, _IO('r', 12)
        let complete = 0x0000_7206u32; // BR_TRANSACTION_COMPLETE, _IO('r', 6)
        let mut at_limit = noop.to_ne_bytes().to_vec();
        at_limit.extend(
            complete
                .to_ne_bytes()
                .repeat((MAX_COMMAND_BYTES - commands.len() - 4) / 4),
        );
        at_limit.extend_from_slice(&commands);
        assert_eq!(at_limit.len(), MAX_COMMAND_BYTES);
        let words = [
            0,
            0,
            0,
            at_limit.len() as u64,
            at_limit.len() as u64,
            at_limit.as_ptr() as u64,
        ];
        let expected = delivered(&memory, words.as_ptr() as usize, 0);
        assert_eq!(expected.fds, vec![12]);
        assert!(expected.refused.is_none());

        let mut over_limit = noop.to_ne_bytes().to_vec();
        over_limit.extend(complete.to_ne_bytes().repeat(MAX_COMMAND_BYTES / 4 - 1));
        over_limit.extend_from_slice(&commands);
        let words = [
            0,
            0,
            0,
            over_limit.len() as u64,
            over_limit.len() as u64,
            over_limit.as_ptr() as u64,
        ];
        assert_eq!(
            delivered(&memory, words.as_ptr() as usize, 0),
            refused("its newly returned commands exceed Detcore's read limit")
        );
        // Old commands do not consume this call's budget or get parsed again.
        // Only the small FD transaction after the large old prefix is new.
        assert_eq!(
            consumed_before(
                &memory,
                [0, 0, 0, 0, MAX_COMMAND_BYTES as u64, 0].as_ptr() as usize
            ),
            MAX_COMMAND_BYTES as u64
        );
        assert_eq!(
            delivered(&memory, words.as_ptr() as usize, MAX_COMMAND_BYTES as u64),
            expected
        );
    }
}
