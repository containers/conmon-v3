use crate::{
    error::{ConmonError, ConmonResult},
    logging::plugin::{LogPlugin, LogPluginCfg},
};
use std::{
    fs::{File, OpenOptions},
    io::Write,
};

/// A simple file-based logging plugin.
///
/// Writes all log data to the configured file path.
pub struct FileLogger {
    file: File,
}

impl FileLogger {
    pub fn new(cfg: &LogPluginCfg) -> ConmonResult<Self> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&cfg.path)
            .map_err(|e| {
                ConmonError::new(
                    format!("Failed to open log file {}: {}", cfg.path.display(), e),
                    1,
                )
            })?;

        Ok(Self { file })
    }
}

impl LogPlugin for FileLogger {
    fn write(&mut self, _is_stdout: bool, data: &[u8]) -> ConmonResult<()> {
        self.file
            .write_all(data)
            .map_err(|e| ConmonError::new(format!("Failed to write log data: {}", e), 1))?;

        Ok(())
    }
}
