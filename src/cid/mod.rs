use crate::error::{ConmonError, ConmonResult};

use std::fmt;
use std::str::FromStr;

/// Validated container ID safe for use as a single path component.
///
/// Rejects empty strings, `.`, `..`, and values containing `/` or NUL, which would
/// allow escaping directories such as `exit_dir` or confuse path APIs. Other
/// characters (including `+`, `:`, and non-ASCII) are accepted so historically
/// valid container IDs keep working.
///
/// Construct only through [`Cid::parse`], [`FromStr`], or [`TryFrom<String>`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Cid(String);

impl Cid {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn parse(s: &str) -> ConmonResult<Self> {
        validate(s)?;
        Ok(Cid(s.to_string()))
    }
}

impl FromStr for Cid {
    type Err = ConmonError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl TryFrom<String> for Cid {
    type Error = ConmonError;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        validate(&s)?;
        Ok(Cid(s))
    }
}

impl fmt::Display for Cid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for Cid {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

fn validate(cid: &str) -> ConmonResult<()> {
    if cid.is_empty() || cid == "." || cid == ".." || cid.contains('/') || cid.contains('\0') {
        return Err(ConmonError::new(format!("Invalid container ID {cid:?}"), 1));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unsafe_ids() {
        for bad in ["", ".", "..", "foo/bar", "../outside", "foo\0bar", "\0"] {
            assert!(
                Cid::from_str(bad).is_err(),
                "expected {bad:?} to be rejected"
            );
        }
    }

    #[test]
    fn accepts_hex_and_extended_ids() {
        for good in [
            "abc123",
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "my-container_1.0",
            "sha256:deadbeef",
            "ctr+with+plus",
            "café",
            "容器",
            "test-cid",
        ] {
            assert!(
                Cid::from_str(good).is_ok(),
                "expected {good:?} to be accepted"
            );
        }
    }
}
