use crate::{
    error::{ConmonError, ConmonResult},
    logging::plugin::LogPlugin,
    unix_socket::{ATTACH_WRITE_TIMEOUT, RemoteSocket, Socket, SocketType, UnixSocket},
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

/// True when any attach peer still has queued stdout/stderr to deliver.
fn attach_output_pending(sockets: &[Socket]) -> bool {
    sockets
        .iter()
        .any(|s| matches!(s, Socket::Remote(r) if r.needs_pollout()))
}

/// Whether `idle_callback` asking to stop should end `handle_stdio`.
///
/// Stop is deferred while attach peers still have pending writes so final
/// container output is not discarded on exit.
fn should_stop_stdio_loop(idle_says_stop: bool, sockets: &[Socket]) -> bool {
    idle_says_stop && !attach_output_pending(sockets)
}

/// Remove peers marked `write_failed`.
///
/// Removing the FD ends the whole attach connection (input and output), so for
/// `SocketType::Console` peers we apply the same stdin policy as read-EOF via
/// [`on_peer_read_eof`] before the socket is dropped.
fn remove_write_failed_peers(
    sockets: &mut Vec<Socket>,
    mut revents: Option<&mut Vec<Option<PollFlags>>>,
    stdin_attached: bool,
    leave_stdin_open: bool,
    workerfd_stdin: &mut Option<OwnedFd>,
) {
    let mut i = 0;
    while i < sockets.len() {
        let failed = matches!(&sockets[i], Socket::Remote(r) if r.write_failed);
        if !failed {
            i += 1;
            continue;
        }
        on_peer_read_eof(
            &sockets[i],
            stdin_attached,
            leave_stdin_open,
            workerfd_stdin,
        );
        let socket = sockets.swap_remove(i);
        info!("Removing write-failed socket {:?}", socket);
        if let Some(revents) = revents.as_mut() {
            revents.swap_remove(i);
        }
    }
}

/// True when the FD must leave the poll set *before* any read.
///
/// `POLLNVAL` is always immediate. `POLLERR` / `POLLHUP` without `POLLIN` are
/// also immediate (nothing left to drain). `POLLIN | POLLERR` and
/// `POLLIN | POLLHUP` are *not* immediate — readable data is handled first.
fn poll_fd_is_immediately_fatal(events: PollFlags) -> bool {
    if events.contains(PollFlags::POLLNVAL) {
        return true;
    }
    if events.contains(PollFlags::POLLIN) {
        return false;
    }
    events.contains(PollFlags::POLLERR)
        || (events.contains(PollFlags::POLLHUP) && !events.contains(PollFlags::POLLOUT))
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
        // Build the poll set each iteration by borrowing the fds owned by
        // `sockets`.
        let mut pollfds: Vec<PollFd> = sockets
            .iter()
            .map(|socket| match socket {
                Socket::Unix(listener) => PollFd::new(
                    listener
                        .fd()
                        .expect("listening socket must have an fd")
                        .as_fd(),
                    PollFlags::POLLIN,
                ),
                Socket::Remote(remote) => {
                    // A socket whose read side reached EOF is no longer polled for
                    // input, but stays alive for writing (and POLLOUT when pending).
                    let mut events = if remote.read_closed {
                        PollFlags::empty()
                    } else {
                        PollFlags::POLLIN
                    };
                    if remote.needs_pollout() {
                        events |= PollFlags::POLLOUT;
                    }
                    PollFd::new(remote.fd.as_fd(), events)
                }
                Socket::Signal(fd) => PollFd::new(fd.as_fd(), PollFlags::POLLIN),
            })
            .collect();

        // Run poll to get informed about new fd events.
        let n = poll(&mut pollfds, 10_u16).map_err(|e| {
            ConmonError::new(
                format!(
                    "handle_stdio poll() failed: {}",
                    io::Error::from_raw_os_error(e as i32)
                ),
                1,
            )
        })?;

        // Snapshot the results so the borrow of `sockets` is released before it
        // is mutated below. `revents` stays index-aligned with `sockets`.
        let mut revents: Vec<Option<PollFlags>> = pollfds.iter().map(|pfd| pfd.revents()).collect();
        drop(pollfds);

        // Expire attach peers that have made no write progress for too long.
        // Also runs on poll timeout (n == 0) so a never-readable client is dropped.
        let now = Instant::now();
        for socket in sockets.iter_mut() {
            if let Socket::Remote(remote) = socket {
                remote.expire_attach_write_timeout(now, ATTACH_WRITE_TIMEOUT);
            }
        }
        remove_write_failed_peers(
            &mut sockets,
            Some(&mut revents),
            stdin_attached,
            leave_stdin_open,
            &mut workerfd_stdin,
        );

        // We have no fd ready, so execute the idle function.
        if n == 0 {
            let keep_running = idle_callback(None)?;
            if should_stop_stdio_loop(!keep_running, &sockets) {
                info!("idle_callback stopped the event loop.");
                return Ok(());
            }
            if !keep_running {
                debug!("deferring idle stop; attach output still pending");
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
            // Full remove due to peer disconnect (HUP/ERR) — may close stdin.
            let mut peer_disconnected = false;

            if let Some(events) = revents[i] {
                // Flush queued attach output when writable (including HUP+POLLOUT).
                if events.contains(PollFlags::POLLOUT)
                    && let Socket::Remote(remote) = &mut sockets[i]
                {
                    remote.flush_pending_attach_writes();
                }

                if poll_fd_is_immediately_fatal(events) {
                    // NVAL, or ERR/HUP with no readable data: drop before we can
                    // busy-loop. Flush already ran above if POLLOUT.
                    debug!("fatal poll events {events:?} on {:?}", sockets[i]);
                    if let Socket::Remote(remote) = &mut sockets[i] {
                        if !remote.write_failed {
                            remote.mark_write_failed("poll reported socket error");
                        }
                    }
                    keep_socket = false;
                    peer_disconnected = true;
                } else if events.contains(PollFlags::POLLIN) {
                    // If the POLLIN comes from the signal fd, hand the signal fd to
                    // the idle_callback so it can read and forward the signal.
                    if let Socket::Signal(signal_fd) = &sockets[i] {
                        let keep_running = idle_callback(Some(signal_fd))?;
                        if should_stop_stdio_loop(!keep_running, &sockets) {
                            info!("idle_callback stopped the event loop after signal.");
                            return Ok(());
                        }
                        if !keep_running {
                            debug!("deferring signal-idle stop; attach output still pending");
                        }
                        i += 1;
                        continue;
                    }

                    // Handle the received data (including final bytes on ERR/HUP).
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

                    // After draining readable data, POLLERR means the FD is done.
                    if events.contains(PollFlags::POLLERR) {
                        debug!("POLLERR after read on {:?}", sockets[i]);
                        if let Socket::Remote(remote) = &mut sockets[i] {
                            if !remote.write_failed {
                                remote.mark_write_failed("poll reported socket error after read");
                            }
                        }
                        keep_socket = false;
                        peer_disconnected = true;
                    }
                } else if events.contains(PollFlags::POLLHUP) {
                    // HUP with POLLOUT already flushed above; peer is going away.
                    debug!("HUP on {:?}", sockets[i]);
                    keep_socket = false;
                    peer_disconnected = true;
                }
            }

            if !continue_reading {
                // Close the read part of the socket and stop polling it for
                // input; it may still be a write target (e.g. attach client).
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

            if keep_socket {
                // Go to next socket in case we want to keep this one.
                i += 1;
            } else {
                if peer_disconnected {
                    on_peer_read_eof(
                        &sockets[i],
                        stdin_attached,
                        leave_stdin_open,
                        &mut workerfd_stdin,
                    );
                }
                // Remove the fd completely.
                let socket = sockets.swap_remove(i);
                info!("Removing socket {:?}", socket);
                revents.swap_remove(i);

                // Do NOT increment the `i`, since it now points to swapped fd.
            }
        }

        // Drop attach clients whose writes failed during POLLOUT flush / handle_data.
        remove_write_failed_peers(
            &mut sockets,
            None,
            stdin_attached,
            leave_stdin_open,
            &mut workerfd_stdin,
        );
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
    use crate::unix_socket::{
        ATTACH_PENDING_MAX_BYTES, ATTACH_WRITE_TIMEOUT, SocketType, UnixSocket,
    };
    use nix::sys::socket::{
        AddressFamily, ControlMessage, SockFlag, SockType, sendmsg, socketpair,
    };
    use nix::sys::stat::Mode;
    use nix::sys::wait::{Id, WaitPidFlag, WaitStatus, waitid};
    use std::io::IoSlice;
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

    #[test]
    fn write_failed_attach_removal_closes_stdin_when_attached() -> ConmonResult<()> {
        let (attach_r, _attach_w) = pipe2(OFlag::O_CLOEXEC)?;
        let (stdin_r, stdin_w) = pipe2(OFlag::O_CLOEXEC)?;
        drop(stdin_r);

        let mut peer = RemoteSocket::new(SocketType::Console, attach_r);
        peer.write_failed = true;
        let mut sockets = vec![Socket::Remote(peer)];
        let mut workerfd_stdin = Some(stdin_w);

        remove_write_failed_peers(&mut sockets, None, true, false, &mut workerfd_stdin);

        assert!(
            sockets.is_empty(),
            "write-failed attach peer must be removed"
        );
        assert!(
            workerfd_stdin.is_none(),
            "full attach disconnect must close container stdin"
        );
        Ok(())
    }

    #[test]
    fn write_failed_attach_removal_keeps_stdin_when_leave_open() -> ConmonResult<()> {
        let (attach_r, _attach_w) = pipe2(OFlag::O_CLOEXEC)?;
        let (stdin_r, stdin_w) = pipe2(OFlag::O_CLOEXEC)?;
        drop(stdin_r);

        let mut peer = RemoteSocket::new(SocketType::Console, attach_r);
        peer.write_failed = true;
        let mut sockets = vec![Socket::Remote(peer)];
        let mut workerfd_stdin = Some(stdin_w);

        remove_write_failed_peers(&mut sockets, None, true, true, &mut workerfd_stdin);

        assert!(sockets.is_empty());
        assert!(
            workerfd_stdin.is_some(),
            "leave_stdin_open must keep container stdin after attach drop"
        );
        Ok(())
    }

    #[test]
    fn write_failed_attach_removal_keeps_stdin_when_not_attached() -> ConmonResult<()> {
        let (attach_r, _attach_w) = pipe2(OFlag::O_CLOEXEC)?;
        let (stdin_r, stdin_w) = pipe2(OFlag::O_CLOEXEC)?;
        drop(stdin_r);

        let mut peer = RemoteSocket::new(SocketType::Console, attach_r);
        peer.write_failed = true;
        let mut sockets = vec![Socket::Remote(peer)];
        let mut workerfd_stdin = Some(stdin_w);

        remove_write_failed_peers(&mut sockets, None, false, false, &mut workerfd_stdin);

        assert!(sockets.is_empty());
        assert!(
            workerfd_stdin.is_some(),
            "stdin stays open when stdin was never attached"
        );
        Ok(())
    }

    #[test]
    fn queue_overflow_disconnect_closes_stdin_like_eof() -> ConmonResult<()> {
        let (srv, _cli) = socketpair(
            AddressFamily::Unix,
            SockType::SeqPacket,
            None,
            SockFlag::SOCK_CLOEXEC | SockFlag::SOCK_NONBLOCK,
        )?;
        let (stdin_r, stdin_w) = pipe2(OFlag::O_CLOEXEC)?;
        drop(stdin_r);

        let mut peer = RemoteSocket::new(SocketType::Console, srv);
        peer.push_pending_for_test(vec![b'x'; ATTACH_PENDING_MAX_BYTES]);
        peer.push_pending_for_test(b"over".to_vec());
        assert!(peer.write_failed);

        let mut sockets = vec![Socket::Remote(peer)];
        let mut workerfd_stdin = Some(stdin_w);
        remove_write_failed_peers(&mut sockets, None, true, false, &mut workerfd_stdin);

        assert!(sockets.is_empty());
        assert!(workerfd_stdin.is_none());
        Ok(())
    }

    #[test]
    fn write_timeout_disconnect_closes_stdin_like_eof() -> ConmonResult<()> {
        let (srv, _cli) = socketpair(
            AddressFamily::Unix,
            SockType::SeqPacket,
            None,
            SockFlag::SOCK_CLOEXEC | SockFlag::SOCK_NONBLOCK,
        )?;
        let (stdin_r, stdin_w) = pipe2(OFlag::O_CLOEXEC)?;
        drop(stdin_r);

        let mut peer = RemoteSocket::new(SocketType::Console, srv);
        peer.push_pending_for_test(b"\x02stuck".to_vec());
        peer.set_pending_since_for_test(
            Instant::now() - ATTACH_WRITE_TIMEOUT - Duration::from_millis(1),
        );
        peer.expire_attach_write_timeout(Instant::now(), ATTACH_WRITE_TIMEOUT);
        assert!(peer.write_failed);

        let mut sockets = vec![Socket::Remote(peer)];
        let mut workerfd_stdin = Some(stdin_w);
        remove_write_failed_peers(&mut sockets, None, true, false, &mut workerfd_stdin);

        assert!(sockets.is_empty());
        assert!(workerfd_stdin.is_none());
        Ok(())
    }

    #[test]
    fn write_failed_non_console_removal_does_not_close_stdin() -> ConmonResult<()> {
        let (stdout_r, _stdout_w) = pipe2(OFlag::O_CLOEXEC)?;
        let (stdin_r, stdin_w) = pipe2(OFlag::O_CLOEXEC)?;
        drop(stdin_r);

        let mut peer = RemoteSocket::new(SocketType::Stdout, stdout_r);
        peer.write_failed = true;
        let mut sockets = vec![Socket::Remote(peer)];
        let mut workerfd_stdin = Some(stdin_w);

        remove_write_failed_peers(&mut sockets, None, true, false, &mut workerfd_stdin);

        assert!(sockets.is_empty());
        assert!(
            workerfd_stdin.is_some(),
            "non-console write_failed peers must not close container stdin"
        );
        Ok(())
    }

    #[test]
    fn write_failed_one_of_many_attach_peers_closes_stdin() -> ConmonResult<()> {
        // Established policy (same as on_peer_read_eof): any Console disconnect
        // closes stdin when attached and leave_stdin_open is false — including
        // when another attach peer is still present.
        let (a_r, _a_w) = pipe2(OFlag::O_CLOEXEC)?;
        let (b_r, _b_w) = pipe2(OFlag::O_CLOEXEC)?;
        let (stdin_r, stdin_w) = pipe2(OFlag::O_CLOEXEC)?;
        drop(stdin_r);

        let mut failed = RemoteSocket::new(SocketType::Console, a_r);
        failed.write_failed = true;
        let healthy = RemoteSocket::new(SocketType::Console, b_r);
        let mut sockets = vec![Socket::Remote(failed), Socket::Remote(healthy)];
        let mut workerfd_stdin = Some(stdin_w);

        remove_write_failed_peers(&mut sockets, None, true, false, &mut workerfd_stdin);

        assert_eq!(sockets.len(), 1);
        assert!(matches!(&sockets[0], Socket::Remote(r) if !r.write_failed));
        assert!(
            workerfd_stdin.is_none(),
            "stdin closes when any stdin-attached Console peer is fully removed"
        );
        Ok(())
    }

    #[test]
    fn poll_fd_is_immediately_fatal_respects_readable_err() {
        // Immediate removal.
        assert!(poll_fd_is_immediately_fatal(PollFlags::POLLNVAL));
        assert!(poll_fd_is_immediately_fatal(PollFlags::POLLERR));
        assert!(poll_fd_is_immediately_fatal(PollFlags::POLLHUP));
        assert!(poll_fd_is_immediately_fatal(
            PollFlags::POLLERR | PollFlags::POLLHUP
        ));

        // Drain readable data first (including with ERR/HUP).
        assert!(!poll_fd_is_immediately_fatal(PollFlags::POLLIN));
        assert!(!poll_fd_is_immediately_fatal(PollFlags::POLLOUT));
        assert!(!poll_fd_is_immediately_fatal(
            PollFlags::POLLIN | PollFlags::POLLHUP
        ));
        assert!(!poll_fd_is_immediately_fatal(
            PollFlags::POLLIN | PollFlags::POLLERR
        ));
        assert!(!poll_fd_is_immediately_fatal(
            PollFlags::POLLIN | PollFlags::POLLERR | PollFlags::POLLHUP
        ));
        // HUP with POLLOUT: flush first via the POLLOUT path, then HUP branch.
        assert!(!poll_fd_is_immediately_fatal(
            PollFlags::POLLOUT | PollFlags::POLLHUP
        ));
        // NVAL wins even alongside POLLIN.
        assert!(poll_fd_is_immediately_fatal(
            PollFlags::POLLIN | PollFlags::POLLNVAL
        ));
    }

    #[test]
    fn idle_stop_deferred_while_final_attach_output_queued() -> ConmonResult<()> {
        let (srv, _cli) = socketpair(
            AddressFamily::Unix,
            SockType::SeqPacket,
            None,
            SockFlag::SOCK_CLOEXEC | SockFlag::SOCK_NONBLOCK,
        )?;
        let mut peer = RemoteSocket::new(SocketType::Console, srv);
        // Simulate final container output queued after EAGAIN, just before exit.
        peer.push_pending_for_test(b"\x02final\n".to_vec());
        let sockets = vec![Socket::Remote(peer)];

        assert!(
            attach_output_pending(&sockets),
            "queued attach output must be visible to the event loop"
        );
        assert!(
            !should_stop_stdio_loop(true, &sockets),
            "must not exit while final attach output is still queued"
        );

        // After the queue drains, idle stop is allowed.
        let (srv, _cli) = socketpair(
            AddressFamily::Unix,
            SockType::SeqPacket,
            None,
            SockFlag::SOCK_CLOEXEC | SockFlag::SOCK_NONBLOCK,
        )?;
        let drained = vec![Socket::Remote(RemoteSocket::new(SocketType::Console, srv))];
        assert!(!attach_output_pending(&drained));
        assert!(should_stop_stdio_loop(true, &drained));
        assert!(!should_stop_stdio_loop(false, &drained));
        Ok(())
    }

    #[test]
    fn idle_stop_allowed_after_timeout_clears_stalled_pending() -> ConmonResult<()> {
        let (srv, _cli) = socketpair(
            AddressFamily::Unix,
            SockType::SeqPacket,
            None,
            SockFlag::SOCK_CLOEXEC | SockFlag::SOCK_NONBLOCK,
        )?;
        let mut peer = RemoteSocket::new(SocketType::Console, srv);
        peer.push_pending_for_test(b"\x02stuck".to_vec());
        peer.set_pending_since_for_test(
            Instant::now() - ATTACH_WRITE_TIMEOUT - Duration::from_millis(1),
        );
        peer.expire_attach_write_timeout(Instant::now(), ATTACH_WRITE_TIMEOUT);
        assert!(peer.write_failed);

        let mut sockets = vec![Socket::Remote(peer)];
        remove_write_failed_peers(&mut sockets, None, true, false, &mut None);
        assert!(sockets.is_empty());
        assert!(should_stop_stdio_loop(true, &sockets));
        Ok(())
    }
}
