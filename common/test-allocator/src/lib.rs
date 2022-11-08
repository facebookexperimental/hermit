// (c) Meta Platforms, Inc. and affiliates. Confidential and proprietary.

//! This create provides a simple bump allocator useful for tests of
//! determinism. Because it never reuses memory, that's just about all it's
//! useful for.
//!
//! The allocator provides deterministic result addresses and retired
//! conditional branch counts in circumstances other allocators don't.
//! When running several tests of determinism forked from the same process,
//! allocations in between test runs will alter the allocator state. This
//! normally results in different allocation addresses and codepaths taken.
//!
//! Determinism is provided in these circumstances by calling the
//! `skip_to_offset` method with a high enough value. If successful, this sets
//! the allocator state to a deterministic point, after which both addresses and
//! branch counts should be consistent. Racing threads that allocate will break
//! this guarantee: it is not known which thread will succeed and which will
//! need to retry, getting a different address and executing additional
//! branches.
//!
//! Under the hood, the allocator is backed by a huge `mmap`ed region at a known
//! address. Linux overcommit behavior ensures the system does not run out of
//! memory. Allocations are lock-free and bump downwards.
//!
//! `skip_to_offset` simply sets the pointer for the next allocation to the
//! requested distance from the top of the slab, failing if that region is
//! already used.

use std::alloc::GlobalAlloc;
use std::alloc::Layout;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering::*;

/// A bump allocator backed by an overcommitted `mmap(2)` region that never
/// deallocates.
pub struct OvercommitBumpAlloc {
    /// Top of the region. Starting value of `next` post-initialization.
    top: *mut u8,
    /// Next available byte, growing downwards. Is INIT_* before the backing slab is `mmap`ed.
    next: AtomicUsize,
    /// Bottom of the region. If `next == bottom`, the slab is full.
    bottom: *mut u8,
}

// General safety:
//
// Thread-safety is acheived using atomic CAS to bump the pointer.
// Acquire/Release semantic are sufficient because access to
// bump-allocator-owned memory is guarded by a single atomic variable, so
// interleavings of visiblity with other atomics isn't an issue.
//
// Initialization is handled in much the same way, with one thread winning the
// race to swap INIT_UNINIT for INIT_INPROGRESS. Creating multiple allocators
// is safe, even though there is no public interface to do so, because of
// MAP_FIXED_NOREPLACE. This flag ensures that our fixed mapping doesn't
// overwrite any other mappings; exactly one overlapping allocator will succeed.

/// No thread has attempted to initialize the slab.
const INIT_UNINIT: usize = usize::MAX;
/// One thread has claimed initialization rights, others should wait for it.
const INIT_INPROGRESS: usize = usize::MAX - 1;

#[derive(Debug)]
pub enum SkipError {
    InUse,
    TooFar,
}
impl std::fmt::Display for SkipError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> Result<(), std::fmt::Error> {
        match self {
            SkipError::InUse => write!(f, "Offset is into occupied memory"),
            SkipError::TooFar => write!(f, "Underflow or out of bounds"),
        }
    }
}
impl std::error::Error for SkipError {}

impl OvercommitBumpAlloc {
    /// Creates a new allocator. The actual `mmap(2)` call is lazy.
    ///
    /// The backing mapping will start at `start_address` and be of size `size`,
    /// and is not changeable or expandable. Due to overcommit, size can be set
    /// extremely large if necessary, keeping in mind process `rlimit`s.
    pub const fn new(start_address: usize, size: usize) -> Self {
        Self {
            top: (start_address + size) as *mut u8,
            next: AtomicUsize::new(INIT_UNINIT),
            bottom: start_address as *mut u8,
        }
    }

    /// Get the size of the total slab.
    fn size(&self) -> usize {
        self.top as usize - self.bottom as usize
    }

    /// Ensure the allocator is initialized. Return a valid value of `next`.
    #[inline(always)]
    fn ensure_init(&self) -> usize {
        let val = self.next.load(Acquire);
        if val != INIT_UNINIT && val != INIT_INPROGRESS {
            val
        } else {
            self.do_ensure_init()
        }
    }

    #[cold]
    fn do_ensure_init(&self) -> usize {
        let mut value = self.next.load(Acquire);
        loop {
            match value {
                INIT_UNINIT => {
                    // attempt to claim initialization rights
                    match self
                        .next
                        .compare_exchange(INIT_UNINIT, INIT_INPROGRESS, AcqRel, Acquire)
                    {
                        Ok(_) => {
                            self.perform_initialization();
                            let start_next = self.top as usize;
                            self.next.store(start_next, Release);
                            return start_next;
                        }
                        Err(val) => {
                            value = val;
                            continue;
                        }
                    }
                }
                // Some other thread has claimed initialization, keep spinning
                INIT_INPROGRESS => value = self.next.load(Acquire),
                // Anything else means we've initialized.
                x => return x,
            }
        }
    }

    /// Attempt to back the allocator by an `mmap` slab. Panics on failure.
    #[cold]
    fn perform_initialization(&self) {
        // double check no overflows occurred during constant initialization
        assert!((self.bottom as usize) < (self.top as usize));

        let map = unsafe {
            libc::mmap(
                self.bottom as _,
                self.size(),
                libc::PROT_READ | libc::PROT_WRITE,
                // MAP_FIXED_NOREPLACE ensures exactly one thread is successful at initializing
                // MAP_NORESERVE ensures overcommit isn't checked, see man proc(5) /proc/sys/vm/overcommit_memory
                libc::MAP_PRIVATE
                    | libc::MAP_ANONYMOUS
                    | libc::MAP_NORESERVE
                    | libc::MAP_FIXED_NOREPLACE,
                -1,
                0,
            )
        };
        if map == libc::MAP_FAILED {
            let errno = unsafe { *libc::__errno_location() };
            if errno == libc::EEXIST {
                abort_msg("mmap failed with EEXIST; we overlapped another mapping.", 0);
            } else {
                abort_msg("mmap failed with errno", errno);
            }
        }
        if map != self.bottom as _ {
            // man pages indicate falling back to non-fixed mapping is a common behavior
            abort_msg("MAP_FIXED_NOREPLACE was not actually fixed.", 0);
        }
    }

    /// Allocate a chunk of memory according to `size` and `align`; return a
    /// pointer to the base of the allocation.
    /// # Safety
    /// `align` must be a power of two.
    pub unsafe fn allocate(&self, size: usize, align: usize) -> *mut u8 {
        let mut next = self.ensure_init();
        loop {
            let spaced = match next.checked_sub(size) {
                None => return std::ptr::null_mut(),
                Some(x) => x,
            };
            let aligned = spaced & !(align - 1);
            if aligned < self.bottom as usize {
                return std::ptr::null_mut();
            }
            match self.next.compare_exchange(next, aligned, AcqRel, Acquire) {
                Ok(_) => return aligned as *mut u8,
                Err(prev) => next = prev,
            }
        }
    }

    /// Set the internal bump pointer to a fixed offset from the top of the
    /// slab. Return an error if the pointer has already passed this offset.
    pub fn skip_to_offset(&self, offset: usize) -> Result<(), SkipError> {
        // SAFETY: we don't need a bounds check here, because any
        // actual use of the pointer to allocate does a bounds check.
        let desired_next = (self.top as usize)
            .checked_sub(offset)
            .ok_or(SkipError::TooFar)?;
        let mut next = self.ensure_init();
        loop {
            if next < desired_next {
                return Err(SkipError::InUse);
            }
            match self
                .next
                .compare_exchange(next, desired_next, AcqRel, Acquire)
            {
                Ok(_) => return Ok(()),
                Err(prev) => next = prev,
            }
        }
    }

    /// Call `skip_to_offset`, but only if this allocator has already been used.
    /// This can be used to avoid initializing the allocator if no one is using
    /// it.
    pub fn skip_to_offset_if_init(&self, offset: usize) -> Result<(), SkipError> {
        if self.next.load(Acquire) == INIT_UNINIT {
            return Ok(());
        }
        self.skip_to_offset(offset)
    }
}

unsafe impl Send for OvercommitBumpAlloc {}
unsafe impl Sync for OvercommitBumpAlloc {}

unsafe impl GlobalAlloc for OvercommitBumpAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: layout.align() is guaranteed to be a power of two
        self.allocate(layout.size(), layout.align())
    }
    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {
        // do nothing
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: layout.align() is guaranteed to be a power of two
        // SAFETY: MAP_ANONYMOUS pages are guaranteed to be zero
        self.allocate(layout.size(), layout.align())
    }
}

impl Drop for OvercommitBumpAlloc {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.bottom as *mut _, self.size());
        }
    }
}

const DEFAULT_PAGE_ADDRESS: usize = 0x400000000000; // 64 TiB
const DEFAULT_SIZE: usize = 0x20000000000; // 128 GiB, try to avoid any rlimits

/// Use this constant to calculate expected addresses in conjunction with
/// `skip_to_offset`.
/// ```no_run
/// GLOBAL.skip_to_offset(0x100000).unwrap()
/// assert_eq!(GLOBAL.allocate(4, 4) as usize, GLOBAL_SLAB_TOP - 0x100000 - 4);
/// ```
pub const GLOBAL_SLAB_TOP: usize = DEFAULT_PAGE_ADDRESS + DEFAULT_SIZE;

/// The maximum argument to `GLOBAL.skip_to_offset`
pub const GLOBAL_MAX_OFFSET: usize = DEFAULT_SIZE;

pub static GLOBAL: OvercommitBumpAlloc =
    OvercommitBumpAlloc::new(DEFAULT_PAGE_ADDRESS, DEFAULT_SIZE);
pub struct Global;
unsafe impl GlobalAlloc for Global {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        GLOBAL.alloc(layout)
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        GLOBAL.dealloc(ptr, layout)
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        GLOBAL.alloc_zeroed(layout)
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        GLOBAL.realloc(ptr, layout, new_size)
    }
}

/// Print a message and abort the process. Used here instead of panic!(), which
/// hangs if we do it in allocator code due to potential recursion.
fn abort_msg(msg: &str, errno: i32) {
    const PREFIX: &str = "test-allocator aborting with reason: ";
    const NEWLINE: &str = "\n";
    const FD: libc::c_int = libc::STDERR_FILENO;
    // Use libc::write directly to avoid buffering.
    // No error checking because we can't recover anyway.
    unsafe {
        libc::write(FD, PREFIX.as_ptr() as *const _, PREFIX.len());
        libc::write(libc::STDERR_FILENO, msg.as_ptr() as *const _, msg.len());
    }
    if errno != 0 {
        let mut buf = [b' '; 5];
        buf[1] = if errno < 0 { b'-' } else { b' ' };
        buf[2] = b'0' + ((errno / 100) % 10) as u8;
        buf[3] = b'0' + ((errno / 10) % 10) as u8;
        buf[4] = b'0' + (errno % 10) as u8;
        unsafe {
            libc::write(FD, buf.as_ptr() as *const _, buf.len());
        }
    }
    unsafe {
        libc::write(FD, NEWLINE.as_ptr() as *const _, NEWLINE.len());
        libc::_exit(101);
    }
}

#[cfg(all(test, not(sanitized)))]
mod tests {
    use std::sync::Arc;

    use super::*;

    const TEST_ADDRESS: usize = 0x6500000000;
    const TEST_SIZE: usize = 0x1000000;

    fn load_allocator(a: &OvercommitBumpAlloc, iters: usize, v: &mut Vec<(usize, usize)>) {
        for _ in 0..iters {
            for size in [5, 10, 28, 32, 64, 77, 81, 256, 1000, 5000, 1024] {
                for align in [4, 8, 16, 32, 64] {
                    let ptr = unsafe { a.allocate(size, align) };
                    assert!(!ptr.is_null());
                    assert_eq!(ptr as usize % align, 0);
                    v.push((ptr as usize, size))
                }
            }
        }
    }

    #[test]
    fn overlaps_and_offset() {
        let alloc = Arc::new(OvercommitBumpAlloc::new(TEST_ADDRESS, TEST_SIZE));

        // race allocator threads
        let mut handles = Vec::new();
        for _ in 0..10 {
            let alloc2 = Arc::clone(&alloc);
            handles.push(std::thread::spawn(move || {
                let mut v = Vec::new();
                load_allocator(&alloc2, 10, &mut v);
                v
            }));
        }
        let mut blocks = Vec::new();
        for h in handles {
            blocks.append(&mut h.join().unwrap());
        }

        // verify we can skip to an offset and get a known address
        alloc.skip_to_offset(TEST_SIZE / 2).unwrap();
        assert_eq!(
            unsafe { alloc.allocate(16, 4) } as usize,
            TEST_ADDRESS + TEST_SIZE / 2 - 16,
        );
        load_allocator(&alloc, 10, &mut blocks);

        // verify no allocations overlapped
        blocks.sort_unstable();
        blocks.iter().fold((0, 0), |(s1, o1), &(s2, o2)| {
            assert!(s1 + o1 <= s2);
            (s2, o2)
        });
    }

    #[test]
    fn global_allocator_usable() {
        // If GLOBAL is set as the #[global_allocator], panics and prints don't
        // work normally, so failures are hard to debug. By testing it here
        // where we use an ordinary allocator, we may find initialization
        // failures more easily.
        use std::ops::Not;
        assert!(
            unsafe { GLOBAL.alloc(Layout::new::<u64>()) }
                .is_null()
                .not()
        );
    }
}
