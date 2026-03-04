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

/// Creates a single log plugin from name and config.
fn create_log_plugin(name: &str, cfg: &LogPluginCfg) -> ConmonResult<Box<dyn LogPlugin>> {
    match name {
        "none" | "passthrough" => Ok(Box::new(NoneLogger::new(cfg)?)),
        "file" | "k8s_file" => Ok(Box::new(FileLogger::new(cfg)?)),
        "journald" => Ok(Box::new(JournaldLogger::new(cfg)?)),
        _ => Err(ConmonError::new(format!("No such log driver {name}"), 1)),
    }
}

/// Composite log plugin that fans out write() and reopen() to multiple plugins.
pub struct MultiLogPlugin {
    plugins: Vec<Box<dyn LogPlugin>>,
}

impl MultiLogPlugin {
    pub fn new(plugins: Vec<Box<dyn LogPlugin>>) -> Self {
        Self { plugins }
    }
}

impl LogPlugin for MultiLogPlugin {
    fn write(&mut self, is_stdout: bool, data: &[u8]) -> ConmonResult<()> {
        let mut first_error: Option<ConmonError> = None;
        for p in &mut self.plugins {
            if let Err(e) = p.write(is_stdout, data) {
                if first_error.is_none() {
                    first_error = Some(e);
                }
            }
        }
        match first_error {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    fn reopen(&mut self) -> ConmonResult<()> {
        let mut first_error: Option<ConmonError> = None;
        for p in &mut self.plugins {
            if let Err(e) = p.reopen() {
                if first_error.is_none() {
                    first_error = Some(e);
                }
            }
        }
        match first_error {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}

/// Initializes one or more log plugins from (name, cfg) entries.
/// If there is exactly one entry, returns that plugin directly; otherwise
/// returns a MultiLogPlugin that fans out to all of them.
pub fn initialize_log_plugins(
    entries: &[(String, LogPluginCfg)],
) -> ConmonResult<Box<dyn LogPlugin>> {
    if entries.is_empty() {
        return Err(ConmonError::new("No log plugin entries provided", 1));
    }
    let mut plugins: Vec<Box<dyn LogPlugin>> = Vec::with_capacity(entries.len());
    for (name, cfg) in entries {
        plugins.push(create_log_plugin(name, cfg)?);
    }
    if plugins.len() == 1 {
        Ok(plugins.into_iter().next().unwrap())
    } else {
        Ok(Box::new(MultiLogPlugin::new(plugins)))
    }
}

/// Thin wrapper for a single (name, cfg) pair; delegates to initialize_log_plugins.
pub fn initialize_log_plugin(name: &str, cfg: &LogPluginCfg) -> ConmonResult<Box<dyn LogPlugin>> {
    let entries = [(name.to_string(), cfg.clone())];
    initialize_log_plugins(&entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initialize_log_plugins_multiple_fans_out() -> ConmonResult<()> {
        let cfg = LogPluginCfg::default();
        let entries = vec![
            ("passthrough".to_string(), cfg.clone()),
            ("none".to_string(), cfg),
        ];
        let mut plugin = initialize_log_plugins(&entries)?;
        plugin.write(true, b"hello")?;
        plugin.write(false, b"world")?;
        plugin.reopen()?;
        Ok(())
    }
}
