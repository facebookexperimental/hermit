// Copyright (c) Meta Platforms, Inc. and affiliates.
// All rights reserved.
// Licensed under the BSD-style license in the LICENSE file.

//! Exercise the ordinary ptrace CLI with CPUID as the literal ELF entry opcode.
#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use goblin::elf::Elf;
use goblin::elf::header::EM_X86_64;
use goblin::elf::header::ET_EXEC;
use goblin::elf::program_header::PF_X;
use goblin::elf::program_header::PT_INTERP;
use goblin::elf::program_header::PT_LOAD;

use super::kvm_cancellation::bounded_command_with_timeout;
use super::kvm_cancellation::bounded_read;

// Reuse the reviewed loader sentinel rather than maintaining another guest.
const SOURCE: &str = include_str!("../../../reverie/reverie-loader/tests/fixtures/guest_cpuid.rs");
const LINKER: &str = include_str!("../../../reverie/reverie-loader/tests/fixtures/guest_static.ld");

fn guest() -> PathBuf {
    let root = super::process_build_root("ptrace-first-instruction-cpuid");
    fs::create_dir_all(&root).expect("create CPUID fixture build root");
    let root = root
        .canonicalize()
        .expect("absolute CPUID fixture build root");
    let source = root.join("guest_cpuid.rs");
    let linker = root.join("guest_static.ld");
    fs::write(&source, SOURCE).expect("retain exact landed CPUID sentinel");
    fs::write(&linker, LINKER).expect("retain exact landed static linker script");
    let image = root.join("guest_cpuid");
    let compile = root.join("compile");
    let status = bounded_command_with_timeout(
        Command::new(std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()))
            .args([
                "--edition=2024",
                "--crate-type=bin",
                "-O",
                "-C",
                "panic=abort",
                "-C",
                "relocation-model=static",
                "-C",
                "code-model=large",
                "-C",
                "no-redzone=yes",
                "-C",
                "link-arg=-nostartfiles",
                "-C",
                "link-arg=-nostdlib",
                "-C",
                "link-arg=-static",
                "-C",
                "link-arg=-Wl,--build-id=none",
                "-C",
            ])
            .arg(format!("link-arg=-T{}", linker.display()))
            .arg("-o")
            .arg(&image)
            .arg(&source),
        &compile,
        Duration::from_secs(20),
    );
    assert!(
        status.success(),
        "CPUID fixture compilation failed: {}",
        String::from_utf8_lossy(&bounded_read(&compile.join("stderr"), 16 * 1024 * 1024))
    );

    let bytes = fs::read(&image).expect("read compiled CPUID sentinel");
    let elf = Elf::parse(&bytes).expect("CPUID sentinel must be ELF");
    assert!(elf.is_64 && elf.little_endian);
    assert_eq!(elf.header.e_type, ET_EXEC);
    assert_eq!(elf.header.e_machine, EM_X86_64);
    assert!(
        elf.program_headers
            .iter()
            .all(|header| header.p_type != PT_INTERP)
    );
    let segment = elf
        .program_headers
        .iter()
        .find(|header| {
            header.p_type == PT_LOAD
                && header.p_flags & PF_X != 0
                && elf
                    .entry
                    .checked_sub(header.p_vaddr)
                    .and_then(|displacement| displacement.checked_add(2))
                    .is_some_and(|end| end <= header.p_filesz)
        })
        .expect("entry opcode must be in a file-backed executable PT_LOAD");
    let offset = usize::try_from(
        segment
            .p_offset
            .checked_add(elf.entry - segment.p_vaddr)
            .expect("ELF entry offset must not overflow"),
    )
    .unwrap();
    assert_eq!(
        bytes.get(offset..offset + 2).expect("entry opcode bytes"),
        [0x0f, 0xa2],
        "CPUID must be the literal first opcode; no interpreter or leaf-selection prologue"
    );
    eprintln!(
        "ptrace CPUID fixture {}: ELF entry {:#x}",
        image.display(),
        elf.entry
    );
    image
}

#[test]
fn ptrace_intercepts_cpuid_at_the_first_image_instruction() {
    let guest = guest();
    let args = [
        "--backend=ptrace",
        "--log=trace",
        "run",
        "--strict",
        "--base-env=minimal",
        "--",
        guest.to_str().expect("absolute guest path is UTF-8"),
    ];
    // The standard helper pins the canonical test epoch and preserves the
    // pinned-root /test execution contract; the guest uses its absolute path.
    let output = super::hermit(&args);
    super::assert_success(&output, &args);
    let mut expected = Vec::new();
    // Detcore's fixed leaf 0, in the fixture's RAX/RBX/RCX/RDX u64 slots.
    for register in [0xdu64, 0x756e_6547, 0x6c65_746e, 0x4965_6e69] {
        expected.extend_from_slice(&register.to_le_bytes());
    }
    expected.extend_from_slice(b"FIRST_CPUID_OK\n");
    assert_eq!(output.stdout, expected);
    assert_eq!(
        super::stderr(&output)
            .matches("handle_cpuid_event: eax: 0, ecx: 0")
            .count(),
        1,
        "the original kernel-entry leaf/subleaf must reach Detcore exactly once"
    );
}
