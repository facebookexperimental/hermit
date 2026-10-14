/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Whether a run may use `--cpu-profile x86-64-v3`, and the host facts it
//! binds when it may (<https://github.com/rrnewton/hermit/issues/4243>).
//!
//! Every class the startup qualification cannot cover is refused by name; the
//! run never falls back to v2.

use detcore_model::cpu_profile::HostXstate;

use crate::xstate_probe;

/// Why a v3 run is refused, as one sentence after "--cpu-profile x86-64-v3
/// refused: ".
pub type AdmissionRefusal = String;

/// What the run asks for, as far as admission is concerned.
#[derive(Clone, Copy, Debug)]
pub struct AdmissionRequest<'a> {
    /// The backend's command-line name, and whether it is the ptrace backend.
    pub backend: &'a str,
    pub ptrace: bool,
    pub virtualize_cpuid: bool,
    pub strace_only: bool,
    pub gdbserver: bool,
}

/// The refusals that need no host probe, in the order they are checked.
pub fn check_request(request: &AdmissionRequest<'_>) -> Result<(), AdmissionRefusal> {
    if !request.ptrace {
        let why = match request.backend {
            "kvm" => {
                "the KVM backend switches XCR0 between the guest's and the host's on every VM \
                 exit, a transition the startup qualification does not cover"
            }
            "dbt" => "the DBT client's CPUID table has no x86-64-v3 rows",
            "sabre" | "liteinst" | "in-guest-trap" => {
                "this backend does not trap CPUID, so the guest would see the host's"
            }
            _ => "only the ptrace backend is qualified",
        };
        return Err(format!("--backend={}: {why}", request.backend));
    }
    if !request.virtualize_cpuid || request.strace_only {
        return Err(
            "the profile is a virtual CPUID table, and this run does not virtualize CPUID".into(),
        );
    }
    if request.gdbserver {
        return Err(
            "--gdbserver writes the guest's FP registers, and the kernel's regset write forces \
             XSAVE header bits the qualification binds"
                .into(),
        );
    }
    Ok(())
}

/// Linux's version as (major, minor), from a `uname -r` release string.
pub fn kernel_version(release: &str) -> Option<(u32, u32)> {
    let mut parts = release.split(['.', '-']);
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    Some((major, minor))
}

/// Linux 5.16 is the first with the `ARCH_*_XCOMP_*` queries the profile
/// answers from the host; before it they return EINVAL.
pub fn check_kernel(release: &str) -> Result<(), AdmissionRefusal> {
    match kernel_version(release) {
        Some(version) if version >= (5, 16) => Ok(()),
        Some((major, minor)) => Err(format!(
            "Linux {major}.{minor} predates the 5.16 xstate queries the profile answers from"
        )),
        None => Err(format!("cannot read the kernel version from {release:?}")),
    }
}

/// A host CPUID reader, so tests can supply a host.
pub trait HostCpuid {
    fn cpuid(&self, leaf: u32, subleaf: u32) -> [u32; 4];
}

/// The CPUID instruction of the host this process runs on.
pub struct NativeCpuid;

impl HostCpuid for NativeCpuid {
    fn cpuid(&self, leaf: u32, subleaf: u32) -> [u32; 4] {
        let r = std::arch::x86_64::__cpuid_count(leaf, subleaf);
        [r.eax, r.ebx, r.ecx, r.edx]
    }
}

const REGISTER_NAMES: [&str; 4] = ["EAX", "EBX", "ECX", "EDX"];

/// The host checks that need only CPUID, in the order they are checked:
/// every advertised v3 feature bit natively, OSXSAVE, not a hybrid part, and
/// not an AMD part without XSAVEERPTR (where Linux runs `fnclex; emms; fildl`
/// before every FPU restore, a path no host here can exercise).
pub fn check_host_cpuid(
    host: &dyn HostCpuid,
    advertised: &[(u32, u32, usize, u32)],
) -> Result<(), AdmissionRefusal> {
    let max_basic = host.cpuid(0, 0)[0];
    let max_extended = host.cpuid(0x8000_0000, 0)[0];
    let mut missing = Vec::new();
    for &(leaf, subleaf, register, bits) in advertised {
        let supported = if leaf >= 0x8000_0000 {
            leaf <= max_extended
        } else {
            leaf <= max_basic
        };
        let native = if supported {
            host.cpuid(leaf, subleaf)[register]
        } else {
            0
        };
        let absent = bits & !native;
        for bit in 0..32 {
            if absent & (1 << bit) != 0 {
                missing.push(format!(
                    "leaf {leaf:#x}.{subleaf} {} bit {bit}",
                    REGISTER_NAMES[register]
                ));
            }
        }
    }
    if !missing.is_empty() {
        return Err(format!(
            "the host lacks feature bits the profile advertises: {}",
            missing.join(", ")
        ));
    }
    if host.cpuid(1, 0)[2] & (1 << 27) == 0 {
        return Err("the host kernel has not enabled XSAVE (OSXSAVE is clear)".into());
    }
    if max_basic >= 7 && host.cpuid(7, 0)[3] & (1 << 15) != 0 {
        return Err(
            "this is a hybrid part (leaf 7 EDX bit 15), whose core types the qualification \
             would each have to sample"
                .into(),
        );
    }
    let vendor = host.cpuid(0, 0);
    let amd = vendor[1] == 0x6874_7541;
    let xsaveerptr = max_extended >= 0x8000_0008 && host.cpuid(0x8000_0008, 0)[1] & (1 << 2) != 0;
    if amd && !xsaveerptr {
        return Err(
            "this AMD part lacks XSAVEERPTR, so Linux runs `fnclex; emms; fildl` before every \
             FPU restore, which the qualification cannot exercise"
                .into(),
        );
    }
    Ok(())
}

/// The user-dynamic xstate component (AMX TILEDATA): Linux enables it in XCR0
/// while withholding process permission, so a permission request changes
/// process state and the three XCOMP answers differ.
const XFEATURE_TILEDATA: u64 = 1 << 18;

const ARCH_GET_XCOMP_SUPP: libc::c_int = 0x1021;
const ARCH_GET_XCOMP_PERM: libc::c_int = 0x1022;
const ARCH_REQ_XCOMP_PERM: libc::c_int = 0x1023;
const ARCH_GET_XCOMP_GUEST_PERM: libc::c_int = 0x1024;

fn arch_prctl_mask(cmd: libc::c_int) -> Result<u64, AdmissionRefusal> {
    let mut mask = 0u64;
    // SAFETY: the GET commands write one u64 through the pointer.
    let rc = unsafe { libc::syscall(libc::SYS_arch_prctl, cmd, &mut mask as *mut u64) };
    if rc != 0 {
        return Err(format!(
            "arch_prctl({cmd:#x}) failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(mask)
}

/// Linux's XFEATURE_MAX as ARCH_REQ_XCOMP_PERM shows it: the first component
/// index it answers EINVAL for. Every index below must answer EOPNOTSUPP; an
/// index that is granted would mean a dynamic feature, which is refused
/// before this runs.
fn xfeature_max() -> Result<u32, AdmissionRefusal> {
    for index in 0..64u64 {
        // SAFETY: the request takes the component index by value.
        let rc = unsafe { libc::syscall(libc::SYS_arch_prctl, ARCH_REQ_XCOMP_PERM, index) };
        let errno = std::io::Error::last_os_error().raw_os_error();
        match (rc, errno) {
            (-1, Some(libc::EINVAL)) => return Ok(index as u32),
            (-1, Some(libc::EOPNOTSUPP)) => {}
            _ => {
                return Err(format!(
                    "ARCH_REQ_XCOMP_PERM for component {index} answered {rc} ({errno:?}), \
                     not EOPNOTSUPP or EINVAL"
                ));
            }
        }
    }
    Err("ARCH_REQ_XCOMP_PERM never answered EINVAL for indices 0-63".into())
}

/// The host facts a qualified v3 run binds.
fn capture(probe: &xstate_probe::ProbeReport) -> Result<HostXstate, AdmissionRefusal> {
    let host = NativeCpuid;
    let leaf0 = host.cpuid(0, 0);
    // SAFETY: XGETBV with ECX 0 is available because OSXSAVE was checked.
    let xcr0 = unsafe { std::arch::x86_64::_xgetbv(0) };
    if xcr0 != probe.xcr0 {
        return Err(format!(
            "XCR0 changed during admission ({:#x}, then {xcr0:#x})",
            probe.xcr0
        ));
    }
    let mut xsave_leaves = Vec::new();
    for subleaf in std::iter::once(0).chain(2..64) {
        if subleaf == 0 || xcr0 & (1 << subleaf) != 0 {
            let [eax, ebx, ecx, edx] = host.cpuid(0xd, subleaf);
            xsave_leaves.push([subleaf, eax, ebx, ecx, edx]);
        }
    }
    let xcomp_supp = arch_prctl_mask(ARCH_GET_XCOMP_SUPP)?;
    if (xcomp_supp | xcr0) & XFEATURE_TILEDATA != 0 {
        return Err(
            "the host has a user-dynamic xstate feature (AMX TILEDATA), whose permission \
             requests change process state"
                .into(),
        );
    }
    Ok(HostXstate {
        vendor: [leaf0[1], leaf0[3], leaf0[2]],
        signature: host.cpuid(1, 0)[0],
        xcr0,
        xsave_leaves,
        xcomp_supp,
        xcomp_perm: arch_prctl_mask(ARCH_GET_XCOMP_PERM)?,
        xcomp_guest_perm: arch_prctl_mask(ARCH_GET_XCOMP_GUEST_PERM)?,
        xfeature_max: xfeature_max()?,
        probe_cases: probe.cases as u32,
        probe_forced_exit: probe.forced_exit,
    })
}

/// Admits a v3 run on this host, or names why not. On success the probe report
/// and the host facts to bind are returned.
pub fn admit(
    request: &AdmissionRequest<'_>,
) -> Result<(xstate_probe::ProbeReport, HostXstate), AdmissionRefusal> {
    check_request(request)?;
    let release = nix::sys::utsname::uname()
        .map_err(|e| format!("uname failed: {e}"))?
        .release()
        .to_string_lossy()
        .into_owned();
    check_kernel(&release)?;
    check_host_cpuid(&NativeCpuid, &detcore::v3_feature_words())?;
    // SAFETY: OSXSAVE was checked above.
    let xcr0 = unsafe { std::arch::x86_64::_xgetbv(0) };
    if xcr0 & XFEATURE_TILEDATA != 0 {
        return Err("the host enables a user-dynamic xstate feature (AMX TILEDATA) in XCR0".into());
    }
    let probe = xstate_probe::qualify(xstate_probe::ProbeOptions::default())?;
    let host = capture(&probe)?;
    Ok((probe, host))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn request(backend: &str) -> AdmissionRequest<'_> {
        AdmissionRequest {
            backend,
            ptrace: backend == "ptrace",
            virtualize_cpuid: true,
            strace_only: false,
            gdbserver: false,
        }
    }

    #[test]
    fn only_a_ptrace_run_with_virtual_cpuid_and_no_gdbserver_is_considered() {
        assert_eq!(check_request(&request("ptrace")), Ok(()));
        for backend in [
            "kvm",
            "dbt",
            "sabre",
            "liteinst",
            "in-guest-trap",
            "e9patch",
        ] {
            let refusal = check_request(&request(backend)).unwrap_err();
            assert!(
                refusal.starts_with(&format!("--backend={backend}")),
                "{refusal}"
            );
        }
        assert!(check_request(&request("kvm")).unwrap_err().contains("XCR0"));
        assert!(
            check_request(&request("dbt"))
                .unwrap_err()
                .contains("no x86-64-v3 rows")
        );
        let mut r = request("ptrace");
        r.virtualize_cpuid = false;
        assert!(check_request(&r).is_err());
        let mut r = request("ptrace");
        r.strace_only = true;
        assert!(check_request(&r).is_err());
        let mut r = request("ptrace");
        r.gdbserver = true;
        assert!(check_request(&r).unwrap_err().contains("--gdbserver"));
    }

    #[test]
    fn kernels_before_5_16_are_refused() {
        assert_eq!(kernel_version("7.1.3-0_fbk0_0_g295ad0959f34"), Some((7, 1)));
        assert_eq!(check_kernel("5.16.0"), Ok(()));
        assert_eq!(check_kernel("6.4.3-0_fbk20"), Ok(()));
        assert!(check_kernel("5.15.120").is_err());
        assert!(check_kernel("4.18.0-553.el8").is_err());
        assert!(check_kernel("garbage").is_err());
    }

    struct FakeHost(BTreeMap<(u32, u32), [u32; 4]>);

    impl HostCpuid for FakeHost {
        fn cpuid(&self, leaf: u32, subleaf: u32) -> [u32; 4] {
            self.0.get(&(leaf, subleaf)).copied().unwrap_or([0; 4])
        }
    }

    /// A bare-metal-shaped host with every v3 bit, AMD with XSAVEERPTR.
    fn qualifying_host() -> FakeHost {
        let mut regs = BTreeMap::new();
        regs.insert((0, 0), [0xd, 0x6874_7541, 0x444d_4163, 0x6974_6e65]);
        regs.insert((0x8000_0000, 0), [0x8000_0008, 0, 0, 0]);
        regs.insert((0x8000_0008, 0), [0, 1 << 2, 0, 0]);
        for &(leaf, subleaf, register, bits) in &detcore::v3_feature_words() {
            regs.entry((leaf, subleaf)).or_insert([0; 4])[register] |= bits;
        }
        regs.get_mut(&(1, 0)).unwrap()[2] |= 1 << 27;
        FakeHost(regs)
    }

    #[test]
    fn a_host_with_every_v3_bit_qualifies_and_removing_any_one_is_named() {
        let words = detcore::v3_feature_words();
        let host = qualifying_host();
        assert_eq!(check_host_cpuid(&host, &words), Ok(()));
        // The hypervisor bit is not required: v3 clears it.
        assert!(words.iter().all(|&(leaf, _, register, bits)| !(leaf == 1
            && register == 2
            && bits & (1 << 31) != 0)));
        let mut checked = 0;
        for &(leaf, subleaf, register, bits) in &words {
            for bit in (0..32).filter(|b| bits & (1 << b) != 0) {
                let mut host = qualifying_host();
                host.0.get_mut(&(leaf, subleaf)).unwrap()[register] &= !(1 << bit);
                if leaf == 1 && register == 2 && bit == 27 {
                    continue; // OSXSAVE: refused by its own check, below
                }
                let refusal = check_host_cpuid(&host, &words).unwrap_err();
                assert!(
                    refusal.contains(&format!(
                        "leaf {leaf:#x}.{subleaf} {} bit {bit}",
                        REGISTER_NAMES[register]
                    )),
                    "{refusal}"
                );
                checked += 1;
            }
        }
        let advertised: u32 = words.iter().map(|&(_, _, _, bits)| bits.count_ones()).sum();
        // Every advertised bit but OSXSAVE, which has its own check.
        assert_eq!(checked, advertised - 1);
    }

    #[test]
    fn osxsave_hybrid_parts_and_old_amd_are_refused() {
        let words = detcore::v3_feature_words();
        let mut host = qualifying_host();
        host.0.get_mut(&(1, 0)).unwrap()[2] &= !(1 << 27);
        assert!(check_host_cpuid(&host, &words).is_err());
        let mut host = qualifying_host();
        host.0.get_mut(&(7, 0)).unwrap()[3] |= 1 << 15;
        assert!(
            check_host_cpuid(&host, &words)
                .unwrap_err()
                .contains("hybrid")
        );
        let mut host = qualifying_host();
        host.0.get_mut(&(0x8000_0008, 0)).unwrap()[1] = 0;
        assert!(
            check_host_cpuid(&host, &words)
                .unwrap_err()
                .contains("XSAVEERPTR")
        );
    }
}
