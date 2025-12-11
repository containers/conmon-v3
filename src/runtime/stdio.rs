use crate::{
    error::{ConmonError, ConmonResult},
    logging::plugin::LogPlugin,
    unix_socket::{RemoteSocket, Socket, SocketType, UnixSocket},
};

use nix::{
    cmsg_space,
    errno::Errno,
    fcntl::OFlag,
    poll::{PollFd, PollFlags, poll},
    sys::socket::{ControlMessageOwned, MsgFlags, SockaddrStorage, recvmsg},
    unistd::{pipe2, read},
};

use std::{
    io::{self, IoSliceMut},
    os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd},
};

use log::{debug, info};

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

pub struct RecvResult {
    n: usize,
    fds: Vec<RawFd>,
}

/// Helper function to receive data from `fd`, store them in `buf` and
/// returns the the lentgh of data read as well as additional fds sent
/// together with the data.
fn recv_data_and_fds(fd: RawFd, buf: &mut [u8]) -> nix::Result<RecvResult> {
    let mut iov = [IoSliceMut::new(buf)];
    let mut cmsgspace = cmsg_space!([RawFd; 4]);

    let msg = recvmsg::<SockaddrStorage>(fd, &mut iov, Some(&mut cmsgspace), MsgFlags::empty())?;

    let mut fds = Vec::new();
    let rights = msg.cmsgs().unwrap().next();
    if let Some(ControlMessageOwned::ScmRights(rights)) = rights {
        fds.extend(rights);
    }
    Ok(RecvResult { n: msg.bytes, fds })
}

/// Helper function to accept `console_socket` connection and return the fd
/// sent byt the connection.
pub fn receive_console_fd(console_socket: UnixSocket) -> ConmonResult<RemoteSocket> {
    let remote = console_socket.accept()?;
    if let Some(r) = remote {
        let mut buf = [0u8; 8192];
        match recv_data_and_fds(r.fd.as_raw_fd(), &mut buf) {
            Ok(res) => {
                let n = res.n;
                if n > 0 {
                    let received_fd = res.fds.first();
                    if let Some(rfd) = received_fd {
                        debug!("Received fd {}", rfd);
                        let owned_fd = unsafe { OwnedFd::from_raw_fd(*rfd) };
                        let ret = RemoteSocket::new(SocketType::Terminal, owned_fd);
                        return Ok(ret);
                    }
                } else {
                    return Err(ConmonError::new(
                        "No file descriptor received using console socket.",
                        1,
                    ));
                }
            }
            Err(e) => {
                return Err(ConmonError::new(
                    format!(
                        "Error receiving file descriptor using console socket: {}",
                        e
                    ),
                    1,
                ));
            }
        }
    }
    Err(ConmonError::new(
        "Cannot receive console socket file descriptor without console socket.",
        1,
    ))
}

/// Handles incomming data on fds and forwards them to right destination.
/// This function blocks until the container is running.
/// # Arguments
///
/// * `log_plugin` - plugin to which the container logs are forwarded into.
/// * `mainfd_stdout` - fd from which the container's stdout is read from.
/// * `mainfd_stderr` - fd from which the container's stderr is read from.
/// * `workerfd_stdin` - fd into which the container's stdin is written.
/// * `attach_socket` - socket for `attach` connections.
/// * `terminal_socket` - terminal socket create by runtime in case of `--terminal`.
/// * `ctl_fifo` - Remote socket for `ctl` fifo.
/// * `winsz_fifo` - Remote socket for `winsz` fifo.
/// * `leave_stdin_open` - Whether to keep stdin open attach client disconnects.
/// * `idle_callback` - function executed periodically during the event-loop.
#[allow(clippy::too_many_arguments)]
pub fn handle_stdio<F>(
    log_plugin: &mut dyn LogPlugin,
    mut mainfd_stdout: Option<OwnedFd>,
    mainfd_stderr: OwnedFd,
    mut workerfd_stdin: Option<OwnedFd>,
    attach_socket: Option<UnixSocket>,
    terminal_socket: Option<RemoteSocket>,
    ctl_fifo: Option<RemoteSocket>,
    winsz_fifo: Option<RemoteSocket>,
    oom_socket: Option<RemoteSocket>,
    leave_stdin_open: bool,
    mut idle_callback: F,
) -> ConmonResult<()>
where
    F: FnMut() -> ConmonResult<bool>,
{
    debug!("Starting event loop");
    let mut sockets: Vec<Socket> = Vec::new();
    let mut new_sockets: Vec<RemoteSocket> = Vec::new();
    let mut fds: Vec<PollFd> = vec![];

    // Helpers containing fds for console and terminal, so we can easily
    // forward data to them
    let mut console_fds = Vec::new();
    let mut terminal_fds = Vec::new();
    let mut stdout_fd: i32 = -1;

    // Container's stdout.
    if let Some(stdout) = mainfd_stdout.take() {
        stdout_fd = stdout.as_raw_fd();
        let borrowed = unsafe { BorrowedFd::borrow_raw(stdout.as_raw_fd()) };
        fds.push(PollFd::new(borrowed, PollFlags::POLLIN));
        sockets.push(Socket::Remote(RemoteSocket::new(
            SocketType::Stdout,
            stdout,
        )));
    }

    // Container's stderr.
    let borrowed = unsafe { BorrowedFd::borrow_raw(mainfd_stderr.as_raw_fd()) };
    fds.push(PollFd::new(borrowed, PollFlags::POLLIN));
    sockets.push(Socket::Remote(RemoteSocket::new(
        SocketType::Stderr,
        mainfd_stderr,
    )));

    // Optional attach socket.
    if let Some(attach) = attach_socket {
        if let Some(fd) = attach.fd() {
            let borrowed = unsafe { BorrowedFd::borrow_raw(fd.as_raw_fd()) };
            fds.push(PollFd::new(borrowed, PollFlags::POLLIN));
        }
        sockets.push(Socket::Unix(attach));
    }

    // Optional terminal socket.
    if let Some(terminal) = terminal_socket {
        stdout_fd = terminal.fd.as_raw_fd();
        terminal_fds.push(terminal.fd.as_raw_fd());
        let borrowed = unsafe { BorrowedFd::borrow_raw(terminal.fd.as_raw_fd()) };
        fds.push(PollFd::new(borrowed, PollFlags::POLLIN));
        sockets.push(Socket::Remote(terminal));
    }

    // Optional ctl fifo.
    if let Some(ctl) = ctl_fifo {
        let borrowed = unsafe { BorrowedFd::borrow_raw(ctl.fd.as_raw_fd()) };
        fds.push(PollFd::new(borrowed, PollFlags::POLLIN));
        sockets.push(Socket::Remote(ctl));
    }

    // Optional winsz fifo.
    if let Some(winsz) = winsz_fifo {
        let borrowed = unsafe { BorrowedFd::borrow_raw(winsz.fd.as_raw_fd()) };
        fds.push(PollFd::new(borrowed, PollFlags::POLLIN));
        sockets.push(Socket::Remote(winsz));
    }

    // Optional OOM socket.
    if let Some(oom) = oom_socket {
        let borrowed = unsafe { BorrowedFd::borrow_raw(oom.fd.as_raw_fd()) };
        fds.push(PollFd::new(borrowed, PollFlags::POLLIN));
        sockets.push(Socket::Remote(oom));
    }

    // Main loop.
    // Iterates as long as we have some RemoteSocket to read from.
    while sockets.iter().any(|s| matches!(s, Socket::Remote(_))) {
        // Run poll to get informed about new fd events.
        let n = poll(&mut fds, 10_u16).map_err(|e| {
            ConmonError::new(
                format!(
                    "handle_stdio poll() failed: {}",
                    io::Error::from_raw_os_error(e as i32)
                ),
                1,
            )
        })?;

        // Execute the idle function.
        if n == 0 {
            let keep_running = idle_callback()?;
            if !keep_running {
                info!("idle_callback stopped the event loop.");
                return Ok(());
            }
            continue;
        }

        // We will mutate fds/remote_sockets, so iterate by index.
        let mut i = 0;
        while i < fds.len() {
            let pfd = &fds[i];
            let mut keep_socket = true;

            if let Some(revents) = pfd.revents() {
                if revents.contains(PollFlags::POLLIN) {
                    // Handle the received data.
                    keep_socket = sockets[i].handle_data(
                        log_plugin,
                        &mut new_sockets,
                        workerfd_stdin.as_ref(),
                        &console_fds,
                        &terminal_fds,
                        stdout_fd,
                    )?;

                    // Add new sockets to `sockets` and `fds`.
                    // This happens when `attach` accepts new connection.
                    if !new_sockets.is_empty() {
                        while !new_sockets.is_empty() {
                            let new_socket = new_sockets.pop();
                            if let Some(n_s) = new_socket {
                                if n_s.socket_type == SocketType::Console {
                                    console_fds.push(n_s.fd.as_raw_fd());
                                }
                                let borrowed =
                                    unsafe { BorrowedFd::borrow_raw(n_s.fd.as_raw_fd()) };
                                fds.push(PollFd::new(borrowed, PollFlags::POLLIN));
                                sockets.push(Socket::Remote(n_s));
                            }
                        }
                        new_sockets.clear();
                    }
                } else if revents.contains(PollFlags::POLLHUP) {
                    // One HUP, close the socket.
                    debug!("HUP on {:?}", pfd);
                    keep_socket = false;
                }
            }

            if keep_socket {
                // Go to next socket in case we want to keep this one.
                i += 1;
            } else {
                // Remove the socket.
                let socket = sockets.swap_remove(i);
                fds.swap_remove(i);
                if let Socket::Remote(r) = socket {
                    if r.socket_type == SocketType::Console {
                        console_fds.retain(|&x| x != r.fd.as_raw_fd());
                        if !leave_stdin_open {
                            // This closes the socket, since moves out of scope.
                            workerfd_stdin.take();
                        }
                    } else if r.socket_type == SocketType::Terminal {
                        terminal_fds.retain(|&x| x != r.fd.as_raw_fd());
                    }
                }
                // Do NOT increment the `i`, since it now points to swapped socket.
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mockall::{
        mock,
        predicate::{self, *},
    };
    use nix::unistd::write as nix_write;

    use std::os::{fd::AsFd, unix::net::UnixStream};
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

    fn dummy_idle_callback() -> ConmonResult<bool> {
        Ok(true)
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
        handle_stdio(
            &mut mock,
            Some(r_out),
            r_err,
            None,
            None,
            None,
            None,
            None,
            None,
            false,
            dummy_idle_callback,
        )?;
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
        handle_stdio(
            &mut mock,
            Some(r_out),
            r_err,
            None,
            None,
            None,
            None,
            None,
            None,
            false,
            dummy_idle_callback,
        )?;
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
            Some(r_out),
            r_err,
            Some(w_in),
            Some(attach),
            None,
            None,
            None,
            None,
            leave_open,
            dummy_idle_callback,
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
