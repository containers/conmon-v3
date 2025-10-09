use crate::error::ConmonError;
use crate::error::ConmonResult;
use libc::EINTR;
use libc::c_char;
use libc::c_int;
use libc::{SIG_BLOCK, SIGHUP, SIGINT, SIGKILL, SIGQUIT, SIGTERM, sigset_t};
use std::ffi::CString;
use std::io::Error;
use std::mem::zeroed;
use std::ptr::{self};

// Helper function to block the signals before fork.
// Returns the old signal mask which can be used to unblock the signals.
fn block_signals() -> ConmonResult<sigset_t> {
    unsafe {
        let mut mask: sigset_t = zeroed();
        let mut oldmask: sigset_t = zeroed();

        if libc::sigemptyset(&mut mask) < 0
            || libc::sigaddset(&mut mask, SIGTERM) < 0
            || libc::sigaddset(&mut mask, SIGQUIT) < 0
            || libc::sigaddset(&mut mask, SIGINT) < 0
            || libc::sigaddset(&mut mask, SIGHUP) < 0
            || libc::sigprocmask(SIG_BLOCK, &mask, &mut oldmask) < 0
        {
            return Err(ConmonError::new(
                format!("Failed to block signals: {}", Error::last_os_error()),
                1,
            ));
        }

        Ok(oldmask)
    }
}

// Helper function to execv command defined by `args`.
fn execv_from_vec(args: &[String]) -> ConmonResult<()> {
    unsafe {
        if args.is_empty() {
            return Err(ConmonError::new(
                "Failed to execute runtime binary: empty args",
                1,
            ));
        }

        // Convert the program path (first arg).
        let path = CString::new(args[0].as_str()).expect("CString::new failed");

        // Convert args into CString pointers.
        let cstr_args: Vec<CString> = args
            .iter()
            .map(|s| CString::new(s.as_str()).expect("CString::new failed"))
            .collect();

        // Make a Vec<*mut c_char> for argv[].
        let mut argv: Vec<*const c_char> = cstr_args
            .iter()
            .map(|s| s.as_ptr() as *const c_char)
            .collect();

        // argv must be null-terminated.
        argv.push(ptr::null_mut());

        // Call execv.
        libc::execv(path.as_ptr(), argv.as_ptr());

        // If execv returns, there was an error.
        Err(ConmonError::new(
            format!("Failed to execv: {}", Error::last_os_error()),
            1,
        ))
    }
}

// Run the runtime binary defined by `args`.
// This function double-forks and returns children PID of the second `fork()`.
pub fn run_runtime(args: &[String]) -> ConmonResult<i32> {
    unsafe {
        if args.is_empty() {
            return Err(ConmonError::new(
                "Failed to execute runtime binary: empty args",
                1,
            ));
        }

        // First fork to ensure process is not a process group leader.
        use libc::SIG_SETMASK;
        let pid = libc::fork();
        if pid < 0 {
            return Err(ConmonError::new(
                format!("Failed to fork: {}", Error::last_os_error()),
                1,
            ));
        }
        if pid > 0 {
            libc::_exit(0);
        }

        // Create a new session (detach from controlling terminal).
        if libc::setsid() < 0 {
            return Err(ConmonError::new(
                format!("Failed to setsid: {}", Error::last_os_error()),
                1,
            ));
        }

        // Block the signals.
        let oldmask = block_signals()?;

        // Second fork.
        let pid2 = libc::fork();
        if pid2 < 0 {
            return Err(ConmonError::new(
                format!("Failed to double-fork: {}", Error::last_os_error()),
                1,
            ));
        }
        if pid2 > 0 {
            return Ok(pid2);
        }

        // Daemon process starts here.

        // Unblock signals.
        if libc::sigprocmask(SIG_SETMASK, &oldmask, ptr::null_mut()) < 0 {
            return Err(ConmonError::new(
                format!("Failed to unblock signals: {}", Error::last_os_error()),
                1,
            ));
        }

        // Set conservative umask.
        libc::umask(0o022);

        // Close all open file descriptors.
        let mut lim: libc::rlimit = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        let max_fd = if libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) == 0 {
            // rlim_cur may be "infinite" (RLIM_INFINITY); clamp to something reasonable.
            if lim.rlim_cur == libc::RLIM_INFINITY {
                65535
            } else {
                lim.rlim_cur as libc::c_ulong
            }
        } else {
            1024
        } as libc::c_int;

        let mut fd = 0;
        while fd < max_fd {
            libc::close(fd);
            fd += 1;
        }

        // Reopen stdin/stdout/stderr to /dev/null.
        let devnull = CString::new("/dev/null")?;
        let null_fd = libc::open(devnull.as_ptr(), libc::O_RDWR, 0);
        if null_fd < 0 {
            return Err(ConmonError::new(
                format!("Failed to open /dev/null: {}", Error::last_os_error()),
                1,
            ));
        }

        // Ensure fds 0,1,2 are valid and point to /dev/null
        // If open returned > 0, dup it down to 0/1/2 then close the extra.
        for target in [0, 1, 2] {
            if libc::dup2(null_fd, target) < 0 {
                let e = Error::last_os_error();
                libc::close(null_fd);
                return Err(ConmonError::new(
                    format!("Failed to dup2 /dev/null: {}", e),
                    1,
                ));
            }
        }
        if null_fd > 2 {
            libc::close(null_fd);
        }

        execv_from_vec(args)?;
        // If execv returns, there was an error.
        Err(ConmonError::new(
            format!("Failed to execv: {}", Error::last_os_error()),
            1,
        ))
    }
}

// Block until the runtime process defined by `runtime_pid` exits.
// Returns the runtime exit code.
pub fn wait_for_runtime(runtime_pid: i32) -> ConmonResult<i32> {
    let mut runtime_status: c_int = 0;

    loop {
        let ret = unsafe { libc::waitpid(runtime_pid, &mut runtime_status, 0) };
        if ret >= 0 {
            if libc::WIFEXITED(runtime_status) {
                // Success: child state is now in runtime_status.
                return Ok(libc::WEXITSTATUS(runtime_status));
            } else {
                return Err(ConmonError::new(
                    format!("Runtime process exited abnormally: {runtime_status}"),
                    1,
                ));
            }
        }

        // Error path: inspect errno
        let err = Error::last_os_error();
        // clippy tries to collapse this if, but this works only for Rust >= 1.70.
        #[allow(clippy::collapsible_if)]
        if let Some(code) = err.raw_os_error() {
            if code == EINTR {
                // Interrupted by a signal: retry the wait.
                continue;
            }
        }

        // Try to kill the child.
        if runtime_pid > 0 {
            unsafe {
                libc::kill(runtime_pid, SIGKILL);
            }
        }

        return Err(ConmonError::new(
            format!("Failed to wait for runtime process to exit: {err}"),
            1,
        ));
    }
}
