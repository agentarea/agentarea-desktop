//! Local MCP servers: stdio processes the desktop runs itself — or MCP servers
//! already listening on a URL (streamable HTTP, e.g. one in development on
//! localhost) — so MCP Apps work on this device without an AgentArea sign-in.
//!
//! What is installed is data — `~/AgentArea/mcp/servers.json`, an array of
//! server specs the UI (or an agent, or the user) writes. Running servers live
//! only in memory: each is a child process speaking JSON-RPC 2.0 over stdio
//! (one JSON message per line). A reader thread per child routes responses to
//! the waiting request by id; stderr is drained on its own thread and its tail
//! kept for error display. Children are killed on stop and on app exit.

use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::State;

const PROTOCOL_VERSION: &str = "2025-06-18";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
/// `npx -y` downloads the package on first start, which can take a while.
const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(180);
const STDERR_TAIL: usize = 4000;

#[derive(Clone, Copy, Debug, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    Recommended,
    Registry,
    Custom,
}

/// One entry of servers.json.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct ServerSpec {
    id: String,
    name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    icons: Vec<String>,
    source: Source,
    /// Registry base URL the server was installed from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    registry: Option<String>,
    /// stdio server: the program to run. Empty when `url` is set.
    #[serde(default)]
    command: String,
    #[serde(default)]
    args: Vec<String>,
    /// HTTP server: its streamable-HTTP MCP endpoint instead of a command.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    url: Option<String>,
    /// Extra request headers for `url` (e.g. Authorization).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    headers: BTreeMap<String, String>,
    // TODO: secrets live here in plain text (the file is 0600); move them to the keychain.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    env: BTreeMap<String, String>,
}

/// Ids name map keys and log lines, and registry names are slugged into them:
/// keep them to a boring charset.
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && !id.starts_with('.')
        && id.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
}

/// https anywhere; plain http only to this machine (a dev server on localhost).
fn check_url(url: &str) -> Result<(), String> {
    let u = tauri::Url::parse(url).map_err(|e| format!("bad url: {e}"))?;
    let local = matches!(u.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
    match u.scheme() {
        "https" => Ok(()),
        "http" if local => Ok(()),
        _ => Err("url must be https (plain http only for localhost)".into()),
    }
}

fn valid_env_key(k: &str) -> bool {
    let mut chars = k.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn validate(spec: &ServerSpec) -> Result<(), String> {
    if !valid_id(&spec.id) {
        return Err(format!("invalid server id: {}", spec.id));
    }
    if spec.name.trim().is_empty() {
        return Err("server name is required".into());
    }
    match &spec.url {
        Some(url) => {
            check_url(url)?;
            if !spec.command.trim().is_empty() {
                return Err("give either a command or a url, not both".into());
            }
        }
        None if spec.command.trim().is_empty() => return Err("command or url is required".into()),
        None => {}
    }
    let strings = std::iter::once(&spec.command).chain(&spec.args).chain(spec.env.values());
    if strings.into_iter().any(|s| s.contains('\0')) {
        return Err("command, args and env must not contain NUL".into());
    }
    if let Some(k) = spec.env.keys().find(|k| !valid_env_key(k)) {
        return Err(format!("invalid env variable name: {k}"));
    }
    Ok(())
}

/// Installed servers; invalid entries and repeated ids are skipped, so one bad
/// hand edit doesn't hide the rest.
fn parse_servers(json: &str) -> Result<Vec<ServerSpec>, String> {
    let raw: Vec<Value> = serde_json::from_str(json).map_err(|e| format!("servers.json: {e}"))?;
    let mut out: Vec<ServerSpec> = Vec::new();
    for v in raw {
        let Ok(spec) = serde_json::from_value::<ServerSpec>(v) else { continue };
        if validate(&spec).is_ok() && !out.iter().any(|s| s.id == spec.id) {
            out.push(spec);
        }
    }
    Ok(out)
}

fn mcp_dir() -> Result<PathBuf, String> {
    let home = std::env::var("HOME").map_err(|_| "HOME not set".to_string())?;
    Ok(PathBuf::from(home).join("AgentArea").join("mcp"))
}

/// Installed servers. The first call seeds two MCP Apps examples, so the Apps
/// panel has something to run on this device before anything is installed.
fn load_servers() -> Result<Vec<ServerSpec>, String> {
    match std::fs::read_to_string(mcp_dir()?.join("servers.json")) {
        Ok(json) => parse_servers(&json),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let servers = seed_servers();
            save_servers(&servers)?;
            Ok(servers)
        }
        Err(e) => Err(format!("servers.json: {e}")),
    }
}

/// Same entries as the UI's Recommended list.
fn seed_servers() -> Vec<ServerSpec> {
    [
        ("system-monitor", "System monitor", "Real-time CPU, memory and system stats"),
        ("budget-allocator", "Budget allocator", "Budget allocation with interactive visualization"),
    ]
    .into_iter()
    .map(|(id, name, description)| ServerSpec {
        id: id.into(),
        name: name.into(),
        description: Some(description.into()),
        icons: vec![],
        source: Source::Recommended,
        registry: None,
        command: "npx".into(),
        args: vec!["-y".into(), format!("@modelcontextprotocol/server-{id}"), "--stdio".into()],
        url: None,
        headers: BTreeMap::new(),
        env: BTreeMap::new(),
    })
    .collect()
}

/// 0600: entries may carry API keys in `env`.
fn save_servers(servers: &[ServerSpec]) -> Result<(), String> {
    let dir = mcp_dir()?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("mcp dir: {e}"))?;
    let json = serde_json::to_string_pretty(servers).map_err(|e| e.to_string())?;
    crate::write_private(&dir.join("servers.json"), &json).map_err(|e| format!("save servers.json: {e}"))
}

// ── JSON-RPC over stdio ──────────────────────────────────────────────────────

type Reply = Result<Value, String>;
type Pending = Arc<Mutex<HashMap<u64, mpsc::Sender<Reply>>>>;
type Writer = Arc<Mutex<Box<dyn Write + Send>>>;

/// Request/response bookkeeping for one server, independent of the process so
/// the framing and dispatch can be tested with in-memory pipes.
struct Rpc {
    writer: Writer,
    pending: Pending,
    next_id: AtomicU64,
}

fn send_line(writer: &Writer, msg: &Value) -> Result<(), String> {
    let mut line = serde_json::to_vec(msg).map_err(|e| e.to_string())?;
    line.push(b'\n');
    let mut w = writer.lock().map_err(|_| "server pipe poisoned".to_string())?;
    w.write_all(&line).and_then(|_| w.flush()).map_err(|e| format!("write to server: {e}"))
}

impl Rpc {
    fn new(writer: Box<dyn Write + Send>) -> Self {
        Rpc { writer: Arc::new(Mutex::new(writer)), pending: Arc::default(), next_id: AtomicU64::new(1) }
    }

    fn notify(&self, method: &str, params: Value) -> Result<(), String> {
        send_line(&self.writer, &json!({ "jsonrpc": "2.0", "method": method, "params": params }))
    }

    fn request(&self, method: &str, params: Value, timeout: Duration) -> Reply {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel();
        self.pending.lock().map_err(|_| "poisoned")?.insert(id, tx);
        let msg = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        if let Err(e) = send_line(&self.writer, &msg) {
            self.pending.lock().map_err(|_| "poisoned")?.remove(&id);
            return Err(e);
        }
        match rx.recv_timeout(timeout) {
            Ok(reply) => reply,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                self.pending.lock().map_err(|_| "poisoned")?.remove(&id);
                let _ = self.notify("notifications/cancelled", json!({ "requestId": id, "reason": "timeout" }));
                Err(format!("{method} timed out after {}s", timeout.as_secs()))
            }
            // The reader dropped every sender: the process is gone.
            Err(mpsc::RecvTimeoutError::Disconnected) => Err("server exited".into()),
        }
    }
}

/// Route one stdout line. Responses go to their waiting request; requests
/// from the server get an answer (only `ping` is supported) so it never
/// blocks on us; notifications and junk (log lines on stdout) are ignored.
fn dispatch(line: &str, pending: &Pending, writer: &Writer) {
    let Ok(msg) = serde_json::from_str::<Value>(line.trim()) else { return };
    let method = msg.get("method").and_then(Value::as_str);
    match (msg.get("id"), method) {
        (Some(id), Some(method)) => {
            let reply = if method == "ping" {
                json!({ "jsonrpc": "2.0", "id": id, "result": {} })
            } else {
                json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": format!("{method} is not supported") } })
            };
            let _ = send_line(writer, &reply);
        }
        (Some(id), None) => {
            let Some(id) = id.as_u64() else { return };
            let Some(tx) = pending.lock().ok().and_then(|mut p| p.remove(&id)) else { return };
            let reply = match (msg.get("result"), msg.get("error")) {
                (_, Some(err)) => Err(err
                    .get("message")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| err.to_string())),
                (Some(result), None) => Ok(result.clone()),
                (None, None) => Ok(Value::Null),
            };
            let _ = tx.send(reply);
        }
        _ => {}
    }
}

/// Read stdout until EOF, then mark the server dead (before failing whatever
/// is still waiting, so no new request slips in and waits for nothing).
fn read_loop(stdout: impl Read, pending: Pending, writer: Writer, alive: &AtomicBool) {
    for line in BufReader::new(stdout).lines().map_while(Result::ok) {
        dispatch(&line, &pending, &writer);
    }
    alive.store(false, Ordering::SeqCst);
    if let Ok(mut p) = pending.lock() {
        p.clear();
    }
}

fn push_tail(buf: &mut String, chunk: &str) {
    buf.push_str(chunk);
    if buf.len() > STDERR_TAIL {
        let cut = (buf.len() - STDERR_TAIL..).find(|&i| buf.is_char_boundary(i)).unwrap_or(buf.len());
        buf.drain(..cut);
    }
}

// ── processes ────────────────────────────────────────────────────────────────

struct Server {
    /// stdio servers only
    rpc: Option<Rpc>,
    child: Mutex<Option<Child>>,
    /// HTTP servers only
    http: Option<Http>,
    stderr: Arc<Mutex<String>>,
    alive: Arc<AtomicBool>,
}

// ── JSON-RPC over streamable HTTP ────────────────────────────────────────────

/// A server reached over HTTP: each request is a POST whose answer comes back
/// as JSON or as an SSE stream; the session id from `initialize` (if the server
/// hands one out) goes on every later request.
struct Http {
    url: String,
    headers: BTreeMap<String, String>,
    client: reqwest::Client,
    session: Mutex<Option<String>>,
    next_id: AtomicU64,
}

impl Http {
    fn new(url: &str, headers: &BTreeMap<String, String>) -> Result<Self, String> {
        // reqwest needs a Tokio reactor to exist when the client is built.
        let client = tauri::async_runtime::block_on(async { reqwest::Client::builder().timeout(REQUEST_TIMEOUT).build() })
            .map_err(|e| e.to_string())?;
        Ok(Http { url: url.into(), headers: headers.clone(), client, session: Mutex::default(), next_id: AtomicU64::new(1) })
    }

    fn post(&self, msg: &Value) -> Result<reqwest::Response, String> {
        let mut req = self
            .client
            .post(&self.url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .header("MCP-Protocol-Version", PROTOCOL_VERSION)
            .json(msg);
        for (k, v) in &self.headers {
            req = req.header(k, v);
        }
        if let Some(sid) = self.session.lock().ok().and_then(|s| s.clone()) {
            req = req.header("Mcp-Session-Id", sid);
        }
        // Called from blocking threads (spawn_blocking), like the stdio path.
        // Build the future inside the runtime: reqwest arms its timeout timer on `send()`.
        tauri::async_runtime::block_on(async move { req.send().await }).map_err(|e| format!("{}: {e}", self.url))
    }

    fn notify(&self, method: &str, params: Value) -> Result<(), String> {
        let res = self.post(&json!({ "jsonrpc": "2.0", "method": method, "params": params }))?;
        if res.status().is_success() {
            Ok(())
        } else {
            Err(format!("{method}: HTTP {}", res.status()))
        }
    }

    fn request(&self, method: &str, params: Value) -> Reply {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let res = self.post(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))?;
        if let Some(sid) = res.headers().get("mcp-session-id").and_then(|v| v.to_str().ok()) {
            if let Ok(mut s) = self.session.lock() {
                *s = Some(sid.to_string());
            }
        }
        let status = res.status();
        let sse = res
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|ct| ct.starts_with("text/event-stream"));
        let body = tauri::async_runtime::block_on(async move { res.text().await }).map_err(|e| e.to_string())?;
        if !status.is_success() {
            return Err(format!("{method}: HTTP {status}: {}", body.chars().take(300).collect::<String>()));
        }
        http_reply(&body, sse, id).ok_or_else(|| format!("{method}: no response for request {id}"))?
    }
}

/// The JSON-RPC reply to request `id` in an HTTP body: plain JSON, or the
/// `data:` lines of an SSE stream (which may carry notifications first).
fn http_reply(body: &str, sse: bool, id: u64) -> Option<Reply> {
    let messages: Vec<Value> = if sse {
        body.lines()
            .filter_map(|l| l.strip_prefix("data:"))
            .filter_map(|d| serde_json::from_str(d.trim()).ok())
            .collect()
    } else {
        serde_json::from_str::<Value>(body).ok().into_iter().flat_map(|v| match v {
            Value::Array(batch) => batch,
            one => vec![one],
        }).collect()
    };
    let msg = messages.into_iter().find(|m| m.get("id").and_then(Value::as_u64) == Some(id))?;
    Some(match (msg.get("result"), msg.get("error")) {
        (_, Some(err)) => Err(err.get("message").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| err.to_string())),
        (Some(result), None) => Ok(result.clone()),
        (None, None) => Ok(Value::Null),
    })
}

#[cfg(unix)]
extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}

impl Server {
    fn stderr_tail(&self) -> String {
        self.stderr.lock().map(|s| s.trim().to_string()).unwrap_or_default()
    }

    fn request(&self, method: &str, params: Value, timeout: Duration) -> Reply {
        if !self.alive.load(Ordering::SeqCst) {
            return Err(self.exit_message());
        }
        if let Some(http) = &self.http {
            return http.request(method, params);
        }
        let rpc = self.rpc.as_ref().expect("stdio server has rpc");
        rpc.request(method, params, timeout).map_err(|e| {
            if e == "server exited" {
                self.exit_message()
            } else {
                e
            }
        })
    }

    fn exit_message(&self) -> String {
        let tail = self.stderr_tail();
        if tail.is_empty() {
            "server exited".into()
        } else {
            format!("server exited: {tail}")
        }
    }

    /// Signal the whole process group: `npx` runs the real server as its own
    /// child, which would outlive a plain kill of npx.
    fn notify(&self, method: &str, params: Value) -> Result<(), String> {
        match (&self.http, &self.rpc) {
            (Some(http), _) => http.notify(method, params),
            (None, Some(rpc)) => rpc.notify(method, params),
            (None, None) => Ok(()),
        }
    }

    fn signal(&self, sig: i32) {
        let Ok(mut guard) = self.child.lock() else { return };
        // HTTP servers aren't ours to kill: stopping just forgets the session.
        let Some(child) = guard.as_mut() else { return };
        #[cfg(unix)]
        unsafe {
            kill(-(child.id() as i32), sig);
        }
        if sig == 9 {
            let _ = child.kill();
        }
    }

    fn reaped(&self) -> bool {
        self.child
            .lock()
            .map(|mut c| c.as_mut().is_none_or(|c| matches!(c.try_wait(), Ok(Some(_)))))
            .unwrap_or(true)
    }
}

/// SIGTERM everyone, give them until `grace` to leave, then SIGKILL.
fn stop_servers(servers: &[Arc<Server>], grace: Duration) {
    for s in servers {
        s.alive.store(false, Ordering::SeqCst);
        s.signal(15);
    }
    let deadline = Instant::now() + grace;
    while Instant::now() < deadline && !servers.iter().all(|s| s.reaped()) {
        std::thread::sleep(Duration::from_millis(50));
    }
    for s in servers.iter().filter(|s| !s.reaped()) {
        s.signal(9);
        if let Ok(mut c) = s.child.lock() {
            if let Some(c) = c.as_mut() {
                let _ = c.wait();
            }
        }
    }
}

/// `$HOME/.local/bin:/opt/homebrew/bin:/usr/local/bin` plus the folder the
/// command really lives in, ahead of the inherited PATH: a GUI app has no shell
/// PATH, and `npx` is a `#!/usr/bin/env node` script that must find `node`.
fn child_path(bin: &std::path::Path) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    let mut dirs: Vec<String> = Vec::new();
    if let Some(dir) = bin.canonicalize().ok().and_then(|p| p.parent().map(|d| d.display().to_string())) {
        dirs.push(dir);
    }
    dirs.extend([format!("{home}/.local/bin"), "/opt/homebrew/bin".into(), "/usr/local/bin".into()]);
    if let Ok(path) = std::env::var("PATH") {
        dirs.push(path);
    }
    dirs.join(":")
}

fn spawn(spec: &ServerSpec) -> Result<Server, String> {
    let bin = if spec.command.contains('/') { PathBuf::from(&spec.command) } else { crate::cli_bin(&spec.command) };
    let cwd = mcp_dir()?;
    std::fs::create_dir_all(&cwd).map_err(|e| format!("mcp dir: {e}"))?;
    let mut cmd = Command::new(&bin);
    cmd.args(&spec.args)
        .envs(&spec.env)
        .env("PATH", child_path(&bin))
        .current_dir(&cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    let mut child = cmd.spawn().map_err(|e| format!("failed to start {}: {e}", spec.command))?;

    let stdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    let rpc = Rpc::new(Box::new(stdin));
    let alive = Arc::new(AtomicBool::new(true));
    let tail = Arc::new(Mutex::new(String::new()));

    let (pending, writer, flag) = (rpc.pending.clone(), rpc.writer.clone(), alive.clone());
    std::thread::spawn(move || read_loop(stdout, pending, writer, &flag));
    let sink = tail.clone();
    std::thread::spawn(move || {
        let mut buf = [0u8; 2048];
        while let Ok(n) = stderr.read(&mut buf) {
            if n == 0 {
                break;
            }
            if let Ok(mut t) = sink.lock() {
                push_tail(&mut t, &String::from_utf8_lossy(&buf[..n]));
            }
        }
    });
    Ok(Server { rpc: Some(rpc), child: Mutex::new(Some(child)), http: None, stderr: tail, alive })
}

fn initialize_params() -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": {
            "extensions": {
                "io.modelcontextprotocol/ui": { "mimeTypes": ["text/html;profile=mcp-app"] }
            }
        },
        "clientInfo": { "name": "AgentArea Desktop", "version": env!("CARGO_PKG_VERSION") }
    })
}

/// Start `spec` and complete the MCP handshake; returns the server and its
/// `initialize` result.
fn start_server(spec: &ServerSpec) -> Result<(Server, Value), String> {
    let server = match &spec.url {
        Some(url) => Server {
            rpc: None,
            child: Mutex::new(None),
            http: Some(Http::new(url, &spec.headers)?),
            stderr: Arc::default(),
            alive: Arc::new(AtomicBool::new(true)),
        },
        None => spawn(spec)?,
    };
    let init = server
        .request("initialize", initialize_params(), INITIALIZE_TIMEOUT)
        .and_then(|r| server.notify("notifications/initialized", json!({})).map(|_| r));
    match init {
        Ok(result) => Ok((server, result)),
        Err(e) => {
            let server = Arc::new(server);
            stop_servers(std::slice::from_ref(&server), Duration::from_millis(300));
            Err(e)
        }
    }
}

// ── Tauri state + commands ───────────────────────────────────────────────────

/// Running servers by id. An entry whose process died stays until the next
/// start or stop, so the UI can show why it died.
#[derive(Default)]
pub struct LocalMcp(Mutex<HashMap<String, Arc<Server>>>);

impl LocalMcp {
    fn get(&self, id: &str) -> Option<Arc<Server>> {
        self.0.lock().ok()?.get(id).cloned()
    }

    fn take(&self, id: &str) -> Option<Arc<Server>> {
        self.0.lock().ok()?.remove(id)
    }

    /// Called on app exit.
    pub fn stop_all(&self) {
        let servers: Vec<_> = self.0.lock().map(|mut m| m.drain().map(|(_, s)| s).collect()).unwrap_or_default();
        stop_servers(&servers, Duration::from_millis(500));
    }
}

#[derive(Serialize)]
pub struct LocalServer {
    #[serde(flatten)]
    spec: ServerSpec,
    /// "stopped" | "running" | "error" (exited on its own)
    status: &'static str,
    /// Last few KB of stderr, for a server that died.
    stderr: Option<String>,
}

#[tauri::command]
pub fn local_mcp_list(state: State<'_, LocalMcp>) -> Result<Vec<LocalServer>, String> {
    Ok(load_servers()?
        .into_iter()
        .map(|spec| {
            let (status, stderr) = match state.get(&spec.id) {
                None => ("stopped", None),
                Some(s) if s.alive.load(Ordering::SeqCst) => ("running", None),
                Some(s) => ("error", Some(s.stderr_tail())),
            };
            LocalServer { spec, status, stderr }
        })
        .collect())
}

/// Add a server, or replace the one with the same id.
#[tauri::command]
pub fn local_mcp_install(server: ServerSpec) -> Result<(), String> {
    validate(&server)?;
    let mut servers = load_servers()?;
    match servers.iter_mut().find(|s| s.id == server.id) {
        Some(existing) => *existing = server,
        None => servers.push(server),
    }
    save_servers(&servers)
}

#[tauri::command]
pub async fn local_mcp_uninstall(state: State<'_, LocalMcp>, id: String) -> Result<(), String> {
    local_mcp_stop(state, id.clone()).await?;
    let mut servers = load_servers()?;
    servers.retain(|s| s.id != id);
    save_servers(&servers)
}

/// Start an installed server (no-op if it is running); returns its
/// `initialize` result.
#[tauri::command]
pub async fn local_mcp_start(state: State<'_, LocalMcp>, id: String) -> Result<Value, String> {
    if !valid_id(&id) {
        return Err(format!("invalid server id: {id}"));
    }
    if state.get(&id).is_some_and(|s| s.alive.load(Ordering::SeqCst)) {
        return Ok(Value::Null);
    }
    let spec = load_servers()?.into_iter().find(|s| s.id == id).ok_or_else(|| format!("not installed: {id}"))?;
    let (server, init) = tauri::async_runtime::spawn_blocking(move || start_server(&spec))
        .await
        .map_err(|e| e.to_string())??;
    let server = Arc::new(server);
    // Two quick starts can race; keep the first and stop the spare.
    let spare = {
        let mut map = state.0.lock().map_err(|_| "poisoned")?;
        match map.get(&id) {
            Some(s) if s.alive.load(Ordering::SeqCst) => Some(server),
            _ => {
                map.insert(id, server);
                None
            }
        }
    };
    if let Some(spare) = spare {
        tauri::async_runtime::spawn_blocking(move || stop_servers(&[spare], Duration::from_millis(300)));
    }
    Ok(init)
}

#[tauri::command]
pub async fn local_mcp_stop(state: State<'_, LocalMcp>, id: String) -> Result<(), String> {
    if let Some(server) = state.take(&id) {
        tauri::async_runtime::spawn_blocking(move || stop_servers(&[server], Duration::from_millis(1000)))
            .await
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// One MCP request (`tools/list`, `tools/call`, `resources/read`, …) to a
/// running server; returns the JSON-RPC result.
#[tauri::command]
pub async fn local_mcp_request(
    state: State<'_, LocalMcp>,
    id: String,
    method: String,
    params: Option<Value>,
) -> Result<Value, String> {
    let server = state.get(&id).ok_or_else(|| format!("{id} is not running"))?;
    let params = params.unwrap_or_else(|| json!({}));
    tauri::async_runtime::spawn_blocking(move || server.request(&method, params, REQUEST_TIMEOUT))
        .await
        .map_err(|e| e.to_string())?
}

/// GET a JSON document over https, for user-added registries: going through
/// Rust keeps arbitrary URLs off the webview's http allowlist.
#[tauri::command]
pub async fn http_get_json(url: String) -> Result<Value, String> {
    let parsed = tauri::Url::parse(&url).map_err(|e| format!("bad url: {e}"))?;
    if parsed.scheme() != "https" {
        return Err("only https URLs are allowed".into());
    }
    let client = tauri_plugin_http::reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|e| e.to_string())?;
    let res = client
        .get(parsed)
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;
    if !res.status().is_success() {
        return Err(format!("GET {url} failed ({})", res.status()));
    }
    let bytes = res.bytes().await.map_err(|e| e.to_string())?;
    if bytes.len() > 8 << 20 {
        return Err("response is larger than 8 MiB".into());
    }
    serde_json::from_slice(&bytes).map_err(|e| format!("not JSON: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_replies_from_json_and_sse() {
        let json_body = r#"{"jsonrpc":"2.0","id":3,"result":{"ok":true}}"#;
        assert_eq!(http_reply(json_body, false, 3).unwrap().unwrap()["ok"], true);
        let sse = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}\n\nevent: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":7,\"error\":{\"code\":1,\"message\":\"nope\"}}\n\n";
        assert_eq!(http_reply(sse, true, 7).unwrap(), Err("nope".to_string()));
        assert!(http_reply(json_body, false, 4).is_none(), "other ids are not ours");
    }

    #[test]
    fn url_servers_validate() {
        let mut spec = seed_servers().remove(0);
        spec.command = String::new();
        spec.args.clear();
        spec.url = Some("http://localhost:3402/mcp".into());
        assert!(validate(&spec).is_ok());
        spec.url = Some("http://example.com/mcp".into());
        assert!(validate(&spec).is_err(), "plain http only for localhost");
        spec.url = Some("https://example.com/mcp".into());
        assert!(validate(&spec).is_ok());
        spec.command = "npx".into();
        assert!(validate(&spec).is_err(), "command and url together");
    }

    /// Needs an MCP server on http://localhost:3402/mcp (e.g. mcp-apps-outreach).
    #[test]
    #[ignore]
    fn http_server_serves_its_ui() {
        let mut spec = seed_servers().remove(0);
        spec.id = "outreach".into();
        spec.command = String::new();
        spec.args.clear();
        spec.url = Some("http://localhost:3402/mcp".into());
        let (server, init) = start_server(&spec).expect("initialize");
        println!("server: {}", init["serverInfo"]);
        let tools = server.request("tools/list", json!({}), REQUEST_TIMEOUT).unwrap();
        let ui = tools["tools"].as_array().unwrap().iter().find_map(|t| t["_meta"]["ui"]["resourceUri"].as_str()).unwrap().to_string();
        let res = server.request("resources/read", json!({ "uri": ui }), REQUEST_TIMEOUT).unwrap();
        let html = res["contents"][0]["text"].as_str().unwrap();
        assert!(html.contains("<html") || html.contains("<!doctype") || html.contains("<!DOCTYPE"));
        println!("ui {ui}: {} bytes of html", html.len());
    }

    /// A Write that appends into a shared buffer the test can inspect.
    #[derive(Clone, Default)]
    struct Sink(Arc<Mutex<Vec<u8>>>);
    impl Write for Sink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl Sink {
        fn lines(&self) -> Vec<Value> {
            String::from_utf8(self.0.lock().unwrap().clone())
                .unwrap()
                .lines()
                .map(|l| serde_json::from_str(l).unwrap())
                .collect()
        }
    }

    #[test]
    fn frames_requests_as_single_lines() {
        let sink = Sink::default();
        let rpc = Rpc::new(Box::new(sink.clone()));
        let (pending, writer) = (rpc.pending.clone(), rpc.writer.clone());
        // Answer the request from another thread once it is on the wire.
        let responder = std::thread::spawn(move || {
            while pending.lock().unwrap().is_empty() {
                std::thread::sleep(Duration::from_millis(5));
            }
            dispatch(r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[]}}"#, &pending, &writer);
        });
        let result = rpc.request("tools/list", json!({"cursor": "a\nb"}), Duration::from_secs(5)).unwrap();
        responder.join().unwrap();
        assert_eq!(result, json!({"tools": []}));
        let raw = String::from_utf8(sink.0.lock().unwrap().clone()).unwrap();
        assert_eq!(raw.matches('\n').count(), 1, "newlines inside values stay escaped");
        assert_eq!(sink.lines(), vec![json!({"jsonrpc":"2.0","id":1,"method":"tools/list","params":{"cursor":"a\nb"}})]);
    }

    #[test]
    fn dispatches_responses_by_id() {
        let sink = Sink::default();
        let rpc = Rpc::new(Box::new(sink.clone()));
        let (tx1, rx1) = mpsc::channel();
        let (tx2, rx2) = mpsc::channel();
        rpc.pending.lock().unwrap().insert(1, tx1);
        rpc.pending.lock().unwrap().insert(2, tx2);

        // Out of order, with noise in between.
        dispatch("npm warn exec something", &rpc.pending, &rpc.writer);
        dispatch(r#"{"jsonrpc":"2.0","method":"notifications/message","params":{}}"#, &rpc.pending, &rpc.writer);
        dispatch(r#"{"jsonrpc":"2.0","id":2,"error":{"code":-32602,"message":"bad uri"}}"#, &rpc.pending, &rpc.writer);
        dispatch(r#"{"jsonrpc":"2.0","id":1,"result":{"ok":true}}"#, &rpc.pending, &rpc.writer);
        dispatch(r#"{"jsonrpc":"2.0","id":99,"result":{}}"#, &rpc.pending, &rpc.writer);

        assert_eq!(rx1.try_recv().unwrap(), Ok(json!({"ok": true})));
        assert_eq!(rx2.try_recv().unwrap(), Err("bad uri".to_string()));
        assert!(rpc.pending.lock().unwrap().is_empty());
        assert!(sink.lines().is_empty(), "responses and notifications need no reply");
    }

    #[test]
    fn answers_server_requests() {
        let sink = Sink::default();
        let rpc = Rpc::new(Box::new(sink.clone()));
        dispatch(r#"{"jsonrpc":"2.0","id":"p1","method":"ping"}"#, &rpc.pending, &rpc.writer);
        dispatch(r#"{"jsonrpc":"2.0","id":7,"method":"roots/list"}"#, &rpc.pending, &rpc.writer);
        let out = sink.lines();
        assert_eq!(out[0], json!({"jsonrpc":"2.0","id":"p1","result":{}}));
        assert_eq!(out[1]["id"], 7);
        assert_eq!(out[1]["error"]["code"], -32601);
    }

    #[test]
    fn eof_fails_pending_requests() {
        let rpc = Rpc::new(Box::new(Sink::default()));
        let (tx, rx) = mpsc::channel();
        rpc.pending.lock().unwrap().insert(1, tx);
        read_loop(&b"{\"jsonrpc\":\"2.0\",\"method\":\"x\"}\n"[..], rpc.pending.clone(), rpc.writer.clone(), &AtomicBool::new(true));
        assert_eq!(rx.recv_timeout(Duration::from_secs(1)), Err(mpsc::RecvTimeoutError::Disconnected));
    }

    #[test]
    fn times_out() {
        let rpc = Rpc::new(Box::new(Sink::default()));
        let err = rpc.request("tools/list", json!({}), Duration::from_millis(20)).unwrap_err();
        assert!(err.contains("timed out"));
        assert!(rpc.pending.lock().unwrap().is_empty());
    }

    #[test]
    fn keeps_a_bounded_stderr_tail() {
        let mut t = String::new();
        push_tail(&mut t, &"a".repeat(STDERR_TAIL));
        push_tail(&mut t, "é-end");
        assert!(t.len() <= STDERR_TAIL);
        assert!(t.ends_with("é-end"));
    }

    #[test]
    fn parses_servers_json() {
        let json = r#"[
          {"id":"system-monitor","name":"System monitor","source":"recommended",
           "command":"npx","args":["-y","@modelcontextprotocol/server-system-monitor","--stdio"]},
          {"id":"gh","name":"GitHub","source":"registry","registry":"https://registry.modelcontextprotocol.io",
           "icons":["https://x/icon.png"],"command":"npx","args":[],"env":{"GITHUB_TOKEN":"t"}},
          {"id":"system-monitor","name":"dup","source":"custom","command":"x"},
          {"id":"../evil","name":"x","source":"custom","command":"x"},
          {"id":"no-cmd","name":"x","source":"custom","command":" "},
          {"id":"bad-env","name":"x","source":"custom","command":"x","env":{"A=B":"1"}},
          {"id":"bad-source","name":"x","source":"elsewhere","command":"x"},
          "junk"
        ]"#;
        let servers = parse_servers(json).unwrap();
        assert_eq!(servers.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(), ["system-monitor", "gh"]);
        assert_eq!(servers[0].source, Source::Recommended);
        assert_eq!(servers[0].args[1], "@modelcontextprotocol/server-system-monitor");
        assert_eq!(servers[1].env["GITHUB_TOKEN"], "t");
        assert!(parse_servers("{}").is_err(), "must be an array");

        // Round-trips without inventing empty fields.
        let out = serde_json::to_value(&servers[0]).unwrap();
        assert!(out.get("env").is_none() && out.get("icons").is_none() && out.get("registry").is_none());
        assert_eq!(parse_servers(&serde_json::to_string(&servers).unwrap()).unwrap(), servers);
    }

    #[test]
    fn validates_ids() {
        for ok in ["system-monitor", "io.github.user.server", "a_b-1", "x"] {
            assert!(valid_id(ok), "{ok}");
        }
        for bad in ["", ".hidden", "a/b", "../x", "a b", "é", &"a".repeat(65)] {
            assert!(!valid_id(bad), "{bad}");
        }
    }

    #[test]
    fn seeds_valid_examples() {
        let seeded = seed_servers();
        assert_eq!(seeded.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(), ["system-monitor", "budget-allocator"]);
        assert_eq!(parse_servers(&serde_json::to_string(&seeded).unwrap()).unwrap(), seeded);
        assert_eq!(seeded[1].args, ["-y", "@modelcontextprotocol/server-budget-allocator", "--stdio"]);
    }

    /// Start a seeded example, list its UI tool, read the `ui://` HTML and call
    /// the tool, then stop it and check the whole process group is gone.
    fn run_example(id: &str, tool_name: &str) {
        let spec = seed_servers().into_iter().find(|s| s.id == id).unwrap();
        let (server, init) = start_server(&spec).unwrap();
        let server = Arc::new(server);
        assert!(init["serverInfo"]["name"].is_string(), "{init}");

        let tools = server.request("tools/list", json!({}), REQUEST_TIMEOUT).unwrap();
        let tool = tools["tools"].as_array().unwrap().iter().find(|t| t["name"] == tool_name).unwrap();
        let uri = tool["_meta"]["ui"]["resourceUri"].as_str().unwrap().to_string();
        assert!(uri.starts_with("ui://"), "{uri}");

        let res = server.request("resources/read", json!({ "uri": uri }), REQUEST_TIMEOUT).unwrap();
        let content = &res["contents"][0];
        assert_eq!(content["mimeType"], "text/html;profile=mcp-app");
        assert!(content["text"].as_str().unwrap().contains("<html"), "ui resource is HTML");

        let call = server
            .request("tools/call", json!({ "name": tool_name, "arguments": {} }), REQUEST_TIMEOUT)
            .unwrap();
        assert_ne!(call["isError"], true, "{call}");

        let pid = server.child.lock().unwrap().as_ref().unwrap().id() as i32;
        stop_servers(&[server], Duration::from_millis(1000));
        #[cfg(unix)]
        assert_eq!(unsafe { kill(-pid, 0) }, -1, "whole process group is gone");
    }

    /// Real end-to-end runs against the published examples; need network and
    /// node. `cargo test -- --ignored examples_`.
    #[test]
    #[ignore]
    fn examples_system_monitor_serves_its_ui() {
        run_example("system-monitor", "get-system-info");
    }

    #[test]
    #[ignore]
    fn examples_budget_allocator_serves_its_ui() {
        run_example("budget-allocator", "get-budget-data");
    }
}
