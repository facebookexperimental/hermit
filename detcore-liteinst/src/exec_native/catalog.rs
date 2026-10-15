//! Read the catalog backing only inside the genuinely admitted native initializer.
//! Actual namespace/artifact/writer authority remains with its retained owner.

use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;

use detcore_model::liteinst_native_exec_catalog as wire;
use reverie_inguest::guest::native_exec_mappings::ExecutableBacking;
use reverie_inguest::guest::native_exec_mappings::ExecutableExtent;
use reverie_inguest::guest::native_exec_mappings::ExecutableObject;
use reverie_inguest::trap::raw_syscall6;
use sha2::Digest;
use sha2::Sha256;

/// The owner borrow and actual file survive until the initializer has installed
/// the policy. This is private; decoding data cannot create admission authority.
pub(crate) struct CapturedCatalog<'owner, O: ?Sized> {
    pub(crate) data: wire::Catalog,
    backings: Vec<ExecutableObject>,
    _file: OwnedFd,
    _owner: &'owner O,
}
impl<O: ?Sized> CapturedCatalog<'_, O> {
    pub(crate) fn into_parts(self) -> (wire::Catalog, Vec<ExecutableObject>) {
        (self.data, self.backings)
    }
}

struct Observation {
    stat: libc::stat,
    mount: u64,
    flags: u64,
}
fn observe(fd: i32) -> Result<Observation, &'static str> {
    let mut stat: libc::stat = unsafe { core::mem::zeroed() };
    call(
        libc::SYS_fstat,
        [fd as u64, (&raw mut stat) as u64, 0, 0, 0, 0],
    )?;
    // On Linux x86_64 this is the SYS_fstatfs layout. libc::statfs
    // hides the flags word; statfs64 exposes the same kernel field.
    const {
        assert!(core::mem::size_of::<libc::statfs64>() == 120);
        assert!(core::mem::align_of::<libc::statfs64>() == 8);
        assert!(core::mem::offset_of!(libc::statfs64, f_flags) == 80);
    }
    let mut fs: libc::statfs64 = unsafe { core::mem::zeroed() };
    call(
        libc::SYS_fstatfs,
        [fd as u64, (&raw mut fs) as u64, 0, 0, 0, 0],
    )?;
    let mut identity: libc::statx = unsafe { core::mem::zeroed() };
    call(
        libc::SYS_statx,
        [
            fd as u64,
            c"".as_ptr() as u64,
            (libc::AT_EMPTY_PATH | libc::AT_SYMLINK_NOFOLLOW) as u64,
            libc::STATX_MNT_ID as u64,
            (&raw mut identity) as u64,
            0,
        ],
    )?;
    let required = libc::ST_RDONLY | libc::ST_NOSUID | libc::ST_NODEV;
    if fs.f_type != libc::TMPFS_MAGIC
        || fs.f_flags as u64 & required != required
        || identity.stx_mask & libc::STATX_MNT_ID == 0
        || identity.stx_mnt_id == 0
    {
        return Err("native backing is not an observed readonly tmpfs mount");
    }
    Ok(Observation {
        stat,
        mount: identity.stx_mnt_id,
        flags: fs.f_flags as u64,
    })
}
fn same(before: &Observation, after: &Observation) -> bool {
    before.stat.st_dev == after.stat.st_dev
        && before.stat.st_ino == after.stat.st_ino
        && before.stat.st_size == after.stat.st_size
        && before.stat.st_mode == after.stat.st_mode
        && before.stat.st_nlink == after.stat.st_nlink
        && before.mount == after.mount
        && before.flags == after.flags
}
fn root_file(root: i32, path: &[u8]) -> Result<OwnedFd, &'static str> {
    let relative = path
        .strip_prefix(b"/")
        .ok_or("native backing path is not absolute")?;
    let name = std::ffi::CString::new(relative).map_err(|_| "native backing path contains NUL")?;
    #[repr(C)]
    struct How {
        flags: u64,
        mode: u64,
        resolve: u64,
    }
    let how = How {
        flags: (libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC) as u64,
        mode: 0,
        // The catalog records resolved placements, so even ordinary symlinks
        // are forbidden here. Bind mounts remain within the admitted root.
        resolve: 0x10 | 0x02 | 0x04, // IN_ROOT | NO_MAGICLINKS | NO_SYMLINKS.
    };
    let fd = call(
        libc::SYS_openat2,
        [
            root as u64,
            name.as_ptr() as u64,
            (&raw const how) as u64,
            core::mem::size_of::<How>() as u64,
            0,
            0,
        ],
    )?;
    Ok(unsafe { OwnedFd::from_raw_fd(fd as i32) })
}
fn hash_exact(fd: i32, size: u64) -> Result<[u8; 32], &'static str> {
    let mut bytes = [0u8; 4096];
    let mut hash = Sha256::new();
    let mut offset = 0;
    while offset < size {
        let length = (size - offset).min(bytes.len() as u64);
        let n = call(
            libc::SYS_pread64,
            [fd as u64, bytes.as_mut_ptr() as u64, length, offset, 0, 0],
        )? as u64;
        if n == 0 || n > length {
            return Err("native backing read shortened");
        }
        hash.update(&bytes[..n as usize]);
        offset += n;
    }
    if call(
        libc::SYS_pread64,
        [fd as u64, bytes.as_mut_ptr() as u64, 1, offset, 0, 0],
    )? != 0
    {
        return Err("native backing read grew");
    }
    Ok(hash.finalize().into())
}

fn call(number: i64, args: [u64; 6]) -> Result<i64, &'static str> {
    // SAFETY: each call below supplies its actual stack/file buffers and uses
    // the already qualified fixed gate under the initializer's real owner.
    let result = unsafe { raw_syscall6(number, args) };
    if result < 0 {
        return Err("native catalog kernel operation failed");
    }
    Ok(result)
}

/// # Safety
/// The actual native Initial or Successor admission has already authenticated
/// this physical leader, protected root, namespace and artifact placements before
/// Guest work.
/// `owner` must retain that real authority throughout this borrow. The fixed
/// path and its ancestors cannot be replaced or rebound. `expected` comes from
/// the actual compiled source/schema and independently authenticated bootstrap;
/// it is not copied from this file or an unredeemed packet. All allocation and
/// reading here occur before publishing the installed Tool and before any exec
/// final cut. The decoded rows alone must never instantiate a native driver.
pub(crate) unsafe fn capture<O: ?Sized>(
    expected: wire::Bindings,
    owner: &O,
) -> Result<CapturedCatalog<'_, O>, &'static str> {
    let root = call(
        libc::SYS_openat,
        [
            libc::AT_FDCWD as u64,
            c"/".as_ptr() as u64,
            (libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC) as u64,
            0,
            0,
            0,
        ],
    )?;
    let root = unsafe { OwnedFd::from_raw_fd(root as i32) };
    let root_before = observe(root.as_raw_fd())?;
    if root_before.stat.st_mode & libc::S_IFMT != libc::S_IFDIR
        || root_before.flags & libc::ST_NOEXEC != 0
    {
        return Err("native admitted root mode or execution policy differs");
    }
    let file = root_file(root.as_raw_fd(), wire::PATH)?;
    let fd = file.as_raw_fd();
    let before = observe(fd)?;
    if before.stat.st_mode != (libc::S_IFREG | 0o444)
        || before.stat.st_size < wire::HEADER_BYTES as i64
        || before.stat.st_size > wire::MAX_BYTES as i64
        || before.flags & libc::ST_NOEXEC == 0
        || before.mount == root_before.mount
    {
        return Err("native catalog is not its bounded readonly data backing");
    }
    // The actual host admission already joined this exact backing's bytes to
    // the independently retained sealed catalog, after root freeze and before
    // replying on the owned stream. A root flag or decoded binding alone is
    // insufficient authority. Sealed provider descriptors keep their seals.
    let mut bytes = vec![0; before.stat.st_size as usize];
    let mut offset = 0;
    while offset < bytes.len() {
        let n = call(
            libc::SYS_pread64,
            [
                fd as u64,
                bytes[offset..].as_mut_ptr() as u64,
                (bytes.len() - offset) as u64,
                offset as u64,
                0,
                0,
            ],
        )? as usize;
        if n == 0 || n > bytes.len() - offset {
            return Err("native catalog read shortened");
        }
        offset += n;
    }
    let mut extra = 0_u8;
    if call(
        libc::SYS_pread64,
        [fd as u64, (&raw mut extra) as u64, 1, offset as u64, 0, 0],
    )? != 0
    {
        return Err("native catalog read grew");
    }
    if !same(&before, &observe(fd)?) {
        return Err("native catalog file identity changed");
    }
    let data = wire::decode(&bytes).map_err(|_| "native catalog shape is unsupported")?;
    data.check_bindings(expected)
        .map_err(|_| "native catalog compiled bindings differ")?;
    let mut backings = Vec::new();
    for object in &data.objects {
        let Some(path) = &object.backing_path else {
            continue;
        };
        let backing = root_file(root.as_raw_fd(), path)?;
        let observed = observe(backing.as_raw_fd())?;
        let executable = !object.executable.is_empty();
        let mode = libc::S_IFREG | if executable { 0o555 } else { 0o444 };
        if observed.stat.st_mode != mode
            || observed.stat.st_size < 0
            || observed.stat.st_size as u64 != object.size
            || observed.mount == root_before.mount
            || (observed.flags & libc::ST_NOEXEC != 0) == executable
            || (observed.stat.st_dev, observed.stat.st_ino) == (object.device, object.inode)
            || hash_exact(backing.as_raw_fd(), object.size)? != object.digest
            || !same(&observed, &observe(backing.as_raw_fd())?)
        {
            return Err("native root backing differs from its sealed catalog source");
        }
        if executable {
            backings.push(ExecutableObject {
                device: observed.stat.st_dev,
                inode: observed.stat.st_ino,
                size: object.size,
                backing: ExecutableBacking::ReadonlyRoot {
                    mount_id: observed.mount,
                    mode,
                },
                extents: object
                    .executable
                    .iter()
                    .map(|extent| ExecutableExtent {
                        start: extent.start,
                        end: extent.end,
                    })
                    .collect::<Vec<_>>()
                    .into_boxed_slice(),
            });
        }
    }
    if !same(&root_before, &observe(root.as_raw_fd())?) {
        return Err("native admitted root changed while deriving backing identities");
    }
    Ok(CapturedCatalog {
        data,
        backings,
        _file: file,
        _owner: owner,
    })
}
