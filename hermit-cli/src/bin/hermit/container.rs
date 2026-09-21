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
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::PermissionsExt;
use std::panic;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::OnceLock;

use anyhow::anyhow;
use detcore_model::config::MountInfoRootRewrite;
use hermit::Context;
use hermit::Error;
use hermit::FailureKind;
use hermit::HERMIT_DEADLINE_EXIT;
use hermit::SerializableError;
use hermit::SkidOvershootError;
use hermit::capture_mountinfo_identity_order;
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
const OVERFLOW_GID: &str = "65534";
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
        }
    }

    /// Include an automatically-created private `/tmp` in mountinfo provenance.
    /// User-supplied `--tmp` paths deliberately do not call this method.
    pub(super) fn add_private_tmp(&mut self, source: &Path) -> Result<(), Error> {
        self.mountinfo_roots
            .push(MountInfoRootSource::private_tmp(source)?);
        Ok(())
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
        self.mountinfo_roots
            .iter()
            .map(MountInfoRootSource::resolve)
            .collect()
    }

    /// Capture the exact raw mount-ID order used by Detcore's snapshot-local
    /// canonicalizer. This runs in the completed, quiescent guest namespace.
    pub(super) fn mountinfo_identity_order(&self) -> Result<Vec<u64>, Error> {
        capture_mountinfo_identity_order()
            .context("Failed to capture guest mountinfo identity order")
    }
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

/// Snapshot the host group database into a private temp file, appending a
/// synthetic overflow group (`nobody:x:65534`) when the host lacks one. Binding
/// this frozen copy read-only over `/etc/group` keeps guest group-name
/// resolution stable across otherwise-identical runs.
fn frozen_group_file() -> Result<tempfile::NamedTempFile, Error> {
    let mut contents = fs::read_to_string(GROUP_FILE)
        .context("Failed to read the host group database for the guest")?;
    let has_overflow_group = contents.lines().any(|line| {
        line.split(':')
            .nth(2)
            .is_some_and(|gid| gid == OVERFLOW_GID)
    });
    if !has_overflow_group {
        if !contents.ends_with('\n') {
            contents.push('\n');
        }
        contents.push_str("nobody:x:");
        contents.push_str(OVERFLOW_GID);
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

    Ok((
        mounts,
        IdentityGuard {
            _group_file: Some(group_file),
            _nscd_dir: nscd_dir,
            mountinfo_roots,
            user_mount_targets: Vec::new(),
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
    let mut container = Container::new();
    container
        .unshare(Namespace::PID)
        .map_root()
        .hostname("hermetic-container.local")
        .domainname("local")
        .mount(Mount::proc());

    apply_affinity(&mut container, pin_threads);
    container
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
        .domainname("local");

    // The cache is a deterministic input, not a writable container layer. Bind
    // it onto itself and remount it read-only so a guest cannot poison later
    // runs or mutate run one underneath `--verify` run two. A fresh writable
    // /tmp is mounted separately for ordinary scratch files.
    container.mount(Mount::bind(rootfs, rootfs));
    container.mount(
        Mount::new(rootfs)
            .flags(MountFlags::MS_BIND | MountFlags::MS_REMOUNT | MountFlags::MS_RDONLY),
    );
    container.mount(Mount::bind(tmpfs, rootfs.join("tmp")).rshared());

    // Mount the deterministic /proc into the target root. The materializer
    // guarantees <rootfs>/proc exists, so we do not need `touch_target()` (which
    // defers dir creation to the pre-exec child on a tiny clone stack).
    let proc_target = rootfs.join("proc");
    container.mount(Mount::proc().target(&proc_target));

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
    Ok((container, identity_sources))
}

/// A [`default_container`] hardened with the deterministic identity mounts
/// (frozen `/etc/group`, hidden nscd cache) that `run` mode applies. Record and
/// replay use this so guest NSS resolution matches `run` and does not reach
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
/// One residual gap: if the parent somehow died between the fork and this call,
/// no death signal will ever arrive. The usual `getppid()` guard for that race
/// is unavailable here, because inside a new PID namespace the out-of-namespace
/// parent is not mapped and `getppid()` reports 0 whether or not it is alive.
/// Closing it completely would mean arming the signal inside
/// `Container::run` before container setup, which is a Reverie-side change.
/// The window is a few milliseconds of container setup, and the case this
/// guards against is a supervisor deadline seconds or minutes later.
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

/// Arm both container-init guards. Call this as the FIRST statement inside any
/// `Container::run` closure that is not going through [`with_container`].
///
/// Why this is public rather than folded into [`with_container`]: `hermit record`
/// -- every spelling, including `record --verify` -- calls [`Container::run`]
/// directly at six sites in `record_start.rs` and never goes through
/// `with_container`. Those containers come from `recording_container()` ->
/// `deterministic_container()` -> `default_container(true)`, which unshares
/// `Namespace::PID`, so each one is a namespace init with exactly the bug the
/// guards exist to fix. An adversarial review of the original change caught this:
/// the claim that "all entry points funnel through `with_container`" was FALSE,
/// and `record --verify` is precisely the long-running command an external
/// deadline most needs to be able to end.
///
/// The structurally better home for this is reverie's `Container::run`, before
/// `setup()`, which would also close the fork-to-prctl window. That is a
/// reverie-side change plus a pin bump; this closes the six sites now.
pub(super) fn arm_container_init_guards() -> Result<(), SerializableError> {
    arm_parent_death_signal().map_err(SerializableError::from)?;
    install_container_init_stop_handlers().map_err(SerializableError::from)?;
    Ok(())
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
        _ => {}
    }
}

/// Returns a [`SerializableError`] rather than a bare [`Error`] so the CLASS
/// survives: a caught panic is tagged HERE, at the only point that still knows
/// one happened, and the tag then crosses the process boundary with the message.
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

/// Runs a container-child closure with panics converted to errors.
///
/// Every `Container::run` call site in Hermit should use this instead of
/// `run`, so that no Hermit closure can reach the nounwind child entry point
/// while unwinding. `with_container` uses it; so do the direct call sites in
/// `record_start`, which is the path the partial-revents-copyout replay
/// divergence takes.
pub trait RunGuarded {
    /// Runs a container-child closure with panics converted to errors, carrying
    /// the CALL SITE'S IDENTITY into the child.
    ///
    /// ⚠️ THE LABEL IS NOT OPTIONAL, AND THAT IS DELIBERATE. An unlabelled form was
    /// tried and removed: every site would have compiled while silently opting out
    /// of being addressable, which is exactly the state that left two sites
    /// untestable. Requiring the argument makes a new call site declare what it is,
    /// and `cargo` asks the question at the moment the site is added.
    ///
    /// The label is inert in production. It is read only by `inject_test_fault`,
    /// which is itself inert unless the test environment variables are set.
    fn run_guarded_at<F, T>(
        &mut self,
        site: &'static str,
        f: F,
    ) -> Result<Result<T, SerializableError>, RunError>
    where
        F: FnMut() -> Result<T, SerializableError>,
        T: serde::Serialize + serde::de::DeserializeOwned;
}

impl RunGuarded for Container {
    fn run_guarded_at<F, T>(
        &mut self,
        site: &'static str,
        mut f: F,
    ) -> Result<Result<T, SerializableError>, RunError>
    where
        F: FnMut() -> Result<T, SerializableError>,
        T: serde::Serialize + serde::de::DeserializeOwned,
    {
        self.run(move || {
            install_panic_location_hook();
            match panic::catch_unwind(panic::AssertUnwindSafe(|| {
                inject_test_fault(site);
                f()
            })) {
                Ok(result) => result,
                // Tagged AT THE CATCH SITE, the only place that still knows a
                // panic is what happened. Everything downstream sees prose.
                Err(payload) => Err(SerializableError::from(anyhow!(
                    "panic in container child at {}: {}",
                    take_panic_location(),
                    panic_message(&*payload)
                ))
                .into_panic()),
            }
        })
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

/// Helper to run a function inside a container, taking care to display any
/// errors and propagate the exit status.
pub fn with_container<F, T>(container: &mut Container, mut f: F) -> Result<T, Error>
where
    F: FnMut() -> Result<T, Error>,
    T: serde::Serialize + serde::de::DeserializeOwned,
{
    classify_container_result(container.run(|| {
        // Runs in the freshly forked container init, not in the caller.
        arm_container_init_guards()?;
        catch_child_panic(&mut f)
    }))
}

/// Turn a `Container::run` / [`RunGuarded::run_guarded`] outcome into an error
/// whose CLASS is still readable by `classify_failure`.
///
/// ⚠️ SHARED BECAUSE `with_container` IS NOT THE ONLY BOUNDARY. `hermit record`
/// -- every spelling -- calls [`RunGuarded::run_guarded`] directly at six sites
/// in `record_start.rs`, so a discard there loses exactly what this change
/// exists to preserve and the failure surfaces as `class=cli-error`.
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

/// Postfix spelling of [`classify_container_result`], so a direct
/// `run_guarded` call site reads the same way `with_container` does.
pub trait Classified<T> {
    fn classified(self) -> Result<T, Error>;
}

impl<T> Classified<T> for Result<Result<T, SerializableError>, RunError> {
    fn classified(self) -> Result<T, Error> {
        classify_container_result(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(
            contents
                .lines()
                .any(|line| line.split(':').nth(2) == Some(OVERFLOW_GID)),
            "frozen group database must resolve overflow gid {OVERFLOW_GID}:\n{contents}"
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
}
