/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Native first-instruction contract of the actual Detcore runtime.
//!
//! Build the runtime explicitly with `cargo rustc -p detcore-dbt --lib
//! --crate-type cdylib -- --cfg test`, using the normal profile and target.
//! Link the native client to that retained test DSO. The two path variables
//! below select test inputs; the runtime has no observer environment switch.

use std::collections::BTreeMap;
use std::fs;
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::ExitStatus;
use std::process::Stdio;
use std::time::Duration;
use std::time::Instant;

use serde_json::Value;
use serde_json::json;

fn input_path(name: &str) -> PathBuf {
    let path = PathBuf::from(std::env::var_os(name).unwrap_or_else(|| panic!("missing {name}")));
    assert!(path.is_file(), "{name} is not a file: {}", path.display());
    path
}

fn run(root: &Path, name: &str, command: &mut Command, seconds: u64) -> ExitStatus {
    fs::write(
        root.join(format!("{name}.command")),
        format!("{command:?}\n"),
    )
    .unwrap();
    command
        .stdin(Stdio::null())
        .stdout(fs::File::create(root.join(format!("{name}.stdout"))).unwrap())
        .stderr(fs::File::create(root.join(format!("{name}.stderr"))).unwrap());
    // A failed admission can stall before the guest can process a signal.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn().unwrap();
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            fs::write(root.join(format!("{name}.status")), format!("{status}\n")).unwrap();
            return status;
        }
        if started.elapsed() >= Duration::from_secs(seconds) {
            unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
            let _ = child.wait();
            panic!("{name} exceeded {seconds}s; evidence at {}", root.display());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn checked(root: &Path, name: &str, command: &mut Command) -> String {
    assert!(
        run(root, name, command, 30).success(),
        "{name}: {}",
        root.display()
    );
    fs::read_to_string(root.join(format!("{name}.stdout"))).unwrap()
}

fn word(data: &[u8], at: usize, size: usize) -> u64 {
    assert!(matches!(size, 2 | 4 | 8));
    let mut bytes = [0; 8];
    bytes[..size].copy_from_slice(&data[at..at + size]);
    u64::from_le_bytes(bytes)
}

fn elf(data: &[u8], kind: u64) -> (u64, Vec<[u64; 6]>) {
    assert_eq!(&data[..6], b"\x7fELF\x02\x01");
    assert_eq!(word(data, 16, 2), kind);
    assert_eq!(word(data, 18, 2), 62);
    assert_eq!(word(data, 54, 2), 56);
    let mut segments = Vec::new();
    for i in 0..word(data, 56, 2) {
        let at = (word(data, 32, 8) + i * 56) as usize;
        let row = [
            word(data, at, 4),
            word(data, at + 4, 4),
            word(data, at + 8, 8),
            word(data, at + 16, 8),
            word(data, at + 32, 8),
            word(data, at + 40, 8),
        ];
        assert_ne!(row[0], 7, "unexpected TLS segment");
        if row[0] == 0x6474_e551 {
            assert_eq!(row[1] & 1, 0, "executable stack");
        }
        segments.push(row);
    }
    assert_eq!(word(data, 58, 2), 64);
    let mut sections = Vec::new();
    for i in 0..word(data, 60, 2) {
        let at = (word(data, 40, 8) + i * 64) as usize;
        let kind = word(data, at + 4, 4);
        let offset = word(data, at + 24, 8) as usize;
        let size = word(data, at + 32, 8) as usize;
        assert!(!matches!(kind, 4 | 9) || size == 0, "unexpected relocation");
        if kind == 6 {
            for at in (offset..offset + size).step_by(16) {
                assert!(
                    !matches!(word(data, at, 8), 1 | 12 | 13 | 25 | 26 | 32),
                    "dynamic dependency or initializer"
                );
            }
        }
        if kind == 2 || kind == 11 {
            for at in (offset..offset + size).step_by(24) {
                assert_ne!(data[at + 4] & 15, 10, "unexpected IFUNC");
            }
        }
        sections.push((kind, word(data, at + 16, 8), size));
    }
    if kind == 2 {
        assert!(
            sections.contains(&(8, 0x402000, 24)),
            "missing fixed NOBITS witnesses"
        );
    }
    (word(data, 24, 8), segments)
}

fn symbols(root: &Path, name: &str, image: &Path) -> BTreeMap<String, u64> {
    checked(root, name, Command::new("nm").arg("-n").arg(image))
        .lines()
        .filter_map(|line| {
            let fields: Vec<_> = line.split_whitespace().collect();
            (fields.len() == 3).then(|| {
                (
                    fields[2].to_owned(),
                    u64::from_str_radix(fields[0], 16).unwrap(),
                )
            })
        })
        .collect()
}

#[test]
#[ignore = "requires the explicit cfg(test) Detcore cdylib, its linked native client, and DynamoRIO"]
fn root_tool_lifecycle_precedes_the_first_application_instruction() {
    let drrun = input_path("HERMIT_DBT_TEST_DRRUN");
    let client = input_path("HERMIT_DBT_TEST_CLIENT");
    let root =
        PathBuf::from(std::env::var_os("E2E_RESULT_ROOT").expect("retained result directory"))
            .join("first-instruction");
    fs::create_dir(&root).unwrap();
    let guest_dir = PathBuf::from("/test/hermit-first-instruction");
    fs::create_dir(&guest_dir).unwrap();
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/first_instruction");
    let interpreter = guest_dir.join("interpreter");
    let app = guest_dir.join("app");
    let cc = std::env::var_os("CC").unwrap_or_else(|| "cc".into());
    checked(
        &root,
        "interpreter-build",
        Command::new(&cc)
            .args([
                "-fPIC",
                "-shared",
                "-nostdlib",
                "-nostartfiles",
                "-nodefaultlibs",
                "-fno-stack-protector",
                "-Wl,-e,interp_start",
                "-Wl,-Bsymbolic",
                "-Wl,--build-id=none",
                "-Wl,-z,noexecstack",
            ])
            .arg(fixtures.join("interpreter.S"))
            .arg("-o")
            .arg(&interpreter),
    );
    checked(
        &root,
        "app-build",
        Command::new(&cc)
            .args([
                "-static",
                "-no-pie",
                "-fno-pie",
                "-nostdlib",
                "-nostartfiles",
                "-nodefaultlibs",
                "-fno-stack-protector",
                "-Wl,--build-id=none",
                "-Wl,-z,noexecstack",
            ])
            .arg(format!(
                "-Wl,-T,{}",
                fixtures.join("executable.ld").display()
            ))
            .arg(format!(
                "-DAPP_INTERPRETER_PATH=\"{}\"",
                interpreter.display()
            ))
            .arg(fixtures.join("executable.S"))
            .arg("-o")
            .arg(&app),
    );
    let interp_bytes = fs::read(&interpreter).unwrap();
    let app_bytes = fs::read(&app).unwrap();
    let (entry, interp_segments) = elf(&interp_bytes, 3);
    let (app_entry, app_segments) = elf(&app_bytes, 2);
    assert_eq!(app_entry, 0x401000);
    assert!(app_segments.iter().any(|r| r[0] == 1
        && r[1] == 6
        && r[3] == 0x402000
        && r[4] == 0
        && r[5] == 24));
    let interp_path = app_segments.iter().find(|r| r[0] == 3).expect("PT_INTERP");
    let path_bytes =
        &app_bytes[interp_path[2] as usize..(interp_path[2] + interp_path[4]) as usize];
    assert_eq!(
        path_bytes,
        format!("{}\0", interpreter.display()).as_bytes()
    );
    let entry_segment = interp_segments
        .iter()
        .find(|r| r[0] == 1 && r[1] & 1 != 0 && r[3] <= entry && entry < r[3] + r[4])
        .unwrap();
    let entry_offset = (entry_segment[2] + entry - entry_segment[3]) as usize;
    // Literal marker store, untouched SP store, RIP-relative entry LEA, PC store.
    let prefix = [
        0xc7, 0x04, 0x25, 0x00, 0x20, 0x40, 0x00, 0x01, 0x00, 0x00, 0x00, 0x48, 0x89, 0x24, 0x25,
        0x08, 0x20, 0x40, 0x00, 0x48, 0x8d, 0x05, 0xe6, 0xff, 0xff, 0xff, 0x48, 0x89, 0x04, 0x25,
        0x10, 0x20, 0x40, 0x00,
    ];
    assert_eq!(
        &interp_bytes[entry_offset..entry_offset + prefix.len()],
        &prefix
    );
    let names = symbols(&root, "interpreter-symbols", &interpreter);
    assert_eq!(names["interp_start"], entry);
    assert_eq!(names["sample_prefix_after"], names["sample_prefix"] + 2);
    checked(
        &root,
        "interpreter-disassembly",
        Command::new("objdump")
            .args(["--insn-width=16", "-d", "-w"])
            .arg(&interpreter),
    );
    fs::copy(&app, root.join("app")).unwrap();
    fs::copy(&interpreter, root.join("interpreter")).unwrap();
    assert!(
        run(
            &root,
            "native",
            Command::new("setarch")
                .args(["x86_64", "-R"])
                .arg(&app)
                .current_dir(&guest_dir)
                .env_clear()
                .env("LC_ALL", "C")
                .env("TZ", "UTC"),
            30
        )
        .success()
    );
    assert_eq!(fs::read(root.join("native.stdout")).unwrap(), b"");
    assert_eq!(fs::read(root.join("native.stderr")).unwrap(), b"");
    let diagnostic = fs::File::create(root.join("dbt.diagnostic")).unwrap();
    let diagnostic_fd = diagnostic.as_raw_fd();
    assert_eq!(unsafe { libc::fcntl(198, libc::F_GETFD) }, -1);
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::EBADF)
    );
    let config_dir = root.join("dr-config");
    fs::create_dir(&config_dir).unwrap();
    let mut command = Command::new("setarch");
    command
        .args(["x86_64", "-R"])
        .arg(drrun)
        .args([
            "-quiet",
            "-disable_rseq",
            "-vm_max_offset",
            "0x0",
            "-stack_size",
            "2M",
            "-c",
        ])
        .arg(client)
        .args(["-diagnostic_fd", "198", "-summary", "--"])
        .arg(&app)
        .current_dir(&guest_dir)
        .env_clear()
        .env("LC_ALL", "C")
        .env("TZ", "UTC")
        .env("DYNAMORIO_CONFIGDIR", config_dir);
    unsafe {
        command.pre_exec(move || {
            if libc::dup2(diagnostic_fd, 198) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    assert!(
        run(&root, "dbt", &mut command, 60).success(),
        "DBT execution: {}",
        root.display()
    );
    assert_eq!(fs::read(root.join("dbt.stdout")).unwrap(), b"");
    assert_eq!(fs::read(root.join("dbt.stderr")).unwrap(), b"");
    assert_eq!(fs::read(&app).unwrap(), app_bytes);
    assert_eq!(fs::read(&interpreter).unwrap(), interp_bytes);
    let log = fs::read_to_string(root.join("dbt.diagnostic")).unwrap();
    assert!(
        log.contains("reverie-dbt: tool=Detcore"),
        "actual Detcore summary missing"
    );
    let rows: Vec<Value> = log
        .lines()
        .filter_map(|line| line.strip_prefix("FIRST_INSTRUCTION_TEST "))
        .map(|line| serde_json::from_str(line).expect("malformed observation is not a zero row"))
        .collect();
    fs::write(
        root.join("observations.json"),
        serde_json::to_vec_pretty(&rows).unwrap(),
    )
    .unwrap();
    let expected_events = [
        "start-enter",
        "start-complete",
        "post-exec-enter",
        "post-exec-complete",
        "first-syscall",
    ];
    assert_eq!(
        rows.len(),
        expected_events.len(),
        "all lifecycle observations are mandatory"
    );
    for (row, event) in rows.iter().zip(expected_events) {
        assert_eq!(row["event"], event);
        assert_eq!(row["runtime"], "Detcore");
        assert_eq!(row["marker_read"], json!({"Ok":4}));
        assert_eq!(row["saved_sp_read"], json!({"Ok":8}));
        assert_eq!(row["saved_pc_read"], json!({"Ok":8}));
        assert_eq!(row["register_status"], 1);
        let regs = row["registers"].as_array().unwrap();
        assert_eq!(regs.len(), 27);
        assert!(regs.iter().all(|v| v.as_u64().is_some()));
        assert_ne!(regs[19], 0);
    }
    let last = rows.last().unwrap();
    let sp = last["saved_sp"].as_u64().unwrap();
    let pc = last["saved_pc"].as_u64().unwrap();
    assert_ne!(sp, 0);
    assert!(pc > entry && (pc - entry).is_multiple_of(4096));
    let regs = last["registers"].as_array().unwrap();
    for (index, value) in [
        (1, 0x112233445566770c_u64),
        (2, 0x112233445566770b),
        (3, 0x112233445566770a),
        (4, 0x1122334455667705),
        (5, 0x1122334455667701),
        (7, 0x43425201),
        (8, 0x11),
        (9, 3),
        (10, 1),
        (11, 0),
        (12, 0),
        (14, 1),
        (15, 1),
        (18, 0x247),
    ] {
        assert_eq!(regs[index], value, "first-syscall register {index}");
    }
    assert_eq!(regs[0], sp);
    assert_eq!(regs[19], sp);
    assert_eq!(regs[16], pc + names["sample_prefix_after"] - entry);
    let markers: Vec<_> = rows.iter().map(|r| r["marker"].as_u64().unwrap()).collect();
    let pcs_match = rows[..4].iter().all(|r| r["registers"][16] == pc);
    let sps_match = rows[..4].iter().all(|r| r["registers"][19] == sp);
    fs::write(root.join("oracle.json"), serde_json::to_vec_pretty(&json!({
        "expected_markers":[0,0,0,0,1], "actual_markers":markers,
        "guest_saved_entry_pc":pc, "guest_saved_initial_sp":sp,
        "lifecycle_pcs_match":pcs_match, "lifecycle_sps_match":sps_match,
        "scope":"actual native plus Detcore, root only; initial_stack_pointer remains None; no clock/auxv/parity claim"
    })).unwrap()).unwrap();
    assert_eq!(
        markers,
        [0, 0, 0, 0, 1],
        "callbacks must precede the first app instruction; {}",
        root.display()
    );
    assert!(
        pcs_match,
        "lifecycle PC is not the guest-executed entry witness"
    );
    assert!(
        sps_match,
        "lifecycle SP is not the guest-executed initial-SP witness"
    );
}
