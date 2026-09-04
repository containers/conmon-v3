use std::{
    fmt,
    os::fd::{AsFd, OwnedFd},
    path::{Path, PathBuf},
};

use log::{debug, error, info, warn};
use nix::{
    errno::Errno,
    fcntl::OFlag,
    sys::{
        socket::{MsgFlags, SockaddrStorage, recvfrom, sendto},
        uio::writev,
    },
    unistd::{read, write},
};

use crate::{
    error::{ConmonError, ConmonResult},
    logging::plugin::LogPlugin,
    runtime::ctl::{process_terminal_ctrl_line, process_winsz_ctrl_line},
};
use std::{
    ffi::OsStr,
    io,
    os::fd::{AsRawFd, FromRawFd},
    os::unix::ffi::OsStrExt,
};

use nix::{
    NixPath,
    fcntl::{AT_FDCWD, open},
    sys::{
        signalfd::SignalFd,
        socket::{
            AddressFamily, Backlog, SockFlag, SockType, UnixAddr, accept, bind, listen, socket,
        },
        stat::{Mode, fchmod},
    },
    unistd::{mkstemp, symlinkat, unlink},
};

// Type of the UnixSocket and RemoteSocket.
#[derive(Debug, Copy, Clone, Eq, PartialEq, Default)]
pub enum SocketType {
    #[default]
    Console, // Socket for container's stdin ("console").
    Notify,       // Socket for sd-notify.
    Terminal,     // Terminal socket received using --console-socket.
    Stdout,       // Socket for container's stdout.
    Stderr,       // Socket for container's stdin.
    Attach,       // Attach Unix socket.
    TerminalFifo, // Fifo for `ctl`.
    ConsoleFifo,  // Fifo for `winsz`.
    Inotify,      // Inotify socket of OOM detection.
    SignalFd,     // Signal fd to receive UNIX signals
}

type RemoteSocketHandler = Box<dyn FnMut(&[u8]) -> bool + Send + 'static>;

// Do not change the buffer size. It is in sync with podman and other
// parent apps. We use SOCK_SEQPACKET and if we cannot fit whole packet
// received from parent in a single `recvfrom`, the remaining data is lost.
const SOCKET_BUFFER_SIZE: usize = 32768;

// The buffer size of podman or other parent app when receiving the data.
// Again, this has to stay 8192, otherwise the podman wouldn't receive whole
// package and some data would be lost. See SOCKET_BUFFER_SIZE.
const CONMON_CLIENT_BUFFER_SIZE: usize = 8192;

/// A fixed-capacity rolling read buffer for socket data.
struct SocketBuffer<const N: usize> {
    /// Backing buffer.
    data: [u8; N],

    /// Index of the first valid byte.
    start: usize,

    /// Index of the last valid byte + 1.
    end: usize,
}

impl<const N: usize> SocketBuffer<N> {
    fn new() -> Self {
        Self {
            data: [0u8; N],
            start: 0,
            end: 0,
        }
    }

    /// Total capacity of the buffer in bytes.
    fn capacity(&self) -> usize {
        N
    }

    /// Returns the valid (unconsumed) bytes currently buffered.
    fn data(&self) -> &[u8] {
        &self.data[self.start..self.end]
    }

    /// Removes all data from the buffer.
    fn clear(&mut self) {
        self.start = 0;
        self.end = 0;
    }

    /// Try to make space if not already available and report true for success.
    fn make_space(&mut self) -> bool {
        if self.end == N {
            self.compact();
        }
        self.end != N
    }

    /// Compacts the buffer so that valid data starts at index 0.
    fn compact(&mut self) {
        if self.start == 0 {
            // We are done already :-).
            return;
        }
        if self.start >= self.end {
            // No data, so just reset the values.
            self.start = 0;
            self.end = 0;
            return;
        }

        let len = self.end - self.start;
        // Move remaining data to beginning.
        self.data.copy_within(self.start..self.end, 0);
        self.start = 0;
        self.end = len;
    }

    /// Returns the free space at the tail into which new bytes can be written.
    fn spare_slice_mut(&mut self) -> &mut [u8] {
        &mut self.data[self.end..]
    }

    /// Marks `n` freshly written tail bytes as valid.
    fn advance_by(&mut self, n: usize) {
        self.end += n;
    }

    /// Returns the next newline-terminated line (including the trailing `\n`)
    /// without consuming it, or `None` if no complete line is buffered yet.
    fn peek_line(&self) -> Option<&[u8]> {
        let rel = self.data[self.start..self.end]
            .iter()
            .position(|&b| b == b'\n')?;

        Some(&self.data[self.start..=self.start + rel]) // include '\n'
    }

    /// Consumes `n` bytes from the head of the buffer.
    fn consume(&mut self, n: usize) {
        debug_assert!(n <= self.end - self.start, "consume past buffered data");

        self.start += n;
        if self.start >= self.end {
            self.clear();
        }
    }
}

#[cfg(test)]
mod socket_buffer_tests {
    use super::*;

    const BUFFER_SIZE: usize = 8;

    /// Simulates RemoteSocket::read() usage.
    fn write(buf: &mut SocketBuffer<BUFFER_SIZE>, bytes: &[u8]) {
        assert!(buf.make_space());
        let dst = buf.spare_slice_mut();
        dst[..bytes.len()].copy_from_slice(bytes);
        buf.advance_by(bytes.len());
    }

    #[test]
    fn multiple_writes_combined() {
        let mut buf = SocketBuffer::<BUFFER_SIZE>::new();
        write(&mut buf, b"abc");
        write(&mut buf, b"de");
        assert_eq!(buf.data(), b"abcde");
    }

    #[test]
    fn clear_empties_buffer() {
        let mut buf = SocketBuffer::<BUFFER_SIZE>::new();
        write(&mut buf, b"abc");
        buf.clear();
        assert!(buf.data().is_empty());
    }

    #[test]
    fn peek_line_is_non_consuming_and_returns_first_line() {
        let mut buf = SocketBuffer::<BUFFER_SIZE>::new();

        // No newline - returns None.
        write(&mut buf, b"ab");
        assert_eq!(buf.peek_line(), None);

        // Two lines buffered.
        write(&mut buf, b"\nc\n");

        // Multiple calls are idempotent - only first line returned.
        assert_eq!(buf.peek_line(), Some(&b"ab\n"[..]));
        assert_eq!(buf.peek_line(), Some(&b"ab\n"[..]));

        // data() returns both lines.
        assert_eq!(buf.data(), b"ab\nc\n");
    }

    #[test]
    fn consume_advances_head_then_resets_when_drained() {
        let mut buf = SocketBuffer::<8>::new();
        write(&mut buf, b"a\nbc");

        // Consume 2 bytes, 2 remain.
        buf.consume(2);
        assert_eq!(buf.data(), b"bc");

        // Consume the rest.
        buf.consume(2);

        // No more data to consume.
        assert!(buf.data().is_empty());

        // Buffer is empty.
        assert_eq!(buf.spare_slice_mut().len(), BUFFER_SIZE);
    }

    #[test]
    fn make_space_compacts_reclaimed_space() {
        let mut buf = SocketBuffer::<BUFFER_SIZE>::new();

        // Fill up the buffer.
        write(&mut buf, b"abcdefgh");
        assert_eq!(buf.data().len(), BUFFER_SIZE);

        // Consume some.
        const CONSUMED: usize = 3;
        buf.consume(CONSUMED); // start = 3

        // Data survives compaction.
        assert!(buf.make_space());
        assert_eq!(buf.data(), b"defgh");

        // There is free space now equal to the consumed bytes.
        assert_eq!(buf.spare_slice_mut().len(), CONSUMED);
    }

    #[test]
    fn make_space_fails_when_full() {
        let mut buf = SocketBuffer::<BUFFER_SIZE>::new();
        write(&mut buf, b"abcdefgh");
        assert!(!buf.make_space());
    }
}

/// Remote side (attach client or sd-notify FD inside container).
pub struct RemoteSocket {
    /// Type of this socket.
    pub socket_type: SocketType,

    /// The file descriptor representing the socket.
    pub fd: OwnedFd,

    /// The buffer for data received from the socket.
    buf: SocketBuffer<SOCKET_BUFFER_SIZE>,

    /// Handler to call on new data.
    handler: Option<RemoteSocketHandler>,

    /// Set when the read side reached EOF.
    pub(crate) read_closed: bool,
}

impl fmt::Debug for RemoteSocket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RemoteSocket")
            .field("socket_type", &self.socket_type)
            .field("fd", &self.fd)
            // avoid dumping the whole 8K buffer
            .field("buf_len", &self.buf.capacity())
            .finish()
    }
}

// Represents all the sockets/fds we can read from.
impl RemoteSocket {
    pub fn new(socket_type: SocketType, fd: OwnedFd) -> Self {
        Self {
            socket_type,
            fd,
            buf: SocketBuffer::new(),
            handler: None,
            read_closed: false,
        }
    }

    /// Attach a handler to this socket.
    ///
    /// The handle is called when new data is received using socket.
    ///
    /// # Arguments
    ///
    /// * `handler` - The `RemoteSocketHandler` to call when data received.
    pub fn set_handler<F>(&mut self, handler: F)
    where
        F: FnMut(&[u8]) -> bool + Send + 'static,
    {
        self.handler = Some(Box::new(handler));
    }

    /// Removes all data from the buffer.
    pub fn clear_buffer(&mut self) {
        self.buf.clear();
    }

    /// Reads some bytes into the rolling buffer, without dispatching yet.
    ///
    /// # Returns
    ///
    /// * The number of bytes read.
    pub fn read(&mut self) -> ConmonResult<usize> {
        // Ensure there is a space. If we are full, try compacting first.
        if !self.buf.make_space() {
            return Err(ConmonError::new("line too long for buffer", 1));
        }

        // Read the data using `read` or `recvfrom`.
        let dst = self.buf.spare_slice_mut();
        let n = loop {
            match self.socket_type {
                SocketType::Stdout
                | SocketType::Stderr
                | SocketType::Terminal
                | SocketType::TerminalFifo
                | SocketType::Inotify
                | SocketType::ConsoleFifo => match read(self.fd.as_fd(), dst) {
                    Ok(n) => break n,
                    Err(err) if err == Errno::EWOULDBLOCK || err == Errno::EAGAIN => {
                        continue;
                    }
                    Err(err) => {
                        return Err(ConmonError::new(
                            format!("read failed: {}", io::Error::from_raw_os_error(err as i32)),
                            1,
                        ));
                    }
                },
                _ => match recvfrom::<SockaddrStorage>(self.fd.as_fd().as_raw_fd(), dst) {
                    Ok((n, _addr)) => break n,
                    Err(err) if err == Errno::EWOULDBLOCK || err == Errno::EAGAIN => {
                        continue;
                    }
                    Err(err) => {
                        return Err(ConmonError::new(
                            format!(
                                "read failed: {}, {:?}",
                                io::Error::from_raw_os_error(err as i32),
                                self.fd
                            ),
                            1,
                        ));
                    }
                },
            }
        };

        self.buf.advance_by(n);
        Ok(n)
    }

    /// Returns the next newline-terminated control line as owned UTF-8 text.
    ///
    /// The line (including the trailing `\n`) is consumed from the buffer
    /// before decoding, so invalid UTF-8 cannot be retried forever. Remaining
    /// bytes stay in place until the buffer is compacted when more space is
    /// needed.
    ///
    /// # Returns
    ///
    /// * `Ok(Some(line))` - A complete line including the trailing `\n`.
    /// * `Ok(None)` - No complete newline-terminated line is buffered yet.
    /// * `Err(_)` - The consumed line is not valid UTF-8.
    pub fn next_line(&mut self) -> ConmonResult<Option<String>> {
        let Some(line) = self.buf.peek_line() else {
            return Ok(None);
        };

        let len = line.len();
        let decoded = std::str::from_utf8(line).map(str::to_owned);
        self.buf.consume(len);

        match decoded {
            Ok(line) => Ok(Some(line)),
            Err(err) => Err(ConmonError::new(
                format!("control line is not valid UTF-8: {err}"),
                1,
            )),
        }
    }
}

impl Drop for RemoteSocket {
    fn drop(&mut self) {
        info!("Dropping RemoteSocket {:?}", self.fd)
    }
}

impl From<UnixSocket> for RemoteSocket {
    fn from(mut us: UnixSocket) -> Self {
        RemoteSocket {
            socket_type: us.socket_type,
            fd: us.fd.take().unwrap(),
            buf: SocketBuffer::new(),
            handler: None,
            read_closed: false,
        }
    }
}

/// Represents single UnixSocket.
#[derive(Default, Debug)]
pub struct UnixSocket {
    use_full_attach_path: bool,
    bundle_path: PathBuf,
    socket_path: Option<PathBuf>,
    cuuid: Option<String>,
    path: Option<PathBuf>,
    fd: Option<OwnedFd>,
    socket_type: SocketType,
}

impl UnixSocket {
    pub fn new(
        socket_type: SocketType,
        use_full_attach_path: bool,
        bundle_path: PathBuf,
        socket_path: Option<PathBuf>,
        cuuid: Option<String>,
    ) -> Self {
        let mut s = Self::default();
        s.socket_type = socket_type;
        s.use_full_attach_path = use_full_attach_path;
        s.bundle_path = bundle_path;
        s.socket_path = socket_path;
        s.cuuid = cuuid;
        s
    }

    pub fn fd(&self) -> Option<&OwnedFd> {
        self.fd.as_ref()
    }

    pub fn path(&self) -> Option<&PathBuf> {
        self.path.as_ref()
    }

    /// Generates the socket path, creates new socket and binds to the path.
    ///
    /// # Arguments
    ///
    /// * `path` - Path for the unix socket. if relative, the `socket_parent_dir()`
    ///   is used as a parent path. If None, socket is create in temporary directory.
    /// * `sock_type` - The type of the socket passed to `socket()`.
    /// * `sock_flags` - The socket flags passed to `socket()`.
    /// * `perms` - Permissions to `fchmod()` socket with.
    pub fn bind(
        &mut self,
        path: Option<PathBuf>,
        sock_type: SockType,
        sock_flags: SockFlag,
        perms: Mode,
    ) -> ConmonResult<()> {
        let mut full_path: PathBuf;
        let mut dir_fd: Option<OwnedFd> = None;

        if let Some(path) = path {
            // We have some path, but we need an absolute path.
            // If the path is an aboslute path, use it.
            // If it's not, generate the absolute path using socket_parent_dir() and
            // prefix the path with it.
            full_path = path.to_owned();
            let mut fallback;
            let dir = if let Some(parent) = path.parent() {
                if parent.is_absolute() {
                    parent
                } else {
                    fallback = self.socket_parent_dir()?;
                    fallback = fallback.join(parent);
                    let fallback_path = fallback.as_path();
                    full_path = fallback_path.join(path);
                    fallback_path
                }
            } else {
                fallback = self.socket_parent_dir()?;
                let fallback_path = fallback.as_path();
                full_path = fallback_path.join(path);
                fallback_path
            };

            // Create the parent-directory of aboslute path.
            let flags = OFlag::O_CREAT | OFlag::O_CLOEXEC | OFlag::O_PATH;
            let dfd = open(dir, flags, Mode::from_bits_truncate(0o600)).map_err(|e| {
                ConmonError::new(format!("Failed to open directory {dir:?}: {e:?}"), 1)
            })?;

            // Store the dir_fd, because we will be creating the socket in this dir.
            dir_fd = Some(dfd);
        } else {
            // We do not have a path, so create temporary one.
            let tmpdir = std::env::temp_dir();
            full_path = tmpdir.join("conmon-term.XXXXXX");
            let (fd_tmp, x) = mkstemp(&full_path)?;
            full_path = x;
            drop(fd_tmp);
        }

        // Remove old socket if present.
        unlink(&full_path).or_else(|e| {
            if e == nix::Error::ENOENT {
                Ok(())
            } else {
                Err(ConmonError::new(
                    format!("Failed to remove old socket {full_path:?}: {e}"),
                    1,
                ))
            }
        })?;

        // Now create a socket and bind to it.
        let fd = socket(AddressFamily::Unix, sock_type, sock_flags, None)?;
        self.bind_relative_to_dir(&fd, dir_fd.as_ref(), &full_path, perms)?;
        info!("Bound to {:?}", full_path);
        self.fd = Some(fd);
        self.path = Some(full_path);

        Ok(())
    }

    pub fn listen(&self) -> ConmonResult<()> {
        if let Some(fd) = &self.fd {
            listen(fd, Backlog::MAXCONN)?;
            info!("Listening on {:?}", self.path);
        }
        Ok(())
    }

    /// Binds the socket to relative path.
    ///
    /// # Arguments
    ///
    /// * `fd` - The socket to bind.
    /// * `dir_fd` - The file descriptor pointing to a directory in which we bind.
    ///   If not set, the `path` is used as a directory.
    /// * `path` - Path to bind to. If `dir_fd` is set, the path is used in
    ///   the `dir_fd` context.
    /// * `perms` - Permissions to `fchmod()` socket with.
    fn bind_relative_to_dir(
        &mut self,
        fd: &OwnedFd,
        dir_fd: Option<&OwnedFd>,
        path: &PathBuf,
        perms: Mode,
    ) -> ConmonResult<()> {
        let addr = if let Some(dfd) = dir_fd {
            // Get the base_name - the directory is defined by dir_fd.
            let base_name = path
                .file_name()
                .map(PathBuf::from)
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no basename"))?;
            // /proc/self/fd/<dir_fd>/<path>
            let name = format!(
                "/proc/self/fd/{}/{}",
                dfd.as_raw_fd(),
                base_name.to_string_lossy()
            );
            let path = Path::new(&name);
            UnixAddr::new(path).map_err(|e| {
                ConmonError::new(format!("Failed to create UnixAddr from {path:?}: {e:?}"), 1)
            })?
        } else {
            UnixAddr::new(path).map_err(|e| {
                ConmonError::new(format!("Failed to create UnixAddr from {path:?}: {e:?}"), 1)
            })?
        };

        info!("{:}", addr);
        fchmod(fd, perms)?;
        bind(fd.as_raw_fd(), &addr)?;
        Ok(())
    }

    /// Returns the max socket path length.
    fn max_socket_path_len(&mut self) -> usize {
        let addr: nix::sys::socket::sockaddr_un = unsafe { std::mem::zeroed() };
        addr.sun_path.len()
    }

    /// Generates the socket parent directory based on the UnixSocket options.
    ///
    /// # Returns
    ///
    /// * The parent directory.
    fn socket_parent_dir(&mut self) -> ConmonResult<PathBuf> {
        // Use the `bundle_path` as base path. Fallback to `socket_path`.
        let base_path = if self.use_full_attach_path {
            self.bundle_path.to_owned()
        } else if let Some(cuuid) = &self.cuuid {
            if let Some(socket_path) = &self.socket_path {
                socket_path.join(cuuid)
            } else {
                "".into()
            }
        } else {
            "".into()
        };

        // We don't have `bundle_path` nor `cuuid` and `socket_path`.
        if base_path.is_empty() {
            return Err(ConmonError::new(
                "Base path for socket cannot be determined",
                1,
            ));
        }

        if self.use_full_attach_path {
            // nothing else to do
            return Ok(base_path);
        }

        let desired_len = self.max_socket_path_len();
        let mut base_path_bytes = base_path.as_os_str().as_bytes().to_vec();
        if base_path_bytes.len() >= desired_len - 1 {
            // chop last char
            if let Some(last) = base_path_bytes.last_mut() {
                *last = b'\0';
            }
        }
        let new_base = PathBuf::from(OsStr::from_bytes(
            base_path_bytes
                .iter()
                .take_while(|b| **b != 0)
                .copied()
                .collect::<Vec<_>>()
                .as_slice(),
        ));

        // Remove old symlink if present
        unlink(&new_base).or_else(|e| {
            if e == nix::Error::ENOENT {
                Ok(())
            } else {
                Err(ConmonError::new(
                    format!("Cannot unlink {:?}: {e}", new_base),
                    1,
                ))
            }
        })?;

        // symlink(bundle_path, base_path)
        if let Err(e) = symlinkat(&self.bundle_path, AT_FDCWD, &new_base) {
            return Err(ConmonError::new(
                format!(
                    "Cannot symlink {:?} to {:?}: {e}",
                    self.bundle_path, new_base
                ),
                1,
            ));
        }

        Ok(new_base)
    }

    /// Accepts new UnixSocket client (remote) connection.
    ///
    /// # Returns
    /// * The RemoteSocket with new client connection. The type of the RemoteSocket
    ///   is the same as type of this UnixSocket.
    pub fn accept(&self) -> ConmonResult<Option<RemoteSocket>> {
        let Some(fd) = self.fd.as_ref() else {
            return Ok(None);
        };

        loop {
            match accept(fd.as_raw_fd()) {
                Ok(new_fd) => {
                    info!(
                        "Accepted new remote connection on socket {:?}: {}",
                        self.path, new_fd
                    );
                    let remote = RemoteSocket::new(self.socket_type, unsafe {
                        OwnedFd::from_raw_fd(new_fd)
                    });
                    return Ok(Some(remote));
                }
                Err(Errno::EINTR) => continue,
                #[allow(unreachable_patterns)]
                // EAGAIN and EWOULDBLOCK are distinct on some platforms.
                Err(Errno::EAGAIN) | Err(Errno::EWOULDBLOCK) => return Ok(None),
                Err(e) => {
                    return Err(ConmonError::new(
                        format!("Failed to accept client connection on {:?}: {e}", self.path),
                        1,
                    ));
                }
            }
        }
    }
}

impl Drop for UnixSocket {
    fn drop(&mut self) {
        if let Some(path) = self.path.take() {
            let _ = unlink(&path);
        }
    }
}

/// Creates new socket for sd-notify.
///
/// This socket is later used to forward messages to systemd.
///
/// # Arguments
///
/// * `socket_path` - Path to `notify.sock`.
///
/// # Returns
///
/// * (created socket, addr)
fn make_notify_socket_and_addr(socket_path: &Path) -> nix::Result<(OwnedFd, UnixAddr)> {
    // socket(AF_UNIX, SOCK_DGRAM | SOCK_NONBLOCK | SOCK_CLOEXEC, 0)
    let fd = socket(
        AddressFamily::Unix,
        SockType::Datagram,
        SockFlag::SOCK_NONBLOCK | SockFlag::SOCK_CLOEXEC,
        None,
    )?;
    let addr = UnixAddr::new(socket_path)?;

    Ok((fd, addr))
}

/// Exact sd-notify lines conmon forwards to the host (matches conmon v2).
const NOTIFY_PASSON_LINES: &[&str] = &[
    "READY=1",
    "RELOADING=1",
    "STOPPING=1",
    "WATCHDOG=1",
    "WATCHDOG=trigger",
];

/// sd-notify line prefixes conmon forwards to the host (matches conmon v2).
const NOTIFY_PASSON_PREFIXES: &[&str] = &["STATUS=", "ERRNO=", "BUSERROR=", "MONOTONIC_USEC="];

/// Returns whether a single notify line should be relayed to the host socket.
fn should_forward_notify_line(line: &str) -> bool {
    NOTIFY_PASSON_LINES.contains(&line)
        || NOTIFY_PASSON_PREFIXES
            .iter()
            .any(|prefix| line.starts_with(prefix))
}

/// Filter a notify datagram, keeping only whitelisted lines (matches conmon v2).
fn filter_notify_payload(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for line in data.split(|&b| b == b'\n' || b == b'\r') {
        if line.is_empty() {
            continue;
        }
        let Ok(line_str) = std::str::from_utf8(line) else {
            continue;
        };
        if should_forward_notify_line(line_str) {
            out.extend_from_slice(line);
            out.push(b'\n');
        }
    }
    out
}

/// Enum representing a UnixSocket, RemoteSocket or the Signal FD.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum Socket {
    Unix(UnixSocket),
    Remote(RemoteSocket),
    Signal(SignalFd),
}

impl Socket {
    /// Handles the POLLIN event for the socket at `sockets[i]`.
    ///
    /// # Arguments
    ///
    /// * `sockets` - The full poll set; `sockets[i]` is the source to read from.
    /// * `log_plugin` - The log plugin to forward container message to.
    /// * `new_sockets` - Vector into which newly created RemoteSocket can be added into.
    /// * `workerfd_stdin` - The container's stdin.
    /// * `sdnotify_socket` - Path to systemd's "notify.sock".
    pub fn handle_data(
        sockets: &mut [Socket],
        i: usize,
        log_plugin: &mut dyn LogPlugin,
        new_sockets: &mut Vec<RemoteSocket>,
        workerfd_stdin: Option<&OwnedFd>,
        sdnotify_socket: &Option<PathBuf>,
    ) -> ConmonResult<bool> {
        // The set is split into disjoint borrows so the source socket can be read
        // while its peers are written to.
        let (before, tail) = sockets.split_at_mut(i);
        let (source, after) = tail.split_first_mut().expect("i within bounds");
        match source {
            Socket::Unix(l) => {
                // Unix socket. Just `accept` new client connection and return.
                if let Some(remote) = l.accept()? {
                    new_sockets.push(remote);
                }
                return Ok(true);
            }
            Socket::Remote(r) => {
                // Client socket. Read what has been sent to it.
                let bytes_read = match r.read() {
                    Ok(n) => n,
                    Err(e) => {
                        r.clear_buffer();
                        error!("read error: {e}");
                        return Ok(true);
                    }
                };
                if bytes_read == 0 {
                    return Ok(false);
                }

                // If the Socket has a handler, call the handler directly and return.
                if let Some(handler) = r.handler.as_mut() {
                    return Ok(handler(r.buf.data()));
                }

                match r.socket_type {
                    SocketType::Stdout | SocketType::Stderr | SocketType::Terminal => {
                        // Forward data to logs.
                        let is_stderr = r.socket_type == SocketType::Stderr;
                        let _ = log_plugin.write(!is_stderr, r.buf.data());

                        // Forward data to remote sockets attached to `attach` socket.
                        // The data is prefixed with single byte indicating whether
                        // it is stdout or stderr.
                        let prefix_buf: &[u8] = if is_stderr {
                            &[3] // stdout
                        } else {
                            &[2] // stderr
                        };

                        // We send data in chunks, because our buffer has 32768 bytes while podman's
                        // buffer has 8192+1 bytes. It would be nice to unify that, but we need to
                        // keep the backwards compatibility for now. We also have to keep using
                        // SOCKET_SEQPACKET and therefore everything needs to be sent in a single packet.
                        let data = r.buf.data();
                        for chunk in data.chunks(CONMON_CLIENT_BUFFER_SIZE) {
                            for sock in before.iter().chain(after.iter()) {
                                let Socket::Remote(peer) = sock else { continue };
                                if peer.socket_type != SocketType::Console {
                                    continue;
                                }
                                let iov = [
                                    std::io::IoSlice::new(prefix_buf),
                                    std::io::IoSlice::new(chunk),
                                ];
                                writev(peer.fd.as_fd(), &iov)?;
                            }
                        }
                        r.clear_buffer();
                    }
                    SocketType::Console => {
                        // Console socket: forward data to container's stdin.
                        if let Some(workerfd_stdin) = workerfd_stdin.as_ref() {
                            let bytes_written = write(workerfd_stdin, r.buf.data())?;
                            info!("bytes written: {}", bytes_written);
                        }
                        // Forward data to terminal.
                        for sock in before.iter().chain(after.iter()) {
                            let Socket::Remote(peer) = sock else { continue };
                            if peer.socket_type != SocketType::Terminal || peer.read_closed {
                                continue;
                            }
                            debug!("Forwarding to terminal {}", peer.fd.as_raw_fd());
                            write(peer.fd.as_fd(), r.buf.data())?;
                        }
                        r.clear_buffer();
                    }
                    SocketType::Notify => {
                        // Relay whitelisted sd-notify lines from the container to the host.
                        // Some messages (e.g. BARRIER=1) are intentionally dropped; see conmon v2.
                        if let Some(notify_path) = &sdnotify_socket {
                            let payload = filter_notify_payload(r.buf.data());
                            if !payload.is_empty() {
                                let (notify_fd, notify_addr) =
                                    make_notify_socket_and_addr(notify_path)?;
                                info!(
                                    "Forwarding systemd notify: {}",
                                    String::from_utf8_lossy(&payload)
                                );
                                sendto(
                                    notify_fd.as_raw_fd(),
                                    &payload,
                                    &notify_addr,
                                    MsgFlags::MSG_DONTWAIT | MsgFlags::MSG_NOSIGNAL,
                                )?;
                            }
                        }
                        r.clear_buffer();
                    }
                    SocketType::TerminalFifo | SocketType::ConsoleFifo => {
                        // We received control message for "ctlr" or "winsz".
                        // Resize target: terminal pty if present, else container stdout.
                        let resize_target = |ty| {
                            before.iter().chain(after.iter()).find_map(|s| match s {
                                Socket::Remote(p) if p.socket_type == ty => Some(p.fd.as_raw_fd()),
                                _ => None,
                            })
                        };
                        let stdout_fd = resize_target(SocketType::Terminal)
                            .or_else(|| resize_target(SocketType::Stdout));
                        // Handle all complete lines. Invalid UTF-8 is logged and
                        // skipped like other malformed control lines, so one bad
                        // line cannot abort the stdio event loop.
                        loop {
                            let line = match r.next_line() {
                                Ok(Some(line)) => line,
                                Ok(None) => break,
                                Err(err) => {
                                    warn!("failed to decode control line: {err}");
                                    continue;
                                }
                            };

                            let Some(stdout_fd) = stdout_fd else { continue };
                            if r.socket_type == SocketType::TerminalFifo {
                                if let Err(err) =
                                    process_terminal_ctrl_line(log_plugin, stdout_fd, &line)
                                {
                                    warn!("failed to process terminal ctrl line: {err}");
                                }
                            } else if let Err(err) = process_winsz_ctrl_line(stdout_fd, &line) {
                                warn!("failed to process terminal winsz line: {err}");
                            }
                        }
                    }
                    SocketType::Inotify | SocketType::SignalFd | SocketType::Attach => {}
                }
            }
            Socket::Signal(_) => {
                return Ok(true);
            }
        }
        Ok(true)
    }
}

#[cfg(test)]
mod notify_filter_tests {
    use super::*;

    #[test]
    fn forwards_ready_line() {
        assert_eq!(filter_notify_payload(b"READY=1\n"), b"READY=1\n");
    }

    #[test]
    fn drops_rejected_lines() {
        assert_eq!(filter_notify_payload(b"BARRIER=1\n"), b"");
        assert!(filter_notify_payload(b"BARRIER=1").is_empty());
        assert!(filter_notify_payload(b"BARRIER=1\nUNKNOWN=1\n").is_empty());
    }

    #[test]
    fn drops_barrier_but_keeps_ready() {
        assert_eq!(filter_notify_payload(b"READY=1\nBARRIER=1\n"), b"READY=1\n");
    }

    #[test]
    fn drops_concatenated_unlisted_line() {
        assert_eq!(filter_notify_payload(b"READY=1BARRIER=1\n"), b"");
    }

    #[test]
    fn forwards_status_prefix() {
        assert_eq!(
            filter_notify_payload(b"STATUS=starting\n"),
            b"STATUS=starting\n"
        );
    }

    #[test]
    fn forwards_multiple_allowed_lines() {
        assert_eq!(
            filter_notify_payload(b"STATUS=ok\nREADY=1\nBARRIER=1\n"),
            b"STATUS=ok\nREADY=1\n"
        );
    }

    #[test]
    fn multi_line_datagram_stays_one_filtered_payload() {
        // A single sd-notify datagram may contain several assignments. Filtering must
        // keep permitted fields together in one combined payload.
        let datagram = b"STATUS=ok\nREADY=1\nBARRIER=1\nWATCHDOG=1\n";
        assert_eq!(
            filter_notify_payload(datagram),
            b"STATUS=ok\nREADY=1\nWATCHDOG=1\n"
        );
    }

    #[test]
    fn datagram_without_trailing_newline_is_complete() {
        // The final assignment in a systemd notify datagram need not end with '\n'.
        assert_eq!(filter_notify_payload(b"READY=1"), b"READY=1\n");
        assert_eq!(
            filter_notify_payload(b"STATUS=ok\nREADY=1"),
            b"STATUS=ok\nREADY=1\n"
        );
    }

    #[test]
    fn should_forward_notify_line_matches_v2_whitelist() {
        assert!(should_forward_notify_line("READY=1"));
        assert!(should_forward_notify_line("WATCHDOG=trigger"));
        assert!(should_forward_notify_line("STATUS=foo"));
        assert!(!should_forward_notify_line("BARRIER=1"));
        assert!(!should_forward_notify_line("READY=1BARRIER=1"));
    }
}

#[cfg(test)]
mod next_line_tests {
    use super::*;

    fn socket_with_buffered_data(data: &[u8], socket_type: SocketType) -> RemoteSocket {
        let (r, _w) = nix::unistd::pipe2(OFlag::O_CLOEXEC).expect("pipe");
        let mut socket = RemoteSocket::new(socket_type, r);
        assert!(
            data.len() <= socket.buf.capacity(),
            "test data exceeds RemoteSocket buffer capacity"
        );
        let len = data.len();
        socket.buf.data[..len].copy_from_slice(data);
        socket.buf.start = 0;
        socket.buf.end = len;
        socket
    }

    #[test]
    fn next_line_returns_both_fifo_control_lines() -> ConmonResult<()> {
        let mut socket = socket_with_buffered_data(b"1 80 24\n2 0 0\n", SocketType::TerminalFifo);

        let first = socket.next_line()?.expect("first line");
        assert_eq!(first, "1 80 24\n");

        let second = socket.next_line()?.expect("second line");
        assert_eq!(second, "2 0 0\n");

        assert!(socket.next_line()?.is_none());
        assert_eq!(socket.buf.start, 0);
        assert_eq!(socket.buf.end, 0);
        Ok(())
    }

    #[test]
    fn next_line_owned_string_remains_valid_after_next_call() -> ConmonResult<()> {
        let mut socket = socket_with_buffered_data(b"a\nbc\n", SocketType::ConsoleFifo);

        let first = socket.next_line()?.expect("first line");
        let second = socket.next_line()?.expect("second line");

        assert_eq!(first, "a\n");
        assert_eq!(second, "bc\n");
        Ok(())
    }

    #[test]
    fn next_line_extracts_second_line_without_eager_compaction() -> ConmonResult<()> {
        let mut socket = socket_with_buffered_data(b"a\nbc\n", SocketType::ConsoleFifo);

        assert_eq!(socket.next_line()?.as_deref(), Some("a\n"));
        assert!(socket.buf.start > 0);

        assert_eq!(socket.next_line()?.as_deref(), Some("bc\n"));
        Ok(())
    }

    #[test]
    fn next_line_preserves_incomplete_trailing_data() -> ConmonResult<()> {
        let mut socket = socket_with_buffered_data(b"first\npartial", SocketType::ConsoleFifo);

        assert_eq!(socket.next_line()?.as_deref(), Some("first\n"));
        assert!(socket.next_line()?.is_none());
        assert_eq!(socket.buf.data(), b"partial");
        Ok(())
    }

    #[test]
    fn next_line_consumes_invalid_utf8_and_preserves_following_line() {
        let mut socket = socket_with_buffered_data(b"ok\n\xff\nlater\n", SocketType::TerminalFifo);

        assert_eq!(socket.next_line().unwrap().as_deref(), Some("ok\n"));

        let err = socket
            .next_line()
            .expect_err("invalid UTF-8 must be an explicit error");
        assert!(err.to_string().contains("not valid UTF-8"));

        // Invalid line was consumed; later valid lines remain available.
        assert_eq!(socket.next_line().unwrap().as_deref(), Some("later\n"));
        assert!(socket.next_line().unwrap().is_none());
    }
}
