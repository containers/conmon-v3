use crate::{
    error::{ConmonError, ConmonResult},
    logging::plugin::LogPlugin,
    unix_socket::{RemoteSocket, SocketType, UnixSocket},
};

use nix::{
    errno::Errno,
    fcntl::OFlag,
    poll::{PollFd, PollFlags, PollTimeout, poll},
    sys::socket::{SockaddrStorage, recvfrom},
    unistd::{pipe2, read, write},
};

use std::{
    io,
    os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd},
};

// Creates new pipe and return read/write fds.
pub fn create_pipe() -> ConmonResult<(OwnedFd, OwnedFd)> {
    let (rfd, wfd) = pipe2(OFlag::O_CLOEXEC).map_err(|e| {
        ConmonError::new(
            format!(
                "Failed to create pipe: {}",
                io::Error::from_raw_os_error(e as i32)
            ),
            1,
        )
    })?;

    // SAFETY: rfd/wfd are newly created FDs we now own.
    Ok((rfd, wfd))
}

/// Reads data from fd and stores them in the buffer.
/// Returns the number of bytes read.
pub fn read_pipe(fd: &OwnedFd, buf: &mut [u8]) -> ConmonResult<usize> {
    loop {
        match read(fd, buf) {
            Ok(n) => return Ok(n),
            Err(Errno::EINTR) | Err(Errno::EAGAIN) => continue,
            Err(e) => {
                return Err(ConmonError::new(
                    format!("read() failed while reading pipe: {e}"),
                    1,
                ));
            }
        }
    }
}

/// Handles incomming data on fds and forwards them to right destination.
/// This function blocks until the container is running.
pub fn handle_stdio(
    log_plugin: &mut dyn LogPlugin,
    mainfd_stdout: &OwnedFd,
    mainfd_stderr: &OwnedFd,
    mut workerfd_stdin: Option<OwnedFd>,
    attach_socket: Option<&UnixSocket>,
) -> ConmonResult<()> {
    let mut buf = [0u8; 8192];
    let mut remote_sockets: Vec<RemoteSocket> = Vec::new();

    // Container's stdout and stderr.
    let mut fds: Vec<PollFd> = vec![
        PollFd::new(mainfd_stdout.as_fd(), PollFlags::POLLIN), // index 0
        PollFd::new(mainfd_stderr.as_fd(), PollFlags::POLLIN), // index 1
    ];
    let stdout_fd = mainfd_stdout.as_raw_fd();
    let stderr_fd = mainfd_stderr.as_raw_fd();

    // Optional attach socket.
    let mut attach_index: Option<usize> = None;
    #[allow(clippy::collapsible_if)]
    if let Some(attach) = attach_socket.as_ref() {
        if let Some(fd) = attach.fd() {
            let idx = fds.len();
            fds.push(PollFd::new(fd.as_fd(), PollFlags::POLLIN));
            attach_index = Some(idx);
        }
    }

    // All RemoteSockets live at indices >= remote_base in `fds`
    let remote_base = fds.len(); // constant for the lifetime of this function

    loop {
        // Run poll to get informed about new fd events.
        let n = poll(&mut fds, PollTimeout::NONE).map_err(|e| {
            ConmonError::new(
                format!(
                    "handle_stdio poll() failed: {}",
                    io::Error::from_raw_os_error(e as i32)
                ),
                1,
            )
        })?;

        if n == 0 {
            // This should not happen, since we use PollTimeout::NONE, but
            // be defensive.
            continue;
        }

        // We will mutate fds/remote_sockets, so iterate by index.
        let mut i = 0;
        while i < fds.len() {
            let pfd = &fds[i];

            if let Some(revents) = pfd.revents() {
                let fd = pfd.as_fd().as_raw_fd();
                if revents.contains(PollFlags::POLLIN) {
                    // 1) attach socket ready: accept new RemoteSocket, extend vectors
                    if Some(i) == attach_index {
                        #[allow(clippy::collapsible_if)]
                        if let Some(attach) = attach_socket.as_ref() {
                            if let Some(remote) = attach.accept()? {
                                // add RemoteSocket to `remote_sockets` and its fd to `fds`.
                                let raw = remote.fd.as_raw_fd();
                                remote_sockets.push(remote);
                                let borrowed = unsafe { BorrowedFd::borrow_raw(raw) };
                                fds.push(PollFd::new(borrowed, PollFlags::POLLIN));
                            }
                        }
                        // Go to next fd.
                        i += 1;
                        continue;
                    }

                    // 2) stdout / stderr ready: read the data and forward to logs.
                    if fd == stdout_fd || fd == stderr_fd {
                        match read(pfd.as_fd(), &mut buf) {
                            Ok(n) => {
                                if n > 0 {
                                    let is_stdout = fd == stdout_fd;
                                    let _ = log_plugin.write(is_stdout, &buf[..n]);
                                } else {
                                    // EOF
                                    return Ok(());
                                }
                            }
                            Err(err) => {
                                if err == Errno::EWOULDBLOCK || err == Errno::EAGAIN {
                                    // nothing more to read right now, jump to next fd
                                    i += 1;
                                    continue;
                                }
                                return Err(ConmonError::new(
                                    format!(
                                        "handle_stdio read() failed: {}",
                                        io::Error::from_raw_os_error(err as i32)
                                    ),
                                    1,
                                ));
                            }
                        }
                        i += 1;
                        continue;
                    }

                    // 3) RemoteSocket ready: read it and forward to the right destination.
                    if i >= remote_base {
                        let remote_idx = i - remote_base; // index in remote_sockets
                        match recvfrom::<SockaddrStorage>(pfd.as_fd().as_raw_fd(), &mut buf) {
                            Ok((n, _sockaddr)) => {
                                if n > 0 {
                                    match remote_sockets[remote_idx].socket_type {
                                        SocketType::Console => {
                                            // Console socket: forward data to container's stdin.
                                            if let Some(workerfd_stdin) = workerfd_stdin.as_ref() {
                                                write(workerfd_stdin, &buf[..n])?;
                                            }
                                        }
                                        SocketType::Notify => {
                                            // handle data coming from this remote
                                        }
                                    }
                                    // Go to next fd.
                                    i += 1;
                                } else {
                                    // EOF: drop this remote socket
                                    let remote = remote_sockets.swap_remove(remote_idx);
                                    fds.swap_remove(i);
                                    #[allow(clippy::collapsible_if)]
                                    if remote.socket_type == SocketType::Console {
                                        if let Some(workerfd_stdin) = workerfd_stdin.take() {
                                            drop(workerfd_stdin);
                                        }
                                    }
                                    // do NOT increment i: we need to process the fd that got swapped in
                                }
                            }
                            Err(err) => {
                                if err == Errno::EWOULDBLOCK || err == Errno::EAGAIN {
                                    i += 1;
                                } else {
                                    // treat as fatal for this remote: remove it
                                    let remote = remote_sockets.swap_remove(remote_idx);
                                    fds.swap_remove(i);
                                    #[allow(clippy::collapsible_if)]
                                    if remote.socket_type == SocketType::Console {
                                        if let Some(workerfd_stdin) = workerfd_stdin.take() {
                                            drop(workerfd_stdin);
                                        }
                                    }
                                }
                            }
                        }
                        continue;
                    }
                } else if revents.contains(PollFlags::POLLHUP) {
                    return Ok(());
                }
            }

            i += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mockall::{
        mock,
        predicate::{self, *},
    };
    use nix::unistd::write as nix_write;

    mock! {
        pub LogPlugin {}
        impl crate::logging::plugin::LogPlugin for LogPlugin {
            fn write(&mut self, is_stdout: bool, data: &[u8]) -> ConmonResult<()>;
        }
    }

    #[test]
    fn handle_stdio_calls_write_for_stdout_and_stderr() -> ConmonResult<()> {
        let mut mock = MockLogPlugin::new();
        mock.expect_write()
            .with(
                eq(true),
                predicate::function(|bytes: &[u8]| bytes.windows(3).any(|w| w == b"OUT")),
            )
            .returning(|_, _| Ok(()));
        mock.expect_write()
            .with(
                eq(false),
                predicate::function(|bytes: &[u8]| bytes.windows(3).any(|w| w == b"ERR")),
            )
            .returning(|_, _| Ok(()));

        let (r_out, w_out) = create_pipe()?;
        let (r_err, w_err) = create_pipe()?;
        nix_write(w_out.as_fd(), b"OUT\n").expect("write failed");
        nix_write(w_err.as_fd(), b"ERR\n").expect("write failed");
        drop(w_out);
        drop(w_err);

        // Read from the other side of pipes.
        handle_stdio(&mut mock, &r_out, &r_err, None, None)?;
        Ok(())
    }

    #[test]
    fn handle_stdio_write_error_ignored() -> ConmonResult<()> {
        let mut mock = MockLogPlugin::new();
        mock.expect_write()
            .with(
                eq(true),
                predicate::function(|bytes: &[u8]| bytes.windows(4).any(|w| w == b"OUT1")),
            )
            .returning(|_, _| Ok(()));
        mock.expect_write()
            .with(
                eq(false),
                predicate::function(|bytes: &[u8]| bytes.windows(3).any(|w| w == b"ERR")),
            )
            .returning(|_, _| Err(ConmonError::new("err write fail", 1)));
        mock.expect_write()
            .with(
                eq(true),
                predicate::function(|bytes: &[u8]| bytes.windows(4).any(|w| w == b"OUT2")),
            )
            .returning(|_, _| Ok(()));

        let (r_out, w_out) = create_pipe()?;
        let (r_err, w_err) = create_pipe()?;
        nix_write(w_out.as_fd(), b"OUT1\n").expect("write failed");
        nix_write(w_err.as_fd(), b"ERR\n").expect("write failed");
        nix_write(w_out.as_fd(), b"OUT2\n").expect("write failed");
        drop(w_out);
        drop(w_err);

        // Read from the other side of pipes.
        handle_stdio(&mut mock, &r_out, &r_err, None, None)?;
        Ok(())
    }
}
