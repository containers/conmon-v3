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
use conmon::error::ConmonResult;
use conmon::exit::run_exit_command;
use conmon::exit::set_subreaper;
use conmon::exit::write_exit_files;
use conmon::log;
use conmon::logging::plugin::initialize_log_plugin;
use std::path::PathBuf;
use std::process::ExitCode;

fn run_conmon(opts: Opts) -> ConmonResult<i32> {
    // Enable subreaper, so we can wait for container process.
    set_subreaper(true)?;

    let mut log_path = PathBuf::new();
    if let Some(ref bundle) = opts.bundle {
        log_path = bundle.join("conmon-debug.log");
    }
    log::init_logging(
        "CONMON_LOG_PATH",
        log_path,
        "CONMON_LOG_LEVEL",
        LevelFilter::Debug,
    )?;

    let git_commit = option_env!("GIT_COMMIT").unwrap_or("unknown");
    info!("Starting conmon version {git_commit}");
    debug!("Command line options: {opts:?}");

    let (plugin_name, plugin_cfg) = determine_log_plugin(&opts)?;
    info!("Using log plugin: {plugin_name:?} {plugin_cfg:?}");
    let mut log_plugin = initialize_log_plugin(&plugin_name, &plugin_cfg)?;

    let result = match determine_cmd(opts) {
        Ok(cmd) => match cmd {
            Cmd::Create(cfg) => Create::new(cfg).exec(log_plugin.as_mut()),
            Cmd::Exec(cfg) => Exec::new(cfg).exec(log_plugin.as_mut()),
            Cmd::Restore(cfg) => Restore::new(cfg).exec(),
            Cmd::Version => Version {}.exec(),
        },
        Err(e) => Err(e),
    };

    // Always call write(), even if result is Err(...)
    let no_data: &[u8] = &[];
    if let Err(e) = log_plugin.write(true, no_data) {
        error!("failed to drain stdout log: {e}");
    }
    if let Err(e) = log_plugin.write(false, no_data) {
        error!("failed to drain stderr log: {e}");
    }

    result
}

fn main() -> ExitCode {
    let opts = Opts::parse();
    let exit_command = opts.exit_command.clone();
    let exit_command_args = opts.exit_args.clone();
    let exit_command_delay = opts.exit_delay;
    let exit_dir = opts.exit_dir.clone();
    let persist_dir = opts.persist_dir.clone();
    let cid = opts.cid.clone();
    let raw_code = match run_conmon(opts) {
        Ok(code) => code,
        Err(e) => {
            error!("Exiting with error message: {}", e.msg);
            eprintln!("conmon: {}", e.msg);
            info!("Exiting with status {}", e.code);
            e.code as i32
        }
    };

    write_exit_files(
        raw_code,
        persist_dir.as_ref(),
        exit_dir.as_ref(),
        cid.as_ref(),
    );
    let _ = run_exit_command(exit_command, exit_command_args, exit_command_delay);
    ExitCode::from(raw_code as u8)
}
