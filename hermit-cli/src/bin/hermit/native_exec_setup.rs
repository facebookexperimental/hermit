//! Actual Command-child adapter for the existing common declared root.
//! No namespace, RPC server, target-vector rewrite or admission owner lives here.

use std::io;
use std::os::fd::AsRawFd;
use std::os::fd::BorrowedFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;

use reverie_inguest::launcher::native::NativeGuestSetup;
use reverie_inguest::launcher::native::NativeRootObservation;
use reverie_inguest::launcher::native_frontend::NativeFrontend;

use super::container::declared_inputs::GuestDeclaredRootSetup;
use super::container::declared_inputs::MaterializedDeclaredRoot;

pub(super) const DETLOG_FD: i32 = 204;
const FIRST_ROLE: i32 = 198;
const ROLES: usize = 7;
const SEALS: i32 = libc::F_SEAL_SEAL
    | libc::F_SEAL_WRITE
    | libc::F_SEAL_GROW
    | libc::F_SEAL_SHRINK
    | libc::F_SEAL_FUTURE_WRITE
    | 0x0020;

#[derive(Clone, Copy)]
struct Identity {
    device: u64,
    inode: u64,
    mode: u32,
}
struct Setup {
    root: GuestDeclaredRootSetup,
    entered: Option<MaterializedDeclaredRoot>,
    frontend: NativeFrontend,
    leases: [OwnedFd; ROLES],
    identities: [Identity; ROLES],
    entered_once: bool,
}

/// Reserve the exact bootstrap/socket descriptors before coordinator workers.
/// `roles` are stock loader, runner, the same stock-loader OFD, sealed initial
/// packet and native configuration; sockets remain separate actual endpoints.
///
/// # Safety
/// Caller owns the genuine qualified sealed artifact/root plan and retained
/// endpoint owners, and is still in single-threaded common-container setup.
/// Every role is independently checked against that owner, not a request or
/// these scalar observations. No other launcher uses 198..=204 until this
/// physical child's reap. The two stock-loader entries must be the same OFD.
/// Guest code has not run, and all surrounding Command setup completes before
/// this adapter installs its sole filter. Parent pins outlive physical reap.
pub(super) unsafe fn prepare(
    root: GuestDeclaredRootSetup,
    frontend: NativeFrontend,
    roles: [BorrowedFd<'_>; 5],
    observer: BorrowedFd<'_>,
    detlog: BorrowedFd<'_>,
) -> io::Result<Box<dyn NativeGuestSetup>> {
    let descriptors = [
        roles[0], roles[1], roles[2], roles[3], roles[4], observer, detlog,
    ];
    let mut identities = [Identity {
        device: 0,
        inode: 0,
        mode: 0,
    }; ROLES];
    let mut leases = Vec::with_capacity(ROLES);
    for (index, descriptor) in descriptors.iter().enumerate() {
        let identity = identify(descriptor.as_raw_fd())?;
        if index < 5 {
            let seals = unsafe { libc::fcntl(descriptor.as_raw_fd(), libc::F_GET_SEALS) };
            if identity.mode & libc::S_IFMT != libc::S_IFREG || seals < 0 || seals & SEALS != SEALS
            {
                return Err(refused());
            }
        } else {
            let mut kind = 0i32;
            let mut length = std::mem::size_of::<i32>() as libc::socklen_t;
            if identity.mode & libc::S_IFMT != libc::S_IFSOCK
                || unsafe {
                    libc::getsockopt(
                        descriptor.as_raw_fd(),
                        libc::SOL_SOCKET,
                        libc::SO_TYPE,
                        (&raw mut kind).cast(),
                        &mut length,
                    )
                } != 0
                || length as usize != std::mem::size_of::<i32>()
                || kind != libc::SOCK_SEQPACKET
            {
                return Err(refused());
            }
        }
        let wanted = FIRST_ROLE + index as i32;
        // F_DUPFD does not overwrite unknown work. A concurrently occupied
        // reserved slot is a refusal; already-made leases close on error.
        let actual = unsafe { libc::fcntl(descriptor.as_raw_fd(), libc::F_DUPFD_CLOEXEC, wanted) };
        if actual < 0 {
            return Err(io::Error::last_os_error());
        }
        let lease = unsafe { OwnedFd::from_raw_fd(actual) };
        if actual != wanted {
            return Err(refused());
        }
        identities[index] = identity;
        leases.push(lease);
    }
    if identities[0].device != identities[2].device
        || identities[0].inode != identities[2].inode
        || (identities[5].device, identities[5].inode)
            == (identities[6].device, identities[6].inode)
    {
        return Err(refused());
    }
    let leases: [OwnedFd; ROLES] = leases.try_into().map_err(|_| refused())?;
    Ok(Box::new(Setup {
        root,
        entered: None,
        frontend,
        leases,
        identities,
        entered_once: false,
    }))
}

// SAFETY: prepare retains one actual common-root token, independently qualified
// preallocated frontend and inherited FD leases before workers. Every child
// operation below uses fixed scalar/stack storage. No native proof is invented.
unsafe impl NativeGuestSetup for Setup {
    unsafe fn bind_and_enter(&mut self) -> io::Result<NativeRootObservation> {
        if self.entered_once {
            return Err(refused());
        }
        self.entered_once = true;
        let entered = unsafe { self.root.bind_and_enter()? };
        if (FIRST_ROLE..=DETLOG_FD).contains(&entered.descriptor().as_raw_fd()) {
            return Err(refused());
        }
        for (index, lease) in self.leases.iter().enumerate() {
            let found = identify(lease.as_raw_fd())?;
            let expected = self.identities[index];
            if (found.device, found.inode, found.mode)
                != (expected.device, expected.inode, expected.mode)
                || lease.as_raw_fd() != FIRST_ROLE + index as i32
            {
                return Err(refused());
            }
            // All seven actual objects survive the original kernel exec.
            // Bootstrap consumes 198..202; the genuine initializer reserves
            // and protects the distinct observer/DETLOG roles before guest work.
            if unsafe { libc::fcntl(lease.as_raw_fd(), libc::F_SETFD, 0) } != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        unsafe { self.frontend.prepare_child()? };
        let namespace = entered.namespace();
        let observation = NativeRootObservation {
            pid: entered.visible_pid(),
            tid: unsafe { libc::syscall(libc::SYS_gettid) } as i32,
            device: entered.device(),
            inode: entered.inode(),
            mount_id: entered.mount_id(),
            namespace_device: namespace.0,
            namespace_inode: namespace.1,
        };
        self.entered = Some(entered);
        Ok(observation)
    }

    fn root_descriptor(&self) -> BorrowedFd<'_> {
        self.entered
            .as_ref()
            .expect("entered root before transfer")
            .descriptor()
    }

    unsafe fn install_frontend(&mut self) -> io::Result<()> {
        // Only a file descriptor is closed here, before irreversible filter
        // publication. Heap-backed adapter/role owners are never dropped in
        // this child: the next operation is the original terminal exec entry.
        let entered = self.entered.take().ok_or_else(refused)?;
        drop(entered);
        unsafe { self.frontend.install_child() }
    }
}

fn identify(fd: i32) -> io::Result<Identity> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::zeroed();
    if unsafe { libc::fstat(fd, stat.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let stat = unsafe { stat.assume_init() };
    Ok(Identity {
        device: stat.st_dev,
        inode: stat.st_ino,
        mode: stat.st_mode,
    })
}
fn refused() -> io::Error {
    io::Error::from_raw_os_error(libc::EPERM)
}
