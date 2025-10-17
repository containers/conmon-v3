use clap::Parser;
use conmon::cli::{Cmd, Opts, determine_cmd, determine_log_plugin};
use conmon::commands::create::Create;
use conmon::commands::exec::Exec;
use conmon::commands::restore::Restore;
use conmon::commands::version::Version;
use conmon::error::ConmonResult;
use conmon::logging::plugin::initialize_log_plugin;
use std::process::ExitCode;

fn run_conmon() -> ConmonResult<()> {
    let opts = Opts::parse();
    let (plugin_name, plugin_cfg) = determine_log_plugin(&opts)?;
    let log_plugin = initialize_log_plugin(&plugin_name, &plugin_cfg)?;

    match determine_cmd(opts)? {
        Cmd::Create(cfg) => Create::new(cfg).exec(log_plugin.as_ref())?,
        Cmd::Exec(cfg) => Exec::new(cfg).exec()?,
        Cmd::Restore(cfg) => Restore::new(cfg).exec()?,
        Cmd::Version => Version {}.exec()?,
    }
    Ok(())
}

fn main() -> ExitCode {
    if let Err(e) = run_conmon() {
        eprintln!("conmon: {}", e.msg);
        return ExitCode::from(e.code);
    }
    ExitCode::SUCCESS
}
