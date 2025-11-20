use crate::error::{ConmonError, ConmonResult};
use crate::runtime::stdio::read_pipe;

use nix::fcntl::{OFlag, open};
use nix::sys::signal::{SigSet, SigmaskHow, Signal, kill, pthread_sigmask};
use nix::sys::stat::Mode;
use nix::sys::wait::{WaitStatus, waitpid};
use nix::unistd::{ForkResult, Pid, dup2_stderr, dup2_stdin, dup2_stdout, fork, setsid};

use std::io::{Error, Result as IoResult};
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::process::CommandExt; // for pre_exec
use std::process::{Command, Stdio, exit};

/// Convert a nix::Error into std::io::Error (for use inside pre_exec closure).
fn io_err(e: nix::Error) -> Error {
    Error::from_raw_os_error(e as i32)
}

/// Block signals in the parent before we spawn.
/// Returns the old mask, which the child will restore in `pre_exec`.
fn block_signals() -> ConmonResult<SigSet> {
    let mut mask = SigSet::empty();
    mask.add(Signal::SIGTERM);
    mask.add(Signal::SIGQUIT);
    mask.add(Signal::SIGINT);
    mask.add(Signal::SIGHUP);

    let mut oldmask = SigSet::empty();
    pthread_sigmask(SigmaskHow::SIG_BLOCK, Some(&mask), Some(&mut oldmask))
        .map_err(|e| ConmonError::new(format!("Failed to block signals: {e}"), 1))?;
    Ok(oldmask)
}

/// Helper function to redirect stdio to /dev/null.
fn redirect_self_to_devnull() -> ConmonResult<()> {
    // stdin -> /dev/null (read side)
    let fd_in = open("/dev/null", OFlag::O_RDONLY, Mode::empty())?;
    dup2_stdin(fd_in.as_fd())?;

    // stdout/stderr -> /dev/null (write side)
    let fd_out = open("/dev/null", OFlag::O_WRONLY, Mode::empty())?;
    dup2_stdout(fd_out.as_fd())?;
    dup2_stderr(fd_out.as_fd())?;

    Ok(())
}

/// Represents single RuntimeProcess.
/// For is low-level implementation. Use RuntimeSession for more convenient
/// way to work with Runtime.
#[derive(Default)]
pub struct RuntimeProcess {
    pid: i32,
}

impl RuntimeProcess {
    pub fn new() -> Self {
        Self { pid: -1 }
    }

    /// Spawn the runtime binary defined by `args`.
    /// The stdio is redirected to `workerfd_stdin`, `workerfd_stdout` and `workerfd_stderr`.
    /// Returns the PID.
    pub fn spawn(
        &mut self,
        args: &[String],
        workerfd_stdin: Stdio,
        workerfd_stdout: Stdio,
        workerfd_stderr: Stdio,
        mut start_pipe_fd: Option<OwnedFd>,
    ) -> ConmonResult<i32> {
        if args.is_empty() {
            return Err(ConmonError::new(
                "Failed to execute runtime binary: empty args",
                1,
            ));
        }

        redirect_self_to_devnull()?;

        unsafe {
            match fork() {
                // In the parent: exit immediately so the child won't be a process group leader.
                Ok(ForkResult::Parent { .. }) => {
                    exit(0);
                }
                // In the child: continue execution.
                Ok(ForkResult::Child) => {}
                Err(e) => {
                    return Err(ConmonError::new(format!("Failed to fork: {e}"), 1));
                }
            }
        }

        // Wait with the `spawn()` until parent tells us to start the runtime
        // using the start_pipe_fd (if defined).
        if let Some(fd) = start_pipe_fd.take() {
            // It is OK to just once from the pipe here. The pipe is used as a sync
            // mechanism. We do not care about the read data at all.
            let mut buf = [0u8; 8192];
            read_pipe(&fd, &mut buf)?;
        }

        // Block signals in the parent so none are delivered between fork and exec.
        let oldmask = block_signals()?;

        // Child setup performed between fork and exec.
        fn child_setup(oldmask: &SigSet) -> IoResult<()> {
            // Detach from controlling terminal: new session.
            setsid().map_err(io_err)?;

            // Restore (unblock) the parent's original signal mask.
            pthread_sigmask(SigmaskHow::SIG_SETMASK, Some(oldmask), None).map_err(io_err)?;

            // Set conservative umask.
            nix::sys::stat::umask(nix::sys::stat::Mode::from_bits_truncate(0o022));
            Ok(())
        }

        // Build and spawn the child.
        let program = &args[0];
        let argv = &args[1..];
        let mut cmd = Command::new(program);
        cmd.args(argv)
            .stdin(workerfd_stdin)
            .stdout(workerfd_stdout)
            .stderr(workerfd_stderr);

        unsafe {
            cmd.pre_exec(move || child_setup(&oldmask));
        }

        let child = cmd
            .spawn()
            .map_err(|e| ConmonError::new(format!("Failed to spawn: {e}"), 1))?;

        self.pid = child.id() as i32;
        Ok(self.pid)
    }

    /// Returns the runtime process pid or -1 if it's not running.
    pub fn pid(&self) -> i32 {
        self.pid
    }

    /// Block until the runtime process exits. Returns the exit code.
    pub fn wait(&self) -> ConmonResult<i32> {
        let pid = Pid::from_raw(self.pid);

        loop {
            match waitpid(pid, None) {
                Ok(WaitStatus::Exited(_, code)) => return Ok(code),
                Ok(WaitStatus::Signaled(_, sig, _core_dumped)) => {
                    return Err(ConmonError::new(
                        format!("Runtime process exited due to signal: {sig:?}"),
                        1,
                    ));
                }
                // These shouldn’t occur with no flags, but if they do, keep waiting.
                Ok(WaitStatus::StillAlive)
                | Ok(WaitStatus::Stopped(_, _))
                | Ok(WaitStatus::Continued(_))
                | Ok(WaitStatus::PtraceEvent(_, _, _))
                | Ok(WaitStatus::PtraceSyscall(_)) => continue,

                // Interrupted - continue.
                Err(nix::Error::EINTR) => continue,

                Err(e) => {
                    // Try to kill the child, then surface the error.
                    let _ = kill(pid, Signal::SIGKILL);
                    return Err(ConmonError::new(
                        format!("Failed to wait for runtime process to exit: {e}"),
                        1,
                    ));
                }
            }
        }
    }
}
