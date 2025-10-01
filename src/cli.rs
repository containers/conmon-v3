use std::path::PathBuf;
use std::path::Path;
use std::fs;
use crate::error::{ConmonResult, ConmonError};

use clap::{ArgAction, Parser};

#[derive(Parser)]
#[command(
    name = "conmon",
    override_usage = "conmon [OPTIONS] -c <CID> --runtime <PATH>",
    disable_version_flag = true
)]
pub struct Opts {
    /// Conmon API version to use
    #[arg(long = "api-version", value_parser = clap::value_parser!(i32))]
    pub api_version: Option<i32>,

    /// Location of the OCI Bundle path
    #[arg(long = "bundle", short = 'b')]
    pub bundle: Option<PathBuf>,

    /// Identification of Container
    #[arg(long = "cid", short = 'c')]
    pub cid: Option<String>,

    /// PID file for the conmon process
    #[arg(long = "conmon-pidfile", short = 'P')]
    pub conmon_pidfile: Option<PathBuf>,

    /// PID file for the initial pid inside of container
    #[arg(long = "container-pidfile", short = 'p')]
    pub container_pidfile: Option<PathBuf>,

    /// Container UUID
    #[arg(long = "cuuid", short = 'u')]
    pub cuuid: Option<String>,

    /// Exec a command into a running container
    #[arg(long = "exec", short = 'e', action = ArgAction::SetTrue)]
    pub exec: bool,

    /// Attach to an exec session
    #[arg(long = "exec-attach", action = ArgAction::SetTrue)]
    pub attach: bool,

    /// Path to the process spec for execution
    #[arg(long = "exec-process-spec")]
    pub exec_process_spec: Option<PathBuf>,

    /// Path to the program to execute when the container terminates
    #[arg(long = "exit-command")]
    pub exit_command: Option<PathBuf>,

    /// Additional arg to pass to the exit command. Can be specified multiple times
    #[arg(long = "exit-command-arg")]
    pub exit_args: Vec<String>,

    /// Delay before invoking the exit command (in seconds)
    #[arg(long = "exit-delay", value_parser = clap::value_parser!(i32))]
    pub exit_delay: Option<i32>,

    /// Path to the directory where exit files are written
    #[arg(long = "exit-dir")]
    pub exit_dir: Option<PathBuf>,

    /// Leave stdin open when attached client disconnects
    #[arg(long = "leave-stdin-open", action = ArgAction::SetTrue)]
    pub leave_stdin_open: bool,

    /// Print debug logs based on log level
    #[arg(long = "log-level")]
    pub log_level: Option<String>,

    /// Log file path (can be specified multiple times)
    #[arg(long = "log-path", short = 'l')]
    pub log_path: Vec<PathBuf>,

    /// Maximum size of log file
    #[arg(long = "log-size-max", value_parser = clap::value_parser!(i64))]
    pub log_size_max: Option<i64>,

    /// Maximum size of all log files
    #[arg(long = "log-global-size-max", value_parser = clap::value_parser!(i64))]
    pub log_global_size_max: Option<i64>,

    /// Additional tag to use for logging
    #[arg(long = "log-tag")]
    pub log_tag: Option<String>,

    /// Additional label to include in logs. Can be specified multiple times
    #[arg(long = "log-label")]
    pub log_labels: Vec<String>,

    /// Do not set CONTAINER_PARTIAL_MESSAGE=true for partial lines (journald driver only)
    #[arg(long = "no-container-partial-message", action = ArgAction::SetTrue)]
    pub no_container_partial_message: bool,

    /// Container name
    #[arg(long = "name", short = 'n')]
    pub name: Option<String>,

    /// Do not create a new session keyring
    #[arg(long = "no-new-keyring", action = ArgAction::SetTrue)]
    pub no_new_keyring: bool,

    /// Do not use pivot_root
    #[arg(long = "no-pivot", action = ArgAction::SetTrue)]
    pub no_pivot: bool,

    /// Do not manually call sync on logs after container shutdown
    #[arg(long = "no-sync-log", action = ArgAction::SetTrue)]
    pub no_sync_log: bool,

    /// Persistent directory for a container
    #[arg(long = "persist-dir", short = '0')]
    pub persist_dir: Option<PathBuf>,

    /// (DEPRECATED) PID file
    #[arg(long = "pidfile", hide = true)]
    pub deprecated_pidfile: Option<PathBuf>,

    /// Replace listen pid if set for oci-runtime pid
    #[arg(long = "replace-listen-pid", action = ArgAction::SetTrue)]
    pub replace_listen_pid: bool,

    /// Restore a container from a checkpoint
    #[arg(long = "restore")]
    pub restore: Option<PathBuf>,

    /// Additional arg to pass to the restore command. (DEPRECATED)
    #[arg(long = "restore-arg", hide = true)]
    pub restore_args: Vec<String>,

    /// Path to store runtime data for the container
    #[arg(long = "runtime", short = 'r')]
    pub runtime: Option<PathBuf>,

    /// Additional arg to pass to the runtime. Can be specified multiple times
    #[arg(long = "runtime-arg")]
    pub runtime_args: Vec<String>,

    /// Additional opts to pass to the restore or exec command. Can be specified multiple times
    #[arg(long = "runtime-opt")]
    pub runtime_opts: Vec<String>,

    /// Path to the host's sd-notify socket to relay messages to
    #[arg(long = "sdnotify-socket")]
    pub sdnotify_socket: Option<PathBuf>,

    /// Location of container attach sockets
    #[arg(long = "socket-dir-path")]
    pub socket_dir_path: Option<PathBuf>,

    /// Open up a pipe to pass stdin to the container
    #[arg(long = "stdin", short = 'i', action = ArgAction::SetTrue)]
    pub stdin: bool,

    /// Keep the main conmon process as its child by only forking once
    #[arg(long = "sync", action = ArgAction::SetTrue)]
    pub sync_flag: bool,

    /// Log to syslog (use with cgroupfs cgroup manager)
    #[arg(long = "syslog", action = ArgAction::SetTrue)]
    pub syslog: bool,

    /// Enable systemd cgroup manager, rather than cgroupfs
    #[arg(long = "systemd-cgroup", short = 's', action = ArgAction::SetTrue)]
    pub systemd_cgroup: bool,

    /// Allocate a pseudo-TTY. The default is false
    #[arg(long = "terminal", short = 't', action = ArgAction::SetTrue)]
    pub terminal: bool,

    /// Kill container after specified timeout in seconds
    #[arg(long = "timeout", short = 'T', value_parser = clap::value_parser!(i32))]
    pub timeout: Option<i32>,

    /// Print the version and exit (matches C behavior; not clap's -V)
    #[arg(long = "version", action = ArgAction::SetTrue)]
    pub version_flag: bool,

    /// Don't truncate path to the attach socket (ignore --socket-dir-path)
    #[arg(long = "full-attach", action = ArgAction::SetTrue)]
    pub full_attach: bool,

    /// Path to the socket where the seccomp notification fd is received
    #[arg(long = "seccomp-notify-socket")]
    pub seccomp_notify_socket: Option<PathBuf>,

    /// Plugins to use for managing the seccomp notifications
    #[arg(long = "seccomp-notify-plugins")]
    pub seccomp_notify_plugins: Option<String>,

    /// Enable log rotation instead of truncation when log-size-max is reached
    #[arg(long = "log-rotate", action = ArgAction::SetTrue)]
    pub log_rotate: bool,

    /// Number of backup log files to keep (default: 1)
    #[arg(long = "log-max-files", value_parser = clap::value_parser!(i32), default_value_t = 1)]
    pub log_max_files: i32,

    /// Allowed log directory (can be specified multiple times)
    #[arg(long = "log-allowlist-dir")]
    pub log_allowlist_dir: Vec<PathBuf>,
}

#[derive(Debug)]
pub enum Cmd {
    Version,
    Run(RunCfg),
    Exec(ExecCfg),
    Restore(RestoreCfg),
}

#[derive(Debug)]
pub struct CommonCfg {
    pub api_version: i32,
    pub cid: String,
    pub cuuid: Option<String>,
    pub runtime: PathBuf,
}

#[derive(Debug)]
pub struct RunCfg {
    pub common: CommonCfg,
    pub bundle: Option<PathBuf>,
    pub container_pidfile: PathBuf,
}

#[derive(Debug)]
pub struct ExecCfg {
    pub common: CommonCfg,
    pub exec_process_spec: PathBuf,
    pub attach: bool,
}

#[derive(Debug)]
pub struct RestoreCfg {
    pub common: CommonCfg,
    pub restore_path: PathBuf,
}

/// Try to detect "executable" bit on Unix-y platforms.
/// On non-Unix, we accept existence as "good enough".
fn is_executable(p: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(md) = fs::metadata(p) {
            let mode = md.permissions().mode();
            return (mode & 0o111) != 0;
        }
        false
    }
    #[cfg(not(unix))]
    {
        p.exists()
    }
}

pub fn determine_cmd(mut opts: Opts) -> ConmonResult<Cmd> {
    let api_version = opts.api_version.unwrap_or(0);

    if opts.version_flag {
        return Ok(Cmd::Version);
    }

    // basic presence validation
    let cid = opts.cid.take().ok_or_else(|| ConmonError::new("Container ID not provided. Use --cid", 1))?;
    let runtime = opts.runtime.take().ok_or_else(|| ConmonError::new("Runtime path not provided. Use --runtime", 1))?;

    // mutual exclusions and dependencies
    if opts.restore.is_some() && opts.exec {
        return Err(ConmonError::new("Cannot use 'exec' and 'restore' at the same time", 1));
    }
    if !opts.exec && opts.attach {
        return Err(ConmonError::new("Attach can only be specified with exec", 1));
    }
    if api_version < 1 && opts.attach {
        return Err(ConmonError::new("Attach can only be specified for a non-legacy exec session", 1));
    }

    // cuuid rule: required unless legacy exec API (<1) with --exec
    if opts.cuuid.is_none() && (!opts.exec || api_version >= 1) {
        return Err(ConmonError::new("Container UUID not provided. Use --cuuid", 1));
    }

    // runtime must be executable
    if !is_executable(&runtime) {
        return Err(ConmonError::new(format!("Runtime path {} is not valid", runtime.display()), 1));
    }

    let common = CommonCfg {
        api_version,
        cid,
        cuuid: opts.cuuid.take(),
        runtime,
    };

    // decide which subcommand this flag combination means
    if let Some(restore_path) = opts.restore.take() {
        Ok(Cmd::Restore(RestoreCfg { common, restore_path }))
    } else if opts.exec {
        let exec_process_spec = opts.exec_process_spec
            .take()
            .ok_or_else(|| ConmonError::new("Exec process spec path not provided. Use --exec-process-spec", 1))?;
        Ok(Cmd::Exec(ExecCfg {
            common,
            exec_process_spec,
            attach: opts.attach,
        }))
    } else {
        let cwd = std::env::current_dir()
            .map_err(|e| ConmonError::new(format!("Failed to get working directory: {e}"), 1))?;

        // bundle defaults to "$cwd" if none provided
        let bundle = opts.bundle.take().or_else(|| Some(cwd.clone()));

        // container-pidfile defaults to "$cwd/pidfile-$cid" if none provided
        let container_pidfile = opts
            .container_pidfile
            .take()
            .unwrap_or_else(|| cwd.join(format!("pidfile-{}", common.cid)));

        Ok(Cmd::Run(RunCfg {
            common,
            bundle,
            container_pidfile,
        }))
    }
}
