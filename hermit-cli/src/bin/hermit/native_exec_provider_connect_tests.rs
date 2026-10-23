// These controls include the actual guest provider. The protocol peer and its
// authority are modeled; socket IO, seals, descriptor identities and cleanup
// are real. They do not install a native control or manufacture exec admission.

use std::fs::File;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::thread;

use reverie::process::Container;
use reverie::process::Mount;
use reverie::process::MountFlags;
use reverie_inguest::guest::native_exec_fds::FdIdentity;
use sha2::Digest;
use sha2::Sha256;

use super::native_exec_guest_provider as provider;
use super::native_exec_inputs::InputRole;
use super::native_exec_inputs::seal_bytes;

fn isolated(body: fn()) {
    // No host pathname is unlinked or bound. The production literal resolves
    // only inside this fresh private /run mount.
    Container::new()
        .map_root()
        .mount(Mount::new("/").rprivate())
        .mount(
            Mount::tmpfs("/run")
                .flags(MountFlags::MS_NOSUID | MountFlags::MS_NODEV | MountFlags::MS_NOEXEC),
        )
        .run(body)
        .expect("isolated provider socket control failed");
}

fn listener(kind: i32) -> OwnedFd {
    let raw = unsafe {
        libc::socket(
            libc::AF_UNIX,
            kind | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            0,
        )
    };
    assert!(raw >= 0, "socket: {}", std::io::Error::last_os_error());
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    address.sun_family = libc::AF_UNIX as _;
    let path = b"/run/provision.sock\0";
    for (to, from) in address.sun_path.iter_mut().zip(path) {
        *to = *from as _;
    }
    assert_eq!(
        unsafe {
            libc::bind(
                fd.as_raw_fd(),
                (&raw const address).cast(),
                (2 + path.len()) as _,
            )
        },
        0,
        "bind: {}",
        std::io::Error::last_os_error()
    );
    assert_eq!(unsafe { libc::listen(fd.as_raw_fd(), 1) }, 0);
    fd
}

fn readable(fd: i32, milliseconds: i32) -> i32 {
    let mut poll = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let result = unsafe { libc::poll(&mut poll, 1, milliseconds) };
    assert!(result >= 0, "poll: {}", std::io::Error::last_os_error());
    if result != 0 {
        assert_eq!(poll.revents, libc::POLLIN, "unexpected socket event");
    }
    result
}

fn no_connection(listener: &OwnedFd) {
    let accepted = unsafe {
        libc::accept4(
            listener.as_raw_fd(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            libc::SOCK_CLOEXEC,
        )
    };
    if accepted >= 0 {
        drop(unsafe { OwnedFd::from_raw_fd(accepted) });
        panic!("provider connected before an actual exchange");
    }
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::EAGAIN)
    );
}

fn client() -> OwnedFd {
    let fd = unsafe { OwnedFd::from_raw_fd(provider::create_socket().unwrap()) };
    // The shared runner bounds the process; this also bounds the actual
    // blocking guest receive if the test peer fails before replying.
    let timeout = libc::timeval {
        tv_sec: 3,
        tv_usec: 0,
    };
    assert_eq!(
        unsafe {
            libc::setsockopt(
                fd.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_RCVTIMEO,
                (&raw const timeout).cast(),
                std::mem::size_of_val(&timeout) as _,
            )
        },
        0
    );
    fd
}

fn sources() -> [File; 5] {
    std::array::from_fn(|index| seal_bytes(&[index as u8 + 1; 32], InputRole::Data).unwrap())
}

fn frames() -> ([u8; 48], [u8; 80]) {
    let request = provider::request(&[17; 32]);
    let response = provider::expected_response(&[17; 32], &[29; 32]);
    assert_eq!(&request[..12], b"HLPROV01\0\x01\0\x30");
    assert_eq!(&request[12..44], &[17; 32]);
    assert_eq!(&response[..14], b"HLPRACK1\0\x01\0\x50\0\x05");
    (request, response)
}

fn peer(
    listener: OwnedFd,
    rights: [i32; 5],
    request: [u8; 48],
    response: [u8; 80],
) -> thread::JoinHandle<(OwnedFd, OwnedFd)> {
    thread::spawn(move || {
        assert_eq!(
            readable(listener.as_raw_fd(), 3000),
            1,
            "no exchange connection"
        );
        let raw = unsafe {
            libc::accept4(
                listener.as_raw_fd(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                libc::SOCK_CLOEXEC,
            )
        };
        assert!(raw >= 0, "accept: {}", std::io::Error::last_os_error());
        let socket = unsafe { OwnedFd::from_raw_fd(raw) };
        assert_eq!(readable(socket.as_raw_fd(), 3000), 1, "no provider request");
        let mut actual = [0; 48];
        assert_eq!(
            unsafe {
                libc::recv(
                    socket.as_raw_fd(),
                    actual.as_mut_ptr().cast(),
                    actual.len(),
                    libc::MSG_TRUNC,
                )
            },
            48
        );
        assert_eq!(actual, request);
        let mut control = [0u64; 5];
        assert_eq!(
            unsafe { libc::CMSG_SPACE(std::mem::size_of_val(&rights) as _) } as usize,
            std::mem::size_of_val(&control)
        );
        let mut iov = libc::iovec {
            iov_base: response.as_ptr().cast_mut().cast(),
            iov_len: response.len(),
        };
        let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
        message.msg_iov = &mut iov;
        message.msg_iovlen = 1;
        message.msg_control = control.as_mut_ptr().cast();
        message.msg_controllen = std::mem::size_of_val(&control);
        let header = unsafe { libc::CMSG_FIRSTHDR(&message) };
        assert!(!header.is_null());
        unsafe {
            (*header).cmsg_level = libc::SOL_SOCKET;
            (*header).cmsg_type = libc::SCM_RIGHTS;
            (*header).cmsg_len = libc::CMSG_LEN(std::mem::size_of_val(&rights) as _) as usize;
            std::ptr::copy_nonoverlapping(
                rights.as_ptr(),
                libc::CMSG_DATA(header).cast(),
                rights.len(),
            );
        }
        assert_eq!(
            unsafe {
                libc::sendmsg(
                    socket.as_raw_fd(),
                    &message,
                    libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL,
                )
            },
            80
        );
        (listener, socket)
    })
}

fn exchange(
    fd: &OwnedFd,
    request: &[u8; 48],
    response: &[u8; 80],
) -> Result<provider::Received, provider::Error> {
    // Only the physical transport is exercised. The peer above models the
    // preencoded ticket/object authority, not a real admitted transaction.
    unsafe { provider::exchange(fd.as_raw_fd(), request, response) }
}

fn aliases(source: &File) -> usize {
    let expected = FdIdentity::read(source.as_raw_fd()).unwrap();
    std::fs::read_dir("/proc/self/fd")
        .unwrap()
        .map(|entry| {
            let name = entry.unwrap().file_name();
            let fd: i32 = name.to_str().unwrap().parse().unwrap();
            let actual = FdIdentity::read(fd).unwrap();
            usize::from((actual.device, actual.inode) == (expected.device, expected.inode))
        })
        .sum()
}

#[test]
fn actual_provider_socket_remains_unconnected_past_request_deadline_and_close() {
    isolated(|| {
        let listener = listener(libc::SOCK_SEQPACKET);
        let client = client();
        no_connection(&listener);
        // The unchanged host accepted-request deadline is five seconds.
        assert_eq!(readable(listener.as_raw_fd(), 6000), 0);
        no_connection(&listener);
        drop(client);
        no_connection(&listener);
    });
}

#[test]
fn actual_provider_first_exchange_preserves_socket_and_receives_five_sealed_roles() {
    isolated(|| {
        let listener = listener(libc::SOCK_SEQPACKET);
        let sources = sources();
        let (request, response) = frames();
        let client = client();
        let before = FdIdentity::read(client.as_raw_fd()).unwrap();
        assert_eq!(before.descriptor_flags, libc::FD_CLOEXEC);
        let server = peer(
            listener,
            sources.each_ref().map(AsRawFd::as_raw_fd),
            request,
            response,
        );
        let received = exchange(&client, &request, &response);
        let (listener, _connection) = server.join().unwrap();
        let received = received.unwrap();
        assert_eq!(FdIdentity::read(client.as_raw_fd()).unwrap(), before);
        for (index, fd) in received.fds().unwrap().into_iter().enumerate() {
            let original = FdIdentity::read(sources[index].as_raw_fd()).unwrap();
            let actual = FdIdentity::read(fd).unwrap();
            assert_eq!(
                (actual.device, actual.inode),
                (original.device, original.inode)
            );
            assert_eq!(actual.descriptor_flags, libc::FD_CLOEXEC);
            let bytes = [index as u8 + 1; 32];
            provider::Object {
                device: original.device,
                inode: original.inode,
                size: bytes.len() as u64,
                digest: Sha256::digest(bytes).into(),
            }
            .verify(fd, &mut [0; 16384])
            .unwrap();
            provider::verify_input(fd, &bytes, &mut [0; 4096]).unwrap();
        }
        received.close().unwrap();
        assert!(
            sources.iter().all(|source| aliases(source) == 1),
            "successful receive cleanup leaked SCM_RIGHTS"
        );
        no_connection(&listener);
    });
}

#[test]
fn actual_provider_connect_failure_preserves_original_socket() {
    isolated(|| {
        // No listener exists at the production literal. Creation must not
        // depend on a host accepted connection or its five-second deadline.
        let client = client();
        let before = FdIdentity::read(client.as_raw_fd()).unwrap();
        let (request, response) = frames();
        assert!(exchange(&client, &request, &response).is_err());
        assert_eq!(FdIdentity::read(client.as_raw_fd()).unwrap(), before);
    });
    isolated(|| {
        let listener = listener(libc::SOCK_STREAM);
        let client = client();
        let before = FdIdentity::read(client.as_raw_fd()).unwrap();
        let (request, response) = frames();
        assert!(exchange(&client, &request, &response).is_err());
        assert_eq!(FdIdentity::read(client.as_raw_fd()).unwrap(), before);
        no_connection(&listener);
    });
}

#[test]
fn actual_provider_second_exchange_refuses_without_second_request() {
    isolated(|| {
        let listener = listener(libc::SOCK_SEQPACKET);
        let sources = sources();
        let (request, response) = frames();
        let client = client();
        let before = FdIdentity::read(client.as_raw_fd()).unwrap();
        let server = peer(
            listener,
            sources.each_ref().map(AsRawFd::as_raw_fd),
            request,
            response,
        );
        let received = exchange(&client, &request, &response);
        let (listener, connection) = server.join().unwrap();
        received.unwrap().close().unwrap();
        // Keep the same accepted peer alive: a second send followed by a
        // receive error must not masquerade as refusing before the request.
        assert!(exchange(&client, &request, &response).is_err());
        assert_eq!(
            readable(connection.as_raw_fd(), 0),
            0,
            "second request reached the peer"
        );
        assert_eq!(FdIdentity::read(client.as_raw_fd()).unwrap(), before);
        no_connection(&listener);
    });
}

#[test]
fn actual_provider_malformed_reply_closes_received_descriptors() {
    isolated(|| {
        let listener = listener(libc::SOCK_SEQPACKET);
        let sources = sources();
        assert!(sources.iter().all(|source| aliases(source) == 1));
        let (request, response) = frames();
        let mut malformed = response;
        malformed[0] ^= 1;
        let client = client();
        let before = FdIdentity::read(client.as_raw_fd()).unwrap();
        let server = peer(
            listener,
            sources.each_ref().map(AsRawFd::as_raw_fd),
            request,
            malformed,
        );
        let result = exchange(&client, &request, &response);
        let (listener, _connection) = server.join().unwrap();
        assert!(result.is_err());
        assert!(
            sources.iter().all(|source| aliases(source) == 1),
            "received SCM_RIGHTS leaked"
        );
        assert_eq!(FdIdentity::read(client.as_raw_fd()).unwrap(), before);
        no_connection(&listener);
    });
}
