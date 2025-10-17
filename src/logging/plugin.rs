use std::path::PathBuf;

use crate::{
    error::{ConmonError, ConmonResult},
    logging::{file_logger::FileLogger, none_logger::NoneLogger},
};

pub trait LogPlugin {
    fn write(&self, is_stdout: bool, data: &str) -> ConmonResult<()>;
}

#[derive(Default)]
pub struct LogPluginCfg {
    pub path: PathBuf,
}

pub fn initialize_log_plugin(name: &str, cfg: &LogPluginCfg) -> ConmonResult<Box<dyn LogPlugin>> {
    match name {
        "none" => Ok(Box::new(NoneLogger::new(cfg)?)),
        "file" | "k8s_file" => Ok(Box::new(FileLogger::new(cfg)?)),
        _ => Err(ConmonError::new(format!("Unknown log plugin: {name}"), 1)),
    }
}
