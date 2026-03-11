use std::os::fd::{AsRawFd, OwnedFd};
use std::path::PathBuf;

use log::{debug, warn};
use nix::errno::Errno;
use nix::fcntl::{OFlag, open};
use nix::libc;
use nix::sys::stat::Mode;
use nix::unistd::{mkfifo, unlink};

use crate::cli::CommonCfg;
use crate::error::{ConmonError, ConmonResult};
use crate::logging::plugin::LogPlugin;
use crate::unix_socket::{RemoteSocket, SocketType};

/// Constants for `ctl` commands.
const WIN_RESIZE_EVENT: i32 = 1;
const REOPEN_LOGS_EVENT: i32 = 2;

/// Parses "height width\n" in `line` and resizes the pty defined by `stdout_fd`.
pub fn process_winsz_ctrl_line(stdout_fd: i32, line: &str) -> ConmonResult<()> {
    let parts: Vec<_> = line.split_whitespace().collect();
    if parts.len() != 2 {
        return Err(ConmonError::new("Failed to parse window size", 1));
    }

    let height: i32 = match parts[0].parse() {
        Ok(h) => h,
        Err(_) => {
            return Err(ConmonError::new("Failed to parse window size (height)", 1));
        }
    };

    let width: i32 = match parts[1].parse() {
        Ok(w) => w,
        Err(_) => {
            return Err(ConmonError::new("Failed to parse window size (width)", 1));
        }
    };

    debug!("Height: {height}, Width: {width}");

    if height < 0 || width < 0 || height > 1000 || width > 1000 {
        return Err(ConmonError::new(
            format!(
                "Invalid window size: {}x{} (must be between 0 and 1000)",
                height, width
            ),
            1,
        ));
    }

    unsafe {
        resize_winsz(stdout_fd, height as u16, width as u16);
    }

    Ok(())
}

/// Parses "msg_type height width\n" in `line` and acts.
pub fn process_terminal_ctrl_line(
    log_plugin: &mut dyn LogPlugin,
    stdout_fd: i32,
    line: &str,
) -> ConmonResult<()> {
    let parts: Vec<_> = line.split_whitespace().collect();
    if parts.len() < 3 {
        return Err(ConmonError::new("Invalid control message format", 1));
    }

    let ctl_msg_type: i32 = match parts[0].parse() {
        Ok(t) => t,
        Err(_) => {
            return Err(ConmonError::new("Invalid control message type", 1));
        }
    };

    let height: i32 = match parts[1].parse() {
        Ok(h) => h,
        Err(_) => {
            return Err(ConmonError::new("Invalid window size (height)", 1));
        }
    };

    let width: i32 = match parts[2].parse() {
        Ok(w) => w,
        Err(_) => {
            return Err(ConmonError::new("Invalid window size (height)", 1));
        }
    };

    if ctl_msg_type != WIN_RESIZE_EVENT && ctl_msg_type != REOPEN_LOGS_EVENT {
        return Err(ConmonError::new(
            format!("Invalid control message type: {ctl_msg_type}"),
            1,
        ));
    }

    if height < 0 || width < 0 || height > 1000 || width > 1000 {
        return Err(ConmonError::new(
            format!(
                "Invalid window size: {}x{} (must be between 0 and 1000)",
                height, width
            ),
            1,
        ));
    }

    debug!("Message type: {ctl_msg_type}");

    match ctl_msg_type {
        WIN_RESIZE_EVENT => {
            let hw_str = format!("{height} {width}\n");
            debug!("resize str: {hw_str}");
            process_winsz_ctrl_line(stdout_fd, &hw_str)?;
        }
        REOPEN_LOGS_EVENT => {
            log_plugin.reopen()?;
        }
        _ => {
            return Err(ConmonError::new(
                format!("Unknown message type: {ctl_msg_type}"),
                1,
            ));
        }
    }

    Ok(())
}

/// Resizes the pty window size using ioctl(TIOCSWINSZ).
unsafe fn resize_winsz(stdout_fd: i32, height: u16, width: u16) {
    let ws = libc::winsize {
        ws_row: height,
        ws_col: width,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };

    let ret = unsafe { libc::ioctl(stdout_fd, libc::TIOCSWINSZ, &ws) };
    if ret == -1 {
        warn!(
            "Failed to set process pty terminal size: {width}x{height}, {:?}",
            Errno::last()
        );
    }
}

// Keep the DUMMY_WRITE_FD around, otherwise we we will receive the flood of
// POLLHUP in the `handle_stdio`.
pub static mut DUMMY_WINSZ_WRITE_FD: Option<OwnedFd> = None;

/// Creates new console fifo ("winsz").
pub fn setup_console_fifo(cfg: &CommonCfg) -> ConmonResult<RemoteSocket> {
    let basename = PathBuf::from("winsz");
    let (r, dummy) = setup_fifo(cfg, &basename, "window resize control fifo")?;
    unsafe { DUMMY_WINSZ_WRITE_FD = Some(dummy) };
    debug!("winsz read size: {}", r.as_raw_fd());
    Ok(RemoteSocket::new(SocketType::ConsoleFifo, r))
}

// Keep the DUMMY_WRITE_FD around, otherwise we we will receive the flood of
// POLLHUP in the `handle_stdio`.
pub static mut DUMMY_CTL_WRITE_FD: Option<OwnedFd> = None;

/// Creates new terminal contro fifo ("ctl").
pub fn setup_terminal_control_fifo(cfg: &CommonCfg) -> ConmonResult<RemoteSocket> {
    let basename = PathBuf::from("ctl");
    let (ctl_fd_r, dummy) = setup_fifo(cfg, &basename, "terminal control fifo")?;
    unsafe { DUMMY_CTL_WRITE_FD = Some(dummy) };
    debug!("ctl fd: {}", ctl_fd_r.as_raw_fd());

    Ok(RemoteSocket::new(SocketType::TerminalFifo, ctl_fd_r))
}

/// Helper function to create a fifo.
fn setup_fifo(
    cfg: &CommonCfg,
    filename: &PathBuf,
    error_var_name: &str,
) -> ConmonResult<(OwnedFd, OwnedFd)> {
    let fifo_path = cfg.bundle.join(filename);
    let mode = Mode::from_bits_truncate(0o660);

    if let Err(e) = mkfifo(&fifo_path, mode) {
        if e == Errno::EEXIST {
            if let Err(e2) = unlink(&fifo_path) {
                return Err(ConmonError::new(
                    format!("Failed to unlink existing fifo {:?}: {e2}", fifo_path),
                    1,
                ));
            }
            if let Err(e3) = mkfifo(&fifo_path, mode) {
                return Err(ConmonError::new(
                    format!("Failed to mkfifo at {:?}: {e3}", fifo_path),
                    1,
                ));
            }
        } else {
            return Err(ConmonError::new(
                format!("Failed to mkfifo at {:?}: {e}", fifo_path),
                1,
            ));
        }
    }

    let fifo_r = match open(
        &fifo_path,
        OFlag::O_RDONLY | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(e) => {
            return Err(ConmonError::new(
                format!("Failed to open {} read half: {e}", error_var_name),
                1,
            ));
        }
    };

    let fifo_w = match open(
        &fifo_path,
        OFlag::O_WRONLY | OFlag::O_CLOEXEC,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(e) => {
            return Err(ConmonError::new(
                format!("Failed to open {} write half: {e}", error_var_name),
                1,
            ));
        }
    };
    Ok((fifo_r, fifo_w))
}
