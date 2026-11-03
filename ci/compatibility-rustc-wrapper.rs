/* Copyright (c) Meta Platforms, Inc. and affiliates. */

//! Source-owned RUSTC_WRAPPER for build-compatible-cli.rs. It forwards the
//! compiler argv unchanged, records its true exit/signal and never captures a
//! general environment. The outer producer owns the compiler resource bound.

use std::ffi::OsString;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::ExitCode;
use std::process::Stdio;
use std::time::Duration;
use std::time::Instant;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use anyhow::ensure;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;

use super::compatibility_graph::BuildInput;
use super::compatibility_graph::Graph;
use super::compatibility_rustc;
use super::compatibility_rustc::Bundle;
use super::compatibility_rustc::CAPTURE_ENV;
use super::compatibility_rustc::CAPTURE_SCHEMA;
use super::compatibility_rustc::Captured;
use super::compatibility_rustc::Compiler;
use super::compatibility_rustc::CompilerInput;
use super::compatibility_rustc::CompilerQuery;
use super::compatibility_rustc::Completion;
use super::compatibility_rustc::DirectoryIdentity;
use super::compatibility_rustc::FileIdentity;
use super::compatibility_rustc::GATE_ENV;
use super::compatibility_rustc::Invocation;
use super::compatibility_rustc::MAX_CAPTURE_BYTES;
use super::compatibility_rustc::MAX_COMMAND_BYTES;
use super::compatibility_rustc::MAX_COMMANDS;
use super::compatibility_rustc::ResponseFile;
use super::compatibility_rustc::SysrootArtifact;
use super::compatibility_rustc::SysrootExtern;
use super::compatibility_rustc::SysrootSearch;

const COMPLETION_ALLOWANCE: usize = 16 * 1024;
const VERSION_BYTES: usize = 64 * 1024;

fn identity(file: &File) -> Result<FileIdentity> {
    let meta = file.metadata()?;
    ensure!(
        meta.is_file(),
        "compiler capture input is not a regular file"
    );
    Ok(FileIdentity {
        device: meta.dev(),
        inode: meta.ino(),
        bytes: meta.len(),
        mode: meta.mode(),
        modified: (meta.mtime(), meta.mtime_nsec()),
        changed: (meta.ctime(), meta.ctime_nsec()),
    })
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn file_digest(file: &mut File) -> Result<String> {
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

fn read(path: &Path, limit: usize) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    File::open(path)?
        .take((limit + 1) as u64)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= limit,
        "compiler capture input exceeds its bound: {}",
        path.display()
    );
    Ok(bytes)
}

fn source_file(argv: &[Vec<u8>], cwd: &Path, argument: usize) -> Result<CompilerInput> {
    let name = Path::new(std::ffi::OsStr::from_bytes(&argv[argument]));
    let path = if name.is_absolute() {
        name.to_owned()
    } else {
        cwd.join(name)
    };
    let canonical = path.canonicalize()?;
    let mut file = File::open(&path)?;
    let before = identity(&file)?;
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take((MAX_COMMAND_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= MAX_COMMAND_BYTES,
        "compiler source exceeds its finite capture bound"
    );
    ensure!(
        identity(&file)? == before
            && identity(&File::open(&path)?)? == before
            && path.canonicalize()? == canonical,
        "compiler source changed while captured"
    );
    Ok(CompilerInput::File {
        argument,
        path: path.as_os_str().as_bytes().to_vec(),
        canonical_path: canonical.as_os_str().as_bytes().to_vec(),
        identity: before,
        sha256: digest(&bytes),
        bytes,
    })
}

/// Read only rustc's declared `-` input. Polling bounds a stalled producer;
/// no flags on the inherited open-file description are changed.
fn compiler_stdin() -> Result<Vec<u8>> {
    let start = Instant::now();
    let mut bytes = Vec::new();
    loop {
        ensure!(
            start.elapsed() < Duration::from_secs(10),
            "compiler stdin capture exceeded10seconds"
        );
        let mut poll = libc::pollfd {
            fd: libc::STDIN_FILENO,
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut poll, 1, 50) };
        if ready < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error.into());
        }
        if ready == 0 {
            continue;
        }
        ensure!(
            poll.revents & libc::POLLNVAL == 0,
            "compiler stdin is invalid"
        );
        let mut buffer = [0u8; 4096];
        let count =
            unsafe { libc::read(libc::STDIN_FILENO, buffer.as_mut_ptr().cast(), buffer.len()) };
        if count < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error.into());
        }
        if count == 0 {
            return Ok(bytes);
        }
        ensure!(
            bytes.len() + count as usize <= MAX_COMMAND_BYTES,
            "compiler stdin exceeds its finite capture bound"
        );
        bytes.extend_from_slice(&buffer[..count as usize]);
    }
}

const INPUT_SEALS: i32 =
    libc::F_SEAL_WRITE | libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL;

fn sealed_stdin(bytes: &[u8]) -> Result<File> {
    let fd = unsafe {
        libc::memfd_create(
            c"hermit-rustc-input".as_ptr(),
            libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
        )
    };
    ensure!(fd >= 0, "cannot create compiler stdin replay descriptor");
    let mut file = unsafe { File::from_raw_fd(fd) };
    file.write_all(bytes)?;
    ensure!(
        unsafe { libc::fcntl(fd, libc::F_ADD_SEALS, INPUT_SEALS) } == 0
            && unsafe { libc::fcntl(fd, libc::F_GET_SEALS) } == INPUT_SEALS,
        "cannot seal exact compiler stdin replay"
    );
    file.seek(SeekFrom::Start(0))?;
    Ok(file)
}

fn source_input(argv: &[Vec<u8>], cwd: &Path) -> Result<(Option<CompilerInput>, Option<File>)> {
    let candidates: Vec<_> = argv
        .iter()
        .enumerate()
        .filter(|(_, arg)| {
            arg.as_slice() == b"-" || (!arg.starts_with(b"-") && arg.ends_with(b".rs"))
        })
        .map(|(index, _)| index)
        .collect();
    ensure!(
        candidates.len() <= 1,
        "compiler input is ambiguous in actual argv"
    );
    let Some(&argument) = candidates.first() else {
        return Ok((None, None));
    };
    if argv[argument] == b"-" {
        let bytes = compiler_stdin()?;
        let file = sealed_stdin(&bytes)?;
        return Ok((
            Some(CompilerInput::Stdin {
                argument,
                sha256: digest(&bytes),
                bytes,
            }),
            Some(file),
        ));
    }
    Ok((Some(source_file(argv, cwd, argument)?), None))
}

fn source_unchanged(command: &Invocation, replay: &mut Option<File>) -> Result<bool> {
    match &command.source {
        Some(input @ CompilerInput::File { argument, .. }) => Ok(source_file(
            &command.argv,
            Path::new(std::ffi::OsStr::from_bytes(&command.cwd)),
            *argument,
        )? == *input),
        Some(CompilerInput::Stdin { bytes, .. }) => {
            let file = replay
                .as_mut()
                .context("sealed compiler stdin descriptor is absent")?;
            ensure!(
                unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GET_SEALS) } == INPUT_SEALS,
                "compiler stdin replay seals changed"
            );
            file.seek(SeekFrom::Start(0))?;
            let mut actual = Vec::new();
            file.take((MAX_COMMAND_BYTES + 1) as u64)
                .read_to_end(&mut actual)?;
            Ok(actual == *bytes)
        }
        None => Ok(replay.is_none()),
    }
}

fn exclusive(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    output.write_all(bytes)?;
    output.sync_all()?;
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    schema: String,
    directory: PathBuf,
    target_directory: PathBuf,
    wrapper: FileIdentity,
    wrapper_sha256: String,
}

pub fn prepare(directory: &Path, target_directory: &Path) -> Result<PathBuf> {
    use std::os::unix::fs::DirBuilderExt;
    ensure!(
        directory.is_absolute() && target_directory.is_absolute(),
        "capture/target paths must be absolute"
    );
    std::fs::DirBuilder::new().mode(0o700).create(directory)?;
    let executable = std::env::current_exe()?;
    let mut file = File::open(&executable)?;
    let before = identity(&file)?;
    let sha256 = file_digest(&mut file)?;
    ensure!(
        identity(&file)? == before,
        "producer executable changed while it was measured"
    );
    let config = Config {
        schema: CAPTURE_SCHEMA.to_owned(),
        directory: directory.to_owned(),
        target_directory: target_directory.to_owned(),
        wrapper: before,
        wrapper_sha256: sha256,
    };
    // Locked counters bound all concurrently recorded bytes and commands.
    exclusive(&directory.join("quota"), &[0u8; 16])?;
    let path = directory.join("config.json");
    exclusive(&path, &serde_json::to_vec(&config)?)?;
    Ok(path)
}

fn config(path: &Path) -> Result<Config> {
    let config: Config = serde_json::from_slice(&read(path, 16 * 1024)?)?;
    ensure!(
        config.schema == CAPTURE_SCHEMA
            && config.directory.is_absolute()
            && config.target_directory.is_absolute()
            && path == config.directory.join("config.json"),
        "unsupported rustc capture configuration"
    );
    let directory = std::fs::metadata(&config.directory)?;
    ensure!(
        directory.is_dir()
            && directory.uid() == unsafe { libc::geteuid() }
            && directory.mode() & 0o077 == 0,
        "rustc capture directory ownership/mode changed"
    );
    ensure!(
        identity(&File::open(std::env::current_exe()?)?)? == config.wrapper,
        "the actual rustc wrapper is not the measured producer executable"
    );
    Ok(config)
}

fn reserve(config: &Config, bytes: usize) -> Result<()> {
    reserve_with_sync(config, bytes, File::sync_data)
}

fn reserve_with_sync(
    config: &Config,
    bytes: usize,
    sync_data: impl FnOnce(&File) -> std::io::Result<()>,
) -> Result<()> {
    let mut quota = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(config.directory.join("quota"))?;
    ensure!(quota.metadata()?.len() == 16, "capture quota is malformed");
    let start = Instant::now();
    loop {
        if unsafe { libc::flock(quota.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            break;
        }
        let error = std::io::Error::last_os_error();
        ensure!(
            error.raw_os_error() == Some(libc::EWOULDBLOCK)
                && start.elapsed() < Duration::from_secs(2),
            "cannot acquire bounded capture quota: {error}"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    let mut counters = [0u8; 16];
    quota.read_exact(&mut counters)?;
    let used = u64::from_le_bytes(counters[..8].try_into()?);
    let count = u64::from_le_bytes(counters[8..].try_into()?);
    let next = used
        .checked_add(bytes as u64)
        .context("capture quota overflow")?;
    ensure!(
        next <= MAX_CAPTURE_BYTES as u64 && count < MAX_COMMANDS as u64,
        "actual rustc capture exceeds its finite byte/command bound"
    );
    counters[..8].copy_from_slice(&next.to_le_bytes());
    counters[8..].copy_from_slice(&(count + 1).to_le_bytes());
    quota.seek(SeekFrom::Start(0))?;
    quota.write_all(&counters)?;
    // Serialize the complete monotonic counter write, but do not keep other
    // reservations waiting on storage flush latency. No capture may be
    // published until this reservation (or a later larger value) is synced.
    if unsafe { libc::flock(quota.as_raw_fd(), libc::LOCK_UN) } != 0 {
        return Err(std::io::Error::last_os_error())
            .context("cannot release bounded capture quota");
    }
    sync_data(&quota)?;
    // A failed flush consumes the reservation without publishing a capture.
    // Dropping this exact descriptor still releases the lock on error paths.
    Ok(())
}

fn nonblocking(file: &impl AsRawFd) -> Result<()> {
    let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
    ensure!(
        flags >= 0
            && unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) }
                == 0,
        "cannot bound compiler identity output"
    );
    Ok(())
}

fn drain(pipe: &mut impl Read, bytes: &mut Vec<u8>) -> Result<bool> {
    let mut buffer = [0u8; 1024];
    for _ in 0..32 {
        match pipe.read(&mut buffer) {
            Ok(0) => return Ok(true),
            Ok(count) => {
                ensure!(
                    bytes.len() + count <= VERSION_BYTES,
                    "compiler identity output exceeds its bound"
                );
                bytes.extend_from_slice(&buffer[..count]);
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Ok(false),
            Err(error) => return Err(error.into()),
        }
    }
    Ok(false)
}

fn compiler_query(compiler: &Path, argv: &[&str]) -> Result<CompilerQuery> {
    let mut child = Command::new(compiler)
        .args(argv)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let result = (|| -> Result<CompilerQuery> {
        let mut stdout = child
            .stdout
            .take()
            .context("compiler identity stdout is absent")?;
        let mut stderr = child
            .stderr
            .take()
            .context("compiler identity stderr is absent")?;
        nonblocking(&stdout)?;
        nonblocking(&stderr)?;
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let (mut out_end, mut err_end) = (false, false);
        let mut status = None;
        let start = Instant::now();
        loop {
            if !out_end {
                out_end = drain(&mut stdout, &mut out)?;
            }
            if !err_end {
                err_end = drain(&mut stderr, &mut err)?;
            }
            if status.is_none() {
                status = child.try_wait()?;
            }
            if let Some(status) = status.filter(|_| out_end && err_end) {
                ensure!(
                    status.success() && err.is_empty(),
                    "compiler identity command failed or left incomplete output: {status}"
                );
                return Ok(CompilerQuery {
                    argv: argv.iter().map(|arg| arg.as_bytes().to_vec()).collect(),
                    stdout: out,
                    stderr: err,
                    exit_code: status.code().context("compiler query has no exit status")?,
                });
            }
            ensure!(
                start.elapsed() < Duration::from_secs(10),
                "compiler identity exceeded10seconds"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    })();
    if result.is_err() {
        // Only this owned metadata child is killed. All compilation remains in
        // the outer producer's original owned process group and resource bound.
        if child.try_wait()?.is_some() {
            return result;
        }
        let _ = child.kill();
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(2) {
            if child.try_wait().ok().flatten().is_some() {
                return result;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        return result.context(
            "compiler identity cleanup could not confirm its exact child reaped within2seconds",
        );
    }
    result
}

fn compiler_version(compiler: &Path) -> Result<Vec<u8>> {
    Ok(compiler_query(compiler, &["-vV"])?.stdout)
}

fn compiler(path: &Path) -> Result<Compiler> {
    ensure!(
        path.is_absolute(),
        "Cargo must supply an absolute actual compiler path"
    );
    // Preserve the actual argv0 path: canonicalizing a rustup rustc symlink
    // and invoking it as `rustup` changes its selected frontend semantics.
    let mut file = File::open(path)?;
    let before = identity(&file)?;
    let sha256 = file_digest(&mut file)?;
    let version = compiler_version(path)?;
    ensure!(
        identity(&file)? == before && identity(&File::open(path)?)? == before,
        "actual compiler changed during identity measurement"
    );
    Ok(Compiler {
        path: path.as_os_str().as_bytes().to_vec(),
        identity: before,
        sha256,
        verbose_version: version,
    })
}

fn directory_identity(path: &Path) -> Result<DirectoryIdentity> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path)?;
    let meta = file.metadata()?;
    ensure!(meta.is_dir(), "sysroot search is not a real directory");
    Ok(DirectoryIdentity {
        device: meta.dev(),
        inode: meta.ino(),
        mode: meta.mode(),
    })
}

fn proc_macro_candidates(directory: &Path) -> Result<Vec<PathBuf>> {
    let mut candidates = Vec::new();
    for (count, entry) in std::fs::read_dir(directory)?.enumerate() {
        ensure!(
            count < MAX_COMMANDS,
            "sysroot search inventory exceeds its finite bound"
        );
        let entry = entry?;
        if compatibility_rustc::proc_macro_artifact_name(entry.file_name().as_bytes())
            .map_err(anyhow::Error::msg)?
            .is_some()
        {
            candidates.push(entry.path());
        }
    }
    candidates.sort();
    Ok(candidates)
}

fn sysroot_artifact(path: &Path, remaining: u64) -> Result<SysrootArtifact> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let before = identity(&file)?;
    ensure!(
        before.bytes > 0 && before.bytes <= remaining,
        "sysroot artifact exceeds its finite byte bound"
    );
    let mut hash = Sha256::new();
    let mut count = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        count = count
            .checked_add(read as u64)
            .context("sysroot artifact byte overflow")?;
        ensure!(
            count <= remaining,
            "sysroot artifact grew beyond its finite byte bound"
        );
        hash.update(&buffer[..read]);
    }
    let current = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    ensure!(
        count == before.bytes
            && identity(&file)? == before
            && identity(&current)? == before
            && path.canonicalize()? == path,
        "sysroot artifact changed while captured"
    );
    Ok(SysrootArtifact {
        path: path.as_os_str().as_bytes().to_vec(),
        identity: before,
        sha256: format!("{:x}", hash.finalize()),
    })
}

/// The query is an actual bounded compiler metadata child in production.
/// Keeping resolution separate lets the control use real tiny files without
/// running a second compiler or inventing a sysroot for an old population.
fn sysroot_resolution(
    command: &Invocation,
    target: &Path,
    query: CompilerQuery,
) -> Result<Vec<SysrootExtern>> {
    let shape = compatibility_rustc::sysroot_extern_shape(command)
        .map_err(anyhow::Error::msg)?
        .context("sysroot resolution has no implicit extern")?;
    let directory = query
        .stdout
        .strip_suffix(b"\n")
        .filter(|value| !value.is_empty() && !value.contains(&b'\n') && !value.contains(&0))
        .context("compiler target-libdir output is not one exact Unix path")?;
    let directory = PathBuf::from(std::ffi::OsStr::from_bytes(directory));
    ensure!(
        directory.is_absolute(),
        "compiler target-libdir is not absolute"
    );
    let canonical = directory.canonicalize()?;
    let before = directory_identity(&canonical)?;
    let candidates = proc_macro_candidates(&canonical)?;
    ensure!(
        candidates.len() == 2,
        "compiler sysroot has no unique proc_macro artifact pair"
    );
    let mut remaining = MAX_CAPTURE_BYTES as u64;
    let mut artifacts = Vec::new();
    for candidate in &candidates {
        let artifact = sysroot_artifact(candidate, remaining)?;
        remaining -= artifact.identity.bytes;
        artifacts.push(artifact);
    }
    ensure!(
        directory.canonicalize()? == canonical
            && directory_identity(&canonical)? == before
            && proc_macro_candidates(&canonical)? == candidates,
        "compiler sysroot directory or candidate set changed while captured"
    );
    let cwd = Path::new(std::ffi::OsStr::from_bytes(&command.cwd));
    let mut searches = Vec::new();
    for (argument, raw) in &shape.searches {
        let name = Path::new(std::ffi::OsStr::from_bytes(raw));
        let directory = if name.is_absolute() {
            name.to_owned()
        } else {
            cwd.join(name)
        };
        let canonical = directory.canonicalize()?;
        ensure!(
            canonical.starts_with(target) && canonical != target,
            "implicit proc_macro dependency search escapes the actual fresh target"
        );
        let identity = directory_identity(&canonical)?;
        let candidates = proc_macro_candidates(&canonical)?;
        ensure!(
            candidates.is_empty(),
            "dependency search contains a proc_macro shadow"
        );
        ensure!(
            directory.canonicalize()? == canonical
                && directory_identity(&canonical)? == identity
                && proc_macro_candidates(&canonical)?.is_empty(),
            "implicit proc_macro dependency search changed while captured"
        );
        searches.push(SysrootSearch {
            argument: *argument,
            directory: directory.as_os_str().as_bytes().to_vec(),
            canonical_directory: canonical.as_os_str().as_bytes().to_vec(),
            identity,
            candidates: Vec::new(),
        });
    }
    let captures = shape
        .arguments
        .iter()
        .map(|argument| SysrootExtern {
            argument: *argument,
            name: b"proc_macro".to_vec(),
            compiler: command.compiler.clone(),
            host: shape.host.clone(),
            query: query.clone(),
            directory: directory.as_os_str().as_bytes().to_vec(),
            canonical_directory: canonical.as_os_str().as_bytes().to_vec(),
            identity: before.clone(),
            artifacts: artifacts.clone(),
            searches: searches.clone(),
        })
        .collect();
    let mut checked = command.clone();
    checked.sysroot_externs = captures;
    compatibility_rustc::validate_sysroot_externs(&checked, target).map_err(anyhow::Error::msg)?;
    Ok(checked.sysroot_externs)
}

fn sysroot_externs(command: &Invocation, target: &Path) -> Result<Vec<SysrootExtern>> {
    if compatibility_rustc::sysroot_extern_shape(command)
        .map_err(anyhow::Error::msg)?
        .is_none()
    {
        return Ok(Vec::new());
    }
    let path = Path::new(std::ffi::OsStr::from_bytes(&command.compiler.path));
    ensure!(
        compiler(path)? == command.compiler,
        "sysroot query compiler changed before resolution"
    );
    let query = compiler_query(path, &["--print", "target-libdir"])?;
    let captured = sysroot_resolution(command, target, query)?;
    ensure!(
        compiler(path)? == command.compiler,
        "sysroot query compiler changed after resolution"
    );
    Ok(captured)
}

fn response_files(argv: &[Vec<u8>], cwd: &Path) -> Result<Vec<ResponseFile>> {
    let mut result = Vec::new();
    for (argument, raw) in argv.iter().enumerate() {
        let Some(raw) = raw.strip_prefix(b"@") else {
            continue;
        };
        let name = Path::new(std::ffi::OsStr::from_bytes(raw));
        let path = if name.is_absolute() {
            name.to_owned()
        } else {
            cwd.join(name)
        };
        let before = identity(&File::open(&path)?)?;
        let bytes = read(&path, MAX_COMMAND_BYTES)?;
        ensure!(
            identity(&File::open(&path)?)? == before,
            "response file changed while captured"
        );
        result.push(ResponseFile {
            argument,
            path: path.as_os_str().as_bytes().to_vec(),
            identity: before,
            sha256: digest(&bytes),
            bytes,
        });
    }
    Ok(result)
}

fn gate(command: &Invocation, config: &Config, completion: Option<&Completion>) -> Result<()> {
    let Some(input) = std::env::var_os(GATE_ENV) else {
        return Ok(());
    };
    let input = BuildInput::load(Path::new(&input)).map_err(anyhow::Error::msg)?;
    let graph = Graph::read(&input).map_err(anyhow::Error::msg)?;
    let expected = Bundle::read(&input.rustc_second).map_err(anyhow::Error::msg)?;
    ensure!(
        expected.wrapper_sha256 == config.wrapper_sha256,
        "final compiler wrapper differs from the compared build producer"
    );
    let current = Bundle {
        schema: CAPTURE_SCHEMA.to_owned(),
        wrapper_sha256: config.wrapper_sha256.clone(),
        target_directory: config.target_directory.clone(),
        commands: Vec::new(),
    };
    if let Some(semantics) =
        compatibility_rustc::for_invocation_with_reference(&graph, &current, &expected, command)
            .map_err(anyhow::Error::msg)?
    {
        let expected =
            compatibility_rustc::shared(&graph, &expected).map_err(anyhow::Error::msg)?;
        ensure!(
            expected.contains_key(&serde_json::to_string(&semantics)?),
            "final actual rustc shared-unit semantics differ before compiler execution"
        );
    }
    if let Some(actual) =
        compatibility_rustc::auxiliary_for_invocation(&graph, &current, &expected, command)
            .map_err(anyhow::Error::msg)?
    {
        let known =
            compatibility_rustc::auxiliary(&graph, &expected).map_err(anyhow::Error::msg)?;
        let mut matched = false;
        for key in known.keys() {
            let (expected, exit_code, signal): (
                compatibility_rustc::Auxiliary,
                Option<i32>,
                Option<i32>,
            ) = serde_json::from_str(key)?;
            if expected == actual
                && completion.is_none_or(|end| end.exit_code == exit_code && end.signal == signal)
            {
                matched = true;
            }
        }
        ensure!(
            matched,
            "final auxiliary compiler inputs/options/outcome differ from the actual cold populations"
        );
    }
    Ok(())
}

fn invocation(
    config: &Config,
    compiler_path: &Path,
    args: &[OsString],
) -> Result<(Invocation, Option<File>)> {
    let cwd = std::env::current_dir()?;
    let argv: Vec<_> = args
        .iter()
        .map(|arg| arg.as_os_str().as_bytes().to_vec())
        .collect();
    let context = compatibility_rustc::Context {
        package: std::env::var("CARGO_PKG_NAME").ok(),
        version: std::env::var("CARGO_PKG_VERSION").ok(),
        manifest_directory: std::env::var_os("CARGO_MANIFEST_DIR")
            .map(|path| path.as_os_str().as_bytes().to_vec()),
    };
    let (source, replay) = source_input(&argv, &cwd)?;
    let mut command = Invocation {
        schema: CAPTURE_SCHEMA.to_owned(),
        pid: std::process::id(),
        compiler: compiler(compiler_path)?,
        cwd: cwd.as_os_str().as_bytes().to_vec(),
        response_files: response_files(&argv, &cwd)?,
        argv,
        context,
        source,
        sysroot_externs: Vec::new(),
    };
    command.sysroot_externs = sysroot_externs(&command, &config.target_directory)?;
    if let Some(source) = &command.source {
        source.validate(&command).map_err(anyhow::Error::msg)?;
    } else {
        ensure!(
            compatibility_rustc::compiler_probe(&command),
            "emitting compiler has no captured source"
        );
    }
    gate(&command, config, None)?;
    Ok((command, replay))
}

fn run(config_path: &Path) -> Result<ExitCode> {
    let config = config(config_path)?;
    let mut arguments = std::env::args_os().skip(1);
    let compiler = arguments
        .next()
        .context("RUSTC_WRAPPER did not receive the actual compiler")?;
    let arguments: Vec<_> = arguments.collect();
    let (command, mut replay) = invocation(&config, Path::new(&compiler), &arguments)?;
    let encoded = serde_json::to_vec(&command)?;
    ensure!(
        encoded.len() <= MAX_COMMAND_BYTES,
        "actual rustc command exceeds its capture bound"
    );
    reserve(&config, encoded.len() + COMPLETION_ALLOWANCE)?;
    let name = format!("rustc-{}", std::process::id());
    exclusive(
        &config.directory.join(format!("{name}.invocation.json")),
        &encoded,
    )?;
    // Only a declared rustc stdin input is replayed from the exact sealed
    // bytes above; argv/environment and the inherited stdout/stderr remain.
    let mut actual = Command::new(&compiler);
    actual.args(&arguments);
    if let Some(file) = &replay {
        actual.stdin(Stdio::from(file.try_clone()?));
    }
    let status = actual.status();
    let unchanged = (|| -> Result<bool> {
        Ok(
            response_files(&command.argv, &std::env::current_dir()?)? == command.response_files
                && source_unchanged(&command, &mut replay)?
                && self::compiler(Path::new(&compiler))? == command.compiler
                && sysroot_externs(&command, &config.target_directory)? == command.sysroot_externs,
        )
    })();
    let (exit_code, signal, mut capture_error) = match &status {
        Ok(status) => (status.code(), status.signal(), None),
        Err(error) => (
            None,
            None,
            Some(format!("actual compiler spawn failed: {error}")),
        ),
    };
    let inputs_unchanged = match unchanged {
        Ok(unchanged) => unchanged,
        Err(error) => {
            capture_error = Some(format!("compiler input recapture failed: {error:#}"));
            false
        }
    };
    let completion = Completion {
        schema: CAPTURE_SCHEMA.to_owned(),
        invocation_sha256: digest(&encoded),
        pid: std::process::id(),
        exit_code,
        signal,
        inputs_unchanged,
        capture_error,
    };
    let completion_bytes = serde_json::to_vec(&completion)?;
    ensure!(
        completion_bytes.len() <= COMPLETION_ALLOWANCE,
        "compiler completion exceeds its reserved capture bound"
    );
    exclusive(
        &config.directory.join(format!("{name}.completion.json")),
        &completion_bytes,
    )?;
    ensure!(
        completion.inputs_unchanged && completion.capture_error.is_none(),
        "actual compiler input/capture changed"
    );
    gate(&command, &config, Some(&completion))?;
    let status = status.context("actual compiler was not started")?;
    if let Some(signal) = status.signal() {
        // Preserve WIFSIGNALED after retaining the actual status. This is the
        // wrapper process itself, never a signal to another workload.
        unsafe {
            libc::signal(signal, libc::SIG_DFL);
            libc::raise(signal);
        }
        bail!("cannot reproduce actual compiler terminating signal {signal}");
    }
    Ok(ExitCode::from(u8::try_from(
        status.code().context("compiler has no exit status")?,
    )?))
}

/// Called before ordinary CLI parsing, only in Cargo's source-owned wrapper
/// environment. Unavailable capture is a compiler failure, never a stamp.
pub fn maybe_run() -> Option<ExitCode> {
    let path = std::env::var_os(CAPTURE_ENV)?;
    Some(match run(Path::new(&path)) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("actual rustc capture refused: {error:#}");
            ExitCode::from(2)
        }
    })
}

pub fn bundle(config_path: &Path) -> Result<Bundle> {
    let config = config(config_path)?;
    let mut invocations = std::collections::BTreeMap::new();
    let mut completions = std::collections::BTreeMap::new();
    let mut entries = 0;
    for entry in std::fs::read_dir(&config.directory)? {
        entries += 1;
        ensure!(
            entries <= MAX_COMMANDS * 2 + 2,
            "capture directory exceeds its entry bound"
        );
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_str().context("capture filename is non-UTF8")?;
        if matches!(name, "config.json" | "quota") {
            continue;
        }
        if let Some(name) = name.strip_suffix(".invocation.json") {
            ensure!(
                invocations
                    .insert(
                        name.to_owned(),
                        serde_json::from_slice::<Invocation>(&read(
                            &entry.path(),
                            MAX_COMMAND_BYTES
                        )?)?
                    )
                    .is_none(),
                "capture repeats an invocation"
            );
        } else if let Some(name) = name.strip_suffix(".completion.json") {
            ensure!(
                completions
                    .insert(
                        name.to_owned(),
                        serde_json::from_slice::<Completion>(&read(
                            &entry.path(),
                            COMPLETION_ALLOWANCE
                        )?)?
                    )
                    .is_none(),
                "capture repeats a completion"
            );
        } else {
            bail!("unexpected file in private rustc capture: {name}");
        }
    }
    ensure!(
        invocations.len() == completions.len() && !invocations.is_empty(),
        "capture contains an unfinished actual compiler command"
    );
    let commands = invocations
        .into_iter()
        .map(|(name, invocation)| {
            Ok(Captured {
                invocation,
                completion: completions
                    .remove(&name)
                    .context("compiler completion is missing")?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    ensure!(
        completions.is_empty(),
        "capture contains an unbound compiler completion"
    );
    Ok(Bundle {
        schema: CAPTURE_SCHEMA.to_owned(),
        wrapper_sha256: config.wrapper_sha256,
        target_directory: config.target_directory,
        commands,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct QuotaFixture {
        root: PathBuf,
        config: Config,
    }

    impl QuotaFixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "hermit-rustc-quota-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir(&root).unwrap();
            let path = prepare(&root.join("capture"), &root.join("target")).unwrap();
            let config = config(&path).unwrap();
            Self { root, config }
        }

        fn counters(&self) -> [u64; 2] {
            let bytes = std::fs::read(self.config.directory.join("quota")).unwrap();
            assert_eq!(bytes.len(), 16);
            [
                u64::from_le_bytes(bytes[..8].try_into().unwrap()),
                u64::from_le_bytes(bytes[8..].try_into().unwrap()),
            ]
        }

        fn set_counters(&self, bytes: u64, commands: u64) {
            let mut quota = OpenOptions::new()
                .write(true)
                .open(self.config.directory.join("quota"))
                .unwrap();
            quota.write_all(&bytes.to_le_bytes()).unwrap();
            quota.write_all(&commands.to_le_bytes()).unwrap();
            quota.sync_data().unwrap();
        }
    }

    impl Drop for QuotaFixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn implicit_proc_macro_capture_binds_one_host_sysroot_and_refuses_shadow_or_change() {
        let fixture = QuotaFixture::new();
        let libdir = fixture
            .root
            .join("toolchain/lib/rustlib/x86_64-unknown-linux-gnu/lib");
        let deps = fixture.config.target_directory.join("debug/deps");
        std::fs::create_dir_all(&libdir).unwrap();
        std::fs::create_dir_all(&deps).unwrap();
        let rlib = libdir.join("libproc_macro-0123456789abcdef.rlib");
        let rmeta = libdir.join("libproc_macro-0123456789abcdef.rmeta");
        std::fs::write(&rlib, b"real fixture rlib bytes").unwrap();
        std::fs::write(&rmeta, b"real fixture rmeta bytes").unwrap();
        let source = fixture.root.join("fixture.rs");
        std::fs::write(&source, b"// real fixture source\n").unwrap();
        let compiler_path = fixture.root.join("fixture-rustc");
        std::fs::write(&compiler_path, b"fixture identity, never executed").unwrap();
        let compiler = Compiler {
            path: compiler_path.as_os_str().as_bytes().to_vec(),
            identity: identity(&File::open(&compiler_path).unwrap()).unwrap(),
            sha256: digest(&std::fs::read(&compiler_path).unwrap()),
            verbose_version: format!(
                "rustc fixture\ncommit-hash: {}\nhost: x86_64-unknown-linux-gnu\n",
                "a".repeat(40)
            )
            .into_bytes(),
        };
        let command = Invocation {
            schema: CAPTURE_SCHEMA.to_owned(),
            pid: 1,
            compiler,
            cwd: fixture.root.as_os_str().as_bytes().to_vec(),
            argv: vec![
                b"--crate-name".to_vec(),
                b"fixture".to_vec(),
                source.as_os_str().as_bytes().to_vec(),
                b"--crate-type".to_vec(),
                b"proc-macro".to_vec(),
                b"--out-dir".to_vec(),
                deps.as_os_str().as_bytes().to_vec(),
                b"-L".to_vec(),
                [b"dependency=".as_slice(), deps.as_os_str().as_bytes()].concat(),
                b"--extern".to_vec(),
                b"proc_macro".to_vec(),
            ],
            context: compatibility_rustc::Context {
                package: None,
                version: None,
                manifest_directory: None,
            },
            response_files: Vec::new(),
            source: None,
            sysroot_externs: Vec::new(),
        };
        let query = CompilerQuery {
            argv: vec![b"--print".to_vec(), b"target-libdir".to_vec()],
            stdout: [libdir.as_os_str().as_bytes(), b"\n"].concat(),
            stderr: Vec::new(),
            exit_code: 0,
        };
        let resolve = |command: &Invocation| {
            sysroot_resolution(command, &fixture.config.target_directory, query.clone())
        };
        let captured = resolve(&command).unwrap();
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0].argument, 9);
        assert_eq!(captured[0].artifacts.len(), 2);
        assert_eq!(captured[0].artifacts[0].identity.bytes, 23);
        assert!(captured[0].searches[0].candidates.is_empty());
        assert_eq!(resolve(&command).unwrap(), captured);

        std::fs::write(
            deps.join("libproc_macro2-0123456789abcdef.rlib"),
            b"a different crate",
        )
        .unwrap();
        assert_eq!(resolve(&command).unwrap(), captured);
        std::fs::write(&rlib, b"changed actual fixture bytes").unwrap();
        assert_ne!(
            resolve(&command).unwrap(),
            captured,
            "post-run recapture must detect changed artifact bytes/identity"
        );
        std::fs::write(&rlib, b"real fixture rlib bytes").unwrap();

        let shadow = deps.join("libproc_macro-0123456789abcdef.rlib");
        std::fs::write(&shadow, b"real shadow").unwrap();
        assert!(
            resolve(&command).is_err(),
            "a competing dependency artifact must refuse"
        );
        std::fs::remove_file(shadow).unwrap();
        let other = fixture.root.join("other.rlib");
        std::fs::write(&other, b"symlink target").unwrap();
        std::fs::remove_file(&rlib).unwrap();
        std::os::unix::fs::symlink(&other, &rlib).unwrap();
        assert!(
            resolve(&command).is_err(),
            "a sysroot artifact symlink must refuse"
        );
        std::fs::remove_file(&rlib).unwrap();
        std::fs::write(&rlib, b"real fixture rlib bytes").unwrap();
        std::fs::remove_file(&rmeta).unwrap();
        assert!(
            resolve(&command).is_err(),
            "both exact artifact kinds are required"
        );
        std::fs::write(&rmeta, b"real fixture rmeta bytes").unwrap();
        let mismatched = libdir.join("libproc_macro-fedcba9876543210.rmeta");
        std::fs::rename(&rmeta, &mismatched).unwrap();
        assert!(
            resolve(&command).is_err(),
            "rlib and rmeta stems must be equal"
        );
        std::fs::rename(&mismatched, &rmeta).unwrap();
        for suffix in ["rlib", "rmeta"] {
            std::fs::write(
                libdir.join(format!("libproc_macro-fedcba9876543210.{suffix}")),
                b"another stem",
            )
            .unwrap();
        }
        assert!(
            resolve(&command).is_err(),
            "multiple matching stems must refuse"
        );
        for suffix in ["rlib", "rmeta"] {
            std::fs::remove_file(libdir.join(format!("libproc_macro-fedcba9876543210.{suffix}")))
                .unwrap();
        }
        let unknown = libdir.join("libproc_macro-0123456789abcdef.so");
        std::fs::write(&unknown, b"unknown matching form").unwrap();
        assert!(resolve(&command).is_err());
        std::fs::remove_file(unknown).unwrap();
        for extra in [
            "--target=x86_64-unknown-linux-gnu",
            "--sysroot=/other",
            "-Lnative=/other",
        ] {
            let mut overridden = command.clone();
            overridden.argv.push(extra.as_bytes().to_vec());
            assert!(
                resolve(&overridden).is_err(),
                "unmeasured override {extra} must refuse"
            );
        }
        let mut unknown = command.clone();
        unknown.argv[10] = b"unknown_bare_crate".to_vec();
        assert!(resolve(&unknown).is_err());
        let mut wrong_query = query;
        wrong_query.exit_code = 1;
        assert!(
            sysroot_resolution(&command, &fixture.config.target_directory, wrong_query).is_err()
        );
    }

    #[test]
    fn quota_reservation_progresses_while_another_checked_flush_is_held() {
        use std::sync::Arc;
        use std::sync::atomic::AtomicBool;
        use std::sync::atomic::Ordering;
        use std::sync::mpsc;

        let fixture = QuotaFixture::new();
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let first_returned = Arc::new(AtomicBool::new(false));
        let observed_return = Arc::clone(&first_returned);
        let first_config = fixture.config.clone();
        let first = std::thread::spawn(move || {
            let result = reserve_with_sync(&first_config, 101, |quota| {
                entered_tx.send(()).map_err(std::io::Error::other)?;
                release_rx
                    .recv_timeout(Duration::from_secs(10))
                    .map_err(std::io::Error::other)?;
                quota.sync_data()
            });
            observed_return.store(true, Ordering::SeqCst);
            result
        });
        let entered = entered_rx.recv_timeout(Duration::from_secs(5));
        if let Err(error) = entered {
            let _ = release_tx.send(());
            let result = first.join().unwrap();
            panic!("first reservation did not reach its flush: {error}; {result:?}");
        }

        let (second_tx, second_rx) = mpsc::channel();
        let second_config = fixture.config.clone();
        let second = std::thread::spawn(move || {
            let result = reserve(&second_config, 203);
            let _ = second_tx.send(result);
        });
        let second_result = second_rx.recv_timeout(Duration::from_secs(5));
        let returned_before_release = first_returned.load(Ordering::SeqCst);
        let _ = release_tx.send(());
        let first_result = first.join().unwrap();
        second.join().unwrap();

        assert!(
            !returned_before_release,
            "reservation returned before checked flush"
        );
        second_result
            .expect("second real reservation must finish while the first flush is held")
            .expect("second real reservation must retain the unchanged acquisition bound");
        first_result.expect("first checked flush must complete before reserve returns");
        assert_eq!(fixture.counters(), [304, 2]);
    }

    #[test]
    fn quota_refuses_existing_limits_and_keeps_failed_flush_reserved() {
        let fixture = QuotaFixture::new();
        fixture.set_counters(MAX_CAPTURE_BYTES as u64 - 1, 0);
        reserve(&fixture.config, 1).unwrap();
        assert_eq!(fixture.counters(), [MAX_CAPTURE_BYTES as u64, 1]);
        let error = reserve(&fixture.config, 1).unwrap_err();
        assert!(error.to_string().contains("finite byte/command bound"));
        assert_eq!(fixture.counters(), [MAX_CAPTURE_BYTES as u64, 1]);

        fixture.set_counters(0, MAX_COMMANDS as u64 - 1);
        reserve(&fixture.config, 1).unwrap();
        assert_eq!(fixture.counters(), [1, MAX_COMMANDS as u64]);
        let error = reserve(&fixture.config, 0).unwrap_err();
        assert!(error.to_string().contains("finite byte/command bound"));
        assert_eq!(fixture.counters(), [1, MAX_COMMANDS as u64]);

        fixture.set_counters(u64::MAX, 0);
        let error = reserve(&fixture.config, 1).unwrap_err();
        assert!(error.to_string().contains("capture quota overflow"));
        assert_eq!(fixture.counters(), [u64::MAX, 0]);

        fixture.set_counters(0, 0);
        let error = reserve_with_sync(&fixture.config, 17, |_| {
            Err(std::io::Error::other("deliberate quota flush refusal"))
        })
        .unwrap_err();
        assert!(error.to_string().contains("deliberate quota flush refusal"));
        assert_eq!(fixture.counters(), [17, 1]);
    }

    #[test]
    fn compiler_stdin_replay_is_exact_and_sealed() {
        let expected = b"// compiler input\nfn main() {}\0\xff";
        let mut replay = sealed_stdin(expected).expect("seal actual compiler input");
        let mut actual = Vec::new();
        replay.read_to_end(&mut actual).unwrap();
        assert_eq!(actual, expected);
        assert_eq!(
            unsafe { libc::fcntl(replay.as_raw_fd(), libc::F_GET_SEALS) },
            INPUT_SEALS
        );
        assert_eq!(
            replay.write(b"changed").unwrap_err().raw_os_error(),
            Some(libc::EPERM)
        );
        assert_eq!(
            replay.set_len(0).unwrap_err().raw_os_error(),
            Some(libc::EPERM)
        );
        replay.seek(SeekFrom::Start(0)).unwrap();
        let mut second = Vec::new();
        replay.read_to_end(&mut second).unwrap();
        assert_eq!(second, expected);
    }
}
