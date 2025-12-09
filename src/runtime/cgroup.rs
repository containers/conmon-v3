// src/cgroup.rs

use log::{debug, info, warn};
use nix::fcntl::{open, OFlag};
use nix::sys::stat::Mode;
use nix::unistd::close;
use std::ffi::CString;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::os::fd::{FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use nix::errno::Errno;
use nix::libc;
use nix::sys::statfs;

use crate::error::{ConmonError, ConmonResult};
use crate::unix_socket::{RemoteSocket, SocketType};

const CGROUP_ROOT: &str = "/sys/fs/cgroup";

/// Sets up OOM (out-of-memory) handling for `pid` .
pub fn setup_oom_handling(pid: i32, persist_dir:&Option<PathBuf>, bundle: &PathBuf) -> ConmonResult<RemoteSocket> {
    info!("Setting up OOM handler.");
    unsafe {
        let stat = statfs::statfs("/sys/fs/cgroup")?;
        if stat.filesystem_type() == statfs::CGROUP2_SUPER_MAGIC {
            let s = setup_oom_handling_cgroup_v2(pid, persist_dir, bundle)?;
            return Ok(s);
        }

        return Err(ConmonError::new(
            format!("Cgroups v1 is not supported."),
            1
        ));
    }
}

/// Helper function that inspects /proc/[pid]/cgroup and returns the absolute
/// filesystem path to the cgroup directory for a given `pid` and `subsystem`.
fn process_cgroup_subsystem_path (
    pid: i32,
    cgroup2: bool,
    subsystem: &str,
) -> ConmonResult<PathBuf> {
    // Open the /proc/`pid`/cgroup file.
    let cgroups_file_path = format!("/proc/{pid}/cgroup");
    let file = match File::open(&cgroups_file_path) {
        Ok(f) => f,
        Err(e) => {
            return Err(ConmonError::new(
                format!("Failed to open cgroups file {}: {}", cgroups_file_path, e),
                1
            ));
        }
    };

    // Parse the cgroup file.
    let reader = BufReader::new(file);
    for line_result in reader.lines() {
        let line = match line_result {
            Ok(l) => l,
            Err(_) => continue,
        };

        // Format: hierarchy-ID:controllers:path
        let (after_first_colon, path_part) = match line.split_once(':') {
            Some((_, rest)) => match rest.split_once(':') {
                Some((controllers, path)) => (controllers, path),
                None => {
                    return Err(ConmonError::new(
                        format!("Error parsing cgroup, second ':' not found: {}", line),
                        1
                    ));
                }
            },
            None => {
                return Err(ConmonError::new(
                    format!("Error parsing cgroup, ':' not found: {}", line),
                    1
                ));
            }
        };

        let mut path = path_part.trim_end_matches('\n');

        if cgroup2 {
            // v2: path is directly under CGROUP_ROOT
            let mut full = PathBuf::from(CGROUP_ROOT);
            // path from /proc is absolute inside cgroup root, e.g. "/user.slice/..."
            if path.starts_with('/') {
                path = &path[1..];
            }
            full.push(path);
            return Ok(full);
        }

        // v1: controllers may be "memory", "cpu,cpuacct", "name=systemd", etc.
        for ctr in after_first_colon.split(',') {
            // "name=systemd" => "name"
            let subpath = ctr.split('=').next().unwrap_or(ctr);

            if subpath == subsystem {
                let mut full = PathBuf::from(CGROUP_ROOT);
                full.push(subpath);
                // path already starts with '/', so join as string
                if path.starts_with('/') {
                    path = &path[1..];
                }
                full.push(path);
                return Ok(full);
            }
        }
    }

    return Err(ConmonError::new(
        format!("Error finding subsystem '{}' in cgroup file", subsystem),
        1
    ));
}

/// Sets up OOM handling using cgroup v2 for `pid`.
unsafe fn setup_oom_handling_cgroup_v2(pid: i32, persist_dir: &Option<PathBuf>, bundle: &PathBuf) -> ConmonResult<RemoteSocket> {
    let cgroup2_path = process_cgroup_subsystem_path(pid, true, "")?;
    let memory_events_file_path = cgroup2_path.join("memory.events");

    let ifd = unsafe { libc::inotify_init1(libc::IN_CLOEXEC) };
    if ifd < 0 {
        return Err(ConmonError::new(
            format!("Failed to create inotify fd: {}", Errno::last()),
            1
        ));
    }

    let ifd_owned = unsafe { OwnedFd::from_raw_fd(ifd) };

    let cstr = CString::new(memory_events_file_path.to_string_lossy().as_bytes()).unwrap();
    if unsafe { libc::inotify_add_watch(ifd, cstr.as_ptr(), libc::IN_MODIFY) } < 0 {
        return Err(ConmonError::new(
            format!("Failed to add inotify watch for: {}", memory_events_file_path.to_string_lossy()),
            1
        ));
    }

    let persist_dir_clone = persist_dir.clone();
    let bundle = bundle.clone();
    info!("OOM inotify watch added for {}", memory_events_file_path.to_string_lossy());
    let mut socket = RemoteSocket::new(SocketType::Inotify, ifd_owned);
    socket.set_handler(move |_data| {
        check_cgroup2_oom(&cgroup2_path, &persist_dir_clone, &bundle);
        return true;
    });

    Ok(socket)
}

pub fn check_cgroup2_oom(cgroup2_path: &PathBuf, persist_dir: &Option<PathBuf>, bundle: &PathBuf) -> bool {
    static mut LAST_OOM_COUNTER: i64 = 0;
    static mut LAST_OOM_KILL_COUNTER: i64 = 0;

    unsafe {
        let base = cgroup2_path.clone();

        let memory_events_file_path = Path::new(&base).join("memory.events");

        let file = match File::open(&memory_events_file_path) {
            Ok(f) => f,
            Err(err) => {
                warn!(
                    "Failed to open cgroups file: {}", memory_events_file_path.to_string_lossy()
                );
                if err.raw_os_error() == Some(libc::ENOENT) {
                    debug!(
                        "Cgroup appears to have been removed, stopping OOM monitoring: {}",
                        &memory_events_file_path.to_string_lossy(),
                    );
                    return false;
                }
                return true;
            }
        };

        let reader = BufReader::new(file);
        let mut oom_detected = false;

        for line_result in reader.lines() {
            let line = match line_result {
                Ok(l) => l,
                Err(_) => continue,
            };

            let line_bytes = line.as_bytes();

            let (is_oom_kill, prefix_len) = if line_bytes.starts_with(b"oom_kill ") {
                (true, "oom_kill ".len())
            } else if line_bytes.starts_with(b"oom ") {
                (false, "oom ".len())
            } else {
                continue;
            };

            let counter_str = &line[prefix_len..].trim();
            let counter: i64 = match counter_str.parse() {
                Ok(v) if v > 0 => v,
                Err(e) => {
                    warn!("Failed to parse '{}': {}", counter_str, e);
                    continue;
                }
                _ => {0}

            };

            if counter == 0 {
                continue;
            }

            let last_counter = if is_oom_kill {
                &raw mut LAST_OOM_KILL_COUNTER
            } else {
                &raw mut LAST_OOM_COUNTER
            };

            if counter != *last_counter {
                if create_oom_files(persist_dir, bundle).is_ok() {
                    *last_counter = counter;
                    oom_detected = true;
                }
            }
        }

        // true => keep watching, false => remove source
        oom_detected
    }
}

// // -----------------------------------------------------------------------------
// // Create OOM marker files (v1 and v2)
// // -----------------------------------------------------------------------------

fn create_oom_files(persist_dir:&Option<PathBuf>, bundle: &PathBuf) -> Result<(), ()> {
    info!("OOM received");
    let mut r = 0;

    if let Some(p) = persist_dir {
        if create_oom_file(&p).is_err() {
            r |= 1;
        }
    }

    if create_oom_file(&bundle).is_err() {
        r |= 1;
    }

    if r == 0 { Ok(()) } else { Err(()) }
}

fn create_oom_file(base_path: &Path) -> Result<(), ()> {
    if base_path.as_os_str().is_empty() {
        return Ok(());
    }

    let ctr_oom_file_path = base_path.join("oom");
    info!("Creating OOM file: {:?}", ctr_oom_file_path);

    let fd = match open(
        &ctr_oom_file_path,
        OFlag::O_CREAT | OFlag::O_CLOEXEC,
        Mode::from_bits_truncate(0o666),
    ) {
        Ok(fd) => fd,
        Err(_) => {
            warn!(
                "Failed to write oom file to the path {}",
                &base_path.to_string_lossy(),
            );
            return Err(());
        }
    };

    let _ = close(fd);
    Ok(())
}
