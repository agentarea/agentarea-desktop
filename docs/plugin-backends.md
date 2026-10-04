# Plugin backends

A plugin can have a backend: an MCP server that runs either **locally** (a
stdio child of the app) or **in the AgentArea workspace** (an existing MCP
instance, reached through the platform's governed MCP proxy). Wherever it
runs, the app serves it at **one stable local endpoint**. The plugin's page
and the local agent CLIs (Claude Code, Codex) both use that endpoint.

Status: phase 1 (host mechanism) is built. Phases 2 and 3 are described at the
end.

Code: `src-tauri/src/plugin_backends.rs` (manifest, placement, routing,
local processes), `src-tauri/src/secrets_mcp.rs` (the gateway route),
`src-tauri/src/lib.rs` (agent wiring), `src/lib/plugins.ts` (bridge, context
sync), `src/components/AppsPanel.tsx` (placement UI). Example:
`examples/plugins/backend-demo/`.

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
  (`[A-Za-z_][A-Za-z0-9_]*`). The env is never sent to the UI.
- The app always sets two variables, overriding any `env` of the same name:
  - `AGENTAREA_PLUGIN_ID`: the plugin id.
  - `AGENTAREA_DATA_DIR`: `~/AgentArea/workspaces/<env>/<slug>/plugins/<id>/`.
    `<env>` is the environment id (`ru`, `local`) and `<slug>` is the active
    workspace. The app creates the folder. With no workspace known, it is
    `~/AgentArea/workspaces/local/default/plugins/<id>/`.
- `cloud.instance` is the **name** of an MCP instance in the active workspace.
  `plugin:<id>` is the convention.
- If `server` is invalid (empty command, bad env name, empty instance, wrong
  types), the whole manifest is rejected and the plugin does not appear. This
  is the same rule as for a bad `entry`.

## Placement

Each plugin has a placement mode: `auto` (the default), `local` or `cloud`. It
is stored in `~/AgentArea/plugins-state.json` under `placements` as plain
strings. A missing entry means `auto`. A value this build doesn't recognise
reads as `auto`, so a file written by a newer app version stays readable.
Older state files without the field load unchanged.

The mode is resolved against the **active workspace** on every request:

| mode    | resolves to                                                      |
|---------|------------------------------------------------------------------|
| `auto`  | cloud if the cloud instance matches, else local if allowed, else unavailable |
| `local` | local if declared and allowed, else unavailable                  |
| `cloud` | cloud if the instance matches, else unavailable                  |

- **Cloud match rule.** The workspace's instances
  (`GET /v1/workspaces/{slug}/mcp-server-instances/`) are matched on `name`
  equal to `cloud.instance`. The match is exact and case-sensitive. The
  platform keeps names unique per workspace. If there are no matches, or more
  than one, cloud is unavailable and the reason says why. An instance id that
  isn't UUID-shaped is refused, because it goes into a URL.
- **Local needs the user's yes.** See [Security model](#security-model).
- **Unavailable** always carries a reason, for example `no MCP instance named
  "plugin:x" in acme`, `not signed in to AgentArea`, or `its local server
  isn't allowed yet`. In `auto` with both parts declared, the reason covers
  both.

**Context.** The frontend reports the active workspace to Rust
(`plugin_backends_set_context`) on start, on sign-in or sign-out, on a
workspace switch, and on Refresh in Apps → Plugins. The report contains the
env id, workspace slug, API base and the instance list. If the user is signed
out, or the listing failed, it sends `instances: null` plus an `error`. The
token is **not** part of the context (see [Auth](#auth)).

**When things change.** A change to placement, approval, enable/disable or
the workspace triggers a reconcile. The reconcile stops local processes that
no longer apply: the plugin was disabled or moved to cloud, its command
changed, or the workspace changed (so the data folder changed). The next
request starts what is needed.

## Gateway

There is one endpoint per plugin, on the vault's existing loopback server
(`secrets_mcp.rs`, `127.0.0.1:<random port>`):

```
POST http://127.0.0.1:<port>/plugins/<id>/mcp     (MCP Streamable HTTP)
```

- **Local.** The JSON-RPC message goes to the plugin's stdio child, which is
  started on first use. All callers share one child per plugin:
  - Client request ids are remapped onto the child's own ids, and the client's
    id comes back in the response.
  - A client's `initialize` gets the result from the child's own
    initialization, which the host performed once at start.
  - Notifications return 202 and are dropped.
  - `GET` and `DELETE` return 405, because the gateway offers no SSE stream.
  - Batches return 400.
  - Responses are plain JSON.
  - The child is stopped on app exit, when the plugin is disabled, and on
    reconcile (see above). It restarts on demand if it dies.
- **Cloud.** The request is streamed to `{apiBase}/v1/mcp/{instance_id}/mcp`
  and the answer (JSON or SSE) is streamed back. It uses the same forwarding
  code as the existing AgentArea client proxy:
  - Only MCP headers are forwarded (`Mcp-Session-Id`, `Mcp-Protocol-Version`,
    `Accept`, `Content-Type`, `Last-Event-ID`).
  - `WWW-Authenticate` and cookies never come back, so a CLI never starts its
    own OAuth flow.
  - The platform binds the request to the instance's own workspace
    (`bind_mcp_instance_workspace`), so the URL needs no workspace segment.
- **Unavailable.** The gateway answers 503 with the reason.

The plugin's page uses the same gateway. A bridge call goes to Rust
(`plugin_backend_tools` / `plugin_backend_call`). Rust acts as an MCP client
of `/plugins/<id>/mcp`: it initializes once per plugin and placement, and
starts over once if the upstream forgot the session. The page therefore sees
exactly what agents see.

## Auth

- **Gateway tokens.** Every caller gets a random 256-bit bearer token bound to
  **one** plugin (`issue_plugin_token`). A token for plugin A gets 401 on
  `/plugins/B/mcp`. Agent tokens live as long as the agent process; the UI
  client's token lives as long as that client. Dropping a token revokes it.
  As on the other loopback routes, a request with a non-local `Origin` gets
  403.
- **Cloud upstream.** Each forwarded request carries `Authorization: Bearer
  <the app's live access token>`. That token is read from the value
  session.ts pushes on every sign-in and renewal (`set_cloud_token`).
  session.ts renews about 90 s before expiry even when the UI is idle. The
  gateway has **no fallback token**: when signed out, it answers 401 locally
  and nothing is sent upstream. The caller's own `Authorization` is never
  forwarded.

## Agents

When a local Claude Code or Codex thread starts, every enabled plugin whose
backend resolves to local or cloud **at that moment** is added as an MCP
server named `plugin_<id>`. It uses the gateway URL and a bearer token for
that plugin, in the same place as `agentarea_data` and `agentarea_secrets`:

- **Claude:** in the 0600 `mcp.json` (header), plus `--allowedTools mcp__plugin_<id>`.
- **Codex:** `-c mcp_servers.plugin_<id>.url=…`,
  `bearer_token_env_var="AGENTAREA_PLUGIN_TOKEN_<n>"`, and
  `default_tools_approval_mode="approve"`.

Unavailable backends are skipped. The set of plugin ids is part of the
process signature, so adding or removing a backend restarts the thread's
process on its next turn. A placement change does not restart it, because the
URL is the same and the gateway re-routes.

## Bridge (plugin page)

| method                          | result                                              |
|---------------------------------|-----------------------------------------------------|
| `plugin.placement`              | `{ mode, resolved: 'local'\|'cloud'\|'unavailable', reason? }` |
| `plugin.tools`                  | the backend's `tools/list` result                   |
| `plugin.call {name, arguments}` | the backend's `tools/call` result                   |

These methods work only for an enabled plugin that declares `server`, and
only on **its own** backend. The id is always the calling frame's plugin, and
no parameter can name another plugin. Rust checks this again.

## UI

The Apps → Plugins panel shows a bar above each plugin that has a server:

- An Auto / Local / Cloud selector. Local or Cloud is disabled when the
  manifest doesn't declare it.
- The resolved placement badge ("Runs on this device", "Runs in the
  workspace", "Unavailable"), plus the reason.
- For a local server: its command and an **Allow local server** / **Revoke**
  button.

## Security model

- **Local servers run unsandboxed, as the user**, with the plugin folder as
  their working directory. Plugin folders are plain files, and an agent in a
  thread can write one. So the app never runs a plugin's command until the
  user clicks **Allow local server**:
  - The approval stores the exact command and env
    (`serverApprovals` in plugins-state.json).
  - Any edit to `command` or `env` cancels the approval and stops the running
    process.
  - Until approved, local placement is unavailable (in `auto`, cloud is still
    used).
- **Gateway:** loopback only, a per-caller and per-plugin bearer token, and a
  local `Origin` only. A browser page can't reach a plugin without a token.
- **Cloud:** the user's own token, with governance applied by the platform
  proxy (workspace binding, policy, audit). The gateway only adds the token
  and filters headers.
- **The plugin page** stays in its opaque-origin sandbox with `connect-src
  'none'`. It reaches its backend only through the bridge and only its own
  backend.
- **Known limits:**
  - Tool results from a plugin backend are **not** passed through the vault
    redactor (`agentarea_data` results are). A local backend can read any
    file the user can read.
  - Agent tool calls to `plugin_<id>` are auto-approved, like
    `agentarea_data`.
  - The local server's stderr is kept only in memory, for error messages.

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
- **Redaction.** Backend results should pass the vault redactor, as
  `agentarea_data` does today. Otherwise inventory credentials could reach the
  model.
- **Long calls.** The gateway forwards with a 60 s request timeout, and
  long-running tools need progress or async results.

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
- **Lifecycle:** update or rollback when `plugin.json` changes. A matching
  rule stronger than the name, such as an id in instance metadata or a
  `plugin_id` field, would stop a hand-made instance with the same name from
  being picked up.
- **Desktop:** a "Run in cloud" action that creates the instance and then
  re-syncs the context (the Refresh in Plugins already does this).
