use crate::error::{ConmonError, ConmonResult};

use nix::sys::signal::{SigSet, SigmaskHow, Signal, kill, pthread_sigmask};
use nix::sys::wait::{WaitStatus, waitpid};
use nix::unistd::{ForkResult, Pid, fork, setsid};

use std::io::{Error, Result as IoResult};
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

/// Run the runtime binary defined by `args`.
pub fn run_runtime(args: &[String]) -> ConmonResult<i32> {
    if args.is_empty() {
        return Err(ConmonError::new(
            "Failed to execute runtime binary: empty args",
            1,
        ));
    }

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
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    unsafe {
        cmd.pre_exec(move || child_setup(&oldmask));
    }

    let child = cmd
        .spawn()
        .map_err(|e| ConmonError::new(format!("Failed to spawn: {e}"), 1))?;

    Ok(child.id() as i32)
}

/// Block until the runtime process defined by `runtime_pid` exits.
/// Returns the runtime exit code.
pub fn wait_for_runtime(runtime_pid: i32) -> ConmonResult<i32> {
    let pid = Pid::from_raw(runtime_pid);

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
