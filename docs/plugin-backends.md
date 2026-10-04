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

- `local.command` is the program followed by its arguments. It runs with the
  plugin folder as its working directory. A bare name (`node`) is looked up
  the same way as the agent CLIs (`~/.local/bin`, Homebrew, `/usr/local/bin`,
  then PATH). A relative path (`./bin/server`) is resolved from the plugin
  folder.
- `local.env` is extra environment for the child. Names must be plain
  (`[A-Za-z_][A-Za-z0-9_]*`). Env values are left out of the plugin list. The
  approval review shows them in full, and they are scrubbed from the server
  error text that callers see.
- The app always sets two variables, overriding any `env` of the same name:
  - `AGENTAREA_PLUGIN_ID`: the plugin id.
  - `AGENTAREA_DATA_DIR`: `~/AgentArea/workspaces/<env>/<slug>/plugins/<id>/`.
    `<env>` is the environment id (`ru`, `local`) and `<slug>` is the
    workspace. The app creates the folder. With no workspace known, it is
    `~/AgentArea/workspaces/local/default/plugins/<id>/`.
  - **A backend must keep its data in this folder, not in the plugin folder.**
    Any write in the plugin folder cancels the approval (see
    [Security model](#security-model)).
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
| `auto`  | cloud if the workspace has `plugin:<id>`, else local if declared, else unavailable |
| `local` | local if declared, else unavailable                              |
| `cloud` | cloud if the workspace has `plugin:<id>`, else unavailable       |

Every placement also needs the user's approval. Without it the result is
unavailable, with the reason "not allowed yet".

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

- **Local.** The JSON-RPC message goes to the plugin's stdio child, which is
  started on first use. There is one child per (plugin, data folder), shared
  by its callers:
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
- **Unavailable.** The gateway answers 503 with the reason.
- **Sessions.** The gateway remembers which route a caller's `initialize`
  went to. If a later message from that caller would go to another route
  (placement or workspace moved), the gateway answers **404** once. Per MCP,
  the client then initializes a new session, so a stale `Mcp-Session-Id` or
  a never-initialized upstream is not used. Pinned agent tokens never move.
- **Redaction.** Results on an agent's token pass the vault `Redactor`, the
  same as `agentarea_data`:
  - Local replies are redacted whole.
  - Cloud answers are redacted per SSE event, or as a whole JSON body. A
    non-SSE body that is too large to hold is cut off rather than passed
    through unredacted.

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

The Apps → Plugins panel shows a bar above each plugin that has a server:

- An Auto / Local / Cloud selector. Local or Cloud is disabled when the
  manifest doesn't declare it.
- The resolved placement badge ("Runs on this device", "Runs in the
  workspace", "Unavailable"), plus the reason.
- **Review and allow…** opens the review (`plugin_server_review`). It shows:
  - the exact command, wrapped and never truncated;
  - the working folder;
  - every env variable with its value;
  - the workspace instance it uses ("Uses workspace instance
    `plugin:<id>`");
  - how many files the approval covers.

  **Allow** sends the fingerprint of what was shown. **Revoke** removes the
  approval.
- `serverError` is shown when the manifest's `server` was rejected.

## Security model

- **One approval covers both places.** A local server runs unsandboxed, as
  the user, with the plugin folder as its working directory. A cloud
  placement calls the workspace's `plugin:<id>` instance with the user's
  token. The app does neither until the user allows the plugin's backend.
- **What is approved: the whole folder.** `plugin_trust.rs` computes a
  SHA-256 over:
  - the plugin id;
  - the folder path;
  - every file in the folder, in sorted order: each file's path and content
    hash, and each symlink's target;
  - and so `plugin.json`, which holds the command, env and instance.

  Any change to a file cancels the approval: an edit, a new file, a deleted
  file. That includes an agent rewriting `server.mjs` or `node_modules`.
  - **Exclusions:** `.DS_Store` files anywhere, and a top-level `.git/`
    folder. Nothing else is excluded. Data belongs in `AGENTAREA_DATA_DIR`,
    outside the folder.
  - **Unverifiable folders can't be approved:** a symlink that leads out of
    the folder, more than 20 000 files, or more than 256 MiB.
- **When it is checked.**
  - Status and routing compare against a cached fingerprint. It is re-hashed
    when any file's size or mtime changes, at most every 2 s.
  - Every local server start re-hashes the whole folder, so an edit that
    keeps the mtime still can't run.
  - A route that finds the approval no longer matching stops the plugin's
    running processes.
  - A revoke that lands while a server is starting stops that server
    before it is used.
- **What you see is what you approve.** The review returns the fingerprint of
  exactly what it showed, and Allow sends that fingerprint back. Rust
  re-hashes and refuses ("changed since it was shown") if they differ. This
  closes the window between display and click.
- **Approvals can't be forged.** `serverApprovals` in plugins-state.json
  holds `{fingerprint, mac}`. The `mac` is HMAC-SHA256 over the id and
  fingerprint, with a 32-byte key that lives only in the macOS Keychain:
  - The key is one item: service "AgentArea Desktop Secrets", account
    `plugin-approvals.key.v1`.
  - It is created on first approval, read once per launch and cached. A
    failed or denied read is not cached, and in that case no approval counts.
  - A same-user process that can write `~/AgentArea` can delete approvals but
    cannot make one. Entries in any other shape (older builds, hand edits)
    are ignored.
- **Gateway:** loopback only, a per-caller and per-plugin bearer token, and a
  local `Origin` only. A browser page can't reach a plugin without a token.
- **Cloud:** the user's own token, with governance applied by the platform
  proxy (workspace binding, policy, audit). The gateway only adds the token
  and filters headers.
- **The plugin page** stays in its opaque-origin sandbox with `connect-src
  'none'`. It reaches its backend only through the bridge.
- **Known limits:**
  - An approved local backend is still unsandboxed. It can read any file the
    user can read, including other threads' 0600 `mcp.json` files, which
    hold bearer tokens. Running it under `sandbox-exec`, with writes limited
    to `AGENTAREA_DATA_DIR`, is future work.
  - Agent tool calls to `plugin_<id>` are auto-approved, like
    `agentarea_data`. The folder approval is the consent.
  - The plugin page's own results are not redacted. Only agent tokens carry
    a redactor.
  - Child JSON-RPC error codes are flattened to -32603. The message is kept.

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
- **Data and approval.** `state.db` must live in `AGENTAREA_DATA_DIR`. A
  database inside the plugin folder would cancel the approval on every write.

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
- **Lifecycle:** update or rollback when `plugin.json` changes. Today a
  hand-made instance named `plugin:<id>` would be picked up once the user
  approves the plugin. A platform-side marker, such as a `plugin_id` in
  instance metadata or a hash of the deployed package, would tie the instance
  to the reviewed folder.
- **Desktop:** a "Run in cloud" action that creates the instance and then
  re-syncs the context (the Refresh in Plugins already does this).
