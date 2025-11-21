use crate::error::{ConmonError, ConmonResult};
use nix::errno::Errno;
use nix::fcntl::{FcntlArg, FdFlag, fcntl};
use nix::unistd::write;
use serde_json::{Map, Value};
use std::env;
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::io::RawFd;

/// Abstraction over environment access (so tests can mock it).
pub trait Env {
    fn var(&self, key: &str) -> Result<String, env::VarError>;
}

pub struct RealEnv;
impl Env for RealEnv {
    fn var(&self, key: &str) -> Result<String, env::VarError> {
        env::var(key)
    }
}

/// Return the pipe FD from an env var, or None if the env var is not set.
pub fn get_pipe_fd_from_env(envname: &str) -> ConmonResult<Option<OwnedFd>> {
    get_pipe_fd_from_env_with(&RealEnv, envname)
}

/// Return the pipe FD from an env var, or None if the env var is not set.
/// On parse/fcntl failure, returns an error. Uses an injected `Env`.
pub fn get_pipe_fd_from_env_with<E: Env>(e: &E, envname: &str) -> ConmonResult<Option<OwnedFd>> {
    let pipe_str = match e.var(envname) {
        Ok(s) => s,
        Err(env::VarError::NotPresent) => return Ok(None),
        Err(_) => {
            return Err(ConmonError::new(format!("unable to parse {}", envname), 1));
        }
    };

    let fd: RawFd = pipe_str.parse::<i32>().map_err(|_| {
        ConmonError::new(
            format!("unable to parse {} : {} not an integer", envname, pipe_str),
            1,
        )
    })?;

    let ofd = unsafe { OwnedFd::from_raw_fd(fd) };

    fcntl(&ofd, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC))
        .map_err(|_| ConmonError::new(format!("unable to make {} CLOEXEC", envname), 1))?;

    Ok(Some(ofd))
}

/// Write all bytes to a RawFd using nix::unistd::write, retrying on EINTR/partial writes.
fn write_all_fd(fd: &OwnedFd, mut buf: &[u8]) -> nix::Result<()> {
    while !buf.is_empty() {
        match write(fd, buf) {
            Ok(0) => {
                // Should not happen for pipes, treat as error like short write
                return Err(Errno::EIO);
            }
            Ok(n) => buf = &buf[n..],
            Err(Errno::EINTR) => {
                // retry
                continue;
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Write a message to the sync pipe it it's not a broken pipe.
/// The fd is closed after this.
pub fn write_or_close_sync_fd(
    fd: OwnedFd,
    int_data: i32,
    str_data: Option<&str>,
    opt_api_version: i32,
    opt_exec: bool,
) -> ConmonResult<Option<OwnedFd>> {
    let data_key = if opt_api_version >= 1 {
        "data"
    } else if opt_exec {
        "exit_code"
    } else {
        "pid"
    };

    // Build JSON.
    let mut obj = Map::with_capacity(2);
    obj.insert(data_key.to_string(), Value::from(int_data));
    // Clippy complains about collapsible_if, but it cannot be collapsed in the older
    // rust versions: https://github.com/rust-lang/rust/issues/53667.
    #[allow(clippy::collapsible_if)]
    if let Some(msg) = str_data {
        if !msg.is_empty() {
            obj.insert("message".to_string(), Value::from(msg));
        }
    }
    let mut json = Value::Object(obj).to_string();
    json.push('\n');

    // Write all; on EPIPE just return Ok(()), OwnedFd will close on drop.
    match write_all_fd(&fd, json.as_bytes()) {
        Ok(_) => Ok(Some(fd)),
        Err(Errno::EPIPE) => Ok(None),
        Err(_) => Err(ConmonError::new(
            "Unable to send container stderr message to parent",
            1,
        )),
    }
}

#[cfg(test)]
mod tests {
    use crate::runtime::stdio::{create_pipe, read_pipe};

    use super::*;
    use mockall::{mock, predicate::eq};
    use nix::fcntl::{FcntlArg, FdFlag, fcntl};
    use serde_json::Value;
    use std::os::{fd::IntoRawFd, unix::io::AsRawFd};

    mock! {
        pub FakeEnv {}
        impl Env for FakeEnv {
            fn var(&self, key: &str) -> Result<String, env::VarError>;
        }
    }

    #[test]
    fn env_missing_returns_none() {
        let key = "X_TEST_NONE";

        let mut mock = MockFakeEnv::new();
        mock.expect_var()
            .with(eq(key))
            .returning(|_| Err(env::VarError::NotPresent));

        let res = get_pipe_fd_from_env_with(&mock, key).unwrap();
        assert!(res.is_none());
    }

    #[test]
    fn env_non_integer_returns_err() {
        let key = "X_TEST_BAD";

        let mut mock = MockFakeEnv::new();
        mock.expect_var()
            .with(eq(key))
            .returning(|_| Ok("not-an-int".to_string()));

        let err = get_pipe_fd_from_env_with(&mock, key).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("not an integer"), "unexpected error: {msg}");
    }

    #[test]
    fn env_ok_sets_cloexec_and_returns_ownedfd() -> ConmonResult<()> {
        let key = "X_TEST_OK";
        let (_r, w) = create_pipe()?;
        let fd = w.into_raw_fd();
        let fd_str = fd.to_string();

        let mut mock = MockFakeEnv::new();
        mock.expect_var()
            .with(eq(key))
            .returning(move |_| Ok(fd_str.clone()));

        let ofd = get_pipe_fd_from_env_with(&mock, key)?.unwrap();

        // Same fd number, CLOEXEC set
        assert_eq!(ofd.as_raw_fd(), fd);
        let flags = fcntl(&ofd, FcntlArg::F_GETFD)?;
        let flags = FdFlag::from_bits_truncate(flags);
        assert!(flags.contains(FdFlag::FD_CLOEXEC));
        Ok(())
    }

    #[test]
    fn write_writes_exit_code_and_message() -> ConmonResult<()> {
        let (r, w) = create_pipe()?;
        write_or_close_sync_fd(w, 7, Some("ok"), 0, true)?;
        let mut buf = [0u8; 8192];
        let n = read_pipe(&r, &mut buf)?;
        drop(r);
        let output = std::str::from_utf8(&buf[..n])?;
        let v: Value = serde_json::from_str(output)?;
        assert_eq!(v.get("exit_code").unwrap(), 7);
        assert_eq!(v.get("message").unwrap(), "ok");
        Ok(())
    }

    #[test]
    fn write_writes_pid_without_message() -> ConmonResult<()> {
        let (r, w) = create_pipe()?;
        write_or_close_sync_fd(w, 123, None, 0, false)?;
        let mut buf = [0u8; 8192];
        let n = read_pipe(&r, &mut buf)?;
        drop(r);
        let output = std::str::from_utf8(&buf[..n])?;
        let v: Value = serde_json::from_str(output)?;
        assert_eq!(v.get("pid").unwrap(), 123);
        assert!(v.get("message").is_none());
        Ok(())
    }

    #[test]
    fn write_writes_data_for_api_v1() -> ConmonResult<()> {
        let (r, w) = create_pipe()?;
        write_or_close_sync_fd(w, 42, Some("hi"), 1, false)?;
        let mut buf = [0u8; 8192];
        let n = read_pipe(&r, &mut buf)?;
        drop(r);
        let output = std::str::from_utf8(&buf[..n])?;
        let v: Value = serde_json::from_str(output)?;
        assert_eq!(v.get("data").unwrap(), 42);
        assert_eq!(v.get("message").unwrap(), "hi");
        Ok(())
    }

    #[test]
    fn write_ok_on_epipe() -> ConmonResult<()> {
        let (r, w) = create_pipe()?;
        drop(r);
        let res = write_or_close_sync_fd(w, 99, Some("ignored"), 1, false);
        assert!(res.is_ok(), "expected Ok(()) on EPIPE");
        Ok(())
    }
}
