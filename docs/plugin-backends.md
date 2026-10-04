# Plugin backends

A plugin can have a backend: an MCP server that runs either **locally** (a
stdio child of the app) or **in the AgentArea workspace** (the workspace's MCP
instance named `plugin:<id>`, reached through the platform's governed MCP
proxy). Wherever it runs, the app serves it at **one stable local endpoint**.
The plugin's page and the local agent CLIs (Claude Code, Codex) both use that
endpoint. Nothing runs and nothing is used until the user has reviewed and
allowed the plugin's backend.

Status: phase 1 (host mechanism) is built. Phases 2 and 3 are described at the
end.

Code:

- `src-tauri/src/plugin_backends.rs`: manifest, placement, routing, local
  processes.
- `src-tauri/src/plugin_trust.rs`: folder fingerprint and signed approvals.
- `src-tauri/src/secrets_mcp.rs`: the gateway route.
- `src-tauri/src/lib.rs`: agent wiring.
- `src/lib/plugins.ts`: bridge and context sync.
- `src/components/AppsPanel.tsx`: placement and approval UI.

Example: `examples/plugins/backend-demo/`.

## Manifest

`plugin.json` takes an optional `server`. Each part is optional, but at least
one must be present:

```json
{
  "id": "backend-demo",
  "title": "Backend demo",
  "server": {
    "local": { "command": ["node", "server.mjs"], "env": { "LOG_LEVEL": "info" } },
    "cloud": { "instance": "plugin:backend-demo" }
  }
}
```

- `local.command` is the program followed by its arguments. It runs from
  the **approved read-only copy** of the plugin folder, which is its working
  directory (see [Security model](#security-model)).
  - A bare name (`node`) is looked up the same way as the agent CLIs:
    `~/.local/bin`, Homebrew, `/usr/local/bin`, then PATH. The review shows
    the absolute path it resolves to.
  - A relative path (`./bin/server`) is resolved from the copy.
  - Arguments must stay inside the plugin folder: no absolute paths, `~`, or
    `..` (also inside `--flag=value`). A relative program path is held to the
    same rule. Only an absolute program path, the interpreter itself, may
    point elsewhere.
  - **Nothing may fetch code at run time.** Such code would be whatever
    version is current, not what was reviewed. **Vendor dependencies into
    the plugin folder** (for example, a committed `node_modules`). Refused,
    each with a reason:
    - downloaders: `npx`, `pnpx`, `bunx`, `uvx`, `pipx`, `yarn`, `npm exec`,
      `pnpm dlx`, `bun x`, `uv tool`;
    - remote specifiers in arguments: `npm:`, `jsr:`, `http:`, `https:`
      (also as `--flag=https:…`), and Node network imports;
    - `uv run` with `--with`, `--with-requirements`, `--with-editable`,
      `--script` or `--isolated`;
    - `uv run` without a committed `uv.lock`;
    - a script with inline dependencies (`# /// script`) without its own
      `<file>.lock`;
    - `bun` without `--no-install`;
    - `deno` without `--no-remote` and `--no-npm`.

    The `uv.lock` rules need the folder. The review reports them (Allow is
    disabled with the reason), approve refuses them, and every start checks
    them again against the copy.
- `local.env` is extra environment for the child. Names must be plain
  (`[A-Za-z_][A-Za-z0-9_]*`). Env values are left out of the plugin list. The
  approval review shows them in full, and they are scrubbed from the server
  error text that callers see.
- The app **forces** these after the plugin's `env`, so the plugin can't
  override them:
  - **Offline package tools:** `UV_OFFLINE=1`, `PIP_NO_INDEX=1`,
    `npm_config_offline=true`, `BUN_CONFIG_NO_INSTALL=1`, `DENO_NO_REMOTE=1`.
  - **`uv run` exception:** `uv run` is not offline. With `UV_FROZEN=1` it
    installs exactly the hash-pinned packages of the reviewed `uv.lock`, into
    an app-owned venv next to the copy (`<fingerprint>.venv`).
  - **Caches** go to the data folder: `XDG_CACHE_HOME`, `UV_CACHE_DIR` and
    `npm_config_cache` under `<data>/.cache`.
  - `PYTHONDONTWRITEBYTECODE=1`, `PYTHONNOUSERSITE=1`, and `NODE_PATH=`
    (empty).
- The app always sets two variables, overriding any `env` of the same name:
  - `AGENTAREA_PLUGIN_ID`: the plugin id.
  - `AGENTAREA_DATA_DIR`: `~/AgentArea/workspaces/<env>/<slug>/plugins/<id>/`.
    `<env>` is the environment id (`ru`, `local`) and `<slug>` is the
    workspace. The app creates the folder. With no workspace known, it is
    `~/AgentArea/workspaces/local/default/plugins/<id>/`.
  - **A backend writes only to this folder.** Its own code folder is a
    read-only copy, so writes there fail.
- `cloud.instance` must be exactly **`plugin:<id>`**, the plugin's own id. A
  plugin cannot point at the workspace's other MCP servers.
- If `server` is invalid (empty command, bad env name, wrong instance name,
  wrong types), the plugin still loads, but without a backend.
  `serverError` says why, and Apps → Plugins shows it.
- Manifests are cached and re-read only when `plugin.json` (or the SVG icon)
  changes size or mtime. The gateway looks plugins up on every message.

## Placement

Each plugin has a placement mode: `auto` (the default), `local` or `cloud`. It
is stored in `~/AgentArea/plugins-state.json` under `placements` as plain
strings. A missing entry means `auto`. A value this build doesn't recognise
reads as `auto`, and older state files load unchanged.

The mode is resolved against the active workspace:

| mode    | resolves to                                                      |
|---------|------------------------------------------------------------------|
| `auto`  | cloud if the workspace's `plugin:<id>` instance is approved, else local if declared, else unavailable |
| `local` | local if declared, else unavailable                              |
| `cloud` | cloud if the workspace's `plugin:<id>` instance is approved, else unavailable |

Every placement needs the user's approval. Without one the result is
unavailable, with the reason "not allowed yet".

A cloud approval names the **concrete instance**: its id, in its environment
and workspace (`<env>/<slug>/<instance id>`).

- Another instance with the same name, the same instance seen from another
  workspace, or a re-created instance are all unapproved until reviewed.
- In `auto`, an instance that appears but hasn't been reviewed **never takes
  over** from an approved local backend. The backend stays local, and the
  status shows the notice "cloud instance available: review it to run
  there".
- In `cloud`, such an instance is unavailable, with a reason that says it
  needs a review.

- **Cloud match rule.** The workspace's instances
  (`GET /v1/workspaces/{slug}/mcp-server-instances/`) are matched on `name`
  equal to `plugin:<id>`. The match is exact and case-sensitive. The platform
  keeps names unique per workspace. If there are no matches, or more than
  one, cloud is unavailable and the reason says why. An instance id that
  isn't UUID-shaped is refused, because it goes into a URL.
- **Unavailable** always carries a reason, for example `no MCP instance named
  "plugin:x" in acme`, `not signed in to AgentArea`, or `not allowed yet: …`.

**Context.** The frontend reports the active workspace to Rust
(`plugin_backends_set_context`) on start, on sign-in or sign-out, on a
workspace switch, and on Refresh in Apps → Plugins. The report contains the
env id, workspace slug, API base and the instance list. If the user is signed
out, or the listing failed, it sends `instances: null` plus an `error`. The
token is **not** part of the context (see [Auth](#auth)).

**Agent threads are pinned.** When a thread's agent process starts, each
plugin token it gets is pinned to the placement at that moment: local plus
its data folder, or cloud plus its instance. A later placement change or
workspace switch does not move a running thread. The pinned thread keeps
working on its workspace's data, and that local process keeps running beside
the new workspace's. The pin is still checked on every request: the plugin
must be enabled, approved, and still declare that kind of backend. The route
keys are part of the process signature, so the thread's **next turn**
restarts its process on the new placement with a fresh MCP session. The
plugin's page is not pinned and follows the current placement.

**When things change.** A change to placement, approval, enable/disable or
the workspace triggers a reconcile. The reconcile stops local processes that
no longer apply:

- the plugin is gone, disabled or not approved;
- the plugin is placed elsewhere;
- the data folder is used neither by the current workspace nor by a pinned
  thread;
- the process has died, in which case it is reaped.

Ending a pinned thread runs the reconcile too. The next request starts what
is needed. A server that fails to start is not retried for 5 s.

## Gateway

There is one endpoint per plugin, on the vault's existing loopback server
(`secrets_mcp.rs`, `127.0.0.1:<random port>`):

```
POST http://127.0.0.1:<port>/plugins/<id>/mcp     (MCP Streamable HTTP)
```

- **Local.** The JSON-RPC message goes to the plugin's stdio child. The child
  is started on first use from the approved copy. There is one child per
  (plugin, data folder), shared by its callers:
  - Client request ids are remapped onto the child's own ids, and the client's
    id comes back in the response.
  - A client's `initialize` gets the child's own initialization result. A
    client that asked for an older known protocol version gets that version
    echoed.
  - Notifications return 202 and are dropped.
  - `GET` and `DELETE` return 405 and start nothing, because the gateway
    offers no SSE stream for local routes.
  - Batches return 400.
  - Child errors come back as -32603 with the message, scrubbed of `env`
    values.
- **Cloud.** The request (POST, GET or DELETE) is streamed to
  `{apiBase}/v1/mcp/{instance_id}/mcp` and the answer (JSON or SSE) is
  streamed back. It uses the same forwarding code as the AgentArea client
  proxy:
  - Only MCP headers are forwarded (`Mcp-Session-Id`, `Mcp-Protocol-Version`,
    `Accept`, `Content-Type`, `Last-Event-ID`).
  - `WWW-Authenticate` and cookies never come back, so a CLI never starts its
    own OAuth flow.
  - The platform binds the request to the instance's own workspace, so the
    URL needs no workspace segment.
- **Unavailable.** The gateway answers 503 with the reason (405 for GET and
  DELETE).
- **Sessions.** The gateway remembers which route a caller's `initialize`
  went to. If a later message from that caller would go to another route
  (placement or workspace moved), the gateway answers **404** once. Per MCP,
  the client then initializes a new session, so a stale `Mcp-Session-Id` or
  a never-initialized upstream is not used. Pinned agent tokens never move.
- **Redaction.** Results on an agent's token pass the vault `Redactor`, the
  same as `agentarea_data`:
  - Local replies are redacted whole.
  - Cloud answers are redacted per SSE event (LF or CRLF framing), or as a
    whole JSON body. A non-SSE body that is too large to hold is cut off
    rather than passed through unredacted.

The plugin's page uses the same gateway. A bridge call goes to Rust
(`plugin_backend_tools` / `plugin_backend_call`). Rust acts as an MCP client
of `/plugins/<id>/mcp`: it initializes once per plugin and route (guarded, so
concurrent calls don't double-initialize), and starts over once on a 404. The
page therefore sees exactly what agents see.

## Auth

- **Gateway tokens.** Every caller gets a random 256-bit bearer token bound to
  **one** plugin. A token for plugin A gets 401 on `/plugins/B/mcp`.
  - Agent tokens are pinned, carry the turn's redactor, and live as long as
    the agent process.
  - The UI client's token lives as long as that client.
  - Dropping a token revokes it.
  - A request with a non-local `Origin` gets 403.
- **Cloud upstream.** Each forwarded request carries `Authorization: Bearer
  <the app's live access token>`. session.ts pushes that token to Rust on
  every sign-in and renewal (`set_cloud_token`), and renews about 90 s before
  expiry even when the UI is idle. The gateway has **no fallback token**:
  when signed out, it answers 401 locally and nothing is sent upstream. The
  caller's own `Authorization` is never forwarded.

## Agents

When a local Claude Code or Codex thread starts, every enabled and approved
plugin whose backend resolves to local or cloud **at that moment** is added
as an MCP server named `plugin_<id>`. It uses the gateway URL and a pinned,
redacting bearer token for that plugin, in the same place as `agentarea_data`
and `agentarea_secrets`:

- **Claude:** in the 0600 `mcp.json` (header), plus `--allowedTools mcp__plugin_<id>`.
- **Codex:** `-c mcp_servers.plugin_<id>.url=…`,
  `bearer_token_env_var="AGENTAREA_PLUGIN_TOKEN_<n>"`, and
  `default_tools_approval_mode="approve"`.

Unavailable backends are skipped. `(plugin id, route)` pairs are part of the
process signature (see "Agent threads are pinned" above).

## Bridge (plugin page)

| method                          | result                                              |
|---------------------------------|-----------------------------------------------------|
| `plugin.placement`              | `{ mode, resolved: 'local'\|'cloud'\|'unavailable', reason? }` |
| `plugin.tools`                  | the backend's `tools/list` result                   |
| `plugin.call {name, arguments}` | the backend's `tools/call` result                   |

These methods work only for an enabled plugin that declares `server`, and
only on its own backend. PluginFrame passes the calling frame's plugin id;
no bridge parameter can name another plugin. The Rust commands take that id
as given. They rely on the plugin frame having no Tauri IPC of its own: it is
sandboxed, with an opaque origin, so only the app's own UI can invoke them.

## UI

The Apps → Plugins panel shows a bar above each plugin that has a server (or
an approval):

- An Auto / Local / Cloud selector. Local or Cloud is disabled when the
  manifest doesn't declare it.
- The resolved placement badge ("Runs on this device", "Runs in the
  workspace", "Unavailable"), plus the reason.
- **Review and allow…** (or **Review update…** when the folder changed, or
  **Review…** when a cloud instance needs one) opens the review
  (`plugin_server_review`). It shows:
  - the exact command, wrapped and never truncated;
  - the absolute path of the program it resolves to;
  - the folder that is copied, with its file count and size;
  - every env variable with its value, plus a warning for `NODE_OPTIONS`,
    `NODE_PATH`, `PYTHONPATH`, `PYTHONSTARTUP`, `PYTHONHOME`, `LD_*`, `DYLD_*`
    and similar;
  - the concrete workspace instance it would use (name, id, workspace), or
    that none exists in this workspace now.

  **Allow** sends the fingerprint and instance binding of what was shown.
- **Revoke** is shown whenever any approval entry exists: current, an older
  version with an update available, or one that no longer counts.
- "Update available" appears when the live folder differs from the approved
  copy. The approved copy keeps running until the update is approved.
- `serverError` is shown when the manifest's `server` was rejected.

## Threat model

**What the approval is.** The approval is **consent plus integrity**:

- **Consent.** Nothing of a plugin's backend runs, and no cloud instance is
  called, until the user has seen exactly what it is and said yes.
- **Integrity.** What runs is exactly what the user saw.
  - Third-party plugin code can't be swapped or extended afterwards, whether
    by an update, a downloaded replacement, or code fetched at run time.
  - Another workspace member can't redirect the plugin to a different cloud
    endpoint.
  - An approval the user revoked can't be put back.

**What it is not.** It is **not a sandbox against the user's own agents.**
Local agent CLIs (Claude Code, Codex) in threads already run shell commands
as the user, within the limits their own sandbox and the user's per-command
approvals set. An agent that the user lets run arbitrary commands can do
anything the user can, with or without plugins.

| protected | not protected |
|---|---|
| Running code nobody reviewed: a new plugin, a changed one, or code fetched at run time. Downloaders, remote specifiers, `uv run --with`/`--script`, unlocked `uv run`, `bun` without `--no-install` and `deno` without `--no-remote --no-npm` are all refused, and package tools run offline. `uv run` installs only the reviewed, hash-pinned `uv.lock`. | Code the user approved doing harm: it runs as the user, unsandboxed, and may itself open network connections or download and run code (only the *launchers* are held offline) |
| Swapping files after approval (live folder edits, symlink tricks, edits racing a start) | A process that already runs as the user editing the app's data folder *and* getting Keychain access (see limits) |
| Pointing the plugin at another workspace instance, or a same-named new one | What the approved cloud instance does with your data (platform governance applies) |
| Forging, replaying or un-revoking approvals by editing `~/AgentArea` | Prompt injection through tool results (results to agents are redacted for vault secrets, not filtered) |
| Hidden or unreviewed command, interpreter, env (all shown; risky env flagged) | Code reached through Node's parent-folder module lookup above the copy (e.g. `~/node_modules`), or loaded from caches under the data folder, which the approved code or the user's agents could write |
| Reverting a revoke by putting an old approval back, including from a second app instance with a stale cache | Revoking while the Keychain can't be written: it is revoked locally and the error is shown, but the counter isn't raised |

**Sandboxing** local backends and agent CLIs alike (for example
`sandbox-exec` with writes limited to the data folder) is **future work**.

## Security model

- **One approval covers the plugin.** It covers the local server's code
  (via the copy) and each cloud instance the user reviewed.
- **Snapshot execution.** Approving copies the plugin folder into the app's
  own data, `~/Library/Application Support/com.jamakase.agentarea-desktop/plugin-snapshots/<id>/<fingerprint>/`.
  - It hashes the **copy** and refuses to store anything that doesn't match
    what was shown.
  - It makes the copy read-only and owner-only: files `0400` (`0500` if
    executable), folders `0500`. A private file in the plugin stays private.
  - The backend always runs from that copy (cwd = copy), and every start
    re-hashes the copy.
  - The live folder can change freely: the change shows as "update
    available" and runs only after review. This closes the check-then-run
    race and lazy imports of changed files. It also stops caches written by
    the server from revoking it, because they go to the data folder.
- **Fingerprint.** SHA-256 over the plugin id and every file's path,
  executable bit and content hash (sorted). It is workspace-independent and
  identical for the live folder and its copy.
  - **Symlinks** are followed by content when they stay inside the folder: a
    link to a file is copied as that file's content, and a link to a folder
    is walked. A link that leads outside the folder, or a cycle, is refused.
  - **Skipped entries:** `.DS_Store` and a top-level `.git/`. These are
    skipped *as entries* only. A link pointing into them still brings its
    target's content into the copy and the hash, so nothing can ride along
    unhashed.
  - **Limits:** 100 000 files and 512 MiB, enough for a typical Node plugin
    with a committed `node_modules`. The review shows file count and size.
- **What you see is what you approve.** The review returns the fingerprint
  and the cloud instance binding of exactly what it showed, and Allow sends
  both back. Rust refuses if the copy's hash or the current instance
  differs.
- **Approvals can't be forged or replayed.** `serverApprovals[id]` in
  plugins-state.json holds
  `{plugin, fingerprint, instances, issued_at, nonce, epoch, mac}`.
  - `mac` is HMAC-SHA256 over the other fields, with a key that lives only in
    one macOS Keychain item: service "AgentArea Desktop Secrets", account
    `plugin-approvals.key.v1`. The item is JSON:
    `{key, epochs: {plugin: n}}`.
  - Every approve and revoke bumps the plugin's epoch in that item, and an
    approval counts only at the current epoch. So a copy of an older or
    revoked approval put back into the file does not count.
  - The item is read once per launch and cached.
  - **Before every write**, the item is read again and epochs are merged
    (highest wins). A second app instance with a stale cache (for example a
    dev build next to the release build) can't roll back another's revoke.
  - **A failed or denied read** isn't cached as a value: nothing is approved
    meanwhile. It is retried only when the user acts in Apps → Plugins
    (review, approve, revoke) or after 5 minutes, so the gateway doesn't
    re-prompt on every request.
  - **Revoke** always deletes the approval entry and the copies, and stops
    the processes. If the Keychain write fails, the user sees the error: the
    revoke happened locally, but the counter wasn't raised.
- **Housekeeping.** At startup, copies and `.tmp-*` folders that no approval
  entry names are removed. Files are hashed in 64 KiB chunks.
- **Gateway:** loopback only, a per-caller and per-plugin bearer token, and a
  local `Origin` only. A browser page can't reach a plugin without a token.
- **Cloud:** the user's own token, sent only to an approved instance, with
  governance applied by the platform proxy (workspace binding, policy,
  audit).
- **The plugin page** stays in its opaque-origin sandbox with `connect-src
  'none'`, and reaches its backend only through the bridge. It is served
  from the live folder, not the copy; it is static web content without host
  access.
- **Known limits:**
  - An approved local backend is unsandboxed. It runs as the user, can read
    other threads' 0600 `mcp.json` files (bearer tokens), and could ask the
    Keychain for the approval key: macOS prompts, and "Always Allow" would
    let it sign approvals. Sandboxing is the fix (future work).
  - Node resolves bare imports by walking up from the copy, so
    `~/node_modules` is still reachable. Vendor every dependency.
  - Agent tool calls to `plugin_<id>` are auto-approved, like
    `agentarea_data`. The approval is the consent.
  - The plugin page's own results are not redacted. Only agent tokens carry
    a redactor.
  - Child JSON-RPC error codes are flattened to -32603. The message is kept.
  - Every local start re-hashes the whole copy while holding that plugin's
    start lock. With a large `node_modules`, this costs time on each start.

## Phase 2: Channels as its own server

Today the Channels network tools live inside the app. The agent tools
(`state_query`, `channel_update`, `inventory_*`, `post_*`, `metrics_add`,
`review_feedback`…) are in `data_mcp.rs` and `state_db.rs`. The page's
`state.*` bridge goes to `state_call`. Moving them into a plugin backend
needs:

- **A server in the Channels plugin.** It would own `state.db`, its
  migrations and the tool schemas (today `state_db::mcp_tools` / `mcp_call`),
  with the db at `$AGENTAREA_DATA_DIR/state.db`. Today it is
  `~/AgentArea/state.db`, so the server needs a one-time move or adopt step.
  Note that the data folder becomes per-workspace.
- **Actor attribution.** `state_db` audits writes as `agent:<thread>` or
  `plugin:<id>`. The gateway would pass the caller to the child, for example
  `params._meta["agentarea/actor"]`, set from the token (agent thread vs plugin
  UI), and the server must trust only that.
- **Bridge.** The page's `state.*` calls become `plugin.call`. `vault.*` stays
  a host power, because the Keychain never leaves the app.
- **Removing the old path.** Delete the state tools from `agentarea_data` and
  the `"state"` permission, and drop `state_db::AGENT_HINT` from the prompt.
  Agents then see `mcp__plugin_channels__*` instead of
  `mcp__agentarea_data__*`, which affects prompts and skills.
- **Redaction.** Backend results already pass the vault redactor on agent
  tokens.
- **Long calls.** The gateway forwards with a 60 s request timeout, and
  long-running tools need progress or async results.
- **Data.** `state.db` must live in `AGENTAREA_DATA_DIR`. The code runs from
  a read-only copy, so it can't write next to itself.

## Phase 3: "Run in cloud"

The aim is to deploy a plugin's server as a workspace MCP instance, so that
`auto` picks it up. This needs:

- **Platform:** a way to create an instance from a plugin package. The proxy
  already dispatches `docker` / `command` instances through the Go manager.
  The instance would be named `cloud.instance` (`plugin:<id>`), with its
  package and version in `json_spec`.
- **Packaging:** the plugin folder (or a declared subset) as an image or
  bundle, and a runtime (node / python) on the manager side.
- **Data:** a persistent volume that plays the role of `AGENTAREA_DATA_DIR`,
  plus an export/import path from the local data folder. For Channels, that
  means `state.db`.
- **Secrets:** `local.env` values that are secrets move to the platform
  secret store and are injected by the proxy, never baked into the package.
- **Lifecycle:** update or rollback when `plugin.json` changes. Cloud
  approvals already bind the concrete instance id. A platform-side marker,
  such as a hash of the deployed package in instance metadata, would also tie
  *what the instance runs* to the reviewed folder.
- **Desktop:** a "Run in cloud" action that creates the instance and then
  re-syncs the context (the Refresh in Plugins already does this).
