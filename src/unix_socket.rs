use std::{
    os::fd::OwnedFd,
    path::{Path, PathBuf},
};

use crate::error::{ConmonError, ConmonResult};
use std::{
    ffi::OsStr,
    io,
    os::fd::{AsRawFd, FromRawFd},
    os::unix::ffi::OsStrExt,
};

use nix::{
    NixPath,
    errno::Errno,
    fcntl::{AT_FDCWD, OFlag, open},
    sys::{
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
    Notify, // Socket for sd-notify.
}

/// Remote side (attach client or sd-notify FD inside container).
#[derive(Debug)]
pub struct RemoteSocket {
    pub socket_type: SocketType,
    pub fd: OwnedFd,
    pub buf: [u8; 8192],
}

impl RemoteSocket {
    pub fn new(socket_type: SocketType, fd: OwnedFd) -> Self {
        Self {
            socket_type,
            fd,
            buf: [0u8; 8192],
        }
    }
}

/// Represents single UnixSocket.
#[derive(Default)]
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
                Err(e)
            }
        })?;

        // symlink(bundle_path, base_path)
        symlinkat(&self.bundle_path, AT_FDCWD, &new_base)?;

        Ok(new_base)
    }

    /// Accept new UnixSocket client (remote) connection.
    pub fn accept(&self) -> ConmonResult<Option<RemoteSocket>> {
        if self.fd.is_none() {
            return Ok(None);
        }

        match accept(self.fd.as_ref().unwrap().as_raw_fd()) {
            Ok(new_fd) => {
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
