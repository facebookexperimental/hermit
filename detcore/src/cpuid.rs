/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

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
    pub fn cpuid(&self, leaf: u32, subleaf: u32) -> Option<CpuIdResult> {
        let request = leaf as usize;
        let result = if request >= 0x80000000 && request < 0x80000000 + EXTENDED_CPUIDS.len() {
            EXTENDED_CPUIDS[request - 0x80000000]
        } else if request < CPUIDS.len() {
            CPUIDS[request]
        } else {
            return None;
        };

        if subleaf != 0 && uses_subleaf(leaf) {
            Some(cpuid_result(0, 0, 0, 0))
        } else {
            Some(result)
        }
    }
}

impl InterceptedCpuid {
    /// `/proc/cpuinfo` for the virtual CPU this table describes, so that the
    /// file and the CPUID instruction show a guest the same CPU
    /// (<https://github.com/rrnewton/hermit/issues/4161>). Without this, the
    /// file shows the host's model, flags and caches, which differ from host
    /// to host and contradict CPUID.
    ///
    /// It is the block Linux's `show_cpuinfo` (arch/x86/kernel/cpu/proc.c)
    /// prints for CPU 0 of a one-CPU machine whose CPUID answers as this table
    /// does, with each value derived as Linux 7.1.3 derives it:
    /// - the vendor, family, model, stepping and `cpuid level` from leaves 0
    ///   and 1, and the model name from leaves 0x80000002-4, trimmed, or
    ///   `family/model` in hex when they hold no name (`identify_cpu`);
    /// - the cache size, and the TLB size when Linux computes one, by the
    ///   vendor's path: for GenuineIntel `init_intel_cacheinfo` (leaf 4,
    ///   else leaf 2's descriptors, falling back to the extended leaves when
    ///   no L2 is found), otherwise `cpu_detect_cache_sizes` (leaves
    ///   0x80000005 and 0x80000006);
    /// - the `clflush` size and `cache_alignment` from leaf 1, with
    ///   `get_cpu_address_sizes`'s defaults, and the address sizes from leaf
    ///   0x80000008;
    /// - `flags`: the named bits of every CPUID word in Linux's capability
    ///   array ([`CPUINFO_FLAGS`]), less `pat` on the Intel family 6 models
    ///   `early_init_intel` clears it for, and `power management` from leaf
    ///   0x80000007's EDX.
    ///
    /// The rest have no CPUID source and are fixed: one CPU at 1000 MHz with
    /// the matching TSC-calibrated `bogomips` (2000.00), and no microcode
    /// revision. `bugs` is [`CPUINFO_BUGS`], the set Linux derives for this
    /// table; it is not recomputed from the table, and
    /// `tests::cpuinfo_describes_the_virtual_cpu` pins it together with the
    /// table so that a table change must revisit it. Linux's synthetic flags
    /// (such as `constant_tsc`), which describe the kernel rather than CPUID,
    /// are not shown.
    pub fn cpuinfo(&self) -> Vec<u8> {
        render_cpuinfo(|leaf, subleaf| {
            self.cpuid(leaf, subleaf)
                .unwrap_or(cpuid_result(0, 0, 0, 0))
        })
    }
}

/// [`InterceptedCpuid::cpuinfo`] for the CPU whose CPUID answers as `leaf`
/// does.
fn render_cpuinfo(leaf: impl Fn(u32, u32) -> CpuIdResult) -> Vec<u8> {
    let basic = leaf(0, 0);
    let vendor: Vec<u8> = [basic.ebx, basic.edx, basic.ecx]
        .iter()
        .flat_map(|register| register.to_le_bytes())
        .collect();
    let signature = leaf(1, 0);
    let base_family = (signature.eax >> 8) & 0xf;
    let family = if base_family == 0xf {
        base_family + ((signature.eax >> 20) & 0xff)
    } else {
        base_family
    };
    let mut model = (signature.eax >> 4) & 0xf;
    if family >= 6 {
        model |= ((signature.eax >> 16) & 0xf) << 4;
    }
    let extended_max = leaf(0x8000_0000, 0).eax;
    let model_name = if extended_max >= 0x8000_0004 {
        let bytes: Vec<u8> = (0x8000_0002..=0x8000_0004)
            .map(|brand| leaf(brand, 0))
            .flat_map(|r| [r.eax, r.ebx, r.ecx, r.edx])
            .flat_map(|register| register.to_le_bytes())
            .take_while(|byte| *byte != 0)
            .collect();
        String::from_utf8_lossy(&bytes)
            .trim_start_matches(' ')
            .trim_end()
            .to_owned()
    } else {
        String::new()
    };
    let model_name = if model_name.is_empty() {
        format!("{family:02x}/{model:02x}")
    } else {
        model_name
    };
    let intel = vendor == b"GenuineIntel";
    let (cache_kb, tlb_pages) = if intel {
        intel_cache_sizes(&leaf, basic.eax, extended_max)
    } else {
        extended_cache_sizes(&leaf, extended_max, 0)
    };
    // `get_cpu_address_sizes`: 64 bytes without leaf 0x80000008, and 32
    // when leaf 1 enumerates none.
    let mut clflush = 64;
    if signature.edx & (1 << 19) != 0 {
        clflush = ((signature.ebx >> 8) & 0xff) * 8;
    }
    if extended_max < 0x8000_0008 {
        clflush = 64;
    } else if clflush == 0 {
        clflush = 32;
    }
    let cache_alignment = if intel && family == 15 {
        clflush * 2
    } else {
        clflush
    };
    let (physical_bits, virtual_bits) = if extended_max >= 0x8000_0008 {
        let sizes = leaf(0x8000_0008, 0).eax;
        (sizes & 0xff, (sizes >> 8) & 0xff)
    } else {
        (36, 48)
    };
    let flags: Vec<&str> = CPUINFO_FLAGS
        .iter()
        .flat_map(|word| {
            let value = word.register.of(leaf(word.leaf, word.subleaf));
            word.names()
                .filter(move |(bit, _)| value & (1 << bit) != 0)
                .map(|(_, name)| name)
        })
        // `early_init_intel` clears PAT from the Pentium Pro to the Core
        // Yonah (family 6, models 0x01-0x0e) for an erratum.
        .filter(|name| !(*name == "pat" && intel && family == 6 && (1..=0xe).contains(&model)))
        .collect();
    let power = if extended_max >= 0x8000_0007 {
        leaf(0x8000_0007, 0).edx
    } else {
        0
    };
    let power: String = (0..32)
        .filter(|bit| power & (1 << bit) != 0)
        .map(|bit| match POWER_FLAGS.get(bit) {
            Some(&"") => String::new(),
            Some(name) => format!(" {name}"),
            None => format!(" [{bit}]"),
        })
        .collect();

    let mut out = String::new();
    out += "processor\t: 0\n";
    out += &format!("vendor_id\t: {}\n", String::from_utf8_lossy(&vendor));
    out += &format!("cpu family\t: {family}\n");
    out += &format!("model\t\t: {model}\n");
    out += &format!("model name\t: {model_name}\n");
    out += &format!("stepping\t: {}\n", signature.eax & 0xf);
    if signature.edx & (1 << 4) != 0 {
        out += "cpu MHz\t\t: 1000.000\n";
    }
    if cache_kb != 0 {
        out += &format!("cache size\t: {cache_kb} KB\n");
    }
    out += "physical id\t: 0\nsiblings\t: 1\ncore id\t\t: 0\ncpu cores\t: 1\n";
    out += "apicid\t\t: 0\ninitial apicid\t: 0\n";
    out += "fpu\t\t: yes\nfpu_exception\t: yes\n";
    out += &format!("cpuid level\t: {}\n", basic.eax);
    out += "wp\t\t: yes\n";
    out += &format!("flags\t\t: {}\n", flags.join(" "));
    out += &format!("bugs\t\t: {CPUINFO_BUGS}\n");
    out += "bogomips\t: 2000.00\n";
    if tlb_pages != 0 {
        out += &format!("TLB size\t: {tlb_pages} 4K pages\n");
    }
    out += &format!("clflush size\t: {clflush}\n");
    out += &format!("cache_alignment\t: {cache_alignment}\n");
    out +=
        &format!("address sizes\t: {physical_bits} bits physical, {virtual_bits} bits virtual\n");
    out += &format!("power management:{power}\n\n");
    out.into_bytes()
}

/// Linux's cache and TLB sizes for GenuineIntel (`init_intel_cacheinfo`,
/// arch/x86/kernel/cpu/cacheinfo.c), in KB and 4K pages: from leaf 4 when it
/// enumerates a cache, else from leaf 2's descriptors; with no L2 found,
/// `cpu_detect_cache_sizes` decides. The size is the L3, else the L2, else
/// L1i + L1d. Only `cpu_detect_cache_sizes` sets a TLB size.
fn intel_cache_sizes(
    leaf: &impl Fn(u32, u32) -> CpuIdResult,
    max_leaf: u32,
    extended_max: u32,
) -> (u32, u32) {
    let (mut l1i, mut l1d, mut l2, mut l3) = (0, 0, 0, 0);
    // `intel_cacheinfo_0x4`: subleaves until the first NULL cache type.
    let leaf4: Vec<CpuIdResult> = if max_leaf >= 4 {
        (0..64)
            .map(|subleaf| leaf(4, subleaf))
            .take_while(|cache| cache.eax & 0x1f != 0)
            .collect()
    } else {
        Vec::new()
    };
    if !leaf4.is_empty() {
        for cache in leaf4 {
            let kb = (((cache.ecx) + 1)
                * ((cache.ebx & 0xfff) + 1)
                * (((cache.ebx >> 12) & 0x3ff) + 1)
                * ((cache.ebx >> 22) + 1))
                / 1024;
            match ((cache.eax >> 5) & 0x7, cache.eax & 0x1f) {
                (1, 1) => l1d = kb,
                (1, 2) => l1i = kb,
                (2, _) => l2 = kb,
                (3, _) => l3 = kb,
                _ => {}
            }
        }
    } else if max_leaf >= 2 {
        // `intel_cacheinfo_0x2`: descriptor bytes 1-15, all NULL unless the
        // iteration count (byte 0) is 1, and a register's NULL when its top
        // bit is set (`cpuid_leaf_0x2`).
        let descriptors = leaf(2, 0);
        let registers = [
            descriptors.eax,
            descriptors.ebx,
            descriptors.ecx,
            descriptors.edx,
        ];
        let bytes: Vec<u8> = registers
            .iter()
            .flat_map(|register| {
                if register & (1 << 31) != 0 {
                    [0; 4]
                } else {
                    register.to_le_bytes()
                }
            })
            .collect();
        if bytes[0] == 0x01 {
            for descriptor in &bytes[1..] {
                match leaf2_cache(*descriptor) {
                    Some(('i', kb)) => l1i += kb,
                    Some(('d', kb)) => l1d += kb,
                    Some(('2', kb)) => l2 += kb,
                    Some(('3', kb)) => l3 += kb,
                    _ => {}
                }
            }
        }
    }
    // `intel_cacheinfo_done`.
    let cache_kb = if l3 != 0 {
        l3
    } else if l2 != 0 {
        l2
    } else {
        l1i + l1d
    };
    if l2 == 0 {
        return extended_cache_sizes(leaf, extended_max, cache_kb);
    }
    (cache_kb, 0)
}

/// `cpu_detect_cache_sizes` (arch/x86/kernel/cpu/common.c): the cache size
/// in KB and the TLB size in 4K pages from leaves 0x80000005 and 0x80000006,
/// each replacing `cache_kb`, the size known before, when present.
fn extended_cache_sizes(
    leaf: &impl Fn(u32, u32) -> CpuIdResult,
    extended_max: u32,
    mut cache_kb: u32,
) -> (u32, u32) {
    let mut tlb_pages = 0;
    if extended_max >= 0x8000_0005 {
        let l1 = leaf(0x8000_0005, 0);
        cache_kb = (l1.ecx >> 24) + (l1.edx >> 24);
    }
    if extended_max >= 0x8000_0006 {
        let l2 = leaf(0x8000_0006, 0);
        cache_kb = l2.ecx >> 16;
        tlb_pages = ((l2.ebx >> 16) & 0xfff) + (l2.ebx & 0xfff);
    }
    (cache_kb, tlb_pages)
}

/// A leaf 2 cache descriptor's level (`i`/`d` for L1 instruction or data,
/// `2`, `3`) and size in KB.
fn leaf2_cache(descriptor: u8) -> Option<(char, u32)> {
    LEAF2_CACHES.split_whitespace().find_map(|entry| {
        let mut fields = entry.split(':');
        let code = u8::from_str_radix(fields.next()?, 16).ok()?;
        let level = fields.next()?.chars().next()?;
        let kb = fields.next()?.parse().ok()?;
        (code == descriptor).then_some((level, kb))
    })
}

/// Linux 7.1.3's leaf 2 cache descriptors, `code:level:KB`
/// (arch/x86/kernel/cpu/cpuid_0x2_table.c, `CACHE_ENTRY` rows; the TLB rows
/// do not bear on /proc/cpuinfo).
const LEAF2_CACHES: &str = "06:i:8 08:i:16 09:i:32 0a:d:8 0c:d:16 0d:d:16 0e:d:24 21:2:256 22:3:512 \
    23:3:1024 25:3:2048 29:3:4096 2c:d:32 30:i:32 39:2:128 3a:2:192 3b:2:128 \
    3c:2:256 3d:2:384 3e:2:512 3f:2:256 41:2:128 42:2:256 43:2:512 44:2:1024 \
    45:2:2048 46:3:4096 47:3:8192 48:2:3072 49:3:4096 4a:3:6144 4b:3:8192 \
    4c:3:12288 4d:3:16384 4e:2:6144 60:d:16 66:d:8 67:d:16 68:d:32 78:2:1024 \
    79:2:128 7a:2:256 7b:2:512 7c:2:1024 7d:2:2048 7f:2:512 80:2:512 82:2:256 \
    83:2:512 84:2:1024 85:2:2048 86:2:512 87:2:1024 d0:3:512 d1:3:1024 d2:3:2048 \
    d6:3:1024 d7:3:2048 d8:3:4096 dc:3:2048 dd:3:4096 de:3:8192 e2:3:2048 \
    e3:3:4096 e4:3:8192 ea:3:12288 eb:3:18432 ec:3:24576";

/// The `bugs` line Linux 7.1.3 derives for this table (`cpu_set_bug_bits`,
/// arch/x86/kernel/cpu/common.c), in bug-bit order: an Intel family 6 model 6
/// matches no entry of its vulnerability lists, the table enumerates no
/// IA32_ARCH_CAPABILITIES (leaf 7 EDX bit 29), so every `*_NO` bit is clear,
/// and it advertises RTM (`taa`) and the hypervisor bit (`bhi`, `its`).
const CPUINFO_BUGS: &str = "cpu_meltdown spectre_v1 spectre_v2 spec_store_bypass l1tf mds swapgs taa \
    itlb_multihit bhi spectre_v2_user its";

/// A CPUID register.
#[derive(Clone, Copy, Debug)]
enum Register {
    Eax,
    Ebx,
    Ecx,
    Edx,
}

impl Register {
    fn of(self, result: CpuIdResult) -> u32 {
        match self {
            Register::Eax => result.eax,
            Register::Ebx => result.ebx,
            Register::Ecx => result.ecx,
            Register::Edx => result.edx,
        }
    }
}

/// One word of Linux's CPU capability array that comes straight from a CPUID
/// register, with the names `/proc/cpuinfo` shows for its bits.
struct CpuinfoFlagWord {
    leaf: u32,
    subleaf: u32,
    register: Register,
    /// `bit:name` for each named bit, separated by whitespace.
    names: &'static str,
}

impl CpuinfoFlagWord {
    fn names(&self) -> impl Iterator<Item = (u32, &'static str)> {
        self.names.split_whitespace().map(|entry| {
            let (bit, name) = entry.split_once(':').expect("a `bit:name` entry");
            (bit.parse().expect("a bit number"), name)
        })
    }
}

/// The CPUID-sourced words of Linux 7.1.3's capability array, in the order
/// `show_cpuinfo` prints them, with every bit that has a quoted name in
/// arch/x86/include/asm/cpufeatures.h (the only bits /proc/cpuinfo shows).
/// Words 2, 3, 5, 7, 8, 11, 17 and 21 are left out: they are Linux-defined or
/// belong to other vendors' leaves, which this table does not answer. Word 20
/// (leaf 0x80000021 EAX) is left out because Linux shows none of its bits.
const CPUINFO_FLAGS: &[CpuinfoFlagWord] = &[
    // Word 0.
    CpuinfoFlagWord {
        leaf: 0x0000_0001,
        subleaf: 0,
        register: Register::Edx,
        names: "0:fpu 1:vme 2:de 3:pse 4:tsc 5:msr 6:pae 7:mce 8:cx8 9:apic 11:sep 12:mtrr \
            13:pge 14:mca 15:cmov 16:pat 17:pse36 18:pn 19:clflush 21:dts 22:acpi 23:mmx \
            24:fxsr 25:sse 26:sse2 27:ss 28:ht 29:tm 30:ia64 31:pbe",
    },
    // Word 1.
    CpuinfoFlagWord {
        leaf: 0x8000_0001,
        subleaf: 0,
        register: Register::Edx,
        names: "11:syscall 19:mp 20:nx 22:mmxext 25:fxsr_opt 26:pdpe1gb 27:rdtscp 29:lm \
            30:3dnowext 31:3dnow",
    },
    // Word 4.
    CpuinfoFlagWord {
        leaf: 0x0000_0001,
        subleaf: 0,
        register: Register::Ecx,
        names: "0:pni 1:pclmulqdq 2:dtes64 3:monitor 4:ds_cpl 5:vmx 6:smx 7:est 8:tm2 9:ssse3 \
            10:cid 11:sdbg 12:fma 13:cx16 14:xtpr 15:pdcm 17:pcid 18:dca 19:sse4_1 \
            20:sse4_2 21:x2apic 22:movbe 23:popcnt 24:tsc_deadline_timer 25:aes 26:xsave \
            28:avx 29:f16c 30:rdrand 31:hypervisor",
    },
    // Word 6.
    CpuinfoFlagWord {
        leaf: 0x8000_0001,
        subleaf: 0,
        register: Register::Ecx,
        names: "0:lahf_lm 1:cmp_legacy 2:svm 3:extapic 4:cr8_legacy 5:abm 6:sse4a \
            7:misalignsse 8:3dnowprefetch 9:osvw 10:ibs 11:xop 12:skinit 13:wdt 15:lwp \
            16:fma4 17:tce 19:nodeid_msr 21:tbm 22:topoext 23:perfctr_core 24:perfctr_nb \
            26:bpext 27:ptsc 28:perfctr_llc 29:mwaitx",
    },
    // Word 9.
    CpuinfoFlagWord {
        leaf: 0x0000_0007,
        subleaf: 0,
        register: Register::Ebx,
        names: "0:fsgsbase 1:tsc_adjust 2:sgx 3:bmi1 4:hle 5:avx2 7:smep 8:bmi2 9:erms \
            10:invpcid 11:rtm 12:cqm 14:mpx 15:rdt_a 16:avx512f 17:avx512dq 18:rdseed \
            19:adx 20:smap 21:avx512ifma 23:clflushopt 24:clwb 25:intel_pt 26:avx512pf \
            27:avx512er 28:avx512cd 29:sha_ni 30:avx512bw 31:avx512vl",
    },
    // Word 10.
    CpuinfoFlagWord {
        leaf: 0x0000_000d,
        subleaf: 1,
        register: Register::Eax,
        names: "0:xsaveopt 1:xsavec 2:xgetbv1 3:xsaves",
    },
    // Word 12.
    CpuinfoFlagWord {
        leaf: 0x0000_0007,
        subleaf: 1,
        register: Register::Eax,
        names: "4:avx_vnni 5:avx512_bf16 6:lass 17:fred 26:lam",
    },
    // Word 13.
    CpuinfoFlagWord {
        leaf: 0x8000_0008,
        subleaf: 0,
        register: Register::Ebx,
        names: "0:clzero 1:irperf 2:xsaveerptr 4:rdpru 9:wbnoinvd 23:amd_ppin 25:virt_ssbd \
            27:cppc 31:brs",
    },
    // Word 14.
    CpuinfoFlagWord {
        leaf: 0x0000_0006,
        subleaf: 0,
        register: Register::Eax,
        names: "0:dtherm 1:ida 2:arat 4:pln 6:pts 7:hwp 8:hwp_notify 9:hwp_act_window \
            10:hwp_epp 11:hwp_pkg_req 19:hfi",
    },
    // Word 15.
    CpuinfoFlagWord {
        leaf: 0x8000_000a,
        subleaf: 0,
        register: Register::Edx,
        names: "0:npt 1:lbrv 2:svm_lock 3:nrip_save 4:tsc_scale 5:vmcb_clean 6:flushbyasid \
            7:decodeassists 10:pausefilter 12:pfthreshold 13:avic 15:v_vmsave_vmload \
            16:vgif 18:x2avic 20:v_spec_ctrl 25:vnmi",
    },
    // Word 16.
    CpuinfoFlagWord {
        leaf: 0x0000_0007,
        subleaf: 0,
        register: Register::Ecx,
        names: "1:avx512vbmi 2:umip 3:pku 4:ospke 5:waitpkg 6:avx512_vbmi2 8:gfni 9:vaes \
            10:vpclmulqdq 11:avx512_vnni 12:avx512_bitalg 13:tme 14:avx512_vpopcntdq \
            16:la57 22:rdpid 24:bus_lock_detect 25:cldemote 27:movdiri 28:movdir64b \
            29:enqcmd 30:sgx_lc",
    },
    // Word 18.
    CpuinfoFlagWord {
        leaf: 0x0000_0007,
        subleaf: 0,
        register: Register::Edx,
        names: "2:avx512_4vnniw 3:avx512_4fmaps 4:fsrm 8:avx512_vp2intersect 10:md_clear \
            14:serialize 16:tsxldtrk 18:pconfig 19:arch_lbr 20:ibt 22:amx_bf16 \
            23:avx512_fp16 24:amx_tile 25:amx_int8 28:flush_l1d 29:arch_capabilities",
    },
    // Word 19.
    CpuinfoFlagWord {
        leaf: 0x8000_001f,
        subleaf: 0,
        register: Register::Eax,
        names: "0:sme 1:sev 3:sev_es 4:sev_snp 14:debug_swap 28:svsm",
    },
];

/// Linux's names for leaf 0x80000007 EDX bits in `power management`
/// (arch/x86/kernel/cpu/powerflags.c). An empty name is printed as nothing; a
/// bit past the table is printed as `[bit]`.
const POWER_FLAGS: &[&str] = &[
    "ts",
    "fid",
    "vid",
    "ttp",
    "tm",
    "stc",
    "100mhzsteps",
    "hwpstate",
    "",
    "cpb",
    "eff_freq_ro",
    "proc_feedback",
    "acc_power",
];

/// Whether ECX selects an entry for a basic leaf represented by [`CPUIDS`].
fn uses_subleaf(leaf: u32) -> bool {
    matches!(leaf, 0x04 | 0x07 | 0x0b | 0x0d | 0x0f | 0x10 | 0x12 | 0x14)
}

const fn cpuid_result(eax: u32, ebx: u32, ecx: u32, edx: u32) -> CpuIdResult {
    CpuIdResult { eax, ebx, ecx, edx }
}

// CPUID output from older CPU (broadwell?), with some features like RDRAND
// masked off to prevent non-determinism.
const CPUIDS: &[CpuIdResult] = &[
    cpuid_result(0x0000000D, 0x756E6547, 0x6C65746E, 0x49656E69),
    cpuid_result(
        0x00000663,
        0x00000800,
        // These flags are required for x86-64-v2 support. Found via https://www.felixcloutier.com/x86/cpuid
        0x90202001 | (1 << 0) | (1 << 9) | (1 << 13) | (1 << 19) | (1 << 20) | (1 << 23),
        0x078BFBFD,
    ),
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

    fn registers(result: CpuIdResult) -> [u32; 4] {
        [result.eax, result.ebx, result.ecx, result.edx]
    }

    #[test]
    fn cpuinfo_describes_the_virtual_cpu() {
        // Each line is derived from the table above as Linux derives it; see
        // `InterceptedCpuid::cpuinfo`. The flags include `avx` without
        // `xsave` and `hle`/`rtm` because the table advertises them
        // (https://github.com/rrnewton/hermit/issues/4251,
        // https://github.com/rrnewton/hermit/issues/4252); Linux shows them too.
        let expected = "processor\t: 0\n\
            vendor_id\t: GenuineIntel\n\
            cpu family\t: 6\n\
            model\t\t: 6\n\
            model name\t: QEMU Virtual CPU version 2.5+\n\
            stepping\t: 3\n\
            cpu MHz\t\t: 1000.000\n\
            cache size\t: 16384 KB\n\
            physical id\t: 0\n\
            siblings\t: 1\n\
            core id\t\t: 0\n\
            cpu cores\t: 1\n\
            apicid\t\t: 0\n\
            initial apicid\t: 0\n\
            fpu\t\t: yes\n\
            fpu_exception\t: yes\n\
            cpuid level\t: 13\n\
            wp\t\t: yes\n\
            flags\t\t: fpu de pse tsc msr pae mce cx8 apic sep mtrr pge mca cmov pse36 \
            clflush mmx fxsr sse sse2 syscall nx lm pni ssse3 cx16 sse4_1 sse4_2 x2apic popcnt \
            avx hypervisor lahf_lm fsgsbase bmi1 hle avx2 smep bmi2 erms invpcid rtm adx smap\n\
            bugs\t\t: cpu_meltdown spectre_v1 spectre_v2 spec_store_bypass l1tf mds swapgs taa \
            itlb_multihit bhi spectre_v2_user its\n\
            bogomips\t: 2000.00\n\
            clflush size\t: 64\n\
            cache_alignment\t: 64\n\
            address sizes\t: 40 bits physical, 48 bits virtual\n\
            power management:\n\n";
        assert_eq!(
            String::from_utf8(InterceptedCpuid::new().cpuinfo()).unwrap(),
            expected
        );
    }

    /// `render_cpuinfo` for a CPU whose CPUID answers with `entries`
    /// (`(leaf, subleaf, [eax, ebx, ecx, edx])`) and zero elsewhere, and the
    /// value of each line it renders.
    fn render(entries: &[(u32, u32, [u32; 4])]) -> std::collections::BTreeMap<String, String> {
        let block = render_cpuinfo(|leaf, subleaf| {
            entries
                .iter()
                .find(|(l, s, _)| *l == leaf && *s == subleaf)
                .map_or(cpuid_result(0, 0, 0, 0), |(_, _, [a, b, c, d])| {
                    cpuid_result(*a, *b, *c, *d)
                })
        });
        String::from_utf8(block)
            .unwrap()
            .lines()
            .filter_map(|line| line.split_once(':'))
            .map(|(key, value)| (key.trim().to_owned(), value.trim().to_owned()))
            .collect()
    }

    // "GenuineIntel" and "AuthenticAMD" as leaf 0's EBX, ECX and EDX.
    const INTEL: [u32; 3] = [0x756e_6547, 0x6c65_746e, 0x4965_6e69];
    const AMD: [u32; 3] = [0x6874_7541, 0x444d_4163, 0x6974_6e65];

    #[test]
    fn cpuinfo_follows_linux_on_other_cpus() {
        const PAT: u32 = 1 << 16;
        const CLFLUSH: u32 = 1 << 19;
        // An Intel family 6 model 0x9e whose leaf 4 lists a 32 KB L1d, a 1 MB
        // L2 and an 8 MB L3: the L3 is the size, Linux computes no TLB size,
        // and PAT stays (the erratum covers models 0x01-0x0e only).
        let lines = render(&[
            (0, 0, [4, INTEL[0], INTEL[1], INTEL[2]]),
            (1, 0, [0x0009_06ea, 0x0800, 0, PAT | CLFLUSH]),
            (4, 0, [1 | 1 << 5, 63 | 7 << 22, 63, 0]),
            (4, 1, [3 | 2 << 5, 63 | 15 << 22, 1023, 0]),
            (4, 2, [3 | 3 << 5, 63 | 15 << 22, 8191, 0]),
            (0x8000_0000, 0, [0x8000_0008, 0, 0, 0]),
            (0x8000_0006, 0, [0, 0x4200_4200, 0x0200_0000, 0]),
            (0x8000_0008, 0, [0x3027, 0, 0, 0]),
        ]);
        assert_eq!(lines["model"], "158");
        assert_eq!(lines["cache size"], "8192 KB");
        assert!(!lines.contains_key("TLB size"));
        assert_eq!(lines["flags"], "pat clflush");
        assert_eq!(lines["clflush size"], "64");
        assert_eq!(lines["address sizes"], "39 bits physical, 48 bits virtual");

        // An AMD family 0x19 without brand leaves or leaf 0x80000008: the
        // name is family/model in hex, the sizes come from leaf 0x80000006,
        // and the clflush size is 64 whatever leaf 1 says.
        let lines = render(&[
            (0, 0, [1, AMD[0], AMD[1], AMD[2]]),
            (1, 0, [0x00a0_0f11, 0, 0, CLFLUSH]),
            (0x8000_0000, 0, [0x8000_0006, 0, 0, 0]),
            (0x8000_0006, 0, [0, 0x4200_4200, 0x0200_0000, 0]),
        ]);
        assert_eq!(lines["model name"], "19/01");
        assert_eq!(lines["cache size"], "512 KB");
        assert_eq!(lines["TLB size"], "1024 4K pages");
        assert_eq!(lines["clflush size"], "64");
        assert_eq!(lines["address sizes"], "36 bits physical, 48 bits virtual");

        // An Intel family 15 whose leaf 2 has a bogus iteration count, so its
        // descriptors are ignored and, with no L2, the extended leaves decide;
        // a zero leaf 1 clflush size is 32, and family 15 doubles the
        // alignment.
        let lines = render(&[
            (0, 0, [2, INTEL[0], INTEL[1], INTEL[2]]),
            (1, 0, [0x0f29, 0, 0, CLFLUSH]),
            (2, 0, [0x0000_7d02, 0, 0, 0]),
            (0x8000_0000, 0, [0x8000_0008, 0, 0, 0]),
            (0x8000_0006, 0, [0, 0x0040_0040, 0x0100_0000, 0]),
            (0x8000_0008, 0, [0x3024, 0, 0, 0]),
        ]);
        assert_eq!(lines["cache size"], "256 KB");
        assert_eq!(lines["TLB size"], "128 4K pages");
        assert_eq!(lines["clflush size"], "32");
        assert_eq!(lines["cache_alignment"], "64");
    }

    #[test]
    fn cpuinfo_flag_words_are_well_formed() {
        for word in CPUINFO_FLAGS {
            let bits: Vec<u32> = word.names().map(|(bit, _)| bit).collect();
            assert!(!bits.is_empty(), "leaf {:#x} names no bit", word.leaf);
            assert!(
                bits.windows(2).all(|pair| pair[0] < pair[1]) && bits[bits.len() - 1] < 32,
                "leaf {:#x}: bits must ascend below 32: {bits:?}",
                word.leaf
            );
        }
    }

    #[test]
    fn cpuid_leaf_count() {
        assert!(CPUIDS[0].eax as usize <= CPUIDS.len());
        assert_eq!(
            1 + (EXTENDED_CPUIDS[0].eax as usize & !0x80000000usize),
            EXTENDED_CPUIDS.len()
        );
    }

    #[test]
    fn indexed_leaves_do_not_alias_subleaf_zero() {
        let cpuid = InterceptedCpuid::new();

        assert_eq!(registers(cpuid.cpuid(0x07, 1).unwrap()), [0; 4]);
        for leaf in [0x04, 0x0b, 0x0d, 0x0f, 0x10, 0x12, 0x14] {
            assert_eq!(
                registers(cpuid.cpuid(leaf, 1).unwrap()),
                [0; 4],
                "leaf {leaf:#x}"
            );
        }
        assert_eq!(
            registers(cpuid.cpuid(0x07, 0).unwrap()),
            [0, 0x0018_0fb9, 0, 0]
        );
        assert_eq!(registers(cpuid.cpuid(0x0b, 0).unwrap()), [0, 1, 0x100, 1]);
    }

    #[test]
    fn nonindexed_leaves_ignore_subleaf() {
        let cpuid = InterceptedCpuid::new();

        for leaf in (0..CPUIDS.len() as u32).filter(|leaf| !uses_subleaf(*leaf)) {
            assert_eq!(
                registers(cpuid.cpuid(leaf, 0).unwrap()),
                registers(cpuid.cpuid(leaf, u32::MAX).unwrap()),
                "basic leaf {leaf:#x}"
            );
        }
        for offset in 0..EXTENDED_CPUIDS.len() as u32 {
            let leaf = 0x8000_0000 + offset;
            assert_eq!(
                registers(cpuid.cpuid(leaf, 0).unwrap()),
                registers(cpuid.cpuid(leaf, u32::MAX).unwrap()),
                "extended leaf {leaf:#x}"
            );
        }
    }
}
