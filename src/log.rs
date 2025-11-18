use chrono::Utc;
use log::{LevelFilter, Log, Metadata, Record};
use std::fs::OpenOptions;
use std::{fs::File, io::Write, path::PathBuf, sync::Mutex};

use crate::error::{ConmonError, ConmonResult};

pub struct FileLogger {
    level: LevelFilter,
    file: Mutex<File>,
}

impl FileLogger {
    pub fn new(file: File, level: LevelFilter) -> Self {
        Self {
            level,
            file: Mutex::new(file),
        }
    }
}

impl Log for FileLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= self.level
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }

        let now = Utc::now();
        let mut file = match self.file.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };

        let _ = writeln!(
            &mut *file,
            "[{}][{:>5}] {}: {}",
            now.to_rfc3339(),
            record.level(),
            record.target(),
            record.args()
        );
    }

    fn flush(&self) {
        if let Ok(mut file) = self.file.lock() {
            let _ = file.flush();
        }
    }
}

pub fn init_logging(
    path_env_var: &str,
    default_path: PathBuf,
    level_env_var: &str,
    default_level: LevelFilter,
) -> ConmonResult<()> {
    let level = std::env::var(level_env_var)
        .ok()
        .and_then(|s| s.parse::<LevelFilter>().ok())
        .unwrap_or(default_level);

    let path = std::env::var(path_env_var)
        .ok()
        .unwrap_or(default_path.to_string_lossy().to_string());

    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| ConmonError::new(format!("Failed to open log file: {e}"), 1))?;

    let logger = FileLogger::new(file, level);

    log::set_max_level(level);
    log::set_boxed_logger(Box::new(logger))
        .map_err(|e| ConmonError::new(format!("Failed to create logger: {e}"), 1))
}
