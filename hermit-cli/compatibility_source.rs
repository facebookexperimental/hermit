/* Copyright (c) Meta Platforms, Inc. and affiliates. */

//! Exact clean Git source measurement shared by the Cargo stamp and its
//! top-level producer. Native/export/registry identity is not inferred.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::process::Command;

use sha2::Digest;
use sha2::Sha256;

use super::compatibility_graph::InputFile;
use super::compatibility_graph::Package;

pub fn full_oid(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub fn git(root: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .arg("--no-optional-locks")
        .args([
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.untrackedCache=false",
        ])
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        .output()
        .map_err(|error| format!("cannot measure core Git identity: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "core Git measurement {args:?} failed: {}",
            output.status
        ));
    }
    String::from_utf8(output.stdout)
        .map(|text| text.trim_end_matches('\n').to_owned())
        .map_err(|_| "core Git measurement returned non-UTF8 data".to_owned())
}

fn metadata_identity(meta: &std::fs::Metadata) -> (u64, u64, u64, u32, i64, i64, i64, i64) {
    (
        meta.dev(),
        meta.ino(),
        meta.len(),
        meta.mode(),
        meta.mtime(),
        meta.mtime_nsec(),
        meta.ctime(),
        meta.ctime_nsec(),
    )
}

fn hash_field(hash: &mut Sha256, bytes: &[u8]) {
    hash.update((bytes.len() as u64).to_le_bytes());
    hash.update(bytes);
}

fn source_digest(root: &Path, track: bool) -> Result<String, String> {
    let entries = git(root, &["ls-tree", "-r", "--full-tree", "-z", "HEAD"])?;
    let mut hash = Sha256::new();
    hash.update(b"hermit-full-core-source/v1\0");
    for entry in entries.split('\0').filter(|entry| !entry.is_empty()) {
        let (header, name) = entry
            .split_once('\t')
            .ok_or_else(|| "malformed core Git tree entry".to_owned())?;
        let mut fields = header.split(' ');
        let mode = fields
            .next()
            .ok_or_else(|| "missing core tree mode".to_owned())?;
        let kind = fields
            .next()
            .ok_or_else(|| "missing core tree kind".to_owned())?;
        let oid = fields
            .next()
            .ok_or_else(|| "missing core tree object".to_owned())?;
        if fields.next().is_some() || !full_oid(oid) || name.contains('\n') || name.contains('\r') {
            return Err(
                "unsupported core tree entry for exact compatibility measurement".to_owned(),
            );
        }
        hash_field(&mut hash, mode.as_bytes());
        hash_field(&mut hash, name.as_bytes());
        let path = root.join(name);
        match (mode, kind) {
            ("160000", "commit") => {
                // Cargo's git dependencies are separately bound by the lock
                // cohort. A gitlink is a declared source identity, not proof
                // that an optional separately produced runtime was qualified.
                hash_field(&mut hash, oid.as_bytes());
            }
            ("120000", "blob") => {
                let before = std::fs::symlink_metadata(&path)
                    .map_err(|error| format!("cannot inspect core symlink {name}: {error}"))?;
                if !before.file_type().is_symlink() {
                    return Err(format!("core symlink {name} has another type"));
                }
                let target = std::fs::read_link(&path)
                    .map_err(|error| format!("cannot read core symlink {name}: {error}"))?;
                let bytes = target.as_os_str().as_bytes();
                let mut blob = sha1_smol::Sha1::new();
                blob.update(format!("blob {}\0", bytes.len()).as_bytes());
                blob.update(bytes);
                if blob.digest().to_string() != oid {
                    return Err(format!(
                        "actual core symlink {name} differs from its canonical Git blob"
                    ));
                }
                hash_field(&mut hash, bytes);
                let after = std::fs::symlink_metadata(&path).map_err(|error| {
                    format!("core symlink changed while measuring {name}: {error}")
                })?;
                if metadata_identity(&before) != metadata_identity(&after) {
                    return Err(format!("core symlink changed while measuring {name}"));
                }
            }
            ("100644" | "100755", "blob") => {
                let before = std::fs::symlink_metadata(&path)
                    .map_err(|error| format!("cannot inspect core source {name}: {error}"))?;
                if !before.is_file() || (before.mode() & 0o111 != 0) != (mode == "100755") {
                    return Err(format!(
                        "core source {name} type/mode differs from its Git tree"
                    ));
                }
                let mut file = std::fs::File::open(&path)
                    .map_err(|error| format!("cannot open core source {name}: {error}"))?;
                if metadata_identity(&before)
                    != metadata_identity(&file.metadata().map_err(|error| error.to_string())?)
                {
                    return Err(format!("core source was replaced while opening {name}"));
                }
                hash.update(before.len().to_le_bytes());
                let mut blob = sha1_smol::Sha1::new();
                blob.update(format!("blob {}\0", before.len()).as_bytes());
                let mut total = 0u64;
                let mut buffer = [0u8; 64 * 1024];
                loop {
                    let n = file
                        .read(&mut buffer)
                        .map_err(|error| format!("cannot read core source {name}: {error}"))?;
                    if n == 0 {
                        break;
                    }
                    total += n as u64;
                    hash.update(&buffer[..n]);
                    blob.update(&buffer[..n]);
                }
                let after = std::fs::symlink_metadata(&path).map_err(|error| error.to_string())?;
                if total != before.len() || metadata_identity(&before) != metadata_identity(&after)
                {
                    return Err(format!("core source changed while measuring {name}"));
                }
                if blob.digest().to_string() != oid {
                    return Err(format!(
                        "actual core source {name} differs from its canonical Git blob"
                    ));
                }
            }
            _ => return Err(format!("unsupported core tree mode/type {mode}/{kind}")),
        }
        if track && path.exists() {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
    Ok(format!("{:x}", hash.finalize()))
}

pub fn source(root: &Path, track: bool) -> Result<serde_json::Value, String> {
    // A generated Cargo project beneath some enclosing repository is not the
    // core checkout. In particular, do not stamp the dev-hermit parent here.
    let actual = std::fs::canonicalize(git(root, &["rev-parse", "--show-toplevel"])?)
        .map_err(|error| format!("cannot resolve core Git root: {error}"))?;
    let expected = std::fs::canonicalize(root)
        .map_err(|error| format!("cannot resolve core source directory: {error}"))?;
    if actual != expected {
        return Err(
            "compatibility measurement requires the actual core repository root".to_owned(),
        );
    }
    let commit = git(root, &["rev-parse", "--verify", "HEAD^{commit}"])?;
    let tree = git(root, &["rev-parse", "--verify", "HEAD^{tree}"])?;
    if !full_oid(&commit) || !full_oid(&tree) {
        return Err(
            "compatibility measurement requires full SHA-1 Git commit and tree IDs".to_owned(),
        );
    }
    let status = git(
        root,
        &[
            "status",
            "--porcelain=v1",
            "--untracked-files=all",
            "--ignore-submodules=none",
        ],
    )?;
    if !status.is_empty() {
        return Err(
            "compatibility measurement requires clean tracked source, index and nonignored inputs"
                .to_owned(),
        );
    }
    let flags = git(root, &["ls-files", "-v", "-z"])?;
    if flags
        .split('\0')
        .filter(|entry| !entry.is_empty())
        .any(|entry| !entry.starts_with("H "))
    {
        return Err(
            "compatibility measurement refuses sparse/assume-unchanged/unmerged index entries"
                .to_owned(),
        );
    }
    // Full actual source bytes/modes are bound as well as the full Git OIDs.
    // This is independent of stat-cache shortcuts in a dirty-state label.
    let sha256 = source_digest(root, track)?;
    if git(root, &["rev-parse", "--verify", "HEAD^{commit}"])? != commit
        || git(
            root,
            &[
                "status",
                "--porcelain=v1",
                "--untracked-files=all",
                "--ignore-submodules=none",
            ],
        )? != status
    {
        return Err("core source changed during compatibility measurement".to_owned());
    }
    if track {
        for name in ["HEAD", "index", "packed-refs"] {
            let path = git(
                root,
                &["rev-parse", "--path-format=absolute", "--git-path", name],
            )?;
            if Path::new(&path).exists() {
                println!("cargo:rerun-if-changed={path}");
            }
        }
        if let Ok(reference) = git(root, &["symbolic-ref", "-q", "HEAD"]) {
            let path = git(
                root,
                &[
                    "rev-parse",
                    "--path-format=absolute",
                    "--git-path",
                    &reference,
                ],
            )?;
            println!("cargo:rerun-if-changed={path}");
        }
    }
    Ok(serde_json::json!({"kind":"git", "commit":commit, "tree":tree, "sha256":sha256}))
}

/// Cargo materializes each Git repository with an empty completion marker.
/// This is a separate policy from `source`: core checkouts remain strict.
/// Metadata must first bind the canonical Reverie URL and exact resolved OID.
pub fn cargo_git_source(
    root: &Path,
    expected: &str,
    track: bool,
) -> Result<serde_json::Value, String> {
    let home = std::env::var_os("CARGO_HOME")
        .ok_or_else(|| "Cargo Git source has no actual CARGO_HOME".to_owned())?;
    let cache = std::fs::canonicalize(Path::new(&home).join("git/checkouts"))
        .map_err(|error| format!("cannot resolve actual Cargo Git cache: {error}"))?;
    cargo_git_source_in(root, expected, &cache, track, unsafe { libc::geteuid() })
}

fn cargo_git_source_in(
    root: &Path,
    expected: &str,
    cache: &Path,
    track: bool,
    owner: u32,
) -> Result<serde_json::Value, String> {
    if !matches!(root.strip_prefix(cache), Ok(relative) if relative.components().count() == 2) {
        return Err("resolved Reverie root is not an actual Cargo Git checkout".to_owned());
    }
    cargo_repository(root, expected, track, owner, &mut BTreeSet::new())
}

fn cargo_marker(root: &Path, owner: u32) -> Result<serde_json::Value, String> {
    let path = root.join(".cargo-ok");
    let before = std::fs::symlink_metadata(&path)
        .map_err(|error| format!("Cargo Git checkout has no completion marker: {error}"))?;
    if !before.is_file()
        || before.uid() != owner
        || before.mode() & 0o7777 != 0o644
        || before.len() != 0
    {
        return Err("Cargo completion marker is not an empty owned regular 0644 file".to_owned());
    }
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&path)
        .map_err(|error| format!("cannot open Cargo completion marker: {error}"))?;
    let opened = file.metadata().map_err(|error| error.to_string())?;
    let mut bytes = [0u8; 1];
    if metadata_identity(&before) != metadata_identity(&opened)
        || opened.uid() != owner
        || file.read(&mut bytes).map_err(|error| error.to_string())? != 0
    {
        return Err("Cargo completion marker changed while opening/reading".to_owned());
    }
    let after = std::fs::symlink_metadata(&path).map_err(|error| error.to_string())?;
    if metadata_identity(&before) != metadata_identity(&after) || after.uid() != owner {
        return Err("Cargo completion marker changed during measurement".to_owned());
    }
    Ok(
        serde_json::json!({"type":"regular", "mode":"100644", "uid":owner,
        "bytes_hex":"", "sha256":format!("{:x}", Sha256::digest(b""))}),
    )
}

fn cargo_state(root: &Path) -> Result<serde_json::Value, String> {
    let flags = git(root, &["ls-files", "-v", "-z"])?;
    if flags
        .split('\0')
        .filter(|entry| !entry.is_empty())
        .any(|entry| !entry.starts_with("H "))
    {
        return Err(
            "Cargo Git source has sparse/assume-unchanged/unmerged index entries".to_owned(),
        );
    }
    let index = git(
        root,
        &["rev-parse", "--path-format=absolute", "--git-path", "index"],
    )?;
    let index = Path::new(&index);
    let before = std::fs::symlink_metadata(index).map_err(|error| error.to_string())?;
    if !before.is_file() {
        return Err("Cargo Git source index is not a regular file".to_owned());
    }
    let bytes = std::fs::read(index).map_err(|error| error.to_string())?;
    let after = std::fs::symlink_metadata(index).map_err(|error| error.to_string())?;
    if metadata_identity(&before) != metadata_identity(&after) {
        return Err("Cargo Git source index changed while measuring".to_owned());
    }
    Ok(serde_json::json!({
        "commit":git(root, &["rev-parse", "--verify", "HEAD^{commit}"])?,
        "tree":git(root, &["rev-parse", "--verify", "HEAD^{tree}"])?,
        "raw_status":git(root, &["status", "--porcelain=v1", "-z", "--untracked-files=all", "--ignore-submodules=none"])?,
        "index_sha256":format!("{:x}", Sha256::digest(bytes)),
        "index_flags_sha256":format!("{:x}", Sha256::digest(flags.as_bytes()))
    }))
}

fn cargo_repository(
    root: &Path,
    expected: &str,
    track: bool,
    owner: u32,
    seen: &mut BTreeSet<std::path::PathBuf>,
) -> Result<serde_json::Value, String> {
    let metadata = std::fs::symlink_metadata(root).map_err(|error| error.to_string())?;
    if !metadata.is_dir()
        || metadata.uid() != owner
        || std::fs::canonicalize(root).map_err(|error| error.to_string())? != root
        || std::fs::canonicalize(git(root, &["rev-parse", "--show-toplevel"])?)
            .map_err(|error| error.to_string())?
            != root
        || !full_oid(expected)
        || !seen.insert(root.to_owned())
        || seen.len() > 128
    {
        return Err(
            "Cargo Git source root is missing, aliased, unowned or noncanonical".to_owned(),
        );
    }
    let before = cargo_state(root)?;
    if before["commit"].as_str() != Some(expected) || !before["tree"].as_str().is_some_and(full_oid)
    {
        return Err("Cargo Git source HEAD differs from its exact declared gitlink/pin".to_owned());
    }
    let marker = cargo_marker(root, owner)?;
    let mut links = BTreeMap::new();
    for entry in git(root, &["ls-tree", "-r", "--full-tree", "-z", "HEAD"])?
        .split('\0')
        .filter(|entry| !entry.is_empty())
    {
        let (header, name) = entry
            .split_once('\t')
            .ok_or_else(|| "malformed Cargo Git tree entry".to_owned())?;
        if let Some(oid) = header.strip_prefix("160000 commit ") {
            if !full_oid(oid) || name.contains('\n') || name.contains('\r') {
                return Err("noncanonical Cargo Git source gitlink".to_owned());
            }
            links.insert(name.to_owned(), oid.to_owned());
        }
    }
    let status = before["raw_status"]
        .as_str()
        .ok_or("missing Cargo Git status")?;
    let mut marker_count = 0;
    for entry in status.split('\0').filter(|entry| !entry.is_empty()) {
        if entry == "?? .cargo-ok" {
            marker_count += 1;
        } else if !entry
            .strip_prefix(" M ")
            .is_some_and(|name| links.contains_key(name))
        {
            return Err(format!(
                "Cargo Git source has an unapproved source/index/untracked change: {entry:?}"
            ));
        }
    }
    if marker_count != 1 || !git(root, &["ls-files", "--", ".cargo-ok"])?.is_empty() {
        return Err("Cargo completion marker is missing, tracked, hidden or duplicated".to_owned());
    }
    let mut children = Vec::new();
    let mut hash = Sha256::new();
    hash.update(b"hermit-cargo-git-materialization/v1\0");
    hash_field(&mut hash, expected.as_bytes());
    let content = source_digest(root, track)?;
    hash_field(&mut hash, content.as_bytes());
    for (name, oid) in links {
        let child = cargo_repository(&root.join(&name), &oid, track, owner, seen)?;
        hash_field(&mut hash, name.as_bytes());
        hash_field(
            &mut hash,
            child["sha256"]
                .as_str()
                .ok_or("missing child source digest")?
                .as_bytes(),
        );
        children.push(serde_json::json!({"path":name,"identity":child}));
    }
    let after = cargo_state(root)?;
    let marker_after = cargo_marker(root, owner)?;
    if before != after || marker_after != marker || source_digest(root, false)? != content {
        return Err(
            "Cargo Git refs/index/status/marker changed during source measurement".to_owned(),
        );
    }
    Ok(
        serde_json::json!({"kind":"git", "commit":expected, "tree":before["tree"],
        "sha256":format!("{:x}",hash.finalize()), "source_sha256":content,
        "cargo_marker_before":marker,
        "cargo_marker_after":marker_after,
        "before":before, "after":after, "gitlinks":children}),
    )
}

/// Path/ordinary core checkouts keep `source`'s exact strict policy. Only an
/// actual canonical metadata-bound Cargo Git package uses materialization
/// proof; its shared core identity is still the ordinary canonical digest.
pub fn metadata_core(package: &Package, track: bool) -> Result<serde_json::Value, String> {
    if package.source.is_none() {
        return metadata_core_in(package, None, track);
    }
    let home = std::env::var_os("CARGO_HOME")
        .ok_or_else(|| "Cargo core source has no actual CARGO_HOME".to_owned())?;
    let cache = std::fs::canonicalize(Path::new(&home).join("git/checkouts"))
        .map_err(|error| format!("cannot resolve actual Cargo core Git cache: {error}"))?;
    metadata_core_in(package, Some(&cache), track)
}

fn metadata_core_in(
    package: &Package,
    cache: Option<&Path>,
    track: bool,
) -> Result<serde_json::Value, String> {
    if package.name != "hermit-run" {
        return Err("metadata core source is not the hermit-run package".to_owned());
    }
    let manifest = package
        .manifest_path
        .canonicalize()
        .map_err(|error| format!("cannot resolve actual core manifest: {error}"))?;
    if !manifest.is_file() || manifest.file_name().is_none_or(|name| name != "Cargo.toml") {
        return Err("metadata core manifest is not an actual Cargo.toml".to_owned());
    }
    let root = manifest
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| "metadata core has no repository parent".to_owned())?;
    let Some(source_id) = package.source.as_deref() else {
        return Ok(serde_json::json!({"path":root,"identity":source(root, track)?}));
    };
    let source_id = source_id
        .strip_prefix("git+https://github.com/rrnewton/hermit?rev=")
        .or_else(|| source_id.strip_prefix("git+https://github.com/rrnewton/hermit.git?rev="))
        .ok_or_else(|| "metadata core has a noncanonical or patched source".to_owned())?;
    let (declared, resolved) = source_id
        .split_once('#')
        .ok_or_else(|| "metadata core Git source has no resolved commit".to_owned())?;
    if !full_oid(declared) || declared != resolved {
        return Err("metadata core must declare and resolve the same full commit".to_owned());
    }
    let materialization = cargo_git_source_in(
        root,
        resolved,
        cache.ok_or_else(|| "metadata core Git source has no actual Cargo cache".to_owned())?,
        track,
        unsafe { libc::geteuid() },
    )?;
    let content = materialization
        .get("source_sha256")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "Cargo core proof has no canonical source digest".to_owned())?;
    let identity = serde_json::json!({"kind":"git","commit":resolved,
        "tree":materialization["tree"],"sha256":content});
    Ok(serde_json::json!({"path":root,"identity":identity,
        "cargo_materialization":materialization}))
}

/// The producer checks these complete source measurements around each build;
/// the final build script independently checks the same pre-build snapshot.
/// Paths are evidence locators, not cross-executable compatibility dimensions.
pub fn snapshot(metadata: &InputFile, track: bool) -> Result<serde_json::Value, String> {
    #[derive(serde::Deserialize)]
    struct Metadata {
        version: u64,
        packages: Vec<Package>,
    }
    let metadata: Metadata = serde_json::from_slice(&metadata.read(64 * 1024 * 1024)?)
        .map_err(|error| format!("cannot read source snapshot metadata: {error}"))?;
    if metadata.version != 1 {
        return Err("unsupported source snapshot metadata version".to_owned());
    }
    let cores: Vec<_> = metadata
        .packages
        .iter()
        .filter(|package| package.name == "hermit-run")
        .collect();
    if cores.len() != 1 {
        return Err("source snapshot requires exactly one actual core package".to_owned());
    }
    let core = metadata_core(cores[0], track)?;
    let mut roots = BTreeMap::new();
    for package in &metadata.packages {
        let Some(value) = package.source.as_deref() else {
            continue;
        };
        let Some(value) = value.strip_prefix("git+https://github.com/rrnewton/reverie.git?rev=")
        else {
            if value.contains("github.com/rrnewton/reverie") {
                return Err("source snapshot has a noncanonical Reverie source".to_owned());
            }
            continue;
        };
        let (declared, resolved) = value
            .split_once('#')
            .ok_or_else(|| "source snapshot Reverie has no resolved commit".to_owned())?;
        if !full_oid(declared) || declared != resolved {
            return Err(
                "source snapshot Reverie pin is incomplete or differs from resolution".to_owned(),
            );
        }
        let directory = package
            .manifest_path
            .parent()
            .ok_or_else(|| "source snapshot package has no directory".to_owned())?;
        let root = std::fs::canonicalize(git(directory, &["rev-parse", "--show-toplevel"])?)
            .map_err(|error| error.to_string())?;
        if roots
            .insert(root, resolved.to_owned())
            .is_some_and(|old| old != resolved)
        {
            return Err("source snapshot root claims different Reverie commits".to_owned());
        }
    }
    if roots.is_empty() {
        return Err("source snapshot has no actual Reverie source".to_owned());
    }
    let mut reverie = Vec::new();
    for (root, expected) in roots {
        let identity = cargo_git_source(&root, &expected, track)?;
        if identity["commit"].as_str() != Some(expected.as_str()) {
            return Err(
                "source snapshot actual Reverie checkout differs from its metadata source"
                    .to_owned(),
            );
        }
        reverie.push(serde_json::json!({"path":root,"identity":identity}));
    }
    Ok(
        serde_json::json!({"schema":"hermit-compatibility-source-snapshot/v1","core":core,"reverie":reverie}),
    )
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::Ordering;

    use super::*;

    static NEXT: AtomicU64 = AtomicU64::new(0);

    struct Fixture {
        directory: PathBuf,
        cache: PathBuf,
        root: PathBuf,
        oid: String,
    }

    fn command(root: &Path, args: &[&str]) -> String {
        // These are new, disposable local source fixtures, not product commits.
        let output = Command::new("/usr/bin/git")
            .args([
                "--no-optional-locks",
                "-c",
                "protocol.file.allow=always",
                "-c",
                "user.name=source-fixture",
                "-c",
                "user.email=source-fixture@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "-C",
            ])
            .arg(root)
            .args(args)
            .output()
            .expect("run local source fixture Git");
        assert!(
            output.status.success(),
            "fixture Git {args:?}: {:?}",
            output
        );
        String::from_utf8(output.stdout)
            .expect("fixture Git UTF8")
            .trim()
            .to_owned()
    }

    fn initialize(root: &Path) {
        std::fs::create_dir_all(root).unwrap();
        command(root, &["init", "--quiet"]);
        std::fs::write(root.join("source.rs"), b"original source\n").unwrap();
        command(root, &["add", "--", "source.rs"]);
        command(root, &["commit", "--quiet", "-m", "canonical fixture"]);
    }

    fn marker(root: &Path) {
        let path = root.join(".cargo-ok");
        std::fs::write(&path, b"").unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644)).unwrap();
    }

    impl Fixture {
        fn new() -> Self {
            let directory = std::env::temp_dir().join(format!(
                "hermit-cargo-source-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&directory).unwrap();
            let cache = directory.join("git/checkouts");
            let root = cache.join("reverie-fixture/revision");
            initialize(&root);
            let child = directory.join("child-source");
            initialize(&child);
            command(
                &root,
                &[
                    "submodule",
                    "add",
                    "--quiet",
                    child.to_str().unwrap(),
                    "child",
                ],
            );
            command(&root, &["commit", "--quiet", "-m", "canonical child"]);
            marker(&root);
            marker(&root.join("child"));
            let oid = command(&root, &["rev-parse", "HEAD"]);
            Self {
                directory,
                cache,
                root,
                oid,
            }
        }

        fn measure(&self) -> Result<serde_json::Value, String> {
            cargo_git_source_in(&self.root, &self.oid, &self.cache, false, unsafe {
                libc::geteuid()
            })
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.directory)
                .expect("remove only owned local source fixture");
        }
    }

    #[test]
    fn cargo_materialization_measures_children_and_keeps_core_strict() {
        let fixture = Fixture::new();
        let measured = fixture.measure().expect("exact Cargo materialization");
        assert_eq!(measured["commit"], fixture.oid);
        assert_eq!(measured["gitlinks"].as_array().unwrap().len(), 1);
        assert_eq!(measured["cargo_marker_before"]["bytes_hex"], "");
        assert_eq!(
            measured["cargo_marker_before"],
            measured["cargo_marker_after"]
        );
        assert_eq!(measured["before"], measured["after"]);
        assert_eq!(fixture.measure().unwrap(), measured);
        assert!(
            source(&fixture.root, false).is_err(),
            "core policy must still reject markers"
        );
        assert!(
            cargo_git_source_in(
                &fixture.root,
                &fixture.oid,
                &fixture.directory,
                false,
                unsafe { libc::geteuid() }
            )
            .is_err(),
            "non-Cargo checkout shape must refuse"
        );
    }

    #[test]
    fn cargo_materialization_refuses_unowned_wrong_markers_and_unknown_files() {
        let fixture = Fixture::new();
        let path = fixture.root.join(".cargo-ok");
        std::fs::write(&path, b"not bookkeeping").unwrap();
        assert!(fixture.measure().is_err());
        marker(&fixture.root);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666)).unwrap();
        assert!(fixture.measure().is_err());
        marker(&fixture.root);
        assert!(
            cargo_git_source_in(
                &fixture.root,
                &fixture.oid,
                &fixture.cache,
                false,
                unsafe { libc::geteuid() }.wrapping_add(1)
            )
            .is_err()
        );
        assert!(cargo_marker(&fixture.root, unsafe { libc::geteuid() }.wrapping_add(1)).is_err());
        std::fs::remove_file(&path).unwrap();
        assert!(
            fixture.measure().is_err(),
            "missing completion marker must refuse"
        );
        std::os::unix::fs::symlink("source.rs", &path).unwrap();
        assert!(fixture.measure().is_err());
        std::fs::remove_file(&path).unwrap();
        marker(&fixture.root);
        let unknown = fixture.root.join("unknown-input");
        std::fs::write(&unknown, b"").unwrap();
        assert!(fixture.measure().is_err());
        std::fs::remove_file(unknown).unwrap();
        let nested_unknown = fixture.root.join("child/unknown-input");
        std::fs::write(&nested_unknown, b"").unwrap();
        assert!(
            fixture.measure().is_err(),
            "nested unknown input must refuse"
        );
        std::fs::remove_file(nested_unknown).unwrap();
        command(&fixture.root, &["add", "--", ".cargo-ok"]);
        assert!(
            fixture.measure().is_err(),
            "tracked marker is not Cargo bookkeeping"
        );
    }

    #[test]
    fn cargo_materialization_refuses_changed_sources_refs_flags_and_missing_children() {
        let fixture = Fixture::new();
        let child = fixture.root.join("child");
        std::fs::write(child.join("source.rs"), b"changed source\n").unwrap();
        assert!(
            fixture.measure().is_err(),
            "nested tracked mutation must refuse"
        );
        assert!(
            source_digest(&child, false).is_err(),
            "canonical byte check must refuse actual changed blob"
        );
        std::fs::write(child.join("source.rs"), b"original source\n").unwrap();
        std::fs::write(fixture.root.join("source.rs"), b"staged source\n").unwrap();
        command(&fixture.root, &["add", "--", "source.rs"]);
        assert!(
            fixture.measure().is_err(),
            "staged source mutation must refuse"
        );
        command(&fixture.root, &["restore", "--staged", "--", "source.rs"]);
        std::fs::write(fixture.root.join("source.rs"), b"original source\n").unwrap();
        command(
            &fixture.root,
            &["update-index", "--assume-unchanged", "source.rs"],
        );
        assert!(fixture.measure().is_err(), "index masking must refuse");
        command(
            &fixture.root,
            &["update-index", "--no-assume-unchanged", "source.rs"],
        );
        let moved = fixture.directory.join("missing-child");
        std::fs::rename(&child, &moved).unwrap();
        assert!(
            fixture.measure().is_err(),
            "required materialized child must exist"
        );
        std::os::unix::fs::symlink(&moved, &child).unwrap();
        assert!(
            fixture.measure().is_err(),
            "symlinked materialized child must refuse"
        );
        std::fs::remove_file(&child).unwrap();
        std::fs::rename(&moved, &child).unwrap();
        std::fs::write(child.join("source.rs"), b"another committed child\n").unwrap();
        command(&child, &["add", "--", "source.rs"]);
        command(&child, &["commit", "--quiet", "-m", "different child"]);
        assert!(
            fixture.measure().is_err(),
            "changed gitlink HEAD must refuse"
        );
        assert!(
            cargo_git_source_in(
                &fixture.root,
                &"0".repeat(40),
                &fixture.cache,
                false,
                unsafe { libc::geteuid() }
            )
            .is_err(),
            "wrong metadata revision must refuse"
        );
        command(&fixture.root, &["add", "--", "child"]);
        command(
            &fixture.root,
            &["commit", "--quiet", "-m", "different root"],
        );
        assert!(
            fixture.measure().is_err(),
            "changed root reference must refuse"
        );
    }

    #[test]
    fn metadata_git_core_has_the_same_canonical_identity_and_keeps_path_core_strict() {
        let mut fixture = Fixture::new();
        let crate_dir = fixture.root.join("hermit-cli");
        std::fs::create_dir(&crate_dir).unwrap();
        std::fs::write(
            crate_dir.join("Cargo.toml"),
            b"[package]\nname='hermit-run'\nversion='0.5.4'\n",
        )
        .unwrap();
        command(&fixture.root, &["add", "--", "hermit-cli/Cargo.toml"]);
        command(
            &fixture.root,
            &["commit", "--quiet", "-m", "core fixture package"],
        );
        fixture.oid = command(&fixture.root, &["rev-parse", "HEAD"]);
        let ordinary = fixture.directory.join("ordinary-core");
        command(
            &fixture.directory,
            &[
                "clone",
                "--quiet",
                "--no-hardlinks",
                fixture.root.to_str().unwrap(),
                ordinary.to_str().unwrap(),
            ],
        );
        command(&ordinary, &["submodule", "update", "--init", "--quiet"]);
        let ordinary_identity = source(&ordinary, false).unwrap();
        let mut package = Package {
            id: "actual-core".to_owned(),
            name: "hermit-run".to_owned(),
            version: "0.5.4".to_owned(),
            source: Some(format!(
                "git+https://github.com/rrnewton/hermit?rev={0}#{0}",
                fixture.oid
            )),
            manifest_path: crate_dir.join("Cargo.toml"),
            targets: vec![],
        };
        let measured = metadata_core_in(&package, Some(&fixture.cache), false).unwrap();
        assert_eq!(
            measured["identity"], ordinary_identity,
            "cache markers must not change the canonical core digest"
        );
        assert!(measured["cargo_materialization"]["cargo_marker_before"].is_object());
        package.source = None;
        assert!(
            metadata_core_in(&package, Some(&fixture.cache), false).is_err(),
            "ordinary/path core must remain strict"
        );
        package.source = Some(format!(
            "git+https://example.invalid/hermit?rev={0}#{0}",
            fixture.oid
        ));
        assert!(metadata_core_in(&package, Some(&fixture.cache), false).is_err());
        package.source = Some(format!(
            "git+https://github.com/rrnewton/hermit?rev={}#{}",
            fixture.oid,
            "0".repeat(40)
        ));
        assert!(metadata_core_in(&package, Some(&fixture.cache), false).is_err());
        package.source = Some(format!(
            "git+https://github.com/rrnewton/hermit?rev={0}#{0}",
            fixture.oid
        ));
        std::fs::write(
            fixture.root.join("unknown-core-input"),
            b"not Cargo bookkeeping",
        )
        .unwrap();
        assert!(metadata_core_in(&package, Some(&fixture.cache), false).is_err());
    }
}
