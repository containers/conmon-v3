use crate::{
    error::{ConmonError, ConmonResult},
    logging::plugin::{LogPlugin, LogPluginCfg},
};
use std::{
    fs::{File, OpenOptions},
    io::Write,
    sync::{Arc, Mutex},
};

/// A simple file-based logging plugin.
///
/// Writes all log data to the configured file path.
pub struct FileLogger {
    file: Arc<Mutex<File>>,
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

        Ok(Self {
            file: Arc::new(Mutex::new(file)),
        })
    }
}

impl LogPlugin for FileLogger {
    fn write(&self, _is_stdout: bool, data: &str) -> ConmonResult<()> {
        let mut file = self.file.lock().unwrap();
        file.write_all(data.as_bytes())
            .map_err(|e| ConmonError::new(format!("Failed to write log data: {}", e), 1))?;

        file.flush()
            .map_err(|e| ConmonError::new(format!("Failed to flush log file: {}", e), 1))?;

        Ok(())
    }
}
