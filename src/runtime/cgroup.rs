// src/cgroup.rs

use log::{debug, info, warn};
use nix::errno::Errno;
use nix::fcntl::{OFlag, open};
use nix::libc;
use nix::sys::stat::Mode;
use nix::sys::statfs;
use nix::unistd::close;
use std::ffi::CString;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, ErrorKind};
use std::os::fd::{FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};

use crate::error::{ConmonError, ConmonResult};
use crate::unix_socket::{RemoteSocket, SocketType};

const CGROUP_ROOT: &str = "/sys/fs/cgroup";

/// Sets up OOM (out-of-memory) handling for `pid` .
///
/// # Arguments
///
/// * `pid` - The pid to setup OOM for.
/// * `persist_dir` - Optional path into which the OOM marker files will be created.
/// * `bundle` - Path into which the OOM marker file is created.
///
/// # Returns
///
/// * RemoteSocket to pass to event-loop.
pub fn setup_oom_handling(
    pid: i32,
    persist_dir: &Option<PathBuf>,
    bundle: &Path,
) -> ConmonResult<Option<RemoteSocket>> {
    info!("Setting up OOM handler.");
    unsafe {
        let stat = statfs::statfs("/sys/fs/cgroup")?;
        if stat.filesystem_type() == statfs::CGROUP2_SUPER_MAGIC {
            // Not being able to detect OOM is not a reason to fail the container.
            return match setup_oom_handling_cgroup_v2(pid, persist_dir, bundle) {
                Ok(s) => Ok(s),
                Err(e) => {
                    warn!("Failed to set up OOM handling: {}", e.msg);
                    Ok(None)
                }
            };
        }

        Err(ConmonError::new("Cgroups v1 is not supported.", 1))
    }
}

/// Inspects /proc/[pid]/cgroup and returns the absolute
/// filesystem path to the cgroup directory for a given `pid`.
///
/// # Arguments
///
/// * `pid` - The pid of process to inspect.
///
/// # Returns
///
/// * Absolut path to cgroup directory.
fn process_cgroup_subsystem_path(pid: i32) -> ConmonResult<PathBuf> {
    // Read the /proc/`pid`/cgroup file.
    let cgroups_file_path = format!("/proc/{pid}/cgroup");
    let content = match fs::read_to_string(&cgroups_file_path) {
        Ok(c) => c,
        Err(e) if e.kind() == ErrorKind::NotFound => {
            return Ok(PathBuf::from(""));
        }
        Err(e) => {
            return Err(ConmonError::new(
                format!("Failed to read cgroups file {}: {}", cgroups_file_path, e),
                1,
            ));
        }
    };

    let Some(path) = cgroup2_path_from(&content) else {
        warn!("No cgroup v2 entry found in {cgroups_file_path}");
        return Ok(PathBuf::new());
    };
    // The path is absolute inside the cgroup root, e.g. "/user.slice/...".
    Ok(Path::new(CGROUP_ROOT).join(path.trim_start_matches('/')))
}

/// Returns the cgroup v2 path from the contents of a /proc/[pid]/cgroup file,
/// or `None` if there is no cgroup v2 entry in it.
fn cgroup2_path_from(content: &str) -> Option<&str> {
    // The cgroup v2 entry is "0::/path". Any other entries are for cgroup v1
    // hierarchies (e.g. "1:net_cls:/"), which may be mounted alongside v2.
    content.lines().find_map(|l| l.strip_prefix("0::"))
}

/// Sets up OOM (out-of-memory) handling for `pid` .
///
/// # Arguments
///
/// * `pid` - The pid to setup OOM for.
/// * `persist_dir` - Optional path into which the OOM marker files will be created.
/// * `bundle` - Path into which the OOM marker file is created.
///
/// # Returns
///
/// * RemoteSocket to pass to event-loop.
unsafe fn setup_oom_handling_cgroup_v2(
    pid: i32,
    persist_dir: &Option<PathBuf>,
    bundle: &Path,
) -> ConmonResult<Option<RemoteSocket>> {
    // Get the cgroup path.
    let cgroup2_path = process_cgroup_subsystem_path(pid)?;
    if cgroup2_path.as_os_str().is_empty() {
        return Ok(None);
    }

    // Setup inotify watch for memory.events.
    let memory_events_file_path = cgroup2_path.join("memory.events");
    let ifd = unsafe { libc::inotify_init1(libc::IN_CLOEXEC) };
    if ifd < 0 {
        return Err(ConmonError::new(
            format!("Failed to create inotify fd: {}", Errno::last()),
            1,
        ));
    }

    let ifd_owned = unsafe { OwnedFd::from_raw_fd(ifd) };
    let cstr = CString::new(memory_events_file_path.to_string_lossy().as_bytes()).unwrap();
    if unsafe { libc::inotify_add_watch(ifd, cstr.as_ptr(), libc::IN_MODIFY) } < 0 {
        let err = Errno::last();
        let path = memory_events_file_path.display();
        if err == Errno::ENOENT {
            // E.g. the memory controller is not enabled for the cgroup.
            debug!("{path} does not exist, skipping OOM monitoring");
            return Ok(None);
        }
        return Err(ConmonError::new(
            format!("Failed to add inotify watch for {path}: {err}"),
            1,
        ));
    }

    // Create new RemoteSocket and set its handler to `check_cgroup2_oom`.
    let persist_dir_clone = persist_dir.clone();
    let bundle = PathBuf::from(bundle);
    info!(
        "OOM inotify watch added for {}",
        memory_events_file_path.to_string_lossy()
    );
    let mut socket = RemoteSocket::new(SocketType::Inotify, ifd_owned);
    socket.set_handler(move |_data| {
        check_cgroup2_oom(&cgroup2_path, &persist_dir_clone, &bundle);
        true
    });

    Ok(Some(socket))
}

/// Handles the memory.events file change.
///
/// This function is called by the event-loop when inotify watch
/// created by `setup_oom_handling_cgroup_v2` triggers.
///
/// # Arguments
///
/// * `cgroup2_path` - The cgroup path for pid we monitor.
/// * `persist_dir` - Optional path into which the OOM marker files will be created.
/// * `bundle` - Path into which the OOM marker file is created.
///
/// # Returns
///
/// * RemoteSocket to pass to event-loop.
pub fn check_cgroup2_oom(
    cgroup2_path: &Path,
    persist_dir: &Option<PathBuf>,
    bundle: &Path,
) -> bool {
    static mut LAST_OOM_COUNTER: i64 = 0;
    static mut LAST_OOM_KILL_COUNTER: i64 = 0;

    unsafe {
        // Open the memory.events file.
        let base = cgroup2_path;
        let memory_events_file_path = Path::new(&base).join("memory.events");
        let file = match File::open(&memory_events_file_path) {
            Ok(f) => f,
            Err(err) => {
                warn!(
                    "Failed to open cgroups file: {}",
                    memory_events_file_path.to_string_lossy()
                );
                if err.raw_os_error() == Some(libc::ENOENT) {
                    debug!(
                        "Cgroup appears to have been removed, stopping OOM monitoring: {}",
                        memory_events_file_path.to_string_lossy(),
                    );
                    return false;
                }
                return true;
            }
        };

        // Read the memory.events file to detect the OOM.
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
                _ => 0,
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

/// Creates the OOM marker files.
///
/// # Arguments
///
/// * `persist_dir` - Optional path into which the OOM marker files will be created.
///   if not provided, the `bundle` path is used.
/// * `bundle` - Fallback path when `persist_dir` is not provided.
fn create_oom_files(persist_dir: &Option<PathBuf>, bundle: &Path) -> Result<(), ()> {
    info!("OOM received");
    let mut r = 0;

    if let Some(p) = persist_dir {
        if create_oom_file(p).is_err() {
            r |= 1;
        }
    }

    if create_oom_file(bundle).is_err() {
        r |= 1;
    }

    if r == 0 { Ok(()) } else { Err(()) }
}

/// Creates the OOM marker file.
///
/// # Arguments
///
/// * `base_path` - The path in which the OOM marker file is created.
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
                base_path.to_string_lossy(),
            );
            return Err(());
        }
    };

    let _ = close(fd);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cgroup2_path_from_v2_only() {
        assert_eq!(
            cgroup2_path_from("0::/user.slice/ctr\n"),
            Some("/user.slice/ctr")
        );
        assert_eq!(cgroup2_path_from("0::/\n"), Some("/"));
    }

    #[test]
    fn cgroup2_path_from_skips_v1_entries() {
        let content = "2:name=zdtmtst:/\n1:net_cls:/\n0::/system.slice/ctr\n";
        assert_eq!(cgroup2_path_from(content), Some("/system.slice/ctr"));
    }

    #[test]
    fn cgroup2_path_from_no_v2_entry() {
        assert_eq!(cgroup2_path_from(""), None);
        assert_eq!(cgroup2_path_from("1:net_cls:/\n1:name=systemd:/x\n"), None);
    }

    #[test]
    fn setup_oom_handling_skips_missing_memory_events() -> ConmonResult<()> {
        // kthreadd (pid 2) is in the root cgroup, which has no memory.events.
        // Skip if this is not the case (e.g. in a pid namespace).
        match std::fs::read_to_string("/proc/2/cgroup") {
            Ok(s) if s == "0::/\n" => {}
            _ => return Ok(()),
        }
        if Path::new("/sys/fs/cgroup/memory.events").exists() {
            return Ok(());
        }
        let dir = tempfile::tempdir()?;
        let socket = unsafe { setup_oom_handling_cgroup_v2(2, &None, dir.path())? };
        assert!(socket.is_none());
        Ok(())
    }
}
