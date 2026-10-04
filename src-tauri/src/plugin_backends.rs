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
//! Nothing is used before the user approves the plugin as reviewed
//! (plugin_trust.rs): a local server runs only from the approved snapshot,
//! and a cloud instance is used only if that very instance was approved.
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
            check_command(&l.command)?;
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

/// Programs that fetch and run registry code at whatever version is current:
/// what would run isn't what was reviewed. Vendor the dependency instead.
const DOWNLOADERS: [&str; 6] = ["npx", "pnpx", "bunx", "uvx", "pipx", "yarn"];

/// A path that leaves the plugin folder: absolute, home-relative, or with `..`.
fn leaves_folder(arg: &str) -> bool {
    arg.split(['=', ':', ',']).any(|part| {
        let part = part.trim_matches(|c| c == '"' || c == '\'');
        part.starts_with('/') || part.starts_with('~') || part.split('/').any(|seg| seg == "..")
    })
}

/// A module specifier fetched from a registry or the network when imported.
fn remote_specifier(arg: &str) -> bool {
    let a = arg.trim_matches(|c| c == '"' || c == '\'').to_ascii_lowercase();
    ["npm:", "jsr:", "http:", "https:"].iter().any(|p| a.starts_with(p) || a.contains(&format!("={p}")))
        || a.contains("network-imports")
}

/// The command runs only what the approval covers (manifest-level checks):
/// - no downloaders (`npx`, `uvx`, …, `npm exec`, `pnpm dlx`, `bun x`, `uv tool`);
/// - no `npm:` / `jsr:` / `http(s):` specifiers, no network imports;
/// - `uv run` without `--with*` / `--script` / `--isolated`;
/// - `bun` only with `--no-install`, `deno` only with `--no-remote --no-npm`;
/// - arguments, and a relative program path, stay inside the plugin folder
///   (an absolute program, the interpreter, may be anywhere; the review shows
///   where it resolves).
///
/// `check_fetchers` adds the checks that need the folder (a `uv.lock`).
fn check_command(command: &[String]) -> Result<(), String> {
    let program = command[0].rsplit('/').next().unwrap_or("");
    let args = &command[1..];
    let has = |f: &str| args.iter().any(|a| a == f || a.starts_with(&format!("{f}=")));
    let dlx = |w: &str| command.get(1).is_some_and(|a| a == w);
    let fetches = |why: &str| Err(format!("server.local.command {why}, which fetches code at run time; vendor the dependency into the plugin folder"));
    if DOWNLOADERS.contains(&program)
        || (program == "npm" && dlx("exec"))
        || (program == "pnpm" && dlx("dlx"))
        || (program == "bun" && dlx("x"))
        || (program == "uv" && dlx("tool"))
    {
        return fetches(&format!("uses {program}"));
    }
    if let Some(a) = args.iter().find(|a| remote_specifier(a)) {
        return fetches(&format!("names \"{a}\""));
    }
    if program == "uv" {
        if let Some(f) = ["--with", "--with-requirements", "--with-editable", "--script", "--isolated"].into_iter().find(|f| has(f)) {
            return fetches(&format!("runs uv with {f}"));
        }
    }
    if program == "bun" && !has("--no-install") {
        return fetches("runs bun without --no-install");
    }
    if program == "deno" && !(has("--no-remote") && has("--no-npm")) {
        return fetches("runs deno without --no-remote --no-npm");
    }
    if !command[0].starts_with('/') && command[0].contains('/') && leaves_folder(&command[0]) {
        return Err(format!("server.local.command program \"{}\" points outside the plugin folder", command[0]));
    }
    if let Some(a) = args.iter().find(|a| leaves_folder(a)) {
        return Err(format!("server.local.command argument \"{a}\" points outside the plugin folder"));
    }
    Ok(())
}

/// Checks that need the plugin folder (live, or its snapshot): `uv run`
/// installs only from a committed `uv.lock` (hash-pinned, `UV_FROZEN`), and a
/// script with inline dependencies (`# /// script`) needs its own lock.
pub fn check_fetchers(command: &[String], dir: &Path) -> Result<(), String> {
    if command[0].rsplit('/').next() != Some("uv") || command.get(1).map(String::as_str) != Some("run") {
        return Ok(());
    }
    if !dir.join("uv.lock").is_file() {
        return Err("`uv run` needs a uv.lock in the plugin folder (otherwise it resolves dependencies at run time)".into());
    }
    for a in command[2..].iter().filter(|a| a.ends_with(".py")) {
        let inline = std::fs::read_to_string(dir.join(a)).is_ok_and(|src| src.lines().any(|l| l.trim() == "# /// script"));
        if inline && !dir.join(format!("{a}.lock")).is_file() {
            return Err(format!("{a} declares inline dependencies; commit {a}.lock (uv lock --script {a})"));
        }
    }
    Ok(())
}

/// Env variables that change what an interpreter loads or where a package
/// tool fetches from; flagged in the review.
pub fn risky_env(name: &str) -> bool {
    [
        "NODE_OPTIONS", "NODE_PATH", "PYTHONPATH", "PYTHONSTARTUP", "PYTHONHOME", "PYTHONUSERBASE", "PERL5LIB", "PERL5OPT",
        "RUBYOPT", "RUBYLIB", "JAVA_TOOL_OPTIONS", "BASH_ENV",
    ]
    .contains(&name)
        || ["LD_", "DYLD_", "UV_", "PIP_", "npm_config_", "NPM_CONFIG_", "BUN_", "DENO_"].iter().any(|p| name.starts_with(p))
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
    /// something for the user to review, e.g. a cloud instance that appeared
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notice: Option<String>,
}

/// What the user approved for a plugin (plugins.rs checks the signature and
/// epoch): the snapshot that runs, and the cloud instances it may use.
#[derive(Clone, Debug, PartialEq)]
pub struct Grant {
    pub fingerprint: String,
    /// `<app data>/plugin-snapshots/<id>/<fingerprint>`
    pub snapshot: PathBuf,
    /// `binding()` keys: `<env>/<slug>/<instance id>`
    pub instances: Vec<String>,
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

/// The approval key of a cloud instance: which instance, in which workspace.
pub fn binding(ctx: &Context, instance_id: &str) -> String {
    format!("{}/{}/{instance_id}", ctx.env, ctx.slug.as_deref().unwrap_or("-"))
}

fn unreviewed(c: &CloudDecl, ctx: &Context) -> String {
    format!("the workspace has an MCP instance \"{}\" in {} that you haven't reviewed: review it in Apps → Plugins", c.instance, ctx.slug.as_deref().unwrap_or("this workspace"))
}

/// Where `decl` runs under `mode` in `ctx`, given what the user approved.
///
/// - cloud is possible when the workspace has exactly one MCP instance whose
///   `name` equals `cloud.instance` (= `plugin:<id>`; exact, case-sensitive)
///   **and** that instance (id, in this workspace) was approved;
/// - local is possible when `local` is declared and the plugin is approved;
/// - `auto` prefers an approved cloud instance, then local. An instance that
///   appeared but wasn't reviewed never takes over: auto stays local and
///   says so in `notice`.
pub fn resolve(decl: &ServerDecl, mode: Mode, grant: Option<&Grant>, ctx: &Context) -> Placement {
    let cloud = decl.cloud.as_ref().map(|c| (c, cloud_instance(c, ctx)));
    let local = decl.local.is_some();
    let none = |reason: String| Placement { mode, resolved: Where::Unavailable, reason: Some(reason), instance: None, notice: None };
    let at = |resolved, instance, notice| Placement { mode, resolved, reason: None, instance, notice };
    let Some(grant) = grant else {
        let declared = match mode {
            Mode::Cloud => decl.cloud.is_some(),
            Mode::Local => local,
            Mode::Auto => true,
        };
        return none(if declared { NOT_ALLOWED.into() } else if mode == Mode::Cloud {
            "the plugin declares no cloud instance".into()
        } else {
            "the plugin declares no local server".into()
        });
    };
    // Some(Ok(id)) approved instance; Some(Err) why not; None no cloud declared.
    let found_any = cloud.as_ref().is_some_and(|(_, f)| f.is_ok());
    let cloud_ok: Option<Result<String, String>> = cloud.map(|(c, found)| {
        let id = found?;
        if grant.instances.contains(&binding(ctx, &id)) {
            Ok(id)
        } else {
            Err(unreviewed(c, ctx))
        }
    });
    let found_unreviewed = found_any && cloud_ok.as_ref().is_some_and(|r| r.is_err());
    match mode {
        Mode::Cloud => match cloud_ok {
            Some(Ok(id)) => at(Where::Cloud, Some(id), None),
            Some(Err(e)) => none(e),
            None => none("the plugin declares no cloud instance".into()),
        },
        Mode::Local if local => at(Where::Local, None, None),
        Mode::Local => none("the plugin declares no local server".into()),
        Mode::Auto => match (cloud_ok, local) {
            (Some(Ok(id)), _) => at(Where::Cloud, Some(id), None),
            (_, true) => at(Where::Local, None, found_unreviewed.then(|| "cloud instance available: review it to run there".to_string())),
            (Some(Err(e)), false) => none(e),
            (None, false) => none("the plugin declares no server".into()),
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
    /// the approved snapshot's declaration when approved, else the live one
    pub decl: ServerDecl,
    pub mode: Mode,
    /// the user's approval (signature and epoch checked), if any
    pub grant: Option<Grant>,
}

/// Where a request goes.
#[derive(Clone, Debug, PartialEq)]
pub enum Target {
    Local { data_dir: PathBuf },
    /// `binding`: the approved instance (see `binding()`)
    Cloud { url: String, api_base: String, binding: String },
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
    /// the snapshot fingerprint it was started from
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
        (self.lookup)().into_iter().map(|e| (e.id.clone(), resolve(&e.decl, e.mode, e.grant.as_ref(), &ctx))).collect()
    }

    /// The workspace instance `decl`'s cloud part matches now, as (binding,
    /// instance id, workspace slug): what approving would bind.
    pub fn cloud_binding(&self, decl: &ServerDecl) -> Option<(String, String, String)> {
        let ctx = self.context();
        let inst = cloud_instance(decl.cloud.as_ref()?, &ctx).ok()?;
        Some((binding(&ctx, &inst), inst, ctx.slug.clone().unwrap_or_default()))
    }

    /// Where `e` goes in `ctx`, or why not.
    fn target_in(&self, e: &Entry, ctx: &Context) -> Result<Target, String> {
        let p = resolve(&e.decl, e.mode, e.grant.as_ref(), ctx);
        match p.resolved {
            Where::Unavailable => Err(p.reason.unwrap_or_default()),
            Where::Local => Ok(Target::Local { data_dir: data_dir(&self.root, ctx, &e.id) }),
            Where::Cloud => match (&ctx.api_base, &p.instance) {
                (Some(base), Some(inst)) => {
                    Ok(Target::Cloud { url: cloud_url(base, inst), api_base: base.clone(), binding: binding(ctx, inst) })
                }
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
        let Some(grant) = &e.grant else {
            // revoked (or never approved)
            self.stop_plugin(id);
            return Err(NOT_ALLOWED.into());
        };
        match pin {
            Some(t @ Target::Local { .. }) if e.decl.local.is_some() => Ok(t.clone()),
            Some(t @ Target::Cloud { binding, .. }) if e.decl.cloud.is_some() && grant.instances.contains(binding) => Ok(t.clone()),
            Some(_) => Err("the backend this thread started with is no longer approved".into()),
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
    /// died or a newer version was approved) as needed. It runs from the
    /// approved snapshot, which is re-hashed before every start.
    pub fn start(&self, id: &str, data_dir: &Path) -> Result<Arc<LocalProc>, String> {
        let e = self.entry(id).ok_or_else(|| format!("no enabled plugin {id} with a server"))?;
        let decl = e.decl.local.as_ref().ok_or("the plugin declares no local server")?;
        let grant = e.grant.clone().ok_or(NOT_ALLOWED)?;
        let approval = grant.fingerprint.clone();
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
            plugin_trust::verify(id, &grant.snapshot, &approval)?;
            check_fetchers(&decl.command, &grant.snapshot)?;
            std::fs::create_dir_all(data_dir).map_err(|err| format!("data folder: {err}"))?;
            let env = child_env(&decl.env, &decl.command, id, data_dir, &grant.snapshot);
            local_mcp::start_stdio(&decl.command[0], &decl.command[1..], &env, &grant.snapshot)
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
        // Revoked (or superseded) while it started: don't keep it.
        if self.entry(id).and_then(|e| e.grant).map(|g| g.fingerprint).as_deref() != Some(approval.as_str()) {
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
                    e.grant.as_ref().is_some_and(|g| g.fingerprint == l.approval)
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

/// The child's environment: the approved `env`, then what the app forces.
/// - Package tools are offline (`UV_OFFLINE`, `PIP_NO_INDEX`,
///   `npm_config_offline`, `BUN_CONFIG_NO_INSTALL`… ), except `uv run`,
///   which may install exactly what the reviewed `uv.lock` pins
///   (`UV_FROZEN=1`, hashes checked) into an app-owned venv next to the
///   snapshot.
/// - No bytecode writes, no user site packages, no `NODE_PATH`; other caches
///   go to the data folder (the snapshot is read-only).
/// - `AGENTAREA_PLUGIN_ID` and `AGENTAREA_DATA_DIR`.
pub fn child_env(env: &BTreeMap<String, String>, command: &[String], id: &str, data_dir: &Path, snapshot: &Path) -> BTreeMap<String, String> {
    let cache = data_dir.join(".cache");
    let uv_run = command.first().and_then(|c| c.rsplit('/').next()) == Some("uv");
    let mut out = env.clone();
    let mut set = |k: &str, v: String| {
        out.insert(k.to_string(), v);
    };
    set("XDG_CACHE_HOME", cache.display().to_string());
    set("UV_CACHE_DIR", cache.join("uv").display().to_string());
    set("npm_config_cache", cache.join("npm").display().to_string());
    set("UV_PROJECT_ENVIRONMENT", snapshot.with_extension("venv").display().to_string());
    set("UV_FROZEN", "1".into());
    set("UV_NO_PROGRESS", "1".into());
    if !uv_run {
        set("UV_OFFLINE", "1".into());
    }
    set("PIP_NO_INDEX", "1".into());
    set("npm_config_offline", "true".into());
    set("BUN_CONFIG_NO_INSTALL", "1".into());
    set("DENO_NO_REMOTE", "1".into());
    set("PYTHONDONTWRITEBYTECODE", "1".into());
    set("PYTHONNOUSERSITE", "1".into());
    set("NODE_PATH", String::new());
    set("AGENTAREA_PLUGIN_ID", id.to_string());
    set("AGENTAREA_DATA_DIR", data_dir.display().to_string());
    out
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

    /// Approved, with the acme instance.
    fn granted() -> Grant {
        Grant { fingerprint: "f".into(), snapshot: PathBuf::from("/snap"), instances: vec![format!("ru/acme/{INSTANCE}")] }
    }

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

    /// NEW-3: no downloaders; arguments stay in the plugin folder.
    #[test]
    fn commands_run_only_what_was_reviewed() {
        let ok = |c: &[&str]| check_command(&c.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert!(ok(&["node", "server.mjs"]).is_ok());
        assert!(ok(&["/opt/homebrew/bin/node", "dist/server.js", "--port=0"]).is_ok(), "the interpreter may be absolute");
        assert!(ok(&["python3", "-m", "server"]).is_ok());
        assert!(ok(&["./bin/server"]).is_ok());
        for bad in [
            &["npx", "-y", "some-mcp"][..],
            &["uvx", "mcp-server"],
            &["/usr/local/bin/npx", "x"],
            &["npm", "exec", "x"],
            &["pnpm", "dlx", "x"],
            &["bunx", "x"],
            &["pipx", "run", "x"],
        ] {
            assert!(ok(bad).unwrap_err().contains("fetches code at run time"), "{bad:?}");
        }
        for bad in [
            &["../../x/bin/server"][..],
            &["bin/../../server"],
            &["node", "../shared/server.mjs"],
            &["node", "/Users/me/proj/server.js"],
            &["node", "--require=/tmp/x.js", "s.js"],
            &["node", "--import", "~/x.mjs"],
            &["python3", "lib/../../x.py"],
        ] {
            assert!(ok(bad).unwrap_err().contains("outside the plugin folder"), "{bad:?}");
        }
        assert!(risky_env("NODE_OPTIONS") && risky_env("DYLD_INSERT_LIBRARIES") && risky_env("LD_PRELOAD") && risky_env("PYTHONPATH"));
        for k in ["PYTHONUSERBASE", "PERL5OPT", "RUBYLIB", "JAVA_TOOL_OPTIONS", "npm_config_registry", "UV_INDEX_URL", "PIP_INDEX_URL", "BUN_CONFIG_REGISTRY", "DENO_DIR"] {
            assert!(risky_env(k), "{k}");
        }
        assert!(!risky_env("LOG_LEVEL"));
    }

    #[test]
    fn child_env_is_offline_and_keeps_caches_out_of_the_snapshot() {
        let node = ["node".to_string(), "s.mjs".to_string()];
        let plugin_env = BTreeMap::from([
            ("LOG".to_string(), "1".to_string()),
            ("AGENTAREA_PLUGIN_ID".to_string(), "spoof".to_string()),
            ("UV_OFFLINE".to_string(), "0".to_string()),
            ("npm_config_offline".to_string(), "false".to_string()),
        ]);
        let env = child_env(&plugin_env, &node, "p", Path::new("/d"), Path::new("/s/p/abc"));
        assert_eq!(env["AGENTAREA_PLUGIN_ID"], "p", "the app's variables win");
        assert_eq!(env["AGENTAREA_DATA_DIR"], "/d");
        assert_eq!(env["LOG"], "1");
        // R2-1: offline, and the plugin's env can't turn it back on.
        for (k, v) in [("UV_OFFLINE", "1"), ("UV_FROZEN", "1"), ("PIP_NO_INDEX", "1"), ("npm_config_offline", "true"), ("BUN_CONFIG_NO_INSTALL", "1"), ("DENO_NO_REMOTE", "1")] {
            assert_eq!(env[k], v, "{k}");
        }
        assert_eq!((env["PYTHONDONTWRITEBYTECODE"].as_str(), env["PYTHONNOUSERSITE"].as_str(), env["NODE_PATH"].as_str()), ("1", "1", ""));
        assert!(!env.contains_key("PYTHONPYCACHEPREFIX"));
        assert_eq!(env["UV_PROJECT_ENVIRONMENT"], "/s/p/abc.venv", "app-owned, per snapshot");
        assert_eq!(env["XDG_CACHE_HOME"], "/d/.cache");
        // uv run may install exactly the reviewed lock (frozen), so it isn't offline.
        let uv = ["uv".to_string(), "run".to_string(), "server.py".to_string()];
        let env = child_env(&BTreeMap::new(), &uv, "p", Path::new("/d"), Path::new("/s/p/abc"));
        assert!(!env.contains_key("UV_OFFLINE"));
        assert_eq!(env["UV_FROZEN"], "1");
    }

    /// R2-1: every runtime fetcher pattern is refused, with a reason.
    #[test]
    fn runtime_fetchers_are_refused() {
        let check = |c: &[&str]| check_command(&c.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        for bad in [
            &["uv", "run", "--with", "requests", "server.py"][..],
            &["uv", "run", "--with=requests", "server.py"],
            &["uv", "run", "--script", "server.py"],
            &["uv", "run", "--isolated", "server.py"],
            &["deno", "run", "npm:some-mcp"],
            &["deno", "run", "jsr:@scope/mcp"],
            &["deno", "run", "https://example.com/server.ts"],
            &["deno", "run", "server.ts"],
            &["deno", "run", "--no-remote", "server.ts"],
            &["bun", "server.ts"],
            &["bun", "run", "server.ts"],
            &["node", "--experimental-network-imports", "s.mjs"],
            &["node", "--import=https://x.example/h.mjs", "s.mjs"],
            &["python3", "-m", "http:x"],
        ] {
            let err = check(bad).unwrap_err();
            assert!(err.contains("fetches code at run time"), "{bad:?}: {err}");
        }
        assert!(check(&["bun", "--no-install", "server.ts"]).is_ok());
        assert!(check(&["deno", "run", "--no-remote", "--no-npm", "server.ts"]).is_ok());
        assert!(check(&["uv", "run", "server.py"]).is_ok(), "manifest-wise; the lock is checked with the folder");

        // uv run needs a committed lock; an inline-dependency script needs its own.
        let dir = std::env::temp_dir().join(format!("aa-fetchers-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let uv: Vec<String> = ["uv", "run", "server.py"].iter().map(|s| s.to_string()).collect();
        std::fs::write(dir.join("server.py"), "print(1)").unwrap();
        assert!(check_fetchers(&uv, &dir).unwrap_err().contains("needs a uv.lock"));
        std::fs::write(dir.join("uv.lock"), "version = 1").unwrap();
        assert!(check_fetchers(&uv, &dir).is_ok());
        std::fs::write(dir.join("server.py"), "# /// script\n# dependencies = [\"httpx\"]\n# ///\nprint(1)").unwrap();
        assert!(check_fetchers(&uv, &dir).unwrap_err().contains("inline dependencies"));
        std::fs::write(dir.join("server.py.lock"), "version = 1").unwrap();
        assert!(check_fetchers(&uv, &dir).is_ok());
        assert!(check_fetchers(&["node".to_string(), "s.mjs".to_string()], &dir).is_ok());
        let _ = std::fs::remove_dir_all(dir);
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
        let g = granted();
        for (mode, local, cloud, has, want) in cases {
            let c = ctx(Some(if has { &present[..] } else { &absent[..] }));
            let p = resolve(&decl(local, cloud), mode, Some(&g), &c);
            assert_eq!(p.resolved, want, "{mode:?} local={local} cloud={cloud} instance={has}: {p:?}");
            assert_eq!(p.mode, mode);
            assert_eq!(p.reason.is_some(), want == Unavailable, "{p:?}");
            assert_eq!(p.instance.as_deref(), (want == Cloud).then_some(INSTANCE));
            // H1: nothing, cloud included, without the user's approval.
            let p = resolve(&decl(local, cloud), mode, None, &c);
            assert_eq!(p.resolved, Unavailable, "unapproved {mode:?} local={local} cloud={cloud} instance={has}");
        }
        let p = resolve(&decl(false, true), Mode::Auto, Some(&g), &ctx(Some(&absent)));
        assert_eq!(p.reason.as_deref(), Some("no MCP instance named \"plugin:demo\" in acme"));
        let p = resolve(&decl(true, false), Mode::Cloud, Some(&g), &ctx(Some(&present)));
        assert!(p.reason.unwrap().contains("no cloud instance"));
        let p = resolve(&decl(false, true), Mode::Cloud, None, &ctx(Some(&present)));
        assert_eq!(p.reason.as_deref(), Some(NOT_ALLOWED), "cloud needs the approval too");
    }

    /// NEW-5: a cloud approval names one instance in one workspace.
    #[test]
    fn cloud_approval_is_bound_to_the_instance() {
        let local_only = Grant { instances: vec![], ..granted() };
        let other_id = "6f1c2a3b-0000-4000-8000-0000000000ff";
        // An instance appears that wasn't reviewed: auto stays local and says so.
        let p = resolve(&decl(true, true), Mode::Auto, Some(&local_only), &ctx(Some(&[(INSTANCE, "plugin:demo")])));
        assert_eq!(p.resolved, Where::Local);
        assert_eq!(p.notice.as_deref(), Some("cloud instance available: review it to run there"));
        let p = resolve(&decl(true, true), Mode::Cloud, Some(&local_only), &ctx(Some(&[(INSTANCE, "plugin:demo")])));
        assert_eq!(p.resolved, Where::Unavailable);
        assert!(p.reason.unwrap().contains("haven't reviewed"));
        // Same name, different instance: not the one approved.
        let p = resolve(&decl(false, true), Mode::Auto, Some(&granted()), &ctx(Some(&[(other_id, "plugin:demo")])));
        assert_eq!(p.resolved, Where::Unavailable);
        // Same instance id, other workspace: not approved there.
        let elsewhere = Context { slug: Some("beta".into()), ..ctx(Some(&[(INSTANCE, "plugin:demo")])) };
        assert_eq!(resolve(&decl(false, true), Mode::Cloud, Some(&granted()), &elsewhere).resolved, Where::Unavailable);
        // The approved one.
        let p = resolve(&decl(true, true), Mode::Auto, Some(&granted()), &ctx(Some(&[(INSTANCE, "plugin:demo")])));
        assert_eq!((p.resolved, p.notice), (Where::Cloud, None));
        assert_eq!(binding(&ctx(None), INSTANCE), format!("ru/acme/{INSTANCE}"));
    }

    #[test]
    fn placement_edge_cases() {
        // Signed out: cloud is out, auto falls back to local.
        let out = Context { env: "ru".into(), ..Context::default() };
        let g = granted();
        assert_eq!(resolve(&decl(true, true), Mode::Auto, Some(&g), &out).resolved, Where::Local);
        let p = resolve(&decl(false, true), Mode::Auto, Some(&g), &out);
        assert_eq!(p.reason.as_deref(), Some("not signed in to AgentArea"));
        let failed = Context { error: Some("couldn't list MCP instances (500)".into()), ..out.clone() };
        assert_eq!(resolve(&decl(false, true), Mode::Cloud, Some(&g), &failed).reason.as_deref(), Some("couldn't list MCP instances (500)"));
        // Exact names only; duplicates and odd ids are refused.
        assert_eq!(resolve(&decl(false, true), Mode::Cloud, Some(&g), &ctx(Some(&[(INSTANCE, "Plugin:Demo")]))).resolved, Where::Unavailable);
        let dup = resolve(&decl(false, true), Mode::Cloud, Some(&g), &ctx(Some(&[(INSTANCE, "plugin:demo"), ("bbbb", "plugin:demo")])));
        assert!(dup.reason.unwrap().starts_with("2 MCP instances"));
        let odd = resolve(&decl(false, true), Mode::Cloud, Some(&g), &ctx(Some(&[("../x", "plugin:demo")])));
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
        let grant = approve_copy(&root, &dir);
        let (m, a, snap) = (mode.clone(), approved.clone(), grant.snapshot.clone());
        let backends = Arc::new(Backends::new(
            root.clone(),
            Box::new(move || {
                vec![Entry {
                    id: "backend-demo".into(),
                    decl: demo_decl(&snap),
                    mode: *m.lock().unwrap(),
                    grant: a.load(std::sync::atomic::Ordering::SeqCst).then(|| grant.clone()),
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

        // NEW-1: the live folder can change at will; what runs is the
        // approved snapshot, before and after a restart.
        std::fs::write(dir.join("server.mjs"), "process.exit(3)").unwrap();
        let (_, _, body) = rpc(&url, &token.token, None, &call(10, "echo", json!({"text":"still the approved code"})));
        assert_eq!(text(&body), "still the approved code");
        backends.stop_all();
        let (_, _, body) = rpc(&url, &token.token, None, &call(11, "echo", json!({"text":"restarted from the snapshot"})));
        assert_eq!(text(&body), "restarted from the snapshot", "{body}");

        // Revoked: refused at once, processes stopped.
        approved.store(false, std::sync::atomic::Ordering::SeqCst);
        let (status, _, body) = rpc(&url, &token.token, None, &call(12, "echo", json!({"text":"x"})));
        assert_eq!(status, 503, "{body}");
        assert!(body.contains("not allowed yet"), "{body}");
        std::thread::sleep(Duration::from_millis(1500));
        assert!(backends.locals.lock().unwrap().values().all(|s| s.lock().unwrap().proc.is_none()), "stopped");

        // Placed in the cloud with nothing there: unavailable.
        *mode.lock().unwrap() = Mode::Cloud;
        backends.reconcile();
        assert!(backends.ui_request(&mcp, "backend-demo", "tools/list", json!({})).unwrap_err().contains("unavailable"));

        backends.stop_all();
        plugin_trust::remove_snapshot(&root);
    }

    /// Approve `dir` as plugins.rs does: snapshot it under `<root>/snaps`.
    fn approve_copy(root: &Path, dir: &Path) -> Grant {
        let (snapshot, tree) = plugin_trust::snapshot("backend-demo", dir, &root.join("snaps")).unwrap();
        Grant { fingerprint: tree.fingerprint, snapshot, instances: vec![] }
    }

    /// Every start re-hashes the snapshot: a tampered copy doesn't run.
    #[test]
    fn start_rechecks_the_snapshot() {
        let root = temp("recheck");
        let dir = demo_copy(&root);
        let grant = approve_copy(&root, &dir);
        // Someone with write access to the app's data folder edits the copy.
        let target = grant.snapshot.join("server.mjs");
        std::fs::set_permissions(&grant.snapshot, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        std::fs::set_permissions(&target, std::os::unix::fs::PermissionsExt::from_mode(0o644)).unwrap();
        std::fs::write(&target, "process.exit(1)").unwrap();
        let g = grant.clone();
        let backends = Backends::new(
            root.clone(),
            Box::new(move || vec![Entry { id: "backend-demo".into(), decl: demo_decl(&g.snapshot), mode: Mode::Local, grant: Some(g.clone()) }]),
        );
        let err = backends.start("backend-demo", &root.join("data")).err().unwrap();
        assert!(err.contains("approved copy was modified"), "{err}");
        assert!(!root.join("data").exists(), "nothing ran");
        // Backoff: the same error, without another attempt.
        assert_eq!(backends.start("backend-demo", &root.join("data")).err().unwrap(), err);
        plugin_trust::remove_snapshot(&root);
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
                let grant = a.load(std::sync::atomic::Ordering::SeqCst).then(granted);
                vec![Entry { id: "demo".into(), decl: decl(true, true), mode: Mode::Auto, grant }]
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
        plugin_trust::remove_snapshot(&root);
    }
}
