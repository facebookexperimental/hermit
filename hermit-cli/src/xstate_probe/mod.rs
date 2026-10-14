/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The startup qualification of `--cpu-profile x86-64-v3`
//! (<https://github.com/rrnewton/hermit/issues/4243>).
//!
//! Under v3 a guest may execute XSAVE. The XSTATE_BV bits a plain XSAVE writes
//! come from the processor's in-use tracking (XINUSE), which Hermit cannot trap
//! on the ptrace backend. The host kernel saves and restores a task's registers
//! at moments Hermit does not control. If such a save and restore changed a bit
//! the guest can read, a guest's output would depend on host timing.
//!
//! This probe checks, for a fixed set of cases, that it does not. Each case
//! dirties every register of every enabled component, brings some components to
//! their initial state by one path, and then runs plain XSAVE three ways:
//!
//! - **reference:** directly, with a certificate that nothing reloaded the
//!   registers in between (no context switch or fault, by `getrusage`; no
//!   interrupt, by the per-thread PMU counter in [`irq_counter`]);
//! - **reload:** after a handoff to a peer thread on the same CPU, so the kernel
//!   saves this thread's state, runs the peer's, and restores ours, with exactly
//!   two new timeslices on the CPU (the peer's, then ours);
//! - **forced exit** (under a hypervisor): after CPUID, which exits to it.
//!
//! The arms must write identical save images (all of it, not only the header)
//! and the same PKRU, and every repetition must agree. Anything inconclusive
//! refuses. The probe samples a fixed set of paths; it cannot exclude a CPU or
//! hypervisor that behaves differently only on others, and that limit is stated
//! wherever v3 is offered.

pub mod asm;
pub mod irq_counter;

use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use asm::ArmCtx;
use asm::ArmStatus;
use asm::Kind;
use asm::Mode;
use asm::PeerCtx;
use asm::PeerState;
use irq_counter::CpuModel;
use irq_counter::IrqCounter;

/// The fewest repetitions per cell (the design's retained minimum).
pub const MIN_REPETITIONS: u32 = 32;
/// The most repetitions accepted, checked before any narrowing.
pub const MAX_REPETITIONS: u32 = 4096;
/// Attempts per trial before its cell is inconclusive. A reload trial is
/// retried whenever another task ran on the CPU between the peer and the probe,
/// which on a busy machine (load 80 on 316 CPUs) happens often enough that 20
/// attempts left a cell inconclusive in one of six runs.
const MAX_ATTEMPTS: u32 = 200;
/// The XCR0 components the probe covers: x87, SSE, AVX, opmask, ZMM_Hi256,
/// Hi16_ZMM and PKRU.
pub const COVERED_XCR0: u64 = 0b10_1110_0111;
const AVX512_COMPONENTS: u64 = 0b1110_0000;
const PKRU_COMPONENT: u32 = 9;
const SIGUSR1: i32 = 10;
/// The MXCSR values each case runs under: the default, and a legal
/// non-default value (the SSE header bit is forced by the kernel's XSAVES when
/// MXCSR is not the default).
pub const MXCSR_VALUES: [u32; 2] = [0x1f80, 0x3f80];

/// One probe case: its initializing step, the components it brings to their
/// initial state, and the components it leaves live (both as XCR0 bit masks).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Case {
    pub kind: Kind,
    pub component: u32,
    pub targets: u64,
    pub live: u64,
}

impl Case {
    pub fn name(&self) -> String {
        match self.kind {
            Kind::XrstorInit | Kind::XrstorHeaderSet | Kind::CompactedXrstorInit => {
                format!("{:?} of component {}", self.kind, self.component)
            }
            kind => format!("{kind:?}"),
        }
    }
}

/// Why the host cannot use the v3 profile, as one sentence for the refusal.
pub type ProbeRefusal = String;

/// The admitted component sets: x87, SSE and AVX are required by v3; the three
/// AVX-512 components come together or not at all; PKRU is optional. Any other
/// bit refuses, over all 64 bits.
pub fn check_xcr0(xcr0: u64) -> Result<(), ProbeRefusal> {
    if xcr0 & !COVERED_XCR0 != 0 {
        return Err(format!(
            "XCR0 {xcr0:#x} enables components the XINUSE probe does not cover \
             (it covers {COVERED_XCR0:#x})"
        ));
    }
    if xcr0 & 0b111 != 0b111 {
        return Err(format!(
            "XCR0 {xcr0:#x} does not enable x87, SSE and AVX, which x86-64-v3 needs"
        ));
    }
    let avx512 = xcr0 & AVX512_COMPONENTS;
    if avx512 != 0 && avx512 != AVX512_COMPONENTS {
        return Err(format!(
            "XCR0 {xcr0:#x} enables only some of the AVX-512 components"
        ));
    }
    Ok(())
}

/// The closed case list for an admitted XCR0 (condition C6): every case whose
/// instructions and components exist on it.
pub fn case_list(xcr0: u64) -> Result<Vec<Case>, ProbeRefusal> {
    check_xcr0(xcr0)?;
    let avx512 = xcr0 & AVX512_COMPONENTS != 0;
    let pkru = xcr0 & (1 << PKRU_COMPONENT) != 0;
    let others = |targets: u64| xcr0 & !targets;
    let mut cases = Vec::new();
    for component in 0..64 {
        if xcr0 & (1 << component) == 0 {
            continue;
        }
        let targets = 1u64 << component;
        for kind in [
            Kind::XrstorInit,
            Kind::XrstorHeaderSet,
            Kind::CompactedXrstorInit,
        ] {
            cases.push(Case {
                kind,
                component,
                targets,
                live: others(targets),
            });
        }
    }
    let mut push = |kind: Kind, targets: u64| {
        let targets = targets & xcr0;
        cases.push(Case {
            kind,
            component: 0,
            targets,
            live: others(targets),
        })
    };
    if pkru {
        push(Kind::WrpkruZero, 1 << PKRU_COMPONENT);
        push(Kind::Sigreturn, 1 << PKRU_COMPONENT);
    }
    // VZEROUPPER zeroes bits 128 and up of registers 0-15: AVX and ZMM_Hi256.
    push(Kind::Vzeroupper, 0b100_0100);
    // A legacy PXOR leaves the upper halves alone.
    push(Kind::LegacyPxor, 0b10);
    // A VEX-128 write zeroes everything above bit 127 of registers 0-15.
    push(Kind::Vex128Zero, 0b100_0110);
    if avx512 {
        push(Kind::Hi16Zero, 0b1000_0000);
        push(Kind::PxorVzeroupperHi16, 0b1100_0110);
        push(Kind::OpmaskZero, 0b10_0000);
    }
    push(Kind::X87FullInit, 0b1);
    push(Kind::X87DirtyFullInit, 0b1);
    // FXRSTOR loads x87 and XMM0-15, leaving the YMM upper halves alone.
    push(Kind::FxrstorInit, 0b11);
    push(Kind::VzeroallAll, 0b1100_0110);
    Ok(cases)
}

/// The number of cases [`case_list`] must produce for `xcr0`, derived
/// independently of it (gate 5).
pub fn expected_case_count(xcr0: u64) -> usize {
    let avx512 = xcr0 & AVX512_COMPONENTS != 0;
    let pkru = xcr0 & (1 << PKRU_COMPONENT) != 0;
    3 * xcr0.count_ones() as usize + 2 * pkru as usize + 3 + 3 * avx512 as usize + 4
}

/// Where each component lives in a standard-format save area (CPUID 0xD).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Layout {
    pub xcr0: u64,
    /// The save-area size for the enabled components (0xD.0 EBX).
    pub size: usize,
    /// (offset, size) of each component from 2 up, by component number.
    pub components: Vec<(u32, usize, usize)>,
}

impl Layout {
    pub fn host() -> Result<Self, ProbeRefusal> {
        // SAFETY: XGETBV with ECX 0 is available whenever OSXSAVE is set, which
        // `check_host_xsave` establishes before the probe runs.
        let xcr0 = unsafe { std::arch::x86_64::_xgetbv(0) };
        let d0 = std::arch::x86_64::__cpuid_count(0xd, 0);
        let mut components = Vec::new();
        for i in 2..64u32 {
            if xcr0 & (1 << i) != 0 {
                let d = std::arch::x86_64::__cpuid_count(0xd, i);
                components.push((i, d.ebx as usize, d.eax as usize));
            }
        }
        let layout = Layout {
            xcr0,
            size: d0.ebx as usize,
            components,
        };
        if layout.size == 0 || layout.size + 64 > USED_BYTES {
            return Err(format!(
                "the XSAVE area for XCR0 {xcr0:#x} is {} bytes, outside what the probe holds",
                layout.size
            ));
        }
        Ok(layout)
    }

    /// Whether `component` is in its initial configuration in `image`: its
    /// header bit is clear, or its saved values are the initial ones.
    pub fn is_initial(&self, image: &[u8], pkru: u32, component: u32) -> bool {
        let header = u64::from_le_bytes(image[512..520].try_into().unwrap());
        if header & (1 << component) == 0 {
            return true;
        }
        match component {
            0 => {
                let fcw = u16::from_le_bytes([image[0], image[1]]);
                fcw == 0x37f && image[2..24].iter().all(|b| *b == 0) && all_zero(&image[32..160])
            }
            1 => all_zero(&image[160..416]),
            PKRU_COMPONENT => pkru == 0,
            _ => self
                .components
                .iter()
                .find(|(i, _, _)| *i == component)
                .is_some_and(|(_, offset, size)| all_zero(&image[*offset..*offset + *size])),
        }
    }
}

fn all_zero(bytes: &[u8]) -> bool {
    bytes.iter().all(|b| *b == 0)
}

const AREA_BYTES: usize = 16384;

#[repr(C, align(64))]
struct Area([u8; AREA_BYTES]);

impl Area {
    fn boxed() -> Box<Area> {
        Box::new(Area([0; AREA_BYTES]))
    }
}

/// Zeroes the part of an area any enabled-component image can use: the host's
/// XSAVE size (at most [`USED_BYTES`]) plus RDPKRU's slot.
fn clear(area: &mut Area) {
    area.0[..USED_BYTES].fill(0);
}

/// The bytes of an area the probe uses; [`Layout::host`] refuses a save area
/// that does not fit.
const USED_BYTES: usize = 8192;

/// A standard-format image of initial values with `header` as XSTATE_BV.
fn fill_standard_image(area: &mut Area, mxcsr: u32, header: u64) {
    clear(area);
    area.0[0..2].copy_from_slice(&0x37fu16.to_le_bytes());
    area.0[24..28].copy_from_slice(&mxcsr.to_le_bytes());
    area.0[28..32].copy_from_slice(&0xffffu32.to_le_bytes()); // MXCSR_MASK
    area.0[512..520].copy_from_slice(&header.to_le_bytes());
}

/// A compacted-format image whose header clears every XSTATE_BV bit.
fn fill_compacted_image(area: &mut Area, mxcsr: u32, xcr0: u64) {
    clear(area);
    area.0[0..2].copy_from_slice(&0x37fu16.to_le_bytes());
    area.0[24..28].copy_from_slice(&mxcsr.to_le_bytes());
    area.0[520..528].copy_from_slice(&((1u64 << 63) | xcr0).to_le_bytes());
}

/// An FXSAVE image of initial x87 and SSE values.
fn fill_fx_image(area: &mut Area, mxcsr: u32) {
    clear(area);
    area.0[0..2].copy_from_slice(&0x37fu16.to_le_bytes());
    area.0[24..28].copy_from_slice(&mxcsr.to_le_bytes());
}

static PROBE_TID: AtomicU64 = AtomicU64::new(0);
static PROBE_SIGNALS: AtomicU64 = AtomicU64::new(0);
static PEER_SIGNALS: AtomicU64 = AtomicU64::new(0);

extern "C" fn count_signal(_signal: libc::c_int) {
    // SAFETY: gettid has no preconditions.
    let tid = unsafe { libc::gettid() } as u64;
    if tid == PROBE_TID.load(Ordering::SeqCst) {
        PROBE_SIGNALS.fetch_add(1, Ordering::SeqCst);
    } else {
        PEER_SIGNALS.fetch_add(1, Ordering::SeqCst);
    }
}

/// Context switches plus page faults of the calling thread.
fn switches_and_faults() -> Result<i64, ProbeRefusal> {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
    // SAFETY: `usage` is writable and RUSAGE_THREAD names the calling thread.
    if unsafe { libc::getrusage(libc::RUSAGE_THREAD, usage.as_mut_ptr()) } != 0 {
        return Err(format!(
            "getrusage failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    // SAFETY: getrusage succeeded and initialized it.
    let u = unsafe { usage.assume_init() };
    Ok(u.ru_nvcsw + u.ru_nivcsw + u.ru_minflt + u.ru_majflt)
}

/// The timeslices that have started on `cpu`, from `/proc/schedstat`.
///
/// The file is about 200 KB on a 316-CPU machine and costs a millisecond to
/// generate whole; reading stops at `cpu`'s line, so for the low-numbered CPU
/// the probe pins itself to, the kernel generates only the first few lines.
fn cpu_timeslices(cpu: usize) -> Result<u64, ProbeRefusal> {
    use std::io::BufRead;
    let file = std::fs::File::open("/proc/schedstat")
        .map_err(|e| format!("cannot read /proc/schedstat: {e}"))?;
    let want = format!("cpu{cpu} ");
    let mut text = String::new();
    for line in std::io::BufReader::with_capacity(4096, file).lines() {
        let line = line.map_err(|e| format!("cannot read /proc/schedstat: {e}"))?;
        let done = line.starts_with(&want);
        text.push_str(&line);
        text.push('\n');
        if done {
            break;
        }
    }
    parse_schedstat_timeslices(&text, cpu)
}

/// Parses a `/proc/schedstat` version 15-17 text for `cpu`'s timeslice count:
/// the ninth field of its `cpuN` line, which `sched_info_switch` advances for
/// each task that starts running there (the idle task excluded).
pub fn parse_schedstat_timeslices(text: &str, cpu: usize) -> Result<u64, ProbeRefusal> {
    let mut lines = text.lines();
    let version = lines
        .next()
        .and_then(|l| l.strip_prefix("version "))
        .and_then(|v| v.trim().parse::<u32>().ok())
        .ok_or("/proc/schedstat has no version line")?;
    if !(15..=17).contains(&version) {
        return Err(format!(
            "/proc/schedstat version {version} is not one whose cpu line the probe reads (15-17)"
        ));
    }
    let want = format!("cpu{cpu}");
    for line in lines {
        let mut fields = line.split_whitespace();
        if fields.next() == Some(want.as_str()) {
            return fields
                .nth(8)
                .and_then(|f| f.parse::<u64>().ok())
                .ok_or_else(|| format!("/proc/schedstat line for {want} has no timeslice count"));
        }
    }
    Err(format!("/proc/schedstat has no line for {want}"))
}

/// What the probe found, bound into a run's manifest and printed on request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProbeReport {
    pub cpu: usize,
    pub xcr0: u64,
    pub cases: usize,
    pub cells: usize,
    pub repetitions: u32,
    pub certified_pairs: u64,
    pub forced_exit: bool,
    pub reload_bound: u64,
    pub cpuid_min_cycles: u64,
    pub calibration: String,
    /// Trials repeated because a certificate failed.
    pub retried_references: u64,
    pub retried_reloads: u64,
    pub retried_exits: u64,
    /// Times the counter was reopened and recalibrated after another PMU user
    /// kept it off the PMU.
    pub counter_reopens: u32,
}

/// How the probe is run.
#[derive(Clone, Copy, Debug)]
pub struct ProbeOptions {
    pub repetitions: u32,
    /// `None`: on exactly when CPUID.1:ECX bit 31 (hypervisor) is set.
    pub forced_exit: Option<bool>,
}

impl Default for ProbeOptions {
    fn default() -> Self {
        ProbeOptions {
            repetitions: MIN_REPETITIONS,
            forced_exit: None,
        }
    }
}

/// Runs the qualification on a new thread pinned to the lowest-numbered CPU it may use.
/// Returns the report, or the reason v3 cannot be offered on this host.
pub fn qualify(options: ProbeOptions) -> Result<ProbeReport, ProbeRefusal> {
    if !(MIN_REPETITIONS..=MAX_REPETITIONS).contains(&options.repetitions) {
        return Err(format!(
            "the probe needs {MIN_REPETITIONS} to {MAX_REPETITIONS} repetitions, not {}",
            options.repetitions
        ));
    }
    // Probes pin themselves to the same CPU, so two at once would switch each
    // other out until both refused. One at a time per user: the others wait.
    let _serialized = ProbeLock::acquire()?;
    std::thread::Builder::new()
        .name("xinuse-probe".into())
        .spawn(move || Probe::run(options))
        .map_err(|e| format!("cannot start the probe thread: {e}"))?
        .join()
        .map_err(|_| "the probe thread panicked".to_owned())?
}

/// An exclusive `flock` on a per-user file in the temporary directory, held
/// for one probe.
struct ProbeLock {
    _file: std::fs::File,
}

impl ProbeLock {
    fn acquire() -> Result<Self, ProbeRefusal> {
        use std::os::fd::AsRawFd;
        // SAFETY: getuid has no preconditions.
        let uid = unsafe { libc::getuid() };
        let path = std::env::temp_dir().join(format!("hermit-xinuse-probe-{uid}.lock"));
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .map_err(|e| format!("cannot open the probe lock {}: {e}", path.display()))?;
        // SAFETY: the descriptor is open for the duration of the call.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(format!(
                "cannot lock {}: {}",
                path.display(),
                std::io::Error::last_os_error()
            ));
        }
        Ok(ProbeLock { _file: file })
    }
}

fn pin_to(cpu: usize) -> Result<(), ProbeRefusal> {
    // SAFETY: cpu_set_t is plain data; CPU_SET writes within it.
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        libc::CPU_SET(cpu, &mut set);
        if libc::sched_setaffinity(0, std::mem::size_of_val(&set), &set) != 0 {
            return Err(format!(
                "cannot pin the probe to CPU {cpu}: {}",
                std::io::Error::last_os_error()
            ));
        }
    }
    Ok(())
}

/// The lowest-numbered CPU this thread may run on. The probe pins itself
/// there, so that the schedstat line it reads each reload trial comes first.
fn first_allowed_cpu() -> Result<usize, ProbeRefusal> {
    // SAFETY: cpu_set_t is plain data that sched_getaffinity fills.
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        if libc::sched_getaffinity(0, std::mem::size_of_val(&set), &mut set) != 0 {
            return Err(format!(
                "cannot read the probe's CPU affinity: {}",
                std::io::Error::last_os_error()
            ));
        }
        (0..libc::CPU_SETSIZE as usize)
            .find(|cpu| libc::CPU_ISSET(*cpu, &set))
            .ok_or_else(|| "the probe may run on no CPU".to_owned())
    }
}

fn current_cpu() -> Result<usize, ProbeRefusal> {
    // SAFETY: sched_getcpu has no preconditions.
    let cpu = unsafe { libc::sched_getcpu() };
    usize::try_from(cpu).map_err(|_| "sched_getcpu failed".to_owned())
}

fn minimum(samples: usize, f: impl Fn() -> u64) -> u64 {
    (0..samples).map(|_| f()).min().unwrap_or(u64::MAX)
}

struct Probe {
    cpu: usize,
    layout: Layout,
    counter: IrqCounter,
    save: [Box<Area>; 3],
    first: Box<Area>,
    image: Box<Area>,
    peer_image: Box<Area>,
    ones: Box<Area>,
    to_peer: [i32; 2],
    from_peer: [i32; 2],
    peer: Box<PeerCtx>,
    tgid: i32,
    tid: i32,
    seq: u8,
    forced_exit: bool,
    reload_bound: u64,
    report: ProbeReport,
}

impl Probe {
    fn run(options: ProbeOptions) -> Result<ProbeReport, ProbeRefusal> {
        let cpu = first_allowed_cpu()?;
        pin_to(cpu)?;
        let layout = Layout::host()?;
        let cases = case_list(layout.xcr0)?;
        if cases.len() != expected_case_count(layout.xcr0) {
            return Err(format!(
                "the probe built {} cases for XCR0 {:#x}, expected {}",
                cases.len(),
                layout.xcr0,
                expected_case_count(layout.xcr0)
            ));
        }
        let (counter, calibration) = open_calibrated_counter(cpu, 0)?;
        let hypervisor = std::arch::x86_64::__cpuid(1).ecx & (1 << 31) != 0;
        let forced_exit = options.forced_exit.unwrap_or(hypervisor);

        let mut save = [Area::boxed(), Area::boxed(), Area::boxed()];
        // A kernel round trip plus a full save and restore: the least a CPUID
        // exit to a hypervisor and back can cost.
        let syscall_min = minimum(1001, || unsafe { asm::hermit_xinuse_syscall_cycles() });
        let area = save[0].0.as_mut_ptr();
        let save_restore_min = minimum(1001, || unsafe {
            asm::hermit_xinuse_save_restore_cycles(area, layout.xcr0)
        });
        let reload_bound = syscall_min + save_restore_min;
        let cpuid_min = minimum(1001, || unsafe { asm::hermit_xinuse_cpuid_cycles() });
        if forced_exit && cpuid_min < reload_bound {
            return Err(format!(
                "v3 is not qualified under this hypervisor: CPUID does not exit \
                 (fastest of 1001 took {cpuid_min} cycles, under the {reload_bound}-cycle \
                 cost of a kernel round trip plus a full save and restore)"
            ));
        }

        let mut ones = Area::boxed();
        for (i, b) in ones.0.iter_mut().enumerate().take(64) {
            *b = (i as u8) | 0x81;
        }
        let mut to_peer = [0i32; 2];
        let mut from_peer = [0i32; 2];
        // SAFETY: each array has room for the two descriptors pipe2 writes.
        if unsafe { libc::pipe2(to_peer.as_mut_ptr(), libc::O_CLOEXEC) } != 0
            || unsafe { libc::pipe2(from_peer.as_mut_ptr(), libc::O_CLOEXEC) } != 0
        {
            return Err(format!(
                "cannot create the handshake pipes: {}",
                std::io::Error::last_os_error()
            ));
        }
        // SAFETY: getpid and gettid have no preconditions.
        let (tgid, tid) = unsafe { (libc::getpid(), libc::gettid()) };
        PROBE_TID.store(tid as u64, Ordering::SeqCst);
        install_signal_counter()?;
        let peer_image = Area::boxed();
        let peer = Box::new(PeerCtx {
            state: PeerState::DirtyPkruDiffers as u32,
            fd_in: to_peer[0],
            fd_out: from_peer[1],
            avx512: (layout.xcr0 & AVX512_COMPONENTS != 0) as u32,
            image: peer_image.0.as_ptr(),
            xcr0: layout.xcr0,
            ones: ones.0.as_ptr(),
            tgid,
            tid: 0,
            expected_cpu: cpu as u32,
            wrong_cpu: 0,
            status: 0,
            rounds: 0,
            byte: 0,
            reply: 0,
            _pad: [0; 2],
            cpu_out: 0,
            saved_pkru: 0,
            saved_mxcsr: 0,
        });
        let mut probe = Probe {
            cpu,
            report: ProbeReport {
                cpu,
                xcr0: layout.xcr0,
                cases: cases.len(),
                cells: 0,
                repetitions: options.repetitions,
                certified_pairs: 0,
                forced_exit,
                reload_bound,
                cpuid_min_cycles: cpuid_min,
                calibration,
                retried_references: 0,
                retried_reloads: 0,
                retried_exits: 0,
                counter_reopens: 0,
            },
            layout,
            counter,
            save,
            first: Area::boxed(),
            image: Area::boxed(),
            peer_image,
            ones,
            to_peer,
            from_peer,
            peer,
            tgid,
            tid,
            seq: 0,
            forced_exit,
            reload_bound,
        };
        tracing::info!(
            "xinuse probe: calibrated on CPU {cpu} ({}); reload bound {reload_bound} cycles",
            probe.report.calibration
        );
        let started = std::time::Instant::now();
        let result = probe.with_peer(|probe| probe.matrix(&cases, options.repetitions));
        tracing::info!(
            "xinuse probe: matrix took {:?}: {} cells, retried {} reference, {} reload and {} exit trials",
            started.elapsed(),
            probe.report.cells,
            probe.report.retried_references,
            probe.report.retried_reloads,
            probe.report.retried_exits
        );
        for fd in to_peer.into_iter().chain(from_peer) {
            // SAFETY: these descriptors were created above and are not used again.
            unsafe { libc::close(fd) };
        }
        result.map(|()| probe.report)
    }

    /// Runs `f` with the peer thread serving handshakes, then stops it.
    fn with_peer(
        &mut self,
        f: impl FnOnce(&mut Self) -> Result<(), ProbeRefusal>,
    ) -> Result<(), ProbeRefusal> {
        let cpu = self.cpu;
        let peer = &mut *self.peer as *mut PeerCtx as usize;
        let tid_slot = std::sync::Arc::new(AtomicU64::new(0));
        let tid_out = tid_slot.clone();
        let handle = std::thread::Builder::new()
            .name("xinuse-peer".into())
            .spawn(move || -> Result<u32, ProbeRefusal> {
                pin_to(cpu)?;
                // SAFETY: gettid has no preconditions.
                let tid = unsafe { libc::gettid() };
                let peer = peer as *mut PeerCtx;
                // SAFETY: the probe thread keeps the context alive until this
                // thread is joined, and touches it only while this thread is
                // blocked in its handshake read.
                unsafe { (*peer).tid = tid };
                tid_out.store(tid as u64, Ordering::SeqCst);
                Ok(unsafe { asm::hermit_xinuse_peer_loop(peer) })
            })
            .map_err(|e| format!("cannot start the peer thread: {e}"))?;
        while tid_slot.load(Ordering::SeqCst) == 0 && !handle.is_finished() {
            std::thread::yield_now();
        }
        let result = f(self);
        let exit = [asm::PEER_EXIT];
        // SAFETY: writes one byte from a live buffer to our pipe.
        let wrote = unsafe { libc::write(self.to_peer[1], exit.as_ptr().cast(), 1) };
        let joined = handle
            .join()
            .map_err(|_| "the peer thread panicked".to_owned())?;
        let status = joined?;
        result?;
        if wrote != 1 {
            return Err("could not stop the peer thread".into());
        }
        match ArmStatus::from_raw(status) {
            Some(ArmStatus::Ok) => {}
            other => return Err(format!("the peer thread failed: {other:?}")),
        }
        if self.peer.wrong_cpu != 0 {
            return Err(format!(
                "the peer ran off CPU {cpu} in {} of {} rounds",
                self.peer.wrong_cpu, self.peer.rounds
            ));
        }
        Ok(())
    }

    fn matrix(&mut self, cases: &[Case], repetitions: u32) -> Result<(), ProbeRefusal> {
        let peer_states = [
            PeerState::DirtyPkruDiffers,
            PeerState::DirtyWrpkruZero,
            PeerState::XrstorInitAll,
            PeerState::DirtyPkruHeaderLoaded,
            PeerState::DirtyPkruSigreturn,
            PeerState::InitValuesAllHeaderBits,
        ];
        let pkru = self.layout.xcr0 & (1 << PKRU_COMPONENT) != 0;
        for case in cases {
            for peer in peer_states {
                if !pkru
                    && matches!(
                        peer,
                        PeerState::DirtyWrpkruZero
                            | PeerState::DirtyPkruHeaderLoaded
                            | PeerState::DirtyPkruSigreturn
                    )
                {
                    continue;
                }
                for mxcsr in MXCSR_VALUES {
                    self.cell(case, peer, mxcsr, repetitions)?;
                    self.report.cells += 1;
                }
            }
        }
        let expected = self.report.cells as u64 * u64::from(repetitions);
        if self.report.certified_pairs != expected {
            return Err(format!(
                "the probe certified {} pairs, expected {expected}",
                self.report.certified_pairs
            ));
        }
        Ok(())
    }

    fn cell(
        &mut self,
        case: &Case,
        peer: PeerState,
        mxcsr: u32,
        repetitions: u32,
    ) -> Result<(), ProbeRefusal> {
        let where_ = || format!("case {}, peer {peer:?}, MXCSR {mxcsr:#x}", case.name());
        let size = self.layout.size;
        for repetition in 0..repetitions {
            self.trial(case, Mode::Reference, peer, mxcsr, 0)
                .map_err(|e| format!("{e} ({})", where_()))?;
            self.check_reached(case, &where_)?;
            self.trial(case, Mode::Reload, peer, mxcsr, 1)
                .map_err(|e| format!("{e} ({})", where_()))?;
            self.compare(0, 1, "a host FPU reload", &where_)?;
            if self.forced_exit {
                self.trial(case, Mode::ForcedExit, peer, mxcsr, 2)
                    .map_err(|e| format!("{e} ({})", where_()))?;
                self.compare(0, 2, "a hypervisor exit", &where_)?;
            }
            if repetition == 0 {
                self.first.0[..size + 4].copy_from_slice(&self.save[0].0[..size + 4]);
            } else if self.first.0[..size + 4] != self.save[0].0[..size + 4] {
                return Err(format!(
                    "XINUSE is not stable across repetitions: the reference image changed \
                     at repetition {repetition} ({})",
                    where_()
                ));
            }
            self.report.certified_pairs += 1;
        }
        Ok(())
    }

    /// Runs one arm into save area `slot` until its certificate holds.
    fn trial(
        &mut self,
        case: &Case,
        mode: Mode,
        peer: PeerState,
        mxcsr: u32,
        slot: usize,
    ) -> Result<(), ProbeRefusal> {
        for _attempt in 0..MAX_ATTEMPTS {
            self.prepare_image(case, mxcsr);
            self.prepare_peer(peer);
            clear(&mut self.save[slot]);
            self.seq = (self.seq + 1) % asm::PEER_EXIT;
            let mut ctx = ArmCtx {
                kind: case.kind as u32,
                component: case.component,
                mxcsr,
                mode: mode as u32,
                xcr0: self.layout.xcr0,
                save_area: self.save[slot].0.as_mut_ptr(),
                image: self.image.0.as_ptr(),
                fd_to_peer: self.to_peer[1],
                fd_from_peer: self.from_peer[0],
                tgid: self.tgid,
                tid: self.tid,
                ones: self.ones.0.as_ptr(),
                pkru_out: 0,
                avx512: (self.layout.xcr0 & AVX512_COMPONENTS != 0) as u32,
                cpuid_cycles: 0,
                seq: self.seq,
                reply: 0,
                _pad: [0; 2],
                status: 0,
                saved_pkru: 0,
                saved_mxcsr: 0,
            };
            let probe_signals = PROBE_SIGNALS.load(Ordering::SeqCst);
            let peer_signals = PEER_SIGNALS.load(Ordering::SeqCst);
            let timeslices = match mode {
                Mode::Reload => cpu_timeslices(self.cpu)?,
                _ => 0,
            };
            let switches = switches_and_faults()?;
            let irqs = match self.counter.read() {
                Ok(irqs) => irqs,
                Err(e) => {
                    self.reopen_counter(e)?;
                    continue;
                }
            };
            // SAFETY: the context and every buffer it points at outlive the call.
            let status = unsafe { asm::hermit_xinuse_arm(&mut ctx) };
            let irqs_after = match self.counter.read() {
                Ok(irqs) => irqs,
                Err(e) => {
                    self.reopen_counter(e)?;
                    continue;
                }
            };
            let switches_after = switches_and_faults()?;
            let timeslices_after = match mode {
                Mode::Reload => cpu_timeslices(self.cpu)?,
                _ => 0,
            };
            if current_cpu()? != self.cpu {
                return Err(format!("the probe thread ran off CPU {}", self.cpu));
            }
            match ArmStatus::from_raw(status) {
                Some(ArmStatus::Ok) => {}
                other => return Err(format!("a probe arm failed: {other:?}")),
            }
            // A sigreturn trial must actually take its signal; a peer round
            // that sends one must too.
            if case.kind == Kind::Sigreturn
                && PROBE_SIGNALS.load(Ordering::SeqCst) != probe_signals + 1
            {
                return Err("the sigreturn case's signal was not delivered".into());
            }
            if mode == Mode::Reload
                && peer == PeerState::DirtyPkruSigreturn
                && PEER_SIGNALS.load(Ordering::SeqCst) != peer_signals + 1
            {
                return Err("the peer's sigreturn signal was not delivered".into());
            }
            self.save[slot].0[self.layout.size..self.layout.size + 4]
                .copy_from_slice(&ctx.pkru_out.to_le_bytes());
            let certified = match mode {
                Mode::Reference => switches_after == switches && irqs_after == irqs,
                // Exactly the peer's timeslice and then ours: no other task,
                // with its own PKRU, ran between them.
                Mode::Reload => timeslices_after - timeslices == 2,
                Mode::ForcedExit => {
                    switches_after == switches
                        && irqs_after == irqs
                        && ctx.cpuid_cycles >= self.reload_bound
                }
            };
            if certified {
                return Ok(());
            }
            match mode {
                Mode::Reference => self.report.retried_references += 1,
                Mode::Reload => self.report.retried_reloads += 1,
                Mode::ForcedExit => self.report.retried_exits += 1,
            }
        }
        Err(format!(
            "no {mode:?} trial was certified in {MAX_ATTEMPTS} attempts; the result is inconclusive"
        ))
    }

    /// Replaces a counter that failed a read (another PMU user kept it off the
    /// PMU, so its coverage is broken for good) with a newly opened and
    /// calibrated one. The trial in progress is discarded. Refuses after
    /// [`MAX_COUNTER_REOPENS`].
    fn reopen_counter(&mut self, why: irq_counter::CounterRefusal) -> Result<(), ProbeRefusal> {
        self.report.counter_reopens += 1;
        if self.report.counter_reopens > MAX_COUNTER_REOPENS {
            return Err(format!(
                "{why}, and it stayed unreliable after {MAX_COUNTER_REOPENS} reopenings"
            ));
        }
        let (counter, calibration) =
            open_calibrated_counter(self.cpu, self.report.counter_reopens)?;
        self.counter = counter;
        self.report.calibration = calibration;
        Ok(())
    }

    fn prepare_image(&mut self, case: &Case, mxcsr: u32) {
        match case.kind {
            Kind::XrstorInit => fill_standard_image(&mut self.image, mxcsr, 0),
            Kind::XrstorHeaderSet => {
                fill_standard_image(&mut self.image, mxcsr, 1 << case.component)
            }
            Kind::CompactedXrstorInit => {
                fill_compacted_image(&mut self.image, mxcsr, self.layout.xcr0)
            }
            Kind::FxrstorInit => fill_fx_image(&mut self.image, mxcsr),
            _ => {}
        }
    }

    fn prepare_peer(&mut self, peer: PeerState) {
        let header = match peer {
            PeerState::DirtyPkruHeaderLoaded => 1 << PKRU_COMPONENT,
            PeerState::InitValuesAllHeaderBits => self.layout.xcr0,
            _ => 0,
        };
        fill_standard_image(&mut self.peer_image, 0x1f80, header);
        // The peer is blocked in its handshake read, so it sees this before it
        // acts on the next byte.
        self.peer.state = peer as u32;
    }

    /// Condition C7: the reference reached the case's intended state.
    fn check_reached(&self, case: &Case, where_: &dyn Fn() -> String) -> Result<(), ProbeRefusal> {
        let image = &self.save[0].0[..self.layout.size];
        let pkru = u32::from_le_bytes(
            self.save[0].0[self.layout.size..self.layout.size + 4]
                .try_into()
                .unwrap(),
        );
        for component in 0..64 {
            let bit = 1u64 << component;
            if case.targets & bit != 0 && !self.layout.is_initial(image, pkru, component) {
                return Err(format!(
                    "the probe case did not bring component {component} to its initial state ({})",
                    where_()
                ));
            }
            if case.live & bit != 0 && self.layout.is_initial(image, pkru, component) {
                return Err(format!(
                    "the probe case left component {component} initial where it should be live ({})",
                    where_()
                ));
            }
        }
        Ok(())
    }

    fn compare(
        &self,
        reference: usize,
        other: usize,
        transition: &str,
        where_: &dyn Fn() -> String,
    ) -> Result<(), ProbeRefusal> {
        let n = self.layout.size + 4;
        if self.save[reference].0[..n] == self.save[other].0[..n] {
            return Ok(());
        }
        let header =
            |slot: usize| u64::from_le_bytes(self.save[slot].0[512..520].try_into().unwrap());
        let offset = (0..n)
            .find(|i| self.save[reference].0[*i] != self.save[other].0[*i])
            .unwrap_or(0);
        Err(format!(
            "XINUSE is not stable across {transition}: XSTATE_BV {:#x} against {:#x}, \
             first difference at save-area byte {offset} ({})",
            header(reference),
            header(other),
            where_()
        ))
    }
}

fn install_signal_counter() -> Result<(), ProbeRefusal> {
    // SAFETY: the handler only calls gettid and updates atomics.
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = count_signal as *const () as usize;
        libc::sigemptyset(&mut action.sa_mask);
        if libc::sigaction(SIGUSR1, &action, std::ptr::null_mut()) != 0 {
            return Err(format!(
                "cannot install the probe's SIGUSR1 handler: {}",
                std::io::Error::last_os_error()
            ));
        }
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, SIGUSR1);
        if libc::pthread_sigmask(libc::SIG_UNBLOCK, &set, std::ptr::null_mut()) != 0 {
            return Err("cannot unblock SIGUSR1 for the probe".into());
        }
    }
    Ok(())
}

/// How many times a counter that lost PMU coverage may be replaced.
const MAX_COUNTER_REOPENS: u32 = 5;

/// Opens the interrupt counter and calibrates it. A counter that fails a read
/// during calibration is replaced, up to [`MAX_COUNTER_REOPENS`] times in all.
fn open_calibrated_counter(cpu: usize, reopens: u32) -> Result<(IrqCounter, String), ProbeRefusal> {
    let mut reopens = reopens;
    loop {
        let counter = IrqCounter::open(CpuModel::host()).map_err(|e| e.to_string())?;
        match calibrate(&counter, cpu) {
            Ok(calibration) => return Ok((counter, calibration)),
            Err(CalibrationFailure::CounterLost(why)) if reopens < MAX_COUNTER_REOPENS => {
                reopens += 1;
                tracing::debug!("xinuse probe: reopening the interrupt counter: {why}");
            }
            Err(CalibrationFailure::CounterLost(why)) => {
                return Err(format!(
                    "{why}, and it stayed unreliable after {MAX_COUNTER_REOPENS} reopenings"
                ));
            }
            Err(CalibrationFailure::Refused(why)) => return Err(why),
        }
    }
}

enum CalibrationFailure {
    /// The counter lost PMU coverage; a new one may do better.
    CounterLost(irq_counter::CounterRefusal),
    Refused(ProbeRefusal),
}

/// Condition C11's binding calibration: over a spin with no context switch or
/// fault, the counter sees at least this CPU's local-timer interrupts and no
/// more than all of them. Retried up to ten times for a spin that was switched
/// out.
fn calibrate(counter: &IrqCounter, cpu: usize) -> Result<String, CalibrationFailure> {
    let refused = |e: irq_counter::CounterRefusal| CalibrationFailure::Refused(e.to_string());
    let lost = CalibrationFailure::CounterLost;
    const SPIN_TSC_TICKS: u64 = 300_000_000;
    const ATTEMPTS: usize = 20;
    let (mut switched, mut slow_reads) = (0, 0);
    for _attempt in 0..ATTEMPTS {
        // SAFETY: RDTSC is available on every x86_64 processor.
        let tsc = || unsafe { std::arch::x86_64::_rdtsc() };
        let measured_from = tsc();
        let before = irq_counter::snapshot_proc_interrupts().map_err(refused)?;
        let switches = switches_and_faults().map_err(CalibrationFailure::Refused)?;
        let start_count = counter.read().map_err(lost)?;
        let start = tsc();
        // SAFETY: as above.
        while unsafe { std::arch::x86_64::_rdtsc() } - start < SPIN_TSC_TICKS {
            std::hint::spin_loop();
        }
        let end_count = counter.read().map_err(lost)?;
        let end = tsc();
        let switches_after = switches_and_faults().map_err(CalibrationFailure::Refused)?;
        let after = irq_counter::snapshot_proc_interrupts().map_err(refused)?;
        let measured_to = tsc();
        let outside = 1.0 - (end - start) as f64 / (measured_to - measured_from) as f64;
        // A spin that was switched out or faulted says nothing, and one whose
        // surrounding reads took over 30% of the interval binds loosely.
        if switches_after != switches {
            switched += 1;
            continue;
        }
        if outside > 0.3 {
            slow_reads += 1;
            continue;
        }
        let before = irq_counter::interrupts_of(&before, cpu).map_err(refused)?;
        let after = irq_counter::interrupts_of(&after, cpu).map_err(refused)?;
        let kernel = irq_counter::CpuInterrupts {
            local_timer: after.local_timer - before.local_timer,
            total: after.total - before.total,
        };
        let count = end_count - start_count;
        irq_counter::calibration_verdict(count, kernel, outside).map_err(|why| {
            CalibrationFailure::Refused(irq_counter::CounterRefusal::Calibration(why).to_string())
        })?;
        return Ok(format!(
            "counter {count}, local timer {}, CPU total {} over {SPIN_TSC_TICKS} TSC ticks \
             ({:.1}% of the interval outside the counter)",
            kernel.local_timer,
            kernel.total,
            outside * 100.0
        ));
    }
    Err(CalibrationFailure::Refused(format!(
        "no interrupt-counter calibration spin was usable in {ATTEMPTS} attempts: \
         {switched} were switched out or faulted, {slow_reads} spent over 30% of their \
         interval reading /proc/interrupts"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_covered_component_sets_are_admitted() {
        assert_eq!(check_xcr0(0x2e7), Ok(()));
        assert_eq!(check_xcr0(0x207), Ok(()), "AVX2-only host with PKRU");
        assert_eq!(check_xcr0(0x7), Ok(()));
        assert!(check_xcr0(0x3).is_err(), "no AVX");
        assert!(check_xcr0(0x267).is_err(), "partial AVX-512");
        // APX (bit 19) and AMX (17, 18) are outside the probe.
        assert!(check_xcr0(0x802e7).is_err());
        assert!(check_xcr0(0x602e7).is_err());
        assert!(
            check_xcr0(0x2e7 | (1 << 62)).is_err(),
            "all 64 bits are checked"
        );
    }

    #[test]
    fn the_case_list_matches_its_independent_count() {
        for xcr0 in [0x2e7u64, 0x207, 0xe7, 0x7] {
            let cases = case_list(xcr0).unwrap();
            assert_eq!(cases.len(), expected_case_count(xcr0), "xcr0 {xcr0:#x}");
            for case in &cases {
                assert_eq!(case.targets & !xcr0, 0, "{case:?}");
                assert_eq!(case.live & !xcr0, 0, "{case:?}");
                assert_eq!(case.targets & case.live, 0, "{case:?}");
                assert_ne!(case.targets, 0, "{case:?}");
            }
        }
        assert_eq!(case_list(0x2e7).unwrap().len(), 33);
        // AVX2-only hosts get no AVX-512 instruction.
        assert!(case_list(0x207).unwrap().iter().all(|c| !matches!(
            c.kind,
            Kind::Hi16Zero | Kind::PxorVzeroupperHi16 | Kind::OpmaskZero
        )));
    }

    #[test]
    fn schedstat_timeslices_are_read_by_version() {
        let text = "version 17\ntimestamp 5\ncpu0 0 0 0 0 0 0 7 8 440964198\ncpu3 0 0 0 0 0 0 1 2 1072656639\n";
        assert_eq!(parse_schedstat_timeslices(text, 3), Ok(1_072_656_639));
        assert!(parse_schedstat_timeslices(text, 4).is_err());
        assert!(parse_schedstat_timeslices("version 18\ncpu0 0 0 0 0 0 0 1 2 3\n", 0).is_err());
        assert!(parse_schedstat_timeslices("cpu0 0 0 0 0 0 0 1 2 3\n", 0).is_err());
    }

    #[test]
    fn initial_state_is_recognized_from_the_header_or_the_values() {
        let layout = Layout {
            xcr0: 0x207,
            size: 832,
            components: vec![(2, 576, 256)],
        };
        let mut image = vec![0u8; 832];
        assert!(layout.is_initial(&image, 0, 0), "header bit clear");
        image[512] = 0b111;
        image[0..2].copy_from_slice(&0x37fu16.to_le_bytes());
        assert!(layout.is_initial(&image, 0, 0), "initial x87 values");
        image[40] = 1;
        assert!(!layout.is_initial(&image, 0, 0), "a nonzero ST register");
        assert!(layout.is_initial(&image, 0, 2));
        image[600] = 1;
        assert!(!layout.is_initial(&image, 0, 2));
        image[513] = 0b10;
        assert!(layout.is_initial(&image, 0, 9));
        assert!(!layout.is_initial(&image, 4, 9));
    }
}
