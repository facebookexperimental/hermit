mod user_access_tests {
    use std::collections::VecDeque;

    use super::*;

    enum Copy {
        Count(usize),
        FailAfter(usize, Errno),
    }

    struct CapabilityMemory {
        base: usize,
        bytes: Vec<u8>,
        actions: VecDeque<Copy>,
        calls: Vec<(usize, Vec<u8>)>,
    }

    impl CapabilityMemory {
        fn new(size: usize, actions: impl IntoIterator<Item = Copy>) -> Self {
            Self {
                base: 0x1000,
                bytes: vec![0xa5; size],
                actions: actions.into_iter().collect(),
                calls: Vec::new(),
            }
        }
    }

    impl MemoryAccess for &mut CapabilityMemory {
        fn read_vectored(&self, _: &[IoSlice], _: &mut [IoSliceMut]) -> Result<usize, Errno> {
            panic!("random copy attempted a guest read")
        }

        fn write_vectored(&mut self, _: &[IoSlice], _: &mut [IoSliceMut]) -> Result<usize, Errno> {
            panic!("random copy fell back to debugger write_vectored")
        }

        fn write(&mut self, _: AddrMut<u8>, _: &[u8]) -> Result<usize, Errno> {
            panic!("random copy fell back to debugger write")
        }

        fn write_with_user_access(
            &mut self,
            addr: AddrMut<u8>,
            bytes: &[u8],
        ) -> Result<usize, Errno> {
            self.calls.push((addr.as_raw(), bytes.to_vec()));
            addr.as_raw()
                .checked_add(bytes.len())
                .ok_or(Errno::EFAULT)?;
            let action = self.actions.pop_front().unwrap_or(Copy::Count(bytes.len()));
            let (n, result) = match action {
                Copy::Count(n) => (n, Ok(n)),
                Copy::FailAfter(n, errno) => (n, Err(errno)),
            };
            assert!(n <= bytes.len());
            let offset = addr.as_raw().checked_sub(self.base).unwrap();
            self.bytes[offset..offset + n].copy_from_slice(&bytes[..n]);
            result
        }
    }

    fn attempted_bytes(lengths: &[usize]) -> (Pcg64Mcg, Vec<u8>) {
        // Draw directly from the independently initialized generator; never
        // derive expected data or state through a production copy helper.
        let mut generator = Pcg64Mcg::seed_from_u64(17);
        let mut bytes = Vec::new();
        for &length in lengths {
            let mut chunk = vec![0; length];
            generator.fill(&mut chunk[..]);
            bytes.extend(chunk);
        }
        (generator, bytes)
    }

    fn terminal(error: Error, expected: Errno) {
        assert!(is_copy_failure(&error));
        let Error::Tool(error) = error else {
            panic!("terminal copy failure became guest-visible: {error:?}");
        };
        assert_eq!(
            error
                .downcast_ref::<RandomCopyFailure>()
                .expect("typed original error")
                .errno(),
            expected
        );
    }

    #[test]
    fn getrandom_uses_only_user_access_for_every_copy_shape() {
        for (length, draws, copies) in [
            (0, vec![], vec![]),
            (7, vec![7], vec![7]),
            (8, vec![8], vec![4, 4]),
            (9, vec![9], vec![9]),
            (4104, vec![4096, 8], vec![4096, 4, 4]),
        ] {
            let mut actual = root_prng(17);
            let mut memory = CapabilityMemory::new(length + 1, []);
            let result = super::super::getrandom(
                &mut actual,
                &mut memory,
                DetTid::from_raw(1),
                call(0x1000, length, 0),
            );
            assert_eq!(result.unwrap(), length as i64);
            let (expected, bytes) = attempted_bytes(&draws);
            assert_eq!(&memory.bytes[..length], bytes);
            assert_eq!(memory.bytes[length], 0xa5);
            assert_eq!(
                memory
                    .calls
                    .iter()
                    .map(|(_, bytes)| bytes.len())
                    .collect::<Vec<_>>(),
                copies
            );
            same_state(&actual, &expected);
        }
    }

    #[test]
    fn unexpected_copy_errors_after_effects_preserve_error_and_attempted_draws() {
        // Diagnostic classification only; the existing cases below still prove
        // real copied prefixes and exact PRNG advancement after failures.
        for errno in [Errno::EPERM, Errno::EIO, Errno::ESRCH, Errno::ENOSYS] {
            let error = copy_error(errno);
            let message = error.to_string();
            assert!(message.contains(&errno.to_string()));
            if errno == Errno::EPERM {
                assert!(message.contains("for a non-dumpable ptrace guest"));
                assert!(message.contains("default namespace configuration"));
                assert!(message.contains("omit --no-namespace"));
            } else {
                assert!(!message.contains("--no-namespace"));
                assert!(!message.contains("EPERM"));
            }
            terminal(error, errno);
        }
        let cases = [
            (7, vec![7], vec![Copy::FailAfter(3, Errno::EIO)], 3, vec![7]),
            (7, vec![7], vec![Copy::FailAfter(7, Errno::EIO)], 7, vec![7]),
            (
                8,
                vec![8],
                vec![Copy::Count(4), Copy::FailAfter(2, Errno::EIO)],
                6,
                vec![4, 4],
            ),
            (
                4104,
                vec![4096, 8],
                vec![Copy::Count(4096), Copy::FailAfter(4, Errno::EIO)],
                4100,
                vec![4096, 4],
            ),
            (
                4104,
                vec![4096, 8],
                vec![
                    Copy::Count(4096),
                    Copy::Count(4),
                    Copy::FailAfter(4, Errno::EIO),
                ],
                4104,
                vec![4096, 4, 4],
            ),
        ];
        for (length, draws, actions, changed, copies) in cases {
            let mut actual = root_prng(17);
            let mut memory = CapabilityMemory::new(length + 1, actions);
            let error = super::super::getrandom(
                &mut actual,
                &mut memory,
                DetTid::from_raw(1),
                call(0x1000, length, 0),
            )
            .unwrap_err();
            terminal(error, Errno::EIO);
            let (expected, bytes) = attempted_bytes(&draws);
            assert_eq!(&memory.bytes[..changed], &bytes[..changed]);
            assert!(memory.bytes[changed..].iter().all(|byte| *byte == 0xa5));
            assert_eq!(
                memory
                    .calls
                    .iter()
                    .map(|(_, bytes)| bytes.len())
                    .collect::<Vec<_>>(),
                copies
            );
            same_state(&actual, &expected);
        }
    }

    #[test]
    fn guest_faults_and_short_copies_keep_exact_prefix_semantics() {
        for (actions, expected, changed, calls) in [
            (
                vec![Copy::FailAfter(0, Errno::EFAULT)],
                Err(Errno::EFAULT),
                0,
                1,
            ),
            (vec![Copy::Count(0)], Err(Errno::EFAULT), 0, 1),
            (vec![Copy::Count(3)], Ok(3), 3, 1),
            (
                vec![Copy::Count(4096), Copy::FailAfter(0, Errno::EFAULT)],
                Ok(4096),
                4096,
                2,
            ),
        ] {
            let mut actual = root_prng(17);
            let mut memory = CapabilityMemory::new(4105, actions);
            let result = super::super::getrandom(
                &mut actual,
                &mut memory,
                DetTid::from_raw(1),
                call(0x1000, 4104, 0),
            )
            .map_err(|error| match error {
                Error::Errno(error) => error,
                other => panic!("guest-fault companion became terminal: {other:?}"),
            });
            assert_eq!(result, expected);
            let draws = if calls == 1 {
                vec![4096]
            } else {
                vec![4096, 8]
            };
            let (expected, bytes) = attempted_bytes(&draws);
            assert_eq!(&memory.bytes[..changed], &bytes[..changed]);
            assert!(memory.bytes[changed..].iter().all(|byte| *byte == 0xa5));
            assert_eq!(memory.calls.len(), calls);
            same_state(&actual, &expected);
        }
    }

    #[test]
    fn overflow_and_invalid_flags_do_not_become_backend_success() {
        let mut actual = root_prng(17);
        let mut memory = CapabilityMemory::new(9, []);
        memory.base = usize::MAX - 2;
        let result = super::super::getrandom(
            &mut actual,
            &mut memory,
            DetTid::from_raw(1),
            call(usize::MAX - 2, 9, 0),
        );
        assert!(matches!(result, Err(Error::Errno(Errno::EFAULT))));
        assert_eq!(memory.calls.len(), 1);
        assert_eq!(memory.bytes, [0xa5; 9]);
        same_state(&actual, &attempted_bytes(&[9]).0);

        let mut actual = root_prng(17);
        let result = super::super::getrandom(
            &mut actual,
            &mut memory,
            DetTid::from_raw(1),
            call(0, 9, usize::MAX),
        );
        assert!(matches!(result, Err(Error::Errno(Errno::EINVAL))));
        assert_eq!(memory.calls.len(), 1);
        same_state(&actual, &Pcg64Mcg::seed_from_u64(17));
    }
}
