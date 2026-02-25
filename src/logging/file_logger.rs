use chrono::{Datelike, Local, Timelike};

use crate::{
    error::{ConmonError, ConmonResult},
    logging::plugin::{LogPlugin, LogPluginCfg},
};
use std::{
    cmp::min, fs::{File, OpenOptions}, io::Write, path::PathBuf
};
use std::fs::rename;


const TSBUFLEN: usize = 44;

/// A simple file-based logging plugin.
///
/// Writes all log data to the configured file path.
pub struct FileLogger {
    file: File,
    stdout_has_partial: bool,
    stderr_has_partial: bool,
    no_sync: bool,
    max_size: u64,
    global_max_size: u64,
    bytes_written: u64,
    total_bytes_written: u64,
    path: PathBuf,
}

impl FileLogger {
    pub fn new(cfg: &LogPluginCfg) -> ConmonResult<Self> {
        if !cfg.log_labels.is_empty() {
            return Err(ConmonError::new("k8s-file doesn't support --log-label", 1));
        }
        if cfg.log_tag.is_some() {
            return Err(ConmonError::new("k8s-file doesn't support --log-tag", 1));
        }

        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&cfg.path)
            .map_err(|e| {
                ConmonError::new(
                    format!("Failed to open log file {}: {}", cfg.path.display(), e),
                    1,
                )
            })?;
        let metadata = file.metadata()?;

        Ok(Self {
            file,
            stdout_has_partial: false,
            stderr_has_partial: false,
            no_sync: cfg.no_sync,
            max_size: cfg.max_size as u64,
            global_max_size: cfg.global_max_size as u64,
            bytes_written: metadata.len(),
            total_bytes_written: metadata.len(),
            path: cfg.path.clone(),
        })
    }

    fn get_line_len(line_len: &mut isize, buf: &[u8], buflen: isize) -> bool {
        let mut partial = false;
        let len = buflen as usize;

        if let Some(pos) = buf[..len].iter().position(|&c| c == b'\n') {
            *line_len = (pos + 1) as isize;
        } else {
            *line_len = len as isize;
            partial = true;
        }
        partial
    }

    fn set_k8s_timestamp(buf: &mut [u8], pipename: &str) {
        let now = Local::now();
        let offset = now.offset().local_minus_utc();
        let off_sign = if offset < 0 { '-' } else { '+' };
        let off_abs = offset.abs();
        let hours = off_abs / 3600;
        let mins = (off_abs % 3600) / 60;

        // "YYYY-MM-DDTHH:MM:SS.NNNNNNNNN+01:00 stdout "
        let nsec = now.timestamp_subsec_nanos();
        let s = format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:09}{}{:02}:{:02} {} ",
            now.year(),
            now.month(),
            now.day(),
            now.hour(),
            now.minute(),
            now.second(),
            nsec,
            off_sign,
            hours,
            mins,
            pipename
        );

        let bytes = s.as_bytes();
        let n = min(buf.len() - 1, bytes.len());
        buf[..n].copy_from_slice(&bytes[..n]);
        if !buf.is_empty() {
            buf[buf.len() - 1] = 0;
        }
    }

    fn reopen(path: &PathBuf) -> ConmonResult<File> {
        // "<path>.tmp".
        let tmp_path: PathBuf = {
            let mut s = path.clone();
            s.push(".tmp");
            PathBuf::from(s)
        };

        // Open temp file.
        let new_file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&tmp_path)
            .map_err(|e| {
                ConmonError::new(
                    format!("Failed to open log file {:?}: {}", tmp_path, e),
                    1,
                )
            })?;

        // Replace the previous file
        rename(tmp_path, path)?;
        Ok(new_file)
    }

}

impl Drop for FileLogger {
    fn drop(&mut self) {
        if !self.no_sync {
            let _ = self.file.sync_all();
        }
    }
}

impl LogPlugin for FileLogger {
    fn write(&mut self, is_stdout: bool, data: &[u8]) -> ConmonResult<()> {
        // Track if we previously wrote a partial line for each stream.
        let has_partial = if is_stdout {
            &mut self.stdout_has_partial
        } else {
            &mut self.stderr_has_partial
        };

        let pipename = if is_stdout { "stdout" } else { "stderr" };

        let mut buf = data;
        let mut buflen = data.len() as isize;

        // Helper to map I/O errors into ConmonError.
        let map_err = |e: std::io::Error, msg: &str| ConmonError::new(format!("{msg}: {e}"), 1);

        // If we get an empty buffer and we had a partial line before, emit a
        // terminating "F" record (end of stream).
        if buflen == 0 && *has_partial {
            let mut tsbuf = [0u8; TSBUFLEN];
            Self::set_k8s_timestamp(&mut tsbuf, pipename);

            let ts_len = tsbuf.iter().position(|&b| b == 0).unwrap_or(tsbuf.len());

            self.file
                .write_all(&tsbuf[..ts_len])
                .map_err(|e| map_err(e, "failed to write timestamp"))?;
            self.file
                .write_all(b"F\n")
                .map_err(|e| map_err(e, "failed to write terminating F-sequence"))?;
            self.file
                .flush()
                .map_err(|e| map_err(e, "failed to flush log file"))?;

            *has_partial = false;
            return Ok(());
        }

        // Normal case: we have data to process
        while buflen > 0 {
            let mut line_len: isize = 0;
            let partial = Self::get_line_len(&mut line_len, buf, buflen);

            let mut tsbuf = [0u8; TSBUFLEN];
            Self::set_k8s_timestamp(&mut tsbuf, pipename);

            let ts_len = tsbuf.iter().position(|&b| b == 0).unwrap_or(tsbuf.len());

            // The log message lenght + 2 for "P " or "F "
            let mut bytes_to_be_written: u64 = ts_len as u64 + 2;
            if partial {
                bytes_to_be_written += 1; // The extra "\n" in the end we add later.
            }

            // If the caller specified a global max, enforce it before writing.
            if self.global_max_size > 0 && self.total_bytes_written + bytes_to_be_written >= self.global_max_size {
                break;
            }
            if self.max_size > 0 && self.bytes_written + bytes_to_be_written >= self.max_size {
                self.file = FileLogger::reopen(&self.path)?;
                self.bytes_written = 0;
            }

            // timestamp + stream
            self.file
                .write_all(&tsbuf[..ts_len])
                .map_err(|e| map_err(e, "failed to write timestamp"))?;

            // partial ("P ") vs full ("F ") marker
            if partial {
                self.file
                    .write_all(b"P ")
                    .map_err(|e| map_err(e, "failed to write partial log tag"))?;
            } else {
                self.file
                    .write_all(b"F ")
                    .map_err(|e| map_err(e, "failed to write end log tag"))?;
            }

            // actual log bytes
            let line_slice_len = line_len as usize;
            self.file
                .write_all(&buf[..line_slice_len])
                .map_err(|e| map_err(e, "failed to write log line"))?;

            // If there was no newline in this chunk, we add one (matching original logic)
            if partial {
                self.file
                    .write_all(b"\n")
                    .map_err(|e| map_err(e, "failed to write newline for partial log"))?;
            }

            *has_partial = partial;

            // Advance buffer ("goto_next!")
            buf = &buf[line_slice_len..];
            buflen -= line_len;
        }

        self.file
            .flush()
            .map_err(|e| map_err(e, "failed to flush log file"))?;

        Ok(())
    }
}
