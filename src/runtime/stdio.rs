use crate::{
    error::{ConmonError, ConmonResult},
    logging::plugin::LogPlugin,
};

use nix::{
    errno::Errno,
    fcntl::OFlag,
    poll::{PollFd, PollFlags, PollTimeout, poll},
    unistd::{pipe2, read},
};

use std::{
    cmp::Ordering,
    io,
    os::fd::{AsFd, AsRawFd, OwnedFd},
};

// Creates new pipe and return read/write fds.
pub fn create_pipe() -> ConmonResult<(OwnedFd, OwnedFd)> {
    let (rfd, wfd) = pipe2(OFlag::O_CLOEXEC).map_err(|e| {
        ConmonError::new(
            format!(
                "Failed to create pipe: {}",
                io::Error::from_raw_os_error(e as i32)
            ),
            1,
        )
    })?;

    // SAFETY: rfd/wfd are newly created FDs we now own.
    Ok((rfd, wfd))
}

// Reads data from fd and returns it.
pub fn read_pipe(fd: &OwnedFd) -> ConmonResult<String> {
    let mut buf = [0u8; 8192];

    let n = loop {
        match read(fd, &mut buf) {
            Ok(0) => return Ok(String::new()), // EOF immediately
            Ok(n) => break n,
            Err(Errno::EINTR) | Err(Errno::EAGAIN) => continue, // retry
            Err(_) => {
                return Err(ConmonError::new("read() failed while reading pipe", 1));
            }
        }
    };

    // Convert to UTF-8 safely
    match String::from_utf8(buf[..n].to_vec()) {
        Ok(s) => Ok(s),
        Err(_) => Err(ConmonError::new("pipe did not contain valid UTF-8", 1)),
    }
}

// Handles the writes to `mainfd_stdout` and `mainfd_stderr` by reading the data
// and forwarding it to log plugin.
pub fn handle_stdio(
    log_plugin: &dyn LogPlugin,
    mainfd_stdout: OwnedFd,
    mainfd_stderr: OwnedFd,
) -> ConmonResult<()> {
    let mut fds = [
        PollFd::new(mainfd_stdout.as_fd(), PollFlags::POLLIN),
        PollFd::new(mainfd_stderr.as_fd(), PollFlags::POLLIN),
    ];

    let mut buf = [0u8; 8192];

    loop {
        let n = poll(&mut fds, PollTimeout::NONE).map_err(|e| {
            ConmonError::new(
                format!(
                    "handle_stdio poll() failed: {}",
                    io::Error::from_raw_os_error(e as i32)
                ),
                1,
            )
        })?;

        if n == 0 {
            // Timeout. It should not happen, but be defensive.
            continue;
        }

        for pfd in &fds {
            if let Some(revents) = pfd.revents() {
                if revents.contains(PollFlags::POLLIN) {
                    match read(pfd.as_fd(), &mut buf) {
                        Ok(bytes) => match bytes.cmp(&0) {
                            Ordering::Greater => {
                                let s = String::from_utf8_lossy(&buf[..bytes]);
                                let is_stdout =
                                    pfd.as_fd().as_raw_fd() == mainfd_stdout.as_raw_fd();
                                let _ = log_plugin.write(is_stdout, &s);
                            }
                            Ordering::Equal => {
                                // EOF
                                return Ok(());
                            }
                            Ordering::Less => unreachable!(), // read() never returns negative sizes
                        },
                        Err(err) => {
                            if err == Errno::EWOULDBLOCK || err == Errno::EAGAIN {
                                break;
                            }
                            return Err(ConmonError::new(
                                format!(
                                    "handle_stdio read() failed: {}",
                                    io::Error::from_raw_os_error(err as i32)
                                ),
                                1,
                            ));
                        }
                    }
                } else if revents.contains(PollFlags::POLLHUP) {
                    return Ok(());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mockall::{
        mock,
        predicate::{self, *},
    };
    use nix::unistd::write as nix_write;

    mock! {
        pub LogPlugin {}
        impl crate::logging::plugin::LogPlugin for LogPlugin {
            fn write(&self, is_stdout: bool, data: &str) -> ConmonResult<()>;
        }
    }

    #[test]
    fn handle_stdio_calls_write_for_stdout_and_stderr() -> ConmonResult<()> {
        let mut mock = MockLogPlugin::new();
        mock.expect_write()
            .with(eq(true), predicate::str::contains("OUT"))
            .returning(|_, _| Ok(()));
        mock.expect_write()
            .with(eq(false), predicate::str::contains("ERR"))
            .returning(|_, _| Ok(()));

        let (r_out, w_out) = create_pipe()?;
        let (r_err, w_err) = create_pipe()?;
        nix_write(w_out.as_fd(), b"OUT\n").expect("write failed");
        nix_write(w_err.as_fd(), b"ERR\n").expect("write failed");
        drop(w_out);
        drop(w_err);

        // Read from the other side of pipes.
        handle_stdio(&mock, r_out, r_err)?;
        Ok(())
    }

    #[test]
    fn handle_stdio_write_error_ignored() -> ConmonResult<()> {
        let mut mock = MockLogPlugin::new();
        mock.expect_write()
            .with(eq(true), predicate::str::contains("OUT1"))
            .returning(|_, _| Ok(()));
        mock.expect_write()
            .with(eq(false), predicate::str::contains("ERR"))
            .returning(|_, _| Err(ConmonError::new("err write fail", 1)));
        mock.expect_write()
            .with(eq(true), predicate::str::contains("OUT2"))
            .returning(|_, _| Ok(()));

        let (r_out, w_out) = create_pipe()?;
        let (r_err, w_err) = create_pipe()?;
        nix_write(w_out.as_fd(), b"OUT1\n").expect("write failed");
        nix_write(w_err.as_fd(), b"ERR\n").expect("write failed");
        nix_write(w_out.as_fd(), b"OUT2\n").expect("write failed");
        drop(w_out);
        drop(w_err);

        // Read from the other side of pipes.
        handle_stdio(&mock, r_out, r_err)?;
        Ok(())
    }
}
