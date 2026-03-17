#![allow(clippy::collapsible_if)]
use ::log::LevelFilter;
use ::log::debug;
use ::log::error;
use ::log::info;
use clap::Parser;
use conmon::cli::{Cmd, Opts, determine_cmd, determine_log_plugin};
use conmon::commands::create::Create;
use conmon::commands::exec::Exec;
use conmon::commands::restore::Restore;
use conmon::commands::version::Version;
use conmon::error::{ConmonError, ConmonResult};
use conmon::exit::run_exit_command;
use conmon::exit::snapshot_open_fds;
use conmon::exit::write_exit_files;
use conmon::log;
use conmon::logging::plugin::initialize_log_plugins;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::ExitCode;

/// Check whether the path refers to an executable file (used for pre-validation only).
fn is_executable(p: &Path) -> bool {
    if let Ok(md) = fs::metadata(p) {
        let mode = md.permissions().mode();
        return (mode & 0o111) != 0;
    }
    false
}

/// Runs the conmon and returns the exit code.
///
/// # Arguments
///
/// * `opts` - The parsed command line options.
///
/// # Returns
///
/// * Exit code the conmon executable should exit with.
fn run_conmon(opts: Opts) -> ConmonResult<i32> {
    // Snapshot the open file descriptors.
    // Podman injects multiple fds into conmon when executing it. It uses these
    // fds to detect whether conmon still runs. We need to close these fds on
    // exit, but we do not want to close any other fd too early, otherwise we
    // could for example kill our own logging fd and stop logging completely
    // too early.
    // We therefore keep the track of fds opened on conmon start before we open
    // anything else.
    let open_files = snapshot_open_fds();

    // Start logging.
    let log_path = PathBuf::new();
    log::init_logging(
        "CONMON_LOG_PATH",
        log_path,
        "CONMON_LOG_LEVEL",
        LevelFilter::Debug,
    )?;

    // Show the basic information about conmon in the logs.
    let git_commit = option_env!("GIT_COMMIT").unwrap_or("unknown");
    info!("Starting conmon version {git_commit}");
    debug!("Command line options: {opts:?}");

    // Handle the `--version` flag here, because we want to show the output
    // even if the log_plugin cannot be initialized for whatever reason.
    if opts.version_flag {
        return Version {}.exec();
    }

    // Pre-validate core arguments so errors match conmon v2 order (e.g. cid before log-path).
    opts.cid
        .as_ref()
        .ok_or_else(|| ConmonError::new("Container ID not provided. Use --cid", 1))?;
    let api_version = opts.api_version.unwrap_or(0);
    let cuuid_required = !opts.exec || api_version >= 1;
    if cuuid_required && opts.cuuid.is_none() {
        return Err(ConmonError::new(
            "Container UUID not provided. Use --cuuid",
            1,
        ));
    }
    let runtime = opts
        .runtime
        .as_ref()
        .ok_or_else(|| ConmonError::new("Runtime path not provided. Use --runtime", 1))?;
    if !is_executable(runtime) {
        return Err(ConmonError::new(
            format!("Runtime path {} is not valid", runtime.display()),
            1,
        ));
    }

    // Parse the log plugin(s) to use and initialize them.
    let plugin_entries = determine_log_plugin(&opts)?;
    let plugin_names: Vec<&str> = plugin_entries.iter().map(|(n, _)| n.as_str()).collect();
    info!("Using log plugin(s): {:?}", plugin_names);
    let mut log_plugin = initialize_log_plugins(&plugin_entries)?;

    // logging_passthrough: only true when the sole plugin is passthrough.
    let logging_passthrough = plugin_entries.len() == 1 && plugin_entries[0].0 == "passthrough";

    // Determine the conmon subcommand to run and execute it.
    let result = match determine_cmd(opts, logging_passthrough) {
        Ok(cmd) => match cmd {
            Cmd::Create(cfg) => Create::new(cfg).exec(log_plugin.as_mut(), &open_files),
            Cmd::Exec(cfg) => Exec::new(cfg).exec(log_plugin.as_mut(), &open_files),
            Cmd::Restore(cfg) => Restore::new(cfg).exec(log_plugin.as_mut(), &open_files),
            Cmd::Version => Version {}.exec(),
        },
        Err(e) => Err(e),
    };

    // Always call write with empty buffer to trigger write of any cached
    // log lines into logs. Without that, we could loose some log messages.
    let no_data: &[u8] = &[];
    if let Err(e) = log_plugin.write(true, no_data) {
        error!("failed to drain stdout log: {e}");
    }
    if let Err(e) = log_plugin.write(false, no_data) {
        error!("failed to drain stderr log: {e}");
    }

    // Return the exit code from subcommand execution.
    result
}

fn main() -> ExitCode {
    // Parse the command line arguments and clone the ones we need
    // for the exit handling.
    let opts = Opts::parse();
    let exit_command = opts.exit_command.clone();
    let exit_command_args = opts.exit_args.clone();
    let exit_command_delay = opts.exit_delay;
    let exit_dir = opts.exit_dir.clone();
    let persist_dir = opts.persist_dir.clone();
    let cid = opts.cid.clone();

    // Run the conmon.
    let raw_code = match run_conmon(opts) {
        Ok(code) => code,
        Err(e) => {
            error!("Exiting with error message: {}", e.msg);
            eprintln!("conmon: {}", e.msg);
            info!("Exiting with status {}", e.code);
            e.code as i32
        }
    };

    // Write the exit files into persistent path. The podman has inotify
    // set for that directory and uses it to detect the conmon exit.
    write_exit_files(
        raw_code,
        persist_dir.as_ref(),
        exit_dir.as_ref(),
        cid.as_ref(),
    );

    // Run the exit command if defined by podman. We do not care about the exit
    // code here.
    let _ = run_exit_command(exit_command, exit_command_args, exit_command_delay);

    // Return the exit code from the run_conmon function.
    info!("Exiting with status {}", raw_code);
    ExitCode::from(raw_code as u8)
}
