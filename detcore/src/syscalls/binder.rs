/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Descriptors a guest receives in a binder transaction.
//!
//! A binder transaction can carry file descriptors: a `BINDER_TYPE_FD`
//! object, or a `BINDER_TYPE_FDA` array inside a buffer object. Linux opens
//! each one in the receiving process when that process reads the transaction
//! with `ioctl(BINDER_WRITE_READ)`, and writes the new numbers into the
//! transaction's buffer in the receiver's binder mapping
//! (`binder_apply_fd_fixups`, always with `O_CLOEXEC`). No call that names
//! them as new descriptors ever reaches Detcore, as with `SCM_RIGHTS`
//! (`crate::syscalls::scm_rights`): on a backend where the tool keeps its own
//! descriptor table (ptrace, KVM), every later call through one of them that
//! consults that table failed with `EBADF`.
//!
//! So after a `BINDER_WRITE_READ` that read something, Detcore walks the
//! return commands Linux wrote (`read_buffer[..read_consumed]`), and for
//! each delivered transaction (`BR_TRANSACTION`, `BR_TRANSACTION_SEC_CTX`,
//! `BR_REPLY`) reads its offsets array and objects from the receiver's
//! mapping and names the descriptors they now hold. They are then recorded as
//! received `SCM_RIGHTS` descriptors are
//! (`Detcore::record_received_descriptors`), from the kernel's own facts.
//!
//! Layouts are the 64-bit ABI of `include/uapi/linux/android/binder.h`, the
//! only one an arm64 or x86-64 kernel builds (`binder_uintptr_t` and
//! `binder_size_t` are 64 bits). Nothing here changes what the guest sees:
//! the call has already returned, and its memory is only read. A buffer that
//! cannot be read or parsed names no descriptor, and says why.

use std::os::fd::RawFd;

use reverie::syscalls::Addr;
use reverie::syscalls::MemoryAccess;

use crate::syscalls::scm_rights::Delivered;

/// `_IOWR('b', 1, struct binder_write_read)`.
pub(crate) const BINDER_WRITE_READ: u64 = binder_request(3, 1, size_of::<WriteRead>());

/// `_IOR('r', 2, struct binder_transaction_data)`.
const BR_TRANSACTION: u32 = 0x8040_7202;
/// `_IOR('r', 2, struct binder_transaction_data_secctx)`.
const BR_TRANSACTION_SEC_CTX: u32 = 0x8048_7202;
/// `_IOR('r', 3, struct binder_transaction_data)`.
const BR_REPLY: u32 = 0x8040_7203;
/// `TF_STATUS_CODE`: the transaction's data is a 32-bit status. Linux does
/// not read it when it installs descriptors; the tests use it.
#[cfg(test)]
const TF_STATUS_CODE: u32 = 0x08;

/// `B_PACK_CHARS('p', 't', '*', B_TYPE_LARGE)`, a buffer object: a
/// descriptor array's parent, which the tests build.
#[cfg(test)]
const BINDER_TYPE_PTR: u32 = 0x7074_2a85;

/// `B_PACK_CHARS('f', 'd', '*', B_TYPE_LARGE)`.
const BINDER_TYPE_FD: u32 = 0x6664_2a85;
/// `B_PACK_CHARS('f', 'd', 'a', B_TYPE_LARGE)`.
const BINDER_TYPE_FDA: u32 = 0x6664_6185;

/// The most return-command bytes read from one call: Linux writes at most
/// the caller's `read_size`, and libbinder reads 256 bytes at a time.
const MAX_COMMAND_BYTES: usize = 64 * 1024;
/// The most objects read from one transaction's offsets array.
const MAX_OBJECTS: usize = 4096;

/// `struct binder_write_read`, the argument of `BINDER_WRITE_READ`: six
/// 64-bit words on a 64-bit kernel, in the uapi header's order.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct WriteRead {
    pub(crate) write_size: u64,
    pub(crate) write_consumed: u64,
    pub(crate) write_buffer: u64,
    pub(crate) read_size: u64,
    pub(crate) read_consumed: u64,
    pub(crate) read_buffer: u64,
}

/// `struct binder_freeze_info`, the argument of `BINDER_FREEZE`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct FreezeInfo {
    _pid: u32,
    enable: u32,
    timeout_ms: u32,
}

/// `_IOC(dir, 'b', nr, size)` for binder's own requests.
const fn binder_request(dir: u64, nr: u64, size: usize) -> u64 {
    (dir << 30) | ((size as u64) << 16) | ((b'b' as u64) << 8) | nr
}

/// `_IOW('b', 14, struct binder_freeze_info)`, its size from the structure
/// rather than written out, so a test cannot share a wrong number.
pub(crate) const BINDER_FREEZE: u64 = binder_request(1, 14, size_of::<FreezeInfo>());

/// `_IOW('b', 7, __s32)`: become the context manager, handle 0.
const BINDER_SET_CONTEXT_MGR: u64 = binder_request(1, 7, size_of::<i32>());
/// `_IOW('b', 13, struct flat_binder_object)`: become the context manager,
/// with the node's flags. `struct flat_binder_object` is 24 bytes: a 32-bit
/// type and flags, then the 64-bit `binder`/`handle` union and `cookie`.
const BINDER_SET_CONTEXT_MGR_EXT: u64 = binder_request(1, 13, 24);

/// Every request `binder_ioctl` serves at v6.1 (drivers/android/binder.c),
/// each with its argument's size as `include/uapi/linux/android/binder.h`
/// spells it: `BINDER_WRITE_READ`, `SET_MAX_THREADS`, `SET_CONTEXT_MGR`,
/// `THREAD_EXIT`, `VERSION`, `GET_NODE_DEBUG_INFO`, `GET_NODE_INFO_FOR_REF`,
/// `SET_CONTEXT_MGR_EXT`, `FREEZE`, `GET_FROZEN_INFO`,
/// `ENABLE_ONEWAY_SPAM_DETECTION` and `GET_EXTENDED_ERROR`. The header's
/// `SET_IDLE_TIMEOUT` and `SET_IDLE_PRIORITY` are left out: the driver
/// answers them `EINVAL` like any unknown request, and the first is the very
/// word of dma-buf's `DMA_BUF_IOCTL_IMPORT_SYNC_FILE`. The type byte `'b'`
/// is shared with other drivers, dma-buf among them, so a request counts as
/// binder's only when number, direction and size all match one of these;
/// the descriptor is not consulted, as for `BINDER_WRITE_READ` elsewhere in
/// this module.
const BINDER_REQUESTS: [u64; 12] = [
    BINDER_WRITE_READ,
    binder_request(1, 5, size_of::<u32>()),
    BINDER_SET_CONTEXT_MGR,
    binder_request(1, 8, size_of::<i32>()),
    binder_request(3, 9, size_of::<i32>()),
    binder_request(3, 11, 24),
    binder_request(3, 12, 24),
    BINDER_SET_CONTEXT_MGR_EXT,
    BINDER_FREEZE,
    binder_request(3, 15, 12),
    binder_request(1, 16, size_of::<u32>()),
    binder_request(3, 17, 12),
];

/// Whether `request` is one of binder's own `ioctl` requests.
pub(crate) fn is_binder_request(request: u64) -> bool {
    BINDER_REQUESTS.contains(&(request & 0xffff_ffff))
}

/// The name `include/uapi/linux/android/binder.h` gives `request`, one of
/// [`BINDER_REQUESTS`], for a refusal.
fn request_name(request: u64) -> &'static str {
    const NAMES: [&str; 12] = [
        "BINDER_WRITE_READ",
        "BINDER_SET_MAX_THREADS",
        "BINDER_SET_CONTEXT_MGR",
        "BINDER_THREAD_EXIT",
        "BINDER_VERSION",
        "BINDER_GET_NODE_DEBUG_INFO",
        "BINDER_GET_NODE_INFO_FOR_REF",
        "BINDER_SET_CONTEXT_MGR_EXT",
        "BINDER_FREEZE",
        "BINDER_GET_FROZEN_INFO",
        "BINDER_ENABLE_ONEWAY_SPAM_DETECTION",
        "BINDER_GET_EXTENDED_ERROR",
    ];
    BINDER_REQUESTS
        .iter()
        .position(|known| *known == request & 0xffff_ffff)
        .map_or("a binder request", |index| NAMES[index])
}

/// `--strict`'s binder policy (`Config::strict_binder`) for one `ioctl`
/// request: why the run must stop, or `None` for a request that is not
/// binder's.
///
/// A `--strict` run never holds a binder descriptor: one is refused where it
/// arrives (by an open, a receipt, or at the run's start), by the open file's
/// identity ([`identity`]). This request-word refusal is the backstop for a
/// descriptor whose identity is binder or cannot be told
/// (`Detcore::binder_entry`); a request on a descriptor known not to be a
/// binder device is left alone, as the same words name other drivers'
/// requests.
///
/// Every binder request is refused, in a fresh run and in a replay alike.
/// What a binder call returns, and when, is another process's to decide:
/// the transactions, replies, reference requests and notices a read takes
/// are bytes and timing from processes Hermit does not schedule, and Hermit
/// records none of them, so no run, fresh or replayed, can promise to repeat
/// a binder call until it does
/// (<https://github.com/rrnewton/hermit/issues/5124>). A replay's record of
/// where each backgrounded call rejoined fixes when the call is readmitted,
/// not what it read. The decision reads only the request word, so it comes
/// before Detcore waits for anything or reads the call's argument
/// (`Detcore::binder_entry`).
pub(crate) fn strict_refusal(request: u64) -> Option<String> {
    if !is_binder_request(request) {
        return None;
    }
    Some(format!(
        "--strict refuses binder ({}): binder reply payloads are not recorded, so no run, fresh \
         or replayed, can repeat what a binder call returns \
         (https://github.com/rrnewton/hermit/issues/5124). Run a guest that uses binder without \
         --strict, where each binder call that waits for another process is reported as \
         external input",
        request_name(request)
    ))
}

/// What `--strict` can tell of the file a guest descriptor refers to
/// ([`identity`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Identity {
    /// A binder device, described for a refusal.
    Binder(String),
    /// Not a binder device.
    NotBinder,
    /// Detcore cannot tell, and why.
    Unknown(String),
}

/// `BINDERFS_SUPER_MAGIC` (include/uapi/linux/magic.h:85 at v6.16): the file
/// system of every binderfs device, its `binder-control` included. Linux
/// tells a binderfs device by it (`is_binderfs_device`,
/// drivers/android/binderfs.c:86), and serves such a device from its inode,
/// not from its number, so a copy of the node elsewhere does not open binder.
const BINDERFS_SUPER_MAGIC: i64 = 0x6c6f_6f70;

/// The misc character devices' major number. Without binderfs, Linux
/// registers each binder device that binder's `devices` parameter names as
/// a misc device (`binder_init`, drivers/android/binder.c:7068 at v6.16, and
/// `init_binder_device`, :7029), which any node with its number opens.
const MISC_MAJOR: u32 = 10;

/// The misc minors no kernel hands out dynamically: binder asks for
/// `MISC_DYNAMIC_MINOR` (binder.c:7021), which Linux v6.16 allocates above
/// 255 (`misc_minor_alloc`, drivers/char/misc.c:65-75) and earlier kernels
/// allocated below 128. A minor in this range is one of the fixed
/// assignments of include/linux/miscdevice.h (`FUSE_MINOR` 229, `TUN_MINOR`
/// 200, `KVM_MINOR` 232, …), so its driver is known and is not binder.
const STATIC_MISC_MINORS: std::ops::RangeInclusive<u32> = 128..=254;

/// The modules a binder driver's `devices` parameter is read from: `binder`,
/// built in, and `binder_linux`, the name an out-of-tree build loads under,
/// with the same misc devices.
const BINDER_MODULES: [&str; 2] = ["binder", "binder_linux"];

/// The first binder driver's `devices` parameter `read` finds
/// (`/sys/module/<module>/parameters/devices` for each of
/// [`BINDER_MODULES`]), or `None`.
pub(crate) fn binder_devices_parameter(read: impl Fn(&str) -> Option<String>) -> Option<String> {
    BINDER_MODULES
        .iter()
        .find_map(|module| read(&format!("/sys/module/{module}/parameters/devices")))
}

/// Whether the file a descriptor refers to is a binder device, told by the
/// open file itself, never by a path:
/// - `stat`: its mode and device number (`fstat`), or `None` when it cannot
///   be read;
/// - `file_system`: its file system's magic (`fstatfs`), or `None`;
/// - `misc`: the contents of `/proc/misc`, which names each misc device by
///   its minor (`misc_seq_show`, drivers/char/misc.c:101 at v6.16), or
///   `None`;
/// - `devices`: the binder driver's `devices` parameter
///   ([`binder_devices_parameter`]), or `None` when none can be read.
///
/// A character device that is not a misc device is never binder's: binder
/// registers no other (/dev/null, /dev/urandom, terminals and
/// pseudo-terminals are not). Nor is a misc device with a fixed minor
/// ([`STATIC_MISC_MINORS`]). A misc device with a dynamic minor is binder's
/// when the driver's parameter names it; when `/proc/misc` or the parameter
/// cannot be read, or does not list the minor, it is unknown. Each lookup is
/// made only when the answer needs it.
pub(crate) fn identity(
    stat: Option<(u32, u64)>,
    file_system: impl FnOnce() -> Option<i64>,
    misc: impl FnOnce() -> Option<String>,
    devices: impl FnOnce() -> Option<String>,
) -> Identity {
    let Some((mode, rdev)) = stat else {
        return Identity::Unknown("its file cannot be inspected".into());
    };
    if mode & libc::S_IFMT != libc::S_IFCHR {
        return Identity::NotBinder;
    }
    match file_system() {
        Some(BINDERFS_SUPER_MAGIC) => return Identity::Binder("a binderfs device".into()),
        Some(_) => {}
        None => return Identity::Unknown("its file system cannot be inspected".into()),
    }
    let (major, minor) = (libc::major(rdev), libc::minor(rdev));
    if major != MISC_MAJOR || STATIC_MISC_MINORS.contains(&minor) {
        return Identity::NotBinder;
    }
    let Some(misc) = misc() else {
        return Identity::Unknown("/proc/misc cannot be read".into());
    };
    let Some(name) = misc.lines().find_map(|line| {
        let (number, name) = line.trim_start().split_once(' ')?;
        (number.parse::<u32>().ok()? == minor).then(|| name.trim().to_owned())
    }) else {
        return Identity::Unknown(format!(
            "/proc/misc does not list misc device {major}:{minor}"
        ));
    };
    match devices() {
        Some(devices) if devices.trim().split(',').any(|device| device == name) => {
            Identity::Binder(format!(
                "the binder device {name} (misc device {major}:{minor})"
            ))
        }
        Some(_) => Identity::NotBinder,
        None => Identity::Unknown(format!(
            "misc device {name} ({major}:{minor}) has a dynamic minor, and no binder driver's \
             devices parameter can be read"
        )),
    }
}

/// [`identity`] of the file `fd_link`, a `/proc/<tid>/fd/<n>` entry, refers
/// to, from host procfs and sysfs.
pub(crate) fn identity_of(fd_link: &std::path::Path) -> Identity {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;
    let stat = std::fs::metadata(fd_link)
        .ok()
        .map(|metadata| (metadata.mode(), metadata.rdev()));
    identity(
        stat,
        || {
            let path = std::ffi::CString::new(fd_link.as_os_str().as_bytes()).ok()?;
            // SAFETY: a zeroed `statfs` is a valid value of the plain C struct.
            let mut file_system: libc::statfs = unsafe { std::mem::zeroed() };
            // SAFETY: a NUL-terminated path and a buffer this frame owns.
            let status = unsafe { libc::statfs(path.as_ptr(), &mut file_system) };
            (status == 0).then_some(file_system.f_type as i64)
        },
        || std::fs::read_to_string("/proc/misc").ok(),
        || binder_devices_parameter(|path| std::fs::read_to_string(path).ok()),
    )
}

/// Why a `--strict` run stops at descriptor `fd`, which the guest got `how`
/// ("opened", "received through SCM_RIGHTS", …) and whose identity is
/// `identity`: a binder device, or one Detcore cannot tell is not. `None`
/// for a descriptor known not to be a binder device.
pub(crate) fn strict_descriptor_refusal(
    fd: RawFd,
    how: &str,
    identity: &Identity,
) -> Option<String> {
    let what = match identity {
        Identity::Binder(what) => format!("is {what}"),
        Identity::Unknown(why) => {
            format!("may be a binder device: its identity could not be read ({why})")
        }
        Identity::NotBinder => return None,
    };
    Some(format!(
        "--strict refuses binder: descriptor {fd}, {how}, {what}. A --strict run holds no binder \
         descriptor: binder reply payloads are not recorded, so no run, fresh or replayed, can \
         repeat what a binder call returns (https://github.com/rrnewton/hermit/issues/5124). Run \
         a guest that uses binder without --strict"
    ))
}

/// The refusal ([`strict_descriptor_refusal`]) of the first of `arrived`,
/// descriptors a call gave the guest `how`, that `identify` does not know to
/// be other than a binder device, or `None`.
pub(crate) fn strict_arrival_refusal(
    arrived: &[RawFd],
    how: &str,
    mut identify: impl FnMut(RawFd) -> Identity,
) -> Option<String> {
    arrived
        .iter()
        .find_map(|&fd| strict_descriptor_refusal(fd, how, &identify(fd)))
}

/// The first descriptor listed in `fd_directory`, a `/proc/<tid>/fd`
/// directory, that `identify` does not know to be other than a binder
/// device, passing over those `skip` names: its number and identity. A
/// directory that cannot be listed is an error, never a pass. Listed with
/// [`crate::util::find_in_directory`], which the in-guest backends can run.
pub(crate) fn held_binder_descriptor(
    fd_directory: &std::path::Path,
    skip: impl Fn(RawFd) -> bool,
    identify: impl Fn(&std::path::Path) -> Identity,
) -> std::io::Result<Option<(RawFd, Identity)>> {
    crate::util::find_in_directory(fd_directory, |name| {
        let fd: RawFd = name.to_str()?.parse().ok()?;
        if skip(fd) {
            return None;
        }
        match identify(&fd_directory.join(name)) {
            Identity::NotBinder => None,
            identity => Some((fd, identity)),
        }
    })
}

/// What the binder driver's `devices` parameter is on this host
/// ([`binder_devices_parameter_state`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DevicesParameter {
    /// The parameters of the [`BINDER_MODULES`] that have one, joined by
    /// commas.
    Present(String),
    /// None of [`BINDER_MODULES`] has one. That proves there is no binder
    /// driver only where `/sys/module` is visible and lists none of them
    /// ([`sys_module_binder_modules`]):
    /// binder declares the parameter readable (`module_param_named(devices,
    /// …, 0444)`, drivers/android/binder.c:121 at v6.16), so Linux lists it
    /// under `/sys/module/binder/parameters/` whether binder is built in
    /// (`param_sysfs_builtin`, kernel/params.c:829) or a module.
    Missing,
    /// One exists but cannot be read, and why.
    Unreadable(String),
}

/// `SYSFS_MAGIC` (include/uapi/linux/magic.h:90 at v6.16).
const SYSFS_MAGIC: i64 = 0x6265_6572;

/// Which of [`BINDER_MODULES`] the kernel's own module directory lists, as
/// `file_system` (the `statfs` magic of `/sys/module`) and `listing` (every
/// entry it lists, [`directory_entries`]) say; or why `/sys/module` is not
/// that directory: a missing, unmounted or unreadable `/sys`, or a directory
/// of another file system standing in for it.
pub(crate) fn sys_module_binder_modules(
    file_system: std::io::Result<i64>,
    listing: std::io::Result<Vec<String>>,
) -> Result<Vec<&'static str>, String> {
    match file_system {
        Ok(SYSFS_MAGIC) => {}
        Ok(magic) => return Err(format!("/sys/module is not sysfs (file system {magic:#x})")),
        Err(error) => return Err(format!("/sys/module cannot be inspected ({error})")),
    }
    let names = listing.map_err(|error| format!("/sys/module cannot be listed ({error})"))?;
    Ok(BINDER_MODULES
        .into_iter()
        .filter(|module| names.iter().any(|name| name == module))
        .collect())
}

/// The name of every entry `directory` lists, "." and ".." aside, read with
/// the raw-syscall walker ([`crate::util::find_in_directory`]) to the end.
pub(crate) fn directory_entries(directory: &std::path::Path) -> std::io::Result<Vec<String>> {
    let mut names = Vec::new();
    crate::util::find_in_directory(directory, |name| {
        if name != "." && name != ".." {
            names.push(name.to_string_lossy().into_owned());
        }
        None::<()>
    })?;
    Ok(names)
}

/// The binder driver's `devices` parameter, from
/// `/sys/module/<module>/parameters/devices` for each of [`BINDER_MODULES`]
/// as `read` reads it. A parameter that exists but cannot be read is
/// [`DevicesParameter::Unreadable`] whatever the others say; one that is
/// absent (`NotFound`) is passed over.
pub(crate) fn binder_devices_parameter_state(
    read: impl Fn(&str) -> std::io::Result<String>,
) -> DevicesParameter {
    let mut present = Vec::new();
    for module in BINDER_MODULES {
        let path = format!("/sys/module/{module}/parameters/devices");
        match read(&path) {
            Ok(devices) => present.push(devices.trim().to_owned()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return DevicesParameter::Unreadable(format!("{path}: {error}")),
        }
    }
    if present.is_empty() {
        DevicesParameter::Missing
    } else {
        DevicesParameter::Present(present.join(","))
    }
}

/// Why `--strict` refuses a run on a backend whose guest descriptors are not
/// host descriptors Detcore can name (KVM) on this host, or `None`.
///
/// There no descriptor's identity can be read where it arrives or at the
/// start (<https://github.com/rrnewton/hermit/issues/5129>). So, as a
/// temporary conservative policy, strict KVM is refused on any host that
/// offers binder in any form, whether or not the guest would ever hold a
/// binder descriptor. The run is refused when:
/// - `filesystems`, the contents of `/proc/filesystems`, lists binderfs.
///   That a guest could reach a binderfs instance (one it mounts, or another
///   mount namespace's through a `/proc/<pid>/root` alias) is not checked:
///   the registration alone refuses;
/// - `misc`, the contents of `/proc/misc`, lists a misc device whose name
///   contains "binder", or one that the binder driver's `devices` parameter
///   names (`devices`, [`binder_devices_parameter_state`]). The first does
///   not depend on the parameter, which a binder module loaded under another
///   name would not have;
/// - the binder driver's `devices` parameter exists but cannot be read;
/// - `sys_module`, which of [`BINDER_MODULES`] `/sys/module` lists
///   ([`sys_module_binder_modules`]), is not `Ok`: there a missing parameter
///   would not prove that there is no binder driver, which a misc device
///   named neither like binder's nor by a parameter Hermit can read would
///   let through;
/// - `/sys/module` lists one of [`BINDER_MODULES`] whose `devices` parameter
///   is missing;
/// - `held`, the first of Hermit's own descriptors, which the guest inherits,
///   that is not known to be other than a binder device
///   ([`held_binder_descriptor`]), is not `None`;
/// - or any of `filesystems`, `misc` or Hermit's descriptor table cannot be
///   read.
///
/// A host with none of these is admitted: no binderfs registered, no misc
/// device named like binder's or named by a binder driver, a visible
/// `/sys/module`, and no binder descriptor handed to the guest. One binder
/// device this lets through remains, on KVM only: one a binder module loaded
/// under a name other than [`BINDER_MODULES`] registers under a name without
/// "binder" in it (ptrace refuses it as of unknown identity). Narrowing the
/// policy to the binder objects a guest can actually reach needs their
/// identity through the guest, which also accounts for `/proc/<pid>/root`
/// aliases and closes that remainder: #5129's follow-up.
///
/// The check trusts host files as the kernel presents them:
/// `/proc/filesystems`, `/proc/misc`, `/sys/module` and Hermit's own
/// descriptor table alike. A host that deliberately mounts a substitute
/// over any of them (another sysfs subtree bound over `/sys/module`, say)
/// defeats them all equally. That is outside `--strict`'s threat model,
/// which guards a run against outside input, not against its host: the
/// guest cannot mount. Where these files come from belongs to the same
/// object-identity follow-up.
pub(crate) fn kvm_strict_binder_refusal(
    filesystems: Option<&str>,
    misc: Option<&str>,
    devices: &DevicesParameter,
    sys_module: Result<Vec<&'static str>, String>,
    held: std::io::Result<Option<(RawFd, Identity)>>,
) -> Option<String> {
    let reason = 'reason: {
        let Some(filesystems) = filesystems else {
            break 'reason "/proc/filesystems cannot be read, so it cannot tell whether the host \
                           kernel offers binderfs"
                .to_owned();
        };
        if filesystems
            .lines()
            .any(|line| line.split_whitespace().last() == Some("binder"))
        {
            break 'reason "the host kernel offers binderfs (/proc/filesystems lists binder)"
                .to_owned();
        }
        let Some(misc) = misc else {
            break 'reason "/proc/misc cannot be read, so it cannot tell whether the host kernel \
                           has a binder device"
                .to_owned();
        };
        let named: Vec<&str> = match devices {
            DevicesParameter::Present(devices) => devices.split(',').map(str::trim).collect(),
            DevicesParameter::Missing | DevicesParameter::Unreadable(_) => Vec::new(),
        };
        if let Some(line) = misc.lines().find(|line| {
            line.trim_start().split_once(' ').is_some_and(|(_, name)| {
                let name = name.trim();
                name.contains("binder") || named.contains(&name)
            })
        }) {
            break 'reason format!(
                "the host kernel has a binder device (/proc/misc: \"{}\")",
                line.trim()
            );
        }
        if let DevicesParameter::Unreadable(why) = devices {
            break 'reason format!(
                "the binder driver's devices parameter cannot be read ({why}), so it cannot tell \
                 which misc devices are binder's"
            );
        }
        match (sys_module, devices) {
            (Err(why), _) => {
                break 'reason format!(
                    "{why}, so the absence of a binder driver's devices parameter does not \
                     prove that the host kernel has no binder driver"
                );
            }
            (Ok(listed), DevicesParameter::Missing) if !listed.is_empty() => {
                break 'reason format!(
                    "/sys/module lists the binder module ({}) but its devices parameter is \
                     missing, so it cannot tell which misc devices are binder's",
                    listed.join(", ")
                );
            }
            (Ok(_), _) => {}
        }
        match held {
            Ok(None) => return None,
            Ok(Some((fd, Identity::Binder(what)))) => {
                format!("Hermit's descriptor {fd}, which the guest inherits, is {what}")
            }
            Ok(Some((fd, Identity::Unknown(why)))) => format!(
                "Hermit's descriptor {fd}, which the guest inherits, may be a binder device: its \
                 identity could not be read ({why})"
            ),
            Ok(Some((_, Identity::NotBinder))) => return None,
            Err(error) => {
                format!(
                    "Hermit's own descriptors, which the guest inherits, cannot be listed ({error})"
                )
            }
        }
    };
    Some(format!(
        "--strict refuses binder: {reason}. Under the KVM backend Hermit cannot tell whether a \
         guest descriptor is a binder device (https://github.com/rrnewton/hermit/issues/5129), and \
         binder reply payloads are not recorded \
         (https://github.com/rrnewton/hermit/issues/5124), so, for now, a strict run is admitted \
         only on a host that offers binder in no form. Use the ptrace backend, or run without \
         --strict"
    ))
}

/// [`kvm_strict_binder_refusal`] for a `--strict` run whose backend's guest
/// descriptors are not host descriptors Detcore can name
/// (`procfs_inode::tool_can_name_guest_descriptors`), from host procfs,
/// sysfs and the calling process's own descriptor table, which the guest
/// inherits; `None` for any other run. Called before the run starts
/// (`Detcore::facility_admission`).
pub(crate) fn kvm_strict_binder_admission(
    config: &detcore_model::config::Config,
) -> Option<String> {
    kvm_strict_binder_admission_with(
        config,
        |path| std::fs::read_to_string(path),
        host_sys_module_binder_modules,
        || {
            held_binder_descriptor(
                std::path::Path::new("/proc/self/fd"),
                |_| false,
                identity_of,
            )
        },
    )
}

/// [`kvm_strict_binder_admission`] with the host's files read by `read` and
/// the calling process's descriptors checked by `held`, neither used unless
/// the run is one the check is for.
fn kvm_strict_binder_admission_with(
    config: &detcore_model::config::Config,
    read: impl Fn(&str) -> std::io::Result<String>,
    sys_module: impl FnOnce() -> Result<Vec<&'static str>, String>,
    held: impl FnOnce() -> std::io::Result<Option<(RawFd, Identity)>>,
) -> Option<String> {
    if !config.strict_binder
        || crate::procfs_inode::tool_can_name_guest_descriptors(&config.backend)
    {
        return None;
    }
    let filesystems = read("/proc/filesystems").ok();
    let misc = read("/proc/misc").ok();
    let devices = binder_devices_parameter_state(&read);
    kvm_strict_binder_refusal(
        filesystems.as_deref(),
        misc.as_deref(),
        &devices,
        sys_module(),
        held(),
    )
}

/// [`sys_module_binder_modules`] of the host's `/sys/module`, from `statfs`
/// and a listing of every entry.
fn host_sys_module_binder_modules() -> Result<Vec<&'static str>, String> {
    let file_system = {
        // SAFETY: a zeroed `statfs` is a valid value of the plain C struct.
        let mut file_system: libc::statfs = unsafe { std::mem::zeroed() };
        // SAFETY: a NUL-terminated path and a buffer this frame owns.
        let status = unsafe { libc::statfs(c"/sys/module".as_ptr(), &mut file_system) };
        if status == 0 {
            Ok(file_system.f_type as i64)
        } else {
            Err(std::io::Error::last_os_error())
        }
    };
    let listing = directory_entries(std::path::Path::new("/sys/module"));
    sys_module_binder_modules(file_system, listing)
}

/// Whether `request`, an `ioctl` request word, is `BINDER_WRITE_READ`.
/// Only the low 32 bits are the request: the kernel's `cmd` is an
/// `unsigned int`.
pub(crate) fn is_write_read(request: u64) -> bool {
    request & 0xffff_ffff == BINDER_WRITE_READ
}

/// Why a binder `ioctl` waits in the kernel until another process acts, so
/// that Detcore runs it as a blocking external call rather than in the
/// caller's turn. `None` for every request that returns on its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Blocking {
    /// A `BINDER_WRITE_READ` whose read half waits for work for this thread:
    /// a transaction, a reply, or a death notification
    /// (`binder_thread_read` -> `binder_wait_for_work`).
    Read,
    /// A `BINDER_FREEZE` that waits for another process's outstanding
    /// transactions to drain, up to its own timeout
    /// (`binder_ioctl_freeze`).
    Freeze,
}

/// What a binder call's argument held, read from guest memory once, after
/// the exit-report barrier (`Detcore::binder_snapshot`): the
/// `binder_write_read` of a `BINDER_WRITE_READ` and the `binder_freeze_info`
/// of a `BINDER_FREEZE`, `None` where the request has none or it cannot be
/// read. Every decision Detcore makes before the call (its route, its poll,
/// its refusals) and the `read_consumed` its receipts parse from after it
/// come from this one read, so none reads the argument twice or before the
/// barrier.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Snapshot {
    pub(crate) write_read: Option<WriteRead>,
    pub(crate) freeze: Option<[u32; 3]>,
}

impl Snapshot {
    /// Whether `request` has an argument Detcore reads:
    /// `BINDER_WRITE_READ` and `BINDER_FREEZE`.
    pub(crate) fn reads(request: u64) -> bool {
        is_write_read(request) || request & 0xffff_ffff == BINDER_FREEZE
    }

    /// Read the argument of binder `request` at `argument`.
    pub(crate) fn read<M: MemoryAccess>(memory: &M, request: u64, argument: usize) -> Self {
        Snapshot {
            write_read: is_write_read(request)
                .then(|| write_read(memory, argument))
                .flatten(),
            freeze: (request & 0xffff_ffff == BINDER_FREEZE)
                .then(|| freeze_info(memory, argument))
                .flatten(),
        }
    }

    /// The `read_consumed` the call was given: the bytes of `read_buffer` an
    /// earlier call wrote, which this one leaves alone. Zero where there is
    /// no `binder_write_read` or it could not be read, so the whole buffer is
    /// parsed, as before.
    pub(crate) fn consumed_before(&self) -> u64 {
        self.write_read.map_or(0, |call| call.read_consumed)
    }
}

/// `WriteRead` of the call whose argument is at `argument`, read from guest
/// memory; `None` when it cannot be read.
fn write_read<M: MemoryAccess>(memory: &M, argument: usize) -> Option<WriteRead> {
    let address = Addr::<WriteRead>::from_raw(argument)?;
    memory.read_value(address).ok()
}

impl WriteRead {
    /// Whether this call has a read half, which is the half that waits.
    /// Linux runs `binder_thread_read` whenever `read_size` is nonzero
    /// (`binder_ioctl_write_read`, drivers/android/binder.c:5073 at v6.4,
    /// <https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/drivers/android/binder.c?h=v6.4#n5073>),
    /// and that read waits for work (`binder_wait_for_work`, :4412) before it
    /// looks at how much of the buffer is left (:4445). A buffer the caller
    /// has already filled (`read_consumed == read_size`) therefore waits as
    /// long as an empty one, and then returns with nothing added.
    pub(crate) fn reads(&self) -> bool {
        self.read_size > 0
    }

    /// Whether this call has a write half left to run: `binder_thread_write`
    /// runs the commands of `write_buffer[write_consumed..write_size]`
    /// (drivers/android/binder.c:3804-3807 at v6.4), before the read half and
    /// in the same call (:5060-5071).
    pub(crate) fn writes(&self) -> bool {
        self.write_size > self.write_consumed
    }
}

/// Why `request` waits in the kernel, from the call's argument already read
/// from guest memory: `None` when it returns on its own. `nonblocking` is the
/// descriptor's `O_NONBLOCK`, which makes a read return `EAGAIN` instead of
/// waiting (`binder_thread_read`, drivers/android/binder.c:4408-4410 at
/// v6.4). A read is classified as waiting whenever it has a read half
/// ([`WriteRead::reads`]); the caller may then find work already queued and
/// run it in the caller's turn anyway ([`route`]).
pub(crate) fn blocking(
    request: u64,
    write_read: Option<&WriteRead>,
    freeze: Option<&[u32; 3]>,
    nonblocking: bool,
) -> Option<Blocking> {
    match request & 0xffff_ffff {
        BINDER_WRITE_READ if !nonblocking => write_read
            .filter(|call| call.reads())
            .map(|_| Blocking::Read),
        BINDER_FREEZE => freeze
            .filter(|info| info[1] != 0 && info[2] != 0)
            .map(|_| Blocking::Freeze),
        _ => None,
    }
}

/// What a binder call that waits does besides waiting, which the background
/// path would run at a moment the host chooses rather than one the schedule
/// orders against the guest's other threads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Effect {
    /// The write half of a `BINDER_WRITE_READ` that also reads: the
    /// transactions, replies and commands it sends, which Linux runs first,
    /// in the same call (`binder_ioctl_write_read`).
    Write,
    /// A `BINDER_FREEZE`: Linux marks the target frozen as the call starts
    /// and then waits for its transactions to drain (`binder_ioctl_freeze`,
    /// drivers/android/binder.c:5249-5256 at v6.4).
    Freeze,
}

/// How Detcore issues a binder `ioctl` (`Detcore::run_binder_call_with`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Route {
    /// It returns on its own, so it runs in the caller's turn.
    InTurn,
    /// A read and nothing else, which waits only when no work is queued for
    /// its thread. A zero-timeout poll in the caller's turn asks first
    /// (`binder_poll` asks what this read's own wait asks, whether the thread
    /// has work it may take: drivers/android/binder.c:5017-5040 against
    /// :4387-4412 at v6.4): with work the read keeps the turn, otherwise it
    /// runs on the background path. No half of the call runs before the
    /// read, so the answer holds for it, with one host-timed exception: work
    /// queued for the whole process (not for this thread) that a sibling
    /// thread, whose own read entered the kernel late on the background path,
    /// takes first. Work that arrives while a sibling already waits goes to
    /// that sibling's own queue (`binder_proc_transaction`, :2785), so this needs the
    /// sibling's entry to trail its turn.
    PollFirst,
    /// It waits and has an effect besides waiting. The caller's readiness
    /// says nothing about whether it waits: a write half can make the thread
    /// unavailable for the process work the poll saw (a synchronous
    /// `BC_TRANSACTION` pushes its transaction stack and defers its own
    /// completion, drivers/android/binder.c:3616-3619 and :559-564 at v6.4),
    /// and a freeze waits on another process's transactions, not on this
    /// thread's work. It never polls, and its effect would run off the turn
    /// on the background path, so the run must allow that
    /// (`Detcore::admit_host_timed_binder_call`).
    HostTimed(Blocking, Effect),
}

/// How Detcore issues `request` ([`Route`]), from the call's argument already
/// read from guest memory, as [`blocking`] reads it.
pub(crate) fn route(
    request: u64,
    write_read: Option<&WriteRead>,
    freeze: Option<&[u32; 3]>,
    nonblocking: bool,
) -> Route {
    match blocking(request, write_read, freeze, nonblocking) {
        None => Route::InTurn,
        Some(Blocking::Freeze) => Route::HostTimed(Blocking::Freeze, Effect::Freeze),
        Some(Blocking::Read) if write_read.is_some_and(WriteRead::writes) => {
            Route::HostTimed(Blocking::Read, Effect::Write)
        }
        Some(Blocking::Read) => Route::PollFirst,
    }
}

/// Why a binder call on descriptor `fd` that waits and has `effect` cannot be
/// ordered by the schedule: the policy's reason, for a refusal or a recorded
/// determinism loss (`Detcore::admit_host_timed_binder_call`).
pub(crate) fn host_timed_reason(effect: Effect, fd: RawFd) -> String {
    let (call, what) = match effect {
        Effect::Write => (
            "ioctl(BINDER_WRITE_READ)",
            "both writes and waits to read, so its write half (the transactions, replies and \
             commands it sends) would run",
        ),
        Effect::Freeze => (
            "ioctl(BINDER_FREEZE)",
            "waits for its target's transactions to drain, so the freeze would take effect",
        ),
    };
    format!(
        "{call} on descriptor {fd} {what} on the background path, at a moment this host chooses \
         rather than one the schedule orders against the guest's other threads"
    )
}

/// How a binder call ran (`Detcore::run_binder_call_with`), which decides how
/// the descriptors its read delivers are taken
/// (`Detcore::take_binder_receipts`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BinderRun {
    /// In the caller's turn.
    InTurn,
    /// Off the turn, on the background path, by the run's one task: no other
    /// task existed to see the descriptors its read delivers, or change the
    /// call or what it returns, before the task resumed, so they are recorded
    /// as in the turn.
    OffTurnAlone,
    /// Off the turn while the run had another task, or a debugger
    /// (`Detcore::binder_other_tasks`), which only a run that allows
    /// unsupported operations lets happen: the descriptors it delivers are
    /// not recorded, and the run records the loss.
    OffTurnShared,
}

/// Why a binder call on descriptor `fd` that waits off the guest's turn
/// cannot be followed by Detcore while `others` (the run's other tasks, or a
/// debugger it serves) can act: the policy's reason, for a refusal or a
/// recorded determinism loss (`Detcore::admit_shared_binder_call`). Linux
/// copies the call's argument in when the call starts, not when Detcore
/// classified it (`binder_ioctl_write_read`, drivers/android/binder.c:5050 at
/// v6.4), installs each descriptor a read delivers in the caller's table
/// before the call returns (`binder_apply_fd_fixups`, :4357 and :4666), and
/// gives a transaction to whichever thread of the binder process waits first
/// (`binder_select_thread_ilocked`, :2785): another task could change the
/// call into one Detcore did not admit, use, close or replace those
/// descriptors, rewrite what the read returned, or take the work the call
/// waits for, before Detcore accounts for it.
pub(crate) fn shared_task_reason(fd: RawFd, others: &str) -> String {
    format!(
        "a binder call on descriptor {fd} waits for another process, so it would run off the \
         guest's turn, which Hermit allows only while the caller is the run's one task, but \
         {others}: Linux reads the call's argument when the call starts and installs each \
         descriptor a read delivers in the caller's table before the call returns, so another \
         task could change the call, use, close or replace those descriptors, or take the work \
         it waits for, before Hermit accounts for them"
    )
}

/// Why the descriptors a binder read on descriptor `fd` delivered off the
/// turn beside another task ([`BinderRun::OffTurnShared`]) go unrecorded, or
/// why one that ran in the turn or alone off it cannot have them recorded
/// (Detcore cannot name them), or `None` when it delivered none. `ran` says
/// where the call ran.
pub(crate) fn unrecorded_receipt_reason(
    fd: RawFd,
    ran: BinderRun,
    delivered: &Delivered,
) -> Option<String> {
    let what = match delivered.refused {
        Some(why) => format!("may have delivered descriptors Detcore cannot name ({why})"),
        None if delivered.fds.is_empty() => return None,
        None => format!("delivered descriptors {:?}", delivered.fds),
    };
    let route = match ran {
        BinderRun::InTurn => "ran in the guest's turn",
        BinderRun::OffTurnAlone | BinderRun::OffTurnShared => "ran off the guest's turn",
    };
    Some(format!(
        "ioctl(BINDER_WRITE_READ) on descriptor {fd} {route} and {what}, which Detcore does not \
         record"
    ))
}

/// Why a fail-closed run stops at a `BINDER_WRITE_READ` on `fd` with a read
/// half (`read_size` > 0) that failed with `EFAULT`: Linux writes the read's
/// return commands, and installs the descriptors they carry, before it copies
/// the header back, and undoes neither when that copy, or a later write to
/// the read buffer, faults (`binder_ioctl_write_read`,
/// drivers/android/binder.c:5410-5423 and :5431-5433 at v6.16;
/// `binder_thread_read` returns `-EFAULT` mid-buffer without updating
/// `read_consumed`, :4750 to :5048). The header then does not say how much
/// was written, so Detcore cannot record the descriptors, and the guest's
/// first use of one would fail with `EBADF`, an answer Linux never gives.
/// Any other error of the read half comes from its wait, after at most a
/// `BR_NOOP` (:4667, :4702), and delivers nothing.
pub(crate) fn faulted_read_reason(fd: RawFd) -> String {
    format!(
        "ioctl(BINDER_WRITE_READ) on descriptor {fd} failed with EFAULT, and its read half may \
         have had effects: Linux does not undo the return commands it wrote, or the descriptors \
         it installed, before a fault, and the call's binder_write_read does not say how much it \
         wrote, so Detcore cannot record them. --allow-unsupported-syscalls lets the guest see \
         the EFAULT"
    )
}

/// `struct binder_freeze_info` at `argument`, read from guest memory.
fn freeze_info<M: MemoryAccess>(memory: &M, argument: usize) -> Option<[u32; 3]> {
    let address = Addr::<[u32; 3]>::from_raw(argument)?;
    memory.read_value(address).ok()
}

/// Count a binder call that runs as external input, and warn once per guest
/// process (`process`, its thread-group id, which only keys the warning: the
/// line does not print it, so it reads the same whatever IDs the run has)
/// that its binder calls are: their
/// results come from processes Hermit does not schedule, so two runs can
/// differ wherever those processes do. The run's summary reports the count
/// (`external_calls`).
///
/// Returns whether it warned. Which calls go to the background is decided by
/// host timing (`run_binder_call_with`'s zero-timeout poll), so whether a process
/// warns at all, and the count, can differ between runs; the line's text
/// cannot.
pub(crate) fn note_blocking(waits: Blocking, process: i32) -> bool {
    EXTERNAL_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    static WARNED: std::sync::Mutex<std::collections::BTreeSet<i32>> =
        std::sync::Mutex::new(std::collections::BTreeSet::new());
    let first = WARNED.lock().unwrap().insert(process);
    if first {
        let call = match waits {
            Blocking::Read => "a binder read (BINDER_WRITE_READ)",
            Blocking::Freeze => "a binder freeze (BINDER_FREEZE)",
        };
        tracing::warn!(
            "[detcore] HERMIT_EXTERNAL_INPUT source=binder: {call} waits for another process, \
             which Hermit does not schedule. It runs as a blocking external call, so when it \
             finishes, and what it reads, are this host's, not the schedule's (warned once per \
             guest process; the run summary counts every such call)"
        );
    }
    first
}

/// Emit the post-grant receipt for a waiting binder call: the scheduler
/// has granted its `BlockingExternalIO` request and the native call is
/// about to run. This is the first point that proves admission — the
/// earlier `HERMIT_EXTERNAL_INPUT` warning only proves the off-turn route
/// was chosen, before the request was even submitted. The receipt names
/// the operation (`ExternalOpId`: thread and per-thread syscall count),
/// so an outside observer can correlate it with the recorded release.
pub(crate) fn note_blocking_granted(op_id: crate::resources::ExternalOpId) {
    GRANTED_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    GRANTED_OPS.lock().unwrap().push(op_id);
    tracing::warn!(
        "[detcore] HERMIT_BINDER_BLOCKING_GRANTED source=binder dettid={} seq={}: \
         the blocking grant is received and the native call follows",
        op_id.tid.as_raw(),
        op_id.sequence,
    );
}

/// Waiting binder calls whose blocking grant was received
/// (`note_blocking_granted`), by operation, so a test can follow its
/// own call past other calls in the same process.
static GRANTED_CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static GRANTED_OPS: std::sync::Mutex<Vec<crate::resources::ExternalOpId>> =
    std::sync::Mutex::new(Vec::new());

/// How many post-grant binder receipts this process has emitted.
pub(crate) fn blocking_granted_calls() -> u64 {
    GRANTED_CALLS.load(std::sync::atomic::Ordering::Relaxed)
}

/// Whether the post-grant receipt for `op_id` has been emitted, for
/// tests that hold a grant pending and prove none is emitted early.
pub(crate) fn blocking_granted(op_id: crate::resources::ExternalOpId) -> bool {
    GRANTED_OPS.lock().unwrap().contains(&op_id)
}

/// Binder calls that ran as external input (`note_blocking`).
static EXTERNAL_CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// How many binder calls this run has run as external input so far, for the
/// run summary. Counted where Detcore handles the guest's system calls, which
/// on ptrace, the backend binder guests run on, is the process that makes the
/// summary; another backend reports none.
pub(crate) fn external_calls() -> u64 {
    EXTERNAL_CALLS.load(std::sync::atomic::Ordering::Relaxed)
}

/// The descriptors a successful `BINDER_WRITE_READ`, whose
/// `struct binder_write_read` is at `argument`, made Linux install in the
/// caller, read from guest memory after the call. `already_consumed` is the
/// `read_consumed` the caller passed in, read before the call: Linux writes
/// this call's commands after it.
pub(crate) fn delivered<M: MemoryAccess>(
    memory: &M,
    argument: usize,
    already_consumed: u64,
) -> Delivered {
    let Some(address) = Addr::<[u64; 6]>::from_raw(argument) else {
        return refused("its binder_write_read is at address 0");
    };
    let Ok(words) = memory.read_value(address) else {
        return refused("its binder_write_read cannot be read");
    };
    // write_size, write_consumed, write_buffer, read_size, read_consumed,
    // read_buffer.
    let [_, _, _, read_size, read_consumed, read_buffer] = words;
    if read_consumed == 0 || read_consumed == already_consumed {
        return Delivered::default();
    }
    if read_consumed > read_size || already_consumed > read_consumed {
        return refused("its read_consumed is not within its read_size");
    }
    // Linux writes this call's commands after the read_consumed the caller
    // passed in, leaving any earlier ones alone: those are an older call's,
    // and their buffers may already be freed.
    let newly_consumed = read_consumed - already_consumed;
    if newly_consumed > MAX_COMMAND_BYTES as u64 {
        return refused("its newly returned commands exceed Detcore's read limit");
    }
    let length = newly_consumed as usize;
    let Some(address) = Addr::<u8>::from_raw((read_buffer + already_consumed) as usize) else {
        return refused("its read_buffer is at address 0");
    };
    let mut commands = vec![0; length];
    if memory.read_exact(address, &mut commands).is_err() {
        return refused("its return commands cannot be read");
    }
    match installed_descriptors(memory, &commands) {
        Ok(fds) => Delivered {
            // One transaction: its descriptors arrive as one message.
            messages: vec![crate::syscalls::scm_rights::DeliveredMessage {
                count: fds.len(),
                truncated: false,
                bytes: None,
            }],
            fds,
            refused: None,
            may_have_installed: false,
        },
        Err(why) => refused(why),
    }
}

fn refused(why: &'static str) -> Delivered {
    Delivered {
        fds: Vec::new(),
        messages: Vec::new(),
        refused: Some(why),
        // A transaction Detcore cannot read may have carried descriptors.
        may_have_installed: true,
    }
}

/// `_IOC_SIZE(cmd)`: the payload size a return command carries after its
/// 32-bit code.
fn payload_size(cmd: u32) -> usize {
    ((cmd >> 16) & 0x3fff) as usize
}

fn u32_at(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_ne_bytes(
        bytes.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

fn u64_at(bytes: &[u8], offset: usize) -> Option<u64> {
    Some(u64::from_ne_bytes(
        bytes.get(offset..offset + 8)?.try_into().ok()?,
    ))
}

/// The descriptors the transactions in `commands`, a run of return commands
/// as Linux wrote them, carry, in order.
fn installed_descriptors<M: MemoryAccess>(
    memory: &M,
    commands: &[u8],
) -> Result<Vec<RawFd>, &'static str> {
    let mut fds = Vec::new();
    let mut offset = 0;
    while offset < commands.len() {
        let cmd = u32_at(commands, offset).ok_or("a return command is cut short")?;
        let payload = commands
            .get(offset + 4..offset + 4 + payload_size(cmd))
            .ok_or("a return command's payload is cut short")?;
        if matches!(cmd, BR_TRANSACTION | BR_TRANSACTION_SEC_CTX | BR_REPLY) {
            fds.extend(transaction_descriptors(memory, payload)?);
        }
        offset += 4 + payload.len();
    }
    Ok(fds)
}

/// The descriptors the transaction `data`, a `struct binder_transaction_data`
/// (the head of a `_secctx` one too), carries.
fn transaction_descriptors<M: MemoryAccess>(
    memory: &M,
    data: &[u8],
) -> Result<Vec<RawFd>, &'static str> {
    let short = "a transaction is cut short";
    // The driver never reads TF_STATUS_CODE when it installs descriptors, so
    // neither does this: a real status reply carries no objects anyway, and
    // skipping on the flag would hide a descriptor Linux did install.
    let offsets_size = u64_at(data, 40).ok_or(short)? as usize;
    let buffer = u64_at(data, 48).ok_or(short)? as usize;
    let offsets_at = u64_at(data, 56).ok_or(short)? as usize;
    let count = offsets_size / 8;
    if count == 0 {
        return Ok(Vec::new());
    }
    if count > MAX_OBJECTS {
        return Err("a transaction carries more objects than Detcore reads");
    }
    let mut offsets = vec![0u64; count];
    let address = Addr::<u64>::from_raw(offsets_at).ok_or("a transaction's offsets are at 0")?;
    memory
        .read_values(address, &mut offsets)
        .map_err(|_| "a transaction's offsets cannot be read")?;
    let object = |index: usize, length: usize| -> Result<Vec<u8>, &'static str> {
        let at = offsets
            .get(index)
            .ok_or("an object names a parent that is not in the transaction")?;
        let address = Addr::<u8>::from_raw(buffer.wrapping_add(*at as usize))
            .ok_or("a transaction's object is at 0")?;
        let mut bytes = vec![0; length];
        memory
            .read_exact(address, &mut bytes)
            .map_err(|_| "a transaction's object cannot be read")?;
        Ok(bytes)
    };
    let mut fds = Vec::new();
    for index in 0..count {
        // Every object starts with its 32-bit type.
        let header = object(index, 4)?;
        match u32_at(&header, 0) {
            // struct binder_fd_object: the type, a 32-bit pad, then the
            // descriptor in the low half of a 64-bit union.
            Some(BINDER_TYPE_FD) => {
                let fd_object = object(index, 24)?;
                fds.push(u32_at(&fd_object, 8).ok_or(short)? as RawFd);
            }
            // A BINDER_TYPE_FDA array's descriptors are the kernel's to
            // close: it closes them itself when the receiver frees the
            // transaction's buffer with BC_FREE_BUFFER, so a record of one
            // would outlive it and describe whatever next takes the number.
            // Left out until Detcore follows those frees
            // (https://github.com/rrnewton/hermit/issues/5046).
            Some(BINDER_TYPE_FDA) => {
                let array = object(index, 32)?;
                let num_fds = u64_at(&array, 8).ok_or(short)?;
                if num_fds != 0 {
                    return Err(
                        "a transaction carries a descriptor array, whose descriptors Linux \
                         closes with the buffer",
                    );
                }
            }
            Some(_) => {}
            None => return Err(short),
        }
    }
    Ok(fds)
}

#[cfg(test)]
mod tests {
    use reverie::syscalls::LocalMemory;

    use super::*;

    /// A transaction's bytes, as a `struct binder_transaction_data`.
    fn transaction(flags: u32, offsets: &[u64], buffer: usize) -> Vec<u8> {
        let mut data = vec![0u8; 64];
        data[20..24].copy_from_slice(&flags.to_ne_bytes());
        data[40..48].copy_from_slice(&((offsets.len() * 8) as u64).to_ne_bytes());
        data[48..56].copy_from_slice(&(buffer as u64).to_ne_bytes());
        data[56..64].copy_from_slice(&(offsets.as_ptr() as u64).to_ne_bytes());
        data
    }

    fn command(cmd: u32, payload: &[u8]) -> Vec<u8> {
        let mut bytes = cmd.to_ne_bytes().to_vec();
        bytes.extend_from_slice(payload);
        bytes
    }

    fn fd_object(fd: u32) -> Vec<u8> {
        let mut object = vec![0u8; 24];
        object[0..4].copy_from_slice(&BINDER_TYPE_FD.to_ne_bytes());
        object[8..12].copy_from_slice(&fd.to_ne_bytes());
        object
    }

    /// A read half that wants bytes blocks, unless the descriptor is
    /// nonblocking; a write-only call and a freeze without a timeout do not.
    #[test]
    fn a_binder_call_blocks_when_it_waits_for_another_process() {
        let reading = WriteRead {
            read_size: 256,
            ..WriteRead::default()
        };
        let writing = WriteRead {
            write_size: 64,
            ..WriteRead::default()
        };
        assert_eq!(
            blocking(BINDER_WRITE_READ, Some(&reading), None, false),
            Some(Blocking::Read)
        );
        assert_eq!(
            blocking(BINDER_WRITE_READ, Some(&reading), None, true),
            None
        );
        assert_eq!(
            blocking(BINDER_WRITE_READ, Some(&writing), None, false),
            None
        );
        // A read whose buffer the caller has already filled still waits for
        // work before Linux finds no room for it
        // (`an_exhausted_read_buffer_still_waits_for_work`).
        let consumed = WriteRead {
            read_size: 256,
            read_consumed: 256,
            ..WriteRead::default()
        };
        assert_eq!(
            blocking(BINDER_WRITE_READ, Some(&consumed), None, false),
            Some(Blocking::Read)
        );
        assert_eq!(
            blocking(BINDER_FREEZE, None, Some(&[7, 1, 100]), false),
            Some(Blocking::Freeze)
        );
        assert_eq!(blocking(BINDER_FREEZE, None, Some(&[7, 1, 0]), false), None);
        assert_eq!(
            blocking(BINDER_FREEZE, None, Some(&[7, 0, 100]), false),
            None
        );
        // Binder's own other requests, and another driver's, wait for nothing.
        let max_threads = binder_request(1, 5, size_of::<u32>());
        assert_eq!(blocking(max_threads, Some(&reading), None, false), None);
        assert_eq!(blocking(0x4004_6205, Some(&reading), None, false), None);
    }

    /// `--strict` refuses every binder request, whatever the run replays,
    /// naming the request and the missing piece: binder reply payloads are
    /// not recorded. Another driver's request is left alone, even one that
    /// shares binder's type byte. Before, a replay of a record that stored
    /// readmissions could make binder calls, against peers whose replies it
    /// did not record.
    #[test]
    fn strict_refuses_every_binder_request() {
        let names = [
            (BINDER_WRITE_READ, "BINDER_WRITE_READ"),
            (binder_request(3, 9, size_of::<i32>()), "BINDER_VERSION"),
            (BINDER_FREEZE, "BINDER_FREEZE"),
            (BINDER_SET_CONTEXT_MGR, "BINDER_SET_CONTEXT_MGR"),
            (BINDER_SET_CONTEXT_MGR_EXT, "BINDER_SET_CONTEXT_MGR_EXT"),
            (binder_request(3, 17, 12), "BINDER_GET_EXTENDED_ERROR"),
        ];
        for (request, name) in names {
            let refusal = strict_refusal(request).unwrap();
            assert!(
                refusal.contains(&format!("--strict refuses binder ({name})")),
                "{refusal}"
            );
        }
        for request in BINDER_REQUESTS {
            let refusal = strict_refusal(request).unwrap_or_else(|| panic!("{request:#x}"));
            assert!(
                refusal.contains("binder reply payloads are not recorded")
                    && refusal.contains("https://github.com/rrnewton/hermit/issues/5124"),
                "{refusal}"
            );
            assert!(
                !refusal.contains("a binder request)"),
                "{request:#x}: {refusal}"
            );
            // Only the low 32 bits are the request.
            assert_eq!(
                strict_refusal(request | 0xffff_ffff_0000_0000),
                Some(refusal)
            );
        }
        // Another driver's requests are not read as binder's: one with
        // BINDER_VERSION's type byte and number but another size; dma-buf's
        // DMA_BUF_IOCTL_IMPORT_SYNC_FILE, _IOW('b', 3, 8 bytes), which is
        // also the header's unserved BINDER_SET_IDLE_TIMEOUT; and the
        // unserved BINDER_SET_IDLE_PRIORITY.
        for other in [binder_request(3, 9, 8), 0x4008_6203, 0x4004_6206] {
            assert!(!is_binder_request(other), "{other:#x}");
            assert_eq!(strict_refusal(other), None);
        }
    }

    /// A binder device is told by the open file, never by a path: a
    /// character device on binderfs, whatever its number, or a misc device
    /// with a dynamic minor that the binder driver's parameter names. Other
    /// character devices (/dev/null, /dev/zero, /dev/urandom, terminals,
    /// pseudo-terminals) and fixed-minor misc devices (fuse, tun, kvm) are
    /// not, with no further lookup. A dynamic-minor misc device whose driver
    /// cannot be resolved is unknown, as is anything that cannot be read.
    #[test]
    fn a_binder_device_is_told_by_its_open_file_never_by_its_path() {
        let chr = |major, minor| Some((libc::S_IFCHR | 0o600, libc::makedev(major, minor)));
        let tmpfs = || Some(0x0102_1994_i64);
        let devtmpfs = || Some(0x0102_1994_i64);
        let misc = || Some("229 fuse\n256 userfaultfd\n257 binder\n258 hwbinder\n".to_owned());
        let devices = || Some("binder,hwbinder,vndbinder\n".to_owned());
        let unread_misc = || -> Option<String> { panic!("read /proc/misc") };
        let unread_devices = || -> Option<String> { panic!("read devices") };
        let unread_fs = || -> Option<i64> { panic!("read the file system") };

        let binderfs = identity(
            chr(511, 3),
            || Some(BINDERFS_SUPER_MAGIC),
            unread_misc,
            unread_devices,
        );
        assert_eq!(binderfs, Identity::Binder("a binderfs device".into()));
        assert_eq!(
            identity(chr(10, 257), tmpfs, misc, devices),
            Identity::Binder("the binder device binder (misc device 10:257)".into())
        );
        assert_eq!(
            identity(chr(10, 258), tmpfs, misc, devices),
            Identity::Binder("the binder device hwbinder (misc device 10:258)".into())
        );
        // A dynamic-minor misc device the parameter does not name.
        assert_eq!(
            identity(chr(10, 256), tmpfs, misc, devices),
            Identity::NotBinder
        );
        assert_eq!(
            identity(chr(10, 257), tmpfs, misc, || Some("vndbinder".into())),
            Identity::NotBinder
        );
        // Ordinary character devices, whatever binder's state: /dev/null,
        // /dev/zero, /dev/urandom, a virtual console, /dev/tty, /dev/ptmx and
        // a pseudo-terminal; and fixed-minor misc devices: fuse, tun, kvm.
        for (major, minor) in [
            (1, 3),
            (1, 5),
            (1, 9),
            (4, 1),
            (5, 0),
            (5, 2),
            (136, 0),
            (10, 229),
            (10, 200),
            (10, 232),
        ] {
            assert_eq!(
                identity(chr(major, minor), devtmpfs, unread_misc, unread_devices),
                Identity::NotBinder,
                "{major}:{minor}"
            );
        }
        // A regular file, such as binderfs's own logs, and a directory.
        for mode in [libc::S_IFREG | 0o444, libc::S_IFDIR | 0o755] {
            assert_eq!(
                identity(Some((mode, 0)), unread_fs, unread_misc, unread_devices),
                Identity::NotBinder
            );
        }
        // Unknown: no stat; no file system; for a dynamic-minor misc device,
        // no /proc/misc, a minor it does not list, or no binder driver's
        // parameter, so that userfaultfd's device, say, is unknown on a kernel
        // without one; and that for a low dynamic minor too.
        for unknown in [
            identity(None, unread_fs, unread_misc, unread_devices),
            identity(chr(1, 3), || None, unread_misc, unread_devices),
            identity(chr(10, 257), tmpfs, || None, unread_devices),
            identity(chr(10, 300), tmpfs, misc, unread_devices),
            identity(chr(10, 257), tmpfs, misc, || None),
            identity(chr(10, 256), tmpfs, misc, || None),
            identity(chr(10, 60), tmpfs, || Some(" 60 binder\n".into()), || None),
        ] {
            assert!(matches!(unknown, Identity::Unknown(_)), "{unknown:?}");
        }
        // Through host procfs: /dev/null, /dev/zero, /dev/urandom, a
        // pseudo-terminal master and a regular file are not binder devices,
        // and a descriptor that is not open is unknown.
        let mut open = vec![tempfile::tempfile().unwrap()];
        for path in ["/dev/null", "/dev/zero", "/dev/urandom", "/dev/ptmx"] {
            if let Ok(file) = std::fs::File::open(path) {
                open.push(file);
            }
        }
        for file in &open {
            let link = format!("/proc/self/fd/{}", std::os::fd::AsRawFd::as_raw_fd(file));
            assert_eq!(
                identity_of(std::path::Path::new(&link)),
                Identity::NotBinder,
                "{link}"
            );
        }
        let closed = identity_of(std::path::Path::new("/proc/self/fd/-1"));
        assert!(matches!(closed, Identity::Unknown(_)), "{closed:?}");
    }

    /// The binder driver's parameter is read from the built-in driver's
    /// module, else from `binder_linux`'s, the name an out-of-tree build
    /// loads under.
    #[test]
    fn the_binder_devices_parameter_is_read_from_binder_or_binder_linux() {
        let only = |module: &'static str| {
            move |path: &str| {
                (path == format!("/sys/module/{module}/parameters/devices"))
                    .then(|| format!("{module}-devices"))
            }
        };
        assert_eq!(
            binder_devices_parameter(only("binder")),
            Some("binder-devices".into())
        );
        assert_eq!(
            binder_devices_parameter(only("binder_linux")),
            Some("binder_linux-devices".into())
        );
        assert_eq!(binder_devices_parameter(|_| None), None);
    }

    /// A call that gives the guest a binder device, or a descriptor whose
    /// identity cannot be read, is refused by name, naming the descriptor,
    /// how it arrived and what it is or why it is unknown, whatever came with
    /// it; a descriptor known not to be a binder device is not.
    #[test]
    fn a_strict_run_refuses_the_first_binder_descriptor_a_call_gives_it() {
        let identify = |fd| match fd {
            4 => Identity::Binder("a binderfs device".into()),
            5 => Identity::Unknown("its file cannot be inspected".into()),
            _ => Identity::NotBinder,
        };
        assert_eq!(strict_arrival_refusal(&[3, 6], "opened", identify), None);
        assert_eq!(strict_arrival_refusal(&[], "opened", identify), None);
        let reason =
            strict_arrival_refusal(&[3, 4, 5], "received through SCM_RIGHTS", identify).unwrap();
        assert!(
            reason.starts_with(
                "--strict refuses binder: descriptor 4, received through SCM_RIGHTS, is a \
                 binderfs device. A --strict run holds no binder descriptor"
            ) && reason.contains("binder reply payloads are not recorded")
                && reason.contains("https://github.com/rrnewton/hermit/issues/5124"),
            "{reason}"
        );
        let unknown = strict_arrival_refusal(&[3, 5], "opened", identify).unwrap();
        assert!(
            unknown.starts_with(
                "--strict refuses binder: descriptor 5, opened, may be a binder device: its \
                 identity could not be read (its file cannot be inspected)."
            ),
            "{unknown}"
        );
    }

    /// The start check finds a binder descriptor, or one of unknown identity,
    /// that the process holds, passes over the ones it is told to skip (the
    /// runtime's own), and fails, never passes, on a descriptor table it
    /// cannot list.
    #[test]
    fn the_start_check_finds_a_held_binder_descriptor_and_fails_on_an_unlistable_table() {
        let held = tempfile::tempfile().unwrap();
        let held_fd = std::os::fd::AsRawFd::as_raw_fd(&held);
        let target = std::fs::read_link(format!("/proc/self/fd/{held_fd}")).unwrap();
        let held_is = |identity: Identity| {
            let target = target.clone();
            move |link: &std::path::Path| {
                if std::fs::read_link(link).ok().as_ref() == Some(&target) {
                    identity.clone()
                } else {
                    Identity::NotBinder
                }
            }
        };
        let table = std::path::Path::new("/proc/self/fd");
        let binder = Identity::Binder("a binderfs device".into());
        assert_eq!(
            held_binder_descriptor(table, |_| false, held_is(binder.clone())).unwrap(),
            Some((held_fd, binder.clone()))
        );
        let unknown = Identity::Unknown("its file cannot be inspected".into());
        assert_eq!(
            held_binder_descriptor(table, |_| false, held_is(unknown.clone())).unwrap(),
            Some((held_fd, unknown))
        );
        assert_eq!(
            held_binder_descriptor(table, |fd| fd == held_fd, held_is(binder.clone())).unwrap(),
            None
        );
        assert!(
            held_binder_descriptor(
                std::path::Path::new("/proc/self/no-such-table"),
                |_| false,
                held_is(binder)
            )
            .is_err()
        );
    }

    /// Under `--strict` on KVM, where no guest descriptor's identity can be
    /// read, a run is refused on a host whose kernel offers binder in any
    /// form: binderfs registered; a misc device named like binder's, with or
    /// without a binder driver's parameter, or named by that parameter; a
    /// parameter that exists but cannot be read; an inherited descriptor that
    /// is a binder device or of unknown identity; and wherever
    /// `/proc/filesystems`, `/proc/misc` or Hermit's descriptor table cannot
    /// be read. A kernel without binder is admitted.
    #[test]
    fn a_strict_kvm_run_is_refused_on_a_host_that_offers_binder() {
        let plain = "nodev\tsysfs\nnodev\ttmpfs\n\text4\n";
        let binderfs = "nodev\tsysfs\nnodev\tbinder\n\text4\n";
        let misc = "229 fuse\n256 userfaultfd\n";
        let binder_misc = "229 fuse\n257 hwbinder\n";
        let custom_misc = "229 fuse\n257 ipc0\n";
        let present = DevicesParameter::Present("binder,hwbinder,vndbinder".into());
        let custom = DevicesParameter::Present("ipc0".into());
        let missing = DevicesParameter::Missing;
        let unreadable = DevicesParameter::Unreadable("permission denied".into());
        let refused = |filesystems, misc, devices: &DevicesParameter, held| {
            kvm_strict_binder_refusal(filesystems, misc, devices, Ok(Vec::new()), held)
                .unwrap_or_else(|| panic!("{filesystems:?} {misc:?} {devices:?}"))
        };
        // Admitted: a kernel without binder, whether or not a binder
        // driver's parameter is there, and no inherited binder descriptor.
        for devices in [&present, &missing] {
            assert_eq!(
                kvm_strict_binder_refusal(
                    Some(plain),
                    Some(misc),
                    devices,
                    Ok(Vec::new()),
                    Ok(None)
                ),
                None
            );
        }
        assert_eq!(
            kvm_strict_binder_refusal(
                Some(plain),
                Some(misc),
                &missing,
                Ok(Vec::new()),
                Ok(Some((0, Identity::NotBinder)))
            ),
            None
        );
        for (reason, expected) in [
            (
                refused(Some(binderfs), Some(misc), &missing, Ok(None)),
                "the host kernel offers binderfs",
            ),
            (
                refused(Some(plain), Some(binder_misc), &present, Ok(None)),
                "the host kernel has a binder device (/proc/misc: \"257 hwbinder\")",
            ),
            // With no binder driver's parameter, as a binder module loaded
            // under another name has none, the name alone refuses.
            (
                refused(Some(plain), Some(binder_misc), &missing, Ok(None)),
                "the host kernel has a binder device (/proc/misc: \"257 hwbinder\")",
            ),
            (
                refused(Some(plain), Some(custom_misc), &custom, Ok(None)),
                "the host kernel has a binder device (/proc/misc: \"257 ipc0\")",
            ),
            (
                refused(Some(plain), Some(misc), &unreadable, Ok(None)),
                "the binder driver's devices parameter cannot be read (permission denied)",
            ),
            // No parameter, and /sys/module not visible: its absence proves
            // nothing, so a custom-named legacy device ("ipc0") is not let
            // through.
            (
                kvm_strict_binder_refusal(
                    Some(plain),
                    Some(custom_misc),
                    &missing,
                    Err("/sys/module cannot be inspected (No such file or directory)".into()),
                    Ok(None),
                )
                .unwrap(),
                "/sys/module cannot be inspected (No such file or directory), so the absence of \
                 a binder driver's devices parameter does not prove",
            ),
            (
                refused(
                    Some(plain),
                    Some(misc),
                    &present,
                    Ok(Some((3, Identity::Binder("a binderfs device".into())))),
                ),
                "Hermit's descriptor 3, which the guest inherits, is a binderfs device",
            ),
            (
                refused(
                    Some(plain),
                    Some(misc),
                    &present,
                    Ok(Some((
                        4,
                        Identity::Unknown("its file cannot be inspected".into()),
                    ))),
                ),
                "Hermit's descriptor 4, which the guest inherits, may be a binder device",
            ),
            (
                refused(None, Some(misc), &present, Ok(None)),
                "/proc/filesystems cannot be read",
            ),
            (
                refused(Some(plain), None, &present, Ok(None)),
                "/proc/misc cannot be read",
            ),
            (
                refused(
                    Some(plain),
                    Some(misc),
                    &present,
                    Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied)),
                ),
                "Hermit's own descriptors, which the guest inherits, cannot be listed",
            ),
        ] {
            assert!(
                reason.starts_with(&format!("--strict refuses binder: {expected}"))
                    && reason.contains("https://github.com/rrnewton/hermit/issues/5129")
                    && reason.contains("https://github.com/rrnewton/hermit/issues/5124"),
                "{reason}"
            );
        }
    }

    /// The binder driver's parameter is read from every one of
    /// [`BINDER_MODULES`]: present where any has one, missing where none
    /// has, and unreadable, whatever the others say, where one exists but
    /// cannot be read.
    #[test]
    fn the_binder_devices_parameter_tells_missing_from_unreadable() {
        use std::io::Error;
        use std::io::ErrorKind;
        let with = |binder: Result<&'static str, ErrorKind>,
                    linux: Result<&'static str, ErrorKind>| {
            move |path: &str| {
                let answer = if path.contains("/binder_linux/") {
                    linux
                } else {
                    binder
                };
                answer.map(str::to_owned).map_err(Error::from)
            }
        };
        let gone = Err(ErrorKind::NotFound);
        let denied = Err(ErrorKind::PermissionDenied);
        assert_eq!(
            binder_devices_parameter_state(with(gone, gone)),
            DevicesParameter::Missing
        );
        assert_eq!(
            binder_devices_parameter_state(with(Ok("binder,hwbinder\n"), gone)),
            DevicesParameter::Present("binder,hwbinder".into())
        );
        assert_eq!(
            binder_devices_parameter_state(with(gone, Ok("binder\n"))),
            DevicesParameter::Present("binder".into())
        );
        for state in [
            binder_devices_parameter_state(with(denied, gone)),
            binder_devices_parameter_state(with(gone, denied)),
            binder_devices_parameter_state(with(Ok("binder"), denied)),
        ] {
            assert!(
                matches!(state, DevicesParameter::Unreadable(_)),
                "{state:?}"
            );
        }
    }

    /// The admission check runs only for a `--strict` run on a backend whose
    /// guest descriptors are not host descriptors (KVM): on a host that
    /// offers binderfs, a strict KVM run is refused, while a strict ptrace
    /// run and a KVM run without `--strict` are not checked at all.
    #[test]
    fn only_a_strict_kvm_run_is_checked_for_a_host_that_offers_binder() {
        let binder_host = |path: &str| match path {
            "/proc/filesystems" => Ok("nodev\tsysfs\nnodev\tbinder\n".to_owned()),
            "/proc/misc" => Ok("229 fuse\n".to_owned()),
            _ => Err(std::io::Error::from(std::io::ErrorKind::NotFound)),
        };
        let unread = |_: &str| -> std::io::Result<String> { panic!("read a host file") };
        let config = |strict_binder, backend| detcore_model::config::Config {
            strict_binder,
            backend,
            ..Default::default()
        };
        let kvm = config(true, reverie::BackendCapabilities::KVM);
        let refusal =
            kvm_strict_binder_admission_with(&kvm, binder_host, || Ok(Vec::new()), || Ok(None))
                .unwrap();
        assert!(
            refusal.contains("the host kernel offers binderfs"),
            "{refusal}"
        );
        for unchecked in [
            config(true, reverie::BackendCapabilities::PTRACE),
            config(false, reverie::BackendCapabilities::KVM),
        ] {
            assert_eq!(
                kvm_strict_binder_admission_with(
                    &unchecked,
                    unread,
                    || panic!("inspect /sys/module"),
                    || panic!("list descriptors")
                ),
                None
            );
        }
    }

    /// The admission's own reader, with `/sys/module` listed by the
    /// production lister ([`directory_entries`]) from a directory laid out
    /// as the kernel's: on a legacy binder host (binderfs not registered, a
    /// misc device "ipc0" named by `binder.devices=ipc0`) whose parameter
    /// paths read as not found:
    /// - a view that hides sysfs proves nothing, and is refused;
    /// - a full module view that lists `binder` (past its first entries)
    ///   with no `devices` parameter is refused by name;
    /// - a full module view that lists neither `binder` nor `binder_linux`
    ///   is a host without a binder driver, and is admitted.
    #[test]
    fn a_strict_kvm_run_is_refused_unless_sys_module_shows_no_binder_driver() {
        let host = |path: &str| match path {
            "/proc/filesystems" => Ok("nodev\tsysfs\nnodev\ttmpfs\n\text4\n".to_owned()),
            "/proc/misc" => Ok("229 fuse\n 60 ipc0\n".to_owned()),
            _ => Err(std::io::Error::from(std::io::ErrorKind::NotFound)),
        };
        let kvm = detcore_model::config::Config {
            strict_binder: true,
            backend: reverie::BackendCapabilities::KVM,
            ..Default::default()
        };
        let modules = |names: &[&str]| {
            let directory = tempfile::tempdir().unwrap();
            for name in names {
                std::fs::create_dir(directory.path().join(name)).unwrap();
            }
            directory
        };
        let hidden = || {
            sys_module_binder_modules(
                Err(std::io::Error::from(std::io::ErrorKind::NotFound)),
                Err(std::io::Error::from(std::io::ErrorKind::NotFound)),
            )
        };
        let refusal = kvm_strict_binder_admission_with(&kvm, host, hidden, || Ok(None)).unwrap();
        assert!(
            refusal.starts_with("--strict refuses binder: /sys/module cannot be inspected")
                && refusal.contains("does not prove that the host kernel has no binder driver"),
            "{refusal}"
        );
        for module in BINDER_MODULES {
            let view = modules(&["acpi", "kvm", "fuse", "loop", "zswap", module]);
            let listed =
                || sys_module_binder_modules(Ok(SYSFS_MAGIC), directory_entries(view.path()));
            let refusal =
                kvm_strict_binder_admission_with(&kvm, host, listed, || Ok(None)).unwrap();
            assert!(
                refusal.starts_with(&format!(
                    "--strict refuses binder: /sys/module lists the binder module ({module}) but \
                     its devices parameter is missing"
                )),
                "{refusal}"
            );
        }
        let view = modules(&["acpi", "kvm", "fuse", "loop", "zswap"]);
        let listed = || sys_module_binder_modules(Ok(SYSFS_MAGIC), directory_entries(view.path()));
        assert_eq!(
            kvm_strict_binder_admission_with(&kvm, host, listed, || Ok(None)),
            None
        );
    }

    /// The module directory is listed to its end, every entry named, and
    /// `/sys/module` counts only as a sysfs directory that lists: not where
    /// `statfs` fails, not on another file system (an empty tmpfs standing
    /// in for sysfs), and not where it cannot be listed.
    #[test]
    fn sys_module_is_listed_in_full_and_only_as_sysfs() {
        use std::io::Error;
        use std::io::ErrorKind;
        let directory = tempfile::tempdir().unwrap();
        let mut expected: Vec<String> = (0..40).map(|index| format!("module{index}")).collect();
        expected.push("binder_linux".into());
        for name in &expected {
            std::fs::create_dir(directory.path().join(name)).unwrap();
        }
        let mut listed = directory_entries(directory.path()).unwrap();
        listed.sort();
        expected.sort();
        assert_eq!(listed, expected);
        assert_eq!(
            sys_module_binder_modules(Ok(SYSFS_MAGIC), Ok(listed)),
            Ok(vec!["binder_linux"])
        );
        assert_eq!(
            sys_module_binder_modules(Ok(SYSFS_MAGIC), Ok(vec!["kvm".into(), "fuse".into()])),
            Ok(vec![])
        );
        for (file_system, listing, expected) in [
            (
                Err(Error::from(ErrorKind::NotFound)),
                Ok(Vec::new()),
                "/sys/module cannot be inspected",
            ),
            (
                Ok(0x0102_1994),
                Ok(Vec::new()),
                "/sys/module is not sysfs (file system 0x1021994)",
            ),
            (
                Ok(SYSFS_MAGIC),
                Err(Error::from(ErrorKind::PermissionDenied)),
                "/sys/module cannot be listed",
            ),
        ] {
            let why = sys_module_binder_modules(file_system, listing).unwrap_err();
            assert!(why.starts_with(expected), "{why}");
        }
    }

    /// Every binder call that runs as external input is counted, and each
    /// guest process is warned about once.
    #[test]
    fn external_binder_calls_are_counted_and_each_process_is_warned_once() {
        let before = external_calls();
        assert!(note_blocking(Blocking::Read, 1_000_001));
        assert!(!note_blocking(Blocking::Read, 1_000_001));
        assert!(note_blocking(Blocking::Freeze, 1_000_002));
        assert!(external_calls() >= before + 3);
    }

    /// A read whose buffer is already full (`read_consumed == read_size`)
    /// waits for work like any other: Linux runs the read half whenever
    /// `read_size` is nonzero and waits in `binder_wait_for_work` before it
    /// checks the room left (drivers/android/binder.c:5073, :4412, :4445 at
    /// v6.4). Classified as returning on its own, it ran in the caller's turn
    /// and held the guest's only turn while the sibling that would send it
    /// work never ran. It polls first, as any read without a write half does,
    /// and so goes to the background path when no work is queued.
    #[test]
    fn an_exhausted_read_buffer_still_waits_for_work() {
        for read_consumed in [1, 255, 256, 4096] {
            let exhausted = WriteRead {
                read_size: 256,
                read_consumed,
                read_buffer: 0x1000,
                ..WriteRead::default()
            };
            assert!(exhausted.reads(), "{exhausted:?}");
            assert_eq!(
                blocking(BINDER_WRITE_READ, Some(&exhausted), None, false),
                Some(Blocking::Read),
                "{exhausted:?}"
            );
            assert_eq!(
                route(BINDER_WRITE_READ, Some(&exhausted), None, false),
                Route::PollFirst,
                "{exhausted:?}"
            );
            // On a nonblocking descriptor the same read returns EAGAIN
            // instead of waiting (:4408-4410).
            assert_eq!(
                route(BINDER_WRITE_READ, Some(&exhausted), None, true),
                Route::InTurn
            );
        }
        // No read half at all: nothing waits.
        let none = WriteRead {
            read_consumed: 8,
            ..WriteRead::default()
        };
        assert!(!none.reads());
        assert_eq!(
            route(BINDER_WRITE_READ, Some(&none), None, false),
            Route::InTurn
        );
    }

    /// The zero-timeout poll that keeps a read with queued work in the
    /// caller's turn asks only about this thread's work, so it is asked only
    /// of a read with nothing before it. A freeze waits for another process's
    /// transactions to drain, and a write+read's write half can leave the
    /// thread unable to take the process work the poll saw (a synchronous
    /// transaction pushes its stack: drivers/android/binder.c:3616-3619 and
    /// :559-564 at v6.4); a ready caller with a blocked target would hold the
    /// guest's only turn until the freeze timed out, or for good. Neither ever
    /// polls: each is host-timed, whether or not its caller has work.
    #[test]
    fn only_a_read_with_nothing_before_it_asks_the_poll() {
        let read = WriteRead {
            read_size: 256,
            ..WriteRead::default()
        };
        assert_eq!(
            route(BINDER_WRITE_READ, Some(&read), None, false),
            Route::PollFirst
        );
        // A write half the caller already ran in full is no write half.
        let written = WriteRead {
            write_size: 8,
            write_consumed: 8,
            ..read
        };
        assert!(!written.writes());
        assert_eq!(
            route(BINDER_WRITE_READ, Some(&written), None, false),
            Route::PollFirst
        );
        // A write+read, whole or partly written, and with an exhausted read
        // buffer too.
        for (write_consumed, read_consumed) in [(0, 0), (4, 0), (0, 256)] {
            let write_read = WriteRead {
                write_size: 8,
                write_consumed,
                read_consumed,
                ..read
            };
            assert!(write_read.writes());
            assert_eq!(
                route(BINDER_WRITE_READ, Some(&write_read), None, false),
                Route::HostTimed(Blocking::Read, Effect::Write),
                "{write_read:?}"
            );
            // On a nonblocking descriptor the read half returns EAGAIN, and
            // the write half runs in the caller's turn with it.
            assert_eq!(
                route(BINDER_WRITE_READ, Some(&write_read), None, true),
                Route::InTurn
            );
        }
        // A write-only call returns on its own.
        let write_only = WriteRead {
            write_size: 8,
            ..WriteRead::default()
        };
        assert_eq!(
            route(BINDER_WRITE_READ, Some(&write_only), None, false),
            Route::InTurn
        );
        // A freeze with a timeout, on any descriptor; one without a timeout,
        // or a thaw, returns on its own.
        for nonblocking in [false, true] {
            assert_eq!(
                route(BINDER_FREEZE, None, Some(&[7, 1, 100]), nonblocking),
                Route::HostTimed(Blocking::Freeze, Effect::Freeze)
            );
            assert_eq!(
                route(BINDER_FREEZE, None, Some(&[7, 1, 0]), nonblocking),
                Route::InTurn
            );
            assert_eq!(
                route(BINDER_FREEZE, None, Some(&[7, 0, 100]), nonblocking),
                Route::InTurn
            );
        }
        let reason = host_timed_reason(Effect::Write, 5);
        assert!(
            reason.starts_with("ioctl(BINDER_WRITE_READ) on descriptor 5 "),
            "{reason}"
        );
        assert!(reason.contains("write half"), "{reason}");
        let reason = host_timed_reason(Effect::Freeze, 6);
        assert!(
            reason.starts_with("ioctl(BINDER_FREEZE) on descriptor 6 "),
            "{reason}"
        );
    }

    /// A read off the turn that delivered descriptors Detcore does not
    /// record, or whose descriptors cannot be told, is named with them; one
    /// that delivered none is not.
    #[test]
    fn a_read_off_the_turn_names_the_descriptors_it_leaves_unrecorded() {
        let delivered = Delivered {
            fds: vec![7, 9],
            messages: Vec::new(),
            refused: None,
            may_have_installed: false,
        };
        let reason = unrecorded_receipt_reason(4, BinderRun::OffTurnShared, &delivered).unwrap();
        assert!(
            reason.starts_with("ioctl(BINDER_WRITE_READ) on descriptor 4 ran off the guest's turn"),
            "{reason}"
        );
        let in_turn = unrecorded_receipt_reason(4, BinderRun::InTurn, &delivered).unwrap();
        assert!(
            in_turn.starts_with("ioctl(BINDER_WRITE_READ) on descriptor 4 ran in the guest's turn"),
            "{in_turn}"
        );
        assert!(reason.contains("delivered descriptors [7, 9]"), "{reason}");
        let unknown = Delivered {
            refused: Some("its return commands cannot be read"),
            ..Delivered::default()
        };
        let reason = unrecorded_receipt_reason(4, BinderRun::OffTurnAlone, &unknown).unwrap();
        assert!(
            reason.contains("may have delivered descriptors Detcore cannot name (its return commands cannot be read)"),
            "{reason}"
        );
        assert_eq!(
            unrecorded_receipt_reason(4, BinderRun::InTurn, &Delivered::default()),
            None
        );
    }

    /// The command codes are the uapi header's, from their `_IOR` parts.
    #[test]
    fn the_command_codes_encode_their_payload_sizes() {
        let ior = |nr: u32, size: u32| (2 << 30) | (size << 16) | (u32::from(b'r') << 8) | nr;
        assert_eq!(BR_TRANSACTION, ior(2, 64));
        assert_eq!(BR_TRANSACTION_SEC_CTX, ior(2, 72));
        assert_eq!(BR_REPLY, ior(3, 64));
        let iowr = (3u64 << 30) | (48 << 16) | (u64::from(b'b') << 8) | 1;
        assert_eq!(BINDER_WRITE_READ, iowr);
        let iow = (1u64 << 30) | (12 << 16) | (u64::from(b'b') << 8) | 14;
        assert_eq!(BINDER_FREEZE, iow);
        assert_eq!(BINDER_FREEZE, 0x400c_620e);
        assert_eq!(BINDER_WRITE_READ, 0xc030_6201);
        assert!(is_write_read(BINDER_WRITE_READ | 0xffff_ffff_0000_0000));
        assert!(!is_write_read(0x4004_6205));
        // The context-manager requests, and the read-write ones the strict
        // policy recognizes by number, direction and size (BINDER_VERSION,
        // BINDER_GET_EXTENDED_ERROR), as include/uapi/linux/android/binder.h
        // spells them at v6.1.
        assert_eq!(BINDER_SET_CONTEXT_MGR, 0x4004_6207);
        assert_eq!(BINDER_SET_CONTEXT_MGR_EXT, 0x4018_620d);
        assert!(is_binder_request(0xc004_6209));
        assert!(is_binder_request(0xc00c_6211));
    }

    /// A reply carrying one descriptor object, after commands without
    /// payload of interest, names it; a status-code reply names none.
    #[test]
    fn a_reply_names_the_descriptors_its_objects_hold() {
        let mut buffer = [0u8; 64];
        buffer[16..40].copy_from_slice(&fd_object(9));
        let offsets = [16u64];
        let noop = 0x0000_720c; // BR_NOOP, _IO('r', 12)
        let complete = 0x0000_7206; // BR_TRANSACTION_COMPLETE, _IO('r', 6)
        let mut commands = command(noop, &[]);
        commands.extend(command(complete, &[]));
        commands.extend(command(
            BR_REPLY,
            &transaction(0, &offsets, buffer.as_ptr() as usize),
        ));
        assert_eq!(
            installed_descriptors(&LocalMemory::new(), &commands),
            Ok(vec![9])
        );

        // A real status reply carries no objects. The flag itself is not
        // read, because the driver does not read it when it installs
        // descriptors: a reply that carries one still names it.
        let status = command(BR_REPLY, &transaction(TF_STATUS_CODE, &[], 0));
        assert_eq!(
            installed_descriptors(&LocalMemory::new(), &status),
            Ok(vec![])
        );
        let flagged = command(
            BR_REPLY,
            &transaction(TF_STATUS_CODE, &offsets, buffer.as_ptr() as usize),
        );
        assert_eq!(
            installed_descriptors(&LocalMemory::new(), &flagged),
            Ok(vec![9])
        );
    }

    /// A descriptor array is refused: Linux closes its numbers with the
    /// transaction's buffer, so a record of one would outlive it.
    #[test]
    fn a_descriptor_array_is_refused_rather_than_recorded() {
        let numbers: [u32; 3] = [4, 5, 6];
        let parent_data = {
            let mut data = vec![0u8; 8];
            numbers
                .iter()
                .for_each(|fd| data.extend_from_slice(&fd.to_ne_bytes()));
            data
        };
        let mut buffer = [0u8; 128];
        // The parent buffer object at 0.
        buffer[0..4].copy_from_slice(&BINDER_TYPE_PTR.to_ne_bytes());
        buffer[8..16].copy_from_slice(&(parent_data.as_ptr() as u64).to_ne_bytes());
        buffer[16..24].copy_from_slice(&(parent_data.len() as u64).to_ne_bytes());
        // The array object at 40: three descriptors at offset 8 of parent 0.
        buffer[40..44].copy_from_slice(&BINDER_TYPE_FDA.to_ne_bytes());
        buffer[48..56].copy_from_slice(&3u64.to_ne_bytes());
        buffer[56..64].copy_from_slice(&0u64.to_ne_bytes());
        buffer[64..72].copy_from_slice(&8u64.to_ne_bytes());
        let offsets = [0u64, 40];
        let commands = command(
            BR_TRANSACTION_SEC_CTX,
            &[
                transaction(0, &offsets, buffer.as_ptr() as usize),
                vec![0u8; 8],
            ]
            .concat(),
        );
        let refusal =
            installed_descriptors(&LocalMemory::new(), &commands).expect_err("a descriptor array");
        assert!(refusal.contains("Linux closes"), "{refusal}");
        // An empty array names nothing and is not refused.
        buffer[48..56].copy_from_slice(&0u64.to_ne_bytes());
        let commands = command(
            BR_TRANSACTION_SEC_CTX,
            &[
                transaction(0, &offsets, buffer.as_ptr() as usize),
                vec![0u8; 8],
            ]
            .concat(),
        );
        assert_eq!(
            installed_descriptors(&LocalMemory::new(), &commands),
            Ok(vec![])
        );
    }

    /// A buffer cut short names nothing and says so, rather than guessing.
    #[test]
    fn a_cut_short_buffer_names_nothing() {
        let commands = command(BR_REPLY, &[0u8; 10]);
        assert!(installed_descriptors(&LocalMemory::new(), &commands).is_err());
        assert!(installed_descriptors(&LocalMemory::new(), &[1, 2]).is_err());
    }

    /// The whole call: a read that consumed nothing names nothing, one that
    /// claims more than its size is refused, and one with a transaction
    /// names its descriptors.
    #[test]
    fn a_write_read_names_what_its_read_delivered() {
        let mut buffer = [0u8; 64];
        buffer[0..24].copy_from_slice(&fd_object(12));
        let offsets = [0u64];
        let commands = command(
            BR_TRANSACTION,
            &transaction(0, &offsets, buffer.as_ptr() as usize),
        );
        let call = |read_size: u64, read_consumed: u64| {
            [0, 0, 0, read_size, read_consumed, commands.as_ptr() as u64]
        };
        let memory = LocalMemory::new();
        let words = call(256, commands.len() as u64);

        assert_eq!(
            delivered(&memory, words.as_ptr() as usize, 0),
            Delivered {
                fds: vec![12],
                messages: vec![crate::syscalls::scm_rights::DeliveredMessage {
                    count: 1,
                    truncated: false,
                    bytes: None,
                }],
                refused: None,
                may_have_installed: false,
            }
        );
        let words = call(256, 0);
        assert_eq!(
            delivered(&memory, words.as_ptr() as usize, 0),
            Delivered::default()
        );
        let words = call(4, commands.len() as u64);
        assert!(
            delivered(&memory, words.as_ptr() as usize, 0)
                .refused
                .is_some()
        );

        // An exactly full command buffer is parsed through its last FD.
        // Cutting a larger one on a command boundary must refuse explicitly:
        // otherwise its valid completion prefix would hide the installed FD.
        let noop = 0x0000_720cu32; // BR_NOOP, _IO('r', 12)
        let complete = 0x0000_7206u32; // BR_TRANSACTION_COMPLETE, _IO('r', 6)
        let mut at_limit = noop.to_ne_bytes().to_vec();
        at_limit.extend(
            complete
                .to_ne_bytes()
                .repeat((MAX_COMMAND_BYTES - commands.len() - 4) / 4),
        );
        at_limit.extend_from_slice(&commands);
        assert_eq!(at_limit.len(), MAX_COMMAND_BYTES);
        let words = [
            0,
            0,
            0,
            at_limit.len() as u64,
            at_limit.len() as u64,
            at_limit.as_ptr() as u64,
        ];
        let expected = delivered(&memory, words.as_ptr() as usize, 0);
        assert_eq!(expected.fds, vec![12]);
        assert!(expected.refused.is_none());

        let mut over_limit = noop.to_ne_bytes().to_vec();
        over_limit.extend(complete.to_ne_bytes().repeat(MAX_COMMAND_BYTES / 4 - 1));
        over_limit.extend_from_slice(&commands);
        let words = [
            0,
            0,
            0,
            over_limit.len() as u64,
            over_limit.len() as u64,
            over_limit.as_ptr() as u64,
        ];
        assert_eq!(
            delivered(&memory, words.as_ptr() as usize, 0),
            refused("its newly returned commands exceed Detcore's read limit")
        );
        // Old commands do not consume this call's budget or get parsed again.
        // Only the small FD transaction after the large old prefix is new.
        assert_eq!(
            Snapshot::read(
                &memory,
                BINDER_WRITE_READ,
                [0, 0, 0, 0, MAX_COMMAND_BYTES as u64, 0].as_ptr() as usize
            )
            .consumed_before(),
            MAX_COMMAND_BYTES as u64
        );
        assert_eq!(
            delivered(&memory, words.as_ptr() as usize, MAX_COMMAND_BYTES as u64),
            expected
        );
    }
}
