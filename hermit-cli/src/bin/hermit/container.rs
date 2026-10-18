/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::any::Any;
use std::ffi::CString;
use std::fs;
use std::fs::File;
use std::io;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::PermissionsExt;
use std::panic;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::AtomicI32;
use std::sync::atomic::Ordering;

use anyhow::anyhow;
use detcore_model::config::MountInfoRootRewrite;
use detcore_model::container_mounts::BASE_MOUNTPOINTS;
use detcore_model::container_mounts::ContainerMountTable;
use detcore_model::container_mounts::Installation;
use detcore_model::container_mounts::assign_installed_rows;
use detcore_model::container_mounts::check_installed_rows;
use detcore_model::container_mounts::owned_mounts;
use hermit::Context;
use hermit::Error;
use hermit::FailureKind;
use hermit::HERMIT_DEADLINE_EXIT;
use hermit::HERMIT_LOG_CAP_EXIT;
use hermit::SerializableError;
use hermit::SkidOvershootError;
use hermit::capture_mountinfo_identity_order_keeping;
use nix::sched::CpuSet;
use nix::sched::sched_getaffinity;
use nix::sys::signal::SaFlags;
use nix::sys::signal::SigAction;
use nix::sys::signal::SigHandler;
use nix::sys::signal::SigSet;
use nix::sys::signal::Signal;
use nix::sys::signal::sigaction;
use nix::unistd::Pid;
use reverie::process::Container;
use reverie::process::ExitStatus;
use reverie::process::Mount;
use reverie::process::MountFlags;
use reverie::process::Namespace;
use reverie::process::RunError;

const GROUP_FILE: &str = "/etc/group";
const NSCD_DIR: &str = "/var/run/nscd";
const DETERMINISTIC_GROUP_ROOT: &[u8] = b"/tmpvol/.hermit/etc/group";
const DETERMINISTIC_NSCD_ROOT: &[u8] = b"/tmpvol/.hermit/run/nscd";
const DETERMINISTIC_TMP_ROOT: &[u8] = b"/tmpvol/.hermit/tmp";

#[derive(Debug)]
struct MountInfoRootSource {
    source: Option<File>,
    target: PathBuf,
    deterministic_root: Option<Vec<u8>>,
    rewrite_descendant_roots: bool,
    raw_mountpoint_prefix: Option<Vec<u8>>,
    deterministic_mountpoint_prefix: Option<Vec<u8>>,
}

fn open_path(path: &Path) -> io::Result<File> {
    let path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))?;
    // O_PATH pins the object without requiring read permission.  The descriptor
    // remains valid if a path is renamed after this point, which is exactly why
    // provenance is carried by descriptors rather than by a second stat(path).
    let fd = unsafe { libc::open(path.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `open` returned a new owned descriptor above.
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn mount_id_for_fd(fd: i32) -> io::Result<u64> {
    let contents = fs::read(format!("/proc/self/fdinfo/{fd}"))?;
    detcore_model::procfs::parse_fdinfo_mount_id(&contents).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "fdinfo must contain exactly one decimal mnt_id field",
        )
    })
}

fn mountinfo_root_for_id(raw_mount_id: u64) -> io::Result<Vec<u8>> {
    let contents = fs::read("/proc/self/mountinfo")?;
    let rows = detcore_model::procfs::parse_mountinfo(&contents).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "mountinfo has a malformed row")
    })?;
    rows.into_iter()
        .find(|row| row.raw_mount_id == raw_mount_id)
        .map(|row| row.root)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "mountinfo omitted the proven mount ID",
            )
        })
}

fn mountinfo_escape_path(path: &Path) -> Vec<u8> {
    let mut encoded = Vec::new();
    for byte in path.as_os_str().as_bytes() {
        match byte {
            b' ' => encoded.extend_from_slice(b"\\040"),
            b'\t' => encoded.extend_from_slice(b"\\011"),
            b'\n' => encoded.extend_from_slice(b"\\012"),
            b'\\' => encoded.extend_from_slice(b"\\134"),
            byte => encoded.push(*byte),
        }
    }
    encoded
}

/// One `/proc/self/mountinfo` row's mount ID, device (`major:minor`), root and
/// mountpoint, the last two as the kernel escapes them.
struct MountRow {
    id: u64,
    device: Vec<u8>,
    root: Vec<u8>,
    mountpoint: Vec<u8>,
}

/// This process's mount table, as [`MountRow`]s.
fn mount_rows() -> io::Result<Vec<MountRow>> {
    let contents = fs::read("/proc/self/mountinfo")?;
    contents
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| {
            let fields = line.split(|byte| *byte == b' ').collect::<Vec<_>>();
            let id = (fields.len() > 4)
                .then(|| std::str::from_utf8(fields[0]).ok()?.parse().ok())
                .flatten();
            match id {
                Some(id) => Ok(MountRow {
                    id,
                    device: fields[2].to_vec(),
                    root: fields[3].to_vec(),
                    mountpoint: fields[4].to_vec(),
                }),
                None => Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "mountinfo has a malformed row",
                )),
            }
        })
        .collect()
}

/// This process's mount namespace, every row of it (host FUSE seed rows
/// included).
fn namespace_rows() -> Result<Vec<detcore_model::container_mounts::ContainerMount>, Error> {
    let contents = fs::read("/proc/self/mountinfo")
        .context("Failed to read the guest mount table for its container mount table")?;
    detcore_model::container_mounts::parse_container_mounts(&contents)
        .ok_or_else(|| anyhow!("the guest mount table has a malformed row"))
}

/// Whether `row`, the row the walk took for the plan's first mount, is the
/// container's own `/proc`: a procfs, and, where `/proc` reaches it, the
/// procfs of this process's PID namespace (its `self` names this process's
/// own PID, which a copy of the host's `/proc` does not). Where a later mount
/// covers `/proc` (a user's mount at `/` or `/proc`), no path reaches the row,
/// and only its filesystem type is checked.
fn is_this_namespaces_proc(row: &detcore_model::container_mounts::ContainerMount) -> bool {
    if row.fstype.as_slice() != b"proc" {
        return false;
    }
    let Ok(proc_dir) = open_path(Path::new("/proc")) else {
        return true;
    };
    if mount_id_for_fd(proc_dir.as_raw_fd()).ok() != Some(row.raw_mount_id) {
        return true;
    }
    // SAFETY: getpid has no preconditions and cannot fail.
    let me = unsafe { libc::getpid() };
    fs::read_link("/proc/self").is_ok_and(|link| link.as_os_str() == me.to_string().as_str())
}

/// Where the contents at a guest path come from, for [`InstallTimeTree`]:
/// a host directory or file, or a filesystem the plan mounted fresh (tmpfs,
/// procfs, an overlay), which holds no symbolic link a target could cross.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Backing {
    Host(PathBuf),
    Fresh,
}

/// The guest's directory tree as each mount of the plan finds it when the
/// container's setup installs it: the host's tree (which the container's
/// mount namespace starts as a copy of) with the plan's earlier mounts laid
/// over it. It resolves a target the way the kernel's `mount(2)` does at that
/// moment, following symbolic links component by component, so that the
/// mountpoint the kernel lists for each installation is known before any
/// later mount can change how the same name resolves (a later bind that
/// replaces a directory holding a symbolic link the target crossed).
#[derive(Debug, Default)]
struct InstallTimeTree {
    /// The plan's mounts so far: where each was mounted, and what it shows.
    layers: Vec<(PathBuf, Backing)>,
}

impl InstallTimeTree {
    /// The most symbolic links one resolution follows, as Linux's `MAXSYMLINKS`.
    const MAX_LINKS: usize = 40;

    /// Where the contents at the resolved guest path `path` come from: the
    /// latest of the plan's mounts at `path` or above it, or the host.
    fn backing_of(&self, path: &Path) -> Backing {
        let layer = self
            .layers
            .iter()
            .enumerate()
            .filter_map(|(index, (mountpoint, backing))| {
                let rest = path.strip_prefix(mountpoint).ok()?;
                Some((mountpoint.components().count(), index, rest, backing))
            })
            .max_by_key(|(depth, index, _, _)| (*depth, *index));
        match layer {
            None => Backing::Host(path.to_path_buf()),
            Some((_, _, rest, Backing::Host(source))) => {
                Backing::Host(if rest.as_os_str().is_empty() {
                    source.clone()
                } else {
                    source.join(rest)
                })
            }
            Some((_, _, _, Backing::Fresh)) => Backing::Fresh,
        }
    }

    /// `path` resolved as the kernel resolves it now: absolute, with every
    /// symbolic link it crosses followed (a final one too), and its missing
    /// components kept as named (Reverie creates a missing target).
    fn resolve(&self, path: &Path) -> PathBuf {
        let mut resolved = PathBuf::from("/");
        let mut pending: std::collections::VecDeque<std::ffi::OsString> = path
            .components()
            .filter_map(|component| match component {
                std::path::Component::Normal(name) => Some(name.to_os_string()),
                std::path::Component::ParentDir => Some("..".into()),
                _ => None,
            })
            .collect();
        let mut links = 0;
        while let Some(name) = pending.pop_front() {
            if name == ".." {
                resolved.pop();
                continue;
            }
            let candidate = resolved.join(&name);
            if let Backing::Host(host) = self.backing_of(&candidate)
                && links < Self::MAX_LINKS
                && fs::symlink_metadata(&host).is_ok_and(|meta| meta.file_type().is_symlink())
                && let Ok(target) = fs::read_link(&host)
            {
                links += 1;
                if target.is_absolute() {
                    resolved = PathBuf::from("/");
                }
                let mut front: std::collections::VecDeque<std::ffi::OsString> = target
                    .components()
                    .filter_map(|component| match component {
                        std::path::Component::Normal(name) => Some(name.to_os_string()),
                        std::path::Component::ParentDir => Some("..".into()),
                        _ => None,
                    })
                    .collect();
                front.extend(pending);
                pending = front;
                continue;
            }
            resolved = candidate;
        }
        resolved
    }

    /// Install `mount` in the tree, and return what the kernel will list for
    /// it: its mountpoint (none for a remount, which creates no mount) and,
    /// for a recursive bind, the resolved source whose mounts it copies.
    fn install(&mut self, mount: &Mount) -> (Option<PathBuf>, Option<PathBuf>) {
        if mount.clone().flags(MountFlags::MS_REMOUNT) == *mount {
            return (None, None);
        }
        let mountpoint = self.resolve(mount.get_target());
        let (backing, source) = match mount.get_source().filter(|_| is_bind_mount(mount)) {
            Some(source) => {
                let source = self.resolve(source);
                let recursive = mount.clone().flags(MountFlags::MS_REC) == *mount;
                (self.backing_of(&source), recursive.then_some(source))
            }
            None => (Backing::Fresh, None),
        };
        self.layers.push((mountpoint.clone(), backing));
        (Some(mountpoint), source)
    }
}

/// What the kernel will list for each mount of `plan`, as the container's
/// setup installs them in order over the host's current tree
/// ([`InstallTimeTree`]), as installations with no rows yet.
fn installations_of(plan: &[PlannedMount]) -> Vec<Installation> {
    let mut tree = InstallTimeTree::default();
    plan.iter()
        .map(|planned| {
            let (mountpoint, source) = tree.install(&planned.mount);
            Installation {
                ids: Vec::new(),
                mountpoints: mountpoint
                    .iter()
                    .map(|path| mountinfo_escape_path(path))
                    .collect(),
                sources: source
                    .iter()
                    .map(|path| mountinfo_escape_path(path))
                    .collect(),
                backend: planned.backend,
            }
        })
        .collect()
}

/// Whether `field` is one Detcore's mountinfo sanitizer accepts: an absolute
/// path with no ASCII control byte or space (the kernel escapes space, tab,
/// newline and backslash; other control bytes it leaves, and the sanitizer
/// refuses them).
fn mountinfo_field_is_canonical(field: &[u8]) -> bool {
    field.first() == Some(&b'/')
        && field
            .iter()
            .all(|byte| !byte.is_ascii_control() && *byte != b' ')
}

/// A user's bind mount (`--bind`, or `--mount` of type bind), whose
/// mountinfo rows show its target as their root rather than the host source
/// path, which differs from run to run and host to host
/// (https://github.com/rrnewton/hermit/issues/4266).
///
/// Its rows are those, in the completed guest namespace, at a mountpoint the
/// kernel may show for the bind that are not the guest's copies of the host's
/// own mounts ([`IdentityGuard::host_rows`]) and do not show a filesystem's
/// own root, `/`, which names no host path (a mount of another type there,
/// such as a tmpfs, keeps it). The mountpoints are the target as named, as
/// the host resolves it before setup (earlier mounts may change that
/// resolution), and as the guest resolves it after, and, for a bind under
/// `/tmp`, its staging path in Hermit's private `/tmp`. A row at a bind's
/// target or staging path as named belongs to that bind before any other
/// bind's resolved spelling claims it. Every layer of the bind at its
/// mountpoint matches, including one a later mount covers, whatever path
/// the source was reached through. No row at another mountpoint changes. It
/// claims no Hermit ownership: the root is the guest's own path, not a
/// `/tmpvol/.hermit` name.
struct UserBind {
    /// The guest path the user named, which its rows show as their root.
    target: PathBuf,
    /// The mountpoints, escaped, at which the bind is installed as named:
    /// its target and, under `/tmp`, its staging path.
    named: Vec<Vec<u8>>,
    /// Further mountpoints, escaped, the kernel may show for the bind, as the
    /// host resolves the named ones before setup.
    resolved: Vec<Vec<u8>>,
}

impl UserBind {
    /// The bind at guest path `target`, staged at `staging` for a bind under
    /// `/tmp`. `None` for a target this cannot show as a root: a relative
    /// one, one with a `..`, or one the mountinfo grammar cannot carry.
    fn new(target: &Path, staging: Option<&Path>) -> Option<Self> {
        let named = |path: &Path| {
            path.is_absolute()
                && !path
                    .components()
                    .any(|component| component == std::path::Component::ParentDir)
        };
        if !named(target) || staging.is_some_and(|staging| !named(staging)) {
            return None;
        }
        // As the kernel spells a mountpoint: no `.` and no trailing `/` (a
        // bind at exactly `/tmp` is staged at the private tmp directory
        // itself).
        let target: PathBuf = target.components().collect();
        if !mountinfo_field_is_canonical(&mountinfo_escape_path(&target)) {
            return None;
        }
        let staging: Option<PathBuf> = staging.map(|staging| staging.components().collect());
        let mut named = vec![mountinfo_escape_path(&target)];
        named.extend(staging.as_deref().map(mountinfo_escape_path));
        let mut resolved = vec![mountinfo_escape_path(&canonical_mount_target(&target))];
        resolved.extend(
            staging
                .as_deref()
                .map(|staging| mountinfo_escape_path(&canonical_mount_target(staging))),
        );
        resolved.retain(|mountpoint| !named.contains(mountpoint));
        Some(Self {
            target,
            named,
            resolved,
        })
    }

    /// The rows of the completed guest namespace `rows` at this bind's
    /// mountpoints as named (`named`) or as resolved, which are neither the
    /// host's own (`host_rows`) nor a filesystem's root.
    fn rows<'rows>(
        &self,
        rows: &'rows [MountRow],
        host_rows: &[(Vec<u8>, Vec<u8>, Vec<u8>)],
        named: bool,
    ) -> impl Iterator<Item = &'rows MountRow> {
        let mountpoints = if named {
            self.named.clone()
        } else {
            let mut resolved = self.resolved.clone();
            // The kernel shows a mountpoint with its symbolic links resolved.
            if let Ok(target) = fs::canonicalize(&self.target) {
                resolved.push(mountinfo_escape_path(&target));
            }
            resolved
        };
        let host_rows = host_rows.to_vec();
        rows.iter().filter(move |row| {
            mountpoints.contains(&row.mountpoint)
                && row.root != b"/"
                && !host_rows.iter().any(|(mountpoint, root, device)| {
                    *mountpoint == row.mountpoint && *root == row.root && *device == row.device
                })
        })
    }
}

/// Say that a user bind's mountinfo rows keep the host source path as their
/// root: Hermit could not identify them, so the guest can see that path.
fn warn_unrewritten_bind(target: &Path) {
    crate::tracing::diagnostic_eprintln!(
        "WARNING: /proc/self/mountinfo shows the host source path of the bind at {}: \
         Hermit could not identify its rows (https://github.com/rrnewton/hermit/issues/4266).",
        target.display()
    );
}

impl MountInfoRootSource {
    fn new(source: &Path, target: &Path, deterministic_root: &[u8]) -> Result<Self, Error> {
        Ok(Self {
            source: Some(open_path(source).with_context(|| {
                format!(
                    "Failed to pin mountinfo provenance source {}",
                    source.display()
                )
            })?),
            target: target.to_path_buf(),
            deterministic_root: Some(deterministic_root.to_vec()),
            rewrite_descendant_roots: false,
            raw_mountpoint_prefix: None,
            deterministic_mountpoint_prefix: None,
        })
    }

    fn private_tmp(source: &Path) -> Result<Self, Error> {
        let mut provenance = Self::new(source, Path::new("/tmp"), DETERMINISTIC_TMP_ROOT)?;
        provenance.rewrite_descendant_roots = true;
        provenance.raw_mountpoint_prefix = Some(mountinfo_escape_path(source));
        provenance.deterministic_mountpoint_prefix = Some(b"/tmp".to_vec());
        Ok(provenance)
    }

    /// Preserve a user-provided `/tmp` mount's own root while translating the
    /// randomly named staging mountpoint back to the guest path. This does not
    /// claim Hermit ownership of the mounted filesystem: the exact top mount is
    /// selected by a held target descriptor after setup, and only field 5's
    /// internal staging prefix is rewritten.
    fn translated_tmp_mountpoint(staging_path: &Path) -> Self {
        Self {
            source: None,
            target: PathBuf::from("/tmp"),
            deterministic_root: None,
            rewrite_descendant_roots: false,
            raw_mountpoint_prefix: Some(mountinfo_escape_path(staging_path)),
            deterministic_mountpoint_prefix: Some(b"/tmp".to_vec()),
        }
    }

    fn resolve(&self) -> Result<MountInfoRootRewrite, Error> {
        let target = open_path(&self.target).with_context(|| {
            format!(
                "Failed to pin mountinfo provenance target {}",
                self.target.display()
            )
        })?;
        let target_metadata = target.metadata().with_context(|| {
            format!(
                "Failed to inspect pinned mountinfo provenance target {}",
                self.target.display()
            )
        })?;
        if let Some(source) = &self.source {
            let source_metadata = source.metadata().with_context(|| {
                format!(
                    "Failed to inspect pinned mountinfo provenance source for {}",
                    self.target.display()
                )
            })?;
            if (source_metadata.dev(), source_metadata.ino())
                != (target_metadata.dev(), target_metadata.ino())
            {
                return Err(Error::msg(format!(
                    "Refusing mountinfo provenance for {}: the completed mount target does not match \
                     the pinned Hermit-owned source",
                    self.target.display()
                )));
            }
        }
        let raw_mount_id = mount_id_for_fd(target.as_raw_fd()).with_context(|| {
            format!(
                "Failed to identify the mount containing proven target {}",
                self.target.display()
            )
        })?;
        let observed_root = (self.rewrite_descendant_roots || self.deterministic_root.is_none())
            .then(|| mountinfo_root_for_id(raw_mount_id))
            .transpose()
            .with_context(|| {
                format!(
                    "Failed to identify the proven mount root for {}",
                    self.target.display()
                )
            })?;
        let deterministic_root = self
            .deterministic_root
            .clone()
            .or_else(|| observed_root.clone())
            .expect("an explicit or observed root is required");
        Ok(MountInfoRootRewrite {
            raw_mount_id,
            deterministic_root: deterministic_root.clone(),
            raw_root_prefix: self.rewrite_descendant_roots.then(|| {
                observed_root
                    .clone()
                    .expect("descendant rewriting captured the observed root")
            }),
            deterministic_root_prefix: self.rewrite_descendant_roots.then_some(deterministic_root),
            raw_mountpoint_prefix: self.raw_mountpoint_prefix.clone(),
            deterministic_mountpoint_prefix: self.deterministic_mountpoint_prefix.clone(),
        })
    }
}

// Bind mount sources must outlive Reverie's pre-exec container setup, which
// applies the mounts in the forked child before exec. Hold this guard in the
// caller until after `Container::run` returns so the backing temp files still
// exist when the child binds them.
pub(super) struct IdentityGuard {
    _group_file: Option<tempfile::NamedTempFile>,
    _nscd_dir: Option<tempfile::TempDir>,
    mountinfo_roots: Vec<MountInfoRootSource>,
    user_mount_targets: Vec<PathBuf>,
    user_binds: Vec<UserBind>,
    /// The host's mount table before setup, as (mountpoint, root, device):
    /// the guest's copies of these rows are not any user bind's.
    host_rows: Vec<(Vec<u8>, Vec<u8>, Vec<u8>)>,
    /// The container's mount plan, which the container's setup installs, in
    /// order, as the last of its mounts, and what each installation created,
    /// recorded in the container's init before the guest starts
    /// ([`Self::record_plan`]).
    plan: MountPlan,
}

/// One mount of the container's mount plan.
#[derive(Clone, Debug)]
pub(super) struct PlannedMount {
    /// The mount, at the target it is installed at (a mount below guest
    /// `/tmp` is installed in Hermit's private tmp directory first).
    pub(super) mount: Mount,
    /// Whether it is a backend's own mount (DBT's statistics bind, e9patch's
    /// overlay of the rewritten program), which the guest's mount table leaves
    /// out by this identity, not by its path. DBT's tmpfs at the test workdir
    /// is not one: it is the guest's working directory, shown as the caller's
    /// mount there is on every other backend.
    pub(super) backend: bool,
}

impl PlannedMount {
    /// One of the plan's own mounts.
    pub(super) fn hermit(mount: Mount) -> Self {
        Self {
            mount,
            backend: false,
        }
    }

    /// A backend's own mount.
    pub(super) fn backend(mount: Mount) -> Self {
        Self {
            mount,
            backend: true,
        }
    }
}

/// Where the guest's container mount table learns which mounts are Hermit's.
#[derive(Debug, Default)]
enum MountPlan {
    /// No table: the container's mounts were installed by its setup, without
    /// a record of what each one made (a native exec domain's container, whose
    /// declared-input mounts must follow the plan's in that setup).
    #[default]
    Unrecorded,
    /// The plan, given to the container's setup to install; what the kernel
    /// will list for each of its mounts, resolved over the host's tree before
    /// the container starts ([`installations_of`]); and, once the rows each
    /// installation created are recorded in them
    /// ([`IdentityGuard::record_plan`]), the namespace's rows as read then,
    /// which nothing changes afterwards (a container's namespace can hold
    /// hundreds of rows, so it is read and parsed once per launch).
    Recorded(
        Vec<PlannedMount>,
        Vec<Installation>,
        Option<Vec<detcore_model::container_mounts::ContainerMount>>,
    ),
    /// An image root (`--image`): every mount the guest's root can see is one
    /// the container's setup installed into the image, so all are Hermit's.
    ImageRoot,
}

impl IdentityGuard {
    /// A guard that owns no backing temp files, for container configurations
    /// (e.g. `--image`) that supply their filesystem from another source and do
    /// not use the frozen-identity bind mounts.
    pub(super) fn empty() -> Self {
        Self {
            _group_file: None,
            _nscd_dir: None,
            mountinfo_roots: Vec::new(),
            user_mount_targets: Vec::new(),
            user_binds: Vec::new(),
            host_rows: Vec::new(),
            plan: MountPlan::Unrecorded,
        }
    }

    /// Give `plan` to `container`'s setup, which installs it after every
    /// mount the container already has, and keep it to record what each
    /// mount makes ([`Self::record_plan`]). The plan's first mount must be
    /// the container's own `/proc` ([`container_proc_mount`]), whose row
    /// marks where the plan's rows begin. Mounts the container already has
    /// come before it: `/` made private, and for `--network=local` Reverie's
    /// fresh `/sys`, which rule 2 shows as a base mountpoint.
    pub(super) fn set_plan(
        &mut self,
        container: &mut Container,
        plan: Vec<PlannedMount>,
    ) -> Result<(), Error> {
        if plan.first().map(|planned| &planned.mount) != Some(&container_proc_mount()) {
            anyhow::bail!("a container's mount plan must begin with its own /proc");
        }
        container.mounts(plan.iter().map(|planned| planned.mount.clone()));
        let installations = installations_of(&plan);
        self.plan = MountPlan::Recorded(plan, installations, None);
        Ok(())
    }

    /// Append `mounts`, a backend's own, to the plan and to `container`'s
    /// setup. There must be a plan, not yet recorded: a backend's mount
    /// dropped silently would run the guest without it.
    pub(super) fn add_backend_mounts(
        &mut self,
        container: &mut Container,
        mounts: impl IntoIterator<Item = Mount>,
    ) -> Result<(), Error> {
        let MountPlan::Recorded(plan, installations, None) = &mut self.plan else {
            anyhow::bail!("a backend's own mounts need the container's mount plan");
        };
        for mount in mounts {
            container.mount(mount.clone());
            plan.push(PlannedMount::backend(mount));
        }
        *installations = installations_of(plan);
        Ok(())
    }

    /// The container's mounts were installed into an image root, every one
    /// of them Hermit's.
    pub(super) fn set_image_root(&mut self) {
        self.plan = MountPlan::ImageRoot;
    }

    // TODO-HUMAN-REVIEW(https://github.com/rrnewton/hermit/issues/4213):
    // Review the provenance of the container mount table's rule 1: the mount
    // IDs each installation of the plan created.
    /// Record what each mount of the plan created, in the container's init,
    /// after its setup installed the plan and before the guest starts or
    /// anything else changes the namespace (the guest cannot mount). The
    /// setup installs the plan with Reverie's own mount operation, one mount
    /// after another, as its last mounts, and the kernel lists rows in
    /// creation order. The premise: the setup makes `/` recursively private
    /// (`namespace_container`) before the plan's first mount, the
    /// container's `/proc`, so no host mount or unmount propagates into the
    /// namespace after that, and no row can follow that `/proc` but the
    /// plan's own. So the plan's rows are the end of the namespace's list,
    /// each installation's together and in plan order:
    /// `detcore_model::container_mounts::assign_installed_rows` walks them
    /// back from the end, giving each to the installation that made it (a
    /// recursive bind's copies included), until it reaches the plan's first
    /// row, the container's `/proc`. A row is matched by the mountpoint its
    /// installation's target resolved to when the setup installed it
    /// ([`installations_of`], resolved before the container starts), never by
    /// how the target resolves after later mounts. Then
    /// `check_installed_rows` refuses the record unless every mount that
    /// creates a row got one at its recorded target and the first row is
    /// this namespace's own procfs ([`is_this_namespaces_proc`]): a walk that
    /// ran past the plan into the host's rows (which hold a copy of the
    /// host's `/proc` at the same path) is refused, not trusted. No process
    /// is created for this, so no guest PID moves. Nothing for a plan with
    /// no record.
    pub(super) fn record_plan(&mut self) -> Result<(), Error> {
        let MountPlan::Recorded(_, installations, recorded) = &mut self.plan else {
            return Ok(());
        };
        if recorded.is_some() {
            return Ok(());
        }
        let rows = namespace_rows()?;
        if !assign_installed_rows(&rows, installations) {
            anyhow::bail!(
                "the container's mount table has no row for the plan's first mount, its /proc, \
                 so the plan's rows cannot be told from the host's"
            );
        }
        check_installed_rows(&rows, installations, is_this_namespaces_proc)
            .map_err(|refusal| anyhow!("refusing the container's mount table: {refusal}"))?;
        *recorded = Some(rows);
        Ok(())
    }

    /// Forget the record of a plan whose container's setup is followed by
    /// more mounts (a native exec domain's declared inputs), so that the
    /// plan's rows are not the end of the namespace's list: no table.
    #[cfg(feature = "native-exec")]
    pub(super) fn forget_plan(&mut self) {
        self.plan = MountPlan::Unrecorded;
    }

    /// The guest's container mount table
    /// (`detcore_model::container_mounts::ContainerMountTable::select`), read
    /// from this process's mount namespace, which must be the completed,
    /// quiescent guest namespace (with the guest's root), after
    /// [`Self::record_plan`]. `None` where the plan has no record
    /// ([`MountPlan::Unrecorded`]).
    ///
    /// Rule 1's rows are those the installations recorded
    /// (`detcore_model::container_mounts::owned_mounts`), a backend's own
    /// excepted, and every row under an image root. Rule 2's are the top
    /// mounts at the base mountpoints, proven by a descriptor held on each.
    /// Host FUSE seed rows stay in the topology, for ancestry and for the
    /// mount a file on one reports, and are only never shown
    /// (`is_ephemeral_host_seed`), unless the plan installed one.
    pub(super) fn container_mount_table(&self) -> Result<Option<ContainerMountTable>, Error> {
        let (rows, installed, backend) = match &self.plan {
            MountPlan::Unrecorded => return Ok(None),
            MountPlan::Recorded(_, _, None) => {
                anyhow::bail!(
                    "the container mount table was asked for before the plan was recorded"
                )
            }
            MountPlan::Recorded(_, installations, Some(rows)) => {
                let (installed, backend) = owned_mounts(rows, installations);
                (std::borrow::Cow::Borrowed(rows), installed, backend)
            }
            MountPlan::ImageRoot => {
                let rows = namespace_rows()?;
                let installed = rows.iter().map(|row| row.raw_mount_id).collect();
                (
                    std::borrow::Cow::Owned(rows),
                    installed,
                    std::collections::BTreeSet::new(),
                )
            }
        };
        let never = rows
            .iter()
            .filter(|row| row.is_ephemeral_host_seed() && !installed.contains(&row.raw_mount_id))
            .map(|row| row.raw_mount_id)
            .chain(backend)
            .collect();
        let reached = |path: &Path| {
            open_path(path)
                .ok()
                .and_then(|file| mount_id_for_fd(file.as_raw_fd()).ok())
        };
        let base = BASE_MOUNTPOINTS
            .iter()
            .filter_map(|mountpoint| {
                let path = Path::new(mountpoint);
                Some((mountinfo_escape_path(path), reached(path)?))
            })
            .collect::<Vec<_>>();
        Ok(Some(ContainerMountTable::select(
            &rows, &installed, &base, &never,
        )))
    }

    /// Include an automatically-created private `/tmp` in mountinfo provenance.
    /// User-supplied `--tmp` paths deliberately do not call this method.
    pub(super) fn add_private_tmp(&mut self, source: &Path) -> Result<(), Error> {
        self.mountinfo_roots
            .push(MountInfoRootSource::private_tmp(source)?);
        Ok(())
    }

    /// Show a user's bind at guest path `target` (staged at `staging` for a
    /// bind under `/tmp`) with the target as its mountinfo root
    /// ([`UserBind`]).
    pub(super) fn add_user_bind(&mut self, target: &Path, staging: Option<&Path>) {
        if self.user_binds.is_empty() {
            match mount_rows() {
                Ok(rows) => {
                    self.host_rows = rows
                        .into_iter()
                        .map(|row| (row.mountpoint, row.root, row.device))
                        .collect();
                }
                Err(_) => return warn_unrewritten_bind(target),
            }
        }
        match UserBind::new(target, staging) {
            Some(bind) => self.user_binds.push(bind),
            None => warn_unrewritten_bind(target),
        }
    }

    /// Translate the internal staging path used for an explicit user mount at
    /// `/tmp`, without rewriting the user mount's root or claiming it as a
    /// Hermit-owned filesystem.
    pub(super) fn add_translated_tmp_mountpoint(&mut self, staging_path: &Path) {
        self.mountinfo_roots
            .push(MountInfoRootSource::translated_tmp_mountpoint(staging_path));
    }

    /// Drop provenance for a Hermit mount hidden by an explicit user mount.
    /// The hidden mount must also be omitted from the mount plan: otherwise its
    /// private root would remain visible as a lower stacked mountinfo row even
    /// though no pathname could reopen that exact layer afterward.
    pub(super) fn discard_mounts_shadowed_by(
        &mut self,
        mounts: &mut Vec<Mount>,
        overriding_target: &Path,
    ) {
        // Host canonicalization describes the namespace only until the first
        // user mount that can replace an ancestor of this target. After that,
        // pathname resolution must follow the ordered mount plan rather than
        // the launcher's host tree. For example, mounting a new `/var` changes
        // what a later `/var/run/nscd` names and must not remove the earlier
        // hardening mount installed at the host-resolved `/run/nscd`.
        let resolution_changed = self
            .user_mount_targets
            .iter()
            .any(|earlier| overriding_target.starts_with(earlier));
        let resolved_override = if resolution_changed {
            overriding_target.to_path_buf()
        } else {
            canonical_mount_target(overriding_target)
        };
        mounts.retain(|mount| {
            !canonical_mount_target(mount.get_target()).starts_with(&resolved_override)
        });
        self.mountinfo_roots.retain(|source| {
            !canonical_mount_target(&source.target).starts_with(&resolved_override)
        });
        self.user_mount_targets
            .push(overriding_target.to_path_buf());
    }

    /// Resolve proven source objects to exact mount IDs in the completed guest
    /// namespace.  This runs after all mounts are installed and before the guest
    /// starts, while the namespace is quiescent.  Held source and target
    /// descriptors plus the target fd's `mnt_id` avoid both pathname TOCTOU and
    /// ambiguity from stacked mounts sharing one textual mountpoint.
    pub(super) fn mountinfo_root_rewrites(&self) -> Result<Vec<MountInfoRootRewrite>, Error> {
        let mut rewrites = self
            .mountinfo_roots
            .iter()
            .map(MountInfoRootSource::resolve)
            .collect::<Result<Vec<_>, _>>()?;
        if self.user_binds.is_empty() {
            return Ok(rewrites);
        }
        let rows = mount_rows().context("Failed to read the guest mount table")?;
        // A row at a bind's target or staging path as named is that bind's;
        // only a row no bind names may go to a bind's resolved spelling.
        let mut found = std::collections::BTreeMap::<u64, Vec<u8>>::new();
        for named in [true, false] {
            for bind in &self.user_binds {
                let root = mountinfo_escape_path(&bind.target);
                for row in bind.rows(&rows, &self.host_rows, named) {
                    found.entry(row.id).or_insert_with(|| root.clone());
                }
            }
        }
        for bind in &self.user_binds {
            let root = mountinfo_escape_path(&bind.target);
            if !found.values().any(|found| *found == root) {
                warn_unrewritten_bind(&bind.target);
            }
        }
        for (raw_mount_id, deterministic_root) in found {
            // A row already carries a rewrite as one of Hermit's own mounts,
            // which keeps its fixed name, or as the staging translation of a
            // user mount at `/tmp`, which kept the mount's root: that root
            // becomes the target. Two binds at one mountpoint name the same
            // rows with the same root.
            match rewrites
                .iter_mut()
                .find(|rewrite| rewrite.raw_mount_id == raw_mount_id)
            {
                Some(rewrite) if rewrite.deterministic_root.starts_with(b"/tmpvol/.hermit/") => {}
                Some(rewrite) => rewrite.deterministic_root = deterministic_root,
                None => rewrites.push(MountInfoRootRewrite {
                    raw_mount_id,
                    deterministic_root,
                    raw_root_prefix: None,
                    deterministic_root_prefix: None,
                    raw_mountpoint_prefix: None,
                    deterministic_mountpoint_prefix: None,
                }),
            }
        }
        Ok(rewrites)
    }

    /// What a recording takes from the completed, quiescent recording
    /// namespace: the proven roots of Hermit's mounts
    /// ([`Self::mountinfo_root_rewrites`]) and the guest's container mount
    /// table ([`Self::container_mount_table`]).
    pub(super) fn mount_provenance(&self) -> Result<hermit::MountProvenance, Error> {
        Ok(hermit::MountProvenance {
            root_rewrites: self.mountinfo_root_rewrites()?,
            container_mount_table: self.container_mount_table()?,
        })
    }

    /// Capture the exact raw mount-ID order used by Detcore's snapshot-local
    /// canonicalizer. This runs in the completed, quiescent guest namespace.
    /// A row `table` shows stays in it whatever its name
    /// (`hermit::capture_mountinfo_identity_order_keeping`).
    pub(super) fn mountinfo_identity_order(
        &self,
        table: Option<&ContainerMountTable>,
    ) -> Result<Vec<u64>, Error> {
        capture_mountinfo_identity_order_keeping(
            &table
                .map(ContainerMountTable::shown_mount_ids)
                .unwrap_or_default(),
        )
        .context("Failed to capture guest mountinfo identity order")
    }
}

/// Whether `mount` is a bind mount: `--mount` without a `type`, or of type
/// `bind`, which sets `MS_BIND` (flags only accumulate, so adding it again
/// changes nothing exactly when it is already there).
pub(super) fn is_bind_mount(mount: &Mount) -> bool {
    mount.clone().flags(MountFlags::MS_BIND) == *mount
}

fn canonical_mount_target(path: &Path) -> PathBuf {
    for ancestor in path.ancestors() {
        if let Ok(mut canonical) = fs::canonicalize(ancestor)
            && let Ok(suffix) = path.strip_prefix(ancestor)
        {
            canonical.push(suffix);
            return canonical;
        }
    }
    path.to_path_buf()
}

#[cfg(test)]
pub(super) fn mount_target_is_shadowed(target: &Path, overriding_target: &Path) -> bool {
    canonical_mount_target(target).starts_with(canonical_mount_target(overriding_target))
}

/// The overflow group ID of the guest's machine (`kernel/overflowgid`), which
/// the frozen group database must name. It does not depend on the run's
/// configuration.
fn overflow_gid() -> String {
    detcore::machine::MachineDescription::default()
        .limits
        .overflow_gid
        .to_string()
}

/// Snapshot the host group database into a private temp file, appending a
/// synthetic overflow group (`nobody:x:<overflow gid>`) when the host lacks
/// one. Binding this frozen copy read-only over `/etc/group` keeps guest
/// group-name resolution stable across otherwise-identical runs.
fn frozen_group_file() -> Result<tempfile::NamedTempFile, Error> {
    let overflow_gid = overflow_gid();
    let mut contents = fs::read_to_string(GROUP_FILE)
        .context("Failed to read the host group database for the guest")?;
    let has_overflow_group = contents.lines().any(|line| {
        line.split(':')
            .nth(2)
            .is_some_and(|gid| gid == overflow_gid)
    });
    if !has_overflow_group {
        if !contents.ends_with('\n') {
            contents.push('\n');
        }
        contents.push_str("nobody:x:");
        contents.push_str(&overflow_gid);
        contents.push_str(":\n");
    }

    let mut group_file = tempfile::NamedTempFile::new()
        .context("Failed to create the frozen group database for the guest")?;
    group_file
        .write_all(contents.as_bytes())
        .context("Failed to populate the frozen group database for the guest")?;
    group_file
        .as_file()
        .set_permissions(fs::Permissions::from_mode(0o644))
        .context("Failed to set permissions on the frozen guest group database")?;
    Ok(group_file)
}

/// Deterministic identity-resolution mounts shared by `run`, `record`, and
/// `replay`: a frozen `/etc/group` and an empty directory over the host nscd
/// cache. These keep guest NSS lookups from reaching nondeterministic host
/// state (the nscd cache and the systemd-userdb socket), so record/replay
/// reproduce the same group/user resolution that `run` mode already enforces.
/// Returns the mounts plus a guard that must outlive container setup.
pub(super) fn identity_hardening_mounts() -> Result<(Vec<Mount>, IdentityGuard), Error> {
    let group_file = frozen_group_file()?;
    let mut mountinfo_roots = vec![MountInfoRootSource::new(
        group_file.path(),
        Path::new(GROUP_FILE),
        DETERMINISTIC_GROUP_ROOT,
    )?];
    let mut mounts = vec![Mount::bind(group_file.path(), GROUP_FILE).readonly()];

    // Host nscd cache readiness is external state and can differ between runs.
    let nscd_dir = if Path::new(NSCD_DIR).is_dir() {
        let directory =
            tempfile::TempDir::new().context("Failed to create the empty guest nscd directory")?;
        // Preserve Linux pathname resolution: on distributions where
        // /var/run is a symlink to /run, mounting the hardening directory at
        // the resolved target keeps it reachable even if a user later mounts
        // an unrelated /var tree.
        let target = canonical_mount_target(Path::new(NSCD_DIR));
        mountinfo_roots.push(MountInfoRootSource::new(
            directory.path(),
            &target,
            DETERMINISTIC_NSCD_ROOT,
        )?);
        mounts.push(Mount::bind(directory.path(), target).readonly());
        Some(directory)
    } else {
        None
    };

    // The container's own `/proc` (`default_container`) and the mounts above.
    Ok((
        mounts,
        IdentityGuard {
            _group_file: Some(group_file),
            _nscd_dir: nscd_dir,
            mountinfo_roots,
            user_mount_targets: Vec::new(),
            user_binds: Vec::new(),
            host_rows: Vec::new(),
            plan: MountPlan::Unrecorded,
        },
    ))
}

fn choose_affinity_core(allowed: &[usize]) -> Option<usize> {
    (!allowed.is_empty()).then(|| allowed[rand::random_range(0..allowed.len())])
}

fn cpu_ids(affinity: &CpuSet) -> Vec<usize> {
    (0..CpuSet::count())
        .filter(|cpu| affinity.is_set(*cpu).unwrap_or(false))
        .collect()
}

pub(super) fn apply_affinity(container: &mut Container, pin_threads: bool) {
    if pin_threads {
        let affinity = sched_getaffinity(Pid::from_raw(0))
            .expect("failed to query the tracer's allowed CPU affinity mask");
        let allowed = cpu_ids(&affinity);
        let rand_core = choose_affinity_core(&allowed)
            .expect("the tracer's allowed CPU affinity mask is empty");
        tracing::info!("Pinning tracer and guest threads to core {}", rand_core);
        container.affinity(rand_core);
    }
}

pub fn default_container(pin_threads: bool) -> Container {
    let mut container = namespace_container(pin_threads);
    container.mount(container_proc_mount());
    container
}

/// [`default_container`] without its `/proc`: the namespaces and the private
/// `/`, for a container given a mount plan, `/proc` first, which its setup
/// installs and its init records (`IdentityGuard::set_plan`).
pub(super) fn namespace_container(pin_threads: bool) -> Container {
    let mut container = Container::new();
    container
        .unshare(Namespace::PID)
        .map_root()
        .hostname("hermetic-container.local")
        .domainname("local")
        // A mount namespace created inside a user namespace starts as a slave
        // of the host's, so host mounts and unmounts would keep changing the
        // guest's mount table during a run. Cut that propagation first.
        .mount(Mount::new("/").rprivate());

    apply_affinity(&mut container, pin_threads);
    container
}

/// The container's own `/proc`, a new procfs of its PID namespace.
pub(super) fn container_proc_mount() -> Mount {
    Mount::proc().allow_readonly_fallback()
}

/// PROTOTYPE: a container whose root filesystem is a materialized OCI image
/// rootfs. This is the *filesystem half* of hermit-as-container-runtime: the
/// guest's file inputs come deterministically from the pinned image rather than
/// from the host filesystem.
///
/// Like the replay chroot path (`replay.rs`), mounts are applied at their
/// literal (pre-chroot) target paths and then `chroot(rootfs)` makes them
/// visible under the new root. We therefore mount the deterministic `/proc`
/// *into* `<rootfs>/proc` (pre-created by the materializer) before chrooting, so
/// the guest sees a `/proc` after entering the image root. The image already
/// carries its own `/etc/group`, loader, and libc — pinned by the image digest —
/// so the frozen-identity hardening mounts that `run` normally adds are
/// unnecessary here. The returned [`IdentityGuard`] still records an
/// automatically-created private `/tmp`; a user-supplied `--tmp` remains
/// unclaimed.
///
/// The CLI currently enables this only for the ptrace backend. Other backends
/// have distinct launch/runtime-file requirements and must be qualified before
/// they can safely share this filesystem setup.
pub(super) fn image_container(
    rootfs: &Path,
    tmpfs: &Path,
    pin_threads: bool,
    private_tmp: bool,
) -> Result<(Container, IdentityGuard), Error> {
    let mut container = Container::new();
    container
        .unshare(Namespace::PID)
        .map_root()
        .hostname("hermetic-container.local")
        .domainname("local")
        // As in `default_container`: stop host mount propagation.
        .mount(Mount::new("/").rprivate());

    // The cache is a deterministic input, not a writable container layer. Bind
    // it onto itself and remount it read-only so a guest cannot poison later
    // runs or mutate run one underneath `--verify` run two. A fresh writable
    // /tmp is mounted separately for ordinary scratch files.
    //
    // Use Reverie's read-only bind rather than a hand-written
    // `MS_BIND | MS_REMOUNT | MS_RDONLY`. When the cache lives on a filesystem
    // mounted `nosuid`, `nodev` or `noexec` outside this user namespace, the
    // kernel locks those flags on the copied mount and refuses (EPERM) any
    // remount that omits them. Reverie restates the source's locked flags in
    // its read-only remount; the hand-written remount did not, so every image
    // run failed at spawn under such a cache.
    container.mount(Mount::bind(rootfs, rootfs).readonly());
    container.mount(Mount::bind(tmpfs, rootfs.join("tmp")).rshared());

    // Mount the deterministic /proc into the target root. The materializer
    // guarantees <rootfs>/proc exists, so we do not need `touch_target()` (which
    // defers dir creation to the pre-exec child on a tiny clone stack).
    let proc_target = rootfs.join("proc");
    container.mount(Mount::proc().allow_readonly_fallback().target(&proc_target));

    // A minimal /dev. An OCI image layer ships no device nodes, so without this
    // the guest sees an empty /dev on a read-only root: `> /dev/null` fails with
    // EROFS and anything wanting a pty fails outright.
    //
    // Every node here is one whose contents a guest cannot use to observe the
    // host:
    //
    // * `null`, `zero` and `full` have kernel-defined, constant behaviour.
    // * `random` and `urandom` are bound only so that `open(2)` finds an inode.
    //   Detcore classifies these two by the path the guest opened
    //   (`FdType::Rng`, see `detcore/src/syscalls/files.rs`) and serves reads
    //   from its deterministic PRNG, so the host entropy pool is never read.
    //   Binding the host node therefore restores functionality without handing
    //   the guest host entropy -- but it does mean the determinism of these two
    //   rests on Detcore's path classification, not on the mount.
    //
    // Deliberately absent:
    //
    // * `/dev/tty` -- its contents *are* the caller's controlling terminal, so it
    //   is pure host coupling, and the guest's behaviour would depend on whether
    //   Hermit was invoked from a terminal or a pipe. Removing exactly that kind
    //   of environmental coupling is the point of image mode. Absent,
    //   `open("/dev/tty")` reports ENOENT instead of ENXIO; both mean "no
    //   controlling terminal".
    // * `/dev/shm` -- a writable shared-memory area is cross-process shared state
    //   and a determinism question in its own right, and nothing in the current
    //   image workloads needs it.
    let dev_root = rootfs.join("dev");
    for node in crate::image::DEV_BIND_TARGETS {
        container.mount(Mount::bind(
            Path::new("/dev").join(node),
            dev_root.join(node),
        ));
    }

    // A FRESH devpts instance, not a bind of the host's. `newinstance` numbers
    // this container's ptys from 0 independently of the host, which is both the
    // isolation-correct and the deterministic choice: binding the host
    // `/dev/ptmx` would allocate out of the host's devpts and leak host-global
    // pty numbers into the guest. `ptmxmode=0666` makes the instance's own
    // `pts/ptmx` usable, which is what the `/dev/ptmx -> pts/ptmx` symlink the
    // materializer creates points at.
    container.mount(
        Mount::devpts(dev_root.join("pts"))
            .data("newinstance,ptmxmode=0666")
            .flags(MountFlags::MS_NOSUID | MountFlags::MS_NOEXEC),
    );

    // Enter the image root last, mirroring the replay chroot ordering
    // (mounts first, then chroot).
    container.chroot(rootfs);

    apply_affinity(&mut container, pin_threads);
    let mut identity_sources = IdentityGuard::empty();
    if private_tmp {
        identity_sources.add_private_tmp(tmpfs)?;
    }
    identity_sources.set_image_root();
    Ok((container, identity_sources))
}

/// Common declared-input root setup. This is filesystem setup data, never an
/// InstalledExecDomain, NativeExecProof, or permission to invoke an exec leaf.
#[cfg(feature = "native-exec")]
pub(super) mod declared_inputs {
    use std::collections::BTreeMap;
    use std::collections::BTreeSet;
    use std::ffi::CString;
    use std::fs::File;
    use std::os::fd::AsFd;
    use std::os::fd::AsRawFd;
    use std::os::fd::BorrowedFd;
    use std::os::fd::FromRawFd;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::FileExt;
    use std::os::unix::fs::MetadataExt;
    use std::path::Path;
    use std::sync::Arc;

    use anyhow::Context;
    use anyhow::Result;
    use anyhow::bail;
    use reverie::process::Container;
    use reverie::process::Mount;
    use reverie::process::MountFlags;
    use reverie::process::PinnedMountSource;
    use sha2::Digest;
    use sha2::Sha256;

    use super::super::native_exec_inputs::CapturedIdentity;
    use super::super::native_exec_inputs::GuestPath;
    use super::super::native_exec_inputs::HeldDynamicInputs;
    use super::super::native_exec_inputs::InputRole;

    /// The exact independently retained original interpreter object. The
    /// bootstrap replaces PT_INTERP; this separate role preserves its identity.
    pub(crate) const STOCK_LOADER_PATH: &[u8] = b"/run/hermit/native/stock-loader";

    const SEALS: i32 = libc::F_SEAL_SEAL
        | libc::F_SEAL_SHRINK
        | libc::F_SEAL_GROW
        | libc::F_SEAL_WRITE
        | libc::F_SEAL_FUTURE_WRITE
        | 0x0020; // F_SEAL_EXEC
    // Linux statvfs uses 0x1000 for ST_RELATIME, distinct from MS_RELATIME.
    const ROOT_ST_RELATIME: libc::c_ulong = 0x1000;

    /// Distinguishes an addition from a deliberate literal PT_INTERP
    /// substitution. Original loader and bootstrap identities remain separate.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub(crate) enum FilePlacement {
        Additional,
        InterpreterReplacement,
    }

    pub(crate) struct RuntimeFile {
        path: GuestPath,
        file: File,
        identity: CapturedIdentity,
        executable: bool,
        placement: FilePlacement,
    }

    impl RuntimeFile {
        /// Check and duplicate the real sealed object. Runtime provenance,
        /// DT_NEEDED coverage and launch authorization belong to its consumer.
        pub(crate) fn new(
            path: GuestPath,
            descriptor: BorrowedFd<'_>,
            executable: bool,
            placement: FilePlacement,
        ) -> Result<Self> {
            let fd = unsafe { libc::fcntl(descriptor.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
            if fd < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            let file = unsafe { File::from_raw_fd(fd) };
            let metadata = file.metadata()?;
            if !metadata.is_file()
                || metadata.len() > 64 * 1024 * 1024
                || metadata.mode() & 0o6000 != 0
                || unsafe { libc::fcntl(fd, libc::F_GET_SEALS) } != SEALS
                || (metadata.mode() & 0o111 != 0) != executable
            {
                bail!("runtime root file is not an exact sealed object with the requested mode");
            }
            use std::os::unix::fs::FileExt;
            let mut bytes = vec![0; usize::try_from(metadata.len())?];
            file.read_exact_at(&mut bytes, 0)?;
            Ok(Self {
                path,
                file,
                identity: CapturedIdentity {
                    device: metadata.dev(),
                    inode: metadata.ino(),
                    size: metadata.len(),
                    digest: Sha256::digest(&bytes).into(),
                },
                executable,
                placement,
            })
        }
        pub(crate) fn try_clone(&self) -> Result<Self> {
            Ok(Self {
                path: self.path.clone(),
                file: self.file.try_clone()?,
                identity: self.identity,
                executable: self.executable,
                placement: self.placement,
            })
        }
        pub(crate) fn path(&self) -> &GuestPath {
            &self.path
        }
        pub(crate) fn identity(&self) -> CapturedIdentity {
            self.identity
        }
        pub(crate) fn executable(&self) -> bool {
            self.executable
        }
        pub(crate) fn placement(&self) -> FilePlacement {
            self.placement
        }
        pub(crate) fn descriptor(&self) -> BorrowedFd<'_> {
            use std::os::fd::AsFd;
            self.file.as_fd()
        }
    }

    /// Only named protected control channels have endpoint additions.
    /// Observer and DETLOG use distinct reserved descriptors instead.
    #[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
    pub(crate) enum Endpoint {
        Rpc,
        Provision,
        Statistics,
    }
    impl Endpoint {
        fn path(self) -> &'static str {
            match self {
                Self::Rpc => "/run/rpc.sock",
                Self::Provision => "/run/provision.sock",
                Self::Statistics => "/run/stats.sock",
            }
        }
    }

    /// Parent-owned until actual reaping. No proc path or child PID is accepted
    /// here: those are derived inside the real child after namespace setup.
    #[derive(Default)]
    pub(crate) struct DeclaredRootAdditions {
        files: Vec<RuntimeFile>,
        endpoints: Vec<(Endpoint, PinnedMountSource, u64, u64)>,
        common_group: Option<Arc<RuntimeFile>>,
    }
    impl DeclaredRootAdditions {
        pub(crate) fn try_clone(&self) -> Result<Self> {
            Ok(Self {
                files: self
                    .files
                    .iter()
                    .map(RuntimeFile::try_clone)
                    .collect::<Result<_>>()?,
                endpoints: self.endpoints.clone(),
                common_group: self.common_group.as_ref().map(Arc::clone),
            })
        }
        /// Capture the existing common identity policy once before either
        /// verify plan is cloned. Both plans retain this exact sealed owner.
        pub(crate) fn capture_common_inputs(&mut self) -> Result<()> {
            if self.common_group.is_some() {
                bail!("common identity inputs already captured");
            }
            let group = super::frozen_group_file()?;
            let sealed = super::super::native_exec_inputs::seal_bytes(
                &std::fs::read(group.path())?,
                InputRole::Data,
            )?;
            self.common_group = Some(Arc::new(RuntimeFile::new(
                GuestPath::new(b"/etc/group".to_vec())?,
                sealed.as_fd(),
                false,
                FilePlacement::Additional,
            )?));
            Ok(())
        }
        pub(crate) fn common_group(&self) -> Result<&RuntimeFile> {
            self.common_group
                .as_deref()
                .context("common identity input is absent")
        }
        pub(crate) fn adopt_common_group(&mut self, descriptor: BorrowedFd<'_>) -> Result<()> {
            if self.common_group.is_some() {
                bail!("common identity inputs already captured");
            }
            self.common_group = Some(Arc::new(RuntimeFile::new(
                GuestPath::new(b"/etc/group".to_vec())?,
                descriptor,
                false,
                FilePlacement::Additional,
            )?));
            Ok(())
        }
        pub(crate) fn add_file(&mut self, file: RuntimeFile) -> Result<()> {
            if file.path.as_bytes() == b"/etc/group"
                || self.files.len() == 256
                || self.files.iter().any(|old| old.path == file.path)
            {
                bail!("duplicate or excessive runtime root files");
            }
            self.files.push(file);
            Ok(())
        }
        pub(crate) fn add_endpoint(&mut self, kind: Endpoint, source: &Path) -> Result<()> {
            if self.endpoints.iter().any(|(old, _, _, _)| *old == kind) {
                bail!("duplicate protected endpoint");
            }
            let metadata = std::fs::symlink_metadata(source)?;
            use std::os::unix::fs::FileTypeExt;
            if !metadata.file_type().is_socket() {
                bail!("protected endpoint is not a socket");
            }
            let pin =
                PinnedMountSource::open(source).map_err(|error| anyhow::anyhow!("{error}"))?;
            self.endpoints
                .push((kind, pin, metadata.dev(), metadata.ino()));
            Ok(())
        }
    }

    /// One root plan per physical run, held by its parent through actual reap.
    /// Both verify plans share the same Arc<HeldDynamicInputs>; additions can
    /// duplicate immutable held descriptors via try_clone, never recapture them.
    pub(crate) struct DeclaredInputRoot {
        inputs: Arc<HeldDynamicInputs>,
        additions: DeclaredRootAdditions,
        stage: tempfile::TempDir,
        stage_parent: Option<File>,
        stage_name: CString,
        stage_pin: PinnedMountSource,
        parent_namespace: (u64, u64),
        stage_identity: (u64, u64),
        configured: bool,
        materialized: bool,
    }

    fn reserved(path: &Path) -> bool {
        ["/proc", "/dev", "/run", "/tmp", "/tool"]
            .iter()
            .any(|reserved| path.starts_with(reserved))
            || path == Path::new("/etc/group")
    }

    impl DeclaredInputRoot {
        /// Stage in an existing caller-owned directory that the common mount
        /// plan does not cover. The caller retains/excludes external writers to
        /// this directory through reap; its pathname is never native authority.
        pub(crate) fn prepare(
            inputs: Arc<HeldDynamicInputs>,
            mut additions: DeclaredRootAdditions,
            staging_parent: &Path,
        ) -> Result<Self> {
            inputs.recheck()?;
            for file in inputs.files() {
                if reserved(file.path.as_path()) || reserved(file.resolved_path.as_path()) {
                    bail!("declared input collides with common runtime-owned paths");
                }
            }
            for alias in inputs.aliases() {
                if reserved(alias.path.as_path()) {
                    bail!("declared alias shadows a runtime-owned path");
                }
                if inputs
                    .resolve_path(&alias.path)
                    .is_ok_and(|target| reserved(target.as_path()))
                {
                    bail!("declared alias redirects into runtime-owned paths");
                }
            }
            // No host input is recaptured here. Per-run plans consume clones
            // of the same pre-verify sealed common identity owner.
            let group = additions
                .common_group
                .as_ref()
                .context("common identity inputs were not captured before verify")?;
            let checked = RuntimeFile::new(
                group.path.clone(),
                group.descriptor(),
                group.executable,
                group.placement,
            )?;
            if checked.identity != group.identity
                || group.path.as_bytes() != b"/etc/group"
                || group.executable
                || group.placement != FilePlacement::Additional
                || additions.files.len() == 256
            {
                bail!("retained common identity object differs or root file bound exceeded");
            }
            additions.files.push(group.try_clone()?);
            let mut endpoints = BTreeSet::new();
            for file in inputs.files() {
                endpoints.insert(file.resolved_path.clone());
            }
            for file in &additions.files {
                let resolved = inputs.resolve_path(&file.path)?;
                match file.placement {
                    FilePlacement::InterpreterReplacement => {
                        if !file.executable
                            || !inputs.is_interpreter(&file.path)
                            || !endpoints.remove(&resolved)
                        {
                            bail!("bootstrap replacement does not name an enrolled PT_INTERP");
                        }
                    }
                    FilePlacement::Additional => {}
                }
                if !endpoints.insert(resolved) {
                    bail!("runtime file collides with declared input");
                }
            }
            for (endpoint, _, _, _) in &additions.endpoints {
                if !endpoints.insert(GuestPath::new(endpoint.path().as_bytes().to_vec())?) {
                    bail!("protected endpoint collides with a runtime file");
                }
            }
            for path in &endpoints {
                if endpoints.iter().any(|other| {
                    other.as_bytes().len() > path.as_bytes().len()
                        && other.as_bytes().starts_with(path.as_bytes())
                        && other.as_bytes()[path.as_bytes().len()] == b'/'
                }) {
                    bail!("root file shadows another endpoint parent");
                }
            }
            for replacement in additions
                .files
                .iter()
                .filter(|file| file.placement == FilePlacement::InterpreterReplacement)
            {
                let resolved = inputs.resolve_path(&replacement.path)?;
                let original = inputs
                    .files()
                    .iter()
                    .find(|file| file.resolved_path == resolved && file.role == InputRole::Loader)
                    .context("original interpreter object missing")?;
                if !additions.files.iter().any(|file| {
                    file.path.as_bytes() == STOCK_LOADER_PATH
                        && file.placement == FilePlacement::Additional
                        && file.executable
                        && file.identity == original.identity
                }) {
                    bail!("bootstrap replacement lacks the exact independently held stock loader");
                }
            }
            let namespace = std::fs::metadata("/proc/self/ns/mnt")?;
            let parent_namespace = (namespace.dev(), namespace.ino());
            // This owner-selected directory must remain visible through the
            // common mount plan. PinnedMountSource reopens/checks the actual
            // child path; covered or replaced source paths are refusals.
            if !staging_parent.is_absolute() {
                bail!("declared root staging parent must be absolute");
            }
            let stage = tempfile::Builder::new()
                .prefix("hermit-declared-root-")
                .tempdir_in(staging_parent)?;
            let stage_parent = File::open(
                stage
                    .path()
                    .parent()
                    .context("root staging parent missing")?,
            )?;
            let stage_name = CString::new(stage.path().file_name().unwrap().as_bytes())?;
            let stage_pin =
                PinnedMountSource::open(stage.path()).map_err(|e| anyhow::anyhow!("{e}"))?;
            let stage_meta = std::fs::metadata(stage.path())?;
            let stage_identity = (stage_meta.dev(), stage_meta.ino());
            Ok(Self {
                inputs,
                additions,
                stage,
                stage_parent: Some(stage_parent),
                stage_name,
                stage_pin,
                parent_namespace,
                stage_identity,
                configured: false,
                materialized: false,
            })
        }

        pub(crate) fn runtime_files(&self) -> &[RuntimeFile] {
            &self.additions.files
        }

        /// Add only root mounts to the existing common Container. PID/UTS,
        /// network, affinity, signals, cleanup and launch remain its existing
        /// owners. The checked staging bind precedes the fresh tmpfs cover.
        pub(crate) fn configure(&mut self, container: &mut Container) -> Result<()> {
            if self.configured {
                bail!("declared root configured twice");
            }
            container.mount(Mount::bind_pinned(&self.stage_pin, self.stage.path()));
            container.mount(
                Mount::tmpfs(self.stage.path())
                    .flags(MountFlags::MS_NOSUID | MountFlags::MS_NODEV)
                    .data("mode=0755,size=384m"),
            );
            container.mount(
                Mount::tmpfs(self.stage.path().join("tmp"))
                    .touch_target()
                    .flags(MountFlags::MS_NOSUID | MountFlags::MS_NODEV | MountFlags::MS_NOEXEC)
                    .data("mode=1777,size=64m"),
            );
            for (kind, source, _, _) in &self.additions.endpoints {
                container.mount(
                    Mount::bind_pinned(source, self.stage.path().join(&kind.path()[1..]))
                        .readonly()
                        .flags(MountFlags::MS_NOSUID | MountFlags::MS_NODEV | MountFlags::MS_NOEXEC)
                        .touch_target(),
                );
            }
            self.configured = true;
            Ok(())
        }

        /// Materialize the static graph in the actual common Container init,
        /// before coordinator workers. Current-task proc files are deliberately
        /// deferred to the actual guest Command child.
        ///
        /// # Safety
        /// The existing Container must have executed this exact mount plan in
        /// a private mount namespace with a private descriptor table. The caller
        /// must exclude external writers to the held staging parent/name during
        /// setup, retain the parent guard through reap, and close/retire every
        /// other outside-root descriptor before guest entry. The prepared root
        /// remains writable only until the one trusted actual guest operation;
        /// no untrusted child may run during this interval. This receipt does
        /// not establish those launch/writer/birth premises on its own.
        pub(crate) unsafe fn materialize(&mut self) -> Result<PreparedDeclaredRoot> {
            if !self.configured || self.materialized {
                bail!("declared root setup order or reuse");
            }
            self.materialized = true; // any partial setup is terminal for this child
            let namespace = std::fs::metadata("/proc/self/ns/mnt")?;
            if unsafe { libc::getpid() } <= 0
                || unsafe { libc::syscall(libc::SYS_gettid) }
                    != i64::from(unsafe { libc::getpid() })
            {
                bail!("common root materialization requires the actual current main task");
            }
            if (namespace.dev(), namespace.ino()) == self.parent_namespace {
                bail!("declared root was not installed in a fresh child mount namespace");
            }
            let held_parent = self
                .stage_parent
                .as_ref()
                .context("child staging descriptor retired")?;
            // Inherited descriptors can refer to a pre-namespace mount even
            // after a lexical path is covered. Resolve the current parent and
            // check the same held object before opening the child tmpfs.
            let parent_path = CString::new(
                self.stage
                    .path()
                    .parent()
                    .context("staging parent missing")?
                    .as_os_str()
                    .as_bytes(),
            )?;
            let parent_fd = unsafe {
                libc::open(
                    parent_path.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if parent_fd < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            let parent = unsafe { File::from_raw_fd(parent_fd) };
            let held = held_parent.metadata()?;
            let actual = parent.metadata()?;
            if (held.dev(), held.ino()) != (actual.dev(), actual.ino()) {
                bail!("common mounts covered or replaced the declared staging parent");
            }
            let fd = unsafe {
                libc::openat(
                    parent.as_raw_fd(),
                    self.stage_name.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if fd < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            let root = unsafe { File::from_raw_fd(fd) };
            let mut fs = std::mem::MaybeUninit::<libc::statfs>::uninit();
            if unsafe { libc::fstatfs(fd, fs.as_mut_ptr()) } != 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            let root_meta = root.metadata()?;
            if unsafe { fs.assume_init() }.f_type != libc::TMPFS_MAGIC
                || (root_meta.dev(), root_meta.ino()) == self.stage_identity
            {
                bail!("declared root is not the new child tmpfs");
            }
            self.inputs.recheck()?;
            for (kind, _, device, inode) in &self.additions.endpoints {
                let target = root_relative(&root, kind.path().as_bytes(), libc::O_PATH)?;
                let actual = target.metadata()?;
                use std::os::unix::fs::FileTypeExt;
                if actual.dev() != *device
                    || actual.ino() != *inode
                    || !actual.file_type().is_socket()
                {
                    bail!("bound protected endpoint changed from its parent-held object");
                }
            }
            let mut copies = Vec::new();
            for file in self.inputs.files() {
                if self.additions.files.iter().any(|replacement| {
                    replacement.placement == FilePlacement::InterpreterReplacement
                        && self.inputs.resolve_path(&replacement.path).ok().as_ref()
                            == Some(&file.resolved_path)
                }) {
                    continue;
                }
                copies.push(RootCopy {
                    path: file.resolved_path.clone(),
                    source: File::from(file.descriptor().try_clone_to_owned()?),
                    executable: file.role != InputRole::Data,
                    expected: file.identity,
                });
            }
            for file in &self.additions.files {
                copies.push(RootCopy {
                    path: self.inputs.resolve_path(&file.path)?,
                    source: file.file.try_clone()?,
                    executable: file.executable,
                    expected: file.identity,
                });
            }
            let copies = copy_root_groups(&root, copies)?;
            // Every hardlink has been created before any path becomes a file
            // mount. No writable copy FD remains when these mounts are made.
            let mut backings = BTreeMap::new();
            for copy in copies {
                let path = copy.request.path.clone();
                let backing = bind_root_copy(&root, copy)?;
                if backings.insert(path, backing).is_some() {
                    bail!("duplicate readonly root placement");
                }
            }
            // Hold the actual common proc mount, but do not bind init's task.
            // The guest operation resolves numeric leaves only after its real
            // PID exists; it never accepts a supplied PID.
            let proc_root = super::open_path(Path::new("/proc"))?;
            let mut proc_fs = std::mem::MaybeUninit::<libc::statfs>::uninit();
            if unsafe { libc::fstatfs(proc_root.as_raw_fd(), proc_fs.as_mut_ptr()) } != 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            if unsafe { proc_fs.assume_init() }.f_type != libc::PROC_SUPER_MAGIC {
                bail!("common proc source is not procfs");
            }
            let proc_mount = observed_mount_id(proc_root.as_fd())?;
            let namespace_fd = super::open_path(Path::new("/proc/self/ns/mnt"))?;
            for node in ["null", "zero", "full", "random", "urandom"] {
                let source = super::open_path(Path::new(&format!("/dev/{node}")))?;
                bind_fd(
                    &root,
                    &GuestPath::new(format!("/dev/{node}").into_bytes())?,
                    source.as_fd(),
                    false,
                    None,
                )?;
            }
            mkdir_parents(&root, b"/run/nscd/empty")?;
            for alias in self.inputs.aliases() {
                create_link(&root, &alias.path, &alias.target)?;
            }
            // The static graph is complete. The root stays writable only for
            // trusted same-namespace guest setup, which creates exact numeric
            // proc endpoints and then makes it read-only before entry/ACK.
            let identity = root.metadata()?;
            let mount_id = observed_mount_id(root.as_fd())?;
            self.stage_pin
                .retire()
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            for (_, source, _, _) in &self.additions.endpoints {
                source.retire().map_err(|e| anyhow::anyhow!("{e}"))?;
            }
            self.stage_parent.take(); // child only; parent guard remains live
            self.materialized = true;
            Ok(PreparedDeclaredRoot {
                inputs: self.inputs.clone(),
                backings,
                root,
                proc_root,
                namespace_fd,
                device: identity.dev(),
                inode: identity.ino(),
                mount_id,
                proc_mount,
                setup_pid: unsafe { libc::getpid() },
                namespace: (namespace.dev(), namespace.ino()),
                guest_prepared: false,
            })
        }
    }

    struct RootCopy {
        path: GuestPath,
        source: File,
        expected: CapturedIdentity,
        executable: bool,
    }

    struct CopiedRootFile {
        request: RootCopy,
        identity: CapturedIdentity,
        links: u64,
    }

    /// A retained source-to-named-copy owner, not an immutable-root ACK. The
    /// guest must still freeze the root and the installer must join that actual
    /// root/namespace/birth independently. Clones retain these exact files and
    /// path bindings through reaping; scalar identities cannot construct it.
    #[derive(Clone)]
    pub(crate) struct ReadonlyRootBacking(Arc<ReadonlyRootBackingOwner>);

    struct ReadonlyRootBackingOwner {
        source: File,
        expected: CapturedIdentity,
        source_mode: u32,
        backing: File,
        identity: CapturedIdentity,
        mount_id: u64,
        flags: libc::c_ulong,
        executable: bool,
        links: u64,
        root: File,
        root_identity: (u64, u64),
        root_mount: u64,
        namespace: File,
        namespace_identity: (u64, u64),
        path: GuestPath,
    }

    impl ReadonlyRootBacking {
        pub(crate) fn source_identity(&self) -> CapturedIdentity {
            self.0.expected
        }
        pub(crate) fn identity(&self) -> CapturedIdentity {
            self.0.identity
        }
        #[cfg(test)]
        pub(crate) fn mount_id(&self) -> u64 {
            self.0.mount_id
        }
        pub(crate) fn descriptor(&self) -> BorrowedFd<'_> {
            self.0.backing.as_fd()
        }
        pub(crate) fn executable(&self) -> bool {
            self.0.executable
        }
        pub(crate) fn verify(&self) -> Result<()> {
            let owner = &self.0;
            let root_meta = owner.root.metadata()?;
            let namespace = owner.namespace.metadata()?;
            let current_namespace = std::fs::metadata("/proc/thread-self/ns/mnt")?;
            let mut fs = std::mem::MaybeUninit::<libc::statfs>::uninit();
            if unsafe { libc::fstatfs(owner.root.as_raw_fd(), fs.as_mut_ptr()) } != 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            if (root_meta.dev(), root_meta.ino()) != owner.root_identity
                || (namespace.dev(), namespace.ino()) != owner.namespace_identity
                || (current_namespace.dev(), current_namespace.ino()) != owner.namespace_identity
                || observed_mount_id(owner.root.as_fd())? != owner.root_mount
                || unsafe { fs.assume_init() }.f_type != libc::TMPFS_MAGIC
                || checked_file_identity(&owner.source, owner.executable, true)? != owner.expected
                || owner.source.metadata()?.mode() != owner.source_mode
            {
                bail!("retained root backing source or root changed");
            }
            // This walk neither creates paths nor follows a substituted parent
            // or leaf symlink. Also inspect the descriptor itself: a pathname
            // re-open is not a replacement for the retained actual owner.
            let current = open_root_file(&owner.root, owner.path.as_bytes())?;
            for file in [&owner.backing, &current] {
                if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) } & libc::O_ACCMODE
                    != libc::O_RDONLY
                    || checked_file_identity(file, owner.executable, false)? != owner.identity
                    || file.metadata()?.nlink() != owner.links
                    || observed_mount_id(file.as_fd())? != owner.mount_id
                    || raw_statvfs(file.as_raw_fd())?.f_flag != owner.flags
                {
                    bail!("readonly root backing bytes, path or mount changed");
                }
            }
            let required = libc::ST_RDONLY | libc::ST_NOSUID | libc::ST_NODEV;
            if owner.flags & required != required
                || (owner.flags & libc::ST_NOEXEC != 0) == owner.executable
            {
                bail!("readonly root backing role policy differs");
            }
            Ok(())
        }
    }

    #[cfg(test)]
    impl ReadonlyRootBackingOwner {
        fn try_clone_for_test(&self) -> Result<Self> {
            Ok(Self {
                source: self.source.try_clone()?,
                expected: self.expected,
                source_mode: self.source_mode,
                backing: self.backing.try_clone()?,
                identity: self.identity,
                mount_id: self.mount_id,
                flags: self.flags,
                executable: self.executable,
                links: self.links,
                root: self.root.try_clone()?,
                root_identity: self.root_identity,
                root_mount: self.root_mount,
                namespace: self.namespace.try_clone()?,
                namespace_identity: self.namespace_identity,
                path: self.path.clone(),
            })
        }
    }

    fn checked_file_identity(
        file: &File,
        executable: bool,
        sealed: bool,
    ) -> Result<CapturedIdentity> {
        let before = file.metadata()?;
        let mode = if executable { 0o555 } else { 0o444 };
        if !before.is_file()
            || before.len() > 64 * 1024 * 1024
            || before.mode() & 0o6000 != 0
            || (before.mode() & 0o111 != 0) != executable
            || (!sealed && before.mode() & 0o7777 != mode)
            || (sealed && unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GET_SEALS) } != SEALS)
        {
            bail!("root copy file kind, size, mode or source seals differ");
        }
        let mut hash = Sha256::new();
        let mut bytes = [0u8; 64 * 1024];
        let mut offset = 0;
        while offset < before.len() {
            let count = usize::try_from((before.len() - offset).min(bytes.len() as u64))?;
            file.read_exact_at(&mut bytes[..count], offset)?;
            hash.update(&bytes[..count]);
            offset += count as u64; // bounded by the checked file length
        }
        if file.read_at(&mut bytes[..1], before.len())? != 0 {
            bail!("root copy file grew during readback");
        }
        let after = file.metadata()?;
        if (
            before.dev(),
            before.ino(),
            before.len(),
            before.mode(),
            before.nlink(),
        ) != (
            after.dev(),
            after.ino(),
            after.len(),
            after.mode(),
            after.nlink(),
        ) || (sealed && unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GET_SEALS) } != SEALS)
        {
            bail!("root copy file metadata changed during readback");
        }
        Ok(CapturedIdentity {
            device: after.dev(),
            inode: after.ino(),
            size: after.len(),
            digest: hash.finalize().into(),
        })
    }

    fn path_leaf(path: &[u8]) -> Result<CString> {
        Ok(CString::new(
            path.rsplit(|byte| *byte == b'/')
                .next()
                .context("missing root leaf")?,
        )?)
    }

    fn open_root_file(root: &File, path: &[u8]) -> Result<File> {
        open_root_file_with_flags(root, path, libc::O_RDONLY)
    }

    fn open_root_file_with_flags(root: &File, path: &[u8], flags: i32) -> Result<File> {
        let mut parent = root.try_clone()?;
        let parts: Vec<_> = path[1..].split(|byte| *byte == b'/').collect();
        for part in &parts[..parts.len() - 1] {
            let component = CString::new(*part)?;
            let fd = unsafe {
                libc::openat(
                    parent.as_raw_fd(),
                    component.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if fd < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            parent = unsafe { File::from_raw_fd(fd) };
        }
        let leaf = path_leaf(path)?;
        let fd = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                leaf.as_ptr(),
                flags | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(unsafe { File::from_raw_fd(fd) })
    }

    fn open_new_root_file(root: &File, path: &[u8], mode: libc::mode_t) -> Result<File> {
        let parent = mkdir_parents(root, path)?;
        let leaf = path_leaf(path)?;
        let fd = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                leaf.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                mode,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let file = unsafe { File::from_raw_fd(fd) };
        // Set the final exact mode independently of the inherited umask.
        if unsafe { libc::fchmod(fd, mode) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(file)
    }

    fn copy_root_groups(root: &File, requests: Vec<RootCopy>) -> Result<Vec<CopiedRootFile>> {
        let mut groups: BTreeMap<(u64, u64), Vec<RootCopy>> = BTreeMap::new();
        let mut names = BTreeSet::new();
        for request in requests {
            if !names.insert(request.path.clone())
                || checked_file_identity(&request.source, request.executable, true)?
                    != request.expected
            {
                bail!("duplicate root path or changed sealed copy source");
            }
            let key = (request.expected.device, request.expected.inode);
            let group = groups.entry(key).or_default();
            if let Some(first) = group.first()
                && (first.expected != request.expected
                    || first.executable != request.executable
                    || first.source.metadata()?.mode() != request.source.metadata()?.mode())
            {
                bail!("same-source root placements disagree about bytes or role");
            }
            group.push(request);
        }
        let mut copies = Vec::new();
        for group in groups.into_values() {
            let first = &group[0];
            let mode = if first.executable { 0o555 } else { 0o444 };
            let writable = open_new_root_file(root, first.path.as_bytes(), mode)?;
            let mut bytes = [0u8; 64 * 1024];
            let mut offset = 0;
            while offset < first.expected.size {
                let count =
                    usize::try_from((first.expected.size - offset).min(bytes.len() as u64))?;
                first.source.read_exact_at(&mut bytes[..count], offset)?;
                writable.write_all_at(&bytes[..count], offset)?;
                offset += count as u64;
            }
            let readback = open_root_file(root, first.path.as_bytes())?;
            let before_close = checked_file_identity(&readback, first.executable, false)?;
            if before_close.size != first.expected.size
                || before_close.digest != first.expected.digest
                || checked_file_identity(&first.source, first.executable, true)? != first.expected
            {
                bail!("root copy differs before writer retirement");
            }
            drop(writable); // no write FD or shared mapping escapes this scope
            let after_close = checked_file_identity(&readback, first.executable, false)?;
            if after_close != before_close {
                bail!("root copy changed at writer retirement");
            }
            let primary_parent = mkdir_parents(root, first.path.as_bytes())?;
            let primary_leaf = path_leaf(first.path.as_bytes())?;
            for other in &group[1..] {
                let parent = mkdir_parents(root, other.path.as_bytes())?;
                let leaf = path_leaf(other.path.as_bytes())?;
                if unsafe {
                    libc::linkat(
                        primary_parent.as_raw_fd(),
                        primary_leaf.as_ptr(),
                        parent.as_raw_fd(),
                        leaf.as_ptr(),
                        0,
                    )
                } != 0
                {
                    return Err(std::io::Error::last_os_error().into());
                }
            }
            let links = u64::try_from(group.len())?;
            for request in group {
                let file = open_root_file(root, request.path.as_bytes())?;
                if checked_file_identity(&file, request.executable, false)? != after_close
                    || file.metadata()?.nlink() != links
                    || checked_file_identity(&request.source, request.executable, true)?
                        != request.expected
                {
                    bail!("root hardlink or sealed source changed before binding");
                }
                copies.push(CopiedRootFile {
                    request,
                    identity: after_close,
                    links,
                });
            }
        }
        Ok(copies)
    }

    fn bind_root_copy(root: &File, copy: CopiedRootFile) -> Result<ReadonlyRootBacking> {
        let request = copy.request;
        let parent = mkdir_parents(root, request.path.as_bytes())?;
        let leaf = path_leaf(request.path.as_bytes())?;
        let source = open_root_file(root, request.path.as_bytes())?;
        if checked_file_identity(&source, request.executable, false)? != copy.identity
            || source.metadata()?.nlink() != copy.links
        {
            bail!("root copy changed before self bind");
        }
        let source_flags = raw_statvfs(source.as_raw_fd())?.f_flag;
        if source_flags & libc::ST_RDONLY != 0 || source_flags & libc::ST_NOEXEC != 0 {
            bail!("new root copy is not on the fresh writable executable tmpfs");
        }
        let from = CString::new(format!("/proc/self/fd/{}", source.as_raw_fd()))?;
        let mut target = format!("/proc/self/fd/{}/", parent.as_raw_fd()).into_bytes();
        target.extend_from_slice(leaf.as_bytes());
        let target = CString::new(target)?;
        if unsafe {
            libc::mount(
                from.as_ptr(),
                target.as_ptr(),
                std::ptr::null(),
                libc::MS_BIND,
                std::ptr::null(),
            )
        } != 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        let mut mount_flags =
            libc::MS_BIND | libc::MS_REMOUNT | libc::MS_RDONLY | libc::MS_NOSUID | libc::MS_NODEV;
        let mut expected_flags = source_flags | libc::ST_RDONLY | libc::ST_NOSUID | libc::ST_NODEV;
        if !request.executable {
            mount_flags |= libc::MS_NOEXEC;
            expected_flags |= libc::ST_NOEXEC;
        }
        // Preserve actual source atime policy; these libc ST and MS constants
        // are different namespaces and cannot be cast interchangeably.
        for (observed, flag) in [
            (libc::ST_NOATIME, libc::MS_NOATIME),
            (libc::ST_NODIRATIME, libc::MS_NODIRATIME),
            (ROOT_ST_RELATIME, libc::MS_RELATIME),
        ] {
            if source_flags & observed != 0 {
                mount_flags |= flag;
            }
        }
        if unsafe {
            libc::mount(
                std::ptr::null(),
                target.as_ptr(),
                std::ptr::null(),
                mount_flags,
                std::ptr::null(),
            )
        } != 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        let backing = open_root_file(root, request.path.as_bytes())?;
        let metadata = root.metadata()?;
        let namespace = File::open("/proc/thread-self/ns/mnt")?;
        let namespace_metadata = namespace.metadata()?;
        let owner = ReadonlyRootBacking(Arc::new(ReadonlyRootBackingOwner {
            source_mode: request.source.metadata()?.mode(),
            source: request.source,
            expected: request.expected,
            identity: copy.identity,
            mount_id: observed_mount_id(backing.as_fd())?,
            flags: expected_flags,
            backing,
            executable: request.executable,
            links: copy.links,
            root: root.try_clone()?,
            root_identity: (metadata.dev(), metadata.ino()),
            root_mount: observed_mount_id(root.as_fd())?,
            namespace,
            namespace_identity: (namespace_metadata.dev(), namespace_metadata.ino()),
            path: request.path,
        }));
        owner.verify()?;
        Ok(owner)
    }

    /// Static graph observed in the common coordinator namespace. This is not
    /// the guest root receipt: no guest PID, proc binding or entry is claimed.
    pub(crate) struct PreparedDeclaredRoot {
        inputs: Arc<HeldDynamicInputs>,
        backings: BTreeMap<GuestPath, ReadonlyRootBacking>,
        root: File,
        proc_root: File,
        namespace_fd: File,
        device: u64,
        inode: u64,
        mount_id: u64,
        proc_mount: u64,
        setup_pid: i32,
        namespace: (u64, u64),
        guest_prepared: bool,
    }
    impl PreparedDeclaredRoot {
        pub(crate) fn inputs(&self) -> &Arc<HeldDynamicInputs> {
            &self.inputs
        }
        /// Only an actually materialized resolved name and its retained sealed
        /// source may select a receipt. A digest/path supplied by a request is
        /// not a source-to-copy admission.
        pub(crate) fn backing(
            &self,
            path: &GuestPath,
            expected: CapturedIdentity,
        ) -> Result<&ReadonlyRootBacking> {
            let backing = self
                .backings
                .get(path)
                .context("unregistered root backing path")?;
            if backing.source_identity() != expected {
                bail!("root backing does not belong to the retained source");
            }
            backing.verify()?;
            Ok(backing)
        }
        pub(crate) fn descriptor(&self) -> BorrowedFd<'_> {
            self.root.as_fd()
        }
        pub(crate) fn device(&self) -> u64 {
            self.device
        }
        pub(crate) fn inode(&self) -> u64 {
            self.inode
        }
        pub(crate) fn mount_id(&self) -> u64 {
            self.mount_id
        }
        pub(crate) fn namespace(&self) -> (u64, u64) {
            self.namespace
        }

        /// Prebuild exactly one guest operation before coordinator workers.
        /// `cwd` is the original Command's exact effective absolute cwd, not
        /// the runner's cwd. Its empty directories are part of the closed root;
        /// this data operation never attests original-directory permissions.
        /// The parent retains this root and all held inputs until actual reap.
        pub(crate) fn prepare_guest_setup(&mut self, cwd: &Path) -> Result<GuestDeclaredRootSetup> {
            if self.guest_prepared {
                bail!("guest root setup prepared twice");
            }
            self.guest_prepared = true;
            let cwd_bytes = cwd.as_os_str().as_bytes();
            let mut working_directory = [0u8; 4096];
            if cwd_bytes != b"/" {
                let path = GuestPath::new(cwd_bytes.to_vec())?;
                let resolved = self.inputs.resolve_path(&path)?;
                let mut directory = resolved.as_bytes().to_vec();
                directory.extend_from_slice(b"/.cwd-endpoint");
                // mkdir_parents creates/checks only the containing directory;
                // no placeholder file is inserted into the declared root.
                mkdir_parents(&self.root, &directory)?;
            }
            working_directory[..cwd_bytes.len()].copy_from_slice(cwd_bytes);
            Ok(GuestDeclaredRootSetup {
                root: Some(self.root.try_clone()?),
                proc_root: Some(self.proc_root.try_clone()?),
                namespace_fd: Some(self.namespace_fd.try_clone()?),
                device: self.device,
                inode: self.inode,
                mount_id: self.mount_id,
                proc_mount: self.proc_mount,
                setup_pid: self.setup_pid,
                namespace: self.namespace,
                working_directory,
                attempted: false,
            })
        }
    }

    /// Only fixed fields and owned descriptors: the child operation has no
    /// allocation, formatting, Rust TLS, mutex, callback or heap-owned drop.
    /// One actual guest spawn may consume it; a forked copy is not authority.
    pub(crate) struct GuestDeclaredRootSetup {
        root: Option<File>,
        proc_root: Option<File>,
        namespace_fd: Option<File>,
        device: u64,
        inode: u64,
        mount_id: u64,
        proc_mount: u64,
        setup_pid: i32,
        namespace: (u64, u64),
        working_directory: [u8; 4096],
        attempted: bool,
    }
    impl GuestDeclaredRootSetup {
        /// Bind the actual guest task's proc leaves, seal the root and enter it.
        /// All paths are fixed stack buffers; errors retain raw OS errno.
        ///
        /// # Safety
        /// Run only in the existing Command's real post-fork/pre-exec child,
        /// with the exact common mount plan and private FD table. The trusted
        /// coordinator must exclude root/mount writers, retain the setup and
        /// input owners through physical reap, and consume this token in only
        /// one spawn. The launcher must transfer the observed root FD/identity,
        /// join it to an actual watched birth and obtain the raw startup ACK,
        /// retire all other setup/outside-root FDs, and install its qualified
        /// filter before original exec. Every error is terminal for that child.
        /// This performs filesystem setup only, never NativeExecProof/admission.
        pub(crate) unsafe fn bind_and_enter(
            &mut self,
        ) -> std::io::Result<MaterializedDeclaredRoot> {
            if self.attempted {
                return Err(raw_refusal());
            }
            self.attempted = true;
            let root = self.root.as_ref().ok_or_else(raw_refusal)?.as_raw_fd();
            let proc_root = self.proc_root.as_ref().ok_or_else(raw_refusal)?.as_raw_fd();
            let namespace_fd = self
                .namespace_fd
                .as_ref()
                .ok_or_else(raw_refusal)?
                .as_raw_fd();
            let pid = unsafe { libc::getpid() };
            if pid <= 0
                || pid == self.setup_pid
                || unsafe { libc::syscall(libc::SYS_gettid) } != i64::from(pid)
            {
                return Err(raw_refusal());
            }
            let namespace = raw_stat(namespace_fd)?;
            if (namespace.st_dev, namespace.st_ino) != self.namespace {
                return Err(raw_refusal());
            }
            let current_namespace = unsafe {
                libc::openat(
                    proc_root,
                    c"self/ns/mnt".as_ptr(),
                    libc::O_RDONLY | libc::O_CLOEXEC,
                )
            };
            if current_namespace < 0 {
                return Err(std::io::Error::last_os_error());
            }
            let current_namespace = unsafe { File::from_raw_fd(current_namespace) };
            let current = raw_stat(current_namespace.as_raw_fd())?;
            if (current.st_dev, current.st_ino) != self.namespace {
                return Err(raw_refusal());
            }
            drop(current_namespace);
            let root_stat = raw_stat(root)?;
            let root_view = raw_statvfs(root)?;
            if (root_stat.st_dev, root_stat.st_ino) != (self.device, self.inode)
                || raw_mount_id(root)? != self.mount_id
                || root_view.f_flag & libc::ST_RDONLY != 0
                || raw_mount_id(proc_root)? != self.proc_mount
            {
                return Err(raw_refusal());
            }
            let mut proc_fs = std::mem::MaybeUninit::<libc::statfs>::uninit();
            if unsafe { libc::fstatfs(proc_root, proc_fs.as_mut_ptr()) } != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if unsafe { proc_fs.assume_init() }.f_type != libc::PROC_SUPER_MAGIC {
                return Err(raw_refusal());
            }
            let mut number = RawPath::new();
            number.number(pid as u32)?;
            let mut self_target = [0u8; 32];
            let length = unsafe {
                libc::readlinkat(
                    proc_root,
                    c"self".as_ptr(),
                    self_target.as_mut_ptr().cast(),
                    self_target.len(),
                )
            };
            if length < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if length as usize != number.len
                || self_target[..length as usize] != number.bytes[..number.len]
            {
                return Err(raw_refusal());
            }
            raw_mkdir(root, c"proc".as_ptr(), true)?;
            raw_mkdir(root, number.prefixed(b"proc/")?.as_ptr(), false)?;
            let mut task = RawPath::new();
            task.push(b"proc/")?;
            task.push(number.as_bytes())?;
            task.push(b"/task")?;
            raw_mkdir(root, task.as_ptr(), false)?;
            task.push(b"/")?;
            task.push(number.as_bytes())?;
            raw_mkdir(root, task.as_ptr(), false)?;
            for leaf in [
                b"maps".as_slice(),
                b"smaps",
                b"status",
                b"auxv",
                b"comm",
                b"fd",
            ] {
                let mut source = RawPath::new();
                source.push(number.as_bytes())?;
                source.push(b"/")?;
                source.push(leaf)?;
                let mut target = source.prefixed(b"proc/")?;
                raw_bind_proc(
                    root,
                    proc_root,
                    &source,
                    &target,
                    leaf == b"fd",
                    self.proc_mount,
                )?;
                target = RawPath::new();
                target.push(task.as_bytes())?;
                target.push(b"/")?;
                target.push(leaf)?;
                raw_bind_proc(
                    root,
                    proc_root,
                    &source,
                    &target,
                    leaf == b"fd",
                    self.proc_mount,
                )?;
            }
            if unsafe { libc::symlinkat(number.as_ptr(), root, c"proc/self".as_ptr()) } != 0 {
                return Err(std::io::Error::last_os_error());
            }
            let mut thread = RawPath::new();
            thread.push(number.as_bytes())?;
            thread.push(b"/task/")?;
            thread.push(number.as_bytes())?;
            if unsafe { libc::symlinkat(thread.as_ptr(), root, c"proc/thread-self".as_ptr()) } != 0
            {
                return Err(std::io::Error::last_os_error());
            }
            let root_path = RawPath::descriptor(root)?;
            if unsafe {
                libc::mount(
                    std::ptr::null(),
                    root_path.as_ptr(),
                    std::ptr::null(),
                    libc::MS_REMOUNT | libc::MS_RDONLY | libc::MS_NOSUID | libc::MS_NODEV,
                    std::ptr::null(),
                )
            } != 0
            {
                return Err(std::io::Error::last_os_error());
            }
            if raw_statvfs(root)?.f_flag & libc::ST_RDONLY == 0
                || raw_mount_id(root)? != self.mount_id
            {
                return Err(raw_refusal());
            }
            // Validate the namespace immediately before entry. Sharing it with
            // the coordinator is intended; a changed namespace is a refusal.
            let actual_ns = unsafe {
                libc::openat(
                    proc_root,
                    c"self/ns/mnt".as_ptr(),
                    libc::O_RDONLY | libc::O_CLOEXEC,
                )
            };
            if actual_ns < 0 {
                return Err(std::io::Error::last_os_error());
            }
            let actual_ns = unsafe { File::from_raw_fd(actual_ns) };
            let actual = raw_stat(actual_ns.as_raw_fd())?;
            if (actual.st_dev, actual.st_ino) != self.namespace {
                return Err(raw_refusal());
            }
            drop(actual_ns);
            if unsafe { libc::fchdir(root) } != 0
                || unsafe { libc::chroot(c".".as_ptr()) } != 0
                || unsafe { libc::chdir(self.working_directory.as_ptr().cast()) } != 0
            {
                return Err(std::io::Error::last_os_error());
            }
            // Close only these child's inherited setup descriptors. Parent
            // copies retain ownership; root itself is supplied to the raw hook.
            self.proc_root.take();
            self.namespace_fd.take();
            Ok(MaterializedDeclaredRoot {
                root: self.root.take().ok_or_else(raw_refusal)?,
                device: self.device,
                inode: self.inode,
                mount_id: self.mount_id,
                visible_pid: pid,
                namespace: self.namespace,
            })
        }
    }

    /// Actual same-guest root identity after proc binding and chroot. Transfer
    /// the held FD itself with the bounded startup protocol; supplied scalars
    /// alone never authorize a launch. All accessors are allocation-free.
    pub(crate) struct MaterializedDeclaredRoot {
        root: File,
        device: u64,
        inode: u64,
        mount_id: u64,
        visible_pid: i32,
        namespace: (u64, u64),
    }
    impl MaterializedDeclaredRoot {
        pub(crate) fn device(&self) -> u64 {
            self.device
        }
        pub(crate) fn inode(&self) -> u64 {
            self.inode
        }
        pub(crate) fn mount_id(&self) -> u64 {
            self.mount_id
        }
        pub(crate) fn visible_pid(&self) -> i32 {
            self.visible_pid
        }
        pub(crate) fn namespace(&self) -> (u64, u64) {
            self.namespace
        }
        pub(crate) fn descriptor(&self) -> BorrowedFd<'_> {
            self.root.as_fd()
        }
    }

    fn raw_refusal() -> std::io::Error {
        std::io::Error::from_raw_os_error(libc::EINVAL)
    }
    fn raw_stat(fd: i32) -> std::io::Result<libc::stat> {
        let mut value = std::mem::MaybeUninit::<libc::stat>::uninit();
        if unsafe { libc::fstat(fd, value.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(unsafe { value.assume_init() })
    }
    fn raw_statvfs(fd: i32) -> std::io::Result<libc::statvfs> {
        let mut value = std::mem::MaybeUninit::<libc::statvfs>::uninit();
        if unsafe { libc::fstatvfs(fd, value.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(unsafe { value.assume_init() })
    }
    fn raw_mount_id(fd: i32) -> std::io::Result<u64> {
        let mut value = std::mem::MaybeUninit::<libc::statx>::zeroed();
        if unsafe {
            libc::statx(
                fd,
                c"".as_ptr(),
                libc::AT_EMPTY_PATH | libc::AT_SYMLINK_NOFOLLOW,
                libc::STATX_MNT_ID,
                value.as_mut_ptr(),
            )
        } != 0
        {
            return Err(std::io::Error::last_os_error());
        }
        let value = unsafe { value.assume_init() };
        if value.stx_mask & libc::STATX_MNT_ID == 0 || value.stx_mnt_id == 0 {
            return Err(raw_refusal());
        }
        Ok(value.stx_mnt_id)
    }
    /// Small bounded C paths for kernel-derived PID/FD numbers, not user data.
    struct RawPath {
        bytes: [u8; 128],
        len: usize,
    }
    impl RawPath {
        fn new() -> Self {
            Self {
                bytes: [0; 128],
                len: 0,
            }
        }
        fn as_ptr(&self) -> *const libc::c_char {
            self.bytes.as_ptr().cast()
        }
        fn as_bytes(&self) -> &[u8] {
            &self.bytes[..self.len]
        }
        fn push(&mut self, bytes: &[u8]) -> std::io::Result<()> {
            if self.len + bytes.len() >= self.bytes.len() || bytes.contains(&0) {
                return Err(raw_refusal());
            }
            self.bytes[self.len..self.len + bytes.len()].copy_from_slice(bytes);
            self.len += bytes.len();
            self.bytes[self.len] = 0;
            Ok(())
        }
        fn number(&mut self, mut value: u32) -> std::io::Result<()> {
            let mut digits = [0u8; 10];
            let mut start = digits.len();
            loop {
                start -= 1;
                digits[start] = b'0' + (value % 10) as u8;
                value /= 10;
                if value == 0 {
                    break;
                }
            }
            self.push(&digits[start..])
        }
        fn prefixed(&self, prefix: &[u8]) -> std::io::Result<Self> {
            let mut result = Self::new();
            result.push(prefix)?;
            result.push(self.as_bytes())?;
            Ok(result)
        }
        fn descriptor(fd: i32) -> std::io::Result<Self> {
            if fd < 0 {
                return Err(raw_refusal());
            }
            let mut result = Self::new();
            result.push(b"/proc/self/fd/")?;
            result.number(fd as u32)?;
            Ok(result)
        }
    }
    fn raw_mkdir(
        root: i32,
        path: *const libc::c_char,
        allow_existing: bool,
    ) -> std::io::Result<()> {
        if unsafe { libc::mkdirat(root, path, 0o755) } == 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        if !allow_existing || error.raw_os_error() != Some(libc::EEXIST) {
            return Err(error);
        }
        let fd = unsafe {
            libc::openat(
                root,
                path,
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        drop(unsafe { File::from_raw_fd(fd) });
        Ok(())
    }
    fn raw_bind_proc(
        root: i32,
        proc_root: i32,
        source_path: &RawPath,
        target_path: &RawPath,
        directory: bool,
        mount_id: u64,
    ) -> std::io::Result<()> {
        let source = unsafe {
            libc::openat(
                proc_root,
                source_path.as_ptr(),
                libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if source < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let source = unsafe { File::from_raw_fd(source) };
        let metadata = raw_stat(source.as_raw_fd())?;
        if raw_mount_id(source.as_raw_fd())? != mount_id
            || (metadata.st_mode & libc::S_IFMT)
                != if directory {
                    libc::S_IFDIR
                } else {
                    libc::S_IFREG
                }
        {
            return Err(raw_refusal());
        }
        if directory {
            raw_mkdir(root, target_path.as_ptr(), false)?;
        } else {
            let target = unsafe {
                libc::openat(
                    root,
                    target_path.as_ptr(),
                    libc::O_CREAT
                        | libc::O_EXCL
                        | libc::O_WRONLY
                        | libc::O_NOFOLLOW
                        | libc::O_CLOEXEC,
                    0o444,
                )
            };
            if target < 0 {
                return Err(std::io::Error::last_os_error());
            }
            drop(unsafe { File::from_raw_fd(target) });
        }
        let from = RawPath::descriptor(source.as_raw_fd())?;
        let mut to = RawPath::descriptor(root)?;
        to.push(b"/")?;
        to.push(target_path.as_bytes())?;
        if unsafe {
            libc::mount(
                from.as_ptr(),
                to.as_ptr(),
                std::ptr::null(),
                libc::MS_BIND,
                std::ptr::null(),
            )
        } != 0
        {
            return Err(std::io::Error::last_os_error());
        }
        let source_flags = raw_statvfs(source.as_raw_fd())?.f_flag;
        let mut flags = libc::MS_BIND
            | libc::MS_REMOUNT
            | libc::MS_RDONLY
            | libc::MS_NOSUID
            | libc::MS_NODEV
            | libc::MS_NOEXEC;
        for (probe, flag) in [
            (libc::ST_NOATIME, libc::MS_NOATIME),
            (libc::ST_NODIRATIME, libc::MS_NODIRATIME),
        ] {
            if source_flags & probe != 0 {
                flags |= flag;
            }
        }
        if unsafe {
            libc::mount(
                std::ptr::null(),
                to.as_ptr(),
                std::ptr::null(),
                flags,
                std::ptr::null(),
            )
        } != 0
        {
            return Err(std::io::Error::last_os_error());
        }
        let target = unsafe {
            libc::openat(
                root,
                target_path.as_ptr(),
                libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if target < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let target = unsafe { File::from_raw_fd(target) };
        let actual = raw_stat(target.as_raw_fd())?;
        let required = libc::ST_RDONLY | libc::ST_NOSUID | libc::ST_NODEV | libc::ST_NOEXEC;
        if (actual.st_dev, actual.st_ino) != (metadata.st_dev, metadata.st_ino)
            || raw_statvfs(target.as_raw_fd())?.f_flag & required != required
        {
            return Err(raw_refusal());
        }
        Ok(())
    }

    fn observed_mount_id(descriptor: BorrowedFd<'_>) -> Result<u64> {
        let mut observed = std::mem::MaybeUninit::<libc::statx>::zeroed();
        if unsafe {
            libc::statx(
                descriptor.as_raw_fd(),
                c"".as_ptr(),
                libc::AT_EMPTY_PATH | libc::AT_SYMLINK_NOFOLLOW,
                libc::STATX_MNT_ID,
                observed.as_mut_ptr(),
            )
        } != 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        let observed = unsafe { observed.assume_init() };
        if observed.stx_mask & libc::STATX_MNT_ID == 0 || observed.stx_mnt_id == 0 {
            bail!("kernel did not report an actual mount identity");
        }
        Ok(observed.stx_mnt_id)
    }

    fn root_relative(root: &File, path: &[u8], flags: i32) -> Result<File> {
        // Paths are validated GuestPath spellings, and parent resolution never
        // follows graph symlinks while inspecting setup-created endpoints.
        let parent = mkdir_parents(root, path)?;
        let name = CString::new(path.rsplit(|byte| *byte == b'/').next().unwrap())?;
        let fd = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                name.as_ptr(),
                flags | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(unsafe { File::from_raw_fd(fd) })
    }

    fn mkdir_parents(root: &File, path: &[u8]) -> Result<File> {
        let mut parent = root.try_clone()?;
        let parts: Vec<_> = path[1..].split(|byte| *byte == b'/').collect();
        for part in &parts[..parts.len() - 1] {
            let name = CString::new(*part)?;
            if unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o755) } != 0
                && std::io::Error::last_os_error().raw_os_error() != Some(libc::EEXIST)
            {
                return Err(std::io::Error::last_os_error().into());
            }
            let next = unsafe {
                libc::openat(
                    parent.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if next < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            parent = unsafe { File::from_raw_fd(next) };
        }
        Ok(parent)
    }

    fn create_link(root: &File, path: &GuestPath, target: &[u8]) -> Result<()> {
        let parent = mkdir_parents(root, path.as_bytes())?;
        let name = CString::new(path.as_bytes().rsplit(|byte| *byte == b'/').next().unwrap())?;
        let target = CString::new(target)?;
        if unsafe { libc::symlinkat(target.as_ptr(), parent.as_raw_fd(), name.as_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(())
    }

    fn bind_fd(
        root: &File,
        path: &GuestPath,
        source: BorrowedFd<'_>,
        executable: bool,
        expected: Option<CapturedIdentity>,
    ) -> Result<()> {
        let metadata = File::from(source.try_clone_to_owned()?).metadata()?;
        let parent = mkdir_parents(root, path.as_bytes())?;
        let name = CString::new(path.as_bytes().rsplit(|byte| *byte == b'/').next().unwrap())?;
        if metadata.is_dir() {
            if unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o555) } != 0 {
                return Err(std::io::Error::last_os_error().into());
            }
        } else {
            let fd = unsafe {
                libc::openat(
                    parent.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_CREAT
                        | libc::O_EXCL
                        | libc::O_WRONLY
                        | libc::O_CLOEXEC
                        | libc::O_NOFOLLOW,
                    0o444,
                )
            };
            if fd < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            drop(unsafe { File::from_raw_fd(fd) });
        }
        let from = CString::new(format!("/proc/self/fd/{}", source.as_raw_fd()))?;
        let mut target = format!("/proc/self/fd/{}/", parent.as_raw_fd()).into_bytes();
        target.extend_from_slice(name.as_bytes());
        let target = CString::new(target)?;
        if unsafe {
            libc::mount(
                from.as_ptr(),
                target.as_ptr(),
                std::ptr::null(),
                libc::MS_BIND,
                std::ptr::null(),
            )
        } != 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        let mut source_view = std::mem::MaybeUninit::<libc::statvfs>::uninit();
        if unsafe { libc::fstatvfs(source.as_raw_fd(), source_view.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let source_flags = unsafe { source_view.assume_init() }.f_flag;
        // Character devices need their device semantics; the root itself stays
        // nodev and only these explicit child mounts can expose supported nodes.
        use std::os::unix::fs::FileTypeExt;
        let mut flags = libc::MS_BIND
            | libc::MS_REMOUNT
            | libc::MS_RDONLY
            | libc::MS_NOSUID
            | if metadata.file_type().is_char_device() {
                0
            } else {
                libc::MS_NODEV
            }
            | if executable { 0 } else { libc::MS_NOEXEC };
        for (probe, flag) in [
            (libc::ST_NOSUID, libc::MS_NOSUID),
            (libc::ST_NODEV, libc::MS_NODEV),
            (libc::ST_NOEXEC, libc::MS_NOEXEC),
            (libc::ST_NOATIME, libc::MS_NOATIME),
            (libc::ST_NODIRATIME, libc::MS_NODIRATIME),
        ] {
            if source_flags & probe != 0 {
                flags |= flag;
            }
        }
        if executable && flags & libc::MS_NOEXEC != 0 {
            bail!("executable held source is on a noexec mount");
        }
        if unsafe {
            libc::mount(
                std::ptr::null(),
                target.as_ptr(),
                std::ptr::null(),
                flags,
                std::ptr::null(),
            )
        } != 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        let fd = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                name.as_ptr(),
                libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let actual = unsafe { File::from_raw_fd(fd) };
        let actual_meta = actual.metadata()?;
        let mut actual_view = std::mem::MaybeUninit::<libc::statvfs>::uninit();
        if unsafe { libc::fstatvfs(actual.as_raw_fd(), actual_view.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let actual_flags = unsafe { actual_view.assume_init() }.f_flag;
        if actual_flags & libc::ST_RDONLY == 0
            || (actual_flags & libc::ST_NOEXEC != 0) == executable
        {
            bail!("bound root object mount policy differs");
        }
        if actual_meta.dev() != metadata.dev() || actual_meta.ino() != metadata.ino() {
            bail!("bound root object identity changed");
        }
        if let Some(expected) = expected
            && (actual_meta.dev() != expected.device
                || actual_meta.ino() != expected.inode
                || actual_meta.len() != expected.size
                || unsafe { libc::fcntl(source.as_raw_fd(), libc::F_GET_SEALS) } != SEALS)
        {
            bail!("bound sealed object changed");
        }
        Ok(())
    }
    #[cfg(test)]
    mod tests {
        use super::super::super::native_exec_inputs::OpenedGuestRoot;
        use super::*;

        fn requested_copy(path: &[u8], source: &File, executable: bool) -> RootCopy {
            let source_identity = checked_file_identity(source, executable, true).unwrap();
            RootCopy {
                path: GuestPath::new(path.to_vec()).unwrap(),
                source: source.try_clone().unwrap(),
                expected: source_identity,
                executable,
            }
        }

        #[test]
        fn readonly_root_copy_checks_real_seals_bytes_and_conflicting_roles() {
            let source = super::super::super::native_exec_inputs::seal_bytes(
                b"captured copy",
                InputRole::Data,
            )
            .unwrap();
            let root_dir = tempfile::tempdir().unwrap();
            let root = File::open(root_dir.path()).unwrap();
            let request = requested_copy(b"/input", &source, false);
            let expected = request.expected;
            let copied = copy_root_groups(&root, vec![request]).unwrap();
            let copy = open_root_file(&root, b"/input").unwrap();
            let identity = checked_file_identity(&copy, false, false).unwrap();
            assert_eq!(identity.size, expected.size);
            assert_eq!(identity.digest, expected.digest);
            assert_ne!(
                (identity.device, identity.inode),
                (expected.device, expected.inode)
            );
            assert_eq!(copied.len(), 1);
            assert_eq!(
                unsafe { libc::fcntl(source.as_raw_fd(), libc::F_GET_SEALS) },
                SEALS
            );
            assert_eq!(
                source.write_at(b"X", 0).unwrap_err().raw_os_error(),
                Some(libc::EPERM)
            );
            let mut wrong = requested_copy(b"/other", &source, false);
            wrong.expected.digest[0] ^= 1;
            assert!(copy_root_groups(&root, vec![wrong]).is_err());
            let mut wrong = requested_copy(b"/role", &source, false);
            wrong.executable = true;
            assert!(copy_root_groups(&root, vec![wrong]).is_err());
            let ordinary = tempfile::tempfile().unwrap();
            ordinary.write_all_at(b"captured copy", 0).unwrap();
            assert!(checked_file_identity(&ordinary, false, true).is_err());
            copy.write_at(b"X", 0).unwrap_err(); // actual retained copy is O_RDONLY
            assert_eq!(unsafe { libc::fchmod(copy.as_raw_fd(), 0o644) }, 0);
            let writable = File::options()
                .write(true)
                .open(root_dir.path().join("input"))
                .unwrap();
            writable.write_all_at(b"X", 0).unwrap();
            assert_eq!(unsafe { libc::fchmod(copy.as_raw_fd(), 0o444) }, 0);
            assert_ne!(
                checked_file_identity(&copy, false, false).unwrap().digest,
                expected.digest
            );
        }

        #[test]
        fn actual_readonly_root_backings_group_aliases_and_refuse_changed_owners() {
            let inputs = data_inputs();
            let source =
                super::super::super::native_exec_inputs::seal_bytes(b"same bytes", InputRole::Data)
                    .unwrap();
            let distinct =
                super::super::super::native_exec_inputs::seal_bytes(b"same bytes", InputRole::Data)
                    .unwrap();
            let code = super::super::super::native_exec_inputs::seal_bytes(
                b"executable bytes",
                InputRole::Executable,
            )
            .unwrap();
            let mut additions = DeclaredRootAdditions::default();
            for (path, file) in [
                (b"/a".as_slice(), &source),
                (b"/b", &source),
                (b"/c", &distinct),
            ] {
                additions
                    .add_file(
                        RuntimeFile::new(
                            GuestPath::new(path.to_vec()).unwrap(),
                            file.as_fd(),
                            false,
                            FilePlacement::Additional,
                        )
                        .unwrap(),
                    )
                    .unwrap();
            }
            additions
                .add_file(
                    RuntimeFile::new(
                        GuestPath::new(b"/exec".to_vec()).unwrap(),
                        code.as_fd(),
                        true,
                        FilePlacement::Additional,
                    )
                    .unwrap(),
                )
                .unwrap();
            let expected = checked_file_identity(&source, false, true).unwrap();
            let mut plan = root_plan(inputs, additions).unwrap();
            let mut container = super::super::default_container(false);
            plan.configure(&mut container).unwrap();
            let result: Result<(), String> = container
                .run(|| {
                    let mut work = || -> Result<()> {
                        let prepared = unsafe { plan.materialize()? };
                        let a_path = GuestPath::new(b"/a".to_vec())?;
                        let a = prepared.backing(&a_path, expected)?.clone();
                        let b = prepared.backing(&GuestPath::new(b"/b".to_vec())?, expected)?;
                        let c = prepared.backing(
                            &GuestPath::new(b"/c".to_vec())?,
                            checked_file_identity(&distinct, false, true)?,
                        )?;
                        assert_eq!(a.identity(), b.identity());
                        assert_ne!(a.identity().inode, c.identity().inode);
                        assert_eq!(a.identity().digest, c.identity().digest);
                        assert_ne!(a.mount_id(), b.mount_id());
                        for backing in [&a, b, c] {
                            backing.verify()?;
                        }
                        // Exercise the same literal symlink helper used for
                        // declared aliases, without treating the alias itself
                        // as a separately registered source receipt.
                        let alias = GuestPath::new(b"/alias".to_vec())?;
                        create_link(&prepared.root, &alias, b"a")?;
                        let alias_fd = unsafe {
                            libc::openat(
                                prepared.root.as_raw_fd(),
                                c"alias".as_ptr(),
                                libc::O_RDONLY | libc::O_CLOEXEC,
                            )
                        };
                        if alias_fd < 0 {
                            return Err(std::io::Error::last_os_error().into());
                        }
                        let alias_file = unsafe { File::from_raw_fd(alias_fd) };
                        assert_eq!(
                            checked_file_identity(&alias_file, false, false)?,
                            a.identity()
                        );
                        assert!(prepared.backing(&alias, expected).is_err());
                        assert!(open_root_file(&prepared.root, b"/alias").is_err());
                        let executable = prepared.backing(
                            &GuestPath::new(b"/exec".to_vec())?,
                            checked_file_identity(&code, true, true)?,
                        )?;
                        executable.verify()?;
                        assert!(executable.executable());
                        assert_eq!(
                            raw_statvfs(executable.descriptor().as_raw_fd())?.f_flag
                                & libc::ST_NOEXEC,
                            0
                        );
                        let flags = raw_statvfs(a.descriptor().as_raw_fd())?.f_flag;
                        assert_eq!(
                            flags
                                & (libc::ST_RDONLY
                                    | libc::ST_NOSUID
                                    | libc::ST_NODEV
                                    | libc::ST_NOEXEC),
                            libc::ST_RDONLY | libc::ST_NOSUID | libc::ST_NODEV | libc::ST_NOEXEC
                        );
                        let mut wrong_source = expected;
                        wrong_source.inode ^= 1;
                        assert!(prepared.backing(&a_path, wrong_source).is_err());
                        assert!(
                            prepared
                                .backing(&GuestPath::new(b"/absent".to_vec())?, expected)
                                .is_err()
                        );
                        let readonly_write = a.0.backing.write_at(b"X", 0).unwrap_err();
                        assert_eq!(readonly_write.raw_os_error(), Some(libc::EBADF));
                        let mut wrong_mount = (*a.0).try_clone_for_test()?;
                        wrong_mount.mount_id ^= 1;
                        assert!(ReadonlyRootBacking(Arc::new(wrong_mount)).verify().is_err());
                        let mut wrong_namespace = (*a.0).try_clone_for_test()?;
                        wrong_namespace.namespace_identity.1 ^= 1;
                        assert!(
                            ReadonlyRootBacking(Arc::new(wrong_namespace))
                                .verify()
                                .is_err()
                        );
                        let parent = mkdir_parents(&prepared.root, b"/a")?;
                        let c_target =
                            CString::new(format!("/proc/self/fd/{}/c", parent.as_raw_fd()))?;
                        assert_eq!(
                            unsafe { libc::umount2(c_target.as_ptr(), libc::MNT_DETACH) },
                            0
                        );
                        assert_eq!(
                            unsafe { libc::unlinkat(parent.as_raw_fd(), c"c".as_ptr(), 0) },
                            0
                        );
                        let replacement = open_new_root_file(&prepared.root, b"/c", 0o444)?;
                        replacement.write_all_at(b"same bytes", 0)?;
                        drop(replacement);
                        assert!(c.verify().is_err()); // equal-byte replacement is a different owner
                        a.verify()?; // the separate same-source a/b group remains sound
                        let target =
                            CString::new(format!("/proc/self/fd/{}/a", parent.as_raw_fd()))?;
                        let mut atime_flags = 0;
                        for (observed, flag) in [
                            (libc::ST_NOATIME, libc::MS_NOATIME),
                            (libc::ST_NODIRATIME, libc::MS_NODIRATIME),
                            (ROOT_ST_RELATIME, libc::MS_RELATIME),
                        ] {
                            if flags & observed != 0 {
                                atime_flags |= flag;
                            }
                        }
                        let remount = |flags| -> Result<()> {
                            if unsafe {
                                libc::mount(
                                    std::ptr::null(),
                                    target.as_ptr(),
                                    std::ptr::null(),
                                    flags | atime_flags,
                                    std::ptr::null(),
                                )
                            } != 0
                            {
                                return Err(std::io::Error::last_os_error().into());
                            }
                            Ok(())
                        };
                        // Each refusal follows a real kernel policy/path mutation.
                        remount(
                            libc::MS_BIND
                                | libc::MS_REMOUNT
                                | libc::MS_RDONLY
                                | libc::MS_NOSUID
                                | libc::MS_NODEV,
                        )?;
                        assert!(a.verify().is_err()); // missing noexec alone
                        remount(
                            libc::MS_BIND
                                | libc::MS_REMOUNT
                                | libc::MS_RDONLY
                                | libc::MS_NOSUID
                                | libc::MS_NODEV
                                | libc::MS_NOEXEC,
                        )?;
                        a.verify()?;
                        remount(
                            libc::MS_BIND
                                | libc::MS_REMOUNT
                                | libc::MS_NOSUID
                                | libc::MS_NODEV
                                | libc::MS_NOEXEC,
                        )?;
                        assert!(a.verify().is_err());
                        let writable =
                            open_root_file_with_flags(&prepared.root, b"/a", libc::O_WRONLY)?;
                        writable.write_all_at(b"X", 0)?;
                        drop(writable);
                        remount(
                            libc::MS_BIND
                                | libc::MS_REMOUNT
                                | libc::MS_RDONLY
                                | libc::MS_NOSUID
                                | libc::MS_NODEV
                                | libc::MS_NOEXEC,
                        )?;
                        assert!(a.verify().is_err()); // corrupt destination survives restored policy
                        assert!(b.verify().is_err()); // physical a/b aliases share the changed bytes
                        remount(
                            libc::MS_BIND
                                | libc::MS_REMOUNT
                                | libc::MS_NOSUID
                                | libc::MS_NODEV
                                | libc::MS_NOEXEC,
                        )?;
                        let writable =
                            open_root_file_with_flags(&prepared.root, b"/a", libc::O_WRONLY)?;
                        writable.set_len(1)?;
                        drop(writable);
                        remount(
                            libc::MS_BIND
                                | libc::MS_REMOUNT
                                | libc::MS_RDONLY
                                | libc::MS_NOSUID
                                | libc::MS_NODEV
                                | libc::MS_NOEXEC,
                        )?;
                        assert!(a.verify().is_err()); // actual truncation, separately restored mount policy
                        Ok(())
                    };
                    work().map_err(|error| format!("{error:#}"))
                })
                .unwrap();
            result.unwrap();
        }

        fn root_plan(
            inputs: Arc<HeldDynamicInputs>,
            mut additions: DeclaredRootAdditions,
        ) -> Result<DeclaredInputRoot> {
            if additions.common_group.is_none() {
                additions.capture_common_inputs()?;
            }
            DeclaredInputRoot::prepare(inputs, additions, &std::env::temp_dir())
        }

        fn data_inputs() -> Arc<HeldDynamicInputs> {
            let source = tempfile::tempdir().unwrap();
            std::fs::write(source.path().join("input"), b"sealed input").unwrap();
            let opened = OpenedGuestRoot::open(source.path()).unwrap();
            Arc::new(
                HeldDynamicInputs::capture_data_control(
                    &opened,
                    GuestPath::new(b"/input".to_vec()).unwrap(),
                )
                .unwrap(),
            )
        }

        #[test]
        fn verify_plans_retain_one_actual_common_group_owner() {
            let inputs = data_inputs();
            assert!(
                DeclaredInputRoot::prepare(
                    inputs.clone(),
                    DeclaredRootAdditions::default(),
                    &std::env::temp_dir(),
                )
                .is_err()
            );
            let mut additions = DeclaredRootAdditions::default();
            additions.capture_common_inputs().unwrap();
            assert!(additions.capture_common_inputs().is_err());
            let second_additions = additions.try_clone().unwrap();
            assert!(Arc::ptr_eq(
                additions.common_group.as_ref().unwrap(),
                second_additions.common_group.as_ref().unwrap(),
            ));
            let first =
                DeclaredInputRoot::prepare(inputs.clone(), additions, &std::env::temp_dir())
                    .unwrap();
            let second =
                DeclaredInputRoot::prepare(inputs, second_additions, &std::env::temp_dir())
                    .unwrap();
            let group = |plan: &DeclaredInputRoot| {
                plan.runtime_files()
                    .iter()
                    .find(|file| file.path().as_bytes() == b"/etc/group")
                    .unwrap()
                    .identity()
            };
            // Device/inode, size and SHA all match one actual sealed object;
            // equal generated text alone cannot satisfy this comparison.
            assert_eq!(group(&first), group(&second));
            assert_ne!(first.stage.path(), second.stage.path());
            assert_eq!(
                unsafe {
                    libc::fcntl(
                        first
                            .additions
                            .common_group
                            .as_ref()
                            .unwrap()
                            .descriptor()
                            .as_raw_fd(),
                        libc::F_GET_SEALS,
                    )
                },
                SEALS
            );
        }

        #[test]
        fn runtime_additions_require_real_seals_and_exact_executable_mode() {
            let ordinary = tempfile::NamedTempFile::new().unwrap();
            assert!(
                RuntimeFile::new(
                    GuestPath::new(b"/tool/config".to_vec()).unwrap(),
                    ordinary.as_file().as_fd(),
                    false,
                    FilePlacement::Additional
                )
                .is_err()
            );
            let sealed = super::super::super::native_exec_inputs::seal_bytes(
                b"runtime bytes",
                InputRole::Data,
            )
            .unwrap();
            assert!(
                RuntimeFile::new(
                    GuestPath::new(b"/tool/config".to_vec()).unwrap(),
                    sealed.as_fd(),
                    true,
                    FilePlacement::Additional
                )
                .is_err()
            );
            let actual = RuntimeFile::new(
                GuestPath::new(b"/tool/config".to_vec()).unwrap(),
                sealed.as_fd(),
                false,
                FilePlacement::Additional,
            )
            .unwrap();
            let cloned = actual.try_clone().unwrap();
            assert_eq!(actual.identity(), cloned.identity());
            assert_ne!(
                actual.descriptor().as_raw_fd(),
                cloned.descriptor().as_raw_fd()
            );
        }

        #[test]
        fn root_plan_refuses_duplicate_files_and_unenrolled_interpreter_replacement() {
            let inputs = data_inputs();
            let sealed = super::super::super::native_exec_inputs::seal_bytes(
                b"bootstrap",
                InputRole::Executable,
            )
            .unwrap();
            let replacement = RuntimeFile::new(
                GuestPath::new(b"/loader".to_vec()).unwrap(),
                sealed.as_fd(),
                true,
                FilePlacement::InterpreterReplacement,
            )
            .unwrap();
            let mut additions = DeclaredRootAdditions::default();
            additions.add_file(replacement).unwrap();
            assert!(root_plan(inputs.clone(), additions).is_err());
            let mut additions = DeclaredRootAdditions::default();
            additions
                .add_file(
                    RuntimeFile::new(
                        GuestPath::new(b"/input".to_vec()).unwrap(),
                        sealed.as_fd(),
                        true,
                        FilePlacement::Additional,
                    )
                    .unwrap(),
                )
                .unwrap();
            assert!(root_plan(inputs, additions).is_err());
        }

        #[test]
        fn root_setup_refuses_parent_materialization_and_duplicate_configuration() {
            fn requires_send_sync<T: Send + Sync>() {}
            requires_send_sync::<GuestDeclaredRootSetup>();
            let mut plan = root_plan(data_inputs(), DeclaredRootAdditions::default()).unwrap();
            let mut container = super::super::default_container(false);
            assert!(unsafe { plan.materialize() }.is_err());
            plan.configure(&mut container).unwrap();
            assert!(plan.configure(&mut container).is_err());
            // The host staging directory has not been covered by the actual
            // child tmpfs. Data/configuration alone cannot manufacture a root.
            assert!(unsafe { plan.materialize() }.is_err());
            assert!(unsafe { plan.materialize() }.is_err());
        }

        #[test]
        fn actual_native_endpoints_survive_declared_root_mount_namespace() {
            use std::os::unix::fs::FileTypeExt;
            use std::os::unix::net::UnixListener;

            use super::super::super::native_exec_run::NativeEndpoints;

            fn socket_view(path: &Path) -> (u64, u64, u32, libc::c_ulong) {
                let path = CString::new(path.as_os_str().as_bytes()).unwrap();
                let raw = unsafe {
                    libc::open(
                        path.as_ptr(),
                        libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                    )
                };
                assert!(
                    raw >= 0,
                    "socket open failed: {}",
                    std::io::Error::last_os_error()
                );
                let fd = unsafe { File::from_raw_fd(raw) };
                let metadata = fd.metadata().unwrap();
                assert!(metadata.file_type().is_socket());
                let mut view = std::mem::MaybeUninit::<libc::statvfs>::uninit();
                assert_eq!(
                    unsafe { libc::fstatvfs(fd.as_raw_fd(), view.as_mut_ptr()) },
                    0
                );
                (
                    metadata.dev(),
                    metadata.ino(),
                    metadata.mode() & libc::S_IFMT,
                    unsafe { view.assume_init() }.f_flag,
                )
            }

            // Use actual endpoint binding and root planning. The long lexical
            // source path cannot be an AF_UNIX bind spelling; the real binder
            // must retain its short procfd spelling without using it as a
            // child-namespace mount source. No native Tool or memfd file bind
            // is exercised: this control stops at the actual setup boundary.
            let backing = tempfile::Builder::new()
                .prefix(&format!("native-endpoint-{}", "x".repeat(120)))
                .tempdir()
                .unwrap();
            let endpoints = NativeEndpoints::bind(backing.path()).unwrap();
            let socket_directory = std::fs::read_dir(backing.path())
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .find(|path| {
                    path.file_name()
                        .unwrap()
                        .as_bytes()
                        .starts_with(b"native-sockets-")
                })
                .unwrap();
            let rpc = socket_directory.join("rpc.sock");
            let unix_address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
            assert!(rpc.as_os_str().as_bytes().len() >= unix_address.sun_path.len());
            let short_stats = reverie_inguest::initial_image_stats::initial_image_stats_endpoint(
                Path::new("/run/rpc.sock"),
            )
            .unwrap();
            let stats = socket_directory.join(short_stats.file_name().unwrap());
            let sources = [
                (Endpoint::Rpc, rpc.clone()),
                (Endpoint::Statistics, stats),
                (Endpoint::Provision, socket_directory.join("provision.sock")),
            ];
            let mut additions = DeclaredRootAdditions::default();
            additions.capture_common_inputs().unwrap();
            endpoints.add_to(&mut additions).unwrap();
            assert_eq!(additions.endpoints.len(), 3);
            let expected = sources.map(|(kind, source)| {
                let identity = socket_view(&source);
                let (_, _, device, inode) = additions
                    .endpoints
                    .iter()
                    .find(|(candidate, _, _, _)| *candidate == kind)
                    .unwrap();
                assert_eq!((identity.0, identity.1), (*device, *inode));
                (kind, identity)
            });
            let inputs = data_inputs();
            let mut plan = DeclaredInputRoot::prepare(
                inputs.clone(),
                additions.try_clone().unwrap(),
                backing.path(),
            )
            .unwrap();
            let namespace = std::fs::metadata("/proc/self/ns/mnt").unwrap();
            let parent_namespace = (namespace.dev(), namespace.ino());
            let mut container = super::super::default_container(false);
            plan.configure(&mut container).unwrap();
            container
                .run(|| {
                    let namespace = std::fs::metadata("/proc/self/ns/mnt").unwrap();
                    assert_ne!((namespace.dev(), namespace.ino()), parent_namespace);
                    for (kind, source) in expected {
                        let actual = socket_view(&plan.stage.path().join(&kind.path()[1..]));
                        assert_eq!(
                            (actual.0, actual.1, actual.2),
                            (source.0, source.1, source.2)
                        );
                        assert_eq!(
                            actual.3,
                            source.3
                                | libc::ST_RDONLY
                                | libc::ST_NOSUID
                                | libc::ST_NODEV
                                | libc::ST_NOEXEC,
                        );
                    }
                    for (_, pin, _, _) in &plan.additions.endpoints {
                        pin.retire().unwrap();
                    }
                    plan.stage_pin.retire().unwrap();
                })
                .unwrap();
            // Actual wait/reap has completed. A new socket at the same lexical
            // name must still be refused before the child workload, rather
            // than gaining admission from the retained directory alone.
            let mut replaced =
                DeclaredInputRoot::prepare(inputs, additions, backing.path()).unwrap();
            std::fs::rename(&rpc, socket_directory.join("held-rpc.sock")).unwrap();
            let directory = File::open(&socket_directory).unwrap();
            let short_rpc = format!("/proc/self/fd/{}/rpc.sock", directory.as_raw_fd());
            let _replacement = UnixListener::bind(&short_rpc).unwrap();
            let mut container = super::super::default_container(false);
            replaced.configure(&mut container).unwrap();
            match container.run(|| -> () { panic!("replacement endpoint reached workload") }) {
                Err(reverie::process::RunError::Spawn(error)) => {
                    assert_eq!(error.context(), reverie::process::Context::Mount);
                    assert_eq!(
                        std::io::Error::from(error).raw_os_error(),
                        Some(libc::ESTALE)
                    );
                }
                other => panic!("expected exact endpoint replacement refusal, received {other:?}"),
            }
            drop(endpoints);
        }

        #[test]
        fn actual_common_container_materializes_readonly_data_and_same_task_proc() {
            // Filesystem component only: no Tool installation/native filter,
            // InstalledExecDomain or native exec success is represented here.
            // Init builds the graph; a real forked guest child binds its proc.
            let inputs = data_inputs();
            let mut plan = root_plan(inputs.clone(), DeclaredRootAdditions::default()).unwrap();
            let mut container = super::super::default_container(false);
            plan.configure(&mut container).unwrap();
            let result: Result<(i32, i32, [u8; 8]), String> = container
                .run(|| {
                    let mut work = || -> Result<_> {
                        let mut prepared = unsafe { plan.materialize()? };
                        assert_ne!(prepared.mount_id(), 0);
                        let setup_pid = unsafe { libc::getpid() };
                        let mut guest = prepared.prepare_guest_setup(Path::new("/work"))?;
                        assert!(prepared.prepare_guest_setup(Path::new("/work")).is_err());
                        let mut pair = [-1; 2];
                        if unsafe { libc::pipe2(pair.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
                            return Err(std::io::Error::last_os_error().into());
                        }
                        let read = unsafe { File::from_raw_fd(pair[0]) };
                        let write = unsafe { File::from_raw_fd(pair[1]) };
                        let pid = unsafe { libc::fork() };
                        if pid < 0 {
                            return Err(std::io::Error::last_os_error().into());
                        }
                        if pid == 0 {
                            // The production operation itself uses no allocation.
                            // Raw fixture observations execute only after it returns.
                            let mut check = || -> std::io::Result<()> {
                                let receipt = unsafe { guest.bind_and_enter()? };
                                if receipt.visible_pid() != unsafe { libc::getpid() }
                                    || receipt.visible_pid() == setup_pid
                                    || receipt.namespace() != prepared.namespace()
                                    || receipt.device() != prepared.device()
                                    || receipt.inode() != prepared.inode()
                                    || receipt.mount_id() != prepared.mount_id()
                                {
                                    return Err(raw_refusal());
                                }
                                assert_eq!(
                                    unsafe { guest.bind_and_enter() }
                                        .err()
                                        .unwrap()
                                        .raw_os_error(),
                                    Some(libc::EINVAL)
                                );
                                let mut actual_cwd = [0u8; 32];
                                if unsafe {
                                    libc::getcwd(actual_cwd.as_mut_ptr().cast(), actual_cwd.len())
                                }
                                .is_null()
                                    || &actual_cwd[..6] != b"/work\0"
                                {
                                    return Err(raw_refusal());
                                }
                                let input = unsafe {
                                    libc::open(c"/input".as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC)
                                };
                                if input < 0 {
                                    return Err(std::io::Error::last_os_error());
                                }
                                let input = unsafe { File::from_raw_fd(input) };
                                let mut bytes = [0; 32];
                                let n = unsafe {
                                    libc::read(
                                        input.as_raw_fd(),
                                        bytes.as_mut_ptr().cast(),
                                        bytes.len(),
                                    )
                                };
                                if n != 12 || &bytes[..12] != b"sealed input" {
                                    return Err(raw_refusal());
                                }
                                let denied = unsafe {
                                    libc::open(c"/input".as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC)
                                };
                                if denied >= 0 {
                                    unsafe {
                                        libc::close(denied);
                                    }
                                    return Err(raw_refusal());
                                }
                                if std::io::Error::last_os_error().raw_os_error()
                                    != Some(libc::EROFS)
                                {
                                    return Err(raw_refusal());
                                }
                                let scratch = unsafe {
                                    libc::open(
                                        c"/tmp/scratch".as_ptr(),
                                        libc::O_WRONLY
                                            | libc::O_CREAT
                                            | libc::O_EXCL
                                            | libc::O_CLOEXEC,
                                        0o600,
                                    )
                                };
                                if scratch < 0 {
                                    return Err(std::io::Error::last_os_error());
                                }
                                let scratch = unsafe { File::from_raw_fd(scratch) };
                                if unsafe {
                                    libc::write(scratch.as_raw_fd(), b"scratch".as_ptr().cast(), 7)
                                } != 7
                                {
                                    return Err(raw_refusal());
                                }
                                let status = unsafe {
                                    libc::open(
                                        c"/proc/self/status".as_ptr(),
                                        libc::O_RDONLY | libc::O_CLOEXEC,
                                    )
                                };
                                if status < 0 {
                                    return Err(std::io::Error::last_os_error());
                                }
                                let status = unsafe { File::from_raw_fd(status) };
                                let mut text = [0u8; 4096];
                                let n = unsafe {
                                    libc::read(
                                        status.as_raw_fd(),
                                        text.as_mut_ptr().cast(),
                                        text.len(),
                                    )
                                };
                                if n <= 0 {
                                    return Err(raw_refusal());
                                }
                                let mut expected = RawPath::new();
                                expected.push(b"Pid:\t")?;
                                expected.number(receipt.visible_pid() as u32)?;
                                expected.push(b"\n")?;
                                if !text[..n as usize]
                                    .windows(expected.len)
                                    .any(|row| row == expected.as_bytes())
                                {
                                    return Err(raw_refusal());
                                }
                                for alias in [c"/proc/self/status", c"/proc/thread-self/status"] {
                                    let fd = unsafe {
                                        libc::open(alias.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC)
                                    };
                                    if fd < 0 {
                                        return Err(std::io::Error::last_os_error());
                                    }
                                    drop(unsafe { File::from_raw_fd(fd) });
                                }
                                let missing = unsafe {
                                    libc::open(
                                        c"/usr/bin/unlisted".as_ptr(),
                                        libc::O_RDONLY | libc::O_CLOEXEC,
                                    )
                                };
                                if missing >= 0 {
                                    unsafe {
                                        libc::close(missing);
                                    }
                                    return Err(raw_refusal());
                                }
                                if std::io::Error::last_os_error().raw_os_error()
                                    != Some(libc::ENOENT)
                                {
                                    return Err(raw_refusal());
                                }
                                let mut packet = [0u8; 8];
                                packet[..4].copy_from_slice(&receipt.visible_pid().to_ne_bytes());
                                packet[4..].copy_from_slice(b"PASS");
                                if unsafe {
                                    libc::write(
                                        write.as_raw_fd(),
                                        packet.as_ptr().cast(),
                                        packet.len(),
                                    )
                                } != packet.len() as isize
                                {
                                    return Err(raw_refusal());
                                }
                                Ok(())
                            };
                            let code = if check().is_ok() { 0 } else { 96 };
                            unsafe {
                                libc::_exit(code);
                            }
                        }
                        drop(write);
                        let mut status = 0;
                        if unsafe { libc::waitpid(pid, &mut status, 0) } != pid || status != 0 {
                            bail!("actual guest root child {pid} failed: wait={status:#x}");
                        }
                        let mut packet = [0u8; 8];
                        use std::io::Read;
                        (&read).read_exact(&mut packet)?;
                        Ok((setup_pid, pid, packet))
                    };
                    work().map_err(|error| format!("{error:#}"))
                })
                .unwrap();
            let (setup_pid, pid, packet) = result.unwrap();
            assert_eq!(setup_pid, 1);
            assert!(pid > setup_pid);
            assert_eq!(i32::from_ne_bytes(packet[..4].try_into().unwrap()), pid);
            assert_eq!(&packet[4..], b"PASS");
            inputs.recheck().unwrap();
        }
    }
}

/// A [`default_container`] hardened with the deterministic identity mounts
/// (frozen `/etc/group`, hidden nscd cache) that `run` mode applies. Replay
/// uses this so guest NSS resolution matches `run` and does not reach
/// nondeterministic host identity state. The returned [`IdentityGuard`] must be
/// held until after `Container::run` returns.
pub(super) fn deterministic_container() -> Result<(Container, IdentityGuard), Error> {
    let mut container = default_container(true);
    let (mounts, identity_guard) = identity_hardening_mounts()?;
    container.mounts(mounts);
    Ok((container, identity_guard))
}

/// Tie the container's init process to the lifetime of the `hermit` process
/// that created it.
///
/// [`Container::run`] forks the process that carries the guest into a fresh PID
/// namespace, so that process is PID 1 *of that namespace*. Linux gives a
/// namespace init the same protection it gives the system init: a signal whose
/// disposition is `SIG_DFL` is discarded rather than delivered, even when the
/// sender is in an ancestor namespace. Only `SIGKILL` and `SIGSTOP` from an
/// ancestor still get through.
///
/// That is why an external deadline could not end a hung run. `timeout N`
/// sends `SIGTERM` to the process group; the outer `hermit` process obeys it
/// and dies, the init process discards it, `timeout` sees its own direct child
/// gone and exits 124 without ever escalating, and the init process is
/// reparented to host PID 1 and keeps running. `timeout --kill-after` does not
/// help, because the escalation is conditioned on that already-dead direct
/// child. Three runs survived this way for more than 45 hours and filled the
/// filesystem.
///
/// `PR_SET_PDEATHSIG` closes that hole from inside: when the thread that forked
/// this process exits, the kernel sends us `SIGKILL`, which is precisely one of
/// the two signals a namespace init cannot ignore. Every existing caller keeps
/// working and no exit code changes, which is why this lives here rather than
/// in each of the ~20 scripts that wrap `hermit` in `timeout`.
///
/// Reverie uses the same idiom for exec'd untraced members in
/// `safeptrace/src/notifier.rs`.
///
/// If the parent died between the fork and this call, no death signal will
/// ever arrive, so the callers check right after it whether the parent is
/// already gone. A parent that crossed `--max-log-bytes` set the shared
/// crossing bit before it exited 123 (`exit_if_log_cap_already_crossed`, which
/// carries the ordering argument). A parent that died for any other reason in
/// the window (killed from outside, OOM, a crash) is seen through the pidfd it
/// opened for itself before it forked (`exit_if_parent_already_gone`). The
/// usual `getppid()` guard is unavailable here, because inside a new PID
/// namespace the out-of-namespace parent is not mapped and `getppid()` reports
/// 0 whether or not it is alive. Under load that window, container setup
/// between the clone and this call, was long enough for an outright SIGKILL of
/// the hermit CLI to land in it: 2 of 180 runs of
/// `container_init_dies_with_the_hermit_process_that_forked_it` on 4 loaded
/// CPUs (2026-10-16), each leaving the init and its guest running.
fn arm_parent_death_signal() -> Result<(), Error> {
    // SAFETY: `PR_SET_PDEATHSIG` only sets the calling thread's parent-death
    // signal attribute. It reads and writes no caller memory, and the remaining
    // arguments are ignored for this option.
    if unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) } == -1 {
        return Err(std::io::Error::last_os_error()).context(
            "Failed to arm the container init parent-death signal \
             (prctl(PR_SET_PDEATHSIG, SIGKILL)); refusing to start a guest that \
             an expiring deadline could not then kill",
        );
    }
    Ok(())
}

/// The signals whose default action is to terminate the process and which a
/// supervisor, a shell, or a terminal actually sends when it wants a run to
/// stop. Every one of them is discarded by the kernel while the container init
/// leaves it at `SIG_DFL`, so all three are dead letters today: `SIGTERM` from
/// a deadline, `SIGINT` from Ctrl-C, and `SIGHUP` when the owning terminal or
/// agent session goes away.
const CONTAINER_INIT_STOP_SIGNALS: [Signal; 3] = [Signal::SIGTERM, Signal::SIGINT, Signal::SIGHUP];

/// Terminate the container init on a stop signal.
///
/// The exit code is the conventional `128 + signo` a shell reports for a
/// process killed by that signal, rather than a genuine signalled wait status,
/// because a namespace init cannot produce the latter. The usual way to honour
/// a signal faithfully -- restore `SIG_DFL`, unblock, re-raise -- is measurably
/// unavailable here: a plain process dies from that sequence with
/// `WIFSIGNALED` set, but a namespace init survives it, because a self-sent
/// signal does not come from an ancestor namespace and so is discarded exactly
/// like the original. Reporting `128 + signo` is the closest honest
/// approximation available.
///
/// Exiting is a complete teardown, not just this process leaving: the kernel
/// `SIGKILL`s every remaining member of a PID namespace whose init exits, so
/// the guest goes with it instead of being orphaned into a namespace with no
/// init.
extern "C" fn on_container_init_stop_signal(signal: libc::c_int) {
    // SAFETY: `_exit` is async-signal-safe and is the only thing this handler
    // does. It deliberately skips destructors: there is nothing in this process
    // whose cleanup is worth blocking a requested shutdown on, and the
    // caller-side guards live in the parent process.
    unsafe { libc::_exit(128 + signal) }
}

/// Make the container init answer the signals a supervisor actually sends.
///
/// [`arm_parent_death_signal`] handles the case where the `hermit` process
/// dies. This handles the case where it does not: a `SIGTERM` aimed at the run
/// itself, a Ctrl-C, or a `SIGHUP` when the session ends. The kernel discards
/// those only while the disposition is `SIG_DFL`; installing any handler at all
/// is what makes them deliverable to a namespace init, so this both restores
/// the expected behaviour and gives hermit a place to shut a run down on
/// purpose rather than being shot.
///
/// The inherited signal mask matters as much as the disposition. A blocked
/// signal stays pending forever and the handler never runs, which would leave
/// the same silent no-op with a handler installed to suggest otherwise, so the
/// mask is cleared for these three. `record_start.rs` learned the same lesson
/// about `SIGALRM`.
///
/// This runs only after Reverie has set up the container. Until then the init
/// relies on the [`ContainerInitStopSignalBridge`] it inherited at clone.
fn install_container_init_stop_handlers() -> Result<(), Error> {
    let action = SigAction::new(
        SigHandler::Handler(on_container_init_stop_signal),
        SaFlags::empty(),
        SigSet::empty(),
    );

    let mut unblock = SigSet::empty();
    for signal in CONTAINER_INIT_STOP_SIGNALS {
        // SAFETY: the handler performs only `_exit`, which is
        // async-signal-safe, and the action outlives the call.
        unsafe { sigaction(signal, &action) }
            .with_context(|| format!("Failed to install the container init {signal} handler"))?;
        unblock.add(signal);
    }

    unblock
        .thread_unblock()
        .context("Failed to unblock the container init stop signals")?;

    Ok(())
}

/// The pid of the process that installed a [`ContainerInitStopSignalBridge`].
///
/// [`bridge_container_init_stop_signal`] compares it with `getpid()` to tell
/// the `hermit` process, which installed the bridge, apart from a container init
/// that inherited it at clone. A container init with its own PID namespace sees
/// itself as PID 1, and one without sees its own host PID, so neither matches
/// unless the `hermit` process is itself PID 1; see
/// [`ContainerInitStopSignalBridge::install`] for that case.
static STOP_SIGNAL_BRIDGE_HOLDER: AtomicI32 = AtomicI32::new(0);

/// The stop-signal disposition the `hermit` process holds while it clones a
/// container init. See [`ContainerInitStopSignalBridge`].
///
/// In the container init this is [`on_container_init_stop_signal`]: exit
/// `128 + signo`, and so take the namespace down with it.
///
/// In the `hermit` process it reproduces `SIG_DFL`. It restores the default
/// disposition, unblocks the signal and raises it again, so the process dies
/// from the signal with the same wait status it would have had without the
/// bridge. If the `hermit` process is itself a namespace init, the kernel
/// discards the raised signal exactly as it would have discarded the original,
/// and the handler returns.
extern "C" fn bridge_container_init_stop_signal(signal: libc::c_int) {
    // SAFETY: getpid(2) is async-signal-safe, and so is a lock-free atomic load.
    if unsafe { libc::getpid() } != STOP_SIGNAL_BRIDGE_HOLDER.load(Ordering::Relaxed) {
        // Does not return.
        on_container_init_stop_signal(signal);
    }
    // SAFETY: sigaction(2), sigemptyset(3), sigaddset(3), sigprocmask(2) and
    // raise(3) are async-signal-safe, and every pointer passed refers to a local
    // that outlives the call.
    unsafe {
        let mut default: libc::sigaction = std::mem::zeroed();
        default.sa_sigaction = libc::SIG_DFL;
        libc::sigaction(signal, &default, std::ptr::null_mut());
        let mut unblock: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut unblock);
        libc::sigaddset(&mut unblock, signal);
        libc::sigprocmask(libc::SIG_UNBLOCK, &unblock, std::ptr::null_mut());
        libc::raise(signal);
    }
}

/// The action currently installed for `signal`, read without changing it.
fn current_signal_action(signal: Signal) -> Result<libc::sigaction, Error> {
    // SAFETY: a null new action makes sigaction(2) a pure query, and `current`
    // is a correctly sized out-parameter that is read only on success.
    let mut current: libc::sigaction = unsafe { std::mem::zeroed() };
    if unsafe { libc::sigaction(signal as libc::c_int, std::ptr::null(), &mut current) } == -1 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("Failed to read the {signal} disposition"));
    }
    Ok(current)
}

/// Stop signals aimed at a container init before it arms its own handlers.
///
/// [`install_container_init_stop_handlers`] runs as the first statement of the
/// container closure. Reverie calls that closure only after it has set up the
/// container: identity maps, mounts, the root change and the new `/proc`. The
/// container init is a PID-namespace init from the moment `clone` returns, and
/// it is visible in `/proc` from then on. For that whole setup window it has
/// whatever disposition it inherited from `hermit`, which is normally
/// `SIG_DFL`, so a `SIGTERM`, `SIGINT` or `SIGHUP` aimed at it is discarded by
/// the kernel at send time. The handler installed a moment later cannot recover
/// a signal that was never queued, so the run keeps going as if nobody had
/// asked it to stop.
///
/// Measured before this guard existed, with a probe that signalled each run's
/// init as soon as it appeared in `/proc`: 12 of 60 signals were sent while
/// `/proc` still showed no handler, and 9 of those 12 were lost (the init was
/// still running 3 seconds later). None of the other 48 was lost. With the guard
/// in place, none of 60 was lost. The integration test
/// `container_init_honours_signals_aimed_at_it_directly` failed this way 6 times
/// in one batch of 360 runs on a loaded host, and none in another.
///
/// No hermit code runs in the container init before Reverie's setup, so the
/// disposition has to be in place when it is created. This guard installs
/// [`bridge_container_init_stop_signal`] in the `hermit` process around the
/// clone, and the new init inherits it. Reverie's setup leaves these
/// dispositions alone: it clears the signal mask and resets only `SIGPIPE`.
/// The container init then replaces the bridge with its own handler.
///
/// Only `SIG_DFL` dispositions are bridged. The bridge reproduces the default
/// action in the `hermit` process exactly, but it cannot reproduce `SIG_IGN`
/// without turning an ignored signal into an interrupted system call, and a
/// handler someone installed deliberately is not ours to replace. A stop signal
/// that `hermit` itself ignores, such as `SIGHUP` under `nohup`, therefore
/// stays discarded until the container init arms.
///
/// Holding a mask instead of a handler would not work. The container init would
/// queue a blocked signal, but Reverie's setup unblocks everything before the
/// handler exists, and the kernel then discards a signal that is still at
/// `SIG_DFL` or `SIG_IGN`. Blocking the signals would also defer a `SIGTERM`
/// aimed at `hermit` itself for the whole run.
#[must_use = "the bridge is removed when this guard is dropped"]
pub(super) struct ContainerInitStopSignalBridge {
    /// Which entries of [`CONTAINER_INIT_STOP_SIGNALS`] this guard bridged.
    bridged: [bool; CONTAINER_INIT_STOP_SIGNALS.len()],
    /// The exact `sa_sigaction` value this guard installed. Drop compares the
    /// current action with this value rather than taking the function's
    /// address again, because Rust does not promise that one function has only
    /// one address.
    handler: libc::sighandler_t,
}

impl ContainerInitStopSignalBridge {
    /// Bridge every stop signal that is currently at `SIG_DFL`.
    ///
    /// Call this in the thread that clones the container init, immediately
    /// before the clone. The query and the install are not one atomic step, so
    /// this relies on the requirement Container already places on its caller:
    /// no other thread is running.
    pub(super) fn install() -> Result<Self, Error> {
        let handler: extern "C" fn(libc::c_int) = bridge_container_init_stop_signal;
        let mut guard = Self {
            bridged: [false; CONTAINER_INIT_STOP_SIGNALS.len()],
            handler: handler as libc::sighandler_t,
        };
        // SAFETY: getpid(2) has no preconditions.
        let holder = unsafe { libc::getpid() };
        if holder == 1 {
            // The container init with its own PID namespace would also see
            // itself as PID 1, so the bridge could not tell the two processes
            // apart. The bridge stays out of the way here, and the container
            // init behaves as it did before the bridge existed. A `hermit` that
            // is PID 1 is a namespace init, so the kernel discards these signals
            // for it in any case.
            return Ok(guard);
        }
        STOP_SIGNAL_BRIDGE_HOLDER.store(holder, Ordering::Relaxed);

        // SAFETY: an all-zero sigaction is a valid value.
        let mut bridge: libc::sigaction = unsafe { std::mem::zeroed() };
        bridge.sa_sigaction = guard.handler;
        bridge.sa_flags = libc::SA_RESTART;
        // SAFETY: sigemptyset(3) and sigaddset(3) only write the mask they are
        // given, which is a field of the local above.
        unsafe {
            libc::sigemptyset(&mut bridge.sa_mask);
            for signal in CONTAINER_INIT_STOP_SIGNALS {
                libc::sigaddset(&mut bridge.sa_mask, signal as libc::c_int);
            }
        }
        for (index, signal) in CONTAINER_INIT_STOP_SIGNALS.into_iter().enumerate() {
            if current_signal_action(signal)?.sa_sigaction != libc::SIG_DFL {
                continue;
            }
            // SAFETY: in the container init the handler performs only `_exit`.
            // In this process it performs only the async-signal-safe calls
            // listed in its body. `bridge` outlives the call.
            if unsafe { libc::sigaction(signal as libc::c_int, &bridge, std::ptr::null_mut()) }
                == -1
            {
                return Err(std::io::Error::last_os_error()).with_context(|| {
                    format!("Failed to install the container init {signal} bridge")
                });
            }
            guard.bridged[index] = true;
        }
        Ok(guard)
    }
}

impl Drop for ContainerInitStopSignalBridge {
    fn drop(&mut self) {
        let default = SigAction::new(SigHandler::SigDfl, SaFlags::empty(), SigSet::empty());
        for (index, signal) in CONTAINER_INIT_STOP_SIGNALS.into_iter().enumerate() {
            if !self.bridged[index] {
                continue;
            }
            // Undo only our own bridge. The handler may already have restored
            // `SIG_DFL`, and any other action was installed after us on purpose.
            let still_bridged = current_signal_action(signal)
                .is_ok_and(|current| current.sa_sigaction == self.handler);
            if still_bridged {
                // SAFETY: restoring the default action installs no handler.
                let _ = unsafe { sigaction(signal, &default) };
            }
        }
    }
}

/// Test-only: hold a freshly cloned container init before it arms its guards,
/// so a test can aim a stop signal at an init that does not yet have its own
/// handlers.
///
/// Without this the window is well under a millisecond to a few milliseconds of
/// container setup, and an integration test lands in it only by chance: 6 times
/// in one batch of 360 runs on a loaded host, and none in another.
/// [`CONTAINER_INIT_ARMING_STALL`] is longer than any budget that test
/// waits for, so a signal sent within those budgets is necessarily sent, and
/// acted on, before the container init installed anything itself.
///
/// This is keyed off an environment variable rather than `cfg(test)` for the
/// same reason as `run_timeout::stall_the_unwind_if_asked`: the bridge lives in
/// the shipped binary and must be exercised there. The variable is read in the
/// container init from the `hermit` process's environment, never from the
/// guest's.
pub(super) fn stall_container_init_before_arming_if_asked() {
    if std::env::var_os(CONTAINER_INIT_ARMING_STALL_ENV).as_deref()
        != Some(std::ffi::OsStr::new("1"))
    {
        return;
    }
    // Rename the thread for the length of the stall, so the test can see that
    // the init really is waiting here, rather than inferring it from timing or
    // passing against a binary that ignores the variable.
    let mut name = [0u8; 16];
    // SAFETY: PR_GET_NAME writes at most 16 bytes, including the terminator,
    // into `name`; PR_SET_NAME reads a NUL-terminated string of at most 16.
    unsafe {
        libc::prctl(libc::PR_GET_NAME, name.as_mut_ptr());
        libc::prctl(libc::PR_SET_NAME, c"unarmed-init".as_ptr());
    }
    // The stall ends early if the parent exits, so a test can place the
    // parent's death in this window without waiting out the whole stall.
    wait_for_parent_exit(CONTAINER_INIT_ARMING_STALL);
    // SAFETY: as above; `name` is still NUL-terminated.
    unsafe { libc::prctl(libc::PR_SET_NAME, name.as_ptr()) };
}

/// A pidfd for this hermit process, opened in `main` before anything can fork,
/// with the device and inode it had then. Every container init this process
/// forks inherits the descriptor (it is closed only at exec), so the init can
/// tell whether the process that forked it is still alive: a pidfd becomes
/// readable once its process has exited. `None` when the kernel has no
/// `pidfd_open` (before Linux 5.3); the death signal is then all there is.
static PARENT_PIDFD: OnceLock<Option<(OwnedFd, u64, u64)>> = OnceLock::new();

/// Open [`PARENT_PIDFD`]. Called from `main` before anything can fork.
pub(super) fn open_parent_pidfd_for_container_inits() {
    PARENT_PIDFD.get_or_init(|| {
        // SAFETY: `pidfd_open` takes a pid and flags and touches no caller
        // memory. The descriptor it returns is close-on-exec.
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, libc::getpid(), 0) };
        if fd < 0 {
            return None;
        }
        // SAFETY: `fd` was just returned by `pidfd_open` and nothing else owns it.
        let fd = unsafe { OwnedFd::from_raw_fd(fd as libc::c_int) };
        let (dev, ino) = fd_identity(fd.as_raw_fd())?;
        Some((fd, dev, ino))
    });
}

/// The device and inode of `fd`, by `fstat`, which is async-signal-safe.
fn fd_identity(fd: libc::c_int) -> Option<(u64, u64)> {
    // SAFETY: an all-zero `stat` is a valid value for `fstat` to overwrite.
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: `stat` is a valid, writable `struct stat`.
    (unsafe { libc::fstat(fd, &mut stat) } == 0).then_some((stat.st_dev, stat.st_ino))
}

/// The inherited [`PARENT_PIDFD`], if this process still holds that very file.
fn inherited_parent_pidfd() -> Option<libc::c_int> {
    let (fd, dev, ino) = PARENT_PIDFD.get()?.as_ref()?;
    let fd = fd.as_raw_fd();
    (fd_identity(fd) == Some((*dev, *ino))).then_some(fd)
}

/// Wait up to `timeout` for the process that forked this one to exit, and say
/// whether it has. Without an inherited pidfd it cannot tell: it waits the
/// whole `timeout` and says no. Async-signal-safe: `fstat`, `poll` and
/// `nanosleep` only.
fn wait_for_parent_exit(timeout: std::time::Duration) -> bool {
    let Some(fd) = inherited_parent_pidfd() else {
        let pause = libc::timespec {
            tv_sec: timeout.as_secs() as libc::time_t,
            tv_nsec: timeout.subsec_nanos() as libc::c_long,
        };
        // SAFETY: `pause` is a valid `timespec`; the remainder is not wanted.
        unsafe { libc::nanosleep(&pause, std::ptr::null_mut()) };
        return false;
    };
    let mut ready = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let millis = libc::c_int::try_from(timeout.as_millis()).unwrap_or(libc::c_int::MAX);
    // SAFETY: one valid `pollfd`.
    (unsafe { libc::poll(&mut ready, 1, millis) } == 1) && ready.revents & libc::POLLIN != 0
}

/// Exit now if the hermit process that forked this container init has already
/// exited. Called right after `prctl(PR_SET_PDEATHSIG)`: a parent that died
/// before that call will never deliver the signal, and nothing else would end
/// this init or the guest it is about to start. The exit status is the
/// `128 + SIGKILL` a shell reports for the death signal that never came; a
/// namespace init cannot be killed by a signal it sends itself.
///
/// Async-signal-safe (`fstat`, `poll`, `_exit`), so it is sound in `pre_exec`.
pub(super) fn exit_if_parent_already_gone() {
    if wait_for_parent_exit(std::time::Duration::ZERO) {
        // SAFETY: `_exit` has no preconditions and is async-signal-safe.
        unsafe { libc::_exit(128 + libc::SIGKILL) }
    }
}

/// See [`stall_container_init_before_arming_if_asked`].
pub(super) const CONTAINER_INIT_ARMING_STALL_ENV: &str =
    "HERMIT_INTERNAL_STALL_CONTAINER_INIT_BEFORE_ARMING";

/// See [`stall_container_init_before_arming_if_asked`]. The integration test
/// `container_init_honours_signals_sent_before_it_arms` copies this value and
/// the thread name the stall sets.
pub(super) const CONTAINER_INIT_ARMING_STALL: std::time::Duration =
    std::time::Duration::from_secs(60);

/// Set inside a container init by [`exit_init_after_publication`].
static EXIT_AFTER_PUBLICATION: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Called by work running in a container init that leaves a thread behind (a
/// detached stdin reader that may stay blocked): the init exits as soon as its
/// result is published instead of waiting for that thread, with the status
/// the init returns anyway. Each init is its own process, so this never
/// affects the caller.
#[cfg(feature = "dbt")]
pub(super) fn exit_init_after_publication() {
    EXIT_AFTER_PUBLICATION.store(true, Ordering::Release);
}

/// Whether [`exit_init_after_publication`] was called in this process.
pub(super) fn exits_init_after_publication() -> bool {
    EXIT_AFTER_PUBLICATION.load(Ordering::Acquire)
}

/// Arm both container-init guards. Call this as the FIRST statement inside any
/// `Container::run` closure that is not going through [`with_container`].
///
/// Why this is public rather than folded into [`with_container`]: `hermit record`
/// -- every spelling, including `record --verify` -- calls [`Container::run`]
/// directly at six sites in `record_start.rs` and never goes through
/// `with_container`. Those containers come from `recording_container()` ->
/// `configured_container()` -> `default_container(true)`, which unshares
/// `Namespace::PID`, so each one is a namespace init with exactly the bug the
/// guards exist to fix. An adversarial review of the original change caught this:
/// the claim that "all entry points funnel through `with_container`" was FALSE,
/// and `record --verify` is precisely the long-running command an external
/// deadline most needs to be able to end.
///
/// The structurally better home for this is reverie's `Container::run`, before
/// `setup()`, which would also close the rest of the fork-to-prctl window (the
/// part `--max-log-bytes` opens is closed here, by the check right after the
/// `prctl`). That is a reverie-side change plus a pin bump; this closes the six
/// sites now.
pub(super) fn arm_container_init_guards() -> Result<(), SerializableError> {
    arm_parent_death_signal().map_err(SerializableError::from)?;
    // A parent that died before the `prctl` above will never send its death
    // signal; nothing would end this init, so end it here.
    exit_if_parent_already_gone();
    // Right after the death signal and before anything can start a guest: a
    // parent that crossed --max-log-bytes and exited 123 before the `prctl`
    // above will never send it, so stop here instead.
    super::tracing::exit_if_log_cap_already_crossed();
    install_container_init_stop_handlers().map_err(SerializableError::from)?;
    Ok(())
}

/// The `run --namespace-only` counterpart of [`arm_container_init_guards`], run
/// from Reverie's `pre_exec` in the forked child that then execs the guest as
/// PID 1 of its namespace. Only the death signal is armed; see the launch
/// site in `run.rs` for why the stop handlers are not.
///
/// It runs between fork and exec, so it must stay async-signal-safe: `prctl`,
/// `poll` and `fstat` on the inherited pidfd, one atomic read-modify-write,
/// `_exit` and the error value only.
pub(super) fn arm_namespace_only_guest() -> Result<(), reverie::Errno> {
    // SAFETY: as in `arm_parent_death_signal`: it reads and writes no caller
    // memory.
    if unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) } == -1 {
        return Err(reverie::Errno::last());
    }
    exit_if_parent_already_gone();
    // In this mode the parent is the only hermit process and holds the log
    // subscriber, so it is the likeliest crosser: see
    // `exit_if_log_cap_already_crossed`.
    super::tracing::exit_if_log_cap_already_crossed();
    Ok(())
}

/// The i386 syscall number of `kill`, reached by `int $0x80` from any process.
const I386_KILL: i32 = 37;
/// `AUDIT_ARCH_I386`, the architecture seccomp reports for an i386 syscall.
const AUDIT_ARCH_I386: u32 = 0x4000_0003;

/// The seccomp filter for a `--namespace-only` guest: every `kill` stops for
/// [`police_group_kills`], and so does every i386 syscall (the filter cannot
/// single out the i386 `kill`; without a rule the filter would kill a process
/// that makes one); every other syscall runs untouched. On a kernel with the
/// x32 ABI enabled, an x32 `kill` is not seen.
pub(super) fn group_kill_filter() -> reverie::process::seccomp::Filter {
    use reverie::process::seccomp::Action;
    use reverie::process::seccomp::TargetArch;
    reverie::process::seccomp::FilterBuilder::new()
        .default_action(Action::Allow)
        .syscall(reverie::syscalls::Sysno::kill, Action::Notify)
        .alternate_arch(TargetArch::x86, Action::Notify)
        .build()
}

/// Whether a seccomp notification is a `kill`: the x86_64 one, or the i386 one.
fn is_kill(nr: i32, arch: u32) -> bool {
    if arch == AUDIT_ARCH_I386 {
        nr == I386_KILL
    } else {
        nr == libc::SYS_kill as i32
    }
}

/// Whether to refuse a guest's `kill` with these arguments: a signal to the
/// caller's own process group (`pid` 0, `sig` a valid signal number; the
/// kernel answers an invalid one with EINVAL) while that group is
/// Hermit's (`caller_group`, read from `/proc`; `None` when it could not be).
///
/// Under `--namespace-only` the guest shares Hermit's process group, and
/// nothing else intercepts it, so such a `kill(0, sig)` would reach Hermit, the
/// rest of Hermit's pipeline and, run without job control, the shell, make or
/// test runner that started it (https://github.com/rrnewton/hermit/issues/4065).
/// No other form can reach outside: inside its PID namespace the guest cannot
/// name Hermit's group (`getpgrp` reads 0 there), so `kill(-pgid)` and
/// `kill(-1)` stay in the namespace. A guest process that leads a group of its
/// own keeps its `kill(0)`.
pub(super) fn refuses_group_kill(
    args: [u64; 6],
    caller_group: Option<i32>,
    hermit_group: i32,
) -> bool {
    args[0] as i32 == 0
        && (1..=libc::SIGRTMAX()).contains(&(args[1] as i32))
        && caller_group.is_none_or(|group| group == hermit_group)
}

/// The process group of host `pid`, from `/proc/<pid>/stat`, or `None`.
fn process_group(pid: u32) -> Option<i32> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command name may hold spaces and parentheses: fields resume after
    // the last ')', as state, ppid, pgrp.
    stat.rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(2)?
        .parse()
        .ok()
}

/// Install [`group_kill_filter`] in the forked `--namespace-only` guest and
/// send its listener to Hermit over `socket` ([`receive_group_kill_listener`]).
///
/// Run from Reverie's `pre_exec`, after its mounts and `chdir`. Hermit
/// installs the filter here, not through Reverie's own seccomp support, whose
/// parent waits without end for a listener that a child failing earlier in its
/// setup (a missing `--workdir`) never publishes. A child that fails before
/// this point closes its end of `socket` as it exits, and Reverie reports its
/// error. The guest's copy of the listener is closed before it returns, so the
/// guest cannot answer its own notifications.
///
/// It sets no `no_new_privs`, which the guest would see: the guest is root of
/// its own user namespace (`map_root`), and that capability is what installing
/// a filter without `no_new_privs` requires. The filter is still visible to
/// the guest (`Seccomp: 2` in `/proc/self/status`), a seccomp listener of its
/// own fails with EBUSY, and a Hermit run inside it refuses the inherited
/// filter; the `--namespace-only` help and the user guide say so.
///
/// It runs between fork and exec, so it must stay async-signal-safe:
/// `seccomp`, `sendmsg` and `close`. The filter stops only `kill`, so these
/// calls run untouched after it is installed.
pub(super) fn install_group_kill_filter(
    filter: &reverie::process::seccomp::Filter,
    socket: i32,
) -> Result<(), reverie::Errno> {
    let listener = match filter.load_and_listen() {
        Ok(listener) => listener,
        Err(errno) => {
            // Tell Hermit which step failed (`group_kill_filter_failure`):
            // Reverie reports only that a `pre_exec` callback failed.
            let raw = errno.into_raw().to_le_bytes();
            let record = [GROUP_KILL_FILTER_FAILED, raw[0], raw[1], raw[2], raw[3]];
            // SAFETY: `send` reads the five bytes of a local.
            unsafe { libc::send(socket, record.as_ptr().cast(), record.len(), 0) };
            return Err(errno);
        }
    };
    let fd = listener.as_raw_fd();
    let mut byte = 0u8;
    let mut iov = libc::iovec {
        iov_base: (&mut byte as *mut u8).cast(),
        iov_len: 1,
    };
    let mut control = [0u64; 4];
    // SAFETY: an all-zero msghdr is a valid empty message.
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &mut iov;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    // SAFETY: CMSG_SPACE only computes a size.
    message.msg_controllen =
        unsafe { libc::CMSG_SPACE(std::mem::size_of::<i32>() as u32) } as usize;
    // SAFETY: the control buffer holds one header and one descriptor
    // (CMSG_SPACE(4) is 24 of its 32 bytes), and the message points only at
    // locals that outlive the call.
    let sent = unsafe {
        let header = libc::CMSG_FIRSTHDR(&message);
        (*header).cmsg_level = libc::SOL_SOCKET;
        (*header).cmsg_type = libc::SCM_RIGHTS;
        (*header).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<i32>() as u32) as usize;
        std::ptr::write(libc::CMSG_DATA(header).cast::<i32>(), fd);
        libc::sendmsg(socket, &message, 0)
    };
    if sent != 1 {
        return Err(reverie::Errno::last());
    }
    drop(listener);
    Ok(())
}

/// The first byte of the record [`install_group_kill_filter`] sends when the
/// filter cannot be installed, followed by the errno, little-endian.
const GROUP_KILL_FILTER_FAILED: u8 = b'F';

/// The errno with which the guest failed to install [`group_kill_filter`],
/// when that is why its launch failed: read from the record
/// [`install_group_kill_filter`] sent over `socket`. `None` when the launch
/// failed for another reason, before the filter was attempted.
pub(super) fn group_kill_filter_failure(socket: &OwnedFd) -> Option<i32> {
    let mut record = [0u8; 8];
    // SAFETY: `recv` writes at most the buffer's length into a local.
    let received = unsafe {
        libc::recv(
            socket.as_raw_fd(),
            record.as_mut_ptr().cast(),
            record.len(),
            libc::MSG_DONTWAIT,
        )
    };
    (received == 5 && record[0] == GROUP_KILL_FILTER_FAILED)
        .then(|| i32::from_le_bytes([record[1], record[2], record[3], record[4]]))
}

/// The listener [`install_group_kill_filter`] sent over `socket`, once the
/// guest has started: the message is already queued, so this does not wait.
pub(super) fn receive_group_kill_listener(socket: &OwnedFd) -> std::io::Result<OwnedFd> {
    let mut byte = 0u8;
    let mut iov = libc::iovec {
        iov_base: (&mut byte as *mut u8).cast(),
        iov_len: 1,
    };
    let mut control = [0u64; 4];
    // SAFETY: as in `install_group_kill_filter`.
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &mut iov;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    message.msg_controllen = std::mem::size_of_val(&control);
    // SAFETY: the message points only at locals that outlive the call.
    let received = unsafe {
        libc::recvmsg(
            socket.as_raw_fd(),
            &mut message,
            libc::MSG_DONTWAIT | libc::MSG_CMSG_CLOEXEC,
        )
    };
    if received != 1 {
        return Err(if received < 0 {
            std::io::Error::last_os_error()
        } else {
            std::io::Error::other("the guest sent no seccomp listener")
        });
    }
    // SAFETY: the kernel filled the control buffer; a header with SCM_RIGHTS
    // carries one descriptor, now owned here.
    unsafe {
        let header = libc::CMSG_FIRSTHDR(&message);
        if header.is_null()
            || (*header).cmsg_level != libc::SOL_SOCKET
            || (*header).cmsg_type != libc::SCM_RIGHTS
        {
            return Err(std::io::Error::other(
                "the guest's message carried no seccomp listener",
            ));
        }
        Ok(OwnedFd::from_raw_fd(std::ptr::read(
            libc::CMSG_DATA(header).cast::<i32>(),
        )))
    }
}

/// Answer the `--namespace-only` guest's `kill` calls on `listener` until no
/// task the filter covers is left ([`group_kill_filter`]) or `stop` becomes
/// readable: refuse with EPERM the ones [`refuses_group_kill`] names, saying
/// so on Hermit's stderr, and let every other one run as issued.
pub(super) fn police_group_kills(listener: OwnedFd, stop: OwnedFd) {
    use reverie::process::seccomp::SECCOMP_IOCTL_NOTIF_ID_VALID;
    use reverie::process::seccomp::SECCOMP_IOCTL_NOTIF_RECV;
    use reverie::process::seccomp::SECCOMP_IOCTL_NOTIF_SEND;
    use reverie::process::seccomp::SECCOMP_USER_NOTIF_FLAG_CONTINUE;
    use reverie::process::seccomp::seccomp_notif;
    use reverie::process::seccomp::seccomp_notif_resp;
    // SAFETY: getpgrp reads no memory.
    let hermit_group = unsafe { libc::getpgrp() };
    let fd = listener.as_raw_fd();
    loop {
        let mut ready = [
            libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: stop.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // SAFETY: two pollfds that live across the call.
        if unsafe { libc::poll(ready.as_mut_ptr(), 2, -1) } < 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            // Every later guest `kill` then fails with ENOSYS.
            tracing::warn!(
                "hermit stopped answering the --namespace-only guest's kill calls: polling \
                 their seccomp listener failed ({error}); each one now fails with ENOSYS \
                 (https://github.com/rrnewton/hermit/issues/4065)"
            );
            return;
        }
        if ready[1].revents != 0 || ready[0].revents & (libc::POLLHUP | libc::POLLERR) != 0 {
            return;
        }
        if ready[0].revents & libc::POLLIN == 0 {
            continue;
        }
        // SAFETY: the kernel requires a zeroed notification, which it fills.
        let mut notification: seccomp_notif = unsafe { std::mem::zeroed() };
        // SAFETY: as above.
        if unsafe { libc::ioctl(fd, SECCOMP_IOCTL_NOTIF_RECV as _, &mut notification) } < 0 {
            // ENOENT: the caller died after the listener became readable.
            continue;
        }
        let args = notification.data.args;
        let refuse = is_kill(notification.data.nr, notification.data.arch)
            && refuses_group_kill(args, process_group(notification.pid), hermit_group);
        // The caller may have died while its group was read.
        // SAFETY: the ioctl reads the id it is given.
        if unsafe { libc::ioctl(fd, SECCOMP_IOCTL_NOTIF_ID_VALID as _, &notification.id) } < 0 {
            continue;
        }
        let response = if refuse {
            tracing::warn!(
                "hermit refused the guest's kill(0, {}) from pid {} under --namespace-only \
                 with EPERM: its process group is Hermit's, so the signal would reach Hermit and \
                 the processes that started it, where Linux would deliver it \
                 (https://github.com/rrnewton/hermit/issues/4065)",
                args[1] as i32,
                notification.pid,
            );
            seccomp_notif_resp {
                id: notification.id,
                val: 0,
                error: -libc::EPERM,
                flags: 0,
            }
        } else {
            seccomp_notif_resp {
                id: notification.id,
                val: 0,
                error: 0,
                flags: SECCOMP_USER_NOTIF_FLAG_CONTINUE,
            }
        };
        // SAFETY: the ioctl reads the response it is given.
        unsafe { libc::ioctl(fd, SECCOMP_IOCTL_NOTIF_SEND as _, &response) };
    }
}

/// Where the most recent panic inside the container child happened.
///
/// `catch_unwind` hands back the panic PAYLOAD but not the LOCATION, and the
/// location is the more useful half of a divergence report. A panic hook is the
/// only place the location is available, so record it there and read it back
/// after the unwind.
static PANIC_LOCATION: OnceLock<Mutex<Option<String>>> = OnceLock::new();

fn panic_location() -> &'static Mutex<Option<String>> {
    PANIC_LOCATION.get_or_init(|| Mutex::new(None))
}

/// Install the location-recording hook, once, in the container child.
///
/// The previous hook is still called, so the ordinary `thread '...' panicked
/// at ...` report continues to reach stderr; this only ADDS a machine-readable
/// copy of the location.
fn install_panic_location_hook() {
    static INSTALLED: OnceLock<()> = OnceLock::new();
    INSTALLED.get_or_init(|| {
        let previous = panic::take_hook();
        panic::set_hook(Box::new(move |info| {
            if let Some(location) = info.location() {
                let recorded = format!(
                    "{}:{}:{}",
                    location.file(),
                    location.line(),
                    location.column()
                );
                if let Ok(mut slot) = panic_location().lock() {
                    *slot = Some(recorded);
                }
            }
            previous(info);
        }));
    });
}

fn take_panic_location() -> String {
    panic_location()
        .lock()
        .ok()
        .and_then(|mut slot| slot.take())
        .unwrap_or_else(|| "<unknown location>".to_string())
}

fn panic_message(payload: &(dyn Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&'static str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "<panic payload was not a string>".to_string()
    }
}

/// Run `f`, converting a panic into an ordinary error.
///
/// WHY THIS EXISTS. The container child's entry point is `extern "C"` (see
/// `reverie_process::clone::clone_with_stack`) and is therefore NOUNWIND. A
/// panic that reaches it hits `panic_cannot_unwind` and ABORTS, and the parent
/// reports that abort as `Signaled(SIGSEGV, true)` -- indistinguishable from a
/// genuine memory fault, with the cause carried nowhere except stderr. That has
/// already sent one investigation hunting a bad dereference that did not exist.
/// Catching the unwind HERE, in Hermit's own closure, means the panic never
/// reaches the nounwind frame and instead travels the result channel
/// `Container::run` already provides.
///
/// This deliberately does NOT change how a REAL fault behaves: `catch_unwind`
/// cannot catch `SIGSEGV`, so a genuine memory fault in the child still aborts
/// and is still reported as a signal. Keeping those two outcomes
/// distinguishable is the point of the change, so both directions are covered
/// by tests.
/// Deliberate fault injection for the two-directional test below. INERT unless
/// `HERMIT_TEST_CONTAINER_CHILD_FAULT` is set, so it cannot fire in normal use.
///
/// Both arms are required, and the SECOND is the one that gives the test its
/// force: if a real fault stopped reporting as a signal after this change, the
/// change would have made a genuine memory error indistinguishable from a
/// caught panic, which is the same blindness in the other direction.
/// ⚠️ AND IT TAKES THE CALL SITE, BECAUSE A FAULT THAT CANNOT BE AIMED CANNOT
/// TEST MORE THAN THE FIRST SITE IT REACHES. `record` enters a container at six
/// sites; with only `HERMIT_TEST_CONTAINER_CHILD_FAULT` set, the FIRST child to
/// run faults and every later stage is never entered. Two of the six -- the
/// replay stages of `--verify` and `--verify-with-gdbex` -- are therefore
/// unreachable by any test, so their classification was asserted rather than
/// measured. `HERMIT_TEST_CONTAINER_CHILD_FAULT_SITE` names which one to hit.
///
/// A LABEL RATHER THAN AN OCCURRENCE INDEX, deliberately. An index is positional:
/// it silently retargets the moment a call site is added, removed or reordered,
/// and the test keeps passing while aiming somewhere else. A label is identity,
/// and identity is the thing whose absence made these two sites untestable. An
/// occurrence counter also cannot be process-local -- each `run_guarded` forks a
/// fresh child and this runs in the CHILD, so a static counter resets to zero
/// every time.
///
/// Unset `..._SITE` keeps the previous behaviour exactly: fault at whichever site
/// is reached first. Existing callers and tests are unaffected.
///
/// `block` is not a fault of its own: the child announces the site on stderr
/// and waits there until something outside kills it. That gives a test a
/// fixed point at which to kill a container child from outside, instead of
/// racing a stage that may already have finished.
fn inject_test_fault(site: &str) {
    if std::env::var("HERMIT_TEST_CONTAINER_CHILD_FAULT_SITE").is_ok_and(|want| want != site) {
        return;
    }
    match std::env::var("HERMIT_TEST_CONTAINER_CHILD_FAULT")
        .ok()
        .as_deref()
    {
        Some("panic") => {
            panic!("deliberate container-child panic for fault-injection testing at site {site}")
        }
        Some("segv") => {
            // A genuine memory fault, NOT a panic. catch_unwind must not touch this.
            unsafe { std::ptr::null_mut::<u8>().write_volatile(1) };
        }
        Some("block") => {
            eprintln!("hermit: test fault: container child blocked at site {site}");
            loop {
                std::thread::park();
            }
        }
        _ => {}
    }
}

/// Returns a [`SerializableError`] rather than a bare [`Error`] so the CLASS
/// survives: a caught panic is tagged HERE, at the only point that still knows
/// one happened, and the tag then crosses the process boundary with the message.
#[cfg(test)]
fn catch_child_panic<F, T>(f: &mut F) -> Result<T, SerializableError>
where
    F: FnMut() -> Result<T, Error>,
{
    install_panic_location_hook();
    match panic::catch_unwind(panic::AssertUnwindSafe(|| {
        inject_test_fault("with_container");
        f()
    })) {
        Ok(result) => result.map_err(SerializableError::from),
        Err(payload) => {
            let location = take_panic_location();
            Err(SerializableError::from(anyhow!(
                "panic in container child at {location}: {}",
                panic_message(&*payload)
            ))
            .into_panic())
        }
    }
}

/// Same child panic boundary with the original explicit call-site fault label.
pub(super) fn catch_child_panic_at<F, T>(
    site: &'static str,
    mut f: F,
) -> Result<T, SerializableError>
where
    F: FnMut() -> Result<T, Error>,
{
    install_panic_location_hook();
    match panic::catch_unwind(panic::AssertUnwindSafe(|| {
        inject_test_fault(site);
        f()
    })) {
        Ok(result) => result.map_err(SerializableError::from),
        Err(payload) => Err(SerializableError::from(anyhow!(
            "panic in container child at {}: {}",
            take_panic_location(),
            panic_message(&*payload)
        ))
        .into_panic()),
    }
}

/// The container child exited with a status IT DID NOT CHOOSE.
///
/// ⚠️ THIS TYPE EXISTS BECAUSE THE STATUS USED TO BE THROWN AWAY HERE, and the
/// information was never missing — only discarded. reverie reports the child's
/// real status as a typed `RunError::ExitStatus(ExitStatus)`; `with_container`
/// then wrote `.context("Sandbox container exited unexpectedly")?`, which turns
/// that typed value into an opaque `anyhow::Error` whose exit code survives only
/// inside Display prose. Nothing downstream could branch on it without parsing
/// English, so a tracer panic (container child dies with Rust's 101) and an
/// ordinary CLI error — a bad flag, an unwritable log path — became the same
/// thing one layer up.
///
/// Carrying it as a downcastable type is the whole fix: no new plumbing, no new
/// channel, just stopping the discard. `main` recovers it with
/// `anyhow::Error::downcast_ref`.
#[derive(Debug)]
pub struct ContainerChildExit(pub ExitStatus);

impl std::fmt::Display for ContainerChildExit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Keep the human text the caller already saw. This type changes what is
        // MACHINE-readable, not what an operator reads.
        write!(f, "Sandbox container exited unexpectedly: {:?}", self.0)
    }
}

impl std::error::Error for ContainerChildExit {}

/// Hermit DELIBERATELY refused the run; the child exited a status hermit CHOSE.
///
/// ⚠️ THIS IS THE INVERSE OF [`ContainerChildExit`], AND SEPARATING THEM IS THE
/// WHOLE POINT. That type means "the child died of something no handler caught".
/// This one means "a fail-closed policy stopped the run on purpose". They are
/// the same observation at this boundary — a container child that exited — and
/// they demand opposite responses: file a bug against hermit, versus read the
/// refusal hermit just printed and change the program or the flags.
///
/// Before this, a policy refusal arrived as `ContainerChildExit(Exited(1))` and
/// surfaced as `HERMIT_INTERNAL_FAILURE class=container-child-exit`, exit 125 —
/// a correct refusal reported as an accident, sending the reader to look for a
/// defect in a shutdown path that behaved as designed.
///
/// The status is the only channel across the fork boundary, so
/// `HERMIT_POLICY_REFUSAL_EXIT` is what carries the distinction. It is safe to
/// key on: an ordinary guest exit does NOT arrive here — it is returned as a
/// value through the `Ok(Ok(..))` arm, which is why `hermit run -- sh -c 'exit
/// 122'` still reports 122 with no failure marker.
#[derive(Debug)]
// A unit struct on purpose: the status is `HERMIT_POLICY_REFUSAL_EXIT` by
// construction -- it is what the match arm keys on -- so carrying a copy would
// be a second place for it to be wrong.
pub struct PolicyRefusal;

impl std::fmt::Display for PolicyRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Hermit refused the run: a fail-closed policy stopped it before completion. \
             This is not a hermit failure; the refusal reason is above."
        )
    }
}

impl std::error::Error for PolicyRefusal {}

/// The container child was terminated by a signal, reported as `128 + signo`.
///
/// Reattaches "a `--timeout` deadline fired" on the PARENT side of the
/// container boundary, where `hermit::GuestTimedOut` no longer exists as a type.
///
/// Carries no number, unlike [`SignalDeath`]: the seconds are already in the
/// child's message and re-parsing them out of English is exactly what
/// `FailureKind` exists to avoid.
#[derive(Debug)]
pub struct RunTimeoutMarker;

impl std::fmt::Display for RunTimeoutMarker {
    /// ⚠️ IT DOES NOT SAY "SEE THE REASON ABOVE", AND AN EARLIER VERSION DID.
    /// [`PolicyRefusal`] can say that truthfully because detcore logs the
    /// refusal before exiting, so the reason really is earlier on stderr. Here
    /// the child's message arrives as a CAUSE, which `display_error` prints
    /// BELOW this line -- measured: the chain reads "Error: ... above" followed
    /// by "> Guest exceeded the --timeout bound of 3 seconds". Pointing the
    /// reader the wrong way is worse than not pointing at all.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the --timeout bound fired")
    }
}

impl std::error::Error for RunTimeoutMarker {}

/// The container child exited [`hermit::HERMIT_LOG_CAP_EXIT`]: hermit's own
/// log output crossed `--max-log-bytes` and the writer that crossed it ended
/// the run. Before exiting, the child attempts one message with the bound and
/// byte count, and only if the writes admitted before the crossing finish
/// within the drain bound; it writes without waiting for its sink, so even an
/// attempted message is best effort and can be missing (see `CappedWriter` in
/// `tracing.rs`). This carries no number for the same reason
/// [`RunTimeoutMarker`] does not.
#[derive(Debug)]
pub struct LogCapExceeded;

impl std::fmt::Display for LogCapExceeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "hermit's log output exceeded --max-log-bytes and the run was aborted; \
             lower the log verbosity or raise --max-log-bytes"
        )
    }
}

impl std::error::Error for LogCapExceeded {}

/// Carries the signal, unlike [`PolicyRefusal`], because there is more than one
/// and the number is the whole content of the report.
#[derive(Debug)]
pub struct SignalDeath(pub i32);

impl std::fmt::Display for SignalDeath {
    /// ⚠️ IT DOES NOT SAY WHERE THE SIGNAL CAME FROM, AND AN EARLIER VERSION DID.
    /// It read "the run was stopped from outside", which hermit cannot know:
    /// `handle_signal_event` carries no origin, so a guest that signals ITSELF
    /// produces the identical report. Measured --
    /// `hermit run --sigint-instakill -- /bin/sh -c 'kill -INT $$'` exits 130 and
    /// printed that the run was stopped externally, when nothing external
    /// happened. agent(hermit-005)'s codex lane found it.
    ///
    /// That is the same defect this whole change exists to remove: a report
    /// asserting something the system has no channel to observe. What hermit
    /// DOES know is that the container died of a signal rather than of a hermit
    /// decision, and that is now all it claims.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Hermit's container was terminated by signal {}. This is not a hermit \
             failure and not a refusal: hermit did not choose to stop this run.",
            self.0
        )
    }
}

impl std::error::Error for SignalDeath {}

/// The container child PANICKED, and the panic was caught and reported.
///
/// Distinct from [`ContainerChildExit`], which is the child dying of something
/// no handler caught. Both are "the tracer broke"; neither is an ordinary CLI
/// error, and before this they were all three the same thing.
#[derive(Debug)]
pub struct ContainerChildPanic(pub Error);

impl std::fmt::Display for ContainerChildPanic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for ContainerChildPanic {}

/// Preserve the primary class from a settled container result. The owned CLI
/// adapter also calls this for its strict error-only failed-child channel.
pub fn classify_container_result<T>(
    ran: Result<Result<T, SerializableError>, RunError>,
) -> Result<T, Error> {
    match ran {
        // The child ran and REPORTED something. Whether that was a caught panic
        // or an error it chose to return is exactly what `kind` preserves; both
        // used to arrive as indistinguishable prose.
        Ok(Ok(value)) => Ok(value),
        Ok(Err(reported)) => {
            let kind = reported.kind();
            let error = Error::from(reported);
            Err(match kind {
                FailureKind::Panic => Error::new(ContainerChildPanic(error)),
                // ⚠️ A REFUSAL REPORTED THROUGH THE ERROR CHANNEL IS STILL A
                // REFUSAL. `hermit record` reaches the same fail-closed policy
                // as `hermit run` but is configured to return a typed error
                // instead of shutting the container down, so it never produces
                // the exit status the run path keys on. Without this arm the two
                // spellings of one policy reported 122 and 125 respectively --
                // "hermit refused" and "hermit broke" -- for the same decision.
                //
                // ⚠️ THE CAUSE IS KEPT AS CONTEXT, NOT REPLACED. `PolicyRefusal`
                // says "the refusal reason is above", which is true on the run
                // path because detcore logs it before exiting. Here the reason
                // -- WHICH syscall -- exists only inside this error, so
                // `Error::new(PolicyRefusal)` would discard the one diagnostic
                // the operator needs while claiming it was printed. Attaching it
                // as context keeps the downcast working and the chain intact.
                FailureKind::PolicyRefusal => error.context(PolicyRefusal),
                FailureKind::SkidOvershoot { count } => error
                    .context(SkidOvershootError::new(count))
                    .context(PolicyRefusal),
                // ⚠️ THE LIMIT IS RE-DERIVED, NOT RE-PARSED. `kind` says a
                // deadline fired; the seconds live only in the message the
                // child wrote, and `RunTimeoutMarker` deliberately carries no
                // number rather than scraping one back out of English. The
                // message itself is still printed as the error chain, so the
                // operator sees the bound; only the machine-readable class is
                // reconstructed here.
                FailureKind::RunTimeout => error.context(RunTimeoutMarker),
                // The marker goes UNDER the child's message, as it was in the
                // child: the first `Error:` line keeps naming the failure.
                FailureKind::BackendRunFailure => {
                    Error::new(hermit::BackendRunFailure).context(format!("{error:#}"))
                }
                FailureKind::Error => error,
            })
        }
        // PRESERVED, not flattened: the child died with a status it did not pick.
        // A DELIBERATE refusal is separated from an unchosen death BEFORE the
        // catch-all below, because at this boundary they look identical: both
        // are a container child that exited. Only the status distinguishes
        // them, and only because `unrecoverable_shutdown` chose one that says
        // so.
        Err(RunError::ExitStatus(status))
            if status.code() == Some(detcore_model::HERMIT_POLICY_REFUSAL_EXIT) =>
        {
            Err(Error::new(PolicyRefusal))
        }
        // A DEADLINE HERMIT ITSELF ENFORCED, and it needs its own arm for exactly
        // the reason the refusal above does: the init chose this status on
        // purpose, and without an arm it falls through to `ContainerChildExit`
        // and is reported as "the child died with a status it did not pick" --
        // exit 125, `class=container-child-exit`, i.e. "hermit broke" for a
        // bound working as designed. Measured before this arm existed: the
        // `run --timeout` fallback exited the init 124 and the run reported 125.
        Err(RunError::ExitStatus(status)) if status.code() == Some(HERMIT_DEADLINE_EXIT) => {
            Err(Error::new(RunTimeoutMarker))
        }
        // The --max-log-bytes cap, chosen by `CappedWriter` in the init. Same
        // reason as the deadline arm: without it a working bound reports 125.
        // From here this process must exit 123 within a fixed bound even if a
        // later diagnostic blocks on a stderr nobody reads.
        Err(RunError::ExitStatus(status)) if status.code() == Some(HERMIT_LOG_CAP_EXIT) => {
            super::tracing::bound_log_cap_exit();
            Err(Error::new(LogCapExceeded))
        }
        // A signal death, before the catch-all for the same reason the refusal arm
        // is: falling through would report a signal-terminated run as an
        // internal failure. `sigint_instakill` and `on_container_init_stop_signal`
        // both land here.
        Err(RunError::ExitStatus(status))
            if status
                .code()
                .and_then(detcore_model::signal_from_exit_status)
                .is_some() =>
        {
            let signal = status
                .code()
                .and_then(detcore_model::signal_from_exit_status)
                .expect("guard just matched");
            Err(Error::new(SignalDeath(signal)))
        }
        Err(RunError::ExitStatus(status)) => Err(Error::new(ContainerChildExit(status))),
        // A spawn failure is a genuine CLI-side failure and keeps its prose.
        Err(error @ RunError::Spawn(_)) => {
            Err(Error::new(error).context("Sandbox container failed to spawn"))
        }
    }
}

#[cfg(test)]
#[path = "../../../tests/common/readonly_proc.rs"]
mod readonly_proc;

#[cfg(test)]
mod tests {
    use super::*;

    /// Each installation's mountpoint is the one its target resolved to when
    /// the setup installed it, over the plan's earlier mounts: a bind at
    /// opt/x crosses the symbolic link x -> foo in the first bind at opt, so
    /// it is recorded at foo, and the later bind that replaces opt (whose x
    /// links to bar) changes nothing recorded (the provenance finding of
    /// round 2 of https://github.com/rrnewton/hermit/issues/4213, K3a). Only
    /// a recursive bind records its source, and a remount records nothing.
    #[test]
    fn installations_record_their_install_time_mountpoints() {
        let scratch = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(scratch.path()).unwrap();
        for dir in ["a", "c", "item", "foo", "bar", "opt"] {
            fs::create_dir(root.join(dir)).unwrap();
        }
        std::os::unix::fs::symlink(root.join("foo"), root.join("a/x")).unwrap();
        std::os::unix::fs::symlink(root.join("bar"), root.join("c/x")).unwrap();
        let plan = [
            PlannedMount::hermit(container_proc_mount()),
            PlannedMount::hermit(Mount::bind(root.join("a"), root.join("opt")).recursive()),
            PlannedMount::hermit(Mount::bind(root.join("item"), root.join("opt/x"))),
            PlannedMount::hermit(Mount::bind(root.join("c"), root.join("opt"))),
            PlannedMount::backend(
                Mount::new(root.join("opt"))
                    .flags(MountFlags::MS_BIND | MountFlags::MS_REMOUNT | MountFlags::MS_RDONLY),
            ),
        ];
        let installations = installations_of(&plan);
        let escaped = |path: &Path| vec![mountinfo_escape_path(path)];
        assert_eq!(installations[0].mountpoints, escaped(Path::new("/proc")));
        assert_eq!(installations[1].mountpoints, escaped(&root.join("opt")));
        assert_eq!(installations[1].sources, escaped(&root.join("a")));
        assert_eq!(installations[2].mountpoints, escaped(&root.join("foo")));
        assert!(
            installations[2].sources.is_empty(),
            "a plain bind copies nothing"
        );
        assert_eq!(installations[3].mountpoints, escaped(&root.join("opt")));
        assert!(
            installations[4].mountpoints.is_empty(),
            "a remount makes no row"
        );
        assert!(installations[4].backend);
        // After the replacement, the same target would resolve to bar.
        let mut tree = InstallTimeTree::default();
        for planned in &plan[..4] {
            tree.install(&planned.mount);
        }
        assert_eq!(tree.resolve(&root.join("opt/x")), root.join("bar"));
    }

    /// How long a signalled test child may take to exit before it counts as
    /// having ignored the signal. A child that honours it exits within
    /// microseconds; this only bounds the failing case.
    const STOP_SIGNAL_BUDGET: std::time::Duration = std::time::Duration::from_secs(5);

    /// Run `body` in a forked process, so that the process-wide signal
    /// dispositions and namespaces it changes cannot reach the test harness or
    /// any other test running beside it.
    fn in_isolated_process<T>(body: impl FnMut() -> Result<T, String>) -> T
    where
        T: serde::Serialize + serde::de::DeserializeOwned,
    {
        Container::new()
            .run(body)
            .expect("the isolated test process should run to completion")
            .unwrap_or_else(|error| panic!("{error}"))
    }

    fn set_default_stop_dispositions() -> Result<(), String> {
        let default = SigAction::new(SigHandler::SigDfl, SaFlags::empty(), SigSet::empty());
        for signal in CONTAINER_INIT_STOP_SIGNALS {
            // SAFETY: the default action installs no handler.
            unsafe { sigaction(signal, &default) }.map_err(|error| format!("{signal}: {error}"))?;
        }
        Ok(())
    }

    /// Enter a new user and PID namespace, so that the next child this process
    /// forks is PID 1 of that namespace: a namespace init, like the container
    /// init, for which the kernel discards `SIG_DFL` signals from outside.
    fn enter_new_pid_namespace() -> Result<(), String> {
        // SAFETY: `unshare` touches no caller memory. This process is a fresh
        // fork and has one thread, as a new user namespace requires.
        if unsafe { libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWPID) } == -1 {
            return Err(format!("unshare: {}", io::Error::last_os_error()));
        }
        Ok(())
    }

    /// Fork a child that runs `prepare`, reports through a pipe that it has
    /// done so, and then waits for signals forever without installing any
    /// handler of its own. Returns the child's pid once it has reported.
    fn fork_waiting_child(prepare: fn() -> bool) -> Result<libc::pid_t, String> {
        let mut fds = [0; 2];
        // SAFETY: `fds` is a correctly sized out-parameter.
        if unsafe { libc::pipe(fds.as_mut_ptr()) } == -1 {
            return Err(format!("pipe: {}", io::Error::last_os_error()));
        }
        // SAFETY: the parent is single-threaded, and the child calls only
        // `prepare`, then async-signal-safe functions, and never returns.
        match unsafe { libc::fork() } {
            -1 => Err(format!("fork: {}", io::Error::last_os_error())),
            0 => unsafe {
                libc::close(fds[0]);
                if !prepare() {
                    libc::_exit(99);
                }
                libc::write(fds[1], [1u8].as_ptr().cast(), 1);
                libc::close(fds[1]);
                loop {
                    libc::pause();
                }
            },
            child => {
                let mut ready = 0u8;
                // SAFETY: both descriptors came from the pipe above, and
                // `ready` is a one-byte buffer that outlives the read.
                let read = unsafe {
                    libc::close(fds[1]);
                    let read = libc::read(fds[0], (&mut ready as *mut u8).cast(), 1);
                    libc::close(fds[0]);
                    read
                };
                if read != 1 {
                    return Err(format!(
                        "child {child} exited without reporting that it was ready, so its \
                         preparation failed"
                    ));
                }
                Ok(child)
            }
        }
    }

    /// Send `signal` to `child` and wait for it to exit. Returns its raw wait
    /// status, or `None` if it was still running [`STOP_SIGNAL_BUDGET`] later,
    /// in which case it is killed and reaped.
    fn signal_and_reap(child: libc::pid_t, signal: Signal) -> Result<Option<i32>, String> {
        // SAFETY: `kill` takes a pid and a signal number and touches no caller
        // memory. `child` is our own unreaped child.
        if unsafe { libc::kill(child, signal as libc::c_int) } == -1 {
            return Err(format!(
                "kill({child}, {signal}): {}",
                io::Error::last_os_error()
            ));
        }
        let deadline = std::time::Instant::now() + STOP_SIGNAL_BUDGET;
        loop {
            let mut status = 0;
            // SAFETY: `status` is a valid out-parameter for waitpid(2).
            match unsafe { libc::waitpid(child, &mut status, libc::WNOHANG) } {
                reaped if reaped == child => return Ok(Some(status)),
                -1 => return Err(format!("waitpid({child}): {}", io::Error::last_os_error())),
                _ => {}
            }
            if std::time::Instant::now() >= deadline {
                // SAFETY: as above. SIGKILL reaches a namespace init from its
                // parent namespace, so the blocking wait returns.
                unsafe {
                    libc::kill(child, libc::SIGKILL);
                    libc::waitpid(child, &mut status, 0);
                }
                return Ok(None);
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    fn describe_wait_status(status: Option<i32>) -> String {
        match status {
            None => format!("still running {STOP_SIGNAL_BUDGET:?} after the signal"),
            Some(status) if libc::WIFEXITED(status) => {
                format!("exited {}", libc::WEXITSTATUS(status))
            }
            Some(status) if libc::WIFSIGNALED(status) => {
                format!("killed by signal {}", libc::WTERMSIG(status))
            }
            Some(status) => format!("wait status {status:#x}"),
        }
    }

    /// The window the bridge closes, reproduced without timing: the namespace
    /// init is forked while the bridge is installed and never installs a
    /// handler of its own, so every signal here arrives before the init has
    /// armed anything. Without the bridge the kernel discards all three and the
    /// init is still running when the budget expires.
    #[test]
    fn a_namespace_init_honours_stop_signals_sent_before_it_arms() {
        for signal in CONTAINER_INIT_STOP_SIGNALS {
            let status = in_isolated_process(|| {
                set_default_stop_dispositions()?;
                let _bridge = ContainerInitStopSignalBridge::install()
                    .map_err(|error| format!("{error:#}"))?;
                enter_new_pid_namespace()?;
                let init = fork_waiting_child(|| true)?;
                signal_and_reap(init, signal)
            });
            assert!(
                status.is_some_and(|status| libc::WIFEXITED(status)
                    && libc::WEXITSTATUS(status) == 128 + signal as i32),
                "{signal}: a namespace init that had not armed its own handlers should \
                 exit {}, as `on_container_init_stop_signal` does; it {}",
                128 + signal as i32,
                describe_wait_status(status)
            );
        }
    }

    /// The bridge must not change what a stop signal does to the process that
    /// holds it: the `hermit` process still dies from the signal itself, with
    /// the wait status `SIG_DFL` would have given it.
    #[test]
    fn the_process_holding_the_bridge_still_dies_from_stop_signals() {
        fn hold_the_bridge() -> bool {
            if set_default_stop_dispositions().is_err() {
                return false;
            }
            match ContainerInitStopSignalBridge::install() {
                Ok(bridge) => {
                    std::mem::forget(bridge);
                    true
                }
                Err(_) => false,
            }
        }
        for signal in CONTAINER_INIT_STOP_SIGNALS {
            let status = in_isolated_process(|| {
                let holder = fork_waiting_child(hold_the_bridge)?;
                signal_and_reap(holder, signal)
            });
            assert!(
                status
                    .is_some_and(|status| libc::WIFSIGNALED(status)
                        && libc::WTERMSIG(status) == signal as i32),
                "{signal}: the process holding the bridge should be killed by {signal}; it {}",
                describe_wait_status(status)
            );
        }
    }

    /// Only `SIG_DFL` is bridged, and dropping the guard restores exactly
    /// what was there before.
    #[test]
    fn the_bridge_leaves_ignored_and_handled_signals_alone_and_restores_the_default() {
        extern "C" fn someone_elses_handler(_signal: libc::c_int) {}
        let dispositions = in_isolated_process(|| {
            let read = || -> Result<Vec<libc::sighandler_t>, String> {
                CONTAINER_INIT_STOP_SIGNALS
                    .into_iter()
                    .map(|signal| {
                        current_signal_action(signal)
                            .map(|action| action.sa_sigaction)
                            .map_err(|error| format!("{error:#}"))
                    })
                    .collect()
            };
            let actions = [
                SigHandler::SigDfl,
                SigHandler::SigIgn,
                SigHandler::Handler(someone_elses_handler),
            ];
            for (signal, handler) in CONTAINER_INIT_STOP_SIGNALS.into_iter().zip(actions) {
                let action = SigAction::new(handler, SaFlags::empty(), SigSet::empty());
                // SAFETY: the handler does nothing, and it is never invoked.
                unsafe { sigaction(signal, &action) }
                    .map_err(|error| format!("{signal}: {error}"))?;
            }
            let before = read()?;
            let bridge =
                ContainerInitStopSignalBridge::install().map_err(|error| format!("{error:#}"))?;
            let during = read()?;
            let installed = bridge.handler;
            drop(bridge);
            Ok((before, during, read()?, installed))
        });
        // Compare the values the kernel reports rather than function addresses
        // taken here, which Rust does not promise are unique.
        let (before, during, after, installed) = dispositions;
        let other = before[2];
        assert_eq!(
            before[..2],
            [libc::SIG_DFL, libc::SIG_IGN],
            "SIGTERM starts at SIG_DFL and SIGINT at SIG_IGN"
        );
        assert!(
            ![libc::SIG_DFL, libc::SIG_IGN, installed].contains(&other),
            "SIGHUP starts at a handler that is not the bridge"
        );
        assert_eq!(
            during,
            vec![installed, libc::SIG_IGN, other],
            "only SIGTERM, the signal at SIG_DFL, is bridged while the guard is held"
        );
        assert_eq!(
            after, before,
            "dropping the guard restores every disposition it found"
        );
    }

    /// A `hermit` process that is itself PID 1 cannot be told apart from a
    /// container init with its own PID namespace, so the bridge stays out of
    /// the way rather than guessing which of the two it is running in.
    #[test]
    fn the_bridge_is_not_installed_by_a_process_that_is_pid_1() {
        fn install_as_pid_1() -> bool {
            // SAFETY: getpid(2) has no preconditions.
            if unsafe { libc::getpid() } != 1 || set_default_stop_dispositions().is_err() {
                return false;
            }
            let Ok(bridge) = ContainerInitStopSignalBridge::install() else {
                return false;
            };
            let untouched = CONTAINER_INIT_STOP_SIGNALS.into_iter().all(|signal| {
                current_signal_action(signal)
                    .is_ok_and(|action| action.sa_sigaction == libc::SIG_DFL)
            });
            drop(bridge);
            untouched
        }
        // A child whose check fails exits before reporting, which fails here.
        in_isolated_process(|| {
            enter_new_pid_namespace()?;
            let init = fork_waiting_child(install_as_pid_1)?;
            signal_and_reap(init, Signal::SIGKILL).map(drop)
        });
    }

    fn namespace_proc_contents() -> Result<(String, String), String> {
        Ok((
            fs::read_to_string("/proc/mounts").map_err(|error| error.to_string())?,
            fs::read_to_string("/proc/self/status").map_err(|error| error.to_string())?,
        ))
    }

    #[test]
    fn default_container_uses_readonly_proc_fallback() {
        // The outer child confines the irreversible seccomp filter to this test.
        let (mounts, status) = Container::new()
            .run(|| {
                readonly_proc::deny_writable_mounts().map_err(|error| error.to_string())?;
                default_container(false)
                    .run(namespace_proc_contents)
                    .map_err(|error| format!("{error:?}"))?
            })
            .unwrap()
            .expect("default container should retry its denied proc mount read-only");
        readonly_proc::assert_readonly_proc(&mounts, &status, 1);
    }

    #[test]
    fn image_container_uses_readonly_proc_fallback() {
        let rootfs = tempfile::tempdir().unwrap();
        let tmpfs = tempfile::tempdir().unwrap();
        for directory in ["proc", "tmp", "dev/pts"] {
            fs::create_dir_all(rootfs.path().join(directory)).unwrap();
        }
        for node in crate::image::DEV_BIND_TARGETS {
            File::create(rootfs.path().join("dev").join(node)).unwrap();
        }

        let (mounts, status) = Container::new()
            .run(|| {
                readonly_proc::deny_writable_mounts().map_err(|error| error.to_string())?;
                let (mut container, _identity) =
                    image_container(rootfs.path(), tmpfs.path(), false, true)
                        .map_err(|error| error.to_string())?;
                // This closure is already loaded, so the image needs no executable
                // or libraries. It reads the proc mount after entering the chroot.
                container
                    .run(namespace_proc_contents)
                    .map_err(|error| format!("{error:?}"))?
            })
            .unwrap()
            .expect("image container should retry its denied proc mount read-only");
        readonly_proc::assert_readonly_proc(&mounts, &status, 1);
    }

    /// The image cache can live on a filesystem mounted `nosuid,nodev` outside
    /// Hermit's user namespace (a hardened `/tmp`, a CI sandbox's scratch
    /// tmpfs). The kernel locks those flags on the mount copied into Hermit's
    /// namespace, so the read-only remount of the image root must restate them.
    /// A remount that asks only for `MS_RDONLY` fails with EPERM and the image
    /// container never starts. This builds that filesystem in an outer user
    /// namespace, so the result does not depend on how the host mounted its
    /// temporary directory, and checks that the image root comes up read-only
    /// with both locked flags still present.
    #[test]
    fn image_container_accepts_a_rootfs_on_a_nosuid_nodev_filesystem() {
        fn root_mount_flags() -> Result<(bool, bool, bool), String> {
            let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
            // SAFETY: the path is a valid NUL-terminated string and `stat` is a
            // correctly sized out-parameter that is read only on success.
            let rc = unsafe { libc::statvfs(c"/".as_ptr(), stat.as_mut_ptr()) };
            if rc != 0 {
                return Err(format!("statvfs /: {}", io::Error::last_os_error()));
            }
            // SAFETY: statvfs returned 0, so it initialized `stat`.
            let flags = unsafe { stat.assume_init() }.f_flag;
            Ok((
                flags & libc::ST_RDONLY != 0,
                flags & libc::ST_NOSUID != 0,
                flags & libc::ST_NODEV != 0,
            ))
        }

        let base = tempfile::tempdir().unwrap();
        let base_path = base.path().to_owned();
        let flags = Container::new()
            .map_root()
            .mount(Mount::tmpfs(&base_path).flags(MountFlags::MS_NOSUID | MountFlags::MS_NODEV))
            .run(move || -> Result<(bool, bool, bool), String> {
                let rootfs = base_path.join("rootfs");
                let tmpfs = base_path.join("scratch");
                for directory in ["proc", "tmp", "dev/pts"] {
                    fs::create_dir_all(rootfs.join(directory)).map_err(|e| e.to_string())?;
                }
                for node in crate::image::DEV_BIND_TARGETS {
                    File::create(rootfs.join("dev").join(node)).map_err(|e| e.to_string())?;
                }
                fs::create_dir(&tmpfs).map_err(|e| e.to_string())?;
                let (mut container, _identity) = image_container(&rootfs, &tmpfs, false, true)
                    .map_err(|error| error.to_string())?;
                container
                    .run(root_mount_flags)
                    .map_err(|error| format!("{error:?}"))?
            })
            .expect("run the outer namespace holding the nosuid,nodev filesystem")
            .expect("an image root on a nosuid,nodev filesystem must mount read-only");
        assert_eq!(
            flags,
            (true, true, true),
            "image root (read-only, nosuid, nodev) flags inside the container"
        );
    }

    /// ⚠️ RECORD MODE REACHES THE SAME POLICY BY A DIFFERENT CHANNEL, and for
    /// one release the two channels disagreed about what happened.
    ///
    /// `hermit run` sets `shutdown_on_unsupported_syscall`, so a refusal calls
    /// `unrecoverable_shutdown` and the STATUS carries the meaning (122).
    /// `hermit record` sets `exit_on_unsupported_syscall` with
    /// `shutdown_on_unsupported_syscall: false` (metadata.rs), so it returns a
    /// typed `UnsupportedSyscallError` through Reverie and produces no status of
    /// its own -- arriving as an ordinary reported error, i.e. 125 and
    /// `class=container-child-exit`, "hermit broke", for a decision hermit made
    /// on purpose.
    ///
    /// Set the kind back to `FailureKind::Error` and this fails.
    #[test]
    fn a_refusal_reported_through_the_error_channel_is_still_a_refusal() {
        let refusal = SerializableError::from(anyhow::Error::new(
            detcore::UnsupportedSyscallError(reverie::syscalls::Sysno::kexec_load),
        ));
        assert_eq!(
            refusal.kind(),
            FailureKind::PolicyRefusal,
            "an UnsupportedSyscallError must be classified at the boundary, which is the \
             last place its TYPE still exists -- past it everything is strings"
        );

        let classified = classify_container_result::<()>(Ok(Err(refusal)))
            .expect_err("a refusal is not a success");
        assert!(
            classified.downcast_ref::<PolicyRefusal>().is_some(),
            "record mode must report the same class as run mode for the same policy"
        );
        assert!(
            classified.downcast_ref::<ContainerChildPanic>().is_none(),
            "a refusal is not a panic"
        );
        // ⚠️ THE CAUSE MUST SURVIVE. `PolicyRefusal` says the reason is above; on
        // this path the reason exists ONLY in this chain, so replacing the error
        // rather than contextualising it would delete the only diagnostic naming
        // WHICH syscall while still claiming it was printed.
        assert!(
            format!("{classified:#}").contains("kexec_load"),
            "the refused syscall must survive into the reported chain: {classified:#}"
        );

        // ⚠️ CONTROL: an ordinary reported error must NOT become a refusal, or
        // this arm would relabel every child-reported failure as deliberate.
        let ordinary = SerializableError::from(anyhow::anyhow!("something broke"));
        assert_eq!(ordinary.kind(), FailureKind::Error);
        let classified = classify_container_result::<()>(Ok(Err(ordinary)))
            .expect_err("an error is not a success");
        assert!(classified.downcast_ref::<PolicyRefusal>().is_none());
    }

    #[test]
    fn skid_overshoot_count_survives_the_container_boundary() {
        let reported = SerializableError::from(Error::new(SkidOvershootError::new(3)));
        assert_eq!(reported.kind(), FailureKind::SkidOvershoot { count: 3 });

        let classified = classify_container_result::<()>(Ok(Err(reported)))
            .expect_err("a skid overshoot is not a success");
        assert!(classified.downcast_ref::<PolicyRefusal>().is_some());
        assert_eq!(
            classified
                .downcast_ref::<SkidOvershootError>()
                .expect("the parent must recover the typed cause")
                .count(),
            3
        );
    }

    /// ⚠️ THE RECOGNITION HALF, WHICH THE END-TO-END TEST DOES NOT WITNESS.
    /// `a_guest_exiting_a_reserved_status_is_not_reported_as_hermit` in
    /// `tests/cli.rs` runs real guests, so every status it sees arrives through
    /// the `Ok(Ok(..))` arm and it never reaches the classifier at all --
    /// agent(hermit-007)'s codex lane proved that by mutation: it still passed
    /// with the signal classifier deleted outright. That test establishes a
    /// guest's own 122/130 is still reported as the guest, which is real and is
    /// the gap hermit#2659 left, but it cannot witness this.
    ///
    /// Feeding the classifier a synthesized `Exited(130)` directly is what
    /// witnesses it, and needs no signal delivery -- which matters because the
    /// end-to-end SIGINT path may not be observable from outside at all: the
    /// outer process can die of the same signal before the child's status is
    /// ever classified.
    ///
    /// Delete the signal arm in `classify_container_result` and this fails.
    #[test]
    fn a_container_child_exiting_in_the_signal_band_is_not_an_internal_failure() {
        use reverie::process::ExitStatus;

        let signal_death = classify_container_result::<()>(Err(RunError::ExitStatus(
            ExitStatus::Exited(detcore_model::HERMIT_SIGINT_DEATH_EXIT),
        )))
        .expect_err("a signal death is not a success");
        assert!(
            signal_death.downcast_ref::<SignalDeath>().is_some(),
            "Exited(130) must classify as a signal death, not as an unchosen child \
             exit -- LiteInst and the exit-code path both key on this distinction"
        );
        assert!(
            signal_death.downcast_ref::<ContainerChildExit>().is_none(),
            "a signal death must not also read as an unaccounted container exit"
        );

        // The refusal arm must still win for 122: the two arms are adjacent and
        // ordering decides meaning.
        let refusal = classify_container_result::<()>(Err(RunError::ExitStatus(
            ExitStatus::Exited(detcore_model::HERMIT_POLICY_REFUSAL_EXIT),
        )))
        .expect_err("a refusal is not a success");
        assert!(refusal.downcast_ref::<PolicyRefusal>().is_some());
        assert!(
            refusal.downcast_ref::<SignalDeath>().is_none(),
            "122 must not parse as a signal death; the const-assert pins them disjoint"
        );

        // ⚠️ CONTROL: a status in neither reserved set must STILL be an unchosen
        // child exit. Without this, a classifier that returned SignalDeath for
        // everything would pass the rows above.
        let unchosen =
            classify_container_result::<()>(Err(RunError::ExitStatus(ExitStatus::Exited(7))))
                .expect_err("an unaccounted exit is not a success");
        assert!(unchosen.downcast_ref::<ContainerChildExit>().is_some());
        assert!(unchosen.downcast_ref::<SignalDeath>().is_none());
    }

    #[test]
    fn child_panic_becomes_an_error_naming_message_and_location() {
        let mut f = || -> Result<(), Error> { panic!("divergence detail that must survive") };
        let reported =
            catch_child_panic(&mut f).expect_err("a panic must not be reported as success");
        // The discriminant is the point: prose alone could not say "panic".
        assert_eq!(
            reported.kind(),
            FailureKind::Panic,
            "a caught panic must be tagged as one at the catch site"
        );
        let error = Error::from(reported);
        let rendered = format!("{error:#}");
        assert!(
            rendered.contains("divergence detail that must survive"),
            "panic message was lost: {rendered}"
        );
        assert!(
            rendered.contains("container.rs:"),
            "panic location was lost, which is the half catch_unwind does not give us: {rendered}"
        );
    }

    #[test]
    fn ordinary_results_pass_through_unchanged() {
        let mut ok = || -> Result<u32, Error> { Ok(7) };
        assert_eq!(catch_child_panic(&mut ok).unwrap(), 7);
        let mut err = || -> Result<u32, Error> { Err(anyhow!("a plain error, not a panic")) };
        let reported = catch_child_panic(&mut err).unwrap_err();
        assert_eq!(
            reported.kind(),
            FailureKind::Error,
            "an ordinary reported error must NOT be tagged as a panic"
        );
        let rendered = format!("{:#}", Error::from(reported));
        assert!(
            rendered.contains("a plain error, not a panic"),
            "{rendered}"
        );
        assert!(
            !rendered.contains("panic in container child"),
            "an ordinary error must not be relabelled as a panic: {rendered}"
        );
    }

    // The frozen group database must always resolve the overflow GID so that
    // guest group-name lookups (e.g. `groups`) do not depend on nondeterministic
    // host NSS. This is what keeps record/replay identity resolution matching
    // `run` mode regardless of whether the host `/etc/group` lists 65534.
    #[test]
    fn identity_hardening_freezes_group_with_overflow_entry() {
        let (mounts, guard) =
            identity_hardening_mounts().expect("identity hardening mounts should be constructible");
        assert!(
            !mounts.is_empty(),
            "expected at least the frozen /etc/group mount"
        );
        let group_file = guard
            ._group_file
            .as_ref()
            .expect("identity hardening must produce a frozen group file");
        let contents = fs::read_to_string(group_file.path())
            .expect("frozen group database should be readable");
        let overflow_gid = overflow_gid();
        assert_eq!(overflow_gid, "65534");
        assert!(
            contents
                .lines()
                .any(|line| line.split(':').nth(2) == Some(overflow_gid.as_str())),
            "frozen group database must resolve overflow gid {overflow_gid}:\n{contents}"
        );
    }

    #[test]
    fn mountinfo_provenance_uses_object_identity_not_temp_name_or_target() {
        let actual = tempfile::NamedTempFile::new().expect("create actual temporary source");
        let target = actual.path().to_path_buf();
        let matching =
            MountInfoRootSource::new(actual.path(), &target, b"/tmpvol/.hermit/matching").unwrap();
        let guard = IdentityGuard {
            _group_file: Some(actual),
            _nscd_dir: None,
            mountinfo_roots: vec![matching],
            user_mount_targets: Vec::new(),
            user_binds: Vec::new(),
            host_rows: Vec::new(),
            plan: MountPlan::Unrecorded,
        };

        let rewrites = guard.mountinfo_root_rewrites().unwrap();
        assert_eq!(rewrites.len(), 1);
        assert_eq!(rewrites[0].deterministic_root, b"/tmpvol/.hermit/matching");
        assert_eq!(rewrites[0].raw_root_prefix, None);
        assert_eq!(rewrites[0].raw_mountpoint_prefix, None);
        assert_ne!(rewrites[0].raw_mount_id, 0);
    }

    #[test]
    fn private_tmp_provenance_marks_descendant_roots_for_rewriting() {
        let private_tmp = tempfile::TempDir::new().expect("create private tmp");
        let provenance = MountInfoRootSource::private_tmp(private_tmp.path()).unwrap();
        assert!(provenance.rewrite_descendant_roots);
        assert_eq!(
            provenance.raw_mountpoint_prefix.as_deref(),
            Some(mountinfo_escape_path(private_tmp.path()).as_slice())
        );
        assert_eq!(
            provenance.deterministic_mountpoint_prefix.as_deref(),
            Some(b"/tmp".as_slice())
        );
    }

    #[test]
    fn image_container_registers_only_an_automatic_private_tmp() {
        let rootfs = tempfile::TempDir::new().expect("create image rootfs");
        let private_tmp = tempfile::TempDir::new().expect("create private tmp");
        let (_, generated) = image_container(rootfs.path(), private_tmp.path(), false, true)
            .expect("construct image container with generated tmp");
        assert_eq!(generated.mountinfo_roots.len(), 1);

        let (_, supplied) = image_container(rootfs.path(), private_tmp.path(), false, false)
            .expect("construct image container with user tmp");
        assert!(supplied.mountinfo_roots.is_empty());
    }

    #[test]
    fn fdinfo_mount_id_parser_refuses_malformed_duplicates_and_missing_fields() {
        assert_eq!(
            detcore_model::procfs::parse_fdinfo_mount_id(b"pos:\t0\nmnt_id:\t37\n").unwrap(),
            37
        );
        for malformed in [
            "pos:\t0\n",
            "mnt_id:\tbad\nmnt_id:\t37\n",
            "mnt_id:\t37\nmnt_id:\t38\n",
            "mnt_id:\t37 trailing\n",
            "mnt_id:\t18446744073709551616\n",
        ] {
            assert!(
                detcore_model::procfs::parse_fdinfo_mount_id(malformed.as_bytes()).is_none(),
                "accepted malformed fdinfo: {malformed:?}"
            );
        }
    }

    #[test]
    fn mountinfo_provenance_refuses_a_same_target_lookalike() {
        let actual = tempfile::NamedTempFile::new().expect("create actual temporary source");
        let lookalike = tempfile::NamedTempFile::new().expect("create lookalike temporary source");
        let target = actual.path().to_path_buf();
        let same_target_wrong_source =
            MountInfoRootSource::new(lookalike.path(), &target, b"/tmpvol/.hermit/lookalike")
                .unwrap();
        let guard = IdentityGuard {
            _group_file: Some(actual),
            _nscd_dir: None,
            mountinfo_roots: vec![same_target_wrong_source],
            user_mount_targets: Vec::new(),
            user_binds: Vec::new(),
            host_rows: Vec::new(),
            plan: MountPlan::Unrecorded,
        };

        let error = guard.mountinfo_root_rewrites().unwrap_err().to_string();
        assert!(error.contains("does not match the pinned Hermit-owned source"));
    }

    #[test]
    fn explicit_tmp_mount_discards_private_tmp_provenance_before_resolution() {
        let private_tmp = tempfile::TempDir::new().expect("create private tmp");
        let mut guard = IdentityGuard::empty();
        guard.add_private_tmp(private_tmp.path()).unwrap();
        assert_eq!(guard.mountinfo_roots.len(), 1);

        let mut mounts = Vec::new();
        guard.discard_mounts_shadowed_by(&mut mounts, Path::new("/tmp"));
        assert!(guard.mountinfo_roots.is_empty());
    }

    #[test]
    fn ordered_user_mounts_do_not_reuse_stale_host_resolution() {
        let var_run_is_run = fs::canonicalize("/var/run").ok() == fs::canonicalize("/run").ok();
        if !var_run_is_run {
            return;
        }

        let mut guard = IdentityGuard::empty();
        guard.mountinfo_roots.push(MountInfoRootSource {
            source: None,
            target: PathBuf::from("/run/nscd"),
            deterministic_root: Some(DETERMINISTIC_NSCD_ROOT.to_vec()),
            rewrite_descendant_roots: false,
            raw_mountpoint_prefix: None,
            deterministic_mountpoint_prefix: None,
        });
        let mut mounts = vec![Mount::bind("/tmp", "/run/nscd")];

        guard.discard_mounts_shadowed_by(&mut mounts, Path::new("/var"));
        guard.discard_mounts_shadowed_by(&mut mounts, Path::new("/var/run/nscd"));

        assert_eq!(mounts.len(), 1);
        assert_eq!(guard.mountinfo_roots.len(), 1);

        for direct_target in ["/run", "/var/run/nscd"] {
            let mut guard = IdentityGuard::empty();
            guard.mountinfo_roots.push(MountInfoRootSource {
                source: None,
                target: PathBuf::from("/run/nscd"),
                deterministic_root: Some(DETERMINISTIC_NSCD_ROOT.to_vec()),
                rewrite_descendant_roots: false,
                raw_mountpoint_prefix: None,
                deterministic_mountpoint_prefix: None,
            });
            let mut mounts = vec![Mount::bind("/tmp", "/run/nscd")];
            guard.discard_mounts_shadowed_by(&mut mounts, Path::new(direct_target));
            assert!(mounts.is_empty(), "{direct_target} must shadow /run/nscd");
            assert!(guard.mountinfo_roots.is_empty());
        }
    }

    #[test]
    fn shadow_detection_follows_the_host_var_run_target() {
        let var_run_is_run = fs::canonicalize("/var/run").ok() == fs::canonicalize("/run").ok();
        assert_eq!(
            mount_target_is_shadowed(Path::new("/var/run/nscd"), Path::new("/var")),
            !var_run_is_run
        );
        assert_eq!(
            mount_target_is_shadowed(Path::new("/var/run/nscd"), Path::new("/run")),
            var_run_is_run
        );
        assert!(!mount_target_is_shadowed(
            Path::new("/var/run/nscd"),
            Path::new("/var/lib")
        ));
    }

    #[test]
    fn affinity_core_is_an_actual_member_of_a_sparse_allowed_mask() {
        let mut affinity = CpuSet::new();
        for cpu in [7, 31, 211] {
            affinity.set(cpu).expect("valid sparse CPU id");
        }
        let allowed = cpu_ids(&affinity);
        assert_eq!(allowed, [7, 31, 211]);
        for _ in 0..128 {
            let selected = choose_affinity_core(&allowed).expect("non-empty mask");
            assert!(allowed.contains(&selected), "selected CPU {selected}");
        }
        assert_eq!(choose_affinity_core(&[]), None);
    }

    /// How the candidate init in [`arm_after_the_parent_crossed`] ended: its
    /// raw wait status, and whether it wrote the byte a guest start would
    /// follow.
    type ArmOutcome = (i32, bool);

    /// The fork-to-`prctl` window, made deterministic. H (this isolated
    /// process) is a child subreaper and forks P, which holds the invocation's
    /// capped budget; P forks C, the candidate init, then crosses the cap
    /// through the production `CappedWriter` path, which exits 123. C waits
    /// until its parent is no longer P -- that is, until P has run `exit_notify`
    /// and C was re-parented to H -- so `arm` deterministically runs after the
    /// parent that crossed is gone. (EOF on a pipe would not do: P's descriptors
    /// close before `exit_notify`, so C could arm in time and be killed by the
    /// death signal with or without the guard, and the test would pass either
    /// way.) If `arm` returns, C writes the marker byte and exits 0.
    fn arm_after_the_parent_crossed(arm: fn() -> bool) -> ArmOutcome {
        use std::io::Read as _;

        in_isolated_process(move || {
            let budget = crate::tracing::LogBudget::new(100).map_err(|e| e.to_string())?;
            // Only in this isolated process; the harness registers nothing.
            crate::tracing::register_invocation_log_budget(Some(budget.clone()));
            // SAFETY: `prctl` touches no caller memory; this process is a fresh
            // fork, so the attribute reaches no other test.
            if unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1) } == -1 {
                return Err(format!("subreaper: {}", io::Error::last_os_error()));
            }
            let mut fds = [0; 2];
            // SAFETY: `fds` is a correctly sized out-parameter.
            if unsafe { libc::pipe(fds.as_mut_ptr()) } == -1 {
                return Err(format!("pipe: {}", io::Error::last_os_error()));
            }
            let (read_fd, marker_fd) = (fds[0], fds[1]);
            // SAFETY: this process has one thread. P and C call only the code
            // under test, write(2), usleep and _exit, and never return.
            let p = unsafe { libc::fork() };
            if p == -1 {
                return Err(format!("fork P: {}", io::Error::last_os_error()));
            }
            if p == 0 {
                unsafe {
                    libc::close(read_fd);
                    let parent = libc::getpid();
                    let c = libc::fork();
                    if c == -1 {
                        libc::_exit(70);
                    }
                    if c == 0 {
                        // At most 10 s, so a regression cannot hang the suite.
                        let mut waits = 0;
                        while libc::getppid() == parent {
                            waits += 1;
                            if waits > 10_000 {
                                libc::_exit(71);
                            }
                            libc::usleep(1000);
                        }
                        if !arm() {
                            libc::_exit(72);
                        }
                        libc::write(marker_fd, b"G".as_ptr().cast(), 1);
                        libc::_exit(0);
                    }
                    libc::close(marker_fd);
                    let null = libc::open(c"/dev/null".as_ptr(), libc::O_WRONLY);
                    let mut writer = crate::tracing::CappedWriter::file(
                        std::fs::File::from_raw_fd(null),
                        0,
                        Some(budget.clone()),
                    );
                    let _ = writer.write_all(&[b'a'; 60]);
                    let _ = writer.write_all(&[b'b'; 60]);
                    // Not reached: the second write crosses and exits 123.
                    libc::_exit(73);
                }
            }
            // SAFETY: `marker_fd` came from the pipe above and is not used
            // again in this process.
            unsafe { libc::close(marker_fd) };
            let mut status = 0;
            // SAFETY: P is our own child, and `status` outlives the call.
            if unsafe { libc::waitpid(p, &mut status, 0) } != p {
                return Err(format!("waitpid P: {}", io::Error::last_os_error()));
            }
            if !libc::WIFEXITED(status) || libc::WEXITSTATUS(status) != hermit::HERMIT_LOG_CAP_EXIT
            {
                return Err(format!(
                    "P did not cross the cap and exit 123: status {status:#x}"
                ));
            }
            // C was re-parented to this subreaper when P ran `exit_notify`,
            // which happened before P could be reaped.
            let mut c_status = 0;
            // SAFETY: as above; C is now our only child.
            if unsafe { libc::waitpid(-1, &mut c_status, 0) } <= 0 {
                return Err(format!("waitpid C: {}", io::Error::last_os_error()));
            }
            let mut marker = Vec::new();
            // SAFETY: `read_fd` came from the pipe above; every write end is
            // closed now that P and C have exited.
            unsafe { std::fs::File::from_raw_fd(read_fd) }
                .read_to_end(&mut marker)
                .map_err(|e| e.to_string())?;
            Ok((c_status, !marker.is_empty()))
        })
    }

    fn assert_stopped_by_the_crossing(name: &str, (status, marker): ArmOutcome) {
        assert!(
            libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == hermit::HERMIT_LOG_CAP_EXIT,
            "{name}: armed after its parent crossed --max-log-bytes and exited 123, so its death \
             signal can never arrive; it must exit 123 itself, but its wait status was \
             {status:#x} (exit code {}){}",
            libc::WEXITSTATUS(status),
            if marker {
                ", and it went on to the guest start"
            } else {
                ""
            },
        );
        assert!(
            !marker,
            "{name}: it exited 123 but wrote the guest-start marker first"
        );
    }

    /// A container init whose parent crossed the cap before the init armed its
    /// death signal must not go on to start a guest.
    #[test]
    fn log_cap_crossed_before_the_container_init_arms_stops_the_init() {
        let outcome = arm_after_the_parent_crossed(|| arm_container_init_guards().is_ok());
        assert_stopped_by_the_crossing("arm_container_init_guards", outcome);
    }

    /// The same window on the `run --namespace-only` launch path, whose child
    /// arms in `pre_exec` and then execs the guest itself.
    #[test]
    fn log_cap_crossed_before_the_namespace_only_guest_arms_stops_the_exec() {
        let outcome = arm_after_the_parent_crossed(|| arm_namespace_only_guest().is_ok());
        assert_stopped_by_the_crossing("arm_namespace_only_guest", outcome);
    }

    /// Hermit refuses a `--namespace-only` guest's `kill(0, sig)` only while
    /// the caller is in Hermit's process group, or its group is unknown; a
    /// signal 0, any other pid, and a caller that leads its own group pass
    /// (https://github.com/rrnewton/hermit/issues/4065). The i386 `kill`
    /// counts too.
    #[test]
    fn only_a_group_kill_from_hermits_group_is_refused() {
        let args = |pid: i64, sig: i64| [pid as u64, sig as u64, 0, 0, 0, 0];
        let hermit = 40;
        assert!(refuses_group_kill(
            args(0, libc::SIGUSR1 as i64),
            Some(hermit),
            hermit
        ));
        assert!(refuses_group_kill(
            args(0, libc::SIGKILL as i64),
            None,
            hermit
        ));
        assert!(!refuses_group_kill(args(0, 0), Some(hermit), hermit));
        // An invalid signal number is the kernel's EINVAL, not a refusal.
        assert!(!refuses_group_kill(args(0, 100), Some(hermit), hermit));
        // Real-time signals are refused too: SIGRTMIN and SIGRTMAX.
        assert!(refuses_group_kill(args(0, 34), Some(hermit), hermit));
        assert!(refuses_group_kill(args(0, 64), Some(hermit), hermit));
        assert!(!refuses_group_kill(
            args(0, libc::SIGUSR1 as i64),
            Some(7),
            hermit
        ));
        assert!(!refuses_group_kill(
            args(-1, libc::SIGUSR1 as i64),
            Some(hermit),
            hermit
        ));
        assert!(!refuses_group_kill(
            args(5, libc::SIGTERM as i64),
            Some(hermit),
            hermit
        ));
        // The register holds the pid in its low 32 bits.
        assert!(refuses_group_kill(
            args(1 << 32, libc::SIGUSR1 as i64),
            Some(hermit),
            hermit
        ));
        assert!(is_kill(libc::SYS_kill as i32, 0xc000_003e));
        assert!(is_kill(I386_KILL, AUDIT_ARCH_I386));
        assert!(!is_kill(I386_KILL, 0xc000_003e));
        assert!(!is_kill(libc::SYS_getpid as i32, 0xc000_003e));
    }
}
