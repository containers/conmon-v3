use crate::{
    error::{ConmonError, ConmonResult},
    logging::plugin::{LogPlugin, LogPluginCfg},
};
use arrayvec::ArrayVec;
use chrono::{Datelike, SecondsFormat, Utc};
use log::warn;
use nix::syslog::{Facility, LogFlags, Priority, Severity, closelog, openlog, syslog};
use std::collections::HashMap;
use std::ffi::{CString, OsStr};
use std::io::{self, Write};
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream, ToSocketAddrs, UdpSocket};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::{UnixDatagram, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

const STDIO_BUF_SIZE: usize = 8192;
const DEFAULT_SYSLOG_PORT: u16 = 514;

/// How long one TCP connect, or one socket write, may block.
///
/// Opening a TCP connection gives up after this long and returns an error.
/// The next resolved address is then tried. If every address fails, this
/// message is not sent.
///
/// The kernel applies the same limit to each individual write, and one
/// syslog frame may need several writes. A receiver that takes a few bytes
/// and then stalls can therefore hold the send for longer than this limit.
/// When a write hits the limit, it returns an error. The socket is closed
/// and the whole message is sent again on a new connection, once. Bytes the
/// peer already accepted stay there. If that retry also times out, the
/// message is dropped.
///
/// Either failure is logged and ignored by the stdio loop, so the container
/// keeps running. The next line tries again from scratch.
///
/// The hostname is looked up before any connect, and that lookup has no
/// timeout. A name with several addresses gets a separate connect attempt
/// for each one. Unix sockets do not use this limit while connecting; it
/// applies only to their later writes.
const IO_TIMEOUT: Duration = Duration::from_secs(1);

const RFC5424_APP_NAME_MAX: usize = 48;
const RFC5424_HOSTNAME_MAX: usize = 255;

const OPT_ADDRESS: &str = "syslog-address";
const OPT_FACILITY: &str = "syslog-facility";

/// `openlog`/`closelog` configure a process-global syslog destination; only one
/// local `SyslogLogger` is supported per process.
static SYSLOG_LOGGER_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Logging plugin that writes container stdout and stderr lines to syslog.
///
/// Leave `syslog-address` unset and each line is handed to the C library
/// (`openlog` / `syslog`), which delivers it to the machine's syslog
/// service. That path uses the `user` facility (`LOG_USER`) unless
/// `syslog-facility` names another one. An empty `syslog-address` is an
/// error; omit the option to stay on this local path.
///
/// `tcp://` and `udp://` use the structured syslog layout (RFC 5424). On
/// TCP, each message is prefixed with its length in bytes (RFC 6587) so
/// the receiver can see where one message ends. UDP puts one message in
/// each datagram. `unix://` and `unixgram://` use the traditional local
/// layout (`<pri>Mmm dd hh:mm:ss tag[pid]: message\n`) and no length
/// prefix. Every remote path defaults to the `daemon` facility
/// (`LOG_DAEMON`). The container line is copied through unchanged,
/// including zero bytes and bytes that are not valid UTF-8. The local C
/// call is a C string, so a zero byte inside a line is replaced with `?`.
///
/// The remote server is contacted when the first line is ready to send.
/// A server that is down at startup still lets the container start. A
/// malformed `syslog-address` or `syslog-facility` is rejected in
/// [`SyslogLogger::new`], before the container runs.
///
/// If a send fails, the socket is closed, a new connection is opened, and
/// that same line is sent once more. If the new connection or the second
/// send fails, the line is dropped. The following line starts over and
/// may connect again. Log rotation calls [`LogPlugin::reopen`], which
/// leaves the remote socket as it is, so rotation does not wait on a
/// syslog connect.
///
/// Connecting and writing run on the thread that reads container output.
/// A slow receiver, or a server that never answers, holds that thread
/// until the attempt finishes, and container output can stall for that
/// time. A failed line leaves later lines free to connect and write on
/// their own. Hostname lookup has no time limit, each address it returns
/// can spend one connect timeout, and writing a stream can take longer
/// than that timeout; see [`IO_TIMEOUT`]. The output thread ignores the
/// error from [`LogPlugin::write`], so a logging failure leaves the
/// container running.
pub struct SyslogLogger {
    stdout: ArrayVec<u8, STDIO_BUF_SIZE>,
    stderr: ArrayVec<u8, STDIO_BUF_SIZE>,
    backend: Backend,
    /// Ident / TAG used in syslog messages (also passed to `openlog` for local).
    ident: CString,
}

enum Backend {
    Local { facility: Facility },
    Remote(RemoteSyslog),
}

struct RemoteSyslog {
    target: RemoteTarget,
    hostname: String,
    facility: Facility,
    conn: Option<RemoteConn>,
}

enum RemoteTarget {
    Tcp(String),
    Udp(String),
    Unix(PathBuf),
    Unixgram(PathBuf),
}

enum RemoteConn {
    Tcp(TcpStream),
    Udp(UdpSocket),
    Unix(UnixStream),
    Unixgram(UnixDatagram),
}

impl SyslogLogger {
    pub fn new(cfg: &LogPluginCfg) -> ConmonResult<Self> {
        if !cfg.log_labels.is_empty() {
            return Err(ConmonError::new("syslog doesn't support --log-label", 1));
        }

        let opts = parse_syslog_opts(&cfg.log_driver_opts)?;
        let address = opts.get(OPT_ADDRESS).map(String::as_str);
        let facility = parse_facility(opts.get(OPT_FACILITY).map(String::as_str))?;
        let ident = Self::syslog_ident(cfg)?;

        if address == Some("") {
            return Err(ConmonError::new(
                "syslog-address must not be empty; omit it to use local syslog",
                1,
            ));
        }

        let Some(address) = address else {
            if SYSLOG_LOGGER_ACTIVE
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                return Err(ConmonError::new(
                    "syslog log driver can only be used once (openlog is process-global)",
                    1,
                ));
            }

            let openlog_facility = facility.unwrap_or(Facility::LOG_USER);
            // Linux openlog requires a 'static ident. Leak a copy so the tag
            // remains valid for the process lifetime (same constraint as the
            // C API); `self.ident` stays owned for remote framing.
            let ident_static: &'static std::ffi::CStr = Box::leak(ident.clone().into_boxed_c_str());
            openlog(
                Some(ident_static),
                LogFlags::LOG_PID | LogFlags::LOG_NDELAY,
                openlog_facility,
            )
            .map_err(|e| ConmonError::new(format!("openlog failed: {e}"), 1))?;

            return Ok(Self {
                stdout: ArrayVec::new(),
                stderr: ArrayVec::new(),
                backend: Backend::Local {
                    facility: openlog_facility,
                },
                ident,
            });
        };

        Ok(Self {
            stdout: ArrayVec::new(),
            stderr: ArrayVec::new(),
            backend: Backend::Remote(RemoteSyslog {
                target: parse_address(address)?,
                hostname: local_hostname(),
                facility: facility.unwrap_or(Facility::LOG_DAEMON),
                conn: None,
            }),
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

    /// Send one assembled line. A leading `<N>` replaces `default_severity`.
    /// Trailing newlines are removed before the send.
    fn emit_assembled(&mut self, assembled: &[u8], default_severity: i32) -> ConmonResult<()> {
        let (severity, message_bytes) =
            if let Some((pri, start)) = Self::parse_priority_prefix(assembled) {
                (pri, &assembled[start..])
            } else {
                (default_severity, assembled)
            };
        self.emit(severity, strip_trailing_newlines(message_bytes))
    }

    /// Send one already-buffered line.
    ///
    /// Local `syslog(3)` has no delivery status; that path always returns
    /// success. A remote failure is warned. The next line tries delivery again.
    fn emit(&mut self, severity: i32, message: &[u8]) -> ConmonResult<()> {
        match &mut self.backend {
            Backend::Local { facility } => {
                // `syslog(3)` stops at the first NUL. Remote delivery uses `message` as-is.
                let message: Vec<u8> = message
                    .iter()
                    .map(|&b| if b == 0 { b'?' } else { b })
                    .collect();
                #[cfg(test)]
                if test_capture::record(severity, &message) {
                    return Ok(());
                }
                let priority = Priority::new(severity_from_i32(severity), *facility);
                // Message bytes are NUL-free after the map above.
                let _ = syslog(priority, OsStr::from_bytes(&message));
                Ok(())
            }
            Backend::Remote(remote) => {
                #[cfg(test)]
                if test_capture::record(severity, message) {
                    return Ok(());
                }
                if let Err(e) = remote.send(self.ident.to_bytes(), severity, message) {
                    warn!("syslog: failed to send remote message: {e}");
                    return Err(e);
                }
                Ok(())
            }
        }
    }

    /// Next line to send, or `None` when the remainder stays buffered.
    ///
    /// A chunk with no newline is appended to the stdout or stderr buffer
    /// until the buffer is full. A newline, a full buffer, an empty flush,
    /// or a chunk that does not fit is returned in one piece, including any
    /// bytes already buffered. stdout and stderr are separate buffers.
    fn next_line(&mut self, is_stdout: bool, input: &mut &[u8]) -> Option<Vec<u8>> {
        let partial = if is_stdout {
            &mut self.stdout
        } else {
            &mut self.stderr
        };
        if input.is_empty() {
            if partial.is_empty() {
                return None;
            }
            return Some(take_line(partial, &[]));
        }

        let (len, incomplete) = Self::get_line_len(input);
        if incomplete {
            if let Ok(()) = partial.try_extend_from_slice(&input[..len]) {
                *input = &input[len..];
                if partial.len() < STDIO_BUF_SIZE {
                    return None;
                }
                return Some(take_line(partial, &[]));
            }
        }

        let line = take_line(partial, &input[..len]);
        *input = &input[len..];
        Some(line)
    }
}

impl Drop for SyslogLogger {
    fn drop(&mut self) {
        if matches!(self.backend, Backend::Local { .. }) {
            closelog();
            SYSLOG_LOGGER_ACTIVE.store(false, Ordering::Release);
        }
    }
}

impl LogPlugin for SyslogLogger {
    /// Log rotation does not reconnect remote syslog.
    ///
    /// Local `openlog` state is unchanged. A remote socket stays as it is.
    /// Connecting here would block the stdio loop during log rotation.
    fn reopen(&mut self) -> ConmonResult<()> {
        Ok(())
    }

    fn write(&mut self, is_stdout: bool, data: &[u8]) -> ConmonResult<()> {
        let severity = if is_stdout {
            Severity::LOG_INFO as i32
        } else {
            Severity::LOG_ERR as i32
        };
        let mut rest = data;
        let mut first_err = None;

        while let Some(line) = self.next_line(is_stdout, &mut rest) {
            if let Err(err) = self.emit_assembled(&line, severity) {
                if first_err.is_none() {
                    first_err = Some(err);
                }
            }
        }

        match first_err {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }
}

/// Drop trailing newlines. A buffer of only newlines becomes empty.
fn strip_trailing_newlines(buf: &[u8]) -> &[u8] {
    match buf.iter().rposition(|&b| b != b'\n') {
        Some(last) => &buf[..=last],
        None => &[],
    }
}

/// Join the buffered bytes with `extra` and clear the buffer.
fn take_line(partial: &mut ArrayVec<u8, STDIO_BUF_SIZE>, extra: &[u8]) -> Vec<u8> {
    let mut line = Vec::with_capacity(partial.len() + extra.len());
    line.extend(partial.drain(..));
    line.extend_from_slice(extra);
    line
}

impl RemoteSyslog {
    /// Dial the configured address once and store the socket.
    fn connect(&mut self) -> ConmonResult<()> {
        let conn = match &self.target {
            RemoteTarget::Tcp(addr) => RemoteConn::Tcp(connect_tcp(addr)?),
            RemoteTarget::Udp(addr) => RemoteConn::Udp(connect_udp(addr)?),
            RemoteTarget::Unix(path) => RemoteConn::Unix(connect_unix(path)?),
            RemoteTarget::Unixgram(path) => RemoteConn::Unixgram(connect_unixgram(path)?),
        };
        self.conn = Some(conn);
        Ok(())
    }

    /// Send one message. A failed write reconnects and retries that frame once.
    ///
    /// With no connection, this dials once and sends once. A failed dial does
    /// not dial again. A failed retry closes the new socket and discards the
    /// message.
    fn send(&mut self, tag: &[u8], severity: i32, message: &[u8]) -> ConmonResult<()> {
        let frame = self.frame(tag, severity, message);
        if self.conn.is_some() {
            match self.write_frame(&frame) {
                Ok(()) => return Ok(()),
                Err(_) => self.conn = None,
            }
        }
        self.connect()?;
        if let Err(err) = self.write_frame(&frame) {
            self.conn = None;
            return Err(err);
        }
        Ok(())
    }

    /// Write one frame on the open socket. A datagram must be sent in full.
    fn write_frame(&mut self, frame: &[u8]) -> ConmonResult<()> {
        let Some(conn) = self.conn.as_mut() else {
            return Err(ConmonError::new(
                "syslog: remote connection not established",
                1,
            ));
        };
        let len = frame.len();
        let result = match conn {
            // Each `write` is limited by `SO_SNDTIMEO`. `write_all` may call
            // it more than once, so a slow peer can take longer than `IO_TIMEOUT`.
            RemoteConn::Tcp(stream) => stream.write_all(frame),
            RemoteConn::Unix(stream) => stream.write_all(frame),
            RemoteConn::Udp(sock) => full_datagram(sock.send(frame), len),
            RemoteConn::Unixgram(sock) => full_datagram(sock.send(frame), len),
        };
        result.map_err(|e| ConmonError::new(format!("syslog: send failed: {e}"), 1))
    }

    /// Build one syslog frame. Unix uses the local layout; TCP and UDP use RFC 5424.
    /// TCP prefixes the message with its length in bytes.
    fn frame(&self, tag: &[u8], severity: i32, message: &[u8]) -> Vec<u8> {
        let pri = (self.facility as i32) | (severity & 0x07);
        if matches!(
            self.target,
            RemoteTarget::Unix(_) | RemoteTarget::Unixgram(_)
        ) {
            return unix_local_frame(pri, &String::from_utf8_lossy(tag), message);
        }
        let ts = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
        let host = rfc5424_token(&self.hostname, RFC5424_HOSTNAME_MAX);
        let app = rfc5424_token(&String::from_utf8_lossy(tag), RFC5424_APP_NAME_MAX);
        // MSG is the original line bytes. Header fields above are sanitized
        // separately, so invalid UTF-8 in the container output is not replaced.
        let mut body =
            format!("<{pri}>1 {ts} {host} {app} {} - - ", std::process::id()).into_bytes();
        body.extend_from_slice(message);
        if matches!(self.target, RemoteTarget::Tcp(_)) {
            let mut framed = format!("{} ", body.len()).into_bytes();
            framed.extend_from_slice(&body);
            framed
        } else {
            body
        }
    }
}

fn full_datagram(sent: io::Result<usize>, len: usize) -> io::Result<()> {
    match sent {
        Ok(n) if n == len => Ok(()),
        Ok(n) => Err(io::Error::new(
            io::ErrorKind::WriteZero,
            format!("short datagram send: {n} of {len}"),
        )),
        Err(e) => Err(e),
    }
}

/// Docker `UnixFormatter` plus a trailing newline. No octet-count prefix.
///
/// `time.Stamp` is an English month, a space-padded day, and local `hh:mm:ss`.
fn unix_local_frame(pri: i32, tag: &str, message: &[u8]) -> Vec<u8> {
    let mut frame = format!(
        "<{pri}>{} {tag}[{}]: ",
        unix_timestamp(),
        std::process::id()
    )
    .into_bytes();
    frame.extend_from_slice(message);
    if !frame.ends_with(b"\n") {
        frame.push(b'\n');
    }
    frame
}

fn unix_timestamp() -> String {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let now = chrono::Local::now();
    format!(
        "{} {:2} {}",
        MONTHS[now.month0() as usize],
        now.day(),
        now.format("%H:%M:%S")
    )
}

fn connect_tcp(addr: &str) -> ConmonResult<TcpStream> {
    let mut last = None;
    for sock in resolve(addr)? {
        match TcpStream::connect_timeout(&sock, IO_TIMEOUT) {
            Ok(stream) => {
                let _ = stream.set_nodelay(true);
                stream.set_write_timeout(Some(IO_TIMEOUT)).map_err(|e| {
                    ConmonError::new(format!("syslog: failed to set TCP write timeout: {e}"), 1)
                })?;
                return Ok(stream);
            }
            Err(e) => last = Some(e),
        }
    }
    Err(ConmonError::new(
        format!(
            "syslog: failed to connect to tcp://{addr}: {}",
            last.map(|e| e.to_string())
                .unwrap_or_else(|| "no addresses".into())
        ),
        1,
    ))
}

fn connect_udp(addr: &str) -> ConmonResult<UdpSocket> {
    let mut last = None;
    for dest in resolve(addr)? {
        let bind = if dest.is_ipv6() {
            SocketAddr::from((Ipv6Addr::UNSPECIFIED, 0))
        } else {
            SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0))
        };
        let sock = match UdpSocket::bind(bind) {
            Ok(sock) => sock,
            Err(e) => {
                last = Some(e);
                continue;
            }
        };
        if let Err(e) = sock.set_write_timeout(Some(IO_TIMEOUT)) {
            last = Some(e);
            continue;
        }
        match sock.connect(dest) {
            Ok(()) => return Ok(sock),
            Err(e) => last = Some(e),
        }
    }
    Err(ConmonError::new(
        format!(
            "syslog: failed to connect to udp://{addr}: {}",
            last.map(|e| e.to_string())
                .unwrap_or_else(|| "no addresses".into())
        ),
        1,
    ))
}

fn connect_unix(path: &Path) -> ConmonResult<UnixStream> {
    let stream = UnixStream::connect(path).map_err(|e| {
        ConmonError::new(
            format!(
                "syslog: failed to connect to unix://{}: {e}",
                path.display()
            ),
            1,
        )
    })?;
    stream.set_write_timeout(Some(IO_TIMEOUT)).map_err(|e| {
        ConmonError::new(format!("syslog: failed to set Unix write timeout: {e}"), 1)
    })?;
    Ok(stream)
}

fn connect_unixgram(path: &Path) -> ConmonResult<UnixDatagram> {
    let sock = UnixDatagram::unbound()
        .map_err(|e| ConmonError::new(format!("syslog: failed to open unixgram socket: {e}"), 1))?;
    sock.set_write_timeout(Some(IO_TIMEOUT)).map_err(|e| {
        ConmonError::new(
            format!("syslog: failed to set unixgram write timeout: {e}"),
            1,
        )
    })?;
    sock.connect(path).map_err(|e| {
        ConmonError::new(
            format!(
                "syslog: failed to connect to unixgram://{}: {e}",
                path.display()
            ),
            1,
        )
    })?;
    Ok(sock)
}

fn resolve(addr: &str) -> ConmonResult<Vec<SocketAddr>> {
    let addrs = addr
        .to_socket_addrs()
        .map_err(|e| ConmonError::new(format!("syslog: failed to resolve '{addr}': {e}"), 1))?
        .collect::<Vec<_>>();
    if addrs.is_empty() {
        return Err(ConmonError::new(
            format!("syslog: failed to resolve '{addr}': no addresses"),
            1,
        ));
    }
    Ok(addrs)
}

fn parse_syslog_opts(log_opts: &[(String, String)]) -> ConmonResult<HashMap<String, String>> {
    let mut opts = HashMap::new();
    for (k, v) in log_opts {
        match k.as_str() {
            "path" => {}
            "syslog-format" => {
                return Err(ConmonError::new(
                    "syslog-format is not supported; TCP and UDP use RFC 5424, Unix sockets use local syslog format",
                    1,
                ));
            }
            OPT_ADDRESS | OPT_FACILITY => {
                if opts.contains_key(k) {
                    return Err(ConmonError::new(
                        format!("duplicate log opt '{k}' for syslog log driver"),
                        1,
                    ));
                }
                opts.insert(k.clone(), v.clone());
            }
            other if other.starts_with("syslog-") => {
                return Err(ConmonError::new(
                    format!("unknown log opt '{other}' for syslog log driver"),
                    1,
                ));
            }
            _ => {}
        }
    }
    Ok(opts)
}

fn parse_address(address: &str) -> ConmonResult<RemoteTarget> {
    let (scheme, rest) = address.split_once("://").ok_or_else(|| {
        ConmonError::new(
            format!(
                "invalid {OPT_ADDRESS} '{address}': expected tcp://, udp://, unix://, or unixgram://"
            ),
            1,
        )
    })?;
    match scheme {
        "tcp" => Ok(RemoteTarget::Tcp(host_port(rest)?)),
        "udp" => Ok(RemoteTarget::Udp(host_port(rest)?)),
        "unix" => Ok(RemoteTarget::Unix(unix_socket_path(rest)?)),
        "unixgram" => Ok(RemoteTarget::Unixgram(unix_socket_path(rest)?)),
        other => Err(ConmonError::new(
            format!(
                "syslog: unsupported scheme '{other}'; use tcp://, udp://, unix://, or unixgram://"
            ),
            1,
        )),
    }
}

/// Absolute path after `unix://` or `unixgram://`. The text is not URL-decoded.
fn unix_socket_path(rest: &str) -> ConmonResult<PathBuf> {
    if rest.is_empty() || rest.contains(['?', '#']) || !rest.starts_with('/') {
        return Err(ConmonError::new(
            format!("invalid {OPT_ADDRESS}: Unix socket path must be absolute, got '{rest}'"),
            1,
        ));
    }
    Ok(PathBuf::from(rest))
}

/// `host[:port]` or `[ipv6]` / `[ipv6]:port`. A missing port defaults to 514.
///
/// Bare IPv6 (`tcp://::1`) is rejected; it must be bracketed. An empty,
/// non-numeric, or zero port is an error and is not replaced with 514.
fn host_port(rest: &str) -> ConmonResult<String> {
    let rest = rest.trim();
    if rest.is_empty() {
        return Err(ConmonError::new(
            format!("invalid {OPT_ADDRESS}: missing host"),
            1,
        ));
    }
    if let Some(host) = rest.strip_prefix('[') {
        let (host, after) = host.split_once(']').ok_or_else(|| {
            ConmonError::new(format!("invalid {OPT_ADDRESS}: malformed IPv6 host"), 1)
        })?;
        if host.parse::<Ipv6Addr>().is_err() {
            return Err(ConmonError::new(
                format!("invalid {OPT_ADDRESS}: malformed IPv6 host '{host}'"),
                1,
            ));
        }
        let port = match after.strip_prefix(':') {
            Some(p) => parse_port(p)?,
            None if after.is_empty() => DEFAULT_SYSLOG_PORT,
            None => {
                return Err(ConmonError::new(
                    format!("invalid {OPT_ADDRESS}: unexpected trailing '{after}'"),
                    1,
                ));
            }
        };
        return Ok(format!("[{host}]:{port}"));
    }
    if rest.matches(':').count() > 1 {
        return Err(ConmonError::new(
            format!("invalid {OPT_ADDRESS}: bracket IPv6 literals, got '{rest}'"),
            1,
        ));
    }
    let (host, port) = if let Some((host, port)) = rest.rsplit_once(':') {
        (host, parse_port(port)?)
    } else {
        (rest, DEFAULT_SYSLOG_PORT)
    };
    if host.is_empty()
        || host.len() > 255
        || !host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
    {
        return Err(ConmonError::new(
            format!("invalid {OPT_ADDRESS}: malformed host '{host}'"),
            1,
        ));
    }
    Ok(format!("{host}:{port}"))
}

fn parse_port(port: &str) -> ConmonResult<u16> {
    // `u16::from_str` accepts a leading `+`, which is not a port token.
    if port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()) {
        return Err(ConmonError::new(
            format!("invalid {OPT_ADDRESS}: bad port '{port}'"),
            1,
        ));
    }
    let parsed = port
        .parse::<u16>()
        .map_err(|_| ConmonError::new(format!("invalid {OPT_ADDRESS}: bad port '{port}'"), 1))?;
    if parsed == 0 {
        return Err(ConmonError::new(
            format!("invalid {OPT_ADDRESS}: bad port '0'"),
            1,
        ));
    }
    Ok(parsed)
}

/// RFC 5424 PRINTUSASCII (`%d33-126`), truncated to `max` characters.
///
/// Each disallowed character, including a non-ASCII scalar, becomes one `_`.
/// An empty result is the NILVALUE `-`.
fn rfc5424_token(value: &str, max: usize) -> String {
    let mut out = String::new();
    for ch in value.chars() {
        if out.len() == max {
            break;
        }
        if ('!'..='~').contains(&ch) {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() { "-".into() } else { out }
}

fn parse_facility(raw: Option<&str>) -> ConmonResult<Option<Facility>> {
    let Some(facility) = raw.filter(|s| !s.is_empty()) else {
        return Ok(None);
    };

    // Prefer nix's portable Facility names. Linux-only cron/authpriv/ftp are
    // accepted via the numeric path / facility_from_code below.
    const NAMED: &[(&str, Facility)] = &[
        ("kern", Facility::LOG_KERN),
        ("user", Facility::LOG_USER),
        ("mail", Facility::LOG_MAIL),
        ("daemon", Facility::LOG_DAEMON),
        ("auth", Facility::LOG_AUTH),
        ("syslog", Facility::LOG_SYSLOG),
        ("lpr", Facility::LOG_LPR),
        ("news", Facility::LOG_NEWS),
        ("uucp", Facility::LOG_UUCP),
        ("local0", Facility::LOG_LOCAL0),
        ("local1", Facility::LOG_LOCAL1),
        ("local2", Facility::LOG_LOCAL2),
        ("local3", Facility::LOG_LOCAL3),
        ("local4", Facility::LOG_LOCAL4),
        ("local5", Facility::LOG_LOCAL5),
        ("local6", Facility::LOG_LOCAL6),
        ("local7", Facility::LOG_LOCAL7),
    ];

    if let Some((_, val)) = NAMED.iter().find(|(n, _)| *n == facility) {
        return Ok(Some(*val));
    }

    // Linux-only names not in nix's portable Facility enum.
    const LINUX_ONLY: &[(&str, i32)] = &[
        ("cron", nix::libc::LOG_CRON),
        ("authpriv", nix::libc::LOG_AUTHPRIV),
        ("ftp", nix::libc::LOG_FTP),
    ];
    if let Some((_, val)) = LINUX_ONLY.iter().find(|(n, _)| *n == facility) {
        return Ok(Some(facility_from_code(*val)?));
    }

    if let Ok(n) = facility.parse::<i32>() {
        if (0..=23).contains(&n) {
            return Ok(Some(facility_from_code(n << 3)?));
        }
    }

    Err(ConmonError::new(
        format!("invalid syslog facility '{facility}'"),
        1,
    ))
}

/// Build a [`Facility`] from a libc facility constant (`n << 3` for `n` in 0..=23).
///
/// nix's portable [`Facility`] enum omits some Linux-only values; those are
/// still valid for `syslog(3)` / remote PRI, so reconstruct via `repr(i32)`.
fn facility_from_code(code: i32) -> ConmonResult<Facility> {
    if code & 0x07 != 0 || !(0..=23).contains(&(code >> 3)) {
        return Err(ConmonError::new(
            format!("invalid syslog facility code {code}"),
            1,
        ));
    }
    // SAFETY: `Facility` is `#[repr(i32)]` and `code` is a valid facility value.
    Ok(unsafe { std::mem::transmute::<i32, Facility>(code) })
}

fn severity_from_i32(severity: i32) -> Severity {
    match severity & 0x07 {
        0 => Severity::LOG_EMERG,
        1 => Severity::LOG_ALERT,
        2 => Severity::LOG_CRIT,
        3 => Severity::LOG_ERR,
        4 => Severity::LOG_WARNING,
        5 => Severity::LOG_NOTICE,
        6 => Severity::LOG_INFO,
        _ => Severity::LOG_DEBUG,
    }
}

fn local_hostname() -> String {
    match nix::unistd::gethostname() {
        Ok(name) => name.to_string_lossy().into_owned(),
        Err(_) => "localhost".into(),
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
    use nix::libc;
    use std::io::Read;
    use std::net::TcpListener;
    use std::sync::{Mutex, MutexGuard};
    use std::thread;
    use std::time::Duration;

    /// Serialize tests that construct a live local `SyslogLogger` (process-global openlog).
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

    fn cfg_with_opts(pairs: &[(&str, &str)]) -> LogPluginCfg {
        let mut c = cfg();
        c.log_driver_opts = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        c
    }

    fn must_err<T>(result: ConmonResult<T>, what: &str) -> ConmonError {
        match result {
            Ok(_) => panic!("{what}"),
            Err(err) => err,
        }
    }

    #[test]
    fn syslog_logger_new_rejects_labels() {
        let _guard = lock_syslog();
        let mut c = cfg();
        c.log_labels = vec!["FOO=bar".into()];
        let err = must_err(SyslogLogger::new(&c), "labels must be rejected");
        assert!(err.msg.contains("doesn't support --log-label"));
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
    fn syslog_logger_rejects_second_local_instance() {
        let _guard = lock_syslog();
        let first = SyslogLogger::new(&cfg()).expect("first syslog logger");
        let err = must_err(SyslogLogger::new(&cfg()), "second instance must fail");
        assert!(err.msg.contains("only be used once"));
        drop(first);
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
        plugin.write(false, b"<3")?;
        plugin.write(false, b">split prefix\n")?;
        plugin.reopen()?;
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
    fn local_write_replaces_interior_nul_and_strips_newlines() {
        let _guard = lock_syslog();
        test_capture::start();
        let mut logger = SyslogLogger::new(&cfg()).expect("create");
        logger.write(true, b"<3>a\0b\n").expect("write");
        let captured = test_capture::take();
        assert_eq!(captured, vec![(3, b"a?b".to_vec())]);
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
        assert_eq!(captured[0].0, Severity::LOG_INFO as i32);
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
        assert_eq!(captured[0].0, Severity::LOG_INFO as i32);
        let mut expected = first;
        expected.extend_from_slice(&second);
        assert_eq!(captured[0].1, expected);
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
        assert_eq!(captured[0].0, Severity::LOG_INFO as i32);
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
        assert_eq!(out[0].0, Severity::LOG_INFO as i32);
        assert_eq!(out[0].1, b"out-partial!");
        assert!(logger.stdout.is_empty());
        assert_eq!(logger.stderr.as_slice(), b"err-partial");

        test_capture::start();
        logger.write(false, b"!\n").expect("stderr complete");
        let err = test_capture::take();
        assert_eq!(err.len(), 1);
        assert_eq!(err[0].0, Severity::LOG_ERR as i32);
        assert_eq!(err[0].1, b"err-partial!");
        assert!(logger.stderr.is_empty());
    }

    #[test]
    fn parse_facility_named_and_numeric() {
        assert_eq!(parse_facility(None).unwrap(), None);
        assert_eq!(parse_facility(Some("")).unwrap(), None);
        assert_eq!(
            parse_facility(Some("daemon")).unwrap(),
            Some(Facility::LOG_DAEMON)
        );
        assert_eq!(
            parse_facility(Some("user")).unwrap(),
            Some(Facility::LOG_USER)
        );
        assert_eq!(
            parse_facility(Some("3")).unwrap(),
            Some(facility_from_code(3 << 3).unwrap())
        );
        assert_eq!(
            parse_facility(Some("local0")).unwrap(),
            Some(Facility::LOG_LOCAL0)
        );
        assert!(parse_facility(Some("nope")).is_err());
        assert!(parse_facility(Some("99")).is_err());
    }

    #[test]
    fn parse_address_accepts_tcp_udp_and_default_port() {
        match parse_address("tcp://127.0.0.1").unwrap() {
            RemoteTarget::Tcp(a) => assert_eq!(a, "127.0.0.1:514"),
            _ => panic!("expected tcp"),
        }
        match parse_address("tcp://localhost:1514").unwrap() {
            RemoteTarget::Tcp(a) => assert_eq!(a, "localhost:1514"),
            _ => panic!("expected tcp"),
        }
        match parse_address("udp://[::1]:1514").unwrap() {
            RemoteTarget::Udp(a) => assert_eq!(a, "[::1]:1514"),
            _ => panic!("expected udp"),
        }
        match parse_address("tcp://[::1]").unwrap() {
            RemoteTarget::Tcp(a) => assert_eq!(a, "[::1]:514"),
            _ => panic!("expected tcp"),
        }
        match parse_address("unix:///run/systemd/journal/syslog").unwrap() {
            RemoteTarget::Unix(path) => {
                assert_eq!(path, PathBuf::from("/run/systemd/journal/syslog"))
            }
            _ => panic!("expected unix"),
        }
        match parse_address("unixgram:///dev/log").unwrap() {
            RemoteTarget::Unixgram(path) => assert_eq!(path, PathBuf::from("/dev/log")),
            _ => panic!("expected unixgram"),
        }
    }

    #[test]
    fn parse_address_rejects_malformed_and_unsupported() {
        for addr in [
            "tcp://localhost:abc",
            "tcp://localhost:",
            "tcp://::1",
            "tcp://[::1]:",
            "tcp://[::1]:0",
            "tcp://[::1]:abc",
            "tcp://[::1]junk",
            "tcp://[gggg::1]:514",
            "tcp://[fe80::1%eth0]:514",
            "tcp://",
            "tcp://:514",
            "tcp://local host",
            "tcp://127.0.0.1:65536",
            "udp://::1",
            "unix://tmp/syslog.sock",
            "unix://localhost/tmp/log",
            "unix://",
            "unix:///tmp/log?x=1",
            "unixgram://relative",
            "unixgram:///tmp/log#frag",
            "tcp+tls://localhost:514",
            "not-a-url",
        ] {
            assert!(parse_address(addr).is_err(), "{addr} must be rejected");
        }
    }

    #[test]
    fn empty_syslog_address_is_rejected() {
        let _guard = lock_syslog();
        let err = must_err(
            SyslogLogger::new(&cfg_with_opts(&[("syslog-address", "")])),
            "empty syslog-address must fail",
        );
        assert!(err.msg.contains("must not be empty"), "{}", err.msg);
    }

    #[test]
    fn syslog_format_opt_is_rejected() {
        let err = must_err(
            SyslogLogger::new(&cfg_with_opts(&[("syslog-format", "rfc5424")])),
            "syslog-format must be rejected",
        );
        assert!(
            err.msg.contains("syslog-format is not supported"),
            "{}",
            err.msg
        );
    }

    #[test]
    fn unknown_syslog_opt_rejected() {
        let err = must_err(
            SyslogLogger::new(&cfg_with_opts(&[("syslog-bogus", "x")])),
            "unknown opt",
        );
        assert!(err.msg.contains("unknown log opt"));
    }

    #[test]
    fn duplicate_syslog_opts_rejected() {
        let err = must_err(
            SyslogLogger::new(&cfg_with_opts(&[
                ("syslog-address", "tcp://127.0.0.1:514"),
                ("syslog-address", "tcp://127.0.0.1:1514"),
            ])),
            "duplicate syslog-address must fail",
        );
        assert!(err.msg.contains("duplicate"), "{}", err.msg);
    }

    #[test]
    fn remote_reopen_keeps_the_same_connection() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().unwrap().port();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let first = read_octet_frame(&mut stream);
            let second = read_octet_frame(&mut stream);
            (first, second)
        });

        let mut logger = SyslogLogger::new(&cfg_with_opts(&[(
            "syslog-address",
            &format!("tcp://127.0.0.1:{port}"),
        )]))
        .expect("remote");
        logger.write(true, b"before-reopen\n").expect("connect");
        {
            let Backend::Remote(remote) = &logger.backend else {
                panic!("remote backend");
            };
            assert!(remote.conn.is_some(), "the first write must connect");
        }

        logger.reopen().expect("reopen");
        {
            let Backend::Remote(remote) = &logger.backend else {
                panic!("remote backend");
            };
            assert!(remote.conn.is_some(), "reopen must keep the connection");
        }

        logger
            .write(true, b"after-reopen\n")
            .expect("same connection");
        drop(logger);

        let (first, second) = handle.join().expect("listener");
        assert!(first.ends_with(b"before-reopen"), "{first:?}");
        assert!(second.ends_with(b"after-reopen"), "{second:?}");
    }

    #[test]
    fn remote_tcp_preserves_nul_and_invalid_utf8() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().unwrap().port();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            read_octet_frame(&mut stream)
        });

        let mut logger = SyslogLogger::new(&cfg_with_opts(&[(
            "syslog-address",
            &format!("tcp://127.0.0.1:{port}"),
        )]))
        .expect("remote");
        logger.write(true, b"a\0b\xff\n").expect("write");
        drop(logger);

        let body = handle.join().expect("listener");
        assert!(body.ends_with(b"a\0b\xff"), "{body:?}");
        assert!(
            !body.windows(3).any(|w| w == "\u{FFFD}".as_bytes()),
            "{body:?}"
        );
    }

    #[test]
    fn remote_udp_preserves_nul_and_invalid_utf8() {
        let sock = UdpSocket::bind("127.0.0.1:0").expect("bind");
        let port = sock.local_addr().unwrap().port();
        sock.set_read_timeout(Some(Duration::from_secs(2))).unwrap();

        let mut logger = SyslogLogger::new(&cfg_with_opts(&[(
            "syslog-address",
            &format!("udp://127.0.0.1:{port}"),
        )]))
        .expect("udp");
        logger.write(true, b"a\0b\xff\n").expect("write");

        let mut buf = [0u8; 2048];
        let n = sock.recv(&mut buf).expect("recv");
        let msg = &buf[..n];
        assert!(msg.ends_with(b"a\0b\xff"), "{msg:?}");
        assert!(!msg.contains(&b'\n'), "{msg:?}");
        assert!(
            !msg.windows(3).any(|w| w == "\u{FFFD}".as_bytes()),
            "{msg:?}"
        );
    }

    #[test]
    fn rfc5424_token_sanitizes_header_fields() {
        assert_eq!(rfc5424_token("harbor", 48), "harbor");
        assert_eq!(rfc5424_token("a b=\"x\"]", 48), "a_b=\"x\"]");
        assert_eq!(rfc5424_token("héllo", 48), "h_llo");
        assert_eq!(rfc5424_token("héllo😀x", 48), "h_llo_x");
        assert_eq!(rfc5424_token("", 48), "-");
        assert_eq!(rfc5424_token(&"n".repeat(60), 48), "n".repeat(48));
    }

    fn read_octet_frame(stream: &mut TcpStream) -> Vec<u8> {
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut digits = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            stream.read_exact(&mut byte).expect("octet-count byte");
            if byte[0] == b' ' {
                break;
            }
            assert!(byte[0].is_ascii_digit(), "octet count must be decimal");
            digits.push(byte[0]);
        }
        let len: usize = std::str::from_utf8(&digits).unwrap().parse().unwrap();
        let mut msg = vec![0u8; len];
        stream.read_exact(&mut msg).expect("octet-counted body");
        msg
    }

    #[test]
    fn remote_tcp_frames_rfc5424_with_octet_counting() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().unwrap().port();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let first = read_octet_frame(&mut stream);
            let second = read_octet_frame(&mut stream);
            (first, second)
        });

        let mut c = cfg_with_opts(&[
            ("syslog-address", &format!("tcp://127.0.0.1:{port}")),
            ("syslog-facility", "local0"),
        ]);
        c.log_tag = Some("a b".into());
        let mut logger = SyslogLogger::new(&c).expect("remote");
        let tag = logger.ident.as_bytes().to_vec();
        {
            let Backend::Remote(remote) = &mut logger.backend else {
                panic!("remote backend");
            };
            // `write` splits on newlines. Send a message that already contains one
            // so the frame is not delimited by that newline.
            remote
                .send(&tag, Severity::LOG_INFO as i32, b"hello\nworld")
                .expect("embedded newline");
            remote
                .send(&tag, Severity::LOG_INFO as i32, b"second-frame")
                .expect("second");
        }
        drop(logger);

        let (first, second) = handle.join().expect("listener");
        let msg = String::from_utf8(first).expect("utf-8");
        // local0|info = 128|6 = 134. The tag's space is not a legal APP-NAME character.
        let (header, body) = msg.split_once(" - - ").expect("NILVALUE fields");
        assert_eq!(body, "hello\nworld");
        assert!(header.starts_with("<134>1 "), "{header}");
        assert!(header.contains(" a_b "), "{header}");
        let second = String::from_utf8(second).expect("utf-8");
        assert!(second.ends_with("second-frame"), "{second}");
    }

    #[test]
    fn remote_udp_sends_one_datagram() {
        let v4 = std::net::UdpSocket::bind("127.0.0.1:0").expect("udp4");
        let port = v4.local_addr().unwrap().port();
        v4.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let mut logger = SyslogLogger::new(&cfg_with_opts(&[(
            "syslog-address",
            &format!("udp://127.0.0.1:{port}"),
        )]))
        .expect("udp4");
        logger.write(true, b"udp-v4\n").expect("udp4 write");
        let mut buf = [0u8; 2048];
        let n = v4.recv(&mut buf).expect("udp4 recv");
        let msg = String::from_utf8_lossy(&buf[..n]);
        // daemon|info = 24|6 = 30. No octet-count prefix and no trailing newline.
        assert!(msg.starts_with("<30>1 "), "{msg}");
        assert!(msg.contains("udp-v4"), "{msg}");
        assert!(!msg.contains('\n'), "{msg}");
        drop(logger);

        let v6 = match std::net::UdpSocket::bind("[::1]:0") {
            Ok(sock) => sock,
            Err(e)
                if e.kind() == std::io::ErrorKind::AddrNotAvailable
                    || e.raw_os_error() == Some(libc::EAFNOSUPPORT) =>
            {
                return;
            }
            Err(e) => panic!("bind [::1]: {e}"),
        };
        let port = v6.local_addr().unwrap().port();
        v6.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let mut logger = SyslogLogger::new(&cfg_with_opts(&[(
            "syslog-address",
            &format!("udp://[::1]:{port}"),
        )]))
        .expect("udp6");
        logger.write(true, b"udp-v6\n").expect("udp6 write");
        let n = v6.recv(&mut buf).expect("udp6 recv");
        let msg = String::from_utf8_lossy(&buf[..n]);
        assert!(msg.starts_with("<30>1 "), "{msg}");
        assert!(msg.contains("udp-v6"), "{msg}");
    }

    fn accept_for(
        listener: &TcpListener,
        timeout: Duration,
    ) -> std::io::Result<(TcpStream, std::net::SocketAddr)> {
        let fd = std::os::fd::AsRawFd::as_raw_fd(listener);
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let waited = i32::try_from(timeout.as_millis()).unwrap_or(i32::MAX);
        let ready = unsafe { libc::poll(&mut pfd, 1, waited) };
        if ready == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "accept timed out",
            ));
        }
        if ready < 0 {
            return Err(std::io::Error::last_os_error());
        }
        listener.accept()
    }

    #[test]
    fn remote_connect_failure_discards_message_and_next_line_connects() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let mut logger = SyslogLogger::new(&cfg_with_opts(&[(
            "syslog-address",
            &format!("tcp://127.0.0.1:{port}"),
        )]))
        .expect("valid config does not connect");

        let err = logger
            .write(true, b"one\npartial")
            .expect_err("delivery must fail");
        assert!(err.msg.contains("failed to connect"), "{}", err.msg);
        assert_eq!(logger.stdout.as_slice(), b"partial");
        logger.stdout.clear();
        {
            let Backend::Remote(remote) = &logger.backend else {
                panic!("remote backend");
            };
            assert!(remote.conn.is_none());
        }

        let again = TcpListener::bind(("127.0.0.1", port)).expect("rebind");
        let recovery = thread::spawn(move || {
            let (mut sock, _) = accept_for(&again, Duration::from_secs(2)).expect("recovery");
            read_octet_frame(&mut sock)
        });
        logger
            .write(true, b"later\n")
            .expect("the next line connects on its own");
        let frame = recovery.join().expect("recovery thread");
        assert!(frame.ends_with(b"later"), "{frame:?}");
        assert!(!frame.windows(b"one".len()).any(|w| w == b"one"));
    }

    #[test]
    fn remote_tcp_retries_failed_write_once() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().unwrap().port();
        let (closed_tx, closed_rx) = std::sync::mpsc::channel();
        let handle = thread::spawn(move || {
            let (mut first_sock, _) = listener.accept().expect("first accept");
            let first = read_octet_frame(&mut first_sock);
            // A clean close can still accept one more write. Reset the socket
            // so the next client write fails and the same message is retried.
            abort_tcp(first_sock);
            closed_tx.send(()).expect("closed");
            let (mut second_sock, _) = listener.accept().expect("retry accept");
            let second = read_octet_frame(&mut second_sock);
            let mut extra = [0u8; 1];
            let extra_n = second_sock
                .read(&mut extra)
                .expect("read after the retried frame");
            (first, second, extra_n)
        });

        let mut logger = SyslogLogger::new(&cfg_with_opts(&[(
            "syslog-address",
            &format!("tcp://127.0.0.1:{port}"),
        )]))
        .expect("remote");
        logger.write(true, b"first\n").expect("first");
        closed_rx.recv().expect("server closed the first socket");
        logger
            .write(true, b"retried\n")
            .expect("retry on a new connection");
        drop(logger);

        let (first, second, extra_n) = handle.join().expect("listener");
        assert!(first.ends_with(b"first"), "{first:?}");
        assert!(second.ends_with(b"retried"), "{second:?}");
        assert_eq!(
            extra_n, 0,
            "the message must be written only once on the new connection"
        );
    }

    #[test]
    fn remote_retry_failure_stops_after_one_attempt() {
        let path = TempSock::new("retry-fail");
        let server = std::os::unix::net::UnixDatagram::bind(&path.0).expect("bind");
        server.set_nonblocking(true).unwrap();
        let mut logger = SyslogLogger::new(&cfg_with_opts(&[(
            "syslog-address",
            &format!("unixgram://{}", path.0.display()),
        )]))
        .expect("unixgram");
        logger.write(true, b"small\n").expect("connected");
        let mut buf = [0u8; 256];
        let n = server.recv(&mut buf).expect("small datagram");
        assert!(buf[..n].windows(b"small".len()).any(|w| w == b"small"));

        // The next send fails locally. The server is still bound, so the one
        // reconnect succeeds and the same message is delivered once.
        shutdown_fd(client_raw_fd(&logger));
        logger.write(true, b"once\n").expect("one retry");
        let n = server.recv(&mut buf).expect("retried datagram");
        assert!(buf[..n].windows(b"once".len()).any(|w| w == b"once"));
        assert_eq!(
            server.recv(&mut buf).unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock,
            "the message must be retried only once"
        );

        // A shutdown server still accepts connect, and every send fails.
        // The logger must stop after that one retry instead of looping.
        shutdown_fd(std::os::fd::AsRawFd::as_raw_fd(&server));
        let err = logger
            .write(true, b"lost\n")
            .expect_err("retry send must fail");
        assert!(err.msg.contains("send failed"), "{}", err.msg);
        {
            let Backend::Remote(remote) = &logger.backend else {
                panic!("remote backend");
            };
            assert!(remote.conn.is_none(), "a failed retry drops the socket");
        }
        assert_eq!(
            server.recv(&mut buf).unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock,
            "the failed message must not be delivered"
        );

        drop(server);
        std::fs::remove_file(&path.0).expect("unlink");
        let server = std::os::unix::net::UnixDatagram::bind(&path.0).expect("rebind");
        server.set_nonblocking(true).unwrap();
        logger
            .write(true, b"after\n")
            .expect("a later message connects again");
        let n = server.recv(&mut buf).expect("later datagram");
        assert!(buf[..n].windows(b"after".len()).any(|w| w == b"after"));
        assert!(!buf[..n].windows(b"lost".len()).any(|w| w == b"lost"));
    }

    fn client_raw_fd(logger: &SyslogLogger) -> std::os::fd::RawFd {
        let Backend::Remote(remote) = &logger.backend else {
            panic!("remote backend");
        };
        let Some(RemoteConn::Unixgram(sock)) = &remote.conn else {
            panic!("expected a unixgram connection");
        };
        std::os::fd::AsRawFd::as_raw_fd(sock)
    }

    fn shutdown_fd(fd: std::os::fd::RawFd) {
        let rc = unsafe { libc::shutdown(fd, libc::SHUT_RDWR) };
        assert_eq!(rc, 0, "{}", std::io::Error::last_os_error());
    }

    fn abort_tcp(stream: TcpStream) {
        let linger = libc::linger {
            l_onoff: 1,
            l_linger: 0,
        };
        let fd = std::os::fd::AsRawFd::as_raw_fd(&stream);
        unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_LINGER,
                &linger as *const libc::linger as *const libc::c_void,
                std::mem::size_of::<libc::linger>() as libc::socklen_t,
            );
        }
        drop(stream);
    }

    struct TempSock(PathBuf);

    impl TempSock {
        fn new(label: &str) -> Self {
            use std::sync::atomic::AtomicU64;
            static N: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "conmon-syslog-{label}-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_file(&path);
            Self(path)
        }
    }

    impl Drop for TempSock {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn read_unix_line(stream: &mut std::os::unix::net::UnixStream) -> Vec<u8> {
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut out = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            stream.read_exact(&mut byte).expect("unix byte");
            out.push(byte[0]);
            if byte[0] == b'\n' {
                return out;
            }
        }
    }

    fn assert_unix_local_line(msg: &[u8], payload: &[u8]) {
        let pri_end = msg.iter().position(|&b| b == b'>').expect("priority");
        assert_eq!(&msg[..=pri_end], b"<30>", "{msg:?}");
        let after = &msg[pri_end + 1..];
        assert!(after.len() > 15, "{msg:?}");
        assert_eq!(after[3], b' ', "month and day: {after:?}");
        assert_eq!(after[6], b' ', "day and time: {after:?}");
        assert_eq!(after[15], b' ', "timestamp and tag: {after:?}");
        let prefix = format!(" testctr[{}]: ", std::process::id());
        assert!(after[15..].starts_with(prefix.as_bytes()), "{after:?}");
        assert!(after.ends_with(payload), "{after:?}");
        assert!(
            !msg.windows(5).any(|w| w == b" - - "),
            "unix framing must not be RFC 5424: {msg:?}"
        );
    }

    fn accept_unix(
        listener: &std::os::unix::net::UnixListener,
        timeout: Duration,
    ) -> std::io::Result<std::os::unix::net::UnixStream> {
        let fd = std::os::fd::AsRawFd::as_raw_fd(listener);
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let waited = i32::try_from(timeout.as_millis()).unwrap_or(i32::MAX);
        let ready = unsafe { libc::poll(&mut pfd, 1, waited) };
        if ready == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "accept timed out",
            ));
        }
        if ready < 0 {
            return Err(std::io::Error::last_os_error());
        }
        listener.accept().map(|(stream, _)| stream)
    }

    #[test]
    fn remote_unix_stream_uses_local_format_and_reconnects() {
        let path = TempSock::new("stream");
        let listener = std::os::unix::net::UnixListener::bind(&path.0).expect("bind");
        let (lines_tx, lines_rx) = std::sync::mpsc::channel();
        let handle = thread::spawn(move || {
            let (mut first_sock, _) = listener.accept().expect("first accept");
            let first = read_unix_line(&mut first_sock);
            // Bound the second accept so a missed reconnect fails the test
            // instead of blocking forever.
            let mut second_sock =
                accept_unix(&listener, Duration::from_secs(2)).expect("retry accept");
            let second = read_unix_line(&mut second_sock);
            lines_tx.send((first, second)).expect("lines");
            let mut extra = [0u8; 1];
            second_sock
                .read(&mut extra)
                .expect("read after the retried frame")
        });

        let mut logger = SyslogLogger::new(&cfg_with_opts(&[(
            "syslog-address",
            &format!("unix://{}", path.0.display()),
        )]))
        .expect("unix");
        logger.write(true, b"unix-one\n").expect("first");
        {
            let Backend::Remote(remote) = &mut logger.backend else {
                panic!("remote backend");
            };
            let Some(RemoteConn::Unix(stream)) = &remote.conn else {
                panic!("expected a unix connection");
            };
            // A closed peer can still accept a write. Shut this socket down so
            // the next write fails and the same message is sent on a new one.
            stream
                .shutdown(std::net::Shutdown::Both)
                .expect("invalidate connection");
        }
        logger.write(true, b"unix-two\n").expect("reconnect");
        let (first, second) = lines_rx.recv().expect("both lines");
        drop(logger);

        let extra_n = handle.join().expect("listener");
        assert_unix_local_line(&first, b"unix-one\n");
        assert_unix_local_line(&second, b"unix-two\n");
        assert_eq!(extra_n, 0, "the message must be written only once");
    }

    #[test]
    fn remote_unixgram_sends_one_datagram_and_reconnects() {
        let path = TempSock::new("dgram");
        let server = std::os::unix::net::UnixDatagram::bind(&path.0).expect("bind");
        server.set_nonblocking(true).unwrap();
        let mut logger = SyslogLogger::new(&cfg_with_opts(&[(
            "syslog-address",
            &format!("unixgram://{}", path.0.display()),
        )]))
        .expect("unixgram");
        logger.write(true, b"gram-one\n").expect("one");
        logger.write(true, b"gram-two\n").expect("two");

        let mut buf = [0u8; 2048];
        let n = server.recv(&mut buf).expect("datagram one");
        assert_unix_local_line(&buf[..n], b"gram-one\n");
        let n = server.recv(&mut buf).expect("datagram two");
        assert_unix_local_line(&buf[..n], b"gram-two\n");
        drop(server);
        std::fs::remove_file(&path.0).expect("unlink");

        let server = std::os::unix::net::UnixDatagram::bind(&path.0).expect("rebind");
        server.set_nonblocking(true).unwrap();
        // A send to the closed peer can still succeed. Shut the client socket
        // down so this write fails and is retried on the new server.
        shutdown_fd(client_raw_fd(&logger));
        logger.write(true, b"gram-three\n").expect("reconnect");
        let n = server.recv(&mut buf).expect("datagram three");
        assert_unix_local_line(&buf[..n], b"gram-three\n");
        assert_eq!(
            server.recv(&mut buf).unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock,
            "the retried message must be delivered once"
        );
    }
}
