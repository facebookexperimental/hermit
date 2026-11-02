//! Maintained archive producer only. No final link, image loading or guest launch.
use std::path::{Path, PathBuf};
use std::time::Duration;
use reverie_liteinst_artifact::private_static::{SourceExpectation, VerifiedStaticSource};
use reverie_liteinst_artifact::{Runner, qualifier_source_sha256, write_new};

fn run() -> Result<(), String> {
    let a: Vec<String> = std::env::args().collect();
    if a.len() != 10 {
        return Err("expected H H_HEAD RUNTIME_R_PIN CARGO RUSTC TARGET LOGS TOOL_R_HEAD QUALIFIER_SHA".into());
    }
    let actual = qualifier_source_sha256();
    if actual != a[9] { return Err(format!("compiled qualifier source mismatch: expected={} actual={actual}", a[9])); }
    let expected = SourceExpectation {
        workspace: Path::new(&a[1]), workspace_head: &a[2], reverie_pin: &a[3],
        cargo: Path::new(&a[4]), rustc: Path::new(&a[5]), target_dir: Path::new(&a[6]),
    };
    let logs = PathBuf::from(&a[7]);
    // Per-command ceiling stays inside the existing 900-second outer wall guard.
    let mut runner = Runner::new(logs.clone(), Duration::from_secs(840), 64 * 1024 * 1024)?;
    let body = VerifiedStaticSource::with_resolved(&expected, &mut runner, |source, runner| {
        let archive = source.build_archive(runner)?;
        archive.verify()?;
        Ok(archive)
    });
    // Record commands on resolver/build/close failure as well. No reconstructed
    // metadata or old Cargo stream is supplied to the factory.
    let commands = write_new(&logs.join("commands.debug"), format!("{:#?}\n", runner.commands).as_bytes());
    let archive = match (body, commands) {
        (Ok(value), Ok(())) => value,
        (Err(error), Ok(())) => return Err(error),
        (Ok(_), Err(error)) => return Err(format!("command evidence write failed: {error}")),
        (Err(error), Err(custody)) => return Err(format!("{error}; command evidence write failed: {custody}")),
    };
    archive.verify()?;
    println!("TOOL_R_HEAD={}\nRUNTIME_R_PIN={}\nQUALIFIER_SOURCE_SHA256={actual}", a[8], a[3]);
    println!("ARCHIVE_PATH={}\nARCHIVE_SHA256={}", archive.file().path.display(), archive.file().sha256);
    println!("METADATA_SHA256={}\nCARGO_MESSAGES_SHA256={}", archive.metadata_sha256(), archive.cargo_messages_sha256());
    println!("SOURCE_STATE=ARCHIVE_BUILD_ONLY_NOT_QUALIFIED");
    println!("ROOT_DIAGNOSTIC=NOT_RUN\nQUALIFICATION=NOT_PROVEN\nPRE_TLS_CLOSURE=NOT_PROVEN\nREADINESS=NOT_PROVEN\nGUESTS_EXECUTED=0\nTESTS_EXECUTED=0");
    Ok(())
}
fn main() {
    if let Err(error) = run() {
        eprintln!("STATIC_ARCHIVE_BUILD_REFUSED: {error}");
        std::process::exit(1);
    }
}
