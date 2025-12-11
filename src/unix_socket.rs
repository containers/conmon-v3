use std::{
    fmt,
    os::fd::{AsFd, OwnedFd},
    path::{Path, PathBuf},
};

use log::{debug, warn};
use nix::{
    errno::Errno,
    fcntl::OFlag,
    sys::{
        socket::{SockaddrStorage, recvfrom},
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
    Inotify,
    EventFd,
}

type RemoteSocketHandler = Box<dyn FnMut(&[u8]) -> bool + Send + 'static>;

/// Remote side (attach client or sd-notify FD inside container).
pub struct RemoteSocket {
    pub socket_type: SocketType,
    pub fd: OwnedFd,
    pub buf: [u8; 8192],
    buf_start: usize, // index of first valid byte
    buf_end: usize,   // one past last valid byte
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

impl RemoteSocket {
    pub fn new(socket_type: SocketType, fd: OwnedFd) -> Self {
        Self {
            socket_type,
            fd,
            buf: [0u8; 8192],
            buf_start: 0,
            buf_end: 0,
            handler: None,
        }
    }

    /// Attach a handler to this socket. The handler can capture arbitrary custom data.
    pub fn set_handler<F>(&mut self, handler: F)
    where
        F: FnMut(&[u8]) -> bool + Send + 'static,
    {
        self.handler = Some(Box::new(handler));
    }

    /// Compact the buffer so that valid data starts at index 0.
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

    /// Remove all data from the buffer.
    pub fn clear_buffer(&mut self) {
        self.buf_start = 0;
        self.buf_end = 0;
    }

    /// Read some bytes into the rolling buffer, without dispatching yet.
    pub fn read(&mut self) -> ConmonResult<usize> {
        // Ensure there is a space. If we are full, try compacting first.
        if self.buf_end == self.buf.len() {
            self.compact_buffer();
            if self.buf_end == self.buf.len() {
                // Still no room: line is longer than buffer.
                return Err(ConmonError::new("line too long for buffer", 1));
            }
        }

        let dst = &mut self.buf[self.buf_end..];
        let n = loop {
            match self.socket_type {
                SocketType::Stdout
                | SocketType::Stderr
                | SocketType::Terminal
                | SocketType::TerminalFifo
                | SocketType::EventFd
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
    /// After returning the line, it advances buf_start and compacts whatever remains.
    /// Returns None if no complete line is available.
    pub fn next_line(&mut self) -> Option<(*const u8, usize)> {
        // Search for '\n'.
        let rel = self.buf[self.buf_start..self.buf_end]
            .iter()
            .position(|&b| b == b'\n')?;

        let line_start = self.buf_start;
        let line_end = line_start + rel + 1; // include '\n'
        let len = line_end - line_start;

        // Get raw pointer to the data BEFORE altering the buffer.
        let ptr = self.buf[line_start..line_end].as_ptr();

        // Advance buffer start.
        self.buf_start = line_end;

        // If consumed everything, reset indices.
        if self.buf_start == self.buf_end {
            self.buf_start = 0;
            self.buf_end = 0;
        } else {
            // Compact remaining data to the beginning.
            self.compact_buffer();
        }

        Some((ptr, len))
    }
}

impl Drop for RemoteSocket {
    fn drop(&mut self) {
        info!("Dropping RemoteSocket {:?}", self.fd)
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

    /// Generates the socket path and starts listening for new client (remote) connections.
    pub fn listen(
        &mut self,
        path: Option<PathBuf>,
        sock_type: SockType,
        sock_flags: SockFlag,
        perms: Mode,
    ) -> ConmonResult<()> {
        let mut full_path: PathBuf;
        let mut dir_fd: Option<OwnedFd> = None;

        if let Some(path) = path {
            // We have some path, but we need a full-path.
            // If the path is a full-path, use it.
            // If it's not, generate the full-path using socket_parent_dir() and
            // prefix the path with it.
            full_path = path.to_owned();
            let fallback;
            let dir = if let Some(parent) = path.parent() {
                if !parent.is_empty() {
                    parent
                } else {
                    fallback = self.socket_parent_dir()?;
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

            // Create the parent-directory of full-path.
            let flags = OFlag::O_CREAT | OFlag::O_CLOEXEC | OFlag::O_PATH;
            let dfd = open(dir, flags, Mode::from_bits_truncate(0o600))?;

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

        // Now bind & listen on the console socket path.
        let fd = socket(AddressFamily::Unix, sock_type, sock_flags, None)?;

        self.bind_relative_to_dir(&fd, dir_fd.as_ref(), &full_path, perms)?;
        listen(&fd, Backlog::MAXCONN)?;
        info!("Listening on {full_path:?}");
        self.fd = Some(fd);
        self.path = Some(full_path);

        Ok(())
    }

    /// Bind the fd socket to relative path in dir_fd if defined.
    /// If not defined, the path is considered as full-path.
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
    fn socket_parent_dir(&mut self) -> ConmonResult<PathBuf> {
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

    /// Accept new UnixSocket client (remote) connection.
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

#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum Socket {
    Unix(UnixSocket),
    Remote(RemoteSocket),
}

impl Socket {
    pub fn handle_data(
        &mut self,
        log_plugin: &mut dyn LogPlugin,
        new_sockets: &mut Vec<RemoteSocket>,
        workerfd_stdin: Option<&OwnedFd>,
        console_fds: &Vec<i32>,
        terminal_fds: &Vec<i32>,
        stdout_fd: i32,
    ) -> ConmonResult<bool> {
        match self {
            Socket::Unix(l) => {
                if let Some(remote) = l.accept()? {
                    new_sockets.push(remote);
                }
                return Ok(true);
            }
            Socket::Remote(r) => {
                let bytes_read = r.read()?;
                if bytes_read == 0 {
                    return Ok(false);
                }

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
                        for &fd in console_fds {
                            let borrowed = unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) };
                            let iov = [
                                std::io::IoSlice::new(prefix_buf),
                                std::io::IoSlice::new(&r.buf[..bytes_read]),
                            ];
                            writev(borrowed, &iov)?;
                        }
                        r.clear_buffer();
                    }
                    SocketType::Console => {
                        // Console socket: forward data to container's stdin.
                        if let Some(workerfd_stdin) = workerfd_stdin.as_ref() {
                            write(workerfd_stdin, &r.buf[..bytes_read])?;
                        }
                        // Forward data to terminal.
                        for &fd in terminal_fds {
                            debug!("Forwarding to terminal {}", fd);
                            let borrowed = unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) };
                            write(borrowed, &r.buf[..bytes_read])?;
                        }
                        r.clear_buffer();
                    }
                    SocketType::Notify => {}
                    SocketType::Attach => {}
                    SocketType::TerminalFifo | SocketType::ConsoleFifo => {
                        // Handle all complete lines
                        while let Some((ptr, len)) = r.next_line() {
                            let line = unsafe { std::slice::from_raw_parts(ptr, len) };
                            let line_str = String::from_utf8_lossy(line);
                            if r.socket_type == SocketType::TerminalFifo {
                                if let Err(err) = process_terminal_ctrl_line(stdout_fd, &line_str) {
                                    warn!("failed to process terminal ctrl line: {}", err);
                                }
                            } else if let Err(err) = process_winsz_ctrl_line(stdout_fd, &line_str) {
                                warn!("failed to process terminal winsz line: {}", err);
                            }
                        }
                    }
                    SocketType::EventFd | SocketType::Inotify => {}
                }
            }
        }
        Ok(true)
    }
}
