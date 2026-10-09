use std::path::PathBuf;

use crate::{
    error::{ConmonError, ConmonResult},
    logging::{
        file_logger::FileLogger, journald_logger::JournaldLogger, none_logger::NoneLogger,
        syslog_logger::SyslogLogger,
    },
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
    /// Per-file size limit in bytes. `None` means unlimited.
    pub max_size: Option<usize>,
    /// Aggregate size limit in bytes. `None` means unlimited.
    pub global_max_size: Option<usize>,
    pub max_files: i32,
    pub allowlist_dirs: Option<Vec<PathBuf>>,
    pub rotate: bool,
    /// Driver-specific options from repeated `--log-driver-opt KEY=VALUE`.
    pub log_driver_opts: Vec<(String, String)>,
}

/// Built-in log driver kinds. Parsing lives here so CLI discovery and logger
/// construction stay on the same match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogDriverKind {
    None,
    /// Discards output like [`Self::None`], but the container inherits stdio
    /// and conmon does not create an attach socket.
    Passthrough,
    File,
    Journald,
    Syslog,
}

/// Normalize a log driver name (`k8s-file` → `k8s_file`).
pub fn normalize_log_driver_name(name: &str) -> String {
    name.trim().replace('-', "_")
}

impl LogDriverKind {
    /// Parse a driver name. Dashes are normalized to underscores.
    ///
    /// `null` and `off` are not valid log driver names. The deprecated
    /// `--log-path` parser maps those legacy spellings to `none` first.
    pub fn parse(name: &str) -> ConmonResult<Self> {
        match normalize_log_driver_name(name).as_str() {
            "none" => Ok(Self::None),
            "passthrough" => Ok(Self::Passthrough),
            "file" | "k8s_file" => Ok(Self::File),
            "journald" => Ok(Self::Journald),
            "syslog" => Ok(Self::Syslog),
            other => Err(ConmonError::new(format!("No such log driver {other}"), 1)),
        }
    }

    /// Construct the logger for this kind.
    ///
    /// Construction can still fail (for example, a missing log file or a
    /// journald container id that is too short).
    fn open(self, cfg: &LogPluginCfg) -> ConmonResult<LogDriver> {
        match self {
            Self::None | Self::Passthrough => Ok(LogDriver::None(NoneLogger::new(cfg)?)),
            Self::File => Ok(LogDriver::File(Box::new(FileLogger::new(cfg)?))),
            Self::Journald => Ok(LogDriver::Journald(Box::new(JournaldLogger::new(cfg)?))),
            Self::Syslog => Ok(LogDriver::Syslog(Box::new(SyslogLogger::new(cfg)?))),
        }
    }
}

/// Dispatches container log writes to one built-in logger.
///
/// Journald and syslog loggers are boxed because each buffers full stdio
/// lines; keeping those buffers out of the enum avoids a large stack value.
pub enum LogDriver {
    None(NoneLogger),
    File(Box<FileLogger>),
    Journald(Box<JournaldLogger>),
    Syslog(Box<SyslogLogger>),
}

impl LogPlugin for LogDriver {
    fn write(&mut self, is_stdout: bool, data: &[u8]) -> ConmonResult<()> {
        match self {
            Self::None(logger) => logger.write(is_stdout, data),
            Self::File(logger) => logger.write(is_stdout, data),
            Self::Journald(logger) => logger.write(is_stdout, data),
            Self::Syslog(logger) => logger.write(is_stdout, data),
        }
    }

    fn reopen(&mut self) -> ConmonResult<()> {
        match self {
            Self::None(logger) => logger.reopen(),
            Self::File(logger) => logger.reopen(),
            Self::Journald(logger) => logger.reopen(),
            Self::Syslog(logger) => logger.reopen(),
        }
    }
}

/// Composite log plugin that fans out write() and reopen() to multiple plugins.
pub struct MultiLogPlugin {
    plugins: Vec<LogDriver>,
}

impl MultiLogPlugin {
    pub fn new(plugins: Vec<LogDriver>) -> Self {
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
    let mut plugins: Vec<LogDriver> = Vec::with_capacity(entries.len());
    for (name, cfg) in entries {
        plugins.push(LogDriverKind::parse(name)?.open(cfg)?);
    }
    if plugins.len() == 1 {
        Ok(Box::new(plugins.into_iter().next().unwrap()))
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
    fn known_drivers_parse() {
        for name in [
            "none",
            "passthrough",
            "file",
            "k8s_file",
            "k8s-file",
            "journald",
            "syslog",
        ] {
            assert!(LogDriverKind::parse(name).is_ok(), "{name} should parse");
        }
        assert!(LogDriverKind::parse("notadriver").is_err());
        assert!(LogDriverKind::parse("null").is_err());
        assert!(LogDriverKind::parse("off").is_err());
        assert_eq!(normalize_log_driver_name("k8s-file"), "k8s_file");
        assert_eq!(LogDriverKind::parse("file").unwrap(), LogDriverKind::File);
        assert_eq!(
            LogDriverKind::parse("k8s_file").unwrap(),
            LogDriverKind::File
        );
        assert_eq!(
            LogDriverKind::parse("journald").unwrap(),
            LogDriverKind::Journald
        );
    }

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

    #[test]
    fn initialize_log_plugin_rejects_legacy_null_and_off_aliases() {
        let cfg = LogPluginCfg::default();
        for name in ["null", "off"] {
            let Err(err) = initialize_log_plugin(name, &cfg) else {
                panic!("{name} must be rejected");
            };
            assert!(
                err.to_string().contains("No such log driver"),
                "{name}: {err}"
            );
        }
    }

    #[test]
    fn initialize_log_plugin_none_works() -> ConmonResult<()> {
        let cfg = LogPluginCfg::default();
        let mut plugin = initialize_log_plugin("none", &cfg)?;
        plugin.write(true, b"test")?;
        plugin.write(false, b"data")?;
        plugin.reopen()?;
        Ok(())
    }
}
