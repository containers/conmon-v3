#![allow(clippy::collapsible_if)]
use ::log::LevelFilter;
use ::log::debug;
use ::log::info;
use clap::Parser;
use conmon::cli::{Cmd, Opts, determine_cmd, determine_log_plugin};
use conmon::commands::create::Create;
use conmon::commands::exec::Exec;
use conmon::commands::restore::Restore;
use conmon::commands::version::Version;
use conmon::error::ConmonResult;
use conmon::log;
use conmon::logging::plugin::initialize_log_plugin;
use ::log::LevelFilter;
use std::process::ExitCode;
use conmon::log;
use ::log::info;
use ::log::debug;

fn run_conmon() -> ConmonResult<ExitCode> {
    let opts = Opts::parse();
    if let Some(ref bundle) = opts.bundle {
        log::init_logging(
            "CONMON_LOG_PATH",
            bundle.join("conmon-debug.log"),
            "CONMON_LOG_LEVEL",
            LevelFilter::Debug,
        )?;
    }

    let git_commit = option_env!("GIT_COMMIT").unwrap_or("unknown");
    info!("Starting conmon version {git_commit}");
    debug!("Command line options: {opts:?}");

    let (plugin_name, plugin_cfg) = determine_log_plugin(&opts)?;
    info!("Using log plugin: {plugin_name:?} {plugin_cfg:?}");
    let mut log_plugin = initialize_log_plugin(&plugin_name, &plugin_cfg)?;

    let exit_code = match determine_cmd(opts)? {
        Cmd::Create(cfg) => Create::new(cfg).exec(log_plugin.as_mut())?,
        Cmd::Exec(cfg) => Exec::new(cfg).exec(log_plugin.as_mut())?,
        Cmd::Restore(cfg) => Restore::new(cfg).exec()?,
        Cmd::Version => Version {}.exec()?,
    };
    Ok(exit_code)
}

fn main() -> ExitCode {
    if let Err(e) = run_conmon() {
        eprintln!("conmon: {}", e.msg);
        return ExitCode::from(e.code);
    }
    ExitCode::SUCCESS
}
