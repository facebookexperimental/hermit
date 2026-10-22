/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Linux's bounded, two-phase import for native-width readv arrays.

use reverie::Error;
use reverie::syscalls::Addr;
use reverie::syscalls::Errno;
use reverie::syscalls::MemoryAccess;

pub(crate) const MAX_RW_COUNT: usize = 0x7fff_f000;
#[cfg(test)]
const FOUR_LEVEL_USER_LIMIT: usize = reverie::X86_64_FOUR_LEVEL_USER_ADDRESS_LIMIT as usize;
/// A four-level x86-64 guest whose kernel (Linux 6.4 or later) shortens a lone
/// vector to `MAX_RW_COUNT` before checking it.
#[cfg(test)]
const FOUR_LEVEL: UserAddressLimit = UserAddressLimit {
    max_end: FOUR_LEVEL_USER_LIMIT,
    caps_single_vector: true,
};

/// A copied descriptor with its length capped by Linux's aggregate read limit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ImportedIovec {
    pub(crate) base: usize,
    pub(crate) len: usize,
}

/// How the guest's kernel checks a user range, as its backend reports it
/// through `Guest::user_address_limit`: the bound that Linux's `access_ok`
/// places on a range of guest memory, independent of whether pages are mapped,
/// and whether a lone vector is shortened to `MAX_RW_COUNT` before that check.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct UserAddressLimit {
    max_end: usize,
    caps_single_vector: bool,
}

impl UserAddressLimit {
    /// The limit from the guest's answer to `Guest::user_address_limit`. A
    /// query that fails is the tool's failure, never a guest errno.
    pub(crate) fn from_query(
        limit: Result<reverie::UserAddressLimit, Error>,
    ) -> Result<Self, Error> {
        match limit {
            Ok(limit) => Ok(Self {
                max_end: usize::try_from(limit.max_end).unwrap_or(usize::MAX),
                caps_single_vector: limit.caps_single_vector,
            }),
            Err(error) => Err(Error::Tool(anyhow::anyhow!(
                "the guest's user address limit query failed: {error}"
            ))),
        }
    }

    /// Check the ranges as Linux's `import_iovec` does, against this limit.
    pub(crate) fn validate(self, iovecs: &[ImportedIovec]) -> Result<(), Error> {
        validate_ranges(iovecs, self).map_err(Error::from)
    }
}

fn validate_ranges(iovecs: &[ImportedIovec], limit: UserAddressLimit) -> Result<(), Errno> {
    for iov in iovecs {
        // Since Linux 6.4, import_iovec imports one descriptor with
        // import_ubuf, which shortens it to MAX_RW_COUNT before access_ok;
        // the guest's kernel reports whether it does. Earlier kernels, and
        // every kernel for multiple descriptors, validate original ranges,
        // even beyond the eventual cap.
        let len = if iovecs.len() == 1 && limit.caps_single_vector {
            iov.len.min(MAX_RW_COUNT)
        } else {
            iov.len
        };
        if len > limit.max_end || iov.base > limit.max_end - len {
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
///
/// `limit` is asked for the guest's user address limit only once the count is
/// known to need one, so a count of zero or above `UIO_MAXIOV` returns its
/// result before any query can fail, as Linux's `import_iovec` checks the
/// count before any range.
pub(crate) fn import_read_iovecs(
    memory: &impl MemoryAccess,
    address: usize,
    raw_count: usize,
    limit: impl FnOnce() -> Result<UserAddressLimit, Error>,
) -> Result<Vec<ImportedIovec>, Error> {
    // Linux's unsigned-long vlen is narrowed by import_iovec's unsigned nr_segs.
    let count = raw_count as u32 as usize;
    if count == 0 {
        return Ok(Vec::new());
    }
    if count > libc::UIO_MAXIOV as usize {
        return Err(Errno::EINVAL.into());
    }
    let limit = limit()?;
    let array_bytes = count * std::mem::size_of::<libc::iovec>();
    // Two descriptors prevent the single-vector MAX_RW_COUNT special case
    // from shortening the array-range check.
    limit.validate(&[
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
    limit.validate(&imported)?;
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

    fn four_level() -> Result<UserAddressLimit, Error> {
        Ok(FOUR_LEVEL)
    }

    fn no_query() -> Result<UserAddressLimit, Error> {
        panic!("the count must be checked before the user address limit is queried")
    }

    #[test]
    fn guest_reported_user_address_limit_is_the_one_enforced() {
        // The limit the KVM executor enforces, written out as before.
        assert_eq!(FOUR_LEVEL_USER_LIMIT, (1_usize << 47) - 4096);
        let five_level = (1_usize << 56) - 4096;
        for caps_single_vector in [true, false] {
            for (reported, max_end) in [
                (
                    reverie::X86_64_FOUR_LEVEL_USER_ADDRESS_LIMIT,
                    FOUR_LEVEL_USER_LIMIT,
                ),
                (five_level as u64, five_level),
                (u64::MAX, usize::MAX),
            ] {
                assert_eq!(
                    UserAddressLimit::from_query(Ok(reverie::UserAddressLimit {
                        max_end: reported,
                        caps_single_vector,
                    }))
                    .unwrap(),
                    UserAddressLimit {
                        max_end,
                        caps_single_vector,
                    }
                );
            }
        }
        // No limit is built in: the byte at the four-level limit is valid
        // exactly when the guest reports a limit above it.
        let at_four_level = [ImportedIovec {
            base: FOUR_LEVEL_USER_LIMIT,
            len: 1,
        }];
        let reported = |max_end| {
            UserAddressLimit::from_query(Ok(reverie::UserAddressLimit {
                max_end,
                caps_single_vector: true,
            }))
            .unwrap()
        };
        assert_eq!(
            errno(reported(reverie::X86_64_FOUR_LEVEL_USER_ADDRESS_LIMIT).validate(&at_four_level)),
            Errno::EFAULT
        );
        assert!(reported(five_level as u64).validate(&at_four_level).is_ok());
    }

    #[test]
    fn rng_iovecs_normalize_count_before_limits_or_memory() {
        let memory = ArrayMemory::new(vec![ImportedIovec {
            base: 0x2000,
            len: 3,
        }]);
        assert!(
            import_read_iovecs(&memory, usize::MAX, 1 << 32, no_query)
                .unwrap()
                .is_empty()
        );
        assert_eq!(memory.reads.get(), 0);
        assert_eq!(
            errno(import_read_iovecs(&memory, 0, (1 << 32) | 1025, no_query)),
            Errno::EINVAL
        );
        assert_eq!(memory.reads.get(), 0);
        assert_eq!(
            import_read_iovecs(&memory, 0x1000, (1 << 32) | 1, four_level).unwrap(),
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
                FOUR_LEVEL_USER_LIMIT - 8,
                2,
                four_level
            )),
            Errno::EFAULT
        );
        assert_eq!(memory.reads.get(), 0);
        assert_eq!(
            errno(import_read_iovecs(&memory, 0x1000, 2, four_level)),
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
            errno(import_read_iovecs(&memory, 0x1000, 2, four_level)),
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
            import_read_iovecs(&memory, 0x1000, 1, four_level).unwrap()[0].len,
            MAX_RW_COUNT
        );
        let memory = ArrayMemory::new(vec![large, ImportedIovec { base: 0, len: 0 }]);
        assert_eq!(
            errno(import_read_iovecs(&memory, 0x1000, 2, four_level)),
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
            errno(import_read_iovecs(&memory, 0x1000, 2, four_level)),
            Errno::EFAULT
        );

        // A guest whose kernel predates Linux 6.4 checks a lone vector whole,
        // as every kernel checks each of several, and still applies the
        // aggregate cap to what it accepts.
        let before_6_4 = UserAddressLimit {
            caps_single_vector: false,
            ..FOUR_LEVEL
        };
        let memory = ArrayMemory::new(vec![large]);
        assert_eq!(
            errno(import_read_iovecs(&memory, 0x1000, 1, || Ok(before_6_4))),
            Errno::EFAULT
        );
        let accepted = ImportedIovec {
            base: 0,
            len: MAX_RW_COUNT + 4096,
        };
        let memory = ArrayMemory::new(vec![accepted]);
        assert_eq!(
            import_read_iovecs(&memory, 0x1000, 1, || Ok(before_6_4)).unwrap(),
            [ImportedIovec {
                base: 0,
                len: MAX_RW_COUNT
            }]
        );
        // The range that tells the two kernels apart: one byte past the limit
        // whole, exactly at the limit once shortened to MAX_RW_COUNT.
        let straddle = [ImportedIovec {
            base: FOUR_LEVEL_USER_LIMIT - MAX_RW_COUNT,
            len: MAX_RW_COUNT + 1,
        }];
        assert_eq!(validate_ranges(&straddle, FOUR_LEVEL), Ok(()));
        assert_eq!(validate_ranges(&straddle, before_6_4), Err(Errno::EFAULT));
        let at_limit = [ImportedIovec {
            len: MAX_RW_COUNT,
            ..straddle[0]
        }];
        assert_eq!(validate_ranges(&at_limit, FOUR_LEVEL), Ok(()));
        assert_eq!(validate_ranges(&at_limit, before_6_4), Ok(()));
    }

    #[test]
    fn rng_iovecs_preserve_zero_length_ceiling_inclusivity() {
        for (base, len, expected) in [
            (FOUR_LEVEL_USER_LIMIT, 0, Ok(())),
            (FOUR_LEVEL_USER_LIMIT, 1, Err(Errno::EFAULT)),
            (FOUR_LEVEL_USER_LIMIT + 1, 0, Err(Errno::EFAULT)),
            (0, 0, Ok(())),
        ] {
            assert_eq!(
                validate_ranges(&[ImportedIovec { base, len }], FOUR_LEVEL),
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
        let five_level = UserAddressLimit {
            max_end: (1 << 56) - 4096,
            caps_single_vector: true,
        };
        assert!(validate_ranges(&vectors, five_level).is_ok());
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
                let result = import_read_iovecs(&memory, 0x1000, 2, four_level);
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
    fn rng_iovecs_limit_query_failures_keep_their_tool_identity() {
        // A failed query is the tool's failure, never a guest result, even
        // when it carries EFAULT, which the guest would read as a bad range.
        for error in [
            Errno::EFAULT,
            Errno::ENOSYS,
            Errno::EPERM,
            Errno::EINVAL,
            Errno::ENOMEM,
            Errno::EIO,
        ] {
            assert!(matches!(
                UserAddressLimit::from_query(Err(error.into())),
                Err(Error::Tool(_))
            ));
        }
        assert!(matches!(
            UserAddressLimit::from_query(Err(Error::Tool(anyhow::anyhow!("query failed")))),
            Err(Error::Tool(_))
        ));
        assert!(matches!(
            UserAddressLimit::from_query(Err(
                std::io::Error::from_raw_os_error(libc::EFAULT).into()
            )),
            Err(Error::Tool(_))
        ));
        // A count that needs ranges checked asks the guest first, and its
        // failure stops the import before any guest memory is read.
        let memory = ArrayMemory::new(vec![ImportedIovec {
            base: 0x2000,
            len: 3,
        }]);
        let result = import_read_iovecs(&memory, 0x1000, 1, || {
            UserAddressLimit::from_query(Err(Errno::EFAULT.into()))
        });
        assert!(matches!(result, Err(Error::Tool(_))), "{result:?}");
        assert_eq!(memory.reads.get(), 0);
    }
}
