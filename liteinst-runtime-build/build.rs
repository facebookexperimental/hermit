use std::env;
use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use reverie_liteinst_artifact as qualifier;
use serde_json::Value;

mod artifact;

fn constructor_evidence_directory(out_dir: &Path) -> PathBuf {
    let serial = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("host clock precedes Unix epoch")
        .as_nanos();
    let directory = out_dir.join(format!(
        "constructor-qualification-{}-{serial}",
        std::process::id()
    ));
    fs::create_dir(&directory).expect("failed to create fresh constructor evidence directory");
    directory
}

fn copy_into_protected_stage(source: &Path, destination: &Path) -> io::Result<()> {
    let mut source_file = File::open(source)?;
    let source_permissions = source_file.metadata()?.permissions();
    let mut destination_file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)?;
    if !destination_file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "LiteInst stage is not a regular file",
        ));
    }
    io::copy(&mut source_file, &mut destination_file)?;
    destination_file.set_permissions(source_permissions)?;
    destination_file.sync_all()
}

fn main() {
    println!("cargo:rerun-if-env-changed=HERMIT_LITEINST_STAGE");
    println!("cargo:rerun-if-env-changed=HERMIT_LITEINST_REVERIE_PIN");
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_ALLOCATOR_FIXTURE");
    println!("cargo:rerun-if-env-changed=PROFILE");
    println!("cargo:rerun-if-changed=Cargo.lock");
    println!("cargo:rerun-if-changed=artifact.rs");
    println!("cargo:rerun-if-changed=runtime/Cargo.toml");
    println!("cargo:rerun-if-changed=runtime/src/lib.rs");
    let allocator_fixture = env::var_os("CARGO_FEATURE_ALLOCATOR_FIXTURE").is_some();
    let destination = PathBuf::from(
        env::var_os("HERMIT_LITEINST_STAGE")
            .expect("HERMIT_LITEINST_STAGE must name a unique runtime output path"),
    );
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo did not set OUT_DIR"));
    // Derive the nested runtime build's `--profile` from cargo's `PROFILE`
    // build-script env var: it is `release` for release-like profiles and
    // `debug` otherwise, which map to the `--profile` values `release` and
    // `dev`. The previous approach walked `out_dir.ancestors().nth(3)` to guess
    // the profile directory name; a cargo nightly changed the build-script
    // OUT_DIR layout so that ancestor now resolves to the literal `build`
    // directory, and passing `--profile build` fails with
    // "error: profile name `build` is reserved". Sourcing the profile from the
    // documented env var is robust against that layout drift.
    let profile = match env::var("PROFILE").as_deref() {
        Ok("debug") => "dev",
        Ok("release") => "release",
        Ok(other) => panic!("unexpected Cargo PROFILE {other:?}; expected debug or release"),
        Err(error) => panic!("Cargo did not set PROFILE: {error}"),
    };
    let manifest_dir = PathBuf::from(
        env::var_os("CARGO_MANIFEST_DIR").expect("Cargo did not set CARGO_MANIFEST_DIR"),
    );
    let trusted_manifest_dir = fs::canonicalize(env!("CARGO_MANIFEST_DIR"))
        .expect("failed to identify the compiled nested workspace");
    let manifest_dir =
        fs::canonicalize(&manifest_dir).expect("failed to identify the current nested workspace");
    assert_eq!(
        manifest_dir, trusted_manifest_dir,
        "current nested workspace differs from the compiled source root"
    );
    let source_root = trusted_manifest_dir
        .parent()
        .expect("nested workspace has no trusted Hermit root");
    let source_manifest = source_root.join("liteinst-runtime-build/Cargo.toml");
    let requested_pin = env::var("HERMIT_LITEINST_REVERIE_PIN")
        .expect("HERMIT_LITEINST_REVERIE_PIN must supply the independently expected full revision");
    assert!(
        requested_pin.len() == 40
            && requested_pin
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "HERMIT_LITEINST_REVERIE_PIN must be a full lowercase 40-hex revision"
    );
    let evidence = constructor_evidence_directory(&out_dir);
    let mut source_runner = qualifier::Runner::new(
        evidence.join("source-logs"),
        Duration::from_secs(60),
        64 * 1024 * 1024,
    )
    .expect("failed to create bounded compiled-source runner");
    let compiled_sources = qualifier::VerifiedCompiledSources::resolve_content(
        &qualifier::CompiledSourceExpectation {
            layout: qualifier::CompiledSourceLayout::HermitGit,
            image_kind: qualifier::FixedImageKind::Standalone,
            workspace_root: source_root,
            manifest: &source_manifest,
            reverie_pin: &requested_pin,
        },
        &mut source_runner,
    )
    .unwrap_or_else(|error| panic!("unbound standalone compiler sources: {error}"));
    let nested_target = out_dir.join("runtime-target");
    let cargo = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let metadata_output = source_runner
        .run(
            cargo.to_str().expect("Cargo path is not UTF-8"),
            &[
                "metadata".into(),
                "--locked".into(),
                "--offline".into(),
                "--format-version=1".into(),
            ],
            &manifest_dir,
        )
        .expect("failed to resolve the locked standalone preload package");
    let metadata: Value = serde_json::from_slice(&metadata_output)
        .expect("standalone preload metadata was not Cargo JSON");
    let packages: Vec<_> = metadata["packages"]
        .as_array()
        .expect("Cargo metadata has no packages")
        .iter()
        .filter(|package| package["name"] == "reverie-liteinst-preload")
        .collect();
    assert_eq!(
        packages.len(),
        1,
        "expected one resolved standalone preload leaf"
    );
    let package = packages[0];
    let source = package["source"]
        .as_str()
        .expect("preload leaf must have a Git source");
    let revision = source
        .strip_prefix("git+https://github.com/rrnewton/reverie.git?rev=")
        .and_then(|suffix| suffix.split_once('#'))
        .filter(|(pin, commit)| {
            pin == commit
                && pin.len() == 40
                && pin
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        })
        .map(|(pin, _)| pin)
        .expect("preload leaf must resolve the exact full Reverie revision");
    assert_eq!(
        requested_pin, revision,
        "staged revision differs from the resolved leaf"
    );
    let package_id = package["id"]
        .as_str()
        .expect("preload leaf has no package ID");
    let version = package["version"]
        .as_str()
        .expect("preload leaf has no version");
    assert_eq!(
        package_id,
        format!(
            "git+https://github.com/rrnewton/reverie.git?rev={revision}#reverie-liteinst-preload@{version}"
        )
    );
    let package_manifest = PathBuf::from(
        package["manifest_path"]
            .as_str()
            .expect("preload leaf has no manifest"),
    );
    assert_eq!(
        package_id,
        compiled_sources.package().id.as_str(),
        "constructor metadata differs from the independently verified package"
    );
    assert_eq!(
        package_manifest.as_os_str(),
        compiled_sources
            .compiler_manifest_path()
            .expect("compiled source root changed before metadata readback")
            .as_os_str(),
        "constructor metadata differs from the independently verified manifest"
    );
    let qualifier_source =
        artifact::constructor_qualifier_source(&metadata, &package_manifest, revision)
            .unwrap_or_else(|error| panic!("unbound standalone constructor qualifier: {error}"));
    let qualifier_source_identity = qualifier::FileIdentity::read(&qualifier_source)
        .expect("failed to identify the Cargo-resolved constructor proof source");
    assert_eq!(
        qualifier_source_identity.sha256,
        qualifier::qualifier_source_sha256(),
        "compiled constructor qualifier differs from the resolved shared proof source"
    );
    println!("cargo:rerun-if-changed={}", qualifier_source.display());
    let mut command = Command::new(&cargo);
    command
        .args([
            "build",
            "--locked",
            "--manifest-path",
            "Cargo.toml",
            "-p",
            "reverie-liteinst-preload",
            "--lib",
            "--profile",
            profile,
            "--target-dir",
        ])
        .arg(&nested_target)
        .arg("--message-format=json-render-diagnostics")
        .current_dir(&manifest_dir)
        .env_remove("HERMIT_LITEINST_STAGE");
    if allocator_fixture {
        // Enable the exact leaf feature through the selected workspace member.
        // Keep the leaf selected too so current artifact qualification applies.
        command.args([
            "-p",
            "hermit-liteinst-runtime-artifact",
            "--features",
            "allocator-fixture",
        ]);
    }
    let output = command
        .output()
        .expect("failed to invoke Cargo for the isolated LiteInst runtime build");
    if !output.status.success() {
        panic!(
            "isolated LiteInst runtime build failed with {}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
    let messages = String::from_utf8(output.stdout)
        .expect("isolated LiteInst runtime Cargo output was not UTF-8");
    let candidate = artifact::liteinst_cdylib_from_cargo_messages(
        &messages,
        package_id,
        &package_manifest,
        allocator_fixture,
    )
    .unwrap_or_else(|error| panic!("failed to parse isolated Cargo output: {error}"));
    assert!(
        candidate.starts_with(&nested_target),
        "current preload output is outside the isolated target root"
    );
    // Both feature modes require the same complete constructor proof. The
    // diagnostic producer separately retains its mandatory M1 route checks.
    qualifier::write_new(&evidence.join("cargo-metadata.json"), &metadata_output)
        .expect("failed to retain constructor package metadata");
    qualifier::write_new(&evidence.join("cargo-build.stdout"), messages.as_bytes())
        .expect("failed to retain current runtime Cargo artifact evidence");
    qualifier::write_new(&evidence.join("cargo-build.stderr"), &output.stderr)
        .expect("failed to retain runtime Cargo diagnostics");
    let mut runner = qualifier::Runner::new(
        evidence.join("logs"),
        Duration::from_secs(60),
        64 * 1024 * 1024,
    )
    .expect("failed to create bounded constructor proof runner");
    compiled_sources
        .verify_unchanged(&mut runner)
        .expect("compiled sources changed during the runtime build");
    let qualification =
        qualifier::qualify_constructor_with_sources(&candidate, &mut runner, &compiled_sources)
            .unwrap_or_else(|error| panic!("current LiteInst constructor is unqualified: {error}"));
    compiled_sources
        .verify_unchanged(&mut runner)
        .expect("compiled sources changed during constructor qualification");
    assert_eq!(
        qualification.artifact.path,
        fs::canonicalize(&candidate).expect("failed to canonicalize current runtime artifact"),
        "constructor proof identifies a different runtime artifact"
    );
    qualifier::write_new(
        &evidence.join("qualification.json"),
        &serde_json::to_vec_pretty(&serde_json::json!({
            "reverie_revision": revision,
            "preload_package_id": package_id,
            "qualifier_source": qualifier_source_identity,
            "compiled_qualifier_source_sha256": qualifier::qualifier_source_sha256(),
            "profile": profile,
            "allocator_fixture": allocator_fixture,
            "qualification": qualification,
            "compiled_sources": Some(compiled_sources.record()),
            "compiled_source_commands": source_runner.commands,
            "commands": runner.commands,
            "executed_tests": 0,
        }))
        .expect("failed to serialize actual constructor qualification"),
    )
    .expect("failed to retain actual constructor qualification");
    qualifier_source_identity
        .verify()
        .expect("constructor proof source changed during qualification");
    qualification
        .artifact
        .verify()
        .expect("qualified current artifact changed before staging");
    copy_into_protected_stage(&candidate, &destination).unwrap_or_else(|error| {
        panic!(
            "failed to stage {} as {}: {error}",
            candidate.display(),
            destination.display()
        )
    });
    assert!(
        destination.is_file()
            && !fs::symlink_metadata(&destination)
                .expect("read staged LiteInst runtime metadata")
                .file_type()
                .is_symlink(),
        "LiteInst runtime stage is missing or not a real file: {}",
        destination.display()
    );
    let staged = qualifier::FileIdentity::read(&destination)
        .expect("failed to identify the protected staged runtime");
    assert_eq!(
        staged.sha256, qualification.artifact.sha256,
        "protected staged bytes differ from the qualified current artifact"
    );
    qualification
        .artifact
        .verify()
        .expect("qualified current artifact changed during staging");
    compiled_sources
        .verify_unchanged(&mut runner)
        .expect("compiled sources changed during protected staging");
    qualifier_source_identity
        .verify()
        .expect("constructor proof source changed during protected staging");
    qualifier::write_new(
        &evidence.join("compiled-sources-readback.json"),
        &serde_json::to_vec_pretty(&serde_json::json!({
            "compiled_sources": compiled_sources.record(),
            "commands": runner.commands,
        }))
        .expect("failed to serialize final compiled-source readback"),
    )
    .expect("failed to retain final compiled-source readback");
    qualifier::write_new(
        &evidence.join("staged.json"),
        &serde_json::to_vec_pretty(&staged).expect("failed to serialize staged runtime identity"),
    )
    .expect("failed to retain protected staged runtime identity");
}
