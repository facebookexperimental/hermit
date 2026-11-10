//! Bounded observations at the real Detcore entry and native adapter route.
//!
//! These diagnostics do not change a result, obtain a turn, or grant authority.
//! They are not an alternative Tool. The loader experiment must join their
//! actual instruction pointers to the two client sites, canonical answers,
//! successful finalization, and the bound engine/shim/coordinator artifacts.

use core::fmt::Write;
use core::sync::atomic::AtomicU64;
use core::sync::atomic::Ordering;

use detcore::dispatch_observer::Entry;
use reverie_inguest::guest::context::RegisterContext;

const MAX_RECORDS: u64 = 64;
static TOOL_RECORDS: AtomicU64 = AtomicU64::new(0);
static ROUTE_RECORDS: AtomicU64 = AtomicU64::new(0);
// Keep brk diagnosis separate so it cannot consume the two-site observation
// population. This records the original request, never a replacement answer.
static BRK_RECORDS: AtomicU64 = AtomicU64::new(0);

struct Line {
    bytes: [u8; 192],
    length: usize,
}

impl Line {
    const fn new() -> Self {
        Self {
            bytes: [0; 192],
            length: 0,
        }
    }

    fn emit(&self) {
        // No allocation, libc interposition, Tool TLS, or scheduled injection.
        // Missing/partial diagnostic output cannot qualify the experiment.
        let mut at = 0;
        while at < self.length {
            let written = unsafe {
                reverie_inguest::trap::raw_syscall6(
                    libc::SYS_write,
                    [
                        2,
                        self.bytes[at..self.length].as_ptr() as u64,
                        (self.length - at) as u64,
                        0,
                        0,
                        0,
                    ],
                )
            };
            if written == -(libc::EINTR as i64) {
                continue;
            }
            if written <= 0 {
                return;
            }
            at += written as usize;
        }
    }
}

impl core::fmt::Write for Line {
    fn write_str(&mut self, value: &str) -> core::fmt::Result {
        let end = self
            .length
            .checked_add(value.len())
            .ok_or(core::fmt::Error)?;
        let destination = self
            .bytes
            .get_mut(self.length..end)
            .ok_or(core::fmt::Error)?;
        destination.copy_from_slice(value.as_bytes());
        self.length = end;
        Ok(())
    }
}

pub(crate) fn install() {
    if detcore::dispatch_observer::install(tool_entry).is_err() {
        crate::install::fail("the Detcore entry observer was already installed");
    }
}

fn tool_entry(entry: Entry) {
    if entry.number == libc::SYS_brk as u64 {
        if BRK_RECORDS.fetch_add(1, Ordering::Relaxed) < MAX_RECORDS {
            let mut line = Line::new();
            if writeln!(
                line,
                "ENGINE_BRK_ENTRY_V1 pid={} tid={} dettid={} ip={:#x} address={:#x}",
                entry.pid, entry.tid, entry.dettid, entry.instruction_pointer, entry.arguments[0],
            )
            .is_ok()
            {
                line.emit();
            }
        }
        return;
    }
    if entry.number != libc::SYS_getpid as u64
        || TOOL_RECORDS.fetch_add(1, Ordering::Relaxed) >= MAX_RECORDS
    {
        return;
    }
    let mut line = Line::new();
    if writeln!(
        line,
        "ENGINE_TOOL_ENTRY_V1 pid={} tid={} dettid={} nr={} ip={:#x}",
        entry.pid, entry.tid, entry.dettid, entry.number, entry.instruction_pointer,
    )
    .is_ok()
    {
        line.emit();
    }
}

pub(crate) fn native_route(context: &RegisterContext, number: i64) {
    if number != libc::SYS_getpid || ROUTE_RECORDS.fetch_add(1, Ordering::Relaxed) >= MAX_RECORDS {
        return;
    }
    let mut line = Line::new();
    if writeln!(
        line,
        "ENGINE_NATIVE_ROUTE_V1 nr={} ip={:#x}",
        number, context.instruction_pointer,
    )
    .is_ok()
    {
        line.emit();
    }
}
