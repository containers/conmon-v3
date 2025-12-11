use crate::{
    error::{ConmonError, ConmonResult},
    logging::plugin::{LogPlugin, LogPluginCfg},
};
use systemd::journal;

const STDIO_BUF_SIZE: usize = 8192;

/// Logging plugin for journald.
pub struct JournaldLogger {
    /// Buffer for partial (not ending with new-lline) stdout log messages.
    stdout_buf: [u8; STDIO_BUF_SIZE],

    /// Pointer to end of stdout buffer.
    stdout_buf_len: usize,

    /// Buffer for partial (not ending with new-lline) stderr log messages.
    stderr_buf: [u8; STDIO_BUF_SIZE],

    /// Pointer to end of stderr buffer.
    stderr_buf_len: usize,

    // Log plugin configuration.
    cfg: LogPluginCfg,
}

/// Helper function to return the number of occurence of `ch` in `str`.
fn count_chars_in_string(s: &str, ch: char) -> usize {
    s.chars().filter(|&c| c == ch).count()
}

/// Helper function to validate the label name.
fn is_valid_label_name(s: &str) -> bool {
    let chars = s.chars().peekable();
    for c in chars {
        if c == '=' {
            return true;
        }
        if !c.is_ascii_uppercase() && !c.is_ascii_digit() && c != '_' {
            return false;
        }
    }
    true
}

impl JournaldLogger {
    pub fn new(cfg: &LogPluginCfg) -> ConmonResult<Self> {
        // Validate the labels.
        for l in &cfg.log_labels {
            if l.starts_with('=') {
                return Err(ConmonError::new(
                    format!(
                        "Container labels must be in format LABEL=VALUE (no LABEL present in '{}')",
                        l
                    ),
                    1,
                ));
            }
            if count_chars_in_string(l, '=') != 1 {
                return Err(ConmonError::new(
                    format!(
                        "Container labels must be in format LABEL=VALUE (none or more than one '=' present in '{}')",
                        l
                    ),
                    1,
                ));
            }
            if !is_valid_label_name(l) {
                return Err(ConmonError::new(
                    format!(
                        "Container label names must contain only uppercase letters, numbers and underscore (in '{}')",
                        l
                    ),
                    1,
                ));
            }
        }

        Ok(Self {
            stdout_buf: [0; STDIO_BUF_SIZE],
            stdout_buf_len: 0,
            stderr_buf: [0; STDIO_BUF_SIZE],
            stderr_buf_len: 0,
            cfg: cfg.clone(),
        })
    }

    /// Parses the journald log message priority from the log message.
    fn parse_priority_prefix(buf: &[u8], priority: &mut i32, message_start: &mut usize) -> i32 {
        if buf.len() < 3 {
            return 0;
        }
        if buf[0] != b'<' {
            return 0;
        }
        if buf[1] < b'0' || buf[1] > b'7' {
            return 0;
        }
        if buf[2] != b'>' {
            return 0;
        }
        *priority = (buf[1] - b'0') as i32;
        *message_start = 3;
        1
    }

    /// Returns the line length and whether the line is partial or not.
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

    /// Truncates the CUUID to 12 characters.
    fn truncate_cuuid(s: &str) -> &str {
        if s.len() <= 12 {
            return s;
        }

        match s.char_indices().nth(12) {
            Some((idx, _)) => &s[..idx],
            None => s, // fewer than 12 chars
        }
    }
}

impl LogPlugin for JournaldLogger {
    fn write(&mut self, is_stdout: bool, data: &[u8]) -> ConmonResult<()> {
        // Select the right partial buffer.
        let (partial_buf, partial_buf_len) = if is_stdout {
            (&mut self.stdout_buf[..], &mut self.stdout_buf_len)
        } else {
            (&mut self.stderr_buf[..], &mut self.stderr_buf_len)
        };

        // Set the default priority according to stdout/stderr.
        let default_priority = if is_stdout { 6 } else { 3 };

        let mut buf = data;
        let mut buflen = buf.len() as isize;

        while buflen > 0 || *partial_buf_len > 0 {
            let mut line_len: isize = 0;

            // determine whether the current line is partial (no '\n' seen)
            let partial = buflen == 0 || Self::get_line_len(&mut line_len, buf, buflen);

            // If we still have input data, and the line is partial, try buffering it.
            if buflen > 0 && partial {
                let needed = line_len as usize;
                if *partial_buf_len + needed < STDIO_BUF_SIZE {
                    partial_buf[*partial_buf_len..*partial_buf_len + needed]
                        .copy_from_slice(&buf[..needed]);
                    *partial_buf_len += needed;

                    // nothing to send yet, wait for the next write() call
                    return Ok(());
                }
            }

            // Priority parsing
            let mut parsed_priority = default_priority;
            let mut message_start_idx = 0usize;
            let mut actual_message_len = line_len;

            if *partial_buf_len == 0 && line_len > 0 && buflen > 0 {
                let to_parse = &buf[..line_len as usize];
                let r = Self::parse_priority_prefix(
                    to_parse,
                    &mut parsed_priority,
                    &mut message_start_idx,
                );
                if r == 1 {
                    actual_message_len = line_len - message_start_idx as isize;
                } else {
                    message_start_idx = 0;
                }
            }

            let mut fields: Vec<String> = Vec::new();

            // MESSAGE=...
            let mut message: Vec<u8> = vec![];
            message.extend_from_slice(b"MESSAGE=");
            message.extend_from_slice(&partial_buf[..*partial_buf_len]);
            if buflen > 0 {
                message.extend_from_slice(
                    &buf[message_start_idx..message_start_idx + actual_message_len as usize],
                );
            }
            fields.push(String::from_utf8_lossy(&message).into_owned());

            // BPRIORITY=...
            let priority_txt = format!("PRIORITY={}", parsed_priority);
            fields.push(priority_txt);

            // Other fields.
            if let Some(cid) = &self.cfg.cid {
                fields.push(format!("CONTAINER_ID={}", cid));
            }

            if let Some(cuuid) = &self.cfg.cuuid {
                fields.push(format!("CONTAINER_ID_FULL={}", cuuid));
            }

            if let Some(tag) = &self.cfg.log_tag {
                fields.push(format!("CONTAINER_TAG={}", tag));
            }

            if let Some(name) = &self.cfg.name {
                fields.push(format!("CONTAINER_NAME={}", name));
            }

            if let Some(cuuid) = &self.cfg.cuuid {
                fields.push(format!("SYSLOG_IDENTIFIER={}", Self::truncate_cuuid(cuuid)));
            }

            if partial && !self.cfg.no_container_partial_message {
                fields.push("CONTAINER_PARTIAL_MESSAGE=true".to_string());
            }

            for label in &self.cfg.log_labels {
                // label is something like "foo=bar"
                fields.push(label.clone());
            }

            // journal::send(&[&str]) wants &str slices, so we build a view
            let field_slices: Vec<&str> = fields.iter().map(|s| s.as_str()).collect();

            let rc = journal::send(&field_slices);
            if rc < 0 {
                return Err(ConmonError::new(
                    format!("Error calling journal::send: {}", rc),
                    1,
                ));
            }

            // Advance in the input buffer and reset partial buffer
            if buflen > 0 {
                buf = &buf[line_len as usize..];
                buflen -= line_len;
            }
            *partial_buf_len = 0;
        }

        Ok(())
    }
}
