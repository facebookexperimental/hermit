/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Linux's bounded, two-phase import for native-width readv arrays.

use reverie::Error;
use reverie::syscalls::Addr;
use reverie::syscalls::Errno;
use reverie::syscalls::MemoryAccess;

pub(crate) const MAX_RW_COUNT: usize = 0x7fff_f000;
const KVM_USER_LIMIT: usize = (1_usize << 47) - 4096;

/// A copied descriptor with its length capped by Linux's aggregate read limit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ImportedIovec {
    pub(crate) base: usize,
    pub(crate) len: usize,
}

/// Structural address policy, independent of whether pages are mapped.
#[derive(Clone, Copy)]
pub(crate) enum UserAddressPolicy {
    Native,
    Kvm,
}

impl UserAddressPolicy {
    pub(crate) fn validate(self, iovecs: &[ImportedIovec]) -> Result<(), Error> {
        match self {
            Self::Kvm => validate_ranges(iovecs, KVM_USER_LIMIT).map_err(Error::from),
            Self::Native => {
                let local: Vec<_> = iovecs
                    .iter()
                    .map(|iov| libc::iovec {
                        iov_base: iov.base as *mut libc::c_void,
                        iov_len: iov.len,
                    })
                    .collect();
                // Linux imports the local vectors before observing zero remote
                // vectors. That zero returns before PID lookup, page pinning,
                // mapping checks or copying (mm/process_vm_access.c). Asking
                // the running kernel preserves native LA57 without guessing
                // its TASK_SIZE from CPU capabilities. KVM uses its own limit.
                let result = unsafe {
                    libc::process_vm_readv(
                        libc::getpid(),
                        local.as_ptr(),
                        local.len() as libc::c_ulong,
                        std::ptr::null(),
                        0,
                        0,
                    )
                };
                native_import_result(result, Errno::last())
            }
        }
    }
}

fn native_import_result(result: isize, error: Errno) -> Result<(), Error> {
    match (result, error) {
        (0, _) => Ok(()),
        (-1, Errno::EFAULT) => Err(Errno::EFAULT.into()),
        _ => Err(Error::Tool(anyhow::anyhow!(
            "native iovec range validation failed: result={result}, errno={error}"
        ))),
    }
}

fn validate_ranges(iovecs: &[ImportedIovec], limit: usize) -> Result<(), Errno> {
    for iov in iovecs {
        // import_iovec uses import_ubuf only for one descriptor. Multiple
        // descriptors validate original ranges, even beyond the eventual cap.
        let len = if iovecs.len() == 1 {
            iov.len.min(MAX_RW_COUNT)
        } else {
            iov.len
        };
        if len > limit || iov.base > limit - len {
            return Err(Errno::EFAULT);
        }
    }
    Ok(())
}

fn cap_lengths(iovecs: &mut [ImportedIovec]) {
    let mut remaining = MAX_RW_COUNT;
    for iov in iovecs {
        iov.len = iov.len.min(remaining);
        remaining -= iov.len;
    }
}

/// Import every descriptor before copying output. Callers must check fd/access
/// first; guest mapping/protection failures are left for the later data copy.
pub(crate) fn import_read_iovecs(
    memory: &impl MemoryAccess,
    address: usize,
    raw_count: usize,
    policy: UserAddressPolicy,
) -> Result<Vec<ImportedIovec>, Error> {
    // Linux's unsigned-long vlen is narrowed by import_iovec's unsigned nr_segs.
    let count = raw_count as u32 as usize;
    if count == 0 {
        return Ok(Vec::new());
    }
    if count > libc::UIO_MAXIOV as usize {
        return Err(Errno::EINVAL.into());
    }
    let array_bytes = count * std::mem::size_of::<libc::iovec>();
    // Two descriptors prevent the single-vector MAX_RW_COUNT special case
    // from shortening the array-range check.
    policy.validate(&[
        ImportedIovec {
            base: address,
            len: array_bytes,
        },
        ImportedIovec { base: 0, len: 0 },
    ])?;
    let mut imported = Vec::with_capacity(count);
    for index in 0..count {
        let pointer = address
            .checked_add(index * std::mem::size_of::<libc::iovec>())
            .and_then(Addr::<u8>::from_raw)
            .ok_or(Errno::EFAULT)?;
        let mut bytes = [0_u8; std::mem::size_of::<libc::iovec>()];
        memory
            .read_exact_with_user_access(pointer, &mut bytes)
            .map_err(crate::random::copy_error)?;
        let word = std::mem::size_of::<usize>();
        let base = usize::from_ne_bytes(bytes[..word].try_into().unwrap());
        let len = usize::from_ne_bytes(bytes[word..].try_into().unwrap());
        if len > isize::MAX as usize {
            return Err(Errno::EINVAL.into());
        }
        imported.push(ImportedIovec { base, len });
    }
    policy.validate(&imported)?;
    cap_lengths(&mut imported);
    Ok(imported)
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::io::IoSlice;
    use std::io::IoSliceMut;

    use super::*;

    struct ArrayMemory {
        vectors: Vec<ImportedIovec>,
        reads: Cell<usize>,
        read_error: Option<(usize, Errno)>,
    }

    impl ArrayMemory {
        fn new(vectors: Vec<ImportedIovec>) -> Self {
            Self {
                vectors,
                reads: Cell::new(0),
                read_error: None,
            }
        }
    }

    impl MemoryAccess for ArrayMemory {
        fn read_vectored(&self, _: &[IoSlice], _: &mut [IoSliceMut]) -> Result<usize, Errno> {
            panic!("import must use reads that honor user protections")
        }

        fn write_vectored(&mut self, _: &[IoSlice], _: &mut [IoSliceMut]) -> Result<usize, Errno> {
            panic!("import must not write memory")
        }

        fn read_exact_with_user_access<'a, A>(
            &self,
            address: A,
            bytes: &mut [u8],
        ) -> Result<(), Errno>
        where
            A: Into<Addr<'a, u8>>,
        {
            self.reads.set(self.reads.get() + 1);
            if let Some((attempt, error)) = self.read_error
                && self.reads.get() == attempt
            {
                return Err(error);
            }
            let index = (address.into().as_raw() - 0x1000) / 16;
            let vector = self.vectors.get(index).ok_or(Errno::EFAULT)?;
            bytes[..8].copy_from_slice(&vector.base.to_ne_bytes());
            bytes[8..].copy_from_slice(&vector.len.to_ne_bytes());
            Ok(())
        }
    }

    fn errno<T>(result: Result<T, Error>) -> Errno {
        match result {
            Err(Error::Errno(error)) => error,
            _ => panic!("expected guest errno"),
        }
    }

    #[test]
    fn rng_iovecs_normalize_count_before_limits_or_memory() {
        let memory = ArrayMemory::new(vec![ImportedIovec {
            base: 0x2000,
            len: 3,
        }]);
        assert!(
            import_read_iovecs(&memory, usize::MAX, 1 << 32, UserAddressPolicy::Kvm)
                .unwrap()
                .is_empty()
        );
        assert_eq!(memory.reads.get(), 0);
        assert_eq!(
            errno(import_read_iovecs(
                &memory,
                0,
                (1 << 32) | 1025,
                UserAddressPolicy::Kvm
            )),
            Errno::EINVAL
        );
        assert_eq!(memory.reads.get(), 0);
        assert_eq!(
            import_read_iovecs(&memory, 0x1000, (1 << 32) | 1, UserAddressPolicy::Kvm).unwrap(),
            memory.vectors
        );
        assert_eq!(memory.reads.get(), 1);
    }

    #[test]
    fn rng_iovecs_array_range_precedes_lengths_but_each_length_precedes_next_read() {
        let memory = ArrayMemory::new(vec![ImportedIovec {
            base: 0,
            len: usize::MAX,
        }]);
        assert_eq!(
            errno(import_read_iovecs(
                &memory,
                KVM_USER_LIMIT - 8,
                2,
                UserAddressPolicy::Kvm
            )),
            Errno::EFAULT
        );
        assert_eq!(memory.reads.get(), 0);
        assert_eq!(
            errno(import_read_iovecs(
                &memory,
                0x1000,
                2,
                UserAddressPolicy::Kvm
            )),
            Errno::EINVAL
        );
        assert_eq!(memory.reads.get(), 1);
    }

    #[test]
    fn rng_iovecs_all_entries_are_imported_before_data_range_validation() {
        let memory = ArrayMemory::new(vec![
            ImportedIovec {
                base: usize::MAX,
                len: 1,
            },
            ImportedIovec {
                base: 0,
                len: usize::MAX,
            },
        ]);
        assert_eq!(
            errno(import_read_iovecs(
                &memory,
                0x1000,
                2,
                UserAddressPolicy::Kvm
            )),
            Errno::EINVAL
        );
        assert_eq!(memory.reads.get(), 2);
    }

    #[test]
    fn rng_iovecs_single_segment_clamps_before_range_but_multiple_check_original_ranges() {
        let large = ImportedIovec {
            base: 0,
            len: isize::MAX as usize,
        };
        let memory = ArrayMemory::new(vec![large]);
        assert_eq!(
            import_read_iovecs(&memory, 0x1000, 1, UserAddressPolicy::Kvm).unwrap()[0].len,
            MAX_RW_COUNT
        );
        let memory = ArrayMemory::new(vec![large, ImportedIovec { base: 0, len: 0 }]);
        assert_eq!(
            errno(import_read_iovecs(
                &memory,
                0x1000,
                2,
                UserAddressPolicy::Kvm
            )),
            Errno::EFAULT
        );
        let memory = ArrayMemory::new(vec![
            ImportedIovec {
                base: 0,
                len: MAX_RW_COUNT,
            },
            ImportedIovec {
                base: usize::MAX,
                len: 0,
            },
        ]);
        assert_eq!(
            errno(import_read_iovecs(
                &memory,
                0x1000,
                2,
                UserAddressPolicy::Kvm
            )),
            Errno::EFAULT
        );
    }

    #[test]
    fn rng_iovecs_preserve_zero_length_ceiling_inclusivity() {
        for (base, len, expected) in [
            (KVM_USER_LIMIT, 0, Ok(())),
            (KVM_USER_LIMIT, 1, Err(Errno::EFAULT)),
            (KVM_USER_LIMIT + 1, 0, Err(Errno::EFAULT)),
            (0, 0, Ok(())),
        ] {
            assert_eq!(
                validate_ranges(&[ImportedIovec { base, len }], KVM_USER_LIMIT),
                expected
            );
        }
    }

    #[test]
    fn rng_iovecs_aggregate_cap_does_not_sum_overflow() {
        let mut vectors = vec![
            ImportedIovec {
                base: 0,
                len: 1 << 55
            };
            1024
        ];
        assert!(validate_ranges(&vectors, (1 << 56) - 4096).is_ok());
        cap_lengths(&mut vectors);
        assert_eq!(vectors[0].len, MAX_RW_COUNT);
        assert!(vectors[1..].iter().all(|iov| iov.len == 0));
    }

    #[test]
    fn rng_iovecs_import_errors_preserve_guest_fault_and_backend_failure_identity() {
        for error in [
            Errno::EFAULT,
            Errno::EPERM,
            Errno::EIO,
            Errno::ENOMEM,
            Errno::ENOSYS,
        ] {
            for attempt in [1, 2] {
                let mut memory = ArrayMemory::new(vec![
                    ImportedIovec {
                        base: 0x2000,
                        len: 3,
                    },
                    ImportedIovec {
                        base: 0x3000,
                        len: 5,
                    },
                ]);
                memory.read_error = Some((attempt, error));
                let result = import_read_iovecs(&memory, 0x1000, 2, UserAddressPolicy::Kvm);
                assert_eq!(
                    memory.reads.get(),
                    attempt,
                    "import continued after {error:?}"
                );
                if error == Errno::EFAULT {
                    assert_eq!(errno(result), Errno::EFAULT);
                } else {
                    let Err(Error::Tool(failure)) = result else {
                        panic!("backend import failure became a guest result: {result:?}");
                    };
                    assert_eq!(
                        failure
                            .downcast_ref::<crate::random::RandomCopyFailure>()
                            .unwrap()
                            .errno(),
                        error
                    );
                }
            }
        }
    }

    #[test]
    fn rng_iovecs_native_oracle_failures_keep_their_tool_identity() {
        assert!(native_import_result(0, Errno::EPERM).is_ok());
        assert_eq!(
            errno(native_import_result(-1, Errno::EFAULT)),
            Errno::EFAULT
        );
        for error in [
            Errno::ENOSYS,
            Errno::EPERM,
            Errno::EINVAL,
            Errno::ENOMEM,
            Errno::EIO,
        ] {
            assert!(matches!(
                native_import_result(-1, error),
                Err(Error::Tool(_))
            ));
        }
        assert!(matches!(
            native_import_result(1, Errno::EFAULT),
            Err(Error::Tool(_))
        ));
    }
}
