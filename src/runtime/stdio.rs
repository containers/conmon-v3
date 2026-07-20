use crate::{
    error::{ConmonError, ConmonResult},
    logging::plugin::LogPlugin,
    unix_socket::{RemoteSocket, Socket, SocketType, UnixSocket},
};

use nix::{
    cmsg_space,
    errno::Errno,
    fcntl::OFlag,
    libc::{SHUT_RD, shutdown},
    poll::{PollFd, PollFlags, poll},
    sys::socket::{ControlMessageOwned, MsgFlags, SockaddrStorage, recvmsg},
    sys::wait::{Id, WaitPidFlag, WaitStatus, waitid},
    unistd::{Pid, pipe2, read},
};

use std::{
    io::{self, IoSliceMut},
    os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd},
    path::PathBuf,
    time::{Duration, Instant},
};

use log::{debug, info};

/// Maximum time to wait for the runtime to connect on `--console-socket` and send the pty fd.
const CONSOLE_SOCKET_WAIT_TIMEOUT: Duration = Duration::from_secs(60);
// Poll interval used while waiting for runtime connection / fd delivery.
const CONSOLE_SOCKET_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Creates new pipe and return read/write fds.
///
/// # Returns
///
/// * (read_fd, write_wf)
///
/// # Errors
///
/// * [`ConmonError`] on any error.
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

    Ok((rfd, wfd))
}

/// Reads data from fd and stores them in the buffer.
/// # Returns
///
/// * Number of bytes read.
///
/// # Arguments
///
/// * `fd` - The file descriptor to read the data from.
/// * `buf` - The buffer to write the data into.
///
/// # Errors
///
/// * [`ConmonError`] on any error.
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

/// Result of the `recv_data_and_fds` function.
pub struct RecvResult {
    /// The number of bytes read.
    n: usize,

    /// The file descriptors received.
    fds: Vec<RawFd>,
}

/// Receives data and file descriptors from existing file descriptor.
///
/// The file descriptors must be sent using the ScmRights Control message.
///
/// # Returns
///
/// * The `RecvResult` with number of bytes read and file descriptors received.
///
/// # Arguments
///
/// * `fd` - The file descriptor to read the data and fds from.
/// * `buf` - The buffer to write the data into.
///
/// # Errors
///
/// * [`ConmonError`] on any error.
fn recv_data_and_fds(fd: RawFd, buf: &mut [u8]) -> nix::Result<RecvResult> {
    let mut iov = [IoSliceMut::new(buf)];
    let mut cmsgspace = cmsg_space!([RawFd; 4]);

    let msg = recvmsg::<SockaddrStorage>(fd, &mut iov, Some(&mut cmsgspace), MsgFlags::empty())?;

    let mut fds = Vec::new();
    let rights = msg.cmsgs()?.next();
    if let Some(ControlMessageOwned::ScmRights(rights)) = rights {
        fds.extend(rights);
    }
    Ok(RecvResult { n: msg.bytes, fds })
}

/// Accepts the console_socket connection and returns the fd sent over it.
///
/// Polls the listen socket until the runtime connects or the runtime process exits.
/// When the runtime exits, any already-queued connection is drained before failing.
/// A timeout prevents conmon from blocking forever if the runtime never connects.
///
/// # Returns
///
/// * The `RemoteSocket` based on the file descriptor received over `console_socket`.
///
/// # Arguments
///
/// * `console_socket` - The `UnixSocket` to receive the console file-descriptor from.
/// * `runtime_pid` - PID of the runtime child (e.g. crun/runc) to detect early exit.
///
/// # Errors
///
/// * [`ConmonError`] on any error.
pub fn receive_console_fd(
    console_socket: UnixSocket,
    runtime_pid: i32,
) -> ConmonResult<RemoteSocket> {
    receive_console_fd_with_timeout(console_socket, runtime_pid, CONSOLE_SOCKET_WAIT_TIMEOUT)
}

fn receive_console_fd_with_timeout(
    console_socket: UnixSocket,
    runtime_pid: i32,
    timeout: Duration,
) -> ConmonResult<RemoteSocket> {
    let listen_fd = console_socket.fd().ok_or_else(|| {
        ConmonError::new(
            "Cannot receive console socket file descriptor without console socket.",
            1,
        )
    })?;

    let deadline = Instant::now() + timeout;

    // Phase 1: wait for and accept the console-socket connection.
    let remote = loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(ConmonError::new(
                "Timed out waiting for runtime to connect on console socket",
                1,
            ));
        }

        if let Some(remote) = accept_if_ready(
            &console_socket,
            listen_fd.as_fd(),
            deadline,
            CONSOLE_SOCKET_POLL_INTERVAL,
        )? {
            break remote;
        }

        if let Some(status) = runtime_exit_status(runtime_pid)? {
            // Important race handling: even after observing runtime exit, do one last
            // non-blocking accept attempt in case the connection is already queued.
            if let Some(remote) =
                accept_if_ready(&console_socket, listen_fd.as_fd(), deadline, Duration::ZERO)?
            {
                break remote;
            }

            return Err(ConmonError::new(
                format!("Runtime process exited with status {status} before sending console fd"),
                1,
            ));
        }
    };

    // Phase 2: wait for and receive the passed terminal fd.
    let conn_fd = &remote.fd;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(ConmonError::new(
                "Timed out waiting for console fd over console socket",
                1,
            ));
        }

        if !poll_until(conn_fd.as_fd(), deadline, CONSOLE_SOCKET_POLL_INTERVAL)? {
            continue;
        }

        let mut buf = [0u8; 1];
        match recv_data_and_fds(conn_fd.as_raw_fd(), &mut buf) {
            Ok(res) if res.n > 0 => {
                let Some(rfd) = res.fds.first() else {
                    return Err(ConmonError::new(
                        "No file descriptor received using console socket.",
                        1,
                    ));
                };

                debug!("Received console fd {}", rfd);
                let owned_fd = unsafe { OwnedFd::from_raw_fd(*rfd) };
                return Ok(RemoteSocket::new(SocketType::Terminal, owned_fd));
            }
            Ok(_) => {
                return Err(ConmonError::new(
                    "Console socket closed before file descriptor was received.",
                    1,
                ));
            }
            #[allow(unreachable_patterns)] // EAGAIN and EWOULDBLOCK are distinct on some platforms.
            Err(Errno::EAGAIN) | Err(Errno::EWOULDBLOCK) => continue,
            Err(e) => {
                return Err(ConmonError::new(
                    format!("Error receiving file descriptor using console socket: {e}"),
                    1,
                ));
            }
        }
    }
}

/// Checks whether the runtime child process has exited without blocking.
///
/// Uses non-blocking `waitid(WNOHANG | WNOWAIT | WEXITED)` so callers can detect
/// early runtime failure without reaping the child.
///
/// # Returns
///
/// * `Some(status)` when the runtime has exited or been signaled.
/// * `None` when the runtime is still running, or when no PID was provided.
///
/// # Arguments
///
/// * `runtime_pid` - PID of the runtime child (e.g. crun/runc), or a non-positive
///   value to skip the check.
///
/// # Errors
///
/// * [`ConmonError`] if `waitid` fails unexpectedly.
fn runtime_exit_status(runtime_pid: i32) -> ConmonResult<Option<i32>> {
    if runtime_pid <= 0 {
        return Ok(None);
    }

    let flags = WaitPidFlag::WEXITED | WaitPidFlag::WNOWAIT | WaitPidFlag::WNOHANG;

    loop {
        match waitid(Id::Pid(Pid::from_raw(runtime_pid)), flags) {
            Ok(WaitStatus::Exited(_, status)) => return Ok(Some(status)),
            Ok(WaitStatus::Signaled(_, sig, _)) => return Ok(Some(128 + sig as i32)),
            Ok(_) | Err(Errno::ECHILD) => return Ok(None),
            Err(Errno::EINTR) => continue,
            Err(e) => {
                return Err(ConmonError::new(
                    format!("waitid({runtime_pid}) failed: {e}"),
                    1,
                ));
            }
        }
    }
}

/// If `fd` becomes ready before `deadline`, try to `accept()` a console-socket
/// connection.
///
/// This is used to handle the case where the runtime may have connected
/// (and queued the connection) but conmon detects runtime exit between loop
/// iterations.
///
/// # Returns
///
/// * `Ok(Some(remote))` when a connection was accepted.
/// * `Ok(None)` when `fd` did not become ready within `max_wait` (bounded
///   by `deadline`).
///
/// # Errors
///
/// * [`ConmonError`] when `poll_until` indicates the socket is ready but
///   `accept()` returned `None`, or when `accept()` fails.
fn accept_if_ready(
    console_socket: &UnixSocket,
    fd: BorrowedFd<'_>,
    deadline: Instant,
    max_wait: Duration,
) -> ConmonResult<Option<RemoteSocket>> {
    if !poll_until(fd, deadline, max_wait)? {
        return Ok(None);
    }

    match console_socket.accept()? {
        Some(remote) => Ok(Some(remote)),
        None => Err(ConmonError::new(
            "Console socket ready but accept returned no connection",
            1,
        )),
    }
}

/// Polls `fd` until it becomes readable or the deadline expires.
///
/// Returns `true` for readiness events that indicate progress is possible
/// (readable, hangup, error) or the fd is no longer valid (POLLNVAL).
///
/// # Returns
///
/// * `Ok(true)` if the poll indicates readiness.
/// * `Ok(false)` if the deadline is reached without a readiness event.
///
/// # Errors
///
/// * [`ConmonError`] if `poll()` fails (other than EINTR, which is retried).
fn poll_until(fd: BorrowedFd<'_>, deadline: Instant, max_wait: Duration) -> ConmonResult<bool> {
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(false);
        }

        let wait = remaining.min(max_wait);
        let timeout_ms = wait.as_millis().min(u16::MAX as u128) as u16;

        let mut pollfds = [PollFd::new(fd, PollFlags::POLLIN)];
        match poll(&mut pollfds, timeout_ms) {
            Ok(0) => return Ok(false),
            Ok(_) => {
                if let Some(revents) = pollfds[0].revents() {
                    if revents.intersects(
                        PollFlags::POLLIN
                            | PollFlags::POLLHUP
                            | PollFlags::POLLERR
                            | PollFlags::POLLNVAL,
                    ) {
                        return Ok(true);
                    }
                }
                return Ok(false);
            }
            Err(Errno::EINTR) => continue,
            Err(e) => {
                return Err(ConmonError::new(
                    format!(
                        "poll() failed while waiting for console socket: {}",
                        io::Error::from_raw_os_error(e as i32)
                    ),
                    1,
                ));
            }
        }
    }
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
    notify_socket: Option<RemoteSocket>,
    notify_host_path: Option<PathBuf>,
    stdin_attached: bool,
    leave_stdin_open: bool,
    signal_fd: i32,
    mut idle_callback: F,
) -> ConmonResult<()>
where
    F: FnMut(bool) -> ConmonResult<bool>,
{
    debug!("Starting event loop");
    let mut sockets: Vec<Socket> = Vec::new();
    let mut new_sockets: Vec<RemoteSocket> = Vec::new();
    let mut fds: Vec<PollFd> = vec![];

    // Helpers containing fds for console, terminal and stdout, so we can easily
    // forward data to them.
    let mut console_fds = Vec::new();
    let mut terminal_fds = Vec::new();
    let mut stdout_fd: i32 = -1;

    // Optional attach socket.
    // WARN: The attach socket must be in `fds` before the stdout and stderr,
    // otherwise the stdout/stderr read is handled before the attach accept
    // callback and some data from stdout/stderr can be lost.
    if let Some(attach) = attach_socket {
        if let Some(fd) = attach.fd() {
            let borrowed = unsafe { BorrowedFd::borrow_raw(fd.as_raw_fd()) };
            fds.push(PollFd::new(borrowed, PollFlags::POLLIN));
        }
        sockets.push(Socket::Unix(attach));
    }

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

    // Optional systemd notify socket.
    if let Some(notify) = notify_socket {
        let borrowed = unsafe { BorrowedFd::borrow_raw(notify.fd.as_raw_fd()) };
        fds.push(PollFd::new(borrowed, PollFlags::POLLIN));
        sockets.push(Socket::Remote(notify));
    }

    // Signal fd to recieve UNIX signals.
    if signal_fd > 0 {
        info!("SignalFD: {}", signal_fd);
        let borrowed = unsafe { BorrowedFd::borrow_raw(signal_fd) };
        fds.push(PollFd::new(borrowed, PollFlags::POLLIN));
        sockets.push(Socket::Invalid());
    }

    // Main loop.
    // Iterates as long as we have some RemoteSocket to read from or
    // as long as `idle_callback` returns `true`.
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

        // We have no fd to read from, so execute the idle function.
        if n == 0 {
            let keep_running = idle_callback(false)?;
            if !keep_running {
                info!("idle_callback stopped the event loop.");
                return Ok(());
            }
            continue;
        }

        // We will mutate fds/remote_sockets, so iterate by index.
        let mut i = 0;
        while i < fds.len() {
            // The poll fd.
            let pfd = &fds[i];
            // If `false`, we close the socket completely.
            let mut keep_socket = true;
            // if `false`, we close the read side of the socket.
            let mut continue_reading = true;

            if let Some(revents) = pfd.revents() {
                if revents.contains(PollFlags::POLLIN) {
                    // If the POLLIN comes from the signal fd, run the idle_callback to handle
                    // the received signal.
                    if pfd.as_fd().as_raw_fd() == signal_fd {
                        idle_callback(true)?;
                        i += 1;
                        continue;
                    }

                    // Handle the received data.
                    continue_reading = sockets[i].handle_data(
                        log_plugin,
                        &mut new_sockets,
                        workerfd_stdin.as_ref(),
                        &console_fds,
                        &terminal_fds,
                        stdout_fd,
                        &notify_host_path,
                    )?;

                    // Add new sockets to `sockets` and `fds`.
                    // This happens when `attach` accepts new connection in the `handle_data`.
                    if !new_sockets.is_empty() {
                        while !new_sockets.is_empty() {
                            let new_socket = new_sockets.pop();
                            info!("Adding {:?} into poll fds", new_socket);
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
                    // On HUP, close the socket.
                    debug!("HUP on {:?}", pfd);
                    keep_socket = false;
                }
            }

            if !continue_reading {
                // Close the read part of the socket.
                debug!("Shutdown {:?}", fds[i].as_fd().as_raw_fd());
                unsafe { shutdown(fds[i].as_fd().as_raw_fd(), SHUT_RD) };

                // Remove it from fds we call poll for.
                fds[i].set_events(PollFlags::empty());

                if let Socket::Remote(r) = &sockets[i] {
                    if r.socket_type == SocketType::Console && stdin_attached {
                        // We closed the Console socket attached to container's stdin.
                        // This normally means we also close the container's stdin, unless
                        // the called instructed us no to do it using the `--leave-stdin-open`.
                        // console_fds.retain(|&x| x != r.fd.as_raw_fd());
                        if !leave_stdin_open {
                            // This closes the socket, since it moves out of scope.
                            workerfd_stdin.take();
                        }
                    } else if r.socket_type == SocketType::Terminal {
                        terminal_fds.retain(|&x| x != r.fd.as_raw_fd());
                    }
                }
            }

            if keep_socket {
                // Go to next socket in case we want to keep this one.
                i += 1;
            } else {
                // Remove the fd completely.
                let socket = sockets.swap_remove(i);
                info!("Removing socket {:?}", socket);
                fds.swap_remove(i);

                // Do NOT increment the `i`, since it now points to swapped fd.
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::unix_socket::{SocketType, UnixSocket};
    use nix::sys::socket::{
        AddressFamily, ControlMessage, SockFlag, SockType, sendmsg, socketpair,
    };
    use nix::sys::stat::Mode;
    use nix::sys::wait::{Id, WaitPidFlag, WaitStatus, waitid, waitpid};
    use nix::unistd::{ForkResult, fork};
    use std::io::IoSlice;
    use std::os::unix::net::UnixStream;
    use std::path::PathBuf;
    use std::thread;
    use std::time::Duration;

    fn wait_exited_nowait(child: Pid) -> ConmonResult<()> {
        let wait_flags = WaitPidFlag::WEXITED | WaitPidFlag::WNOWAIT | WaitPidFlag::WNOHANG;
        loop {
            match waitid(Id::Pid(child), wait_flags)? {
                WaitStatus::Exited(_, _) | WaitStatus::Signaled(_, _, _) => return Ok(()),
                WaitStatus::StillAlive => thread::sleep(Duration::from_millis(10)),
                _ => thread::sleep(Duration::from_millis(10)),
            }
        }
    }

    fn spawn_send_terminal_fd(listen_path: PathBuf) -> thread::JoinHandle<()> {
        thread::spawn(move || {
            let client = UnixStream::connect(listen_path).unwrap();
            let (r, w) = pipe2(OFlag::O_CLOEXEC).unwrap();
            drop(w);
            let iov = [IoSlice::new(b"x")];
            let cmsg = ControlMessage::ScmRights(&[r.as_raw_fd()]);
            sendmsg::<()>(client.as_raw_fd(), &iov, &[cmsg], MsgFlags::empty(), None).unwrap();
        })
    }

    fn test_console_socket() -> ConmonResult<UnixSocket> {
        let mut console_socket = UnixSocket::new(
            SocketType::Terminal,
            false,
            PathBuf::from("/tmp"),
            None,
            None,
        );
        console_socket.bind(
            None,
            SockType::Stream,
            SockFlag::SOCK_CLOEXEC,
            Mode::from_bits_truncate(0o700),
        )?;
        console_socket.listen()?;
        Ok(console_socket)
    }

    fn send_fds(count: usize, payload: &[u8]) -> ConmonResult<(OwnedFd, Vec<(OwnedFd, OwnedFd)>)> {
        let (sender, receiver) = socketpair(
            AddressFamily::Unix,
            SockType::Stream,
            None,
            SockFlag::empty(),
        )?;

        let mut keepalive = Vec::new();
        let mut fds_to_send: Vec<RawFd> = Vec::new();
        for _ in 0..count {
            let (r, w) = pipe2(OFlag::O_CLOEXEC)?;
            fds_to_send.push(r.as_raw_fd());
            keepalive.push((r, w));
        }

        let iov = [IoSlice::new(payload)];
        let cmsg = ControlMessage::ScmRights(&fds_to_send);
        sendmsg::<()>(sender.as_raw_fd(), &iov, &[cmsg], MsgFlags::empty(), None)?;

        Ok((receiver, keepalive))
    }

    #[test]
    fn recv_data() -> ConmonResult<()> {
        let payload = b"foo";
        let (receiver, _keepalive) = send_fds(1, payload)?;

        let mut buf = [0u8; 16];
        let res = recv_data_and_fds(receiver.as_raw_fd(), &mut buf)?;
        assert_eq!(res.n, payload.len());
        assert_eq!(&buf[..payload.len()], payload);
        assert_eq!(res.fds.len(), 1);
        Ok(())
    }

    /// `recv_data_and_fds` only expects at max 4 fds in the `SCM_RIGHTS` control message.
    #[test]
    fn recv_data_too_many_scm_rights() -> ConmonResult<()> {
        let (receiver, _keepalive) = send_fds(5, b"foo")?;

        let mut buf = [0u8; 16];
        let recv = recv_data_and_fds(receiver.as_raw_fd(), &mut buf);
        assert_eq!(recv.err(), Some(Errno::ENOBUFS));
        Ok(())
    }

    #[test]
    fn runtime_exit_status_does_not_reap_child() -> ConmonResult<()> {
        match unsafe { fork() }? {
            ForkResult::Parent { child } => {
                wait_exited_nowait(child)?;

                let status = runtime_exit_status(child.as_raw())?;
                assert_eq!(status, Some(42));

                match waitpid(child, None)? {
                    WaitStatus::Exited(pid, code) => {
                        assert_eq!(pid, child);
                        assert_eq!(code, 42);
                    }
                    other => panic!("unexpected wait status: {other:?}"),
                }
            }
            ForkResult::Child => std::process::exit(42),
        }
        Ok(())
    }

    #[test]
    fn receive_console_fd_times_out_without_connection() -> ConmonResult<()> {
        let console_socket = test_console_socket()?;
        let result =
            receive_console_fd_with_timeout(console_socket, -1, Duration::from_millis(200));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.msg.contains("Timed out waiting for runtime"));
        Ok(())
    }

    #[test]
    fn receive_console_fd_gets_passed_fd() -> ConmonResult<()> {
        let console_socket = test_console_socket()?;
        let listen_path = console_socket.path().unwrap().clone();

        let server = spawn_send_terminal_fd(listen_path);

        let terminal = receive_console_fd_with_timeout(console_socket, -1, Duration::from_secs(5))?;
        assert_eq!(terminal.socket_type, SocketType::Terminal);
        server.join().unwrap();
        Ok(())
    }

    #[test]
    fn receive_console_fd_accepts_queued_connection_after_runtime_exit() -> ConmonResult<()> {
        let console_socket = test_console_socket()?;
        let listen_path = console_socket.path().unwrap().clone();

        match unsafe { fork() }? {
            ForkResult::Parent { child } => {
                wait_exited_nowait(child)?;

                let sender = spawn_send_terminal_fd(listen_path);
                sender.join().unwrap();

                let terminal = receive_console_fd_with_timeout(
                    console_socket,
                    child.as_raw(),
                    Duration::from_secs(5),
                )?;
                assert_eq!(terminal.socket_type, SocketType::Terminal);
                let status = waitpid(child, None)?;
                assert_eq!(status, WaitStatus::Exited(child, 42));
            }
            ForkResult::Child => {
                std::process::exit(42);
            }
        }
        Ok(())
    }

    #[test]
    fn receive_console_fd_fails_when_runtime_exits() -> ConmonResult<()> {
        let console_socket = test_console_socket()?;

        match unsafe { fork() }? {
            ForkResult::Parent { child } => {
                let result = receive_console_fd_with_timeout(
                    console_socket,
                    child.as_raw(),
                    Duration::from_secs(5),
                );
                let _ = waitpid(child, None);
                assert!(result.is_err());
                let err = result.unwrap_err();
                assert!(err.msg.contains("Runtime process exited"));
            }
            ForkResult::Child => {
                std::process::exit(42);
            }
        }
        Ok(())
    }
}
