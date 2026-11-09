/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The TCP port a gdbserver serves its one gdb client on, bound in the host's
//! network namespace.
//!
//! The gdbserver runs in the tracer, inside the container. A port the tracer
//! bound itself would be in the guest's network namespace, which a gdb client
//! on the host cannot reach while the guest has a private network
//! (`--network=local`). So the CLI binds the port before it makes the
//! container (`GdbPort::bind`): a socket stays in the network namespace it
//! was made in, and the tracer inherits it at the clone. Just before the
//! tracer spawns the guest it listens on that socket (`connection`), and a
//! relay accepts the one client and passes its bytes to the gdbserver, which
//! listens on an abstract Unix socket in the tracer's own namespace. The guest
//! keeps the network it was given, and a host gdb client reaches the port
//! either way.
//!
//! The relay is a task on the tracer's Tokio runtime, not a thread: a thread
//! in the tracer would take a PID in the container's PID namespace and shift
//! every guest PID after it.

use std::ffi::OsString;
use std::io;
use std::os::fd::AsRawFd;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStringExt;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use nix::sys::socket::AddressFamily;
use nix::sys::socket::Backlog;
use nix::sys::socket::Shutdown;
use nix::sys::socket::SockFlag;
use nix::sys::socket::SockType;
use nix::sys::socket::SockaddrIn;
use nix::sys::socket::sockopt::ReuseAddr;
use reverie_ptrace::GdbConnection;
use tokio::net::TcpListener;
use tokio::net::TcpStream;
use tokio::net::UnixStream;

use crate::Context;
use crate::Error;

/// How often the relay tries the gdbserver's socket until the gdbserver
/// listens on it, which it does once the guest is spawned.
const GDBSERVER_RETRY: Duration = Duration::from_millis(5);

/// A loopback TCP port bound, not yet listening, in the network namespace of
/// the process that bound it, for a gdbserver to serve its client on.
#[derive(Debug)]
pub struct GdbPort {
    socket: OwnedFd,
    port: u16,
}

/// The port [`GdbPort::serve_next_gdbserver`] gave this process.
static NEXT_GDBSERVER_PORT: Mutex<Option<GdbPort>> = Mutex::new(None);

impl GdbPort {
    /// Binds `127.0.0.1:PORT` in this process's network namespace; port 0
    /// lets the system pick a free one, which [`GdbPort::port`] then names.
    /// The socket is close-on-exec, so no guest inherits it. It listens only
    /// when the tracer is about to spawn the guest (`connection`), so a
    /// client finds the port listening once the gdbserver is moments from
    /// taking it, as when the gdbserver bound the port itself.
    pub fn bind(port: u16) -> Result<Self, Error> {
        let socket = nix::sys::socket::socket(
            AddressFamily::Inet,
            SockType::Stream,
            SockFlag::SOCK_CLOEXEC,
            None,
        )
        .context("Failed to create the gdbserver's TCP socket")?;
        // As std's TcpListener::bind, which the gdbserver used: a port whose
        // last session is still in TIME_WAIT can be bound again at once.
        nix::sys::socket::setsockopt(&socket, ReuseAddr, &true)
            .context("Failed to set SO_REUSEADDR on the gdbserver's TCP socket")?;
        nix::sys::socket::bind(socket.as_raw_fd(), &SockaddrIn::new(127, 0, 0, 1, port))
            .with_context(|| format!("Failed to bind 127.0.0.1:{port} for the gdbserver"))?;
        let port = nix::sys::socket::getsockname::<SockaddrIn>(socket.as_raw_fd())
            .context("Failed to read the gdbserver's bound port")?
            .port();
        Ok(Self { socket, port })
    }

    /// The bound port.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Makes this the port that the next gdbserver this process starts
    /// serves its client on. Call it in the process that starts the tracer,
    /// the container's, after the port was bound outside the container.
    pub fn serve_next_gdbserver(self) {
        *NEXT_GDBSERVER_PORT
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(self);
    }

    /// Listens on this port and starts the relay from it to the gdbserver on
    /// the current Tokio runtime. Returns the abstract Unix socket the
    /// gdbserver must listen on, or the error that left the port unusable,
    /// before any guest exists.
    fn relay(self) -> Result<GdbConnection, Error> {
        // With SO_REUSEADDR, another socket may have bound the port too and
        // listened first; then this fails, and the run with it.
        nix::sys::socket::listen(&self.socket, Backlog::new(1)?).with_context(|| {
            format!(
                "The gdbserver cannot listen on 127.0.0.1:{}, which another process took after \
                 Hermit bound it; pass another --gdbserver-port",
                self.port
            )
        })?;
        // Unique wherever the namespace is shared: this process holds the
        // listening port.
        let mut path = vec![0];
        path.extend(format!("hermit-gdbserver-{}", self.port).into_bytes());
        let path = PathBuf::from(OsString::from_vec(path));
        let listener = std::net::TcpListener::from(self.socket);
        listener.set_nonblocking(true)?;
        let listener = TcpListener::from_std(listener)?;
        tokio::spawn(serve_one_client(listener, self.port, path.clone()));
        Ok(GdbConnection::Path(path))
    }
}

/// Where a tracer's gdbserver takes its client, for a tracer configured to
/// serve gdb on PORT: through a relay from the port
/// [`GdbPort::serve_next_gdbserver`] gave this process, if one did
/// ([`GdbPort::relay`]), or else on PORT, bound by the gdbserver in this
/// process's network namespace. Only library callers that bound no port take
/// the second way; a host gdb client reaches it only where that namespace is
/// the host's.
///
/// Call it on the tracer's Tokio runtime, which runs the relay.
pub(crate) fn connection(port: u16) -> Result<GdbConnection, Error> {
    let served = NEXT_GDBSERVER_PORT
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take();
    match served {
        Some(bound) => bound.relay(),
        None => Ok(port.into()),
    }
}

/// Relays the one client LISTENER accepts to the gdbserver on its abstract
/// Unix socket GDBSERVER, until either side ends the session.
async fn serve_one_client(listener: TcpListener, port: u16, gdbserver: PathBuf) {
    let (mut client, mut server) = match accept_client(listener, &gdbserver).await {
        Ok(connected) => connected,
        Err(error) => {
            tracing::error!("The gdbserver relay on 127.0.0.1:{port} failed: {error:#}");
            return;
        }
    };
    // A session gdb ends by killing the guest can end with a reset; only the
    // session is over, as when the client was connected directly.
    if let Err(error) = tokio::io::copy_bidirectional(&mut client, &mut server).await {
        tracing::debug!("The gdbserver session on 127.0.0.1:{port} ended: {error}");
    }
}

/// Connects to the gdbserver once it listens, then accepts one client. A
/// client that connects first waits in the kernel's accept queue meanwhile.
async fn accept_client(
    listener: TcpListener,
    gdbserver: &Path,
) -> Result<(TcpStream, UnixStream), Error> {
    let server = loop {
        match UnixStream::connect(gdbserver).await {
            Ok(server) => break server,
            Err(error) if error.kind() == io::ErrorKind::ConnectionRefused => {
                tokio::time::sleep(GDBSERVER_RETRY).await;
            }
            Err(error) => return Err(error).context("Failed to connect to the gdbserver"),
        }
    };
    let (client, address) = listener.accept().await?;
    tracing::info!("Relaying gdb client {address} to the gdbserver");
    // The gdbserver serves one client. The process that bound the port keeps
    // its descriptor of this socket until the run ends, so dropping this one
    // would leave the port listening for clients nothing accepts. Shutting it
    // down for reading stops it listening for every holder, and refuses later
    // clients: Linux's inet_shutdown disconnects a TCP_LISTEN socket on
    // SHUT_RD (net/ipv4/af_inet.c:930-935 at v6.16), and tcp_disconnect stops
    // the listen (net/ipv4/tcp.c:3339-3340).
    if let Err(error) = nix::sys::socket::shutdown(listener.as_raw_fd(), Shutdown::Read) {
        tracing::warn!(
            "The gdbserver port still listens after its one client connected; a later client \
             will wait unanswered: {error}"
        );
    }
    Ok((client, server))
}

#[cfg(test)]
mod tests {
    use std::io::Read;
    use std::io::Write;

    use super::*;

    /// Takes the one client of the gdbserver's abstract socket `path` and
    /// echoes what it sends, upper-cased, until it closes.
    fn fake_gdbserver(path: &Path) -> tokio::task::JoinHandle<()> {
        use tokio::io::AsyncReadExt;
        use tokio::io::AsyncWriteExt;

        let listener = tokio::net::UnixListener::bind(path).unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            drop(listener);
            let mut buffer = [0; 64];
            loop {
                let read = stream.read(&mut buffer).await.unwrap();
                if read == 0 {
                    break;
                }
                stream
                    .write_all(&buffer[..read].to_ascii_uppercase())
                    .await
                    .unwrap();
            }
        })
    }

    fn listening(port: u16) -> bool {
        let hex = format!(":{port:04X} ");
        std::fs::read_to_string("/proc/net/tcp")
            .unwrap()
            .lines()
            .any(|line| line.contains(&hex) && line.split_whitespace().nth(3) == Some("0A"))
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    #[test]
    fn a_bound_port_is_closed_on_exec_and_not_yet_listening() {
        let port = GdbPort::bind(0).unwrap();
        assert_ne!(port.port(), 0);
        let flags = nix::fcntl::fcntl(&port.socket, nix::fcntl::FcntlArg::F_GETFD).unwrap();
        assert_ne!(flags & libc::FD_CLOEXEC, 0);
        // Bound, not listening: a client is refused until the tracer is about
        // to spawn the guest, as when the gdbserver bound the port itself.
        assert!(!listening(port.port()));
        let error = std::net::TcpStream::connect(("127.0.0.1", port.port())).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::ConnectionRefused);
    }

    #[test]
    fn a_port_in_use_is_refused_naming_it() {
        let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = taken.local_addr().unwrap().port();
        let error = format!("{:#}", GdbPort::bind(port).unwrap_err());
        assert!(
            error.contains(&format!("127.0.0.1:{port}")) && error.contains("EADDRINUSE"),
            "{error}"
        );
    }

    /// SO_REUSEADDR lets another socket bind the port while it does not
    /// listen yet. If that one listens first, the relay fails before the
    /// tracer spawns anything, naming the remedy, instead of leaving a guest
    /// waiting for a client that cannot reach it.
    #[test]
    fn a_port_another_socket_listened_on_first_fails_before_the_guest_spawns() {
        let runtime = runtime();
        let _guard = runtime.enter();
        let port = GdbPort::bind(0).unwrap();
        let other = std::net::TcpListener::bind(("127.0.0.1", port.port())).unwrap();
        let Err(error) = port.relay() else {
            panic!("the relay listened on a port another socket listens on");
        };
        let error = format!("{error:#}");
        assert!(
            error.contains("--gdbserver-port") && error.contains("EADDRINUSE"),
            "{error}"
        );
        drop(other);
    }

    /// The only test that touches the process-wide slot, so tests running
    /// as threads of one process cannot take each other's port.
    #[test]
    fn a_served_port_is_relayed_once_and_then_the_gdbserver_binds_its_own() {
        let runtime = runtime();
        let _guard = runtime.enter();
        let port = GdbPort::bind(0).unwrap();
        let number = port.port();
        port.serve_next_gdbserver();
        match connection(4321).unwrap() {
            GdbConnection::Path(path) => assert_eq!(
                path.as_os_str().as_encoded_bytes(),
                format!("\0hermit-gdbserver-{number}").as_bytes()
            ),
            GdbConnection::Addr(address) => panic!("{address} bound with a served port"),
        }
        match connection(4321).unwrap() {
            GdbConnection::Addr(address) => assert_eq!(address.to_string(), "127.0.0.1:4321"),
            GdbConnection::Path(path) => panic!("relayed to {path:?} with no served port"),
        }
    }

    #[test]
    fn the_relay_serves_one_client_once_the_gdbserver_listens() {
        if crate::run_in_fresh_process(
            "gdbserver::tests::the_relay_serves_one_client_once_the_gdbserver_listens",
        ) {
            return;
        }
        let runtime = runtime();
        let port = GdbPort::bind(0).unwrap();
        let number = port.port();
        // The process that bound the port keeps its descriptor of the socket
        // until the run ends.
        let binders_copy = port.socket.try_clone().unwrap();
        runtime.block_on(async move {
            let GdbConnection::Path(path) = port.relay().unwrap() else {
                panic!("a relay serves the gdbserver on a Unix socket");
            };
            assert!(listening(number));
            // The client connects and sends before the gdbserver exists, as
            // a client may while the tracer spawns the guest.
            let client = tokio::task::spawn_blocking(move || {
                let mut client = std::net::TcpStream::connect(("127.0.0.1", number)).unwrap();
                client.write_all(b"$qsupported#00").unwrap();
                let mut reply = [0; 14];
                client.read_exact(&mut reply).unwrap();
                // One client only: a second is refused.
                let second = std::net::TcpStream::connect(("127.0.0.1", number)).unwrap_err();
                drop(client);
                (reply, second.kind())
            });
            tokio::time::sleep(GDBSERVER_RETRY * 4).await;
            let server = fake_gdbserver(&path);
            let (reply, second) = client.await.unwrap();
            assert_eq!(&reply, b"$QSUPPORTED#00");
            assert_eq!(second, io::ErrorKind::ConnectionRefused);
            assert!(!listening(number));
            // The client's close reaches the gdbserver, which ends.
            server.await.unwrap();
        });
        drop(binders_copy);
    }
}
