//! Plugin backends: a plugin's `server` (plugin.json) is an MCP server that
//! runs either here (a stdio child started from the plugin folder) or in the
//! AgentArea workspace (the MCP instance named `plugin:<id>`, reached through
//! the platform's governed proxy `/v1/mcp/{instance_id}/mcp`).
//!
//! Whichever place it runs, the app exposes it at one stable local endpoint on
//! the vault's loopback server (secrets_mcp.rs): `/plugins/<id>/mcp`, MCP
//! Streamable HTTP, bearer token per caller. Agent CLIs get that URL as the
//! MCP server `plugin_<id>`; the plugin's own page reaches it through the
//! bridge (`plugin.tools` / `plugin.call`), which calls the same endpoint.
//!
//! Nothing is used before the user approves the plugin folder as reviewed
//! (plugin_trust.rs): no local process, no cloud instance.
//!
//! Placement (`auto` | `local` | `cloud`, per plugin in plugins-state.json) is
//! resolved against the active workspace the frontend reports
//! (`plugin_backends_set_context`). An agent thread's token is pinned to the
//! placement it started with (`Pin`); the plugin page follows the current one.
//! Tokens are never stored here: the cloud path takes the app's live access
//! token at request time.
//!
//! See docs/plugin-backends.md.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::local_mcp::{self, Server};
use crate::plugin_trust;
use crate::secrets_mcp::{PluginToken, SecretsMcp};
use crate::vault::Redactor;

// ── manifest ─────────────────────────────────────────────────────────────────

/// plugin.json `server`: where the plugin's MCP backend can run.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct ServerDecl {
    #[serde(default)]
    pub local: Option<LocalDecl>,
    #[serde(default)]
    pub cloud: Option<CloudDecl>,
}

/// A stdio MCP server started from the plugin folder.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct LocalDecl {
    /// program and arguments, e.g. `["node", "server.mjs"]`
    pub command: Vec<String>,
    /// Extra environment. Not in the plugin list; the approval review shows it.
    #[serde(default, skip_serializing)]
    pub env: BTreeMap<String, String>,
}

/// The workspace MCP instance named `plugin:<id>`.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct CloudDecl {
    pub instance: String,
}

fn valid_env_key(k: &str) -> bool {
    let mut chars = k.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// The only instance name plugin `id` may use.
pub fn instance_name(id: &str) -> String {
    format!("plugin:{id}")
}

impl ServerDecl {
    /// A usable declaration for plugin `id`: at least one place, a non-empty
    /// command, plain env names, and a cloud instance named `plugin:<id>` (a
    /// plugin can't point at the workspace's other MCP servers).
    pub fn validate(&self, id: &str) -> Result<(), String> {
        if self.local.is_none() && self.cloud.is_none() {
            return Err("server needs \"local\" or \"cloud\"".into());
        }
        if let Some(l) = &self.local {
            if l.command.first().is_none_or(|c| c.trim().is_empty()) {
                return Err("server.local.command is empty".into());
            }
            if l.command.iter().chain(l.env.values()).any(|s| s.contains('\0')) {
                return Err("server.local must not contain NUL".into());
            }
            if let Some(k) = l.env.keys().find(|k| !valid_env_key(k)) {
                return Err(format!("invalid env variable name: {k}"));
            }
        }
        if let Some(c) = &self.cloud {
            let want = instance_name(id);
            if c.instance != want {
                return Err(format!("server.cloud.instance must be \"{want}\" (got \"{}\")", c.instance));
            }
        }
        Ok(())
    }
}

// ── placement ────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[default]
    Auto,
    Local,
    Cloud,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Where {
    Local,
    Cloud,
    Unavailable,
}

/// A plugin's placement for the active workspace.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Placement {
    pub mode: Mode,
    pub resolved: Where,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// the matched cloud instance (resolved = cloud)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,
}

/// One MCP instance of the active workspace (`GET /v1/workspaces/{slug}/mcp-server-instances/`).
#[derive(Clone, Debug, Deserialize)]
pub struct Instance {
    pub id: String,
    pub name: String,
}

/// The active workspace as the frontend sees it.
#[derive(Clone, Debug, Default)]
pub struct Context {
    /// environment id ("ru", "local"); names the data folder
    pub env: String,
    pub slug: Option<String>,
    pub api_base: Option<String>,
    /// None = not signed in, or the listing failed (`error` says why)
    pub instances: Option<Vec<Instance>>,
    pub error: Option<String>,
}

pub const NOT_ALLOWED: &str = "not allowed yet: review and allow the backend in Apps → Plugins";

/// Where `decl` runs under `mode` in `ctx`.
///
/// - cloud is possible when the workspace has exactly one MCP instance whose
///   `name` equals `cloud.instance` (= `plugin:<id>`; exact, case-sensitive;
///   names are unique per workspace on the platform);
/// - local is possible when `local` is declared;
/// - `auto` prefers cloud, then local; `local` / `cloud` take only that one;
/// - either way only once the user approved the plugin (`approved`).
pub fn resolve(decl: &ServerDecl, mode: Mode, approved: bool, ctx: &Context) -> Placement {
    let cloud = decl.cloud.as_ref().map(|c| cloud_instance(c, ctx));
    let local = decl.local.is_some();
    let pick: Result<(Where, Option<String>), String> = match mode {
        Mode::Cloud => match cloud {
            Some(Ok(id)) => Ok((Where::Cloud, Some(id))),
            Some(Err(e)) => Err(e),
            None => Err("the plugin declares no cloud instance".into()),
        },
        Mode::Local if local => Ok((Where::Local, None)),
        Mode::Local => Err("the plugin declares no local server".into()),
        Mode::Auto => match (cloud, local) {
            (Some(Ok(id)), _) => Ok((Where::Cloud, Some(id))),
            (_, true) => Ok((Where::Local, None)),
            (Some(Err(e)), false) => Err(e),
            (None, false) => Err("the plugin declares no server".into()),
        },
    };
    let none = |reason: String| Placement { mode, resolved: Where::Unavailable, reason: Some(reason), instance: None };
    match pick {
        Ok(_) if !approved => none(NOT_ALLOWED.into()),
        Ok((resolved, instance)) => Placement { mode, resolved, reason: None, instance },
        Err(e) => none(e),
    }
}

fn valid_instance_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

fn cloud_instance(c: &CloudDecl, ctx: &Context) -> Result<String, String> {
    let Some(instances) = &ctx.instances else {
        return Err(ctx.error.clone().unwrap_or_else(|| "not signed in to AgentArea".into()));
    };
    let ws = ctx.slug.as_deref().unwrap_or("the workspace");
    let name = c.instance.as_str();
    let found: Vec<&Instance> = instances.iter().filter(|i| i.name == name).collect();
    match found[..] {
        [one] if valid_instance_id(&one.id) => Ok(one.id.clone()),
        [_] => Err(format!("instance \"{name}\" has an unexpected id")),
        [] => Err(format!("no MCP instance named \"{name}\" in {ws}")),
        _ => Err(format!("{} MCP instances are named \"{name}\" in {ws}", found.len())),
    }
}

/// The platform's governed MCP proxy for one instance.
pub fn cloud_url(api_base: &str, instance: &str) -> String {
    format!("{}/v1/mcp/{instance}/mcp", api_base.trim_end_matches('/'))
}

fn valid_segment(s: &str) -> bool {
    !s.is_empty() && s.len() <= 120 && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// `<root>/workspaces/<env>/<slug>/plugins/<id>`: a backend's data follows the
/// workspace; with none known, `workspaces/local/default`.
pub fn data_dir(root: &Path, ctx: &Context, id: &str) -> PathBuf {
    let (env, slug) = match &ctx.slug {
        Some(slug) if valid_segment(slug) && valid_segment(&ctx.env) => (ctx.env.as_str(), slug.as_str()),
        _ => ("local", "default"),
    };
    root.join("workspaces").join(env).join(slug).join("plugins").join(id)
}

// ── routing ──────────────────────────────────────────────────────────────────

/// An enabled plugin that declares a server, with its stored settings.
#[derive(Clone, Debug)]
pub struct Entry {
    pub id: String,
    /// the plugin folder (cwd of the local server)
    pub dir: PathBuf,
    pub decl: ServerDecl,
    pub mode: Mode,
    /// a genuine approval matches the folder as it is (cached check)
    pub approved: bool,
    /// the fingerprint a genuine approval names; a start re-hashes the folder
    /// and must match it
    pub approval: Option<String>,
}

/// Where a request goes.
#[derive(Clone, Debug, PartialEq)]
pub enum Target {
    Local { data_dir: PathBuf },
    Cloud { url: String, api_base: String },
}

impl Target {
    /// Names the route: a session made on one key is no good on another.
    pub fn key(&self) -> String {
        match self {
            Target::Local { data_dir } => format!("local:{}", data_dir.display()),
            Target::Cloud { url, .. } => format!("cloud:{url}"),
        }
    }
}

/// An agent thread's placement, fixed when its process started.
pub struct PinGuard {
    backends: Weak<Backends>,
    n: u64,
    pub target: Target,
}

impl Drop for PinGuard {
    fn drop(&mut self) {
        if let Some(b) = self.backends.upgrade() {
            if let Ok(mut pins) = b.pins.lock() {
                pins.remove(&self.n);
            }
            // A thread from another workspace may have kept a process alive.
            b.reconcile();
        }
    }
}

/// Who holds a gateway token: the plugin it opens, an agent's pinned
/// placement and redactor (none for the plugin page), and the route its MCP
/// session was made on.
pub struct Caller {
    pub plugin: String,
    pub pin: Option<PinGuard>,
    pub redactor: Option<Redactor>,
    session: Mutex<Option<String>>,
}

impl Caller {
    pub fn new(plugin: &str, pin: Option<PinGuard>, redactor: Option<Redactor>) -> Caller {
        Caller { plugin: plugin.into(), pin, redactor, session: Mutex::default() }
    }

    /// `initialize` ties the caller's session to route `key`; a later message
    /// on another route (placement or workspace moved) is refused once, and
    /// the client is expected to initialize again (MCP: 404 = session gone).
    pub fn session_ok(&self, key: &str, initializing: bool) -> bool {
        let Ok(mut s) = self.session.lock() else { return false };
        if initializing {
            *s = Some(key.to_string());
            return true;
        }
        match s.as_deref() {
            Some(k) if k != key => {
                *s = None;
                false
            }
            _ => true,
        }
    }
}

/// A running local server.
pub struct LocalProc {
    server: Arc<Server>,
    /// its `initialize` result, given to every client that initializes
    init: Value,
    /// the folder fingerprint it was started from
    approval: String,
    /// (name, value) of its env: kept out of error text sent to callers
    scrub: Vec<(String, String)>,
}

impl LocalProc {
    fn scrub(&self, text: &str) -> String {
        scrub(&self.scrub, text)
    }
}

fn scrub(env: &[(String, String)], text: &str) -> String {
    env.iter().filter(|(_, v)| v.len() >= 4).fold(text.to_string(), |t, (k, v)| t.replace(v.as_str(), &format!("[env:{k}]")))
}

#[derive(Default)]
struct SlotState {
    proc: Option<Arc<LocalProc>>,
    /// last failed start: retried only after `BACKOFF`
    failed: Option<(Instant, String)>,
}

type Lookup = Box<dyn Fn() -> Vec<Entry> + Send + Sync>;
type Slot = Arc<Mutex<SlotState>>;
type SlotKey = (String, PathBuf);

/// The plugin's page talks to its backend through the gateway too.
struct UiClient {
    http: local_mcp::Http,
    initialized: Mutex<bool>,
    /// the route it was made for; another one gets a fresh client
    key: String,
    _token: PluginToken,
}

pub struct Backends {
    lookup: Lookup,
    /// `~/AgentArea` (data folders live under it)
    root: PathBuf,
    ctx: Mutex<Context>,
    /// one process per (plugin, data folder): threads of two workspaces can
    /// run side by side
    locals: Mutex<HashMap<SlotKey, Slot>>,
    pins: Mutex<HashMap<u64, (String, Target)>>,
    next_pin: AtomicU64,
    ui: Mutex<HashMap<String, Arc<UiClient>>>,
}

/// Stop grace for a local server.
const STOP_GRACE: Duration = Duration::from_millis(1000);
/// A server that failed to start isn't retried sooner than this.
const BACKOFF: Duration = Duration::from_secs(5);

fn stop_later(servers: Vec<Arc<Server>>) {
    if !servers.is_empty() {
        std::thread::spawn(move || local_mcp::stop_servers(&servers, STOP_GRACE));
    }
}

impl Backends {
    pub fn new(root: PathBuf, lookup: Lookup) -> Backends {
        Backends {
            lookup,
            root,
            ctx: Mutex::default(),
            locals: Mutex::default(),
            pins: Mutex::default(),
            next_pin: AtomicU64::new(1),
            ui: Mutex::default(),
        }
    }

    fn context(&self) -> Context {
        self.ctx.lock().map(|c| c.clone()).unwrap_or_default()
    }

    fn entry(&self, id: &str) -> Option<Entry> {
        (self.lookup)().into_iter().find(|e| e.id == id)
    }

    /// Placement of every enabled plugin with a server, by id.
    pub fn placements(&self) -> Vec<(String, Placement)> {
        let ctx = self.context();
        (self.lookup)().into_iter().map(|e| (e.id.clone(), resolve(&e.decl, e.mode, e.approved, &ctx))).collect()
    }

    /// Where `e` goes in `ctx`, or why not.
    fn target_in(&self, e: &Entry, ctx: &Context) -> Result<Target, String> {
        let p = resolve(&e.decl, e.mode, e.approved, ctx);
        match p.resolved {
            Where::Unavailable => Err(p.reason.unwrap_or_default()),
            Where::Local => Ok(Target::Local { data_dir: data_dir(&self.root, ctx, &e.id) }),
            Where::Cloud => match (&ctx.api_base, &p.instance) {
                (Some(base), Some(inst)) => Ok(Target::Cloud { url: cloud_url(base, inst), api_base: base.clone() }),
                _ => Err("not signed in to AgentArea".into()),
            },
        }
    }

    /// Plugins whose backend can be reached now, with their route key (for
    /// agent runs: a different route restarts the thread's process).
    pub fn available(&self) -> Vec<(String, String)> {
        let ctx = self.context();
        (self.lookup)().into_iter().filter_map(|e| Some((e.id.clone(), self.target_in(&e, &ctx).ok()?.key()))).collect()
    }

    pub fn set_context(&self, ctx: Context) {
        if let Ok(mut c) = self.ctx.lock() {
            *c = ctx;
        }
        self.reconcile();
    }

    /// Fix the current placement of `id` for an agent thread.
    pub fn pin(self: &Arc<Self>, id: &str) -> Option<PinGuard> {
        let e = self.entry(id)?;
        let target = self.target_in(&e, &self.context()).ok()?;
        let n = self.next_pin.fetch_add(1, Ordering::Relaxed);
        self.pins.lock().ok()?.insert(n, (id.to_string(), target.clone()));
        Some(PinGuard { backends: Arc::downgrade(self), n, target })
    }

    /// Where a request for `id` goes: the caller's pinned placement (still
    /// subject to the plugin being enabled, approved and declaring it) or the
    /// current one. Starts nothing.
    pub fn target(&self, id: &str, pin: Option<&Target>) -> Result<Target, String> {
        let Some(e) = self.entry(id) else {
            self.stop_plugin(id);
            return Err(format!("no enabled plugin {id} with a server"));
        };
        if !e.approved {
            // revoked, or the folder changed since it was approved
            self.stop_plugin(id);
            return Err(NOT_ALLOWED.into());
        }
        match pin {
            Some(t @ Target::Local { .. }) if e.decl.local.is_some() => Ok(t.clone()),
            Some(t @ Target::Cloud { .. }) if e.decl.cloud.is_some() => Ok(t.clone()),
            Some(_) => Err("the plugin no longer declares the backend this thread started with".into()),
            None => self.target_in(&e, &self.context()),
        }
    }

    /// Stop every local server of `id` (that isn't mid-start).
    fn stop_plugin(&self, id: &str) {
        let slots: Vec<Slot> =
            self.locals.lock().map(|m| m.iter().filter(|(k, _)| k.0 == id).map(|(_, v)| v.clone()).collect()).unwrap_or_default();
        let stop = slots.iter().filter_map(|s| s.try_lock().ok().and_then(|mut s| s.proc.take())).map(|l| l.server.clone()).collect();
        stop_later(stop);
    }

    fn slot(&self, id: &str, data_dir: &Path) -> Slot {
        self.locals
            .lock()
            .map(|mut m| m.entry((id.to_string(), data_dir.to_path_buf())).or_default().clone())
            .unwrap_or_default()
    }

    /// The plugin's local server for `data_dir`, started (or restarted when it
    /// died or the folder changed) as needed. A start re-hashes the whole
    /// plugin folder and needs it to match the approval.
    pub fn start(&self, id: &str, data_dir: &Path) -> Result<Arc<LocalProc>, String> {
        let e = self.entry(id).ok_or_else(|| format!("no enabled plugin {id} with a server"))?;
        let decl = e.decl.local.as_ref().ok_or("the plugin declares no local server")?;
        let approval = e.approval.clone().filter(|_| e.approved).ok_or(NOT_ALLOWED)?;
        let slot = self.slot(id, data_dir);
        let mut state = slot.lock().map_err(|_| "poisoned")?;
        if let Some(l) = state.proc.as_ref() {
            if l.server.is_alive() && l.approval == approval {
                return Ok(l.clone());
            }
        }
        if let Some(old) = state.proc.take() {
            local_mcp::stop_servers(std::slice::from_ref(&old.server), STOP_GRACE);
        }
        if let Some((at, err)) = &state.failed {
            if at.elapsed() < BACKOFF {
                return Err(err.clone());
            }
        }
        let scrub_env: Vec<(String, String)> = decl.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        let started = (|| {
            if plugin_trust::fingerprint(id, &e.dir)? != approval {
                return Err(format!("the plugin folder changed since it was approved; {NOT_ALLOWED}"));
            }
            std::fs::create_dir_all(data_dir).map_err(|err| format!("data folder: {err}"))?;
            let mut env = decl.env.clone();
            env.insert("AGENTAREA_PLUGIN_ID".into(), id.to_string());
            env.insert("AGENTAREA_DATA_DIR".into(), data_dir.display().to_string());
            local_mcp::start_stdio(&decl.command[0], &decl.command[1..], &env, &e.dir)
                .map_err(|err| format!("local server: {}", scrub(&scrub_env, &err)))
        })();
        let (server, init) = match started {
            Ok(s) => s,
            Err(err) => {
                state.failed = Some((Instant::now(), err.clone()));
                return Err(err);
            }
        };
        let server = Arc::new(server);
        // Revoked while it started: don't keep it.
        if self.entry(id).and_then(|e| e.approval.filter(|_| e.approved)).as_deref() != Some(approval.as_str()) {
            stop_later(vec![server]);
            return Err(NOT_ALLOWED.into());
        }
        state.failed = None;
        let proc = Arc::new(LocalProc { server, init, approval, scrub: scrub_env });
        state.proc = Some(proc.clone());
        Ok(proc)
    }

    /// Stop local servers that no longer should run: plugin gone, disabled or
    /// not approved, placed elsewhere, or for a data folder that neither the
    /// current workspace nor a running thread's pin uses; and reap dead ones.
    /// They start again on the next request.
    pub fn reconcile(&self) {
        let ctx = self.context();
        let entries = (self.lookup)();
        let pins: Vec<(String, Target)> = self.pins.lock().map(|p| p.values().cloned().collect()).unwrap_or_default();
        let slots: Vec<(SlotKey, Slot)> =
            self.locals.lock().map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect()).unwrap_or_default();
        let mut stop = Vec::new();
        for ((id, dir), slot) in slots {
            // A server that is starting right now is checked by its next request.
            let Ok(mut state) = slot.try_lock() else { continue };
            let Some(l) = state.proc.as_ref() else { continue };
            let wanted = Target::Local { data_dir: dir.clone() };
            let keep = l.server.is_alive()
                && entries.iter().find(|e| e.id == id).is_some_and(|e| {
                    e.approved
                        && e.decl.local.is_some()
                        && (self.target_in(e, &ctx).ok().as_ref() == Some(&wanted) || pins.iter().any(|(p, t)| *p == id && *t == wanted))
                });
            if !keep {
                if let Some(l) = state.proc.take() {
                    stop.push(l.server.clone());
                }
            }
        }
        stop_later(stop);
    }

    /// App exit: every local server goes (one mid-start is left to its process group's fate).
    pub fn stop_all(&self) {
        let slots: Vec<Slot> = self.locals.lock().map(|mut m| m.drain().map(|(_, v)| v).collect()).unwrap_or_default();
        let servers: Vec<Arc<Server>> =
            slots.iter().filter_map(|s| s.try_lock().ok().and_then(|mut s| s.proc.take())).map(|l| l.server.clone()).collect();
        local_mcp::stop_servers(&servers, Duration::from_millis(500));
    }

    /// One MCP request from the plugin's own page, through the gateway (so it
    /// sees exactly what agents see). A client is made per plugin and route,
    /// and initialized once.
    pub fn ui_request(&self, mcp: &SecretsMcp, id: &str, method: &str, params: Value) -> Result<Value, String> {
        let key = self.target(id, None).map_err(|e| format!("backend unavailable: {e}"))?.key();
        for attempt in 0..2 {
            let client = self.ui_client(mcp, id, &key)?;
            {
                let mut init = client.initialized.lock().map_err(|_| "poisoned")?;
                if !*init {
                    client.http.request("initialize", local_mcp::initialize_params())?;
                    client.http.notify("notifications/initialized", json!({}))?;
                    *init = true;
                }
            }
            match client.http.request(method, params.clone()) {
                // The session is gone (backend restarted or moved): start over once.
                Err(err) if attempt == 0 && err.contains("HTTP 404") => {
                    if let Ok(mut ui) = self.ui.lock() {
                        ui.remove(id);
                    }
                }
                other => return other,
            }
        }
        Err("unreachable".into())
    }

    fn ui_client(&self, mcp: &SecretsMcp, id: &str, key: &str) -> Result<Arc<UiClient>, String> {
        let mut ui = self.ui.lock().map_err(|_| "poisoned")?;
        if let Some(c) = ui.get(id).filter(|c| c.key == key) {
            return Ok(c.clone());
        }
        let token = mcp.issue_plugin_token(id);
        let headers = BTreeMap::from([("Authorization".to_string(), format!("Bearer {}", token.token))]);
        let client = Arc::new(UiClient {
            http: local_mcp::Http::new(&mcp.plugin_url(id), &headers)?,
            initialized: Mutex::new(false),
            key: key.to_string(),
            _token: token,
        });
        ui.insert(id.to_string(), client.clone());
        Ok(client)
    }
}

/// A gateway message for a local server; `None` for notifications and
/// client responses. The server was initialized when it started, so a
/// client's `initialize` gets that answer (with the client's protocol version
/// when it asked for an older known one); everything else is forwarded (with
/// the server's own request ids: several clients share one process).
pub fn local_message(local: &LocalProc, msg: &Value) -> Option<Value> {
    let method = msg.get("method").and_then(Value::as_str)?;
    let id = msg.get("id")?.clone();
    let params = msg.get("params").cloned().unwrap_or_else(|| json!({}));
    let result = match method {
        "initialize" => {
            let mut init = local.init.clone();
            let asked = params["protocolVersion"].as_str().unwrap_or("");
            let has = init["protocolVersion"].as_str().unwrap_or("").to_string();
            if crate::secrets_mcp::PROTOCOL_VERSIONS.contains(&asked) && asked < has.as_str() {
                init["protocolVersion"] = json!(asked);
            }
            Ok(init)
        }
        "ping" => Ok(json!({})),
        _ => local.server.request(method, params, local_mcp::REQUEST_TIMEOUT),
    };
    Some(match result {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Err(message) => json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32603, "message": local.scrub(&message) } }),
    })
}

/// The app's backends: enabled plugins from ~/AgentArea/plugins.
pub fn global() -> &'static Arc<Backends> {
    static BACKENDS: OnceLock<Arc<Backends>> = OnceLock::new();
    BACKENDS.get_or_init(|| {
        let root = PathBuf::from(std::env::var("HOME").unwrap_or_default()).join("AgentArea");
        Arc::new(Backends::new(root, Box::new(crate::plugins::backend_entries)))
    })
}

// ── commands ─────────────────────────────────────────────────────────────────

/// The frontend reports the active workspace on start, sign-in/out and
/// workspace switch: env id, slug, API base and the workspace's MCP
/// instances (`null` when signed out or the listing failed, with `error`).
#[tauri::command]
pub fn plugin_backends_set_context(
    env: String,
    slug: Option<String>,
    api_base: Option<String>,
    instances: Option<Vec<Instance>>,
    error: Option<String>,
) -> Result<(), String> {
    if !valid_segment(&env) {
        return Err(format!("invalid environment: {env}"));
    }
    if let Some(s) = &slug {
        if !valid_segment(s) {
            return Err(format!("invalid workspace: {s}"));
        }
    }
    if let Some(b) = &api_base {
        if !(b.starts_with("https://") || b.starts_with("http://")) || b.chars().any(char::is_whitespace) {
            return Err(format!("invalid API base: {b}"));
        }
    }
    global().set_context(Context { env, slug, api_base, instances, error });
    Ok(())
}

#[derive(Serialize)]
pub struct Status {
    id: String,
    #[serde(flatten)]
    placement: Placement,
}

/// Placement of every enabled plugin with a server.
#[tauri::command]
pub fn plugin_backend_status() -> Vec<Status> {
    global().placements().into_iter().map(|(id, placement)| Status { id, placement }).collect()
}

async fn ui_call(app: tauri::AppHandle, id: String, method: &'static str, params: Value) -> Result<Value, String> {
    use tauri::Manager;
    let mcp = app.try_state::<SecretsMcp>().map(|s| s.inner().clone()).ok_or("the local MCP gateway isn't running")?;
    tauri::async_runtime::spawn_blocking(move || global().ui_request(&mcp, &id, method, params))
        .await
        .map_err(|e| e.to_string())?
}

/// `plugin.tools()`: the backend's tools/list. The frontend passes the id of
/// the frame that asked (plugin pages have no IPC of their own).
#[tauri::command]
pub async fn plugin_backend_tools(app: tauri::AppHandle, id: String) -> Result<Value, String> {
    ui_call(app, id, "tools/list", json!({})).await
}

/// `plugin.call(name, args)`: tools/call on the plugin's backend.
#[tauri::command]
pub async fn plugin_backend_call(app: tauri::AppHandle, id: String, name: String, arguments: Option<Value>) -> Result<Value, String> {
    ui_call(app, id, "tools/call", json!({ "name": name, "arguments": arguments.unwrap_or_else(|| json!({})) })).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::AtomicBool;

    fn decl(local: bool, cloud: bool) -> ServerDecl {
        ServerDecl {
            local: local.then(|| LocalDecl { command: vec!["node".into(), "server.js".into()], env: BTreeMap::new() }),
            cloud: cloud.then(|| CloudDecl { instance: "plugin:demo".into() }),
        }
    }

    const INSTANCE: &str = "6f1c2a3b-0000-4000-8000-000000000001";

    fn ctx(instances: Option<&[(&str, &str)]>) -> Context {
        Context {
            env: "ru".into(),
            slug: Some("acme".into()),
            api_base: Some("https://api.example".into()),
            instances: instances.map(|l| l.iter().map(|(id, name)| Instance { id: id.to_string(), name: name.to_string() }).collect()),
            error: None,
        }
    }

    #[test]
    fn validates_declarations() {
        let ok: ServerDecl = serde_json::from_str(r#"{"local":{"command":["node","s.js"],"env":{"A_1":"x"}},"cloud":{"instance":"plugin:x"}}"#).unwrap();
        assert!(ok.validate("x").is_ok());
        assert_eq!(ok.local.as_ref().unwrap().env["A_1"], "x");
        assert!(!serde_json::to_string(&ok).unwrap().contains("A_1"), "env stays out of the plugin list");
        for bad in [
            r#"{}"#,
            r#"{"local":{"command":[]}}"#,
            r#"{"local":{"command":[" "]}}"#,
            r#"{"local":{"command":["x"],"env":{"A=B":"1"}}}"#,
            r#"{"local":{"command":["x\u0000"]}}"#,
            r#"{"cloud":{"instance":"  "}}"#,
        ] {
            let d: ServerDecl = serde_json::from_str(bad).unwrap();
            assert!(d.validate("x").is_err(), "{bad}");
        }
        assert!(serde_json::from_str::<ServerDecl>(r#"{"local":{"command":"node s.js"}}"#).is_err(), "command is an array");
    }

    /// H1: a plugin may only name its own instance, `plugin:<id>`.
    #[test]
    fn cloud_instance_must_be_the_plugins_own() {
        let d = |inst: &str| ServerDecl { local: None, cloud: Some(CloudDecl { instance: inst.into() }) };
        assert!(d("plugin:demo").validate("demo").is_ok());
        for other in ["gmail", "plugin:other", "Plugin:demo", "plugin:demo ", " plugin:demo", "plugin:"] {
            let err = d(other).validate("demo").unwrap_err();
            assert!(err.contains("must be \"plugin:demo\""), "{other}: {err}");
        }
    }

    #[test]
    fn resolves_placement() {
        use Where::*;
        let present = [(INSTANCE, "plugin:demo"), ("aaaa", "other")];
        let absent = [("aaaa", "other")];
        // (mode, local declared, cloud declared, instance present) → resolved
        let cases = [
            (Mode::Auto, true, true, true, Cloud),
            (Mode::Auto, true, true, false, Local),
            (Mode::Auto, false, true, true, Cloud),
            (Mode::Auto, false, true, false, Unavailable),
            (Mode::Auto, true, false, true, Local),
            (Mode::Auto, true, false, false, Local),
            (Mode::Local, true, true, true, Local),
            (Mode::Local, true, true, false, Local),
            (Mode::Local, false, true, true, Unavailable),
            (Mode::Local, false, true, false, Unavailable),
            (Mode::Cloud, true, true, true, Cloud),
            (Mode::Cloud, true, true, false, Unavailable),
            (Mode::Cloud, false, true, true, Cloud),
            (Mode::Cloud, true, false, true, Unavailable),
        ];
        for (mode, local, cloud, has, want) in cases {
            let c = ctx(Some(if has { &present[..] } else { &absent[..] }));
            let p = resolve(&decl(local, cloud), mode, true, &c);
            assert_eq!(p.resolved, want, "{mode:?} local={local} cloud={cloud} instance={has}: {p:?}");
            assert_eq!(p.mode, mode);
            assert_eq!(p.reason.is_some(), want == Unavailable, "{p:?}");
            assert_eq!(p.instance.as_deref(), (want == Cloud).then_some(INSTANCE));
            // H1: nothing, cloud included, without the user's approval.
            let p = resolve(&decl(local, cloud), mode, false, &c);
            assert_eq!(p.resolved, Unavailable, "unapproved {mode:?} local={local} cloud={cloud} instance={has}");
        }
        let p = resolve(&decl(false, true), Mode::Auto, true, &ctx(Some(&absent)));
        assert_eq!(p.reason.as_deref(), Some("no MCP instance named \"plugin:demo\" in acme"));
        let p = resolve(&decl(true, false), Mode::Cloud, true, &ctx(Some(&present)));
        assert!(p.reason.unwrap().contains("no cloud instance"));
        let p = resolve(&decl(false, true), Mode::Cloud, false, &ctx(Some(&present)));
        assert_eq!(p.reason.as_deref(), Some(NOT_ALLOWED), "cloud needs the approval too");
    }

    #[test]
    fn placement_edge_cases() {
        // Signed out: cloud is out, auto falls back to local.
        let out = Context { env: "ru".into(), ..Context::default() };
        assert_eq!(resolve(&decl(true, true), Mode::Auto, true, &out).resolved, Where::Local);
        let p = resolve(&decl(false, true), Mode::Auto, true, &out);
        assert_eq!(p.reason.as_deref(), Some("not signed in to AgentArea"));
        let failed = Context { error: Some("couldn't list MCP instances (500)".into()), ..out.clone() };
        assert_eq!(resolve(&decl(false, true), Mode::Cloud, true, &failed).reason.as_deref(), Some("couldn't list MCP instances (500)"));
        // Exact names only; duplicates and odd ids are refused.
        assert_eq!(resolve(&decl(false, true), Mode::Cloud, true, &ctx(Some(&[(INSTANCE, "Plugin:Demo")]))).resolved, Where::Unavailable);
        let dup = resolve(&decl(false, true), Mode::Cloud, true, &ctx(Some(&[(INSTANCE, "plugin:demo"), ("bbbb", "plugin:demo")])));
        assert!(dup.reason.unwrap().starts_with("2 MCP instances"));
        let odd = resolve(&decl(false, true), Mode::Cloud, true, &ctx(Some(&[("../x", "plugin:demo")])));
        assert_eq!(odd.resolved, Where::Unavailable);
        assert_eq!(cloud_url("https://api.example/", INSTANCE), format!("https://api.example/v1/mcp/{INSTANCE}/mcp"));
    }

    #[test]
    fn data_dirs_follow_the_workspace() {
        let root = Path::new("/h/AgentArea");
        assert_eq!(data_dir(root, &ctx(None), "demo"), Path::new("/h/AgentArea/workspaces/ru/acme/plugins/demo"));
        let none = Context { env: "ru".into(), ..Context::default() };
        assert_eq!(data_dir(root, &none, "demo"), Path::new("/h/AgentArea/workspaces/local/default/plugins/demo"));
        let odd = Context { env: "ru".into(), slug: Some("../x".into()), ..Context::default() };
        assert_eq!(data_dir(root, &odd, "demo"), Path::new("/h/AgentArea/workspaces/local/default/plugins/demo"));
    }

    #[test]
    fn sessions_follow_the_route() {
        let c = Caller::new("p", None, None);
        assert!(c.session_ok("local:/a", false), "never initialized here: let it through");
        assert!(c.session_ok("local:/a", true));
        assert!(c.session_ok("local:/a", false));
        assert!(!c.session_ok("cloud:x", false), "moved: the session is gone");
        assert!(c.session_ok("cloud:x", true), "initialize again");
        assert!(c.session_ok("cloud:x", false));
    }

    // ── through the gateway ──────────────────────────────────────────────────

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("aa-backends-{name}-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A copy of the example plugin (tests edit it).
    fn demo_copy(root: &Path) -> PathBuf {
        let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../examples/plugins/backend-demo");
        let dir = root.join("plugins/backend-demo");
        std::fs::create_dir_all(&dir).unwrap();
        for f in ["plugin.json", "server.mjs", "index.html"] {
            std::fs::copy(src.join(f), dir.join(f)).unwrap();
        }
        dir
    }

    /// The example plugin's manifest, as the app would read it.
    fn demo_decl(dir: &Path) -> ServerDecl {
        let manifest: Value = serde_json::from_str(&std::fs::read_to_string(dir.join("plugin.json")).unwrap()).unwrap();
        let decl: ServerDecl = serde_json::from_value(manifest["server"].clone()).unwrap();
        decl.validate("backend-demo").unwrap();
        decl
    }

    /// JSON-RPC over raw HTTP; (status, headers, body).
    fn http(method: &str, url: &str, token: &str, session: Option<&str>, body: &Value) -> (u16, String, String) {
        let (addr, path) = url.trim_start_matches("http://").split_once('/').unwrap();
        let mut s = TcpStream::connect(addr).unwrap();
        let body = if method == "POST" { body.to_string() } else { String::new() };
        let session = session.map(|v| format!("Mcp-Session-Id: {v}\r\n")).unwrap_or_default();
        write!(s, "{method} /{path} HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {token}\r\n{session}Accept: application/json, text/event-stream\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).unwrap();
        let (head, body) = out.split_once("\r\n\r\n").unwrap();
        let status = head.split_whitespace().nth(1).unwrap().parse().unwrap();
        let body = if head.to_ascii_lowercase().contains("transfer-encoding: chunked") { dechunk(body) } else { body.to_string() };
        (status, head.to_string(), body)
    }

    fn rpc(url: &str, token: &str, session: Option<&str>, body: &Value) -> (u16, String, String) {
        http("POST", url, token, session, body)
    }

    fn dechunk(mut s: &str) -> String {
        let mut out = String::new();
        while let Some((size, rest)) = s.split_once("\r\n") {
            let n = usize::from_str_radix(size.trim(), 16).unwrap();
            if n == 0 {
                break;
            }
            out.push_str(&rest[..n]);
            s = &rest[n + 2..];
        }
        out
    }

    fn call(id: u64, name: &str, args: Value) -> Value {
        json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":name,"arguments":args}})
    }

    fn text(reply: &str) -> String {
        let v: Value = serde_json::from_str(reply).unwrap();
        v["result"]["content"][0]["text"].as_str().unwrap_or_default().to_string()
    }

    fn init() -> Value {
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{}}})
    }

    fn mcp_for(root: &Path, backends: Arc<Backends>) -> SecretsMcp {
        SecretsMcp::start_with(crate::vault::Vault::at(root.join("vault"), "AgentArea Desktop Secrets (test)"), backends).unwrap()
    }

    /// The demo plugin's own stdio server behind `/plugins/backend-demo/mcp`,
    /// started on demand: echo, a counter persisted in the workspace data
    /// folder, the plugin page's path (ui_request), workspaces side by side
    /// for pinned threads, stop on revoke / file change / placement change.
    #[test]
    fn local_backend_through_the_gateway() {
        if !crate::cli_bin("node").exists() {
            println!("node not found; skipped");
            return;
        }
        let root = temp("local");
        let dir = demo_copy(&root);
        let mode = Arc::new(Mutex::new(Mode::Local));
        let approved = Arc::new(AtomicBool::new(true));
        let approval = plugin_trust::fingerprint("backend-demo", &dir).unwrap();
        let (m, a, d) = (mode.clone(), approved.clone(), dir.clone());
        let backends = Arc::new(Backends::new(
            root.clone(),
            Box::new(move || {
                // Like plugins::backend_entries: approved = the approval matches the folder now.
                let ok = a.load(std::sync::atomic::Ordering::SeqCst)
                    && plugin_trust::fingerprint("backend-demo", &d).ok().as_deref() == Some(approval.as_str());
                vec![Entry {
                    id: "backend-demo".into(),
                    dir: d.clone(),
                    decl: demo_decl(&d),
                    mode: *m.lock().unwrap(),
                    approved: ok,
                    approval: Some(approval.clone()),
                }]
            }),
        ));
        backends.set_context(Context { env: "ru".into(), slug: Some("acme".into()), ..Context::default() });
        let mcp = mcp_for(&root, backends.clone());
        let url = mcp.plugin_url("backend-demo");
        assert!(url.ends_with("/plugins/backend-demo/mcp"), "{url}");

        // Auth: no token, someone else's token, a token for another plugin.
        assert_eq!(rpc(&url, "nope", None, &init()).0, 401);
        let other = mcp.issue_plugin_token("other-plugin");
        assert_eq!(rpc(&url, &other.token, None, &init()).0, 401, "a token opens only its own plugin");
        let token = mcp.issue_plugin_token("backend-demo");

        // M5: a GET (SSE stream) on a local route is refused and starts nothing.
        assert_eq!(http("GET", &url, &token.token, None, &json!({})).0, 405);
        assert_eq!(http("DELETE", &url, &token.token, None, &json!({})).0, 405);
        assert!(backends.locals.lock().unwrap().is_empty(), "nothing started");

        let (status, _, body) = rpc(&url, &token.token, None, &init());
        assert_eq!(status, 200, "{body}");
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["id"], 1);
        assert_eq!(v["result"]["serverInfo"]["name"], "backend-demo");
        let old = json!({"jsonrpc":"2.0","id":2,"method":"initialize","params":{"protocolVersion":"2025-03-26"}});
        let (_, _, body) = rpc(&url, &token.token, None, &old);
        assert_eq!(serde_json::from_str::<Value>(&body).unwrap()["result"]["protocolVersion"], "2025-03-26", "an older client keeps its version");
        assert_eq!(rpc(&url, &token.token, None, &json!({"jsonrpc":"2.0","method":"notifications/initialized"})).0, 202);

        let (_, _, body) = rpc(&url, &token.token, None, &json!({"jsonrpc":"2.0","id":"t","method":"tools/list"}));
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["id"], "t", "the client's id comes back");
        let names: Vec<&str> = v["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["echo", "counter_increment"]);

        let (_, _, body) = rpc(&url, &token.token, None, &call(3, "echo", json!({"text":"hi there"})));
        assert_eq!(text(&body), "hi there");
        let (_, _, body) = rpc(&url, &token.token, None, &call(4, "counter_increment", json!({})));
        assert_eq!(text(&body), "1");
        let (_, _, body) = rpc(&url, &token.token, None, &call(5, "counter_increment", json!({"by": 2})));
        assert_eq!(text(&body), "3");
        let counter = root.join("workspaces/ru/acme/plugins/backend-demo/counter.json");
        let saved: Value = serde_json::from_str(&std::fs::read_to_string(&counter).unwrap()).unwrap();
        assert_eq!(saved["count"], 3, "persisted in AGENTAREA_DATA_DIR");

        // M4: an agent's token redacts vault values in results.
        let agent = mcp.issue_agent_plugin_token("backend-demo", Redactor::new([("API_KEY", "sk-very-secret")])).unwrap();
        rpc(&url, &agent.token, None, &init());
        let (_, _, body) = rpc(&url, &agent.token, None, &call(6, "echo", json!({"text":"key=sk-very-secret"})));
        assert_eq!(text(&body), "key=[secret:API_KEY]", "{body}");

        // The plugin page's path: the same endpoint, via the bridge's client.
        let tools = backends.ui_request(&mcp, "backend-demo", "tools/list", json!({})).unwrap();
        assert_eq!(tools["tools"].as_array().unwrap().len(), 2);
        let r = backends.ui_request(&mcp, "backend-demo", "tools/call", json!({"name":"counter_increment","arguments":{}})).unwrap();
        assert_eq!(r["content"][0]["text"], "4");

        // M2: the user switches workspace. The agent stays pinned to acme
        // (same process, same counter); the page and new callers move to
        // "other", with its own process and data, side by side.
        let acme_proc = backends.slot("backend-demo", &root.join("workspaces/ru/acme/plugins/backend-demo")).lock().unwrap().proc.clone().unwrap();
        backends.set_context(Context { env: "ru".into(), slug: Some("other".into()), ..Context::default() });
        let (_, _, body) = rpc(&url, &agent.token, None, &call(7, "counter_increment", json!({})));
        assert_eq!(text(&body), "5", "pinned thread still on acme");
        // M1: the unpinned caller initialized on acme; its session is gone now.
        let (status, _, body) = rpc(&url, &token.token, None, &call(8, "counter_increment", json!({})));
        assert_eq!(status, 404, "{body}");
        rpc(&url, &token.token, None, &init());
        let (_, _, body) = rpc(&url, &token.token, None, &call(9, "counter_increment", json!({})));
        assert_eq!(text(&body), "1", "the other workspace has its own counter");
        assert!(acme_proc.server.is_alive(), "acme's process lives while the pinned thread does");
        drop(agent);
        std::thread::sleep(Duration::from_millis(1500));
        assert!(!acme_proc.server.is_alive(), "and goes with it");

        // H2: an edit anywhere in the folder revokes: refused at once, process stopped.
        std::fs::write(dir.join("server.mjs"), std::fs::read_to_string(dir.join("server.mjs")).unwrap() + "\n// edited\n").unwrap();
        let (status, _, body) = rpc(&url, &token.token, None, &call(10, "echo", json!({"text":"x"})));
        assert_eq!(status, 503, "{body}");
        assert!(body.contains("not allowed yet"), "{body}");
        std::thread::sleep(Duration::from_millis(1500));
        assert!(backends.locals.lock().unwrap().values().all(|s| s.lock().unwrap().proc.is_none()), "stopped");

        // Placed in the cloud with nothing there: unavailable.
        approved.store(false, std::sync::atomic::Ordering::SeqCst);
        *mode.lock().unwrap() = Mode::Cloud;
        backends.reconcile();
        assert!(backends.ui_request(&mcp, "backend-demo", "tools/list", json!({})).unwrap_err().contains("unavailable"));

        backends.stop_all();
        let _ = std::fs::remove_dir_all(root);
    }

    /// A start re-hashes the folder: an edit that the cached status hasn't
    /// seen yet (or an mtime-preserving one) still can't run.
    #[test]
    fn start_rechecks_the_whole_folder() {
        let root = temp("recheck");
        let dir = demo_copy(&root);
        let approval = plugin_trust::fingerprint("backend-demo", &dir).unwrap();
        std::fs::write(dir.join("extra.js"), "process.exit(1)").unwrap();
        let d = dir.clone();
        let backends = Backends::new(
            root.clone(),
            Box::new(move || {
                vec![Entry { id: "backend-demo".into(), dir: d.clone(), decl: demo_decl(&d), mode: Mode::Local, approved: true, approval: Some(approval.clone()) }]
            }),
        );
        let err = backends.start("backend-demo", &root.join("data")).err().unwrap();
        assert!(err.contains("changed since it was approved"), "{err}");
        assert!(!root.join("data").exists(), "nothing ran");
        // Backoff: the same error, without another attempt.
        assert_eq!(backends.start("backend-demo", &root.join("data")).err().unwrap(), err);
        let _ = std::fs::remove_dir_all(root);
    }

    /// What the fake platform proxy saw.
    struct Seen {
        line: String,
        auth: String,
        session: String,
    }

    /// A fake `POST /v1/mcp/{id}/mcp`: answers `n` requests with JSON-RPC
    /// results echoing the method and `secret`, and an Mcp-Session-Id.
    fn fake_proxy(n: usize) -> (String, std::sync::mpsc::Receiver<Seen>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for stream in listener.incoming().take(n).flatten() {
                let mut r = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                r.read_line(&mut line).unwrap();
                let (mut auth, mut session, mut len) = (String::new(), String::new(), 0usize);
                loop {
                    let mut h = String::new();
                    if r.read_line(&mut h).unwrap() == 0 || h.trim().is_empty() {
                        break;
                    }
                    let (k, v) = h.split_once(':').unwrap();
                    match k.trim().to_ascii_lowercase().as_str() {
                        "authorization" => auth = v.trim().into(),
                        "mcp-session-id" => session = v.trim().into(),
                        "content-length" => len = v.trim().parse().unwrap(),
                        _ => {}
                    }
                }
                let mut body = vec![0; len];
                r.read_exact(&mut body).unwrap();
                let msg: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                let _ = tx.send(Seen { line: line.trim().into(), auth: auth.clone(), session });
                let (status, reply) = if auth.starts_with("Bearer live-") {
                    (200, json!({"jsonrpc":"2.0","id":msg["id"],"result":{"upstream":msg["method"],"leak":"tok=sk-very-secret"}}).to_string())
                } else {
                    (401, "{\"detail\":\"no\"}".to_string())
                };
                let mut w = stream;
                let _ = write!(w, "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nMcp-Session-Id: up-1\r\nWWW-Authenticate: Bearer\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}", reply.len());
            }
        });
        (base, rx)
    }

    /// Cloud placement: the gateway forwards to `{api}/v1/mcp/{id}/mcp` with
    /// the app's live token (never the caller's), passes the session through,
    /// redacts for agents, needs the approval, and refuses when signed out.
    #[test]
    fn cloud_backend_forwards_to_the_platform_proxy() {
        let _live = crate::data_mcp::LIVE_TOKEN_TEST.lock().unwrap_or_else(|e| e.into_inner());
        let root = temp("cloud");
        let (api, seen) = fake_proxy(4);
        let approved = Arc::new(AtomicBool::new(true));
        let a = approved.clone();
        let backends = Arc::new(Backends::new(
            root.clone(),
            Box::new(move || {
                let ok = a.load(std::sync::atomic::Ordering::SeqCst);
                vec![Entry { id: "demo".into(), dir: PathBuf::from("/nonexistent"), decl: decl(true, true), mode: Mode::Auto, approved: ok, approval: None }]
            }),
        ));
        let with_instance = || Context {
            env: "ru".into(),
            slug: Some("acme".into()),
            api_base: Some(api.clone()),
            instances: Some(vec![Instance { id: INSTANCE.into(), name: "plugin:demo".into() }]),
            error: None,
        };
        backends.set_context(with_instance());
        assert_eq!(backends.available().iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(), ["demo"]);
        let mcp = mcp_for(&root, backends.clone());
        let url = mcp.plugin_url("demo");
        let token = mcp.issue_plugin_token("demo");
        crate::data_mcp::set_cloud_token(api.clone(), Some("live-1".into()));

        let (status, head, body) = rpc(&url, &token.token, Some("up-0"), &json!({"jsonrpc":"2.0","id":9,"method":"tools/list"}));
        assert_eq!(status, 200, "{head}\n{body}");
        assert_eq!(serde_json::from_str::<Value>(&body).unwrap()["result"]["upstream"], "tools/list");
        let up = seen.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(up.line, format!("POST /v1/mcp/{INSTANCE}/mcp HTTP/1.1"));
        assert_eq!(up.auth, "Bearer live-1", "the app's live token, not the gateway token");
        assert_eq!(up.session, "up-0");
        let head = head.to_ascii_lowercase();
        assert!(head.contains("mcp-session-id: up-1") && !head.contains("www-authenticate"), "{head}");

        // M4: an agent's token gets the body redacted; renewed token used at once.
        crate::data_mcp::set_cloud_token(api.clone(), Some("live-2".into()));
        let agent = mcp.issue_agent_plugin_token("demo", Redactor::new([("API_KEY", "sk-very-secret")])).unwrap();
        let (_, _, body) = rpc(&url, &agent.token, None, &json!({"jsonrpc":"2.0","id":10,"method":"ping"}));
        assert_eq!(seen.recv_timeout(Duration::from_secs(5)).unwrap().auth, "Bearer live-2");
        assert!(body.contains("tok=[secret:API_KEY]") && !body.contains("sk-very-secret"), "{body}");

        // H1: revoked → nothing reaches the platform.
        approved.store(false, std::sync::atomic::Ordering::SeqCst);
        let (status, _, body) = rpc(&url, &token.token, None, &json!({"jsonrpc":"2.0","id":11,"method":"ping"}));
        assert_eq!(status, 503);
        assert!(body.contains("not allowed yet"), "{body}");
        assert!(backends.available().is_empty());
        approved.store(true, std::sync::atomic::Ordering::SeqCst);

        // Signed out: refused here, nothing reaches the platform.
        crate::data_mcp::set_cloud_token(api.clone(), None);
        let (status, _, body) = rpc(&url, &token.token, None, &json!({"jsonrpc":"2.0","id":12,"method":"ping"}));
        assert_eq!(status, 401, "{body}");
        assert!(seen.recv_timeout(Duration::from_millis(300)).is_err());

        // GET (SSE) is forwarded on a cloud route.
        crate::data_mcp::set_cloud_token(api.clone(), Some("live-3".into()));
        http("GET", &url, &token.token, Some("up-1"), &json!({}));
        assert!(seen.recv_timeout(Duration::from_secs(5)).unwrap().line.starts_with("GET /v1/mcp/"));
        crate::data_mcp::set_cloud_token(api, None);
        let _ = std::fs::remove_dir_all(root);
    }
}
