// (c) Meta Platforms, Inc. and affiliates. Confidential and proprietary.

//! cpuid interception

use raw_cpuid::CpuIdResult;

#[derive(Debug, Clone, Copy)]
pub struct InterceptedCpuid();

impl InterceptedCpuid {
    pub fn new() -> Self {
        InterceptedCpuid()
    }
}

impl InterceptedCpuid {
    pub fn cpuid(&self, index: u32) -> Option<CpuIdResult> {
        let request = index as usize;
        if request >= 0x80000000 && request < 0x80000000 + EXTENDED_CPUIDS.len() {
            Some(EXTENDED_CPUIDS[request - 0x80000000])
        } else if request < CPUIDS.len() {
            Some(CPUIDS[request])
        } else {
            None
        }
    }
}

const fn cpuid_result(eax: u32, ebx: u32, ecx: u32, edx: u32) -> CpuIdResult {
    CpuIdResult { eax, ebx, ecx, edx }
}

// CPUID output from older CPU (broadwell?), with some features like RDRAND
// masked off to prevent non-determinism.
const CPUIDS: &[CpuIdResult] = &[
    cpuid_result(0x0000000D, 0x756E6547, 0x6C65746E, 0x49656E69),
    cpuid_result(0x00000663, 0x00000800, 0x90202001, 0x078BFBFD),
    cpuid_result(0x00000001, 0x00000000, 0x0000004D, 0x002C307D),
    cpuid_result(0x00000000, 0x00000000, 0x00000000, 0x00000000),
    cpuid_result(0x00000120, 0x01C0003F, 0x0000003F, 0x00000001),
    cpuid_result(0x00000000, 0x00000000, 0x00000003, 0x00000000),
    cpuid_result(0x00000000, 0x00000000, 0x00000000, 0x00000000),
    cpuid_result(0x00000000, 0x00180FB9, 0x00000000, 0x00000000),
    cpuid_result(0x00000000, 0x00000000, 0x00000000, 0x00000000),
    cpuid_result(0x00000000, 0x00000000, 0x00000000, 0x00000000),
    cpuid_result(0x00000000, 0x00000000, 0x00000000, 0x00000000),
    cpuid_result(0x00000000, 0x00000001, 0x00000100, 0x00000001),
    cpuid_result(0x00000000, 0x00000000, 0x00000000, 0x00000000),
    cpuid_result(0x00000000, 0x00000000, 0x00000000, 0x00000000),
    cpuid_result(0x00000000, 0x00000000, 0x00000000, 0x00000000),
    cpuid_result(0x00000000, 0x00000000, 0x00000000, 0x00000000),
    cpuid_result(0x00000000, 0x00000000, 0x00000000, 0x00000000),
    cpuid_result(0x00000000, 0x00000000, 0x00000000, 0x00000000),
    cpuid_result(0x00000000, 0x00000000, 0x00000000, 0x00000000),
    cpuid_result(0x00000000, 0x00000000, 0x00000000, 0x00000000),
    cpuid_result(0x00000000, 0x00000000, 0x00000000, 0x00000000),
];

const EXTENDED_CPUIDS: &[CpuIdResult] = &[
    cpuid_result(0x8000000A, 0x756E6547, 0x6C65746E, 0x49656E69),
    cpuid_result(0x00000663, 0x00000000, 0x00000001, 0x20100800),
    cpuid_result(0x554D4551, 0x72695620, 0x6C617574, 0x55504320),
    cpuid_result(0x72657620, 0x6E6F6973, 0x352E3220, 0x0000002B),
    cpuid_result(0x00000000, 0x00000000, 0x00000000, 0x00000000),
    cpuid_result(0x01FF01FF, 0x01FF01FF, 0x40020140, 0x40020140),
    cpuid_result(0x00000000, 0x42004200, 0x02008140, 0x00808140),
    cpuid_result(0x00000000, 0x00000000, 0x00000000, 0x00000000),
    cpuid_result(0x00003028, 0x00000000, 0x00000000, 0x00000000),
    cpuid_result(0x00000000, 0x00000000, 0x00000000, 0x00000000),
    cpuid_result(0x00000000, 0x00000000, 0x00000000, 0x00000000),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpuid_leaf_count() {
        assert!(CPUIDS[0].eax as usize <= CPUIDS.len());
        assert_eq!(
            1 + (EXTENDED_CPUIDS[0].eax as usize & !0x80000000usize),
            EXTENDED_CPUIDS.len()
        );
    }
}
