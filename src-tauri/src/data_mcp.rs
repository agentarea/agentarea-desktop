//! Data tools for local agent CLIs: the MCP server `agentarea_data`, served on
//! the vault's loopback server at `/data` (secrets_mcp.rs owns the HTTP side).
//!
//! A thread sees two kinds of data:
//! - **local** folders — every enabled plugin's granted folder (e.g. LLM Wiki)
//!   plus the thread's own folder. The agent reads those with its own file
//!   tools; `run_agent` grants the folders (`--add-dir` for Claude).
//! - **cloud** files — the AgentArea workspace library (`/v1/files`), read
//!   through `cloud_list` / `cloud_read` here with the user's access token.
//!
//! The token, API base and workspace live only in the turn's `DataTurn`, which
//! is dropped (and its bearer revoked) when the turn ends. Everything a cloud
//! file returns passes through the vault redactor.

use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::secrets_mcp::PROTOCOL_VERSIONS;
use crate::vault::Redactor;

/// Cloud files the agent may read in one call.
pub const MAX_READ: usize = 1 << 20;
/// How long one `/v1/files` listing is reused within a turn.
const LISTING_TTL: Duration = Duration::from_secs(20);
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_ENTRIES: usize = 500;
/// What `agentarea://files/<path>` references in prompts look like.
pub const CLOUD_SCHEME: &str = "agentarea://files/";

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Source {
    pub name: String,
    /// "local" | "cloud"
    pub kind: &'static str,
    /// an absolute folder, or `agentarea://files/` for the workspace library
    pub location: String,
    pub description: String,
}

/// What the cloud tools need to call the AgentArea API as the user.
pub struct CloudAccess {
    pub api_base: String,
    pub token: String,
    /// `X-AgentArea-Workspace` (id or slug); None = the user's default
    pub workspace: Option<String>,
}

/// The app's current AgentArea access token (with its API base), pushed by the
/// frontend on every sign-in and renewal. A turn uses it instead of the copy it
/// got at start, so a turn running past the ~1h token life keeps working.
static LIVE_TOKEN: Mutex<Option<(String, String)>> = Mutex::new(None);

#[tauri::command]
pub fn set_cloud_token(api_base: String, token: Option<String>) {
    if let Ok(mut live) = LIVE_TOKEN.lock() {
        *live = token.map(|t| (api_base, t));
    }
}

fn token_for(cloud: &CloudAccess) -> String {
    live_token(&cloud.api_base, &cloud.token)
}

/// The app's current token for `api_base`, else `fallback` (the one a turn got
/// at start). Also used by the AgentArea MCP proxy (secrets_mcp.rs).
pub fn live_token(api_base: &str, fallback: &str) -> String {
    LIVE_TOKEN
        .lock()
        .ok()
        .and_then(|live| live.as_ref().filter(|(base, _)| base == api_base).map(|(_, t)| t.clone()))
        .unwrap_or_else(|| fallback.to_string())
}

/// One turn's data state; lives as long as the turn's bearer token.
pub struct DataTurn {
    pub sources: Vec<Source>,
    cloud: Option<CloudAccess>,
    redactor: Redactor,
    listing: Mutex<Option<(Instant, Listing)>>,
}

impl DataTurn {
    pub fn new(sources: Vec<Source>, cloud: Option<CloudAccess>, redactor: Redactor) -> DataTurn {
        DataTurn { sources, cloud, redactor, listing: Mutex::new(None) }
    }
}

// ── sources ──────────────────────────────────────────────────────────────────

/// The thread's data sources: plugin folders, the thread folder, and (when
/// signed in) the workspace library with the cloud plugins' folders as hints.
pub fn sources(
    local: &[crate::plugins::LocalRoot],
    cwd: &Path,
    cloud: Option<(&str, &[(String, String)])>,
) -> Vec<Source> {
    let mut out: Vec<Source> = local
        .iter()
        .map(|r| Source {
            name: r.title.clone(),
            kind: "local",
            location: r.path.clone(),
            description: format!(
                "Local folder the user granted to the {} plugin. Read files in it directly with your own file tools, by absolute path (read-only).",
                r.title
            ),
        })
        .collect();
    out.push(Source {
        name: "Thread folder".into(),
        kind: "local",
        location: cwd.display().to_string(),
        description: "This thread's working folder (your current directory).".into(),
    });
    if let Some((workspace, hints)) = cloud {
        let mut description = format!(
            "Files in the AgentArea workspace \"{workspace}\" (cloud library). Browse with cloud_list, read with cloud_read. `{CLOUD_SCHEME}<path>` references mean these files."
        );
        for (title, prefix) in hints {
            let at = if prefix.is_empty() { "the top level".to_string() } else { prefix.clone() };
            description.push_str(&format!(" {title} keeps its cloud pages under {at}."));
        }
        out.push(Source { name: format!("AgentArea · {workspace}"), kind: "cloud", location: CLOUD_SCHEME.into(), description });
    }
    out
}

/// Appended to every local turn's prompt: what the thread can read and how.
pub fn agent_hint(sources: &[Source]) -> String {
    let mut hint = String::from("[AgentArea data] Data sources for this thread:");
    for s in sources {
        hint.push_str(&format!("\n- {} ({}): {}", s.name, s.kind, s.location));
    }
    hint.push_str("\nLocal paths are readable directly with your file tools.");
    if sources.iter().any(|s| s.kind == "cloud") {
        hint.push_str(&format!(
            " `{CLOUD_SCHEME}<path>` is a file in the AgentArea workspace library: read it with the agentarea_data MCP tool cloud_read (mcp__agentarea_data__cloud_read), browse with cloud_list."
        ));
    } else {
        hint.push_str(" Cloud files are unavailable: the user is not signed in to AgentArea.");
    }
    hint.push_str(" list_sources (agentarea_data) repeats this list.");
    hint
}

// ── workspace files: paths and listing ───────────────────────────────────────

#[derive(Clone, Debug, Deserialize)]
struct FileInfo {
    path: String,
    #[serde(default)]
    size: u64,
}

#[derive(Clone, Debug, Deserialize)]
struct Listing {
    #[serde(default)]
    files: Vec<FileInfo>,
    #[serde(default)]
    directories: Vec<String>,
}

#[derive(Debug, PartialEq, Serialize)]
struct Entry {
    name: String,
    /// full workspace path; folders without the trailing "/"
    path: String,
    dir: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    size: Option<u64>,
}

/// A workspace path from the agent: `agentarea://files/` and leading slashes
/// dropped, then plain segments only (no `..`, `.`, empty or backslash).
/// `folder` allows (and drops) a trailing slash and the empty root.
fn clean_path(raw: &str, folder: bool) -> Result<String, String> {
    let p = raw.trim();
    let p = p.strip_prefix(CLOUD_SCHEME).unwrap_or(p).trim_start_matches('/');
    let p = if folder { p.trim_end_matches('/') } else { p };
    if p.is_empty() {
        return if folder { Ok(String::new()) } else { Err("path is required, e.g. wiki/index.md".into()) };
    }
    let ok = p.split('/').all(|s| !s.is_empty() && s != "." && s != ".." && !s.contains(['\\', '\0']));
    if ok {
        Ok(p.to_string())
    } else {
        Err(format!("bad path: {raw} (use a plain workspace path like wiki/index.md; no .. or empty segments)"))
    }
}

/// One folder level of the flat listing: the files directly in `folder`
/// (a clean path, "" = top) and the folders under it, from file paths and
/// the listing's empty-folder markers alike. Folders first, then by name.
fn list_level(listing: &Listing, folder: &str) -> Vec<Entry> {
    let prefix = if folder.is_empty() { String::new() } else { format!("{folder}/") };
    let mut out: Vec<Entry> = Vec::new();
    let add_dir = |out: &mut Vec<Entry>, name: &str| {
        if !out.iter().any(|e| e.dir && e.name == name) {
            out.push(Entry { name: name.into(), path: format!("{prefix}{name}"), dir: true, size: None });
        }
    };
    for f in &listing.files {
        let Some(rest) = f.path.strip_prefix(&prefix) else { continue };
        match rest.split_once('/') {
            Some((dir, _)) if !dir.is_empty() => add_dir(&mut out, dir),
            Some(_) => {}
            None if !rest.is_empty() => {
                out.push(Entry { name: rest.into(), path: f.path.clone(), dir: false, size: Some(f.size) })
            }
            None => {}
        }
    }
    for d in &listing.directories {
        let Some(rest) = d.strip_prefix(&prefix) else { continue };
        if let Some(dir) = rest.split('/').next().filter(|s| !s.is_empty()) {
            add_dir(&mut out, dir);
        }
    }
    out.sort_by(|a, b| b.dir.cmp(&a.dir).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
    out
}

/// Text we hand the agent: text/* and the usual structured types; for a
/// generic or missing type, decided by the file extension.
fn is_text(content_type: Option<&str>, path: &str) -> bool {
    let ct = content_type.unwrap_or("").split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    if ct.starts_with("text/") {
        return true;
    }
    let structured = ["json", "xml", "yaml", "javascript", "markdown", "csv", "toml", "x-sh", "sql", "x-ndjson"];
    if ct.starts_with("application/") && structured.iter().any(|k| ct.contains(k)) {
        return true;
    }
    if !(ct.is_empty() || ct == "application/octet-stream" || ct == "binary/octet-stream") {
        return false;
    }
    let ext = path.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    [
        "md", "markdown", "mdx", "txt", "text", "json", "jsonl", "ndjson", "yaml", "yml", "toml", "csv", "tsv", "xml",
        "html", "htm", "css", "js", "mjs", "ts", "tsx", "jsx", "py", "rs", "go", "rb", "java", "sh", "sql", "ini",
        "cfg", "conf", "log", "rst", "org", "tex", "svg", "env",
    ]
    .contains(&ext.as_str())
}

/// Each segment percent-encoded, "/" kept.
fn encode_path(path: &str) -> String {
    path.split('/').map(crate::vault::percent_encode).collect::<Vec<_>>().join("/")
}

// ── HTTP ─────────────────────────────────────────────────────────────────────

fn api_error(status: u16, what: &str) -> String {
    match status {
        401 => "AgentArea rejected the sign-in (401); the user may need to sign in again".into(),
        403 => format!("AgentArea refused access to {what} (403)"),
        404 => format!("not found: {what} (cloud_list shows what exists)"),
        s => format!("AgentArea answered {s} for {what}"),
    }
}

async fn get(cloud: &CloudAccess, path: &str) -> Result<reqwest::Response, String> {
    let client = reqwest::Client::builder().timeout(HTTP_TIMEOUT).build().map_err(|e| e.to_string())?;
    let mut req = client
        .get(format!("{}{path}", cloud.api_base.trim_end_matches('/')))
        .header("Authorization", format!("Bearer {}", token_for(cloud)));
    if let Some(w) = &cloud.workspace {
        req = req.header("X-AgentArea-Workspace", w);
    }
    req.send().await.map_err(|e| format!("AgentArea request failed: {}", e.without_url()))
}

async fn fetch_listing(cloud: &CloudAccess) -> Result<Listing, String> {
    let res = get(cloud, "/v1/files").await?;
    if !res.status().is_success() {
        return Err(api_error(res.status().as_u16(), "the workspace file list"));
    }
    res.json::<Listing>().await.map_err(|e| format!("unexpected file list: {}", e.without_url()))
}

/// One workspace file as text, at most `MAX_READ` bytes.
async fn fetch_text(cloud: &CloudAccess, path: &str) -> Result<String, String> {
    let mut res = get(cloud, &format!("/v1/files/download/{}", encode_path(path))).await?;
    if !res.status().is_success() {
        return Err(api_error(res.status().as_u16(), path));
    }
    let ct = res.headers().get("content-type").and_then(|v| v.to_str().ok()).map(str::to_string);
    if !is_text(ct.as_deref(), path) {
        return Err(format!("{path} is not a text file ({}); cloud_read returns text only", ct.unwrap_or_default()));
    }
    if res.content_length().is_some_and(|n| n > MAX_READ as u64) {
        return Err(format!("{path} is larger than 1 MiB"));
    }
    let mut body = Vec::new();
    while let Some(chunk) = res.chunk().await.map_err(|e| format!("reading {path}: {}", e.without_url()))? {
        if body.len() + chunk.len() > MAX_READ {
            return Err(format!("{path} is larger than 1 MiB"));
        }
        body.extend_from_slice(&chunk);
    }
    if body.contains(&0) {
        return Err(format!("{path} looks binary; cloud_read returns text only"));
    }
    String::from_utf8(body).map_err(|_| format!("{path} is not UTF-8 text"))
}

impl DataTurn {
    fn cloud(&self) -> Result<&CloudAccess, String> {
        self.cloud.as_ref().ok_or_else(|| "Cloud files are unavailable: the user is not signed in to AgentArea.".into())
    }

    fn listing(&self) -> Result<Listing, String> {
        let cloud = self.cloud()?;
        if let Some((at, l)) = self.listing.lock().map_err(|_| "poisoned")?.as_ref() {
            if at.elapsed() < LISTING_TTL {
                return Ok(l.clone());
            }
        }
        let l = tauri::async_runtime::block_on(fetch_listing(cloud))?;
        *self.listing.lock().map_err(|_| "poisoned")? = Some((Instant::now(), l.clone()));
        Ok(l)
    }
}

// ── MCP ──────────────────────────────────────────────────────────────────────

/// A reply for a request; `None` for notifications and client responses.
pub fn handle_message(turn: &DataTurn, msg: &Value) -> Option<Value> {
    let method = msg.get("method").and_then(Value::as_str)?;
    let id = msg.get("id")?.clone();
    let params = msg.get("params").cloned().unwrap_or_else(|| json!({}));
    let result = match method {
        "initialize" => Ok(initialize_result(&params)),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tool_defs() })),
        "tools/call" => call_tool(turn, &params),
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
        "serverInfo": { "name": "agentarea-data", "version": env!("CARGO_PKG_VERSION") },
        "instructions": "The thread's data sources. Local folders: read them with your own file tools by absolute path. Cloud (AgentArea workspace files, agentarea://files/<path>): cloud_list and cloud_read."
    })
}

fn tool_defs() -> Value {
    json!([
        {
            "name": "list_sources",
            "description": "List this thread's data sources: local folders (read them directly with your file tools, by absolute path) and the AgentArea workspace's cloud files (read with cloud_list / cloud_read).",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "cloud_list",
            "description": "List one folder of the AgentArea workspace library (cloud files): its files and subfolders. Paths are workspace paths like wiki/index.md.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "prefix": { "type": "string", "description": "Folder to list, e.g. wiki/ (empty = top level)" }
                }
            }
        },
        {
            "name": "cloud_read",
            "description": "Read a text file (up to 1 MiB) from the AgentArea workspace library. Accepts a workspace path (wiki/index.md) or an agentarea://files/… reference.",
            "inputSchema": {
                "type": "object",
                "properties": { "path": { "type": "string", "description": "e.g. wiki/index.md or agentarea://files/wiki/index.md" } },
                "required": ["path"]
            }
        }
    ])
}

fn call_tool(turn: &DataTurn, params: &Value) -> Result<Value, (i64, String)> {
    let name = params["name"].as_str().ok_or((-32602, "tool name is required".to_string()))?;
    let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
    let out = match name {
        "list_sources" => Ok(serde_json::to_string_pretty(&turn.sources).unwrap_or_default()),
        "cloud_list" => cloud_list(turn, &args),
        "cloud_read" => cloud_read(turn, &args),
        other => return Err((-32602, format!("unknown tool: {other}"))),
    };
    let (text, is_error) = match out {
        Ok(t) => (turn.redactor.redact(&t), false),
        Err(e) => (turn.redactor.redact(&e), true),
    };
    Ok(json!({ "content": [{ "type": "text", "text": text }], "isError": is_error }))
}

fn cloud_list(turn: &DataTurn, args: &Value) -> Result<String, String> {
    let folder = clean_path(args["prefix"].as_str().unwrap_or(""), true)?;
    let listing = turn.listing()?;
    let mut entries = list_level(&listing, &folder);
    if entries.is_empty() && !folder.is_empty() {
        return Err(format!("no folder {folder}/ in the workspace library (cloud_list with no prefix shows the top level)"));
    }
    let total = entries.len();
    entries.truncate(MAX_ENTRIES);
    let mut out = json!({ "folder": if folder.is_empty() { String::new() } else { format!("{folder}/") }, "entries": entries });
    if total > MAX_ENTRIES {
        out["truncated"] = json!(format!("showing {MAX_ENTRIES} of {total}"));
    }
    Ok(serde_json::to_string_pretty(&out).unwrap_or_default())
}

fn cloud_read(turn: &DataTurn, args: &Value) -> Result<String, String> {
    let path = clean_path(args["path"].as_str().unwrap_or(""), false)?;
    if args["path"].as_str().is_some_and(|p| p.trim_end().ends_with('/')) {
        return Err(format!("{path}/ is a folder; use cloud_list"));
    }
    let cloud = turn.cloud()?;
    tauri::async_runtime::block_on(fetch_text(cloud, &path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;

    fn listing() -> Listing {
        serde_json::from_value(json!({
            "files": [
                {"path":"wiki/index.md","size":12,"content_type":"text/markdown","last_modified":"x"},
                {"path":"wiki/b/page.md","size":3,"content_type":null},
                {"path":"wiki/A.md","size":1},
                {"path":"README.md","size":5},
                {"path":"wikipedia.txt","size":1}
            ],
            "directories": ["empty/", "projects/p1/", "wiki/c/", "wiki/b/"]
        }))
        .unwrap()
    }

    #[test]
    fn derives_one_level_from_the_flat_listing() {
        let l = listing();
        let top: Vec<(String, bool)> = list_level(&l, "").into_iter().map(|e| (e.path, e.dir)).collect();
        assert_eq!(
            top,
            [("empty", true), ("projects", true), ("wiki", true), ("README.md", false), ("wikipedia.txt", false)]
                .map(|(p, d)| (p.to_string(), d))
        );
        let wiki = list_level(&l, "wiki");
        assert_eq!(
            wiki,
            vec![
                Entry { name: "b".into(), path: "wiki/b".into(), dir: true, size: None },
                Entry { name: "c".into(), path: "wiki/c".into(), dir: true, size: None },
                Entry { name: "A.md".into(), path: "wiki/A.md".into(), dir: false, size: Some(1) },
                Entry { name: "index.md".into(), path: "wiki/index.md".into(), dir: false, size: Some(12) },
            ]
        );
        assert_eq!(list_level(&l, "projects").len(), 1, "empty project folder shows from directories");
        assert!(list_level(&l, "wiki/c").is_empty());
        assert!(list_level(&l, "nope").is_empty());
    }

    #[test]
    fn cleans_cloud_paths() {
        assert_eq!(clean_path("wiki/index.md", false).unwrap(), "wiki/index.md");
        assert_eq!(clean_path("agentarea://files/wiki/index.md", false).unwrap(), "wiki/index.md");
        assert_eq!(clean_path("/wiki/index.md", false).unwrap(), "wiki/index.md");
        assert_eq!(clean_path("wiki/", true).unwrap(), "wiki");
        assert_eq!(clean_path("", true).unwrap(), "");
        assert!(clean_path("", false).is_err());
        for bad in ["../x", "wiki/../../x", "wiki//x", "./x", "a\\b", "wiki/./x"] {
            assert!(clean_path(bad, false).is_err(), "{bad}");
            assert!(clean_path(bad, true).is_err(), "{bad}");
        }
        assert_eq!(encode_path("wiki/a b/é.md"), "wiki/a%20b/%C3%A9.md");
    }

    #[test]
    fn tells_text_from_binary() {
        assert!(is_text(Some("text/markdown; charset=utf-8"), "x"));
        assert!(is_text(Some("application/json"), "x"));
        assert!(is_text(Some("application/octet-stream"), "notes.md"));
        assert!(is_text(None, "data.csv"));
        assert!(!is_text(Some("application/octet-stream"), "photo.png"));
        assert!(!is_text(Some("image/png"), "x.md"), "a declared binary type wins");
        assert!(!is_text(Some("application/pdf"), "doc.pdf"));
    }

    fn local(title: &str, path: &str) -> crate::plugins::LocalRoot {
        crate::plugins::LocalRoot { title: title.into(), path: path.into() }
    }

    #[test]
    fn lists_sources() {
        let roots = [local("LLM Wiki", "/w/wiki")];
        let hints = [("LLM Wiki".to_string(), "wiki/".to_string())];
        let s = sources(&roots, Path::new("/t/thread"), Some(("Acme", &hints)));
        assert_eq!(s.iter().map(|x| (x.kind, x.location.as_str())).collect::<Vec<_>>(), [
            ("local", "/w/wiki"),
            ("local", "/t/thread"),
            ("cloud", "agentarea://files/")
        ]);
        assert_eq!(s[2].name, "AgentArea · Acme");
        assert!(s[2].description.contains("LLM Wiki keeps its cloud pages under wiki/"));
        let hint = agent_hint(&s);
        assert!(hint.contains("/w/wiki") && hint.contains("mcp__agentarea_data__cloud_read"), "{hint}");

        let offline = sources(&roots, Path::new("/t"), None);
        assert_eq!(offline.len(), 2);
        assert!(agent_hint(&offline).contains("not signed in"));
    }

    fn turn(cloud: Option<CloudAccess>) -> DataTurn {
        let roots = [local("LLM Wiki", "/w/wiki")];
        DataTurn::new(sources(&roots, Path::new("/t"), cloud.as_ref().map(|_| ("Acme", &[][..]))), cloud, Redactor::new([("TOKEN", "sk-secret-value")]))
    }

    fn call(t: &DataTurn, name: &str, args: Value) -> (String, bool) {
        let r = handle_message(t, &json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":name,"arguments":args}})).unwrap();
        (r["result"]["content"][0]["text"].as_str().unwrap().to_string(), r["result"]["isError"] == true)
    }

    #[test]
    fn dispatches_json_rpc() {
        let t = turn(None);
        let init = handle_message(&t, &json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}})).unwrap();
        assert_eq!(init["result"]["serverInfo"]["name"], "agentarea-data");
        assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
        let tools = handle_message(&t, &json!({"jsonrpc":"2.0","id":2,"method":"tools/list"})).unwrap();
        let names: Vec<_> = tools["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["list_sources", "cloud_list", "cloud_read"]);
        assert!(handle_message(&t, &json!({"jsonrpc":"2.0","method":"notifications/initialized"})).is_none());
        let unknown = handle_message(&t, &json!({"jsonrpc":"2.0","id":3,"method":"resources/list"})).unwrap();
        assert_eq!(unknown["error"]["code"], -32601);
        let bad = handle_message(&t, &json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"nope"}})).unwrap();
        assert_eq!(bad["error"]["code"], -32602);

        let (text, err) = call(&t, "list_sources", json!({}));
        assert!(!err);
        let list: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(list[0]["location"], "/w/wiki");
        assert_eq!(list.as_array().unwrap().len(), 2, "no cloud source when signed out");
        let (text, err) = call(&t, "cloud_read", json!({"path":"wiki/index.md"}));
        assert!(err && text.contains("not signed in"), "{text}");
        let (text, err) = call(&t, "cloud_read", json!({"path":"../etc/passwd"}));
        assert!(err && text.contains("bad path"), "{text}");
    }

    /// A fake AgentArea API: `/v1/files` and `/v1/files/download/…`, checking
    /// the bearer and workspace header. Answers `n` requests.
    pub(crate) fn fake_api(n: usize) -> (String, std::sync::mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for stream in listener.incoming().take(n).flatten() {
                let mut r = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                r.read_line(&mut line).unwrap();
                let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
                let mut auth = String::new();
                let mut ws = String::new();
                loop {
                    let mut h = String::new();
                    if r.read_line(&mut h).unwrap() == 0 || h.trim().is_empty() {
                        break;
                    }
                    let lower = h.to_ascii_lowercase();
                    if lower.starts_with("authorization:") {
                        auth = h[14..].trim().to_string();
                    }
                    if lower.starts_with("x-agentarea-workspace:") {
                        ws = h[22..].trim().to_string();
                    }
                }
                let _ = tx.send(format!("{path} {auth} {ws}"));
                let (status, ct, body): (u16, &str, Vec<u8>) = if auth != "Bearer at-123" {
                    (401, "application/json", b"{\"detail\":\"nope\"}".to_vec())
                } else if path == "/v1/files" {
                    (200, "application/json", serde_json::to_vec(&json!({
                        "files": [
                            {"path":"wiki/index.md","size":40,"content_type":"text/markdown"},
                            {"path":"wiki/logo.png","size":4,"content_type":"image/png"}
                        ],
                        "directories": ["wiki/"]
                    })).unwrap())
                } else if path == "/v1/files/download/wiki/index.md" {
                    (200, "text/markdown", b"# Cloud index\nkey: sk-secret-value\n".to_vec())
                } else if path == "/v1/files/download/wiki/logo.png" {
                    (200, "image/png", vec![0x89, b'P', b'N', b'G'])
                } else {
                    (404, "application/json", b"{\"detail\":\"File not found\"}".to_vec())
                };
                let mut w = stream;
                let _ = write!(w, "HTTP/1.1 {status} X\r\nContent-Type: {ct}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                let _ = w.write_all(&body);
                let mut sink = Vec::new();
                let _ = r.read_to_end(&mut sink);
            }
        });
        (base, rx)
    }

    #[test]
    fn cloud_tools_against_a_fake_api() {
        let (base, seen) = fake_api(5);
        let t = turn(Some(CloudAccess { api_base: base, token: "at-123".into(), workspace: Some("ws-1".into()) }));
        let (text, err) = call(&t, "cloud_list", json!({"prefix":"wiki/"}));
        assert!(!err, "{text}");
        assert!(text.contains("\"wiki/index.md\"") && text.contains("logo.png"), "{text}");
        assert_eq!(seen.recv().unwrap(), "/v1/files Bearer at-123 ws-1");
        let (_, err) = call(&t, "cloud_list", json!({}));
        assert!(!err, "second list is served from the cache");

        let (text, err) = call(&t, "cloud_read", json!({"path":"agentarea://files/wiki/index.md"}));
        assert!(!err, "{text}");
        assert_eq!(text, "# Cloud index\nkey: [secret:TOKEN]\n", "content is redacted");
        assert_eq!(seen.recv().unwrap(), "/v1/files/download/wiki/index.md Bearer at-123 ws-1");

        let (text, err) = call(&t, "cloud_read", json!({"path":"wiki/logo.png"}));
        assert!(err && text.contains("not a text file"), "{text}");
        let (text, err) = call(&t, "cloud_read", json!({"path":"wiki/missing.md"}));
        assert!(err && text.contains("not found"), "{text}");
        let (text, err) = call(&t, "cloud_list", json!({"prefix":"nope"}));
        assert!(err && text.contains("no folder"), "{text}");
    }

    /// A real agent CLI with run_agent's flags against `/data` and a fake
    /// AgentArea API: it lists sources, reads a cloud file through cloud_read,
    /// reads a local source file outside its cwd, and can't edit that file.
    /// Needs signed-in CLIs: `cargo test -- --ignored cli_data --nocapture`.
    fn cli_data(runner: &str) {
        let tmp = std::env::temp_dir().join(format!("aa-data-cli-{}", uuid::Uuid::new_v4().simple()));
        // Not under $TMPDIR or /tmp: Codex's workspace-write sandbox may write
        // there, and a real wiki folder lives somewhere like ~/Projects.
        let wiki = std::path::PathBuf::from(std::env::var("HOME").unwrap())
            .join(format!("Library/Caches/aa-data-cli-wiki-{}", uuid::Uuid::new_v4().simple()));
        let cwd = tmp.join("thread");
        std::fs::create_dir_all(wiki.join("wiki")).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();
        let marker = format!("local-marker-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);
        let note = wiki.join("wiki/local-note.md");
        std::fs::write(&note, format!("# Local note\n{marker}\n")).unwrap();

        let server = crate::secrets_mcp::SecretsMcp::start(crate::vault::Vault::at(tmp.join("vault"), "AgentArea Desktop Secrets (test)")).unwrap();
        let (base, seen) = fake_api(20);
        let roots = [crate::plugins::LocalRoot { title: "LLM Wiki".into(), path: wiki.display().to_string() }];
        let hints = [("LLM Wiki".to_string(), "wiki/".to_string())];
        let srcs = sources(&roots, &cwd, Some(("Acme", &hints)));
        let hint = agent_hint(&srcs);
        let cloud = CloudAccess { api_base: base, token: "at-123".into(), workspace: Some("ws-1".into()) };
        let data = server.issue_data_token(DataTurn::new(srcs, Some(cloud), Redactor::new([("TOKEN", "sk-secret-value")])));

        let mut cmd = std::process::Command::new(crate::cli_bin(runner));
        if runner == "claude" {
            let cfg = json!({ "mcpServers": { "agentarea_data": {
                "type": "http", "url": server.data_url(), "headers": { "Authorization": format!("Bearer {}", data.token) },
            }}});
            let cfg_path = tmp.join("mcp.json");
            crate::write_private(&cfg_path, &cfg.to_string()).unwrap();
            let dir = wiki.display().to_string();
            cmd.args(["-p", "--output-format", "stream-json", "--verbose"])
                .args(["--strict-mcp-config", "--setting-sources", "project"])
                .args(["--permission-mode", "acceptEdits"])
                .args(["--allowedTools", "Read", "Glob", "Grep", "Edit", "Write"])
                .arg("--mcp-config")
                .arg(&cfg_path)
                .args(["--allowedTools", "mcp__agentarea_data"])
                .arg("--add-dir")
                .arg(&dir)
                .arg("--disallowedTools")
                .arg(format!("Edit(/{dir}/**)"))
                .arg(format!("Write(/{dir}/**)"));
        } else {
            let home = tmp.join("codex-home");
            std::fs::create_dir_all(&home).unwrap();
            let auth = std::path::PathBuf::from(std::env::var("HOME").unwrap()).join(".codex/auth.json");
            std::os::unix::fs::symlink(&auth, home.join("auth.json")).unwrap();
            cmd.env("CODEX_HOME", &home)
                .env("AGENTAREA_DATA_TOKEN", &data.token)
                .args(["exec", "--json", "--skip-git-repo-check"])
                .args(["--disable", "apps", "--disable", "plugins", "--disable", "hooks"])
                .args(["-c", "sandbox_mode=\"workspace-write\""])
                .arg("-c")
                .arg(format!("mcp_servers.agentarea_data.url={}", json!(server.data_url())))
                .args(["-c", "mcp_servers.agentarea_data.bearer_token_env_var=\"AGENTAREA_DATA_TOKEN\""])
                .args(["-c", "mcp_servers.agentarea_data.default_tools_approval_mode=\"approve\""])
                .arg("-");
        }
        let mut child = cmd
            .current_dir(&cwd)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let prompt = format!(
            "List your data sources, then read the file wiki/index.md from the cloud and the file wiki/local-note.md from the local LLM Wiki folder, and quote both files in full. Finally try to append the line EDITED to the local note {} and say whether that worked.\n\n{hint}",
            note.display()
        );
        child.stdin.take().unwrap().write_all(prompt.as_bytes()).unwrap();
        let out = child.wait_with_output().unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        println!("{stdout}\n--- stderr ---\n{}", String::from_utf8_lossy(&out.stderr));
        let requests: Vec<String> = seen.try_iter().collect();
        println!("--- fake API saw ---\n{}", requests.join("\n"));
        let note_after = std::fs::read_to_string(&note).unwrap();
        drop(data);
        let _ = std::fs::remove_dir_all(&tmp);
        let _ = std::fs::remove_dir_all(&wiki);
        assert!(stdout.contains("list_sources"), "listed sources");
        assert!(stdout.contains("cloud_read"), "called cloud_read");
        assert!(requests.iter().any(|r| r.starts_with("/v1/files/download/wiki/index.md Bearer at-123 ws-1")), "API hit with the token");
        assert!(stdout.contains("Cloud index") && stdout.contains("[secret:TOKEN]"), "cloud content came back, redacted");
        assert!(!stdout.contains("sk-secret-value"), "never unredacted");
        assert!(stdout.matches(&marker).count() >= 2, "local file was read (marker in tool result and answer)");
        assert!(!note_after.contains("EDITED"), "local source stays read-only: {note_after}");
    }

    #[test]
    #[ignore]
    fn cli_data_claude() {
        cli_data("claude");
    }

    #[test]
    #[ignore]
    fn cli_data_codex() {
        cli_data("codex");
    }
}
