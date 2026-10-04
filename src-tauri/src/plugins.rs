//! Local plugins: folders under `~/AgentArea/plugins/<id>/` holding a
//! `plugin.json` and some static web files. Plain files on purpose — an agent
//! working in a thread can write a new plugin, and it shows up on next list.
//!
//! Files are served over the `aa-plugin` scheme and rendered by the frontend in
//! an opaque-origin sandboxed iframe; the only way out is the host's
//! postMessage bridge.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use base64::Engine;
use serde::{Deserialize, Serialize};
use tauri::http::{Request, Response, StatusCode};
use tauri::{Runtime, UriSchemeContext, UriSchemeResponder};

use crate::plugin_backends::{self, Grant, Mode, ServerDecl};
use crate::plugin_trust;
use crate::web;

pub const SCHEME: &str = "aa-plugin";

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Plugin {
    id: String,
    title: String,
    description: Option<String>,
    entry: String,
    /// Off = hidden from use and not served; the folder stays untouched.
    enabled: bool,
    /// Optional page shown as a section in the app's sidebar.
    sidebar: Option<String>,
    /// Extra bridge powers the plugin asks for: "fs" (a local folder) and
    /// "cloud" (the AgentArea workspace's files).
    permissions: Vec<String>,
    /// Folder granted to an "fs" plugin by the user; it can read only inside.
    root: Option<String>,
    /// Workspace-files prefix a "cloud" plugin reads under ("" = the whole
    /// library, else "a/b/"); `wiki/` until the user changes it.
    #[serde(rename = "cloudRoot")]
    cloud_root: Option<String>,
    /// Sidebar glyph: a lucide icon name ("radio-tower"), or the plugin's own
    /// monochrome SVG as a `data:` URL the app renders as a CSS mask.
    icon: Option<String>,
    /// MCP backend (plugin_backends.rs): where it can run.
    server: Option<ServerDecl>,
    /// Why plugin.json's `server` was not accepted (the plugin still loads,
    /// without a backend).
    #[serde(rename = "serverError")]
    server_error: Option<String>,
    /// Where the user wants the backend: auto (default), local or cloud.
    placement: Option<Mode>,
    /// The user approved a version of the backend (plugin_trust.rs); it runs
    /// from that version's snapshot.
    #[serde(rename = "serverApproved")]
    server_approved: bool,
    /// The live folder differs from the approved version: review again to
    /// switch (the approved one keeps running meanwhile).
    #[serde(rename = "updateAvailable")]
    update_available: bool,
    /// There is an approval entry that no longer counts (revoked elsewhere,
    /// forged, or its snapshot is gone): Revoke clears it.
    #[serde(rename = "staleApproval")]
    stale_approval: bool,
    /// The genuine approval, if any.
    #[serde(skip)]
    grant: Option<Grant>,
}

#[derive(Deserialize)]
struct Manifest {
    id: String,
    title: String,
    description: Option<String>,
    entry: Option<String>,
    sidebar: Option<String>,
    #[serde(default)]
    permissions: Vec<String>,
    icon: Option<String>,
    #[serde(default)]
    server: Option<serde_json::Value>,
}

/// Largest SVG icon a plugin may ship.
const ICON_MAX_BYTES: u64 = 16 * 1024;

/// A manifest `icon`: a lucide name (lowercase words joined by "-") or a
/// relative `.svg` path inside the plugin folder. Anything else is ignored.
fn icon_spec(icon: &str) -> Option<String> {
    let icon = icon.trim();
    if icon.to_ascii_lowercase().ends_with(".svg") {
        return safe_relative(icon).map(|_| icon.to_string());
    }
    let lucide = icon.len() <= 48
        && icon.starts_with(|c: char| c.is_ascii_lowercase())
        && icon.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    lucide.then(|| icon.to_string())
}

/// The plugin's SVG icon as a `data:` URL: a small file that stays inside the
/// plugin folder (symlinks included). Rendered only as an image mask, so any
/// script or external reference in it never runs or loads.
fn svg_icon(plugin_dir: &Path, rel: &str) -> Option<String> {
    let base = plugin_dir.canonicalize().ok()?;
    let file = base.join(safe_relative(rel)?).canonicalize().ok()?;
    let meta = std::fs::metadata(&file).ok()?;
    if !file.starts_with(&base) || !meta.is_file() || meta.len() > ICON_MAX_BYTES {
        return None;
    }
    let svg = std::fs::read_to_string(&file).ok()?;
    svg.contains("<svg")
        .then(|| format!("data:image/svg+xml;base64,{}", base64::engine::general_purpose::STANDARD.encode(svg)))
}

fn plugins_dir() -> Result<PathBuf, String> {
    let home = std::env::var("HOME").map_err(|_| "HOME not set".to_string())?;
    Ok(PathBuf::from(home).join("AgentArea").join("plugins"))
}

/// On/off state lives next to the plugins, not inside them: plugin folders are
/// agent-written content, and switching one off shouldn't rewrite its files.
fn state_file() -> Result<PathBuf, String> {
    Ok(plugins_dir()?.with_file_name("plugins-state.json"))
}

#[derive(Default, Deserialize, Serialize)]
struct State {
    #[serde(default)]
    disabled: Vec<String>,
    /// plugin id → folder the user granted it (fs permission)
    #[serde(default)]
    roots: HashMap<String, String>,
    /// plugin id → workspace-files prefix (cloud permission)
    #[serde(default, rename = "cloudRoots")]
    cloud_roots: HashMap<String, String>,
    /// plugin id → backend placement ("local" | "cloud"); absent or unknown =
    /// auto. Plain strings, so a mode this build doesn't know can't make the
    /// whole file unreadable.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    placements: HashMap<String, String>,
    /// plugin id → a signed `Approval`; anything else in there (hand edits,
    /// forgeries, approvals revoked since) doesn't count.
    #[serde(default, rename = "serverApprovals", skip_serializing_if = "HashMap::is_empty")]
    server_approvals: HashMap<String, serde_json::Value>,
}

/// One approval, signed with the Keychain key over every other field. It is
/// valid only while `epoch` is the plugin's current epoch in the Keychain:
/// any later approve or revoke makes it history.
#[derive(Clone, Debug, Deserialize, Serialize)]
struct Approval {
    plugin: String,
    /// the snapshot's tree fingerprint (workspace-independent)
    fingerprint: String,
    /// cloud instances approved, as `<env>/<slug>/<instance id>`
    #[serde(default)]
    instances: Vec<String>,
    /// unix ms
    issued_at: u64,
    nonce: String,
    epoch: u64,
    #[serde(default)]
    mac: String,
}

impl Approval {
    fn message(&self) -> Vec<u8> {
        serde_json::json!([self.plugin, self.fingerprint, self.instances, self.issued_at, self.nonce, self.epoch])
            .to_string()
            .into_bytes()
    }
}

pub const DEFAULT_CLOUD_ROOT: &str = "wiki/";

impl State {
    fn placement(&self, id: &str) -> Mode {
        self.placements
            .get(id)
            .and_then(|m| serde_json::from_value(serde_json::Value::String(m.clone())).ok())
            .unwrap_or_default()
    }

    /// `id`'s approval if it is genuine (MAC), current (epoch) and its
    /// snapshot exists under `snapshots`.
    fn grant(&self, id: &str, snapshots: &Path) -> Option<Grant> {
        let a: Approval = serde_json::from_value(self.server_approvals.get(id)?.clone()).ok()?;
        let snapshot = plugin_trust::snapshot_dir(snapshots, id, &a.fingerprint);
        let ok = a.plugin == id
            && plugin_trust::verify_mac(&a.message(), &a.mac)
            && plugin_trust::epoch(id).ok() == Some(a.epoch)
            && snapshot.is_dir();
        ok.then_some(Grant { fingerprint: a.fingerprint, snapshot, instances: a.instances })
    }
}

/// plugins-state.json is read-modify-written by several commands.
static STATE_LOCK: Mutex<()> = Mutex::new(());

fn load_state() -> State {
    state_file().map(|f| load_state_at(&f)).unwrap_or_default()
}

fn load_state_at(file: &Path) -> State {
    std::fs::read_to_string(file).ok().and_then(|json| serde_json::from_str::<State>(&json).ok()).unwrap_or_default()
}

/// Change plugins-state.json under the lock.
fn edit_state<T>(f: impl FnOnce(&mut State) -> Result<T, String>) -> Result<T, String> {
    edit_state_at(&state_file()?, f)
}

fn edit_state_at<T>(file: &Path, f: impl FnOnce(&mut State) -> Result<T, String>) -> Result<T, String> {
    let _guard = STATE_LOCK.lock().map_err(|_| "plugin state lock poisoned")?;
    let mut state = load_state_at(file);
    let out = f(&mut state)?;
    state.disabled.sort();
    state.disabled.dedup();
    let json = serde_json::to_string_pretty(&state).map_err(|e| e.to_string())?;
    std::fs::write(file, json).map_err(|e| format!("save plugin state: {e}"))?;
    Ok(out)
}

fn disabled() -> HashSet<String> {
    load_state().disabled.into_iter().collect()
}

#[tauri::command]
pub fn set_plugin_enabled(id: String, enabled: bool) -> Result<(), String> {
    if !valid_id(&id) {
        return Err(format!("invalid plugin id: {id}"));
    }
    edit_state(|state| {
        state.disabled.retain(|d| d != &id);
        if !enabled {
            state.disabled.push(id);
        }
        Ok(())
    })?;
    // A disabled plugin's local server stops.
    plugin_backends::global().reconcile();
    Ok(())
}

/// An installed plugin that declares a server.
fn server_plugin(id: &str) -> Result<Plugin, String> {
    list_plugins()?
        .into_iter()
        .find(|p| p.id == id && p.server.is_some())
        .ok_or_else(|| format!("plugin {id} has no backend"))
}

/// Where the plugin's backend should run: "auto", "local" or "cloud".
#[tauri::command]
pub fn plugin_set_placement(id: String, mode: Mode) -> Result<(), String> {
    server_plugin(&id)?;
    edit_state(|state| {
        match mode {
            Mode::Auto => state.placements.remove(&id),
            Mode::Local => state.placements.insert(id, "local".into()),
            Mode::Cloud => state.placements.insert(id, "cloud".into()),
        };
        Ok(())
    })?;
    plugin_backends::global().reconcile();
    Ok(())
}

/// The cloud part as the review shows it.
#[derive(Debug, Serialize)]
pub struct CloudReview {
    /// `plugin:<id>`
    instance: String,
    /// the matching instance in the active workspace, if any
    #[serde(rename = "instanceId")]
    instance_id: Option<String>,
    workspace: Option<String>,
    /// what approving binds (`<env>/<slug>/<instance id>`); pass back to approve
    binding: Option<String>,
    /// that instance is already approved
    approved: bool,
}

/// What the user is asked to approve: exactly what will run, from where, with
/// what, and what it may reach, plus the fingerprint of the folder as shown.
#[derive(Debug, Serialize)]
pub struct ServerReview {
    /// local server: program and arguments
    command: Option<Vec<String>>,
    /// where the program resolves on this machine (absolute), or why not
    executable: Option<String>,
    /// the plugin folder reviewed
    cwd: String,
    /// where it will run from once approved: a read-only copy
    #[serde(rename = "runsFrom")]
    runs_from: String,
    /// local server: its extra environment, values included
    env: BTreeMap<String, String>,
    /// things worth a second look (env that changes what an interpreter loads)
    warnings: Vec<String>,
    cloud: Option<CloudReview>,
    /// files and bytes the fingerprint covers
    files: usize,
    bytes: u64,
    fingerprint: String,
    /// why this version can't be approved (e.g. `uv run` without a lock)
    problem: Option<String>,
    /// this exact version (and, if any, this cloud instance) is approved
    approved: bool,
    /// some version is approved (Revoke applies)
    #[serde(rename = "hasApproval")]
    has_approval: bool,
}

/// Where `command[0]` resolves: relative paths from the snapshot, bare names
/// the way the agent CLIs are found.
fn resolve_executable(program: &str, runs_from: &Path) -> String {
    let path = if program.starts_with('/') {
        PathBuf::from(program)
    } else if program.contains('/') {
        runs_from.join(program)
    } else {
        crate::cli_bin(program)
    };
    match path.canonicalize() {
        Ok(p) => p.display().to_string(),
        Err(_) if path.is_relative() => format!("{program} (not found in the usual places; left to PATH)"),
        Err(_) => format!("{} (not found)", path.display()),
    }
}

fn review(state_file: &Path, snapshots: &Path, id: &str, dir: &Path, binding: Option<(String, String, String)>) -> Result<ServerReview, String> {
    let server = load_folder(dir, id).and_then(|p| p.server).ok_or_else(|| format!("plugin {id} has no backend"))?;
    let tree = plugin_trust::tree(id, dir)?;
    let grant = load_state_at(state_file).grant(id, snapshots);
    let runs_from = plugin_trust::snapshot_dir(snapshots, id, &tree.fingerprint);
    let env = server.local.as_ref().map(|l| l.env.clone()).unwrap_or_default();
    let warnings = env
        .keys()
        .filter(|k| plugin_backends::risky_env(k))
        .map(|k| format!("{k} changes what the interpreter loads; check its value"))
        .collect();
    let cloud = server.cloud.as_ref().map(|c| {
        let approved = binding.as_ref().is_some_and(|(b, _, _)| grant.as_ref().is_some_and(|g| g.instances.contains(b)));
        CloudReview {
            instance: c.instance.clone(),
            instance_id: binding.as_ref().map(|b| b.1.clone()),
            workspace: binding.as_ref().map(|b| b.2.clone()),
            binding: binding.as_ref().map(|b| b.0.clone()),
            approved,
        }
    });
    let cloud_ok = cloud.as_ref().is_none_or(|c| c.binding.is_none() || c.approved);
    let problem = server.local.as_ref().and_then(|l| plugin_backends::check_fetchers(&l.command, dir).err());
    Ok(ServerReview {
        problem,
        executable: server.local.as_ref().map(|l| resolve_executable(&l.command[0], &runs_from)),
        command: server.local.map(|l| l.command),
        cwd: dir.display().to_string(),
        runs_from: runs_from.display().to_string(),
        env,
        warnings,
        cloud,
        files: tree.files,
        bytes: tree.bytes,
        approved: grant.as_ref().is_some_and(|g| g.fingerprint == tree.fingerprint) && cloud_ok,
        has_approval: grant.is_some() || load_state_at(state_file).server_approvals.contains_key(id),
        fingerprint: tree.fingerprint,
    })
}

/// The plugin's backend as the approval dialog shows it (every file read now).
#[tauri::command]
pub fn plugin_server_review(id: String) -> Result<ServerReview, String> {
    // The user is here: a Keychain prompt they denied earlier may be asked again.
    plugin_trust::retry_keychain();
    if !valid_id(&id) {
        return Err(format!("invalid plugin id: {id}"));
    }
    let dir = plugins_dir()?.join(&id);
    let binding = load_folder(&dir, &id).and_then(|p| p.server).and_then(|d| plugin_backends::global().cloud_binding(&d));
    review(&state_file()?, &plugin_trust::snapshot_root(), &id, &dir, binding)
}

/// Approve the plugin folder exactly as shown (`shown` fingerprint, and the
/// cloud `binding` shown, if any) or revoke.
///
/// Approving copies the folder into its snapshot, hashes the copy, and stores
/// a signed approval for the next epoch (so older approvals stop counting);
/// cloud instances approved before stay approved. Revoking bumps the epoch,
/// so no earlier approval can be put back, and removes the snapshots.
#[allow(clippy::too_many_arguments)]
fn approve(
    state_file: &Path,
    snapshots: &Path,
    id: &str,
    dir: &Path,
    approve: bool,
    shown: Option<&str>,
    shown_binding: Option<&str>,
    current_binding: Option<&str>,
) -> Result<(), String> {
    if !approve {
        // The local part always happens; a Keychain failure is reported after.
        let bumped = plugin_trust::bump(id);
        edit_state_at(state_file, |state| {
            state.server_approvals.remove(id);
            Ok(())
        })?;
        plugin_trust::remove_snapshot(&snapshots.join(id));
        return bumped.map(|_| ()).map_err(|e| {
            format!("revoked here (approval and copies deleted), but the Keychain counter couldn't be updated, so a saved copy of the old approval could still be put back: {e}")
        });
    }
    let shown = shown.ok_or("approving needs the fingerprint that was shown")?;
    if let Some(problem) = review(state_file, snapshots, id, dir, None)?.problem {
        return Err(problem);
    }
    if shown_binding != current_binding {
        return Err("the workspace's cloud instance changed since it was shown; review it again".into());
    }
    let previous = load_state_at(state_file).grant(id, snapshots);
    let (snapshot, tree) = plugin_trust::snapshot(id, dir, snapshots)?;
    if tree.fingerprint != shown {
        if previous.as_ref().is_none_or(|g| g.snapshot != snapshot) {
            plugin_trust::remove_snapshot(&snapshot);
        }
        return Err("the plugin changed since it was shown; review it again".into());
    }
    let mut instances = previous.map(|g| g.instances).unwrap_or_default();
    if let Some(b) = current_binding {
        if !instances.iter().any(|i| i == b) {
            instances.push(b.to_string());
        }
    }
    let mut a = Approval {
        plugin: id.to_string(),
        fingerprint: tree.fingerprint.clone(),
        instances,
        issued_at: crate::vault::now_ms(),
        nonce: uuid::Uuid::new_v4().simple().to_string(),
        epoch: plugin_trust::bump(id)?,
        mac: String::new(),
    };
    a.mac = plugin_trust::sign(&a.message())?;
    edit_state_at(state_file, |state| {
        state.server_approvals.insert(id.to_string(), serde_json::to_value(&a).map_err(|e| e.to_string())?);
        Ok(())
    })?;
    // Older versions' copies (and venvs) go; their processes are stopped by the reconcile.
    prune(&snapshots.join(id), Some(&tree.fingerprint));
    Ok(())
}

/// Remove everything in a plugin's snapshot folder except `keep`'s copy and
/// venv (`<fp>` and `<fp>.venv`); with `None`, the folder itself.
fn prune(dir: &Path, keep: Option<&str>) {
    let Some(keep) = keep else {
        plugin_trust::remove_snapshot(dir);
        return;
    };
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name != keep && name != format!("{keep}.venv") {
                plugin_trust::remove_snapshot(&e.path());
            }
        }
    }
}

/// At startup: drop copies no approval entry names (uninstalled plugins,
/// superseded versions, `.tmp-*` from a crash). Reads only the state file;
/// a kept copy still has to pass the MAC and epoch to be used.
pub fn sweep_snapshots(snapshots: &Path) {
    sweep_with(snapshots, &load_state());
}

fn sweep_with(snapshots: &Path, state: &State) {
    let Ok(entries) = std::fs::read_dir(snapshots) else { return };
    for e in entries.flatten() {
        let id = e.file_name().to_string_lossy().into_owned();
        let keep = state
            .server_approvals
            .get(&id)
            .and_then(|a| a.get("fingerprint")?.as_str().map(String::from))
            .filter(|fp| fp.len() == 64 && fp.chars().all(|c| c.is_ascii_hexdigit()));
        prune(&e.path(), keep.as_deref());
    }
}

/// Allow (or revoke) the plugin's backend exactly as reviewed: `fingerprint`
/// and `binding` from `plugin_server_review`.
#[tauri::command]
pub fn plugin_approve_server(id: String, approve: bool, fingerprint: Option<String>, binding: Option<String>) -> Result<(), String> {
    plugin_trust::retry_keychain();
    if !valid_id(&id) {
        return Err(format!("invalid plugin id: {id}"));
    }
    let dir = plugins_dir()?.join(&id);
    let current = load_folder(&dir, &id).and_then(|p| p.server).and_then(|d| plugin_backends::global().cloud_binding(&d)).map(|b| b.0);
    if approve {
        server_plugin(&id)?;
    }
    let done = self::approve(
        &state_file()?,
        &plugin_trust::snapshot_root(),
        &id,
        &dir,
        approve,
        fingerprint.as_deref(),
        binding.as_deref(),
        current.as_deref(),
    );
    // Also after a partly failed revoke: its processes must stop.
    plugin_backends::global().reconcile();
    done
}

/// Enabled plugins with a backend, for plugin_backends.rs.
pub fn backend_entries() -> Vec<plugin_backends::Entry> {
    plugins_with(false)
        .unwrap_or_default()
        .into_iter()
        .filter(|p| p.enabled)
        .filter_map(|p| {
            // What runs is what was approved: the snapshot's own manifest.
            let decl = match &p.grant {
                Some(g) => load_folder(&g.snapshot, &p.id)?.server?,
                None => p.server?,
            };
            Some(plugin_backends::Entry {
                decl,
                mode: p.placement.unwrap_or_default(),
                grant: p.grant,
                id: p.id,
            })
        })
        .collect()
}

/// Grant (or with `None`, revoke) the folder an "fs" plugin may read.
#[tauri::command]
pub fn plugin_set_root(id: String, path: Option<String>) -> Result<(), String> {
    fs_plugin(&id)?;
    edit_state(|state| {
        match path {
            Some(p) if PathBuf::from(&p).is_dir() => {
                state.roots.insert(id, p);
            }
            Some(p) => return Err(format!("not a folder: {p}")),
            None => {
                state.roots.remove(&id);
            }
        }
        Ok(())
    })
}

/// Set (or with `None`, reset to `wiki/`) the workspace-files prefix a
/// "cloud" plugin reads under. Returns the stored prefix.
#[tauri::command]
pub fn plugin_set_cloud_root(id: String, prefix: Option<String>) -> Result<String, String> {
    list_plugins()?
        .into_iter()
        .find(|p| p.id == id)
        .filter(|p| p.enabled && p.permissions.iter().any(|x| x == "cloud"))
        .ok_or_else(|| format!("plugin {id} has no cloud access"))?;
    edit_state(|state| {
        Ok(match prefix {
            Some(p) => {
                let p = cloud_prefix(&p).ok_or_else(|| format!("bad folder: {p}"))?;
                state.cloud_roots.insert(id, p.clone());
                p
            }
            None => {
                state.cloud_roots.remove(&id);
                DEFAULT_CLOUD_ROOT.to_string()
            }
        })
    })
}

/// A workspace-files folder as a prefix: "" (the whole library) or plain
/// segments ending in "/". Surrounding slashes are forgiven; `..`, `.`, empty
/// segments and backslashes are not.
pub fn cloud_prefix(p: &str) -> Option<String> {
    let p = p.trim().trim_matches('/');
    if p.is_empty() {
        return Some(String::new());
    }
    safe_relative(p)?;
    Some(format!("{p}/"))
}

/// A local folder an enabled plugin was granted, for the agent's data sources.
pub struct LocalRoot {
    pub title: String,
    pub path: String,
}

/// Enabled "fs" plugins with a granted folder that still exists.
pub fn local_roots() -> Vec<LocalRoot> {
    list_plugins()
        .unwrap_or_default()
        .into_iter()
        .filter(|p| p.enabled && p.permissions.iter().any(|x| x == "fs"))
        .filter_map(|p| {
            let path = p.root.filter(|r| Path::new(r).is_dir())?;
            Some(LocalRoot { title: p.title, path })
        })
        .collect()
}

/// (plugin title, prefix) of enabled "cloud" plugins: hints of where in the
/// workspace library their pages live.
pub fn cloud_roots() -> Vec<(String, String)> {
    list_plugins()
        .unwrap_or_default()
        .into_iter()
        .filter(|p| p.enabled && p.permissions.iter().any(|x| x == "cloud"))
        .filter_map(|p| Some((p.title, p.cloud_root?)))
        .collect()
}

#[derive(Serialize)]
pub struct FsEntry {
    name: String,
    /// relative to the plugin's root, with "/" separators
    path: String,
    dir: bool,
}

/// Refuse unless `id` is an installed, enabled plugin that declared `permission`.
pub fn require_permission(id: &str, permission: &str) -> Result<(), String> {
    list_plugins()?
        .into_iter()
        .find(|p| p.id == id)
        .filter(|p| p.enabled && p.permissions.iter().any(|x| x == permission))
        .map(|_| ())
        .ok_or_else(|| format!("plugin {id} has no \"{permission}\" permission"))
}

/// An installed, enabled plugin that declared "fs".
fn fs_plugin(id: &str) -> Result<Plugin, String> {
    list_plugins()?
        .into_iter()
        .find(|p| p.id == id)
        .filter(|p| p.enabled && p.permissions.iter().any(|x| x == "fs"))
        .ok_or_else(|| format!("plugin {id} has no file access"))
}

/// `rel` inside the plugin's granted folder, canonical, never outside it
/// (`..`, absolute paths and symlinks pointing out are all refused).
fn fs_path(id: &str, rel: &str) -> Result<(PathBuf, PathBuf), String> {
    let plugin = fs_plugin(id)?;
    let root = plugin.root.ok_or("no folder chosen yet")?;
    let root = PathBuf::from(root).canonicalize().map_err(|e| format!("folder: {e}"))?;
    let rel = rel.trim_matches('/');
    let target = if rel.is_empty() {
        root.clone()
    } else {
        root.join(safe_relative(rel).ok_or("bad path")?)
    };
    let target = target.canonicalize().map_err(|_| "not found".to_string())?;
    if !target.starts_with(&root) {
        return Err("outside the plugin's folder".into());
    }
    Ok((root, target))
}

#[tauri::command]
pub fn plugin_fs_list(id: String, path: String) -> Result<Vec<FsEntry>, String> {
    let (root, dir) = fs_path(&id, &path)?;
    let mut out: Vec<FsEntry> = std::fs::read_dir(&dir)
        .map_err(|e| e.to_string())?
        .filter_map(Result::ok)
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            // Dotfiles (.git, .obsidian…) are tooling, not content.
            if name.starts_with('.') {
                return None;
            }
            let full = e.path();
            let rel = full.strip_prefix(&root).ok()?.to_string_lossy().replace('\\', "/");
            Some(FsEntry { name, path: rel, dir: full.is_dir() })
        })
        .collect();
    out.sort_by(|a, b| b.dir.cmp(&a.dir).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
    Ok(out)
}

/// A text file inside the plugin's folder, up to 1 MiB.
#[tauri::command]
pub fn plugin_fs_read(id: String, path: String) -> Result<String, String> {
    let (_, file) = fs_path(&id, &path)?;
    let meta = std::fs::metadata(&file).map_err(|e| e.to_string())?;
    if !meta.is_file() {
        return Err("not a file".into());
    }
    if meta.len() > 1 << 20 {
        return Err("file is larger than 1 MiB".into());
    }
    let bytes = std::fs::read(&file).map_err(|e| e.to_string())?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Ids name folders and URL segments: keep them to a boring charset.
fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// A path relative to the plugin folder that can't climb out of it.
fn safe_relative(path: &str) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for seg in path.split('/') {
        if seg.is_empty() || seg == "." || seg == ".." || seg.contains(['\\', ':', '\0']) {
            return None;
        }
        out.push(seg);
    }
    // Belt and braces: whatever the platform parses, it must be plain names.
    out.components().all(|c| matches!(c, Component::Normal(_))).then_some(out)
}

/// Parse `<folder>/plugin.json`. The manifest id must match the folder so the
/// id alone locates the plugin's files.
fn parse_manifest(folder: &str, json: &str) -> Option<Plugin> {
    let m: Manifest = serde_json::from_str(json).ok()?;
    let entry = m.entry.filter(|e| !e.is_empty()).unwrap_or_else(|| "index.html".to_string());
    let sidebar = m.sidebar.filter(|e| !e.is_empty());
    if m.id != folder
        || !valid_id(&m.id)
        || m.title.trim().is_empty()
        || safe_relative(&entry).is_none()
        || sidebar.as_deref().is_some_and(|e| safe_relative(e).is_none())
    {
        return None;
    }
    let (server, server_error) = match m.server {
        None | Some(serde_json::Value::Null) => (None, None),
        Some(v) => match serde_json::from_value::<ServerDecl>(v) {
            Ok(decl) => match decl.validate(&m.id) {
                Ok(()) => (Some(decl), None),
                Err(e) => (None, Some(e)),
            },
            Err(e) => (None, Some(format!("server: {e}"))),
        },
    };
    Some(Plugin {
        id: m.id,
        title: m.title.trim().to_string(),
        description: m.description.filter(|d| !d.trim().is_empty()),
        entry,
        enabled: true,
        sidebar,
        permissions: m.permissions,
        root: None,
        cloud_root: None,
        icon: m.icon.as_deref().and_then(icon_spec),
        server,
        server_error,
        placement: None,
        server_approved: false,
        update_available: false,
        stale_approval: false,
        grant: None,
    })
}

/// A file's (mtime, size), to tell when it changed.
type Stamp = Option<(SystemTime, u64)>;

fn stamp(path: &Path) -> Stamp {
    std::fs::metadata(path).ok().and_then(|m| Some((m.modified().ok()?, m.len())))
}

/// Parsed plugin folders by path: (plugin.json stamp, SVG icon path + stamp,
/// the parse). Gateway requests list plugins on every message, so a folder is
/// re-read only when its plugin.json or icon file changed.
type ManifestCache = HashMap<PathBuf, (Stamp, Option<(PathBuf, Stamp)>, Option<Plugin>)>;
static MANIFESTS: Mutex<Option<ManifestCache>> = Mutex::new(None);

/// One plugin folder, from the cache while its files are unchanged.
fn load_folder(path: &Path, folder: &str) -> Option<Plugin> {
    let manifest = path.join("plugin.json");
    let ms = stamp(&manifest);
    ms?;
    if let Some((_, _, cached)) = MANIFESTS.lock().ok()?.as_ref().and_then(|m| m.get(path)).filter(|(m, icon, _)| {
        *m == ms && icon.as_ref().is_none_or(|(p, st)| stamp(p) == *st)
    }) {
        return cached.clone();
    }
    let json = std::fs::read_to_string(&manifest).ok()?;
    let mut icon_stamp = None;
    let plugin = parse_manifest(folder, &json).map(|mut plugin| {
        // A path icon becomes the file's contents; one that fails the checks is dropped.
        if let Some(rel) = plugin.icon.take() {
            plugin.icon = if rel.to_ascii_lowercase().ends_with(".svg") {
                let file = path.join(&rel);
                icon_stamp = Some((file.clone(), stamp(&file)));
                svg_icon(path, &rel)
            } else {
                Some(rel)
            };
        }
        plugin
    });
    if let Ok(mut m) = MANIFESTS.lock() {
        m.get_or_insert_with(HashMap::new).insert(path.to_path_buf(), (ms, icon_stamp, plugin.clone()));
    }
    plugin
}

fn scan(dir: &Path) -> Vec<Plugin> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut plugins: Vec<Plugin> = entries
        .filter_map(Result::ok)
        .filter(|e| e.path().is_dir())
        .filter_map(|e| load_folder(&e.path(), &e.file_name().into_string().ok()?))
        .collect();
    plugins.sort_by(|a, b| a.title.to_lowercase().cmp(&b.title.to_lowercase()));
    plugins
}

/// Installed plugins; invalid folders are skipped. The first call seeds an
/// example plugin so there is something to open and copy from.
#[tauri::command]
pub fn list_plugins() -> Result<Vec<Plugin>, String> {
    plugins_with(true)
}

/// `updates`: also compare each approved plugin's live folder with its
/// approved version (the UI wants it; gateway lookups don't).
fn plugins_with(updates: bool) -> Result<Vec<Plugin>, String> {
    let dir = plugins_dir()?;
    if !dir.exists() {
        seed(&dir).map_err(|e| format!("seed plugins: {e}"))?;
    }
    Ok(with_state(&dir, scan(&dir), &load_state(), &plugin_trust::snapshot_root(), updates))
}

/// Plugins with the user's settings: on/off, folders, placement, approval.
fn with_state(dir: &Path, plugins: Vec<Plugin>, state: &State, snapshots: &Path, updates: bool) -> Vec<Plugin> {
    plugins
        .into_iter()
        .map(|p| {
            // Only a genuine, current approval counts; it names its snapshot.
            let grant = state.server_approvals.contains_key(&p.id).then(|| state.grant(&p.id, snapshots)).flatten();
            let update = updates
                && grant.as_ref().is_some_and(|g| {
                    plugin_trust::fingerprint_cached(&p.id, &dir.join(&p.id)).ok().as_deref() != Some(g.fingerprint.as_str())
                });
            let stale = grant.is_none() && state.server_approvals.contains_key(&p.id);
            (p, grant, update, stale)
        })
        .map(|(p, grant, update_available, stale_approval)| Plugin {
            enabled: !state.disabled.contains(&p.id),
            root: state.roots.get(&p.id).cloned(),
            cloud_root: p.permissions.iter().any(|x| x == "cloud").then(|| {
                state.cloud_roots.get(&p.id).cloned().unwrap_or_else(|| DEFAULT_CLOUD_ROOT.to_string())
            }),
            placement: (p.server.is_some() || grant.is_some()).then(|| state.placement(&p.id)),
            server_approved: grant.is_some(),
            update_available,
            stale_approval,
            grant,
            ..p
        })
        .collect()
}

fn seed(dir: &Path) -> std::io::Result<()> {
    let board = dir.join("threads-board");
    std::fs::create_dir_all(&board)?;
    std::fs::write(board.join("plugin.json"), SEED_MANIFEST)?;
    std::fs::write(board.join("index.html"), SEED_INDEX)
}

/// Map a request path `/<id>/<file…>` to a file inside that plugin's folder.
/// Segments are decoded one by one (so `%2F` can't smuggle a separator), and
/// the canonical result must stay inside the plugin — which also stops symlinks
/// pointing elsewhere.
fn resolve(root: &Path, uri_path: &str, disabled: &HashSet<String>) -> Result<PathBuf, StatusCode> {
    let mut segs = uri_path.trim_start_matches('/').split('/');
    let id = segs.next().and_then(|s| web::percent_decode(s, false)).filter(|s| valid_id(s));
    let id = id.ok_or(StatusCode::NOT_FOUND)?;
    if disabled.contains(&id) {
        return Err(StatusCode::FORBIDDEN);
    }
    let rest = segs
        .map(|s| web::percent_decode(s, false))
        .collect::<Option<Vec<_>>>()
        .ok_or(StatusCode::BAD_REQUEST)?
        .join("/");
    let rel = safe_relative(&rest).ok_or(StatusCode::FORBIDDEN)?;

    let base = root.join(&id).canonicalize().map_err(|_| StatusCode::NOT_FOUND)?;
    let file = base.join(rel).canonicalize().map_err(|_| StatusCode::NOT_FOUND)?;
    if !file.starts_with(&base) {
        return Err(StatusCode::FORBIDDEN);
    }
    if !file.is_file() {
        return Err(StatusCode::NOT_FOUND);
    }
    Ok(file)
}

fn content_type(path: &Path) -> &'static str {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "wasm" => "application/wasm",
        "txt" | "md" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// Own files, inline script/style, no network: plugins talk to the app only
/// through the bridge. The frame is opaque-origin (no allow-same-origin), where
/// `'self'` may match nothing, so the scheme is named explicitly too.
fn plugin_csp(ancestors: &[String]) -> String {
    let own = "'self' aa-plugin: http://aa-plugin.localhost https://aa-plugin.localhost";
    [
        format!("default-src {own}"),
        format!("script-src {own} 'unsafe-inline'"),
        format!("style-src {own} 'unsafe-inline'"),
        format!("img-src {own} data: blob:"),
        format!("font-src {own} data:"),
        "connect-src 'none'".to_string(),
        "frame-src 'none'".to_string(),
        "form-action 'none'".to_string(),
        "base-uri 'none'".to_string(),
        format!("frame-ancestors {}", ancestors.join(" ")),
    ]
    .join("; ")
}

pub fn handle<R: Runtime>(ctx: UriSchemeContext<'_, R>, request: Request<Vec<u8>>, responder: UriSchemeResponder) {
    let ancestors = web::app_origins(ctx.app_handle());
    let path = request.uri().path().to_string();
    // File reads stay off the webview's thread.
    std::thread::spawn(move || {
        let response = plugins_dir()
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
            .and_then(|root| resolve(&root, &path, &disabled()))
            .and_then(|file| std::fs::read(&file).map(|b| (file, b)).map_err(|_| StatusCode::NOT_FOUND));
        responder.respond(match response {
            Ok((file, bytes)) => Response::builder()
                .header("Content-Type", content_type(&file))
                .header("Content-Security-Policy", plugin_csp(&ancestors))
                .header("X-Content-Type-Options", "nosniff")
                // Plugins are edited in place; always serve the current file.
                .header("Cache-Control", "no-store")
                .body(bytes)
                .expect("static response"),
            Err(status) => web::error(status, status.canonical_reason().unwrap_or("error")),
        });
    });
}

const SEED_MANIFEST: &str = r#"{
  "id": "threads-board",
  "title": "Threads board",
  "description": "Example plugin: every thread at a glance, with a quick-send box.",
  "entry": "index.html",
  "icon": "square-kanban"
}
"#;

const SEED_INDEX: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>Threads board</title>
<style>
  body { margin: 0; padding: 20px; font: 13px/1.45 -apple-system, system-ui, sans-serif; color: #0d0d0d; background: #fff; }
  h1 { font-size: 15px; margin: 0 0 12px; display: flex; justify-content: space-between; align-items: center; }
  ul { list-style: none; margin: 0 0 16px; padding: 0; border: 1px solid #ececec; border-radius: 10px; }
  li { padding: 8px 12px; border-top: 1px solid #ececec; cursor: pointer; display: flex; gap: 8px; align-items: center; }
  li:first-child { border-top: 0; }
  li.sel { background: #f3f3f3; }
  .runner { color: #6b6b6b; font-size: 12px; margin-left: auto; }
  .dot { width: 7px; height: 7px; border-radius: 50%; background: #d9d9d9; flex: none; }
  .dot.on { background: #15803d; }
  form { display: flex; gap: 8px; }
  input { flex: 1; padding: 8px 10px; border: 1px solid #ececec; border-radius: 8px; font: inherit; }
  button { padding: 7px 12px; border: 1px solid #ececec; border-radius: 8px; background: #fff; font: inherit; cursor: pointer; }
  button.primary { background: #0d0d0d; color: #fff; border-color: #0d0d0d; }
  .muted { color: #6b6b6b; }
</style>
</head>
<body>
<h1>Threads <button id="new">New thread</button></h1>
<ul id="list"><li class="muted">Loading…</li></ul>
<form id="send">
  <input id="text" placeholder="Message the selected thread" autocomplete="off">
  <button class="primary">Send</button>
</form>
<p id="status" class="muted"></p>
<script>
  // Bridge: one request/response pair per call, matched by id.
  const pending = new Map();
  let seq = 0;
  function call(method, params) {
    return new Promise((resolve, reject) => {
      const id = ++seq;
      pending.set(id, { resolve, reject });
      parent.postMessage({ type: "aa:request", id, method, params }, "*");
    });
  }
  addEventListener("message", (e) => {
    const m = e.data;
    if (e.source !== parent || !m || m.type !== "aa:response" || !pending.has(m.id)) return;
    const p = pending.get(m.id);
    pending.delete(m.id);
    m.error ? p.reject(new Error(m.error)) : p.resolve(m.result);
  });

  let selected = null;
  const $ = (id) => document.getElementById(id);
  const status = (text) => { $("status").textContent = text; };

  async function refresh() {
    const threads = await call("threads.list");
    if (!threads.some((t) => t.id === selected)) selected = threads[0]?.id ?? null;
    const list = $("list");
    list.replaceChildren();
    if (!threads.length) list.innerHTML = '<li class="muted">No threads yet</li>';
    for (const t of threads) {
      const li = document.createElement("li");
      li.className = t.id === selected ? "sel" : "";
      li.innerHTML = '<span class="dot"></span><span class="title"></span><span class="runner"></span>';
      li.querySelector(".dot").classList.toggle("on", t.running);
      li.querySelector(".title").textContent = t.title;
      li.querySelector(".runner").textContent = t.runner;
      li.onclick = () => { selected = t.id; refresh(); };
      list.append(li);
    }
  }

  $("new").onclick = async () => {
    selected = (await call("threads.create")).id;
    refresh();
  };
  $("send").onsubmit = async (e) => {
    e.preventDefault();
    const text = $("text").value.trim();
    if (!text || !selected) return;
    try {
      await call("threads.send", { threadId: selected, text });
      $("text").value = "";
      status("Sent.");
    } catch (err) {
      status(err.message);
    }
    refresh();
  };

  refresh().catch((err) => status(err.message));
  setInterval(() => refresh().catch(() => {}), 3000);
</script>
</body>
</html>
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("aa-plugins-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("demo/sub")).unwrap();
        std::fs::write(dir.join("demo/index.html"), "<p>hi</p>").unwrap();
        std::fs::write(dir.join("demo/sub/app.js"), "1").unwrap();
        std::fs::write(dir.join("secret.txt"), "nope").unwrap();
        dir
    }

    #[test]
    fn parses_manifests() {
        let p = parse_manifest("demo", r#"{"id":"demo","title":" Demo "}"#).unwrap();
        assert_eq!(p, Plugin {
            id: "demo".into(),
            title: "Demo".into(),
            description: None,
            entry: "index.html".into(),
            enabled: true,
            sidebar: None,
            permissions: vec![],
            root: None,
            cloud_root: None,
            icon: None,
            server: None,
            server_error: None,
            placement: None,
            server_approved: false,
            update_available: false,
            stale_approval: false,
            grant: None,
        });
        let p = parse_manifest("demo", r#"{"id":"demo","title":"D","description":"x","entry":"app/main.html"}"#).unwrap();
        assert_eq!(p.entry, "app/main.html");
        assert_eq!(p.description.as_deref(), Some("x"));

        assert!(parse_manifest("other", r#"{"id":"demo","title":"D"}"#).is_none(), "id must match folder");
        assert!(parse_manifest("demo", r#"{"id":"demo","title":"  "}"#).is_none(), "title required");
        assert!(parse_manifest("demo", r#"{"id":"demo","title":"D","entry":"../x.html"}"#).is_none());
        assert!(parse_manifest("demo", r#"{"id":"demo","title":"D","entry":"/etc/passwd"}"#).is_none());
        assert!(parse_manifest("a b", r#"{"id":"a b","title":"D"}"#).is_none());
        assert!(parse_manifest("demo", "not json").is_none());
    }

    #[test]
    fn parses_server_declarations() {
        let p = parse_manifest(
            "demo",
            r#"{"id":"demo","title":"D","server":{"local":{"command":["node","server.js"],"env":{"LOG":"1"}},"cloud":{"instance":"plugin:demo"}}}"#,
        )
        .unwrap();
        let server = p.server.unwrap();
        assert_eq!(server.local.as_ref().unwrap().command, ["node", "server.js"]);
        assert_eq!(server.local.as_ref().unwrap().env["LOG"], "1");
        assert_eq!(server.cloud.unwrap().instance, "plugin:demo");
        let p = parse_manifest("demo", r#"{"id":"demo","title":"D","server":{"cloud":{"instance":"plugin:demo"}}}"#).unwrap();
        assert!(p.server.unwrap().local.is_none(), "either part is optional");
        assert!(parse_manifest("demo", r#"{"id":"demo","title":"D"}"#).unwrap().server.is_none());
        // A bad `server` keeps the plugin (its page still works) without a
        // backend, and says why.
        for bad in [
            r#"{}"#,
            r#"{"local":{"command":[]}}"#,
            r#"{"local":{"command":"node server.js"}}"#,
            r#"{"local":{"command":["node"],"env":{"BAD KEY":"x"}}}"#,
            r#"{"cloud":{"instance":""}}"#,
            r#"{"cloud":{}}"#,
            r#""node server.js""#,
            // H1: only its own instance
            r#"{"cloud":{"instance":"gmail"}}"#,
            r#"{"cloud":{"instance":"plugin:other"}}"#,
        ] {
            let p = parse_manifest("demo", &format!(r#"{{"id":"demo","title":"D","server":{bad}}}"#)).unwrap();
            assert!(p.server.is_none(), "{bad}");
            assert!(p.server_error.is_some(), "{bad}");
        }
        let p = parse_manifest("demo", r#"{"id":"demo","title":"D","server":{"cloud":{"instance":"prod-db"}}}"#).unwrap();
        assert_eq!(p.server_error.as_deref(), Some("server.cloud.instance must be \"plugin:demo\" (got \"prod-db\")"));
    }

    /// A plugin folder with a server, in a temp root.
    struct Fixture {
        root: PathBuf,
        dir: PathBuf,
        state: PathBuf,
        snaps: PathBuf,
        id: String,
    }

    fn server_plugin_at(id: &str) -> Fixture {
        let root = std::env::temp_dir().join(format!("aa-plugins-{id}-{}", uuid::Uuid::new_v4().simple()));
        let dir = root.join("plugins").join(id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("plugin.json"),
            format!(r#"{{"id":"{id}","title":"Demo","server":{{"local":{{"command":["node","s.mjs"],"env":{{"TOKEN":"abc123","NODE_OPTIONS":"--trace-warnings"}}}},"cloud":{{"instance":"plugin:{id}"}}}}}}"#),
        )
        .unwrap();
        std::fs::write(dir.join("s.mjs"), "1").unwrap();
        Fixture { state: root.join("plugins-state.json"), snaps: root.join("snaps"), dir, root, id: id.to_string() }
    }

    impl Fixture {
        fn plugin(&self) -> Plugin {
            let dir = self.root.join("plugins");
            with_state(&dir, scan(&dir), &load_state_at(&self.state), &self.snaps, true).remove(0)
        }
        fn review(&self, binding: Option<&str>) -> ServerReview {
            let b = binding.map(|b| (b.to_string(), "inst".to_string(), "acme".to_string()));
            review(&self.state, &self.snaps, &self.id, &self.dir, b).unwrap()
        }
        fn approve(&self, yes: bool, shown: Option<&str>, shown_b: Option<&str>, current_b: Option<&str>) -> Result<(), String> {
            approve(&self.state, &self.snaps, &self.id, &self.dir, yes, shown, shown_b, current_b)
        }
        fn grant(&self) -> Option<Grant> {
            load_state_at(&self.state).grant(&self.id, &self.snaps)
        }
    }

    /// The review shows what runs and from where; approving needs what was
    /// shown; the approved copy runs while the live folder moves on.
    #[test]
    fn approvals_snapshot_what_was_shown() {
        let f = server_plugin_at("demo-approve");
        let r = f.review(None);
        assert_eq!(r.command.as_deref(), Some(&["node".to_string(), "s.mjs".to_string()][..]));
        assert!(r.executable.as_deref().is_some_and(|e| e.contains("node")), "{:?}", r.executable);
        assert_eq!(r.env["TOKEN"], "abc123");
        assert!(r.warnings.iter().any(|w| w.starts_with("NODE_OPTIONS")), "{:?}", r.warnings);
        assert_eq!(r.cwd, f.dir.display().to_string());
        assert!(r.runs_from.ends_with(&r.fingerprint), "{}", r.runs_from);
        assert_eq!(r.cloud.as_ref().unwrap().instance, "plugin:demo-approve");
        assert_eq!((r.files, r.approved, r.has_approval), (2, false, false));

        assert!(f.approve(true, None, None, None).is_err(), "no fingerprint, no approval");
        // Changed between showing and clicking: refused, nothing kept.
        std::fs::write(f.dir.join("s.mjs"), "2").unwrap();
        let err = f.approve(true, Some(&r.fingerprint), None, None).unwrap_err();
        assert!(err.contains("changed since it was shown"), "{err}");
        assert!(f.grant().is_none());
        assert!(!f.snaps.join(&f.id).read_dir().unwrap().any(|e| e.unwrap().file_name().to_string_lossy().len() == 64), "no stray snapshot");

        let r = f.review(None);
        f.approve(true, Some(&r.fingerprint), None, None).unwrap();
        let g = f.grant().unwrap();
        assert_eq!(g.fingerprint, r.fingerprint);
        assert_eq!(std::fs::read_to_string(g.snapshot.join("s.mjs")).unwrap(), "2");
        let p = f.plugin();
        assert!(p.server_approved && !p.update_available);
        assert!(f.review(None).approved);

        // A live edit: still approved (the copy runs), but an update is available.
        std::fs::write(f.dir.join("s.mjs"), "3").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2100));
        let p = f.plugin();
        assert!(p.server_approved && p.update_available, "update available, approval kept");
        assert_eq!(std::fs::read_to_string(g.snapshot.join("s.mjs")).unwrap(), "2", "the copy didn't move");
        // Approving the update replaces the copy.
        let r = f.review(None);
        f.approve(true, Some(&r.fingerprint), None, None).unwrap();
        let g2 = f.grant().unwrap();
        assert_ne!(g2.snapshot, g.snapshot);
        assert!(!g.snapshot.exists(), "the old copy is gone");
        assert!(!f.plugin().update_available);
        plugin_trust::remove_snapshot(&f.root);
    }

    /// NEW-6: what a running backend writes (caches) never touches its
    /// approval: the copy is read-only and caches go to the data folder; even
    /// cache files appearing in the live folder only offer an update.
    #[test]
    fn cache_writes_dont_revoke() {
        let f = server_plugin_at("demo-cache");
        let r = f.review(None);
        f.approve(true, Some(&r.fingerprint), None, None).unwrap();
        let g = f.grant().unwrap();
        assert!(std::fs::create_dir_all(g.snapshot.join("__pycache__")).is_err(), "the copy can't be written");
        std::fs::create_dir_all(f.dir.join("__pycache__")).unwrap();
        std::fs::write(f.dir.join("__pycache__/s.cpython-312.pyc"), "x").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2100));
        let p = f.plugin();
        assert!(p.server_approved, "still approved");
        assert_eq!(f.grant().unwrap(), g, "same approved copy");
        assert!(plugin_trust::verify(&f.id, &g.snapshot, &g.fingerprint).is_ok());
        plugin_trust::remove_snapshot(&f.root);
    }

    /// NEW-4: approvals forged, replayed after a revoke, or superseded don't
    /// count; a stale entry can still be revoked.
    #[test]
    fn approvals_cant_be_forged_or_replayed() {
        let f = server_plugin_at("demo-replay");
        let r = f.review(None);
        f.approve(true, Some(&r.fingerprint), None, None).unwrap();
        let genuine = std::fs::read_to_string(&f.state).unwrap();
        assert!(f.grant().is_some());
        let entry: serde_json::Value = serde_json::from_str::<serde_json::Value>(&genuine).unwrap()["serverApprovals"][&f.id].clone();
        for field in ["plugin", "fingerprint", "issued_at", "nonce", "epoch", "mac"] {
            assert!(entry.get(field).is_some(), "{field}");
        }

        // Forgeries: tampered fields, no MAC, an old-format entry.
        for forged in [
            { let mut e = entry.clone(); e["mac"] = serde_json::json!("0".repeat(64)); e },
            { let mut e = entry.clone(); e["instances"] = serde_json::json!(["ru/acme/x"]); e },
            { let mut e = entry.clone(); e["plugin"] = serde_json::json!("other"); e },
            serde_json::json!({ "fingerprint": r.fingerprint, "mac": "00" }),
        ] {
            std::fs::write(&f.state, serde_json::json!({ "serverApprovals": { &f.id: forged } }).to_string()).unwrap();
            assert!(f.grant().is_none(), "{forged}");
            assert!(f.plugin().stale_approval, "Revoke stays available");
        }

        // Revoke, then put the saved (genuine) entry back: it doesn't count.
        std::fs::write(&f.state, &genuine).unwrap();
        f.approve(false, None, None, None).unwrap();
        assert!(f.grant().is_none());
        std::fs::write(&f.state, &genuine).unwrap();
        assert!(f.grant().is_none(), "replayed approval rejected");
        assert!(f.plugin().stale_approval && !f.plugin().server_approved);
        // A later approval supersedes the earlier ones too.
        f.approve(true, Some(&f.review(None).fingerprint), None, None).unwrap();
        let second = std::fs::read_to_string(&f.state).unwrap();
        f.approve(true, Some(&f.review(None).fingerprint), None, None).unwrap();
        std::fs::write(&f.state, &second).unwrap();
        assert!(f.grant().is_none(), "superseded approval rejected");
        plugin_trust::remove_snapshot(&f.root);
    }

    /// R2-4: revoking works locally even when the Keychain can't be written.
    #[test]
    fn revoke_without_the_keychain() {
        let f = server_plugin_at("demo-revoke-nokc");
        f.approve(true, Some(&f.review(None).fingerprint), None, None).unwrap();
        assert!(f.grant().is_some());
        plugin_trust::TEST_FAIL_WRITES.lock().unwrap().push(f.id.clone());
        let err = f.approve(false, None, None, None).unwrap_err();
        plugin_trust::TEST_FAIL_WRITES.lock().unwrap().retain(|i| i != &f.id);
        assert!(err.contains("revoked here") && err.contains("Keychain"), "{err}");
        assert!(f.grant().is_none(), "approval entry deleted");
        assert!(!f.snaps.join(&f.id).exists(), "copies deleted");
        assert!(!f.plugin().server_approved && !f.plugin().stale_approval);
        plugin_trust::remove_snapshot(&f.root);
    }

    /// R2-1: a `uv run` plugin without a lock can't be approved.
    #[test]
    fn uv_without_a_lock_cant_be_approved() {
        let f = server_plugin_at("demo-uv");
        std::fs::write(f.dir.join("plugin.json"), r#"{"id":"demo-uv","title":"UV","server":{"local":{"command":["uv","run","server.py"]}}}"#).unwrap();
        std::fs::write(f.dir.join("server.py"), "print(1)").unwrap();
        let r = f.review(None);
        assert!(r.problem.as_deref().is_some_and(|p| p.contains("uv.lock")), "{:?}", r.problem);
        assert!(f.approve(true, Some(&r.fingerprint), None, None).unwrap_err().contains("uv.lock"));
        std::fs::write(f.dir.join("uv.lock"), "version = 1").unwrap();
        let r = f.review(None);
        assert!(r.problem.is_none());
        f.approve(true, Some(&r.fingerprint), None, None).unwrap();
        plugin_trust::remove_snapshot(&f.root);
    }

    /// R2-7: startup sweep keeps only copies an approval entry names.
    #[test]
    fn sweeps_orphan_snapshots() {
        let root = std::env::temp_dir().join(format!("aa-sweep-{}", uuid::Uuid::new_v4().simple()));
        let fp = "a".repeat(64);
        for d in [format!("gone/{fp}"), format!("kept/{fp}"), format!("kept/{fp}.venv"), format!("kept/{}", "b".repeat(64)), "kept/.tmp-x".into()] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        let state = State {
            server_approvals: [("kept".to_string(), serde_json::json!({ "fingerprint": fp }))].into(),
            ..State::default()
        };
        sweep_with(&root, &state);
        let left: Vec<String> = std::fs::read_dir(root.join("kept")).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        assert!(!root.join("gone").exists());
        assert_eq!(left.len(), 2, "{left:?}");
        assert!(left.contains(&fp) && left.contains(&format!("{fp}.venv")));
        plugin_trust::remove_snapshot(&root);
    }

    /// NEW-5: approving binds the cloud instance that was shown, by id and
    /// workspace; others with the same name need their own review.
    #[test]
    fn cloud_approval_binds_the_instance_shown() {
        let f = server_plugin_at("demo-cloud");
        let a = "ru/acme/6f1c2a3b-0000-4000-8000-000000000001";
        let b = "ru/acme/6f1c2a3b-0000-4000-8000-0000000000ff";
        let r = f.review(Some(a));
        assert_eq!(r.cloud.as_ref().unwrap().binding.as_deref(), Some(a));
        // The instance changed between showing and clicking.
        let err = f.approve(true, Some(&r.fingerprint), Some(a), Some(b)).unwrap_err();
        assert!(err.contains("cloud instance changed"), "{err}");
        f.approve(true, Some(&r.fingerprint), Some(a), Some(a)).unwrap();
        assert_eq!(f.grant().unwrap().instances, [a]);
        assert!(f.review(Some(a)).approved);
        assert!(!f.review(Some(b)).approved, "another instance with the same name isn't approved");
        // Reviewing it adds it, keeping the first.
        f.approve(true, Some(&r.fingerprint), Some(b), Some(b)).unwrap();
        assert_eq!(f.grant().unwrap().instances, [a, b]);
        plugin_trust::remove_snapshot(&f.root);
    }

    /// NEW-6: a Node plugin with its dependencies (more files than the old
    /// 20 000 cap, `.bin` links) can be approved; the review shows the size.
    #[test]
    fn node_modules_plugin_is_approvable() {
        let f = server_plugin_at("demo-nm");
        for pkg in 0..210 {
            let d = f.dir.join(format!("node_modules/pkg{pkg}/lib"));
            std::fs::create_dir_all(&d).unwrap();
            for i in 0..100 {
                std::fs::write(d.join(format!("m{i}.js")), format!("module.exports={i}")).unwrap();
            }
        }
        std::fs::create_dir_all(f.dir.join("node_modules/.bin")).unwrap();
        std::os::unix::fs::symlink("../pkg0/lib/m0.js", f.dir.join("node_modules/.bin/tool")).unwrap();
        let r = f.review(None);
        assert_eq!(r.files, 2 + 210 * 100 + 1);
        assert!(r.bytes > 300_000, "{}", r.bytes);
        f.approve(true, Some(&r.fingerprint), None, None).unwrap();
        let g = f.grant().unwrap();
        assert_eq!(std::fs::read_to_string(g.snapshot.join("node_modules/.bin/tool")).unwrap(), "module.exports=0");
        plugin_trust::remove_snapshot(&f.root);
    }

    #[test]
    fn state_file_stays_backward_compatible() {
        // A file from before backends: new fields default, nothing is lost.
        let old: State = serde_json::from_str(r#"{"disabled":["x"],"roots":{"a":"/x"},"cloudRoots":{"a":"docs/"}}"#).unwrap();
        assert!(old.placements.is_empty() && old.server_approvals.is_empty());
        let json = serde_json::to_string(&old).unwrap();
        assert!(!json.contains("placements") && !json.contains("serverApprovals"), "unchanged shape: {json}");
        let empty: State = serde_json::from_str("{}").unwrap();
        assert!(empty.disabled.is_empty());

        let new: State = serde_json::from_str(r#"{"placements":{"a":"cloud","b":"local"},"serverApprovals":{"a":"{}"}}"#).unwrap();
        assert_eq!((new.placement("a"), new.placement("b"), new.placement("c")), (Mode::Cloud, Mode::Local, Mode::Auto));
        let back: State = serde_json::from_str(&serde_json::to_string(&new).unwrap()).unwrap();
        assert_eq!(back.placements, new.placements);
        assert_eq!(back.server_approvals, new.server_approvals);
        // A mode from a newer build reads as auto; the rest of the file survives.
        let newer: State = serde_json::from_str(r#"{"disabled":["x"],"placements":{"a":"moon"}}"#).unwrap();
        assert_eq!((newer.placement("a"), newer.disabled.len()), (Mode::Auto, 1));
    }

    #[test]
    fn parses_icons() {
        let icon = |v: &str| parse_manifest("demo", &format!(r#"{{"id":"demo","title":"D","icon":{v}}}"#)).unwrap().icon;
        assert_eq!(icon(r#""radio-tower""#).as_deref(), Some("radio-tower"));
        assert_eq!(icon(r#""icons/glyph.svg""#).as_deref(), Some("icons/glyph.svg"));
        for bad in [r#""../x.svg""#, r#""/etc/x.svg""#, r#""icon.png""#, r#""Radio Tower""#, r#""a/b""#, r#""""#, "null"] {
            assert_eq!(icon(bad), None, "{bad}");
        }
    }

    #[test]
    fn loads_svg_icons_inside_the_plugin_only() {
        let root = temp_root("icons");
        let demo = root.join("demo");
        std::fs::write(demo.join("glyph.svg"), r#"<svg viewBox="0 0 16 16"><path d="M1 1h14"/></svg>"#).unwrap();
        std::fs::write(demo.join("big.svg"), format!("<svg>{}</svg>", " ".repeat(20_000))).unwrap();
        std::fs::write(demo.join("fake.svg"), "not an image").unwrap();
        std::fs::write(root.join("outside.svg"), "<svg/>").unwrap();
        let url = svg_icon(&demo, "glyph.svg").unwrap();
        assert!(url.starts_with("data:image/svg+xml;base64,"));
        assert_eq!(svg_icon(&demo, "big.svg"), None, "size cap");
        assert_eq!(svg_icon(&demo, "fake.svg"), None, "not an svg");
        assert_eq!(svg_icon(&demo, "missing.svg"), None);
        assert_eq!(svg_icon(&demo, "../outside.svg"), None, "traversal");
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(root.join("outside.svg"), demo.join("link.svg")).unwrap();
            assert_eq!(svg_icon(&demo, "link.svg"), None, "symlink escape");
        }

        std::fs::write(demo.join("plugin.json"), r#"{"id":"demo","title":"Demo","icon":"glyph.svg"}"#).unwrap();
        assert_eq!(scan(&root)[0].icon.as_deref(), Some(url.as_str()));
        std::fs::write(demo.join("plugin.json"), r#"{"id":"demo","title":"Demo","icon":"big.svg"}"#).unwrap();
        assert_eq!(scan(&root)[0].icon, None, "a failed icon falls back to none");
        std::fs::write(demo.join("plugin.json"), r#"{"id":"demo","title":"Demo","icon":"kanban"}"#).unwrap();
        assert_eq!(scan(&root)[0].icon.as_deref(), Some("kanban"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn normalizes_cloud_prefixes() {
        assert_eq!(cloud_prefix("wiki").as_deref(), Some("wiki/"));
        assert_eq!(cloud_prefix(" /wiki/pages/ ").as_deref(), Some("wiki/pages/"));
        assert_eq!(cloud_prefix("").as_deref(), Some(""));
        assert_eq!(cloud_prefix("/").as_deref(), Some(""));
        for bad in ["..", "wiki/../x", "a//b", "./a", "a\\b", "a/./b"] {
            assert_eq!(cloud_prefix(bad), None, "{bad}");
        }
        let state: State = serde_json::from_str(r#"{"roots":{"a":"/x"},"cloudRoots":{"a":"docs/"}}"#).unwrap();
        assert_eq!(state.cloud_roots["a"], "docs/");
        assert!(serde_json::to_string(&state).unwrap().contains("\"cloudRoots\""));
    }

    #[test]
    fn disabled_plugins_are_not_served() {
        let root = temp_root("off");
        let off: HashSet<String> = ["demo".to_string()].into();
        assert_eq!(resolve(&root, "/demo/index.html", &off), Err(StatusCode::FORBIDDEN));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn resolves_files_inside_the_plugin() {
        let root = temp_root("ok");
        assert!(resolve(&root, "/demo/index.html", &HashSet::new()).unwrap().ends_with("demo/index.html"));
        assert!(resolve(&root, "/demo/sub/app.js", &HashSet::new()).unwrap().ends_with("sub/app.js"));
        assert!(resolve(&root, "/demo/sub%2Fapp.js", &HashSet::new()).is_ok(), "decoded separator is still inside");
        assert_eq!(resolve(&root, "/demo/missing.html", &HashSet::new()), Err(StatusCode::NOT_FOUND));
        assert_eq!(resolve(&root, "/demo/sub", &HashSet::new()), Err(StatusCode::NOT_FOUND), "directories are not served");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn blocks_path_traversal() {
        let root = temp_root("traversal");
        assert_eq!(resolve(&root, "/demo/../secret.txt", &HashSet::new()), Err(StatusCode::FORBIDDEN));
        assert_eq!(resolve(&root, "/demo/%2e%2e/secret.txt", &HashSet::new()), Err(StatusCode::FORBIDDEN));
        assert_eq!(resolve(&root, "/demo/sub%2F..%2F..%2Fsecret.txt", &HashSet::new()), Err(StatusCode::FORBIDDEN));
        assert_eq!(resolve(&root, "/demo/..%5Csecret.txt", &HashSet::new()), Err(StatusCode::FORBIDDEN));
        assert_eq!(resolve(&root, "/demo//etc/passwd", &HashSet::new()), Err(StatusCode::FORBIDDEN));
        assert_eq!(resolve(&root, "/demo/", &HashSet::new()), Err(StatusCode::FORBIDDEN));
        assert_eq!(resolve(&root, "/..%2Fsecret.txt", &HashSet::new()), Err(StatusCode::NOT_FOUND), "bad id");
        assert_eq!(resolve(&root, "/%2e%2e/secret.txt", &HashSet::new()), Err(StatusCode::NOT_FOUND), "bad id");
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(root.join("secret.txt"), root.join("demo/link.txt")).unwrap();
            assert_eq!(resolve(&root, "/demo/link.txt", &HashSet::new()), Err(StatusCode::FORBIDDEN), "symlink escape");
        }
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn seeds_a_valid_example() {
        let root = std::env::temp_dir().join(format!("aa-plugins-seed-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        seed(&root).unwrap();
        let plugins = scan(&root);
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].id, "threads-board");
        assert!(resolve(&root, "/threads-board/index.html", &HashSet::new()).is_ok());
        let _ = std::fs::remove_dir_all(root);
    }
}
