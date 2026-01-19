use crate::error::{ConmonError, ConmonResult};

use log::{error, info, warn};
use nix::errno::Errno;
use nix::sys::wait::waitpid;
use nix::unistd::Pid;

use std::os::fd::RawFd;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;
use std::{fs, thread};

use nix::libc::{PR_SET_CHILD_SUBREAPER, close, prctl};

/// Sets this process as subreaper.
///
/// A subreaper becomes the ancestor for orphaned descendants in its subtree.
/// This is needed to read the exit status of all child processes.
///
/// # Argments
///
/// * `enabled` - When true, this process is a subreaper.
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
///
/// Cleanups all the child processes and calls the exit command.
///
/// # Arguments
///
/// * `exit_command` - The path to exit command.
/// * `exit_command_args` - Vector of arguments for exit command.
/// * `exit_command_delay` - Optional delay in seconnds to wit before
///   executing the exit command.
pub fn run_exit_command(
    exit_command: Option<PathBuf>,
    exit_command_args: Vec<String>,
    exit_command_delay: Option<i32>,
) -> ConmonResult<()> {
    // Stop being a subreaper.
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
        // No exit-command, so return.
        return Ok(());
    }

    // Wait for a delay if used.
    if let Some(delay) = exit_command_delay {
        thread::sleep(Duration::from_secs(delay as u64));
    }

    // Build and spawn the exit command.
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

/// Writes exit files into persistent_path and exit_dir.
pub fn write_exit_files(
    exit_status: i32,
    persist_path: Option<&PathBuf>,
    exit_dir: Option<&PathBuf>,
    cid: Option<&String>,
) {
    let status_str: String = exit_status.to_string();

    // Write the exit file to container persistent directory if it is specified
    if let Some(persist_path) = persist_path {
        let ctr_exit_file_path: PathBuf = persist_path.join("exit");
        if let Err(e) = fs::write(&ctr_exit_file_path, &status_str) {
            error!(
                "Failed to write {} to container exit file {}: {}",
                status_str,
                ctr_exit_file_path.display(),
                e
            );
        }
    }

    // Writing to this directory helps if a daemon process wants to monitor
    // all container exits using inotify.
    if let Some(exit_dir) = exit_dir {
        if let Some(cid) = cid {
            let exit_file_path: PathBuf = exit_dir.join(cid);
            if let Err(e) = fs::write(&exit_file_path, &status_str) {
                error!(
                    "Failed to write {} to exit file {}: {}",
                    status_str,
                    exit_file_path.display(),
                    e
                );
            }
        }
    }
}

const OPEN_FILES_DIR: &str = "/proc/self/fd";

#[derive(Default, Clone)]
pub struct OpenFilesSnapshot {
    max_fd: RawFd,
    // List of file descriptors that existed at snapshot time.
    // Kept sorted and unique.
    open_fds: Vec<RawFd>,
}

impl OpenFilesSnapshot {
    fn mark(&mut self, fd: RawFd) {
        if fd < 0 {
            return;
        }

        match self.open_fds.binary_search(&fd) {
            Ok(_) => {
                // already present
            }
            Err(pos) => {
                self.open_fds.insert(pos, fd);
            }
        }

        if fd > self.max_fd {
            self.max_fd = fd;
        }
    }

    fn has(&self, fd: RawFd) -> bool {
        if fd < 0 {
            return false;
        }

        self.open_fds.binary_search(&fd).is_ok()
    }

    pub fn remove(&mut self, fd: RawFd) {
        if fd < 0 {
            return;
        }

        if let Ok(pos) = self.open_fds.binary_search(&fd) {
            self.open_fds.remove(pos);
        }
    }
}

pub fn snapshot_open_fds() -> OpenFilesSnapshot {
    let mut snap = OpenFilesSnapshot::default();

    // Best-effort: if we can't read the directory, do nothing.
    let Ok(dir) = std::fs::read_dir(OPEN_FILES_DIR) else {
        return snap;
    };

    // Read the number of open fds.
    for entry in dir.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.starts_with('.') {
            continue;
        }
        let Ok(fd) = name.parse::<RawFd>() else {
            continue;
        };
        snap.mark(fd);
    }

    snap
}

/// Close all file descriptors that were open at snapshot time, except:
/// - stdin(0), stdout(1), stderr(2)
pub fn close_all_except_stdio(snap: &OpenFilesSnapshot) {
    if snap.open_fds.is_empty() {
        return;
    }

    for fd in 3..=snap.max_fd {
        if snap.has(fd) {
            info!("Closing {}", fd);
            // Best-effort: ignore EBADF and any other errors (common when racing / already closed).
            let _ = unsafe { close(fd) };
        }
    }
}
