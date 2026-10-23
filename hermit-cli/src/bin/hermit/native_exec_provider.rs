/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Native successor provision component. Installation adds no listener or
//! native exec; its caller owns the measured root/watch/launch admission.
//! Request fields identify a retained transaction; they supply no authority.

use std::collections::BTreeMap;
use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::os::fd::RawFd;
use std::os::unix::fs::FileExt;
use std::os::unix::fs::MetadataExt;
use std::sync::Arc;
use std::sync::Mutex;

use detcore::detlog::NativePhysicalWatchIdentity;
use detcore::native_exec_control::ExecTicket;
use detcore::native_exec_control::NativeExecRegistry;
use detcore::native_exec_control::NativeExecSocketBinding;
use detcore::native_exec_control::NativeExecSocketBindings;
use detcore_model::liteinst_native_exec_input as input;
use hermit::in_guest_exits::NativeWatchedProcess;
use hermit::physical_exit_watch;
use sha2::Digest;
use sha2::Sha256;

use super::native_exec_artifacts::QualifiedNativeArtifacts;
use super::native_exec_inputs::HeldDynamicInputs;
use super::native_exec_inputs::InputRole;

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}
fn alive(fd: &OwnedFd) -> io::Result<()> {
    let mut status = libc::pollfd {
        fd: fd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let result = unsafe { libc::poll(&mut status, 1, 0) };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    if result != 0 || status.revents != 0 {
        return Err(invalid("actual retained birth is no longer live"));
    }
    Ok(())
}
fn sealed(_name: &std::ffi::CStr, bytes: &[u8]) -> io::Result<OwnedFd> {
    super::native_exec_inputs::seal_bytes(bytes, InputRole::Data)
        .map(Into::into)
        .map_err(|error| invalid(error.to_string()))
}
// Owned alias of the already-registered pidfd, never a watcher/PID factory.
struct RetainedPeer {
    fd: OwnedFd,
    identity: u64,
    pid: i32,
}

enum WatchRegistration {
    Actual(Arc<NativeWatchedProcess>),
    // Old protocol fixtures model watch authority only. This branch cannot be
    // constructed by the production installer or parsed request.
    #[cfg(test)]
    Model(Arc<hermit::in_guest_exits::InGuestExitAdmission>),
}
impl WatchRegistration {
    fn healthy(&self, peer: &RetainedPeer) -> bool {
        match self {
            Self::Actual(watch) => {
                watch.watch_is_healthy()
                    && watch.process_id() == peer.pid
                    && watch.birth_identity() == peer.identity
                    && physical_exit_watch::birth_identity(&peer.fd) == Some(peer.identity)
            }
            #[cfg(test)]
            Self::Model(watch) => watch.failure().is_none(),
        }
    }
}
enum QualificationOwner {
    Actual {
        inputs: Arc<HeldDynamicInputs>,
        artifacts: Arc<QualifiedNativeArtifacts>,
    },
    #[cfg(test)]
    Model,
}

const SO_PEERPIDFD: libc::c_int = 77;
const REQUEST_LEN: usize = 48;
const RESPONSE_LEN: usize = 80;
const RIGHTS_COUNT: usize = 5;
const ALL_SEALS: libc::c_int = libc::F_SEAL_WRITE
    | libc::F_SEAL_GROW
    | libc::F_SEAL_SHRINK
    | libc::F_SEAL_SEAL
    | libc::F_SEAL_FUTURE_WRITE
    | 0x0020; // F_SEAL_EXEC
/// Explicit host-only bounded first-domain source size, not a control-packet cap.
const MAX_HELD_BYTES: u64 = 256 * 1024 * 1024;
const NATIVE_CONFIG_LEN: usize = 1552;

/// Independently retained launch data. These values must be computed/held before
/// the native request, never copied from it or from a decoded bootstrap packet.
pub(super) struct ProviderBindings {
    pub config_digest: [u8; 32],
    pub config_schema: [u8; 16],
    pub native_source_digest: [u8; 32],
    pub coordinator: Vec<u8>,
    pub detlog_identity: String,
    pub forward_policy: String,
    pub observer_identity: String,
}

impl ProviderBindings {
    fn sockets(&self) -> io::Result<NativeExecSocketBindings> {
        let parse = |value: &str| -> io::Result<NativeExecSocketBinding> {
            let (fd, (device, inode)) = detcore::detlog::parse_tool_output_env_value(value)
                .ok_or_else(|| invalid("independently held socket identity malformed"))?;
            Ok(NativeExecSocketBinding { fd, device, inode })
        };
        let sockets = NativeExecSocketBindings {
            detlog: parse(&self.detlog_identity)?,
            observer: parse(&self.observer_identity)?,
        };
        sockets
            .validate()
            .map_err(|_| invalid("held socket roles collide"))?;
        Ok(sockets)
    }
}

/// Independent exact source digest for each opened descriptor's concrete role.
pub(crate) struct ObjectDigests {
    pub stock_loader: [u8; 32],
    pub runner: [u8; 32],
    pub tool_loader: [u8; 32],
    pub native_config: [u8; 32],
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Identity {
    dev: u64,
    ino: u64,
    size: u64,
    mode: u32,
    uid: u32,
    gid: u32,
    nlink: u64,
    mtime: i64,
    mtime_ns: i64,
    ctime: i64,
    ctime_ns: i64,
}
impl Identity {
    fn read(file: &File) -> io::Result<Self> {
        let m = file.metadata()?;
        if !m.is_file() || m.len() == 0 || m.len() > MAX_HELD_BYTES {
            return Err(invalid("held native object is not a bounded regular file"));
        }
        Ok(Self {
            dev: m.dev(),
            ino: m.ino(),
            size: m.size(),
            mode: m.mode(),
            uid: m.uid(),
            gid: m.gid(),
            nlink: m.nlink(),
            mtime: m.mtime(),
            mtime_ns: m.mtime_nsec(),
            ctime: m.ctime(),
            ctime_ns: m.ctime_nsec(),
        })
    }
}
struct HeldObject {
    file: File,
    identity: Identity,
    digest: [u8; 32],
    sealed: bool,
}
impl HeldObject {
    fn retain(file: File, digest: [u8; 32], sealed: bool) -> io::Result<Self> {
        if digest == [0; 32] {
            return Err(invalid("missing independent native object digest"));
        }
        let object = Self {
            identity: Identity::read(&file)?,
            file,
            digest,
            sealed,
        };
        object.verify()?;
        Ok(object)
    }
    fn verify(&self) -> io::Result<()> {
        if Identity::read(&self.file)? != self.identity {
            return Err(invalid("held object metadata changed"));
        }
        let fd = self.file.as_raw_fd();
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        let fdflags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        if flags < 0 || fdflags < 0 {
            return Err(io::Error::last_os_error());
        }
        if fdflags & libc::FD_CLOEXEC == 0
            || flags & libc::O_PATH != 0
            || (!self.sealed && flags & libc::O_ACCMODE != libc::O_RDONLY)
        {
            return Err(invalid("held source flags do not fit its role"));
        }
        if self.sealed {
            check_seals(fd)?;
        }
        // pread never changes this OFD's offset. Finite retained size and fixed
        // scratch bound memory; synchronous filesystem IO still needs containment.
        let mut hash = Sha256::new();
        let mut scratch = [0u8; 16 * 1024];
        let mut offset = 0;
        while offset < self.identity.size {
            let length = (self.identity.size - offset).min(scratch.len() as u64) as usize;
            let n = self.file.read_at(&mut scratch[..length], offset)?;
            if n == 0 {
                return Err(invalid("held object shortened during exact read"));
            }
            hash.update(&scratch[..n]);
            offset += n as u64;
        }
        if self.file.read_at(&mut scratch[..1], offset)? != 0
            || Identity::read(&self.file)? != self.identity
            || <[u8; 32]>::from(hash.finalize()) != self.digest
        {
            return Err(invalid("held object exact digest/identity differs"));
        }
        Ok(())
    }
}
fn check_seals(fd: RawFd) -> io::Result<()> {
    let value = unsafe { libc::fcntl(fd, libc::F_GET_SEALS) };
    if value < 0 {
        return Err(io::Error::last_os_error());
    }
    if value != ALL_SEALS {
        return Err(invalid("native input is not fully sealed"));
    }
    Ok(())
}
fn verify_native_config(file: &File) -> io::Result<()> {
    if file.metadata()?.len() != NATIVE_CONFIG_LEN as u64 {
        return Err(invalid("native config exact SE ABI size differs"));
    }
    let mut bytes = [0u8; NATIVE_CONFIG_LEN];
    file.read_exact_at(&mut bytes, 0)?;
    if bytes[..8] != 1u64.to_le_bytes() || bytes[8..16] != (NATIVE_CONFIG_LEN as u64).to_le_bytes()
    {
        return Err(invalid("native config ABI header differs"));
    }
    for path in [&bytes[16..528], &bytes[528..1552]] {
        let end = path
            .iter()
            .position(|b| *b == 0)
            .ok_or_else(|| invalid("unterminated native config absolute path"))?;
        if end <= 1 || path[0] != b'/' || path[..end].iter().any(|b| b":$".contains(b)) {
            return Err(invalid(
                "native config path violates SE absolute-path contract",
            ));
        }
    }
    Ok(())
}

/// Owned held descriptors; final input is created only from the sealed registry.
pub(crate) struct HeldFiles {
    stock_loader: HeldObject,
    runner: HeldObject,
    tool_loader: HeldObject,
    native_config: HeldObject,
}
impl HeldFiles {
    pub(crate) fn retain(
        stock_loader: File,
        runner: File,
        enrolled_tool_loader: File,
        native_config: File,
        expected: ObjectDigests,
    ) -> io::Result<Self> {
        // SE's actual launcher duplicates one stock-loader OFD, opened before
        // the private interpreter bind, into both198 and200. Preserve that
        // contract by cloning this owned OFD, never accepting another role path.
        let checked_tool_loader =
            HeldObject::retain(enrolled_tool_loader, expected.tool_loader, true)?;
        if Identity::read(&stock_loader)? != checked_tool_loader.identity
            || expected.stock_loader != expected.tool_loader
        {
            return Err(invalid(
                "tool loader is not the exact enrolled stock-loader object",
            ));
        }
        let tool_loader = stock_loader.try_clone()?;
        let result = Self {
            stock_loader: HeldObject::retain(stock_loader, expected.stock_loader, true)?,
            runner: HeldObject::retain(runner, expected.runner, true)?,
            tool_loader: HeldObject::retain(tool_loader, expected.tool_loader, true)?,
            native_config: HeldObject::retain(native_config, expected.native_config, true)?,
        };
        verify_native_config(&result.native_config.file)?;
        Ok(result)
    }
    fn verify(&self) -> io::Result<()> {
        for object in [
            &self.stock_loader,
            &self.runner,
            &self.tool_loader,
            &self.native_config,
        ] {
            object.verify()?;
        }
        verify_native_config(&self.native_config.file)
    }
}

/// Independently selected role bundle. Keys are held target bindings, never
/// request paths or parsed bootstrap fields.
struct CatalogEntry {
    target_digest: [u8; 32],
    bootstrap_digest: [u8; 32],
    files: HeldFiles,
}

/// Host transfer owner. Production installation retains the actual existing
/// physical watcher and privately qualified artifact/input owners.
pub(super) struct Provider {
    global: Arc<detcore::GlobalState>,
    birth: RetainedPeer,
    watch: WatchRegistration,
    qualification: QualificationOwner,
    catalog: BTreeMap<[u8; 32], CatalogEntry>,
    expected: ProviderBindings,
}
impl Provider {
    /// # Safety
    /// The admission owner must independently establish the actual immutable
    /// root/namespace/mappings and covered target/PT_INTERP/runner/Tool chain
    /// against these exact held sources. It must preserve original operation,
    /// one-task preparation, mask/filter/FD cut and unchanged Config. This
    /// exact watcher is also the registry proof's retained birth and final
    /// completion owner. Complete compiled source qualification and actual
    /// distinct protected DETLOG/observer endpoints are required; catalog data,
    /// file hashes and identity strings alone never establish these premises.
    /// All retained OFD flags/metadata and caller aliases stay unchanged.
    pub(super) unsafe fn install(
        global: Arc<detcore::GlobalState>,
        watch: Arc<NativeWatchedProcess>,
        inputs: Arc<HeldDynamicInputs>,
        artifacts: Arc<QualifiedNativeArtifacts>,
        expected: ProviderBindings,
    ) -> io::Result<Self> {
        if global.native_exec_registry().is_none()
            || !watch.watch_is_healthy()
            || watch.process_id() <= 0
            || watch.birth_identity() == 0
            || expected.native_source_digest != artifacts.source_digest()
            || !Arc::ptr_eq(&inputs, artifacts.inputs())
            || [expected.config_digest, expected.native_source_digest].contains(&[0; 32])
        {
            return Err(invalid(
                "actual watched registry/qualified source binding is absent",
            ));
        }
        inputs
            .recheck()
            .map_err(|error| invalid(error.to_string()))?;
        let mut catalog = BTreeMap::new();
        for entry in artifacts.catalog() {
            if [entry.target_digest, entry.bootstrap_digest].contains(&[0; 32]) {
                return Err(invalid("missing independently enrolled catalog binding"));
            }
            let duplicate = |fd: std::os::fd::BorrowedFd<'_>| -> io::Result<File> {
                fd.try_clone_to_owned().map(File::from)
            };
            let files = HeldFiles::retain(
                duplicate(entry.stock_loader())?,
                duplicate(entry.runner())?,
                duplicate(entry.tool_loader())?,
                duplicate(entry.native_config())?,
                ObjectDigests {
                    stock_loader: entry.stock_loader_digest(),
                    runner: entry.runner_digest(),
                    tool_loader: entry.tool_loader_digest(),
                    native_config: entry.native_config_digest(),
                },
            )?;
            if catalog
                .insert(
                    entry.target_digest,
                    CatalogEntry {
                        target_digest: entry.target_digest,
                        bootstrap_digest: entry.bootstrap_digest,
                        files,
                    },
                )
                .is_some()
            {
                return Err(invalid("duplicate/ambiguous provider target binding"));
            }
        }
        if catalog.is_empty() {
            return Err(invalid("qualified provider catalog is empty"));
        }
        let fd = watch.retained_pidfd().try_clone_to_owned()?;
        let birth = RetainedPeer {
            fd,
            identity: watch.birth_identity(),
            pid: watch.process_id(),
        };
        alive(&birth.fd)?;
        let result = Self {
            global,
            birth,
            watch: WatchRegistration::Actual(watch),
            qualification: QualificationOwner::Actual { inputs, artifacts },
            catalog,
            expected,
        };
        result.check_birth(&result.birth.fd)?;
        // Validate every exact packet shape/cap using independently held roles
        // before any transaction; placeholder affine fields confer no authority.
        for entry in result.catalog.values() {
            result.packet(
                entry,
                (ExecTicket([1; 32]), [1; 32]),
                entry.target_digest,
                entry.bootstrap_digest,
                result.expected.config_digest,
                result.expected.sockets()?,
            )?;
        }
        Ok(result)
    }

    #[cfg(test)]
    unsafe fn install_model(
        global: Arc<detcore::GlobalState>,
        birth: &RetainedPeer,
        watched: Arc<hermit::in_guest_exits::InGuestExitAdmission>,
        files: HeldFiles,
        expected: ProviderBindings,
    ) -> io::Result<Self> {
        // Protocol-only fixture, never a NativeWatchedProcess/qualified-source
        // factory and never used by production install.
        let fd = birth.fd.try_clone()?;
        alive(&fd)?;
        files.verify()?;
        let result = Self {
            global,
            birth: RetainedPeer {
                fd,
                pid: birth.pid,
                identity: birth.identity,
            },
            watch: WatchRegistration::Model(watched),
            qualification: QualificationOwner::Model,
            catalog: [(
                [2; 32],
                CatalogEntry {
                    target_digest: [2; 32],
                    bootstrap_digest: [3; 32],
                    files,
                },
            )]
            .into(),
            expected,
        };
        result.packet(
            result.model_entry(),
            (ExecTicket([1; 32]), [1; 32]),
            [2; 32],
            [3; 32],
            result.expected.config_digest,
            result.expected.sockets()?,
        )?;
        Ok(result)
    }
    #[cfg(test)]
    fn model_entry(&self) -> &CatalogEntry {
        self.catalog.get(&[2; 32]).unwrap()
    }
    #[cfg(test)]
    fn model_entry_mut(&mut self) -> &mut CatalogEntry {
        self.catalog.get_mut(&[2; 32]).unwrap()
    }

    fn registry(&self) -> io::Result<&Mutex<NativeExecRegistry>> {
        self.global
            .native_exec_registry()
            .ok_or_else(|| invalid("native registry absent"))
    }
    fn check_qualification(&self) -> io::Result<()> {
        match &self.qualification {
            QualificationOwner::Actual { inputs, artifacts } => {
                if !Arc::ptr_eq(inputs, artifacts.inputs())
                    || artifacts.source_digest() != self.expected.native_source_digest
                {
                    return Err(invalid("provider held input/source owners differ"));
                }
                inputs.recheck().map_err(|error| invalid(error.to_string()))
            }
            #[cfg(test)]
            QualificationOwner::Model => Ok(()),
        }
    }
    fn authenticate(&self, socket: &OwnedFd) -> io::Result<OwnedFd> {
        let mut kind: libc::c_int = 0;
        let mut size = std::mem::size_of_val(&kind) as libc::socklen_t;
        if unsafe {
            libc::getsockopt(
                socket.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_TYPE,
                (&raw mut kind).cast(),
                &mut size,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        if size as usize != std::mem::size_of_val(&kind) || kind != libc::SOCK_SEQPACKET {
            return Err(invalid(
                "provider requires an accepted Unix SOCK_SEQPACKET endpoint",
            ));
        }
        let mut domain: libc::c_int = 0;
        size = std::mem::size_of_val(&domain) as libc::socklen_t;
        if unsafe {
            libc::getsockopt(
                socket.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_DOMAIN,
                (&raw mut domain).cast(),
                &mut size,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        if size as usize != std::mem::size_of_val(&domain) || domain != libc::AF_UNIX {
            return Err(invalid("provider endpoint is not an actual Unix socket"));
        }
        let mut raw: libc::c_int = -1;
        size = std::mem::size_of_val(&raw) as libc::socklen_t;
        if unsafe {
            libc::getsockopt(
                socket.as_raw_fd(),
                libc::SOL_SOCKET,
                SO_PEERPIDFD,
                (&raw mut raw).cast(),
                &mut size,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        if raw < 0 {
            return Err(invalid("SO_PEERPIDFD did not return an owned descriptor"));
        }
        let peer = unsafe { OwnedFd::from_raw_fd(raw) };
        if size as usize != std::mem::size_of_val(&raw) {
            return Err(invalid("SO_PEERPIDFD exact size differs"));
        }
        let flags = unsafe { libc::fcntl(peer.as_raw_fd(), libc::F_GETFD) };
        if flags < 0
            || unsafe { libc::fcntl(peer.as_raw_fd(), libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0
        {
            return Err(io::Error::last_os_error());
        }
        self.check_birth(&peer)?;
        Ok(peer)
    }
    fn check_birth(&self, peer: &OwnedFd) -> io::Result<()> {
        alive(&self.birth.fd)?;
        alive(peer)?;
        if !self.watch.healthy(&self.birth)
            || physical_exit_watch::birth_identity(peer) != Some(self.birth.identity)
            || physical_exit_watch::birth_identity(&self.birth.fd) != Some(self.birth.identity)
        {
            return Err(invalid(
                "provider actual peer/watched birth differs or failed",
            ));
        }
        Ok(())
    }
    fn packet(
        &self,
        entry: &CatalogEntry,
        transaction: (ExecTicket, [u8; 32]),
        target: [u8; 32],
        bootstrap: [u8; 32],
        config: [u8; 32],
        sockets: NativeExecSocketBindings,
    ) -> io::Result<Vec<u8>> {
        let (ticket, composite) = transaction;
        let e = &self.expected;
        let held = e.sockets()?;
        sockets
            .validate()
            .map_err(|_| invalid("admitted socket roles collide"))?;
        if sockets.detlog.object() != held.detlog.object()
            || sockets.observer.object() != held.observer.object()
        {
            return Err(invalid(
                "admitted socket objects differ from held launch sockets",
            ));
        }
        // The affine binding supplies old-image handoff slot numbers only.
        // Object identities remain independently held launch facts; the fresh
        // successor will protect/relocate these same objects again.
        let detlog_identity = format!(
            "{}:{:016x}:{:016x}",
            sockets.detlog.fd, held.detlog.device, held.detlog.inode
        );
        let observer_identity = format!(
            "{}:{:016x}:{:016x}",
            sockets.observer.fd, held.observer.device, held.observer.inode
        );
        let packet = input::Packet {
            config_digest: &config,
            config_schema: &e.config_schema,
            native_source_digest: &e.native_source_digest,
            target_digest: &target,
            bootstrap_digest: &bootstrap,
            image: input::Image::Successor {
                ticket: &ticket.0,
                composite_digest: &composite,
            },
            coordinator: &e.coordinator,
            detlog_identity: &detlog_identity,
            forward_policy: &e.forward_policy,
            observer_identity: &observer_identity,
        };
        packet
            .check_bindings(input::ExpectedBindings {
                config_digest: &e.config_digest,
                config_schema: &e.config_schema,
                native_source_digest: &e.native_source_digest,
                target_digest: &entry.target_digest,
                bootstrap_digest: &entry.bootstrap_digest,
            })
            .map_err(|_| invalid("registry differs from independently held provider bindings"))?;
        input::encode(packet).map_err(|error| invalid(error.to_string()))
    }

    /// Serve exactly one readable accepted endpoint. Consume before packet build
    /// and one nonblocking SCM_RIGHTS send. This returns host transfer evidence,
    /// never an exec-success result. Guest exact validation remains mandatory.
    pub(super) fn serve(&self, socket: &OwnedFd) -> io::Result<()> {
        // A wrong peer cannot consume or poison the genuine registry.
        let peer = self.authenticate(socket)?;
        let ticket = match receive_ticket(socket) {
            Ok(ticket) => ticket,
            Err(error) => {
                self.registry()?
                    .lock()
                    .map_err(|_| invalid("native registry poisoned"))?
                    .provision_transport_failed();
                return Err(error);
            }
        };
        let mut registry = self
            .registry()?
            .lock()
            .map_err(|_| invalid("native registry poisoned"))?;
        let lease = registry
            .begin_provision(&peer, ticket)
            .map_err(|error| invalid(format!("native provision refused: {error:?}")))?;
        // Everything from here either completes this affine consumed attempt or
        // drops it into a retained Failed tombstone. No second send is attempted.
        self.check_birth(&peer)?;
        self.check_qualification()?;
        let binding = lease.binding();
        // Selection is from the retained, independently qualified lease binding.
        // The request carries only a ticket; it cannot supply role paths/digests.
        let entry = self
            .catalog
            .get(&binding.target_digest)
            .ok_or_else(|| invalid("retained target has no qualified provider catalog entry"))?;
        entry.files.verify()?;
        let composite = lease.state_digest();
        let bytes = self.packet(
            entry,
            (ticket, composite),
            binding.target_digest,
            binding.bootstrap_digest,
            binding.context.config_digest,
            binding.sockets,
        )?;
        let input = sealed(c"exec-native-successor-input", &bytes)?;
        check_seals(input.as_raw_fd())?;
        self.check_birth(&peer)?;
        let response = response(ticket, composite);
        let rights = [
            entry.files.stock_loader.file.as_raw_fd(),
            entry.files.runner.file.as_raw_fd(),
            entry.files.tool_loader.file.as_raw_fd(),
            input.as_raw_fd(),
            entry.files.native_config.file.as_raw_fd(),
        ];
        send_objects(socket, &response, rights)?;
        self.check_birth(&peer)?;
        // SAFETY: exact response plus all five independently checked held role
        // descriptors were just transferred in one successful bounded sendmsg.
        // complete also rechecks the genuine registry proof's watch liveness.
        unsafe { lease.complete() }
            .map_err(|error| invalid(format!("provision completion: {error:?}")))
    }
}

fn response(ticket: ExecTicket, digest: [u8; 32]) -> [u8; RESPONSE_LEN] {
    let mut bytes = [0u8; RESPONSE_LEN];
    bytes[..8].copy_from_slice(b"HLPRACK1");
    bytes[8..10].copy_from_slice(&1u16.to_be_bytes());
    bytes[10..12].copy_from_slice(&(RESPONSE_LEN as u16).to_be_bytes());
    bytes[12..14].copy_from_slice(&(RIGHTS_COUNT as u16).to_be_bytes());
    bytes[16..48].copy_from_slice(&ticket.0);
    bytes[48..80].copy_from_slice(&digest);
    bytes
}
fn receive_ticket(socket: &OwnedFd) -> io::Result<ExecTicket> {
    let mut bytes = [0u8; REQUEST_LEN];
    let mut control = [0u64; 8];
    let mut iov = libc::iovec {
        iov_base: bytes.as_mut_ptr().cast(),
        iov_len: bytes.len(),
    };
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = std::mem::size_of_val(&control);
    let n = unsafe {
        libc::recvmsg(
            socket.as_raw_fd(),
            &mut msg,
            libc::MSG_DONTWAIT | libc::MSG_TRUNC | libc::MSG_CMSG_CLOEXEC,
        )
    };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut ancillary = false;
    let mut cmsg = unsafe { libc::CMSG_FIRSTHDR(&msg) };
    while !cmsg.is_null() {
        ancillary = true;
        let header = unsafe { &*cmsg };
        let minimum = unsafe { libc::CMSG_LEN(0) } as usize;
        let offset = cmsg as usize - msg.msg_control as usize;
        if header.cmsg_len < minimum
            || offset > msg.msg_controllen
            || header.cmsg_len > msg.msg_controllen - offset
        {
            break;
        }
        if header.cmsg_level == libc::SOL_SOCKET && header.cmsg_type == libc::SCM_RIGHTS {
            let count = (header.cmsg_len - minimum) / std::mem::size_of::<RawFd>();
            for index in 0..count {
                let raw = unsafe {
                    libc::CMSG_DATA(cmsg)
                        .cast::<RawFd>()
                        .add(index)
                        .read_unaligned()
                };
                // Kernel-delivered rights are ours; close them on this refusal.
                drop(unsafe { OwnedFd::from_raw_fd(raw) });
            }
        }
        cmsg = unsafe { libc::CMSG_NXTHDR(&msg, cmsg) };
    }
    if n as usize != bytes.len()
        || msg.msg_flags & (libc::MSG_TRUNC | libc::MSG_CTRUNC) != 0
        || ancillary
        || bytes[..8] != *b"HLPROV01"
        || bytes[8..10] != 1u16.to_be_bytes()
        || bytes[10..12] != (REQUEST_LEN as u16).to_be_bytes()
        || bytes[44..] != [0; 4]
    {
        return Err(invalid(
            "native provision request exact framing/ancillary mismatch",
        ));
    }
    Ok(ExecTicket(bytes[12..44].try_into().unwrap()))
}
fn send_objects(
    socket: &OwnedFd,
    bytes: &[u8; RESPONSE_LEN],
    rights: [RawFd; RIGHTS_COUNT],
) -> io::Result<()> {
    // CMSG_SPACE(5*sizeof(int)) ==40 on the covered x86-64 Linux ABI.
    let mut control = [0u64; 5];
    if unsafe { libc::CMSG_SPACE(std::mem::size_of_val(&rights) as u32) } as usize
        != std::mem::size_of_val(&control)
    {
        return Err(invalid("native provision ancillary ABI differs"));
    }
    let mut iov = libc::iovec {
        iov_base: bytes.as_ptr().cast_mut().cast(),
        iov_len: bytes.len(),
    };
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = std::mem::size_of_val(&control);
    let cmsg = unsafe { libc::CMSG_FIRSTHDR(&msg) };
    if cmsg.is_null() {
        return Err(invalid("no native provision rights storage"));
    }
    unsafe {
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(std::mem::size_of_val(&rights) as u32) as usize;
        std::ptr::copy_nonoverlapping(rights.as_ptr(), libc::CMSG_DATA(cmsg).cast(), rights.len());
    }
    let n = unsafe {
        libc::sendmsg(
            socket.as_raw_fd(),
            &msg,
            libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL,
        )
    };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    if n as usize != bytes.len() {
        return Err(invalid(
            "partial native provision descriptor transfer is terminal",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;

    use detcore::native_exec_control::ExecControlError;
    use detcore::native_exec_control::ExecStage;
    use detcore::native_exec_control::NativeExecContext;
    use detcore::native_exec_control::NativeExecNeutralBinding;
    use detcore::native_exec_control::NativeExecProof;
    use detcore::native_exec_control::PrepareNativeExec;
    use detcore::types::DetTid;
    use detcore::types::MmId;
    use detcore_model::fd::FilesId;
    use reverie::GlobalTool;

    use super::*;

    // Deliberate model: real self pidfds and SCM_RIGHTS, but no launcher
    // namespace, genuine exec preparation, admitted watch or kernel exec.
    #[derive(Debug)]
    struct MockProof {
        birth: OwnedFd,
        live: AtomicBool,
        socket_objects: [(u64, u64); 2],
    }
    unsafe impl NativeExecProof for MockProof {
        fn owner(&self) -> DetTid {
            DetTid::from_raw(41)
        }
        fn physical_ids(&self) -> Option<(i32, i32)> {
            // Main-thread identity is deliberately modeled: this fixture
            // proves provider protocol/FD mechanics, never native admission.
            let pid = std::process::id() as i32;
            Some((pid, pid))
        }
        fn is_live(&self) -> bool {
            self.live.load(Ordering::SeqCst)
        }
        fn matches_peer(&self, peer: &OwnedFd) -> bool {
            physical_exit_watch::birth_identity(peer).is_some()
                && physical_exit_watch::birth_identity(peer)
                    == physical_exit_watch::birth_identity(&self.birth)
        }
        fn qualify_target(&self, request: &PrepareNativeExec) -> bool {
            request.target_digest == [2; 32]
                && request.bootstrap_digest == [3; 32]
                && request.resume_mask == 0x42
                && [
                    request.sockets.detlog.object(),
                    request.sockets.observer.object(),
                ] == self.socket_objects
        }
        fn qualify_runtime(&self, bytes: &[u8]) -> bool {
            bytes == b"provider-runtime-model"
        }
    }
    fn model_pidfd(pid: u32) -> io::Result<OwnedFd> {
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as i32, 0) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(unsafe { OwnedFd::from_raw_fd(fd as i32) })
    }
    fn sockets() -> (OwnedFd, OwnedFd) {
        let mut raw = [-1; 2];
        assert_eq!(
            unsafe {
                libc::socketpair(
                    libc::AF_UNIX,
                    libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
                    0,
                    raw.as_mut_ptr(),
                )
            },
            0
        );
        unsafe { (OwnedFd::from_raw_fd(raw[0]), OwnedFd::from_raw_fd(raw[1])) }
    }
    fn request(ticket: ExecTicket) -> [u8; REQUEST_LEN] {
        let mut bytes = [0u8; REQUEST_LEN];
        bytes[..8].copy_from_slice(b"HLPROV01");
        bytes[8..10].copy_from_slice(&1u16.to_be_bytes());
        bytes[10..12].copy_from_slice(&(REQUEST_LEN as u16).to_be_bytes());
        bytes[12..44].copy_from_slice(&ticket.0);
        bytes
    }
    fn send(socket: &OwnedFd, bytes: &[u8]) {
        assert_eq!(
            unsafe {
                libc::send(
                    socket.as_raw_fd(),
                    bytes.as_ptr().cast(),
                    bytes.len(),
                    libc::MSG_NOSIGNAL,
                )
            },
            bytes.len() as isize
        );
    }
    fn no_response(socket: &OwnedFd) {
        let mut byte = 0u8;
        assert_eq!(
            unsafe {
                libc::recv(
                    socket.as_raw_fd(),
                    (&raw mut byte).cast(),
                    1,
                    libc::MSG_DONTWAIT,
                )
            },
            -1
        );
        assert_eq!(
            io::Error::last_os_error().raw_os_error(),
            Some(libc::EAGAIN)
        );
    }
    fn read_reply(socket: &OwnedFd) -> ([u8; RESPONSE_LEN], Vec<OwnedFd>) {
        let mut bytes = [0u8; RESPONSE_LEN];
        let mut control = [0u64; 5];
        let mut iov = libc::iovec {
            iov_base: bytes.as_mut_ptr().cast(),
            iov_len: bytes.len(),
        };
        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = control.as_mut_ptr().cast();
        msg.msg_controllen = std::mem::size_of_val(&control);
        assert_eq!(
            unsafe {
                libc::recvmsg(
                    socket.as_raw_fd(),
                    &mut msg,
                    libc::MSG_DONTWAIT | libc::MSG_CMSG_CLOEXEC,
                )
            },
            RESPONSE_LEN as isize
        );
        assert_eq!(msg.msg_flags & (libc::MSG_TRUNC | libc::MSG_CTRUNC), 0);
        let cmsg = unsafe { libc::CMSG_FIRSTHDR(&msg) };
        assert!(!cmsg.is_null());
        assert_eq!(unsafe { (*cmsg).cmsg_level }, libc::SOL_SOCKET);
        assert_eq!(unsafe { (*cmsg).cmsg_type }, libc::SCM_RIGHTS);
        assert_eq!(unsafe { (*cmsg).cmsg_len }, unsafe {
            libc::CMSG_LEN((RIGHTS_COUNT * std::mem::size_of::<RawFd>()) as u32)
        } as usize);
        let rights = (0..RIGHTS_COUNT)
            .map(|index| unsafe {
                OwnedFd::from_raw_fd(
                    libc::CMSG_DATA(cmsg)
                        .cast::<RawFd>()
                        .add(index)
                        .read_unaligned(),
                )
            })
            .collect();
        assert!(unsafe { libc::CMSG_NXTHDR(&msg, cmsg) }.is_null());
        (bytes, rights)
    }
    fn source() -> (File, [u8; 32]) {
        // Fixture executable stands in for each distinct declared role. Never
        // executed here; the installer authority is explicitly modelled.
        let file = File::open("/bin/true").unwrap();
        let mut bytes = vec![0u8; file.metadata().unwrap().len() as usize];
        file.read_exact_at(&mut bytes, 0).unwrap();
        (file, Sha256::digest(&bytes).into())
    }
    fn held() -> HeldFiles {
        let (stock_original, a) = source();
        let (runner_original, b) = source();
        let seal_role = |source: File| {
            let mut bytes = vec![0u8; source.metadata().unwrap().len() as usize];
            source.read_exact_at(&mut bytes, 0).unwrap();
            super::super::native_exec_inputs::seal_bytes(&bytes, InputRole::Executable).unwrap()
        };
        let stock_loader = seal_role(stock_original);
        let runner = seal_role(runner_original);
        let tool_loader = stock_loader.try_clone().unwrap();
        let mut bytes = [0u8; NATIVE_CONFIG_LEN];
        bytes[..8].copy_from_slice(&1u64.to_le_bytes());
        bytes[8..16].copy_from_slice(&(NATIVE_CONFIG_LEN as u64).to_le_bytes());
        bytes[16..31].copy_from_slice(b"/fixture/runner");
        bytes[528..540].copy_from_slice(b"/fixture/lib");
        let native_config: File = sealed(c"provider-model-native-config", &bytes)
            .unwrap()
            .into();
        HeldFiles::retain(
            stock_loader,
            runner,
            tool_loader,
            native_config,
            ObjectDigests {
                stock_loader: a,
                runner: b,
                tool_loader: a,
                native_config: Sha256::digest(bytes).into(),
            },
        )
        .unwrap()
    }
    async fn fixture(
        seal_state: bool,
    ) -> (
        Provider,
        Arc<MockProof>,
        ExecTicket,
        NativeExecContext,
        [u8; 32],
    ) {
        let sockets = NativeExecSocketBindings {
            detlog: NativeExecSocketBinding {
                fd: 203,
                device: 1,
                inode: 2,
            },
            observer: NativeExecSocketBinding {
                fd: 204,
                device: 1,
                inode: 3,
            },
        };
        fixture_with_sockets(seal_state, sockets, sockets).await
    }

    async fn fixture_with_sockets(
        seal_state: bool,
        launch: NativeExecSocketBindings,
        current: NativeExecSocketBindings,
    ) -> (
        Provider,
        Arc<MockProof>,
        ExecTicket,
        NativeExecContext,
        [u8; 32],
    ) {
        let own = model_pidfd(std::process::id()).unwrap();
        let proof = Arc::new(MockProof {
            birth: own,
            live: AtomicBool::new(true),
            socket_objects: [launch.detlog.object(), launch.observer.object()],
        });
        let owner = proof.owner();
        let old_mm = MmId::initial(owner).for_exec(owner);
        let context = NativeExecContext {
            owner,
            old_mm,
            prepared_mm: old_mm.for_exec(owner),
            config_digest: [1; 32],
        };
        let cut = NativeExecNeutralBinding {
            dettid: owner,
            detpid: owner,
            physical_tid: std::process::id() as i32,
            old_mm,
            old_files: FilesId::initial(owner),
            prepared_mm: context.prepared_mm,
            config_digest: context.config_digest,
        };
        let mut registry = NativeExecRegistry::new(proof.clone(), [7; 32]).unwrap();
        let tool = b"provider-tool-model";
        let ticket = registry
            .prepare(
                context,
                PrepareNativeExec {
                    sockets: current,
                    cut,
                    prepared_mm: context.prepared_mm,
                    config_digest: context.config_digest,
                    target_digest: [2; 32],
                    bootstrap_digest: [3; 32],
                    resume_mask: 0x42,
                    snapshot_digest: Sha256::digest(tool).into(),
                    snapshot: tool.to_vec(),
                },
            )
            .unwrap();
        let digest = if seal_state {
            let runtime = b"provider-runtime-model";
            let mut expected = b"HLXSTATE".to_vec();
            expected.extend_from_slice(&2u16.to_le_bytes());
            expected.extend_from_slice(&(tool.len() as u32).to_le_bytes());
            expected.extend_from_slice(&(runtime.len() as u32).to_le_bytes());
            // Independently spell the entire fixed positional identity
            // envelope; do not call the production frame encoder here.
            expected.extend_from_slice(&owner.as_raw().to_le_bytes());
            expected.extend_from_slice(&owner.as_raw().to_le_bytes());
            expected.extend_from_slice(&cut.physical_tid.to_le_bytes());
            for (creator, generation) in [
                (old_mm.creator(), old_mm.generation()),
                (cut.old_files.creator(), cut.old_files.generation()),
                (
                    context.prepared_mm.creator(),
                    context.prepared_mm.generation(),
                ),
            ] {
                expected.extend_from_slice(&creator.as_raw().to_le_bytes());
                expected.extend_from_slice(&generation.to_le_bytes());
            }
            expected.extend_from_slice(&context.config_digest);
            expected.extend_from_slice(tool);
            expected.extend_from_slice(runtime);
            let independently_computed: [u8; 32] = Sha256::digest(&expected).into();
            let returned = registry
                .seal(
                    owner,
                    ticket,
                    context.prepared_mm,
                    Sha256::digest(runtime).into(),
                    runtime,
                )
                .unwrap();
            assert_eq!(returned, independently_computed);
            independently_computed
        } else {
            [0; 32]
        };
        let global =
            Arc::new(detcore::GlobalState::init_global_state(&detcore::Config::default()).await);
        global.install_native_exec_registry(registry).unwrap();
        let fd = model_pidfd(std::process::id()).unwrap();
        let birth = RetainedPeer {
            identity: physical_exit_watch::birth_identity(&fd).unwrap(),
            fd,
            pid: std::process::id() as i32,
        };
        let expected = ProviderBindings {
            config_digest: [1; 32],
            config_schema: *b"0123456789abcdef",
            native_source_digest: [4; 32],
            coordinator: b"/fixture/coordinator.sock".to_vec(),
            detlog_identity: format!(
                "{}:{:016x}:{:016x}",
                launch.detlog.fd, launch.detlog.device, launch.detlog.inode
            ),
            forward_policy: "strict-forward".into(),
            observer_identity: format!(
                "{}:{:016x}:{:016x}",
                launch.observer.fd, launch.observer.device, launch.observer.inode
            ),
        };
        // SAFETY: model-only installer fixture, never connected to any guest
        // exec consumer. Qualification/namespace/watch are not runtime evidence.
        let provider = unsafe {
            Provider::install_model(
                global,
                &birth,
                Arc::new(hermit::in_guest_exits::InGuestExitAdmission::default()),
                held(),
                expected,
            )
        }
        .unwrap();
        (provider, proof, ticket, context, digest)
    }

    #[tokio::test]
    async fn model_provider_sealed_packet_matches_current_relocated_socket_bindings() {
        // Qualification/watch/Prepare authority is modeled by fixture(). The
        // sockets, relocated descriptors, SCM_RIGHTS, seals and packet bytes
        // below are actual; this test never executes a guest or native leaf.
        fn relocate(source: OwnedFd, minimum: i32) -> OwnedFd {
            let raw = unsafe { libc::fcntl(source.as_raw_fd(), libc::F_DUPFD_CLOEXEC, minimum) };
            assert!(raw >= minimum);
            let relocated = unsafe { OwnedFd::from_raw_fd(raw) };
            assert_eq!(
                detcore::detlog::socket_identity(source.as_raw_fd()),
                detcore::detlog::socket_identity(relocated.as_raw_fd())
            );
            drop(source);
            relocated
        }
        fn current_binding(current: &OwnedFd) -> NativeExecSocketBinding {
            let (device, inode) = detcore::detlog::socket_identity(current.as_raw_fd()).unwrap();
            NativeExecSocketBinding {
                fd: current.as_raw_fd(),
                device,
                inode,
            }
        }

        let (detlog, _detlog_peer) = sockets();
        let (observer, _observer_peer) = sockets();
        let detlog = relocate(detlog, 1025);
        let observer = relocate(observer, 1026);
        assert_ne!(detlog.as_raw_fd(), observer.as_raw_fd());
        assert_ne!(
            detcore::detlog::socket_identity(detlog.as_raw_fd()),
            detcore::detlog::socket_identity(observer.as_raw_fd())
        );
        let detlog_identity = detcore::detlog::tool_output_env_value(detlog.as_raw_fd()).unwrap();
        let observer_identity =
            detcore::detlog::tool_output_env_value(observer.as_raw_fd()).unwrap();
        let current = NativeExecSocketBindings {
            detlog: current_binding(&detlog),
            observer: current_binding(&observer),
        };
        // Model only launch/Prepare authority: current facts travel through the
        // real Prepare request and retained registry binding, never a mutation
        // of the bound registry or provider after ticket preparation.
        let launch = NativeExecSocketBindings {
            detlog: NativeExecSocketBinding {
                fd: 204,
                ..current.detlog
            },
            observer: NativeExecSocketBinding {
                fd: 203,
                ..current.observer
            },
        };
        let (provider, _, ticket, context, digest) =
            fixture_with_sockets(true, launch, current).await;

        let (host, client) = sockets();
        send(&client, &request(ticket));
        provider.serve(&host).unwrap();
        let (reply, rights) = read_reply(&client);
        assert_eq!(reply, response(ticket, digest));
        assert_eq!(rights.len(), 5);
        check_seals(rights[3].as_raw_fd()).unwrap();
        let raw = unsafe { libc::fcntl(rights[3].as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
        assert!(raw >= 0);
        let received = unsafe { File::from_raw_fd(raw) };
        let length = received.metadata().unwrap().len() as usize;
        assert!(length <= input::MAX_PACKET_LEN);
        let mut actual = vec![0; length];
        received.read_exact_at(&mut actual, 0).unwrap();

        let entry = provider.model_entry();
        let mut expected = [0; input::MAX_PACKET_LEN];
        let expected_len = input::encode_into(
            input::Packet {
                config_digest: &context.config_digest,
                config_schema: &provider.expected.config_schema,
                native_source_digest: &provider.expected.native_source_digest,
                target_digest: &entry.target_digest,
                bootstrap_digest: &entry.bootstrap_digest,
                image: input::Image::Successor {
                    ticket: &ticket.0,
                    composite_digest: &digest,
                },
                coordinator: &provider.expected.coordinator,
                detlog_identity: &detlog_identity,
                forward_policy: &provider.expected.forward_policy,
                observer_identity: &observer_identity,
            },
            &mut expected,
        )
        .unwrap();
        assert_eq!(
            actual.as_slice(),
            &expected[..expected_len],
            "sealed successor input must name the current relocated socket slots"
        );
    }

    #[tokio::test]
    async fn model_provider_exact_packet_composite_five_roles_cloexec_seals_and_replay() {
        let (provider, _, ticket, context, digest) = fixture(true).await;
        let (host, client) = sockets();
        send(&client, &request(ticket));
        provider.serve(&host).unwrap();
        let (bytes, rights) = read_reply(&client);
        assert_eq!(bytes, response(ticket, digest));
        assert_eq!(rights.len(), 5);
        for fd in &rights {
            assert_ne!(
                unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC,
                0
            );
        }
        for (fd, source) in rights.iter().zip([
            &provider.model_entry().files.stock_loader.file,
            &provider.model_entry().files.runner.file,
            &provider.model_entry().files.tool_loader.file,
        ]) {
            let cloned = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
            assert!(cloned >= 0);
            let received = unsafe { File::from_raw_fd(cloned) };
            assert_eq!(
                Identity::read(&received).unwrap(),
                Identity::read(source).unwrap()
            );
        }
        // Actual transferred aliases must share the original stock-loader
        // OFD, matching SE198/200. This offset test is transport-only; restore
        // the offset and never execute these model fixture objects.
        let offset = unsafe { libc::lseek(rights[0].as_raw_fd(), 0, libc::SEEK_CUR) };
        assert!(offset >= 0);
        assert_eq!(
            unsafe { libc::lseek(rights[0].as_raw_fd(), 17, libc::SEEK_SET) },
            17
        );
        assert_eq!(
            unsafe { libc::lseek(rights[2].as_raw_fd(), 0, libc::SEEK_CUR) },
            17
        );
        assert_eq!(
            unsafe { libc::lseek(rights[0].as_raw_fd(), offset, libc::SEEK_SET) },
            offset
        );
        check_seals(rights[3].as_raw_fd()).unwrap();
        check_seals(rights[4].as_raw_fd()).unwrap();
        let cloned_config = unsafe { libc::fcntl(rights[4].as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
        assert!(cloned_config >= 0);
        let received_config = unsafe { File::from_raw_fd(cloned_config) };
        assert_eq!(
            Identity::read(&received_config).unwrap(),
            Identity::read(&provider.model_entry().files.native_config.file).unwrap()
        );
        verify_native_config(&received_config).unwrap();
        let cloned = unsafe { libc::fcntl(rights[3].as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
        assert!(cloned >= 0);
        let input = unsafe { File::from_raw_fd(cloned) };
        let length = input.metadata().unwrap().len() as usize;
        assert!(length <= input::MAX_PACKET_LEN);
        let mut packet = vec![0u8; length];
        input.read_exact_at(&mut packet, 0).unwrap();
        let decoded = input::decode(&packet).unwrap();
        assert_eq!(
            decoded.image,
            input::Image::Successor {
                ticket: &ticket.0,
                composite_digest: &digest,
            }
        );
        assert_eq!(decoded.config_digest, &provider.expected.config_digest);
        assert_eq!(
            decoded.native_source_digest,
            &provider.expected.native_source_digest
        );
        assert_eq!(
            decoded.observer_identity,
            provider.expected.observer_identity
        );
        let mut registry = provider.registry().unwrap().lock().unwrap();
        registry
            .issue(context.owner, ticket, context.prepared_mm, digest)
            .unwrap();
        drop(registry);
        let (again, client_again) = sockets();
        send(&client_again, &request(ticket));
        assert!(provider.serve(&again).is_err());
        no_response(&client_again);
        assert_eq!(
            provider.registry().unwrap().lock().unwrap().stage(ticket),
            Some(ExecStage::Issued)
        );
    }

    #[tokio::test]
    async fn model_provider_preseal_and_death_transfer_nothing() {
        for dead in [false, true] {
            let (provider, proof, ticket, _, _) = fixture(dead).await;
            if dead {
                proof.live.store(false, Ordering::SeqCst);
            }
            let (host, client) = sockets();
            send(&client, &request(ticket));
            assert!(provider.serve(&host).is_err());
            no_response(&client);
            assert_eq!(
                provider.registry().unwrap().lock().unwrap().stage(ticket),
                Some(if dead {
                    ExecStage::Dead
                } else {
                    ExecStage::Prepared
                })
            );
        }
    }

    struct Child(std::process::Child);
    impl Drop for Child {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    #[tokio::test]
    async fn model_provider_actual_peer_pidfd_wrong_retained_birth_refuses_without_consuming() {
        let (mut provider, _, ticket, _, _) = fixture(true).await;
        let child = Child(
            std::process::Command::new("/bin/sleep")
                .arg("10")
                .spawn()
                .unwrap(),
        );
        let fd = model_pidfd(child.0.id()).unwrap();
        provider.birth = RetainedPeer {
            identity: physical_exit_watch::birth_identity(&fd).unwrap(),
            fd,
            pid: child.0.id() as i32,
        };
        let (host, client) = sockets();
        send(&client, &request(ticket));
        // SO_PEERPIDFD belongs to this real parent, not the distinct held child.
        assert!(provider.serve(&host).is_err());
        no_response(&client);
        assert_eq!(
            provider.registry().unwrap().lock().unwrap().stage(ticket),
            Some(ExecStage::Prepared)
        );
    }

    #[tokio::test]
    async fn model_provider_real_dead_pidfd_refuses_and_watch_death_tombstone_stays_dead() {
        let (mut provider, _, ticket, context, digest) = fixture(true).await;
        let mut child = Child(
            std::process::Command::new("/bin/sleep")
                .arg("10")
                .spawn()
                .unwrap(),
        );
        let fd = model_pidfd(child.0.id()).unwrap();
        provider.birth = RetainedPeer {
            identity: physical_exit_watch::birth_identity(&fd).unwrap(),
            fd,
            pid: child.0.id() as i32,
        };
        child.0.kill().unwrap();
        child.0.wait().unwrap();
        assert!(alive(&provider.birth.fd).is_err());
        // Model the registry callback only after genuine pidfd death is observed;
        // this test does not create a watched native exec admission.
        provider.registry().unwrap().lock().unwrap().died();
        let (host, client) = sockets();
        send(&client, &request(ticket));
        assert!(provider.serve(&host).is_err());
        no_response(&client);
        let mut registry = provider.registry().unwrap().lock().unwrap();
        assert_eq!(registry.stage(ticket), Some(ExecStage::Dead));
        assert_eq!(
            registry.issue(context.owner, ticket, context.prepared_mm, digest),
            Err(ExecControlError::Dead)
        );
    }

    #[tokio::test]
    async fn model_provider_closed_peer_send_failure_is_terminal_no_issue_or_retry() {
        let (provider, _, ticket, context, digest) = fixture(true).await;
        let (host, client) = sockets();
        send(&client, &request(ticket));
        assert_eq!(
            unsafe { libc::shutdown(client.as_raw_fd(), libc::SHUT_RD) },
            0
        );
        assert!(provider.serve(&host).is_err());
        let mut registry = provider.registry().unwrap().lock().unwrap();
        assert_eq!(registry.stage(ticket), Some(ExecStage::Failed));
        assert_eq!(
            registry.issue(context.owner, ticket, context.prepared_mm, digest),
            Err(ExecControlError::Dead)
        );
        drop(registry);
        let (again, other) = sockets();
        send(&other, &request(ticket));
        assert!(provider.serve(&again).is_err());
        no_response(&other);
    }

    #[tokio::test]
    async fn model_provider_socket_object_disagreement_after_consume_poisoned() {
        for mutation in 0..5 {
            let (mut provider, _, ticket, context, digest) = fixture(true).await;
            let held = provider.expected.sockets().unwrap();
            let mut changed = held;
            match mutation {
                0 => changed.detlog.device ^= 1,
                1 => changed.detlog.inode ^= 1,
                2 => changed.observer.device ^= 1,
                3 => changed.observer.inode ^= 1,
                4 => std::mem::swap(&mut changed.detlog, &mut changed.observer),
                _ => unreachable!(),
            }
            // Mutate only the independent provider expectation, never the
            // already admitted transaction. Both checks must agree on roles.
            provider.expected.detlog_identity = format!(
                "{}:{:016x}:{:016x}",
                changed.detlog.fd, changed.detlog.device, changed.detlog.inode
            );
            provider.expected.observer_identity = format!(
                "{}:{:016x}:{:016x}",
                changed.observer.fd, changed.observer.device, changed.observer.inode
            );
            let (host, client) = sockets();
            send(&client, &request(ticket));
            assert!(provider.serve(&host).is_err(), "mutation {mutation}");
            no_response(&client);
            let mut registry = provider.registry().unwrap().lock().unwrap();
            assert_eq!(registry.stage(ticket), Some(ExecStage::Failed));
            assert_eq!(
                registry.issue(context.owner, ticket, context.prepared_mm, digest),
                Err(ExecControlError::Dead)
            );
        }
    }

    #[tokio::test]
    async fn model_provider_binding_disagreement_after_consume_poisoned() {
        let (mut provider, _, ticket, context, digest) = fixture(true).await;
        provider.model_entry_mut().target_digest[31] ^= 1;
        let (host, client) = sockets();
        send(&client, &request(ticket));
        assert!(provider.serve(&host).is_err());
        no_response(&client);
        let mut registry = provider.registry().unwrap().lock().unwrap();
        assert_eq!(registry.stage(ticket), Some(ExecStage::Failed));
        assert_eq!(
            registry.issue(context.owner, ticket, context.prepared_mm, digest),
            Err(ExecControlError::Dead)
        );
    }

    #[tokio::test]
    async fn model_provider_malformed_truncated_and_ancillary_requests_are_terminal() {
        for bad in 0..5 {
            let (provider, _, ticket, _, _) = fixture(true).await;
            let (host, client) = sockets();
            let mut bytes = request(ticket).to_vec();
            match bad {
                0 => {
                    bytes[0] ^= 1;
                    send(&client, &bytes);
                }
                1 => {
                    bytes.pop();
                    send(&client, &bytes);
                }
                2 => {
                    bytes.push(0);
                    send(&client, &bytes);
                }
                3 => {
                    bytes[47] = 1;
                    send(&client, &bytes);
                }
                4 => {
                    let source = provider.model_entry().files.native_config.file.as_raw_fd();
                    // Genuine ancillary descriptors on an oversized frame. The
                    // receiver must close installed rights and reject framing.
                    send_objects(&client, &response(ticket, [0; 32]), [source; 5]).unwrap();
                }
                _ => unreachable!(),
            }
            assert!(provider.serve(&host).is_err());
            no_response(&client);
            assert_eq!(
                provider.registry().unwrap().lock().unwrap().stage(ticket),
                Some(ExecStage::Failed)
            );
        }
    }

    #[test]
    fn model_held_sources_reject_changed_digest_unsealed_config_and_bad_native_paths() {
        let (file, mut digest) = source();
        digest[31] ^= 1;
        assert!(HeldObject::retain(file, digest, false).is_err());
        let (file, digest) = source();
        assert!(HeldObject::retain(file, digest, true).is_err());
        let mut bytes = [0u8; NATIVE_CONFIG_LEN];
        bytes[..8].copy_from_slice(&1u64.to_le_bytes());
        bytes[8..16].copy_from_slice(&(NATIVE_CONFIG_LEN as u64).to_le_bytes());
        bytes[16..23].copy_from_slice(b"/a:$bad");
        bytes[528..532].copy_from_slice(b"/lib");
        let file: File = sealed(c"provider-model-bad-config", &bytes).unwrap().into();
        assert!(verify_native_config(&file).is_err());
    }
}
