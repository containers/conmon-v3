use chrono::{Datelike, Local, Timelike};

use crate::{
    error::{ConmonError, ConmonResult},
    logging::plugin::{LogPlugin, LogPluginCfg},
};

use nix::errno::Errno;
use nix::fcntl::{open, openat, OFlag, AtFlags, readlink};
use nix::sys::stat::{fstat, fstatat, stat, Mode, SFlag};
use nix::unistd::{geteuid, getuid};
use nix::libc;

use std::{
    cmp::min,
    ffi::CString,
    fs::{File, OpenOptions},
    io::Write,
    os::fd::{AsRawFd, BorrowedFd, OwnedFd, RawFd},
    os::unix::{ffi::OsStrExt, fs::OpenOptionsExt},
    path::{Path, PathBuf},
};

use log::warn;

const TSBUFLEN: usize = 44;

/// A simple file-based logging plugin.
///
/// Writes all log data to the configured file path.
pub struct FileLogger {
    file: File,
    stdout_has_partial: bool,
    stderr_has_partial: bool,
    no_sync: bool,
    max_size: u64,
    global_max_size: u64,
    bytes_written: u64,
    total_bytes_written: u64,
    path: PathBuf,
    max_files: i32,
    allowlist_dirs: Option<Vec<PathBuf>>,
    opt_rotate: bool,
}

impl FileLogger {
    pub fn new(cfg: &LogPluginCfg) -> ConmonResult<Self> {
        if !cfg.log_labels.is_empty() {
            return Err(ConmonError::new("k8s-file doesn't support --log-label", 1));
        }
        if cfg.log_tag.is_some() {
            return Err(ConmonError::new("k8s-file doesn't support --log-tag", 1));
        }

        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o640)
            .open(&cfg.path)
            .map_err(|e| {
                ConmonError::new(
                    format!("Failed to open log file {}: {}", cfg.path.display(), e),
                    1,
                )
            })?;

        let metadata = file.metadata()?;

        Ok(Self {
            file,
            stdout_has_partial: false,
            stderr_has_partial: false,
            no_sync: cfg.no_sync,
            max_size: cfg.max_size as u64,
            global_max_size: cfg.global_max_size as u64,
            bytes_written: metadata.len(),
            total_bytes_written: metadata.len(),
            path: cfg.path.clone(),
            max_files: cfg.max_files,
            allowlist_dirs: cfg.allowlist_dirs.clone(),
            opt_rotate: cfg.rotate,
        })
    }

    fn get_line_len(line_len: &mut isize, buf: &[u8], buflen: isize) -> bool {
        let mut partial = false;
        let len = buflen as usize;

        if let Some(pos) = buf[..len].iter().position(|&c| c == b'\n') {
            *line_len = (pos + 1) as isize;
        } else {
            *line_len = len as isize;
            partial = true;
        }
        partial
    }

    fn set_k8s_timestamp(buf: &mut [u8], pipename: &str) {
        let now = Local::now();
        let offset = now.offset().local_minus_utc();
        let off_sign = if offset < 0 { '-' } else { '+' };
        let off_abs = offset.abs();
        let hours = off_abs / 3600;
        let mins = (off_abs % 3600) / 60;

        // "YYYY-MM-DDTHH:MM:SS.NNNNNNNNN+01:00 stdout "
        let nsec = now.timestamp_subsec_nanos();
        let s = format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:09}{}{:02}:{:02} {} ",
            now.year(),
            now.month(),
            now.day(),
            now.hour(),
            now.minute(),
            now.second(),
            nsec,
            off_sign,
            hours,
            mins,
            pipename
        );

        let bytes = s.as_bytes();
        let n = min(buf.len().saturating_sub(1), bytes.len());
        buf[..n].copy_from_slice(&bytes[..n]);
        if !buf.is_empty() {
            buf[buf.len() - 1] = 0;
        }
    }

    fn canonicalize(path: &Path) -> ConmonResult<PathBuf> {
        std::fs::canonicalize(path).map_err(|e| {
            ConmonError::new(format!("Failed to canonicalize {}: {e}", path.display()), 1)
        })
    }

    fn is_path_in_allowlist(&self, canonical_path: &Path) -> bool {
        let Some(dirs) = &self.allowlist_dirs else {
            return true; // no allowlist configured
        };
        if dirs.is_empty() {
            return true; // treat empty allowlist like no allowlist
        }

        for dir in dirs {
            // Skip empty
            if dir.as_os_str().is_empty() {
                continue;
            }

            let allowed_canon = match std::fs::canonicalize(dir) {
                Ok(p) => p,
                Err(_) => {
                    // mirror C: warn and continue
                    warn!("Invalid allowlist directory");
                    continue;
                }
            };

            // Component-wise prefix check (safer than string prefix).
            if canonical_path.starts_with(&allowed_canon) {
                return true;
            }
        }

        false
    }

    /// Atomic symlink validation using file descriptors to reduce race conditions.
    /// Returns true if any component is a symlink OR an unsafe error occurs.
    fn path_contains_symlinks_atomic(&self, canonical_path: Option<&Path>) -> bool {
        let Some(path) = canonical_path else {
            return true; // treat None as unsafe, matching the C
        };

        let raw = path.as_os_str().as_bytes();
        if raw.is_empty() {
            return false;
        }

        // Starting directory fd: root for absolute paths; AT_FDCWD for relative.
        // We track an optional owned directory fd; when None, we use AT_FDCWD.
        let mut cur_fd: Option<OwnedFd> = None;

        let is_abs = raw.first() == Some(&b'/');
        let mut comps = path.components();

        if is_abs {
            match open("/", OFlag::O_PATH | OFlag::O_CLOEXEC, Mode::empty()) {
                Ok(fd) => {
                    cur_fd = Some(fd);
                }
                Err(_) => return true,
            }
            let _ = comps.next(); // skip root
        }

        let comps_vec: Vec<_> = comps.collect();

        for (idx, comp) in comps_vec.iter().enumerate() {
            let name_bytes = comp.as_os_str().as_bytes();
            if name_bytes.is_empty() {
                continue;
            }

            let name = match CString::new(name_bytes.to_vec()) {
                Ok(c) => c,
                Err(_) => return true,
            };

            let result = match &cur_fd {
                Some(fd) => fstatat(fd, name.as_c_str(), AtFlags::AT_SYMLINK_NOFOLLOW),
                None => {
                    let dir_fd = unsafe { BorrowedFd::borrow_raw(libc::AT_FDCWD) };
                    fstatat(dir_fd, name.as_c_str(), AtFlags::AT_SYMLINK_NOFOLLOW)
                }
            };

            match result {
                Ok(st) => {
                    let kind = SFlag::from_bits_truncate(st.st_mode);

                    if kind.contains(SFlag::S_IFLNK) {
                        return true;
                    }

                    let has_more = idx + 1 < comps_vec.len();
                    if has_more && kind.contains(SFlag::S_IFDIR) {
                        let open_result = match &cur_fd {
                            Some(fd) => openat(
                                fd,
                                name.as_c_str(),
                                OFlag::O_PATH | OFlag::O_CLOEXEC,
                                Mode::empty(),
                            ),
                            None => {
                                let dir_fd =
                                    unsafe { BorrowedFd::borrow_raw(libc::AT_FDCWD) };
                                openat(
                                    dir_fd,
                                    name.as_c_str(),
                                    OFlag::O_PATH | OFlag::O_CLOEXEC,
                                    Mode::empty(),
                                )
                            }
                        };

                        match open_result {
                            Ok(next_fd) => {
                                cur_fd = Some(next_fd);
                            }
                            Err(_) => return true, // treat access failure as unsafe
                        }
                    }
                }
                Err(e) => {
                    if e != Errno::ENOENT {
                        return true; // treat access failure as unsafe
                    }
                }
            }
        }

        false
    }

    /// Secure file descriptor validation to prevent TOCTOU attacks,
    fn validate_fd_path_security(&self, expected_path: &Path) -> bool {
        let proc_fd = format!("/proc/self/fd/{}", self.file.as_raw_fd());

        let fd_path = match readlink(proc_fd.as_str()) {
            Ok(p) => p,
            Err(e) => {
                warn!("Failed to read fd path: {e}");
                return false;
            }
        };

        let expected_canon = match std::fs::canonicalize(expected_path) {
            Ok(p) => p,
            Err(e) => {
                warn!(
                    "Failed to canonicalize expected path {}: {e}",
                    expected_path.display()
                );
                return false;
            }
        };

        if fd_path != expected_canon {
            warn!("File descriptor path mismatch: expected {}, got {}",
                expected_canon.display(),
                fd_path.display()
            );
            return false;
        }

        let fd_stat = match fstat(&self.file) {
            Ok(s) => s,
            Err(e) => {
                warn!("Failed to stat file descriptor: {e}");
                return false;
            }
        };

        let path_stat = match stat(&expected_canon) {
            Ok(s) => s,
            Err(e) => {
                warn!("Failed to stat expected path {:?}: {e}", expected_canon);
                return false;
            }
        };

        if fd_stat.st_dev != path_stat.st_dev || fd_stat.st_ino != path_stat.st_ino {
            warn!("File descriptor and path point to different files");
            return false;
        }

        true
    }

    /// Secure parent directory validation to prevent TOCTOU.
    /// Returns an OwnedFd to the parent directory on success.
    fn secure_validate_log_path(&self, path: &Path) -> ConmonResult<()> {
        let bytes = path.as_os_str().as_bytes();
        if bytes.is_empty() {
            return Err(ConmonError::new("Empty log path", 1));
        }
        if bytes.len() >= libc::PATH_MAX as usize {
            return Err(ConmonError::new("Log path too long", 1));
        }

        // Mirror the C substring checks.
        let s = path.to_string_lossy();
        if s.contains("../") || s.contains("/..") {
            return Err(ConmonError::new("Log path contains traversal patterns", 1));
        }

        let parent_dir = path
            .parent()
            .ok_or_else(|| ConmonError::new("Log path has no parent directory", 1))?;
        let base = path
            .file_name()
            .ok_or_else(|| ConmonError::new("Log path has no basename", 1))?;

        let parent_c = CString::new(parent_dir.as_os_str().as_bytes().to_vec())
            .map_err(|_| ConmonError::new("Parent directory contains NUL byte", 1))?;
        let base_c = CString::new(base.as_bytes().to_vec())
            .map_err(|_| ConmonError::new("Basename contains NUL byte", 1))?;

        let parent_fd = open(
            parent_c.as_c_str(),
            OFlag::O_PATH | OFlag::O_CLOEXEC,
            Mode::empty(),
        )
        .map_err(|e| ConmonError::new(format!("Failed to open parent dir: {e}"), 1))?;

        match fstatat(
            &parent_fd,
            base_c.as_c_str(),
            AtFlags::AT_SYMLINK_NOFOLLOW,
        ) {
            Ok(st) => {
                let kind = SFlag::from_bits_truncate(st.st_mode);
                if kind.contains(SFlag::S_IFLNK) {
                    return Err(ConmonError::new("Log path is a symbolic link", 1));
                }
                Ok(())
            }
            Err(e) if e == Errno::ENOENT => {
                // Parent directory checks
                let pst = fstat(&parent_fd)
                    .map_err(|e2| ConmonError::new(format!("Failed to stat parent directory: {e2}"), 1))?;

                // Not world-writable
                if (pst.st_mode & (libc::S_IWOTH as u32)) != 0 {
                    return Err(ConmonError::new(
                        format!("Parent directory is world-writable: {}", parent_dir.display()),
                        1,
                    ));
                }

                // Ownership check: accept root, real uid, effective uid
                let uid = getuid().as_raw();
                let euid = geteuid().as_raw();
                let owner = pst.st_uid;

                if owner != 0 && owner != uid && owner != euid {
                    return Err(ConmonError::new(
                        format!(
                            "Parent directory owned by unexpected UID {}: {}",
                            owner,
                            parent_dir.display()
                        ),
                        1,
                    ));
                }

                let canon_parent = Self::canonicalize(parent_dir)?;

                if self.allowlist_dirs.is_some() && !self.is_path_in_allowlist(&canon_parent) {
                    return Err(ConmonError::new("Parent directory not in allowlist", 1));
                }

                if self.path_contains_symlinks_atomic(Some(&canon_parent)) {
                    return Err(ConmonError::new("Parent path contains symlinks", 1));
                }

                Ok(())
            }
            Err(e) => Err(ConmonError::new(
                format!("Unsafe error during path validation: {e}"),
                1,
            )),
        }
    }

    fn shift_backup_files(&self) -> ConmonResult<()> {
        // Bounds checking
        if self.max_files <= 0 {
            return Err(ConmonError::new(
                format!("Invalid log_max_files value: {}", self.max_files),
                1,
            ));
        }

        // Validate path using secure validation
        let _validation_fd = self.secure_validate_log_path(&self.path)?;

        // Shift: .N-1 -> .N, ...
        let loop_start = if self.max_files > 1 { self.max_files } else { 2 };
        let mut had_errors = false;

        for i in (2..=loop_start).rev() {
            let from = PathBuf::from(format!("{}.{}", self.path.display(), i - 1));
            let to = PathBuf::from(format!("{}.{}", self.path.display(), i));

            match std::fs::rename(&from, &to) {
                Ok(_) => {}
                Err(e) => {
                    // Ignore ENOENT
                    if e.kind() != std::io::ErrorKind::NotFound {
                        warn!("{}", format!(
                            "Failed to shift backup file {} to {}: {e}",
                            from.display(),
                            to.display()
                        ));
                        had_errors = true;
                    }
                }
            }
        }

        if had_errors {
            warn!("Backup file shifting completed with some errors");
        }

        Ok(())
    }

    fn lock_fd_write(&self, fd: RawFd) -> bool {
        let mut lock = libc::flock {
            l_type: libc::F_WRLCK as i16,
            l_whence: libc::SEEK_SET as i16,
            l_start: 0,
            l_len: 0,
            l_pid: 0,
        };
        unsafe { libc::fcntl(fd, libc::F_SETLK, &mut lock) != -1 }
    }

    fn unlock_fd(&self, fd: RawFd) {
        let mut unlock = libc::flock {
            l_type: libc::F_UNLCK as i16,
            l_whence: libc::SEEK_SET as i16,
            l_start: 0,
            l_len: 0,
            l_pid: 0,
        };
        unsafe {
            let _ = libc::fcntl(fd, libc::F_SETLK, &mut unlock);
        }
    }

    fn setup_rotation_files(&self) -> ConmonResult<(File, PathBuf, PathBuf)> {
        let temp_path = PathBuf::from(format!("{}.new", self.path.display()));
        let backup_path = PathBuf::from(format!("{}.1", self.path.display()));

        let new_fd = OpenOptions::new()
                    .create(true)
                    .write(true)
                    .create_new(true)
                    .mode(0o640)
                    .open(&temp_path)
                    .map_err(|e| {
                        ConmonError::new(
                            format!("Failed to create new file {:?}: {}", temp_path, e),
                            1,
                        )
                    })?;
        Ok((new_fd, temp_path, backup_path))
    }

    fn perform_file_rotation(&self, temp_path: &Path, backup_path: &Path) -> ConmonResult<()> {
        // Rename current log to .1
        std::fs::rename(&self.path, backup_path).map_err(|e| {
            ConmonError::new(
                format!("Failed to rotate log file {}: {e}", self.path.display()),
                1,
            )
        })?;

        // Move new file into place
        if let Err(e) = std::fs::rename(temp_path, &self.path) {
            warn!("{}", format!("Failed to move new log file into place: {e}"));

            // Try to restore original file
            if let Err(e2) = std::fs::rename(backup_path, &self.path) {
                warn!("{}", format!("CRITICAL: Failed to restore original log file: {e2}"));
                warn!("Original log data may be in backup file");
            }

            return Err(ConmonError::new("Rotation failed", 1));
        }

        Ok(())
    }

    fn cleanup_temp_file(&self, new_fd: Option<File>, temp_path: Option<&Path>) {
        drop(new_fd); // closes if present

        if let Some(p) = temp_path {
            if !p.as_os_str().is_empty() {
                if let Err(e) = std::fs::remove_file(p) {
                    if e.kind() != std::io::ErrorKind::NotFound {
                        warn!("{}", format!("Failed to remove temporary file {}: {e}", p.display()));
                    }
                }
            }
        }
    }

    /// Rotates and replaces `self.file` with a new file handle.
    fn rotate(&mut self) -> ConmonResult<()> {
        if self.opt_rotate {
            // Validate log path before rotating.
            self.secure_validate_log_path(&self.path)?;

            // Lock old fd.
            if self.file.as_raw_fd() < 0 {
                return Err(ConmonError::new("Cannot rotate: invalid file descriptor", 1));
            }
            if !self.lock_fd_write(self.file.as_raw_fd()) {
                // Locked by other process => skip rotation.
                return Ok(());
            }

            // Validate fd still points at expected path/device/inode.
            if !self.validate_fd_path_security(&self.path) {
                self.unlock_fd(self.file.as_raw_fd());
                return Err(ConmonError::new("File descriptor security validation failed", 1));
            }

            // Create new temporary log file with restrictive permissions.
            let (new_fd, temp_path, backup_path) = match self.setup_rotation_files() {
                Ok(v) => v,
                Err(e) => {
                    self.unlock_fd(self.file.as_raw_fd());
                    return Err(e);
                }
            };

            // Shift backups and rotate.
            if let Err(e) = self.shift_backup_files().and_then(|_| self.perform_file_rotation(&temp_path, &backup_path)) {
                self.cleanup_temp_file(Some(new_fd), Some(&temp_path));
                self.unlock_fd(self.file.as_raw_fd());
                return Err(e);
            }

            // Unlock and close the old file; drop the old File afterwards.
            self.unlock_fd(self.file.as_raw_fd());

            // Replace file handle
            self.file = new_fd;
            self.bytes_written = 0;
        } else {
            // Reopen without rotation: truncate the existing log atomically.
            let temp_path = PathBuf::from(format!("{}.new", self.path.display()));
            let new_fd = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .mode(0o640)
                .open(&temp_path)
                .map_err(|e| {
                    ConmonError::new(
                        format!("Failed to create new log file {:?}: {}", temp_path, e),
                        1,
                    )
                })?;

            if let Err(e) = std::fs::rename(&temp_path, &self.path) {
                warn!("{}", format!("Failed to move new log file into place: {e}"));
                if let Err(e2) = std::fs::remove_file(&temp_path) {
                    if e2.kind() != std::io::ErrorKind::NotFound {
                        warn!(
                            "{}",
                            format!(
                                "Failed to remove temporary log file {}: {e2}",
                                temp_path.display()
                            )
                        );
                    }
                }

                return Err(ConmonError::new("Reopen failed", 1));
            }

            self.file = new_fd;
            self.bytes_written = 0;
        }

        Ok(())
    }

    /// Rotates a log when configured so and next record would push us over `self.max_size`.
    fn rotate_if_needed(&mut self, bytes_to_be_written: u64) -> ConmonResult<()> {
        if self.max_size > 0 && self.bytes_written.saturating_add(bytes_to_be_written) >= self.max_size {
            self.rotate()?;
        }
        Ok(())
    }
}

impl Drop for FileLogger {
    fn drop(&mut self) {
        if !self.no_sync {
            let _ = self.file.sync_all();
        }
    }
}

impl LogPlugin for FileLogger {
    fn reopen(&mut self) -> ConmonResult<()> {
        self.rotate()
    }

    fn write(&mut self, is_stdout: bool, data: &[u8]) -> ConmonResult<()> {
        // Track if we previously wrote a partial line for each stream.
        let has_partial = if is_stdout {
            self.stdout_has_partial
        } else {
            self.stderr_has_partial
        };

        let pipename = if is_stdout { "stdout" } else { "stderr" };

        let mut buf = data;
        let mut buflen = data.len() as isize;

        // Helper to map I/O errors into ConmonError.
        let map_err = |e: std::io::Error, msg: &str| ConmonError::new(format!("{msg}: {e}"), 1);

        // If we get an empty buffer and we had a partial line before, emit terminating "F\n".
        if buflen == 0 && has_partial {
            let mut tsbuf = [0u8; TSBUFLEN];
            Self::set_k8s_timestamp(&mut tsbuf, pipename);
            let ts_len = tsbuf.iter().position(|&b| b == 0).unwrap_or(tsbuf.len());

            // bytes: timestamp + "F\n"
            let bytes_to_be_written = ts_len as u64 + 2;
            if self.global_max_size > 0
                && self.total_bytes_written.saturating_add(bytes_to_be_written) >= self.global_max_size
            {
                return Ok(());
            }
            self.rotate_if_needed(bytes_to_be_written)?;

            self.file
                .write_all(&tsbuf[..ts_len])
                .map_err(|e| map_err(e, "failed to write timestamp"))?;
            self.file
                .write_all(b"F\n")
                .map_err(|e| map_err(e, "failed to write terminating F-sequence"))?;
            self.file.flush().map_err(|e| map_err(e, "failed to flush log file"))?;

            self.bytes_written = self.bytes_written.saturating_add(bytes_to_be_written);
            self.total_bytes_written = self.total_bytes_written.saturating_add(bytes_to_be_written);

            if is_stdout {
                self.stdout_has_partial = false;
            } else {
                self.stderr_has_partial = false;
            };
            return Ok(());
        }

        while buflen > 0 {
            let mut line_len: isize = 0;
            let partial = Self::get_line_len(&mut line_len, buf, buflen);

            let mut tsbuf = [0u8; TSBUFLEN];
            Self::set_k8s_timestamp(&mut tsbuf, pipename);
            let ts_len = tsbuf.iter().position(|&b| b == 0).unwrap_or(tsbuf.len());

            // timestamp + ("P " or "F ") + line + maybe extra "\n"
            let mut bytes_to_be_written: u64 = ts_len as u64 + 2 + (line_len as u64);
            if partial {
                bytes_to_be_written = bytes_to_be_written.saturating_add(1);
            }

            // Enforce global max before writing.
            if self.global_max_size > 0
                && self.total_bytes_written.saturating_add(bytes_to_be_written) >= self.global_max_size
            {
                break;
            }

            // Rotate if needed before writing this record.
            self.rotate_if_needed(bytes_to_be_written)?;

            // timestamp + stream
            self.file
                .write_all(&tsbuf[..ts_len])
                .map_err(|e| map_err(e, "failed to write timestamp"))?;

            // partial ("P ") vs full ("F ") marker
            if partial {
                self.file
                    .write_all(b"P ")
                    .map_err(|e| map_err(e, "failed to write partial log tag"))?;
            } else {
                self.file
                    .write_all(b"F ")
                    .map_err(|e| map_err(e, "failed to write end log tag"))?;
            }

            // actual log bytes
            let line_slice_len = line_len as usize;
            self.file
                .write_all(&buf[..line_slice_len])
                .map_err(|e| map_err(e, "failed to write log line"))?;

            // If there was no newline in this chunk, add one
            if partial {
                self.file
                    .write_all(b"\n")
                    .map_err(|e| map_err(e, "failed to write newline for partial log"))?;
            }

            self.bytes_written = self.bytes_written.saturating_add(bytes_to_be_written);
            self.total_bytes_written = self.total_bytes_written.saturating_add(bytes_to_be_written);

            if is_stdout {
                self.stdout_has_partial = partial;
            } else {
                self.stderr_has_partial = partial;
            };

            // Advance buffer
            buf = &buf[line_slice_len..];
            buflen -= line_len;
        }

        self.file.flush().map_err(|e| map_err(e, "failed to flush log file"))?;
        Ok(())
    }
}
