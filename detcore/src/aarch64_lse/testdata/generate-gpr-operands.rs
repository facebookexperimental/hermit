#!/usr/bin/env -S rust-script --force
//! Write `gpr-operands.txt`, the general registers an independent
//! disassembler lists for each of a sample of AArch64 encodings, which
//! `aarch64_lse::mapping`'s reader scan is tested against: an encoding it
//! does not decode in full must treat every register listed as passed on.
//!
//! The sample covers each top-level encoding class (op0, bits 28:25) with
//! words drawn from a fixed-seed generator, and the families whose
//! register operands are easiest to miss with their fields varied: `CASP`,
//! the 128-bit atomics, `LD64B` and `ST64B*`, the RCW atomics, SVE and SME
//! forms taking general registers, the system pair forms, the hints and
//! pointer authentication, and the encodings in `SEEDS`. Words LLVM does not
//! decode are left out.
//!
//! LLVM prints only the first register of an implicit list (`ld64b x0`)
//! and none of a fixed implicit one (`pacia171615`), so the test adds
//! those itself, from its own table; this file records what LLVM prints.
//!
//! Regenerate from the repository root, with an LLVM whose `llvm-mc`
//! knows every extension (`-mattr=+all`):
//!
//! ```text
//! detcore/src/aarch64_lse/testdata/generate-gpr-operands.rs \
//!     > detcore/src/aarch64_lse/testdata/gpr-operands.txt
//! ```
//!
//! `LLVM_MC` names another `llvm-mc`. The file's first lines record the
//! version that wrote it.

#[path = "../../../../scripts/lib/rust_script_prelude.rs"]
mod rust_script_prelude;

use std::collections::BTreeMap;
use std::io::Write;
use std::process::Command;
use std::process::Stdio;

/// Words drawn per top-level encoding class.
const PER_CLASS: usize = 2000;
/// Variants drawn per family.
const PER_FAMILY: usize = 96;

/// The families sampled with their register fields varied, as (fixed bits,
/// their value): each variant keeps the fixed bits and draws the rest.
const FAMILIES: &[(&str, u32, u32)] = &[
    ("CASP", 0xBFA0_7C00, 0x0820_7C00),
    ("CAS", 0x3FA0_7C00, 0x08A0_7C00),
    ("LDXP/STXP", 0xBFA0_0000, 0x8820_0000),
    ("SWPP/LDCLRP/LDSETP, RCW*P", 0xBF20_8C00, 0x1920_8000),
    ("RCWCAS, RCWCASP", 0xBF20_F800, 0x1920_0800),
    ("LD64B/ST64B*, SWP", 0xFFE0_8C00, 0xF820_8000),
    ("LSE atomics", 0x3B20_0C00, 0x3820_0000),
    ("RCW atomics", 0xBF20_8C00, 0x3820_8000),
    ("STGP/LDPSW", 0x7E00_0000, 0x6800_0000),
    ("LDRAA/LDRAB", 0xFF20_0400, 0xF820_0400),
    ("CPY*/SET*", 0x3F20_0C00, 0x1900_0400),
    ("SVE DUP/CPY/INSR (scalar)", 0xFF20_0000, 0x0520_0000),
    ("SVE INDEX/ADDVL/INC (scalar)", 0xFF20_0000, 0x0420_0000),
    ("SVE WHILE", 0xFF20_0000, 0x2520_0000),
    ("SME", 0xFE00_0000, 0xC000_0000),
    ("SME loads and stores", 0xFE00_0000, 0xE000_0000),
    ("SYSP/MSRR/MRRS", 0xFFC0_0000, 0xD540_0000),
    ("SYS/SYSL", 0xFFD8_0000, 0xD508_0000),
    ("MSR/MRS", 0xFFD0_0000, 0xD510_0000),
    ("hints", 0xFFFF_F01F, 0xD503_201F),
    ("pointer authentication", 0xFFFF_0000, 0xDAC1_0000),
    ("compare and branch", 0x7C00_0000, 0x7400_0000),
    ("SIMD and FP moves", 0x7F20_FC00, 0x1E20_0000),
    ("SIMD DUP/INS/UMOV", 0xBFE0_8400, 0x0E00_0400),
];

/// Encodings sampled as they are: those the reviews of the reader scan
/// named (SVE `DUP`, `INDEX`, `CPY` and `INSR` from a general register,
/// `CASP` in its four orderings, `SWPP`, `LDCLRP` and `LDSETP`, `LD64B`,
/// `ST64B`, `ST64BV` and `ST64BV0`), and one of each other family a test
/// names.
const SEEDS: &[u32] = &[
    0x05e0_3801, 0x04e1_4401, 0x05e8_a001, 0x05e4_3801, 0x0421_5022, 0x04f0_e3e1,
    0x4860_7c82, 0x4820_7c82, 0x4820_fc82, 0x4860_fc82, 0x1926_8104, 0x1926_1104,
    0x1926_3104, 0xf83f_d100, 0xf83f_9100, 0xf82a_b100, 0xf82a_a100, 0x3822_a023,
    0x6900_8c22, 0xe100_0020, 0xf402_0041, 0xd503_251f, 0xdac1_8bfe, 0xd503_233f,
    0xd503_211f, 0xd548_8000, 0xd558_2000, 0x4e08_0c20, 0x9e67_0020, 0xd50b_7421,
    0xd50b_7a21, 0xb348_3c41, 0xfa42_0021,
];

/// xorshift64*, from a fixed seed, so a run with the same LLVM writes the
/// same file.
struct Draw(u64);

impl Draw {
    fn next(&mut self) -> u32 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        (self.0.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 32) as u32
    }
}

fn main() {
    rust_script_prelude::init();
    let llvm_mc = std::env::var("LLVM_MC").unwrap_or_else(|_| "llvm-mc".to_owned());
    let mut draw = Draw(0x4865_726D_6974_4C53);
    let mut words = SEEDS.to_vec();
    for class in 0..16u32 {
        for _ in 0..PER_CLASS {
            words.push((draw.next() & !(0xF << 25)) | class << 25);
        }
    }
    for &(_, fixed, value) in FAMILIES {
        for _ in 0..PER_FAMILY {
            words.push((draw.next() & !fixed) | value);
        }
    }
    let input: String = words
        .iter()
        .map(|word| {
            let b = word.to_le_bytes();
            format!("{:#04x},{:#04x},{:#04x},{:#04x}\n", b[0], b[1], b[2], b[3])
        })
        .collect();
    let mut child = Command::new(&llvm_mc)
        .args(["--disassemble", "-triple=aarch64", "-mattr=+all", "--show-encoding"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap_or_else(|error| panic!("cannot run {llvm_mc}: {error}"));
    let mut stdin = child.stdin.take().expect("llvm-mc's stdin");
    let writer = std::thread::spawn(move || stdin.write_all(input.as_bytes()));
    let output = child.wait_with_output().expect("llvm-mc's output");
    writer.join().expect("the writer").expect("writing to llvm-mc");
    let text = String::from_utf8(output.stdout).expect("llvm-mc writes UTF-8");
    let mut listed: BTreeMap<u32, (Vec<u32>, String)> = BTreeMap::new();
    for line in text.lines() {
        let Some((assembly, encoding)) = line.split_once("// encoding: [") else {
            continue;
        };
        let bytes: Vec<u8> = encoding
            .trim_end_matches(']')
            .split(',')
            .filter_map(|byte| u8::from_str_radix(byte.trim().trim_start_matches("0x"), 16).ok())
            .collect();
        let Ok(bytes) = <[u8; 4]>::try_from(bytes) else {
            continue;
        };
        let assembly = assembly.split_whitespace().collect::<Vec<_>>().join(" ");
        listed.insert(u32::from_le_bytes(bytes), (registers(&assembly), assembly));
    }
    let version = Command::new(&llvm_mc)
        .arg("--version")
        .output()
        .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
        .unwrap_or_default();
    let version = version
        .lines()
        .find(|line| line.contains("version"))
        .unwrap_or("unknown")
        .trim();
    println!("# General registers LLVM lists for each AArch64 encoding sampled; written by");
    println!("# generate-gpr-operands.rs with {version}, -mattr=+all.");
    println!("# Each line: the encoding, then the registers' numbers (w and x alike).");
    for (word, (registers, _)) in &listed {
        let registers: Vec<String> = registers.iter().map(u32::to_string).collect();
        println!("{word:08x} {}", registers.join(","));
    }
}

/// The general registers `assembly` names, x and w alike, without the zero
/// register or the stack pointer; a range `x0-x7` names each between.
fn registers(assembly: &str) -> Vec<u32> {
    let mut found = Vec::new();
    let bytes = assembly.as_bytes();
    let mut at = 0;
    while at < bytes.len() {
        let starts = (bytes[at] == b'x' || bytes[at] == b'w')
            && (at == 0 || !bytes[at - 1].is_ascii_alphanumeric());
        let digits = bytes[at + 1..].iter().take_while(|b| b.is_ascii_digit()).count();
        let ends = at + 1 + digits;
        if starts && digits > 0 && (ends == bytes.len() || !bytes[ends].is_ascii_alphanumeric()) {
            let number: u32 = assembly[at + 1..ends].parse().expect("digits");
            let last = (bytes.get(ends) == Some(&b'-'))
                .then(|| registers(&assembly[ends + 1..]).first().copied())
                .flatten()
                .filter(|last| *last > number);
            for register in number..=last.unwrap_or(number) {
                if register < 31 && !found.contains(&register) {
                    found.push(register);
                }
            }
            at = ends;
        } else {
            at += 1;
        }
    }
    found.sort_unstable();
    found
}
