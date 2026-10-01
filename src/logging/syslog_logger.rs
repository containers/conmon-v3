use crate::{
    error::{ConmonError, ConmonResult},
    logging::plugin::{LogPlugin, LogPluginCfg},
};
use arrayvec::ArrayVec;
use nix::libc::{self, LOG_ERR, LOG_INFO, LOG_NDELAY, LOG_PID, LOG_USER};
use std::ffi::CString;
use std::sync::atomic::{AtomicBool, Ordering};

const STDIO_BUF_SIZE: usize = 8192;

/// `openlog`/`closelog` configure a process-global syslog destination; only one
/// active `SyslogLogger` is supported per process.
static SYSLOG_LOGGER_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Logging plugin that writes container stdout/stderr lines to syslog.
pub struct SyslogLogger {
    stdout: ArrayVec<u8, STDIO_BUF_SIZE>,
    stderr: ArrayVec<u8, STDIO_BUF_SIZE>,

    /// Ident string passed to `openlog`; must outlive syslog usage.
    /// Kept so the C pointer from `openlog` remains valid for the logger lifetime.
    #[allow(dead_code)]
    ident: CString,
}

impl SyslogLogger {
    pub fn new(cfg: &LogPluginCfg) -> ConmonResult<Self> {
        if !cfg.log_labels.is_empty() {
            return Err(ConmonError::new("syslog doesn't support --log-label", 1));
        }

        if SYSLOG_LOGGER_ACTIVE
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(ConmonError::new(
                "syslog log driver can only be used once (openlog is process-global)",
                1,
            ));
        }

        let ident = match Self::syslog_ident(cfg) {
            Ok(ident) => ident,
            Err(e) => {
                SYSLOG_LOGGER_ACTIVE.store(false, Ordering::Release);
                return Err(e);
            }
        };

        // SAFETY: `ident` remains owned by this struct for the logger lifetime.
        // LOG_PID includes the PID; LOG_NDELAY connects immediately.
        // openlog state is process-global; guarded by SYSLOG_LOGGER_ACTIVE.
        unsafe {
            libc::openlog(ident.as_ptr(), LOG_PID | LOG_NDELAY, LOG_USER);
        }

        Ok(Self {
            stdout: ArrayVec::new(),
            stderr: ArrayVec::new(),
            ident,
        })
    }

    /// Choose the syslog identity: `--log-tag`, container name, short cuuid, or `"conmon"`.
    fn syslog_ident(cfg: &LogPluginCfg) -> ConmonResult<CString> {
        let raw = if let Some(ref tag) = cfg.log_tag {
            Self::validate_syslog_ident(tag)?;
            tag.as_str()
        } else if let Some(ref name) = cfg.name {
            // Container names are caller-controlled; apply the same identity rules.
            Self::validate_syslog_ident(name)?;
            name.as_str()
        } else if let Some(ref cuuid) = cfg.cuuid {
            Self::truncate_cuuid(cuuid)
        } else {
            "conmon"
        };
        CString::new(raw).map_err(|_| {
            ConmonError::new(
                format!("syslog identity contains interior NUL byte: {raw:?}"),
                1,
            )
        })
    }

    /// Reject identities that are unsafe as syslog TAG / openlog ident.
    ///
    /// Disallows control characters. Empty tags and colons are allowed.
    fn validate_syslog_ident(ident: &str) -> ConmonResult<()> {
        if let Some(b) = ident.bytes().find(|&b| b < 0x20 || b == 0x7f) {
            return Err(ConmonError::new(
                format!(
                    "syslog identity contains invalid byte {b:#04x} (no control characters): {ident:?}"
                ),
                1,
            ));
        }
        Ok(())
    }

    /// Parses a leading `<N>` syslog/journal priority prefix (`N` in 0..=7).
    ///
    /// Returns `Some((priority, message_start))` when a complete prefix is present.
    fn parse_priority_prefix(buf: &[u8]) -> Option<(i32, usize)> {
        if buf.len() < 3 {
            return None;
        }
        if buf[0] != b'<' || buf[2] != b'>' || !(b'0'..=b'7').contains(&buf[1]) {
            return None;
        }
        Some(((buf[1] - b'0') as i32, 3))
    }

    /// Returns `(line_len, is_partial)` for the next line in `buf`.
    ///
    /// `line_len` includes the trailing newline when `is_partial` is false.
    fn get_line_len(buf: &[u8]) -> (usize, bool) {
        if let Some(pos) = buf.iter().position(|&c| c == b'\n') {
            (pos + 1, false)
        } else {
            (buf.len(), true)
        }
    }

    fn truncate_cuuid(s: &str) -> &str {
        if s.len() <= 12 {
            return s;
        }
        match s.char_indices().nth(12) {
            Some((idx, _)) => &s[..idx],
            None => s,
        }
    }

    /// Build a NUL-free C string from message bytes (trailing newline stripped).
    ///
    /// Interior NUL bytes are replaced with `b'?'` so the payload can be passed to
    /// the printf-style `syslog(3)` API without truncating at the first NUL.
    fn message_cstring(parts: &[&[u8]]) -> CString {
        let mut message = Vec::new();
        for part in parts {
            for &b in *part {
                message.push(if b == 0 { b'?' } else { b });
            }
        }
        while message.last() == Some(&b'\n') {
            message.pop();
        }
        // `message` has no NUL bytes, so this cannot fail.
        CString::new(message).unwrap_or_else(|_| CString::new("").unwrap())
    }

    /// Emit one assembled line: parse optional `<N>` priority, then syslog it.
    fn emit_assembled(assembled: &[u8], default_priority: i32) {
        let (priority, message_bytes) =
            if let Some((pri, start)) = Self::parse_priority_prefix(assembled) {
                (pri, &assembled[start..])
            } else {
                (default_priority, assembled)
            };
        let message = Self::message_cstring(&[message_bytes]);
        Self::emit(priority, &message);
    }

    fn emit(priority: i32, message: &CString) {
        #[cfg(test)]
        if test_capture::record(priority, message.as_bytes()) {
            return;
        }
        // SAFETY: format string is a literal; message is a valid C string.
        // Never pass user data as the format string (printf-style API).
        unsafe {
            libc::syslog(priority, c"%s".as_ptr(), message.as_ptr());
        }
    }
}

impl Drop for SyslogLogger {
    fn drop(&mut self) {
        // SAFETY: pairs with openlog in `new`.
        unsafe {
            libc::closelog();
        }
        SYSLOG_LOGGER_ACTIVE.store(false, Ordering::Release);
    }
}

impl LogPlugin for SyslogLogger {
    fn reopen(&mut self) -> ConmonResult<()> {
        Ok(())
    }

    fn write(&mut self, is_stdout: bool, data: &[u8]) -> ConmonResult<()> {
        let partial = if is_stdout {
            &mut self.stdout
        } else {
            &mut self.stderr
        };

        let default_priority = if is_stdout { LOG_INFO } else { LOG_ERR };

        let mut buf = data;

        while !buf.is_empty() || !partial.is_empty() {
            let (line_len, is_partial) = if buf.is_empty() {
                (0, true)
            } else {
                Self::get_line_len(buf)
            };

            // Buffer partial lines until we see a newline (or the buffer fills).
            if !buf.is_empty() && is_partial {
                match partial.try_extend_from_slice(&buf[..line_len]) {
                    Ok(()) if partial.len() < STDIO_BUF_SIZE => {
                        // Still below capacity: keep buffering.
                        return Ok(());
                    }
                    Ok(()) => {
                        // Exactly at capacity: emit the buffered bytes only.
                        // The just-appended input is already in `partial`, so do
                        // not concatenate `buf[..line_len]` again.
                        Self::emit_assembled(partial, default_priority);
                        partial.clear();
                        buf = &buf[line_len..];
                        continue;
                    }
                    Err(_) => {
                        // Would not fit: fall through and emit previous partial
                        // plus the current chunk together.
                    }
                }
            }

            // Assemble the complete line (partial buffer + current input) so a
            // `<N>` priority prefix split across writes is still recognized.
            let mut assembled = Vec::with_capacity(partial.len() + line_len);
            assembled.extend_from_slice(partial);
            if !buf.is_empty() {
                assembled.extend_from_slice(&buf[..line_len]);
            }

            Self::emit_assembled(&assembled, default_priority);

            if !buf.is_empty() {
                buf = &buf[line_len..];
            }
            partial.clear();
        }

        Ok(())
    }
}

/// Test-only seam that records `(priority, payload)` instead of calling syslog(3).
#[cfg(test)]
mod test_capture {
    use std::cell::RefCell;

    type Captured = Vec<(i32, Vec<u8>)>;

    thread_local! {
        static CAPTURE: RefCell<Option<Captured>> = const { RefCell::new(None) };
    }

    pub fn start() {
        CAPTURE.with(|c| *c.borrow_mut() = Some(Vec::new()));
    }

    pub fn take() -> Captured {
        CAPTURE.with(|c| c.borrow_mut().take().unwrap_or_default())
    }

    /// Returns true when a capture session is active (caller should skip syslog).
    pub fn record(priority: i32, payload: &[u8]) -> bool {
        CAPTURE.with(|c| {
            if let Some(ref mut v) = *c.borrow_mut() {
                v.push((priority, payload.to_vec()));
                true
            } else {
                false
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logging::plugin::initialize_log_plugin;
    use std::sync::{Mutex, MutexGuard};

    /// Serialize tests that construct a live `SyslogLogger` (process-global openlog).
    static TEST_SYSLOG_LOCK: Mutex<()> = Mutex::new(());

    fn lock_syslog() -> MutexGuard<'static, ()> {
        TEST_SYSLOG_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn cfg() -> LogPluginCfg {
        LogPluginCfg {
            cid: Some("0123456789abcdef".into()),
            cuuid: Some("0123456789abcdef0123456789abcdef".into()),
            name: Some("testctr".into()),
            ..Default::default()
        }
    }

    #[test]
    fn syslog_logger_new_rejects_labels() {
        let _guard = lock_syslog();
        let mut c = cfg();
        c.log_labels = vec!["FOO=bar".into()];
        match SyslogLogger::new(&c) {
            Ok(_) => panic!("labels must be rejected"),
            Err(err) => assert!(err.msg.contains("doesn't support --log-label")),
        }
    }

    #[test]
    fn syslog_logger_uses_log_tag_as_ident() {
        let _guard = lock_syslog();
        let mut c = cfg();
        c.log_tag = Some("mytag".into());
        let logger = SyslogLogger::new(&c).expect("create");
        assert_eq!(logger.ident.to_bytes(), b"mytag");
    }

    #[test]
    fn syslog_logger_preserves_log_tag_longer_than_48_chars() {
        let _guard = lock_syslog();
        let long_tag = "t".repeat(64);
        let mut c = cfg();
        c.log_tag = Some(long_tag.clone());
        let logger = SyslogLogger::new(&c).expect("long --log-tag must be accepted");
        assert_eq!(logger.ident.to_bytes(), long_tag.as_bytes());
    }

    #[test]
    fn syslog_logger_preserves_container_name_longer_than_48_chars() {
        let _guard = lock_syslog();
        let long_name = "n".repeat(80);
        let mut c = cfg();
        c.log_tag = None;
        c.name = Some(long_name.clone());
        let logger = SyslogLogger::new(&c).expect("long container name must be accepted");
        assert_eq!(logger.ident.to_bytes(), long_name.as_bytes());
    }

    #[test]
    fn syslog_logger_allows_colon_in_ident() {
        let _guard = lock_syslog();
        let mut c = cfg();
        c.log_tag = Some("app:component".into());
        let logger = SyslogLogger::new(&c).expect("colon in tag must be accepted");
        assert_eq!(logger.ident.to_bytes(), b"app:component");
    }

    #[test]
    fn syslog_logger_rejects_second_instance() {
        let _guard = lock_syslog();
        let first = SyslogLogger::new(&cfg()).expect("first syslog logger");
        let err = match SyslogLogger::new(&cfg()) {
            Ok(_) => panic!("second instance must fail"),
            Err(e) => e,
        };
        assert!(err.msg.contains("only be used once"));
        drop(first);
        // After drop, a new instance must succeed again.
        let _second = SyslogLogger::new(&cfg()).expect("logger after drop");
    }

    #[test]
    fn syslog_logger_write_and_reopen() -> ConmonResult<()> {
        let _guard = lock_syslog();
        let mut plugin = initialize_log_plugin("syslog", &cfg())?;
        plugin.write(true, b"hello\n")?;
        plugin.write(false, b"<3>error line\n")?;
        plugin.write(true, b"partial")?;
        plugin.write(true, b" line\n")?;
        // Priority prefix split across writes must still be parsed.
        plugin.write(false, b"<3")?;
        plugin.write(false, b">split prefix\n")?;
        plugin.reopen()?;
        // Drain any remaining partial buffers (mirrors main shutdown).
        plugin.write(true, b"")?;
        Ok(())
    }

    #[test]
    fn parse_priority_prefix_accepts_valid_priorities() {
        for p in b'0'..=b'7' {
            let line = [b'<', p, b'>', b'm'];
            let (pri, start) = SyslogLogger::parse_priority_prefix(&line).expect("valid prefix");
            assert_eq!(pri, (p - b'0') as i32);
            assert_eq!(start, 3);
        }
    }

    #[test]
    fn parse_priority_prefix_rejects_missing_or_out_of_range() {
        assert!(SyslogLogger::parse_priority_prefix(b"msg\n").is_none());
        assert!(SyslogLogger::parse_priority_prefix(b"<9>x").is_none());
        assert!(SyslogLogger::parse_priority_prefix(b"<").is_none());
        assert!(SyslogLogger::parse_priority_prefix(b"<3").is_none());
        assert!(SyslogLogger::parse_priority_prefix(b"<>x").is_none());
    }

    #[test]
    fn message_cstring_replaces_interior_nul_with_question_mark() {
        let cstr = SyslogLogger::message_cstring(&[b"a\0b\n"]);
        assert_eq!(cstr.to_bytes(), b"a?b");
    }

    #[test]
    fn message_cstring_strips_trailing_newlines() {
        let cstr = SyslogLogger::message_cstring(&[b"hi\n\n"]);
        assert_eq!(cstr.to_bytes(), b"hi");
    }

    #[test]
    fn syslog_logger_allows_empty_log_tag() {
        let _guard = lock_syslog();
        let mut c = cfg();
        c.log_tag = Some(String::new());
        let logger = SyslogLogger::new(&c).expect("empty --log-tag must be accepted");
        assert_eq!(logger.ident.to_bytes(), b"");
    }

    #[test]
    fn validate_syslog_ident_rejects_controls_allows_empty_and_colon() {
        assert!(SyslogLogger::validate_syslog_ident("ok-tag").is_ok());
        assert!(SyslogLogger::validate_syslog_ident("").is_ok());
        assert!(SyslogLogger::validate_syslog_ident("app:component").is_ok());
        assert!(SyslogLogger::validate_syslog_ident("bad\ntag").is_err());
        assert!(SyslogLogger::validate_syslog_ident(&"a".repeat(64)).is_ok());
    }

    #[test]
    fn get_line_len_reports_partial_and_complete() {
        assert_eq!(SyslogLogger::get_line_len(b"abc\ndef"), (4, false));
        assert_eq!(SyslogLogger::get_line_len(b"abc"), (3, true));
        assert_eq!(SyslogLogger::get_line_len(b""), (0, true));
    }

    #[test]
    fn write_emits_exactly_capacity_bytes_without_newline() {
        let _guard = lock_syslog();
        test_capture::start();
        let mut logger = SyslogLogger::new(&cfg()).expect("create");
        let payload = vec![b'x'; STDIO_BUF_SIZE];
        logger.write(true, &payload).expect("write");
        let captured = test_capture::take();
        assert_eq!(captured.len(), 1, "full buffer must emit immediately");
        assert_eq!(captured[0].0, LOG_INFO);
        assert_eq!(captured[0].1, payload);
        assert!(logger.stdout.is_empty());
    }

    #[test]
    fn write_emits_once_when_writes_accumulate_to_capacity() {
        let _guard = lock_syslog();
        test_capture::start();
        let mut logger = SyslogLogger::new(&cfg()).expect("create");
        let first = vec![b'a'; STDIO_BUF_SIZE / 2];
        let second = vec![b'b'; STDIO_BUF_SIZE - first.len()];
        logger.write(true, &first).expect("first");
        assert!(
            test_capture::take().is_empty(),
            "below capacity stays buffered"
        );
        test_capture::start();
        logger.write(true, &second).expect("second");
        let captured = test_capture::take();
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0].0, LOG_INFO);
        let mut expected = first;
        expected.extend_from_slice(&second);
        assert_eq!(
            captured[0].1, expected,
            "must emit concatenated bytes once with no duplication"
        );
        assert!(logger.stdout.is_empty());
    }

    #[test]
    fn write_buffers_below_capacity_until_newline_or_flush() {
        let _guard = lock_syslog();
        test_capture::start();
        let mut logger = SyslogLogger::new(&cfg()).expect("create");
        logger.write(true, b"partial").expect("partial");
        assert!(test_capture::take().is_empty());
        assert_eq!(logger.stdout.as_slice(), b"partial");

        test_capture::start();
        logger.write(true, b" line\n").expect("complete");
        let captured = test_capture::take();
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0].0, LOG_INFO);
        assert_eq!(captured[0].1, b"partial line");
        assert!(logger.stdout.is_empty());

        test_capture::start();
        logger.write(true, b"leftover").expect("buffer again");
        assert!(test_capture::take().is_empty());
        test_capture::start();
        logger.write(true, b"").expect("flush");
        let flushed = test_capture::take();
        assert_eq!(flushed.len(), 1);
        assert_eq!(flushed[0].1, b"leftover");
    }

    #[test]
    fn write_parses_priority_prefix_split_across_writes() {
        let _guard = lock_syslog();
        test_capture::start();
        let mut logger = SyslogLogger::new(&cfg()).expect("create");
        logger.write(false, b"<3").expect("prefix start");
        assert!(test_capture::take().is_empty());
        test_capture::start();
        logger.write(false, b">split prefix\n").expect("prefix end");
        let captured = test_capture::take();
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0].0, 3);
        assert_eq!(captured[0].1, b"split prefix");
    }

    #[test]
    fn write_uses_stdout_and_stderr_buffers_and_default_priorities() {
        let _guard = lock_syslog();
        test_capture::start();
        let mut logger = SyslogLogger::new(&cfg()).expect("create");

        logger.write(true, b"out-partial").expect("stdout partial");
        logger.write(false, b"err-partial").expect("stderr partial");
        assert!(test_capture::take().is_empty());
        assert_eq!(logger.stdout.as_slice(), b"out-partial");
        assert_eq!(logger.stderr.as_slice(), b"err-partial");

        test_capture::start();
        logger.write(true, b"!\n").expect("stdout complete");
        let out = test_capture::take();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, LOG_INFO);
        assert_eq!(out[0].1, b"out-partial!");
        assert!(logger.stdout.is_empty());
        assert_eq!(logger.stderr.as_slice(), b"err-partial");

        test_capture::start();
        logger.write(false, b"!\n").expect("stderr complete");
        let err = test_capture::take();
        assert_eq!(err.len(), 1);
        assert_eq!(err[0].0, LOG_ERR);
        assert_eq!(err[0].1, b"err-partial!");
        assert!(logger.stderr.is_empty());
    }
}
