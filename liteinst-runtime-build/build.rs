use std::env;
use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

use goblin::elf::Elf;
use goblin::elf::header;
use goblin::elf::section_header;

mod artifact;

fn has_preload_constructor(path: &Path) -> io::Result<bool> {
    let bytes = fs::read(path)?;
    let elf =
        Elf::parse(&bytes).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if elf.header.e_type != header::ET_DYN || elf.header.e_machine != header::EM_X86_64 {
        return Ok(false);
    }
    let Some((initializer_index, initializer)) =
        elf.dynsyms.iter().enumerate().find(|(_, symbol)| {
            elf.dynstrtab.get_at(symbol.st_name) == Some("reverie_liteinst_initialize")
        })
    else {
        return Ok(false);
    };
    let Some(init_array) = elf
        .section_headers
        .iter()
        .find(|section| section.sh_type == section_header::SHT_INIT_ARRAY)
    else {
        return Ok(false);
    };
    let init_start = init_array.sh_addr;
    let init_end = init_start.saturating_add(init_array.sh_size);
    let relocated = elf
        .dynrelas
        .iter()
        .chain(elf.dynrels.iter())
        .any(|relocation| {
            (init_start..init_end).contains(&relocation.r_offset)
                && relocation.r_sym == initializer_index
        });
    let direct = usize::try_from(init_array.sh_offset)
        .ok()
        .and_then(|start| {
            usize::try_from(init_array.sh_size)
                .ok()
                .and_then(|size| bytes.get(start..start.checked_add(size)?))
        })
        .unwrap_or_default()
        .as_chunks::<8>()
        .0
        .iter()
        .any(|entry| u64::from_le_bytes(*entry) == initializer.st_value);
    Ok(relocated || direct)
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
    println!("cargo:rerun-if-env-changed=PROFILE");
    println!("cargo:rerun-if-changed=Cargo.lock");
    println!("cargo:rerun-if-changed=artifact.rs");
    println!("cargo:rerun-if-changed=runtime/Cargo.toml");
    println!("cargo:rerun-if-changed=runtime/src/lib.rs");
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
    let nested_target = out_dir.join("runtime-target");
    let output = Command::new(env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
        .args([
            "build",
            "--locked",
            "--manifest-path",
            "Cargo.toml",
            "-p",
            "hermit-liteinst-runtime-artifact",
            "--profile",
            profile,
            "--target-dir",
        ])
        .arg(&nested_target)
        .arg("--message-format=json-render-diagnostics")
        .current_dir(&manifest_dir)
        .env_remove("HERMIT_LITEINST_STAGE")
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
    let candidates = artifact::liteinst_cdylibs_from_cargo_messages(&messages)
        .unwrap_or_else(|error| panic!("failed to parse isolated Cargo output: {error}"));
    assert_eq!(
        candidates.len(),
        1,
        "expected exactly one LiteInst cdylib in current isolated Cargo output, found {candidates:?}",
    );
    assert!(
        has_preload_constructor(&candidates[0]).unwrap_or_else(|error| panic!(
            "failed to validate current LiteInst artifact {}: {error}",
            candidates[0].display()
        )),
        "current LiteInst artifact lacks the preload constructor: {}",
        candidates[0].display()
    );
    copy_into_protected_stage(&candidates[0], &destination).unwrap_or_else(|error| {
        panic!(
            "failed to stage {} as {}: {error}",
            candidates[0].display(),
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
}
