use std::os::fd::{AsFd, AsRawFd};
use std::time::{SystemTime, UNIX_EPOCH};
use std::{fs, os::fd::OwnedFd, path::PathBuf, process::Stdio};

use log::{debug, error, info};
use nix::sys::signal::{SigSet, SigmaskHow, Signal, kill, pthread_sigmask};
use nix::sys::signalfd::{SfdFlags, SignalFd};
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

use crate::exit::{OpenFilesSnapshot, close_all_except_stdio};
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

    // True if container started.
    container_started: bool,

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

    /// UnixSocket for systemd notify.socket.
    notify_socket: Option<RemoteSocket>,

    /// Host counter-part of `notify_socket`.
    sdnotify_socket_path: Option<PathBuf>,

    /// The signal-fd to handle incomming UNIX signals.
    signals: Option<SignalFd>,

    // Open file descriptor snapshot.
    open_files: OpenFilesSnapshot,
}

impl RuntimeSession {
    pub fn new(open_files: OpenFilesSnapshot) -> Self {
        Self {
            process: RuntimeProcess::new(),
            exit_code: -1,
            container_pid: -1,
            container_status: -1,
            timed_out: false,
            container_started: false,
            open_files,
            ..Default::default()
        }
    }

    /// Returns the exit_code of "runtime" process.
    ///
    /// # Returns
    ///
    /// * The exit code of "runtime" process.
    pub fn exit_code(&self) -> i32 {
        self.exit_code
    }

    /// Returns the container's exit_code.
    ///
    /// # Returns
    ///
    /// * The exit code of container.
    pub fn container_exit_code(&self) -> i32 {
        self.container_status
    }

    /// Reads and returns the container's PID.
    ///
    /// The "runtime" executable creates container and stores its process into
    /// `container_pidfile`. This function reads it and returns it so we can
    /// later call `waitpid()` using this PID and read the container's exit code.
    ///
    /// # Arguments
    ///
    /// * `common` - The Conmon common configuration.
    ///
    /// # Returns
    ///
    /// * Parsed container's PID.
    ///
    /// # Errors
    ///
    /// * [`ConmonError`] if the PID file does not exit or cannot be parsed.
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

    /// Launches the "runtime" binary.
    ///
    /// This function prepares the complete environment for the "runtime" binary,
    /// including the stdio pipes, sd-notify socket, attach socket, signal-fd and
    /// many more.
    ///
    /// # Arguments
    ///
    /// * `common` - The Conmon common configuration.
    /// * `args_gen` - The Conmon subcommand specific arguments generator.
    /// * `attach` - True if `--attach` Conmon flag is used.
    ///
    /// # Errors
    ///
    /// * [`ConmonError`] on any error.
    pub fn launch(
        &mut self,
        common: &CommonCfg,
        args_gen: &impl RuntimeArgsGenerator,
        attach: bool,
    ) -> ConmonResult<()> {
        // Get the sync_pipe FD. It is used by the Conmon caller to obtain the container_pid
        // or the runtime error message later.
        self.sync_pipe_fd = get_pipe_fd_from_env("_OCI_SYNCPIPE")?;

        // If we have some sync_pipe fd, we need to remove it from `open_files` snapshot,
        // otherwise we would close this fd prematurely before exit.
        if let Some(fd) = &self.sync_pipe_fd {
            self.open_files.remove(fd.as_raw_fd());
        }

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

        // If we have some attach_pipe_fd fd, we need to remove it from `open_files` snapshot,
        // otherwise we would close this fd prematurely before exit.
        if let Some(fd) = &attach_pipe_fd {
            self.open_files.remove(fd.as_raw_fd());
        }

        // If logging is not passthrough, we will create attach UNIX socket and other
        // sockets the control the terminal.
        if !common.logging_passthrough {
            // Create the `attach` socket which is used to send data to container's stdin.
            let mut attach_socket = UnixSocket::new(
                SocketType::Console,
                common.full_attach,
                common.bundle.clone(),
                Some(common.socket_dir_path.clone()),
                common.cuuid.clone(),
            );
            attach_socket.bind(
                Some(PathBuf::from("attach")),
                SockType::SeqPacket,
                SockFlag::SOCK_NONBLOCK | SockFlag::SOCK_CLOEXEC,
                Mode::from_bits_truncate(0o700),
            )?;
            attach_socket.listen()?;
            self.attach_socket = Some(attach_socket);

            // Create `ctl` fifo - this is used to control Conmon using simple commands
            // sent to it.
            self.ctl_fifo = Some(setup_terminal_control_fifo(common)?);

            // Create `winsz` fifo - this is the outdated way to control terminal
            // size. It is replaced with the `ctl`, but we still support this old way.
            self.winsz_fifo = Some(setup_console_fifo(common)?);

            // Inform the parent that the attach socket is ready.
            if let Some(fd) = attach_pipe_fd.take() {
                write_or_close_sync_fd(fd, 0, None, common.api_version, true)?;
            }
        }

        // Get the start pipe FD. We wait for the parent to write some data into it
        // before continuing with the runtime process execution. This is a simple
        // sync mechanism between parent and us.
        let mut start_pipe_fd = get_pipe_fd_from_env("_OCI_STARTPIPE")?;
        if let Some(fd) = &start_pipe_fd {
            self.open_files.remove(fd.as_raw_fd());
        }

        if let Some(fd) = start_pipe_fd.take() {
            // It is OK to just once from the pipe here. The pipe is used as a sync
            // mechanism. We do not care about the read data at all.
            info!("exec with attach is waiting for start message from parent");
            let mut buf = [0u8; 8192];
            read_pipe(&fd, &mut buf)?;
            // If we are using attach, we want to keep the start_pipe_fd valid,
            // so it can be passed to `process.spawn()` and block the runtime
            // execution for the second time. Parent uses that to inform us that
            // it is attached to container.
            if attach {
                start_pipe_fd = Some(fd);
            }
            info!("exec with attach got start message from parent");
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
            console_socket.bind(
                None,
                SockType::Stream,
                SockFlag::SOCK_CLOEXEC,
                Mode::from_bits_truncate(0o700),
            )?;
            console_socket.listen()?;
            self.console_socket = Some(console_socket);
        }

        // Create systemd notify.socket if requested.
        if common.sdnotify_socket.is_some() {
            let mut notify_socket = UnixSocket::new(
                SocketType::Notify,
                true,
                common.bundle.clone(),
                Some(common.socket_dir_path.clone()),
                common.cuuid.clone(),
            );
            notify_socket.bind(
                Some(PathBuf::from("notify/notify.sock")),
                SockType::Datagram,
                SockFlag::SOCK_NONBLOCK | SockFlag::SOCK_CLOEXEC,
                Mode::from_bits_truncate(0o777),
            )?;
            self.notify_socket = Some(notify_socket.into());
            self.sdnotify_socket_path = common.sdnotify_socket.clone();
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
            info!("--terminal used: Passing null stdio and stdout to runtime binary.");
            mainfd_stdin_stdio = Stdio::null();
            mainfd_stdout_stdio = Stdio::null();
        } else {
            // Create the pipe to handle stdin in case the --stdin is used.
            if common.stdin {
                let (fd_out, fd_in) = create_pipe()?;
                info!("Created pipe for stdin: {:?} {:?}", fd_out, fd_in);
                // We store the "in" part of the pipe to `self.workerfd_stdin`, so anything
                // written to it can be sent to container's stdin.
                self.workerfd_stdin = Some(fd_in);
                // We pass the "out" part of the pipe to `self.process.spawn`, so the container
                // can read from it.
                mainfd_stdin_stdio = Stdio::from(fd_out)
            } else {
                info!("--stdin not used: Passing null stdio to runtime binary");
                // No stdin -> null.
                mainfd_stdin_stdio = Stdio::null();
            }

            // Create the pipe to handle stdout.
            let (fd_out, fd_in) = create_pipe()?;
            info!("Created pipe for stdout: {:?} {:?}", fd_out, fd_in);
            // We pass the "in" part of the pipe to `self.process.spawn`, so the container
            // can write to it.
            mainfd_stdout_stdio = Stdio::from(fd_in);
            // We store the "out" part of the pipe to `self.workerfd_stout`, so anything
            // written to it can be treated as container's stdout.
            self.mainfd_stdout = Some(fd_out);
        }

        // We create stderr every time, because we need to capture the runtime error log.
        let (mainfd_stderr, workerfd_stderr) = create_pipe()?;
        info!(
            "Created pipe for stderr: {:?} {:?}",
            mainfd_stderr, workerfd_stderr
        );
        self.mainfd_stderr = Some(mainfd_stderr);

        // Run the `runtime create` and store our PID after first fork to `conmon_pidfile`.
        self.process.spawn(
            &runtime_args,
            mainfd_stdin_stdio,
            mainfd_stdout_stdio,
            Stdio::from(workerfd_stderr),
            start_pipe_fd,
            common.replace_listen_pid,
            common.logging_passthrough,
            !common.sync_flag,
            &common.conmon_pidfile,
        )?;

        // Setup the signal-fd for the signals we want to handle.
        let mut mask = SigSet::empty();
        mask.add(Signal::SIGTERM);
        mask.add(Signal::SIGQUIT);
        mask.add(Signal::SIGINT);
        pthread_sigmask(SigmaskHow::SIG_BLOCK, Some(&mask), None)?;
        let signals = SignalFd::with_flags(&mask, SfdFlags::SFD_CLOEXEC | SfdFlags::SFD_NONBLOCK)?;
        self.signals = Some(signals);

        Ok(())
    }

    /// Writes the container pid file to all the configured locations.
    ///
    /// This funtion is called after the `launch`. It writes the container PID
    /// into `sync_pipe_fd` and other locations so parent can get the container's PID.
    ///
    /// # Arguments
    ///
    /// * `common` - The Conmon common configuration.
    ///
    /// # Errors
    ///
    /// * [`ConmonError`] on any error.
    pub fn write_container_pid_file(&mut self, common: &CommonCfg) -> ConmonResult<()> {
        // Read the container PID and store it.
        self.container_pid = self.read_container_pid(common)?;

        // We know the container started, so note it.
        self.container_started = true;

        // Setup the out-of-mana (eh, *-memory) handler, so we can detect OOM event
        // and pass it to parent.
        self.oom_socket =
            setup_oom_handling(self.container_pid, &common.persist_dir, &common.bundle)?;

        // Pass the container_pid to sync_pipe if there is one.
        if let Some(fd) = self.sync_pipe_fd.take() {
            self.sync_pipe_fd =
                write_or_close_sync_fd(fd, self.container_pid, None, common.api_version, false)?;
        }

        Ok(())
    }

    /// Writes the "runtime" exit code to all the configured locations.
    ///
    /// This funtion is called at the end of Conmon Session execution and ensures
    /// the exit code of container or runtime is written to all the locations where
    /// parents look for it.
    ///
    /// # Arguments
    ///
    /// * `api_version` - The `--api-version` value.
    /// * `write_exit_code` - When `true`, the real `self.exit_code` is written
    ///   instead of -1.
    ///
    /// # Errors
    ///
    /// * [`ConmonError`] on any error.
    pub fn write_exit_code(&mut self, api_version: i32, write_exit_code: bool) -> ConmonResult<()> {
        // Before exiting, we close all the fds injected to Conmon by parent, since we will
        // be exiting soon.
        close_all_except_stdio(&self.open_files);

        // Send exit code toe sync_pipe.
        if let Some(fd) = self.sync_pipe_fd.take() {
            // Prepare the exit code according to the `write_exit_code`.
            let mut to_report = -1;

            if self.container_started {
                to_report = self.container_status
            } else if write_exit_code && self.exit_code > 0 {
                to_report = -self.exit_code;
            }

            // On timeout, write custom error message.
            if self.timed_out {
                let err_str = "command timed out";
                self.sync_pipe_fd =
                    write_or_close_sync_fd(fd, to_report, Some(err_str), api_version, true)?;
            } else if let Some(mainfd_stderr) = &self.mainfd_stderr {
                // If we have stderr from runtime, read it and pass the error message to parent.
                // TODO: We are reading just once here and if container prints more than
                // a buffer sizeto stderr, we ignore whatever does not fid into the buffer.
                // This might be a problem, but the original conmon-v2 code behaves the same way.
                let mut err_bytes = [0u8; 8192];
                let n = read_pipe(mainfd_stderr, &mut err_bytes)?;
                let err_str = std::str::from_utf8(&err_bytes[..n])?;
                error!("Runtime exited with error: {err_str}");
                self.sync_pipe_fd =
                    write_or_close_sync_fd(fd, to_report, Some(err_str), api_version, true)?;
            } else {
                // We do not have any error message, so just pass None as err_str.
                self.sync_pipe_fd = write_or_close_sync_fd(fd, to_report, None, api_version, true)?;
            }
        }
        Ok(())
    }

    /// Waits for the Runtime process to exit. Returns the exit code.
    ///
    /// # Returns
    ///
    /// * The "runtime" process exit code.
    ///
    /// # Errors
    ///
    /// * [`ConmonError`] on any error.
    pub fn wait(&mut self) -> ConmonResult<i32> {
        self.exit_code = self.process.wait()?;
        Ok(self.exit_code)
    }

    /// Waits for the Runtime process to exit. Returns the exit code.
    ///
    /// In case of non-zero exit code, calls `write_exit_code` and returns `ConmonError`.
    ///
    /// # Returns
    ///
    /// * The "runtime" process exit code.
    ///
    /// # Arguments
    ///
    /// * `api_version` - The `--api-version` value.
    /// * `write_exit_code` - When `true`, the real `self.exit_code` is written
    ///   instead of -1.
    ///
    /// # Errors
    ///
    /// * [`ConmonError`] on any error.
    pub fn wait_for_success(
        &mut self,
        api_version: i32,
        write_exit_code: bool,
    ) -> ConmonResult<()> {
        // Wait until the `runtime create` finishes.
        self.wait()?;
        if self.exit_code != 0 {
            self.write_exit_code(api_version, write_exit_code)?;
            return Err(ConmonError::new(
                format!("Runtime exited with status: {}", self.exit_code),
                1,
            ));
        }
        Ok(())
    }

    /// Function executed periodically during the event-loop execuction.
    ///
    /// This function monitors the signal-fd, all the children processes and
    /// also stops the event-loop in case of execution timeout.
    ///
    /// # Returns
    ///
    /// * True if the event-loop should still continue.
    ///
    /// # Arguments
    ///
    /// * `signal_received` - True if there is a signal to read from signal-fd.
    ///
    /// # Errors
    ///
    /// * [`ConmonError`] on any error.
    fn idle_callback(&mut self, signal_received: bool) -> ConmonResult<bool> {
        // We received a signal.
        if signal_received {
            if let Some(signals) = &self.signals {
                // Read the signal.
                match signals.read_signal() {
                    Ok(Some(info)) => {
                        if let Ok(sig) = Signal::try_from(info.ssi_signo as i32) {
                            // Forward the signal the container if it's running.
                            info!("Received signal: {:?}", sig);
                            if self.container_pid > 0 {
                                let pid = Pid::from_raw(self.container_pid);
                                kill(pid, sig)?;
                            }
                        }
                    }
                    Ok(None) => return Ok(true),
                    Err(_) => return Ok(true),
                }
            }
            return Ok(true);
        }

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
                    } else {
                        info!("No more child processes.");
                        return Ok(false);
                    }
                }

                // If container has not started yet, keep running.
                Ok(!self.container_started)
            }

            // some other waitpid error
            Err(e) => Err(ConmonError::new(
                format!("Failed to read child process status: {e}"),
                1,
            )),

            // No child has changed state.
            Ok(WaitStatus::StillAlive) => Ok(true),

            // Child exited, store the exit code.
            Ok(WaitStatus::Exited(p, code)) => {
                if p == Pid::from_raw(self.container_pid) {
                    self.container_status = code;
                    info!("Container exited: {}", self.container_status);
                    return Ok(false);
                } else if p == Pid::from_raw(self.process.pid()) {
                    self.exit_code = code;
                    info!("Runtime exited: {}", self.exit_code);
                    return Ok(false);
                } else {
                    info!("Uknown child {} exited", p);
                }
                Ok(true)
            }

            // Child killed with a signal, store it as exit code.
            Ok(WaitStatus::Signaled(p, s, _)) => {
                let code: i32 = s as i32;
                if p == Pid::from_raw(self.container_pid) {
                    self.container_status = 128 + code;
                    info!("Container killed with signal: {}", self.container_status);
                    return Ok(false);
                } else if p == Pid::from_raw(self.process.pid()) {
                    self.exit_code = 128 + code;
                    info!("Runtime killed with signal: {}", self.exit_code);
                    return Ok(false);
                } else {
                    info!("Uknown child {} exited", p);
                }
                Ok(true)
            }

            Ok(
                WaitStatus::Stopped(_, _)
                | WaitStatus::Continued(_)
                | WaitStatus::PtraceEvent(_, _, _)
                | WaitStatus::PtraceSyscall(_),
            ) => {
                // Just continue with the event-loop.
                Ok(true)
            }
        }
    }

    /// Runs the main event-loop.
    ///
    /// The event-loop polls all the file descriptors which drives the Conmon's logic.
    ///
    /// # Arguments
    ///
    /// * `log_plugin` - The LogPlugin to log messages into.
    /// * `leave_stdin_open` - True if --leave-stdin-open is used.
    /// * `stdin_attached` - True if --stdin is used.
    ///
    /// # Errors
    ///
    /// * [`ConmonError`] on any error.
    pub fn run_event_loop(
        &mut self,
        log_plugin: &mut dyn LogPlugin,
        leave_stdin_open: bool,
        stdin_attached: bool,
    ) -> ConmonResult<()> {
        #[allow(clippy::collapsible_if)]
        if let Some(mainfd_err) = self.mainfd_stderr.take() {
            let mut signal_fd: i32 = -1;
            if let Some(signals) = &self.signals {
                signal_fd = signals.as_fd().as_raw_fd();
            }
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
                self.notify_socket.take(),
                self.sdnotify_socket_path.take(),
                stdin_attached,
                leave_stdin_open,
                signal_fd,
                |signal_received| self.idle_callback(signal_received),
            )?;
            return Ok(());
        }

        Err(ConmonError::new("RuntimeSession called without stdio", 1))
    }

    /// Waits for the runtime to send the terminal fd to conmon.
    ///
    /// When `--terminal` is used, this function is called to receive the terminal
    /// fd from runtime.
    ///
    /// # Errors
    ///
    /// * [`ConmonError`] on any error.
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
        let open_files = OpenFilesSnapshot::default();
        let s = RuntimeSession::new(open_files);
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

        let open_files = OpenFilesSnapshot::default();
        let sess = RuntimeSession::new(open_files);
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

        let open_files = OpenFilesSnapshot::default();
        let sess = RuntimeSession::new(open_files);
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

        let open_files = OpenFilesSnapshot::default();
        let mut sess = RuntimeSession::new(open_files);
        let err = sess.run_event_loop(&mut NoopLog, false, false).unwrap_err();
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
        let open_files = OpenFilesSnapshot::default();
        let mut sess = RuntimeSession::new(open_files);
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
        let res = sess.write_exit_code(1, true);
        assert!(
            res.is_ok(),
            "write_exit_code should be a no-op when sync fd is None"
        );
        Ok(())
    }
}
