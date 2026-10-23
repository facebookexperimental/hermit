/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! A per-thread count of the interrupts the probe thread takes, from the core
//! PMU (<https://github.com/rrnewton/hermit/issues/4243>), condition C11).
//!
//! The XINUSE probe's reference trial is valid only if nothing saved and
//! reloaded the thread's FPU state inside it. Without a context switch, Linux
//! does that only through `kernel_fpu_begin` in interrupt or softirq context, or
//! in the task's own syscall, so a trial that took no interrupt, no context
//! switch and no syscall cannot contain a reload. This counter supplies the
//! interrupt half of that certificate, and every property it rests on is
//! checked rather than assumed:
//!
//! - the raw event is used only on CPU models whose perf tables document it;
//! - it is opened pinned, so it is never time-shared, and every read checks
//!   that its time enabled equals its time running (both are cumulative, so
//!   equality at a read proves it counted for its whole enabled history);
//! - a failed read is an error, never a value that could compare equal;
//! - at startup its count over a spin must bind to the kernel's own count of
//!   this CPU's interrupts in `/proc/interrupts`.

use std::fs::File;
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;

/// The CPU's vendor and display family/model, from CPUID leaves 0 and 1.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CpuModel {
    /// CPUID leaf 0 EBX: `Auth` (AMD) or `Genu` (Intel).
    pub vendor_ebx: u32,
    pub family: u32,
    pub model: u32,
}

const AUTHENTIC_AMD_EBX: u32 = 0x6874_7541;
const GENUINE_INTEL_EBX: u32 = 0x756e_6547;

impl CpuModel {
    /// The display family and model from leaf 1 EAX, as the SDM and the AMD APM
    /// define them: the extended family is added only for family 0xF, and the
    /// extended model is used only for families 0x6 and 0xF and above.
    pub fn from_leaves(vendor_ebx: u32, leaf1_eax: u32) -> Self {
        let mut family = (leaf1_eax >> 8) & 0xf;
        let mut model = (leaf1_eax >> 4) & 0xf;
        if family == 0xf {
            family += (leaf1_eax >> 20) & 0xff;
        }
        if family == 0x6 || family >= 0xf {
            model |= ((leaf1_eax >> 16) & 0xf) << 4;
        }
        CpuModel {
            vendor_ebx,
            family,
            model,
        }
    }

    /// The host's model, read with the CPUID instruction.
    pub fn host() -> Self {
        let leaf0 = std::arch::x86_64::__cpuid(0);
        let leaf1 = std::arch::x86_64::__cpuid(1);
        Self::from_leaves(leaf0.ebx, leaf1.eax)
    }
}

/// The raw PMU configuration of the interrupts-taken event on `cpu`, or `None`
/// where perf's event tables do not document one.
///
/// - AMD `LsIntTaken` (event 0x2C): Zen 2 and later, that is family 17h from
///   model 0x30, and families 19h and 1Ah. Zen 1 does not list it.
/// - Intel `HW_INTERRUPTS.RECEIVED` (event 0xCB, umask 0x01): Skylake client
///   and the client parts on the same core (Kaby, Coffee and Comet Lake),
///   Skylake-X, Cascade and Cooper Lake, Sapphire, Emerald and Granite Rapids,
///   Goldmont, Goldmont Plus, Tremont (Elkhart and Jasper Lake) and Snow Ridge.
///   Haswell, Broadwell, Ice Lake (including Ice Lake-X), Tiger, Alder, Raptor,
///   Meteor, Lunar and Arrow Lake and Sierra Forest do not list it, so they are
///   refused rather than given an event that might count something else.
pub fn interrupts_taken_event(cpu: CpuModel) -> Option<u64> {
    const INTEL_MODELS: &[u32] = &[
        0x4e, 0x5e, 0x8e, 0x9e, 0xa5, 0xa6, // Skylake client and same-core successors
        0x55, // Skylake-X, Cascade Lake, Cooper Lake
        0x8f, // Sapphire Rapids
        0xcf, // Emerald Rapids
        0xad, 0xae, // Granite Rapids
        0x5c, 0x5f, // Goldmont
        0x7a, // Goldmont Plus
        0x96, 0x9c, // Tremont: Elkhart Lake, Jasper Lake
        0x86, // Snow Ridge
    ];
    match cpu.vendor_ebx {
        AUTHENTIC_AMD_EBX
            if (cpu.family == 0x17 && cpu.model >= 0x30)
                || cpu.family == 0x19
                || cpu.family == 0x1a =>
        {
            Some(0x2c)
        }
        GENUINE_INTEL_EBX if cpu.family == 6 && INTEL_MODELS.contains(&cpu.model) => Some(0x01cb),
        _ => None,
    }
}

/// Why the counter cannot certify trials on this host.
#[derive(Debug, PartialEq, Eq)]
pub enum CounterRefusal {
    /// No documented interrupts-taken event for this CPU model.
    UndocumentedModel(CpuModel),
    /// `perf_event_open` failed; carries the error and `perf_event_paranoid`.
    Open { error: String, paranoid: String },
    /// A read returned fewer bytes than its three values: the pinned event
    /// could not be scheduled and is in the error state, or the descriptor is
    /// gone.
    Read,
    /// The event was not on the PMU for part of its enabled time.
    Multiplexed { enabled: u64, running: u64 },
    /// The startup spin did not bind the counter to the kernel's count.
    Calibration(String),
}

impl std::fmt::Display for CounterRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CounterRefusal::UndocumentedModel(cpu) => write!(
                f,
                "no documented interrupts-taken PMU event for CPU family {:#x} model {:#x}",
                cpu.family, cpu.model
            ),
            CounterRefusal::Open { error, paranoid } => write!(
                f,
                "perf_event_open of the interrupts-taken event failed: {error} \
                 (perf_event_paranoid is {paranoid}; counting kernel-mode interrupts \
                 needs 1 or less, or CAP_PERFMON)"
            ),
            CounterRefusal::Read => {
                write!(f, "reading the interrupts-taken counter failed")
            }
            CounterRefusal::Multiplexed { enabled, running } => write!(
                f,
                "the interrupts-taken counter was not on the PMU for its whole \
                 enabled time (enabled {enabled} ns, running {running} ns)"
            ),
            CounterRefusal::Calibration(why) => {
                write!(f, "the interrupts-taken counter failed calibration: {why}")
            }
        }
    }
}

/// An open, pinned interrupts-taken counter for the calling thread.
pub struct IrqCounter {
    fd: OwnedFd,
}

// `perf_event_attr` field offsets used below (include/uapi/linux/perf_event.h).
const PERF_TYPE_RAW: u32 = 4;
const PERF_FORMAT_TOTAL_TIME_ENABLED: u64 = 1 << 0;
const PERF_FORMAT_TOTAL_TIME_RUNNING: u64 = 1 << 1;
/// The flag word's bitfields in `perf_event_attr` order: `disabled` (bit 0),
/// `inherit` (bit 1), `pinned` (bit 2, "must always be on PMU").
const ATTR_FLAG_DISABLED: u64 = 1 << 0;
const ATTR_FLAG_INHERIT: u64 = 1 << 1;
const ATTR_FLAG_PINNED: u64 = 1 << 2;
/// The counter's flag word: enabled, not inherited by threads this thread
/// starts (the peer), pinned, and with no mode excluded.
const COUNTER_FLAGS: u64 = ATTR_FLAG_PINNED & !(ATTR_FLAG_DISABLED | ATTR_FLAG_INHERIT);
const PERF_ATTR_SIZE_VER0: u32 = 64;
const PERF_FLAG_FD_CLOEXEC: libc::c_ulong = 1 << 3;

#[repr(C)]
#[derive(Default)]
struct PerfEventAttrVer0 {
    type_: u32,
    size: u32,
    config: u64,
    sample_period: u64,
    sample_type: u64,
    read_format: u64,
    flags: u64,
    wakeup_events: u32,
    bp_type: u32,
    config1: u64,
}

impl IrqCounter {
    /// Opens the counter for the calling thread, counting user and kernel mode,
    /// pinned, with both times in each read.
    pub fn open(cpu: CpuModel) -> Result<Self, CounterRefusal> {
        let config = interrupts_taken_event(cpu).ok_or(CounterRefusal::UndocumentedModel(cpu))?;
        let attr = PerfEventAttrVer0 {
            type_: PERF_TYPE_RAW,
            size: PERF_ATTR_SIZE_VER0,
            config,
            read_format: PERF_FORMAT_TOTAL_TIME_ENABLED | PERF_FORMAT_TOTAL_TIME_RUNNING,
            flags: COUNTER_FLAGS,
            ..Default::default()
        };
        // SAFETY: `attr` is a valid PERF_ATTR_SIZE_VER0 structure that outlives
        // the call; pid 0 and cpu -1 select the calling thread on any CPU.
        let fd = unsafe {
            libc::syscall(
                libc::SYS_perf_event_open,
                &attr as *const PerfEventAttrVer0,
                0,
                -1,
                -1,
                PERF_FLAG_FD_CLOEXEC,
            )
        };
        if fd < 0 {
            let error = std::io::Error::last_os_error().to_string();
            let paranoid = std::fs::read_to_string("/proc/sys/kernel/perf_event_paranoid")
                .map(|s| s.trim().to_owned())
                .unwrap_or_else(|e| format!("unreadable ({e})"));
            return Err(CounterRefusal::Open { error, paranoid });
        }
        // SAFETY: the kernel just returned this descriptor and nothing else owns it.
        let fd = unsafe { OwnedFd::from_raw_fd(fd as i32) };
        Ok(IrqCounter { fd })
    }

    /// The interrupts taken so far, after checking that the read returned all
    /// three values and that the event was on the PMU for all of its enabled
    /// time.
    pub fn read(&self) -> Result<u64, CounterRefusal> {
        let mut values = [0u64; 3];
        // SAFETY: `values` is 24 writable bytes that outlive the call.
        let n = unsafe {
            libc::read(
                self.fd.as_raw_fd(),
                values.as_mut_ptr().cast(),
                std::mem::size_of_val(&values),
            )
        };
        check_read(n, values)
    }
}

/// The checks behind [`IrqCounter::read`], separated so tests can drive them.
fn check_read(n: isize, [value, enabled, running]: [u64; 3]) -> Result<u64, CounterRefusal> {
    if n != 24 {
        return Err(CounterRefusal::Read);
    }
    if enabled != running {
        return Err(CounterRefusal::Multiplexed { enabled, running });
    }
    Ok(value)
}

/// One CPU's local-timer count and its column total in `/proc/interrupts`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CpuInterrupts {
    pub local_timer: u64,
    pub total: u64,
}

/// Parses `/proc/interrupts` text for `cpu`'s column. Rows whose entry in that
/// column is not a number (the trailing descriptions of `ERR` and `MIS`) are
/// skipped; the `LOC` row must be present.
pub fn parse_proc_interrupts(text: &str, cpu: usize) -> Option<CpuInterrupts> {
    let mut lines = text.lines();
    let want = format!("CPU{cpu}");
    let column = lines.next()?.split_whitespace().position(|t| t == want)?;
    let mut local_timer = None;
    let mut total = 0u64;
    for line in lines {
        let mut fields = line.split_whitespace();
        let Some(label) = fields.next() else { continue };
        if let Some(Ok(count)) = fields.nth(column).map(str::parse::<u64>) {
            total = total.checked_add(count)?;
            if label == "LOC:" {
                local_timer = Some(count);
            }
        }
    }
    Some(CpuInterrupts {
        local_timer: local_timer?,
        total,
    })
}

/// Reads `/proc/interrupts` whole. It is about 1.4 MB on a 316-CPU machine;
/// the caller parses it after taking its other samples, so parsing time does
/// not widen the interval the counter must cover.
pub fn snapshot_proc_interrupts() -> Result<String, CounterRefusal> {
    let mut text = String::with_capacity(4 << 20);
    File::open("/proc/interrupts")
        .and_then(|mut f| f.read_to_string(&mut text))
        .map_err(|e| CounterRefusal::Calibration(format!("cannot read /proc/interrupts: {e}")))?;
    Ok(text)
}

/// `cpu`'s interrupt counts from a [`snapshot_proc_interrupts`] text.
pub fn interrupts_of(text: &str, cpu: usize) -> Result<CpuInterrupts, CounterRefusal> {
    parse_proc_interrupts(text, cpu).ok_or_else(|| {
        CounterRefusal::Calibration(format!("/proc/interrupts has no LOC count for CPU{cpu}"))
    })
}

/// The fewest local-timer interrupts a calibration must see, summed over its
/// clean spins. [`CALIBRATION_SLACK`] is absolute, so this floor is what keeps
/// the binding tight: at 190 interrupts the counter must see at least 180 of
/// them, about 95%, when the reads around the spins take no time. (A single
/// 70 ms spin sees about 70, where the slack alone would admit about 86%.) A
/// tickless (`nohz_full`) CPU sees almost none, and there the binding says
/// nothing.
pub const MIN_CALIBRATION_TICKS: u64 = 190;
/// A fixed allowance on top of the proportional one in [`calibration_verdict`].
pub const CALIBRATION_SLACK: u64 = 10;

/// Whether a calibration binds the counter to the kernel's count: over its
/// clean spins, summed, the counter must see at least this CPU's local-timer
/// interrupts and no more than all of its interrupts.
///
/// The `/proc/interrupts` reads around the spin lie outside the counter's
/// interval, and reading the file takes milliseconds on a large machine, so
/// timer interrupts during them are in the kernel's count only. `outside` is the
/// share of the measured interval that the counter did not cover (from the TSC),
/// and the allowance is that share of the kernel's count, plus
/// [`CALIBRATION_SLACK`].
pub fn calibration_verdict(
    counter: u64,
    kernel: CpuInterrupts,
    outside: f64,
) -> Result<(), String> {
    let allowance =
        (kernel.total as f64 * outside.clamp(0.0, 1.0)).ceil() as u64 + CALIBRATION_SLACK;
    if kernel.local_timer < MIN_CALIBRATION_TICKS {
        return Err(format!(
            "only {} local-timer interrupts during the calibration (need \
             {MIN_CALIBRATION_TICKS}); a tickless CPU cannot bind the counter",
            kernel.local_timer
        ));
    }
    if counter + allowance < kernel.local_timer {
        return Err(format!(
            "the counter saw {counter} interrupts but the CPU took {} local-timer interrupts \
             (allowing {allowance} for the reads around the spin)",
            kernel.local_timer
        ));
    }
    if counter > kernel.total + CALIBRATION_SLACK {
        return Err(format!(
            "the counter saw {counter} interrupts but the CPU took only {} in all",
            kernel.total
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn amd(family: u32, model: u32) -> CpuModel {
        CpuModel {
            vendor_ebx: AUTHENTIC_AMD_EBX,
            family,
            model,
        }
    }

    fn intel(model: u32) -> CpuModel {
        CpuModel {
            vendor_ebx: GENUINE_INTEL_EBX,
            family: 6,
            model,
        }
    }

    #[test]
    fn display_family_and_model_follow_the_extended_fields() {
        // EPYC 9D85 (Zen 5): family 0xF + 0x0B = 0x1A, model 0x11.
        assert_eq!(
            CpuModel::from_leaves(AUTHENTIC_AMD_EBX, 0x00b1_0f11),
            amd(0x1a, 0x11)
        );
        // Sapphire Rapids: family 6, extended model 8, model 0xF.
        assert_eq!(
            CpuModel::from_leaves(GENUINE_INTEL_EBX, 0x0008_06f8),
            intel(0x8f)
        );
    }

    #[test]
    fn the_event_is_chosen_only_where_perf_documents_it() {
        assert_eq!(interrupts_taken_event(amd(0x1a, 0x11)), Some(0x2c));
        assert_eq!(interrupts_taken_event(amd(0x19, 0x01)), Some(0x2c));
        assert_eq!(interrupts_taken_event(amd(0x17, 0x31)), Some(0x2c));
        // Zen 1 does not list LsIntTaken.
        assert_eq!(interrupts_taken_event(amd(0x17, 0x01)), None);
        assert_eq!(interrupts_taken_event(intel(0x55)), Some(0x01cb));
        assert_eq!(interrupts_taken_event(intel(0x8f)), Some(0x01cb));
        // Ice Lake-X has XCR0 0x2e7 but no documented event 0xCB.
        assert_eq!(interrupts_taken_event(intel(0x6a)), None);
        // Haswell and Alder Lake do not list it either.
        assert_eq!(interrupts_taken_event(intel(0x3f)), None);
        assert_eq!(interrupts_taken_event(intel(0x97)), None);
        assert_eq!(
            interrupts_taken_event(CpuModel {
                vendor_ebx: 0,
                family: 6,
                model: 0x55
            }),
            None
        );
    }

    #[test]
    fn a_read_must_return_all_three_values_with_equal_times() {
        assert_eq!(check_read(24, [7, 100, 100]), Ok(7));
        // A failed read returns no value that could compare equal to another.
        assert_eq!(check_read(-1, [0, 0, 0]), Err(CounterRefusal::Read));
        assert_eq!(check_read(0, [0, 0, 0]), Err(CounterRefusal::Read));
        assert_eq!(
            check_read(24, [7, 100, 90]),
            Err(CounterRefusal::Multiplexed {
                enabled: 100,
                running: 90
            })
        );
    }

    const INTERRUPTS: &str = "            CPU0       CPU1       CPU2\n\
           0:         44          0          0  IR-IO-APIC    2-edge      timer\n\
         LOC:  982944332  217340956  303290468   Local timer interrupts\n\
         RES:         10         20         30   Rescheduling interrupts\n\
         ERR:          0\n\
         MIS:          0\n";

    #[test]
    fn proc_interrupts_is_read_by_the_cpus_column() {
        assert_eq!(
            parse_proc_interrupts(INTERRUPTS, 1),
            Some(CpuInterrupts {
                local_timer: 217_340_956,
                total: 217_340_976
            })
        );
        assert_eq!(parse_proc_interrupts(INTERRUPTS, 3), None);
        assert_eq!(
            parse_proc_interrupts("            CPU0\n  0:  1  timer\n", 0),
            None,
            "no LOC row"
        );
    }

    #[test]
    fn calibration_binds_the_counter_to_the_kernels_count() {
        let kernel = CpuInterrupts {
            local_timer: 196,
            total: 197,
        };
        // Measured on CPU 3 of an EPYC 9D85: counter 194, LOC 196, total 197.
        assert_eq!(calibration_verdict(194, kernel, 0.0), Ok(()));
        assert!(
            calibration_verdict(100, kernel, 0.0).is_err(),
            "misses ticks"
        );
        assert!(
            calibration_verdict(400, kernel, 0.0).is_err(),
            "counts something else"
        );
        // Measured: counter 198, LOC 224, with the reads outside the counter's
        // interval taking a tenth of the measured time.
        let busy = CpuInterrupts {
            local_timer: 224,
            total: 230,
        };
        assert_eq!(calibration_verdict(198, busy, 0.1), Ok(()));
        assert!(calibration_verdict(198, busy, 0.0).is_err());
        assert!(
            calibration_verdict(150, busy, 0.1).is_err(),
            "still misses ticks"
        );
        assert!(
            calibration_verdict(
                3,
                CpuInterrupts {
                    local_timer: 3,
                    total: 3
                },
                0.0
            )
            .is_err(),
            "tickless CPU"
        );
    }

    /// The counter is pinned and counts only the calling thread: bit 2 of the
    /// flag word is `pinned` and bit 1 is `inherit` in `perf_event_attr`'s
    /// bitfield order (`disabled`, `inherit`, `pinned`, `exclusive`, ...).
    #[test]
    fn the_counter_is_pinned_enabled_and_not_inherited() {
        assert_eq!(
            (ATTR_FLAG_DISABLED, ATTR_FLAG_INHERIT, ATTR_FLAG_PINNED),
            (1, 2, 4)
        );
        assert_eq!(COUNTER_FLAGS & ATTR_FLAG_PINNED, ATTR_FLAG_PINNED);
        assert_eq!(COUNTER_FLAGS & (ATTR_FLAG_INHERIT | ATTR_FLAG_DISABLED), 0);
        // Bits 4-9 clear: no exclude_* bit (4-7), so user, kernel, hypervisor
        // and idle all count, and no mmap or comm records (8-9).
        assert_eq!(COUNTER_FLAGS & (0x3f << 4), 0);
    }

    /// The tightest the binding is, pinned: at the 190-interrupt floor with no
    /// time outside the counter, 180 passes and 179 does not. And one short
    /// spin cannot pass on its own, however its counts line up: a 70 ms spin
    /// measured counter 72, LOC 73, total 74 with 4.7% outside, where the slack
    /// alone would admit a counter of 59 (81%).
    #[test]
    fn calibration_needs_about_95_percent_of_the_timer_interrupts() {
        let floor = CpuInterrupts {
            local_timer: MIN_CALIBRATION_TICKS,
            total: MIN_CALIBRATION_TICKS,
        };
        assert_eq!(calibration_verdict(180, floor, 0.0), Ok(()));
        assert!(calibration_verdict(179, floor, 0.0).is_err());
        let one_short_spin = CpuInterrupts {
            local_timer: 73,
            total: 74,
        };
        assert!(calibration_verdict(72, one_short_spin, 0.047).is_err());
        assert!(calibration_verdict(59, one_short_spin, 0.047).is_err());
    }
}
