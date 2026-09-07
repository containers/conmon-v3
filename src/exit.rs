use crate::Cid;
use crate::error::{ConmonError, ConmonResult};

use atomic_write_file::AtomicWriteFile;
use log::{error, info, warn};
use nix::errno::Errno;
use nix::sys::wait::waitpid;
use nix::unistd::Pid;

use std::io::{self, Write};
use std::os::fd::RawFd;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::Duration;

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

/// Atomically write `contents` to `path` via `atomic-write-file`.
///
/// Same intent as conmon-v2 `g_file_set_contents`: inotify waiters never see a
/// truncated/empty exit file.
fn write_file_atomic(path: &Path, contents: &str) -> io::Result<()> {
    let mut file = AtomicWriteFile::options().open(path)?;
    file.write_all(contents.as_bytes())?;
    file.commit()?;
    Ok(())
}

/// Returns true when `path` is a direct child of `base`.
fn path_stays_under(base: &Path, path: &Path) -> bool {
    path.starts_with(base) && path.parent() == Some(base)
}

/// Writes exit files into persistent_path and exit_dir.
pub fn write_exit_files(
    exit_status: i32,
    persist_path: Option<&PathBuf>,
    exit_dir: Option<&PathBuf>,
    cid: Option<&Cid>,
) {
    let status_str: String = exit_status.to_string();

    // Write the exit file to container persistent directory if it is specified
    if let Some(persist_path) = persist_path {
        let ctr_exit_file_path: PathBuf = persist_path.join("exit");
        if let Err(e) = write_file_atomic(&ctr_exit_file_path, &status_str) {
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
            let exit_file_path = exit_dir.join(cid.as_str());
            if !path_stays_under(exit_dir, &exit_file_path) {
                error!(
                    "Exit file path {} escapes exit directory {}",
                    exit_file_path.display(),
                    exit_dir.display()
                );
                return;
            }

            if let Err(e) = write_file_atomic(&exit_file_path, &status_str) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Cid;
    use std::io;
    use std::thread;
    use tempfile::tempdir;

    #[test]
    fn write_exit_files_writes_contents() -> io::Result<()> {
        let dir = tempdir()?;
        let persist = dir.path().to_path_buf();

        write_exit_files(42, Some(&persist), None, None);

        let path = persist.join("exit");
        assert_eq!(std::fs::read_to_string(&path)?, "42");
        Ok(())
    }

    #[test]
    fn write_exit_files_replaces_existing_file() -> io::Result<()> {
        let dir = tempdir()?;
        let persist = dir.path().to_path_buf();
        std::fs::write(persist.join("exit"), "old")?;

        write_exit_files(7, Some(&persist), None, None);

        assert_eq!(std::fs::read_to_string(persist.join("exit"))?, "7");
        Ok(())
    }

    #[test]
    fn write_exit_files_leaves_no_temp_files() -> io::Result<()> {
        let dir = tempdir()?;
        let persist = dir.path().to_path_buf();

        write_exit_files(0, Some(&persist), None, None);

        let leftovers: Vec<_> = std::fs::read_dir(dir.path())?
            .filter_map(|e| e.ok())
            .map(|e| e.file_name())
            .filter(|n| {
                let name = n.to_string_lossy();
                name != "exit" && name.starts_with('.')
            })
            .collect();
        assert!(leftovers.is_empty(), "leftover temps: {leftovers:?}");
        Ok(())
    }

    #[test]
    fn write_exit_files_replaces_destination_symlink_not_target() -> io::Result<()> {
        let dir = tempdir()?;
        let persist = dir.path().to_path_buf();
        let target = dir.path().join("secret");
        std::fs::write(&target, "untouched")?;
        std::os::unix::fs::symlink(&target, persist.join("exit"))?;

        write_exit_files(1, Some(&persist), None, None);

        let path = persist.join("exit");
        assert_eq!(std::fs::read_to_string(&path)?, "1");
        assert_eq!(std::fs::read_to_string(&target)?, "untouched");
        assert!(!std::fs::symlink_metadata(&path)?.file_type().is_symlink());
        Ok(())
    }

    #[test]
    fn write_exit_files_to_exit_dir_uses_cid_name() -> io::Result<()> {
        let dir = tempdir()?;
        let exit_dir = dir.path().to_path_buf();
        let cid = Cid::parse("abc123").unwrap();

        write_exit_files(9, None, Some(&exit_dir), Some(&cid));

        assert_eq!(std::fs::read_to_string(exit_dir.join(cid.as_str()))?, "9");
        Ok(())
    }

    #[test]
    fn write_file_atomic_concurrent_writers_do_not_clobber_temp() -> io::Result<()> {
        let dir = tempdir()?;
        let path = dir.path().join("exit");

        let mut handles = Vec::new();
        for i in 0..8 {
            let path = path.clone();
            handles.push(thread::spawn(move || {
                write_file_atomic(&path, &i.to_string())
            }));
        }
        for h in handles {
            h.join().expect("thread panicked")?;
        }

        let contents = std::fs::read_to_string(&path)?;
        assert!(
            matches!(
                contents.as_str(),
                "0" | "1" | "2" | "3" | "4" | "5" | "6" | "7"
            ),
            "unexpected contents {contents:?}"
        );
        Ok(())
    }

    #[test]
    fn path_stays_under_rejects_prefix_sibling_paths() {
        let dir = tempdir().unwrap();
        let base = dir.path().join("exit");
        std::fs::create_dir_all(&base).unwrap();
        let sibling = dir.path().join("exit-extra").join("file");

        assert!(path_stays_under(&base, &base.join("container1")));
        assert!(!path_stays_under(&base, &sibling));
    }

    #[test]
    fn write_exit_files_does_not_escape_exit_dir() {
        let dir = tempdir().unwrap();
        let exit_dir = dir.path().join("exits");
        std::fs::create_dir_all(&exit_dir).unwrap();
        let outside = dir.path().join("outside");

        for bad_cid in ["", ".", "..", "foo/bar", "../outside", "foo\0bar"] {
            assert!(
                Cid::parse(bad_cid).is_err(),
                "expected {bad_cid:?} to be rejected"
            );
            write_exit_files(42, None, Some(&exit_dir), None);
            assert!(
                !outside.exists(),
                "rejected cid {bad_cid:?} must not create {outside:?}"
            );
            assert!(
                std::fs::read_dir(&exit_dir).unwrap().next().is_none(),
                "rejected cid {bad_cid:?} must not create entries under exit_dir"
            );
        }
    }

    #[test]
    fn write_exit_files_accepts_extended_container_ids() -> io::Result<()> {
        let dir = tempdir()?;
        let exit_dir = dir.path().to_path_buf();

        for cid_str in ["sha256:deadbeef", "ctr+with+plus", "café", "容器"] {
            let cid = Cid::parse(cid_str).unwrap();
            write_exit_files(3, None, Some(&exit_dir), Some(&cid));
            assert_eq!(std::fs::read_to_string(exit_dir.join(cid_str))?, "3");
        }
        Ok(())
    }

    #[test]
    fn write_exit_files_writes_only_under_exit_dir() {
        let dir = tempdir().unwrap();
        let exit_dir = dir.path().to_path_buf();

        let cid = Cid::parse("container1").unwrap();
        write_exit_files(7, None, Some(&exit_dir), Some(&cid));

        let exit_file = exit_dir.join("container1");
        assert_eq!(std::fs::read_to_string(exit_file).unwrap(), "7");
    }

    #[test]
    fn write_exit_files_does_not_follow_symlink_in_exit_dir() {
        use std::os::unix::fs::symlink;

        let dir = tempdir().unwrap();
        let exit_dir = dir.path().to_path_buf();

        let outside = dir.path().join("outside");
        std::fs::write(&outside, "original").unwrap();
        symlink(&outside, exit_dir.join("container1")).unwrap();

        let cid = Cid::parse("container1").unwrap();
        write_exit_files(99, None, Some(&exit_dir), Some(&cid));

        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "original");
        assert_eq!(
            std::fs::read_to_string(exit_dir.join("container1")).unwrap(),
            "99"
        );
    }

    #[test]
    fn write_exit_files_does_not_truncate_hard_linked_target() {
        let dir = tempdir().unwrap();
        let exit_dir = dir.path().to_path_buf();

        let outside = dir.path().join("outside");
        std::fs::write(&outside, "original").unwrap();
        std::fs::hard_link(&outside, exit_dir.join("container1")).unwrap();

        let cid = Cid::parse("container1").unwrap();
        write_exit_files(42, None, Some(&exit_dir), Some(&cid));

        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "original");
        assert_eq!(
            std::fs::read_to_string(exit_dir.join("container1")).unwrap(),
            "42"
        );
    }

    #[test]
    fn write_exit_files_replaces_fifo_with_regular_file() {
        use nix::sys::stat::Mode;
        use nix::unistd::mkfifo;

        let dir = tempdir().unwrap();
        let exit_dir = dir.path().to_path_buf();
        let fifo_path = exit_dir.join("container1");
        mkfifo(&fifo_path, Mode::S_IRUSR | Mode::S_IWUSR).unwrap();

        let cid = Cid::parse("container1").unwrap();
        write_exit_files(13, None, Some(&exit_dir), Some(&cid));

        let meta = std::fs::metadata(&fifo_path).unwrap();
        assert!(meta.is_file());
        assert_eq!(std::fs::read_to_string(&fifo_path).unwrap(), "13");
    }
}
