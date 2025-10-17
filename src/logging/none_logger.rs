use crate::{cli::Opts, error::ConmonResult, logging::plugin::LogPlugin};

/// A no-op logging plugin that discards all data.
///
/// This is useful as a default "null" logger when no logging
/// backend is desired.
pub struct NoneLogger;

impl NoneLogger {
    pub fn new(_opts: &Opts) -> Self {
        Self
    }
}

impl LogPlugin for NoneLogger {
    fn write(&self, _is_stdout: bool, _data: &str) -> ConmonResult<()> {
        Ok(())
    }
}
