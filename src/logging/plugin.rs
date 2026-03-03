use std::path::PathBuf;

use crate::{
    error::{ConmonError, ConmonResult},
    logging::{file_logger::FileLogger, journald_logger::JournaldLogger, none_logger::NoneLogger},
};

pub trait LogPlugin {
    fn write(&mut self, is_stdout: bool, data: &[u8]) -> ConmonResult<()>;
    fn reopen(&mut self) -> ConmonResult<()>;
}

#[derive(Default, Debug, Clone)]
pub struct LogPluginCfg {
    pub path: PathBuf,
    pub cid: Option<String>,
    pub cuuid: Option<String>,
    pub log_tag: Option<String>,
    pub log_labels: Vec<String>,
    pub no_container_partial_message: bool,
    pub name: Option<String>,
    pub no_sync: bool,
    pub max_size: usize,
    pub global_max_size: usize,
    pub max_files: i32,
    pub allowlist_dirs: Option<Vec<PathBuf>>,
    pub rotate: bool,
}

pub fn initialize_log_plugin(name: &str, cfg: &LogPluginCfg) -> ConmonResult<Box<dyn LogPlugin>> {
    match name {
        "none" | "passthrough" => Ok(Box::new(NoneLogger::new(cfg)?)),
        "file" | "k8s_file" => Ok(Box::new(FileLogger::new(cfg)?)),
        "journald" => Ok(Box::new(JournaldLogger::new(cfg)?)),
        _ => Err(ConmonError::new(format!("No such log driver {name}"), 1)),
    }
}
