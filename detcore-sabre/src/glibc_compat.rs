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
//! SHARED FILE: detcore-liteinst/src/lib.rs compiles this same file into the
//! in-guest LiteInst runtime with `#[path]`
//! (<https://github.com/rrnewton/hermit/issues/3967>), so an edit here changes
//! both guest preloads. Each preload compiles its own copy because the hidden
//! definitions must sit in that cdylib's own object for its static link to
//! bind them.
//!
//! Statically linked C code would otherwise import them from the guest's libc:
//!
//! - `__isoc23_strtol` (GLIBC_2.38): glibc 2.38+ headers rename `strtol` to it
//!   when C is compiled as C23, GCC 15's default; mimalloc's options.c calls
//!   `strtol`.
//! - `_dl_find_object` (GLIBC_2.35): libgcc_eh.a (see each crate's
//!   build.rs) looks up a program counter's unwind tables with it when it was
//!   built against glibc 2.35+. The private definition forwards to the
//!   guest's real `_dl_find_object` when its glibc has one (2.35 and later, or
//!   a backport). Only without it does it search with `dl_iterate_phdr`, which
//!   sees the caller's loader namespace only. The real lookup sees every
//!   namespace, so a panic that unwinds through a frame of a library loaded
//!   with `dlmopen(LM_ID_NEWLM)` is still caught; with `dl_iterate_phdr` alone
//!   it aborts ("failed to initiate panic, error 5",
//!   <https://github.com/rrnewton/hermit/issues/3980>).
//!
//!   The real function is found with Reverie's `reverie::glibc_symbol`,
//!   which reads libc.so.6's symbol and version tables itself through
//!   `dl_iterate_phdr` and calls no `dl*` function: `dlsym`/`dlvsym` take the
//!   loader's `dl_load_lock`, which `dlopen` holds while it runs
//!   constructors, and change `dlerror` state. An `.init_array` entry looks it
//!   up while the preload is initialized. A lookup before that (a panic in an
//!   earlier constructor) does it then. After the first lookup the unwinder
//!   calls glibc's own `_dl_find_object`: lock-free and async-signal-safe, it
//!   sees every namespace. The lookup itself takes only `dl_iterate_phdr`'s
//!   recursive `dl_load_write_lock`, under which glibc runs no constructor or
//!   destructor; its remaining exposure, before initialization, is the one
//!   libgcc's unwinder had on every unwind before glibc 2.35 (see that
//!   module). Reverie's reverie-liteinst/src/glibc_compat.rs does the same for
//!   Reverie's own preload.
//!
//! The assembly below defines both names with hidden visibility. The static
//! link binds those references here, and the names stay out of the plugin's
//! dynamic symbol table, so the guest's own references still bind to its libc.

use std::ffi::c_char;
use std::ffi::c_int;
use std::ffi::c_long;
use std::ffi::c_void;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

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

type DlFindObjectFn = unsafe extern "C" fn(*mut c_void, *mut DlFindObject) -> c_int;

/// The guest's real `_dl_find_object`, as an address: [`UNRESOLVED`] until
/// [`resolve_real_dl_find_object`] has run, [`ABSENT`] when its libc has none.
static REAL_DL_FIND_OBJECT: AtomicUsize = AtomicUsize::new(UNRESOLVED);
const UNRESOLVED: usize = 0;
const ABSENT: usize = 1;

#[used]
#[unsafe(link_section = ".init_array")]
static RESOLVE_REAL_DL_FIND_OBJECT: extern "C" fn() = resolve_real_dl_find_object;

/// Finds the guest's real `_dl_find_object@GLIBC_2.35` and caches it. It
/// runs as an `.init_array` entry, and on a lookup before that entry has run.
/// Two threads racing to run it store the same answer.
extern "C" fn resolve_real_dl_find_object() {
    let address =
        reverie::glibc_symbol::glibc_versioned_function(b"_dl_find_object", b"GLIBC_2.35")
            .unwrap_or(ABSENT);
    REAL_DL_FIND_OBJECT.store(address, Ordering::Release);
}

/// The guest's real `_dl_find_object`, if its libc has one.
fn real_dl_find_object() -> Option<DlFindObjectFn> {
    if REAL_DL_FIND_OBJECT.load(Ordering::Acquire) == UNRESOLVED {
        resolve_real_dl_find_object();
    }
    match REAL_DL_FIND_OBJECT.load(Ordering::Acquire) {
        UNRESOLVED | ABSENT => None,
        // SAFETY: any other value is glibc's `_dl_find_object`, whose
        // signature this is.
        address => Some(unsafe { std::mem::transmute::<usize, DlFindObjectFn>(address) }),
    }
}

/// The guest's real `_dl_find_object` when it has one; otherwise a lookup over
/// `dl_iterate_phdr`, the one libgcc used before glibc 2.35. Either returns 0
/// and fills `result` if a loaded object maps `pc`, else -1. The fallback
/// leaves `dlfo_link_map` null: libgcc reads only `dlfo_eh_frame`.
unsafe extern "C" fn dl_find_object(pc: *mut c_void, result: *mut DlFindObject) -> c_int {
    if let Some(real) = real_dl_find_object() {
        // SAFETY: the caller's arguments are exactly `_dl_find_object`'s.
        return unsafe { real(pc, result) };
    }
    // SAFETY: as above.
    unsafe { phdr_find_object(pc, result) }
}

/// The `dl_iterate_phdr` fallback of [`dl_find_object`].
///
/// # Safety
///
/// `result` must be writable storage for one `struct dl_find_object`.
unsafe fn phdr_find_object(pc: *mut c_void, result: *mut DlFindObject) -> c_int {
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
// Test setup and cleanup outside any guest.
#[allow(clippy::disallowed_methods)]
mod tests {
    use std::ffi::CStr;
    use std::ffi::CString;
    use std::process::Command;

    use super::*;

    /// Set in the child process a namespace test starts: which loader
    /// namespace the child loads the C bridge into.
    const NAMESPACE_CHILD: &str = "HERMIT_GLIBC_COMPAT_UNWIND_NAMESPACE_CHILD";
    /// The C bridge's path, for the child.
    const BRIDGE_PATH: &str = "HERMIT_GLIBC_COMPAT_UNWIND_BRIDGE";

    /// A Rust panic unwinds through a C frame of a library loaded into
    /// NAMESPACE (`new`: `dlmopen(LM_ID_NEWLM)`; `same`: `dlopen`) and is
    /// caught by `catch_unwind` beneath it
    /// (https://github.com/rrnewton/hermit/issues/3980). This test binary
    /// links the same static unwinder and `_dl_find_object` as the preloads,
    /// so the unwinder must find the bridge's unwind tables in either
    /// namespace. The scenario runs in a child, because a failed unwind aborts
    /// the process.
    fn panic_through_bridge(namespace: &str, test: &str) {
        if std::env::var(NAMESPACE_CHILD).as_deref() == Ok(namespace) {
            panic_through_bridge_in_this_process(namespace);
            return;
        }
        let directory = std::env::temp_dir().join(format!(
            "hermit-glibc-compat-unwind-{}-{namespace}",
            std::process::id()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let source = directory.join("bridge.c");
        let bridge = directory.join("bridge.so");
        std::fs::write(
            &source,
            "void bridge_call(void (*callback)(void)) { callback(); }\n",
        )
        .unwrap();
        let built = Command::new("cc")
            .args(["-shared", "-fPIC", "-fexceptions", "-O0", "-o"])
            .arg(&bridge)
            .arg(&source)
            .status()
            .expect("the namespace unwind tests need a C compiler (cc) on PATH");
        assert!(built.success(), "cc failed: {built}");
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test, "--nocapture", "--test-threads=1"])
            .env(NAMESPACE_CHILD, namespace)
            .env(BRIDGE_PATH, &bridge)
            .output()
            .unwrap();
        std::fs::remove_dir_all(&directory).unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success() && stdout.contains(&format!("namespace={namespace} caught=1")),
            "the child exited with {}\nstdout:\n{stdout}\nstderr:\n{stderr}",
            output.status
        );
    }

    fn panic_through_bridge_in_this_process(namespace: &str) {
        let path = CString::new(std::env::var(BRIDGE_PATH).unwrap()).unwrap();
        let caught = catch_a_panic_through_the_bridge(namespace, &path);
        println!("namespace={namespace} caught={}", u8::from(caught));
    }

    /// Loads the bridge at `path` into NAMESPACE and reports whether a panic
    /// through it was caught.
    fn catch_a_panic_through_the_bridge(namespace: &str, path: &CStr) -> bool {
        // SAFETY: `path` names a shared object built by the parent.
        let handle = unsafe {
            match namespace {
                "new" => libc::dlmopen(libc::LM_ID_NEWLM, path.as_ptr(), libc::RTLD_NOW),
                _ => libc::dlopen(path.as_ptr(), libc::RTLD_NOW),
            }
        };
        assert!(
            !handle.is_null(),
            "cannot load the bridge into namespace {namespace}"
        );
        // SAFETY: `handle` is a loaded library.
        let symbol = unsafe { libc::dlsym(handle, c"bridge_call".as_ptr()) };
        assert!(!symbol.is_null());
        // SAFETY: bridge.c defines bridge_call with this signature, and its
        // frame has unwind tables (-fexceptions).
        let bridge_call = unsafe {
            std::mem::transmute::<*mut c_void, unsafe extern "C-unwind" fn(extern "C-unwind" fn())>(
                symbol,
            )
        };
        extern "C-unwind" fn callback() {
            panic!("unwinding through the bridge");
        }
        // SAFETY: as above.
        std::panic::catch_unwind(|| unsafe { bridge_call(callback) }).is_err()
    }

    /// Writes the C bridge into a new directory and builds it; returns the
    /// directory and the library.
    fn build_bridge(label: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let directory = std::env::temp_dir().join(format!(
            "hermit-glibc-compat-{label}-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let source = directory.join("bridge.c");
        let bridge = directory.join("bridge.so");
        std::fs::write(
            &source,
            "void bridge_call(void (*callback)(void)) { callback(); }\n",
        )
        .unwrap();
        let built = Command::new("cc")
            .args(["-shared", "-fPIC", "-fexceptions", "-O0", "-o"])
            .arg(&bridge)
            .arg(&source)
            .status()
            .expect("the namespace unwind tests need a C compiler (cc) on PATH");
        assert!(built.success(), "cc failed: {built}");
        (directory, bridge)
    }

    /// In a preload test's child, the C bridge's path: the child catches a
    /// panic through it in a new `dlmopen` namespace before libtest's main
    /// (whose later `dlsym` calls could read a poisoned preload) and exits.
    const PRELOAD_BRIDGE: &str = "HERMIT_GLIBC_COMPAT_UNWIND_PRELOAD_BRIDGE";

    #[used]
    #[unsafe(link_section = ".init_array")]
    static PRELOAD_NEWLM_CHECK: extern "C" fn() = preload_newlm_check;

    extern "C" fn preload_newlm_check() {
        let Some(path) = std::env::var_os(PRELOAD_BRIDGE) else {
            return;
        };
        let path = CString::new(path.into_encoded_bytes()).unwrap();
        let line: &[u8] = if catch_a_panic_through_the_bridge("new", &path) {
            b"caught=1\n"
        } else {
            b"caught=0\n"
        };
        // SAFETY: writes a static buffer to stdout, then exits without
        // running anything else in this process.
        unsafe {
            libc::write(1, line.as_ptr().cast(), line.len());
            libc::_exit(0);
        }
    }

    /// A preload whose constructor makes the page holding its own dynamic
    /// section unreadable.
    const POISON_LIBRARY: &str = r#"
#include <stdint.h>
#include <sys/mman.h>
#include <unistd.h>
extern char _DYNAMIC[] __attribute__((visibility("hidden")));
__attribute__((constructor)) static void hide_dynamic(void) {
    uintptr_t page = (uintptr_t)sysconf(_SC_PAGESIZE);
    mprotect((void *)((uintptr_t)_DYNAMIC & ~(page - 1)), page, PROT_NONE);
}
"#;

    /// Runs this binary with `preload` preloaded, catching a panic through a
    /// frame in a new `dlmopen` namespace before main, and requires the catch;
    /// then removes `directory`.
    fn newlm_catch_with_a_preload(
        label: &str,
        directory: &std::path::Path,
        bridge: &std::path::Path,
        preload: &std::path::Path,
    ) {
        let output = Command::new(std::env::current_exe().unwrap())
            .env(PRELOAD_BRIDGE, bridge)
            .env("LD_PRELOAD", preload)
            .output()
            .unwrap();
        std::fs::remove_dir_all(directory).unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && stdout.trim() == "caught=1",
            "the {label} child exited with {}\nstdout:\n{stdout}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// With a preload named libc.so.6 (another soname) whose dynamic section is
    /// unreadable, ahead of libc, a panic through a frame in a new `dlmopen`
    /// namespace is still caught: the lookup neither reads nor picks it.
    #[test]
    fn a_newlm_panic_is_caught_past_an_object_named_libc_with_an_unreadable_dynamic_section() {
        let (directory, bridge) = build_bridge("impostor");
        let source = directory.join("impostor.c");
        let impostor = directory.join("libc.so.6");
        std::fs::write(&source, POISON_LIBRARY).unwrap();
        let built = Command::new("cc")
            .args(["-shared", "-fPIC", "-O0", "-Wl,-soname,impostor.so", "-o"])
            .arg(&impostor)
            .arg(&source)
            .status()
            .expect("the namespace unwind tests need a C compiler (cc) on PATH");
        assert!(built.success(), "cc failed: {built}");
        newlm_catch_with_a_preload("impostor", &directory, &bridge, &impostor);
    }

    /// A preload that interposes `dl_iterate_phdr` and `gnu_get_libc_version`
    /// and forwards each to the next definition (libc's), as profilers' and
    /// sanitizers' wrappers do.
    const FORWARDING_INTERPOSER: &str = r#"
#define _GNU_SOURCE
#include <dlfcn.h>
#include <link.h>
#include <stddef.h>
typedef int (*iterate_function)(int (*)(struct dl_phdr_info *, size_t, void *), void *);
typedef const char *(*version_function)(void);
static iterate_function next_iterate;
static version_function next_version;
__attribute__((constructor)) static void find_the_next_definitions(void) {
    next_iterate = (iterate_function)dlsym(RTLD_NEXT, "dl_iterate_phdr");
    next_version = (version_function)dlsym(RTLD_NEXT, "gnu_get_libc_version");
}
int dl_iterate_phdr(int (*callback)(struct dl_phdr_info *, size_t, void *), void *data) {
    return next_iterate(callback, data);
}
const char *gnu_get_libc_version(void) { return next_version(); }
"#;

    /// With a forwarding interposer of `dl_iterate_phdr` and
    /// `gnu_get_libc_version` preloaded, a panic through a frame in a new
    /// `dlmopen` namespace is still caught: the lookup finds libc by the string
    /// libc returns, not by either function's address.
    #[test]
    fn a_newlm_panic_is_caught_through_a_forwarding_interposer() {
        let (directory, bridge) = build_bridge("interposer");
        let source = directory.join("interposer.c");
        let interposer = directory.join("interposer.so");
        std::fs::write(&source, FORWARDING_INTERPOSER).unwrap();
        let built = Command::new("cc")
            .args(["-shared", "-fPIC", "-O0", "-o"])
            .arg(&interposer)
            .arg(&source)
            .status()
            .expect("the namespace unwind tests need a C compiler (cc) on PATH");
        assert!(built.success(), "cc failed: {built}");
        newlm_catch_with_a_preload("interposer", &directory, &bridge, &interposer);
    }

    /// With the real libc loaded under another name (preloaded through a
    /// symlink), a panic through a frame in a new `dlmopen` namespace is still
    /// caught: the lookup finds libc by address, not name.
    #[test]
    fn a_newlm_panic_is_caught_with_libc_loaded_under_another_name() {
        let (directory, bridge) = build_bridge("alias");
        let mut info: libc::Dl_info = unsafe { std::mem::zeroed() };
        // SAFETY: dladdr fills `info` for an address in a loaded object.
        assert_ne!(
            unsafe { libc::dladdr(libc::dl_iterate_phdr as *const c_void, &mut info) },
            0
        );
        // SAFETY: dladdr's dli_fname is a NUL-terminated path.
        let real = unsafe { CStr::from_ptr(info.dli_fname) }
            .to_str()
            .unwrap()
            .to_owned();
        let alias = directory.join("libc-alias-2.99.so");
        std::os::unix::fs::symlink(std::fs::canonicalize(real).unwrap(), &alias).unwrap();
        newlm_catch_with_a_preload("alias", &directory, &bridge, &alias);
    }

    /// In the child of the early-constructor test, the C bridge's path.
    const EARLY_BRIDGE: &str = "HERMIT_GLIBC_COMPAT_UNWIND_EARLY_BRIDGE";
    /// What the early constructor saw: 0 when it did not run, else
    /// 1 + caught + 2 * (the cache was already resolved).
    static EARLY_OUTCOME: AtomicUsize = AtomicUsize::new(0);

    /// Runs before the preload's own `.init_array` entry, as a constructor of
    /// priority 101 does, and in the early-constructor test's child panics
    /// through a frame in a new `dlmopen` namespace.
    #[used]
    #[unsafe(link_section = ".init_array.00101")]
    static EARLY_CONSTRUCTOR: extern "C" fn() = early_constructor;

    extern "C" fn early_constructor() {
        let Some(path) = std::env::var_os(EARLY_BRIDGE) else {
            return;
        };
        let resolved = REAL_DL_FIND_OBJECT.load(Ordering::Acquire) != UNRESOLVED;
        let path = CString::new(path.into_encoded_bytes()).unwrap();
        let caught = catch_a_panic_through_the_bridge("new", &path);
        EARLY_OUTCOME.store(
            1 + usize::from(caught) + 2 * usize::from(resolved),
            Ordering::SeqCst,
        );
    }

    /// A panic in a constructor that runs before the preload's own
    /// initializer, unwinding through a frame in a new `dlmopen` namespace, is
    /// caught: the first lookup finds glibc's real `_dl_find_object` then.
    #[test]
    fn a_panic_in_an_earlier_constructor_unwinds_through_a_new_dlmopen_namespace() {
        const TEST: &str = "glibc_compat::tests::a_panic_in_an_earlier_constructor_unwinds_through_a_new_dlmopen_namespace";
        if std::env::var_os(EARLY_BRIDGE).is_some() {
            let outcome = EARLY_OUTCOME.load(Ordering::SeqCst);
            println!(
                "early_ran={} resolved_before={} caught={}",
                u8::from(outcome != 0),
                u8::from(outcome >= 3),
                u8::from(outcome == 2 || outcome == 4)
            );
            return;
        }
        let (directory, bridge) = build_bridge("early");
        let stdout = cold_child("early", TEST, &[(EARLY_BRIDGE, bridge.as_os_str())]);
        std::fs::remove_dir_all(&directory).unwrap();
        assert!(
            stdout.contains("early_ran=1 resolved_before=0 caught=1"),
            "{stdout}"
        );
    }

    #[test]
    fn a_panic_unwinds_through_a_frame_in_a_new_dlmopen_namespace() {
        panic_through_bridge(
            "new",
            "glibc_compat::tests::a_panic_unwinds_through_a_frame_in_a_new_dlmopen_namespace",
        );
    }

    #[test]
    fn a_panic_unwinds_through_a_frame_in_the_callers_namespace() {
        panic_through_bridge(
            "same",
            "glibc_compat::tests::a_panic_unwinds_through_a_frame_in_the_callers_namespace",
        );
    }

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

    /// Set in the child process a cold-unwind test starts.
    const COLD_CHILD: &str = "HERMIT_GLIBC_COMPAT_UNWIND_COLD_CHILD";
    /// The constructor library: its constructor runs the function whose
    /// address HERMIT_GLIBC_COMPAT_UNWIND_WORKER holds (set by the child itself)
    /// on a new thread and waits for it, for at most one second, while
    /// `dlopen` holds the loader lock. It records whether it timed out.
    const CONSTRUCTOR_LIBRARY: &str = r#"
#define _GNU_SOURCE
#include <pthread.h>
#include <stdlib.h>
#include <time.h>
int constructor_timed_out = -1;
static void *worker(void *function) {
    ((void (*)(void))function)();
    return 0;
}
__attribute__((constructor)) static void wait_for_worker(void) {
    const char *address = getenv("HERMIT_GLIBC_COMPAT_UNWIND_WORKER");
    pthread_t thread;
    struct timespec deadline;
    if (!address) return;
    pthread_create(&thread, 0, worker, (void *)strtoull(address, 0, 16));
    clock_gettime(CLOCK_REALTIME, &deadline);
    deadline.tv_sec += 1;
    constructor_timed_out = pthread_timedjoin_np(thread, 0, &deadline) != 0;
    if (constructor_timed_out) pthread_detach(thread);
}
"#;

    /// Runs `test` again in a child with COLD_CHILD set to `case`, so its
    /// panic is the first unwind in the process, and returns its stdout once
    /// it exits successfully within ten seconds.
    fn cold_child(case: &str, test: &str, extra: &[(&str, &std::ffi::OsStr)]) -> String {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", test, "--nocapture", "--test-threads=1"])
            .env(COLD_CHILD, case)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        for (name, value) in extra {
            command.env(name, value);
        }
        let mut child = command.spawn().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while child.try_wait().unwrap().is_none() {
            if std::time::Instant::now() > deadline {
                child.kill().unwrap();
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let output = child.wait_with_output().unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        assert!(
            output.status.success(),
            "the {case} child exited with {}\nstdout:\n{stdout}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
        stdout
    }

    static WORKER_CAUGHT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

    extern "C" fn worker_catches_a_panic() {
        let caught = std::panic::catch_unwind(|| panic!("the worker's first unwind")).is_err();
        WORKER_CAUGHT.store(caught, Ordering::SeqCst);
    }

    /// A library constructor waits for a worker thread whose panic is the
    /// process's first unwind. The unwind must not need the loader lock that
    /// `dlopen` holds while the constructor waits, so the constructor must not
    /// time out.
    #[test]
    fn a_constructor_waiting_for_a_workers_first_unwind_does_not_time_out() {
        const TEST: &str = "glibc_compat::tests::a_constructor_waiting_for_a_workers_first_unwind_does_not_time_out";
        if std::env::var(COLD_CHILD).as_deref() == Ok("constructor") {
            // This process's own address of the worker (the parent's differs:
            // the binary is position-independent).
            let worker = CString::new(format!(
                "{:x}",
                worker_catches_a_panic as extern "C" fn() as usize
            ))
            .unwrap();
            // SAFETY: both strings are NUL-terminated, and no other thread of
            // this child reads the environment.
            assert_eq!(
                unsafe {
                    libc::setenv(
                        c"HERMIT_GLIBC_COMPAT_UNWIND_WORKER".as_ptr(),
                        worker.as_ptr(),
                        1,
                    )
                },
                0
            );
            let path = CString::new(std::env::var(BRIDGE_PATH).unwrap()).unwrap();
            // SAFETY: `path` names the constructor library the parent built.
            let handle = unsafe { libc::dlopen(path.as_ptr(), libc::RTLD_NOW) };
            assert!(!handle.is_null());
            // SAFETY: `handle` is a loaded library defining this int.
            let timed_out =
                unsafe { *libc::dlsym(handle, c"constructor_timed_out".as_ptr()).cast::<c_int>() };
            while !WORKER_CAUGHT.load(Ordering::SeqCst) {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            println!("constructor_timed_out={timed_out} worker_caught=1");
            return;
        }
        let directory = std::env::temp_dir().join(format!(
            "hermit-glibc-compat-constructor-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let source = directory.join("constructor.c");
        let library = directory.join("constructor.so");
        std::fs::write(&source, CONSTRUCTOR_LIBRARY).unwrap();
        let built = Command::new("cc")
            .args(["-shared", "-fPIC", "-O0", "-pthread", "-o"])
            .arg(&library)
            .arg(&source)
            .status()
            .expect("the cold-unwind tests need a C compiler (cc) on PATH");
        assert!(built.success(), "cc failed: {built}");
        let stdout = cold_child("constructor", TEST, &[(BRIDGE_PATH, library.as_os_str())]);
        std::fs::remove_dir_all(&directory).unwrap();
        assert!(
            stdout.contains("constructor_timed_out=0 worker_caught=1"),
            "{stdout}"
        );
    }

    /// A caller's pending `dlerror` message survives the process's first
    /// unwind.
    #[test]
    fn the_first_unwind_keeps_a_pending_dlerror() {
        const TEST: &str = "glibc_compat::tests::the_first_unwind_keeps_a_pending_dlerror";
        if std::env::var(COLD_CHILD).as_deref() == Ok("dlerror") {
            // SAFETY: the name is NUL-terminated; it names no library.
            let missing = unsafe {
                libc::dlopen(
                    c"/nonexistent/hermit-glibc-compat-missing.so".as_ptr(),
                    libc::RTLD_NOW,
                )
            };
            assert!(missing.is_null());
            let caught = std::panic::catch_unwind(|| panic!("the first unwind")).is_err();
            // SAFETY: dlerror has no preconditions; a non-null result is a
            // NUL-terminated message.
            let error = unsafe { libc::dlerror() };
            let message = if error.is_null() {
                String::from("<none>")
            } else {
                unsafe { std::ffi::CStr::from_ptr(error) }
                    .to_string_lossy()
                    .into_owned()
            };
            println!("caught={} dlerror={message}", u8::from(caught));
            return;
        }
        let stdout = cold_child("dlerror", TEST, &[]);
        assert!(
            stdout.contains("caught=1 dlerror=")
                && stdout.contains("hermit-glibc-compat-missing.so"),
            "{stdout}"
        );
    }

    #[test]
    fn dl_find_object_finds_this_code_and_its_unwind_tables() {
        find_this_code_and_its_unwind_tables(dl_find_object);
    }

    /// The fallback, which this host would otherwise not reach when its glibc
    /// has the real `_dl_find_object`.
    #[test]
    fn the_dl_iterate_phdr_fallback_finds_this_code_and_its_unwind_tables() {
        unsafe extern "C" fn fallback(pc: *mut c_void, result: *mut DlFindObject) -> c_int {
            // SAFETY: the caller's arguments are `_dl_find_object`'s.
            unsafe { phdr_find_object(pc, result) }
        }
        find_this_code_and_its_unwind_tables(fallback);
    }

    fn find_this_code_and_its_unwind_tables(lookup: DlFindObjectFn) {
        let pc = dl_find_object as *const () as usize;
        let mut result = std::mem::MaybeUninit::<DlFindObject>::uninit();
        assert_eq!(unsafe { lookup(pc as *mut c_void, result.as_mut_ptr()) }, 0);
        let result = unsafe { result.assume_init() };
        let (start, end) = (result.dlfo_map_start as usize, result.dlfo_map_end as usize);
        assert!(
            (start..end).contains(&pc),
            "{start:#x}..{end:#x} misses {pc:#x}"
        );
        // The unwind tables are the object's own PT_GNU_EH_FRAME, read here
        // from its program headers. They need not lie in start..end: glibc's
        // `_dl_find_object` reports the mapping that contains `pc`, and when
        // the linker starts the code segment on a fresh page the tables sit in
        // the read-only segment before it. Measured with glibc 2.42 in the
        // pinned root: one build of this test, whose read-only segment ends
        // at 0xe1fec with code at 0xe3000, got only the code segment, while
        // one whose segments share a page got the whole object. libgcc reads
        // only `dlfo_eh_frame`.
        let eh_frame = result.dlfo_eh_frame as usize;
        let headers = loaded_object_containing(pc).expect("pc is in a loaded object");
        assert_eq!(eh_frame, headers.eh_frame, "eh_frame {eh_frame:#x}");
        // The `.eh_frame_hdr` section starts with version 1.
        assert_eq!(unsafe { *(eh_frame as *const u8) }, 1);
        let mut unused = std::mem::MaybeUninit::<DlFindObject>::uninit();
        assert_eq!(
            unsafe { lookup(std::ptr::null_mut(), unused.as_mut_ptr()) },
            -1
        );
    }
}
