/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Declared filesystem data for a native exec domain. This module deliberately
//! cannot construct an InstalledExecDomain or activate a native runtime.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::ffi::CString;
use std::ffi::OsStr;
use std::fs::File;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::fd::BorrowedFd;
use std::os::fd::FromRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use serde::Deserialize;
use sha2::Digest;
use sha2::Sha256;

const MAX_DECLARATION: usize = 1024 * 1024;
const MAX_OBJECTS: usize = 256;
const MAX_ALIASES: usize = 256;
const MAX_PATH: usize = 4095;
const MAX_FILE: u64 = 64 * 1024 * 1024;
const MAX_TOTAL: u64 = 256 * 1024 * 1024;
const MAX_ALIAS_HOPS: usize = 40;
const F_SEAL_EXEC: i32 = 0x0020;
const REQUIRED_SEALS: i32 = libc::F_SEAL_WRITE
    | libc::F_SEAL_GROW
    | libc::F_SEAL_SHRINK
    | libc::F_SEAL_SEAL
    | libc::F_SEAL_FUTURE_WRITE
    | F_SEAL_EXEC;
// MFD_EXEC is required explicitly: a host policy that only permits noexec
// memfds is unsupported, rather than silently furnishing a nonexecutable file.
const MFD_EXEC: u32 = 0x0010;

/// A literal absolute Unix path. Invalid spellings refuse instead of being
/// silently normalized; symlink target bytes have separate Linux semantics.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(super) struct GuestPath(Vec<u8>);

impl GuestPath {
    pub(super) fn new(bytes: Vec<u8>) -> Result<Self> {
        if bytes.len() > MAX_PATH
            || bytes.first() != Some(&b'/')
            || bytes.contains(&0)
            || bytes == b"/"
            || bytes.ends_with(b"/")
            || bytes[1..]
                .split(|byte| *byte == b'/')
                .any(|part| part.is_empty() || part == b"." || part == b"..")
        {
            bail!("unsupported declared guest path");
        }
        Ok(Self(bytes))
    }

    pub(super) fn as_bytes(&self) -> &[u8] {
        &self.0
    }
    pub(super) fn as_path(&self) -> &Path {
        Path::new(OsStr::from_bytes(&self.0))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum InputRole {
    Executable,
    Loader,
    Data,
}

#[derive(Clone, Debug)]
pub(super) struct DeclaredAlias {
    pub(super) path: GuestPath,
    pub(super) target: Vec<u8>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Declaration {
    version: u32,
    executables: Vec<Vec<u8>>,
    loader_files: Vec<Vec<u8>>,
    data_files: Vec<Vec<u8>>,
    aliases: Vec<AliasDeclaration>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AliasDeclaration {
    path: Vec<u8>,
    target: Vec<u8>,
}

/// Data-only enrollment. No transition table, backend name or execution grant.
#[derive(Debug)]
pub(super) struct DynamicExecInputs {
    files: BTreeMap<GuestPath, InputRole>,
    aliases: BTreeMap<GuestPath, Vec<u8>>,
}

impl DynamicExecInputs {
    pub(super) fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_DECLARATION {
            bail!("declared input document exceeds bound");
        }
        let declaration: Declaration = serde_json::from_slice(bytes)?;
        if declaration.version != 1 {
            bail!("unsupported declared input version");
        }
        let count = declaration
            .executables
            .len()
            .checked_add(declaration.loader_files.len())
            .and_then(|count| count.checked_add(declaration.data_files.len()));
        if declaration.executables.is_empty()
            || count.is_none_or(|count| count > MAX_OBJECTS)
            || declaration.aliases.len() > MAX_ALIASES
        {
            bail!("declared input object count exceeds domain");
        }
        let mut files = BTreeMap::new();
        for (role, paths) in [
            (InputRole::Executable, declaration.executables),
            (InputRole::Loader, declaration.loader_files),
            (InputRole::Data, declaration.data_files),
        ] {
            for path in paths {
                if files.insert(GuestPath::new(path)?, role).is_some() {
                    bail!("duplicate declared file path or role");
                }
            }
        }
        let mut aliases = BTreeMap::new();
        for alias in declaration.aliases {
            let path = GuestPath::new(alias.path)?;
            if alias.target.is_empty()
                || alias.target.len() > MAX_PATH
                || alias.target.contains(&0)
                || files.contains_key(&path)
                || aliases.insert(path, alias.target).is_some()
            {
                bail!("invalid or duplicate declared alias");
            }
        }
        let inputs = Self { files, aliases };
        // Resolve every endpoint before opening anything. Parent/file/alias
        // collisions and cycles must not be left to mount ordering.
        let mut resolved_files = BTreeSet::new();
        for path in inputs.files.keys() {
            let resolved = inputs.resolve(path.as_bytes(), true)?;
            if !resolved_files.insert(resolved) {
                bail!("declared files resolve to the same path");
            }
        }
        let mut endpoints = resolved_files;
        for path in inputs.aliases.keys() {
            let resolved = inputs.resolve(path.as_bytes(), false)?;
            if !endpoints.insert(resolved) {
                bail!("declared endpoint collision");
            }
            inputs.resolve(path.as_bytes(), true)?;
        }
        for path in &endpoints {
            if endpoints.iter().any(|other| {
                other.len() > path.len() && other.starts_with(path) && other[path.len()] == b'/'
            }) {
                bail!("declared file or alias shadows an endpoint parent");
            }
        }
        Ok(inputs)
    }

    /// Resolve ONLY explicitly declared links, retaining Linux relative-link
    /// meaning and refusing root escape. Unknown physical links are refused by
    /// openat2 later. `follow_final=false` identifies the link object itself.
    fn resolve(&self, bytes: &[u8], follow_final: bool) -> Result<Vec<u8>> {
        let mut pending: std::collections::VecDeque<Vec<u8>> = bytes
            .split(|b| *b == b'/')
            .filter(|part| !part.is_empty())
            .map(<[u8]>::to_vec)
            .collect();
        let mut components: Vec<Vec<u8>> = Vec::new();
        let mut hops = 0;
        while let Some(part) = pending.pop_front() {
            if part == b"." {
                continue;
            }
            if part == b".." {
                if components.pop().is_none() {
                    bail!("declared alias escapes root");
                }
                continue;
            }
            components.push(part);
            let current = joined(&components);
            if (follow_final || !pending.is_empty())
                && let Some(target) = self.aliases.get(&GuestPath(current))
            {
                hops += 1;
                if hops > MAX_ALIAS_HOPS {
                    bail!("declared alias cycle or hop limit");
                }
                components.pop();
                if target.first() == Some(&b'/') {
                    components.clear();
                }
                for part in target
                    .split(|b| *b == b'/')
                    .filter(|part| !part.is_empty())
                    .rev()
                {
                    pending.push_front(part.to_vec());
                }
            }
            if joined(&components).len() > MAX_PATH {
                bail!("resolved alias path exceeds bound");
            }
        }
        let result = joined(&components);
        if result == b"/" {
            bail!("declared endpoint resolves to root");
        }
        Ok(result)
    }
}

fn joined(components: &[Vec<u8>]) -> Vec<u8> {
    let mut result = Vec::new();
    for component in components {
        result.push(b'/');
        result.extend_from_slice(component);
    }
    if result.is_empty() {
        result.push(b'/');
    }
    result
}

/// Pins the selected input root. Opening a path under this descriptor never
/// follows an undeclared symlink, magic link, or mount-independent host root.
pub(super) struct OpenedGuestRoot(File);

#[repr(C)]
struct OpenHow {
    flags: u64,
    mode: u64,
    resolve: u64,
}

impl OpenedGuestRoot {
    pub(super) fn open(path: &Path) -> Result<Self> {
        let path = CString::new(path.as_os_str().as_bytes())?;
        // SAFETY: path is a live C string; a successful open transfers one FD.
        let fd = unsafe {
            libc::open(
                path.as_ptr(),
                libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(Self(unsafe { File::from_raw_fd(fd) }))
    }

    fn open_regular(&self, path: &[u8]) -> Result<File> {
        // Inspect with O_PATH first: opening a FIFO/device for read merely to
        // reject it can block or invoke a driver before the kind check.
        let object = self.open_relative(path, libc::O_PATH)?;
        let expected = object.metadata()?;
        if !expected.is_file() {
            bail!("declared input is not a regular file");
        }
        let actual = File::open(format!("/proc/self/fd/{}", object.as_raw_fd()))?;
        let found = actual.metadata()?;
        if found.dev() != expected.dev() || found.ino() != expected.ino() || !found.is_file() {
            bail!("opened declared object identity changed");
        }
        Ok(actual)
    }

    fn open_relative(&self, path: &[u8], flags: i32) -> Result<File> {
        let path = CString::new(&path[1..])?;
        let how = OpenHow {
            flags: (flags | libc::O_CLOEXEC | libc::O_NOFOLLOW) as u64,
            mode: 0,
            resolve: 0x10 | 0x04,
        }; // RESOLVE_IN_ROOT | RESOLVE_NO_SYMLINKS
        // SAFETY: root owns its FD; path and how are live bounded input buffers.
        let fd = unsafe {
            libc::syscall(
                libc::SYS_openat2,
                self.0.as_raw_fd(),
                path.as_ptr(),
                &how,
                std::mem::size_of::<OpenHow>(),
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(unsafe { File::from_raw_fd(fd as i32) })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct CapturedIdentity {
    pub(super) device: u64,
    pub(super) inode: u64,
    pub(super) size: u64,
    pub(super) digest: [u8; 32],
}

/// Retained immutable object. The borrowed FD never grants mutable ownership.
pub(super) struct HeldInputFile {
    pub(super) path: GuestPath,
    pub(super) resolved_path: GuestPath,
    pub(super) role: InputRole,
    pub(super) identity: CapturedIdentity,
    file: File,
    pub(super) interpreter: Option<GuestPath>,
    pub(super) needed: Vec<Vec<u8>>,
}

impl HeldInputFile {
    pub(super) fn descriptor(&self) -> BorrowedFd<'_> {
        use std::os::fd::AsFd;
        self.file.as_fd()
    }
    pub(super) fn read_bytes(&self) -> Result<Vec<u8>> {
        use std::os::unix::fs::FileExt;
        let mut bytes = vec![0; usize::try_from(self.identity.size)?];
        self.file.read_exact_at(&mut bytes, 0)?;
        Ok(bytes)
    }
}

/// Hold one instance across both verify runs; creating a new capture per run
/// would admit input changes. It contains filesystem data, not launch proof.
pub(super) struct HeldDynamicInputs {
    files: Vec<HeldInputFile>,
    aliases: Vec<DeclaredAlias>,
}

impl HeldDynamicInputs {
    #[cfg(test)]
    pub(super) fn capture_data_control(root: &OpenedGuestRoot, path: GuestPath) -> Result<Self> {
        capture_inputs(
            root,
            DynamicExecInputs {
                files: [(path, InputRole::Data)].into(),
                aliases: BTreeMap::new(),
            },
        )
    }
    pub(super) fn files(&self) -> &[HeldInputFile] {
        &self.files
    }
    pub(super) fn aliases(&self) -> &[DeclaredAlias] {
        &self.aliases
    }
    pub(super) fn require_initial(&self, path: &GuestPath) -> Result<&HeldInputFile> {
        self.files
            .iter()
            .find(|file| file.path == *path && file.role == InputRole::Executable)
            .context("initial executable is not declared")
    }
    pub(super) fn resolve_path(&self, path: &GuestPath) -> Result<GuestPath> {
        let aliases = self
            .aliases
            .iter()
            .map(|alias| (alias.path.clone(), alias.target.clone()))
            .collect();
        let inputs = DynamicExecInputs {
            files: BTreeMap::new(),
            aliases,
        };
        Ok(GuestPath(inputs.resolve(path.as_bytes(), true)?))
    }
    /// Actual interpreter placements after resolving the captured alias graph.
    /// Literal replacement declarations remain distinct from these paths.
    pub(super) fn resolved_interpreters(&self) -> Result<BTreeSet<GuestPath>> {
        self.files
            .iter()
            .filter(|file| file.role == InputRole::Executable)
            .map(|file| {
                self.resolve_path(
                    file.interpreter
                        .as_ref()
                        .context("native target lacks interpreter placement")?,
                )
            })
            .collect()
    }
    pub(super) fn is_interpreter(&self, path: &GuestPath) -> bool {
        self.files
            .iter()
            .any(|file| file.interpreter.as_ref() == Some(path))
    }
    pub(super) fn recheck(&self) -> Result<()> {
        for file in &self.files {
            let metadata = file.file.metadata()?;
            if metadata.dev() != file.identity.device
                || metadata.ino() != file.identity.inode
                || metadata.len() != file.identity.size
                || unsafe { libc::fcntl(file.file.as_raw_fd(), libc::F_GET_SEALS) }
                    != REQUIRED_SEALS
            {
                bail!("held input object or exact seals changed");
            }
            if <[u8; 32]>::from(Sha256::digest(file.read_bytes()?)) != file.identity.digest {
                bail!("held input bytes changed");
            }
        }
        Ok(())
    }
}

pub(super) fn capture_inputs(
    root: &OpenedGuestRoot,
    inputs: DynamicExecInputs,
) -> Result<HeldDynamicInputs> {
    let mut aliases = Vec::new();
    for (path, target) in &inputs.aliases {
        let resolved = inputs.resolve(path.as_bytes(), false)?;
        let link = root.open_relative(&resolved, libc::O_PATH)?;
        if !link.metadata()?.file_type().is_symlink() {
            bail!("declared alias is not a symlink");
        }
        let mut actual = [0; MAX_PATH + 1];
        // SAFETY: empty-path readlinkat reads the actual held symlink itself.
        let used = unsafe {
            libc::readlinkat(
                link.as_raw_fd(),
                c"".as_ptr(),
                actual.as_mut_ptr().cast(),
                actual.len(),
            )
        };
        if used < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        if &actual[..used as usize] != target {
            bail!("declared alias bytes differ from actual root");
        }
        aliases.push(DeclaredAlias {
            path: GuestPath(resolved),
            target: target.clone(),
        });
    }
    let mut files = Vec::new();
    let mut total = 0u64;
    for (path, role) in &inputs.files {
        let resolved = inputs.resolve(path.as_bytes(), true)?;
        let mut source = root.open_regular(&resolved)?;
        let before = source.metadata()?;
        if !before.is_file()
            || before.len() > MAX_FILE
            || before.mode() & 0o6000 != 0
            || (*role == InputRole::Executable && before.mode() & 0o111 == 0)
        {
            bail!("unsupported declared source kind, mode or size");
        }
        // No set-id/file-capability input can be converted into ordinary exec.
        let capability = unsafe {
            libc::fgetxattr(
                source.as_raw_fd(),
                c"security.capability".as_ptr(),
                std::ptr::null_mut(),
                0,
            )
        };
        if capability >= 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ENODATA)
        {
            bail!("file capabilities are present or could not be checked");
        }
        total = total
            .checked_add(before.len())
            .context("captured size overflow")?;
        if total > MAX_TOTAL {
            bail!("captured input bytes exceed domain bound");
        }
        let mut bytes = Vec::new();
        (&mut source)
            .take(before.len() + 1)
            .read_to_end(&mut bytes)?;
        let after = source.metadata()?;
        if bytes.len() as u64 != before.len()
            || before.len() != after.len()
            || before.mtime() != after.mtime()
            || before.mtime_nsec() != after.mtime_nsec()
            || before.ctime() != after.ctime()
            || before.ctime_nsec() != after.ctime_nsec()
        {
            bail!("declared source changed during capture");
        }
        let (interpreter, needed) = if *role == InputRole::Data {
            (None, Vec::new())
        } else {
            elf_requirements(&bytes, *role)?
        };
        let file = seal_bytes(&bytes, *role)?;
        let metadata = file.metadata()?;
        files.push(HeldInputFile {
            path: path.clone(),
            resolved_path: GuestPath(resolved),
            role: *role,
            identity: CapturedIdentity {
                device: metadata.dev(),
                inode: metadata.ino(),
                size: metadata.len(),
                digest: Sha256::digest(&bytes).into(),
            },
            file,
            interpreter,
            needed,
        });
    }
    finish_held(inputs, files, aliases)
}

fn finish_held(
    inputs: DynamicExecInputs,
    files: Vec<HeldInputFile>,
    aliases: Vec<DeclaredAlias>,
) -> Result<HeldDynamicInputs> {
    // Closed root guarantees that a loader search can only find enrolled
    // objects or stable absence. This is dependency closure, not a claim about
    // a particular stock loader's search ordering or InstalledExecDomain.
    for file in &files {
        if let Some(interpreter) = &file.interpreter {
            let resolved = inputs.resolve(interpreter.as_bytes(), true)?;
            if !files.iter().any(|candidate| {
                candidate.resolved_path.as_bytes() == resolved
                    && candidate.role == InputRole::Loader
            }) {
                bail!("PT_INTERP is not an enrolled loader object");
            }
        }
        for needed in &file.needed {
            let mut represented = files.iter().any(|candidate| {
                candidate.role == InputRole::Loader
                    && candidate
                        .path
                        .as_bytes()
                        .rsplit(|byte| *byte == b'/')
                        .next()
                        == Some(needed.as_slice())
            });
            for alias in inputs.aliases.keys() {
                if alias.as_bytes().rsplit(|byte| *byte == b'/').next() == Some(needed.as_slice()) {
                    let resolved = inputs.resolve(alias.as_bytes(), true)?;
                    represented |= files.iter().any(|candidate| {
                        candidate.role == InputRole::Loader
                            && candidate.resolved_path.as_bytes() == resolved
                    });
                }
            }
            if !represented {
                bail!("DT_NEEDED is not represented in the closed loader graph");
            }
        }
    }
    let held = HeldDynamicInputs { files, aliases };
    held.recheck()?;
    Ok(held)
}

/// Read a retained native object without reopening any name or changing its
/// shared open-file-description offset. The native policy is stronger than
/// the generic manifest's minimum seals.
pub(super) fn retained_bytes(file: &File, role: InputRole, bound: u64) -> Result<Vec<u8>> {
    use std::os::unix::fs::FileExt;
    let metadata = file.metadata()?;
    let mode = if role == InputRole::Data {
        0o444
    } else {
        0o555
    };
    if !metadata.is_file()
        || metadata.len() > bound
        || metadata.mode() & 0o7777 != mode
        || unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GET_SEALS) } != REQUIRED_SEALS
        || unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFD) } != libc::FD_CLOEXEC
    {
        bail!("adopted native object kind, mode, size, seals or descriptor flags differ");
    }
    let capability = unsafe {
        libc::fgetxattr(
            file.as_raw_fd(),
            c"security.capability".as_ptr(),
            std::ptr::null_mut(),
            0,
        )
    };
    if capability >= 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ENODATA) {
        bail!("adopted native file capabilities are present or could not be checked");
    }
    let mut bytes = vec![0; usize::try_from(metadata.len())?];
    file.read_exact_at(&mut bytes, 0)?;
    Ok(bytes)
}

/// Restore the original declaration/alias graph with the same sealed inodes.
/// No source path, symlink or build output is opened or resealed here.
pub(super) fn adopt_inputs(
    inputs: DynamicExecInputs,
    objects: Vec<(GuestPath, InputRole, File)>,
) -> Result<HeldDynamicInputs> {
    if objects.len() != inputs.files.len() {
        bail!("adopted native input count differs");
    }
    let mut seen = BTreeSet::new();
    let mut files = Vec::new();
    let mut total = 0u64;
    for (path, role, file) in objects {
        if inputs.files.get(&path) != Some(&role) || !seen.insert(path.clone()) {
            bail!("adopted native input path/role differs or duplicates");
        }
        let bytes = retained_bytes(&file, role, MAX_FILE)?;
        total = total
            .checked_add(bytes.len() as u64)
            .context("adopted size overflow")?;
        if total > MAX_TOTAL {
            bail!("adopted input bytes exceed domain bound");
        }
        let (interpreter, needed) = if role == InputRole::Data {
            (None, Vec::new())
        } else {
            elf_requirements(&bytes, role)?
        };
        let metadata = file.metadata()?;
        files.push(HeldInputFile {
            resolved_path: GuestPath(inputs.resolve(path.as_bytes(), true)?),
            path,
            role,
            identity: CapturedIdentity {
                device: metadata.dev(),
                inode: metadata.ino(),
                size: metadata.len(),
                digest: Sha256::digest(&bytes).into(),
            },
            file,
            interpreter,
            needed,
        });
    }
    // Match capture's deterministic declared-path order regardless of manifest
    // row order; alias placements use the original sealed declaration bytes.
    files.sort_by(|a, b| a.path.cmp(&b.path));
    let aliases = inputs
        .aliases
        .iter()
        .map(|(path, target)| {
            Ok(DeclaredAlias {
                path: GuestPath(inputs.resolve(path.as_bytes(), false)?),
                target: target.clone(),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    finish_held(inputs, files, aliases)
}

fn elf_requirements(bytes: &[u8], role: InputRole) -> Result<(Option<GuestPath>, Vec<Vec<u8>>)> {
    let elf = goblin::elf::Elf::parse(bytes).context("declared executable/loader is not ELF")?;
    if !elf.is_64
        || !elf.little_endian
        || elf.header.e_machine != goblin::elf::header::EM_X86_64
        || ![goblin::elf::header::ET_EXEC, goblin::elf::header::ET_DYN].contains(&elf.header.e_type)
        || elf.dynamic.is_none()
        || !elf.rpaths.is_empty()
        || !elf.runpaths.is_empty()
    {
        bail!("unsupported native dynamic ELF or loader search policy");
    }
    let interpreter = elf
        .interpreter
        .map(|path| GuestPath::new(path.as_bytes().to_vec()))
        .transpose()?;
    if role == InputRole::Executable && interpreter.is_none() {
        bail!("static executable unsupported");
    }
    let mut needed = Vec::new();
    for library in elf.libraries {
        if library.is_empty() || library.contains('/') {
            bail!("unsupported DT_NEEDED path");
        }
        needed.push(library.as_bytes().to_vec());
    }
    Ok((interpreter, needed))
}

pub(super) fn seal_bytes(bytes: &[u8], role: InputRole) -> Result<File> {
    // SAFETY: creates a new owned anonymous file with explicit executable policy.
    let fd = unsafe {
        libc::memfd_create(
            c"hermit-declared-input".as_ptr(),
            libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING | MFD_EXEC,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let mut file = unsafe { File::from_raw_fd(fd) };
    file.write_all(bytes)?;
    let mode = if role == InputRole::Data {
        0o444
    } else {
        0o555
    };
    if unsafe { libc::fchmod(fd, mode) } != 0
        || unsafe { libc::fcntl(fd, libc::F_ADD_SEALS, REQUIRED_SEALS) } != 0
    {
        return Err(std::io::Error::last_os_error().into());
    }
    file.seek(SeekFrom::Start(0))?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::FileExt;
    use std::os::unix::fs::symlink;

    use super::*;

    fn data_inputs(path: &[u8], aliases: serde_json::Value) -> DynamicExecInputs {
        // Executable enrollment is intentionally separate from these data-only
        // physical capture controls; no test calls capture_inputs with fake ELF.
        let document = serde_json::json!({"version":1,"executables":[b"/program".to_vec()],
            "loader_files":[],"data_files":[path],"aliases":aliases});
        DynamicExecInputs::parse(&serde_json::to_vec(&document).unwrap()).unwrap()
    }

    #[test]
    fn declarations_refuse_unknown_trailing_duplicate_and_nonliteral_paths() {
        let good = serde_json::json!({"version":1,"executables":[b"/program".to_vec()],
            "loader_files":[],"data_files":[],"aliases":[]});
        assert!(DynamicExecInputs::parse(&serde_json::to_vec(&good).unwrap()).is_ok());
        let mut unknown = good.clone();
        unknown["unknown"] = true.into();
        assert!(DynamicExecInputs::parse(&serde_json::to_vec(&unknown).unwrap()).is_err());
        let mut trailing = serde_json::to_vec(&good).unwrap();
        trailing.extend_from_slice(b"{} ");
        assert!(DynamicExecInputs::parse(&trailing).is_err());
        for path in [b"relative".as_slice(), b"/a/../b", b"/a//b", b"/a\0b", b"/"] {
            assert!(GuestPath::new(path.to_vec()).is_err());
        }
        assert!(GuestPath::new(b"/nonutf8-\xff".to_vec()).is_ok());
        let mut duplicate = good.clone();
        duplicate["data_files"] = good["executables"].clone();
        assert!(DynamicExecInputs::parse(&serde_json::to_vec(&duplicate).unwrap()).is_err());
        assert!(DynamicExecInputs::parse(&vec![b' '; MAX_DECLARATION + 1]).is_err());
    }

    #[test]
    fn alias_graph_preserves_bytes_and_refuses_cycles_escape_and_collisions() {
        let inputs = data_inputs(
            b"/lib/data",
            serde_json::json!([
            {"path":b"/lib".to_vec(),"target":b"usr/lib".to_vec()}]),
        );
        assert_eq!(
            inputs.resolve(b"/lib/data", true).unwrap(),
            b"/usr/lib/data"
        );
        for aliases in [
            serde_json::json!([{"path":b"/lib".to_vec(),"target":b"../../escape".to_vec()}]),
            serde_json::json!([{"path":b"/lib".to_vec(),"target":b"lib".to_vec()}]),
        ] {
            let document = serde_json::json!({"version":1,"executables":[b"/program".to_vec()],
                "loader_files":[],"data_files":[b"/lib/data".to_vec()],"aliases":aliases});
            assert!(DynamicExecInputs::parse(&serde_json::to_vec(&document).unwrap()).is_err());
        }
    }

    #[test]
    fn actual_root_open_refuses_undeclared_links_and_retains_renamed_directory() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("data"), b"root bytes").unwrap();
        symlink("data", root.path().join("link")).unwrap();
        let opened = OpenedGuestRoot::open(root.path()).unwrap();
        assert!(opened.open_relative(b"/link", libc::O_RDONLY).is_err());
        let mut data = opened.open_relative(b"/data", libc::O_RDONLY).unwrap();
        let mut actual = Vec::new();
        data.read_to_end(&mut actual).unwrap();
        assert_eq!(actual, b"root bytes");
        let moved = root.path().with_extension("moved");
        std::fs::rename(root.path(), &moved).unwrap();
        assert_eq!(
            opened
                .open_relative(b"/data", libc::O_RDONLY)
                .unwrap()
                .metadata()
                .unwrap()
                .len(),
            10
        );
        std::fs::rename(&moved, root.path()).unwrap();
    }

    #[test]
    fn actual_capture_refuses_fifo_before_opening_it_for_read() {
        let root = tempfile::tempdir().unwrap();
        let path = CString::new(root.path().join("fifo").as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        let opened = OpenedGuestRoot::open(root.path()).unwrap();
        assert!(opened.open_regular(b"/fifo").is_err());
    }

    #[test]
    fn actual_capture_retains_alias_bytes_and_data_after_root_changes() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("usr/lib")).unwrap();
        std::fs::write(root.path().join("usr/lib/data"), b"captured data").unwrap();
        symlink("usr/lib", root.path().join("lib")).unwrap();
        let opened = OpenedGuestRoot::open(root.path()).unwrap();
        let mut inputs = data_inputs(
            b"/lib/data",
            serde_json::json!([
            {"path":b"/lib".to_vec(),"target":b"usr/lib".to_vec()}]),
        );
        // This is a filesystem-data component control; no executable or native
        // launch is represented. Production capture handles the same real data.
        inputs
            .files
            .remove(&GuestPath::new(b"/program".to_vec()).unwrap());
        let held = capture_inputs(&opened, inputs).unwrap();
        assert_eq!(held.aliases()[0].target, b"usr/lib");
        assert_eq!(held.files()[0].resolved_path.as_bytes(), b"/usr/lib/data");
        std::fs::write(root.path().join("usr/lib/data"), b"new data").unwrap();
        std::fs::remove_file(root.path().join("lib")).unwrap();
        symlink("somewhere-else", root.path().join("lib")).unwrap();
        assert_eq!(held.files()[0].read_bytes().unwrap(), b"captured data");
        held.recheck().unwrap();
        let mut mismatched = data_inputs(
            b"/lib/data",
            serde_json::json!([
            {"path":b"/lib".to_vec(),"target":b"usr/lib".to_vec()}]),
        );
        mismatched
            .files
            .remove(&GuestPath::new(b"/program".to_vec()).unwrap());
        assert!(capture_inputs(&opened, mismatched).is_err());
    }

    #[test]
    fn actual_sealed_bytes_survive_source_mutation_and_refuse_all_writes() {
        let source = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(source.path(), b"captured").unwrap();
        let held = seal_bytes(
            &std::fs::read(source.path()).unwrap(),
            InputRole::Executable,
        )
        .unwrap();
        std::fs::write(source.path(), b"mutated!").unwrap();
        let mut actual = [0; 8];
        held.read_exact_at(&mut actual, 0).unwrap();
        assert_eq!(&actual, b"captured");
        assert_eq!(
            unsafe { libc::fcntl(held.as_raw_fd(), libc::F_GET_SEALS) },
            REQUIRED_SEALS
        );
        assert_eq!(
            held.write_at(b"X", 0).unwrap_err().raw_os_error(),
            Some(libc::EPERM)
        );
        assert_eq!(
            held.set_len(0).unwrap_err().raw_os_error(),
            Some(libc::EPERM)
        );
        assert_eq!(
            held.set_len(9).unwrap_err().raw_os_error(),
            Some(libc::EPERM)
        );
        assert_eq!(
            unsafe {
                libc::fcntl(
                    held.as_raw_fd(),
                    libc::F_ADD_SEALS,
                    libc::F_SEAL_FUTURE_WRITE,
                )
            },
            -1
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EPERM)
        );
        assert_eq!(held.metadata().unwrap().mode() & 0o777, 0o555);
        assert_eq!(unsafe { libc::fchmod(held.as_raw_fd(), 0o444) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EPERM)
        );
        let mapping = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                4096,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                held.as_raw_fd(),
                0,
            )
        };
        assert_eq!(mapping, libc::MAP_FAILED);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EPERM)
        );
    }

    #[test]
    fn actual_native_adoption_retains_non_utf8_graph_and_inode_after_host_changes() {
        let root = tempfile::tempdir().unwrap();
        let source_name = OsStr::from_bytes(b"source-\xff");
        std::fs::create_dir(root.path().join(source_name)).unwrap();
        std::fs::write(root.path().join(source_name).join("data"), b"captured data").unwrap();
        let alias_name = OsStr::from_bytes(b"alias-\xfe");
        symlink(source_name, root.path().join(alias_name)).unwrap();
        let declaration = || DynamicExecInputs {
            files: [(
                GuestPath::new(b"/alias-\xfe/data".to_vec()).unwrap(),
                InputRole::Data,
            )]
            .into(),
            aliases: [(
                GuestPath::new(b"/alias-\xfe".to_vec()).unwrap(),
                b"source-\xff".to_vec(),
            )]
            .into(),
        };
        // Filesystem-data control only; full Capture still requires real ELF
        // enrollment and the compiled artifact/source qualification.
        let held =
            capture_inputs(&OpenedGuestRoot::open(root.path()).unwrap(), declaration()).unwrap();
        std::fs::write(
            root.path().join(source_name).join("data"),
            b"changed host source",
        )
        .unwrap();
        std::fs::remove_file(root.path().join(alias_name)).unwrap();
        symlink("wrong", root.path().join(alias_name)).unwrap();
        for _ in 0..2 {
            let original = &held.files()[0];
            let adopted = adopt_inputs(
                declaration(),
                vec![(
                    original.path.clone(),
                    original.role,
                    original.file.try_clone().unwrap(),
                )],
            )
            .unwrap();
            assert_eq!(adopted.files()[0].identity, original.identity);
            assert_eq!(adopted.files()[0].read_bytes().unwrap(), b"captured data");
            assert_eq!(
                adopted.files()[0].resolved_path.as_bytes(),
                b"/source-\xff/data"
            );
            assert_eq!(adopted.aliases()[0].target, b"source-\xff");
        }
    }

    #[test]
    fn actual_native_adoption_refuses_missing_extra_role_and_unsealed_sources() {
        let path = GuestPath::new(b"/data".to_vec()).unwrap();
        let declaration = || DynamicExecInputs {
            files: [(path.clone(), InputRole::Data)].into(),
            aliases: BTreeMap::new(),
        };
        let held = seal_bytes(b"data", InputRole::Data).unwrap();
        assert!(adopt_inputs(declaration(), Vec::new()).is_err());
        assert!(
            adopt_inputs(
                declaration(),
                vec![
                    (path.clone(), InputRole::Data, held.try_clone().unwrap()),
                    (path.clone(), InputRole::Data, held.try_clone().unwrap())
                ]
            )
            .is_err()
        );
        assert!(
            adopt_inputs(
                declaration(),
                vec![(path.clone(), InputRole::Loader, held.try_clone().unwrap())]
            )
            .is_err()
        );
        let raw = tempfile::tempfile().unwrap();
        assert!(adopt_inputs(declaration(), vec![(path, InputRole::Data, raw)]).is_err());
    }
}
