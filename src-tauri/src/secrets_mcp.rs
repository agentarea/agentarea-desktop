//! The vault's MCP server for local agent CLIs: MCP streamable HTTP on
//! `127.0.0.1:<random port>/mcp`, JSON-RPC over POST answered with plain JSON
//! (no SSE — every tool here answers in one response).
//!
//! An agent never sees a secret value. It lists secrets, asks for a one-time
//! handle (`aas_<32 hex>`: single use, 10 minutes, bound to its thread and that
//! secret) and puts the handle into an `http_request`. This server swaps in the
//! value only when the URL's host is on the secret's allowlist, sends the
//! request itself (no redirects), and redacts every vault value from what comes
//! back. A handle is spent once it passes the checks, even if the request fails.
//!
//! Auth: `run_agent` gets a bearer token per turn that names the thread
//! (`issue_token`); it is revoked when the turn ends.
//!
//! The same server also answers `/data`: the `agentarea_data` tools
//! (data_mcp.rs), with their own per-turn tokens (`issue_data_token`).
//!
//! And `/proxy/agentarea/<secret>`: a thread's AgentArea client MCP endpoint,
//! forwarded with the app's live access token (`register_proxy`). Agent
//! processes live for hours and the token for one, so the CLIs get this URL —
//! its random path segment is the only credential they hold — and every
//! request goes upstream as `Authorization: Bearer <current token>`.
//! Responses (JSON or SSE) are streamed through as they arrive.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use reqwest::Url;
use serde_json::{json, Value};

use crate::data_mcp::{self, DataTurn};
use crate::vault::{percent_encode, Redactor, SecretMeta, Vault};

const HANDLE_TTL: Duration = Duration::from_secs(600);
const MAX_REQUEST_BODY: usize = 4 << 20;
const MAX_RESPONSE_BODY: usize = 1 << 20;
const OUTBOUND_TIMEOUT: Duration = Duration::from_secs(30);
const IDLE_TIMEOUT: Duration = Duration::from_secs(120);
/// Newest first; an unknown version asked for gets 2025-06-18.
pub(crate) const PROTOCOL_VERSIONS: [&str; 4] = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

// ── one-time handles ─────────────────────────────────────────────────────────

#[derive(Debug)]
struct Handle {
    thread: String,
    secret_id: String,
    /// What the agent said it needs the secret for; saved as lastPurpose.
    purpose: String,
    expires: Instant,
    used: bool,
}

#[derive(Default)]
struct Handles(HashMap<String, Handle>);

impl Handles {
    fn issue(&mut self, thread: &str, secret: &SecretMeta, purpose: &str, now: Instant) -> String {
        self.0.retain(|_, h| h.expires > now);
        let handle = format!("aas_{}", uuid::Uuid::new_v4().simple());
        self.0.insert(
            handle.clone(),
            Handle {
                thread: thread.into(),
                secret_id: secret.id.clone(),
                purpose: purpose.into(),
                expires: now + HANDLE_TTL,
                used: false,
            },
        );
        handle
    }

    /// The handle, if this thread may still redeem it. Another thread's handle
    /// looks like an unknown one, so it can't be probed or burned from here.
    fn check(&self, handle: &str, thread: &str, now: Instant) -> Result<&Handle, String> {
        let short = &handle[..handle.len().min(12)];
        match self.0.get(handle) {
            Some(h) if h.thread == thread => {
                if h.used {
                    Err(format!("{short}… was already used; a handle works once. Call request_secret again."))
                } else if h.expires <= now {
                    Err(format!("{short}… expired; call request_secret again."))
                } else {
                    Ok(h)
                }
            }
            _ => Err(format!("{short}… is not a valid handle for this thread. Call request_secret to get one.")),
        }
    }

    fn mark_used(&mut self, handle: &str) {
        if let Some(h) = self.0.get_mut(handle) {
            h.used = true;
        }
    }
}

/// Every distinct `aas_<32 hex>` in `s`, in order.
fn find_handles(s: &str) -> Vec<String> {
    let b = s.as_bytes();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while let Some(pos) = s[i..].find("aas_") {
        let start = i + pos;
        let end = start + 4 + 32;
        if end <= b.len() && b[start + 4..end].iter().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(c)) {
            let h = s[start..end].to_string();
            if !out.contains(&h) {
                out.push(h);
            }
            i = end;
        } else {
            i = start + 4;
        }
    }
    out
}

/// https to an allowed host (`*.example.com` matches any subdomain, not the
/// apex); plain http only to localhost / 127.0.0.1.
fn host_allowed(url: &Url, allowed: &[String]) -> Result<(), String> {
    let host = url.host_str().unwrap_or("").to_ascii_lowercase();
    let local = host == "localhost" || host == "127.0.0.1";
    match url.scheme() {
        "https" => {}
        "http" if local => {}
        s => return Err(format!("{s}:// is not allowed: secrets go over https only (http only to localhost)")),
    }
    let ok = allowed.iter().any(|a| match a.strip_prefix("*.") {
        Some(domain) => host.ends_with(&format!(".{domain}")),
        None => *a == host,
    });
    if ok {
        Ok(())
    } else if allowed.is_empty() {
        Err("this secret has no allowed hosts; the user can add some in AgentArea → Secrets".into())
    } else {
        Err(format!("host {host} is not one of this secret's allowed hosts ({})", allowed.join(", ")))
    }
}

// ── server ───────────────────────────────────────────────────────────────────

struct Inner {
    vault: Vault,
    /// bearer token → thread (session) id
    tokens: Mutex<HashMap<String, String>>,
    handles: Mutex<Handles>,
    /// `/data` bearer token → that turn's data state
    data_turns: Mutex<HashMap<String, Arc<DataTurn>>>,
    /// `/proxy/agentarea/<secret>` → where it goes
    proxies: Mutex<HashMap<String, ProxyRoute>>,
}

/// An AgentArea client MCP endpoint and the sign-in that reaches it.
#[derive(Clone)]
struct ProxyRoute {
    upstream: String,
    api_base: String,
    /// used only when the app has no live token for `api_base`
    token: String,
}

/// The running server; managed as Tauri state.
#[derive(Clone)]
pub struct SecretsMcp {
    inner: Arc<Inner>,
    port: u16,
}

/// A turn's bearer token; revoked when dropped (the turn ended or never started).
pub struct TurnToken {
    mcp: SecretsMcp,
    pub token: String,
}

impl Drop for TurnToken {
    fn drop(&mut self) {
        if let Ok(mut t) = self.mcp.inner.tokens.lock() {
            t.remove(&self.token);
        }
    }
}

/// A turn's `/data` bearer token; revoked (with the turn's API token) when dropped.
pub struct DataToken {
    mcp: SecretsMcp,
    pub token: String,
}

impl Drop for DataToken {
    fn drop(&mut self) {
        if let Ok(mut t) = self.mcp.inner.data_turns.lock() {
            t.remove(&self.token);
        }
    }
}

/// A proxy route for one agent process; removed when dropped.
pub struct ProxyToken {
    mcp: SecretsMcp,
    secret: String,
    pub url: String,
}

impl Drop for ProxyToken {
    fn drop(&mut self) {
        if let Ok(mut p) = self.mcp.inner.proxies.lock() {
            p.remove(&self.secret);
        }
    }
}

fn new_token() -> String {
    format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple())
}

impl SecretsMcp {
    pub fn start(vault: Vault) -> Result<SecretsMcp, String> {
        let listener = TcpListener::bind(("127.0.0.1", 0)).map_err(|e| format!("secrets MCP bind: {e}"))?;
        let port = listener.local_addr().map_err(|e| e.to_string())?.port();
        let inner = Arc::new(Inner {
            vault,
            tokens: Mutex::default(),
            handles: Mutex::default(),
            data_turns: Mutex::default(),
            proxies: Mutex::default(),
        });
        let srv = inner.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let inner = srv.clone();
                std::thread::spawn(move || serve_connection(stream, &inner));
            }
        });
        Ok(SecretsMcp { inner, port })
    }

    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}/mcp", self.port)
    }

    pub fn data_url(&self) -> String {
        format!("http://127.0.0.1:{}/data", self.port)
    }

    /// A bearer token for `/data` that serves `turn` until dropped.
    pub fn issue_data_token(&self, turn: DataTurn) -> DataToken {
        let token = new_token();
        if let Ok(mut t) = self.inner.data_turns.lock() {
            t.insert(token.clone(), Arc::new(turn));
        }
        DataToken { mcp: self.clone(), token }
    }

    /// A local URL that forwards to `upstream` (a client MCP endpoint under
    /// `api_base`) with the live access token; `token` is the fallback.
    pub fn register_proxy(&self, upstream: &str, api_base: &str, token: &str) -> ProxyToken {
        let secret = new_token();
        if let Ok(mut p) = self.inner.proxies.lock() {
            p.insert(
                secret.clone(),
                ProxyRoute { upstream: upstream.into(), api_base: api_base.into(), token: token.into() },
            );
        }
        let url = format!("http://127.0.0.1:{}/proxy/agentarea/{secret}", self.port);
        ProxyToken { mcp: self.clone(), secret, url }
    }

    pub fn issue_token(&self, thread: &str) -> TurnToken {
        let token = new_token();
        if let Ok(mut t) = self.inner.tokens.lock() {
            t.insert(token.clone(), thread.to_string());
        }
        TurnToken { mcp: self.clone(), token }
    }
}

struct HttpRequest {
    method: String,
    path: String,
    /// lowercased names
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl HttpRequest {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
}

enum ReadErr {
    /// Connection ended (EOF, idle timeout, I/O error): just close.
    Closed,
    Bad(u16, &'static str),
}

fn read_line(r: &mut impl BufRead) -> Result<String, ReadErr> {
    let mut line = String::new();
    match r.by_ref().take(8192).read_line(&mut line) {
        Ok(0) | Err(_) => Err(ReadErr::Closed),
        Ok(_) if !line.ends_with('\n') => Err(ReadErr::Bad(431, "line too long")),
        Ok(_) => Ok(line.trim_end_matches(['\r', '\n']).to_string()),
    }
}

fn read_chunked(r: &mut impl BufRead) -> Result<Vec<u8>, ReadErr> {
    let mut body = Vec::new();
    loop {
        let size_line = read_line(r)?;
        let size = usize::from_str_radix(size_line.split(';').next().unwrap_or("").trim(), 16)
            .map_err(|_| ReadErr::Bad(400, "bad chunk size"))?;
        if size == 0 {
            while !read_line(r)?.is_empty() {}
            return Ok(body);
        }
        if body.len() + size > MAX_REQUEST_BODY {
            return Err(ReadErr::Bad(413, "request body too large"));
        }
        let start = body.len();
        body.resize(start + size, 0);
        r.read_exact(&mut body[start..]).map_err(|_| ReadErr::Closed)?;
        read_line(r)?;
    }
}

/// One HTTP/1.1 request: request line, headers, then a Content-Length or
/// chunked body.
fn read_request(r: &mut impl BufRead) -> Result<HttpRequest, ReadErr> {
    let line = read_line(r)?;
    let mut parts = line.split_whitespace();
    let (Some(method), Some(path)) = (parts.next(), parts.next()) else {
        return Err(ReadErr::Bad(400, "bad request line"));
    };
    let (method, path) = (method.to_string(), path.to_string());
    let mut headers = Vec::new();
    loop {
        let line = read_line(r)?;
        if line.is_empty() {
            break;
        }
        if headers.len() >= 100 {
            return Err(ReadErr::Bad(431, "too many headers"));
        }
        let (k, v) = line.split_once(':').ok_or(ReadErr::Bad(400, "bad header"))?;
        headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
    }
    let mut req = HttpRequest { method, path, headers, body: Vec::new() };
    if req.header("transfer-encoding").is_some_and(|t| t.to_ascii_lowercase().contains("chunked")) {
        req.body = read_chunked(r)?;
    } else if let Some(len) = req.header("content-length") {
        let len: usize = len.parse().map_err(|_| ReadErr::Bad(400, "bad content-length"))?;
        if len > MAX_REQUEST_BODY {
            return Err(ReadErr::Bad(413, "request body too large"));
        }
        req.body = vec![0; len];
        r.read_exact(&mut req.body).map_err(|_| ReadErr::Closed)?;
    }
    Ok(req)
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        202 => "Accepted",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        413 => "Payload Too Large",
        431 => "Request Header Fields Too Large",
        502 => "Bad Gateway",
        _ => "Error",
    }
}

fn write_response(w: &mut impl Write, status: u16, body: Option<&Value>, close: bool) -> std::io::Result<()> {
    let body = body.map(Value::to_string).unwrap_or_default();
    let mut head = format!("HTTP/1.1 {status} {}\r\nContent-Length: {}\r\n", reason(status), body.len());
    if !body.is_empty() {
        head.push_str("Content-Type: application/json\r\n");
    }
    if status == 405 {
        head.push_str("Allow: POST\r\n");
    }
    head.push_str(if close { "Connection: close\r\n\r\n" } else { "Connection: keep-alive\r\n\r\n" });
    w.write_all(head.as_bytes())?;
    w.write_all(body.as_bytes())?;
    w.flush()
}

/// Keep-alive loop: requests on one connection are answered in order.
fn serve_connection(stream: TcpStream, inner: &Inner) {
    let _ = stream.set_read_timeout(Some(IDLE_TIMEOUT));
    let Ok(read_half) = stream.try_clone() else { return };
    let mut reader = BufReader::new(read_half);
    let mut writer = stream;
    loop {
        let (status, body, close) = match read_request(&mut reader) {
            Ok(req) if req.path.starts_with("/proxy/") => {
                let close = req.header("connection").is_some_and(|c| c.eq_ignore_ascii_case("close"));
                match proxy(inner, &req, &mut writer, close) {
                    Ok(()) if !close => continue,
                    Ok(()) => return,
                    Err(Some((status, msg))) => (status, Some(json!({ "error": msg })), close),
                    // The client went away mid-response.
                    Err(None) => return,
                }
            }
            Ok(req) => {
                let close = req.header("connection").is_some_and(|c| c.eq_ignore_ascii_case("close"));
                let (status, body) = route(inner, &req);
                (status, body, close)
            }
            Err(ReadErr::Closed) => return,
            Err(ReadErr::Bad(status, msg)) => (status, Some(json!({ "error": msg })), true),
        };
        if write_response(&mut writer, status, body.as_ref(), close).is_err() || close {
            return;
        }
    }
}

/// A browser page can reach loopback too; only a local origin (or none, as
/// the CLIs send) gets in, on top of the bearer token.
fn local_origin(origin: &str) -> bool {
    ["http://127.0.0.1", "http://localhost"]
        .iter()
        .any(|p| origin == *p || origin.strip_prefix(p).is_some_and(|rest| rest.starts_with(':')))
}

/// Who a bearer token lets in: a thread on `/mcp`, a turn's data on `/data`.
enum Caller {
    Secrets(String),
    Data(Arc<DataTurn>),
}

fn route(inner: &Inner, req: &HttpRequest) -> (u16, Option<Value>) {
    let path = req.path.split('?').next();
    if path != Some("/mcp") && path != Some("/data") {
        return (404, Some(json!({ "error": "not found" })));
    }
    if req.header("origin").is_some_and(|o| !local_origin(o)) {
        return (403, Some(json!({ "error": "origin not allowed" })));
    }
    let token = req.header("authorization").and_then(|a| a.strip_prefix("Bearer ")).unwrap_or("");
    let caller = if path == Some("/mcp") {
        inner.tokens.lock().ok().and_then(|t| t.get(token).cloned()).map(Caller::Secrets)
    } else {
        inner.data_turns.lock().ok().and_then(|t| t.get(token).cloned()).map(Caller::Data)
    };
    let Some(caller) = caller else {
        return (401, Some(json!({ "error": "missing or unknown bearer token" })));
    };
    if req.method != "POST" {
        return (405, None);
    }
    let Ok(msg) = serde_json::from_slice::<Value>(&req.body) else {
        let err = json!({ "jsonrpc": "2.0", "id": null, "error": { "code": -32700, "message": "parse error" } });
        return (400, Some(err));
    };
    if msg.is_array() {
        let err = json!({ "jsonrpc": "2.0", "id": null, "error": { "code": -32600, "message": "batches are not supported" } });
        return (400, Some(err));
    }
    let reply = match &caller {
        Caller::Secrets(thread) => handle_message(inner, thread, &msg),
        Caller::Data(turn) => data_mcp::handle_message(turn, &msg),
    };
    match reply {
        Some(reply) => (200, Some(reply)),
        None => (202, None),
    }
}

// ── AgentArea MCP proxy ──────────────────────────────────────────────────────

/// Request headers the upstream MCP endpoint needs; nothing else goes (no
/// cookies, no Authorization from the CLI).
const PROXY_REQUEST_HEADERS: [&str; 5] = ["content-type", "accept", "mcp-session-id", "mcp-protocol-version", "last-event-id"];
/// Response headers the CLI needs. `WWW-Authenticate` stays behind: the app
/// owns the sign-in, a CLI must not start its own OAuth flow.
const PROXY_RESPONSE_HEADERS: [&str; 5] = ["content-type", "mcp-session-id", "mcp-protocol-version", "cache-control", "retry-after"];

fn proxy_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            // No overall timeout: an SSE stream stays open as long as the CLI wants.
            .connect_timeout(Duration::from_secs(15))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("http client")
    })
}

/// Forward one request to the route's upstream and stream the answer back
/// (chunked). `Err(Some(..))` is a response still to send; `Err(None)` means
/// the client is gone.
fn proxy(inner: &Inner, req: &HttpRequest, w: &mut impl Write, close: bool) -> Result<(), Option<(u16, String)>> {
    if req.header("origin").is_some_and(|o| !local_origin(o)) {
        return Err(Some((403, "origin not allowed".into())));
    }
    let (path, query) = match req.path.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (req.path.as_str(), None),
    };
    let secret = path.strip_prefix("/proxy/agentarea/").unwrap_or("");
    let route = inner.proxies.lock().ok().and_then(|p| p.get(secret).cloned());
    let Some(route) = route else {
        return Err(Some((404, "not found".into())));
    };
    let method = match req.method.as_str() {
        "GET" => reqwest::Method::GET,
        "POST" => reqwest::Method::POST,
        "DELETE" => reqwest::Method::DELETE,
        _ => return Err(Some((405, "method not allowed".into()))),
    };
    let mut url = route.upstream.clone();
    if let Some(q) = query {
        url.push(if url.contains('?') { '&' } else { '?' });
        url.push_str(q);
    }
    let token = data_mcp::live_token(&route.api_base, &route.token);
    let mut builder = proxy_client().request(method, url).bearer_auth(token);
    for (k, v) in &req.headers {
        if PROXY_REQUEST_HEADERS.contains(&k.as_str()) {
            builder = builder.header(k.as_str(), v.as_str());
        }
    }
    if !req.body.is_empty() {
        builder = builder.body(req.body.clone());
    }
    tauri::async_runtime::block_on(async move {
        let mut res = builder
            .send()
            .await
            .map_err(|e| Some((502, format!("AgentArea is unreachable: {}", e.without_url()))))?;
        let status = res.status().as_u16();
        let mut head = format!("HTTP/1.1 {status} {}\r\n", res.status().canonical_reason().unwrap_or("Status"));
        for (k, v) in res.headers() {
            if PROXY_RESPONSE_HEADERS.contains(&k.as_str()) {
                if let Ok(v) = v.to_str() {
                    head.push_str(&format!("{k}: {v}\r\n"));
                }
            }
        }
        let bodiless = status == 204 || status == 304;
        head.push_str(if bodiless { "Content-Length: 0\r\n" } else { "Transfer-Encoding: chunked\r\n" });
        head.push_str(if close { "Connection: close\r\n\r\n" } else { "Connection: keep-alive\r\n\r\n" });
        w.write_all(head.as_bytes()).and_then(|_| w.flush()).map_err(|_| None)?;
        if bodiless {
            return Ok(());
        }
        // Each upstream chunk goes out as it comes: SSE events must not wait.
        loop {
            let chunk = match res.chunk().await {
                Ok(Some(c)) => c,
                Ok(None) => break,
                // Upstream broke mid-body; end the connection so the CLI notices.
                Err(_) => return Err(None),
            };
            if chunk.is_empty() {
                continue;
            }
            write!(w, "{:x}\r\n", chunk.len())
                .and_then(|_| w.write_all(&chunk))
                .and_then(|_| w.write_all(b"\r\n"))
                .and_then(|_| w.flush())
                .map_err(|_| None)?;
        }
        w.write_all(b"0\r\n\r\n").and_then(|_| w.flush()).map_err(|_| None)
    })
}

// ── MCP ──────────────────────────────────────────────────────────────────────

/// A reply for a request; `None` for notifications and client responses.
fn handle_message(inner: &Inner, thread: &str, msg: &Value) -> Option<Value> {
    let method = msg.get("method").and_then(Value::as_str)?;
    let id = msg.get("id")?.clone();
    let params = msg.get("params").cloned().unwrap_or_else(|| json!({}));
    let result = match method {
        "initialize" => Ok(initialize_result(&params)),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tool_defs() })),
        "tools/call" => call_tool(inner, thread, &params),
        other => Err((-32601, format!("{other} is not supported"))),
    };
    Some(match result {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Err((code, message)) => json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }),
    })
}

fn initialize_result(params: &Value) -> Value {
    let asked = params["protocolVersion"].as_str().unwrap_or("");
    let version = PROTOCOL_VERSIONS.iter().find(|v| **v == asked).unwrap_or(&PROTOCOL_VERSIONS[1]);
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": {} },
        "serverInfo": { "name": "agentarea-secrets", "version": env!("CARGO_PKG_VERSION") },
        "instructions": "Secrets from the user's AgentArea vault. You never see a value: request_secret gives a one-time handle, and http_request fills it in for an allowed host. [secret:NAME] marks a hidden value."
    })
}

fn tool_defs() -> Value {
    json!([
        {
            "name": "list_secrets",
            "description": "List the secrets in the user's AgentArea vault: name, description and the hosts each may be sent to. Values are never shown.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "request_secret",
            "description": "Get a one-time handle (aas_…) for a vault secret. The handle works once, for 10 minutes, only inside http_request to one of the secret's allowed hosts. You never see the value.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Secret name, e.g. GITHUB_TOKEN" },
                    "purpose": { "type": "string", "description": "What you will do with it (recorded)" }
                },
                "required": ["name", "purpose"]
            }
        },
        {
            "name": "http_request",
            "description": "Send an HTTPS request that uses vault secrets. Put handles from request_secret where the secret goes: a header value (e.g. \"Authorization\": \"Bearer aas_…\"), the URL query, or the body. The value is filled in only for the secret's allowed hosts; each handle works once. Secret values in the response appear as [secret:NAME]. Redirects are not followed.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "method": { "type": "string", "enum": ["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD"], "default": "GET" },
                    "url": { "type": "string" },
                    "headers": { "type": "object", "additionalProperties": { "type": "string" } },
                    "body": { "description": "String, or a JSON value sent as application/json" }
                },
                "required": ["url"]
            }
        }
    ])
}

fn call_tool(inner: &Inner, thread: &str, params: &Value) -> Result<Value, (i64, String)> {
    let name = params["name"].as_str().ok_or((-32602, "tool name is required".to_string()))?;
    let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
    let out = match name {
        "list_secrets" => list_secrets(inner),
        "request_secret" => request_secret(inner, thread, &args),
        "http_request" => http_request(inner, thread, &args),
        other => return Err((-32602, format!("unknown tool: {other}"))),
    };
    let (text, is_error) = match out {
        Ok(v) => (serde_json::to_string_pretty(&v).unwrap_or_default(), false),
        Err(e) => (e, true),
    };
    Ok(json!({ "content": [{ "type": "text", "text": text }], "isError": is_error }))
}

fn list_secrets(inner: &Inner) -> Result<Value, String> {
    let secrets = inner.vault.list()?;
    Ok(json!(secrets
        .iter()
        .map(|s| json!({ "name": s.name, "description": s.description, "allowedHosts": s.allowed_hosts }))
        .collect::<Vec<_>>()))
}

fn request_secret(inner: &Inner, thread: &str, args: &Value) -> Result<Value, String> {
    let name = args["name"].as_str().unwrap_or("").trim();
    let purpose = args["purpose"].as_str().unwrap_or("").trim();
    if purpose.is_empty() {
        return Err("purpose is required: say what you will do with the secret".into());
    }
    let purpose: String = purpose.chars().take(500).collect();
    let meta = inner
        .vault
        .list()?
        .into_iter()
        .find(|s| s.name == name)
        .ok_or_else(|| format!("no secret named {name:?}; list_secrets shows what exists"))?;
    if meta.allowed_hosts.is_empty() {
        return Err(format!("{name} has no allowed hosts; the user can add some in AgentArea → Secrets"));
    }
    let handle = inner.handles.lock().map_err(|_| "poisoned")?.issue(thread, &meta, &purpose, Instant::now());
    Ok(json!({
        "handle": handle,
        "secret": meta.name,
        "expiresInSeconds": HANDLE_TTL.as_secs(),
        "allowedHosts": meta.allowed_hosts,
        "usage": "Put the handle where the secret goes (a header value, the URL query or the body) in one http_request to an allowed host. It works once."
    }))
}

struct Outbound {
    method: reqwest::Method,
    url: Url,
    headers: Vec<(String, String)>,
    body: Option<String>,
}

struct Inbound {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    truncated: bool,
}

/// Response headers worth showing the agent; the rest is noise.
fn keep_header(name: &str) -> bool {
    ["content-type", "content-length", "location", "retry-after", "www-authenticate", "x-request-id"].contains(&name)
        || name.starts_with("x-ratelimit")
}

async fn send(req: Outbound) -> Result<Inbound, String> {
    let client = reqwest::Client::builder()
        .timeout(OUTBOUND_TIMEOUT)
        // A redirect could carry the secret (in a header, query or body) to
        // another host; the agent sees the 3xx and decides.
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| e.to_string())?;
    let mut builder = client.request(req.method, req.url);
    for (k, v) in req.headers {
        builder = builder.header(k, v);
    }
    if let Some(body) = req.body {
        builder = builder.body(body);
    }
    let mut res = builder.send().await.map_err(|e| format!("request failed: {}", e.without_url()))?;
    let status = res.status().as_u16();
    let headers = res
        .headers()
        .iter()
        .filter(|(k, _)| keep_header(k.as_str()))
        .map(|(k, v)| (k.to_string(), String::from_utf8_lossy(v.as_bytes()).into_owned()))
        .collect();
    let mut body = Vec::new();
    let mut truncated = false;
    while let Some(chunk) = res.chunk().await.map_err(|e| format!("reading response: {}", e.without_url()))? {
        let room = MAX_RESPONSE_BODY - body.len();
        if chunk.len() > room {
            body.extend_from_slice(&chunk[..room]);
            truncated = true;
            break;
        }
        body.extend_from_slice(&chunk);
    }
    Ok(Inbound { status, headers, body, truncated })
}

fn http_request(inner: &Inner, thread: &str, args: &Value) -> Result<Value, String> {
    let method = args["method"].as_str().unwrap_or("GET").to_ascii_uppercase();
    if !["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD"].contains(&method.as_str()) {
        return Err(format!("method {method} is not supported"));
    }
    let method = reqwest::Method::from_bytes(method.as_bytes()).map_err(|e| e.to_string())?;
    let mut url = Url::parse(args["url"].as_str().unwrap_or("")).map_err(|e| format!("bad url: {e}"))?;
    let mut headers: Vec<(String, String)> = match &args["headers"] {
        Value::Null => Vec::new(),
        Value::Object(m) => m
            .iter()
            .map(|(k, v)| (k.clone(), v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string())))
            .collect(),
        _ => return Err("headers must be an object of name → value".into()),
    };
    if headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("host")) {
        return Err("the Host header can't be set".into());
    }
    // A JSON body gets JSON-escaped values, so a quote in a secret can't break it.
    let (mut body, mut json_body) = match &args["body"] {
        Value::Null => (None, false),
        Value::String(s) => (Some(s.clone()), false),
        v => (Some(v.to_string()), true),
    };
    let content_type = headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("content-type")).map(|(_, v)| v.clone());
    match &content_type {
        Some(ct) => json_body |= ct.to_ascii_lowercase().contains("json"),
        None if json_body => headers.push(("Content-Type".into(), "application/json".into())),
        None => {}
    }

    // Where handles may be: header values, the query, the body. Anywhere else
    // in the URL (host, path, userinfo) the value would leak to DNS or logs.
    let mut bare = url.clone();
    bare.set_query(None);
    if !find_handles(bare.as_str()).is_empty() || headers.iter().any(|(k, _)| !find_handles(k).is_empty()) {
        return Err("handles may only appear in a header value, the URL query or the body".into());
    }
    let mut found = find_handles(url.query().unwrap_or(""));
    for text in headers.iter().map(|(_, v)| v.as_str()).chain(body.as_deref()) {
        for h in find_handles(text) {
            if !found.contains(&h) {
                found.push(h);
            }
        }
    }
    if found.is_empty() {
        return Err("no handle found: http_request only sends secrets. Call request_secret and put its handle where the secret goes.".into());
    }

    // Check every handle before spending any; then spend them all at once.
    let metas = inner.vault.list()?;
    let mut exchanged: Vec<(String, SecretMeta, String)> = Vec::new();
    {
        let mut handles = inner.handles.lock().map_err(|_| "poisoned")?;
        let now = Instant::now();
        for h in &found {
            let handle = handles.check(h, thread, now)?;
            let meta = metas
                .iter()
                .find(|m| m.id == handle.secret_id)
                .ok_or("the secret behind this handle was deleted")?;
            host_allowed(&url, &meta.allowed_hosts).map_err(|e| format!("{}: {e}", meta.name))?;
            exchanged.push((h.clone(), meta.clone(), handle.purpose.clone()));
        }
        for h in &found {
            handles.mark_used(h);
        }
    }

    // All vault values: the response is scrubbed of every one, not just these.
    let values = inner.vault.values()?;
    let redactor = Redactor::new(values.iter().map(|(m, v)| (m.name.as_str(), v.as_str())));
    for (handle, meta, _) in &exchanged {
        let value = values
            .iter()
            .find(|(m, _)| m.id == meta.id)
            .map(|(_, v)| v.as_str())
            .ok_or_else(|| format!("{} has no value in the keychain", meta.name))?;
        if let Some(q) = url.query().map(str::to_string) {
            url.set_query(Some(&q.replace(handle.as_str(), &percent_encode(value))));
        }
        for (_, v) in headers.iter_mut() {
            *v = v.replace(handle.as_str(), value);
        }
        if let Some(b) = body.as_mut() {
            let escaped;
            let with = if json_body {
                let s = serde_json::to_string(value).map_err(|e| e.to_string())?;
                escaped = s[1..s.len() - 1].to_string();
                escaped.as_str()
            } else {
                value
            };
            *b = b.replace(handle.as_str(), with);
        }
    }
    for (_, meta, purpose) in &exchanged {
        let _ = inner.vault.touch(&meta.id, purpose);
    }

    let res = tauri::async_runtime::block_on(send(Outbound { method, url, headers, body }))
        .map_err(|e| redactor.redact(&e))?;
    let mut out = json!({
        "status": res.status,
        "headers": res
            .headers
            .iter()
            .map(|(k, v)| (k.clone(), Value::String(redactor.redact(v))))
            .collect::<serde_json::Map<_, _>>(),
        "body": redactor.redact(&String::from_utf8_lossy(&res.body)),
        "secretsUsed": exchanged.iter().map(|(_, m, _)| m.name.clone()).collect::<Vec<_>>(),
    });
    if res.truncated {
        out["truncated"] = json!(format!("body cut at {} bytes", MAX_RESPONSE_BODY));
    }
    // Once more over the whole thing, for values split across fields' escaping.
    serde_json::from_str(&redactor.redact(&out.to_string())).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::path::PathBuf;

    fn temp_vault() -> (Vault, PathBuf) {
        let dir = std::env::temp_dir().join(format!("aa-secrets-mcp-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        (Vault::at(dir.clone(), "AgentArea Desktop Secrets (test)"), dir)
    }

    fn meta(id: &str, name: &str, hosts: &[&str]) -> SecretMeta {
        SecretMeta {
            id: id.into(),
            name: name.into(),
            description: None,
            allowed_hosts: hosts.iter().map(|h| h.to_string()).collect(),
            created_at: 0,
            last_used_at: None,
            last_purpose: None,
        }
    }

    #[test]
    fn handle_is_single_use() {
        let mut hs = Handles::default();
        let now = Instant::now();
        let h = hs.issue("t1", &meta("s1", "KEY", &["x.io"]), "test", now);
        assert!(h.starts_with("aas_") && h.len() == 36, "{h}");
        assert_eq!(find_handles(&h), [h.clone()]);
        assert_eq!(hs.check(&h, "t1", now).unwrap().secret_id, "s1");
        hs.mark_used(&h);
        assert!(hs.check(&h, "t1", now).unwrap_err().contains("already used"));
    }

    #[test]
    fn handle_expires() {
        let mut hs = Handles::default();
        let now = Instant::now();
        let h = hs.issue("t1", &meta("s1", "KEY", &["x.io"]), "test", now);
        assert!(hs.check(&h, "t1", now + Duration::from_secs(599)).is_ok());
        assert!(hs.check(&h, "t1", now + HANDLE_TTL).unwrap_err().contains("expired"));
        // Expired handles are dropped on the next issue.
        hs.issue("t1", &meta("s1", "KEY", &["x.io"]), "test", now + HANDLE_TTL + Duration::from_secs(1));
        assert!(!hs.0.contains_key(&h));
    }

    #[test]
    fn handle_is_bound_to_its_thread() {
        let mut hs = Handles::default();
        let now = Instant::now();
        let h = hs.issue("t1", &meta("s1", "KEY", &["x.io"]), "test", now);
        assert!(hs.check(&h, "t2", now).unwrap_err().contains("not a valid handle"));
        assert!(hs.check("aas_00000000000000000000000000000000", "t1", now).unwrap_err().contains("not a valid"));
        assert!(hs.check(&h, "t1", now).is_ok(), "a wrong-thread attempt doesn't burn it");
    }

    #[test]
    fn finds_handles() {
        let a = "aas_0123456789abcdef0123456789abcdef";
        let b = "aas_ffffffffffffffffffffffffffffffff";
        let text = format!("Bearer {a} and ?k={b}&again={a} aas_short aas_0123456789ABCDEF0123456789ABCDEF");
        assert_eq!(find_handles(&text), [a, b]);
        assert!(find_handles("nothing here").is_empty());
    }

    #[test]
    fn allows_only_listed_hosts_over_https() {
        let hosts = vec!["api.github.com".to_string(), "*.example.com".to_string(), "localhost".to_string()];
        let ok = |u: &str| host_allowed(&Url::parse(u).unwrap(), &hosts);
        assert!(ok("https://api.github.com/user").is_ok());
        assert!(ok("https://API.GITHUB.COM/user").is_ok());
        assert!(ok("https://a.example.com/").is_ok());
        assert!(ok("https://a.b.example.com:8443/").is_ok());
        assert!(ok("http://localhost:3000/").is_ok(), "http to localhost is fine");
        assert!(ok("https://example.com/").unwrap_err().contains("not one of"), "wildcard excludes the apex");
        assert!(ok("https://evilexample.com/").is_err());
        assert!(ok("https://api.github.com.evil.io/").is_err());
        assert!(ok("http://api.github.com/").unwrap_err().contains("https only"));
        assert!(ok("ftp://api.github.com/").is_err());
        assert!(ok("http://127.0.0.1/").unwrap_err().contains("not one of"), "local still needs to be listed");
        assert!(host_allowed(&Url::parse("https://x.io").unwrap(), &[]).unwrap_err().contains("no allowed hosts"));
    }

    fn inner(vault: Vault) -> Inner {
        Inner {
            vault,
            tokens: Mutex::default(),
            handles: Mutex::default(),
            data_turns: Mutex::default(),
            proxies: Mutex::default(),
        }
    }

    #[test]
    fn dispatches_json_rpc() {
        let (vault, dir) = temp_vault();
        let inner = inner(vault);
        let init = handle_message(
            &inner,
            "t1",
            &json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{}}}),
        )
        .unwrap();
        assert_eq!(init["id"], 1);
        assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(init["result"]["serverInfo"]["name"], "agentarea-secrets");
        assert!(init["result"]["capabilities"]["tools"].is_object());
        let old = handle_message(&inner, "t1", &json!({"jsonrpc":"2.0","id":2,"method":"initialize","params":{"protocolVersion":"1999-01-01"}}));
        assert_eq!(old.unwrap()["result"]["protocolVersion"], "2025-06-18");

        let tools = handle_message(&inner, "t1", &json!({"jsonrpc":"2.0","id":"x","method":"tools/list"})).unwrap();
        let names: Vec<_> = tools["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["list_secrets", "request_secret", "http_request"]);

        assert_eq!(handle_message(&inner, "t1", &json!({"jsonrpc":"2.0","id":3,"method":"ping"})).unwrap()["result"], json!({}));
        assert!(handle_message(&inner, "t1", &json!({"jsonrpc":"2.0","method":"notifications/initialized"})).is_none());
        assert!(handle_message(&inner, "t1", &json!({"jsonrpc":"2.0","id":9,"result":{}})).is_none());
        let unknown = handle_message(&inner, "t1", &json!({"jsonrpc":"2.0","id":4,"method":"resources/list"})).unwrap();
        assert_eq!(unknown["error"]["code"], -32601);
        let bad_tool = handle_message(&inner, "t1", &json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"nope"}})).unwrap();
        assert_eq!(bad_tool["error"]["code"], -32602);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// list_secrets / request_secret read only secrets.json (no keychain).
    #[test]
    fn lists_and_hands_out_without_values() {
        let (vault, dir) = temp_vault();
        std::fs::write(
            dir.join("secrets.json"),
            r#"[{"id":"s1","name":"GH_TOKEN","description":"repo","allowedHosts":["api.github.com"],"createdAt":1},
                {"id":"s2","name":"NO_HOSTS","createdAt":1}]"#,
        )
        .unwrap();
        let inner = inner(vault);
        let call = |name: &str, args: Value| {
            let r = handle_message(&inner, "t1", &json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":name,"arguments":args}})).unwrap();
            let r = &r["result"];
            (r["content"][0]["text"].as_str().unwrap().to_string(), r["isError"] == true)
        };
        let (text, err) = call("list_secrets", json!({}));
        assert!(!err);
        let list: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(list[0], json!({"name":"GH_TOKEN","description":"repo","allowedHosts":["api.github.com"]}));

        let (text, err) = call("request_secret", json!({"name":"GH_TOKEN","purpose":"list repos"}));
        assert!(!err, "{text}");
        let handle = serde_json::from_str::<Value>(&text).unwrap()["handle"].as_str().unwrap().to_string();
        assert!(inner.handles.lock().unwrap().check(&handle, "t1", Instant::now()).is_ok());

        assert!(call("request_secret", json!({"name":"GH_TOKEN"})).1, "purpose required");
        assert!(call("request_secret", json!({"name":"MISSING","purpose":"x"})).0.contains("no secret named"));
        assert!(call("request_secret", json!({"name":"NO_HOSTS","purpose":"x"})).0.contains("no allowed hosts"));
        // Rejected before any keychain read: host not allowed, handle outside the query, no handle.
        let (text, err) = call("http_request", json!({"url":"https://evil.io/","headers":{"Authorization":format!("Bearer {handle}")}}));
        assert!(err && text.contains("not one of"), "{text}");
        let (text, _) = call("http_request", json!({"url":format!("https://api.github.com/{handle}")}));
        assert!(text.contains("may only appear"), "{text}");
        let (text, _) = call("http_request", json!({"url":"https://api.github.com/"}));
        assert!(text.contains("no handle found"), "{text}");
        assert!(inner.handles.lock().unwrap().check(&handle, "t1", Instant::now()).is_ok(), "rejections don't spend it");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Raw HTTP to the running server: auth, methods, notifications.
    fn post(url: &str, token: Option<&str>, method: &str, body: &str) -> (u16, String) {
        let (addr, path) = url.trim_start_matches("http://").split_once('/').unwrap();
        let mut s = TcpStream::connect(addr).unwrap();
        let auth = token.map(|t| format!("Authorization: Bearer {t}\r\n")).unwrap_or_default();
        write!(s, "{method} /{path} HTTP/1.1\r\nHost: x\r\n{auth}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).unwrap();
        let status = out.split_whitespace().nth(1).unwrap().parse().unwrap();
        (status, out.split("\r\n\r\n").nth(1).unwrap_or("").to_string())
    }

    #[test]
    fn serves_streamable_http() {
        let (vault, dir) = temp_vault();
        let mcp = SecretsMcp::start(vault).unwrap();
        let url = mcp.url();
        let init = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#;
        assert_eq!(post(&url, None, "POST", init).0, 401);
        assert_eq!(post(&url, Some("nope"), "POST", init).0, 401);
        let turn = mcp.issue_token("t1");
        let (status, body) = post(&url, Some(&turn.token), "POST", init);
        assert_eq!(status, 200);
        assert_eq!(serde_json::from_str::<Value>(&body).unwrap()["result"]["serverInfo"]["name"], "agentarea-secrets");
        assert_eq!(post(&url, Some(&turn.token), "POST", r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#).0, 202);
        assert_eq!(post(&url, Some(&turn.token), "GET", "").0, 405);
        assert_eq!(post(&url, Some(&turn.token), "POST", "not json").0, 400);
        assert_eq!(post(&url, Some(&turn.token), "POST", "[]").0, 400);
        let token = turn.token.clone();
        drop(turn);
        assert_eq!(post(&url, Some(&token), "POST", init).0, 401, "revoked when the turn ends");

        // /data takes only data tokens, and vice versa.
        let data = mcp.issue_data_token(DataTurn::new(vec![], None, Redactor::default()));
        let secrets = mcp.issue_token("t1");
        let data_url = mcp.data_url();
        let (status, body) = post(&data_url, Some(&data.token), "POST", init);
        assert_eq!(status, 200);
        assert_eq!(serde_json::from_str::<Value>(&body).unwrap()["result"]["serverInfo"]["name"], "agentarea-data");
        assert_eq!(post(&data_url, Some(&secrets.token), "POST", init).0, 401);
        assert_eq!(post(&url, Some(&data.token), "POST", init).0, 401);
        let token = data.token.clone();
        drop(data);
        assert_eq!(post(&data_url, Some(&token), "POST", init).0, 401, "data token revoked with the turn");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// What the fake AgentArea upstream saw of a proxied request.
    struct Seen {
        path: String,
        auth: String,
        session: String,
        body: String,
    }

    /// An SSE-answering fake of `<api>/client-mcp/<id>`: sends one event, waits
    /// for the test's go, then a second one — so the test can tell streaming
    /// from buffering.
    fn sse_upstream(requests: usize) -> (u16, std::sync::mpsc::Receiver<Seen>, std::sync::mpsc::Sender<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (seen_tx, seen_rx) = std::sync::mpsc::channel();
        let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
        std::thread::spawn(move || {
            for _ in 0..requests {
                let (s, _) = listener.accept().unwrap();
                let mut r = BufReader::new(s.try_clone().unwrap());
                let req = read_request(&mut r).ok().unwrap();
                let _ = seen_tx.send(Seen {
                    path: req.path.clone(),
                    auth: req.header("authorization").unwrap_or("").into(),
                    session: req.header("mcp-session-id").unwrap_or("").into(),
                    body: String::from_utf8_lossy(&req.body).into(),
                });
                let mut w = s;
                let chunk = |w: &mut TcpStream, text: &str| {
                    write!(w, "{:x}\r\n{text}\r\n", text.len()).unwrap();
                    w.flush().unwrap();
                };
                write!(w, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nMcp-Session-Id: sess-42\r\nWWW-Authenticate: Bearer realm=\"x\"\r\nSet-Cookie: a=b\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n").unwrap();
                chunk(&mut w, "event: message\ndata: {\"n\":1}\n\n");
                go_rx.recv_timeout(Duration::from_secs(10)).unwrap();
                chunk(&mut w, "event: message\ndata: {\"n\":2}\n\n");
                w.write_all(b"0\r\n\r\n").unwrap();
            }
        });
        (port, seen_rx, go_tx)
    }

    /// Read from `s` until `needle` shows up; everything read so far.
    fn read_until(s: &mut TcpStream, got: &mut String, needle: &str) {
        let mut buf = [0u8; 4096];
        while !got.contains(needle) {
            let n = s.read(&mut buf).unwrap();
            assert!(n > 0, "connection closed before {needle:?}; got {got}");
            got.push_str(&String::from_utf8_lossy(&buf[..n]));
        }
    }

    /// `/proxy/agentarea/<secret>`: the CLI's request goes upstream with the
    /// app's live token (not whatever the CLI sent), Mcp-Session-Id passes both
    /// ways, SSE events arrive one by one, and a renewed token is used at once.
    #[test]
    fn proxies_agentarea_mcp_with_the_live_token() {
        let (vault, dir) = temp_vault();
        let mcp = SecretsMcp::start(vault).unwrap();
        let (port, seen, go) = sse_upstream(2);
        let api_base = format!("http://127.0.0.1:{port}");
        data_mcp::set_cloud_token(api_base.clone(), Some("live-1".into()));
        let route = mcp.register_proxy(&format!("{api_base}/client-mcp/c1"), &api_base, "stale-start-token");
        let (addr, path) = route.url.trim_start_matches("http://").split_once('/').unwrap();
        let (addr, path) = (addr.to_string(), format!("/{path}"));

        let body = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
        let request = |extra: &str| {
            format!(
                "POST {path}?probe=1 HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer from-the-cli\r\nCookie: c=d\r\nMcp-Session-Id: sess-41\r\nAccept: application/json, text/event-stream\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{extra}\r\n{body}",
                body.len()
            )
        };
        let mut s = TcpStream::connect(&addr).unwrap();
        s.write_all(request("Connection: close\r\n").as_bytes()).unwrap();
        let mut got = String::new();
        read_until(&mut s, &mut got, "\"n\":1");
        assert!(!got.contains("\"n\":2"), "second event not sent yet: {got}");
        let up = seen.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(up.auth, "Bearer live-1", "live token, not the CLI's or the stale one");
        assert_eq!(up.session, "sess-41");
        assert_eq!(up.path, "/client-mcp/c1?probe=1");
        assert_eq!(up.body, body);
        go.send(()).unwrap();
        read_until(&mut s, &mut got, "0\r\n\r\n");
        println!("proxied response as the CLI sees it:\n{got}");
        let head = got.split("\r\n\r\n").next().unwrap().to_ascii_lowercase();
        assert!(head.starts_with("http/1.1 200"), "{head}");
        assert!(head.contains("content-type: text/event-stream"), "{head}");
        assert!(head.contains("mcp-session-id: sess-42"), "{head}");
        assert!(head.contains("transfer-encoding: chunked"), "{head}");
        assert!(!head.contains("www-authenticate") && !head.contains("set-cookie"), "{head}");
        assert!(got.contains("\"n\":2"));

        // The frontend renews the token; the next request carries the new one.
        data_mcp::set_cloud_token(api_base.clone(), Some("live-2".into()));
        let mut s = TcpStream::connect(&addr).unwrap();
        s.write_all(request("Connection: close\r\n").as_bytes()).unwrap();
        let mut got = String::new();
        read_until(&mut s, &mut got, "\"n\":1");
        assert_eq!(seen.recv_timeout(Duration::from_secs(5)).unwrap().auth, "Bearer live-2");
        go.send(()).unwrap();
        read_until(&mut s, &mut got, "0\r\n\r\n");

        // Unknown or dropped routes are gone; other methods aren't forwarded.
        let bogus = format!("http://{addr}/proxy/agentarea/nope");
        assert_eq!(post(&bogus, None, "POST", body).0, 404);
        assert_eq!(post(&route.url, None, "PUT", body).0, 405);
        let url = route.url.clone();
        drop(route);
        assert_eq!(post(&url, None, "POST", body).0, 404, "route removed with its process");
        data_mcp::set_cloud_token(api_base, None);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A one-request HTTP server that answers with the Authorization header it
    /// got (and hands it to the test).
    fn echo_server() -> (u16, std::sync::mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let (s, _) = listener.accept().unwrap();
            let mut r = BufReader::new(s.try_clone().unwrap());
            let req = read_request(&mut r).ok().unwrap();
            let auth = req.header("authorization").unwrap_or("").to_string();
            let _ = tx.send(auth.clone());
            let body = format!("{{\"youSent\":\"{auth}\"}}");
            let mut w = s;
            write!(w, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        });
        (port, rx)
    }

    /// End to end over HTTP with a real keychain entry: tools/list →
    /// request_secret → http_request to a local echo server. The server gets
    /// the value, the agent sees `[secret:NAME]`, and the handle works once.
    /// `cargo test -- --ignored e2e_`.
    #[test]
    #[ignore]
    fn e2e_exchange_through_http() {
        let (vault, dir) = temp_vault();
        let value = format!("sk-test-{}", uuid::Uuid::new_v4().simple());
        let secret = vault.create("E2E_TOKEN".into(), None, vec!["127.0.0.1".into()], value.clone()).unwrap();
        let mcp = SecretsMcp::start(vault.clone()).unwrap();
        let turn = mcp.issue_token("thread-1");
        let rpc = |id: u64, method: &str, params: Value| -> Value {
            let body = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}).to_string();
            let (status, body) = post(&mcp.url(), Some(&turn.token), "POST", &body);
            assert_eq!(status, 200, "{body}");
            serde_json::from_str(&body).unwrap()
        };
        let tool = |id: u64, name: &str, args: Value| -> (String, bool) {
            let r = rpc(id, "tools/call", json!({"name":name,"arguments":args}));
            (r["result"]["content"][0]["text"].as_str().unwrap().to_string(), r["result"]["isError"] == true)
        };

        let tools = rpc(1, "tools/list", json!({}));
        assert_eq!(tools["result"]["tools"].as_array().unwrap().len(), 3);
        let (list, _) = tool(2, "list_secrets", json!({}));
        assert!(list.contains("E2E_TOKEN") && !list.contains(&value));

        let (text, err) = tool(3, "request_secret", json!({"name":"E2E_TOKEN","purpose":"e2e test"}));
        assert!(!err, "{text}");
        let handle = serde_json::from_str::<Value>(&text).unwrap()["handle"].as_str().unwrap().to_string();

        let (port, received) = echo_server();
        let args = json!({"method":"GET","url":format!("http://127.0.0.1:{port}/echo"),"headers":{"Authorization":format!("Bearer {handle}")}});
        let (text, err) = tool(4, "http_request", args.clone());
        assert!(!err, "{text}");
        assert_eq!(received.recv_timeout(Duration::from_secs(5)).unwrap(), format!("Bearer {value}"), "server got the real value");
        assert!(!text.contains(&value), "agent never sees the value: {text}");
        assert!(text.contains("[secret:E2E_TOKEN]"), "{text}");
        let result: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(result["status"], 200);
        println!("http_request result as the agent sees it:\n{text}");

        let (text, err) = tool(5, "http_request", args);
        assert!(err && text.contains("already used"), "second use fails: {text}");
        let used = &vault.list().unwrap()[0];
        assert!(used.last_used_at.is_some() && used.last_purpose.as_deref() == Some("e2e test"), "use recorded");

        vault.delete(&secret.id).unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A real agent CLI against this server, with run_agent's isolation flags:
    /// it lists secrets, exchanges a handle in http_request to a local echo
    /// server, and never prints the value. Needs signed-in CLIs:
    /// `cargo test -- --ignored cli_ --nocapture`.
    fn cli_exchange(runner: &str) {
        let (vault, dir) = temp_vault();
        let value = format!("sk-cli-{}", uuid::Uuid::new_v4().simple());
        let secret = vault
            .create("CLI_TEST_TOKEN".into(), Some("test token".into()), vec!["127.0.0.1".into()], value.clone())
            .unwrap();
        let mcp = SecretsMcp::start(vault.clone()).unwrap();
        let turn = mcp.issue_token("thread-cli");
        let (port, received) = echo_server();
        let mut cmd = std::process::Command::new(crate::cli_bin(runner));
        if runner == "claude" {
            let cfg = json!({ "mcpServers": { "agentarea_secrets": {
                "type": "http", "url": mcp.url(), "headers": { "Authorization": format!("Bearer {}", turn.token) },
            }}});
            let cfg_path = dir.join("mcp.json");
            crate::write_private(&cfg_path, &cfg.to_string()).unwrap();
            cmd.args(["-p", "--output-format", "stream-json", "--verbose"])
                .args(["--strict-mcp-config", "--setting-sources", "project"])
                .args(["--permission-mode", "acceptEdits"])
                .args(["--allowedTools", "Read", "Glob", "Grep", "Edit", "Write"])
                .arg("--mcp-config")
                .arg(&cfg_path)
                .args(["--allowedTools", "mcp__agentarea_secrets"]);
        } else {
            // A private CODEX_HOME with only the login, as run_agent sets up.
            let home = dir.join("codex-home");
            std::fs::create_dir_all(&home).unwrap();
            let auth = PathBuf::from(std::env::var("HOME").unwrap()).join(".codex/auth.json");
            std::os::unix::fs::symlink(&auth, home.join("auth.json")).unwrap();
            cmd.env("CODEX_HOME", &home)
                .env("AGENTAREA_SECRETS_TOKEN", &turn.token)
                .args(["exec", "--json", "--skip-git-repo-check"])
                .args(["--disable", "apps", "--disable", "plugins", "--disable", "hooks"])
                .args(["-c", "sandbox_mode=\"workspace-write\""])
                .arg("-c")
                .arg(format!("mcp_servers.agentarea_secrets.url={}", json!(mcp.url())))
                .args(["-c", "mcp_servers.agentarea_secrets.bearer_token_env_var=\"AGENTAREA_SECRETS_TOKEN\""])
                .args(["-c", "mcp_servers.agentarea_secrets.default_tools_approval_mode=\"approve\""])
                .arg("-");
        }
        let mut child = cmd
            .current_dir(&dir)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        // The prompt goes in on stdin, as in run_agent (--allowedTools is variadic).
        let prompt = format!(
            "List the available secrets using your tools. Then use CLI_TEST_TOKEN as a bearer token in a GET to http://127.0.0.1:{port}/echo and tell me the response body."
        );
        child.stdin.take().unwrap().write_all(prompt.as_bytes()).unwrap();
        let out = child.wait_with_output().unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        println!("{stdout}\n--- stderr ---\n{}", String::from_utf8_lossy(&out.stderr));
        let got = received.recv_timeout(Duration::from_secs(1));
        vault.delete(&secret.id).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(stdout.contains("list_secrets") && stdout.contains("CLI_TEST_TOKEN"), "listed");
        assert!(stdout.contains("request_secret") && stdout.contains("http_request"), "exchanged");
        assert_eq!(got.ok(), Some(format!("Bearer {value}")), "echo server got the real value");
        assert!(stdout.contains("[secret:CLI_TEST_TOKEN]"), "agent saw the marker");
        assert!(!stdout.contains(&value), "value never printed");
    }

    #[test]
    #[ignore]
    fn cli_claude_exchanges_a_secret() {
        cli_exchange("claude");
    }

    #[test]
    #[ignore]
    fn cli_codex_exchanges_a_secret() {
        cli_exchange("codex");
    }
}
