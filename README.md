# AgentArea Desktop

Thin desktop client for AgentArea — a governed agent endpoint that employees run
on their machines. Built on **Tauri 2 (Rust core) + React + Tailwind v4**.

The visual language is intentionally close to [Orca](https://github.com/stablyai/orca)
(MIT) — same Tailwind v4 + shadcn "new-york" neutral theme and Geist font — because
we liked their design. We did **not** fork Orca: it's an Electron coding tool welded
to git/worktrees, whereas we need a light, governed, non-coding client. We borrow the
look (and may later borrow specific modules like the terminal view) rather than the shell.

## What it is (for now)

A Codex-style thread app: threads in the sidebar, a composer with a "where it
runs" picker under it. A thread runs either

- **locally** — Claude Code or Codex, one long-lived process per thread
  (`src-tauri/src/runners.rs`), or
- **in the cloud** — delegated to an AgentArea agent (`src/lib/cloud.ts`): the
  first message creates a task, follow-ups are `queue_message` commands to it.

For local threads you pick which **MCP servers** and **skills** from AgentArea the
agent gets. The pick is stored on the control plane as an AgentArea **client**
(one per thread, created on the first pick), which serves its MCP servers *and*
skills through one endpoint, `{API}/client-mcp/{client_id}` — the only MCP server
the local CLI sees. Either CLI runs isolated from the user's own setup: Claude
with user settings/hooks ignored and a strict MCP config; Codex with a private
`CODEX_HOME` (only `auth.json` linked in) and apps/plugins/hooks disabled.
Cloud agents use their own tools.

**Local runners.** Each local thread keeps its CLI process alive between turns,
speaking the CLI's own protocol over stdio:

- **Codex** — `codex app-server` (JSON-RPC: `thread/start|resume`,
  `turn/start`, `turn/interrupt`, approval requests as server requests).
- **Claude Code** — `claude -p --input-format stream-json`, one user message
  per turn on stdin; permission prompts come back over the same pipe
  (`--permission-prompt-tool stdio`), Stop is an `interrupt` control request.

Text streams as it is generated, and a running turn can be stopped. The
approval policy matches Codex's default "Auto": reads, edits inside the
thread's folder and shell commands inside the OS sandbox (writes confined to
the folder, no network) run without asking; stepping outside the sandbox,
edits elsewhere and other tools ask. Asks show up as Allow / Deny cards in the
thread, its grid tile and the unified inbox. The AgentArea MCP servers are
pre-approved — they are governed on the platform side. A process that dies is
restarted (resuming its session) on the next turn; idle ones are reaped after
30 minutes; all of them exit with the app.

The AgentArea MCP endpoint reaches the CLI through a local proxy
(`/proxy/agentarea/<secret>` on the app's loopback server) that adds the
app's current access token to every request, so hours-long processes never
hold a token that expires; the CLI only sees a per-thread secret.

**Workspaces and inbox.** A Slack-style rail switches between AgentArea
workspaces (orgs). The inbox merges the workspace's `/v1/inbox` (approvals,
escalations, questions from cloud agents) with local threads that need you:
pending approvals and replies you haven't read.

A thread keeps its runner once the first message is sent (the next turn resumes
that runner's own session). Threads live in `localStorage` for now.

**Views.** Focus (one thread) or a grid of 2–4 panes (⌘1–⌘4 sets the count;
⌘[ / ⌘] steps between threads, ⌘Enter zooms a pane, ⌘T opens a new thread,
⌃1–⌃9 switches workspace).
Each local thread works in a folder: a picked one, or its own `~/AgentArea/<id>`;
files dropped on a thread are copied there (or read in place when a folder is
picked). Replies render as markdown; local file links become chips that open
the file (⌘-click reveals it in Finder); file edits show as "Created / Edited
file +N −M" rows with an inline diff; other tool calls expand to show input and
result.

**Apps.** MCP Apps in two explicit sections, both rendered like the web app's
Apps page (`AppBridge` in a sandbox served from the `aa-sandbox://` scheme, a
separate origin from the main window):

- **On this device** — MCP servers the desktop runs itself as stdio children
  (`src-tauri/src/local_mcp.rs`); no sign-in needed. What is installed is data in
  `~/AgentArea/mcp/servers.json` (seeded with two examples: system monitor and
  budget allocator). Installed servers start when the panel opens; tools whose
  `_meta.ui.resourceUri` is a `ui://` resource are apps, other servers show
  "No UI". **Add app** installs from a bundled Recommended list, the public MCP
  registry (npm → `npx -y`, PyPI → `uvx`; remote-only servers aren't
  installable yet), your own `/v0/servers` registry, or a manual command.
- **AgentArea · workspace** — the workspace's apps (`/v1/mcp-apps/`); shown
  once connected.

**Plugins.** Folders in `~/AgentArea/plugins/<id>/` with `plugin.json` + an HTML
entry, served from `aa-plugin://` into a sandboxed iframe. A postMessage bridge
gives them `threads.list` / `threads.send` / `threads.create`. An agent can write
one; an example `threads-board` is seeded on first use. Plugins can be turned
off (state in `~/AgentArea/plugins-state.json`); a disabled plugin is not
served at all. Apps can be pinned to the sidebar.

**Secrets.** A vault for API keys that local agents can *use* without *seeing*
(`src-tauri/src/vault.rs`, `secrets_mcp.rs`, UI in `SecretsView`). Values live in
the OS keychain (service "AgentArea Desktop Secrets"); `~/AgentArea/secrets.json`
holds only name, description, allowed hosts and last use. The UI can set or
replace a value, never read it back.

- **Handles, not values.** At launch the app starts a local MCP server
  (streamable HTTP on `127.0.0.1:<random port>/mcp`). When the vault has
  secrets, each turn's CLI gets it as `agentarea_secrets` with a per-turn bearer
  token bound to the thread. Tools: `list_secrets`, `request_secret(name,
  purpose)` → a one-time handle `aas_…` (single use, 10 min, this thread only),
  and `http_request`. The handle can go in a header value, the URL query or the
  body. The server swaps in the value only for an allowed host (exact or
  `*.domain`, https only, http just for localhost), sends the request itself
  without following redirects, and redacts every vault value (raw, base64,
  URL-encoded) from the response as `[secret:NAME]`.
- **Pre-check.** Before a prompt goes to the CLI, any registered value in it
  (6+ chars) is replaced with `[secret:NAME]`, and a short note tells the agent
  what the markers mean. Every stdout/stderr line from the CLI is redacted the
  same way before it reaches the UI.
- **Not covered in v1.** Files the CLI's own tools read (for example Claude's
  `Read` on a `.env` holding a secret) go to the model without passing through
  us. Running commands with a secret in env is left out on purpose, because
  `echo`/`base64` would leak it.

**Data sources.** Data can be local or in the AgentArea cloud. The UI shows
which is which, and a local agent can see both.

- **What the agent sees.** Every local turn gets a second local MCP server,
  `agentarea_data` (`src-tauri/src/data_mcp.rs`). It runs at `/data` on the
  vault's loopback server and has its own per-turn bearer token. Tools:
  `list_sources`, `cloud_list({prefix})`, `cloud_read({path})` (text only,
  ≤1 MiB, passed through the vault redactor).
  - Local sources: each enabled `fs` plugin's granted folder, plus the
    thread's folder. The agent reads them with its own file tools. Claude gets
    each plugin folder as `--add-dir`, with `Edit`/`Write` there denied, so
    it can read but not edit. Codex's workspace-write sandbox already reads
    anywhere and writes only in the thread folder.
  - Cloud source: the workspace library (`/v1/files`). The access token, API
    base and workspace go to Rust per turn and live only in that turn's
    memory. They are never put in args or in files the agent can read.
  - A short note added to each prompt lists the sources and explains that
    `agentarea://files/<path>` means a workspace file readable with
    `cloud_read`.
- **What the user sees.** A read-only chip in the composer ("Data · 1 local ·
  1 cloud") lists the same sources.
- **Plugins.** A plugin that declares `"permissions": ["fs", "cloud"]` gets
  `cloud.info`, `cloud.list({prefix})`, `cloud.read({path})` and
  `cloud.setRoot({prefix})`. Paths are relative to the plugin's cloud root
  (default `wiki/`, stored as `cloudRoots` in `~/AgentArea/plugins-state.json`)
  and can't leave it. Requests go through `apiRequest`, so the plugin never
  sees a token. `threads.attachCloud({path})` attaches an
  `agentarea://files/…` reference.
- **LLM Wiki.** The example plugin (`examples/plugins/llm-wiki/`) shows
  "Local · folder" and "Cloud · AgentArea · workspace / root" trees side by
  side, with one filter across both. The viewer shows a source badge; hashes
  look like `#local:…` and `#cloud:…`.

Later the desktop also becomes the gate for launching agents; that is not
built yet.

## Develop

```bash
pnpm install
pnpm tauri dev      # native window + Vite HMR
pnpm build          # typecheck + frontend build
cd src-tauri && cargo test            # unit tests
cargo test -- --ignored e2e_ --nocapture   # against the real claude / codex CLIs
```

Dev builds are ad-hoc signed, so macOS forgets Keychain and privacy grants on
every rebuild. Run `scripts/dev-signing-setup.sh` once: it creates a local
self-signed code-signing identity, and a cargo runner (`scripts/dev-run.sh`)
signs each dev build with it so the grants stick.

## License

Apache-2.0 — see [LICENSE](LICENSE). Third-party notices (Orca theme, Geist
font) are in [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).

## Layout

```
src/
  App.tsx              # shell: top bar, sessions sidebar, chat + MCP/skill pickers
  store.ts             # sessions, catalog, auth (zustand)
  data.ts              # session types + offline mock catalog
  lib/api.ts           # AgentArea REST: catalog + per-session client
  lib/local.ts         # thread processes: run/approve/stop + claude/codex event parsing
  lib/vault.ts         # vault secrets: list/create/update/delete (no reads of values)
  lib/localMcp.ts      # local MCP servers: install/start, tools/list → apps
  lib/registry.ts      # Recommended list + MCP registry search → servers.json specs
  lib/cloud.ts         # delegate a thread to a cloud agent (tasks + SSE)
  lib/cloudFiles.ts    # workspace files (/v1/files) for "cloud" plugins
  lib/cloudPaths.ts    # pure path checks + one-level views of the flat file list
  lib/utils.ts         # cn()
  styles.css           # Orca-derived Tailwind v4 theme tokens (light/dark)
src-tauri/             # Rust core: OAuth loopback, runners (persistent CLI
                       # processes), local MCP runtime,
                       # vault (keychain) + secrets MCP server, data MCP
                       # server (data_mcp.rs: local + cloud data sources),
                       # AgentArea MCP proxy (secrets_mcp.rs)
```
