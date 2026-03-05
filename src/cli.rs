use crate::error::{ConmonError, ConmonResult};
use crate::logging::plugin::LogPluginCfg;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;

use clap::{ArgAction, Parser};
use log::warn;

#[derive(Parser)]
#[command(
    name = "conmon",
    about = "OCI container runtime monitor",
    long_about = "An OCI container runtime monitor (conmon v3). Monitors containers and handles logging, attach, and lifecycle.",
    override_usage = "conmon [OPTIONS] -c <CID> --runtime <PATH>",
    disable_version_flag = true
)]
#[derive(Default, Debug)]
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
    #[arg(long = "exit-command-arg", allow_hyphen_values = true)]
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
    #[arg(long = "restore-arg", hide = true, allow_hyphen_values = true)]
    pub restore_args: Vec<String>,

    /// Path to store runtime data for the container
    #[arg(long = "runtime", short = 'r')]
    pub runtime: Option<PathBuf>,

    /// Additional arg to pass to the runtime. Can be specified multiple times
    #[arg(long = "runtime-arg", allow_hyphen_values = true)]
    pub runtime_args: Vec<String>,

    /// Additional opts to pass to the restore or exec command. Can be specified multiple times
    #[arg(long = "runtime-opt", allow_hyphen_values = true)]
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
    #[arg(long = "log-rotate", action = ArgAction::SetTrue, default_value_t = false)]
    pub log_rotate: bool,

    /// Number of backup log files to keep (default: 1)
    #[arg(long = "log-max-files", value_parser = clap::value_parser!(i64), allow_hyphen_values = true, default_value_t = 1)]
    pub log_max_files: i64,

    /// Allowed log directory (can be specified multiple times)
    #[arg(long = "log-allowlist-dir")]
    pub log_allowlist_dir: Vec<PathBuf>,
}

#[derive(Debug)]
pub enum Cmd {
    Version,
    Create(CreateCfg),
    Exec(ExecCfg),
    Restore(RestoreCfg),
}

#[derive(Debug, Default)]
pub struct CommonCfg {
    pub api_version: i32,
    pub cid: String,
    pub cuuid: Option<String>,
    pub runtime: PathBuf,
    pub runtime_args: Vec<String>,
    pub runtime_opts: Vec<String>,
    pub no_pivot: bool,
    pub no_new_keyring: bool,
    pub conmon_pidfile: Option<PathBuf>,
    pub container_pidfile: PathBuf,
    pub bundle: PathBuf,
    pub full_attach: bool,
    pub socket_dir_path: PathBuf,
    pub stdin: bool,
    pub leave_stdin_open: bool,
    pub terminal: bool,
    pub timeout: Option<i32>,
    pub replace_listen_pid: bool,
    pub persist_dir: Option<PathBuf>,
    pub exit_dir: Option<PathBuf>,
    pub name: Option<String>,
    pub no_sync_log: bool,
    pub logging_passthrough: bool,
    pub sync_flag: bool,
    pub sdnotify_socket: Option<PathBuf>,
}

#[derive(Debug, Default)]
pub struct CreateCfg {
    pub common: CommonCfg,
    pub systemd_cgroup: bool,
}

#[derive(Debug, Default)]
pub struct ExecCfg {
    pub common: CommonCfg,
    pub exec_process_spec: PathBuf,
    pub attach: bool,
}

#[derive(Debug, Default)]
pub struct RestoreCfg {
    pub common: CommonCfg,
    pub restore_path: PathBuf,
    pub systemd_cgroup: bool,
}

/// Try to detect "executable" bit.
fn is_executable(p: &Path) -> bool {
    if let Ok(md) = fs::metadata(p) {
        let mode = md.permissions().mode();
        return (mode & 0o111) != 0;
    }
    false
}

pub fn determine_cmd(mut opts: Opts, logging_passthrough: bool) -> ConmonResult<Cmd> {
    let api_version = opts.api_version.unwrap_or(0);

    if opts.version_flag {
        return Ok(Cmd::Version);
    }

    // basic presence validation
    let cid = opts
        .cid
        .take()
        .ok_or_else(|| ConmonError::new("Container ID not provided. Use --cid", 1))?;
    let runtime = opts
        .runtime
        .take()
        .ok_or_else(|| ConmonError::new("Runtime path not provided. Use --runtime", 1))?;

    // mutual exclusions and dependencies
    if opts.restore.is_some() && opts.exec {
        return Err(ConmonError::new(
            "Cannot use 'exec' and 'restore' at the same time",
            1,
        ));
    }
    if !opts.exec && opts.attach {
        return Err(ConmonError::new(
            "Attach can only be specified with exec",
            1,
        ));
    }
    if api_version < 1 && opts.attach {
        return Err(ConmonError::new(
            "Attach can only be specified for a non-legacy exec session",
            1,
        ));
    }

    // cuuid rule: required unless legacy exec API (<1) with --exec
    if opts.cuuid.is_none() && (!opts.exec || api_version >= 1) {
        return Err(ConmonError::new(
            "Container UUID not provided. Use --cuuid",
            1,
        ));
    }

    // runtime must be executable
    if !is_executable(&runtime) {
        return Err(ConmonError::new(
            format!("Runtime path {} is not valid", runtime.display()),
            1,
        ));
    }

    let cwd = std::env::current_dir()
        .map_err(|e| ConmonError::new(format!("Failed to get working directory: {e}"), 1))?;

    // container-pidfile defaults to "$cwd/pidfile-$cid" if none provided
    let container_pidfile = opts
        .container_pidfile
        .take()
        .unwrap_or_else(|| cwd.join(format!("pidfile-{}", cid)));

    // bundle defaults to "$cwd" if none provided
    let bundle = opts.bundle.take().unwrap_or_else(|| cwd.clone());

    let socket_dir_path = opts
        .socket_dir_path
        .take()
        .unwrap_or_else(|| PathBuf::from("/var/run/crio"));

    let common = CommonCfg {
        api_version,
        cid,
        cuuid: opts.cuuid.take(),
        runtime,
        runtime_args: opts.runtime_args,
        runtime_opts: opts.runtime_opts,
        no_pivot: opts.no_pivot,
        no_new_keyring: opts.no_new_keyring,
        conmon_pidfile: opts.conmon_pidfile,
        container_pidfile,
        bundle,
        full_attach: opts.full_attach,
        socket_dir_path,
        stdin: opts.stdin,
        leave_stdin_open: opts.leave_stdin_open,
        terminal: opts.terminal,
        timeout: opts.timeout,
        replace_listen_pid: opts.replace_listen_pid,
        persist_dir: opts.persist_dir,
        exit_dir: opts.exit_dir,
        name: opts.name,
        no_sync_log: opts.no_sync_log,
        logging_passthrough,
        sync_flag: opts.sync_flag,
        sdnotify_socket: opts.sdnotify_socket,
    };

    // decide which subcommand this flag combination means
    if let Some(restore_path) = opts.restore.take() {
        Ok(Cmd::Restore(RestoreCfg {
            common,
            restore_path,
            systemd_cgroup: opts.systemd_cgroup,
        }))
    } else if opts.exec {
        let exec_process_spec = opts.exec_process_spec.take().ok_or_else(|| {
            ConmonError::new(
                "Exec process spec path not provided. Use --exec-process-spec",
                1,
            )
        })?;
        Ok(Cmd::Exec(ExecCfg {
            common,
            exec_process_spec,
            attach: opts.attach,
        }))
    } else {
        Ok(Cmd::Create(CreateCfg {
            common,
            systemd_cgroup: opts.systemd_cgroup,
        }))
    }
}

// Handles the logging related options from `opts` and returns a list of (plugin name, LogPluginCfg)
// so that multiple log plugins can be configured (one entry per --log-path).
pub fn determine_log_plugin(opts: &Opts) -> ConmonResult<Vec<(String, LogPluginCfg)>> {
    if opts.log_path.is_empty() {
        return Err(ConmonError::new(
            "Log driver not provided. Use --log-path",
            1,
        ));
    }

    // Validate and normalize log-max-files bounds (apply to all file-based plugins).
    let raw_max_files = opts.log_max_files;
    if raw_max_files < 0 {
        return Err(ConmonError::new(
            "log-max-files must be non-negative",
            1,
        ));
    }
    if opts.log_rotate && raw_max_files == 0 {
        return Err(ConmonError::new(
            "log-max-files must be at least 1 when log-rotate is enabled",
            1,
        ));
    }
    if raw_max_files > i32::MAX as i64 {
        return Err(ConmonError::new("log-max-files out of range", 1));
    }
    let max_files = raw_max_files as i32;

    // Base config from non-path options (shared by all plugin instances).
    let base_cfg = LogPluginCfg {
        path: PathBuf::new(),
        cid: opts.cid.clone(),
        cuuid: opts.cuuid.clone(),
        log_tag: opts.log_tag.clone(),
        log_labels: opts.log_labels.clone(),
        no_container_partial_message: opts.no_container_partial_message,
        name: opts.name.clone(),
        no_sync: opts.no_sync_log,
        max_size: opts.log_size_max.unwrap_or_else(|| 0) as usize,
        global_max_size: opts.log_global_size_max.unwrap_or_else(|| 0) as usize,
        max_files,
        allowlist_dirs: if opts.log_allowlist_dir.is_empty() {
            None
        } else {
            Some(opts.log_allowlist_dir.clone())
        },
        rotate: opts.log_rotate,
    };

    let mut entries: Vec<(String, LogPluginCfg)> = Vec::with_capacity(opts.log_path.len());

    for p in &opts.log_path {
        let s = p.to_string_lossy();
        let mut plugin: String = "file".into();
        let mut path = PathBuf::new();

        if let Some((plug, path_str)) = s.split_once(':') {
            let path_str = path_str.trim();
            if !path_str.is_empty() {
                path = path_str.into();
            }
            let plug = plug.trim();
            if !plug.is_empty() {
                plugin = plug.replace("-", "_");
            }
        } else if s == "journald" {
            plugin = "journald".to_string();
        } else if s == "passthrough" {
            plugin = "passthrough".to_string();
        } else if !s.is_empty() {
            path = s.to_string().into();
        }

        let mut cfg = base_cfg.clone();
        cfg.path = path;
        entries.push((plugin, cfg));
    }

    // Passthrough must be the sole plugin: reject mixing with others.
    let passthrough_count = entries.iter().filter(|(name, _)| name == "passthrough").count();
    if passthrough_count > 0 && entries.len() > 1 {
        return Err(ConmonError::new(
            "passthrough log driver cannot be combined with other log drivers",
            1,
        ));
    }

    let has_journald = entries.iter().any(|(name, _)| name == "journald");
    if opts.no_container_partial_message && !has_journald {
        warn!("--no-container-partial-message has no effect without journald log driver");
    }

    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    /// Create a temp file with the given mode.
    fn make_temp_file_with_mode(mode: u32) -> NamedTempFile {
        let f = NamedTempFile::new().expect("tmp file");
        let p = f.path().to_path_buf();
        let mut perms = std::fs::metadata(&p).unwrap().permissions();
        perms.set_mode(mode);
        std::fs::set_permissions(&p, perms).unwrap();
        f
    }

    #[test]
    fn version_flag_returns_version_cmd() -> ConmonResult<()> {
        let o = Opts {
            version_flag: true,
            ..Default::default()
        };
        // Even if other required fields are missing, version should short-circuit
        let cmd = determine_cmd(o, false).expect("ok");
        match cmd {
            Cmd::Version => {}
            _ => panic!("expected Version"),
        }
        Ok(())
    }

    #[test]
    fn missing_cid_errors() -> ConmonResult<()> {
        let o = Opts {
            ..Default::default()
        };
        let err = determine_cmd(o, false).unwrap_err();
        assert!(err.to_string().contains("Container ID not provided"));
        Ok(())
    }

    #[test]
    fn missing_runtime_errors() -> ConmonResult<()> {
        let o = Opts {
            cid: Some("abc".into()),
            ..Default::default()
        };
        let err = determine_cmd(o, false).unwrap_err();
        assert!(err.to_string().contains("Runtime path not provided"));
        Ok(())
    }

    #[test]
    fn attach_without_exec_errors() -> ConmonResult<()> {
        let runtime = make_temp_file_with_mode(0o700);
        let o = Opts {
            attach: true,
            cid: Some("abc".into()),
            runtime: Some(runtime.path().to_path_buf()),
            ..Default::default()
        };
        let err = determine_cmd(o, false).unwrap_err();
        assert!(
            err.to_string()
                .contains("Attach can only be specified with exec")
        );
        Ok(())
    }

    #[test]
    fn attach_legacy_api_errors_even_with_exec() -> ConmonResult<()> {
        let runtime = make_temp_file_with_mode(0o700);
        let o = Opts {
            api_version: Some(0),
            exec: true,
            attach: true,
            cid: Some("abc".into()),
            runtime: Some(runtime.path().to_path_buf()),
            ..Default::default()
        };
        let err = determine_cmd(o, false).unwrap_err();
        assert!(err.to_string().contains("non-legacy exec session"));
        Ok(())
    }

    #[test]
    fn missing_cuuid_for_run_errors() -> ConmonResult<()> {
        let runtime = make_temp_file_with_mode(0o700);
        let o = Opts {
            cid: Some("abc".into()),
            runtime: Some(runtime.path().to_path_buf()),
            ..Default::default()
        };
        // run path (no exec/restore) requires cuuid
        let err = determine_cmd(o, false).unwrap_err();
        assert!(err.to_string().contains("Container UUID not provided"));
        Ok(())
    }

    #[test]
    fn cannot_mix_exec_and_restore() -> ConmonResult<()> {
        let runtime = make_temp_file_with_mode(0o700);
        let o = Opts {
            exec: true,
            restore: Some(PathBuf::from("checkpoint")),
            cid: Some("abc".into()),
            cuuid: Some("u1".into()),
            runtime: Some(runtime.path().to_path_buf()),
            ..Default::default()
        };
        let err = determine_cmd(o, false).unwrap_err();
        assert!(err.to_string().contains("Cannot use 'exec' and 'restore'"));
        Ok(())
    }

    #[test]
    fn runtime_must_be_executable() -> ConmonResult<()> {
        let runtime = make_temp_file_with_mode(0o600);
        let o = Opts {
            cid: Some("abc".into()),
            cuuid: Some("u1".into()),
            runtime: Some(runtime.path().to_path_buf()),
            ..Default::default()
        };
        let err = determine_cmd(o, false).unwrap_err();
        assert!(err.to_string().contains("is not valid"));
        Ok(())
    }

    #[test]
    fn exec_success_with_spec_and_attach_new_api() -> ConmonResult<()> {
        let runtime = make_temp_file_with_mode(0o700);
        let o = Opts {
            api_version: Some(1),
            exec: true,
            attach: true,
            cid: Some("abc".into()),
            cuuid: Some("u1".into()),
            runtime: Some(runtime.path().to_path_buf()),
            exec_process_spec: Some(PathBuf::from("proc.json")),
            ..Default::default()
        };
        let cmd = determine_cmd(o, false).expect("ok");
        match cmd {
            Cmd::Exec(cfg) => {
                assert_eq!(cfg.common.api_version, 1);
                assert_eq!(cfg.common.cid, "abc");
                assert!(cfg.attach);
                assert_eq!(cfg.exec_process_spec, PathBuf::from("proc.json"));
            }
            _ => panic!("expected Exec"),
        }
        Ok(())
    }

    #[test]
    fn exec_missing_spec_errors() -> ConmonResult<()> {
        let runtime = make_temp_file_with_mode(0o700);
        let o = Opts {
            api_version: Some(1),
            exec: true,
            cid: Some("abc".into()),
            cuuid: Some("u1".into()),
            runtime: Some(runtime.path().to_path_buf()),
            ..Default::default()
        };
        let err = determine_cmd(o, false).unwrap_err();
        assert!(
            err.to_string()
                .contains("Exec process spec path not provided")
        );
        Ok(())
    }

    #[test]
    fn restore_success() -> ConmonResult<()> {
        let runtime = make_temp_file_with_mode(0o700);
        let o = Opts {
            cid: Some("abc".into()),
            cuuid: Some("u1".into()),
            runtime: Some(runtime.path().to_path_buf()),
            restore: Some(PathBuf::from("checkpoint")),
            ..Default::default()
        };
        let cmd = determine_cmd(o, false).expect("ok");
        match cmd {
            Cmd::Restore(cfg) => {
                assert_eq!(cfg.common.cid, "abc");
                assert_eq!(cfg.restore_path, PathBuf::from("checkpoint"));
            }
            _ => panic!("expected Restore"),
        }
        Ok(())
    }

    #[test]
    fn run_defaults_success() -> ConmonResult<()> {
        let runtime = make_temp_file_with_mode(0o700);
        let o = Opts {
            cid: Some("abc".into()),
            cuuid: Some("u1".into()),
            runtime: Some(runtime.path().to_path_buf()),
            ..Default::default()
        };
        // no bundle/container_pidfile specified -> defaults should kick in
        let cwd = std::env::current_dir()?;
        let cmd = determine_cmd(o, false).expect("ok");
        match cmd {
            Cmd::Create(cfg) => {
                // bundle defaults to cwd
                assert_eq!(cfg.common.bundle, cwd);
                // container-pidfile defaults to "$cwd/pidfile-$cid"
                assert_eq!(cfg.common.container_pidfile, cwd.join("pidfile-abc"));
            }
            _ => panic!("expected Run"),
        }
        Ok(())
    }

    #[test]
    fn is_executable_behaves_as_expected() -> ConmonResult<()> {
        let exec = make_temp_file_with_mode(0o700);
        assert!(is_executable(exec.path()));

        let nonexec = make_temp_file_with_mode(0o600);
        assert!(!is_executable(nonexec.path()));
        Ok(())
    }

    #[test]
    fn plain_path_without_plugin_prefix() -> ConmonResult<()> {
        let o = Opts {
            log_path: vec![PathBuf::from("/var/log/my.log")],
            ..Default::default()
        };

        let entries = determine_log_plugin(&o)?;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, "file");
        assert_eq!(entries[0].1.path, PathBuf::from("/var/log/my.log"));
        Ok(())
    }

    #[test]
    fn plugin_and_path_with_whitespace_are_trimmed() -> ConmonResult<()> {
        let o = Opts {
            log_path: vec![PathBuf::from("  file  :  /var/log/app.log  ")],
            ..Default::default()
        };

        let entries = determine_log_plugin(&o)?;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, "file");
        assert_eq!(entries[0].1.path, PathBuf::from("/var/log/app.log"));
        Ok(())
    }

    #[test]
    fn null_plugin_alias_is_parsed() -> ConmonResult<()> {
        let o = Opts {
            log_path: vec![PathBuf::from("null:/var/log/null.log")],
            ..Default::default()
        };

        let entries = determine_log_plugin(&o)?;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, "null");
        assert_eq!(entries[0].1.path, PathBuf::from("/var/log/null.log"));
        Ok(())
    }

    #[test]
    fn off_plugin_alias_is_parsed() -> ConmonResult<()> {
        let o = Opts {
            log_path: vec![PathBuf::from("off:/var/log/off.log")],
            ..Default::default()
        };

        let entries = determine_log_plugin(&o)?;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, "off");
        assert_eq!(entries[0].1.path, PathBuf::from("/var/log/off.log"));
        Ok(())
    }

    #[test]
    fn plugin_dash_is_normalized_to_underscore() -> ConmonResult<()> {
        let o = Opts {
            log_path: vec![PathBuf::from("k8s-file:/var/log/k8s.log")],
            ..Default::default()
        };

        let entries = determine_log_plugin(&o)?;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, "k8s_file");
        assert_eq!(entries[0].1.path, PathBuf::from("/var/log/k8s.log"));
        Ok(())
    }

    #[test]
    fn empty_plugin_part_does_not_change_plugin_but_sets_path() -> ConmonResult<()> {
        // Starts with default "file" plugin, but entry has empty plugin name.
        // Should still set the path from the right side of the colon.
        let o = Opts {
            log_path: vec![PathBuf::from(":/tmp/only-path.log")],
            ..Default::default()
        };

        let entries = determine_log_plugin(&o)?;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, "file"); // unchanged
        assert_eq!(entries[0].1.path, PathBuf::from("/tmp/only-path.log"));
        Ok(())
    }

    #[test]
    fn log_max_files_negative_is_rejected() {
        let o = Opts {
            log_path: vec![PathBuf::from("/var/log/my.log")],
            log_max_files: -1,
            ..Default::default()
        };

        let err = determine_log_plugin(&o).unwrap_err();
        assert!(err
            .to_string()
            .contains("log-max-files must be non-negative"));
    }

    #[test]
    fn log_max_files_zero_with_rotate_is_rejected() {
        let o = Opts {
            log_path: vec![PathBuf::from("/var/log/my.log")],
            log_rotate: true,
            log_max_files: 0,
            ..Default::default()
        };

        let err = determine_log_plugin(&o).unwrap_err();
        assert!(err
            .to_string()
            .contains("log-max-files must be at least 1"));
    }

    #[test]
    fn allowlist_dirs_is_none_when_not_specified() -> ConmonResult<()> {
        let o = Opts {
            log_path: vec![PathBuf::from("/var/log/my.log")],
            ..Default::default()
        };

        let entries = determine_log_plugin(&o)?;
        assert_eq!(entries.len(), 1);
        assert!(entries[0].1.allowlist_dirs.is_none());
        Ok(())
    }

    #[test]
    fn multiple_log_paths_produce_multiple_entries() -> ConmonResult<()> {
        let o = Opts {
            log_path: vec![
                PathBuf::from("file:/var/log/a.log"),
                PathBuf::from("journald"),
            ],
            cid: Some("cid".into()),
            cuuid: Some("cuuid".into()),
            ..Default::default()
        };

        let entries = determine_log_plugin(&o)?;
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].0, "file");
        assert_eq!(entries[0].1.path, PathBuf::from("/var/log/a.log"));
        assert_eq!(entries[1].0, "journald");
        assert_eq!(entries[1].1.path, PathBuf::new());
        Ok(())
    }

    #[test]
    fn passthrough_combined_with_other_plugin_is_rejected() {
        let o = Opts {
            log_path: vec![
                PathBuf::from("passthrough"),
                PathBuf::from("/var/log/other.log"),
            ],
            ..Default::default()
        };

        let err = determine_log_plugin(&o).unwrap_err();
        assert!(err
            .to_string()
            .contains("passthrough log driver cannot be combined"));
    }
}
