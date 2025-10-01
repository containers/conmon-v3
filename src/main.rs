use clap::Parser;
use std::process::ExitCode;
use conmon::error::ConmonResult;
use conmon::cli::{Opts, Cmd, determine_cmd};
use conmon::commands::version::Version;
use conmon::commands::run::Run;
use conmon::commands::restore::Restore;
use conmon::commands::exec::Exec;

fn run_conmon() -> ConmonResult<()> {
    let opts = Opts::parse();
    match determine_cmd(opts)? {
        Cmd::Run(cfg)     => Run {}.exec(cfg)?,
        Cmd::Exec(cfg)    => Exec {}.exec(cfg)?,
        Cmd::Restore(cfg) => Restore {}.exec(cfg)?,
        Cmd::Version      => Version {}.exec()?,
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
