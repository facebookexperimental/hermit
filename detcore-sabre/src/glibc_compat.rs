/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Plugin-private definitions of two glibc symbols newer than the oldest
//! supported host's glibc (2.34), so the guest's dynamic loader never has to
//! find them (<https://github.com/rrnewton/hermit/issues/3652>).
//!
//! Statically linked C code would otherwise import them from the guest's libc:
//!
//! - `__isoc23_strtol` (GLIBC_2.38): glibc 2.38+ headers rename `strtol` to it
//!   when C is compiled as C23, GCC 15's default; mimalloc's options.c calls
//!   `strtol`.
//! - `_dl_find_object` (GLIBC_2.35): libgcc_eh.a (see build.rs) looks up a
//!   program counter's unwind tables with it when it was built against glibc
//!   2.35+.
//!
//! The assembly below defines both names with hidden visibility. The static
//! link binds those references here, and the names stay out of the plugin's
//! dynamic symbol table, so the guest's own references still bind to its libc.

use std::ffi::c_char;
use std::ffi::c_int;
use std::ffi::c_long;
use std::ffi::c_void;

std::arch::global_asm!(
    ".globl __isoc23_strtol",
    ".hidden __isoc23_strtol",
    ".set __isoc23_strtol, {strtol}",
    ".globl _dl_find_object",
    ".hidden _dl_find_object",
    ".set _dl_find_object, {find_object}",
    strtol = sym isoc23_strtol,
    find_object = sym dl_find_object,
);

/// C23 `strtol`. It differs from C17 `strtol` only in accepting a `0b` or `0B`
/// prefix before binary digits when `base` is 0 or 2; C17 parses that as the
/// number 0 followed by text.
unsafe extern "C" fn isoc23_strtol(
    nptr: *const c_char,
    endptr: *mut *mut c_char,
    base: c_int,
) -> c_long {
    if base == 0 || base == 2 {
        // SAFETY: the caller passes a NUL-terminated string, as strtol requires.
        if let Some(value) = unsafe { binary_prefixed(nptr, endptr) } {
            return value;
        }
    }
    // SAFETY: the caller's arguments are exactly strtol's.
    unsafe { libc::strtol(nptr, endptr, base) }
}

/// Parses `[space][sign]0b<binary digits>` as C23 does, or returns `None` if
/// `nptr` does not start that way.
///
/// # Safety
///
/// `nptr` must be NUL-terminated, and `endptr` null or writable.
unsafe fn binary_prefixed(nptr: *const c_char, endptr: *mut *mut c_char) -> Option<c_long> {
    let byte = |offset: usize| unsafe { *nptr.add(offset) as u8 };
    let mut at = 0;
    while unsafe { libc::isspace(c_int::from(byte(at))) } != 0 {
        at += 1;
    }
    let negative = byte(at) == b'-';
    if negative || byte(at) == b'+' {
        at += 1;
    }
    if byte(at) != b'0'
        || !matches!(byte(at + 1), b'b' | b'B')
        || !matches!(byte(at + 2), b'0' | b'1')
    {
        return None;
    }
    at += 2;
    let mut magnitude: u64 = 0;
    let mut overflow = false;
    while let digit @ (b'0' | b'1') = byte(at) {
        match magnitude.checked_mul(2) {
            Some(doubled) => magnitude = doubled | u64::from(digit - b'0'),
            None => overflow = true,
        }
        at += 1;
    }
    if !endptr.is_null() {
        // SAFETY: the caller allows writing `endptr`; strtol stores a pointer
        // into its (const) input the same way.
        unsafe { *endptr = nptr.add(at).cast_mut() };
    }
    let value = if negative {
        if overflow || magnitude > c_long::MIN.unsigned_abs() {
            None
        } else {
            Some(0_i64.wrapping_sub_unsigned(magnitude))
        }
    } else {
        c_long::try_from(magnitude).ok().filter(|_| !overflow)
    };
    Some(value.unwrap_or_else(|| {
        // SAFETY: errno is this thread's.
        unsafe { *libc::__errno_location() = libc::ERANGE };
        if negative { c_long::MIN } else { c_long::MAX }
    }))
}

/// glibc's `struct dl_find_object` on x86_64.
#[repr(C)]
struct DlFindObject {
    dlfo_flags: u64,
    dlfo_map_start: *mut c_void,
    dlfo_map_end: *mut c_void,
    dlfo_link_map: *mut c_void,
    dlfo_eh_frame: *mut c_void,
    dlfo_reserved: [u64; 7],
}

/// The loaded object containing a program counter, as `_dl_find_object` reports it.
#[derive(Debug, PartialEq, Eq)]
struct LoadedObject {
    map_start: usize,
    map_end: usize,
    eh_frame: usize,
}

/// `_dl_find_object` over `dl_iterate_phdr`, the lookup libgcc used before
/// glibc 2.35. It returns 0 and fills `result` if a loaded object maps `pc`,
/// else -1. `dlfo_link_map` is null: libgcc reads only `dlfo_eh_frame`.
unsafe extern "C" fn dl_find_object(pc: *mut c_void, result: *mut DlFindObject) -> c_int {
    let Some(object) = loaded_object_containing(pc as usize) else {
        return -1;
    };
    // SAFETY: the caller passes a writable `struct dl_find_object`.
    unsafe {
        result.write(DlFindObject {
            dlfo_flags: 0,
            dlfo_map_start: object.map_start as *mut c_void,
            dlfo_map_end: object.map_end as *mut c_void,
            dlfo_link_map: std::ptr::null_mut(),
            dlfo_eh_frame: object.eh_frame as *mut c_void,
            dlfo_reserved: [0; 7],
        })
    };
    0
}

fn loaded_object_containing(pc: usize) -> Option<LoadedObject> {
    struct Search {
        pc: usize,
        found: Option<LoadedObject>,
    }

    unsafe extern "C" fn visit(
        info: *mut libc::dl_phdr_info,
        _size: libc::size_t,
        data: *mut c_void,
    ) -> c_int {
        // SAFETY: `data` is the `Search` passed below, and glibc passes a valid
        // `info` whose program headers stay mapped during the callback.
        let (search, info) = unsafe { (&mut *data.cast::<Search>(), &*info) };
        let headers = unsafe { std::slice::from_raw_parts(info.dlpi_phdr, info.dlpi_phnum.into()) };
        let base = info.dlpi_addr as usize;
        let mut object = LoadedObject {
            map_start: usize::MAX,
            map_end: 0,
            eh_frame: 0,
        };
        let mut contains_pc = false;
        for header in headers {
            let start = base.wrapping_add(header.p_vaddr as usize);
            match header.p_type {
                libc::PT_LOAD => {
                    let end = start.wrapping_add(header.p_memsz as usize);
                    object.map_start = object.map_start.min(start);
                    object.map_end = object.map_end.max(end);
                    contains_pc |= (start..end).contains(&search.pc);
                }
                libc::PT_GNU_EH_FRAME => object.eh_frame = start,
                _ => {}
            }
        }
        if !contains_pc {
            return 0;
        }
        search.found = Some(object);
        1
    }

    let mut search = Search { pc, found: None };
    // SAFETY: `visit` matches dl_iterate_phdr's callback contract and `search`
    // outlives the call.
    unsafe { libc::dl_iterate_phdr(Some(visit), (&raw mut search).cast()) };
    search.found
}

#[cfg(test)]
mod tests {
    use std::ffi::CStr;

    use super::*;

    fn parse(text: &CStr, base: c_int) -> (c_long, usize, c_int) {
        let mut end = std::ptr::null_mut();
        // SAFETY: errno is this thread's.
        unsafe { *libc::__errno_location() = 0 };
        let value = unsafe { isoc23_strtol(text.as_ptr(), &mut end, base) };
        let errno = unsafe { *libc::__errno_location() };
        (value, end as usize - text.as_ptr() as usize, errno)
    }

    #[test]
    fn isoc23_strtol_matches_strtol_without_a_binary_prefix() {
        assert_eq!(parse(c"  -42xyz", 10), (-42, 5, 0));
        assert_eq!(parse(c"0x1f", 0), (31, 4, 0));
        assert_eq!(parse(c"017", 0), (15, 3, 0));
        assert_eq!(parse(c"0b101", 10), (0, 1, 0));
        assert_eq!(parse(c"0b2", 0), (0, 1, 0));
        assert_eq!(
            parse(c"99999999999999999999", 10),
            (c_long::MAX, 20, libc::ERANGE)
        );
    }

    #[test]
    fn isoc23_strtol_accepts_a_binary_prefix_in_bases_0_and_2() {
        assert_eq!(parse(c"0b101", 0), (5, 5, 0));
        assert_eq!(parse(c" +0B11!", 2), (3, 6, 0));
        assert_eq!(parse(c"-0b1", 0), (-1, 4, 0));
        let min = format!("-0b1{}", "0".repeat(63));
        let min = std::ffi::CString::new(min).unwrap();
        assert_eq!(parse(&min, 0), (c_long::MIN, 67, 0));
        let over = format!("0b1{}", "0".repeat(63));
        let over = std::ffi::CString::new(over).unwrap();
        assert_eq!(parse(&over, 2), (c_long::MAX, 66, libc::ERANGE));
    }

    #[test]
    fn dl_find_object_finds_this_code_and_its_unwind_tables() {
        let pc = dl_find_object as *const () as usize;
        let mut result = std::mem::MaybeUninit::<DlFindObject>::uninit();
        assert_eq!(
            unsafe { dl_find_object(pc as *mut c_void, result.as_mut_ptr()) },
            0
        );
        let result = unsafe { result.assume_init() };
        let (start, end) = (result.dlfo_map_start as usize, result.dlfo_map_end as usize);
        assert!(
            (start..end).contains(&pc),
            "{start:#x}..{end:#x} misses {pc:#x}"
        );
        let eh_frame = result.dlfo_eh_frame as usize;
        assert!((start..end).contains(&eh_frame), "eh_frame {eh_frame:#x}");
        // The `.eh_frame_hdr` section starts with version 1.
        assert_eq!(unsafe { *(eh_frame as *const u8) }, 1);
        let mut unused = std::mem::MaybeUninit::<DlFindObject>::uninit();
        assert_eq!(
            unsafe { dl_find_object(std::ptr::null_mut(), unused.as_mut_ptr()) },
            -1
        );
    }
}
