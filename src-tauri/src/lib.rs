use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};

mod data_mcp;
mod local_mcp;
mod mcp_sandbox;
mod plugins;
mod runners;
mod secrets;
mod secrets_mcp;
mod vault;
mod web;

/// Run a one-shot loopback HTTP server on 127.0.0.1:`port`, wait for the OAuth
/// redirect (`GET /callback?code=…&state=…`), reply with a small "you can close
/// this" page, and return the raw query string to the frontend.
///
/// The frontend invokes this (it starts listening immediately), then opens the
/// system browser at Hydra's authorize URL. When the user finishes signing in,
/// Hydra redirects here, the server captures the code, and the promise resolves.
#[tauri::command]
async fn await_oauth_redirect(port: u16) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let listener = TcpListener::bind(("127.0.0.1", port))
            .map_err(|e| format!("loopback bind on :{port} failed: {e}"))?;
        // Give up if the browser never comes back (e.g. the auth server showed an
        // error page), so the port is freed and the user can try again.
        listener
            .set_nonblocking(true)
            .map_err(|e| format!("loopback setup failed: {e}"))?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if std::time::Instant::now() > deadline {
                        return Err("Sign-in didn't come back to the app. Try again.".to_string());
                    }
                    std::thread::sleep(std::time::Duration::from_millis(200));
                }
                Err(e) => return Err(format!("loopback accept failed: {e}")),
            }
        };
        stream
            .set_nonblocking(false)
            .map_err(|e| format!("loopback setup failed: {e}"))?;

        let mut buf = [0u8; 4096];
        let n = stream
            .read(&mut buf)
            .map_err(|e| format!("loopback read failed: {e}"))?;
        let req = String::from_utf8_lossy(&buf[..n]);

        // First request line: "GET /callback?code=...&state=... HTTP/1.1"
        let target = req
            .lines()
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .unwrap_or("");
        let query = target.splitn(2, '?').nth(1).unwrap_or("").to_string();

        let body = "<!doctype html><html><body style=\"font-family:-apple-system,system-ui,sans-serif;text-align:center;padding-top:4rem;color:#0a0a0a\"><h2>AgentArea</h2><p>Signed in. You can close this tab and return to the app.</p></body></html>";
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = stream.write_all(resp.as_bytes());
        let _ = stream.flush();

        if query.is_empty() {
            Err("no query string in redirect".to_string())
        } else {
            Ok(query)
        }
    })
    .await
    .map_err(|e| format!("loopback task join error: {e}"))?
}

#[derive(Clone, Serialize)]
struct AgentLine {
    session_id: String,
    line: String,
}

#[derive(Clone, Serialize)]
struct AgentApproval {
    session_id: String,
    #[serde(flatten)]
    approval: runners::Approval,
}

#[derive(Clone, Serialize)]
struct AgentResolved {
    session_id: String,
    id: String,
}

#[derive(Clone, Serialize)]
struct AgentTurnEnd {
    session_id: String,
    error: Option<String>,
    interrupted: bool,
}

/// What the thread processes tell the UI, as Tauri events.
fn emit_agent(app: &AppHandle, session_id: &str, event: runners::Event) {
    let session_id = session_id.to_string();
    let _ = match event {
        runners::Event::Line(line) => app.emit("agent-line", AgentLine { session_id, line }),
        runners::Event::Approval(approval) => app.emit("agent-approval", AgentApproval { session_id, approval }),
        runners::Event::Resolved(id) => app.emit("agent-approval-resolved", AgentResolved { session_id, id }),
        runners::Event::TurnEnd { error, interrupted } => {
            app.emit("agent-turn-end", AgentTurnEnd { session_id, error, interrupted })
        }
    };
}

/// A GUI app on macOS doesn't inherit the shell PATH, so look where the CLI
/// installers put the binaries before falling back to PATH.
fn cli_bin(name: &str) -> PathBuf {
    if let Ok(p) = std::env::var(format!("{}_BIN", name.to_uppercase())) {
        return PathBuf::from(p);
    }
    let home = std::env::var("HOME").unwrap_or_default();
    [
        format!("{home}/.local/bin/{name}"),
        format!("/opt/homebrew/bin/{name}"),
        format!("/usr/local/bin/{name}"),
    ]
    .into_iter()
    .map(PathBuf::from)
    .find(|p| p.exists())
    .unwrap_or_else(|| PathBuf::from(name))
}

/// Model names and effort levels go into CLI args and a TOML value: keep them plain.
fn check_name(v: &str) -> Result<(), String> {
    if v.is_empty() || v.len() > 64 || !v.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c)) {
        return Err(format!("invalid model or effort: {v}"));
    }
    Ok(())
}

/// A private CODEX_HOME holding only the user's login: no user config, hooks,
/// skills or plugins, and session files stay inside the app.
fn codex_home(data_dir: &std::path::Path) -> Result<PathBuf, String> {
    let home = data_dir.join("codex-home");
    std::fs::create_dir_all(&home).map_err(|e| format!("codex home: {e}"))?;
    let auth = PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".codex/auth.json");
    let link = home.join("auth.json");
    if auth.exists() && !link.exists() {
        #[cfg(unix)]
        std::os::unix::fs::symlink(&auth, &link).map_err(|e| format!("codex auth: {e}"))?;
    }
    Ok(home)
}

#[derive(Serialize)]
struct CodexModel {
    slug: String,
    name: String,
    default_effort: Option<String>,
    efforts: Vec<String>,
}

/// Models the user's Codex account offers: `model/list` from `codex app-server`,
/// else the CLI's own cache (our private CODEX_HOME first, then ~/.codex).
/// Empty if neither works.
#[tauri::command]
async fn codex_models(app: AppHandle) -> Vec<CodexModel> {
    let data_dir = app.path().app_data_dir().ok();
    tauri::async_runtime::spawn_blocking(move || {
        let listed = data_dir
            .as_deref()
            .and_then(|d| codex_home(d).ok())
            .and_then(|home| runners::codex_model_list(&cli_bin("codex"), &home).ok())
            .unwrap_or_default();
        let models: Vec<CodexModel> = listed
            .iter()
            .filter(|m| m["hidden"] != true)
            .filter_map(|m| {
                let slug = m["model"].as_str().or(m["id"].as_str())?.to_string();
                Some(CodexModel {
                    name: m["displayName"].as_str().unwrap_or(&slug).to_string(),
                    slug,
                    default_effort: m["defaultReasoningEffort"].as_str().map(String::from),
                    efforts: m["supportedReasoningEfforts"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|e| e["reasoningEffort"].as_str().map(String::from))
                        .collect(),
                })
            })
            .collect();
        if models.is_empty() {
            cached_codex_models(data_dir.as_deref())
        } else {
            models
        }
    })
    .await
    .unwrap_or_default()
}

fn cached_codex_models(data_dir: Option<&std::path::Path>) -> Vec<CodexModel> {
    let home = PathBuf::from(std::env::var("HOME").unwrap_or_default());
    let candidates = [data_dir.map(|d| d.join("codex-home/models_cache.json")), Some(home.join(".codex/models_cache.json"))];
    let Some(json) = candidates.into_iter().flatten().find_map(|p| std::fs::read_to_string(p).ok()) else {
        return Vec::new();
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&json) else { return Vec::new() };
    v["models"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|m| m["visibility"] == "list")
        .filter_map(|m| {
            let slug = m["slug"].as_str()?.to_string();
            Some(CodexModel {
                name: m["display_name"].as_str().unwrap_or(&slug).to_string(),
                slug,
                default_effort: m["default_reasoning_level"].as_str().map(String::from),
                efforts: m["supported_reasoning_levels"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|l| l["effort"].as_str().map(String::from))
                    .collect(),
            })
        })
        .collect()
}

/// Accept only a UUID's charset: ids end up in paths and CLI arguments.
fn check_id(id: &str) -> Result<(), String> {
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_hexdigit() || c == '-') {
        return Err(format!("invalid id: {id}"));
    }
    Ok(())
}

/// What a turn asks of the thread's process (see `run_agent`).
struct TurnArgs {
    runner: String,
    session_id: String,
    prompt: String,
    resume_id: Option<String>,
    mcp_url: Option<String>,
    token: Option<String>,
    cwd: Option<String>,
    model: Option<String>,
    effort: Option<String>,
    workspace: Option<String>,
    workspace_name: Option<String>,
    api_base: Option<String>,
    tools_key: Option<String>,
}

/// Run one turn of a thread through its local agent CLI — `claude` or `codex` —
/// kept running between turns (runners.rs). The turn's output arrives as
/// `agent-line` events, approvals as `agent-approval`, its end as `agent-turn-end`.
///
/// Either way the thread is isolated from the user's own CLI setup (no user
/// MCP servers, settings, hooks or plugins), and its only extra tools come from
/// `mcp_url`: the thread's AgentArea client endpoint, which serves both its MCP
/// servers and its skills. The CLIs reach it through the local proxy
/// (secrets_mcp.rs), which adds the app's live token, so a process can outlive
/// the token it started with.
///
/// `resume_id` continues a CLI session when the process has to be (re)started:
/// for claude it is our session id (the first start passes it as
/// `--session-id`), for codex the thread id it reported. A running process is
/// reused while what it was started with (runner, folder, MCP set, data
/// sources, vault, Claude's model) stays the same; otherwise it is restarted.
///
/// Vault secrets: known values are swapped for `[secret:NAME]` in the prompt
/// before the CLI gets it and in every line it prints, and when the vault has
/// any the CLI also gets the local `agentarea_secrets` MCP server, where it can
/// use a secret only through a one-time handle (secrets_mcp.rs). The honest
/// boundary of v1: what the CLI's own tools read (Read on a file holding a
/// secret) goes to the model without passing us, and there is deliberately no
/// "run a command with the secret in env" (echo/base64 would leak it).
///
/// Data sources: every thread also gets the local `agentarea_data` MCP server
/// (data_mcp.rs). Local sources (plugin folders, the thread folder) are read by
/// the CLI's own file tools — Claude gets each plugin folder as `--add-dir`,
/// read-only. Cloud files (the workspace library) go through its tools, which
/// use the live token + `api_base` + `workspace`; those stay in the process's memory.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
async fn run_agent(
    app: AppHandle,
    runner: String,
    session_id: String,
    prompt: String,
    resume_id: Option<String>,
    mcp_url: Option<String>,
    token: Option<String>,
    cwd: Option<String>,
    model: Option<String>,
    effort: Option<String>,
    workspace: Option<String>,
    workspace_name: Option<String>,
    api_base: Option<String>,
    tools_key: Option<String>,
) -> Result<(), String> {
    let args = TurnArgs {
        runner,
        session_id,
        prompt,
        resume_id,
        mcp_url,
        token,
        cwd,
        model,
        effort,
        workspace,
        workspace_name,
        api_base,
        tools_key,
    };
    tauri::async_runtime::spawn_blocking(move || start_turn(&app, args))
        .await
        .map_err(|e| format!("turn task: {e}"))?
}

fn start_turn(app: &AppHandle, a: TurnArgs) -> Result<(), String> {
    let session_id = a.session_id.as_str();
    check_id(session_id)?;
    // The workspace goes into a header, the base into URLs: keep both plain.
    if let Some(w) = &a.workspace {
        if w.is_empty() || w.len() > 128 || !w.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c)) {
            return Err(format!("invalid workspace: {w}"));
        }
    }
    if let Some(b) = &a.api_base {
        if !(b.starts_with("https://") || b.starts_with("http://")) || b.chars().any(|c| c.is_whitespace()) {
            return Err(format!("invalid API base: {b}"));
        }
    }
    for v in a.model.iter().chain(a.effort.iter()) {
        check_name(v)?;
    }
    if let Some(id) = &a.resume_id {
        check_id(id)?;
    }
    let kind = match a.runner.as_str() {
        "claude" => runners::Kind::Claude,
        "codex" => runners::Kind::Codex,
        other => return Err(format!("unknown runner: {other}")),
    };
    let runners = app.state::<runners::Runners>().inner().clone();
    let data_dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    // The folder the agent works in: the one picked for the thread, or its own.
    let cwd = match &a.cwd {
        Some(dir) => PathBuf::from(dir),
        None => default_session_dir(session_id)?,
    };
    if !cwd.is_dir() {
        return Err(format!("folder not found: {}", cwd.display()));
    }

    // Values are read every turn; a secret the keychain won't give us can't
    // be redacted, so that fails the turn rather than risking a leak.
    let secrets = vault::Vault::user()?.values()?;
    let redactor = vault::Redactor::new(secrets.iter().map(|(m, v)| (m.name.as_str(), v.as_str())));
    let server = app.try_state::<secrets_mcp::SecretsMcp>().map(|s| s.inner().clone());

    // Data sources: what the agent may read, locally and in the workspace.
    let signed_in = a.token.is_some() && a.api_base.is_some();
    let local_roots = plugins::local_roots();
    let cloud_hints = plugins::cloud_roots();
    let workspace_label = a.workspace_name.clone().filter(|n| !n.trim().is_empty()).unwrap_or_else(|| "workspace".into());
    let sources = data_mcp::sources(
        &local_roots,
        &cwd,
        signed_in.then_some((workspace_label.as_str(), cloud_hints.as_slice())),
    );
    // Plugin folders outside the thread's folder get granted to Claude read-only.
    let extra_dirs: Vec<String> = local_roots
        .iter()
        .map(|r| r.path.clone())
        .filter(|r| {
            let r = PathBuf::from(r);
            !cwd.starts_with(&r) && !r.starts_with(&cwd)
        })
        .collect();

    // Everything the process is started with; when any of it changes, the
    // next turn restarts it. Codex takes model/effort per turn, so only
    // "back to default" needs a restart there.
    let signature = {
        let mut h = DefaultHasher::new();
        (&a.runner, &cwd, &a.mcp_url, &a.tools_key, &a.api_base, &a.workspace, signed_in, &extra_dirs).hash(&mut h);
        serde_json::to_string(&sources).unwrap_or_default().hash(&mut h);
        secrets.iter().for_each(|(m, v)| (&m.name, v).hash(&mut h));
        server.is_some().hash(&mut h);
        match kind {
            runners::Kind::Claude => (&a.model, &a.effort).hash(&mut h),
            runners::Kind::Codex => (a.model.is_none(), a.effort.is_none()).hash(&mut h),
        }
        h.finish()
    };

    let proc = match runners.reusable(session_id, signature)? {
        Some(p) => p,
        None => {
            let (cmd, guards) = agent_command(kind, &a, &data_dir, &server, &secrets, &redactor, &sources, &extra_dirs)?;
            let codex = (kind == runners::Kind::Codex).then(|| runners::CodexThread {
                resume_id: a.resume_id.clone(),
                cwd: cwd.clone(),
                model: a.model.clone(),
            });
            runners.spawn(kind, session_id, signature, cmd, &cwd, redactor.clone(), guards, codex)?
        }
    };

    // Hints about the vault and data sources go with a process's first turn.
    let mut prompt = if secrets.is_empty() { a.prompt.clone() } else { redactor.redact(&a.prompt) };
    if runners.is_fresh(&proc) {
        if !secrets.is_empty() {
            let names: Vec<String> = secrets.iter().map(|(m, _)| m.name.clone()).collect();
            prompt = format!("{prompt}\n\n{}", vault::agent_hint(&names, server.is_some()));
        }
        if server.is_some() {
            prompt = format!("{prompt}\n\n{}", data_mcp::agent_hint(&sources));
        }
    }
    runners.turn(&proc, &prompt, a.model.as_deref(), a.effort.as_deref())
}

/// The CLI command for a thread's process, and what must live as long as it
/// (bearer tokens, the proxy route, the MCP config file).
#[allow(clippy::too_many_arguments)]
fn agent_command(
    kind: runners::Kind,
    a: &TurnArgs,
    data_dir: &std::path::Path,
    server: &Option<secrets_mcp::SecretsMcp>,
    secrets: &[(vault::SecretMeta, String)],
    redactor: &vault::Redactor,
    sources: &[data_mcp::Source],
    extra_dirs: &[String],
) -> Result<(Command, runners::Guards), String> {
    let session_id = a.session_id.as_str();
    let mut guards = runners::Guards { keep: Vec::new(), files: Vec::new() };
    // Cloud files need a signed-in user; the token stays in the process's memory.
    let cloud = a.token.clone().zip(a.api_base.clone()).map(|(token, api_base)| data_mcp::CloudAccess {
        api_base,
        token,
        workspace: a.workspace.clone(),
    });
    let secrets_turn = if secrets.is_empty() { None } else { server.as_ref().map(|s| (s.url(), s.issue_token(session_id))) };
    let data_turn = server
        .as_ref()
        .map(|s| (s.data_url(), s.issue_data_token(data_mcp::DataTurn::new(sources.to_vec(), cloud, redactor.clone()))));
    // The AgentArea client endpoint: through the local proxy (live token) when
    // it runs, else directly with the token this turn got.
    let agentarea: Option<(String, Option<String>)> = match (a.mcp_url.clone(), a.token.clone()) {
        (Some(url), Some(token)) => Some(match (server, &a.api_base) {
            (Some(s), Some(base)) => {
                let route = s.register_proxy(&url, base, &token);
                let local = route.url.clone();
                guards.keep.push(Box::new(route));
                (local, None)
            }
            _ => (url, Some(token)),
        }),
        _ => None,
    };
    let cmd = match kind {
        runners::Kind::Claude => {
            let mut cmd = Command::new(cli_bin("claude"));
            // One process per thread: user turns come in on stdin as stream-json,
            // permission prompts go to us over the same pipe (`stdio`).
            cmd.args(["-p", "--input-format", "stream-json", "--output-format", "stream-json", "--verbose"])
                .arg("--include-partial-messages")
                .args(["--permission-prompt-tool", "stdio"])
                // Isolated from the user's own setup: no user MCP servers or settings.
                .args(["--strict-mcp-config", "--setting-sources", "project"])
                // Like Codex's "Auto": reads run anywhere, edits in the thread's
                // folder are accepted, and Bash runs without asking inside the OS
                // sandbox (writes confined to the folder). Leaving the sandbox,
                // edits elsewhere and web access ask the user.
                .args(["--permission-mode", "acceptEdits"])
                .args(["--settings", r#"{"sandbox":{"enabled":true,"autoAllowBashIfSandboxed":true}}"#])
                .args(["--allowedTools", "Read", "Glob", "Grep"]);
            match &a.resume_id {
                Some(id) => cmd.args(["--resume", id]),
                None => cmd.args(["--session-id", session_id]),
            };
            if let Some(m) = &a.model {
                cmd.args(["--model", m]);
            }
            if let Some(e) = &a.effort {
                cmd.args(["--effort", e]);
            }
            let mut servers = serde_json::Map::new();
            if let Some((url, bearer)) = &agentarea {
                let mut s = serde_json::json!({ "type": "http", "url": url });
                if let Some(token) = bearer {
                    s["headers"] = serde_json::json!({ "Authorization": format!("Bearer {token}") });
                }
                servers.insert("agentarea".into(), s);
            }
            if let Some((url, turn)) = &secrets_turn {
                servers.insert("agentarea_secrets".into(), serde_json::json!({
                    "type": "http", "url": url,
                    "headers": { "Authorization": format!("Bearer {}", turn.token) },
                }));
            }
            if let Some((url, turn)) = &data_turn {
                servers.insert("agentarea_data".into(), serde_json::json!({
                    "type": "http", "url": url,
                    "headers": { "Authorization": format!("Bearer {}", turn.token) },
                }));
            }
            // Local data sources: readable, never edited (`//` = absolute path
            // in a permission rule). The rules are space-separated, so a folder
            // whose path has spaces or commas isn't added rather than left writable.
            for dir in extra_dirs.iter().filter(|d| !d.contains([' ', ','])) {
                cmd.arg("--add-dir").arg(dir);
                let dir = dir.trim_end_matches('/');
                cmd.arg("--disallowedTools").arg(format!("Edit(/{dir}/**)")).arg(format!("Write(/{dir}/**)"));
            }
            if !servers.is_empty() {
                // Claude reads MCP headers only from a config file; it carries
                // bearer tokens, so it is 0600 in the app's own state and removed
                // when the process ends (argv would show them in `ps`).
                let state_dir = data_dir.join("sessions").join(session_id);
                std::fs::create_dir_all(&state_dir).map_err(|e| format!("session dir: {e}"))?;
                let path = state_dir.join("mcp.json");
                let cfg = serde_json::json!({ "mcpServers": servers });
                write_private(&path, &cfg.to_string()).map_err(|e| format!("mcp config: {e}"))?;
                cmd.arg("--mcp-config").arg(&path);
                for name in servers.keys() {
                    cmd.args(["--allowedTools", &format!("mcp__{name}")]);
                }
                guards.files.push(path);
            }
            cmd
        }
        runners::Kind::Codex => {
            let home = codex_home(data_dir)?;
            let mut cmd = Command::new(cli_bin("codex"));
            cmd.env("CODEX_HOME", &home)
                .arg("app-server")
                .args(["--disable", "apps", "--disable", "plugins", "--disable", "hooks"])
                // May write inside the thread's folder only.
                .args(["-c", "sandbox_mode=\"workspace-write\""]);
            if let Some((url, bearer)) = &agentarea {
                cmd.arg("-c")
                    .arg(format!("mcp_servers.agentarea.url={}", serde_json::json!(url)))
                    // Governed on the AgentArea side (policies, approvals, audit).
                    .args(["-c", "mcp_servers.agentarea.default_tools_approval_mode=\"approve\""]);
                if let Some(token) = bearer {
                    cmd.env("AGENTAREA_TOKEN", token)
                        .args(["-c", "mcp_servers.agentarea.bearer_token_env_var=\"AGENTAREA_TOKEN\""]);
                }
            }
            if let Some((url, turn)) = &data_turn {
                // Local sources need no grant: the workspace-write sandbox reads
                // anywhere and writes only in the thread's folder.
                cmd.env("AGENTAREA_DATA_TOKEN", &turn.token)
                    .arg("-c")
                    .arg(format!("mcp_servers.agentarea_data.url={}", serde_json::json!(url)))
                    .args(["-c", "mcp_servers.agentarea_data.bearer_token_env_var=\"AGENTAREA_DATA_TOKEN\""])
                    // Read-only tools over the user's own data.
                    .args(["-c", "mcp_servers.agentarea_data.default_tools_approval_mode=\"approve\""]);
            }
            if let Some((url, turn)) = &secrets_turn {
                cmd.env("AGENTAREA_SECRETS_TOKEN", &turn.token)
                    .arg("-c")
                    .arg(format!("mcp_servers.agentarea_secrets.url={}", serde_json::json!(url)))
                    .args(["-c", "mcp_servers.agentarea_secrets.bearer_token_env_var=\"AGENTAREA_SECRETS_TOKEN\""])
                    // These tools gate themselves (handles, host allowlist).
                    .args(["-c", "mcp_servers.agentarea_secrets.default_tools_approval_mode=\"approve\""]);
            }
            cmd
        }
    };
    if let Some((_, t)) = secrets_turn {
        guards.keep.push(Box::new(t));
    }
    if let Some((_, t)) = data_turn {
        guards.keep.push(Box::new(t));
    }
    Ok((cmd, guards))
}

/// Answer an approval the thread's agent is waiting on: "allow",
/// "allow_session" or "deny".
#[tauri::command]
async fn agent_approve(app: AppHandle, session_id: String, id: String, decision: String) -> Result<(), String> {
    let decision = runners::Decision::parse(&decision)?;
    let runners = app.state::<runners::Runners>().inner().clone();
    tauri::async_runtime::spawn_blocking(move || runners.answer(&session_id, &id, decision))
        .await
        .map_err(|e| e.to_string())?
}

/// Stop the thread's process for good (the thread was deleted).
#[tauri::command]
fn agent_close(app: AppHandle, session_id: String) {
    app.state::<runners::Runners>().close(&session_id);
}

/// Stop the thread's running turn (Stop in the composer).
#[tauri::command]
async fn agent_interrupt(app: AppHandle, session_id: String) -> Result<(), String> {
    let runners = app.state::<runners::Runners>().inner().clone();
    tauri::async_runtime::spawn_blocking(move || runners.interrupt(&session_id))
        .await
        .map_err(|e| e.to_string())?
}

/// `~/AgentArea/<first 8 of the session id>`: a thread's own folder when none
/// was picked. Visible on purpose — dropped files and agent output live here.
fn default_session_dir(session_id: &str) -> Result<PathBuf, String> {
    let home = std::env::var("HOME").map_err(|_| "HOME not set".to_string())?;
    let dir = PathBuf::from(home).join("AgentArea").join(&session_id[..session_id.len().min(8)]);
    std::fs::create_dir_all(&dir).map_err(|e| format!("session folder: {e}"))?;
    Ok(dir)
}

/// The thread's local data sources, as the next turn will see them (the
/// composer's data chip). Cloud is added by the frontend, which knows the
/// sign-in; nothing is created on disk.
#[tauri::command]
fn data_sources(session_id: String, cwd: Option<String>) -> Result<Vec<data_mcp::Source>, String> {
    check_id(&session_id)?;
    let cwd = match cwd {
        Some(dir) => PathBuf::from(dir),
        None => {
            let home = std::env::var("HOME").map_err(|_| "HOME not set".to_string())?;
            PathBuf::from(home).join("AgentArea").join(&session_id[..session_id.len().min(8)])
        }
    };
    Ok(data_mcp::sources(&plugins::local_roots(), &cwd, None))
}

/// The thread's own folder (created on demand), for showing it before a run.
#[tauri::command]
fn session_dir(session_id: String) -> Result<String, String> {
    check_id(&session_id)?;
    Ok(default_session_dir(&session_id)?.display().to_string())
}

/// Copy dropped files/folders into `dest`; returns the copied paths. Existing
/// names get a numeric suffix instead of being overwritten.
#[tauri::command]
fn import_files(paths: Vec<String>, dest: String) -> Result<Vec<String>, String> {
    let dest = PathBuf::from(dest);
    std::fs::create_dir_all(&dest).map_err(|e| e.to_string())?;
    paths
        .iter()
        .map(|p| {
            let src = PathBuf::from(p);
            let name = src.file_name().ok_or_else(|| format!("bad path: {p}"))?;
            let mut target = dest.join(name);
            let mut n = 1;
            while target.exists() {
                let stem = src.file_stem().unwrap_or(name).to_string_lossy();
                let ext = src.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
                target = dest.join(format!("{stem}-{n}{ext}"));
                n += 1;
            }
            copy_recursive(&src, &target).map_err(|e| format!("copy {p}: {e}"))?;
            Ok(target.display().to_string())
        })
        .collect()
}

fn copy_recursive(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<()> {
    if src.is_dir() {
        std::fs::create_dir_all(dst)?;
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            copy_recursive(&entry.path(), &dst.join(entry.file_name()))?;
        }
        Ok(())
    } else {
        std::fs::copy(src, dst).map(|_| ())
    }
}

fn write_private(path: &std::path::Path, contents: &str) -> std::io::Result<()> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    opts.open(path)?.write_all(contents.as_bytes())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_http::init())
        .plugin(tauri_plugin_dialog::init())
        .manage(local_mcp::LocalMcp::default())
        // The vault's MCP server lives as long as the app; without it agents
        // just don't get secret tools.
        .setup(|app| {
            let handle = app.handle().clone();
            app.manage(runners::Runners::new(Arc::new(move |session_id, event| emit_agent(&handle, session_id, event))));
            match vault::Vault::user().and_then(secrets_mcp::SecretsMcp::start) {
                Ok(server) => {
                    app.manage(server);
                }
                Err(e) => eprintln!("secrets MCP server not started: {e}"),
            }
            Ok(())
        })
        // Separate origins for untrusted web content: MCP Apps and local plugins.
        .register_uri_scheme_protocol(mcp_sandbox::SCHEME, mcp_sandbox::handle)
        .register_asynchronous_uri_scheme_protocol(plugins::SCHEME, plugins::handle)
        .invoke_handler(tauri::generate_handler![
            secrets::token_load,
            data_mcp::set_cloud_token,
            secrets::token_save,
            secrets::token_clear,
            vault::vault_list,
            vault::vault_create,
            vault::vault_update,
            vault::vault_delete,
            await_oauth_redirect,
            run_agent,
            agent_approve,
            agent_interrupt,
            agent_close,
            session_dir,
            data_sources,
            import_files,
            codex_models,
            plugins::list_plugins,
            plugins::set_plugin_enabled,
            plugins::plugin_set_root,
            plugins::plugin_set_cloud_root,
            plugins::plugin_fs_list,
            plugins::plugin_fs_read,
            local_mcp::local_mcp_list,
            local_mcp::local_mcp_install,
            local_mcp::local_mcp_uninstall,
            local_mcp::local_mcp_start,
            local_mcp::local_mcp_stop,
            local_mcp::local_mcp_request,
            local_mcp::http_get_json
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app, event| {
            // Local MCP servers and agent processes are our children; they go
            // when the app goes.
            if let tauri::RunEvent::Exit = event {
                app.state::<local_mcp::LocalMcp>().stop_all();
                app.state::<runners::Runners>().stop_all();
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use runners::{Decision, Event, Kind, Runners};

    fn runners() -> (Runners, mpsc::Receiver<Event>) {
        let (tx, rx) = mpsc::channel();
        let tx = std::sync::Mutex::new(tx);
        (Runners::new(Arc::new(move |_, e| drop(tx.lock().unwrap().send(e)))), rx)
    }

    fn args(runner: &str, session_id: &str, model: &str, effort: Option<&str>) -> TurnArgs {
        TurnArgs {
            runner: runner.into(),
            session_id: session_id.into(),
            prompt: String::new(),
            resume_id: None,
            mcp_url: None,
            token: None,
            cwd: None,
            model: Some(model.into()),
            effort: effort.map(String::from),
            workspace: None,
            workspace_name: None,
            api_base: None,
            tools_key: None,
        }
    }

    fn start(r: &Runners, kind: Kind, a: &TurnArgs, data_dir: &std::path::Path, cwd: &std::path::Path) -> Arc<runners::Proc> {
        let (cmd, guards) = agent_command(kind, a, data_dir, &None, &[], &vault::Redactor::default(), &[], &[]).unwrap();
        let codex = (kind == Kind::Codex).then(|| runners::CodexThread {
            resume_id: a.resume_id.clone(),
            cwd: cwd.to_path_buf(),
            model: a.model.clone(),
        });
        r.spawn(kind, &a.session_id, 1, cmd, cwd, vault::Redactor::default(), guards, codex).unwrap()
    }

    /// Events until `done` says stop (printed as they come); panics on timeout.
    fn until(rx: &mpsc::Receiver<Event>, secs: u64, mut done: impl FnMut(&Event) -> bool) -> Vec<Event> {
        let deadline = Instant::now() + Duration::from_secs(secs);
        let mut seen = Vec::new();
        loop {
            let left = deadline.checked_duration_since(Instant::now()).expect("timed out waiting for the agent");
            let e = rx.recv_timeout(left).expect("timed out waiting for the agent");
            match &e {
                Event::Line(l) => println!("  line {}", &l[..l.len().min(160)]),
                other => println!("  {other:?}"),
            }
            let stop = done(&e);
            seen.push(e);
            if stop {
                return seen;
            }
        }
    }

    fn turn_ended(e: &Event) -> bool {
        matches!(e, Event::TurnEnd { .. })
    }

    /// Text the agent streamed (Claude text deltas / Codex agentMessage deltas).
    fn deltas(events: &[Event]) -> Vec<String> {
        events
            .iter()
            .filter_map(|e| match e {
                Event::Line(l) => serde_json::from_str::<serde_json::Value>(l).ok(),
                _ => None,
            })
            .filter_map(|v| {
                v["params"]["delta"].as_str().or(v["event"]["delta"]["text"].as_str()).map(String::from)
            })
            .collect()
    }

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("aa-{name}-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// `codex app-server` for real: a command in the folder runs without asking,
    /// one outside asks and is denied;
    /// text streams; Stop interrupts a long turn; a new process resumes the
    /// thread and remembers it. `cargo test -- --ignored e2e_codex --nocapture`.
    #[test]
    #[ignore]
    fn e2e_codex_app_server() {
        let (data_dir, cwd) = (temp("codex-data"), temp("codex-cwd"));
        std::fs::write(cwd.join("hello.txt"), "hi").unwrap();
        let (r, rx) = runners();
        let session = uuid::Uuid::new_v4().to_string();
        let mut a = args("codex", &session, "gpt-6-luna", Some("low"));
        let p = start(&r, Kind::Codex, &a, &data_dir, &cwd);

        println!("turn 1: a command inside the folder runs in the sandbox, no asking");
        r.turn(&p, "Run the shell command `touch made.txt && ls` and tell me which files are there. Be brief.", a.model.as_deref(), a.effort.as_deref()).unwrap();
        let asked = until(&rx, 180, turn_ended);
        assert!(!asked.iter().any(|e| matches!(e, Event::Approval(_))), "sandboxed command asked");
        assert!(matches!(asked.last(), Some(Event::TurnEnd { error: None, interrupted: false })));
        assert!(cwd.join("made.txt").exists(), "the command ran");
        let text = deltas(&asked).concat();
        println!("streamed: {text:?}");
        assert!(text.contains("made.txt"), "{text}");

        println!("turn 1b: writing outside the folder asks, and is denied");
        // Not the temp dir: Codex's workspace-write sandbox may write there.
        let outside = PathBuf::from(std::env::var("HOME").unwrap()).join(format!("aa-escape-{}.txt", uuid::Uuid::new_v4().simple()));
        r.turn(&p, &format!("Run the shell command `touch {}`. If the sandbox blocks it, request escalated permissions and retry. Be brief.", outside.display()), None, None).unwrap();
        let events = until(&rx, 180, |e| matches!(e, Event::Approval(_)) || turn_ended(e));
        let Some(Event::Approval(ap)) = events.last() else { panic!("no approval asked: {:?}", events.last()) };
        println!("asked: {ap:?}");
        r.answer(&session, &ap.id, Decision::Deny).unwrap();
        until(&rx, 180, turn_ended);
        assert!(!outside.exists(), "denied write happened");

        println!("turn 2: Stop mid-stream");
        r.turn(&p, "Write a 600-word essay about rivers.", None, None).unwrap();
        until(&rx, 120, |e| !deltas(std::slice::from_ref(e)).is_empty());
        r.interrupt(&session).unwrap();
        let events = until(&rx, 30, turn_ended);
        assert!(matches!(events.last(), Some(Event::TurnEnd { error: None, interrupted: true })), "{:?}", events.last());

        println!("turn 3: a new process resumes the thread");
        let thread_id = events_thread_id(&asked).expect("thread/started carried the thread id");
        r.stop_all();
        std::thread::sleep(Duration::from_millis(500));
        a.resume_id = Some(thread_id);
        let p = start(&r, Kind::Codex, &a, &data_dir, &cwd);
        r.turn(&p, "Which file did you create with touch earlier? Answer with the file name only.", None, None).unwrap();
        let events = until(&rx, 180, turn_ended);
        let text = deltas(&events).concat();
        println!("streamed: {text:?}");
        assert!(text.contains("made.txt"), "remembered across processes: {text}");
        r.stop_all();
        let _ = std::fs::remove_dir_all(&data_dir);
        let _ = std::fs::remove_dir_all(&cwd);
    }

    fn events_thread_id(events: &[Event]) -> Option<String> {
        events.iter().find_map(|e| match e {
            Event::Line(l) => serde_json::from_str::<serde_json::Value>(l).ok().and_then(|v| {
                (v["method"] == "thread/started").then(|| v["params"]["thread"]["id"].as_str().map(String::from)).flatten()
            }),
            _ => None,
        })
    }

    /// Claude Code as one process over several turns: the second turn remembers
    /// the first, sandboxed Bash runs without asking, an escape asks and is denied, Stop interrupts a long answer
    /// and the process keeps serving. `cargo test -- --ignored e2e_claude --nocapture`.
    #[test]
    #[ignore]
    fn e2e_claude_persistent() {
        let (data_dir, cwd) = (temp("claude-data"), temp("claude-cwd"));
        let (r, rx) = runners();
        let session = uuid::Uuid::new_v4().to_string();
        let a = args("claude", &session, "haiku", None);
        let p = start(&r, Kind::Claude, &a, &data_dir, &cwd);

        println!("turn 1");
        r.turn(&p, "Remember the number 4817. Reply with just OK.", None, None).unwrap();
        let events = until(&rx, 120, turn_ended);
        assert!(matches!(events.last(), Some(Event::TurnEnd { error: None, .. })));

        println!("turn 2: memory + a sandboxed Bash call that runs without asking");
        r.turn(&p, "What number did I ask you to remember? Then use the Bash tool to run exactly: python3 -c 'print(6*7)'", None, None).unwrap();
        let events = until(&rx, 120, turn_ended);
        assert!(!events.iter().any(|e| matches!(e, Event::Approval(_))), "sandboxed Bash asked");
        let text = deltas(&events).concat();
        println!("streamed: {text:?}");
        assert!(text.contains("4817"), "same process remembers turn 1: {text}");
        assert!(text.contains("42"), "{text}");

        println!("turn 2b: writing outside the folder asks, and is denied");
        let outside = PathBuf::from(std::env::var("HOME").unwrap()).join(format!("aa-escape-{}.txt", uuid::Uuid::new_v4().simple()));
        r.turn(&p, &format!("Use the Bash tool to run exactly: touch {}  If the sandbox blocks it, retry with the sandbox disabled.", outside.display()), None, None).unwrap();
        let asked = until(&rx, 120, |e| matches!(e, Event::Approval(_)) || turn_ended(e));
        let Some(Event::Approval(ap)) = asked.last() else { panic!("no approval asked: {:?}", asked.last()) };
        println!("asked: {ap:?}");
        assert_eq!(ap.kind, "shell");
        r.answer(&session, &ap.id, Decision::Deny).unwrap();
        let events = until(&rx, 120, turn_ended);
        let result = events.iter().find_map(|e| match e {
            Event::Line(l) if l.contains("The user denied this action.") => Some(l.clone()),
            _ => None,
        });
        assert!(result.is_some(), "the denial reached Claude");
        assert!(!outside.exists(), "denied write happened");

        println!("turn 3: Stop mid-stream");
        r.turn(&p, "Write a 600-word story about a cat.", None, None).unwrap();
        until(&rx, 120, |e| !deltas(std::slice::from_ref(e)).is_empty());
        r.interrupt(&session).unwrap();
        let events = until(&rx, 30, turn_ended);
        assert!(matches!(events.last(), Some(Event::TurnEnd { error: None, interrupted: true })), "{:?}", events.last());

        println!("turn 4: still the same process");
        r.turn(&p, "Say just: still alive", None, None).unwrap();
        let events = until(&rx, 120, turn_ended);
        assert!(deltas(&events).concat().to_lowercase().contains("still alive"));
        r.stop_all();
        let _ = std::fs::remove_dir_all(&data_dir);
        let _ = std::fs::remove_dir_all(&cwd);
    }
}
