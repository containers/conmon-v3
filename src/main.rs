use clap::Parser;
use conmon::cli::{Cmd, Opts, determine_cmd};
use conmon::commands::exec::Exec;
use conmon::commands::restore::Restore;
use conmon::commands::run::Run;
use conmon::commands::version::Version;
use conmon::error::ConmonResult;
use conmon::logging::plugin::initialize_log_plugin;
use std::process::ExitCode;

fn run_conmon() -> ConmonResult<()> {
    let opts = Opts::parse();
    let _log_plugin = initialize_log_plugin("none", &opts);

    match determine_cmd(opts)? {
        Cmd::Run(cfg) => Run {}.exec(cfg)?,
        Cmd::Exec(cfg) => Exec {}.exec(cfg)?,
        Cmd::Restore(cfg) => Restore {}.exec(cfg)?,
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
