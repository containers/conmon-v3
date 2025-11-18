use std::process::ExitCode;

use crate::error::ConmonResult;

pub struct Version {}

impl Version {
    pub fn exec(&self) -> ConmonResult<ExitCode> {
        let version = env!("CARGO_PKG_VERSION");
        let git_commit = option_env!("GIT_COMMIT").unwrap_or("unknown");
        println!("conmon version {version}\ncommit: {git_commit}");

        Ok(ExitCode::SUCCESS)
    }
}
