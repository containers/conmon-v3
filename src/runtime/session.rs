use std::{fs, os::fd::OwnedFd, path::PathBuf, process::Stdio};

use nix::sys::{
    socket::{SockFlag, SockType},
    stat::Mode,
};

use crate::{
    cli::CommonCfg,
    error::{ConmonError, ConmonResult},
    logging::plugin::LogPlugin,
    parent_pipe::{get_pipe_fd_from_env, write_or_close_sync_fd},
    runtime::{
        args::{RuntimeArgsGenerator, generate_runtime_args},
        process::RuntimeProcess,
        stdio::{create_pipe, handle_stdio, read_pipe},
    },
    unix_socket::{SocketType, UnixSocket},
};

/// Represents Runtime session.
/// Handles spawning of runtime process, reading its stdio, writing its
/// pid and error code as well as the event loop to forward its log messages
/// to log plugins.
#[derive(Default)]
pub struct RuntimeSession {
    process: RuntimeProcess,
    sync_pipe_fd: Option<OwnedFd>,
    workerfd_stdin: Option<OwnedFd>,
    mainfd_stdout: Option<OwnedFd>,
    mainfd_stderr: Option<OwnedFd>,
    exit_code: i32,
    attach_socket: Option<UnixSocket>,
}

impl RuntimeSession {
    pub fn new() -> Self {
        Self {
            process: RuntimeProcess::new(),
            exit_code: -1,
            ..Default::default()
        }
    }

    /// Returns the exit_code.
    pub fn exit_code(&self) -> i32 {
        self.exit_code
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

        // Generate the list of arguments for runtime.
        let runtime_args = generate_runtime_args(common, args_gen)?;

        // Generate pipes to handle stdio.
        let (workerfd_stdin, mainfd_stdin_stdio) = if common.stdin {
            let (fd_out, fd_in) = create_pipe()?;
            (Some(fd_in), Stdio::from(fd_out))
        } else {
            (None, Stdio::null())
        };
        let (mainfd_stdout, workerfd_stdout) = create_pipe()?;
        let (mainfd_stderr, workerfd_stderr) = create_pipe()?;
        self.workerfd_stdin = workerfd_stdin;
        self.mainfd_stdout = Some(mainfd_stdout);
        self.mainfd_stderr = Some(mainfd_stderr);

        // Run the `runtime create` and store our PID after first fork to `conmon_pidfile.
        self.process.spawn(
            &runtime_args,
            mainfd_stdin_stdio,
            Stdio::from(workerfd_stdout),
            Stdio::from(workerfd_stderr),
            start_pipe_fd,
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
            let container_pid = self.read_container_pid(common)?;
            self.sync_pipe_fd =
                write_or_close_sync_fd(fd, container_pid, None, common.api_version, false)?;
        }
        Ok(())
    }

    /// Writes the Runtime exit code to all the configured locations.
    pub fn write_exit_code(&mut self, api_version: i32) -> ConmonResult<()> {
        #[allow(clippy::collapsible_if)]
        if let Some(fd) = self.sync_pipe_fd.take() {
            if let Some(mainfd_stderr) = &self.mainfd_stderr {
                // TODO: We are reading just once here and if container prints more than
                // a buffer sizeto stderr, we ignore whatever does not fid into the buffer.
                // This might be a problem, but the original conmon-v2 code behaves the same way.
                let mut err_bytes = [0u8; 8192];
                let n = read_pipe(mainfd_stderr, &mut err_bytes)?;
                let err_str = std::str::from_utf8(&err_bytes[..n])?;
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

    /// Runs the event loop handling the container Runtime stdio.
    pub fn run_event_loop(&mut self, log_plugin: &mut dyn LogPlugin) -> ConmonResult<()> {
        #[allow(clippy::collapsible_if)]
        if let Some(mainfd_out) = &self.mainfd_stdout {
            if let Some(mainfd_err) = &self.mainfd_stderr {
                handle_stdio(
                    log_plugin,
                    mainfd_out,
                    mainfd_err,
                    self.workerfd_stdin.take(),
                    self.attach_socket.as_ref(),
                )?;
                return Ok(());
            }
        }

        Err(ConmonError::new("RuntimeSession called without stdio", 1))
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
        let err = sess.run_event_loop(&mut NoopLog).unwrap_err();
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
