/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Bounded real root/image observations joined to the existing physical watch.
//! Filesystem and mapping receipts are evidence, never NativeExecProof on their
//! own. The coordinator owns exact original exec/lifecycle and final admission.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::ffi::CString;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::Read;
use std::io::{self};
use std::os::fd::AsFd;
use std::os::fd::AsRawFd;
use std::os::fd::BorrowedFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::os::unix::fs::FileExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::OpenOptionsExt;
use std::sync::Arc;

use detcore::detlog::NativePhysicalWatchIdentity;
use goblin::elf::Elf;
use goblin::elf::header;
use goblin::elf::program_header;
use hermit::in_guest_exits::NativeWatchedProcess;
use hermit::physical_exit_watch;
use reverie_inguest::launcher::native::NativeRootObservation;
use sha2::Digest;
use sha2::Sha256;

use super::container::declared_inputs::FilePlacement;
use super::container::declared_inputs::PreparedDeclaredRoot;
use super::container::declared_inputs::ReadonlyRootBacking;
use super::container::declared_inputs::RuntimeFile;
use super::container::declared_inputs::STOCK_LOADER_PATH;
use super::native_exec_artifacts::QualifiedNativeArtifacts;
use super::native_exec_artifacts::RUNNER_PATH;
use super::native_exec_artifacts::RUNTIME_PATH;
use super::native_exec_inputs::CapturedIdentity;
use super::native_exec_inputs::GuestPath;
use super::native_exec_inputs::HeldDynamicInputs;
use super::native_exec_inputs::InputRole;

const PAGE: u64 = 4096;
const MAPS_CAP: u64 = 128 * 1024;
const STATUS_CAP: u64 = 16 * 1024;
const AUXV_CAP: u64 = 4096;
const MOUNTINFO_CAP: u64 = 4 * 1024 * 1024;
const ALL_SEALS: i32 = libc::F_SEAL_SEAL
    | libc::F_SEAL_SHRINK
    | libc::F_SEAL_GROW
    | libc::F_SEAL_WRITE
    | libc::F_SEAL_FUTURE_WRITE
    | 0x0020;
const ENDPOINTS: [&[u8]; 3] = [b"/run/rpc.sock", b"/run/provision.sock", b"/run/stats.sock"];
const DEVICES: [&[u8]; 5] = [
    b"/dev/null",
    b"/dev/zero",
    b"/dev/full",
    b"/dev/random",
    b"/dev/urandom",
];
fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}
fn number(s: &str, radix: u32) -> io::Result<u64> {
    u64::from_str_radix(s, radix).map_err(|_| invalid("invalid exact proc integer"))
}
fn integer(s: &[u8], radix: u32) -> io::Result<u64> {
    number(
        std::str::from_utf8(s).map_err(|_| invalid("non-ASCII proc integer"))?,
        radix,
    )
}
#[derive(Clone, Debug, Eq, PartialEq)]
struct Row {
    lo: u64,
    hi: u64,
    offset: u64,
    major: u64,
    minor: u64,
    inode: u64,
    permissions: [u8; 4],
    name: Vec<u8>,
}
fn parse_maps(bytes: &[u8]) -> io::Result<Vec<Row>> {
    if bytes.is_empty() || bytes.len() > MAPS_CAP as usize || bytes.last() != Some(&b'\n') {
        return Err(invalid("bounded complete maps required"));
    }
    let mut rows = Vec::new();
    let mut end = 0;
    for line in bytes[..bytes.len() - 1].split(|b| *b == b'\n') {
        let mut words = line
            .split(|b| b.is_ascii_whitespace())
            .filter(|s| !s.is_empty());
        let mut fields = Vec::new();
        for _ in 0..5 {
            fields.push(words.next().ok_or_else(|| invalid("short maps row"))?);
        }
        let range = fields[0].split(|b| *b == b'-').collect::<Vec<_>>();
        let device = fields[3].split(|b| *b == b':').collect::<Vec<_>>();
        if range.len() != 2 || device.len() != 2 {
            return Err(invalid("invalid map extent/device"));
        }
        let row = Row {
            lo: integer(range[0], 16)?,
            hi: integer(range[1], 16)?,
            offset: integer(fields[2], 16)?,
            major: integer(device[0], 16)?,
            minor: integer(device[1], 16)?,
            inode: integer(fields[4], 10)?,
            permissions: fields[1]
                .try_into()
                .map_err(|_| invalid("invalid exact map permissions"))?,
            // Pathnames are bytes. Non-UTF8 declared names must not become an
            // implicit exemption or reject the otherwise exact numeric fields.
            name: words.collect::<Vec<_>>().join(&b' '),
        };
        if rows.len() >= 1024
            || row.lo < end
            || row.lo >= row.hi
            || !row.lo.is_multiple_of(PAGE)
            || !row.hi.is_multiple_of(PAGE)
            || !row.offset.is_multiple_of(PAGE)
            || !matches!(row.permissions[0], b'r' | b'-')
            || !matches!(row.permissions[1], b'w' | b'-')
            || !matches!(row.permissions[2], b'x' | b'-')
            || !matches!(row.permissions[3], b'p' | b's')
        {
            return Err(invalid("invalid/overlapping/over-bound map row"));
        }
        end = row.hi;
        rows.push(row);
    }
    Ok(rows)
}
fn read_proc(pid: i32, name: &str, cap: u64) -> io::Result<Vec<u8>> {
    if pid <= 0 || !["maps", "status", "auxv", "mountinfo"].contains(&name) {
        return Err(invalid("unbounded/unknown proc observation"));
    }
    let mut bytes = Vec::new();
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(format!("/proc/{pid}/{name}"))?
        .take(cap + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > cap {
        return Err(invalid("proc observation bound exceeded"));
    }
    Ok(bytes)
}
fn live(fd: &OwnedFd) -> io::Result<()> {
    let mut p = libc::pollfd {
        fd: fd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let rc = unsafe { libc::poll(&mut p, 1, 0) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    if rc != 0 || p.revents != 0 {
        return Err(invalid("retained watched birth is not alive"));
    }
    Ok(())
}
fn mount_id(fd: BorrowedFd<'_>) -> io::Result<u64> {
    let mut stat = std::mem::MaybeUninit::<libc::statx>::zeroed();
    if unsafe {
        libc::statx(
            fd.as_raw_fd(),
            c"".as_ptr(),
            libc::AT_EMPTY_PATH | libc::AT_SYMLINK_NOFOLLOW,
            libc::STATX_MNT_ID,
            stat.as_mut_ptr(),
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    let stat = unsafe { stat.assume_init() };
    if stat.stx_mask & libc::STATX_MNT_ID == 0 || stat.stx_mnt_id == 0 {
        return Err(invalid("actual mount identity absent"));
    }
    Ok(stat.stx_mnt_id)
}
fn view(fd: BorrowedFd<'_>) -> io::Result<u64> {
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    if unsafe { libc::fstatvfs(fd.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { stat.assume_init() }.f_flag)
}
fn root_open(root: &File, path: &[u8], no_follow: bool) -> io::Result<File> {
    let relative = path
        .strip_prefix(b"/")
        .ok_or_else(|| invalid("nonabsolute root observation"))?;
    let path = CString::new(if relative.is_empty() {
        b".".as_slice()
    } else {
        relative
    })
    .map_err(|_| invalid("NUL root observation"))?;
    #[repr(C)]
    struct How {
        flags: u64,
        mode: u64,
        resolve: u64,
    }
    let how = How {
        flags: (libc::O_PATH | libc::O_CLOEXEC | if no_follow { libc::O_NOFOLLOW } else { 0 })
            as u64,
        mode: 0,
        resolve: 0x10 | 0x02,
    }; // RESOLVE_IN_ROOT | RESOLVE_NO_MAGICLINKS
    let fd = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            root.as_raw_fd(),
            path.as_ptr(),
            &how,
            std::mem::size_of::<How>(),
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd as i32) })
}
fn read_link(root: &File, path: &[u8]) -> io::Result<Vec<u8>> {
    let link = root_open(root, path, true)?;
    let mut bytes = vec![0; 4096];
    let n = unsafe {
        libc::readlinkat(
            link.as_raw_fd(),
            c"".as_ptr(),
            bytes.as_mut_ptr().cast(),
            bytes.len(),
        )
    };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    if n as usize == bytes.len() {
        return Err(invalid("root alias link exceeds exact bound"));
    }
    bytes.truncate(n as usize);
    Ok(bytes)
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
struct Key {
    device: u64,
    inode: u64,
}
fn key(file: &File) -> io::Result<Key> {
    let m = file.metadata()?;
    Ok(Key {
        device: m.dev(),
        inode: m.ino(),
    })
}
fn exact_root(root: &File, device: u64, inode: u64, mount: u64, readonly: bool) -> io::Result<()> {
    let m = root.metadata()?;
    let mut fs = std::mem::MaybeUninit::<libc::statfs>::uninit();
    if unsafe { libc::fstatfs(root.as_raw_fd(), fs.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    if !m.is_dir()
        || (m.dev(), m.ino()) != (device, inode)
        || mount_id(root.as_fd())? != mount
        || unsafe { fs.assume_init() }.f_type != libc::TMPFS_MAGIC
        || readonly && view(root.as_fd())? & libc::ST_RDONLY == 0
    {
        return Err(invalid("actual common root identity/policy differs"));
    }
    Ok(())
}
fn status(bytes: &[u8], pid: i32, filters: u64) -> io::Result<u64> {
    let text = std::str::from_utf8(bytes).map_err(|_| invalid("status UTF8"))?;
    let mut values = BTreeMap::new();
    for line in text.lines() {
        if let Some((name, value)) = line.split_once(':')
            && values.insert(name, value.trim()).is_some()
        {
            return Err(invalid("duplicate status field"));
        }
    }
    for (field, wanted) in [
        ("Pid", pid as u64),
        ("Tgid", pid as u64),
        ("Threads", 1),
        ("TracerPid", 0),
        ("NoNewPrivs", 1),
        ("Seccomp", if filters == 0 { 0 } else { 2 }),
        ("Seccomp_filters", filters),
    ] {
        if number(
            values
                .get(field)
                .ok_or_else(|| invalid("missing native status field"))?,
            10,
        )? != wanted
        {
            return Err(invalid(format!("native status {field} differs")));
        }
    }
    for field in ["CapInh", "CapPrm", "CapEff", "CapBnd", "CapAmb"] {
        if number(
            values
                .get(field)
                .ok_or_else(|| invalid("missing capability set"))?,
            16,
        )? != 0
        {
            return Err(invalid("native launcher retains capabilities"));
        }
    }
    number(
        values
            .get("SigBlk")
            .ok_or_else(|| invalid("missing actual blocked mask"))?,
        16,
    )
}
fn auxv(bytes: &[u8]) -> io::Result<BTreeMap<u64, u64>> {
    if bytes.is_empty() || bytes.len() > AUXV_CAP as usize || !bytes.len().is_multiple_of(16) {
        return Err(invalid("actual auxv exact bounded shape"));
    }
    let mut result = BTreeMap::new();
    let mut ended = false;
    for pair in bytes.as_chunks::<16>().0 {
        let key = u64::from_ne_bytes(pair[..8].try_into().unwrap());
        let value = u64::from_ne_bytes(pair[8..].try_into().unwrap());
        if ended || result.insert(key, value).is_some() {
            return Err(invalid("duplicate/trailing auxv"));
        }
        if key == 0 {
            if value != 0 {
                return Err(invalid("nonzero auxv terminator"));
            }
            ended = true;
        }
    }
    if !ended
        || result.get(&6) != Some(&4096)
        || !result.contains_key(&3)
        || !result.contains_key(&7)
        || !result.contains_key(&9)
    {
        return Err(invalid("missing required actual auxv fields"));
    }
    Ok(result)
}

#[derive(Clone, Debug)]
struct Segment {
    offset: u64,
    address: u64,
    file_size: u64,
    memory_size: u64,
    flags: u32,
}
struct Object {
    file: File,
    backing: Option<ReadonlyRootBacking>,
    identity: CapturedIdentity,
    segments: Vec<Segment>,
    entry: u64,
}
impl Object {
    fn retain(fd: BorrowedFd<'_>, expected: CapturedIdentity) -> io::Result<Self> {
        let file = File::from(fd.try_clone_to_owned()?);
        Self::retain_file(file, expected, None)
    }
    fn retain_root(backing: &ReadonlyRootBacking) -> io::Result<Self> {
        backing.verify().map_err(|e| invalid(e.to_string()))?;
        if !backing.executable() {
            return Err(invalid("mapping role is not an executable root backing"));
        }
        Self::retain_file(
            File::from(backing.descriptor().try_clone_to_owned()?),
            backing.identity(),
            Some(backing.clone()),
        )
    }
    fn retain_file(
        file: File,
        expected: CapturedIdentity,
        backing: Option<ReadonlyRootBacking>,
    ) -> io::Result<Self> {
        let m = file.metadata()?;
        if !m.is_file()
            || m.len() == 0
            || m.len() > 64 * 1024 * 1024
            || (m.dev(), m.ino(), m.len()) != (expected.device, expected.inode, expected.size)
            || (backing.is_none()
                && unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GET_SEALS) } != ALL_SEALS)
        {
            return Err(invalid(
                "mapping role lacks its exact immutable held identity",
            ));
        }
        let mut bytes =
            vec![0; usize::try_from(expected.size).map_err(|_| invalid("ELF size overflow"))?];
        file.read_exact_at(&mut bytes, 0)?;
        if <[u8; 32]>::from(Sha256::digest(&bytes)) != expected.digest {
            return Err(invalid("held ELF bytes differ"));
        }
        let elf = Elf::parse(&bytes).map_err(|e| invalid(format!("held mapping ELF: {e}")))?;
        if !elf.is_64
            || !elf.little_endian
            || elf.header.e_machine != header::EM_X86_64
            || ![header::ET_DYN, header::ET_EXEC].contains(&elf.header.e_type)
        {
            return Err(invalid("mapping role is not supported x86-64 ELF"));
        }
        let segments = elf
            .program_headers
            .iter()
            .filter(|p| p.p_type == program_header::PT_LOAD)
            .map(|p| Segment {
                offset: p.p_offset,
                address: p.p_vaddr,
                file_size: p.p_filesz,
                memory_size: p.p_memsz,
                flags: p.p_flags,
            })
            .collect::<Vec<_>>();
        if segments.is_empty()
            || segments.iter().any(|s| {
                s.file_size > s.memory_size
                    || s.offset
                        .checked_add(s.file_size)
                        .is_none_or(|n| n > expected.size)
                    || s.address.checked_add(s.memory_size).is_none()
                    || s.offset % PAGE != s.address % PAGE
                    || s.flags & !7 != 0
                    || s.flags & 3 == 3
            })
        {
            return Err(invalid("invalid held PT_LOAD bounds/permissions"));
        }
        let entry = elf.entry;
        Ok(Self {
            file,
            backing,
            identity: expected,
            segments,
            entry,
        })
    }
    fn key(&self) -> Key {
        Key {
            device: self.identity.device,
            inode: self.identity.inode,
        }
    }
    fn verify(&self) -> io::Result<()> {
        if let Some(backing) = &self.backing {
            backing.verify().map_err(|e| invalid(e.to_string()))?;
        }
        let m = self.file.metadata()?;
        if (m.dev(), m.ino(), m.len())
            != (
                self.identity.device,
                self.identity.inode,
                self.identity.size,
            )
            || (self.backing.is_none()
                && unsafe { libc::fcntl(self.file.as_raw_fd(), libc::F_GET_SEALS) } != ALL_SEALS)
        {
            return Err(invalid("retained mapping object changed"));
        }
        Ok(())
    }
}
fn file_rows<'a>(rows: &'a [Row], object: &Object) -> Vec<&'a Row> {
    rows.iter()
        .filter(|r| {
            r.major == u64::from(libc::major(object.identity.device))
                && r.minor == u64::from(libc::minor(object.identity.device))
                && r.inode == object.identity.inode
        })
        .collect()
}
fn permission(row: &Row, segment: &Segment) -> bool {
    (row.permissions[0] == b'r') == (segment.flags & 4 != 0)
        && (row.permissions[2] == b'x') == (segment.flags & 1 != 0)
        && (row.permissions[1] != b'w' || segment.flags & 2 != 0)
        && !(row.permissions[1] == b'w' && row.permissions[2] == b'x')
        && row.permissions[3] == b'p'
}
fn segment_extent(base: u64, s: &Segment) -> io::Result<(u64, u64, u64)> {
    let lo = base
        .checked_add(s.address & !(PAGE - 1))
        .ok_or_else(|| invalid("PT_LOAD base overflow"))?;
    let hi = base
        .checked_add(s.address)
        .and_then(|n| n.checked_add(s.file_size))
        .and_then(|n| n.checked_add(PAGE - 1))
        .map(|n| n & !(PAGE - 1))
        .ok_or_else(|| invalid("PT_LOAD extent overflow"))?;
    Ok((lo, hi, s.offset & !(PAGE - 1)))
}
fn mapped_object(rows: &[Row], object: &Object, count: usize) -> io::Result<Vec<u64>> {
    let actual = file_rows(rows, object);
    let first = object
        .segments
        .iter()
        .find(|s| s.file_size > 0)
        .ok_or_else(|| invalid("ELF needs a file-backed PT_LOAD"))?;
    let bases = actual
        .iter()
        .filter(|r| r.offset == (first.offset & !(PAGE - 1)))
        .filter_map(|r| r.lo.checked_sub(first.address & !(PAGE - 1)))
        .collect::<BTreeSet<_>>();
    if bases.len() != count {
        return Err(invalid("held ELF mapping multiplicity differs"));
    }
    for base in &bases {
        for s in &object.segments {
            if s.file_size == 0 {
                continue;
            }
            let (lo, hi, offset) = segment_extent(*base, s)?;
            let mut cursor = lo;
            while cursor < hi {
                let r = actual
                    .iter()
                    .find(|r| {
                        r.lo <= cursor
                            && cursor < r.hi
                            && r.offset.checked_add(cursor - r.lo)
                                == offset.checked_add(cursor - lo)
                    })
                    .ok_or_else(|| {
                        invalid("held sealed PT_LOAD bytes lack exact actual coverage")
                    })?;
                if !permission(r, s) {
                    return Err(invalid("held PT_LOAD actual permissions differ"));
                }
                cursor = r.hi.min(hi);
            }
        }
    }
    // Refuse extra aliases/pages as well as missing expected pages. Every file
    // mapping of an admitted role must fit some exact load at a derived base.
    for r in actual {
        let mut cursor = r.lo;
        while cursor < r.hi {
            let mut next = None;
            for base in &bases {
                for s in &object.segments {
                    if s.file_size == 0 {
                        continue;
                    }
                    let (lo, hi, offset) = segment_extent(*base, s)?;
                    if lo <= cursor
                        && cursor < hi
                        && permission(r, s)
                        && r.offset.checked_add(cursor - r.lo) == offset.checked_add(cursor - lo)
                    {
                        next = Some(hi.min(r.hi));
                    }
                }
            }
            cursor =
                next.ok_or_else(|| invalid("extra held-file mapping outside PT_LOAD coverage"))?;
        }
    }
    Ok(bases.into_iter().collect())
}
fn selected_target<'a>(rows: &[Row], candidates: &'a [Object]) -> io::Result<&'a Object> {
    let mut selected = None;
    for object in candidates {
        if file_rows(rows, object).is_empty() {
            continue;
        }
        if selected.is_some() {
            return Err(invalid(
                "multiple actual objects match selected target bytes",
            ));
        }
        selected = Some(object);
    }
    selected.ok_or_else(|| invalid("selected target bytes have no actual mapped held object"))
}
#[derive(Clone, Debug, Eq, PartialEq)]
struct MountRow {
    id: u64,
    parent: u64,
    path: Vec<u8>,
    options: BTreeSet<Vec<u8>>,
    filesystem: Vec<u8>,
}
fn unescape(bytes: &[u8]) -> io::Result<Vec<u8>> {
    let mut result = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' {
            let tail = bytes
                .get(i + 1..i + 4)
                .ok_or_else(|| invalid("partial mountinfo escape"))?;
            let value = match tail {
                b"040" => b' ',
                b"011" => b'\t',
                b"012" => b'\n',
                b"134" => b'\\',
                _ => return Err(invalid("unknown mountinfo escape")),
            };
            result.push(value);
            i += 4;
        } else {
            result.push(bytes[i]);
            i += 1;
        }
    }
    Ok(result)
}
fn mounts(bytes: &[u8]) -> io::Result<Vec<MountRow>> {
    if bytes.is_empty() || bytes.len() > MOUNTINFO_CAP as usize || bytes.last() != Some(&b'\n') {
        return Err(invalid("bounded complete mountinfo required"));
    }
    let mut rows = Vec::new();
    let mut ids = BTreeSet::new();
    for line in bytes[..bytes.len() - 1].split(|b| *b == b'\n') {
        let f = line.split(|b| *b == b' ').collect::<Vec<_>>();
        let split = f
            .iter()
            .position(|v| *v == b"-")
            .ok_or_else(|| invalid("missing mountinfo separator"))?;
        if f.len() < 10 || split < 6 || split + 4 != f.len() || rows.len() >= 2048 {
            return Err(invalid("invalid bounded mountinfo row"));
        }
        let id = integer(f[0], 10)?;
        if id == 0 || !ids.insert(id) {
            return Err(invalid("duplicate mount ID"));
        }
        let path = unescape(f[4])?;
        if !path.starts_with(b"/") {
            return Err(invalid("relative mount point"));
        }
        rows.push(MountRow {
            id,
            parent: integer(f[1], 10)?,
            path,
            options: f[5].split(|b| *b == b',').map(Vec::from).collect(),
            filesystem: f[split + 1].to_vec(),
        });
    }
    Ok(rows)
}
fn descends(rows: &[MountRow], mut id: u64, root: u64) -> bool {
    for _ in 0..=rows.len() {
        if id == root {
            return true;
        }
        let Some(r) = rows.iter().find(|r| r.id == id) else {
            return false;
        };
        if r.parent == id {
            return false;
        }
        id = r.parent;
    }
    false
}
fn option(row: &MountRow, name: &[u8]) -> bool {
    row.options.contains(name)
}
fn proc_paths(pid: i32) -> BTreeSet<Vec<u8>> {
    let mut result = BTreeSet::new();
    for leaf in ["maps", "smaps", "status", "auxv", "comm", "fd"] {
        for p in [
            format!("/proc/{pid}/{leaf}"),
            format!("/proc/{pid}/task/{pid}/{leaf}"),
        ] {
            result.insert(p.into_bytes());
        }
    }
    result
}
fn check_mountinfo(
    bytes: &[u8],
    proc: &BTreeSet<Vec<u8>>,
    expected: &BTreeMap<u64, MountRow>,
) -> io::Result<()> {
    let rows = mounts(bytes)?;
    let mut paths = BTreeSet::new();
    let mut seen = BTreeSet::new();
    for r in &rows {
        if !paths.insert(r.path.clone()) {
            return Err(invalid("stacked mount in guest root"));
        }
        if let Some(old) = expected.get(&r.id) {
            if r.path != old.path || r.filesystem != old.filesystem || !seen.insert(r.id) {
                return Err(invalid("retained mount path/object differs"));
            }
            if r.path == b"/tmp" {
                if !option(r, b"rw")
                    || option(r, b"ro")
                    || r.filesystem != b"tmpfs"
                    || ![b"nosuid".as_slice(), b"nodev", b"noexec"]
                        .into_iter()
                        .all(|f| option(r, f))
                {
                    return Err(invalid("common tmp is not exact writable noexec tmpfs"));
                }
            } else if !option(r, b"ro") || option(r, b"rw") || !option(r, b"nosuid") {
                return Err(invalid("protected common-root mount became writable"));
            }
            for flag in [b"nosuid".as_slice(), b"nodev", b"noexec"] {
                if option(old, flag) && !option(r, flag) {
                    return Err(invalid("retained mount protection removed"));
                }
            }
        } else if proc.contains(&r.path) {
            if r.filesystem != b"proc"
                || !option(r, b"ro")
                || option(r, b"rw")
                || ![b"nosuid".as_slice(), b"nodev", b"noexec"]
                    .into_iter()
                    .all(|f| option(r, f))
            {
                return Err(invalid("task proc view is not exact protected live leaf"));
            }
        } else {
            return Err(invalid("unknown reachable mount in declared guest root"));
        }
    }
    if seen.len() != expected.len() || !proc.iter().all(|p| paths.contains(p)) {
        return Err(invalid("required held/proc mount missing"));
    }
    Ok(())
}
struct PathObject {
    path: Vec<u8>,
    key: Key,
    mount: u64,
    executable: bool,
    kind: u32,
    rdev: u64,
}
fn observe_path(
    root: &File,
    path: &[u8],
    expected: Option<CapturedIdentity>,
    executable: bool,
) -> io::Result<PathObject> {
    let file = root_open(root, path, false)?;
    let m = file.metadata()?;
    let flags = view(file.as_fd())?;
    if let Some(e) = expected
        && (m.dev(), m.ino(), m.len()) != (e.device, e.inode, e.size)
    {
        return Err(invalid("root object differs from independently held input"));
    }
    if flags & libc::ST_RDONLY == 0 || (flags & libc::ST_NOEXEC != 0) == executable {
        return Err(invalid("root file exact mount role differs"));
    }
    Ok(PathObject {
        path: path.to_vec(),
        key: Key {
            device: m.dev(),
            inode: m.ino(),
        },
        mount: mount_id(file.as_fd())?,
        executable,
        kind: m.mode() & libc::S_IFMT,
        rdev: m.rdev(),
    })
}
fn check_path(root: &File, object: &PathObject) -> io::Result<()> {
    let actual = observe_path(root, &object.path, None, object.executable)?;
    if (actual.key, actual.mount, actual.kind, actual.rdev)
        != (object.key, object.mount, object.kind, object.rdev)
    {
        return Err(invalid("actual root path/mount identity changed"));
    }
    Ok(())
}
fn kernel_vsyscall(row: &Row) -> bool {
    row.lo == 0xffff_ffff_ff60_0000
        && row.hi == 0xffff_ffff_ff60_1000
        && row.offset == 0
        && row.major == 0
        && row.minor == 0
        && row.inode == 0
        && row.permissions == *b"--xp"
        && row.name == b"[vsyscall]"
}
fn executable_rows(
    rows: &[Row],
    known: &BTreeSet<Key>,
    aux: &BTreeMap<u64, u64>,
    vsyscall: &Option<Row>,
) -> io::Result<()> {
    let vdso = *aux
        .get(&33)
        .ok_or_else(|| invalid("actual auxv lacks kernel vDSO"))?;
    let mut saw_vdso = false;
    let mut saw_vsyscall = false;
    for r in rows.iter().filter(|r| r.permissions[2] == b'x') {
        if r.permissions[1] == b'w' {
            return Err(invalid("writable executable mapping"));
        }
        if r.inode != 0 {
            if !known.iter().any(|k| {
                r.inode == k.inode
                    && r.major == u64::from(libc::major(k.device))
                    && r.minor == u64::from(libc::minor(k.device))
            }) {
                return Err(invalid("unknown file-backed executable mapping"));
            }
        } else if r.lo == vdso
            && r.offset == 0
            && r.major == 0
            && r.minor == 0
            && r.permissions == *b"r-xp"
            && r.name == b"[vdso]"
            && !saw_vdso
        {
            saw_vdso = true;
        } else if kernel_vsyscall(r) && vsyscall.as_ref() == Some(r) && !saw_vsyscall {
            saw_vsyscall = true;
        } else {
            return Err(invalid(
                "unknown executable user VMA/kernel pseudo-map differs",
            ));
        }
    }
    if !saw_vdso || saw_vsyscall != vsyscall.is_some() {
        return Err(invalid("kernel vDSO/vsyscall witness missing"));
    }
    Ok(())
}

/// Captured before child startup from the actual common-root owner, not paths
/// or role names supplied by the guest. Parent retention lasts through reap.
pub(super) struct RootIdentity {
    root: File,
    device: u64,
    inode: u64,
    mount: u64,
    namespace: (u64, u64),
    inputs: Arc<HeldDynamicInputs>,
    artifacts: Arc<QualifiedNativeArtifacts>,
    backings: Vec<ReadonlyRootBacking>,
    paths: Vec<PathObject>,
    aliases: Vec<(Vec<u8>, Vec<u8>)>,
    mounts: BTreeMap<u64, MountRow>,
    targets: BTreeMap<[u8; 32], Vec<Object>>,
    libraries: Vec<(GuestPath, Object)>,
    bootstrap: Object,
    runner: Object,
    runtime: Object,
    stock_loader: Object,
    vsyscall: Option<Row>,
}
impl RootIdentity {
    pub(super) fn capture(
        prepared: &PreparedDeclaredRoot,
        runtime_files: &[RuntimeFile],
        artifacts: Arc<QualifiedNativeArtifacts>,
    ) -> io::Result<Arc<Self>> {
        if !Arc::ptr_eq(prepared.inputs(), artifacts.inputs()) {
            return Err(invalid(
                "root and artifacts have different actual input owners",
            ));
        }
        let root = File::from(prepared.descriptor().try_clone_to_owned()?);
        exact_root(
            &root,
            prepared.device(),
            prepared.inode(),
            prepared.mount_id(),
            false,
        )?;
        let ns = File::open("/proc/self/ns/mnt")?.metadata()?;
        if (ns.dev(), ns.ino()) != prepared.namespace() {
            return Err(invalid("capture outside actual common namespace"));
        }
        let inputs = Arc::clone(prepared.inputs());
        inputs.recheck().map_err(|e| invalid(e.to_string()))?;
        // Authenticate original qualified sources before selecting any copy.
        let bootstrap_source =
            Object::retain(artifacts.bootstrap(), artifacts.bootstrap_identity())?;
        let runner = Object::retain(artifacts.runner(), artifacts.runner_identity())?;
        let runtime_source = Object::retain(artifacts.runtime(), artifacts.runtime_identity())?;
        let bootstrap_path = runtime_files
            .iter()
            .find(|file| {
                file.placement() == FilePlacement::InterpreterReplacement
                    && file.identity() == bootstrap_source.identity
                    && file.executable()
            })
            .ok_or_else(|| invalid("qualified bootstrap placement missing"))?;
        let bootstrap_resolved = inputs
            .resolve_path(bootstrap_path.path())
            .map_err(|e| invalid(e.to_string()))?;
        let bootstrap = Object::retain_root(
            prepared
                .backing(&bootstrap_resolved, bootstrap_source.identity)
                .map_err(|e| invalid(e.to_string()))?,
        )?;
        let runtime_path = inputs
            .resolve_path(
                &GuestPath::new(RUNTIME_PATH.to_vec()).map_err(|e| invalid(e.to_string()))?,
            )
            .map_err(|e| invalid(e.to_string()))?;
        let runtime = Object::retain_root(
            prepared
                .backing(&runtime_path, runtime_source.identity)
                .map_err(|e| invalid(e.to_string()))?,
        )?;
        let entry = artifacts
            .catalog()
            .first()
            .ok_or_else(|| invalid("missing artifact catalog"))?;
        let stock_fd = File::from(entry.stock_loader().try_clone_to_owned()?);
        let m = stock_fd.metadata()?;
        let stock_identity = CapturedIdentity {
            device: m.dev(),
            inode: m.ino(),
            size: m.len(),
            digest: entry.stock_loader_digest(),
        };
        let stock_loader = Object::retain(stock_fd.as_fd(), stock_identity)?;
        if artifacts
            .catalog()
            .iter()
            .any(|e| e.stock_loader_digest() != stock_identity.digest)
        {
            return Err(invalid("mixed stock loader catalog"));
        }
        // Catalog data is a distinct independently compiled/sealed role.
        // An arbitrary non-executable RuntimeFile at this name is insufficient.
        let catalog_identity = artifacts.driver_catalog_identity();
        let catalog_fd = File::from(artifacts.driver_catalog().try_clone_to_owned()?);
        let catalog_metadata = catalog_fd.metadata()?;
        if (
            catalog_metadata.dev(),
            catalog_metadata.ino(),
            catalog_metadata.len(),
        ) != (
            catalog_identity.device,
            catalog_identity.inode,
            catalog_identity.size,
        ) || unsafe { libc::fcntl(catalog_fd.as_raw_fd(), libc::F_GET_SEALS) } != ALL_SEALS
            || !runtime_files.iter().any(|file| {
                file.path().as_bytes() == detcore_model::liteinst_native_exec_catalog::PATH
                    && !file.executable()
                    && file.placement() == FilePlacement::Additional
                    && file.identity() == catalog_identity
            })
        {
            return Err(invalid("exact retained native driver catalog role missing"));
        }
        let approved = [
            bootstrap_source.key(),
            runner.key(),
            runtime_source.key(),
            stock_loader.key(),
        ]
        .into_iter()
        .collect::<BTreeSet<_>>();
        let mut paths = Vec::new();
        let mut backings = Vec::new();
        let mut occupied = BTreeSet::new();
        for file in runtime_files {
            let path = inputs
                .resolve_path(file.path())
                .map_err(|e| invalid(e.to_string()))?;
            let id = file.identity();
            let k = Key {
                device: id.device,
                inode: id.inode,
            };
            if file.executable() && !approved.contains(&k) {
                return Err(invalid(
                    "runtime executable has no independently qualified artifact role",
                ));
            }
            if file.placement() == FilePlacement::InterpreterReplacement
                && (!inputs.is_interpreter(file.path()) || id != bootstrap_source.identity)
            {
                return Err(invalid("unqualified interpreter replacement"));
            }
            if !occupied.insert(path.as_bytes().to_vec()) {
                return Err(invalid("duplicate actual runtime root path"));
            }
            let backing = prepared
                .backing(&path, id)
                .map_err(|e| invalid(e.to_string()))?;
            backing.verify().map_err(|e| invalid(e.to_string()))?;
            if backing.executable() != file.executable()
                || (file.placement() == FilePlacement::InterpreterReplacement
                    && backing.identity() != bootstrap.identity)
            {
                return Err(invalid("root backing policy or bootstrap alias differs"));
            }
            paths.push(observe_path(
                &root,
                path.as_bytes(),
                Some(backing.identity()),
                file.executable(),
            )?);
            backings.push(backing.clone());
        }
        for file in inputs.files() {
            if occupied.contains(file.resolved_path.as_bytes()) {
                if !runtime_files.iter().any(|r| {
                    r.placement() == FilePlacement::InterpreterReplacement
                        && inputs.resolve_path(r.path()).ok().as_ref() == Some(&file.resolved_path)
                }) {
                    return Err(invalid("runtime shadows independently declared input"));
                }
            } else {
                occupied.insert(file.resolved_path.as_bytes().to_vec());
                let backing = prepared
                    .backing(&file.resolved_path, file.identity)
                    .map_err(|e| invalid(e.to_string()))?;
                backing.verify().map_err(|e| invalid(e.to_string()))?;
                if backing.executable() != (file.role != InputRole::Data) {
                    return Err(invalid("declared input backing policy differs"));
                }
                paths.push(observe_path(
                    &root,
                    file.resolved_path.as_bytes(),
                    Some(backing.identity()),
                    file.role != InputRole::Data,
                )?);
                backings.push(backing.clone());
            }
        }
        let stock_path = inputs
            .resolve_path(
                &GuestPath::new(STOCK_LOADER_PATH.to_vec()).map_err(|e| invalid(e.to_string()))?,
            )
            .map_err(|e| invalid(e.to_string()))?;
        let stock_copy = prepared
            .backing(&stock_path, stock_identity)
            .map_err(|e| invalid(e.to_string()))?;
        if !paths.iter().any(|p| {
            p.path == stock_path.as_bytes()
                && p.key
                    == Key {
                        device: stock_copy.identity().device,
                        inode: stock_copy.identity().inode,
                    }
                && p.executable
        }) {
            return Err(invalid(
                "exact original stock loader not bound in common root",
            ));
        }
        for (path, source) in [
            (RUNNER_PATH, runner.identity),
            (RUNTIME_PATH, runtime_source.identity),
        ] {
            let resolved = inputs
                .resolve_path(&GuestPath::new(path.to_vec()).map_err(|e| invalid(e.to_string()))?)
                .map_err(|e| invalid(e.to_string()))?;
            let backing = prepared
                .backing(&resolved, source)
                .map_err(|e| invalid(e.to_string()))?;
            let id = backing.identity();
            if !paths.iter().any(|p| {
                p.path == resolved.as_bytes()
                    && p.key
                        == Key {
                            device: id.device,
                            inode: id.inode,
                        }
                    && p.executable
            }) {
                return Err(invalid("exact qualified runner/runtime path not bound"));
            }
        }
        for input in inputs
            .files()
            .iter()
            .filter(|f| f.role == InputRole::Executable)
        {
            let interpreter = input
                .interpreter
                .as_ref()
                .ok_or_else(|| invalid("actual target lacks its declared interpreter"))?;
            let resolved = inputs
                .resolve_path(interpreter)
                .map_err(|e| invalid(e.to_string()))?;
            if !runtime_files.iter().any(|r| {
                r.placement() == FilePlacement::InterpreterReplacement
                    && r.executable()
                    && r.identity() == bootstrap_source.identity
                    && inputs.resolve_path(r.path()).ok().as_ref() == Some(&resolved)
            }) {
                return Err(invalid(
                    "target interpreter lacks exact qualified bootstrap replacement",
                ));
            }
        }
        // These objects are installed and observed by the same common owner,
        // not accepted as another arbitrary executable-role authority.
        for path in ENDPOINTS {
            let p = observe_path(&root, path, None, false)?;
            if p.kind != libc::S_IFSOCK {
                return Err(invalid("protected endpoint is not a socket"));
            }
            paths.push(p);
        }
        // DETLOG and observer are distinct anonymous inherited seqpacket
        // endpoints. Their real descriptor authentication belongs to the
        // coordinator/driver; no placeholder root paths stand in for it.
        for path in DEVICES {
            let p = observe_path(&root, path, None, false)?;
            let from = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_PATH | libc::O_CLOEXEC)
                .open(std::path::Path::new(std::ffi::OsStr::from_bytes(path)))?;
            let source = from.metadata()?;
            if p.kind != libc::S_IFCHR
                || p.key
                    != (Key {
                        device: source.dev(),
                        inode: source.ino(),
                    })
                || p.rdev != source.rdev()
            {
                return Err(invalid("common device identity differs"));
            }
            paths.push(p);
        }
        if !runtime_files.iter().any(|r| {
            r.path().as_bytes() == b"/etc/group"
                && r.placement() == FilePlacement::Additional
                && !r.executable()
        }) {
            return Err(invalid(
                "once-held common group identity missing from root roles",
            ));
        }
        let tmp = root_open(&root, b"/tmp", false)?;
        let flags = view(tmp.as_fd())?;
        if !tmp.metadata()?.is_dir()
            || flags & libc::ST_RDONLY != 0
            || flags & (libc::ST_NOSUID | libc::ST_NODEV | libc::ST_NOEXEC)
                != (libc::ST_NOSUID | libc::ST_NODEV | libc::ST_NOEXEC)
        {
            return Err(invalid("common writable tmp policy differs"));
        }
        let all = mounts(&read_proc(
            unsafe { libc::getpid() },
            "mountinfo",
            MOUNTINFO_CAP,
        )?)?;
        let base = all
            .iter()
            .find(|r| r.id == prepared.mount_id())
            .ok_or_else(|| invalid("actual prepared mount absent"))?;
        let mut ids = paths.iter().map(|p| p.mount).collect::<BTreeSet<_>>();
        ids.insert(prepared.mount_id());
        ids.insert(mount_id(tmp.as_fd())?);
        let mut expected = BTreeMap::new();
        for r in all
            .iter()
            .filter(|r| descends(&all, r.id, prepared.mount_id()))
        {
            if !ids.remove(&r.id) {
                return Err(invalid("unknown mount under prepared root"));
            }
            let mut row = r.clone();
            row.path = if r.id == prepared.mount_id() {
                b"/".to_vec()
            } else {
                let tail = r
                    .path
                    .strip_prefix(base.path.as_slice())
                    .ok_or_else(|| invalid("prepared mount ancestry path differs"))?;
                if !tail.starts_with(b"/") {
                    return Err(invalid("prepared mount prefix boundary differs"));
                }
                tail.to_vec()
            };
            expected.insert(r.id, row);
        }
        if !ids.is_empty() {
            return Err(invalid("held root mount not in actual root topology"));
        }
        let aliases = inputs
            .aliases()
            .iter()
            .map(|a| (a.path.as_bytes().to_vec(), a.target.clone()))
            .collect::<Vec<_>>();
        for (path, target) in &aliases {
            if read_link(&root, path)? != *target {
                return Err(invalid("declared alias bytes differ in actual root"));
            }
        }
        let mut targets = BTreeMap::new();
        let mut libraries = Vec::new();
        let interpreter_paths = inputs
            .resolved_interpreters()
            .map_err(|e| invalid(e.to_string()))?;
        for file in inputs.files().iter().filter(|f| f.role != InputRole::Data) {
            // The original interpreter source is descriptor-mapped twice;
            // its declared pathname now contains the bootstrap replacement.
            if interpreter_paths.contains(&file.resolved_path) {
                if file.identity.digest != stock_identity.digest {
                    return Err(invalid(
                        "original interpreter source differs from qualified loader",
                    ));
                }
                continue;
            }
            let backing = prepared
                .backing(&file.resolved_path, file.identity)
                .map_err(|e| invalid(e.to_string()))?;
            let object = Object::retain_root(backing)?;
            if file.role == InputRole::Executable {
                if !artifacts
                    .catalog()
                    .iter()
                    .any(|entry| entry.target_digest == file.identity.digest)
                {
                    return Err(invalid(
                        "held target has no independent qualified catalog entry",
                    ));
                }
                targets
                    .entry(file.identity.digest)
                    .or_insert_with(Vec::new)
                    .push(object);
            } else if object.key() != stock_loader.key() {
                libraries.push((file.path.clone(), object));
            }
        }
        if targets.is_empty() {
            return Err(invalid("root has no actual dynamic target"));
        }
        let own = parse_maps(&read_proc(unsafe { libc::getpid() }, "maps", MAPS_CAP)?)?;
        let pseudo = own
            .iter()
            .filter(|r| r.name == b"[vsyscall]")
            .collect::<Vec<_>>();
        if pseudo.len() > 1 || pseudo.first().is_some_and(|r| !kernel_vsyscall(r)) {
            return Err(invalid("unsupported kernel vsyscall pseudo-VMA"));
        }
        let vsyscall = pseudo.first().map(|r| (*r).clone());
        Ok(Arc::new(Self {
            root,
            device: prepared.device(),
            inode: prepared.inode(),
            mount: prepared.mount_id(),
            namespace: prepared.namespace(),
            inputs,
            artifacts,
            backings,
            paths,
            aliases,
            mounts: expected,
            targets,
            libraries,
            bootstrap,
            runner,
            runtime,
            stock_loader,
            vsyscall,
        }))
    }
    pub(super) fn bind_startup(
        self: &Arc<Self>,
        received_root: OwnedFd,
        observation: NativeRootObservation,
        watch: Arc<NativeWatchedProcess>,
    ) -> io::Result<Arc<WatchedRoot>> {
        if observation.pid <= 0
            || observation.pid != observation.tid
            || observation.pid != watch.process_id()
            || observation.device != self.device
            || observation.inode != self.inode
            || observation.mount_id != self.mount
            || (observation.namespace_device, observation.namespace_inode) != self.namespace
            || !watch.watch_is_healthy()
        {
            return Err(invalid("actual startup root/watch observation differs"));
        }
        let root = File::from(received_root);
        exact_root(&root, self.device, self.inode, self.mount, true)?;
        let pidfd = watch.retained_pidfd().try_clone_to_owned()?;
        // Only the actual original mask is measured before ACK. Capability,
        // NoNewPrivs and installed-filter predicates are checked at Initial.
        let text = read_proc(observation.pid, "status", STATUS_CAP)?;
        let mask = initial_mask(&text)?;
        if mask != 0 {
            return Err(invalid(
                "first-domain original guest signal mask is not zero",
            ));
        }
        let result = Arc::new(WatchedRoot {
            identity: Arc::clone(self),
            watch,
            pidfd,
            received_root: root,
            initial_guest_mask: mask,
        });
        result.verify()?;
        if initial_mask(&read_proc(observation.pid, "status", STATUS_CAP)?)? != mask {
            return Err(invalid(
                "actual original mask moved during startup observation",
            ));
        }
        Ok(result)
    }
}
use std::os::unix::ffi::OsStrExt;
fn initial_mask(bytes: &[u8]) -> io::Result<u64> {
    let text = std::str::from_utf8(bytes).map_err(|_| invalid("status UTF8"))?;
    let values = text
        .lines()
        .filter_map(|l| l.strip_prefix("SigBlk:"))
        .collect::<Vec<_>>();
    if values.len() != 1 {
        return Err(invalid("actual original mask missing/duplicate"));
    }
    number(values[0].trim(), 16)
}
/// Bound actual child/root. This type cannot be constructed from PID or packet
/// scalars alone and does not implement any unsafe launch-authority trait.
pub(super) struct WatchedRoot {
    identity: Arc<RootIdentity>,
    watch: Arc<NativeWatchedProcess>,
    pidfd: OwnedFd,
    received_root: File,
    initial_guest_mask: u64,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Image {
    pub(super) target_digest: [u8; 32],
    pub(super) role_bases: BTreeMap<String, Vec<u64>>,
    pub(super) auxv: BTreeMap<u64, u64>,
    pub(super) namespace: (u64, u64),
    pub(super) observed_mask: u64,
    /// Actual complete bootstrap record read through the retained watched PID.
    /// Data observation alone does not prove guest control/map ownership.
    pub(super) native_resume: [u8; 256],
}
const RESUME_ADDRESS: u64 =
    reverie_inguest::guest::exec_private::NativeLoaderResume::ADDRESS as u64;
const RESUME_PAGE: usize = reverie_inguest::guest::exec_private::NativeLoaderResume::MAPPING_BYTES;
fn resume_word(bytes: &[u8; 256], index: usize) -> u64 {
    u64::from_le_bytes(bytes[index * 8..index * 8 + 8].try_into().unwrap())
}
fn check_resume(
    rows: &[Row],
    record: &[u8; 256],
    tail: &[u8],
    pid: i32,
    stock_loader: &Object,
    loader_bases: &[u64],
) -> io::Result<()> {
    let page = rows
        .iter()
        .find(|row| row.lo == RESUME_ADDRESS && row.hi == RESUME_ADDRESS + RESUME_PAGE as u64);
    if !page.is_some_and(|r| {
        r.permissions == *b"r--p"
            && r.offset == 0
            && r.major == 0
            && r.minor == 0
            && r.inode == 0
            && r.name.is_empty()
    }) || tail.len() != RESUME_PAGE - 256
        || tail.iter().any(|b| *b != 0)
        || &record[..8] != b"HLXREG01"
        || resume_word(record, 1) != 1
        || resume_word(record, 2) != 256
        || resume_word(record, 3) != pid as u64
        || resume_word(record, 4 + 15) != u64::MAX
        || resume_word(record, 4 + 18) != 0x202
        || resume_word(record, 31) > u32::MAX as u64
        || resume_word(record, 31) & 1 != 0
    {
        return Err(invalid("actual native loader resume mapping/ABI differs"));
    }
    let rip = resume_word(record, 4 + 16);
    let rsp = resume_word(record, 4 + 19);
    // This is the final stock-loader RIP, never the target's AT_ENTRY. Two
    // actual mappings of one retained loader exist; the record must name one
    // unique admitted entry and its real executable PT_LOAD coverage.
    let matches = loader_bases
        .iter()
        .filter(|base| {
            base.checked_add(stock_loader.entry) == Some(rip)
                && stock_loader.segments.iter().any(|segment| {
                    segment.flags & 1 != 0
                        && base.checked_add(segment.address).is_some_and(|lo| {
                            lo <= rip
                                && lo.checked_add(segment.file_size).is_some_and(|hi| rip < hi)
                        })
                })
        })
        .count();
    if matches != 1
        || !rows.iter().any(|r| {
            r.lo <= rsp
                && rsp < r.hi
                && r.permissions == *b"rw-p"
                && r.inode == 0
                && r.name == b"[stack]"
        })
    {
        return Err(invalid(
            "resume does not bind actual loader entry/original stack mapping",
        ));
    }
    Ok(())
}
fn read_resume(pid: i32) -> io::Result<[u8; RESUME_PAGE]> {
    let mut bytes = [0u8; RESUME_PAGE];
    let local = libc::iovec {
        iov_base: bytes.as_mut_ptr().cast(),
        iov_len: bytes.len(),
    };
    let remote = libc::iovec {
        iov_base: RESUME_ADDRESS as *mut libc::c_void,
        iov_len: bytes.len(),
    };
    let n = unsafe { libc::process_vm_readv(pid, &local, 1, &remote, 1, 0) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    if n as usize != bytes.len() {
        return Err(invalid("short actual native resume read"));
    }
    Ok(bytes)
}
impl WatchedRoot {
    pub(super) fn initial_guest_mask(&self) -> u64 {
        self.initial_guest_mask
    }
    pub(super) fn verify(&self) -> io::Result<()> {
        live(&self.pidfd)?;
        if !self.watch.watch_is_healthy()
            || physical_exit_watch::birth_identity(&self.pidfd) != Some(self.watch.birth_identity())
        {
            return Err(invalid("actual watched birth/health changed"));
        }
        let id = &self.identity;
        id.inputs.recheck().map_err(|e| invalid(e.to_string()))?;
        if !Arc::ptr_eq(&id.inputs, id.artifacts.inputs()) {
            return Err(invalid("root source owner changed"));
        }
        exact_root(&id.root, id.device, id.inode, id.mount, true)?;
        exact_root(&self.received_root, id.device, id.inode, id.mount, true)?;
        let pid = self.watch.process_id();
        let peer_root = File::open(format!("/proc/{pid}/root"))?;
        exact_root(&peer_root, id.device, id.inode, id.mount, true)?;
        let ns = File::open(format!("/proc/{pid}/ns/mnt"))?.metadata()?;
        if (ns.dev(), ns.ino()) != id.namespace {
            return Err(invalid("actual common guest mount namespace changed"));
        }
        // This runs after the actual root/proc freeze, before startup ACK,
        // and again at Initial/Complete. Provisional capture is insufficient.
        for backing in &id.backings {
            backing.verify().map_err(|e| invalid(e.to_string()))?;
        }
        for path in &id.paths {
            check_path(&peer_root, path)?;
        }
        for (path, target) in &id.aliases {
            if read_link(&peer_root, path)? != *target {
                return Err(invalid("actual declared symlink changed"));
            }
        }
        if read_link(&peer_root, b"/proc/self")? != pid.to_string().as_bytes()
            || read_link(&peer_root, b"/proc/thread-self")?
                != format!("{pid}/task/{pid}").as_bytes()
        {
            return Err(invalid("actual child proc aliases differ"));
        }
        let leaves = proc_paths(pid);
        for path in &leaves {
            let object = root_open(&peer_root, path, false)?;
            let leaf = path
                .rsplit(|b| *b == b'/')
                .next()
                .ok_or_else(|| invalid("proc leaf missing"))?;
            let mut host = format!("/proc/{pid}/").into_bytes();
            host.extend_from_slice(leaf);
            let actual = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_PATH | libc::O_CLOEXEC)
                .open(std::path::Path::new(std::ffi::OsStr::from_bytes(&host)))?;
            if key(&object)? != key(&actual)?
                || view(object.as_fd())?
                    & (libc::ST_RDONLY | libc::ST_NOSUID | libc::ST_NODEV | libc::ST_NOEXEC)
                    != (libc::ST_RDONLY | libc::ST_NOSUID | libc::ST_NODEV | libc::ST_NOEXEC)
            {
                return Err(invalid("root proc leaf is not same actual watched task"));
            }
        }
        check_mountinfo(
            &read_proc(pid, "mountinfo", MOUNTINFO_CAP)?,
            &leaves,
            &id.mounts,
        )?;
        live(&self.pidfd)?;
        Ok(())
    }
    pub(super) fn image(&self, peer: &OwnedFd, target_digest: [u8; 32]) -> io::Result<Image> {
        self.verify()?;
        live(peer)?;
        if physical_exit_watch::birth_identity(peer) != Some(self.watch.birth_identity()) {
            return Err(invalid("image RPC peer birth differs"));
        }
        let pid = self.watch.process_id();
        let id = &self.identity;
        let targets = id
            .targets
            .get(&target_digest)
            .ok_or_else(|| invalid("selected original target not in retained root"))?;
        let before = read_proc(pid, "maps", MAPS_CAP)?;
        let status_before = read_proc(pid, "status", STATUS_CAP)?;
        let actual_aux = read_proc(pid, "auxv", AUXV_CAP)?;
        let after = read_proc(pid, "maps", MAPS_CAP)?;
        let status_after = read_proc(pid, "status", STATUS_CAP)?;
        let mask = status(&status_before, pid, 1)?;
        if before != after
            || mask != status(&status_after, pid, 1)?
            || actual_aux != read_proc(pid, "auxv", AUXV_CAP)?
        {
            return Err(invalid("actual image moved during mapping observation"));
        }
        let rows = parse_maps(&before)?;
        // Equal bytes at different declared paths may have different sealed
        // inodes. Never overwrite one with another or choose by input order.
        let target = selected_target(&rows, targets)?;
        let auxv = auxv(&actual_aux)?;
        let mut role_bases = BTreeMap::new();
        let mut known = BTreeSet::new();
        for (role, object, count) in [
            ("target", target, 1),
            ("bootstrap", &id.bootstrap, 1),
            ("runner", &id.runner, 1),
            ("runtime", &id.runtime, 1),
            ("stock_loader", &id.stock_loader, 2),
        ] {
            object.verify()?;
            role_bases.insert(role.to_owned(), mapped_object(&rows, object, count)?);
            known.insert(object.key());
        }
        let mut seen_libraries = BTreeSet::new();
        for (path, object) in &id.libraries {
            if !seen_libraries.insert(object.key()) {
                continue;
            }
            object.verify()?;
            if file_rows(&rows, object).is_empty() {
                continue;
            }
            let bases =
                mapped_object(&rows, object, 1).or_else(|_| mapped_object(&rows, object, 2))?;
            role_bases.insert(format!("declared_loader:{:?}", path.as_bytes()), bases);
            known.insert(object.key());
        }
        if auxv.get(&7) != Some(&0)
            || auxv.get(&9) != role_bases["target"][0].checked_add(target.entry).as_ref()
        {
            return Err(invalid(
                "actual AT_BASE/AT_ENTRY does not name original target",
            ));
        }
        let resume = read_resume(pid)?;
        let record: [u8; 256] = resume[..256].try_into().unwrap();
        check_resume(
            &rows,
            &record,
            &resume[256..],
            pid,
            &id.stock_loader,
            &role_bases["stock_loader"],
        )?;
        if resume != read_resume(pid)? || before != read_proc(pid, "maps", MAPS_CAP)? {
            return Err(invalid("native resume/image changed during observation"));
        }
        executable_rows(&rows, &known, &auxv, &id.vsyscall)?;
        self.verify()?;
        live(peer)?;
        Ok(Image {
            target_digest,
            role_bases,
            auxv,
            namespace: id.namespace,
            observed_mask: mask,
            native_resume: record,
        })
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn model_maps_exact_extents_and_every_truncation_refuse() {
        let bytes = b"00400000-00401000 r-xp 00000000 00:01 42 /memfd:sealed (deleted)\n";
        assert_eq!(parse_maps(bytes).unwrap()[0].inode, 42);
        for end in 0..bytes.len() {
            assert!(parse_maps(&bytes[..end]).is_err());
        }
        assert!(parse_maps(b"00400000-00401000 rwxp 00000000 00:01 42 x\n").is_ok());
    }
    #[test]
    fn model_mountinfo_requires_exact_live_leaves_not_whole_host_proc() {
        let valid=b"1 1 0:1 / / ro,nosuid,nodev - tmpfs tmpfs ro\n2 1 0:2 / /proc/self/maps ro,nosuid,nodev,noexec - proc proc ro\n";
        let expected = BTreeSet::from([b"/proc/self/maps".to_vec()]);
        let root = mounts(valid).unwrap()[0].clone();
        let held = BTreeMap::from([(root.id, root)]);
        assert!(check_mountinfo(valid, &expected, &held).is_ok());
        for wrong in [
            String::from_utf8(valid.to_vec())
                .unwrap()
                .replace("maps ro", "maps rw"),
            String::from_utf8(valid.to_vec())
                .unwrap()
                .replace(",noexec", ""),
            String::from_utf8(valid.to_vec())
                .unwrap()
                .replace("- proc", "- tmpfs"),
            String::from_utf8(valid.to_vec())
                .unwrap()
                .replace("/proc/self/maps", "/proc"),
            String::from_utf8(valid.to_vec())
                .unwrap()
                .replace("/proc/self/maps", "/proc/1/root"),
        ] {
            assert!(check_mountinfo(wrong.as_bytes(), &expected, &held).is_err());
        }
    }
    #[test]
    fn model_auxv_exact_terminator_and_mandatory_values() {
        let mut bytes = Vec::new();
        for (k, v) in [
            (3u64, 0x401000u64),
            (6, 4096),
            (7, 0x500000),
            (9, 0x401100),
            (0, 0),
        ] {
            bytes.extend_from_slice(&k.to_ne_bytes());
            bytes.extend_from_slice(&v.to_ne_bytes());
        }
        assert!(auxv(&bytes).is_ok());
        let mut extra = bytes.clone();
        extra.extend_from_slice(&[0; 16]);
        assert!(auxv(&extra).is_err());
        bytes[24..32].copy_from_slice(&8192u64.to_ne_bytes());
        assert!(auxv(&bytes).is_err());
    }
}

#[cfg(test)]
mod observation_controls {
    use super::*;

    fn pseudo(name: &[u8], lo: u64, hi: u64, permissions: [u8; 4]) -> Row {
        Row {
            lo,
            hi,
            offset: 0,
            major: 0,
            minor: 0,
            inode: 0,
            permissions,
            name: name.to_vec(),
        }
    }

    #[test]
    fn model_kernel_vsyscall_is_exact_and_requires_coordinator_witness() {
        // Parser/classifier control only. A synthetic row supplies no kernel,
        // namespace, physical child, frontend or native exec qualification.
        let vdso = pseudo(b"[vdso]", 0x7000_0000, 0x7000_1000, *b"r-xp");
        let kernel = pseudo(
            b"[vsyscall]",
            0xffff_ffff_ff60_0000,
            0xffff_ffff_ff60_1000,
            *b"--xp",
        );
        let aux = BTreeMap::from([(33, vdso.lo)]);
        let held = Some(kernel.clone());
        let known = BTreeSet::new();
        assert!(kernel_vsyscall(&kernel));
        assert!(executable_rows(&[vdso.clone(), kernel.clone()], &known, &aux, &held).is_ok());
        assert!(executable_rows(&[vdso.clone(), kernel.clone()], &known, &aux, &None).is_err());
        assert!(executable_rows(std::slice::from_ref(&vdso), &known, &aux, &held).is_err());
        let mut mismatched_witness = kernel.clone();
        mismatched_witness.hi += PAGE;
        assert!(
            executable_rows(
                &[vdso.clone(), kernel.clone()],
                &known,
                &aux,
                &Some(mismatched_witness)
            )
            .is_err()
        );
        let mut mutations = Vec::new();
        let mut changed = kernel.clone();
        changed.lo -= PAGE;
        mutations.push(changed);
        let mut changed = kernel.clone();
        changed.hi += PAGE;
        mutations.push(changed);
        let mut changed = kernel.clone();
        changed.permissions = *b"r-xp";
        mutations.push(changed);
        let mut changed = kernel.clone();
        changed.permissions = *b"--xs";
        mutations.push(changed);
        let mut changed = kernel.clone();
        changed.name = b"[vdso]".to_vec();
        mutations.push(changed);
        let mut changed = kernel.clone();
        changed.major = 1;
        mutations.push(changed);
        let mut changed = kernel.clone();
        changed.minor = 1;
        mutations.push(changed);
        let mut changed = kernel.clone();
        changed.inode = 1;
        mutations.push(changed);
        let mut changed = kernel.clone();
        changed.offset = PAGE;
        mutations.push(changed);
        for changed in mutations {
            assert!(!kernel_vsyscall(&changed));
            assert!(executable_rows(&[vdso.clone(), changed], &known, &aux, &held).is_err());
        }
        for extra in [
            pseudo(b"", 0x8000_0000, 0x8000_1000, *b"r-xp"),
            pseudo(b"[anon:unknown]", 0x8000_0000, 0x8000_1000, *b"r-xp"),
            Row {
                inode: 9,
                major: 1,
                ..pseudo(b"unknown-file", 0x8000_0000, 0x8000_1000, *b"r-xp")
            },
        ] {
            assert!(
                executable_rows(&[vdso.clone(), kernel.clone(), extra], &known, &aux, &held)
                    .is_err()
            );
        }
        let mut wrong_vdso = vdso.clone();
        wrong_vdso.lo += PAGE;
        assert!(executable_rows(&[wrong_vdso, kernel], &known, &aux, &held).is_err());
    }

    fn sealed_elf(bytes: &[u8]) -> Object {
        let file =
            super::super::native_exec_inputs::seal_bytes(bytes, InputRole::Executable).unwrap();
        let metadata = file.metadata().unwrap();
        Object::retain(
            file.as_fd(),
            CapturedIdentity {
                device: metadata.dev(),
                inode: metadata.ino(),
                size: metadata.len(),
                digest: Sha256::digest(bytes).into(),
            },
        )
        .unwrap()
    }

    struct MappedElf {
        start: *mut libc::c_void,
        size: usize,
        base: u64,
    }
    impl Drop for MappedElf {
        fn drop(&mut self) {
            assert_eq!(unsafe { libc::munmap(self.start, self.size) }, 0);
        }
    }
    fn map_real_segments(object: &Object) -> MappedElf {
        // Reserve the entire extent first: every MAP_FIXED is wholly inside
        // our live reservation. Other parallel tests cannot reuse these VMAs.
        let min = object
            .segments
            .iter()
            .map(|s| s.address & !(PAGE - 1))
            .min()
            .unwrap();
        let max = object
            .segments
            .iter()
            .map(|s| {
                s.address
                    .checked_add(s.memory_size)
                    .unwrap()
                    .checked_add(PAGE - 1)
                    .unwrap()
                    & !(PAGE - 1)
            })
            .max()
            .unwrap();
        let size = usize::try_from(max.checked_sub(min).unwrap()).unwrap();
        let start = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size,
                libc::PROT_NONE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert_ne!(start, libc::MAP_FAILED);
        let mapping = MappedElf {
            start,
            size,
            base: (start as u64).checked_sub(min).unwrap(),
        };
        for segment in object.segments.iter().filter(|s| s.file_size != 0) {
            let (lo, hi, offset) = segment_extent(mapping.base, segment).unwrap();
            assert!(lo >= start as u64 && hi <= (start as u64).checked_add(size as u64).unwrap());
            let protection = if segment.flags & 4 != 0 {
                libc::PROT_READ
            } else {
                0
            } | if segment.flags & 2 != 0 {
                libc::PROT_WRITE
            } else {
                0
            } | if segment.flags & 1 != 0 {
                libc::PROT_EXEC
            } else {
                0
            };
            let actual = unsafe {
                libc::mmap(
                    lo as *mut _,
                    (hi - lo) as usize,
                    protection,
                    libc::MAP_PRIVATE | libc::MAP_FIXED,
                    object.file.as_raw_fd(),
                    offset as libc::off_t,
                )
            };
            assert_eq!(actual, lo as *mut _);
        }
        mapping
    }
    fn actual_rows() -> Vec<Row> {
        parse_maps(&read_proc(unsafe { libc::getpid() }, "maps", MAPS_CAP).unwrap()).unwrap()
    }

    #[test]
    fn model_resume_binds_stock_loader_not_target_and_full_fixed_record() {
        // Parser/model negatives around a real held ELF; the rows/record here
        // confer no actual child, tracked control, register or launch proof.
        let bytes = std::fs::read("/bin/true").unwrap();
        let loader = sealed_elf(&bytes);
        let base = 0x7000_0000u64;
        let rip = base.checked_add(loader.entry).unwrap();
        let rows = vec![
            pseudo(
                b"",
                RESUME_ADDRESS,
                RESUME_ADDRESS + RESUME_PAGE as u64,
                *b"r--p",
            ),
            pseudo(b"[stack]", 0x8000_0000, 0x8000_1000, *b"rw-p"),
        ];
        let mut record = [0u8; 256];
        let set = |record: &mut [u8; 256], word: usize, value: u64| {
            record[word * 8..word * 8 + 8].copy_from_slice(&value.to_le_bytes());
        };
        record[..8].copy_from_slice(b"HLXREG01");
        for (word, value) in [
            (1, 1),
            (2, 256),
            (3, 42),
            (19, u64::MAX),
            (20, rip),
            (22, 0x202),
            (23, 0x8000_0800),
            (31, 0),
        ] {
            set(&mut record, word, value);
        }
        let tail = vec![0u8; RESUME_PAGE - 256];
        let check = |rows: &[Row], record: &[u8; 256], tail: &[u8], bases: &[u64]| {
            check_resume(rows, record, tail, 42, &loader, bases)
        };
        check(&rows, &record, &tail, &[base]).unwrap();
        for (word, value) in [
            (0, 0),
            (1, 2),
            (2, 255),
            (3, 43),
            (19, 0),
            (20, loader.entry),
            (20, rip + 1),
            (22, 0x246),
            (23, 0x8000_1000),
            (31, 1),
            (31, 1u64 << 32),
        ] {
            let mut wrong = record;
            set(&mut wrong, word, value);
            assert!(
                check(&rows, &wrong, &tail, &[base]).is_err(),
                "resume word {word}"
            );
        }
        for mutate in 0..8 {
            let mut wrong = rows.clone();
            match mutate {
                0 => wrong[0].lo += PAGE,
                1 => wrong[0].hi += PAGE,
                2 => wrong[0].permissions = *b"rw-p",
                3 => wrong[0].permissions = *b"r--s",
                4 => wrong[0].inode = 1,
                5 => wrong[0].offset = PAGE,
                6 => wrong[0].name = b"[anon:other]".to_vec(),
                _ => wrong[1].permissions = *b"rwxp",
            }
            assert!(check(&wrong, &record, &tail, &[base]).is_err());
        }
        assert!(check(&rows, &record, &tail, &[]).is_err());
        assert!(check(&rows, &record, &tail, &[base, base]).is_err());
        let mut wrong_tail = tail.clone();
        wrong_tail[0] = 1;
        assert!(check(&rows, &record, &wrong_tail, &[base]).is_err());
        assert!(check(&rows, &record, &tail[..tail.len() - 1], &[base]).is_err());
    }

    #[test]
    fn actual_bootstrap_loads_without_file_headers_require_exact_maps() {
        // Map the actual compiled and qualified bootstrap, without executing
        // it. Its ELF headers are intentionally outside its file-backed loads.
        let (file, identity) =
            super::super::native_exec_artifacts::bootstrap_mapping_fixture().unwrap();
        let object = Object::retain(file.as_fd(), identity).unwrap();
        let anchor = object.segments.iter().find(|s| s.file_size > 0).unwrap();
        assert_eq!(anchor.offset, PAGE);
        assert!(
            object
                .segments
                .iter()
                .all(|s| s.file_size == 0 || s.offset != 0)
        );
        let first = map_real_segments(&object);
        let rows = actual_rows();
        assert_eq!(mapped_object(&rows, &object, 1).unwrap(), vec![first.base]);
        assert!(mapped_object(&rows, &object, 0).is_err());
        assert!(mapped_object(&rows, &object, 2).is_err());

        let anchor_row = rows
            .iter()
            .position(|r| r.inode == identity.inode && r.offset == PAGE)
            .unwrap();
        for control in 0..5 {
            let mut wrong = rows.clone();
            match control {
                0 => wrong[anchor_row].offset = 0,
                1 => wrong[anchor_row].permissions[2] = b'-',
                2 => wrong[anchor_row].permissions[3] = b's',
                3 => wrong[anchor_row].permissions[1] = b'w',
                _ => {
                    wrong.remove(anchor_row);
                }
            }
            assert!(mapped_object(&wrong, &object, 1).is_err(), "{control}");
        }
        let mut extra = rows.clone();
        let mut header = rows[anchor_row].clone();
        header.lo = header.lo.checked_sub(PAGE).unwrap();
        header.hi = header.lo + PAGE;
        header.offset = 0;
        extra.push(header);
        assert!(mapped_object(&extra, &object, 1).is_err());

        let second = map_real_segments(&object);
        let rows = actual_rows();
        assert!(mapped_object(&rows, &object, 1).is_err());
        let mut bases = vec![first.base, second.base];
        bases.sort_unstable();
        assert_eq!(mapped_object(&rows, &object, 2).unwrap(), bases);
    }

    #[test]
    fn actual_sealed_elf_loads_require_exact_coverage_roles_and_multiplicity() {
        // Actual kernel/file mapping component only; we never execute these
        // bytes, install a Tool, make a child receipt or grant launch authority.
        let bytes = std::fs::read("/bin/true").unwrap();
        let object = sealed_elf(&bytes);
        let first = map_real_segments(&object);
        let rows = actual_rows();
        assert_eq!(mapped_object(&rows, &object, 1).unwrap(), vec![first.base]);
        assert!(mapped_object(&rows, &object, 2).is_err());
        let mut wrong_offset = rows.clone();
        wrong_offset
            .iter_mut()
            .find(|r| r.inode == object.identity.inode)
            .unwrap()
            .offset += PAGE;
        assert!(mapped_object(&wrong_offset, &object, 1).is_err());
        let mut wrong_permissions = rows.clone();
        wrong_permissions
            .iter_mut()
            .find(|r| r.inode == object.identity.inode && r.permissions[2] == b'x')
            .unwrap()
            .permissions[2] = b'-';
        assert!(mapped_object(&wrong_permissions, &object, 1).is_err());
        let mut missing = rows.clone();
        missing.retain(|r| !(r.inode == object.identity.inode && r.permissions[2] == b'x'));
        assert!(mapped_object(&missing, &object, 1).is_err());
        let second = map_real_segments(&object);
        let rows = actual_rows();
        assert!(mapped_object(&rows, &object, 1).is_err());
        let mut bases = vec![first.base, second.base];
        bases.sort_unstable();
        assert_eq!(mapped_object(&rows, &object, 2).unwrap(), bases);
        let mut wrong_identity = object.identity;
        wrong_identity.digest[0] ^= 1;
        assert!(Object::retain(object.file.as_fd(), wrong_identity).is_err());
        let unsealed = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(unsealed.path(), &bytes).unwrap();
        let metadata = unsealed.as_file().metadata().unwrap();
        assert!(
            Object::retain(
                unsealed.as_file().as_fd(),
                CapturedIdentity {
                    device: metadata.dev(),
                    inode: metadata.ino(),
                    size: metadata.len(),
                    digest: Sha256::digest(&bytes).into(),
                }
            )
            .is_err()
        );
    }

    #[test]
    fn actual_identical_target_bytes_keep_distinct_inodes_and_refuse_ambiguous_images() {
        let bytes = std::fs::read("/bin/true").unwrap();
        let candidates = vec![sealed_elf(&bytes), sealed_elf(&bytes)];
        assert_eq!(candidates[0].identity.digest, candidates[1].identity.digest);
        assert_ne!(candidates[0].key(), candidates[1].key());
        assert!(selected_target(&actual_rows(), &candidates).is_err());
        let first = map_real_segments(&candidates[0]);
        assert_eq!(
            selected_target(&actual_rows(), &candidates).unwrap().key(),
            candidates[0].key()
        );
        let second = map_real_segments(&candidates[1]);
        assert!(selected_target(&actual_rows(), &candidates).is_err());
        drop(second);
        assert_eq!(
            selected_target(&actual_rows(), &candidates).unwrap().key(),
            candidates[0].key()
        );
        drop(first);
        assert!(selected_target(&actual_rows(), &candidates).is_err());
    }
}
