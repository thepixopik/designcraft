//! Streamable HTTP transport for DesignCraft's MCP server (MCP 2025-03-26 / 2025-06-18).
//!
//! Exposes a single MCP endpoint (`POST /mcp`, `DELETE /mcp`, `GET /mcp` -> 405) plus a liveness
//! probe (`GET /health` and `GET /healthz`) over `std::net::TcpListener`, keeping the workspace
//! dependency-free of async HTTP runtimes.
//!
//! A single dedicated engine worker thread owns all [`Server`] sessions (`Session` is `!Send` due
//! to font/render caches), while a bounded pool of short-lived socket threads parses HTTP/1.1
//! requests and writes responses.

use std::collections::HashMap;
use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError, sync_channel};
use std::time::{Duration, Instant};

use crate::backend::Backend;
use crate::server::Server;

const MAX_HEADER_LINE_BYTES: usize = 8 * 1024;
const MAX_HEADERS: usize = 64;
const MAX_BODY_BYTES: usize = 16 << 20;
const SOCKET_IO_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_CONCURRENT_CONNECTIONS: usize = 64;
const JOB_QUEUE_CAPACITY: usize = 128;
const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const DEFAULT_MAX_SESSIONS: usize = 32;
const SHARED_SESSION_KEY: &str = "default";

static NEXT_SESSION_SEQ: AtomicU64 = AtomicU64::new(1);

/// Configuration for the Streamable HTTP MCP server.
#[derive(Clone, Debug)]
pub struct HttpConfig {
    /// Optional `Authorization: Bearer <token>` secret required on `/mcp`.
    pub auth_token: Option<String>,
    /// Evict non-default sessions after this much inactivity.
    pub idle_timeout: Duration,
    /// Maximum number of concurrent stateful sessions kept in memory.
    pub max_sessions: usize,
    /// Reuse a warm default session when clients omit `Mcp-Session-Id` (or send an expired one),
    /// allowing stateless cloud connectors to keep document state across tool calls.
    pub shared_session: bool,
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self { auth_token: None, idle_timeout: DEFAULT_IDLE_TIMEOUT, max_sessions: DEFAULT_MAX_SESSIONS, shared_session: true }
    }
}

enum EngineJob {
    Post { session_id: Option<String>, body: String, reply: SyncSender<HttpReply> },
    Delete { session_id: Option<String>, reply: SyncSender<HttpReply> },
    Shutdown,
}

struct HttpReply {
    status: u16,
    reason: &'static str,
    content_type: Option<&'static str>,
    session_id: Option<String>,
    allow: Option<&'static str>,
    body: Vec<u8>,
}

impl HttpReply {
    fn json(status: u16, reason: &'static str, session_id: Option<String>, body: String) -> Self {
        Self { status, reason, content_type: Some("application/json; charset=utf-8"), session_id, allow: None, body: body.into_bytes() }
    }

    fn empty(status: u16, reason: &'static str, session_id: Option<String>) -> Self {
        Self { status, reason, content_type: None, session_id, allow: None, body: Vec::new() }
    }

    fn method_not_allowed(allow: &'static str) -> Self {
        Self { status: 405, reason: "Method Not Allowed", content_type: None, session_id: None, allow: Some(allow), body: Vec::new() }
    }

    fn unauthorized() -> Self {
        Self::json(401, "Unauthorized", None, "{\"error\":\"unauthorized\"}".into())
    }
}

struct SessionEntry {
    server: Server,
    last_used: Instant,
}

/// Bind `addr` and serve Streamable HTTP MCP requests until the listener fails.
pub fn serve<F>(addr: &str, config: HttpConfig, make_backend: F) -> std::io::Result<()>
where
    F: FnMut() -> Result<Box<dyn Backend>, String> + Send + 'static,
{
    let listener = TcpListener::bind(addr)?;
    serve_listener(listener, config, make_backend)
}

/// Serve Streamable HTTP MCP requests on an already-bound [`TcpListener`].
pub fn serve_listener<F>(listener: TcpListener, config: HttpConfig, make_backend: F) -> std::io::Result<()>
where
    F: FnMut() -> Result<Box<dyn Backend>, String> + Send + 'static,
{
    let stop = Arc::new(AtomicBool::new(false));
    serve_until(listener, config, stop, make_backend)
}

/// Serve Streamable HTTP MCP requests until `stop` is set to `true`.
pub fn serve_until<F>(listener: TcpListener, config: HttpConfig, stop: Arc<AtomicBool>, mut make_backend: F) -> std::io::Result<()>
where
    F: FnMut() -> Result<Box<dyn Backend>, String> + Send + 'static,
{
    let (job_tx, job_rx) = sync_channel::<EngineJob>(JOB_QUEUE_CAPACITY);
    let idle_timeout = config.idle_timeout;
    let max_sessions = config.max_sessions.max(1);
    let shared_session = config.shared_session;

    let engine_thread = std::thread::Builder::new().name("designcraft-mcp-engine".into()).spawn(move || {
        let mut sessions: HashMap<String, SessionEntry> = HashMap::new();
        while let Ok(job) = job_rx.recv() {
            if matches!(job, EngineJob::Shutdown) {
                break;
            }
            evict_expired(&mut sessions, idle_timeout);
            run_engine_job(&mut sessions, max_sessions, shared_session, &mut make_backend, job);
        }
    })?;

    let config = Arc::new(config);
    let active = Arc::new(AtomicUsize::new(0));
    let accept_res = accept_loop(listener, config, active, stop, job_tx.clone());
    let _ = job_tx.send(EngineJob::Shutdown);
    let _ = engine_thread.join();
    accept_res
}

fn accept_loop(
    listener: TcpListener,
    config: Arc<HttpConfig>,
    active: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    job_tx: SyncSender<EngineJob>,
) -> std::io::Result<()> {
    for incoming in listener.incoming() {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let mut stream = match incoming {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }
            Err(e) => return Err(e),
        };
        if active.load(Ordering::Relaxed) >= MAX_CONCURRENT_CONNECTIONS {
            let busy = HttpReply::json(503, "Service Unavailable", None, "{\"error\":\"too many connections\"}".into());
            let _ = write_http_reply(&mut stream, &busy, true);
            continue;
        }
        let cfg = Arc::clone(&config);
        let tx = job_tx.clone();
        let counter = Arc::clone(&active);
        counter.fetch_add(1, Ordering::Relaxed);
        let spawn_res = std::thread::Builder::new().name("designcraft-mcp-http".into()).spawn(move || {
            let _ = handle_connection(&mut stream, &cfg, &tx);
            counter.fetch_sub(1, Ordering::Relaxed);
        });
        if spawn_res.is_err() {
            active.fetch_sub(1, Ordering::Relaxed);
        }
    }
    Ok(())
}

fn handle_connection(stream: &mut TcpStream, config: &HttpConfig, job_tx: &SyncSender<EngineJob>) -> std::io::Result<()> {
    let _ = stream.set_read_timeout(Some(SOCKET_IO_TIMEOUT));
    let _ = stream.set_write_timeout(Some(SOCKET_IO_TIMEOUT));
    let _ = stream.set_nodelay(true);
    let cloned = stream.try_clone()?;
    let mut reader = BufReader::new(cloned);

    loop {
        let req = match read_http_request(&mut reader) {
            Ok(Some(r)) => r,
            Ok(None) => return Ok(()),
            Err(reply) => {
                write_http_reply(stream, &reply, true)?;
                return Ok(());
            }
        };
        let close = req.close_after;
        let reply = dispatch_http_request(req, config, job_tx);
        write_http_reply(stream, &reply, close)?;
        if close {
            return Ok(());
        }
    }
}

fn dispatch_http_request(req: ParsedRequest, config: &HttpConfig, job_tx: &SyncSender<EngineJob>) -> HttpReply {
    let path = req.path.split('?').next().unwrap_or(req.path.as_str());
    if path == "/health" || path == "/healthz" {
        if req.method == "GET" || req.method == "HEAD" {
            return HttpReply::json(200, "OK", None, "{\"ok\":true,\"service\":\"designcraft-mcp\"}".into());
        }
        return HttpReply::method_not_allowed("GET, HEAD");
    }

    if path != "/mcp" && path != "/" {
        return HttpReply::json(404, "Not Found", None, "{\"error\":\"not found\"}".into());
    }

    if req.method == "OPTIONS" {
        return HttpReply::empty(204, "No Content", None);
    }

    if let Some(expected) = config.auth_token.as_deref()
        && !authorized(req.authorization.as_deref(), expected)
    {
        return HttpReply::unauthorized();
    }

    match req.method.as_str() {
        "GET" => HttpReply::method_not_allowed("POST, DELETE"),
        "DELETE" => send_job(job_tx, |reply| EngineJob::Delete { session_id: req.session_id, reply }),
        "POST" => send_job(job_tx, |reply| EngineJob::Post { session_id: req.session_id, body: req.body, reply }),
        _ => HttpReply::method_not_allowed("POST, DELETE"),
    }
}

fn send_job(job_tx: &SyncSender<EngineJob>, build: impl FnOnce(SyncSender<HttpReply>) -> EngineJob) -> HttpReply {
    let (tx, rx) = sync_channel(1);
    match job_tx.try_send(build(tx)) {
        Ok(()) => rx.recv().unwrap_or_else(|_| HttpReply::json(500, "Internal Server Error", None, "{\"error\":\"engine closed\"}".into())),
        Err(TrySendError::Full(_)) => HttpReply::json(503, "Service Unavailable", None, "{\"error\":\"server busy\"}".into()),
        Err(TrySendError::Disconnected(_)) => HttpReply::json(503, "Service Unavailable", None, "{\"error\":\"server shutting down\"}".into()),
    }
}

fn evict_expired(sessions: &mut HashMap<String, SessionEntry>, timeout: Duration) {
    let now = Instant::now();
    sessions.retain(|id, entry| id == SHARED_SESSION_KEY || now.duration_since(entry.last_used) < timeout);
}

fn run_engine_job<F>(sessions: &mut HashMap<String, SessionEntry>, max_sessions: usize, shared_session: bool, make_backend: &mut F, job: EngineJob)
where
    F: FnMut() -> Result<Box<dyn Backend>, String>,
{
    match job {
        EngineJob::Shutdown => {}
        EngineJob::Delete { session_id, reply } => {
            let sid = session_id.unwrap_or_else(|| SHARED_SESSION_KEY.to_string());
            let removed = sessions.remove(&sid).is_some();
            let resp = if removed || shared_session {
                HttpReply::empty(204, "No Content", None)
            } else {
                HttpReply::json(404, "Not Found", None, "{\"error\":\"unknown session\"}".into())
            };
            let _ = reply.send(resp);
        }
        EngineJob::Post { session_id, body, reply } => {
            let resp = handle_post(sessions, max_sessions, shared_session, make_backend, session_id, &body);
            let _ = reply.send(resp);
        }
    }
}

fn handle_post<F>(
    sessions: &mut HashMap<String, SessionEntry>,
    max_sessions: usize,
    shared_session: bool,
    make_backend: &mut F,
    asked_sid: Option<String>,
    body: &str,
) -> HttpReply
where
    F: FnMut() -> Result<Box<dyn Backend>, String>,
{
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return HttpReply::json(400, "Bad Request", None, "{\"error\":\"empty JSON-RPC body\"}".into());
    }
    let is_init = is_initialize_payload(trimmed);

    let target_sid = match asked_sid {
        Some(id) if sessions.contains_key(&id) => id,
        Some(_) if !shared_session && !is_init => {
            return HttpReply::json(404, "Not Found", None, "{\"error\":\"session expired or not found\"}".into());
        }
        Some(_) | None if shared_session => SHARED_SESSION_KEY.to_string(),
        Some(_) | None => new_session_id(),
    };

    if !sessions.contains_key(&target_sid) {
        if sessions.len() >= max_sessions {
            evict_oldest_non_shared(sessions);
        }
        if sessions.len() >= max_sessions {
            return HttpReply::json(503, "Service Unavailable", None, "{\"error\":\"session limit reached\"}".into());
        }
        let backend = match make_backend() {
            Ok(b) => b,
            Err(e) => {
                let msg = serde_json::json!({"error": e}).to_string();
                return HttpReply::json(500, "Internal Server Error", None, msg);
            }
        };
        sessions.insert(target_sid.clone(), SessionEntry { server: Server::new(backend), last_used: Instant::now() });
    }

    let Some(entry) = sessions.get_mut(&target_sid) else {
        return HttpReply::json(500, "Internal Server Error", None, "{\"error\":\"session lookup failed\"}".into());
    };
    entry.last_used = Instant::now();

    let out = if trimmed.starts_with('[') { dispatch_batch(&mut entry.server, trimmed) } else { entry.server.handle_line(trimmed) };

    match out {
        Some(json_body) => HttpReply::json(200, "OK", Some(target_sid), json_body),
        None => HttpReply::empty(202, "Accepted", Some(target_sid)),
    }
}

fn dispatch_batch(server: &mut Server, raw: &str) -> Option<String> {
    let items: Vec<serde_json::Value> = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(e) => {
            return Some(serde_json::json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": format!("parse error: {e}")}}).to_string());
        }
    };
    if items.is_empty() {
        return Some(serde_json::json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32600, "message": "empty batch"}}).to_string());
    }
    let mut responses = Vec::new();
    for item in items {
        let line = serde_json::to_string(&item).unwrap_or_default();
        if let Some(resp_str) = server.handle_line(&line)
            && let Ok(val) = serde_json::from_str::<serde_json::Value>(&resp_str)
        {
            responses.push(val);
        }
    }
    if responses.is_empty() { None } else { serde_json::to_string(&responses).ok() }
}

fn is_initialize_payload(raw: &str) -> bool {
    let Ok(val) = serde_json::from_str::<serde_json::Value>(raw) else { return false };
    match val {
        serde_json::Value::Object(map) => map.get("method").and_then(|m| m.as_str()) == Some("initialize"),
        serde_json::Value::Array(arr) => arr.iter().any(|item| item.get("method").and_then(|m| m.as_str()) == Some("initialize")),
        _ => false,
    }
}

fn evict_oldest_non_shared(sessions: &mut HashMap<String, SessionEntry>) {
    let oldest = sessions
        .iter()
        .filter(|(k, _)| k.as_str() != SHARED_SESSION_KEY)
        .min_by_key(|(_, v)| v.last_used)
        .map(|(k, _)| k.clone());
    if let Some(k) = oldest {
        sessions.remove(&k);
    }
}

fn new_session_id() -> String {
    let seq = NEXT_SESSION_SEQ.fetch_add(1, Ordering::Relaxed);
    let mut h1 = RandomState::new().build_hasher();
    h1.write_u64(seq);
    let a = h1.finish();
    let mut h2 = RandomState::new().build_hasher();
    h2.write_u64(a ^ seq.rotate_left(17));
    let b = h2.finish();
    format!("dc-{seq:x}-{a:016x}{b:016x}")
}

fn authorized(header: Option<&str>, expected_token: &str) -> bool {
    let Some(raw) = header else { return false };
    let mut parts = raw.splitn(2, ' ');
    let scheme = parts.next().unwrap_or("");
    let provided = parts.next().unwrap_or("").trim();
    if !scheme.eq_ignore_ascii_case("bearer") {
        return false;
    }
    constant_time_eq(provided.as_bytes(), expected_token.as_bytes())
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (&x, &y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

struct ParsedRequest {
    method: String,
    path: String,
    authorization: Option<String>,
    session_id: Option<String>,
    close_after: bool,
    body: String,
}

fn read_line_capped(reader: &mut impl BufRead, buf: &mut Vec<u8>, cap: usize) -> std::io::Result<bool> {
    buf.clear();
    let limit = (cap as u64).saturating_add(2);
    let n = reader.take(limit).read_until(b'\n', buf)?;
    if n == 0 {
        return Ok(false);
    }
    while matches!(buf.last(), Some(b'\r' | b'\n')) {
        buf.pop();
    }
    Ok(true)
}

fn read_http_request(reader: &mut BufReader<TcpStream>) -> Result<Option<ParsedRequest>, HttpReply> {
    let mut line_buf = Vec::new();
    let eof = match read_line_capped(reader, &mut line_buf, MAX_HEADER_LINE_BYTES) {
        Ok(has_data) => !has_data,
        Err(_) => return Ok(None),
    };
    if eof {
        return Ok(None);
    }
    if line_buf.len() > MAX_HEADER_LINE_BYTES {
        return Err(HttpReply::json(414, "URI Too Long", None, "{\"error\":\"request line too long\"}".into()));
    }
    let req_line = String::from_utf8(line_buf)
        .map_err(|_| HttpReply::json(400, "Bad Request", None, "{\"error\":\"invalid request line encoding\"}".into()))?;
    if req_line.trim().is_empty() {
        return Ok(None);
    }

    let mut parts = req_line.split_whitespace();
    let method = parts
        .next()
        .ok_or_else(|| HttpReply::json(400, "Bad Request", None, "{\"error\":\"missing HTTP method\"}".into()))?
        .to_uppercase();
    let path = parts
        .next()
        .ok_or_else(|| HttpReply::json(400, "Bad Request", None, "{\"error\":\"missing request target\"}".into()))?
        .to_string();
    let version = parts
        .next()
        .ok_or_else(|| HttpReply::json(400, "Bad Request", None, "{\"error\":\"missing HTTP version\"}".into()))?;

    let mut content_length: Option<usize> = None;
    let mut chunked = false;
    let mut authorization: Option<String> = None;
    let mut session_id: Option<String> = None;
    let mut connection_hdr: Option<String> = None;

    for _ in 0..MAX_HEADERS {
        let mut hbuf = Vec::new();
        let has_line = read_line_capped(reader, &mut hbuf, MAX_HEADER_LINE_BYTES)
            .map_err(|_| HttpReply::json(400, "Bad Request", None, "{\"error\":\"truncated headers\"}".into()))?;
        if !has_line || hbuf.is_empty() {
            break;
        }
        if hbuf.len() > MAX_HEADER_LINE_BYTES {
            return Err(HttpReply::json(431, "Request Header Fields Too Large", None, "{\"error\":\"header line too large\"}".into()));
        }
        let header_str = String::from_utf8(hbuf)
            .map_err(|_| HttpReply::json(400, "Bad Request", None, "{\"error\":\"invalid header encoding\"}".into()))?;
        let Some((name, val)) = header_str.split_once(':') else {
            return Err(HttpReply::json(400, "Bad Request", None, "{\"error\":\"malformed header\"}".into()));
        };
        let name = name.trim();
        let val = val.trim();
        if name.eq_ignore_ascii_case("content-length") {
            let parsed = val
                .parse::<usize>()
                .map_err(|_| HttpReply::json(400, "Bad Request", None, "{\"error\":\"bad Content-Length\"}".into()))?;
            if parsed > MAX_BODY_BYTES {
                return Err(HttpReply::json(413, "Payload Too Large", None, "{\"error\":\"payload exceeds max size\"}".into()));
            }
            content_length = Some(parsed);
        } else if name.eq_ignore_ascii_case("transfer-encoding") {
            if val.to_ascii_lowercase().contains("chunked") {
                chunked = true;
            }
        } else if name.eq_ignore_ascii_case("authorization") {
            authorization = Some(val.to_string());
        } else if name.eq_ignore_ascii_case("mcp-session-id") && !val.is_empty() {
            session_id = Some(val.to_string());
        } else if name.eq_ignore_ascii_case("connection") {
            connection_hdr = Some(val.to_ascii_lowercase());
        }
    }

    let body_bytes = if chunked {
        read_chunked_body(reader)?
    } else if let Some(len) = content_length {
        let mut buf = vec![0u8; len];
        reader
            .read_exact(&mut buf)
            .map_err(|_| HttpReply::json(400, "Bad Request", None, "{\"error\":\"unexpected EOF reading body\"}".into()))?;
        buf
    } else {
        Vec::new()
    };

    let body = String::from_utf8(body_bytes)
        .map_err(|_| HttpReply::json(400, "Bad Request", None, "{\"error\":\"request body is not UTF-8\"}".into()))?;

    let close_after = match connection_hdr.as_deref() {
        Some(c) if c.contains("close") => true,
        Some(c) if c.contains("keep-alive") => false,
        _ => version == "HTTP/1.0",
    };

    Ok(Some(ParsedRequest { method, path, authorization, session_id, close_after, body }))
}

fn read_chunked_body(reader: &mut BufReader<TcpStream>) -> Result<Vec<u8>, HttpReply> {
    let mut out = Vec::new();
    loop {
        let mut size_line = Vec::new();
        let ok = read_line_capped(reader, &mut size_line, 128)
            .map_err(|_| HttpReply::json(400, "Bad Request", None, "{\"error\":\"invalid chunk header\"}".into()))?;
        if !ok || size_line.len() > 128 {
            return Err(HttpReply::json(400, "Bad Request", None, "{\"error\":\"invalid chunk size line\"}".into()));
        }
        let size_str = String::from_utf8(size_line)
            .map_err(|_| HttpReply::json(400, "Bad Request", None, "{\"error\":\"bad chunk header encoding\"}".into()))?;
        let hex_part = size_str.split(';').next().unwrap_or("").trim();
        let chunk_len = usize::from_str_radix(hex_part, 16)
            .map_err(|_| HttpReply::json(400, "Bad Request", None, "{\"error\":\"malformed chunk size\"}".into()))?;
        if chunk_len == 0 {
            let mut trailer = Vec::new();
            let _ = read_line_capped(reader, &mut trailer, MAX_HEADER_LINE_BYTES);
            break;
        }
        if out.len().saturating_add(chunk_len) > MAX_BODY_BYTES {
            return Err(HttpReply::json(413, "Payload Too Large", None, "{\"error\":\"chunked payload too large\"}".into()));
        }
        let start = out.len();
        out.resize(start + chunk_len, 0);
        if let Some(target) = out.get_mut(start..) {
            reader
                .read_exact(target)
                .map_err(|_| HttpReply::json(400, "Bad Request", None, "{\"error\":\"truncated chunk body\"}".into()))?;
        }
        let mut crlf = [0u8; 2];
        reader
            .read_exact(&mut crlf)
            .map_err(|_| HttpReply::json(400, "Bad Request", None, "{\"error\":\"missing chunk CRLF\"}".into()))?;
    }
    Ok(out)
}

fn write_http_reply(stream: &mut TcpStream, reply: &HttpReply, close: bool) -> std::io::Result<()> {
    let mut head = format!("HTTP/1.1 {} {}\r\nContent-Length: {}\r\n", reply.status, reply.reason, reply.body.len());
    if let Some(ct) = reply.content_type {
        head.push_str(&format!("Content-Type: {ct}\r\n"));
    }
    if let Some(sid) = reply.session_id.as_deref() {
        head.push_str(&format!("Mcp-Session-Id: {sid}\r\n"));
    }
    if let Some(allow) = reply.allow {
        head.push_str(&format!("Allow: {allow}\r\n"));
    }
    head.push_str("Access-Control-Allow-Origin: *\r\n");
    head.push_str("Access-Control-Allow-Methods: POST, DELETE, OPTIONS\r\n");
    head.push_str("Access-Control-Allow-Headers: Authorization, Content-Type, Accept, Mcp-Session-Id, MCP-Protocol-Version\r\n");
    head.push_str("Access-Control-Expose-Headers: Mcp-Session-Id\r\n");
    head.push_str(if close { "Connection: close\r\n\r\n" } else { "Connection: keep-alive\r\n\r\n" });

    stream.write_all(head.as_bytes())?;
    if !reply.body.is_empty() {
        stream.write_all(&reply.body)?;
    }
    stream.flush()
}

#[cfg(test)]
mod http_tests {
    use super::*;
    use crate::Headless;

    struct TestServer {
        addr: String,
        stop: Arc<AtomicBool>,
        join: Option<std::thread::JoinHandle<()>>,
    }

    impl TestServer {
        fn start(config: HttpConfig) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
            let addr = listener.local_addr().expect("local addr").to_string();
            let stop = Arc::new(AtomicBool::new(false));
            let stop_flag = Arc::clone(&stop);
            let join = std::thread::spawn(move || {
                let _ = serve_until(listener, config, stop_flag, || Ok(Box::new(Headless::with_document())));
            });
            Self { addr, stop, join: Some(join) }
        }

        fn raw_request(&self, raw: &str) -> String {
            let mut s = TcpStream::connect(&self.addr).expect("connect");
            s.write_all(raw.as_bytes()).expect("write");
            let mut resp = String::new();
            s.read_to_string(&mut resp).expect("read");
            resp
        }
    }

    impl Drop for TestServer {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            let _ = TcpStream::connect(&self.addr);
            if let Some(j) = self.join.take() {
                let _ = j.join();
            }
        }
    }

    #[test]
    fn health_endpoint_responds_without_auth() {
        let srv = TestServer::start(HttpConfig { auth_token: Some("secret-123".into()), ..Default::default() });
        let resp = srv.raw_request("GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
        assert!(resp.starts_with("HTTP/1.1 200 OK\r\n"), "unexpected response: {resp}");
        assert!(resp.contains("\"service\":\"designcraft-mcp\""));
    }

    #[test]
    fn bearer_token_gate_blocks_unauthenticated_mcp_calls() {
        let srv = TestServer::start(HttpConfig { auth_token: Some("top-secret".into()), ..Default::default() });
        let body = "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}";
        let unauth = srv.raw_request(&format!(
            "POST /mcp HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        ));
        assert!(unauth.starts_with("HTTP/1.1 401 Unauthorized\r\n"));

        let authed = srv.raw_request(&format!(
            "POST /mcp HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer top-secret\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        ));
        assert!(authed.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(authed.contains("\"result\":{}"));
    }

    #[test]
    fn initialize_and_tool_call_persist_across_stateless_and_stateful_requests() {
        let srv = TestServer::start(HttpConfig::default());

        let init = "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"2025-03-26\"}}";
        let r1 = srv.raw_request(&format!(
            "POST /mcp HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{init}",
            init.len()
        ));
        assert!(r1.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(r1.contains("Mcp-Session-Id: default\r\n"));
        assert!(r1.contains("\"name\":\"designcraft\""));

        let notif = "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}";
        let r2 = srv.raw_request(&format!(
            "POST /mcp HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{notif}",
            notif.len()
        ));
        assert!(r2.starts_with("HTTP/1.1 202 Accepted\r\n"));

        let create = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "execute",
                "arguments": {
                    "command": "frame.create",
                    "params": {"rect": [72.0, 72.0, 300.0, 200.0], "content": "text", "text": "PersistLayout"}
                }
            }
        })
        .to_string();
        let r3 = srv.raw_request(&format!(
            "POST /mcp HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{create}",
            create.len()
        ));
        assert!(r3.starts_with("HTTP/1.1 200 OK\r\n"), "create failed: {r3}");

        let inspect = "{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"tools/call\",\"params\":{\"name\":\"inspect_document\",\"arguments\":{}}}";
        let r4 = srv.raw_request(&format!(
            "POST /mcp HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{inspect}",
            inspect.len()
        ));
        assert!(r4.contains("PersistLayout"), "expected frame text persisted across calls: {r4}");

        let r5 = srv.raw_request("GET /mcp HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
        assert!(r5.starts_with("HTTP/1.1 405 Method Not Allowed\r\n"));
    }

    #[test]
    fn chunked_transfer_encoding_post_works() {
        let srv = TestServer::start(HttpConfig::default());
        let body = "{\"jsonrpc\":\"2.0\",\"id\":9,\"method\":\"ping\"}";
        let chunked = format!(
            "POST /mcp HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n{body}\r\n0\r\n\r\n",
            body.len()
        );
        let resp = srv.raw_request(&chunked);
        assert!(resp.starts_with("HTTP/1.1 200 OK\r\n"), "chunked failed: {resp}");
        assert!(resp.contains("\"id\":9"));
    }
}
