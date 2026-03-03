use std::{
    fmt,
    os::fd::{AsFd, OwnedFd},
    path::{Path, PathBuf},
};

use log::{debug, error, warn};
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
        socket::{
            AddressFamily, Backlog, SockFlag, SockType, UnixAddr, accept, bind, listen, socket,
        },
        stat::{Mode, fchmod},
    },
    unistd::{mkstemp, symlinkat, unlink},
};

use ::log::info;

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

/// Remote side (attach client or sd-notify FD inside container).
pub struct RemoteSocket {
    /// Type of this socket.
    pub socket_type: SocketType,

    /// The file descriptor representing the socket.
    pub fd: OwnedFd,

    /// The buffer for a data received from the socket.
    pub buf: [u8; SOCKET_BUFFER_SIZE],

    /// Index of the first valid byte.
    buf_start: usize,

    /// One past the last valid byte.
    buf_end: usize,

    /// Handler to call on new data.
    handler: Option<RemoteSocketHandler>,
}

impl fmt::Debug for RemoteSocket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RemoteSocket")
            .field("socket_type", &self.socket_type)
            .field("fd", &self.fd)
            // avoid dumping the whole 8K buffer
            .field("buf_len", &self.buf.len())
            .finish()
    }
}

// Represents all the sockets/fds we can read from.
impl RemoteSocket {
    pub fn new(socket_type: SocketType, fd: OwnedFd) -> Self {
        Self {
            socket_type,
            fd,
            buf: [0u8; SOCKET_BUFFER_SIZE],
            buf_start: 0,
            buf_end: 0,
            handler: None,
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

    /// Compacts the buffer so that valid data starts at index 0.
    fn compact_buffer(&mut self) {
        if self.buf_start == 0 {
            // We are done already :-).
            return;
        }
        if self.buf_start >= self.buf_end {
            // No data, so just reset the values.
            self.buf_start = 0;
            self.buf_end = 0;
            return;
        }

        let len = self.buf_end - self.buf_start;
        // Move remaining data to beginning.
        self.buf.copy_within(self.buf_start..self.buf_end, 0);
        self.buf_start = 0;
        self.buf_end = len;
    }

    /// Removes all data from the buffer.
    pub fn clear_buffer(&mut self) {
        self.buf_start = 0;
        self.buf_end = 0;
    }

    /// Reads some bytes into the rolling buffer, without dispatching yet.
    ///
    /// # Returns
    ///
    /// * The number of bytes read.
    pub fn read(&mut self) -> ConmonResult<usize> {
        // Ensure there is a space. If we are full, try compacting first.
        if self.buf_end == self.buf.len() {
            self.compact_buffer();
            if self.buf_end == self.buf.len() {
                // Still no room: line is longer than buffer.
                return Err(ConmonError::new("line too long for buffer", 1));
            }
        }

        // Read the data using `read` or `recvfrom`.
        let dst = &mut self.buf[self.buf_end..];
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

        // EOF or no data
        if n == 0 {
            return Ok(0);
        }

        self.buf_end += n;
        Ok(n)
    }

    /// Returns a pointer + length to the next newline-terminated line.
    ///
    /// After returning the line, it advances buf_start and compacts whatever remains.
    ///
    /// # Returns
    ///
    /// * (pointer to line, length of the line)
    /// * None if no complete line is available.
    ///
    /// # Arguments
    ///
    /// * `allow_partial` - If true, if returns the buffer content even if it does
    ///   not end with a new-line character.
    pub fn next_line(&mut self, allow_partial: bool) -> Option<(*const u8, usize)> {
        // Search for '\n' in the available buffer slice.
        if let Some(rel) = self.buf[self.buf_start..self.buf_end]
            .iter()
            .position(|&b| b == b'\n')
        {
            let line_start = self.buf_start;
            let line_end = line_start + rel + 1; // include '\n'
            let len = line_end - line_start;

            // Get raw pointer BEFORE altering the buffer.
            let ptr = self.buf[line_start..line_end].as_ptr();

            // Advance buffer start.
            self.buf_start = line_end;

            // If consumed everything, reset indices.
            if self.buf_start == self.buf_end {
                self.buf_start = 0;
                self.buf_end = 0;
            } else {
                self.compact_buffer();
            }

            return Some((ptr, len));
        }

        // No '\n' found
        if allow_partial && self.buf_start < self.buf_end {
            let line_start = self.buf_start;
            let line_end = self.buf_end;
            let len = line_end - line_start;

            // Raw pointer to remaining data
            let ptr = self.buf[line_start..line_end].as_ptr();

            // Consume everything
            self.buf_start = 0;
            self.buf_end = 0;

            return Some((ptr, len));
        }

        None
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
            buf: [0u8; SOCKET_BUFFER_SIZE],
            buf_start: 0,
            buf_end: 0,
            handler: None,
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
        if self.fd.is_none() {
            return Ok(None);
        }

        match accept(self.fd.as_ref().unwrap().as_raw_fd()) {
            Ok(new_fd) => {
                info!(
                    "Accepted new remote connection on socket {:?}: {}",
                    self.path, new_fd
                );
                let remote =
                    RemoteSocket::new(self.socket_type, unsafe { OwnedFd::from_raw_fd(new_fd) });
                Ok(Some(remote))
            }
            Err(Errno::EWOULDBLOCK) => Ok(None),
            Err(e) => {
                eprintln!("warn: Failed to accept client connection on attach socket: {e}");
                Ok(None)
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

/// Enum representing UnixSocket, RemoteSocket or invalid socket.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum Socket {
    Unix(UnixSocket),
    Remote(RemoteSocket),
    Invalid(),
}

impl Socket {
    /// Handles the POLLIN event for a Socket.
    ///
    /// # Arguments
    ///
    /// * `log_plugin` - The log plugin to forward container message to.
    /// * `new_sockets` - Vector into which newly created RemoteSocket can be added into.
    /// * `workerfd_stdin` - The container's stdin.
    /// * `console_fds` - The list of podman's fds using which the podman receives
    ///   stdout/stderr data from container.
    /// * `terminal_fds` - Terminal fds into which we forward data for container's stdin.
    /// * `stdout_fd` - The fd of container's stdout. We use it to change the terminal
    ///   size.
    /// * `sdnotify_socket` - Path to systemd's "notify.sock".
    #[allow(clippy::too_many_arguments)]
    pub fn handle_data(
        &mut self,
        log_plugin: &mut dyn LogPlugin,
        new_sockets: &mut Vec<RemoteSocket>,
        workerfd_stdin: Option<&OwnedFd>,
        console_fds: &Vec<i32>,
        terminal_fds: &Vec<i32>,
        stdout_fd: i32,
        sdnotify_socket: &Option<PathBuf>,
    ) -> ConmonResult<bool> {
        match self {
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
                    return Ok(handler(&r.buf[..bytes_read]));
                }

                match r.socket_type {
                    SocketType::Stdout | SocketType::Stderr | SocketType::Terminal => {
                        // Forward data to logs.
                        let is_stderr = r.socket_type == SocketType::Stderr;
                        let _ = log_plugin.write(!is_stderr, &r.buf[..bytes_read]);

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
                        let data = &r.buf[..bytes_read];
                        for chunk in data.chunks(CONMON_CLIENT_BUFFER_SIZE) {
                            for &fd in console_fds {
                                let borrowed = unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) };
                                let iov = [
                                    std::io::IoSlice::new(prefix_buf),
                                    std::io::IoSlice::new(chunk),
                                ];
                                writev(borrowed, &iov)?;
                            }
                        }
                        r.clear_buffer();
                    }
                    SocketType::Console => {
                        // Console socket: forward data to container's stdin.
                        if let Some(workerfd_stdin) = workerfd_stdin.as_ref() {
                            let bytes_written = write(workerfd_stdin, &r.buf[..bytes_read])?;
                            info!("bytes written: {}", bytes_written);
                        }
                        // Forward data to terminal.
                        for &fd in terminal_fds {
                            debug!("Forwarding to terminal {}", fd);
                            let borrowed = unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) };
                            write(borrowed, &r.buf[..bytes_read])?;
                        }
                        r.clear_buffer();
                    }
                    SocketType::Notify => {
                        // We received something from "notify.sock" from the container. We need
                        // to forward it to host system's systemd.
                        if let Some(notify_path) = &sdnotify_socket {
                            let (notify_fd, notify_addr) =
                                make_notify_socket_and_addr(notify_path)?;
                            // Handle all complete lines.
                            while let Some((ptr, len)) = r.next_line(true) {
                                let line = unsafe { std::slice::from_raw_parts(ptr, len) };
                                let line_str = String::from_utf8_lossy(line);
                                info!("Received systemd notify line: {}", line_str);
                                sendto(
                                    notify_fd.as_raw_fd(),
                                    line,
                                    &notify_addr,
                                    MsgFlags::MSG_DONTWAIT | MsgFlags::MSG_NOSIGNAL,
                                )?;
                            }
                        }
                    }
                    SocketType::TerminalFifo | SocketType::ConsoleFifo => {
                        // We received control message for "ctlr" or "winsz".
                        // Handle all complete lines.
                        while let Some((ptr, len)) = r.next_line(false) {
                            let line = unsafe { std::slice::from_raw_parts(ptr, len) };
                            let line_str = String::from_utf8_lossy(line);
                            if r.socket_type == SocketType::TerminalFifo {
                                if let Err(err) = process_terminal_ctrl_line(log_plugin, stdout_fd, &line_str) {
                                    warn!("failed to process terminal ctrl line: {}", err);
                                }
                            } else if let Err(err) = process_winsz_ctrl_line(stdout_fd, &line_str) {
                                warn!("failed to process terminal winsz line: {}", err);
                            }
                        }
                    }
                    SocketType::Inotify | SocketType::SignalFd | SocketType::Attach => {}
                }
            }
            Socket::Invalid() => {
                return Ok(true);
            }
        }
        Ok(true)
    }
}
