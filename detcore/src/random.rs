/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Shared guest random-state and memory operations, independent of a backend.
//! These synchronous operations preserve draws even when a later write fails.

use std::collections::hash_map::DefaultHasher;
use std::hash::Hash;
use std::hash::Hasher;

use rand::RngExt as _;
use rand::SeedableRng as _;
use rand_pcg::Pcg64Mcg;
use reverie::Error;
use reverie::syscalls::AddrMut;
use reverie::syscalls::Errno;
use reverie::syscalls::Getrandom;
use reverie::syscalls::MemoryAccess;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest as _;
use sha2::Sha256;

use crate::detlog;
use crate::types::DetTid;

pub(crate) const RANDOM_FILL_CHUNK_BYTES: usize = 4096;

/// A user-access random copy failed for a reason other than a guest fault.
///
/// Memory may already contain a copied prefix, or even the entire attempted
/// write. This is a failed run, not a guest errno or a successful short read.
#[derive(Debug)]
pub struct RandomCopyFailure {
    errno: Errno,
}

impl RandomCopyFailure {
    /// The exact error returned by the backend's user-access copy.
    pub fn errno(&self) -> Errno {
        self.errno
    }
}

impl std::fmt::Display for RandomCopyFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "random user-access copy failed: {}", self.errno)?;
        if self.errno == Errno::EPERM {
            f.write_str(
                "; for a non-dumpable ptrace guest, retry with Hermit's default namespace \
                 configuration (omit --no-namespace); embedders must check tracing permissions \
                 in the guest's user namespace",
            )?;
        }
        Ok(())
    }
}

impl std::error::Error for RandomCopyFailure {}

pub(crate) fn copy_error(error: Errno) -> Error {
    match error {
        Errno::EFAULT => Error::Errno(error),
        errno => Error::Tool(anyhow::Error::new(RandomCopyFailure { errno })),
    }
}

pub(crate) fn is_copy_failure(error: &Error) -> bool {
    matches!(error, Error::Tool(inner) if inner.is::<RandomCopyFailure>())
}

/// Construct the root guest stream from its configured seed, without creating
/// a thread or discovering any process metadata.
pub fn root_prng(seed: u64) -> Pcg64Mcg {
    Pcg64Mcg::seed_from_u64(seed)
}

/// Fixed maximum for the backend-independent initial random-state handoff.
pub const MAX_INITIAL_STATE_BYTES: usize = 4096;

/// Identity of the sole initial image whose real auxv was already written.
/// Backends must authenticate this identity before constructing a handoff.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InitialImage {
    /// Initial physical process ID in the backend's guest PID namespace.
    pub pid: i32,
    /// Linux process generation from field22 of this process's proc stat.
    pub start_time_ticks: u64,
    /// Actual writable16-byte target from the authenticated initial auxv.
    pub at_random: usize,
}

impl InitialImage {
    fn validate(self) -> Result<(), Errno> {
        if self.pid <= 0
            || self.start_time_ticks == 0
            || self.at_random == 0
            || self.at_random.checked_add(16).is_none()
        {
            return Err(Errno::EPROTO);
        }
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct InitialRandomState {
    version: u32,
    configuration: [u8; 32],
    image: InitialImage,
    state: LoaderState,
}

/// Authenticated loader result. Continuations carry no random state and must
/// leave the ordinary newly constructed thread completely unchanged.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum LoaderState {
    /// Initial dynamic image: the actual auxv write and early requests ran.
    InitialRandom {
        /// Stream after the real auxv and getrandom operations.
        prng: Pcg64Mcg,
    },
    /// A later real kernel exec observed in this owned process lineage.
    ObservedExecContinuation,
    /// The held initial program takes the existing static-loader path.
    InitialStaticLegacy,
}

fn configuration_identity(config: &crate::Config) -> Result<[u8; 32], Errno> {
    struct HashWriter(Sha256);
    impl std::io::Write for HashWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.update(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut writer = HashWriter(Sha256::new());
    writer.0.update(b"hermit-initial-random-state-v1\0");
    writer.0.update(crate::config_wire_fingerprint());
    // Include actual effective values, not merely Config's type fingerprint.
    // Stream into the digest instead of allocating a second config copy.
    serde_json::to_writer(&mut writer, config).map_err(|_| Errno::EPROTO)?;
    Ok(writer.0.finalize().into())
}

/// Encode only the actual PRNG and completed auxv identity. No clock, metadata,
/// chaos RNG, scheduler state or request history is transferred.
pub fn encode_initial_state(
    config: &crate::Config,
    image: InitialImage,
    prng: &Pcg64Mcg,
) -> Result<Vec<u8>, Errno> {
    image.validate()?;
    let value = InitialRandomState {
        version: 1,
        configuration: configuration_identity(config)?,
        image,
        state: LoaderState::InitialRandom { prng: prng.clone() },
    };
    encode_state(value)
}

/// Encode a supervisor-authenticated legacy path without transferring RNG,
/// clock, metadata or any auxiliary-vector completion fact.
pub fn encode_continuation(
    config: &crate::Config,
    image: InitialImage,
    state: LoaderState,
) -> Result<Vec<u8>, Errno> {
    image.validate()?;
    if matches!(state, LoaderState::InitialRandom { .. }) {
        return Err(Errno::EPROTO);
    }
    encode_state(InitialRandomState {
        version: 1,
        configuration: configuration_identity(config)?,
        image,
        state,
    })
}

fn encode_state(value: InitialRandomState) -> Result<Vec<u8>, Errno> {
    let bytes = serde_json::to_vec(&value).map_err(|_| Errno::EPROTO)?;
    if bytes.is_empty() || bytes.len() > MAX_INITIAL_STATE_BYTES {
        return Err(Errno::EOVERFLOW);
    }
    Ok(bytes)
}

/// Decode an exact canonical, configuration- and image-bound loader result.
/// The backend must obtain these bytes only from its authenticated callback.
pub fn decode_loader_state(
    bytes: &[u8],
    config: &crate::Config,
    expected: InitialImage,
) -> Result<LoaderState, Errno> {
    expected.validate()?;
    if bytes.is_empty() || bytes.len() > MAX_INITIAL_STATE_BYTES {
        return Err(Errno::EPROTO);
    }
    let value: InitialRandomState = serde_json::from_slice(bytes).map_err(|_| Errno::EPROTO)?;
    if value.version != 1
        || value.configuration != configuration_identity(config)?
        || value.image != expected
        || serde_json::to_vec(&value).map_err(|_| Errno::EPROTO)? != bytes
    {
        // Re-encoding also rejects trailing whitespace/bytes and alternate
        // representations. A rejected handoff is never replaced with a seed.
        return Err(Errno::EPROTO);
    }
    Ok(value.state)
}

pub(crate) fn decode_initial_state(
    bytes: &[u8],
    config: &crate::Config,
    expected: InitialImage,
) -> Result<Pcg64Mcg, Errno> {
    match decode_loader_state(bytes, config, expected)? {
        LoaderState::InitialRandom { prng } => Ok(prng),
        LoaderState::ObservedExecContinuation | LoaderState::InitialStaticLegacy => {
            Err(Errno::EPROTO)
        }
    }
}

const GETRANDOM_ALLOWED_FLAGS: u32 = libc::GRND_NONBLOCK | libc::GRND_RANDOM | libc::GRND_INSECURE;

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(#545): Confirm getrandom flag, stream, and fault semantics.
pub(crate) fn validate_getrandom_flags(flags: usize) -> Result<(), Errno> {
    let flags = flags as u32;
    let random = flags & libc::GRND_RANDOM != 0;
    let insecure = flags & libc::GRND_INSECURE != 0;

    if flags & !GETRANDOM_ALLOWED_FLAGS != 0 || (random && insecure) {
        Err(Errno::EINVAL)
    } else {
        Ok(())
    }
}

// Linux's import_ubuf clamps getrandom requests to MAX_RW_COUNT on x86_64.
pub(crate) const GETRANDOM_MAX_BYTES: usize = (i32::MAX as usize) & !4095;

pub(crate) fn getrandom_request_len(requested: usize) -> usize {
    requested.min(GETRANDOM_MAX_BYTES)
}

pub(crate) fn write_random_chunk(
    memory: &mut impl MemoryAccess,
    remote_buf: AddrMut<u8>,
    local_buf: &[u8],
) -> Result<usize, Errno> {
    const PTRACE_WORD_SPLIT: usize = std::mem::size_of::<u64>() / 2;

    if local_buf.len() != std::mem::size_of::<u64>() {
        return memory.write_with_user_access(remote_buf, local_buf);
    }

    // Preserve the existing eight-byte split and its prefix semantics. Every
    // length now uses the explicit user-access capability; debugger writes can
    // bypass protection for other sizes too (including the KVM backend).
    let first = memory.write_with_user_access(remote_buf, &local_buf[..PTRACE_WORD_SPLIT])?;
    if first < PTRACE_WORD_SPLIT {
        return Ok(first);
    }
    let Some(second_buf) = remote_buf
        .as_raw()
        .checked_add(PTRACE_WORD_SPLIT)
        .and_then(AddrMut::<u8>::from_raw)
    else {
        return Ok(first);
    };
    match memory.write_with_user_access(second_buf, &local_buf[PTRACE_WORD_SPLIT..]) {
        Ok(second) => Ok(first + second),
        Err(Errno::EFAULT) => Ok(first),
        Err(error) => Err(error),
    }
}

/// Fill guest memory from the same stream/chunk/write algorithm used by the
/// normal Detcore handler. No syscall/scheduler accounting is performed here.
pub fn fill_bytes(
    prng: &mut Pcg64Mcg,
    mut memory: impl MemoryAccess,
    remote_buf: AddrMut<u8>,
    len: usize,
    dettid: DetTid,
    source: &str,
) -> Result<usize, Error> {
    let mut local_words = [0_u64; RANDOM_FILL_CHUNK_BYTES / std::mem::size_of::<u64>()];
    let mut hasher = DefaultHasher::new();
    let mut written = 0;

    while written < len {
        let remote_chunk = match remote_buf
            .as_raw()
            .checked_add(written)
            .and_then(AddrMut::<u8>::from_raw)
        {
            Some(address) => address,
            None if written == 0 => return Err(Errno::EFAULT.into()),
            None => break,
        };
        let chunk_len = (len - written).min(RANDOM_FILL_CHUNK_BYTES);
        // Keep the existing aligned scratch and full attempted-chunk draw.
        let local_buf = unsafe {
            std::slice::from_raw_parts_mut(local_words.as_mut_ptr().cast::<u8>(), chunk_len)
        };
        prng.fill(local_buf);
        let n = match write_random_chunk(&mut memory, remote_chunk, local_buf) {
            Ok(n) => n,
            Err(Errno::EFAULT) if written > 0 => break,
            Err(error) => return Err(copy_error(error)),
        };
        if n == 0 {
            if written == 0 {
                return Err(Errno::EFAULT.into());
            }
            break;
        }
        if cfg!(debug_assertions) {
            Hash::hash_slice(&local_buf[..n], &mut hasher);
        }
        written += n;
        if n < chunk_len {
            break;
        }
    }

    if cfg!(debug_assertions) {
        detlog!(
            "[dtid {}] USER RAND [{}] Filled guest memory with {} random bytes, hash of bytes: {}",
            dettid,
            source,
            written,
            hasher.finish()
        );
    }
    Ok(written)
}

/// Apply getrandom's existing flag, length, null-buffer and fill semantics.
pub fn getrandom(
    prng: &mut Pcg64Mcg,
    memory: impl MemoryAccess,
    dettid: DetTid,
    call: Getrandom,
) -> Result<i64, Error> {
    validate_getrandom_flags(call.flags())?;
    let len = getrandom_request_len(call.buflen());
    if len == 0 {
        return Ok(0);
    }
    let buf = call.buf().ok_or(Errno::EFAULT)?;
    fill_bytes(prng, memory, buf, len, dettid, "getrandom").map(|n| n as i64)
}

/// Draw and write the actual initial auxv bytes. A write failure preserves the
/// consumed PRNG state, as in the normal post-exec callback.
pub fn initialize_auxv(
    prng: &mut Pcg64Mcg,
    mut memory: impl MemoryAccess,
    pointer: AddrMut<u8>,
    dettid: DetTid,
) -> Result<(), Errno> {
    let bytes: [u8; 16] = prng.random();
    detlog!(
        "[post_exec, dtid {}] init auxv AT_RANDOM value to {:?}",
        dettid,
        bytes
    );
    memory.write_value(pointer.cast::<[u8; 16]>(), &bytes)
}

#[cfg(test)]
mod tests {
    include!("random/user_access_tests.rs");
    use std::io::IoSlice;
    use std::io::IoSliceMut;

    use reverie::syscalls::Syscall;
    use reverie::syscalls::SyscallArgs;
    use reverie::syscalls::Sysno;

    use super::*;

    #[derive(Clone, Copy)]
    struct OwnMemory;

    impl MemoryAccess for OwnMemory {
        fn write_with_user_access(
            &mut self,
            addr: AddrMut<u8>,
            bytes: &[u8],
        ) -> Result<usize, Errno> {
            addr.as_raw()
                .checked_add(bytes.len())
                .ok_or(Errno::EFAULT)?;
            if bytes.is_empty() {
                return Ok(0);
            }
            let local = libc::iovec {
                iov_base: bytes.as_ptr().cast_mut().cast(),
                iov_len: bytes.len(),
            };
            let remote = libc::iovec {
                iov_base: addr.as_raw() as *mut libc::c_void,
                iov_len: bytes.len(),
            };
            let n = unsafe { libc::process_vm_writev(libc::getpid(), &local, 1, &remote, 1, 0) };
            if n < 0 {
                Err(Errno::last())
            } else {
                Ok(n as usize)
            }
        }
        fn read_vectored(
            &self,
            remote: &[IoSlice],
            local: &mut [IoSliceMut],
        ) -> Result<usize, Errno> {
            let n = unsafe {
                libc::process_vm_readv(
                    libc::getpid(),
                    local.as_ptr().cast(),
                    local.len() as _,
                    remote.as_ptr().cast(),
                    remote.len() as _,
                    0,
                )
            };
            if n < 0 {
                Err(Errno::last())
            } else {
                Ok(n as usize)
            }
        }
        fn write_vectored(
            &mut self,
            local: &[IoSlice],
            remote: &mut [IoSliceMut],
        ) -> Result<usize, Errno> {
            let n = unsafe {
                libc::process_vm_writev(
                    libc::getpid(),
                    local.as_ptr().cast(),
                    local.len() as _,
                    remote.as_ptr().cast(),
                    remote.len() as _,
                    0,
                )
            };
            if n < 0 {
                Err(Errno::last())
            } else {
                Ok(n as usize)
            }
        }
    }

    struct SecondHalfFailure {
        error: Errno,
        bytes: [u8; 8],
        writes: Vec<(usize, Vec<u8>)>,
    }

    impl MemoryAccess for SecondHalfFailure {
        fn read_vectored(
            &self,
            _remote: &[IoSlice],
            _local: &mut [IoSliceMut],
        ) -> Result<usize, Errno> {
            panic!("random copying must not read guest memory")
        }

        fn write_vectored(
            &mut self,
            _local: &[IoSlice],
            _remote: &mut [IoSliceMut],
        ) -> Result<usize, Errno> {
            panic!("random copying must use the user-access capability")
        }

        fn write_with_user_access(
            &mut self,
            addr: AddrMut<u8>,
            buf: &[u8],
        ) -> Result<usize, Errno> {
            self.writes.push((addr.as_raw(), buf.to_vec()));
            match self.writes.len() {
                1 => {
                    assert_eq!(addr.as_raw(), 0x1000);
                    assert_eq!(buf.len(), 4);
                    self.bytes[..4].copy_from_slice(buf);
                    Ok(4)
                }
                2 => {
                    assert_eq!(addr.as_raw(), 0x1004);
                    assert_eq!(buf.len(), 4);
                    Err(self.error)
                }
                _ => panic!("random copying retried a failed write"),
            }
        }
    }

    #[test]
    fn eight_byte_random_copy_distinguishes_faults_from_backend_errors() {
        for (error, expected) in [(Errno::EFAULT, Ok(4)), (Errno::EIO, Err(Errno::EIO))] {
            let mut memory = SecondHalfFailure {
                error,
                bytes: [0xa5; 8],
                writes: Vec::new(),
            };
            let address = AddrMut::from_raw(0x1000).unwrap();

            assert_eq!(
                write_random_chunk(&mut memory, address, &[1, 2, 3, 4, 5, 6, 7, 8]),
                expected
            );
            assert_eq!(memory.bytes, [1, 2, 3, 4, 0xa5, 0xa5, 0xa5, 0xa5]);
            assert_eq!(
                memory.writes,
                [(0x1000, vec![1, 2, 3, 4]), (0x1004, vec![5, 6, 7, 8])]
            );
        }
    }

    struct Pages {
        address: *mut u8,
        size: usize,
    }
    impl Pages {
        fn new() -> Self {
            let size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) as usize };
            assert!(size >= 4096);
            let address = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    2 * size,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                    -1,
                    0,
                )
            };
            assert_ne!(address, libc::MAP_FAILED);
            unsafe {
                std::ptr::write_bytes(address.cast::<u8>(), 0xa5, 2 * size);
            }
            assert_eq!(
                unsafe { libc::mprotect(address.add(size), size, libc::PROT_READ) },
                0
            );
            Self {
                address: address.cast(),
                size,
            }
        }
        fn address(&self, offset: usize) -> AddrMut<'static, u8> {
            assert!(offset < self.size * 2);
            AddrMut::from_raw(self.address as usize + offset).unwrap()
        }
        fn bytes(&self, offset: usize, length: usize) -> Vec<u8> {
            assert!(offset + length <= 2 * self.size);
            unsafe { std::slice::from_raw_parts(self.address.add(offset), length) }.to_vec()
        }
    }
    impl Drop for Pages {
        fn drop(&mut self) {
            assert_eq!(
                unsafe { libc::munmap(self.address.cast(), 2 * self.size) },
                0
            );
        }
    }

    fn call(buffer: usize, length: usize, flags: usize) -> Getrandom {
        let Syscall::Getrandom(call) = Syscall::from_raw(
            Sysno::getrandom,
            SyscallArgs::new(buffer, length, flags, 0, 0, 0),
        ) else {
            unreachable!()
        };
        call
    }
    // Existing guest-errno cases must still be guest errnos. A terminal Tool
    // error here is a failed assertion, never a successful errno projection.
    fn guest_getrandom(
        prng: &mut Pcg64Mcg,
        memory: impl MemoryAccess,
        tid: DetTid,
        call: Getrandom,
    ) -> Result<i64, Errno> {
        super::getrandom(prng, memory, tid, call).map_err(|error| match error {
            Error::Errno(errno) => errno,
            other => panic!("unexpected terminal failure in guest-errno companion: {other:?}"),
        })
    }
    fn same_state(a: &Pcg64Mcg, b: &Pcg64Mcg) {
        assert_eq!(
            serde_json::to_vec(a).unwrap(),
            serde_json::to_vec(b).unwrap()
        );
    }

    #[test]
    fn initial_handoff_preserves_unrelated_state_and_consumes_only_auxv_fact() {
        let pages = Pages::new();
        let config = crate::Config::default();
        let tid = DetTid::from_raw(3);
        let image = InitialImage {
            pid: 3,
            start_time_ticks: 1234,
            at_random: pages.address(0).as_raw(),
        };
        let mut stream = root_prng(config.rng_seed());
        initialize_auxv(&mut stream, OwnMemory, pages.address(0), tid).unwrap();
        guest_getrandom(
            &mut stream,
            OwnMemory,
            tid,
            call(pages.address(32).as_raw(), 8, 1),
        )
        .unwrap();
        let encoded = encode_initial_state(&config, image, &stream).unwrap();
        same_state(
            &decode_initial_state(&encoded, &config, image).unwrap(),
            &stream,
        );
        for bad in [
            Vec::new(),
            [encoded.as_slice(), b" "].concat(),
            String::from_utf8(encoded.clone())
                .unwrap()
                .replace("\"version\":1", "\"version\":2")
                .into_bytes(),
        ] {
            assert!(decode_initial_state(&bad, &config, image).is_err());
        }
        for wrong in [
            InitialImage { pid: 4, ..image },
            InitialImage {
                start_time_ticks: 1235,
                ..image
            },
            InitialImage {
                at_random: image.at_random + 16,
                ..image
            },
        ] {
            assert!(decode_initial_state(&encoded, &config, wrong).is_err());
        }
        let mut different = config.clone();
        different.virtualize_time = !different.virtualize_time;
        assert!(decode_initial_state(&encoded, &different, image).is_err());

        for kind in [
            LoaderState::ObservedExecContinuation,
            LoaderState::InitialStaticLegacy,
        ] {
            let legacy = encode_continuation(&config, image, kind).unwrap();
            assert!(matches!(
                decode_loader_state(&legacy, &config, image).unwrap(),
                LoaderState::ObservedExecContinuation | LoaderState::InitialStaticLegacy
            ));
            // Continuation is not a random state and cannot be applied through
            // the initial-state API, even to an otherwise eligible normal root.
            assert!(matches!(
                decode_initial_state(&legacy, &config, image),
                Err(Errno::EPROTO)
            ));
            let mut untouched = crate::tool_local::ThreadState::new(tid, &config, ());
            let prng_before = serde_json::to_vec(&untouched.prng).unwrap();
            let chaos_before = serde_json::to_vec(&untouched.chaos_prng).unwrap();
            let clock_before = serde_json::to_vec(&untouched.thread_logical_time).unwrap();
            let metadata_before = std::sync::Arc::clone(&untouched.file_metadata);
            let memory_before = std::sync::Arc::clone(&untouched.memory_metadata);
            assert_eq!(
                untouched.apply_initial_random_state(&legacy, &config, image),
                Err(Errno::EPROTO)
            );
            assert_eq!(serde_json::to_vec(&untouched.prng).unwrap(), prng_before);
            assert_eq!(
                serde_json::to_vec(&untouched.chaos_prng).unwrap(),
                chaos_before
            );
            assert_eq!(
                serde_json::to_vec(&untouched.thread_logical_time).unwrap(),
                clock_before
            );
            assert!(std::sync::Arc::ptr_eq(
                &untouched.file_metadata,
                &metadata_before
            ));
            assert!(std::sync::Arc::ptr_eq(
                &untouched.memory_metadata,
                &memory_before
            ));
            assert!(
                !untouched
                    .complete_initial_random_auxv(Some(image.at_random))
                    .unwrap()
            );
            assert!(matches!(
                decode_loader_state(
                    &legacy,
                    &config,
                    InitialImage {
                        start_time_ticks: 1235,
                        ..image
                    }
                ),
                Err(Errno::EPROTO)
            ));
        }

        let mut state = crate::tool_local::ThreadState::new(tid, &config, ());
        let chaos = serde_json::to_vec(&state.chaos_prng).unwrap();
        let clock = serde_json::to_vec(&state.thread_logical_time).unwrap();
        let metadata = std::sync::Arc::clone(&state.file_metadata);
        let memory = std::sync::Arc::clone(&state.memory_metadata);
        let pedigree = state.pedigree.clone();
        state.committed_clock_value = 47;
        state
            .apply_initial_random_state(&encoded, &config, image)
            .unwrap();
        same_state(&state.prng, &stream);
        assert_eq!(serde_json::to_vec(&state.chaos_prng).unwrap(), chaos);
        assert_eq!(
            serde_json::to_vec(&state.thread_logical_time).unwrap(),
            clock
        );
        assert!(std::sync::Arc::ptr_eq(&state.file_metadata, &metadata));
        assert!(std::sync::Arc::ptr_eq(&state.memory_metadata, &memory));
        assert_eq!(state.pedigree.raw(), pedigree.raw());
        assert_eq!(state.committed_clock_value, 47);
        assert!(
            state
                .apply_initial_random_state(&encoded, &config, image)
                .is_err()
        );
        assert!(
            state
                .complete_initial_random_auxv(Some(image.at_random + 1))
                .is_err()
        );
        // Libc/guest writes after the early acknowledgement must survive the
        // normal late post-exec completion. Completion performs no memory I/O.
        OwnMemory
            .write_exact(pages.address(0), &[0x7c; 16])
            .unwrap();
        // handle_post_exec sets this before consuming the completion fact.
        state.past_global_first_execve = true;
        assert!(
            state
                .complete_initial_random_auxv(Some(image.at_random))
                .unwrap()
        );
        assert_eq!(pages.bytes(0, 16), [0x7c; 16]);
        same_state(&state.prng, &stream);
        assert!(
            !state
                .complete_initial_random_auxv(Some(image.at_random))
                .unwrap()
        );
        assert!(
            state
                .apply_initial_random_state(&encoded, &config, image)
                .is_err()
        );
    }

    #[test]
    fn shared_random_preserves_auxv_and_fault_semantics() {
        let pages = Pages::new();
        let tid = DetTid::from_raw(3);
        let mut actual = root_prng(0);
        let mut expected = Pcg64Mcg::seed_from_u64(0);
        let auxv: [u8; 16] = expected.random();
        initialize_auxv(&mut actual, OwnMemory, pages.address(0), tid).unwrap();
        assert_eq!(pages.bytes(0, 16), auxv);
        same_state(&actual, &expected);

        for (buffer, len, flags, result) in [
            (0, 0, 0, Ok(0)),
            (0, 8, 0, Err(Errno::EFAULT)),
            (
                pages.address(32).as_raw(),
                16,
                0x8000_0001,
                Err(Errno::EINVAL),
            ),
        ] {
            assert_eq!(
                guest_getrandom(&mut actual, OwnMemory, tid, call(buffer, len, flags)),
                result
            );
            same_state(&actual, &expected);
            assert_eq!(pages.bytes(32, 16), [0xa5; 16]);
        }
        for len in [8, 16, 32, 4096] {
            let mut bytes = vec![0; len];
            expected.fill(&mut bytes[..]);
            assert_eq!(
                guest_getrandom(
                    &mut actual,
                    OwnMemory,
                    tid,
                    call(pages.address(0).as_raw(), len, 1)
                ),
                Ok(len as i64)
            );
            assert_eq!(pages.bytes(0, len), bytes);
            same_state(&actual, &expected);
        }
        // A fully read-only destination consumes the generated chunk before
        // its first write fails; it must not use ptrace's protection bypass.
        let mut discarded = [0u8; 8];
        expected.fill(&mut discarded[..]);
        assert_eq!(
            guest_getrandom(
                &mut actual,
                OwnMemory,
                tid,
                call(pages.address(pages.size).as_raw(), 8, 0)
            ),
            Err(Errno::EFAULT)
        );
        assert_eq!(pages.bytes(pages.size, 8), [0xa5; 8]);
        same_state(&actual, &expected);

        // A cross-page short write returns the writable prefix, but the PRNG
        // has generated the complete requested chunk, exactly as before.
        for len in [8, 16] {
            let mut bytes = vec![0; len];
            expected.fill(&mut bytes[..]);
            assert_eq!(
                guest_getrandom(
                    &mut actual,
                    OwnMemory,
                    tid,
                    call(pages.address(pages.size - 4).as_raw(), len, 0)
                ),
                Ok(4)
            );
            assert_eq!(pages.bytes(pages.size - 4, 4), bytes[..4]);
            assert_eq!(pages.bytes(pages.size, len - 4), vec![0xa5; len - 4]);
            same_state(&actual, &expected);
        }
    }
}
