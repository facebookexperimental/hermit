/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Authenticated initial-image random ingress at existing SaBRe ptrace stops.
//! No RPC readiness, scheduling operation or additional stop is introduced.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::ffi::CString;
use std::ffi::OsStr;
use std::fs::File;
use std::io::IoSlice;
use std::io::IoSliceMut;
use std::io::Read;
use std::io::Write;
use std::ops::Deref;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::FileExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Result;
use anyhow::anyhow;
use anyhow::ensure;
use detcore::random::InitialImage;
use detcore::random::LoaderState;
use detcore::random::encode_continuation;
use detcore::random::encode_initial_state;
use detcore::random::getrandom;
use detcore::random::initialize_auxv;
use detcore::random::root_prng;
use nix::unistd::Pid;
use object::Object;
use object::ObjectSegment;
use object::ObjectSymbol;
use reverie::syscalls::AddrMut;
use reverie::syscalls::Errno;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::Syscall;
use reverie::syscalls::SyscallArgs;
use reverie::syscalls::Sysno;
// Versioned loader wire protocol. No plugin crate is linked into the CLI:
// that crate owns a global allocator and guest-local runtime state.
mod bootstrap {
    pub const PRCTL_OPTION: u64 = 0x53425242;
    pub const VERSION: u64 = 1;
    pub const IMAGE: u64 = 1;
    pub const GETRANDOM: u64 = 2;
    pub const TAKE_STATE: u64 = 3;
    pub const MAX_STATE_BYTES: usize = 4096;
    pub const SYSCALL_SYMBOL: &str = "sbr_bootstrap_syscall_v1";
}
pub(super) const ENVIRONMENT: &str = "REVERIE_SABRE_BOOTSTRAP_V1";
const LAYOUT_SYMBOL: &str = "sbr_bootstrap_frame_layout_v1";

const IN_MEMORY_SNAPSHOT_LIMIT: usize = 128 * 1024 * 1024;
const MAX_MAPS: usize = 1024 * 1024;
const MAX_OBJECTS: usize = 16;
const PAGE: usize = 4096;

fn bounded_file(mut file: &File, maximum: usize) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(maximum as u64 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= maximum,
        "bootstrap input exceeds declared bound"
    );
    Ok(bytes)
}

fn read_path(path: impl AsRef<Path>, maximum: usize) -> Result<Vec<u8>> {
    bounded_file(&File::open(path)?, maximum)
}

fn word(bytes: &[u8], offset: usize) -> Result<usize> {
    let end = offset
        .checked_add(8)
        .ok_or_else(|| anyhow!("word offset overflow"))?;
    Ok(u64::from_le_bytes(
        bytes
            .get(offset..end)
            .ok_or_else(|| anyhow!("short word"))?
            .try_into()?,
    ) as usize)
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Map {
    start: usize,
    end: usize,
    offset: usize,
    device: String,
    inode: u64,
    path: Vec<u8>,
    permissions: String,
}

impl Map {
    fn contains(&self, address: usize, size: usize) -> bool {
        address >= self.start && address.checked_add(size).is_some_and(|end| end <= self.end)
    }

    fn displays_path(&self, path: &Path) -> bool {
        // Linux maps escapes newline as \\012 but leaves a literal backslash
        // unchanged. Never decode this ambiguous display into an object path.
        let mut displayed = Vec::new();
        for byte in path.as_os_str().as_bytes() {
            if *byte == b'\n' {
                displayed.extend_from_slice(b"\\012");
            } else {
                displayed.push(*byte);
            }
        }
        self.path == displayed
    }

    fn mapped_path(&self, pid: Pid) -> Result<PathBuf> {
        ensure!(
            self.path.starts_with(b"/") && self.inode != 0,
            "unsupported anonymous early getrandom source"
        );
        // Ordinary path displays are already raw bytes. Only ambiguous kernel
        // displays need map_files; do not add that proc capability requirement
        // to an otherwise unambiguous legacy path.
        if !self.path.windows(4).any(|part| part == b"\\012") && !self.path.ends_with(b" (deleted)")
        {
            return Ok(PathBuf::from(OsStr::from_bytes(&self.path)));
        }
        // The magic link returns actual pathname bytes. Authenticate its
        // display and then the opened FD/device/inode/root-relative contents;
        // a newline and a literal \\012 alone cannot select the same object.
        let path = std::fs::read_link(format!(
            "/proc/{pid}/map_files/{:x}-{:x}",
            self.start, self.end
        ))?;
        ensure!(
            self.displays_path(&path),
            "mapped object path display changed"
        );
        Ok(path)
    }
}

fn maps(pid: Pid) -> Result<Vec<Map>> {
    let bytes = read_path(format!("/proc/{pid}/maps"), MAX_MAPS)?;
    bytes
        .strip_suffix(b"\n")
        .unwrap_or(&bytes)
        .split(|byte| *byte == b'\n')
        .map(|line| {
            // Only the five structural fields are whitespace-separated text.
            // The remaining pathname may contain spaces, kernel escapes or
            // non-UTF8 bytes in an unrelated mapping. Retain those bytes and
            // authenticate pathname bytes only when selecting that object.
            let mut rest = line;
            let mut fields = Vec::with_capacity(5);
            for _ in 0..5 {
                rest = rest.trim_ascii_start();
                let end = rest
                    .iter()
                    .position(u8::is_ascii_whitespace)
                    .unwrap_or(rest.len());
                ensure!(end > 0, "missing bootstrap mapping field");
                fields.push(std::str::from_utf8(&rest[..end])?);
                rest = &rest[end..];
            }
            let (start, end) = fields[0]
                .split_once('-')
                .ok_or_else(|| anyhow!("bad map range"))?;
            let row = Map {
                start: usize::from_str_radix(start, 16)?,
                end: usize::from_str_radix(end, 16)?,
                offset: usize::from_str_radix(fields[2], 16)?,
                device: fields[3].to_owned(),
                inode: fields[4].parse()?,
                path: rest.trim_ascii_start().to_vec(),
                permissions: fields[1].to_owned(),
            };
            ensure!(
                row.start < row.end && row.permissions.len() == 4,
                "invalid bootstrap map"
            );
            Ok(row)
        })
        .collect()
}

fn containing(rows: &[Map], address: usize, size: usize) -> Result<&Map> {
    let mut found = rows.iter().filter(|row| row.contains(address, size));
    let first = found
        .next()
        .ok_or_else(|| anyhow!("bootstrap extent is not in one mapping"))?;
    ensure!(found.next().is_none(), "ambiguous bootstrap extent");
    Ok(first)
}

fn open_under_root(pid: Pid, path: &Path) -> Result<File> {
    let path = path.as_os_str().as_bytes();
    ensure!(path.starts_with(b"/"), "nonabsolute bootstrap object path");
    let components: Vec<_> = path[1..].split(|byte| *byte == b'/').collect();
    ensure!(
        !components.is_empty()
            && components
                .iter()
                .all(|c| !c.is_empty() && *c != b"." && *c != b".."),
        "noncanonical bootstrap object path"
    );
    // Follow only the proc magic link to this owned task's root. Every actual
    // path component thereafter is no-follow and relative to a held directory.
    let mut file = File::open(format!("/proc/{pid}/root"))?;
    for (i, part) in components.iter().enumerate() {
        let name = CString::new(*part)?;
        let flags = libc::O_CLOEXEC
            | libc::O_NOFOLLOW
            | if i + 1 < components.len() {
                libc::O_DIRECTORY
            } else {
                0
            };
        let fd = unsafe { libc::openat(file.as_raw_fd(), name.as_ptr(), flags, 0) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        file = unsafe { File::from_raw_fd(fd) };
    }
    Ok(file)
}

struct DiskSnapshot {
    address: usize,
    length: usize,
}

fn require_supported_snapshot_filesystem(file: &File) -> Result<()> {
    let mut fs = std::mem::MaybeUninit::<libc::statfs>::uninit();
    if unsafe { libc::fstatfs(file.as_raw_fd(), fs.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let fs = unsafe { fs.assume_init() };
    // XFS_SUPER_MAGIC from linux/magic.h is not exported by this libc version.
    // Limit this storage path to qualified regular filesystems; this is not a
    // claim about their physical block-device backing. The mapped snapshot
    // pages must be reclaimable independently of the supervisor's heap.
    const XFS_SUPER_MAGIC: libc::c_long = 0x58465342;
    ensure!(
        matches!(
            fs.f_type,
            libc::BTRFS_SUPER_MAGIC | libc::EXT4_SUPER_MAGIC | XFS_SUPER_MAGIC
        ),
        "large bootstrap snapshot requires a supported cache filesystem (unqualified filesystem {:#x})",
        fs.f_type
    );
    Ok(())
}

impl DiskSnapshot {
    fn new(file: &File, length: usize) -> Result<Self> {
        ensure!(
            length <= isize::MAX as usize,
            "bootstrap snapshot exceeds address space"
        );
        // Reuse the existing Hermit cache placement, never cwd or a new
        // hardcoded scratch directory. Small objects do not need this cache.
        let data = super::HermitData::new();
        ensure!(
            data.data_dir().is_absolute(),
            "bootstrap snapshot cache must be absolute"
        );
        let parent = data.data_dir().join("tmp");
        std::fs::create_dir_all(&parent)?;
        let directory = tempfile::Builder::new()
            .prefix("sabre-snapshot-")
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir_in(parent)?;
        let mut output = tempfile::tempfile_in(directory.path())?;
        require_supported_snapshot_filesystem(&output)?;
        let mut buffer = [0; 64 * 1024];
        let mut offset = 0;
        while offset < length {
            let size = buffer.len().min(length - offset);
            file.read_exact_at(&mut buffer[..size], offset as u64)?;
            output.write_all(&buffer[..size])?;
            offset += size;
        }
        ensure!(
            file.metadata()?.len() == length as u64,
            "bootstrap object size changed during snapshot"
        );
        // Reopen read-only, then discard the writer. After mmap discard even
        // the read-only FD: the guest shares this supervisor's PID namespace
        // and must not obtain a snapshot alias through /proc/<parent>/fd.
        let readonly = File::open(format!("/proc/self/fd/{}", output.as_raw_fd()))?;
        drop(output);
        let address = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                length,
                libc::PROT_READ,
                libc::MAP_PRIVATE,
                readonly.as_raw_fd(),
                0,
            )
        };
        if address == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error().into());
        }
        let snapshot = Self {
            address: address as usize,
            length,
        };
        drop(readonly);
        directory.close()?;
        Ok(snapshot)
    }

    fn as_slice(&self) -> &[u8] {
        // The anonymous copied backing has no writable handle or exposed
        // pathname. It is owned solely by this read-only mapping until Drop,
        // under the same trusted-supervisor assumption as the former Vec.
        unsafe { std::slice::from_raw_parts(self.address as *const u8, self.length) }
    }
}

impl Drop for DiskSnapshot {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.address as *mut libc::c_void, self.length) };
    }
}

enum OriginalBytes {
    Memory(Vec<u8>),
    Disk(DiskSnapshot),
}

impl OriginalBytes {
    fn new(file: &File, length: usize) -> Result<Self> {
        if length <= IN_MEMORY_SNAPSHOT_LIMIT {
            Ok(Self::Memory(bounded_file(file, IN_MEMORY_SNAPSHOT_LIMIT)?))
        } else {
            Ok(Self::Disk(DiskSnapshot::new(file, length)?))
        }
    }

    fn as_slice(&self) -> &[u8] {
        match self {
            Self::Memory(bytes) => bytes,
            Self::Disk(snapshot) => snapshot.as_slice(),
        }
    }

    fn matches_file(&self, file: &File) -> Result<bool> {
        if file.metadata()?.len() != self.len() as u64 {
            return Ok(false);
        }
        let mut buffer = [0; 64 * 1024];
        for (index, expected) in self.chunks(buffer.len()).enumerate() {
            let actual = &mut buffer[..expected.len()];
            file.read_exact_at(actual, (index * 64 * 1024) as u64)?;
            if actual != expected {
                return Ok(false);
            }
        }
        Ok(file.metadata()?.len() == self.len() as u64)
    }
}

impl Deref for OriginalBytes {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        self.as_slice()
    }
}

struct HeldObject {
    path: PathBuf,
    file: File,
    bytes: OriginalBytes,
    device: String,
    inode: u64,
}

impl HeldObject {
    fn open(path: &Path) -> Result<Self> {
        let held = Self::open_file(path)?;
        let elf = object::File::parse(held.bytes.as_slice())?;
        ensure!(
            elf.format() == object::BinaryFormat::Elf
                && elf.architecture() == object::Architecture::X86_64
                && elf.is_little_endian(),
            "bootstrap requires x86-64 ELF"
        );
        Ok(held)
    }

    fn open_file(path: &Path) -> Result<Self> {
        let path = std::fs::canonicalize(path)?;
        let file = File::open(&path)?;
        let meta = file.metadata()?;
        ensure!(
            meta.is_file() && meta.len() > 0,
            "unsupported bootstrap ELF object size"
        );
        let length = usize::try_from(meta.len())?;
        let bytes = OriginalBytes::new(&file, length)?;
        ensure!(
            bytes.matches_file(&file)?,
            "bootstrap object changed during snapshot"
        );
        // Compare maps-device to maps-device for this exact held FD. Btrfs's
        // maps superblock device need not equal its subvolume st_dev.
        let address = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                PAGE,
                libc::PROT_READ,
                libc::MAP_PRIVATE,
                file.as_raw_fd(),
                0,
            )
        };
        ensure!(
            address != libc::MAP_FAILED,
            "cannot map held bootstrap object"
        );
        let observed = (|| {
            let own = maps(Pid::this())?;
            let row = containing(&own, address as usize, 1)?;
            ensure!(
                row.displays_path(&path) && row.inode == meta.ino() && row.offset == 0,
                "held bootstrap FD mapping mismatch"
            );
            Ok::<_, anyhow::Error>(row.device.clone())
        })();
        let unmapped = unsafe { libc::munmap(address, PAGE) };
        ensure!(unmapped == 0, "failed to unmap bootstrap identity mapping");
        Ok(Self {
            path,
            file,
            bytes,
            device: observed?,
            inode: meta.ino(),
        })
    }

    fn matches_mapping(&self, row: &Map) -> bool {
        row.displays_path(&self.path) && row.inode == self.inode && row.device == self.device
    }

    fn authenticate(&self, pid: Pid, row: &Map) -> Result<()> {
        ensure!(
            self.matches_mapping(row),
            "bootstrap mapped object identity mismatch: map={:#x}..{:#x} offset={:#x} \
             device={} inode={} permissions={} expected_device={} expected_inode={}",
            row.start,
            row.end,
            row.offset,
            row.device,
            row.inode,
            row.permissions,
            self.device,
            self.inode
        );
        self.authenticate_path(pid)
    }

    fn authenticate_path(&self, pid: Pid) -> Result<()> {
        let other = open_under_root(pid, &self.path)?;
        let a = self.file.metadata()?;
        let b = other.metadata()?;
        ensure!(
            (a.dev(), a.ino(), a.len()) == (b.dev(), b.ino(), b.len()),
            "bootstrap root-relative object changed"
        );
        ensure!(
            self.bytes.matches_file(&other)?,
            "bootstrap mapped pathname content changed"
        );
        Ok(())
    }

    fn bias(&self, pid: Pid, rows: &[Map]) -> Result<usize> {
        let elf = object::File::parse(self.bytes.as_slice())?;
        let mut common: Option<BTreeSet<usize>> = None;
        for row in rows.iter().filter(|r| self.matches_mapping(r)) {
            self.authenticate(pid, row)?;
            if !row.permissions.contains('x') {
                continue;
            }
            let mut candidates = BTreeSet::new();
            for segment in elf.segments() {
                let object::SegmentFlags::Elf { p_flags } = segment.flags() else {
                    continue;
                };
                if p_flags & object::elf::PF_X == 0 {
                    continue;
                }
                let (offset, size) = segment.file_range();
                if size == 0 {
                    continue;
                }
                let lo = offset as usize & !(PAGE - 1);
                let end = (offset as usize)
                    .checked_add(size as usize)
                    .and_then(|n| n.checked_add(PAGE - 1))
                    .ok_or_else(|| anyhow!("ELF segment overflow"))?
                    & !(PAGE - 1);
                if row.offset >= lo && row.offset < end {
                    let virtual_start = (segment.address() as usize & !(PAGE - 1))
                        .checked_add(row.offset - lo)
                        .ok_or_else(|| anyhow!("ELF bias overflow"))?;
                    if let Some(bias) = row.start.checked_sub(virtual_start) {
                        candidates.insert(bias);
                    }
                }
            }
            ensure!(
                !candidates.is_empty(),
                "executable mapping is not backed by an executable ELF load segment"
            );
            common = Some(match common {
                None => candidates,
                Some(old) => old.intersection(&candidates).copied().collect(),
            });
        }
        let biases =
            common.ok_or_else(|| anyhow!("missing executable bootstrap object mapping"))?;
        ensure!(biases.len() == 1, "ambiguous bootstrap ELF load bias");
        Ok(*biases.first().unwrap())
    }

    /// Select the instance containing an observed executable address. The
    /// loader and guest may map the same PT_INTERP file at different bases;
    /// unrelated instances cannot contribute constraints to this address.
    fn bias_at(&self, pid: Pid, rows: &[Map], anchor: usize, role: &str) -> Result<usize> {
        let result = (|| {
            let row = containing(rows, anchor, 1)?;
            self.authenticate(pid, row)?;
            ensure!(
                row.permissions.contains('x'),
                "anchor mapping is not executable"
            );
            let elf = object::File::parse(self.bytes.as_slice())?;
            let mut candidates = BTreeSet::new();
            for segment in elf.segments() {
                let object::SegmentFlags::Elf { p_flags } = segment.flags() else {
                    continue;
                };
                if p_flags & object::elf::PF_X == 0 {
                    continue;
                }
                let (offset, size) = segment.file_range();
                let offset = usize::try_from(offset)?;
                let size = usize::try_from(size)?;
                let virtual_start = usize::try_from(segment.address())?;
                let file_anchor = row
                    .offset
                    .checked_add(anchor - row.start)
                    .ok_or_else(|| anyhow!("anchor file offset overflow"))?;
                if file_anchor < offset || file_anchor - offset >= size {
                    continue;
                }
                let relative = virtual_start
                    .checked_add(file_anchor - offset)
                    .ok_or_else(|| anyhow!("anchor virtual address overflow"))?;
                if let Some(bias) = anchor.checked_sub(relative)
                    && bias.is_multiple_of(PAGE)
                {
                    candidates.insert(bias);
                }
            }
            ensure!(
                candidates.len() == 1,
                "missing/ambiguous anchored ELF load bias"
            );
            Ok(*candidates.first().unwrap())
        })();
        result.map_err(|error: anyhow::Error| anyhow!("{role} anchor={anchor:#x}: {error:#}"))
    }

    fn program_headers(
        &self,
        pid: Pid,
        rows: &[Map],
        bias: usize,
        address: usize,
        count: usize,
        size: usize,
    ) -> Result<()> {
        let result = (|| {
            let phoff = word(&self.bytes, 32)?;
            let phnum = u16::from_le_bytes(self.bytes[56..58].try_into()?) as usize;
            ensure!(
                count == phnum && size == 56,
                "final program header count/size changed"
            );
            let length = phnum
                .checked_mul(56)
                .ok_or_else(|| anyhow!("program header extent overflow"))?;
            let elf = object::File::parse(self.bytes.as_slice())?;
            let first = elf
                .segments()
                .next()
                .ok_or_else(|| anyhow!("missing first ELF load"))?;
            let (offset, _) = first.file_range();
            let virtual_address = usize::try_from(first.address())?
                .checked_add(
                    phoff
                        .checked_sub(usize::try_from(offset)?)
                        .ok_or_else(|| anyhow!("program headers precede first ELF load"))?,
                )
                .ok_or_else(|| anyhow!("program header virtual address overflow"))?;
            ensure!(
                bias.checked_add(virtual_address) == Some(address),
                "final program header address differs from anchored ELF"
            );
            let row = containing(rows, address, length)?;
            if self.matches_mapping(row) {
                self.authenticate(pid, row)?;
            } else {
                // library_buf_get_original replaces precisely the first file
                // page after saving the original image. Authenticate only the
                // PHDR bytes there, anchored by the still-file-backed entry.
                // This is not authority for anonymous instructions or frames.
                let page = bias
                    .checked_add(usize::try_from(first.address())? & !(PAGE - 1))
                    .ok_or_else(|| anyhow!("copied ELF page address overflow"))?;
                let object::SegmentFlags::Elf { p_flags } = first.flags() else {
                    return Err(anyhow!("missing first ELF load flags"));
                };
                let permissions = format!(
                    "{}{}{}p",
                    if p_flags & object::elf::PF_R != 0 {
                        'r'
                    } else {
                        '-'
                    },
                    if p_flags & object::elf::PF_W != 0 {
                        'w'
                    } else {
                        '-'
                    },
                    if p_flags & object::elf::PF_X != 0 {
                        'x'
                    } else {
                        '-'
                    }
                );
                ensure!(
                    offset == 0
                        && row.start == page
                        && page.checked_add(PAGE) == Some(row.end)
                        && row.offset == 0
                        && row.inode == 0
                        && row.device == "00:00"
                        && row.path.is_empty()
                        && row.permissions == permissions,
                    "unrecognized copied PHDR mapping: map={:#x}..{:#x} offset={:#x} \
                     device={} inode={} permissions={} expected_page={page:#x} expected_permissions={permissions}",
                    row.start,
                    row.end,
                    row.offset,
                    row.device,
                    row.inode,
                    row.permissions
                );
                self.authenticate_path(pid)?;
            }
            let expected = self
                .bytes
                .get(
                    phoff
                        ..phoff
                            .checked_add(length)
                            .ok_or_else(|| anyhow!("program header range overflow"))?,
                )
                .ok_or_else(|| anyhow!("short program headers"))?;
            ensure!(
                self.at_virtual(virtual_address, length, false)? == expected
                    && remote_bytes(pid, address, length)? == expected,
                "final program headers differ from held ELF"
            );
            Ok(())
        })();
        result.map_err(|error: anyhow::Error| {
            anyhow!("program PHDR address={address:#x} load_bias={bias:#x}: {error:#}")
        })
    }

    fn at_virtual(&self, address: usize, length: usize, readonly: bool) -> Result<&[u8]> {
        let elf = object::File::parse(self.bytes.as_slice())?;
        let end = address
            .checked_add(length)
            .ok_or_else(|| anyhow!("ELF extent overflow"))?;
        let mut found = None;
        for segment in elf.segments() {
            let (offset, size) = segment.file_range();
            let lo = segment.address() as usize;
            let hi = lo
                .checked_add(size as usize)
                .ok_or_else(|| anyhow!("ELF segment overflow"))?;
            if address >= lo && end <= hi {
                let object::SegmentFlags::Elf { p_flags } = segment.flags() else {
                    continue;
                };
                ensure!(
                    !readonly
                        || (p_flags & object::elf::PF_R != 0 && p_flags & object::elf::PF_W == 0),
                    "bootstrap descriptor is in a writable ELF load segment"
                );
                ensure!(found.is_none(), "ambiguous ELF file extent");
                let start = (offset as usize)
                    .checked_add(address - lo)
                    .ok_or_else(|| anyhow!("ELF file offset overflow"))?;
                found = self.bytes.get(
                    start
                        ..start
                            .checked_add(length)
                            .ok_or_else(|| anyhow!("ELF file range overflow"))?,
                );
            }
        }
        found.ok_or_else(|| anyhow!("bootstrap ELF extent is not fully file-backed"))
    }

    fn layout(&self) -> Result<FrameLayout> {
        let elf = object::File::parse(self.bytes.as_slice())?;
        let definitions: BTreeSet<_> = elf
            .symbols()
            .chain(elf.dynamic_symbols())
            .filter(|s| s.name().ok() == Some(LAYOUT_SYMBOL) && !s.is_undefined())
            .map(|s| {
                (
                    s.address() as usize,
                    s.size() as usize,
                    s.kind() == object::SymbolKind::Data,
                )
            })
            .collect();
        ensure!(
            definitions.len() == 1,
            "missing/ambiguous bootstrap frame descriptor"
        );
        let (address, size, data) = *definitions.first().unwrap();
        ensure!(
            data && size == 9 * 8,
            "wrong bootstrap frame descriptor type/extent"
        );
        FrameLayout::decode(self.at_virtual(address, size, true)?)
    }

    fn symbol(&self, name: &str) -> Result<usize> {
        let elf = object::File::parse(self.bytes.as_slice())?;
        let symbols: BTreeSet<_> = elf
            .symbols()
            .chain(elf.dynamic_symbols())
            .filter(|s| s.name().ok() == Some(name) && !s.is_undefined())
            .map(|s| s.address() as usize)
            .collect();
        ensure!(
            symbols.len() == 1,
            "missing/ambiguous bootstrap ELF symbol {name}"
        );
        Ok(*symbols.first().unwrap())
    }
}

#[derive(Clone, Copy)]
struct RemoteMemory(Pid);
impl MemoryAccess for RemoteMemory {
    fn read_vectored(
        &self,
        remote: &[IoSlice],
        local: &mut [IoSliceMut],
    ) -> std::result::Result<usize, Errno> {
        let n = unsafe {
            libc::process_vm_readv(
                self.0.as_raw(),
                local.as_ptr().cast(),
                local.len() as _,
                remote.as_ptr().cast(),
                remote.len() as _,
                0,
            )
        };
        if n < 0 {
            Err(Errno::last())
        } else {
            Ok(n as usize)
        }
    }
    fn write_vectored(
        &mut self,
        local: &[IoSlice],
        remote: &mut [IoSliceMut],
    ) -> std::result::Result<usize, Errno> {
        let n = unsafe {
            libc::process_vm_writev(
                self.0.as_raw(),
                local.as_ptr().cast(),
                local.len() as _,
                remote.as_ptr().cast(),
                remote.len() as _,
                0,
            )
        };
        if n < 0 {
            Err(Errno::last())
        } else {
            Ok(n as usize)
        }
    }
}

fn remote_bytes(pid: Pid, address: usize, length: usize) -> Result<Vec<u8>> {
    ensure!(
        length <= MAX_MAPS && address.checked_add(length).is_some(),
        "bootstrap remote read exceeds bound"
    );
    let pointer =
        reverie::syscalls::Addr::from_raw(address).ok_or_else(|| anyhow!("null bootstrap read"))?;
    let mut bytes = vec![0; length];
    RemoteMemory(pid).read_exact(pointer, &mut bytes)?;
    Ok(bytes)
}

fn generation(pid: Pid) -> Result<u64> {
    let bytes = read_path(format!("/proc/{pid}/stat"), 4096)?;
    stat_generation(pid, &bytes)
}

fn stat_generation(pid: Pid, bytes: &[u8]) -> Result<u64> {
    // Linux comm is raw bytes and may itself contain ") ". Only the PID
    // prefix and the numeric fields following its final delimiter are text.
    let end = bytes
        .windows(2)
        .rposition(|pair| pair == b") ")
        .ok_or_else(|| anyhow!("malformed bootstrap process stat"))?;
    let prefix = &bytes[..end];
    let owner_end = prefix
        .windows(2)
        .position(|pair| pair == b" (")
        .ok_or_else(|| anyhow!("missing process stat owner"))?;
    ensure!(
        std::str::from_utf8(&prefix[..owner_end])?.parse::<i32>()? == pid.as_raw(),
        "bootstrap stat owner mismatch"
    );
    let start = bytes[end + 2..]
        .split(u8::is_ascii_whitespace)
        .filter(|field| !field.is_empty())
        .nth(19)
        .ok_or_else(|| anyhow!("missing process generation"))?;
    Ok(std::str::from_utf8(start)?.parse()?)
}

fn script_interpreter(bytes: &[u8]) -> Result<Option<&Path>> {
    Ok(script_words(bytes)?
        .first()
        .map(|&path| Path::new(OsStr::from_bytes(path))))
}

fn script_words(bytes: &[u8]) -> Result<Vec<&[u8]>> {
    // Match the loader's single BINPRM_BUF_SIZE/fgets/strtok resolution.
    if !bytes.starts_with(b"#!") {
        return Ok(Vec::new());
    }
    let line = &bytes[..bytes.len().min(255)];
    let line = &line[..line.iter().position(|b| *b == b'\n').unwrap_or(line.len())];
    let mut tokens = line[2..]
        .split(|b| matches!(b, b' ' | b'\n' | b'\r' | b'\t'))
        .filter(|part| !part.is_empty());
    let path = tokens
        .next()
        .ok_or_else(|| anyhow!("missing SaBRe script interpreter"))?;
    // Preserve the existing interpreter-path NUL refusal. C strtok ends the
    // optional argument at its first NUL and never considers later tokens.
    ensure!(!path.contains(&0), "NUL in SaBRe script interpreter");
    let mut words = vec![path];
    if let Some(token) = tokens.next() {
        let token = &token[..token
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(token.len())];
        if !token.is_empty() {
            words.push(token);
        }
    }
    Ok(words)
}

fn elf_interpreter(bytes: &[u8]) -> Result<Option<PathBuf>> {
    // PT_INTERP is a program header, not a PT_LOAD segment.
    let phoff = word(bytes, 32)?;
    let phnum = u16::from_le_bytes(
        bytes
            .get(56..58)
            .ok_or_else(|| anyhow!("short ELF header"))?
            .try_into()?,
    ) as usize;
    ensure!(
        phnum <= 128 && bytes.get(54..56) == Some(&56u16.to_le_bytes()),
        "unsupported ELF program headers"
    );
    ensure!(
        phnum > 0
            && phoff
                .checked_add(phnum * 56)
                .is_some_and(|end| end <= bytes.len()),
        "program headers outside held ELF"
    );
    let mut path = None;
    for i in 0..phnum {
        let p = phoff
            .checked_add(i * 56)
            .ok_or_else(|| anyhow!("program header overflow"))?;
        if bytes.get(p..p + 4) == Some(&3u32.to_le_bytes()) {
            ensure!(path.is_none(), "duplicate guest interpreter");
            let offset = word(bytes, p + 8)?;
            let size = word(bytes, p + 32)?;
            ensure!((2..=4096).contains(&size), "invalid guest interpreter path");
            let bytes = bytes
                .get(
                    offset
                        ..offset
                            .checked_add(size)
                            .ok_or_else(|| anyhow!("interpreter extent overflow"))?,
                )
                .ok_or_else(|| anyhow!("short interpreter path"))?;
            ensure!(
                bytes.last() == Some(&0) && !bytes[..size - 1].contains(&0),
                "invalid interpreter terminator"
            );
            path = Some(PathBuf::from(OsStr::from_bytes(&bytes[..size - 1])));
        }
    }
    Ok(path)
}

// The initial exec stop precedes the loader's first instruction. Bound string
// capture by the actual kernel-created stack mapping, with one non-overlapping
// extent per argument. No smaller per-string limit is imposed. Only argv is
// retained; environment values are not retained or included in diagnostics.
fn initial_arguments(pid: Pid, rows: &[Map], stack: usize) -> Result<Vec<Vec<u8>>> {
    let row = containing(rows, stack, 8)?;
    ensure!(
        row.path == b"[stack]" && row.permissions.starts_with("rw"),
        "initial argv is not on the owned writable stack"
    );
    let argc = word(&remote_bytes(pid, stack, 8)?, 0)?;
    // run_sabre prepends the loader, plugin and delimiter to the existing
    // supported guest argc (4096). IMAGE retains that original guest bound.
    ensure!(
        (4..=4099).contains(&argc),
        "unsupported initial loader argc"
    );
    let vector_size = (argc + 2) * 8;
    ensure!(
        row.contains(stack, vector_size),
        "initial argv vector leaves stack"
    );
    let vector = remote_bytes(pid, stack, vector_size)?;
    ensure!(
        word(&vector, (argc + 1) * 8)? == 0,
        "missing initial argv terminator"
    );
    let mut previous_end = stack + vector_size;
    let mut arguments = Vec::with_capacity(argc);
    for index in 0..argc {
        let pointer = word(&vector, (index + 1) * 8)?;
        ensure!(
            pointer >= previous_end && row.contains(pointer, 1),
            "initial argv string overlaps or leaves kernel stack: index={index}"
        );
        let mut value = Vec::new();
        let mut at = pointer;
        loop {
            ensure!(
                at < row.end,
                "unterminated initial argv string: index={index}"
            );
            let chunk = remote_bytes(pid, at, (row.end - at).min(PAGE))?;
            if let Some(end) = chunk.iter().position(|byte| *byte == 0) {
                value.extend_from_slice(&chunk[..=end]);
                previous_end = at + end + 1;
                break;
            }
            value.extend_from_slice(&chunk);
            at += chunk.len();
        }
        arguments.push(value);
    }
    Ok(arguments)
}

fn guest_arguments(initial: Vec<Vec<u8>>, script: Option<&[u8]>) -> Result<Vec<Vec<u8>>> {
    // find_client_path_idx scans after argv[0], and stops at the first exact
    // delimiter. Later literal "--" arguments belong to the client unchanged.
    let delimiter = initial
        .iter()
        .skip(1)
        .position(|value| value == b"--\0")
        .map(|index| index + 1)
        .ok_or_else(|| anyhow!("initial loader argv lacks delimiter"))?;
    ensure!(
        delimiter + 1 < initial.len(),
        "initial loader argv lacks client"
    );
    let mut expected = Vec::new();
    if let Some(script) = script {
        for token in script_words(script)? {
            let mut value = token.to_vec();
            value.push(0);
            expected.push(value);
        }
    }
    expected.extend(initial.into_iter().skip(delimiter + 1));
    ensure!(
        !expected.is_empty() && expected.len() <= 4096,
        "unsupported bootstrap argc"
    );
    Ok(expected)
}

fn authenticate_arguments(
    pid: Pid,
    rows: &[Map],
    vector: &[u8],
    expected: &[Vec<u8>],
) -> Result<usize> {
    let argc = word(vector, 0)?;
    ensure!(
        argc > 0 && argc <= 4096 && argc == expected.len(),
        "final argv count changed"
    );
    for (index, value) in expected.iter().enumerate() {
        let pointer = word(vector, (index + 1) * 8)?;
        let row = containing(rows, pointer, value.len()).map_err(|error| {
            anyhow!("final argv mapping index={index} address={pointer:#x}: {error:#}")
        })?;
        ensure!(
            row.permissions.starts_with('r'),
            "final argv mapping is not readable: index={index}"
        );
        // Values include the terminating NUL. Comparing the full retained
        // extent rejects truncation, extension, reordered arguments and an
        // unrelated readable pointer without assuming strings live on stack.
        let mut offset = 0;
        while offset < value.len() {
            let length = (value.len() - offset).min(PAGE);
            ensure!(
                remote_bytes(pid, pointer + offset, length)? == value[offset..offset + length],
                "final argv bytes/order changed: index={index}"
            );
            offset += length;
        }
    }
    let at = (argc + 1) * 8;
    ensure!(word(vector, at)? == 0, "missing final argv terminator");
    Ok(at + 8)
}

/// Launch inputs are retained before the owned child can execute the loader.
pub(super) struct Launch {
    loader: HeldObject,
    program: HeldObject,
    interpreter: Option<HeldObject>,
    script: Option<HeldObject>,
    config: detcore::Config,
    layout: FrameLayout,
}
impl Launch {
    pub(super) fn new(loader: &Path, program: &Path, config: &detcore::Config) -> Result<Self> {
        let loader = HeldObject::open(loader)?;
        loader.symbol(bootstrap::SYSCALL_SYMBOL)?;
        let layout = loader.layout()?;
        let input = HeldObject::open_file(program)?;
        let (program, script) = if let Some(path) = script_interpreter(&input.bytes)? {
            (HeldObject::open(path)?, Some(input))
        } else {
            let elf = object::File::parse(input.bytes.as_slice())?;
            ensure!(
                elf.format() == object::BinaryFormat::Elf
                    && elf.architecture() == object::Architecture::X86_64
                    && elf.is_little_endian(),
                "bootstrap requires x86-64 ELF"
            );
            (input, None)
        };
        let path = elf_interpreter(&program.bytes)?;
        let interpreter = path.as_deref().map(HeldObject::open).transpose()?;
        Ok(Self {
            loader,
            program,
            interpreter,
            script,
            config: config.clone(),
            layout,
        })
    }

    pub(super) fn initializes_random(&self) -> bool {
        self.interpreter.is_some()
    }

    fn authenticate_script(&self, pid: Pid) -> Result<()> {
        if let Some(script) = &self.script {
            script.authenticate_path(pid)?;
        }
        Ok(())
    }
}

struct SigillOrigin {
    generation: u64,
    registers: libc::user_regs_struct,
    mapping: Map,
}

struct VdsoSnapshot {
    mapping: Map,
    bytes: Vec<u8>,
}

/// State is single-root/single-image until TAKE. Ordinary post-handoff process
/// and robust-exit accounting remains in the existing supervisor.
pub(super) struct Bootstrap {
    launch: Launch,
    root: Pid,
    generation: u64,
    image: Option<InitialImage>,
    prng: rand_pcg::Pcg64Mcg,
    taken: bool,
    sigill: Option<SigillOrigin>,
    initial_random: usize,
    expected_arguments: Vec<Vec<u8>>,
    vdso: Option<VdsoSnapshot>,
    other_objects: Vec<HeldObject>,
    // Only real kernel EXEC events create these entries. They are removed on
    // successful TAKE or final physical exit, never copied across fork.
    continuations: BTreeMap<Pid, InitialImage>,
    static_reexec_observed: bool,
}

#[derive(Clone, Copy, Debug)]
struct FrameLayout {
    size: usize,
    rdi: usize,
    rsi: usize,
    rdx: usize,
    architectural_return: usize,
    scratch_return: usize,
}
impl FrameLayout {
    fn decode(bytes: &[u8]) -> Result<Self> {
        ensure!(
            bytes.len() == 72
                && word(bytes, 0)? == 1
                && word(bytes, 56)? == 8
                && word(bytes, 64)? == 9,
            "unsupported bootstrap frame descriptor"
        );
        let size = word(bytes, 8)?;
        ensure!(
            (8..=4096).contains(&size) && size.is_multiple_of(8),
            "invalid bootstrap full frame size"
        );
        let offsets = [
            word(bytes, 16)?,
            word(bytes, 24)?,
            word(bytes, 32)?,
            word(bytes, 40)?,
            word(bytes, 48)?,
        ];
        ensure!(
            offsets
                .iter()
                .all(|o| o.is_multiple_of(8) && o.checked_add(8).is_some_and(|end| end <= size)),
            "bootstrap frame field outside extent"
        );
        ensure!(
            offsets.iter().copied().collect::<BTreeSet<_>>().len() == offsets.len(),
            "overlapping bootstrap frame fields"
        );
        Ok(Self {
            size,
            rdi: offsets[0],
            rsi: offsets[1],
            rdx: offsets[2],
            architectural_return: offsets[3],
            scratch_return: offsets[4],
        })
    }
}

fn auxv_random(bytes: &[u8]) -> Result<usize> {
    ensure!(bytes.len().is_multiple_of(16), "short initial kernel auxv");
    let mut random = None;
    for pair in bytes.as_chunks::<16>().0 {
        let kind = word(pair, 0)?;
        let value = word(pair, 8)?;
        if kind == libc::AT_NULL as usize {
            ensure!(value == 0, "invalid auxv terminator");
            return random.ok_or_else(|| anyhow!("initial kernel auxv has no AT_RANDOM"));
        }
        if kind == libc::AT_RANDOM as usize {
            ensure!(
                value != 0 && random.replace(value).is_none(),
                "ambiguous initial kernel AT_RANDOM"
            );
        }
    }
    Err(anyhow!("unterminated initial kernel auxv"))
}

fn initial_vdso<'a>(rows: &'a [Map], auxv: &[u8]) -> Result<Option<&'a Map>> {
    ensure!(auxv.len().is_multiple_of(16), "short initial kernel auxv");
    let mut address = None;
    let mut terminated = false;
    for pair in auxv.as_chunks::<16>().0 {
        let kind = word(pair, 0)?;
        let value = word(pair, 8)?;
        if kind == libc::AT_NULL as usize {
            ensure!(value == 0, "invalid auxv terminator");
            terminated = true;
            break;
        }
        if kind == libc::AT_SYSINFO_EHDR as usize {
            ensure!(
                address.replace(value).is_none(),
                "ambiguous initial vDSO auxv"
            );
        }
    }
    ensure!(terminated, "unterminated initial kernel auxv");
    let mut candidates = rows.iter().filter(|row| row.path == b"[vdso]");
    let mapping = candidates.next();
    ensure!(candidates.next().is_none(), "ambiguous initial kernel vDSO");
    match (address.filter(|value| *value != 0), mapping) {
        (None, None) => Ok(None),
        (Some(address), Some(row)) => {
            ensure!(address == row.start, "initial vDSO auxv/mapping mismatch");
            ensure!(
                row.permissions == "r-xp" && row.inode == 0 && row.offset == 0,
                "unsupported initial vDSO mapping"
            );
            Ok(Some(row))
        }
        _ => Err(anyhow!("initial vDSO auxv/mapping mismatch")),
    }
}

impl Bootstrap {
    pub(super) fn new(root: Pid, launch: Launch) -> Result<Self> {
        let generation = generation(root)?;
        ensure!(generation != 0, "invalid initial process generation");
        let rows = maps(root)?;
        let expected_arguments = if launch.initializes_random() {
            let registers = nix::sys::ptrace::getregs(root)?;
            guest_arguments(
                initial_arguments(root, &rows, registers.rsp as usize)?,
                launch.script.as_ref().map(|script| script.bytes.as_slice()),
            )?
        } else {
            // Static legacy TAKE has no IMAGE/argv proof. Preserve its
            // existing admission instead of imposing the dynamic argc bound.
            Vec::new()
        };
        let auxv = read_path(format!("/proc/{root}/auxv"), 4096)?;
        // The caller still owns the initial attach stop. Capture pristine
        // bytes now; absence requires agreement between maps and kernel auxv,
        // and cannot stand in for an unreadable or already rewritten vDSO.
        let vdso = initial_vdso(&rows, &auxv)?
            .map(|mapping| {
                let bytes = remote_bytes(root, mapping.start, mapping.end - mapping.start)?;
                let elf = object::File::parse(bytes.as_slice())?;
                ensure!(
                    elf.architecture() == object::Architecture::X86_64 && elf.is_little_endian(),
                    "invalid initial kernel vDSO ELF"
                );
                Ok::<_, anyhow::Error>(VdsoSnapshot {
                    mapping: mapping.clone(),
                    bytes,
                })
            })
            .transpose()?;
        let initial_random = auxv_random(&auxv)?;
        ensure!(
            self::generation(root)? == generation,
            "initial process generation changed"
        );
        let prng = root_prng(launch.config.rng_seed());
        Ok(Self {
            launch,
            root,
            generation,
            image: None,
            prng,
            taken: false,
            sigill: None,
            initial_random,
            expected_arguments,
            vdso,
            other_objects: Vec::new(),
            continuations: BTreeMap::new(),
            static_reexec_observed: false,
        })
    }

    fn same_owner(&self, pid: Pid) -> Result<()> {
        ensure!(
            pid == self.root && generation(pid)? == self.generation,
            "bootstrap owner or process generation changed"
        );
        Ok(())
    }

    fn syscall_site(&self, pid: Pid, rows: &[Map], site: usize) -> Result<usize> {
        let loader = &self.launch.loader;
        let bias = loader.bias_at(pid, rows, site, "loader private syscall")?;
        let symbol = loader.symbol(bootstrap::SYSCALL_SYMBOL)?;
        ensure!(
            site == bias
                .checked_add(symbol)
                .ok_or_else(|| anyhow!("loader syscall address overflow"))?,
            "private bootstrap request is not at the retained loader instruction"
        );
        let row = containing(rows, site, 2)?;
        ensure!(
            row.permissions.contains('x'),
            "private bootstrap instruction is not executable"
        );
        loader.authenticate(pid, row)?;
        ensure!(
            loader.at_virtual(symbol, 2, false)? == [0x0f, 0x05]
                && remote_bytes(pid, site, 2)? == [0x0f, 0x05],
            "private bootstrap instruction bytes changed"
        );
        // Recheck the complete immutable descriptor in its live readonly load
        // mapping as well as the pre-launch held-file extraction.
        let descriptor = loader.symbol(LAYOUT_SYMBOL)?;
        let address = bias
            .checked_add(descriptor)
            .ok_or_else(|| anyhow!("descriptor address overflow"))?;
        let mapping = containing(rows, address, 72)?;
        ensure!(
            mapping.permissions.starts_with("r-") && !mapping.permissions.contains('w'),
            "live bootstrap descriptor is writable"
        );
        loader.authenticate(pid, mapping)?;
        ensure!(
            remote_bytes(pid, address, 72)? == loader.at_virtual(descriptor, 72, true)?,
            "live bootstrap descriptor differs from held object"
        );
        Ok(bias)
    }

    fn image(&mut self, pid: Pid, rows: &[Map], stack: usize, entry: usize) -> Result<i64> {
        ensure!(
            self.image.is_none() && !self.taken,
            "duplicate bootstrap IMAGE"
        );
        ensure!(stack.is_multiple_of(16), "unaligned final guest stack");
        let stack_map = containing(rows, stack, 8)?;
        ensure!(
            stack_map.permissions.starts_with("rw") && stack_map.path == b"[stack]",
            "final guest stack is not the owned writable stack"
        );
        let length = (stack_map.end - stack).min(64 * 1024);
        let bytes = remote_bytes(pid, stack, length)?;
        let mut at = authenticate_arguments(pid, rows, &bytes, &self.expected_arguments)?;
        let mut env_count = 0;
        loop {
            let pointer = word(&bytes, at)?;
            at += 8;
            if pointer == 0 {
                break;
            }
            env_count += 1;
            ensure!(
                env_count <= 4096 && stack_map.contains(pointer, 1),
                "unsupported final environment extent"
            );
        }
        let mut aux = std::collections::BTreeMap::new();
        let mut terminated = false;
        for _ in 0..128 {
            let kind = word(&bytes, at)?;
            let value = word(&bytes, at + 8)?;
            at += 16;
            if kind == libc::AT_NULL as usize {
                ensure!(value == 0, "bad final auxv terminator");
                terminated = true;
                break;
            }
            ensure!(
                aux.insert(kind, value).is_none(),
                "duplicate final auxv entry"
            );
        }
        ensure!(terminated, "unterminated final auxv");
        let get = |key: u64| {
            aux.get(&(key as usize))
                .copied()
                .ok_or_else(|| anyhow!("missing final auxv field {key}"))
        };
        let random = get(libc::AT_RANDOM)?;
        ensure!(
            random == self.initial_random && stack_map.contains(random, 16),
            "final AT_RANDOM is not the original owned writable target"
        );
        let program = &self.launch.program;
        let bias = program.bias_at(pid, rows, get(libc::AT_ENTRY)?, "program entry")?;
        let elf = object::File::parse(program.bytes.as_slice())?;
        let expected_entry = bias
            .checked_add(elf.entry() as usize)
            .ok_or_else(|| anyhow!("guest entry overflow"))?;
        ensure!(
            get(libc::AT_ENTRY)? == expected_entry,
            "final guest entry differs from held ELF"
        );
        let entry_map = containing(rows, expected_entry, 1)?;
        program.authenticate(pid, entry_map)?;
        ensure!(
            entry_map.permissions.contains('x'),
            "guest entry not executable"
        );
        program.program_headers(
            pid,
            rows,
            bias,
            get(libc::AT_PHDR)?,
            get(libc::AT_PHNUM)?,
            get(libc::AT_PHENT)?,
        )?;
        self.launch.authenticate_script(pid)?;
        let interpreter = self
            .launch
            .interpreter
            .as_ref()
            .ok_or_else(|| anyhow!("initial static image must not request random IMAGE"))?;
        let interp_bias = interpreter.bias_at(pid, rows, entry, "guest interpreter entry")?;
        let interp = object::File::parse(interpreter.bytes.as_slice())?;
        ensure!(
            entry
                == interp_bias
                    .checked_add(interp.entry() as usize)
                    .ok_or_else(|| anyhow!("interpreter entry overflow"))?,
            "loader final entry is not held guest interpreter"
        );
        let row = containing(rows, entry, 1)?;
        interpreter.authenticate(pid, row)?;
        ensure!(
            row.permissions.contains('x'),
            "interpreter entry not executable"
        );
        initialize_auxv(
            &mut self.prng,
            RemoteMemory(pid),
            AddrMut::from_raw(random).ok_or_else(|| anyhow!("null final AT_RANDOM"))?,
            detcore::types::DetTid::from_raw(pid.as_raw()),
        )?;
        self.image = Some(InitialImage {
            pid: pid.as_raw(),
            start_time_ticks: self.generation,
            at_random: random,
        });
        Ok(0)
    }

    fn original_bytes(
        &mut self,
        pid: Pid,
        rows: &[Map],
        site: usize,
        length: usize,
    ) -> Result<Vec<u8>> {
        ensure!(length <= 128, "original instruction read exceeds bound");
        let row = containing(rows, site, length)?.clone();
        ensure!(
            row.permissions.contains('x'),
            "early getrandom source is not executable"
        );
        if row.path == b"[vdso]" {
            let vdso = self
                .vdso
                .as_ref()
                .ok_or_else(|| anyhow!("vDSO origin lacks an initial snapshot"))?;
            ensure!(row == vdso.mapping, "initial kernel vDSO mapping changed");
            let offset = site - vdso.mapping.start;
            Ok(vdso
                .bytes
                .get(offset..offset + length)
                .ok_or_else(|| anyhow!("short original vDSO instruction range"))?
                .to_vec())
        } else {
            let object = if self.launch.program.matches_mapping(&row) {
                &self.launch.program
            } else if let Some(interpreter) = self.launch.interpreter.as_ref()
                && interpreter.matches_mapping(&row)
            {
                interpreter
            } else {
                if !self
                    .other_objects
                    .iter()
                    .any(|object| object.matches_mapping(&row))
                {
                    ensure!(
                        self.other_objects.len() < MAX_OBJECTS,
                        "too many early bootstrap source objects"
                    );
                    let object = HeldObject::open(&row.mapped_path(pid)?)?;
                    object.authenticate(pid, &row)?;
                    self.other_objects.push(object);
                }
                self.other_objects
                    .iter()
                    .find(|object| object.matches_mapping(&row))
                    .ok_or_else(|| anyhow!("bootstrap source object identity changed"))?
            };
            object.authenticate(pid, &row)?;
            let bias = object.bias_at(pid, rows, site, "original instruction")?;
            let relative = site
                .checked_sub(bias)
                .ok_or_else(|| anyhow!("source bias underflow"))?;
            Ok(object.at_virtual(relative, length, false)?.to_vec())
        }
    }

    fn original_syscall(&mut self, pid: Pid, rows: &[Map], site: usize) -> Result<Map> {
        ensure!(
            self.original_bytes(pid, rows, site, 2)? == [0x0f, 0x05],
            "early source was not a syscall in the authenticated image"
        );
        Ok(containing(rows, site, 2)?.clone())
    }

    /// Save only a real kernel signal-delivery observation. A later private
    /// request's argument values or supplied stack pointer cannot create it.
    pub(super) fn signal(&mut self, pid: Pid, signal: nix::sys::signal::Signal) -> Result<()> {
        ensure!(
            self.sigill.take().is_none(),
            "intervening signal invalidated bootstrap SIGILL provenance"
        );
        if self.taken
            || !self.launch.initializes_random()
            || signal != nix::sys::signal::Signal::SIGILL
        {
            return Ok(());
        }
        self.same_owner(pid)?;
        let regs = nix::sys::ptrace::getregs(pid)?;
        if regs.rax != libc::SYS_getrandom as u64
            || remote_bytes(pid, regs.rip as usize, 2)? != [0x0f, 0xff]
        {
            return Ok(());
        }
        ensure!(
            self.image.is_some(),
            "getrandom SIGILL before authenticated IMAGE"
        );
        let info = nix::sys::ptrace::getsiginfo(pid)?;
        ensure!(
            info.si_code > 0 && unsafe { info.si_addr() } as usize == regs.rip as usize,
            "SIGILL is not a kernel fault at the rewritten source"
        );
        let rows = maps(pid)?;
        let mapping = self.original_syscall(pid, &rows, regs.rip as usize)?;
        self.sigill = Some(SigillOrigin {
            generation: self.generation,
            registers: regs,
            mapping,
        });
        Ok(())
    }

    fn origin(
        &mut self,
        pid: Pid,
        rows: &[Map],
        arguments: [usize; 3],
        wrapper: usize,
        loader_bias: usize,
    ) -> Result<()> {
        let layout = self.launch.layout;
        if let Some(saved) = self.sigill.take() {
            ensure!(
                saved.generation == self.generation,
                "stale bootstrap SIGILL generation"
            );
            let regs = saved.registers;
            ensure!(
                [regs.rdi as usize, regs.rsi as usize, regs.rdx as usize] == arguments,
                "SIGILL original arguments differ from forwarded request"
            );
            ensure!(
                self.original_syscall(pid, rows, regs.rip as usize)? == saved.mapping
                    && remote_bytes(pid, regs.rip as usize, 2)? == [0x0f, 0xff],
                "SIGILL source changed before forwarding"
            );
            let address = wrapper
                .checked_add(layout.scratch_return)
                .ok_or_else(|| anyhow!("SIGILL return pointer overflow"))?;
            let row = containing(rows, address, 8)?;
            ensure!(
                row.path == b"[stack]" && row.permissions.starts_with("rw"),
                "SIGILL return word is not on owned stack"
            );
            let returned = word(&remote_bytes(pid, address, 8)?, 0)?;
            ensure!(
                returned == regs.rip as usize + 2,
                "SIGILL return word differs from saved kernel continuation"
            );
            return Ok(());
        }
        let row = containing(rows, wrapper, layout.size)?;
        ensure!(
            row.path == b"[stack]"
                && row.permissions.starts_with("rw")
                && wrapper.is_multiple_of(8),
            "ordinary bootstrap frame is not on owned stack"
        );
        let frame = remote_bytes(pid, wrapper, layout.size)?;
        ensure!(
            [
                word(&frame, layout.rdi)?,
                word(&frame, layout.rsi)?,
                word(&frame, layout.rdx)?
            ] == arguments,
            "assembly frame arguments differ from forwarded request"
        );
        let returned = word(&frame, layout.architectural_return)?;
        let site = returned
            .checked_sub(2)
            .ok_or_else(|| anyhow!("architectural return underflow"))?;
        self.original_syscall(pid, rows, site)?;
        let scratch = word(&frame, layout.scratch_return)?;
        // Real rewriter.c's syscall trampoline: saved return points at the
        // red-zone-restoring LEA, 33 bytes after its PUSH/LEA/body begins.
        let start = scratch
            .checked_sub(33)
            .ok_or_else(|| anyhow!("scratch return underflow"))?;
        let mapping = containing(rows, start, 41)?;
        ensure!(
            mapping.permissions.contains('x'),
            "scratch continuation is not executable"
        );
        let code = remote_bytes(pid, start, 41)?;
        ensure!(
            code[..4] == [0x50, 0x48, 0x8d, 0x05]
                && code[8..11] == [0x50, 0x48, 0xb8]
                && code[19..33]
                    == [
                        0x50, 0x48, 0x8d, 0x05, 0x06, 0, 0, 0, 0x48, 0x87, 0x44, 0x24, 0x10, 0xc3
                    ]
                && code[33..] == [0x48, 0x8d, 0xa4, 0x24, 0x80, 0, 0, 0],
            "unrecognized full-frame scratch trampoline"
        );
        let displacement = i32::from_le_bytes(code[4..8].try_into()?) as i64;
        ensure!(
            (start as i64)
                .checked_add(8)
                .and_then(|n| n.checked_add(displacement))
                == Some(returned as i64),
            "scratch trampoline names a different syscall continuation"
        );
        let handler = word(&code, 11)?;
        let bias = loader_bias;
        let mut handlers = BTreeSet::new();
        for symbol in ["handle_syscall", "handle_syscall_loader"] {
            handlers.insert(
                bias.checked_add(self.launch.loader.symbol(symbol)?)
                    .ok_or_else(|| anyhow!("handler address overflow"))?,
            );
        }
        ensure!(
            handlers.contains(&handler),
            "scratch trampoline does not call the held loader wrapper"
        );
        // Bind the source jump to this exact scratch body, including the real
        // SYSCALL clobber/red-zone prefix and both relocated byte sequences.
        // The production rewriter keeps five instructions in its ring; four
        // preceding/following x86 instructions occupy at most 4 * 15 bytes.
        // A matching frame and an unrelated executable byte pattern alone are
        // not evidence that this original instruction enters that trampoline.
        let prefix = start
            .checked_sub(17)
            .ok_or_else(|| anyhow!("scratch prefix underflow"))?;
        let prefix_bytes = remote_bytes(pid, prefix, 17)?;
        ensure!(
            prefix_bytes[..13]
                == [
                    0x48, 0x8d, 0x64, 0x24, 0x80, 0x90, 0x90, 0x9c, 0x41, 0x5b, 0x48, 0x8d, 0x0d,
                ]
                && (start as i64)
                    .checked_add(i32::from_le_bytes(prefix_bytes[13..17].try_into()?) as i64)
                    == Some(returned as i64),
            "scratch prefix does not preserve the original SYSCALL continuation"
        );
        let source_map = containing(rows, site, 2)?;
        let mut matched = 0;
        for pre in 0..=60 {
            let Some(jump) = site.checked_sub(pre) else {
                continue;
            };
            let Some(destination) = prefix.checked_sub(pre) else {
                continue;
            };
            if !source_map.contains(jump, 5) {
                continue;
            }
            let entry = remote_bytes(pid, jump, 5)?;
            if entry[0] != 0xe9
                || (jump as i64 + 5).checked_add(i32::from_le_bytes(entry[1..5].try_into()?) as i64)
                    != Some(destination as i64)
            {
                continue;
            }
            ensure!(
                containing(rows, destination, pre + 17)?
                    .permissions
                    .contains('x'),
                "scratch entry is not executable"
            );
            if pre != 0
                && self.original_bytes(pid, rows, jump, pre)?
                    != remote_bytes(pid, destination, pre)?
            {
                continue;
            }
            for post in 0..=60 {
                if pre + 2 + post < 5 || !source_map.contains(jump, pre + 2 + post) {
                    continue;
                }
                let after = start + 41 + post;
                if !mapping.contains(after, 5) {
                    continue;
                }
                let exit = remote_bytes(pid, after, 5)?;
                if exit[0] != 0xe9
                    || (after as i64 + 5)
                        .checked_add(i32::from_le_bytes(exit[1..5].try_into()?) as i64)
                        != Some((returned + post) as i64)
                {
                    continue;
                }
                if post != 0
                    && self.original_bytes(pid, rows, returned, post)?
                        != remote_bytes(pid, start + 41, post)?
                {
                    continue;
                }
                if pre + 2 + post > 5
                    && remote_bytes(pid, jump + 5, pre + 2 + post - 5)?
                        .iter()
                        .any(|byte| *byte != 0x90)
                {
                    continue;
                }
                matched += 1;
            }
        }
        ensure!(
            matched == 1,
            "missing/ambiguous rewritten source-to-scratch linkage"
        );
        ensure!(
            remote_bytes(pid, site, 2)? != [0x0f, 0x05]
                && remote_bytes(pid, site, 2)? != [0x0f, 0xff],
            "ordinary assembly path lacks a rewritten source site"
        );
        Ok(())
    }

    /// None leaves the existing supervisor path untouched. Protocol ownership,
    /// phase and shape errors abort the run; syscall errors remain Linux results.
    pub(super) fn request(
        &mut self,
        pid: Pid,
        regs: &libc::user_regs_struct,
    ) -> Result<Option<i64>> {
        let private =
            regs.orig_rax == libc::SYS_prctl as u64 && regs.rdi == bootstrap::PRCTL_OPTION;
        if !private {
            ensure!(
                self.sigill.take().is_none(),
                "intervening syscall invalidated bootstrap SIGILL provenance"
            );
            return Ok(None);
        }
        let continuation = self.continuations.get(&pid).copied();
        if let Some(image) = continuation {
            ensure!(
                generation(pid)? == image.start_time_ticks,
                "continuation owner or process generation changed"
            );
        } else {
            self.same_owner(pid)?;
        }
        let rows = maps(pid)?;
        let site = (regs.rip as usize)
            .checked_sub(2)
            .ok_or_else(|| anyhow!("private syscall RIP underflow"))?;
        let loader_bias = self.syscall_site(pid, &rows, site)?;
        let initial_static = !self.taken && !self.launch.initializes_random();
        if continuation.is_some() || initial_static {
            ensure!(
                self.sigill.is_none()
                    && regs.rsi == bootstrap::TAKE_STATE
                    && regs.r8 == bootstrap::VERSION
                    && regs.r9 == 0,
                "legacy continuation requires the exact TAKE protocol"
            );
            let image = if let Some(image) = continuation {
                image
            } else {
                // Static clients initialize their plugin before final-stack
                // compaction. Authenticate the actual mapped held program,
                // while leaving their ordinary auxv/ThreadState path alone.
                self.launch.authenticate_script(pid)?;
                let program = &self.launch.program;
                let bias = program.bias(pid, &rows)?;
                let elf = object::File::parse(program.bytes.as_slice())?;
                let entry = bias
                    .checked_add(elf.entry() as usize)
                    .ok_or_else(|| anyhow!("static entry overflow"))?;
                let row = containing(&rows, entry, 1)?;
                ensure!(
                    row.permissions.contains('x'),
                    "held static entry not executable"
                );
                program.authenticate(pid, row)?;
                InitialImage {
                    pid: pid.as_raw(),
                    start_time_ticks: self.generation,
                    at_random: self.initial_random,
                }
            };
            self.check_current_auxv(pid, &rows, image)?;
            let state = if initial_static {
                LoaderState::InitialStaticLegacy
            } else {
                LoaderState::ObservedExecContinuation
            };
            let bytes = encode_continuation(&self.launch.config, image, state)?;
            let result = Self::write_take(pid, regs, &bytes)?;
            if result > 0 {
                if initial_static {
                    self.taken = true;
                }
                self.continuations.remove(&pid);
            }
            return Ok(Some(result));
        }
        ensure!(!self.taken, "bootstrap state already handed off");
        let result = match regs.rsi {
            bootstrap::IMAGE => {
                ensure!(
                    self.sigill.is_none() && regs.r8 == bootstrap::VERSION && regs.r9 == 0,
                    "invalid IMAGE protocol shape"
                );
                self.image(pid, &rows, regs.rdx as usize, regs.r10 as usize)
                    .map_err(|error| anyhow!("bootstrap IMAGE authentication: {error:#}"))?
            }
            bootstrap::GETRANDOM => {
                ensure!(self.image.is_some(), "early getrandom before IMAGE");
                let args = [regs.rdx as usize, regs.r10 as usize, regs.r8 as usize];
                self.origin(pid, &rows, args, regs.r9 as usize, loader_bias)?;
                let call = Syscall::from_raw(
                    Sysno::getrandom,
                    SyscallArgs::new(args[0], args[1], args[2], 0, 0, 0),
                );
                let Syscall::Getrandom(call) = call else {
                    unreachable!()
                };
                match getrandom(
                    &mut self.prng,
                    RemoteMemory(pid),
                    detcore::types::DetTid::from_raw(pid.as_raw()),
                    call,
                ) {
                    Ok(n) => n,
                    Err(e) => -(e.into_raw() as i64),
                }
            }
            bootstrap::TAKE_STATE => {
                ensure!(
                    self.sigill.is_none() && regs.r8 == bootstrap::VERSION && regs.r9 == 0,
                    "invalid TAKE protocol shape"
                );
                let image = self.image.ok_or_else(|| anyhow!("TAKE before IMAGE"))?;
                self.check_current_auxv(pid, &rows, image)?;
                let bytes = encode_initial_state(&self.launch.config, image, &self.prng)?;
                let result = Self::write_take(pid, regs, &bytes)?;
                if result > 0 {
                    self.taken = true;
                }
                result
            }
            _ => return Err(anyhow!("unknown loader bootstrap operation")),
        };
        Ok(Some(result))
    }

    fn write_take(pid: Pid, regs: &libc::user_regs_struct, bytes: &[u8]) -> Result<i64> {
        let capacity = regs.r10 as usize;
        let destination = AddrMut::from_raw(regs.rdx as usize);
        if capacity == 0 || capacity > bootstrap::MAX_STATE_BYTES || destination.is_none() {
            return Ok(-libc::EINVAL as i64);
        }
        if bytes.len() > capacity {
            return Ok(-libc::EMSGSIZE as i64);
        }
        Ok(
            match RemoteMemory(pid).write_exact(destination.unwrap(), bytes) {
                Ok(()) => bytes.len() as i64,
                Err(e) => -(e.into_raw() as i64),
            },
        )
    }

    fn check_current_auxv(&self, pid: Pid, rows: &[Map], image: InitialImage) -> Result<()> {
        ensure!(
            generation(pid)? == image.start_time_ticks
                && pid.as_raw() == image.pid
                && auxv_random(&read_path(format!("/proc/{pid}/auxv"), 4096)?)? == image.at_random,
            "loader handoff image or kernel auxv changed"
        );
        let row = containing(rows, image.at_random, 16)?;
        ensure!(
            row.path == b"[stack]" && row.permissions.starts_with("rw"),
            "loader handoff AT_RANDOM is not the owned writable stack"
        );
        Ok(())
    }

    pub(super) fn forget(&mut self, pid: Pid) {
        self.continuations.remove(&pid);
    }

    /// Called only for a real kernel event on the supervisor's owned lineage,
    /// before it resumes that image. An exec replaces any unconsumed earlier
    /// image proof; a mere matching pid/start or user-supplied payload cannot
    /// create one. Entries live no longer than the existing owned tracee set.
    pub(super) fn event(&mut self, pid: Pid, event: libc::c_int) -> Result<()> {
        ensure!(
            self.sigill.take().is_none(),
            "intervening ptrace event invalidated bootstrap SIGILL provenance"
        );
        if event == libc::PTRACE_EVENT_EXEC {
            let image = InitialImage {
                pid: pid.as_raw(),
                start_time_ticks: generation(pid)?,
                at_random: auxv_random(&read_path(format!("/proc/{pid}/auxv"), 4096)?)?,
            };
            self.check_current_auxv(pid, &maps(pid)?, image)?;
            if self.taken {
                self.continuations.insert(pid, image);
                return Ok(());
            }
            if !self.launch.initializes_random() && !self.static_reexec_observed {
                self.same_owner(pid)?;
                // The static loader may exec its dynamic linker once to
                // preload the plugin. This does not authorize arbitrary initial
                // dynamic execution: TAKE must still prove the original held
                // static program and exact loader mapping before succeeding.
                self.static_reexec_observed = true;
                self.initial_random = image.at_random;
                return Ok(());
            }
        }
        ensure!(
            self.taken
                || !matches!(
                    event,
                    libc::PTRACE_EVENT_CLONE
                        | libc::PTRACE_EVENT_FORK
                        | libc::PTRACE_EVENT_VFORK
                        | libc::PTRACE_EVENT_EXEC
                ),
            "clone/fork/exec before initial random handoff is unsupported"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::io::Seek;
    use std::io::SeekFrom;
    use std::io::Write;

    use super::*;

    #[test]
    fn generation_accepts_raw_worker_comm_and_rejects_invalid_identity() {
        struct RestoreComm {
            tid: i32,
            name: [u8; 16],
        }
        impl Drop for RestoreComm {
            fn drop(&mut self) {
                assert_eq!(unsafe { libc::syscall(libc::SYS_gettid) } as i32, self.tid);
                assert_eq!(
                    unsafe { libc::prctl(libc::PR_SET_NAME, self.name.as_ptr()) },
                    0
                );
            }
        }
        // Change only this libtest worker's name, never the process leader or
        // another test's thread. This is a native reader control, not a guest.
        let tid = unsafe { libc::syscall(libc::SYS_gettid) } as i32;
        let pid = Pid::from_raw(tid);
        let expected = generation(pid).unwrap();
        let mut restore = RestoreComm { tid, name: [0; 16] };
        assert_eq!(
            unsafe { libc::prctl(libc::PR_GET_NAME, restore.name.as_mut_ptr()) },
            0
        );
        assert_eq!(
            unsafe { libc::prctl(libc::PR_SET_NAME, c"raw) \xff) task".as_ptr()) },
            0
        );
        let bytes = read_path(format!("/proc/{pid}/stat"), 4096).unwrap();
        // These are actual kernel bytes rejected by the former whole-file
        // UTF-8 conversion, including an embedded closing delimiter.
        assert!(std::str::from_utf8(&bytes).is_err());
        assert!(bytes.windows(12).any(|row| row == b"raw) \xff) task"));
        assert_eq!(generation(pid).unwrap(), expected);
        assert_eq!(stat_generation(pid, &bytes).unwrap(), expected);
        expect_error(
            stat_generation(Pid::from_raw(tid + 1), &bytes),
            "owner mismatch",
        );
        expect_error(stat_generation(pid, b"missing delimiter"), "malformed");
        let end = bytes.windows(2).rposition(|row| row == b") ").unwrap();
        let mut fields: Vec<&[u8]> = bytes[end + 2..]
            .split(u8::is_ascii_whitespace)
            .filter(|field| !field.is_empty())
            .collect();
        let mut malformed = bytes[..end + 2].to_vec();
        fields[19] = b"not-a-number";
        malformed.extend(fields.join(&b' '));
        assert!(stat_generation(pid, &malformed).is_err());
        expect_error(
            stat_generation(pid, &bytes[..end + 2]),
            "missing process generation",
        );
        let owner_end = bytes.windows(2).position(|row| row == b" (").unwrap();
        let mut malformed_owner = b"not-a-pid".to_vec();
        malformed_owner.extend_from_slice(&bytes[owner_end..]);
        assert!(stat_generation(pid, &malformed_owner).is_err());
        drop(restore);
    }

    struct Mapping {
        address: *mut libc::c_void,
        _file: File,
    }

    impl Mapping {
        fn new(path: &Path) -> Self {
            let file = File::open(path).unwrap();
            let address = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    PAGE,
                    libc::PROT_READ,
                    libc::MAP_PRIVATE,
                    file.as_raw_fd(),
                    0,
                )
            };
            assert_ne!(address, libc::MAP_FAILED);
            Self {
                address,
                _file: file,
            }
        }
    }

    impl Drop for Mapping {
        fn drop(&mut self) {
            assert_eq!(unsafe { libc::munmap(self.address, PAGE) }, 0);
        }
    }

    fn fixture_file(directory: &Path, name: &[u8]) -> PathBuf {
        let path = directory.join(std::ffi::OsStr::from_bytes(name));
        std::fs::write(&path, [0x5a; PAGE]).unwrap();
        path
    }

    fn expect_error<T>(result: Result<T>, expected: &str) {
        let error = result.err().expect("invalid bootstrap input was accepted");
        assert!(
            error.to_string().contains(expected),
            "expected {expected:?}, got {error:#}"
        );
    }

    fn unrelated_mapping(name: &[u8], deleted: bool) {
        let directory = tempfile::tempdir().unwrap();
        let clean = fixture_file(directory.path(), b"selected");
        let unrelated = fixture_file(directory.path(), name);
        let _mapping = Mapping::new(&unrelated);
        if deleted {
            std::fs::remove_file(&unrelated).unwrap();
        }
        // Exercise the actual /proc reader while the unrelated VMA is live.
        let held = HeldObject::open_file(&clean).unwrap();
        held.authenticate_path(Pid::this()).unwrap();
    }

    #[test]
    fn unrelated_space_mapping_preserves_selected_object() {
        unrelated_mapping(b"unrelated space", false);
    }

    #[test]
    fn unrelated_deleted_mapping_preserves_selected_object() {
        unrelated_mapping(b"unrelated-deleted", true);
    }

    #[test]
    fn unrelated_non_utf8_mapping_preserves_selected_object() {
        unrelated_mapping(b"unrelated-\xff", false);
    }

    #[test]
    fn unrelated_newline_mapping_preserves_selected_object() {
        unrelated_mapping(b"unrelated-\n-name", false);
    }

    #[test]
    fn selected_mapping_authenticates_identity_and_content() {
        let directory = tempfile::tempdir().unwrap();
        let path = fixture_file(directory.path(), b"selected");
        let held = HeldObject::open_file(&path).unwrap();
        let mapping = Mapping::new(&path);
        let rows = maps(Pid::this()).unwrap();
        held.authenticate(
            Pid::this(),
            containing(&rows, mapping.address as usize, 1).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn selected_mapping_rejects_same_bytes_in_another_object() {
        let directory = tempfile::tempdir().unwrap();
        let path = fixture_file(directory.path(), b"selected");
        let held = HeldObject::open_file(&path).unwrap();
        let other = fixture_file(directory.path(), b"same-bytes-distinct-object");
        let mapping = Mapping::new(&other);
        let rows = maps(Pid::this()).unwrap();
        expect_error(
            held.authenticate(
                Pid::this(),
                containing(&rows, mapping.address as usize, 1).unwrap(),
            ),
            "bootstrap mapped object identity mismatch",
        );
    }

    #[test]
    fn selected_mapping_rejects_changed_content() {
        let directory = tempfile::tempdir().unwrap();
        let path = fixture_file(directory.path(), b"selected");
        let held = HeldObject::open_file(&path).unwrap();
        let mut file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.seek(SeekFrom::Start(128)).unwrap();
        file.write_all(&[held.bytes[128] ^ 1]).unwrap();
        expect_error(
            held.authenticate_path(Pid::this()),
            "bootstrap mapped pathname content changed",
        );
    }

    fn raw_selected_mismatch(name: &[u8]) {
        let directory = tempfile::tempdir().unwrap();
        let path = fixture_file(directory.path(), name);
        let held = HeldObject::open_file(&path).unwrap();
        let other = fixture_file(directory.path(), b"different-object");
        let mapping = Mapping::new(&other);
        let rows = maps(Pid::this()).unwrap();
        expect_error(
            held.authenticate(
                Pid::this(),
                containing(&rows, mapping.address as usize, 1).unwrap(),
            ),
            "bootstrap mapped object identity mismatch",
        );
    }

    #[test]
    fn selected_non_utf8_mapping_rejects_another_object() {
        raw_selected_mismatch(b"selected-\xff");
    }

    #[test]
    fn selected_newline_mapping_rejects_another_object() {
        raw_selected_mismatch(b"selected-\n-name");
    }

    #[test]
    fn selected_deleted_mapping_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        let path = fixture_file(directory.path(), b"selected");
        let held = HeldObject::open_file(&path).unwrap();
        let mapping = Mapping::new(&path);
        std::fs::remove_file(&path).unwrap();
        let rows = maps(Pid::this()).unwrap();
        expect_error(
            held.authenticate(
                Pid::this(),
                containing(&rows, mapping.address as usize, 1).unwrap(),
            ),
            "bootstrap mapped object identity mismatch",
        );
    }

    fn words(values: &[u64]) -> Vec<u8> {
        values
            .iter()
            .flat_map(|value| value.to_le_bytes())
            .collect()
    }

    // The descriptor exported by the landed x86-64 SaBRe C frame definition.
    const FRAME_WORDS: [u64; 9] = [1, 144, 72, 80, 88, 128, 136, 8, 9];

    #[test]
    fn frame_descriptor_decodes_the_exported_layout() {
        let layout = FrameLayout::decode(&words(&FRAME_WORDS)).unwrap();
        assert_eq!(
            (
                layout.size,
                layout.rdi,
                layout.rsi,
                layout.rdx,
                layout.architectural_return,
                layout.scratch_return
            ),
            (144, 72, 80, 88, 128, 136)
        );
    }

    #[test]
    fn frame_descriptor_rejects_invalid_protocol() {
        let bytes = words(&FRAME_WORDS);
        expect_error(
            FrameLayout::decode(&bytes[..71]),
            "unsupported bootstrap frame descriptor",
        );
        let mut extended = bytes;
        extended.extend_from_slice(&0u64.to_le_bytes());
        expect_error(
            FrameLayout::decode(&extended),
            "unsupported bootstrap frame descriptor",
        );
        for (field, value) in [(0, 2), (7, 4), (8, 8)] {
            let mut descriptor = FRAME_WORDS;
            descriptor[field] = value;
            expect_error(
                FrameLayout::decode(&words(&descriptor)),
                "unsupported bootstrap frame descriptor",
            );
        }
    }

    #[test]
    fn frame_descriptor_rejects_invalid_size() {
        for size in [0, 7, 145, 4104] {
            let mut descriptor = FRAME_WORDS;
            descriptor[1] = size;
            expect_error(
                FrameLayout::decode(&words(&descriptor)),
                "invalid bootstrap full frame size",
            );
        }
    }

    #[test]
    fn frame_descriptor_rejects_invalid_offsets() {
        for offset in [73, 144, u64::MAX - 7] {
            let mut descriptor = FRAME_WORDS;
            descriptor[2] = offset;
            expect_error(
                FrameLayout::decode(&words(&descriptor)),
                "bootstrap frame field outside extent",
            );
        }
        let mut descriptor = FRAME_WORDS;
        descriptor[3] = descriptor[2];
        expect_error(
            FrameLayout::decode(&words(&descriptor)),
            "overlapping bootstrap frame fields",
        );
    }

    #[test]
    fn auxv_random_accepts_a_terminated_vector() {
        let bytes = words(&[
            libc::AT_PAGESZ,
            4096,
            libc::AT_RANDOM,
            0x1234,
            libc::AT_NULL,
            0,
        ]);
        assert_eq!(auxv_random(&bytes).unwrap(), 0x1234);
    }

    #[test]
    fn auxv_random_requires_a_valid_terminator_and_random_entry() {
        expect_error(
            auxv_random(&words(&[libc::AT_RANDOM, 0x1234])),
            "unterminated initial kernel auxv",
        );
        expect_error(
            auxv_random(&words(&[libc::AT_RANDOM, 0x1234, libc::AT_NULL, 1])),
            "invalid auxv terminator",
        );
        expect_error(
            auxv_random(&words(&[libc::AT_PAGESZ, 4096, libc::AT_NULL, 0])),
            "initial kernel auxv has no AT_RANDOM",
        );
    }

    #[test]
    fn auxv_random_rejects_duplicate_or_zero_random_entries() {
        expect_error(
            auxv_random(&words(&[
                libc::AT_RANDOM,
                0x1234,
                libc::AT_RANDOM,
                0x5678,
                libc::AT_NULL,
                0,
            ])),
            "ambiguous initial kernel AT_RANDOM",
        );
        expect_error(
            auxv_random(&words(&[libc::AT_RANDOM, 0, libc::AT_NULL, 0])),
            "ambiguous initial kernel AT_RANDOM",
        );
    }

    #[test]
    fn auxv_random_rejects_partial_entries() {
        let bytes = words(&[libc::AT_RANDOM, 0x1234, libc::AT_NULL, 0]);
        for length in [1, 8, 15, 17, 31] {
            expect_error(auxv_random(&bytes[..length]), "short initial kernel auxv");
        }
    }

    #[test]
    fn selected_raw_paths_authenticate_the_held_object() {
        for name in [
            b"space name".as_slice(),
            b"non-utf8-\xff",
            b"newline-\n",
            b"literal-\\012",
            b"back\\slash",
            b"literal (deleted)",
        ] {
            let directory = tempfile::tempdir().unwrap();
            let path = fixture_file(directory.path(), name);
            let held = HeldObject::open_file(&path).unwrap();
            let mapping = Mapping::new(&path);
            let rows = maps(Pid::this()).unwrap();
            let row = containing(&rows, mapping.address as usize, 1).unwrap();
            held.authenticate(Pid::this(), row).unwrap();
            assert_eq!(row.mapped_path(Pid::this()).unwrap(), held.path);
        }
    }

    #[test]
    fn ambiguous_kernel_path_display_does_not_select_another_inode() {
        let directory = tempfile::tempdir().unwrap();
        let newline = fixture_file(directory.path(), b"same-\n");
        let literal = fixture_file(directory.path(), b"same-\\012");
        let a = HeldObject::open_file(&newline).unwrap();
        let b = HeldObject::open_file(&literal).unwrap();
        let ma = Mapping::new(&newline);
        let mb = Mapping::new(&literal);
        let rows = maps(Pid::this()).unwrap();
        let ra = containing(&rows, ma.address as usize, 1).unwrap();
        let rb = containing(&rows, mb.address as usize, 1).unwrap();
        assert_eq!(
            ra.path, rb.path,
            "exercise the real kernel's ambiguous display"
        );
        assert_ne!(ra.inode, rb.inode);
        assert_eq!(ra.mapped_path(Pid::this()).unwrap(), a.path);
        assert_eq!(rb.mapped_path(Pid::this()).unwrap(), b.path);
        a.authenticate(Pid::this(), ra).unwrap();
        b.authenticate(Pid::this(), rb).unwrap();
        expect_error(
            a.authenticate(Pid::this(), rb),
            "bootstrap mapped object identity mismatch",
        );
        expect_error(
            b.authenticate(Pid::this(), ra),
            "bootstrap mapped object identity mismatch",
        );
    }

    #[test]
    fn raw_root_relative_open_preserves_no_follow_and_canonical_guards() {
        let directory = tempfile::tempdir().unwrap();
        let path = fixture_file(directory.path(), b"selected-\xff");
        let link = directory.path().join("alias");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(open_under_root(Pid::this(), &link).is_err());
        expect_error(
            open_under_root(Pid::this(), Path::new("relative")),
            "nonabsolute bootstrap object path",
        );
        expect_error(
            open_under_root(Pid::this(), &directory.path().join("../outside")),
            "noncanonical bootstrap object path",
        );
        assert_eq!(
            open_under_root(Pid::this(), &path)
                .unwrap()
                .metadata()
                .unwrap()
                .ino(),
            path.metadata().unwrap().ino()
        );
    }

    fn executable_fixture(interpreter: Option<&[u8]>) -> Vec<u8> {
        // A real ELF64 ET_EXEC with one executable PT_LOAD, a Linux exit(0)
        // entry, and an optional PT_INTERP. No native execution is needed to
        // test object parsing, pathname identity or trailing file data.
        let mut bytes = vec![0; PAGE];
        bytes[..7].copy_from_slice(b"\x7fELF\x02\x01\x01");
        bytes[16..18].copy_from_slice(&2u16.to_le_bytes());
        bytes[18..20].copy_from_slice(&62u16.to_le_bytes());
        bytes[20..24].copy_from_slice(&1u32.to_le_bytes());
        bytes[24..32].copy_from_slice(&0x400100u64.to_le_bytes());
        bytes[32..40].copy_from_slice(&64u64.to_le_bytes());
        bytes[52..54].copy_from_slice(&64u16.to_le_bytes());
        bytes[54..56].copy_from_slice(&56u16.to_le_bytes());
        bytes[56..58]
            .copy_from_slice(&(if interpreter.is_some() { 2u16 } else { 1 }).to_le_bytes());
        bytes[64..68].copy_from_slice(&1u32.to_le_bytes());
        bytes[68..72].copy_from_slice(&5u32.to_le_bytes());
        bytes[80..88].copy_from_slice(&0x400000u64.to_le_bytes());
        bytes[96..104].copy_from_slice(&(PAGE as u64).to_le_bytes());
        bytes[104..112].copy_from_slice(&(PAGE as u64).to_le_bytes());
        bytes[112..120].copy_from_slice(&(PAGE as u64).to_le_bytes());
        bytes[256..265].copy_from_slice(&[0xb8, 60, 0, 0, 0, 0x31, 0xff, 0x0f, 0x05]);
        if let Some(path) = interpreter {
            bytes[120..124].copy_from_slice(&3u32.to_le_bytes());
            bytes[128..136].copy_from_slice(&512u64.to_le_bytes());
            bytes[152..160].copy_from_slice(&((path.len() + 1) as u64).to_le_bytes());
            bytes[512..512 + path.len()].copy_from_slice(path);
        }
        bytes
    }

    #[test]
    fn initial_exec_arguments_preserve_kernel_bytes_and_client_delimiters() {
        use std::os::unix::process::CommandExt;

        use nix::sys::signal::Signal;
        use nix::sys::wait::WaitStatus;
        use nix::sys::wait::waitpid;
        struct OwnedChild(std::process::Child);
        impl Drop for OwnedChild {
            fn drop(&mut self) {
                // This test remains the sole waiter; the child has not been
                // reaped and cannot denote a reused PID at this point.
                let _ = self.0.kill();
                self.0.wait().expect("reap owned native exec-stop fixture");
            }
        }
        let long = vec![b'a'; 64 * 1024];
        let raw = b"raw-\xff\\012";
        let mut command = std::process::Command::new("/bin/true");
        command.args([
            OsStr::new("plugin"),
            OsStr::new("--"),
            OsStr::new("client"),
            OsStr::new("--"),
            OsStr::from_bytes(&long),
            OsStr::from_bytes(raw),
        ]);
        unsafe {
            command.pre_exec(|| {
                if libc::ptrace(libc::PTRACE_TRACEME, 0, 0, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = OwnedChild(command.spawn().unwrap());
        let pid = Pid::from_raw(child.0.id() as i32);
        assert_eq!(
            waitpid(pid, None).unwrap(),
            WaitStatus::Stopped(pid, Signal::SIGTRAP)
        );
        let registers = nix::sys::ptrace::getregs(pid).unwrap();
        let rows = maps(pid).unwrap();
        let captured = initial_arguments(pid, &rows, registers.rsp as usize).unwrap();
        let direct = guest_arguments(captured.clone(), None).unwrap();
        assert_eq!(direct[0], b"client\0");
        assert_eq!(direct[1], b"--\0");
        assert_eq!(&direct[2][..long.len()], long);
        assert_eq!(direct[2].last(), Some(&0));
        assert_eq!(&direct[3][..raw.len()], raw);
        let script =
            guest_arguments(captured, Some(b"#! /raw-\xff --option ignored\nbody")).unwrap();
        assert_eq!(
            &script[..2],
            &[b"/raw-\xff\0".to_vec(), b"--option\0".to_vec()]
        );
        assert_eq!(&script[2..], direct);
        expect_error(
            guest_arguments(vec![b"loader\0".to_vec(), b"--\0".to_vec()], None),
            "lacks client",
        );
        expect_error(
            guest_arguments(vec![b"loader\0".to_vec(), b"client\0".to_vec()], None),
            "lacks delimiter",
        );
    }

    #[test]
    fn relocated_arguments_require_exact_bytes_order_and_terminators() {
        let expected = vec![
            b"/interpreter-\xff\0".to_vec(),
            b"--option\0".to_vec(),
            b"script\0".to_vec(),
        ];
        let mut actual = expected.clone();
        let vector = |values: &[Vec<u8>]| {
            let mut pointers = vec![values.len() as u64];
            pointers.extend(values.iter().map(|value| value.as_ptr() as u64));
            pointers.push(0);
            words(&pointers)
        };
        let rows = maps(Pid::this()).unwrap();
        assert_ne!(
            containing(&rows, actual[0].as_ptr() as usize, actual[0].len())
                .unwrap()
                .path,
            b"[stack]"
        );
        assert_eq!(
            authenticate_arguments(Pid::this(), &rows, &vector(&actual), &expected).unwrap(),
            40
        );
        actual.swap(0, 1);
        expect_error(
            authenticate_arguments(Pid::this(), &rows, &vector(&actual), &expected),
            "bytes/order changed",
        );
        actual.swap(0, 1);
        actual[0][0] ^= 1;
        expect_error(
            authenticate_arguments(Pid::this(), &rows, &vector(&actual), &expected),
            "bytes/order changed",
        );
        actual[0][0] ^= 1;
        *actual[1].last_mut().unwrap() = b'x';
        expect_error(
            authenticate_arguments(Pid::this(), &rows, &vector(&actual), &expected),
            "bytes/order changed",
        );
        *actual[1].last_mut().unwrap() = 0;
        let mut bad = vector(&actual);
        bad[8..16].copy_from_slice(&1u64.to_le_bytes());
        expect_error(
            authenticate_arguments(Pid::this(), &rows, &bad, &expected),
            "final argv mapping",
        );
        bad = vector(&actual);
        bad[0..8].copy_from_slice(&2u64.to_le_bytes());
        expect_error(
            authenticate_arguments(Pid::this(), &rows, &bad, &expected),
            "count changed",
        );
        bad = vector(&actual);
        bad[32..40].copy_from_slice(&1u64.to_le_bytes());
        expect_error(
            authenticate_arguments(Pid::this(), &rows, &bad, &expected),
            "missing final argv terminator",
        );
    }

    struct ElfMapping {
        address: *mut libc::c_void,
        _file: File,
    }
    impl ElfMapping {
        fn new(path: &Path) -> Self {
            let file = File::open(path).unwrap();
            let address = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    2 * PAGE,
                    libc::PROT_READ,
                    libc::MAP_PRIVATE,
                    file.as_raw_fd(),
                    0,
                )
            };
            assert_ne!(address, libc::MAP_FAILED);
            assert_eq!(
                unsafe {
                    libc::mprotect(
                        address.byte_add(PAGE),
                        PAGE,
                        libc::PROT_READ | libc::PROT_EXEC,
                    )
                },
                0
            );
            Self {
                address,
                _file: file,
            }
        }
        fn entry(&self) -> usize {
            self.address as usize + PAGE + 256
        }
        fn bias(&self) -> usize {
            self.address as usize - 0x400000
        }
        fn copy_phdr_page(&self, bytes: &[u8]) {
            assert_eq!(
                unsafe {
                    libc::mmap(
                        self.address,
                        PAGE,
                        libc::PROT_READ | libc::PROT_WRITE,
                        libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_FIXED,
                        -1,
                        0,
                    )
                },
                self.address
            );
            // Match the real rewriter's --i loop: word zero is not copied.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    bytes.as_ptr().add(8),
                    self.address.cast::<u8>().add(8),
                    PAGE - 8,
                );
            }
            assert_eq!(
                unsafe { libc::mprotect(self.address, PAGE, libc::PROT_READ) },
                0
            );
        }
    }
    impl Drop for ElfMapping {
        fn drop(&mut self) {
            assert_eq!(unsafe { libc::munmap(self.address, 2 * PAGE) }, 0);
        }
    }
    fn two_page_elf() -> Vec<u8> {
        let mut bytes = executable_fixture(None);
        bytes.resize(2 * PAGE, 0);
        bytes[24..32].copy_from_slice(&0x401100u64.to_le_bytes());
        bytes[56..58].copy_from_slice(&2u16.to_le_bytes());
        bytes[68..72].copy_from_slice(&4u32.to_le_bytes());
        let header = bytes[64..120].to_vec();
        bytes[120..176].copy_from_slice(&header);
        bytes[124..128].copy_from_slice(&5u32.to_le_bytes());
        bytes[128..136].copy_from_slice(&(PAGE as u64).to_le_bytes());
        bytes[136..144].copy_from_slice(&0x401000u64.to_le_bytes());
        let code = bytes[256..265].to_vec();
        bytes[PAGE + 256..PAGE + 265].copy_from_slice(&code);
        bytes
    }

    #[test]
    fn executable_anchor_selects_real_duplicate_instances_and_rejects_other_objects() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("held-elf");
        let other = directory.path().join("different-inode");
        let bytes = two_page_elf();
        std::fs::write(&path, &bytes).unwrap();
        std::fs::write(&other, &bytes).unwrap();
        let held = HeldObject::open(&path).unwrap();
        let first = ElfMapping::new(&path);
        let second = ElfMapping::new(&path);
        let wrong = ElfMapping::new(&other);
        let rows = maps(Pid::this()).unwrap();
        // The old unanchored intersection still refuses two distinct bases.
        expect_error(
            held.bias(Pid::this(), &rows),
            "ambiguous bootstrap ELF load bias",
        );
        for mapping in [&first, &second] {
            assert_eq!(
                held.bias_at(Pid::this(), &rows, mapping.entry(), "test entry")
                    .unwrap(),
                mapping.bias()
            );
        }
        expect_error(
            held.bias_at(Pid::this(), &rows, wrong.entry(), "test entry"),
            "identity mismatch",
        );
        expect_error(
            held.bias_at(
                Pid::this(),
                &rows,
                first.address as usize + 64,
                "test entry",
            ),
            "not executable",
        );
        expect_error(
            held.bias_at(Pid::this(), &rows, 1, "test entry"),
            "test entry anchor=0x1",
        );
        // One real mapping can still have two candidate virtual addresses.
        // Deliberately overlapping file extents must not become first-match
        // selection merely because the observation names a single VMA.
        let mut ambiguous = bytes.clone();
        ambiguous[56..58].copy_from_slice(&3u16.to_le_bytes());
        ambiguous[176..232].copy_from_slice(&bytes[120..176]);
        ambiguous[192..200].copy_from_slice(&0x402000u64.to_le_bytes());
        let ambiguous_path = directory.path().join("ambiguous-loads");
        std::fs::write(&ambiguous_path, ambiguous).unwrap();
        let ambiguous_held = HeldObject::open(&ambiguous_path).unwrap();
        let ambiguous_mapping = ElfMapping::new(&ambiguous_path);
        expect_error(
            ambiguous_held.bias_at(
                Pid::this(),
                &maps(Pid::this()).unwrap(),
                ambiguous_mapping.entry(),
                "ambiguous entry",
            ),
            "missing/ambiguous anchored ELF load bias",
        );
        // Executable VMA permissions cannot extend the ELF's actual file
        // extent: the observed anchor is beyond p_filesz in this object.
        let mut short = bytes.clone();
        short[152..160].copy_from_slice(&128u64.to_le_bytes());
        let short_path = directory.path().join("short-executable-load");
        std::fs::write(&short_path, short).unwrap();
        let short_held = HeldObject::open(&short_path).unwrap();
        let short_mapping = ElfMapping::new(&short_path);
        expect_error(
            short_held.bias_at(
                Pid::this(),
                &maps(Pid::this()).unwrap(),
                short_mapping.entry(),
                "beyond file extent",
            ),
            "missing/ambiguous anchored ELF load bias",
        );
    }

    #[test]
    fn copied_phdr_requires_anchored_page_geometry_and_immutable_header_bytes() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("held-elf");
        let bytes = two_page_elf();
        std::fs::write(&path, &bytes).unwrap();
        let held = HeldObject::open(&path).unwrap();
        let mapping = ElfMapping::new(&path);
        let other = ElfMapping::new(&path);
        let phdr = mapping.address as usize + 64;
        let rows = maps(Pid::this()).unwrap();
        let bias = held
            .bias_at(Pid::this(), &rows, mapping.entry(), "test entry")
            .unwrap();
        held.program_headers(Pid::this(), &rows, bias, phdr, 2, 56)
            .unwrap();
        mapping.copy_phdr_page(&bytes);
        let rows = maps(Pid::this()).unwrap();
        expect_error(
            held.authenticate(Pid::this(), containing(&rows, phdr, 112).unwrap()),
            "identity mismatch",
        );
        assert_eq!(
            held.bias_at(Pid::this(), &rows, mapping.entry(), "test entry")
                .unwrap(),
            bias
        );
        held.program_headers(Pid::this(), &rows, bias, phdr, 2, 56)
            .unwrap();
        expect_error(
            held.program_headers(Pid::this(), &rows, other.bias(), phdr, 2, 56),
            "address differs",
        );
        expect_error(
            held.program_headers(Pid::this(), &rows, bias, phdr + 8, 2, 56),
            "address differs",
        );
        expect_error(
            held.program_headers(Pid::this(), &rows, bias, phdr, 3, 56),
            "count/size changed",
        );
        expect_error(
            held.program_headers(Pid::this(), &rows, bias, phdr, 2, 64),
            "count/size changed",
        );
        assert_eq!(
            unsafe { libc::mprotect(mapping.address, PAGE, libc::PROT_READ | libc::PROT_WRITE) },
            0
        );
        expect_error(
            held.program_headers(Pid::this(), &maps(Pid::this()).unwrap(), bias, phdr, 2, 56),
            "unrecognized copied PHDR mapping",
        );
        unsafe {
            *mapping.address.cast::<u8>().add(72) ^= 1;
        }
        assert_eq!(
            unsafe { libc::mprotect(mapping.address, PAGE, libc::PROT_READ) },
            0
        );
        expect_error(
            held.program_headers(Pid::this(), &maps(Pid::this()).unwrap(), bias, phdr, 2, 56),
            "headers differ",
        );
    }

    #[test]
    fn interpreter_parsers_preserve_raw_unix_paths() {
        for (script, expected) in [
            (
                b"#!/interpreter --op\0ignored later\n".as_slice(),
                vec![b"/interpreter".as_slice(), b"--op".as_slice()],
            ),
            (
                b"#!/interpreter \0ignored later\n".as_slice(),
                vec![b"/interpreter".as_slice()],
            ),
        ] {
            assert_eq!(script_words(script).unwrap(), expected);
            let arguments = guest_arguments(
                vec![
                    b"loader\0".to_vec(),
                    b"plugin\0".to_vec(),
                    b"--\0".to_vec(),
                    b"script\0".to_vec(),
                ],
                Some(script),
            )
            .unwrap();
            let mut expected_arguments: Vec<_> = expected
                .iter()
                .map(|word| {
                    let mut value = word.to_vec();
                    value.push(0);
                    value
                })
                .collect();
            expected_arguments.push(b"script\0".to_vec());
            assert_eq!(arguments, expected_arguments);
        }
        expect_error(
            script_words(b"#!/interpreter\0bad --option\n"),
            "NUL in SaBRe script interpreter",
        );
        let script = b"#! /interpreter-\xff\\012 --option\nbody\n";
        assert_eq!(
            script_interpreter(script)
                .unwrap()
                .unwrap()
                .as_os_str()
                .as_bytes(),
            b"/interpreter-\xff\\012"
        );
        let path = b"/interpreter-\xff\n\\012";
        let bytes = executable_fixture(Some(path));
        object::File::parse(bytes.as_slice()).unwrap();
        assert_eq!(
            elf_interpreter(&bytes)
                .unwrap()
                .unwrap()
                .as_os_str()
                .as_bytes(),
            path
        );
        assert!(
            elf_interpreter(&executable_fixture(None))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn interpreter_parsers_preserve_terminator_and_header_refusals() {
        expect_error(
            script_interpreter(b"#! \n"),
            "missing SaBRe script interpreter",
        );
        expect_error(
            script_interpreter(b"#!/bad\0path\n"),
            "NUL in SaBRe script interpreter",
        );
        let mut bytes = executable_fixture(Some(b"/interpreter"));
        bytes[524] = b'x';
        expect_error(elf_interpreter(&bytes), "invalid interpreter terminator");
        let mut bytes = executable_fixture(Some(b"/bad\0path"));
        expect_error(elf_interpreter(&bytes), "invalid interpreter terminator");
        bytes[56..58].copy_from_slice(&129u16.to_le_bytes());
        expect_error(elf_interpreter(&bytes), "unsupported ELF program headers");
        let mut bytes = executable_fixture(None);
        bytes[32..40].copy_from_slice(&u64::MAX.to_le_bytes());
        expect_error(elf_interpreter(&bytes), "program headers outside held ELF");
    }

    #[test]
    fn large_sparse_elf_snapshot_preserves_bytes_and_closes_aliases() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("large-elf");
        let file = std::fs::OpenOptions::new()
            .write(true)
            .read(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        file.write_all_at(&executable_fixture(None), 0).unwrap();
        let length = (IN_MEMORY_SNAPSHOT_LIMIT + PAGE) as u64;
        file.set_len(length).unwrap();
        file.write_all_at(b"X", length - 1).unwrap();
        let held = HeldObject::open(&path).unwrap();
        let OriginalBytes::Disk(snapshot) = &held.bytes else {
            panic!("large object must use disk backing")
        };
        let rows = maps(Pid::this()).unwrap();
        let row = containing(&rows, snapshot.address, snapshot.length)
            .unwrap()
            .clone();
        assert_eq!(row.permissions, "r--p");
        let raw = row.mapped_path(Pid::this()).unwrap();
        assert!(
            !raw.parent().unwrap().exists(),
            "private snapshot directory survived construction"
        );
        let cache_device = super::super::HermitData::new()
            .data_dir()
            .metadata()
            .unwrap()
            .dev();
        for entry in std::fs::read_dir("/proc/self/fd").unwrap() {
            if let Ok(metadata) = entry.unwrap().path().metadata() {
                assert_ne!(
                    (metadata.dev(), metadata.ino()),
                    (cache_device, row.inode),
                    "snapshot FD alias survived construction"
                );
            }
        }
        assert_eq!(held.bytes.len() as u64, length);
        assert_eq!(held.bytes.last(), Some(&b'X'));
        held.authenticate_path(Pid::this()).unwrap();
        file.write_all_at(b"Y", length - 1).unwrap();
        expect_error(
            held.authenticate_path(Pid::this()),
            "bootstrap mapped pathname content changed",
        );
        assert_eq!(held.bytes.last(), Some(&b'X'));
        drop(held);
        assert!(
            !maps(Pid::this())
                .unwrap()
                .iter()
                .any(|current| current.start == row.start
                    && current.end == row.end
                    && current.inode == row.inode
                    && current.device == row.device),
            "snapshot mapping survived Drop"
        );
    }

    #[test]
    fn small_snapshot_preserves_memory_storage_and_content_authentication() {
        let directory = tempfile::tempdir().unwrap();
        let path = fixture_file(directory.path(), b"small");
        let held = HeldObject::open_file(&path).unwrap();
        assert!(matches!(held.bytes, OriginalBytes::Memory(_)));
        held.authenticate_path(Pid::this()).unwrap();
    }

    #[test]
    fn disk_snapshot_refuses_a_real_memory_backed_file() {
        let fd =
            unsafe { libc::memfd_create(c"bootstrap-backing-control".as_ptr(), libc::MFD_CLOEXEC) };
        assert!(fd >= 0);
        let file = unsafe { File::from_raw_fd(fd) };
        expect_error(
            require_supported_snapshot_filesystem(&file),
            "large bootstrap snapshot requires a supported cache filesystem",
        );
    }

    fn vdso_row() -> Map {
        Map {
            start: 0x1000,
            end: 0x2000,
            offset: 0,
            device: "00:00".into(),
            inode: 0,
            path: b"[vdso]".to_vec(),
            permissions: "r-xp".into(),
        }
    }

    #[test]
    fn vdso_absence_requires_both_maps_and_kernel_auxv_absence() {
        assert!(
            initial_vdso(&[], &words(&[libc::AT_NULL, 0]))
                .unwrap()
                .is_none()
        );
        assert!(
            initial_vdso(&[], &words(&[libc::AT_SYSINFO_EHDR, 0, libc::AT_NULL, 0]))
                .unwrap()
                .is_none()
        );
        expect_error(
            initial_vdso(
                &[],
                &words(&[libc::AT_SYSINFO_EHDR, 0x1000, libc::AT_NULL, 0]),
            ),
            "initial vDSO auxv/mapping mismatch",
        );
        expect_error(
            initial_vdso(&[vdso_row()], &words(&[libc::AT_NULL, 0])),
            "initial vDSO auxv/mapping mismatch",
        );
    }

    #[test]
    fn vdso_selection_preserves_identity_and_ambiguity_refusals() {
        let rows = [vdso_row()];
        let auxv = words(&[libc::AT_SYSINFO_EHDR, 0x1000, libc::AT_NULL, 0]);
        assert_eq!(initial_vdso(&rows, &auxv).unwrap(), Some(&rows[0]));
        expect_error(
            initial_vdso(&[vdso_row(), vdso_row()], &auxv),
            "ambiguous initial kernel vDSO",
        );
        expect_error(
            initial_vdso(
                &rows,
                &words(&[libc::AT_SYSINFO_EHDR, 0x2000, libc::AT_NULL, 0]),
            ),
            "initial vDSO auxv/mapping mismatch",
        );
        let mut writable = vdso_row();
        writable.permissions = "rwxp".into();
        expect_error(
            initial_vdso(&[writable], &auxv),
            "unsupported initial vDSO mapping",
        );
        expect_error(
            initial_vdso(
                &rows,
                &words(&[
                    libc::AT_SYSINFO_EHDR,
                    0x1000,
                    libc::AT_SYSINFO_EHDR,
                    0x1000,
                    libc::AT_NULL,
                    0,
                ]),
            ),
            "ambiguous initial vDSO auxv",
        );
        expect_error(
            initial_vdso(&rows, &auxv[..24]),
            "short initial kernel auxv",
        );
    }

    #[test]
    fn vdso_selection_matches_actual_proc_inputs() {
        let rows = maps(Pid::this()).unwrap();
        let auxv = read_path("/proc/self/auxv", 4096).unwrap();
        let selected = initial_vdso(&rows, &auxv).unwrap();
        assert_eq!(
            selected.is_some(),
            rows.iter().any(|row| row.path == b"[vdso]")
        );
    }
}
