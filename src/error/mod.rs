use std::fmt;

use nix::errno::Errno;

pub type ConmonResult<T> = Result<T, ConmonError>;

#[derive(Debug)]
pub struct ConmonError {
    pub msg: String,
    pub code: u8,
}

impl ConmonError {
    pub fn new<M: Into<String>>(m: M, code: u8) -> Self {
        Self {
            msg: m.into(),
            code,
        }
    }
}

impl fmt::Display for ConmonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} (code {})", self.msg, self.code)
    }
}

impl From<std::io::Error> for ConmonError {
    fn from(err: std::io::Error) -> Self {
        ConmonError::new(format!("IO error: {}", err), 1)
    }
}

impl From<std::ffi::NulError> for ConmonError {
    fn from(err: std::ffi::NulError) -> Self {
        ConmonError::new(format!("CString error: {}", err), 1)
    }
}

impl From<Errno> for ConmonError {
    fn from(err: Errno) -> Self {
        ConmonError::new(format!("Errno error: {}", err), 1)
    }
}
