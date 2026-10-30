use crate::{
    error::{ConmonError, ConmonResult},
    logging::{
        plugin::{LogPlugin, LogPluginCfg},
        util::{STDIO_BUF_SIZE, next_stdio_line, strip_trailing_newlines, truncate_cuuid},
    },
};
use arrayvec::ArrayVec;
use log::warn;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use ureq::tls::TlsConfig;
use ureq::{Agent, Error as UreqError};

/// Bound on connect + full HTTP round trip. DNS is outside this budget.
/// A timeout after Splunk accepted the body can yield a duplicate on retry.
const IO_TIMEOUT: Duration = Duration::from_secs(1);

/// Cap HEC response reads. Success bodies are tiny; larger ones error.
const MAX_RESPONSE_BODY: u64 = 64 * 1024;

const OPT_URL: &str = "splunk-url";
const OPT_TOKEN: &str = "splunk-token";
const OPT_SOURCE: &str = "splunk-source";
const OPT_SOURCETYPE: &str = "splunk-sourcetype";
const OPT_INDEX: &str = "splunk-index";
const OPT_INSECURE: &str = "splunk-insecureskipverify";

/// Synchronous Splunk HEC logger: one POST per complete container line.
///
/// Required opts: `splunk-url` (full `http(s)://…` event endpoint) and
/// `splunk-token`. Does not dial until the first line. Transient failures
/// rebuild the client and retry the same body once; permanent failures do
/// not. After a failed line, remaining complete lines in that `write` are
/// dropped (partial stays buffered). Logging failure does not stop the
/// container.
pub struct SplunkLogger {
    url: String,
    token: String,
    source: Option<String>,
    sourcetype: Option<String>,
    index: Option<String>,
    insecure: bool,
    tag: String,
    stdout: ArrayVec<u8, STDIO_BUF_SIZE>,
    stderr: ArrayVec<u8, STDIO_BUF_SIZE>,
    agent: Agent,
}

impl std::fmt::Debug for SplunkLogger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SplunkLogger")
            .field("url", &self.url)
            .field("token", &"[redacted]")
            .field("source", &self.source)
            .field("sourcetype", &self.sourcetype)
            .field("index", &self.index)
            .field("insecure", &self.insecure)
            .field("tag", &self.tag)
            .finish_non_exhaustive()
    }
}

struct AttemptError {
    transient: bool,
    message: String,
}

impl SplunkLogger {
    pub fn new(cfg: &LogPluginCfg) -> ConmonResult<Self> {
        if !cfg.log_labels.is_empty() {
            return Err(ConmonError::new("splunk doesn't support --log-label", 1));
        }

        let opts = parse_splunk_opts(&cfg.log_driver_opts)?;
        let url = require_nonempty(&opts, OPT_URL)?;
        let token = require_nonempty(&opts, OPT_TOKEN)?;
        validate_hec_url(&url)?;
        let insecure = parse_bool_opt(opts.get(OPT_INSECURE))?;

        Ok(Self {
            url,
            token,
            source: nonempty_opt(&opts, OPT_SOURCE),
            sourcetype: nonempty_opt(&opts, OPT_SOURCETYPE),
            index: nonempty_opt(&opts, OPT_INDEX),
            insecure,
            tag: log_tag(cfg),
            stdout: ArrayVec::new(),
            stderr: ArrayVec::new(),
            agent: build_agent(insecure),
        })
    }

    fn next_line(&mut self, is_stdout: bool, input: &mut &[u8]) -> Option<Vec<u8>> {
        let partial = if is_stdout {
            &mut self.stdout
        } else {
            &mut self.stderr
        };
        next_stdio_line(partial, input)
    }

    fn emit(&mut self, is_stdout: bool, message: &[u8]) -> ConmonResult<()> {
        let line = String::from_utf8_lossy(strip_trailing_newlines(message));
        let payload = self.payload(is_stdout, &line, unix_time_secs());
        self.deliver(&payload)
    }

    fn payload(&self, is_stdout: bool, line: &str, time: Value) -> String {
        let mut body = json!({
            "time": time,
            "event": {
                "line": line,
                "source": if is_stdout { "stdout" } else { "stderr" },
                "tag": self.tag,
            }
        });
        if let Some(source) = &self.source {
            body["source"] = json!(source);
        }
        if let Some(sourcetype) = &self.sourcetype {
            body["sourcetype"] = json!(sourcetype);
        }
        if let Some(index) = &self.index {
            body["index"] = json!(index);
        }
        body.to_string()
    }

    fn deliver(&mut self, payload: &str) -> ConmonResult<()> {
        match self.post_once(payload) {
            Ok(()) => Ok(()),
            Err(err) if err.transient => {
                // Drop the pool so the retry does not reuse a bad connection.
                self.agent = build_agent(self.insecure);
                match self.post_once(payload) {
                    Ok(()) => Ok(()),
                    Err(err) => self.fail(err),
                }
            }
            Err(err) => self.fail(err),
        }
    }

    fn fail(&self, err: AttemptError) -> ConmonResult<()> {
        let message = redact(&err.message, &self.token);
        warn!("splunk: failed to send event: {message}");
        Err(ConmonError::new(format!("splunk: {message}"), 1))
    }

    fn post_once(&mut self, payload: &str) -> Result<(), AttemptError> {
        let auth = format!("Splunk {}", self.token);
        let response = self
            .agent
            .post(&self.url)
            .header("Authorization", &auth)
            .header("Content-Type", "application/json")
            .send(payload.as_bytes());
        let mut response = match response {
            Ok(response) => response,
            Err(err) => return Err(classify_ureq_error(err, &self.token)),
        };
        let status = response.status().as_u16();
        let body = match response
            .body_mut()
            .with_config()
            .limit(MAX_RESPONSE_BODY)
            .read_to_string()
        {
            Ok(body) => body,
            Err(err) => return Err(classify_ureq_error(err, &self.token)),
        };
        classify_response(status, &body)
    }
}

impl LogPlugin for SplunkLogger {
    fn reopen(&mut self) -> ConmonResult<()> {
        Ok(())
    }

    fn write(&mut self, is_stdout: bool, data: &[u8]) -> ConmonResult<()> {
        let mut rest = data;
        while let Some(line) = self.next_line(is_stdout, &mut rest) {
            if let Err(err) = self.emit(is_stdout, &line) {
                while !rest.is_empty() {
                    if self.next_line(is_stdout, &mut rest).is_none() {
                        break;
                    }
                }
                return Err(err);
            }
        }
        Ok(())
    }
}

impl AttemptError {
    fn transient(message: impl Into<String>) -> Self {
        Self {
            transient: true,
            message: message.into(),
        }
    }

    fn permanent(message: impl Into<String>) -> Self {
        Self {
            transient: false,
            message: message.into(),
        }
    }
}

fn build_agent(insecure: bool) -> Agent {
    let tls = TlsConfig::builder().disable_verification(insecure).build();
    ureq::Agent::config_builder()
        .timeout_global(Some(IO_TIMEOUT))
        .timeout_connect(Some(IO_TIMEOUT))
        .http_status_as_error(false)
        .tls_config(tls)
        .build()
        .into()
}

fn classify_response(status: u16, body: &str) -> Result<(), AttemptError> {
    match status {
        200 => hec_success(body).map_err(AttemptError::permanent),
        429 | 500..=599 => Err(AttemptError::transient(format!("HTTP {status}"))),
        _ => Err(AttemptError::permanent(format!("HTTP {status}"))),
    }
}

/// Classify ureq transport/read errors. HTTP statuses use [`classify_response`]
/// (`http_status_as_error(false)`), so `StatusCode` is not expected here.
fn classify_ureq_error(err: UreqError, token: &str) -> AttemptError {
    use std::io::ErrorKind;

    let transient = match &err {
        UreqError::Timeout(_)
        | UreqError::HostNotFound
        | UreqError::ConnectionFailed
        | UreqError::BodyStalled
        | UreqError::Other(_) => true,
        UreqError::Io(e) => !matches!(
            e.kind(),
            ErrorKind::InvalidData | ErrorKind::InvalidInput | ErrorKind::Unsupported
        ),
        // TLS, BadUri, BodyExceedsLimit, Protocol, etc.
        _ => false,
    };

    let message = redact(&err.to_string(), token);
    if transient {
        AttemptError::transient(message)
    } else {
        AttemptError::permanent(message)
    }
}

fn hec_success(body: &str) -> Result<(), String> {
    if body.trim().is_empty() {
        return Err("empty HEC response".into());
    }
    let value: Value =
        serde_json::from_str(body).map_err(|_| "malformed HEC response".to_string())?;
    match value.get("code") {
        Some(Value::Number(code)) if code.as_i64() == Some(0) => Ok(()),
        Some(code) => Err(format!("HEC error code {code}")),
        None => Err("HEC response missing code".into()),
    }
}

fn redact(message: &str, token: &str) -> String {
    if token.is_empty() {
        return message.to_string();
    }
    message.replace(token, "[redacted]")
}

fn unix_time_secs() -> Value {
    json!(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0)
    )
}

fn log_tag(cfg: &LogPluginCfg) -> String {
    if let Some(tag) = cfg.log_tag.as_ref().filter(|s| !s.is_empty()) {
        return tag.clone();
    }
    if let Some(name) = cfg.name.as_ref().filter(|s| !s.is_empty()) {
        return name.clone();
    }
    if let Some(cuuid) = cfg.cuuid.as_ref().filter(|s| !s.is_empty()) {
        return truncate_cuuid(cuuid).to_string();
    }
    "conmon".into()
}

fn parse_bool_opt(raw: Option<&String>) -> ConmonResult<bool> {
    match raw.map(String::as_str) {
        None => Ok(false),
        Some("1" | "true" | "TRUE" | "True") => Ok(true),
        Some("0" | "false" | "FALSE" | "False") => Ok(false),
        Some(other) => Err(ConmonError::new(
            format!("invalid {OPT_INSECURE} '{other}'"),
            1,
        )),
    }
}

fn require_nonempty(opts: &HashMap<String, String>, key: &str) -> ConmonResult<String> {
    match opts.get(key).map(String::as_str) {
        Some(value) if !value.is_empty() => Ok(value.to_string()),
        Some(_) => Err(ConmonError::new(format!("{key} must not be empty"), 1)),
        None => Err(ConmonError::new(format!("missing {key}"), 1)),
    }
}

fn nonempty_opt(opts: &HashMap<String, String>, key: &str) -> Option<String> {
    opts.get(key).filter(|v| !v.is_empty()).cloned()
}

/// Reject empty / wrong-scheme / missing-host / embedded-credentials URLs.
/// Path is left unchanged; ureq parses the URL again at request time.
fn validate_hec_url(url: &str) -> ConmonResult<()> {
    let uri: ureq::http::Uri = url
        .parse()
        .map_err(|e| ConmonError::new(format!("{OPT_URL} is not a valid URL ({e})"), 1))?;
    match uri.scheme_str() {
        Some("http") | Some("https") => {}
        Some(other) => {
            return Err(ConmonError::new(
                format!("{OPT_URL} must use http:// or https://, not {other}://"),
                1,
            ));
        }
        None => {
            return Err(ConmonError::new(
                format!("{OPT_URL} must use http:// or https://"),
                1,
            ));
        }
    }
    let Some(authority) = uri.authority() else {
        return Err(ConmonError::new(format!("{OPT_URL} is missing a host"), 1));
    };
    if authority.as_str().contains('@') {
        return Err(ConmonError::new(
            format!("{OPT_URL} must not include user info"),
            1,
        ));
    }
    if authority.host().is_empty() {
        return Err(ConmonError::new(format!("{OPT_URL} is missing a host"), 1));
    }
    Ok(())
}

fn parse_splunk_opts(log_opts: &[(String, String)]) -> ConmonResult<HashMap<String, String>> {
    let mut opts = HashMap::new();
    for (k, v) in log_opts {
        match k.as_str() {
            "path" => {}
            "splunk-format" => {
                return Err(ConmonError::new(
                    "splunk-format is not supported; events use the inline HEC format",
                    1,
                ));
            }
            OPT_URL | OPT_TOKEN | OPT_SOURCE | OPT_SOURCETYPE | OPT_INDEX | OPT_INSECURE => {
                if opts.contains_key(k) {
                    return Err(ConmonError::new(
                        format!("duplicate log opt '{k}' for splunk log driver"),
                        1,
                    ));
                }
                opts.insert(k.clone(), v.clone());
            }
            other if other.starts_with("splunk-") => {
                return Err(ConmonError::new(
                    format!("unknown log opt '{other}' for splunk log driver"),
                    1,
                ));
            }
            _ => {}
        }
    }
    Ok(opts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::Instant;

    fn cfg(opts: &[(&str, &str)]) -> LogPluginCfg {
        LogPluginCfg {
            log_tag: Some("ctr".into()),
            log_driver_opts: opts
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
            ..LogPluginCfg::default()
        }
    }

    fn logger(url: &str) -> SplunkLogger {
        SplunkLogger::new(&cfg(&[
            ("splunk-url", url),
            ("splunk-token", "sekret"),
            ("splunk-source", "src"),
            ("splunk-sourcetype", "stype"),
            ("splunk-index", "main"),
        ]))
        .expect("logger")
    }

    enum Step {
        Status(u16, &'static str),
        StatusOwned(u16, String),
        Close,
        Stall,
    }

    struct TestServer {
        url: String,
        seen: Arc<Mutex<Vec<(String, String)>>>,
        queue: Arc<Mutex<VecDeque<Step>>>,
        stop: Arc<std::sync::atomic::AtomicBool>,
        handle: Option<thread::JoinHandle<()>>,
    }

    impl TestServer {
        fn start() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
            let addr = listener.local_addr().expect("addr");
            let seen = Arc::new(Mutex::new(Vec::new()));
            let queue = Arc::new(Mutex::new(VecDeque::new()));
            let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let seen_t = Arc::clone(&seen);
            let queue_t = Arc::clone(&queue);
            let stop_t = Arc::clone(&stop);
            let handle = thread::spawn(move || {
                while !stop_t.load(std::sync::atomic::Ordering::Relaxed) {
                    let Ok((stream, _)) = listener.accept() else {
                        break;
                    };
                    if stop_t.load(std::sync::atomic::Ordering::Relaxed) {
                        break;
                    }
                    let seen_t = Arc::clone(&seen_t);
                    let queue_t = Arc::clone(&queue_t);
                    thread::spawn(move || handle_client(stream, seen_t, queue_t));
                }
            });
            Self {
                url: format!("http://{addr}/services/collector/event"),
                seen,
                queue,
                stop,
                handle: Some(handle),
            }
        }

        fn push(&self, step: Step) {
            self.queue.lock().expect("queue").push_back(step);
        }

        fn seen(&self) -> Vec<(String, String)> {
            self.seen.lock().expect("seen").clone()
        }

        fn assert_posts(&self, n: usize) {
            assert_eq!(self.seen().len(), n);
        }
    }

    impl Drop for TestServer {
        fn drop(&mut self) {
            self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
            let _ = TcpStream::connect(
                self.url
                    .trim_start_matches("http://")
                    .split('/')
                    .next()
                    .unwrap_or(""),
            );
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }

    fn handle_client(
        mut stream: TcpStream,
        seen: Arc<Mutex<Vec<(String, String)>>>,
        queue: Arc<Mutex<VecDeque<Step>>>,
    ) {
        let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
        let Ok((headers, body)) = read_http_request(&mut stream) else {
            return;
        };
        seen.lock().expect("seen").push((headers, body));
        match queue.lock().expect("queue").pop_front() {
            Some(Step::Status(status, body)) => {
                let _ = write_http_response(&mut stream, status, body);
            }
            Some(Step::StatusOwned(status, body)) => {
                let _ = write_http_response(&mut stream, status, &body);
            }
            Some(Step::Stall) => thread::sleep(IO_TIMEOUT + Duration::from_millis(500)),
            Some(Step::Close) | None => {}
        }
    }

    fn read_http_request(stream: &mut TcpStream) -> std::io::Result<(String, String)> {
        let mut reader = BufReader::new(stream);
        let mut headers = String::new();
        loop {
            let len = headers.len();
            reader.read_line(&mut headers)?;
            if headers.len() == len {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "eof",
                ));
            }
            if headers.ends_with("\r\n\r\n") || headers.ends_with("\n\n") {
                break;
            }
        }
        let length = headers.lines().find_map(|line| {
            let (name, value) = line.split_once(':')?;
            if name.eq_ignore_ascii_case("content-length") {
                value.trim().parse::<usize>().ok()
            } else {
                None
            }
        });
        let mut body = vec![0u8; length.unwrap_or(0)];
        reader.read_exact(&mut body)?;
        Ok((headers, String::from_utf8_lossy(&body).into_owned()))
    }

    fn write_http_response(stream: &mut TcpStream, status: u16, body: &str) -> std::io::Result<()> {
        write!(
            stream,
            "HTTP/1.1 {status} OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )?;
        stream.flush()
    }

    #[test]
    fn rejects_invalid_config() {
        for (opts, needle) in [
            (&[][..], "missing splunk-url"),
            (
                &[("splunk-url", "http://127.0.0.1/e")][..],
                "missing splunk-token",
            ),
            (
                &[("splunk-url", ""), ("splunk-token", "t")][..],
                "must not be empty",
            ),
            (
                &[("splunk-url", "ftp://127.0.0.1/e"), ("splunk-token", "t")][..],
                "http:// or https://",
            ),
            (
                &[
                    ("splunk-url", "http://user:pass@127.0.0.1/e"),
                    ("splunk-token", "t"),
                ][..],
                "user info",
            ),
            (
                &[("splunk-url", "not-a-url"), ("splunk-token", "t")][..],
                "http:// or https://",
            ),
            (
                &[
                    ("splunk-url", "http://127.0.0.1/e"),
                    ("splunk-token", "t"),
                    ("splunk-bogus", "1"),
                ][..],
                "unknown log opt",
            ),
            (
                &[
                    ("splunk-url", "http://127.0.0.1/e"),
                    ("splunk-token", "t"),
                    ("splunk-format", "json"),
                ][..],
                "splunk-format is not supported",
            ),
            (
                &[
                    ("splunk-url", "http://127.0.0.1/e"),
                    ("splunk-token", "t"),
                    ("splunk-insecureskipverify", "yes"),
                ][..],
                "invalid splunk-insecureskipverify",
            ),
        ] {
            let err = SplunkLogger::new(&cfg(opts)).unwrap_err();
            assert!(
                err.to_string().contains(needle),
                "opts={opts:?} err={err} needle={needle}"
            );
        }

        let err = SplunkLogger::new(&LogPluginCfg {
            log_labels: vec!["A=b".into()],
            log_driver_opts: vec![
                ("splunk-url".into(), "http://127.0.0.1/e".into()),
                ("splunk-token".into(), "t".into()),
            ],
            ..LogPluginCfg::default()
        })
        .unwrap_err();
        assert!(err.to_string().contains("doesn't support --log-label"));
    }

    #[test]
    fn new_does_not_dial_and_redacts_token() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap();
        let mut logger = SplunkLogger::new(&cfg(&[
            (
                "splunk-url",
                &format!("https://{addr}/services/collector/event"),
            ),
            ("splunk-token", "sekret"),
            ("splunk-insecureskipverify", "true"),
        ]))
        .unwrap();
        assert!(listener.accept().is_err());
        logger.reopen().unwrap();
        assert!(listener.accept().is_err());
        let rendered = format!("{logger:?}");
        assert!(rendered.contains("[redacted]"));
        assert!(!rendered.contains("sekret"));
    }

    #[test]
    fn posts_events_with_auth_metadata_and_buffering() {
        let server = TestServer::start();
        for _ in 0..3 {
            server.push(Step::Status(200, r#"{"text":"Success","code":0}"#));
        }
        let mut plugin = logger(&server.url);

        plugin.write(true, b"he said \"hi\"\nand more").unwrap();
        server.assert_posts(1);
        plugin.write(true, b"\n").unwrap();
        plugin.write(false, b"err\n").unwrap();
        server.assert_posts(3);

        let seen = server.seen();
        let hdr = seen[0].0.to_ascii_lowercase();
        assert!(hdr.contains("authorization: splunk sekret"));
        assert!(hdr.contains("content-type: application/json"));

        let stdout: Value = serde_json::from_str(&seen[0].1).unwrap();
        assert!(stdout["time"].is_number());
        assert_eq!(stdout["event"]["line"], "he said \"hi\"");
        assert_eq!(stdout["event"]["source"], "stdout");
        assert_eq!(stdout["event"]["tag"], "ctr");
        assert_eq!(stdout["source"], "src");
        assert_eq!(stdout["sourcetype"], "stype");
        assert_eq!(stdout["index"], "main");

        let rest: Value = serde_json::from_str(&seen[1].1).unwrap();
        assert_eq!(rest["event"]["line"], "and more");
        let stderr: Value = serde_json::from_str(&seen[2].1).unwrap();
        assert_eq!(stderr["event"]["source"], "stderr");
        assert_eq!(stderr["event"]["line"], "err");
    }

    #[test]
    fn utf8_and_optional_fields() {
        let server = TestServer::start();
        server.push(Step::Status(200, r#"{"text":"Success","code":0}"#));
        let mut plugin = logger(&server.url);
        plugin.write(true, b"\xff\xfe ok\n").unwrap();
        let value: Value = serde_json::from_str(&server.seen()[0].1).unwrap();
        assert_eq!(value["event"]["line"], "\u{FFFD}\u{FFFD} ok");

        let server = TestServer::start();
        server.push(Step::Status(200, r#"{"text":"Success","code":0}"#));
        let mut plugin = SplunkLogger::new(&LogPluginCfg {
            name: Some("box".into()),
            log_driver_opts: vec![
                ("splunk-url".into(), server.url.clone()),
                ("splunk-token".into(), "sekret".into()),
            ],
            ..LogPluginCfg::default()
        })
        .unwrap();
        plugin.write(true, b"hi\n").unwrap();
        let value: Value = serde_json::from_str(&server.seen()[0].1).unwrap();
        assert!(value.get("source").is_none());
        assert_eq!(value["event"]["tag"], "box");
    }

    #[test]
    fn retries_429_and_5xx_not_4xx_or_hec_errors() {
        let server = TestServer::start();
        server.push(Step::Status(400, "no"));
        server.push(Step::Status(200, r#"{"text":"Success","code":0}"#));
        let mut plugin = logger(&server.url);
        let err = plugin.write(true, b"x\n").unwrap_err();
        assert!(err.to_string().contains("HTTP 400"));
        assert!(!err.to_string().contains("sekret"));
        server.assert_posts(1);

        for status in [429, 503] {
            let server = TestServer::start();
            server.push(Step::Status(status, "busy"));
            server.push(Step::Status(200, r#"{"text":"Success","code":0}"#));
            let mut plugin = logger(&server.url);
            plugin.write(true, b"later\n").unwrap();
            server.assert_posts(2);
        }

        let server = TestServer::start();
        server.push(Step::Status(200, r#"{"text":"Invalid token","code":4}"#));
        server.push(Step::Status(200, r#"{"text":"Success","code":0}"#));
        let mut plugin = logger(&server.url);
        let err = plugin.write(true, b"x\n").unwrap_err();
        assert!(err.to_string().contains("HEC error code"));
        server.assert_posts(1);

        let server = TestServer::start();
        server.push(Step::Status(200, "not-json"));
        let mut plugin = logger(&server.url);
        let err = plugin.write(true, b"x\n").unwrap_err();
        assert!(err.to_string().contains("malformed HEC response"));
        server.assert_posts(1);
    }

    #[test]
    fn transport_error_retries_once() {
        let server = TestServer::start();
        server.push(Step::Close);
        server.push(Step::Status(200, r#"{"text":"Success","code":0}"#));
        let mut plugin = logger(&server.url);
        plugin.write(true, b"again\n").unwrap();
        server.assert_posts(2);
    }

    #[test]
    fn timeout_retries_once() {
        let server = TestServer::start();
        server.push(Step::Stall);
        server.push(Step::Status(200, r#"{"text":"Success","code":0}"#));
        let mut plugin = logger(&server.url);
        let started = Instant::now();
        plugin.write(true, b"slow\n").expect("retry after timeout");
        let elapsed = started.elapsed();
        assert!(elapsed >= IO_TIMEOUT);
        assert!(elapsed < Duration::from_secs(3));
        server.assert_posts(2);
    }

    #[test]
    fn write_stops_after_failure_then_recovers() {
        let server = TestServer::start();
        server.push(Step::Status(500, "down"));
        server.push(Step::Status(500, "down"));
        server.push(Step::Status(200, r#"{"text":"Success","code":0}"#));
        let mut plugin = logger(&server.url);
        let err = plugin.write(true, b"one\ntwo\npartial").unwrap_err();
        assert!(err.to_string().contains("HTTP 500"));
        server.assert_posts(2);

        server.push(Step::Status(200, r#"{"text":"Success","code":0}"#));
        plugin.write(true, b"-rest\n").unwrap();
        let seen = server.seen();
        assert_eq!(seen.len(), 3);
        let recovered: Value = serde_json::from_str(&seen[2].1).unwrap();
        assert_eq!(recovered["event"]["line"], "partial-rest");
    }

    #[test]
    fn oversized_response_is_permanent() {
        let server = TestServer::start();
        server.push(Step::StatusOwned(
            200,
            "x".repeat((MAX_RESPONSE_BODY as usize) + 1),
        ));
        server.push(Step::Status(200, r#"{"text":"Success","code":0}"#));
        let mut plugin = logger(&server.url);
        let err = plugin.write(true, b"big\n").unwrap_err();
        assert!(
            err.to_string().contains("larger than request limit")
                || err.to_string().contains("response body")
        );
        assert!(!err.to_string().contains("sekret"));
        server.assert_posts(1);
    }

    #[test]
    fn classify_ureq_tls_and_token_redaction() {
        assert!(classify_ureq_error(UreqError::ConnectionFailed, "tok").transient);
        assert!(!classify_ureq_error(UreqError::TlsRequired, "tok").transient);
        assert!(!classify_ureq_error(UreqError::Tls("cert"), "tok").transient);
        assert!(!classify_ureq_error(UreqError::BodyExceedsLimit(1), "tok").transient);
        let err = classify_ureq_error(UreqError::BadUri("sekret".into()), "sekret");
        assert!(err.message.contains("[redacted]"));
        assert!(!err.message.contains("sekret"));

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let mut plugin = logger(&format!("http://{addr}/services/collector/event"));
        let err = plugin.write(true, b"nope\n").unwrap_err();
        assert!(!err.to_string().contains("sekret"));
    }

    #[test]
    fn stdout_stderr_buffers_independent() {
        let server = TestServer::start();
        server.push(Step::Status(200, r#"{"text":"Success","code":0}"#));
        server.push(Step::Status(200, r#"{"text":"Success","code":0}"#));
        let mut plugin = logger(&server.url);
        plugin.write(true, b"out-partial").unwrap();
        plugin.write(false, b"err-line\n").unwrap();
        server.assert_posts(1);
        plugin.write(true, b"\n").unwrap();
        let seen = server.seen();
        assert_eq!(seen.len(), 2);
        let err_ev: Value = serde_json::from_str(&seen[0].1).unwrap();
        let out_ev: Value = serde_json::from_str(&seen[1].1).unwrap();
        assert_eq!(err_ev["event"]["line"], "err-line");
        assert_eq!(out_ev["event"]["line"], "out-partial");
    }
}
