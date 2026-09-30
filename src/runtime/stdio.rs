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
    sys::signalfd::SignalFd,
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
const CONSOLE_SOCKET_POLL_INTERVAL: Duration = Duration::from_millis(500);
/// Max SCM_RIGHTS descriptors accepted in one `recvmsg`.
const MAX_SCM_RIGHTS_FDS: usize = 4;

/// Delay before re-arming terminal PTY reads after temporary HUP/EIO.
/// Matches conmon-v2's 100ms `tty_hup_timeout_cb` re-add.
const TERMINAL_HUP_RETRY: Duration = Duration::from_millis(100);
/// Event-loop `poll` timeout. Short enough that a 100ms terminal retry is
/// noticed within one tick when the deadline is checked each iteration.
const STDIO_POLL_MS: u16 = 10;

fn is_terminal_remote(socket: &Socket) -> bool {
    matches!(
        socket,
        Socket::Remote(r) if r.socket_type == SocketType::Terminal
    )
}

/// Re-arm terminal sockets paused after HUP/EIO once `resume_at` is due.
///
/// Must run on every event-loop iteration (not only when `poll` returns 0);
/// another readable fd can otherwise keep the loop busy and strand the PTY.
fn resume_paused_terminals(sockets: &mut [Socket], resume_at: &mut Option<Instant>) {
    let Some(when) = *resume_at else {
        return;
    };
    if Instant::now() < when {
        return;
    }
    for socket in sockets.iter_mut() {
        if let Socket::Remote(remote) = socket {
            if remote.socket_type == SocketType::Terminal && remote.read_closed {
                debug!(
                    "Re-arming terminal fd {} after HUP/EIO pause",
                    remote.fd.as_raw_fd()
                );
                remote.read_closed = false;
            }
        }
    }
    *resume_at = None;
}

/// Pause terminal reads and schedule a retry; keep the PTY master fd open.
fn pause_terminal_reads(remote: &mut RemoteSocket, resume_at: &mut Option<Instant>) {
    remote.read_closed = true;
    *resume_at = Some(Instant::now() + TERMINAL_HUP_RETRY);
}

/// Fill `pollfds` from `sockets`, recording which socket index each entry maps to.
///
/// Read-closed remotes (including HUP-paused terminals) are omitted: clearing
/// `POLLIN` alone is not enough, because `poll` still reports `POLLHUP` and
/// would reset the retry deadline every iteration.
fn build_stdio_pollfds<'a>(
    sockets: &'a [Socket],
    pollfds: &mut Vec<PollFd<'a>>,
    poll_to_socket: &mut Vec<usize>,
) {
    pollfds.clear();
    poll_to_socket.clear();
    for (i, socket) in sockets.iter().enumerate() {
        match socket {
            Socket::Unix(listener) => {
                poll_to_socket.push(i);
                pollfds.push(PollFd::new(
                    listener
                        .fd()
                        .expect("listening socket must have an fd")
                        .as_fd(),
                    PollFlags::POLLIN,
                ));
            }
            Socket::Remote(remote) if remote.read_closed => {}
            Socket::Remote(remote) => {
                poll_to_socket.push(i);
                pollfds.push(PollFd::new(remote.fd.as_fd(), PollFlags::POLLIN));
            }
            Socket::Signal(fd) => {
                poll_to_socket.push(i);
                pollfds.push(PollFd::new(fd.as_fd(), PollFlags::POLLIN));
            }
        }
    }
}

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
struct RecvResult {
    n: usize,
    /// Owned SCM_RIGHTS descriptors (at most [`MAX_SCM_RIGHTS_FDS`]).
    fds: Vec<OwnedFd>,
}

/// Receives data and SCM_RIGHTS file descriptors. Every received FD is wrapped
/// in [`OwnedFd`] immediately so unused extras cannot leak.
fn recv_data_and_fds(fd: RawFd, buf: &mut [u8]) -> nix::Result<RecvResult> {
    let mut iov = [IoSliceMut::new(buf)];
    let mut cmsgspace = cmsg_space!([RawFd; MAX_SCM_RIGHTS_FDS]);

    let msg = recvmsg::<SockaddrStorage>(fd, &mut iov, Some(&mut cmsgspace), MsgFlags::empty())?;

    let mut fds = Vec::new();
    if let Some(ControlMessageOwned::ScmRights(rights)) = msg.cmsgs()?.next() {
        fds.extend(rights.into_iter().map(|raw| {
            // SAFETY: `raw` is an FD newly received via SCM_RIGHTS from
            // `recvmsg`. We take ownership exactly once here; unused FDs
            // are closed when their `OwnedFd` is dropped.
            unsafe { OwnedFd::from_raw_fd(raw) }
        }));
    }
    Ok(RecvResult { n: msg.bytes, fds })
}

/// Accepts the console-socket connection and returns the terminal FD sent over it.
///
/// Polls with a timeout and watches for runtime exit via non-reaping `waitid`.
/// On runtime exit, one final non-blocking attempt drains an already-queued
/// connection or FD before failing.
pub fn receive_console_fd(
    console_socket: UnixSocket,
    runtime_pid: Option<Pid>,
) -> ConmonResult<RemoteSocket> {
    receive_console_fd_with_timeout(console_socket, runtime_pid, CONSOLE_SOCKET_WAIT_TIMEOUT)
}

fn receive_console_fd_with_timeout(
    console_socket: UnixSocket,
    runtime_pid: Option<Pid>,
    timeout: Duration,
) -> ConmonResult<RemoteSocket> {
    let listen_fd = console_socket.fd().ok_or_else(|| {
        ConmonError::new(
            "Cannot receive console socket file descriptor without console socket.",
            1,
        )
    })?;
    let deadline = Instant::now() + timeout;

    let remote = wait_until_console_ready(
        deadline,
        runtime_pid,
        "Timed out waiting for runtime to connect on console socket",
        |wait| {
            if poll_fd(listen_fd.as_fd(), wait)? {
                console_socket.accept()
            } else {
                Ok(None)
            }
        },
    )?;

    wait_until_console_ready(
        deadline,
        runtime_pid,
        "Timed out waiting for console fd over console socket",
        |wait| {
            if poll_fd(remote.fd.as_fd(), wait)? {
                try_receive_console_fd(remote.fd.as_fd())
            } else {
                Ok(None)
            }
        },
    )
}

/// Shared wait loop for accept and FD receive. On runtime exit, tries once more
/// with a zero timeout before failing (queued connection/FD race).
fn wait_until_console_ready(
    deadline: Instant,
    runtime_pid: Option<Pid>,
    timeout_msg: &str,
    mut try_progress: impl FnMut(Duration) -> ConmonResult<Option<RemoteSocket>>,
) -> ConmonResult<RemoteSocket> {
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(ConmonError::new(timeout_msg, 1));
        }
        let wait = remaining.min(CONSOLE_SOCKET_POLL_INTERVAL);

        if let Some(value) = try_progress(wait)? {
            return Ok(value);
        }

        if let Some(status) = runtime_exit_status(runtime_pid)? {
            if let Some(value) = try_progress(Duration::ZERO)? {
                return Ok(value);
            }
            return Err(ConmonError::new(
                format!("Runtime process exited with status {status} before sending console fd"),
                1,
            ));
        }
    }
}

fn try_receive_console_fd(fd: BorrowedFd<'_>) -> ConmonResult<Option<RemoteSocket>> {
    let mut buf = [0u8; 1];
    match recv_data_and_fds(fd.as_raw_fd(), &mut buf) {
        Ok(res) if res.n > 0 => {
            let mut fds = res.fds.into_iter();
            let Some(owned_fd) = fds.next() else {
                return Err(ConmonError::new(
                    "No file descriptor received using console socket.",
                    1,
                ));
            };
            drop(fds); // close any extra SCM_RIGHTS descriptors
            debug!("Received console fd {}", owned_fd.as_raw_fd());
            Ok(Some(RemoteSocket::new(SocketType::Terminal, owned_fd)))
        }
        Ok(_) => Err(ConmonError::new(
            "Console socket closed before file descriptor was received.",
            1,
        )),
        #[allow(unreachable_patterns)]
        Err(Errno::EAGAIN) | Err(Errno::EWOULDBLOCK) => Ok(None),
        Err(e) => Err(ConmonError::new(
            format!("Error receiving file descriptor using console socket: {e}"),
            1,
        )),
    }
}

/// Non-reaping runtime exit check (`waitid` + `WNOWAIT`).
fn runtime_exit_status(runtime_pid: Option<Pid>) -> ConmonResult<Option<i32>> {
    let Some(pid) = runtime_pid else {
        return Ok(None);
    };
    let flags = WaitPidFlag::WEXITED | WaitPidFlag::WNOWAIT | WaitPidFlag::WNOHANG;
    loop {
        match waitid(Id::Pid(pid), flags) {
            Ok(WaitStatus::Exited(_, status)) => return Ok(Some(status)),
            Ok(WaitStatus::Signaled(_, sig, _)) => return Ok(Some(128 + sig as i32)),
            Ok(_) => return Ok(None),
            Err(Errno::EINTR) => continue,
            Err(Errno::ECHILD) => {
                return Err(ConmonError::new(
                    format!(
                        "waitid({pid}): unexpected ECHILD; runtime should remain waitable until session cleanup"
                    ),
                    1,
                ));
            }
            Err(e) => {
                return Err(ConmonError::new(format!("waitid({pid}) failed: {e}"), 1));
            }
        }
    }
}

/// Single `poll()`. `EINTR` returns `Ok(false)` so the caller recomputes remaining time.
fn poll_fd(fd: BorrowedFd<'_>, timeout: Duration) -> ConmonResult<bool> {
    let timeout_ms = timeout.as_millis().min(u16::MAX as u128) as u16;
    let mut pollfds = [PollFd::new(fd, PollFlags::POLLIN)];
    match poll(&mut pollfds, timeout_ms) {
        Ok(0) => Ok(false),
        Ok(_) => Ok(pollfds[0].revents().is_some_and(|r| {
            r.intersects(
                PollFlags::POLLIN | PollFlags::POLLHUP | PollFlags::POLLERR | PollFlags::POLLNVAL,
            )
        })),
        Err(Errno::EINTR) => Ok(false),
        Err(e) => Err(ConmonError::new(
            format!(
                "poll() failed while waiting for console socket: {}",
                io::Error::from_raw_os_error(e as i32)
            ),
            1,
        )),
    }
}

/// Handle a peer that reached read-side EOF without dropping the socket.
///
/// Attach clients often EOF the read side immediately (no stdin) while still
/// expecting stdout/stderr writes, so Console peers stay alive for writing.
/// Terminal peers that reach EOF are skipped by the `handle_data` with
/// the `read_closed` flag.
fn on_peer_read_eof(
    socket: &Socket,
    stdin_attached: bool,
    leave_stdin_open: bool,
    workerfd_stdin: &mut Option<OwnedFd>,
) {
    let Socket::Remote(remote) = socket else {
        return;
    };

    if remote.socket_type == SocketType::Console && stdin_attached && !leave_stdin_open {
        workerfd_stdin.take();
    }
}

/// Handles incoming data on fds and forwards them to right destination.
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
    signal_fd: Option<SignalFd>,
    mut idle_callback: F,
) -> ConmonResult<()>
where
    F: FnMut(Option<&SignalFd>) -> ConmonResult<bool>,
{
    debug!("Starting event loop");
    let mut sockets: Vec<Socket> = Vec::new();
    let mut new_sockets: Vec<RemoteSocket> = Vec::new();
    // When set, Terminal sockets paused after POLLHUP/EIO are re-armed at this time.
    let mut terminal_hup_resume_at: Option<Instant> = None;

    // Optional attach socket.
    // WARN: The attach socket must come before stdout and stderr, otherwise the
    // stdout/stderr read is handled before the attach accept callback and some
    // data from stdout/stderr can be lost.
    if let Some(attach) = attach_socket {
        sockets.push(Socket::Unix(attach));
    }

    // Container's stdout.
    if let Some(stdout) = mainfd_stdout.take() {
        sockets.push(Socket::Remote(RemoteSocket::new(
            SocketType::Stdout,
            stdout,
        )));
    }

    // Container's stderr.
    sockets.push(Socket::Remote(RemoteSocket::new(
        SocketType::Stderr,
        mainfd_stderr,
    )));

    // Optional terminal socket.
    if let Some(terminal) = terminal_socket {
        sockets.push(Socket::Remote(terminal));
    }

    // Optional ctl fifo.
    if let Some(ctl) = ctl_fifo {
        sockets.push(Socket::Remote(ctl));
    }

    // Optional winsz fifo.
    if let Some(winsz) = winsz_fifo {
        sockets.push(Socket::Remote(winsz));
    }

    // Optional OOM socket.
    if let Some(oom) = oom_socket {
        sockets.push(Socket::Remote(oom));
    }

    // Optional systemd notify socket.
    if let Some(notify) = notify_socket {
        sockets.push(Socket::Remote(notify));
    }

    // Signal FD to receive UNIX signals.
    // It is owned by the RuntimeSession and borrowed here only for polling.
    if let Some(signal_fd) = signal_fd {
        info!("SignalFD: {}", signal_fd.as_raw_fd());
        sockets.push(Socket::Signal(signal_fd));
    }

    // Main loop.
    // Iterates as long as we have some RemoteSocket to read from or
    // as long as `idle_callback` returns `true`.
    while sockets.iter().any(|s| matches!(s, Socket::Remote(_))) {
        // Re-arm before building the poll set so a due deadline is honored even
        // when another fd keeps returning events (never idle).
        resume_paused_terminals(&mut sockets, &mut terminal_hup_resume_at);

        // Build the poll set each iteration by borrowing the fds owned by
        // `sockets`. Read-closed remotes (including HUP-paused terminals) are
        // omitted so lingering POLLHUP cannot reset the retry deadline.
        let mut pollfds: Vec<PollFd> = Vec::with_capacity(sockets.len());
        let mut poll_to_socket: Vec<usize> = Vec::with_capacity(sockets.len());
        build_stdio_pollfds(&sockets, &mut pollfds, &mut poll_to_socket);

        let n = poll(&mut pollfds, STDIO_POLL_MS).map_err(|e| {
            ConmonError::new(
                format!(
                    "handle_stdio poll() failed: {}",
                    io::Error::from_raw_os_error(e as i32)
                ),
                1,
            )
        })?;

        // Snapshot results into a sockets-aligned vector (None = not polled /
        // no events) so later mutation can use socket indices.
        let mut revents: Vec<Option<PollFlags>> = vec![None; sockets.len()];
        for (poll_i, &sock_i) in poll_to_socket.iter().enumerate() {
            revents[sock_i] = pollfds[poll_i].revents();
        }
        drop(pollfds);
        drop(poll_to_socket);

        // We have no fd to read from, so execute the idle function.
        if n == 0 {
            let keep_running = idle_callback(None)?;
            if !keep_running {
                info!("idle_callback stopped the event loop.");
                return Ok(());
            }
            continue;
        }

        // We will mutate sockets/revents, so iterate by index.
        let mut i = 0;
        while i < revents.len() {
            // If `false`, we close the socket completely.
            let mut keep_socket = true;
            // if `false`, we close the read side of the socket.
            let mut continue_reading = true;

            if let Some(events) = revents[i] {
                if events.contains(PollFlags::POLLIN) {
                    // If the POLLIN comes from the signal fd, hand the signal fd to
                    // the idle_callback so it can read and forward the signal.
                    if let Socket::Signal(signal_fd) = &sockets[i] {
                        if !idle_callback(Some(signal_fd))? {
                            info!("idle_callback stopped the event loop after signal.");
                            return Ok(());
                        }
                        i += 1;
                        continue;
                    }

                    // Handle the received data.
                    continue_reading = Socket::handle_data(
                        &mut sockets,
                        i,
                        log_plugin,
                        &mut new_sockets,
                        workerfd_stdin.as_ref(),
                        &notify_host_path,
                    )?;

                    // Add connections accepted during this iteration and
                    // keep it aligned with the `revents` vector.
                    while let Some(n_s) = new_sockets.pop() {
                        info!("Adding {:?} into poll fds", n_s);
                        sockets.push(Socket::Remote(n_s));
                        revents.push(None);
                    }
                }

                if events.contains(PollFlags::POLLHUP) {
                    if is_terminal_remote(&sockets[i]) {
                        // Transient: no slave open. Keep the master; pause and retry.
                        // Paused terminals are omitted from the next poll set, so this
                        // path only runs for an armed terminal.
                        if let Socket::Remote(remote) = &mut sockets[i] {
                            debug!(
                                "Terminal HUP on fd {}; pausing reads (v2 tty_hup behavior)",
                                remote.fd.as_raw_fd()
                            );
                            pause_terminal_reads(remote, &mut terminal_hup_resume_at);
                        }
                        continue_reading = true;
                    } else if !events.contains(PollFlags::POLLIN) {
                        // Pipe HUP without input is final.
                        debug!("HUP on {:?}", sockets[i]);
                        keep_socket = false;
                    }
                }
            }

            if !continue_reading {
                // Stop polling this fd for input; it may still be a write target.
                if is_terminal_remote(&sockets[i]) {
                    // EOF/EIO on the PTY: pause and retry; never SHUT_RD the master.
                    if let Socket::Remote(remote) = &mut sockets[i] {
                        debug!(
                            "Terminal read pause on fd {} (EOF/EIO)",
                            remote.fd.as_raw_fd()
                        );
                        pause_terminal_reads(remote, &mut terminal_hup_resume_at);
                    }
                } else {
                    if let Socket::Remote(remote) = &mut sockets[i] {
                        let raw = remote.fd.as_raw_fd();
                        debug!("Shutdown {}", raw);
                        unsafe { shutdown(raw, SHUT_RD) };
                        remote.read_closed = true;
                    }
                    on_peer_read_eof(
                        &sockets[i],
                        stdin_attached,
                        leave_stdin_open,
                        &mut workerfd_stdin,
                    );
                }
            }

            if keep_socket {
                // Go to next socket in case we want to keep this one.
                i += 1;
            } else {
                // Remove the fd completely.
                let socket = sockets.swap_remove(i);
                info!("Removing socket {:?}", socket);
                revents.swap_remove(i);

                // Do NOT increment the `i`, since it now points to swapped fd.
            }
        }
    }

    // All remote sockets closed; probe for a container that exited while I/O drained.
    let keep_running = idle_callback(None)?;
    if !keep_running {
        info!("idle_callback stopped the event loop after sockets closed.");
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
    use nix::sys::wait::{Id, WaitPidFlag, WaitStatus, waitid};
    use std::io::IoSlice;
    use std::os::fd::{AsFd, FromRawFd};
    use std::os::unix::net::UnixStream;
    use std::process::Command;
    use std::thread;
    use std::time::{Duration, Instant};
    use tempfile::tempdir;

    fn test_console_socket() -> ConmonResult<(tempfile::TempDir, UnixSocket)> {
        let tmp = tempdir().map_err(|e| ConmonError::new(e.to_string(), 1))?;
        let mut s = UnixSocket::new(
            SocketType::Terminal,
            false,
            tmp.path().to_path_buf(),
            None,
            None,
        );
        s.bind(
            Some(tmp.path().join("console.sock")),
            SockType::Stream,
            SockFlag::SOCK_CLOEXEC,
            Mode::from_bits_truncate(0o700),
        )?;
        s.listen()?;
        Ok((tmp, s))
    }

    fn send_fds(count: usize, payload: &[u8]) -> ConmonResult<(OwnedFd, Vec<(OwnedFd, OwnedFd)>)> {
        let (sender, receiver) = socketpair(
            AddressFamily::Unix,
            SockType::Stream,
            None,
            SockFlag::empty(),
        )?;
        let mut keepalive = Vec::new();
        let mut fds = Vec::new();
        for _ in 0..count {
            let (r, w) = pipe2(OFlag::O_CLOEXEC)?;
            fds.push(r.as_raw_fd());
            keepalive.push((r, w));
        }
        sendmsg::<()>(
            sender.as_raw_fd(),
            &[IoSlice::new(payload)],
            &[ControlMessage::ScmRights(&fds)],
            MsgFlags::empty(),
            None,
        )?;
        Ok((receiver, keepalive))
    }

    fn wait_exited_nowait(pid: Pid) {
        let flags = WaitPidFlag::WEXITED | WaitPidFlag::WNOWAIT | WaitPidFlag::WNOHANG;
        loop {
            match waitid(Id::Pid(pid), flags).unwrap() {
                WaitStatus::Exited(_, _) | WaitStatus::Signaled(_, _, _) => return,
                _ => thread::sleep(Duration::from_millis(10)),
            }
        }
    }

    #[test]
    fn recv_data() -> ConmonResult<()> {
        let (receiver, _k) = send_fds(1, b"foo")?;
        let mut buf = [0u8; 16];
        let res = recv_data_and_fds(receiver.as_raw_fd(), &mut buf)?;
        assert_eq!(res.n, 3);
        assert_eq!(&buf[..3], b"foo");
        assert_eq!(res.fds.len(), 1);
        Ok(())
    }

    #[test]
    fn recv_data_too_many_scm_rights() -> ConmonResult<()> {
        let (receiver, _k) = send_fds(MAX_SCM_RIGHTS_FDS + 1, b"foo")?;
        let mut buf = [0u8; 16];
        assert_eq!(
            recv_data_and_fds(receiver.as_raw_fd(), &mut buf).err(),
            Some(Errno::ENOBUFS)
        );
        Ok(())
    }

    #[test]
    fn receive_console_fd_times_out_without_connection() -> ConmonResult<()> {
        let (_tmp, sock) = test_console_socket()?;
        let err =
            receive_console_fd_with_timeout(sock, None, Duration::from_millis(200)).unwrap_err();
        assert!(err.msg.contains("Timed out waiting for runtime to connect"));
        Ok(())
    }

    #[test]
    fn receive_console_fd_gets_passed_fd() -> ConmonResult<()> {
        let (_tmp, sock) = test_console_socket()?;
        let path = sock.path().unwrap().clone();
        let peer = thread::spawn(move || {
            let client = UnixStream::connect(path).unwrap();
            let (r, w) = pipe2(OFlag::O_CLOEXEC).unwrap();
            drop(w);
            sendmsg::<()>(
                client.as_raw_fd(),
                &[IoSlice::new(b"x")],
                &[ControlMessage::ScmRights(&[r.as_raw_fd()])],
                MsgFlags::empty(),
                None,
            )
            .unwrap();
        });
        let terminal = receive_console_fd_with_timeout(sock, None, Duration::from_secs(5))?;
        assert_eq!(terminal.socket_type, SocketType::Terminal);
        peer.join().unwrap();
        Ok(())
    }

    #[test]
    fn runtime_exit_status_does_not_reap_child() {
        let mut child = Command::new("sh").args(["-c", "exit 42"]).spawn().unwrap();
        let pid = Pid::from_raw(child.id() as i32);
        wait_exited_nowait(pid);
        assert_eq!(runtime_exit_status(Some(pid)).unwrap(), Some(42));
        assert_eq!(child.wait().unwrap().code(), Some(42));
    }

    #[test]
    fn receive_console_fd_fails_when_runtime_exits() -> ConmonResult<()> {
        let (_tmp, sock) = test_console_socket()?;
        let mut child = Command::new("sh").args(["-c", "exit 42"]).spawn().unwrap();
        let pid = Pid::from_raw(child.id() as i32);
        let err =
            receive_console_fd_with_timeout(sock, Some(pid), Duration::from_secs(5)).unwrap_err();
        let _ = child.wait();
        assert!(err.msg.contains("Runtime process exited with status 42"));
        Ok(())
    }

    #[test]
    fn accept_propagates_errors() -> ConmonResult<()> {
        let tmp = tempdir().map_err(|e| ConmonError::new(e.to_string(), 1))?;
        let mut sock = UnixSocket::new(
            SocketType::Terminal,
            false,
            tmp.path().to_path_buf(),
            None,
            None,
        );
        sock.bind(
            Some(tmp.path().join("nolisten.sock")),
            SockType::Stream,
            SockFlag::SOCK_CLOEXEC,
            Mode::from_bits_truncate(0o700),
        )?;
        let err = sock.accept().unwrap_err();
        assert!(err.msg.contains("Failed to accept client connection"));
        Ok(())
    }

    #[test]
    fn read_eof_closes_container_stdin_when_attached() -> ConmonResult<()> {
        let (attach_r, _attach_w) = pipe2(OFlag::O_CLOEXEC)?;
        let (stdin_r, stdin_w) = pipe2(OFlag::O_CLOEXEC)?;
        drop(stdin_r);
        let sockets = [Socket::Remote(RemoteSocket::new(
            SocketType::Console,
            attach_r,
        ))];
        let mut workerfd_stdin = Some(stdin_w);

        on_peer_read_eof(&sockets[0], true, false, &mut workerfd_stdin);

        assert!(
            workerfd_stdin.is_none(),
            "container stdin is closed when the attach client EOFs"
        );
        Ok(())
    }

    /// Open a PTY master with the slave closed (master yields EIO/HUP).
    fn pty_master_slave_closed() -> ConmonResult<(OwnedFd, std::ffi::CString)> {
        use std::ffi::CStr;
        unsafe {
            let master = nix::libc::posix_openpt(
                nix::libc::O_RDWR | nix::libc::O_NOCTTY | nix::libc::O_NONBLOCK,
            );
            if master < 0 {
                return Err(ConmonError::new("posix_openpt failed", 1));
            }
            if nix::libc::grantpt(master) != 0 || nix::libc::unlockpt(master) != 0 {
                let _ = nix::libc::close(master);
                return Err(ConmonError::new("grantpt/unlockpt failed", 1));
            }
            let name = nix::libc::ptsname(master);
            if name.is_null() {
                let _ = nix::libc::close(master);
                return Err(ConmonError::new("ptsname failed", 1));
            }
            let path = std::ffi::CString::from(CStr::from_ptr(name));
            let slave = nix::libc::open(path.as_ptr(), nix::libc::O_RDWR | nix::libc::O_NOCTTY);
            if slave < 0 {
                let _ = nix::libc::close(master);
                return Err(ConmonError::new("open pty slave failed", 1));
            }
            let _ = nix::libc::close(slave);
            Ok((OwnedFd::from_raw_fd(master), path))
        }
    }

    #[test]
    fn terminal_rearms_while_other_fd_keeps_poll_busy() -> ConmonResult<()> {
        // Re-arm must not depend on poll idle (n==0), and the master must stay
        // open so a later slave reopen delivers data.
        let (master, slave_path) = pty_master_slave_closed()?;
        let (err_r, err_w) = pipe2(OFlag::O_CLOEXEC | OFlag::O_NONBLOCK)?;
        let _ = nix::unistd::write(err_w.as_fd(), b"x")?;

        let mut sockets = vec![
            Socket::Remote(RemoteSocket::new(SocketType::Stderr, err_r)),
            Socket::Remote(RemoteSocket::new(SocketType::Terminal, master)),
        ];
        let mut resume_at = Some(Instant::now() + Duration::from_millis(40));
        if let Socket::Remote(term) = &mut sockets[1] {
            term.read_closed = true;
        }

        let mut rearmed = false;
        let mut saw_busy_poll = false;
        let deadline = Instant::now() + Duration::from_millis(500);
        while Instant::now() < deadline {
            resume_paused_terminals(&mut sockets, &mut resume_at);
            if let Socket::Remote(term) = &sockets[1] {
                if !term.read_closed {
                    rearmed = true;
                    break;
                }
            }

            let mut pollfds = Vec::new();
            let mut poll_to_socket = Vec::new();
            build_stdio_pollfds(&sockets, &mut pollfds, &mut poll_to_socket);
            let n = poll(&mut pollfds, STDIO_POLL_MS).unwrap();
            if n > 0 {
                if let Some(stderr_poll_i) = poll_to_socket.iter().position(|&i| i == 0) {
                    if pollfds[stderr_poll_i]
                        .revents()
                        .is_some_and(|ev| ev.intersects(PollFlags::POLLIN | PollFlags::POLLHUP))
                    {
                        saw_busy_poll = true;
                    }
                }
            }
        }

        assert!(saw_busy_poll, "stderr must keep poll busy during the wait");
        assert!(
            rearmed,
            "terminal must re-arm even when poll never returns 0"
        );

        let slave = unsafe {
            nix::libc::open(slave_path.as_ptr(), nix::libc::O_RDWR | nix::libc::O_NOCTTY)
        };
        assert!(slave >= 0);
        let marker = b"rearm-ok";
        unsafe {
            let _ = nix::libc::write(slave, marker.as_ptr() as *const _, marker.len());
        }
        if let Socket::Remote(term) = &sockets[1] {
            let mut pfd = [PollFd::new(term.fd.as_fd(), PollFlags::POLLIN)];
            let _ = poll(&mut pfd, 200u16).unwrap();
            let mut buf = [0u8; 64];
            let n = read(term.fd.as_fd(), &mut buf)
                .map_err(|e| ConmonError::new(format!("read after re-arm: {e}"), 1))?;
            assert!(n > 0, "expected terminal data after re-arm");
            assert!(
                buf[..n].windows(marker.len()).any(|w| w == marker),
                "master must stay open across pause; got {:?}",
                &buf[..n]
            );
        }
        unsafe {
            let _ = nix::libc::close(slave);
        }
        drop(err_w);
        Ok(())
    }

    #[test]
    fn paused_terminal_with_closed_slave_does_not_spin_or_postpone_deadline() -> ConmonResult<()> {
        // Empty-events poll still reports POLLHUP on a master with no slave; the
        // paused fd must be omitted from the poll set so the deadline is not reset.
        let (master, _path) = pty_master_slave_closed()?;
        {
            let mut pfd = [PollFd::new(master.as_fd(), PollFlags::empty())];
            let n = poll(&mut pfd, 0u16).unwrap();
            assert!(
                n > 0
                    && pfd[0]
                        .revents()
                        .is_some_and(|r| r.contains(PollFlags::POLLHUP)),
                "precondition: empty events still surface POLLHUP"
            );
        }

        let mut sockets = vec![Socket::Remote(RemoteSocket::new(
            SocketType::Terminal,
            master,
        ))];
        let scheduled = Instant::now() + Duration::from_secs(60);
        let mut resume_at = Some(scheduled);
        if let Socket::Remote(term) = &mut sockets[0] {
            term.read_closed = true;
        }

        for _ in 0..25 {
            let mut pollfds = Vec::new();
            let mut poll_to_socket = Vec::new();
            build_stdio_pollfds(&sockets, &mut pollfds, &mut poll_to_socket);
            assert!(
                pollfds.is_empty() && poll_to_socket.is_empty(),
                "paused terminal must be excluded from poll"
            );
            assert_eq!(poll(&mut pollfds, 0u16).unwrap(), 0);
            assert_eq!(resume_at, Some(scheduled));
        }

        resume_at = Some(Instant::now() - Duration::from_millis(1));
        resume_paused_terminals(&mut sockets, &mut resume_at);
        assert!(resume_at.is_none());
        match &sockets[0] {
            Socket::Remote(t) => assert!(!t.read_closed, "deadline arrival must re-arm"),
            _ => panic!("expected terminal"),
        }
        Ok(())
    }
}
