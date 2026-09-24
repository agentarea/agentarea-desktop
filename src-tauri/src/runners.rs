//! Local agent CLIs as long-lived processes: one per thread, kept between
//! turns, speaking each CLI's own protocol over stdio.
//!
//! - **Codex**: `codex app-server`, JSON-RPC 2.0 as JSONL. We `initialize`,
//!   then `thread/start` (or `thread/resume` with the stored thread id) once
//!   per process, and every turn is a `turn/start`. Approvals arrive as server
//!   requests (`item/commandExecution/requestApproval`, …) and are answered
//!   with the user's decision. One process per thread rather than one shared:
//!   each thread has its own MCP config and bearer tokens, which Codex reads
//!   from `-c` flags and the process env, so a process is the unit of config.
//! - **Claude Code**: `claude -p --input-format stream-json`, one user message
//!   per turn on stdin. Permission prompts use `--permission-prompt-tool stdio`
//!   — the control protocol the Claude Agent SDK speaks: the CLI sends a
//!   `control_request` (`can_use_tool`) on stdout and waits for our
//!   `control_response` (`{"behavior":"allow"|"deny",…}`) on stdin. Stop is a
//!   `control_request` with subtype `interrupt` on the same channel.
//!
//! Approval policy, like Codex's default "Auto": reads, edits inside the
//! thread's folder and shell commands in the OS sandbox (writes confined to
//! the folder, no network) run without asking — Claude: `acceptEdits` +
//! Read/Glob/Grep + sandboxed Bash auto-allowed; Codex: `on-request` with the
//! workspace-write sandbox, file changes all in the folder accepted here.
//! Stepping outside the sandbox, edits elsewhere and other tools (web, …) ask
//! the user. The
//! AgentArea MCP servers (`agentarea`, `agentarea_secrets`, `agentarea_data`)
//! are pre-approved: they are governed on their own side.
//!
//! The UI gets `agent-line` (stdout lines worth parsing, redacted),
//! `agent-approval` / `agent-approval-resolved`, and `agent-turn-end`.
//! A process that dies is simply started again (resuming) on the next turn;
//! idle ones are reaped after `IDLE_REAP`, and all of them go with the app.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{json, Value};

use crate::vault::Redactor;

/// An idle process (no turn, nothing waiting on the user) is stopped after this.
const IDLE_REAP: Duration = Duration::from_secs(30 * 60);
/// How long a JSON-RPC request to `codex app-server` may take.
const RPC_TIMEOUT: Duration = Duration::from_secs(60);
/// After Stop, a turn that hasn't ended by then gets its process killed.
const STOP_GRACE: Duration = Duration::from_secs(5);
const STDERR_TAIL: usize = 4000;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Kind {
    Claude,
    Codex,
}

/// What the UI hears about a thread's process.
#[derive(Clone, Debug)]
pub enum Event {
    /// A stdout line for the chat parser (local.ts), already redacted.
    Line(String),
    Approval(Approval),
    /// An approval that no longer waits: answered, cancelled, or its turn ended.
    Resolved(String),
    /// The turn is over. `error` when it failed (or the process died mid-turn).
    TurnEnd { error: Option<String>, interrupted: bool },
}

/// Something the agent wants to do that needs the user's yes.
#[derive(Clone, Debug, Serialize)]
pub struct Approval {
    pub id: String,
    /// "shell" | "edit" | "tool" | "permissions"
    pub kind: &'static str,
    /// e.g. "Run a command", "Use WebFetch"
    pub title: String,
    /// the command, the files, or the tool's input
    pub detail: String,
    pub reason: Option<String>,
    /// whether "Allow for this session" is on offer
    pub can_session: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Decision {
    Allow,
    AllowSession,
    Deny,
}

impl Decision {
    pub fn parse(s: &str) -> Result<Decision, String> {
        match s {
            "allow" => Ok(Decision::Allow),
            "allow_session" => Ok(Decision::AllowSession),
            "deny" => Ok(Decision::Deny),
            other => Err(format!("unknown decision: {other}")),
        }
    }
}

pub type Sink = Arc<dyn Fn(&str, Event) + Send + Sync>;

/// A request waiting on the user, with what answering it needs.
enum Pending {
    Codex { rpc_id: Value, method: String, params: Value },
    Claude { request_id: String, input: Value, suggestions: Value },
}

struct State {
    busy: bool,
    /// Stop was asked for this turn.
    interrupted: bool,
    /// We killed it (restart, reap, Stop fallback, app exit).
    killed: bool,
    dead: bool,
    last_used: Instant,
    /// approval id → request
    approvals: HashMap<String, Pending>,
    stderr: String,
    // Codex only
    thread_id: Option<String>,
    turn_id: Option<String>,
    next_rpc: u64,
    waiting: HashMap<u64, mpsc::Sender<Value>>,
    /// fileChange item id → the paths it touches (from `item/started`)
    file_changes: HashMap<String, Vec<String>>,
}

/// Held for the life of a process: bearer tokens, the proxy route, files to delete.
pub struct Guards {
    pub keep: Vec<Box<dyn Send + Sync>>,
    pub files: Vec<PathBuf>,
}

impl Drop for Guards {
    fn drop(&mut self) {
        for f in &self.files {
            let _ = std::fs::remove_file(f);
        }
    }
}

pub struct Proc {
    kind: Kind,
    session_id: String,
    /// What it was started with; a turn asking for something else restarts it.
    signature: u64,
    cwd: PathBuf,
    child: Mutex<Child>,
    stdin: Mutex<ChildStdin>,
    state: Mutex<State>,
    redactor: Redactor,
    guards: Mutex<Option<Guards>>,
    /// Whether the next turn is the first this process sees (hints go there).
    fresh: Mutex<bool>,
}

impl Proc {
    fn write(&self, v: &Value) -> Result<(), String> {
        let mut stdin = self.stdin.lock().map_err(|_| "poisoned")?;
        writeln!(stdin, "{v}").and_then(|_| stdin.flush()).map_err(|e| format!("agent process: {e}"))
    }

    /// One JSON-RPC request to `codex app-server`; the reader thread routes the reply.
    fn rpc(&self, method: &str, params: Value, timeout: Duration) -> Result<Value, String> {
        let (tx, rx) = mpsc::channel();
        let id = {
            let mut st = self.state.lock().map_err(|_| "poisoned")?;
            if st.dead {
                return Err("codex app-server has exited".into());
            }
            st.next_rpc += 1;
            let id = st.next_rpc;
            st.waiting.insert(id, tx);
            id
        };
        self.write(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))?;
        let reply = rx.recv_timeout(timeout).map_err(|_| {
            let tail = self.state.lock().map(|s| s.stderr.clone()).unwrap_or_default();
            format!("codex app-server didn't answer {method}{}", if tail.is_empty() { String::new() } else { format!(": {}", self.redactor.redact(&tail)) })
        })?;
        if let Some(err) = reply.get("error") {
            return Err(format!("{method}: {}", err["message"].as_str().unwrap_or("failed")));
        }
        Ok(reply.get("result").cloned().unwrap_or(Value::Null))
    }

    fn kill(&self) {
        if let Ok(mut st) = self.state.lock() {
            st.killed = true;
        }
        if let Ok(mut c) = self.child.lock() {
            let _ = c.kill();
        }
    }

    fn alive(&self) -> bool {
        self.state.lock().map(|s| !s.dead).unwrap_or(false)
    }
}

struct Inner {
    procs: Mutex<HashMap<String, Arc<Proc>>>,
    sink: Sink,
}

/// Every thread's process; managed as Tauri state.
#[derive(Clone)]
pub struct Runners {
    inner: Arc<Inner>,
}

impl Runners {
    pub fn new(sink: Sink) -> Runners {
        let inner = Arc::new(Inner { procs: Mutex::default(), sink });
        let weak = Arc::downgrade(&inner);
        std::thread::spawn(move || loop {
            std::thread::sleep(Duration::from_secs(60));
            let Some(inner) = weak.upgrade() else { return };
            let idle: Vec<Arc<Proc>> = inner
                .procs
                .lock()
                .map(|p| {
                    p.values()
                        .filter(|p| {
                            p.state.lock().is_ok_and(|s| !s.busy && s.approvals.is_empty() && s.last_used.elapsed() > IDLE_REAP)
                        })
                        .cloned()
                        .collect()
                })
                .unwrap_or_default();
            for p in idle {
                p.kill();
            }
        });
        Runners { inner }
    }

    fn get(&self, session_id: &str) -> Option<Arc<Proc>> {
        self.inner.procs.lock().ok()?.get(session_id).cloned()
    }

    /// The thread's process if it is running with `signature`. One started
    /// differently is stopped (it can't be while a turn runs).
    pub fn reusable(&self, session_id: &str, signature: u64) -> Result<Option<Arc<Proc>>, String> {
        let Some(p) = self.get(session_id) else { return Ok(None) };
        if !p.alive() {
            return Ok(None);
        }
        let busy = p.state.lock().map(|s| s.busy).unwrap_or(false);
        if busy {
            return Err("a turn is already running in this thread".into());
        }
        if p.signature == signature {
            return Ok(Some(p));
        }
        p.kill();
        self.forget(&p);
        Ok(None)
    }

    fn forget(&self, p: &Arc<Proc>) {
        if let Ok(mut procs) = self.inner.procs.lock() {
            if procs.get(&p.session_id).is_some_and(|q| Arc::ptr_eq(q, p)) {
                procs.remove(&p.session_id);
            }
        }
    }

    /// Start `cmd` as the thread's process. For Codex this also runs the
    /// handshake and opens (or resumes) the Codex thread.
    #[allow(clippy::too_many_arguments)]
    pub fn spawn(
        &self,
        kind: Kind,
        session_id: &str,
        signature: u64,
        mut cmd: Command,
        cwd: &Path,
        redactor: Redactor,
        guards: Guards,
        codex: Option<CodexThread>,
    ) -> Result<Arc<Proc>, String> {
        let mut child = cmd
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("failed to start {}: {e}", if kind == Kind::Claude { "claude" } else { "codex" }))?;
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let proc = Arc::new(Proc {
            kind,
            session_id: session_id.to_string(),
            signature,
            cwd: cwd.to_path_buf(),
            child: Mutex::new(child),
            stdin: Mutex::new(stdin),
            state: Mutex::new(State {
                busy: false,
                interrupted: false,
                killed: false,
                dead: false,
                last_used: Instant::now(),
                approvals: HashMap::new(),
                stderr: String::new(),
                thread_id: None,
                turn_id: None,
                next_rpc: 0,
                waiting: HashMap::new(),
                file_changes: HashMap::new(),
            }),
            redactor,
            guards: Mutex::new(Some(guards)),
            fresh: Mutex::new(true),
        });

        let p = proc.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            let mut stderr = stderr;
            while let Ok(n) = stderr.read(&mut buf) {
                if n == 0 {
                    break;
                }
                if let Ok(mut st) = p.state.lock() {
                    st.stderr.push_str(&String::from_utf8_lossy(&buf[..n]));
                    if st.stderr.len() > STDERR_TAIL {
                        let cut = (st.stderr.len() - STDERR_TAIL..).find(|&i| st.stderr.is_char_boundary(i)).unwrap_or(0);
                        st.stderr.drain(..cut);
                    }
                }
            }
        });

        let (p, runners) = (proc.clone(), self.clone());
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                match p.kind {
                    Kind::Claude => runners.on_claude_line(&p, &line),
                    Kind::Codex => runners.on_codex_line(&p, &line),
                }
            }
            runners.on_exit(&p);
        });

        if let Ok(mut procs) = self.inner.procs.lock() {
            if let Some(old) = procs.insert(session_id.to_string(), proc.clone()) {
                old.kill();
            }
        }
        if let Some(thread) = codex {
            if let Err(e) = codex_open(&proc, thread) {
                proc.kill();
                self.forget(&proc);
                return Err(e);
            }
        }
        Ok(proc)
    }

    /// Start a turn on the thread's (running) process. `prompt` is final text.
    pub fn turn(&self, proc: &Arc<Proc>, prompt: &str, model: Option<&str>, effort: Option<&str>) -> Result<(), String> {
        {
            let mut st = proc.state.lock().map_err(|_| "poisoned")?;
            if st.busy {
                return Err("a turn is already running in this thread".into());
            }
            st.busy = true;
            st.interrupted = false;
            st.last_used = Instant::now();
        }
        if let Ok(mut f) = proc.fresh.lock() {
            *f = false;
        }
        let sent = match proc.kind {
            Kind::Claude => proc.write(&json!({
                "type": "user",
                "message": { "role": "user", "content": prompt },
                "parent_tool_use_id": null,
                "session_id": proc.session_id,
            })),
            Kind::Codex => {
                let thread_id = proc.state.lock().ok().and_then(|s| s.thread_id.clone()).unwrap_or_default();
                let mut params = json!({
                    "threadId": thread_id,
                    "input": [{ "type": "text", "text": prompt, "text_elements": [] }],
                });
                // Overrides stick for later turns, so a model change needs no restart.
                if let Some(m) = model {
                    params["model"] = json!(m);
                }
                if let Some(e) = effort {
                    params["effort"] = json!(e);
                }
                proc.rpc("turn/start", params, RPC_TIMEOUT).map(|r| {
                    if let (Some(id), Ok(mut st)) = (r["turn"]["id"].as_str(), proc.state.lock()) {
                        st.turn_id = Some(id.to_string());
                    }
                })
            }
        };
        if sent.is_err() {
            if let Ok(mut st) = proc.state.lock() {
                st.busy = false;
            }
        }
        sent
    }

    /// Whether the thread's process has run no turn yet (prompt hints go on its first).
    pub fn is_fresh(&self, proc: &Proc) -> bool {
        proc.fresh.lock().map(|f| *f).unwrap_or(true)
    }

    /// Stop the running turn. Falls back to killing the process (the next
    /// turn resumes the session) when the CLI doesn't wind down in time.
    pub fn interrupt(&self, session_id: &str) -> Result<(), String> {
        let Some(p) = self.get(session_id) else { return Ok(()) };
        {
            let mut st = p.state.lock().map_err(|_| "poisoned")?;
            if !st.busy {
                return Ok(());
            }
            st.interrupted = true;
        }
        let asked = match p.kind {
            Kind::Claude => p.write(&json!({
                "type": "control_request",
                "request_id": format!("interrupt-{}", uuid::Uuid::new_v4().simple()),
                "request": { "subtype": "interrupt" },
            })),
            Kind::Codex => {
                let (thread, turn) = p.state.lock().map(|s| (s.thread_id.clone(), s.turn_id.clone())).unwrap_or_default();
                match (thread, turn) {
                    (Some(thread), Some(turn)) => p.rpc("turn/interrupt", json!({ "threadId": thread, "turnId": turn }), STOP_GRACE).map(|_| ()),
                    _ => Err("no turn id yet".into()),
                }
            }
        };
        if asked.is_err() {
            p.kill();
            return Ok(());
        }
        std::thread::spawn(move || {
            std::thread::sleep(STOP_GRACE);
            if p.state.lock().is_ok_and(|s| s.busy && s.interrupted) {
                p.kill();
            }
        });
        Ok(())
    }

    /// The user's answer to an approval.
    pub fn answer(&self, session_id: &str, id: &str, decision: Decision) -> Result<(), String> {
        let p = self.get(session_id).ok_or("this thread's agent is no longer running")?;
        let pending = p.state.lock().map_err(|_| "poisoned")?.approvals.remove(id).ok_or("this request was already answered")?;
        let reply = match pending {
            Pending::Codex { rpc_id, method, params } => {
                json!({ "jsonrpc": "2.0", "id": rpc_id, "result": codex_decision(&method, &params, decision) })
            }
            Pending::Claude { request_id, input, suggestions } => {
                json!({ "type": "control_response", "response": {
                    "subtype": "success", "request_id": request_id, "response": claude_decision(input, suggestions, decision),
                }})
            }
        };
        let sent = p.write(&reply);
        (self.inner.sink)(session_id, Event::Resolved(id.to_string()));
        sent
    }

    /// The thread is gone (deleted): so is its process.
    pub fn close(&self, session_id: &str) {
        if let Some(p) = self.get(session_id) {
            p.kill();
            self.forget(&p);
        }
    }

    /// Everything goes when the app does.
    pub fn stop_all(&self) {
        let procs: Vec<Arc<Proc>> = self.inner.procs.lock().map(|p| p.values().cloned().collect()).unwrap_or_default();
        for p in procs {
            p.kill();
        }
    }

    fn emit(&self, p: &Proc, e: Event) {
        (self.inner.sink)(&p.session_id, e);
    }

    fn line(&self, p: &Proc, line: &str) {
        self.emit(p, Event::Line(p.redactor.redact(line)));
    }

    fn ask(&self, p: &Proc, approval: Approval, pending: Pending) {
        if let Ok(mut st) = p.state.lock() {
            st.approvals.insert(approval.id.clone(), pending);
        }
        let mut approval = approval;
        approval.detail = p.redactor.redact(&approval.detail);
        approval.reason = approval.reason.map(|r| p.redactor.redact(&r));
        self.emit(p, Event::Approval(approval));
    }

    fn resolved(&self, p: &Proc, id: &str) {
        let had = p.state.lock().map(|mut s| s.approvals.remove(id).is_some()).unwrap_or(false);
        if had {
            self.emit(p, Event::Resolved(id.to_string()));
        }
    }

    fn turn_end(&self, p: &Proc, error: Option<String>) {
        let (was_busy, interrupted, left) = match p.state.lock() {
            Ok(mut st) => {
                let was = st.busy;
                st.busy = false;
                st.turn_id = None;
                st.last_used = Instant::now();
                let left: Vec<String> = st.approvals.drain().map(|(id, _)| id).collect();
                (was, st.interrupted, left)
            }
            Err(_) => return,
        };
        for id in left {
            self.emit(p, Event::Resolved(id));
        }
        if was_busy {
            let error = if interrupted { None } else { error.map(|e| p.redactor.redact(&e)) };
            self.emit(p, Event::TurnEnd { error, interrupted });
        }
    }

    fn on_exit(&self, p: &Arc<Proc>) {
        let code = p.child.lock().ok().and_then(|mut c| c.wait().ok()).and_then(|s| s.code());
        let (killed, stderr) = match p.state.lock() {
            Ok(mut st) => {
                st.dead = true;
                st.waiting.clear();
                (st.killed, st.stderr.trim().to_string())
            }
            Err(_) => (true, String::new()),
        };
        self.forget(p);
        let error = if killed {
            "The agent was stopped.".to_string()
        } else if stderr.is_empty() {
            format!("The agent exited (code {}).", code.map(|c| c.to_string()).unwrap_or_else(|| "signal".into()))
        } else {
            stderr
        };
        self.turn_end(p, Some(error));
        drop(p.guards.lock().ok().and_then(|mut g| g.take()));
    }

    // ── Claude Code: stream-json + control protocol ─────────────────────────

    fn on_claude_line(&self, p: &Proc, line: &str) {
        let Ok(ev) = serde_json::from_str::<Value>(line) else { return };
        match ev["type"].as_str().unwrap_or("") {
            "control_request" => {
                let request_id = ev["request_id"].as_str().unwrap_or("").to_string();
                let req = &ev["request"];
                if req["subtype"] != "can_use_tool" {
                    // Nothing else is ours to serve (hooks, SDK MCP servers).
                    let _ = p.write(&json!({ "type": "control_response", "response": {
                        "subtype": "error", "request_id": request_id, "error": "not supported by AgentArea Desktop",
                    }}));
                    return;
                }
                let tool = req["tool_name"].as_str().unwrap_or("tool").to_string();
                let input = req["input"].clone();
                if is_agentarea_tool(&tool) {
                    let _ = p.write(&json!({ "type": "control_response", "response": {
                        "subtype": "success", "request_id": request_id,
                        "response": claude_decision(input, Value::Null, Decision::Allow),
                    }}));
                    return;
                }
                let approval = claude_approval(&request_id, &tool, req);
                self.ask(p, approval, Pending::Claude { request_id, input, suggestions: req["permission_suggestions"].clone() });
            }
            "control_cancel_request" => self.resolved(p, ev["request_id"].as_str().unwrap_or("")),
            "result" => {
                let error = (ev["is_error"] == true).then(|| {
                    ev["result"].as_str().filter(|s| !s.is_empty()).or(ev["subtype"].as_str()).unwrap_or("Claude failed").to_string()
                });
                self.turn_end(p, error);
            }
            // Only text deltas are worth the trip to the UI.
            "stream_event" => {
                let e = &ev["event"];
                if e["type"] == "content_block_delta" && e["delta"]["type"] == "text_delta" {
                    self.line(p, line);
                }
            }
            "assistant" | "user" => self.line(p, line),
            "system" if ev["subtype"] == "init" => self.line(p, line),
            _ => {}
        }
    }

    // ── Codex: app-server JSON-RPC ──────────────────────────────────────────

    fn on_codex_line(&self, p: &Proc, line: &str) {
        let Ok(msg) = serde_json::from_str::<Value>(line) else { return };
        let method = msg["method"].as_str().map(str::to_string);
        match (msg.get("id"), method) {
            // A reply to one of our requests.
            (Some(id), None) => {
                let tx = id.as_u64().and_then(|id| p.state.lock().ok()?.waiting.remove(&id));
                if let Some(tx) = tx {
                    let _ = tx.send(msg);
                }
            }
            (Some(id), Some(method)) => self.on_codex_request(p, id.clone(), &method, &msg["params"]),
            (None, Some(method)) => self.on_codex_notification(p, &method, &msg["params"], line),
            _ => {}
        }
    }

    fn on_codex_notification(&self, p: &Proc, method: &str, params: &Value, line: &str) {
        match method {
            "turn/started" => {
                if let (Some(id), Ok(mut st)) = (params["turn"]["id"].as_str(), p.state.lock()) {
                    st.turn_id = Some(id.to_string());
                }
            }
            "turn/completed" => {
                let turn = &params["turn"];
                let error = (turn["status"] == "failed")
                    .then(|| turn["error"]["message"].as_str().unwrap_or("Codex failed").to_string());
                self.turn_end(p, error.map(|e| codex_error_text(&e)));
            }
            "serverRequest/resolved" => {
                let id = codex_approval_id(&params["requestId"]);
                self.resolved(p, &id);
            }
            "thread/started" => {
                if let (Some(id), Ok(mut st)) = (params["thread"]["id"].as_str(), p.state.lock()) {
                    st.thread_id = Some(id.to_string());
                }
                self.line(p, line);
            }
            "item/started" | "item/completed" => {
                let item = &params["item"];
                if method == "item/started" && item["type"] == "fileChange" {
                    let paths = item["changes"].as_array().into_iter().flatten();
                    let paths = paths.filter_map(|c| c["path"].as_str().map(str::to_string)).collect();
                    if let (Some(id), Ok(mut st)) = (item["id"].as_str(), p.state.lock()) {
                        st.file_changes.insert(id.to_string(), paths);
                    }
                }
                if !matches!(item["type"].as_str(), Some("userMessage" | "reasoning")) {
                    self.line(p, line);
                }
            }
            "item/agentMessage/delta" => self.line(p, line),
            _ => {}
        }
    }

    fn on_codex_request(&self, p: &Proc, rpc_id: Value, method: &str, params: &Value) {
        let id = codex_approval_id(&rpc_id);
        let reply = |result: Value| {
            let _ = p.write(&json!({ "jsonrpc": "2.0", "id": rpc_id, "result": result }));
        };
        let decisions = params["availableDecisions"].as_array();
        let can_session = decisions.is_none_or(|d| d.iter().any(|x| x == "acceptForSession"));
        let reason = params["reason"].as_str().map(str::to_string);
        let approval = match method {
            "item/commandExecution/requestApproval" | "execCommandApproval" => {
                let command = match &params["command"] {
                    Value::Array(parts) => parts.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(" "),
                    v => v.as_str().unwrap_or("").to_string(),
                };
                Approval { id: id.clone(), kind: "shell", title: "Run a command".into(), detail: unwrap_shell(&command), reason, can_session }
            }
            "item/fileChange/requestApproval" | "applyPatchApproval" => {
                let paths = match params["itemId"].as_str() {
                    Some(item) => p.state.lock().ok().and_then(|s| s.file_changes.get(item).cloned()).unwrap_or_default(),
                    None => params["fileChanges"].as_object().map(|m| m.keys().cloned().collect()).unwrap_or_default(),
                };
                let extra_root = params["grantRoot"].as_str().is_some();
                if !extra_root && !paths.is_empty() && paths.iter().all(|f| inside(&p.cwd, f)) {
                    reply(codex_decision(method, params, Decision::Allow));
                    return;
                }
                let detail = if paths.is_empty() { "files outside the thread's folder".to_string() } else { paths.join("\n") };
                Approval { id: id.clone(), kind: "edit", title: "Edit files".into(), detail, reason, can_session: true }
            }
            "item/permissions/requestApproval" => Approval {
                id: id.clone(),
                kind: "permissions",
                title: "Grant more access".into(),
                detail: serde_json::to_string_pretty(&params["permissions"]).unwrap_or_default(),
                reason,
                can_session: true,
            },
            // Pre-approved servers never ask; anything else asking for a form is declined.
            "mcpServer/elicitation/request" => {
                return reply(json!({ "action": "decline", "content": null, "_meta": null }));
            }
            _ => {
                let _ = p.write(&json!({ "jsonrpc": "2.0", "id": rpc_id, "error": { "code": -32601, "message": format!("{method} is not supported") } }));
                return;
            }
        };
        self.ask(p, approval, Pending::Codex { rpc_id: rpc_id.clone(), method: method.to_string(), params: params.clone() });
    }
}

/// How to open the Codex thread once `codex app-server` is up.
pub struct CodexThread {
    pub resume_id: Option<String>,
    pub cwd: PathBuf,
    pub model: Option<String>,
}

pub fn codex_init_params() -> Value {
    json!({
        "clientInfo": { "name": "agentarea-desktop", "title": "AgentArea Desktop", "version": env!("CARGO_PKG_VERSION") },
        "capabilities": {
            "experimentalApi": false,
            "requestAttestation": false,
            // Streams the UI never shows.
            "optOutNotificationMethods": [
                "item/reasoning/summaryTextDelta", "item/reasoning/summaryPartAdded", "item/reasoning/textDelta",
                "item/commandExecution/outputDelta", "item/fileChange/outputDelta", "thread/tokenUsage/updated",
                "account/rateLimits/updated", "rawResponseItem/completed", "rawResponse/completed",
            ],
        },
    })
}

/// Handshake, then `thread/resume` (falling back to a new thread when Codex
/// no longer has it) or `thread/start`.
fn codex_open(p: &Proc, t: CodexThread) -> Result<(), String> {
    p.rpc("initialize", codex_init_params(), RPC_TIMEOUT)?;
    p.write(&json!({ "jsonrpc": "2.0", "method": "initialized" }))?;
    let mut params = json!({
        "cwd": t.cwd.display().to_string(),
        // Codex's own default ("Auto"): commands run in the sandbox without
        // asking; the model asks only to step outside it (see on_codex_request).
        "approvalPolicy": "on-request",
        "sandbox": "workspace-write",
    });
    if let Some(m) = &t.model {
        params["model"] = json!(m);
    }
    let opened = match &t.resume_id {
        Some(id) => {
            let mut resume = params.clone();
            resume["threadId"] = json!(id);
            resume["excludeTurns"] = json!(true);
            p.rpc("thread/resume", resume, RPC_TIMEOUT).or_else(|e| {
                eprintln!("codex: resume {id} failed ({e}); starting a new thread");
                p.rpc("thread/start", params, RPC_TIMEOUT)
            })?
        }
        None => p.rpc("thread/start", params, RPC_TIMEOUT)?,
    };
    let id = opened["thread"]["id"].as_str().ok_or("codex didn't return a thread id")?;
    p.state.lock().map_err(|_| "poisoned")?.thread_id = Some(id.to_string());
    Ok(())
}

/// Approval ids the UI sees: Codex request ids are numbers, Claude's are uuids.
fn codex_approval_id(rpc_id: &Value) -> String {
    match rpc_id {
        Value::String(s) => format!("codex-{s}"),
        v => format!("codex-{v}"),
    }
}

fn codex_decision(method: &str, params: &Value, d: Decision) -> Value {
    match method {
        "item/permissions/requestApproval" => {
            let granted = match d {
                Decision::Deny => json!({}),
                _ => {
                    let asked = params["permissions"].as_object().cloned().unwrap_or_default();
                    Value::Object(asked.into_iter().filter(|(_, v)| !v.is_null()).collect())
                }
            };
            json!({ "permissions": granted, "scope": if d == Decision::AllowSession { "session" } else { "turn" } })
        }
        "execCommandApproval" | "applyPatchApproval" => json!({ "decision": match d {
            Decision::Allow => json!("approved"),
            Decision::AllowSession => json!("approved_for_session"),
            Decision::Deny => json!({ "denied": { "rejection": "The user declined." } }),
        }}),
        _ => json!({ "decision": match d {
            Decision::Allow => "accept",
            Decision::AllowSession => "acceptForSession",
            Decision::Deny => "decline",
        }}),
    }
}

fn claude_decision(input: Value, suggestions: Value, d: Decision) -> Value {
    match d {
        Decision::Deny => json!({ "behavior": "deny", "message": "The user denied this action." }),
        Decision::Allow => json!({ "behavior": "allow", "updatedInput": input }),
        // The CLI's own suggestions, kept for this session only (never written
        // to the folder's settings files).
        Decision::AllowSession => {
            let rules: Vec<Value> = suggestions
                .as_array()
                .into_iter()
                .flatten()
                .map(|s| {
                    let mut s = s.clone();
                    s["destination"] = json!("session");
                    s
                })
                .collect();
            json!({ "behavior": "allow", "updatedInput": input, "updatedPermissions": rules })
        }
    }
}

fn claude_approval(request_id: &str, tool: &str, req: &Value) -> Approval {
    let input = &req["input"];
    let (kind, title, detail) = match tool {
        "Bash" => ("shell", "Run a command".to_string(), input["command"].as_str().unwrap_or("").to_string()),
        "Edit" | "Write" | "MultiEdit" | "NotebookEdit" => (
            "edit",
            "Edit files".to_string(),
            input["file_path"].as_str().or(input["notebook_path"].as_str()).unwrap_or("").to_string(),
        ),
        _ => ("tool", format!("Use {}", req["display_name"].as_str().unwrap_or(tool)), serde_json::to_string_pretty(input).unwrap_or_default()),
    };
    let reason = req["description"].as_str().or(req["decision_reason"].as_str()).map(str::to_string);
    let can_session = req["permission_suggestions"].as_array().is_some_and(|s| !s.is_empty());
    Approval { id: request_id.to_string(), kind, title, detail, reason, can_session }
}

/// The AgentArea servers' tools never ask: they are governed server-side.
fn is_agentarea_tool(tool: &str) -> bool {
    ["mcp__agentarea__", "mcp__agentarea_secrets__", "mcp__agentarea_data__"].iter().any(|p| tool.starts_with(p))
}

/// `/bin/zsh -lc 'ls -la'` → `ls -la`, for showing the user what runs.
fn unwrap_shell(command: &str) -> String {
    for shell in ["/bin/zsh -lc ", "/bin/bash -lc ", "bash -lc ", "zsh -lc ", "/bin/sh -c "] {
        if let Some(rest) = command.strip_prefix(shell) {
            let rest = rest.trim();
            if rest.len() >= 2 && rest.starts_with('\'') && rest.ends_with('\'') && !rest[1..rest.len() - 1].contains('\'') {
                return rest[1..rest.len() - 1].to_string();
            }
            return rest.to_string();
        }
    }
    command.to_string()
}

/// Codex error messages are often the API's JSON; show just its message.
fn codex_error_text(raw: &str) -> String {
    serde_json::from_str::<Value>(raw)
        .ok()
        .and_then(|v| v["error"]["message"].as_str().map(str::to_string))
        .unwrap_or_else(|| raw.to_string())
}

/// Whether `path` (absolute, or relative to `root`) stays inside `root`.
fn inside(root: &Path, path: &str) -> bool {
    let path = Path::new(path);
    if path.components().any(|c| c == Component::ParentDir) {
        return false;
    }
    if path.is_relative() {
        return true;
    }
    let canon = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    path.starts_with(root) || path.starts_with(&canon)
}

/// Models the user's Codex account offers, from `model/list` on a short-lived
/// `codex app-server` (with the same private CODEX_HOME the threads use).
pub fn codex_model_list(program: &Path, home: &Path) -> Result<Vec<Value>, String> {
    let mut child = Command::new(program)
        .env("CODEX_HOME", home)
        .args(["app-server", "--disable", "apps", "--disable", "plugins", "--disable", "hooks"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("codex: {e}"))?;
    let mut stdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
            if msg["id"] == 2 {
                let _ = tx.send(msg);
                return;
            }
        }
    });
    let requests = [
        json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": codex_init_params() }),
        json!({ "jsonrpc": "2.0", "method": "initialized" }),
        json!({ "jsonrpc": "2.0", "id": 2, "method": "model/list", "params": { "limit": 100 } }),
    ];
    for r in requests {
        let _ = writeln!(stdin, "{r}");
    }
    let _ = stdin.flush();
    let reply = rx.recv_timeout(Duration::from_secs(20));
    let _ = child.kill();
    let _ = child.wait();
    let reply = reply.map_err(|_| "codex app-server didn't list models".to_string())?;
    if let Some(err) = reply.get("error") {
        return Err(format!("model/list: {}", err["message"].as_str().unwrap_or("failed")));
    }
    Ok(reply["result"]["data"].as_array().cloned().unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unwraps_login_shells() {
        assert_eq!(unwrap_shell("/bin/zsh -lc 'touch a && ls'"), "touch a && ls");
        assert_eq!(unwrap_shell("/bin/zsh -lc \"echo 'x'\""), "\"echo 'x'\"");
        assert_eq!(unwrap_shell("ls -la"), "ls -la");
    }

    #[test]
    fn paths_inside_the_folder() {
        let root = std::env::temp_dir();
        assert!(inside(&root, "notes.md"));
        assert!(inside(&root, &root.join("a/b.txt").display().to_string()));
        assert!(!inside(&root, "/etc/hosts"));
        assert!(!inside(&root, "../x"));
        assert!(!inside(&root, &root.join("../x").display().to_string()));
    }

    #[test]
    fn maps_decisions_per_protocol() {
        let cmd = "item/commandExecution/requestApproval";
        assert_eq!(codex_decision(cmd, &Value::Null, Decision::Allow), json!({ "decision": "accept" }));
        assert_eq!(codex_decision(cmd, &Value::Null, Decision::AllowSession), json!({ "decision": "acceptForSession" }));
        assert_eq!(codex_decision(cmd, &Value::Null, Decision::Deny), json!({ "decision": "decline" }));
        let perms = json!({ "permissions": { "network": { "enabled": true }, "fileSystem": null } });
        assert_eq!(
            codex_decision("item/permissions/requestApproval", &perms, Decision::Allow),
            json!({ "permissions": { "network": { "enabled": true } }, "scope": "turn" })
        );
        assert_eq!(
            codex_decision("item/permissions/requestApproval", &perms, Decision::Deny),
            json!({ "permissions": {}, "scope": "turn" })
        );

        let input = json!({ "command": "rm -rf build" });
        assert_eq!(claude_decision(input.clone(), Value::Null, Decision::Allow), json!({ "behavior": "allow", "updatedInput": input }));
        assert_eq!(claude_decision(input.clone(), Value::Null, Decision::Deny)["behavior"], "deny");
        let suggestions = json!([{ "type": "addRules", "rules": [{ "toolName": "Bash" }], "behavior": "allow", "destination": "localSettings" }]);
        let session = claude_decision(input, suggestions, Decision::AllowSession);
        assert_eq!(session["updatedPermissions"][0]["destination"], "session", "never written to the folder's settings");
    }

    #[test]
    fn agentarea_tools_are_preapproved() {
        assert!(is_agentarea_tool("mcp__agentarea__search"));
        assert!(is_agentarea_tool("mcp__agentarea_data__cloud_read"));
        assert!(!is_agentarea_tool("mcp__agentarea_other__x"));
        assert!(!is_agentarea_tool("Bash"));
    }
}
