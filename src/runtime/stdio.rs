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
    leave_stdin_open: bool,
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
                                    if remote.socket_type == SocketType::Console
                                        && !leave_stdin_open
                                    {
                                        // This closes the socket, since moves out of scope.
                                        workerfd_stdin.take();
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
                                    if remote.socket_type == SocketType::Console
                                        && !leave_stdin_open
                                    {
                                        // This closes the socket, since moves out of scope.
                                        workerfd_stdin.take();
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

    use std::os::unix::net::UnixStream;
    use std::time::Duration;
    use std::{io::Write as _, path::PathBuf};

    use nix::sys::{
        socket::{SockFlag, SockType},
        stat::Mode,
    };

    use tempfile::TempDir;

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
        handle_stdio(&mut mock, &r_out, &r_err, None, None, false)?;
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
        handle_stdio(&mut mock, &r_out, &r_err, None, None, false)?;
        Ok(())
    }

    struct TestLog;

    impl crate::logging::plugin::LogPlugin for TestLog {
        fn write(&mut self, _is_stdout: bool, _data: &[u8]) -> ConmonResult<()> {
            Ok(())
        }
    }

    /// Helper that drives handle_stdio with an attach socket and two console clients.
    ///
    /// If `leave_open` is true, data from both clients should reach container stdin.
    /// If false, only data from the first client should be forwarded (stdin closed on first EOF).
    fn run_leave_stdin_open_scenario(leave_open: bool) -> ConmonResult<Vec<u8>> {
        // Container stdout/stderr pipes.
        let (r_out, w_out) = create_pipe()?;
        let (r_err, w_err) = create_pipe()?;

        // Container stdin pipe: w_in is what handle_stdio writes to.
        let (r_in, w_in) = create_pipe()?;

        // Prepare attach Unix socket.
        let tmpdir = TempDir::new()?;
        let socket_path = tmpdir.path().join("attach.sock");

        let mut attach = UnixSocket::new(
            SocketType::Console,
            true,
            PathBuf::from(tmpdir.path()),
            None,
            None,
        );
        attach
            .listen(
                Some(socket_path.clone()),
                SockType::Stream,
                SockFlag::SOCK_NONBLOCK,
                Mode::from_bits_truncate(0o600),
            )
            .expect("listen failed");

        // Spawn a thread to simulate attach clients and send stdout/stderr data.
        let socket_path_for_thread = socket_path.clone();
        let client_thread = std::thread::spawn(move || {
            // Give handle_stdio a moment to start and enter poll().
            std::thread::sleep(Duration::from_millis(500));

            // First console client – should always be forwarded.
            let mut c1 =
                UnixStream::connect(&socket_path_for_thread).expect("failed to connect client1");
            c1.write_all(b"CLIENT1\n").expect("write client1 failed");
            drop(c1); // EOF

            // Allow time for server to process EOF and, if !leave_open, close stdin.
            std::thread::sleep(Duration::from_millis(100));

            // Second console client – only forwarded if leave_stdin_open == true.
            let mut c2 =
                UnixStream::connect(&socket_path_for_thread).expect("failed to connect client2");
            c2.write_all(b"CLIENT2\n").expect("write client2 failed");
            drop(c2); // EOF

            // Also send something to stdout/stderr so handle_stdio can eventually exit.
            nix_write(w_out.as_fd(), b"OUT\n").ok();
            nix_write(w_err.as_fd(), b"ERR\n").ok();

            // Close writers to produce EOF on stdout/stderr.
            drop(w_out);
            drop(w_err);
        });

        // Run handle_stdio in the main thread.
        let mut logger = TestLog;
        handle_stdio(
            &mut logger,
            &r_out,
            &r_err,
            Some(w_in),
            Some(&attach),
            leave_open,
        )?;

        // Wait for client thread to finish.
        client_thread.join().expect("client thread panicked");

        // Read everything that reached "container stdin" (r_in).
        let mut collected = Vec::new();
        let mut buf = [0u8; 1024];
        loop {
            match read(r_in.as_fd(), &mut buf) {
                Ok(0) => break,
                Ok(n) => collected.extend_from_slice(&buf[..n]),
                Err(Errno::EINTR) | Err(Errno::EAGAIN) => continue,
                Err(e) => panic!("read from container stdin failed: {e}"),
            }
        }

        Ok(collected)
    }

    #[test]
    fn handle_stdio_leave_stdin_open_true() -> ConmonResult<()> {
        let data = run_leave_stdin_open_scenario(true)?;

        let s = String::from_utf8_lossy(&data);
        assert!(
            s.contains("CLIENT1"),
            "stdin data did not contain CLIENT1: {s:?}"
        );
        assert!(
            s.contains("CLIENT2"),
            "stdin data did not contain CLIENT2 even though leave_stdin_open=true: {s:?}"
        );
        Ok(())
    }

    #[test]
    fn handle_stdio_leave_stdin_open_false() -> ConmonResult<()> {
        let data = run_leave_stdin_open_scenario(false)?;

        let s = String::from_utf8_lossy(&data);
        assert!(
            s.contains("CLIENT1"),
            "stdin data did not contain CLIENT1: {s:?}"
        );
        assert!(
            !s.contains("CLIENT2"),
            "stdin data unexpectedly contained CLIENT2 even though leave_stdin_open=false: {s:?}"
        );
        Ok(())
    }
}
