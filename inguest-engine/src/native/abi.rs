//! Scalar native callback ABI. Rust state is never a wire representation.

use std::io;

use reverie::Pid;
use reverie_inguest::guest::host::native_state::NativeThreadIdentity;
use reverie_inguest::guest::native_capture::MappingCapture;
use reverie_inguest::guest::native_rpc::NativeCurrent;
use reverie_inguest::guest::native_rpc::NativeInvocation;
use reverie_inguest::guest::native_rpc::NativeInvoker;
use reverie_inguest::guest::native_services::ControllerCaptureControl;
use reverie_inguest::guest::native_services::MemoryCompareExchange;
use reverie_inguest::guest::native_services::MemoryReader;
use reverie_inguest::guest::native_services::NativeEventFacts;
use reverie_inguest::guest::native_services::NativeServiceTable;
use reverie_inguest::guest::native_services::NativeServices;
use reverie_inguest::guest::native_services::RegisterReader;
use reverie_inguest::guest::native_services::RegisterWriter;
use reverie_inguest::guest::native_services::RuntimePrivateMemoryQuery;

pub(super) const VERSION: u32 = 1;
/// The native owner's actual fault-safe write, returning the prefix written.
/// It is not memcpy and a short write must never be promoted to success.
pub type MemoryWriter = unsafe extern "C" fn(usize, *const u8, usize) -> usize;
const AUXV_LIMIT: usize = 256;

/// This is a table of mechanisms, not a phase or scheduler permission.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct Mechanisms {
    pub version: u32,
    pub size: u32,
    pub current: Option<NativeCurrent>,
    pub invoke: Option<NativeInvoker>,
    pub read_registers: Option<RegisterReader>,
    pub write_registers: Option<RegisterWriter>,
    pub compare_exchange_u32: Option<MemoryCompareExchange>,
    pub controller_capture: Option<ControllerCaptureControl>,
    pub private_memory: Option<RuntimePrivateMemoryQuery>,
    pub mapping_capture: Option<MappingCapture>,
    pub read_memory: Option<MemoryReader>,
    pub write_memory: Option<MemoryWriter>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C)]
pub struct Identity {
    pub physical_pid: i32,
    pub physical_tid: i32,
    pub tool_pid: i32,
    pub tool_tid: i32,
    pub image: u64,
}

impl Identity {
    /// Only the actual native callback owner may supply these sampled facts.
    pub(super) unsafe fn admitted(self) -> io::Result<NativeThreadIdentity> {
        unsafe {
            NativeThreadIdentity::from_admitted(
                Pid::from_raw(self.physical_pid),
                Pid::from_raw(self.physical_tid),
                Pid::from_raw(self.tool_pid),
                Pid::from_raw(self.tool_tid),
                self.image,
            )
        }
    }
}

/// One synchronous callback. All pointed-to storage belongs to its original
/// native owner and must remain live until the engine entry returns. Deferred
/// *scratch* has the separate engine-owned completion lifetime below.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct Callback {
    pub version: u32,
    pub size: u32,
    pub identity: Identity,
    pub context: usize,
    pub mechanisms: *const Mechanisms,
    pub parent: i32,
    pub cpuid_interception: u32,
    pub branches: u64,
    pub auxv: *const [libc::c_ulong; 2],
    pub auxv_count: usize,
    pub initial_stack: u64,
    pub signal_info: *const [u8; reverie::SIGNAL_INFO_SIZE],
}

impl Callback {
    pub(super) fn validate(&self, expected: Identity) -> io::Result<&Mechanisms> {
        if self.version != VERSION
            || self.size as usize != size_of::<Self>()
            || self.identity != expected
            || self.mechanisms.is_null()
            || self.parent < 0
            || self.cpuid_interception > 1
            || self.auxv_count > AUXV_LIMIT
            || (self.auxv_count != 0 && self.auxv.is_null())
            || (self.auxv_count == 0 && !self.auxv.is_null())
        {
            return Err(io::Error::from_raw_os_error(libc::ESTALE));
        }
        // SAFETY: the native entry contract keeps the fixed table live. Its
        // version/size are checked before any trailing field is selected.
        let table = unsafe { &*self.mechanisms };
        if table.version != VERSION
            || table.size as usize != size_of::<Mechanisms>()
            || table.invoke.is_none()
            || table.read_registers.is_none()
            || table.write_registers.is_none()
        {
            return Err(io::Error::from_raw_os_error(libc::EPROTO));
        }
        let current = table
            .current
            .ok_or_else(|| io::Error::from_raw_os_error(libc::EPROTO))?;
        if unsafe { current(self.context) } != 1 {
            return Err(io::Error::from_raw_os_error(libc::ESTALE));
        }
        Ok(table)
    }

    /// Called only after the real native owner has checked context, image,
    /// physical task, Rust TLS/allocator and stack entry. Numeric facts alone
    /// cannot establish that unsafe contract.
    pub(super) unsafe fn select<'callback>(
        &'callback self,
        table: &'callback NativeServiceTable,
        auxv: &'callback [(libc::c_ulong, libc::c_ulong)],
    ) -> io::Result<NativeServices<'callback>> {
        let raw = self.validate(self.identity)?;
        let identity = unsafe { self.identity.admitted()? };
        let invocation = unsafe {
            NativeInvocation::from_admitted(
                self.context,
                raw.invoke.expect("validated invoker"),
                raw.current.expect("validated current owner"),
                identity,
            )
        };
        // A provided C vector must match its exact count. An absent C vector
        // is resolved by copy_auxv through the maintained actual-stack/kernel
        // owner; it does not mean that this guest has an empty vector.
        if self.auxv_count != 0 && auxv.len() != self.auxv_count {
            return Err(io::Error::from_raw_os_error(libc::EPROTO));
        }
        let signal_info = if self.signal_info.is_null() {
            None
        } else {
            Some(unsafe { self.signal_info.read() })
        };
        unsafe {
            NativeServices::from_admitted(
                invocation,
                table,
                NativeEventFacts {
                    parent: (self.parent != 0).then(|| Pid::from_raw(self.parent)),
                    branches: self.branches,
                    cpuid_interception: self.cpuid_interception == 1,
                    pidfd_self_spelling: reverie::PidfdSelfSpelling::Unknown,
                    auxv,
                    initial_stack: (self.initial_stack != 0).then_some(self.initial_stack),
                    signal_info,
                },
            )
        }
    }

    pub(super) unsafe fn copy_auxv(&self) -> io::Result<Vec<(libc::c_ulong, libc::c_ulong)>> {
        self.validate(self.identity)?;
        if self.auxv_count == 0 {
            let reader = self
                .validate(self.identity)?
                .read_memory
                .ok_or_else(|| io::Error::from_raw_os_error(libc::EPROTO))?;
            return super::auxv::read(reader);
        }
        // Copy scalar C pairs rather than asserting the layout of Rust tuples.
        Ok(
            unsafe { std::slice::from_raw_parts(self.auxv, self.auxv_count) }
                .iter()
                .map(|pair| (pair[0], pair[1]))
                .collect(),
        )
    }
}

impl Mechanisms {
    pub(super) fn selected(&self) -> io::Result<NativeServiceTable> {
        if self.version != VERSION || self.size as usize != size_of::<Self>() {
            return Err(io::Error::from_raw_os_error(libc::EPROTO));
        }
        Ok(NativeServiceTable {
            read_registers: self
                .read_registers
                .ok_or_else(|| io::Error::from_raw_os_error(libc::EPROTO))?,
            write_registers: self
                .write_registers
                .ok_or_else(|| io::Error::from_raw_os_error(libc::EPROTO))?,
            compare_exchange_u32: self.compare_exchange_u32,
            controller_capture: self.controller_capture,
            private_memory: self.private_memory,
            mapping_capture: self.mapping_capture,
            read_memory: self.read_memory,
        })
    }
}

/// Returned values only. A Deferred answer leaves its original affine Rust
/// lease inside the engine until actual native post-event/exit consumption.
#[repr(C)]
pub struct Answer {
    pub action: i32,
    pub reserved: u32,
    pub value: i64,
    pub number: i64,
    pub arguments: [u64; 6],
    pub registers: [u32; 4],
    pub signal: i32,
    pub signal_info_changed: u32,
    pub signal_info: [u8; reverie::SIGNAL_INFO_SIZE],
}

impl Default for Answer {
    fn default() -> Self {
        Self {
            action: 0,
            reserved: 0,
            value: 0,
            number: 0,
            arguments: [0; 6],
            registers: [0; 4],
            signal: 0,
            signal_info_changed: 0,
            signal_info: [0; 128],
        }
    }
}

const _: () = {
    assert!(size_of::<Identity>() == 24);
    assert!(size_of::<Mechanisms>() == 88);
    assert!(size_of::<Callback>() == 96);
    assert!(size_of::<Answer>() == 224);
};
