//! Plugin backends: a plugin's `server` (plugin.json) is an MCP server that
//! runs either here (a stdio child started from the plugin folder) or in the
//! AgentArea workspace (an existing MCP instance, reached through the
//! platform's governed proxy `/v1/mcp/{instance_id}/mcp`).
//!
//! Whichever place it runs, the app exposes it at one stable local endpoint on
//! the vault's loopback server (secrets_mcp.rs): `/plugins/<id>/mcp`, MCP
//! Streamable HTTP, bearer token per caller. Agent CLIs get that URL as the
//! MCP server `plugin_<id>`; the plugin's own page reaches it through the
//! bridge (`plugin.tools` / `plugin.call`), which calls the same endpoint.
//!
//! Placement (`auto` | `local` | `cloud`, per plugin in plugins-state.json) is
//! resolved per request against the active workspace the frontend reports
//! (`plugin_backends_set_context`), so a workspace switch or sign-out moves the
//! backend without the callers noticing. Tokens are never stored here: the
//! cloud path takes the app's live access token at request time.
//!
//! See docs/plugin-backends.md.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::local_mcp::{self, Server};
use crate::secrets_mcp::{PluginToken, SecretsMcp};

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
    /// program and arguments, e.g. `["node", "server.js"]`
    pub command: Vec<String>,
    /// Extra environment; kept out of what the UI is sent.
    #[serde(default, skip_serializing)]
    pub env: BTreeMap<String, String>,
}

/// An MCP instance in the AgentArea workspace, matched by its name.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct CloudDecl {
    pub instance: String,
}

fn valid_env_key(k: &str) -> bool {
    let mut chars = k.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

impl ServerDecl {
    /// A usable declaration: at least one place, a non-empty command, plain
    /// env names, a non-empty instance name.
    pub fn validate(&self) -> Result<(), String> {
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
            let name = c.instance.trim();
            if name.is_empty() || name.len() > 255 {
                return Err("server.cloud.instance must be 1–255 characters".into());
            }
        }
        Ok(())
    }
}

/// What the user approves before a plugin's local server may run: its exact
/// command and env. Any edit to either needs a new approval.
pub fn fingerprint(local: &LocalDecl) -> String {
    json!({ "command": local.command, "env": local.env }).to_string()
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

/// Where `decl` runs under `mode` in `ctx`.
///
/// - cloud is possible when the workspace has exactly one MCP instance whose
///   `name` equals `cloud.instance` (exact, case-sensitive; names are unique
///   per workspace on the platform);
/// - local is possible when `local` is declared and the user approved it;
/// - `auto` prefers cloud, then local; `local` / `cloud` take only that one.
pub fn resolve(decl: &ServerDecl, mode: Mode, approved: bool, ctx: &Context) -> Placement {
    let cloud = decl.cloud.as_ref().map(|c| cloud_instance(c, ctx));
    let local = decl.local.as_ref().map(|_| {
        if approved {
            Ok(())
        } else {
            Err("its local server isn't allowed yet (Apps → Plugins)".to_string())
        }
    });
    let at = |resolved, instance| Placement { mode, resolved, reason: None, instance };
    let none = |reason: String| Placement { mode, resolved: Where::Unavailable, reason: Some(reason), instance: None };
    match mode {
        Mode::Cloud => match cloud {
            Some(Ok(id)) => at(Where::Cloud, Some(id)),
            Some(Err(e)) => none(e),
            None => none("the plugin declares no cloud instance".into()),
        },
        Mode::Local => match local {
            Some(Ok(())) => at(Where::Local, None),
            Some(Err(e)) => none(e),
            None => none("the plugin declares no local server".into()),
        },
        Mode::Auto => match (cloud, local) {
            (Some(Ok(id)), _) => at(Where::Cloud, Some(id)),
            (_, Some(Ok(()))) => at(Where::Local, None),
            (Some(Err(c)), Some(Err(l))) => none(format!("{c}; {l}")),
            (Some(Err(e)), None) | (None, Some(Err(e))) => none(e),
            (None, None) => none("the plugin declares no server".into()),
        },
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
    let name = c.instance.trim();
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

// ── running backends ─────────────────────────────────────────────────────────

/// An enabled plugin that declares a server, with its stored settings.
#[derive(Clone, Debug)]
pub struct Entry {
    pub id: String,
    /// the plugin folder (cwd of the local server)
    pub dir: PathBuf,
    pub decl: ServerDecl,
    pub mode: Mode,
    /// the user allowed this exact local command (`fingerprint`)
    pub approved: bool,
}

/// A running local server.
pub struct LocalProc {
    server: Arc<Server>,
    /// its `initialize` result, given to every client that initializes
    init: Value,
    data_dir: PathBuf,
    fingerprint: String,
}

/// Where a gateway request goes.
pub enum Route {
    Local(Arc<LocalProc>),
    Cloud { url: String, api_base: String },
    Unavailable(String),
}

type Lookup = Box<dyn Fn() -> Vec<Entry> + Send + Sync>;
type Slot = Arc<Mutex<Option<Arc<LocalProc>>>>;

/// The plugin's page talks to its backend through the gateway too.
struct UiClient {
    http: local_mcp::Http,
    initialized: AtomicBool,
    /// the placement it was made for; another one gets a fresh client
    key: String,
    _token: PluginToken,
}

pub struct Backends {
    lookup: Lookup,
    /// `~/AgentArea` (data folders live under it)
    root: PathBuf,
    ctx: Mutex<Context>,
    locals: Mutex<HashMap<String, Slot>>,
    ui: Mutex<HashMap<String, Arc<UiClient>>>,
}

/// Stop grace for a local server.
const STOP_GRACE: Duration = Duration::from_millis(1000);

impl Backends {
    pub fn new(root: PathBuf, lookup: Lookup) -> Backends {
        Backends { lookup, root, ctx: Mutex::default(), locals: Mutex::default(), ui: Mutex::default() }
    }

    fn context(&self) -> Context {
        self.ctx.lock().map(|c| c.clone()).unwrap_or_default()
    }

    fn entry(&self, id: &str) -> Option<Entry> {
        (self.lookup)().into_iter().find(|e| e.id == id)
    }

    fn placement_of(&self, e: &Entry, ctx: &Context) -> Placement {
        resolve(&e.decl, e.mode, e.approved, ctx)
    }

    /// Placement of every enabled plugin with a server, by id.
    pub fn placements(&self) -> Vec<(String, Placement)> {
        let ctx = self.context();
        (self.lookup)().into_iter().map(|e| (e.id.clone(), self.placement_of(&e, &ctx))).collect()
    }

    /// Plugins whose backend can be reached now (for agent runs).
    pub fn available(&self) -> Vec<String> {
        self.placements().into_iter().filter(|(_, p)| p.resolved != Where::Unavailable).map(|(id, _)| id).collect()
    }

    pub fn set_context(&self, ctx: Context) {
        if let Ok(mut c) = self.ctx.lock() {
            *c = ctx;
        }
        self.reconcile();
    }

    /// Where a request for `id` goes now; a local server is started on demand.
    pub fn route(&self, id: &str) -> Route {
        let Some(e) = self.entry(id) else {
            return Route::Unavailable(format!("no enabled plugin {id} with a server"));
        };
        let ctx = self.context();
        let p = self.placement_of(&e, &ctx);
        if p.resolved != Where::Local {
            // e.g. plugin.json changed and its approval no longer matches
            self.stop_local(id);
        }
        match p.resolved {
            Where::Unavailable => Route::Unavailable(p.reason.unwrap_or_default()),
            Where::Cloud => match (&ctx.api_base, &p.instance) {
                (Some(base), Some(inst)) => Route::Cloud { url: cloud_url(base, inst), api_base: base.clone() },
                _ => Route::Unavailable("not signed in to AgentArea".into()),
            },
            Where::Local => match self.local(&e, &ctx) {
                Ok(l) => Route::Local(l),
                Err(err) => Route::Unavailable(err),
            },
        }
    }

    /// Stop the plugin's local server, if one runs (and isn't starting).
    fn stop_local(&self, id: &str) {
        let slot = self.locals.lock().ok().and_then(|m| m.get(id).cloned());
        let running = slot.and_then(|s| s.try_lock().ok().and_then(|mut r| r.take()));
        if let Some(l) = running {
            std::thread::spawn(move || local_mcp::stop_servers(std::slice::from_ref(&l.server), STOP_GRACE));
        }
    }

    fn slot(&self, id: &str) -> Slot {
        self.locals.lock().map(|mut m| m.entry(id.to_string()).or_default().clone()).unwrap_or_default()
    }

    /// The plugin's running local server, started (or restarted, when its
    /// command or data folder changed, or it died) as needed.
    fn local(&self, e: &Entry, ctx: &Context) -> Result<Arc<LocalProc>, String> {
        let decl = e.decl.local.as_ref().ok_or("the plugin declares no local server")?;
        let data_dir = data_dir(&self.root, ctx, &e.id);
        let fp = fingerprint(decl);
        let slot = self.slot(&e.id);
        let mut running = slot.lock().map_err(|_| "poisoned")?;
        if let Some(l) = running.as_ref() {
            if l.server.is_alive() && l.data_dir == data_dir && l.fingerprint == fp {
                return Ok(l.clone());
            }
        }
        if let Some(old) = running.take() {
            local_mcp::stop_servers(std::slice::from_ref(&old.server), STOP_GRACE);
        }
        std::fs::create_dir_all(&data_dir).map_err(|err| format!("data folder: {err}"))?;
        let mut env = decl.env.clone();
        env.insert("AGENTAREA_PLUGIN_ID".into(), e.id.clone());
        env.insert("AGENTAREA_DATA_DIR".into(), data_dir.display().to_string());
        let (server, init) = local_mcp::start_stdio(&decl.command[0], &decl.command[1..], &env, &e.dir)
            .map_err(|err| format!("local server: {err}"))?;
        let proc = Arc::new(LocalProc { server: Arc::new(server), init, data_dir, fingerprint: fp });
        *running = Some(proc.clone());
        Ok(proc)
    }

    /// Stop local servers that no longer should run: plugin gone or disabled,
    /// placed elsewhere, its command changed, or the workspace (data folder)
    /// changed. They start again on the next request.
    pub fn reconcile(&self) {
        let ctx = self.context();
        let entries = (self.lookup)();
        let slots: Vec<(String, Slot)> =
            self.locals.lock().map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect()).unwrap_or_default();
        let mut stop = Vec::new();
        for (id, slot) in slots {
            // A server that is starting right now is checked by its next request.
            let Ok(mut running) = slot.try_lock() else { continue };
            let Some(l) = running.as_ref() else { continue };
            let keep = entries.iter().find(|e| e.id == id).is_some_and(|e| {
                self.placement_of(e, &ctx).resolved == Where::Local
                    && l.data_dir == data_dir(&self.root, &ctx, &id)
                    && e.decl.local.as_ref().is_some_and(|d| fingerprint(d) == l.fingerprint)
            });
            if !keep {
                if let Some(l) = running.take() {
                    stop.push(l.server.clone());
                }
            }
        }
        if !stop.is_empty() {
            std::thread::spawn(move || local_mcp::stop_servers(&stop, STOP_GRACE));
        }
    }

    /// App exit: every local server goes.
    pub fn stop_all(&self) {
        let slots: Vec<Slot> = self.locals.lock().map(|mut m| m.drain().map(|(_, v)| v).collect()).unwrap_or_default();
        let servers: Vec<Arc<Server>> =
            slots.iter().filter_map(|s| s.lock().ok().and_then(|mut s| s.take())).map(|l| l.server.clone()).collect();
        local_mcp::stop_servers(&servers, Duration::from_millis(500));
    }

    /// One MCP request from the plugin's own page, through the gateway (so it
    /// sees exactly what agents see). A client is made per plugin and
    /// placement, and initialized once.
    pub fn ui_request(&self, mcp: &SecretsMcp, id: &str, method: &str, params: Value) -> Result<Value, String> {
        let e = self.entry(id).ok_or_else(|| format!("plugin {id} has no backend"))?;
        let ctx = self.context();
        let p = self.placement_of(&e, &ctx);
        let key = match p.resolved {
            Where::Unavailable => return Err(format!("backend unavailable: {}", p.reason.unwrap_or_default())),
            Where::Cloud => format!("cloud:{}", p.instance.unwrap_or_default()),
            Where::Local => format!("local:{}", data_dir(&self.root, &ctx, id).display()),
        };
        for attempt in 0..2 {
            let client = self.ui_client(mcp, id, &key)?;
            if !client.initialized.load(Ordering::SeqCst) {
                client.http.request("initialize", local_mcp::initialize_params())?;
                client.http.notify("notifications/initialized", json!({}))?;
                client.initialized.store(true, Ordering::SeqCst);
            }
            match client.http.request(method, params.clone()) {
                // The upstream forgot the session (restart, placement moved): start over once.
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
            initialized: AtomicBool::new(false),
            key: key.to_string(),
            _token: token,
        });
        ui.insert(id.to_string(), client.clone());
        Ok(client)
    }
}

/// A gateway message for a local server; `None` for notifications and
/// client responses. The server was initialized when it started, so a
/// client's `initialize` gets that answer; everything else is forwarded (with
/// the server's own request ids: several clients share one process).
pub fn local_message(local: &LocalProc, msg: &Value) -> Option<Value> {
    let method = msg.get("method").and_then(Value::as_str)?;
    let id = msg.get("id")?.clone();
    let params = msg.get("params").cloned().unwrap_or_else(|| json!({}));
    let result = match method {
        "initialize" => Ok(local.init.clone()),
        "ping" => Ok(json!({})),
        _ => local.server.request(method, params, local_mcp::REQUEST_TIMEOUT),
    };
    Some(match result {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Err(message) => json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32603, "message": message } }),
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

/// `plugin.tools()`: the backend's tools/list (the plugin's own backend only).
#[tauri::command]
pub async fn plugin_backend_tools(app: tauri::AppHandle, id: String) -> Result<Value, String> {
    ui_call(app, id, "tools/list", json!({})).await
}

/// `plugin.call(name, args)`: tools/call on the plugin's own backend.
#[tauri::command]
pub async fn plugin_backend_call(app: tauri::AppHandle, id: String, name: String, arguments: Option<Value>) -> Result<Value, String> {
    ui_call(app, id, "tools/call", json!({ "name": name, "arguments": arguments.unwrap_or_else(|| json!({})) })).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::{TcpListener, TcpStream};

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
        assert!(ok.validate().is_ok());
        assert_eq!(ok.local.as_ref().unwrap().env["A_1"], "x");
        assert!(!serde_json::to_string(&ok).unwrap().contains("A_1"), "env stays out of the UI listing");
        for bad in [
            r#"{}"#,
            r#"{"local":{"command":[]}}"#,
            r#"{"local":{"command":[" "]}}"#,
            r#"{"local":{"command":["x"],"env":{"A=B":"1"}}}"#,
            r#"{"local":{"command":["x\u0000"]}}"#,
            r#"{"cloud":{"instance":"  "}}"#,
        ] {
            let d: ServerDecl = serde_json::from_str(bad).unwrap();
            assert!(d.validate().is_err(), "{bad}");
        }
        assert!(serde_json::from_str::<ServerDecl>(r#"{"local":{"command":"node s.js"}}"#).is_err(), "command is an array");
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
        }
        let p = resolve(&decl(false, true), Mode::Auto, true, &ctx(Some(&absent)));
        assert_eq!(p.reason.as_deref(), Some("no MCP instance named \"plugin:demo\" in acme"));
        let p = resolve(&decl(true, false), Mode::Cloud, true, &ctx(Some(&present)));
        assert!(p.reason.unwrap().contains("no cloud instance"));
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
        // A local server needs the user's yes.
        let p = resolve(&decl(true, false), Mode::Auto, false, &out);
        assert_eq!(p.resolved, Where::Unavailable);
        assert!(p.reason.unwrap().contains("isn't allowed yet"));
        let p = resolve(&decl(true, true), Mode::Auto, false, &ctx(Some(&[])));
        assert!(p.reason.as_deref().is_some_and(|r| r.contains("no MCP instance") && r.contains("isn't allowed")), "{p:?}");
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

    // ── through the gateway ──────────────────────────────────────────────────

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("aa-backends-{name}-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn demo_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../examples/plugins/backend-demo")
    }

    /// The example plugin's manifest, as the app would read it.
    fn demo_decl() -> ServerDecl {
        let manifest: Value = serde_json::from_str(&std::fs::read_to_string(demo_dir().join("plugin.json")).unwrap()).unwrap();
        let decl: ServerDecl = serde_json::from_value(manifest["server"].clone()).unwrap();
        decl.validate().unwrap();
        decl
    }

    /// JSON-RPC over raw HTTP; (status, headers, body).
    fn rpc(url: &str, token: &str, session: Option<&str>, body: &Value) -> (u16, String, String) {
        let (addr, path) = url.trim_start_matches("http://").split_once('/').unwrap();
        let mut s = TcpStream::connect(addr).unwrap();
        let body = body.to_string();
        let session = session.map(|v| format!("Mcp-Session-Id: {v}\r\n")).unwrap_or_default();
        write!(s, "POST /{path} HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {token}\r\n{session}Accept: application/json, text/event-stream\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).unwrap();
        let (head, body) = out.split_once("\r\n\r\n").unwrap();
        let status = head.split_whitespace().nth(1).unwrap().parse().unwrap();
        let body = if head.to_ascii_lowercase().contains("transfer-encoding: chunked") { dechunk(body) } else { body.to_string() };
        (status, head.to_string(), body)
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

    /// The demo plugin's own stdio server behind `/plugins/backend-demo/mcp`,
    /// started on demand: echo, a counter persisted in the workspace data
    /// folder, the plugin page's path (ui_request), and stop on placement change.
    #[test]
    fn local_backend_through_the_gateway() {
        if !crate::cli_bin("node").exists() {
            println!("node not found; skipped");
            return;
        }
        let root = temp("local");
        let mode = Arc::new(Mutex::new(Mode::Local));
        let approved = Arc::new(AtomicBool::new(true));
        let (m, a) = (mode.clone(), approved.clone());
        let backends = Arc::new(Backends::new(
            root.clone(),
            Box::new(move || {
                let (mode, approved) = (*m.lock().unwrap(), a.load(Ordering::SeqCst));
                vec![Entry { id: "backend-demo".into(), dir: demo_dir(), decl: demo_decl(), mode, approved }]
            }),
        ));
        backends.set_context(Context { env: "ru".into(), slug: Some("acme".into()), ..Context::default() });
        let mcp = SecretsMcp::start_with(crate::vault::Vault::at(root.join("vault"), "AgentArea Desktop Secrets (test)"), backends.clone()).unwrap();
        let url = mcp.plugin_url("backend-demo");
        assert!(url.ends_with("/plugins/backend-demo/mcp"), "{url}");

        // Auth: no token, someone else's token, a token for another plugin.
        let init = json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{}}});
        assert_eq!(rpc(&url, "nope", None, &init).0, 401);
        let other = mcp.issue_plugin_token("other-plugin");
        assert_eq!(rpc(&url, &other.token, None, &init).0, 401, "a token opens only its own plugin");
        let token = mcp.issue_plugin_token("backend-demo");

        let (status, _, body) = rpc(&url, &token.token, None, &init);
        assert_eq!(status, 200, "{body}");
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["id"], 1);
        assert_eq!(v["result"]["serverInfo"]["name"], "backend-demo");
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

        // The plugin page's path: the same endpoint, via the bridge's client.
        let tools = backends.ui_request(&mcp, "backend-demo", "tools/list", json!({})).unwrap();
        assert_eq!(tools["tools"].as_array().unwrap().len(), 2);
        let r = backends.ui_request(&mcp, "backend-demo", "tools/call", json!({"name":"counter_increment","arguments":{}})).unwrap();
        assert_eq!(r["content"][0]["text"], "4");
        let r = backends.ui_request(&mcp, "backend-demo", "tools/call", json!({"name":"echo","arguments":{"text":"x"}})).unwrap();
        assert_eq!(r["content"][0]["text"], "x");

        // Another workspace: a fresh process with its own data folder.
        let first = backends.slot("backend-demo").lock().unwrap().as_ref().unwrap().server.clone();
        backends.set_context(Context { env: "ru".into(), slug: Some("other".into()), ..Context::default() });
        let (_, _, body) = rpc(&url, &token.token, None, &call(6, "counter_increment", json!({})));
        assert_eq!(text(&body), "1", "the other workspace has its own counter");
        std::thread::sleep(Duration::from_millis(1500));
        assert!(!first.is_alive(), "the first workspace's process was stopped");

        // Approval revoked (or plugin.json edited): refused, and the process goes.
        approved.store(false, Ordering::SeqCst);
        let (status, _, body) = rpc(&url, &token.token, None, &call(7, "echo", json!({"text":"x"})));
        assert_eq!(status, 503, "{body}");
        assert!(body.contains("isn't allowed yet"), "{body}");
        assert!(backends.slot("backend-demo").lock().unwrap().is_none(), "stopped without waiting for a reconcile");
        approved.store(true, Ordering::SeqCst);
        let (_, _, body) = rpc(&url, &token.token, None, &call(8, "counter_increment", json!({})));
        assert_eq!(text(&body), "2", "allowed again: restarted on demand, same workspace data");

        // Placed in the cloud with nothing there: unavailable, and the local process goes.
        *mode.lock().unwrap() = Mode::Cloud;
        backends.reconcile();
        let (status, _, body) = rpc(&url, &token.token, None, &call(7, "echo", json!({"text":"x"})));
        assert_eq!(status, 503, "{body}");
        assert!(body.contains("not signed in"), "{body}");
        assert!(backends.slot("backend-demo").lock().unwrap().is_none(), "local server stopped");
        assert!(backends.ui_request(&mcp, "backend-demo", "tools/list", json!({})).unwrap_err().contains("unavailable"));

        backends.stop_all();
        let _ = std::fs::remove_dir_all(root);
    }

    /// What the fake platform proxy saw.
    struct Seen {
        line: String,
        auth: String,
        session: String,
    }

    /// A fake `POST /v1/mcp/{id}/mcp`: answers `n` requests with JSON-RPC
    /// results echoing the method, and an Mcp-Session-Id.
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
                    (200, json!({"jsonrpc":"2.0","id":msg["id"],"result":{"upstream":msg["method"]}}).to_string())
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
    /// and refuses when signed out.
    #[test]
    fn cloud_backend_forwards_to_the_platform_proxy() {
        let _live = crate::data_mcp::LIVE_TOKEN_TEST.lock().unwrap_or_else(|e| e.into_inner());
        let root = temp("cloud");
        let (api, seen) = fake_proxy(3);
        let backends = Arc::new(Backends::new(
            root.clone(),
            Box::new(|| vec![Entry { id: "demo".into(), dir: PathBuf::from("/nonexistent"), decl: decl(true, true), mode: Mode::Auto, approved: false }]),
        ));
        backends.set_context(Context {
            env: "ru".into(),
            slug: Some("acme".into()),
            api_base: Some(api.clone()),
            instances: Some(vec![Instance { id: INSTANCE.into(), name: "plugin:demo".into() }]),
            error: None,
        });
        assert_eq!(backends.available(), ["demo"]);
        let mcp = SecretsMcp::start_with(crate::vault::Vault::at(root.join("vault"), "AgentArea Desktop Secrets (test)"), backends.clone()).unwrap();
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

        // Renewed token is used at once.
        crate::data_mcp::set_cloud_token(api.clone(), Some("live-2".into()));
        rpc(&url, &token.token, None, &json!({"jsonrpc":"2.0","id":10,"method":"ping"}));
        assert_eq!(seen.recv_timeout(Duration::from_secs(5)).unwrap().auth, "Bearer live-2");

        // Signed out: refused here, nothing reaches the platform.
        crate::data_mcp::set_cloud_token(api.clone(), None);
        let (status, _, body) = rpc(&url, &token.token, None, &json!({"jsonrpc":"2.0","id":11,"method":"ping"}));
        assert_eq!(status, 401, "{body}");
        assert!(seen.recv_timeout(Duration::from_millis(300)).is_err());

        // Instance removed: auto would fall back to local, which isn't approved.
        backends.set_context(Context { env: "ru".into(), slug: Some("acme".into()), api_base: Some(api), instances: Some(vec![]), error: None });
        assert!(backends.available().is_empty());
        let (status, _, body) = rpc(&url, &token.token, None, &json!({"jsonrpc":"2.0","id":12,"method":"ping"}));
        assert_eq!(status, 503);
        assert!(body.contains("isn't allowed yet"), "{body}");
        let _ = std::fs::remove_dir_all(root);
    }
}
