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
