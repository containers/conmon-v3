use crate::{
    error::ConmonResult,
    logging::plugin::{LogPlugin, LogPluginCfg},
};

/// A no-op logging plugin that discards all data.
///
/// This is useful as a default "null" logger when no logging
/// backend is desired.
pub struct NoneLogger;

impl NoneLogger {
    pub fn new(_cfg: &LogPluginCfg) -> ConmonResult<Self> {
        Ok(Self)
    }
}

impl LogPlugin for NoneLogger {
    fn write(&mut self, _is_stdout: bool, _data: &[u8]) -> ConmonResult<()> {
        Ok(())
    }
}
