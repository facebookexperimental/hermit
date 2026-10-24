/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Whether Detcore renders `/proc/cpuinfo` is a fact of each run, not of the
//! process that hosts it (https://github.com/rrnewton/hermit/issues/4213,
//! Codex review): Detcore renders the file exactly where it answers the
//! guest's CPUID instruction from its own table. Runs with CPUID virtualized
//! and not, one after the other in this process and two at once, each see
//! their own answer.

use std::fs::File;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;

use detcore::Config;
use detcore::Detcore;
use reverie::ExitStatus;

/// The brand string Detcore's CPUID table answers (`cpuid::InterceptedCpuid`).
const VIRTUAL_BRAND: &str = "QEMU Virtual CPU version 2.5+";

fn config(virtualize_cpuid: bool) -> Config {
    Config {
        sequentialize_threads: true,
        max_timeslice: None,
        virtualize_metadata: true,
        virtualize_cpuid,
        ..Default::default()
    }
}

/// The brand string the guest's CPUID instruction reports.
fn cpuid_brand() -> String {
    let bytes: Vec<u8> = (0x8000_0002u32..=0x8000_0004)
        .map(std::arch::x86_64::__cpuid)
        .flat_map(|r| [r.eax, r.ebx, r.ecx, r.edx])
        .flat_map(u32::to_le_bytes)
        .take_while(|byte| *byte != 0)
        .collect();
    String::from_utf8_lossy(&bytes).trim().to_owned()
}

/// The guest: /proc/cpuinfo is Detcore's rendering exactly when its CPUID
/// instruction is answered from Detcore's table. A rendered file is a
/// seq_file whose `SEEK_END` Linux refuses (EINVAL) and whose model name is
/// the table's; the kernel's file, read through Detcore's snapshot, seeks to
/// its end. Prints which it saw.
fn guest(expect_virtual: bool) {
    let virtual_cpuid = cpuid_brand() == VIRTUAL_BRAND;
    let mut file = File::open("/proc/cpuinfo").unwrap();
    let mut text = String::new();
    file.read_to_string(&mut text).unwrap();
    let rendered = file.seek(SeekFrom::End(0)).is_err();
    let names_virtual = text.contains(&format!("model name\t: {VIRTUAL_BRAND}\n"));
    assert_eq!(
        rendered, virtual_cpuid,
        "/proc/cpuinfo rendered: {rendered}; CPUID answered by Detcore: {virtual_cpuid}"
    );
    assert_eq!(names_virtual, virtual_cpuid, "{text}");
    // Each run's CPUID is its own configuration's, whatever another run in
    // this process does (the tests below require CPUID faulting).
    assert_eq!(
        virtual_cpuid, expect_virtual,
        "CPUID answered by Detcore: {virtual_cpuid}; this run virtualizes it: {expect_virtual}"
    );
}

/// Whether this host can intercept CPUID, without which no run here answers
/// CPUID from Detcore's table and the tests below would check nothing. As
/// `rdrand_rdseed_is_masked`: a named skip only where CI cannot choose a host
/// with CPUID faulting, and a failure elsewhere.
fn requires_cpuid_faulting(test: &str) -> bool {
    if super::cpuid_faulting_supported() {
        return true;
    }
    if cfg!(fbcode_build) {
        eprintln!("SKIP: {test}: CPUID faulting is unavailable on this host");
        return false;
    }
    panic!("ERROR: {test} requires host CPUID faulting support");
}

fn run(virtualize_cpuid: bool) {
    let (output, _) = detcore_testutils::test_fn_with_config::<Detcore, _>(
        move || guest(virtualize_cpuid),
        detcore_testutils::prepare_ptrace_timer_config(config(virtualize_cpuid)),
        true,
    )
    .unwrap();
    assert_eq!(
        output.status,
        ExitStatus::Exited(0),
        "virtualize_cpuid {virtualize_cpuid}: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// One after the other in this process, in both orders: each run renders
/// /proc/cpuinfo exactly where its own CPUID is Detcore's.
#[test]
fn sequential_runs_each_render_cpuinfo_by_their_own_configuration() {
    if !requires_cpuid_faulting("sequential_runs_each_render_cpuinfo_by_their_own_configuration") {
        return;
    }
    run(true);
    run(false);
    run(true);
}

/// Two runs at once in this process, one virtualizing CPUID and one not,
/// each see their own answer.
#[test]
fn concurrent_runs_each_render_cpuinfo_by_their_own_configuration() {
    if !requires_cpuid_faulting("concurrent_runs_each_render_cpuinfo_by_their_own_configuration") {
        return;
    }
    std::thread::scope(|scope| {
        let virtualized = scope.spawn(|| run(true));
        let host = scope.spawn(|| run(false));
        virtualized.join().unwrap();
        host.join().unwrap();
    });
}
