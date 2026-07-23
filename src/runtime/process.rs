use crate::error::{ConmonError, ConmonResult};
use crate::exit::set_subreaper;
use crate::runtime::stdio::read_pipe;

use log::{info, warn};
use nix::errno::Errno;
use nix::fcntl::{OFlag, open};
use nix::sys::signal::{SigSet, SigmaskHow, Signal, kill, pthread_sigmask};
use nix::sys::stat::Mode;
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::{
    ForkResult, Pid, dup2_stderr, dup2_stdin, dup2_stdout, fork, getpid, getppid, setsid,
};

use std::env;
use std::fs;
use std::io::{Error, Result as IoResult};
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
// for pre_exec
use std::process::{Command, Stdio, exit};

/// Convert a nix::Error into std::io::Error (for use inside pre_exec closure).
fn io_err(e: nix::Error) -> Error {
    Error::from_raw_os_error(e as i32)
}

/// Parse the one-letter process state from the contents of `/proc/<pid>/stat`.
fn state_from_proc_stat(contents: &str) -> Option<char> {
    let rparen = contents.rfind(')')?;
    contents.get(rparen + 2..)?.chars().next()
}

/// Reads the one-letter process state from `/proc/<pid>/stat`.
///
/// Returns `None` if the process does not exist or the stat file cannot be parsed.
pub fn proc_state(pid: i32) -> Option<char> {
    if pid <= 0 {
        return None;
    }
    let contents = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    state_from_proc_stat(&contents)
}

/// Returns true when `pid` refers to a live, non-zombie process.
///
/// `kill(pid, 0)` also succeeds for zombie processes, so callers must not use
/// that syscall alone to decide whether a container is still running.
pub fn is_running_process(pid: i32) -> bool {
    matches!(proc_state(pid), Some(s) if s != 'Z')
}

/// Non-blocking wait for a specific PID.
///
/// As subreaper, conmon may reap orphaned container processes this way even when
/// they are not returned by `waitpid(-1, WNOHANG)`.
pub fn try_wait_process(pid: i32) -> Result<Option<WaitStatus>, Errno> {
    let pid = Pid::from_raw(pid);
    loop {
        match waitpid(pid, Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::StillAlive) => return Ok(None),
            Ok(status @ (WaitStatus::Exited(_, _) | WaitStatus::Signaled(_, _, _))) => {
                return Ok(Some(status));
            }
            Ok(_) => return Ok(None),
            Err(Errno::ECHILD) => return Ok(None),
            Err(Errno::EINTR) => continue,
            Err(e) => return Err(e),
        }
    }
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

/// Helper function to replace LISTEN_PID with the proper afterk-fork PID.
fn update_listen_pid(replace_listen_pid: bool) {
    // If LISTEN_PID env is set, we may need to update it to the new child process
    if let Ok(listenpid) = env::var("LISTEN_PID") {
        // Try to parse LISTEN_PID as an integer
        let lpid: i32 = match listenpid.parse() {
            Ok(v) if v > 0 => v,
            _ => {
                warn!("Invalid LISTEN_PID {}", listenpid);
                return;
            }
        };

        let parent_pid = getppid().as_raw();

        // If we should replace it, or it matches the parent PID, set it to our PID
        if replace_listen_pid || lpid == parent_pid {
            let pid = getpid().as_raw();
            let pidstr = pid.to_string();
            unsafe { env::set_var("LISTEN_PID", &pidstr) };
        }
    }
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
    #[allow(clippy::too_many_arguments)]
    pub fn spawn(
        &mut self,
        args: &[String],
        workerfd_stdin: Stdio,
        workerfd_stdout: Stdio,
        workerfd_stderr: Stdio,
        mut start_pipe_fd: Option<OwnedFd>,
        replace_listen_pid: bool,
        logging_is_passthrough: bool,
        double_fork: bool,
        pidfile: &Option<PathBuf>,
    ) -> ConmonResult<i32> {
        if args.is_empty() {
            return Err(ConmonError::new(
                "Failed to execute runtime binary: empty args",
                1,
            ));
        }

        if !logging_is_passthrough {
            redirect_self_to_devnull()?;
        }

        if double_fork {
            unsafe {
                match fork() {
                    // In the parent: exit immediately so the child won't be a process group leader.
                    Ok(ForkResult::Parent { child }) => {
                        self.pid = child.as_raw();
                        // Store the RuntimeProcess::pid in the `conmon_pidfile`.
                        if let Some(pidfile) = &pidfile {
                            std::fs::write(pidfile, self.pid.to_string())?;
                        }
                        exit(0);
                    }
                    // In the child: continue execution.
                    Ok(ForkResult::Child) => {}
                    Err(e) => {
                        return Err(ConmonError::new(format!("Failed to fork: {e}"), 1));
                    }
                }
            }
        }

        // Detach from controlling terminal: new session.
        setsid()?;

        // Enable subreaper, so we can wait for container process exit code.
        set_subreaper(true)?;

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
        fn child_setup(oldmask: &SigSet, replace_listen_pid: bool) -> IoResult<()> {
            // Restore (unblock) the parent's original signal mask.
            pthread_sigmask(SigmaskHow::SIG_SETMASK, Some(oldmask), None).map_err(io_err)?;

            // Set conservative umask.
            nix::sys::stat::umask(nix::sys::stat::Mode::from_bits_truncate(0o022));

            update_listen_pid(replace_listen_pid);
            Ok(())
        }

        info!("Executing {:?}", args);

        // Build and spawn the child.
        let program = &args[0];
        let argv = &args[1..];
        let mut cmd = Command::new(program);
        cmd.args(argv);
        if !logging_is_passthrough {
            cmd.stdin(workerfd_stdin)
                .stdout(workerfd_stdout)
                .stderr(workerfd_stderr);
        }
        unsafe {
            cmd.pre_exec(move || child_setup(&oldmask, replace_listen_pid));
        }

        let child = cmd
            .spawn()
            .map_err(|e| ConmonError::new(format!("Failed to spawn: {e}"), 1))?;

        if logging_is_passthrough {
            redirect_self_to_devnull()?;
        }

        self.pid = child.id() as i32;
        info!("Conmon PID: {}", self.pid);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_from_proc_stat_parses_zombie() {
        assert_eq!(state_from_proc_stat("42 (sh) Z 1 42 42"), Some('Z'));
    }

    #[test]
    fn state_from_proc_stat_parses_running() {
        assert_eq!(state_from_proc_stat("99 (sleep) S 1 99 99"), Some('S'));
    }

    #[test]
    fn proc_state_for_current_process_is_not_zombie() {
        let pid = std::process::id() as i32;
        assert!(is_running_process(pid));
        assert_ne!(proc_state(pid), Some('Z'));
    }
}
