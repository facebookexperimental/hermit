/* Copyright (c) Meta Platforms, Inc. and affiliates. */
//! Adapter controls using actual process_vm_writev. These do not authenticate
//! a SaBRe loader or exercise its complete request/supervisor protocol.
use rand::RngExt as _;
use rand::SeedableRng as _;
use rand_pcg::Pcg64Mcg;
use reverie::syscalls::Getrandom;

use super::*;

/// The bootstrap's getrandom adapter ([`getrandom_unrecorded`]). A request
/// that writes bytes returns the fill post-exec records, for exactly the bytes
/// written; any other request returns none.
fn bootstrap_getrandom(
    prng: &mut Pcg64Mcg,
    memory: impl MemoryAccess,
    call: Getrandom,
) -> std::result::Result<i64, reverie::Error> {
    let (result, fill) = getrandom_unrecorded(prng, memory, call)?;
    assert_eq!(
        fill.map(|fill| fill.written as i64),
        (result > 0).then_some(result)
    );
    Ok(result)
}

struct Pages(*mut u8);
impl Pages {
    fn new() -> Self {
        let p = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                PAGE * 2,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert_ne!(p, libc::MAP_FAILED);
        unsafe { std::ptr::write_bytes(p.cast::<u8>(), 0xa5, PAGE * 2) };
        Self(p.cast())
    }
    fn at(&self, offset: usize) -> AddrMut<'_, u8> {
        assert!(offset < PAGE * 2);
        AddrMut::from_raw(self.0 as usize + offset).unwrap()
    }
    fn protect_tail(&self) {
        assert_eq!(
            unsafe { libc::mprotect(self.0.add(PAGE).cast(), PAGE, libc::PROT_READ) },
            0
        );
    }
    fn bytes(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.0, PAGE * 2) }
    }
}
impl Drop for Pages {
    fn drop(&mut self) {
        assert_eq!(unsafe { libc::munmap(self.0.cast(), PAGE * 2) }, 0);
    }
}
fn call(address: AddrMut<u8>, len: usize) -> Getrandom {
    let syscall = Syscall::from_raw(
        Sysno::getrandom,
        SyscallArgs::new(address.as_raw(), len, 0, 0, 0, 0),
    );
    match syscall {
        Syscall::Getrandom(call) => call,
        _ => unreachable!(),
    }
}
fn oracle(lengths: &[usize]) -> (Pcg64Mcg, Vec<u8>) {
    let mut expected = Pcg64Mcg::seed_from_u64(17);
    let mut bytes = Vec::new();
    for &len in lengths {
        let mut chunk = vec![0; len];
        expected.fill(&mut chunk[..]);
        bytes.extend(chunk);
    }
    (expected, bytes)
}
fn same_cursor(actual: &Pcg64Mcg, expected: &Pcg64Mcg) {
    assert_eq!(
        serde_json::to_vec(actual).unwrap(),
        serde_json::to_vec(expected).unwrap()
    );
    let mut a = actual.clone();
    let mut b = expected.clone();
    assert_eq!(a.random::<[u8; 64]>(), b.random::<[u8; 64]>());
}

#[test]
fn actual_remote_copy_obeys_read_only_pages_and_reports_real_prefix() {
    for len in [7, 8, 9, PAGE] {
        let pages = Pages::new();
        pages.protect_tail();
        let mut memory = RemoteMemory(Pid::this());
        assert_eq!(
            memory.write_with_user_access(pages.at(PAGE), &vec![0x31; len]),
            Err(Errno::EFAULT)
        );
        assert!(pages.bytes().iter().all(|b| *b == 0xa5));
        assert_eq!(memory.write_with_user_access(pages.at(PAGE), &[]), Ok(0));
    }
    let pages = Pages::new();
    pages.protect_tail();
    assert_eq!(
        RemoteMemory(Pid::this()).write_with_user_access(pages.at(0), &vec![0x31; PAGE + 8]),
        Ok(PAGE)
    );
    assert!(pages.bytes()[..PAGE].iter().all(|b| *b == 0x31));
    assert!(pages.bytes()[PAGE..].iter().all(|b| *b == 0xa5));
}

#[test]
fn actual_bootstrap_adapter_faults_keep_prefix_and_attempted_prng_cursor() {
    for len in [7, 8] {
        let pages = Pages::new();
        pages.protect_tail();
        let mut actual = root_prng(17);
        let result = bootstrap_getrandom(
            &mut actual,
            RemoteMemory(Pid::this()),
            call(pages.at(PAGE), len),
        );
        assert!(matches!(result, Err(reverie::Error::Errno(Errno::EFAULT))));
        assert_eq!(random_response(result).unwrap(), -i64::from(libc::EFAULT));
        assert!(pages.bytes().iter().all(|b| *b == 0xa5));
        same_cursor(&actual, &oracle(&[len]).0);
    }
    let pages = Pages::new();
    pages.protect_tail();
    let mut actual = root_prng(17);
    let result = bootstrap_getrandom(
        &mut actual,
        RemoteMemory(Pid::this()),
        call(pages.at(0), PAGE + 8),
    );
    assert_eq!(random_response(result).unwrap(), PAGE as i64);
    let (expected, bytes) = oracle(&[PAGE, 8]);
    assert_eq!(&pages.bytes()[..PAGE], &bytes[..PAGE]);
    assert!(pages.bytes()[PAGE..].iter().all(|b| *b == 0xa5));
    same_cursor(&actual, &expected);
}

struct ErrorAfterEffects {
    fail_call: usize,
    prefix: usize,
    calls: Vec<usize>,
}
impl MemoryAccess for &mut ErrorAfterEffects {
    fn read_vectored(
        &self,
        _: &[IoSlice],
        _: &mut [IoSliceMut],
    ) -> std::result::Result<usize, Errno> {
        panic!("random adapter must not read guest memory")
    }
    fn write_vectored(
        &mut self,
        _: &[IoSlice],
        _: &mut [IoSliceMut],
    ) -> std::result::Result<usize, Errno> {
        panic!("random adapter must use the explicit user-access copy")
    }
    fn write_with_user_access(
        &mut self,
        address: AddrMut<u8>,
        bytes: &[u8],
    ) -> std::result::Result<usize, Errno> {
        self.calls.push(bytes.len());
        let fail = self.calls.len() == self.fail_call;
        let prefix = if fail { self.prefix } else { bytes.len() };
        assert!(prefix <= bytes.len());
        assert_eq!(
            RemoteMemory(Pid::this()).write_with_user_access(address, &bytes[..prefix]),
            Ok(prefix)
        );
        if fail {
            // A real unrelated operation supplies the non-EFAULT error after
            // actual memory effects. This is a deliberate adapter seam, not a
            // claim that process_vm_writev itself returned this EBADF.
            assert_eq!(unsafe { libc::fcntl(-1, libc::F_GETFD) }, -1);
            let original = Errno::last();
            assert_eq!(original, Errno::EBADF);
            Err(original)
        } else {
            Ok(prefix)
        }
    }
}

#[test]
fn bootstrap_bridge_keeps_typed_error_after_real_partial_and_full_effects() {
    for (len, fail_call, prefix, changed, draws, calls) in [
        (7, 1, 3, 3, vec![7], vec![7]),
        (7, 1, 7, 7, vec![7], vec![7]),
        (8, 2, 2, 6, vec![8], vec![4, 4]),
        (PAGE + 8, 3, 4, PAGE + 8, vec![PAGE, 8], vec![PAGE, 4, 4]),
    ] {
        let pages = Pages::new();
        let mut memory = ErrorAfterEffects {
            fail_call,
            prefix,
            calls: Vec::new(),
        };
        let mut actual = root_prng(17);
        let error =
            bootstrap_getrandom(&mut actual, &mut memory, call(pages.at(0), len)).unwrap_err();
        let reverie::Error::Tool(inner) = &error else {
            panic!("fatal copy became guest result: {error:?}")
        };
        let original = inner
            .downcast_ref::<detcore::random::RandomCopyFailure>()
            .unwrap() as *const _;
        let error = random_response(Err(error)).unwrap_err();
        let reverie::Error::Tool(inner) = error.downcast_ref::<reverie::Error>().unwrap() else {
            panic!("bridge lost the Tool error")
        };
        let retained = inner
            .downcast_ref::<detcore::random::RandomCopyFailure>()
            .unwrap();
        assert!(std::ptr::eq(original, retained));
        assert_eq!(retained.errno(), Errno::EBADF);
        let (expected, bytes) = oracle(&draws);
        assert_eq!(&pages.bytes()[..changed], &bytes[..changed]);
        assert!(pages.bytes()[changed..].iter().all(|b| *b == 0xa5));
        assert_eq!(memory.calls, calls);
        same_cursor(&actual, &expected);
    }
}
