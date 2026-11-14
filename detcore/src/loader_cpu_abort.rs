/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! A typed refusal for a program interpreter that aborts because the virtual
//! CPU is below the x86-64 level it was built for
//! (<https://github.com/rrnewton/hermit/issues/4695>).
//!
//! glibc's dynamic loader checks the CPU's x86-64 level before it opens any
//! library or first tries to set the thread pointer. A loader built for
//! x86-64-v3 (CentOS Stream 10's, for example) that finds an x86-64-v2 CPU
//! prints `Fatal glibc error: CPU does not support x86-64-v3` and exits 127.
//! Without this module that run ends as an ordinary guest exit 127, with no
//! word from Hermit about the CPU profile.
//! Such a loader need not declare its level in a GNU property note, so
//! [`crate::Detcore::isa_refusal`] cannot see it before it runs. Its abort is
//! recognised here instead, and only its abort.
//!
//! The run is refused (exit 122) only when ALL of these hold for one thread:
//!
//! 1. the exec that started it loaded a program interpreter (`AT_BASE` is
//!    set), under a profile with a fixed x86-64 level whose CPUID Detcore
//!    answers;
//! 2. since that exec the thread has not attempted `arch_prctl(ARCH_SET_FS)`,
//!    successfully or not, has neither opened nor mapped a file, and
//!    every system call it made came from the interpreter's own mappings: the
//!    loader is still in its startup, before any library or the program's
//!    `main` (a statically linked program has no interpreter, so it is
//!    never watched);
//! 3. it wrote, to standard error, exactly the loader's message: the bytes of
//!    [`MESSAGE_PREFIX`], a level, and a newline, nothing before or after;
//! 4. its very next system call is `exit_group(127)`, again from the
//!    interpreter's mappings.
//!
//! Why the first `ARCH_SET_FS` attempt ends the startup. A program can call
//! the loader's own `_dl_fatal_printf` once it runs, and a program with no
//! library besides the loader opens and maps no file, so neither the
//! instruction address nor the file calls alone tell the loader's startup
//! from the program. The attempt to set the thread pointer does, as both
//! quoted loaders' code shows (file offsets in each, el9 then el10):
//!
//! - the level check runs in `_dl_sysdep_start`, which calls
//!   `_dl_x86_init_cpu_features` (0x1e5a3, 0x1d4a3) before `dl_main`
//!   (0x1e5f1, 0x1d4f1); the only other code naming the message,
//!   `__x86_cpu_features_ifunc`, shares its run-once guard;
//! - each loader's only `arch_prctl(ARCH_SET_FS)` calls (0x1f7bf and 0x22b83;
//!   0x1e830 and 0x21acc) are in `dl_main` and the `init_tls` it calls, which
//!   sets the thread pointer before `dl_main` relocates any object, where an
//!   IFUNC resolver, the first code of another object, can run (the program's
//!   constructors and entry come after `dl_main` returns);
//! - the el9 loader queries CET with `arch_prctl(0x3001)` inside its CPU
//!   setup, before the level check, so only `ARCH_SET_FS` ends the watch.
//!
//! Traced natively under both loaders, a program with no C library whose
//! only `DT_NEEDED` is the loader and whose `main` calls `_dl_fatal_printf`
//! with the message exits 127 without opening or mapping a file; its
//! `arch_prctl(ARCH_SET_FS)` comes before the message, so it keeps its exit.
//! The watch ends at the attempt, at syscall entry, whatever its result.
//! For these two loaders that decides every run as ending it at a successful
//! call would: their level check comes before any attempt, and a failed
//! attempt is followed only by their own fatal error and exit 127 (el9
//! 0x1f7f0 and 0x22b8d, "cannot set up thread-local storage: ..."; el10
//! 0x1e883 and 0x22cf4, "Fatal glibc error: Cannot allocate TLS block"),
//! never by the level message, so a watch kept past it would end at that
//! exit unrefused all the same.
//!
//! What this argument does not cover, for these two loaders' builds as for
//! any other: an interpreter (not glibc's) that runs code of another object
//! before its first `ARCH_SET_FS` attempt, or without one, could still have
//! that code's exact message and exit 127 refused; a glibc that set the
//! thread pointer without the system call could too; and a glibc that moved
//! its CPU-level check after its first `ARCH_SET_FS` would end the watch
//! before its own abort, which would then exit 127 unrefused.
//!
//! The loader's message reaches standard error as the loader wrote it; only
//! its exit is replaced by Hermit's refusal. Whether a run is refused is a
//! function of the guest's own system calls, so a `--verify` pair and a
//! replay decide it alike.

use detcore_model::cpu_profile::CpuProfile;
use reverie::syscalls::Addr;
use reverie::syscalls::MemoryAccess;
use serde::Deserialize;
use serde::Serialize;

use crate::exec_record::x86_64_level_name;

/// What glibc's loader writes before the level when the CPU is below the
/// x86-64 level the loader was built for. Quoted from the loader binaries
/// themselves: `ld-linux-x86-64.so.2` from glibc-2.34-288.el9 (SHA-256
/// 861e6e23bb12212683d535c00e0fd752cac36b760fb4bc0770049281f08f6d84, at file
/// offset 0x308e0) and from glibc-2.39-148.el10 (SHA-256
/// c2e7622f474e937212e74d75c32b11729045218a097303b4679a4c7ceed8ccbd, at file
/// offset 0x2d128) both hold the same format string,
/// `Fatal glibc error: CPU does not support x86-64-v%d\n`, which
/// `_dl_fatal_printf` fills with the level the loader was built for (2 for
/// the el9 loader, 3 for the el10 loader) before it calls `_exit (127)`.
pub(crate) const MESSAGE_PREFIX: &[u8] = b"Fatal glibc error: CPU does not support x86-64-v";

/// The status glibc's loader exits with after a fatal error.
pub(crate) const LOADER_FATAL_EXIT: i32 = 127;

/// The longest write that can be the loader's message: the prefix, a level
/// of at most two digits, and the newline. A longer write is not read.
pub(crate) const MAX_MESSAGE_LEN: usize = MESSAGE_PREFIX.len() + 3;

/// The most `iovec`s a `writev` may use and still be read as the message
/// (glibc writes it as three: the prefix, the level, the newline).
pub(crate) const MAX_MESSAGE_IOVECS: usize = 8;

/// The x86-64 level the loader's message names, when `bytes` is exactly that
/// message: [`MESSAGE_PREFIX`], a decimal level from 1 to 99 without a
/// leading zero, and one newline.
pub(crate) fn parse_message(bytes: &[u8]) -> Option<u8> {
    let digits = bytes.strip_prefix(MESSAGE_PREFIX)?.strip_suffix(b"\n")?;
    if digits.is_empty()
        || digits.len() > 2
        || digits[0] == b'0'
        || !digits.iter().all(u8::is_ascii_digit)
    {
        return None;
    }
    std::str::from_utf8(digits).ok()?.parse().ok()
}

/// The bytes a `write(2, buf, len)` writes, where `len` is at most
/// [`MAX_MESSAGE_LEN`] and the guest's memory holds them; `None` otherwise.
/// Only reads: the call itself is handled as it would be.
pub(crate) fn read_write<M: MemoryAccess>(
    memory: &M,
    buf: Option<Addr<u8>>,
    len: usize,
) -> Option<Vec<u8>> {
    if len > MAX_MESSAGE_LEN {
        return None;
    }
    let mut bytes = vec![0; len];
    if len > 0 {
        memory.read_exact(buf?, &mut bytes).ok()?;
    }
    Some(bytes)
}

/// The bytes a `writev(2, iov, count)` writes, concatenated, where `count`
/// is at most [`MAX_MESSAGE_IOVECS`], their total at most
/// [`MAX_MESSAGE_LEN`], and the guest's memory holds the array and every
/// buffer; `None` otherwise. Only reads: the call itself is handled as it
/// would be.
pub(crate) fn read_writev<M: MemoryAccess>(
    memory: &M,
    iov: Option<Addr<libc::iovec>>,
    count: usize,
) -> Option<Vec<u8>> {
    if count > MAX_MESSAGE_IOVECS {
        return None;
    }
    let mut iovecs = vec![
        libc::iovec {
            iov_base: std::ptr::null_mut(),
            iov_len: 0,
        };
        count
    ];
    if count > 0 {
        memory.read_values(iov?, &mut iovecs).ok()?;
    }
    let total = iovecs
        .iter()
        .try_fold(0_usize, |total, iovec| total.checked_add(iovec.iov_len))?;
    if total > MAX_MESSAGE_LEN {
        return None;
    }
    let mut bytes = Vec::with_capacity(total);
    for iovec in iovecs.iter().filter(|iovec| iovec.iov_len > 0) {
        let mut part = vec![0; iovec.iov_len];
        memory
            .read_exact(Addr::<u8>::from_raw(iovec.iov_base as usize)?, &mut part)
            .ok()?;
        bytes.extend_from_slice(&part);
    }
    Some(bytes)
}

/// One thread's watch over the program interpreter its last exec loaded,
/// from that exec until the interpreter's startup is over.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Watch {
    /// The interpreter's mappings: from the start of its first to the end of
    /// its last, `[start, end)`.
    pub(crate) start: u64,
    pub(crate) end: u64,
    /// The interpreter's pathname, as the process's maps name it.
    pub(crate) loader: String,
    /// The executable the exec ran, as `/proc/<pid>/exe` names it.
    pub(crate) executable: String,
    /// The level the loader's message named, once the loader wrote it.
    pub(crate) printed: Option<u8>,
}

/// One system call, as the watch sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Event<'a> {
    /// A `write` or `writev` to file descriptor 2, with the bytes it writes
    /// where they are at most [`MAX_MESSAGE_LEN`] long and readable.
    StderrWrite(Option<&'a [u8]>),
    /// `exit_group` with this status.
    ExitGroup(i32),
    /// A call that opens a file or maps one: the loader has moved on to its
    /// libraries.
    OpensOrMapsFile,
    /// An `arch_prctl(ARCH_SET_FS)` attempt, successful or not: the loader
    /// tries to set the thread pointer, after its level check and before any
    /// other object's code runs (see the module documentation). No other
    /// `arch_prctl` command is one: the el9 loader queries CET with 0x3001
    /// before its level check.
    SetsThreadPointer,
    /// Any other call.
    Other,
}

/// What the watch sees of the system call `call`, where `written` is `Some`
/// for a `write` or `writev` to standard error, holding the bytes it writes
/// when [`read_write`] or [`read_writev`] could read them. Detcore's syscall
/// handler classifies each call with this while a watch is kept
/// (`Detcore::watch_loader_cpu_abort`).
#[cfg(target_arch = "x86_64")]
pub(crate) fn classify<'a>(
    call: &reverie::syscalls::Syscall,
    written: Option<Option<&'a [u8]>>,
) -> Event<'a> {
    use reverie::syscalls::ArchPrctlCmd;
    use reverie::syscalls::MapFlags;
    use reverie::syscalls::Syscall;
    match (call, written) {
        (_, Some(bytes)) => Event::StderrWrite(bytes),
        (Syscall::ExitGroup(e), None) => Event::ExitGroup(e.status()),
        (Syscall::Open(_) | Syscall::Openat(_) | Syscall::Openat2(_) | Syscall::Creat(_), None) => {
            Event::OpensOrMapsFile
        }
        (Syscall::Mmap(m), None) if !m.flags().contains(MapFlags::MAP_ANONYMOUS) => {
            Event::OpensOrMapsFile
        }
        // The command alone decides, never the call's result.
        (Syscall::ArchPrctl(a), None) if matches!(a.cmd(), ArchPrctlCmd::ARCH_SET_FS(_)) => {
            Event::SetsThreadPointer
        }
        _ => Event::Other,
    }
}

/// What a system call does to the watch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Step {
    /// Keep watching, unchanged.
    Keep,
    /// The interpreter's startup is over, or this was not its abort: stop
    /// watching.
    Clear,
    /// The loader wrote its message naming this level.
    Printed(u8),
    /// The loader exited after its message: refuse the run, for this level.
    Refuse(u8),
}

impl Watch {
    /// Whether `address` lies in the interpreter's mappings.
    pub(crate) fn contains(&self, address: u64) -> bool {
        self.start <= address && address < self.end
    }

    /// What a system call made from instruction address `address` does.
    pub(crate) fn step(&self, address: u64, event: Event<'_>) -> Step {
        if !self.contains(address) {
            // Code outside the interpreter made a system call: the program
            // or a library runs, so the loader's startup is over.
            return Step::Clear;
        }
        match (self.printed, event) {
            (None, Event::StderrWrite(Some(bytes))) => {
                parse_message(bytes).map_or(Step::Keep, Step::Printed)
            }
            (None, Event::StderrWrite(None) | Event::Other) => Step::Keep,
            (None, Event::OpensOrMapsFile | Event::SetsThreadPointer | Event::ExitGroup(_)) => {
                Step::Clear
            }
            (Some(level), Event::ExitGroup(LOADER_FATAL_EXIT)) => Step::Refuse(level),
            // The message was not followed by the loader's exit.
            (Some(_), _) => Step::Clear,
        }
    }

    /// The refusal of a run whose loader aborted for `level` under `profile`.
    pub(crate) fn refusal(&self, profile: CpuProfile, level: u8) -> String {
        let presented = profile.x86_64_level().unwrap_or(0);
        let printed = format!("{}{level}", String::from_utf8_lossy(MESSAGE_PREFIX));
        let why = if level > presented {
            let remedy = if level == 3 {
                "rerun with --cpu-profile x86-64-v3, on a host that qualifies for it"
            } else {
                "no CPU profile presents it"
            };
            format!(
                "the {} CPU this profile presents is below the {} it was built for; {remedy}",
                x86_64_level_name(presented),
                x86_64_level_name(level),
            )
        } else {
            format!(
                "although this profile presents {}, which satisfies {}: Hermit's CPU table \
                 lacks a feature the loader checks, so please report this message",
                x86_64_level_name(presented),
                x86_64_level_name(level),
            )
        };
        format!(
            "--cpu-profile {profile} refused: the program interpreter {} of {} stopped before \
             the program started, printing \"{printed}\" and exiting {LOADER_FATAL_EXIT}; {why}",
            self.loader, self.executable,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The loader's message as glibc's `_dl_fatal_printf` fills the format
    /// both quoted loaders hold.
    fn message(level: u8) -> Vec<u8> {
        format!("Fatal glibc error: CPU does not support x86-64-v{level}\n").into_bytes()
    }

    fn watch() -> Watch {
        Watch {
            start: 0x7f00_0000_0000,
            end: 0x7f00_0003_0000,
            loader: "/lib64/ld-linux-x86-64.so.2".into(),
            executable: "/usr/bin/true".into(),
            printed: None,
        }
    }

    const IN: u64 = 0x7f00_0001_2345;
    const OUT: u64 = 0x5555_5555_0000;

    #[test]
    fn the_message_is_exactly_the_quoted_format_filled_with_a_level() {
        assert_eq!(MESSAGE_PREFIX.len(), 48);
        assert_eq!(parse_message(&message(2)), Some(2));
        assert_eq!(parse_message(&message(3)), Some(3));
        assert_eq!(parse_message(&message(4)), Some(4));
        assert_eq!(
            parse_message(b"Fatal glibc error: CPU does not support x86-64-v12\n"),
            Some(12)
        );
        assert!(message(3).len() <= MAX_MESSAGE_LEN);
    }

    #[test]
    fn anything_else_is_not_the_message() {
        for bytes in [
            &b""[..],
            b"Fatal glibc error: CPU does not support x86-64-v",
            b"Fatal glibc error: CPU does not support x86-64-v\n",
            b"Fatal glibc error: CPU does not support x86-64-v3",
            b"Fatal glibc error: CPU does not support x86-64-v3\n\n",
            b"Fatal glibc error: CPU does not support x86-64-v03\n",
            b"Fatal glibc error: CPU does not support x86-64-v123\n",
            b"Fatal glibc error: CPU does not support x86-64-vx\n",
            b"fatal glibc error: CPU does not support x86-64-v3\n",
            b"prog: Fatal glibc error: CPU does not support x86-64-v3\n",
            b"Fatal glibc error: CPU ISA level is lower than required\n",
            b"hello\n",
        ] {
            assert_eq!(
                parse_message(bytes),
                None,
                "{:?}",
                String::from_utf8_lossy(bytes)
            );
        }
    }

    #[test]
    fn the_loaders_message_then_its_exit_is_refused() {
        let mut watch = watch();
        let message = message(3);
        assert_eq!(watch.step(IN, Event::Other), Step::Keep); // brk
        assert_eq!(
            watch.step(IN, Event::StderrWrite(Some(&message))),
            Step::Printed(3)
        );
        watch.printed = Some(3);
        assert_eq!(watch.step(IN, Event::ExitGroup(127)), Step::Refuse(3));
    }

    #[test]
    fn the_message_must_come_from_the_loader_and_be_followed_by_its_exit() {
        let message = message(3);
        let mut watch = watch();
        // Written by code outside the interpreter: the program runs.
        assert_eq!(
            watch.step(OUT, Event::StderrWrite(Some(&message))),
            Step::Clear
        );
        // Another write to stderr keeps the watch, and matches nothing.
        assert_eq!(
            watch.step(IN, Event::StderrWrite(Some(b"hello\n"))),
            Step::Keep
        );
        assert_eq!(watch.step(IN, Event::StderrWrite(None)), Step::Keep);
        watch.printed = Some(3);
        // Any status but the loader's.
        assert_eq!(watch.step(IN, Event::ExitGroup(1)), Step::Clear);
        assert_eq!(watch.step(IN, Event::ExitGroup(0)), Step::Clear);
        // Anything but an exit after the message.
        assert_eq!(watch.step(IN, Event::Other), Step::Clear);
        assert_eq!(
            watch.step(IN, Event::StderrWrite(Some(&message))),
            Step::Clear
        );
        // The exit from outside the interpreter.
        assert_eq!(watch.step(OUT, Event::ExitGroup(127)), Step::Clear);
    }

    #[test]
    fn opening_or_mapping_a_file_ends_the_loaders_startup() {
        let watch = watch();
        assert_eq!(watch.step(IN, Event::OpensOrMapsFile), Step::Clear);
        // An exit with no message first is no abort of this kind.
        assert_eq!(watch.step(IN, Event::ExitGroup(127)), Step::Clear);
    }

    /// One system call of a traced run: whether it came from the
    /// interpreter's mappings, and what the watch sees of it.
    type Traced<'a> = (bool, Event<'a>);

    /// Steps a fresh watch through `trace` as Detcore does: `Ok` with the
    /// level of a refusal, or `Err` with the index of the call that ended the
    /// watch (the trace's length if none did).
    fn replay(trace: &[Traced<'_>]) -> Result<u8, usize> {
        let mut watch = watch();
        for (index, &(in_loader, event)) in trace.iter().enumerate() {
            match watch.step(if in_loader { IN } else { OUT }, event) {
                Step::Keep => {}
                Step::Clear => return Err(index),
                Step::Printed(level) => watch.printed = Some(level),
                Step::Refuse(level) => return Ok(level),
            }
        }
        Err(trace.len())
    }

    /// The system calls of a program with no C library, whose only
    /// `DT_NEEDED` is the loader and whose `main` calls the loader's exported
    /// `_dl_fatal_printf` with the message, as `strace -i` showed them under
    /// each quoted loader started by the kernel. Every call comes from the
    /// loader's text and none opens or maps a file. Only the el9 loader makes
    /// the CET query.
    fn fatal_printf_from_main(message: &[u8], cet_query: bool) -> Vec<Traced<'_>> {
        let mut trace = vec![(true, Event::Other)]; // brk(NULL)
        if cet_query {
            trace.push((true, Event::Other)); // arch_prctl(0x3001)
        }
        trace.extend([
            (true, Event::Other),                        // mmap(MAP_ANONYMOUS)
            (true, Event::Other),                        // access("/etc/ld.so.preload")
            (true, Event::SetsThreadPointer),            // arch_prctl(ARCH_SET_FS), init_tls
            (true, Event::Other),                        // set_tid_address
            (true, Event::Other),                        // set_robust_list
            (true, Event::Other),                        // rseq
            (true, Event::Other),                        // mprotect, RELRO
            (true, Event::Other),                        // mprotect, RELRO
            (true, Event::StderrWrite(Some(message))),   // writev, _dl_debug_vdprintf
            (true, Event::ExitGroup(LOADER_FATAL_EXIT)), // exit_group, _Exit
        ]);
        trace
    }

    /// A program that calls the loader's `_dl_fatal_printf` from `main`
    /// keeps its exit: the watch ends at the loader's
    /// `arch_prctl(ARCH_SET_FS)`, before the program runs. The same trace
    /// without that call is refused, so it is the thread pointer, not an
    /// address or a file call, that tells the two apart.
    #[test]
    fn a_program_calling_the_loaders_fatal_printf_from_main_is_not_refused() {
        let message = message(3);
        for (loader, cet_query) in [("el9", true), ("el10", false)] {
            let trace = fatal_printf_from_main(&message, cet_query);
            let set_fs = trace
                .iter()
                .position(|&(_, event)| event == Event::SetsThreadPointer)
                .unwrap();
            assert_eq!(replay(&trace), Err(set_fs), "{loader}");
            let without: Vec<Traced<'_>> = trace
                .iter()
                .copied()
                .filter(|&(_, event)| event != Event::SetsThreadPointer)
                .collect();
            assert_eq!(replay(&without), Ok(3), "{loader} without ARCH_SET_FS");
        }
    }

    /// The loader's own abort comes before it sets the thread pointer, after
    /// its program break and, on el9, its CET query, neither of which ends
    /// the watch. With the thread pointer set first it is no such abort.
    #[test]
    fn the_loaders_abort_before_its_thread_pointer_is_refused() {
        let message = message(3);
        let brk = (true, Event::Other);
        let cet_query = (true, Event::Other);
        let printed = (true, Event::StderrWrite(Some(&message[..])));
        let exit = (true, Event::ExitGroup(LOADER_FATAL_EXIT));
        assert_eq!(replay(&[brk, cet_query, printed, exit]), Ok(3));
        assert_eq!(replay(&[brk, printed, exit]), Ok(3));
        let set_fs = (true, Event::SetsThreadPointer);
        assert_eq!(replay(&[brk, set_fs, printed, exit]), Err(1));
        // From outside the interpreter it ends the watch as any call does.
        assert_eq!(replay(&[brk, (false, Event::SetsThreadPointer)]), Err(1));
    }

    /// One system call as Detcore receives it, with what a `write` or
    /// `writev` to standard error writes (`classify`'s `written`).
    #[cfg(target_arch = "x86_64")]
    type Call<'a> = (reverie::syscalls::Syscall, Option<Option<&'a [u8]>>);

    /// The system call `number` with `args`, as Detcore receives it.
    #[cfg(target_arch = "x86_64")]
    fn raw(number: reverie::syscalls::Sysno, args: [usize; 6]) -> reverie::syscalls::Syscall {
        reverie::syscalls::Syscall::from_raw(
            number,
            reverie::syscalls::SyscallArgs::new(
                args[0], args[1], args[2], args[3], args[4], args[5],
            ),
        )
    }

    /// `arch_prctl(command, address)`, writing nothing to standard error.
    #[cfg(target_arch = "x86_64")]
    fn arch_prctl<'a>(command: usize, address: usize) -> Call<'a> {
        let call = raw(
            reverie::syscalls::Sysno::arch_prctl,
            [command, address, 0, 0, 0, 0],
        );
        (call, None)
    }

    /// [`replay`] of `calls` from the interpreter, each classified as
    /// Detcore's syscall handler classifies it ([`classify`]).
    #[cfg(target_arch = "x86_64")]
    fn replay_calls(calls: &[Call<'_>]) -> Result<u8, usize> {
        let events: Vec<Traced<'_>> = calls
            .iter()
            .map(|&(call, written)| (true, classify(&call, written)))
            .collect();
        replay(&events)
    }

    /// Only an `arch_prctl(ARCH_SET_FS)` attempt is the thread-pointer event,
    /// whether the kernel would accept its address or not: never another
    /// command, the el9 loader's CET query (0x3001) above all, which comes
    /// before its level check.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn only_an_arch_prctl_set_fs_attempt_is_the_thread_pointer_event() {
        use reverie::syscalls::Sysno;
        const ADDRESS: usize = 0x7f00_0000_1000;
        // A canonical address, and one ARCH_SET_FS refuses with EPERM.
        for address in [ADDRESS, 1 << 63] {
            let (call, written) = arch_prctl(0x1002, address);
            assert_eq!(classify(&call, written), Event::SetsThreadPointer);
        }
        // ARCH_SET_GS, ARCH_GET_FS, ARCH_GET_GS, ARCH_GET_CPUID,
        // ARCH_SET_CPUID, the CET status query, ARCH_SHSTK_ENABLE and an
        // unknown command.
        for command in [
            0x1001, 0x1003, 0x1004, 0x1011, 0x1012, 0x3001, 0x5001, 0x9999,
        ] {
            let (call, written) = arch_prctl(command, ADDRESS);
            assert_eq!(classify(&call, written), Event::Other, "{command:#x}");
        }
        let message = message(3);
        let writev = raw(Sysno::writev, [2, ADDRESS, 3, 0, 0, 0]);
        assert_eq!(
            classify(&writev, Some(Some(&message[..]))),
            Event::StderrWrite(Some(&message[..]))
        );
        assert_eq!(classify(&writev, Some(None)), Event::StderrWrite(None));
        let exit_group = raw(Sysno::exit_group, [127, 0, 0, 0, 0, 0]);
        assert_eq!(classify(&exit_group, None), Event::ExitGroup(127));
        let openat = raw(Sysno::openat, [(-100_isize) as usize, ADDRESS, 0, 0, 0, 0]);
        assert_eq!(classify(&openat, None), Event::OpensOrMapsFile);
        // mmap(NULL, 4096, PROT_READ, MAP_PRIVATE, 3, 0), then anonymous.
        let mapped = raw(Sysno::mmap, [0, 4096, 1, 0x02, 3, 0]);
        assert_eq!(classify(&mapped, None), Event::OpensOrMapsFile);
        let anonymous = raw(Sysno::mmap, [0, 4096, 3, 0x22, usize::MAX, 0]);
        assert_eq!(classify(&anonymous, None), Event::Other);
        assert_eq!(classify(&raw(Sysno::brk, [0; 6]), None), Event::Other);
    }

    /// Through Detcore's own classification of raw system calls: the el9
    /// loader's CET query, or any other `arch_prctl` command, before the
    /// message keeps the watch, so the loader's abort is still refused; an
    /// `ARCH_SET_FS` attempt ends it, successful or not. A classifier that
    /// took every `arch_prctl` for the thread pointer fails here.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn the_production_classifier_ends_the_watch_only_at_set_fs() {
        use reverie::syscalls::Sysno;
        const ADDRESS: usize = 0x7f00_0000_1000;
        let message = message(3);
        let brk: Call<'_> = (raw(Sysno::brk, [0; 6]), None);
        let printed: Call<'_> = (
            raw(Sysno::writev, [2, ADDRESS, 3, 0, 0, 0]),
            Some(Some(&message[..])),
        );
        let exit: Call<'_> = (raw(Sysno::exit_group, [127, 0, 0, 0, 0, 0]), None);
        for command in [0x3001, 0x1001, 0x1003, 0x1004] {
            let other = arch_prctl(command, ADDRESS);
            assert_eq!(
                replay_calls(&[brk, other, printed, exit]),
                Ok(3),
                "{command:#x}"
            );
        }
        for address in [ADDRESS, 1 << 63] {
            let set_fs = arch_prctl(0x1002, address);
            assert_eq!(
                replay_calls(&[brk, set_fs, printed, exit]),
                Err(1),
                "{address:#x}"
            );
        }
    }

    #[test]
    fn the_refusal_names_the_loader_the_level_and_the_remedy() {
        let watch = watch();
        let v3 = watch.refusal(CpuProfile::X86_64V2, 3);
        assert_eq!(
            v3,
            "--cpu-profile x86-64-v2 refused: the program interpreter \
             /lib64/ld-linux-x86-64.so.2 of /usr/bin/true stopped before the program \
             started, printing \"Fatal glibc error: CPU does not support x86-64-v3\" and \
             exiting 127; the x86-64-v2 CPU this profile presents is below the x86-64-v3 it \
             was built for; rerun with --cpu-profile x86-64-v3, on a host that qualifies \
             for it"
        );
        let v4 = watch.refusal(CpuProfile::X86_64V3, 4);
        assert!(v4.starts_with("--cpu-profile x86-64-v3 refused: "), "{v4}");
        assert!(
            v4.ends_with("below the x86-64-v4 it was built for; no CPU profile presents it"),
            "{v4}"
        );
        let v2 = watch.refusal(CpuProfile::X86_64V2, 2);
        assert!(
            v2.contains("printing \"Fatal glibc error: CPU does not support x86-64-v2\"")
                && v2.contains(
                    "although this profile presents x86-64-v2, which satisfies x86-64-v2"
                ),
            "{v2}"
        );
    }
}
