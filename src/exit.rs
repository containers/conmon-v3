use crate::error::{ConmonError, ConmonResult};

use log::{info, warn};
use nix::errno::Errno;
use nix::sys::wait::waitpid;
use nix::unistd::Pid;

use std::path::PathBuf;
use std::process::Command;
use std::thread;
use std::time::Duration;

use nix::libc::{PR_SET_CHILD_SUBREAPER, prctl};

/// Sets this process as subreaper.
/// A subreaper becomes the ancestor for orphaned descendants in its subtree.
pub fn set_subreaper(enabled: bool) -> ConmonResult<()> {
    let flag = if enabled { 1 } else { 0 };

    let rc = unsafe { prctl(PR_SET_CHILD_SUBREAPER, flag, 0, 0, 0) };

    if rc == 0 {
        Ok(())
    } else {
        Err(ConmonError::new(
            format!("Failed to set subreaper to {enabled}: {}", Errno::last()),
            1,
        ))
    }
}

/// Cleanup function to execute at the end of conmon execution.
pub fn run_exit_command(
    exit_command: Option<PathBuf>,
    exit_command_args: Vec<String>,
    exit_command_delay: Option<i32>,
) -> ConmonResult<()> {
    let r = set_subreaper(false);
    if let Err(e) = r {
        warn!("{}", e);
    }

    // Clean-up any possible children left.
    loop {
        let res = waitpid(Pid::from_raw(-1), None);

        match res {
            // ret < 0 && errno == EINTR  -> keep looping
            Err(Errno::EINTR) => continue,

            // ret < 0 && errno != EINTR  -> break out of loop
            Err(_e) => break,

            // ret > 0
            Ok(_status) => {}
        }
    }

    if exit_command.is_none() {
        return Ok(());
    }

    if let Some(delay) = exit_command_delay {
        thread::sleep(Duration::from_secs(delay as u64));
    }

    // Build and spawn the child.
    if let Some(program) = &exit_command {
        let mut cmd = Command::new(program);
        cmd.args(exit_command_args.clone());

        info!(
            "Starting exit command: {:?} {:?}",
            program, exit_command_args
        );
        let mut child = cmd
            .spawn()
            .map_err(|e| ConmonError::new(format!("Failed to spawn: {e}"), 1))?;

        let exit_code = child.wait()?;
        info!("Exit command exited with: {exit_code}.");
    }
    Ok(())
}
