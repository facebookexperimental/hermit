//! Independent protected observer endpoint. Never installs/replaces DETLOG.
use detcore::detlog::NativePhysicalExitCompletion;
use detcore::detlog::NativePhysicalWatchIdentity;
use hermit::in_guest_exits::NativeWatchedProcess;
fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}
use std::io;
use std::os::fd::AsRawFd;
use std::os::fd::OwnedFd;
use std::sync::Arc;
use std::sync::Mutex;
const FRAME: usize = 96;
const MAX_ENTRIES: usize = 32768;
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Entry {
    pub sequence: u64,
    pub pid: i32,
    pub tid: i32,
    pub dettid: i64,
    pub number: i64,
    pub pc: u64,
    pub arguments: [u64; 6],
}
fn decode(bytes: &[u8], pid: i32, next: u64) -> io::Result<Entry> {
    if bytes.len() != FRAME || &bytes[..8] != b"HLISE001" {
        return Err(invalid("native observer exact shape/magic differs"));
    }
    let word = |i| u64::from_le_bytes(bytes[i..i + 8].try_into().unwrap());
    let entry = Entry {
        sequence: word(8),
        pid: i32::from_le_bytes(bytes[16..20].try_into().unwrap()),
        tid: i32::from_le_bytes(bytes[20..24].try_into().unwrap()),
        dettid: word(24) as i64,
        number: word(32) as i64,
        pc: word(40),
        arguments: std::array::from_fn(|i| word(48 + i * 8)),
    };
    if pid <= 0
        || entry.pid != pid
        || entry.tid != pid
        || entry.dettid != i64::from(pid)
        || entry.sequence != next
        || next == u64::MAX
        || entry.pc == 0
    {
        return Err(invalid("native observer actual owner/sequence/PC differs"));
    }
    Ok(entry)
}
#[derive(Default)]
struct State {
    entries: Vec<Entry>,
    bytes: Vec<u8>,
    error: Option<String>,
    closed: bool,
}
struct Stream {
    fd: OwnedFd,
    pid: i32,
    state: Mutex<State>,
}
impl Stream {
    pub fn new(fd: OwnedFd, pid: i32) -> io::Result<Arc<Self>> {
        if pid <= 0 {
            return Err(invalid("missing native observer watched PID"));
        }
        let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
        if flags < 0
            || unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
        {
            return Err(io::Error::last_os_error());
        }
        let one = 1i32;
        if unsafe {
            libc::setsockopt(
                fd.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PASSCRED,
                (&raw const one).cast(),
                4,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(Arc::new(Self {
            fd,
            pid,
            state: Mutex::new(State::default()),
        }))
    }
    pub fn drain(&self) -> io::Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| invalid("native observer poisoned"))?;
        if let Some(error) = &state.error {
            return Err(invalid(error.clone()));
        }
        let result = (|| {
            for _ in 0..MAX_ENTRIES {
                let mut bytes = [0u8; FRAME];
                let mut control = [0u64; 8];
                let mut iov = libc::iovec {
                    iov_base: bytes.as_mut_ptr().cast(),
                    iov_len: FRAME,
                };
                let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
                msg.msg_iov = &mut iov;
                msg.msg_iovlen = 1;
                msg.msg_control = control.as_mut_ptr().cast();
                msg.msg_controllen = std::mem::size_of_val(&control);
                let n = unsafe {
                    libc::recvmsg(
                        self.fd.as_raw_fd(),
                        &mut msg,
                        libc::MSG_DONTWAIT | libc::MSG_TRUNC | libc::MSG_CMSG_CLOEXEC,
                    )
                };
                if n < 0 {
                    let error = io::Error::last_os_error();
                    if error.kind() == io::ErrorKind::WouldBlock {
                        return Ok(());
                    }
                    return Err(error);
                }
                if n == 0 && msg.msg_controllen == 0 {
                    state.closed = true;
                    return Ok(());
                }
                let mut credential = None;
                let mut unexpected = false;
                let mut header = unsafe { libc::CMSG_FIRSTHDR(&msg) };
                while !header.is_null() {
                    let h = unsafe { &*header };
                    let minimum = unsafe { libc::CMSG_LEN(0) } as usize;
                    if h.cmsg_len < minimum {
                        unexpected = true;
                        break;
                    }
                    if h.cmsg_level == libc::SOL_SOCKET && h.cmsg_type == libc::SCM_RIGHTS {
                        let count = (h.cmsg_len - minimum) / 4;
                        for i in 0..count {
                            unsafe {
                                libc::close(
                                    libc::CMSG_DATA(header)
                                        .cast::<i32>()
                                        .add(i)
                                        .read_unaligned(),
                                )
                            };
                        }
                        unexpected = true;
                    } else if h.cmsg_level == libc::SOL_SOCKET
                        && h.cmsg_type == libc::SCM_CREDENTIALS
                        && h.cmsg_len
                            == unsafe { libc::CMSG_LEN(std::mem::size_of::<libc::ucred>() as u32) }
                                as usize
                        && credential.is_none()
                    {
                        credential = Some(unsafe {
                            libc::CMSG_DATA(header)
                                .cast::<libc::ucred>()
                                .read_unaligned()
                        });
                    } else {
                        unexpected = true;
                    }
                    header = unsafe { libc::CMSG_NXTHDR(&msg, header) };
                }
                if unexpected
                    || msg.msg_flags & (libc::MSG_TRUNC | libc::MSG_CTRUNC) != 0
                    || n as usize != FRAME
                    || credential.is_none_or(|c| c.pid != self.pid)
                {
                    return Err(invalid(
                        "native observer actual sender/ancillary/extent refused",
                    ));
                }
                if state.entries.len() >= MAX_ENTRIES {
                    return Err(invalid("native observer finite entry bound exceeded"));
                }
                let next = state.entries.last().map_or(1, |e| e.sequence + 1);
                let entry = decode(&bytes, self.pid, next)?;
                state.entries.push(entry);
                state.bytes.extend_from_slice(&bytes);
            }
            Err(invalid("native observer drain exceeded finite work bound"))
        })();
        if let Err(error) = &result {
            state.error = Some(error.to_string());
        }
        result
    }
    #[cfg(any(feature = "native-exec", test))]
    pub fn last(&self) -> io::Result<Entry> {
        self.drain()?;
        let state = self
            .state
            .lock()
            .map_err(|_| invalid("observer poisoned"))?;
        if state.closed {
            return Err(invalid("native observer closed before cut"));
        }
        state
            .entries
            .last()
            .cloned()
            .ok_or_else(|| invalid("native observer has no genuine dispatch entry"))
    }
    #[cfg(any(feature = "native-exec", test))]
    pub fn original_exec_cut(&self) -> io::Result<Entry> {
        let entry = self.last()?;
        let a = entry.arguments;
        if !((entry.number == libc::SYS_execve && a[0] != 0 && a[1] != 0 && a[2] != 0)
            || (entry.number == libc::SYS_execveat
                && a[0] == libc::AT_FDCWD as i64 as u64
                && a[1] != 0
                && a[2] != 0
                && a[3] != 0
                && a[4] == 0))
        {
            return Err(invalid(
                "last real dispatch is not supported original exec syscall",
            ));
        }
        Ok(entry)
    }
    pub fn finish(&self) -> io::Result<Vec<u8>> {
        self.drain()?;
        let state = self
            .state
            .lock()
            .map_err(|_| invalid("observer poisoned"))?;
        if !state.closed || state.error.is_some() || state.entries.is_empty() {
            return Err(invalid("native observer incomplete/error drain"));
        }
        Ok(state.bytes.clone())
    }
    pub async fn serve(self: Arc<Self>) -> io::Result<()> {
        let raw = unsafe { libc::fcntl(self.fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        use std::os::fd::FromRawFd;
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        let fd = tokio::io::unix::AsyncFd::new(fd)?;
        loop {
            let mut readiness = fd.readable().await?;
            self.drain()?;
            if self
                .state
                .lock()
                .map_err(|_| invalid("observer poisoned"))?
                .closed
            {
                return Ok(());
            }
            readiness.clear_ready();
        }
    }
}
/// Actual watched owner around the bounded ancillary/sequence decoder. The
/// mechanism's model tests never manufacture this physical launch authority.
pub(super) struct Observer {
    stream: Arc<Stream>,
    watch: Arc<NativeWatchedProcess>,
    identity: (u64, u64),
}
impl Observer {
    pub(super) fn new(fd: OwnedFd, watch: Arc<NativeWatchedProcess>) -> io::Result<Arc<Self>> {
        if !watch.watch_is_healthy() {
            return Err(invalid("native observer has no healthy physical owner"));
        }
        let identity = detcore::detlog::socket_identity(fd.as_raw_fd())
            .ok_or_else(|| invalid("native observer socket identity unavailable"))?;
        let mut kind = 0i32;
        let mut size = std::mem::size_of_val(&kind) as libc::socklen_t;
        if unsafe {
            libc::getsockopt(
                fd.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_TYPE,
                (&raw mut kind).cast(),
                &mut size,
            )
        } != 0
            || size as usize != std::mem::size_of_val(&kind)
            || kind != libc::SOCK_SEQPACKET
        {
            return Err(invalid("native observer requires actual sequenced packets"));
        }
        Ok(Arc::new(Self {
            stream: Stream::new(fd, watch.process_id())?,
            watch,
            identity,
        }))
    }
    /// Failure diagnostics only; does not establish completion or a valid cut.
    #[cfg(all(feature = "exec-stop-launcher", feature = "liteinst"))]
    pub(super) fn observed_bytes(&self) -> io::Result<Vec<u8>> {
        let state = self
            .stream
            .state
            .lock()
            .map_err(|_| invalid("observer poisoned"))?;
        Ok(state.bytes.clone())
    }

    #[cfg(feature = "native-exec")]
    pub(super) fn last(&self) -> io::Result<Entry> {
        if !self.watch.watch_is_healthy() {
            return Err(invalid(
                "native observer physical watch failed or was replaced",
            ));
        }
        self.stream.last()
    }
    #[cfg(feature = "native-exec")]
    pub(super) fn original_exec_cut(&self) -> io::Result<Entry> {
        self.last()?;
        self.stream.original_exec_cut()
    }
    pub(super) async fn serve(self: Arc<Self>) -> io::Result<()> {
        Arc::clone(&self.stream).serve().await
    }
    pub(super) fn finish(
        &self,
        completion: &dyn NativePhysicalExitCompletion,
    ) -> io::Result<Vec<u8>> {
        if completion.process_id() != self.watch.process_id()
            || completion.birth_identity() != self.watch.birth_identity()
            || detcore::detlog::socket_identity(self.stream.fd.as_raw_fd()) != Some(self.identity)
        {
            return Err(invalid(
                "native observer final drain has a different physical owner or socket",
            ));
        }
        self.stream.finish()
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn frame() -> [u8; FRAME] {
        let mut b = [0; FRAME];
        b[..8].copy_from_slice(b"HLISE001");
        b[8..16].copy_from_slice(&1u64.to_le_bytes());
        b[16..20].copy_from_slice(&7i32.to_le_bytes());
        b[20..24].copy_from_slice(&7i32.to_le_bytes());
        b[24..32].copy_from_slice(&7u64.to_le_bytes());
        b[40..48].copy_from_slice(&0x401000u64.to_le_bytes());
        b
    }
    #[test]
    fn model_observer_exact_shape_and_owner_not_detlog() {
        let b = frame();
        assert_eq!(decode(&b, 7, 1).unwrap().sequence, 1);
        assert!(decode(b"[7]INFO record", 7, 1).is_err());
        for end in 0..FRAME {
            assert!(decode(&b[..end], 7, 1).is_err());
        }
        assert!(decode(&b, 8, 1).is_err());
        assert!(decode(&b, 7, 2).is_err());
    }
    #[test]
    fn model_observer_all_identity_words_and_sequence_are_required() {
        let b = frame();
        for i in [0, 8, 16, 20, 24] {
            let mut changed = b;
            changed[i] ^= 1;
            assert!(decode(&changed, 7, 1).is_err());
        }
        let mut changed = b;
        changed[40..48].fill(0);
        assert!(decode(&changed, 7, 1).is_err());
    }
    fn actual_pair() -> (OwnedFd, OwnedFd) {
        use std::os::fd::FromRawFd;
        let mut fds = [-1; 2];
        assert_eq!(
            unsafe {
                libc::socketpair(
                    libc::AF_UNIX,
                    libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
                    0,
                    fds.as_mut_ptr(),
                )
            },
            0
        );
        unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) }
    }
    fn actual_frame(pid: i32, sequence: u64) -> [u8; FRAME] {
        let mut bytes = frame();
        bytes[8..16].copy_from_slice(&sequence.to_le_bytes());
        bytes[16..20].copy_from_slice(&pid.to_le_bytes());
        bytes[20..24].copy_from_slice(&pid.to_le_bytes());
        bytes[24..32].copy_from_slice(&(pid as u64).to_le_bytes());
        bytes
    }
    fn send(fd: &OwnedFd, bytes: &[u8]) {
        assert_eq!(
            unsafe {
                libc::send(
                    fd.as_raw_fd(),
                    bytes.as_ptr().cast(),
                    bytes.len(),
                    libc::MSG_NOSIGNAL,
                )
            },
            bytes.len() as isize
        );
    }
    #[test]
    fn actual_observer_sender_credentials_are_independent_of_claimed_frame_ids() {
        let pid = std::process::id() as i32;
        let (read, write) = actual_pair();
        let observer = Stream::new(read, pid).unwrap();
        send(&write, &actual_frame(pid, 1));
        assert_eq!(observer.last().unwrap().sequence, 1);
        drop(write);
        assert_eq!(observer.finish().unwrap().len(), 96);
        let (read, write) = actual_pair();
        let observer = Stream::new(read, pid + 1).unwrap();
        send(&write, &actual_frame(pid + 1, 1));
        assert!(observer.last().is_err());
        drop(write);
        assert!(observer.finish().is_err());
    }
    #[test]
    fn actual_observer_keeps_sequence_and_early_close_failure() {
        let pid = std::process::id() as i32;
        let (read, write) = actual_pair();
        let observer = Stream::new(read, pid).unwrap();
        send(&write, &actual_frame(pid, 1));
        send(&write, &actual_frame(pid, 3));
        assert!(observer.last().is_err());
        drop(write);
        assert!(observer.finish().is_err());
        let (read, write) = actual_pair();
        let observer = Stream::new(read, pid).unwrap();
        drop(write);
        assert!(observer.original_exec_cut().is_err());
        assert!(observer.finish().is_err());
    }
}
