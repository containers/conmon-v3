use crate::error::{ConmonError, ConmonResult};
use crate::exit::set_subreaper;
use crate::runtime::stdio::read_pipe;

use log::{info, warn};
use nix::errno::Errno;
use nix::fcntl::{OFlag, open};
use nix::libc;
use nix::sys::prctl::set_pdeathsig;
use nix::sys::signal::{SigSet, SigmaskHow, Signal, kill, pthread_sigmask};
use nix::sys::stat::Mode;
use nix::sys::wait::{WaitStatus, waitpid};
use nix::unistd::{
    ForkResult, Pid, dup2_stderr, dup2_stdin, dup2_stdout, fork, getpid, getppid, setsid,
};

use std::ffi::{CStr, CString};
use std::fs;
use std::io::{Error, ErrorKind, Result as IoResult};
use std::os::fd::{AsFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
// for pre_exec
use std::process::{Command, Stdio, exit};

/// Convert a nix::Error into std::io::Error (for use inside pre_exec closure).
fn io_err(e: nix::Error) -> Error {
    Error::from_raw_os_error(e as i32)
}

/// Definitive liveness of a process as observed via `/proc`.
///
/// Callers must not treat [`ProcessStatus::Unknown`] as exit: that covers
/// permission failures, transient I/O errors, and unparseable `/proc` contents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessStatus {
    /// Process exists and is not zombie/dead.
    Running,
    /// Process is gone (`ENOENT`) or is zombie/dead (`Z`/`X`/`x`).
    Exited,
    /// Status could not be determined; keep polling / retry.
    Unknown,
}

impl ProcessStatus {
    /// Observe whether `pid` is still running via `/proc/<pid>/stat`.
    ///
    /// `kill(pid, 0)` also succeeds for zombie processes, so callers must not use
    /// that syscall alone to decide whether a container is still running.
    pub fn from_pid(pid: Pid) -> Self {
        let pid = pid.as_raw();
        if pid <= 0 {
            return Self::Unknown;
        }
        Self::from_stat_result(fs::read_to_string(format!("/proc/{pid}/stat")))
    }

    /// Parse the one-letter process state from the contents of `/proc/<pid>/stat`.
    fn state_from_proc_stat(contents: &str) -> Option<char> {
        let rparen = contents.rfind(')')?;
        contents.as_bytes().get(rparen + 2).map(|&b| b as char)
    }

    fn from_state(state: char) -> Self {
        match state {
            // Zombie or dead: treat as exited. `kill(pid, 0)` still succeeds for zombies.
            'Z' | 'X' | 'x' => Self::Exited,
            _ => Self::Running,
        }
    }

    /// Map a `/proc/<pid>/stat` read result to [`ProcessStatus`].
    ///
    /// Only `NotFound` and a definitive zombie/dead state mean [`ProcessStatus::Exited`].
    /// Any other I/O failure or parse failure is [`ProcessStatus::Unknown`].
    fn from_stat_result(result: Result<String, Error>) -> Self {
        match result {
            Ok(contents) => match Self::state_from_proc_stat(&contents) {
                Some(state) => Self::from_state(state),
                None => Self::Unknown,
            },
            Err(e) if e.kind() == ErrorKind::NotFound => Self::Exited,
            Err(_) => Self::Unknown,
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

/// Helper function to replace LISTEN_PID with the proper after-fork PID.
///
/// This runs in the runtime child, between fork and exec, so it must not use
/// [`std::env::var`] or [`std::env::set_var`]: [`Command::spawn`] holds a read lock on
/// the environment across the fork, and `set_var` taking the write lock in the
/// child would wait for it forever. Use libc's `getenv` and `setenv` instead,
/// which is safe as conmon is single-threaded. As the runtime command has no
/// environment changes of its own, it is executed with `environ`, so it sees
/// the new value.
fn update_listen_pid(replace_listen_pid: bool) {
    let name = c"LISTEN_PID";
    // SAFETY: name is NUL-terminated, and the result is used before the
    // environment is modified.
    let value = unsafe { libc::getenv(name.as_ptr()) };
    if value.is_null() {
        return;
    }
    // SAFETY: getenv returns a NUL-terminated string.
    let listenpid = unsafe { CStr::from_ptr(value) };

    // Try to parse LISTEN_PID as an integer
    let lpid = match listenpid.to_str().map(str::parse::<i32>) {
        Ok(Ok(v)) if v > 0 => v,
        _ => {
            warn!("Invalid LISTEN_PID {:?}", listenpid);
            return;
        }
    };

    let parent_pid = getppid().as_raw();

    // If we should replace it, or it matches the parent PID, set it to our PID
    if replace_listen_pid || lpid == parent_pid {
        let pidstr = CString::new(getpid().as_raw().to_string()).expect("pid has no NUL");
        // SAFETY: both strings are NUL-terminated, and conmon is single-threaded.
        unsafe { libc::setenv(name.as_ptr(), pidstr.as_ptr(), 1) };
    }
}

/// Returns whether the runtime takes the `LISTEN_FDS` descriptors.
///
/// A runtime only takes them if `LISTEN_PID` is its own pid, which is only the
/// case if [`update_listen_pid`] replaced it, i.e. if `replace_listen_pid` is
/// set, or `LISTEN_PID` is the pid of the conmon process that spawned it
/// (`conmon_pid`).
fn runtime_takes_listen_fds(
    listen_pid: Option<&str>,
    replace_listen_pid: bool,
    conmon_pid: i32,
) -> bool {
    match listen_pid.map(str::parse::<i32>) {
        Some(Ok(lpid)) if lpid > 0 => replace_listen_pid || lpid == conmon_pid,
        _ => false,
    }
}

/// Parses a count of descriptors from `value` (named `name` in errors).
fn parse_fd_count(value: &str, name: &str) -> ConmonResult<RawFd> {
    value
        .parse::<RawFd>()
        .ok()
        .filter(|n| (0..=RawFd::MAX - 3).contains(n))
        .ok_or_else(|| ConmonError::new(format!("Invalid {name} {value:.10}"), 1))
}

/// Returns the number of descriptors (starting at fd 3) the runtime gets:
/// the `--preserve-fds` value from `runtime_opts`, plus `listen_fds` if the
/// runtime takes them (see [`runtime_takes_listen_fds`]).
fn runtime_fd_count_with(
    runtime_opts: &[String],
    listen_fds: Option<&str>,
    listen_pid: Option<&str>,
    replace_listen_pid: bool,
    conmon_pid: i32,
) -> ConmonResult<RawFd> {
    let mut preserved = 0;
    let mut opts = runtime_opts.iter();
    while let Some(opt) = opts.next() {
        if opt == "--preserve-fds" {
            let value = opts
                .next()
                .ok_or_else(|| ConmonError::new(format!("Missing value for {opt}"), 1))?;
            preserved = parse_fd_count(value, "--preserve-fds")?;
        } else if let Some(value) = opt.strip_prefix("--preserve-fds=") {
            preserved = parse_fd_count(value, "--preserve-fds")?;
        }
    }

    let mut listen = 0;
    if let Some(value) = listen_fds
        && runtime_takes_listen_fds(listen_pid, replace_listen_pid, conmon_pid)
    {
        listen = parse_fd_count(value, "LISTEN_FDS")?;
    }

    Ok(preserved.saturating_add(listen))
}

/// Returns the number of descriptors (starting at fd 3) passed to the runtime
/// spawned by this process, see [`runtime_fd_count_with`].
pub fn runtime_fd_count(runtime_opts: &[String], replace_listen_pid: bool) -> ConmonResult<RawFd> {
    runtime_fd_count_with(
        runtime_opts,
        std::env::var("LISTEN_FDS").ok().as_deref(),
        std::env::var("LISTEN_PID").ok().as_deref(),
        replace_listen_pid,
        getpid().as_raw(),
    )
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
            // Match classic conmon: if this process's parent (conmon) dies,
            // kill the runtime so it is not left under PID 1.
            set_pdeathsig(Signal::SIGKILL).map_err(io_err)?;

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

        // Restore the parent's mask after spawn. Signals stay blocked only in
        // the fork/exec window (child restores via pre_exec). Session::launch
        // will block TERM/QUIT/INT again for signalfd; HUP must not stay stuck
        // blocked for the rest of the process lifetime (conmon-v2 unblocks).
        pthread_sigmask(SigmaskHow::SIG_SETMASK, Some(&oldmask), None).map_err(|e| {
            ConmonError::new(format!("Failed to restore signal mask after spawn: {e}"), 1)
        })?;

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

    /// Send `SIGKILL` to the runtime process if it is still running.
    ///
    /// Used when create fails early (for example console-socket handshake
    /// failure) while conmon is still alive. PDEATHSIG alone is not enough on
    /// that path: exit cleanup blocks in `waitpid(-1)` until children exit.
    pub fn kill(&self) -> ConmonResult<()> {
        let raw = self.pid;
        if raw <= 0 {
            return Ok(());
        }
        let pid = Pid::from_raw(raw);
        match kill(pid, Signal::SIGKILL) {
            Ok(()) | Err(Errno::ESRCH) => Ok(()),
            Err(e) => Err(ConmonError::new(
                format!("Failed to kill runtime process {raw}: {e}"),
                1,
            )),
        }
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

    fn opts(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn runtime_fd_count_parses_preserve_fds() -> ConmonResult<()> {
        assert_eq!(runtime_fd_count_with(&[], None, None, false, 1)?, 0);
        let o = opts(&["--foo", "--preserve-fds", "2", "--bar"]);
        assert_eq!(runtime_fd_count_with(&o, None, None, false, 1)?, 2);
        let o = opts(&["--preserve-fds=3"]);
        assert_eq!(runtime_fd_count_with(&o, None, None, false, 1)?, 3);
        Ok(())
    }

    #[test]
    fn runtime_fd_count_rejects_bad_preserve_fds() {
        for o in [
            opts(&["--preserve-fds"]),
            opts(&["--preserve-fds", "x"]),
            opts(&["--preserve-fds=-1"]),
            opts(&["--preserve-fds=2147483647"]),
        ] {
            let res = runtime_fd_count_with(&o, None, None, false, 1);
            assert!(res.is_err(), "{o:?}");
        }
    }

    #[test]
    fn runtime_fd_count_counts_listen_fds_taken_by_runtime() -> ConmonResult<()> {
        let o = opts(&["--preserve-fds=1"]);
        // Conmon's pid is 42.
        let count = |fds, pid, replace| runtime_fd_count_with(&o, fds, pid, replace, 42);
        // LISTEN_PID is conmon's pid: the runtime takes LISTEN_FDS.
        assert_eq!(count(Some("2"), Some("42"), false)?, 3);
        // --replace-listen-pid: the runtime takes LISTEN_FDS.
        assert_eq!(count(Some("2"), Some("1"), true)?, 3);
        // LISTEN_PID is someone else's (podman's own LISTEN_FDS, which podman
        // already counts in --preserve-fds): not taken.
        assert_eq!(count(Some("2"), Some("1"), false)?, 1);
        // No or invalid LISTEN_PID: not taken, even with --replace-listen-pid.
        assert_eq!(count(Some("2"), None, true)?, 1);
        assert_eq!(count(Some("2"), Some("x"), true)?, 1);
        // Invalid LISTEN_FDS is only an error if the runtime takes it.
        assert!(count(Some("x"), Some("42"), false).is_err());
        assert_eq!(count(Some("x"), Some("1"), false)?, 1);
        Ok(())
    }

    #[test]
    fn state_from_proc_stat_parses_zombie() {
        assert_eq!(
            ProcessStatus::state_from_proc_stat("42 (sh) Z 1 42 42"),
            Some('Z')
        );
    }

    #[test]
    fn state_from_proc_stat_parses_running() {
        assert_eq!(
            ProcessStatus::state_from_proc_stat("99 (sleep) S 1 99 99"),
            Some('S')
        );
    }

    #[test]
    fn state_from_proc_stat_rejects_malformed() {
        assert_eq!(ProcessStatus::state_from_proc_stat("no-paren Z"), None);
        assert_eq!(ProcessStatus::state_from_proc_stat("1 (x)"), None);
    }

    #[test]
    fn from_stat_result_running() {
        assert_eq!(
            ProcessStatus::from_stat_result(Ok("99 (sleep) S 1 99 99".into())),
            ProcessStatus::Running
        );
    }

    #[test]
    fn from_stat_result_zombie_is_exited() {
        assert_eq!(
            ProcessStatus::from_stat_result(Ok("42 (sh) Z 1 42 42".into())),
            ProcessStatus::Exited
        );
    }

    #[test]
    fn from_stat_result_dead_is_exited() {
        assert_eq!(
            ProcessStatus::from_stat_result(Ok("7 (x) X 1 7 7".into())),
            ProcessStatus::Exited
        );
    }

    #[test]
    fn from_stat_result_enoent_is_exited() {
        assert_eq!(
            ProcessStatus::from_stat_result(Err(Error::new(
                ErrorKind::NotFound,
                "No such file or directory"
            ))),
            ProcessStatus::Exited
        );
    }

    #[test]
    fn from_stat_result_permission_is_unknown() {
        assert_eq!(
            ProcessStatus::from_stat_result(Err(Error::new(
                ErrorKind::PermissionDenied,
                "Permission denied"
            ))),
            ProcessStatus::Unknown
        );
    }

    #[test]
    fn from_stat_result_other_io_is_unknown() {
        assert_eq!(
            ProcessStatus::from_stat_result(Err(Error::other("I/O error"))),
            ProcessStatus::Unknown
        );
    }

    #[test]
    fn from_stat_result_malformed_is_unknown() {
        assert_eq!(
            ProcessStatus::from_stat_result(Ok("garbage".into())),
            ProcessStatus::Unknown
        );
    }

    #[test]
    fn from_pid_for_current_process_is_running() {
        let pid = Pid::from_raw(std::process::id() as i32);
        assert_eq!(ProcessStatus::from_pid(pid), ProcessStatus::Running);
    }

    #[test]
    fn from_pid_for_nonexistent_pid_is_exited() {
        // PIDs this high are unused on Linux; /proc/<pid>/stat returns ENOENT.
        assert_eq!(
            ProcessStatus::from_pid(Pid::from_raw(i32::MAX)),
            ProcessStatus::Exited
        );
    }

    #[test]
    fn from_pid_for_invalid_pid_is_unknown() {
        assert_eq!(
            ProcessStatus::from_pid(Pid::from_raw(0)),
            ProcessStatus::Unknown
        );
        assert_eq!(
            ProcessStatus::from_pid(Pid::from_raw(-1)),
            ProcessStatus::Unknown
        );
    }
}
