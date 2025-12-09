use std::time::{SystemTime, UNIX_EPOCH};
use std::{fs, os::fd::OwnedFd, path::PathBuf, process::Stdio};

use log::{debug, error, info};
use nix::sys::signal::{Signal, kill};
use nix::unistd::{Pid, getpgid};
use nix::{
    errno::Errno,
    libc,
    sys::{
        socket::{SockFlag, SockType},
        stat::Mode,
        wait::{WaitPidFlag, WaitStatus, waitpid},
    },
};

use crate::runtime::cgroup::setup_oom_handling;
use crate::{
    cli::CommonCfg,
    error::{ConmonError, ConmonResult},
    logging::plugin::LogPlugin,
    parent_pipe::{get_pipe_fd_from_env, write_or_close_sync_fd},
    runtime::{
        args::{RuntimeArgsGenerator, generate_runtime_args},
        ctl::{setup_console_fifo, setup_terminal_control_fifo},
        process::RuntimeProcess,
        stdio::{create_pipe, handle_stdio, read_pipe, receive_console_fd},
    },
    unix_socket::{RemoteSocket, SocketType, UnixSocket},
};

/// Represents Runtime session.
/// Handles spawning of runtime process, reading its stdio, writing its
/// pid and error code as well as the event loop to forward its log messages
/// to log plugins.
#[derive(Default)]
pub struct RuntimeSession {
    /// The low-level runtime process.
    process: RuntimeProcess,

    /// File descriptor for synchronization pipe.
    /// The process executing `conmon` uses this pipe to receive the container PID
    /// or error message in case the runtime cannot be executed.
    sync_pipe_fd: Option<OwnedFd>,

    /// Represents container's stdin.
    /// Any data written here are sent to container's input.
    workerfd_stdin: Option<OwnedFd>,

    /// Represents container's stdout.
    /// Any data read from here should be treated as container's standard output.
    mainfd_stdout: Option<OwnedFd>,

    /// Represents container's stderr.
    /// Any data read from here should be treated as container's standard error.
    mainfd_stderr: Option<OwnedFd>,

    /// Exit code of `process`.
    exit_code: i32,

    /// UnixSocket for `attach`.
    /// The process executing conmon uses it to attach to container. It opens new
    /// connection to socket and any data read from it are forwarded to
    /// container's stdin (`workerfd_stdin`). Any data read from `mainfd_stdout`
    /// and `mainfd_stderr` are forwarded to that `attach` connection, so
    /// the process executing conmon can handle them.
    attach_socket: Option<UnixSocket>,

    /// UnixSocket for runtime `--console-socket`. The runtime connects to it
    /// and sends a `terminal_socket` of terminal to that connection. This is used if
    /// `--terminal` is used.
    console_socket: Option<UnixSocket>,

    /// Terminal created by runtime and received using `console_socket`. Anything written
    /// to this socket is forwarded to container's stdin. Anything read from it is treated
    /// as container's stdout.
    terminal_socket: Option<RemoteSocket>,

    /// Fifo for `ctl` file used by parent to control conmon session.
    ctl_fifo: Option<RemoteSocket>,

    /// Fifo for `winsz` file used by parent to control terminal window size.
    winsz_fifo: Option<RemoteSocket>,

    /// The PID of container created by the runtime.
    container_pid: i32,

    /// The exit status of container.
    container_status: i32,

    // Time (unix timestamp) after which the session should terminate
    timeout: u64,

    // True if timeout occured.
    timed_out: bool,

    /// RemoteSocket for OOM handling.
    oom_socket: Option<RemoteSocket>,
}

impl RuntimeSession {
    pub fn new() -> Self {
        Self {
            process: RuntimeProcess::new(),
            exit_code: -1,
            container_pid: -1,
            container_status: -1,
            timed_out: false,
            ..Default::default()
        }
    }

    /// Returns the exit_code.
    pub fn exit_code(&self) -> i32 {
        self.exit_code
    }

    /// Returns the container's exit_code.
    pub fn container_exit_code(&self) -> i32 {
        self.container_status
    }

    // Helper function to read and return the container pid.
    fn read_container_pid(&self, common: &CommonCfg) -> ConmonResult<i32> {
        let contents = fs::read_to_string(common.container_pidfile.as_path())?;
        let pid = contents.trim().parse::<i32>().map_err(|e| {
            ConmonError::new(
                format!(
                    "Invalid PID contents in {}: {} ({})",
                    common.container_pidfile.display(),
                    contents.trim(),
                    e
                ),
                1,
            )
        })?;
        Ok(pid)
    }

    /// Launches the Runtime binary and ensures the stdio pipes are created and
    /// pid written to locations according to configuration.
    pub fn launch(
        &mut self,
        common: &CommonCfg,
        args_gen: &impl RuntimeArgsGenerator,
        attach: bool,
    ) -> ConmonResult<()> {
        // Get the sync_pipe FD. It is used by the conmon caller to obtain the container_pid
        // or the runtime error message later.
        self.sync_pipe_fd = get_pipe_fd_from_env("_OCI_SYNCPIPE")?;

        // Get the attach pipe FD. We use it later to inform parent that attach
        // socket is ready.
        let mut attach_pipe_fd: Option<OwnedFd> = None;
        if attach {
            attach_pipe_fd = get_pipe_fd_from_env("_OCI_ATTACHPIPE")?;
            if attach_pipe_fd.is_none() {
                return Err(ConmonError::new(
                    "--attach specified but _OCI_ATTACHPIPE was not set",
                    1,
                ));
            }
        }

        // Create the `attach` socket which is used to send data to container's stdin.
        let mut attach_socket = UnixSocket::new(
            SocketType::Console,
            common.full_attach,
            common.bundle.clone(),
            common.socket_dir_path.clone(),
            common.cuuid.clone(),
        );
        attach_socket.listen(
            Some(PathBuf::from("attach")),
            SockType::SeqPacket,
            SockFlag::SOCK_NONBLOCK | SockFlag::SOCK_CLOEXEC,
            Mode::from_bits_truncate(0o700),
        )?;
        self.attach_socket = Some(attach_socket);

        // Create `ctl` fifo.
        self.ctl_fifo = Some(setup_terminal_control_fifo(common)?);
        self.winsz_fifo = Some(setup_console_fifo(common)?);

        // Inform the parent that the attach socket is ready.
        if let Some(fd) = attach_pipe_fd.take() {
            write_or_close_sync_fd(fd, 0, None, common.api_version, true)?;
        }

        // Get the start pipe FD. We wait for the parent to write some data into it
        // before continuing with the runtime process execution. This is a simple
        // sync mechanism between parent and us.
        let mut start_pipe_fd = get_pipe_fd_from_env("_OCI_STARTPIPE")?;
        if let Some(fd) = start_pipe_fd.take() {
            // It is OK to just once from the pipe here. The pipe is used as a sync
            // mechanism. We do not care about the read data at all.
            let mut buf = [0u8; 8192];
            read_pipe(&fd, &mut buf)?;
            // If we are using attach, we want to keep the start_pipe_fd valid,
            // so it can be passed to `process.spawn()` and block the runtime
            // execution for the second time. Parent use that to inform us that
            // it is attached to container.
            if attach {
                start_pipe_fd = Some(fd);
            }
        }

        // Create console socket if the --terminal option is used.
        // We later pass the path to the socket to runtime and it sends a fd
        // through it which will be used to communicate with the container.
        if common.terminal {
            let mut console_socket = UnixSocket::new(
                SocketType::Terminal,
                common.full_attach,
                common.bundle.clone(),
                None,
                None,
            );
            // The path is None here - listen will use unique random name.
            console_socket.listen(
                None,
                SockType::Stream,
                SockFlag::SOCK_CLOEXEC,
                Mode::from_bits_truncate(0o700),
            )?;
            self.console_socket = Some(console_socket);
        }

        // Set the timeout if --timeout is used.
        if let Some(t) = common.timeout {
            let now = SystemTime::now().duration_since(UNIX_EPOCH)?;
            self.timeout = now.as_secs() + t as u64;
        }


        // Generate the list of arguments for runtime.
        let runtime_args = generate_runtime_args(common, args_gen, self.console_socket.as_ref())?;

        // Generate the stdin and stdout.
        let mainfd_stdin_stdio: Stdio;
        let mainfd_stdout_stdio: Stdio;
        if common.terminal {
            // If we are using a terminal, we pass the console-socket to runtime
            // and the communication will happen using that socket. So there is no
            // need to create custom stdin/stdout pipes.
            mainfd_stdin_stdio = Stdio::null();
            mainfd_stdout_stdio = Stdio::null();
        } else {
            // Create the pipe to handle stdin in case the --stdin is used.
            if common.stdin {
                let (fd_out, fd_in) = create_pipe()?;
                // We store the "in" part of the pipe to `self.workerfd_stdin`, so anything
                // written to it can be sent to container's stdin.
                self.workerfd_stdin = Some(fd_in);
                // We pass the "out" part of the pipe to `self.process.spawn`, so the container
                // can read from it.
                mainfd_stdin_stdio = Stdio::from(fd_out)
            } else {
                // No stdin -> null.
                mainfd_stdin_stdio = Stdio::null();
            }

            // Create the pipe to handle stdout.
            let (fd_out, fd_in) = create_pipe()?;
            // We pass the "in" part of the pipe to `self.process.spawn`, so the container
            // can write to it.
            mainfd_stdout_stdio = Stdio::from(fd_in);
            // We store the "out" part of the pipe to `self.workerfd_stout`, so anything
            // written to it can be treated as container's stdout.
            self.mainfd_stdout = Some(fd_out);
        }

        // We create stderr every time, because we need to capture the runtime error log.
        let (mainfd_stderr, workerfd_stderr) = create_pipe()?;
        self.mainfd_stderr = Some(mainfd_stderr);

        // Run the `runtime create` and store our PID after first fork to `conmon_pidfile.
        self.process.spawn(
            &runtime_args,
            mainfd_stdin_stdio,
            mainfd_stdout_stdio,
            Stdio::from(workerfd_stderr),
            start_pipe_fd,
            common.replace_listen_pid,
        )?;

        // Store the RuntimeProcess::pid in the `conmon_pidfile`.
        if let Some(pidfile) = &common.conmon_pidfile {
            std::fs::write(pidfile, self.process.pid().to_string())?;
        }

        Ok(())
    }

    /// Write the container pid file to all the configured locations.
    pub fn write_container_pid_file(&mut self, common: &CommonCfg) -> ConmonResult<()> {
        // Pass the container_pid to sync_pipe if there is one.
        if let Some(fd) = self.sync_pipe_fd.take() {
            self.container_pid = self.read_container_pid(common)?;
            self.sync_pipe_fd =
                write_or_close_sync_fd(fd, self.container_pid, None, common.api_version, false)?;
            self.oom_socket = Some(setup_oom_handling(self.container_pid, &common.persist_dir, &common.bundle)?);
        }
        Ok(())
    }

    /// Writes the Runtime exit code to all the configured locations.
    pub fn write_exit_code(&mut self, api_version: i32) -> ConmonResult<()> {
        #[allow(clippy::collapsible_if)]
        if let Some(fd) = self.sync_pipe_fd.take() {
            if self.timed_out {
                let err_str = "command timed out";
                self.sync_pipe_fd =
                    write_or_close_sync_fd(fd, self.exit_code, Some(err_str), api_version, true)?;
            } else if let Some(mainfd_stderr) = &self.mainfd_stderr {
                // TODO: We are reading just once here and if container prints more than
                // a buffer sizeto stderr, we ignore whatever does not fid into the buffer.
                // This might be a problem, but the original conmon-v2 code behaves the same way.
                let mut err_bytes = [0u8; 8192];
                let n = read_pipe(mainfd_stderr, &mut err_bytes)?;
                let err_str = std::str::from_utf8(&err_bytes[..n])?;
                error!("Runtime exited with error: {err_str}");
                self.sync_pipe_fd =
                    write_or_close_sync_fd(fd, self.exit_code, Some(err_str), api_version, true)?;
            }
        }
        Ok(())
    }

    /// Waits for the Runtime process to exit. Returns the exit code.
    pub fn wait(&mut self) -> ConmonResult<i32> {
        self.exit_code = self.process.wait()?;
        Ok(self.exit_code)
    }

    /// Waits for the Runtime process to exit with zero exit code.
    pub fn wait_for_success(&mut self, api_version: i32) -> ConmonResult<()> {
        // Wait until the `runtime create` finishes.
        self.wait()?;
        if self.exit_code != 0 {
            self.write_exit_code(api_version)?;
            return Err(ConmonError::new(
                format!("Runtime exited with status: {}", self.exit_code),
                1,
            ));
        }
        Ok(())
    }

    /// Function executed periodically during the event-loop execuction.
    /// Returns true when event-loop should finish.
    fn idle_callback(&mut self) -> ConmonResult<bool> {
        // Stop the event-loop if we reach a timeout.
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?;
        if self.timeout > 0 && now.as_secs() > self.timeout {
            info!("Timed out - exiting event-loop.");
            // Kill the container in case it exists.
            if self.container_pid > 0 {
                let pid = Pid::from_raw(self.container_pid);
                // Get the process group ID of the container
                let pgid = getpgid(Some(pid))?;

                // NOTE:
                // If pgid is 1, calling kill(-1, SIGKILL) would kill everything we have permission for.
                if pgid.as_raw() > 1 {
                    // kill entire process group
                    kill(Pid::from_raw(-pgid.as_raw()), Signal::SIGKILL)?;
                } else {
                    // kill only the container process
                    kill(pid, Signal::SIGKILL)?;
                }
            }
            self.timed_out = true;
            // Quite the event-loop.
            return Ok(false);
        }

        // Wait for any child to finish (non-blocking).
        let res = waitpid(Pid::from_raw(-1), Some(WaitPidFlag::WNOHANG));

        match res {
            // Interrupted by signal - retry.
            Err(Errno::EINTR) => Ok(true),

            // no more child processes
            Err(Errno::ECHILD) => {
                // Before quitting, probe the container_pid.
                // It might not be a direct child.
                if self.container_pid > 0 {
                    // Nix kill function does not support 0 signal, so we have to use libc one.
                    let rc = unsafe { libc::kill(self.container_pid, 0) };
                    if rc == 0 {
                        info!(
                            "Container process {} is still alive but not a direct child",
                            self.container_pid
                        );
                        // Do not quit main loop yet...
                        return Ok(true);
                    } else if Errno::last() == Errno::ESRCH {
                        // Process exited.
                        info!(
                            "Container process {} has exited (detected via kill probe)",
                            self.container_pid
                        );
                        // We cannot get real exit status.
                        self.container_status = 0;
                        self.container_pid = -1;
                        return Ok(false);
                    }
                }

                Ok(false)
            }

            // some other waitpid error
            Err(e) => Err(ConmonError::new(
                format!("Failed to read child process status: {e}"),
                1,
            )),

            // No child has changed state.
            Ok(WaitStatus::StillAlive) => Ok(true),

            // Child exiteed, store the exit code.
            Ok(WaitStatus::Exited(p, code)) => {
                if p == Pid::from_raw(self.container_pid) {
                    self.container_status = code;
                } else if p == Pid::from_raw(self.process.pid()) {
                    self.exit_code = code;
                }
                Ok(false)
            }

            Ok(
                WaitStatus::Signaled(_, _, _)
                | WaitStatus::Stopped(_, _)
                | WaitStatus::Continued(_)
                | WaitStatus::PtraceEvent(_, _, _)
                | WaitStatus::PtraceSyscall(_),
            ) => {
                // Just keep looping until StillAlive or ECHILD.
                Ok(true)
            }
        }
    }

    /// Runs the event loop handling the container Runtime stdio.
    pub fn run_event_loop(
        &mut self,
        log_plugin: &mut dyn LogPlugin,
        leave_stdin_open: bool,
    ) -> ConmonResult<()> {
        #[allow(clippy::collapsible_if)]
        if let Some(mainfd_err) = self.mainfd_stderr.take() {
            handle_stdio(
                log_plugin,
                self.mainfd_stdout.take(),
                mainfd_err,
                self.workerfd_stdin.take(),
                self.attach_socket.take(),
                self.terminal_socket.take(),
                self.ctl_fifo.take(),
                self.winsz_fifo.take(),
                self.oom_socket.take(),
                leave_stdin_open,
                || self.idle_callback(),
            )?;
            return Ok(());
        }

        Err(ConmonError::new("RuntimeSession called without stdio", 1))
    }

    /// Waits for the runtime to sent the terminal fd to conmon.
    pub fn wait_for_terminal_creation(&mut self) -> ConmonResult<()> {
        debug!("Waiting for terminal creation.");
        if let Some(cs) = self.console_socket.take() {
            self.terminal_socket = Some(receive_console_fd(cs)?);
        }
        debug!("Terminal created.");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nix::unistd::write;
    use tempfile::tempdir;

    #[test]
    fn exit_code_defaults_and_accessor_work() {
        let s = RuntimeSession::new();
        assert_eq!(s.exit_code(), -1);
    }

    #[test]
    fn read_container_pid_returns_pid_on_valid_file() -> ConmonResult<()> {
        let tmp = tempdir()?;
        let pid_path = tmp.path().join("pidfile.txt");
        std::fs::write(&pid_path, b"12345\n")?;

        let cfg = CommonCfg {
            container_pidfile: pid_path,
            conmon_pidfile: None,
            api_version: 1,
            ..Default::default()
        };

        let sess = RuntimeSession::new();
        let pid = sess.read_container_pid(&cfg)?;
        assert_eq!(pid, 12345);
        Ok(())
    }

    #[test]
    fn read_container_pid_errors_on_invalid_contents() -> ConmonResult<()> {
        let tmp = tempdir()?;
        let pid_path = tmp.path().join("pidfile.txt");
        std::fs::write(&pid_path, b"not-a-number")?;

        let cfg = CommonCfg {
            container_pidfile: pid_path.clone(),
            conmon_pidfile: None,
            api_version: 1,
            ..Default::default()
        };

        let sess = RuntimeSession::new();
        let err = sess.read_container_pid(&cfg).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("Invalid PID contents"),
            "unexpected error: {msg}"
        );
        Ok(())
    }

    #[test]
    fn run_event_loop_errors_without_stdio() -> ConmonResult<()> {
        struct NoopLog;
        impl crate::logging::plugin::LogPlugin for NoopLog {
            fn write(&mut self, _is_stdout: bool, _data: &[u8]) -> ConmonResult<()> {
                Ok(())
            }
        }

        let mut sess = RuntimeSession::new();
        let err = sess.run_event_loop(&mut NoopLog, false).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("RuntimeSession called without stdio"),
            "unexpected error message: {msg}"
        );
        Ok(())
    }

    #[test]
    fn write_exit_code_is_noop_without_sync_fd() -> ConmonResult<()> {
        // If no syncpipe FD was captured in launch(), write_exit_code should simply succeed
        // and not attempt to read from stderr (so it must not block).
        let mut sess = RuntimeSession::new();
        let (r, w) = create_pipe()?;
        // Write something into the write end so that if the code ever tried to read it,
        // there would be data instead of blocking — but the code MUST NOT read
        // because sync_pipe_fd is None.
        write(w, "err\n".as_bytes())?;
        // Prevent double-close of w after turning into File
        // (we wrote and let File drop; raw FD consumed).

        sess.mainfd_stderr = Some(r);

        // Also set an exit code to make sure the function path is covered.
        sess.exit_code = 42;

        // No sync_pipe_fd set -> should be a no-op success.
        let res = sess.write_exit_code(1);
        assert!(
            res.is_ok(),
            "write_exit_code should be a no-op when sync fd is None"
        );
        Ok(())
    }
}
