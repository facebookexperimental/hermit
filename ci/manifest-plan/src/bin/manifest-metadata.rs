//! Export the current manifest policy as deterministic, typed JSON.

use std::io::BufWriter;
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use hermit_manifest_plan::manifest_metadata::build_export;

const HELP: &str = "Usage: manifest-metadata

Export current E2E manifest policy as JSON.

The JSON contains current test metadata, every comparable cell in the manifest,
and custom commands selected by full validation. Each comparable cell records
whether full validation selects it. It contains no run result or measurement
state. Schema 2 includes both inherited or overridden manifest CPU and wall
limits; execution-time environment multipliers are not applied to this policy.
";

fn main() -> ExitCode {
    match real_main() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("manifest-metadata: REFUSED: {error}");
            ExitCode::from(2)
        }
    }
}

fn real_main() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    if let Some(arg) = args.next() {
        if matches!(arg.as_str(), "-h" | "--help") && args.next().is_none() {
            print!("{HELP}");
            return Ok(());
        }
        return Err(format!("unexpected argument {arg:?}\n\n{HELP}"));
    }

    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .map_err(|error| format!("cannot resolve repository root: {error}"))?;
    let export = build_export(&root)?;
    let stdout = std::io::stdout();
    let mut output = BufWriter::new(stdout.lock());
    serde_json::to_writer(&mut output, &export)
        .map_err(|error| format!("cannot encode manifest metadata: {error}"))?;
    output
        .write_all(b"\n")
        .and_then(|()| output.flush())
        .map_err(|error| format!("cannot write manifest metadata: {error}"))?;
    Ok(())
}
