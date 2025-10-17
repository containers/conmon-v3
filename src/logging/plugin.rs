use crate::{cli::Opts, error::ConmonResult, logging::none_logger::NoneLogger};

pub trait LogPlugin {
    fn write(&self, is_stdout: bool, data: &str) -> ConmonResult<()>;
}

pub fn initialize_log_plugin(name: &str, opts: &Opts) -> Option<Box<dyn LogPlugin>> {
    match name {
        "none" => Some(Box::new(NoneLogger::new(opts))),
        _ => None,
    }
}
